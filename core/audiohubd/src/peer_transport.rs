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

/// Which transport a peer's media runs on (`docs/plan.md` §16, design
/// `docs/design-m8-fallback.md` §5.1).
///
/// # Why this is **per peer** and not per direction
///
/// Both directions share one `ConnShared`. On tier 0 they share one UDP socket
/// and one destination; on tier 1 they share the one media TCP connection.
/// **After a downgrade the transport is per peer as a matter of physics.** The
/// asymmetry that does exist (our outbound UDP gets through, their inbound does
/// not) belongs to *detection*, and it is carried by the reason string, not by
/// splitting this into two values — a per-direction field would read as a
/// promise that one direction can sit on tier 0 while the other sits on tier 1,
/// and somebody would eventually try to implement it.
///
/// # Tier 2 is reachable only by pinning, and that is the design, not a gap
///
/// plan §16.2: tier 0 → 1 is automatic because it has a clean criterion (UDP
/// silence plus a healthy control channel), and → 2 is **manual** because its
/// premise is a property of the user's tunnel that cannot be observed — "the
/// peer cannot be dialled" and "the peer is switched off" are the same
/// observation. Automatic probing could only be repeated dialling and guessing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransportTier {
    /// Decide at runtime: **the only value that defers to the detector**
    /// ([`PeerTransport::effective_tier`]). Dials tier 0 until something has
    /// been observed, then tier 1 for as long as the verdict stands.
    ///
    /// It also *grants* a tier 1 attach a peer asks for, which is a separate
    /// question from what it dials and is decided by [`PeerTransport::tier`],
    /// not the effective tier. Two AUTO machines are both nominally tier 0
    /// until one of them notices something; if the granting side asked the
    /// effective tier, each would refuse the other and no downgrade could ever
    /// complete.
    Auto,
    /// Pinned to UDP media. Refuses to attach a tier 1 link even when asked;
    /// this is the "通告 ≠ 授权" rule (design decision C) in its one concrete
    /// form on this path.
    Tier0,
    /// Pinned to media over a second TCP connection.
    Tier1,
    /// Pinned to **one** connection carrying control and both media directions.
    Tier2,
}

impl TransportTier {
    pub(crate) fn parse(s: &str) -> Option<TransportTier> {
        match s {
            "auto" => Some(TransportTier::Auto),
            "tier0" => Some(TransportTier::Tier0),
            "tier1" => Some(TransportTier::Tier1),
            "tier2" => Some(TransportTier::Tier2),
            _ => None,
        }
    }

    pub(crate) fn as_wire(self) -> &'static str {
        match self {
            TransportTier::Auto => "auto",
            TransportTier::Tier0 => "tier0",
            TransportTier::Tier1 => "tier1",
            TransportTier::Tier2 => "tier2",
        }
    }
}

/// Which side may originate the control connection to a peer (design §4.2
/// item 1).
///
/// # Why this is a setting and not something we discover
///
/// The same argument that makes tier 2 manual: a tunnel that only forwards one
/// way produces failed dials, and a failed dial is exactly what a powered-off
/// machine produces. The difference is knowledge the user has and we do not.
///
/// # Why it lives here and not in `paired_peers.json`
///
/// See this file's header: `PeerStore::upsert` rebuilds a record from the wire
/// and keeps only the fields it explicitly preserves, so a local setting put
/// there is erased **the next time the peer reconnects** — silently, with the
/// UI still showing the old value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DialPolicy {
    /// The default and the tier 0/1 behaviour: dial when we have somewhere to
    /// dial, accept when we do not.
    Both,
    /// Only ever dial out. We are on the side of the tunnel that can originate,
    /// and the peer will never reach us.
    OutboundOnly,
    /// **Never dial; wait to be dialled.** The peer is behind a tunnel that
    /// only carries connections in the other direction.
    ///
    /// The consequence that is easy to miss: such a peer is not *offline* while
    /// it is not connected — it is waiting, which is a third state and has to
    /// be reported as one. Rendering it as offline would put a permanent red
    /// mark on a peer whose setup is working exactly as configured.
    InboundOnly,
}

impl DialPolicy {
    pub(crate) fn parse(s: &str) -> Option<DialPolicy> {
        match s {
            "both" => Some(DialPolicy::Both),
            "outbound_only" => Some(DialPolicy::OutboundOnly),
            "inbound_only" => Some(DialPolicy::InboundOnly),
            _ => None,
        }
    }

