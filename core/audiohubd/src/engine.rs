//! Media plane wiring: single shared UDP socket, 10ms send scheduler with
//! fan-out + AUTO resample-before-encode, receive/decrypt into jitter buffers,
//! 10ms mixer with soft clip and a 2s post-mix ring for mix_verdicts.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, VecDeque};
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};

use audiohub_core::audio::{self, AudioTx, LivePlayback};
use audiohub_core::dsp::{self, LinearResampler, ToneVerdict};
use audiohub_core::sysaudio::{self, SysAudioCapture};
use audiohub_net::media::{rung_rate, FrameSource, LossInjector, MediaCrypto, MicSource, ToneSource};
use audiohub_net::packet::{Codec, Header, Kind};

use crate::{dlog, lk, rd, DaemonInner, RxStream, TxShared};

const FRAME_MS: u64 = 10;
const F48: usize = 480; // 48k @ 10ms
const RING_CAP: usize = 96000; // 2s @ 48k
const TONE_AMP: f32 = 0.5;

fn poll_tick(kind: ErrorKind) -> bool {
    // see audiohub-net session.rs: Windows latches ICMP unreachable as
    // ConnectionReset on unconnected UDP sockets
    matches!(
        kind,
        ErrorKind::WouldBlock
            | ErrorKind::TimedOut
            | ErrorKind::ConnectionReset
            | ErrorKind::ConnectionRefused
            | ErrorKind::ConnectionAborted
    )
}

// ---------------------------------------------------------------- tx engine

#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) enum SourceSpec {
    Tone { freq_bits: u32 },
    Mic,
    /// What this machine is playing (spec-m4b §B2). The backend id is part of
    /// the dedup key: two streams naming different backends are two captures.
    SysAudio { backend: String },
    /// What an application played into ONE peer's virtual speaker (spec-m5b
    /// §5.4). The slot is part of the dedup key, so each speaker ring gets
    /// exactly one consumer entry — which is what keeps the halbridge SPSC rule
    /// (exactly one reader per ring) literally true with sixteen of them.
    ///
    /// Collapsing this back to a slot-less variant is the single most dangerous
    /// simplification available here: every peer's audio would come out of one
    /// ring, every positive test would still pass, and the only symptom would
    /// be one peer hearing another's audio.
    HalSpeaker { slot: u8 },
}

impl SourceSpec {
    pub(crate) fn tone(freq: f32) -> SourceSpec {
        SourceSpec::Tone { freq_bits: freq.to_bits() }
    }

    fn label(&self) -> String {
        match self {
            SourceSpec::Tone { freq_bits } => format!("tone {}Hz", f32::from_bits(*freq_bits)),
            SourceSpec::Mic => "mic".to_string(),
            SourceSpec::SysAudio { backend } => format!("sysaudio '{backend}'"),
            SourceSpec::HalSpeaker { slot } => format!("hal speaker slot {slot}"),
        }
    }
}

pub(crate) enum TxCmd {
    Add {
        stream_id: u32,
        key: [u8; 32],
        /// Per-stream media salt from the stream opener (frozen API).
        salt: Vec<u8>,
        dest: SocketAddr,
        spec: SourceSpec,
        loss_pct: f32,
        shared: Arc<TxShared>,
        /// Reports whether the source actually started, so the control-plane
        /// handler can answer AcceptStream/RejectStream truthfully.
        ack: Option<mpsc::Sender<std::result::Result<(), String>>>,
    },
    Remove {
        stream_id: u32,
    },
}

/// Mixer-thread commands. A cpal stream is not `Send` on every platform, so a
/// bridge device can only be opened (and dropped) on the thread that renders
/// into it — the ack carries the real open error back to `session.open`.
pub(crate) enum MixCmd {
    OpenBridge {
        device: String,
        /// Single-winner commit flag between the opener and the mixer. The
        /// mixer does the slow part (cpal) first and only KEEPS the device if
        /// it wins this flag; an opener whose ack deadline expired claims it on
        /// the way out. `true` after the mixer's swap therefore means exactly
        /// "a refcount is held for this open" — nobody has to guess.
        claim: Arc<AtomicBool>,
        ack: mpsc::Sender<std::result::Result<(), String>>,
    },
    ReleaseBridge {
        device: String,
    },
}

/// How long `session.open` waits for the mixer to actually open the bridge
/// device before it reports the session as failed.
const BRIDGE_ACK_TIMEOUT: Duration = Duration::from_secs(5);

/// Resolves a bridge selector to the device name the mixer will actually open.
/// Bridges are refcounted by this name, so it MUST be the resolved one: keyed
/// by the raw selector, "BlackHole" and "BlackHole 2ch" are two entries for one
/// card — opened twice, and neither release frees the other.
///
/// audiohub-core resolves privately inside `LivePlayback::start_on` and exposes
/// only the listing, so the rule is mirrored here: exact match first, then a
/// unique case-insensitive prefix.
pub fn resolve_bridge_device(names: &[String], query: &str) -> Result<String> {
    let q = query.trim();
    if q.is_empty() {
        return Err(anyhow!("empty bridge device name"));
    }
    if let Some(n) = names.iter().find(|n| n.as_str() == q) {
        return Ok(n.clone());
    }
    let ql = q.to_lowercase();
    let hits: Vec<&String> = names
        .iter()
        .filter(|n| n.to_lowercase().starts_with(&ql))
        .collect();
    match hits.len() {
        1 => Ok(hits[0].clone()),
        0 => Err(anyhow!(
            "no output device matches {q:?}; available: [{}]",
            names.join(", ")
        )),
        _ => Err(anyhow!(
            "output device name {q:?} is ambiguous; candidates: [{}]",
            hits.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
        )),
    }
}

