//! Automatic tier 0 → tier 1 downgrade: noticing that UDP does not get through
//! this link, and moving the peer's media onto TCP without being asked.
//!
//! `plan.md` §9 (M8 acceptance) and §16.2 require the 0 → 1 step to be
//! **automatic**; only the step to tier 2 is manual, because tier 2's premise
//! ("this tunnel only forwards at layer 7, and only one side can originate")
//! is a property of the user's network that produces exactly the same
//! observation as a switched-off machine. UDP silence has no such twin, so it
//! can be observed — and this module is that observation.
//!
//! # Why nothing is probed before the first stream
//!
//! The obvious design is to test UDP during connection setup. It is also the
//! one thing the requirement forbids: a probe costs its own timeout on **every
//! first connection**, including the overwhelming majority where UDP works
//! perfectly, so the price of handling the rare link is paid by every link.
//!
//! And the probe would be nearly redundant. A completed control handshake has
//! already proved TCP reaches this peer; the only open question afterwards is
//! whether UDP does too. So media opens on UDP exactly as it always has (design
//! §5.1: first connection pays nothing), and the answer is read off the media
//! itself.
//!
//! **The cost, stated plainly**: on a link where UDP really is blocked, the
//! user hears roughly 0.6 s of silence before the sound starts. On every other
//! link, nothing.
//!
//! # The two signals, and why it takes two
//!
//! | | Signal | Who can see it |
//! |---|---|---|
//! | 1 | A receiving stream that has had **no media datagram at all** for [`T_UDP_SILENCE`] | the receiver — *the only party who can see "nothing arrived"* |
//! | 2 | A sending stream that has never been answered by a single `PullReq` keepalive | the sender, locally |
//!
//! Signal 1 is the primary one and it has to be evaluated on the receiving
//! side, because that is the only side with the evidence. Inferring it on the
//! sending side from missing `Stats` costs a whole statistics interval and
//! cannot distinguish "no media arrived" from "the reply was lost".
//!
//! Signal 2 is free — the keepalive already exists so the sender can learn the
//! peer's port (`engine::handle_datagram`'s `PullReq` arm) — and it covers the
//! case signal 1 misses on this machine: **UDP blocked in one direction only**.
//! A machine that is purely sending has no receiving stream to run signal 1 on,
//! and would otherwise sit there transmitting into a hole.
//!
//! # The clause that stops this from misfiring: the control channel must be
//! # healthy
//!
//! "No media has arrived" on its own also describes a peer that simply has not
//! started sending yet, and a peer whose network died a moment ago. Both would
//! be downgraded by a naive detector — the second one permanently, since the
//! verdict is persisted.
//!
//! So a verdict additionally requires the control channel to be healthy, which
//! is two facts and not one:
//!
//!  - a **complete control frame received after the stream armed**
//!    ([`crate::ConnShared::heard_since_ms`]) — a positive statement, not the
//!    absence of a negative one: the peer was alive *after* it undertook to
//!    send media;
//!  - and the channel **still delivering now**
//!    ([`crate::ConnShared::silent_for`] under [`conn::CONTROL_SILENCE_LIMIT`]).
//!    The first fact never expires — `last_rx_ms` only moves forward — so on its
//!    own it becomes permanently true about a second in and stops describing the
//!    present. The conclusion being drawn is "TCP gets through this link and UDP
//!    does not", and that comparison exists only while TCP is currently getting
//!    through.
//!
//! The 1 Hz `Ping`/`Pong` in `conn::ping_and_reap` satisfies both within about a
//! second even when the peer is otherwise quiet, so the clause costs at most one
//! extra detection cycle and buys immunity to the failure mode that would
//! otherwise pin a healthy peer to TCP for an hour after a transient blip.
//!
//! # The other half of the same discipline lives in `conn::open_session_from`
//!
//! Signal 1 arms on the **receiving** side the moment `OpenStream` arrives, so
//! the sender must have its media source running *before* that message goes out
//! — otherwise the receiver's clock starts while the sender is still opening a
//! device, and a slow source (a macOS process tap builds an aggregate device and
//! may wait on TCC) is indistinguishable from a blocked link.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use audiohub_net::secure::SessionMsg;

