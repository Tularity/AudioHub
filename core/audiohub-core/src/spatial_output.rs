//! Fixed-layout PCM delivery to a provider's native spatial renderer.
//! This does not decode an Atmos bitstream or emulate a Dolby license.

use anyhow::{bail, Result};
use ringbuf::traits::{Observer, Producer};
use ringbuf::HeapProd;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

pub const SPATIAL_SAMPLE_RATE: u32 = 48_000;
pub const SPATIAL_QUEUE_FRAMES: usize = 4_800;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpeakerLayout {
    #[serde(rename = "surround_5_1")]
    Surround51,
    #[serde(rename = "surround_7_1")]
    Surround71,
    #[serde(rename = "immersive_7_1_4")]
    Immersive714,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpeakerPosition {
    FrontLeft,
    FrontRight,
    FrontCenter,
    LowFrequency,
    BackLeft,
    BackRight,
    SideLeft,
    SideRight,
    TopFrontLeft,
    TopFrontRight,
    TopBackLeft,
    TopBackRight,
}

impl SpeakerLayout {
    /// Canonical wire order follows ascending Windows speaker-mask bits.
    /// Platforms with a different native order must map explicit positions.
    pub const fn positions(self) -> &'static [SpeakerPosition] {
        use SpeakerPosition::*;
        match self {
            Self::Surround51 => &[
                FrontLeft,
                FrontRight,
                FrontCenter,
                LowFrequency,
                SideLeft,
                SideRight,
            ],
            Self::Surround71 => &[
                FrontLeft,
                FrontRight,
                FrontCenter,
                LowFrequency,
                BackLeft,
                BackRight,
                SideLeft,
                SideRight,
            ],
            Self::Immersive714 => &[
                FrontLeft,
                FrontRight,
                FrontCenter,
                LowFrequency,
                BackLeft,
                BackRight,
                SideLeft,
                SideRight,
                TopFrontLeft,
                TopFrontRight,
                TopBackLeft,
                TopBackRight,
            ],
        }
    }

    pub const fn channels(self) -> usize {
        self.positions().len()
    }
}

#[derive(Debug, Clone)]
pub struct SpatialOutputConfig {
    pub endpoint_id: String,
    pub active_format: String,
    pub layout: SpeakerLayout,
}

pub(crate) struct SpatialOutputState {
    pub stop: AtomicBool,
    pub closed: AtomicBool,
    pub failed: AtomicBool,
    pub failure: Mutex<Option<String>>,
    pub rendered_frames: AtomicU64,
    pub consumed_frames: AtomicU64,
    pub underrun_frames: AtomicU64,
}

impl SpatialOutputState {
    pub(crate) fn new() -> Self {
        Self {
            stop: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            failure: Mutex::new(None),
            rendered_frames: AtomicU64::new(0),
            consumed_frames: AtomicU64::new(0),
            underrun_frames: AtomicU64::new(0),
        }
    }

    pub(crate) fn fail(&self, message: String) {
        *self.failure.lock().unwrap_or_else(|e| e.into_inner()) = Some(message);
        self.failed.store(true, Ordering::Release);
    }
}

pub struct SpatialOutput {
    pub(crate) producer: HeapProd<f32>,
    pub(crate) state: Arc<SpatialOutputState>,
    pub(crate) layout: SpeakerLayout,
}

impl SpatialOutput {
    /// Revalidates the exact default endpoint and active native format before
    /// starting. The caller must drop this stream if its provider contract is
    /// invalidated; it must not reuse it for a different output or layout.
    pub fn start(config: SpatialOutputConfig) -> Result<Self> {
        crate::output_capabilities::start_spatial_output(config)
    }

    /// Returns whole interleaved frames accepted, never a partial channel frame.
    /// No gain/downmix is applied; backpressure stays observable to the caller.
    pub fn write_interleaved(&mut self, samples: &[f32]) -> Result<usize> {
        // Observing worker closure must also observe any preceding failure
        // publication before choosing the more general closed-state error.
        let closed =
            self.state.stop.load(Ordering::Acquire) || self.state.closed.load(Ordering::Acquire);
        if let Some(message) = self.failure() {
            bail!("native spatial output failed: {message}");
        }
        if closed {
            bail!("native spatial output is closed");
        }
        let channels = self.layout.channels();
        if samples.len() % channels != 0 || samples.iter().any(|sample| !sample.is_finite()) {
            bail!("spatial PCM must contain complete finite interleaved frames");
        }
        let writable = self.producer.vacant_len() / channels * channels;
        let count = samples.len().min(writable);
        let written = self.producer.push_slice(&samples[..count]);
        debug_assert_eq!(written % channels, 0);
        Ok(written / channels)
    }

