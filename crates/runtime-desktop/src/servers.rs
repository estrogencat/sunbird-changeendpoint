use std::{
   collections::HashMap,
   fs,
   path::{Path, PathBuf},
   sync::{Arc, Mutex, OnceLock, PoisonError},
   time::SystemTime,
};

pub const DISCORD_API: &str = "https://discord.com/api/v9";
pub const DISCORD_ORIGIN: &str = "https://discord.com";
pub const RELAY_GATEWAY: &str = "ws://127.0.0.1:8099/g";
const DISCORD_GATEWAY: &str = "wss://gateway.discord.gg";
const DISCORD_APIS: [&str; 6] = [
   "https://discord.com/api/v9",
   "https://discord.com/api/v10",
   "https://ptb.discord.com/api/v9",
   "https://canary.discord.com/api/v9",
   "https://discordapp.com/api/v9",
   "https://discord.com/api",
];

#[derive(serde::Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Server {
   #[serde(default)]
   pub name: Option<String>,
   pub api: String,
   pub gateway: String,
   pub origin: Option<String>,
   pub cdn: Option<String>,
   pub media: Option<String>,
}

#[derive(serde::Deserialize, Default)]
struct Stored {
   #[serde(default)]
   servers: HashMap<String, Server>,
   #[serde(default)]
   accounts: HashMap<String, String>,
   login: Option<String>,
}

#[derive(Default)]
pub struct Routes {
   servers: HashMap<String, Server>,
   accounts: HashMap<u64, String>,
   login: Option<String>,
   login_discord: bool,
}

impl Routes {
   pub fn active(&self) -> bool {
      !self.servers.is_empty()
   }

   pub fn login(&self) -> Option<&Server> {
      self.servers.get(self.login.as_deref()?)
   }

   /// The servers new logins can go to, as `(value, label)`, and the chosen one.
   pub fn login_choices(&self) -> (Vec<(String, String)>, String) {
      let mut choices: Vec<(String, String)> = self
         .servers
         .iter()
         .map(|(key, server)| {
            (
               key.clone(),
               server.name.clone().unwrap_or_else(|| key.clone()),
            )
         })
         .collect();
      choices.sort();
      choices.push(("discord".to_owned(), "Discord".to_owned()));
      let chosen = if self.login_discord {
         "discord".to_owned()
      } else {
         self.login.clone().unwrap_or_default()
      };
      (choices, chosen)
   }

   pub fn of_user(&self, id: u64) -> Option<&Server> {
      self.servers.get(self.accounts.get(&id)?)
   }

