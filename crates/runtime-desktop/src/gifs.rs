//! GIFs on Spacebar servers.
//!
//! Phoenix sends a picked GIF as its `url`, the provider's page, which
//! Spacebar can't embed as an animated image. Search results get their GIF
//! file as `url` instead. Tenor's `media1.tenor.com/m/` links are pages too,
//! so sent messages get the `media.tenor.com` file link. Favorites saved
//! before keep the page link, which a send swaps when this session's results
//! showed its file.

use std::{
   collections::HashMap,
   sync::{
      LazyLock,
      Mutex,
      PoisonError,
   },
};

use serde_json::Value;

const TENOR_PAGE: &str = "https://media1.tenor.com/m/";
const TENOR_FILE: &str = "https://media.tenor.com/";

/// Page links from picker results and their GIF files.
static FILES: LazyLock<Mutex<HashMap<String, String>>> = LazyLock::new(Mutex::default);
/// Keeps a long session from growing the map without bound.
const FILES_CAP: usize = 4096;

/// Whether a GIF picker request goes to these paths.
pub fn is_picker(path: &str) -> bool {
   ["/gifs/search", "/gifs/trending", "/gifs/trending-gifs"]
      .iter()
      .any(|end| path.ends_with(end))
}

/// The GIF file a result links to, skipping video previews.
fn file_of(result: &Value) -> Option<&str> {
   let gif_src = result.get("gif_src").and_then(Value::as_str);
   let src = result
      .get("src")
      .and_then(Value::as_str)
      .filter(|src| !is_video(src));
   gif_src
      .or(src)
      .filter(|link| !link.is_empty())
}

fn is_video(link: &str) -> bool {
   let path = link.split(['?', '#']).next().unwrap_or(link);
   path
      .rsplit_once('.')
      .is_some_and(|(_, ext)| ext.eq_ignore_ascii_case("mp4") || ext.eq_ignore_ascii_case("webm"))
}

fn with_scheme(link: &str) -> String {
   if link.starts_with("//") {
      format!("https:{link}")
   } else {
      link.to_owned()
   }
}

/// Picker results with their GIF files as `url`, or `None` when nothing
/// changed. Takes a result list or trending's `{categories, gifs}`.
pub fn direct_links(body: &[u8]) -> Option<Vec<u8>> {
   let mut value = serde_json::from_slice::<Value>(body).ok()?;
   let results = match &mut value {
      Value::Array(results) => results,
      Value::Object(fields) => match fields.get_mut("gifs") {
         Some(Value::Array(results)) => results,
         _ => return None,
      },
      _ => return None,
   };
   let mut changed = false;
   let mut files = FILES.lock().unwrap_or_else(PoisonError::into_inner);
   if files.len() > FILES_CAP {
      files.clear();
   }
   for result in results {
      let Some(file) = file_of(result).map(with_scheme) else {
         continue;
      };
      let page = result.get("url").and_then(Value::as_str).map(str::to_owned);
      if page.as_deref() != Some(file.as_str())
         && let Value::Object(fields) = result
      {
         if let Some(page) = page {
            files.insert(page, file.clone());
         }
         fields.insert("url".to_owned(), Value::String(file));
         changed = true;
      }
   }
   drop(files);
   changed.then(|| serde_json::to_vec(&value).ok()).flatten()
}

/// `text` with Tenor page links swapped for their file links.
fn tenor_files(text: &str) -> Option<String> {
   let mut out = String::with_capacity(text.len());
   let mut rest = text;
   let mut changed = false;
   while let Some(at) = rest.find(TENOR_PAGE) {
      let (before, from) = rest.split_at(at);
      out.push_str(before);
      let tail = &from[TENOR_PAGE.len()..];
      let end = tail
         .find(|c: char| c.is_whitespace() || matches!(c, '?' | '#' | '<' | '>' | ')'))
         .unwrap_or(tail.len());
      let mut parts = tail[..end].split('/');
      let id = parts.next().unwrap_or_default();
      let name = parts.next().unwrap_or_default();
      let is_file = parts.next().is_none()
         && !id.is_empty()
         && !name.is_empty()
         && id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'));
      if is_file {
         out.push_str(TENOR_FILE);
         changed = true;
      } else {
         out.push_str(TENOR_PAGE);
      }
      rest = tail;
   }
   out.push_str(rest);
   changed.then_some(out)
}

/// A message being sent to a Spacebar server with Tenor page links swapped
/// for file links, or `None` when it needs no change.
pub fn outgoing(body: &[u8]) -> Option<Vec<u8>> {
   let mut value = serde_json::from_slice::<Value>(body).ok()?;
   let content = value.get("content").and_then(Value::as_str)?;
   let known = FILES
      .lock()
      .unwrap_or_else(PoisonError::into_inner)
      .get(content.trim())
      .cloned();
   let swapped = known.or_else(|| tenor_files(content))?;
   let Value::Object(fields) = &mut value else {
      return None;
   };
   fields.insert("content".to_owned(), Value::String(swapped));
   serde_json::to_vec(&value).ok()
}

#[cfg(test)]
mod tests {
   use super::{
      direct_links,
      outgoing,
   };

   #[test]
   fn links_gif_files() {
      let body = br#"[{"url":"https://tenor.com/view/a","src":"https://x.test/a.mp4","gif_src":"https://x.test/a.gif"},{"url":"https://klipy.com/b","src":"//x.test/b.gif"}]"#;
      let out: serde_json::Value = serde_json::from_slice(&direct_links(body).unwrap()).unwrap();
      assert_eq!(out[0]["url"], "https://x.test/a.gif");
      assert_eq!(out[1]["url"], "https://x.test/b.gif");
      let video_only = br#"{"categories":[],"gifs":[{"url":"https://p.test/c","src":"https://x.test/c.mp4"}]}"#;
      assert!(direct_links(video_only).is_none());
   }

   #[test]
   fn swaps_seen_pages() {
      direct_links(br#"[{"url":"https://klipy.com/gifs/seen","gif_src":"https://x.test/seen.gif"}]"#);
      let out = outgoing(br#"{"content":"https://klipy.com/gifs/seen"}"#).unwrap();
      let out: serde_json::Value = serde_json::from_slice(&out).unwrap();
      assert_eq!(out["content"], "https://x.test/seen.gif");
      assert!(outgoing(br#"{"content":"https://klipy.com/gifs/unseen"}"#).is_none());
   }

   #[test]
   fn swaps_tenor_pages() {
      let body = br#"{"content":"look https://media1.tenor.com/m/AbC-1_x/cat.gif ok","nonce":"1"}"#;
      let out: serde_json::Value = serde_json::from_slice(&outgoing(body).unwrap()).unwrap();
      assert_eq!(out["content"], "look https://media.tenor.com/AbC-1_x/cat.gif ok");
      assert_eq!(out["nonce"], "1");
      assert!(outgoing(br#"{"content":"https://media1.tenor.com/m/only"}"#).is_none());
      assert!(outgoing(br#"{"content":"hi"}"#).is_none());
   }
}
