//! Plumbing for calling Java from any thread, shared by the UI bridge and
//! fetches.
//!
//! Every call goes through [`SAVED`], the JNI table as ART had it before the
//! bridge swapped two of its slots, so phoenix never sees its own calls.

use std::{
   ffi::{
      CStr,
      c_char,
      c_int,
      c_void,
   },
   ptr,
   sync::{
      OnceLock,
      atomic::{
         AtomicPtr,
         Ordering,
      },
   },
};

use jni_sys::{
   JNI_OK,
   JNI_VERSION_1_6,
   JNIEnv,
   JNINativeInterface__1_6,
   JavaVM,
   jbyte,
   jbyteArray,
   jclass,
   jint,
   jmethodID,
   jsize,
   jstring,
};

pub struct Table(pub JNINativeInterface__1_6);

// SAFETY: a copy of ART's table holds function pointers into libart, which are
// valid on every thread.
unsafe impl Send for Table {}
// SAFETY: the copy is never written after it is saved.
unsafe impl Sync for Table {}

pub static SAVED: OnceLock<Table> = OnceLock::new();

macro_rules! jni {
   ($env:expr, $name:ident $(, $arg:expr)* $(,)?) => {
      ($crate::jvm::table($env).$name)($env $(, $arg)*)
   };
}

/// A Java class, written once and used both to find it and in signatures.
#[derive(Clone, Copy)]
pub struct Class {
   slashed: &'static CStr,
   dotted:  &'static CStr,
}

impl Class {
   #[doc(hidden)]
   pub const fn new(slashed: &'static str, dotted: &'static str) -> Self {
      Self {
         slashed: nul_terminated(slashed),
         dotted:  nul_terminated(dotted),
      }
   }

   /// The binary name with `.`, as `ClassLoader.loadClass` takes it.
   pub const fn dotted(self) -> &'static CStr {
      self.dotted
   }

   #[doc(hidden)]
   pub const fn slashed(self) -> &'static [u8] {
      self.slashed.to_bytes()
   }

   /// The class through `FindClass`, which only sees the boot and system
   /// class loaders.
   ///
   /// # Safety
   ///
   /// `env` is the current thread's `JNIEnv`.
   pub unsafe fn find(self, env: *mut JNIEnv) -> Result<jclass, Missing> {
      // SAFETY: forwarded from the caller, and the name is NUL-terminated.
      let class = unsafe { jni!(env, FindClass, self.slashed.as_ptr()) };
      if class.is_null() {
         // SAFETY: forwarded from the caller.
         unsafe { clear_exception(env) };
         return Err(Missing(self.dotted.to_string_lossy().into_owned()));
      }
      Ok(class)
   }
}