    pub(crate) fn as_wire(self) -> &'static str {
        match self {
            DialPolicy::Both => "both",
            DialPolicy::OutboundOnly => "outbound_only",
            DialPolicy::InboundOnly => "inbound_only",
        }
    }

    /// May this daemon originate a connection to the peer?
    pub(crate) fn may_dial(self) -> bool {
        !matches!(self, DialPolicy::InboundOnly)
    }
}

/// 一台对端的四个档位，外加它的连通性档位。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PeerTransport {
    /// 本机**收**这台对端的音（我取它的麦克风）。
    #[serde(default)]
    pub recv: StoredDir,
    /// 本机**发**给这台对端（我送它的扬声器）。
    #[serde(default)]
    pub send: StoredDir,
    /// `auto | tier0 | tier1`，见 [`TransportTier`]。松存紧取，与两个档位串
    /// 同一条纪律。
    ///
    /// **落点是这个文件，不是 `paired_peers.json`** —— `PeerStore::upsert` 会在
    /// 对端下次重连时抹掉非保留字段（本文件头逐条论证过），而那正是「我明明钉了
    /// tier1，重连之后又变回 auto」这一类现象的成因。
    #[serde(default = "auto_tier")]
    pub transport_tier: String,
    /// 与 `*_reset_from` 同义、同纪律：**绝不落盘**。
    #[serde(skip)]
    pub transport_tier_reset_from: Option<String>,
    /// What the **link turned out to be**, as opposed to what the user asked
    /// for. `None` = never observed, or the observation has been retired for a
    /// re-probe. Only ever `Some("tier1")` (see [`PeerTransport::auto_tier`]).
    ///
    /// # This field exists because the alternative is a lie
    ///
    /// The obvious implementation of automatic downgrade is to write `tier1`
    /// into `transport_tier` and be done. That silently converts the sentence
    /// "let the daemon decide" into the sentence "I, the user, pinned tier 1" —
    /// and it is not reversible, because after the write nothing on this
    /// machine remembers that AUTO was ever selected. The user would then have
    /// to un-pin, by hand, a pin they never made, on a link that has since
    /// recovered.
    ///
    /// So the two are stored separately and combined only at the point of use
    /// ([`PeerTransport::effective_tier`]). `plan.md` §16.4 rule 5 and the
    /// contract note on `PeerTransportView::tier` both spell this out: the
    /// choice and the state of the link **must not impersonate each other**.
    ///
    /// # Why it is persisted
    ///
    /// design §5.1: without it every connection to a peer behind a UDP-blocking
    /// firewall replays the detection delay — 600 ms of silence before any
    /// sound, every single time. Persisting turns that into a one-off.
    #[serde(default)]
    pub auto_tier: Option<String>,
    /// Why the detector reached that verdict, in the words the UI shows.
    /// Carried with the tier so a downgrade is never a bare fact with nobody
    /// able to say what caused it (design §5.2: the reason string is where the
    /// per-direction asymmetry of *detection* goes, since the tier itself is
    /// per peer).
    #[serde(default)]
    pub auto_tier_reason: Option<String>,
    /// When it was reached, unix seconds. Drives the low-frequency re-probe
    /// (design §5.1 "后台低频重探 UDP") and is shown next to the reason.
    ///
    /// Unix seconds rather than an `Instant` because it outlives the process:
    /// an observation made three restarts ago is exactly the one that most
    /// needs retiring.
    #[serde(default)]
    pub auto_tier_since: Option<u64>,
    /// `both | outbound_only | inbound_only`，见 [`DialPolicy`]。同一条松存紧取
    /// 纪律，同一个落点理由。
    #[serde(default = "both_dial")]
    pub dial_policy: String,
    #[serde(skip)]
    pub dial_policy_reset_from: Option<String>,
    /// P6: a URL-shaped address for this peer (`ws://host[:port][/path]`),
    /// empty when the peer is reached by `host:port`.
    ///
    /// # Why the address decides the transport, and why it lives here
    ///
    /// plan §16.2: tier 2 is manual **because its premise cannot be observed**
    /// — a peer behind an L7-only tunnel and a peer that is switched off look
    /// identical from here. So the decision is delegated to the one party who
    /// does know, in the form they already have to supply: the address. A URL
    /// says "reach me by WebSocket" the way `192.168.1.5:47810` says "reach me
    /// directly", and neither needs a second setting to agree with.
    ///
    /// It sits in this file rather than in `paired_peers.json` for the reason
    /// the file header gives: `PeerStore::upsert` rebuilds a record from the
    /// wire and would erase it at the peer's next reconnect — and a peer whose
    /// tunnel address vanishes on reconnect is exactly the "I pinned it and it
    /// came back auto" complaint this file exists to prevent.
    ///
    /// Loose on the way in like everything else here: an unparseable URL is
    /// reset and reported rather than left in place to fail at dial time.
    #[serde(default)]
    pub endpoint: String,
    #[serde(skip)]
    pub endpoint_reset_from: Option<String>,
}

