pub const MAGIC: [u8; 4] = *b"AUHB";
pub const VERSION: u8 = 1;

/// 包头长度。逐字段：`MAGIC 4 + VERSION 1 + kind 1 + codec 1 + channels 1
/// + sample_rate 4 + session_id 8 + stream_id 4 + seq 4 + timestamp_us 8
/// + payload_len 4 = 40`。无保留位。
///
/// # 别再来砍这个包头（把这笔账算死，省下后人反复算）
///
/// 反复出现的想法是「砍掉几个字节，好让最深的档挤进一个不分片的数据报」。
/// 算术上它是**不可能**的：以太网 MTU 1500 − IP/UDP 28 − AEAD 标签 16 = 1456
/// 是密文预算；而 48 kHz × 24 bit × 10 ms = 1440 字节明文
/// ⇒ 包头必须 **≤ 16 字节**。
///
/// 16 字节的包头恰好是 `MAGIC 4 + VERSION 1 + kind 1 + codec 1 + channels 1
/// + stream_id 4 + seq 4` —— **一个字节都不剩，`timestamp_us` 必须整个删掉**。
/// 而 `timestamp_us` 是 transit / RFC 3550 抖动的**全部原料**
/// （`engine.rs` 的 `transit = arrival − timestamp_us`），删了它 AUTO 阶梯
/// 就没有升降判据了。
///
/// ⇒ **绑定约束是 10 ms 的包时长，不是包头。** 深档的解法是把线上包时长
/// 压到 5 ms（`FRAME_MS` 一个字不改，只动线路层；见
/// `docs/design-bitdepth-ladder.md` §1.B），不是砍包头。
///
/// 真要腾字节时**第一顺位是 `session_id`**：`engine.rs` 里它恒等于
/// `stream_id as u64`，8 字节纯重复。它排在 `timestamp_us` 前面。
pub const HEADER_LEN: usize = 40;

/// What a packet is. Byte 5 of the header, validated by [`Header::parse`], so a
/// value the reader does not know is a parse failure rather than something that
/// falls through.
///
/// # ⚠ Adding a value here is a wire-format change — check `PROTOCOL_VERSION`
///
/// Same discipline as [`Codec`] below, and the same failure mode. A peer that
/// predates the value gets `None` from [`Kind::from_u8`] ⇒ [`Header::parse`]
/// returns [`PacketError::BadKind`] ⇒ `handle_datagram` returns **with no log
/// at all**. Both ends healthy, both screens green, no audio and no diagnostic.
/// The only gate against that is the strict-equality version comparison at
/// handshake time (`control::PROTOCOL_VERSION`).
///
/// [`Kind::Control`] and [`Kind::MuxKeepalive`] exist for the framed transports
/// of `framed.rs`, and `PROTOCOL_VERSION` is deliberately **not** bumped for
/// them: nothing emits them yet, so no peer can receive one. **The bump belongs
/// to the change that first puts one of these on a socket**, not to the change
/// that names them — "the value exists" and "the value can arrive" are separate
/// events, and only the second can break a peer. Whoever wires the framed
/// transport up owns that bump.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Media = 0,
    EchoReq = 1,
    EchoResp = 2,
    PullReq = 3,
    Bye = 4,
    /// A slice of the control byte stream, carried on a framed transport.
    ///
    /// The payload is **not** one `ControlMsg`. It is a chunk of the existing
    /// `u32 length ‖ JSON` stream cut wherever the writer chose to cut it, so a
    /// message may span several frames and a frame may hold several messages.
    /// Reassembly belongs to the control reader, not to the frame layer.
    Control = 5,
    /// Keeps a framed transport alive when neither media nor control has
    /// anything to say. Carries no payload.
    MuxKeepalive = 6,
}

impl Kind {
    fn from_u8(v: u8) -> Option<Kind> {
        Some(match v {
            0 => Kind::Media,
            1 => Kind::EchoReq,
            2 => Kind::EchoResp,
            3 => Kind::PullReq,
            4 => Kind::Bye,
            5 => Kind::Control,
            6 => Kind::MuxKeepalive,
            _ => return None,
        })
    }
}

