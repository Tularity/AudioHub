//! Explicitly negotiated provider-side fixed-layout PCM rendering.

use crate::{lk, rd, DaemonInner, RxStream};
use anyhow::{bail, Result};
use audiohub_core::audio::Dll;
use audiohub_core::dsp::InterleavedLinearResampler;
use audiohub_core::output_capabilities::{
    NativeOutputCapabilities, NativeOutputObservation, SpatialAudioCapabilities,
};
use audiohub_core::spatial_output::{SpatialOutput, SpeakerLayout};
use audiohub_net::packet::{Codec, Header};
use audiohub_net::secure::SpatialOutputOffer;
use audiohub_net::spatial_media::{
    ReassembledFrame, SpatialMediaContract, SpatialReassembler, FRAME_FRAMES, VERSION,
};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

const OUTPUT_TARGET_FRAMES: usize = 480;
const OUTPUT_PREROLL_FRAMES: usize = OUTPUT_TARGET_FRAMES + FRAME_FRAMES;

fn native_mask(layout: SpeakerLayout) -> u32 {
    match layout {
        SpeakerLayout::Surround51 => 0x7E,
        SpeakerLayout::Surround71 => 0x1FE,
        SpeakerLayout::Immersive714 => 0x1FFE,
    }
}

fn rate_correction(dll: &mut Dll, queued_frames: usize) -> f64 {
    // A shallow output queue needs more produced samples, hence a shorter
    // resampling step. Reuse the existing bounded, anti-windup controller.
    dll.update_clamped(
        OUTPUT_TARGET_FRAMES as f64 - queued_frames as f64,
        0.9995,
        1.0005,
    )
    .0
}

fn offer_for_platform(
    observation: &NativeOutputObservation,
    implemented: bool,
) -> SpatialOutputOffer {
    let mut offer = SpatialOutputOffer {
        revision: observation.revision,
        contracts: Vec::new(),
    };
    if !implemented || observation.revision == 0 || !observation.capabilities.is_valid() {
        return offer;
    }
    if let NativeOutputCapabilities::Available {
        endpoint_id,
        spatial_audio:
            SpatialAudioCapabilities::Available {
                active_format: Some(active_format),
                static_object_mask,
                ..
            },
        ..
    } = &observation.capabilities
    {
        // These are native AudioObjectType masks, not WAVE channel masks.
        for layout in [
            SpeakerLayout::Surround51,
            SpeakerLayout::Surround71,
            SpeakerLayout::Immersive714,
        ] {
            let mask = native_mask(layout);
            if static_object_mask & mask == mask {
                offer.contracts.push(SpatialMediaContract {
                    version: VERSION,
                    provider_revision: observation.revision,
                    endpoint_id: endpoint_id.clone(),
                    active_format: active_format.clone(),
                    layout,
                });
            }
        }
    }
    offer
}

pub(crate) fn offer_for(observation: &NativeOutputObservation) -> SpatialOutputOffer {
    offer_for_platform(observation, cfg!(target_os = "windows"))
}

pub(crate) fn accept_offer(
    current: &mut Option<SpatialOutputOffer>,
    mut offer: SpatialOutputOffer,
) {
    if current
        .as_ref()
        .is_some_and(|old| offer.revision <= old.revision)
    {
        return;
    }
    let valid = offer.contracts.len() <= 3
        && offer.contracts.iter().enumerate().all(|(i, c)| {
            c.validate().is_ok()
                && c.provider_revision == offer.revision
                && !offer.contracts[..i]
                    .iter()
                    .any(|old| old.layout == c.layout)
                && offer.contracts.first().is_none_or(|first| {
                    first.endpoint_id == c.endpoint_id && first.active_format == c.active_format
                })
        });
    if !valid {
        offer.contracts.clear();
    }
    *current = Some(offer);
}

