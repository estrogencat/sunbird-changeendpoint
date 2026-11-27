//! `launcher.conf` in Sunbird's home: `key=value` lines that the launchers
//! read too (`crates/inject`, `scripts/sunbird-{linux,macos}.sh`), so it stays
//! plain enough for `/bin/sh` and needs no parser crate there.

use std::{
   path::PathBuf,
   sync::{
      Mutex,
      PoisonError,
   },
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LauncherConfig {
   /// The first-launch modal was answered or dismissed.
   pub onboarded:   bool,
   /// The OS starts Sunbird's launcher at login.
   pub auto_start:  bool,
   /// The launcher downloads a newer loader before each start.
   pub auto_update: bool,
}

impl Default for LauncherConfig {
   fn default() -> Self {
      Self {
         onboarded:   false,
         auto_start:  true,
         auto_update: true,
      }
   }
}

impl LauncherConfig {
   /// Reads `text`, keeping the default for every key it lacks or can't read.
   #[must_use]
   pub fn parse(text: &str) -> Self {
      let mut config = Self::default();
      for line in text.lines() {
         let Some((key, value)) = line.split_once('=') else {
            continue;
         };
         let value = match value.trim() {
            "true" => true,
            "false" => false,
            _ => continue,
         };
         match key.trim() {
            "onboarded" => config.onboarded = value,
            "auto_start" => config.auto_start = value,
            "auto_update" => config.auto_update = value,
            _ => {},
         }
      }
      config
   }

   #[must_use]
   pub fn render(self) -> String {
      format!(
         "onboarded={}\nauto_start={}\nauto_update={}\n",
         self.onboarded, self.auto_start, self.auto_update
      )
   }
}

fn path() -> Option<PathBuf> {
   phoenix_runtime::state().map(|state| state.home.join("launcher.conf"))
}

/// The file as last read or written, since the settings page reads it every
/// frame.
static CURRENT: Mutex<Option<LauncherConfig>> = Mutex::new(None);

/// The saved config, or the defaults when there is none yet.
pub fn load() -> LauncherConfig {
   *CURRENT
      .lock()
      .unwrap_or_else(PoisonError::into_inner)
      .get_or_insert_with(|| {
         path()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .map_or_else(LauncherConfig::default, |text| LauncherConfig::parse(&text))
      })
}

/// Changes the config [`load`] returns from now on, without saving it.
pub fn update(change: impl FnOnce(&mut LauncherConfig)) -> LauncherConfig {
   let mut config = load();
   change(&mut config);
   *CURRENT.lock().unwrap_or_else(PoisonError::into_inner) = Some(config);
   config
}

/// Writes `config` and adds or removes the OS login entry to match it.
pub fn save(config: LauncherConfig) -> Result<(), String> {
   *CURRENT.lock().unwrap_or_else(PoisonError::into_inner) = Some(config);
   let path = path().ok_or("the runtime has not started")?;
   std::fs::write(&path, config.render()).map_err(|err| format!("{}: {err}", path.display()))?;
   phoenix_ui_desktop::set_autostart(config.auto_start).map_err(|err| err.to_string())
}

/// Saves `config`, logging and toasting what failed.
pub fn save_or_report(config: LauncherConfig) {
   if let Err(err) = save(config) {
      log::warn!("launcher.conf: {err}");
      phoenix_ui_desktop::toast(&format!("Could not save Sunbird's startup settings: {err}"));
   }
}

#[cfg(test)]
mod tests {
   use super::LauncherConfig;

   #[test]
   fn missing_file_means_defaults() {
      assert_eq!(LauncherConfig::parse(""), LauncherConfig::default());
      let defaults = LauncherConfig::default();
      assert!(!defaults.onboarded && defaults.auto_start && defaults.auto_update);
   }

   #[test]
   fn round_trips_and_skips_junk() {
      let config = LauncherConfig {
         onboarded:   true,
         auto_start:  false,
         auto_update: false,
      };
      assert_eq!(LauncherConfig::parse(&config.render()), config);
      let parsed = LauncherConfig::parse(
         "# comment\r\nauto_update = false\r\nonboarded=yes\nother=true\nauto_start\n",
      );
      assert_eq!(parsed, LauncherConfig {
         auto_update: false,
         ..LauncherConfig::default()
      });
   }
}
