//! DeviceFacts publication (provider side) and storage (consumer side),
//! docs/physical-presentation-1.0.2-design.md §3.5, §4.2, §4.3.
//!
//! Provider: one loop classifies this machine's current default input and
//! output (`audiohub_core::device_facts::probe_default_facts`) and broadcasts
//! the result to every live peer that advertised
//! `AudioCapabilities.device_facts_version >= 1`. A default-device change first
//! publishes that direction as `Unknown` — the previous endpoint's facts cannot
//! describe the new one — and only then, after a short debounce and a bounded
//! probe, the new classification. A low-frequency recheck catches properties
//! that change without a device event. The `share_device_facts` setting off
//! publishes both directions as `Virtual("provider_opt_out")` and probes
//! nothing.
//!
//! Consumer: each connection keeps the peer's advertised facts version and the
//! newest valid observation (revision de-dup, structural sanitisation, injected
//! origin refused unless this build has `facts-injection`). [`peer_facts`] is
//! the read accessor the presentation policy builds on; this module decides
//! nothing about presentation itself.
//!
//! The provider's classification is a gate for unmodified AudioHub builds, not
//! an anti-forgery boundary (design §2.4): whatever arrives here is treated as
//! an affordance only and never changes what any machine may open.

use crate::{dlog, lk, ConnShared, DaemonInner};
use audiohub_core::device_facts::{
    probe_default_facts, reason, DeviceClass, DeviceFactsObservation, Direction, DirectionFacts,
    FactsOrigin,
};
use audiohub_net::secure::SessionMsg;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

/// Reasons this module emits. Same `[a-z0-9_]{1,64}` alphabet as
/// `audiohub_core::device_facts::reason`, so a consumer's validation accepts
/// them.
pub(crate) mod local_reason {
    /// A default device changed (or an injection changed) and its new
    /// classification has not been produced yet.
    pub const PROBE_PENDING: &str = "probe_pending";
    /// The classification probe did not answer within its budget.
    pub const PROBE_TIMEOUT: &str = "probe_timeout";
    /// The probe worker panicked, could not be started or stopped.
    pub const PROBE_FAILED: &str = "probe_worker_failed";
    /// The provider's `share_device_facts` setting is off.
    pub const PROVIDER_OPT_OUT: &str = "provider_opt_out";
    /// A test-only injected record reached a consumer built without
    /// `facts-injection` (design §8.2).
    pub const INJECTED_REFUSED: &str = "injected_refused";
    /// Consumer view: the peer advertised `device_facts_version == 0`.
    pub const PEER_UNSUPPORTED: &str = "peer_unsupported";
    /// Consumer view: the peer's `AudioCapabilities` has not arrived yet.
    pub const PEER_UNHEARD: &str = "peer_capabilities_unheard";
    /// Consumer view: the peer supports facts but none has arrived yet.
    pub const FACTS_PENDING: &str = "facts_pending";
}

/// A burst of default-device events settles before the (property-reading)
/// probe runs. `Unknown` is published immediately, so this only delays the
/// final answer, never the withdrawal.
const DEBOUNCE: Duration = Duration::from_millis(300);
/// Budget of one probe (design §3.5).
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// Low-frequency recheck for properties that change without a device event
/// (TransportType, owning plug-in). An unchanged result publishes nothing.
const RECHECK: Duration = Duration::from_secs(10);
const TICK: Duration = Duration::from_millis(100);

#[cfg_attr(not(any(test, feature = "facts-injection")), allow(dead_code))]
const INPUT: usize = 0;
#[cfg_attr(not(any(test, feature = "facts-injection")), allow(dead_code))]
const OUTPUT: usize = 1;

#[cfg_attr(not(feature = "facts-injection"), allow(dead_code))]
fn index(direction: Direction) -> usize {
    match direction {
        Direction::Input => INPUT,
        Direction::Output => OUTPUT,
    }
}

// ------------------------------------------------------------ daemon state

/// Provider-side state hung on `DaemonInner`.
pub(crate) struct LocalFacts {
    /// What this machine currently publishes. Its `revision` is monotonic for
    /// the daemon's lifetime, so a peer connecting later simply accepts
    /// whatever revision it sees first.
    published: Mutex<DeviceFactsObservation>,
    /// Bumped whenever any peer's stored facts change. The presentation
    /// policy can poll it instead of being called from the network reader.
    peer_facts_epoch: AtomicU64,
    #[cfg(feature = "facts-injection")]
    injection: Mutex<Injection>,
}

/// Test-only classifier override (design §8.2), per direction.
#[cfg(feature = "facts-injection")]
#[derive(Default)]
struct Injection {
    facts: [Option<DirectionFacts>; 2],
    /// Bumped on every inject/clear. Part of the planner's per-direction key,
    /// so an injection change takes the same Unknown-then-result path as a
    /// default-device change.
    generation: [u64; 2],
}

impl LocalFacts {
    pub(crate) fn new() -> LocalFacts {
        LocalFacts {
            published: Mutex::new(DeviceFactsObservation::default()),
            peer_facts_epoch: AtomicU64::new(0),
            #[cfg(feature = "facts-injection")]
            injection: Mutex::new(Injection::default()),
        }
    }

