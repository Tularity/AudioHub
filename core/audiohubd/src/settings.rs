//! Daemon-owned settings, persisted at `<config_dir>/settings.json`
//! (spec-m5b §6.1).
//!
//! The consumer mode used to live in the UI's localStorage, which made it a
//! per-BROWSER-PROFILE opinion about a machine-wide fact: two UI windows could
//! disagree, a CLI knew nothing about it, and the daemon — the only process
//! that can actually publish or remove virtual devices — was never told. It is
//! daemon state now, and the UI's copy is a cache of what this file says.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use audiohub_ipc::{MODE_A, MODE_B};

/// Exactly the fields this daemon owns. `effective_mode`, `hal_capacity` and
/// `hal_used` are NOT here: they are derived at read time from what the driver
/// is actually doing, and persisting a derived value is how the two ends come
/// to disagree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct StoredSettings {
    pub version: u32,
    pub consumer_mode: String,
    pub remove_virtual_on_disconnect: bool,
    pub mark_offline_devices: bool,
    pub latency: String,
    pub quality: String,
}

impl Default for StoredSettings {
    fn default() -> Self {
        StoredSettings {
            version: 1,
            // Mode A is the default because it is the mode that works with no
            // driver installed: a fresh machine must never start in a mode
            // whose devices cannot exist.
            consumer_mode: MODE_A.to_string(),
            // plan §7.3 freezes this default: frequent device churn breaks the
            // system's and applications' remembered device selections.
            remove_virtual_on_disconnect: false,
            // spec-m5b OPEN QUESTION 1: without this, the commonest mode-B
            // failure (peer asleep -> default output silent) is invisible
            // everywhere except inside our own window.
            mark_offline_devices: true,
            latency: "min".to_string(),
            quality: "auto".to_string(),
        }
    }
}

impl StoredSettings {
    fn path(dir: &Path) -> PathBuf {
        dir.join("settings.json")
    }

    /// A missing or unparseable file is the DEFAULTS, never an error: settings
    /// are a convenience, and refusing to start a daemon over a corrupt
    /// preferences file would be a worse failure than losing the preference.
    pub(crate) fn load(dir: &Path) -> StoredSettings {
        let path = Self::path(dir);
        let Ok(bytes) = std::fs::read(&path) else {
            return StoredSettings::default();
        };
        match serde_json::from_slice::<StoredSettings>(&bytes) {
            Ok(s) => s.normalized(),
            Err(e) => {
                crate::dlog!(
                    "[audiohubd] {} is not readable ({e}); using default settings",
                    path.display()
                );
                StoredSettings::default()
            }
        }
    }

    /// An unknown mode string is mode A, not an error: whatever wrote it, the
    /// safe reading of "I do not understand this" is the mode that needs no
    /// driver.
    fn normalized(mut self) -> StoredSettings {
        if self.consumer_mode != MODE_A && self.consumer_mode != MODE_B {
            self.consumer_mode = MODE_A.to_string();
        }
        self
    }

    pub(crate) fn save(&self, dir: &Path) -> Result<()> {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        let path = Self::path(dir);
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?.as_bytes())
            .with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("rename to {}", path.display()))?;
        Ok(())
    }

    pub(crate) fn wants_mode_b(&self) -> bool {
        self.consumer_mode == MODE_B
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let p = std::env::temp_dir().join(format!("ahb-settings-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&p).expect("mkdir");
        p
    }

    #[test]
    fn defaults_are_the_no_driver_safe_ones() {
        let d = StoredSettings::default();
        assert_eq!(d.consumer_mode, MODE_A, "a fresh machine must not start in a mode whose devices cannot exist");
        assert!(!d.remove_virtual_on_disconnect, "plan §7.3 freezes 'keep'");
        assert!(d.mark_offline_devices);
    }

    #[test]
    fn settings_round_trip_through_the_file() {
        let dir = tmp("roundtrip");
        assert_eq!(StoredSettings::load(&dir), StoredSettings::default());
        let want = StoredSettings {
            consumer_mode: MODE_B.to_string(),
            remove_virtual_on_disconnect: true,
            mark_offline_devices: false,
            ..StoredSettings::default()
        };
        want.save(&dir).expect("save");
        assert_eq!(StoredSettings::load(&dir), want);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_corrupt_file_reads_as_defaults_rather_than_failing_startup() {
        let dir = tmp("corrupt");
        std::fs::write(dir.join("settings.json"), b"{not json").expect("write");
        assert_eq!(StoredSettings::load(&dir), StoredSettings::default());
        // ...and so does a file whose mode is a string nobody defined: the safe
        // reading of an unknown mode is the one that needs no driver.
        std::fs::write(
            dir.join("settings.json"),
            br#"{"version":1,"consumer_mode":"c","remove_virtual_on_disconnect":false,
                 "mark_offline_devices":true,"latency":"min","quality":"auto"}"#,
        )
        .expect("write");
        assert_eq!(StoredSettings::load(&dir).consumer_mode, MODE_A);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
