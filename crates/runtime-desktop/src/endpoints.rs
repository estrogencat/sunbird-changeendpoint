use std::{env, fs, path::PathBuf, ptr, sync::OnceLock};

use phoenix_hook::{
   image::Image,
   x86::{self, Abi},
};
use phoenix_sigs::desktop;

use crate::servers;

type ForStackFn = unsafe extern "C" fn(*mut u8, u8) -> *mut u8;
type StringFromFn = unsafe extern "C" fn(*mut [usize; 3], *const u8, usize) -> *mut [usize; 3];

const SLOTS: usize = 5;
const SLOT_SIZE: usize = size_of::<[usize; 3]>();
const MAX_URL: usize = 4096;
const GATEWAY: &[u8] = b"wss://gateway.discord.gg";
const REMOTE_AUTH: &[u8] = b"wss://remote-auth-gateway.discord.gg/?v=2";
const API_SUFFIX: &[u8] = b"/api/v9";
const NO_REMOTE_AUTH: &str = "wss://remote-auth.invalid/";
const SECRET_FILES: [&str; 2] = ["secrets.json", "secrets.dpapi"];

#[derive(serde::Deserialize)]
struct Session {
   user_id: Option<serde::de::IgnoredAny>,
}

#[derive(serde::Deserialize, serde::Serialize)]
struct Config {
   api: String,
   gateway: String,
   origin: Option<String>,
   cdn: Option<String>,
   media: Option<String>,
}

struct Target {
   api: String,
   origin: String,
   gateway: String,
   cdn: Option<String>,
   media: Option<String>,
}

struct Installed {
   string_from: StringFromFn,
   target: Target,
}

static INSTALLED: OnceLock<Installed> = OnceLock::new();
static ORIGINAL: OnceLock<ForStackFn> = OnceLock::new();

pub fn data_dir() -> PathBuf {
   env::var_os("LIBDISCORD_DATA_DIR").map_or_else(
      || {
         env::var_os("LOCALAPPDATA")
            .map_or_else(env::temp_dir, PathBuf::from)
            .join("DiscordPhoenix")
      },
      PathBuf::from,
   )
}

fn origin_of(url: &str) -> Option<String> {
   let scheme_end = url.find("://")? + 3;
   let path = url[scheme_end..]
      .find('/')
      .map_or(url.len(), |at| scheme_end + at);
   Some(url[..path].to_owned())
}

fn target(config: Config) -> Result<Target, String> {
   let api = config.api.trim_end_matches('/').to_owned();
   let gateway = config.gateway.trim_end_matches('/').to_owned();
   if !api.starts_with("https://") && !api.starts_with("http://") {
      return Err(format!("api {api} is not an http(s) URL"));
   }
   if !gateway.starts_with("wss://") && !gateway.starts_with("ws://") {
      return Err(format!("gateway {gateway} is not a ws(s) URL"));
   }
   let origin = match config.origin {
      Some(origin) => origin.trim_end_matches('/').to_owned(),
      None => origin_of(&api).ok_or_else(|| format!("api {api} has no origin"))?,
   };
   let host = |url: Option<String>, name: &str| -> Result<Option<String>, String> {
      url.map(|url| {
         let url = url.trim_end_matches('/').to_owned();
         if url.starts_with("https://") || url.starts_with("http://") {
            Ok(url)
         } else {
            Err(format!("{name} {url} is not an http(s) URL"))
         }
      })
      .transpose()
   };
   Ok(Target {
      api,
      origin,
      gateway,
      cdn: host(config.cdn, "cdn")?,
      media: host(config.media, "media")?,
   })
}

pub fn media_url(url: &str) -> Option<String> {
   ORIGINAL.get()?;
   {
      static SEEN: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
      let key: String = url
         .split(['?', '#'])
         .next()
         .unwrap_or(url)
         .splitn(5, '/')
         .take(4)
         .collect::<Vec<_>>()
         .join("/");
      let mut seen = SEEN
         .lock()
         .unwrap_or_else(std::sync::PoisonError::into_inner);
      if !seen.contains(&key) {
         log::info!("endpoints: media fetch {key}");
         seen.push(key);
      }
   }
   if let Some(external) = external_url(url) {
      return Some(external);
   }
   if DISCORD_APP_ASSETS
      .iter()
      .any(|prefix| url.starts_with(prefix))
   {
      return None;
   }
   INSTALLED.get()?;
   let server = signed_in_owner(url).map_or_else(servers::media_server, servers::media_server_of)?;
   let (cdn, media) = (server.cdn, server.media);
   let cdn = cdn.as_deref();
   if let Some(decoration) = decoration_url(url, cdn?) {
      return Some(decoration);
   }
   [
      ("https://cdn.discordapp.com/", cdn),
      ("https://media.discordapp.net/", media.as_deref().or(cdn)),
   ]
   .into_iter()
   .find_map(|(from, to)| Some(format!("{}/{}", to?, url.strip_prefix(from)?)))
   .inspect(|rewritten| {
      if !LOGGED_MEDIA.swap(true, std::sync::atomic::Ordering::Relaxed) {
         log::info!("endpoints: first media fetch rewritten to {rewritten}");
      }
   })
}