    pub(crate) fn published(&self) -> DeviceFactsObservation {
        lk(&self.published).clone()
    }
}

/// Consumer-side state hung on `ConnShared`: connection-scoped like
/// `peer_native_output`, because a dead channel's last word is history.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PeerDeviceFacts {
    /// `AudioCapabilities.device_facts_version` from this connection; `None`
    /// until the capability message has been heard.
    pub version: Option<u8>,
    /// Newest accepted observation, already sanitised and with the injected
    /// origin policy applied.
    pub observation: Option<DeviceFactsObservation>,
}

/// What the presentation policy reads for one peer (design §4.5, §5.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PeerFactsView {
    pub facts_version: Option<u8>,
    pub observation: Option<DeviceFactsObservation>,
}

impl PeerFactsView {
    /// The record to present for `direction` on this live connection. Every
    /// "no facts" case is non-physical: unheard / pending are `Unknown`, an
    /// old peer is `Virtual`. Callers that want to keep a persisted record
    /// while a supporting peer's facts are still in flight look at
    /// `facts_version` / `observation` directly.
    pub(crate) fn effective(&self, direction: Direction) -> DirectionFacts {
        match (self.facts_version, &self.observation) {
            (None, _) => DirectionFacts::unknown(local_reason::PEER_UNHEARD),
            (Some(0), _) => DirectionFacts::virtual_(local_reason::PEER_UNSUPPORTED),
            (Some(_), None) => DirectionFacts::unknown(local_reason::FACTS_PENDING),
            (Some(_), Some(o)) => match direction {
                Direction::Input => o.input.clone(),
                Direction::Output => o.output.clone(),
            },
        }
    }
}

/// The current live connection's facts for peer `fp`; `None` when the peer is
/// not connected.
pub(crate) fn peer_facts(inner: &DaemonInner, fp: &str) -> Option<PeerFactsView> {
    let conn = lk(&inner.state)
        .conns
        .get(fp)
        .filter(|c| c.alive.load(Ordering::SeqCst))
        .cloned()?;
    let cell = lk(&conn.peer_device_facts).clone();
    Some(PeerFactsView {
        facts_version: cell.version,
        observation: cell.observation,
    })
}

/// Bumped whenever any peer's stored facts changed (see [`LocalFacts`]).
pub(crate) fn peer_facts_epoch(inner: &DaemonInner) -> u64 {
    inner.device_facts.peer_facts_epoch.load(Ordering::Acquire)
}

// ---------------------------------------------------------- consumer side

/// Whether this build accepts records whose origin is `Injected`.
pub(crate) const ACCEPT_INJECTED: bool = cfg!(feature = "facts-injection");

/// Sanitise a remote observation and apply the injected-origin policy.
fn sanitize_remote(observation: DeviceFactsObservation, accept_injected: bool) -> DeviceFactsObservation {
    let refuse = |f: DirectionFacts| {
        if f.origin == FactsOrigin::Injected && !accept_injected {
            DirectionFacts::virtual_(local_reason::INJECTED_REFUSED)
        } else {
            f
        }
    };
    let clean = observation.sanitized();
    DeviceFactsObservation {
        revision: clean.revision,
        input: refuse(clean.input),
        output: refuse(clean.output),
    }
}

/// Store `incoming` in `cell` if it is newer than what is there. `true` = the
/// stored observation changed. Facts from a peer that did not advertise
/// support are dropped: that peer is presented as virtual (design §4.4).
fn accept_into(
    cell: &mut PeerDeviceFacts,
    incoming: DeviceFactsObservation,
    accept_injected: bool,
) -> bool {
    if cell.version.unwrap_or(0) < 1 {
        return false;
    }
    if cell
        .observation
        .as_ref()
        .is_some_and(|current| incoming.revision <= current.revision)
    {
        return false;
    }
    let incoming = sanitize_remote(incoming, accept_injected);
    let changed = cell
        .observation
        .as_ref()
        .is_none_or(|current| current.input != incoming.input || current.output != incoming.output);
    cell.observation = Some(incoming);
    changed
}

/// Record the peer's advertised facts version. Returns `true` when the peer
/// just became facts-capable, i.e. our current facts must be sent to it now.
/// Going back to 0 drops whatever facts this connection held.
fn note_version(cell: &mut PeerDeviceFacts, version: u8) -> (bool, bool) {
    let was_capable = cell.version.unwrap_or(0) >= 1;
    cell.version = Some(version);
    let dropped = version < 1 && cell.observation.take().is_some();
    (version >= 1 && !was_capable, dropped)
}

