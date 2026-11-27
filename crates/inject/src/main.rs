//! `sunbird.exe`, the Windows launcher, also `phoenix-inject [phoenix.dll] [--
//! discord args]`.
//!
//! `Discord.exe` resolves its imports from System32 only (`DependentLoadFlags`
//! 0x800), so a DLL planted beside it is never loaded. This starts it
//! suspended and queues `LoadLibraryW(phoenix.dll)` as an APC on its main
//! thread, which runs once the OS loader finishes and before Discord's own
//! code. Quit Discord first: a second instance hands off to the running one
//! and exits.
//!
//! Without a DLL argument it installs itself to `%LOCALAPPDATA%\Sunbird` with
//! Start Menu and Desktop shortcuts, and fetches the site's newest
//! `phoenix.dll` there before each launch, replacing itself first when the
//! site ships a newer launcher. Discord's own files stay untouched, so its
//! updates never undo Sunbird. `--uninstall` removes both.

#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)] mod http;
#[cfg(windows)] mod shortcuts;

#[cfg(windows)]
fn main() {
   misstep::install_panic_hook();
   if let Err(err) = windows::run() {
      windows::alert(&format!("{err:?}"));
      std::process::exit(1);
   }
}

#[cfg(not(windows))]
fn main() {
   eprintln!("phoenix-inject only runs on Windows");
}

#[cfg(windows)]
mod windows {
   use std::{
      env,
      ffi::{
         OsStr,
         OsString,
         c_void,
      },
      fs,
      io,
      os::windows::ffi::{
         OsStrExt,
         OsStringExt,
      },
      path::{
         Path,
         PathBuf,
      },
      process::Command,
      ptr,
      thread,
      time::Duration,
   };

   use misstep::{
      OptionExt,
      ResultExt,
   };

   type Handle = *mut c_void;

   #[repr(C)]
   struct ProcessInformation {
      process:    Handle,
      thread:     Handle,
      process_id: u32,
      thread_id:  u32,
   }

   const CREATE_SUSPENDED: u32 = 0x4;
   const MEM_COMMIT_RESERVE: u32 = 0x3000;
   const PAGE_READWRITE: u32 = 0x4;

   #[link(name = "kernel32")]
   unsafe extern "system" {
      fn CreateProcessW(
         application: *const u16,
         command_line: *mut u16,
         process_attributes: *const c_void,
         thread_attributes: *const c_void,
         inherit_handles: i32,
         flags: u32,
         environment: *const c_void,
         directory: *const u16,
         startup: *const c_void,
         info: *mut ProcessInformation,
      ) -> i32;
      fn VirtualAllocEx(
         process: Handle,
         address: *const c_void,
         size: usize,
         kind: u32,
         protect: u32,
      ) -> *mut c_void;
      fn WriteProcessMemory(
         process: Handle,
         address: *mut c_void,
         buffer: *const c_void,
         size: usize,
         written: *mut usize,
      ) -> i32;
      fn GetModuleHandleW(name: *const u16) -> Handle;
      fn GetProcAddress(module: Handle, name: *const u8) -> *mut c_void;
      fn QueueUserAPC(function: *mut c_void, thread: Handle, data: usize) -> u32;
      fn ResumeThread(thread: Handle) -> u32;
      fn TerminateProcess(process: Handle, code: u32) -> i32;
      fn CloseHandle(handle: Handle) -> i32;
      fn CreateToolhelp32Snapshot(flags: u32, process_id: u32) -> Handle;
      fn Process32FirstW(snapshot: Handle, entry: *mut c_void) -> i32;
      fn Process32NextW(snapshot: Handle, entry: *mut c_void) -> i32;
      fn Module32FirstW(snapshot: Handle, entry: *mut c_void) -> i32;
      fn Module32NextW(snapshot: Handle, entry: *mut c_void) -> i32;
      fn OpenProcess(access: u32, inherit: i32, process_id: u32) -> Handle;
      fn QueryFullProcessImageNameW(
         process: Handle,
         flags: u32,
         name: *mut u16,
         size: *mut u32,
      ) -> i32;
      fn WaitForSingleObject(handle: Handle, milliseconds: u32) -> u32;
   }

   #[link(name = "advapi32")]
   unsafe extern "system" {
      fn RegDeleteKeyValueW(key: isize, sub_key: *const u16, value: *const u16) -> i32;
   }

