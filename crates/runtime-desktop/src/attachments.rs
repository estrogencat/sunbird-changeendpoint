//! Spacebar answers that Phoenix reads differently from Discord's.
//!
//! Discord flags animated attachments, and Phoenix only plays a webp image
//! that carries the flag. Spacebar servers never set it, so their webp
//! attachments and linked webp images get it here before Phoenix reads the
//! message. Spacebar has no spoiler flag either, only the `SPOILER_` file
//! name, so sent spoilers get the name and received ones get the flag back.
//! Spacebar also answers an attachment link refresh with bare links
//! where Discord pairs each with the one asked for, and Phoenix drops the
//! whole answer.

use serde::Deserialize;
use serde_json::Value;

const IS_SPOILER: u64 = 1 << 3;
const IS_ANIMATED: u64 = 1 << 5;
const SPOILER_PREFIX: &str = "SPOILER_";

/// A Spacebar request whose answer needs reshaping.
pub enum Fix {
   /// Message lists and sends, whose webp attachments need the flag.
   Messages,
   /// An attachment link refresh, with the links asked for in order.
   RefreshUrls(Vec<String>),
   /// GIF picker results, whose links need to be the GIF files.
   Gifs,
   /// The account, shown verified.
   Me,
   /// A server, shown boosted.
   Guild,
}

impl Fix {
   /// The fix a request needs, if it goes to a Spacebar server.
   pub fn of(method: &str, url: &str, body: &[u8]) -> Option<Self> {
      #[derive(Deserialize)]
      struct Refresh {
         attachment_urls: Vec<String>,
      }
      let path = spacebar_path(url)?;
      if method == "GET" && crate::gifs::is_picker(path) {
         return Some(Self::Gifs);
      }
      if method == "GET" {
         let tweaks = crate::servers::routes().tweaks();
         if tweaks.verified && path.ends_with("/users/@me") {
            return Some(Self::Me);
         }
         if tweaks.boosts && guild_id(path).is_some() {
            return Some(Self::Guild);
         }
      }
      if method == "POST" && path.ends_with("/attachments/refresh-urls") {
         let refresh = serde_json::from_slice::<Refresh>(body).ok()?;
         return Some(Self::RefreshUrls(refresh.attachment_urls));
      }
      (matches!(method, "GET" | "POST" | "PATCH")
         && (path.contains("/messages") || path.ends_with("/pins")))
      .then_some(Self::Messages)
   }

   /// The reshaped answer, or `None` when it needs no change.
   pub fn apply(&self, body: &[u8]) -> Option<Vec<u8>> {
      match self {
         Self::Messages => flag_response(body),
         Self::RefreshUrls(asked) => paired_refresh(body, asked),
         Self::Gifs => crate::gifs::direct_links(body),
         Self::Me => reshaped(body, crate::tweaks::verify),
         Self::Guild => reshaped(body, crate::tweaks::boost),
      }
   }
}

fn reshaped(body: &[u8], change: fn(&mut Value)) -> Option<Vec<u8>> {
   let mut value = serde_json::from_slice::<Value>(body).ok()?;
   change(&mut value);
   serde_json::to_vec(&value).ok()
}