/// `SessionMsg::DeviceFacts` from the current connection of a peer.
pub(crate) fn accept_remote(
    inner: &DaemonInner,
    conn: &Arc<ConnShared>,
    observation: DeviceFactsObservation,
) {
    let revision = observation.revision;
    let (changed, stored) = {
        let mut cell = lk(&conn.peer_device_facts);
        if cell.version.unwrap_or(0) < 1 {
            drop(cell);
            dlog!(
                "[audiohubd] ignoring device facts r{revision} from {}: it did not advertise \
                 device_facts_version >= 1",
                conn.fp
            );
            return;
        }
        let changed = accept_into(&mut cell, observation, ACCEPT_INJECTED);
        (changed, cell.observation.clone())
    };
    if changed {
        inner.device_facts.peer_facts_epoch.fetch_add(1, Ordering::AcqRel);
        if let Some(o) = stored {
            dlog!(
                "[audiohubd] peer {} device facts r{}: input={} output={}",
                conn.fp,
                o.revision,
                describe(&o.input),
                describe(&o.output)
            );
        }
    }
}

/// `AudioCapabilities.device_facts_version` from the current connection of a
/// peer. Sends our current facts the first time the peer turns out to be
/// facts-capable.
pub(crate) fn note_peer_version(inner: &DaemonInner, conn: &Arc<ConnShared>, version: u8) {
    let (send_now, dropped) = note_version(&mut lk(&conn.peer_device_facts), version);
    if dropped {
        inner.device_facts.peer_facts_epoch.fetch_add(1, Ordering::AcqRel);
    }
    if send_now {
        let observation = inner.device_facts.published();
        let _ = conn.send_msg(&SessionMsg::DeviceFacts { observation });
    }
}

fn describe(f: &DirectionFacts) -> String {
    let class = match f.class {
        DeviceClass::Physical => "physical",
        DeviceClass::Virtual => "virtual",
        DeviceClass::Unknown => "unknown",
        DeviceClass::Absent => "absent",
    };
    let origin = match f.origin {
        FactsOrigin::Probe => "",
        FactsOrigin::Injected => " [injected]",
    };
    match f.physical_descriptor() {
        Some(d) => format!("{class}({}, {:?}/{:?}){origin}", d.name, d.transport, d.form_factor),
        None => format!("{class}({}){origin}", f.reason),
    }
}

// ------------------------------------------------------- provider planner

/// Per-direction identity of "the thing being described": the platform
/// default-device epoch plus the injection generation. A change in either
/// invalidates the previous classification of that direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct DirKeys {
    pub dev: [u64; 2],
    pub inj: [u64; 2],
}

impl DirKeys {
    fn dir(&self, d: usize) -> (u64, u64) {
        (self.dev[d], self.inj[d])
    }
}

/// Everything the planner reads from the daemon in one pass.
#[derive(Debug, Clone, Default)]
pub(crate) struct PlanInputs {
    pub keys: DirKeys,
    pub opt_out: bool,
    /// Injected overrides (`origin` already `Injected`); always `None` in a
    /// build without `facts-injection`.
    pub overrides: [Option<DirectionFacts>; 2],
}

#[derive(Debug, Clone, Copy)]
struct Pending {
    started: Instant,
    keys: DirKeys,
    timed_out: bool,
}

/// The publication state machine, free of threads and I/O so it can be
/// driven with synthetic time.
pub(crate) struct Planner {
    published: DeviceFactsObservation,
    /// Keys and opt-out last seen; a difference is a change to settle.
    observed: Option<(DirKeys, bool)>,
    /// The last completed probe and the keys it describes.
    probe: Option<(DirKeys, [DirectionFacts; 2])>,
    /// Keys a probe timed out for: those directions read `probe_timeout`
    /// rather than `probe_pending` until something newer arrives.
    timed_out: Option<DirKeys>,
    settle_from: Option<Instant>,
    pending: Option<Pending>,
    next_recheck: Instant,
}

impl Planner {
    pub(crate) fn new(now: Instant) -> Planner {
        Planner {
            published: DeviceFactsObservation::default(),
            observed: None,
            probe: None,
            timed_out: None,
            settle_from: None,
            pending: None,
            next_recheck: now + RECHECK,
        }
    }

    #[cfg(test)]
    pub(crate) fn published(&self) -> &DeviceFactsObservation {
        &self.published
    }

    /// One loop pass. Returns the observation to broadcast (if what we
    /// publish changed) and the keys to probe for (if a probe should start).
    pub(crate) fn tick(
        &mut self,
        now: Instant,
        inputs: &PlanInputs,
    ) -> (Option<DeviceFactsObservation>, Option<DirKeys>) {
        if self.observed != Some((inputs.keys, inputs.opt_out)) {
            self.observed = Some((inputs.keys, inputs.opt_out));
            self.settle_from = Some(now);
        }
        if let Some(p) = self.pending.as_mut() {
            if !p.timed_out && now.saturating_duration_since(p.started) >= PROBE_TIMEOUT {
                p.timed_out = true;
                self.timed_out = Some(p.keys);
            }
        }
        let mut probe = None;
        if self.pending.is_none() && !inputs.opt_out {
            let settled = self
                .settle_from
                .is_some_and(|t| now.saturating_duration_since(t) >= DEBOUNCE);
            if settled || (self.settle_from.is_none() && now >= self.next_recheck) {
                self.settle_from = None;
                self.pending = Some(Pending {
                    started: now,
                    keys: inputs.keys,
                    timed_out: false,
                });
                probe = Some(inputs.keys);
            }
        } else if inputs.opt_out {
            // Nothing to probe while opted out. The last classification is
            // forgotten, so an opt-in re-probes instead of republishing facts
            // that may have gone stale without a device event.
            self.settle_from = None;
            self.probe = None;
        }
        (self.commit(inputs), probe)
    }