   #[link(name = "user32")]
   unsafe extern "system" {
      fn MessageBoxW(window: Handle, text: *const u16, caption: *const u16, kind: u32) -> i32;
   }

   /// Where the site serves `version.json` and the loaders. `SUNBIRD_SITE`
   /// points it at a local server for testing; anything else must be https,
   /// since what it serves is loaded into Discord.
   fn site() -> misstep::Result<String> {
      let site =
         env::var("SUNBIRD_SITE").unwrap_or_else(|_| "https://assets.sunbird.tools".to_owned());
      if !site_allowed(&site) {
         misstep::bail!("SUNBIRD_SITE must be https:// or a localhost server, not {site}");
      }
      Ok(site)
   }

   fn site_allowed(site: &str) -> bool {
      site.starts_with("https://")
         || ["http://127.0.0.1", "http://localhost"]
            .iter()
            .any(|local| {
               site
                  .strip_prefix(local)
                  .is_some_and(|rest| rest.is_empty() || rest.starts_with([':', '/']))
            })
   }

   /// Shows `text` in a message box, since the launcher has no console.
   pub fn alert(text: &str) {
      // SAFETY: both strings are NUL-terminated and outlive the call.
      unsafe {
         MessageBoxW(
            ptr::null_mut(),
            wide(text).as_ptr(),
            wide("Sunbird").as_ptr(),
            0x10,
         )
      };
   }

   /// Asks a yes or no question, true for yes.
   fn ask(text: &str) -> bool {
      // `MB_YESNO | MB_ICONQUESTION`, and `IDYES`.
      // SAFETY: both strings are NUL-terminated and outlive the call.
      unsafe {
         MessageBoxW(
            ptr::null_mut(),
            wide(text).as_ptr(),
            wide("Sunbird").as_ptr(),
            0x24,
         ) == 6
      }
   }

   const SNAPSHOT_PROCESSES: u32 = 0x2;
   const SNAPSHOT_MODULES: u32 = 0x8 | 0x10;
   /// `TERMINATE | QUERY_LIMITED_INFORMATION | SYNCHRONIZE`.
   const PROCESS_ACCESS: u32 = 0x0010_1001;

   /// The UTF-16 text in `entry` from byte `at`, up to its NUL.
   fn entry_text(entry: &[u8], at: usize) -> OsString {
      let units: Vec<u16> = entry[at..]
         .as_chunks::<2>()
         .0
         .iter()
         .map(|pair| u16::from_le_bytes(*pair))
         .take_while(|&unit| unit != 0)
         .collect();
      OsString::from_wide(&units)
   }

   /// Every running process of `exe`, opened to end it: Discord's main
   /// process and its helpers.
   fn running(exe: &Path) -> Vec<(Handle, u32)> {
      let name = exe.file_name().unwrap_or_default();
      let want = exe.as_os_str().to_string_lossy().to_lowercase();
      // SAFETY: a process snapshot of the whole system.
      let snapshot = unsafe { CreateToolhelp32Snapshot(SNAPSHOT_PROCESSES, 0) };
      if snapshot as isize == -1 {
         return Vec::new();
      }
      // `PROCESSENTRY32W`: its size first, the pid at 8, the file name at 44.
      let mut entry = [0u64; 71];
      entry[0] = 568;
      let mut found = Vec::new();
      // SAFETY: `entry` is a 568-byte `PROCESSENTRY32W` with its size set.
      let mut more = unsafe { Process32FirstW(snapshot, entry.as_mut_ptr().cast()) } != 0;
      while more {
         let bytes: &[u8] = as_bytes(&entry);
         let pid = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
         if entry_text(bytes, 44).eq_ignore_ascii_case(name) {
            // SAFETY: opens by pid, checked for null below.
            let process = unsafe { OpenProcess(PROCESS_ACCESS, 0, pid) };
            if !process.is_null() {
               let mut path = [0u16; 1024];
               let mut len = u32::try_from(path.len()).unwrap_or(1024);
               // SAFETY: `path` holds `len` units.
               let named = unsafe {
                  QueryFullProcessImageNameW(process, 0, path.as_mut_ptr(), &raw mut len)
               } != 0;
               let path = String::from_utf16_lossy(&path[..len as usize]).to_lowercase();
               if named && path == want {
                  found.push((process, pid));
               } else {
                  // SAFETY: opened above, closed once.
                  unsafe { CloseHandle(process) };
               }
            }
         }
         // SAFETY: as above.
         more = unsafe { Process32NextW(snapshot, entry.as_mut_ptr().cast()) } != 0;
      }
      // SAFETY: the snapshot is closed once.
      unsafe { CloseHandle(snapshot) };
      found
   }

