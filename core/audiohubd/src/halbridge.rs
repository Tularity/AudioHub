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

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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

const MSG_HELLO: i32 = 0x4148_0001; // daemon -> driver, carries our control port only
const MSG_HELLO_REPLY: i32 = 0x4148_0002; // driver -> daemon, carries both memory entries
const MSG_CONTROL: i32 = 0x4148_0003; // driver -> daemon, fire and forget
const MSG_NOTIFY: i32 = 0x4148_0004; // daemon -> driver, fire and forget

/// `kAudioHubProtocolVersion` in AudioHubBridge.h. The driver compares this for
/// EQUALITY and answers `kAudioHubStatus_BadVersion` on anything else, so it is
/// not a floor to be raised unilaterally — it changes only when that header
/// changes. (It was briefly 2 here alone, which the driver could only refuse.)
const PROTOCOL_VERSION: u32 = 1;

const CTL_VOLUME: u32 = 1;
const CTL_HEARTBEAT: u32 = 2;
const CTL_IO_STATE: u32 = 3;
/// The driver accepted another daemon's HELLO and is about to drop our port.
/// Sent as the LAST message on it, best-effort. Handled by detaching at once
/// instead of waiting out `DRIVER_SILENT_AFTER` — and by NOT reconnecting
/// immediately, because an instant re-HELLO displaces whoever displaced us and
/// the two daemons then trade the rings every few seconds forever.
const CTL_SUPERSEDED: u32 = 4;

const NOTIFY_VOLUME: u32 = 1;
const NOTIFY_PING: u32 = 2;

const DEV_SPEAKER: u32 = 0;
const DEV_MIC: u32 = 1;

const STATUS_OK: u32 = 0;
const STATUS_BAD_VERSION: u32 = 1;
const STATUS_NO_MEMORY: u32 = 2;

const FLAG_MUTED: u32 = 0x1;
const FLAG_IO_RUNNING: u32 = 0x2;

// ---------------------------------------------------------------- public API

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HalDevice {
    Speaker,
    Microphone,
}

impl HalDevice {
    fn from_wire(v: u32) -> HalDevice {
        if v == DEV_MIC {
            HalDevice::Microphone
        } else {
            HalDevice::Speaker
        }
    }
    fn to_wire(self) -> u32 {
        match self {
            HalDevice::Speaker => DEV_SPEAKER,
            HalDevice::Microphone => DEV_MIC,
        }
    }
}

/// What the driver tells us about its virtual devices (spec-round2 §B2).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HalControlEvent {
    /// The local user moved the virtual device's slider. The daemon must relay
    /// this to the peer's REAL device via the existing `VolumeSet` path.
    Volume { device: HalDevice, scalar: f32, muted: bool },
    /// An application started/stopped using the virtual device.
    IoState { device: HalDevice, running: bool },
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

#[derive(Debug, Clone, Copy, serde::Serialize)]
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
    /// Speaker-direction frames handed to the media engine.
    pub spk_frames: u64,
    /// Microphone-direction frames accepted by the ring.
    pub mic_frames: u64,
    /// Microphone frames the ring had no room for (driver not draining).
    pub mic_dropped: u64,
    /// Seconds since the last message from the driver, if it ever spoke.
    pub last_driver_msg_secs: Option<f64>,
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
        HalBridgeStatus {
            driver_found: s.driver_found.load(Ordering::Relaxed),
            driver_connected: s.driver_connected.load(Ordering::Relaxed),
            spk_frames: s.spk_frames.load(Ordering::Relaxed),
            mic_frames: s.mic_frames.load(Ordering::Relaxed),
            mic_dropped: s.mic_dropped.load(Ordering::Relaxed),
            last_driver_msg_secs: last.map(|t| t.elapsed().as_secs_f64()),
        }
    }

    /// APPENDS one 10ms mono frame of whatever an app played into "AudioHub
    /// Speaker", padding with silence so exactly `HAL_FRAME_48K` samples are
    /// added: a missing or idle driver produces silence, never a stall or a
    /// short frame. Returns how many of those samples were real.
    ///
    /// The name says `append` because the previous one (`read_spk_frame`) read
    /// as "replace" and a caller took it that way — `FrameSource::next_frame`
    /// must replace, the engine truncated the over-long frame back to its first
    /// 480 samples, and the peer received the silence captured before any app
    /// had played anything, forever, with every counter and probe still green.
    pub fn append_spk_frame(&self, out: &mut Vec<f32>) -> usize {
        self.shared.append_spk_frame(out)
    }

    /// Appends up to `max_frames` mono samples; returns how many were real.
    pub fn read_spk_mono(&self, out: &mut Vec<f32>, max_frames: usize) -> usize {
        let mut done = 0;
        while done < max_frames {
            let want = (max_frames - done).min(HAL_FRAME_48K);
            let got = self.shared.read_spk_chunk(out, want);
            done += got;
            if got < want {
                break;
            }
        }
        done
    }

    /// Peer microphone audio for "AudioHub Microphone". Returns the number of
    /// samples the ring accepted; the remainder is dropped rather than queued,
    /// because a driver that is not draining is a driver nobody is listening to.
    pub fn write_mic_mono(&self, mono: &[f32]) -> usize {
        self.shared.write_mic(mono)
    }

    /// Volume/mute and IO-state changes the driver has reported since the last
    /// call. Never blocks.
    pub fn drain_events(&self) -> Vec<HalControlEvent> {
        let mut q = lk(&self.shared.events);
        std::mem::take(&mut *q)
    }

    /// Reverse direction of plan §7.2: the peer's real device reported a new
    /// volume, so the virtual control must show it. Best effort — a driver that
    /// is not attached simply misses it and re-reads on its next handshake.
    pub fn notify_volume(&self, device: HalDevice, scalar: f32, muted: bool) {
        self.shared.notify_volume(device, scalar, muted);
    }

    pub fn shutdown(&self) {
        self.shared.stop.store(true, Ordering::SeqCst);
    }
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
    dbg_peak: f32,
    dbg_frames: u32,
}

