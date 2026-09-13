use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

/// A receiver-owned estimate, not a claim of exact acoustic presentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputTiming {
    /// Changes when the output presentation timeline is invalidated, including
    /// a scheduling discontinuity which does not replace the native stream.
    pub epoch: u64,
    /// Total protocol-to-output lead, including this bus's startup cushion.
    pub lead: Duration,
    /// Configured staging cushion only; excludes downstream and converter delay.
    pub staging_slack: Duration,
    pub estimated: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PcmWriteTiming {
    pub epoch: u64,
    pub presentation: Instant,
}

/// AudioHub's internal sample rate.
pub const AUDIOHUB_RATE: u32 = 48_000;
/// Frames per 10 ms at AudioHub's internal rate.
pub const AUDIOHUB_FRAME_SAMPLES: usize = 480;

/// Bounds and startup latency for the receiver PCM bus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BusConfig {
    /// Maximum 48 kHz stereo frames retained. The legacy field name is kept for
    /// config compatibility; one count is one `[left, right]` frame.
    pub capacity_samples: usize,
    /// Stereo frames accumulated before a new/reset reader starts. The legacy
    /// field name is kept for config compatibility.
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

// All concurrently accessed ring/control/feedback fields use SeqCst. Payload
// remains atomic even when a reader subsequently rejects a torn slot snapshot.
#[derive(Debug, Default)]
struct AtomicFrame {
    sequence: AtomicU64,
    lr: AtomicU64,
    time_ns: AtomicU64,
    step: AtomicU64,
    end: AtomicU64,
    presentation_epoch: AtomicU64,
}

#[derive(Debug, Default)]
struct Feedback {
    version: AtomicU64,
    cursor: AtomicU64,
    epoch: AtomicU64,
    presentation_epoch: AtomicU64,
    primed: AtomicBool,
    retired: AtomicBool,
}

#[derive(Debug)]
struct State {
    next_seq: u64,
    epoch: u64,
    active_session: Option<u64>,
    next_reader: u64,
    readers: HashMap<u64, Arc<Feedback>>,
    presentation_epoch: u64,
    timed: bool,
}

#[derive(Debug)]
struct Inner {
    config: BusConfig,
    slots: Box<[AtomicFrame]>,
    head: AtomicU64,
    floor: AtomicU64,
    reset_seq: AtomicU64,
    control: AtomicU64,
    timed: AtomicBool,
    presentation_epoch: AtomicU64,
    required_presentation_epoch: AtomicU64,
    active_session: AtomicU64,
    state: Mutex<State>,
    timing_base: Instant,
    timing_version: AtomicU64,
    timing_epoch: AtomicU64,
    timing_delay_us: AtomicU64,
    timing_updated_ns: AtomicU64,
    timing_estimated: AtomicBool,
}

#[derive(Clone, Copy)]
struct FrameSnapshot {
    lr: [f32; 2],
    time_ns: i64,
    step: f64,
    end: u64,
    presentation_epoch: u64,
}

impl Inner {
    fn frame(&self, q: u64) -> Option<FrameSnapshot> {
        let expected = q.checked_mul(2)?.checked_add(2)?;
        let slot = &self.slots[q as usize % self.slots.len()];
        if slot.sequence.load(Ordering::SeqCst) != expected { return None; }
        let lr = slot.lr.load(Ordering::SeqCst);
        let frame = FrameSnapshot {
            lr: [f32::from_bits(lr as u32), f32::from_bits((lr >> 32) as u32)],
            time_ns: slot.time_ns.load(Ordering::SeqCst) as i64,
            step: f64::from_bits(slot.step.load(Ordering::SeqCst)),
            end: slot.end.load(Ordering::SeqCst),
            presentation_epoch: slot.presentation_epoch.load(Ordering::SeqCst),
        };
        (slot.sequence.load(Ordering::SeqCst) == expected).then_some(frame)
    }
}

/// A bounded 48 kHz stereo-frame PCM bus with independent reader cursors.
///
/// A slow or abandoned reader can lose old frames, but it cannot make the
/// producer allocate without bound or hold another reader back. Starting a
/// stream and flushing it reset every cursor atomically, so no reader can mix
/// frames from two AirPlay sessions.
#[derive(Debug, Clone)]
pub struct PcmBus {
    inner: Arc<Inner>,
}

impl PcmBus {
    /// Create a bus. Invalid values are rejected by runtime startup; this
    /// constructor clamps them as a defensive convenience for direct users.
    pub fn new(mut config: BusConfig) -> Self {
        // Construction runs on the controller/startup thread. Initialize UUID
        // and reset metadata here so the first realtime read cannot initialize
        // the process-wide OnceLock.
        crate::telemetry::prewarm();
        config.capacity_samples = config.capacity_samples.max(AUDIOHUB_FRAME_SAMPLES);
        config.prebuffer_samples = config.prebuffer_samples.min(config.capacity_samples);
        config.max_correction_ppm = config.max_correction_ppm.min(2_000);
        Self {
            inner: Arc::new(Inner {
                config,
                slots: (0..config.capacity_samples).map(|_| AtomicFrame::default()).collect(),
                head: AtomicU64::new(0),
                floor: AtomicU64::new(0),
                reset_seq: AtomicU64::new(0),
                control: AtomicU64::new(0),
                timed: AtomicBool::new(false),
                presentation_epoch: AtomicU64::new(0),
                required_presentation_epoch: AtomicU64::new(0),
                active_session: AtomicU64::new(0),
                timing_base: Instant::now(),
                timing_version: AtomicU64::new(0),
                timing_epoch: AtomicU64::new(0),
                timing_delay_us: AtomicU64::new(0),
                timing_updated_ns: AtomicU64::new(0),
                timing_estimated: AtomicBool::new(true),
                state: Mutex::new(State {

                    next_seq: 0,
                    epoch: 0,
                    active_session: None,
                    next_reader: 1,
                    readers: HashMap::new(),
                    presentation_epoch: 0,
                    timed: false,
                }),
            }),
        }
    }

