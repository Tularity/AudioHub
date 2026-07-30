//! spec-m4c §C: reconnect + session recovery.
//!
//! Only a peer THIS daemon has connected out to gets a retry loop — a peer that
//! merely connected to us is its own side's job to re-establish, and retrying
//! toward it would mean dialling an address we were never told to dial.

use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use audiohub_ipc::OpenSessionParams;
use audiohub_net::identity::PeerStore;

use crate::conn::{self, ConnectOrigin};
use crate::{dlog, lk, DaemonInner};

/// Frozen backoff ladder in seconds; the last rung repeats forever (cap 30s).
pub const BACKOFF_S: [f64; 5] = [1.0, 2.0, 5.0, 10.0, 30.0];
/// ±20%: peers that dropped together (a switch reboot) must not retry in
/// lockstep and re-create the thundering herd their reconnect is meant to heal.
pub const JITTER_FRAC: f64 = 0.2;

/// Delay before retry number `attempt` (0 = the first retry after the drop).
pub fn backoff_base_s(attempt: u32) -> f64 {
    BACKOFF_S[(attempt as usize).min(BACKOFF_S.len() - 1)]
}

/// Maps `r` in [0,1] onto [base*(1-JITTER_FRAC), base*(1+JITTER_FRAC)].
pub fn apply_jitter(base: f64, r: f64) -> f64 {
    base * (1.0 - JITTER_FRAC + 2.0 * JITTER_FRAC * r.clamp(0.0, 1.0))
}

pub fn next_delay_s(attempt: u32) -> f64 {
    apply_jitter(backoff_base_s(attempt), rand_unit())
}

fn rand_unit() -> f64 {
    use rand_core::RngCore;
    rand_core::OsRng.next_u32() as f64 / (u32::MAX as f64 + 1.0)
}

/// How often the supervisor looks for a due peer.
const SUPERVISOR_TICK: Duration = Duration::from_millis(200);

pub(crate) struct PeerRecon {
    /// The address override that worked, so a peer whose mDNS/last_addr is
    /// stale still reconnects to where we were actually told to look.
    pub addr: Option<String>,
    pub attempts: u32,
    /// `Some` = a retry is armed. `None` = connected, or never dropped.
    pub next_at: Option<Instant>,
    pub in_flight: bool,
    /// Params of the sessions WE opened, captured at disconnect.
    pub sessions: Vec<OpenSessionParams>,
}

impl PeerRecon {
    fn new(addr: Option<String>) -> PeerRecon {
        PeerRecon {
            addr,
            attempts: 0,
            next_at: None,
            in_flight: false,
            sessions: Vec::new(),
        }
    }
}

/// Records that this daemon connected out to `fp` (and how), which is what
/// makes the peer eligible for the retry loop at all. Returns the recovery plan
/// the cleared retry was holding: this connect took that retry's place, so the
/// caller owes it the same replay — nothing else will ever do it.
#[must_use = "these sessions are only recoverable here; dropping them loses them"]
pub(crate) fn note_outbound(
    inner: &DaemonInner,
    fp: &str,
    addr: Option<&str>,
) -> Vec<OpenSessionParams> {
    let mut m = lk(&inner.recon);
    let e = m
        .entry(fp.to_string())
        .or_insert_with(|| PeerRecon::new(addr.map(str::to_string)));
    if addr.is_some() {
        e.addr = addr.map(str::to_string);
    }
    e.attempts = 0;
    e.next_at = None;
    std::mem::take(&mut e.sessions)
}

/// Arms the retry loop after a drop, on the first backoff rung.
pub(crate) fn arm(inner: &DaemonInner, fp: &str, sessions: Vec<OpenSessionParams>) {
    arm_in(inner, fp, sessions, false)
}

/// Arms with no delay: a newer conn to this peer already took over, so there is
/// nothing to back off from. The supervisor's next tick finds the live conn
/// (connect_peer returns it immediately) and replays the plan on it.
pub(crate) fn arm_now(inner: &DaemonInner, fp: &str, sessions: Vec<OpenSessionParams>) {
    arm_in(inner, fp, sessions, true)
}

