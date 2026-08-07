use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

pub const CONTROL_MAX_FRAME: usize = 65536;

/// The byte transport the control stack runs on.
///
/// The frame codec below, the verify exchange in `pairing`, and `SecureChannel`
/// only ever needed `Read + Write` — except in three places where they reached
/// past the byte stream and touched the socket directly: `set_read_timeout`,
/// `set_nodelay` and `peer_addr`. Those three calls were the entire reason the
/// control stack was typed on [`TcpStream`], so they are collected here and
/// nowhere else. Everything above this trait is now transport-agnostic.
///
/// ## Why the read bound is a *deadline* and not a timeout
///
/// `SO_RCVTIMEO` is a property of a socket. The second implementation of this
/// trait (design §4: control frames sharing one connection with media, handed
/// out by a demultiplexing reader thread) has no socket to set it on — "how
/// long may this read block" is answered there by waiting on a condition
/// variable. A trait method spelled `set_read_timeout` would export an
/// implementation detail that only one implementor can honour, and the other
/// would have to fake it. "Do not block past this instant" is a question both
/// can answer, so that is what this trait asks.
pub trait ControlIo: Read + Write + Send {
    /// Reads must not block past `deadline`. `None` = no bound.
    ///
    /// Advisory in the same way `SO_RCVTIMEO` is: a read that hits the bound
    /// reports [`std::io::ErrorKind::WouldBlock`] or
    /// [`std::io::ErrorKind::TimedOut`] having consumed nothing, and the caller
    /// re-arms the deadline before the next read it wants bounded. It is not a
    /// cancellation: a read already blocked when the deadline moves is not
    /// required to notice.
    fn set_read_deadline(&mut self, deadline: Option<Instant>) -> std::io::Result<()>;

    /// The address of the other end, for transports that have one.
    ///
    /// Fallible on purpose. A multiplexed or in-memory transport has no peer
    /// address at all, and the honest answer there is an error rather than a
    /// synthetic `0.0.0.0:0` that reads like a measurement.
    fn peer_addr(&self) -> std::io::Result<SocketAddr>;

    /// Disable Nagle, where the transport has Nagle to disable.
    ///
    /// Best effort by contract: transports without it return `Ok(())`. Every
    /// caller on the control plane already ignores the result (`let _ = ...`),
    /// which is acceptable at ~1 Hz — the media plane will not have that
    /// latitude, and that is a separate change.
    fn set_nodelay(&mut self, nodelay: bool) -> std::io::Result<()>;
}

impl ControlIo for TcpStream {
    fn set_read_deadline(&mut self, deadline: Option<Instant>) -> std::io::Result<()> {
        // A socket can only express "per read call", so the deadline is
        // converted at the moment it is armed — which is exactly what the
        // callers used to compute inline.
        let timeout = deadline.map(|d| {
            // `set_read_timeout(Some(ZERO))` means "block forever" to the
            // option and is rejected outright on Unix, so an already-elapsed
            // deadline has to become the *shortest* bound the socket can
            // express rather than the longest.
            d.saturating_duration_since(Instant::now())
                .max(Duration::from_millis(1))
        });
        TcpStream::set_read_timeout(self, timeout)
    }

    fn peer_addr(&self) -> std::io::Result<SocketAddr> {
        TcpStream::peer_addr(self)
    }

    fn set_nodelay(&mut self, nodelay: bool) -> std::io::Result<()> {
        TcpStream::set_nodelay(self, nodelay)
    }
}

