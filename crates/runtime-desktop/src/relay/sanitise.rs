//! Trims identify (op 2) and presence (op 3) payloads to what Spacebar's
//! schemas accept. Stricter servers such as Harmony close the gateway on any
//! key Discord added after them, and Phoenix sends plenty.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Identify properties Harmony accepts. Its source also lists the native
/// client ones (`os_arch`, `app_arch`, `os_sdk_version`,
/// `native_build_number`), but the live server closes the gateway over them.
const PROPERTIES: [&str; 24] = [
   "os",
   "browser",
   "device",
   "$os",
   "$browser",
   "$device",
   "os_version",
   "browser_user_agent",
   "browser_version",
   "release_channel",
   "client_build_number",
   "client_event_source",
   "client_version",
   "system_locale",
   "has_client_mods",
   "client_launch_id",
   "launch_signature",
   "client_heartbeat_session_id",
   "is_fast_connect",
   "gateway_connect_reasons",
   "referrer",
   "referring_domain",
   "referrer_current",
   "referring_domain_current",
];

/// Top-level identify keys Spacebar never accepted.
const DROPPED: [&str; 2] = ["qos_token", "client_state"];

enum Shape {
   Leaf,
   Object(&'static [(&'static str, Self)]),
}

use Shape::{Leaf, Object};

const PRESENCE: &[(&str, Shape)] = &[("afk", Leaf), ("status", Leaf), ("since", Leaf)];

const ACTIVITY: &[(&str, Shape)] = &[
   ("name", Leaf),
   ("type", Leaf),
   ("url", Leaf),
   ("created_at", Leaf),
   ("timestamps", Object(&[("start", Leaf), ("end", Leaf)])),
   ("application_id", Leaf),
   ("parent_application_id", Leaf),
   ("details", Leaf),
   ("details_url", Leaf),
   ("state", Leaf),
   ("state_url", Leaf),
   (
      "emoji",
      Object(&[("name", Leaf), ("id", Leaf), ("animated", Leaf)]),
   ),
   ("party", Object(&[("id", Leaf), ("size", Leaf)])),
   (
      "assets",
      Object(&[
         ("large_image", Leaf),
         ("large_text", Leaf),
         ("small_image", Leaf),
         ("small_text", Leaf),
      ]),
   ),
   (
      "secrets",
      Object(&[("join", Leaf), ("spectate", Leaf), ("match", Leaf)]),
   ),
   ("instance", Leaf),
   ("flags", Leaf),
   ("id", Leaf),
   ("sync_id", Leaf),
   (
      "metadata",
      Object(&[
         ("button_urls", Leaf),
         ("context_uri", Leaf),
         ("album_id", Leaf),
         ("artist_ids", Leaf),
      ]),
   ),
   ("session_id", Leaf),
   ("platform", Leaf),
   ("supported_platforms", Leaf),
   ("status_display_type", Leaf),
   ("buttons", Leaf),
];

#[derive(Deserialize, Serialize)]
struct Payload {
   op: u8,
   d: Value,
   #[serde(flatten)]
   rest: Map<String, Value>,
}

fn pick(mut value: Map<String, Value>, shape: &[(&str, Shape)]) -> Map<String, Value> {
   let mut picked = Map::new();
   for (key, rule) in shape {
      let Some(field) = value.remove(*key) else {
         continue;
      };
      let field = match (rule, field) {
         (Object(inner), Value::Object(field)) => Value::Object(pick(field, inner)),
         (_, field) => field,
      };
      picked.insert((*key).to_owned(), field);
   }
   picked
}

/// Spacebar types activity flags as a string, Discord sends a number. Harmony
/// also requires `created_at` and a timestamps `start`, which custom statuses
/// leave out.
fn presence(mut value: Map<String, Value>, shown: bool) -> Option<Map<String, Value>> {
   let Some(Value::Array(mut activities)) = value.remove("activities") else {
      return None;
   };
   if !shown {
      activities.clear();
   }
   let now = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .map_or(0, |elapsed| {
         u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
      });
   let activities = activities
      .into_iter()
      .map(|activity| match activity {
         Value::Object(activity) => {
            let mut activity = pick(activity, ACTIVITY);
            if let Some(Value::Number(flags)) = activity.get("flags") {
               let flags = flags.to_string();
               activity.insert("flags".to_owned(), Value::String(flags));
            }
            activity.entry("created_at").or_insert(Value::from(now));
            if let Some(Value::Object(timestamps)) = activity.get_mut("timestamps") {
               timestamps.entry("start").or_insert(Value::from(now));
            }
            Value::Object(activity)
         },
         other => other,
      })
      .collect();
   let mut picked = pick(value, PRESENCE);
   picked.insert("activities".to_owned(), Value::Array(activities));
   Some(picked)
}

