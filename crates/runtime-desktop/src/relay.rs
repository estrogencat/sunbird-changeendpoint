//! The gateway relay. Phoenix has one gateway URL for every account, so it
//! points at this local socket, which reads the token from the first
//! identify or resume and connects to that account's server. Frames pass
//! through untouched after that, compressed or not.

mod upstream;
mod wire;

use std::{
   io::{self, BufRead, BufReader, Write},
   net::{Shutdown, TcpListener, TcpStream},
   sync::{Arc, Mutex, OnceLock, PoisonError},
   thread,
};

use upstream::Socket;
use wire::Message;

use crate::servers;

static GATEWAY: OnceLock<Option<String>> = OnceLock::new();

/// Headers passed on to the server. Origin stays behind: Discord's gateway
/// refuses any.
const FORWARDED: [&str; 2] = ["user-agent", "accept-language"];
const FALLBACK_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) DiscordPhoenix";

/// The relay's `ws://` URL, started on first use.
pub fn gateway() -> Option<String> {
   GATEWAY
      .get_or_init(|| {
         let listener = TcpListener::bind(("127.0.0.1", 0))
            .map_err(|err| log::error!("relay: cannot listen: {err}"))
            .ok()?;
         let port = listener.local_addr().ok()?.port();
         thread::Builder::new()
            .name("sunbird-relay".to_owned())
            .spawn(move || {
               for stream in listener.incoming().flatten() {
                  let spawned = thread::Builder::new()
                     .name("sunbird-relay-conn".to_owned())
                     .spawn(move || {
                        if let Err(err) = relay(stream) {
                           log::warn!("relay: {err}");
                        }
                     });
                  if let Err(err) = spawned {
                     log::warn!("relay: {err}");
                  }
               }
            })
            .map_err(|err| log::error!("relay: {err}"))
            .ok()?;
         log::info!("relay: gateway relay on 127.0.0.1:{port}");
         Some(format!("ws://127.0.0.1:{port}/g"))
      })
      .clone()
}

struct Upgrade {
   query: String,
   headers: Vec<(String, String)>,
}

fn handshake(stream: &mut TcpStream, reader: &mut BufReader<TcpStream>) -> io::Result<Upgrade> {
   let mut line = String::new();
   reader.read_line(&mut line)?;
   let target = line.split(' ').nth(1).unwrap_or("/");
   let query = target.find('?').map_or("", |at| &target[at..]).to_owned();
   let mut key = None;
   let mut headers = Vec::new();
   loop {
      line.clear();
      if reader.read_line(&mut line)? == 0 {
         return Err(io::ErrorKind::UnexpectedEof.into());
      }
      let header = line.trim_end();
      if header.is_empty() {
         break;
      }
      let Some((name, value)) = header.split_once(':') else {
         continue;
      };
      let name = name.trim().to_ascii_lowercase();
      let value = value.trim().to_owned();
      if name == "sec-websocket-key" {
         key = Some(value);
      } else if FORWARDED.contains(&name.as_str()) {
         headers.push((name, value));
      }
   }
   let key =
      key.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "not a websocket upgrade"))?;
   if !headers.iter().any(|(name, _)| name == "user-agent") {
      headers.push(("user-agent".to_owned(), FALLBACK_AGENT.to_owned()));
   }
   write!(
      stream,
      "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n",
      wire::accept_key(&key)
   )?;
   Ok(Upgrade { query, headers })
}

/// The token of an identify (op 2) or resume (op 6). Never logged.
fn session_token(message: &Message) -> Option<String> {
   #[derive(serde::Deserialize)]
   struct Payload {
      op: u8,
      d: Option<Data>,
   }
   #[derive(serde::Deserialize)]
   struct Data {
      token: Option<String>,
   }
   let (Message::Text(bytes) | Message::Binary(bytes)) = message else {
      return None;
   };
   let payload = serde_json::from_slice::<Payload>(bytes).ok()?;
   matches!(payload.op, 2 | 6).then(|| payload.d.and_then(|data| data.token).unwrap_or_default())
}

type Writer = Arc<Mutex<TcpStream>>;

fn relay(mut stream: TcpStream) -> io::Result<()> {
   stream.set_nodelay(true)?;
   let mut reader = BufReader::new(stream.try_clone()?);
   let upgrade = handshake(&mut stream, &mut reader)?;
   let writer: Writer = Arc::new(Mutex::new(stream));
   let mut pending = Vec::new();
   let token = loop {
      let message = wire::read_message(&mut reader, &writer)?;
      if matches!(message, Message::Close(..)) {
         return Ok(());
      }
      let token = session_token(&message);
      pending.push(message);
      if let Some(token) = token {
         break token;
      }
   };
   let Some(target) = servers::gateway_for(&token) else {
      log::warn!("relay: the token belongs to no known server, refusing");
      wire::close(&writer, 4004, b"unknown account");
      return Ok(());
   };
   let url = format!("{}/{}", target.trim_end_matches('/'), upgrade.query);
   log::info!("relay: relaying to {}", host_of(&target));
   let socket = match Socket::connect(&url, &upgrade.headers) {
      Ok(socket) => Arc::new(socket),
      Err(err) => {
         wire::close(&writer, 4000, b"relay failed");
         return Err(err);
      },
   };
   for message in &pending {
      socket.send(message)?;
   }
   let down = {
      let socket = socket.clone();
      let writer = writer.clone();
      thread::Builder::new()
         .name("sunbird-relay-down".to_owned())
         .spawn(move || pump_down(&socket, &writer))?
   };
   loop {
      match wire::read_message(&mut reader, &writer) {
         Ok(Message::Close(code, reason)) => {
            socket.shutdown(code, &reason);
            break;
         },
         Ok(message) => {
            if let Err(err) = socket.send(&message) {
               log::warn!("relay: send upstream: {err}");
               break;
            }
         },
         Err(_) => {
            socket.shutdown(4000, b"");
            break;
         },
      }
   }
   let _ = down.join();
   Ok(())
}

/// Server to Phoenix, until the server closes. A close outside the 4000s
/// becomes 4000 so Phoenix resumes instead of dropping the session.
fn pump_down(socket: &Socket, writer: &Writer) {
   loop {
      match socket.receive() {
         Ok(Message::Close(code, reason)) => {
            log::info!("relay: server closed {code}");
            let code = if (4000..5000).contains(&code) {
               code
            } else {
               4000
            };
            wire::close(writer, code, &reason);
            break;
         },
         Ok(message) => {
            if wire::write_message(writer, &message).is_err() {
               socket.shutdown(4000, b"");
               break;
            }
         },
         Err(err) => {
            log::info!("relay: server connection ended: {err}");
            wire::close(writer, 4000, b"relay lost the server");
            break;
         },
      }
   }
   let _ = writer
      .lock()
      .unwrap_or_else(PoisonError::into_inner)
      .shutdown(Shutdown::Both);
}

fn host_of(url: &str) -> &str {
   let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
   rest.split(['/', '?']).next().unwrap_or(rest)
}