#[doc(hidden)]
pub const fn nul_terminated(text: &'static str) -> &'static CStr {
   match CStr::from_bytes_with_nul(text.as_bytes()) {
      Ok(text) => text,
      Err(_) => panic!("a JNI name has a NUL inside"),
   }
}

#[doc(hidden)]
pub const fn joined_len(parts: &[&[u8]]) -> usize {
   let (mut len, mut index) = (0, 0);
   while index < parts.len() {
      len += parts[index].len();
      index += 1;
   }
   len
}

#[doc(hidden)]
pub const fn join<const LEN: usize>(parts: &[&[u8]]) -> [u8; LEN] {
   let mut out = [0; LEN];
   let (mut at, mut part) = (0, 0);
   while part < parts.len() {
      let mut index = 0;
      while index < parts[part].len() {
         out[at] = parts[part][index];
         at += 1;
         index += 1;
      }
      part += 1;
   }
   out
}

/// A [`Class`] from its Java path, with `::` between packages and `.` before
/// a nested class:
/// `class!(android::database::sqlite::SQLiteDatabase.CursorFactory)`.
macro_rules! class {
   (@path [$($slashed:tt)*] [$($dotted:tt)*] [$package:ident :: $($rest:tt)+]) => {
      $crate::jvm::class!(@path
         [$($slashed)* stringify!($package), "/",]
         [$($dotted)* stringify!($package), ".",]
         [$($rest)+])
   };
   (@path [$($slashed:tt)*] [$($dotted:tt)*] [$class:ident . $($rest:tt)+]) => {
      $crate::jvm::class!(@path
         [$($slashed)* stringify!($class), "$",]
         [$($dotted)* stringify!($class), "$",]
         [$($rest)+])
   };
   (@path [$($slashed:tt)*] [$($dotted:tt)*] [$class:ident]) => {
      $crate::jvm::Class::new(
         concat!($($slashed)* stringify!($class), "\0"),
         concat!($($dotted)* stringify!($class), "\0"),
      )
   };
   ($($path:tt)+) => {
      $crate::jvm::class!(@path [] [] [$($path)+])
   };
}

pub const STRING: Class = class!(java::lang::String);
pub const CLASS: Class = class!(java::lang::Class);
pub const CLASS_LOADER: Class = class!(java::lang::ClassLoader);

/// A JNI method descriptor as a `&'static CStr`, with primitives by their
/// Java names and classes as [`Class`] constants:
/// `signature!((STRING, [byte]) -> boolean)` is `c"(Ljava/lang/String;[B)Z"`.
macro_rules! signature {
   (($($args:tt)*) -> $($ret:tt)+) => {
      $crate::jvm::signature!(@type [b"(",] [$($args)* @close $($ret)+])
   };
   (@type [$($done:tt)*] []) => {
      const {
         const PARTS: &[&[u8]] = &[$($done)* b"\0"];
         const BYTES: [u8; $crate::jvm::joined_len(PARTS)] = $crate::jvm::join(PARTS);
         match ::core::ffi::CStr::from_bytes_with_nul(&BYTES) {
            Ok(signature) => signature,
            Err(_) => panic!("signature! has a NUL inside"),
         }
      }
   };
   (@type [$($done:tt)*] [, $($rest:tt)*]) => {
      $crate::jvm::signature!(@type [$($done)*] [$($rest)*])
   };
   (@type [$($done:tt)*] [@close $($rest:tt)*]) => {
      $crate::jvm::signature!(@type [$($done)* b")",] [$($rest)*])
   };
   (@type [$($done:tt)*] [[$($inner:tt)+] $($rest:tt)*]) => {
      $crate::jvm::signature!(@type [$($done)* b"[",] [$($inner)+ $($rest)*])
   };
   (@type [$($done:tt)*] [void $($rest:tt)*]) => {
      $crate::jvm::signature!(@type [$($done)* b"V",] [$($rest)*])
   };
   (@type [$($done:tt)*] [boolean $($rest:tt)*]) => {
      $crate::jvm::signature!(@type [$($done)* b"Z",] [$($rest)*])
   };
   (@type [$($done:tt)*] [byte $($rest:tt)*]) => {
      $crate::jvm::signature!(@type [$($done)* b"B",] [$($rest)*])
   };
   (@type [$($done:tt)*] [int $($rest:tt)*]) => {
      $crate::jvm::signature!(@type [$($done)* b"I",] [$($rest)*])
   };
   (@type [$($done:tt)*] [long $($rest:tt)*]) => {
      $crate::jvm::signature!(@type [$($done)* b"J",] [$($rest)*])
   };
   (@type [$($done:tt)*] [double $($rest:tt)*]) => {
      $crate::jvm::signature!(@type [$($done)* b"D",] [$($rest)*])
   };
   (@type [$($done:tt)*] [$class:ident $($rest:tt)*]) => {
      $crate::jvm::signature!(@type [$($done)* b"L", $class.slashed(), b";",] [$($rest)*])
   };
}

/// # Safety
///
/// `env` is the current thread's `JNIEnv`.
pub unsafe fn table(env: *mut JNIEnv) -> &'static JNINativeInterface__1_6 {
   SAVED.get().map_or_else(
      // SAFETY: forwarded from the caller. ART's table lives for the whole
      // process and starts with the JNI 1.6 layout.
      || unsafe { &(**env).v1_6 },
      |saved| &saved.0,
   )
}