use crate::peer_transport::TransportTier;
use crate::tcpmedia::MediaPath;
use crate::{conn, dlog, lk, snapshot_sessions, DaemonInner};

/// How long a receiving stream may see zero media datagrams before the link is
/// declared UDP-hostile (design §5.1, `T_udp`).
///
/// 600 ms is about sixty frames at the 10 ms send cadence — three orders of
/// magnitude more than a link needs to deliver one. It is not a tuning knob
/// balancing false positives against speed; the distribution is not continuous.
/// Either packets are arriving at 100 Hz or none are.
pub(crate) const T_UDP_SILENCE: Duration = Duration::from_millis(600);

/// How long a sending stream may go without a single keepalive before the
/// reverse direction is declared blocked.
///
/// Five times [`T_UDP_SILENCE`], and the asymmetry is deliberate. Keepalives
/// are emitted by the peer's **1 Hz** ticker, so the first one is due up to a
/// second after its receiving stream exists, and that clock is not aligned with
/// ours. A budget in the same range as signal 1's would be reporting on the
/// phase of the peer's ticker rather than on the network.
pub(crate) const T_KEEPALIVE_SILENCE: Duration = Duration::from_millis(3000);

/// How long a stored verdict stands before the next connection re-probes UDP.
///
/// design §5.1 asks for a low-frequency re-probe, and this is it — the probe
/// is simply "start on tier 0 again and let the detector have another go",
/// which costs 600 ms of silence at most and only on a link that is still
/// broken. Retiring happens **between** connections, never inside a live one:
/// promotion back to tier 0 mid-stream would need a new jitter buffer, a new
/// destination and a story for reordering across two transports, to save one
/// stream open.
pub(crate) const AUTO_TIER_RETRY_SECS: u64 = 3600;

/// Signal 1's reason string, as shown to the user on both machines.
///
/// Says which direction failed, because the tier is per peer while the
/// detection is not (design §5.2): the asymmetry that a downgrade throws away
/// is preserved here and nowhere else.
const REASON_NO_INBOUND: &str = "no inbound UDP media on this link";

/// Signal 2's reason string.
const REASON_NO_KEEPALIVE: &str = "the peer's UDP keepalives never arrived";

/// What a peer told us about its own half of the link.
const REASON_PEER_REPORTED: &str = "the peer reported that UDP does not get through";

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Everything one live stream has to say about whether UDP works on this link,
/// sampled off the atomics in one place.
///
/// It exists so the decision below is a pure function. The clauses that make
/// this feature safe are all *refusals* — "not yet", "not evidence", "the
/// control channel is not healthy" — and a refusal is invisible to an
/// end-to-end test: a build with the refusal deleted still downgrades the
/// blocked link, still carries the tone over TCP, still writes the right words
/// into the store. The only way to hold a refusal down is to be able to ask for
/// it directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Evidence {
    /// This stream's media is currently on UDP. A tier 1 or tier 2 stream sees
    /// no datagrams and gets no keepalives *by design*
    /// (`engine::send_pullreq` returns early when the path has no address), so
    /// reading either as a verdict would re-downgrade an already downgraded
    /// link for ever.
    pub on_udp: bool,
    /// The peer's EFFECTIVE tier is still 0 — i.e. neither already downgraded
    /// nor pinned somewhere that makes the question moot.
    pub tier0: bool,
    /// How long since this stream armed, on the connection's own clock.
    pub age_ms: u64,
    /// A complete control frame arrived AFTER this stream armed.
    pub heard_since_arm: bool,
    /// How long the control channel has been silent, as of right now.
    pub control_silent_ms: u64,
    /// Datagrams received on this stream, when it receives at all.
    pub media_seen: Option<u64>,
    /// `(packets sent, keepalives answered)`, when this stream sends at all.
    pub sent_and_ka: Option<(u64, u64)>,
}

