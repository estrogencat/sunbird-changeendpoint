//! The server side of the relay through `WinHTTP`'s websockets, so TLS,
//! certificates and the proxy are the system's.

use std::{ffi::c_void, io, ptr};

use super::wire::Message;

const WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY: u32 = 4;
const WINHTTP_FLAG_SECURE: u32 = 0x0080_0000;
const WINHTTP_ADDREQ_FLAG_ADD: u32 = 0x2000_0000;
const WINHTTP_ADDREQ_FLAG_REPLACE: u32 = 0x8000_0000;
const WINHTTP_OPTION_UPGRADE_TO_WEB_SOCKET: u32 = 114;
const WINHTTP_QUERY_STATUS_CODE: u32 = 19;
const WINHTTP_QUERY_FLAG_NUMBER: u32 = 0x2000_0000;
const CONNECT_TIMEOUT_MS: i32 = 15_000;

const BINARY_MESSAGE: i32 = 0;
const BINARY_FRAGMENT: i32 = 1;
const UTF8_MESSAGE: i32 = 2;
const UTF8_FRAGMENT: i32 = 3;
const CLOSE: i32 = 4;

#[link(name = "winhttp")]
unsafe extern "system" {
   fn WinHttpOpen(
      agent: *const u16,
      access_type: u32,
      proxy: *const u16,
      proxy_bypass: *const u16,
      flags: u32,
   ) -> *mut c_void;
   fn WinHttpCloseHandle(handle: *mut c_void) -> i32;
   fn WinHttpSetTimeouts(
      handle: *mut c_void,
      resolve: i32,
      connect: i32,
      send: i32,
      receive: i32,
   ) -> i32;
   fn WinHttpConnect(
      session: *mut c_void,
      server_name: *const u16,
      port: u16,
      reserved: u32,
   ) -> *mut c_void;
   fn WinHttpOpenRequest(
      connect: *mut c_void,
      verb: *const u16,
      object_name: *const u16,
      version: *const u16,
      referrer: *const u16,
      accept_types: *const *const u16,
      flags: u32,
   ) -> *mut c_void;
   fn WinHttpSetOption(
      handle: *mut c_void,
      option: u32,
      buffer: *const c_void,
      buffer_len: u32,
   ) -> i32;
   fn WinHttpAddRequestHeaders(
      request: *mut c_void,
      headers: *const u16,
      headers_len: u32,
      modifiers: u32,
   ) -> i32;
   fn WinHttpSendRequest(
      request: *mut c_void,
      headers: *const u16,
      headers_len: u32,
      optional: *const c_void,
      optional_len: u32,
      total_len: u32,
      context: usize,
   ) -> i32;
   fn WinHttpReceiveResponse(request: *mut c_void, reserved: *mut c_void) -> i32;
   fn WinHttpQueryHeaders(
      request: *mut c_void,
      info_level: u32,
      name: *const u16,
      buffer: *mut c_void,
      buffer_len: *mut u32,
      index: *mut u32,
   ) -> i32;
   fn WinHttpWebSocketCompleteUpgrade(request: *mut c_void, context: usize) -> *mut c_void;
   fn WinHttpWebSocketSend(
      socket: *mut c_void,
      buffer_type: i32,
      buffer: *const c_void,
      buffer_len: u32,
   ) -> u32;
   fn WinHttpWebSocketReceive(
      socket: *mut c_void,
      buffer: *mut c_void,
      buffer_len: u32,
      read: *mut u32,
      buffer_type: *mut i32,
   ) -> u32;
   fn WinHttpWebSocketShutdown(
      socket: *mut c_void,
      status: u16,
      reason: *const c_void,
      reason_len: u32,
   ) -> u32;
   fn WinHttpWebSocketQueryCloseStatus(
      socket: *mut c_void,
      status: *mut u16,
      reason: *mut c_void,
      reason_len: u32,
      consumed: *mut u32,
   ) -> u32;
}

struct Handle(*mut c_void);

impl Handle {
   fn check(handle: *mut c_void, context: &str) -> io::Result<Self> {
      if handle.is_null() {
         Err(os_error(context))
      } else {
         Ok(Self(handle))
      }
   }
}

impl Drop for Handle {
   fn drop(&mut self) {
      // SAFETY: `self.0` is a live WinHTTP handle owned by this guard.
      unsafe { WinHttpCloseHandle(self.0) };
   }
}

fn os_error(context: &str) -> io::Error {
   let err = io::Error::last_os_error();
   io::Error::new(err.kind(), format!("{context}: {err}"))
}