pub(crate) fn require_current(
    contract: &SpatialMediaContract,
    observation: &NativeOutputObservation,
) -> Result<()> {
    contract.validate()?;
    if !contract_matches_observation(contract, observation, cfg!(target_os = "windows")) {
        bail!("spatial PCM contract is not available on the current provider output");
    }
    Ok(())
}

fn contract_matches_observation(
    contract: &SpatialMediaContract,
    observation: &NativeOutputObservation,
    implemented: bool,
) -> bool {
    if !implemented || contract.provider_revision != observation.revision {
        return false;
    }
    matches!(&observation.capabilities,
        NativeOutputCapabilities::Available {
            endpoint_id,
            spatial_audio: SpatialAudioCapabilities::Available {
                active_format: Some(format), static_object_mask, ..
            }, ..
        } if endpoint_id == &contract.endpoint_id && format == &contract.active_format
            && static_object_mask & native_mask(contract.layout) == native_mask(contract.layout)
    )
}

pub(crate) struct SpatialReceive {
    pub(crate) contract: SpatialMediaContract,
    output_epoch: u64,
    output: SpatialOutput,
    assembly: SpatialReassembler,
    last_packet: Option<Instant>,
    frame: Vec<f32>,
    failure: Option<String>,
    submitted_frames: u64,
    concealed_fragments: u64,
    next_tuning: Instant,
    dll: Dll,
    resampler: InterleavedLinearResampler,
    corrected: Vec<f32>,
    rate_correction: f64,
    last_render: Instant,
}

impl SpatialReceive {
    pub(crate) fn start(contract: SpatialMediaContract, output_epoch: u64) -> Result<Self> {
        contract.validate()?;
        let mut output = SpatialOutput::start(contract.output_config())?;
        let preroll = vec![0.0; OUTPUT_PREROLL_FRAMES * contract.channels() as usize];
        if output.write_interleaved(&preroll)? != OUTPUT_PREROLL_FRAMES {
            bail!("native spatial output could not accept its initial clock margin");
        }
        Ok(Self {
            assembly: SpatialReassembler::new(contract.layout),
            frame: vec![0.0; FRAME_FRAMES * contract.channels() as usize],
            corrected: Vec::with_capacity((FRAME_FRAMES + 2) * contract.channels() as usize),
            resampler: InterleavedLinearResampler::servoed(48_000, 48_000, contract.channels()),
            contract,
            output_epoch,
            output,
            last_packet: None,
            failure: None,
            submitted_frames: 0,
            concealed_fragments: 0,
            next_tuning: Instant::now(),
            dll: Dll::new(Dll::BW_MAX, FRAME_FRAMES as u32, 48_000),
            rate_correction: 1.0,
            last_render: Instant::now(),
        })
    }

    fn fail(&mut self, reason: String) {
        if self.failure.is_none() {
            self.failure = Some(reason);
        }
        self.output.request_stop();
    }

    pub(crate) fn stop(&self) {
        self.output.request_stop();
    }

    pub(crate) fn queued_frames(&self) -> usize {
        self.output.queued_frames()
    }

    pub(crate) fn status(&self) -> serde_json::Value {
        serde_json::json!({
            "contract": self.contract,
            "submitted_frames": self.submitted_frames,
            "preroll_frames": OUTPUT_PREROLL_FRAMES,
            "correction_ppm": (self.rate_correction - 1.0) * 1e6,
            "consumed_frames": self.output.consumed_frames(),
            "rendered_frames": self.output.rendered_frames(),
            "queued_frames": self.output.queued_frames(),
            "underrun_frames": self.output.underrun_frames(),
            "concealed_fragments": self.concealed_fragments,
            "failure": self.failure.clone().or_else(|| self.output.failure()),
        })
    }
}