/// What [`look_at`] concluded about one stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Look {
    /// Nothing to conclude — not yet, or not from this stream at all.
    Quiet,
    /// UDP does not get through. The payload is the reason string shown on both
    /// machines.
    Blocked(&'static str),
}

/// The whole automatic-downgrade decision for one stream, with no I/O in it.
///
/// # The health clause, and why it is two facts
///
/// "No media has arrived" also describes a peer that has not started sending
/// yet, and a peer whose network died a moment ago. Both would be downgraded by
/// a naive detector — the second one for an hour, since the verdict persists.
/// So a verdict additionally requires the control channel to be healthy:
///
///  - [`Evidence::heard_since_arm`]: the peer was alive *after* it undertook to
///    send media. A positive statement, not the absence of a negative one. It
///    is a fact about the past and never expires.
///  - [`Evidence::control_silent_ms`] under [`conn::CONTROL_SILENCE_LIMIT`]: the
///    channel is *still* delivering. The first fact alone becomes permanently
///    true about a second in — `last_rx_ms` only moves forward — and stops
///    describing the present, while the conclusion being drawn ("TCP gets
///    through this link and UDP does not") exists only while TCP currently is.
///
/// The bound is `CONTROL_SILENCE_LIMIT` and deliberately not tighter: it is the
/// exact silence at which `conn::ping_and_reap` declares the channel dead, so
/// this clause can never refuse a verdict on a connection the daemon still
/// considers healthy. What it buys is the window between crossing that line and
/// the 1 Hz ticker noticing.
pub(crate) fn look_at(e: Evidence) -> Look {
    if !e.on_udp || !e.tier0 {
        return Look::Quiet;
    }
    if !e.heard_since_arm || e.control_silent_ms >= conn::CONTROL_SILENCE_LIMIT.as_millis() as u64 {
        return Look::Quiet;
    }
    if let Some(seen) = e.media_seen {
        if e.age_ms >= T_UDP_SILENCE.as_millis() as u64 && seen == 0 {
            return Look::Blocked(REASON_NO_INBOUND);
        }
    }
    if let Some((sent, ka)) = e.sent_and_ka {
        // `sent > 0` is load-bearing: with no packets sent there is nothing for
        // a keepalive to be a reply to, and the absence of one says nothing
        // about the network. Same discipline the health clause applies to the
        // control channel — require evidence that the other side had something
        // to respond to.
        if e.age_ms >= T_KEEPALIVE_SILENCE.as_millis() as u64 && sent > 0 && ka == 0 {
            return Look::Blocked(REASON_NO_KEEPALIVE);
        }
    }
    Look::Quiet
}

/// One watchdog sweep. Called from the ticker's 200 ms sub-tick.
///
/// # Why the sub-tick and not the 1 s tick
///
/// [`T_UDP_SILENCE`] is 600 ms, and a detector sampled once a second cannot
/// resolve it: the verdict would land anywhere between 0.6 s and 1.6 s, so the
/// silence the user hears would be up to three times what the design costed
/// out. On the 200 ms cadence the quantisation is bounded at 800 ms.
///
/// The sweep is cheap enough to belong there: one snapshot of the session
/// table, then per session two atomic loads and a mutex-free path check. It
/// allocates only when a verdict is actually reached.
pub(crate) fn watchdog_pass(inner: &Arc<DaemonInner>) {
    if inner.shutdown.load(Ordering::SeqCst) {
        return;
    }
    // At most one verdict per peer per sweep, even with sixteen streams on it.
    // Acting is "persist, tell the peer, drop the control connection"; doing
    // that once per stream would tear the connection down under its own
    // replay.
    let mut verdicts: HashMap<String, &'static str> = HashMap::new();
    for e in snapshot_sessions(inner) {
        if verdicts.contains_key(&e.conn.fp) || !e.conn.alive.load(Ordering::SeqCst) {
            continue;
        }
        let ev = Evidence {
            on_udp: matches!(e.conn.current_media_path(), MediaPath::Udp(_)),
            // The EFFECTIVE tier: a peer under AUTO that was downgraded ten
            // minutes ago reads as tier 1 here and is skipped, which is what
            // stops the sweep re-announcing a verdict it already reached.
            tier0: lk(&inner.peer_transport).effective_tier(&e.conn.fp) == TransportTier::Tier0,
            age_ms: e.conn.clock_ms().saturating_sub(e.armed_conn_ms),
            heard_since_arm: e.conn.heard_since_ms(e.armed_conn_ms),
            control_silent_ms: e.conn.silent_for().as_millis() as u64,
            media_seen: e
                .rx
                .as_ref()
                .map(|rx| rx.media_seen.load(Ordering::Relaxed)),
            sent_and_ka: e.tx.as_ref().map(|tx| {
                (
                    tx.sent_packets.load(Ordering::Relaxed),
                    tx.ka_count.load(Ordering::Relaxed),
                )
            }),
        };
        if let Look::Blocked(reason) = look_at(ev) {
            verdicts.insert(e.conn.fp.clone(), reason);
        }
    }
    for (fp, reason) in verdicts {
        downgrade(inner, &fp, reason, true);
    }
}

