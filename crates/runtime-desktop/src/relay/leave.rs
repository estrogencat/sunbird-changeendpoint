//! Leaving voice while streaming. Phoenix ends the stream and leaves right
//! after, and Spacebar's stream delete saves the voice state it read before the
//! leave, putting the user back in the channel. So the leave waits for the
//! server's `STREAM_DELETE`.

use std::{
   sync::{Arc, Mutex, MutexGuard, PoisonError},
   thread,
   time::Duration,
};

use serde_json::Value;

use super::{upstream::Socket, wire::Message};

const WAIT: Duration = Duration::from_secs(3);

struct Held {
   key: String,
   update: Message,
   generation: u64,
}

#[derive(Default)]
struct State {
   own: Option<String>,
   creating: Option<String>,
   deleting: Option<String>,
   held: Option<Held>,
   generation: u64,
}

pub struct Leave {
   socket: Arc<Socket>,
   state: Mutex<State>,
}

#[derive(serde::Deserialize)]
struct Frame {
   op: u8,
   #[serde(default)]
   d: Value,
}

impl Leave {
   pub fn new(socket: Arc<Socket>) -> Arc<Self> {
      Arc::new(Self {
         socket,
         state: Mutex::new(State::default()),
      })
   }

   fn state(&self) -> MutexGuard<'_, State> {
      self.state.lock().unwrap_or_else(PoisonError::into_inner)
   }

   /// Phoenix to the server: the frame to send now, if any.
   pub fn up(self: &Arc<Self>, message: Message) -> Option<Message> {
      let Message::Text(bytes) = &message else {
         return Some(message);
      };
      let Ok(frame) = serde_json::from_slice::<Frame>(bytes) else {
         return Some(message);
      };
      let mut state = self.state();
      match frame.op {
         18 => {
            state.creating = stream_prefix(&frame.d);
            Some(message)
         },
         19 => {
            let key = frame.d.get("stream_key").and_then(Value::as_str);
            if key.is_some() && key == state.own.as_deref() {
               state.deleting = state.own.take();
            }
            Some(message)
         },
         4 => {
            if let Some(held) = state.held.as_mut() {
               held.update = message;
               return None;
            }
            let Some(key) = state.deleting.clone() else {
               return Some(message);
            };
            let channel = frame.d.get("channel_id").and_then(Value::as_str);
            if channel.is_some() && channel == stream_channel(&key) {
               return Some(message);
            }
            state.generation += 1;
            let generation = state.generation;
            state.held = Some(Held {
               key,
               update: message,
               generation,
            });
            drop(state);
            let leave = self.clone();
            let _ = thread::Builder::new().name("sunbird-relay-leave".to_owned()).spawn(move || {
               thread::sleep(WAIT);
               leave.release(|held| held.generation == generation);
            });
            None
         },
         _ => Some(message),
      }
   }

   /// Server to Phoenix: follows our own stream and lets a held update go
   /// once the stream is gone.
   pub fn down(&self, frame: &[u8]) {
      let head = &frame[..frame.len().min(64)];
      let created = contains(head, b"\"t\":\"STREAM_CREATE\"");
      if !created && !contains(head, b"\"t\":\"STREAM_DELETE\"") {
         return;
      }
      let Ok(payload) = serde_json::from_slice::<Value>(frame) else {
         return;
      };
      let Some(key) = payload.pointer("/d/stream_key").and_then(Value::as_str) else {
         return;
      };
      if created {
         let mut state = self.state();
         if let Some(prefix) = state.creating.take() {
            if key.starts_with(&prefix) {
               state.own = Some(key.to_owned());
            } else {
               state.creating = Some(prefix);
            }
         }
         drop(state);
         return;
      }
      {
         let mut state = self.state();
         if state.own.as_deref() == Some(key) {
            state.own = None;
         }
         if state.deleting.as_deref() == Some(key) {
            state.deleting = None;
         }
      }
      self.release(|held| held.key == key);
   }

   fn release(&self, matches: impl FnOnce(&Held) -> bool) {
      let held = {
         let mut state = self.state();
         if !state.held.as_ref().is_some_and(matches) {
            return;
         }
         state.held.take()
      };
      if let Some(held) = held
         && let Err(err) = self.socket.send(&held.update)
      {
         log::warn!("relay: send upstream: {err}");
      }
   }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
   haystack.windows(needle.len()).any(|window| window == needle)
}

/// The start of the stream key a Go Live request (op 18) will get.
fn stream_prefix(d: &Value) -> Option<String> {
   let text = |name: &str| d.get(name).and_then(Value::as_str);
   let channel = text("channel_id")?;
   match text("type")? {
      "guild" => Some(format!("guild:{}:{channel}:", text("guild_id")?)),
      "call" => Some(format!("call:{channel}:")),
      _ => None,
   }
}

fn stream_channel(key: &str) -> Option<&str> {
   key.rsplit(':').nth(1)
}

#[cfg(test)]
mod tests {
   use serde_json::json;

   use super::*;

   #[test]
   fn reads_stream_keys() {
      let d = json!({ "type": "guild", "guild_id": "1", "channel_id": "2" });
      assert_eq!(stream_prefix(&d).as_deref(), Some("guild:1:2:"));
      assert_eq!(stream_prefix(&json!({ "type": "call", "channel_id": "5" })).as_deref(), Some("call:5:"));
      assert_eq!(stream_channel("guild:1:2:3"), Some("2"));
      assert_eq!(stream_channel("call:5:3"), Some("5"));
   }
}