/// `sessions` is ADDED to the recovery plan, never substituted for it: each
/// drop contributes the streams that were live on ITS connection and the sets
/// are disjoint (a session belongs to exactly one conn), so a set arriving
/// while a retry is already armed must not be thrown away.
fn arm_in(inner: &DaemonInner, fp: &str, sessions: Vec<OpenSessionParams>, immediate: bool) {
    if inner.shutdown.load(Ordering::SeqCst) {
        return;
    }
    let armed = {
        let mut m = lk(&inner.recon);
        match m.get_mut(fp) {
            // no entry = never connected out to this peer, or disarmed on
            // purpose (explicit disconnect / unpair): not ours to retry
            None => None,
            Some(e) => {
                e.sessions.extend(sessions);
                let n = e.sessions.len();
                match e.next_at {
                    // already retrying: keep the ladder where it is, except
                    // that a takeover has a live conn waiting and must not sit
                    // out a 30s rung
                    Some(t) => {
                        let now = Instant::now();
                        if immediate && now < t {
                            e.next_at = Some(now);
                            Some((0.0, n))
                        } else {
                            None
                        }
                    }
                    None => {
                        e.attempts = 0;
                        let d = if immediate { 0.0 } else { next_delay_s(0) };
                        e.next_at = Some(Instant::now() + Duration::from_secs_f64(d));
                        Some((d, n))
                    }
                }
            }
        }
    };
    if let Some((d, n)) = armed {
        if immediate {
            dlog!("[audiohubd] peer {fp}: a newer control channel took over; re-opening {n} session(s) on it");
        } else {
            dlog!(
                "[audiohubd] peer {fp}: control channel lost, reconnecting in {d:.1}s \
                 ({n} session(s) to recover)"
            );
        }
    }
}

/// Stops retrying `fp` for good (explicit disconnect / unpair).
pub(crate) fn disarm(inner: &DaemonInner, fp: &str) {
    lk(&inner.recon).remove(fp);
}

/// `(reconnecting, retry_in_s)` per fingerprint, for `peers.list`.
pub(crate) fn snapshot(inner: &DaemonInner) -> HashMap<String, (bool, Option<f64>)> {
    let now = Instant::now();
    lk(&inner.recon)
        .iter()
        .map(|(fp, e)| {
            let retry_in = e.next_at.map(|t| t.saturating_duration_since(now).as_secs_f64());
            (fp.clone(), (e.next_at.is_some(), retry_in))
        })
        .collect()
}

pub(crate) fn supervisor_loop(inner: Arc<DaemonInner>) {
    loop {
        if inner.shutdown.load(Ordering::SeqCst) {
            return;
        }
        std::thread::sleep(SUPERVISOR_TICK);
        if inner.shutdown.load(Ordering::SeqCst) {
            return;
        }
        let due: Vec<String> = {
            let mut m = lk(&inner.recon);
            let now = Instant::now();
            let mut v = Vec::new();
            for (fp, e) in m.iter_mut() {
                if e.in_flight || e.next_at.map_or(true, |t| now < t) {
                    continue;
                }
                e.in_flight = true;
                v.push(fp.clone());
            }
            v
        };
        // one thread per due peer: a connect blocks for up to the TCP connect +
        // handshake timeout, and one unreachable peer must not delay the others
        for fp in due {
            let i = inner.clone();
            let f = fp.clone();
            let spawned = std::thread::Builder::new()
                .name("ahb-retry".into())
                .spawn(move || {
                    if std::panic::catch_unwind(AssertUnwindSafe(|| attempt(&i, &f))).is_err() {
                        dlog!("[audiohubd] peer {f}: reconnect attempt panicked");
                    }
                    if let Some(e) = lk(&i.recon).get_mut(&f) {
                        e.in_flight = false;
                    }
                });
            if spawned.is_err() {
                if let Some(e) = lk(&inner.recon).get_mut(&fp) {
                    e.in_flight = false;
                }
            }
        }
    }
}