/// Record a tier 1 verdict for `fp` and, if it changed anything, act on it.
///
/// `announce` is false when the verdict *came from* the peer, so a
/// `TransportSwitch` cannot bounce back and forth between two daemons that both
/// think they are informing the other.
fn downgrade(inner: &Arc<DaemonInner>, fp: &str, reason: &str, announce: bool) {
    let changed = {
        let mut store = lk(&inner.peer_transport);
        let changed = store.note_auto_tier(fp, TransportTier::Tier1, reason, now_unix());
        if changed {
            // Persisted before anything acts on it, for the reason
            // `peers.set_tier` gives at its own save: the next step drops the
            // connection, and a reconnect that beat the write would come back
            // on the old tier while every surface said otherwise.
            if let Err(e) = store.save(&inner.cfg_dir) {
                dlog!("[audiohubd] could not persist the tier 1 verdict for {fp}: {e:#}");
            }
        }
        changed
    };
    if !changed {
        // Already recorded. Silence here is what keeps a peer pinned to tier 0
        // — which keeps re-detecting, correctly, on every sweep — from emitting
        // a `TransportSwitch` five times a second forever.
        return;
    }
    let user_tier = lk(&inner.peer_transport).tier(fp);
    dlog!(
        "[audiohubd] peer {fp}: {reason}; recording tier 1 (the user's setting is \
         `{}` and is not being changed)",
        user_tier.as_wire()
    );
    let conn = lk(&inner.state).conns.get(fp).cloned();
    if user_tier != TransportTier::Auto {
        // The user pinned this peer. The observation is still recorded and
        // still shown — "your link has no UDP and you told me not to fall
        // back" is a far better thing for the interface to be able to say than
        // silence — but acting on it is exactly what the pin forbids.
        //
        // Dropping the connection here would be worse than useless. Applying a
        // verdict means tearing the control channel down so a reconnect can
        // renegotiate — and against a pin the reconnect comes back on the same
        // tier it left. The cost is a real teardown plus a session replay, and
        // the benefit is nothing.
        //
        // It would not loop: the `changed` guard above makes it fire exactly
        // once. That is worth stating precisely, because "it would loop" is the
        // wrong reason and an earlier version of the test that guards this line
        // was written against it — it watched for a loop, and passed happily
        // while a wrong build dropped the connection once. The test now
        // compares `ConnShared` identity instead.
        //
        // Nothing is announced either, and the ordering is the point: a pinned
        // machine that told the peer 「switch to tier 1」 would be advertising a
        // move it then refuses to take part in — `tcpmedia::on_request` turns
        // the peer's attach away with "this peer is pinned to tier 0", after the
        // peer has already paid a control-channel teardown and a session replay
        // to get there.
        //
        // The peer does lose the information, and that is the right trade: the
        // only thing it could do with it is a downgrade this machine will not
        // accept. A pin is a statement about what this link may do, and it binds
        // what we ask of the peer exactly as much as what we do ourselves.
        return;
    }
    if announce {
        if let Some(c) = &conn {
            // Best effort. If it does not arrive, the peer's own detector
            // reaches the same verdict from its own half of the link; the
            // message only saves it a detection cycle.
            let _ = c.send_msg(&SessionMsg::TransportSwitch {
                tier: TransportTier::Tier1.as_wire().to_string(),
                reason: reason.to_string(),
            });
        }
    }
    // Same route `peers.set_tier` takes, and for the same reason it takes it:
    // media never changes transport inside a live stream (design §5.1), so
    // every stream has to be re-opened regardless, and `teardown_conn` already
    // arms the retry loop with exactly those streams.
    let applied = conn::retier(inner, fp);
    dlog!(
        "[audiohubd] peer {fp}: automatic downgrade to tier 1 applied ({})",
        applied.as_wire()
    );
}

