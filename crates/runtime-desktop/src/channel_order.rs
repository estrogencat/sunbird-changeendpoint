//! Channel moves on Spacebar servers. Phoenix sends only the moved channels,
//! with positions counted the way Discord sorts them, but Spacebar reads each
//! position as a slot in one flat list of every channel, so moves landed
//! categories and channels in the wrong spots. Moves go out as the whole
//! list instead, in the order Discord would show it.

use std::{
   cmp::Ordering,
   collections::HashMap,
   sync::{Mutex, PoisonError},
};

use serde_json::{Map, Value};

const CATEGORY: u64 = 4;
const VOICE: [u64; 2] = [2, 13];
const THREADS: [u64; 3] = [10, 11, 12];

#[derive(Clone)]
struct Channel {
   kind:     u64,
   position: i64,
   parent:   Option<String>,
}

/// Each server's channels by id, as the gateway last described them.
static GUILDS: Mutex<Option<HashMap<String, HashMap<String, Channel>>>> = Mutex::new(None);

fn with_guilds<T>(f: impl FnOnce(&mut HashMap<String, HashMap<String, Channel>>) -> T) -> T {
   f(GUILDS
      .lock()
      .unwrap_or_else(PoisonError::into_inner)
      .get_or_insert_default())
}

fn parse(channel: &Value) -> Option<(String, Channel)> {
   let kind = channel.get("type")?.as_u64()?;
   if THREADS.contains(&kind) {
      return None;
   }
   Some((channel.get("id")?.as_str()?.to_owned(), Channel {
      kind,
      position: channel.get("position").and_then(Value::as_i64).unwrap_or_default(),
      parent: channel
         .get("parent_id")
         .and_then(Value::as_str)
         .map(str::to_owned),
   }))
}

/// Remembers the channels of a server from `READY` or `GUILD_CREATE`.
pub fn remember_guild(guild: &Value) {
   let Some(id) = guild
      .get("id")
      .or_else(|| guild.get("properties").and_then(|p| p.get("id")))
      .and_then(Value::as_str)
   else {
      return;
   };
   let Some(Value::Array(channels)) = guild.get("channels") else {
      return;
   };
   channels.iter().for_each(crate::channel_icons::note);
   let channels = channels.iter().filter_map(parse).collect();
   with_guilds(|guilds| guilds.insert(id.to_owned(), channels));
}

/// Follows a gateway event that changes a server's channels.
pub fn follow(event: &str, data: &Value) {
   match event {
      "GUILD_DELETE" => {
         if let Some(id) = data.get("id").and_then(Value::as_str) {
            with_guilds(|guilds| guilds.remove(id));
         }
      },
      "CHANNEL_CREATE" | "CHANNEL_UPDATE" | "CHANNEL_DELETE" => {
         if event != "CHANNEL_DELETE" {
            crate::channel_icons::note(data);
         }
         let Some(guild) = data.get("guild_id").and_then(Value::as_str) else {
            return;
         };
         let Some((id, channel)) = parse(data) else {
            return;
         };
         with_guilds(|guilds| {
            let channels = guilds.entry(guild.to_owned()).or_default();
            if event == "CHANNEL_DELETE" {
               channels.remove(&id);
            } else {
               channels.insert(id, channel);
            }
         });
      },
      _ => {},
   }
}

fn snowflake(a: &str, b: &str) -> Ordering {
   a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

fn sorted<'a>(mut list: Vec<(&'a String, &'a Channel)>) -> Vec<(&'a String, &'a Channel)> {
   list.sort_by(|(a_id, a), (b_id, b)| {
      VOICE
         .contains(&a.kind)
         .cmp(&VOICE.contains(&b.kind))
         .then(a.position.cmp(&b.position))
         .then_with(|| snowflake(a_id, b_id))
   });
   list
}