fn auto_tier() -> String {
    TransportTier::Auto.as_wire().to_string()
}

fn both_dial() -> String {
    DialPolicy::Both.as_wire().to_string()
}

impl Default for PeerTransport {
    fn default() -> Self {
        PeerTransport {
            recv: StoredDir::default(),
            send: StoredDir::default(),
            transport_tier: auto_tier(),
            transport_tier_reset_from: None,
            auto_tier: None,
            auto_tier_reason: None,
            auto_tier_since: None,
            dial_policy: both_dial(),
            dial_policy_reset_from: None,
            endpoint: String::new(),
            endpoint_reset_from: None,
        }
    }
}

impl PeerTransport {
    pub(crate) fn tier(&self) -> TransportTier {
        TransportTier::parse(&self.transport_tier).unwrap_or(TransportTier::Auto)
    }

    /// The tier the detector settled on, if any.
    ///
    /// Narrowed to tier 1 on the way out, not merely on the way in: tier 2's
    /// premise cannot be observed (plan §16.2 — an L7-only tunnel and a
    /// switched-off machine produce the same failed dial), so a stored
    /// `"tier2"` here can only have come from a hand-edited file or from a
    /// future version whose meaning this build does not know. Either way,
    /// executing it would silently reroute a peer's media through a transport
    /// nobody selected.
    pub(crate) fn auto_tier(&self) -> Option<TransportTier> {
        match TransportTier::parse(self.auto_tier.as_deref()?) {
            Some(TransportTier::Tier1) => Some(TransportTier::Tier1),
            _ => None,
        }
    }

    /// **The one place the user's choice and the link's state are combined.**
    ///
    /// `Auto` is the only value that defers: it means "you decide", so the
    /// observation gets to speak. Every other value is a pin, and a pin beats
    /// an observation — including `Tier0`, where the user is explicitly saying
    /// "do not fall back", and where honouring the detector would be the
    /// setting failing to do the single thing it exists for.
    ///
    /// Nothing else in the daemon may reconstruct this expression. Two copies
    /// of a precedence rule are how one of them ends up inverted on a path that
    /// only runs on a broken link — which is the least tested path there is.
    pub(crate) fn effective_tier(&self) -> TransportTier {
        match self.tier() {
            TransportTier::Auto => self.auto_tier().unwrap_or(TransportTier::Tier0),
            pinned => pinned,
        }
    }

    pub(crate) fn dial_policy(&self) -> DialPolicy {
        DialPolicy::parse(&self.dial_policy).unwrap_or(DialPolicy::Both)
    }

    /// Record a detector verdict. The three fields move together — a tier with
    /// no reason is a downgrade nobody can explain, and a tier with no
    /// timestamp can never be retired for a re-probe.
    pub(crate) fn set_auto_tier(&mut self, tier: TransportTier, reason: &str, now_unix: u64) {
        self.auto_tier = Some(tier.as_wire().to_string());
        self.auto_tier_reason = Some(reason.to_string());
        self.auto_tier_since = Some(now_unix);
    }

    /// Retire the observation, so the next connection starts from tier 0 again.
    pub(crate) fn clear_auto_tier(&mut self) {
        self.auto_tier = None;
        self.auto_tier_reason = None;
        self.auto_tier_since = None;
    }