    /// A probe for `keys` finished (possibly after it was declared timed out).
    pub(crate) fn probe_done(
        &mut self,
        now: Instant,
        keys: DirKeys,
        result: DeviceFactsObservation,
        inputs: &PlanInputs,
    ) -> Option<DeviceFactsObservation> {
        if self.pending.is_some_and(|p| p.keys == keys) {
            self.pending = None;
        }
        self.next_recheck = now + RECHECK;
        self.probe = Some((keys, [result.input, result.output]));
        if self.timed_out == Some(keys) {
            self.timed_out = None;
        }
        self.commit(inputs)
    }

    /// The probe could not even be requested (worker gone). Fail closed.
    pub(crate) fn probe_unavailable(
        &mut self,
        now: Instant,
        keys: DirKeys,
        inputs: &PlanInputs,
    ) -> Option<DeviceFactsObservation> {
        let failed = DirectionFacts::unknown(local_reason::PROBE_FAILED);
        self.probe_done(
            now,
            keys,
            DeviceFactsObservation {
                revision: 0,
                input: failed.clone(),
                output: failed,
            },
            inputs,
        )
    }

    fn effective(&self, inputs: &PlanInputs) -> [DirectionFacts; 2] {
        if inputs.opt_out {
            let v = DirectionFacts::virtual_(local_reason::PROVIDER_OPT_OUT);
            return [v.clone(), v];
        }
        std::array::from_fn(|d| {
            let current = inputs.keys.dir(d);
            match &self.probe {
                Some((keys, facts)) if keys.dir(d) == current => inputs.overrides[d]
                    .clone()
                    .unwrap_or_else(|| facts[d].clone()),
                _ if self.timed_out.is_some_and(|k| k.dir(d) == current) => {
                    DirectionFacts::unknown(local_reason::PROBE_TIMEOUT)
                }
                _ => DirectionFacts::unknown(local_reason::PROBE_PENDING),
            }
        })
    }

    fn commit(&mut self, inputs: &PlanInputs) -> Option<DeviceFactsObservation> {
        let [input, output] = self.effective(inputs);
        if input == self.published.input && output == self.published.output {
            return None;
        }
        let revision = self.published.revision.checked_add(1)?;
        self.published = DeviceFactsObservation {
            revision,
            input,
            output,
        };
        Some(self.published.clone())
    }
}

// --------------------------------------------------------- provider loop

fn plan_inputs(inner: &DaemonInner) -> PlanInputs {
    let opt_out = !lk(&inner.settings).share_device_facts;
    #[allow(unused_mut)]
    let mut inputs = PlanInputs {
        keys: DirKeys {
            dev: [
                inner.dev_in_epoch.load(Ordering::Acquire),
                inner.dev_out_epoch.load(Ordering::Acquire),
            ],
            inj: [0, 0],
        },
        opt_out,
        overrides: [None, None],
    };
    #[cfg(feature = "facts-injection")]
    {
        let inj = lk(&inner.device_facts.injection);
        inputs.keys.inj = inj.generation;
        inputs.overrides = inj.facts.clone();
    }
    inputs
}

fn publish(inner: &DaemonInner, observation: DeviceFactsObservation) {
    *lk(&inner.device_facts.published) = observation.clone();
    dlog!(
        "[audiohubd] local device facts r{}: input={} output={}",
        observation.revision,
        describe(&observation.input),
        describe(&observation.output)
    );
    let peers: Vec<_> = lk(&inner.state)
        .conns
        .values()
        .filter(|conn| conn.alive.load(Ordering::SeqCst))
        .cloned()
        .collect();
    for peer in peers {
        // Only peers that understand the variant: an old peer would just count
        // it in `bad_session_msgs` (design §4.2).
        if lk(&peer.peer_device_facts).version.unwrap_or(0) < 1 {
            continue;
        }
        let _ = peer.send_msg(&SessionMsg::DeviceFacts {
            observation: observation.clone(),
        });
    }
}

