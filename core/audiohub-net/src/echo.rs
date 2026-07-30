use std::io::ErrorKind;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use crate::packet::{Codec, Header, Kind};

const POLL: Duration = Duration::from_millis(100);

fn is_poll_tick(kind: ErrorKind) -> bool {
    // see session.rs: WouldBlock/TimedOut are poll ticks; the ConnectionReset
    // family is Windows surfacing ICMP unreachable on unconnected UDP — benign.
    matches!(
        kind,
        ErrorKind::WouldBlock
            | ErrorKind::TimedOut
            | ErrorKind::ConnectionReset
            | ErrorKind::ConnectionRefused
            | ErrorKind::ConnectionAborted
    )
}

pub fn run_echo_server_for(sock: &UdpSocket, secs: f32) -> std::io::Result<u64> {
    sock.set_read_timeout(Some(POLL))?;
    let deadline = Instant::now() + Duration::from_secs_f32(secs);
    let mut buf = [0u8; 65536];
    let mut handled = 0u64;
    while Instant::now() < deadline {
        match sock.recv_from(&mut buf) {
            Ok((n, from)) => {
                if let Ok((h, _)) = Header::parse(&buf[..n]) {
                    if h.kind == Kind::EchoReq {
                        buf[5] = Kind::EchoResp as u8; // flip kind, echo everything else verbatim
                        sock.send_to(&buf[..n], from)?;
                        handled += 1;
                    }
                }
            }
            Err(e) if is_poll_tick(e.kind()) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(handled)
}

#[derive(Debug, Clone)]
pub struct EchoCfg {
    pub count: u32,
    pub interval_ms: u64,
    pub size: usize,
    pub timeout_ms: u64,
}

impl Default for EchoCfg {
    fn default() -> Self {
        EchoCfg {
            count: 200,
            interval_ms: 10,
            size: 960,
            timeout_ms: 1000,
        }
    }
}

#[derive(Debug, serde::Serialize)]
pub struct EchoSummary {
    pub sent: u32,
    pub received: u32,
    pub loss_pct: f64,
    pub rtt_min_ms: f64,
    pub rtt_avg_ms: f64,
    pub rtt_p50_ms: f64,
    pub rtt_p95_ms: f64,
    pub rtt_max_ms: f64,
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() as f64 * p).ceil() as usize).saturating_sub(1);
    sorted[idx.min(sorted.len() - 1)]
}

pub fn run_echo_client(
    sock: &UdpSocket,
    dest: SocketAddr,
    cfg: &EchoCfg,
) -> std::io::Result<EchoSummary> {
    sock.set_read_timeout(Some(POLL))?;
    let session_id = {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64
    };
    let start = Instant::now();
    let payload_len = cfg.size.max(8);
    let mut buf = [0u8; 65536];
    let mut rtts: Vec<f64> = Vec::new();
    let mut sent = 0u32;

    for i in 0..cfg.count {
        let send_slot = start + Duration::from_millis(i as u64 * cfg.interval_ms);
        let now = Instant::now();
        if now < send_slot {
            std::thread::sleep(send_slot - now);
        }

        let send_us = start.elapsed().as_micros() as u64;
        let mut payload = vec![0u8; payload_len];
        payload[..8].copy_from_slice(&send_us.to_le_bytes());
        let datagram = Header {
            kind: Kind::EchoReq,
            codec: Codec::PcmS16le,
            channels: 0,
            sample_rate: 0,
            session_id,
            stream_id: 0,
            seq: i,
            timestamp_us: send_us,
            payload_len: payload.len() as u32,
        }
        .encode(&payload);
        sock.send_to(&datagram, dest)?;
        sent += 1;

        let wait_deadline = Instant::now() + Duration::from_millis(cfg.timeout_ms);
        loop {
            let now = Instant::now();
            if now >= wait_deadline {
                break; // lost
            }
            let remaining = wait_deadline - now;
            sock.set_read_timeout(Some(remaining.min(POLL)))?;
            match sock.recv_from(&mut buf) {
                Ok((n, _)) => {
                    if let Ok((h, pl)) = Header::parse(&buf[..n]) {
                        if h.kind == Kind::EchoResp
                            && h.session_id == session_id
                            && h.seq == i
                            && pl.len() >= 8
                        {
                            let echoed_us = u64::from_le_bytes(pl[..8].try_into().unwrap());
                            let rtt_us = (start.elapsed().as_micros() as u64).saturating_sub(echoed_us);
                            rtts.push(rtt_us as f64 / 1000.0);
                            break;
                        }
                        // stale response for an earlier seq: keep waiting
                    }
                }
                Err(e) if is_poll_tick(e.kind()) => {}
                Err(e) => return Err(e),
            }
        }
    }

    let received = rtts.len() as u32;
    let loss_pct = if sent > 0 {
        (sent - received) as f64 * 100.0 / sent as f64
    } else {
        0.0
    };
    rtts.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let (min, max, avg) = if rtts.is_empty() {
        (0.0, 0.0, 0.0)
    } else {
        (
            rtts[0],
            rtts[rtts.len() - 1],
            rtts.iter().sum::<f64>() / rtts.len() as f64,
        )
    };
    Ok(EchoSummary {
        sent,
        received,
        loss_pct,
        rtt_min_ms: min,
        rtt_avg_ms: avg,
        rtt_p50_ms: percentile(&rtts, 0.50),
        rtt_p95_ms: percentile(&rtts, 0.95),
        rtt_max_ms: max,
    })
}
