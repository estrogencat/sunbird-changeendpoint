//! The Servers page's Spacebar tweaks, applied to what Spacebar servers send
//! and to what Phoenix sends back.
//!
//! Spacebar has no boosting, so every server sits at level 0 and Phoenix
//! hides the perks the server would allow anyway. Boosted servers show level
//! 3 with its perks, and edits Phoenix sends back drop the perks the server
//! never had, which Spacebar would refuse. Spacebar servers that can't send
//! emails also leave accounts unverified, which locks them out of servers
//! requiring it.

use std::{
   collections::HashMap,
   sync::{Mutex, PoisonError},
};

use serde_json::Value;

/// Boosts for level 3, where Discord unlocks every perk below.
const BOOSTS: u64 = 14;

const PERKS: [&str; 10] = [
   "ANIMATED_ICON",
   "ANIMATED_BANNER",
   "BANNER",
   "INVITE_SPLASH",
   "VANITY_URL",
   "MORE_EMOJI",
   "MORE_STICKERS",
   "MORE_SOUNDBOARD",
   "ROLE_ICONS",
   "ROLE_SUBSCRIPTIONS_ENABLED",
];

/// Each boosted server's features as its server sent them, by id.
static REAL: Mutex<Option<HashMap<String, Vec<String>>>> = Mutex::new(None);

/// Shows `guild` at boost level 3, keeping a higher real boost count.
pub fn boost(guild: &mut Value) {
   let Value::Object(fields) = guild else {
      return;
   };
   let count = fields
      .get("premium_subscription_count")
      .and_then(Value::as_u64)
      .unwrap_or_default()
      .max(BOOSTS);
   fields.insert("premium_subscription_count".to_owned(), Value::from(count));
   let id = fields.get("id").and_then(Value::as_str).map(str::to_owned);
   let properties = if fields.get("properties").is_some_and(Value::is_object) {
      let Some(Value::Object(properties)) = fields.get_mut("properties") else {
         return;
      };
      properties
   } else {
      fields
   };
   properties.insert("premium_tier".to_owned(), Value::from(3));
   let mut features: Vec<String> = match properties.get("features") {
      Some(Value::Array(features)) => {
         features
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect()
      },
      _ => Vec::new(),
   };
   let id = id.or_else(|| properties.get("id").and_then(Value::as_str).map(str::to_owned));
   if let Some(id) = id {
      REAL
         .lock()
         .unwrap_or_else(PoisonError::into_inner)
         .get_or_insert_default()
         .insert(id, features.clone());
   }
   for perk in PERKS {
      if !features.iter().any(|feature| feature == perk) {
         features.push(perk.to_owned());
      }
   }
   properties.insert(
      "features".to_owned(),
      Value::Array(features.into_iter().map(Value::String).collect()),
   );
}

/// An edit of server `id` without the perks [`boost`] added, or `None` to
/// send it as it is. Features of a server never seen go out without its perks
/// at all.
pub fn unboosted_edit(id: &str, body: &[u8]) -> Option<Vec<u8>> {
   if !body.windows(10).any(|window| window == b"\"features\"") {
      return None;
   }
   let mut value = serde_json::from_slice::<Value>(body).ok()?;
   let Some(Value::Array(features)) = value.get_mut("features") else {
      return None;
   };
   let real = REAL
      .lock()
      .unwrap_or_else(PoisonError::into_inner)
      .as_ref()
      .and_then(|real| real.get(id).cloned())
      .unwrap_or_default();
   let before = features.len();
   features.retain(|feature| {
      feature
         .as_str()
         .is_none_or(|feature| !PERKS.contains(&feature) || real.iter().any(|kept| kept == feature))
   });
   if features.len() == before {
      return None;
   }
   serde_json::to_vec(&value).ok()
}

/// Marks the account as verified.
pub fn verify(user: &mut Value) {
   if let Value::Object(fields) = user {
      fields.insert("verified".to_owned(), Value::Bool(true));
   }
}

#[cfg(test)]
mod tests {
   use serde_json::json;

   use super::{boost, unboosted_edit};

   #[test]
   fn boosts_and_unboosts() {
      let mut guild = json!({"id":"77","premium_subscription_count":1,"properties":{"premium_tier":0,"features":["COMMUNITY","BANNER"]}});
      boost(&mut guild);
      assert_eq!(guild["premium_subscription_count"], 14);
      assert_eq!(guild["properties"]["premium_tier"], 3);
      let features = guild["properties"]["features"].as_array().unwrap();
      assert!(features.contains(&json!("ROLE_ICONS")));
      assert_eq!(features.iter().filter(|f| *f == "BANNER").count(), 1);
      let edit = serde_json::to_vec(&json!({"features": features, "name":"x"})).unwrap();
      let out: serde_json::Value =
         serde_json::from_slice(&unboosted_edit("77", &edit).unwrap()).unwrap();
      assert_eq!(out["features"], json!(["COMMUNITY", "BANNER"]));
      assert!(unboosted_edit("77", br#"{"name":"x"}"#).is_none());
      let mut flat = json!({"id":"78","premium_subscription_count":20});
      boost(&mut flat);
      assert_eq!(flat["premium_subscription_count"], 20);
      assert_eq!(flat["premium_tier"], 3);
   }
}
