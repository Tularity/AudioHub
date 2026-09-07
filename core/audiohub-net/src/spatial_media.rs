//! Fragmentation and bounded reassembly for negotiated fixed-layout spatial PCM.

use anyhow::{bail, Result};
use audiohub_core::spatial_output::{SpatialOutputConfig, SpeakerLayout, SPATIAL_SAMPLE_RATE};
use serde::{Deserialize, Serialize};

use crate::media::SINGLE_PACKET_PAYLOAD_MAX;

pub const VERSION: u8 = 1;
pub const FRAME_FRAMES: usize = 480;
pub const FRAGMENT_HEADER_LEN: usize = 12;

const SAMPLE_BYTES: usize = std::mem::size_of::<f32>();

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpatialMediaContract {
    pub version: u8,
    pub provider_revision: u64,
    pub endpoint_id: String,
    pub active_format: String,
    pub layout: SpeakerLayout,
}

impl SpatialMediaContract {
    pub fn validate(&self) -> Result<()> {
        if self.version != VERSION {
            bail!("unsupported spatial media version {}", self.version);
        }
        if SPATIAL_SAMPLE_RATE != 48_000 {
            bail!("spatial media requires a 48000 Hz core output");
        }
        if self.provider_revision == 0 {
            bail!("spatial media provider revision must be nonzero");
        }
        validate_text("endpoint_id", &self.endpoint_id, 1024)?;
        validate_text("active_format", &self.active_format, 128)?;
        Ok(())
    }

    pub fn channels(&self) -> u8 {
        self.layout.channels() as u8
    }

    pub fn output_config(&self) -> SpatialOutputConfig {
        SpatialOutputConfig {
            endpoint_id: self.endpoint_id.clone(),
            active_format: self.active_format.clone(),
            layout: self.layout,
        }
    }
}

fn validate_text(name: &str, value: &str, max_bytes: usize) -> Result<()> {
    if value.is_empty() {
        bail!("{name} must not be empty");
    }
    if value.len() > max_bytes {
        bail!("{name} exceeds {max_bytes} bytes");
    }
    if value.chars().any(char::is_control) {
        bail!("{name} contains a control character");
    }
    Ok(())
}

fn frames_per_part(layout: SpeakerLayout) -> usize {
    (SINGLE_PACKET_PAYLOAD_MAX - FRAGMENT_HEADER_LEN) / (layout.channels() * SAMPLE_BYTES)
}

pub fn fragment_count(layout: SpeakerLayout) -> usize {
    let frames = frames_per_part(layout);
    (FRAME_FRAMES + frames - 1) / frames
}

pub fn encode_fragment(
    layout: SpeakerLayout,
    frame_seq: u64,
    part: usize,
    samples: &[f32],
    out: &mut Vec<u8>,
) -> Result<()> {
    let channels = layout.channels();
    let expected_samples = FRAME_FRAMES * channels;
    if samples.len() != expected_samples {
        bail!(
            "spatial frame has {} samples, expected {expected_samples}",
            samples.len()
        );
    }
    if samples.iter().any(|sample| !sample.is_finite()) {
        bail!("spatial frame contains a non-finite sample");
    }

    let part_count = fragment_count(layout);
    if part >= part_count {
        bail!("spatial fragment index {part} is outside 0..{part_count}");
    }

    let part_frames = frames_per_part(layout);
    let start_sample = part * part_frames * channels;
    let end_sample = ((part + 1) * part_frames * channels).min(expected_samples);
    let encoded_len = FRAGMENT_HEADER_LEN + (end_sample - start_sample) * SAMPLE_BYTES;

    out.clear();
    out.reserve(encoded_len);
    out.push(VERSION);
    out.push(part as u8);
    out.push(part_count as u8);
    out.push(0);
    out.extend_from_slice(&frame_seq.to_le_bytes());
    for sample in &samples[start_sample..end_sample] {
        out.extend_from_slice(&sample.to_le_bytes());
    }
    Ok(())
}

