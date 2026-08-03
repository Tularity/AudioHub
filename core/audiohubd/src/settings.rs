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

use audiohub_ipc::Mode;

/// Bumped to 2 by plan §13: `consumer_mode` became `mode` and gained `share`.
///
/// Nothing migrates. A v1 file simply fails to provide `mode`, so the whole
/// record falls back to [`StoredSettings::default`] — which is exactly the
/// intent, because the pre-§13 value cannot be translated: every v1 machine was
/// a provider AND a consumer at once, so both `"a"` and `"b"` are half of the
/// answer and neither is the whole one. Choosing for the user here would
/// silently decide which half of their setup keeps working.
pub(crate) const SETTINGS_VERSION: u32 = 2;

/// Exactly the fields this daemon owns. `effective_mode`, `hal_capacity` and
/// `hal_used` are NOT here: they are derived at read time from what the driver
/// is actually doing, and persisting a derived value is how the two ends come
/// to disagree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct StoredSettings {
    pub version: u32,
    pub mode: Mode,
    pub remove_virtual_on_disconnect: bool,
    pub mark_offline_devices: bool,
    pub latency: String,
    pub quality: String,
}

impl Default for StoredSettings {
    fn default() -> Self {
        StoredSettings {
            version: SETTINGS_VERSION,
            // Share is the default (plan §13). Three reasons, in order of
            // weight:
            //
            //  1. It is the only mode that needs NOTHING — no driver, no
            //     system-audio TCC grant. The old comment here made that
            //     argument for mode A ("a fresh machine must never start in a
            //     mode whose devices cannot exist"); Share satisfies it
            //     strictly better, since mode A's own job needs a capture
            //     permission that a fresh machine has not granted either.
            //  2. It is what every pre-§13 machine already was. The provider
            //     side was unconditionally on for everyone; the consumer side
            //     was a choice most installs never exercised. Defaulting to
            //     Share removes a capability nobody had asked for rather than
            //     one they were using.
            //  3. Two fresh machines that both default to Share are inert but
            //     coherent: nothing happens until the user names a consumer.
            //     Two that both defaulted to a consumer mode would refuse each
            //     other, and every attempt would fail with the interface
            //     insisting both ends were fine.
            mode: Mode::Share,
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
                // Includes every pre-§13 file: `mode` is absent there, so serde
                // fails and the whole record becomes the defaults. Logged as a
                // plain reason rather than a migration notice, because it is
                // not one — see SETTINGS_VERSION for why nothing is carried
                // across.
                crate::dlog!(
                    "[audiohubd] {} is not readable ({e}); using default settings \
                     (mode={})",
                    path.display(),
                    Mode::Share
                );
                StoredSettings::default()
            }
        }
    }

    /// A file from the future — a `version` this build does not know — is read
    /// for whatever fields it does have, but its MODE is not trusted: a mode
    /// written by a newer build may have a meaning this one cannot honour, and
    /// the failure would be silent on exactly the axis §13 exists to police
    /// ("can this machine be used"). Falling back to the default is loud in the
    /// only way that matters — the user sees the mode they did not choose.
    fn normalized(mut self) -> StoredSettings {
        if self.version != SETTINGS_VERSION {
            crate::dlog!(
                "[audiohubd] settings.json is version {} (this build writes {SETTINGS_VERSION}); \
                 keeping the file's other fields but resetting mode to {}",
                self.version,
                Mode::Share
            );
            self.mode = Mode::Share;
            self.version = SETTINGS_VERSION;
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
    fn the_default_mode_is_the_one_that_needs_nothing_installed() {
        let d = StoredSettings::default();
        assert_eq!(
            d.mode,
            Mode::Share,
            "a fresh machine must start in the only mode that needs neither a driver nor a \
             capture permission"
        );
        assert!(
            d.mode.serves_peers() && !d.mode.consumes_peers(),
            "the default must be the provider side: it is what every pre-§13 machine already \
             was, and two fresh machines that both default to a consumer mode would refuse \
             each other"
        );
        assert!(!d.remove_virtual_on_disconnect, "plan §7.3 freezes 'keep'");
        assert!(d.mark_offline_devices);
    }

    #[test]
    fn settings_round_trip_through_the_file() {
        let dir = tmp("roundtrip");
        assert_eq!(StoredSettings::load(&dir), StoredSettings::default());
        let want = StoredSettings {
            mode: Mode::B,
            remove_virtual_on_disconnect: true,
            mark_offline_devices: false,
            ..StoredSettings::default()
        };
        want.save(&dir).expect("save");
        assert_eq!(StoredSettings::load(&dir), want);
        // ...and every mode survives the trip, not just the one above: a mode
        // that round-trips as some *other* mode is the failure this whole
        // change exists to prevent.
        for m in [Mode::Share, Mode::A, Mode::B] {
            let s = StoredSettings { mode: m, ..StoredSettings::default() };
            s.save(&dir).expect("save");
            assert_eq!(StoredSettings::load(&dir).mode, m, "{m} did not survive the file");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_corrupt_file_reads_as_defaults_rather_than_failing_startup() {
        let dir = tmp("corrupt");
        std::fs::write(dir.join("settings.json"), b"{not json").expect("write");
        assert_eq!(StoredSettings::load(&dir), StoredSettings::default());
        // A mode string nobody defined does NOT get guessed into a neighbour:
        // serde refuses the record and the whole file reads as the defaults.
        std::fs::write(
            dir.join("settings.json"),
            br#"{"version":2,"mode":"c","remove_virtual_on_disconnect":false,
                 "mark_offline_devices":true,"latency":"min","quality":"auto"}"#,
        )
        .expect("write");
        assert_eq!(StoredSettings::load(&dir), StoredSettings::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A settings.json written before plan §13 has `consumer_mode`, no `mode`,
    /// and version 1. It must land on the DEFAULT, not on a translation of the
    /// old value — see `SETTINGS_VERSION` for why translating is wrong.
    ///
    /// The assertion is deliberately on `Share` and not merely on "not `b`":
    /// the interesting failure is a future `#[serde(alias = "consumer_mode")]`
    /// added "to be helpful", which would resurrect `"b"` on a machine the user
    /// has to consciously re-choose.
    #[test]
    fn a_pre_s13_file_lands_on_the_default_rather_than_a_translation() {
        let dir = tmp("legacy");
        for old in ["a", "b"] {
            std::fs::write(
                dir.join("settings.json"),
                format!(
                    r#"{{"version":1,"consumer_mode":"{old}","remove_virtual_on_disconnect":true,
                       "mark_offline_devices":false,"latency":"min","quality":"auto"}}"#
                ),
            )
            .expect("write");
            let got = StoredSettings::load(&dir);
            assert_eq!(
                got.mode,
                Mode::Share,
                "consumer_mode={old} must not be carried into the new field"
            );
            assert_eq!(got, StoredSettings::default(), "the whole record resets, not just mode");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A file from a FUTURE build parses (the fields happen to line up) but its
    /// mode is not honoured: a newer build's mode may mean something this one
    /// cannot do, and "can this machine be used by others" is not a question to
    /// get silently wrong.
    #[test]
    fn a_newer_version_keeps_its_other_fields_but_not_its_mode() {
        let dir = tmp("future");
        std::fs::write(
            dir.join("settings.json"),
            br#"{"version":99,"mode":"b","remove_virtual_on_disconnect":true,
                 "mark_offline_devices":false,"latency":"min","quality":"auto"}"#,
        )
        .expect("write");
        let got = StoredSettings::load(&dir);
        assert_eq!(got.mode, Mode::Share, "a mode from an unknown version is not trusted");
        assert!(got.remove_virtual_on_disconnect, "...but the ordinary preferences are kept");
        assert!(!got.mark_offline_devices);
        assert_eq!(got.version, SETTINGS_VERSION, "and it is rewritten as ours");
        let _ = std::fs::remove_dir_all(&dir);
    }

}