/// Peer-to-peer control protocol version, compared for **strict equality** on
/// every verify (see `pairing::check_protocol`). A mismatch refuses the
/// connection; it never edits the peer store.
///
/// ## Why equality, and why this exists at all now
///
/// Until plan §13 there was no peer version negotiation whatsoever — the
/// `Hello` variant below carries a `version` field that nothing has ever sent
/// or read. Adding message *variants* did not need one: `secure.rs` skips a
/// `SessionMsg` it cannot decode and keeps the connection, so new telemetry
/// degrades to "the peer never tells us" (that guarantee is documented on
/// `SessionMsg::Unpaired` and still holds).
///
/// **Mode advertisement is not that shape.** A build that predates §13 is
/// simultaneously a provider and a consumer, and it has no field in which to
/// say so. It would therefore list a machine sitting in mode A/B as usable,
/// open streams that get refused, and — the part that is not merely cosmetic —
/// it can still be the relay leg of the §13 cycle, because *its* half of the
/// exclusion does not exist. Skipping an unparseable `ModeState` would leave
/// that gap open and silent. Refusing the connection makes it loud, and the
/// fix (upgrade both ends) is the same either way.
///
/// Version 1 = the unversioned pre-§13 protocol. Peers of that vintage send no
/// `version` field at all, which `serde(default)` reads as [`VERSION_ABSENT`]
/// so the refusal can name the problem instead of failing as a parse error.
///
/// ## 版本 3：位深进质量阶梯（`docs/design-bitdepth-ladder.md`）
///
/// 线上格式从「s16 写死」变成 (采样率, 位深) 六档。**这一次必须升版本号**，
/// 理由不是「新增了能力」，而是**老对端会静默错解**：
///
/// - rung 1 的 `codec = 3`（`PcmS24le`）在 v2 里不存在 ⇒ 老对端 `Codec::from_u8`
///   返回 `BadCodec`，`handle_datagram` 无日志早退 ⇒ **全程静音、零诊断**。
/// - rung 0 的 `codec = 1`（`PcmF32le`）**在 v2 里就是个合法枚举值**，只是从没
///   有人发过它。v2 的收流路径一行都不看 `h.codec`，直接 `s16le_to_f32(&plain)`
///   ⇒ 960 B 的 f32 半帧被当成 480 个 s16 样本，**满长度垃圾帧直接进 mixer，
///   从用户的真实扬声器全音量放出来**。AEAD 帮不上忙：那个包是合法签名的。
///
/// 而这个方向**用户单方面就能触发**（`publish_targets` 用的是本机的质量档，
/// 从来没问过对端能不能解）。所以唯一的闸门就是握手时这一次严格相等比较。
///
/// ⚠ `packet.rs` 的 `Codec` 注释曾写「零线格式风险，老对端得到 `BadCodec`
/// 显式失败」——那句话**只对 codec 3 成立**，已在那里改正。
///
/// ## 版本 4：降级链路（M8 Tier 1，`docs/design-m8-fallback.md` 决定 C）
///
/// 判据仍然是那一条：**缺席不等于没数据，而等于一个危险的默认行为**。
///
/// - 一台 v3 对端不认识 [`ControlMsg::MediaAttach`] 与那两条
///   `SessionMsg::MediaAttach*`，于是它继续往一个（按前提）被封死的 UDP 洞里
///   发，而我们坐在 TCP 上等。两端都健康、屏幕全绿、**全程静音**。这与
///   `ModeState` 当初升版本的理由逐字相同。
/// - `Kind::Control = 5` / `Kind::MuxKeepalive = 6` 对 v3 是
///   `Kind::from_u8 → None` ⇒ `Header::parse` 失败 ⇒ `handle_datagram` 无日志
///   早退。同样是静默。
///
/// 位一次留够：这两个 `Kind` 在 P2 就已经加进枚举，但**「值存在」与「值会到达」
/// 是两件事**，只有后者能弄坏对端 —— 所以升版本的责任落在第一次把它们放上
/// socket 的那次改动（P3），不是定义它们的那次（P2）。
///
/// 安全网是 `check_protocol` 的严格相等：它把版本不匹配变成一条指名道姓的拒绝，
/// 且**不修改任何一侧的配对记录**。部署纪律因此是「同一窗口内两端全部重建」。
pub const PROTOCOL_VERSION: u32 = 4;