pub struct ReassembledFrame {
    pub frame_seq: u64,
    pub samples: Vec<f32>,
    pub concealed_fragments: usize,
    /// Missing sample frames, including the shorter final fragment exactly.
    pub concealed_frames: usize,
}

impl ReassembledFrame {
    pub fn requires_whole_frame_concealment(&self) -> bool {
        self.concealed_frames > FRAME_FRAMES / 2
    }
}

pub struct SpatialReassembler {
    layout: SpeakerLayout,
    channels: usize,
    part_count: usize,
    cursor: SequenceCursor,
    pending: Vec<PendingFrame>,
}

impl SpatialReassembler {
    pub fn new(layout: SpeakerLayout) -> Self {
        Self {
            layout,
            channels: layout.channels(),
            part_count: fragment_count(layout),
            cursor: SequenceCursor::Uninitialized,
            pending: Vec::with_capacity(2),
        }
    }

    pub fn push(&mut self, plaintext: &[u8]) -> Result<Vec<ReassembledFrame>> {
        let fragment = parse_fragment(self.layout, plaintext)?;
        let mut emitted = Vec::with_capacity(2);

        match self.cursor {
            SequenceCursor::Uninitialized => {
                self.cursor = SequenceCursor::Next(fragment.frame_seq);
            }
            SequenceCursor::Exhausted => return Ok(emitted),
            SequenceCursor::Next(next) => {
                if fragment.frame_seq < next {
                    return Ok(emitted);
                }

                let distance = fragment.frame_seq - next;
                if distance == 2 {
                    if let Some(frame) = self.retire_sequence(next) {
                        emitted.push(frame);
                    }
                    self.cursor = SequenceCursor::Next(next + 1);
                    self.drain_ready(&mut emitted);
                } else if distance > 2 {
                    self.retire_all_pending(&mut emitted);
                    self.cursor = SequenceCursor::Next(fragment.frame_seq);
                }
            }
        }

        let frame_index = match self
            .pending
            .binary_search_by_key(&fragment.frame_seq, |frame| frame.frame_seq)
        {
            Ok(index) => index,
            Err(index) => {
                self.pending.insert(
                    index,
                    PendingFrame::new(fragment.frame_seq, self.channels, self.part_count),
                );
                index
            }
        };

        self.pending[frame_index].accept(
            fragment.part,
            &fragment.samples,
            frames_per_part(self.layout),
            self.channels,
        );
        self.drain_ready(&mut emitted);
        Ok(emitted)
    }

    pub fn flush_expired(&mut self) -> Vec<ReassembledFrame> {
        if self.pending.is_empty() {
            return Vec::new();
        }

        let pending = std::mem::take(&mut self.pending);
        let last_seq = pending.last().map(|frame| frame.frame_seq);
        let emitted = pending.into_iter().map(PendingFrame::finish).collect();
        if let Some(last_seq) = last_seq {
            self.cursor = match last_seq.checked_add(1) {
                Some(next) => SequenceCursor::Next(next),
                None => SequenceCursor::Exhausted,
            };
        }
        emitted
    }

    fn retire_sequence(&mut self, frame_seq: u64) -> Option<ReassembledFrame> {
        let index = self
            .pending
            .binary_search_by_key(&frame_seq, |frame| frame.frame_seq)
            .ok()?;
        Some(self.pending.remove(index).finish())
    }

    fn retire_all_pending(&mut self, emitted: &mut Vec<ReassembledFrame>) {
        emitted.extend(
            std::mem::take(&mut self.pending)
                .into_iter()
                .map(PendingFrame::finish),
        );
    }

