//! 每对端 × 每方向的传输档位（plan §15），持久化在
//! `<config_dir>/peer_transport.json`。
//!
//! # 为什么它是一个**独立文件**
//!
//! 两个显而易见的落点都被逐条否掉了：
//!
//! - **`paired_peers.json`**：`PeerStore::upsert` 的语义是「从线上重建记录，
//!   只显式保留 `alias` 与 `added_unix`」。`PairedPeer` 上每加一个**本地**字段，
//!   都必须同时在 `upsert` 里手工加一行保留逻辑，否则**对端下一次重连就把它
//!   抹掉**——表现是用户设的 300 ms 在某次重连后悄悄回到 auto，没有日志、
//!   没有报错、界面照旧显示「配对正常」。`alias` 的保留逻辑就是被这个坑逼
//!   出来的，不该再赌第三次。
//! - **`settings.json`**：`StoredSettings` 是一个整体 `Deserialize`，任一字段
//!   坏掉 ⇒ **整条记录回默认**。往里塞一张按对端的表，等于让**任意一台对端的
//!   任意一个档位串写坏 ⇒ 全机模式被重置回 `Share`**——爆炸半径从「一个档位」
//!   扩大到 §13 那条互斥线，与 `settings.rs` 那段「枚举化会重置 mode」的注释
//!   方向完全相反。
//!
//! # 失效隔离比 `settings.json` 再严一层
//!
//! `settings.json` 是「整个文件坏 ⇒ 全默认」。这里做到**「一条对端记录坏 ⇒
//! 只有那台回默认」**：先读成 `HashMap<String, Value>`，再逐条 `from_value`。
//!
//! # 字段仍然松存紧取
//!
//! 盘上是 `String`（与 `StoredSettings::latency` 同一条理由），写入口用
//! `LatencyTarget::parse` / `QualityTarget::parse` 严格校验并拒绝未知值，
//! 读出口用 `unwrap_or(Auto)` 兜底。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use audiohub_ipc::{LatencyTarget, QualityTarget};

/// 盘上格式的版本。坏了/缺了都只影响这一个文件，见 [`PeerTransportStore::load`]。
const FILE_VERSION: u32 = 1;

/// 一个方向的两个档位。
///
/// **「方向」是用户视角的收/发**，不是执行器所在的那一端——两者在协议消息上
/// 恰好交叉（见 `conn::push_transport`）。这个结构只出现在**本机存储与 UI**
/// 两处，线上永远不出现，所以这里按用户视角命名是安全的。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StoredDir {
    #[serde(default = "auto_latency")]
    pub latency: String,
    #[serde(default = "auto_quality")]
    pub quality: String,
    /// 这一格的质量档在装载时**没被认出来**，已重置为默认；这里留着它原来的
    /// 字符串，只为让 UI 说得出「你存的 X 这个 build 不认识」。
    ///
    /// `#[serde(skip)]` —— **绝不落盘**。落盘的话，一次重置会在文件里长住，
    /// 而用户下次改这一格时它还在，界面就会永远挂着一条已经不成立的说明。
    /// 用户一写入就自然消失（写入口造的是一个全新的 `StoredDir`）。
    #[serde(skip)]
    pub quality_reset_from: Option<String>,
    #[serde(skip)]
    pub latency_reset_from: Option<String>,
}

fn auto_latency() -> String {
    audiohub_ipc::LATENCY_AUTO.to_string()
}

fn auto_quality() -> String {
    audiohub_ipc::QUALITY_AUTO.to_string()
}

impl Default for StoredDir {
    fn default() -> Self {
        StoredDir {
            latency: auto_latency(),
            quality: auto_quality(),
            quality_reset_from: None,
            latency_reset_from: None,
        }
    }
}

impl StoredDir {
    pub(crate) fn latency_target(&self) -> LatencyTarget {
        LatencyTarget::parse(&self.latency).unwrap_or(LatencyTarget::Auto)
    }

    pub(crate) fn quality_target(&self) -> QualityTarget {
        QualityTarget::parse(&self.quality).unwrap_or(QualityTarget::Auto)
    }