pub fn vm() -> *mut JavaVM {
   type GetCreatedVms = unsafe extern "system" fn(*mut *mut JavaVM, jsize, *mut jsize) -> jint;
   static VM: AtomicPtr<JavaVM> = AtomicPtr::new(ptr::null_mut());

   let cached = VM.load(Ordering::Acquire);
   if !cached.is_null() {
      return cached;
   }

   // SAFETY: the names are NUL-terminated, and the symbol, when present, is
   // JNI's `JNI_GetCreatedJavaVMs` with this signature.
   unsafe {
      let mut symbol = libc::dlsym(libc::RTLD_DEFAULT, c"JNI_GetCreatedJavaVMs".as_ptr());
      if symbol.is_null() {
         let handle = libc::dlopen(c"libnativehelper.so".as_ptr(), libc::RTLD_NOW);
         if !handle.is_null() {
            symbol = libc::dlsym(handle, c"JNI_GetCreatedJavaVMs".as_ptr());
         }
      }
      if symbol.is_null() {
         symbol = art_symbol(b"JNI_GetCreatedJavaVMs");
         if symbol.is_null() {
            return ptr::null_mut();
         }
         log::info!("jvm: JNI_GetCreatedJavaVMs found in libart's symbol table");
      }

      let get_created = std::mem::transmute::<*mut c_void, GetCreatedVms>(symbol);
      let (mut vm, mut count) = (ptr::null_mut(), 0);
      if get_created(&raw mut vm, 1, &raw mut count) != JNI_OK || count < 1 {
         return ptr::null_mut();
      }
      VM.store(vm, Ordering::Release);
      vm
   }
}

/// The address of `name` in the loaded `libart.so`, read from its dynamic
/// symbol table, or null.
///
/// Before Android 12 (API 31) `libnativehelper.so` is not a public library
/// and libart lives outside the app's linker namespace, so neither `dlsym`
/// nor `dlopen` reaches `JNI_GetCreatedJavaVMs`. `dl_iterate_phdr` still
/// reports every loaded library, whatever its namespace.
fn art_symbol(name: &'static [u8]) -> *mut c_void {
   struct Search {
      name:  &'static [u8],
      found: Option<usize>,
   }

   unsafe extern "C" fn visit(
      info: *mut libc::dl_phdr_info,
      _size: usize,
      out: *mut c_void,
   ) -> c_int {
      // SAFETY: the loader passes a valid record, and `out` is the Search
      // below.
      let (info, search) = unsafe { (&*info, &mut *out.cast::<Search>()) };
      // SAFETY: dlpi_name is NUL-terminated when present.
      if info.dlpi_name.is_null()
         || !unsafe { CStr::from_ptr(info.dlpi_name) }
            .to_bytes()
            .ends_with(b"/libart.so")
      {
         return 0;
      }
      // SAFETY: libart stays loaded for the life of the process, and the
      // loader reported its headers.
      search.found = unsafe { dynamic_symbol(info, search.name) };
      1
   }

   let mut search = Search { name, found: None };
   // SAFETY: `visit` only touches `search` through the pointer it is handed.
   unsafe { libc::dl_iterate_phdr(Some(visit), (&raw mut search).cast()) };
   search
      .found
      .map_or(ptr::null_mut(), |addr| addr as *mut c_void)
}

#[cfg(target_pointer_width = "64")]
#[repr(C)]
struct Sym {
   name:   u32,
   _info:  u8,
   _other: u8,
   shndx:  u16,
   value:  usize,
   _size:  u64,
}

#[cfg(target_pointer_width = "32")]
#[repr(C)]
struct Sym {
   name:   u32,
   value:  usize,
   _size:  u32,
   _info:  u8,
   _other: u8,
   shndx:  u16,
}