    fn drain_ready(&mut self, emitted: &mut Vec<ReassembledFrame>) {
        loop {
            let next = match self.cursor {
                SequenceCursor::Next(next) => next,
                SequenceCursor::Uninitialized | SequenceCursor::Exhausted => return,
            };
            let Some(frame) = self.pending.first() else {
                return;
            };
            if frame.frame_seq != next || !frame.is_complete() {
                return;
            }

            emitted.push(self.pending.remove(0).finish());
            self.cursor = match next.checked_add(1) {
                Some(next) => SequenceCursor::Next(next),
                None => SequenceCursor::Exhausted,
            };
        }
    }
}

#[derive(Clone, Copy)]
enum SequenceCursor {
    Uninitialized,
    Next(u64),
    Exhausted,
}

struct PendingFrame {
    frame_seq: u64,
    samples: Vec<f32>,
    received: Vec<bool>,
    received_count: usize,
    received_frames: usize,
}

impl PendingFrame {
    fn new(frame_seq: u64, channels: usize, part_count: usize) -> Self {
        Self {
            frame_seq,
            samples: vec![0.0; FRAME_FRAMES * channels],
            received: vec![false; part_count],
            received_count: 0,
            received_frames: 0,
        }
    }

    fn accept(&mut self, part: usize, samples: &[f32], part_frames: usize, channels: usize) {
        if self.received[part] {
            return;
        }

        let start = part * part_frames * channels;
        self.samples[start..start + samples.len()].copy_from_slice(samples);
        self.received[part] = true;
        self.received_count += 1;
        self.received_frames += samples.len() / channels;
    }

    fn is_complete(&self) -> bool {
        self.received_count == self.received.len()
    }

    fn finish(self) -> ReassembledFrame {
        ReassembledFrame {
            frame_seq: self.frame_seq,
            samples: self.samples,
            concealed_fragments: self.received.len() - self.received_count,
            concealed_frames: FRAME_FRAMES - self.received_frames,
        }
    }
}

struct ParsedFragment {
    frame_seq: u64,
    part: usize,
    samples: Vec<f32>,
}

