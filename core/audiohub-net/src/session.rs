use std::io::ErrorKind;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use audiohub_core::dsp;

use crate::packet::{Codec, Header, Kind};
use crate::stats::{RxStats, RxSummary};

const POLL: Duration = Duration::from_millis(100);
const MAX_SUBSCRIBERS: usize = 8;
const ACCUM_CAP_SECS: usize = 30;

fn is_poll_tick(kind: ErrorKind) -> bool {
    // Unix reports WouldBlock, Windows reports TimedOut on read timeouts.
    // Windows also latches ICMP Port Unreachable onto unconnected UDP sockets
    // (WSAECONNRESET) — benign for our peer-to-peer flows, treat as a tick.
    matches!(
        kind,
        ErrorKind::WouldBlock
            | ErrorKind::TimedOut
            | ErrorKind::ConnectionReset
            | ErrorKind::ConnectionRefused
            | ErrorKind::ConnectionAborted
    )
}

fn new_session_id() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}

fn now_us(start: Instant) -> u64 {
    start.elapsed().as_micros() as u64
}

#[derive(Debug, Clone)]
pub struct ToneTxCfg {
    pub freq_hz: f32,
    pub amp: f32,
    pub sample_rate: u32,
    pub frame_ms: u32,
    pub secs: f32,
}

impl Default for ToneTxCfg {
    fn default() -> Self {
        ToneTxCfg {
            freq_hz: 1000.0,
            amp: 0.5,
            sample_rate: 48000,
            frame_ms: 10,
            secs: 10.0,
        }
    }
}

#[derive(Debug, serde::Serialize)]
pub struct TxReport {
    pub sent_packets: u64,
    pub sent_bytes: u64,
    pub secs: f64,
}

pub enum TxMode {
    Push(SocketAddr),
    Serve,
}

fn add_subscriber(subs: &mut Vec<SocketAddr>, addr: SocketAddr) {
    if !subs.contains(&addr) && subs.len() < MAX_SUBSCRIBERS {
        subs.push(addr);
    }
}

pub fn run_tx_tone(sock: &UdpSocket, mode: TxMode, cfg: &ToneTxCfg) -> anyhow::Result<TxReport> {
    sock.set_read_timeout(Some(POLL))?;
    let mut buf = [0u8; 4096];

    let serving = matches!(mode, TxMode::Serve);
    let mut subs: Vec<SocketAddr> = Vec::new();
    match mode {
        TxMode::Push(dest) => subs.push(dest),
        TxMode::Serve => {
            // wait for the first valid PullReq, but bounded: the CLI contract
            // (spec §5) is self-termination, so an absent puller must not hang us.
            let wait_deadline =
                Instant::now() + Duration::from_secs_f32(cfg.secs.max(10.0) + 5.0);
            loop {
                if Instant::now() >= wait_deadline {
                    return Ok(TxReport { sent_packets: 0, sent_bytes: 0, secs: 0.0 });
                }
                match sock.recv_from(&mut buf) {
                    Ok((n, from)) => {
                        if let Ok((h, _)) = Header::parse(&buf[..n]) {
                            if h.kind == Kind::PullReq {
                                add_subscriber(&mut subs, from);
                                break;
                            }
                        }
                    }
                    Err(e) if is_poll_tick(e.kind()) => {}
                    Err(e) => return Err(e.into()),
                }
            }
        }
    }

    let session_id = new_session_id();
    let frame_samples = (cfg.sample_rate as u64 * cfg.frame_ms as u64 / 1000) as usize;
    let total_frames = (cfg.secs as f64 * 1000.0 / cfg.frame_ms as f64).round() as u64;
    let total_samples = frame_samples * total_frames as usize;
    let tone = dsp::gen_sine(cfg.freq_hz, cfg.sample_rate, total_samples, cfg.amp);

    let mut sent_packets = 0u64;
    let mut sent_bytes = 0u64;
    let start = Instant::now();

    for n in 0..total_frames {
        let deadline = start + Duration::from_millis(n * cfg.frame_ms as u64);
        // wait until deadline; in Serve mode keep accepting PullReqs while waiting.
        // if behind schedule, fall through and send immediately.
        loop {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let remaining = deadline - now;
            if serving {
                sock.set_read_timeout(Some(remaining.min(POLL).max(Duration::from_millis(1))))?;
                match sock.recv_from(&mut buf) {
                    Ok((cnt, from)) => {
                        if let Ok((h, _)) = Header::parse(&buf[..cnt]) {
                            if h.kind == Kind::PullReq {
                                add_subscriber(&mut subs, from);
                            }
                        }
                    }
                    Err(e) if is_poll_tick(e.kind()) => {}
                    Err(e) => return Err(e.into()),
                }
            } else {
                std::thread::sleep(remaining.min(POLL));
            }
        }

        let base = n as usize * frame_samples;
        // 探针路径**刻意固定在 s16**：它测的是链路本身，不是阶梯。
        // 位深写在这里而不是隐含在函数名里 —— 与包头的 `codec` 是同一件事的
        // 两次声明，写错会在下面的 `Codec::for_depth` 断言上现形。
        const PROBE_DEPTH: dsp::WireDepth = dsp::WireDepth::S16;
        let payload = dsp::encode_pcm(&tone[base..base + frame_samples], PROBE_DEPTH);
        let header = Header {
            kind: Kind::Media,
            codec: Codec::for_depth(PROBE_DEPTH),
            channels: 1,
            sample_rate: cfg.sample_rate,
            session_id,
            stream_id: 0,
            seq: n as u32,
            timestamp_us: now_us(start),
            payload_len: payload.len() as u32,
        };
        let datagram = header.encode(&payload);
        for dest in &subs {
            sock.send_to(&datagram, dest)?;
            sent_packets += 1;
            sent_bytes += datagram.len() as u64;
        }
    }

    let bye = Header {
        kind: Kind::Bye,
        codec: Codec::PcmS16le,
        channels: 1,
        sample_rate: cfg.sample_rate,
        session_id,
        stream_id: 0,
        seq: total_frames as u32,
        timestamp_us: now_us(start),
        payload_len: 0,
    }
    .encode(&[]);
    for _ in 0..3 {
        for dest in &subs {
            let _ = sock.send_to(&bye, dest);
        }
    }

    Ok(TxReport {
        sent_packets,
        sent_bytes,
        secs: start.elapsed().as_secs_f64(),
    })
}

