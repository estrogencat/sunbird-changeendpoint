use std::{
   collections::{BTreeMap, HashMap},
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

#[derive(serde::Deserialize, serde::Serialize, Clone, Debug, PartialEq, Eq)]
pub struct Server {
   #[serde(default, skip_serializing_if = "Option::is_none")]
   pub name: Option<String>,
   pub api: String,
   pub gateway: String,
   #[serde(default, skip_serializing_if = "Option::is_none")]
   pub origin: Option<String>,
   #[serde(default, skip_serializing_if = "Option::is_none")]
   pub cdn: Option<String>,
   #[serde(default, skip_serializing_if = "Option::is_none")]
   pub media: Option<String>,
}

/// What the Servers page edits, kept in `sunbird/servers.json`.
#[derive(serde::Deserialize, serde::Serialize, Default, Clone)]
pub struct Config {
   #[serde(default)]
   pub enabled: bool,
   #[serde(default)]
   pub custom: BTreeMap<String, Server>,
   #[serde(default)]
   pub accounts: BTreeMap<String, String>,
   #[serde(default, skip_serializing_if = "Option::is_none")]
   pub login: Option<String>,
   /// Servers whose activity switch was flipped from its default.
   #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
   pub activities: BTreeMap<String, bool>,
}

/// What the Spacebar plugin kept before the Servers page, read once to carry
/// it over.
#[derive(serde::Deserialize, Default)]
struct PluginStored {
   #[serde(default)]
   servers: HashMap<String, Server>,
   #[serde(default)]
   accounts: BTreeMap<String, String>,
   login: Option<String>,
}

fn builtin() -> BTreeMap<String, Server> {
   let server = |name: &str, api: &str, gateway: &str, cdn: &str, origin: &str| Server {
      name: Some(name.to_owned()),
      api: api.to_owned(),
      gateway: gateway.to_owned(),
      origin: Some(origin.to_owned()),
      cdn: Some(cdn.to_owned()),
      media: None,
   };
   BTreeMap::from([
      (
         "spacebar".to_owned(),
         server(
            "Spacebar",
            "https://api.rory.server.spacebar.chat/api/v9",
            "wss://gateway.rory.server.spacebar.chat",
            "https://cdn.rory.server.spacebar.chat",
            "https://rory.server.spacebar.chat",
         ),
      ),
      (
         "hazletbar".to_owned(),
         server(
            "Hazletbar",
            "https://api.softgaypaws.com/api/v9",
            "wss://gateway.softgaypaws.com",
            "https://cdn.softgaypaws.com",
            "https://api.softgaypaws.com",
         ),
      ),
   ])
}

pub fn is_builtin(key: &str) -> bool {
   builtin().contains_key(key)
}

#[derive(Default)]
pub struct Routes {
   servers: BTreeMap<String, Server>,
   accounts: HashMap<u64, String>,
   linked: Vec<(u64, String)>,
   login: Option<String>,
   login_discord: bool,
   enabled: bool,
   activities: BTreeMap<String, bool>,
}

impl Routes {
   pub const fn active(&self) -> bool {
      self.enabled
   }

   pub fn servers(&self) -> impl Iterator<Item = (&str, &Server)> {
      self
         .servers
         .iter()
         .map(|(key, server)| (key.as_str(), server))
   }

   pub fn server(&self, key: &str) -> Option<&Server> {
      self.servers.get(key)
   }

   /// Accounts and the server each one is on, by user ID.
   pub fn linked(&self) -> &[(u64, String)] {
      &self.linked
   }

   /// The server key new logins go to, `discord` included.
   pub fn login_key(&self) -> String {
      self.login_choices().1
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

   /// Whether the server gets activities. Servers added by address start
   /// without, since stricter ones like Harmony close the gateway over them.
   pub fn shows_activity(&self, key: &str) -> bool {
      self
         .activities
         .get(key)
         .copied()
         .unwrap_or_else(|| is_builtin(key))
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

fn config_file() -> PathBuf {
   crate::endpoints::data_dir()
      .join("sunbird")
      .join("servers.json")
}

/// The plugin's servers, links and login choice, the first time Sunbird runs
/// with the Servers page.
fn carried_over() -> Config {
   let stored = fs::read(storage_file(&crate::endpoints::data_dir()))
      .ok()
      .and_then(|bytes| serde_json::from_slice::<PluginStored>(&bytes).ok())
      .unwrap_or_default();
   let builtin = builtin();
   Config {
      enabled: !stored.servers.is_empty(),
      custom: stored
         .servers
         .into_iter()
         .filter(|(key, _)| !builtin.contains_key(key))
         .collect(),
      accounts: stored.accounts,
      login: stored.login,
      activities: BTreeMap::new(),
   }
}

pub fn config() -> Config {
   fs::read(config_file()).map_or_else(
      |_| carried_over(),
      |bytes| {
         serde_json::from_slice(&bytes).unwrap_or_else(|err| {
            log::warn!("servers: servers.json is unreadable ({err}), starting empty");
            Config::default()
         })
      },
   )
}

static WRITING: Mutex<()> = Mutex::new(());

/// Changes the saved config. Routing picks the change up on its next request.
pub fn update(change: impl FnOnce(&mut Config)) -> std::io::Result<()> {
   let _writing = WRITING.lock().unwrap_or_else(PoisonError::into_inner);
   let mut config = config();
   change(&mut config);
   let path = config_file();
   if let Some(dir) = path.parent() {
      fs::create_dir_all(dir)?;
   }
   let staged = path.with_extension("json.tmp");
   fs::write(
      &staged,
      serde_json::to_vec_pretty(&config).map_err(std::io::Error::other)?,
   )?;
   fs::rename(&staged, &path)?;
   *ROUTES.lock().unwrap_or_else(PoisonError::into_inner) = None;
   Ok(())
}

fn usable(mut server: Server) -> Option<Server> {
   let web = |url: &str| url.starts_with("https://") || url.starts_with("http://");
   let socket = |url: &str| url.starts_with("wss://") || url.starts_with("ws://");
   if !web(&server.api) || !socket(&server.gateway) {
      return None;
   }
   server.api = server.api.trim_end_matches('/').to_owned();
   server.gateway = server.gateway.trim_end_matches('/').to_owned();
   Some(server)
}

pub fn routes() -> Arc<Routes> {
   let path = config_file();
   let stamp = modified(&path);
   let mut cache = ROUTES.lock().unwrap_or_else(PoisonError::into_inner);
   if let Some((seen, routes)) = cache.as_ref()
      && *seen == stamp
   {
      return routes.clone();
   }
   let config = config();
   let mut servers = builtin();
   for (key, server) in config.custom {
      if let (false, Some(server)) = (servers.contains_key(&key), usable(server)) {
         servers.insert(key, server);
      }
   }
   let linked: Vec<(u64, String)> = config
      .accounts
      .into_iter()
      .filter(|(_, key)| servers.contains_key(key))
      .filter_map(|(id, key)| Some((id.parse().ok()?, key)))
      .collect();
   let routes = Arc::new(Routes {
      accounts: linked.iter().cloned().collect(),
      linked,
      login_discord: config.login.as_deref() == Some("discord"),
      login: config.login.filter(|key| servers.contains_key(key)),
      enabled: config.enabled,
      activities: config.activities,
      servers,
   });
   *cache = Some((stamp, routes.clone()));
   routes
}

/// Links the account a login answered with to the server it logged in to.
pub fn record_login(token: &str, server: &str) {
   let Identity::Jwt(id) = identity(token) else {
      return;
   };
   match update(|config| {
      config.accounts.insert(id.to_string(), server.to_owned());
   }) {
      Ok(()) => log::info!("servers: account {id} is on {server}"),
      Err(err) => log::warn!("servers: could not link account {id}: {err}"),
   }
}

/// The server key a login request goes to, when `url` is one. `None` for
/// requests that carry a token.
pub fn login_target(method: &str, url: &str, authorization: Option<&str>) -> Option<String> {
   let routes = routes();
   if !routes.active() || authorization.is_some() || !method.eq_ignore_ascii_case("POST") {
      return None;
   }
   let rest = api_rest(url, &routes)?;
   if !rest.starts_with("/auth/") {
      return None;
   }
   let key = routes.login_key();
   (key != "discord" && routes.server(&key).is_some()).then_some(key)
}

/// The account on screen.
pub fn viewing() -> Option<Identity> {
   viewer()
}

/// The legacy single server from `endpoints.json`, used for accounts no
/// server claims.
pub fn legacy() -> Option<&'static Server> {
   LEGACY.get()
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

pub struct Gateway {
   pub url: String,
   /// Spacebar servers get payloads trimmed to their schemas.
   pub spacebar: bool,
   pub activities: bool,
}

/// The gateway the account behind `token` connects to, or `None` when no
/// known server holds it.
pub fn gateway_for(token: &str) -> Option<Gateway> {
   match identity(token) {
      Identity::Discord => Some(Gateway {
         url: DISCORD_GATEWAY.to_owned(),
         spacebar: false,
         activities: true,
      }),
      Identity::Jwt(id) => {
         let routes = routes();
         let key = routes
            .accounts
            .get(&id)
            .filter(|key| routes.servers.contains_key(*key));
         let server = key
            .and_then(|key| routes.servers.get(key))
            .or_else(|| LEGACY.get())?;
         Some(Gateway {
            url: server.gateway.clone(),
            spacebar: true,
            activities: key.is_none_or(|key| routes.shows_activity(key)),
         })
      },
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