pub(crate) fn watch_loop(inner: Arc<DaemonInner>) {
    let (request_tx, request_rx) = mpsc::sync_channel::<(DirKeys, String)>(1);
    let (result_tx, result_rx) = mpsc::sync_channel::<(DirKeys, DeviceFactsObservation)>(1);
    // One worker, at most one probe outstanding: a wedged OS property call
    // must neither stall this loop nor spawn a succession of replacements
    // (same discipline as `output_caps::watch_loop`).
    let worker = std::thread::Builder::new()
        .name("ahb-facts-probe".into())
        .spawn(move || {
            while let Ok((keys, fp)) = request_rx.recv() {
                let facts = std::panic::catch_unwind(|| probe_default_facts(&fp)).unwrap_or_else(
                    |_| {
                        let failed = DirectionFacts::unknown(reason::PROBE_FAILED);
                        DeviceFactsObservation {
                            revision: 0,
                            input: failed.clone(),
                            output: failed,
                        }
                    },
                );
                if result_tx.send((keys, facts)).is_err() {
                    break;
                }
            }
        });
    let mut worker_ok = worker.is_ok();
    if !worker_ok {
        dlog!("[audiohubd] could not start the device facts probe worker; publishing unknown");
    }
    // Not joined: the worker owns no daemon state and may be inside an OS
    // call that never returns. Dropping `request_tx` ends it.
    drop(worker);
    let mut planner = Planner::new(Instant::now());
    while !inner.shutdown.load(Ordering::SeqCst) {
        let inputs = plan_inputs(&inner);
        match result_rx.try_recv() {
            Ok((keys, facts)) => {
                if let Some(o) = planner.probe_done(Instant::now(), keys, facts, &inputs) {
                    publish(&inner, o);
                }
            }
            Err(mpsc::TryRecvError::Disconnected) => worker_ok = false,
            Err(mpsc::TryRecvError::Empty) => {}
        }
        let (change, probe) = planner.tick(Instant::now(), &inputs);
        if let Some(o) = change {
            publish(&inner, o);
        }
        if let Some(keys) = probe {
            let fp = inner.identity().fingerprint;
            if !worker_ok || request_tx.try_send((keys, fp)).is_err() {
                if let Some(o) = planner.probe_unavailable(Instant::now(), keys, &inputs) {
                    publish(&inner, o);
                }
            }
        }
        std::thread::sleep(TICK);
    }
}

// ------------------------------------------------------- test injection

/// The marker the release-binary scan looks for (design §8.2 item 4). It is
/// referenced at daemon start (the banner), so no linker can drop it from a
/// test build; a release build has no such symbol and no such bytes.
#[cfg(feature = "facts-injection")]
#[used]
pub static AUDIOHUB_FACTS_INJECTION_ENABLED: [u8; 32] = *b"AUDIOHUB_FACTS_INJECTION_ENABLED";

/// The build marker as text, for the start-up banner.
#[cfg(feature = "facts-injection")]
pub fn facts_injection_marker() -> &'static str {
    std::str::from_utf8(std::hint::black_box(&AUDIOHUB_FACTS_INJECTION_ENABLED))
        .unwrap_or("facts-injection")
}

/// Test features compiled into this daemon (`daemon.status.build_features`).
/// Empty in every shipped build.
pub(crate) fn build_features() -> Vec<&'static str> {
    #[allow(unused_mut)]
    let mut features = Vec::new();
    #[cfg(feature = "facts-injection")]
    features.push("facts-injection");
    #[cfg(feature = "fake-audio")]
    features.push("fake-audio");
    features
}

/// `test.facts.inject`: override (or with `None`, stop overriding) the
/// classifier for one direction. The record is stamped `origin = Injected`
/// and then travels the normal publication path, so a consumer sees exactly
/// the wire messages a real device change would produce.
#[cfg(feature = "facts-injection")]
pub(crate) fn inject(
    inner: &DaemonInner,
    direction: Direction,
    facts: Option<DirectionFacts>,
) -> anyhow::Result<u64> {
    let facts = match facts {
        Some(mut f) => {
            anyhow::ensure!(
                f.is_valid(),
                "injected facts are not a valid DirectionFacts record"
            );
            f.origin = FactsOrigin::Injected;
            Some(f)
        }
        None => None,
    };
    let d = index(direction);
    let mut inj = lk(&inner.device_facts.injection);
    inj.facts[d] = facts;
    inj.generation[d] += 1;
    Ok(inj.generation[d])
}

/// `daemon.status.device_facts`: what this machine publishes and what each
/// connected peer published to it.
pub(crate) fn status(inner: &DaemonInner) -> serde_json::Value {
    let fps: Vec<String> = lk(&inner.state).conns.keys().cloned().collect();
    let mut by_peer = serde_json::Map::new();
    for fp in fps {
        let Some(view) = peer_facts(inner, &fp) else {
            continue;
        };
        by_peer.insert(
            fp,
            serde_json::json!({
                "facts_version": view.facts_version,
                "observation": view.observation.clone(),
                "effective": {
                    "input": view.effective(Direction::Input),
                    "output": view.effective(Direction::Output),
                },
            }),
        );
    }
    #[allow(unused_mut)]
    let mut v = serde_json::json!({
        "version": audiohub_core::device_facts::DEVICE_FACTS_VERSION,
        "share_device_facts": lk(&inner.settings).share_device_facts,
        "accept_injected": ACCEPT_INJECTED,
        "local": inner.device_facts.published(),
        "peers": by_peer,
        "peer_facts_epoch": peer_facts_epoch(inner),
    });
    #[cfg(feature = "facts-injection")]
    {
        let inj = lk(&inner.device_facts.injection);
        v["injection"] = serde_json::json!({
            "input": inj.facts[INPUT],
            "output": inj.facts[OUTPUT],
            "generation": inj.generation,
        });
    }
    v
}

