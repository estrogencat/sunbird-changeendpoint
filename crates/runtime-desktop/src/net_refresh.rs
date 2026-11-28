//! Tells Phoenix the network went down and came back when its window returns
//! after a long time away. Connections that died quietly while idle otherwise
//! keep every new image waiting until a restart, and only going offline and
//! back makes Phoenix drop them: it checks its adapters on each notification
//! and ignores one that leaves it online.

use std::{
   ffi::c_void,
   sync::{
      Mutex,
      OnceLock,
      PoisonError,
      atomic::{AtomicBool, AtomicUsize, Ordering},
   },
   time::{Duration, Instant},
};

use phoenix_hook::image::Image;
use phoenix_sigs::desktop;

type Callback = unsafe extern "system" fn(context: *mut c_void, row: *mut c_void, kind: u32);
type RegisterFn = unsafe extern "system" fn(
   family: u16,
   callback: Callback,
   context: *mut c_void,
   initial: u8,
   handle: *mut *mut c_void,
) -> u32;
type CancelFn = unsafe extern "system" fn(handle: *mut c_void) -> u32;
type ConnectionFn = unsafe extern "C" fn() -> u8;

/// How long the window stays in the background before coming back refreshes.
const AWAY: Duration = Duration::from_mins(20);
const POLL: Duration = Duration::from_secs(5);
/// `MibParameterNotification`, the kind a changed interface reports.
const PARAMETER_CHANGED: u32 = 0;
/// What the adapter check answers when no adapter is up.
const OFFLINE: u8 = 0;
/// Phoenix believes a change only when its check still agrees 1.5 s later.
const OFFLINE_FOR: Duration = Duration::from_secs(3);

static REGISTER: OnceLock<RegisterFn> = OnceLock::new();
static CANCEL: OnceLock<CancelFn> = OnceLock::new();
static CALLBACK: OnceLock<usize> = OnceLock::new();
static CONNECTION: OnceLock<ConnectionFn> = OnceLock::new();
/// Makes the adapter check answer offline.
static GO_OFFLINE: AtomicBool = AtomicBool::new(false);
static CHECKS: AtomicUsize = AtomicUsize::new(0);

struct Watch {
   handle:  usize,
   context: usize,
}

/// Phoenix's live registration, gone once it cancels.
static WATCH: Mutex<Option<Watch>> = Mutex::new(None);

pub fn install(image: &Image<'static>) {
   let found = desktop::network_watch(image).and_then(|found| {
      desktop::entry_prologue(image, found.connection).map(|prologue| (found, prologue))
   });
   let (found, prologue) = match found {
      Ok(found) => found,
      Err(err) => {
         log::warn!("net refresh: {err}, images may stall after long idle");
         return;
      },
   };
   let (Some(register), Some(cancel)) =
      (image.read_usize(found.register), image.read_usize(found.cancel))
   else {
      log::warn!("net refresh: import slots are unreadable");
      return;
   };
   // SAFETY: both slots hold the resolved iphlpapi functions with these
   // signatures.
   unsafe {
      let _ = REGISTER.set(std::mem::transmute::<usize, RegisterFn>(register));
      let _ = CANCEL.set(std::mem::transmute::<usize, CancelFn>(cancel));
   }
   let _ = CALLBACK.set(found.callback);
   // SAFETY: runs from `prepare` before Discord's threads, and
   // `connection_detour` takes no arguments and returns a byte like the check.
   let trampoline = match unsafe {
      image.hook_entry(
         found.connection,
         prologue.pattern,
         prologue.len,
         connection_detour as *const () as usize,
      )
   } {
      Ok(trampoline) => trampoline,
      Err(err) => {
         log::warn!("net refresh: adapter check hook failed: {err}");
         return;
      },
   };
   // SAFETY: the trampoline replays the prologue and jumps back into the check.
   let _ = CONNECTION.set(unsafe { std::mem::transmute::<usize, ConnectionFn>(trampoline) });
   // SAFETY: runs from `prepare` before Discord's threads, the slots are
   // aligned import entries, and the detours share their signatures.
   if let Err(err) = unsafe {
      image.swap_words(&[
         (found.register, register_detour as *const () as usize),
         (found.cancel, cancel_detour as *const () as usize),
      ])
   } {
      log::warn!("net refresh: hook failed: {err}");
      return;
   }
   if let Err(err) = std::thread::Builder::new()
      .name("sunbird-net-refresh".to_owned())
      .spawn(watch_foreground)
   {
      log::warn!("net refresh: could not start: {err}");
   }
}