    /// Subscribe an independent non-blocking reader.
    pub fn subscribe(&self) -> PcmReader {
        let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        let id = state.next_reader;
        state.next_reader = state.next_reader.checked_add(1).expect("PCM reader ids exhausted");
        state.readers.retain(|_, r| !r.retired.load(Ordering::SeqCst));
        let feedback = Arc::new(Feedback::default());
        feedback.cursor.store(state.next_seq, Ordering::SeqCst);
        state.readers.insert(id, feedback.clone());
        PcmReader {
            inner: Arc::downgrade(&self.inner),
            id,
            feedback,
            cursor: state.next_seq,
            epoch: state.epoch,
            presentation_epoch: self.inner.required_presentation_epoch.load(Ordering::SeqCst),
            primed: false,
            needs_timed_realign: false,
        }
    }

    /// Session whose samples currently own the bus. When protocol sessions
    /// overlap, the most recently created sink wins.
    pub fn active_session(&self) -> Option<u64> {
        session_from_wire(self.inner.active_session.load(Ordering::SeqCst))
    }

    /// Single writer: the application's local output owner. No locks, waits or
    /// allocation on its mixer thread. Epoch zero invalidates the observation.
    pub fn publish_output_timing(&self, epoch: u64, downstream: Duration, estimated: bool) {
        let epoch = if downstream <= Duration::from_secs(1) { epoch } else { 0 };
        let inner = &self.inner;
        let version = inner.timing_version.load(Ordering::SeqCst);
        let Some(next_version) = version.checked_add(2) else {
            inner.timing_version.store(u64::MAX, Ordering::SeqCst);
            return;
        };
        inner.timing_version.store(next_version - 1, Ordering::SeqCst);
        inner.timing_epoch.store(epoch, Ordering::SeqCst);
        inner.timing_delay_us.store(downstream.as_micros().min(u64::MAX as u128) as u64, Ordering::SeqCst);
        inner.timing_updated_ns.store(inner.timing_base.elapsed().as_nanos().min(u64::MAX as u128) as u64, Ordering::SeqCst);
        inner.timing_estimated.store(estimated, Ordering::SeqCst);
        inner.timing_version.store(next_version, Ordering::SeqCst);
    }

    pub(crate) fn output_timing(&self) -> Option<OutputTiming> {
        let inner = &self.inner;
        let version = inner.timing_version.load(Ordering::SeqCst);
        if version & 1 != 0 { return None; }
        let epoch = inner.timing_epoch.load(Ordering::SeqCst);
        let delay_us = inner.timing_delay_us.load(Ordering::SeqCst);
        let updated = inner.timing_updated_ns.load(Ordering::SeqCst);
        let estimated = inner.timing_estimated.load(Ordering::SeqCst);
        let now = inner.timing_base.elapsed().as_nanos().min(u64::MAX as u128) as u64;
        std::sync::atomic::fence(Ordering::SeqCst);
        if epoch == 0 || now.saturating_sub(updated) > 250_000_000
            || version != inner.timing_version.load(Ordering::SeqCst) { return None; }
        let cushion = Duration::from_secs_f64(self.inner.config.prebuffer_samples as f64 / AUDIOHUB_RATE as f64);
        Some(OutputTiming { epoch, lead: Duration::from_micros(delay_us) + cushion, staging_slack: cushion, estimated })
    }

    pub(crate) fn activate(&self, session_id: u64) {
        debug_assert_ne!(session_id, 0, "zero is the inactive-session sentinel");
        let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        state.timed = false;
        reset_locked(&self.inner, &mut state, Some(session_id));
    }

    pub(crate) fn end(&self, session_id: u64) {
        let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.active_session == Some(session_id) {
            reset_locked(&self.inner, &mut state, None);
            self.inner.active_session.store(0, Ordering::SeqCst);
        }
    }

    pub(crate) fn flush(&self, session_id: u64) {
        let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.active_session == Some(session_id) {
            reset_locked(&self.inner, &mut state, Some(session_id));
        }
    }

    pub(crate) fn push(&self, session_id: u64, frames: &[[f32; 2]]) -> bool {
        let telemetry_token = crate::telemetry::operation_token();
        self.push_with_token(session_id, frames, telemetry_token)
    }

    pub(crate) fn push_with_token(
        &self,
        session_id: u64,
        frames: &[[f32; 2]],
        telemetry_token: crate::telemetry::OperationToken,
    ) -> bool {
        self.push_timed_with_token(session_id, frames, telemetry_token, None)
    }

    pub(crate) fn push_timed_with_token(
        &self,
        session_id: u64,
        frames: &[[f32; 2]],
        telemetry_token: crate::telemetry::OperationToken,
        timing: Option<PcmWriteTiming>,
    ) -> bool {
        self.push_resampled_with_token(session_id, frames, telemetry_token, timing, 1.0 / AUDIOHUB_RATE as f64)
    }

