use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, TryLockError, Weak};

/// AudioHub's internal sample rate.
pub const AUDIOHUB_RATE: u32 = 48_000;
/// One 10 ms AudioHub mono frame.
pub const AUDIOHUB_FRAME_SAMPLES: usize = 480;

/// Bounds and startup latency for the receiver PCM bus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BusConfig {
    /// Maximum 48 kHz mono samples retained. Oldest samples are discarded on
    /// overflow, independently for every reader.
    pub capacity_samples: usize,
    /// Samples accumulated before a new/reset reader starts. This avoids
    /// presenting a fragmented first frame to the mixer.
    pub prebuffer_samples: usize,
    /// Maximum absolute adaptive resampling correction.
    pub max_correction_ppm: u32,
}

impl Default for BusConfig {
    fn default() -> Self {
        Self {
            capacity_samples: AUDIOHUB_RATE as usize * 2,
            prebuffer_samples: AUDIOHUB_FRAME_SAMPLES * 5,
            // Covers the requested +/-200 ppm independent-clock case while
            // remaining far below a musically meaningful pitch shift.
            max_correction_ppm: 500,
        }
    }
}

impl BusConfig {
    pub(crate) fn validate(self) -> std::io::Result<Self> {
        if self.capacity_samples < AUDIOHUB_FRAME_SAMPLES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "AirPlay PCM capacity must hold at least one 10 ms frame",
            ));
        }
        if self.prebuffer_samples > self.capacity_samples {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "AirPlay PCM prebuffer exceeds its bounded capacity",
            ));
        }
        if self.max_correction_ppm > 2_000 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "AirPlay resampling correction exceeds the 2000 ppm safety limit",
            ));
        }
        Ok(self)
    }
}

#[derive(Debug)]
struct ReaderState {
    cursor: u64,
    epoch: u64,
    primed: bool,
    dropped_pending: u64,
}

#[derive(Debug)]
struct State {
    samples: VecDeque<f32>,
    base_seq: u64,
    next_seq: u64,
    epoch: u64,
    active_session: Option<u64>,
    next_reader: u64,
    readers: HashMap<u64, ReaderState>,
}

#[derive(Debug)]
struct Inner {
    config: BusConfig,
    // Zero means idle; runtime session ids start at one. This mirrors the
    // mutex-owned field so deadline readers can distinguish lock contention
    // from a real disconnect without waiting for the producer.
    active_session: AtomicU64,
    state: Mutex<State>,
}

/// A bounded 48 kHz mono PCM bus with independent reader cursors.
///
/// A slow or abandoned reader can lose old samples, but it cannot make the
/// producer allocate without bound or hold another reader back. Starting a
/// stream and flushing it reset every cursor atomically, so no reader can mix
/// samples from two AirPlay sessions.
#[derive(Debug, Clone)]
pub struct PcmBus {
    inner: Arc<Inner>,
}

impl PcmBus {
    /// Create a bus. Invalid values are rejected by runtime startup; this
    /// constructor clamps them as a defensive convenience for direct users.
    pub fn new(mut config: BusConfig) -> Self {
        config.capacity_samples = config.capacity_samples.max(AUDIOHUB_FRAME_SAMPLES);
        config.prebuffer_samples = config.prebuffer_samples.min(config.capacity_samples);
        config.max_correction_ppm = config.max_correction_ppm.min(2_000);
        Self {
            inner: Arc::new(Inner {
                config,
                active_session: AtomicU64::new(0),
                state: Mutex::new(State {
                    samples: VecDeque::with_capacity(config.capacity_samples),
                    base_seq: 0,
                    next_seq: 0,
                    epoch: 0,
                    active_session: None,
                    next_reader: 1,
                    readers: HashMap::new(),
                }),
            }),
        }
    }

    /// Subscribe an independent non-blocking reader.
    pub fn subscribe(&self) -> PcmReader {
        let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        let id = state.next_reader;
        state.next_reader = state.next_reader.wrapping_add(1).max(1);
        let cursor = state.next_seq;
        let epoch = state.epoch;
        state.readers.insert(
            id,
            ReaderState {
                cursor,
                epoch,
                primed: false,
                dropped_pending: 0,
            },
        );
        PcmReader {
            inner: Arc::downgrade(&self.inner),
            id,
        }
    }

    /// Session whose samples currently own the bus. When protocol sessions
    /// overlap, the most recently created sink wins.
    pub fn active_session(&self) -> Option<u64> {
        session_from_wire(self.inner.active_session.load(Ordering::Acquire))
    }

    pub(crate) fn activate(&self, session_id: u64) {
        debug_assert_ne!(session_id, 0, "zero is the inactive-session sentinel");
        let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        reset_locked(&mut state, Some(session_id));
        self.inner
            .active_session
            .store(session_id, Ordering::Release);
    }

    pub(crate) fn end(&self, session_id: u64) {
        let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.active_session == Some(session_id) {
            reset_locked(&mut state, None);
            self.inner.active_session.store(0, Ordering::Release);
        }
    }

    pub(crate) fn flush(&self, session_id: u64) {
        let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.active_session == Some(session_id) {
            reset_locked(&mut state, Some(session_id));
        }
    }