// ------------------------------------------------------------------ tests

#[cfg(test)]
mod tests {
    use super::*;
    use audiohub_core::device_facts::{FormFactor, PhysicalDescriptor, PhysicalTransport};

    fn physical(name: &str) -> DirectionFacts {
        DirectionFacts::physical(PhysicalDescriptor {
            transport: PhysicalTransport::Usb,
            form_factor: FormFactor::Headphones,
            name: name.into(),
            endpoint_desc: None,
            adapter_name: None,
            manufacturer: Some("Vendor".into()),
            device_token: "0123456789abcdef".into(),
            group: 0,
        })
    }

    fn obs(input: DirectionFacts, output: DirectionFacts) -> DeviceFactsObservation {
        DeviceFactsObservation {
            revision: 0,
            input,
            output,
        }
    }

    fn inputs(dev: [u64; 2]) -> PlanInputs {
        PlanInputs {
            keys: DirKeys { dev, inj: [0, 0] },
            ..PlanInputs::default()
        }
    }

    /// Drives `planner` from `t0` in TICK steps until it asks for a probe,
    /// collecting everything it published on the way.
    fn run_until_probe(
        planner: &mut Planner,
        t: &mut Instant,
        i: &PlanInputs,
        published: &mut Vec<DeviceFactsObservation>,
    ) -> DirKeys {
        for _ in 0..200 {
            let (change, probe) = planner.tick(*t, i);
            published.extend(change);
            if let Some(k) = probe {
                return k;
            }
            *t += TICK;
        }
        panic!("the planner never asked for a probe");
    }

    #[test]
    fn startup_publishes_pending_then_the_first_probe_result() {
        let mut t = Instant::now();
        let mut p = Planner::new(t);
        let i = inputs([0, 0]);
        let mut out = Vec::new();
        let keys = run_until_probe(&mut p, &mut t, &i, &mut out);
        assert_eq!(out.len(), 1, "exactly one pending publication before the probe");
        assert_eq!(out[0].output.class, DeviceClass::Unknown);
        assert_eq!(out[0].output.reason, local_reason::PROBE_PENDING);
        let result = obs(DirectionFacts::virtual_("transport_virtual"), physical("Spk"));
        let done = p.probe_done(t, keys, result.clone(), &i).expect("result published");
        assert_eq!(done.revision, out[0].revision + 1);
        assert_eq!(done.output, result.output);
        assert_eq!(done.input, result.input);
    }

    /// Design §3.5: a default-device change withdraws that direction at once
    /// (Unknown), and only the settled probe publishes the new result. The
    /// direction that did not change keeps its facts throughout.
    #[test]
    fn a_default_device_change_publishes_unknown_first_then_the_new_result() {
        let mut t = Instant::now();
        let mut p = Planner::new(t);
        let mut i = inputs([0, 0]);
        let mut out = Vec::new();
        let k = run_until_probe(&mut p, &mut t, &i, &mut out);
        p.probe_done(t, k, obs(physical("Mic"), physical("Spk")), &i)
            .expect("first result");
        let before = p.published().clone();

        i.keys.dev[OUTPUT] += 1;
        t += TICK;
        let (change, probe) = p.tick(t, &i);
        assert!(probe.is_none(), "a burst must settle before probing");
        let withdrawn = change.expect("the change is published immediately");
        assert_eq!(withdrawn.revision, before.revision + 1);
        assert_eq!(withdrawn.output.class, DeviceClass::Unknown);
        assert_eq!(withdrawn.input, before.input, "the unchanged direction is untouched");

        // A second event inside the debounce window restarts it and publishes
        // nothing new (still Unknown).
        t += TICK;
        i.keys.dev[OUTPUT] += 1;
        let (change, probe) = p.tick(t, &i);
        assert!(change.is_none() && probe.is_none());

        let mut more = Vec::new();
        let k = run_until_probe(&mut p, &mut t, &i, &mut more);
        assert!(more.is_empty(), "no duplicate Unknown while settling");
        assert_eq!(k, i.keys);
        let after = p
            .probe_done(t, k, obs(physical("Mic"), physical("Headset")), &i)
            .expect("new result");
        assert_eq!(after.revision, withdrawn.revision + 1);
        assert_eq!(after.output, physical("Headset"));
    }

    #[test]
    fn identical_rechecks_publish_nothing_and_revisions_only_move_on_change() {
        let mut t = Instant::now();
        let mut p = Planner::new(t);
        let i = inputs([3, 4]);
        let mut out = Vec::new();
        let k = run_until_probe(&mut p, &mut t, &i, &mut out);
        let result = obs(physical("Mic"), physical("Spk"));
        p.probe_done(t, k, result.clone(), &i).expect("first");
        let rev = p.published().revision;
        // The periodic recheck fires after RECHECK without any event...
        let mut recheck = Vec::new();
        let k = run_until_probe(&mut p, &mut t, &i, &mut recheck);
        assert!(recheck.is_empty());
        // ...and an unchanged answer is not re-published.
        assert!(p.probe_done(t, k, result, &i).is_none());
        assert_eq!(p.published().revision, rev);
    }