unsafe extern "system" fn register_detour(
   family: u16,
   callback: Callback,
   context: *mut c_void,
   initial: u8,
   handle: *mut *mut c_void,
) -> u32 {
   let Some(original) = REGISTER.get() else {
      return 1;
   };
   // SAFETY: forwards every argument to the real registration.
   let status = unsafe { original(family, callback, context, initial, handle) };
   if status == 0 && !handle.is_null() && CALLBACK.get() == Some(&(callback as usize)) {
      // SAFETY: a successful registration wrote the handle.
      let handle = unsafe { *handle } as usize;
      *WATCH.lock().unwrap_or_else(PoisonError::into_inner) = Some(Watch {
         handle,
         context: context as usize,
      });
   }
   status
}

unsafe extern "C" fn connection_detour() -> u8 {
   CHECKS.fetch_add(1, Ordering::Relaxed);
   if GO_OFFLINE.load(Ordering::Acquire) {
      return OFFLINE;
   }
   let Some(original) = CONNECTION.get() else {
      return OFFLINE;
   };
   // SAFETY: the check's trampoline, which takes no arguments.
   unsafe { original() }
}

unsafe extern "system" fn cancel_detour(handle: *mut c_void) -> u32 {
   let Some(original) = CANCEL.get() else {
      return 1;
   };
   {
      let mut watch = WATCH.lock().unwrap_or_else(PoisonError::into_inner);
      if watch.as_ref().is_some_and(|watch| watch.handle == handle as usize) {
         *watch = None;
      }
   }
   // SAFETY: forwards the handle to the real cancel.
   unsafe { original(handle) }
}

fn watch_foreground() {
   let mut away_since: Option<Instant> = None;
   loop {
      std::thread::sleep(POLL);
      if !in_front() {
         away_since.get_or_insert_with(Instant::now);
         continue;
      }
      if let Some(since) = away_since.take()
         && since.elapsed() >= AWAY
      {
         refresh(since.elapsed());
      }
   }
}

fn refresh(away: Duration) {
   let Some(&callback) = CALLBACK.get() else {
      return;
   };
   // held across the call so Phoenix can't cancel and free the context
   // mid-call.
   let guard = WATCH.lock().unwrap_or_else(PoisonError::into_inner);
   let Some(watch) = guard.as_ref() else {
      return;
   };
   log::info!(
      "net refresh: back after {} min, refreshing connections",
      away.as_secs() / 60
   );
   // SAFETY: `callback` is the function Phoenix registered with this live
   // context, and it reads neither the row nor the kind.
   let notify = || unsafe {
      let callback = std::mem::transmute::<usize, Callback>(callback);
      callback(watch.context as *mut c_void, std::ptr::null_mut(), PARAMETER_CHANGED);
   };
   let checks = CHECKS.load(Ordering::Relaxed);
   GO_OFFLINE.store(true, Ordering::Release);
   notify();
   std::thread::sleep(OFFLINE_FOR);
   GO_OFFLINE.store(false, Ordering::Release);
   notify();
   drop(guard);
   if CHECKS.load(Ordering::Relaxed) == checks {
      log::warn!("net refresh: Phoenix never checked its adapters");
   }
}

fn in_front() -> bool {
   #[link(name = "user32")]
   unsafe extern "system" {
      fn GetForegroundWindow() -> isize;
      fn GetWindowThreadProcessId(hwnd: isize, pid: *mut u32) -> u32;
   }
   // SAFETY: no arguments.
   let window = unsafe { GetForegroundWindow() };
   if window == 0 {
      return false;
   }
   let mut process = 0u32;
   // SAFETY: `window` came from the system and `process` is a valid out slot.
   unsafe { GetWindowThreadProcessId(window, &raw mut process) };
   process == std::process::id()
}