    pub(crate) fn push_resampled_with_token(
        &self,
        session_id: u64,
        frames: &[[f32; 2]],
        telemetry_token: crate::telemetry::OperationToken,
        timing: Option<PcmWriteTiming>,
        frame_seconds: f64,
    ) -> bool {
        if frames.is_empty() {
            return true;
        }
        if !frame_seconds.is_finite() || frame_seconds <= 0.0 {
            return false;
        }
        let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.active_session != Some(session_id) {
            let active_session = state.active_session;
            drop(state);
            crate::telemetry::record_pcm_bus_push(
                telemetry_token,
                session_id,
                active_session,
                frames.len(),
                0,
            );
            return false;
        }

        let inner = &self.inner;
        let gate = inner.required_presentation_epoch.load(Ordering::SeqCst);
        if timing.is_some_and(|t| t.epoch == 0 || t.epoch != gate) { return false; }
        let Some(end) = state.next_seq.checked_add(frames.len() as u64)
            .filter(|end| *end <= u64::MAX / 2) else { return false; };
        // Validate the entire time range before changing the ring.
        let first_ns = timing.map(|t| signed_seconds(t.presentation, inner.timing_base) * 1e9);
        let last_ns = first_ns.map(|ns| ns + (frames.len() - 1) as f64 * frame_seconds * 1e9);
        if first_ns.into_iter().chain(last_ns).any(|ns| !ns.is_finite() || ns <= i64::MIN as f64 || ns >= i64::MAX as f64) { return false; }
        if timing.is_some() && (!state.timed || state.presentation_epoch != gate) {
            state.timed = true;
            state.presentation_epoch = gate;
            reset_locked(inner, &mut state, Some(session_id));
        }
        if state.epoch == u64::MAX { return false; }
        state.readers.retain(|_, r| !r.retired.load(Ordering::SeqCst));
        let old_floor = inner.floor.load(Ordering::SeqCst);
        for (index, &frame) in frames.iter().enumerate() {
            let q = state.next_seq;
            // Eviction precedes touching a target slot; unrelated appends never
            // change the control version used to validate a whole read.
            let floor = inner.reset_seq.load(Ordering::SeqCst)
                .max((q + 1).saturating_sub(inner.slots.len() as u64));
            inner.floor.store(floor, Ordering::SeqCst);
            let slot = &inner.slots[q as usize % inner.slots.len()];
            slot.sequence.store(q * 2 + 1, Ordering::SeqCst);
            let left = if frame[0].is_finite() { frame[0] } else { 0.0 };
            let right = if frame[1].is_finite() { frame[1] } else { 0.0 };
            slot.lr.store(left.to_bits() as u64 | ((right.to_bits() as u64) << 32), Ordering::SeqCst);
            slot.time_ns.store(first_ns.map(|ns| (ns + index as f64 * frame_seconds * 1e9).round() as i64 as u64).unwrap_or(0), Ordering::SeqCst);
            slot.step.store(frame_seconds.to_bits(), Ordering::SeqCst);
            slot.end.store(end, Ordering::SeqCst);
            slot.presentation_epoch.store(timing.map(|t| t.epoch).unwrap_or(0), Ordering::SeqCst);
            slot.sequence.store(q * 2 + 2, Ordering::SeqCst);
            state.next_seq += 1;
            inner.head.store(state.next_seq, Ordering::SeqCst);
        }
        let overflow = inner.floor.load(Ordering::SeqCst).saturating_sub(old_floor) as usize;
        let accepted = timing.is_none() || inner.required_presentation_epoch.load(Ordering::SeqCst) == gate;
        let active_session = state.active_session;
        drop(state);
        crate::telemetry::record_pcm_bus_push(telemetry_token, session_id, active_session, frames.len(), overflow);
        accepted
    }

    /// Feedback for the streaming resampler. The fastest primed reader is the
    /// clock reference, so a stale observer cannot bend the audio rate. A
    /// small proportional offset is enough: protocol playback is already
    /// paced, this loop only cancels the residual oscillator mismatch.
    pub(crate) fn correction_ppm(&self) -> f64 {
        let state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        let fill = state.readers.values().filter_map(|r| {
            let version = r.version.load(Ordering::SeqCst);
            let cursor = r.cursor.load(Ordering::SeqCst);
            let epoch = r.epoch.load(Ordering::SeqCst);
            let gate = r.presentation_epoch.load(Ordering::SeqCst);
            let primed = r.primed.load(Ordering::SeqCst);
            if version & 1 != 0 || version != r.version.load(Ordering::SeqCst)
                || r.retired.load(Ordering::SeqCst) || !primed || epoch != state.epoch
                || (state.timed && gate != self.inner.required_presentation_epoch.load(Ordering::SeqCst)) {
                None
            } else { Some(state.next_seq.saturating_sub(cursor.max(self.inner.floor.load(Ordering::SeqCst)))) }
        }).min();
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
            .map(|r| state.next_seq.saturating_sub(r.cursor.load(Ordering::SeqCst).max(self.inner.floor.load(Ordering::SeqCst))))
            .unwrap_or(0)
    }
}

fn reset_locked(inner: &Inner, state: &mut State, active_session: Option<u64>) {
    // Exhaustion permanently closes this instance; generations are never reused.
    let Some(epoch) = state.epoch.checked_add(2) else {
        state.epoch = u64::MAX;
        state.active_session = None;
        inner.control.store(u64::MAX, Ordering::SeqCst);
        inner.active_session.store(0, Ordering::SeqCst);
        return;
    };
    inner.control.store(epoch - 1, Ordering::SeqCst);
    state.epoch = epoch;
    state.active_session = active_session;
    inner.reset_seq.store(state.next_seq, Ordering::SeqCst);
    inner.floor.store(state.next_seq, Ordering::SeqCst);
    inner.active_session.store(active_session.unwrap_or(0), Ordering::SeqCst);
    inner.timed.store(state.timed, Ordering::SeqCst);
    inner.presentation_epoch.store(state.presentation_epoch, Ordering::SeqCst);
    inner.control.store(epoch, Ordering::SeqCst);
}