    #[test]
    fn a_probe_result_for_superseded_keys_does_not_describe_the_new_device() {
        let mut t = Instant::now();
        let mut p = Planner::new(t);
        let mut i = inputs([0, 0]);
        let mut out = Vec::new();
        let old_keys = run_until_probe(&mut p, &mut t, &i, &mut out);
        // The output changes while that probe is in flight.
        i.keys.dev[OUTPUT] += 1;
        t += TICK;
        p.tick(t, &i);
        let late = p.probe_done(t, old_keys, obs(physical("Mic"), physical("Old")), &i);
        let published = late.expect("the input half is current and publishable");
        assert_eq!(published.input, physical("Mic"));
        assert_eq!(published.output.class, DeviceClass::Unknown);
        // A fresh probe for the current keys follows.
        let mut more = Vec::new();
        assert_eq!(run_until_probe(&mut p, &mut t, &i, &mut more), i.keys);
    }

    #[test]
    fn a_timed_out_probe_reads_unknown_timeout() {
        let mut t = Instant::now();
        let mut p = Planner::new(t);
        let i = inputs([0, 0]);
        let mut out = Vec::new();
        let k = run_until_probe(&mut p, &mut t, &i, &mut out);
        t += PROBE_TIMEOUT;
        let (change, probe) = p.tick(t, &i);
        assert!(probe.is_none(), "never a second probe while one is outstanding");
        let o = change.expect("timeout is published");
        assert_eq!(o.output.reason, local_reason::PROBE_TIMEOUT);
        assert_eq!(o.input.reason, local_reason::PROBE_TIMEOUT);
        // The late answer still describes the current keys and is used.
        let done = p.probe_done(t, k, obs(physical("Mic"), physical("Spk")), &i);
        assert_eq!(done.expect("late result").output, physical("Spk"));
    }

    #[test]
    fn share_device_facts_off_publishes_virtual_and_never_probes() {
        let mut t = Instant::now();
        let mut p = Planner::new(t);
        let mut i = inputs([0, 0]);
        let mut out = Vec::new();
        let k = run_until_probe(&mut p, &mut t, &i, &mut out);
        p.probe_done(t, k, obs(physical("Mic"), physical("Spk")), &i);

        i.opt_out = true;
        let mut published = Vec::new();
        for _ in 0..(RECHECK.as_millis() / TICK.as_millis() + 10) {
            t += TICK;
            let (change, probe) = p.tick(t, &i);
            assert!(probe.is_none(), "an opted-out provider reads no device properties");
            published.extend(change);
        }
        assert_eq!(published.len(), 1, "exactly one opt-out publication");
        for f in [&published[0].input, &published[0].output] {
            assert_eq!(f.class, DeviceClass::Virtual);
            assert_eq!(f.reason, local_reason::PROVIDER_OPT_OUT);
            assert!(f.physical.is_none(), "no descriptive facts leave an opted-out provider");
        }
        // Opting back in withdraws to Unknown until a fresh probe answers.
        i.opt_out = false;
        t += TICK;
        let (change, _) = p.tick(t, &i);
        assert_eq!(change.expect("opt-in").output.class, DeviceClass::Unknown);
    }

    #[test]
    fn an_injection_change_takes_the_unknown_then_result_path_with_injected_origin() {
        let mut t = Instant::now();
        let mut p = Planner::new(t);
        let mut i = inputs([0, 0]);
        let mut out = Vec::new();
        let k = run_until_probe(&mut p, &mut t, &i, &mut out);
        p.probe_done(t, k, obs(DirectionFacts::virtual_("x"), DirectionFacts::virtual_("x")), &i);

        let mut injected = physical("Injected Spk");
        injected.origin = FactsOrigin::Injected;
        i.keys.inj[OUTPUT] += 1;
        i.overrides[OUTPUT] = Some(injected.clone());
        t += TICK;
        let (change, _) = p.tick(t, &i);
        assert_eq!(change.expect("withdraw").output.class, DeviceClass::Unknown);
        let mut more = Vec::new();
        let k = run_until_probe(&mut p, &mut t, &i, &mut more);
        let o = p
            .probe_done(t, k, obs(DirectionFacts::virtual_("x"), DirectionFacts::virtual_("x")), &i)
            .expect("injected result");
        assert_eq!(o.output, injected, "the override replaces the classifier");
        assert_eq!(o.input, DirectionFacts::virtual_("x"));
    }

    // ----------------------------------------------------------- consumer

    fn capable() -> PeerDeviceFacts {
        PeerDeviceFacts {
            version: Some(1),
            observation: None,
        }
    }

    fn rev(mut o: DeviceFactsObservation, revision: u64) -> DeviceFactsObservation {
        o.revision = revision;
        o
    }