impl HalSpeakerSource {
    pub fn new(bridge: &HalBridge) -> HalSpeakerSource {
        HalSpeakerSource { bridge: bridge.shared.clone(), dbg_peak: 0.0, dbg_frames: 0 }
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
        self.bridge.append_spk_frame(out);
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
    fn sample_rate(&self) -> u32 {
        HAL_SAMPLE_RATE
    }
}

// ---------------------------------------------------------------- shared state

struct Shared {
    stop: AtomicBool,
    driver_found: AtomicBool,
    driver_connected: AtomicBool,
    spk_frames: AtomicU64,
    mic_frames: AtomicU64,
    mic_dropped: AtomicU64,
    last_driver_msg: Mutex<Option<Instant>>,
    events: Mutex<Vec<HalControlEvent>>,
    /// Set by the service thread on every handshake; consumed by whichever
    /// daemon thread reads spk next. Only the consumer may move `read_idx`, so
    /// the flush has to happen here rather than on the service thread.
    spk_flush: AtomicBool,
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

    fn append_spk_frame(&self, out: &mut Vec<f32>) -> usize {
        let got = self.read_spk_chunk(out, HAL_FRAME_48K);
        if got < HAL_FRAME_48K {
            out.resize(out.len() + (HAL_FRAME_48K - got), 0.0);
        }
        got
    }

    /// Appends at most `frames` (<= HAL_FRAME_48K) mono samples.
    fn read_spk_chunk(&self, out: &mut Vec<f32>, frames: usize) -> usize {
        let frames = frames.min(HAL_FRAME_48K);
        if self.spk_flush.swap(false, Ordering::AcqRel) {
            self.rings.flush_spk_consumer();
        }
        let mut scratch = [0.0f32; HAL_FRAME_48K * (HAL_SPK_CHANNELS as usize)];
        let got = self.rings.read_spk(&mut scratch[..frames * HAL_SPK_CHANNELS as usize], frames);
        for f in 0..got {
            let l = scratch[f * 2];
            let r = scratch[f * 2 + 1];
            out.push((l + r) * 0.5);
        }
        if got > 0 {
            self.spk_frames.fetch_add(got as u64, Ordering::Relaxed);
        }
        got
    }

    fn write_mic(&self, mono: &[f32]) -> usize {
        // `None` is "no driver attached, so there is no ring": nothing was
        // accepted, but nothing was DROPPED either. mic_dropped means "the
        // driver is not draining", and counting an absent driver into it would
        // bury the only reading of that number that diagnoses anything.
        let Some(wrote) = self.rings.write_mic(mono) else {
            return 0;
        };
        self.mic_frames.fetch_add(wrote as u64, Ordering::Relaxed);
        if wrote < mono.len() {
            self.mic_dropped
                .fetch_add((mono.len() - wrote) as u64, Ordering::Relaxed);
        }
        wrote
    }

