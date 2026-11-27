//! Custom channel icons on Spacebar servers. Spacebar lets any channel carry
//! its own picture, which Phoenix only draws for group DMs, so sidebar rows of
//! channels with one are drawn the way a group DM's picture is.

use std::{
   collections::HashSet,
   sync::{OnceLock, PoisonError, RwLock},
};

use phoenix_hook::image::Image;
use phoenix_sigs::desktop;
use serde_json::Value;

/// The row kind that draws a group DM's own picture.
const GROUP_PICTURE: usize = 3;
/// `nop word [rax+rax+0]`, as long as the `jne` it replaces.
const NOP6: [u8; 6] = [0x66, 0x0F, 0x1F, 0x44, 0x00, 0x00];

type LookupFn = unsafe extern "C" fn(
   *mut u8,
   u64,
   usize,
   usize,
   usize,
   usize,
   usize,
   usize,
) -> *mut u8;

static ORIGINAL: OnceLock<LookupFn> = OnceLock::new();
/// Channels whose server gave them a picture.
static WITH_ICON: RwLock<Option<HashSet<u64>>> = RwLock::new(None);

/// Notes whether a channel the gateway described has a picture.
pub fn note(channel: &Value) {
   let Some(id) = channel
      .get("id")
      .and_then(Value::as_str)
      .and_then(|id| id.parse::<u64>().ok())
   else {
      return;
   };
   let has = channel
      .get("icon")
      .and_then(Value::as_str)
      .is_some_and(|icon| !icon.is_empty());
   let mut guard = WITH_ICON.write().unwrap_or_else(PoisonError::into_inner);
   let set = guard.get_or_insert_default();
   if has {
      set.insert(id);
   } else {
      set.remove(&id);
   }
   drop(guard);
}

fn has_icon(id: u64) -> bool {
   WITH_ICON
      .read()
      .unwrap_or_else(PoisonError::into_inner)
      .as_ref()
      .is_some_and(|set| set.contains(&id))
}

pub fn install(image: &Image<'static>) {
   let found = desktop::channel_row_picture(image).and_then(|found| {
      desktop::entry_prologue(image, found.lookup).map(|prologue| (found, prologue))
   });
   let (found, prologue) = match found {
      Ok(found) => found,
      Err(err) => {
         log::warn!("channel icons: {err}, keeping Phoenix's");
         return;
      },
   };
   // SAFETY: runs from `prepare` before Discord's threads; the `jne` is
   // replaced by a nop of the same length.
   if let Err(err) = unsafe { image.write_code(found.kind_check, &NOP6) } {
      log::warn!("channel icons: kind check patch failed: {err}");
      return;
   }
   // SAFETY: `detour` takes the lookup's eight arguments and returns what it
   // returns.
   let trampoline = match unsafe {
      image.hook_entry(
         found.lookup,
         prologue.pattern,
         prologue.len,
         detour as *const () as usize,
      )
   } {
      Ok(trampoline) => trampoline,
      Err(err) => {
         log::warn!("channel icons: hook failed: {err}");
         return;
      },
   };
   // SAFETY: the trampoline replays the prologue and jumps back into the
   // function.
   let _ = ORIGINAL.set(unsafe { std::mem::transmute::<usize, LookupFn>(trampoline) });
   log::info!("channel icons: hooked @ {:#x}", found.lookup);
}

unsafe extern "C" fn detour(
   out: *mut u8,
   channel: u64,
   kind: usize,
   arg4: usize,
   arg5: usize,
   arg6: usize,
   arg7: usize,
   arg8: usize,
) -> *mut u8 {
   let kind = if !matches!(kind & 0xFFFF, 1 | 3) && has_icon(channel) {
      GROUP_PICTURE
   } else {
      kind
   };
   // SAFETY: forwards every argument to the original.
   unsafe { ORIGINAL.wait()(out, channel, kind, arg4, arg5, arg6, arg7, arg8) }
}