/// 线上编码。**对线性 PCM 而言位深就是编码方式**，所以位深住在这个字节里，
/// 不是包头里另一个字段。
///
/// # 为什么位深进 `codec` 而采样率留在 `sample_rate`
///
/// 这与 IETF 的切法完全同构：**位深是不同的 payload type**（`L16` 在 RFC
/// 3551/2586，`L20`/`L24` 在 RFC 3190，各有独立 MIME 注册），
/// 而**采样率是同一 payload type 内的 `rate` 参数**。
/// 「改采样率 = 改参数；改位深 = 换编码」。
///
/// 好处是 `HEADER_LEN` / `VERSION` / 40 字节布局一字不动。
///
/// # ⚠ 但这**不等于**「零跨版本风险」——两个新 codec 的失效形态完全不同
///
/// 这里曾写着「老对端收到 `codec = 3` 得到 `BadCodec` 并丢包，显式失败而不是
/// 静默错解」。那句话**只对 rung 1 成立**，逐条订正：
///
/// | rung | codec | 老对端（协议 v2）的行为 |
/// |---|---|---|
/// | 1 | `PcmS24le = 3` | v2 的 [`Codec::from_u8`] 不认识 3 ⇒ `BadCodec` ⇒ `handle_datagram` **无日志**早退 ⇒ 全程静音、零诊断。不是错解，但也谈不上「显式」。 |
/// | 0 | `PcmF32le = 1` | **v2 里这个枚举值就存在**，只是从没人发过。而 v2 的收流路径一行都不看 `codec`，直接 `s16le_to_f32(&plain)` ⇒ f32 半帧被当成两倍数量的 s16 样本，**满长度垃圾帧全音量进 mixer**。AEAD 挡不住：包是合法签名的。 |
///
/// ⇒ **唯一的闸门是握手时的协议版本严格相等比较**
/// （`control::PROTOCOL_VERSION`，位深进阶梯时已由 2 升到 3）。
/// 往 `Codec` 里加值时请连带检查那个常数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    /// 16 位有符号整数，小端。
    PcmS16le = 0,
    /// 32 位浮点，小端。枚举值一直在，位深进阶梯之后才首次真正上线。
    PcmF32le = 1,
    Opus = 2,
    /// 24 位有符号整数，**3 字节紧凑打包**，小端。
    /// ⚠ RFC 3190 的 `audio/L24` 用网络序，这一档与它字节序相反（见 `dsp.rs`）。
    PcmS24le = 3,
    Passthrough = 255,
}

impl Codec {
    fn from_u8(v: u8) -> Option<Codec> {
        Some(match v {
            0 => Codec::PcmS16le,
            1 => Codec::PcmF32le,
            2 => Codec::Opus,
            3 => Codec::PcmS24le,
            255 => Codec::Passthrough,
            _ => return None,
        })
    }

    /// 这个 codec 的线上位深。`None` = 不是线性 PCM（Opus / Passthrough）。
    ///
    /// **单向映射只写这一处。** 让别处（前端、遥测）各自复刻一份 codec → 位深
    /// 的对照表，两处一漂**没有任何地方会报错**——那正是 `wire_depth` 要做成
    /// 一等字段的同一条理由。
    pub fn wire_depth(self) -> Option<audiohub_core::dsp::WireDepth> {
        use audiohub_core::dsp::WireDepth;
        Some(match self {
            Codec::PcmS16le => WireDepth::S16,
            Codec::PcmS24le => WireDepth::S24,
            Codec::PcmF32le => WireDepth::F32,
            Codec::Opus | Codec::Passthrough => return None,
        })
    }