/// Opens (or ref-counts) the named output device on the mixer thread and hands
/// back the RESOLVED device name, which is the refcount key the caller must
/// release with. spec-m4c §B: a failure here fails the session open with the
/// device's real reason — there is no fallback to the default output.
pub(crate) fn open_bridge(inner: &DaemonInner, device: &str) -> Result<String> {
    let resolved = resolve_bridge_device(&audio::list_output_devices(), device)
        .map_err(|e| anyhow!("open bridge device '{device}': {e:#}"))?;
    let claim = Arc::new(AtomicBool::new(false));
    let (ack_tx, ack_rx) = mpsc::channel();
    lk(&inner.mix_cmds)
        .send(MixCmd::OpenBridge {
            device: resolved.clone(),
            claim: claim.clone(),
            ack: ack_tx,
        })
        .map_err(|_| anyhow!("mixer unavailable"))?;
    match ack_rx.recv_timeout(BRIDGE_ACK_TIMEOUT) {
        Ok(Ok(())) => Ok(resolved),
        Ok(Err(e)) => Err(anyhow!("{e}")),
        Err(_) => {
            // Nothing will ever release what the mixer may still be about to
            // take: this open has no session behind it any more. Winning the
            // claim tells a late mixer to keep nothing; losing it means the
            // refcount is already real, so balance it here.
            if claim.swap(true, Ordering::SeqCst) {
                release_bridge(inner, &resolved);
            }
            Err(anyhow!(
                "bridge device '{resolved}' did not open within {BRIDGE_ACK_TIMEOUT:?}"
            ))
        }
    }
}

pub(crate) fn release_bridge(inner: &DaemonInner, device: &str) {
    let _ = lk(&inner.mix_cmds).send(MixCmd::ReleaseBridge { device: device.to_string() });
}

struct TxStream {
    id: u32,
    crypto: MediaCrypto,
    dest: SocketAddr,
    spec: SourceSpec,
    loss: LossInjector,
    seq: u32,
    rung: u32,
    rs: Option<LinearResampler>, // 48k -> rung rate, recreated on rung switch
    rs_last: f32,                // last source sample; seeds the next resampler
    staged: Vec<f32>,
    shared: Arc<TxShared>,
}

struct SourceEnt {
    src: Src,
    refs: usize,
    frame: Vec<f32>, // one 48k frame per tick, broadcast to all attached streams
}

/// A media source plus the one thing `FrameSource` cannot express: a system
/// capture that has died for good (group C's frozen `SysAudioCapture::failed`).
enum Src {
    Frame(Box<dyn FrameSource>),
    Sys(SysAudioFrames),
}

impl Src {
    fn next_frame(&mut self, out: &mut Vec<f32>) -> bool {
        match self {
            Src::Frame(f) => f.next_frame(out),
            Src::Sys(s) => s.next_frame(out),
        }
    }

    /// `Some(reason)` once the source can never produce audio again.
    fn failed(&self) -> Option<String> {
        match self {
            Src::Frame(_) => None,
            Src::Sys(s) => s.cap.failed(),
        }
    }
}

/// Bridges `SysAudioCapture` into the 10ms send scheduler: the capture appends
/// mono f32 at its own rate in irregular WASAPI-sized chunks, the scheduler
/// wants exactly one 48k frame per tick. Underruns emit silence rather than
/// stalling the cadence — a loopback capture is silent whenever nothing plays.
struct SysAudioFrames {
    cap: Box<dyn SysAudioCapture>,
    backend: String,
    excludes_self: bool,
    rs: Option<LinearResampler>,
    fifo: VecDeque<f32>,
    raw: Vec<f32>,
    staged: Vec<f32>,
}

impl SysAudioFrames {
    /// 1s: a reader that fell behind must drop old audio, never grow unbounded.
    const FIFO_CAP: usize = 48000;

    fn new(cap: Box<dyn SysAudioCapture>, backend: String, excludes_self: bool) -> SysAudioFrames {
        let rate = cap.sample_rate();
        SysAudioFrames {
            cap,
            backend,
            excludes_self,
            rs: (rate != 48000).then(|| LinearResampler::new(rate, 48000)),
            fifo: VecDeque::new(),
            raw: Vec::new(),
            staged: Vec::new(),
        }
    }

    fn next_frame(&mut self, out: &mut Vec<f32>) -> bool {
        self.raw.clear();
        self.cap.read(&mut self.raw);
        match self.rs.as_mut() {
            None => self.fifo.extend(self.raw.iter().copied()),
            Some(rs) => {
                self.staged.clear();
                rs.process(&self.raw, &mut self.staged);
                self.fifo.extend(self.staged.iter().copied());
            }
        }
        while self.fifo.len() > Self::FIFO_CAP {
            self.fifo.pop_front();
        }
        out.clear();
        if self.fifo.len() >= F48 {
            out.extend(self.fifo.drain(..F48));
        } else {
            out.resize(F48, 0.0);
        }
        true
    }
}

/// plan §5 hard requirement, fired at most once per process: a backend that
/// cannot keep our own playback out of its capture, running while we are also
/// playing a peer's audio, closes an audio loop (peer mic -> our speakers ->
/// our capture -> back to the peer). Warn the operator; do not spam them.
static SELF_CAPTURE_WARNED: AtomicBool = AtomicBool::new(false);

/// True while some received stream is routed to this machine's real output.
fn playing_remote_audio(inner: &DaemonInner) -> bool {
    rd(&inner.rx_table).values().any(|r| r.is_spk || r.monitor)
}

fn warn_feedback_risk(inner: &DaemonInner, backend: &str) {
    // cheap guard first: this runs once a second while such a capture is live
    if SELF_CAPTURE_WARNED.load(Ordering::Relaxed) || !playing_remote_audio(inner) {
        return;
    }
    if SELF_CAPTURE_WARNED.swap(true, Ordering::Relaxed) {
        return;
    }
    dlog!(
        "[audiohubd] WARNING: sysaudio backend '{backend}' does not exclude this process, and \
         this machine is also playing a peer's audio — the capture will contain that audio and \
         feed it back (plan §5). Use a backend with excludes_self, or stop playing remote audio \
         while mirroring."
    );
}

