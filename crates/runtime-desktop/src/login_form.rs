use std::{ffi::c_void, sync::OnceLock};

use phoenix_hook::{image::Image, x86};
use phoenix_sigs::desktop;

use crate::{
   gpui::{self, RawAnyElement},
   servers,
};

type LoginFormFn = unsafe extern "C" fn(
   sret: *mut RawAnyElement,
   this: *mut c_void,
   step: *const u64,
   flags: usize,
   window: *mut c_void,
   cx: *mut c_void,
) -> *mut RawAnyElement;

static ORIGINAL: OnceLock<LoginFormFn> = OnceLock::new();

const CREDENTIALS: u64 = 0;

pub fn install(image: &Image<'static>) {
   let func = match desktop::login_form(image) {
      Ok(func) => func,
      Err(err) => {
         log::warn!("login form: {err}, no server picker");
         return;
      },
   };
   // SAFETY: runs from `prepare` before Discord's threads, and `detour` has the
   // form builder's six-argument sret ABI.
   let trampoline = match unsafe {
      image.hook_entry(
         func,
         detour as *const () as usize,
      )
   } {
      Ok(trampoline) => trampoline,
      Err(err) => {
         log::warn!("login form: hook failed: {err}");
         return;
      },
   };
   // SAFETY: the trampoline replays the prologue and jumps back into `func`.
   let _ = ORIGINAL.set(unsafe { std::mem::transmute::<usize, LoginFormFn>(trampoline) });
   log::info!("login form: server picker hooked @ {func:#x}");
}

unsafe extern "C" fn detour(
   sret: *mut RawAnyElement,
   this: *mut c_void,
   step: *const u64,
   flags: usize,
   window: *mut c_void,
   cx: *mut c_void,
) -> *mut RawAnyElement {
   let original = ORIGINAL.wait();
   // SAFETY: forwards every argument to the original builder.
   let out = unsafe { original(sret, this, step, flags, window, cx) };
   let asks_credentials = x86::read_memory(step as usize, 8)
      .and_then(|bytes| bytes.try_into().ok())
      .is_some_and(|bytes| u64::from_le_bytes(bytes) == CREDENTIALS);
   if out.is_null() || !asks_credentials {
      return out;
   }
   let _ = std::panic::catch_unwind(|| {
      let routes = servers::routes();
      if !routes.active() {
         return;
      }
      let (choices, chosen) = routes.login_choices();
      gpui::wrap_with_login_servers(out, "Server", &choices, &chosen);
   });
   out
}