/// Re-opens the sessions captured at the drop. Shared by the retry loop and by
/// a user-initiated connect that took an armed retry's place.
fn replay_sessions(inner: &Arc<DaemonInner>, fp: &str, sessions: Vec<OpenSessionParams>) {
    if sessions.is_empty() {
        return;
    }
    dlog!(
        "[audiohubd] peer {fp}: reconnected; recovering {} session(s)",
        sessions.len()
    );
    for p in sessions {
        if inner.shutdown.load(Ordering::SeqCst) {
            return;
        }
        // open_session mints a FRESH stream id and a FRESH media salt;
        // OpenSessionParams carries neither, so a replay cannot re-use the old
        // pair and re-create the AEAD nonce reuse defect.
        match conn::open_session(inner, &p) {
            Ok(info) => dlog!(
                "[audiohubd] peer {fp}: recovered {} session as stream {}",
                p.kind,
                info.id
            ),
            Err(e) => dlog!(
                "[audiohubd] peer {fp}: could not recover the {} session: {e:#}",
                p.kind
            ),
        }
    }
}

/// Replays off-thread: the caller is `connect_peer`, and a session open dials
/// the same peer back plus waits on an ack — doing that inline would re-enter
/// the connect path and stall an IPC request for as long as the opens take.
pub(crate) fn spawn_replay(inner: &Arc<DaemonInner>, fp: &str, sessions: Vec<OpenSessionParams>) {
    if sessions.is_empty() || inner.shutdown.load(Ordering::SeqCst) {
        return;
    }
    let i = inner.clone();
    let f = fp.to_string();
    let spawned = std::thread::Builder::new()
        .name("ahb-replay".into())
        .spawn(move || {
            if std::panic::catch_unwind(AssertUnwindSafe(|| replay_sessions(&i, &f, sessions)))
                .is_err()
            {
                dlog!("[audiohubd] peer {f}: session recovery panicked");
            }
        });
    if spawned.is_err() {
        dlog!("[audiohubd] peer {fp}: could not spawn the session recovery thread");
    }
}

fn attempt(inner: &Arc<DaemonInner>, fp: &str) {
    if inner.shutdown.load(Ordering::SeqCst) {
        return;
    }
    // An unpaired peer must not be dialled forever: the store is the authority
    // on who we are allowed to talk to, whoever removed the entry.
    if let Ok(store) = PeerStore::load_at(Some(&inner.cfg_dir)) {
        if !store.list().iter().any(|p| p.fingerprint == fp) {
            disarm(inner, fp);
            dlog!("[audiohubd] peer {fp} is no longer paired; reconnect stopped");
            return;
        }
    }
    let addr = lk(&inner.recon).get(fp).and_then(|e| e.addr.clone());
    match conn::connect_peer(inner, fp, addr.as_deref(), ConnectOrigin::Retry) {
        Ok(_) => {
            let sessions = {
                let mut m = lk(&inner.recon);
                match m.get_mut(fp) {
                    Some(e) => {
                        e.attempts = 0;
                        e.next_at = None;
                        std::mem::take(&mut e.sessions)
                    }
                    // disarmed while the connect was in flight (an explicit
                    // disconnect): drop what we just built, recover nothing
                    None => {
                        conn::drop_conn(inner, fp, "disconnected while reconnecting");
                        return;
                    }
                }
            };
            replay_sessions(inner, fp, sessions);
        }
        Err(e) => {
            let next = {
                let mut m = lk(&inner.recon);
                match m.get_mut(fp) {
                    Some(ent) if ent.next_at.is_some() => {
                        ent.attempts = ent.attempts.saturating_add(1);
                        let d = next_delay_s(ent.attempts);
                        ent.next_at = Some(Instant::now() + Duration::from_secs_f64(d));
                        Some((ent.attempts, d))
                    }
                    _ => None, // disarmed mid-attempt: stay stopped
                }
            };
            if let Some((n, d)) = next {
                dlog!("[audiohubd] peer {fp}: reconnect attempt {n} failed ({e:#}); retry in {d:.1}s");
            }
        }
    }
}
