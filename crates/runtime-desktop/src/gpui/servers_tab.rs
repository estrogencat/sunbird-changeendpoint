//! The Servers page: which server each account and new logins go to, the
//! custom servers beside the built-in ones, and the Spacebar tweaks.

use std::sync::PoisonError;

use super::{Action, BtnSize, BtnVariant, Builder, RawDiv, UI_STATE};
use crate::{
   discover,
   modal::{FieldKind, SettingField},
   servers::{self, Identity, Server, Tweaks},
};

const FIELDS: [(&str, &str); 4] = [
   ("name", "Name"),
   ("api", "API"),
   ("gateway", "Gateway"),
   ("cdn", "CDN"),
];

#[derive(Clone)]
pub enum Press {
   Login(String),
   Expand(String),
   Remove(String),
   Unlink(u64),
   Add,
   Edit { key: String, field: &'static str },
}

#[derive(Clone, Copy)]
pub enum Tweak {
   Boosts,
   Verified,
   Announce,
   UploadLimit,
}

impl Tweak {
   const ALL: [Self; 4] = [Self::Boosts, Self::Verified, Self::Announce, Self::UploadLimit];

   const fn text(self) -> (&'static str, &'static str, &'static str) {
      match self {
         Self::Boosts => (
            "boosts",
            "Unlock Server Boost Perks",
            "Shows every server at boost level 3, so you can use animated icons, banners, role icons and more emoji. Spacebar has no boosting, but most servers allow these anyway.",
         ),
         Self::Verified => (
            "verified",
            "Skip Email Verification",
            "Treats your account as verified, so servers that need a verified email let you chat. Useful on servers that can't send emails.",
         ),
         Self::Announce => (
            "announce",
            "Show When You Come Online",
            "Lets friends and server members see you come online. Spacebar only tells them when you connect as offline, so Sunbird does that and the server switches you to online. Invisible stays invisible.",
         ),
         Self::UploadLimit => (
            "upload-limit",
            "Use the Server's Upload Limit",
            "Lets you upload files as big as the server allows, instead of Discord's 10 MB without Nitro.",
         ),
      }
   }

   const fn get(self, tweaks: Tweaks) -> bool {
      match self {
         Self::Boosts => tweaks.boosts,
         Self::Verified => tweaks.verified,
         Self::Announce => tweaks.announce,
         Self::UploadLimit => tweaks.upload_limit,
      }
   }

