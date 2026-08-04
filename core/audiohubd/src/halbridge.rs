//! halbridge — daemon end of the macOS HAL bridge (spec-round2 §B1/§B2).
//!
//! The other end is `drivers/macos-hal/src/AudioHubBridge.{h,c}`; every
//! constant, struct layout and message id below is a hand-maintained mirror of
//! that header, and the `const` asserts at the bottom of each block are what
//! turn a drift into a build failure instead of into noise on the user's
//! speakers.
//!
//! # Direction (INVERTED 2026-07-30 — the daemon is the client now)
//!
//! The first design had the daemon own the mach name and the driver look it up.
//! That was measured wrong twice on real hardware:
//!
//! * daemon as a per-user LaunchAgent — coreaudiod runs as `_coreaudiod` in the
//!   SYSTEM bootstrap namespace and cannot resolve a `gui/<uid>` name, so the
//!   bridge never connected at all;
//! * daemon as a system-domain LaunchDaemon — the bridge connected, but every
//!   outbound LAN connect returned `EHOSTUNREACH`, because local-network consent
//!   is bound to a *user session* a system daemon does not have. Stable code
//!   signing and toggling the Local Network switch changed nothing.
//!
//! Both requirements cannot be met by one process on one side of that fence, so
//! the direction is reversed (this is what Rogue Amoeba's shipping ARK driver
//! does, verified live: `bootstrap_look_up("com.rogueamoeba.ARK.driver")` from
//! `gui/501` returns `kr=0`, and that name is NOT in `launchctl print gui/501`
//! — it is global, and bootstrap visibility is one-way):
//!
//! * the DRIVER calls `bootstrap_check_in` on ITS OWN name from inside
//!   coreaudiod, so the name lands in the GLOBAL namespace, and its bundle
//!   Info.plist declares that same name under `AudioServerPlugIn_MachServices`;
//! * this daemon is an ordinary USER-SESSION process again — full TCC identity,
//!   full LAN access, no sudo — and reaches the driver with `bootstrap_look_up`.
//!
//! # Shape of the exchange — THE DRIVER OWNS THE RINGS
//!
//! `HELLO` travels daemon -> driver carrying exactly ONE port descriptor, a
//! send right on our control port, and the driver answers with TWO memory-entry
//! ports it created itself. The daemon allocates no shared memory at all.
//!
//! That split is the driver's, and it buys a safety property this side cannot:
//! the plug-in creates both rings once and never unmaps them, so its IOProc —
//! which runs on coreaudiod's realtime thread and may not take a lock — can
//! never race an unmap. Only the per-ring publish flag moves at connect time.
//! The mirror-image race does exist HERE, because this side unmaps on every
//! disconnect; [`platform::Rings`] closes it with an `RwLock` that a detach
//! must win before any mapping goes away.
//!
//! Shared memory is conveyed as mach memory-entry PORTS, never as
//! `mach_msg_ool_descriptor_t`: an OOL descriptor delivers a virtual *copy* of
//! the pages, which would share nothing and is the exact failure mode that once
//! had the daemon reading zeroes while the driver's IO was demonstrably running.
//! `mach_vm_map` therefore takes `copy = FALSE` on both sides, and
//! `an_attached_ring_is_genuinely_shared_not_copied` below proves it end to end
//! against a driver-shaped entry, with no driver.
//!
//! Nothing about the ring layout is assumed from these constants: the reply
//! carries `data_offset` and the full geometry of both rings, that is what gets
//! mapped, and a geometry this daemon cannot consume is a named refusal rather
//! than a mapping read at the wrong offsets.
//!
//! After the handshake both directions are fire-and-forget: the driver posts
//! `CONTROL` to our control port, we post `NOTIFY` straight to the driver's
//! service port. A `NOTIFY_PING` on a modest cadence is what turns "coreaudiod
//! exited" into a `MACH_SEND_INVALID_DEST` we can act on.
//!
//! Ownership of the ring indices is the whole safety argument: `spk_ring` has
//! the driver as its only producer and this process as its only consumer,
//! `mic_ring` the reverse, and neither side ever writes the other's index. So:
//!
//! * exactly ONE daemon thread may call [`HalBridge::append_spk_frame`] /
//!   [`HalBridge::read_spk_mono`] (the tx engine), and
//! * exactly ONE may call [`HalBridge::write_mic_mono`] (the mixer).
//!
//! Calling either from two threads at once is not memory-unsafe, but it breaks
//! SPSC and will duplicate or lose frames.
//!
//! Not having a bridge is NOT an error: on a machine where the driver's name
//! does not resolve, `start` returns `Ok(None)` silently and the daemon behaves
//! exactly as it did before this module existed.

#![allow(dead_code)] // wiring into engine/ipcserv lands with the main session

use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;

#[cfg(target_os = "macos")]
use crate::{rd, wr};
use crate::{dlog, lk};

// ---------------------------------------------------------------- frozen contract

/// Mirrors `kAudioHubDriverMachServiceName`. The DRIVER checks this name in
/// from inside coreaudiod (global bootstrap namespace); we look it up.
pub const HAL_SERVICE_NAME: &str = "com.audiohub.driver";

pub const HAL_RING_MAGIC: u32 = 0x4148_5231; // 'AHR1'
pub const HAL_RING_VERSION: u32 = 1;
pub const HAL_RING_DATA_OFFSET: usize = 64;
pub const HAL_SAMPLE_RATE: u32 = 48_000;
pub const HAL_RING_MS: u32 = 500;
pub const HAL_RING_FRAMES: u32 = (HAL_SAMPLE_RATE / 1000) * HAL_RING_MS;
pub const HAL_SPK_CHANNELS: u32 = 2;
pub const HAL_MIC_CHANNELS: u32 = 1;

/// 10ms at 48k — the daemon's internal frame, and the chunk the ring helpers
/// work in so nothing on this path has to allocate.
pub const HAL_FRAME_48K: usize = 480;

/// 进程单调微秒，**归因埋点共用的时基**。
///
/// 「欠载是我们排空过头造成的，还是生产侧本来就没数据」这个问题只能靠**时刻**
/// 回答：孤立发生 ⇒ 生产侧；紧随一次排空 / trim / 重同步 ⇒ 我们的锅。所以
/// `drain_spk`、`try_trim`、`Dll::resync` 各自留一个「上次发生在什么时候」的
/// 戳，欠载那一行把它们的年龄一起打出来。
///
/// 只在**事件发生时**取（每次跳 tick / 每次 trim / 每段欠载的头尾），不在每
/// tick 上取：`Instant::now()` 在 macOS 上是 `mach_absolute_time`，几十纳秒，
/// 但音频路径上「便宜」不是不做的理由。
///
/// `0` 是保留值 = **从未发生过**，`age_ms` 因此要能区分「刚发生」与「没发生过」。
pub(crate) fn mono_us() -> u64 {
    static T0: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    // +1：保证返回值非零，于是 0 可以专门表示「从未发生过」。
    T0.get_or_init(Instant::now).elapsed().as_micros() as u64 + 1
}

/// 一个「上次发生在什么时候」的戳距今多少毫秒。`None` = 从未发生过。
pub(crate) fn age_ms(stamp_us: u64, now_us: u64) -> Option<f64> {
    (stamp_us != 0).then(|| now_us.saturating_sub(stamp_us) as f64 / 1000.0)
}

/// 把 [`age_ms`] 排成日志里那一列。`—` = 本进程从未发生过这件事。
pub(crate) fn age_str(stamp_us: u64, now_us: u64) -> String {
    match age_ms(stamp_us, now_us) {
        Some(ms) => format!("{ms:.0}ms前"),
        None => "—".to_string(),
    }
}

const fn page_align(n: usize) -> usize {
    (n + 16383) & !16383
}
const fn ring_bytes(channels: u32) -> usize {
    page_align(HAL_RING_DATA_OFFSET + (HAL_RING_FRAMES as usize) * (channels as usize) * 4)
}
/// What the driver is expected to report for each ring. Only a cross-check —
/// the mapping itself uses the sizes the reply carries.
pub const HAL_SPK_BYTES: usize = ring_bytes(HAL_SPK_CHANNELS);
pub const HAL_MIC_BYTES: usize = ring_bytes(HAL_MIC_CHANNELS);

/// One (out, in) ring pair per slot, `kAudioHubMaxSlots` in AudioHubBridge.h.
/// The driver creates them all up front and never releases one, so binding a
/// peer to a slot never has to interact with the realtime path (spec-m5b §1).
pub const HAL_MAX_SLOTS: usize = 16;
/// `kAudioHubMaxEndpoints`. A literal rather than `2 * HAL_MAX_SLOTS` because
/// it is a frozen wire quantity — it sizes the reply's descriptor array on both
/// sides — and the assertion below is what keeps the two facts consistent.
pub const HAL_MAX_ENDPOINTS: usize = 32;
const _: () = assert!(HAL_MAX_ENDPOINTS == 2 * HAL_MAX_SLOTS);

const MSG_HELLO: i32 = 0x4148_0001; // daemon -> driver, carries our control port only
const MSG_HELLO_REPLY: i32 = 0x4148_0002; // driver -> daemon, carries every memory entry
const MSG_CONTROL: i32 = 0x4148_0003; // driver -> daemon, fire and forget
const MSG_NOTIFY: i32 = 0x4148_0004; // daemon -> driver, fire and forget
const MSG_BIND: i32 = 0x4148_0005; // daemon -> driver, fire and forget

/// `kAudioHubProtocolVersion` in AudioHubBridge.h. The driver compares this for
/// EQUALITY and answers `kAudioHubStatus_BadVersion` on anything else, so it is
/// not a floor to be raised unilaterally — it changes only when that header
/// changes. (It was briefly 2 here alone, which the driver could only refuse.)
///
/// v2 is the per-peer device protocol (spec-m5b §4): the reply grew from 104 to
/// 472 bytes and the control message from 48 to 56, so the two versions cannot
/// be told apart by parsing — only by this number, before anything is parsed.
/// An installed v1 driver therefore refuses this daemon outright, which is the
/// intended loud failure; a shim that tried to speak both would be guessing at
/// which layout it is holding.
pub const PROTOCOL_VERSION: u32 = 2;

/// v1's `AudioHubHelloReply`: 24 header + 4 body + 2 descriptors + 52 payload.
/// Kept ONLY so a reply of exactly this size can be named as "an old driver is
/// installed" instead of being reported as an unrecognised message. Never parse
/// a v1 reply — the two layouts are indistinguishable past the header.
const HELLO_REPLY_V1_SIZE: usize = 104;

const CTL_VOLUME: u32 = 1;
const CTL_HEARTBEAT: u32 = 2;
const CTL_IO_STATE: u32 = 3;
/// The driver accepted another daemon's HELLO and is about to drop our port.
/// Sent as the LAST message on it, best-effort. Handled by detaching at once
/// instead of waiting out `DRIVER_SILENT_AFTER` — and by NOT reconnecting
/// immediately, because an instant re-HELLO displaces whoever displaced us and
/// the two daemons then trade the rings every few seconds forever.
const CTL_SUPERSEDED: u32 = 4;
/// The driver's account of one slot — `endpoint = slot*2`, `scalar_bits` a
/// `SLOT_*` state, `generation` that slot's stamp. This is what establishes the
/// generation every other control message is then filtered against, and what
/// makes publication closed-loop rather than fire-and-hope (spec-m5b §4.6).
const CTL_BIND_STATE: u32 = 5;

/// Slot states carried in `CTL_BIND_STATE`'s `scalar_bits`.
const SLOT_FREE: u32 = 0;
const SLOT_BOUND: u32 = 1;
const SLOT_DELISTED: u32 = 2;

const NOTIFY_VOLUME: u32 = 1;
const NOTIFY_PING: u32 = 2;

/// `hal.status_reason` for "a driver is installed but speaks another protocol".
/// The UI keys its third driver state off this exact string (spec-m5b §6.2),
/// and regression N14 asserts it, so it is a contract rather than a message.
pub const REASON_PROTOCOL_MISMATCH: &str = "driver_protocol_mismatch";

const BIND_CLEAR: u32 = 0;
const BIND_SET: u32 = 1;

/// The low bit of an endpoint. What used to be a one-bit device selector is now
/// `slot * 2 + dir`, and the direction stayed the LOW bit so slot 0 keeps v1's
/// numbering (`0` was the speaker, `1` the microphone).
const DIR_OUT: u32 = 0;
const DIR_IN: u32 = 1;

/// `endpoint = slot*2 + dir`, the u32 that names one of the 32 virtual devices.
const fn endpoint(slot: usize, dir: u32) -> u32 {
    (slot as u32) * 2 + dir
}
const fn endpoint_slot(ep: u32) -> usize {
    (ep / 2) as usize
}
const fn endpoint_dir(ep: u32) -> u32 {
    ep & 1
}

const STATUS_OK: u32 = 0;
const STATUS_BAD_VERSION: u32 = 1;
const STATUS_NO_MEMORY: u32 = 2;
/// The driver replaced the session this message quotes. Whoever sent it was
/// superseded and does not know yet.
const STATUS_STALE_SESSION: u32 = 3;
const STATUS_BAD_REQUEST: u32 = 4;

const FLAG_MUTED: u32 = 0x1;
const FLAG_IO_RUNNING: u32 = 0x2;
const FLAG_IS_INPUT: u32 = 0x4;

// ---------------------------------------------------------------- public API

/// One of the 32 virtual devices: which slot, and which direction of it.
/// `slot` is a DAEMON-INTERNAL index — it names a pair of rings, never anything
/// a user or an IPC client can select (spec-m5b §5.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct HalEndpoint {
    pub slot: u8,
    /// true = the virtual MICROPHONE (we write it), false = the virtual
    /// SPEAKER (we read it).
    pub input: bool,
}

impl HalEndpoint {
    pub fn out(slot: u8) -> HalEndpoint {
        HalEndpoint { slot, input: false }
    }
    pub fn mic(slot: u8) -> HalEndpoint {
        HalEndpoint { slot, input: true }
    }
    fn from_wire(v: u32) -> Option<HalEndpoint> {
        let slot = endpoint_slot(v);
        (slot < HAL_MAX_SLOTS).then(|| HalEndpoint {
            slot: slot as u8,
            input: endpoint_dir(v) == DIR_IN,
        })
    }
    fn to_wire(self) -> u32 {
        endpoint(self.slot as usize, if self.input { DIR_IN } else { DIR_OUT })
    }
}

/// The driver's account of one slot, carried by `CTL_BIND_STATE`. This is the
/// ONLY thing that establishes a slot's generation, and therefore the only
/// thing that makes any other control message from that slot acceptable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HalSlotState {
    Free,
    Bound,
    Delisted,
}

impl HalSlotState {
    fn from_wire(v: u32) -> Option<HalSlotState> {
        match v {
            SLOT_FREE => Some(HalSlotState::Free),
            SLOT_BOUND => Some(HalSlotState::Bound),
            SLOT_DELISTED => Some(HalSlotState::Delisted),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            HalSlotState::Free => "free",
            HalSlotState::Bound => "bound",
            HalSlotState::Delisted => "delisted",
        }
    }
}

/// What the driver tells us about its virtual devices (spec-m5b §4.6).
///
/// Every variant that concerns a slot carries that slot's `generation`, and the
/// receive path has ALREADY dropped anything whose stamp does not match the one
/// the last `BindState` established — a late `StopIO` from the previous tenant
/// of a slot must never light up the next peer's microphone.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HalControlEvent {
    /// The local user moved the virtual device's slider. The daemon must relay
    /// this to the peer's REAL device via the existing `VolumeSet` path.
    Volume { at: HalEndpoint, generation: u32, scalar: f32, muted: bool },
    /// An application started/stopped using the virtual device. In mode B this
    /// is the ONLY signal that opens a session (spec-m5b §5.6).
    IoState { at: HalEndpoint, generation: u32, running: bool },
    /// A slot changed state, or answered an idempotent `Bind`.
    BindState { slot: u8, generation: u32, state: HalSlotState },
    /// A handshake completed. Everything this daemon still intends must be
    /// re-`Set` after this — the driver replays a slot's IO state and volume
    /// only when an idempotent Set lands on it (spec-m5b §1, third trade-off),
    /// so skipping the re-Set leaves an app that was mid-recording recording
    /// silence with no error anywhere.
    Attached { session_id: u64, slot_count: u8 },
    /// The rings are gone (driver exited, superseded, went silent). Bindings
    /// survive on the driver's side; sessions do not.
    Detached,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HalBridgeMode {
    /// Build a bridge only on a machine whose driver is actually there — the
    /// name has to resolve once at startup. Everywhere else this is `Ok(None)`
    /// and the daemon runs exactly as it did before this module existed.
    ///
    /// The old build gated on a LaunchAgent marker env var instead, because
    /// `bootstrap_check_in` of an unknown name SUCCEEDS and would let any
    /// hand-run daemon steal the name. Looking a name up has no such hazard,
    /// so the marker is gone.
    Auto,
    /// Build the bridge even when the driver is not there yet and keep looking.
    /// Only a failure we cannot retry out of (no ports, no memory) is fatal.
    Require,
    /// Never touch mach at all.
    Off,
}

#[derive(Debug, Clone)]
pub struct HalBridgeCfg {
    pub service_name: String,
    pub mode: HalBridgeMode,
}

impl Default for HalBridgeCfg {
    fn default() -> Self {
        HalBridgeCfg { service_name: HAL_SERVICE_NAME.to_string(), mode: HalBridgeMode::Auto }
    }
}

impl HalBridgeCfg {
    /// `AUDIOHUB_HAL_BRIDGE` = `off` | `auto` (default) | `require`.
    pub fn from_env() -> HalBridgeCfg {
        let mode = match std::env::var("AUDIOHUB_HAL_BRIDGE").ok().as_deref() {
            Some("off") | Some("0") => HalBridgeMode::Off,
            Some("require") | Some("1") => HalBridgeMode::Require,
            _ => HalBridgeMode::Auto,
        };
        let service_name = std::env::var("AUDIOHUB_HAL_SERVICE")
            .unwrap_or_else(|_| HAL_SERVICE_NAME.to_string());
        HalBridgeCfg { service_name, mode }
    }
}

/// 主动 trim（`docs/spec-latency-trim.md` §10.1）的计数与三个当前读数。
///
/// **绝不并进 `dropped`。** 那个字段的语义是「饱和丢弃」，规格 §3.3 的三态诊断
/// （`dropped` 冻结 vs 增长）押在这个语义上；主动 trim 是我们自己删的，混进去
/// 会把那条诊断毁掉。
#[derive(Debug, Clone, Copy, Default, PartialEq, serde::Serialize)]
pub struct HalTrimCounters {
    /// 拼接次数。
    pub events: u64,
    /// 累计削掉的帧（1 ms = 48 帧）。
    pub frames: u64,
    /// F 档（等不到低能量段，强制执行）的次数。
    pub forced: u64,
    /// 想削但被令牌 / 可行性 / 追平期挡下的 tick 数。
    pub deferred_ticks: u64,
    /// 当前目标水位 `D_target`。
    ///
    /// ⚠ **这是 trim 的目标，不是稳态水位。** `safety_net` 档下它被顶成固定的
    /// 60 ms（`D_TARGET_SAFETY`）——那是**重同步的触发线**，不是环该停在哪。
    /// 环真正会收敛到的是 [`Self::dll_target_ms`]。两者相差可达 30 ms，照这个
    /// 数去读现场水位会得出「环长期低于目标 27 ms」这种完全错误的结论
    /// （本项目排查欠载时真的踩过）。
    pub target_ms: f32,
    /// **DLL 伺服的目标水位**（`Ctl::dll_target_frames`）——稳态水位就是它。
    ///
    /// `clamp(1.25 × MaxDrawdown_60s + 5 ms + 欠载惩罚, 15, 120)`。与
    /// [`Self::drawdown_ms`] 一起读，可以直接答「现在离欠载边界还有多远」，
    /// 这是判断「延迟被换成了欠载」的核心读数。
    pub dll_target_ms: f32,
    /// 当前 `MaxDrawdown_60s`（低于它必欠载的实测边界）。
    pub drawdown_ms: f32,
    /// 令牌余额。
    pub tokens_ms: f32,
}

/// 欠载（短读 ⇒ 补静音）计数（规格 §6.2）。
///
/// 这是「trim 是否削过头」的**唯一直接证据**：`Shared::append_spk_frame` 一直
/// 返回真实样本数，但 `HalSpeakerSource::next_frame` 过去把返回值丢了，补进去的
/// 静音原样发给对端而没有任何计数器知道。
#[derive(Debug, Clone, Copy, Default, PartialEq, serde::Serialize)]
pub struct HalUnderrunCounters {
    /// 累计补静音帧数 = Σ(F − got)。
    pub frames: u64,
    /// 连续短读段的次数（区分「一次大坑」与「持续细碎」）。
    pub events: u64,
    /// 最长一次连续短读的帧数。
    pub worst_run_frames: u32,
}

/// `hal_mic` 生产侧闸门（`crate::micgate`）的现场读数。
///
/// 怎么读，按排除顺序：
///
/// - `starved_ticks` **非 0** ⇒ 驱动已经取不满过，App 那侧录到了静音或断续。
///   这是这一级唯一的欠载信号，且它比扬声器方向的欠载更要紧：那一级我们自己
///   读不满、当场就知道；这一级发生在驱动进程里，没有任何回执。
/// - `low_water_ms` 贴近 `Q_C`（10.7 ms）⇒ 还没欠载，但余量已经吃完了。
///   `starved_ticks == 0` 时它是唯一还能回答「还剩多少」的读数。
/// - `drain_events` 在**稳态**下涨 ⇒ 天花板定低了，闸门在误伤一条健康的会话。
///   健康稳态的判据是它恒为 0（自由带 20.7–41.3 ms 整个在天花板 60 ms 之下）。
/// - `drain_events` 只在**开流后不久**涨一次 ⇒ 正常，那是上一条会话的存量
///   被一次性排掉，正是这套治理要做的事。看 `withheld_frames` 判断空洞多长。
/// - `depth_ms` 稳定落在 `[floor_ms, ceil_ms]` ⇒ 闸门在工作。
///   稳定**高于** `ceil_ms` ⇒ 闸门没被调用（接线断了），因为它结构上不可能
///   允许水位停在天花板之上。
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct HalMicGateCounters {
    pub drain_events: u64,
    pub drain_ticks: u64,
    pub withheld_frames: u64,
    pub starved_ticks: u64,
    /// 观测到的最低水位（ms）。`None` = 还没观测过任何一拍。
    pub low_water_ms: Option<f32>,
    /// 最近一次观测到的水位（ms）。
    pub depth_ms: f32,
}

/// Per-slot traffic and state, summed into the three headline counters and
/// reported per slot beside them (spec-m5b §6.1).
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct HalSlotCounters {
    pub spk_frames: u64,
    pub mic_frames: u64,
    pub mic_dropped: u64,
    pub generation: u32,
    /// 主动水位削减（规格 §10.1 的 `trim`）。
    pub trim: HalTrimCounters,
    /// 欠载补静音（规格 §10.1 的 `underrun`）。
    pub underrun: HalUnderrunCounters,
    /// 治法 A 在跳 tick 时从这个槽的扬声器环里排掉的帧
    /// （规格 §10.1 的 `skip.drained_frames`，按槽分摊）。
    pub skip_drained_frames: u64,
    /// `hal_mic` 生产侧闸门的现场（见 [`HalMicGateCounters`]）。
    pub mic_gate: HalMicGateCounters,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct HalBridgeStatus {
    /// The driver's mach name resolves and we hold a send right on it. False
    /// means coreaudiod has not published it (plug-in absent, or coreaudiod
    /// restarting), which is a normal transient, not an error.
    ///
    /// Reported to IPC as `hal.registered` — the name that shipped, kept
    /// because `test/tests/hal_wiring.rs` freezes that JSON key.
    pub driver_found: bool,
    /// A HAL plug-in has completed the handshake and holds live rings.
    pub driver_connected: bool,
    /// Speaker-direction frames handed to the media engine, summed over slots.
    pub spk_frames: u64,
    /// Microphone-direction frames accepted by the rings, summed over slots.
    pub mic_frames: u64,
    /// Microphone frames the rings had no room for (driver not draining).
    pub mic_dropped: u64,
    /// Seconds since the last message from the driver, if it ever spoke.
    pub last_driver_msg_secs: Option<f64>,
    /// How many slots the attached driver actually offers. 0 when detached.
    pub slot_count: u8,
    /// What the driver said it speaks, when it refused us for that reason.
    pub driver_protocol_version: Option<u32>,
    /// Why there is no live bridge, in words a UI can show. `None` while
    /// connected.
    pub status_reason: Option<String>,
    /// Binds the driver refused, or performed only halfway. Monotonic.
    pub bind_failures: u64,
    /// The most recent of those, in words. `None` once a bind succeeds again.
    ///
    /// A connected driver that will not publish an endpoint is otherwise
    /// INVISIBLE from here: `driver_connected` is true and the counters keep
    /// moving. This pair is the only place that state is representable.
    pub last_bind_error: Option<String>,
    /// Binds that SUCCEEDED but published the peer's devices under the generic
    /// direction names instead of the peer's own (Windows only). Monotonic.
    ///
    /// Separate from `bind_failures` because it is not one: the devices exist
    /// and work. It is counted at all because with two peers paired it means
    /// two identically labelled speakers, and "the devices are there but you
    /// cannot tell them apart" needs somewhere to be said.
    pub endpoint_name_fallbacks: u64,
    /// 主动水位削减，跨槽汇总。计数是**和**，三个读数（target/drawdown/tokens）
    /// 取**最大**——它们是水位而不是流量，相加没有物理含义。
    pub trim: HalTrimCounters,
    /// 欠载补静音，跨槽汇总（`worst_run_frames` 取最大）。
    pub underrun: HalUnderrunCounters,
    /// 治法 A 从扬声器环里排掉的帧，跨槽汇总。
    pub skip_drained_frames: u64,
    /// `hal_mic` 闸门，跨槽汇总。计数求和；两个水位读数取**最坏**
    /// （`low_water` 取最小、`depth` 取最大）——它们是水位不是流量，
    /// 相加没有物理含义，取平均会把一个正在饿死的槽藏进另一个健康的槽里。
    pub mic_gate: HalMicGateCounters,
    /// Per-slot detail, indexed by slot.
    pub slots: Vec<HalSlotCounters>,
}

pub struct HalBridge {
    shared: Arc<Shared>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

/// 一条扬声器环这一 tick 的相位误差观测。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpkPhase {
    /// 观测来自哪个槽（诊断用）。
    pub slot: u8,
    /// `D_target − 读后残量`，帧。**消费者语义**：`> 0` = 请让水位涨
    /// （周期变长、读得更慢）。符号推导见 [`dll`] 的模块文档。
    pub err_frames: f32,
}

/// [`HalBridge::spk_phase_error`] 的调用方状态：上一次看到的每槽发布代次。
///
/// 放在调用方而不是 `Shared` 里，是因为「新鲜与否」是**相对某个消费者**的：
/// `tx_loop` 一份，测试一份，互不干扰。
#[derive(Debug, Clone)]
pub struct SpkPhaseWindow {
    epochs: [u64; HAL_MAX_SLOTS],
}

impl Default for SpkPhaseWindow {
    fn default() -> SpkPhaseWindow {
        SpkPhaseWindow { epochs: [0; HAL_MAX_SLOTS] }
    }
}

impl SpkPhaseWindow {
    pub fn new() -> SpkPhaseWindow {
        SpkPhaseWindow::default()
    }

    /// 作废全部观测基准（跳 tick 排空、空闲重锚之后调）。下一 tick 的观测会被
    /// 当成「第一次看见」而跳过，于是**跳变那一刻的水位不会被喂进环路**。
    pub fn invalidate(&mut self) {
        self.epochs = [0; HAL_MAX_SLOTS];
    }
}

impl HalBridge {
    /// `Ok(None)` means "no bridge here, keep behaving exactly as before":
    /// the driver's mach name does not resolve, mode `off`, or a non-macOS
    /// build. Only `Require` turns the first of those into a live-but-searching
    /// bridge instead.
    pub fn start(cfg: HalBridgeCfg) -> Result<Option<HalBridge>> {
        platform::start(cfg)
    }

    pub fn status(&self) -> HalBridgeStatus {
        let s = &self.shared;
        let last = *lk(&s.last_driver_msg);
        let n = s.slot_count.load(Ordering::Relaxed) as usize;
        let slots: Vec<HalSlotCounters> = (0..n.min(HAL_MAX_SLOTS))
            .map(|i| s.slots[i].snapshot())
            .collect();
        HalBridgeStatus {
            driver_found: s.driver_found.load(Ordering::Relaxed),
            driver_connected: s.driver_connected.load(Ordering::Relaxed),
            spk_frames: slots.iter().map(|c| c.spk_frames).sum(),
            mic_frames: slots.iter().map(|c| c.mic_frames).sum(),
            mic_dropped: slots.iter().map(|c| c.mic_dropped).sum(),
            last_driver_msg_secs: last.map(|t| t.elapsed().as_secs_f64()),
            slot_count: n as u8,
            driver_protocol_version: match s.driver_protocol.load(Ordering::Relaxed) {
                0 => None,
                v => Some(v),
            },
            status_reason: lk(&s.status_reason).clone(),
            bind_failures: s.bind_failures.load(Ordering::Relaxed),
            last_bind_error: lk(&s.last_bind_error).clone(),
            endpoint_name_fallbacks: s.endpoint_name_fallbacks.load(Ordering::Relaxed),
            trim: HalTrimCounters {
                events: slots.iter().map(|c| c.trim.events).sum(),
                frames: slots.iter().map(|c| c.trim.frames).sum(),
                forced: slots.iter().map(|c| c.trim.forced).sum(),
                deferred_ticks: slots.iter().map(|c| c.trim.deferred_ticks).sum(),
                // 水位取最大而不是求和：把两个槽各自的 44 ms 目标加成 88 ms
                // 是一句没有物理含义的话。
                target_ms: slots.iter().map(|c| c.trim.target_ms).fold(0.0, f32::max),
                dll_target_ms: slots.iter().map(|c| c.trim.dll_target_ms).fold(0.0, f32::max),
                drawdown_ms: slots.iter().map(|c| c.trim.drawdown_ms).fold(0.0, f32::max),
                tokens_ms: slots.iter().map(|c| c.trim.tokens_ms).fold(0.0, f32::max),
            },
            underrun: HalUnderrunCounters {
                frames: slots.iter().map(|c| c.underrun.frames).sum(),
                events: slots.iter().map(|c| c.underrun.events).sum(),
                worst_run_frames: slots
                    .iter()
                    .map(|c| c.underrun.worst_run_frames)
                    .max()
                    .unwrap_or(0),
            },
            skip_drained_frames: slots.iter().map(|c| c.skip_drained_frames).sum(),
            mic_gate: HalMicGateCounters {
                drain_events: slots.iter().map(|c| c.mic_gate.drain_events).sum(),
                drain_ticks: slots.iter().map(|c| c.mic_gate.drain_ticks).sum(),
                withheld_frames: slots.iter().map(|c| c.mic_gate.withheld_frames).sum(),
                starved_ticks: slots.iter().map(|c| c.mic_gate.starved_ticks).sum(),
                // 取最小，且**跳过没观测过的槽**：`None` 参与 min 会让一个
                // 从未被使用的槽把汇总读数据为己有。
                low_water_ms: slots
                    .iter()
                    .filter_map(|c| c.mic_gate.low_water_ms)
                    .fold(None, |acc: Option<f32>, v| Some(acc.map_or(v, |a| a.min(v)))),
                depth_ms: slots.iter().map(|c| c.mic_gate.depth_ms).fold(0.0, f32::max),
            },
            slots,
        }
    }

    /// 记录一次 `hal_mic` 闸门处置。**只被 mixer 循环调用**（每槽每 tick 一次）。
    ///
    /// 全部 `Relaxed`：这些量只被读来看，不参与任何同步，而它跑在 10 ms
    /// 截止期线程上，代价必须是零。
    pub fn record_mic_gate(&self, slot: u8, plan: &crate::micgate::MicPlan, occupied: u32) {
        let Some(c) = self.shared.slots.get(slot as usize) else { return };
        c.mic_depth_frames.store(occupied, Ordering::Relaxed);
        c.mic_low_water.fetch_min(occupied, Ordering::Relaxed);
        if plan.starved {
            c.mic_starved_ticks.fetch_add(1, Ordering::Relaxed);
        }
        if plan.drain_started {
            c.mic_drain_events.fetch_add(1, Ordering::Relaxed);
        }
        if plan.draining {
            c.mic_drain_ticks.fetch_add(1, Ordering::Relaxed);
            c.mic_withheld_frames
                .fetch_add(plan.withheld as u64, Ordering::Relaxed);
        }
    }

    /// How many slots the attached driver offers, 0 when detached. This is the
    /// capacity a peer past the end gets `hal_reason: "capacity"` against — the
    /// DRIVER's number, not this build's `HAL_MAX_SLOTS`, so a driver built
    /// with a smaller pool is a visible capacity limit rather than a refused
    /// handshake.
    pub fn slot_count(&self) -> usize {
        (self.shared.slot_count.load(Ordering::Relaxed) as usize).min(HAL_MAX_SLOTS)
    }

    /// Bumped on every completed handshake. A coordinator that sees this move
    /// must re-`Set` every binding it still intends (see `Attached`).
    pub fn attach_epoch(&self) -> u64 {
        self.shared.attach_epoch.load(Ordering::Acquire)
    }

    /// APPENDS one 10ms mono frame of whatever an app played into this slot's
    /// virtual speaker, padding with silence so exactly `HAL_FRAME_48K` samples
    /// are added: a missing or idle driver produces silence, never a stall or a
    /// short frame. Returns how many of those samples were real.
    ///
    /// The name says `append` because the previous one (`read_spk_frame`) read
    /// as "replace" and a caller took it that way — `FrameSource::next_frame`
    /// must replace, the engine truncated the over-long frame back to its first
    /// 480 samples, and the peer received the silence captured before any app
    /// had played anything, forever, with every counter and probe still green.
    pub fn append_spk_frame(&self, slot: u8, out: &mut Vec<f32>) -> usize {
        self.shared.append_spk_frame(slot, out)
    }

    /// 这个槽的扬声器环此刻积着多少帧，以及环有多大（规格 §3.2 的级 3′
    /// `hal_spk`）。**只读，不移动 `read_idx`。**
    ///
    /// 规格 §0.3 的修正二：`HAL_RING_FRAMES = 24000` = 500 ms，而
    /// `engine.rs` 只在开流那一刻冲掉积压，之后**没有任何机制收敛驻留深度**。
    /// 「写入与读出严格相等」只证明速率相等，证明不了驻留深度小——一个恒定
    /// 存着 400 ms 的 FIFO，进出速率同样严格相等。这个读数就是用来把那个
    /// 结构上无法证伪的场景变成一个可以直接看的数。
    pub fn spk_depth(&self, slot: u8) -> Option<(u32, u32)> {
        self.shared.rings.spk_readable(slot as usize)
    }

    /// 这一 tick 的 DLL 相位误差观测，帧。`None` = 没有任何一条扬声器环给出
    /// 新鲜观测（没槽被消费 / 驱动没附着 / 全都落在追平期或不连续 tick 上）。
    ///
    /// ## 多槽怎么归约
    ///
    /// 唤醒周期只有一个，而扬声器环可能有好几条（每个对端一条，spec-m5b）。
    /// **取所有新鲜观测里 `err` 最大的那一条**——也就是**最饿的那条环**。
    ///
    /// 理由：一个 tick 从每条环各读走 480 帧，所以加快唤醒会**同时**加快所有环
    /// 的消费。若按「最积压的那条」去伺服，代价就是把其余环读穿 ⇒ 欠载 ⇒ 可闻
    /// 断续。而反过来的代价只是「最积压的那条排得慢一点」，那一条本来就有
    /// **按槽独立**的 trim 兜底（DLL 是全局相位伺服，trim 是单槽重同步，
    /// 分工正好落在这个缺口上）。风险不对称 ⇒ 取保守的那一端。
    ///
    /// `seen` 由调用方持有（`tx_loop` 一份），用来判「这一 tick 这条环真的被
    /// 消费了」。
    pub fn spk_phase_error(&self, seen: &mut SpkPhaseWindow) -> Option<SpkPhase> {
        let mut best: Option<SpkPhase> = None;
        for slot in 0..HAL_MAX_SLOTS {
            let s = &self.shared.slots[slot];
            let epoch = s.dll_epoch.load(Ordering::Acquire);
            let prev = std::mem::replace(&mut seen.epochs[slot], epoch);
            // 代次没动 = 这条环这一 tick 没有产生有效观测。`prev == 0` 是本进程
            // 第一次看这个槽，同样不算新鲜（没有基准）。
            if epoch == prev || prev == 0 {
                continue;
            }
            let err = f32::from_bits(s.dll_err_frames.load(Ordering::Relaxed));
            if !err.is_finite() {
                continue;
            }
            if best.map_or(true, |b| err > b.err_frames) {
                best = Some(SpkPhase { slot: slot as u8, err_frames: err });
            }
        }
        best
    }

    /// 这个槽的**麦克风**环此刻积着多少（规格 §3.2 之外新建模的级 8″
    /// `hal_mic`）。**只读，不动任何下标。**
    ///
    /// 与 `spk_depth` 严格对称，只是方向相反：扬声器环是驱动写、我们读，
    /// 麦克风环是我们写、驱动读。模式 B 的接收流把对端麦克风音频写进这里，
    /// 选了这个虚拟麦克风的 App 从驱动那一侧取走——**它在送音频的路径上**，
    /// 少一级就是 `local_ms` 少一段，且没有任何字段标出它缺席。
    ///
    /// 丢弃方向 `Newest`（`write` 满了短写），且**在我们这一侧**，所以
    /// 与 `hal_spk` 不同：这一级的 `dropped` 是真读数，不是 `None`。
    pub fn mic_depth(&self, slot: u8) -> Option<audiohub_core::latency::StageDepth> {
        use audiohub_core::latency::{DropMode, StageDepth, StageId};
        let (samples, capacity) = self.shared.rings.mic_occupied(slot as usize)?;
        Some(StageDepth {
            id: StageId::HalMic,
            samples,
            capacity,
            rate: HAL_SAMPLE_RATE,
            dropped: Some(
                self.shared
                    .slots
                    .get(slot as usize)
                    .map(|c| c.mic_dropped.load(Ordering::Relaxed))
                    .unwrap_or(0),
            ),
            drop_mode: DropMode::Newest,
        })
    }

    /// Appends up to `max_frames` mono samples; returns how many were real.
    pub fn read_spk_mono(&self, slot: u8, out: &mut Vec<f32>, max_frames: usize) -> usize {
        let mut done = 0;
        while done < max_frames {
            let want = (max_frames - done).min(HAL_FRAME_48K);
            let got = self.shared.read_spk_chunk(slot, out, want);
            done += got;
            if got < want {
                break;
            }
        }
        done
    }

    /// Peer microphone audio for one slot's virtual microphone. Returns the
    /// number of samples the ring accepted; the remainder is dropped rather
    /// than queued, because a driver that is not draining is a driver nobody is
    /// listening to.
    pub fn write_mic_mono(&self, slot: u8, mono: &[f32]) -> usize {
        self.shared.write_mic(slot, mono)
    }

    /// Drops whatever is queued in the speaker rings of every PUBLISHED slot
    /// that has no consumer, so an idle virtual speaker cannot fill up and make
    /// the driver's census report "audiohubd has stopped draining it"
    /// (spec-m5b §5.4). MUST be called from the tx thread and nowhere else:
    /// only a ring's consumer may move `read_idx`.
    ///
    /// `busy` is the mask of slots that DO have a live source this tick; those
    /// are left alone.
    pub fn drain_idle_speakers(&self, busy: u16) {
        let published = self.shared.published.load(Ordering::Relaxed);
        let idle = published & !busy;
        for slot in 0..HAL_MAX_SLOTS {
            if idle & (1 << slot) != 0 {
                self.shared.take_flush(slot as u8); // consumed here, not lost
                self.shared.rings.flush_spk_consumer(slot);
            }
        }
    }

    /// **治法 A**：一次 >100 ms 的消费侧卡顿之后，把被跳过的那些帧从环里读走
    /// 丢掉，而不是留在环里。返回真正丢掉的帧数。
    ///
    /// ## 为什么这是对的
    ///
    /// `tick = behind` 的跳 tick 里，被跳过的那段音频既没被读走也没被丢掉——它
    /// **永久**留在环里，从此每一个样本都要多排那么久的队。生产者与消费者锚在
    /// 同一个 `mach_absolute_time` 上，长期速率误差为零 ⇒ 没有任何机制会把它
    /// 排出去（实测 9 小时 40 分从 ≈0 涨到 434 ms，环只有 500 ms）。
    ///
    /// 换来的是「一次 108 ms 的音频空洞」代替「永久 +108 ms 延迟」。而那个空洞
    /// **两种情况下都存在**——tx 线程停了，什么都没发出去，对端的 JB 必然饿死；
    /// 做了 A 只是在已有的空洞上额外丢掉 108 ms 内容，换掉永久延迟。对实时设备
    /// 共享这个产品定位，这个交换是对的，且与 `// never replay missed ticks`
    /// 的既有设计意图一致。
    ///
    /// **不会欠载**：只丢已经积压的部分，丢完水位回到跳变前，不是回到 0。
    ///
    /// **不会排进工作储备**：留下至少 `trim::D_FLOOR_MIN`（15 ms）。要丢的量按
    /// 定义等于「那些被跳过的 tick 本来会读走的帧」，正常情况下丢完就回到卡顿
    /// 之前的水位；但如果生产者在同一段时间里**也**漏写了，环里根本没那么多东西，
    /// 无脑丢到底就会把一个延迟问题换成一个欠载问题。低于 15 ms 的那部分不是
    /// 积压，是驱动周期（512 帧 = 10.67 ms，比一个 tick 长）必需的储备。
    ///
    /// MUST be called from the tx thread and nowhere else: only a ring's
    /// consumer may move `read_idx`.
    pub fn drain_spk(&self, slot: u8, frames: usize) -> usize {
        let avail = self
            .shared
            .rings
            .spk_readable(slot as usize)
            .map(|(n, _)| n as usize)
            .unwrap_or(0);
        let want = frames.min(avail.saturating_sub(trim::D_FLOOR_MIN));
        let n = if want == 0 {
            0
        } else {
            self.shared.rings.drop_spk(slot as usize, want)
        };
        if let Some(c) = self.shared.slots.get(slot as usize) {
            if n > 0 {
                c.skip_drained.fetch_add(n as u64, Ordering::Relaxed);
            }
            // 归因埋点。**`n == 0` 时也要盖戳**：那正是「想排却排不动」的情形
            // （生产者同时也停了），它是欠载最强的嫌疑人之一，不能因为没排掉
            // 东西就从案发记录里消失。`left` 是这一刀之后环里还剩多少。
            c.last_drain_us.store(mono_us(), Ordering::Relaxed);
            c.last_drain_frames.store(n as u32, Ordering::Relaxed);
            c.last_drain_left
                .store(avail.saturating_sub(n) as u32, Ordering::Relaxed);
            // 排空之后「上一 tick 的读后残量」作废：`W_n = A_{n+1} − D_n` 只在
            // 两次读之间只有生产者动过时才成立。
            //
            // **`n == 0` 时也要作废**，而且那种情况更危险：什么都没丢掉，说明
            // 生产者在同一段时间里也停了，此时那个差值会算出一个**过大**的 `W`，
            // 把回撤压成 0 ⇒ `D_floor` 掉到结构性下限 ⇒ trim 变得更激进，恰好
            // 在生产侧最不稳的时候。方向反了，比不作废更糟。
            c.disc_epoch.fetch_add(1, Ordering::Relaxed);
        }
        n
    }

    /// `tx_loop` 每 tick 报一次「本 tick 准不准时」（`behind <= tick`）。
    ///
    /// 规格不变量 I6：追平期的高水位是**假象**（我们暂时没读，不是积压），在那
    /// 些 tick 上 trim 会把马上就要用到的音频削掉，紧接着就欠载。这是整套水位
    /// 逻辑里最容易写错的一条，所以它是一个显式的调用而不是一个推断。
    pub fn set_tick_punctual(&self, punctual: bool) {
        self.shared.tick_punctual.store(punctual, Ordering::Relaxed);
    }

    /// The set of slots the daemon has bound, as a bitmask. Set by the device
    /// coordinator, read by the tx loop.
    pub fn set_published(&self, mask: u16) {
        self.shared.published.store(mask, Ordering::Relaxed);
    }

    /// Volume/mute, IO-state and slot-state changes the driver has reported
    /// since the last call. Never blocks.
    pub fn drain_events(&self) -> Vec<HalControlEvent> {
        let mut q = lk(&self.shared.events);
        std::mem::take(&mut *q)
    }

    /// Reverse direction of plan §7.2: the peer's real device reported a new
    /// volume, so the virtual control must show it. Best effort — a driver that
    /// is not attached simply misses it and re-reads on its next handshake.
    ///
    /// `generation` is the slot's current stamp; the driver drops anything that
    /// does not match, which is what keeps a volume meant for the previous
    /// tenant of a slot off the new one's control.
    pub fn notify_volume(&self, at: HalEndpoint, generation: u32, scalar: f32, muted: bool) {
        self.shared.notify_volume(at, generation, scalar, muted);
    }

    /// Binds `slot` to a peer, or re-states an existing binding. IDEMPOTENT by
    /// contract: same slot + same UIDs is a no-op on the driver's side except
    /// for the state replay, so this is what a daemon restart sends instead of
    /// Clear-then-Set (which would destroy the user's chosen default output on
    /// every restart, silently — spec-m5b §1 third trade-off).
    pub fn bind_set(&self, req: &HalBindRequest) -> bool {
        platform::send_bind_set(&self.shared, req)
    }

    /// Retires `slot`. `generation` is what this daemon believes the slot's
    /// stamp to be; the driver ignores a mismatch, so a Clear delayed past a
    /// re-bind cannot cut down the binding that replaced it.
    pub fn bind_clear(&self, slot: u8, generation: u32) -> bool {
        platform::send_bind_clear(&self.shared, slot, generation)
    }

    pub fn shutdown(&self) {
        self.shared.stop.store(true, Ordering::SeqCst);
    }
}

/// One `Bind Set`. The driver owns no naming or disambiguation logic at all
/// (it runs in coreaudiod's sandbox and can read neither the computer name nor
/// a localisation), so the daemon hands it the finished strings — spec-m5b §3.5.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HalBindRequest {
    pub slot: u8,
    /// Peer fingerprint. On macOS this is only the driver's log line and its
    /// idempotency compare; on WINDOWS it is additionally the device-interface
    /// reference string, i.e. the endpoint's identity, so the driver validates
    /// it against a `[0-9a-f]{16}` whitelist.
    pub peer_key: String,
    pub out_uid: String,
    pub in_uid: String,
    pub out_name: String,
    pub in_name: String,
    /// The peer's disambiguated display name with NO direction suffix —
    /// "AudioHub – WIN-30", where `out_name` would be "AudioHub – WIN-30 扬声器".
    ///
    /// macOS ignores this and uses `out_name`/`in_name`, which are the complete
    /// device names. Windows uses ONLY this, because it composes the string the
    /// user sees itself: the endpoint name is `"<pin name> (<filter
    /// FriendlyName>)"`, and the driver controls only the half in brackets.
    /// Sending `out_name` there would produce "扬声器 (AudioHub – WIN-30 扬声器)".
    pub display: String,
    /// bit0 of the wire `flags`: the peer is connected. Logging only on the
    /// driver's side — a device is published either way (plan §7.3).
    pub online: bool,
}

impl Drop for HalBridge {
    fn drop(&mut self) {
        self.shutdown();
        if let Some(j) = lk(&self.thread).take() {
            let _ = j.join();
        }
    }
}

// ---------------------------------------------------------------- 水位削减
//
// `docs/spec-latency-trim.md` 的实现。与平台无关、不碰任何环，所以整套逻辑在
// 任何机器上都可以单测——这一点是有意的：控制器写错的表现是**削过头导致欠载**，
// 而欠载在真机上只会表现成「偶尔有点断续」，靠听是抓不住的。

pub(crate) mod trim {
    //! 把 HAL 扬声器环的驻留深度从「历史的积分」变成受控量。
    //!
    //! ## 为什么需要它（一句话病理）
    //!
    //! 生产者（驱动 IOProc）与消费者（`tx_loop`）锚在**同一个**
    //! `mach_absolute_time` 上，长期速率误差为零 ⇒ 水位没有任何吸引子把它拉回来。
    //! 它只在事件发生时跳变、事件之间纹丝不动：
    //! `驻留(t) = Σ(消费侧 >100 ms 卡顿) − Σ(生产侧漏写)`。
    //! 实测 9 小时 40 分从 ≈0 涨到 434 ms，环容量只有 500 ms。
    //!
    //! ## 两个**互相独立**的预算（规格 §4.1）
    //!
    //! | 预算 | 被谁决定 | 越界的听感 |
    //! |---|---|---|
    //! | 单次拼接质量 | `X`、`τ` 的选择、门控 | 咔哒 / 相位跳变 / 电平凹陷 |
    //! | 累计时间压缩率 | 每秒削掉多少 ms | 「声音在加速播放」 |
    //!
    //! 它们不能互相替代，也不能用一个数合并。前者由 [`splice`] 管，后者由
    //! [`Ctl`] 的令牌桶管。**令牌桶本身就是「分多次」**：它不关心积压多大，
    //! 只按 ρ 匀速排——所以不需要另一套「缓慢时间压缩」机制。

    /// 1 ms 有多少帧。全文的 ms 都可以直接乘它得到帧数。
    pub const MSF: usize = (super::HAL_SAMPLE_RATE as usize) / 1000; // 48

    /// 交叉淡化长度（帧）。4 ms。
    ///
    /// 下界 2 ms：过渡越短，拼接残差的谱扩散越宽（Hann 型过渡主瓣宽 ≈ 2/X）。
    /// 上界 8 ms：混合区越长，两条时间轴并存越久，瞬态上的「重影」越明显。
    pub const X: usize = 4 * MSF; // 192
    /// 相似度搜索半宽（帧）。8 ms ⇒ 覆盖 ≥60 Hz 的一切内容（60 Hz 半周期
    /// 400 帧 ≈ 8.3 ms），任意基频都能找到同相位点。
    pub const DELTA: usize = 8 * MSF; // 384
    /// 小于它就不削：每次拼接都要花一次「单次拼接质量」预算，为了 2 ms 花掉
    /// 不划算；而迟滞本来就是 10 ms，真要削时可削量天然 ≥10 ms。
    pub const T_MIN: usize = 5 * MSF; // 240
    /// 相关档（Q/F）的单次上限：10 ms 的内容移除低于音素尺度，相位由搜索保证
    /// 连续——这是 VoIP 端点（NetEq accelerate）每天都在做的操作。
    pub const T_MAX_CORR: usize = 10 * MSF; // 480
    /// 静音档的单次上限。删掉的是「停顿的时长」，人对无参照停顿的分辨力远粗于
    /// 120 ms；这个值只是为了限制单次 memcpy，不是听感边界。
    pub const T_MAX_SILENT: usize = 120 * MSF; // 5760
    /// 单次 peek 的帧数上界（静音快速通道用）。250 ms。
    pub const PEEK_MAX: usize = 250 * MSF; // 12000

    /// 峰值门控阈值（**线性幅度，不是 RMS，也不做 log10**）。
    ///
    /// 用峰值的理由：RMS 会把「一片安静里的一个瞬态」平均掉，而那个瞬态恰恰是
    /// 唯一不能被剪掉的东西。峰值判据的物理含义是「这一段里最响的那个样本也
    /// 低于阈值」，这才是「剪掉它不会被听见」的充分条件。
    pub const GATE_SILENT: f32 = 0.001; // −60 dBFS
    pub const GATE_QUIET: f32 = 0.01; // −40 dBFS

    /// F 档的软条件：相关度低于它就再等一等，最多等 [`NCC_RETRY_TICKS`] 个 tick。
    pub const NCC_MIN_F: f32 = 0.3;
    pub const NCC_RETRY_TICKS: u32 = 20; // 200 ms

    /// 结构性下限，任何配置都不得突破。驱动声明的周期 512 帧 = 10.67 ms **比一个
    /// tick 长**，`W_n = 0` 的 tick 必然周期性出现，此时需要 `D ≥ F = 10 ms`；
    /// 再加半个 tick 余量 ⇒ 15 ms。
    pub const D_FLOOR_MIN: usize = 15 * MSF; // 720
    pub const D_TARGET_MIN: usize = 15 * MSF;
    /// 超过它说明生产侧病得很重（IOProc 都在漏 100 ms 以上），该报警而不是继续
    /// 加缓冲。
    pub const D_TARGET_MAX: usize = 120 * MSF; // 5760
    /// 还没攒够 60 s 观测数据时的目标水位。
    pub const D_TARGET_COLD: usize = 30 * MSF; // 1440
    /// `safety_net` 档的固定目标 / 迟滞上沿。
    pub const D_TARGET_SAFETY: usize = 60 * MSF;
    pub const W_HIGH_SAFETY: usize = 100 * MSF;

    /// 令牌桶容量与填充率。
    ///
    /// ρ = 1 % 的依据：WSOLA 保持音高不变，可闻的是节奏；无参照条件下的节奏 JND
    /// 约 2–4 %，1 % 有 2–4 倍余量。而且它是**上限**——稳态下实际占空比是 0。
    /// **即使控制器写错、一直误开火，失真上限也只是 1 % 变速。**
    pub const B_TOK: f32 = (120 * MSF) as f32;
    pub const RHO: f32 = 0.01;
    /// 水位过 60 % 容量后的应急速率。不削的代价是**驱动侧不可观测的丢弃**
    /// （drop-newest，一定可闻且全链路无一处看得见），3 % 严格优于断续。
    pub const RHO_EMERGENCY: f32 = 0.03;
    pub const EMERGENCY_FRAC: f32 = 0.6;

    /// escalate 步长：`[0,5s)` 只等真静音，`[5s,10s)` 放宽到安静，`≥10s` 不再等。
    ///
    /// 等待的代价是「水位继续挂在高位」，在 ρ = 1 % 的框架下等 10 s 只相当于
    /// 放弃 100 ms 的削减额度，而 10 s 足以覆盖语音的句间停顿与音乐的绝大多数
    /// 乐句间隙。**等待很便宜，别急。**
    pub const ESCALATE_STEP_US: u64 = 5_000_000;

    /// `MaxDrawdown` 的观测窗口。
    const DD_WINDOW_S: usize = 60;
    /// §6.3 的惩罚项上限与衰减节律。
    const EXTRA_MAX: f32 = (60 * MSF) as f32;
    const EXTRA_DECAY_US: u64 = 30_000_000;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Mode {
        /// 完全不削（只观测）。
        Off,
        /// 固定 60 ms 目标、100 ms 上沿：正常运行时占空比为 0，只在治法 A
        /// 漏掉的情形下开火（规格 §7.4）。
        SafetyNet,
        /// §3.3 的自适应目标。
        Active,
    }

    impl Mode {
        /// `AUDIOHUB_TRIM_MODE=off|safety_net|active`。
        ///
        /// 走环境变量而不是设置项，是因为设置项的落盘与 IPC 都在 `lib.rs`
        /// （本次改动不碰那个文件）。
        ///
        /// ## 默认档从 `active` 改成 `safety_net`（DLL 落地之后）
        ///
        /// 规格 §10.2 定的默认是 `active`，那是在 trim 还是**水位唯一执行器**的
        /// 前提下定的。[`super::dll`] 落地之后这个前提没了，继续默认 `active`
        /// 有三个具体害处：
        ///
        /// 1. **两个控制器抢同一个被控量。** `active` 的触发线是
        ///    `D_target + 10 ms`，而 DLL 正是把水位**收敛到 `D_target`** 的那个
        ///    环路。稳态附近的正常抖动就会反复越过 10 ms 迟滞 ⇒ trim 每次开火都
        ///    给 DLL 注一个阶跃 ⇒ 环路永远在追一个被别人踢来踢去的水位。
        /// 2. **削样本变成了没有必要的代价。** trim 每次拼接都要花一次「单次
        ///    拼接质量」预算（§4.1）；DLL 达到同一个水位**一个样本都不丢**。
        ///    在 DLL 够得着的量级上（≤100 ms，见 [`super::dll::CORR_CLAMP`]）
        ///    付这个代价是纯亏。
        /// 3. **`safety_net` 的射程恰好是 DLL 够不着的那一段。** 它的上沿是
        ///    100 ms、目标 60 ms：DLL 以 30 ms/分钟收敛，100 ms 以内的残量三分钟
        ///    内排完；超过 100 ms 的只可能来自**阶跃**（驱动重附着、索引回绕、
        ///    睡眠唤醒、治法 A 够不着的那一级），那正是「丢一次样本换回一整段
        ///    延迟」划算的场合。
        ///
        /// **不是 `off`**：重同步这条路必须留着。DLL 是速率受限的，遇到阶跃只能
        /// 以 500 ppm 慢慢爬，`off` 会让一次睡眠唤醒的积压挂上几十分钟。
        ///
        /// 想回到旧行为显式设 `AUDIOHUB_TRIM_MODE=active` 即可；`active` 的全部
        /// 代码路径与测试原样保留。
        pub fn from_env() -> Mode {
            match std::env::var("AUDIOHUB_TRIM_MODE").ok().as_deref() {
                Some("off") | Some("0") => Mode::Off,
                Some("active") | Some("1") => Mode::Active,
                _ => Mode::SafetyNet,
            }
        }
    }

    /// 三个档。顺序即严格程度：`Silent < Quiet < Forced`。
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
    pub enum Tier {
        /// 峰值 < −60 dBFS。误差信号逐样本 ≤ 2 × 0.001 = −54 dBFS，以常规监听
        /// 电平（满幅 ≈ 85 dB SPL）计误差 ≈ 31 dB SPL 且持续 ≤4 ms —— 低于安静
        /// 房间的听阈。**这是证明，不是经验。**
        Silent,
        /// 峰值 < −40 dBFS。误差 ≤ −34 dBFS 的 4 ms 事件，被同期节目内容掩蔽。
        Quiet,
        /// 任意电平。等价于一次 WSOLA accelerate。
        Forced,
    }

    // ------------------------------------------------------------ 原语
    //
    // 全规格只有**一个**「削」的动作。所有队列、两个方向、三个档位共用它。

    /// 归一化互相关：参考段 `r[f−x .. f)` 与候选段 `r[f−x+tau .. f+tau)`。
    ///
    /// f64 累加：f32 在 192 个抽头上就会把「完全相同的两段」算成 0.9999xx，
    /// 而 `NCC == 1` 恰恰是等增益律（⇒ 逐样本恒等）的触发条件。
    pub fn ncc_at(r: &[f32], f: usize, x: usize, tau: usize) -> f32 {
        if x == 0 || f < x || r.len() < f + tau {
            return 0.0;
        }
        let (mut num, mut sa, mut sb) = (0.0f64, 0.0f64, 0.0f64);
        for k in 0..x {
            let a = r[f - x + k] as f64;
            let b = r[f - x + k + tau] as f64;
            num += a * b;
            sa += a * a;
            sb += b * b;
        }
        let den = (sa * sb).sqrt();
        // 两段里有一段是纯零 ⇒ 没有相位可言。返回 0 ⇒ p = 0.5（等功率），
        // 对全零输入两条律给出同一个结果，不影响不变量 I3。
        if den <= 0.0 {
            return 0.0;
        }
        ((num / den) as f32).clamp(-1.0, 1.0)
    }

    /// 峰值门控：`max|r[i]|`，`i ∈ [f−x, f+span)`。
    ///
    /// 区间必须同时含**被混合的**（淡化区）和**被丢弃的**（`[f, f+τ)`）两段。
    /// 只测被丢弃的那段是错的：淡化区里的瞬态同样会被搓成重影。
    pub fn gate_peak(r: &[f32], f: usize, x: usize, span: usize) -> f32 {
        let lo = f.saturating_sub(x);
        let hi = (f + span).min(r.len());
        r[lo.min(hi)..hi].iter().fold(0.0f32, |m, s| m.max(s.abs()))
    }

    /// 从 `f` 起，峰值持续低于 `thr` 的最长跨度（帧）。
    ///
    /// 淡化区 `[f−x, f)` 里只要有一个样本越阈值就直接返回 0 —— 那个瞬态会被
    /// 搓成重影，不能剪。
    ///
    /// 比「量整段峰值再全有全无」好在两点：热路径上对**有声**内容几乎立刻退出
    /// （第一个越阈的样本就够），而对静音内容给出的是**最优**可削长度，而不是
    /// 因为 100 ms 之后有一个瞬态就整段放弃。
    pub fn silent_span(r: &[f32], f: usize, x: usize, thr: f32) -> usize {
        let lo = f.saturating_sub(x);
        for &s in &r[lo.min(r.len())..f.min(r.len())] {
            if s.abs() >= thr {
                return 0;
            }
        }
        let mut n = 0;
        for &s in &r[f.min(r.len())..] {
            if s.abs() >= thr {
                break;
            }
            n += 1;
        }
        n
    }

    /// 一次拼接的完整决定。
    #[derive(Debug, Clone, Copy, PartialEq)]
    pub struct Decision {
        /// 接缝跨度（帧）。读指针要前进 `F + tau`。
        pub tau: usize,
        /// 该 `tau` 上的相关度，决定淡化律。
        pub ncc: f32,
        pub tier: Tier,
        /// 走的是 F 档（没等到低能量段，强制执行）。
        pub forced: bool,
        /// 要不要扣令牌。静音快速通道不扣。
        pub charge: bool,
    }

    /// **纯函数**：从一段已下混的样本 + 本 tick 的许可，决定削多少、在哪儿接。
    ///
    /// 不碰环、不碰计数器、不看时钟 —— 于是「定档 / 门控 / 搜索 / 可行性夹取」
    /// 这一整套判断可以在任何机器上被直接断言，而不需要一个真驱动。
    /// `HalSpeakerSource::try_trim` 就是 peek + 下混 + **这一个函数** + splice。
    ///
    /// `r` 越短，可削量只会越小 —— 调用方的分段 peek 优化因此**不可能**破坏
    /// 任何不变量，最多是这一 tick 少削一点。
    pub fn decide(r: &[f32], plan: &Plan) -> Option<Decision> {
        let f = super::HAL_FRAME_48K;
        if r.len() < f + T_MIN {
            return None;
        }
        let span = r.len() - f;

        // ---- S 档：从 f 起最长的纯静音跨度 ----
        let sil = silent_span(r, f, X, GATE_SILENT).min(span);
        if sil >= T_MIN {
            // 静音快速通道（§4.2）：删掉纯静音在数学上不改变任何被听见的样本，
            // 所以它绕过令牌桶 —— 让「App 暂停播放后水位仍然很高」几个 tick 收敛。
            let fast = plan.fast.min(sil).min(plan.feasible);
            let w = if fast >= T_MIN {
                Some((fast, false))
            } else {
                let t = plan.budget.min(T_MAX_SILENT).min(sil).min(plan.feasible);
                (t >= T_MIN).then_some((t, true))
            };
            if let Some((tau, charge)) = w {
                // 静音档跳过**搜索**（那一段没有可被破坏的相位），但相关度仍要
                // 算：它决定的是淡化律（等增益 vs 等功率），不是位置。
                return Some(Decision {
                    tau,
                    ncc: ncc_at(r, f, X, tau),
                    tier: Tier::Silent,
                    forced: false,
                    charge,
                });
            }
        }

        // ---- Q / F 档 ----
        let want = plan.budget.min(T_MAX_CORR).min(plan.feasible);
        if want < T_MIN {
            return None;
        }
        let peak = gate_peak(r, f, X, want);
        let tier = if peak < GATE_QUIET { Tier::Quiet } else { Tier::Forced };
        if tier > plan.allow {
            return None; // escalate 还没放行到这一档：再等等，等待很便宜
        }
        let lo = T_MIN.max(want.saturating_sub(DELTA));
        let hi = (want + DELTA).min(plan.feasible).min(span);
        if hi < lo {
            return None;
        }
        let (tau, ncc) = search_tau(r, f, X, want, lo, hi);
        if tau < T_MIN || tau > span || tau > plan.feasible {
            return None;
        }
        Some(Decision { tau, ncc, tier, forced: tier == Tier::Forced, charge: true })
    }

    /// 本 tick 的**第一段** peek 长度：一帧 + 相关档上限 + 搜索余量。
    /// 有声内容（绝大多数 tick）只需要这么多，约 1 344 帧 = 5.4 KB 立体声。
    pub fn peek_base(plan: &Plan) -> usize {
        super::HAL_FRAME_48K + plan.budget.min(T_MAX_CORR) + DELTA
    }

    /// 确认了第一段确实是静音之后，值得再 peek 到多少帧（静音档 / 快速通道）。
    pub fn peek_ext(plan: &Plan) -> usize {
        super::HAL_FRAME_48K + plan.fast.max(plan.budget.min(T_MAX_SILENT))
    }

    /// 在 `[lo, hi]` 里找相关度最高的 `tau`；**并列取距 `want` 最近者**
    /// （并列规则写死，测试才判定得了）。
    ///
    /// 算力：769 个 lag × 192 tap ≈ 1.5×10⁵ MAC。按 §4.3 的速率上限两次 trim
    /// 至少间隔 500 ms ⇒ ≈0.3 MFLOP/s，可以忽略——但**必须在 tick 内完成且
    /// 不分配**（调用方预分配 scratch）。
    pub fn search_tau(
        r: &[f32],
        f: usize,
        x: usize,
        want: usize,
        lo: usize,
        hi: usize,
    ) -> (usize, f32) {
        let hi = hi.min(r.len().saturating_sub(f));
        if lo > hi {
            return (want.min(hi), ncc_at(r, f, x, want.min(hi)));
        }
        let mut best = lo;
        let mut best_ncc = f32::NEG_INFINITY;
        for tau in lo..=hi {
            let n = ncc_at(r, f, x, tau);
            let better = n > best_ncc + 1e-6;
            let tie = (n - best_ncc).abs() <= 1e-6
                && want.abs_diff(tau) < want.abs_diff(best);
            if better || tie {
                best = tau;
                best_ncc = n;
            }
        }
        (best, best_ncc)
    }

    /// 交叉淡化拼接。**输出恒为 `f` 个样本**（不变量 I1，发送节拍是硬约束），
    /// 而读指针要前进 `f + tau` —— 队列深度因此净减少 `tau` 帧。
    ///
    /// ```text
    /// out[i] = r[i]                              , i ∈ [0, f−x)
    /// out[i] = g_a·r[i] + g_b·r[i+tau]           , i ∈ [f−x, f)
    /// u   = 0.5·(1 − cos(π·(k+0.5)/x))
    /// p   = 0.5 + 0.5·clamp(ncc, 0, 1)
    /// g_a = (1−u)^p ,  g_b = u^p
    /// ```
    ///
    /// 一条公式同时给出两种教科书律：
    /// - `ncc = 1`（τ 命中整周期）⇒ `p = 1` ⇒ `g_a + g_b = 1`，**等增益**，
    ///   对相同内容是逐样本恒等变换（除 f32 舍入）；
    /// - `ncc = 0`（噪声类内容）⇒ `p = 0.5` ⇒ `g_a² + g_b² = 1`，**等功率**，
    ///   避免不相关内容在淡化区中点掉 −3 dB。
    ///
    /// 半样本偏移 `+0.5` 让两端斜率为零（C¹ 连续）；线性淡化在两端有斜率跳变，
    /// 会在 `1/x` 附近留下可测的谱纹。
    ///
    /// **明确否掉「淡出到静音再淡入」**：那会在拼接点制造一个宽度 `2x` 的振幅
    /// 凹陷（−∞ dB 谷底），比硬切更难听。
    ///
    /// **明确否掉过零判据**：过零只保证拼接点的**值**连续，不保证斜率与相位
    /// 连续，多谐波内容上会直接失败；而有了交叉淡化 + 相似度搜索之后，过零能
    /// 提供的信息已被完全包含。别「补上」它。
    pub fn splice(r: &[f32], f: usize, x: usize, tau: usize, ncc: f32, out: &mut Vec<f32>) {
        debug_assert!(r.len() >= f + tau, "拼接需要 f+tau 个样本");
        let x = x.min(f);
        let n = r.len();
        out.reserve(f);
        for i in 0..f.saturating_sub(x) {
            out.push(r[i]);
        }
        if x == 0 {
            return;
        }
        let p = 0.5 + 0.5 * ncc.clamp(0.0, 1.0);
        // p 恰好为 1 时**不走 powf**：`powf(v, 1.0)` 未必逐位返回 v，而不变量 I2
        // （相关时逐样本恒等）正押在这一位上。
        let equal_gain = p >= 1.0 - 1e-6;
        for k in 0..x {
            let i = f - x + k;
            let u = 0.5
                * (1.0 - (std::f32::consts::PI * (k as f32 + 0.5) / x as f32).cos());
            let (ga, gb) = if equal_gain {
                (1.0 - u, u)
            } else {
                ((1.0 - u).powf(p), u.powf(p))
            };
            let b = if i + tau < n { r[i + tau] } else { 0.0 };
            out.push(ga * r[i] + gb * b);
        }
    }

    // ------------------------------------------------------------ 控制器

    /// 60 s 滑动最大值。固定 60 个 1 s 桶，无分配。
    #[derive(Debug)]
    struct MaxWin {
        b: [u32; DD_WINDOW_S],
        cur: u64,
        started: bool,
    }

    impl MaxWin {
        fn new() -> MaxWin {
            MaxWin { b: [0; DD_WINDOW_S], cur: 0, started: false }
        }
        fn push(&mut self, sec: u64, v: u32) {
            if !self.started {
                self.started = true;
                self.cur = sec;
            }
            if sec > self.cur {
                let adv = (sec - self.cur).min(DD_WINDOW_S as u64) as usize;
                for i in 1..=adv {
                    self.b[(self.cur as usize).wrapping_add(i) % DD_WINDOW_S] = 0;
                }
                self.cur = sec;
            }
            let s = (self.cur as usize) % DD_WINDOW_S;
            self.b[s] = self.b[s].max(v);
        }
        fn max(&self) -> u32 {
            self.b.iter().copied().max().unwrap_or(0)
        }
        fn reset(&mut self) {
            self.b = [0; DD_WINDOW_S];
            self.started = false;
        }
    }

    /// 本 tick 的削减许可。全部单位是**帧**。
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Plan {
        /// 允许削的最大帧数，已经过目标水位、可行性、令牌三重夹取。
        /// `0` = 本 tick 不削。
        pub budget: usize,
        /// 削完之后的残量绝不允许低于它（`max(15 ms, MaxDrawdown_60s)`）。
        pub d_floor: usize,
        /// 本 tick 最多能削多少而不破 `d_floor`（`A − F − D_floor`）。
        pub feasible: usize,
        /// escalate 允许的最高档。
        pub allow: Tier,
        /// 静音快速通道的一次性削减量（`0` = 不启用）。走这条通道**不扣令牌**：
        /// 删掉纯静音在数学上不改变任何被听见的样本。
        pub fast: usize,
    }

    impl Plan {
        pub const NONE: Plan = Plan {
            budget: 0,
            d_floor: D_FLOOR_MIN,
            feasible: 0,
            allow: Tier::Silent,
            fast: 0,
        };
        /// 本 tick 值不值得去 peek 环。
        pub fn wants_trim(&self) -> bool {
            self.budget >= T_MIN || self.fast >= T_MIN
        }
    }

    /// 一个扬声器环的水位控制器。**单所有者**：只被消费那条环的线程碰，
    /// 所以内部没有一个原子量。
    #[derive(Debug)]
    pub struct Ctl {
        pub mode: Mode,
        /// 上一个 tick 的**读后**残量 `D_n`。`None` = 上一个读数已作废
        /// （追平期、排空、重新附着），`W_n` 这一 tick 不算。
        last_d: Option<u32>,
        /// `D_req(n) = max(0, D_req(n−1) + F − W_n)` 的当前值。
        drawdown: u32,
        dd: MaxWin,
        d_target: f32,
        /// §6.3 惩罚项：已经付出过的欠载代价不会在 60 s 窗口滑出后被遗忘。
        extra: f32,
        tokens: f32,
        trimming: bool,
        high_since_us: Option<u64>,
        ncc_retry: u32,
        /// 本 tick 准不准时，由 `begin_tick` 记下给 `end_tick` 用。
        punctual: bool,
        last_us: Option<u64>,
        next_target_us: u64,
        next_decay_us: u64,
    }

    impl Ctl {
        pub fn new(mode: Mode) -> Ctl {
            Ctl {
                mode,
                last_d: None,
                drawdown: 0,
                dd: MaxWin::new(),
                d_target: D_TARGET_COLD as f32,
                extra: 0.0,
                tokens: 0.0,
                trimming: false,
                high_since_us: None,
                ncc_retry: 0,
                punctual: true,
                last_us: None,
                next_target_us: 0,
                next_decay_us: 0,
            }
        }

        /// 一次不连续（治法 A 的排空、代次 flush、驱动重新附着）之后调用。
        ///
        /// **只作废观测，不清计数**：`W_n = A_{n+1} − D_n` 只在两次读之间「只有
        /// 生产者动过」时才成立，排空之后那个差值是垃圾，喂进递推会把欠载边界
        /// 永久抬高。回撤窗口一并清掉——那是上一段现场的统计。
        pub fn on_discontinuity(&mut self) {
            self.last_d = None;
            self.drawdown = 0;
            self.dd.reset();
            self.trimming = false;
            self.high_since_us = None;
            self.ncc_retry = 0;
        }

        pub fn d_target_frames(&self) -> f32 {
            match self.mode {
                Mode::SafetyNet => D_TARGET_SAFETY as f32,
                _ => self.d_target,
            }
        }

        /// **DLL 伺服的目标水位**，帧。
        ///
        /// 与 [`Ctl::d_target_frames`] 的区别只有一处、但很关键：这里返回的永远是
        /// §3.3 那个由 `MaxDrawdown` 导出的**自适应**值，不受 `mode` 覆盖。
        ///
        /// 理由：`safety_net` 档把 `d_target_frames()` 顶成固定 60 ms，那是
        /// **重同步的触发线**（配 100 ms 上沿），不是稳态该停的水位。DLL 是常规
        /// 执行器，它该收敛到「实测的不欠载边界 + 余量」——也就是
        /// `clamp(1.25×MaxDrawdown_60s + 5 ms + 惩罚, 15, 120)`。让 DLL 去追
        /// 60 ms 等于凭空多留 16 ms 延迟（实测 MaxDrawdown ⇒ D_target ≈ 44 ms）。
        ///
        /// `update_target()` 在**所有**档位下都照跑（`begin_tick` 里它排在
        /// mode 判断之前），所以 `off` / `safety_net` 档这个值一样是活的。
        /// **不引入第二套水位**：全仓库只有 `Ctl::d_target` 一个自适应水位。
        pub fn dll_target_frames(&self) -> f32 {
            self.d_target
        }

        fn w_high(&self) -> f32 {
            match self.mode {
                Mode::SafetyNet => W_HIGH_SAFETY as f32,
                _ => self.d_target + (10 * MSF) as f32,
            }
        }

        pub fn drawdown_frames(&self) -> u32 {
            self.dd.max()
        }
        pub fn tokens_frames(&self) -> f32 {
            self.tokens
        }
        pub fn is_trimming(&self) -> bool {
            self.trimming
        }

        /// §6.3 的伺服：一次短读段就把目标抬高该段时长，并且不让它被窗口滑出。
        pub fn on_underrun(&mut self, run_frames: u32, now_us: u64) {
            self.extra = (self.extra + run_frames as f32).min(EXTRA_MAX);
            self.next_decay_us = now_us + EXTRA_DECAY_US;
        }

        /// 每个 tick 的第一步：观测 + 记账 + 出许可。`avail` 是**读之前**的
        /// `readable()`（即 `A_n`），`cap` 是环容量。
        pub fn begin_tick(
            &mut self,
            now_us: u64,
            punctual: bool,
            avail: u32,
            cap: u32,
        ) -> Plan {
            let dt = match self.last_us {
                Some(t) if now_us > t => (now_us - t) as f32 / 1e6,
                _ => 0.0,
            };
            self.last_us = Some(now_us);
            self.punctual = punctual;
            if self.next_target_us == 0 {
                self.next_target_us = now_us + 1_000_000;
                self.next_decay_us = now_us + EXTRA_DECAY_US;
            }

            // ---- 观测：`W_n = A_{n+1} − D_n`，两个已有读数相减，零成本 ----
            //
            // 不变量 I6：追平期的 `A_n / D_n` 不喂递推。那期间循环背靠背补跑，
            // 水位高是因为我们暂时没读——把它算成「生产侧漏写」会污染回撤估计。
            //
            // ★ `E_n` 必须是常数 `F`，**不许**换成 `48000 × dt`（2026-08-04 实测，
            //   换过一次，上机就发散，已回退）。
            //
            //   动机看着无懈可击：DLL 把 tick 弯成 `10 ms / corr`，一个 tick 里
            //   真实写入的是 `48000 · dt` 而不是 480，所以恒加 480 会把伺服自己的
            //   排水动作记成「生产侧漏写」。`docs/spec-latency-budget.md` §3.2–§3.4
            //   照这个推理给出了「一行修正」，预测 `D_target` 从 27.5 落到 17.5 ms。
            //
            //   **实测的结果是反的。** 换成 `48000 · dt` 之后（mac ↔ 30-win，
            //   真实会话，8 分钟）：
            //
            //   ```text
            //   drawdown_ms   8.9 → 55.9 → 83.9 → 103.4 → 139.7 → 199.0   单调，无上界
            //   dll_target_ms 16.1 → 74.8 → 109.9 → 120.00（撞 D_TARGET_MAX 封顶）
            //   hal_spk 分项  122.6 ms（基线 p50 27.3）   sum_ms 178.3（基线 p50 104.8）
            //   增长率 ≈ 0.5 ms/s = 500 ppm × 480 帧 —— 正是 CORR_CLAMP 的权限
            //   ```
            //
            //   根因是**符号只算对了一半**。§3.3 只推了 `corr > 1`（水位高、伺服排水、
            //   `dt` 变短）那半个周期，在那里恒加 480 确实高估。但另半个周期
            //   `corr < 1`（水位低、伺服蓄水、`dt` 变长）时 `48000 · dt > 480 > W_n`,
            //   于是回撤上涨——而回撤上涨恰恰**抬高** `D_target`，`err = D_target −
            //   depth` 更大，`corr` 更低，`dt` 更长……闭合成**正反馈**：
            //
            //   ```text
            //   drawdown ↑ → D_target ↑ → err ↑ → corr ↓ → dt ↑ → E_n ↑ → drawdown ↑
            //   ```
            //
            //   常数 `F` 之所以稳，正是因为它**与 `corr` 无关**，把这条回路断开了。
            //   §3.3 描述的 ±6.7 ms 摆动是一个**有界限幅环**；换成 `dt` 是拿一个
            //   有界的环去换一个无界的发散。**这不是整定问题，是结构问题**——
            //   任何让 `E_n` 依赖于伺服自己弯出来的 `dt` 的写法都有这条回路。
            //
            //   要消掉「伺服动作被记成漏写」这件事，唯一安全的方向是让参考量
            //   **独立于本环**：例如用累计式 `48000 × (now − t0) − Σ W`（每 tick
            //   无反馈项），而不是逐 tick 的 `48000 × dt`。那是一次重新设计，
            //   不是一行修正。守这一条的回归测试见 `t26` —— 注意 `t26` 只喂了
            //   `corr > 1` 那半个周期，**它对上述发散是盲的**，所以它当时是绿的。
            //   任何重做都必须先补一条 `corr < 1` 的对偶用例。
            if punctual {
                if let Some(d) = self.last_d {
                    let w = avail.saturating_sub(d);
                    self.drawdown = (self.drawdown + super::HAL_FRAME_48K as u32)
                        .saturating_sub(w);
                    self.dd.push(now_us / 1_000_000, self.drawdown);
                }
            }

            // ---- 令牌桶：把「等待」和「限速」两件事统一掉 ----
            let rho = if (avail as f32) > EMERGENCY_FRAC * cap as f32 {
                RHO_EMERGENCY
            } else {
                RHO
            };
            self.tokens = (self.tokens
                + rho * dt * super::HAL_SAMPLE_RATE as f32)
                .min(B_TOK);

            // ---- 每秒重算目标水位 ----
            if now_us >= self.next_target_us {
                self.next_target_us = now_us + 1_000_000;
                self.update_target();
            }
            if now_us >= self.next_decay_us {
                self.next_decay_us = now_us + EXTRA_DECAY_US;
                self.extra = (self.extra - MSF as f32).max(0.0);
            }

            if self.mode == Mode::Off || !punctual || !self.trimming {
                return Plan {
                    d_floor: self.d_floor(),
                    ..Plan::NONE
                };
            }

            // ---- 硬不变量（§6.1）：削完的残量必须 ≥ 实测的「不欠载最小值」 ----
            let f = super::HAL_FRAME_48K;
            let d_floor = self.d_floor();
            let feasible = (avail as usize).saturating_sub(f + d_floor);
            let to_target = (avail as usize).saturating_sub(f + self.d_target_frames() as usize);
            let emergency = (avail as f32) > EMERGENCY_FRAC * cap as f32;
            let allow = if emergency {
                Tier::Forced
            } else {
                self.escalate_tier(now_us)
            };
            let budget = to_target.min(feasible).min(self.tokens.max(0.0) as usize);
            // 静音快速通道只在积压明显大于一次相关档削减量时才值得那次长 peek。
            let fast = if to_target.min(feasible) > T_MAX_CORR {
                to_target.min(feasible).min(PEEK_MAX)
            } else {
                0
            };
            Plan { budget, d_floor, feasible, allow, fast }
        }

        /// 每个 tick 的最后一步：`d_after` 是**读后**残量（`A − 实际读走`）。
        pub fn end_tick(&mut self, d_after: u32) {
            // 追平期的读数**一个都不留**（不变量 I6 的另一半）。只在追平的那些
            // tick 上跳过 `W` 的计算是不够的：追平结束后的第一个准时 tick 会拿
            // 上一次**背靠背**的读数当基准，两次读之间根本没隔一个 tick，算出来
            // 的 `W` 于是偏小 ⇒ 回撤虚高 ⇒ 目标水位被一次卡顿永久推上去。
            self.last_d = self.punctual.then_some(d_after);
            let d = d_after as f32;
            if !self.trimming {
                if d > self.w_high() {
                    self.trimming = true;
                    self.high_since_us = self.last_us;
                    self.ncc_retry = 0;
                }
            } else if d <= self.d_target_frames() {
                self.trimming = false;
                self.high_since_us = None;
                self.ncc_retry = 0;
            }
        }

        /// 一次真正的拼接落地。`charge` = 是否扣令牌（静音快速通道不扣）。
        pub fn on_trim(&mut self, tau: usize, charge: bool) {
            if charge {
                self.tokens = (self.tokens - tau as f32).max(0.0);
            }
            self.ncc_retry = 0;
        }

        /// F 档相关度不达标 ⇒ 再等一个 tick。返回 `true` 表示还能等。
        pub fn retry_ncc(&mut self) -> bool {
            self.ncc_retry += 1;
            self.ncc_retry <= NCC_RETRY_TICKS
        }

        /// `D_floor = max(15 ms, MaxDrawdown_60s)` —— 削完之后的残量绝不许低于
        /// 它。**这个值是实测的，不是估计的**：它自动包含写块量化、生产侧漏写
        /// 与两者叠加的最坏组合。
        pub fn d_floor_frames(&self) -> usize {
            self.d_floor()
        }

        fn d_floor(&self) -> usize {
            D_FLOOR_MIN.max(self.dd.max() as usize)
        }

        fn escalate_tier(&self, now_us: u64) -> Tier {
            let Some(h) = self.high_since_us else {
                return Tier::Silent;
            };
            let t = now_us.saturating_sub(h);
            if t >= 2 * ESCALATE_STEP_US {
                Tier::Forced
            } else if t >= ESCALATE_STEP_US {
                Tier::Quiet
            } else {
                Tier::Silent
            }
        }

        /// `D_target = clamp(ceil(1.25 × MaxDrawdown_60s) + 5 ms + 惩罚, 15, 120)`。
        ///
        /// **允许立刻上调，只允许每秒下调 1 ms。** 不对称是必须的——上调是止血，
        /// 下调是省延迟，宁可慢。
        fn update_target(&mut self) {
            let dd = self.dd.max() as f32;
            let base = (1.25 * dd).ceil() + (5 * MSF) as f32 + self.extra;
            let want = base.clamp(D_TARGET_MIN as f32, D_TARGET_MAX as f32);
            if want > self.d_target {
                self.d_target = want;
            } else {
                self.d_target = (self.d_target - MSF as f32).max(want);
            }
        }
    }

    // ================================================================ 测试
    //
    // 全部与平台无关：原语和控制器都不碰环、不碰时钟，所以这一整套在任何机器上
    // 都跑得起来。**判据本身也要被测**——四条客观判据里有两条配了反向测试
    // （故意把实现写错，断言判据变红），否则一条写错的判据会永远通过。

    #[cfg(test)]
    pub(crate) mod tests {
        use super::*;
        use crate::halbridge::{HAL_FRAME_48K as F, HAL_SAMPLE_RATE as RATE};
        use audiohub_core::dsp;

        // ------------------------------------------------------ 客观判据
        //
        // C1 相关恒等 / C2 斜率不增 / C3 短时电平连续 / C4 带外能量。
        // 一次拼接可能的失败只有四种：值/斜率不连续（咔哒）→ C2；相位不匹配
        // 导致的抵消（电平塌陷）→ C3；相位不匹配导致的谱扩散（毛刺/金属声）
        // → C4；增益律错误导致的电平漂移 → C1。四条同时通过，残差在**时域
        // 幅度、时域包络、频域**三个维度上都被夹住。

        /// C2：`max|x[n] − x[n−1]|`。
        ///
        /// 阈值 1.10 的分离度：1 kHz 正弦 @48k 的正常最大一阶差分
        /// = `A·2π·1000/48000 ≈ 0.131A`；最坏相位的硬切给出 `≈2A`，比值 15×。
        /// 10 % 余量与 15× 之间隔着一个数量级，阈值不敏感。
        pub(crate) fn max_slope(x: &[f32]) -> f32 {
            x.windows(2).fold(0.0f32, |m, w| m.max((w[1] - w[0]).abs()))
        }

        /// C3：短时 RMS 的连续性，dB。
        ///
        /// **窗取 2 ms（不是规格草案里的 5 ms）**：交叉淡化只有 4 ms 宽，5 ms 的
        /// 窗比它还宽，会把自己要抓的那个凹陷平均掉——已知会塌 3 dB 的等增益
        /// 误用在 5 ms 窗下只量到 0.97 dB，判据于是永远不响。窗必须比被测事件
        /// 窄，这是判据能成立的前提，不是口味问题。
        ///
        /// 邻居取**一个整窗之外**的左右两窗（不是相邻 hop）：相邻窗重叠 75 %，
        /// 它们之间的差值对一个 4 ms 的事件几乎为零。
        pub(crate) fn c3_worst_db(x: &[f32], win: usize, hop: usize) -> f32 {
            let lv: Vec<f32> = (0..)
                .map(|i| i * hop)
                .take_while(|&s| s + win <= x.len())
                .map(|s| {
                    let e: f32 = x[s..s + win].iter().map(|v| v * v).sum();
                    (e / win as f32).sqrt().max(1e-9)
                })
                .collect();
            let k = (win / hop).max(1);
            if lv.len() <= 2 * k {
                return 0.0;
            }
            let db = |v: f32| 20.0 * v.log10();
            (k..lv.len() - k)
                .map(|i| (db(lv[i]) - 0.5 * (db(lv[i - k]) + db(lv[i + k]))).abs())
                .fold(0.0f32, f32::max)
        }

        /// C4：逐 10 ms 窗（480 点，1 kHz 恰为整数 bin ⇒ 无泄漏底噪）的
        /// 基频功率 / 其余功率，取最差的一窗，dB。
        pub(crate) fn c4_min_snr_db(x: &[f32], f0: f32) -> f32 {
            let win = 480usize;
            // **必须重叠**（hop = win/2）。拼接点恰好落在帧边界上，而帧长就是
            // 480 —— 不重叠的话每一个分析窗内部都是干净的单音，一个真实的硬切
            // 会从窗与窗的缝里整个漏过去（实测：不重叠量到 75.8 dB，重叠之后
            // 才量到它真实的样子）。
            let hop = win / 2;
            let mut worst = f32::INFINITY;
            let mut s = 0usize;
            while s + win <= x.len() {
                let c = &x[s..s + win];
                s += hop;
                let p = dsp::goertzel_power(c, RATE, f0) as f64;
                let total: f64 =
                    c.iter().map(|&v| (v as f64) * (v as f64)).sum::<f64>() / win as f64 / 2.0;
                if total < 1e-12 {
                    continue; // 静音窗没有信噪比可言
                }
                let noise = (total - p).max(0.0) + 1e-15;
                worst = worst.min((10.0 * (p.max(1e-15) / noise).log10()) as f32);
            }
            worst
        }

        // ------------------------------------------------------ 测试素材

        fn sine(f0: f32, n: usize, amp: f32) -> Vec<f32> {
            dsp::gen_sine(f0, RATE, n, amp)
        }

        /// 确定性伪随机（不引入 rand 依赖）。
        struct Lcg(u64);
        impl Lcg {
            fn next_u32(&mut self) -> u32 {
                self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                (self.0 >> 33) as u32
            }
            fn unit(&mut self) -> f32 {
                self.next_u32() as f32 / u32::MAX as f32 * 2.0 - 1.0
            }
        }

        /// 一条连续输入按 10 ms 一帧交出去，在第 `at` 帧做**一次**拼接。
        /// 返回 `(输出流, τ, ncc)`。`forced_*` 用来构造反向测试。
        fn stream_one_splice(
            input: &[f32],
            x: usize,
            at: usize,
            want: usize,
            forced_tau: Option<usize>,
            forced_ncc: Option<f32>,
        ) -> (Vec<f32>, usize, f32) {
            let mut out = Vec::new();
            let (mut c, mut n) = (0usize, 0usize);
            let (mut tau_used, mut ncc_used) = (0usize, 0.0f32);
            while c + F <= input.len() {
                if n == at {
                    let (tau, ncc) = match forced_tau {
                        Some(t) => (t, ncc_at(&input[c..], F, x.max(1), t)),
                        None => search_tau(
                            &input[c..],
                            F,
                            x,
                            want,
                            T_MIN.max(want.saturating_sub(DELTA)),
                            want + DELTA,
                        ),
                    };
                    if c + F + tau > input.len() {
                        break;
                    }
                    let ncc = forced_ncc.unwrap_or(ncc);
                    splice(&input[c..], F, x, tau, ncc, &mut out);
                    c += F + tau;
                    tau_used = tau;
                    ncc_used = ncc;
                } else {
                    out.extend_from_slice(&input[c..c + F]);
                    c += F;
                }
                n += 1;
            }
            (out, tau_used, ncc_used)
        }

        // ============================================== 原语层（规格 §9.2 1–7）

        /// 不变量 I3：直流进、直流出。权重不归一的第一现场。
        #[test]
        fn t01_a_dc_input_survives_a_trim_unchanged() {
            let input = vec![0.37f32; F * 4];
            let (out, tau, _) = stream_one_splice(&input, X, 1, T_MAX_CORR, None, None);
            assert!(tau >= T_MIN, "确实削了一次, tau={tau}");
            for (i, &v) in out.iter().enumerate() {
                assert!(
                    (v - 0.37).abs() <= 1e-6,
                    "样本 {i} = {v} —— 直流被改了，说明两侧增益不归一"
                );
            }
        }

        /// C1 / 不变量 I2：τ 命中整周期时输出与「未 trim 的同一时间轴」逐样本相等。
        /// 这是等增益律（`NCC = 1 ⇒ p = 1 ⇒ g_a + g_b = 1`）的数学后果。
        #[test]
        fn t02_a_periodic_signal_is_bit_identical_when_tau_hits_a_whole_period() {
            let input = sine(1000.0, F * 6, 0.8); // 周期恰好 48 帧
            let (out, tau, ncc) = stream_one_splice(&input, X, 1, T_MAX_CORR, None, None);
            assert_eq!(tau, 480, "1 kHz 的整周期倍数里离请求值最近的就是 480");
            assert!(ncc > 0.999, "整周期命中 ⇒ 相关度必须是 1, got {ncc}");
            let err = out
                .iter()
                .zip(input.iter())
                .fold(0.0f32, |m, (a, b)| m.max((a - b).abs()));
            assert!(err <= 1e-6, "C1 逐样本恒等被破坏, max|err| = {err:e}");
        }

        /// 请求值不是周期整数倍时，搜索仍然命中整周期；并列按「距请求值最近」定夺。
        #[test]
        fn t03_the_search_snaps_to_a_whole_period_and_breaks_ties_toward_the_request() {
            let input = sine(1000.0, F * 6, 0.8);
            let (_, tau, ncc) = stream_one_splice(&input, X, 1, 470, None, None);
            assert!(
                tau == 432 || tau == 480,
                "只有 48 的整数倍相关度才是 1, got {tau}"
            );
            assert_eq!(tau, 480, "|480−470| = 10 < |432−470| = 38，并列取近的那个");
            assert!(ncc > 0.999);
        }

        /// **反向测试**：把交叉淡化拿掉（X = 0）＝硬切，C2 必须变红。
        ///
        /// 没有这一条，C2 可能因为写错而永远通过。τ 取 492（1 kHz 的四分之一
        /// 周期偏移）—— 相位错开 90°，是搜索永远不会选、而硬切一定会露馅的点位。
        #[test]
        fn t04_a_hard_cut_is_caught_by_c2_and_a_crossfade_is_not() {
            let input = sine(1000.0, F * 6, 0.8);
            let reference = max_slope(&input);
            let (soft, _, _) = stream_one_splice(&input, X, 1, 0, Some(492), None);
            let (hard, _, _) = stream_one_splice(&input, 0, 1, 0, Some(492), None);
            let r_soft = max_slope(&soft) / reference;
            let r_hard = max_slope(&hard) / reference;
            assert!(r_soft <= 1.10, "交叉淡化必须通过 C2, got {r_soft:.3}×");
            assert!(
                r_hard > 1.10,
                "**判据自身失效**：硬切没被 C2 抓住 (got {r_hard:.3}×)，\
                 那这条判据以后永远会通过"
            );
            assert!(r_hard > 5.0, "硬切与阈值之间应有一个数量级的分离度, got {r_hard:.3}×");
        }

        /// 不相关内容必须用**等功率**律，否则电平在拼接点塌陷。
        /// 后半是**反向测试**：强行用等增益（p = 1），C3 必须变红。
        ///
        /// 素材用 1 kHz 正弦 + τ = 492（四分之一周期）⇒ 两段恰好是 sin 与 cos：
        /// 相关度精确为 0、包络恒定，于是 RMS 判据没有统计噪声，两个方向都是
        /// 确定性的。
        #[test]
        fn t05_uncorrelated_content_needs_the_equal_power_law() {
            let input = sine(1000.0, F * 6, 0.8);
            let (eq_power, _, ncc) = stream_one_splice(&input, X, 1, 0, Some(492), None);
            assert!(ncc.abs() < 0.05, "四分之一周期偏移 ⇒ 相关度 ≈ 0, got {ncc}");
            let (eq_gain, _, _) = stream_one_splice(&input, X, 1, 0, Some(492), Some(1.0));
            let win = 2 * MSF; // 2 ms
            let hop = MSF / 2; // 0.5 ms
            let d_power = c3_worst_db(&eq_power, win, hop);
            let d_gain = c3_worst_db(&eq_gain, win, hop);
            assert!(d_power <= 1.5, "等功率律不该有电平凹陷, got {d_power:.2} dB");
            assert!(
                d_gain > 1.5,
                "**判据自身失效**：等增益误用在不相关内容上没被 C3 抓住 \
                 (got {d_gain:.2} dB)"
            );
        }

        /// 不变量 I1：输出恒为一帧。发送节拍是硬约束。
        #[test]
        fn t06_the_output_is_always_exactly_one_frame() {
            let input = sine(440.0, F + T_MAX_SILENT + DELTA + 16, 0.5);
            for tau in [T_MIN, 300, T_MAX_CORR, 1000, T_MAX_SILENT] {
                for x in [0usize, 96, X, 384, F] {
                    let mut out = Vec::new();
                    splice(&input, F, x, tau, 0.5, &mut out);
                    assert_eq!(out.len(), F, "tau={tau} x={x} 输出了 {} 个样本", out.len());
                }
            }
        }

        /// C4：拼接窗的带外能量。正向（搜索命中）与反向（硬切）一起断言，
        /// 否则「SNR ≥ 40 dB」可能只是因为这条判据算错了。
        #[test]
        fn t07_a_pure_tone_keeps_its_spectrum_across_a_splice() {
            let input = sine(1000.0, F * 8, 0.8);
            let (good, tau, _) = stream_one_splice(&input, X, 2, T_MAX_CORR, None, None);
            assert_eq!(tau, 480);
            let snr_good = c4_min_snr_db(&good, 1000.0);
            assert!(snr_good >= 40.0, "搜索命中整周期时不该有谱扩散, got {snr_good:.1} dB");
            let (bad, _, _) = stream_one_splice(&input, 0, 2, 0, Some(492), None);
            let snr_bad = c4_min_snr_db(&bad, 1000.0);
            assert!(
                snr_bad < 40.0,
                "**判据自身失效**：硬切的谱扩散没被 C4 抓住, got {snr_bad:.1} dB"
            );
        }

        // ============================================== 门控层（规格 §9.2 8–10）

        /// 门控保护瞬态：一片静音里的一个 click 不许被剪掉。
        #[test]
        fn t08_the_gate_protects_a_transient() {
            let mut buf = vec![0.0f32; F + T_MAX_SILENT + DELTA];
            let click = F + 600; // 落在会被丢弃的区间里
            buf[click] = 0.9;
            // 静音跨度只到 click 为止 ⇒ 可削量被它挡住。
            let sil = silent_span(&buf, F, X, GATE_SILENT);
            assert_eq!(sil, 600, "静音跨度必须在瞬态那一刻停住, got {sil}");
            let plan = Plan {
                budget: T_MAX_SILENT,
                d_floor: D_FLOOR_MIN,
                feasible: T_MAX_SILENT,
                allow: Tier::Silent,
                fast: T_MAX_SILENT,
            };
            let d = decide(&buf, &plan).expect("静音段够长，应当削");
            assert!(d.tau <= 600, "削到了瞬态上：tau={} > 600", d.tau);
            // 而且 click 在输出之后仍然原样活着（它在下一帧里）。
            let mut out = Vec::new();
            splice(&buf, F, X, d.tau, d.ncc, &mut out);
            let rest = &buf[F + d.tau..];
            assert!(
                rest.iter().any(|v| (*v - 0.9).abs() < 1e-6),
                "瞬态被削掉了 —— 它必须原样存活在读指针之后"
            );
        }

        /// 门控测的是**峰值**不是 RMS。
        ///
        /// 「大片静音里一个 −20 dBFS 的瞬态」用 RMS 量会落在 −60 dBFS 以下而被
        /// 放行；用峰值量则一定被拦住。这条测的就是这个差别。
        #[test]
        fn t09_the_gate_measures_peak_not_rms() {
            // 12 000 帧 = 250 ms = 一次静音快速通道能削掉的最大跨度，正是这条
            // 判据真正要守的那个尺度。
            let n = F + PEEK_MAX;
            let mut buf = vec![0.0f32; n];
            buf[F + 100] = 0.1; // −20 dBFS 单样本
            let rms = (buf[F..].iter().map(|v| v * v).sum::<f32>() / (n - F) as f32).sqrt();
            assert!(
                rms < GATE_SILENT,
                "构造前提：这一段的 RMS ({rms:e}) 确实低于静音阈值 —— \
                 用 RMS 实现的门控会放行"
            );
            assert_eq!(
                silent_span(&buf, F, X, GATE_SILENT),
                100,
                "峰值门控必须在那个瞬态处停住"
            );
        }

        /// 门控区间必须**含淡化区** `[F−X, F)`：那里的瞬态会被搓成重影。
        #[test]
        fn t10_the_gate_covers_the_crossfade_region_too() {
            let mut buf = vec![0.0f32; F + 4800];
            buf[F - X / 2] = 0.5; // 落在淡化区里，不在被丢弃的区间里
            assert_eq!(
                silent_span(&buf, F, X, GATE_SILENT),
                0,
                "淡化区里的瞬态没被门控看见 —— 只量被丢弃的那一段是错的"
            );
            // 挪到淡化区之前就与本次拼接无关了。
            let mut ok = vec![0.0f32; F + 4800];
            ok[F - X - 1] = 0.5;
            assert!(silent_span(&ok, F, X, GATE_SILENT) > 0);
        }

        // ============================================ 控制器层（规格 §9.2 11–17）

        /// 假环 + 脚本化生产者 + 虚拟时钟。跑的是**真的** `Ctl` / `decide` /
        /// `splice`，只有环和时钟是假的。
        struct Sim {
            /// 周期性素材，按 `(r + i) % len` 取样。避免为 10 分钟虚拟时间分配
            /// 一条 115 MB 的磁带。
            pattern: Vec<f32>,
            scratch: Vec<f32>,
            w: usize,
            r: usize,
            cap: usize,
            ctl: Ctl,
            now_us: u64,
            out: Option<Vec<f32>>,
            short_frames: u64,
            short_events: u64,
            in_short: bool,
            trims: u64,
            trimmed: u64,
            forced: u64,
            min_d_seen: usize,
            max_d_seen: usize,
            floor_violations: u64,
        }

        impl Sim {
            fn new(pattern: Vec<f32>, level0: usize, mode: Mode, collect: bool) -> Sim {
                Sim {
                    scratch: vec![0.0; F + PEEK_MAX + DELTA],
                    pattern,
                    w: level0,
                    r: 0,
                    cap: crate::halbridge::HAL_RING_FRAMES as usize,
                    ctl: Ctl::new(mode),
                    now_us: 0,
                    out: collect.then(Vec::new),
                    short_frames: 0,
                    short_events: 0,
                    in_short: false,
                    trims: 0,
                    trimmed: 0,
                    forced: 0,
                    min_d_seen: usize::MAX,
                    max_d_seen: 0,
                    floor_violations: 0,
                }
            }

            fn level(&self) -> usize {
                self.w - self.r
            }

            fn fill(&mut self, n: usize) -> &[f32] {
                let p = self.pattern.len();
                for i in 0..n {
                    self.scratch[i] = self.pattern[(self.r + i) % p];
                }
                &self.scratch[..n]
            }

            /// 走 10 ms 一格的正常节拍。
            fn tick(&mut self, produce: usize, punctual: bool) {
                let t = self.now_us;
                self.tick_at(t, produce, punctual);
                self.now_us = t + 10_000;
            }

            /// 显式给时刻。**追平期必须用它**：那时循环是背靠背补跑的，墙钟几乎
            /// 不走，生产者也就写不进东西 —— 用 `tick()` 会把追平模拟成一串正常
            /// 节拍，那恰恰把要测的东西抹掉了。
            fn tick_at(&mut self, now_us: u64, produce: usize, punctual: bool) {
                self.now_us = now_us;
                // 生产者：环满则短写（驱动侧 drop-newest，本进程观测不到）
                self.w = (self.w + produce).min(self.r + self.cap);
                let avail = self.level() as u32;
                let plan = self.ctl.begin_tick(self.now_us, punctual, avail, self.cap as u32);
                let floor = self.ctl.d_floor_frames();
                let mut tau = 0usize;
                if plan.wants_trim() {
                    let mut n = trim_peek_len(&plan, avail as usize, self.scratch.len());
                    if n >= F + T_MIN {
                        let ext = peek_ext(&plan).min(avail as usize).min(self.scratch.len());
                        let all_silent = {
                            let r = self.fill(n);
                            silent_span(r, F, X, GATE_SILENT) >= n - F
                        };
                        if ext > n && all_silent {
                            n = ext;
                        }
                        let (dec, buf_len) = {
                            let r = self.fill(n);
                            (decide(r, &plan), r.len())
                        };
                        let _ = buf_len;
                        if let Some(d) = dec {
                            if !(d.forced && d.ncc < NCC_MIN_F && self.ctl.retry_ncc()) {
                                if let Some(o) = self.out.as_mut() {
                                    let p = self.pattern.len();
                                    for i in 0..n {
                                        self.scratch[i] = self.pattern[(self.r + i) % p];
                                    }
                                    splice(&self.scratch[..n], F, X, d.tau, d.ncc, o);
                                }
                                tau = d.tau;
                                self.ctl.on_trim(d.tau, d.charge);
                                self.trims += 1;
                                self.trimmed += d.tau as u64;
                                if d.forced {
                                    self.forced += 1;
                                }
                            }
                        }
                    }
                }
                if tau == 0 {
                    let got = F.min(self.level());
                    if let Some(o) = self.out.as_mut() {
                        let p = self.pattern.len();
                        for i in 0..got {
                            o.push(self.pattern[(self.r + i) % p]);
                        }
                        for _ in got..F {
                            o.push(0.0);
                        }
                    }
                    if got < F {
                        self.short_frames += (F - got) as u64;
                        if !self.in_short {
                            self.short_events += 1;
                        }
                        self.in_short = true;
                    } else {
                        self.in_short = false;
                    }
                    self.r += got;
                } else {
                    self.in_short = false;
                    self.r += F + tau;
                }
                let d_after = self.level();
                if tau > 0 && d_after < floor {
                    self.floor_violations += 1;
                }
                self.min_d_seen = self.min_d_seen.min(d_after);
                self.max_d_seen = self.max_d_seen.max(d_after);
                self.ctl.end_tick(d_after as u32);
            }
        }

        /// 与 `HalSpeakerSource::try_trim` 同一条第一段 peek 长度。
        fn trim_peek_len(plan: &Plan, avail: usize, scratch: usize) -> usize {
            peek_base(plan).min(avail).min(scratch)
        }

        fn loud_pattern() -> Vec<f32> {
            sine(1000.0, 48, 0.5) // 一个整周期，循环取样即是连续正弦
        }

        /// 11：**欠载不变量**。随机块大小、随机相位、随机生产侧停顿，10 分钟
        /// 虚拟时间。断言一个样本的静音都没补过，且每次 trim 之后的残量都不低于
        /// 实测的 `D_floor`。
        #[test]
        fn t11_trimming_never_causes_an_underrun() {
            let mut rng = Lcg(0xA11CE);
            let mut sim = Sim::new(loud_pattern(), 60 * MSF, Mode::Active, false);
            let blocks = [128usize, 256, 384, 512];
            let (mut owed, mut stall) = (0usize, 0u32);
            // 停顿的**节律**是固定的（6–12 s 一次），只有长度和相位随机。
            // 理由写在规格 §9.2 的前提里：断言「一个样本静音都没补」只在
            // `G ≤ 已被 MaxDrawdown 覆盖` 时成立，而覆盖靠的是 60 s 观测窗里
            // 确实见过同量级的停顿。真的一整分钟风平浪静之后突然来一次更大的
            // 停顿，那是 §6.3 的伺服该管的事，由 t20 单独断言。
            let mut next_stall = 100u32;
            for t in 0..60_000u32 {
                owed += F;
                if stall > 0 {
                    stall -= 1;
                    sim.tick(0, true);
                    continue;
                }
                if t == next_stall {
                    stall = 1 + rng.next_u32() % 3; // ≤30 ms
                    next_stall = t + 600 + rng.next_u32() % 600;
                }
                let b = blocks[(rng.next_u32() % 4) as usize];
                let mut produce = 0;
                while owed >= b {
                    produce += b;
                    owed -= b;
                }
                sim.tick(produce, true);
            }
            assert!(sim.trims > 0, "这 10 分钟里必须真的削过，否则这条测试是空的");
            assert_eq!(
                sim.short_frames, 0,
                "trim 削过头了：补了 {} 帧静音（{} 段）",
                sim.short_frames, sim.short_events
            );
            assert_eq!(sim.floor_violations, 0, "有 trim 把残量削到了 D_floor 以下");
            assert!(sim.min_d_seen > 0, "水位见底了");
        }

        /// 20：一次大的生产侧漏写把目标水位**永久**抬到足以吸收它的高度。
        ///
        /// 这是 t11 的补集，也是整套自适应存在的理由：目标水位不是拍脑袋的常数，
        /// 而是由实测回撤导出的。同一段素材、同一条控制器，只因为见过一次
        /// 120 ms 的漏写，收敛点就必须停在高得多的地方 —— 否则下一次同样的漏写
        /// 就是一次听得见的断续。
        ///
        /// 顺带钉死一件物理事实：**水位自己长不回来**。生产者和消费者是同一个
        /// 时钟，漏写掉的那 120 ms 没有任何机制会补回来 —— 所以「事后把目标抬
        /// 高」根本不足以自愈，唯一有用的是「事前不要削到那么低」。
        #[test]
        fn t20_a_big_producer_stall_permanently_raises_the_target() {
            let run = |stall_at: Option<u32>| {
                let mut sim = Sim::new(loud_pattern(), 400 * MSF, Mode::Active, false);
                let mut stall = 0u32;
                for t in 0..5_000u32 {
                    if Some(t) == stall_at {
                        stall = 12; // 120 ms
                    }
                    if stall > 0 {
                        stall -= 1;
                        sim.tick(0, true);
                    } else {
                        sim.tick(F, true);
                    }
                }
                sim
            };
            let calm = run(None);
            let hit = run(Some(10));
            assert_eq!(hit.short_frames, 0, "400 ms 的存量装得下一次 120 ms 漏写");
            assert!(
                hit.ctl.d_floor_frames() >= 120 * MSF,
                "D_floor 没跟上实测回撤: {} 帧",
                hit.ctl.d_floor_frames()
            );
            let (a, b) = (calm.level() as f32 / MSF as f32, hit.level() as f32 / MSF as f32);
            assert!(
                b >= a + 60.0,
                "见过 120 ms 漏写之后仍然收敛到 {b:.1} ms（对照组 {a:.1} ms）                 —— 目标水位没有跟着实测回撤走，下一次漏写就是一次断续"
            );
            assert!(a <= 60.0, "对照组该收敛到冷启动量级, got {a:.1} ms");
        }

        /// §6.3 的惩罚项：已经付出过的欠载代价不许被 60 s 观测窗滑出后遗忘。
        #[test]
        fn t21_an_underrun_raises_the_target_beyond_the_observation_window() {
            let mut ctl = Ctl::new(Mode::Active);
            ctl.begin_tick(0, true, 1440, 24_000);
            ctl.end_tick(1440);
            ctl.begin_tick(1_000_000, true, 1440, 24_000);
            let cold = ctl.d_target_frames();
            ctl.on_underrun(40 * MSF as u32, 1_000_000);
            ctl.begin_tick(2_000_000, true, 1440, 24_000);
            let after = ctl.d_target_frames();
            assert!(
                after >= cold + (35 * MSF) as f32,
                "一次 40 ms 的短读段没有抬高目标: {cold:.0} -> {after:.0} 帧"
            );
            // 衰减是每 30 s 才 1 ms —— 慢是故意的，代价已经付过一次了。
            let mut t = 2_000_000u64;
            for _ in 0..10 {
                t += 30_000_000;
                ctl.begin_tick(t, true, 1440, 24_000);
            }
            assert!(
                ctl.d_target_frames() >= after - (11 * MSF) as f32,
                "惩罚项衰减得太快"
            );
        }

        /// 12：**速率上限**。任意 10 分钟窗口内 `ΣT ≤ ρ×窗口 + B_tok`。
        ///
        /// 生产者**持续**多写 20 %，于是控制器永远有得削 —— 这是必须的：一次性
        /// 的存量收敛之后就没有压力了，那种场景下即使把令牌桶整个拿掉，总量也
        /// 只是那一次存量，测不出任何东西（第一版就是这么写的，注入验证时它
        /// 纹丝不动地通过了）。有了持续压力，没有限速器的实现会以 T_MAX_CORR
        /// 的速度一直削，总量差着一个半数量级。
        ///
        /// 用有声素材：静音快速通道按设计绕过令牌桶（规格 §4.2），那是另一条
        /// 有独立可闻性证明的路径。
        #[test]
        fn t12_the_token_bucket_caps_the_time_compression_rate() {
            let mut sim = Sim::new(loud_pattern(), 400 * MSF, Mode::Active, false);
            let ticks = 60_000u32;
            for _ in 0..ticks {
                sim.tick(F + F / 5, true); // 持续 +20 % 的生产
            }
            let secs = ticks as f64 / 100.0;
            let bound = RHO_EMERGENCY as f64 * secs * RATE as f64 + B_TOK as f64;
            assert!(
                (sim.trimmed as f64) <= bound,
                "10 分钟里削了 {} 帧（= {:.1} % 的时间压缩），上限是 {bound:.0}",
                sim.trimmed,
                sim.trimmed as f64 / (secs * RATE as f64) * 100.0
            );
            assert!(
                sim.trimmed as f64 >= 0.5 * bound,
                "压力这么大却几乎没削，这条测试是空的: {} 帧",
                sim.trimmed
            );
            // ...而稳态（生产者不再多写）下占空比应当回到 0：收敛之后不该再削。
            let mut calm = Sim::new(loud_pattern(), 400 * MSF, Mode::Active, false);
            for _ in 0..6_000 {
                calm.tick(F, true);
            }
            let after = calm.trimmed;
            for _ in 0..6_000 {
                calm.tick(F, true);
            }
            assert_eq!(calm.trimmed, after, "已经收敛到目标还在削 —— 迟滞没起作用");
        }

        /// 22：`D_floor` 是**第二道**防线，只在 `D_target` 撞上 120 ms 上限之后
        /// 才真正吃劲 —— 而那正是生产侧病得最重、最需要它的时候。
        ///
        /// 这一条是注入验证逼出来的：把 `d_floor()` 改成恒 0，t11/t13 全都照样
        /// 通过（`D_target = 1.25×回撤 + 5 ms` 本来就 > 回撤，`D_floor` 在那些
        /// 场景里根本不 binding）。**没有这一条，欠载保护的下半截是没有测试的。**
        ///
        /// 现场：生产者每 5 秒漏写 150 ms 再一次性补上（这正是 §3.2 的回撤递推
        /// 建模的形态）。`MaxDrawdown = 150 ms > D_target` 的 120 ms 上限，于是
        /// 收敛点由 `D_floor` 决定；把它拿掉，控制器会削到 120 ms，下一次漏写
        /// 就是一次听得见的断续。
        #[test]
        fn t22_the_measured_floor_binds_once_the_target_saturates() {
            let stall = 15usize; // 150 ms
            let mut sim = Sim::new(loud_pattern(), 480 * MSF, Mode::Active, false);
            for round in 0..12 {
                for _ in 0..500 {
                    sim.tick(F, true); // 5 秒正常
                }
                if round == 0 {
                    continue; // 第一轮先让它开始收敛
                }
                for _ in 0..stall {
                    sim.tick(0, true); // 生产侧漏写
                }
                sim.tick(F * (stall + 1), true); // 一次性补上：水位回到漏写之前
            }
            assert!(
                sim.ctl.d_floor_frames() >= stall * F,
                "D_floor 没跟上实测回撤: {} 帧",
                sim.ctl.d_floor_frames()
            );
            assert_eq!(
                sim.ctl.d_target_frames() as usize,
                D_TARGET_MAX,
                "前提：这个现场里目标水位确实撞到了 120 ms 上限，                 于是收敛点只能由 D_floor 决定"
            );
            assert_eq!(
                sim.short_frames, 0,
                "削过头了：补了 {} 帧静音 —— D_floor 没挡住",
                sim.short_frames
            );
            assert!(sim.trims > 0, "这一轮里必须真的削过");
            assert!(
                sim.level() >= stall * F,
                "收敛点低于实测回撤（{} < {}）—— 下一次漏写就是一次断续",
                sim.level(),
                stall * F
            );
        }

        /// 13：**收敛**。存量 400 ms 必须在算式给出的时间内排掉，且全程不下冲。
        #[test]
        fn t13_a_large_backlog_converges_without_undershooting() {
            let mut sim = Sim::new(loud_pattern(), 400 * MSF, Mode::Active, false);
            let mut converged: Option<u32> = None;
            for t in 0..6_000u32 {
                sim.tick(F, true);
                if converged.is_none()
                    && (sim.level() as f32) <= sim.ctl.d_target_frames() + (10 * MSF) as f32
                {
                    converged = Some(t);
                }
            }
            let t = converged.expect("60 秒里必须收敛");
            // 400→300 用 ρ=3 %（≈3.3 s），300→目标用 ρ=1 %；素材全程有声，
            // 所以 escalate 到 F 档要等 10 s。合起来约 36 s。
            assert!(
                t <= 4_500,
                "收敛用了 {:.1} 秒，超出算式给出的量级",
                t as f32 / 100.0
            );
            assert_eq!(sim.short_frames, 0, "收敛过程中欠载了");
            assert_eq!(sim.floor_violations, 0);
            assert!(
                sim.min_d_seen >= D_FLOOR_MIN,
                "下冲到了 {} 帧（D_floor = {}）",
                sim.min_d_seen,
                D_FLOOR_MIN
            );
        }

        /// 14：**追平期不 trim**（不变量 I6），且追平期的读数不污染回撤递推。
        ///
        /// 这是整套水位逻辑里最容易写错的一条：追平期的高水位是假象——它高是
        /// 因为我们暂时没读，不是因为积压。在那些 tick 上削，紧接着就欠载。
        #[test]
        fn t14_no_trimming_while_catching_up() {
            let mut sim = Sim::new(loud_pattern(), 400 * MSF, Mode::Active, false);
            for _ in 0..2_000 {
                sim.tick(F, true); // 先让它进入 TRIMMING 并把 F 档 escalate 出来
            }
            assert!(sim.ctl.is_trimming(), "前提：此刻处在 TRIMMING 态");
            let trims_before = sim.trims;
            let dd_before = sim.ctl.drawdown_frames();
            // 一次 108 ms 的消费侧停顿：生产者照写，我们一个 tick 都没跑。
            sim.w = (sim.w + 11 * F).min(sim.r + sim.cap);
            let t0 = sim.now_us + 110_000;
            // 追平的那几个 tick：背靠背补跑（每个 50 µs），墙钟几乎不走，
            // 所以生产者一个样本都写不进来。
            for i in 0..11u64 {
                sim.tick_at(t0 + i * 50, 0, false);
            }
            assert_eq!(
                sim.trims, trims_before,
                "追平期削了 {} 次 —— 那些水位是假高，削完立刻欠载",
                sim.trims - trims_before
            );
            assert_eq!(
                sim.ctl.drawdown_frames(),
                dd_before,
                "追平期的读数污染了 MaxDrawdown 递推 —— 欠载边界会被永久抬高"
            );
            // ...而且**追平结束后的第一个准时 tick 也不许**用那个基准：它只隔了
            // 「睡到 deadline」的那一小段（这里 3 ms），生产者相应地只写进 3 ms，
            // 拿它当一整个 tick 的 W 会凭空多出 7 ms 的回撤 ⇒ 目标水位被每一次
            // 卡顿再往上推一截。这是 I6 最容易漏掉的那一半。
            sim.now_us = t0 + 3_000;
            sim.tick(3 * MSF, true);
            assert_eq!(
                sim.ctl.drawdown_frames(),
                dd_before,
                "追平后的第一个准时 tick 拿背靠背的读数当了基准"
            );
        }

        /// 23：**可行性夹取本身**必须守住工作储备 —— `T_feasible = A − F − D_floor`。
        ///
        /// 为什么单独立一条：t15/t20/t22 断言的是 `D_floor` 这个**数**算得对，
        /// 但没有一条断言那个数**被用上了**。独立注入验证过这个缺口是真的：把
        /// `plan()` 里的 `feasible` 改成 `avail`（trim 可以削进工作储备、直到把环
        /// 掏空），全工作区 312 条测试**一条都不红**。原因是常见现场里
        /// `to_target` 与令牌先夹住了，`feasible` 根本不 binding —— 它只在
        /// 「目标高、水位低」时才成为唯一的那道闸，而那正是欠载最危险的时刻。
        ///
        /// 所以这里直接扫参数空间，把 §4 的那条等式钉在 `begin_tick` 的出口上。
        #[test]
        fn t23_the_feasibility_clamp_never_eats_the_working_reserve() {
            let cap = 24_000u32;
            let mut binding = 0usize; // 有多少个格点上 `feasible` 是唯一那道闸
            for &starve in &[0usize, 1, 3, 10, 20] {
                for &avail in &[
                    0u32, 1, 480, 700, 720, 721, 1_200, 1_201, 1_440, 2_000, 5_000,
                    12_000, 20_000, 23_000, 24_000,
                ] {
                    let mut c = Ctl::new(Mode::Active);
                    let mut t = 0u64;
                    // 先攒够令牌（规律生产，回撤保持 0），确保被测的是可行性
                    // 这一道闸，而不是限速那一道。
                    for _ in 0..3_000 {
                        c.begin_tick(t, true, 12_000, cap);
                        c.end_tick(12_000 - F as u32);
                        t += 10_000;
                    }
                    // 再注入若干个「生产者一个样本都没写」的 tick，把实测回撤
                    // （进而 `D_floor`）推上去。
                    for _ in 0..starve {
                        c.begin_tick(t, true, 12_000 - F as u32, cap);
                        c.end_tick(12_000 - F as u32);
                        t += 10_000;
                    }
                    let p = c.begin_tick(t, true, avail, cap);

                    // §4 的等式，带饱和：水位不够时可削量必须是 0，而不是负数
                    // 绕回去、也不是「反正读得到就削」。
                    let bound = (avail as usize).saturating_sub(F + p.d_floor);
                    assert!(
                        p.feasible <= bound,
                        "starve={starve} avail={avail}: feasible={} > A−F−D_floor={bound} \
                         (d_floor={}) —— 削完这一 tick 就会欠载",
                        p.feasible,
                        p.d_floor
                    );
                    // 两条实际出口都不许越过它。
                    assert!(p.budget <= p.feasible, "budget 越过了可行性");
                    assert!(p.fast <= p.feasible, "静音快速通道越过了可行性");
                    // `D_floor` 本身不得低于结构性下限。
                    assert!(p.d_floor >= D_FLOOR_MIN, "d_floor 掉到了 {}", p.d_floor);
                    if p.feasible > 0 && p.feasible == bound {
                        binding += 1;
                    }
                }
            }
            // 这条判据必须真的被行使过 —— 全程不 binding 的扫描等于没测。
            assert!(binding >= 8, "可行性从未成为那道闸，扫描没有意义（{binding}）");
        }

        /// 15：**目标水位随实测回撤上调**，且下调受限。
        #[test]
        fn t15_the_target_follows_the_measured_drawdown() {
            let mut sim = Sim::new(loud_pattern(), 30 * MSF, Mode::Active, false);
            for _ in 0..300 {
                sim.tick(F, true);
            }
            assert!(
                sim.ctl.d_target_frames() <= (D_TARGET_COLD + MSF) as f32,
                "安静的机器上目标该待在冷启动值附近"
            );
            // 一次 40 ms 的生产侧漏写。
            for _ in 0..4 {
                sim.tick(0, true);
            }
            for _ in 0..200 {
                sim.tick(F, true); // 追上并跨过一次每秒重算
            }
            let target = sim.ctl.d_target_frames();
            assert!(
                target >= (55 * MSF) as f32,
                "40 ms 回撤应把目标抬到 ≥55 ms, got {:.1} ms",
                target / MSF as f32
            );
            assert!(
                sim.ctl.d_floor_frames() >= 40 * MSF,
                "D_floor 必须跟着实测回撤走"
            );
            // 30 秒后最多降 30 ms（1 ms/s）。
            for _ in 0..3_000 {
                sim.tick(F, true);
            }
            let later = sim.ctl.d_target_frames();
            assert!(
                later >= target - (31 * MSF) as f32,
                "下调超过了 1 ms/s：{:.1} → {:.1} ms",
                target / MSF as f32,
                later / MSF as f32
            );
        }

        /// 16：**整条流**逐窗满足 C2/C3/C4，而不是只在被测的那个点位上做对。
        #[test]
        fn t16_the_whole_stream_passes_every_criterion() {
            let mut sim = Sim::new(loud_pattern(), 400 * MSF, Mode::Active, true);
            for _ in 0..3_000 {
                sim.tick(F, true); // 30 秒
            }
            assert!(sim.trims > 0, "这段里必须真的削过, trims={}", sim.trims);
            let out = sim.out.take().unwrap();
            let reference = sine(1000.0, out.len(), 0.5);
            let ratio = max_slope(&out) / max_slope(&reference);
            assert!(ratio <= 1.10, "C2：整条流的最大斜率涨了 {ratio:.3}×");
            let d = c3_worst_db(&out, 2 * MSF, MSF / 2);
            assert!(d <= 1.5, "C3：整条流出现了 {d:.2} dB 的电平不连续");
            let snr = c4_min_snr_db(&out, 1000.0);
            assert!(snr >= 40.0, "C4：整条流最差一窗只有 {snr:.1} dB");
        }

        /// 17：**金标数值**。回归会表现为数值漂移而不是突然失败，便于判断
        /// 「变差了多少」。
        #[test]
        fn t17_golden_numbers() {
            let input = sine(1000.0, F * 6, 0.8);
            let reference = max_slope(&input);

            // (a) 病态 τ（四分之一周期，相关度 0）+ 交叉淡化。搜索永远不会选这个
            //     点位，放在这里是因为它同时钉住等功率律（C3 好）和「为什么必须
            //     搜索」（C4 差得离谱 —— 4 ms 里转 90° 相当于 62.5 Hz 的频偏）。
            let (soft, tau, ncc) = stream_one_splice(&input, X, 1, 0, Some(492), None);
            assert_eq!(tau, 492);
            assert!(ncc.abs() < 0.02, "sin/cos 正交, got {ncc:.6}");
            let c2 = max_slope(&soft) / reference;
            let c3 = c3_worst_db(&soft, 2 * MSF, MSF / 2);
            let c4 = c4_min_snr_db(&soft, 1000.0);
            assert!((1.03..1.10).contains(&c2), "金标漂移 c2={c2:.4}");
            assert!((0.15..0.40).contains(&c3), "金标漂移 c3={c3:.4} dB");
            assert!((2.0..6.0).contains(&c4), "金标漂移 c4={c4:.2} dB");

            // (b) 同一个 τ、去掉淡化 = 硬切。C2 与 C4 一起变红，C3 纹丝不动
            //     —— 硬切是**相位**问题不是**电平**问题，三条判据各管一段，
            //     这一行就是它们不可互相替代的证据。
            let (hard, _, _) = stream_one_splice(&input, 0, 1, 0, Some(492), None);
            let c2h = max_slope(&hard) / reference;
            let c3h = c3_worst_db(&hard, 2 * MSF, MSF / 2);
            let c4h = c4_min_snr_db(&hard, 1000.0);
            assert!((8.0..9.5).contains(&c2h), "金标漂移 c2_hard={c2h:.4}");
            assert!(c3h < 0.05, "金标漂移 c3_hard={c3h:.4} dB");
            assert!(c4h < 1.0, "金标漂移 c4_hard={c4h:.2} dB");

            // (c) 搜索命中整周期：残差应当落回 f32 底噪。
            let (good, tau2, _) = stream_one_splice(&input, X, 1, T_MAX_CORR, None, None);
            assert_eq!(tau2, 480);
            let c4g = c4_min_snr_db(&good, 1000.0);
            // 75 dB 而不是 90+：Goertzel 单 bin 对 f32 量化底噪的分离度就到这里，
            // 与 trim 无关（未拼接的参考流量出来是同一个数）。
            let c4ref = c4_min_snr_db(&input, 1000.0);
            assert!(c4g > 70.0, "金标漂移 c4_search={c4g:.2} dB");
            assert!(
                (c4g - c4ref).abs() < 1.0,
                "命中整周期的拼接必须与未拼接的参考流同底噪: {c4g:.2} vs {c4ref:.2} dB"
            );
        }

        // ============================================ 模式与令牌桶的边界

        /// `off` 档一个 tick 都不削，但**观测照做** —— 目标水位和回撤仍然更新，
        /// 所以出问题时那些数字还在。
        #[test]
        fn t18_off_mode_observes_but_never_trims() {
            let mut sim = Sim::new(loud_pattern(), 400 * MSF, Mode::Off, false);
            for _ in 0..3_000 {
                sim.tick(F, true);
            }
            assert_eq!(sim.trims, 0, "off 档削了 {} 次", sim.trims);
            assert!(sim.level() >= 400 * MSF - F, "水位应当纹丝不动");
            assert!(sim.ctl.d_target_frames() > 0.0, "但目标水位仍在算");
        }

        // ================================================ DLL 落地之后的降级

        /// **默认档是 `safety_net`，不是 `active`。**
        ///
        /// DLL（[`super::super::dll`]）成为水位常规执行器之后，`active` 的触发线
        /// （`D_target + 10 ms`）正好压在 DLL 的收敛点上，两个控制器会抢同一个
        /// 被控量：稳态附近的正常抖动就反复越过迟滞 ⇒ trim 每次开火都给环路注
        /// 一个阶跃。`safety_net` 的 100 ms 上沿恰好是 DLL 够不着的那一段
        /// （500 ppm ⇒ 30 ms/分钟，100 ms 以内三分钟排完）。
        ///
        /// 环境变量测试要串行跑（`set_var` 是进程级的），所以三个档在同一条
        /// 测试里按顺序验完。
        #[test]
        fn t24_the_default_mode_is_the_safety_net_now_that_the_dll_exists() {
            // SAFETY: 单条测试内串行读写自己的变量，测完恢复。整个 `trim::tests`
            // 里只有这一条碰环境。
            unsafe {
                std::env::remove_var("AUDIOHUB_TRIM_MODE");
                assert_eq!(
                    Mode::from_env(),
                    Mode::SafetyNet,
                    "默认档回到了 `active` —— trim 会和 DLL 抢同一个水位"
                );
                // 三个档都还能显式选到：降级不是删除，重同步这条路必须留着。
                for (v, want) in [
                    ("active", Mode::Active),
                    ("1", Mode::Active),
                    ("safety_net", Mode::SafetyNet),
                    ("off", Mode::Off),
                    ("0", Mode::Off),
                ] {
                    std::env::set_var("AUDIOHUB_TRIM_MODE", v);
                    assert_eq!(Mode::from_env(), want, "AUDIOHUB_TRIM_MODE={v}");
                }
                std::env::remove_var("AUDIOHUB_TRIM_MODE");
            }
            // `safety_net` 的射程必须真的落在 DLL 的射程之外，否则这次降级白做。
            assert!(
                W_HIGH_SAFETY >= 100 * MSF,
                "safety_net 的上沿掉到 100 ms 以下 —— 又和 DLL 抢上了"
            );
        }

        /// **全仓库只有一个自适应水位**：DLL 用的目标不受 `mode` 覆盖。
        ///
        /// `d_target_frames()` 在 `safety_net` 档被顶成固定 60 ms，那是**重同步的
        /// 触发线**；DLL 是常规执行器，它该收敛到实测的 `MaxDrawdown` 边界
        /// （实测 ≈44 ms）。让 DLL 去追 60 ms 等于凭空多留十几毫秒延迟。
        #[test]
        fn t25_the_dll_target_is_the_adaptive_one_in_every_mode() {
            for mode in [Mode::Off, Mode::SafetyNet, Mode::Active] {
                let mut sim = Sim::new(loud_pattern(), 60 * MSF, mode, false);
                for _ in 0..3_000 {
                    sim.tick(F, true);
                }
                let dll_t = sim.ctl.dll_target_frames();
                assert!(
                    dll_t > 0.0 && dll_t < D_TARGET_SAFETY as f32,
                    "{mode:?} 档的 DLL 目标是 {dll_t} 帧 —— 生产者严格规律 ⇒ 回撤 0 \
                     ⇒ 自适应目标应当压到下限附近，而不是 safety_net 的固定 60 ms"
                );
                if mode == Mode::SafetyNet {
                    assert_eq!(
                        sim.ctl.d_target_frames(),
                        D_TARGET_SAFETY as f32,
                        "safety_net 的 trim 触发线不该被 DLL 目标带跑"
                    );
                    assert_ne!(
                        sim.ctl.d_target_frames(),
                        dll_t,
                        "两个目标合成了一个 —— 要么 DLL 多留延迟，要么 trim 抢水位"
                    );
                }
            }
        }

        /// 静音快速通道：整段是静音时几个 tick 内收敛，且**不扣令牌**。
        #[test]
        fn t19_a_silent_backlog_collapses_in_a_few_ticks() {
            let mut sim = Sim::new(vec![0.0f32; 1], 400 * MSF, Mode::Active, false);
            for _ in 0..40 {
                sim.tick(F, true);
            }
            let ms = sim.level() as f32 / MSF as f32;
            assert!(
                ms <= 60.0,
                "纯静音的 400 ms 积压 400 ms 之内没排掉：还剩 {ms:.1} ms"
            );
            assert_eq!(sim.short_frames, 0, "快速通道削过头了");
        }
    }
}

pub(crate) mod dll {
    //! `tx_loop` 唤醒周期的二阶延迟锁定环（DLL）——**水位的常规执行器**。
    //!
    //! ## 它取代了什么
    //!
    //! 开环版是 `deadline = start + tick × 10 ms`。那条式子把**每一次相位扰动
    //! 永久积分**：一次 108 ms 的抢占之后，环里那 108 ms 音频既没被读走也没被
    //! 丢掉，而生产者与消费者锚在同一个 `mach_absolute_time` 上、长期速率误差为
    //! 零 ⇒ **没有任何机制会把它排出去**。实测 9 小时积到 434 ms。
    //!
    //! 闭环版是 `next_time += 10 ms / corr`，`corr = update(err)`。存量由环路
    //! 自己以受控速度吐掉：**不丢一个样本、不改一个音高、不需要交叉淡化**。
    //!
    //! ## 先例
    //!
    //! PipeWire `spa/plugins/alsa/alsa-pcm.c` 的 **driver + tsched** 路径
    //! （`state->following == false`）：`setup_matching()` 置 `matching = false`
    //! ⇒ 重采样器完全不参与、`rate_match->rate` 硬置 1.0，**但 DLL 照跑**，
    //! `corr` 唯一的去处就是 `state->next_time += threshold / corr * 1e9 / rate`
    //! （3110 行，**无条件执行、不在任何 `if` 里**）。注释逐字为
    //! （3089–3092 行）：
    //!
    //! > `Only set rate_match rate when matching is active. When not matching,`
    //! > `set it to 1.0 to indicate no rate adjustment needed, even though DLL`
    //! > `may still be running for buffer level management.`
    //!
    //! 这条注释同时推翻了我方此前的推论「同时钟 ⇒ 一次性压下去就够了」：
    //! **同时钟消除的是速率误差，不消除相位误差。**
    //!
    //! 控制律本身抄 `spa/utils/dll.h`（MIT，5 行），只把阻尼系数换成 zita 的
    //! 2.0（见 [`DAMPING`]）。
    //!
    //! ## ⚠ 误差符号：本模块唯一一处写反就直接推向饱和的地方
    //!
    //! 按 [`Dll::update`] 的定义（`z1 += w0(w1·err − z1)`，`w1 > 0`，
    //! 返回 `1 − (z2+z3)`）：
    //!
    //! ```text
    //! err > 0  ⇒  z1,z2 > 0  ⇒  corr < 1  ⇒  10ms/corr 变大  ⇒  周期变长
    //!          ⇒  读得更少  ⇒  水位上涨
    //! ```
    //!
    //! 所以 **`err > 0` 的物理含义是「请让水位涨」**。我们的 `tx_loop` 是 HAL
    //! 环的**消费者**（capture 语义），要的是「水位高就降下来」，于是必须写成
    //!
    //! ```text
    //! err = D_target − 水位
    //! ```
    //!
    //! 写成 `水位 − D_target` 是**正反馈**：水位高 ⇒ err>0 ⇒ 周期变长 ⇒ 读得更少
    //! ⇒ 水位更高 ⇒ 直到撞环容量。PipeWire 自己就是按生产/消费分开取符号的
    //! （`alsa-pcm.c:3032–3035`）：playback（往设备**写**）`err = delay − target`；
    //! capture（从设备**读**）`err = target − delay` ← 我们属于这一支。
    //!
    //! 这条推导配了一条会在符号写反时变红的闭环仿真测试
    //! （`tests::inverting_the_error_sign_diverges`）。

    use std::sync::atomic::{AtomicU64, Ordering};

    /// 阻尼系数 `k`。等效二阶环路 `b = k·w`、`c = w²` ⇒ **ζ = k/2**。
    ///
    /// 三个独立实现：Adriaensen 论文 √2（ζ=0.707）、PipeWire `spa_dll` 1.5
    /// （ζ=0.75）、zita-ajbridge/njbridge 2.0（ζ=1.0）。唯一硬约束是
    /// **ζ ∈ [0.7, 1.0]**：低于 0.7 过冲振铃，高于 1 收敛慢但绝对安全。
    ///
    /// 取 **2.0（临界阻尼，无过冲）**：被控量是「离欠载边界还有多远」，
    /// 过冲的代价是**真实的可闻断续**，收敛慢 33 % 的代价只是延迟多留一会儿。
    /// 风险不对称 ⇒ 取安全的那一端。
    pub const DAMPING: f64 = 2.0;

    /// 捕获带宽（Hz）。ζ=1.0 下 τ = 1/(2πB) ≈ 0.32 s，2 % 稳定时间 ≈ 1.3 s。
    pub const BW_CAPTURE: f64 = 0.5;
    /// 跟踪带宽（Hz）。τ ≈ 3.2 s，2 % 稳定时间 ≈ 12.7 s。
    ///
    /// 抄 zita 的两段式（`setloop(0.5)` → 4 s 后 `setloop(0.05)`）而不是
    /// PipeWire 的自适应带宽：行为可推理，没有第二个状态机。
    pub const BW_TRACK: f64 = 0.05;
    /// 捕获段的长度（tick）。4 s @ 10 ms。
    pub const CAPTURE_TICKS: u32 = 400;

    /// 上一次 [`Dll::resync`] 的时刻（[`super::mono_us`]，0 = 从未）。
    ///
    /// 进程级：唤醒周期只有一条环路。欠载的归因行读它，用来回答「这一段欠载
    /// 是不是落在重同步之后那段高增益窗口里」。
    pub(crate) static LAST_RESYNC_US: AtomicU64 = AtomicU64::new(0);

    /// `corr` 的对称限幅：**±500 ppm**。
    ///
    /// 依据（四条，注意我们的执行器是**唤醒相位**不是重采样器，所以「音高可闻性」
    /// 那一路依据在这里并不适用）：
    ///
    /// 1. **下游吸收能力**（真正的约束）。`corr` 同时决定从 HAL 环读走的速率
    ///    **和 UDP 发包的速率**，`corr ≠ 1` 期间对端抖动缓冲以
    ///    `500 ppm × 48 kHz = 24 帧/秒 = 0.5 ms/s` 的速度涨/落。对端 JB 是
    ///    70–80 ms，即**两分钟以上**的余量才需要它自己的深度控制动一下手——
    ///    对端看到的是一个可被常规策略吸收的缓慢偏置，不是一次突发。
    /// 2. **发包节奏**。每个 10 ms 包的间隔偏差 ≤ **5 µs**，比 JB 深度低三个
    ///    数量级，对端的到达抖动统计不受影响。
    /// 3. **先例**。Snapcast 的软同步硬上限正是 500 ppm
    ///    （`client/stream.cpp:416`，`rate = 1.0 − min(rate, 0.0005)`）。执行器
    ///    不同，但角色相同：**超过此值就该走重同步，不该靠速率弯曲。**
    /// 4. **它划出了 DLL 与 trim 的分工线**。500 ppm ⇒ 水位以 30 ms/分钟收敛：
    ///    一次 100 ms 的注入约 3.3 分钟排完，434 ms 的存量约 14.5 分钟。**再大
    ///    就不是 DLL 该管的**——那正是 trim 的 `safety_net` 档（100 ms 上沿）
    ///    与治法 A（>100 ms 跳 tick）的射程。
    ///
    /// 注意这个限幅在**常态下是饱和的**：B=0.05 时比例项 `w1 = 1.31e-5 /帧`，
    /// 误差超过约 38 帧（0.8 ms）就把 500 ppm 吃满。所以大误差下环路表现为
    /// **压摆率限制的斜坡**，只有最后 0.8 ms 才回到线性二阶动力学。这是有意的：
    /// 斜坡段的速度由第 1 条（下游吸收能力）定，不由环路增益定。
    pub const CORR_CLAMP: f64 = 500e-6;

    /// 喂进环路的误差绝对值上限（帧）。取一整环（500 ms）。
    ///
    /// 这**不是**稳定性措施——稳定性由 [`CORR_CLAMP`] 与 `z3` 的抗饱和钳位保证；
    /// 它只是一道「读数荒谬就别往里灌」的护栏（环里不可能存在超过一环的积压，
    /// 出现即读数损坏）。
    pub const ERR_CLAMP_FRAMES: f64 = super::HAL_RING_FRAMES as f64;

    /// 二阶延迟锁定环。移植自 `spa/utils/dll.h`（MIT）。
    ///
    /// 契约：输入 `err` 单位是**帧**，输出是**围绕 1.0 的相对速率修正因子**。
    #[derive(Debug, Clone)]
    pub struct Dll {
        period: f64,
        rate: f64,
        bw: f64,
        w0: f64,
        w1: f64,
        w2: f64,
        z1: f64,
        z2: f64,
        z3: f64,
        corr: f64,
        /// 还剩几个 tick 走捕获带宽。0 = 已切到跟踪带宽。
        capture_left: u32,
        updates: u64,
        clamped: u64,
        resyncs: u64,
    }

    impl Dll {
        /// `period` = 一个 tick 读走多少帧（480），`rate` = 采样率。
        pub fn new(period: f64, rate: f64) -> Dll {
            let mut d = Dll {
                period,
                rate,
                bw: 0.0,
                w0: 0.0,
                w1: 0.0,
                w2: 0.0,
                z1: 0.0,
                z2: 0.0,
                z3: 0.0,
                corr: 1.0,
                capture_left: CAPTURE_TICKS,
                updates: 0,
                clamped: 0,
                resyncs: 0,
            };
            d.set_bw(BW_CAPTURE);
            d
        }

        /// `spa_dll_set_bw`。`w0` 是转角在 **20 × 环路带宽**的前置一阶平滑器：
        /// 只滤测量噪声，不动环路动力学。驱动按 512 帧写块 ⇒ 水位读数带一条
        /// ±512 帧的锯齿（≈94 Hz，远高于环路带宽），不先滤一下会直接灌进积分器。
        pub fn set_bw(&mut self, bw: f64) {
            let w = 2.0 * std::f64::consts::PI * bw * self.period / self.rate;
            self.w0 = 1.0 - (-20.0 * w).exp();
            self.w1 = w * DAMPING / self.period;
            self.w2 = w / DAMPING;
            self.bw = bw;
        }

        /// `spa_dll_init` + 回到捕获带宽。
        ///
        /// **跳 tick / 驱动重附着 / 索引回绕之后必须调**：那一刻水位发生的是
        /// 阶跃，而 `z3` 里存的是阶跃**之前**那段历史的积分。不清掉，它会在
        /// 跳变之后继续输出为旧误差算出的修正 ⇒ 过冲 ⇒ 欠载。
        /// PipeWire 在 `node-driver.c:487–494` 做同一件事：
        /// `> max_resync` ⇒ 强制 `BW_MAX` + `err = 0`；
        /// `> 2×max_resync` ⇒ 再叠加 `spa_dll_init()` 整环重置。
        pub fn resync(&mut self) {
            self.z1 = 0.0;
            self.z2 = 0.0;
            self.z3 = 0.0;
            self.corr = 1.0;
            self.capture_left = CAPTURE_TICKS;
            self.resyncs += 1;
            self.set_bw(BW_CAPTURE);
            // 归因埋点。重同步是环路唯一一次「丢掉全部历史、回到捕获带宽」的
            // 动作，紧随其后是环路增益最高、最可能过冲的一段。欠载若总落在这
            // 一段里，嫌疑就从生产侧转到我们自己身上。进程级而非按槽：唤醒周
            // 期只有一条，重同步作用于整条 `tx_loop`。
            LAST_RESYNC_US.store(super::mono_us(), Ordering::Relaxed);
        }

        /// 喂一次误差，返回**已限幅**的 `corr`。
        ///
        /// `err` **必须**是 `D_target − 水位`（消费者语义，见模块文档）。
        pub fn update(&mut self, err_frames: f64) -> f64 {
            let err = if err_frames.is_finite() {
                err_frames.clamp(-ERR_CLAMP_FRAMES, ERR_CLAMP_FRAMES)
            } else {
                0.0
            };
            // ---- spa_dll_update 的 5 行，一字不改 ----
            self.z1 += self.w0 * (self.w1 * err - self.z1);
            self.z2 += self.w0 * (self.z1 - self.z2);
            self.z3 += self.w2 * self.z2;
            // ---- 抗积分饱和：z3 是**唯一**的积分器（z1/z2 是有泄漏的一阶节，
            //      不会绕死）。输出被钳而积分器不被钳 = 经典 windup：误差反号之
            //      后环路要先把 z3 卸完才反应得过来。把它钳在同一个范围里，
            //      windup 在结构上不存在。
            self.z3 = self.z3.clamp(-CORR_CLAMP, CORR_CLAMP);
            let raw = 1.0 - (self.z2 + self.z3);
            let corr = raw.clamp(1.0 - CORR_CLAMP, 1.0 + CORR_CLAMP);
            if corr != raw {
                self.clamped += 1;
            }
            self.corr = corr;
            self.updates += 1;
            if self.capture_left > 0 {
                self.capture_left -= 1;
                if self.capture_left == 0 {
                    self.set_bw(BW_TRACK);
                }
            }
            corr
        }

        /// 最近一次的 `corr`。没有有效观测的 tick（追平期、驱动未附着）用它
        /// **保持**上一次的命令，而不是回落到 1.0——回落等于每次观测中断都给
        /// 环路注一次阶跃。
        pub fn corr(&self) -> f64 {
            self.corr
        }

        pub fn bw(&self) -> f64 {
            self.bw
        }

        /// 下一次唤醒相对本次的间隔，纳秒：`period / corr × 1e9 / rate`
        /// （PipeWire `alsa-pcm.c:3110` 的同一式子）。
        pub fn period_nanos(&self) -> u64 {
            let ns = self.period / self.corr * 1e9 / self.rate;
            // corr 已被钳在 [1−5e-4, 1+5e-4]，ns 只可能在 10 ms 的 ±0.05 % 里；
            // 这个夹取纯粹是不让一个 NaN 变成 `Duration::from_nanos(0)` 的死转。
            if ns.is_finite() {
                ns.clamp(1.0, 1e9) as u64
            } else {
                (self.period * 1e9 / self.rate) as u64
            }
        }

        /// 埋点：`(更新次数, corr 被限幅的次数, 重同步次数, 当前 corr 的 ppm 偏移)`。
        pub fn counters(&self) -> DllCounters {
            DllCounters {
                updates: self.updates,
                clamped: self.clamped,
                resyncs: self.resyncs,
                corr_ppm: ((self.corr - 1.0) * 1e6) as f32,
                bw_hz: self.bw as f32,
            }
        }
    }

    /// DLL 的现场读数（IPC / probe 用）。
    #[derive(Debug, Clone, Copy, Default, PartialEq, serde::Serialize)]
    pub struct DllCounters {
        /// 喂进环路的有效观测次数。
        pub updates: u64,
        /// `corr` 撞上 ±500 ppm 限幅的次数。**长期居高 = 存量在被斜坡排空，
        /// 或者符号写反了。**
        pub clamped: u64,
        /// 整环重置次数（跳 tick / 重附着 / 空闲）。
        pub resyncs: u64,
        /// 当前速率修正，ppm。稳态应在 0 附近抖动。
        pub corr_ppm: f32,
        /// 当前环路带宽（Hz）：0.5 = 捕获段，0.05 = 跟踪段。
        pub bw_hz: f32,
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::halbridge::{HAL_FRAME_48K as F, HAL_SAMPLE_RATE as RATE};

        fn dll() -> Dll {
            Dll::new(F as f64, RATE as f64)
        }

        /// 最小闭环被控对象：生产者恒速写、消费者按 DLL 给出的周期读走 `F` 帧。
        ///
        /// 一个 tick 睡 `dt = (F/rate)/corr`，其间生产者写 `rate·dt = F/corr` 帧，
        /// 消费者读 `F` 帧 ⇒ `水位 += F(1/corr − 1)`。
        /// **`corr > 1` ⇒ 水位下降**，这是全模块符号约定的物理落点。
        struct Plant {
            dll: Dll,
            level: f64,
            target: f64,
            /// true = 故意把误差符号写反（用于反向测试）。
            inverted: bool,
        }

        impl Plant {
            fn new(level_ms: f64, target_ms: f64, inverted: bool) -> Plant {
                let msf = (RATE / 1000) as f64;
                Plant {
                    dll: dll(),
                    level: level_ms * msf,
                    target: target_ms * msf,
                    inverted,
                }
            }
            fn step(&mut self) {
                let err = if self.inverted {
                    self.level - self.target // ← 反的
                } else {
                    self.target - self.level // ← 消费者语义，唯一正确的写法
                };
                let corr = self.dll.update(err);
                self.level += F as f64 * (1.0 / corr - 1.0);
            }
            fn run(&mut self, ticks: usize) {
                for _ in 0..ticks {
                    self.step();
                }
            }
            fn level_ms(&self) -> f64 {
                self.level / (RATE / 1000) as f64
            }
        }

        /// **本模块最重要的一条测试**：符号写反 ⇒ 水位发散。
        ///
        /// 两个 Plant 从同一个初始状态出发，只有误差符号不同。正的收敛、反的
        /// 发散——把 [`Plant::step`] 里那两支换过来，第一条断言立刻变红。
        #[test]
        fn inverting_the_error_sign_diverges() {
            let mut ok = Plant::new(200.0, 40.0, false);
            let mut bad = Plant::new(200.0, 40.0, true);
            ok.run(60_000); // 10 分钟
            bad.run(60_000);
            assert!(
                ok.level_ms() < 200.0,
                "正确符号下水位没降：{:.1} ms（起点 200 ms）",
                ok.level_ms()
            );
            assert!(
                bad.level_ms() > 200.0,
                "反符号下水位居然没涨到 200 ms 以上（{:.1} ms）—— 这条测试失去了\
                 它唯一的意义：它必须证明写反会发散",
                bad.level_ms()
            );
            assert!(
                bad.level_ms() > ok.level_ms() + 100.0,
                "两个符号的结局分不开（正 {:.1} ms / 反 {:.1} ms）",
                ok.level_ms(),
                bad.level_ms()
            );
        }

        /// 水位**高于**目标 ⇒ `corr > 1` ⇒ 唤醒周期**变短** ⇒ 读得更快。
        ///
        /// 这是「误差符号 → 执行器方向」这条链上唯一一次真正的方向判定，
        /// 直接用 `period_nanos()`（执行器本身）而不是 `corr` 来断言。
        #[test]
        fn a_level_above_target_shortens_the_period() {
            let nominal = (F as f64 * 1e9 / RATE as f64) as u64; // 10 ms
            let mut d = dll();
            // 目标 40 ms、水位 60 ms ⇒ err = 40−60 = −20 ms = −960 帧。
            let err = -20.0 * (RATE / 1000) as f64;
            let corr = d.update(err);
            assert!(corr > 1.0, "水位高于目标却给出 corr = {corr}（≤1 = 读得更慢）");
            assert!(
                d.period_nanos() < nominal,
                "周期没变短：{} ns（标称 {} ns）",
                d.period_nanos(),
                nominal
            );
            // 反方向同样要成立，否则「方向对」可能只是限幅的巧合。
            let mut d2 = dll();
            let corr2 = d2.update(-err);
            assert!(corr2 < 1.0, "水位低于目标却给出 corr = {corr2}");
            assert!(d2.period_nanos() > nominal, "周期没变长");
        }

        /// 限幅：任意大的误差都不许把周期拉到荒谬值，且积分器不许 windup。
        #[test]
        fn the_rate_clamp_holds_and_the_integrator_cannot_wind_up() {
            let nominal = F as f64 * 1e9 / RATE as f64;
            let mut d = dll();
            // 灌 10 分钟的巨大误差：换成没有 z3 钳位的实现，这一步之后 z3 会
            // 大到需要成千上万个 tick 才卸得完。
            for _ in 0..60_000 {
                let corr = d.update(-1e9); // 会先被 ERR_CLAMP_FRAMES 夹到一整环
                assert!(
                    (0.9..=1.1).contains(&corr),
                    "corr 跑飞了：{corr}"
                );
                assert!((corr - 1.0).abs() <= CORR_CLAMP + 1e-12, "corr 超出 ±500 ppm：{corr}");
            }
            let fast = d.period_nanos() as f64;
            assert!(
                fast >= nominal * (1.0 - CORR_CLAMP - 1e-9) / (1.0 + CORR_CLAMP),
                "周期被拉到了限幅之外"
            );
            assert!(
                d.counters().clamped > 0,
                "限幅一次都没生效 —— 那这条测试什么也没测"
            );
            // 抗饱和的判据：误差反号之后，环路必须在**一个环路时间常数量级**内
            // 把 corr 拉回另一侧，而不是先卸几千个 tick 的积分。
            // B=0.05 ⇒ τ ≈ 3.2 s ≈ 320 tick，给 3τ 的余量。
            let mut back = None;
            for i in 0..1_000 {
                if d.update(1e9) < 1.0 {
                    back = Some(i);
                    break;
                }
            }
            let i = back.expect("误差反号 1000 tick 之后 corr 还在错误的一侧 —— 积分器绕死了");
            assert!(i < 960, "卸积分用了 {i} 个 tick（> 3τ），抗饱和没生效");
        }

        /// `resync` 必须把三个状态量、`corr` 和带宽一起复位。
        ///
        /// 只清 `z3` 不清 `z1/z2` 也能过「归零」这种弱断言，所以这里断的是
        /// **行为**：复位之后喂 0 误差，`corr` 必须恰好是 1.0。
        #[test]
        fn resync_clears_every_state_the_loop_carries() {
            let mut d = dll();
            for _ in 0..CAPTURE_TICKS + 100 {
                d.update(-5_000.0);
            }
            assert!((d.bw() - BW_TRACK).abs() < 1e-12, "前提：已经切到跟踪带宽");
            assert!(d.corr() > 1.0, "前提：环路确实带着一个非零的命令");

            d.resync();
            assert_eq!(d.corr(), 1.0, "resync 之后 corr 不是 1.0");
            assert!((d.bw() - BW_CAPTURE).abs() < 1e-12, "resync 没有回到捕获带宽");
            assert_eq!(
                d.update(0.0),
                1.0,
                "零误差喂进去 corr 却不是 1.0 —— 环里还残留着跳变之前的状态"
            );
            assert_eq!(d.counters().resyncs, 1);
        }

        /// 不 resync 会怎样：跳变之后积分器残留把水位推到目标的另一侧（过冲）。
        ///
        /// 这条是上一条的**收益证明**——没有它，`resync()` 是否被调用就只是
        /// 一个没有后果的仪式。
        #[test]
        fn skipping_resync_after_a_step_overshoots() {
            let msf = (RATE / 1000) as f64;
            // 两个完全相同的环路，先都在「水位 300 ms、目标 40 ms」下跑够久，
            // 把积分器喂到饱和；然后治法 A 把水位一把削到目标。
            let mut with_reset = Plant::new(300.0, 40.0, false);
            let mut without = Plant::new(300.0, 40.0, false);
            with_reset.run(3_000);
            without.run(3_000);
            with_reset.level = 40.0 * msf;
            without.level = 40.0 * msf;
            with_reset.dll.resync();

            with_reset.run(2_000); // 20 s
            without.run(2_000);
            let dev_ok = (with_reset.level_ms() - 40.0).abs();
            let dev_bad = (without.level_ms() - 40.0).abs();
            assert!(
                dev_ok < dev_bad,
                "复位之后反而偏得更远（复位 {dev_ok:.2} ms / 不复位 {dev_bad:.2} ms）"
            );
            assert!(
                dev_ok < 1.0,
                "复位之后 20 s 还偏离目标 {dev_ok:.2} ms"
            );
        }

        /// 稳态：同时钟、无扰动 ⇒ 水位停在目标上，`corr` 回到 1.0 附近。
        ///
        /// 这条同时证明了「二阶环有积分器 ⇒ 稳态误差为零」——一阶比例环会停在
        /// 一个与增益成反比的偏置上。
        #[test]
        fn the_loop_settles_on_the_target_with_no_standing_error() {
            let mut p = Plant::new(120.0, 40.0, false);
            p.run(60_000); // 10 分钟：500 ppm 的斜坡排 80 ms 需要约 2.7 分钟
            assert!(
                (p.level_ms() - 40.0).abs() < 0.5,
                "没收敛到目标：{:.2} ms",
                p.level_ms()
            );
            assert!(
                (p.dll.corr() - 1.0).abs() < 20e-6,
                "稳态还挂着 {:.1} ppm 的速率偏置",
                (p.dll.corr() - 1.0) * 1e6
            );
        }

        /// 收敛速度的量级：500 ppm 的限幅 ⇒ 约 30 ms/分钟。
        ///
        /// 这条把 [`CORR_CLAMP`] 文档里那个数字钉住：它是 DLL 与 trim 分工线的
        /// 依据，写文档里没人会去验，写成测试才不会烂掉。
        #[test]
        fn the_clamp_sets_the_drain_rate_at_about_30ms_per_minute() {
            let mut p = Plant::new(140.0, 40.0, false);
            let before = p.level_ms();
            p.run(6_000); // 60 s
            let drained = before - p.level_ms();
            assert!(
                (25.0..=31.0).contains(&drained),
                "一分钟排掉了 {drained:.1} ms，与 500 ppm 的 30 ms/min 对不上"
            );
        }

        /// 带宽两段式：前 4 s 捕获，之后跟踪。
        #[test]
        fn the_bandwidth_switches_from_capture_to_tracking_after_four_seconds() {
            let mut d = dll();
            assert!((d.bw() - BW_CAPTURE).abs() < 1e-12);
            for _ in 0..CAPTURE_TICKS - 1 {
                d.update(0.0);
            }
            assert!((d.bw() - BW_CAPTURE).abs() < 1e-12, "提前切了");
            d.update(0.0);
            assert!((d.bw() - BW_TRACK).abs() < 1e-12, "4 s 之后没切到跟踪带宽");
            assert_eq!(CAPTURE_TICKS as u64 * 10, 4_000, "捕获段不是 4 秒");
        }

        /// 阻尼比落在硬约束区间里，且是临界阻尼那一端。
        #[test]
        fn the_damping_ratio_is_inside_the_only_hard_constraint() {
            let zeta = DAMPING / 2.0;
            assert!(
                (0.7..=1.0).contains(&zeta),
                "ζ = {zeta} 落在 [0.7, 1.0] 之外：低于 0.7 过冲振铃，高于 1 白白慢"
            );
            assert_eq!(zeta, 1.0, "取的不是临界阻尼 —— 过冲的代价是可闻断续");
        }

        /// 坏输入不许把执行器变成死转或停摆。
        #[test]
        fn non_finite_input_cannot_stall_the_loop() {
            let nominal = (F as f64 * 1e9 / RATE as f64) as u64;
            let mut d = dll();
            for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
                let corr = d.update(bad);
                assert!(corr.is_finite(), "{bad} 喂出了 {corr}");
                let ns = d.period_nanos();
                assert!(ns > 0, "周期变成 0 ⇒ 忙等");
                assert!(ns < 2 * nominal, "周期被拉到 {ns} ns");
            }
        }
    }
}

/// Plugs the speaker direction into the media engine. `SourceSpec` needs a new
/// variant and `build_source` a new arm; both live in engine.rs, which this
/// group does not own.
pub struct HalSpeakerSource {
    bridge: Arc<Shared>,
    slot: u8,
    dbg_peak: f32,
    dbg_frames: u32,
    /// 水位控制器（治法 B）。单所有者，只被 tx 线程碰。
    ctl: trim::Ctl,
    /// 本源自己的单调时基。`tx_loop` 的 `start` 拿不到，也不该拿——控制器只需要
    /// 一个单调的 μs 读数，用哪个原点无所谓。
    t0: Instant,
    /// 上一次看到的 `attach_epoch` / 槽的不连续计数。任一变化都作废观测。
    seen_epoch: u64,
    seen_disc: u64,
    /// 连续短读（欠载）的当前段长，帧。0 = 上一 tick 没短读。
    short_run: u32,
    /// 冷启动预填的截止时刻（`now_us` 时基）。`None` = 已经在正常消费。
    ///
    /// `Some(t)` 期间每 tick 发整帧静音、不动 `read_idx`，直到环攒够
    /// `D_TARGET_COLD` 或 `now_us >= t`。见 `tick()` 里那段说明。
    prime_until_us: Option<u64>,
    /// 欠载日志的令牌桶：`(窗口起点 μs, 本窗口已打印的段数)`。
    ///
    /// 存在的理由：`logln` 是一次**阻塞 write 系统调用**，而 `note_short` 跑在
    /// 10 ms 音频节拍上。欠载稀疏时（现场是 21 小时 30 段）日志是纯收益；但
    /// 「稀疏」正是这套埋点要去证实的假设，不能拿它当前提。环深度贴着 0 抖动时
    /// 会出现「短读一 tick、好一 tick」的交替 ⇒ 每 20 ms 两行 ⇒ 日志本身变成
    /// 欠载的成因。所以设上限，并在恢复打印时报出被压掉了多少段。
    log_window_us: u64,
    log_in_window: u32,
    log_suppressed: u32,
    /// 预分配的 peek 暂存：交织立体声 + 下混单声道。**10 ms 节拍上零分配**。
    /// 只在 `mode != off` 时分配。
    peek_st: Vec<f32>,
    peek_mono: Vec<f32>,
}

impl HalSpeakerSource {
    /// peek 暂存的帧数上界：一帧 + 静音快速通道的最长一次 peek。
    const SCRATCH_FRAMES: usize = HAL_FRAME_48K + trim::PEEK_MAX; // 12 480 帧

    /// 欠载日志的限流窗口（μs）与窗口内上限（段）。
    ///
    /// 10 段/10 s 远高于现场速率（21 小时 30 段 ≈ 0.004 段/秒），所以正常情况下
    /// 限流永不生效；它挡的是「环深度贴着 0 抖动」那种病理，在那种情况下日志
    /// 会以 50 段/秒的速度往 10 ms 音频线程上压阻塞 write 系统调用。
    const LOG_WINDOW_US: u64 = 10_000_000;
    const LOG_PER_WINDOW: u32 = 10;

    /// 冷启动预填的最长等待（μs）。
    ///
    /// 兜底而非目标：环正常填到 `D_TARGET_COLD`（30 ms）只需要 30 ms，用不到
    /// 这个数。它防的是「驱动附着了但 IO 还没起来」——那种情况下环永远填不满，
    /// 无限等下去就是这条流永远不出声。200 ms 给足了 IO 启动的余量，又短到
    /// 用户不会把它当成故障。
    const PRIME_TIMEOUT_US: u64 = 200_000;

    /// One source per SLOT. The tx engine dedups sources by `SourceSpec`, and
    /// `HalSpeaker { slot }` makes two slots two distinct keys — which is what
    /// keeps each speaker ring to exactly one consumer and the SPSC contract
    /// literally true (spec-m5b §5.4).
    pub fn new(bridge: &HalBridge, slot: u8) -> HalSpeakerSource {
        HalSpeakerSource::with_mode(bridge.shared.clone(), slot, trim::Mode::from_env())
    }

    fn with_mode(bridge: Arc<Shared>, slot: u8, mode: trim::Mode) -> HalSpeakerSource {
        let seen_epoch = bridge.attach_epoch.load(Ordering::Acquire);
        let seen_disc = bridge
            .slots
            .get(slot as usize)
            .map(|s| s.disc_epoch.load(Ordering::Relaxed))
            .unwrap_or(0);
        // 一次性预分配，之后 10 ms 节拍上零分配。mode=off 不分配这 154 KB。
        let n = if mode == trim::Mode::Off { 0 } else { Self::SCRATCH_FRAMES };
        HalSpeakerSource {
            bridge,
            slot,
            dbg_peak: 0.0,
            dbg_frames: 0,
            ctl: trim::Ctl::new(mode),
            t0: Instant::now(),
            seen_epoch,
            seen_disc,
            short_run: 0,
            // 源一建出来就先预填一次：`build_source` 那里刚把积压削到
            // `D_TARGET_COLD`，但环里本来不足那么多时它一帧都丢不掉。
            prime_until_us: Some(Self::PRIME_TIMEOUT_US),
            log_window_us: 0,
            log_in_window: 0,
            log_suppressed: 0,
            peek_st: vec![0.0; n * HAL_SPK_CHANNELS as usize],
            peek_mono: vec![0.0; n],
        }
    }

    /// 一个 tick 的全部工作。`now_us` 由调用方给，测试因此可以跑虚拟时间。
    fn tick(&mut self, now_us: u64, out: &mut Vec<f32>) {
        let slot = self.slot as usize;

        // ---- 不连续检测：驱动重新附着 / 治法 A 排空 / 代次 flush ----
        let epoch = self.bridge.attach_epoch.load(Ordering::Acquire);
        let disc = self
            .bridge
            .slots
            .get(slot)
            .map(|s| s.disc_epoch.load(Ordering::Relaxed))
            .unwrap_or(0);
        let mut discontinuous = false;
        if epoch != self.seen_epoch || disc != self.seen_disc {
            // 驱动**重新附着**：环是新映射的一段共享内存，里面什么都没有。
            // 这与治法 A 的排空（只动 `disc_epoch`）性质不同——那一种排完仍留着
            // 工作储备，接着读是对的；这一种从 0 开始读必然连续短读。
            if epoch != self.seen_epoch {
                self.prime_until_us = Some(now_us + Self::PRIME_TIMEOUT_US);
            }
            self.seen_epoch = epoch;
            self.seen_disc = disc;
            self.ctl.on_discontinuity();
            discontinuous = true;
        }
        // 代次 flush 必须在读 `A_n` **之前**吃掉：不然那个读数描述的是一段马上
        // 就要被整体丢掉的音频，喂进控制器就是一条纯噪声。
        if self.bridge.take_flush(self.slot) {
            self.bridge.rings.flush_spk_consumer(slot);
            self.ctl.on_discontinuity();
            discontinuous = true;
            // flush 把环清空了，和重新附着同一个处境。
            self.prime_until_us = Some(now_us + Self::PRIME_TIMEOUT_US);
        }

        let Some((avail, cap)) = self.bridge.rings.spk_readable(slot) else {
            // 没有驱动附着 ⇒ 这一级根本不存在。整帧静音，且**不算欠载**：
            // 把「没有环」计进短读会把那个计数器唯一的诊断价值埋掉。
            //
            // 但**必须先把正在进行的那一段欠载收尾**。此前这里直接 `return`，
            // 于是驱动在一段欠载中途脱离时 `short_run` 不会归零：重新附着后再
            // 短读，`run_before != 0` ⇒ 不计新事件、长度继续往上累加。结果是
            // `underrun.events` 少计，`worst_run_frames` 变成一个**跨越脱离
            // 期**的合成数——它描述的时长里有一半根本没有环存在。
            self.note_short(0, 0, now_us);
            self.bridge.append_spk_frame(self.slot, out);
            return;
        };

        // ---- 冷启动预填（`engine.rs` 开流处那段注释的另一半） -----------------
        //
        // 开流那里把积压**削到** `D_TARGET_COLD`，并写明了理由：「排到 0 的代价
        // 是真实的：此后每一个 `W_n < F` 的 tick 都要短读补静音，水位靠我们自己
        // 的短读慢慢爬回写块抖动之上——那段爬升期是听得见的细碎断续。」
        //
        // 但削减只能**封顶**，不能**兜底**。环里本来就不足 30 ms 时（驱动刚重新
        // 附着、代次 flush 之后，环是全新的、空的），那段代码 `want == 0`、一帧
        // 都不丢，日志照打「留下 30ms 作为起始水位」，而实际起始水位是 **0**。
        // 于是它警告过的那个后果原样发生。
        //
        // 现场实测（2026-08-03，21.7 h 的 daemon，10 Hz 采样）：驱动重新附着后
        // 环深度从 0 起步，靠 DLL 以 500 ppm 上限爬升，**十几秒后仍只有 800 帧
        // (17 ms)**，其间连续短读。同一窗口 `skip.tx` 一次没动——这批欠载既不是
        // 排空过头也不是 coreaudiod 卡顿，就是这里。
        //
        // 治法与接收侧的 `jb_prebuffering` 同一条：**先等够再开始消费**。等待期
        // 发整帧静音、不动 `read_idx`、不计欠载（这不是「该有数据却没有」，是我们
        // 自己选择还不读），也不喂 DLL（水位还没进入工作区，那不是有效观测）。
        //
        // 代价 ≤ 30 ms 的开流静音——而那 30 ms 无论如何都是静音，区别只在于它是
        // 干净的一段，还是几十秒的细碎断续外加 DLL 的长爬升。超时兜底防的是
        // 「驱动附着了但 IO 还没起来」：那种情况下等下去就是永远不出声。
        if let Some(deadline) = self.prime_until_us {
            if (avail as usize) < trim::D_TARGET_COLD && now_us < deadline {
                self.note_short(0, avail, now_us);
                out.resize(out.len() + HAL_FRAME_48K, 0.0);
                return;
            }
            self.prime_until_us = None;
        }

        let punctual = self.bridge.tick_punctual.load(Ordering::Relaxed);
        let plan = self.ctl.begin_tick(now_us, punctual, avail, cap);
        let mut tau = 0usize;
        if plan.wants_trim() {
            tau = self.try_trim(&plan, avail, out);
            if tau == 0 {
                if let Some(s) = self.bridge.slots.get(slot) {
                    s.trim_deferred.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        if tau == 0 {
            let got = self.bridge.append_spk_frame(self.slot, out);
            self.note_short((HAL_FRAME_48K - got) as u32, avail, now_us);
        } else {
            self.note_short(0, avail, now_us);
        }
        let consumed = (HAL_FRAME_48K + tau) as u32;
        let d_after = avail.saturating_sub(consumed);
        self.ctl.end_tick(d_after);
        self.publish();
        self.publish_phase_error(d_after, punctual && !discontinuous);
    }

    /// 把这一 tick 的 DLL 相位误差交给 `tx_loop`（[`super::dll`] 的输入）。
    ///
    /// **相位**：用 `d_after`（读后残量），与 `Ctl::d_target` 的定义同相
    /// （`end_tick` 拿的正是这个数去比目标）。用读前的 `avail` 会恒定多出一整帧，
    /// 而且那一帧看起来完全像一个真实缓冲。
    ///
    /// **符号**：`err = D_target − d_after`，消费者语义。写成 `d_after − D_target`
    /// 是正反馈，见 [`super::dll`] 的模块文档。
    ///
    /// `valid == false`（追平期 / 刚发生不连续）时只更新数值不推进代次，于是
    /// `tx_loop` 看不到新鲜观测、环路保持上一次的命令。
    fn publish_phase_error(&self, d_after: u32, valid: bool) {
        let Some(s) = self.bridge.slots.get(self.slot as usize) else {
            return;
        };
        let err = self.ctl.dll_target_frames() - d_after as f32;
        s.dll_err_frames.store(err.to_bits(), Ordering::Relaxed);
        if valid {
            // Release：`tx_loop` 用 Acquire 读代次，读到新代次就必须读到与它配对
            // 的那个误差值，而不是上一 tick 的。
            s.dll_epoch.fetch_add(1, Ordering::Release);
        }
    }

    /// 试着削一次。返回真正削掉的 `τ`（0 = 本 tick 不削，调用方走普通读路径）。
    ///
    /// 顺序是「先量后削」：先 peek（**不动 `read_idx`**）、下混、量峰值、定档、
    /// 搜相位，全部通过了才 splice + advance。任何一步不满足就原样退出，环没被
    /// 动过一个下标。
    fn try_trim(&mut self, plan: &trim::Plan, avail: u32, out: &mut Vec<f32>) -> usize {
        let f = HAL_FRAME_48K;
        if self.peek_mono.is_empty() {
            return 0;
        }
        // 两段式 peek：绝大多数 tick 只需要第一段（≈1 344 帧 = 5.4 KB 立体声）。
        // 只有第一段确认为纯静音时才去取更长的那一段——静音档的可削量可达
        // 250 ms，为它每 tick 搬 100 KB 是纯浪费。
        let base_len = trim::peek_base(plan);
        let Some((got, base)) = self.peek_mono_frames(base_len, avail) else {
            return 0;
        };
        let ext_len = trim::peek_ext(plan);
        let (got, base) = if ext_len > base_len
            && trim::silent_span(&self.peek_mono[..got], f, trim::X, trim::GATE_SILENT)
                >= got - f
        {
            match self.peek_mono_frames(ext_len, avail) {
                Some(v) => v,
                None => (got, base),
            }
        } else {
            (got, base)
        };

        let Some(d) = trim::decide(&self.peek_mono[..got], plan) else {
            return 0;
        };
        // F 档的软条件：相关度太低就再等 200 ms 找个好点位。不改变 10 s 死线。
        if d.forced && d.ncc < trim::NCC_MIN_F && self.ctl.retry_ncc() {
            return 0;
        }
        trim::splice(&self.peek_mono[..got], f, trim::X, d.tau, d.ncc, out);
        debug_assert_eq!(out.len(), f, "不变量 I1：输出恒为一帧");
        self.bridge
            .rings
            .advance_spk(self.slot as usize, base, f + d.tau);
        self.ctl.on_trim(d.tau, d.charge);
        if let Some(s) = self.bridge.slots.get(self.slot as usize) {
            // 从环里真正取走的是 f+τ 帧 —— 这个计数器对账的是驱动写了多少，
            // 不是我们发了多少，所以记取走量。
            s.spk_frames.fetch_add((f + d.tau) as u64, Ordering::Relaxed);
            s.trim_events.fetch_add(1, Ordering::Relaxed);
            s.trim_frames.fetch_add(d.tau as u64, Ordering::Relaxed);
            if d.forced {
                s.trim_forced.fetch_add(1, Ordering::Relaxed);
            }
            // 归因埋点：欠载那一行要能答「我前面刚被削过一刀吗」。
            s.last_trim_us.store(mono_us(), Ordering::Relaxed);
            s.last_trim_frames.store(d.tau as u32, Ordering::Relaxed);
        }
        d.tau
    }

    /// peek `want` 帧、下混进 `self.peek_mono`，返回 `(帧数, 读基准)`。
    /// `None` = 没有环 / 不够一帧加最小削减量。
    fn peek_mono_frames(&mut self, want: usize, avail: u32) -> Option<(usize, u64)> {
        let ch = HAL_SPK_CHANNELS as usize;
        let n = want.min(avail as usize).min(self.peek_mono.len());
        if n < HAL_FRAME_48K + trim::T_MIN {
            return None;
        }
        let (got, base) =
            self.bridge
                .rings
                .peek_spk(self.slot as usize, &mut self.peek_st[..n * ch], n)?;
        if got < HAL_FRAME_48K + trim::T_MIN {
            return None;
        }
        // 与 `read_spk_chunk` 同一条下混：分析必须发生在**我们真正要发出去的
        // 那个信号**上，不是发生在左声道上。
        for i in 0..got {
            self.peek_mono[i] = (self.peek_st[i * ch] + self.peek_st[i * ch + 1]) * 0.5;
        }
        Some((got, base))
    }

    /// 短读（欠载 ⇒ 补静音）记账。`short == 0` 结束当前连续段。
    ///
    /// 这是「trim 有没有削过头」的唯一直接证据：`append_spk_frame` 一直返回真实
    /// 样本数，此前这个返回值被丢掉了，补进去的静音原样发给对端而没有任何计数器
    /// 知道。
    ///
    /// ## 为什么这里要记日志（而不只是计数）
    ///
    /// 计数器只能答「发生过几次」，答不了**性质**。同一个 `underrun.events++`
    /// 有两种成因，处理方式正好相反：
    ///
    /// - **生产侧卡顿**（coreaudiod 的 IOProc 也被拖住）⇒ 环里本来就没数据，
    ///   补静音是正确行为，不是缺陷，不该改任何参数；
    /// - **排空过头**（治法 A 的储备不够 / trim 削狠了 / DLL 在限幅上排太久）
    ///   ⇒ 数据是被我们自己削掉的，是缺陷。
    ///
    /// 区分它们只需要一件东西：**时刻**。孤立发生 ⇒ 前者；紧随一次排空 / trim /
    /// 重同步 ⇒ 后者。所以段首那一行把三个「上次发生在什么时候」的年龄、当时的
    /// 环深度、以及本 tick 准不准时一起打出来——一个能潜伏 21 小时的问题不该
    /// 只留下一个孤零零的计数。
    ///
    /// `avail` 是**本次读之前**环里的帧数（读后残量恒为 0，没有信息量）。
    fn note_short(&mut self, short: u32, avail: u32, now_us: u64) {
        let run_before = self.short_run;
        self.short_run = if short > 0 { run_before + short } else { 0 };
        let run_now = self.short_run;
        let Some(s) = self.bridge.slots.get(self.slot as usize) else {
            return;
        };
        if short > 0 {
            // 计数**永远**先记。日志可以被限流压掉，计数不行——计数器是这套
            // 埋点的底线，日志只是它的注解。
            s.underrun_frames.fetch_add(short as u64, Ordering::Relaxed);
            s.underrun_worst_run.fetch_max(run_now, Ordering::Relaxed);
            if run_before > 0 {
                return; // 段中间，没有新信息
            }
            s.underrun_events.fetch_add(1, Ordering::Relaxed);
            let t = mono_us();
            s.underrun_start_us.store(t, Ordering::Relaxed);
            // 现场读数先落到局部量，`s` 的借用到此为止——下面的 `log_allow`
            // 要 `&mut self`。
            let drain = age_str(s.last_drain_us.load(Ordering::Relaxed), t);
            let drain_n = s.last_drain_frames.load(Ordering::Relaxed);
            let drain_left = s.last_drain_left.load(Ordering::Relaxed);
            let trimmed = age_str(s.last_trim_us.load(Ordering::Relaxed), t);
            let trim_n = s.last_trim_frames.load(Ordering::Relaxed);
            let resync = age_str(dll::LAST_RESYNC_US.load(Ordering::Relaxed), t);
            let punctual = self.bridge.tick_punctual.load(Ordering::Relaxed);
            let target = self.ctl.d_target_frames() / trim::MSF as f32;
            // 段首与段尾**成对**打印或**成对**压掉：只剩一半的日志比没有更难读。
            if !self.log_allow(t) {
                // 起点戳作废，段尾据此知道自己也该闭嘴。
                if let Some(s) = self.bridge.slots.get(self.slot as usize) {
                    s.underrun_start_us.store(0, Ordering::Relaxed);
                }
                return;
            }
            dlog!(
                "[audiohubd] 欠载开始 slot {} 环里只有 {} 帧（{:.1}ms），本 tick 差 {} 帧；\
                 上次排空 {drain}（排掉 {drain_n} 帧、剩 {drain_left} 帧）、\
                 上次 trim {trimmed}（削 {trim_n} 帧）、上次 DLL 重同步 {resync}；\
                 准时={punctual} 目标水位={target:.1}ms",
                self.slot,
                avail,
                avail as f32 / trim::MSF as f32,
                short,
            );
            return;
        }
        if run_before > 0 {
            let t = mono_us();
            // `swap(0)` 兼作「段首那一行有没有打印」的标记：被限流压掉的段不写
            // 起点戳，于是这里 `None` ⇒ 段尾也不打印，成对性由数据本身保证。
            let start = s.underrun_start_us.swap(0, Ordering::Relaxed);
            if start != 0 {
                // 帧长度与墙钟长度**必须一起看**。两者相当 ⇒ 我们一直在按 10 ms
                // 读、只是环里没东西（生产侧的病）；墙钟远长于帧长度 ⇒ 这段时间
                // 我们自己也没在按时读（消费侧被抢占），欠载只是那次抢占的影子。
                dlog!(
                    "[audiohubd] 欠载结束 slot {} 连续补了 {} 帧（{:.1}ms），墙钟 {:.1}ms；\
                     累计 {} 次 / {} 帧{}",
                    self.slot,
                    run_before,
                    run_before as f32 / trim::MSF as f32,
                    (t.saturating_sub(start)) as f64 / 1000.0,
                    s.underrun_events.load(Ordering::Relaxed),
                    s.underrun_frames.load(Ordering::Relaxed),
                    match std::mem::take(&mut self.log_suppressed) {
                        0 => String::new(),
                        n => format!("（前面另有 {n} 段因限流未记录）"),
                    },
                );
            }
            // §6.3 伺服：已经付出过的代价不许被 60 s 窗口滑出后遗忘。
            self.ctl.on_underrun(run_before, now_us);
        }
    }

    /// 欠载日志的令牌桶：每 [`Self::LOG_WINDOW_US`] 最多 [`Self::LOG_PER_WINDOW`]
    /// 段。压掉的段计进 `log_suppressed`，下一条段尾行把它报出来——**被限流**与
    /// **没发生**必须能分开，否则限流本身就成了新的观测盲区。
    fn log_allow(&mut self, now_us: u64) -> bool {
        if now_us.saturating_sub(self.log_window_us) >= Self::LOG_WINDOW_US {
            self.log_window_us = now_us;
            self.log_in_window = 0;
        }
        if self.log_in_window >= Self::LOG_PER_WINDOW {
            self.log_suppressed = self.log_suppressed.saturating_add(1);
            return false;
        }
        self.log_in_window += 1;
        true
    }

    fn publish(&self) {
        let Some(s) = self.bridge.slots.get(self.slot as usize) else {
            return;
        };
        let ms = |frames: f32| frames / trim::MSF as f32;
        s.trim_target_ms
            .store(ms(self.ctl.d_target_frames()).to_bits(), Ordering::Relaxed);
        s.dll_target_ms
            .store(ms(self.ctl.dll_target_frames()).to_bits(), Ordering::Relaxed);
        s.trim_drawdown_ms.store(
            ms(self.ctl.drawdown_frames() as f32).to_bits(),
            Ordering::Relaxed,
        );
        s.trim_tokens_ms
            .store(ms(self.ctl.tokens_frames()).to_bits(), Ordering::Relaxed);
    }
}

impl audiohub_net::media::FrameSource for HalSpeakerSource {
    fn next_frame(&mut self, out: &mut Vec<f32>) -> bool {
        // FrameSource::next_frame REPLACES `out` (ToneSource/MicSource both open
        // with a clear) while append_spk_frame APPENDS, so without this the
        // engine's shared frame grew every tick and its `resize(F48)` kept the
        // FIRST 480 samples — the ones captured before any app had played
        // anything. The ring counters, the peak probe and the packet counts all
        // looked healthy while the peer received that initial silence, forever.
        out.clear();
        let now_us = self.t0.elapsed().as_micros() as u64;
        self.tick(now_us, out);
        // AUDIOHUB_HAL_DEBUG=1 prints the peak of what actually came out of the
        // ring once a second. Frame COUNTS advancing only proves the ring header
        // is shared; they say nothing about the sample area, and a silent stream
        // with a healthy counter is exactly the failure this exists to tell apart.
        // Read once: `env::var_os` allocates and takes the process-wide env lock,
        // and this runs every 10ms on the audio path in release builds too.
        static DEBUG_PEAK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *DEBUG_PEAK.get_or_init(|| std::env::var_os("AUDIOHUB_HAL_DEBUG").is_some()) {
            let peak = out.iter().fold(0.0f32, |m, s| m.max(s.abs()));
            self.dbg_peak = self.dbg_peak.max(peak);
            self.dbg_frames += 1;
            if self.dbg_frames >= 100 {
                dlog!("[audiohubd] hal spk peak over 1s: {:.5}", self.dbg_peak);
                self.dbg_peak = 0.0;
                self.dbg_frames = 0;
            }
        }
        true // silence when idle: the virtual device is alive even with no app on it
    }
    /// 只有虚拟扬声器环一级。`None` = 驱动没附着，那一级**不存在**（不是 0 ms）。
    fn depths(&self) -> audiohub_core::latency::SourceDepths {
        use audiohub_core::latency::{DropMode, StageDepth, StageId};
        let Some((samples, capacity)) = self.bridge.rings.spk_readable(self.slot as usize) else {
            return audiohub_core::latency::NO_DEPTHS;
        };
        [
            Some(StageDepth {
                id: StageId::HalSpk,
                samples,
                capacity,
                rate: HAL_SAMPLE_RATE,
                // 环满时写不进去的是生产者（驱动的 IOProc），丢的是新样本；
                // 我们这一侧只做消费，从不 pop_front，所以绝不是 Oldest。
                // 丢弃计数在驱动那一侧，本进程只有两个下标 —— 观测不到就报
                // None，不报 0（报 0 是「这一级很健康」的假保证）。
                dropped: None,
                drop_mode: DropMode::Newest,
            }),
            None,
        ]
    }

    fn sample_rate(&self) -> u32 {
        HAL_SAMPLE_RATE
    }
}

// ---------------------------------------------------------------- shared state

/// Everything that is per-slot rather than per-bridge. One record per ring
/// pair, allocated once and never moved.
struct SlotShared {
    spk_frames: AtomicU64,
    mic_frames: AtomicU64,
    mic_dropped: AtomicU64,
    /// The slot's stamp, as last reported by `CTL_BIND_STATE`. 0 = "we know
    /// nothing about this slot", and everything the driver sends about it is
    /// dropped until a BindState says otherwise.
    generation: AtomicU32,

    // ---- 延迟水位治理的埋点（规格 §10.1）。全部 Relaxed：它们只被读来看，
    //      不参与任何同步；写在 10 ms 音频路径上，代价必须是零。
    trim_events: AtomicU64,
    trim_frames: AtomicU64,
    trim_forced: AtomicU64,
    trim_deferred: AtomicU64,
    /// 三个当前读数按 f32 的位模式存。存 ms 而不是帧：读它的人是 UI 和 probe。
    trim_target_ms: AtomicU32,
    /// DLL 伺服真正在追的水位（`Ctl::dll_target_frames`）。与 `trim_target_ms`
    /// 分开发布：后者在 `safety_net` 档是固定的重同步触发线，不是稳态水位。
    dll_target_ms: AtomicU32,
    trim_drawdown_ms: AtomicU32,
    trim_tokens_ms: AtomicU32,
    underrun_frames: AtomicU64,
    underrun_events: AtomicU64,
    underrun_worst_run: AtomicU32,
    skip_drained: AtomicU64,

    // ---- `hal_mic` 生产侧闸门（`micgate`）的埋点。
    //
    // 这一级此前**一个仪表都没有**：`latency_guard` 的键里根本没有 `hal_mic`，
    // 「环里 132 ms 是存量还是设计深度」在运行时无法分辨，而唯一沾边的
    // `mic_dropped` 只在环**满**（500 ms）时才动——132 ms 时它是 0，
    // 499 ms 时它还是 0，只有饱和才报警。
    /// 排空段的**段数**（进入排空 +1），以及它持续了多少拍、丢掉多少帧。
    /// 段数与拍数分开：一次 47 拍的连续空洞与 47 次单拍毛刺是完全不同的听感。
    mic_drain_events: AtomicU64,
    mic_drain_ticks: AtomicU64,
    mic_withheld_frames: AtomicU64,
    /// **麦克风方向的欠载判据**：观测到水位 < 一个消费量子的拍数。
    ///
    /// 扬声器方向的欠载我们当场就知道（自己读不满）；这一级的欠载发生在驱动
    /// 进程里、没有任何回执，表现是 App 录到静音或断续。所以判据只能是
    /// 「水位低到驱动下一次读必然取不满」，而且必须比扬声器方向更早报警。
    mic_starved_ticks: AtomicU64,
    /// 会话内观测到的**最低**水位（帧）。`u32::MAX` = 还没观测过。
    /// 它是 `mic_starved_ticks` 恒为 0 时唯一还能回答「余量还剩多少」的读数。
    mic_low_water: AtomicU32,
    /// 最近一次观测到的水位（帧）。排空段里也发布——那正是最该看见它的时候。
    mic_depth_frames: AtomicU32,

    // ---- 归因埋点：「上一次 X 发生在什么时候」（[`mono_us`]，0 = 从未）。
    //
    // 存在的理由只有一条：欠载**只有计数没有时刻**时，「排空过头」与「生产侧
    // 卡顿」这两个性质完全相反的解释在数据上无法分辨——一个能潜伏 21 小时的
    // 问题不该只有一个孤零零的计数器。有了这三个戳，欠载那一行就能自答
    // 「我是不是紧跟在一次排空后面」。
    /// 上一次治法 A 排空（`drain_spk`）的时刻，以及它排掉了多少帧。
    last_drain_us: AtomicU64,
    last_drain_frames: AtomicU32,
    /// 排空**之后**环里还剩多少帧。这是「排空过头」的直接证据：它若接近
    /// `D_FLOOR_MIN`（720），说明那一刀贴着工作储备落下，紧随其后的欠载就是
    /// 我们自己造成的；它若仍在目标水位附近，排空就洗清了嫌疑。
    last_drain_left: AtomicU32,
    /// 上一次 trim 削减（`try_trim`）的时刻与削掉的 τ。
    last_trim_us: AtomicU64,
    last_trim_frames: AtomicU32,
    /// 上一段欠载**开始**的时刻。段结束时用它算段的墙钟长度——与帧数长度对照
    /// 能直接看出「这段时间我们到底在不在按 10 ms 读」。
    underrun_start_us: AtomicU64,

    /// **DLL 的相位误差观测**：`D_target − 读后残量`，帧，按 f32 的位模式存。
    ///
    /// 符号是**消费者语义**（`err > 0` = 请让水位涨），推导见 [`super::dll`]
    /// 的模块文档。这里存的是**误差**而不是「目标」与「水位」两个数，是为了让
    /// 相位在源头就被钉死：两者必须取自**同一次读的同一个相位**（读后残量），
    /// 分开发布再由 `tx_loop` 相减，中间隔着一次调度就会差出一整帧。
    dll_err_frames: AtomicU32,
    /// 每发布一次有效观测 +1。`tx_loop` 靠它分辨「这一 tick 这条环真的被消费
    /// 了」与「上一次的读数还挂在那儿」——一条不再被消费的槽的陈旧误差如果被
    /// 当成新鲜的，环路就会去追一个早已不存在的水位。
    ///
    /// 追平期（`tick_punctual == false`）与不连续 tick **不发布**：那些 tick 的
    /// 水位是假高（我们暂时没读，不是积压），喂进环路等于让它去排一段马上就要
    /// 被自己读走的音频（不变量 I6 的 DLL 侧对应物）。
    dll_epoch: AtomicU64,

    /// **不连续计数**：每次有人从消费者这一侧非顺序地推进 `read_idx`
    /// （治法 A 的排空、代次变更后的 flush）就 +1。
    ///
    /// 水位控制器靠它把「上一 tick 的读后残量」作废：`W_n = A_{n+1} − D_n`
    /// 只在两次读之间**只有生产者动过**时才成立，排空之后那个差值是垃圾，
    /// 喂进 `MaxDrawdown` 递推会把欠载边界永久抬高。
    disc_epoch: AtomicU64,
}

impl SlotShared {
    fn new() -> SlotShared {
        SlotShared {
            spk_frames: AtomicU64::new(0),
            mic_frames: AtomicU64::new(0),
            mic_dropped: AtomicU64::new(0),
            generation: AtomicU32::new(0),
            trim_events: AtomicU64::new(0),
            trim_frames: AtomicU64::new(0),
            trim_forced: AtomicU64::new(0),
            trim_deferred: AtomicU64::new(0),
            trim_target_ms: AtomicU32::new(0),
            dll_target_ms: AtomicU32::new(0),
            trim_drawdown_ms: AtomicU32::new(0),
            trim_tokens_ms: AtomicU32::new(0),
            underrun_frames: AtomicU64::new(0),
            underrun_events: AtomicU64::new(0),
            underrun_worst_run: AtomicU32::new(0),
            skip_drained: AtomicU64::new(0),
            mic_drain_events: AtomicU64::new(0),
            mic_drain_ticks: AtomicU64::new(0),
            mic_withheld_frames: AtomicU64::new(0),
            mic_starved_ticks: AtomicU64::new(0),
            mic_low_water: AtomicU32::new(u32::MAX),
            mic_depth_frames: AtomicU32::new(0),
            dll_err_frames: AtomicU32::new(0),
            dll_epoch: AtomicU64::new(0),
            disc_epoch: AtomicU64::new(0),
            last_drain_us: AtomicU64::new(0),
            last_drain_frames: AtomicU32::new(0),
            last_drain_left: AtomicU32::new(0),
            last_trim_us: AtomicU64::new(0),
            last_trim_frames: AtomicU32::new(0),
            underrun_start_us: AtomicU64::new(0),
        }
    }

    fn snapshot(&self) -> HalSlotCounters {
        HalSlotCounters {
            spk_frames: self.spk_frames.load(Ordering::Relaxed),
            mic_frames: self.mic_frames.load(Ordering::Relaxed),
            mic_dropped: self.mic_dropped.load(Ordering::Relaxed),
            generation: self.generation.load(Ordering::Relaxed),
            trim: HalTrimCounters {
                events: self.trim_events.load(Ordering::Relaxed),
                frames: self.trim_frames.load(Ordering::Relaxed),
                forced: self.trim_forced.load(Ordering::Relaxed),
                deferred_ticks: self.trim_deferred.load(Ordering::Relaxed),
                target_ms: f32::from_bits(self.trim_target_ms.load(Ordering::Relaxed)),
                dll_target_ms: f32::from_bits(self.dll_target_ms.load(Ordering::Relaxed)),
                drawdown_ms: f32::from_bits(self.trim_drawdown_ms.load(Ordering::Relaxed)),
                tokens_ms: f32::from_bits(self.trim_tokens_ms.load(Ordering::Relaxed)),
            },
            underrun: HalUnderrunCounters {
                frames: self.underrun_frames.load(Ordering::Relaxed),
                events: self.underrun_events.load(Ordering::Relaxed),
                worst_run_frames: self.underrun_worst_run.load(Ordering::Relaxed),
            },
            skip_drained_frames: self.skip_drained.load(Ordering::Relaxed),
            mic_gate: HalMicGateCounters {
                drain_events: self.mic_drain_events.load(Ordering::Relaxed),
                drain_ticks: self.mic_drain_ticks.load(Ordering::Relaxed),
                withheld_frames: self.mic_withheld_frames.load(Ordering::Relaxed),
                starved_ticks: self.mic_starved_ticks.load(Ordering::Relaxed),
                // `u32::MAX` 是「一拍都还没观测过」的哨兵。**不折成 0**：
                // 0 帧是「环空了、必然短读」这个最坏读数，与「还不知道」
                // 长得完全一样就会把一次未观测报成一次事故。
                low_water_ms: match self.mic_low_water.load(Ordering::Relaxed) {
                    u32::MAX => None,
                    f => Some(crate::micgate::frames_to_ms(f)),
                },
                depth_ms: crate::micgate::frames_to_ms(
                    self.mic_depth_frames.load(Ordering::Relaxed),
                ),
            },
        }
    }
}

struct Shared {
    stop: AtomicBool,
    driver_found: AtomicBool,
    driver_connected: AtomicBool,
    slots: [SlotShared; HAL_MAX_SLOTS],
    last_driver_msg: Mutex<Option<Instant>>,
    events: Mutex<Vec<HalControlEvent>>,
    /// One bit per slot, set by the service thread on every handshake and
    /// whenever a slot's generation changes; consumed by whichever daemon
    /// thread reads that slot's speaker ring next. Only the consumer may move
    /// `read_idx`, so the flush has to happen there rather than here.
    ///
    /// A BITMASK rather than a flag because a generation change is per slot: a
    /// single flag would either flush all sixteen consumers (dropping live
    /// audio on fifteen innocent slots) or none.
    spk_flush: AtomicU16,
    /// Slots the device coordinator has bound. The tx loop drains the idle ones
    /// so their rings cannot fill up (spec-m5b §5.4).
    published: AtomicU16,
    /// The driver's session id from the last handshake. Every `Bind` quotes it;
    /// the driver answers `StaleSession` to anything else.
    session_id: AtomicU64,
    /// Bumped on every completed handshake, so the coordinator can tell "still
    /// the same rings" from "re-attached, re-Set everything".
    attach_epoch: AtomicU64,
    /// Slots this driver actually offers (`slot_count` from the reply).
    slot_count: AtomicU32,
    /// What the driver said it speaks when it refused us over version skew.
    driver_protocol: AtomicU32,
    /// Why there is no bridge right now, for `daemon.status`.
    status_reason: Mutex<Option<String>>,
    /// How many binds/unbinds the driver has refused or half-performed, and
    /// what the most recent one said.
    ///
    /// SURFACED OVER IPC, not merely logged. A driver that fails to publish an
    /// endpoint while still answering every message is invisible to every
    /// other field here — `driver_connected` stays true, `state` stays
    /// `bound` — so the daemon has to carry the failure itself or nothing
    /// upstream can ever show it. That gap is precisely how "the speaker is
    /// gone" became something only a human could notice.
    bind_failures: AtomicU64,
    last_bind_error: Mutex<Option<String>>,
    endpoint_name_fallbacks: AtomicU64,
    rings: platform::Rings,
    /// Send right on the driver's service port, 0 when no driver is attached.
    /// A mutex rather than an atomic because two threads send on it (the
    /// service loop's ping and the daemon's volume relay) and a lost race
    /// would mean sending on a name someone else just deallocated.
    driver_port: Mutex<u32>,
    /// The driver told us another daemon took the rings (`CTL_SUPERSEDED`).
    /// Set by the receive path, acted on by the service loop.
    superseded: AtomicBool,
    /// **本 tick 是不是准时的**（`behind <= tick`）。由 `tx_loop` 每 tick 写一次，
    /// 由每个 `HalSpeakerSource` 读（它们全都被那一条线程消费）。
    ///
    /// 规格不变量 I6：追平期（`behind > tick`，循环背靠背补跑）的水位是**假高**
    /// ——它高是因为我们暂时没读，不是因为积压。在那些 tick 上 trim 会把马上就
    /// 要用到的音频削掉，紧接着就欠载。所以水位状态机只在准时 tick 上评估，
    /// 追平期的读数也不喂 `MaxDrawdown` 递推。
    ///
    /// 默认 `true`：没有 tx_loop 的场景（测试、非 mac）不该被当成永久追平期。
    tick_punctual: AtomicBool,
}

/// Bounded so a daemon that never drains cannot grow this without limit; the
/// newest state is what matters, so the oldest entries go first.
const MAX_PENDING_EVENTS: usize = 256;

impl Shared {
    fn push_event(&self, ev: HalControlEvent) {
        let mut q = lk(&self.events);
        if q.len() >= MAX_PENDING_EVENTS {
            q.remove(0);
        }
        q.push(ev);
    }

    /// Arms the flush for one slot. Called when that slot's generation moves:
    /// the driver resets a ring only while it is unpublished, so the daemon's
    /// own `read_idx` is what would otherwise compute `avail = 0 - 24000` and
    /// replay a full half second of the PREVIOUS peer's audio to the next one
    /// (spec-m5b §4.6).
    fn arm_flush(&self, slot: u8) {
        if (slot as usize) < HAL_MAX_SLOTS {
            self.spk_flush.fetch_or(1 << slot, Ordering::AcqRel);
        }
    }

    /// Takes the flush bit for one slot, if set.
    fn take_flush(&self, slot: u8) -> bool {
        if slot as usize >= HAL_MAX_SLOTS {
            return false;
        }
        let bit = 1u16 << slot;
        self.spk_flush.fetch_and(!bit, Ordering::AcqRel) & bit != 0
    }

    fn append_spk_frame(&self, slot: u8, out: &mut Vec<f32>) -> usize {
        let got = self.read_spk_chunk(slot, out, HAL_FRAME_48K);
        if got < HAL_FRAME_48K {
            out.resize(out.len() + (HAL_FRAME_48K - got), 0.0);
        }
        got
    }

    /// Appends at most `frames` (<= HAL_FRAME_48K) mono samples.
    fn read_spk_chunk(&self, slot: u8, out: &mut Vec<f32>, frames: usize) -> usize {
        let frames = frames.min(HAL_FRAME_48K);
        if self.take_flush(slot) {
            self.rings.flush_spk_consumer(slot as usize);
        }
        let mut scratch = [0.0f32; HAL_FRAME_48K * (HAL_SPK_CHANNELS as usize)];
        let got = self.rings.read_spk(
            slot as usize,
            &mut scratch[..frames * HAL_SPK_CHANNELS as usize],
            frames,
        );
        for f in 0..got {
            let l = scratch[f * 2];
            let r = scratch[f * 2 + 1];
            out.push((l + r) * 0.5);
        }
        if got > 0 {
            if let Some(c) = self.slots.get(slot as usize) {
                c.spk_frames.fetch_add(got as u64, Ordering::Relaxed);
            }
        }
        got
    }

    fn write_mic(&self, slot: u8, mono: &[f32]) -> usize {
        // `None` is "no driver attached, so there is no ring": nothing was
        // accepted, but nothing was DROPPED either. mic_dropped means "the
        // driver is not draining", and counting an absent driver into it would
        // bury the only reading of that number that diagnoses anything.
        let Some(wrote) = self.rings.write_mic(slot as usize, mono) else {
            return 0;
        };
        if let Some(c) = self.slots.get(slot as usize) {
            c.mic_frames.fetch_add(wrote as u64, Ordering::Relaxed);
            if wrote < mono.len() {
                c.mic_dropped
                    .fetch_add((mono.len() - wrote) as u64, Ordering::Relaxed);
            }
        }
        wrote
    }

    fn notify_volume(&self, at: HalEndpoint, generation: u32, scalar: f32, muted: bool) {
        platform::send_notify(self, at, generation, scalar, muted);
    }
}

// ---------------------------------------------------------------- macOS

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    use std::ffi::CString;

    // ---- mach ABI ----------------------------------------------------------
    //
    // The bitfields in `mach_msg_port_descriptor_t` (pad2:16, disposition:8,
    // type:8 sharing one word after name+pad1) are laid out low-bit-first on
    // little-endian, which both macOS arches are — that is why plain u16/u8/u8
    // fields reproduce the C layout exactly.

    pub type MachPort = u32;
    pub type KernReturn = i32;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct MsgHeader {
        bits: u32,
        size: u32,
        remote: MachPort,
        local: MachPort,
        voucher: MachPort,
        id: i32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct MsgBody {
        descriptor_count: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct PortDescriptor {
        name: MachPort,
        pad1: u32,
        pad2: u16,
        disposition: u8,
        dtype: u8,
    }

    /// daemon -> driver, `AudioHubHelloRequest`. EXACTLY ONE descriptor: a send
    /// right on our control port, which is where the driver posts volume, IO
    /// state and heartbeats. No memory entries — the driver owns the rings and
    /// a second descriptor is a hard reject on its side (see below).
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct HelloRequest {
        header: MsgHeader,
        body: MsgBody,
        control_port: PortDescriptor,
        protocol_version: u32,
        client_pid: u32,
    }

    /// driver -> daemon, `AudioHubHelloReply`, on the send-once reply port.
    ///
    /// Complex with exactly `2 * slot_count` port descriptors ONLY when
    /// `status` is OK; on any other status it is a PLAIN message whose
    /// descriptor words are zero. So `entries` is meaningless until both the
    /// COMPLEX bit and `status` have been tested — reading it first would hand
    /// `mach_vm_map` a port name that was never received.
    ///
    /// `entries[2*s]` is slot s's out ring and `entries[2*s+1]` its in ring:
    /// the array is indexed by the same endpoint number the control plane uses.
    /// One reply carrying all 32 is measured rather than assumed — see
    /// `a_single_reply_can_carry_thirty_two_memory_entries` below.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct HelloReply {
        header: MsgHeader,
        body: MsgBody,
        entries: [PortDescriptor; HAL_MAX_ENDPOINTS],
        status: u32,
        protocol_version: u32,
        slot_count: u32,
        data_offset: u32,
        spk_capacity_frames: u32,
        spk_channels: u32,
        mic_capacity_frames: u32,
        mic_channels: u32,
        sample_rate: u32,
        // The descriptor array ends at 412, which is 4 mod 8, so an ODD number
        // of u32 must follow before the first u64 or the compiler inserts four
        // bytes of padding here and the two ends read the ring geometry four
        // bytes apart — with neither side's build failing on its own. Nine of
        // them; the offset asserts below are what keeps that true.
        session_id: u64,
        spk_bytes: u64,
        mic_bytes: u64,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct ControlMsg {
        header: MsgHeader,
        op: u32,
        endpoint: u32,
        scalar_bits: u32,
        flags: u32,
        /// The slot's stamp when this message was produced; 0 means "concerns
        /// no slot" (Heartbeat / Superseded / Ping). Anything whose stamp is
        /// not the one the receiver currently holds for that slot is dropped,
        /// which is what stops a late StopIO from lighting up the NEXT peer's
        /// microphone after the slot has been reused (spec-m5b §4.6).
        generation: u32,
        reserved: u32,
        seq: u64,
    }

    /// daemon -> driver, `AudioHubBindMsg`. Binds one slot to a peer or retires
    /// it; the driver answers on the control port with a `CTL_BIND_STATE`, so
    /// the outcome comes back through the same closed loop as every other slot
    /// transition rather than from mach's send status.
    ///
    /// The strings are fixed-size and the RECEIVER terminates them — neither
    /// end trusts the sender's terminator.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct BindMsg {
        header: MsgHeader,
        op: u32,
        slot: u32,
        flags: u32,
        generation: u32,
        session_id: u64,
        peer_key: [u8; 40],
        out_uid: [u8; 64],
        in_uid: [u8; 64],
        out_name: [u8; 128],
        in_name: [u8; 128],
    }

    // Mirrors the _Static_asserts in AudioHubBridge.h one for one, OFFSETS
    // included: a size that happens to match while a field moved is exactly the
    // drift these exist to catch. A break here is a struct read at the wrong
    // offsets on the far side of a mach message, so it has to fail the build,
    // not the audio.
    const _: () = {
        assert!(std::mem::size_of::<MsgHeader>() == 24);
        assert!(std::mem::size_of::<MsgBody>() == 4);
        assert!(std::mem::size_of::<PortDescriptor>() == 12);
        assert!(std::mem::offset_of!(HelloRequest, control_port) == 28);
        assert!(std::mem::offset_of!(HelloRequest, protocol_version) == 40);
        assert!(std::mem::size_of::<HelloRequest>() == 48);

        assert!(std::mem::offset_of!(HelloReply, entries) == 28);
        assert!(std::mem::offset_of!(HelloReply, status) == 412);
        assert!(std::mem::offset_of!(HelloReply, session_id) == 448);
        assert!(std::mem::offset_of!(HelloReply, spk_bytes) == 456);
        assert!(std::mem::offset_of!(HelloReply, mic_bytes) == 464);
        assert!(std::mem::size_of::<HelloReply>() == 472);

        assert!(std::mem::offset_of!(ControlMsg, endpoint) == 28);
        assert!(std::mem::offset_of!(ControlMsg, generation) == 40);
        assert!(std::mem::offset_of!(ControlMsg, seq) == 48);
        assert!(std::mem::size_of::<ControlMsg>() == 56);

        assert!(std::mem::offset_of!(BindMsg, session_id) == 40);
        assert!(std::mem::offset_of!(BindMsg, peer_key) == 48);
        assert!(std::mem::offset_of!(BindMsg, out_uid) == 88);
        assert!(std::mem::offset_of!(BindMsg, in_uid) == 152);
        assert!(std::mem::offset_of!(BindMsg, out_name) == 216);
        assert!(std::mem::offset_of!(BindMsg, in_name) == 344);
        assert!(std::mem::size_of::<BindMsg>() == 472);

        assert!(std::mem::size_of::<RingHeader>() == 40);
        assert!(std::mem::offset_of!(RingHeader, write_idx) == 24);
        assert!(std::mem::offset_of!(RingHeader, read_idx) == 32);
        assert!(std::mem::size_of::<RingHeader>() <= HAL_RING_DATA_OFFSET);
    };

    const MACH_PORT_NULL: MachPort = 0;
    const MACH_MSG_TYPE_MAKE_SEND_ONCE: u32 = 21;
    const MACH_MSG_TYPE_COPY_SEND: u32 = 19;
    const MACH_MSG_TYPE_MAKE_SEND: u32 = 20;
    const MACH_MSG_PORT_DESCRIPTOR: u8 = 0;
    const MACH_MSGH_BITS_COMPLEX: u32 = 0x8000_0000;
    const MACH_SEND_MSG: i32 = 0x0000_0001;
    const MACH_RCV_MSG: i32 = 0x0000_0002;
    const MACH_SEND_TIMEOUT: i32 = 0x0000_0010;
    const MACH_RCV_TIMEOUT: i32 = 0x0000_0100;
    const MACH_MSG_SUCCESS: KernReturn = 0;
    const MACH_RCV_TIMED_OUT: KernReturn = 0x1000_4003u32 as i32;
    const MACH_SEND_TIMED_OUT: KernReturn = 0x1000_0004u32 as i32;
    /// The driver's port died — coreaudiod exited or the plug-in was unloaded.
    /// This, not a silence heuristic, is what makes a reconnect deterministic.
    const MACH_SEND_INVALID_DEST: KernReturn = 0x1000_0003u32 as i32;
    const KERN_SUCCESS: KernReturn = 0;
    const TASK_BOOTSTRAP_PORT: i32 = 4;
    const VM_FLAGS_ANYWHERE: i32 = 0x0001;
    const VM_PROT_READ: i32 = 1;
    const VM_PROT_WRITE: i32 = 2;
    const VM_INHERIT_NONE: u32 = 2;
    /// Create a brand-new extant VM object rather than naming a range of our
    /// own address space. Both can share, but a freshly created named object
    /// has exactly one interpretation on the receiving side: map it and you
    /// have these pages. Naming an existing range depends on that range still
    /// being one VM object, which is a property no test can pin down.
    const MAP_MEM_NAMED_CREATE: i32 = 0x0002_0000;
    const MACH_PORT_RIGHT_SEND: i32 = 0;
    const MACH_PORT_RIGHT_RECEIVE: i32 = 1;
    const BOOTSTRAP_SUCCESS: KernReturn = 0;
    const BOOTSTRAP_UNKNOWN_SERVICE: KernReturn = 1102;
    /// `name_t` in <servers/bootstrap.h> is char[128]; a longer name is a
    /// caller bug, not something to hand to a C API that will truncate it.
    const BOOTSTRAP_MAX_NAME_LEN: usize = 127;

    const fn msgh_bits(remote: u32, local: u32) -> u32 {
        remote | (local << 8)
    }

    extern "C" {
        static mach_task_self_: MachPort;
        fn task_get_special_port(task: MachPort, which: i32, port: *mut MachPort) -> KernReturn;
        fn bootstrap_look_up(bp: MachPort, name: *const i8, sp: *mut MachPort) -> KernReturn;
        fn mach_msg(
            msg: *mut MsgHeader,
            option: i32,
            send_size: u32,
            rcv_size: u32,
            rcv_name: MachPort,
            timeout: u32,
            notify: MachPort,
        ) -> KernReturn;
        fn mach_msg_destroy(msg: *mut MsgHeader);
        fn mach_port_allocate(task: MachPort, right: i32, name: *mut MachPort) -> KernReturn;
        fn mach_port_deallocate(task: MachPort, name: MachPort) -> KernReturn;
        fn mach_port_mod_refs(
            task: MachPort,
            name: MachPort,
            right: i32,
            delta: i32,
        ) -> KernReturn;
        fn mach_vm_deallocate(task: MachPort, address: u64, size: u64) -> KernReturn;
        fn mach_make_memory_entry_64(
            target_task: MachPort,
            size: *mut u64,
            offset: u64,
            permission: i32,
            object_handle: *mut MachPort,
            parent_entry: MachPort,
        ) -> KernReturn;
        #[allow(clippy::too_many_arguments)]
        fn mach_vm_map(
            target_task: MachPort,
            address: *mut u64,
            size: u64,
            mask: u64,
            flags: i32,
            object: MachPort,
            offset: u64,
            copy: i32,
            cur_protection: i32,
            max_protection: i32,
            inheritance: u32,
        ) -> KernReturn;
    }

    fn task_self() -> MachPort {
        unsafe { mach_task_self_ }
    }

    /// Receive right, ours until [`PortGuard`] drops it.
    fn alloc_recv_port() -> Result<MachPort> {
        let mut p: MachPort = MACH_PORT_NULL;
        let kr = unsafe { mach_port_allocate(task_self(), MACH_PORT_RIGHT_RECEIVE, &mut p) };
        if kr != KERN_SUCCESS || p == MACH_PORT_NULL {
            anyhow::bail!("mach_port_allocate(receive) failed: {kr}");
        }
        Ok(p)
    }

    // ---- rings -------------------------------------------------------------

    #[repr(C)]
    pub struct RingHeader {
        magic: u32,
        version: u32,
        sample_rate: u32,
        channels: u32,
        capacity_frames: u32,
        reserved: u32,
        write_idx: AtomicU64,
        read_idx: AtomicU64,
    }

    /// Geometry of one ring exactly as the driver reported it in the reply.
    /// Nothing here is assumed from this file's constants; they only get to say
    /// what this daemon is able to consume.
    #[derive(Clone, Copy)]
    struct RingGeom {
        channels: u32,
        sample_rate: u32,
        capacity_frames: u32,
        data_offset: u32,
        bytes: u64,
    }

    /// A ring the DRIVER created, mapped into this task from the memory-entry
    /// port its reply carried. We own the send right and the mapping; the pages
    /// themselves outlive us, because the driver never unmaps them.
    struct RingMem {
        addr: u64,
        size: usize,
        entry: MachPort,
        channels: u32,
        /// Local copies of the bounds, validated ONCE against the mapping size
        /// at attach. The same numbers are in the shared header, but indexing
        /// with a bound read out of memory the peer can rewrite is how a shared
        /// ring turns into an out-of-bounds write.
        capacity: u32,
        data_offset: usize,
    }

    /// Largest ring this daemon will map. The driver asks for 192K/96K; the cap
    /// only exists so a nonsense `spk_bytes` becomes a refusal instead of a
    /// gigabyte-sized mapping.
    const MAX_RING_BYTES: u64 = 64 * 1024 * 1024;

    /// Maps a memory-entry port into this task, sharing (copy = FALSE) rather
    /// than copying. This is byte-for-byte what the driver does on its side,
    /// which is why the test can use it to prove the entry really shares.
    fn map_entry(entry: MachPort, bytes: usize) -> Result<u64> {
        let mut addr: u64 = 0;
        let kr = unsafe {
            mach_vm_map(
                task_self(),
                &mut addr,
                bytes as u64,
                0,
                VM_FLAGS_ANYWHERE,
                entry,
                0,
                0, // copy = FALSE: share the object's pages
                VM_PROT_READ | VM_PROT_WRITE,
                VM_PROT_READ | VM_PROT_WRITE,
                VM_INHERIT_NONE,
            )
        };
        if kr != KERN_SUCCESS || addr == 0 {
            anyhow::bail!("mach_vm_map({bytes}) failed: {kr}");
        }
        Ok(addr)
    }

    impl RingMem {
        /// Takes ownership of a memory-entry send right the driver sent us and
        /// maps it. `want_channels` is what this daemon's audio path is wired
        /// for (2 for spk, 1 for mic) — it is the ONE thing that cannot be
        /// renegotiated at runtime, so a driver reporting anything else is a
        /// named refusal. Everything else about the layout comes from `geom`.
        ///
        /// Consumes `entry` on every path: on failure it is deallocated here,
        /// on success `Drop` owns it.
        fn attach(entry: MachPort, geom: RingGeom, want_channels: u32, what: &str) -> Result<RingMem> {
            let mut me = RingMem {
                addr: 0,
                size: 0,
                entry,
                channels: geom.channels,
                capacity: geom.capacity_frames,
                data_offset: geom.data_offset as usize,
            };
            me.init(geom, want_channels, what)?; // Drop cleans up whatever got as far as being ours
            Ok(me)
        }

        fn init(&mut self, geom: RingGeom, want_channels: u32, what: &str) -> Result<()> {
            if self.entry == MACH_PORT_NULL {
                anyhow::bail!("driver sent no {what} memory entry");
            }
            if geom.channels != want_channels {
                anyhow::bail!(
                    "driver's {what} ring is {}ch, this daemon can only consume {want_channels}ch",
                    geom.channels
                );
            }
            if geom.sample_rate != HAL_SAMPLE_RATE {
                anyhow::bail!(
                    "driver's {what} ring runs at {}Hz, the media plane is fixed at {HAL_SAMPLE_RATE}Hz",
                    geom.sample_rate
                );
            }
            if geom.capacity_frames == 0 {
                anyhow::bail!("driver's {what} ring has no capacity");
            }
            let hdr_len = std::mem::size_of::<RingHeader>() as u32;
            if geom.data_offset < hdr_len || geom.data_offset % 4 != 0 {
                anyhow::bail!(
                    "driver's data_offset {} is not a 4-aligned offset past the {hdr_len}-byte header",
                    geom.data_offset
                );
            }
            // The bound that actually matters: every index this file computes
            // is < capacity, so the samples have to fit inside the mapping.
            let need = (geom.data_offset as u64)
                + (geom.capacity_frames as u64) * (geom.channels as u64) * 4;
            if geom.bytes < need || geom.bytes > MAX_RING_BYTES {
                anyhow::bail!(
                    "driver's {what} ring is {} bytes but its own geometry needs {need} (cap {MAX_RING_BYTES})",
                    geom.bytes
                );
            }

            self.addr = map_entry(self.entry, geom.bytes as usize)?;
            self.size = geom.bytes as usize;

            // Cross-check the mapping against what the reply claimed. If these
            // disagree the reply described a different object than the entry
            // port points at, and no amount of arithmetic recovers from that.
            let h = self.hdr();
            if h.magic != HAL_RING_MAGIC || h.version != HAL_RING_VERSION {
                anyhow::bail!(
                    "{what} ring header is magic {:#x} v{}, expected {HAL_RING_MAGIC:#x} v{HAL_RING_VERSION}",
                    h.magic,
                    h.version
                );
            }
            if h.channels != geom.channels
                || h.capacity_frames != geom.capacity_frames
                || h.sample_rate != geom.sample_rate
            {
                anyhow::bail!(
                    "{what} ring header says {}ch/{}fr/{}Hz, the reply said {}ch/{}fr/{}Hz",
                    h.channels,
                    h.capacity_frames,
                    h.sample_rate,
                    geom.channels,
                    geom.capacity_frames,
                    geom.sample_rate
                );
            }
            Ok(())
        }

        /// ATTACH-TIME AND TESTS ONLY. `RingHeader`'s identity fields are plain
        /// `u32` in memory the DRIVER may write concurrently, so holding a
        /// `&RingHeader` while the ring is live is a data race by the letter of
        /// the model even though only the two atomics are ever touched through
        /// it. The hot paths use `w_idx`/`r_idx`, which never form a reference
        /// to anything but the atomic itself.
        fn hdr(&self) -> &RingHeader {
            unsafe { &*(self.addr as *const RingHeader) }
        }

        /// The two indices, addressed by offset so no reference to the
        /// surrounding struct is ever created. `AtomicU64` is the only type in
        /// the header that both sides may write while the ring is live.
        fn w_idx(&self) -> &AtomicU64 {
            const OFF: usize = std::mem::offset_of!(RingHeader, write_idx);
            unsafe { &*((self.addr as usize + OFF) as *const AtomicU64) }
        }

        fn r_idx(&self) -> &AtomicU64 {
            const OFF: usize = std::mem::offset_of!(RingHeader, read_idx);
            unsafe { &*((self.addr as usize + OFF) as *const AtomicU64) }
        }

        fn data(&self) -> *mut f32 {
            (self.addr as usize + self.data_offset) as *mut f32
        }

        /// Producer. Returns frames written; a full ring drops the tail.
        fn write(&self, src: &[f32], frames: usize) -> usize {
            let cap = self.capacity as usize;
            let ch = self.channels as usize;
            let w = self.w_idx().load(Ordering::Relaxed);
            let r = self.r_idx().load(Ordering::Acquire);
            let used = (w.wrapping_sub(r) as usize).min(cap);
            let count = frames.min(cap - used).min(src.len() / ch);
            if count == 0 {
                return 0;
            }
            let start = (w % cap as u64) as usize;
            let first = (cap - start).min(count);
            unsafe {
                std::ptr::copy_nonoverlapping(
                    src.as_ptr(),
                    self.data().add(start * ch),
                    first * ch,
                );
                if count > first {
                    std::ptr::copy_nonoverlapping(
                        src.as_ptr().add(first * ch),
                        self.data(),
                        (count - first) * ch,
                    );
                }
            }
            self.w_idx().store(w + count as u64, Ordering::Release);
            count
        }

        /// Consumer. Returns frames read; the caller zero-fills the rest.
        fn read(&self, dst: &mut [f32], frames: usize) -> usize {
            let cap = self.capacity as usize;
            let ch = self.channels as usize;
            let r = self.r_idx().load(Ordering::Relaxed);
            let w = self.w_idx().load(Ordering::Acquire);
            // More than a full buffer behind means we stalled: jump to the
            // newest window rather than replay half a second of stale audio.
            let avail = (w.wrapping_sub(r) as usize).min(cap);
            let count = frames.min(avail).min(dst.len() / ch);
            if count == 0 {
                return 0;
            }
            // wrapping, not plain `-`: a SECOND daemon handshaking rewinds the
            // driver's indices to 0 underneath this mapping, which leaves our
            // read_idx ahead of write_idx for one pass. `w - avail` would panic
            // there in a debug build; wrapping re-converges on the next read.
            let effective = w.wrapping_sub(avail as u64);
            let start = (effective % cap as u64) as usize;
            let first = (cap - start).min(count);
            unsafe {
                std::ptr::copy_nonoverlapping(
                    self.data().add(start * ch),
                    dst.as_mut_ptr(),
                    first * ch,
                );
                if count > first {
                    std::ptr::copy_nonoverlapping(
                        self.data(),
                        dst.as_mut_ptr().add(first * ch),
                        (count - first) * ch,
                    );
                }
            }
            self.r_idx()
                .store(effective.wrapping_add(count as u64), Ordering::Release);
            count
        }

        /// Consumer, **只窥不取**：取样与 `read()` 完全相同，但不写 `read_idx`。
        ///
        /// 返回 `(帧数, 本次窥视的读基准)`。基准必须原样交给 [`RingMem::advance`]
        /// ——`read()` 是「算一次下标、拷一次、存一次」的整体，拆成两半之后如果
        /// advance 自己重算下标，中间被生产者推进过的那点差量就会被当成已消费。
        ///
        /// 并发安全性与 `read()` 完全一致：生产者只写 `[w, r+cap)`，与被窥的
        /// `[r, r+n)`（`n ≤ readable()`）不相交。
        fn peek(&self, dst: &mut [f32], frames: usize) -> (usize, u64) {
            let cap = self.capacity as usize;
            let ch = self.channels as usize;
            let r = self.r_idx().load(Ordering::Relaxed);
            let w = self.w_idx().load(Ordering::Acquire);
            let avail = (w.wrapping_sub(r) as usize).min(cap);
            let effective = w.wrapping_sub(avail as u64);
            let count = frames.min(avail).min(dst.len() / ch);
            if count == 0 {
                return (0, effective);
            }
            let start = (effective % cap as u64) as usize;
            let first = (cap - start).min(count);
            unsafe {
                std::ptr::copy_nonoverlapping(
                    self.data().add(start * ch),
                    dst.as_mut_ptr(),
                    first * ch,
                );
                if count > first {
                    std::ptr::copy_nonoverlapping(
                        self.data(),
                        dst.as_mut_ptr().add(first * ch),
                        (count - first) * ch,
                    );
                }
            }
            (count, effective)
        }

        /// 把 `read_idx` 从一次 [`RingMem::peek`] 的基准推进 `frames` 帧。
        fn advance(&self, base: u64, frames: usize) {
            self.r_idx()
                .store(base.wrapping_add(frames as u64), Ordering::Release);
        }

        /// Consumer 侧的主动丢弃：把 `read_idx` 前进至多 `frames` 帧，返回真正
        /// 丢掉的帧数。治法 A 用它，不搬运任何样本。
        fn drop_frames(&self, frames: usize) -> usize {
            let cap = self.capacity as usize;
            let r = self.r_idx().load(Ordering::Relaxed);
            let w = self.w_idx().load(Ordering::Acquire);
            let avail = (w.wrapping_sub(r) as usize).min(cap);
            let effective = w.wrapping_sub(avail as u64);
            let n = frames.min(avail);
            self.r_idx()
                .store(effective.wrapping_add(n as u64), Ordering::Release);
            n
        }

        /// 此刻环里占着的帧数。与 `read()` 里的 `avail` / `write()` 里的 `used`
        /// 用同一个式子（含同样的 `min(cap)` 封顶与 wrapping 语义），但
        /// **不移动任何下标**。
        ///
        /// 两个方向都用它：对扬声器环（驱动写、我们读）这是「可读」，对麦克风环
        /// （我们写、驱动读）这是「驱动还没取走的积压」。数值定义相同，物理含义
        /// 由调用方的方向决定 —— 见 `Rings::spk_readable` / `Rings::mic_occupied`。
        fn readable(&self) -> u32 {
            let r = self.r_idx().load(Ordering::Relaxed);
            let w = self.w_idx().load(Ordering::Acquire);
            (w.wrapping_sub(r) as usize).min(self.capacity as usize) as u32
        }

        /// Consumer-side backlog drop; only ever called on the ring this
        /// process consumes.
        fn flush_consumer(&self) {
            self.r_idx()
                .store(self.w_idx().load(Ordering::Acquire), Ordering::Release);
        }
    }

    impl Drop for RingMem {
        fn drop(&mut self) {
            // Only this task's mapping and this task's send right go away. The
            // pages belong to the driver's named object and it keeps its own
            // mapping for its whole life, so an IOProc running right now is
            // unaffected — it writes into a ring nobody is reading.
            unsafe {
                if self.entry != MACH_PORT_NULL {
                    mach_port_deallocate(task_self(), self.entry);
                }
                if self.addr != 0 {
                    mach_vm_deallocate(task_self(), self.addr, self.size as u64);
                }
            }
        }
    }

    struct RingPair {
        spk: RingMem,
        mic: RingMem,
    }

    /// EVERY slot's mappings, present only while a driver is attached.
    ///
    /// Still a single `Option`, so attach and detach stay whole-set atomic: a
    /// handshake either installs all `slot_count` pairs or none, and a detach
    /// takes them all away at once. A per-slot `Option` would let a caller read
    /// slot 3 of the previous driver while slot 4 belongs to the new one.
    ///
    /// The lock is NOT what makes the rings safe to use concurrently — the
    /// free-running SPSC indices do that, and both audio callers take it shared.
    /// It exists because this side unmaps on disconnect: a detach has to wait
    /// for the tx engine and the mixer to be out of the pages before
    /// `mach_vm_deallocate` runs, or a reconnect during playback is a segfault.
    /// The driver has no such lock and needs none; it never unmaps.
    ///
    /// A boxed SLICE rather than `[RingPair; HAL_MAX_SLOTS]`: a driver built
    /// with a smaller pool reports a smaller `slot_count` and hands over
    /// exactly that many pairs, and indexing a fixed array would then read
    /// mappings that were never made.
    pub struct Rings {
        inner: std::sync::RwLock<Option<Box<[RingPair]>>>,
    }

    // The pointers are into mappings owned by `RingPair` and live exactly as
    // long as it does, which the RwLock is what guarantees; the SPSC discipline
    // (one reader thread, one writer thread) is documented on the public methods.
    unsafe impl Send for Rings {}
    unsafe impl Sync for Rings {}

    impl Rings {
        pub fn new() -> Rings {
            Rings { inner: std::sync::RwLock::new(None) }
        }

        /// 0 with no driver attached (or a slot this driver does not have):
        /// the caller zero-fills, so a missing driver is silence, never a stall.
        pub fn read_spk(&self, slot: usize, dst: &mut [f32], frames: usize) -> usize {
            match rd(&self.inner).as_ref().and_then(|p| p.get(slot)) {
                Some(p) => p.spk.read(dst, frames),
                None => 0,
            }
        }

        /// `None` distinguishes "no ring at all" from "the ring was full",
        /// which are the same number of frames accepted but very different
        /// diagnoses. See `Shared::write_mic`.
        pub fn write_mic(&self, slot: usize, mono: &[f32]) -> Option<usize> {
            rd(&self.inner)
                .as_ref()
                .and_then(|p| p.get(slot))
                .map(|p| p.mic.write(mono, mono.len()))
        }

        pub fn flush_spk_consumer(&self, slot: usize) {
            if let Some(p) = rd(&self.inner).as_ref().and_then(|p| p.get(slot)) {
                p.spk.flush_consumer();
            }
        }

        /// 只窥不取（规格 §2.6）。`None` = 没有驱动附着 / 没有这个槽。
        pub fn peek_spk(
            &self,
            slot: usize,
            dst: &mut [f32],
            frames: usize,
        ) -> Option<(usize, u64)> {
            rd(&self.inner)
                .as_ref()
                .and_then(|p| p.get(slot))
                .map(|p| p.spk.peek(dst, frames))
        }

        /// 把读指针从一次 `peek_spk` 的基准推进 `frames` 帧。
        pub fn advance_spk(&self, slot: usize, base: u64, frames: usize) {
            if let Some(p) = rd(&self.inner).as_ref().and_then(|p| p.get(slot)) {
                p.spk.advance(base, frames);
            }
        }

        /// 治法 A：丢掉至多 `frames` 帧，返回真正丢掉的。
        pub fn drop_spk(&self, slot: usize, frames: usize) -> usize {
            rd(&self.inner)
                .as_ref()
                .and_then(|p| p.get(slot))
                .map(|p| p.spk.drop_frames(frames))
                .unwrap_or(0)
        }

        /// `(可读帧数, 容量帧数)`，**只读**：不动 `read_idx`，所以从这个方法
        /// 观测不会改变被测对象（规格 §0.3 点名的那条纪律——HAL 环是唯一一个
        /// 恒定驻留上限恰好 500 ms 的级，误把观测写成消费就会自己把它清零）。
        ///
        /// `None` = 没有驱动附着 / 没有这个槽 ⇒ 这一级不存在，不是 0 ms。
        pub fn spk_readable(&self, slot: usize) -> Option<(u32, u32)> {
            rd(&self.inner)
                .as_ref()
                .and_then(|p| p.get(slot))
                .map(|p| (p.spk.readable(), p.spk.capacity))
        }

        /// `(已占用帧数, 容量帧数)` of the MIC ring, **read-only**.
        ///
        /// 同一个 `w - r` 表达式在两个方向上含义不同：扬声器环我们是消费者，
        /// 它叫「可读」；麦克风环我们是生产者，同一个数是「驱动还没取走的积压」
        /// ——也正是我们此刻写进去的那一帧要等的排队量。所以方法名叫
        /// `occupied` 而不是 `readable`。
        ///
        /// `None` = 没有驱动附着 / 没有这个槽 ⇒ 这一级不存在，不是 0 ms。
        pub fn mic_occupied(&self, slot: usize) -> Option<(u32, u32)> {
            rd(&self.inner)
                .as_ref()
                .and_then(|p| p.get(slot))
                .map(|p| (p.mic.readable(), p.mic.capacity))
        }

        /// Installs a freshly handshaked set. Dropping whatever was there
        /// unmaps it, and the write lock is what makes that safe.
        fn attach(&self, pairs: Vec<RingPair>) {
            *wr(&self.inner) = Some(pairs.into_boxed_slice());
        }

        fn detach(&self) {
            *wr(&self.inner) = None;
        }

        fn attached(&self) -> bool {
            rd(&self.inner).is_some()
        }
    }

    // ---- service -----------------------------------------------------------

    const RECV_TIMEOUT_MS: u32 = 200;
    const SEND_TIMEOUT_MS: u32 = 500;
    /// Reply deadline for the one synchronous exchange there is. Generous: the
    /// driver answers from its own bridge thread, which may be mid-mapping.
    const HELLO_TIMEOUT_MS: u32 = 2_000;
    /// Liveness probe. Cheap (48 bytes, no reply) and it is what turns a dead
    /// coreaudiod into a MACH_SEND_INVALID_DEST instead of a guess.
    const PING_EVERY: Duration = Duration::from_secs(1);
    /// Only applied once the driver has spoken at least once: a driver that
    /// heartbeats and then stops is gone even if its port is still alive
    /// (plug-in unloaded inside a live coreaudiod). A driver that never speaks
    /// is not judged by silence — in this direction it owes us nothing.
    const DRIVER_SILENT_AFTER: Duration = Duration::from_secs(5);
    /// Backoff bounds for the look-up/handshake retry. Never spins: even the
    /// first failure waits.
    const RETRY_MIN: Duration = Duration::from_millis(500);
    const RETRY_MAX: Duration = Duration::from_secs(5);
    /// A driver whose queue stays full this many pings running is not draining
    /// its port; treat it as gone rather than wedging on it forever.
    const MAX_SEND_TIMEOUTS: u32 = 3;

    pub fn start(cfg: HalBridgeCfg) -> Result<Option<HalBridge>> {
        if cfg.mode == HalBridgeMode::Off {
            return Ok(None);
        }
        // The gate for `Auto`: does the driver's name resolve right now? Unlike
        // the old check-in, a look-up cannot accidentally succeed — the name
        // exists only because coreaudiod published it — so no marker env var is
        // needed and a hand-run daemon is a first-class citizen again.
        let first_port = match look_up(&cfg.service_name) {
            Ok(p) => Some(p),
            Err(e) => {
                if cfg.mode != HalBridgeMode::Require {
                    // The normal case on a machine with no driver. Silent: this
                    // is every CI box and every Mac before installation.
                    return Ok(None);
                }
                dlog!("[audiohubd] HAL bridge: '{}' not published yet ({e:#}); will keep looking", cfg.service_name);
                None
            }
        };

        // No shared memory is created here: the rings arrive in the reply. All
        // this needs is the port the driver will post Control to — and if even
        // that fails, the send right has to go back, or a daemon that retries
        // `start` accumulates rights on a port it no longer talks to.
        let control = match alloc_recv_port() {
            Ok(p) => p,
            Err(e) => {
                if let Some(p) = first_port {
                    unsafe { mach_port_deallocate(task_self(), p) };
                }
                return Err(e);
            }
        };
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            driver_found: AtomicBool::new(first_port.is_some()),
            driver_connected: AtomicBool::new(false),
            slots: std::array::from_fn(|_| SlotShared::new()),
            last_driver_msg: Mutex::new(None),
            events: Mutex::new(Vec::new()),
            spk_flush: AtomicU16::new(0),
            published: AtomicU16::new(0),
            session_id: AtomicU64::new(0),
            attach_epoch: AtomicU64::new(0),
            slot_count: AtomicU32::new(0),
            driver_protocol: AtomicU32::new(0),
            status_reason: Mutex::new(Some("no driver attached yet".to_string())),
            bind_failures: AtomicU64::new(0),
            last_bind_error: Mutex::new(None),
            endpoint_name_fallbacks: AtomicU64::new(0),
            rings: Rings::new(),
            driver_port: Mutex::new(MACH_PORT_NULL),
            superseded: AtomicBool::new(false),
            tick_punctual: AtomicBool::new(true),
        });
        dlog!("[audiohubd] HAL bridge: looking for mach service '{}'", cfg.service_name);

        let s = shared.clone();
        let name = cfg.service_name.clone();
        // service_loop is what builds the PortGuard, so a thread that never
        // starts leaks the receive right AND the send right for the life of the
        // process. The names are Copy, so keep them to clean up by hand — the
        // closure that owned them is dropped inside spawn() on failure.
        let (ctl_name, first_name) = (control, first_port);
        let thread = match std::thread::Builder::new()
            .name("ahb-halbridge".to_string())
            .spawn(move || service_loop(s, name, control, first_port))
        {
            Ok(t) => t,
            Err(e) => {
                unsafe {
                    mach_port_mod_refs(task_self(), ctl_name, MACH_PORT_RIGHT_RECEIVE, -1);
                    if let Some(p) = first_name {
                        mach_port_deallocate(task_self(), p);
                    }
                }
                return Err(e.into());
            }
        };

        Ok(Some(HalBridge { shared, thread: Mutex::new(Some(thread)) }))
    }

    /// A SEND right on the driver's service port, or an error naming why not.
    /// `BOOTSTRAP_UNKNOWN_SERVICE` is the ordinary "no driver here" answer.
    fn look_up(name: &str) -> Result<MachPort> {
        if name.len() > BOOTSTRAP_MAX_NAME_LEN {
            anyhow::bail!("mach service name is {} bytes, max {BOOTSTRAP_MAX_NAME_LEN}", name.len());
        }
        let cname = CString::new(name)?;
        let mut bootstrap: MachPort = MACH_PORT_NULL;
        let kr = unsafe { task_get_special_port(task_self(), TASK_BOOTSTRAP_PORT, &mut bootstrap) };
        if kr != KERN_SUCCESS {
            anyhow::bail!("task_get_special_port(bootstrap) failed: {kr}");
        }
        let mut port: MachPort = MACH_PORT_NULL;
        let kr = unsafe { bootstrap_look_up(bootstrap, cname.as_ptr(), &mut port) };
        unsafe { mach_port_deallocate(task_self(), bootstrap) };
        match kr {
            BOOTSTRAP_SUCCESS if port != MACH_PORT_NULL => Ok(port),
            BOOTSTRAP_UNKNOWN_SERVICE => {
                anyhow::bail!("mach service '{name}' is not published")
            }
            _ => anyhow::bail!("bootstrap_look_up('{name}') failed: {kr}"),
        }
    }

    /// Receive buffer sized for the largest message plus the largest trailer.
    /// Aligned to 8 because `ControlMsg` carries a u64 at offset 48 and reading
    /// it out of a 1-aligned `[u8; N]` would be undefined behaviour.
    ///
    /// v2 SIZING, MEASURED. The reply this buffer has to hold went from 104 to
    /// 472 bytes. `a_reply_that_does_not_fit_is_destroyed_rather_than_queued`
    /// walks the receive size up one byte at a time against a real 472-byte
    /// 32-descriptor message and finds the kernel refuses everything below 480
    /// (472 + an 8-byte format-0 trailer) — and, because `MACH_RCV_LARGE` is
    /// deliberately not requested, DESTROYS the message rather than leaving it
    /// queued. The old 256 would therefore not have truncated a handshake, it
    /// would have made every handshake time out with no diagnostic at either
    /// end. 640 is 540 (472 + `MAX_TRAILER`) rounded up.
    const MAX_TRAILER: usize = 68;
    const RCV_BUF: usize = 640;

    #[repr(C, align(8))]
    struct MsgBuf([u8; RCV_BUF]);

    impl MsgBuf {
        fn new() -> MsgBuf {
            MsgBuf([0u8; RCV_BUF])
        }
        fn header(&mut self) -> *mut MsgHeader {
            self.0.as_mut_ptr() as *mut MsgHeader
        }
        fn zero(&mut self) {
            self.0.fill(0);
        }
    }

    /// Owns a RECEIVE right for as long as it lives. `mach_port_deallocate`
    /// refuses a receive right, hence `mod_refs`.
    struct PortGuard(MachPort);
    impl Drop for PortGuard {
        fn drop(&mut self) {
            unsafe { mach_port_mod_refs(task_self(), self.0, MACH_PORT_RIGHT_RECEIVE, -1) };
        }
    }

    /// The whole connect-retry policy, deliberately kept free of mach so it can
    /// be driven by a fake clock in a unit test.
    struct RetryPlan {
        backoff: Duration,
        next_attempt: Instant,
    }

    impl RetryPlan {
        fn ready_now(now: Instant) -> RetryPlan {
            RetryPlan { backoff: RETRY_MIN, next_attempt: now }
        }
        fn due(&self, now: Instant) -> bool {
            now >= self.next_attempt
        }
        /// Doubling, capped. Called after every failed attempt, which is what
        /// keeps a machine whose coreaudiod is wedged from being polled 5x a
        /// second forever.
        fn failed(&mut self, now: Instant) {
            self.next_attempt = now + self.backoff;
            self.backoff = (self.backoff * 2).min(RETRY_MAX);
        }
        /// A connection resets the ladder: the next outage should recover fast.
        fn succeeded(&mut self) {
            self.backoff = RETRY_MIN;
        }
    }

    fn service_loop(
        shared: Arc<Shared>,
        service_name: String,
        control: MachPort,
        first_port: Option<MachPort>,
    ) {
        let _guard = PortGuard(control);
        let mut buf = MsgBuf::new();
        // The two things that ever land in this buffer. A message that does not
        // fit is discarded by the kernel (no MACH_RCV_LARGE), which would turn
        // a handshake into a silent timeout. `BindMsg` is the same 472 bytes as
        // the reply but travels the other way, so it is the DRIVER's buffer
        // that has to hold it (AudioHubBridge.c's BridgeRcvBuf).
        const _: () = assert!(RCV_BUF >= std::mem::size_of::<HelloReply>() + MAX_TRAILER);
        const _: () = assert!(RCV_BUF >= std::mem::size_of::<ControlMsg>() + MAX_TRAILER);
        const _: () = assert!(RCV_BUF >= std::mem::size_of::<HelloRequest>() + MAX_TRAILER);
        // Held between the look-up and the handshake so a failed handshake does
        // not throw away a perfectly good send right.
        let mut pending = first_port;
        let mut retry = RetryPlan::ready_now(Instant::now());
        let mut next_ping = Instant::now() + PING_EVERY;
        let mut send_timeouts = 0u32;

        while !shared.stop.load(Ordering::SeqCst) {
            // The receive doubles as the loop's pacing: 200ms of waiting for a
            // driver message, then one cheap pass over the state machine.
            buf.zero();
            let kr = unsafe {
                mach_msg(
                    buf.header(),
                    MACH_RCV_MSG | MACH_RCV_TIMEOUT,
                    0,
                    RCV_BUF as u32,
                    control,
                    RECV_TIMEOUT_MS,
                    MACH_PORT_NULL,
                )
            };
            if kr == MACH_MSG_SUCCESS {
                let hdr = buf.header();
                if unsafe { (*hdr).id } == MSG_CONTROL {
                    handle_control(&shared, hdr);
                }
                // Every path disposes of the whole message: mach_msg_destroy is
                // the only disposal that cannot leak a right we did not expect.
                unsafe { mach_msg_destroy(hdr) };
            } else if kr != MACH_RCV_TIMED_OUT {
                // A malformed or oversized message must not spin this thread.
                std::thread::sleep(Duration::from_millis(50));
            }

            let now = Instant::now();
            // Another daemon owns the rings now. Detach immediately rather than
            // spending DRIVER_SILENT_AFTER producing into a ring a second
            // consumer is also draining, and take a backoff step so the two do
            // not trade the driver back and forth on every retry tick.
            if shared.superseded.swap(false, Ordering::AcqRel) {
                disconnect(&shared, "another daemon took over the driver");
                retry.failed(now);
                next_ping = now + PING_EVERY;
                send_timeouts = 0;
            }
            if shared.driver_connected.load(Ordering::Relaxed) {
                if now >= next_ping {
                    next_ping = now + PING_EVERY;
                    let (kr, port) = send_to_driver(&shared, NOTIFY_PING, 0, 0.0, 0, 0);
                    match kr {
                        MACH_MSG_SUCCESS => send_timeouts = 0,
                        MACH_SEND_TIMED_OUT => {
                            send_timeouts += 1;
                            if send_timeouts >= MAX_SEND_TIMEOUTS {
                                disconnect_port(&shared, port, "stopped draining its port");
                            }
                        }
                        _ => disconnect_port(&shared, port, "its mach port is dead"),
                    }
                }
                expire_silent_driver(&shared, now);
                continue;
            }

            if !retry.due(now) {
                continue;
            }
            let port = match pending.take() {
                Some(p) => p,
                None => match look_up(&service_name) {
                    Ok(p) => p,
                    Err(_) => {
                        // Silent: "not published" is the steady state on a Mac
                        // whose coreaudiod is between plug-in loads.
                        shared.driver_found.store(false, Ordering::Relaxed);
                        retry.failed(now);
                        continue;
                    }
                },
            };
            shared.driver_found.store(true, Ordering::Relaxed);
            match handshake(&shared, port, control) {
                Ok(()) => {
                    set_driver_port(&shared, port);
                    shared.driver_connected.store(true, Ordering::Relaxed);
                    send_timeouts = 0;
                    next_ping = Instant::now() + PING_EVERY;
                    retry.succeeded();
                    dlog!("[audiohubd] HAL driver attached; spk/mic rings handed over");
                }
                Err(e) => {
                    unsafe { mach_port_deallocate(task_self(), port) };
                    dlog!("[audiohubd] HAL handshake failed: {e:#}");
                    retry.failed(Instant::now());
                }
            }
        }
        // Shutdown path: unmap before the thread exits, while it is still the
        // only thing that could be attaching. `Shared` outlives this thread
        // (HalSpeakerSource holds an Arc), so leaving the mappings behind would
        // outlive the bridge itself.
        drop_driver_port(&shared);
        shared.rings.detach();
    }

    /// Installs the port of a freshly handshaked driver, releasing whatever was
    /// there. Nothing should be, but leaking a mach port because of a state
    /// machine bug is exactly the kind of leak nobody ever notices.
    fn set_driver_port(shared: &Shared, port: MachPort) {
        let mut g = lk(&shared.driver_port);
        if *g != MACH_PORT_NULL && *g != port {
            unsafe { mach_port_deallocate(task_self(), *g) };
        }
        *g = port;
    }

    fn drop_driver_port(shared: &Shared) {
        let mut g = lk(&shared.driver_port);
        if *g != MACH_PORT_NULL {
            unsafe { mach_port_deallocate(task_self(), *g) };
            *g = MACH_PORT_NULL;
        }
    }

    fn disconnect(shared: &Shared, why: &str) {
        disconnect_inner(shared, MACH_PORT_NULL, why);
    }

    /// Only ends the session that the failing send actually belonged to. Both
    /// senders run off-thread from the reconnect, so without the identity check
    /// a slow failure could tear down the connection that replaced it.
    fn disconnect_port(shared: &Shared, port: MachPort, why: &str) {
        if port == MACH_PORT_NULL {
            return;
        }
        disconnect_inner(shared, port, why);
    }

    /// `only_port` = MACH_PORT_NULL tears down whatever session is live;
    /// otherwise only the one that port belongs to.
    fn disconnect_inner(shared: &Shared, only_port: MachPort, why: &str) {
        // The identity check and the claim have to happen under ONE acquisition
        // of driver_port. Reading the port, then re-locking to clear it, leaves
        // a window in which a send failure that observed the OLD port resumes
        // after the service thread has already handshaked a new one — and tears
        // down the session that replaced it, which is the exact outcome the
        // identity check exists to prevent.
        {
            let mut g = lk(&shared.driver_port);
            if only_port != MACH_PORT_NULL && *g != only_port {
                return;
            }
            if !shared.driver_connected.swap(false, Ordering::AcqRel) {
                return;
            }
            if *g != MACH_PORT_NULL {
                unsafe { mach_port_deallocate(task_self(), *g) };
                *g = MACH_PORT_NULL;
            }
        }
        // Outside the lock: detach unmaps every ring and blocks until the tx
        // engine and the mixer are out of them, and it must not do that while
        // holding a lock the sending threads also take. From here read_spk
        // yields silence and write_mic reports "no ring" — driverless behaviour.
        shared.rings.detach();
        shared.slot_count.store(0, Ordering::Relaxed);
        shared.published.store(0, Ordering::Relaxed);
        // The BINDINGS survive on the driver's side (spec-m5b §4.4: disconnect
        // unpublishes the rings and keeps every binding), so the devices stay
        // in the system list and go silent. What does NOT survive is our right
        // to speak about a slot: the generations are re-established by the
        // BindState answers to the full re-Set the coordinator sends after the
        // next handshake.
        for c in shared.slots.iter() {
            c.generation.store(0, Ordering::Relaxed);
        }
        *lk(&shared.status_reason) = Some(format!("driver_gone: {why}"));
        shared.push_event(HalControlEvent::Detached);
        dlog!("[audiohubd] HAL driver gone ({why}); virtual devices are on their own");
    }

    fn expire_silent_driver(shared: &Shared, now: Instant) {
        let last = *lk(&shared.last_driver_msg);
        if let Some(t) = last {
            if now.duration_since(t) > DRIVER_SILENT_AFTER {
                disconnect(shared, "went silent");
            }
        }
    }

    /// One synchronous exchange on a reply port that exists only for it, so a
    /// stale reply from an earlier attempt can never be mistaken for this one.
    /// `control` is the loop's own receive right, from which the send right the
    /// driver keeps is manufactured.
    fn handshake(shared: &Shared, service: MachPort, control: MachPort) -> Result<()> {
        let reply = alloc_recv_port()?;
        let _reply_guard = PortGuard(reply);

        let mut buf = MsgBuf::new();
        // SAFETY: MsgBuf is 8-aligned and RCV_BUF > size_of::<HelloRequest>().
        let req = buf.0.as_mut_ptr() as *mut HelloRequest;
        unsafe {
            (*req) = HelloRequest {
                header: MsgHeader {
                    bits: msgh_bits(MACH_MSG_TYPE_COPY_SEND, MACH_MSG_TYPE_MAKE_SEND_ONCE)
                        | MACH_MSGH_BITS_COMPLEX,
                    size: std::mem::size_of::<HelloRequest>() as u32,
                    remote: service,
                    local: reply,
                    voucher: MACH_PORT_NULL,
                    id: MSG_HELLO,
                },
                // EXACTLY one. The driver rejects any other count outright and
                // its rejection path is `mach_msg_destroy`, which destroys the
                // send-once reply right without answering — so the kernel, not
                // the driver, sends the reply, and it is a bare 24-byte
                // MACH_NOTIFY_SEND_ONCE (id 0x47). That is what a descriptor
                // count of 3 looked like from this side for a whole debugging
                // session; if this ever reads 0x47 again, look here first.
                body: MsgBody { descriptor_count: 1 },
                // MAKE_SEND, not COPY_SEND: we hold the RECEIVE right and are
                // manufacturing a send right for the driver out of it.
                control_port: PortDescriptor {
                    name: control,
                    pad1: 0,
                    pad2: 0,
                    disposition: MACH_MSG_TYPE_MAKE_SEND as u8,
                    dtype: MACH_MSG_PORT_DESCRIPTOR,
                },
                protocol_version: PROTOCOL_VERSION,
                client_pid: std::process::id(),
            };
        }
        // Send and receive in one call: the canonical mach RPC, and the only
        // way the send-once right is consumed exactly once on every path.
        let kr = unsafe {
            mach_msg(
                buf.header(),
                MACH_SEND_MSG | MACH_RCV_MSG | MACH_SEND_TIMEOUT | MACH_RCV_TIMEOUT,
                std::mem::size_of::<HelloRequest>() as u32,
                RCV_BUF as u32,
                reply,
                HELLO_TIMEOUT_MS,
                MACH_PORT_NULL,
            )
        };
        if kr != MACH_MSG_SUCCESS {
            anyhow::bail!("hello exchange failed: {kr:#x}");
        }
        let hdr = buf.header();
        let id = unsafe { (*hdr).id };
        let size = unsafe { (*hdr).size } as usize;
        if id != MSG_HELLO_REPLY || size < std::mem::size_of::<HelloReply>() {
            unsafe { mach_msg_destroy(hdr) };
            // A v1 driver answers with the RIGHT id and the OLD size, so the
            // size check fires before anything reads `protocol_version` and the
            // careful "it speaks v1, we speak v2" message below is unreachable.
            // Left generic, this prints `unexpected reply id 0x41480002 size
            // 104` — accurate, and useless: it is the same shape as the
            // `0x47 size 24` line that once cost hours. An installed driver
            // that predates the protocol is the ONE cause a user can act on,
            // so it gets named here rather than diagnosed later.
            if id == MSG_HELLO_REPLY && size == HELLO_REPLY_V1_SIZE {
                // The UI's three-state driver hint keys off this exact string
                // (spec-m5b §6.2): "installed but talking a protocol this
                // daemon cannot parse" is a reinstall, not a transient.
                *lk(&shared.status_reason) = Some(REASON_PROTOCOL_MISMATCH.to_string());
                shared.driver_protocol.store(1, Ordering::Relaxed);
                anyhow::bail!(
                    "the installed HAL driver speaks protocol v1, this daemon speaks \
                     v{PROTOCOL_VERSION} — reinstall the driver \
                     (drivers/macos-hal/install.sh), the virtual devices stay silent until you do"
                );
            }
            anyhow::bail!(
                "unexpected reply id {id:#x} size {size} (expected id {MSG_HELLO_REPLY:#x} size {})",
                std::mem::size_of::<HelloReply>()
            );
        }
        // SAFETY: shape checked above; MsgBuf is 8-aligned.
        let rep = unsafe { *(hdr as *const HelloReply) };
        let complex = unsafe { (*hdr).bits } & MACH_MSGH_BITS_COMPLEX != 0;

        // ORDER MATTERS. A refusal is a PLAIN message whose descriptor words
        // are zero, so status and the COMPLEX bit are both read before either
        // entry name is looked at, let alone handed to mach_vm_map. Until the
        // names are proven received, `mach_msg_destroy` is the correct disposal
        // and it is a no-op on a plain message.
        if rep.status != STATUS_OK {
            unsafe { mach_msg_destroy(hdr) };
            shared.driver_protocol.store(rep.protocol_version, Ordering::Relaxed);
            let why = match rep.status {
                STATUS_BAD_VERSION => {
                    *lk(&shared.status_reason) = Some(REASON_PROTOCOL_MISMATCH.to_string());
                    format!(
                        "protocol mismatch: it speaks v{}, we speak v{PROTOCOL_VERSION}",
                        rep.protocol_version
                    )
                }
                STATUS_NO_MEMORY => {
                    *lk(&shared.status_reason) =
                        Some("driver_out_of_memory".to_string());
                    "it could not create the shared rings".to_string()
                }
                s => {
                    *lk(&shared.status_reason) = Some(format!("driver_refused_{s}"));
                    format!("status {s}")
                }
            };
            anyhow::bail!("driver refused the handshake: {why}");
        }
        // `slot_count` is checked BEFORE the descriptor count it predicts, so a
        // driver that claims sixteen slots and sends two descriptors is named
        // as such instead of being read as a two-slot driver. The reply is
        // fixed-length whatever the count: only `2 * slot_count` of the 32
        // descriptor words are populated and the rest are zero, so attaching
        // more than that would hand `mach_vm_map` a null name.
        if rep.slot_count == 0 || rep.slot_count as usize > HAL_MAX_SLOTS {
            unsafe { mach_msg_destroy(hdr) };
            anyhow::bail!(
                "driver reported OK with slot_count {} (this daemon supports 1..={HAL_MAX_SLOTS})",
                rep.slot_count
            );
        }
        if !complex || rep.body.descriptor_count != 2 * rep.slot_count {
            unsafe { mach_msg_destroy(hdr) };
            anyhow::bail!(
                "driver reported OK with slot_count {} but sent {} descriptors (complex={complex}); \
                 expected {}",
                rep.slot_count,
                rep.body.descriptor_count,
                2 * rep.slot_count
            );
        }

        // From here every entry name is ours, so disposal moves from
        // mach_msg_destroy to the RingMem that owns each one. EVERY attach is
        // ATTEMPTED before any error propagates: taking ownership of thirty
        // entries and returning early on the thirty-first would leak thirty
        // mach ports every time a mismatched driver retried, and the port name
        // space is the one resource coreaudiod cannot be restarted to reclaim.
        let n = rep.slot_count as usize;
        let mut attached: Vec<Result<RingPair>> = Vec::with_capacity(n);
        for slot in 0..n {
            let spk = RingMem::attach(
                rep.entries[endpoint(slot, DIR_OUT) as usize].name,
                RingGeom {
                    channels: rep.spk_channels,
                    sample_rate: rep.sample_rate,
                    capacity_frames: rep.spk_capacity_frames,
                    data_offset: rep.data_offset,
                    bytes: rep.spk_bytes,
                },
                HAL_SPK_CHANNELS,
                "speaker",
            );
            let mic = RingMem::attach(
                rep.entries[endpoint(slot, DIR_IN) as usize].name,
                RingGeom {
                    channels: rep.mic_channels,
                    sample_rate: rep.sample_rate,
                    capacity_frames: rep.mic_capacity_frames,
                    data_offset: rep.data_offset,
                    bytes: rep.mic_bytes,
                },
                HAL_MIC_CHANNELS,
                "microphone",
            );
            attached.push(match (spk, mic) {
                (Ok(spk), Ok(mic)) => Ok(RingPair { spk, mic }),
                (Err(e), _) | (Ok(_), Err(e)) => {
                    Err(e.context(format!("slot {slot}")))
                }
            });
        }
        let mut pairs = Vec::with_capacity(n);
        let mut first_err: Option<anyhow::Error> = None;
        for r in attached {
            match r {
                Ok(p) => pairs.push(p),
                Err(e) => {
                    first_err.get_or_insert(e);
                }
            }
        }
        if let Some(e) = first_err {
            // `pairs` unmaps here: a partial attach is not a bridge, and
            // leaving half a driver mapped would make the next handshake's
            // rings disagree with the slots the coordinator then binds.
            return Err(e);
        }

        // The driver rewound every ring before it replied and publishes them
        // only after, so nothing stale can be in there. The flush is still
        // armed for the gap between here and the tx engine's first read, which
        // is unbounded — a session opened a minute later must start at live
        // audio, not half a second behind it. Only the consumer may move
        // read_idx, hence a flag for the reading thread rather than a call.
        // Attach BEFORE arming the flush. The other order has a window in which
        // a tx tick clears the bit and flushes a ring that is still None — the
        // bit is consumed, the backlog is never dropped, and the session keeps
        // the extra latency the flush exists to remove. This way at worst one
        // tick reads before the flush and the next one performs it.
        shared.rings.attach(pairs);
        shared.spk_flush.store(u16::MAX, Ordering::Release);
        shared.slot_count.store(rep.slot_count, Ordering::Relaxed);
        shared.session_id.store(rep.session_id, Ordering::Relaxed);
        shared.driver_protocol.store(rep.protocol_version, Ordering::Relaxed);
        *lk(&shared.status_reason) = None;
        // Bumped LAST: a coordinator that sees the new epoch must find a
        // session id and rings it can actually use in the same instant.
        shared.attach_epoch.fetch_add(1, Ordering::AcqRel);
        shared.push_event(HalControlEvent::Attached {
            session_id: rep.session_id,
            slot_count: rep.slot_count as u8,
        });
        *lk(&shared.last_driver_msg) = None;
        Ok(())
    }

    fn handle_control(shared: &Shared, hdr: *mut MsgHeader) {
        // A complex message puts a descriptor array where `op` is meant to be,
        // so anything carrying rights is not the control message we mirror —
        // whatever it is, the caller's mach_msg_destroy will dispose of it.
        if unsafe { (*hdr).bits } & MACH_MSGH_BITS_COMPLEX != 0 {
            return;
        }
        if unsafe { (*hdr).size } < std::mem::size_of::<ControlMsg>() as u32 {
            return;
        }
        // SAFETY: MsgBuf is 8-aligned, so the u64 at offset 40 is aligned.
        let msg = unsafe { *(hdr as *const ControlMsg) };
        // Deliberately BEFORE last_driver_msg: this message means the session is
        // already over, so counting it as liveness would keep the silence timer
        // from ever noticing that the port under us is about to be deallocated.
        if msg.op == CTL_SUPERSEDED {
            shared.superseded.store(true, Ordering::Release);
            return;
        }
        *lk(&shared.last_driver_msg) = Some(Instant::now());
        match msg.op {
            // BindState is the ONE message that is not generation-filtered:
            // it is what ESTABLISHES the generation everything else is then
            // filtered against (spec-m5b §4.6).
            CTL_BIND_STATE => {
                let slot = endpoint_slot(msg.endpoint);
                let Some(state) = HalSlotState::from_wire(msg.scalar_bits) else {
                    dlog!(
                        "[audiohubd] hal: slot {slot} reported unknown state {}",
                        msg.scalar_bits
                    );
                    return;
                };
                let Some(c) = shared.slots.get(slot) else { return };
                let prev = c.generation.swap(msg.generation, Ordering::AcqRel);
                if prev != msg.generation {
                    // The slot was reused (or first bound). The driver resets a
                    // ring only while it is unpublished and cannot know where
                    // OUR read_idx stands, so a consumer that does not jump to
                    // write_idx here computes a half-second backlog out of a
                    // wrapping subtraction and replays the PREVIOUS peer's
                    // audio to the new one (spec-m5b §4.6, regression N16).
                    shared.arm_flush(slot as u8);
                }
                shared.push_event(HalControlEvent::BindState {
                    slot: slot as u8,
                    generation: msg.generation,
                    state,
                });
            }
            CTL_VOLUME | CTL_IO_STATE => {
                let Some(at) = HalEndpoint::from_wire(msg.endpoint) else { return };
                let Some(c) = shared.slots.get(at.slot as usize) else { return };
                let want = c.generation.load(Ordering::Acquire);
                if want == 0 || msg.generation != want {
                    // A late StopIO from the slot's previous tenant, or a
                    // volume event that crossed a re-bind. Applying it would
                    // light up the NEXT peer's microphone indicator out of
                    // nowhere, or move a stranger's volume.
                    dlog!(
                        "[audiohubd] hal: dropping op {} for slot {} at generation {} (current {want})",
                        msg.op,
                        at.slot,
                        msg.generation
                    );
                    return;
                }
                if msg.op == CTL_VOLUME {
                    let scalar = f32::from_bits(msg.scalar_bits);
                    if !scalar.is_finite() {
                        return;
                    }
                    shared.push_event(HalControlEvent::Volume {
                        at,
                        generation: msg.generation,
                        scalar: scalar.clamp(0.0, 1.0),
                        muted: msg.flags & FLAG_MUTED != 0,
                    });
                } else {
                    shared.push_event(HalControlEvent::IoState {
                        at,
                        generation: msg.generation,
                        running: msg.flags & FLAG_IO_RUNNING != 0,
                    });
                }
            }
            CTL_HEARTBEAT => {}
            _ => {}
        }
    }

    /// Fire-and-forget to the driver's service port. Returns the raw kern
    /// return so the caller can tell "queue full" from "port is dead", plus the
    /// port it was sent on so a failure can be attributed to the right session.
    fn send_to_driver(
        shared: &Shared,
        op: u32,
        endpoint: u32,
        scalar: f32,
        flags: u32,
        generation: u32,
    ) -> (KernReturn, MachPort) {
        let g = lk(&shared.driver_port);
        if *g == MACH_PORT_NULL {
            return (MACH_SEND_INVALID_DEST, MACH_PORT_NULL);
        }
        let port = *g;
        let mut msg = ControlMsg {
            header: MsgHeader {
                bits: msgh_bits(MACH_MSG_TYPE_COPY_SEND, 0),
                size: std::mem::size_of::<ControlMsg>() as u32,
                remote: port,
                local: MACH_PORT_NULL,
                voucher: MACH_PORT_NULL,
                id: MSG_NOTIFY,
            },
            op,
            endpoint,
            scalar_bits: scalar.to_bits(),
            flags,
            // 0 is the wire's "concerns no slot", which is what a Ping is; a
            // Volume notify carries the slot's current stamp so the driver can
            // refuse one that crossed a re-bind (spec-m5b §4.6).
            generation,
            reserved: 0,
            seq: 0,
        };
        let kr = unsafe {
            mach_msg(
                &mut msg.header,
                MACH_SEND_MSG | MACH_SEND_TIMEOUT,
                std::mem::size_of::<ControlMsg>() as u32,
                0,
                MACH_PORT_NULL,
                SEND_TIMEOUT_MS,
                MACH_PORT_NULL,
            )
        };
        drop(g);
        (kr, port)
    }

    pub fn send_notify(
        shared: &Shared,
        at: HalEndpoint,
        generation: u32,
        scalar: f32,
        muted: bool,
    ) {
        let (kr, port) = send_to_driver(
            shared,
            NOTIFY_VOLUME,
            at.to_wire(),
            scalar.clamp(0.0, 1.0),
            if muted { FLAG_MUTED } else { 0 },
            generation,
        );
        // A dead port is definitive; a full queue is not. Only the former ends
        // the session, and the service loop then goes back to looking up.
        if kr != MACH_MSG_SUCCESS && kr != MACH_SEND_TIMED_OUT {
            disconnect_port(shared, port, "volume relay found its port dead");
        }
    }

    /// Copies `s` into a fixed `char[N]` and guarantees the NUL. The driver
    /// terminates what it receives too (spec-m5b §4.5: neither end trusts the
    /// sender's terminator) — this is the sending half of the same rule, and it
    /// truncates on a CHARACTER boundary so a multi-byte name can never arrive
    /// as invalid UTF-8, which the driver rejects the whole message over.
    fn fixed_cstr<const N: usize>(s: &str) -> [u8; N] {
        let mut out = [0u8; N];
        let mut end = s.len().min(N - 1);
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        out[..end].copy_from_slice(&s.as_bytes()[..end]);
        out
    }

    fn send_bind(shared: &Shared, mut msg: BindMsg) -> bool {
        let g = lk(&shared.driver_port);
        if *g == MACH_PORT_NULL {
            return false;
        }
        let port = *g;
        msg.header = MsgHeader {
            bits: msgh_bits(MACH_MSG_TYPE_COPY_SEND, 0),
            size: std::mem::size_of::<BindMsg>() as u32,
            remote: port,
            local: MACH_PORT_NULL,
            voucher: MACH_PORT_NULL,
            id: MSG_BIND,
        };
        msg.session_id = shared.session_id.load(Ordering::Relaxed);
        let kr = unsafe {
            mach_msg(
                &mut msg.header,
                MACH_SEND_MSG | MACH_SEND_TIMEOUT,
                std::mem::size_of::<BindMsg>() as u32,
                0,
                MACH_PORT_NULL,
                SEND_TIMEOUT_MS,
                MACH_PORT_NULL,
            )
        };
        drop(g);
        if kr != MACH_MSG_SUCCESS && kr != MACH_SEND_TIMED_OUT {
            disconnect_port(shared, port, "bind found its port dead");
            return false;
        }
        // A full queue is NOT success: the coordinator is closed-loop and will
        // re-send on its next tick, which is exactly the recovery a dropped
        // Bind needs.
        kr == MACH_MSG_SUCCESS
    }

    pub fn send_bind_set(shared: &Shared, req: &HalBindRequest) -> bool {
        send_bind(
            shared,
            BindMsg {
                header: MsgHeader::default(),
                op: BIND_SET,
                slot: req.slot as u32,
                flags: if req.online { 1 } else { 0 },
                // Set carries 0: the DRIVER allocates the generation and
                // reports it back in the BindState this message provokes.
                generation: 0,
                session_id: 0, // stamped by send_bind under the port lock
                peer_key: fixed_cstr(&req.peer_key),
                out_uid: fixed_cstr(&req.out_uid),
                in_uid: fixed_cstr(&req.in_uid),
                out_name: fixed_cstr(&req.out_name),
                in_name: fixed_cstr(&req.in_name),
            },
        )
    }

    pub fn send_bind_clear(shared: &Shared, slot: u8, generation: u32) -> bool {
        send_bind(
            shared,
            BindMsg {
                header: MsgHeader::default(),
                op: BIND_CLEAR,
                slot: slot as u32,
                flags: 0,
                generation,
                session_id: 0,
                peer_key: [0; 40],
                out_uid: [0; 64],
                in_uid: [0; 64],
                out_name: [0; 128],
                in_name: [0; 128],
            },
        )
    }

    // Every expectation below is the OBSERVED output of the C implementation in
    // drivers/macos-hal/src/AudioHubBridge.h run over the same scenario. They
    // are what stops the two ends of a shared buffer from drifting apart in a
    // way that only shows up as noise on someone's speakers.
    #[cfg(test)]
    mod tests {
        use super::*;

        /// Stands in for the driver: creates a ring the way AudioHubBridge.c's
        /// `bridge_ring_create` does — a MAP_MEM_NAMED_CREATE object, mapped
        /// shared, header stamped — and hands back the entry port the reply
        /// would carry. Everything below then goes through the REAL
        /// `RingMem::attach`, so these tests exercise the production mapping
        /// path rather than a parallel one that could drift from it.
        struct FakeDriverRing {
            entry: MachPort,
            addr: u64,
            bytes: usize,
        }

        impl FakeDriverRing {
            fn new(channels: u32, bytes: usize) -> FakeDriverRing {
                let mut size = bytes as u64;
                let mut entry: MachPort = MACH_PORT_NULL;
                let kr = unsafe {
                    mach_make_memory_entry_64(
                        task_self(),
                        &mut size,
                        0,
                        MAP_MEM_NAMED_CREATE | VM_PROT_READ | VM_PROT_WRITE,
                        &mut entry,
                        MACH_PORT_NULL,
                    )
                };
                assert_eq!(kr, KERN_SUCCESS, "mach_make_memory_entry_64({bytes})");
                assert!(entry != MACH_PORT_NULL && size >= bytes as u64);
                let addr = map_entry(entry, bytes).expect("driver maps its own ring");
                let hdr = addr as *mut RingHeader;
                unsafe {
                    (*hdr).magic = HAL_RING_MAGIC;
                    (*hdr).version = HAL_RING_VERSION;
                    (*hdr).sample_rate = HAL_SAMPLE_RATE;
                    (*hdr).channels = channels;
                    (*hdr).capacity_frames = HAL_RING_FRAMES;
                    (*hdr).reserved = 0;
                }
                FakeDriverRing { entry, addr, bytes }
            }

            fn geom(&self, channels: u32) -> RingGeom {
                RingGeom {
                    channels,
                    sample_rate: HAL_SAMPLE_RATE,
                    capacity_frames: HAL_RING_FRAMES,
                    data_offset: HAL_RING_DATA_OFFSET as u32,
                    bytes: self.bytes as u64,
                }
            }

            /// A second send right, so the daemon's `attach` can consume one
            /// without taking the "driver's" away — exactly what COPY_SEND on
            /// the reply descriptor does on the wire.
            fn entry_copy(&self) -> MachPort {
                let kr = unsafe {
                    mach_port_mod_refs(task_self(), self.entry, MACH_PORT_RIGHT_SEND, 1)
                };
                assert_eq!(kr, KERN_SUCCESS, "duplicate the entry send right");
                self.entry
            }

            fn hdr(&self) -> &RingHeader {
                unsafe { &*(self.addr as *const RingHeader) }
            }
        }

        impl Drop for FakeDriverRing {
            fn drop(&mut self) {
                unsafe {
                    mach_vm_deallocate(task_self(), self.addr, self.bytes as u64);
                    mach_port_deallocate(task_self(), self.entry);
                }
            }
        }

        /// The daemon's side of a ring the fake driver just created.
        fn attach_ring(d: &FakeDriverRing, channels: u32) -> RingMem {
            RingMem::attach(d.entry_copy(), d.geom(channels), channels, "test")
                .expect("attach a well-formed ring")
        }

        fn spk_ring() -> (FakeDriverRing, RingMem) {
            let d = FakeDriverRing::new(HAL_SPK_CHANNELS, HAL_SPK_BYTES);
            let m = attach_ring(&d, HAL_SPK_CHANNELS);
            (d, m)
        }

        fn mic_ring() -> (FakeDriverRing, RingMem) {
            let d = FakeDriverRing::new(HAL_MIC_CHANNELS, HAL_MIC_BYTES);
            let m = attach_ring(&d, HAL_MIC_CHANNELS);
            (d, m)
        }

        /// Rings as the service thread would install them after a handshake.
        fn attached_rings() -> (FakeDriverRing, FakeDriverRing, Rings) {
            let ds = FakeDriverRing::new(HAL_SPK_CHANNELS, HAL_SPK_BYTES);
            let dm = FakeDriverRing::new(HAL_MIC_CHANNELS, HAL_MIC_BYTES);
            let r = Rings::new();
            // A one-slot set: every per-slot assertion below is about slot 0,
            // and the multi-slot fan-out is `rings_route_each_slot_to_its_own_pair`.
            r.attach(vec![RingPair {
                spk: attach_ring(&ds, HAL_SPK_CHANNELS),
                mic: attach_ring(&dm, HAL_MIC_CHANNELS),
            }]);
            (ds, dm, r)
        }

        fn set_idx(m: &RingMem, w: u64, r: u64) {
            m.hdr().write_idx.store(w, Ordering::Relaxed);
            m.hdr().read_idx.store(r, Ordering::Relaxed);
        }

        #[test]
        fn sizes_match_the_driver_header() {
            assert_eq!(HAL_SPK_BYTES, 196_608);
            assert_eq!(HAL_MIC_BYTES, 98_304);
            assert_eq!(HAL_RING_FRAMES, 24_000);
            assert_eq!(HAL_RING_DATA_OFFSET, 64);
            assert_eq!(std::mem::size_of::<RingHeader>(), 40);

            // v2 message sizes (spec-m5b §4.2). The `const _` block beside the
            // struct definitions asserts these at compile time; restating them
            // here is what makes a deliberate contract change show up as a
            // NAMED test failure rather than as a wall of const-eval errors.
            assert_eq!(PROTOCOL_VERSION, 2);
            assert_eq!(std::mem::size_of::<HelloRequest>(), 48);
            assert_eq!(std::mem::size_of::<HelloReply>(), 472);
            assert_eq!(std::mem::size_of::<ControlMsg>(), 56);
            assert_eq!(std::mem::size_of::<BindMsg>(), 472);
            assert_eq!(HAL_MAX_SLOTS, 16);
            assert_eq!(HAL_MAX_ENDPOINTS, 32);
        }

        // ---- v2 wire round trips ------------------------------------------
        //
        // One round trip per message shape (spec-m5b §7 step 2). Each one does
        // TWO things, and the second is the one that matters: it reads every
        // field back at the ABSOLUTE byte offset AudioHubBridge.h names for it.
        // A pure encode/decode round trip only proves this file agrees with
        // itself — a struct with a padding word the C side does not have would
        // pass it and still put `session_id` four bytes off, which is a
        // disagreement neither side's build can see alone. Every field also
        // gets a distinct sentinel, so two swapped fields of the same width are
        // a failure rather than a coincidence.

        /// A message as `mach_msg` leaves it in a receive buffer: an 8-aligned
        /// byte blob with no Rust type over it.
        struct Wire(Vec<u64>);

        impl Wire {
            fn of<T: Copy>(v: &T) -> Wire {
                let mut w = Wire(vec![0u64; std::mem::size_of::<T>().div_ceil(8)]);
                // SAFETY: the destination is 8-aligned and at least
                // size_of::<T>() bytes; T is Copy and repr(C), so these are the
                // bytes the kernel would carry.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        v as *const T as *const u8,
                        w.0.as_mut_ptr() as *mut u8,
                        std::mem::size_of::<T>(),
                    );
                }
                w
            }
            /// The same pointer cast `handshake` and `handle_control` use.
            fn back<T: Copy>(&self) -> T {
                unsafe { *(self.0.as_ptr() as *const T) }
            }
            fn at(&self, off: usize) -> *const u8 {
                unsafe { (self.0.as_ptr() as *const u8).add(off) }
            }
            fn u32_at(&self, off: usize) -> u32 {
                unsafe { *(self.at(off) as *const u32) }
            }
            fn u64_at(&self, off: usize) -> u64 {
                unsafe { *(self.at(off) as *const u64) }
            }
            fn bytes_at(&self, off: usize, len: usize) -> &[u8] {
                unsafe { std::slice::from_raw_parts(self.at(off), len) }
            }
        }

        /// A `char[N]` field: copied in, NOT terminated by us. Both ends
        /// terminate what they receive themselves (spec-m5b §4.5), so a test
        /// helper that quietly guaranteed a NUL would be testing a promise the
        /// wire does not make.
        fn fixed<const N: usize>(s: &str) -> [u8; N] {
            let mut out = [0u8; N];
            let n = s.len().min(N);
            out[..n].copy_from_slice(&s.as_bytes()[..n]);
            out
        }

        #[test]
        fn a_hello_reply_round_trips_at_the_offsets_the_header_names() {
            let mut sent = HelloReply {
                header: MsgHeader {
                    bits: msgh_bits(MACH_MSG_TYPE_COPY_SEND, 0) | MACH_MSGH_BITS_COMPLEX,
                    size: std::mem::size_of::<HelloReply>() as u32,
                    remote: 0x0101,
                    local: MACH_PORT_NULL,
                    voucher: MACH_PORT_NULL,
                    id: MSG_HELLO_REPLY,
                },
                body: MsgBody { descriptor_count: HAL_MAX_ENDPOINTS as u32 },
                entries: [PortDescriptor::default(); HAL_MAX_ENDPOINTS],
                status: STATUS_OK,
                protocol_version: PROTOCOL_VERSION,
                slot_count: HAL_MAX_SLOTS as u32,
                data_offset: HAL_RING_DATA_OFFSET as u32,
                spk_capacity_frames: HAL_RING_FRAMES,
                spk_channels: HAL_SPK_CHANNELS,
                mic_capacity_frames: HAL_RING_FRAMES,
                mic_channels: HAL_MIC_CHANNELS,
                sample_rate: HAL_SAMPLE_RATE,
                session_id: 0x0102_0304_0506_0708,
                spk_bytes: HAL_SPK_BYTES as u64,
                mic_bytes: HAL_MIC_BYTES as u64,
            };
            // A distinct name per endpoint: an array that arrived reordered, or
            // collapsed onto one descriptor, has to be visible here. 0x2000+i
            // rather than i so a zeroed slot cannot pass as slot 0.
            for (i, e) in sent.entries.iter_mut().enumerate() {
                *e = PortDescriptor {
                    name: 0x2000 + i as u32,
                    pad1: 0,
                    pad2: 0,
                    disposition: MACH_MSG_TYPE_COPY_SEND as u8,
                    dtype: MACH_MSG_PORT_DESCRIPTOR,
                };
            }

            let wire = Wire::of(&sent);
            let got: HelloReply = wire.back();

            assert_eq!(got.header.bits, sent.header.bits);
            assert_eq!(got.header.size, 472);
            assert_eq!(got.header.remote, sent.header.remote);
            assert_eq!(got.header.id, MSG_HELLO_REPLY);
            assert_eq!(got.body.descriptor_count, 32);
            for i in 0..HAL_MAX_ENDPOINTS {
                assert_eq!(got.entries[i].name, 0x2000 + i as u32, "entry {i} moved");
                assert_eq!(got.entries[i].disposition, MACH_MSG_TYPE_COPY_SEND as u8);
                assert_eq!(got.entries[i].dtype, MACH_MSG_PORT_DESCRIPTOR);
            }
            assert_eq!(got.status, STATUS_OK);
            assert_eq!(got.protocol_version, 2);
            assert_eq!(got.slot_count, 16);
            assert_eq!(got.data_offset, 64);
            assert_eq!(got.spk_capacity_frames, HAL_RING_FRAMES);
            assert_eq!(got.spk_channels, HAL_SPK_CHANNELS);
            assert_eq!(got.mic_capacity_frames, HAL_RING_FRAMES);
            assert_eq!(got.mic_channels, HAL_MIC_CHANNELS);
            assert_eq!(got.sample_rate, HAL_SAMPLE_RATE);
            assert_eq!(got.session_id, 0x0102_0304_0506_0708);
            assert_eq!(got.spk_bytes, HAL_SPK_BYTES as u64);
            assert_eq!(got.mic_bytes, HAL_MIC_BYTES as u64);

            // AudioHubBridge.h's _Static_asserts, read off the wire.
            assert_eq!(wire.u32_at(24), 32, "body.descriptor_count @24");
            assert_eq!(wire.u32_at(28), 0x2000, "entries @28");
            assert_eq!(wire.u32_at(28 + 31 * 12), 0x2000 + 31, "the last entry ends at 412");
            assert_eq!(wire.u32_at(412), STATUS_OK, "status @412");
            assert_eq!(wire.u32_at(416), 2, "protocol_version @416");
            assert_eq!(wire.u32_at(420), 16, "slot_count @420");
            assert_eq!(wire.u32_at(424), 64, "data_offset @424");
            assert_eq!(wire.u32_at(444), HAL_SAMPLE_RATE, "sample_rate @444");
            // The padding trap: nine u32 from 412 land session_id on 448. Ten
            // or eight would put four bytes of compiler padding here and every
            // number below would be read from the wrong place.
            assert_eq!(wire.u64_at(448), 0x0102_0304_0506_0708, "session_id @448");
            assert_eq!(wire.u64_at(456), HAL_SPK_BYTES as u64, "spk_bytes @456");
            assert_eq!(wire.u64_at(464), HAL_MIC_BYTES as u64, "mic_bytes @464");
        }

        #[test]
        fn a_control_message_round_trips_at_the_offsets_the_header_names() {
            // The driver->daemon direction (Control) and the daemon->driver one
            // (Notify) are the SAME 56 bytes with a different msgh_id, so both
            // ids go through the identical layout check here.
            for (id, op) in [(MSG_CONTROL, CTL_IO_STATE), (MSG_NOTIFY, NOTIFY_VOLUME)] {
                let sent = ControlMsg {
                    header: MsgHeader {
                        bits: msgh_bits(MACH_MSG_TYPE_COPY_SEND, 0),
                        size: std::mem::size_of::<ControlMsg>() as u32,
                        remote: 0x0303,
                        local: MACH_PORT_NULL,
                        voucher: MACH_PORT_NULL,
                        id,
                    },
                    op,
                    // Slot 7's input, i.e. 15 — a value that is neither 0 nor 1,
                    // so a receiver still reading a one-bit device selector
                    // cannot accidentally agree with us.
                    endpoint: endpoint(7, DIR_IN),
                    scalar_bits: 0.75f32.to_bits(),
                    flags: FLAG_MUTED | FLAG_IO_RUNNING | FLAG_IS_INPUT,
                    generation: 0xABCD_1234,
                    reserved: 0,
                    seq: 0x1122_3344_5566_7788,
                };

                let wire = Wire::of(&sent);
                let got: ControlMsg = wire.back();

                assert_eq!(got.header.id, id);
                assert_eq!(got.header.size, 56);
                assert_eq!(got.op, op);
                assert_eq!(got.endpoint, 15);
                assert_eq!(endpoint_slot(got.endpoint), 7);
                assert_eq!(endpoint_dir(got.endpoint), DIR_IN);
                assert_eq!(f32::from_bits(got.scalar_bits), 0.75);
                assert_eq!(got.flags, 0x7);
                assert_eq!(got.generation, 0xABCD_1234);
                assert_eq!(got.reserved, 0);
                assert_eq!(got.seq, 0x1122_3344_5566_7788);

                assert_eq!(wire.u32_at(24), op, "op @24");
                assert_eq!(wire.u32_at(28), 15, "endpoint @28");
                assert_eq!(wire.u32_at(32), 0.75f32.to_bits(), "scalar_bits @32");
                assert_eq!(wire.u32_at(36), 0x7, "flags @36");
                assert_eq!(wire.u32_at(40), 0xABCD_1234, "generation @40");
                assert_eq!(wire.u32_at(44), 0, "reserved @44 must be MBZ");
                // v1 read `seq` at 40. Two new u32 moved it to 48, and this is
                // the assertion that says so out loud.
                assert_eq!(wire.u64_at(48), 0x1122_3344_5566_7788, "seq @48");
            }
        }

        #[test]
        fn a_bind_message_round_trips_at_the_offsets_the_header_names() {
            let sent = BindMsg {
                header: MsgHeader {
                    bits: msgh_bits(MACH_MSG_TYPE_COPY_SEND, 0),
                    size: std::mem::size_of::<BindMsg>() as u32,
                    remote: 0x0404,
                    local: MACH_PORT_NULL,
                    voucher: MACH_PORT_NULL,
                    id: MSG_BIND,
                },
                op: BIND_SET,
                slot: 9,
                flags: 0x1,
                generation: 0,
                session_id: 0x0A0B_0C0D_0E0F_1011,
                peer_key: fixed("fp-0123456789abcdef"),
                out_uid: fixed("AudioHub:fp-0123456789abcdef:out"),
                in_uid: fixed("AudioHub:fp-0123456789abcdef:in"),
                out_name: fixed("Living Room Mac Speaker"),
                in_name: fixed("Living Room Mac Microphone"),
            };

            let wire = Wire::of(&sent);
            let got: BindMsg = wire.back();

            assert_eq!(got.header.id, MSG_BIND);
            assert_eq!(got.header.size, 472);
            assert_eq!(got.op, BIND_SET);
            assert_eq!(got.slot, 9);
            assert_eq!(got.flags, 0x1);
            assert_eq!(got.generation, 0);
            assert_eq!(got.session_id, 0x0A0B_0C0D_0E0F_1011);
            assert_eq!(got.peer_key, sent.peer_key);
            assert_eq!(got.out_uid, sent.out_uid);
            assert_eq!(got.in_uid, sent.in_uid);
            assert_eq!(got.out_name, sent.out_name);
            assert_eq!(got.in_name, sent.in_name);

            assert_eq!(wire.u32_at(24), BIND_SET, "op @24");
            assert_eq!(wire.u32_at(28), 9, "slot @28");
            assert_eq!(wire.u32_at(32), 0x1, "flags @32");
            assert_eq!(wire.u32_at(36), 0, "generation @36");
            assert_eq!(wire.u64_at(40), 0x0A0B_0C0D_0E0F_1011, "session_id @40");
            // The five char arrays, each read at its own offset: a size that is
            // right while two of them are transposed is exactly the drift the
            // absolute offsets exist to catch, and the uids are the field the
            // driver turns into a system device UID.
            assert_eq!(wire.bytes_at(48, 40), &sent.peer_key[..], "peer_key @48");
            assert_eq!(wire.bytes_at(88, 64), &sent.out_uid[..], "out_uid @88");
            assert_eq!(wire.bytes_at(152, 64), &sent.in_uid[..], "in_uid @152");
            assert_eq!(wire.bytes_at(216, 128), &sent.out_name[..], "out_name @216");
            assert_eq!(wire.bytes_at(344, 128), &sent.in_name[..], "in_name @344");
            assert_eq!(
                std::mem::size_of::<BindMsg>(),
                344 + 128,
                "in_name is the last field; anything after it is padding the C side lacks"
            );

            // Unterminated on purpose: `out_uid` fills 32 of 64 bytes here, and
            // a sender that filled all 64 would arrive without a NUL at all.
            // That is why the receiving side terminates rather than trusting.
            assert_eq!(&got.out_uid[..32], b"AudioHub:fp-0123456789abcdef:out");
            assert_eq!(got.out_uid[32], 0);
        }

        #[test]
        fn write_and_read_wrap_around_the_end() {
            let (_d, m) = spk_ring();
            set_idx(&m, 23_998, 23_998);
            let mut input = [0.0f32; 10];
            for i in 0..5 {
                input[i * 2] = 100.0 + i as f32;
                input[i * 2 + 1] = 200.0 + i as f32;
            }
            assert_eq!(m.write(&input, 5), 5);
            let mut out = [0.0f32; 10];
            assert_eq!(m.read(&mut out, 5), 5);
            assert_eq!(out, [100.0, 200.0, 101.0, 201.0, 102.0, 202.0, 103.0, 203.0, 104.0, 204.0]);
            assert_eq!(m.hdr().write_idx.load(Ordering::Relaxed), 24_003);
            assert_eq!(m.hdr().read_idx.load(Ordering::Relaxed), 24_003);
            // the physical split: 2 frames at the tail, 3 wrapped to the front
            let d = m.data();
            let at = |i: usize| unsafe { *d.add(i) };
            assert_eq!((at(23_998 * 2), at(23_998 * 2 + 1)), (100.0, 200.0));
            assert_eq!((at(23_999 * 2), at(23_999 * 2 + 1)), (101.0, 201.0));
            assert_eq!((at(0), at(1), at(2), at(3)), (102.0, 202.0, 103.0, 203.0));
        }

        #[test]
        fn a_full_ring_drops_the_tail_instead_of_blocking() {
            let (_d, m) = mic_ring();
            set_idx(&m, 23_997, 0);
            let mono: Vec<f32> = (1..=10).map(|i| i as f32).collect();
            assert_eq!(m.write(&mono, mono.len()), 3);
            assert_eq!(m.hdr().write_idx.load(Ordering::Relaxed), 24_000);
        }

        #[test]
        fn a_consumer_more_than_a_buffer_behind_skips_ahead() {
            let (_drv, m) = mic_ring();
            let d = m.data();
            for i in 0..HAL_RING_FRAMES as usize {
                unsafe { *d.add(i) = i as f32 };
            }
            set_idx(&m, 30_000, 0);
            let mut out = [0.0f32; 4];
            assert_eq!(m.read(&mut out, 4), 4);
            // 30000 - 24000 = 6000: the oldest frame still in the buffer
            assert_eq!(out, [6000.0, 6001.0, 6002.0, 6003.0]);
            assert_eq!(m.hdr().read_idx.load(Ordering::Relaxed), 6_004);
        }

        #[test]
        fn guards_reject_empty_wrong_channels_and_bad_magic() {
            let (_d, m) = mic_ring();
            let mut out = [9.0f32; 4];
            assert_eq!(m.read(&mut out, 4), 0, "empty ring");
            // The C side rejects a channel-count mismatch outright; the Rust
            // side cannot be handed one (the count is a field of RingMem), so
            // the equivalent guard is the src/dst length clamp.
            assert_eq!(m.write(&[1.0, 2.0], 8), 2, "clamped to the source length");
            let mut small = [0.0f32; 1];
            assert_eq!(m.read(&mut small, 8), 1, "clamped to the destination length");
        }

        #[test]
        fn flush_drops_the_backlog_without_touching_the_producer() {
            let (_d, m) = mic_ring();
            set_idx(&m, 500, 0);
            m.flush_consumer();
            assert_eq!(m.hdr().read_idx.load(Ordering::Relaxed), 500);
            assert_eq!(m.hdr().write_idx.load(Ordering::Relaxed), 500);
            let mut out = [0.0f32; 4];
            assert_eq!(m.read(&mut out, 4), 0);
        }

        /// The header now belongs to the DRIVER; attaching is what checks it.
        #[test]
        fn attach_takes_its_layout_from_the_reply_not_from_these_constants() {
            let d = FakeDriverRing::new(HAL_SPK_CHANNELS, HAL_SPK_BYTES);
            let m = attach_ring(&d, HAL_SPK_CHANNELS);
            assert_eq!(m.hdr().magic, HAL_RING_MAGIC);
            assert_eq!(m.hdr().capacity_frames, HAL_RING_FRAMES);
            assert_eq!(m.data_offset, HAL_RING_DATA_OFFSET, "the reply's offset, not a local one");
            assert_eq!(m.capacity, HAL_RING_FRAMES);
            assert_eq!(m.size, HAL_SPK_BYTES);
            // ...and it is genuinely the driver's object: the sample the
            // "driver" writes through its own mapping is visible through ours.
            unsafe { *((d.addr as usize + HAL_RING_DATA_OFFSET) as *mut f32) = 0.125 };
            assert_eq!(unsafe { *m.data() }, 0.125);
        }

        /// Every field the reply can lie about, one at a time. The point is
        /// that each is REPORTED, not assumed away: mapping a ring whose
        /// geometry we misread is the failure that produces noise, not silence.
        #[test]
        fn attach_refuses_a_geometry_this_daemon_cannot_consume() {
            let d = FakeDriverRing::new(HAL_SPK_CHANNELS, HAL_SPK_BYTES);
            let base = d.geom(HAL_SPK_CHANNELS);
            let bad: Vec<(&str, RingGeom, &str)> = vec![
                ("channels", RingGeom { channels: 4, ..base }, "can only consume"),
                ("rate", RingGeom { sample_rate: 44_100, ..base }, "fixed at 48000Hz"),
                ("no capacity", RingGeom { capacity_frames: 0, ..base }, "no capacity"),
                ("offset inside header", RingGeom { data_offset: 8, ..base }, "past the 40-byte header"),
                ("unaligned offset", RingGeom { data_offset: 66, ..base }, "4-aligned"),
                ("too small", RingGeom { bytes: 4096, ..base }, "its own geometry needs"),
                ("absurd", RingGeom { bytes: MAX_RING_BYTES + 1, ..base }, "its own geometry needs"),
            ];
            // `RingMem` owns a mapping and a port right, so it deliberately has
            // no Debug; unwrap_err would need one.
            let refuse = |what: &str, geom: RingGeom| -> String {
                match RingMem::attach(d.entry_copy(), geom, HAL_SPK_CHANNELS, "speaker") {
                    Ok(_) => panic!("{what} must be refused, not mapped"),
                    Err(e) => format!("{e:#}"),
                }
            };
            for (what, geom, expect) in bad {
                let msg = refuse(what, geom);
                assert!(msg.contains(expect), "{what}: {msg}");
            }
            // A ring whose header contradicts the reply describes a different
            // object than the entry port points at.
            unsafe { (*(d.addr as *mut RingHeader)).magic = 0xdead_beef };
            let msg = refuse("bad magic", base);
            assert!(msg.contains("magic"), "{msg}");
            unsafe {
                (*(d.addr as *mut RingHeader)).magic = HAL_RING_MAGIC;
                (*(d.addr as *mut RingHeader)).capacity_frames = 7;
            }
            let msg = refuse("header/reply disagreement", base);
            assert!(msg.contains("the reply said"), "{msg}");
        }

        #[test]
        fn speaker_frames_are_downmixed_and_padded_to_a_full_frame() {
            let (ds, _dm, rings) = attached_rings();
            // L=1.0 R=0.0 for 3 frames, written by the "driver" through ITS
            // mapping — the daemon only ever reads this ring.
            let spk = attach_ring(&ds, HAL_SPK_CHANNELS);
            let stereo = [1.0, 0.0, 1.0, 0.0, 1.0, 0.0];
            assert_eq!(spk.write(&stereo, 3), 3);
            let shared = test_shared(rings);
            let mut out = Vec::new();
            assert_eq!(shared.append_spk_frame(0, &mut out), 3);
            assert_eq!(out.len(), HAL_FRAME_48K);
            assert_eq!(&out[..3], &[0.5, 0.5, 0.5]);
            assert!(out[3..].iter().all(|s| *s == 0.0), "underrun must be silence");
        }

        #[test]
        fn mic_writes_are_counted_and_overflow_is_reported() {
            let (_ds, dm, rings) = attached_rings();
            dm.hdr().write_idx.store(23_997, Ordering::Relaxed);
            dm.hdr().read_idx.store(0, Ordering::Relaxed);
            let shared = test_shared(rings);
            let mono: Vec<f32> = (1..=10).map(|i| i as f32).collect();
            assert_eq!(shared.write_mic(0, &mono), 3);
            assert_eq!(shared.slots[0].mic_frames.load(Ordering::Relaxed), 3);
            assert_eq!(shared.slots[0].mic_dropped.load(Ordering::Relaxed), 7);
        }

        /// With no driver there is no ring, and that is NOT a drop: mic_dropped
        /// is the "driver is not draining" number and burying it under every
        /// frame produced while nothing is attached makes it useless.
        #[test]
        fn a_detached_bridge_is_silent_and_counts_nothing() {
            let shared = test_shared(Rings::new());
            let mut out = Vec::new();
            assert_eq!(shared.append_spk_frame(0, &mut out), 0);
            assert_eq!(out.len(), HAL_FRAME_48K);
            assert!(out.iter().all(|s| *s == 0.0));
            assert_eq!(shared.write_mic(0, &[0.5; 64]), 0);
            assert_eq!(shared.slots[0].mic_frames.load(Ordering::Relaxed), 0);
            assert_eq!(
                shared.slots[0].mic_dropped.load(Ordering::Relaxed),
                0,
                "no ring is not a drop"
            );
        }

        // ------------------------------------ HalSpeakerSource::depths()
        //
        // 规格 §3.2 的级 3′。此前零覆盖，而它恰恰是全链路**唯一恒定驻留上限
        // 正好 500 ms** 的级，也是 §0.3「修正二」点名未被洗清嫌疑的那一个：
        // 父任务的证据（90.25 s 内驱动写入 = tx_loop 读出，均 9025 帧）只证明
        // 速率相等，**结构上无法证伪**「恒定存着 400 ms」这个形态。
        // 下面这条就把那个形态造出来，断言新遥测直接把它显示成 400 ms。

        use audiohub_core::latency::{DropMode, StageId};

        fn attached_spk(frames_written: usize) -> (FakeDriverRing, FakeDriverRing, Arc<Shared>) {
            let ds = FakeDriverRing::new(HAL_SPK_CHANNELS, HAL_SPK_BYTES);
            let dm = FakeDriverRing::new(HAL_MIC_CHANNELS, HAL_MIC_BYTES);
            let rings = Rings::new();
            rings.attach(vec![RingPair {
                spk: attach_ring(&ds, HAL_SPK_CHANNELS),
                mic: attach_ring(&dm, HAL_MIC_CHANNELS),
            }]);
            if frames_written > 0 {
                // 「驱动」把应用播的音频写进它自己的那份映射。
                let ch = HAL_SPK_CHANNELS as usize;
                let buf = vec![0.5f32; frames_written * ch];
                let wrote = attach_ring(&ds, HAL_SPK_CHANNELS).write(&buf, frames_written);
                assert_eq!(wrote, frames_written, "环装得下这些帧");
            }
            (ds, dm, Arc::new(test_shared(rings)))
        }

        /// 既有的深度/消费语义测试都不该被水位削减改掉行为，所以默认建 `off`
        /// 档的源；治法 B 自己的测试显式传 `Mode::Active`。
        fn spk_source(shared: &Arc<Shared>, slot: u8) -> HalSpeakerSource {
            HalSpeakerSource::with_mode(shared.clone(), slot, trim::Mode::Off)
        }

        /// 环里恒定压着 400 ms —— 不饱和（80%）、不丢弃、进出速率可以严格相等，
        /// 但每一个样本都要迟到 400 ms 才出得来。旧证据对这个场景完全无感。
        #[test]
        fn a_hal_speaker_source_reports_the_ring_backlog_in_frames_at_48k() {
            use audiohub_net::media::FrameSource;
            let (_ds, _dm, shared) = attached_spk(19_200); // 400 ms @48k
            let src = spk_source(&shared, 0);

            let [first, second] = src.depths();
            let d = first.expect("附着着驱动，这一级必须存在");
            assert_eq!(d.id, StageId::HalSpk);
            assert_eq!(d.samples, 19_200);
            assert_eq!(d.capacity, HAL_RING_FRAMES, "500 ms 上限");
            assert_eq!(d.rate, HAL_SAMPLE_RATE, "环恒为 48k");
            assert_eq!(d.ms(), Some(400.0), "19200 / 48000 = 400 ms");
            assert!(
                !d.saturated(),
                "80% 不算饱和 —— 靠『是否饱和』判断这一级健不健康，恰好会漏掉它"
            );
            assert_eq!(
                d.dropped, None,
                "环满时写不进去的是驱动的 IOProc，计数在它那一侧 —— 观测不到就报 None，不报 0"
            );
            assert_eq!(d.drop_mode, DropMode::Newest, "我们只消费，从不 pop_front");
            assert!(second.is_none(), "这个源只有一级");
        }

        /// **观测不得改变被测对象**：读深度绝不能移动 `read_idx`，否则一读就把
        /// 那 400 ms 自己清零，遥测会永远报 0（规格 §0.3 点名的纪律）。
        #[test]
        fn reading_the_hal_depth_does_not_consume_the_ring() {
            use audiohub_net::media::FrameSource;
            let (_ds, _dm, shared) = attached_spk(19_200);
            let mut src = spk_source(&shared, 0);

            for _ in 0..5 {
                assert_eq!(src.depths()[0].unwrap().samples, 19_200, "读一次就少一点？");
            }
            // 真的取走一帧之后才准下降，且恰好降 480。
            let mut out = Vec::new();
            src.next_frame(&mut out);
            assert_eq!(out.len(), HAL_FRAME_48K);
            assert_eq!(src.depths()[0].unwrap().samples, 19_200 - 480);
        }

        /// 没有驱动 = 这一级**不存在**，不是 0 ms。空数组与「0 样本」在 UI 上是
        /// 两句不同的话，用 0 冒充会给出「这一级很健康」的假保证。
        #[test]
        fn a_detached_hal_speaker_source_reports_no_stage_at_all() {
            use audiohub_net::media::FrameSource;
            let shared = Arc::new(test_shared(Rings::new()));
            let src = spk_source(&shared, 0);
            assert!(src.depths().iter().all(|s| s.is_none()));
            // ...驱动没给出的槽同理。
            let (_ds, _dm, attached) = attached_spk(0);
            assert!(spk_source(&attached, 3).depths().iter().all(|s| s.is_none()));
            // 而附着着的槽 0 即使空环也**要**报这一级（0 样本是真读数）。
            let d = spk_source(&attached, 0).depths()[0].expect("槽 0 有环");
            assert_eq!(d.samples, 0);
            assert_eq!(d.ms(), Some(0.0));
        }

        /// 规格 §6.3 **注入 C 的满载形态**：把这条 500 ms 的环彻底灌满。
        ///
        /// 任务点名要求：分别灌满四个 1.000 秒 FIFO 与这条 500 ms 的 HAL 环，
        /// 确认遥测报出 ~1000 / ~500 ms **而不是沉默**。这一条负责后者。
        ///
        /// 注意它与 400 ms 那条的区别：400 ms 时 `saturated()` 是 **false**
        /// （80% < 95%），此刻才是 true。也就是说「靠是否饱和判断这一级健不
        /// 健康」在 400 ms 上会漏掉，在满载上才报警——所以真正可依赖的是
        /// `ms()` 这个读数本身，不是那个布尔。
        #[test]
        fn a_completely_full_hal_speaker_ring_reads_five_hundred_milliseconds() {
            use audiohub_net::media::FrameSource;
            let (_ds, _dm, shared) = attached_spk(HAL_RING_FRAMES as usize);
            let d = spk_source(&shared, 0).depths()[0].expect("这一级必须存在");
            assert_eq!(d.samples, HAL_RING_FRAMES, "环装满 = 24000 帧");
            assert_eq!(d.capacity, HAL_RING_FRAMES);
            assert_eq!(
                d.ms(),
                Some(500.0),
                "满载的虚拟扬声器环 = 500 ms —— 这一级要是不报，用户的半秒就没人说"
            );
            assert!(d.saturated());
            assert_eq!(d.dropped, None, "丢弃发生在驱动那一侧，观测不到就报 None");
            assert_eq!(d.drop_mode, DropMode::Newest);
        }


        // ==================================================== 治法 A / B / 埋点
        //
        // 这一组跑在**真的映射环**上：真 `RingMem`、真 `peek/advance`、真
        // `HalSpeakerSource::tick`。只有驱动的 IOProc 与时钟是脚本化的。

        /// 左右同值的立体声素材：下混 `(l+r)/2` 之后就是原始单声道。
        fn stereo(mono: &[f32]) -> Vec<f32> {
            let mut v = Vec::with_capacity(mono.len() * 2);
            for &s in mono {
                v.push(s);
                v.push(s);
            }
            v
        }

        /// 一台「驱动 + 守护进程」的最小现场。
        struct Rig {
            prod: RingMem,
            shared: Arc<Shared>,
            _ds: FakeDriverRing,
            _dm: FakeDriverRing,
        }

        fn rig() -> Rig {
            let ds = FakeDriverRing::new(HAL_SPK_CHANNELS, HAL_SPK_BYTES);
            let dm = FakeDriverRing::new(HAL_MIC_CHANNELS, HAL_MIC_BYTES);
            let rings = Rings::new();
            rings.attach(vec![RingPair {
                spk: attach_ring(&ds, HAL_SPK_CHANNELS),
                mic: attach_ring(&dm, HAL_MIC_CHANNELS),
            }]);
            let prod = attach_ring(&ds, HAL_SPK_CHANNELS);
            Rig { prod, shared: Arc::new(test_shared(rings)), _ds: ds, _dm: dm }
        }

        impl Rig {
            /// 「驱动的 IOProc 写了这么多帧」。
            fn drive(&self, mono: &[f32]) -> usize {
                self.prod.write(&stereo(mono), mono.len())
            }
            fn readable(&self) -> u32 {
                self.shared.rings.spk_readable(0).unwrap().0
            }
            fn bridge(&self) -> HalBridge {
                HalBridge { shared: self.shared.clone(), thread: Mutex::new(None) }
            }
            fn source(&self, mode: trim::Mode) -> HalSpeakerSource {
                HalSpeakerSource::with_mode(self.shared.clone(), 0, mode)
            }
            fn counters(&self) -> HalSlotCounters {
                self.shared.slots[0].snapshot()
            }
        }

        /// 相位连续的一帧 1 kHz（480 = 恰好 10 个周期，所以每帧都一样，
        /// 拼起来仍然是一条连续正弦）。
        fn tone_frame() -> Vec<f32> {
            audiohub_core::dsp::gen_sine(1000.0, HAL_SAMPLE_RATE, HAL_FRAME_48K, 0.5)
        }

        // ------------------------------------------------------------ 治法 A

        /// **治法 A**：跳 tick 时被跳过的那些帧必须从环里丢掉。
        #[test]
        fn treatment_a_drops_the_frames_a_skipped_tick_left_behind() {
            let rig = rig();
            let b = rig.bridge();
            let frame = tone_frame();
            for _ in 0..20 {
                rig.drive(&frame); // 200 ms 的既有水位
            }
            let before = rig.readable();
            // 一次 108 ms 的消费侧卡顿：生产者照写，我们一个 tick 都没跑。
            for _ in 0..11 {
                rig.drive(&frame);
            }
            assert_eq!(rig.readable(), before + 11 * HAL_FRAME_48K as u32);

            let epoch_before = rig.shared.slots[0].disc_epoch.load(Ordering::Relaxed);
            let dropped = b.drain_spk(0, 11 * HAL_FRAME_48K);
            assert_eq!(dropped, 11 * HAL_FRAME_48K, "被跳过的帧要一帧不剩地丢掉");
            assert_eq!(
                rig.readable(),
                before,
                "水位必须回到卡顿之前 —— 不是回到 0（那会欠载），是回到跳变前"
            );
            assert_eq!(rig.counters().skip_drained_frames, 11 * HAL_FRAME_48K as u64);
            assert!(
                rig.shared.slots[0].disc_epoch.load(Ordering::Relaxed) > epoch_before,
                "排空必须作废「上一 tick 的读后残量」：`W = A − D` 只在两次读之间\
                 只有生产者动过时才成立，不作废就会被当成一次巨大的生产侧漏写"
            );
        }

        /// **不做治法 A 会怎样**（即当前行为）：那 108 ms 一帧都出不去，永久。
        ///
        /// 这一条不是在测新代码，它是在把病本身钉成一条断言 —— 如果哪天有人
        /// 「简化」掉排空，上面那条会红，而这一条会**继续绿**，两条合起来才说清
        /// 「排空不是可有可无的优化」。
        #[test]
        fn without_treatment_a_a_stall_becomes_a_permanent_delay() {
            let rig = rig();
            let frame = tone_frame();
            for _ in 0..2 {
                rig.drive(&frame); // 20 ms 起手
            }
            let base = rig.readable();
            for _ in 0..11 {
                rig.drive(&frame); // 108 ms 卡顿期间生产者写进来的
            }
            let after_stall = rig.readable();
            assert_eq!(after_stall, base + 11 * HAL_FRAME_48K as u32);

            // 跳 tick（不排空），此后生产者与消费者严格同速率跑 5 秒。
            let mut src = rig.source(trim::Mode::Off);
            let mut out = Vec::new();
            for t in 0..500u64 {
                rig.drive(&frame);
                src.tick(t * 10_000, &mut out);
                assert_eq!(out.len(), HAL_FRAME_48K);
                out.clear();
            }
            assert_eq!(
                rig.readable(),
                after_stall,
                "同时钟 + 同速率 ⇒ 水位是历史的积分，没有任何吸引子把它拉回来。\
                 这就是那 9 小时 434 ms 的全部机制"
            );
        }

        /// 治法 A **不会排进工作储备**。
        ///
        /// 要丢的量按定义等于「被跳过的 tick 本来会读走的帧」，正常情况下丢完就
        /// 回到卡顿之前的水位。但如果生产者在同一段时间里**也**漏写了，环里根本
        /// 没那么多东西 —— 无脑丢到底就是把一个延迟问题换成一个欠载问题，而欠载
        /// 在真机上只表现成「偶尔有点断续」，靠听抓不住。
        #[test]
        fn treatment_a_never_drains_into_the_working_reserve() {
            let rig = rig();
            let frame = tone_frame();
            // 只有 20 ms：跳了 11 个 tick，但生产者显然也停了。
            for _ in 0..2 {
                rig.drive(&frame);
            }
            let b = rig.bridge();
            let dropped = b.drain_spk(0, 11 * HAL_FRAME_48K);
            assert_eq!(
                dropped,
                2 * HAL_FRAME_48K - trim::D_FLOOR_MIN,
                "排到底了 —— 低于 15 ms 的那部分不是积压，是驱动周期                 （512 帧 = 10.67 ms，比一个 tick 长）必需的储备"
            );
            assert_eq!(rig.readable() as usize, trim::D_FLOOR_MIN);
            // 已经只剩储备了：再排一次一帧都不许动。
            assert_eq!(b.drain_spk(0, 11 * HAL_FRAME_48K), 0);
            assert_eq!(rig.readable() as usize, trim::D_FLOOR_MIN);
            // ...但**不连续仍然要记**：什么都没丢掉恰恰说明生产者也停了，
            // 那时的 `A − D` 会算出一个过大的 W，把回撤压成 0 ⇒ D_floor 掉到
            // 结构性下限 ⇒ trim 在生产侧最不稳的时候反而更激进。方向反了。
            let mut src = rig.source(trim::Mode::Active);
            let mut out = Vec::new();
            src.tick(0, &mut out);
            out.clear();
            let e = rig.shared.slots[0].disc_epoch.load(Ordering::Relaxed);
            b.drain_spk(0, 11 * HAL_FRAME_48K);
            assert!(
                rig.shared.slots[0].disc_epoch.load(Ordering::Relaxed) > e,
                "一次没丢掉任何东西的排空没有作废观测"
            );
        }

        /// peek 只窥不取；advance 才动读指针；drop 以可读量封顶。
        #[test]
        fn peek_does_not_consume_and_advance_is_what_does() {
            let rig = rig();
            let frame = tone_frame();
            for _ in 0..4 {
                rig.drive(&frame);
            }
            let mut dst = vec![0.0f32; 960 * 2];
            let before = rig.readable();
            let (got, base) = rig.shared.rings.peek_spk(0, &mut dst, 960).unwrap();
            assert_eq!(got, 960);
            assert_eq!(rig.readable(), before, "窥视改变了被测对象");
            let (got2, base2) = rig.shared.rings.peek_spk(0, &mut dst, 960).unwrap();
            assert_eq!((got2, base2), (got, base), "第二次窥视必须拿到同一段");
            rig.shared.rings.advance_spk(0, base, 480);
            assert_eq!(rig.readable(), before - 480);
            // drop 以可读量封顶，不会把读指针推过写指针。
            let n = rig.shared.rings.drop_spk(0, 10 * HAL_FRAME_48K);
            assert_eq!(n as u32, before - 480);
            assert_eq!(rig.readable(), 0);
        }

        // ----------------------------------------------------- DLL 相位误差接线
        //
        // `dll::tests` 已经在纯控制律那一层证明了符号与限幅。这一段测的是**接线**：
        // 真环 → `HalSpeakerSource` 发布 → `HalBridge::spk_phase_error` 归约 →
        // 执行器方向。符号在任何一段被翻过来，下面第一条都会红。

        /// **端到端的符号**：真环上的高水位必须一路走成「周期变短」。
        ///
        /// 三段合起来才算数：源发布的 `err` 是负的（消费者语义）、归约不改符号、
        /// 喂进 DLL 之后 `period_nanos()` **小于**标称 10 ms。只断言中间某一段
        /// 的话，把两处符号同时写反的实现照样能过。
        #[test]
        fn the_published_phase_error_is_consumer_signed_end_to_end() {
            let rig = rig();
            let b = rig.bridge();
            let frame = tone_frame();
            for _ in 0..20 {
                rig.drive(&frame); // 200 ms —— 远高于任何可能的目标水位
            }
            let mut src = rig.source(trim::Mode::Off);
            let mut out = Vec::new();
            let mut win = SpkPhaseWindow::new();

            // 第一拍只建立代次基准（`prev == 0` 不算新鲜观测）。
            src.tick(0, &mut out);
            out.clear();
            assert!(
                b.spk_phase_error(&mut win).is_none(),
                "第一次看这个槽就当成新鲜观测 —— 那就没有任何东西能挡住\
                 「排空当拍的水位」被喂进环路"
            );

            rig.drive(&frame);
            src.tick(10_000, &mut out);
            out.clear();
            let p = b
                .spk_phase_error(&mut win)
                .expect("第二拍必须给出新鲜观测");
            assert_eq!(p.slot, 0);
            assert!(
                p.err_frames < 0.0,
                "水位 200 ms 远在目标之上，err 却不是负的（{}）—— 符号写成了\
                 `水位 − 目标`，那是正反馈",
                p.err_frames
            );

            let nominal = (HAL_FRAME_48K as f64 * 1e9 / HAL_SAMPLE_RATE as f64) as u64;
            let mut d = dll::Dll::new(HAL_FRAME_48K as f64, HAL_SAMPLE_RATE as f64);
            let corr = d.update(p.err_frames as f64);
            assert!(corr > 1.0, "corr = {corr} ≤ 1 ⇒ 读得更慢 ⇒ 水位继续涨");
            assert!(
                d.period_nanos() < nominal,
                "执行器方向反了：周期 {} ns ≥ 标称 {} ns",
                d.period_nanos(),
                nominal
            );
        }

        /// 陈旧读数不许冒充新鲜观测。
        ///
        /// 一条不再被消费的槽（会话关掉、驱动掉线）的最后一个误差会永远挂在
        /// 原子量里。把它当成新鲜的，环路就会去追一个早已不存在的水位。
        #[test]
        fn a_slot_that_did_not_tick_is_not_a_fresh_observation() {
            let rig = rig();
            let b = rig.bridge();
            let frame = tone_frame();
            for _ in 0..20 {
                rig.drive(&frame);
            }
            let mut src = rig.source(trim::Mode::Off);
            let mut out = Vec::new();
            let mut win = SpkPhaseWindow::new();
            for t in 0..2u64 {
                rig.drive(&frame);
                src.tick(t * 10_000, &mut out);
                out.clear();
                b.spk_phase_error(&mut win);
            }
            // 源这一拍没跑。
            assert!(
                b.spk_phase_error(&mut win).is_none(),
                "没被消费的槽给出了观测 —— 那个数是上一拍的"
            );
        }

        /// 不变量 I6 的 DLL 侧对应物：追平期一个观测都不发布。
        ///
        /// 追平期水位是**假高**（高是因为我们暂时没读，不是积压）。喂进环路，
        /// 它会去排一段马上就要被自己读走的音频 ⇒ 紧接着欠载。
        #[test]
        fn the_catch_up_window_publishes_no_observation() {
            let rig = rig();
            let b = rig.bridge();
            let frame = tone_frame();
            for _ in 0..20 {
                rig.drive(&frame);
            }
            let mut src = rig.source(trim::Mode::Off);
            let mut out = Vec::new();
            let mut win = SpkPhaseWindow::new();
            for t in 0..2u64 {
                rig.drive(&frame);
                src.tick(t * 10_000, &mut out);
                out.clear();
                b.spk_phase_error(&mut win);
            }
            rig.shared.tick_punctual.store(false, Ordering::Relaxed);
            rig.drive(&frame);
            src.tick(20_000, &mut out);
            out.clear();
            assert!(
                b.spk_phase_error(&mut win).is_none(),
                "追平期的假高水位被发布了出去"
            );
            // 恢复准时之后观测必须回来，否则这条挡板就变成了永久静音。
            rig.shared.tick_punctual.store(true, Ordering::Relaxed);
            rig.drive(&frame);
            src.tick(30_000, &mut out);
            out.clear();
            assert!(
                b.spk_phase_error(&mut win).is_some(),
                "准时之后观测没有恢复 —— 环路会永远保持最后一次命令"
            );
        }

        /// 一次不连续（治法 A 排空 / 重附着 / 代次 flush）当拍不发布观测。
        ///
        /// 排空**之后**那一拍的水位是对的，但**当拍**的读数横跨了跳变，
        /// 它既不是跳前也不是跳后。
        #[test]
        fn a_discontinuity_tick_publishes_no_observation() {
            let rig = rig();
            let b = rig.bridge();
            let frame = tone_frame();
            for _ in 0..30 {
                rig.drive(&frame);
            }
            let mut src = rig.source(trim::Mode::Off);
            let mut out = Vec::new();
            let mut win = SpkPhaseWindow::new();
            for t in 0..2u64 {
                rig.drive(&frame);
                src.tick(t * 10_000, &mut out);
                out.clear();
                b.spk_phase_error(&mut win);
            }
            // 治法 A：排空 11 个 tick 的量（会 bump disc_epoch）。
            b.drain_spk(0, 11 * HAL_FRAME_48K);
            rig.drive(&frame);
            src.tick(20_000, &mut out);
            out.clear();
            assert!(
                b.spk_phase_error(&mut win).is_none(),
                "横跨排空的那一拍被当成了有效观测"
            );
        }

        /// 多槽归约取**最饿的那条环**（`err` 最大者），不是最积压的那条。
        ///
        /// 唤醒周期只有一个，加快唤醒会同时加快所有环的消费。按最积压的那条去
        /// 伺服 = 把其余环读穿 ⇒ 欠载 ⇒ 可闻断续；反过来只是「最积压的那条排得
        /// 慢一点」，而那一条有按槽独立的 trim 兜底。风险不对称。
        #[test]
        fn the_multi_slot_reduction_picks_the_hungriest_ring() {
            let rig = rig();
            let b = rig.bridge();
            let mut win = SpkPhaseWindow::new();
            let put = |slot: usize, err: f32| {
                rig.shared.slots[slot]
                    .dll_err_frames
                    .store(err.to_bits(), Ordering::Relaxed);
                rig.shared.slots[slot].dll_epoch.fetch_add(1, Ordering::Release);
            };
            // 槽 0 严重积压（想读快），槽 1 已经在目标之下（想读慢）。
            put(0, -9_000.0);
            put(1, 300.0);
            assert!(b.spk_phase_error(&mut win).is_none(), "第一轮只建基准");
            put(0, -9_000.0);
            put(1, 300.0);
            let p = b.spk_phase_error(&mut win).expect("两个槽都新鲜");
            assert_eq!(
                p.slot, 1,
                "归约取了积压那条 —— 加快唤醒会把槽 1 读穿"
            );
            assert_eq!(p.err_frames, 300.0);
        }

        // ------------------------------------------------------------ 治法 B

        /// **治法 B**：真环上 400 ms 的存量被削到目标水位，全程零欠载。
        #[test]
        fn treatment_b_converges_a_real_ring_without_underrunning() {
            let rig = rig();
            let frame = tone_frame();
            for _ in 0..40 {
                rig.drive(&frame); // 400 ms 存量
            }
            assert_eq!(rig.readable(), 400 * 48);
            let mut src = rig.source(trim::Mode::Active);
            let mut out = Vec::new();
            for t in 0..6_000u64 {
                rig.drive(&frame);
                src.tick(t * 10_000, &mut out);
                assert_eq!(out.len(), HAL_FRAME_48K, "不变量 I1：输出恒为一帧");
                out.clear();
            }
            let c = rig.counters();
            let ms = rig.readable() as f32 / 48.0;
            assert!(
                ms <= c.trim.target_ms + 12.0,
                "60 秒里没收敛：还剩 {ms:.1} ms（目标 {:.1} ms）",
                c.trim.target_ms
            );
            assert!(c.trim.events > 0, "一次都没削");
            assert!(c.trim.frames >= (400 * 48 - 60 * 48) as u64 * 9 / 10, "削掉的量对不上存量");
            assert_eq!(c.underrun.frames, 0, "削过头了，补了 {} 帧静音", c.underrun.frames);
            assert!(c.trim.drawdown_ms >= 0.0 && c.trim.target_ms >= 15.0);
        }

        /// 反向：同一个现场，`mode = off` ⇒ 400 ms 一毫秒都不会少。
        #[test]
        fn without_treatment_b_the_backlog_never_leaves() {
            let rig = rig();
            let frame = tone_frame();
            for _ in 0..40 {
                rig.drive(&frame);
            }
            let mut src = rig.source(trim::Mode::Off);
            let mut out = Vec::new();
            for t in 0..6_000u64 {
                rig.drive(&frame);
                src.tick(t * 10_000, &mut out);
                out.clear();
            }
            assert_eq!(rig.readable(), 400 * 48, "off 档必须一帧都不动");
            assert_eq!(rig.counters().trim.events, 0);
        }

        /// 不变量 I6：追平期（`tick_punctual = false`）一次都不许削。
        #[test]
        fn no_trimming_on_catch_up_ticks_even_with_a_huge_backlog() {
            let rig = rig();
            let frame = tone_frame();
            for _ in 0..49 {
                rig.drive(&frame); // 490 ms，快贴顶了
            }
            rig.shared.tick_punctual.store(false, Ordering::Relaxed);
            let mut src = rig.source(trim::Mode::Active);
            let mut out = Vec::new();
            for t in 0..3_000u64 {
                rig.drive(&frame);
                src.tick(t * 10_000, &mut out);
                out.clear();
            }
            assert_eq!(
                rig.counters().trim.events,
                0,
                "追平期削了 —— 那些水位是假高（我们暂时没读），削完立刻欠载"
            );
            // 一旦准时，同一个现场立刻开始削。
            rig.shared.tick_punctual.store(true, Ordering::Relaxed);
            for t in 3_000..9_000u64 {
                rig.drive(&frame);
                src.tick(t * 10_000, &mut out);
                out.clear();
            }
            assert!(rig.counters().trim.events > 0, "准时之后必须开始削");
        }

        // ------------------------------------------------- 棘轮判据（核心交付物）
        //
        // 上面那些测试每个只注入**一次**事件，证明的是「一次卡顿会被吸收」。
        // 但这个病的本质是**积分**：单次行为正确不蕴含 N 次之后仍然正确
        //   —— 出病的原代码单看一次也「只是」多留 108 ms，正是那 108 ms 每次
        // 都留下来才在 9 小时后变成 434 ms。所以判据必须是**驻留对 N 的依赖**，
        // 不是任何单次读数。
        //
        // 下面这一组把 `tx_loop` 的跳 tick 算术连同真环一起重演 N 次，
        // 并把「治法前 / 只有 A / A+B」三档跑在**同一个**现场上做对照。

        /// 三档现场。对照是判据本身的一部分：只跑发布形态而不跑「治法前」，
        /// 就无从知道这个装置**能不能**测出棘轮 —— 一个永远不红的判据没有价值。
        #[derive(Clone, Copy, Debug, PartialEq)]
        enum Guard {
            /// 出病前的代码：跳 tick 不排空、无水位控制。
            Pre,
            /// 只有治法 A。
            AOnly,
            /// 发布形态：A + B。
            Full,
        }

        struct RatchetRun {
            /// 每次卡顿被吸收、现场重新稳定之后的驻留（ms），第 i 项 = 经历过
            /// i+1 次卡顿。
            marks: Vec<f32>,
            underrun_frames: u64,
            trim_frames: u64,
            skip_drained: u64,
        }

        /// 重演「正常跑一段 → 一次 >100 ms 消费侧卡顿」N 次。
        ///
        /// 忠实于 `engine.rs::tx_loop` 的那一段：
        ///   · 卡顿期间**生产者照写**（coreaudiod 的 IOProc 是实时优先级，
        ///     被抢占的是我们），我们一个 tick 都没跑；
        ///   · 卡顿被发现的那一个 tick，`punctual = behind <= tick` 为假 ——
        ///     先算 `punctual`、再排空、最后才 `tick = behind`（不变量 I6：
        ///     追平期的高水位是假象，不许削）；
        ///   · 治法 A 排空的量 = 被跳过的 tick 本来会读走的帧。
        fn ratchet_run(g: Guard, n_stalls: usize, stall_ms: u64, settle_ticks: usize) -> RatchetRun {
            let rig = rig();
            let bridge = rig.bridge();
            let frame = tone_frame();
            let mode = if g == Guard::Full { trim::Mode::Active } else { trim::Mode::Off };
            let mut src = rig.source(mode);
            let mut out = Vec::new();
            let mut now_us = 0u64;
            let mut marks = Vec::new();

            // 开流之后的起点：`engine.rs` 的一次性排空留下 `D_TARGET_COLD`。
            for _ in 0..(trim::D_TARGET_COLD / HAL_FRAME_48K) {
                rig.drive(&frame);
            }

            let missed = (stall_ms / 10) as usize;
            for _ in 0..n_stalls {
                // ---- 一次消费侧卡顿：生产者照写，我们停了。
                for _ in 0..missed {
                    rig.drive(&frame);
                }
                now_us += stall_ms * 1_000;

                // ---- `tx_loop` 的跳 tick 分支本身。
                if g != Guard::Pre {
                    // 治法 A。`drain_skipped_ticks` 对 HAL 槽做的就是这一句。
                    bridge.drain_spk(0, missed * HAL_FRAME_48K);
                }
                // 被发现的那个 tick 是**不准时**的。
                rig.shared.tick_punctual.store(false, Ordering::Relaxed);
                rig.drive(&frame);
                src.tick(now_us, &mut out);
                out.clear();
                now_us += 10_000;
                rig.shared.tick_punctual.store(true, Ordering::Relaxed);

                // ---- 之后正常跑，让现场重新稳定下来。
                for _ in 0..settle_ticks {
                    rig.drive(&frame);
                    src.tick(now_us, &mut out);
                    out.clear();
                    now_us += 10_000;
                }
                marks.push(rig.readable() as f32 / 48.0);
            }

            let c = rig.counters();
            RatchetRun {
                marks,
                underrun_frames: c.underrun.frames,
                trim_frames: c.trim.frames,
                skip_drained: c.skip_drained_frames,
            }
        }

        /// **核心判据（治法前）**：这个装置确实能测出棘轮。
        ///
        /// 没有这一条，下面两条「不增长」就是不可证伪的 —— 任何什么都不做的
        /// 实现都能让一个恒定的数保持恒定。
        #[test]
        fn before_the_fix_每次卡顿都永久留在环里() {
            let stall = 120u64;
            let r = ratchet_run(Guard::Pre, 3, stall, 300);
            let base = trim::D_TARGET_COLD as f32 / 48.0;

            // 逐次递增，步长就是卡顿时长。
            for (i, &m) in r.marks.iter().enumerate() {
                let want = base + stall as f32 * (i + 1) as f32;
                assert!(
                    (m - want).abs() < 1.0,
                    "第 {} 次卡顿之后应当是 {want:.1} ms（每次永久 +{stall} ms），实际 {m:.1} ms",
                    i + 1
                );
            }
            // 严格单调 —— 棘轮的定义。
            assert!(
                r.marks.windows(2).all(|w| w[1] > w[0] + stall as f32 - 1.0),
                "棘轮没有复现出来：{:?}",
                r.marks
            );
            assert_eq!(r.skip_drained, 0, "治法前不该有任何排空");
            assert_eq!(r.trim_frames, 0, "治法前不该有任何 trim");
        }

        /// **核心判据（治法后）**：驻留**不随 N 增长**。
        ///
        /// 判据写成「N 与 4N 的终点相同」而不是「终点小于某个常数」：后者可以被
        /// 一个「削到固定值」的实现蒙混过去，前者不能 —— 它直接问的就是
        /// 「这个量对卡顿次数有没有依赖」，也就是棘轮本身。
        #[test]
        fn after_the_fix_驻留不随卡顿次数增长() {
            let stall = 120u64;
            for g in [Guard::AOnly, Guard::Full] {
                let settle = if g == Guard::Full { 2_000 } else { 300 };
                let few = ratchet_run(g, 3, stall, settle);
                let many = ratchet_run(g, 12, stall, settle);

                let f_end = *few.marks.last().unwrap();
                let m_end = *many.marks.last().unwrap();

                // 1) 12 次卡顿与 3 次卡顿的终点一致。若还是棘轮，这里会差
                //    9 × 120 = 1080 ms（且早就撞上 500 ms 的环容量）。
                assert!(
                    (m_end - f_end).abs() <= 12.0,
                    "{g:?}: 3 次卡顿后 {f_end:.1} ms，12 次卡顿后 {m_end:.1} ms —— \
                     驻留仍然依赖卡顿次数，棘轮没被消除"
                );

                // 2) 整条轨迹没有单调段：最大值与最小值的差远小于一次卡顿。
                let hi = many.marks.iter().cloned().fold(f32::MIN, f32::max);
                let lo = many.marks.iter().cloned().fold(f32::MAX, f32::min);
                assert!(
                    hi - lo <= stall as f32 / 2.0,
                    "{g:?}: 12 次卡顿的轨迹极差 {:.1} ms，仍有累积迹象：{:?}",
                    hi - lo,
                    many.marks
                );

                // 3) 每一次卡顿都被真的排掉了（而不是碰巧没发生）。
                assert!(
                    many.skip_drained >= 12 * (stall / 10) as u64 * HAL_FRAME_48K as u64 * 9 / 10,
                    "{g:?}: 排空量对不上 12 次卡顿：{} 帧",
                    many.skip_drained
                );

                // 4) 代价不是欠载 —— 换来的必须是「一次空洞」，不是「持续断续」。
                assert_eq!(
                    many.underrun_frames, 0,
                    "{g:?}: 排空把延迟问题换成了欠载问题，补了 {} 帧静音",
                    many.underrun_frames
                );
            }
        }

        /// A+B 合起来还要把水位**拉回目标**，而不只是「不再增长」。
        ///
        /// 只有 A 时，起点是多少就停在多少（A 只还原卡顿前的水位）；B 才是把
        /// 那个水位本身变成受控量。分开断言，免得一档的功劳记在另一档头上。
        #[test]
        fn treatment_b_还把水位拉回目标而不只是止住增长() {
            let stall = 120u64;
            let a = ratchet_run(Guard::AOnly, 6, stall, 2_000);
            let full = ratchet_run(Guard::Full, 6, stall, 2_000);

            let a_end = *a.marks.last().unwrap();
            let f_end = *full.marks.last().unwrap();
            let cold = trim::D_TARGET_COLD as f32 / 48.0;

            // 只有 A：停在开流水位附近，一帧都没削。
            assert!((a_end - cold).abs() < 6.0, "只有 A 应当停在 {cold:.1} ms，实际 {a_end:.1}");
            assert_eq!(a.trim_frames, 0);

            // A+B：收敛到 `D_target`（生产者严格规律 ⇒ 回撤 0 ⇒ 目标压到下限）。
            let floor = trim::D_FLOOR_MIN as f32 / 48.0;
            assert!(
                f_end <= cold && f_end >= floor - 2.0,
                "A+B 应当收敛到 [{floor:.1}, {cold:.1}] ms，实际 {f_end:.1} ms"
            );
            assert!(full.trim_frames > 0, "B 一帧都没削");
            assert_eq!(full.underrun_frames, 0);
        }

        // ------------------------------------------- 听感安全（客观判据 · 生产路径）

        /// 四条客观判据跑在**生产路径**上，而不是直接调 `splice`。
        ///
        /// 为什么单列：t16 证明的是「控制器 + 原语」这一段合起来没问题，但它
        /// 不经过 `try_trim` 的**两段式 peek**、不经过立体声下混、不经过
        /// `advance_spk` 的环回绕。那三样恰恰是最容易把一次正确的拼接在落地时
        /// 弄坏的地方（peek 拿到的是环里两段不连续的内存）。这里把真环上
        /// `HalSpeakerSource::tick` 吐出来的**整条流**接住，再上量具。
        ///
        /// 用的是 `trim::tests` 里那**同一份**判据实现，不是复制品 —— 复制的
        /// 量具会各自漂移，届时两处都绿而实际已经不同。
        ///
        /// **这条测不了什么（独立注入实测，不要误信）**：把 `X` 改成 0（硬切）
        /// 时它**照样通过**。那不是判据失灵，是周期素材上 WSOLA 搜索会把 τ
        /// 吸附到整周期，此时交叉淡化在数学上是冗余的 —— t04 必须**强行**指定
        /// τ = 492（四分之一周期）才能把硬切的危害逼出来。淡化长度与淡化律的
        /// 敏感性归 t04 / t05 管；这一条管的是「环 + 两段式 peek + 下混 +
        /// 回绕」这段落地管路会不会把一次本来正确的拼接弄坏。
        #[test]
        fn the_production_trim_path_passes_the_objective_criteria() {
            use crate::halbridge::trim::tests::{c3_worst_db, c4_min_snr_db, max_slope};

            let rig = rig();
            let frame = tone_frame(); // 480 点 = 恰好 10 个周期 ⇒ 拼起来相位连续
            for _ in 0..40 {
                rig.drive(&frame); // 400 ms 存量，逼出真实的削减
            }
            let mut src = rig.source(trim::Mode::Active);
            let mut out = Vec::new();
            let mut stream: Vec<f32> = Vec::new();
            for t in 0..6_000u64 {
                rig.drive(&frame);
                src.tick(t * 10_000, &mut out);
                assert_eq!(out.len(), HAL_FRAME_48K, "不变量 I1：输出恒为一帧");
                stream.extend_from_slice(&out);
                out.clear();
            }
            let c = rig.counters();
            assert!(c.trim.events > 0, "这段里必须真的削过");
            assert_eq!(c.underrun.frames, 0, "补静音会把判据的前提（连续单音）破坏掉");

            // C2 时域斜率：参照物是同长度、同幅度的干净正弦。
            let reference = audiohub_core::dsp::gen_sine(
                1000.0,
                HAL_SAMPLE_RATE,
                stream.len(),
                0.5,
            );
            let c2 = max_slope(&stream) / max_slope(&reference);
            assert!(c2 <= 1.10, "C2：生产路径上整条流的最大斜率涨了 {c2:.3}×（硬切约 15×）");

            // C3 时域包络：2 ms 窗（必须比 4 ms 的淡化窄）。
            let c3 = c3_worst_db(&stream, 2 * 48, 48 / 2);
            assert!(c3 <= 1.5, "C3：生产路径上出现了 {c3:.2} dB 的短时电平不连续");

            // C4 频域：拼接窗的带外能量。
            let c4 = c4_min_snr_db(&stream, 1000.0);
            assert!(c4 >= 40.0, "C4：生产路径上最差一窗只有 {c4:.1} dB");

            // C1（流级）：从一条正弦里删掉**整数个周期**，剩下的仍然是同一条
            // 连续正弦。所以整条输出必须逐样本等于一条干净正弦。
            //
            // 这是这条测试里最锋利的一句：τ 只要不是整周期、淡化律只要用错、
            // peek 拿到的两段内存只要接错位、`advance_spk` 只要回绕算错，
            // 它立刻就红 —— 而 C2/C3/C4 都是**统计量**，小的错位可能被平均掉。
            let err = stream
                .iter()
                .zip(reference.iter())
                .fold(0.0f32, |m, (a, b)| m.max((a - b).abs()));
            assert!(
                err <= 1e-6,
                "C1：生产路径的输出不再是那条连续正弦，max|err| = {err:e} \
                 —— τ 没落在整周期上，或者环上的拼接接错位了"
            );
        }

        /// 静音的存量走快速通道，几个 tick 就收敛（且不扣令牌）。
        #[test]
        fn a_silent_backlog_collapses_on_the_fast_path() {
            let rig = rig();
            let silence = vec![0.0f32; HAL_FRAME_48K];
            for _ in 0..40 {
                rig.drive(&silence); // 400 ms 纯静音
            }
            let mut src = rig.source(trim::Mode::Active);
            let mut out = Vec::new();
            for t in 0..40u64 {
                rig.drive(&silence);
                src.tick(t * 10_000, &mut out);
                assert_eq!(out.len(), HAL_FRAME_48K);
                out.clear();
            }
            let ms = rig.readable() as f32 / 48.0;
            assert!(ms <= 60.0, "静音快速通道 400 ms 之内没收敛：还剩 {ms:.1} ms");
            assert_eq!(rig.counters().underrun.frames, 0);
        }

        // ------------------------------------------------------------ 埋点

        /// 欠载（短读 ⇒ 补静音）必须被数出来。这是「trim 有没有削过头」的唯一
        /// 直接证据，而 `append_spk_frame` 的返回值此前是被丢掉的。
        ///
        /// 时基从 `PRIME_TIMEOUT_US` 之后起步：这一条要测的是**稳态**下的短读
        /// 记账，不是冷启动。冷启动那一段由
        /// `a_cold_ring_is_primed_before_the_first_read` 单独管——在预填窗口里
        /// 空环发静音是**设计行为**而不是欠载，两件事必须分开测，否则改动其中
        /// 一件会莫名其妙地弄红另一件。
        #[test]
        fn an_underrun_is_counted_instead_of_silently_padded() {
            let rig = rig();
            let mut src = rig.source(trim::Mode::Off);
            let mut out = Vec::new();
            let t0 = HalSpeakerSource::PRIME_TIMEOUT_US + 10_000;
            // 环是空的：整帧补静音。
            src.tick(t0, &mut out);
            assert_eq!(out.len(), HAL_FRAME_48K);
            assert!(out.iter().all(|s| *s == 0.0));
            let c = rig.counters();
            assert_eq!(c.underrun.frames, HAL_FRAME_48K as u64);
            assert_eq!(c.underrun.events, 1);
            assert_eq!(c.underrun.worst_run_frames, HAL_FRAME_48K as u32);

            // 再连着短两个 tick ⇒ 还是**同一段**，段数不变、最长段变长。
            out.clear();
            src.tick(t0 + 10_000, &mut out);
            out.clear();
            src.tick(t0 + 20_000, &mut out);
            let c = rig.counters();
            assert_eq!(c.underrun.frames, 3 * HAL_FRAME_48K as u64);
            assert_eq!(c.underrun.events, 1, "连续短读是一段，不是三段");
            assert_eq!(c.underrun.worst_run_frames, 3 * HAL_FRAME_48K as u32);

            // 喂满一帧 ⇒ 段结束；下一次短读才是新的一段。
            rig.drive(&tone_frame());
            out.clear();
            src.tick(t0 + 30_000, &mut out);
            assert_eq!(rig.counters().underrun.events, 1);
            out.clear();
            src.tick(t0 + 40_000, &mut out);
            let c = rig.counters();
            assert_eq!(c.underrun.events, 2, "隔了一帧之后再短读是新的一段");
            assert_eq!(c.underrun.worst_run_frames, 3 * HAL_FRAME_48K as u32, "最长段是历史最大");
        }

        /// 冷启动/重新附着后**先把环填到 `D_TARGET_COLD` 再开始消费**。
        ///
        /// 现场捕获（2026-08-03，10 Hz 采样一台跑了 21.7 h 的 daemon）：驱动被
        /// 另一个进程接管后重新附着，环从 0 起步，`engine.rs` 开流处那段「留下
        /// 30ms 作为起始水位」一帧都没丢（`want = 0 − 1440` 饱和成 0），日志照打，
        /// 实际起始水位是 0。随后十几秒环深度在 0–800 帧之间爬，其间连续短读；
        /// 同一窗口 `skip.tx` **一次没动** —— 既不是排空过头也不是生产侧卡顿。
        ///
        /// 这条测的正是「削减只能封顶、不能兜底」这个缺口。
        #[test]
        fn a_cold_ring_is_primed_before_the_first_read() {
            let rig = rig();
            let mut src = rig.source(trim::Mode::Off);
            let mut out = Vec::new();
            let frame = tone_frame();

            // 环是空的：预填期发整帧静音，**不计欠载**，也不动 read_idx。
            for t in 0..2u64 {
                src.tick(t * 10_000, &mut out);
                assert_eq!(out.len(), HAL_FRAME_48K, "预填期仍然要输出整整一帧");
                assert!(out.iter().all(|s| *s == 0.0), "预填期输出必须是静音");
                out.clear();
            }
            assert_eq!(
                rig.counters().underrun.frames,
                0,
                "预填不是欠载：是我们自己选择还不读，不是该有数据却没有"
            );

            // 攒到 20 ms（< D_TARGET_COLD 30 ms）：还不够，继续等，环不许被动。
            for _ in 0..2 {
                rig.drive(&frame);
            }
            let before = rig.readable();
            src.tick(20_000, &mut out);
            out.clear();
            assert_eq!(rig.readable(), before, "预填期不许移动 read_idx");
            assert_eq!(rig.counters().underrun.frames, 0);

            // 攒够 30 ms ⇒ 开始正常消费，而且第一帧就是**满帧真实音频**。
            for _ in 0..2 {
                rig.drive(&frame);
            }
            src.tick(30_000, &mut out);
            assert_eq!(out.len(), HAL_FRAME_48K);
            assert!(out.iter().any(|s| *s != 0.0), "预填结束后第一帧必须有声音");
            assert_eq!(
                rig.counters().underrun.frames,
                0,
                "预填的全部意义就是让这第一帧不欠载"
            );
        }

        /// 生产者始终不来时，预填必须**超时放行**，而不是让这条流永远不出声。
        ///
        /// 这一条守的是兜底而不是主路径：驱动附着了但 IO 还没起来时环永远填不满，
        /// 无限等下去就把一个「30 ms 静音」换成了「永久静音」。
        #[test]
        fn priming_gives_up_after_the_timeout() {
            let rig = rig();
            let mut src = rig.source(trim::Mode::Off);
            let mut out = Vec::new();
            // 超时之前：静音，不计欠载。
            src.tick(0, &mut out);
            out.clear();
            assert_eq!(rig.counters().underrun.frames, 0);
            // 超时之后：回到正常路径，空环 ⇒ 如实计欠载。
            src.tick(HalSpeakerSource::PRIME_TIMEOUT_US + 10_000, &mut out);
            assert_eq!(out.len(), HAL_FRAME_48K);
            assert_eq!(
                rig.counters().underrun.frames,
                HAL_FRAME_48K as u64,
                "超时放行之后短读必须重新如实上报"
            );
        }

        /// 治法 A 的排空**不得**触发预填。
        ///
        /// 两者性质不同：排空之后环里仍留着工作储备（`D_FLOOR_MIN`），接着读是
        /// 对的；重新附着后环是空的，接着读必然连续短读。把两者混为一谈会让每
        /// 一次跳 tick 都白搭上 30 ms 静音。
        #[test]
        fn a_treatment_a_drain_does_not_re_arm_priming() {
            let rig = rig();
            let frame = tone_frame();
            for _ in 0..20 {
                rig.drive(&frame); // 200 ms
            }
            let mut src = rig.source(trim::Mode::Off);
            let mut out = Vec::new();
            src.tick(0, &mut out); // 环够深 ⇒ 预填立刻结束
            out.clear();

            rig.bridge().drain_spk(0, 5 * HAL_FRAME_48K); // 治法 A：排 50 ms
            rig.drive(&frame);
            src.tick(10_000, &mut out);
            assert!(
                out.iter().any(|s| *s != 0.0),
                "排空之后立刻又进了预填期：那 30 ms 静音是白搭的"
            );
        }

        /// 驱动在一段欠载**中途**脱离时，那一段必须就地结束。
        ///
        /// 修的是一个**测量**缺陷，不是音频缺陷：`tick` 在「没有环」那条路径上
        /// 直接 `return`，`note_short` 收不到归零信号 ⇒ `short_run` 一直挂着。
        /// 重新附着后再短读，`run_before != 0` ⇒ **不计新事件**、长度**继续累加**。
        /// 结果是 `underrun.events` 系统性少计，而 `worst_run_frames` 变成一个
        /// 横跨脱离期的合成数——它声称的那段时长里有一半根本没有环存在。
        ///
        /// 现场证据：一份 21 小时的读数里 `worst_run_frames = 96096`（2.0 秒），
        /// 而同期没有任何一次生产侧中断接近那个量级。
        #[test]
        fn a_driver_detach_closes_the_open_underrun_run() {
            let rig = rig();
            let mut src = rig.source(trim::Mode::Off);
            let mut out = Vec::new();
            // 时基跨过预填窗口：这一条测的是段的**边界**，不是冷启动。
            let t0 = HalSpeakerSource::PRIME_TIMEOUT_US + 10_000;
            // 环空 ⇒ 两个 tick 的短读，段还开着。
            for t in 0..2u64 {
                src.tick(t0 + t * 10_000, &mut out);
                out.clear();
            }
            assert_eq!(rig.counters().underrun.events, 1);
            assert_eq!(rig.counters().underrun.worst_run_frames, 2 * HAL_FRAME_48K as u32);

            // 驱动脱离：这一级不存在了，段必须在这里收尾。
            rig.shared.rings.detach();
            src.tick(t0 + 20_000, &mut out);
            out.clear();

            // 重新附着后再短读 ⇒ **新的一段**，而不是接着上一段往上加。
            // `attach_epoch` 必须跟着动：生产路径（`attach_reply`）在挂上 rings
            // 之后**最后**才 bump 它，消费者正是靠它认出「环换了一段内存」。只
            // 调 `rings.attach` 会造出一个现实中不存在的状态，测出来的行为也就
            // 不是生产行为。重新附着同时重新武装预填，所以这里要越过超时。
            rig.shared.rings.attach(vec![RingPair {
                spk: attach_ring(&rig._ds, HAL_SPK_CHANNELS),
                mic: attach_ring(&rig._dm, HAL_MIC_CHANNELS),
            }]);
            rig.shared.attach_epoch.fetch_add(1, Ordering::AcqRel);
            let t1 = t0 + 30_000 + HalSpeakerSource::PRIME_TIMEOUT_US;
            src.tick(t0 + 30_000, &mut out); // 这一拍重新武装预填
            out.clear();
            for t in 0..3u64 {
                src.tick(t1 + t * 10_000, &mut out);
                out.clear();
            }
            let c = rig.counters();
            assert_eq!(c.underrun.events, 2, "脱离期把两段黏成了一段");
            assert_eq!(
                c.underrun.worst_run_frames,
                3 * HAL_FRAME_48K as u32,
                "最长段跨过了驱动脱离期：它描述的时长里有一段根本没有环"
            );
        }

        /// 治法 A 的排空必须把「排完还剩多少」记下来。
        ///
        /// 这是「排空过头」与「生产侧卡顿」唯一的分辨依据：欠载紧随一次把水位
        /// 削到工作储备（15 ms）的排空 ⇒ 是我们的锅；欠载发生时上一次排空还剩
        /// 着正常水位 ⇒ 排空洗清嫌疑。此前这个数字根本没有被记下来过，于是那
        /// 两种性质完全相反的解释在数据上无法分辨。
        #[test]
        fn a_drain_records_what_it_left_behind() {
            let rig = rig();
            let frame = tone_frame();
            for _ in 0..10 {
                rig.drive(&frame); // 100 ms 存量
            }
            let b = rig.bridge();
            let before = rig.readable();
            let n = b.drain_spk(0, 4 * HAL_FRAME_48K); // 排 40 ms
            assert_eq!(n, 4 * HAL_FRAME_48K);
            let s = &rig.shared.slots[0];
            assert_ne!(s.last_drain_us.load(Ordering::Relaxed), 0, "排空没盖时间戳");
            assert_eq!(s.last_drain_frames.load(Ordering::Relaxed), n as u32);
            assert_eq!(
                s.last_drain_left.load(Ordering::Relaxed),
                before - n as u32,
                "剩余水位记错了"
            );

            // 「想排却排不动」也必须留痕：那正是生产者同时停摆的情形，是欠载
            // 最强的嫌疑人之一，不能因为没排掉东西就从案发记录里消失。
            let t_before = s.last_drain_us.load(Ordering::Relaxed);
            let left = rig.readable();
            assert_eq!(b.drain_spk(0, 100 * HAL_FRAME_48K), left as usize - trim::D_FLOOR_MIN);
            assert!(s.last_drain_us.load(Ordering::Relaxed) >= t_before);
            assert_eq!(
                s.last_drain_left.load(Ordering::Relaxed),
                trim::D_FLOOR_MIN as u32,
                "排到工作储备下沿时 left 必须等于 D_FLOOR_MIN"
            );
        }

        /// 没有驱动附着时的整帧静音**不算欠载** —— 那一级根本不存在，
        /// 把它计进去会把这个计数器唯一的诊断价值埋掉。
        #[test]
        fn silence_from_a_detached_driver_is_not_an_underrun() {
            let shared = Arc::new(test_shared(Rings::new()));
            let mut src = HalSpeakerSource::with_mode(shared.clone(), 0, trim::Mode::Off);
            let mut out = Vec::new();
            for t in 0..10u64 {
                src.tick(t * 10_000, &mut out);
                assert_eq!(out.len(), HAL_FRAME_48K);
                out.clear();
            }
            assert_eq!(shared.slots[0].snapshot().underrun.frames, 0);
        }

        /// 三组新计数器必须出现在 `HalBridge::status()` 上（IPC 就读这里），
        /// 而且跨槽是**求和**、三个水位读数是**取最大**。
        #[test]
        fn the_new_counters_reach_the_status_object() {
            let rig = rig();
            rig.shared.slot_count.store(1, Ordering::Relaxed);
            let frame = tone_frame();
            for _ in 0..40 {
                rig.drive(&frame);
            }
            let mut src = rig.source(trim::Mode::Active);
            let mut out = Vec::new();
            for t in 0..4_000u64 {
                rig.drive(&frame);
                src.tick(t * 10_000, &mut out);
                out.clear();
            }
            let b = rig.bridge();
            // 排空要看得见，就得先有超出工作储备的东西可排。
            for _ in 0..3 {
                rig.drive(&frame);
            }
            assert_eq!(b.drain_spk(0, 240), 240);
            let st = b.status();
            assert_eq!(st.slots.len(), 1);
            assert!(st.trim.events > 0, "trim.events 没上报");
            assert!(st.trim.frames > 0, "trim.frames 没上报");
            assert!(st.trim.target_ms >= 15.0, "target_ms 没上报: {}", st.trim.target_ms);
            assert!(st.trim.tokens_ms >= 0.0);
            assert_eq!(st.skip_drained_frames, 240);
            assert_eq!(st.underrun.frames, 0);
            // 汇总必须真的等于逐槽的和/最大，而不是各算各的。
            assert_eq!(st.trim.events, st.slots[0].trim.events);
            assert_eq!(st.trim.frames, st.slots[0].trim.frames);
            assert_eq!(st.trim.target_ms, st.slots[0].trim.target_ms);
            // ...并且 `dropped` 的语义没有被污染：主动 trim 绝不进那个字段。
            use audiohub_net::media::FrameSource;
            assert_eq!(
                src.depths()[0].unwrap().dropped,
                None,
                "trim 的量混进了 dropped —— 那会毁掉「饱和丢弃 vs 速率失配」的三态诊断"
            );
        }

        /// 代次 flush 与驱动重新附着都要作废控制器的观测，否则那个差值会被当成
        /// 一次巨大的生产侧漏写，把欠载边界永久抬高。
        #[test]
        fn a_flush_or_reattach_invalidates_the_controllers_observation() {
            let rig = rig();
            let frame = tone_frame();
            for _ in 0..30 {
                rig.drive(&frame);
            }
            let mut src = rig.source(trim::Mode::Active);
            let mut out = Vec::new();
            for t in 0..300u64 {
                rig.drive(&frame);
                src.tick(t * 10_000, &mut out);
                out.clear();
            }
            let dd = rig.counters().trim.drawdown_ms;
            // 代次变更：驱动 arm 了 flush，消费者这一侧吃掉它。
            rig.shared.arm_flush(0);
            for _ in 0..30 {
                rig.drive(&frame);
            }
            src.tick(3_000_000, &mut out);
            out.clear();
            assert_eq!(
                rig.readable(),
                0,
                "flush 的语义就是整段丢掉（代次换了，那是上一个对端的音频）"
            );
            let dd2 = rig.counters().trim.drawdown_ms;
            assert!(
                dd2 <= dd + 1.0,
                "flush 之后的差值被当成生产侧漏写了：回撤 {dd} → {dd2} ms，\
                 欠载边界会被这一下永久抬高"
            );
            // flush 之后那一帧仍然是发出去的静音，但它**不再计欠载**：flush 把环
            // 清空是**设计动作**，紧接着的空环是它的必然结果，不是「该有数据却
            // 没有」。这一拍进的是冷启动预填窗口（`prime_until_us`），语义是
            // 「我们自己选择还不读」——与接收侧的 `jb_prebuffering` 同一件事。
            //
            // 计数器仍然只说事实，只是事实被分成了两类：`underrun` 专管
            // 「想读而读不到」（trim/排空削过头，或生产侧真的断供），预填窗口
            // 专管「主动等」。混在一起的代价是实测过的：现场 30 次欠载里混着
            // 冷启动那一类，把「排空过头」与「生产侧卡顿」两个相反的结论都变得
            // 无法证伪。
            assert_eq!(rig.counters().underrun.frames, 0);
        }

        /// 模式 B 的虚拟麦克风环（500 ms）同样要能被灌满并报出来。与 spk 环
        /// 严格对称、只是方向相反：**丢弃发生在我们这一侧**（`write` 满了短写），
        /// 所以这一级的 `dropped` 是可观测的 `Some`，与 spk 的 `None` 构成对照。
        ///
        /// 走的是生产写入路径 `Shared::write_mic` 与生产读数路径
        /// `HalBridge::mic_depth`，不手写任何 `StageDepth`。
        #[test]
        fn a_completely_full_hal_mic_ring_reads_five_hundred_milliseconds() {
            let (_ds, _dm, rings) = attached_rings();
            let bridge = HalBridge {
                shared: Arc::new(test_shared(rings)),
                thread: Mutex::new(None),
            };

            // 空环：这一级**存在**且是 0 ms（0 ≠ 不存在）。
            let d = bridge.mic_depth(0).expect("附着着，这一级必须存在");
            assert_eq!(d.samples, 0);
            assert_eq!(d.ms(), Some(0.0));
            assert_eq!(d.capacity, HAL_RING_FRAMES);

            // 灌满：正好 24000 帧写得进去，第 24001 帧起短写。
            let mono = vec![0.5f32; HAL_RING_FRAMES as usize + 5_000];
            let wrote = bridge.shared.write_mic(0, &mono);
            assert_eq!(wrote, HAL_RING_FRAMES as usize, "环只装得下 24000 帧");

            let d = bridge.mic_depth(0).expect("这一级必须存在");
            assert_eq!(d.samples, HAL_RING_FRAMES);
            assert_eq!(d.ms(), Some(500.0), "满载的虚拟麦克风环 = 500 ms");
            assert!(d.saturated());
            assert_eq!(d.drop_mode, DropMode::Newest, "写满了短写：丢的是新样本");
            assert_eq!(
                d.dropped,
                Some(5_000),
                "**这一侧的丢弃数得出来** —— 与 hal_spk 的 None 正好构成对照"
            );
        }

        /// 驱动没附着时这一级不存在（`None`），而不是「0 ms 的健康读数」。
        #[test]
        fn a_detached_hal_mic_ring_reports_no_stage_at_all() {
            let bridge = HalBridge {
                shared: Arc::new(test_shared(Rings::new())),
                thread: Mutex::new(None),
            };
            assert!(bridge.mic_depth(0).is_none());
        }

        fn test_shared(rings: Rings) -> Shared {
            Shared {
                stop: AtomicBool::new(false),
                driver_found: AtomicBool::new(false),
                driver_connected: AtomicBool::new(false),
                slots: std::array::from_fn(|_| SlotShared::new()),
                last_driver_msg: Mutex::new(None),
                events: Mutex::new(Vec::new()),
                spk_flush: AtomicU16::new(0),
                published: AtomicU16::new(0),
                session_id: AtomicU64::new(0),
                attach_epoch: AtomicU64::new(0),
                slot_count: AtomicU32::new(0),
                driver_protocol: AtomicU32::new(0),
                status_reason: Mutex::new(None),
                bind_failures: AtomicU64::new(0),
                last_bind_error: Mutex::new(None),
                endpoint_name_fallbacks: AtomicU64::new(0),
                rings,
                driver_port: Mutex::new(MACH_PORT_NULL),
                superseded: AtomicBool::new(false),
                tick_punctual: AtomicBool::new(true),
            }
        }

        // ------------------------------------------------ shared, not copied

        /// The whole point of the memory-entry transport, and the invariant the
        /// "daemon reads zeroes while the driver's IO runs" bug violated: what
        /// the driver writes through ITS mapping has to be visible through the
        /// one `attach` just made. If this passes and the daemon still reads
        /// zeroes, the driver's IOProc is not producing — the transport is not
        /// the suspect.
        #[test]
        fn an_attached_ring_is_genuinely_shared_not_copied() {
            let d = FakeDriverRing::new(HAL_MIC_CHANNELS, HAL_MIC_BYTES);
            let m = attach_ring(&d, HAL_MIC_CHANNELS);
            assert_ne!(m.addr, d.addr, "our mapping is at a different address...");

            // ...of the same pages: the header the driver stamped before we
            // ever saw the entry is already visible through ours.
            assert_eq!(m.hdr().magic, HAL_RING_MAGIC, "we mapped a copy, not the object");
            assert_eq!(m.hdr().capacity_frames, HAL_RING_FRAMES);

            // Writes cross in both directions, in the header and in the samples.
            d.hdr().write_idx.store(4_242, Ordering::Release);
            assert_eq!(m.hdr().write_idx.load(Ordering::Acquire), 4_242);
            m.hdr().read_idx.store(99, Ordering::Release);
            assert_eq!(d.hdr().read_idx.load(Ordering::Acquire), 99);

            let a = (d.addr as usize + HAL_RING_DATA_OFFSET) as *mut f32;
            let b = m.data();
            unsafe {
                *a.add(7) = 0.75;
                assert_eq!(*b.add(7), 0.75, "sample area is a copy, not shared memory");
                *b.add(HAL_RING_FRAMES as usize - 1) = -0.5;
                assert_eq!(*a.add(HAL_RING_FRAMES as usize - 1), -0.5, "last frame not shared");
            }
        }

        /// The routing property the whole per-peer design rests on: slot N's
        /// audio goes to slot N's ring and nowhere else, in BOTH directions.
        ///
        /// An implementation that collapsed the set back to one pair would pass
        /// every "did the audio arrive" test there is, and the only symptom
        /// would be one peer hearing another's audio (regressions N1/N2).
        #[test]
        fn rings_route_each_slot_to_its_own_pair() {
            const N: usize = 3;
            let drivers: Vec<(FakeDriverRing, FakeDriverRing)> = (0..N)
                .map(|_| {
                    (
                        FakeDriverRing::new(HAL_SPK_CHANNELS, HAL_SPK_BYTES),
                        FakeDriverRing::new(HAL_MIC_CHANNELS, HAL_MIC_BYTES),
                    )
                })
                .collect();
            let rings = Rings::new();
            rings.attach(
                drivers
                    .iter()
                    .map(|(ds, dm)| RingPair {
                        spk: attach_ring(ds, HAL_SPK_CHANNELS),
                        mic: attach_ring(dm, HAL_MIC_CHANNELS),
                    })
                    .collect(),
            );

            // Each "driver" plays a distinct value into ITS speaker ring.
            for (slot, (ds, _)) in drivers.iter().enumerate() {
                let v = (slot + 1) as f32;
                assert_eq!(attach_ring(ds, HAL_SPK_CHANNELS).write(&[v, v, v, v], 2), 2);
            }
            for slot in 0..N {
                let mut out = [0.0f32; 4];
                assert_eq!(rings.read_spk(slot, &mut out, 2), 2);
                assert_eq!(
                    out[0],
                    (slot + 1) as f32,
                    "slot {slot} read another slot's speaker audio"
                );
                // ...and reading it consumed nothing but its own: every slot
                // still ahead of us in this loop must be untouched.
                for other in (slot + 1)..N {
                    assert_eq!(
                        drivers[other].0.hdr().read_idx.load(Ordering::Relaxed),
                        0,
                        "reading slot {slot} moved slot {other}'s consumer index"
                    );
                }
            }

            // Microphone direction: what the daemon writes for one peer must
            // reach that peer's ring only.
            assert_eq!(rings.write_mic(1, &[0.5; 8]), Some(8));
            for slot in 0..N {
                let want = if slot == 1 { 8 } else { 0 };
                assert_eq!(
                    drivers[slot].1.hdr().write_idx.load(Ordering::Relaxed),
                    want,
                    "slot {slot} received microphone audio meant for slot 1"
                );
            }

            // A slot beyond what this driver offers is silence and "no ring",
            // never a panic and never somebody else's ring.
            let mut out = [0.0f32; 4];
            assert_eq!(rings.read_spk(N, &mut out, 2), 0);
            assert_eq!(rings.write_mic(N, &[0.5; 8]), None);
        }

        /// The per-slot flush mask. A generation change on one slot must drop
        /// THAT slot's backlog and leave every other consumer where it is —
        /// a single flag would either flush all sixteen (dropping live audio on
        /// fifteen innocent slots) or none (replaying the previous tenant's
        /// half second to the new peer, spec-m5b §4.6).
        #[test]
        fn the_flush_mask_is_per_slot() {
            let shared = test_shared(Rings::new());
            shared.arm_flush(2);
            shared.arm_flush(5);
            assert!(!shared.take_flush(0));
            assert!(shared.take_flush(2));
            assert!(!shared.take_flush(2), "taking it consumes it");
            assert!(shared.take_flush(5));
            // Out of range is a no-op rather than a wrapped bit that would
            // flush an unrelated slot.
            shared.arm_flush(200);
            assert_eq!(shared.spk_flush.load(Ordering::Relaxed), 0);
        }

        /// A driver restart is a detach followed by an attach, any number of
        /// times. What this pins down is that the SECOND session reads the
        /// second driver's audio — a stale mapping surviving a detach would
        /// look exactly like a working bridge and carry nothing.
        #[test]
        fn rings_can_be_attached_detached_and_attached_again() {
            let rings = Rings::new();
            assert!(!rings.attached());
            let mut heard = Vec::new();
            for gen in 1..=3u32 {
                let ds = FakeDriverRing::new(HAL_SPK_CHANNELS, HAL_SPK_BYTES);
                let dm = FakeDriverRing::new(HAL_MIC_CHANNELS, HAL_MIC_BYTES);
                rings.attach(vec![RingPair {
                    spk: attach_ring(&ds, HAL_SPK_CHANNELS),
                    mic: attach_ring(&dm, HAL_MIC_CHANNELS),
                }]);
                assert!(rings.attached());

                // this generation's "driver" plays a tone the daemon must hear
                let spk = attach_ring(&ds, HAL_SPK_CHANNELS);
                let v = gen as f32;
                assert_eq!(spk.write(&[v, v, v, v], 2), 2);
                let mut out = [0.0f32; 4];
                assert_eq!(rings.read_spk(0, &mut out, 2), 2);
                heard.push(out[0]);
                // ...and the daemon's mic writes reach this generation's driver
                assert_eq!(rings.write_mic(0, &[v; 8]), Some(8));
                let mut got = [0.0f32; 8];
                assert_eq!(dm.hdr().write_idx.load(Ordering::Relaxed), 8);
                assert_eq!(attach_ring(&dm, HAL_MIC_CHANNELS).read(&mut got, 8), 8);
                assert_eq!(got[0], v);

                rings.detach();
                assert!(!rings.attached());
                assert_eq!(rings.read_spk(0, &mut out, 2), 0, "a detached ring is silence");
                assert_eq!(rings.write_mic(0, &[0.5; 8]), None, "not 'full' — absent");
            }
            assert_eq!(heard, vec![1.0, 2.0, 3.0], "each session heard its own driver");
        }

        // ------------------------------ one reply, thirty-two memory entries
        //
        // The per-peer redesign (one virtual device PAIR per paired peer, 16
        // slots) would turn today's two-entry `HelloReply` into a reply
        // carrying 32 memory entries. Everything downstream of that plan rests
        // on two things: that ONE mach message can carry them, and that the
        // receive buffer can be sized by arithmetic. The second is only safe
        // because NEITHER end asks for MACH_RCV_LARGE — a reply that does not
        // fit is DESTROYED by the kernel, so an undersized buffer degrades the
        // handshake to a silent timeout instead of wedging the port forever
        // (AudioHubBridge.c's BridgeRcvBuf comment makes the same trade on its
        // side). Both halves are measured here against the real kernel rather
        // than reasoned about, because getting either wrong is discovered as
        // "the driver never attaches" long after the protocol is frozen.

        /// `MACH_RCV_TOO_LARGE`. Only the tests need to name it: the service
        /// loop lumps every non-success, non-timeout receive into one backoff
        /// arm, which is precisely why what the kernel DID with the message has
        /// to be pinned down somewhere.
        const MACH_RCV_TOO_LARGE: KernReturn = 0x1000_4004u32 as i32;

        /// The shape the redesign wants: one reply, `N` memory entries, where
        /// `HelloReply` carries exactly two. Field order mirrors `HelloReply`
        /// so the sizes measured here are the sizes an equivalent C struct in
        /// AudioHubBridge.h would produce.
        ///
        /// The count of trailing `u32` is not arbitrary. Header + body is 28
        /// bytes, so for any EVEN descriptor count the payload starts at
        /// `28 + 12N` ≡ 4 (mod 8) and the closing `u64` pair only lands without
        /// compiler-inserted padding when an ODD number of `u32` precedes it.
        /// The shipped `HelloReply` solves this with nine `u32` and three
        /// `u64`; this probe uses eleven and two. Both are odd, both total 472
        /// at N=32, and that is the point — the arithmetic, not the field list,
        /// is what has to hold. This struct is deliberately NOT the contract:
        /// it is generic over N so the same code can measure 64 and 128, which
        /// no real message will ever be. `WIRE == size_of::<HelloReply>()`
        /// below is what keeps the measurement about the message we actually
        /// send.
        #[repr(C)]
        #[derive(Clone, Copy)]
        struct MultiEntryReply<const N: usize> {
            header: MsgHeader,
            body: MsgBody,
            entries: [PortDescriptor; N],
            status: u32,
            protocol_version: u32,
            data_offset: u32,
            slot_count: u32,
            active_slots: u32,
            spk_capacity_frames: u32,
            spk_channels: u32,
            spk_sample_rate: u32,
            mic_capacity_frames: u32,
            mic_channels: u32,
            mic_sample_rate: u32,
            spk_bytes: u64,
            mic_bytes: u64,
        }

        impl<const N: usize> MultiEntryReply<N> {
            /// Bytes on the wire — what `msgh_size` must say and what the
            /// receiver has to budget for before adding a trailer.
            const WIRE: usize = std::mem::size_of::<Self>();
            /// Where the descriptor array starts, for both ends.
            const DESCRIPTORS_AT: usize =
                std::mem::size_of::<MsgHeader>() + std::mem::size_of::<MsgBody>();

            /// `MAKE_SEND` on the destination manufactures a send right out of
            /// the receive right we hold, which is how a task sends to its own
            /// port without minting a second name — the same disposition the
            /// real `handshake` uses for its control-port descriptor.
            /// `COPY_SEND` on each entry is what the driver's reply does, and
            /// it leaves the sender's own right intact.
            fn build(entries: &[MachPort], dest: MachPort) -> MultiEntryReply<N> {
                assert_eq!(entries.len(), N);
                let mut m = MultiEntryReply::<N> {
                    header: MsgHeader {
                        bits: msgh_bits(MACH_MSG_TYPE_MAKE_SEND, 0) | MACH_MSGH_BITS_COMPLEX,
                        size: Self::WIRE as u32,
                        remote: dest,
                        local: MACH_PORT_NULL,
                        voucher: MACH_PORT_NULL,
                        id: MSG_HELLO_REPLY,
                    },
                    body: MsgBody { descriptor_count: N as u32 },
                    entries: [PortDescriptor::default(); N],
                    status: STATUS_OK,
                    protocol_version: PROTOCOL_VERSION,
                    data_offset: HAL_RING_DATA_OFFSET as u32,
                    slot_count: (N / 2) as u32,
                    active_slots: u32::MAX,
                    spk_capacity_frames: HAL_RING_FRAMES,
                    spk_channels: HAL_SPK_CHANNELS,
                    spk_sample_rate: HAL_SAMPLE_RATE,
                    mic_capacity_frames: HAL_RING_FRAMES,
                    mic_channels: HAL_MIC_CHANNELS,
                    mic_sample_rate: HAL_SAMPLE_RATE,
                    spk_bytes: HAL_SPK_BYTES as u64,
                    mic_bytes: HAL_MIC_BYTES as u64,
                };
                for (i, e) in entries.iter().enumerate() {
                    m.entries[i] = PortDescriptor {
                        name: *e,
                        pad1: 0,
                        pad2: 0,
                        disposition: MACH_MSG_TYPE_COPY_SEND as u8,
                        dtype: MACH_MSG_PORT_DESCRIPTOR,
                    };
                }
                m
            }
        }

        /// Sends one `N`-descriptor reply to `dest` from ANOTHER thread and
        /// waits for that thread to finish, so the message is provably queued
        /// before anything tries to receive it. Two threads, one task: mach
        /// rights are task-wide, so this is the same kernel path the driver
        /// takes, minus the process boundary.
        fn send_reply_from_thread<const N: usize>(entries: &[MachPort], dest: MachPort) -> KernReturn {
            let msg = MultiEntryReply::<N>::build(entries, dest);
            std::thread::spawn(move || {
                let mut msg = msg;
                unsafe {
                    mach_msg(
                        &mut msg.header,
                        MACH_SEND_MSG | MACH_SEND_TIMEOUT,
                        MultiEntryReply::<N>::WIRE as u32,
                        0,
                        MACH_PORT_NULL,
                        SEND_TIMEOUT_MS,
                        MACH_PORT_NULL,
                    )
                }
            })
            .join()
            .expect("the sending thread must not panic")
        }

        struct Received {
            kr: KernReturn,
            /// `msgh_size` as the kernel copied it out. Port descriptors are
            /// 12 bytes in both directions on 64-bit, so this should equal what
            /// was sent — an inequality would mean the wire size cannot be
            /// computed from the struct, which is how the buffer gets sized.
            size: u32,
            descriptors: u32,
            /// What the kernel actually appended. With no MACH_RCV_TRAILER_*
            /// bits requested this is the minimum format-0 trailer, and it is
            /// the number that decides how much slack `RCV_BUF` really needs.
            trailer: u32,
            names: Vec<MachPort>,
        }

        /// Receives into EXACTLY `rcv_size` bytes. A `Vec<u64>` rather than the
        /// production `MsgBuf`, for the same 8-alignment but a size chosen at
        /// runtime — finding where the kernel starts refusing is the point.
        fn receive_into(port: MachPort, rcv_size: usize, timeout_ms: u32) -> Received {
            let mut buf = vec![0u64; rcv_size.div_ceil(8).max(4)];
            let base = buf.as_mut_ptr() as *mut u8;
            let kr = unsafe {
                mach_msg(
                    base as *mut MsgHeader,
                    MACH_RCV_MSG | MACH_RCV_TIMEOUT,
                    0,
                    rcv_size as u32,
                    port,
                    timeout_ms,
                    MACH_PORT_NULL,
                )
            };
            if kr != MACH_MSG_SUCCESS {
                return Received { kr, size: 0, descriptors: 0, trailer: 0, names: Vec::new() };
            }
            // SAFETY: every offset below is 4-aligned against an 8-aligned
            // allocation, and a successful receive guarantees the kernel wrote
            // `size` bytes of message plus a trailer inside `rcv_size`.
            let hdr = base as *const MsgHeader;
            let size = unsafe { (*hdr).size };
            let complex = unsafe { (*hdr).bits } & MACH_MSGH_BITS_COMPLEX != 0;
            let descriptors = if complex {
                unsafe { *(base.add(std::mem::size_of::<MsgHeader>()) as *const u32) }
            } else {
                0
            };
            let trailer = unsafe { *(base.add(size as usize + 4) as *const u32) };
            let at = std::mem::size_of::<MsgHeader>() + std::mem::size_of::<MsgBody>();
            let names = (0..descriptors as usize)
                .map(|i| unsafe {
                    (*(base.add(at + i * std::mem::size_of::<PortDescriptor>())
                        as *const PortDescriptor))
                        .name
                })
                .collect();
            Received { kr, size, descriptors, trailer, names }
        }

        /// Received send rights are OURS. Dropping them by hand is the whole
        /// reason this file can assert about port leaks at all.
        fn release_names(names: &[MachPort]) {
            for n in names {
                let kr = unsafe { mach_port_deallocate(task_self(), *n) };
                assert_eq!(kr, KERN_SUCCESS, "received name {n:#x} must be a live send right");
            }
        }

        /// THE question. 32 memory entries in ONE reply, received into a buffer
        /// sized by arithmetic, every one of them attached through the REAL
        /// `RingMem::attach` and proven to share pages in both directions.
        ///
        /// A failure here is not a bug in this test: it means the 32-descriptor
        /// single-reply design is not viable and the redesign has to fall back
        /// to per-slot attach messages.
        #[test]
        fn a_single_reply_can_carry_thirty_two_memory_entries() {
            const N: usize = 32;
            const WIRE: usize = MultiEntryReply::<N>::WIRE;
            // 24 header + 4 body + 32*12 descriptors + 44 payload + 16 = 472.
            const _: () = assert!(WIRE == 472);
            // ...which is the size of the message this actually stands in for.
            // If `HelloReply` ever changes size, this measurement stops being
            // about it and has to be re-taken.
            const _: () = assert!(WIRE == std::mem::size_of::<HelloReply>());
            const _: () = assert!(MultiEntryReply::<N>::DESCRIPTORS_AT == 28);
            // The padding trap the doc comment warns about: if the eleven u32
            // were ten, the u64 pair would sit four bytes further along than a
            // hand-written wire layout says.
            const _: () = assert!(std::mem::offset_of!(MultiEntryReply<N>, spk_bytes) == 456);

            let rings: Vec<FakeDriverRing> = (0..N)
                .map(|i| {
                    let d = FakeDriverRing::new(HAL_MIC_CHANNELS, HAL_MIC_BYTES);
                    // A per-slot stamp so a mapping can be traced back to the
                    // descriptor index it arrived in: 32 entries that all
                    // shared ONE object would pass a naive "is it shared?"
                    // check and silently cross-wire every peer's audio.
                    d.hdr().write_idx.store(0x1000 + i as u64, Ordering::Release);
                    d
                })
                .collect();
            let entries: Vec<MachPort> = rings.iter().map(|d| d.entry).collect();

            let port = alloc_recv_port().expect("a receive port for the reply");
            let _guard = PortGuard(port);

            assert_eq!(
                send_reply_from_thread::<N>(&entries, port),
                MACH_MSG_SUCCESS,
                "the kernel refused to SEND a {N}-descriptor message; \
                 the 32-descriptor single-reply design is not viable — \
                 fall back to per-slot attach messages"
            );
            let got = receive_into(port, WIRE + MAX_TRAILER, HELLO_TIMEOUT_MS);
            assert_eq!(
                got.kr, MACH_MSG_SUCCESS,
                "a {N}-descriptor reply did not survive receive into {} bytes (kr {:#x}); \
                 the 32-descriptor single-reply design is not viable — \
                 fall back to per-slot attach messages",
                WIRE + MAX_TRAILER,
                got.kr
            );
            println!(
                "[32-entry reply] wire={WIRE}B received={}B descriptors={} trailer={}B \
                 buffer={}B (today's HelloReply={}B, RCV_BUF={RCV_BUF}B)",
                got.size,
                got.descriptors,
                got.trailer,
                WIRE + MAX_TRAILER,
                std::mem::size_of::<HelloReply>(),
            );
            assert_eq!(got.size as usize, WIRE, "the wire size must be computable from the struct");
            assert_eq!(got.descriptors as usize, N, "every descriptor must arrive");
            assert_eq!(got.names.len(), N);

            // Distinct names. In one task the received name for an entry we
            // also created is the SAME name with one more user reference (mach
            // names are per-task), so what this proves is that 32 distinct
            // kernel objects stayed 32 distinct objects across the message —
            // which is the property the design needs. Across a real process
            // boundary the names would differ; the ref-counting below is
            // identical either way.
            let unique: std::collections::HashSet<MachPort> = got.names.iter().copied().collect();
            assert_eq!(unique.len(), N, "the kernel collapsed distinct memory entries onto one name");
            assert!(!got.names.contains(&MACH_PORT_NULL), "a null name is a descriptor that never arrived");

            // Every entry through the PRODUCTION attach path, then a two-way
            // sharing check per slot. Anything less would prove the message
            // arrived without proving the memory behind it is usable.
            let mapped: Vec<RingMem> = got
                .names
                .iter()
                .enumerate()
                .map(|(i, name)| {
                    RingMem::attach(*name, rings[i].geom(HAL_MIC_CHANNELS), HAL_MIC_CHANNELS, "slot")
                        .unwrap_or_else(|e| panic!("slot {i} of {N} would not map ({e:#}); \
                             the 32-descriptor single-reply design is not viable — \
                             fall back to per-slot attach messages"))
                })
                .collect();
            for (i, m) in mapped.iter().enumerate() {
                assert_eq!(
                    m.hdr().write_idx.load(Ordering::Acquire),
                    0x1000 + i as u64,
                    "slot {i} mapped the wrong object: descriptors do not keep their order, \
                     or the entry is a copy rather than the driver's pages"
                );
                m.hdr().read_idx.store(0x2000 + i as u64, Ordering::Release);
            }
            for (i, d) in rings.iter().enumerate() {
                assert_eq!(
                    d.hdr().read_idx.load(Ordering::Acquire),
                    0x2000 + i as u64,
                    "slot {i}: the daemon's write never reached the driver's mapping"
                );
            }
            // `RingMem::drop` unmaps and releases each received right, so the
            // 32 rings below go back to exactly the one reference they were
            // created with.
            drop(mapped);
        }

        /// How much headroom is there above 32? The redesign is sized for 16
        /// slots today, and knowing whether the ceiling is 33 or thousands is
        /// the difference between "room to grow" and "one more device pair
        /// silently breaks the handshake".
        ///
        /// One-page entries here rather than full rings: the question is the
        /// DESCRIPTOR count, and 128 real rings would map 24MB to prove nothing
        /// extra. They go through the same `map_entry` the production attach
        /// uses, just without the geometry validator (which would reject a
        /// 16KB object claiming 24000 frames).
        #[test]
        fn descriptor_counts_well_past_thirty_two_still_fit_one_message() {
            /// Reports the measured line for `N` so the caller can print it:
            /// wire size, the size the kernel copied out, and the trailer.
            fn probe<const N: usize>() -> (usize, usize, u32, u32) {
                const PAGE: usize = 16_384;
                let rings: Vec<FakeDriverRing> = (0..N)
                    .map(|i| {
                        let d = FakeDriverRing::new(HAL_MIC_CHANNELS, PAGE);
                        d.hdr().write_idx.store(0x5000 + i as u64, Ordering::Release);
                        d
                    })
                    .collect();
                let entries: Vec<MachPort> = rings.iter().map(|d| d.entry).collect();
                let port = alloc_recv_port().expect("a receive port");
                let _guard = PortGuard(port);

                let wire = MultiEntryReply::<N>::WIRE;
                assert_eq!(
                    send_reply_from_thread::<N>(&entries, port),
                    MACH_MSG_SUCCESS,
                    "the kernel refused to send {N} descriptors"
                );
                let got = receive_into(port, wire + MAX_TRAILER, HELLO_TIMEOUT_MS);
                assert_eq!(got.kr, MACH_MSG_SUCCESS, "{N} descriptors failed to arrive: {:#x}", got.kr);
                assert_eq!(got.descriptors as usize, N);
                // Spot-check that the pages are real this far up, not just the
                // descriptor array: a per-message limit could plausibly deliver
                // names that no longer map.
                const W_IDX: usize = std::mem::offset_of!(RingHeader, write_idx);
                for (i, name) in got.names.iter().enumerate() {
                    let addr = map_entry(*name, PAGE).expect("a delivered entry must map");
                    let w = unsafe { &*((addr as usize + W_IDX) as *const AtomicU64) };
                    assert_eq!(
                        w.load(Ordering::Acquire),
                        0x5000 + i as u64,
                        "entry {i} of {N} does not point at slot {i}'s pages"
                    );
                    unsafe { mach_vm_deallocate(task_self(), addr, PAGE as u64) };
                }
                release_names(&got.names);
                (N, wire, got.size, got.trailer)
            }

            for (n, wire, size, trailer) in [probe::<32>(), probe::<64>(), probe::<128>()] {
                println!(
                    "[descriptor scaling] N={n:<4} wire={wire:<5}B received={size:<5}B \
                     trailer={trailer}B buffer_needed={}B",
                    wire + MAX_TRAILER
                );
                assert_eq!(size as usize, wire);
            }
        }

        /// The assumption the no-`MACH_RCV_LARGE` decision rests on, and the
        /// only one of these that can be got wrong silently: a message that
        /// does not fit must be DESTROYED, not left queued for a second,
        /// bigger receive. If it were queued, an undersized buffer would wedge
        /// the port permanently instead of costing one handshake.
        ///
        /// It also measures the true minimum receive size, which is what the
        /// redesign has to budget: `RCV_BUF >= size_of::<Reply>() + MAX_TRAILER`
        /// is correct but conservative, and knowing by how much is the
        /// difference between an informed constant and a lucky one.
        #[test]
        fn a_reply_that_does_not_fit_is_destroyed_rather_than_queued() {
            const N: usize = 32;
            const WIRE: usize = MultiEntryReply::<N>::WIRE;
            const PAGE: usize = 16_384;

            let rings: Vec<FakeDriverRing> =
                (0..N).map(|_| FakeDriverRing::new(HAL_MIC_CHANNELS, PAGE)).collect();
            let entries: Vec<MachPort> = rings.iter().map(|d| d.entry).collect();
            let port = alloc_recv_port().expect("a receive port");
            let _guard = PortGuard(port);

            // Walk up from "exactly the message, no trailer room" until the
            // kernel accepts it. Every rejected pass also proves the queue is
            // empty again: if rejected messages accumulated, the port's default
            // queue limit would make the sends below start timing out.
            let mut minimum = None;
            for extra in 0..=MAX_TRAILER {
                let rcv = WIRE + extra;
                assert_eq!(send_reply_from_thread::<N>(&entries, port), MACH_MSG_SUCCESS,
                    "send failed at probe {rcv} — earlier rejects were queued, not destroyed");
                let got = receive_into(port, rcv, HELLO_TIMEOUT_MS);
                if got.kr == MACH_MSG_SUCCESS {
                    assert_eq!(got.descriptors as usize, N);
                    release_names(&got.names);
                    minimum = Some(rcv);
                    break;
                }
                assert_eq!(
                    got.kr, MACH_RCV_TOO_LARGE,
                    "an undersized receive ({rcv}B for a {WIRE}B message) must report \
                     MACH_RCV_TOO_LARGE, got {:#x}",
                    got.kr
                );
            }
            let minimum = minimum.expect("some size in [WIRE, WIRE+MAX_TRAILER] must be accepted");

            println!(
                "[undersized receive] wire={WIRE}B minimum_rcv={minimum}B \
                 (= wire + {}B trailer); production budget wire+MAX_TRAILER={}B, \
                 slack={}B",
                minimum - WIRE,
                WIRE + MAX_TRAILER,
                WIRE + MAX_TRAILER - minimum
            );
            assert!(minimum >= WIRE, "the kernel cannot have delivered a truncated message");
            assert!(
                minimum <= WIRE + MAX_TRAILER,
                "sizing a buffer as message + MAX_TRAILER is NOT sufficient; every \
                 const assert in this file that uses that rule is wrong"
            );

            // The trap this test was written to find, kept as a live control
            // now that v2 has walked out of it. 256 was `RCV_BUF` while the
            // reply was 104 bytes; the v2 reply is 472, and growing the
            // descriptor count without growing the buffer does not produce a
            // short read or a truncated reply — it produces a handshake that
            // times out with no diagnostic anywhere, forever. Naming the old
            // value as a literal keeps that demonstration honest instead of
            // asserting something about the current constant that stopped being
            // true the moment the constant was fixed.
            const PRE_V2_RCV_BUF: usize = 256;
            assert!(PRE_V2_RCV_BUF < minimum);
            assert_eq!(send_reply_from_thread::<N>(&entries, port), MACH_MSG_SUCCESS);
            let with_the_old_buffer = receive_into(port, PRE_V2_RCV_BUF, HELLO_TIMEOUT_MS);
            assert_eq!(
                with_the_old_buffer.kr, MACH_RCV_TOO_LARGE,
                "a {WIRE}B reply into the pre-v2 {PRE_V2_RCV_BUF}B buffer must be refused outright"
            );
            // ...and the positive form: the constant this daemon ships MUST be
            // able to receive the reply it is about to start asking for. This is
            // the assertion that now guards the handshake.
            assert!(
                RCV_BUF >= minimum,
                "RCV_BUF is {RCV_BUF}B; a {WIRE}B HelloReply needs at least {minimum}B or the \
                 kernel destroys it and the handshake times out with no diagnostic"
            );
            assert_eq!(send_reply_from_thread::<N>(&entries, port), MACH_MSG_SUCCESS);
            let with_todays_buffer = receive_into(port, RCV_BUF, HELLO_TIMEOUT_MS);
            assert_eq!(
                with_todays_buffer.kr, MACH_MSG_SUCCESS,
                "a {WIRE}B reply must fit today's RCV_BUF={RCV_BUF}B"
            );
            assert_eq!(with_todays_buffer.descriptors as usize, N);
            release_names(&with_todays_buffer.names);
            println!(
                "[undersized receive] a {WIRE}B reply into the pre-v2 {PRE_V2_RCV_BUF}B buffer -> \
                 MACH_RCV_TOO_LARGE; into v2's RCV_BUF={RCV_BUF}B -> delivered with {N} descriptors"
            );

            // ONE byte too small, then a full-size retry with a real timeout.
            // A timeout is the proof: the message is gone, not waiting.
            assert_eq!(send_reply_from_thread::<N>(&entries, port), MACH_MSG_SUCCESS);
            let short = receive_into(port, minimum - 1, HELLO_TIMEOUT_MS);
            assert_eq!(
                short.kr, MACH_RCV_TOO_LARGE,
                "one byte under the minimum must be refused outright, got {:#x}",
                short.kr
            );
            let retry = receive_into(port, WIRE + MAX_TRAILER, RECV_TIMEOUT_MS);
            assert_eq!(
                retry.kr, MACH_RCV_TIMED_OUT,
                "the refused message was still QUEUED (kr {:#x}, {} descriptors). Without \
                 MACH_RCV_LARGE the kernel is supposed to destroy it; if it does not, an \
                 undersized buffer wedges the port instead of costing one handshake, and \
                 both ends' buffer-sizing comments are wrong",
                retry.kr,
                retry.descriptors
            );
            println!(
                "[undersized receive] {}B (minimum-1) -> MACH_RCV_TOO_LARGE, \
                 re-receive at {}B -> MACH_RCV_TIMED_OUT: destroyed, not queued",
                minimum - 1,
                WIRE + MAX_TRAILER
            );

            // ...and destroying it gave the rights back. This test has by now
            // sent each entry through ~11 messages, all but one of them
            // destroyed by the kernel, so if a destroyed message dropped its
            // COPY_SEND references on the floor the entries would be carrying
            // ten surplus user references each. That matters more to the
            // redesign than to today's code: a driver that retries a reply the
            // daemon's buffer is too small for would bleed 32 rights per
            // attempt, and the port name space is the one resource coreaudiod
            // cannot be restarted to reclaim.
            //
            // Counting is by subtraction, since nothing here can read a uref
            // count directly: one `mod_refs(-1)` must succeed (the creator's
            // own reference) and the next must NOT, which is only true if the
            // count was exactly one.
            for (i, d) in rings.iter().enumerate() {
                let first = unsafe {
                    mach_port_mod_refs(task_self(), d.entry, MACH_PORT_RIGHT_SEND, -1)
                };
                assert_eq!(first, KERN_SUCCESS, "entry {i} lost the creator's own reference");
                let second = unsafe {
                    mach_port_mod_refs(task_self(), d.entry, MACH_PORT_RIGHT_SEND, -1)
                };
                assert_ne!(
                    second, KERN_SUCCESS,
                    "entry {i} still holds a send right after every message carrying it was \
                     destroyed: the kernel leaks descriptor rights on MACH_RCV_TOO_LARGE, so a \
                     retried oversized reply bleeds one right per entry per attempt"
                );
            }
            // The names are gone now, so `FakeDriverRing::drop`'s deallocate is
            // a no-op; its `mach_vm_deallocate` still has work to do, because a
            // mapping holds its own reference to the VM object.
            println!("[undersized receive] all {N} entries back to a single user reference: \
                      a destroyed message returns the rights it carried");
        }

        // --------------------------------------------- connect/retry ladder

        #[test]
        fn retry_backs_off_by_doubling_and_stops_at_the_cap() {
            let t0 = Instant::now();
            let mut r = RetryPlan::ready_now(t0);
            assert!(r.due(t0), "the first attempt happens immediately");

            r.failed(t0);
            assert!(!r.due(t0), "a failure must never be retried in the same pass");
            assert!(!r.due(t0 + RETRY_MIN - Duration::from_millis(1)));
            assert!(r.due(t0 + RETRY_MIN));

            // 500ms, 1s, 2s, 4s, then pinned at the 5s cap forever.
            let mut waits = Vec::new();
            let mut now = t0;
            let mut r = RetryPlan::ready_now(t0);
            for _ in 0..7 {
                r.failed(now);
                waits.push(r.next_attempt - now);
                now = r.next_attempt;
            }
            assert_eq!(
                waits,
                vec![
                    Duration::from_millis(500),
                    Duration::from_secs(1),
                    Duration::from_secs(2),
                    Duration::from_secs(4),
                    RETRY_MAX,
                    RETRY_MAX,
                    RETRY_MAX,
                ]
            );
            assert!(waits.iter().all(|w| *w >= RETRY_MIN), "the loop must never spin");
        }

        #[test]
        fn a_connection_resets_the_ladder_so_the_next_outage_recovers_fast() {
            let t0 = Instant::now();
            let mut r = RetryPlan::ready_now(t0);
            for _ in 0..5 {
                r.failed(t0);
            }
            assert_eq!(r.backoff, RETRY_MAX);
            r.succeeded();
            let mut now = t0;
            r.failed(now);
            assert_eq!(r.next_attempt - now, RETRY_MIN);
            now = r.next_attempt;
            assert!(r.due(now));
        }

        #[test]
        fn silence_only_expires_a_driver_that_has_actually_spoken() {
            let shared = test_shared(Rings::new());
            shared.driver_connected.store(true, Ordering::Relaxed);
            let now = Instant::now();

            // Never spoke: in this direction the driver owes us nothing, so
            // silence is not evidence of death — only a dead port is.
            expire_silent_driver(&shared, now + DRIVER_SILENT_AFTER * 10);
            assert!(shared.driver_connected.load(Ordering::Relaxed));

            // Spoke once, then stopped: that IS evidence (a plug-in unloaded
            // inside a coreaudiod that is still alive keeps its port).
            *lk(&shared.last_driver_msg) = Some(now);
            expire_silent_driver(&shared, now + DRIVER_SILENT_AFTER);
            assert!(shared.driver_connected.load(Ordering::Relaxed), "exactly at the edge is alive");
            expire_silent_driver(&shared, now + DRIVER_SILENT_AFTER + Duration::from_millis(1));
            assert!(!shared.driver_connected.load(Ordering::Relaxed));
        }

        #[test]
        fn a_send_with_no_driver_reports_a_dead_destination_and_never_panics() {
            let shared = test_shared(Rings::new());
            assert_eq!(*lk(&shared.driver_port), MACH_PORT_NULL);
            let (kr, port) = send_to_driver(&shared, NOTIFY_PING, 0, 0.0, 0, 0);
            assert_eq!(kr, MACH_SEND_INVALID_DEST);
            assert_eq!(port, MACH_PORT_NULL);
            // ...and a failure on a port that is not the live one is ignored:
            // a slow send must never tear down the session that replaced it.
            *lk(&shared.driver_port) = 0xdead_beef;
            shared.driver_connected.store(true, Ordering::Relaxed);
            disconnect_port(&shared, 0x1234, "a stale session's failure");
            assert!(shared.driver_connected.load(Ordering::Relaxed), "wrong port tore down the live one");
            *lk(&shared.driver_port) = MACH_PORT_NULL;
            shared.driver_connected.store(false, Ordering::Relaxed);
            // The public path swallows it: a peer volume change with no driver
            // attached is a no-op, not an error the daemon has to handle.
            shared.notify_volume(HalEndpoint::out(0), 1, 0.5, false);
            assert!(!shared.driver_connected.load(Ordering::Relaxed));
        }

        #[test]
        fn look_up_of_a_name_nobody_published_fails_without_side_effects() {
            // The steady state on a machine with no driver, and the reason
            // `Auto` can gate on it: unlike bootstrap_check_in, a look-up of an
            // unknown name cannot accidentally succeed.
            let e = look_up("com.audiohub.driver.nonexistent.test")
                .expect_err("an unpublished name must not resolve");
            assert!(format!("{e:#}").contains("not published"), "{e:#}");
        }

        #[test]
        fn auto_mode_is_silent_and_bridgeless_when_the_driver_is_absent() {
            let cfg = HalBridgeCfg {
                service_name: "com.audiohub.driver.nonexistent.test".to_string(),
                mode: HalBridgeMode::Auto,
            };
            assert!(start(cfg).expect("absent driver is not an error").is_none());
        }

        /// The retry ladder for real, thread and all: `Require` builds a bridge
        /// against a name nobody will ever publish, so the loop spends its
        /// whole life in the searching branch. What this pins down is that a
        /// driverless daemon still answers `status`, still hands the media
        /// engine silence, and still shuts down promptly — the three things a
        /// user whose driver is not installed yet actually experiences.
        #[test]
        fn a_bridge_that_never_finds_a_driver_stays_usable_and_stops_on_command() {
            let cfg = HalBridgeCfg {
                service_name: "com.audiohub.driver.nonexistent.test".to_string(),
                mode: HalBridgeMode::Require,
            };
            let b = start(cfg).expect("require builds a bridge and keeps looking").expect("some");
            // Long enough for the loop's 200ms receive to time out and at least
            // one look-up to fail.
            std::thread::sleep(Duration::from_millis(600));
            let st = b.status();
            assert!(!st.driver_found, "nothing published that name");
            assert!(!st.driver_connected);
            assert_eq!(st.spk_frames, 0);
            assert!(st.last_driver_msg_secs.is_none(), "no driver ever spoke");

            // The audio path works with no driver: a full frame of silence.
            let mut out = Vec::new();
            assert_eq!(b.append_spk_frame(0, &mut out), 0);
            assert_eq!(out.len(), HAL_FRAME_48K);
            assert!(out.iter().all(|s| *s == 0.0));
            // ...and mic writes are swallowed without an error and without
            // running the drop counter up: there is no ring to be full.
            assert_eq!(b.write_mic_mono(0, &[0.25; 64]), 0);
            assert_eq!(b.status().mic_dropped, 0);
            assert!(b.drain_events().is_empty());
            b.notify_volume(HalEndpoint::out(0), 1, 0.4, false); // no driver: a no-op
            // ...as are Binds: nothing to send them to, and the coordinator
            // simply retries on its next pass.
            assert!(!b.bind_clear(0, 1));
            assert_eq!(b.slot_count(), 0, "a bridge with no driver has no capacity");

            let t = Instant::now();
            drop(b); // shutdown + join
            assert!(t.elapsed() < Duration::from_secs(2), "join took {:?}", t.elapsed());
        }

        #[test]
        fn off_mode_never_touches_mach_even_with_a_driver_present() {
            let cfg = HalBridgeCfg {
                service_name: HAL_SERVICE_NAME.to_string(),
                mode: HalBridgeMode::Off,
            };
            assert!(start(cfg).expect("off is not an error").is_none());
        }
    }
}

// ---------------------------------------------------------------- other platforms

#[cfg(not(target_os = "macos"))]
mod platform {
    use super::*;

    /// The platform's private state bag, held by `Shared`.
    ///
    /// The name is inherited from the macOS side. On Windows it holds two
    /// things: the driver session (control plane) and the mapped rings (data
    /// plane), and they are deliberately separate. The session lives behind a
    /// `Mutex` because an IOCTL is a blocking round trip; the rings live behind
    /// their own `RwLock` because the audio path must never wait on a control
    /// call to read a frame.
    ///
    /// Putting both HERE rather than adding fields to `Shared` is what keeps
    /// `Shared` free of `cfg`: it is already the one member declared as
    /// `platform::…`.
    ///
    /// On a third platform there is neither, and every method is a no-op that
    /// reports absence.
    pub struct Rings {
        #[cfg(windows)]
        session: Mutex<Option<crate::halbridge_win::session::Session>>,
        /// Moved out of the session at attach, so nothing on the audio path
        /// ever takes the session mutex.
        ///
        /// Safe to declare AFTER `session` — i.e. to be dropped after the
        /// control handle closes — only because `WinRings` has no `Drop` that
        /// touches the pages; the driver unmaps them itself in
        /// `IRP_MJ_CLEANUP`. Give it one and this order becomes a
        /// use-after-free, and `detach` below is the place that already gets
        /// the sequence right.
        #[cfg(windows)]
        rings: crate::halbridge_win::rings::WinRings,
    }

    #[cfg(windows)]
    impl Rings {
        pub fn new() -> Rings {
            Rings {
                session: Mutex::new(None),
                rings: crate::halbridge_win::rings::WinRings::new(),
            }
        }
        pub fn read_spk(&self, slot: usize, dst: &mut [f32], frames: usize) -> usize {
            self.rings.read_spk(slot, dst, frames)
        }
        pub fn write_mic(&self, slot: usize, mono: &[f32]) -> Option<usize> {
            self.rings.write_mic(slot, mono)
        }
        pub fn flush_spk_consumer(&self, slot: usize) {
            self.rings.flush_spk_consumer(slot)
        }
        pub fn peek_spk(
            &self,
            slot: usize,
            dst: &mut [f32],
            frames: usize,
        ) -> Option<(usize, u64)> {
            self.rings.peek_spk(slot, dst, frames)
        }
        pub fn advance_spk(&self, slot: usize, base: u64, frames: usize) {
            self.rings.advance_spk(slot, base, frames)
        }
        pub fn drop_spk(&self, slot: usize, frames: usize) -> usize {
            self.rings.drop_spk(slot, frames)
        }
        pub fn spk_readable(&self, slot: usize) -> Option<(u32, u32)> {
            self.rings.spk_readable(slot)
        }
        pub fn mic_occupied(&self, slot: usize) -> Option<(u32, u32)> {
            self.rings.mic_occupied(slot)
        }
    }

    #[cfg(not(windows))]
    impl Rings {
        pub fn new() -> Rings {
            Rings {}
        }
        pub fn read_spk(&self, _slot: usize, _dst: &mut [f32], _frames: usize) -> usize {
            0
        }
        /// Permanently "no driver attached" — see the macOS one.
        pub fn write_mic(&self, _slot: usize, _mono: &[f32]) -> Option<usize> {
            None
        }
        pub fn flush_spk_consumer(&self, _slot: usize) {}
        /// 同 `read_spk`：没有环就窥不到东西。
        pub fn peek_spk(
            &self,
            _slot: usize,
            _dst: &mut [f32],
            _frames: usize,
        ) -> Option<(usize, u64)> {
            None
        }
        pub fn advance_spk(&self, _slot: usize, _base: u64, _frames: usize) {}
        pub fn drop_spk(&self, _slot: usize, _frames: usize) -> usize {
            0
        }
        /// 没有环就没有深度。`None` 是「这一级不存在」，**不是 0 ms**
        /// （规格附录约束 1：绝不用 0 填补缺失分项）。
        pub fn spk_readable(&self, _slot: usize) -> Option<(u32, u32)> {
            None
        }
        /// 同上：没有环就没有这一级。
        pub fn mic_occupied(&self, _slot: usize) -> Option<(u32, u32)> {
            None
        }
    }

    // ------------------------------------------------------------ no driver

    #[cfg(not(windows))]
    pub fn start(cfg: HalBridgeCfg) -> Result<Option<HalBridge>> {
        if cfg.mode == HalBridgeMode::Require {
            anyhow::bail!("the HAL bridge needs macOS or Windows");
        }
        Ok(None)
    }

    #[cfg(not(windows))]
    pub fn send_notify(
        _shared: &Shared,
        _at: HalEndpoint,
        _generation: u32,
        _scalar: f32,
        _muted: bool,
    ) {
    }

    #[cfg(not(windows))]
    pub fn send_bind_set(_shared: &Shared, _req: &HalBindRequest) -> bool {
        false
    }

    #[cfg(not(windows))]
    pub fn send_bind_clear(_shared: &Shared, _slot: u8, _generation: u32) -> bool {
        false
    }

    // ------------------------------------------------------------- windows

    #[cfg(windows)]
    use crate::halbridge_win::{session::Session, wire};

    /// How often the service thread retries a missing driver and drains the
    /// inverted call. The macOS side is woken by mach; here there is nothing to
    /// block on until the data plane exists, so it polls.
    #[cfg(windows)]
    const SERVICE_TICK: Duration = Duration::from_millis(500);

    #[cfg(windows)]
    pub fn start(cfg: HalBridgeCfg) -> Result<Option<HalBridge>> {
        if cfg.mode == HalBridgeMode::Off {
            return Ok(None);
        }

        // The gate for `Auto`: can we open the control device right now? A
        // machine without the driver is the ordinary case (every CI box, every
        // Windows machine before installation) and must stay silent.
        let first = match Session::open() {
            Ok(s) => Some(s),
            Err(e) => {
                if cfg.mode != HalBridgeMode::Require {
                    return Ok(None);
                }
                dlog!("[audiohubd] HAL bridge: {} ; will keep looking", e.text());
                None
            }
        };

        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            driver_found: AtomicBool::new(first.is_some()),
            driver_connected: AtomicBool::new(false),
            slots: std::array::from_fn(|_| SlotShared::new()),
            last_driver_msg: Mutex::new(None),
            events: Mutex::new(Vec::new()),
            spk_flush: AtomicU16::new(0),
            published: AtomicU16::new(0),
            session_id: AtomicU64::new(0),
            attach_epoch: AtomicU64::new(0),
            slot_count: AtomicU32::new(0),
            driver_protocol: AtomicU32::new(0),
            status_reason: Mutex::new(Some("no driver attached yet".to_string())),
            bind_failures: AtomicU64::new(0),
            last_bind_error: Mutex::new(None),
            endpoint_name_fallbacks: AtomicU64::new(0),
            rings: Rings::new(),
            driver_port: Mutex::new(0),
            superseded: AtomicBool::new(false),
            tick_punctual: AtomicBool::new(true),
        });

        if let Some(s) = first {
            attach(&shared, s);
        }

        let s = shared.clone();
        let thread = std::thread::Builder::new()
            .name("ahb-halbridge".to_string())
            .spawn(move || service_loop(s))?;

        Ok(Some(HalBridge { shared, thread: Mutex::new(Some(thread)) }))
    }

    /// Publishes a freshly handshaken session. Bumping `attach_epoch` is what
    /// makes the device coordinator re-`Set` every binding it still intends —
    /// the driver keeps its bindings across a daemon restart, but none of them
    /// are acknowledged to THIS daemon any more.
    #[cfg(windows)]
    fn attach(shared: &Arc<Shared>, s: Session) {
        let (id, slots, protocol, check) =
            (s.session_id, s.slot_count, s.driver_protocol, s.client_check);

        if s.identity_check_degraded() {
            // Loud on purpose. A caller-identity check that silently degraded
            // to "the ACL let you in" is indistinguishable from no check, and
            // the whole point of reporting the level is that the weaker one is
            // visible rather than assumed.
            dlog!(
                "[audiohubd] hal: the driver is enforcing client_check={check} (ACL only). \
                 Set AudioHubDaemonImage in the device software key to pin the daemon's \
                 image path."
            );
        }

        if let Some(why) = s.dataplane_off.as_deref() {
            // Loud, and for the same reason the degraded identity check above
            // is: a bridge that publishes devices which then carry no audio is
            // indistinguishable from a peer that is simply not sending, and the
            // difference is the whole diagnosis.
            dlog!("[audiohubd] hal: NO DATA PLANE — {why}");
        } else if !s.has_volume() {
            // The squared-attenuation trap: with no hardware volume node the
            // audio engine inserts a software volume APO ahead of the driver,
            // so the samples in the rings are ALREADY attenuated by the user's
            // slider. Relaying that same slider to the peer's real device
            // applies it a second time, and the only place the difference shows
            // is a level meter nobody is looking at.
            dlog!(
                "[audiohubd] hal: the driver has no volume node (AH_CAP_VOLUME is clear); \
                 the samples in the rings are pre-attenuated, so volume must not be synced"
            );
        }

        // The mapping moves out of the session and onto `Shared`, where the
        // mixer and the tx engine reach it WITHOUT taking the session mutex —
        // which an IOCTL can hold for its whole round trip.
        s.rings.move_into(&shared.rings.rings);

        *lk(&shared.rings.session) = Some(s);
        shared.session_id.store(id, Ordering::SeqCst);
        shared.slot_count.store(slots as u32, Ordering::SeqCst);
        shared.driver_protocol.store(protocol, Ordering::Relaxed);
        shared.driver_found.store(true, Ordering::Relaxed);
        shared.driver_connected.store(true, Ordering::Relaxed);
        *lk(&shared.status_reason) = None;
        *lk(&shared.last_driver_msg) = Some(Instant::now());
        shared.attach_epoch.fetch_add(1, Ordering::AcqRel);
        shared.push_event(HalControlEvent::Attached { session_id: id, slot_count: slots });
        dlog!("[audiohubd] hal: attached, session {id}, {slots} slots, client_check {check}");
    }

    /// Drops the session. The DRIVER's bindings survive this — a daemon that
    /// died is not a peer that was unpaired, and plan §7.3 keeps a paired
    /// peer's devices in the system list either way.
    #[cfg(windows)]
    fn detach(shared: &Shared, why: &str) {
        // THE RINGS GO FIRST, before anything can drop the session and with it
        // the control handle. The mapping is torn down by the driver in
        // IRP_MJ_CLEANUP — i.e. the instant `CloseHandle` runs — so the mixer
        // and the tx engine have to be out of the pages by then. Taking
        // `WinRings`'s write lock is exactly that wait. Reverse these two and
        // a driver restart during playback is a segfault in whichever thread
        // happened to be mid-memcpy.
        shared.rings.rings.detach();
        if lk(&shared.rings.session).take().is_none() {
            return;
        }
        shared.driver_connected.store(false, Ordering::Relaxed);
        shared.slot_count.store(0, Ordering::SeqCst);
        shared.session_id.store(0, Ordering::SeqCst);
        *lk(&shared.status_reason) = Some(why.to_string());
        shared.push_event(HalControlEvent::Detached);
        dlog!("[audiohubd] hal: detached ({why})");
    }

    #[cfg(windows)]
    fn service_loop(shared: Arc<Shared>) {
        while !shared.stop.load(Ordering::SeqCst) {
            let connected = lk(&shared.rings.session).is_some();

            if !connected {
                match Session::open() {
                    Ok(s) => attach(&shared, s),
                    Err(e) => {
                        shared.driver_found.store(e.driver_present(), Ordering::Relaxed);
                        shared
                            .driver_protocol
                            .store(e.driver_protocol().unwrap_or(0), Ordering::Relaxed);
                        *lk(&shared.status_reason) = Some(e.text());
                    }
                }
            } else {
                // Drain whatever the driver pushed. Empty through M6-2 — there
                // is no volume node and no data plane — but the loop is here so
                // that adding either is a driver-side change only.
                let events = {
                    let mut g = lk(&shared.rings.session);
                    g.as_mut().map(|s| s.poll_events()).unwrap_or_default()
                };
                if !events.is_empty() {
                    *lk(&shared.last_driver_msg) = Some(Instant::now());
                }
                for ev in events {
                    if let Some(mapped) = map_event(&shared, ev) {
                        shared.push_event(mapped);
                    }
                }
            }

            std::thread::sleep(SERVICE_TICK);
        }

        detach(&shared, "daemon shutting down");
    }

    /// Wire event -> bridge event, dropping anything about a slot the driver
    /// cannot legitimately be talking about.
    #[cfg(windows)]
    fn map_event(shared: &Shared, ev: wire::ControlEvent) -> Option<HalControlEvent> {
        let slot = u8::try_from(ev.slot).ok()?;
        if slot as usize >= HAL_MAX_SLOTS {
            return None;
        }
        let at = if ev.input() { HalEndpoint::mic(slot) } else { HalEndpoint::out(slot) };
        match ev.kind {
            wire::EVENT_VOLUME => Some(HalControlEvent::Volume {
                at,
                generation: ev.generation,
                scalar: ev.scalar(),
                muted: ev.muted(),
            }),
            wire::EVENT_IOSTATE => {
                Some(HalControlEvent::IoState { at, generation: ev.generation, running: ev.running() })
            }
            wire::EVENT_SLOT => {
                let state = match ev.state {
                    wire::SLOT_FREE => HalSlotState::Free,
                    wire::SLOT_BOUND => HalSlotState::Bound,
                    wire::SLOT_DELISTED => HalSlotState::Delisted,
                    _ => return None,
                };
                shared.arm_flush(slot);
                Some(HalControlEvent::BindState { slot, generation: ev.generation, state })
            }
            _ => None,
        }
    }

    /// Relays the far peer's real device level onto the virtual endpoint's
    /// volume node, so the two sliders read the same number.
    ///
    /// A driver with `AH_CAP_VOLUME` CLEAR is skipped rather than told: it has
    /// no node to move, and the audio engine has already applied the user's
    /// setting in a software APO upstream of the ring. Sending anyway would be
    /// asking for the setting to be applied twice.
    ///
    /// Failures are logged, never propagated. This is a best-effort follow of
    /// somebody else's slider; it must not be able to tear down a session that
    /// is otherwise carrying audio perfectly well.
    #[cfg(windows)]
    pub fn send_notify(
        shared: &Shared,
        at: HalEndpoint,
        generation: u32,
        scalar: f32,
        muted: bool,
    ) {
        let guard = lk(&shared.rings.session);
        let Some(s) = guard.as_ref() else { return };
        if !s.has_volume() {
            return;
        }
        if let Err(e) = s.notify(at.slot, generation, at.input, muted, scalar) {
            dlog!(
                "[audiohubd] hal: slot {} volume notify failed: {e:#}",
                at.slot
            );
        }
    }

    #[cfg(windows)]
    pub fn send_bind_set(shared: &Shared, req: &HalBindRequest) -> bool {
        // `display`, not `out_name`/`in_name`: those two already carry the
        // direction suffix, which on Windows is the driver's to append.
        //
        // And `display` goes across UNMODIFIED. The prefix is composed inside
        // wire::encode_bind_request, through the same haldev helper the macOS
        // names are built with. Composing it here instead is what M6-2 did —
        // by leaving it out — and the result was every endpoint labelled with
        // a bare host name. There is deliberately nothing at this call site
        // left to get wrong.
        bind_call(shared, req.slot, true, |s| {
            s.bind_set(req.slot, &req.peer_key, &req.display, req.online)
        })
    }

    #[cfg(windows)]
    pub fn send_bind_clear(shared: &Shared, slot: u8, generation: u32) -> bool {
        bind_call(shared, slot, false, |s| s.bind_clear(slot, generation))
    }

    /// Runs one bind IOCTL and turns the reply into the same `BindState` the
    /// macOS driver pushes asynchronously.
    ///
    /// "The IOCTL succeeded" is NOT "the device appeared" — that distinction is
    /// carried by the reply's own status field, and a non-OK status is reported
    /// as failure so the coordinator retries on its next pass.
    ///
    /// `is_set` splits the two operations because their success invariants are
    /// different and BOTH are checked here: a SET that returns OK must have
    /// published `PUB_BOTH`, a CLEAR that returns OK must have published
    /// nothing. The driver checks the same thing; this side checks it again
    /// because the whole class of defect being guarded against is "the driver
    /// said OK and it was not true", and a guard that lives only inside the
    /// thing it is guarding cannot catch that.
    #[cfg(windows)]
    fn bind_call<F>(shared: &Shared, slot: u8, is_set: bool, f: F) -> bool
    where
        F: FnOnce(&Session) -> Result<wire::BindReply>,
    {
        let guard = lk(&shared.rings.session);
        let Some(s) = guard.as_ref() else { return false };

        let reply = match f(s) {
            Ok(r) => r,
            Err(e) => {
                // A failed IOCTL means the handle is no longer usable in any
                // way we can distinguish, so the session goes. Dropping it
                // inside the guard, then reporting, keeps the two consistent.
                drop(guard);
                detach(shared, &format!("the driver stopped answering: {e:#}"));
                return false;
            }
        };
        drop(guard);

        if let Err(what) = wire::bind_outcome(is_set, &reply) {
            let op = if is_set { "bind" } else { "unbind" };
            dlog!("[audiohubd] hal: slot {slot} {op} failed: {what}");
            shared.bind_failures.fetch_add(1, Ordering::Relaxed);
            *lk(&shared.last_bind_error) = Some(format!("slot {slot}: {op} failed: {what}"));
            return false;
        }
        *lk(&shared.last_bind_error) = None;

        if reply.endpoint_name_fell_back() {
            // Not a failure: the devices are published and usable. But with
            // more than one peer paired it means two speakers with the same
            // label, so it is counted and logged rather than absorbed — the
            // whole point of the v2/v3 reply fields is that the driver never
            // gets to answer OK and leave part of the truth out.
            shared.endpoint_name_fallbacks.fetch_add(1, Ordering::Relaxed);
            dlog!(
                "[audiohubd] hal: slot {slot} bound, but the per-peer device name could \
                 not be applied; the endpoints carry the generic direction names"
            );
        }

        let state = match reply.state {
            wire::SLOT_BOUND => HalSlotState::Bound,
            wire::SLOT_DELISTED => HalSlotState::Delisted,
            _ => HalSlotState::Free,
        };
        if state != HalSlotState::Bound {
            // A retired slot may go to another peer, and the daemon's own read
            // index into that slot's ring would otherwise replay the previous
            // tenant's audio to the next one. Harmless while there is no data
            // plane; wrong the moment there is.
            shared.arm_flush(slot);
        }
        shared.push_event(HalControlEvent::BindState {
            slot,
            generation: reply.generation,
            state,
        });
        *lk(&shared.last_driver_msg) = Some(Instant::now());
        true
    }
}