    pub(crate) fn push(&self, session_id: u64, samples: &[f32]) -> bool {
        if samples.is_empty() {
            return true;
        }
        let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.active_session != Some(session_id) {
            return false;
        }

        for &sample in samples {
            state
                .samples
                .push_back(if sample.is_finite() { sample } else { 0.0 });
            state.next_seq = state.next_seq.saturating_add(1);
        }

        let overflow = state
            .samples
            .len()
            .saturating_sub(self.inner.config.capacity_samples);
        if overflow != 0 {
            state.samples.drain(..overflow);
            state.base_seq = state.base_seq.saturating_add(overflow as u64);
            let base = state.base_seq;
            for reader in state.readers.values_mut() {
                if reader.cursor < base {
                    reader.dropped_pending = reader
                        .dropped_pending
                        .saturating_add(base.saturating_sub(reader.cursor));
                    reader.cursor = base;
                }
            }
        }
        true
    }

    /// Feedback for the streaming resampler. The fastest primed reader is the
    /// clock reference, so a stale observer cannot bend the audio rate. A
    /// small proportional offset is enough: protocol playback is already
    /// paced, this loop only cancels the residual oscillator mismatch.
    pub(crate) fn correction_ppm(&self) -> f64 {
        let state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        let fill = state
            .readers
            .values()
            .filter(|r| r.primed && r.epoch == state.epoch)
            .map(|r| state.next_seq.saturating_sub(r.cursor))
            .min();
        let Some(fill) = fill else {
            return 0.0;
        };
        // read_into is normally followed by the producer write. Target the
        // post-read waterline rather than the prebuffer threshold itself.
        let target = self
            .inner
            .config
            .prebuffer_samples
            .saturating_sub(AUDIOHUB_FRAME_SAMPLES) as f64;
        let error = target - fill as f64;
        (error * 0.25).clamp(
            -(self.inner.config.max_correction_ppm as f64),
            self.inner.config.max_correction_ppm as f64,
        )
    }

    #[cfg(test)]
    pub(crate) fn unread_for(&self, reader_id: u64) -> u64 {
        let state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        state
            .readers
            .get(&reader_id)
            .map(|r| state.next_seq.saturating_sub(r.cursor))
            .unwrap_or(0)
    }
}

fn reset_locked(state: &mut State, active_session: Option<u64>) {
    state.samples.clear();
    state.base_seq = state.next_seq;
    state.epoch = state.epoch.wrapping_add(1);
    state.active_session = active_session;
    let cursor = state.next_seq;
    let epoch = state.epoch;
    for reader in state.readers.values_mut() {
        reader.cursor = cursor;
        reader.epoch = epoch;
        reader.primed = false;
        reader.dropped_pending = 0;
    }
}

/// Result of one non-blocking PCM bus read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PcmRead {
    /// Samples copied from the active AirPlay stream.
    pub copied: usize,
    /// Silence samples written because the reader was prebuffering or starved.
    pub silence: usize,
    /// Samples this reader lost to bounded-ring overflow since its last read.
    pub dropped: u64,
    /// Active AirPlay session, if any.
    pub session_id: Option<u64>,
    /// Changes on a new stream or flush.
    pub epoch: u64,
}

/// Independent cursor into a [`PcmBus`].
#[derive(Debug)]
pub struct PcmReader {
    inner: Weak<Inner>,
    id: u64,
}

impl PcmReader {
    /// Fill `out` without blocking. Unavailable samples are zero-filled.
    pub fn read_into(&mut self, out: &mut [f32]) -> PcmRead {
        out.fill(0.0);
        let Some(inner) = self.inner.upgrade() else {
            return PcmRead {
                copied: 0,
                silence: out.len(),
                dropped: 0,
                session_id: None,
                epoch: 0,
            };
        };
        let active_session_hint = session_from_wire(inner.active_session.load(Ordering::Acquire));
        // This reader runs on AudioHub's 10 ms deadline thread. A concurrent
        // A network-media write must never park that thread: retain the cursor
        // and emit one silent frame, then catch up on the next tick.
        let mut state = match inner.state.try_lock() {
            Ok(state) => state,
            Err(TryLockError::WouldBlock) => {
                return PcmRead {
                    copied: 0,
                    silence: out.len(),
                    dropped: 0,
                    session_id: active_session_hint,
                    epoch: 0,
                };
            }
            Err(TryLockError::Poisoned(error)) => error.into_inner(),
        };
        let active_session = state.active_session;
        let epoch = state.epoch;
        let next_seq = state.next_seq;
        let base_seq = state.base_seq;
        let prebuffer = inner.config.prebuffer_samples as u64;

        let Some(reader) = state.readers.get_mut(&self.id) else {
            return PcmRead {
                copied: 0,
                silence: out.len(),
                dropped: 0,
                session_id: active_session,
                epoch,
            };
        };

        if reader.epoch != epoch {
            reader.cursor = base_seq;
            reader.epoch = epoch;
            reader.primed = false;
            reader.dropped_pending = 0;
        }
        if reader.cursor < base_seq {
            reader.dropped_pending = reader
                .dropped_pending
                .saturating_add(base_seq.saturating_sub(reader.cursor));
            reader.cursor = base_seq;
        }

        let unread = next_seq.saturating_sub(reader.cursor);
        if active_session.is_none() || (!reader.primed && unread < prebuffer) {
            return PcmRead {
                copied: 0,
                silence: out.len(),
                dropped: std::mem::take(&mut reader.dropped_pending),
                session_id: active_session,
                epoch,
            };
        }
        reader.primed = true;

        let copied = (unread as usize).min(out.len());
        let offset = reader.cursor.saturating_sub(base_seq) as usize;
        reader.cursor = reader.cursor.saturating_add(copied as u64);
        if copied < out.len() {
            // Rebuffer after an underrun. Otherwise every later block would
            // be played one callback late and latency would ratchet upward.
            reader.primed = false;
            reader.cursor = next_seq;
        }
        let dropped = std::mem::take(&mut reader.dropped_pending);
        // End the mutable reader borrow before indexing the ring itself.
        let _ = reader;
        for (dst, src_index) in out.iter_mut().take(copied).zip(offset..) {
            *dst = state.samples.get(src_index).copied().unwrap_or(0.0);
        }
        PcmRead {
            copied,
            silence: out.len() - copied,
            dropped,
            session_id: active_session,
            epoch,
        }
    }