   /// Whether process `pid` has a module named `dll` loaded.
   fn has_module(pid: u32, dll: &OsStr) -> bool {
      // SAFETY: a module snapshot of one process.
      let snapshot = unsafe { CreateToolhelp32Snapshot(SNAPSHOT_MODULES, pid) };
      if snapshot as isize == -1 {
         return false;
      }
      // `MODULEENTRY32W`: its size first, the module name at 48.
      let mut entry = [0u64; 135];
      entry[0] = 1080;
      let mut found = false;
      // SAFETY: `entry` is a 1080-byte `MODULEENTRY32W` with its size set.
      let mut more = unsafe { Module32FirstW(snapshot, entry.as_mut_ptr().cast()) } != 0;
      while more && !found {
         found = entry_text(as_bytes(&entry), 48).eq_ignore_ascii_case(dll);
         // SAFETY: as above.
         more = unsafe { Module32NextW(snapshot, entry.as_mut_ptr().cast()) } != 0;
      }
      // SAFETY: the snapshot is closed once.
      unsafe { CloseHandle(snapshot) };
      found
   }

   const fn as_bytes(words: &[u64]) -> &[u8] {
      // SAFETY: any `u64`s are valid bytes, and the length covers them.
      unsafe { std::slice::from_raw_parts(words.as_ptr().cast(), size_of_val(words)) }
   }

   fn wide(text: impl AsRef<OsStr>) -> Vec<u16> {
      text.as_ref().encode_wide().chain([0]).collect()
   }

   fn append_quoted(command: &mut Vec<u16>, arg: &OsStr) {
      const QUOTE: u16 = b'"' as u16;
      const SLASH: u16 = b'\\' as u16;
      command.push(QUOTE);
      let mut slashes = 0;
      for unit in arg.encode_wide() {
         if unit == SLASH {
            slashes += 1;
            continue;
         }
         // Windows treats backslashes before a quote as escapes.
         let count = if unit == QUOTE {
            slashes * 2 + 1
         } else {
            slashes
         };
         command.extend(std::iter::repeat_n(SLASH, count));
         command.push(unit);
         slashes = 0;
      }
      command.extend(std::iter::repeat_n(SLASH, slashes * 2));
      command.push(QUOTE);
   }

   fn check(ok: bool) -> io::Result<()> {
      if ok {
         Ok(())
      } else {
         Err(io::Error::last_os_error())
      }
   }

   /// `--wait-pid <pid>`, which an in-app restart passes so this starts
   /// Discord only once the old one is gone.
   fn wait_pid(
      args: &mut std::iter::Peekable<impl Iterator<Item = OsString>>,
   ) -> misstep::Result<Option<u32>> {
      if args.next_if(|arg| arg == "--wait-pid").is_none() {
         return Ok(None);
      }
      let pid = args
         .next()
         .and_then(|pid| pid.to_str()?.parse::<u32>().ok())
         .context("--wait-pid needs a process id")?;
      Ok(Some(pid))
   }

   /// Waits up to `timeout` for process `pid` to exit. One already gone, or
   /// one this can't open, doesn't wait.
   fn wait_for_exit(pid: u32, timeout: Duration) {
      const SYNCHRONIZE: u32 = 0x0010_0000;
      // SAFETY: opens by pid, checked for null below.
      let process = unsafe { OpenProcess(SYNCHRONIZE, 0, pid) };
      if process.is_null() {
         return;
      }
      let millis = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
      // SAFETY: opened with SYNCHRONIZE above, and closed once.
      unsafe {
         WaitForSingleObject(process, millis);
         CloseHandle(process);
      }
   }

   /// Set for the launcher a self-update starts, so it doesn't update again.
   const SELF_UPDATED: &str = "SUNBIRD_SELF_UPDATED";