/// Looks `name` up through the module's GNU or `SysV` hash table.
///
/// # Safety
///
/// `info` describes a loaded module whose dynamic section, hash tables, and
/// symbol and string tables are mapped and well formed.
unsafe fn dynamic_symbol(info: &libc::dl_phdr_info, name: &[u8]) -> Option<usize> {
   const DT_NULL: isize = 0;
   const DT_HASH: isize = 4;
   const DT_STRTAB: isize = 5;
   const DT_SYMTAB: isize = 6;
   const DT_GNU_HASH: isize = 0x6FFF_FEF5;

   let bias = usize::try_from(info.dlpi_addr).ok()?;
   if info.dlpi_phdr.is_null() {
      return None;
   }
   // SAFETY: the loader reports dlpi_phnum headers at dlpi_phdr.
   let headers =
      unsafe { std::slice::from_raw_parts(info.dlpi_phdr, usize::from(info.dlpi_phnum)) };
   let dynamic = headers
      .iter()
      .find(|header| header.p_type == libc::PT_DYNAMIC)?;

   // Bionic leaves the dynamic section's addresses unrelocated, so each one
   // is an offset from the load bias.
   let (mut sysv, mut gnu, mut strings, mut symbols) = (0, 0, 0, 0);
   let mut entry = (bias + usize::try_from(dynamic.p_vaddr).ok()?) as *const [usize; 2];
   loop {
      // SAFETY: PT_DYNAMIC is mapped and ends with a DT_NULL entry, each one a
      // pointer-sized tag and value.
      let [tag, value] = unsafe { entry.read() };
      match tag.cast_signed() {
         DT_NULL => break,
         DT_HASH => sysv = bias + value,
         DT_GNU_HASH => gnu = bias + value,
         DT_STRTAB => strings = bias + value,
         DT_SYMTAB => symbols = bias + value,
         _ => {},
      }
      // SAFETY: the entry just read was not DT_NULL, so another follows.
      entry = unsafe { entry.add(1) };
   }
   if strings == 0 || symbols == 0 {
      return None;
   }

   let defined = |index: u32| {
      // SAFETY: the hash table only holds indices into the symbol table, and
      // each symbol's name is a NUL-terminated offset into the string table.
      let (symbol, symbol_name) = unsafe {
         let symbol = &*(symbols as *const Sym).add(index as usize);
         (
            symbol,
            CStr::from_ptr((strings + symbol.name as usize) as *const c_char),
         )
      };
      (symbol.shndx != 0 && symbol.value != 0 && symbol_name.to_bytes() == name)
         .then(|| bias + symbol.value)
   };

   if gnu != 0 {
      let hash = name.iter().fold(5381u32, |hash, &byte| {
         hash.wrapping_mul(33).wrapping_add(u32::from(byte))
      });
      let table = gnu as *const u32;
      // SAFETY: a GNU hash table is four words (bucket count, first hashed
      // symbol, bloom word count, bloom shift), the pointer-sized bloom
      // words, the buckets, then one chain word per hashed symbol, the last
      // of each chain with bit 0 set.
      unsafe {
         let (bucket_count, first, bloom_words) = (*table, *table.add(1), *table.add(2));
         if bucket_count == 0 {
            return None;
         }
         let buckets = table.add(4 + bloom_words as usize * (size_of::<usize>() / 4));
         let chains = buckets.add(bucket_count as usize);
         let mut index = *buckets.add((hash % bucket_count) as usize);
         if index < first {
            return None;
         }
         loop {
            let chained = *chains.add((index - first) as usize);
            if chained | 1 == hash | 1
               && let Some(found) = defined(index)
            {
               return Some(found);
            }
            if chained & 1 != 0 {
               return None;
            }
            index += 1;
         }
      }
   }

   if sysv != 0 {
      let hash = name.iter().fold(0u32, |hash, &byte| {
         let hash = (hash << 4).wrapping_add(u32::from(byte));
         (hash ^ ((hash & 0xF000_0000) >> 24)) & 0x0FFF_FFFF
      });
      let table = sysv as *const u32;
      // SAFETY: a SysV hash table is the bucket count, the chain count, the
      // buckets, then the chains, with index 0 ending a chain.
      unsafe {
         let (bucket_count, chain_count) = (*table, *table.add(1));
         if bucket_count == 0 {
            return None;
         }
         let chains = table.add(2 + bucket_count as usize);
         let mut index = *table.add(2 + (hash % bucket_count) as usize);
         while index != 0 && index < chain_count {
            if let Some(found) = defined(index) {
               return Some(found);
            }
            index = *chains.add(index as usize);
         }
      }
   }
   None
}

