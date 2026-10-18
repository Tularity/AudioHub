//! Daemon tests against the fake audio backend (regression suite, DESIGN §8.7).
//!
//! Two real daemons run in this process — distinct ports, config directories
//! and names, announcing off, HAL bridge off — paired non-interactively over
//! loopback. They share one fake audio world, which stands in for every audio
//! device: nothing here opens real hardware, and each test ends by asserting
//! the fake backend's tripwire (`native_touches`) is still zero.
//!
//! The fake world is process-global, so every test holds a [`WorldGuard`] for
//! its whole run; the guard serialises the tests in this module. Run them on
//! their own (`cargo test --features fake-audio ... fake_`): other daemon
//! tests in the same process would be served by an installed fake world.
//!
//! `fake_capture_death_without_default_change_recovers` encodes the product
//! contract for the known defect PR#1 and is `#[ignore]`d so the ordinary run
//! stays green; the suite runs it with `--ignored` and records the failure as
//! a known issue. It is written to pass once the defect is fixed.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use audiohub_core::audio::fake::{
    self, FakeDeviceCfg, FakeSourceCfg, FakeWorldCfg, SinkStats, WorldGuard, SCHEMA,
};
use audiohub_ipc::{methods, Mode, KIND_MIC, KIND_SPK, SOURCE_MIC, SOURCE_TONE};

use crate::halbridge::HalBridgeMode;
use crate::{fake_ipc, ipcserv, start_daemon, DaemonCfg, DaemonHandle};

const MIC1: &str = "fake-mic-1";
const MIC2: &str = "fake-mic-2";
const SPK1: &str = "fake-spk-1";
const SPK2: &str = "fake-spk-2";
const MIC1_HZ: f64 = 1000.0;
const MIC2_HZ: f64 = 1500.0;
const TONE_HZ: f64 = 1000.0;

/// Analysis window for "is the tone there right now".
const WINDOW_MS: u32 = 200;
/// Startup: pairing, connection, session negotiation, jitter buffer fill.
const START_BUDGET: Duration = Duration::from_secs(20);
/// Time allowed to recover after an injected device event. The same budget
/// for the positive controls and the PR#1 case, so a pass and a fail are
/// measured on one scale.
const RECOVERY_BUDGET: Duration = Duration::from_secs(5);
/// Time for an injected capture death to show up as silence at the far end
/// (the capture FIFO and the receiver's jitter buffer drain first).
const SILENCE_BUDGET: Duration = Duration::from_secs(5);

fn world() -> FakeWorldCfg {
    FakeWorldCfg {
        schema: Some(SCHEMA.to_string()),
        dir: None,
        devices: vec![
            FakeDeviceCfg::input(MIC1, 48_000, 2, FakeSourceCfg::sine(MIC1_HZ, 0.25)),
            FakeDeviceCfg::input(MIC2, 44_100, 1, FakeSourceCfg::sine(MIC2_HZ, 0.25)),
            FakeDeviceCfg::output(SPK1, 48_000, 2),
            FakeDeviceCfg::output(SPK2, 96_000, 2),
        ],
        default_input: Some(MIC1.to_string()),
        default_output: Some(SPK1.to_string()),
    }
}

struct Node {
    h: DaemonHandle,
    dir: PathBuf,
}

impl Drop for Node {
    fn drop(&mut self) {
        self.h.shutdown();
        // Joined, not merely signalled: a daemon thread outliving the test
        // could otherwise open a stream in the NEXT test's fake world.
        self.h.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Node {
    fn start(tag: &str) -> Node {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("ahb-fake-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let h = start_daemon(DaemonCfg {
            control_port: 0,
            ipc_port: 0,
            config_dir: Some(dir.clone()),
            announce: Some(false),
            hal_bridge: Some(HalBridgeMode::Off),
            tx_throttle_kbps: None,
            block_udp: None,
            announce_fault: false,
            airplay_advertise: false,
        })
        .expect("start daemon");
        let node = Node { h, dir };
        node.ok(methods::SETTINGS_SET, json!({ "name": format!("ahr-fake-{tag}") }));
        node
    }

    fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        ipcserv::dispatch_for_test(self.h.inner_for_test(), method, &params)
    }

    fn ok(&self, method: &str, params: Value) -> Value {
        self.call(method, params)
            .unwrap_or_else(|e| panic!("{method} failed: {e}"))
    }