   pub fn run() -> misstep::Result<()> {
      let local = PathBuf::from(env::var_os("LOCALAPPDATA").context("LOCALAPPDATA is unset")?);
      let home = local.join("Sunbird");
      let self_updated = env::var_os(SELF_UPDATED).is_some();
      // SAFETY: the launcher is single-threaded. Removed so Discord and the
      // launcher an in-app restart starts don't inherit it.
      unsafe { env::remove_var(SELF_UPDATED) };
      let mut args = env::args_os().skip(1).peekable();
      if let Some(pid) = wait_pid(&mut args)? {
         wait_for_exit(pid, Duration::from_secs(15));
      }
      if args.peek().is_some_and(|arg| arg == "--uninstall") {
         uninstall(&local, &home);
         return Ok(());
      }
      let mut no_runtime = false;
      while args
         .next_if(|arg| arg == "--no-runtime" || arg == "--gate-only")
         .is_some()
      {
         no_runtime = true;
      }
      let app = local.join("DiscordPhoenix").join("app");
      let exe = app.join("Discord.exe");
      if !exe.is_file() {
         misstep::bail!(
            "Discord Phoenix is not installed at {}. Install it first, then open Sunbird again.",
            exe.display()
         );
      }
      let explicit = args.next_if(|arg| {
         Path::new(arg)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("dll"))
      });
      while args
         .next_if(|arg| arg == "--no-runtime" || arg == "--gate-only")
         .is_some()
      {
         no_runtime = true;
      }
      let launcher = if explicit.is_some() {
         env::current_exe()?
      } else {
         home.join("sunbird.exe")
      };
      let dll = if let Some(path) = &explicit {
         PathBuf::from(path)
      } else if let Some(dll) = install(&local, &home, self_updated)? {
         dll
      } else {
         return Ok(());
      };
      let dll = dll.canonicalize()?;
      args.next_if(|arg| arg == "--");
      let args: Vec<OsString> = args.collect();

      // A second Discord hands off to the running one and exits, so Sunbird
      // would never run in the Discord the user sees.
      let open = running(&exe);
      if !open.is_empty() {
         let dll_name = dll.file_name().unwrap_or_default();
         let with_sunbird = open.iter().any(|&(_, pid)| has_module(pid, dll_name));
         let restart = if no_runtime {
            ask(
               "Discord is already running.\n\nRestart it without the Sunbird plugin runtime? A \
                call you are in will end.",
            )
         } else {
            !with_sunbird
               && ask(
                  "Discord is already running without Sunbird.\n\nRestart it with Sunbird? A call \
                   you are in will end.",
               )
         };
         for &(process, _) in &open {
            if restart {
               // SAFETY: opened with TERMINATE by `running`.
               unsafe { TerminateProcess(process, 0) };
            }
         }
         for (process, _) in open {
            if restart {
               // SAFETY: opened with SYNCHRONIZE by `running`.
               unsafe { WaitForSingleObject(process, 5000) };
            }
            // SAFETY: opened by `running`, closed once.
            unsafe { CloseHandle(process) };
         }
         if !restart {
            // Brings the running Discord forward, or leaves it as the user chose.
            if with_sunbird {
               Command::new(&exe).args(&args).current_dir(&app).spawn()?;
            }
            return Ok(());
         }
      }

