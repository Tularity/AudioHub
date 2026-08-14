//! Daemon-owned settings, persisted at `<config_dir>/settings.json`
//! (spec-m5b §6.1).
//!
//! The consumer mode used to live in the UI's localStorage, which made it a
//! per-BROWSER-PROFILE opinion about a machine-wide fact: two UI windows could
//! disagree, a CLI knew nothing about it, and the daemon — the only process
//! that can actually publish or remove virtual devices — was never told. It is
//! daemon state now, and the UI's copy is a cache of what this file says.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use audiohub_security::{secure_private_directory, secure_private_file};
use serde::{Deserialize, Serialize};

use audiohub_ipc::Mode;

/// `native_locale` was added while keeping version 6. It is a defaultable,
/// additive preference: an existing file must keep the old Chinese device
/// names until its native App reports another locale, and an older v6 daemon
/// must be able to read a file rewritten by this build without treating it as
/// a future version and resetting `mode` to `share`.
///
/// Bumped to 6 for the audio-only AirPlay receiver wish/name. The password is
/// intentionally not part of this record; it has a separate restricted file
/// and only a presence bit crosses IPC.
///
/// Version 6 originally also carried `airplay_route`. AirPlay is now always
/// played on the receiving machine, so that field was deleted without another
/// settings-version bump: serde ignores the old unknown key, and keeping v6
/// lets an older daemon read a file rewritten by this build without treating
/// it as a future version and resetting `mode` to `share`.
///
/// Bumped to 5 by plan M3 「同网段互见」: `discovery_announce` arrived.
///
/// It is the first field here whose absent value is **not** the `bool` default.
/// A bare `#[serde(default)]` would read as `false`, so every machine that
/// already has a settings.json would come back from the upgrade invisible on
/// the network while a machine installed the same day would be visible — a
/// split nothing in the interface could explain, and nothing in the code would
/// report. `#[serde(default = "announce_by_default")]` makes "absent" mean the
/// same thing as "fresh", which is the only reading that keeps one behaviour.
///
/// Bumped to 4 by plan §7.1: the two mode-A volume switches
/// (`mode_a_volume_sync` / `mode_a_mute_local`) arrived.
///
/// Both are `#[serde(default)]`, and that is load-bearing rather than tidy: a
/// field without it makes every v3 file fail to deserialize, which sends the
/// WHOLE record to the defaults and resets `mode` — the exact 2026-08-04
/// accident recorded under `normalized()`. Adding a field must never be able to
/// change a mode.
///
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
pub(crate) const SETTINGS_VERSION: u32 = 6;

/// `discovery_announce` defaults ON — see `StoredSettings::discovery_announce`.
/// A named function rather than `#[serde(default)]` on purpose; the comment on
/// [`SETTINGS_VERSION`] says why.
fn announce_by_default() -> bool {
    true
}

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
    /// Resolved language of the native App for names shown by the operating
    /// system. This is deliberately not inferred by the headless daemon: a
    /// service/session locale can differ from the interactive user's locale.
    #[serde(default = "default_native_locale")]
    pub native_locale: String,
    /// plan §7.1 「与对端音量同步」 — a mode-A option, so it lives HERE and not
    /// in `peer_transport.json`. Three reasons, in order of weight:
    ///
    ///  1. §7.1 freezes 「模式是全局设置，不是每个对端各自的开关」, and this is
    ///     an option *of that mode*, introduced in the same paragraph.
    ///  2. §7.1 also keeps 「同一时刻只能使用一个对端」 as mode A's own rule, so
    ///     a per-peer copy would have at most one row that could ever be in
    ///     force, and nothing in the interface could say which one.
    ///  3. What it drives is THIS machine's default output device — one device,
    ///     machine-global. Stored per peer, two peers could hold contradictory
    ///     opinions about it and we would have to invent an arbitration rule
    ///     plan never wrote.
    #[serde(default)]
    pub mode_a_volume_sync: bool,
    /// plan §7.1 「静音本机输出」. Same file for the same reasons, plus one of
    /// its own: it is a ONE-SHOT action on the local device at stream setup,
    /// with no per-peer state to remember afterwards.
    #[serde(default)]
    pub mode_a_mute_local: bool,
    /// plan M3 「同网段互见」: announce this machine over mDNS so the others can
    /// list it without anyone typing an IP.
    ///
    /// **Machine state, not a launch flag.** It used to be `--announce`, and
    /// the consequence was that the shipping app never announced at all: the
    /// UI spawns the daemon with no arguments, the autostart entries do the
    /// same, and only two hand-run CLI subcommands passed it. A flag also has
    /// no way back — nothing in the interface can turn a launch argument off,
    /// which is exactly what the privacy half of this switch needs.
    ///
    /// Default ON: 「打开就看见对方」 is the M3 acceptance criterion, and a
    /// discovery feature that is off until found is not a feature. The cost is
    /// disclosed and reversible — see `audiohub_net::discovery` for what the
    /// record contains and why the fingerprint is in it.
    #[serde(default = "announce_by_default")]
    pub discovery_announce: bool,
    /// Audio-only AirPlay receiver. Disabled by default because enabling it
    /// opens unauthenticated LAN listeners unless a password is configured.
    #[serde(default)]
    pub airplay_enabled: bool,
    /// Empty follows the daemon identity (`AudioHub — <name>`).
    #[serde(default)]
    pub airplay_name: String,
}