fn build_source(inner: &DaemonInner, spec: &SourceSpec) -> Result<Src> {
    Ok(match spec {
        SourceSpec::Tone { freq_bits } => Src::Frame(Box::new(ToneSource::new(
            f32::from_bits(*freq_bits),
            TONE_AMP,
            48000,
            FRAME_MS as u32,
        ))),
        SourceSpec::Mic => Src::Frame(Box::new(
            MicSource::new(FRAME_MS as u32).context("start microphone capture")?,
        )),
        SourceSpec::SysAudio { backend } => {
            // resolve first: start_backend would re-resolve "auto" and we need
            // the concrete id + excludes_self for the feedback check anyway
            let info = sysaudio::resolve_backend(backend)?;
            let cap = sysaudio::start_backend(&info.id)
                .with_context(|| format!("start sysaudio backend '{}'", info.id))?;
            if !info.excludes_self {
                warn_feedback_risk(inner, &info.id);
            }
            Src::Sys(SysAudioFrames::new(cap, info.id, info.excludes_self))
        }
        // No bridge = no ring to read, so this would be an accepted stream that
        // is silent forever with nothing saying why. Fail the open instead, the
        // same way an unresolvable sysaudio backend does. A bridge that IS
        // there but has no driver attached is a different thing entirely and
        // succeeds: halbridge answers silence, one full frame per tick.
        SourceSpec::HalSpeaker { slot } => {
            let hal = inner.hal().ok_or_else(|| {
                anyhow!(
                    "the macOS HAL bridge is not available (no LaunchDaemon holding \
                     '{}', or AUDIOHUB_HAL_BRIDGE=off)",
                    crate::halbridge::HAL_SERVICE_NAME
                )
            })?;
            // An app may have been playing into the virtual speaker long before
            // anyone opened a session for it, and only the ring's CONSUMER may
            // move read_idx — so a backlog we do not drop here is not a
            // one-off: producer and consumer then run at the same 480/10ms and
            // the peer hears everything a fixed half second late, forever.
            // Same reasoning (and the same 500ms) as the driver's own flush of
            // mic_ring at handshake.
            let mut stale = Vec::with_capacity(crate::halbridge::HAL_RING_FRAMES as usize);
            let dropped =
                hal.read_spk_mono(*slot, &mut stale, crate::halbridge::HAL_RING_FRAMES as usize);
            if dropped > 0 {
                dlog!(
                    "[audiohubd] hal speaker slot {slot}: dropped {}ms of audio played before \
                     this stream opened",
                    dropped / (crate::halbridge::HAL_SAMPLE_RATE as usize / 1000)
                );
            }
            Src::Frame(Box::new(crate::halbridge::HalSpeakerSource::new(&hal, *slot)))
        }
    })
}

/// Creates a resampler for the new rate that continues from `last`, so a rung
/// switch mid-stream cannot inject the zero sample (audible click) a fresh
/// resampler would interpolate from.
fn seeded_resampler(src_rate: u32, dst_rate: u32, last: f32) -> LinearResampler {
    let mut rs = LinearResampler::new(src_rate, dst_rate);
    let mut discard = Vec::new();
    rs.process(&[last], &mut discard); // primes `last`; output is not audio
    rs
}

fn apply_txcmd(
    inner: &DaemonInner,
    cmd: TxCmd,
    streams: &mut HashMap<u32, TxStream>,
    sources: &mut HashMap<SourceSpec, SourceEnt>,
) {
    match cmd {
        TxCmd::Add { stream_id, key, salt, dest, spec, loss_pct, shared, ack } => {
            let started = match sources.entry(spec.clone()) {
                Entry::Occupied(mut o) => {
                    o.get_mut().refs += 1;
                    Ok(())
                }
                Entry::Vacant(v) => match build_source(inner, &spec) {
                    Ok(src) => {
                        v.insert(SourceEnt { src, refs: 1, frame: Vec::new() });
                        Ok(())
                    }
                    Err(e) => {
                        dlog!("[audiohubd] source for stream {stream_id}: {e:#}");
                        Err(format!("{e:#}"))
                    }
                },
            };
            if started.is_ok() {
                streams.insert(
                    stream_id,
                    TxStream {
                        id: stream_id,
                        // real streams are always keyed per stream, never with
                        // the bare connection media key
                        crypto: MediaCrypto::new_for_stream(&key, stream_id, &salt),
                        dest,
                        spec,
                        loss: LossInjector::new(stream_id, loss_pct),
                        seq: 0,
                        rung: 0,
                        rs: None,
                        rs_last: 0.0,
                        staged: Vec::new(),
                        shared,
                    },
                );
            }
            if let Some(a) = ack {
                let _ = a.send(started);
            }
        }
        TxCmd::Remove { stream_id } => {
            if let Some(st) = streams.remove(&stream_id) {
                if let Some(ent) = sources.get_mut(&st.spec) {
                    ent.refs = ent.refs.saturating_sub(1);
                    if ent.refs == 0 {
                        sources.remove(&st.spec);
                    }
                }
            }
        }
    }
}

/// Closes every stream fed by a source that reported itself dead (the frozen
/// `SysAudioCapture::failed` seam). Without this the capture keeps returning 0
/// samples and the peer receives digital silence forever, with nothing on
/// either side saying why — the reason is logged and the peer gets CloseStream.
fn reap_dead_sources(
    inner: &DaemonInner,
    streams: &mut HashMap<u32, TxStream>,
    sources: &mut HashMap<SourceSpec, SourceEnt>,
) {
    let dead: Vec<(SourceSpec, String)> = sources
        .iter()
        .filter_map(|(spec, ent)| ent.src.failed().map(|why| (spec.clone(), why)))
        .collect();
    for (spec, why) in dead {
        let ids: Vec<u32> = streams
            .values()
            .filter(|s| s.spec == spec)
            .map(|s| s.id)
            .collect();
        for id in ids {
            dlog!(
                "[audiohubd] stream {id}: media source ({}) died: {why}; closing the stream",
                spec.label()
            );
            // queues a TxCmd::Remove we will drain next tick, and tells the peer
            crate::conn::teardown_stream(inner, id, true);
        }
        // drop the corpse now: the queued Remove would only reach it next tick
        streams.retain(|_, s| s.spec != spec);
        sources.remove(&spec);
    }
}