fn push_frames(rx: &RxStream, spatial: &mut SpatialReceive, frames: Vec<ReassembledFrame>) {
    let mut state = lk(&rx.jbs);
    for frame in frames {
        // The existing jitter timeline is u32 and intentionally non-wrapping.
        // A sender must establish a fresh stream before this horizon (and
        // before exhausting its much shorter per-packet AEAD sequence space).
        let Ok(sequence) = u32::try_from(frame.frame_seq) else {
            spatial.fail("spatial frame sequence requires a new stream".into());
            return;
        };
        spatial.concealed_fragments += frame.concealed_fragments as u64;
        if frame.requires_whole_frame_concealment() {
            continue; // Leave a sequence hole for the existing jitter-buffer PLC.
        }
        if frame.concealed_fragments != 0 {
            // At least half the sample frames are real. The existing 0.5-frame
            // charge is a conservative upper bound on this partial concealment.
            state.half_conceal = state.half_conceal.saturating_add(1);
        }
        state.jb.push(sequence, frame.samples);
        state.pushes = state.pushes.saturating_add(1);
        if state.pushes % 10 == 0 {
            state.sample_conceal();
        }
    }
}

pub(crate) fn receive(rx: &RxStream, header: &Header, plain: &[u8], jitter_ms: f32) {
    if header.channels != rx.channels
        || header.sample_rate != 48_000
        || header.codec != Codec::PcmF32le
    {
        lk(&rx.stats).format_mismatch += 1;
        return;
    }
    let mut spatial = lk(rx.spatial.as_ref().expect("spatial receive branch"));
    if spatial.failure.is_some() {
        return;
    }
    let frames = match spatial.assembly.push(plain) {
        Ok(frames) => frames,
        Err(_) => {
            drop(spatial);
            lk(&rx.stats).format_mismatch += 1;
            return;
        }
    };
    spatial.last_packet = Some(Instant::now());
    push_frames(rx, &mut spatial, frames);
    let mut state = lk(&rx.jbs);
    state.jit_win.push(jitter_ms);
    if state.jit_win.len() > 256 {
        state.jit_win.remove(0);
    }
}

pub(crate) fn render(inner: &DaemonInner, rx: &RxStream, now_ms: u64) {
    let mut spatial = lk(rx.spatial.as_ref().expect("spatial render branch"));
    if spatial.failure.is_some() {
        return;
    }
    if !lk(&inner.settings).mode.serves_peers()
        || spatial.output_epoch != inner.dev_out_epoch.load(Ordering::Acquire)
        || !contract_matches_observation(
            &spatial.contract,
            &lk(&inner.native_output),
            cfg!(target_os = "windows"),
        )
    {
        spatial.fail("provider output changed; spatial PCM must be renegotiated".into());
        return;
    }
    if let Some(error) = spatial.output.failure() {
        spatial.fail(error);
        return;
    }
    if spatial
        .last_packet
        .is_some_and(|last| last.elapsed() >= Duration::from_millis(30))
    {
        let expired = spatial.assembly.flush_expired();
        spatial.last_packet = None;
        push_frames(rx, &mut spatial, expired);
    }
    if spatial.failure.is_some() {
        return;
    }
    if Instant::now() >= spatial.next_tuning {
        let mut state = lk(&rx.jbs);
        let target = rx.transport.latency_target();
        let reseeded = crate::engine::reshape_jitter_envelope(
            &mut state,
            target,
            crate::engine::jb_tuning_for(&rx.ka_path),
            rx.stream_id,
        );
        if reseeded {
            rx.transport.set_servo_frames(None);
        } else if let Some(want) = rx.transport.servo_frames() {
            crate::engine::steer_jitter_target(&mut state.jb, want);
        } else if matches!(target, audiohub_ipc::LatencyTarget::Auto) && !state.jit_win.is_empty() {
            let mut window = state.jit_win.clone();
            window.sort_by(f32::total_cmp);
            let p95 = window[(window.len() * 95 / 100).min(window.len() - 1)];
            state.jb.update_target(p95 as f64, 10.0);
        }
        spatial.next_tuning = Instant::now() + Duration::from_secs(1);
    }
    let popped = lk(&rx.jbs).jb.pop();
    lk(&rx.post).advance(popped, &mut spatial.frame);
    rx.clip.feed(now_ms, &spatial.frame);
    let queued_frames = spatial.output.queued_frames();
    let now = Instant::now();
    let elapsed = now.duration_since(spatial.last_render);
    spatial.last_render = now;
    if (Duration::from_millis(5)..=Duration::from_millis(20)).contains(&elapsed) {
        spatial.rate_correction = rate_correction(&mut spatial.dll, queued_frames);
    }
    let correction = spatial.rate_correction;
    let SpatialReceive {
        output,
        frame,
        resampler,
        corrected,
        ..
    } = &mut *spatial;
    resampler.set_correction(correction);
    corrected.clear();
    resampler.process(frame, corrected);
    let expected = corrected.len() / rx.channels as usize;
    match output.write_interleaved(corrected) {
        Ok(written) => {
            spatial.submitted_frames += written as u64;
            if written != expected {
                spatial.fail(
                    "native spatial output queue overflow; stream stopped without downmix".into(),
                );
            }
        }
        Err(error) => spatial.fail(format!("native spatial output: {error:#}")),
    }
}