    fn notify_volume(&self, device: HalDevice, scalar: f32, muted: bool) {
        platform::send_notify(self, device, scalar, muted);
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
    /// Complex with exactly two port descriptors ONLY when `status` is OK; on
    /// any other status it is a PLAIN message whose descriptor words are zero.
    /// So `spk_entry`/`mic_entry` are meaningless until both the COMPLEX bit
    /// and `status` have been tested — reading them first would hand
    /// `mach_vm_map` a port name that was never received.
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct HelloReply {
        header: MsgHeader,
        body: MsgBody,
        spk_entry: PortDescriptor, // driver writes, daemon reads
        mic_entry: PortDescriptor, // daemon writes, driver reads
        status: u32,
        protocol_version: u32,
        data_offset: u32,
        spk_capacity_frames: u32,
        spk_channels: u32,
        spk_sample_rate: u32,
        mic_capacity_frames: u32,
        mic_channels: u32,
        mic_sample_rate: u32,
        // nine u32 from offset 52 land the u64 pair on 88 with no padding
        spk_bytes: u64,
        mic_bytes: u64,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct ControlMsg {
        header: MsgHeader,
        op: u32,
        device: u32,
        scalar_bits: u32,
        flags: u32,
        seq: u64,
    }

    // Mirrors the _Static_asserts in AudioHubBridge.h one for one. A drift here
    // is a struct read at the wrong offsets on the far side of a mach message,
    // so it has to fail the build, not the audio.
    const _: () = {
        assert!(std::mem::size_of::<MsgHeader>() == 24);
        assert!(std::mem::size_of::<PortDescriptor>() == 12);
        assert!(std::mem::offset_of!(HelloRequest, protocol_version) == 40);
        assert!(std::mem::size_of::<HelloRequest>() == 48);
        assert!(std::mem::offset_of!(HelloReply, status) == 52);
        assert!(std::mem::offset_of!(HelloReply, spk_bytes) == 88);
        assert!(std::mem::size_of::<HelloReply>() == 104);
        assert!(std::mem::offset_of!(ControlMsg, seq) == 40);
        assert!(std::mem::size_of::<ControlMsg>() == 48);
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

    /// The two mappings, present only while a driver is attached.
    ///
    /// The lock is NOT what makes the rings safe to use concurrently — the
    /// free-running SPSC indices do that, and both audio callers take it shared.
    /// It exists because this side unmaps on disconnect: a detach has to wait
    /// for the tx engine and the mixer to be out of the pages before
    /// `mach_vm_deallocate` runs, or a reconnect during playback is a segfault.
    /// The driver has no such lock and needs none; it never unmaps.
    pub struct Rings {
        inner: std::sync::RwLock<Option<RingPair>>,
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

        /// 0 with no driver attached: the caller zero-fills, so a missing
        /// driver is silence rather than a stall.
        pub fn read_spk(&self, dst: &mut [f32], frames: usize) -> usize {
            match rd(&self.inner).as_ref() {
                Some(p) => p.spk.read(dst, frames),
                None => 0,
            }
        }

        /// `None` distinguishes "no ring at all" from "the ring was full",
        /// which are the same number of frames accepted but very different
        /// diagnoses. See `Shared::write_mic`.
        pub fn write_mic(&self, mono: &[f32]) -> Option<usize> {
            rd(&self.inner).as_ref().map(|p| p.mic.write(mono, mono.len()))
        }

        pub fn flush_spk_consumer(&self) {
            if let Some(p) = rd(&self.inner).as_ref() {
                p.spk.flush_consumer();
            }
        }

        /// Installs a freshly handshaked pair. Dropping whatever was there
        /// unmaps it, and the write lock is what makes that safe.
        fn attach(&self, spk: RingMem, mic: RingMem) {
            *wr(&self.inner) = Some(RingPair { spk, mic });
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
            spk_frames: AtomicU64::new(0),
            mic_frames: AtomicU64::new(0),
            mic_dropped: AtomicU64::new(0),
            last_driver_msg: Mutex::new(None),
            events: Mutex::new(Vec::new()),
            spk_flush: AtomicBool::new(false),
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
    /// Aligned to 8 because `ControlMsg` carries a u64 at offset 40 and reading
    /// it out of a 1-aligned `[u8; N]` would be undefined behaviour.
    const MAX_TRAILER: usize = 68;
    const RCV_BUF: usize = 256;

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
        // a handshake into a silent timeout.
        const _: () = assert!(RCV_BUF >= std::mem::size_of::<HelloReply>() + MAX_TRAILER);
        const _: () = assert!(RCV_BUF >= std::mem::size_of::<ControlMsg>() + MAX_TRAILER);
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
                    let (kr, port) = send_to_driver(&shared, NOTIFY_PING, 0, 0.0, 0);
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
        // Outside the lock: detach unmaps both rings and blocks until the tx
        // engine and the mixer are out of them, and it must not do that while
        // holding a lock the sending threads also take. From here read_spk
        // yields silence and write_mic reports "no ring" — driverless behaviour.
        shared.rings.detach();
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
            anyhow::bail!("unexpected reply id {id:#x} size {size}");
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
            let why = match rep.status {
                STATUS_BAD_VERSION => format!(
                    "protocol mismatch: it speaks v{}, we speak v{PROTOCOL_VERSION}",
                    rep.protocol_version
                ),
                STATUS_NO_MEMORY => "it could not create the shared rings".to_string(),
                s => format!("status {s}"),
            };
            anyhow::bail!("driver refused the handshake: {why}");
        }
        if !complex || rep.body.descriptor_count != 2 {
            unsafe { mach_msg_destroy(hdr) };
            anyhow::bail!(
                "driver reported OK but sent {} descriptors (complex={complex}); expected the two ring entries",
                rep.body.descriptor_count
            );
        }

        // From here the two entry names are ours, so disposal moves from
        // mach_msg_destroy to the RingMem that owns each one.
        let spk = RingMem::attach(
            rep.spk_entry.name,
            RingGeom {
                channels: rep.spk_channels,
                sample_rate: rep.spk_sample_rate,
                capacity_frames: rep.spk_capacity_frames,
                data_offset: rep.data_offset,
                bytes: rep.spk_bytes,
            },
            HAL_SPK_CHANNELS,
            "speaker",
        );
        let mic = RingMem::attach(
            rep.mic_entry.name,
            RingGeom {
                channels: rep.mic_channels,
                sample_rate: rep.mic_sample_rate,
                capacity_frames: rep.mic_capacity_frames,
                data_offset: rep.data_offset,
                bytes: rep.mic_bytes,
            },
            HAL_MIC_CHANNELS,
            "microphone",
        );
        // Both attaches are ATTEMPTED before either error propagates: taking
        // ownership of one entry and returning early on the other would leak a
        // mach port every time a mismatched driver retried.
        let spk = spk?;
        let mic = mic?;

        // The driver rewound both rings before it replied and publishes them
        // only after, so nothing stale can be in there. The flush is still
        // armed for the gap between here and the tx engine's first read, which
        // is unbounded — a session opened a minute later must start at live
        // audio, not half a second behind it. Only the consumer may move
        // read_idx, hence a flag for the reading thread rather than a call.
        // Attach BEFORE arming the flush. The other order has a window in which
        // a tx tick swaps the flag to false and flushes a ring that is still
        // None — the flag is consumed, the backlog is never dropped, and the
        // session keeps the extra latency the flush exists to remove. This way
        // at worst one tick reads before the flush and the next one performs it.
        shared.rings.attach(spk, mic);
        shared.spk_flush.store(true, Ordering::Release);
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
            CTL_VOLUME => {
                let scalar = f32::from_bits(msg.scalar_bits);
                if !scalar.is_finite() {
                    return;
                }
                shared.push_event(HalControlEvent::Volume {
                    device: HalDevice::from_wire(msg.device),
                    scalar: scalar.clamp(0.0, 1.0),
                    muted: msg.flags & FLAG_MUTED != 0,
                });
            }
            CTL_IO_STATE => shared.push_event(HalControlEvent::IoState {
                device: HalDevice::from_wire(msg.device),
                running: msg.flags & FLAG_IO_RUNNING != 0,
            }),
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
        device: u32,
        scalar: f32,
        flags: u32,
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
            device,
            scalar_bits: scalar.to_bits(),
            flags,
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

    pub fn send_notify(shared: &Shared, device: HalDevice, scalar: f32, muted: bool) {
        let (kr, port) = send_to_driver(
            shared,
            NOTIFY_VOLUME,
            device.to_wire(),
            scalar.clamp(0.0, 1.0),
            if muted { FLAG_MUTED } else { 0 },
        );
        // A dead port is definitive; a full queue is not. Only the former ends
        // the session, and the service loop then goes back to looking up.
        if kr != MACH_MSG_SUCCESS && kr != MACH_SEND_TIMED_OUT {
            disconnect_port(shared, port, "volume relay found its port dead");
        }
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
            r.attach(attach_ring(&ds, HAL_SPK_CHANNELS), attach_ring(&dm, HAL_MIC_CHANNELS));
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
            assert_eq!(shared.append_spk_frame(&mut out), 3);
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
            assert_eq!(shared.write_mic(&mono), 3);
            assert_eq!(shared.mic_frames.load(Ordering::Relaxed), 3);
            assert_eq!(shared.mic_dropped.load(Ordering::Relaxed), 7);
        }

        /// With no driver there is no ring, and that is NOT a drop: mic_dropped
        /// is the "driver is not draining" number and burying it under every
        /// frame produced while nothing is attached makes it useless.
        #[test]
        fn a_detached_bridge_is_silent_and_counts_nothing() {
            let shared = test_shared(Rings::new());
            let mut out = Vec::new();
            assert_eq!(shared.append_spk_frame(&mut out), 0);
            assert_eq!(out.len(), HAL_FRAME_48K);
            assert!(out.iter().all(|s| *s == 0.0));
            assert_eq!(shared.write_mic(&[0.5; 64]), 0);
            assert_eq!(shared.mic_frames.load(Ordering::Relaxed), 0);
            assert_eq!(shared.mic_dropped.load(Ordering::Relaxed), 0, "no ring is not a drop");
        }

        fn test_shared(rings: Rings) -> Shared {
            Shared {
                stop: AtomicBool::new(false),
                driver_found: AtomicBool::new(false),
                driver_connected: AtomicBool::new(false),
                spk_frames: AtomicU64::new(0),
                mic_frames: AtomicU64::new(0),
                mic_dropped: AtomicU64::new(0),
                last_driver_msg: Mutex::new(None),
                events: Mutex::new(Vec::new()),
                spk_flush: AtomicBool::new(false),
                rings,
                driver_port: Mutex::new(MACH_PORT_NULL),
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
                rings.attach(attach_ring(&ds, HAL_SPK_CHANNELS), attach_ring(&dm, HAL_MIC_CHANNELS));
                assert!(rings.attached());

                // this generation's "driver" plays a tone the daemon must hear
                let spk = attach_ring(&ds, HAL_SPK_CHANNELS);
                let v = gen as f32;
                assert_eq!(spk.write(&[v, v, v, v], 2), 2);
                let mut out = [0.0f32; 4];
                assert_eq!(rings.read_spk(&mut out, 2), 2);
                heard.push(out[0]);
                // ...and the daemon's mic writes reach this generation's driver
                assert_eq!(rings.write_mic(&[v; 8]), Some(8));
                let mut got = [0.0f32; 8];
                assert_eq!(dm.hdr().write_idx.load(Ordering::Relaxed), 8);
                assert_eq!(attach_ring(&dm, HAL_MIC_CHANNELS).read(&mut got, 8), 8);
                assert_eq!(got[0], v);

                rings.detach();
                assert!(!rings.attached());
                assert_eq!(rings.read_spk(&mut out, 2), 0, "a detached ring is silence");
                assert_eq!(rings.write_mic(&[0.5; 8]), None, "not 'full' — absent");
            }
            assert_eq!(heard, vec![1.0, 2.0, 3.0], "each session heard its own driver");
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
            let (kr, port) = send_to_driver(&shared, NOTIFY_PING, 0, 0.0, 0);
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
            shared.notify_volume(HalDevice::Speaker, 0.5, false);
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
            assert_eq!(b.append_spk_frame(&mut out), 0);
            assert_eq!(out.len(), HAL_FRAME_48K);
            assert!(out.iter().all(|s| *s == 0.0));
            // ...and mic writes are swallowed without an error and without
            // running the drop counter up: there is no ring to be full.
            assert_eq!(b.write_mic_mono(&[0.25; 64]), 0);
            assert_eq!(b.status().mic_dropped, 0);
            assert!(b.drain_events().is_empty());
            b.notify_volume(HalDevice::Speaker, 0.4, false); // no driver: a no-op

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
        pub fn read_spk(&self, _dst: &mut [f32], _frames: usize) -> usize {
            0
        }
        /// Permanently "no driver attached" — see the macOS one.
        pub fn write_mic(&self, _mono: &[f32]) -> Option<usize> {
            None
        }
        pub fn flush_spk_consumer(&self) {}
    }

    pub fn start(cfg: HalBridgeCfg) -> Result<Option<HalBridge>> {
        if cfg.mode == HalBridgeMode::Require {
            anyhow::bail!("the HAL bridge is macOS-only");
        }
        Ok(None)
    }

    pub fn send_notify(_shared: &Shared, _device: HalDevice, _scalar: f32, _muted: bool) {}
}