/// The signed-in account a user image belongs to. The account switcher shows
/// every account's avatar while another one is on screen.
fn signed_in_owner(url: &str) -> Option<servers::Identity> {
   let path = url.strip_prefix("https://cdn.discordapp.com/")?;
   let mut parts = path.split('/');
   let user = match parts.next()? {
      "avatars" | "banners" => parts.next()?,
      "guilds" => parts.nth(2)?,
      _ => return None,
   };
   servers::identity_of_user(user.parse().ok()?)
}

const DISCORD_APP_ASSETS: [&str; 2] = [
   "https://cdn.discordapp.com/app-assets/",
   "https://cdn.discordapp.com/app-icons/",
];

fn external_url(url: &str) -> Option<String> {
   let rest = url.strip_prefix("https://media.discordapp.net/external/")?;
   let (_, rest) = rest.split_once('/')?;
   let (scheme, rest) = rest.split_once('/')?;
   matches!(scheme, "http" | "https").then(|| format!("{scheme}://{rest}"))
}

#[derive(serde::Deserialize)]
struct PluginStore {
   #[serde(default)]
   decorations: std::collections::HashMap<String, String>,
}

fn decoration_url(url: &str, cdn: &str) -> Option<String> {
   let rest = url.strip_prefix("https://cdn.discordapp.com/media/v1/collectibles-shop/")?;
   let sku = rest.split('/').next()?;
   let path = phoenix_runtime::state()?.storage_path("spacebar");
   let store = read_json::<PluginStore>(&path)?;
   let asset = store.decorations.get(sku)?;
   asset
      .bytes()
      .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
      .then(|| format!("{cdn}/avatar-decoration-presets/{asset}.png"))
}

static LOGGED_MEDIA: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn read_json<T: serde::de::DeserializeOwned>(path: &std::path::Path) -> Option<T> {
   serde_json::from_slice(&fs::read(path).ok()?).ok()
}

fn backend() -> Option<Config> {
   let dir = data_dir();
   let marker = dir.join("sunbird-endpoints");
   let signed_in = read_json::<Session>(&dir.join("session.json"))
      .is_some_and(|session| session.user_id.is_some())
      || read_json::<Vec<serde::de::IgnoredAny>>(&dir.join("accounts.json"))
         .is_some_and(|accounts| !accounts.is_empty())
      || SECRET_FILES.iter().any(|name| dir.join(name).exists());
   let wanted = fs::read(dir.join("endpoints.json"))
      .ok()
      .map(|bytes| serde_json::from_slice::<Config>(&bytes).map_err(|err| err.to_string()));
   if signed_in {
      let Some(owner) = read_json::<Config>(&marker) else {
         if wanted.is_some() {
            log::error!("endpoints: signed in on Discord, sign out before switching backends");
         }
         return None;
      };
      match &wanted {
         Some(Ok(wanted)) if wanted.api == owner.api => {},
         _ => {
            log::warn!(
               "endpoints: signed in on {}, staying there until you sign out",
               owner.api
            );
            return Some(owner);
         },
      }
   }
   let config = match wanted {
      Some(Ok(config)) => config,
      Some(Err(err)) => {
         log::error!("endpoints: endpoints.json: {err}, staying on Discord");
         let _ = fs::remove_file(&marker);
         return None;
      },
      None => {
         let _ = fs::remove_file(&marker);
         return None;
      },
   };
   let written = serde_json::to_vec(&config)
      .map_err(|err| err.to_string())
      .and_then(|record| fs::write(&marker, record).map_err(|err| err.to_string()));
   if let Err(err) = written {
      log::error!("endpoints: could not record the backend: {err}, staying on Discord");
      return None;
   }
   Some(config)
}