/// The current thread's `JNIEnv`, attaching the thread if needed. Attached
/// threads stay attached.
pub fn env() -> Option<*mut JNIEnv> {
   let vm = vm();
   if vm.is_null() {
      return None;
   }
   let mut env = ptr::null_mut();
   // SAFETY: `vm` is the process's live JavaVM, and both calls only write
   // `env`.
   unsafe {
      if ((**vm).v1_2.GetEnv)(vm, &raw mut env, JNI_VERSION_1_6) == JNI_OK {
         return Some(env.cast());
      }
      (((**vm).v1_2.AttachCurrentThread)(vm, &raw mut env, ptr::null_mut()) == JNI_OK)
         .then_some(env.cast())
   }
}

/// # Safety
///
/// `env` is the current thread's `JNIEnv`.
pub unsafe fn clear_exception(env: *mut JNIEnv) -> bool {
   // SAFETY: forwarded from the caller.
   unsafe {
      let pending = jni!(env, ExceptionCheck);
      if pending {
         jni!(env, ExceptionClear);
      }
      pending
   }
}

/// # Safety
///
/// `env` is the current thread's `JNIEnv`, and `array` is null or a live
/// `byte[]`.
pub unsafe fn read_bytes(env: *mut JNIEnv, array: jbyteArray, limit: Option<jsize>) -> Vec<u8> {
   if array.is_null() {
      return Vec::new();
   }
   // SAFETY: forwarded from the caller. The region stays within the array's
   // length and the buffer's.
   unsafe {
      let mut len = jni!(env, GetArrayLength, array).max(0);
      if let Some(limit) = limit
         && (0..len).contains(&limit)
      {
         len = limit;
      }
      let mut out = vec![0u8; usize::try_from(len).unwrap_or(0)];
      jni!(
         env,
         GetByteArrayRegion,
         array,
         0,
         len,
         out.as_mut_ptr().cast::<jbyte>()
      );
      out
   }
}

/// A new local `byte[]` holding `bytes`, and its length.
///
/// # Safety
///
/// `env` is the current thread's `JNIEnv`.
pub unsafe fn new_bytes(env: *mut JNIEnv, bytes: &[u8]) -> Option<(jbyteArray, jsize)> {
   let Ok(len) = jsize::try_from(bytes.len()) else {
      log::warn!(
         "jvm: a {} byte frame does not fit a Java array",
         bytes.len()
      );
      return None;
   };
   // SAFETY: forwarded from the caller. The region covers exactly `bytes`.
   unsafe {
      let array = jni!(env, NewByteArray, len);
      if array.is_null() {
         clear_exception(env);
         log::warn!("jvm: could not allocate a {len} byte array");
         return None;
      }
      jni!(
         env,
         SetByteArrayRegion,
         array,
         0,
         len,
         bytes.as_ptr().cast::<jbyte>()
      );
      Some((array, len))
   }
}

/// # Safety
///
/// `env` is the current thread's `JNIEnv`, and `string` is null or a live
/// `String`.
pub unsafe fn read_string(env: *mut JNIEnv, string: jstring) -> Option<String> {
   if string.is_null() {
      return None;
   }
   // SAFETY: forwarded from the caller. The chars are NUL-terminated modified
   // UTF-8 and released after the copy.
   unsafe {
      let chars = jni!(env, GetStringUTFChars, string, ptr::null_mut());
      if chars.is_null() {
         clear_exception(env);
         return None;
      }
      let owned = CStr::from_ptr(chars).to_string_lossy().into_owned();
      jni!(env, ReleaseStringUTFChars, string, chars);
      Some(owned)
   }
}