fn parse_fragment(layout: SpeakerLayout, plaintext: &[u8]) -> Result<ParsedFragment> {
    if plaintext.len() < FRAGMENT_HEADER_LEN {
        bail!("spatial fragment is shorter than its header");
    }
    if plaintext.len() > SINGLE_PACKET_PAYLOAD_MAX {
        bail!("spatial fragment exceeds the plaintext payload limit");
    }
    if plaintext[0] != VERSION {
        bail!("unsupported spatial fragment version {}", plaintext[0]);
    }
    if plaintext[3] != 0 {
        bail!("spatial fragment reserved byte must be zero");
    }

    let part = plaintext[1] as usize;
    let encoded_part_count = plaintext[2] as usize;
    let expected_part_count = fragment_count(layout);
    if encoded_part_count != expected_part_count {
        bail!(
            "spatial fragment count {encoded_part_count} does not match expected {expected_part_count}"
        );
    }
    if part >= encoded_part_count {
        bail!("spatial fragment index {part} is outside 0..{encoded_part_count}");
    }

    let channels = layout.channels();
    let frame_bytes = channels * SAMPLE_BYTES;
    let payload = &plaintext[FRAGMENT_HEADER_LEN..];
    if payload.len() % frame_bytes != 0 {
        bail!("spatial fragment payload is not aligned to whole sample frames");
    }

    let part_frames = frames_per_part(layout);
    let start_frame = part * part_frames;
    let expected_frames = part_frames.min(FRAME_FRAMES - start_frame);
    let expected_payload_len = expected_frames * frame_bytes;
    if payload.len() != expected_payload_len {
        bail!(
            "spatial fragment payload has {} bytes, expected {expected_payload_len}",
            payload.len()
        );
    }

    let frame_seq = u64::from_le_bytes(
        plaintext[4..FRAGMENT_HEADER_LEN]
            .try_into()
            .expect("spatial fragment sequence has a fixed width"),
    );
    let mut samples = Vec::with_capacity(expected_frames * channels);
    for bytes in payload.chunks_exact(SAMPLE_BYTES) {
        let sample = f32::from_le_bytes(
            bytes
                .try_into()
                .expect("spatial PCM sample has a fixed width"),
        );
        if !sample.is_finite() {
            bail!("spatial fragment contains a non-finite sample");
        }
        samples.push(sample);
    }

    Ok(ParsedFragment {
        frame_seq,
        part,
        samples,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::MediaCrypto;
    use crate::packet::{Codec, Header, Kind, HEADER_LEN};

    const LAYOUTS: [SpeakerLayout; 3] = [
        SpeakerLayout::Surround51,
        SpeakerLayout::Surround71,
        SpeakerLayout::Immersive714,
    ];

    fn frame_samples(layout: SpeakerLayout, seed: usize) -> Vec<f32> {
        (0..FRAME_FRAMES)
            .flat_map(|frame| {
                (0..layout.channels())
                    .map(move |channel| (seed * 10_000 + frame * 16 + channel) as f32)
            })
            .collect()
    }

    fn fragments(layout: SpeakerLayout, frame_seq: u64, samples: &[f32]) -> Vec<Vec<u8>> {
        (0..fragment_count(layout))
            .map(|part| {
                let mut encoded = Vec::new();
                encode_fragment(layout, frame_seq, part, samples, &mut encoded).unwrap();
                encoded
            })
            .collect()
    }

    fn assert_frame(frame: &ReassembledFrame, frame_seq: u64, samples: &[f32]) {
        assert_eq!(frame.frame_seq, frame_seq);
        assert_eq!(frame.samples, samples);
        assert_eq!(frame.concealed_fragments, 0);
        assert_eq!(frame.concealed_frames, 0);
    }

    fn pending_snapshot(
        reassembler: &SpatialReassembler,
    ) -> Vec<(u64, Vec<f32>, Vec<bool>, usize)> {
        reassembler
            .pending
            .iter()
            .map(|frame| {
                (
                    frame.frame_seq,
                    frame.samples.clone(),
                    frame.received.clone(),
                    frame.received_count,
                )
            })
            .collect()
    }

    #[test]
    fn all_layouts_roundtrip_with_bounded_fragment_geometry() {
        let expected_counts = [10, 13, 20];

        for (layout, expected_count) in LAYOUTS.into_iter().zip(expected_counts) {
            let samples = frame_samples(layout, expected_count);
            let encoded = fragments(layout, 0x0102_0304_0506_0708, &samples);
            assert_eq!(fragment_count(layout), expected_count);
            assert_eq!(encoded.len(), expected_count);

            for (part, fragment) in encoded.iter().enumerate() {
                let start_frame = part * frames_per_part(layout);
                let encoded_frames = frames_per_part(layout).min(FRAME_FRAMES - start_frame);
                let expected_len =
                    FRAGMENT_HEADER_LEN + encoded_frames * layout.channels() * SAMPLE_BYTES;
                assert_eq!(fragment.len(), expected_len);
                assert!(fragment.len() <= SINGLE_PACKET_PAYLOAD_MAX);
                assert_eq!(fragment[0], VERSION);
                assert_eq!(fragment[1] as usize, part);
                assert_eq!(fragment[2] as usize, expected_count);
                assert_eq!(fragment[3], 0);
                assert_eq!(
                    u64::from_le_bytes(fragment[4..FRAGMENT_HEADER_LEN].try_into().unwrap()),
                    0x0102_0304_0506_0708
                );
                assert_eq!(
                    (fragment.len() - FRAGMENT_HEADER_LEN) % (layout.channels() * SAMPLE_BYTES),
                    0
                );
            }

            if layout == SpeakerLayout::Immersive714 {
                assert_eq!(encoded.last().unwrap().len(), encoded[0].len());
            } else {
                assert!(encoded.last().unwrap().len() < encoded[0].len());
            }

            let mut reassembler = SpatialReassembler::new(layout);
            let mut emitted = Vec::new();
            for fragment in encoded {
                emitted.extend(reassembler.push(&fragment).unwrap());
            }
            assert_eq!(emitted.len(), 1);
            assert_frame(&emitted[0], 0x0102_0304_0506_0708, &samples);
        }
    }

    #[test]
    fn fragments_reassemble_when_received_in_reverse_order() {
        let layout = SpeakerLayout::Surround71;
        let samples = frame_samples(layout, 1);
        let mut reassembler = SpatialReassembler::new(layout);
        let mut emitted = Vec::new();

        for fragment in fragments(layout, 31, &samples).into_iter().rev() {
            emitted.extend(reassembler.push(&fragment).unwrap());
        }

        assert_eq!(emitted.len(), 1);
        assert_frame(&emitted[0], 31, &samples);
    }

    #[test]
    fn a_complete_next_frame_waits_for_the_current_frame_tail() {
        let layout = SpeakerLayout::Surround51;
        let current_samples = frame_samples(layout, 2);
        let next_samples = frame_samples(layout, 3);
        let current = fragments(layout, 100, &current_samples);
        let next = fragments(layout, 101, &next_samples);
        let mut reassembler = SpatialReassembler::new(layout);

        assert!(reassembler.push(&current[0]).unwrap().is_empty());
        for fragment in &next {
            assert!(reassembler.push(fragment).unwrap().is_empty());
        }

        let mut emitted = Vec::new();
        for fragment in &current[1..] {
            emitted.extend(reassembler.push(fragment).unwrap());
        }
        assert_eq!(emitted.len(), 2);
        assert_frame(&emitted[0], 100, &current_samples);
        assert_frame(&emitted[1], 101, &next_samples);
    }

    #[test]
    fn first_seen_sequence_sets_the_forward_playback_floor() {
        let layout = SpeakerLayout::Surround51;
        let late_samples = frame_samples(layout, 15);
        let first_samples = frame_samples(layout, 16);
        let late = fragments(layout, 500, &late_samples);
        let first = fragments(layout, 501, &first_samples);
        let mut reassembler = SpatialReassembler::new(layout);

        assert!(reassembler.push(&first[0]).unwrap().is_empty());
        for fragment in &late {
            assert!(reassembler.push(fragment).unwrap().is_empty());
        }

        let mut emitted = Vec::new();
        for fragment in &first[1..] {
            emitted.extend(reassembler.push(fragment).unwrap());
        }
        assert_eq!(emitted.len(), 1);
        assert_frame(&emitted[0], 501, &first_samples);
    }

    #[test]
    fn expiry_conceals_only_the_missing_part_and_first_valid_duplicate_wins() {
        let layout = SpeakerLayout::Surround71;
        let current_samples = frame_samples(layout, 4);
        let next_samples = frame_samples(layout, 5);
        let future_samples = frame_samples(layout, 6);
        let current = fragments(layout, 200, &current_samples);
        let next = fragments(layout, 201, &next_samples);
        let future = fragments(layout, 202, &future_samples);
        let missing_part = 3;
        let mut reassembler = SpatialReassembler::new(layout);

        assert!(reassembler.push(&current[0]).unwrap().is_empty());
        let mut conflicting = current[0].clone();
        conflicting[FRAGMENT_HEADER_LEN..FRAGMENT_HEADER_LEN + SAMPLE_BYTES]
            .copy_from_slice(&(-1234.0f32).to_le_bytes());
        assert!(reassembler.push(&conflicting).unwrap().is_empty());

        for (part, fragment) in current.iter().enumerate().skip(1) {
            if part != missing_part {
                assert!(reassembler.push(fragment).unwrap().is_empty());
            }
        }
        for fragment in &next {
            assert!(reassembler.push(fragment).unwrap().is_empty());
        }

        let emitted = reassembler.push(&future[0]).unwrap();
        assert_eq!(emitted.len(), 2);
        assert_eq!(emitted[0].frame_seq, 200);
        assert_eq!(emitted[0].concealed_fragments, 1);
        let mut expected_current = current_samples.clone();
        let start = missing_part * frames_per_part(layout) * layout.channels();
        let end = ((missing_part + 1) * frames_per_part(layout) * layout.channels())
            .min(expected_current.len());
        expected_current[start..end].fill(0.0);
        assert_eq!(emitted[0].samples, expected_current);
        assert_frame(&emitted[1], 201, &next_samples);

        assert!(reassembler.push(&current[missing_part]).unwrap().is_empty());
        assert_eq!(reassembler.pending[0].received_count, 1);
        let mut future_conflict = future[0].clone();
        future_conflict[FRAGMENT_HEADER_LEN..FRAGMENT_HEADER_LEN + SAMPLE_BYTES]
            .copy_from_slice(&(-4321.0f32).to_le_bytes());
        assert!(reassembler.push(&future_conflict).unwrap().is_empty());

        let mut future_emitted = Vec::new();
        for fragment in &future[1..] {
            future_emitted.extend(reassembler.push(fragment).unwrap());
        }
        assert_eq!(future_emitted.len(), 1);
        assert_frame(&future_emitted[0], 202, &future_samples);
    }

    #[test]
    fn malformed_fragments_do_not_mutate_pending_state() {
        let layout = SpeakerLayout::Surround51;
        let samples = frame_samples(layout, 7);
        let encoded = fragments(layout, 77, &samples);
        let mut reassembler = SpatialReassembler::new(layout);
        reassembler.push(&encoded[0]).unwrap();
        let before = pending_snapshot(&reassembler);

        let mut malformed = Vec::new();
        malformed.push(vec![0; FRAGMENT_HEADER_LEN - 1]);

        let mut bad_version = encoded[1].clone();
        bad_version[0] = VERSION + 1;
        malformed.push(bad_version);

        let mut bad_reserved = encoded[1].clone();
        bad_reserved[3] = 1;
        malformed.push(bad_reserved);

        let mut bad_count = encoded[1].clone();
        bad_count[2] -= 1;
        malformed.push(bad_count);

        let mut bad_index = encoded[1].clone();
        bad_index[1] = bad_index[2];
        malformed.push(bad_index);

        let mut bad_length = encoded[1].clone();
        bad_length.truncate(bad_length.len() - layout.channels() * SAMPLE_BYTES);
        malformed.push(bad_length);

        let mut nonfinite = encoded[1].clone();
        nonfinite[FRAGMENT_HEADER_LEN..FRAGMENT_HEADER_LEN + SAMPLE_BYTES]
            .copy_from_slice(&f32::NAN.to_le_bytes());
        malformed.push(nonfinite);

        let mut oversized = encoded[1].clone();
        oversized.resize(SINGLE_PACKET_PAYLOAD_MAX + 1, 0);
        malformed.push(oversized);

        for fragment in malformed {
            assert!(reassembler.push(&fragment).is_err());
            assert_eq!(pending_snapshot(&reassembler), before);
        }
    }

    #[test]
    fn huge_sequence_jump_and_u64_max_remain_bounded() {
        let layout = SpeakerLayout::Immersive714;
        let old_samples = frame_samples(layout, 8);
        let max_samples = frame_samples(layout, 9);
        let old = fragments(layout, 5, &old_samples);
        let max = fragments(layout, u64::MAX, &max_samples);
        let mut reassembler = SpatialReassembler::new(layout);

        assert!(reassembler.push(&old[0]).unwrap().is_empty());
        let retired = reassembler.push(&max[0]).unwrap();
        assert_eq!(retired.len(), 1);
        assert_eq!(retired[0].frame_seq, 5);
        assert_eq!(retired[0].concealed_fragments, fragment_count(layout) - 1);
        assert_eq!(reassembler.pending.len(), 1);

        let mut emitted = Vec::new();
        for fragment in &max[1..] {
            emitted.extend(reassembler.push(fragment).unwrap());
            assert!(reassembler.pending.len() <= 1);
        }
        assert_eq!(emitted.len(), 1);
        assert_frame(&emitted[0], u64::MAX, &max_samples);
        assert!(reassembler.pending.is_empty());
        assert!(reassembler.push(&old[1]).unwrap().is_empty());
        assert!(reassembler.push(&max[0]).unwrap().is_empty());
    }

    #[test]
    fn flush_retires_pending_frames_in_order_and_ignores_late_parts() {
        let layout = SpeakerLayout::Surround51;
        let first_samples = frame_samples(layout, 10);
        let second_samples = frame_samples(layout, 11);
        let third_samples = frame_samples(layout, 12);
        let first = fragments(layout, 40, &first_samples);
        let second = fragments(layout, 41, &second_samples);
        let third = fragments(layout, 42, &third_samples);
        let mut reassembler = SpatialReassembler::new(layout);

        reassembler.push(&first[0]).unwrap();
        reassembler.push(&second[1]).unwrap();
        let flushed = reassembler.flush_expired();
        assert_eq!(flushed.len(), 2);
        assert_eq!(flushed[0].frame_seq, 40);
        assert_eq!(flushed[1].frame_seq, 41);
        assert_eq!(flushed[0].concealed_fragments, fragment_count(layout) - 1);
        assert_eq!(flushed[1].concealed_fragments, fragment_count(layout) - 1);

        assert!(reassembler.push(&first[1]).unwrap().is_empty());
        assert!(reassembler.push(&second[0]).unwrap().is_empty());
        let mut emitted = Vec::new();
        for fragment in &third {
            emitted.extend(reassembler.push(fragment).unwrap());
        }
        assert_eq!(emitted.len(), 1);
        assert_frame(&emitted[0], 42, &third_samples);
    }

    #[test]
    fn encoder_rejects_invalid_input_without_touching_output() {
        let layout = SpeakerLayout::Surround51;
        let samples = frame_samples(layout, 13);
        let mut out = vec![1, 2, 3];

        assert!(encode_fragment(layout, 1, 0, &samples[..samples.len() - 1], &mut out).is_err());
        assert_eq!(out, vec![1, 2, 3]);

        let mut nonfinite = samples.clone();
        nonfinite[17] = f32::INFINITY;
        assert!(encode_fragment(layout, 1, 0, &nonfinite, &mut out).is_err());
        assert_eq!(out, vec![1, 2, 3]);

        assert!(encode_fragment(layout, 1, fragment_count(layout), &samples, &mut out).is_err());
        assert_eq!(out, vec![1, 2, 3]);
    }

    #[test]
    fn spatial_loss_matrix_uses_sample_frames_for_the_whole_frame_plc_threshold() {
        for layout in LAYOUTS {
            let samples = frame_samples(layout, 3);
            let encoded = fragments(layout, 10, &samples);
            let future = fragments(layout, 12, &samples);
            for received in 1..encoded.len() {
                for tail in [false, true] {
                    for quiet in [false, true] {
                        let mut reassembler = SpatialReassembler::new(layout);
                        let start = if tail { encoded.len() - received } else { 0 };
                        let mut real_frames = 0;
                        for fragment in &encoded[start..start + received] {
                            real_frames += (fragment.len() - FRAGMENT_HEADER_LEN)
                                / (layout.channels() * SAMPLE_BYTES);
                            assert!(reassembler.push(fragment).unwrap().is_empty());
                        }
                        let retired = if quiet {
                            reassembler.flush_expired()
                        } else {
                            reassembler.push(&future[0]).unwrap()
                        };
                        assert_eq!(retired.len(), 1);
                        assert_eq!(retired[0].concealed_fragments, encoded.len() - received);
                        assert_eq!(retired[0].concealed_frames, FRAME_FRAMES - real_frames);
                        assert_eq!(
                            retired[0].requires_whole_frame_concealment(),
                            real_frames < FRAME_FRAMES / 2
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn contract_enforces_version_revision_text_bounds_and_control_characters() {
        let valid = SpatialMediaContract {
            version: VERSION,
            provider_revision: 1,
            endpoint_id: "e".repeat(1024),
            active_format: "f".repeat(128),
            layout: SpeakerLayout::Immersive714,
        };
        valid.validate().unwrap();
        assert_eq!(valid.channels(), 12);
        let output = valid.output_config();
        assert_eq!(output.endpoint_id, valid.endpoint_id);
        assert_eq!(output.active_format, valid.active_format);
        assert_eq!(output.layout, valid.layout);

        let mut invalid = valid.clone();
        invalid.version = VERSION + 1;
        assert!(invalid.validate().is_err());
        invalid = valid.clone();
        invalid.provider_revision = 0;
        assert!(invalid.validate().is_err());
        invalid = valid.clone();
        invalid.endpoint_id.clear();
        assert!(invalid.validate().is_err());
        invalid = valid.clone();
        invalid.endpoint_id.push('e');
        assert!(invalid.validate().is_err());
        invalid = valid.clone();
        invalid.active_format.push('f');
        assert!(invalid.validate().is_err());
        invalid = valid.clone();
        invalid.endpoint_id = "endpoint\nname".to_owned();
        assert!(invalid.validate().is_err());
        invalid = valid;
        invalid.active_format = "format\u{7f}".to_owned();
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn media_aead_authenticates_spatial_prefix_header_and_ciphertext() {
        let layout = SpeakerLayout::Surround71;
        let samples = frame_samples(layout, 14);
        let encoded = fragments(layout, 900, &samples);
        let crypto = MediaCrypto::new_for_stream(&[0x5a; 32], 17, b"spatial-test-salt");
        let mut datagrams = Vec::new();

        for (part, plaintext) in encoded.iter().enumerate() {
            let header = Header {
                kind: Kind::Media,
                codec: Codec::PcmF32le,
                channels: layout.channels() as u8,
                sample_rate: SPATIAL_SAMPLE_RATE,
                session_id: 23,
                stream_id: 17,
                seq: part as u32,
                timestamp_us: 4_500_000,
                payload_len: plaintext.len() as u32,
            };
            datagrams.push(crypto.seal(&header, plaintext).unwrap());
        }

        let mut reassembler = SpatialReassembler::new(layout);
        let mut tampered_prefix = datagrams[0].clone();
        tampered_prefix[HEADER_LEN] ^= 1;
        assert!(crypto.open(&tampered_prefix).is_err());

        let mut tampered_header = datagrams[0].clone();
        tampered_header[7] ^= 1;
        assert!(crypto.open(&tampered_header).is_err());

        let mut tampered_ciphertext = datagrams[0].clone();
        tampered_ciphertext[HEADER_LEN + FRAGMENT_HEADER_LEN] ^= 1;
        assert!(crypto.open(&tampered_ciphertext).is_err());
        assert!(reassembler.pending.is_empty());

        let mut emitted = Vec::new();
        for datagram in datagrams {
            let (header, plaintext) = crypto.open(&datagram).unwrap();
            assert_eq!(header.kind, Kind::Media);
            assert_eq!(header.codec, Codec::PcmF32le);
            assert_eq!(header.channels, layout.channels() as u8);
            assert_eq!(header.sample_rate, SPATIAL_SAMPLE_RATE);
            assert_eq!(header.timestamp_us, 4_500_000);
            emitted.extend(reassembler.push(&plaintext).unwrap());
        }
        assert_eq!(emitted.len(), 1);
        assert_frame(&emitted[0], 900, &samples);
    }
}