/// Result of one non-blocking PCM bus read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PcmRead {
    /// Stereo frames copied from the active AirPlay stream.
    pub copied: usize,
    /// Silent stereo frames written because the reader was prebuffering or starved.
    pub silence: usize,
    /// Stereo frames this reader lost to overflow since its last read.
    pub dropped: u64,
    /// Frames intentionally skipped while aligning a timed startup/resume.
    /// Unlike `dropped`, these were not lost to bounded-bus overflow.
    pub late: u64,
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
    feedback: Arc<Feedback>,
    cursor: u64,
    epoch: u64,
    presentation_epoch: u64,
    primed: bool,
    // Real missing output slots require fresh timestamp alignment.
    needs_timed_realign: bool,
}

impl PcmReader {
    /// Fill legacy mono output without blocking by explicitly downmixing L/R.
    pub fn read_into(&mut self, out: &mut [f32]) -> PcmRead {
        out.fill(0.0);
        self.read_frames(out.len(), None, |index, frame| {
            out[index] = (frame[0] + frame[1]) * 0.5;
        })
    }

    /// Fill native stereo frames without blocking. Unavailable frames are silent.
    pub fn read_stereo_into(&mut self, out: &mut [[f32; 2]]) -> PcmRead {
        out.fill([0.0; 2]);
        self.read_frames(out.len(), None, |index, frame| out[index] = frame)
    }

    /// Align only startup/recovery to the next output block's content time.
    /// Established readers retain continuous samples and smooth clock correction.
    /// Untimed protocols keep their original prebuffer semantics.
    pub fn read_stereo_timed(
        &mut self,
        out: &mut [[f32; 2]],
        presentation_epoch: u64,
        next_output: Option<Instant>,
    ) -> PcmRead {
        out.fill([0.0; 2]);
        self.read_frames(out.len(), Some((presentation_epoch, next_output)), |index, frame| out[index] = frame)
    }

    fn read_frames(
        &mut self,
        frame_count: usize,
        timing: Option<(u64, Option<Instant>)>,
        mut write_frame: impl FnMut(usize, [f32; 2]),
    ) -> PcmRead {
        let telemetry_token = crate::telemetry::operation_token();
        let Some(inner) = self.inner.upgrade() else {
            return finish_read(
                telemetry_token,
                PcmRead {
                    copied: 0,
                    silence: frame_count,
                    dropped: 0,
                    late: 0,
                    session_id: None,
                    epoch: 0,
                },
                false,
                false,
            );
        };
        if let Some((gate, _)) = timing {
            inner.required_presentation_epoch.store(gate, Ordering::SeqCst);
        }
        let epoch = inner.control.load(Ordering::SeqCst);
        let gate = inner.required_presentation_epoch.load(Ordering::SeqCst);
        let session = session_from_wire(inner.active_session.load(Ordering::SeqCst));
        let reset = inner.reset_seq.load(Ordering::SeqCst);
        let floor = inner.floor.load(Ordering::SeqCst);
        let head = inner.head.load(Ordering::SeqCst);
        let stream_timed = inner.timed.load(Ordering::SeqCst);
        let installed_gate = inner.presentation_epoch.load(Ordering::SeqCst);
        let timed = timing.is_some() && stream_timed;
        let mut read = PcmRead { copied: 0, silence: frame_count, dropped: 0, late: 0, session_id: session, epoch };
        if epoch & 1 != 0 || epoch != inner.control.load(Ordering::SeqCst) {
            read.epoch = 0;
            self.primed = false;
            return finish_read(telemetry_token, read, false, false);
        }
        if self.epoch != epoch {
            self.cursor = reset;
            self.epoch = epoch;
            self.primed = false;
        }
        if stream_timed && self.presentation_epoch != gate {
            self.primed = false;
        }
        self.presentation_epoch = gate;
        if timed && self.needs_timed_realign {
            self.primed = false;
            self.needs_timed_realign = false;
        }
        if self.cursor < floor {
            read.dropped = floor - self.cursor;
            self.cursor = floor;
            if timed { self.primed = false; }
        }
        let mut waiting = session.is_none() || (stream_timed && installed_gate != gate);
        let mut leading = 0;
        if !waiting && !self.primed && timed {
            if let Some(at) = timing.and_then(|(_, at)| at) {
                let target_ns = signed_seconds(at, inner.timing_base) * 1e9;
                // At most capacity snapshots: every iteration advances a span
                // or stops. Each slot owns its exact converted PTS and step.
                for _ in 0..inner.slots.len() {
                    if self.cursor >= head { break; }
                    let Some(frame) = inner.frame(self.cursor) else {
                        // A concurrently evicted alignment slot does not
                        // authorize copying its neighbors without a PTS check.
                        // Re-sample the floor and align on the next bounded read.
                        waiting = true;
                        break;
                    };
                    if frame.presentation_epoch != gate { waiting = true; break; }
                    let seconds = (target_ns - frame.time_ns as f64) / 1e9;
                    let delta = (seconds / frame.step).round();
                    if delta < 0.0 {
                        leading = (-seconds * AUDIOHUB_RATE as f64).round().max(0.0).min(frame_count as f64) as usize;
                        waiting = leading == frame_count;
                        break;
                    }
                    let skip = (delta as u64).min(frame.end.min(head).saturating_sub(self.cursor));
                    self.cursor += skip;
                    read.late += skip;
                    if self.cursor < frame.end || skip == 0 { break; }
                }
            } else { waiting = true; }
        }
        let unread = head.saturating_sub(self.cursor);
        waiting |= !self.primed && !timed && unread < inner.config.prebuffer_samples as u64;
        let wanted = frame_count - leading;
        let mut lost = false;
        if !waiting {
            let available = unread.min(wanted as u64) as usize;
            for i in 0..available {
                let q = self.cursor;
                match inner.frame(q) {
                    Some(frame) if !stream_timed || frame.presentation_epoch == gate => {
                        write_frame(leading + i, frame.lr);
                        read.copied += 1;
                    }
                    Some(_) => { lost = true; }
                    None => { read.dropped += 1; lost = true; }
                }
                self.cursor += 1;
            }
            self.primed = read.copied == wanted && wanted != 0;
            if available < wanted || lost { self.primed = false; }
        }
        // The final control/gate check is the read linearization point. A real
        // reset invalidates all writes to the caller, never a normal append.
        if inner.control.load(Ordering::SeqCst) != epoch
            || (stream_timed && inner.required_presentation_epoch.load(Ordering::SeqCst) != gate) {
            for i in 0..frame_count { write_frame(i, [0.0; 2]); }
            self.primed = false;
            read.copied = 0;
            read.dropped = 0;
            read.late = 0;
            read.epoch = 0;
        }
        read.silence = frame_count - read.copied;
        let feedback = &self.feedback;
        let version = feedback.version.load(Ordering::SeqCst);
        if let Some(next) = version.checked_add(2) {
            feedback.version.store(next - 1, Ordering::SeqCst);
            feedback.cursor.store(self.cursor, Ordering::SeqCst);
            feedback.epoch.store(self.epoch, Ordering::SeqCst);
            feedback.presentation_epoch.store(self.presentation_epoch, Ordering::SeqCst);
            feedback.primed.store(self.primed, Ordering::SeqCst);
            feedback.version.store(next, Ordering::SeqCst);
        } else { feedback.retired.store(true, Ordering::SeqCst); }
        let underrun = !waiting && !lost && read.epoch != 0 && read.copied < wanted;
        finish_read(telemetry_token, read, false, underrun)
    }

