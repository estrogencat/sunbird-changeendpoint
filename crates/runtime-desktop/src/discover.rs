//! Finds a Spacebar server's endpoints from its address. Servers publish them
//! in `.well-known` documents and `/policies/instance/domains`, with fields
//! that differ between versions, so every API candidate is pinged.

use phoenix_runtime::fetch::Request;
use serde::de::DeserializeOwned;

use crate::servers::Server;

pub struct Discovered {
   pub name: Option<String>,
   pub server: Server,
}

#[derive(serde::Deserialize)]
struct Base {
   #[serde(rename = "baseUrl")]
   base_url: Option<String>,
}

#[derive(serde::Deserialize)]
struct Client {
   api: Option<Base>,
   cdn: Option<Base>,
   gateway: Option<Base>,
}

#[derive(serde::Deserialize)]
struct WellKnown {
   api: Option<String>,
}

#[derive(serde::Deserialize)]
struct Domains {
   api: Option<String>,
   #[serde(rename = "apiEndpoint")]
   api_endpoint: Option<String>,
   cdn: Option<String>,
   gateway: Option<String>,
}

#[derive(serde::Deserialize)]
struct Instance {
   name: Option<String>,
}

#[derive(serde::Deserialize)]
struct Ping {
   ping: Option<String>,
   instance: Option<Instance>,
}

fn get<T: DeserializeOwned>(url: &str) -> Option<T> {
   let response = crate::fetch::send(&Request {
      url: url.to_owned(),
      method: "GET".to_owned(),
      headers: vec![("Accept".to_owned(), "application/json".to_owned())],
      body: None,
   })
   .ok()?;
   if !(200..300).contains(&response.status) {
      return None;
   }
   serde_json::from_slice(&response.body).ok()
}

/// The bare host of an address, without scheme or path.
pub fn host_of(address: &str) -> &str {
   let address = address.trim();
   let rest = address.split_once("://").map_or(address, |(_, rest)| rest);
   rest.split('/').next().unwrap_or(rest)
}

/// A lowercase origin without a default port, trailing slash or API version.
fn normalize(url: Option<&str>) -> Option<String> {
   let url = url?.trim();
   if url.is_empty() {
      return None;
   }
   let (scheme, rest) = url.split_once("://")?;
   let scheme = scheme.to_ascii_lowercase();
   let split = rest.find('/').unwrap_or(rest.len());
   let (authority, path) = rest.split_at(split);
   let mut authority = authority.to_ascii_lowercase();
   if matches!(scheme.as_str(), "https" | "wss") && authority.ends_with(":443") {
      authority.truncate(authority.len() - 4);
   }
   let mut path = path.trim_end_matches('/');
   if let Some((head, version)) = path.rsplit_once('/')
      && version.len() > 1
      && version.starts_with(['v', 'V'])
      && version[1..].bytes().all(|byte| byte.is_ascii_digit())
   {
      path = head;
   }
   Some(format!("{scheme}://{authority}{path}"))
}

pub fn discover(address: &str) -> Result<Discovered, String> {
   let host = host_of(address);
   if host.is_empty() {
      return Err("Enter a server address.".to_owned());
   }
   let secure = !address.trim().to_ascii_lowercase().starts_with("http://");
   let http = if secure { "https" } else { "http" };

   let client = get::<Client>(&format!("{http}://{host}/.well-known/spacebar/client"));
   let known = if client.is_none() {
      get::<WellKnown>(&format!("{http}://{host}/.well-known/spacebar"))
   } else {
      None
   };
   let base = |pick: fn(&Client) -> Option<&Base>| {
      client
         .as_ref()
         .and_then(pick)
         .and_then(|base| normalize(base.base_url.as_deref()))
   };
   let api_seeds: Vec<String> = [
      base(|client| client.api.as_ref()),
      known
         .as_ref()
         .and_then(|known| normalize(known.api.as_deref())),
      Some(format!("{http}://{host}/api")),
      Some(format!("{http}://api.{host}/api")),
   ]
   .into_iter()
   .flatten()
   .flat_map(|seed| {
      if seed.ends_with("/api") {
         vec![seed]
      } else {
         vec![format!("{seed}/api"), seed]
      }
   })
   .collect();

   let domains = api_seeds
      .iter()
      .find_map(|seed| get::<Domains>(&format!("{seed}/policies/instance/domains")));
   let endpoint = domains
      .as_ref()
      .and_then(|domains| normalize(domains.api_endpoint.as_deref()));
   let mut candidates: Vec<String> = Vec::new();
   for candidate in [
      domains
         .as_ref()
         .and_then(|domains| normalize(domains.api.as_deref())),
      endpoint.clone(),
      endpoint.map(|endpoint| format!("{endpoint}/api")),
   ]
   .into_iter()
   .flatten()
   .chain(api_seeds)
   {
      if !candidates.contains(&candidate) {
         candidates.push(candidate);
      }
   }

   for api in candidates {
      let Some(ping) = get::<Ping>(&format!("{api}/v9/ping")) else {
         continue;
      };
      if ping.ping.as_deref() != Some("pong!") {
         continue;
      }
      let name = ping
         .instance
         .and_then(|instance| instance.name)
         .map(|name| name.trim().to_owned())
         .filter(|name| !name.is_empty());
      let cdn = base(|client| client.cdn.as_ref())
         .or_else(|| {
            domains
               .as_ref()
               .and_then(|domains| normalize(domains.cdn.as_deref()))
         })
         .unwrap_or_else(|| format!("{http}://cdn.{host}"));
      let gateway = base(|client| client.gateway.as_ref())
         .or_else(|| {
            domains
               .as_ref()
               .and_then(|domains| normalize(domains.gateway.as_deref()))
         })
         .unwrap_or_else(|| format!("{}://gateway.{host}", if secure { "wss" } else { "ws" }));
      return Ok(Discovered {
         name,
         server: Server {
            name: None,
            api: format!("{api}/v9"),
            gateway,
            origin: Some(format!("{http}://{host}")),
            cdn: Some(cdn),
            media: None,
         },
      });
   }
   Err(format!("Couldn't find a Spacebar server at {host}."))
}

#[cfg(test)]
mod tests {
   use super::normalize;

   #[test]
   fn normalizes_origins() {
      assert_eq!(
         normalize(Some("HTTPS://Api.Example.com:443/api/v9/")).as_deref(),
         Some("https://api.example.com/api")
      );
      assert_eq!(
         normalize(Some("wss://gateway.example.com")).as_deref(),
         Some("wss://gateway.example.com")
      );
   }
}