/// spec-m4c §D: the default input changed, so a live `MicSource` is now bound
/// to the wrong device. Build the replacement BEFORE dropping the old one: if
/// the new default cannot be opened the session keeps its (silent) capture
/// rather than dying, and the reason is on stderr.
fn rebuild_mic_source(inner: &DaemonInner, sources: &mut HashMap<SourceSpec, SourceEnt>) {
    let Some(ent) = sources.get_mut(&SourceSpec::Mic) else {
        dlog!("[audiohubd] default input changed; no microphone source to rebuild");
        return;
    };
    match build_source(inner, &SourceSpec::Mic) {
        Ok(src) => {
            ent.src = src; // old capture dropped here, after the new one exists
            dlog!("[audiohubd] default input changed; microphone source rebuilt");
        }
        Err(e) => dlog!(
            "[audiohubd] default input changed but the new device failed to open ({e:#}); \
             keeping the previous capture"
        ),
    }
}

pub(crate) fn tx_loop(inner: Arc<DaemonInner>, cmds: mpsc::Receiver<TxCmd>) {
    let mut streams: HashMap<u32, TxStream> = HashMap::new();
    let mut sources: HashMap<SourceSpec, SourceEnt> = HashMap::new();
    // Lifted out of the daemon mutex once, here, so the tick itself never
    // touches that lock; the bridge is installed before any thread starts and
    // is never replaced.
    let hal = inner.hal();
    let mut dev_epoch = inner.dev_in_epoch.load(Ordering::Relaxed);
    let start = Instant::now();
    let mut tick: u64 = 0;
    loop {
        if inner.shutdown.load(Ordering::SeqCst) {
            return;
        }
        {
            let e = inner.dev_in_epoch.load(Ordering::Relaxed);
            if e != dev_epoch {
                dev_epoch = e;
                rebuild_mic_source(&inner, &mut sources);
            }
        }
        // if a stall (device open, scheduler) put us far behind, skip the
        // missed frames instead of bursting them — receiver JBs trim bursts
        // by advancing, which starves them against the steady arrival rate
        let behind = start.elapsed().as_millis() as u64 / FRAME_MS;
        if behind > tick + 10 {
            tick = behind;
        }
        let deadline = start + Duration::from_millis(tick * FRAME_MS);
        loop {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            match cmds.recv_timeout(deadline - now) {
                Ok(cmd) => apply_txcmd(&inner, cmd, &mut streams, &mut sources),
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }
        while let Ok(cmd) = cmds.try_recv() {
            apply_txcmd(&inner, cmd, &mut streams, &mut sources);
        }
        // spec-m5b §5.4: a PUBLISHED speaker ring with no session behind it
        // still receives whatever the app that selected it played. Nobody would
        // ever move its read_idx, the ring fills, and the driver's census
        // starts logging "audiohubd has stopped draining it" at error level.
        // Only a ring's consumer may move read_idx, and on this side that is
        // THIS thread — so the drain belongs here, above the idle short-circuit
        // below, because "no streams at all" is exactly the case it exists for.
        if let Some(h) = hal.as_ref() {
            let mut busy = 0u16;
            for spec in sources.keys() {
                if let SourceSpec::HalSpeaker { slot } = spec {
                    busy |= 1u16 << (*slot).min(15);
                }
            }
            h.drain_idle_speakers(busy);
        }
        if streams.is_empty() {
            match cmds.recv_timeout(Duration::from_millis(200)) {
                Ok(cmd) => apply_txcmd(&inner, cmd, &mut streams, &mut sources),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
            tick = start.elapsed().as_millis() as u64 / FRAME_MS + 1;
            continue;
        }

        let slow_tick = tick % 100 == 0; // ~1s
        for ent in sources.values_mut() {
            if ent.refs == 0 {
                continue;
            }
            if !ent.src.next_frame(&mut ent.frame) {
                ent.frame.clear();
            }
            if ent.frame.len() != F48 {
                // An OVER-long frame means the source appended instead of
                // replacing, and the resize below then re-sends whatever its
                // very first call produced, forever, while the packet counts,
                // the loss rate and the tone probe all stay green. That cost a
                // full debugging session once; it must never be silent again.
                debug_assert!(
                    ent.frame.len() <= F48,
                    "FrameSource yielded {} samples (> {F48}): it appended instead of replacing",
                    ent.frame.len()
                );
                if ent.frame.len() > F48 && slow_tick {
                    dlog!(
                        "[audiohubd] BUG: source yielded {} samples, expected {F48} — \
                         the stream is repeating its first frame",
                        ent.frame.len()
                    );
                }
                ent.frame.resize(F48, 0.0);
            }
            // playback can start long after the capture did, so the plan §5
            // condition is re-evaluated while such a capture is alive
            if slow_tick {
                if let Src::Sys(s) = &ent.src {
                    if !s.excludes_self {
                        warn_feedback_risk(&inner, &s.backend);
                    }
                }
            }
        }
        reap_dead_sources(&inner, &mut streams, &mut sources);
        let ts_us = start.elapsed().as_micros() as u64;
        for st in streams.values_mut() {
            let Some(ent) = sources.get(&st.spec) else { continue };
            let want = st.shared.rung.load(Ordering::Relaxed).min(3);
            if want != st.rung {
                st.rung = want;
                let last = st.rs_last;
                st.rs = (want != 0).then(|| seeded_resampler(48000, rung_rate(want), last));
            }
            st.rs_last = ent.frame.last().copied().unwrap_or(st.rs_last);
            let rate = rung_rate(st.rung);
            let samples: &[f32] = match st.rs.as_mut() {
                Some(rs) => {
                    st.staged.clear();
                    rs.process(&ent.frame, &mut st.staged);
                    &st.staged
                }
                None => &ent.frame,
            };
            let seq = st.seq;
            st.seq = st.seq.wrapping_add(1);
            let dropped = st.loss.should_drop(); // advance LCG every frame
            if dropped {
                continue;
            }
            if let Some(a) = *lk(&st.shared.dest_override) {
                if a != st.dest {
                    dlog!("[audiohubd] stream {} dest {} -> {} (keepalive)", st.id, st.dest, a);
                    st.dest = a;
                }
            }
            let payload = dsp::f32_to_s16le(samples);
            let header = Header {
                kind: Kind::Media,
                codec: Codec::PcmS16le,
                channels: 1,
                sample_rate: rate,
                session_id: st.id as u64,
                stream_id: st.id,
                seq,
                timestamp_us: ts_us,
                payload_len: 0, // seal() sets ciphertext length
            };
            match st.crypto.seal(&header, &payload) {
                Ok(dg) => {
                    if inner.udp.send_to(&dg, st.dest).is_ok() {
                        st.shared.sent_packets.fetch_add(1, Ordering::Relaxed);
                        st.shared.sent_bytes.fetch_add(dg.len() as u64, Ordering::Relaxed);
                    }
                }
                Err(e) => dlog!("[audiohubd] media seal stream {}: {e}", st.id),
            }
        }
        tick += 1;
    }
}

// ---------------------------------------------------------------- rx engine

pub(crate) fn rx_loop(inner: Arc<DaemonInner>) {
    let mut buf = [0u8; 2048];
    loop {
        if inner.shutdown.load(Ordering::SeqCst) {
            return;
        }
        match inner.udp.recv_from(&mut buf) {
            Ok((n, from)) => handle_datagram(&inner, &buf[..n], from),
            Err(e) if poll_tick(e.kind()) => {}
            Err(e) => {
                dlog!("[audiohubd] udp recv: {e}");
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

fn handle_datagram(inner: &DaemonInner, dg: &[u8], from: SocketAddr) {
    let Ok((h, _payload)) = Header::parse(dg) else { return };
    match h.kind {
        Kind::Media => {
            let rx = rd(&inner.rx_table).get(&h.stream_id).cloned();
            let Some(rx) = rx else { return };
            let Ok((h, plain)) = rx.crypto.open(dg) else { return }; // tampered/foreign
            let arrival = inner.start.elapsed().as_micros() as u64;
            let mut jit_ms = 0.0f32;
            {
                let mut c = lk(&rx.stats);
                if c.first.is_none() {
                    c.first = Some(Instant::now());
                }
                c.rx.on_packet(h.seq, h.timestamp_us, arrival, plain.len());
                c.last_rate = h.sample_rate;
                let transit = arrival as i64 - h.timestamp_us as i64;
                if let Some(p) = c.prev_transit {
                    jit_ms = (transit - p).unsigned_abs() as f32 / 1000.0;
                    c.note_jitter(jit_ms); // feeds the per-interval Stats window
                }
                c.prev_transit = Some(transit);
            }
            let raw = dsp::s16le_to_f32(&plain);
            let last_sample = raw.last().copied();
            let mut st = lk(&rx.jbs);
            let frame = if h.sample_rate == 48000 {
                raw
            } else {
                if st.rs_rate != h.sample_rate || st.rs.is_none() {
                    // continue from the last decoded sample: a mid-stream rate
                    // change must not interpolate up from zero
                    st.rs = Some(seeded_resampler(h.sample_rate, 48000, st.rs_last));
                    st.rs_rate = h.sample_rate;
                }
                let mut out = Vec::with_capacity(F48 + 8);
                st.rs.as_mut().unwrap().process(&raw, &mut out);
                out
            };
            if let Some(l) = last_sample {
                st.rs_last = l;
            }
            st.jb.push(h.seq, frame.clone());
            // starvation self-heal: if the JB keeps rejecting arrivals as
            // late while nearly empty (expected seq raced ahead — mixer
            // stall or cross-machine clock drift), restart it cleanly
            if st.jb.dropped > st.last_dropped && st.jb.depth() <= 1 {
                st.late_streak += 1;
            } else {
                st.late_streak = 0;
            }
            st.last_dropped = st.jb.dropped;
            if st.late_streak >= 50 {
                let target = st.jb.target();
                st.jb = audiohub_net::media::JitterBuffer::new(target);
                st.jb.push(h.seq, frame);
                st.last_dropped = 0;
                st.late_streak = 0;
                dlog!("[audiohubd] jb resync on stream {}", h.stream_id);
            }
            st.jit_win.push(jit_ms);
            if st.jit_win.len() > 256 {
                st.jit_win.remove(0);
            }
            st.pushes += 1;
            if st.pushes % 100 == 0 && !st.jit_win.is_empty() {
                let mut v = st.jit_win.clone();
                v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                let p95 = v[(v.len() * 95 / 100).min(v.len() - 1)] as f64;
                st.jb.update_target(p95, FRAME_MS as f64);
            }
        }
        Kind::PullReq => {
            // Receiver keepalive. spec-m4a §3 freezes the media destination as
            // "control-TCP peer IP + peer daemon port" and keepalives as
            // count-only, so this arm may learn the PORT (the peer store's port
            // goes stale when a daemon moves) but never the IP: a keepalive is
            // an unencrypted 40-byte header carrying a cleartext stream_id, so
            // trusting its source IP lets any host on the path redirect the
            // live media stream to itself. Tier-0 single-side reachability is
            // docs/plan.md §4.3, not spec-m4a §4.3.
            let found = {
                let st = lk(&inner.state);
                st.sessions
                    .get(&h.stream_id)
                    .and_then(|e| e.tx.clone().map(|t| (t, e.conn.media_dest.ip())))
            };
            let Some((t, peer_ip)) = found else { return };
            t.ka_count.fetch_add(1, Ordering::Relaxed);
            if from.ip() != peer_ip {
                t.ka_rejected.fetch_add(1, Ordering::Relaxed);
                if t.first_ka_warning() {
                    dlog!(
                        "[audiohubd] stream {}: keepalive from {} ignored (control peer is {})",
                        h.stream_id,
                        from,
                        peer_ip
                    );
                }
                return;
            }
            let learned = SocketAddr::new(peer_ip, from.port());
            let mut d = lk(&t.dest_override);
            if *d != Some(learned) {
                *d = Some(learned);
            }
        }
        _ => {}
    }
}

/// Receiver-side keepalive (spec §3): one unencrypted PullReq per stream per
/// second toward the sender to hold NAT/firewall state.
pub(crate) fn send_pullreq(inner: &DaemonInner, rx: &RxStream) {
    let h = Header {
        kind: Kind::PullReq,
        codec: Codec::PcmS16le,
        channels: 1,
        sample_rate: 48000,
        session_id: rx.stream_id as u64,
        stream_id: rx.stream_id,
        seq: rx.ka_seq.fetch_add(1, Ordering::Relaxed),
        timestamp_us: inner.start.elapsed().as_micros() as u64,
        payload_len: 0,
    };
    let _ = inner.udp.send_to(&h.encode(&[]), rx.ka_dest);
}

// ---------------------------------------------------------------- mixer

/// Frozen clip curve: linear to 0.8, tanh-compressed knee above.
fn soft_clip(s: f32) -> f32 {
    let a = s.abs();
    if a <= 0.8 {
        s
    } else {
        (0.8 + 0.2 * ((a - 0.8) / 0.2).tanh()).copysign(s)
    }
}

/// Appends post-clip mixer output to the 2s ring used by mix_verdicts.
fn push_mix(inner: &DaemonInner, samples: &[f32]) {
    let mut r = lk(&inner.mix_ring);
    r.extend(samples.iter().copied());
    if r.len() > RING_CAP {
        let d = r.len() - RING_CAP;
        r.drain(..d);
    }
}

/// Drops the mix history when nothing feeds the mixer. The ring is a rolling
/// window read by mix_verdicts, and the idle path advances it far slower than
/// real time, so rolling silence through it would keep a stopped tone testing
/// as present for seconds. No spk stream means no mix output at all.
fn clear_mix(inner: &DaemonInner) {
    let mut r = lk(&inner.mix_ring);
    if !r.is_empty() {
        r.clear();
    }
}

/// One bridge target: a NAMED output device fed by every mic-recv stream that
/// asked for it (spec-m4c §B). Ref-counted so two sessions bridging to the same
/// card share one device stream.
struct BridgeOut {
    _pb: LivePlayback,
    tx: AudioTx,
    refs: usize,
    buf: [f32; F48],
}

fn apply_mixcmd(cmd: MixCmd, bridges: &mut HashMap<String, BridgeOut>) {
    match cmd {
        MixCmd::OpenBridge { device, claim, ack } => {
            // Open first, commit second: cpal can sit here for seconds, which
            // is exactly when the opener's ack deadline expires. Whatever is
            // built before the claim is lost costs nothing to drop.
            let opened = if bridges.contains_key(&device) {
                Ok(None) // already open: this is only a new reference
            } else {
                LivePlayback::start_on(&device, 48000)
                    .map(|(pb, tx)| Some(BridgeOut { _pb: pb, tx, refs: 0, buf: [0.0; F48] }))
                    .map_err(|e| format!("open bridge device '{device}': {e:#}"))
            };
            let r = match opened {
                // a failed open holds nothing, so it never claims: the opener
                // must stay free to give up without releasing someone else's
                Err(e) => Err(e),
                Ok(fresh) => {
                    if claim.swap(true, Ordering::SeqCst) {
                        return; // opener gave up: hold nothing, `fresh` drops here
                    }
                    if let Some(b) = fresh {
                        dlog!("[audiohubd] bridge output '{device}' opened");
                        bridges.insert(device.clone(), b);
                    }
                    if let Some(b) = bridges.get_mut(&device) {
                        b.refs += 1;
                    }
                    Ok(())
                }
            };
            let _ = ack.send(r);
        }
        MixCmd::ReleaseBridge { device } => {
            if let Some(b) = bridges.get_mut(&device) {
                b.refs = b.refs.saturating_sub(1);
                if b.refs == 0 {
                    bridges.remove(&device);
                    dlog!("[audiohubd] bridge output '{device}' closed");
                }
            }
        }
    }
}

pub(crate) fn mixer_loop(inner: Arc<DaemonInner>, cmds: mpsc::Receiver<MixCmd>) {
    let start = Instant::now();
    let mut tick: u64 = 0;
    let mut playback: Option<(LivePlayback, AudioTx)> = None;
    let mut pb_fail_at: Option<Instant> = None;
    let mut bridges: HashMap<String, BridgeOut> = HashMap::new();
    let mut dev_epoch = inner.dev_out_epoch.load(Ordering::Relaxed);
    let mut mix = [0.0f32; F48];
    let mut mon = [0.0f32; F48];
    let mut frame = [0.0f32; F48];
    // spec-m5b §5.4 microphone direction. Lifted out of the daemon mutex once,
    // here, so the tick itself never touches that lock; the bridge is installed
    // before any thread starts and is never replaced.
    let hal = inner.hal();
    // ONE BUCKET PER SLOT, not one shared buffer.
    //
    // The version this replaces summed every `hal` stream into a single `hal_buf`
    // and wrote it into the one mic ring. With two peers bound that is a mixer,
    // not a router: whoever recorded peer A's virtual microphone got peer B's
    // audio too — and every positive test still passed, because A's audio was
    // in there as well. `dirty` keeps the clearing cost proportional to the
    // buckets actually used rather than to 16 * 480 floats per 10ms tick.
    let mut hal_bufs = vec![[0.0f32; F48]; crate::haldev::HAL_MAX_SLOTS];
    let mut hal_dirty: u16 = 0;
    loop {
        if inner.shutdown.load(Ordering::SeqCst) {
            return;
        }
        while let Ok(cmd) = cmds.try_recv() {
            apply_mixcmd(cmd, &mut bridges);
        }
        {
            // spec-m4c §D: the default output moved, so this stream now plays
            // into the old device. Drop it and let the code below re-open on
            // the new default; one frame of silence, no session teardown.
            // Bridges name their device explicitly and are left alone.
            let e = inner.dev_out_epoch.load(Ordering::Relaxed);
            if e != dev_epoch {
                dev_epoch = e;
                dlog!("[audiohubd] default output changed; rebuilding the playback stream");
                playback = None;
                pb_fail_at = None; // retry now, not after the 10s backoff
            }
        }
        // never replay missed ticks (see tx_loop): each replayed tick is an
        // extra pop that races the JB expected-seq ahead of real arrivals
        let behind = start.elapsed().as_millis() as u64 / FRAME_MS;
        if behind > tick + 10 {
            tick = behind;
        }
        let deadline = start + Duration::from_millis(tick * FRAME_MS);
        let now = Instant::now();
        if now < deadline {
            std::thread::sleep(deadline - now);
        }
        let streams: Vec<Arc<RxStream>> = rd(&inner.rx_table).values().cloned().collect();
        if streams.is_empty() {
            // an open bridge keeps being written to even before its stream's
            // first frame arrives: a virtual card that is never written to may
            // not spin up its IO cycle at all, and the first real audio would
            // then be swallowed by the warm-up
            for b in bridges.values_mut() {
                b.buf.fill(0.0);
                let silence = b.buf;
                b.tx.push(&silence);
            }
            clear_mix(inner.as_ref()); // never serve stale mix audio
            std::thread::sleep(Duration::from_millis(20));
            tick = start.elapsed().as_millis() as u64 / FRAME_MS + 1;
            continue;
        }
        mix.fill(0.0);
        mon.fill(0.0);
        for slot in 0..crate::haldev::HAL_MAX_SLOTS {
            if hal_dirty & (1 << slot) != 0 {
                hal_bufs[slot].fill(0.0);
            }
        }
        hal_dirty = 0;
        for b in bridges.values_mut() {
            b.buf.fill(0.0);
        }
        let mut any_spk = false;
        let mut any_mon = false;
        for s in &streams {
            let popped = lk(&s.jbs).jb.pop();
            lk(&s.post).advance(popped, &mut frame);
            if let Some(ring) = s.ring.as_ref() {
                let mut r = lk(ring);
                r.extend(frame.iter().copied());
                if r.len() > RING_CAP {
                    let d = r.len() - RING_CAP;
                    r.drain(..d);
                }
            }
            // the bridge is a third destination, not an alternative to monitor:
            // one decoded frame may feed the virtual card AND the local output
            if let Some(name) = s.bridge.as_ref() {
                if let Some(b) = bridges.get_mut(name) {
                    for i in 0..F48 {
                        b.buf[i] += frame[i];
                    }
                }
            }
            // ...and the virtual microphone is a fourth one: monitor, bridge
            // and hal are independent destinations for the SAME decode
            // (spec-m5b §5.4). The bucket is chosen by the PEER's slot, so two
            // peers' audio can never meet.
            add_to_hal_bucket(s.hal_slot, &frame, &mut hal_bufs, &mut hal_dirty);
            if s.is_spk {
                any_spk = true;
                for i in 0..F48 {
                    mix[i] += frame[i];
                }
            } else if s.monitor {
                any_mon = true;
                for i in 0..F48 {
                    mon[i] += frame[i];
                }
            }
        }
        for b in bridges.values_mut() {
            let out: Vec<f32> = b.buf.iter().map(|&v| soft_clip(v)).collect();
            b.tx.push(&out);
        }
        // Exactly 480 mono samples per 10ms tick per slot = each ring's 48k
        // rate. Only into slots a session asked for AND an application is
        // actually reading: writing into a ring nobody drains would do nothing
        // but run that slot's mic_dropped up. The write is a lock-free SPSC
        // index bump, safe to do on this loop.
        if hal_dirty != 0 {
            if let Some(h) = hal.as_ref() {
                let mut out = [0.0f32; F48];
                for slot in 0..crate::haldev::HAL_MAX_SLOTS {
                    if hal_dirty & (1 << slot) == 0
                        || !inner.hal_mic_io[slot].load(Ordering::Relaxed)
                    {
                        continue;
                    }
                    for i in 0..F48 {
                        out[i] = soft_clip(hal_bufs[slot][i]);
                    }
                    h.write_mic_mono(slot as u8, &out);
                }
            }
        }
        if any_spk {
            let clipped: Vec<f32> = mix.iter().map(|&v| soft_clip(v)).collect();
            push_mix(inner.as_ref(), &clipped);
        } else {
            clear_mix(inner.as_ref());
        }
        if any_spk || any_mon {
            if playback.is_none()
                && pb_fail_at.map_or(true, |t| t.elapsed() > Duration::from_secs(10))
            {
                match LivePlayback::start(48000) {
                    Ok(p) => playback = Some(p),
                    Err(e) => {
                        dlog!("[audiohubd] playback unavailable: {e:#}");
                        pb_fail_at = Some(Instant::now());
                    }
                }
            }
            if let Some((_, tx)) = playback.as_mut() {
                let mut out = [0.0f32; F48];
                for i in 0..F48 {
                    out[i] = soft_clip(mix[i] + mon[i]);
                }
                tx.push(&out);
            }
        }
        tick += 1;
    }
}

/// Routes ONE decoded frame into the bucket of the peer that owns it.
///
/// Extracted so the rule can be tested without a driver, because it is the rule
/// the previous implementation did not have: every `hal` stream was summed into
/// a single buffer and written to a single ring, so with two peers bound,
/// whoever recorded peer A's virtual microphone also got peer B. Every positive
/// test still passed — A's audio WAS in there.
fn add_to_hal_bucket(
    hal_slot: Option<u8>,
    frame: &[f32; F48],
    bufs: &mut [[f32; F48]],
    dirty: &mut u16,
) {
    let Some(slot) = hal_slot else { return };
    let slot = slot as usize;
    if slot >= bufs.len() {
        return;
    }
    *dirty |= 1 << slot;
    for i in 0..F48 {
        bufs[slot][i] += frame[i];
    }
}

/// Presence verdict for one frequency on the summed mixer output. Plain
/// verify_tone can't apply here: concurrent probe tones are signal, not
/// noise, so detection keys on absolute Goertzel power (median of 100ms
/// windows); snr_db is still reported for diagnostics.
pub(crate) fn mix_tone_verdict(samples: &[f32], rate: u32, freq: f32) -> ToneVerdict {
    let win = (rate / 10) as usize;
    let skip = (rate / 5) as usize;
    if win == 0 || samples.len() < skip + win {
        return ToneVerdict {
            freq_hz: freq,
            snr_db: f32::NEG_INFINITY,
            detected: false,
            samples_analyzed: samples.len(),
        };
    }
    let mut powers: Vec<f32> = Vec::new();
    let mut snrs: Vec<f32> = Vec::new();
    let mut analyzed = 0usize;
    for chunk in samples[skip..].chunks(win) {
        if chunk.len() < win {
            break;
        }
        analyzed += chunk.len();
        let p = dsp::goertzel_power(chunk, rate, freq) as f64;
        let total: f64 = chunk.iter().map(|&x| (x as f64) * (x as f64)).sum::<f64>()
            / chunk.len() as f64
            / 2.0;
        let noise = (total - p).max(0.0) + 1e-12;
        powers.push(p as f32);
        snrs.push((10.0 * (p.max(1e-12) / noise).log10()) as f32);
    }
    if powers.is_empty() {
        return ToneVerdict {
            freq_hz: freq,
            snr_db: f32::NEG_INFINITY,
            detected: false,
            samples_analyzed: samples.len(),
        };
    }
    powers.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    snrs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let p_med = powers[powers.len() / 2];
    ToneVerdict {
        freq_hz: freq,
        snr_db: snrs[snrs.len() / 2],
        // amp-0.5 tone lands at ~0.0625; PLC decay and clipping keep a live
        // tone well above this floor while silence/noise stays far below
        detected: p_med > 1e-4,
        samples_analyzed: analyzed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two peers' virtual microphones are two rings, and one decoded frame
    /// belongs to exactly one of them.
    ///
    /// This is regression N2 in miniature: with a single shared buffer (what
    /// this code did before spec-m5b §5.4), capturing peer A's virtual
    /// microphone yielded A's audio AND B's — inaudible as a bug in any test
    /// that only checks "did A arrive", and a privacy defect in the field.
    #[test]
    fn each_peers_audio_lands_only_in_its_own_bucket() {
        let n = crate::haldev::HAL_MAX_SLOTS;
        let mut bufs = vec![[0.0f32; F48]; n];
        let mut dirty = 0u16;

        add_to_hal_bucket(Some(0), &[0.25; F48], &mut bufs, &mut dirty);
        add_to_hal_bucket(Some(3), &[0.75; F48], &mut bufs, &mut dirty);

        assert_eq!(dirty, 0b1001, "exactly the two slots written are dirty");
        assert!(bufs[0].iter().all(|&v| v == 0.25), "slot 0 must carry only its own peer");
        assert!(bufs[3].iter().all(|&v| v == 0.75), "slot 3 must carry only its own peer");
        for (i, b) in bufs.iter().enumerate() {
            if i != 0 && i != 3 {
                assert!(b.iter().all(|&v| v == 0.0), "slot {i} was written to by nobody");
            }
        }
    }

    #[test]
    fn two_streams_on_the_same_slot_still_mix() {
        // The bucket is per DEVICE, not per stream: two sessions feeding one
        // peer's virtual microphone are a mix, which is the provider-side
        // fan-in plan §1 asks for.
        let mut bufs = vec![[0.0f32; F48]; 4];
        let mut dirty = 0u16;
        add_to_hal_bucket(Some(1), &[0.25; F48], &mut bufs, &mut dirty);
        add_to_hal_bucket(Some(1), &[0.25; F48], &mut bufs, &mut dirty);
        assert!(bufs[1].iter().all(|&v| v == 0.5));
        assert_eq!(dirty, 0b10);
    }

    #[test]
    fn a_stream_bound_to_no_device_touches_nothing() {
        let mut bufs = vec![[0.0f32; F48]; 4];
        let mut dirty = 0u16;
        add_to_hal_bucket(None, &[1.0; F48], &mut bufs, &mut dirty);
        // ...and neither does one naming a slot this driver does not have.
        add_to_hal_bucket(Some(200), &[1.0; F48], &mut bufs, &mut dirty);
        assert_eq!(dirty, 0);
        assert!(bufs.iter().all(|b| b.iter().all(|&v| v == 0.0)));
    }

    /// The tx engine dedups sources by `SourceSpec`. If the slot were not part
    /// of the key, every peer's speaker session would share ONE entry — one
    /// ring, read once, fanned out to everybody — which is the same collapse
    /// from the other direction.
    #[test]
    fn each_slots_speaker_is_its_own_source_key() {
        let mut m: HashMap<SourceSpec, u32> = HashMap::new();
        m.insert(SourceSpec::HalSpeaker { slot: 0 }, 10);
        m.insert(SourceSpec::HalSpeaker { slot: 1 }, 11);
        assert_eq!(m.len(), 2, "two slots must be two sources, not one shared ring");
        // ...and the same slot twice is one source with two references, which
        // is what keeps the ring to a single consumer (halbridge SPSC rule).
        m.insert(SourceSpec::HalSpeaker { slot: 0 }, 12);
        assert_eq!(m.len(), 2);
        assert_eq!(m[&SourceSpec::HalSpeaker { slot: 0 }], 12);
    }
}
