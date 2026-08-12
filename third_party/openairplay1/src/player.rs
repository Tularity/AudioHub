//! The playback thread: ALAC decode, prebuffer, and the latency-correct
//! start, feeding the host's [`AudioSink`].
//!
//! A dedicated OS thread owns the ALAC decoder; the async audio receiver
//! hands it decrypted ALAC frames (tagged with their RTP timestamp) over a
//! channel. Once the clock model is ready the first chunk is held until its
//! `play_time`, giving a latency-correct start; otherwise a fixed prebuffer
//! is used. Blocking [`AudioSink::write`] calls pace playback afterwards.
//!
//! This is the library half of the sink seam: everything here is protocol
//! semantics (the start instant comes from the NTP clock model). The device
//! itself — output, pacing against the hardware, drift, gain — lives in the
//! host's sink.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use log::{debug, warn};

use crate::clock::{self, ClockModel};
use crate::decode::AlacDecoder;
use crate::events::{Event, EventSender};
use crate::sdp::AlacConfig;
use crate::session::TrackAnchor;
use crate::sink::AudioSink;

/// Roughly two seconds at the stock 352-frame packet size. A sender may burst
/// briefly, but cannot make the playback thread retain memory without bound.
const PLAYER_QUEUE_CAPACITY: usize = 256;

enum Command {
    Frame {
        ts: u32,
        packet: Vec<u8>,
        epoch: u64,
    },
    /// A lost packet at this RTP timestamp: emit silence to keep timing.
    Silence {
        ts: u32,
        epoch: u64,
    },
    Wake,
    Stop,
}

/// A cloneable handle for feeding ALAC frames to the playback thread. Held by
/// the audio-receiver task; sending is a no-op once the [`Player`] is dropped.
#[derive(Clone)]
pub struct PlayerSender {
    tx: SyncSender<Command>,
    epoch: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
}

