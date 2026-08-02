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

/// Per-slot traffic and state, summed into the three headline counters and
/// reported per slot beside them (spec-m5b §6.1).
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct HalSlotCounters {
    pub spk_frames: u64,
    pub mic_frames: u64,
    pub mic_dropped: u64,
    pub generation: u32,
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
    /// Per-slot detail, indexed by slot.
    pub slots: Vec<HalSlotCounters>,
}

pub struct HalBridge {
    shared: Arc<Shared>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
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
            slots,
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
    /// Peer fingerprint, for the driver's logs and its idempotency compare.
    pub peer_key: String,
    pub out_uid: String,
    pub in_uid: String,
    pub out_name: String,
    pub in_name: String,
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

/// Plugs the speaker direction into the media engine. `SourceSpec` needs a new
/// variant and `build_source` a new arm; both live in engine.rs, which this
/// group does not own.
pub struct HalSpeakerSource {
    bridge: Arc<Shared>,
    slot: u8,
    dbg_peak: f32,
    dbg_frames: u32,
}

impl HalSpeakerSource {
    /// One source per SLOT. The tx engine dedups sources by `SourceSpec`, and
    /// `HalSpeaker { slot }` makes two slots two distinct keys — which is what
    /// keeps each speaker ring to exactly one consumer and the SPSC contract
    /// literally true (spec-m5b §5.4).
    pub fn new(bridge: &HalBridge, slot: u8) -> HalSpeakerSource {
        HalSpeakerSource { bridge: bridge.shared.clone(), slot, dbg_peak: 0.0, dbg_frames: 0 }
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
        self.bridge.append_spk_frame(self.slot, out);
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
}

impl SlotShared {
    fn new() -> SlotShared {
        SlotShared {
            spk_frames: AtomicU64::new(0),
            mic_frames: AtomicU64::new(0),
            mic_dropped: AtomicU64::new(0),
            generation: AtomicU32::new(0),
        }
    }

    fn snapshot(&self) -> HalSlotCounters {
        HalSlotCounters {
            spk_frames: self.spk_frames.load(Ordering::Relaxed),
            mic_frames: self.mic_frames.load(Ordering::Relaxed),
            mic_dropped: self.mic_dropped.load(Ordering::Relaxed),
            generation: self.generation.load(Ordering::Relaxed),
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
    rings: platform::Rings,
    /// Send right on the driver's service port, 0 when no driver is attached.
    /// A mutex rather than an atomic because two threads send on it (the
    /// service loop's ping and the daemon's volume relay) and a lost race
    /// would mean sending on a name someone else just deallocated.
    driver_port: Mutex<u32>,
    /// The driver told us another daemon took the rings (`CTL_SUPERSEDED`).
    /// Set by the receive path, acted on by the service loop.
    superseded: AtomicBool,
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
            rings: Rings::new(),
            driver_port: Mutex::new(MACH_PORT_NULL),
            superseded: AtomicBool::new(false),
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

        fn spk_source(shared: &Arc<Shared>, slot: u8) -> HalSpeakerSource {
            HalSpeakerSource {
                bridge: shared.clone(),
                slot,
                dbg_peak: 0.0,
                dbg_frames: 0,
            }
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
                rings,
                driver_port: Mutex::new(MACH_PORT_NULL),
                superseded: AtomicBool::new(false),
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

    /// There is no HAL to bridge to; the type exists so the rest of the module
    /// compiles unchanged on Windows.
    pub struct Rings;

    impl Rings {
        pub fn new() -> Rings {
            Rings
        }
        pub fn read_spk(&self, _slot: usize, _dst: &mut [f32], _frames: usize) -> usize {
            0
        }
        /// Permanently "no driver attached" — see the macOS one.
        pub fn write_mic(&self, _slot: usize, _mono: &[f32]) -> Option<usize> {
            None
        }
        pub fn flush_spk_consumer(&self, _slot: usize) {}
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

    pub fn start(cfg: HalBridgeCfg) -> Result<Option<HalBridge>> {
        if cfg.mode == HalBridgeMode::Require {
            anyhow::bail!("the HAL bridge is macOS-only");
        }
        Ok(None)
    }

    pub fn send_notify(
        _shared: &Shared,
        _at: HalEndpoint,
        _generation: u32,
        _scalar: f32,
        _muted: bool,
    ) {
    }

    pub fn send_bind_set(_shared: &Shared, _req: &HalBindRequest) -> bool {
        false
    }

    pub fn send_bind_clear(_shared: &Shared, _slot: u8, _generation: u32) -> bool {
        false
    }
}