    pub fn queued_frames(&self) -> usize {
        self.producer.occupied_len() / self.layout.channels()
    }

    pub fn rendered_frames(&self) -> u64 {
        self.state.rendered_frames.load(Ordering::Relaxed)
    }

    pub fn underrun_frames(&self) -> u64 {
        self.state.underrun_frames.load(Ordering::Relaxed)
    }

    pub fn consumed_frames(&self) -> u64 {
        self.state.consumed_frames.load(Ordering::Relaxed)
    }

    /// Stop without blocking a realtime caller; use `shutdown` off that thread
    /// when the caller needs confirmation of native resource cleanup.
    pub fn request_stop(&self) {
        self.state.stop.store(true, Ordering::Release);
    }

    pub fn shutdown(self) -> Result<()> {
        let state = Arc::clone(&self.state);
        drop(self);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while !state.closed.load(Ordering::Acquire) {
            if std::time::Instant::now() >= deadline {
                bail!("native spatial output is still closing");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        if state.failed.load(Ordering::Acquire) {
            let message = state
                .failure
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            bail!(
                "native spatial output failed before closing: {}",
                message.unwrap_or_default()
            );
        }
        Ok(())
    }

    pub fn failure(&self) -> Option<String> {
        if !self.state.failed.load(Ordering::Acquire) {
            return None;
        }
        self.state
            .failure
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl Drop for SpatialOutput {
    fn drop(&mut self) {
        self.state.stop.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ringbuf::traits::{Consumer, Split};
    use ringbuf::HeapRb;

    #[test]
    fn layouts_keep_positions_unique_and_ordered() {
        for (layout, channels) in [
            (SpeakerLayout::Surround51, 6),
            (SpeakerLayout::Surround71, 8),
            (SpeakerLayout::Immersive714, 12),
        ] {
            assert_eq!(layout.channels(), channels);
            for (index, position) in layout.positions().iter().enumerate() {
                assert!(!layout.positions()[..index].contains(position));
            }
        }
        assert_eq!(
            SpeakerLayout::Surround71.positions()[4],
            SpeakerPosition::BackLeft
        );
        assert_eq!(
            SpeakerLayout::Surround51.positions()[4],
            SpeakerPosition::SideLeft
        );
    }

    #[test]
    fn asynchronous_failure_survives_native_thread_closure() {
        let layout = SpeakerLayout::Surround51;
        let (producer, _consumer) = HeapRb::new(12).split();
        let state = Arc::new(SpatialOutputState::new());
        state.fail("native endpoint invalidated: 0x88890004".into());
        state.closed.store(true, Ordering::Release);
        let mut output = SpatialOutput {
            producer,
            layout,
            state,
        };
        let error = output.write_interleaved(&[0.0; 6]).unwrap_err().to_string();
        assert!(error.contains("0x88890004"));
        assert!(output
            .shutdown()
            .unwrap_err()
            .to_string()
            .contains("0x88890004"));
    }

    #[test]
    fn spatial_queue_preserves_whole_frames_without_downmix() {
        let layout = SpeakerLayout::Immersive714;
        let (producer, mut consumer) = HeapRb::new(layout.channels() * 2).split();
        let mut output = SpatialOutput {
            producer,
            layout,
            state: Arc::new(SpatialOutputState::new()),
        };
        let values: Vec<f32> = (0..36).map(|n| n as f32 / 36.0).collect();
        assert_eq!(output.write_interleaved(&values).unwrap(), 2);
        assert_eq!(output.queued_frames(), 2);
        let mut frame = [0.0; 12];
        assert_eq!(consumer.pop_slice(&mut frame), 12);
        assert_eq!(&frame[..], &values[..12]);
        assert_eq!(output.write_interleaved(&values[24..]).unwrap(), 1);
        assert!(output.write_interleaved(&[0.0; 11]).is_err());
        assert!(output.write_interleaved(&[f32::NAN; 12]).is_err());
    }
}