pub(crate) fn reap_failed(inner: &DaemonInner) {
    let streams: Vec<_> = rd(&inner.rx_table).values().cloned().collect();
    for rx in streams {
        let failure = rx.spatial.as_ref().and_then(|spatial| {
            let state = lk(spatial);
            state.failure.clone().or_else(|| state.output.failure())
        });
        if let Some(reason) = failure {
            eprintln!(
                "[audiohubd] closing spatial stream {}: {reason}",
                rx.stream_id
            );
            crate::conn::teardown_stream(inner, rx.stream_id, true);
        }
    }
    let entries = crate::snapshot_sessions(inner);
    for entry in entries {
        if let Some(tx) = &entry.tx {
            if tx.media_failed.load(Ordering::Acquire) {
                let reason = lk(&tx.media_failure).clone().unwrap_or_else(|| "media source failed".into());
                eprintln!("[audiohubd] closing transmit stream {}: {reason}", entry.id);
                crate::conn::teardown_stream(inner, entry.id, true);
            }
        }
    }
}

pub(crate) fn invalidate_peer_transmits(inner: &DaemonInner, connection_id: u64, offer: &SpatialOutputOffer) {
    for entry in crate::snapshot_sessions(inner) {
        if entry.conn.connection_id != connection_id { continue; }
        if let Some(tx) = &entry.tx {
            if tx.spatial.as_ref().is_some_and(|contract| !offer.contracts.contains(contract)) {
                tx.fail_media("provider spatial PCM contract changed; a new stream is required".into());
                crate::conn::teardown_stream(inner, entry.id, true);
            }
        }
    }
}

pub(crate) fn status(inner: &DaemonInner) -> serde_json::Value {
    let offer = offer_for(&lk(&inner.native_output));
    let streams: Vec<_> = rd(&inner.rx_table).values().cloned().collect();
    let receivers: Vec<_> = streams
        .iter()
        .filter_map(|rx| {
            rx.spatial.as_ref().map(|state| {
                serde_json::json!({
                    "stream_id": rx.stream_id, "output": lk(state).status(),
                })
            })
        })
        .collect();
    let connections: Vec<_> = lk(&inner.state).conns.values().cloned().collect();
    let peers: Vec<_> = connections
        .iter()
        .map(|conn| {
            serde_json::json!({
                "fingerprint": conn.fp, "offer": lk(&conn.peer_spatial_output).clone(),
            })
        })
        .collect();
    serde_json::json!({"offer": offer, "receivers": receivers, "peers": peers})
}

