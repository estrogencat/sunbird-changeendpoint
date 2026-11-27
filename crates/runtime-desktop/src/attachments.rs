//! Discord flags animated attachments, and Phoenix only plays a webp image
//! that carries the flag. Spacebar servers never set it, so their webp
//! attachments and linked webp images get it here before Phoenix reads the
//! message.

use serde_json::Value;

const IS_ANIMATED: u64 = 1 << 5;

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
   use super::flag_response;

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
}