#[derive(Debug, Clone)]
pub struct RxCfg {
    pub secs: f32,
    pub verify_freq: Option<f32>,
    pub idle_timeout_ms: u64,
}

impl Default for RxCfg {
    fn default() -> Self {
        RxCfg {
            secs: 10.0,
            verify_freq: Some(1000.0),
            idle_timeout_ms: 5000,
        }
    }
}

pub enum RxMode {
    Listen,
    Pull(SocketAddr),
}

#[derive(Debug, serde::Serialize)]
pub struct RxOutcome {
    pub summary: RxSummary,
    pub verdict: Option<audiohub_core::dsp::ToneVerdict>,
    pub sample_rate: u32,
    pub channels: u8,
    pub timed_out: bool,
}

pub fn run_rx(
    sock: &UdpSocket,
    mode: RxMode,
    cfg: &RxCfg,
    mut on_frame: Option<Box<dyn FnMut(&[f32]) + Send>>,
) -> anyhow::Result<RxOutcome> {
    sock.set_read_timeout(Some(POLL))?;
    let mut buf = [0u8; 4096];

    let start = Instant::now();
    let deadline = start + Duration::from_secs_f32(cfg.secs);
    let idle = Duration::from_millis(cfg.idle_timeout_ms);

    let pull_dest = match mode {
        RxMode::Pull(addr) => Some(addr),
        RxMode::Listen => None,
    };
    let pull_session = new_session_id();
    let mut pull_seq = 0u32;
    let mut next_pull = start;

    let mut stats = RxStats::new();
    let mut samples: Vec<f32> = Vec::new();
    let mut sample_rate = 48000u32;
    let mut channels = 1u8;
    let mut accum_cap;
    let mut last_activity = start;
    let mut first_arrival_us: Option<u64> = None;
    let mut last_arrival_us: u64 = 0;
    let mut timed_out = false;

    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        if now.duration_since(last_activity) >= idle {
            timed_out = true;
            break;
        }
        if let Some(dest) = pull_dest {
            if now >= next_pull {
                let pull = Header {
                    kind: Kind::PullReq,
                    codec: Codec::PcmS16le,
                    channels: 1,
                    sample_rate: 48000,
                    session_id: pull_session,
                    stream_id: 0,
                    seq: pull_seq,
                    timestamp_us: now_us(start),
                    payload_len: 0,
                }
                .encode(&[]);
                let _ = sock.send_to(&pull, dest);
                pull_seq = pull_seq.wrapping_add(1);
                next_pull = now + Duration::from_secs(1);
            }
        }

        match sock.recv_from(&mut buf) {
            Ok((n, _from)) => {
                let arrival = now_us(start);
                let Ok((h, payload)) = Header::parse(&buf[..n]) else {
                    continue;
                };
                match h.kind {
                    Kind::Media => {
                        last_activity = Instant::now();
                        stats.on_packet(h.seq, h.timestamp_us, arrival, payload.len());
                        if first_arrival_us.is_none() {
                            first_arrival_us = Some(arrival);
                        }
                        last_arrival_us = arrival;
                        sample_rate = h.sample_rate;
                        channels = h.channels;
                        accum_cap = ACCUM_CAP_SECS * sample_rate.max(1) as usize;
                        let frame = dsp::s16le_to_f32(payload);
                        if let Some(cb) = on_frame.as_mut() {
                            cb(&frame);
                        }
                        if samples.len() < accum_cap {
                            let room = accum_cap - samples.len();
                            samples.extend_from_slice(&frame[..frame.len().min(room)]);
                        }
                    }
                    Kind::Bye => break,
                    _ => {}
                }
            }
            Err(e) if is_poll_tick(e.kind()) => {}
            Err(e) => return Err(e.into()),
        }
    }

    let duration_s = match first_arrival_us {
        Some(first) if last_arrival_us > first => (last_arrival_us - first) as f64 / 1e6,
        _ => start.elapsed().as_secs_f64(),
    };
    let summary = stats.summary(duration_s);
    let verdict = cfg
        .verify_freq
        .map(|f| dsp::verify_tone(&samples, sample_rate, f));

    Ok(RxOutcome {
        summary,
        verdict,
        sample_rate,
        channels,
        timed_out,
    })
}