    /// 承载这个位深的 codec。与 [`Codec::wire_depth`] 互逆。
    pub fn for_depth(depth: audiohub_core::dsp::WireDepth) -> Codec {
        use audiohub_core::dsp::WireDepth;
        match depth {
            WireDepth::S16 => Codec::PcmS16le,
            WireDepth::S24 => Codec::PcmS24le,
            WireDepth::F32 => Codec::PcmF32le,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Header {
    pub kind: Kind,
    pub codec: Codec,
    pub channels: u8,
    pub sample_rate: u32,
    pub session_id: u64,
    pub stream_id: u32,
    pub seq: u32,
    pub timestamp_us: u64,
    pub payload_len: u32,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum PacketError {
    #[error("datagram too short")]
    TooShort,
    #[error("bad magic")]
    BadMagic,
    #[error("bad version")]
    BadVersion,
    #[error("bad kind")]
    BadKind,
    #[error("bad codec")]
    BadCodec,
    #[error("payload length mismatch")]
    LengthMismatch,
}

impl Header {
    pub fn encode(&self, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
        self.encode_into(payload, &mut out);
        out
    }

    /// [`Header::encode`] 的**零分配**形态：清空 `out` 并就地写入。
    ///
    /// `tx_loop` 每 tick 每流封一个包，那条线程上不许有 `malloc`
    /// （`docs/spec-latency-floor.md` §9.3 手段 J1）。字节序与字段顺序必须与
    /// `encode` 逐字相同 —— 两份实现就是两份线格式，所以 `encode` 改成调它。
    pub fn encode_into(&self, payload: &[u8], out: &mut Vec<u8>) {
        out.clear();
        self.encode_append(payload, out);
    }

    /// The same bytes as [`Header::encode_into`], **appended** rather than
    /// replacing what `out` already holds.
    ///
    /// Split out for the stream framing in [`crate::framed`], which puts
    /// several frames in one buffer and therefore cannot use the clearing form.
    /// It is a split, not a copy: two encoders would be two wire formats, and
    /// the field order below is the only one that exists.
    #[inline]
    pub fn encode_append(&self, payload: &[u8], out: &mut Vec<u8>) {
        out.reserve(HEADER_LEN + payload.len());
        out.extend_from_slice(&MAGIC);
        out.push(VERSION);
        out.push(self.kind as u8);
        out.push(self.codec as u8);
        out.push(self.channels);
        out.extend_from_slice(&self.sample_rate.to_le_bytes());
        out.extend_from_slice(&self.session_id.to_le_bytes());
        out.extend_from_slice(&self.stream_id.to_le_bytes());
        out.extend_from_slice(&self.seq.to_le_bytes());
        out.extend_from_slice(&self.timestamp_us.to_le_bytes());
        out.extend_from_slice(&self.payload_len.to_le_bytes());
        out.extend_from_slice(payload);
    }

    pub fn parse(datagram: &[u8]) -> Result<(Header, &[u8]), PacketError> {
        if datagram.len() < HEADER_LEN {
            return Err(PacketError::TooShort);
        }
        if datagram[0..4] != MAGIC {
            return Err(PacketError::BadMagic);
        }
        if datagram[4] != VERSION {
            return Err(PacketError::BadVersion);
        }
        let kind = Kind::from_u8(datagram[5]).ok_or(PacketError::BadKind)?;
        let codec = Codec::from_u8(datagram[6]).ok_or(PacketError::BadCodec)?;
        let channels = datagram[7];
        let le_u32 = |o: usize| u32::from_le_bytes(datagram[o..o + 4].try_into().unwrap());
        let le_u64 = |o: usize| u64::from_le_bytes(datagram[o..o + 8].try_into().unwrap());
        let payload_len = le_u32(36);
        if payload_len as usize != datagram.len() - HEADER_LEN {
            return Err(PacketError::LengthMismatch);
        }
        Ok((
            Header {
                kind,
                codec,
                channels,
                sample_rate: le_u32(8),
                session_id: le_u64(12),
                stream_id: le_u32(20),
                seq: le_u32(24),
                timestamp_us: le_u64(28),
                payload_len,
            },
            &datagram[HEADER_LEN..],
        ))
    }
}

#[cfg(test)]
mod codec_depth_tests {
    use super::*;
    use audiohub_core::dsp::WireDepth;

    /// codec ↔ 位深必须是一一对应，且**这是唯一一份映射**。
    ///
    /// 注入对照：把 `for_depth(S24)` 改成返回 `Codec::PcmS16le`，这条立刻变红。
    /// 没有它，那次改动的表现是「选了 24 bit，线上发的是 16 bit，包头写着
    /// PcmS16le，遥测据此报 s16」——**处处自洽，全都是错的**。
    #[test]
    fn every_pcm_codec_maps_onto_exactly_one_wire_depth_and_back() {
        for depth in [WireDepth::S16, WireDepth::S24, WireDepth::F32] {
            let c = Codec::for_depth(depth);
            assert_eq!(c.wire_depth(), Some(depth), "{depth:?} 的 codec 往返对不上");
        }
        // 非 PCM 的两个不该冒出一个位深来。
        assert_eq!(Codec::Opus.wire_depth(), None);
        assert_eq!(Codec::Passthrough.wire_depth(), None);
    }

    /// 三个 PCM codec 的线上字节值**冻结**，且 `PcmS24le` 是新加的那个。
    ///
    /// ⚠ 这条**保证不了跨版本安全**：`PcmF32le = 1` 在协议 v2 里就是合法值，
    /// 而 v2 的收流路径根本不看 `codec` ⇒ 它会把 f32 载荷按 s16 静默错解。
    /// 挡住这件事的是 `control::PROTOCOL_VERSION`（已升到 3），不是这条断言。
    ///
    /// Stated as the **whole** 0..=255 space, for the same reason as
    /// [`the_kind_byte_values_are_frozen`] below — and this test is where that
    /// reason was first learned. The unassigned side used to be four samples,
    /// `[4, 5, 100, 254]`, which leaves two holes: assigning codec 4 turns a
    /// sample into a claim that a *valid* codec must be rejected, and a
    /// `from_u8` that accepted some byte nobody sampled would go unnoticed
    /// entirely. Enumerating closes both, and makes assigning a codec fail here
    /// on purpose, next to the `PROTOCOL_VERSION` note above that such an edit
    /// has to revisit.
    #[test]
    fn the_codec_byte_values_are_frozen() {
        let assigned: [(u8, Codec); 5] = [
            (0, Codec::PcmS16le),
            (1, Codec::PcmF32le),
            (2, Codec::Opus),
            (3, Codec::PcmS24le),
            (255, Codec::Passthrough),
        ];
        assert_eq!(Codec::PcmS16le as u8, 0);
        assert_eq!(Codec::PcmF32le as u8, 1);
        assert_eq!(Codec::Opus as u8, 2);
        assert_eq!(Codec::PcmS24le as u8, 3, "改这个值 = 换线格式，两端会各说各话");
        assert_eq!(Codec::Passthrough as u8, 255);

        for (byte, want) in assigned {
            assert_eq!(Codec::from_u8(byte), Some(want), "codec byte {byte} must decode to {want:?}");
        }
        for bad in 0u8..=255 {
            if assigned.iter().any(|(b, _)| *b == bad) {
                continue;
            }
            assert_eq!(Codec::from_u8(bad), None, "unassigned codec byte {bad} must not be recognised");
        }
    }

    /// The on-wire byte for every packet class is frozen, `Control` and
    /// `MuxKeepalive` included.
    ///
    /// Injection control: change `Control` to 7 and this goes red. Without it,
    /// that edit ships a build whose framed transport is mutually unintelligible
    /// with every other build — and the symptom is `BadKind`, which
    /// `handle_datagram` drops without a log.
    ///
    /// Stated as the **whole** 0..=255 space, not as a handful of samples. That
    /// is not thoroughness for its own sake: `parse_rejects_bad_kind` in the
    /// out-of-tree suite held a bare "5 and 255 are not assigned", so the moment
    /// `Control` took 5 it was asserting that a valid kind must be rejected —
    /// green right up until the parser learned the value, then red for a reason
    /// that had nothing to do with the change. Sampling is what let a stale
    /// claim survive; enumerating is what makes assigning a kind fail here, on
    /// purpose, in front of the `PROTOCOL_VERSION` note on [`Kind`].
    #[test]
    fn the_kind_byte_values_are_frozen() {
        let assigned: [(u8, Kind); 7] = [
            (0, Kind::Media),
            (1, Kind::EchoReq),
            (2, Kind::EchoResp),
            (3, Kind::PullReq),
            (4, Kind::Bye),
            (5, Kind::Control),
            (6, Kind::MuxKeepalive),
        ];
        assert_eq!(Kind::Media as u8, 0);
        assert_eq!(Kind::EchoReq as u8, 1);
        assert_eq!(Kind::EchoResp as u8, 2);
        assert_eq!(Kind::PullReq as u8, 3);
        assert_eq!(Kind::Bye as u8, 4);
        assert_eq!(Kind::Control as u8, 5, "changing this splits the framed transport in two");
        assert_eq!(Kind::MuxKeepalive as u8, 6, "changing this splits the framed transport in two");

        for (byte, want) in assigned {
            assert_eq!(Kind::from_u8(byte), Some(want), "byte {byte} must decode to {want:?}");
        }
        for bad in 0u8..=255 {
            if assigned.iter().any(|(b, _)| *b == bad) {
                continue;
            }
            assert_eq!(Kind::from_u8(bad), None, "unassigned kind byte {bad} must not be recognised");
        }
    }

    /// 包头长度与逐字段之和一致；顺手把 §1.A 那笔 MTU 账钉成断言。
    #[test]
    fn the_header_is_forty_bytes_and_leaves_no_room_for_a_deeper_rung() {
        let h = Header {
            kind: Kind::Media,
            codec: Codec::PcmS24le,
            channels: 1,
            sample_rate: 48_000,
            session_id: 7,
            stream_id: 7,
            seq: 3,
            timestamp_us: 1234,
            payload_len: 0,
        };
        assert_eq!(h.encode(&[]).len(), HEADER_LEN);
        // 48 kHz × 24 bit × 10 ms = 1440 B 明文；1500 − 28(IP/UDP) − 16(AEAD)
        // = 1456 ⇒ 包头预算 16 字节。今天的包头是 40 ⇒ **装不下**，
        // 这正是深档要按 5 ms 分包的理由。这条断言把「装不下」钉住，
        // 免得有人以为砍几个字段就能塞进去。
        const MTU: usize = 1500;
        const IP_UDP: usize = 28;
        const AEAD_TAG: usize = 16;
        let ten_ms_48k_24bit = 480 * 3;
        assert!(
            HEADER_LEN + ten_ms_48k_24bit + AEAD_TAG + IP_UDP > MTU,
            "48k/24 的 10 ms 帧居然进得去一个数据报了——若真是包头变小了，\
             请重新读 HEADER_LEN 上方那段账：能塞下的包头是没有 timestamp_us 的那个"
        );
    }
}