      set_restart_env(&launcher, explicit.as_ref().map(|_| dll.as_path()), no_runtime);
      launch(&exe, &app, &dll, &args)
   }

   fn set_restart_env(launcher: &Path, explicit_dll: Option<&Path>, no_runtime: bool) {
      let mut restart_args = Vec::new();
      if no_runtime {
         restart_args.extend("--no-runtime ".encode_utf16());
      }
      if let Some(dll) = explicit_dll {
         append_quoted(&mut restart_args, dll.as_os_str());
         restart_args.push(u16::from(b' '));
      }
      restart_args.extend("--".encode_utf16());
      // SAFETY: the launcher is single-threaded. Discord and its restart helper
      // inherit these settings with the rest of the environment.
      unsafe {
         env::set_var("SUNBIRD_RESTART_EXE", launcher);
         env::set_var("SUNBIRD_RESTART_ARGS", OsString::from_wide(&restart_args));
         if no_runtime {
            env::set_var("SUNBIRD_GATE_ONLY", "1");
         }
      }
   }

   fn launch(
      exe: &Path,
      app: &Path,
      dll: &Path,
      args: &[OsString],
   ) -> misstep::Result<()> {
      let mut command = Vec::new();
      append_quoted(&mut command, exe.as_os_str());
      for arg in args {
         command.push(u16::from(b' '));
         append_quoted(&mut command, arg);
      }
      command.push(0);

      // STARTUPINFOW is 104 bytes on x64 and only `cb` must be set.
      let mut startup = [0u64; 13];
      startup[0] = 104;
      let mut info = ProcessInformation {
         process:    ptr::null_mut(),
         thread:     ptr::null_mut(),
         process_id: 0,
         thread_id:  0,
      };
      // SAFETY: every pointer is valid for the call and the strings are
      // NUL-terminated.
      check(unsafe {
         CreateProcessW(
            wide(exe).as_ptr(),
            command.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            0,
            CREATE_SUSPENDED,
            ptr::null(),
            wide(app).as_ptr(),
            startup.as_ptr().cast(),
            &raw mut info,
         ) != 0
      })?;

      let queued = queue_load(&info, &wide(dll));
      if queued.is_err() {
         // SAFETY: `info.process` is the suspended child created above.
         unsafe { TerminateProcess(info.process, 1) };
      } else {
         // SAFETY: `info.thread` is the child's suspended main thread.
         unsafe { ResumeThread(info.thread) };
      }
      // SAFETY: both handles came from CreateProcessW and are closed once.
      unsafe {
         CloseHandle(info.thread);
         CloseHandle(info.process);
      }
      queued?;
      println!(
         "started Discord (pid {}) with {}",
         info.process_id,
         dll.display()
      );
      Ok(())
   }

   /// Copies `dll` into the child and queues `LoadLibraryW` on its main
   /// thread. kernel32 sits at the same address in every process of a boot.
   fn queue_load(info: &ProcessInformation, dll: &[u16]) -> io::Result<()> {
      let size = size_of_val(dll);
      // SAFETY: `info.process` is a live process handle with full access.
      let remote = unsafe {
         VirtualAllocEx(
            info.process,
            ptr::null(),
            size,
            MEM_COMMIT_RESERVE,
            PAGE_READWRITE,
         )
      };
      check(!remote.is_null())?;
      // SAFETY: `remote` holds `size` bytes and `dll` is `size` bytes long.
      check(unsafe {
         WriteProcessMemory(
            info.process,
            remote,
            dll.as_ptr().cast(),
            size,
            ptr::null_mut(),
         ) != 0
      })?;
      // SAFETY: kernel32 is always loaded and the names are NUL-terminated.
      let load = unsafe {
         GetProcAddress(
            GetModuleHandleW(wide("kernel32.dll").as_ptr()),
            c"LoadLibraryW".as_ptr().cast(),
         )
      };
      check(!load.is_null())?;
      // SAFETY: LoadLibraryW takes one pointer argument, as an APC does.
      check(unsafe { QueueUserAPC(load, info.thread, remote as usize) } != 0)
   }

   /// Copies this launcher into `home`, adds its shortcuts once, and brings
   /// the launcher and `phoenix.dll` up to the site's versions. A failed
   /// update keeps what is already there, so Sunbird still starts offline.
   /// `None` when a newer launcher took over the start.
   fn install(local: &Path, home: &Path, self_updated: bool) -> misstep::Result<Option<PathBuf>> {
      fs::create_dir_all(home)?;
      let launcher = home.join("sunbird.exe");
      // What the last self-update moved aside, once that launcher exited.
      let _ = fs::remove_file(home.join("sunbird.exe.old"));
      let me = env::current_exe()?;
      let copied = !same_file(&me, &launcher);
      if copied {
         fs::copy(&me, &launcher).context("copying the launcher")?;
         // Missing shortcuts never stop Discord from starting, and a shortcut
         // the user deleted stays deleted until they run the installer again.
         let _ = crate::shortcuts::sync(local, &launcher, false);
      }
      let dll = home.join("phoenix.dll");
      let auto = auto_update(local);
      // A missing DLL is always downloaded, whatever the setting says.
      if dll.is_file() && !auto {
         return Ok(Some(dll));
      }
      let updated = site().and_then(|site| {
         let manifest =
            crate::http::text(&format!("{site}/version.json"), Duration::from_secs(10))?;
         // A failed self-update never stops Discord from starting with this
         // launcher.
         if auto
            && !self_updated
            && let Some(expected) = launcher_crc(&manifest)
            && self_update(&site, &launcher, expected).unwrap_or(false)
         {
            return Ok(false);
         }
         update(&site, &manifest, home, &dll).map(|()| true)
      });
      match updated {
         Ok(false) => Ok(None),
         Err(err) if !dll.is_file() => Err(err.context("downloading phoenix.dll")),
         _ => Ok(Some(dll)),
      }
   }

   /// Replaces `launcher` with the site's when its CRC-32 is not `expected`,
   /// and starts the new one with this one's arguments. True once it runs.
   fn self_update(site: &str, launcher: &Path, expected: u32) -> misstep::Result<bool> {
      if crc32fast::hash(&fs::read(launcher)?) == expected {
         return Ok(false);
      }
      let fresh = launcher.with_file_name(format!("sunbird.exe.{}.new", std::process::id()));
      let url = format!("{site}/plugins/sunbird-windows-x86_64.exe");
      let installed = crate::http::download(&url, &fresh, Duration::from_secs(60)).and_then(|()| {
         if !is_launcher(&fs::read(&fresh)?, expected) {
            misstep::bail!("the downloaded sunbird.exe is not the one version.json lists");
         }
         Ok(replace(launcher, &fresh)?)
      });
      if installed.is_err() {
         let _ = fs::remove_file(&fresh);
      }
      installed?;
      Command::new(launcher)
         .args(env::args_os().skip(1))
         .env(SELF_UPDATED, "1")
         .spawn()?;
      Ok(true)
   }

   /// Whether `bytes` are a PE image with CRC-32 `expected`.
   fn is_launcher(bytes: &[u8], expected: u32) -> bool {
      bytes.starts_with(b"MZ") && crc32fast::hash(bytes) == expected
   }

   /// `launcher_crcs["sunbird-windows-x86_64.exe"]` from `version.json`.
   fn launcher_crc(manifest: &str) -> Option<u32> {
      const KEY: &str = "\"sunbird-windows-x86_64.exe\"";
      let rest = manifest[manifest.find(KEY)? + KEY.len()..]
         .trim_start()
         .strip_prefix(':')?
         .trim_start();
      let end = rest
         .find(|c: char| !c.is_ascii_digit())
         .unwrap_or(rest.len());
      rest[..end].parse().ok()
   }

   /// Puts `fresh` in place of `dest`. Windows lets a running launcher or a
   /// mapped DLL be renamed but not overwritten, so `dest` moves aside to
   /// `.old`, and back when `fresh` can't take its place.
   fn replace(dest: &Path, fresh: &Path) -> io::Result<()> {
      let mut old = dest.as_os_str().to_owned();
      old.push(".old");
      let old = PathBuf::from(old);
      let _ = fs::remove_file(&old);
      let moved = dest.is_file();
      if moved {
         fs::rename(dest, &old)?;
      }
      if let Err(err) = fs::rename(fresh, dest) {
         if moved {
            let _ = fs::rename(&old, dest);
         }
         return Err(err);
      }
      // Fails while the old copy still runs; the next start removes it.
      let _ = fs::remove_file(&old);
      Ok(())
   }

   /// Whether `launcher.conf`, which Sunbird's settings write into its home
   /// (`crates/runtime-desktop/src/config.rs`), leaves updates on.
   fn auto_update(local: &Path) -> bool {
      let conf = local
         .join("DiscordPhoenix")
         .join("sunbird")
         .join("launcher.conf");
      fs::read_to_string(conf).map_or(true, |text| auto_update_in(&text))
   }

   fn auto_update_in(conf: &str) -> bool {
      !conf.lines().any(|line| {
         line
            .split_once('=')
            .is_some_and(|(key, value)| key.trim() == "auto_update" && value.trim() == "false")
      })
   }

   fn same_file(a: &Path, b: &Path) -> bool {
      a.canonicalize()
         .ok()
         .zip(b.canonicalize().ok())
         .is_some_and(|(a, b)| a == b)
   }

   /// Downloads `phoenix.dll` when the site's loader version differs from the
   /// one recorded beside it. A running Discord keeps the old DLL mapped,
   /// which Windows lets us rename but not overwrite.
   fn update(site: &str, manifest: &str, home: &Path, dll: &Path) -> misstep::Result<()> {
      let version = loader_version(manifest).context("version.json has no loader")?;
      let stamp = home.join("version.txt");
      if dll.is_file() && fs::read_to_string(&stamp).is_ok_and(|have| have.trim() == version) {
         return Ok(());
      }
      // Per process, so a second launch mid-download writes its own file.
      let fresh = home.join(format!("phoenix.dll.{}.new", std::process::id()));
      // Discord waits on this download. With a DLL to fall back on, a slow one
      // gives up sooner; a first install has nothing else, so it keeps trying.
      let attempts = if dll.is_file() { 1 } else { 3 };
      let url = format!("{site}/plugins/phoenix-windows-x86_64.dll");
      let mut downloaded = crate::http::download(&url, &fresh, Duration::from_secs(60));
      for _ in 1..attempts {
         if downloaded.is_ok() {
            break;
         }
         thread::sleep(Duration::from_secs(2));
         downloaded = crate::http::download(&url, &fresh, Duration::from_secs(60));
      }
      if let Err(err) = downloaded {
         let _ = fs::remove_file(&fresh);
         return Err(err);
      }
      // A captive portal or a wrong deploy still writes its body here;
      // swapping that in would leave Discord without a working DLL.
      if !is_loader(&fresh) {
         let _ = fs::remove_file(&fresh);
         misstep::bail!("the downloaded phoenix.dll is not a Windows loader");
      }
      if let Err(install) = replace(dll, &fresh) {
         // A failed swap must not leave Sunbird without a DLL: the one moved
         // aside went back into place, and this launch carries on with it.
         let _ = fs::remove_file(&fresh);
         if dll.is_file() {
            return Ok(());
         }
         misstep::bail!("installing phoenix.dll failed: {install}");
      }
      fs::write(stamp, version)?;
      Ok(())
   }

   /// Whether `path` is a Windows PE image carrying the version marker every
   /// loader is built with (`crates/runtime/src/update.rs`).
   fn is_loader(path: &Path) -> bool {
      fs::read(path).is_ok_and(|bytes| {
         bytes.starts_with(b"MZ")
            && bytes
               .windows(16)
               .any(|window| window == b"phoenix-version:")
      })
   }

   /// `"loader": "<version>"` from `version.json`.
   fn loader_version(manifest: &str) -> Option<&str> {
      let rest = &manifest[manifest.find("\"loader\"")? + 8..];
      let rest = &rest[rest.find('"')? + 1..];
      rest.get(..rest.find('"')?)
   }

   /// Removes the shortcuts and `home`. Files a running Discord or launcher
   /// still holds stay until they exit, and the user is told so.
   fn uninstall(local: &Path, home: &Path) {
      /// `HKEY_CURRENT_USER`, a sign-extended predefined handle.
      const HKEY_CURRENT_USER: isize = 0x8000_0001_u32.cast_signed() as isize;
      let _ = crate::shortcuts::sync(local, &home.join("sunbird.exe"), true);
      // SAFETY: both strings are NUL-terminated and outlive the call. A
      // value that isn't there is not an error worth reporting.
      unsafe {
         RegDeleteKeyValueW(
            HKEY_CURRENT_USER,
            wide(r"Software\Microsoft\Windows\CurrentVersion\Run").as_ptr(),
            wide("Sunbird").as_ptr(),
         );
      }
      match fs::remove_dir_all(home) {
         Err(err) if err.kind() != io::ErrorKind::NotFound => {
            alert(&format!(
               "The shortcuts are removed, but {} could not be deleted ({err}). Quit Discord and \
                delete it.",
               home.display()
            ));
         },
         _ => alert("Sunbird is uninstalled. Discord itself was not changed."),
      }
   }

   #[cfg(test)]
   mod tests {
      #[test]
      fn quotes_windows_arguments_for_restart() {
         for (arg, quoted) in [
            ("", r#""""#),
            ("two words", r#""two words""#),
            (r"C:\a b\", r#""C:\a b\\""#),
            (r#"a\"b"#, r#""a\\\"b""#),
         ] {
            let mut command = Vec::new();
            super::append_quoted(&mut command, arg.as_ref());
            assert_eq!(String::from_utf16(&command).unwrap(), quoted);
         }
      }

      #[test]
      fn reads_the_loader_version() {
         let manifest = "{\n  \"loader\": \"2026.11.23.2353-b51d77c\",\n  \"plugins\": {}\n}";
         assert_eq!(
            super::loader_version(manifest),
            Some("2026.11.23.2353-b51d77c")
         );
         assert_eq!(super::loader_version("{}"), None);
      }

      #[test]
      fn reads_the_launcher_crc() {
         let manifest = "{\n  \"loader\": \"1\",\n  \"launcher_crcs\": {\n    \
                         \"sunbird-linux.sh\": 7,\n    \"sunbird-windows-x86_64.exe\": \
                         3735928559\n  },\n  \"plugins\": {}\n}";
         assert_eq!(super::launcher_crc(manifest), Some(0xDEAD_BEEF));
         assert_eq!(super::launcher_crc("{\"loader\": \"1\"}"), None);
         assert_eq!(
            super::launcher_crc("{\"sunbird-windows-x86_64.exe\": \"x\"}"),
            None
         );
      }

      #[test]
      fn checks_a_downloaded_launcher() {
         // The standard CRC-32 check value.
         assert_eq!(crc32fast::hash(b"123456789"), 0xCBF4_3926);
         let exe = b"MZ\x90\0rest";
         let crc = crc32fast::hash(exe);
         assert!(super::is_launcher(exe, crc));
         assert!(!super::is_launcher(exe, crc ^ 1));
         assert!(!super::is_launcher(b"<html>", crc32fast::hash(b"<html>")));
      }

      #[test]
      fn replaces_a_file_through_old() {
         let dir = std::env::temp_dir().join(format!("sunbird-replace-{}", std::process::id()));
         let _ = std::fs::remove_dir_all(&dir);
         std::fs::create_dir_all(&dir).unwrap();
         let dest = dir.join("sunbird.exe");
         let fresh = dir.join("sunbird.exe.1.new");
         std::fs::write(&dest, "old").unwrap();
         std::fs::write(&fresh, "new").unwrap();
         super::replace(&dest, &fresh).unwrap();
         assert_eq!(std::fs::read_to_string(&dest).unwrap(), "new");
         assert!(!fresh.exists() && !dir.join("sunbird.exe.old").exists());
         // Without a fresh file it fails, and the current one goes back.
         assert!(super::replace(&dest, &fresh).is_err());
         assert_eq!(std::fs::read_to_string(&dest).unwrap(), "new");
         std::fs::remove_dir_all(&dir).unwrap();
      }

      #[test]
      fn reads_auto_update_from_launcher_conf() {
         assert!(super::auto_update_in(""));
         assert!(super::auto_update_in("onboarded=true\nauto_update=true\n"));
         assert!(!super::auto_update_in(
            "onboarded=true\nauto_start=true\nauto_update=false\n"
         ));
         assert!(!super::auto_update_in("auto_update = false\r\n"));
      }

      #[test]
      fn takes_a_wait_pid() {
         let args = |list: &[&str]| {
            list
               .iter()
               .map(std::ffi::OsString::from)
               .collect::<Vec<_>>()
               .into_iter()
               .peekable()
         };
         let mut given = args(&["--wait-pid", "4242", "--uninstall"]);
         assert_eq!(super::wait_pid(&mut given).unwrap(), Some(4242));
         assert_eq!(given.next().as_deref(), Some("--uninstall".as_ref()));
         assert_eq!(super::wait_pid(&mut args(&["--", "x"])).unwrap(), None);
         assert!(super::wait_pid(&mut args(&["--wait-pid", "nope"])).is_err());
         assert!(super::wait_pid(&mut args(&["--wait-pid"])).is_err());
      }

      #[test]
      fn waits_for_a_process_to_exit() {
         let started = std::time::Instant::now();
         super::wait_for_exit(u32::MAX, std::time::Duration::from_secs(15));
         assert!(started.elapsed() < std::time::Duration::from_secs(1));
         let mut child = std::process::Command::new("cmd")
            .args(["/c", "ping -n 2 127.0.0.1 >nul"])
            .spawn()
            .unwrap();
         super::wait_for_exit(child.id(), std::time::Duration::from_secs(15));
         assert!(child.try_wait().unwrap().is_some());
      }

      #[test]
      fn allows_https_or_localhost_sites() {
         for ok in [
            "https://assets.sunbird.tools",
            "http://127.0.0.1:8765",
            "http://localhost",
            "http://localhost/x",
         ] {
            assert!(super::site_allowed(ok), "{ok}");
         }
         for bad in [
            "http://assets.sunbird.tools",
            "http://localhost.evil.com",
            "http://127.0.0.1.evil.com",
            "ftp://x",
         ] {
            assert!(!super::site_allowed(bad), "{bad}");
         }
      }
   }
}
