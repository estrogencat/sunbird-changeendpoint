//! Holds the account on screen to its Spacebar server's upload limit. Phoenix
//! picks the limit by Nitro tier, which Spacebar accounts never have, so a
//! server allowing far more was held to 10 MB.

use std::{
   collections::HashMap,
   ffi::c_void,
   sync::{
      Mutex,
      OnceLock,
      PoisonError,
      atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
   },
};

use phoenix_hook::image::Image;
use phoenix_runtime::fetch::Request;
use phoenix_sigs::desktop;

use crate::servers::{self, Identity};

type PerksFn = unsafe extern "C" fn(out: *mut u8, state: *mut c_void) -> *mut u8;

static ORIGINAL: OnceLock<PerksFn> = OnceLock::new();
static FIELD: AtomicUsize = AtomicUsize::new(0);
/// The limit for the account on screen in bytes, 0 for Phoenix's own.
static LIMIT: AtomicU64 = AtomicU64::new(0);
static STARTED: AtomicBool = AtomicBool::new(false);
/// Each server's limit by API, `None` while it is being asked.
static LIMITS: Mutex<Option<HashMap<String, Option<u64>>>> = Mutex::new(None);

pub fn install(image: &Image<'static>) {
   let found = desktop::upload_limit(image).and_then(|found| {
      desktop::entry_prologue(image, found.func).map(|prologue| (found, prologue))
   });
   let (found, prologue) = match found {
      Ok(found) => found,
      Err(err) => {
         log::warn!("upload limit: {err}, keeping Phoenix's");
         return;
      },
   };
   // SAFETY: runs from `prepare` before Discord's threads, and `detour` takes
   // the perks function's two arguments and returns what it returns.
   let trampoline = match unsafe {
      image.hook_entry(
         found.func,
         prologue.pattern,
         prologue.len,
         detour as *const () as usize,
      )
   } {
      Ok(trampoline) => trampoline,
      Err(err) => {
         log::warn!("upload limit: hook failed: {err}");
         return;
      },
   };
   FIELD.store(found.field, Ordering::Release);
   // SAFETY: the trampoline replays the prologue and jumps back into the
   // function.
   let _ = ORIGINAL.set(unsafe { std::mem::transmute::<usize, PerksFn>(trampoline) });
   log::info!("upload limit: hooked @ {:#x}", found.func);
}

unsafe extern "C" fn detour(out: *mut u8, state: *mut c_void) -> *mut u8 {
   let original = ORIGINAL.wait();
   // SAFETY: forwards both arguments to the original.
   let out = unsafe { original(out, state) };
   if !STARTED.swap(true, Ordering::AcqRel) {
      let _ = std::thread::Builder::new()
         .name("sunbird-upload-limit".to_owned())
         .spawn(refresh);
   }
   let limit = LIMIT.load(Ordering::Acquire);
   if limit != 0 && !out.is_null() {
      // SAFETY: `out` is the perks the original just wrote, which hold the
      // limit as a `u64` at `FIELD`.
      unsafe {
         out
            .add(FIELD.load(Ordering::Acquire))
            .cast::<u64>()
            .write_unaligned(limit);
      }
   }
   out
}

/// Picks the limit for the account on screen again.
pub fn refresh() {
   if ORIGINAL.get().is_none() {
      return;
   }
   let routes = servers::routes();
   let server = match servers::viewing() {
      Some(Identity::Jwt(id)) if routes.tweaks().upload_limit => {
         routes.of_user(id).cloned().or_else(|| servers::legacy().cloned())
      },
      _ => None,
   };
   let limit = server.and_then(|server| known(&server.api));
   LIMIT.store(limit.unwrap_or(0), Ordering::Release);
}

/// The server's limit, or `None` while it is asked in the background, which
/// refreshes once it answers.
fn known(api: &str) -> Option<u64> {
   let mut guard = LIMITS.lock().unwrap_or_else(PoisonError::into_inner);
   let limits = guard.get_or_insert_default();
   if let Some(limit) = limits.get(api) {
      return *limit;
   }
   limits.insert(api.to_owned(), None);
   drop(guard);
   let api = api.to_owned();
   let asked = std::thread::Builder::new()
      .name("sunbird-upload-limit".to_owned())
      .spawn(move || {
         let limit = ask(&api);
         let host = crate::discover::host_of(&api).to_owned();
         let mut guard = LIMITS.lock().unwrap_or_else(PoisonError::into_inner);
         let limits = guard.get_or_insert_default();
         if let Some(limit) = limit {
            limits.insert(api, Some(limit));
            drop(guard);
            log::info!("upload limit: {limit} bytes on {host}");
            refresh();
         } else {
            limits.remove(&api);
            drop(guard);
            log::info!("upload limit: {host} didn't say");
         }
      });
   if let Err(err) = asked {
      log::warn!("upload limit: {err}");
   }
   None
}

/// The largest attachment Spacebar's limits allow.
fn ask(api: &str) -> Option<u64> {
   #[derive(serde::Deserialize)]
   #[serde(rename_all = "camelCase")]
   struct Message {
      max_attachment_size: u64,
   }
   #[derive(serde::Deserialize)]
   struct Limits {
      message: Message,
   }
   let response = crate::fetch::send(&Request {
      url: format!("{}/policies/instance/limits", api.trim_end_matches('/')),
      method: "GET".to_owned(),
      headers: vec![("Accept".to_owned(), "application/json".to_owned())],
      body: None,
   })
   .ok()?;
   if !(200..300).contains(&response.status) {
      return None;
   }
   let limits = serde_json::from_slice::<Limits>(&response.body).ok()?;
   Some(limits.message.max_attachment_size).filter(|&size| size > 0)
}