fn code_error(code: u32, context: &str) -> io::Error {
   let err = io::Error::from_raw_os_error(i32::try_from(code).unwrap_or(i32::MAX));
   io::Error::new(err.kind(), format!("{context}: {err}"))
}

fn wide(text: &str) -> Vec<u16> {
   text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn len_u32(len: usize) -> io::Result<u32> {
   u32::try_from(len).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "too large"))
}

/// One websocket to a gateway. `WinHTTP` allows one send and one receive at
/// a time from different threads, which is how the relay uses it.
pub struct Socket {
   handle: Handle,
   _connection: Handle,
   _session: Handle,
}

#[expect(
   clippy::non_send_fields_in_send_ty,
   reason = "WinHTTP handles are thread-safe"
)]
// SAFETY: WinHTTP handles may be used from any thread, and the relay keeps to
// one sender and one receiver.
unsafe impl Send for Socket {}
// SAFETY: as above.
unsafe impl Sync for Socket {}

struct Parts<'a> {
   secure: bool,
   host: &'a str,
   port: u16,
   path: &'a str,
}

fn parse(url: &str) -> io::Result<Parts<'_>> {
   let bad = || io::Error::new(io::ErrorKind::InvalidInput, "not a ws:// or wss:// URL");
   let (secure, rest) = if let Some(rest) = url.strip_prefix("wss://") {
      (true, rest)
   } else {
      (false, url.strip_prefix("ws://").ok_or_else(bad)?)
   };
   let split = rest.find(['/', '?']).unwrap_or(rest.len());
   let (authority, path) = rest.split_at(split);
   let (host, port) = match authority.rsplit_once(':') {
      Some((host, port)) => (host, port.parse().map_err(|_| bad())?),
      None => (authority, if secure { 443 } else { 80 }),
   };
   if host.is_empty() {
      return Err(bad());
   }
   Ok(Parts {
      secure,
      host,
      port,
      path: if path.is_empty() { "/" } else { path },
   })
}

/// Opens the upgrade request and reads its answer, which must be a 101.
fn upgrade_request(
   connection: &Handle,
   parts: &Parts<'_>,
   headers: &[(String, String)],
) -> io::Result<Handle> {
   let verb = wide("GET");
   let path = wide(parts.path);
   // SAFETY: `verb` and `path` are NUL-terminated UTF-16.
   let request = Handle::check(
      unsafe {
         WinHttpOpenRequest(
            connection.0,
            verb.as_ptr(),
            path.as_ptr(),
            ptr::null(),
            ptr::null(),
            ptr::null(),
            if parts.secure { WINHTTP_FLAG_SECURE } else { 0 },
         )
      },
      "WinHttpOpenRequest",
   )?;
   // SAFETY: `request.0` is live, the option takes no buffer.
   if unsafe {
      WinHttpSetOption(
         request.0,
         WINHTTP_OPTION_UPGRADE_TO_WEB_SOCKET,
         ptr::null(),
         0,
      )
   } == 0
   {
      return Err(os_error("WinHttpSetOption"));
   }
   for (name, value) in headers {
      if [name, value]
         .iter()
         .any(|part| part.contains(['\r', '\n', '\0']))
      {
         continue;
      }
      let line = wide(&format!("{name}: {value}\r\n"));
      // SAFETY: `line` holds `len` UTF-16 units before its NUL.
      if unsafe {
         WinHttpAddRequestHeaders(
            request.0,
            line.as_ptr(),
            len_u32(line.len() - 1)?,
            WINHTTP_ADDREQ_FLAG_ADD | WINHTTP_ADDREQ_FLAG_REPLACE,
         )
      } == 0
      {
         return Err(os_error("WinHttpAddRequestHeaders"));
      }
   }
   // SAFETY: `request.0` is live and the request has no body.
   if unsafe { WinHttpSendRequest(request.0, ptr::null(), 0, ptr::null(), 0, 0, 0) } == 0 {
      return Err(os_error("WinHttpSendRequest"));
   }
   // SAFETY: the request was sent.
   if unsafe { WinHttpReceiveResponse(request.0, ptr::null_mut()) } == 0 {
      return Err(os_error("WinHttpReceiveResponse"));
   }
   let mut status = 0u32;
   let mut status_len = len_u32(size_of::<u32>())?;
   // SAFETY: `status` is `status_len` writable bytes.
   if unsafe {
      WinHttpQueryHeaders(
         request.0,
         WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
         ptr::null(),
         (&raw mut status).cast(),
         &raw mut status_len,
         ptr::null_mut(),
      )
   } == 0
   {
      return Err(os_error("WinHttpQueryHeaders"));
   }
   if status != 101 {
      return Err(io::Error::other(format!(
         "the server answered {status}, not an upgrade"
      )));
   }
   Ok(request)
}