/// Accounts can sit on different servers, so the gateway goes through the
/// local relay, which picks the server from the token in IDENTIFY. The origin
/// stays Discord's: Discord refuses logins from any other (50067) and Spacebar
/// never checks it.
fn relayed(legacy: Option<Target>) -> Target {
   let routes = servers::routes();
   let login = routes.login().map(|server| Target {
      api: server.api.clone(),
      origin: String::new(),
      gateway: String::new(),
      cdn: server.cdn.clone(),
      media: server.media.clone(),
   });
   let base = login.or(legacy).unwrap_or_else(|| Target {
      api: servers::DISCORD_API.to_owned(),
      origin: servers::DISCORD_ORIGIN.to_owned(),
      gateway: String::new(),
      cdn: None,
      media: None,
   });
   #[cfg(windows)]
   let relay = crate::relay::gateway();
   #[cfg(not(windows))]
   let relay = None;
   let gateway = relay.unwrap_or_else(|| {
      log::warn!("endpoints: no built-in relay, expecting one at {}", servers::RELAY_GATEWAY);
      servers::RELAY_GATEWAY.to_owned()
   });
   log::info!("endpoints: several servers, the gateway goes through the local relay");
   Target {
      origin: servers::DISCORD_ORIGIN.to_owned(),
      gateway,
      ..base
   }
}

pub fn install(image: &Image<'static>) {
   let legacy = backend().and_then(|config| {
      target(config)
         .map_err(|err| log::error!("endpoints: {err}, staying on Discord"))
         .ok()
   });
   if let Some(legacy) = &legacy {
      servers::set_legacy(servers::Server {
         name: None,
         api: legacy.api.clone(),
         gateway: legacy.gateway.clone(),
         origin: Some(legacy.origin.clone()),
         cdn: legacy.cdn.clone(),
         media: legacy.media.clone(),
      });
   }
   let target = relayed(legacy);
   if Abi::of(image) != Abi::Win64 {
      log::warn!("endpoints: only Windows builds are supported, staying on Discord");
      return;
   }
   let found = match desktop::endpoints(image) {
      Ok(found) if desktop::ENDPOINTS_PROLOGUE.at(image, found.func) => found,
      Ok(found) => {
         log::warn!(
            "endpoints: prologue changed @ {:#x}, staying on Discord",
            found.func
         );
         return;
      },
      Err(err) => {
         log::warn!("endpoints: {err}, staying on Discord");
         return;
      },
   };
   let api = target.api.clone();
   let string_from = unsafe { std::mem::transmute::<usize, StringFromFn>(found.string_from) };
   let _ = INSTALLED.set(Installed {
      string_from,
      target,
   });
   let trampoline = match unsafe {
      image.hook_entry(
         found.func,
         for_stack as *const () as usize,
      )
   } {
      Ok(trampoline) => trampoline,
      Err(err) => {
         log::warn!("endpoints: {err}, staying on Discord");
         return;
      },
   };
   let _ = ORIGINAL.set(unsafe { std::mem::transmute::<usize, ForStackFn>(trampoline) });
   match desktop::preferred_regions_skip(image)
      .and_then(|je| unsafe { image.write_code(je, &[0xEB]) }.map_err(|err| err.to_string()))
   {
      Ok(()) => log::info!("endpoints: voice state updates leave out preferred_regions"),
      Err(err) => log::warn!("endpoints: preferred_regions stays in voice state updates: {err}"),
   }
   match desktop::dave_downgrade_allow(image)
      .and_then(|jne| unsafe { image.write_code(jne, &[0xEB]) }.map_err(|err| err.to_string()))
   {
      Ok(()) => log::info!("endpoints: voice accepts DAVE protocol version 0"),
      Err(err) => log::warn!("endpoints: voice still refuses DAVE version 0: {err}"),
   }
   match install_token_user_id(image) {
      Ok(()) => log::info!("endpoints: user IDs are read from JWT tokens too"),
      Err(err) => log::warn!("endpoints: JWT tokens name no user: {err}"),
   }
   log::info!("endpoints: hooked @ {:#x} for {api}", found.func);
}

#[repr(C)]
#[derive(Clone, Copy)]
struct UserId {
   tag: u64,
   id: u64,
}

type TokenUserIdFn = unsafe fn(*const *const u8) -> UserId;

struct TokenParser {
   original: TokenUserIdFn,
   data: usize,
   len: usize,
}

static TOKEN_PARSER: OnceLock<TokenParser> = OnceLock::new();

