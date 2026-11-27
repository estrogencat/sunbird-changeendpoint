//! Trims identify (op 2) and presence (op 3) payloads to what Spacebar's
//! schemas accept. Stricter servers such as Harmony close the gateway on any
//! key Discord added after them, and Phoenix sends plenty.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::servers::Tweaks;

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

/// Spacebar only tells friends and servers someone came online when they
/// connect as offline, then switches them to online itself. Invisible stays
/// as it is.
fn announce(data: &mut Map<String, Value>) {
   if let Some(Value::Object(presence)) = data.get_mut("presence")
      && presence.get("status").and_then(Value::as_str) != Some("invisible")
   {
      presence.insert("status".to_owned(), Value::from("offline"));
   }
}

fn identify(mut data: Map<String, Value>, activities: bool, tweaks: Tweaks) -> Map<String, Value> {
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
   if tweaks.announce {
      announce(&mut data);
   }
   data
}

/// The trimmed payload, or `None` to send the frame as it is. Without
/// `activities` the presence goes out with none.
pub fn sanitise(frame: &[u8], activities: bool, tweaks: Tweaks) -> Option<Vec<u8>> {
   let mut payload = serde_json::from_slice::<Payload>(frame).ok()?;
   payload.d = match (payload.op, payload.d) {
      (2, Value::Object(data)) => Value::Object(identify(data, activities, tweaks)),
      (3, Value::Object(data)) => Value::Object(presence(data, activities)?),
      _ => return None,
   };
   serde_json::to_vec(&payload).ok()
}

/// Harmony sends a guild's `version` as a string where Discord sends a
/// number, and Phoenix drops a READY whose guilds don't decode. Spacebar
/// also sends `null` for some flags Phoenix reads as true or false.
fn guild(guild: &mut Value) {
   if let Some(version) = guild.get_mut("version")
      && let Value::String(text) = version
   {
      *version = text.parse::<u64>().map_or(Value::Null, Value::from);
   }
   let flags = ["premium_progress_bar_enabled"];
   if guild.get("properties").is_some_and(Value::is_object) {
      force_bools(&mut guild["properties"], &flags);
   } else {
      force_bools(guild, &flags);
   }
   if let Some(Value::Array(states)) = guild.get_mut("voice_states") {
      for state in states {
         force_bools(state, &VOICE_FLAGS);
      }
   }
}

const VOICE_FLAGS: [&str; 7] =
   ["deaf", "mute", "self_deaf", "self_mute", "self_video", "self_stream", "suppress"];

/// Sets each of `keys` that is present but not true or false to false.
fn force_bools(fields: &mut Value, keys: &[&str]) -> bool {
   let Value::Object(fields) = fields else {
      return false;
   };
   let mut changed = false;
   for &key in keys {
      if fields.get(key).is_some_and(|value| !value.is_boolean()) {
         fields.insert(key.to_owned(), Value::Bool(false));
         changed = true;
      }
   }
   changed
}

/// Spacebar sends `null` for a relationship's spam and stranger flags, and
/// Phoenix drops every friend from READY when one doesn't decode.
fn relationship(relationship: &mut Value) -> bool {
   force_bools(relationship, &["is_spam_request", "stranger_request"])
}

/// Spacebar's own avatar decorations have short SKU ids where Discord's are
/// snowflakes, and Phoenix drops the whole DM whose recipient wears one. The
/// id is rebuilt from the asset hash, with bit 60 set for a snowflake's length,
/// the same way the Spacebar plugin does so it can still find the image.
fn decorations(value: &mut Value) {
   match value {
      Value::Array(items) => items.iter_mut().for_each(decorations),
      Value::Object(fields) => {
         if let Some(Value::Object(decoration)) = fields.get_mut("avatar_decoration_data")
            && let Some(sku) = decoration_sku(decoration)
         {
            decoration.insert("sku_id".to_owned(), Value::String(sku));
         }
         fields.values_mut().for_each(decorations);
      },
      _ => {},
   }
}