/// What a missing `version` field decodes to: a peer old enough to have no
/// version at all. Distinct from any real version so the refusal message can
/// say "upgrade it" rather than quoting a number the peer never sent.
pub const VERSION_ABSENT: u32 = 0;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlMsg {
    Hello {
        version: u32,
        name: String,
        fingerprint: String,
    },
    PairInit {
        spake_msg_b64: String,
        name: String,
        listen_port: u16,
    },
    PairResp {
        spake_msg_b64: String,
        name: String,
    },
    PairConfirmA {
        mac_b64: String,
        public_key_b64: String,
    },
    PairConfirmB {
        mac_b64: String,
        public_key_b64: String,
    },
    /// The initiator's opening frame, and the first place it can state its
    /// protocol version. `serde(default)` so a pre-§13 peer decodes as
    /// [`VERSION_ABSENT`] and gets a refusal that names the problem, instead of
    /// a bare "parse control message" that names nothing.
    VerifyHello {
        fingerprint: String,
        nonce_b64: String,
        #[serde(default)]
        version: u32,
    },
    /// The responder's first frame, and the mirror of the version above: both
    /// ends have to learn the other's, and this is the earliest frame in which
    /// the responder speaks.
    VerifyChallenge {
        nonce_b64: String,
        #[serde(default)]
        version: u32,
    },
    VerifyResponse {
        sig_b64: String,
        public_key_b64: String,
        name: String,
    },
    SecInit {
        eph_pub_b64: String,
        nonce_b64: String,
        sig_b64: String,
    },
    SecResp {
        eph_pub_b64: String,
        nonce_b64: String,
        sig_b64: String,
    },
    Enc {
        n: u64,
        data_b64: String,
    },
    /// The one and only frame a **tier 1 media connection** opens with
    /// (`docs/design-m8-fallback.md` decision A): a *second* TCP connection to
    /// the same address the control channel already reached, carrying media
    /// frames rather than a control stream.
    ///
    /// ## What the ticket does and does not authenticate
    ///
    /// It authenticates **attachment only** — "this socket belongs to that
    /// already-verified connection". The 32 random bytes are minted on the
    /// secure control channel, are single-use, and expire in ten seconds.
    ///
    /// The frames that follow are **not** covered by it: each one is still
    /// self-authenticating under `MediaCrypto`, exactly as the UDP datagram it
    /// is byte-identical to. Injected frames fail AEAD, are counted
    /// (`SessionStats.auth_failed`) and dropped. Making the ticket carry more
    /// weight than that would put a second, weaker authenticator in front of a
    /// stronger one, which is how the weaker one ends up being the one that
    /// matters.
    ///
    /// ## …but attaching is not a write-only privilege, so the address is
    /// checked too
    ///
    /// This paragraph replaces an earlier one that said a stolen ticket bought
    /// nothing but "bytes that fail AEAD — the same thing they could already do
    /// by sending us UDP". That was wrong in a way worth recording, because
    /// everything downstream of it inherited the mistake. Attaching **replaces
    /// the connection's media path**: the holder receives that peer's entire
    /// media egress (the real peer then hears nothing, and the holder gets the
    /// ciphertext stream's timing and lengths, which name the bit-depth rung),
    /// and it can drop the control connection at will by closing its socket.
    ///
    /// So `tcpmedia::claim` also requires the source address to be the control
    /// peer's, for the same reason `handle_datagram` refuses a `PullReq` whose
    /// source is not `conn.peer_ip`. Defence in depth, not the door: a ticket
    /// only ever travels inside an established AEAD channel, so holding one
    /// already means that channel is compromised or the peer is hostile.
    MediaAttach {
        ticket_b64: String,
    },
    Ok {},
    /// "I am not paired with you any more." Sent instead of the generic
    /// `Error{unknown peer}` when a verify arrives from a fingerprint we do not
    /// know (spec-m5b OPEN QUESTION 5, plan §7.1 — the owner ruled this in).
    ///
    /// AUTHENTICATED, because acting on it DELETES A TRUST RELATIONSHIP. This
    /// frame answers `VerifyHello`, i.e. it arrives before either side has
    /// signed anything, so an empty version of it is a bare assertion by
    /// whoever happens to answer on the peer's address — a stale service that
    /// inherited the port, an attacker who can occupy or intercept it, or (as
    /// measured on 2026-07-31, when a peer record pointed at this daemon's own
    /// control port) the daemon itself. Any of them could make us silently
    /// destroy a pairing.
    ///
    /// The responder still knows its OWN key when it no longer knows ours, so
    /// it can prove who it is: `sig_b64` signs the initiator's nonce under a
    /// domain separator of its own (`pairing::UNPAIRED_LABEL`), and the
    /// initiator acts only when the key's fingerprint is the peer it meant to
    /// reach AND the signature holds against the key it has on file.
    ///
    /// Wire-compatible in the safe direction, both ways. `serde(default)` makes
    /// an old responder's empty `{"type":"unpaired"}` parse here with empty
    /// strings — unauthenticated, so the initiator keeps the pairing. In the
    /// other direction serde ignores unknown fields on a struct variant, so an
    /// old initiator reads a new responder's frame as the empty one it expects.
    Unpaired {
        #[serde(default)]
        sig_b64: String,
        #[serde(default)]
        public_key_b64: String,
    },
    Error {
        message: String,
    },
}