fn install_token_user_id(image: &Image<'static>) -> Result<(), String> {
   let found = desktop::token_user_id(image)?;
   if !desktop::ENDPOINTS_PROLOGUE.at(image, found.func) {
      return Err(format!("prologue changed @ {:#x}", found.func));
   }
   let trampoline = unsafe {
      image.hook_entry(
         found.func,
         token_user_id as *const () as usize,
      )
   }
   .map_err(|err| err.to_string())?;
   let _ = TOKEN_PARSER.set(TokenParser {
      original: unsafe { std::mem::transmute::<usize, TokenUserIdFn>(trampoline) },
      data: found.data,
      len: found.len,
   });
   Ok(())
}

unsafe fn token_user_id(token: *const *const u8) -> UserId {
   let parser = TOKEN_PARSER.wait();
   let parsed = unsafe { (parser.original)(token) };
   if parsed.tag == 1 || token.is_null() {
      return parsed;
   }
   let jwt = std::panic::catch_unwind(|| {
      let word = |at: usize| {
         Some(usize::from_le_bytes(
            x86::read_memory(at, 8)?.try_into().ok()?,
         ))
      };
      let inner = word(token as usize)?;
      let data = word(inner + parser.data)?;
      let len = word(inner + parser.len)?;
      if len == 0 || len > MAX_URL {
         return None;
      }
      servers::jwt_user_id(&x86::read_memory(data, len)?)
   });
   match jwt {
      Ok(Some(id)) => UserId { tag: 1, id },
      _ => parsed,
   }
}

fn read_string(slot: usize) -> Option<Vec<u8>> {
   let bytes = x86::read_memory(slot, SLOT_SIZE)?;
   let words: Vec<usize> = bytes
      .as_chunks::<{ size_of::<usize>() }>()
      .0
      .iter()
      .map(|word| usize::from_le_bytes(*word))
      .collect();
   if words.len() != 3 {
      return None;
   }
   (0..3).find_map(|at| {
      let data = words[at];
      let len = words
         .iter()
         .enumerate()
         .filter(|&(other, &len)| other != at && len > 0 && len <= MAX_URL)
         .map(|(_, &len)| len)
         .min()?;
      x86::read_memory(data, len).filter(|bytes| bytes.len() == len)
   })
}

extern "C" fn for_stack(out: *mut u8, stack: u8) -> *mut u8 {
   let original = ORIGINAL.wait();
   let out = unsafe { original(out, stack) };
   if let Some(installed) = INSTALLED.get() {
      rewrite(out, installed);
   }
   out
}

fn rewrite(out: *mut u8, installed: &Installed) {
   let slots: Vec<Option<Vec<u8>>> = (0..SLOTS)
      .map(|index| read_string(out as usize + index * SLOT_SIZE))
      .collect();
   let find = |want: &[u8]| slots.iter().position(|slot| slot.as_deref() == Some(want));
   let api = slots.iter().position(|slot| {
      slot
         .as_deref()
         .is_some_and(|text| text.starts_with(b"https://") && text.ends_with(API_SUFFIX))
   });
   let origin = api
      .and_then(|index| slots[index].as_deref())
      .and_then(|text| find(&text[..text.len() - API_SUFFIX.len()]));
   let target = &installed.target;
   let plan = [
      (api, target.api.as_str()),
      (origin, target.origin.as_str()),
      (find(GATEWAY), target.gateway.as_str()),
      (find(REMOTE_AUTH), NO_REMOTE_AUTH),
   ];
   if plan.iter().any(|(slot, _)| slot.is_none()) {
      let seen: Vec<String> = (0..SLOTS)
         .map(|index| {
            let at = out as usize + index * SLOT_SIZE;
            let words = x86::read_memory(at, SLOT_SIZE).unwrap_or_default();
            let text = slots[index].as_deref().map_or_else(
               || "?".to_owned(),
               |text| String::from_utf8_lossy(text).into_owned(),
            );
            format!("{index}: {words:02x?} {text}")
         })
         .collect();
      log::error!(
         "endpoints: the endpoint table changed shape, staying on Discord ({})",
         seen.join("; ")
      );
      return;
   }
   for (slot, value) in plan {
      let Some(slot) = slot else {
         continue;
      };
      let mut fresh = [0usize; 3];
      unsafe { (installed.string_from)(&raw mut fresh, value.as_ptr(), value.len()) };
      unsafe { ptr::write_unaligned(out.add(slot * SLOT_SIZE).cast::<[usize; 3]>(), fresh) };
   }
   log::info!("endpoints: API {}, gateway {}", target.api, target.gateway);
}