/// The id of a request for one server, `/guilds/<id>`.
fn guild_id(path: &str) -> Option<&str> {
   path
      .rsplit_once("/guilds/")
      .map(|(_, id)| id)
      .filter(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
}

/// The path of a request to a Spacebar server, without its query.
fn spacebar_path(url: &str) -> Option<&str> {
   let path = url.split(['?', '#']).next().unwrap_or(url);
   (!path.contains("discord.com/") && !path.contains("discordapp.com/")).then_some(path)
}

/// A request body to send in place of the one Phoenix built, for message
/// sends and edits, server edits and channel moves to a Spacebar server.
pub fn rewrite_request(method: &str, url: &str, body: &[u8]) -> Option<Vec<u8>> {
   let path = spacebar_path(url)?;
   if method == "PATCH"
      && let Some(id) = path.strip_suffix("/channels").and_then(guild_id)
   {
      return crate::channel_order::reorder(id, body);
   }
   if method == "PATCH"
      && let Some(id) = guild_id(path)
   {
      return crate::servers::routes()
         .tweaks()
         .boosts
         .then(|| crate::tweaks::unboosted_edit(id, body))
         .flatten();
   }
   let sends = matches!(method, "POST" | "PATCH")
      && path
         .rsplit_once("/messages")
         .is_some_and(|(_, rest)| {
            rest.is_empty()
               || rest
                  .strip_prefix('/')
                  .is_some_and(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
         });
   if !sends {
      return None;
   }
   let spoilered = spoiler_names(body);
   crate::gifs::outgoing(spoilered.as_deref().unwrap_or(body)).or(spoilered)
}

/// A message body with spoilered attachments named `SPOILER_` instead of
/// flagged `is_spoiler`, the only way Spacebar stores a spoiler.
fn spoiler_names(body: &[u8]) -> Option<Vec<u8>> {
   let mut value = serde_json::from_slice::<Value>(body).ok()?;
   let Some(Value::Array(attachments)) = value.get_mut("attachments") else {
      return None;
   };
   let mut changed = false;
   for attachment in attachments {
      let Value::Object(fields) = attachment else {
         continue;
      };
      let Some(spoiler) = fields.remove("is_spoiler") else {
         continue;
      };
      changed = true;
      if spoiler.as_bool() == Some(true)
         && let Some(Value::String(name)) = fields.get_mut("filename")
         && !name.starts_with(SPOILER_PREFIX)
      {
         name.insert_str(0, SPOILER_PREFIX);
      }
   }
   changed.then(|| serde_json::to_vec(&value).ok()).flatten()
}

#[derive(serde::Serialize)]
struct RefreshedUrl {
   original:  String,
   refreshed: String,
}

/// A refresh answer of bare links, as `{original, refreshed}` pairs in the
/// order they were asked for.
fn paired_refresh(body: &[u8], asked: &[String]) -> Option<Vec<u8>> {
   #[derive(Deserialize)]
   struct Bare {
      refreshed_urls: Vec<String>,
   }
   #[derive(serde::Serialize)]
   struct Paired {
      refreshed_urls: Vec<RefreshedUrl>,
   }
   let bare = serde_json::from_slice::<Bare>(body).ok()?;
   let refreshed_urls = bare
      .refreshed_urls
      .into_iter()
      .enumerate()
      .map(|(at, refreshed)| RefreshedUrl {
         original: asked.get(at).cloned().unwrap_or_else(|| refreshed.clone()),
         refreshed,
      })
      .collect();
   serde_json::to_vec(&Paired { refreshed_urls }).ok()
}

fn is_webp_url(url: &str) -> bool {
   let path = url.split(['?', '#']).next().unwrap_or(url);
   path.to_ascii_lowercase().ends_with(".webp")
}

fn is_webp(attachment: &Value) -> bool {
   attachment.get("content_type").and_then(Value::as_str) == Some("image/webp")
      || attachment
         .get("filename")
         .and_then(Value::as_str)
         .is_some_and(|name| name.to_ascii_lowercase().ends_with(".webp"))
}

fn is_spoiler(attachment: &Value) -> bool {
   attachment
      .get("filename")
      .and_then(Value::as_str)
      .is_some_and(|name| name.starts_with(SPOILER_PREFIX))
}

fn flag(fields: &mut Value, bit: u64) -> bool {
   let flags = fields.get("flags").and_then(Value::as_u64).unwrap_or(0);
   if flags & bit != 0 {
      return false;
   }
   let Value::Object(fields) = fields else {
      return false;
   };
   fields.insert("flags".to_owned(), Value::from(flags | bit));
   true
}

/// Embedded media from a webp link, which Discord flags the same way.
fn is_webp_media(media: &Value) -> bool {
   media.get("content_type").and_then(Value::as_str) == Some("image/webp")
      || ["url", "proxy_url"]
         .iter()
         .filter_map(|key| media.get(key).and_then(Value::as_str))
         .any(is_webp_url)
}

/// Flags the webp attachments and embeds of a message and the one it replies to.
/// Returns whether anything changed.
pub fn flag_message(message: &mut Value) -> bool {
   let mut changed = false;
   if let Some(Value::Array(attachments)) = message.get_mut("attachments") {
      for attachment in attachments {
         if is_webp(attachment) {
            changed |= flag(attachment, IS_ANIMATED);
         }
         if is_spoiler(attachment) {
            changed |= flag(attachment, IS_SPOILER);
         }
      }
   }
   if let Some(Value::Array(embeds)) = message.get_mut("embeds") {
      for embed in embeds {
         for key in ["image", "thumbnail", "video"] {
            if let Some(media) = embed.get_mut(key)
               && is_webp_media(media)
            {
               changed |= flag(media, IS_ANIMATED);
            }
         }
      }
   }
   if let Some(reply) = message.get_mut("referenced_message") {
      changed |= flag_message(reply);
   }
   changed
}

/// A messages response with its webp attachments flagged, or `None` when
/// nothing changed. Takes a message list, one message, or search results.
pub fn flag_response(body: &[u8]) -> Option<Vec<u8>> {
   let mut value = serde_json::from_slice::<Value>(body).ok()?;
   let changed = if let Some(Value::Array(groups)) = value.get_mut("messages") {
      groups
         .iter_mut()
         .filter_map(Value::as_array_mut)
         .flatten()
         .fold(false, |changed, message| flag_message(message) | changed)
   } else if let Value::Array(messages) = &mut value {
      messages
         .iter_mut()
         .fold(false, |changed, message| flag_message(message) | changed)
   } else {
      value.is_object() && flag_message(&mut value)
   };
   changed.then(|| serde_json::to_vec(&value).ok()).flatten()
}

#[cfg(test)]
mod tests {
   use super::{
      Fix,
      flag_response,
      rewrite_request,
   };

   #[test]
   fn flags_webp_attachments() {
      let body = br#"[{"id":"1","attachments":[{"filename":"a.webp"},{"filename":"b.png"}]}]"#;
      let out: serde_json::Value = serde_json::from_slice(&flag_response(body).unwrap()).unwrap();
      assert_eq!(out[0]["attachments"][0]["flags"], 32);
      assert!(out[0]["attachments"][1].get("flags").is_none());
      assert!(flag_response(br#"[{"id":"1","attachments":[]}]"#).is_none());
      let body = br#"{"id":"1","embeds":[{"type":"image","thumbnail":{"url":"https://x.test/a.webp?b=1"}}]}"#;
      let out: serde_json::Value = serde_json::from_slice(&flag_response(body).unwrap()).unwrap();
      assert_eq!(out["embeds"][0]["thumbnail"]["flags"], 32);
   }

   #[test]
   fn names_spoilers() {
      let body = br#"[{"id":"1","attachments":[{"filename":"SPOILER_a.png","flags":4}]}]"#;
      let out: serde_json::Value = serde_json::from_slice(&flag_response(body).unwrap()).unwrap();
      assert_eq!(out[0]["attachments"][0]["flags"], 12);
      let url = "https://api.x.test/api/v9/channels/1/messages";
      let sent = br#"{"content":"","attachments":[{"id":"0","filename":"a.png","uploaded_filename":"u","is_spoiler":true},{"id":"1","filename":"b.png","is_spoiler":false}]}"#;
      let out: serde_json::Value =
         serde_json::from_slice(&rewrite_request("POST", url, sent).unwrap()).unwrap();
      assert_eq!(out["attachments"][0]["filename"], "SPOILER_a.png");
      assert_eq!(out["attachments"][0]["uploaded_filename"], "u");
      assert!(out["attachments"][0].get("is_spoiler").is_none());
      assert_eq!(out["attachments"][1]["filename"], "b.png");
      assert!(rewrite_request("POST", url, br#"{"content":"hi"}"#).is_none());
   }

   #[test]
   fn pairs_refreshed_urls() {
      let fix = Fix::of(
         "POST",
         "https://api.x.test/api/v9/attachments/refresh-urls",
         br#"{"attachment_urls":["https://cdn.x.test/a.png?ex=1"]}"#,
      )
      .unwrap();
      let out = fix
         .apply(br#"{"refreshed_urls":["https://cdn.x.test/a.png?ex=2"]}"#)
         .unwrap();
      let out: serde_json::Value = serde_json::from_slice(&out).unwrap();
      assert_eq!(out["refreshed_urls"][0]["original"], "https://cdn.x.test/a.png?ex=1");
      assert_eq!(out["refreshed_urls"][0]["refreshed"], "https://cdn.x.test/a.png?ex=2");
      assert!(fix
         .apply(br#"{"refreshed_urls":[{"original":"a","refreshed":"b"}]}"#)
         .is_none());
      let discord = "https://discord.com/api/v9/attachments/refresh-urls";
      assert!(Fix::of("POST", discord, b"{}").is_none());
   }
}