   fn apis(&self) -> impl Iterator<Item = &str> {
      self.servers.values().map(|server| server.api.as_str())
   }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Identity {
   Discord,
   Jwt(u64),
   Unreadable,
}

pub enum Route {
   Keep,
   Rewrite(String),
   Block,
}

static LEGACY: OnceLock<Server> = OnceLock::new();
static ROUTES: Mutex<Option<(Option<SystemTime>, Arc<Routes>)>> = Mutex::new(None);
static ACTIVE: Mutex<Option<(Option<SystemTime>, Option<u64>)>> = Mutex::new(None);
static VIEWER: Mutex<Option<Identity>> = Mutex::new(None);

fn viewer_file() -> PathBuf {
   crate::endpoints::data_dir().join("sunbird").join("viewer")
}

/// The account last on screen, remembered across restarts so images load
/// from its server before it fetches any history.
fn viewer() -> Option<Identity> {
   let mut current = VIEWER.lock().unwrap_or_else(PoisonError::into_inner);
   if current.is_none() {
      *current = fs::read_to_string(viewer_file())
         .ok()
         .and_then(|text| match text.trim() {
            "discord" => Some(Identity::Discord),
            id => id.parse().ok().map(Identity::Jwt),
         });
   }
   *current
}

pub fn set_legacy(server: Server) {
   let _ = LEGACY.set(server);
}

fn storage_file(data_dir: &Path) -> PathBuf {
   data_dir
      .join("sunbird")
      .join("storage")
      .join("spacebar.json")
}

fn modified(path: &Path) -> Option<SystemTime> {
   fs::metadata(path).and_then(|meta| meta.modified()).ok()
}

pub fn routes() -> Arc<Routes> {
   let path = storage_file(&crate::endpoints::data_dir());
   let stamp = modified(&path);
   let mut cache = ROUTES.lock().unwrap_or_else(PoisonError::into_inner);
   if let Some((seen, routes)) = cache.as_ref()
      && *seen == stamp
   {
      return routes.clone();
   }
   let stored = fs::read(&path)
      .ok()
      .and_then(|bytes| serde_json::from_slice::<Stored>(&bytes).ok())
      .unwrap_or_default();
   let servers: HashMap<String, Server> = stored
      .servers
      .into_iter()
      .filter(|(_, server)| server.api.starts_with("https://") || server.api.starts_with("http://"))
      .map(|(name, mut server)| {
         server.api = server.api.trim_end_matches('/').to_owned();
         server.gateway = server.gateway.trim_end_matches('/').to_owned();
         (name, server)
      })
      .collect();
   let routes = Arc::new(Routes {
      accounts: stored
         .accounts
         .into_iter()
         .filter(|(_, name)| servers.contains_key(name))
         .filter_map(|(id, name)| Some((id.parse().ok()?, name)))
         .collect(),
      login_discord: stored.login.as_deref() == Some("discord"),
      login: stored.login.filter(|name| servers.contains_key(name)),
      servers,
   });
   *cache = Some((stamp, routes.clone()));
   routes
}

fn active_user() -> Option<u64> {
   #[derive(serde::Deserialize)]
   struct Session {
      user_id: Option<String>,
   }
   let path = crate::endpoints::data_dir().join("session.json");
   let stamp = modified(&path);
   let mut cache = ACTIVE.lock().unwrap_or_else(PoisonError::into_inner);
   if let Some((seen, user)) = cache.as_ref()
      && *seen == stamp
   {
      return *user;
   }
   let user = fs::read(&path)
      .ok()
      .and_then(|bytes| serde_json::from_slice::<Session>(&bytes).ok())
      .and_then(|session| session.user_id?.parse().ok());
   *cache = Some((stamp, user));
   user
}

pub fn identity(authorization: &str) -> Identity {
   let token = authorization
      .strip_prefix("Bearer ")
      .or_else(|| authorization.strip_prefix("Bot "))
      .unwrap_or(authorization)
      .trim();
   let mut parts = token.split('.');
   let header = parts.next().unwrap_or_default();
   let is_jwt = base64url(header.as_bytes())
      .is_some_and(|json| serde_json::from_slice::<JwtHeader>(&json).is_ok());
   if !is_jwt {
      return Identity::Discord;
   }
   jwt_user_id(token.as_bytes()).map_or(Identity::Unreadable, Identity::Jwt)
}

#[derive(serde::Deserialize)]
struct JwtHeader {
   #[serde(rename = "alg")]
   _alg: String,
}

#[derive(serde::Deserialize)]
#[serde(untagged)]
enum Snowflake {
   Text(String),
   Number(u64),
}

#[derive(serde::Deserialize)]
struct Claims {
   id: Snowflake,
}

pub fn jwt_user_id(token: &[u8]) -> Option<u64> {
   let mut parts = token.split(|&byte| byte == b'.');
   let (_, payload, _) = (parts.next()?, parts.next()?, parts.next()?);
   if parts.next().is_some() {
      return None;
   }
   match serde_json::from_slice::<Claims>(&base64url(payload)?)
      .ok()?
      .id
   {
      Snowflake::Text(id) => id.parse().ok(),
      Snowflake::Number(id) => Some(id),
   }
}

pub fn base64url(text: &[u8]) -> Option<Vec<u8>> {
   let value = |byte: u8| match byte {
      b'A'..=b'Z' => Some(byte - b'A'),
      b'a'..=b'z' => Some(byte - b'a' + 26),
      b'0'..=b'9' => Some(byte - b'0' + 52),
      b'-' | b'+' => Some(62),
      b'_' | b'/' => Some(63),
      _ => None,
   };
   let mut out = Vec::with_capacity(text.len() * 3 / 4);
   let mut acc = 0u32;
   let mut bits = 0;
   for &byte in text.iter().take_while(|&&byte| byte != b'=') {
      acc = ((acc << 6) | u32::from(value(byte)?)) & 0xFF_FFFF;
      bits += 6;
      if bits >= 8 {
         bits -= 8;
         out.push((acc >> bits).to_le_bytes()[0]);
      }
   }
   Some(out)
}

fn api_rest<'a>(url: &'a str, routes: &Routes) -> Option<&'a str> {
   DISCORD_APIS
      .into_iter()
      .chain(routes.apis())
      .chain(LEGACY.get().map(|server| server.api.as_str()))
      .filter(|base| {
         url.strip_prefix(base)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(['/', '?']))
      })
      .max_by_key(|base| base.len())
      .map(|base| &url[base.len()..])
}