impl Socket {
   pub fn connect(url: &str, headers: &[(String, String)]) -> io::Result<Self> {
      let parts = parse(url)?;
      let agent = wide("Sunbird");
      // SAFETY: `agent` is NUL-terminated UTF-16.
      let session = Handle::check(
         unsafe {
            WinHttpOpen(
               agent.as_ptr(),
               WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
               ptr::null(),
               ptr::null(),
               0,
            )
         },
         "WinHttpOpen",
      )?;
      // SAFETY: `session.0` is live. The receive timeout stays infinite, a
      // gateway can be quiet for longer than any fixed one.
      unsafe {
         WinHttpSetTimeouts(
            session.0,
            CONNECT_TIMEOUT_MS,
            CONNECT_TIMEOUT_MS,
            CONNECT_TIMEOUT_MS,
            0,
         )
      };
      let host = wide(parts.host);
      // SAFETY: `host` is NUL-terminated UTF-16.
      let connection = Handle::check(
         unsafe { WinHttpConnect(session.0, host.as_ptr(), parts.port, 0) },
         "WinHttpConnect",
      )?;
      let request = upgrade_request(&connection, &parts, headers)?;
      // SAFETY: the response is a 101 to an upgrade request.
      let socket = Handle::check(
         unsafe { WinHttpWebSocketCompleteUpgrade(request.0, 0) },
         "WinHttpWebSocketCompleteUpgrade",
      )?;
      Ok(Self {
         handle: socket,
         _connection: connection,
         _session: session,
      })
   }

   pub fn send(&self, message: &Message) -> io::Result<()> {
      let (kind, data) = match message {
         Message::Text(data) => (UTF8_MESSAGE, data),
         Message::Binary(data) => (BINARY_MESSAGE, data),
         Message::Close(code, reason) => {
            self.shutdown(*code, reason);
            return Ok(());
         },
      };
      // SAFETY: `data` is valid for its length.
      let code = unsafe {
         WinHttpWebSocketSend(
            self.handle.0,
            kind,
            data.as_ptr().cast(),
            len_u32(data.len())?,
         )
      };
      if code == 0 {
         Ok(())
      } else {
         Err(code_error(code, "WinHttpWebSocketSend"))
      }
   }

   /// The next whole message from the server.
   pub fn receive(&self) -> io::Result<Message> {
      let mut data = Vec::new();
      let mut chunk = vec![0u8; 64 << 10];
      loop {
         let mut read = 0u32;
         let mut kind = 0i32;
         // SAFETY: `chunk` is writable for its length.
         let code = unsafe {
            WinHttpWebSocketReceive(
               self.handle.0,
               chunk.as_mut_ptr().cast(),
               len_u32(chunk.len())?,
               &raw mut read,
               &raw mut kind,
            )
         };
         if code != 0 {
            return Err(code_error(code, "WinHttpWebSocketReceive"));
         }
         data.extend_from_slice(&chunk[..read as usize]);
         match kind {
            BINARY_FRAGMENT | UTF8_FRAGMENT => {},
            BINARY_MESSAGE => return Ok(Message::Binary(data)),
            UTF8_MESSAGE => return Ok(Message::Text(data)),
            CLOSE => return Ok(self.close_status()),
            _ => return Err(io::Error::other(format!("unknown buffer type {kind}"))),
         }
      }
   }

   fn close_status(&self) -> Message {
      let mut status = 1005u16;
      let mut reason = [0u8; 123];
      let mut consumed = 0u32;
      // SAFETY: `reason` is writable for its length.
      let code = unsafe {
         WinHttpWebSocketQueryCloseStatus(
            self.handle.0,
            &raw mut status,
            reason.as_mut_ptr().cast(),
            123,
            &raw mut consumed,
         )
      };
      if code != 0 {
         return Message::Close(1005, Vec::new());
      }
      Message::Close(status, reason[..(consumed as usize).min(123)].to_vec())
   }

   /// Sends a close without waiting for the server's, which the receiving
   /// thread reads.
   pub fn shutdown(&self, code: u16, reason: &[u8]) {
      let code = if matches!(code, 1000 | 1001 | 3000..5000) {
         code
      } else {
         4000
      };
      let reason = &reason[..reason.len().min(123)];
      // SAFETY: `reason` is valid for its length.
      unsafe {
         WinHttpWebSocketShutdown(
            self.handle.0,
            code,
            reason.as_ptr().cast(),
            u32::try_from(reason.len()).unwrap_or(0),
         );
      }
   }
}
