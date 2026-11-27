//! Spacebar answers that Phoenix reads differently from Discord's.
//!
//! Discord flags animated attachments, and Phoenix only plays a webp image
//! that carries the flag. Spacebar servers never set it, so their webp
//! attachments and linked webp images get it here before Phoenix reads the
//! message. Spacebar also answers an attachment link refresh with bare links
//! where Discord pairs each with the one asked for, and Phoenix drops the
//! whole answer.

use serde::Deserialize;
use serde_json::Value;

const IS_ANIMATED: u64 = 1 << 5;

/// A Spacebar request whose answer needs reshaping.
pub enum Fix {
   /// Message lists and sends, whose webp attachments need the flag.
   Messages,
   /// An attachment link refresh, with the links asked for in order.
   RefreshUrls(Vec<String>),
}

impl Fix {
   /// The fix a request needs, if it goes to a Spacebar server.
   pub fn of(method: &str, url: &str, body: &[u8]) -> Option<Self> {
      #[derive(Deserialize)]
      struct Refresh {
         attachment_urls: Vec<String>,
      }
      let path = url.split(['?', '#']).next().unwrap_or(url);
      if path.contains("discord.com/") || path.contains("discordapp.com/") {
         return None;
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
      }
   }
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

fn flag(fields: &mut Value) -> bool {
   let flags = fields.get("flags").and_then(Value::as_u64).unwrap_or(0);
   if flags & IS_ANIMATED != 0 {
      return false;
   }
   let Value::Object(fields) = fields else {
      return false;
   };
   fields.insert("flags".to_owned(), Value::from(flags | IS_ANIMATED));
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
      for attachment in attachments
         .iter_mut()
         .filter(|attachment| is_webp(attachment))
      {
         changed |= flag(attachment);
      }
   }
   if let Some(Value::Array(embeds)) = message.get_mut("embeds") {
      for embed in embeds {
         for key in ["image", "thumbnail", "video"] {
            if let Some(media) = embed.get_mut(key)
               && is_webp_media(media)
            {
               changed |= flag(media);
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