/// Makes `viewer` the account whose server images come from.
pub fn set_viewer(viewer: Identity) {
   if viewer == Identity::Unreadable {
      return;
   }
   let changed = VIEWER
      .lock()
      .unwrap_or_else(PoisonError::into_inner)
      .replace(viewer)
      != Some(viewer);
   if changed {
      let text = if let Identity::Jwt(id) = viewer {
         log::info!("servers: viewing account {id}");
         id.to_string()
      } else {
         log::info!("servers: viewing a Discord account");
         "discord".to_owned()
      };
      let _ = fs::write(viewer_file(), text);
   }
}

/// Only the account on screen loads message history, so its token names
/// the account whose server the images come from.
fn loads_history(method: &str, rest: &str) -> bool {
   method.eq_ignore_ascii_case("GET")
      && rest
         .split('?')
         .next()
         .and_then(|path| path.strip_prefix("/channels/"))
         .and_then(|path| path.strip_suffix("/messages"))
         .is_some_and(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()))
}

pub fn route(method: &str, url: &str, authorization: Option<&str>) -> Route {
   let routes = routes();
   if !routes.active() {
      return Route::Keep;
   }
   let Some(rest) = api_rest(url, &routes) else {
      return Route::Keep;
   };
   let who = authorization.map(identity);
   if let Some(viewer @ (Identity::Discord | Identity::Jwt(_))) = who
      && loads_history(method, rest)
   {
      crate::account_switch::history_loaded(viewer);
      set_viewer(viewer);
   }
   let base = match who {
      None if routes.login_discord => DISCORD_API,
      None => routes
         .login()
         .or_else(|| LEGACY.get())
         .map_or(DISCORD_API, |server| server.api.as_str()),
      Some(Identity::Discord) => DISCORD_API,
      Some(Identity::Jwt(id)) => {
         let Some(server) = routes.of_user(id).or_else(|| LEGACY.get()) else {
            log::warn!("servers: an account with no known server made a request, blocked");
            return Route::Block;
         };
         server.api.as_str()
      },
      Some(Identity::Unreadable) => {
         log::warn!("servers: a request carried an unreadable token, blocked");
         return Route::Block;
      },
   };
   let next = format!("{base}{rest}");
   if next == url {
      Route::Keep
   } else {
      Route::Rewrite(next)
   }
}

/// The gateway the account behind `token` connects to, or `None` when no
/// known server holds it.
pub fn gateway_for(token: &str) -> Option<String> {
   match identity(token) {
      Identity::Discord => Some(DISCORD_GATEWAY.to_owned()),
      Identity::Jwt(id) => routes()
         .of_user(id)
         .or_else(|| LEGACY.get())
         .map(|server| server.gateway.clone()),
      Identity::Unreadable => None,
   }
}

pub fn media_server() -> Option<Server> {
   let routes = routes();
   match viewer() {
      Some(Identity::Jwt(id)) => return routes.of_user(id).or_else(|| LEGACY.get()).cloned(),
      Some(Identity::Discord) => return None,
      Some(Identity::Unreadable) | None => {},
   }
   match active_user() {
      Some(id) => routes.of_user(id).or_else(|| LEGACY.get()).cloned(),
      None if routes.login_discord => None,
      None => routes.login().or_else(|| LEGACY.get()).cloned(),
   }
}