    /// Same shape as [`StoredDir::sanitize`], and for the same reason: an
    /// unexecutable string must not sit in memory pretending to be a setting,
    /// and the reset must not be silent.
    fn sanitize(&mut self) {
        self.recv.sanitize();
        self.send.sanitize();
        if TransportTier::parse(&self.transport_tier).is_none() {
            self.transport_tier_reset_from =
                Some(std::mem::replace(&mut self.transport_tier, auto_tier()));
        }
        if DialPolicy::parse(&self.dial_policy).is_none() {
            self.dial_policy_reset_from =
                Some(std::mem::replace(&mut self.dial_policy, both_dial()));
        }
        // An observation this build cannot execute is simply discarded, and —
        // uniquely on this struct — **without** a `*_reset_from` companion.
        //
        // Every other field here is the user's choice, where a silent reset
        // destroys something only the user can recreate, so the reset has to be
        // reported. This one is our own measurement of a link, and the detector
        // makes it again 600 ms into the next stream. Carrying "your stored
        // observation was unreadable" out to the interface would put a
        // permanent notice on screen about a value the user never entered and
        // cannot influence.
        if self.auto_tier.is_some() && self.auto_tier().is_none() {
            self.clear_auto_tier();
        }
        if !self.endpoint.is_empty() && crate::wsshell::WsUrl::parse(&self.endpoint).is_err() {
            self.endpoint_reset_from = Some(std::mem::replace(&mut self.endpoint, String::new()));
        }
    }
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
                crate::dlog!(
                    "[audiohubd] {} 读不出来（{e}）；全部对端按默认档位跑",
                    path.display()
                );
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
                    t.sanitize();
                    if let Some(old) = &t.transport_tier_reset_from {
                        crate::dlog!(
                            "[audiohubd] peer_transport.json {fp}: 连通性档 `{old}` \
                             本 build 不认识，已重置为 auto"
                        );
                    }
                    if let Some(old) = &t.dial_policy_reset_from {
                        crate::dlog!(
                            "[audiohubd] peer_transport.json {fp}: 拨号策略 `{old}` \
                             本 build 不认识，已重置为 both"
                        );
                    }
                    if let Some(old) = &t.endpoint_reset_from {
                        crate::dlog!(
                            "[audiohubd] peer_transport.json {fp}: 对端地址 `{old}` \
                             解析不出来，已清空（该对端改按 host:port 拨号）"
                        );
                    }
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
        let body = serde_json::to_vec_pretty(&OnDisk {
            version: FILE_VERSION,
            peers: &self.map,
        })?;
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

    /// [`PeerTransportStore::get`]'s honest twin: `None` when there is no
    /// record, instead of a default that is indistinguishable from one.
    ///
    /// Every *executing* caller wants `get` — "never set" and "set to auto" run
    /// the same. Every *reporting* caller wants this one, because "we have
    /// never observed this link" and "we observed it and it was fine" are
    /// different sentences and the interface has to be able to tell them apart.
    pub(crate) fn peek(&self, fp: &str) -> Option<PeerTransport> {
        self.map.get(fp).cloned()
    }

    pub(crate) fn set(&mut self, fp: &str, t: PeerTransport) {
        self.map.insert(fp.to_string(), t);
    }

    /// The connectivity tier for a peer we may never have heard of. Same
    /// contract as [`PeerTransportStore::get`]: callers need something
    /// executable, and "never set" executes the same as "set to auto".
    pub(crate) fn tier(&self, fp: &str) -> TransportTier {
        self.map
            .get(fp)
            .map_or(TransportTier::Auto, PeerTransport::tier)
    }

    /// The tier to actually **run** on: [`PeerTransport::effective_tier`] for a
    /// peer we may never have heard of.
    ///
    /// Read by the dialling path. Deliberately *not* read by `on_request` /
    /// `on_ticket`, which decide whether to let a peer attach: those two ask
    /// [`PeerTransportStore::tier`] because the question there is "did this
    /// machine's user forbid tier 1", and a peer under AUTO that has not
    /// observed anything yet must still grant an attach the peer asks for. Ask
    /// the effective tier there and a symmetric pair of AUTO machines refuses
    /// each other forever: each one is nominally tier 0, so each one says no,
    /// and no downgrade can ever complete.
    pub(crate) fn effective_tier(&self, fp: &str) -> TransportTier {
        self.map
            .get(fp)
            .map_or(TransportTier::Tier0, PeerTransport::effective_tier)
    }

    /// Record a detector verdict for `fp`. Returns `true` when it changed
    /// something, so callers can skip the disk write and the peer notification
    /// on a re-detection of what is already stored.
    pub(crate) fn note_auto_tier(
        &mut self,
        fp: &str,
        tier: TransportTier,
        reason: &str,
        now_unix: u64,
    ) -> bool {
        let e = self.map.entry(fp.to_string()).or_default();
        if e.auto_tier() == Some(tier) {
            return false;
        }
        e.set_auto_tier(tier, reason, now_unix);
        true
    }

    /// Retire a stale observation so the next connection re-probes UDP.
    /// Returns `true` when there was one to retire.
    pub(crate) fn clear_auto_tier(&mut self, fp: &str) -> bool {
        match self.map.get_mut(fp) {
            Some(e) if e.auto_tier.is_some() => {
                e.clear_auto_tier();
                true
            }
            _ => false,
        }
    }