    fn fingerprint(&self) -> String {
        self.h.fingerprint.clone()
    }

    fn addr(&self) -> String {
        format!("127.0.0.1:{}", self.h.control_port)
    }

    fn set_mode(&self, m: Mode) {
        self.ok(methods::SETTINGS_SET, json!({ "mode": m.as_str() }));
    }

    fn inject(&self, action: Value) -> Value {
        self.ok(fake_ipc::INJECT, action)
    }

    /// The node's receiving session of `kind`, if one is open.
    fn recv_session(&self, kind: &str) -> Option<Value> {
        self.ok(methods::SESSION_LIST, json!({}))
            .as_array()?
            .iter()
            .find(|s| s["dir"].as_str() == Some("recv") && s["kind"].as_str() == Some(kind))
            .cloned()
    }
}

/// A (consumer, mode A) and B (share), paired and connected over loopback.
fn linked() -> (Node, Node) {
    let a = Node::start("a");
    let b = Node::start("b");
    assert_ne!(a.h.control_port, b.h.control_port);
    let name = |n: &Node| n.ok(methods::DAEMON_STATUS, json!({}))["name"].clone();
    assert_eq!(name(&a), "ahr-fake-a");
    assert_eq!(name(&b), "ahr-fake-b");
    a.set_mode(Mode::A);
    b.set_mode(Mode::Share);
    let pin = b.ok(methods::PAIRING_ENABLE, json!({ "ttl_s": 60 }));
    let pin = pin["pin"].as_str().expect("pin").to_string();
    a.ok(methods::PEERS_PAIR, json!({ "addr": b.addr(), "pin": pin }));
    a.ok(methods::PEERS_CONNECT, json!({ "peer": b.fingerprint(), "addr": b.addr() }));
    for n in [&a, &b] {
        let st = n.ok(methods::DAEMON_STATUS, json!({}));
        assert_eq!(st["audio_backend"], "fake", "not a fake daemon: {st}");
        assert_eq!(st["fake"]["marker"].as_str(), Some(fake::MARKER));
    }
    (a, b)
}

fn sink(guard: &WorldGuard, uid: &str, freq: f64) -> SinkStats {
    guard
        .world()
        .sink_stats(uid, Some(freq), WINDOW_MS)
        .expect("sink stats")
}

fn tone_present(guard: &WorldGuard, uid: &str, freq: f64) -> bool {
    sink(guard, uid, freq)
        .tone
        .is_some_and(|t| t.detected)
}

fn eventually_within(budget: Duration, what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out after {budget:?} waiting for: {what}");
}