pub(crate) fn append_output_latency(
    pipeline: &mut audiohub_ipc::PipelineLatency,
    queued_frames: usize,
) {
    use audiohub_core::latency::{DevLatency, DropMode, StageDepth, StageId};
    pipeline.stages.push(crate::to_ipc_stage(
        StageDepth {
            id: StageId::PlayRing,
            samples: queued_frames as u32,
            capacity: audiohub_core::spatial_output::SPATIAL_QUEUE_FRAMES as u32,
            rate: 48_000,
            dropped: None,
            drop_mode: DropMode::Newest,
        },
        None,
    ));
    pipeline.local_ms = crate::sum_stage_ms(&pipeline.stages);
    // The ordinary WASAPI calibration does not measure the native spatial
    // processing tail. Its absence must never contribute a fabricated zero.
    pipeline.dev = Some(DevLatency::unavailable());
}

#[cfg(test)]
mod tests {
    use super::*;
    use audiohub_core::output_capabilities::OutputMixFormat;

    fn observation() -> NativeOutputObservation {
        NativeOutputObservation {
            revision: 7,
            capabilities: NativeOutputCapabilities::Available {
                endpoint_id: "provider-default".into(),
                mix_format: OutputMixFormat {
                    sample_rate: 48_000,
                    channels: 2,
                    sample_format: "f32".into(),
                    channel_mask: Some(3),
                },
                spatial_audio: SpatialAudioCapabilities::Available {
                    active_format: Some("native-renderer-format".into()),
                    configured_format: None,
                    max_dynamic_objects: 128,
                    static_object_mask: 0x1FFE,
                },
            },
        }
    }

    #[test]
    fn spatial_offer_requires_an_implemented_sink_and_matching_native_positions() {
        let mut facts = observation();
        assert!(offer_for_platform(&facts, false).contracts.is_empty());
        let offer = offer_for_platform(&facts, true);
        assert_eq!(
            offer
                .contracts
                .iter()
                .map(|c| c.channels())
                .collect::<Vec<_>>(),
            vec![6, 8, 12]
        );
        for contract in &offer.contracts {
            assert_eq!(contract.provider_revision, 7);
            assert!(contract.validate().is_ok());
        }
        if let NativeOutputCapabilities::Available {
            spatial_audio:
                SpatialAudioCapabilities::Available {
                    static_object_mask, ..
                },
            ..
        } = &mut facts.capabilities
        {
            *static_object_mask = 0x7E;
        }
        assert_eq!(offer_for_platform(&facts, true).contracts.len(), 1);
        facts.capabilities = NativeOutputCapabilities::Unknown;
        assert!(offer_for_platform(&facts, true).contracts.is_empty());
    }

    #[test]
    fn spatial_remote_offer_cannot_reuse_stale_or_mixed_output_contracts() {
        let good = offer_for_platform(&observation(), true);
        let mut current = None;
        accept_offer(&mut current, good.clone());
        let mut old = good.clone();
        old.contracts.clear();
        accept_offer(&mut current, old);
        assert_eq!(current.as_ref().unwrap(), &good);
        let mut bad = good;
        bad.revision += 1;
        accept_offer(&mut current, bad);
        assert!(current.as_ref().unwrap().contracts.is_empty());
        let mut mixed = offer_for_platform(&observation(), true);
        mixed.revision = 9;
        for contract in &mut mixed.contracts {
            contract.provider_revision = 9;
        }
        mixed.contracts[1].endpoint_id = "another-output".into();
        accept_offer(&mut current, mixed);
        assert!(current.unwrap().contracts.is_empty());
    }

    #[test]
    fn spatial_current_contract_is_invalidated_by_revision_endpoint_format_and_mask_changes() {
        let facts = observation();
        let contract = offer_for_platform(&facts, true).contracts.pop().unwrap();
        assert!(contract_matches_observation(&contract, &facts, true));
        assert!(!contract_matches_observation(&contract, &facts, false));
        let mut changed = facts.clone();
        changed.revision += 1;
        assert!(!contract_matches_observation(&contract, &changed, true));
        for field in 0..3 {
            let mut changed = facts.clone();
            if let NativeOutputCapabilities::Available {
                endpoint_id,
                spatial_audio:
                    SpatialAudioCapabilities::Available {
                        active_format,
                        static_object_mask,
                        ..
                    },
                ..
            } = &mut changed.capabilities
            {
                match field {
                    0 => *endpoint_id = "new-default".into(),
                    1 => *active_format = Some("new-format".into()),
                    _ => *static_object_mask = 0x1FE,
                }
            }
            assert!(!contract_matches_observation(&contract, &changed, true));
        }
    }