/// Generic over the sink rather than typed on [`TcpStream`]: the frame is four
/// little-endian length bytes followed by JSON, and nothing about that needs a
/// socket. See [`ControlIo`] for the three things that did.
pub fn write_frame<W: Write + ?Sized>(s: &mut W, msg: &ControlMsg) -> std::io::Result<()> {
    let body = serde_json::to_vec(msg)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    if body.len() > CONTROL_MAX_FRAME {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "control frame too large",
        ));
    }
    s.write_all(&(body.len() as u32).to_le_bytes())?;
    s.write_all(&body)?;
    s.flush()
}

/// Returns the decoded message, including `ControlMsg::Error` as-is (caller interprets).
///
/// How long this may block is the source's business, not this function's: on a
/// socket it is `SO_RCVTIMEO`, elsewhere it is whatever
/// [`ControlIo::set_read_deadline`] was last told.
pub fn read_frame<R: Read + ?Sized>(s: &mut R) -> Result<ControlMsg> {
    let mut len_bytes = [0u8; 4];
    s.read_exact(&mut len_bytes).context("read frame length")?;
    let len = u32::from_le_bytes(len_bytes) as usize;
    if len > CONTROL_MAX_FRAME {
        bail!("control frame too large: {len} bytes");
    }
    let mut body = vec![0u8; len];
    s.read_exact(&mut body).context("read frame body")?;
    serde_json::from_slice(&body).context("parse control message")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape `Unpaired` had before the refusal was signed. A peer running
    /// that build must survive a new responder's frame, so this asserts the
    /// property the wire compatibility rests on: serde ignores fields a struct
    /// variant does not declare (no `deny_unknown_fields` anywhere here).
    #[derive(Debug, Serialize, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    enum LegacyControlMsg {
        Unpaired {},
        Ok {},
    }

    #[test]
    fn a_signed_refusal_still_parses_where_the_frame_carried_no_fields() {
        let json = serde_json::to_vec(&ControlMsg::Unpaired {
            sig_b64: "c2ln".into(),
            public_key_b64: "a2V5".into(),
        })
        .expect("serialize");
        let old: LegacyControlMsg = serde_json::from_slice(&json)
            .expect("an old peer must still parse the frame, not crash on it");
        assert!(matches!(old, LegacyControlMsg::Unpaired {}));
    }

    /// The other direction: an old responder sends the bare frame, and the new
    /// initiator has to READ it (to log "unauthenticated, keeping the pairing")
    /// rather than fail at the parse. That is what `serde(default)` buys.
    #[test]
    fn an_old_refusal_parses_here_as_an_unsigned_one() {
        let msg: ControlMsg =
            serde_json::from_slice(br#"{"type":"unpaired"}"#).expect("parse legacy frame");
        match msg {
            ControlMsg::Unpaired { sig_b64, public_key_b64 } => {
                assert!(sig_b64.is_empty(), "no signature in the legacy frame");
                assert!(public_key_b64.is_empty(), "no key in the legacy frame");
            }
            other => panic!("expected Unpaired, got {other:?}"),
        }
    }
}
