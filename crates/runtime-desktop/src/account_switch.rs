use std::{
   collections::HashMap,
   ffi::c_void,
   sync::{Mutex, OnceLock, PoisonError},
};

use phoenix_hook::{image::Image, x86};
use phoenix_sigs::desktop;

use crate::servers;

type SwitchFn = unsafe extern "C" fn(
   app: *mut c_void,
   account: *const u8,
   flag: usize,
   a: usize,
   b: usize,
   c: usize,
   d: usize,
) -> usize;

static ORIGINAL: OnceLock<SwitchFn> = OnceLock::new();

const ACCOUNT_SIZE: usize = 48;
const USER_WORD: usize = 3;

/// Account numbers and whose they are, learned this session. Phoenix renumbers
/// accounts on restart.
static KNOWN: Mutex<Option<HashMap<usize, servers::Identity>>> = Mutex::new(None);
/// The account last switched to, while its identity is not known yet.
static UNNAMED: Mutex<Option<usize>> = Mutex::new(None);

pub fn install(image: &Image<'static>) {
   let func = match desktop::account_switch(image) {
      Ok(func) => func,
      Err(err) => {
         log::warn!("account switch: {err}, images follow history loads only");
         return;
      },
   };
   // SAFETY: runs from `prepare` before Discord's threads, and `detour` takes
   // the switch's seven arguments and returns what it returns.
   let trampoline = match unsafe {
      image.hook_entry(
         func,
         detour as *const () as usize,
      )
   } {
      Ok(trampoline) => trampoline,
      Err(err) => {
         log::warn!("account switch: hook failed: {err}");
         return;
      },
   };
   // SAFETY: the trampoline replays the prologue and jumps back into `func`.
   let _ = ORIGINAL.set(unsafe { std::mem::transmute::<usize, SwitchFn>(trampoline) });
   log::info!("account switch: hooked @ {func:#x}");
}

fn learn(index: usize, identity: servers::Identity) {
   KNOWN
      .lock()
      .unwrap_or_else(PoisonError::into_inner)
      .get_or_insert_default()
      .insert(index, identity);
}

/// The account loading history is the one on screen, so it names the account
/// last switched to when that one is not known yet.
pub fn history_loaded(identity: servers::Identity) {
   let unnamed = UNNAMED
      .lock()
      .unwrap_or_else(PoisonError::into_inner)
      .take();
   if let Some(index) = unnamed {
      log::info!("account switch: account {index} learned from history");
      learn(index, identity);
   }
}

/// The record's last word is the account number Phoenix logs.
fn account_index(account: *const u8) -> Option<usize> {
   let bytes = x86::read_memory(account as usize, ACCOUNT_SIZE)?;
   let last = bytes.as_chunks::<8>().0.last()?;
   Some(usize::from_le_bytes(*last)).filter(|&index| index < 64)
}

/// The record's fourth word is the account's user ID.
fn account_user(account: *const u8) -> Option<u64> {
   let bytes = x86::read_memory(account as usize, ACCOUNT_SIZE)?;
   let word = bytes.as_chunks::<8>().0.get(USER_WORD)?;
   Some(u64::from_le_bytes(*word)).filter(|&id| id != 0)
}

unsafe extern "C" fn detour(
   app: *mut c_void,
   account: *const u8,
   flag: usize,
   a: usize,
   b: usize,
   c: usize,
   d: usize,
) -> usize {
   let (index, user) = if account.is_null() {
      (None, None)
   } else {
      (account_index(account), account_user(account))
   };
   if let Some(index) = index {
      let named = user.and_then(servers::identity_of_user);
      if let Some(identity) = named {
         learn(index, identity);
      }
      let known = named.or_else(|| {
         KNOWN
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .and_then(|known| known.get(&index).copied())
      });
      if let Some(identity) = known {
         servers::set_viewer(identity);
      }
      *UNNAMED.lock().unwrap_or_else(PoisonError::into_inner) = known.is_none().then_some(index);
   }
   let original = ORIGINAL.wait();
   // SAFETY: forwards every argument to the original switch.
   unsafe { original(app, account, flag, a, b, c, d) }
}
