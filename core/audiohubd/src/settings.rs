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

use audiohub_ipc::{LatencyTarget, Mode, QualityTarget};

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
    /// 端到端总延迟的目标：`"auto"` 或档位表里的十进制毫秒数。
    ///
    /// 仍然是 `String` 而不是枚举，理由只有一条：**盘上的旧值不能让整条记录
    /// 失效**。`StoredSettings` 是 `#[derive(Deserialize)]` 的一个整体，任何
    /// 字段解析失败 ⇒ 整个文件回落到默认值（见 `load`）。把它换成枚举之后，
    /// 一个来自旧版本或未来版本的档位字符串会把用户的**模式**也一起重置——
    /// 而模式是 §13 那条互斥线，重置它比丢一个档位严重得多。
    ///
    /// 所以：字段松，取值紧。写入口 (`settings.set`) 用
    /// [`LatencyTarget::parse`] 严格校验并拒绝未知值，读出口用
    /// [`StoredSettings::latency_target`] 兜底，两头都不给「静默变成别的档」
    /// 留缝。
    pub latency: String,
    /// 质量档：`"auto"` 或某个**可用**档位的 id。同上，字段松取值紧。
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
            // **AUTO，不是「最低」**，尽管 plan §5 写的是「默认建议延迟固定取
            // 最低」。理由是行为守恒，不是偏好：
            //
            // 这两个字段在本轮之前**根本没人读**，所以每一台机器实际跑的都是
            // AUTO 那条路（抖动 p95 驱动 `update_target`，包络是
            // `JbTuning::DEFAULT` 那套实测整定：min_target=4 ⇒ JB 50 ms、
            // 欠载 0.18 次/min）。把默认写成「最低」会在升级的那一刻，
            // 对**每一台从没动过这个设置的机器**把 JB 削到 1 帧 —— 按 `JbTuning`
            // 文档里那张实测表，欠载会涨到 3.75 次/min，20 倍。
            //
            // 一个用户没要求过、也不会被告知的听感变化，不该由「把默认值抄成
            // 文档里那句话」引入。想要最低延迟的人现在有一个真的能用的 `0` 档。
            latency: audiohub_ipc::LATENCY_AUTO.to_string(),
            quality: audiohub_ipc::QUALITY_AUTO.to_string(),
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
        // 盘上的 `"min"` 是**行为守恒**地迁移到 AUTO 的，不是翻译成 `0`。
        //
        // `"min"` 只可能由本轮之前的 build 写下，而在那些 build 里这个字段
        // 被读过零次 —— 那台机器实际跑的是 AUTO 那条路。把它读成 `TotalMs(0)`
        // 会在升级的一瞬间改变一台没人动过的机器的听感（JB 50 ms -> 10 ms）。
        //
        // 「`"min"` 的语义等于 `0`」这件事本身是对的，`LatencyTarget::parse`
        // 照旧那么翻 —— 那条路服务的是**明确写来**的请求（老客户端的
        // `settings.set`），与「从盘上读到一个从未生效过的值」是两回事。
        if self.latency == audiohub_ipc::LATENCY_LEGACY_MIN {
            crate::dlog!(
                "[audiohubd] settings.json 的 latency=\"{}\" 出自一个从不读它的版本；\
                 按该机器一直以来的实际行为迁移到 \"{}\"",
                audiohub_ipc::LATENCY_LEGACY_MIN,
                audiohub_ipc::LATENCY_AUTO
            );
            self.latency = audiohub_ipc::LATENCY_AUTO.to_string();
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

    /// 盘上的延迟档。**认不出来就回落到默认档并说出来**——不是 panic，
    /// 也不是「就近取一个」。
    ///
    /// 会走到这条兜底的只有两种文件：本模块出现之前写的（`"min"`，
    /// [`LatencyTarget::parse`] 认得，翻译是忠实的），以及未来版本写的
    /// （新档位，这个 build 给不了）。后者回落到 AUTO 而不是某个固定毫秒数：
    /// 一个这个 build 兑现不了的固定目标，会让伺服一直贴在边界上报「够不到」，
    /// 而 AUTO 至少是一个**这个 build 能完整执行**的语义。
    pub(crate) fn latency_target(&self) -> LatencyTarget {
        LatencyTarget::parse(&self.latency).unwrap_or_else(|| {
            crate::dlog!(
                "[audiohubd] settings.json 的 latency=\"{}\" 不是本 build 认识的档，按 AUTO 运行",
                self.latency
            );
            LatencyTarget::Auto
        })
    }

    /// 盘上的质量档。同上：认不出来 ⇒ AUTO。
    ///
    /// **Opus 三档会走到这里。** 它们在档位表里可见（UI 要画灰刻度），但
    /// [`QualityTarget::parse`] 拒绝它们，于是一个手工写进 settings.json 的
    /// `"quality":"opus128"` 落到 AUTO 并留下一行日志，而不是被当成某个 PCM 档
    /// 悄悄执行。
    pub(crate) fn quality_target(&self) -> QualityTarget {
        QualityTarget::parse(&self.quality).unwrap_or_else(|| {
            crate::dlog!(
                "[audiohubd] settings.json 的 quality=\"{}\" 本 build 给不了，按 AUTO 运行",
                self.quality
            );
            QualityTarget::Auto
        })
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

    /// 默认档在**语义**上是什么，而不是它拼作哪个字符串。
    ///
    /// 两档都必须是 AUTO —— 这是**行为守恒**的要求，不是偏好：本轮之前这两个
    /// 字段没人读，每台机器实际跑的都是 AUTO 那条路。默认写成别的，就是在升级
    /// 那一刻替所有没动过设置的用户改听感。理由全文见 `StoredSettings::default`。
    #[test]
    fn the_defaults_preserve_the_behaviour_machines_already_had() {
        let d = StoredSettings::default();
        assert_eq!(
            d.latency_target(),
            LatencyTarget::Auto,
            "默认延迟档必须是 AUTO：改成固定档会在升级时把每一台机器的 JB 重新整定"
        );
        assert_eq!(d.quality_target(), QualityTarget::Auto, "默认质量档必须是 AUTO");
    }

    /// 盘上的旧 `"min"` **迁移到 AUTO**，不是翻译成 `0`。
    ///
    /// 它出自一个从不读这个字段的 build，那台机器实际跑的是 AUTO。
    /// 读成 `TotalMs(0)` 会在升级的一瞬间把 JB 从 50 ms 削到 10 ms
    /// （`JbTuning` 实测表：欠载 0.18 -> 3.75 次/min），而用户什么都没做。
    #[test]
    fn a_legacy_min_on_disk_migrates_to_auto_rather_than_changing_the_sound() {
        let dir = tmp("legacymin");
        std::fs::write(
            dir.join("settings.json"),
            br#"{"version":2,"mode":"share","remove_virtual_on_disconnect":false,
                 "mark_offline_devices":true,"latency":"min","quality":"auto"}"#,
        )
        .expect("write");
        assert_eq!(
            StoredSettings::load(&dir).latency_target(),
            LatencyTarget::Auto,
            "盘上的 \"min\" 必须行为守恒地迁移，不是翻译成最低档"
        );
        // 但**明确写来**的 "min"（老客户端的 settings.set）仍然是 0：
        // 那是一个真实的请求，与「从盘上读到一个从未生效过的值」是两回事。
        assert_eq!(
            LatencyTarget::parse(audiohub_ipc::LATENCY_LEGACY_MIN),
            Some(LatencyTarget::TotalMs(0)),
            "明确请求的 \"min\" 仍然是最低档"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 盘上认不出来的档 ⇒ AUTO，而且**其余字段一个都不受影响**。
    ///
    /// 关键在后半句：`StoredSettings` 是一个整体 `Deserialize`，若哪天有人把
    /// `latency` 换成枚举，一个陌生档位会让整份记录（含**模式**）回落默认——
    /// 而模式是 §13 的互斥线。这条测试钉住「档位坏了不许波及模式」。
    #[test]
    fn an_unknown_transport_choice_falls_back_to_auto_without_touching_the_mode() {
        let dir = tmp("badstop");
        std::fs::write(
            dir.join("settings.json"),
            br#"{"version":2,"mode":"b","remove_virtual_on_disconnect":true,
                 "mark_offline_devices":false,"latency":"137","quality":"opus128"}"#,
        )
        .expect("write");
        let got = StoredSettings::load(&dir);
        assert_eq!(got.latency_target(), LatencyTarget::Auto, "137 ms 不是档位 -> AUTO");
        assert_eq!(
            got.quality_target(),
            QualityTarget::Auto,
            "opus128 本 build 给不了 -> AUTO，而不是被当成某个 PCM 档悄悄执行"
        );
        assert_eq!(got.mode, Mode::B, "档位认不出来不许波及模式");
        assert!(got.remove_virtual_on_disconnect, "也不许波及别的开关");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 每一个档位都能穿过**文件**活下来。只测内存里的 parse 不够：
    /// 档位是要写进 settings.json 再读回来的。
    #[test]
    fn every_transport_choice_survives_the_file() {
        let dir = tmp("stops");
        let mut want: Vec<LatencyTarget> = vec![LatencyTarget::Auto];
        want.extend(
            audiohub_ipc::LATENCY_STOPS_MS
                .iter()
                .map(|&m| LatencyTarget::TotalMs(m)),
        );
        for t in want {
            let s = StoredSettings { latency: t.as_wire(), ..StoredSettings::default() };
            s.save(&dir).expect("save");
            assert_eq!(StoredSettings::load(&dir).latency_target(), t, "{t:?} 没活过文件");
        }
        for stop in audiohub_ipc::transport::quality_stops() {
            let Some(q) = QualityTarget::parse(&stop.id) else { continue };
            let s = StoredSettings { quality: q.as_wire(), ..StoredSettings::default() };
            s.save(&dir).expect("save");
            assert_eq!(StoredSettings::load(&dir).quality_target(), q, "{q:?} 没活过文件");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