impl PlayerSender {
    /// Queue a decrypted ALAC frame (with its RTP timestamp) for playback.
    pub fn frame(&self, ts: u32, packet: Vec<u8>) {
        let epoch = self.epoch.load(Ordering::Acquire);
        if matches!(
            self.tx.try_send(Command::Frame { ts, packet, epoch }),
            Err(TrySendError::Full(_))
        ) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Signal a lost packet so the player conceals it with silence.
    pub fn silence(&self, ts: u32) {
        let epoch = self.epoch.load(Ordering::Acquire);
        if matches!(
            self.tx.try_send(Command::Silence { ts, epoch }),
            Err(TrySendError::Full(_))
        ) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Drop buffered audio and re-arm the prebuffer (used on FLUSH).
    pub fn flush(&self) {
        self.epoch.fetch_add(1, Ordering::AcqRel);
        // Wake an idle player. A full queue means it is already awake; every
        // queued audio command carries the old epoch and will be skipped.
        let _ = self.tx.try_send(Command::Wake);
    }

    #[cfg(test)]
    fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// Owns the playback thread. Dropping it stops the thread and drops the sink.
/// Held by the session for lifecycle and FLUSH.
pub struct Player {
    tx: Option<SyncSender<Command>>,
    handle: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    epoch: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
}

impl Player {
    /// Spawn the playback thread feeding `sink`. Never fails: a sink is
    /// always usable (the host decides what `write` does, including
    /// discarding audio when it has no device).
    pub fn spawn(
        stream_id: u64,
        config: &AlacConfig,
        sink: Box<dyn AudioSink>,
        clock: Arc<Mutex<ClockModel>>,
        events: EventSender,
        track: TrackAnchor,
    ) -> Player {
        let config = *config;
        let prebuffer_packets = prebuffer_packets(&config);
        let (tx, rx) = std::sync::mpsc::sync_channel(PLAYER_QUEUE_CAPACITY);
        let stop = Arc::new(AtomicBool::new(false));
        let epoch = Arc::new(AtomicU64::new(0));
        let dropped = Arc::new(AtomicU64::new(0));
        let thread_stop = stop.clone();
        let thread_epoch = epoch.clone();
        let handle = std::thread::Builder::new()
            .name("audio-player".into())
            .spawn(move || {
                run(
                    stream_id,
                    config,
                    prebuffer_packets,
                    rx,
                    thread_stop,
                    thread_epoch,
                    clock,
                    sink,
                    events,
                    track,
                )
            })
            .expect("spawn player thread");
        Player {
            tx: Some(tx),
            handle: Some(handle),
            stop,
            epoch,
            dropped,
        }
    }

    /// A cloneable sender for the audio-receiver task to push frames.
    pub fn sender(&self) -> PlayerSender {
        PlayerSender {
            tx: self
                .tx
                .clone()
                .expect("player sender available before drop"),
            epoch: self.epoch.clone(),
            dropped: self.dropped.clone(),
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        // Set the flag so a queued backlog is skipped, and send an explicit
        // Stop so the thread wakes even if it's idle in recv() — dropping our
        // sender alone wouldn't close the channel while the audio task still
        // holds a cloned PlayerSender.
        self.stop.store(true, Ordering::Relaxed);
        if let Some(tx) = &self.tx {
            let _ = tx.try_send(Command::Stop);
        }
        self.tx = None;
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Target ~200 ms of prebuffer before the first write, expressed in packets.
fn prebuffer_packets(config: &AlacConfig) -> usize {
    let frames = config.frames_per_packet.max(1);
    let target = config.sample_rate / 5; // 200 ms
    (target / frames).max(1) as usize
}

#[allow(clippy::too_many_arguments)] // the thread body; every argument is one thing it owns
fn run(
    stream_id: u64,
    config: AlacConfig,
    prebuffer_packets: usize,
    rx: Receiver<Command>,
    stop: Arc<AtomicBool>,
    epoch: Arc<AtomicU64>,
    clock: Arc<Mutex<ClockModel>>,
    mut sink: Box<dyn AudioSink>,
    events: EventSender,
    track: TrackAnchor,
) {
    let mut decoder = match AlacDecoder::new(&config) {
        Ok(d) => d,
        Err(e) => {
            warn!("player: cannot build ALAC decoder ({e}); audio disabled");
            drain(rx);
            return;
        }
    };

    let channels = decoder.channels();
    debug!(
        "player: {} Hz {}ch, prebuffer {prebuffer_packets} packets",
        config.sample_rate, channels
    );
    let silence_frame = vec![0i16; config.frames_per_packet as usize * channels];
    let mut playout = Playout::new(prebuffer_packets, clock);
    let mut position = Position::new(stream_id, config.sample_rate, events, track);
    let mut decoded_packets: u64 = 0;
    // Epoch zero is established before the thread is spawned. Do not sample
    // it here: the caller can FLUSH immediately after `spawn` returns, before
    // this thread reaches initialization, and that boundary must still flush
    // the sink exactly once.
    let mut applied_epoch = 0;
    while let Ok(command) = rx.recv() {
        // Preempt a queued backlog on teardown rather than draining it in
        // real time.
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let target_epoch = epoch.load(Ordering::Acquire);
        if target_epoch != applied_epoch {
            playout.reset();
            sink.flush();
            applied_epoch = target_epoch;
        }
        match command {
            Command::Frame { ts, packet, epoch } => {
                if epoch != applied_epoch {
                    continue;
                }
                let pcm = match decoder.decode(&packet) {
                    Ok(pcm) => pcm,
                    Err(e) => {
                        debug!("player: dropping undecodable packet: {e}");
                        continue;
                    }
                };
                decoded_packets += 1;
                if decoded_packets <= 3 || decoded_packets.is_multiple_of(500) {
                    debug!("player: {decoded_packets} packets decoded");
                }
                playout.feed(ts, pcm, sink.as_mut());
                position.played(ts);
            }
            Command::Silence { ts, epoch } => {
                if epoch != applied_epoch {
                    continue;
                }
                playout.feed(ts, &silence_frame, sink.as_mut());
                position.played(ts);
            }
            Command::Wake => {}
            Command::Stop => break,
        }
    }
    debug!("player: stopped, {decoded_packets} packets played");
}

fn drain(rx: Receiver<Command>) {
    while rx.recv().is_ok() {}
}

/// Reports where playback is, from the audio itself.
///
/// The sender's own `progress:` line arrives about once per track — one
/// capture showed a 251-second track play to its end with no further report —
/// so a display that wants a running clock has to get it from somewhere else.
/// The somewhere else is here: every packet carries the RTP timestamp of the
/// audio being played, and the sender told us where the track starts and ends
/// on that same timeline.
///
/// It falls out of this that **a paused sender freezes the clock**: no audio
/// means no packets, which means nothing to report, and the last position
/// stands until playback resumes. Nothing has to detect the pause.
struct Position {
    stream_id: u64,
    rate: u32,
    events: EventSender,
    track: TrackAnchor,
    /// Whole seconds last reported, so a report is one per second rather than
    /// one per 8 ms packet.
    reported: Option<u64>,
}

impl Position {
    fn new(stream_id: u64, rate: u32, events: EventSender, track: TrackAnchor) -> Position {
        Position {
            stream_id,
            rate,
            events,
            track,
            reported: None,
        }
    }

    /// Note that the audio at `ts` is playing, and report the position when
    /// the displayed second changes.
    fn played(&mut self, ts: u32) {
        let Some(track) = *self.track.lock().unwrap() else {
            return; // the sender hasn't said where the track starts
        };
        let Some((elapsed, duration)) = extent(track, ts, self.rate) else {
            return;
        };
        let second = elapsed.as_secs();
        if self.reported == Some(second) {
            return;
        }
        self.reported = Some(second);
        let _ = self.events.try_send(Event::Progress {
            stream_id: self.stream_id,
            elapsed,
            duration,
        });
    }
}

/// Where `ts` sits within `track`, or `None` if it sits outside it — which
/// happens between a track change and the sender's next `progress:`, when the
/// audio has moved on but the anchor hasn't.
fn extent(track: crate::session::Track, ts: u32, rate: u32) -> Option<(Duration, Duration)> {
    if rate == 0 {
        return None;
    }
    let played = ts.wrapping_sub(track.start);
    let total = track.end.wrapping_sub(track.start);
    if played > total {
        return None;
    }
    let frames = |f: u32| Duration::from_secs_f64(f as f64 / rate as f64);
    Some((frames(played), frames(total)))
}

/// Drives the transition from prebuffering to steady playback: a
/// latency-correct start off the clock model (falling back to a fixed
/// prebuffer).
struct Playout {
    prebuffer: Prebuffer,
    clock: Arc<Mutex<ClockModel>>,
    started: bool,
    first_ts: Option<u32>,
}

impl Playout {
    fn new(prebuffer_packets: usize, clock: Arc<Mutex<ClockModel>>) -> Playout {
        Playout {
            prebuffer: Prebuffer::new(prebuffer_packets),
            clock,
            started: false,
            first_ts: None,
        }
    }

    fn feed(&mut self, ts: u32, pcm: &[i16], sink: &mut dyn AudioSink) {
        if !self.started {
            self.first_ts.get_or_insert(ts);
        }
        let Some(chunk) = self.prebuffer.push(pcm) else {
            return;
        };
        let starting = !self.started;
        if starting {
            self.started = true;
            self.await_start_instant();
        }
        sink.write(&chunk);
    }

    /// Hold the first chunk until its frame's `play_time`, so audio starts at
    /// the instant the client intends. No-op until the clock model is ready.
    fn await_start_instant(&self) {
        let Some(first_ts) = self.first_ts else {
            return;
        };
        let target = self.clock.lock().unwrap().play_time(first_ts);
        let Some(target) = target else {
            debug!("player: start via prebuffer (no clock sync yet)");
            return;
        };
        let now = clock::now_ns();
        if target > now {
            let wait = (target - now).min(2_000_000_000); // cap the wait at 2 s
            debug!(
                "player: latency-correct start, waiting {} ms",
                wait / 1_000_000
            );
            std::thread::sleep(Duration::from_nanos(wait));
        } else {
            debug!("player: latency-correct start (already due, no wait)");
        }
    }

    fn reset(&mut self) {
        self.prebuffer.reset();
        self.started = false;
        self.first_ts = None;
    }
}

/// Accumulates decoded PCM until a packet threshold is reached, then releases
/// it and passes subsequent packets straight through. `reset` re-arms it.
struct Prebuffer {
    threshold_packets: usize,
    packets: usize,
    buffer: Vec<i16>,
    released: bool,
}

impl Prebuffer {
    fn new(threshold_packets: usize) -> Prebuffer {
        Prebuffer {
            threshold_packets,
            packets: 0,
            buffer: Vec::new(),
            released: false,
        }
    }

    /// Feed decoded PCM. Returns the samples to write now, or `None` while
    /// still prebuffering.
    fn push(&mut self, pcm: &[i16]) -> Option<Vec<i16>> {
        if self.released {
            return Some(pcm.to_vec());
        }
        self.buffer.extend_from_slice(pcm);
        self.packets += 1;
        if self.packets >= self.threshold_packets {
            self.released = true;
            Some(std::mem::take(&mut self.buffer))
        } else {
            None
        }
    }

    fn reset(&mut self) {
        self.packets = 0;
        self.buffer.clear();
        self.released = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Track;
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    use std::sync::atomic::AtomicUsize;

    /// Records every delivered chunk and every flush.
    #[derive(Clone, Default)]
    struct Recorder {
        writes: Arc<Mutex<Vec<Vec<i16>>>>,
        flushes: Arc<AtomicUsize>,
    }

    impl AudioSink for Recorder {
        fn write(&mut self, pcm: &[i16]) {
            self.writes.lock().unwrap().push(pcm.to_vec());
        }
        fn flush(&mut self) {
            self.flushes.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn golden_config() -> AlacConfig {
        let fmtp = include_str!("testdata/golden_fmtp.txt").trim();
        AlacConfig::parse(&format!("96 {fmtp}")).unwrap()
    }

    fn golden_packet() -> Vec<u8> {
        STANDARD
            .decode(
                include_str!("testdata/golden_packet.b64")
                    .split_whitespace()
                    .collect::<String>(),
            )
            .unwrap()
    }

    fn golden_pcm() -> Vec<i16> {
        STANDARD
            .decode(
                include_str!("testdata/golden_pcm_i16le.b64")
                    .split_whitespace()
                    .collect::<String>(),
            )
            .unwrap()
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect()
    }

    /// Wait (bounded) until `predicate` holds.
    fn settle(mut predicate: impl FnMut() -> bool) {
        for _ in 0..400 {
            if predicate() {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("player did not settle");
    }

    /// The position reports the playback thread produced for `packets`
    /// packets of a track anchored at `start`..`end`.
    fn positions(track: Option<Track>, packets: u32) -> Vec<(f64, f64)> {
        let config = golden_config();
        let clock = Arc::new(Mutex::new(ClockModel::new(config.sample_rate)));
        let (events, mut rx) = tokio::sync::mpsc::channel(64);
        let recorder = Recorder::default();
        let writes = recorder.writes.clone();
        let player = Player::spawn(
            7,
            &config,
            Box::new(recorder),
            clock,
            events,
            Arc::new(Mutex::new(track)),
        );
        let sender = player.sender();
        let packet = golden_packet();
        for i in 0..packets {
            sender.frame(i * config.frames_per_packet, packet.clone());
        }
        settle(|| !writes.lock().unwrap().is_empty());
        drop(player);

        let mut out = Vec::new();
        while let Ok(Event::Progress {
            elapsed, duration, ..
        }) = rx.try_recv()
        {
            out.push((elapsed.as_secs_f64(), duration.as_secs_f64()));
        }
        out
    }

    #[test]
    fn playback_reports_its_position_once_a_second() {
        // A five-minute track starting at RTP 0. The golden packet is 4096
        // frames, so ~11 packets per second at 44.1 kHz.
        let track = Track {
            start: 0,
            end: 300 * 44100,
        };
        let reports = positions(Some(track), 40);
        assert!(!reports.is_empty(), "playback should report where it is");
        assert!(
            reports
                .iter()
                .all(|(_, total)| (*total - 300.0).abs() < 0.01),
            "the track length comes from the sender's anchor: {reports:?}"
        );
        // One report per second of audio, not one per packet.
        assert!(
            reports.len() <= 5,
            "40 packets is under 4 seconds of audio: {reports:?}"
        );
        // And the position advances with the audio.
        let elapsed: Vec<f64> = reports.iter().map(|(e, _)| *e).collect();
        assert!(
            elapsed.windows(2).all(|w| w[1] > w[0]),
            "position must advance: {elapsed:?}"
        );
    }

    #[test]
    fn no_anchor_means_no_position_reports() {
        // Before the sender's first `progress:` we have no idea where in the
        // track the audio is, and saying nothing beats guessing.
        assert!(positions(None, 40).is_empty());
    }

    #[test]
    fn audio_outside_the_anchored_track_is_not_reported() {
        // Right after a track change the audio has moved past the old track's
        // end, and the sender's new anchor hasn't arrived yet.
        let stale = Track {
            start: 0,
            end: 4096, // one packet long
        };
        let reports = positions(Some(stale), 40);
        assert!(
            reports.iter().all(|(elapsed, total)| elapsed <= total),
            "never report a position past the end of the track: {reports:?}"
        );
    }

    #[test]
    fn position_stops_when_the_audio_does() {
        // The pause case, which needs no pause detection at all: no packets,
        // no reports, so whatever the host last heard stands.
        let track = Track {
            start: 0,
            end: 300 * 44100,
        };
        let config = golden_config();
        let clock = Arc::new(Mutex::new(ClockModel::new(config.sample_rate)));
        let (events, mut rx) = tokio::sync::mpsc::channel(64);
        let recorder = Recorder::default();
        let writes = recorder.writes.clone();
        let player = Player::spawn(
            7,
            &config,
            Box::new(recorder),
            clock,
            events,
            Arc::new(Mutex::new(Some(track))),
        );
        let sender = player.sender();
        let packet = golden_packet();
        for i in 0..40 {
            sender.frame(i * config.frames_per_packet, packet.clone());
        }
        // Wait for the queue to actually drain: audio still in flight is not
        // a pause, it is a backlog.
        settle(|| writes.lock().unwrap().len() >= 39);
        while rx.try_recv().is_ok() {} // drain what playing produced

        // The sender pauses: nothing arrives.
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            rx.try_recv().is_err(),
            "a paused stream must produce no position reports"
        );

        // And resuming picks up again.
        sender.frame(60 * config.frames_per_packet, packet);
        settle(|| !rx.is_empty());
        assert!(matches!(rx.try_recv(), Ok(Event::Progress { .. })));
    }

    #[test]
    fn prebuffer_holds_until_threshold_then_releases() {
        let mut pb = Prebuffer::new(3);
        assert_eq!(pb.push(&[1, 2]), None); // packet 1
        assert_eq!(pb.push(&[3, 4]), None); // packet 2
                                            // Packet 3 hits the threshold and releases the whole accumulation.
        assert_eq!(pb.push(&[5, 6]), Some(vec![1, 2, 3, 4, 5, 6]));
        // Subsequent packets pass straight through.
        assert_eq!(pb.push(&[7, 8]), Some(vec![7, 8]));
    }

    #[test]
    fn reset_rearms_prebuffer() {
        let mut pb = Prebuffer::new(2);
        assert_eq!(pb.push(&[1]), None);
        assert_eq!(pb.push(&[2]), Some(vec![1, 2]));
        pb.reset();
        assert_eq!(pb.push(&[9]), None, "after reset it prebuffers again");
        assert_eq!(pb.push(&[10]), Some(vec![9, 10]));
    }

    #[test]
    fn threshold_of_one_releases_immediately() {
        let mut pb = Prebuffer::new(1);
        assert_eq!(pb.push(&[42, 43]), Some(vec![42, 43]));
    }

    #[test]
    fn prebuffer_packet_count_from_config() {
        let config = AlacConfig::parse("96 352 0 16 40 10 14 2 255 0 0 44100").unwrap();
        // 200 ms = 8820 frames / 352 per packet = 25 packets.
        assert_eq!(prebuffer_packets(&config), 25);
    }

    #[test]
    fn player_sender_drops_overflow_instead_of_growing_without_bound() {
        let (tx, _rx) = std::sync::mpsc::sync_channel(PLAYER_QUEUE_CAPACITY);
        let sender = PlayerSender {
            tx,
            epoch: Arc::new(AtomicU64::new(0)),
            dropped: Arc::new(AtomicU64::new(0)),
        };
        for timestamp in 0..(PLAYER_QUEUE_CAPACITY as u32 + 17) {
            sender.frame(timestamp, vec![0; 64]);
        }
        assert_eq!(sender.dropped(), 17);
        sender.flush();
        assert_eq!(sender.epoch.load(Ordering::Acquire), 1);
    }

    #[test]
    fn delivers_decoded_golden_pcm_to_the_sink() {
        let config = golden_config();
        let prebuffer = prebuffer_packets(&config);
        let packet = golden_packet();
        let expected = golden_pcm();
        let recorder = Recorder::default();
        let writes = recorder.writes.clone();
        let clock = Arc::new(Mutex::new(ClockModel::new(config.sample_rate)));
        let (events, _events_rx) = tokio::sync::mpsc::channel(64);
        let player = Player::spawn(
            7,
            &config,
            Box::new(recorder),
            clock,
            events,
            Arc::new(Mutex::new(None)),
        );
        let sender = player.sender();

        for i in 0..prebuffer {
            sender.frame(i as u32 * config.frames_per_packet, packet.clone());
        }
        settle(|| !writes.lock().unwrap().is_empty());
        drop(player);

        let writes = writes.lock().unwrap();
        assert_eq!(writes.len(), 1, "the prebuffer releases as one chunk");
        assert_eq!(writes[0].len(), prebuffer * expected.len());
        assert_eq!(writes[0][..expected.len()], expected[..]);
    }

    #[test]
    fn flush_drops_prebuffered_audio_and_flushes_the_sink() {
        let config = golden_config();
        let prebuffer = prebuffer_packets(&config);
        let packet = golden_packet();
        let recorder = Recorder::default();
        let writes = recorder.writes.clone();
        let flushes = recorder.flushes.clone();
        let clock = Arc::new(Mutex::new(ClockModel::new(config.sample_rate)));
        let (events, _events_rx) = tokio::sync::mpsc::channel(64);
        let player = Player::spawn(
            7,
            &config,
            Box::new(recorder),
            clock,
            events,
            Arc::new(Mutex::new(None)),
        );
        let sender = player.sender();

        // Below the prebuffer threshold: nothing can have played yet.
        for i in 0..prebuffer - 1 {
            sender.frame(i as u32 * config.frames_per_packet, packet.clone());
        }
        sender.flush();
        settle(|| flushes.load(Ordering::Relaxed) == 1);

        // After the flush the prebuffer is re-armed: a full threshold of new
        // audio plays, and none of the pre-flush audio.
        for i in 0..prebuffer {
            sender.frame((1000 + i as u32) * config.frames_per_packet, packet.clone());
        }
        settle(|| !writes.lock().unwrap().is_empty());
        drop(player);

        let writes = writes.lock().unwrap();
        assert_eq!(writes.len(), 1);
        assert_eq!(
            writes[0].len(),
            prebuffer * golden_pcm().len(),
            "only post-flush audio reaches the sink"
        );
    }

    #[test]
    fn lost_packets_are_concealed_with_silence() {
        let config = golden_config();
        let prebuffer = prebuffer_packets(&config);
        let packet = golden_packet();
        let pcm_len = golden_pcm().len(); // frames_per_packet * channels
        let recorder = Recorder::default();
        let writes = recorder.writes.clone();
        let clock = Arc::new(Mutex::new(ClockModel::new(config.sample_rate)));
        let (events, _events_rx) = tokio::sync::mpsc::channel(64);
        let player = Player::spawn(
            7,
            &config,
            Box::new(recorder),
            clock,
            events,
            Arc::new(Mutex::new(None)),
        );
        let sender = player.sender();

        for i in 0..prebuffer - 1 {
            sender.frame(i as u32 * config.frames_per_packet, packet.clone());
        }
        sender.silence((prebuffer - 1) as u32 * config.frames_per_packet);
        settle(|| !writes.lock().unwrap().is_empty());
        drop(player);

        let writes = writes.lock().unwrap();
        assert_eq!(writes.len(), 1);
        let chunk = &writes[0];
        assert_eq!(chunk.len(), prebuffer * pcm_len);
        assert!(
            chunk[chunk.len() - pcm_len..].iter().all(|&s| s == 0),
            "the lost packet arrives as a silent frame"
        );
    }
}