    /// Replace any stop string this build does not recognise with the default,
    /// remembering what was there so the UI can explain the reset.
    ///
    /// # Why reset instead of translating, and why say so instead of resetting
    /// # quietly
    ///
    /// Translating is what the deleted legacy layer did: an on-disk `pcm32k`
    /// executed as one rung while a read-only overview drew the raw string, and
    /// nothing could notice the two disagreed. Resetting quietly swaps that for
    /// a different version of the same disease — the user's choice disappears
    /// and every surface stays self-consistent about the wrong thing.
    ///
    /// So: the value is reset (an unexecutable string must not sit there
    /// pretending), and the original is carried out to the UI.
    fn sanitize(&mut self) {
        if LatencyTarget::parse(&self.latency).is_none() {
            self.latency_reset_from = Some(std::mem::replace(&mut self.latency, auto_latency()));
        }
        if QualityTarget::parse(&self.quality).is_none() {
            self.quality_reset_from = Some(std::mem::replace(&mut self.quality, auto_quality()));
        }
    }
}

/// 一台对端的四个档位。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PeerTransport {
    /// 本机**收**这台对端的音（我取它的麦克风）。
    #[serde(default)]
    pub recv: StoredDir,
    /// 本机**发**给这台对端（我送它的扬声器）。
    #[serde(default)]
    pub send: StoredDir,
}

/// 全部对端的档位表，按**指纹**索引。
///
/// 按指纹而不是按连接：连接是易失的（`ConnShared` 随掉线消失），而档位是
/// **用户的持久选择**，必须跨断线、跨重启存活。按会话更不行——重连会铸一个
/// 全新的 `stream_id`，按会话存等于每次重连丢一次设置。
#[derive(Debug, Default)]
pub(crate) struct PeerTransportStore {
    map: HashMap<String, PeerTransport>,
}

#[derive(Serialize)]
struct OnDisk<'a> {
    version: u32,
    peers: &'a HashMap<String, PeerTransport>,
}

impl PeerTransportStore {
    fn path(dir: &Path) -> PathBuf {
        dir.join("peer_transport.json")
    }