/// Polls `f` until it holds or `budget` runs out; `true` = it held.
fn within(budget: Duration, mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

fn assert_no_native_touch(guard: &WorldGuard) {
    let st = guard.world().status();
    assert_eq!(
        st.native_touches, 0,
        "the fake backend's tripwire fired: a product path reached a native audio entry point \
         ({:?}); this run is not a fake-backend result",
        st.native_touch_sites
    );
}

fn opens(guard: &WorldGuard, uid: &str) -> u64 {
    guard
        .world()
        .status()
        .devices
        .iter()
        .find(|d| d.uid == uid)
        .map(|d| d.opens)
        .unwrap_or(0)
}

/// A consumes B's default microphone and monitors it on its own default
/// output, so the microphone's tone ends up in the `SPK1` sink.
fn open_monitored_mic(a: &Node, b: &Node) {
    a.ok(
        methods::SESSION_OPEN,
        json!({
            "peer": b.fingerprint(), "kind": KIND_MIC, "source": SOURCE_MIC,
            "monitor": true, "verify_freq": MIC1_HZ
        }),
    );
}

#[test]
fn fake_tone_reaches_fake_sink() {
    let guard = fake::install(world()).expect("install fake world");
    let (a, b) = linked();
    a.ok(
        methods::SESSION_OPEN,
        json!({
            "peer": b.fingerprint(), "kind": KIND_SPK, "source": SOURCE_TONE,
            "freq": TONE_HZ, "verify_freq": TONE_HZ
        }),
    );
    eventually_within(START_BUDGET, "a 1 kHz tone in B's fake output sink", || {
        tone_present(&guard, SPK1, TONE_HZ)
    });

    // The same answer through the IPC surface the suite uses.
    let ipc = b.ok(
        fake_ipc::SINK_STATS,
        json!({ "uid": SPK1, "freq": TONE_HZ, "window_ms": WINDOW_MS }),
    );
    assert_eq!(ipc["tone"]["detected"], true, "debug.fake.sink_stats disagrees: {ipc}");
    let tone = sink(&guard, SPK1, TONE_HZ).tone.expect("fitted");
    eprintln!("[fake_tests] smoke: sink tone {tone:?}");

    // The receiver's own verdict agrees with the sink once its rolling
    // analysis window has filled.
    eventually_within(START_BUDGET, "B's receiver verdict detects the tone", || {
        b.recv_session(KIND_SPK)
            .is_some_and(|s| s["stats"]["verdict"]["detected"] == Value::Bool(true))
    });

    // The fake device consumed audio at its own clock: about one second of
    // frames per second of wall time.
    let f0 = sink(&guard, SPK1, TONE_HZ).frames_total;
    let t0 = Instant::now();
    std::thread::sleep(Duration::from_secs(1));
    let f1 = sink(&guard, SPK1, TONE_HZ).frames_total;
    let rate = (f1 - f0) as f64 / t0.elapsed().as_secs_f64();
    assert!(
        (rate - 48_000.0).abs() < 4_800.0,
        "the fake output consumed {rate:.0} frames/s instead of ~48000"
    );
    assert_eq!(opens(&guard, MIC1), 0, "a tone session must not open the microphone");
    assert_no_native_touch(&guard);
    drop(a);
    drop(b);
    assert_no_native_touch(&guard);
}

/// Positive control: the fake backend can express an output stream death, and
/// the product's existing site-playback retire/reopen path recovers from it.
#[test]
fn fake_output_stream_death_reopens_playback() {
    let guard = fake::install(world()).expect("install fake world");
    let (a, b) = linked();
    a.ok(
        methods::SESSION_OPEN,
        json!({ "peer": b.fingerprint(), "kind": KIND_SPK, "source": SOURCE_TONE, "freq": TONE_HZ }),
    );
    eventually_within(START_BUDGET, "a 1 kHz tone in B's fake output sink", || {
        tone_present(&guard, SPK1, TONE_HZ)
    });
    let gen_before = sink(&guard, SPK1, TONE_HZ).gen;

    let out = b.inject(json!({ "action": "kill_stream", "uid": SPK1 }));
    assert!(
        out["killed_streams"].as_u64().unwrap_or(0) >= 1,
        "the injection killed nothing: {out}"
    );

    eventually_within(
        RECOVERY_BUDGET,
        "playback reopened on a new stream generation with the tone back",
        || {
            let s = sink(&guard, SPK1, TONE_HZ);
            s.gen > gen_before
                && s.window_frames >= (s.rate as u64 * WINDOW_MS as u64 / 1000)
                && s.tone.is_some_and(|t| t.detected)
        },
    );
    eprintln!(
        "[fake_tests] output death: gen {gen_before} -> {}",
        sink(&guard, SPK1, TONE_HZ).gen
    );
    assert_no_native_touch(&guard);
    drop(a);
    drop(b);
    assert_no_native_touch(&guard);
}

/// Positive control for PR#1: a default-input change notification reaches the
/// daemon through the fake watcher and the microphone source is rebuilt on the
/// new default device.
#[test]
fn fake_default_input_change_rebuilds_mic() {
    let guard = fake::install(world()).expect("install fake world");
    let (a, b) = linked();
    open_monitored_mic(&a, &b);
    eventually_within(START_BUDGET, "B's 1 kHz microphone in A's fake output sink", || {
        tone_present(&guard, SPK1, MIC1_HZ)
    });
    assert_eq!(opens(&guard, MIC2), 0);

    let out = b.inject(json!({ "action": "set_default", "kind": "input", "uid": MIC2 }));
    assert_eq!(out["notified"], true, "no default-input watcher was notified: {out}");

    eventually_within(
        RECOVERY_BUDGET,
        "the microphone rebuilt on the new default input (1.5 kHz in A's sink)",
        || tone_present(&guard, SPK1, MIC2_HZ),
    );
    assert!(opens(&guard, MIC2) >= 1, "the new default input was never opened");
    assert_no_native_touch(&guard);
    drop(a);
    drop(b);
    assert_no_native_touch(&guard);
}

/// PR#1: the capture stream dies while the default input is unchanged.
///
/// Product contract: within the recovery budget the microphone source is
/// rebuilt and the far end hears it again, or the session is closed (so the
/// failure is visible). Today neither happens: `LiveCapture`'s health is never
/// read, `MicSource` never reports `failed()`, and a rebuild is only requested
/// when the default-input epoch changes — the session stays open and silent.
#[test]
#[ignore = "known_issue: PR#1 capture death without default change is never rebuilt"]
fn fake_capture_death_without_default_change_recovers() {
    let guard = fake::install(world()).expect("install fake world");
    let (a, b) = linked();
    open_monitored_mic(&a, &b);
    eventually_within(START_BUDGET, "B's 1 kHz microphone in A's fake output sink", || {
        tone_present(&guard, SPK1, MIC1_HZ)
    });
    let mic_opens_before = opens(&guard, MIC1);

    let out = b.inject(json!({ "action": "kill_stream", "uid": MIC1 }));
    assert!(
        out["killed_streams"].as_u64().unwrap_or(0) >= 1,
        "precondition: the injection killed no capture stream: {out}"
    );
    assert_eq!(out["notified"], false, "precondition: a stream death must not notify");

    // Precondition: the death is observable at the far end. If the tone never
    // goes away, the fake did not express the fault and nothing below means
    // anything.
    let went_silent = within(SILENCE_BUDGET, || !tone_present(&guard, SPK1, MIC1_HZ));
    let recovered_early = !went_silent;
    let mut recovered = recovered_early && opens(&guard, MIC1) > mic_opens_before;
    let mut closed = false;
    if !recovered {
        assert!(
            went_silent,
            "precondition: the microphone tone never went away after its capture stream died"
        );
        let deadline = Instant::now() + RECOVERY_BUDGET;
        while Instant::now() < deadline {
            if tone_present(&guard, SPK1, MIC1_HZ) {
                recovered = true;
                break;
            }
            if a.recv_session(KIND_MIC).is_none() {
                closed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    eprintln!(
        "[fake_tests] PR#1: recovered={recovered} closed={closed} mic_opens {} -> {}",
        mic_opens_before,
        opens(&guard, MIC1)
    );
    assert!(
        recovered || closed,
        "PR#1: the capture stream died with the default input unchanged, and within {:?} the \
         microphone source was neither rebuilt (opens of {MIC1} stayed at {}) nor was the \
         session closed — it stayed open and silent",
        RECOVERY_BUDGET,
        opens(&guard, MIC1)
    );
    assert_no_native_touch(&guard);
    drop(a);
    drop(b);
    assert_no_native_touch(&guard);
}

// ------------------------------------------------- device facts (design §3.5/§4)

/// `b`'s facts as `a` currently holds them: (version, output record).
fn facts_seen_by(
    a: &Node,
    b: &Node,
) -> Option<(Option<u8>, Option<u64>, audiohub_core::device_facts::DirectionFacts)> {
    let view = crate::device_facts_pub::peer_facts(a.h.inner_for_test(), &b.fingerprint())?;
    let output = view.effective(audiohub_core::device_facts::Direction::Output);
    Some((
        view.facts_version,
        view.observation.as_ref().map(|o| o.revision),
        output,
    ))
}

/// The real publication path end to end over loopback: B classifies its fake
/// defaults (Virtual "fake_audio_backend", nothing native is touched), A
/// stores them; B's `share_device_facts=false` reaches A as a provider
/// opt-out, and turning it back on restores the classification.
#[test]
fn fake_device_facts_reach_the_peer_and_honour_the_opt_out() {
    use audiohub_core::device_facts::{reason, DeviceClass};
    let guard = fake::install(world()).expect("install fake world");
    let (a, b) = linked();
    eventually_within(START_BUDGET, "A holds B's classified facts", || {
        facts_seen_by(&a, &b).is_some_and(|(v, _, out)| {
            v == Some(audiohub_core::device_facts::DEVICE_FACTS_VERSION)
                && out.class == DeviceClass::Virtual
                && out.reason == reason::FAKE_AUDIO_BACKEND
        })
    });

    b.ok(methods::SETTINGS_SET, json!({ "share_device_facts": false }));
    eventually_within(RECOVERY_BUDGET, "B's opt-out reaches A", || {
        facts_seen_by(&a, &b).is_some_and(|(_, _, out)| {
            out.class == DeviceClass::Virtual
                && out.reason == crate::device_facts_pub::local_reason::PROVIDER_OPT_OUT
        })
    });
    let st = a.ok(methods::DAEMON_STATUS, json!({}));
    let peer = &st["device_facts"]["peers"][b.fingerprint()];
    assert_eq!(peer["effective"]["input"]["reason"], "provider_opt_out", "{peer}");
    assert_eq!(
        b.ok(methods::DAEMON_STATUS, json!({}))["device_facts"]["share_device_facts"],
        false
    );

    b.ok(methods::SETTINGS_SET, json!({ "share_device_facts": true }));
    eventually_within(RECOVERY_BUDGET, "B's opt-in restores the classification", || {
        facts_seen_by(&a, &b)
            .is_some_and(|(_, _, out)| out.reason == reason::FAKE_AUDIO_BACKEND)
    });
    assert_no_native_touch(&guard);
    drop(a);
    drop(b);
    assert_no_native_touch(&guard);
}

/// Design §8.3 in-process: injected physical facts on the provider reach the
/// consumer through the normal wire path (Unknown first, then the injected
/// record), and withdrawing the injection goes Unknown then back to the real
/// (virtual) classification. Revisions only ever increase.
#[cfg(feature = "facts-injection")]
#[test]
fn fake_injected_physical_facts_reach_the_consumer_and_withdraw_via_unknown() {
    use audiohub_core::device_facts::{DeviceClass, FactsOrigin};
    let guard = fake::install(world()).expect("install fake world");
    let (a, b) = linked();
    for n in [&a, &b] {
        let st = n.ok(methods::DAEMON_STATUS, json!({}));
        assert!(
            st["build_features"]
                .as_array()
                .is_some_and(|f| f.iter().any(|x| x == "facts-injection")),
            "{}",
            st["build_features"]
        );
    }
    eventually_within(START_BUDGET, "A holds B's classified facts", || {
        facts_seen_by(&a, &b).is_some_and(|(_, _, out)| out.class == DeviceClass::Virtual)
    });

    // An invalid record is refused at the IPC boundary.
    let bad = b.call(
        methods::TEST_FACTS_INJECT,
        json!({ "direction": "output", "facts": {
            "class": "physical", "reason": "physical",
            "physical": { "transport": "usb", "form_factor": "headphones",
                          "name": "X", "device_token": "not-a-token" } } }),
    );
    assert!(bad.is_err(), "an invalid injected record was accepted: {bad:?}");

    let physical = json!({
        "class": "physical", "reason": "physical",
        "physical": { "transport": "usb", "form_factor": "headphones",
                      "name": "Injected Headphones", "manufacturer": "Test",
                      "device_token": "0123456789abcdef" } });

    // Records every distinct output state A goes through, in order.
    let watch = |until: &dyn Fn(&audiohub_core::device_facts::DirectionFacts) -> bool| {
        let mut seen: Vec<(u64, audiohub_core::device_facts::DirectionFacts)> = Vec::new();
        let deadline = Instant::now() + RECOVERY_BUDGET;
        while Instant::now() < deadline {
            if let Some((_, Some(rev), out)) = facts_seen_by(&a, &b) {
                if seen.last().is_none_or(|(r, _)| *r != rev) {
                    seen.push((rev, out.clone()));
                }
                if until(&out) {
                    return seen;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out; states seen: {seen:?}");
    };
    let increasing = |seen: &[(u64, audiohub_core::device_facts::DirectionFacts)]| {
        seen.windows(2).all(|w| w[0].0 < w[1].0)
    };

    b.ok(methods::TEST_FACTS_INJECT, json!({ "direction": "output", "facts": physical }));
    let seen = watch(&|out| out.class == DeviceClass::Physical);
    eprintln!("[fake_tests] facts inject: {seen:?}");
    assert!(increasing(&seen), "{seen:?}");
    let last = &seen.last().unwrap().1;
    assert_eq!(last.origin, FactsOrigin::Injected);
    assert_eq!(last.physical_descriptor().unwrap().name, "Injected Headphones");
    assert!(
        seen.iter().any(|(_, f)| f.class == DeviceClass::Unknown),
        "the change must withdraw to Unknown before the new record: {seen:?}"
    );
    // The input direction was not injected and stays as classified.
    let view = crate::device_facts_pub::peer_facts(a.h.inner_for_test(), &b.fingerprint()).unwrap();
    assert_eq!(
        view.effective(audiohub_core::device_facts::Direction::Input).class,
        DeviceClass::Virtual
    );

    b.ok(methods::TEST_FACTS_INJECT, json!({ "direction": "output", "facts": null }));
    let seen = watch(&|out| out.class == DeviceClass::Virtual);
    eprintln!("[fake_tests] facts withdraw: {seen:?}");
    assert!(increasing(&seen), "{seen:?}");
    assert!(
        seen.iter().any(|(_, f)| f.class == DeviceClass::Unknown),
        "the withdrawal must pass through Unknown: {seen:?}"
    );
    assert!(seen.iter().all(|(_, f)| f.class != DeviceClass::Physical
        || f.origin == FactsOrigin::Injected));
    assert_eq!(
        seen.last().unwrap().1.reason,
        audiohub_core::device_facts::reason::FAKE_AUDIO_BACKEND
    );
    assert_no_native_touch(&guard);
    drop(a);
    drop(b);
    assert_no_native_touch(&guard);
}

/// Design §5.4 end to end over the real wire path: B (provider) injects a
/// physical microphone; A's presentation policy — the driver capability
/// simulated as Supported and the platform's ack simulated by `mark_applied`,
/// since this harness has no HAL — decides to present it. Withdrawing the
/// injection engages A's microphone gate for B in the first pass that sees
/// the withdrawal (before any rebuild ran), visible in `daemon.status`, and
/// the gate releases only once the driver confirms the virtual look.
#[cfg(feature = "facts-injection")]
#[test]
fn fake_withdrawn_physical_microphone_engages_the_presentation_gate() {
    use crate::presentation::{DriverPresentation, Presentation};
    use audiohub_core::device_facts::Direction;
    let guard = fake::install(world()).expect("install fake world");
    let (a, b) = linked();
    let inner = a.h.inner_for_test();
    let fp = b.fingerprint();
    {
        let mut st = crate::lk(&inner.haldev);
        st.presentation.set_driver(DriverPresentation::Supported);
        // What a freshly bound device presents (design §6.4).
        st.presentation.mark_applied(&fp, Direction::Input, Presentation::Virtual);
        st.presentation.mark_applied(&fp, Direction::Output, Presentation::Virtual);
    }
    let input_target = || {
        crate::haldev::evaluate_presentation(inner);
        crate::lk(&inner.haldev)
            .presentation
            .target(&fp, Direction::Input)
            .cloned()
    };
    let gated = || crate::lk(&inner.haldev).presentation.mic_gated(&fp);

    let mic = json!({
        "class": "physical", "reason": "physical",
        "physical": { "transport": "usb", "form_factor": "microphone",
                      "name": "Injected Microphone", "manufacturer": "Test",
                      "device_token": "fedcba9876543210" } });
    b.ok(methods::TEST_FACTS_INJECT, json!({ "direction": "input", "facts": mic }));
    eventually_within(START_BUDGET, "A decides to present B's microphone as physical", || {
        input_target().is_some_and(|d| d.presentation.is_physical())
    });
    assert!(!gated(), "a virtual look over physical facts is never gated");
    let look = input_target().expect("target").presentation;
    assert_eq!(look.descriptor().map(|d| d.name.as_str()), Some("Injected Microphone"));
    crate::lk(&inner.haldev)
        .presentation
        .mark_applied(&fp, Direction::Input, look);
    let _ = input_target();
    assert!(!gated(), "a physical look matching the facts lets audio flow");

    b.ok(methods::TEST_FACTS_INJECT, json!({ "direction": "input", "facts": null }));
    eventually_within(RECOVERY_BUDGET, "A's microphone gate engages for B", || {
        input_target().is_some_and(|d| !d.presentation.is_physical()) && gated()
    });
    let st = a.ok(methods::DAEMON_STATUS, json!({}));
    let peer = &st["presentation"]["peers"][fp.as_str()];
    assert_eq!(peer["input"]["mic_gate"], true, "{st}");
    assert_eq!(peer["input"]["target"]["class"], "virtual", "{peer}");
    assert_eq!(peer["input"]["applied"]["class"], "physical", "{peer}");
    assert_eq!(st["presentation"]["driver"], "supported");

    crate::lk(&inner.haldev)
        .presentation
        .mark_applied(&fp, Direction::Input, Presentation::Virtual);
    let _ = input_target();
    assert!(!gated(), "the confirmed virtual look releases the gate");
    assert_no_native_touch(&guard);
    drop(a);
    drop(b);
    assert_no_native_touch(&guard);
}