    #[test]
    fn spatial_jitter_buffer_keeps_twelve_channels_and_concealment_frame_alignment() {
        let mut buffer = audiohub_net::media::JitterBuffer::with_tuning_channels(
            2,
            audiohub_net::media::JbTuning::DEFAULT,
            12,
        );
        let frame: Vec<f32> = (0..FRAME_FRAMES * 12)
            .map(|n| (n % 12) as f32 / 16.0)
            .collect();
        for sequence in 0..8 {
            buffer.push(sequence, frame.clone());
        }
        let mut seen = false;
        for _ in 0..12 {
            if let Some(output) = buffer.pop() {
                seen = true;
                assert_eq!(output.len(), FRAME_FRAMES * 12);
                for sample_frame in output.chunks_exact(12) {
                    assert_eq!(sample_frame[0], 0.0);
                    assert!(sample_frame.windows(2).all(|pair| pair[0] <= pair[1]));
                }
            }
        }
        assert!(seen);
    }

    #[test]
    fn spatial_output_rate_control_converges_for_both_clock_directions() {
        for ppm in [-200.0, 200.0] {
            let mut dll = Dll::new(Dll::BW_MAX, FRAME_FRAMES as u32, 48_000);
            let mut depth = OUTPUT_TARGET_FRAMES as f64;
            for _ in 0..18_000 {
                let correction = rate_correction(&mut dll, depth.round().max(0.0) as usize);
                depth += FRAME_FRAMES as f64 / correction - FRAME_FRAMES as f64 * (1.0 + ppm / 1e6);
                assert!((0.0..4800.0).contains(&depth));
            }
            assert!(
                (depth - OUTPUT_TARGET_FRAMES as f64).abs() < 50.0,
                "ppm={ppm}, depth={depth}"
            );
        }
    }

    #[test]
    fn spatial_rate_conversion_preserves_all_twelve_lane_histories() {
        let mut resampler = InterleavedLinearResampler::servoed(48_000, 48_000, 12);
        resampler.set_correction(1.0002);
        let input: Vec<f32> = (0..FRAME_FRAMES * 12)
            .map(|n| (n % 12) as f32 / 16.0)
            .collect();
        let mut output = Vec::new();
        for _ in 0..8 {
            output.clear();
            resampler.process(&input, &mut output);
            assert_eq!(resampler.channels(), 12);
            assert_eq!(output.len() % 12, 0);
            for frame in output.chunks_exact(12).skip(1) {
                for (channel, sample) in frame.iter().enumerate() {
                    assert!((*sample - channel as f32 / 16.0).abs() < 1e-6);
                }
            }
        }
    }

    #[test]
    fn spatial_queue_latency_includes_the_queue_but_not_a_fabricated_device_tail() {
        let mut pipeline: audiohub_ipc::PipelineLatency =
            serde_json::from_value(serde_json::json!({
                "side": "recv", "stages": [], "peer_stages": [], "confidence": "localOnly",
            }))
            .unwrap();
        append_output_latency(&mut pipeline, 960);
        assert_eq!(pipeline.local_ms, Some(20.0));
        assert_eq!(pipeline.stages[0].id, "play_ring");
        assert_eq!(pipeline.stages[0].samples, 960);
        assert_eq!(crate::devlats::dev_sum_ms(pipeline.dev), None);
        assert_eq!(
            crate::compose_sum_ms(pipeline.local_ms, Some(1.0), Some(0.0), pipeline.dev, None),
            None
        );
        assert!(!crate::devlats::both_exact(pipeline.dev, None));
    }
}