/// Why a JNI call could not run.
#[derive(Debug, flub::Error)]
pub enum JniError {
   #[error("no Java VM is running in this process")]
   NoVm,
   #[error("a query argument or the query holds a NUL byte")]
   Nul,
   #[error(transparent)]
   Missing(Missing),
}

impl From<Missing> for JniError {
   fn from(missing: Missing) -> Self {
      Self::Missing(missing)
   }
}

impl From<JniError> for phoenix_runtime::fetch::FetchError {
   fn from(err: JniError) -> Self {
      Self::Backend(Box::new(err))
   }
}

impl From<Missing> for phoenix_runtime::fetch::FetchError {
   fn from(missing: Missing) -> Self {
      JniError::Missing(missing).into()
   }
}

impl From<JniError> for phoenix_runtime::store::StoreError {
   fn from(err: JniError) -> Self {
      Self::Backend(Box::new(err))
   }
}

impl From<Missing> for phoenix_runtime::store::StoreError {
   fn from(missing: Missing) -> Self {
      JniError::Missing(missing).into()
   }
}

/// A class or method this process does not have, which a calling JNI would
/// abort the app on.
#[derive(Debug, flub::Error)]
#[error("this app has no {0}")]
pub struct Missing(String);

impl From<Missing> for String {
   fn from(missing: Missing) -> Self {
      missing.to_string()
   }
}

/// # Safety
///
/// `env` is the current thread's `JNIEnv`, and `class` a live class.
pub unsafe fn method(
   env: *mut JNIEnv,
   class: jclass,
   name: &CStr,
   signature: &CStr,
) -> Result<jmethodID, Missing> {
   // SAFETY: forwarded from the caller, and both strings are NUL-terminated.
   let method = unsafe { jni!(env, GetMethodID, class, name.as_ptr(), signature.as_ptr()) };
   checked_method(env, method, name, signature)
}

/// # Safety
///
/// As [`method`].
pub unsafe fn static_method(
   env: *mut JNIEnv,
   class: jclass,
   name: &CStr,
   signature: &CStr,
) -> Result<jmethodID, Missing> {
   // SAFETY: forwarded from the caller, and both strings are NUL-terminated.
   let method = unsafe {
      jni!(
         env,
         GetStaticMethodID,
         class,
         name.as_ptr(),
         signature.as_ptr()
      )
   };
   checked_method(env, method, name, signature)
}

fn checked_method(
   env: *mut JNIEnv,
   method: jmethodID,
   name: &CStr,
   signature: &CStr,
) -> Result<jmethodID, Missing> {
   if !method.is_null() {
      return Ok(method);
   }
   // SAFETY: `env` is the caller's, and the failed lookup left a
   // `NoSuchMethodError` pending.
   unsafe { clear_exception(env) };
   Err(Missing(format!(
      "method {}{}",
      name.to_string_lossy(),
      signature.to_string_lossy()
   )))
}

/// # Safety
///
/// `env` is the current thread's `JNIEnv`.
pub unsafe fn take_exception(env: *mut JNIEnv) -> Option<String> {
   // SAFETY: forwarded from the caller. The exception is cleared before
   // `toString` runs, as JNI requires.
   unsafe {
      let thrown = jni!(env, ExceptionOccurred);
      jni!(env, ExceptionClear);
      if thrown.is_null() {
         return None;
      }
      let class = jni!(env, GetObjectClass, thrown);
      let to_string = method(env, class, c"toString", signature!(() -> STRING)).ok()?;
      let text = jni!(env, CallObjectMethodA, thrown, to_string, ptr::null());
      jni!(env, ExceptionClear);
      read_string(env, text)
   }
}

pub(crate) use class;
pub(crate) use jni;
pub(crate) use signature;