    #[test]
    fn consumer_dedups_by_revision_and_rejects_reordered_updates() {
        let mut cell = capable();
        assert!(accept_into(&mut cell, rev(obs(physical("A"), physical("B")), 5), false));
        for r in [0, 4, 5] {
            assert!(!accept_into(
                &mut cell,
                rev(obs(DirectionFacts::virtual_("x"), DirectionFacts::virtual_("x")), r),
                false
            ));
        }
        assert_eq!(cell.observation.as_ref().unwrap().output, physical("B"));
        // A newer revision with identical content is stored but is no change.
        assert!(!accept_into(&mut cell, rev(obs(physical("A"), physical("B")), 6), false));
        assert_eq!(cell.observation.as_ref().unwrap().revision, 6);
        assert!(accept_into(
            &mut cell,
            rev(obs(physical("A"), DirectionFacts::unknown("probe_pending")), 7),
            false
        ));
    }

    #[test]
    fn consumer_sanitises_invalid_records_to_virtual() {
        let mut cell = capable();
        let mut bad = physical("B");
        bad.physical.as_mut().unwrap().device_token = "NOT-A-TOKEN".into();
        assert!(accept_into(&mut cell, rev(obs(physical("A"), bad), 1), false));
        let o = cell.observation.unwrap();
        assert_eq!(o.input, physical("A"), "a valid half survives");
        assert_eq!(o.output.class, DeviceClass::Virtual);
        assert_eq!(o.output.reason, reason::INVALID_FACTS);
    }

    #[test]
    fn injected_records_are_accepted_only_by_a_consumer_built_with_injection() {
        let mut injected = physical("Injected");
        injected.origin = FactsOrigin::Injected;
        let incoming = rev(obs(physical("A"), injected.clone()), 1);

        let mut release = capable();
        accept_into(&mut release, incoming.clone(), false);
        let o = release.observation.unwrap();
        assert_eq!(o.output.class, DeviceClass::Virtual);
        assert_eq!(o.output.reason, local_reason::INJECTED_REFUSED);
        assert_eq!(o.input, physical("A"), "probe-origin facts are unaffected");

        let mut test_build = capable();
        accept_into(&mut test_build, incoming, true);
        assert_eq!(test_build.observation.unwrap().output, injected);

        // This build's policy is exactly its feature set.
        assert_eq!(ACCEPT_INJECTED, cfg!(feature = "facts-injection"));
    }

    /// Design §4.4: a 1.0.1 provider (device_facts_version absent = 0) is
    /// presented as virtual, and facts it might send anyway are not stored.
    #[test]
    fn a_peer_without_device_facts_support_stays_virtual() {
        let mut cell = PeerDeviceFacts::default();
        let view = |c: &PeerDeviceFacts| PeerFactsView {
            facts_version: c.version,
            observation: c.observation.clone(),
        };
        assert_eq!(
            view(&cell).effective(Direction::Output).reason,
            local_reason::PEER_UNHEARD
        );
        let (send, dropped) = note_version(&mut cell, 0);
        assert!(!send && !dropped, "nothing is sent to an old peer");
        assert!(!accept_into(&mut cell, rev(obs(physical("A"), physical("B")), 1), true));
        assert!(cell.observation.is_none());
        for d in [Direction::Input, Direction::Output] {
            let f = view(&cell).effective(d);
            assert_eq!(f.class, DeviceClass::Virtual);
            assert_eq!(f.reason, local_reason::PEER_UNSUPPORTED);
        }
    }

    #[test]
    fn facts_are_sent_once_when_a_peer_becomes_capable_and_dropped_if_it_stops() {
        let mut cell = PeerDeviceFacts::default();
        assert_eq!(note_version(&mut cell, 1), (true, false));
        assert_eq!(note_version(&mut cell, 1), (false, false), "re-announce sends nothing new");
        assert_eq!(
            PeerFactsView {
                facts_version: cell.version,
                observation: None
            }
            .effective(Direction::Input)
            .reason,
            local_reason::FACTS_PENDING
        );
        assert!(accept_into(&mut cell, rev(obs(physical("A"), physical("B")), 1), false));
        assert_eq!(note_version(&mut cell, 0), (false, true));
        assert!(cell.observation.is_none());
    }

    #[test]
    fn every_local_reason_passes_wire_validation() {
        for r in [
            local_reason::PROBE_PENDING,
            local_reason::PROBE_TIMEOUT,
            local_reason::PROBE_FAILED,
            local_reason::PROVIDER_OPT_OUT,
            local_reason::INJECTED_REFUSED,
            local_reason::PEER_UNSUPPORTED,
            local_reason::PEER_UNHEARD,
            local_reason::FACTS_PENDING,
        ] {
            assert!(DirectionFacts::unknown(r).is_valid(), "{r}");
        }
    }

    #[cfg(feature = "facts-injection")]
    #[test]
    fn the_injection_marker_and_build_feature_are_present_in_this_build() {
        assert_eq!(facts_injection_marker(), "AUDIOHUB_FACTS_INJECTION_ENABLED");
        assert!(build_features().contains(&"facts-injection"));
    }

    #[cfg(not(feature = "facts-injection"))]
    #[test]
    fn a_build_without_injection_reports_no_such_feature() {
        assert!(!build_features().contains(&"facts-injection"));
    }
}