fn decoration_sku(decoration: &Map<String, Value>) -> Option<String> {
   let sku = decoration.get("sku_id").and_then(Value::as_str).unwrap_or_default();
   if sku.len() >= 17 && sku.bytes().all(|b| b.is_ascii_digit()) {
      return None;
   }
   let asset = decoration.get("asset")?.as_str()?;
   let hex = asset.strip_prefix("a_").unwrap_or(asset).get(..15)?;
   u64::from_str_radix(hex, 16).ok().map(|id| (id | 1 << 60).to_string())
}

fn each(data: &mut Value, key: &str, fix: fn(&mut Value) -> bool) {
   if let Some(Value::Array(items)) = data.get_mut(key) {
      for item in items {
         fix(item);
      }
   }
}

/// A server dispatch fixed up for Phoenix (guild versions, friends and DMs,
/// attachment flags, and the Servers page's tweaks), or `None` to pass it as
/// it is.
pub fn dispatch(frame: &[u8], tweaks: Tweaks) -> Option<Vec<u8>> {
   let head = &frame[..frame.len().min(64)];
   let has = |name: &[u8]| head.windows(name.len()).any(|window| window == name);
   let names: [&[u8]; 7] = [
      b"\"t\":\"READY\"",
      b"\"t\":\"GUILD_CREATE\"",
      b"\"t\":\"GUILD_UPDATE\"",
      b"\"t\":\"USER_UPDATE\"",
      b"\"t\":\"MESSAGE_CREATE\"",
      b"\"t\":\"MESSAGE_UPDATE\"",
      b"\"t\":\"RELATIONSHIP_ADD\"",
   ];
   if !names.into_iter().any(has) {
      return None;
   }
   let mut payload = serde_json::from_slice::<Payload>(frame).ok()?;
   match payload.rest.get("t").and_then(Value::as_str) {
      Some("READY") => {
         each(&mut payload.d, "guilds", |value| {
            guild(value);
            true
         });
         if tweaks.boosts {
            each(&mut payload.d, "guilds", |value| {
               crate::tweaks::boost(value);
               true
            });
         }
         if tweaks.verified
            && let Some(user) = payload.d.get_mut("user")
         {
            crate::tweaks::verify(user);
         }
         each(&mut payload.d, "relationships", relationship);
         decorations(&mut payload.d);
      },
      Some("GUILD_CREATE") => {
         guild(&mut payload.d);
         if tweaks.boosts {
            crate::tweaks::boost(&mut payload.d);
         }
      },
      Some("GUILD_UPDATE") if tweaks.boosts => crate::tweaks::boost(&mut payload.d),
      Some("USER_UPDATE") if tweaks.verified => crate::tweaks::verify(&mut payload.d),
      Some("RELATIONSHIP_ADD") => {
         if !relationship(&mut payload.d) {
            return None;
         }
      },
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
   use crate::servers::Tweaks;

   const OFF: Tweaks = Tweaks {
      boosts: false,
      verified: false,
      announce: false,
      upload_limit: false,
   };

   #[test]
   fn trims_identify() {
      let frame = br#"{"op":2,"d":{"token":"t","qos_token":"q","client_state":{},"properties":{"os":"Windows","client_app_state":"focused"},"presence":{"status":"online","since":null,"broadcast":null,"activities":[{"name":"x","type":0,"flags":3,"extra":1,"created_at":5,"timestamps":{"start":1,"end":2}}]}}}"#;
      let out: serde_json::Value = serde_json::from_slice(&sanitise(frame, true, OFF).unwrap()).unwrap();
      assert_eq!(
         out,
         serde_json::json!({"op":2,"d":{"token":"t","properties":{"os":"Windows"},"presence":{"status":"online","since":null,"activities":[{"name":"x","type":0,"flags":"3","created_at":5,"timestamps":{"start":1,"end":2}}]}}})
      );
   }

   #[test]
   fn leaves_other_ops() {
      assert!(sanitise(br#"{"op":1,"d":5}"#, true, OFF).is_none());
   }

   #[test]
   fn fixes_guild_versions() {
      let frame = br#"{"t":"READY","s":1,"op":0,"d":{"guilds":[{"id":"1","version":"1"}]}}"#;
      let out: serde_json::Value = serde_json::from_slice(&dispatch(frame, OFF).unwrap()).unwrap();
      assert_eq!(out["d"]["guilds"][0]["version"], 1);
      assert!(dispatch(br#"{"t":"READY_SUPPLEMENTAL","s":2,"op":0,"d":{}}"#, OFF).is_none());
      let frame = br#"{"t":"READY","s":1,"op":0,"d":{"guilds":[{"id":"1","properties":{"premium_progress_bar_enabled":null},"voice_states":[{"self_stream":null,"self_mute":true}]}]}}"#;
      let out: serde_json::Value = serde_json::from_slice(&dispatch(frame, OFF).unwrap()).unwrap();
      let guild = &out["d"]["guilds"][0];
      assert_eq!(guild["properties"]["premium_progress_bar_enabled"], false);
      assert_eq!(guild["voice_states"][0]["self_stream"], false);
      assert_eq!(guild["voice_states"][0]["self_mute"], true);
   }

   #[test]
   fn fixes_friends_and_dms() {
      let frame = br#"{"t":"READY","s":1,"op":0,"d":{"guilds":[],"relationships":[{"id":"2","type":1,"is_spam_request":null,"stranger_request":null,"user_ignored":false}],"private_channels":[{"id":"3","type":1,"recipients":[{"id":"2","avatar_decoration_data":{"asset":"a_0123456789abcdef0123456789abcdef","sku_id":"1234"}},{"id":"4","avatar_decoration_data":{"asset":"x","sku_id":"1144058844004233369"}}]}]}}"#;
      let out: serde_json::Value = serde_json::from_slice(&dispatch(frame, OFF).unwrap()).unwrap();
      assert_eq!(out["d"]["relationships"][0]["is_spam_request"], false);
      assert_eq!(out["d"]["relationships"][0]["stranger_request"], false);
      assert_eq!(out["d"]["relationships"][0]["user_ignored"], false);
      let recipients = &out["d"]["private_channels"][0]["recipients"];
      let derived = (0x0123_4567_89ab_cde_u64 | 1 << 60).to_string();
      assert_eq!(recipients[0]["avatar_decoration_data"]["sku_id"], derived);
      assert_eq!(recipients[1]["avatar_decoration_data"]["sku_id"], "1144058844004233369");
      let friend = br#"{"t":"RELATIONSHIP_ADD","s":2,"op":0,"d":{"id":"2","type":1,"is_spam_request":false}}"#;
      assert!(dispatch(friend, OFF).is_none());
   }

   #[test]
   fn applies_tweaks() {
      let on = Tweaks::default();
      let frame = br#"{"op":2,"d":{"token":"t","presence":{"status":"idle","activities":[]}}}"#;
      let out: serde_json::Value = serde_json::from_slice(&sanitise(frame, true, on).unwrap()).unwrap();
      assert_eq!(out["d"]["presence"]["status"], "offline");
      let frame = br#"{"op":2,"d":{"token":"t","presence":{"status":"invisible","activities":[]}}}"#;
      let out: serde_json::Value = serde_json::from_slice(&sanitise(frame, true, on).unwrap()).unwrap();
      assert_eq!(out["d"]["presence"]["status"], "invisible");
      let frame = br#"{"t":"READY","s":1,"op":0,"d":{"user":{"id":"1","verified":false},"guilds":[{"id":"9","properties":{"premium_tier":0,"features":[]}}]}}"#;
      let out: serde_json::Value = serde_json::from_slice(&dispatch(frame, on).unwrap()).unwrap();
      assert_eq!(out["d"]["user"]["verified"], true);
      assert_eq!(out["d"]["guilds"][0]["properties"]["premium_tier"], 3);
      let frame = br#"{"t":"USER_UPDATE","s":3,"op":0,"d":{"id":"1","verified":false}}"#;
      let out: serde_json::Value = serde_json::from_slice(&dispatch(frame, on).unwrap()).unwrap();
      assert_eq!(out["d"]["verified"], true);
      assert!(dispatch(frame, OFF).is_none());
   }

   #[test]
   fn hides_activities() {
      let frame = br#"{"op":3,"d":{"status":"online","activities":[{"name":"x","type":0}]}}"#;
      let out: serde_json::Value =
         serde_json::from_slice(&sanitise(frame, false, OFF).unwrap()).unwrap();
      assert_eq!(
         out,
         serde_json::json!({"op":3,"d":{"status":"online","activities":[]}})
      );
   }
}