fn identify(mut data: Map<String, Value>, activities: bool) -> Map<String, Value> {
   for key in DROPPED {
      data.remove(key);
   }
   if let Some(Value::Object(properties)) = data.get_mut("properties") {
      properties.retain(|key, _| PROPERTIES.contains(&key.as_str()));
   }
   if let Some(Value::Object(presence)) = data.get("presence").cloned()
      && let Some(trimmed) = self::presence(presence, activities)
   {
      data.insert("presence".to_owned(), Value::Object(trimmed));
   }
   data
}

/// The trimmed payload, or `None` to send the frame as it is. Without
/// `activities` the presence goes out with none.
pub fn sanitise(frame: &[u8], activities: bool) -> Option<Vec<u8>> {
   let mut payload = serde_json::from_slice::<Payload>(frame).ok()?;
   payload.d = match (payload.op, payload.d) {
      (2, Value::Object(data)) => Value::Object(identify(data, activities)),
      (3, Value::Object(data)) => Value::Object(presence(data, activities)?),
      _ => return None,
   };
   serde_json::to_vec(&payload).ok()
}

/// Harmony sends a guild's `version` as a string where Discord sends a
/// number, and Phoenix drops a READY whose guilds don't decode.
fn guild(guild: &mut Value) {
   if let Some(version) = guild.get_mut("version")
      && let Value::String(text) = version
   {
      *version = text.parse::<u64>().map_or(Value::Null, Value::from);
   }
}

/// A server dispatch fixed up for Phoenix (guild versions, and the animated
/// flag on WebP attachments), or `None` to pass it as it is.
pub fn dispatch(frame: &[u8]) -> Option<Vec<u8>> {
   let head = &frame[..frame.len().min(64)];
   let has = |name: &[u8]| head.windows(name.len()).any(|window| window == name);
   let names: [&[u8]; 4] = [
      b"\"t\":\"READY\"",
      b"\"t\":\"GUILD_CREATE\"",
      b"\"t\":\"MESSAGE_CREATE\"",
      b"\"t\":\"MESSAGE_UPDATE\"",
   ];
   if !names.into_iter().any(has) {
      return None;
   }
   let mut payload = serde_json::from_slice::<Payload>(frame).ok()?;
   match payload.rest.get("t").and_then(Value::as_str) {
      Some("READY") => {
         let Some(Value::Array(guilds)) = payload.d.get_mut("guilds") else {
            return None;
         };
         guilds.iter_mut().for_each(guild);
      },
      Some("GUILD_CREATE") => guild(&mut payload.d),
      Some("MESSAGE_CREATE" | "MESSAGE_UPDATE") => {
         if !crate::attachments::flag_message(&mut payload.d) {
            return None;
         }
      },
      _ => return None,
   }
   serde_json::to_vec(&payload).ok()
}

#[cfg(test)]
mod tests {
   use super::{dispatch, sanitise};

   #[test]
   fn trims_identify() {
      let frame = br#"{"op":2,"d":{"token":"t","qos_token":"q","client_state":{},"properties":{"os":"Windows","client_app_state":"focused"},"presence":{"status":"online","since":null,"broadcast":null,"activities":[{"name":"x","type":0,"flags":3,"extra":1,"created_at":5,"timestamps":{"start":1,"end":2}}]}}}"#;
      let out: serde_json::Value = serde_json::from_slice(&sanitise(frame, true).unwrap()).unwrap();
      assert_eq!(
         out,
         serde_json::json!({"op":2,"d":{"token":"t","properties":{"os":"Windows"},"presence":{"status":"online","since":null,"activities":[{"name":"x","type":0,"flags":"3","created_at":5,"timestamps":{"start":1,"end":2}}]}}})
      );
   }

   #[test]
   fn leaves_other_ops() {
      assert!(sanitise(br#"{"op":1,"d":5}"#, true).is_none());
   }

   #[test]
   fn fixes_guild_versions() {
      let frame = br#"{"t":"READY","s":1,"op":0,"d":{"guilds":[{"id":"1","version":"1"}]}}"#;
      let out: serde_json::Value = serde_json::from_slice(&dispatch(frame).unwrap()).unwrap();
      assert_eq!(out["d"]["guilds"][0]["version"], 1);
      assert!(dispatch(br#"{"t":"READY_SUPPLEMENTAL","s":2,"op":0,"d":{}}"#).is_none());
   }

   #[test]
   fn hides_activities() {
      let frame = br#"{"op":3,"d":{"status":"online","activities":[{"name":"x","type":0}]}}"#;
      let out: serde_json::Value =
         serde_json::from_slice(&sanitise(frame, false).unwrap()).unwrap();
      assert_eq!(
         out,
         serde_json::json!({"op":3,"d":{"status":"online","activities":[]}})
      );
   }
}