fn default_native_locale() -> String {
    "zh-CN".to_string()
}

pub(crate) fn valid_native_locale(locale: &str) -> bool {
    matches!(locale, "zh-CN" | "en-US")
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
            native_locale: default_native_locale(),
            // plan §7.1 calls both of these 「选项」/「独立开关」 — opt-in, and
            // both reach outside our own process to change a device the user is
            // listening to right now. A default that silences a machine, or
            // that hands its volume knob to another machine, is not a default.
            mode_a_volume_sync: false,
            mode_a_mute_local: false,
            // ON, unlike the two above, and the difference is the direction the
            // switch points: those two reach OUT and change a device the user
            // is listening to right now, so they have to be asked for. This one
            // only makes this machine listable by machines that can already
            // reach its control port, and it is the whole content of M3
            // 「同网段互见」 — off by default, the acceptance criterion is a
            // feature nobody finds.
            discovery_announce: announce_by_default(),
            airplay_enabled: false,
            airplay_name: String::new(),
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
        if !valid_native_locale(&self.native_locale) {
            crate::dlog!(
                "[audiohubd] settings.json has unsupported native_locale {:?}; using zh-CN",
                self.native_locale
            );
            self.native_locale = default_native_locale();
        }
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

const AIRPLAY_SECRET_FILE: &str = "airplay-password";
static AIRPLAY_SECRET_TMP_SEQ: AtomicU64 = AtomicU64::new(1);

/// Exact on-disk state used to compensate a failed multi-file settings commit.
///
/// Bytes, rather than `Option<String>`, are intentional: an invalid UTF-8
/// secret makes the receiver fail closed, but the settings page must still be
/// able to replace it. If a later settings write fails, rollback has to restore
/// that fail-closed file byte-for-byte instead of failing while trying to read
/// it as a password.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AirPlayPasswordSnapshot(Option<Vec<u8>>);

pub(crate) fn snapshot_airplay_password(dir: &Path) -> Result<AirPlayPasswordSnapshot> {
    secure_private_directory(dir)
        .with_context(|| format!("secure private directory {}", dir.display()))?;
    let path = dir.join(AIRPLAY_SECRET_FILE);
    match secure_private_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(AirPlayPasswordSnapshot(None))
        }
        Err(error) => return Err(error).with_context(|| format!("secure {}", path.display())),
    }
    match std::fs::read(&path) {
        Ok(bytes) => Ok(AirPlayPasswordSnapshot(Some(bytes))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(AirPlayPasswordSnapshot(None))
        }
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

/// Read the write-only AirPlay secret. It deliberately lives outside
/// `settings.json`: that file is routinely returned over IPC, while this value
/// must never enter a settings view or frontend state snapshot.
pub(crate) fn load_airplay_password(dir: &Path) -> Result<Option<String>> {
    secure_private_directory(dir)
        .with_context(|| format!("secure private directory {}", dir.display()))?;
    let path = dir.join(AIRPLAY_SECRET_FILE);
    match secure_private_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("secure {}", path.display())),
    }
    match std::fs::read_to_string(&path) {
        Ok(secret) if secret.is_empty() => Ok(None),
        Ok(secret) => Ok(Some(secret)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

pub(crate) fn airplay_password_is_set(dir: &Path) -> bool {
    // This field is only a presence bit. A present-but-unreadable file still
    // means the user configured protection; reconcile reports the read error
    // and refuses to start instead of presenting an open receiver.
    dir.join(AIRPLAY_SECRET_FILE).exists()
}

pub(crate) fn validate_airplay_password(password: &str) -> Result<()> {
    anyhow::ensure!(
        password.chars().count() <= 128,
        "AirPlay password must be at most 128 characters"
    );
    anyhow::ensure!(
        !password.chars().any(char::is_control),
        "AirPlay password cannot contain control characters"
    );
    Ok(())
}

fn clear_airplay_password(dir: &Path) -> Result<()> {
    let path = dir.join(AIRPLAY_SECRET_FILE);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove {}", path.display())),
    }
}

fn replace_airplay_password_bytes(dir: &Path, contents: &[u8]) -> Result<()> {
    secure_private_directory(dir)
        .with_context(|| format!("secure private directory {}", dir.display()))?;
    let path = dir.join(AIRPLAY_SECRET_FILE);
    let seq = AIRPLAY_SECRET_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(
        ".airplay-password.{}.{}.tmp",
        std::process::id(),
        seq
    ));
    let mut opts = std::fs::OpenOptions::new();
    opts.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let result = (|| -> Result<()> {
        let mut file = opts
            .open(&tmp)
            .with_context(|| format!("write {}", tmp.display()))?;
        secure_private_file(&tmp).with_context(|| format!("secure {}", tmp.display()))?;
        file.write_all(contents)
            .with_context(|| format!("write {}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("sync {}", tmp.display()))?;
        drop(file);
        std::fs::rename(&tmp, &path).with_context(|| format!("rename to {}", path.display()))?;
        secure_private_file(&path).with_context(|| format!("secure {}", path.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

pub(crate) fn restore_airplay_password(
    dir: &Path,
    snapshot: &AirPlayPasswordSnapshot,
) -> Result<()> {
    match &snapshot.0 {
        Some(contents) => replace_airplay_password_bytes(dir, contents),
        None => clear_airplay_password(dir),
    }
}

/// Replace (or explicitly clear) the AirPlay password using an atomic rename.
/// On Unix the file is created 0600. On Windows its DACL is protected from
/// inheritance and grants access only to the current user, Administrators and
/// SYSTEM; persistence never shells out to PowerShell.
pub(crate) fn save_airplay_password(dir: &Path, password: &str) -> Result<()> {
    validate_airplay_password(password)?;
    if password.is_empty() {
        return clear_airplay_password(dir);
    }
    replace_airplay_password_bytes(dir, password.as_bytes())
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
        // plan §7.1 calls both of these 「选项」. Both reach outside this process
        // to change a device the user is listening to; neither is a default.
        assert!(
            !d.mode_a_volume_sync,
            "「与对端音量同步」 must be opt-in: on by default, a fresh pairing hands this \
             machine's volume knob to another machine"
        );
        assert!(
            !d.mode_a_mute_local,
            "「静音本机」 must be opt-in: on by default, the first stream silences a machine \
             the user never asked to silence"
        );
        // plan M3 「同网段互见」. The opposite default from the two above, and
        // the asymmetry is the point: those reach out and change a device the
        // user is listening to, this one only makes this machine listable.
        assert!(
            d.discovery_announce,
            "a fresh machine must announce itself: 「打开就看见对方」 is the M3 acceptance \
             criterion, and off by default it is a feature nobody finds"
        );
    }

    /// **A settings.json written before this field existed must read as ON.**
    ///
    /// This is the whole reason `discovery_announce` uses
    /// `#[serde(default = "announce_by_default")]` and not a bare
    /// `#[serde(default)]`. With the bare one, absent reads as `false`: every
    /// machine that upgrades comes back invisible on the network while a
    /// machine installed the same afternoon is visible, the two disagree
    /// forever, and nothing anywhere reports a difference — the file simply
    /// does not have the key.
    ///
    /// The v4 body below is a real one: `mode` and both mode-A switches present
    /// and non-default, so a failure here also shows whether the record
    /// survived at all or fell back wholesale.
    #[test]
    fn a_settings_file_from_before_discovery_announce_still_announces() {
        let dir = tmp("m3announce");
        std::fs::write(
            dir.join("settings.json"),
            br#"{"version":4,"mode":"b","remove_virtual_on_disconnect":true,
                 "mark_offline_devices":false,"mode_a_volume_sync":true,
                 "mode_a_mute_local":true}"#,
        )
        .expect("write");
        let got = StoredSettings::load(&dir);
        assert!(
            got.discovery_announce,
            "an upgraded machine went silent: an absent 'discovery_announce' must mean the \
             default (on), not bool::default() (off)"
        );
        // The rest of the record is untouched — adding a field must never be
        // able to change a mode (see SETTINGS_VERSION).
        assert_eq!(got.mode, Mode::B, "加字段把用户的模式重置了");
        assert!(got.remove_virtual_on_disconnect);
        assert!(!got.mark_offline_devices);
        assert!(got.mode_a_volume_sync);
        assert!(got.mode_a_mute_local);
        assert_eq!(got.version, SETTINGS_VERSION);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The privacy switch has to survive the file in **both** directions.
    ///
    /// Asserting only `false` would pass against an implementation that never
    /// reads the key back and always answers with the default — which is `true`
    /// — and asserting only `true` would pass against one that never writes it.
    #[test]
    fn the_discovery_switch_survives_the_file_both_ways() {
        let dir = tmp("m3switch");
        for want in [false, true, false] {
            let s = StoredSettings {
                discovery_announce: want,
                ..StoredSettings::default()
            };
            s.save(&dir).expect("save");
            let got = StoredSettings::load(&dir);
            assert_eq!(
                got.discovery_announce, want,
                "discovery_announce={want} 没有活过盘：隐私开关关掉之后重启又自己广播了"
            );
            assert_eq!(got, s, "这一个开关之外还有别的字段被这次写入改掉了");
        }
        // And it is really IN the file, not merely reconstructed from defaults.
        let body = std::fs::read_to_string(dir.join("settings.json")).expect("read");
        assert!(
            body.contains("\"discovery_announce\""),
            "settings.json 里没有 discovery_announce 这个键：{body}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// plan §7.1 的两个开关必须真的**落盘并读回**。
    ///
    /// 分开断言 true/false 两种取值，而不是只写一次 `true`：把 `load` 写成
    /// 「永远返回默认值」的实现能通过任何只测一个方向的断言，而默认值恰好
    /// 就是 false。
    #[test]
    fn the_two_mode_a_switches_survive_the_file() {
        let dir = tmp("modeavol");
        for (sync, mute) in [(true, false), (false, true), (true, true), (false, false)] {
            let want = StoredSettings {
                mode_a_volume_sync: sync,
                mode_a_mute_local: mute,
                ..StoredSettings::default()
            };
            want.save(&dir).expect("save");
            let got = StoredSettings::load(&dir);
            assert_eq!(
                got.mode_a_volume_sync, sync,
                "mode_a_volume_sync 没有活过盘"
            );
            assert_eq!(got.mode_a_mute_local, mute, "mode_a_mute_local 没有活过盘");
            assert_eq!(got, want, "两个开关之外还有别的字段被这次写入改掉了");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **加字段不许改模式。**
    ///
    /// 一个 v3 文件（§15 之后、§7.1 之前）里没有这两个新字段。少了
    /// `#[serde(default)]`，整条记录反序列化失败、落到默认值，用户的 `mode`
    /// 就被一次升级重置了——2026-08-04 那次实机事故的成因一模一样，只是触发
    /// 方式从「版本号比较写错」换成了「加了个字段」。
    #[test]
    fn a_v3_file_keeps_its_mode_and_simply_gains_the_new_switches() {
        let dir = tmp("v71mig");
        std::fs::write(
            dir.join("settings.json"),
            br#"{"version":3,"mode":"b","remove_virtual_on_disconnect":true,
                 "mark_offline_devices":false}"#,
        )
        .expect("write");
        let got = StoredSettings::load(&dir);
        assert_eq!(got.mode, Mode::B, "加两个字段把用户的模式重置了");
        assert!(got.remove_virtual_on_disconnect, "普通开关也必须原样保住");
        assert!(!got.mark_offline_devices);
        assert!(!got.mode_a_volume_sync, "缺席的新开关取默认值，不是随机值");
        assert!(!got.mode_a_mute_local);
        assert_eq!(
            got.version, SETTINGS_VERSION,
            "读回来要按本 build 的版本重写"
        );
        let _ = std::fs::remove_dir_all(&dir);
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
            let s = StoredSettings {
                mode: m,
                ..StoredSettings::default()
            };
            s.save(&dir).expect("save");
            assert_eq!(
                StoredSettings::load(&dir).mode,
                m,
                "{m} did not survive the file"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn native_locale_survives_restart_and_old_files_keep_the_old_chinese_names() {
        let dir = tmp("native-locale");
        let en = StoredSettings {
            native_locale: "en-US".into(),
            ..StoredSettings::default()
        };
        en.save(&dir).expect("save en-US");
        assert_eq!(StoredSettings::load(&dir).native_locale, "en-US");

        // v6 predates this field. Defaulting it to zh-CN is behavioural
        // preservation: those installations already showed `（离线）`.
        std::fs::write(
            dir.join("settings.json"),
            br#"{"version":6,"mode":"b","remove_virtual_on_disconnect":false,
                 "mark_offline_devices":true}"#,
        )
        .expect("write v6");
        let old = StoredSettings::load(&dir);
        assert_eq!(old.native_locale, "zh-CN");
        assert_eq!(old.mode, Mode::B, "adding locale changed the user's mode");
        assert_eq!(old.version, SETTINGS_VERSION);

        // A hand-edited unsupported value cannot leak arbitrary language tags
        // into the naming path; normalize to the same compatibility default.
        std::fs::write(
            dir.join("settings.json"),
            br#"{"version":6,"mode":"b","remove_virtual_on_disconnect":false,
                 "mark_offline_devices":true,"native_locale":"fr-FR"}"#,
        )
        .expect("write unsupported locale");
        assert_eq!(StoredSettings::load(&dir).native_locale, "zh-CN");
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
            assert_eq!(
                got,
                StoredSettings::default(),
                "the whole record resets, not just mode"
            );
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
        assert_eq!(
            got.mode,
            Mode::Share,
            "a mode from an unknown version is not trusted"
        );
        assert!(
            got.remove_virtual_on_disconnect,
            "...but the ordinary preferences are kept"
        );
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
        assert_eq!(
            got.version, SETTINGS_VERSION,
            "读回来要按本 build 的版本重写"
        );
        assert!(got.remove_virtual_on_disconnect, "普通开关必须原样保住");
        assert!(!got.mark_offline_devices);
        // 两个走掉的字段不该在结构上留下任何痕迹。
        let json = serde_json::to_string(&got).expect("serialize");
        assert!(
            !json.contains("\"latency\""),
            "latency 还在 settings.json 里：{json}"
        );
        assert!(
            !json.contains("\"quality\""),
            "quality 还在 settings.json 里：{json}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn airplay_defaults_are_closed() {
        let s = StoredSettings::default();
        assert!(
            !s.airplay_enabled,
            "a fresh install must not open a LAN listener"
        );
        assert_eq!(s.airplay_name, "", "empty follows the daemon identity");
    }

    #[test]
    fn legacy_v6_airplay_route_is_ignored_and_dropped_on_next_save() {
        let dir = tmp("airplay-route-v6");
        std::fs::write(
            dir.join("settings.json"),
            br#"{"version":6,"mode":"b","remove_virtual_on_disconnect":true,
                 "mark_offline_devices":false,"airplay_enabled":true,
                 "airplay_name":"Legacy receiver","airplay_route":"both"}"#,
        )
        .expect("write legacy settings");

        let got = StoredSettings::load(&dir);
        assert_eq!(got.version, SETTINGS_VERSION);
        assert_eq!(got.mode, Mode::B, "removing AirPlay routing changed mode");
        assert!(got.remove_virtual_on_disconnect);
        assert!(!got.mark_offline_devices);
        assert!(got.airplay_enabled);
        assert_eq!(got.airplay_name, "Legacy receiver");

        got.save(&dir).expect("rewrite settings");
        let body = std::fs::read_to_string(dir.join("settings.json")).expect("read rewrite");
        assert!(
            !body.contains("airplay_route"),
            "the removed route survived a settings rewrite: {body}"
        );
        assert_eq!(StoredSettings::load(&dir), got);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn airplay_password_is_separate_write_only_state() {
        let dir = tmp("airplay-secret");
        assert!(!airplay_password_is_set(&dir));
        save_airplay_password(&dir, "correct horse battery staple").expect("save secret");
        assert_eq!(
            load_airplay_password(&dir).expect("load secret").as_deref(),
            Some("correct horse battery staple")
        );
        let ordinary = std::fs::read_to_string(dir.join("settings.json")).unwrap_or_default();
        assert!(
            !ordinary.contains("correct horse"),
            "secret leaked into settings.json"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = dir.join(AIRPLAY_SECRET_FILE);
            let mode = std::fs::metadata(&path)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);

            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
                .expect("loosen file for upgrade test");
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755))
                .expect("loosen directory for upgrade test");
            load_airplay_password(&dir).expect("reload repairs permissions");
            assert_eq!(
                std::fs::metadata(&path)
                    .expect("metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(&dir)
                    .expect("metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
        save_airplay_password(&dir, "").expect("clear secret");
        assert!(!airplay_password_is_set(&dir));
        assert!(!dir.join(AIRPLAY_SECRET_FILE).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unreadable_airplay_secret_is_an_error_not_an_open_receiver() {
        let dir = tmp("airplay-secret-invalid");
        std::fs::create_dir_all(&dir).expect("create config dir");
        std::fs::write(dir.join(AIRPLAY_SECRET_FILE), [0xff]).expect("write invalid UTF-8");
        assert!(
            airplay_password_is_set(&dir),
            "the configured-secret bit must remain true"
        );
        assert!(
            load_airplay_password(&dir).is_err(),
            "an unreadable/invalid secret must not be mistaken for no password"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_secret_replacements_never_share_a_temp_file() {
        let dir = tmp("airplay-secret-concurrent");
        let values: Vec<String> = (0..12).map(|n| format!("secret-{n}")).collect();
        let joins: Vec<_> = values
            .iter()
            .cloned()
            .map(|value| {
                let dir = dir.clone();
                std::thread::spawn(move || save_airplay_password(&dir, &value))
            })
            .collect();
        for join in joins {
            join.join()
                .expect("secret writer panicked")
                .expect("secret write");
        }
        let final_value = load_airplay_password(&dir)
            .expect("load final secret")
            .expect("secret exists");
        assert!(
            values.contains(&final_value),
            "torn secret: {final_value:?}"
        );
        let leftovers = std::fs::read_dir(&dir)
            .expect("read config dir")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(leftovers, 0, "unique temp files must be cleaned or renamed");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