/// A peer told us it has decided this link cannot carry UDP.
///
/// **Evidence, not an instruction.** It lands in the same field our own
/// detector writes, goes through the same precedence rule against the user's
/// setting, and is refused outright unless it names tier 1 — the only tier
/// reachable without a human (plan §16.2).
pub(crate) fn on_peer_switch(inner: &Arc<DaemonInner>, fp: &str, tier: &str, reason: &str) {
    if TransportTier::parse(tier) != Some(TransportTier::Tier1) {
        // Named rather than ignored: a peer asking for tier 2 is either a
        // version we do not understand or a peer trying to move our media onto
        // a transport nobody selected, and both are worth being able to find.
        dlog!(
            "[audiohubd] peer {fp} asked us to switch to `{tier}`; only tier 1 is reachable \
             without a human, ignoring"
        );
        return;
    }
    let why = if reason.is_empty() {
        REASON_PEER_REPORTED.to_string()
    } else {
        format!("{REASON_PEER_REPORTED} ({reason})")
    };
    downgrade(inner, fp, &why, false);
}

/// Retire a verdict older than [`AUTO_TIER_RETRY_SECS`], so this connection
/// starts on tier 0 and the detector gets to look again.
///
/// Called once per connection, from `tcpmedia::negotiate`, which is the single
/// point where a connection decides which transport to bring up. Putting it
/// there rather than on a timer is what makes "promotion happens at the next
/// stream open, never inside a live one" true by construction instead of by
/// convention.
pub(crate) fn retire_stale_verdict(inner: &Arc<DaemonInner>, fp: &str) {
    let now = now_unix();
    let stale = {
        let store = lk(&inner.peer_transport);
        let t = store.get(fp);
        match (t.auto_tier(), t.auto_tier_since) {
            (Some(_), Some(since)) => {
                // `since > now` means the clock moved backwards under us (an
                // NTP step, a VM restored from a snapshot). The age is then
                // unknowable, and an unknowable age is retired: the cost of
                // retiring early is 600 ms of silence once, the cost of keeping
                // it is a peer stuck on TCP until the timestamp catches up.
                since > now || now - since >= AUTO_TIER_RETRY_SECS
            }
            // A verdict with no timestamp cannot be aged out at all, so it is
            // retired on sight rather than allowed to become permanent.
            (Some(_), None) => true,
            _ => false,
        }
    };
    if !stale {
        return;
    }
    let mut store = lk(&inner.peer_transport);
    if store.clear_auto_tier(fp) {
        dlog!("[audiohubd] peer {fp}: retiring the stored tier 1 verdict; re-probing UDP");
        if let Err(e) = store.save(&inner.cfg_dir) {
            dlog!("[audiohubd] could not persist the retired tier 1 verdict for {fp}: {e:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    //! Every clause in [`look_at`], asked for directly.
    //!
    //! The end-to-end tests in `transport_tests` prove the feature fires. They
    //! cannot prove it *refuses*: a build with any of the guards below deleted
    //! still downgrades a blocked link, still carries the tone over TCP and
    //! still writes the right words into the store, so it passes all of them.
    //! Two of these clauses shipped with no coverage at all for exactly that
    //! reason, and one of them was the health clause — the single thing standing
    //! between a transient blip and an hour on TCP.

    use super::*;

    /// A link that really is UDP-hostile, seen from the receiving side: armed
    /// long enough, control channel answering, not one datagram.
    fn blocked_receiver() -> Evidence {
        Evidence {
            on_udp: true,
            tier0: true,
            age_ms: 1_000,
            heard_since_arm: true,
            control_silent_ms: 100,
            media_seen: Some(0),
            sent_and_ka: None,
        }
    }

    /// The same, seen from a pure sender: packets going out, no keepalive back.
    fn blocked_sender() -> Evidence {
        Evidence {
            age_ms: 4_000,
            media_seen: None,
            sent_and_ka: Some((400, 0)),
            ..blocked_receiver()
        }
    }

    #[test]
    fn the_two_signals_each_reach_a_verdict_on_their_own() {
        assert_eq!(
            look_at(blocked_receiver()),
            Look::Blocked(REASON_NO_INBOUND)
        );
        assert_eq!(
            look_at(blocked_sender()),
            Look::Blocked(REASON_NO_KEEPALIVE)
        );
    }

    /// The peer was never heard from after this stream armed, so "no media
    /// arrived" is indistinguishable from "it has not started sending yet".
    ///
    /// This is the clause that costs an hour on TCP when it is wrong, and it
    /// had no test at all: short-circuiting it left the whole
    /// `transport_tests::` suite green.
    #[test]
    fn a_peer_that_has_not_spoken_since_the_stream_armed_is_not_evidence() {
        let e = Evidence {
            heard_since_arm: false,
            ..blocked_receiver()
        };
        assert_eq!(
            look_at(e),
            Look::Quiet,
            "a peer that may simply not have started sending yet was declared UDP-hostile, and \
             the verdict persists for an hour"
        );
        assert_eq!(
            look_at(Evidence {
                heard_since_arm: false,
                ..blocked_sender()
            }),
            Look::Quiet,
            "the health clause gates BOTH signals; it is not a 'not yet' for one and a verdict \
             for the other"
        );
    }

    /// ...and having been heard from once is not the same as being healthy now.
    /// `last_rx_ms` only moves forward, so `heard_since_arm` is permanently true
    /// about a second after the stream arms. plan §16.2 asks for a control
    /// channel that is 「健康」, present tense: the conclusion "TCP gets through
    /// and UDP does not" is only available while TCP currently does.
    #[test]
    fn a_control_channel_that_has_gone_quiet_is_not_healthy_any_more() {
        let dead = conn::CONTROL_SILENCE_LIMIT.as_millis() as u64;
        assert_eq!(
            look_at(Evidence {
                control_silent_ms: dead,
                ..blocked_receiver()
            }),
            Look::Quiet,
            "the peer's whole network had just died and the link was blamed on UDP"
        );
        assert_eq!(
            look_at(Evidence {
                control_silent_ms: dead,
                ..blocked_sender()
            }),
            Look::Quiet
        );
        // One millisecond inside the limit is still healthy: the bound matches
        // `conn::ping_and_reap` exactly, so this clause never refuses a verdict
        // on a connection the daemon still considers alive.
        assert_eq!(
            look_at(Evidence {
                control_silent_ms: dead - 1,
                ..blocked_receiver()
            }),
            Look::Blocked(REASON_NO_INBOUND)
        );
    }

    /// Media that is not on UDP says nothing about UDP. A tier 1 or tier 2
    /// stream sees no datagrams and gets no keepalives by design, so reading
    /// either as a verdict re-downgrades an already downgraded link for ever.
    #[test]
    fn a_stream_that_left_udp_is_not_evidence_about_udp() {
        assert_eq!(
            look_at(Evidence {
                on_udp: false,
                ..blocked_receiver()
            }),
            Look::Quiet
        );
        assert_eq!(
            look_at(Evidence {
                on_udp: false,
                ..blocked_sender()
            }),
            Look::Quiet
        );
        // Same for a peer already at tier 1 by any route: that is what stops the
        // sweep re-announcing a verdict it reached ten minutes ago.
        assert_eq!(
            look_at(Evidence {
                tier0: false,
                ..blocked_receiver()
            }),
            Look::Quiet
        );
    }

    #[test]
    fn neither_signal_fires_before_its_window() {
        let e = Evidence {
            age_ms: T_UDP_SILENCE.as_millis() as u64 - 1,
            ..blocked_receiver()
        };
        assert_eq!(look_at(e), Look::Quiet);
        let e = Evidence {
            age_ms: T_KEEPALIVE_SILENCE.as_millis() as u64 - 1,
            ..blocked_sender()
        };
        assert_eq!(look_at(e), Look::Quiet);
    }

    /// One datagram is enough: the distribution is not continuous, and a link
    /// that delivered anything at all is not the link this feature is about.
    #[test]
    fn a_single_datagram_settles_the_question() {
        assert_eq!(
            look_at(Evidence {
                media_seen: Some(1),
                ..blocked_receiver()
            }),
            Look::Quiet
        );
    }

    /// With nothing sent there is nothing for a keepalive to be a reply to, so
    /// its absence says nothing — the same discipline the health clause applies
    /// to the control channel.
    #[test]
    fn a_sender_that_has_sent_nothing_cannot_complain_about_the_silence() {
        assert_eq!(
            look_at(Evidence {
                sent_and_ka: Some((0, 0)),
                ..blocked_sender()
            }),
            Look::Quiet
        );
        assert_eq!(
            look_at(Evidence {
                sent_and_ka: Some((400, 1)),
                ..blocked_sender()
            }),
            Look::Quiet,
            "one keepalive came back, so the reverse direction works"
        );
    }

    /// A stream that both sends and receives is judged on the receiver's
    /// evidence first: it is the earlier and the more direct of the two, and
    /// the reason string is the one that names the direction that actually
    /// failed.
    #[test]
    fn the_receivers_evidence_is_read_first() {
        let both = Evidence {
            age_ms: 4_000,
            media_seen: Some(0),
            sent_and_ka: Some((400, 0)),
            ..blocked_receiver()
        };
        assert_eq!(look_at(both), Look::Blocked(REASON_NO_INBOUND));
    }

    /// The half of this feature's correctness that lives in another file:
    /// signal 1's clock starts on the RECEIVING side the moment `OpenStream`
    /// arrives, so the opener must have its media source running before it
    /// sends that message.
    ///
    /// The other order is a false positive on the main path, not an edge case.
    /// `start_tx_stream` waits up to `SOURCE_ACK_TIMEOUT` (5 s) and a macOS
    /// process tap really does take its time — it builds an aggregate device,
    /// and the first run waits on a TCC prompt — while the receiver reaches a
    /// verdict in about a second. The cost of losing that race is an hour of
    /// TCP, a dropped control channel and a session replay, on a link where
    /// nothing was ever wrong.
    ///
    /// Asserted as an order in the source because that is what it is: no unit
    /// test can observe "which of two lines ran first" in a function that opens
    /// a device and blocks on a peer, and this repository has re-broken orderings
    /// that only a comment defended.
    #[test]
    fn the_media_source_is_running_before_open_stream_goes_out() {
        let src = std::fs::read_to_string(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/conn.rs"),
        )
        .expect("conn.rs");
        let open = src.find("pub(crate) fn open_session_from").expect(
            "open_session_from was renamed; update this guard rather than letting it decay into \
             an always-true assertion",
        );
        let body = &src[open..];
        let start = body
            .find("start_tx_stream(")
            .expect("the send side is built somewhere");
        let send = body
            .find("conn.send_msg(&open)")
            .expect("OpenStream is sent somewhere");
        assert!(
            start < send,
            "open_session_from sends OpenStream before starting its media source. The peer arms \
             the UDP-silence watchdog the moment that message lands, and a source that takes a \
             second to open then reads as a link that cannot carry UDP — persisted for an hour."
        );
    }
}
