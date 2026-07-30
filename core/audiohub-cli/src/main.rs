mod ctl;
mod m3;

use std::io::ErrorKind;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use clap::{Parser, Subcommand};

use audiohub_core::audio::{default_devices_report, play_samples_blocking, LiveCapture, LivePlayback};
use audiohub_core::{dsp, sysaudio};
use audiohub_net::echo::{run_echo_client, run_echo_server_for, EchoCfg};
use audiohub_net::media::{FrameSource, SysAudioSource, ToneSource};
use audiohub_net::packet::{Codec, Header, Kind, PacketError};
use audiohub_net::session::{run_rx, run_tx_tone, RxCfg, RxMode, ToneTxCfg, TxMode, TxReport};

const DEFAULT_PORT: u16 = 47810;
const EXIT_CHECK_FAILED: i32 = 2;
const EXIT_NO_TRAFFIC: i32 = 3;
const EXIT_ERROR: i32 = 4;

#[derive(Parser)]
#[command(name = "audiohub", disable_help_subcommand = true)]
struct Cli {
    #[command(subcommand)]
    cmd: TopCmd,
    /// emit exactly one JSON line on stdout
    #[arg(long, global = true)]
    json: bool,
}

#[derive(Subcommand)]
enum TopCmd {
    Probe {
        #[command(subcommand)]
        cmd: ProbeCmd,
    },
    #[command(flatten)]
    M3(m3::M3Cmd),
    #[command(flatten)]
    M4(ctl::M4Cmd),
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum Source {
    Tone,
    Mic,
}

#[derive(Subcommand)]
enum ProbeCmd {
    Devices,
    Tone {
        #[arg(long, default_value_t = 440.0)]
        freq: f32,
        #[arg(long, default_value_t = 2.0)]
        secs: f32,
        #[arg(long, default_value_t = 0.2)]
        amp: f32,
        /// play into a NAMED output device instead of the system default —
        /// the equivalent of picking that device inside an app
        #[arg(long)]
        device: Option<String>,
    },
    Loopback {
        #[arg(long, default_value_t = 5.0)]
        secs: f32,
    },
    /// Capture from a NAMED input device and optionally verify a tone in it
    /// (spec-m4c §B): this is what "any app can select that virtual card and
    /// hear the peer's microphone" actually means, measured.
    Capture {
        /// input device name (exact, or a case-insensitive prefix)
        #[arg(long)]
        device: String,
        #[arg(long, default_value_t = 5.0)]
        secs: f32,
        /// assert this frequency IS present in the capture
        #[arg(long)]
        verify_freq: Option<f32>,
    },
    Selftest,
    Tx {
        #[arg(long, conflicts_with = "serve")]
        to: Option<SocketAddr>,
        #[arg(long)]
        serve: bool,
        #[arg(long, default_value_t = DEFAULT_PORT)]
        port: u16,
        #[arg(long, value_enum, default_value = "tone")]
        source: Source,
        #[arg(long, default_value_t = 1000.0)]
        freq: f32,
        #[arg(long, default_value_t = 0.5)]
        amp: f32,
        #[arg(long, default_value_t = 10.0)]
        secs: f32,
    },
    Rx {
        #[arg(long, default_value_t = DEFAULT_PORT)]
        port: u16,
        #[arg(long)]
        pull: Option<SocketAddr>,
        #[arg(long)]
        play: bool,
        #[arg(long)]
        verify_freq: Option<f32>,
        #[arg(long, default_value_t = 10.0)]
        secs: f32,
    },
    EchoServer {
        #[arg(long, default_value_t = DEFAULT_PORT)]
        port: u16,
        #[arg(long, default_value_t = 60.0)]
        secs: f32,
    },
    /// System-audio capture (spec-m4b §B2): backend inventory, or capture +
    /// spectral verification of what this machine is playing.
    Sysaudio {
        /// list backends and detected virtual cards, then exit
        #[arg(long)]
        list: bool,
        #[arg(long, default_value = "auto")]
        backend: String,
        #[arg(long, default_value_t = 5.0)]
        secs: f32,
        /// assert this frequency IS present in the capture
        #[arg(long)]
        verify_freq: Option<f32>,
        /// assert this frequency is NOT present (self-exclusion proof)
        #[arg(long)]
        absent_freq: Option<f32>,
        /// play this tone from THIS process while capturing
        #[arg(long)]
        self_tone: Option<f32>,
        /// pull a media stream from this peer and play it from THIS process
        #[arg(long)]
        play_pull: Option<SocketAddr>,
        /// forward the captured audio as media packets (repeatable)
        #[arg(long)]
        to: Vec<SocketAddr>,
    },
    Echo {
        #[arg(long)]
        to: SocketAddr,
        #[arg(long, default_value_t = 200)]
        count: u32,
        #[arg(long, default_value_t = 10)]
        interval_ms: u64,
        #[arg(long, default_value_t = 960)]
        size: usize,
    },
}

fn info(msg: &str) {
    eprintln!("[audiohub] {msg}");
}

fn emit_json<T: serde::Serialize>(json: bool, value: &T) {
    if json {
        println!("{}", serde_json::to_string(value).expect("json encode"));
    }
}

fn main() {
    let cli = Cli::parse();
    let result = match cli.cmd {
        TopCmd::Probe { cmd } => dispatch(cmd, cli.json),
        TopCmd::M3(cmd) => m3::dispatch(cmd, cli.json),
        TopCmd::M4(cmd) => ctl::dispatch(cmd, cli.json),
    };
    let code = match result {
        Ok(code) => code,
        Err(e) => {
            info(&format!("error: {e:#}"));
            EXIT_ERROR
        }
    };
    std::process::exit(code);
}

fn dispatch(cmd: ProbeCmd, json: bool) -> Result<i32> {
    match cmd {
        ProbeCmd::Devices => {
            let report = default_devices_report()?;
            info(&format!("{report:?}"));
            emit_json(json, &report);
            Ok(0)
        }
        ProbeCmd::Tone { freq, secs, amp, device } => {
            let samples = dsp::gen_sine(freq, 48000, (48000.0 * secs) as usize, amp);
            match device.as_deref() {
                Some(name) => {
                    info(&format!("playing {freq} Hz for {secs}s (amp {amp}) into '{name}'"));
                    // Feed the named device in real time: LivePlayback::start_on
                    // resolves the name (no silent fallback to the default) and
                    // owns the stream, so the tone lands where it was asked to.
                    let (guard, mut tx) = LivePlayback::start_on(name, 48000)?;
                    // Pace against an ABSOLUTE clock with a lead, never against a
                    // fixed sleep: `sleep(10ms)` always overshoots, so pushing one
                    // 10ms frame per sleep feeds slower than 48k is consumed and
                    // the stream underruns into near-silence (measured: a 1200 Hz
                    // tone came out at -5 dB SNR, indistinguishable from a broken
                    // audio path — which is exactly what it was mistaken for).
                    const LEAD: usize = 4800; // 100ms of slack in the ring
                    let mut sent = 0usize;
                    let start = Instant::now();
                    while sent < samples.len() {
                        let due = (start.elapsed().as_secs_f64() * 48000.0) as usize + LEAD;
                        while sent < samples.len() && sent < due {
                            let end = (sent + 480).min(samples.len());
                            tx.push(&samples[sent..end]);
                            sent = end;
                        }
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    // the ring still holds up to LEAD samples the device has not played
                    std::thread::sleep(Duration::from_millis(300));
                    drop(guard);
                }
                None => {
                    info(&format!("playing {freq} Hz tone for {secs}s (amp {amp})"));
                    play_samples_blocking(&samples, 48000)?;
                }
            }
            emit_json(
                json,
                &serde_json::json!({"ok": true, "freq": freq, "secs": secs, "device": device}),
            );
            Ok(0)
        }
        ProbeCmd::Loopback { secs } => cmd_loopback(secs, json),
        ProbeCmd::Capture {
            device,
            secs,
            verify_freq,
        } => cmd_capture(&device, secs, verify_freq, json),
        ProbeCmd::Selftest => cmd_selftest(json),
        ProbeCmd::Tx {
            to,
            serve,
            port,
            source,
            freq,
            amp,
            secs,
        } => cmd_tx(to, serve, port, source, freq, amp, secs, json),
        ProbeCmd::Rx {
            port,
            pull,
            play,
            verify_freq,
            secs,
        } => cmd_rx(port, pull, play, verify_freq, secs, json),
        ProbeCmd::EchoServer { port, secs } => {
            let sock = UdpSocket::bind(("0.0.0.0", port))?;
            info(&format!("echo server on 0.0.0.0:{port} for {secs}s"));
            let handled = run_echo_server_for(&sock, secs)?;
            info(&format!("handled {handled} echo requests"));
            emit_json(json, &serde_json::json!({"handled": handled}));
            Ok(0)
        }
        ProbeCmd::Sysaudio {
            list,
            backend,
            secs,
            verify_freq,
            absent_freq,
            self_tone,
            play_pull,
            to,
        } => cmd_sysaudio(
            list,
            &backend,
            secs,
            verify_freq,
            absent_freq,
            self_tone,
            play_pull,
            &to,
            json,
        ),
        ProbeCmd::Echo {
            to,
            count,
            interval_ms,
            size,
        } => {
            let sock = UdpSocket::bind("0.0.0.0:0")?;
            let cfg = EchoCfg {
                count,
                interval_ms,
                size,
                ..EchoCfg::default()
            };
            info(&format!("echo {count} packets to {to}"));
            let summary = run_echo_client(&sock, to, &cfg)?;
            info(&format!(
                "sent={} received={} loss={:.2}% p50={:.2}ms p95={:.2}ms",
                summary.sent, summary.received, summary.loss_pct, summary.rtt_p50_ms, summary.rtt_p95_ms
            ));
            emit_json(json, &summary);
            Ok(if summary.received == 0 { EXIT_NO_TRAFFIC } else { 0 })
        }
    }
}

fn cmd_loopback(secs: f32, json: bool) -> Result<i32> {
    info("starting mic->speaker loopback; macOS may show a microphone permission (TCC) prompt");
    let (capture, mut rx, capture_rate) = LiveCapture::start()?;
    let (playback, mut tx) = LivePlayback::start(capture_rate)?;
    let deadline = Instant::now() + Duration::from_secs_f32(secs);
    let mut buf: Vec<f32> = Vec::new();
    while Instant::now() < deadline {
        buf.clear();
        if rx.pop(&mut buf) > 0 {
            tx.push(&buf);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    drop(capture);
    drop(playback);
    emit_json(
        json,
        &serde_json::json!({"ok": true, "secs": secs, "capture_rate": capture_rate}),
    );
    Ok(0)
}

/// probe capture (spec-m4c §B4). Reads a NAMED input device — typically the
/// input end of the virtual card a mic session bridges into — and reports a
/// ToneVerdict for it. Never touches the system default device.
fn cmd_capture(device: &str, secs: f32, verify_freq: Option<f32>, json: bool) -> Result<i32> {
    info(&format!(
        "capturing from '{device}' for {secs}s; macOS may show a microphone permission (TCC) prompt"
    ));
    let (capture, mut rx, rate) = LiveCapture::start_on(device)?;
    // one extra second of slack so the tail of the window is never clipped
    let cap = (rate as f32 * secs.max(0.0)) as usize + rate as usize;
    let mut accum: Vec<f32> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs_f32(secs);
    while Instant::now() < deadline {
        rx.pop(&mut accum);
        if accum.len() > cap {
            accum.truncate(cap);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    drop(capture);

    let rms = if accum.is_empty() {
        0.0
    } else {
        (accum.iter().map(|s| (*s as f64) * (*s as f64)).sum::<f64>() / accum.len() as f64).sqrt()
    };
    let verdict = verify_freq.map(|f| dsp::verify_tone(&accum, rate, f));
    let detected = verdict.as_ref().map(|v| v.detected).unwrap_or(false);
    let ok = verify_freq.is_none() || detected;
    info(&format!(
        "captured {} samples ({:.2}s @ {rate} Hz) rms={rms:.5}",
        accum.len(),
        accum.len() as f32 / rate.max(1) as f32,
    ));
    if let Some(v) = &verdict {
        info(&format!(
            "verify {} Hz: detected={} snr={:.1}dB",
            v.freq_hz, v.detected, v.snr_db
        ));
    }
    emit_json(
        json,
        &serde_json::json!({
            "device": device,
            "capture_rate": rate,
            "secs": secs,
            "samples": accum.len(),
            "rms": rms,
            "verdict": verdict,
            "detected": detected,
            "ok": ok,
        }),
    );
    if accum.is_empty() {
        return Ok(EXIT_NO_TRAFFIC);
    }
    if !ok {
        return Ok(EXIT_CHECK_FAILED);
    }
    Ok(0)
}

/// probe sysaudio (spec-m4b §B2). Capture what the machine is playing, with
/// two optional in-process players (`--self-tone`, `--play-pull`): audio this
/// very process emits is exactly what a self-excluding backend must keep out of
/// the capture, so `--verify-freq F --absent-freq G` is the whole
/// self-exclusion assertion in one command. `--to` forwards the captured audio
/// so the far side can judge it instead.
#[allow(clippy::too_many_arguments)]
fn cmd_sysaudio(
    list: bool,
    backend: &str,
    secs: f32,
    verify_freq: Option<f32>,
    absent_freq: Option<f32>,
    self_tone: Option<f32>,
    play_pull: Option<SocketAddr>,
    dests: &[SocketAddr],
    json: bool,
) -> Result<i32> {
    if list {
        let backends = sysaudio::list_backends();
        let cards = sysaudio::detect_virtual_cards();
        for b in &backends {
            info(&format!(
                "backend {:<20} available={} excludes_self={} ({})",
                b.id, b.available, b.excludes_self, b.note
            ));
        }
        for c in &cards {
            info(&format!(
                "virtual card {:<12} present={} kind={} name={}",
                c.id, c.present, c.kind, c.name
            ));
        }
        emit_json(
            json,
            &serde_json::json!({"backends": backends, "virtual_cards": cards}),
        );
        return Ok(0);
    }

    const FRAME_MS: u64 = 10;
    let rate = SysAudioSource::OUT_RATE;
    let mut src = SysAudioSource::new(FRAME_MS as u32, backend)?;
    let chosen = src.backend().clone();
    let capture_rate = src.capture_rate();
    info(&format!(
        "capturing system audio via {} (excludes_self={}, {} Hz) for {secs}s",
        chosen.id,
        chosen.excludes_self,
        src.capture_rate()
    ));
    if !chosen.excludes_self && (self_tone.is_some() || play_pull.is_some()) {
        info("WARNING: this backend does not exclude our own playback — expect a feedback loop");
    }

    // Peer stream played by THIS process: run_rx owns the socket and the cpal
    // stream on its own thread (cpal streams are not Send on every platform).
    let pull_secs = secs + 2.0;
    let pull = play_pull.map(|peer| {
        std::thread::spawn(move || -> Result<u64> {
            let sock = UdpSocket::bind("0.0.0.0:0")?;
            let prate = peek_media_rate(&sock, Some(peer), Duration::from_millis(5000))?
                .unwrap_or(48000);
            let (guard, mut audio_tx) = LivePlayback::start(prate)?;
            let cfg = RxCfg {
                secs: pull_secs,
                verify_freq: None,
                idle_timeout_ms: 5000,
            };
            let outcome = run_rx(
                &sock,
                RxMode::Pull(peer),
                &cfg,
                Some(Box::new(move |frame: &[f32]| audio_tx.push(frame))),
            )?;
            drop(guard);
            Ok(outcome.summary.received)
        })
    });

    let mut tone = match self_tone {
        Some(freq) => {
            let (guard, audio_tx) = LivePlayback::start(rate)?;
            Some((guard, audio_tx, ToneSource::new(freq, 0.5, rate, FRAME_MS as u32)))
        }
        None => None,
    };
    let mut tone_frame: Vec<f32> = Vec::new();
    if let Some((_, audio_tx, gen)) = tone.as_mut() {
        for _ in 0..20 {
            gen.next_frame(&mut tone_frame); // 200ms cushion before capture starts
            audio_tx.push(&tone_frame);
        }
    }

    let sock = if dests.is_empty() {
        None
    } else {
        Some(UdpSocket::bind("0.0.0.0:0")?)
    };
    let session_id = {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64
    };

    let accum_cap = rate as usize * 30;
    let mut accum: Vec<f32> = Vec::new();
    let mut frame: Vec<f32> = Vec::new();
    let mut seq = 0u32;
    let mut sent_packets = 0u64;
    let start = Instant::now();
    let deadline = start + Duration::from_secs_f32(secs);
    let mut tick = 0u64;
    while Instant::now() < deadline {
        let next = start + Duration::from_millis(tick * FRAME_MS);
        let now = Instant::now();
        if now < next {
            std::thread::sleep(next - now);
        }
        tick += 1;
        src.next_frame(&mut frame);
        if accum.len() < accum_cap {
            accum.extend_from_slice(&frame);
        }
        if let Some(s) = sock.as_ref() {
            let payload = dsp::f32_to_s16le(&frame);
            let datagram = Header {
                kind: Kind::Media,
                codec: Codec::PcmS16le,
                channels: 1,
                sample_rate: rate,
                session_id,
                stream_id: 0,
                seq,
                timestamp_us: start.elapsed().as_micros() as u64,
                payload_len: payload.len() as u32,
            }
            .encode(&payload);
            for d in dests {
                let _ = s.send_to(&datagram, d);
                sent_packets += 1;
            }
            seq = seq.wrapping_add(1);
        }
        if let Some((_, audio_tx, gen)) = tone.as_mut() {
            gen.next_frame(&mut tone_frame);
            audio_tx.push(&tone_frame);
        }
    }
    if let Some(s) = sock.as_ref() {
        let bye = Header {
            kind: Kind::Bye,
            codec: Codec::PcmS16le,
            channels: 1,
            sample_rate: rate,
            session_id,
            stream_id: 0,
            seq,
            timestamp_us: start.elapsed().as_micros() as u64,
            payload_len: 0,
        }
        .encode(&[]);
        for _ in 0..3 {
            for d in dests {
                let _ = s.send_to(&bye, d);
            }
        }
    }
    drop(tone);
    drop(src);

    let played = match pull {
        Some(h) => match h.join() {
            Ok(Ok(n)) => Some(n),
            Ok(Err(e)) => {
                info(&format!("play-pull failed: {e:#}"));
                Some(0)
            }
            Err(_) => Some(0),
        },
        None => None,
    };

    let rms = if accum.is_empty() {
        0.0
    } else {
        (accum.iter().map(|s| (*s as f64) * (*s as f64)).sum::<f64>() / accum.len() as f64).sqrt()
    };
    let verdict = verify_freq.map(|f| dsp::verify_tone(&accum, rate, f));
    let absent_verdict = absent_freq.map(|f| dsp::verify_tone(&accum, rate, f));
    let want_ok = verdict.as_ref().map(|v| v.detected).unwrap_or(true);
    let absent_ok = !absent_verdict.as_ref().map(|v| v.detected).unwrap_or(false);
    let ok = want_ok && absent_ok;

    info(&format!(
        "captured {} samples ({:.2}s) rms={rms:.5} sent={sent_packets} played={:?}",
        accum.len(),
        accum.len() as f32 / rate as f32,
        played
    ));
    for (label, v) in [("verify", &verdict), ("absent", &absent_verdict)] {
        if let Some(v) = v {
            info(&format!(
                "{label} {} Hz: detected={} snr={:.1}dB",
                v.freq_hz, v.detected, v.snr_db
            ));
        }
    }
    emit_json(
        json,
        &serde_json::json!({
            "backend": chosen.id,
            "excludes_self": chosen.excludes_self,
            "capture_rate": capture_rate,
            "sample_rate": rate,
            "secs": secs,
            "samples": accum.len(),
            "rms": rms,
            "sent_packets": sent_packets,
            "played_packets": played,
            "verdict": verdict,
            "absent_verdict": absent_verdict,
            "ok": ok,
        }),
    );
    if !ok {
        return Ok(EXIT_CHECK_FAILED);
    }
    if accum.is_empty() {
        return Ok(EXIT_NO_TRAFFIC);
    }
    Ok(0)
}

fn cmd_selftest(json: bool) -> Result<i32> {
    let mut checks: Vec<(String, bool)> = Vec::new();
    let mut check = |name: &str, ok: bool| {
        info(&format!("check {name}: {}", if ok { "ok" } else { "FAIL" }));
        checks.push((name.to_string(), ok));
    };

    // packet encode/parse round trip
    let hdr = Header {
        kind: Kind::Media,
        codec: Codec::PcmS16le,
        channels: 1,
        sample_rate: 48000,
        session_id: 0x1122334455667788,
        stream_id: 7,
        seq: 42,
        timestamp_us: 123456789,
        payload_len: 4,
    };
    let payload = [1u8, 2, 3, 4];
    let wire = hdr.encode(&payload);
    let roundtrip = matches!(Header::parse(&wire), Ok((h, p)) if h == hdr && p == &payload[..]);
    check("packet_roundtrip", roundtrip);

    // malformed packets rejected
    let mut bad_magic = wire.clone();
    bad_magic[0] = b'X';
    let mut bad_version = wire.clone();
    bad_version[4] = 9;
    let mut bad_kind = wire.clone();
    bad_kind[5] = 200;
    let mut bad_codec = wire.clone();
    bad_codec[6] = 9;
    let mut bad_len = wire.clone();
    bad_len[36] = 99;
    let reject = Header::parse(&wire[..10]) == Err(PacketError::TooShort)
        && Header::parse(&bad_magic) == Err(PacketError::BadMagic)
        && Header::parse(&bad_version) == Err(PacketError::BadVersion)
        && Header::parse(&bad_kind) == Err(PacketError::BadKind)
        && Header::parse(&bad_codec) == Err(PacketError::BadCodec)
        && Header::parse(&bad_len) == Err(PacketError::LengthMismatch);
    check("packet_reject", reject);

    // goertzel separates 1k from 2k
    let sine = dsp::gen_sine(1000.0, 48000, 4800, 0.5);
    let p1k = dsp::goertzel_power(&sine, 48000, 1000.0);
    let p2k = dsp::goertzel_power(&sine, 48000, 2000.0);
    check("goertzel_selectivity", p1k > p2k * 100.0);

    // f32 <-> s16le round trip
    let src: Vec<f32> = vec![-1.0, -0.5, -0.25, 0.0, 0.25, 0.5, 0.9999, 1.0];
    let back = dsp::s16le_to_f32(&dsp::f32_to_s16le(&src));
    let rt_ok = back.len() == src.len()
        && src
            .iter()
            .zip(back.iter())
            .all(|(a, b)| (a - b).abs() < 1.0 / 32768.0 + 1e-6)
        && dsp::s16le_to_f32(&dsp::f32_to_s16le(&[1.5]))[0] > 0.999;
    check("s16le_roundtrip", rt_ok);

    // localhost tx/rx session with tone verification
    let e2e = (|| -> Result<bool> {
        let tx_sock = UdpSocket::bind("127.0.0.1:0")?;
        let rx_sock = UdpSocket::bind("127.0.0.1:0")?;
        let rx_addr = rx_sock.local_addr()?;
        let tx_cfg = ToneTxCfg {
            secs: 1.0,
            ..ToneTxCfg::default()
        };
        let handle = std::thread::spawn(move || run_tx_tone(&tx_sock, TxMode::Push(rx_addr), &tx_cfg));
        let rx_cfg = RxCfg {
            secs: 3.0,
            verify_freq: Some(1000.0),
            idle_timeout_ms: 2000,
        };
        let outcome = run_rx(&rx_sock, RxMode::Listen, &rx_cfg, None)?;
        let tx_ok = matches!(handle.join(), Ok(Ok(_)));
        let detected = outcome.verdict.as_ref().map(|v| v.detected).unwrap_or(false);
        Ok(tx_ok && detected && outcome.summary.lost == 0 && !outcome.timed_out)
    })()
    .unwrap_or(false);
    check("local_session_verify", e2e);

    let ok = checks.iter().all(|(_, o)| *o);
    let json_checks: Vec<serde_json::Value> = checks
        .iter()
        .map(|(name, o)| serde_json::json!({"name": name, "ok": o}))
        .collect();
    emit_json(json, &serde_json::json!({"ok": ok, "checks": json_checks}));
    Ok(if ok { 0 } else { EXIT_CHECK_FAILED })
}

#[allow(clippy::too_many_arguments)]
fn cmd_tx(
    to: Option<SocketAddr>,
    serve: bool,
    port: u16,
    source: Source,
    freq: f32,
    amp: f32,
    secs: f32,
    json: bool,
) -> Result<i32> {
    let sock = if serve {
        UdpSocket::bind(("0.0.0.0", port))?
    } else {
        UdpSocket::bind("0.0.0.0:0")?
    };
    let report = match source {
        Source::Tone => {
            let cfg = ToneTxCfg {
                freq_hz: freq,
                amp,
                sample_rate: 48000,
                frame_ms: 10,
                secs,
            };
            match to {
                Some(dest) => {
                    info(&format!("tx tone push -> {dest} for {secs}s"));
                    run_tx_tone(&sock, TxMode::Push(dest), &cfg)?
                }
                None if serve => {
                    info(&format!("tx tone serve on 0.0.0.0:{port}, waiting for PullReq"));
                    run_tx_tone(&sock, TxMode::Serve, &cfg)?
                }
                None => return Err(anyhow!("tx requires --to or --serve")),
            }
        }
        Source::Mic => {
            if to.is_none() && !serve {
                return Err(anyhow!("tx requires --to or --serve"));
            }
            run_tx_mic(&sock, to, serve, port, secs)?
        }
    };
    info(&format!(
        "sent {} packets / {} bytes in {:.2}s",
        report.sent_packets, report.sent_bytes, report.secs
    ));
    emit_json(json, &report);
    Ok(0)
}

fn is_poll_tick(kind: ErrorKind) -> bool {
    // WouldBlock/TimedOut = read-timeout tick; ConnectionReset family = Windows
    // surfacing ICMP unreachable on unconnected UDP sockets — benign here.
    matches!(
        kind,
        ErrorKind::WouldBlock
            | ErrorKind::TimedOut
            | ErrorKind::ConnectionReset
            | ErrorKind::ConnectionRefused
            | ErrorKind::ConnectionAborted
    )
}

fn run_tx_mic(
    sock: &UdpSocket,
    to: Option<SocketAddr>,
    serve: bool,
    port: u16,
    secs: f32,
) -> Result<TxReport> {
    sock.set_read_timeout(Some(Duration::from_millis(100)))?;
    let mut buf = [0u8; 4096];
    let mut subs: Vec<SocketAddr> = Vec::new();
    if let Some(dest) = to {
        info(&format!("tx mic push -> {dest} for {secs}s"));
        subs.push(dest);
    } else if serve {
        info(&format!("tx mic serve on 0.0.0.0:{port}, waiting for PullReq"));
        let wait_deadline = Instant::now() + Duration::from_secs_f32(secs.max(10.0) + 5.0);
        loop {
            if Instant::now() >= wait_deadline {
                info("no PullReq arrived; giving up (self-termination contract)");
                return Ok(TxReport { sent_packets: 0, sent_bytes: 0, secs: 0.0 });
            }
            match sock.recv_from(&mut buf) {
                Ok((n, from)) => {
                    if let Ok((h, _)) = Header::parse(&buf[..n]) {
                        if h.kind == Kind::PullReq {
                            subs.push(from);
                            break;
                        }
                    }
                }
                Err(e) if is_poll_tick(e.kind()) => {}
                Err(e) => return Err(e.into()),
            }
        }
    }

    info("capturing microphone; macOS may show a permission (TCC) prompt");
    let (capture, mut rx, rate) = LiveCapture::start()?;
    let frame_samples = (rate / 100) as usize; // 10ms
    let session_id = {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64
    };

    let start = Instant::now();
    let deadline = start + Duration::from_secs_f32(secs);
    let mut pending: Vec<f32> = Vec::new();
    let mut seq = 0u32;
    let mut sent_packets = 0u64;
    let mut sent_bytes = 0u64;

    while Instant::now() < deadline {
        rx.pop(&mut pending);
        while pending.len() >= frame_samples {
            let frame: Vec<f32> = pending.drain(..frame_samples).collect();
            let payload = dsp::f32_to_s16le(&frame);
            let datagram = Header {
                kind: Kind::Media,
                codec: Codec::PcmS16le,
                channels: 1,
                sample_rate: rate,
                session_id,
                stream_id: 0,
                seq,
                timestamp_us: start.elapsed().as_micros() as u64,
                payload_len: payload.len() as u32,
            }
            .encode(&payload);
            for dest in &subs {
                sock.send_to(&datagram, dest)?;
                sent_packets += 1;
                sent_bytes += datagram.len() as u64;
            }
            seq = seq.wrapping_add(1);
        }
        if serve {
            sock.set_read_timeout(Some(Duration::from_millis(5)))?;
            match sock.recv_from(&mut buf) {
                Ok((n, from)) => {
                    if let Ok((h, _)) = Header::parse(&buf[..n]) {
                        if h.kind == Kind::PullReq && !subs.contains(&from) && subs.len() < 8 {
                            subs.push(from);
                        }
                    }
                }
                Err(e) if is_poll_tick(e.kind()) => {}
                Err(e) => return Err(e.into()),
            }
        } else {
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    let bye = Header {
        kind: Kind::Bye,
        codec: Codec::PcmS16le,
        channels: 1,
        sample_rate: rate,
        session_id,
        stream_id: 0,
        seq,
        timestamp_us: start.elapsed().as_micros() as u64,
        payload_len: 0,
    }
    .encode(&[]);
    for _ in 0..3 {
        for dest in &subs {
            let _ = sock.send_to(&bye, dest);
        }
    }
    drop(capture);

    Ok(TxReport {
        sent_packets,
        sent_bytes,
        secs: start.elapsed().as_secs_f64(),
    })
}

/// Peek (without consuming) until the first Media datagram reveals its sample
/// rate. In pull mode we must solicit the sender ourselves since run_rx has not
/// started yet; run_rx's own PullReq keepalive takes over afterwards.
fn peek_media_rate(
    sock: &UdpSocket,
    pull: Option<SocketAddr>,
    deadline: Duration,
) -> Result<Option<u32>> {
    sock.set_read_timeout(Some(Duration::from_millis(100)))?;
    let start = Instant::now();
    let end = start + deadline;
    let mut next_pull = start;
    let mut buf = [0u8; 4096];
    while Instant::now() < end {
        if let Some(dest) = pull {
            if Instant::now() >= next_pull {
                let req = Header {
                    kind: Kind::PullReq,
                    codec: Codec::PcmS16le,
                    channels: 1,
                    sample_rate: 48000,
                    session_id: 0,
                    stream_id: 0,
                    seq: 0,
                    timestamp_us: 0,
                    payload_len: 0,
                }
                .encode(&[]);
                let _ = sock.send_to(&req, dest);
                next_pull = Instant::now() + Duration::from_secs(1);
            }
        }
        match sock.peek_from(&mut buf) {
            Ok((n, _)) => {
                if let Ok((h, _)) = Header::parse(&buf[..n]) {
                    if h.kind == Kind::Media {
                        return Ok(Some(h.sample_rate));
                    }
                }
                // not media (stray Bye etc.) — consume so peek can move on
                let _ = sock.recv_from(&mut buf);
            }
            Err(e) if is_poll_tick(e.kind()) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(None)
}

fn cmd_rx(
    port: u16,
    pull: Option<SocketAddr>,
    play: bool,
    verify_freq: Option<f32>,
    secs: f32,
    json: bool,
) -> Result<i32> {
    let (sock, mode) = match pull {
        Some(peer) => {
            info(&format!("rx pull from {peer} for {secs}s"));
            (UdpSocket::bind("0.0.0.0:0")?, RxMode::Pull(peer))
        }
        None => {
            info(&format!("rx listen on 0.0.0.0:{port} for {secs}s"));
            (UdpSocket::bind(("0.0.0.0", port))?, RxMode::Listen)
        }
    };
    let cfg = RxCfg {
        secs,
        verify_freq,
        idle_timeout_ms: 5000,
    };

    let mut playback_guard: Option<LivePlayback> = None;
    let on_frame: Option<Box<dyn FnMut(&[f32]) + Send>> = if play {
        // the stream's true rate is whatever the sender captured at — learn it
        // from the first Media header (peeked, so run_rx still counts the packet)
        let rate = peek_media_rate(&sock, pull, Duration::from_millis(5000))?.unwrap_or(48000);
        info(&format!("playback wired at {rate} Hz (from first media packet)"));
        let (guard, mut audio_tx) = LivePlayback::start(rate)?;
        playback_guard = Some(guard);
        Some(Box::new(move |frame: &[f32]| audio_tx.push(frame)))
    } else {
        None
    };

    let outcome = run_rx(&sock, mode, &cfg, on_frame)?;
    drop(playback_guard); // tear down the stream before exiting

    info(&format!(
        "received={} lost={} loss={:.2}% jitter={:.2}ms timed_out={}",
        outcome.summary.received,
        outcome.summary.lost,
        outcome.summary.loss_pct,
        outcome.summary.jitter_ms,
        outcome.timed_out
    ));
    if let Some(v) = &outcome.verdict {
        info(&format!(
            "verify {} Hz: detected={} snr={:.1}dB",
            v.freq_hz, v.detected, v.snr_db
        ));
    }
    emit_json(json, &outcome);

    if outcome.timed_out && outcome.summary.received == 0 {
        return Ok(EXIT_NO_TRAFFIC);
    }
    if verify_freq.is_some() && !outcome.verdict.as_ref().map(|v| v.detected).unwrap_or(false) {
        return Ok(EXIT_CHECK_FAILED);
    }
    Ok(0)
}