    /// Same contract again: a peer nobody has configured dials both ways, which
    /// is what every peer did before this setting existed.
    pub(crate) fn dial_policy(&self, fp: &str) -> DialPolicy {
        self.map
            .get(fp)
            .map_or(DialPolicy::Both, PeerTransport::dial_policy)
    }

    /// The peer's URL-shaped address, if it has one and it parses.
    ///
    /// Returns the parsed value rather than the string: every caller wants the
    /// host, the port and the path, and handing out the string would put a
    /// second parse (and a second set of accepted spellings) at each of them.
    pub(crate) fn endpoint(&self, fp: &str) -> Option<crate::wsshell::WsUrl> {
        let raw = self.map.get(fp).map(|t| t.endpoint.as_str()).unwrap_or("");
        if raw.is_empty() {
            return None;
        }
        crate::wsshell::WsUrl::parse(raw).ok()
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
                transport_tier: "tier1".into(),
                ..PeerTransport::default()
            },
        );
        s.save(&dir).expect("save");

        let back = PeerTransportStore::load(&dir);
        assert_eq!(back.get("aa11").recv.latency, "300");
        assert_eq!(back.get("aa11").send.latency, "100");
        assert_eq!(back.get("aa11").recv.quality, "pcm32k16");
        assert_eq!(
            back.tier("aa11"),
            TransportTier::Tier1,
            "the pinned tier did not survive"
        );
        // 没设过的对端拿到默认，不是恐慌也不是 None。
        assert_eq!(back.get("zz99"), PeerTransport::default());
        assert_eq!(back.tier("zz99"), TransportTier::Auto);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A record written before tier existed still loads, and reads as `auto`.
    ///
    /// Not a formality: this file is on disk on every machine that has ever run
    /// a build predating P3, and `serde` would otherwise fail the whole record
    /// and reset that peer's latency and quality — a downgrade feature silently
    /// undoing two settings that have nothing to do with it.
    #[test]
    fn a_record_written_before_the_tier_field_existed_still_loads() {
        let dir = tmpdir("notier");
        let raw = r#"{
          "version": 1,
          "peers": {
            "aa11": { "recv": { "latency": "300", "quality": "auto" },
                      "send": { "latency": "auto", "quality": "auto" } }
          }
        }"#;
        std::fs::write(PeerTransportStore::path(&dir), raw).expect("write");

        let s = PeerTransportStore::load(&dir);
        assert_eq!(
            s.get("aa11").recv.latency,
            "300",
            "the neighbouring setting was lost"
        );
        assert_eq!(s.tier("aa11"), TransportTier::Auto);
        assert_eq!(
            s.get("aa11").transport_tier_reset_from,
            None,
            "absent is not corrupt"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An unrecognised tier resets to `auto` and says so — and takes nothing
    /// else with it.
    ///
    /// # This test used to name `tier2`, and that is worth recording
    ///
    /// P3 wrote it as "`tier2` is designed and not built", which was true then
    /// and became **an assertion that a legal value must be refused** the
    /// moment P5 built it — green the whole time, right up until the parser
    /// learned the value. Identical in shape to `parse_rejects_bad_kind`
    /// asserting `Kind` 5 was illegal until `Kind::Control` took 5, which the
    /// P2 record notes had *already* happened once to `Codec` 3.
    ///
    /// So the subject is now a string that cannot become a tier, and the
    /// whole-space freeze lives in `the_tier_spellings_round_trip` where a new
    /// tier makes it fail loudly instead of silently.
    #[test]
    fn an_unrecognised_tier_is_reset_and_reported() {
        let mut t = PeerTransport {
            recv: StoredDir {
                latency: "300".into(),
                ..StoredDir::default()
            },
            transport_tier: "tier-of-the-week".into(),
            ..PeerTransport::default()
        };
        t.sanitize();
        assert_eq!(
            t.transport_tier, "auto",
            "an unbuildable tier was left in place"
        );
        assert_eq!(
            t.transport_tier_reset_from.as_deref(),
            Some("tier-of-the-week"),
            "the tier was reset silently; the UI has nothing to explain it with"
        );
        assert_eq!(t.tier(), TransportTier::Auto);
        assert_eq!(
            t.recv.latency, "300",
            "a valid neighbouring cell was collateral damage"
        );
    }

    /// The tier's reset marker never reaches disk, same as the other two.
    #[test]
    fn the_tier_reset_marker_is_not_persisted() {
        let dir = tmpdir("notiermark");
        let mut s = PeerTransportStore::default();
        let mut t = PeerTransport {
            transport_tier: "tier9".into(),
            ..PeerTransport::default()
        };
        t.sanitize();
        s.set("aa11", t);
        s.save(&dir).expect("save");

        let raw = std::fs::read_to_string(PeerTransportStore::path(&dir)).expect("read");
        assert!(
            !raw.contains("reset_from"),
            "the reset marker was written to disk"
        );
        assert!(
            !raw.contains("tier9"),
            "the unrecognised tier was written back out"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The wire spellings round-trip, and **the set of them is frozen**.
    ///
    /// Whole-space, in the shape `packet.rs`'s `the_kind_byte_values_are_frozen`
    /// settled on: the previous version pinned three values and separately
    /// asserted that `tier2` was refused, which meant P5 had to delete an
    /// assertion to add a tier — and deleting an assertion is not a review
    /// step anybody notices. This version goes red on *any* change to the set,
    /// which is right: a new tier needs a stored string, a UI sentence
    /// (plan §16.4 rule 2 forbids showing the code name) and a decision about
    /// what `auto` may promote to. Failing here is how those get remembered.
    #[test]
    fn the_tier_spellings_round_trip() {
        let all = [
            (TransportTier::Auto, "auto"),
            (TransportTier::Tier0, "tier0"),
            (TransportTier::Tier1, "tier1"),
            (TransportTier::Tier2, "tier2"),
        ];
        for (t, wire) in all {
            assert_eq!(
                t.as_wire(),
                wire,
                "a stored tier string changed under the file on disk"
            );
            assert_eq!(TransportTier::parse(wire), Some(t));
        }
        for other in ["", "tier3", "TIER1", "udp", "mux", " tier1"] {
            assert_eq!(
                TransportTier::parse(other),
                None,
                "'{other}' parsed as a tier; the accepted set is exactly {:?}",
                all.map(|(_, w)| w)
            );
        }
    }

    /// The same freeze for the dial policy, and for the same reason: each of
    /// these three strings is a different sentence in the UI, and
    /// `inbound_only` in particular is the one that decides whether a peer is
    /// drawn as "offline" or as "waiting to be connected to".
    #[test]
    fn the_dial_policy_spellings_round_trip() {
        let all = [
            (DialPolicy::Both, "both", true),
            (DialPolicy::OutboundOnly, "outbound_only", true),
            (DialPolicy::InboundOnly, "inbound_only", false),
        ];
        for (p, wire, may_dial) in all {
            assert_eq!(p.as_wire(), wire);
            assert_eq!(DialPolicy::parse(wire), Some(p));
            assert_eq!(
                p.may_dial(),
                may_dial,
                "'{wire}' decides the wrong way about dialling"
            );
        }
        for other in ["", "inbound", "outbound", "none", "InboundOnly"] {
            assert_eq!(
                DialPolicy::parse(other),
                None,
                "'{other}' parsed as a dial policy"
            );
        }
    }

    /// An unrecognised dial policy resets to `both` and says so, and does not
    /// take the tier beside it down.
    ///
    /// `both` is the right fallback and not merely the default: it is what
    /// every peer did before this setting existed, so a corrupt value degrades
    /// to the previous behaviour rather than to "never dial this peer again" —
    /// which would be indistinguishable, from the user's side, from the peer
    /// having disappeared.
    #[test]
    fn an_unrecognised_dial_policy_is_reset_and_reported() {
        let mut t = PeerTransport {
            transport_tier: "tier2".into(),
            dial_policy: "sometimes".into(),
            ..PeerTransport::default()
        };
        t.sanitize();
        assert_eq!(t.dial_policy, "both");
        assert_eq!(t.dial_policy_reset_from.as_deref(), Some("sometimes"));
        assert_eq!(t.dial_policy(), DialPolicy::Both);
        assert_eq!(
            t.transport_tier, "tier2",
            "a valid neighbouring cell was collateral damage"
        );
    }

    /// A record written before `dial_policy` existed still loads and reads as
    /// `both`. Every machine that has run any earlier build has such a file.
    #[test]
    fn a_record_written_before_the_dial_policy_field_existed_still_loads() {
        let dir = tmpdir("nodial");
        let raw = r#"{
          "version": 1,
          "peers": {
            "aa11": { "recv": { "latency": "300", "quality": "auto" },
                      "send": { "latency": "auto", "quality": "auto" },
                      "transport_tier": "tier1" }
          }
        }"#;
        std::fs::write(PeerTransportStore::path(&dir), raw).expect("write");

        let s = PeerTransportStore::load(&dir);
        assert_eq!(s.dial_policy("aa11"), DialPolicy::Both);
        assert_eq!(
            s.tier("aa11"),
            TransportTier::Tier1,
            "the neighbouring setting was lost"
        );
        assert_eq!(
            s.get("aa11").dial_policy_reset_from,
            None,
            "absent is not corrupt"
        );
        assert_eq!(
            s.dial_policy("zz99"),
            DialPolicy::Both,
            "an unknown peer must still dial"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Tier 2 and the dial policy survive a round trip through the file
    /// together. They are set in one operation (`peers.set_tier`) and a peer
    /// that came back with only one of them applied would be in a combination
    /// the user never asked for.
    #[test]
    fn tier_two_and_its_dial_policy_survive_a_round_trip_together() {
        let dir = tmpdir("tier2rt");
        let mut s = PeerTransportStore::default();
        s.set(
            "bb22",
            PeerTransport {
                transport_tier: "tier2".into(),
                dial_policy: "inbound_only".into(),
                ..PeerTransport::default()
            },
        );
        s.save(&dir).expect("save");

        let back = PeerTransportStore::load(&dir);
        assert_eq!(back.tier("bb22"), TransportTier::Tier2);
        assert_eq!(back.dial_policy("bb22"), DialPolicy::InboundOnly);
        assert!(!back.dial_policy("bb22").may_dial());
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
        assert_eq!(
            d.quality, "auto",
            "stale id `pcm32k` was left in place or translated"
        );
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
        assert_eq!(
            d.latency_reset_from, None,
            "a valid stop was flagged as reset"
        );
        assert_eq!(
            d.quality_reset_from, None,
            "a valid stop was flagged as reset"
        );
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
        assert_eq!(
            t.recv.latency, "300",
            "a valid neighbouring cell was collateral damage"
        );
        assert_eq!(
            t.send.quality, "pcm48k24",
            "a valid cell in the other direction was reset"
        );
        assert_eq!(t.send.quality_reset_from, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The reset marker **never reaches disk**. Persisting it would leave a
    /// permanent note on a file the user has since fixed.
    #[test]
    fn the_reset_marker_is_not_persisted() {
        let dir = tmpdir("nomark");
        let mut s = PeerTransportStore::default();
        let mut d = StoredDir {
            quality: "pcm32k".into(),
            ..StoredDir::default()
        };
        d.sanitize();
        s.set(
            "aa11",
            PeerTransport {
                recv: d,
                ..PeerTransport::default()
            },
        );
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

    // ------------------------------------------- automatic downgrade (plan §16.2)

    /// **The red line** (`plan.md` §16.4 rule 5): an automatic verdict is a
    /// statement about the link and must never be written into the field that
    /// holds the user's choice.
    ///
    /// Why this is worth its own test rather than being left to the integration
    /// suite: the wrong implementation — `transport_tier = "tier1"` — makes
    /// every behavioural assertion about downgrading pass. Media moves to TCP,
    /// audio comes back, the link is repaired. The only thing that breaks is
    /// that the user's "let the daemon decide" has silently become "I pinned
    /// tier 1", irreversibly, because after the write nothing remembers
    /// otherwise. Nothing observable about the audio can catch that.
    #[test]
    fn an_automatic_verdict_does_not_touch_the_users_choice() {
        let mut s = PeerTransportStore::default();
        assert!(s.note_auto_tier("aa11", TransportTier::Tier1, "no inbound UDP", 1_000));
        let t = s.get("aa11");
        assert_eq!(
            t.transport_tier, "auto",
            "the detector overwrote the user's setting; AUTO is now indistinguishable from a pin"
        );
        assert_eq!(
            t.tier(),
            TransportTier::Auto,
            "the choice must still read as AUTO"
        );
        assert_eq!(
            t.effective_tier(),
            TransportTier::Tier1,
            "...while the tier actually run is the one that was observed"
        );
        assert_eq!(t.auto_tier_reason.as_deref(), Some("no inbound UDP"));
        assert_eq!(t.auto_tier_since, Some(1_000));
    }

    /// A pin beats an observation, in **both** directions — including the one
    /// that matters most: `tier0` means "do not fall back", and a detector that
    /// overrode it would be the setting failing at the single job it has.
    #[test]
    fn a_pinned_tier_beats_a_verdict_in_both_directions() {
        let mut s = PeerTransportStore::default();
        s.note_auto_tier("pinned0", TransportTier::Tier1, "no inbound UDP", 1);
        let mut t = s.get("pinned0");
        t.transport_tier = "tier0".into();
        s.set("pinned0", t);
        assert_eq!(
            s.effective_tier("pinned0"),
            TransportTier::Tier0,
            "a verdict overrode an explicit tier 0 pin — the one setting whose entire meaning \
             is 'do not do that'"
        );
        // ...and the observation is still *recorded*, because "your link has no
        // UDP and you told me not to fall back" is the one sentence the
        // interface needs here.
        assert_eq!(s.get("pinned0").auto_tier(), Some(TransportTier::Tier1));

        // The other direction: pinned to tier 1 with nothing ever observed.
        let mut t = PeerTransport::default();
        t.transport_tier = "tier1".into();
        s.set("pinned1", t);
        assert_eq!(s.effective_tier("pinned1"), TransportTier::Tier1);
        assert_eq!(
            s.get("pinned1").auto_tier(),
            None,
            "a pin is not an observation"
        );
    }

    /// An unknown peer runs tier 0, and "never observed" is not stored as
    /// "observed to be tier 0".
    #[test]
    fn a_peer_nobody_has_observed_runs_tier_zero_and_says_so() {
        let s = PeerTransportStore::default();
        assert_eq!(s.effective_tier("nobody"), TransportTier::Tier0);
        assert_eq!(
            s.peek("nobody"),
            None,
            "`peek` must not invent a record to report"
        );
        assert_eq!(s.get("nobody").auto_tier(), None);
    }

    /// Only tier 1 is reachable without a human (plan §16.2: tier 2's premise
    /// cannot be observed). A stored `tier2` — hand-edited, or written by a
    /// future version — must not silently reroute a peer's media through a
    /// transport nobody selected.
    #[test]
    fn a_verdict_may_only_ever_name_tier_one() {
        for bogus in ["tier2", "tier0", "auto", "tier-of-the-week"] {
            let mut t = PeerTransport::default();
            t.auto_tier = Some(bogus.to_string());
            assert_eq!(
                t.auto_tier(),
                None,
                "`{bogus}` was accepted as an automatic verdict"
            );
            assert_eq!(
                t.effective_tier(),
                TransportTier::Tier0,
                "`{bogus}` steered the link even though it was refused as a verdict"
            );
            // ...and it is dropped rather than left on disk for ever.
            t.sanitize();
            assert_eq!(
                t.auto_tier, None,
                "the unexecutable `{bogus}` is still stored"
            );
        }
    }

    /// 「降了就记住」: the verdict survives a restart, or the 600 ms of silence
    /// is paid again on every single connection to a firewalled peer.
    #[test]
    fn a_verdict_survives_a_round_trip_through_the_file() {
        let dir = tmpdir("verdict");
        let mut s = PeerTransportStore::default();
        s.note_auto_tier(
            "aa11",
            TransportTier::Tier1,
            "no inbound UDP media",
            1_700_000_000,
        );
        s.save(&dir).expect("save");
        let back = PeerTransportStore::load(&dir);
        assert_eq!(
            back.effective_tier("aa11"),
            TransportTier::Tier1,
            "the verdict did not survive; every reconnect replays the detection silence"
        );
        let t = back.get("aa11");
        assert_eq!(
            t.transport_tier, "auto",
            "the user's choice was rewritten on the way out"
        );
        assert_eq!(t.auto_tier_reason.as_deref(), Some("no inbound UDP media"));
        assert_eq!(t.auto_tier_since, Some(1_700_000_000));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Re-detecting what is already stored is not a change, and the caller uses
    /// that to avoid rewriting the file and re-announcing to the peer several
    /// times a second for as long as the link stays broken.
    #[test]
    fn re_detecting_the_same_verdict_reports_no_change() {
        let mut s = PeerTransportStore::default();
        assert!(s.note_auto_tier("aa11", TransportTier::Tier1, "first", 1));
        assert!(
            !s.note_auto_tier("aa11", TransportTier::Tier1, "second", 2),
            "an unchanged verdict reported a change; the announcement would repeat every sweep"
        );
        assert_eq!(
            s.get("aa11").auto_tier_reason.as_deref(),
            Some("first"),
            "the original reason and timestamp are what the user has been looking at"
        );
        assert!(s.clear_auto_tier("aa11"));
        assert!(
            !s.clear_auto_tier("aa11"),
            "clearing twice is not a second change"
        );
        assert_eq!(
            s.effective_tier("aa11"),
            TransportTier::Tier0,
            "retired means re-probe"
        );
    }
}