    /// 文件缺失 / 顶层 JSON 坏掉 ⇒ 全默认，且**不报错**（与 `StoredSettings::load`
    /// 同一条纪律：偏好文件坏掉不该拦住 daemon 启动）。
    ///
    /// 单条记录坏掉 ⇒ **只有那一台**回默认，记一行日志，其余对端不受影响。
    pub(crate) fn load(dir: &Path) -> PeerTransportStore {
        let path = Self::path(dir);
        let Ok(bytes) = std::fs::read(&path) else {
            return PeerTransportStore::default();
        };
        let top: Value = match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(e) => {
                crate::dlog!("[audiohubd] {} 读不出来（{e}）；全部对端按默认档位跑", path.display());
                return PeerTransportStore::default();
            }
        };
        let mut map = HashMap::new();
        let Some(peers) = top.get("peers").and_then(Value::as_object) else {
            return PeerTransportStore::default();
        };
        for (fp, v) in peers {
            match serde_json::from_value::<PeerTransport>(v.clone()) {
                Ok(mut t) => {
                    // Unrecognised stop strings are reset here, once, at the
                    // single point where the file becomes in-memory state — not
                    // at each of the several read sites, which is how the two
                    // halves of the deleted legacy layer drifted apart.
                    t.recv.sanitize();
                    t.send.sanitize();
                    for (dir, d) in [("recv", &t.recv), ("send", &t.send)] {
                        if let Some(old) = &d.latency_reset_from {
                            crate::dlog!(
                                "[audiohubd] peer_transport.json {fp}/{dir}: 延迟档 `{old}` \
                                 本 build 不认识，已重置为默认（UI 会说明）"
                            );
                        }
                        if let Some(old) = &d.quality_reset_from {
                            crate::dlog!(
                                "[audiohubd] peer_transport.json {fp}/{dir}: 质量档 `{old}` \
                                 本 build 不认识，已重置为默认（UI 会说明）"
                            );
                        }
                    }
                    map.insert(fp.clone(), t);
                }
                // **只有这一台**回默认。整表回默认会让一个手工写坏的字符串
                // 把其它每一台对端的设置一起抹掉，而用户只碰过其中一台。
                Err(e) => crate::dlog!(
                    "[audiohubd] peer_transport.json 里 {fp} 那条读不出来（{e}）；\
                     只有这一台回到默认档位，其余对端不受影响"
                ),
            }
        }
        PeerTransportStore { map }
    }

    pub(crate) fn save(&self, dir: &Path) -> Result<()> {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        let path = Self::path(dir);
        let body = serde_json::to_vec_pretty(&OnDisk { version: FILE_VERSION, peers: &self.map })?;
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, &body).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("rename into {}", path.display()))?;
        Ok(())
    }

    /// 没设过的对端返回**默认**（auto/auto ×2），不是 `None`：调用方一律
    /// 需要一个可执行的档位，而「没设过」与「设成了 auto」在执行上就是同一件事。
    pub(crate) fn get(&self, fp: &str) -> PeerTransport {
        self.map.get(fp).cloned().unwrap_or_default()
    }

    pub(crate) fn set(&mut self, fp: &str, t: PeerTransport) {
        self.map.insert(fp.to_string(), t);
    }

    /// 解除配对时清掉。留着的话，重新配对同一台机器会**静默继承**上一段关系的
    /// 档位——「我明明没设过 300」的又一种成因。
    pub(crate) fn remove(&mut self, fp: &str) -> bool {
        self.map.remove(fp).is_some()
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.map.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let d = std::env::temp_dir().join(format!("ahb-pt-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&d).expect("mkdir");
        d
    }

    #[test]
    fn a_choice_survives_a_round_trip_through_the_file() {
        let dir = tmpdir("rt");
        let mut s = PeerTransportStore::default();
        s.set(
            "aa11",
            PeerTransport {
                recv: StoredDir {
                    latency: "300".into(),
                    quality: "pcm32k16".into(),
                    ..StoredDir::default()
                },
                send: StoredDir {
                    latency: "100".into(),
                    quality: "auto".into(),
                    ..StoredDir::default()
                },
            },
        );
        s.save(&dir).expect("save");

        let back = PeerTransportStore::load(&dir);
        assert_eq!(back.get("aa11").recv.latency, "300");
        assert_eq!(back.get("aa11").send.latency, "100");
        assert_eq!(back.get("aa11").recv.quality, "pcm32k16");
        // 没设过的对端拿到默认，不是恐慌也不是 None。
        assert_eq!(back.get("zz99"), PeerTransport::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **一条坏记录只毒死它自己。**
    ///
    /// 这一条就是「不放进 `settings.json`」那个决定的可执行形式：放进去的话
    /// 同样一个坏字符串会把 `mode` 一起重置，而 `mode` 是 §13 的互斥线。
    #[test]
    fn one_corrupt_record_does_not_take_the_others_down() {
        let dir = tmpdir("iso");
        let raw = r#"{
          "version": 1,
          "peers": {
            "good": { "recv": { "latency": "300", "quality": "auto" },
                      "send": { "latency": "auto", "quality": "auto" } },
            "bad":  { "recv": 42, "send": "nope" }
          }
        }"#;
        std::fs::write(PeerTransportStore::path(&dir), raw).expect("write");

        let s = PeerTransportStore::load(&dir);
        assert_eq!(s.get("good").recv.latency, "300", "好记录被坏记录连坐了");
        assert_eq!(s.get("bad"), PeerTransport::default(), "坏记录该回默认");
        assert_eq!(s.len(), 1, "坏记录不该被留在表里");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An unrecognised stop is reset **per cell**, and the original string is
    /// carried out so the UI can explain it. The other cells are untouched.
    ///
    /// `pcm32k` is the interesting case: it used to be silently translated to
    /// `pcm32k16` by the deleted compatibility layer. Silent translation is
    /// exactly what let one page draw `pcm32k` and another draw
    /// "PCM 32 kHz - 16 bit" for the same stored byte, with nothing to notice.
    ///
    /// Injection check: make `sanitize` leave the string alone (return early)
    /// and this goes red on "was left in place"; make it reset without setting
    /// `*_reset_from` and it goes red on "reset silently".
    #[test]
    fn an_unrecognised_stop_is_reset_per_cell_and_reported() {
        let mut d = StoredDir {
            latency: "nonsense".into(),
            quality: "pcm32k".into(),
            ..StoredDir::default()
        };
        d.sanitize();

        assert_eq!(d.latency, "auto", "unrecognised latency was left in place");
        assert_eq!(
            d.latency_reset_from.as_deref(),
            Some("nonsense"),
            "latency was reset silently; the UI has nothing to explain it with"
        );
        assert_eq!(d.quality, "auto", "stale id `pcm32k` was left in place or translated");
        assert_eq!(
            d.quality_reset_from.as_deref(),
            Some("pcm32k"),
            "quality was reset silently; the UI has nothing to explain it with"
        );
        assert_eq!(d.latency_target(), LatencyTarget::Auto);
        assert_eq!(d.quality_target(), QualityTarget::Auto);
    }

    /// A recognised value is left completely alone — no reset marker, so the UI
    /// does not show an explanation nobody needs.
    ///
    /// Without this, a `sanitize` that reset everything unconditionally would
    /// pass the test above.
    #[test]
    fn a_recognised_stop_is_untouched_by_sanitize() {
        let mut d = StoredDir {
            latency: "300".into(),
            quality: "pcm48k24".into(),
            ..StoredDir::default()
        };
        d.sanitize();
        assert_eq!(d.latency, "300");
        assert_eq!(d.quality, "pcm48k24");
        assert_eq!(d.latency_reset_from, None, "a valid stop was flagged as reset");
        assert_eq!(d.quality_reset_from, None, "a valid stop was flagged as reset");
    }

    /// The reset happens **on load**, so every read site sees the sanitised
    /// value. Having each read site fall back on its own is how the deleted
    /// legacy layer ended up with one path that forgot to.
    #[test]
    fn loading_a_file_with_a_stale_id_resets_it_and_remembers_the_original() {
        let dir = tmpdir("stale");
        let raw = r#"{
          "version": 1,
          "peers": {
            "aa11": { "recv": { "latency": "300", "quality": "pcm32k" },
                      "send": { "latency": "auto", "quality": "pcm48k24" } }
          }
        }"#;
        std::fs::write(PeerTransportStore::path(&dir), raw).expect("write");

        let t = PeerTransportStore::load(&dir).get("aa11");
        assert_eq!(t.recv.quality, "auto", "stale id survived the load path");
        assert_eq!(t.recv.quality_reset_from.as_deref(), Some("pcm32k"));
        assert_eq!(t.recv.latency, "300", "a valid neighbouring cell was collateral damage");
        assert_eq!(t.send.quality, "pcm48k24", "a valid cell in the other direction was reset");
        assert_eq!(t.send.quality_reset_from, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The reset marker **never reaches disk**. Persisting it would leave a
    /// permanent note on a file the user has since fixed.
    #[test]
    fn the_reset_marker_is_not_persisted() {
        let dir = tmpdir("nomark");
        let mut s = PeerTransportStore::default();
        let mut d = StoredDir { quality: "pcm32k".into(), ..StoredDir::default() };
        d.sanitize();
        s.set("aa11", PeerTransport { recv: d, send: StoredDir::default() });
        s.save(&dir).expect("save");

        let raw = std::fs::read_to_string(PeerTransportStore::path(&dir)).expect("read");
        assert!(
            !raw.contains("reset_from"),
            "the reset marker was written to disk; it would outlive the condition it describes"
        );
        assert!(!raw.contains("pcm32k"), "the stale id was written back out");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn forgetting_a_peer_drops_its_row() {
        let mut s = PeerTransportStore::default();
        s.set("aa11", PeerTransport::default());
        assert!(s.remove("aa11"));
        assert!(!s.remove("aa11"), "第二次删该报 false（没有这条了）");
    }
}