    #[cfg(test)]
    pub(crate) fn id(&self) -> u64 {
        self.id
    }
}

fn session_from_wire(value: u64) -> Option<u64> {
    (value != 0).then_some(value)
}

impl Drop for PcmReader {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.upgrade() {
            inner
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .readers
                .remove(&self.id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_bus() -> PcmBus {
        PcmBus::new(BusConfig {
            capacity_samples: 960,
            prebuffer_samples: 0,
            max_correction_ppm: 500,
        })
    }

    #[test]
    fn readers_have_independent_cursors() {
        let bus = tiny_bus();
        let mut a = bus.subscribe();
        let mut b = bus.subscribe();
        bus.activate(7);
        assert!(bus.push(7, &[1.0, 2.0, 3.0, 4.0]));

        let mut two = [0.0; 2];
        assert_eq!(a.read_into(&mut two).copied, 2);
        assert_eq!(two, [1.0, 2.0]);
        let mut four = [0.0; 4];
        assert_eq!(b.read_into(&mut four).copied, 4);
        assert_eq!(four, [1.0, 2.0, 3.0, 4.0]);
        assert_eq!(a.read_into(&mut two).copied, 2);
        assert_eq!(two, [3.0, 4.0]);
    }

    #[test]
    fn overflow_is_bounded_and_reported_per_reader() {
        let bus = tiny_bus();
        let mut reader = bus.subscribe();
        bus.activate(9);
        let samples: Vec<f32> = (0..1_100).map(|n| n as f32).collect();
        bus.push(9, &samples);
        let mut out = [0.0; 2];
        let report = reader.read_into(&mut out);
        assert_eq!(report.dropped, 140);
        assert_eq!(out, [140.0, 141.0]);
    }

    #[test]
    fn new_session_invalidates_old_sink_and_resets_readers() {
        let bus = tiny_bus();
        let mut reader = bus.subscribe();
        bus.activate(1);
        bus.push(1, &[0.5, 0.6]);
        bus.activate(2);
        assert!(!bus.push(1, &[0.9]));
        assert!(bus.push(2, &[0.2]));
        let mut out = [0.0; 1];
        let report = reader.read_into(&mut out);
        assert_eq!(report.session_id, Some(2));
        assert_eq!(out, [0.2]);
    }

    #[test]
    fn correction_uses_fastest_reader_not_stale_observer() {
        let bus = PcmBus::new(BusConfig {
            capacity_samples: 4_800,
            prebuffer_samples: 960,
            max_correction_ppm: 500,
        });
        let mut realtime = bus.subscribe();
        let _stale = bus.subscribe();
        bus.activate(1);
        bus.push(1, &vec![0.0; 960]);
        realtime.read_into(&mut [0.0; 480]);
        // The realtime reader is at the post-read target (480); the stale
        // observer still has 960 unread but must not influence the servo.
        assert_eq!(bus.unread_for(realtime.id()), 480);
        assert!(bus.correction_ppm().abs() < f64::EPSILON);
    }

    #[test]
    fn deadline_reader_never_waits_for_a_contended_bus() {
        let bus = tiny_bus();
        let mut reader = bus.subscribe();
        bus.activate(1);
        bus.push(1, &[0.25, 0.5]);

        let guard = bus.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut out = [1.0; 2];
        let report = reader.read_into(&mut out);
        assert_eq!(report.copied, 0);
        assert_eq!(report.silence, 2);
        assert_eq!(report.session_id, Some(1), "contention is not a disconnect");
        assert_eq!(out, [0.0, 0.0]);
        drop(guard);

        assert_eq!(reader.read_into(&mut out).copied, 2);
        assert_eq!(out, [0.25, 0.5], "content remains queued after contention");
    }
}
