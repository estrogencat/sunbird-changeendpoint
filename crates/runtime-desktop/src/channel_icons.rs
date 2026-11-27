//! Custom channel icons on Spacebar servers. Spacebar lets any channel carry
//! its own picture, which Phoenix only draws for group DMs, so sidebar rows of
//! channels with one are drawn the way a group DM's picture is. Those
//! pictures are masked into circles while they paint, so the mask and the
//! rounding are skipped for these rows and the whole picture shows.

use std::{
   cell::Cell,
   collections::HashSet,
   sync::{Mutex, Once, OnceLock, PoisonError, RwLock},
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
type PaintFn = unsafe extern "C" fn(usize, usize, usize, usize) -> usize;
type MaskFn = unsafe extern "C" fn(usize, usize, usize, usize) -> usize;
type PaintImageFn =
   unsafe extern "C" fn(usize, usize, usize, *const f32, usize, usize, usize, usize) -> usize;

/// The paint slot in a GPUI element's vtable, after drop, size, align and
/// three other methods.
const PAINT_SLOT: usize = 6;
const SQUARE: [f32; 4] = [0.0; 4];

static PAINT: OnceLock<(usize, PaintFn)> = OnceLock::new();
static PAINT_HOOK: Once = Once::new();
static MASK: OnceLock<MaskFn> = OnceLock::new();
static PAINT_IMAGE: OnceLock<PaintImageFn> = OnceLock::new();
/// Pictures built for channel rows this frame, by element.
static ROW_PICTURES: Mutex<Option<HashSet<usize>>> = Mutex::new(None);

thread_local! {
   /// How deep this thread is in painting a channel row's picture.
   static PAINTING: Cell<u32> = const { Cell::new(0) };
}

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
   unmask(image);
}

fn hook(image: &Image<'static>, func: usize, detour: usize) -> Result<usize, String> {
   let prologue = desktop::entry_prologue(image, func)?;
   // SAFETY: each detour takes and returns what the function it replaces
   // does.
   unsafe { image.hook_entry(func, prologue.pattern, prologue.len, detour) }
      .map_err(|err| err.to_string())
}

fn unmask(image: &Image<'static>) {
   let hooked = desktop::picture_mask(image).and_then(|found| {
      let mask = hook(image, found.mask, mask_detour as *const () as usize)?;
      let paint = hook(image, found.paint_image, paint_image_detour as *const () as usize)?;
      Ok((mask, paint))
   });
   match hooked {
      // SAFETY: the trampolines replay each prologue and jump back.
      Ok((mask, paint)) => unsafe {
         let _ = MASK.set(std::mem::transmute::<usize, MaskFn>(mask));
         let _ = PAINT_IMAGE.set(std::mem::transmute::<usize, PaintImageFn>(paint));
      },
      Err(err) => log::warn!("channel icons: {err}, pictures stay round"),
   }
}

/// Hooks the paint method of the element channel row pictures are, from its
/// vtable, the first time one is built.
fn hook_paint(vtable: usize) {
   PAINT_HOOK.call_once(|| {
      let hooked = phoenix_runtime::target().map_err(|err| err.to_string()).and_then(|image| {
         // SAFETY: `vtable` is the live vtable of an element Phoenix just
         // built, whose slots are function pointers.
         let paint = unsafe { (vtable as *const usize).add(PAINT_SLOT).read() };
         if image.function_bounds(paint).is_none() && !image.is_function(paint) {
            return Err(format!("{paint:#x} is not a function"));
         }
         hook(&image, paint, paint_detour as *const () as usize)
      });
      match hooked {
         // SAFETY: the trampoline replays the prologue and jumps back.
         Ok(trampoline) => {
            let _ = PAINT
               .set((vtable, unsafe { std::mem::transmute::<usize, PaintFn>(trampoline) }));
         },
         Err(err) => log::warn!("channel icons: {err}, pictures stay round"),
      }
   });
}

fn row_pictures<T>(f: impl FnOnce(&mut HashSet<usize>) -> T) -> T {
   f(ROW_PICTURES
      .lock()
      .unwrap_or_else(PoisonError::into_inner)
      .get_or_insert_default())
}

unsafe extern "C" fn paint_detour(element: usize, arg2: usize, arg3: usize, arg4: usize) -> usize {
   let ours = row_pictures(|set| set.remove(&element));
   if ours {
      PAINTING.with(|depth| depth.set(depth.get() + 1));
   }
   let (_, original) = PAINT.wait();
   // SAFETY: forwards every argument to the original.
   let result = unsafe { original(element, arg2, arg3, arg4) };
   if ours {
      PAINTING.with(|depth| depth.set(depth.get().saturating_sub(1)));
   }
   result
}

fn painting_row() -> bool {
   PAINTING.with(|depth| depth.get() > 0)
}

unsafe extern "C" fn mask_detour(arg1: usize, arg2: usize, arg3: usize, arg4: usize) -> usize {
   if painting_row() {
      return 0;
   }
   // SAFETY: forwards every argument to the original.
   unsafe { MASK.wait()(arg1, arg2, arg3, arg4) }
}

unsafe extern "C" fn paint_image_detour(
   window: usize,
   bounds: usize,
   mask_bounds: usize,
   radii: *const f32,
   arg5: usize,
   arg6: usize,
   arg7: usize,
   arg8: usize,
) -> usize {
   let radii = if painting_row() { SQUARE.as_ptr() } else { radii };
   // SAFETY: forwards every argument to the original, with corners Phoenix
   // reads the same way.
   unsafe { PAINT_IMAGE.wait()(window, bounds, mask_bounds, radii, arg5, arg6, arg7, arg8) }
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
   let ours = !matches!(kind & 0xFFFF, 1 | 3) && has_icon(channel);
   let kind = if ours { GROUP_PICTURE } else { kind };
   // SAFETY: forwards every argument to the original.
   let result = unsafe { ORIGINAL.wait()(out, channel, kind, arg4, arg5, arg6, arg7, arg8) };
   if ours && !out.is_null() && MASK.get().is_some() {
      // SAFETY: `out` is the `Option<AnyElement>` the original just wrote:
      // zero in its first word for none, else the element and its vtable in
      // the next two.
      let [first, element, vtable] = unsafe { out.cast::<[usize; 3]>().read_unaligned() };
      if first != 0 && element != 0 && vtable != 0 {
         hook_paint(vtable);
         if PAINT.get().is_some_and(|&(hooked, _)| hooked == vtable) {
            row_pictures(|set| {
               if set.len() > 4096 {
                  set.clear();
               }
               set.insert(element);
            });
         }
      }
   }
   result
}