   const fn set(self, tweaks: &mut Tweaks, on: bool) {
      match self {
         Self::Boosts => tweaks.boosts = on,
         Self::Verified => tweaks.verified = on,
         Self::Announce => tweaks.announce = on,
         Self::UploadLimit => tweaks.upload_limit = on,
      }
   }
}

pub fn tweak(tweak: Tweak, on: bool) {
   report(servers::update(|config| tweak.set(&mut config.tweaks, on)));
   if !matches!(tweak, Tweak::UploadLimit) {
      phoenix_ui_desktop::toast("Restart Discord to apply");
   }
}

fn tweaks(b: &Builder, root: RawDiv, routes: &servers::Routes) -> RawDiv {
   let root = b.push_div(
      root,
      b.text_block(
         "Spacebar Tweaks",
         "Changes to how Spacebar servers work in Discord. Discord accounts stay as they are.",
      ),
   );
   let mut card = b.card(8.0, 14.0, 8.0, 0.18);
   for (index, tweak) in Tweak::ALL.into_iter().enumerate() {
      if index > 0 {
         card = b.push_div(card, b.divider());
      }
      let (id, title, description) = tweak.text();
      card = b.push_div(
         card,
         b.switch_row(
            &format!("sunbird-servers-tweak-{id}"),
            title,
            Some(description),
            tweak.get(routes.tweaks()),
            Action::ServerTweak(tweak),
         ),
      );
   }
   b.push_div(root, card)
}

fn label(key: &str, server: &Server) -> String {
   server.name.clone().unwrap_or_else(|| key.to_owned())
}

fn host(url: &str) -> &str {
   let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
   rest.split('/').next().unwrap_or(rest)
}

fn report(result: std::io::Result<()>) {
   if let Err(err) = result {
      log::warn!("servers: could not save servers.json: {err}");
      phoenix_ui_desktop::toast(&format!("Could not save: {err}"));
   }
}

pub fn activity(key: String, on: bool) {
   report(servers::update(|config| {
      if on == servers::activity_default(&key) {
         config.activities.remove(&key);
      } else {
         config.activities.insert(key, on);
      }
   }));
   phoenix_ui_desktop::toast("Restart Discord to apply");
}

fn activity_row(b: &Builder, key: &str, on: bool) -> RawDiv {
   b.switch_row(
      &format!("sunbird-servers-activity-{key}"),
      "Show Activity",
      Some("Shares your custom status and what you're playing. Turn it off if this server keeps disconnecting you."),
      on,
      Action::ServerActivity(key.to_owned()),
   )
}

pub fn press(press: Press) {
   match press {
      Press::Login(key) => report(servers::update(|config| config.login = Some(key))),
      Press::Expand(key) => {
         let mut ui = UI_STATE.lock().unwrap_or_else(PoisonError::into_inner);
         ui.servers_open = if ui.servers_open.as_deref() == Some(key.as_str()) {
            None
         } else {
            Some(key)
         };
         ui.servers_confirm = None;
      },
      Press::Remove(key) => {
         let mut ui = UI_STATE.lock().unwrap_or_else(PoisonError::into_inner);
         if ui.servers_confirm.as_deref() != Some(key.as_str()) {
            ui.servers_confirm = Some(key);
            return;
         }
         ui.servers_confirm = None;
         ui.servers_open = None;
         drop(ui);
         report(servers::update(|config| {
            config.custom.remove(&key);
            config.accounts.retain(|_, linked| *linked != key);
            if config.login.as_deref() == Some(key.as_str()) {
               config.login = None;
            }
         }));
      },
      Press::Unlink(id) => {
         report(servers::update(|config| {
            config.accounts.remove(&id.to_string());
         }));
      },
      Press::Add => {
         crate::modal::open_setting_editor(SettingField {
            plugin: "Servers".to_owned(),
            key: "add".to_owned(),
            title: "Add a Server".to_owned(),
            initial: String::new(),
            kind: FieldKind::Server,
            then: None,
         });
      },
      Press::Edit { key, field } => {
         let routes = servers::routes();
         let Some(server) = routes.server(&key) else {
            return;
         };
         let initial = match field {
            "name" => label(&key, server),
            "api" => server.api.clone(),
            "gateway" => server.gateway.clone(),
            _ => server.cdn.clone().unwrap_or_default(),
         };
         let title = FIELDS
            .iter()
            .find(|(name, _)| *name == field)
            .map_or(field, |(_, title)| title);
         crate::modal::open_setting_editor(SettingField {
            plugin: "Servers".to_owned(),
            key: format!("{key}/{field}"),
            title: format!("{} {title}", label(&key, server)),
            initial,
            kind: FieldKind::Server,
            then: None,
         });
      },
   }
}

/// Finds the server at `address` and adds it, named after what it calls
/// itself. Runs off the UI thread, it waits on the network.
pub fn add_server(address: &str) -> Result<String, String> {
   let found = discover::discover(address)?;
   let host = discover::host_of(address).to_ascii_lowercase();
   let name = found.name.unwrap_or_else(|| host.clone());
   let slug: String = host
      .chars()
      .map(|char| {
         if char.is_ascii_alphanumeric() {
            char
         } else {
            '-'
         }
      })
      .collect();
   servers::update(|config| {
      let mut key = format!("custom-{slug}");
      let mut n = 2;
      while config.custom.contains_key(&key) || servers::is_builtin(&key) {
         key = format!("custom-{slug}-{n}");
         n += 1;
      }
      config.custom.insert(
         key,
         Server {
            name: Some(name.clone()),
            ..found.server
         },
      );
   })
   .map_err(|err| format!("Could not save: {err}"))?;
   Ok(name)
}

/// Saves one field of a custom server, `spec` being `key/field`.
pub fn edit_server(spec: &str, text: &str) -> Result<(), String> {
   let (key, field) = spec.rsplit_once('/').ok_or("Unknown field")?;
   let text = text.trim().trim_end_matches('/');
   let web = text.starts_with("https://") || text.starts_with("http://");
   let socket = text.starts_with("wss://") || text.starts_with("ws://");
   match field {
      "name" if text.is_empty() => return Err("Enter a name".to_owned()),
      "api" if !web => return Err("The API starts with https://".to_owned()),
      "gateway" if !socket => return Err("The gateway starts with wss://".to_owned()),
      "cdn" if !text.is_empty() && !web => return Err("The CDN starts with https://".to_owned()),
      _ => {},
   }
   let mut missing = false;
   servers::update(|config| {
      let Some(server) = config.custom.get_mut(key) else {
         missing = true;
         return;
      };
      let value = text.to_owned();
      match field {
         "name" => server.name = Some(value),
         "api" => server.api = value,
         "gateway" => server.gateway = value,
         _ => server.cdn = (!value.is_empty()).then_some(value),
      }
   })
   .map_err(|err| format!("Could not save: {err}"))?;
   if missing {
      return Err("Only custom servers can be edited".to_owned());
   }
   Ok(())
}

fn status(b: &Builder, routes: &servers::Routes) -> RawDiv {
   let (title, subtitle) = match servers::viewing() {
      Some(Identity::Jwt(id)) => {
         let found = routes
            .linked()
            .iter()
            .find(|(user, _)| *user == id)
            .and_then(|(_, key)| Some((key.as_str(), routes.server(key)?)));
         match (found, servers::legacy()) {
            (Some((key, server)), _) => (
               format!("Connected to {}", label(key, server)),
               format!("Account {id} · {}", host(&server.api)),
            ),
            (None, Some(legacy)) => (
               "Connected to the endpoints.json server".to_owned(),
               format!("Account {id} · {}", host(&legacy.api)),
            ),
            (None, None) => (
               "Connected to an unknown server".to_owned(),
               format!("Account {id} is linked to no server"),
            ),
         }
      },
      Some(Identity::Discord) => ("Connected to Discord".to_owned(), "discord.com".to_owned()),
      _ => (
         "Not connected yet".to_owned(),
         "Open a channel to see which account is on screen".to_owned(),
      ),
   };
   let card = b.card(10.0, 16.0, 8.0, 0.20);
   b.push_div(card, b.text_block(&title, &subtitle))
}

fn logins(b: &Builder, root: RawDiv, routes: &servers::Routes) -> RawDiv {
   let mut root = b.push_div(
      root,
      b.text_block(
         "New Logins",
         "Where Add Account and logging in go. Accounts already added stay on their own server.",
      ),
   );
   let (choices, chosen) = routes.login_choices();
   let mut row = b.h_row(8.0, false);
   for (value, name) in choices {
      let variant = if value == chosen {
         BtnVariant::Primary
      } else {
         BtnVariant::Secondary
      };
      row = b.push_button(
         row,
         &format!("sunbird-servers-login-{value}"),
         &name,
         variant,
         BtnSize::Small,
         Action::Servers(Press::Login(value)),
      );
   }
   root = b.push_div(root, row);
   root
}

fn server_card(
   b: &Builder,
   key: &str,
   server: &Server,
   activity: bool,
   open: bool,
   confirm: bool,
) -> RawDiv {
   let mut card = b.card(8.0, 14.0, 8.0, 0.18);
   let name = label(key, server);
   if servers::is_builtin(key) {
      card = b.push_div(
         card,
         b.text_block(&name, &format!("{} · built in", host(&server.api))),
      );
      card = b.push_div(card, b.divider());
      return b.push_div(card, activity_row(b, key, activity));
   }
   card = b.push_div(
      card,
      b.action_row(
         &format!("sunbird-servers-open-{key}"),
         &name,
         host(&server.api),
         if open { "Done" } else { "Edit" },
         BtnVariant::Secondary,
         BtnSize::Small,
         Action::Servers(Press::Expand(key.to_owned())),
      ),
   );
   card = b.push_div(card, b.divider());
   card = b.push_div(card, activity_row(b, key, activity));
   if !open {
      return card;
   }
   card = b.push_div(card, b.divider());
   for (field, title) in FIELDS {
      let value = match field {
         "name" => name.clone(),
         "api" => server.api.clone(),
         "gateway" => server.gateway.clone(),
         _ => server.cdn.clone().unwrap_or_else(|| "not set".to_owned()),
      };
      card = b.push_div(
         card,
         b.action_row(
            &format!("sunbird-servers-{key}-{field}"),
            title,
            &value,
            "Edit",
            BtnVariant::Secondary,
            BtnSize::Small,
            Action::Servers(Press::Edit {
               key: key.to_owned(),
               field,
            }),
         ),
      );
   }
   b.push_div(
      card,
      b.action_row(
         &format!("sunbird-servers-remove-{key}"),
         "Remove Server",
         "Accounts linked to it are unlinked.",
         if confirm { "Confirm Remove" } else { "Remove" },
         BtnVariant::Critical,
         BtnSize::Small,
         Action::Servers(Press::Remove(key.to_owned())),
      ),
   )
}

fn accounts(b: &Builder, mut root: RawDiv, routes: &servers::Routes) -> RawDiv {
   root = b.push_div(
      root,
      b.text_block(
         "Accounts",
         "Accounts link themselves to the server they log in to. An unlinked Spacebar account uses the endpoints.json server.",
      ),
   );
   if routes.linked().is_empty() {
      return b.push_div(
         root,
         b.text_block("No linked accounts", "Log in to an account to link it."),
      );
   }
   let mut list = b.card(8.0, 14.0, 8.0, 0.18);
   for (index, (id, key)) in routes.linked().iter().enumerate() {
      if index > 0 {
         list = b.push_div(list, b.divider());
      }
      let server = routes
         .server(key)
         .map_or_else(|| key.clone(), |server| label(key, server));
      list = b.push_div(
         list,
         b.action_row(
            &format!("sunbird-servers-account-{id}"),
            &format!("Account {id}"),
            &server,
            "Unlink",
            BtnVariant::Secondary,
            BtnSize::Small,
            Action::Servers(Press::Unlink(*id)),
         ),
      );
   }
   b.push_div(root, list)
}

pub fn render(b: &Builder, mut root: RawDiv) -> RawDiv {
   let routes = servers::routes();
   let (open, confirm) = {
      let ui = UI_STATE.lock().unwrap_or_else(PoisonError::into_inner);
      (ui.servers_open.clone(), ui.servers_confirm.clone())
   };
   root = b.push_div(root, status(b, &routes));
   root = b.push_div(root, b.divider());
   root = logins(b, root, &routes);
   root = b.push_div(root, b.divider());
   root = b.push_div(
      root,
      b.text_block(
         "Servers",
         "Built-in servers and your own. Changes apply to the next request, no restart needed.",
      ),
   );
   for (key, server) in routes.servers() {
      root = b.push_div(
         root,
         server_card(
            b,
            key,
            server,
            routes.shows_activity(key),
            open.as_deref() == Some(key),
            confirm.as_deref() == Some(key),
         ),
      );
   }
   root = b.push_div(
      root,
      b.action_row(
         "sunbird-servers-add",
         "Add a Server",
         "Enter its address, like rory.server.spacebar.chat. Its endpoints are found for you.",
         "Add Server",
         BtnVariant::Primary,
         BtnSize::Small,
         Action::Servers(Press::Add),
      ),
   );
   root = b.push_div(root, b.divider());
   root = tweaks(b, root, &routes);
   root = b.push_div(root, b.divider());
   accounts(b, root, &routes)
}
