//! What each patch [`crate::prepare`] installed came to. A critical patch
//! failing means Sunbird can't work on this Discord build (most likely Discord
//! updated first), which [`crate::recovery`] offers to fix. A feature patch
//! failing only turns that feature off.

use std::sync::{
   Mutex,
   PoisonError,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
   Critical,
   Feature,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct Patch {
   pub name:  &'static str,
   pub kind:  Kind,
   #[serde(skip_serializing_if = "Option::is_none")]
   pub error: Option<String>,
}

/// Every recorded patch, in install order. A later record of the same name
/// replaces the earlier one.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct PatchHealth {
   pub patches: Vec<Patch>,
}

impl PatchHealth {
   fn record(&mut self, name: &'static str, kind: Kind, outcome: Result<(), String>) {
      let patch = Patch {
         name,
         kind,
         error: outcome.err(),
      };
      match self.patches.iter_mut().find(|known| known.name == name) {
         Some(known) => *known = patch,
         None => self.patches.push(patch),
      }
   }

   fn failed(&self, kind: Kind) -> impl Iterator<Item = &Patch> {
      self
         .patches
         .iter()
         .filter(move |patch| patch.kind == kind && patch.error.is_some())
   }

   #[must_use]
   pub fn critical_failures(&self) -> Vec<Patch> {
      self.failed(Kind::Critical).cloned().collect()
   }

   #[must_use]
   pub fn feature_failures(&self) -> Vec<Patch> {
      self.failed(Kind::Feature).cloned().collect()
   }
}

static HEALTH: Mutex<PatchHealth> = Mutex::new(PatchHealth {
   patches: Vec::new(),
});

fn health() -> std::sync::MutexGuard<'static, PatchHealth> {
   HEALTH.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `Ok` when `ok`, else `error`, for patches whose outcome is a check.
pub fn require(ok: bool, error: &str) -> Result<(), String> {
   if ok { Ok(()) } else { Err(error.to_owned()) }
}

pub fn critical(name: &'static str, outcome: Result<(), String>) {
   record(name, Kind::Critical, outcome);
}

pub fn record(name: &'static str, kind: Kind, outcome: Result<(), String>) {
   if let Err(err) = &outcome {
      log::warn!("patch health: {name} ({kind:?}) failed: {err}");
   }
   health().record(name, kind, outcome);
}

/// Runs a feature patch's installer and records it as failed when it logged
/// a warning or error.
// ponytail: counts this thread's warnings, so an installer that warns about a
// harmless fallback reads as failed and one that fails silently reads as ok.
// Have installers return their outcome when that matters.
pub fn feature(name: &'static str, install: impl FnOnce()) {
   let before = phoenix_runtime::console::problems();
   install();
   let logged = phoenix_runtime::console::problems().wrapping_sub(before);
   let outcome = if logged == 0 {
      Ok(())
   } else {
      Err(format!("{logged} warning(s) while installing, see the log"))
   };
   record(name, Kind::Feature, outcome);
}

#[must_use]
pub fn snapshot() -> PatchHealth {
   health().clone()
}

/// Logs one line naming what failed, once [`crate::prepare`] is done.
pub fn log_summary() {
   let health = snapshot();
   let names = |patches: Vec<Patch>| {
      patches
         .iter()
         .map(|patch| patch.name)
         .collect::<Vec<_>>()
         .join(", ")
   };
   let (critical, feature) = (health.critical_failures(), health.feature_failures());
   if critical.is_empty() && feature.is_empty() {
      log::info!("patch health: all {} patches ok", health.patches.len());
   } else {
      log::warn!(
         "patch health: critical failed [{}], features failed [{}]",
         names(critical),
         names(feature)
      );
   }
}

#[cfg(test)]
mod tests {
   use super::{
      Kind,
      PatchHealth,
   };

   #[test]
   fn sorts_failures_by_kind() {
      let mut health = PatchHealth::default();
      health.record("access_gate", Kind::Critical, Ok(()));
      health.record("gateway", Kind::Critical, Err("no registry".to_owned()));
      health.record("badges", Kind::Feature, Err("moved".to_owned()));
      health.record("emoji", Kind::Feature, Ok(()));
      let names = |patches: Vec<super::Patch>| patches.iter().map(|p| p.name).collect::<Vec<_>>();
      assert_eq!(names(health.critical_failures()), ["gateway"]);
      assert_eq!(names(health.feature_failures()), ["badges"]);
   }

   #[test]
   fn a_later_record_replaces_the_earlier_one() {
      let mut health = PatchHealth::default();
      health.record("modal", Kind::Critical, Ok(()));
      health.record(
         "modal",
         Kind::Critical,
         Err("hook installed but did not render".to_owned()),
      );
      assert_eq!(health.patches.len(), 1);
      assert_eq!(health.critical_failures().len(), 1);
   }
}