    #[cfg(test)]
    pub(crate) fn id(&self) -> u64 {
        self.id
    }
}

fn signed_seconds(at: Instant, origin: Instant) -> f64 {
    if at >= origin {
        at.duration_since(origin).as_secs_f64()
    } else {
        -origin.duration_since(at).as_secs_f64()
    }
}

fn finish_read(
    token: crate::telemetry::OperationToken,
    read: PcmRead,
    lock_contention: bool,
    underrun: bool,
) -> PcmRead {
    // Reads never acquire the writer mutex; telemetry uses a bounded token handshake.
    crate::telemetry::record_pcm_bus_read(
        token,
        read.copied,
        read.silence,
        read.dropped,
        lock_contention,
        underrun,
    );
    read
}

fn session_from_wire(value: u64) -> Option<u64> {
    (value != 0).then_some(value)
}

impl Drop for PcmReader {
    fn drop(&mut self) {
        self.feedback.retired.store(true, Ordering::SeqCst);
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

    fn indexed_frames(count: usize) -> Vec<[f32; 2]> {
        (1..=count).map(|i| [i as f32, -(i as f32)]).collect()
    }

    #[test]
    fn timed_start_places_pcm_inside_a_block_and_preserves_continuity() {
        let bus = tiny_bus();
        let mut reader = bus.subscribe();
        bus.activate(7);
        let start = Instant::now();
        let mut out = [[0.0; 2]; 8];
        reader.read_stereo_timed(&mut out, 1, None);
        let source = indexed_frames(32);
        assert!(bus.push_timed_with_token(7, &source, crate::telemetry::operation_token(), Some(PcmWriteTiming { epoch: 1, presentation: start })));
        let read = reader.read_stereo_timed(&mut out, 1, Some(start - Duration::from_nanos(62_500)));
        assert_eq!((read.copied, read.silence, read.late), (5, 3, 0));
        assert_eq!(&out[..3], &[[0.0; 2]; 3]);
        assert_eq!(&out[3..], &source[..5]);
        // A missing native phase snapshot must not mute an established stream.
        let read = reader.read_stereo_timed(&mut out, 1, None);
        assert_eq!((read.copied, read.late), (8, 0));
        assert_eq!(&out, &source[5..13]);
        // Timing jitter does not force repeated sample skips after startup.
        reader.read_stereo_timed(&mut out, 1, Some(start + Duration::from_millis(10)));
        assert_eq!(&out, &source[13..21]);
    }

    #[test]
    fn timed_start_skips_late_samples_without_calling_them_overflow() {
        let bus = tiny_bus();
        let mut reader = bus.subscribe();
        bus.activate(7);
        let start = Instant::now();
        let mut out = [[0.0; 2]; 8];
        reader.read_stereo_timed(&mut out, 1, None);
        let source = indexed_frames(16);
        bus.push_timed_with_token(7, &source, crate::telemetry::operation_token(), Some(PcmWriteTiming { epoch: 1, presentation: start }));
        let read = reader.read_stereo_timed(&mut out, 1, Some(start + Duration::from_nanos(62_500)));
        assert_eq!((read.copied, read.late, read.dropped), (8, 3, 0));
        assert_eq!(&out, &source[3..11]);
    }

    #[test]
    fn timed_epoch_fences_old_pcm_and_old_writes_before_protocol_flush() {
        let bus = tiny_bus();
        let mut reader = bus.subscribe();
        bus.activate(7);
        let start = Instant::now();
        let mut out = [[0.0; 2]; 8];
        reader.read_stereo_timed(&mut out, 1, None);
        let old = PcmWriteTiming { epoch: 1, presentation: start };
        bus.push_timed_with_token(7, &indexed_frames(32), crate::telemetry::operation_token(), Some(old));
        assert_eq!(reader.read_stereo_timed(&mut out, 1, Some(start)).copied, 8);
        assert_eq!(reader.read_stereo_timed(&mut out, 2, Some(start)).copied, 0);
        assert_eq!(out, [[0.0; 2]; 8]);
        assert!(!bus.push_timed_with_token(7, &indexed_frames(32), crate::telemetry::operation_token(), Some(old)));
        bus.flush(7);
        let fresh = [[-0.5, 0.5]; 32];
        assert!(bus.push_timed_with_token(7, &fresh, crate::telemetry::operation_token(), Some(PcmWriteTiming { epoch: 2, presentation: start })));
        assert_eq!(reader.read_stereo_timed(&mut out, 2, Some(start)).copied, 8);
        assert_eq!(out, [[-0.5, 0.5]; 8]);
    }

    #[test]
    fn contended_timed_read_consumes_the_complete_output_slot() {
        let bus = tiny_bus();
        let mut reader = bus.subscribe();
        bus.activate(7);
        let start = Instant::now();
        let mut out = [[0.0; 2]; 8];
        reader.read_stereo_timed(&mut out, 1, None);
        let source = indexed_frames(32);
        assert!(bus.push_timed_with_token(7, &source, crate::telemetry::operation_token(), Some(PcmWriteTiming { epoch: 1, presentation: start })));
        assert_eq!(reader.read_stereo_timed(&mut out, 1, Some(start)).copied, 8);
        let locked = bus.inner.state.lock().unwrap();
        let missed = reader.read_stereo_timed(&mut out, 1, Some(start + Duration::from_secs_f64(8.0/48_000.0)));
        assert_eq!((missed.copied, missed.silence), (8, 0));
        assert_eq!(&out, &source[8..16]);
        drop(locked);
        let recovered = reader.read_stereo_timed(&mut out, 1, Some(start + Duration::from_secs_f64(16.0/48_000.0)));
        assert_eq!((recovered.copied, recovered.late), (8, 0));
        assert_eq!(&out, &source[16..24]);
        assert_eq!(reader.read_stereo_timed(&mut out, 1, None).copied, 8);
        assert_eq!(&out, &source[24..32]);
    }

    #[test]
    fn timed_contention_preserves_continuity_without_a_fresh_target() {
        let bus = tiny_bus();
        let mut reader = bus.subscribe();
        bus.activate(7);
        let start = Instant::now();
        let mut out = [[0.0; 2]; 8];
        reader.read_stereo_timed(&mut out, 1, None);
        let source = indexed_frames(40);
        bus.push_timed_with_token(7, &source, crate::telemetry::operation_token(), Some(PcmWriteTiming { epoch: 1, presentation: start }));
        reader.read_stereo_timed(&mut out, 1, Some(start));
        let locked = bus.inner.state.lock().unwrap();
        reader.read_stereo_timed(&mut out, 1, None);
        drop(locked);
        assert_eq!(&out, &source[8..16]);
        assert_eq!(reader.read_stereo_timed(&mut out, 1, None).copied, 8);
        assert_eq!(&out, &source[16..24]);
        let recovered = reader.read_stereo_timed(&mut out, 1, Some(start + Duration::from_secs_f64(24.0/48_000.0)));
        assert_eq!((recovered.copied, recovered.late), (8, 0));
        assert_eq!(&out, &source[24..32]);
    }

    #[test]
    fn partial_timed_start_counts_the_silence_already_presented() {
        let bus = tiny_bus();
        let mut reader = bus.subscribe();
        bus.activate(7);
        let start = Instant::now();
        let mut out = [[0.0; 2]; 8];
        reader.read_stereo_timed(&mut out, 1, None);
        let source = indexed_frames(24);
        let token = crate::telemetry::operation_token();
        assert!(bus.push_timed_with_token(7, &source[..4], token, Some(PcmWriteTiming { epoch: 1, presentation: start })));
        let first = reader.read_stereo_timed(&mut out, 1, Some(start));
        assert_eq!((first.copied, first.silence), (4, 4));
        assert_eq!(&out[..4], &source[..4]);
        assert_eq!(&out[4..], &[[0.0; 2]; 4]);
        assert!(bus.push_timed_with_token(7, &source[4..], token, Some(PcmWriteTiming { epoch: 1, presentation: start + Duration::from_secs_f64(4.0 / 48_000.0) })));
        let next = reader.read_stereo_timed(&mut out, 1, Some(start + Duration::from_secs_f64(8.0 / 48_000.0)));
        assert_eq!((next.copied, next.late), (8, 4));
        assert_eq!(&out, &source[8..16]);
    }

    #[test]
    fn timed_writes_cannot_advance_the_output_owners_epoch() {
        let bus = tiny_bus();
        let mut reader = bus.subscribe();
        bus.activate(7);
        let start = Instant::now();
        let mut out = [[0.0; 2]; 8];
        reader.read_stereo_timed(&mut out, 1, None);
        let first = PcmWriteTiming { epoch: 1, presentation: start };
        assert!(bus.push_timed_with_token(7, &indexed_frames(16), crate::telemetry::operation_token(), Some(first)));
        reader.read_stereo_timed(&mut out, 1, Some(start));
        bus.flush(7);
        let newer = [[-0.25, 0.25]; 16];
        let timing = PcmWriteTiming { epoch: 2, presentation: start };
        assert!(!bus.push_timed_with_token(7, &newer, crate::telemetry::operation_token(), Some(timing)));
        assert_eq!(reader.read_stereo_timed(&mut out, 2, Some(start)).copied, 0);
        bus.publish_output_timing(2, Duration::from_millis(20), true);
        assert_eq!(bus.output_timing().unwrap().epoch, 2);
        assert!(bus.push_timed_with_token(7, &newer, crate::telemetry::operation_token(), Some(timing)));
        assert_eq!(reader.read_stereo_timed(&mut out, 2, Some(start)).copied, 8);
        assert_eq!(out, [[-0.25, 0.25]; 8]);
    }

    #[test]
    fn timed_reader_keeps_untimed_protocols_live_without_an_epoch_ack() {
        let bus = tiny_bus();
        let mut reader = bus.subscribe();
        bus.activate(7);
        let source = indexed_frames(16);
        bus.push(7, &source);
        let mut out = [[0.0; 2]; 8];
        assert_eq!(reader.read_stereo_timed(&mut out, 7, None).copied, 8);
        assert_eq!(&out, &source[..8]);
        assert_eq!(reader.read_stereo_timed(&mut out, 8, None).copied, 8);
        assert_eq!(&out, &source[8..]);
    }

    #[test]
    fn timed_start_uses_available_pcm_at_its_deadline_not_a_count_threshold() {
        let bus = PcmBus::new(BusConfig::default());
        let mut reader = bus.subscribe();
        bus.activate(7);
        let start = Instant::now();
        let mut out = [[0.0; 2]; 480];
        reader.read_stereo_timed(&mut out, 1, None);
        let source = vec![[0.25, -0.25]; 1024];
        bus.push_timed_with_token(7, &source, crate::telemetry::operation_token(), Some(PcmWriteTiming { epoch: 1, presentation: start }));
        assert_eq!(reader.read_stereo_timed(&mut out, 1, Some(start - Duration::from_millis(20))).copied, 0);
        assert_eq!(reader.read_stereo_timed(&mut out, 1, None).copied, 0);
        assert_eq!(reader.read_stereo_timed(&mut out, 1, Some(start)).copied, 480);
        assert_eq!(out, [[0.25, -0.25]; 480]);
    }

    #[test]
    fn output_timing_is_bounded_fresh_and_includes_the_bus_cushion() {
        let bus = PcmBus::new(BusConfig::default());
        assert_eq!(bus.output_timing(), None);
        bus.publish_output_timing(7, Duration::from_millis(65), true);
        let timing = bus.output_timing().unwrap();
        assert_eq!(timing.epoch, 7);
        assert_eq!(timing.lead, Duration::from_millis(115));
        assert!(timing.estimated);
        bus.publish_output_timing(8, Duration::from_secs(2), false);
        assert_eq!(bus.output_timing(), None);
        bus.publish_output_timing(8, Duration::from_millis(20), false);
        bus.inner.timing_version.fetch_add(1, Ordering::SeqCst);
        assert_eq!(bus.output_timing(), None);
        bus.inner.timing_version.fetch_add(1, Ordering::SeqCst);
        assert_eq!(bus.output_timing().unwrap().epoch, 8);
        bus.publish_output_timing(0, Duration::ZERO, true);
        assert_eq!(bus.output_timing(), None);
    }

    #[test]
    fn readers_have_independent_cursors() {
        let bus = tiny_bus();
        let mut a = bus.subscribe();
        let mut b = bus.subscribe();
        bus.activate(7);
        assert!(bus.push(
            7,
            &[
                [1.0, -1.0],
                [2.0, -2.0],
                [3.0, -3.0],
                [4.0, -4.0],
            ]
        ));

        let mut two = [[0.0; 2]; 2];
        assert_eq!(a.read_stereo_into(&mut two).copied, 2);
        assert_eq!(two, [[1.0, -1.0], [2.0, -2.0]]);
        let mut four = [[0.0; 2]; 4];
        assert_eq!(b.read_stereo_into(&mut four).copied, 4);
        assert_eq!(
            four,
            [[1.0, -1.0], [2.0, -2.0], [3.0, -3.0], [4.0, -4.0]]
        );
        assert_eq!(a.read_stereo_into(&mut two).copied, 2);
        assert_eq!(two, [[3.0, -3.0], [4.0, -4.0]]);
    }

    #[test]
    fn telemetry_reset_preserves_pcm_and_reader_priming() {
        let bus = PcmBus::new(BusConfig {
            capacity_samples: 960,
            prebuffer_samples: 4,
            max_correction_ppm: 500,
        });
        let mut reader = bus.subscribe();
        bus.activate(17);
        let pre_reset_token = crate::telemetry::operation_token();
        assert!(bus.push(
            17,
            &[
                [1.0, -1.0],
                [2.0, -2.0],
                [3.0, -3.0],
                [4.0, -4.0],
                [5.0, -5.0],
                [6.0, -6.0],
            ]
        ));

        let mut first = [[0.0; 2]; 2];
        assert_eq!(reader.read_stereo_into(&mut first).copied, 2);
        assert_eq!(first, [[1.0, -1.0], [2.0, -2.0]]);

        crate::telemetry::reset();
        assert!(bus.push_with_token(
            17,
            &[[7.0, -7.0], [8.0, -8.0]],
            pre_reset_token
        ));

        let mut second = [[0.0; 2]; 6];
        let report = reader.read_stereo_into(&mut second);
        assert_eq!(report.copied, 6);
        assert_eq!(report.silence, 0);
        assert_eq!(report.session_id, Some(17));
        assert_eq!(
            second,
            [
                [3.0, -3.0],
                [4.0, -4.0],
                [5.0, -5.0],
                [6.0, -6.0],
                [7.0, -7.0],
                [8.0, -8.0],
            ]
        );
    }

    #[test]
    fn overflow_is_bounded_and_reported_per_reader() {
        let telemetry_before = crate::telemetry::snapshot().counters;
        let bus = tiny_bus();
        let mut reader = bus.subscribe();
        bus.activate(9);
        let frames: Vec<[f32; 2]> = (0..1_100)
            .map(|n| [n as f32, -(n as f32)])
            .collect();
        bus.push(9, &frames);
        let mut out = [[0.0; 2]; 2];
        let report = reader.read_stereo_into(&mut out);
        assert_eq!(report.dropped, 140);
        assert_eq!(out, [[140.0, -140.0], [141.0, -141.0]]);
        let telemetry_after = crate::telemetry::snapshot().counters;
        assert!(
            telemetry_after.pcm_bus_overflow_events
                >= telemetry_before.pcm_bus_overflow_events.saturating_add(1)
        );
        assert!(
            telemetry_after.pcm_bus_overflow_samples
                >= telemetry_before
                    .pcm_bus_overflow_samples
                    .saturating_add(140)
        );
        assert!(
            telemetry_after.pcm_bus_reader_dropped_samples
                >= telemetry_before
                    .pcm_bus_reader_dropped_samples
                    .saturating_add(140)
        );
    }

    #[test]
    fn writer_lock_does_not_count_as_contention_during_real_underrun() {
        let telemetry_before = crate::telemetry::snapshot().counters;
        let bus = tiny_bus();
        let mut reader = bus.subscribe();
        bus.activate(11);

        let state = bus.inner.state.lock().unwrap();
        let contention = reader.read_into(&mut [0.0; 8]);
        drop(state);
        assert_eq!(contention.copied, 0);
        assert_eq!(contention.silence, 8);

        assert!(bus.push(11, &[[0.25, -0.25], [0.5, -0.5]]));
        let underrun = reader.read_stereo_into(&mut [[0.0; 2]; 8]);
        assert_eq!(underrun.copied, 2);
        assert_eq!(underrun.silence, 6);

        let telemetry_after = crate::telemetry::snapshot().counters;
        assert_eq!(telemetry_after.pcm_bus_lock_contention_reads,
            telemetry_before.pcm_bus_lock_contention_reads);
        assert!(
            telemetry_after.pcm_bus_underrun_reads
                >= telemetry_before.pcm_bus_underrun_reads.saturating_add(1)
        );
        assert!(
            telemetry_after.pcm_bus_failures_total
                >= telemetry_before.pcm_bus_failures_total.saturating_add(2)
        );
    }

    #[test]
    fn new_session_invalidates_old_sink_and_resets_readers() {
        let mismatches_before = crate::telemetry::snapshot()
            .counters
            .pcm_bus_pushes_owner_mismatch;
        let bus = tiny_bus();
        let mut reader = bus.subscribe();
        bus.activate(1);
        bus.push(1, &[[0.5, -0.5], [0.6, -0.6]]);
        bus.activate(2);
        assert!(!bus.push(1, &[[0.9, -0.9]]));
        assert!(bus.push(2, &[[0.2, -0.2]]));
        let mut out = [[0.0; 2]; 1];
        let report = reader.read_stereo_into(&mut out);
        assert_eq!(report.session_id, Some(2));
        assert_eq!(out, [[0.2, -0.2]]);
        let telemetry = crate::telemetry::snapshot();
        assert!(
            telemetry.counters.pcm_bus_pushes_owner_mismatch >= mismatches_before.saturating_add(1)
        );
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
        bus.push(1, &vec![[0.0; 2]; 960]);
        realtime.read_stereo_into(&mut [[0.0; 2]; 480]);
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
        bus.push(1, &[[0.25, -0.25], [0.5, -0.5]]);

        let guard = bus.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut out = [[1.0; 2]; 2];
        let report = reader.read_stereo_into(&mut out);
        assert_eq!(report.copied, 2);
        assert_eq!(report.silence, 0);
        assert_eq!(report.session_id, Some(1), "contention is not a disconnect");
        assert_eq!(out, [[0.25, -0.25], [0.5, -0.5]]);
        drop(guard);

        assert_eq!(reader.read_stereo_into(&mut out).copied, 0);
        assert_eq!(out, [[0.0; 2]; 2]);
    }

    #[test]
    fn prebuffer_and_underrun_use_whole_stereo_frames() {
        let bus = PcmBus::new(BusConfig {
            capacity_samples: 960,
            prebuffer_samples: 4,
            max_correction_ppm: 500,
        });
        let mut reader = bus.subscribe();
        bus.activate(1);
        bus.push(1, &[[1.0, 10.0], [2.0, 20.0], [3.0, 30.0]]);

        let mut out = [[9.0; 2]; 2];
        let waiting = reader.read_stereo_into(&mut out);
        assert_eq!(waiting.copied, 0);
        assert_eq!(waiting.silence, 2);
        assert_eq!(out, [[0.0; 2]; 2]);

        bus.push(1, &[[4.0, 40.0]]);
        let mut out = [[0.0; 2]; 6];
        let underrun = reader.read_stereo_into(&mut out);
        assert_eq!(underrun.copied, 4);
        assert_eq!(underrun.silence, 2);
        assert_eq!(
            out,
            [
                [1.0, 10.0],
                [2.0, 20.0],
                [3.0, 30.0],
                [4.0, 40.0],
                [0.0, 0.0],
                [0.0, 0.0],
            ]
        );
    }

    #[test]
    fn legacy_mono_reader_explicitly_downmixes_stereo_frames() {
        let bus = tiny_bus();
        let mut reader = bus.subscribe();
        bus.activate(1);
        bus.push(1, &[[0.75, -0.25], [-0.5, 0.25]]);

        let mut out = [0.0; 2];
        let read = reader.read_into(&mut out);
        assert_eq!(read.copied, 2);
        assert_eq!(out, [0.25, -0.125]);
    }
}