/// The body of a channel move for server `guild` as the whole list, or `None`
/// to send it as it is.
pub fn reorder(guild: &str, body: &[u8]) -> Option<Vec<u8>> {
   let Value::Array(updates) = serde_json::from_slice::<Value>(body).ok()? else {
      return None;
   };
   let mut channels = with_guilds(|guilds| guilds.get(guild).cloned())?;
   let mut moves = HashMap::new();
   for update in &updates {
      let Value::Object(update) = update else {
         return None;
      };
      let id = update.get("id")?.as_str()?;
      let channel = channels.get_mut(id)?;
      if let Some(position) = update.get("position").and_then(Value::as_i64) {
         channel.position = position;
      }
      let mut kept = Map::new();
      if let Some(parent) = update.get("parent_id") {
         let parent = parent.as_str().map(str::to_owned);
         let locked = update
            .get("lock_permissions")
            .and_then(Value::as_bool)
            .unwrap_or_default();
         if locked || parent != channel.parent {
            kept.insert("parent_id".to_owned(), parent.clone().map_or(Value::Null, Value::String));
            kept.insert("lock_permissions".to_owned(), Value::Bool(locked));
         }
         channel.parent = parent;
      }
      moves.insert(id.to_owned(), kept);
   }

   let is_category = |id: &str| channels.get(id).is_some_and(|c| c.kind == CATEGORY);
   let children = |parent: Option<&str>| {
      sorted(
         channels
            .iter()
            .filter(|(_, c)| {
               c.kind != CATEGORY
                  && parent.map_or_else(
                     || !c.parent.as_deref().is_some_and(is_category),
                     |parent| c.parent.as_deref() == Some(parent),
                  )
            })
            .collect(),
      )
   };
   let categories = sorted(channels.iter().filter(|(_, c)| c.kind == CATEGORY).collect());
   let order = children(None).into_iter().chain(
      categories
         .into_iter()
         .flat_map(|category| std::iter::once(category).chain(children(Some(category.0)))),
   );

   let list: Vec<Value> = order
      .enumerate()
      .map(|(position, (id, _))| {
         let mut entry = moves.remove(id).unwrap_or_default();
         entry.insert("id".to_owned(), Value::String(id.clone()));
         entry.insert("position".to_owned(), Value::from(position));
         Value::Object(entry)
      })
      .collect();
   serde_json::to_vec(&list).ok()
}

#[cfg(test)]
mod tests {
   use serde_json::{Value, json};

   use super::{follow, remember_guild, reorder};

   fn ids(body: &[u8]) -> Vec<String> {
      let list: Vec<Value> = serde_json::from_slice(body).unwrap();
      list
         .iter()
         .enumerate()
         .map(|(i, entry)| {
            assert_eq!(entry["position"], i);
            entry["id"].as_str().unwrap().to_owned()
         })
         .collect()
   }

   #[test]
   fn sends_the_whole_order() {
      remember_guild(&json!({"id":"500","channels":[
         {"id":"1","type":0,"position":0},
         {"id":"2","type":4,"position":1},
         {"id":"3","type":0,"position":2,"parent_id":"2"},
         {"id":"4","type":2,"position":3,"parent_id":"2"},
         {"id":"5","type":4,"position":4},
         {"id":"6","type":0,"position":5,"parent_id":"5"},
         {"id":"7","type":11,"position":0,"parent_id":"3"},
      ]}));
      let moved = reorder("500", br#"[{"id":"5","position":0},{"id":"2","position":1}]"#).unwrap();
      assert_eq!(ids(&moved), ["1", "5", "6", "2", "3", "4"]);

      let body = br#"[{"id":"6","position":0,"parent_id":"2","lock_permissions":false}]"#;
      let moved = reorder("500", body).unwrap();
      assert_eq!(ids(&moved), ["1", "2", "6", "3", "4", "5"]);
      let list: Vec<Value> = serde_json::from_slice(&moved).unwrap();
      assert_eq!(list[2]["parent_id"], "2");
      assert!(list[3].get("parent_id").is_none());

      follow("CHANNEL_DELETE", &json!({"id":"6","type":0,"guild_id":"500"}));
      let moved = reorder("500", br#"[{"id":"4","position":0}]"#).unwrap();
      assert_eq!(ids(&moved), ["1", "2", "3", "4", "5"]);
      assert!(reorder("501", br#"[{"id":"4","position":0}]"#).is_none());
      assert!(reorder("500", br#"[{"id":"99","position":0}]"#).is_none());
   }
}
