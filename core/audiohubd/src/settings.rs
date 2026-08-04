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

/// Bumped to 3 by plan §15: `latency` / `quality` **left this file** and became
/// per-peer × per-direction (`peer_transport.json`).
///
/// 迁移是**删字段**，不是翻译。serde 默认忽略未知字段，所以一个 v2 文件照常
/// 解析，`mode` / `remove_virtual_on_disconnect` / `mark_offline_devices`
/// 全部保住，随后按 v3 重写盘。
///
/// **不把旧的全局值翻译成每对端值。** 把 300 写进每台对端的两个方向，等于替
/// 用户做了一个他从未做过的决定：`send.latency = 300` 这个值在旧世界里**从来
/// 没有存在过**（旧世界里发送方向由对端自己的档位管，实测对端是 `min`）。
/// 凭空造一个设定值再宣布「已迁移」，与下面拒绝翻译 `consumer_mode` 是同一条
/// 理由。用户此前的全局档位**丢失，需要按对端重设**，设置页那一块位置换成一条
/// 迁移说明——沉默地丢掉也不行。
///
/// Bumped to 2 by plan §13: `consumer_mode` became `mode` and gained `share`.
///
/// Nothing migrates. A v1 file simply fails to provide `mode`, so the whole
/// record falls back to [`StoredSettings::default`] — which is exactly the
/// intent, because the pre-§13 value cannot be translated: every v1 machine was
/// a provider AND a consumer at once, so both `"a"` and `"b"` are half of the
/// answer and neither is the whole one. Choosing for the user here would
/// silently decide which half of their setup keeps working.
pub(crate) const SETTINGS_VERSION: u32 = 3;

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
    ///
    /// # 判据是 `>`，不是 `!=`（2026-08-04 实机事故）
    ///
    /// `!=` 把**每一次自家版本号上调**也算成「来自未来」。§15 把
    /// `SETTINGS_VERSION` 从 2 提到 3（只是删掉 `latency`/`quality` 两个字段，
    /// `mode` 的语义一个字没改），于是升级后第一次启动时，所有老用户的
    /// `mode` 都被重置为 `share`——实测：磁盘上还写着 `"mode":"b"`，运行中的
    /// daemon 却报 `mode=share`，§13 随即拆掉全部虚拟设备、会话归零、
    /// 对端判为离线。用户没做任何操作，一次升级就把模式换掉了。
    ///
    /// 旧版本**必须**保住 `mode`：能读出 `mode` 字段就说明它是 §13 之后的文件，
    /// 取值域与本 build 相同。真正不可翻译的是 §13 **之前**的 v1
    /// （字段名还叫 `consumer_mode`），那种文件在 `load` 里就反序列化失败、
    /// 整条记录落到默认值，根本走不到这里 —— 见 `SETTINGS_VERSION` 的注释。
    fn normalized(mut self) -> StoredSettings {
        if self.version > SETTINGS_VERSION {
            crate::dlog!(
                "[audiohubd] settings.json is version {} (this build writes {SETTINGS_VERSION}); \
                 keeping the file's other fields but resetting mode to {}",
                self.version,
                Mode::Share
            );
            self.mode = Mode::Share;
            self.version = SETTINGS_VERSION;
        } else if self.version < SETTINGS_VERSION {
            // 老版本：只补版本号，**不碰 mode**。写回发生在下一次 `save`，
            // 届时那些本 build 不认识的字段（`latency`/`quality`）自然消失。
            crate::dlog!(
                "[audiohubd] settings.json is version {} (this build writes {SETTINGS_VERSION}); \
                 keeping mode={} and dropping any fields this build no longer owns",
                self.version,
                self.mode
            );
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

    /// plan §15 的迁移：一个 v2 文件里的 `latency` / `quality` **被丢掉**，
    /// 而 `mode` 与另外两个开关**一个都不许丢**。
    ///
    /// 前半句是刻意的（见 `SETTINGS_VERSION`：凭空造一个 `send.latency=300`
    /// 等于替用户做了一个他从未做过的决定）。后半句是承重的：`mode` 是 §13
    /// 那条互斥线，把它一起重置比丢一个档位严重得多，而「删字段」这个做法
    /// 之所以安全，正是因为 serde 默认忽略未知字段。
    #[test]
    fn dropping_the_global_stops_does_not_take_the_mode_with_them() {
        let dir = tmp("v15mig");
        std::fs::write(
            dir.join("settings.json"),
            br#"{"version":2,"mode":"b","remove_virtual_on_disconnect":true,
                 "mark_offline_devices":false,"latency":"300","quality":"pcm32k"}"#,
        )
        .expect("write");
        let got = StoredSettings::load(&dir);
        // 这一条是本测试的承重断言，而它此前**只写在上面的注释里、没有断言**——
        // 于是 `normalized()` 用 `!=` 把自家的 2→3 也当成「来自未来」，升级后
        // 把每一个老用户的 mode 重置为 share，测试全绿。实测现场：磁盘上
        // `"mode":"b"`，运行中的 daemon 报 `mode=share`，§13 拆掉全部虚拟设备。
        assert_eq!(got.mode, Mode::B, "老版本文件的 mode 必须原样保住");
        assert_eq!(got.version, SETTINGS_VERSION, "读回来要按本 build 的版本重写");
        assert!(got.remove_virtual_on_disconnect, "普通开关必须原样保住");
        assert!(!got.mark_offline_devices);
        // 两个走掉的字段不该在结构上留下任何痕迹。
        let json = serde_json::to_string(&got).expect("serialize");
        assert!(!json.contains("\"latency\""), "latency 还在 settings.json 里：{json}");
        assert!(!json.contains("\"quality\""), "quality 还在 settings.json 里：{json}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
