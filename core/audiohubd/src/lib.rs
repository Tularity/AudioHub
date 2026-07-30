//! audiohubd — daemon assembly (spec-m4a §1/§4).
//! Frozen lib entry: `DaemonCfg` / `DaemonHandle` / `start_daemon`.

mod conn;
mod engine;
pub mod halbridge;
mod ipcserv;
pub mod reconnect;

/// Public for the deviceless test that pins the "one device = one bridge
/// refcount" rule: a raw selector and its resolved name must key the same
/// entry, and the resolver is the only thing that can guarantee it.
pub use engine::resolve_bridge_device;

use std::collections::{HashMap, VecDeque};
use std::net::{SocketAddr, TcpListener, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex, MutexGuard, Once, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use base64::prelude::*;

use audiohub_core::audio::{self, DeviceChangeWatcher, DeviceKind};
use audiohub_core::dsp::{self, LinearResampler};
use audiohub_core::sysaudio::{self, VirtualCard};
use audiohub_core::volume::{self, VolumeState, VolumeSync};
use audiohub_ipc::{
    IpcEndpoint, OpenSessionParams, SessionInfo, SessionStats, IPC_VERSION, KIND_SPK,
};
use audiohub_net::discovery::{self, AnnounceGuard};
use audiohub_net::identity::{LocalIdentity, PairedPeer};
use audiohub_net::media::{AutoLadder, JitterBuffer, MediaCrypto, AUTO_RATES};
use audiohub_net::secure::{SecureChannel, SessionMsg};
use audiohub_net::stats::RxStats;

pub const DIR_SEND: &str = "send";
pub const DIR_RECV: &str = "recv";

/// Media salt length frozen with `SessionMsg::OpenStream.media_salt_b64`.
pub(crate) const MEDIA_SALT_LEN: usize = 16;

/// Shutdown must finish even when a peer stopped draining its socket: past
/// this budget the remaining Byes are skipped (spec §4 calls Bye best effort).
const BYE_BUDGET: Duration = Duration::from_millis(1500);

// Poison-tolerant locking. spec §8: a panic must degrade one stream/conn, not
// wedge the daemon — a thread that panicked while holding shared state leaves
// the data structurally intact, so recovering the guard is the safe choice.
pub(crate) fn lk<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub(crate) fn rd<T>(l: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    l.read().unwrap_or_else(|e| e.into_inner())
}

pub(crate) fn wr<T>(l: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    l.write().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------- logging

/// The daemon's only stderr writer. `eprintln!` PANICS when the write fails —
/// EPIPE the moment a parent closes the pipe it captured us with — and a panic
/// on a daemon thread costs incomparably more than a lost log line: it killed
/// the signal thread before `begin_shutdown` (SIGTERM then ignored, ipc.json
/// stranded, no Bye), and it unwound past `teardown_conn`. Write errors are
/// therefore dropped on the floor, deliberately.
///
/// Public so the binary in main.rs shares the one writer.
pub fn logln(args: std::fmt::Arguments<'_>) {
    use std::io::Write;
    // one write per line: interleaved threads must not tear a line apart
    let mut line = args.to_string();
    line.push('\n');
    let _ = std::io::stderr().write_all(line.as_bytes());
}

macro_rules! dlog {
    ($($arg:tt)*) => { $crate::logln(format_args!($($arg)*)) };
}
pub(crate) use dlog;

// ---------------------------------------------------------------- signals

/// Set by the signal handler only. Polled by each daemon's watchdog thread.
static SIGNAL_SHUTDOWN: AtomicBool = AtomicBool::new(false);
static SIGNAL_INSTALL: Once = Once::new();

#[cfg(unix)]
extern "C" fn on_term(sig: libc::c_int) {
    // async-signal-safe: an atomic store plus signal(), which POSIX lists as
    // safe. Restoring SIG_DFL means a second Ctrl-C still hard-kills, so an
    // embedding host (or a test binary) can never have its signal swallowed.
    SIGNAL_SHUTDOWN.store(true, Ordering::SeqCst);
    unsafe { libc::signal(sig, libc::SIG_DFL) };
}

/// spec §4: SIGTERM / Ctrl-C must run the normal exit path (Bye to peers, close
/// streams, remove ipc.json). Installed here rather than in a binary so every
/// host of `start_daemon` gets it. Idempotent.
fn install_signal_handlers() {
    SIGNAL_INSTALL.call_once(|| {
        #[cfg(unix)]
        {
            let h = on_term as *const () as libc::sighandler_t;
            unsafe {
                libc::signal(libc::SIGTERM, h);
                libc::signal(libc::SIGINT, h);
            }
        }
        // non-unix: no console-ctrl handler yet; spec calls this best effort
    });
}

/// True once a SIGTERM/SIGINT has been observed by this process.
pub fn shutdown_signalled() -> bool {
    SIGNAL_SHUTDOWN.load(Ordering::SeqCst)
}

/// Runs `begin_shutdown` however the signal thread ends — including an unwind.
/// SIGTERM handling is the one path with no second chance: if this thread dies
/// silently the daemon ignores the signal, strands ipc.json and never sends Bye.
/// `begin_shutdown` is idempotent, so the normal exit (shutdown already set by
/// someone else) is a no-op.
struct ShutdownOnExit(Arc<DaemonInner>);

impl Drop for ShutdownOnExit {
    fn drop(&mut self) {
        self.0.begin_shutdown();
    }
}

fn signal_watch_loop(inner: Arc<DaemonInner>) {
    let guard = ShutdownOnExit(inner);
    while !guard.0.shutdown.load(Ordering::SeqCst) {
        if SIGNAL_SHUTDOWN.load(Ordering::SeqCst) {
            dlog!("[audiohubd] signal received, shutting down");
            return; // guard runs begin_shutdown
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Fresh per-stream media salt, generated by the stream opener (B0/frozen API).
pub(crate) fn gen_media_salt() -> [u8; MEDIA_SALT_LEN] {
    use rand_core::RngCore;
    let mut b = [0u8; MEDIA_SALT_LEN];
    rand_core::OsRng.fill_bytes(&mut b);
    b
}

#[derive(Debug, Clone)]
pub struct DaemonCfg {
    pub control_port: u16,
    pub ipc_port: u16,
    pub config_dir: Option<PathBuf>, // explicit override beats AUDIOHUB_CONFIG_DIR / platform default
    pub announce: bool,
    /// `None` = whatever `AUDIOHUB_HAL_BRIDGE` says (the production path).
    ///
    /// Tests must pass `Some(HalBridgeMode::Off)`. The driver hands its rings to
    /// ONE client and a fresh HELLO supersedes the incumbent, so a test daemon
    /// that attaches evicts the user's real one; the real one then times out
    /// after 5s, reconnects, evicts the test daemon, and the two oscillate. That
    /// is not hypothetical — running the suite on a Mac with the driver
    /// installed printed alternating "HAL driver gone (went silent)" /
    /// "HAL driver attached" for as long as the tests ran. The env var cannot
    /// express this: it is process-global and the tests share a process.
    pub hal_bridge: Option<halbridge::HalBridgeMode>,
}

pub struct DaemonHandle {
    pub ipc_port: u16,
    pub token: String,
    pub control_port: u16,
    pub fingerprint: String,
    pub name: String,
    inner: Arc<DaemonInner>,
    threads: Mutex<Vec<JoinHandle<()>>>,
}

impl DaemonHandle {
    /// Signal shutdown and run cleanup (Bye to peers, drop announce, remove
    /// ipc.json). Idempotent; worker threads exit within ~200ms.
    pub fn shutdown(&self) {
        self.inner.begin_shutdown();
    }

    /// Block until the daemon has fully stopped (all core threads joined).
    pub fn wait(&self) {
        loop {
            let h = lk(&self.threads).pop();
            match h {
                Some(j) => {
                    let _ = j.join();
                }
                None => break,
            }
        }
        self.inner.begin_shutdown(); // cleanup even if stop was internal
    }

    /// True once shutdown has been signalled (by `shutdown`, `--secs`, a
    /// signal, or the `daemon.shutdown` IPC method).
    pub fn is_shutdown(&self) -> bool {
        self.inner.shutdown.load(Ordering::SeqCst)
    }

    /// Exactly what `daemon.status` reports as `hal` (spec-round2 §B2). `None`
    /// means this daemon has no HAL bridge, which is the normal case anywhere
    /// but a macOS host running the installed LaunchDaemon — and the case the
    /// deviceless tests pin, because "no bridge" must change nothing.
    pub fn hal_status(&self) -> Option<audiohub_ipc::HalStatus> {
        hal_status(&self.inner)
    }
}

pub fn start_daemon(cfg: DaemonCfg) -> Result<DaemonHandle> {
    let cfg_dir = cfg
        .config_dir
        .clone()
        .unwrap_or_else(LocalIdentity::config_dir);
    let id = LocalIdentity::load_or_create_at(Some(&cfg_dir))?;

    let (control_listener, udp, control_port) = bind_control_media(cfg.control_port)?;
    control_listener.set_nonblocking(true)?;
    udp.set_read_timeout(Some(Duration::from_millis(100)))?;

    let announce_guard = if cfg.announce {
        Some(discovery::announce(&id, control_port).context("mdns announce")?)
    } else {
        None
    };

    // Looks the DRIVER's mach name up (spec-round2 §B1, direction inverted);
    // on a machine with no driver this is Ok(None) and nothing changes.
    let mut hal_cfg = halbridge::HalBridgeCfg::from_env();
    if let Some(mode) = cfg.hal_bridge {
        hal_cfg.mode = mode;
    }
    let hal_bridge = match halbridge::HalBridge::start(hal_cfg) {
        Ok(b) => {
            if let Some(br) = &b {
                let st = br.status();
                dlog!(
                    "hal bridge: driver_found={} name={}",
                    st.driver_found,
                    halbridge::HAL_SERVICE_NAME
                );
            }
            b.map(Arc::new)
        }
        Err(e) => {
            dlog!("hal bridge unavailable: {e:#}");
            None
        }
    };

    ensure_endpoint_unowned(&cfg_dir)?; // refuse to hijack a live daemon's ipc.json

    let ipc_listener =
        TcpListener::bind(("127.0.0.1", cfg.ipc_port)).context("bind ipc listener")?;
    ipc_listener.set_nonblocking(true)?;
    let ipc_port = ipc_listener.local_addr()?.port();
    let token = gen_token();
    write_ipc_json(&cfg_dir, ipc_port, &token)?;

    let (tx_send, tx_recv) = mpsc::channel::<engine::TxCmd>();
    let (mix_send, mix_recv) = mpsc::channel::<engine::MixCmd>();
    let inner = Arc::new(DaemonInner {
        id: id.clone(),
        cfg_dir,
        control_port,
        ipc_port,
        token: token.clone(),
        udp,
        start: Instant::now(),
        state: Mutex::new(DaemonState {
            conns: HashMap::new(),
            sessions: HashMap::new(),
            pairing: None,
        }),
        rx_table: RwLock::new(HashMap::new()),
        tx_cmds: Mutex::new(tx_send),
        mix_cmds: Mutex::new(mix_send),
        mix_ring: Mutex::new(VecDeque::new()),
        store_lock: Mutex::new(()),
        shutdown: AtomicBool::new(false),
        cleanup: Once::new(),
        announce_guard: Mutex::new(announce_guard),
        halbridge: Mutex::new(hal_bridge),
        hal_vol: Mutex::new(None),
        hal_mic_io: AtomicBool::new(true),
        preauth: AtomicUsize::new(0),
        recon: Mutex::new(HashMap::new()),
        dev_in_epoch: AtomicU64::new(0),
        dev_out_epoch: AtomicU64::new(0),
        devices: Mutex::new(None),
    });

    let mut threads = Vec::new();
    let spawn = |name: &str, f: Box<dyn FnOnce() + Send>| -> Result<JoinHandle<()>> {
        std::thread::Builder::new()
            .name(name.to_string())
            .spawn(f)
            .with_context(|| format!("spawn {name}"))
    };
    {
        let i = inner.clone();
        threads.push(spawn(
            "ahb-control",
            Box::new(move || conn::accept_loop(i, control_listener)),
        )?);
    }
    {
        let i = inner.clone();
        threads.push(spawn("ahb-media-rx", Box::new(move || engine::rx_loop(i)))?);
    }
    {
        let i = inner.clone();
        threads.push(spawn(
            "ahb-media-tx",
            Box::new(move || engine::tx_loop(i, tx_recv)),
        )?);
    }
    {
        let i = inner.clone();
        threads.push(spawn(
            "ahb-mixer",
            Box::new(move || engine::mixer_loop(i, mix_recv)),
        )?);
    }
    {
        let i = inner.clone();
        threads.push(spawn(
            "ahb-reconnect",
            Box::new(move || reconnect::supervisor_loop(i)),
        )?);
    }
    {
        let i = inner.clone();
        threads.push(spawn(
            "ahb-devwatch",
            Box::new(move || device_watch_loop(i)),
        )?);
    }
    {
        let i = inner.clone();
        threads.push(spawn(
            "ahb-ipc",
            Box::new(move || ipcserv::accept_loop(i, ipc_listener)),
        )?);
    }
    {
        let i = inner.clone();
        threads.push(spawn("ahb-ticker", Box::new(move || ticker_loop(i)))?);
    }
    {
        install_signal_handlers();
        let i = inner.clone();
        threads.push(spawn(
            "ahb-signal",
            Box::new(move || signal_watch_loop(i)),
        )?);
    }

    Ok(DaemonHandle {
        ipc_port,
        token,
        control_port,
        fingerprint: id.fingerprint,
        name: id.name,
        inner,
        threads: Mutex::new(threads),
    })
}

// ---------------------------------------------------------------- state

pub(crate) struct PairingMode {
    pub pin: String,
    pub until: Instant,
    /// A 6-digit PIN is guessable in ~10^6 tries, so pairing is not just
    /// time-boxed: it is single-use, capped at `MAX_PAIR_FAILURES`, and admits
    /// one attempt at a time.
    pub fails: u32,
    pub in_flight: bool,
}

pub(crate) struct DaemonState {
    pub conns: HashMap<String, Arc<ConnShared>>,
    pub sessions: HashMap<u32, SessionEntry>,
    pub pairing: Option<PairingMode>,
}

pub(crate) struct DaemonInner {
    pub id: LocalIdentity,
    pub cfg_dir: PathBuf,
    pub control_port: u16,
    #[allow(dead_code)]
    pub ipc_port: u16,
    pub token: String,
    pub udp: UdpSocket,
    pub start: Instant,
    pub state: Mutex<DaemonState>,
    pub rx_table: RwLock<HashMap<u32, Arc<RxStream>>>,
    pub tx_cmds: Mutex<mpsc::Sender<engine::TxCmd>>,
    pub mix_cmds: Mutex<mpsc::Sender<engine::MixCmd>>,
    pub mix_ring: Mutex<VecDeque<f32>>, // last 2s of post-clip mixer output @48k
    pub store_lock: Mutex<()>,          // serializes peer-store read/modify/write
    pub shutdown: AtomicBool,
    cleanup: Once,
    pub announce_guard: Mutex<Option<AnnounceGuard>>,
    /// macOS HAL bridge. `None` is the normal case (no LaunchAgent, mode off,
    /// or another platform) — the daemon must behave exactly as before then.
    /// Behind an `Arc` so the 10ms loops can lift a handle out with one short
    /// lock instead of holding this mutex across a tick.
    pub halbridge: Mutex<Option<Arc<halbridge::HalBridge>>>,
    /// Last (scalar, muted) this daemon pushed INTO the driver's controls with
    /// `notify_volume`. Two jobs: it suppresses the mach send when the peer
    /// merely re-reports an unchanged value (the provider refreshes every 5
    /// ticks), and it lets the event drain recognise a driver report that is
    /// only our own value coming back, which is what would otherwise close a
    /// driver -> peer -> driver volume loop.
    pub hal_vol: Mutex<Option<(f32, bool)>>,
    /// Is an application actually reading "AudioHub Microphone"? Driven by the
    /// driver's IoState reports (which it re-posts on every reconnect, so this
    /// re-syncs by itself). It gates the mixer's ring writes for a latency
    /// reason, not a correctness one: only the ring's CONSUMER may move
    /// read_idx, so a ring we fill while nobody drains it stays full, and the
    /// app that eventually starts recording would then read 500ms behind us —
    /// permanently. Starts `true` because "not told yet" must never mean
    /// silence: writes made before the driver attaches are dropped by the
    /// handshake flush on its side anyway.
    pub hal_mic_io: AtomicBool,
    /// Control connections past the first frame but not yet verified; bounds
    /// the number of unauthenticated handshake threads an attacker can pin.
    pub preauth: AtomicUsize,
    /// Reconnect bookkeeping, keyed by fingerprint. An entry exists ONLY for a
    /// peer this daemon has connected out to (spec-m4c §C).
    pub recon: Mutex<HashMap<String, reconnect::PeerRecon>>,
    /// Bumped by the platform default-device watchers and by
    /// `daemon.simulate_device_change`; the tx/mixer/ticker loops each compare
    /// against their own last-seen value, so one event drives every rebuild.
    pub dev_in_epoch: AtomicU64,
    pub dev_out_epoch: AtomicU64,
    pub devices: Mutex<Option<DeviceCache>>,
}

/// Device enumeration costs a system call per device and `daemon.status` sits
/// on the IPC hello path, so the listing is cached briefly and dropped on any
/// default-device change.
pub(crate) struct DeviceCache {
    at: Instant,
    epoch: u64,
    outputs: Vec<String>,
    cards: Vec<VirtualCard>,
}

const DEVICE_CACHE_TTL: Duration = Duration::from_secs(2);

pub(crate) fn device_listing(inner: &DaemonInner) -> (Vec<String>, Vec<VirtualCard>) {
    let epoch = inner
        .dev_in_epoch
        .load(Ordering::Relaxed)
        .wrapping_add(inner.dev_out_epoch.load(Ordering::Relaxed));
    {
        let c = lk(&inner.devices);
        if let Some(c) = c.as_ref() {
            if c.epoch == epoch && c.at.elapsed() < DEVICE_CACHE_TTL {
                return (c.outputs.clone(), c.cards.clone());
            }
        }
    }
    let outputs = audio::list_output_devices();
    let cards = sysaudio::detect_virtual_cards();
    *lk(&inner.devices) = Some(DeviceCache {
        at: Instant::now(),
        epoch,
        outputs: outputs.clone(),
        cards: cards.clone(),
    });
    (outputs, cards)
}

/// spec-m4c §D: the platform watchers are created and dropped on ONE thread —
/// registration is not required to be `Send`, and dropping the watcher is what
/// unregisters it. The callbacks fire on a platform thread, so they do nothing
/// but bump an epoch; every rebuild happens on the loop that owns the device.
fn device_watch_loop(inner: Arc<DaemonInner>) {
    let mut guards = Vec::new();
    for (kind, label) in [(DeviceKind::Input, "input"), (DeviceKind::Output, "output")] {
        let i = inner.clone();
        let cb = Box::new(move || {
            match kind {
                DeviceKind::Input => &i.dev_in_epoch,
                DeviceKind::Output => &i.dev_out_epoch,
            }
            .fetch_add(1, Ordering::Relaxed);
        });
        match DeviceChangeWatcher::start(kind, cb) {
            Ok(w) => guards.push(w),
            Err(e) => dlog!("[audiohubd] default {label} device watcher unavailable: {e:#}"),
        }
    }
    while !inner.shutdown.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(200));
    }
    drop(guards); // unregisters
}

impl DaemonInner {
    pub(crate) fn begin_shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        self.cleanup.call_once(|| {
            // Order matters: dropping mDNS and removing ipc.json cannot block,
            // so they run before any peer I/O. A wedged peer must never strand
            // ipc.json (that would leave `ctl` and wait() waiting forever).
            *lk(&self.announce_guard) = None;
            remove_ipc_json_if_ours(&self.cfg_dir);
            let conns: Vec<Arc<ConnShared>> = lk(&self.state).conns.values().cloned().collect();
            let deadline = Instant::now() + BYE_BUDGET;
            for c in conns {
                // send before marking dead: send_msg only latches death on a
                // real write failure, and each write is bounded by the control
                // write timeout set at connect/accept time
                if Instant::now() < deadline {
                    let _ = c.send_msg(&SessionMsg::Bye {});
                }
                c.alive.store(false, Ordering::SeqCst);
            }
        });
    }

    /// A handle on the HAL bridge, or `None` where there is none — which is
    /// every non-macOS host and every macOS host without the LaunchDaemon.
    /// Callers must lift the handle out and drop the guard (this returns a
    /// clone for exactly that reason): the 10ms loops hold no daemon lock
    /// across a tick.
    pub(crate) fn hal(&self) -> Option<Arc<halbridge::HalBridge>> {
        lk(&self.halbridge).clone()
    }
}

/// Volume values this close are the same value: the driver stores a float the
/// user dragged, the peer's device snaps to its own step grid, and neither is
/// allowed to look like a change and start another round trip.
const HAL_VOL_EPS: f32 = 1.0 / 512.0;

fn hal_vol_same(a: (f32, bool), b: (f32, bool)) -> bool {
    (a.0 - b.0).abs() < HAL_VOL_EPS && a.1 == b.1
}

/// spec-round2 §B2 reverse direction: the peer's real output reported a new
/// state (a `VolumeState` the consumer cell holds), so the virtual speaker's
/// control must show it. Sending only on a genuine change is what keeps this
/// off the provider's every-5-tick refresh, and what stops a driver that
/// echoes its own controls from looping.
///
/// Runs on the ticker, not on the control reader that received the report: a
/// mach send can sit for its full 500ms timeout, and the reader must stay free
/// to keep draining the peer's channel.
fn hal_push_peer_volume(inner: &DaemonInner, hal: &halbridge::HalBridge) {
    // The consumer end of a volume_sync'd spk stream: its cell is the peer's
    // real device, filled from VolumeState (and from the optimistic local echo
    // set_session_volume writes, which is the same value by construction).
    // Snapshot first — every other reader of a session's volume cell takes it
    // with the state lock already released, and this one must not be the
    // exception that introduces a lock order.
    let state = snapshot_sessions(inner) // sorted by id: one pair this round
        .iter()
        .find(|e| e.kind == KIND_SPK && e.dir == DIR_SEND && e.volume.enabled)
        .and_then(|e| *lk(&e.volume.state));
    let Some(v) = state else { return };
    if !v.scalar.is_finite() {
        return;
    }
    let now = (v.scalar.clamp(0.0, 1.0), v.muted);
    {
        let mut last = lk(&inner.hal_vol);
        if last.map_or(false, |l| hal_vol_same(l, now)) {
            return;
        }
        *last = Some(now);
    }
    hal.notify_volume(halbridge::HalDevice::Speaker, now.0, now.1);
}

/// Both halves of spec-round2 §B2's volume sync, once per 200ms sub-tick.
/// Never on a media loop: this writes the control TCP socket and sends mach
/// messages, either of which can block for as long as its own timeout.
fn hal_tick(inner: &Arc<DaemonInner>) {
    let Some(hal) = inner.hal() else { return };
    // Order matters: the driver's own change is dispatched (and recorded as
    // "the control already reads this") BEFORE the peer's state is pushed back,
    // so a slider move never bounces off its own round trip.
    drain_hal_events(inner, &hal);
    hal_push_peer_volume(inner, &hal);
}

/// spec-round2 §B2 forward direction: the local user moved the VIRTUAL
/// speaker's slider, so the peer's REAL device must follow. Reuses the one
/// VolumeSet emitter (`conn::set_session_volume`) rather than inventing a
/// second path, so the admission rules, the SRC_LOCAL tagging and the
/// optimistic local echo are the ones already under test. The IoState reports
/// arrive on the same queue and are drained here too.
fn drain_hal_events(inner: &Arc<DaemonInner>, hal: &halbridge::HalBridge) {
    let events = hal.drain_events();
    if events.is_empty() {
        return;
    }
    // A slider drag posts a burst; only where it ENDED is worth a round trip.
    let mut latest: Option<(f32, bool)> = None;
    for ev in events {
        match ev {
            halbridge::HalControlEvent::Volume { device, scalar, muted } => match device {
                halbridge::HalDevice::Speaker => latest = Some((scalar, muted)),
                // The virtual microphone's own gain is a local control over a
                // stream we WRITE; there is no peer device it could drive.
                halbridge::HalDevice::Microphone => dlog!(
                    "[audiohubd] hal: virtual microphone volume {scalar:.3} muted={muted} \
                     (nothing to sync: the peer owns its own capture gain)"
                ),
            },
            halbridge::HalControlEvent::IoState { device, running } => {
                let dev = match device {
                    halbridge::HalDevice::Speaker => "speaker",
                    halbridge::HalDevice::Microphone => {
                        inner.hal_mic_io.store(running, Ordering::Relaxed);
                        "microphone"
                    }
                };
                dlog!(
                    "[audiohubd] hal: virtual {dev} io {}",
                    if running { "started" } else { "stopped" }
                );
            }
        }
    }
    let Some((scalar, muted)) = latest else { return };
    // Our own notify_volume coming back around: applying it would send the
    // peer what the peer just told us.
    {
        let mut last = lk(&inner.hal_vol);
        if last.map_or(false, |l| hal_vol_same(l, (scalar, muted))) {
            return;
        }
        // The driver's control IS at this value now — recording it here is what
        // keeps the push-back below from sending it straight back.
        *last = Some((scalar, muted));
    }
    // Only a spk session THIS side drives can carry it (the same gate
    // set_session_volume enforces); with §B2's single fixed device pair there
    // is normally exactly one.
    let targets: Vec<u32> = lk(&inner.state)
        .sessions
        .values()
        .filter(|e| e.kind == KIND_SPK && e.dir == DIR_SEND && e.volume.enabled)
        .map(|e| e.id)
        .collect();
    if targets.is_empty() {
        dlog!(
            "[audiohubd] hal: virtual speaker volume {scalar:.3} muted={muted} ignored: no \
             volume_sync'd spk session to carry it"
        );
        return;
    }
    for id in targets {
        if let Err(e) = conn::set_session_volume(inner, id, scalar, Some(muted)) {
            dlog!("[audiohubd] hal: volume {scalar:.3} -> session {id}: {e:#}");
        }
    }
}

/// Bridge health for `daemon.status`. `None` (serialised as null) is the
/// normal answer: no macOS HAL bridge on this host.
pub(crate) fn hal_status(inner: &DaemonInner) -> Option<audiohub_ipc::HalStatus> {
    inner.hal().map(|h| {
        let s = h.status();
        audiohub_ipc::HalStatus {
            // `registered` is the shipped IPC key (frozen by
            // test/tests/hal_wiring.rs); since the direction inverted it means
            // "the driver's mach name is registered and we found it".
            registered: s.driver_found,
            driver_connected: s.driver_connected,
            spk_frames: s.spk_frames,
            mic_frames: s.mic_frames,
            mic_dropped: s.mic_dropped,
            last_driver_msg_secs: s.last_driver_msg_secs,
        }
    })
}

/// `daemon.status` result: the DaemonInfo object plus `hal` (spec-round2 §B2).
/// Kept here rather than as a DaemonInfo field so the struct every existing
/// client deserializes is untouched, and so ipcserv.rs needs exactly one line:
/// `methods::DAEMON_STATUS => crate::status_with_hal(inner, daemon_info(inner))?,`
#[allow(dead_code)] // used the moment ipcserv.rs calls it (that one line)
pub(crate) fn status_with_hal(
    inner: &DaemonInner,
    info: audiohub_ipc::DaemonInfo,
) -> Result<serde_json::Value> {
    let mut v = serde_json::to_value(info)?;
    if let Some(obj) = v.as_object_mut() {
        obj.insert("hal".to_string(), serde_json::to_value(hal_status(inner))?);
    }
    Ok(v)
}

/// One verified + encrypted control connection to a peer.
pub(crate) struct ConnShared {
    pub fp: String,
    pub peer: PairedPeer,
    pub chan: Mutex<SecureChannel>,
    pub tx_key: [u8; 32],
    pub rx_key: [u8; 32],
    /// Frozen: media destination = control-TCP peer IP + peer daemon port.
    pub media_dest: SocketAddr,
    /// Fingerprint of whoever opened this TCP connection. Both peers apply the
    /// same "lower fingerprint wins" rule to a simultaneous bidirectional
    /// connect, so they converge on one connection instead of evicting each
    /// other's.
    pub initiator_fp: String,
    pub created: Instant,
    pub pending: Mutex<HashMap<u32, mpsc::Sender<std::result::Result<(), String>>>>,
    pub alive: AtomicBool,
    /// Millis since `created` at the last frame we read off this channel.
    /// Instant is not atomic, and this is written on every frame.
    pub(crate) last_rx_ms: AtomicU64,
}

impl ConnShared {
    pub fn send_msg(&self, m: &SessionMsg) -> Result<()> {
        let r = lk(&self.chan).send(m);
        if r.is_err() {
            // A timed-out or failed control write leaves a truncated frame on
            // the wire and the AEAD counter out of step, so the channel can
            // never resync: the connection is done, not merely slow.
            self.alive.store(false, Ordering::SeqCst);
        }
        r
    }

    /// Records that a frame arrived. Only a COMPLETE frame counts: a peer
    /// trickling bytes it never finishes is not alive for our purposes.
    pub(crate) fn note_rx(&self) {
        self.last_rx_ms
            .store(self.created.elapsed().as_millis() as u64, Ordering::Relaxed);
    }

    /// How long this channel has been silent. A conn that has never received
    /// anything is measured from its creation, so it gets the same grace.
    pub(crate) fn silent_for(&self) -> Duration {
        let now = self.created.elapsed().as_millis() as u64;
        Duration::from_millis(now.saturating_sub(self.last_rx_ms.load(Ordering::Relaxed)))
    }
}

#[derive(Clone)]
pub(crate) struct SessionEntry {
    pub id: u32,
    pub conn: Arc<ConnShared>,
    pub kind: String, // OpenStream kind label, same on both sides
    pub dir: String,  // local perspective: "send" = we emit media
    pub rx: Option<Arc<RxStream>>,
    pub tx: Option<Arc<TxShared>>,
    pub volume: Arc<VolumeCell>,
    /// `Some` = WE opened this session, and these are the exact params to
    /// replay after a reconnect (spec-m4c §C). A peer-originated session is
    /// `None`: the peer re-opens it.
    pub origin: Option<Arc<OpenSessionParams>>,
}

/// Per-session volume sync state (spec-m4b §A2). Shared behind an Arc because
/// SessionEntry is cloned for every stats snapshot.
pub(crate) struct VolumeCell {
    /// `volume_sync` negotiated on OpenStream. Only ever true on a spk stream.
    pub enabled: bool,
    /// Last known provider-device state: read from our own device on the
    /// provider side, taken from VolumeState reports on the consumer side.
    pub state: Mutex<Option<VolumeState>>,
    /// Provider-side echo suppression; untouched on the consumer side.
    pub sync: Mutex<VolumeSync>,
    /// Ticks since the last VolumeState we put on the wire.
    pub since_report: AtomicU32,
    /// One stderr line per session when the device cannot be read at all.
    read_warned: AtomicBool,
}

impl VolumeCell {
    pub(crate) fn new(enabled: bool) -> VolumeCell {
        VolumeCell {
            enabled,
            state: Mutex::new(None),
            sync: Mutex::new(VolumeSync::new()),
            since_report: AtomicU32::new(0),
            read_warned: AtomicBool::new(false),
        }
    }

    /// True the first time only: a device that stays unreadable is polled once
    /// a second, and stderr must not become the amplifier.
    fn first_read_warning(&self) -> bool {
        !self.read_warned.swap(true, Ordering::Relaxed)
    }
}

pub(crate) struct JbState {
    pub jb: JitterBuffer,
    pub rs_rate: u32,
    pub rs: Option<LinearResampler>, // wire rate -> 48k, recreated on rung switch
    pub rs_last: f32,                // last decoded sample; seeds the next resampler
    pub jit_win: Vec<f32>,           // per-packet transit deltas (ms) for p95
    pub pushes: u32,
    pub last_dropped: u64, // starvation detector (expected-seq raced ahead)
    pub late_streak: u32,
}

pub(crate) struct PostMix {
    pub fifo: VecDeque<f32>, // absorbs resampler length wobble -> exact 480/frame
}

impl PostMix {
    pub fn advance(&mut self, popped: Option<Vec<f32>>, out: &mut [f32]) {
        if let Some(f) = popped {
            self.fifo.extend(f);
        }
        let n = out.len().min(self.fifo.len());
        for o in out.iter_mut().take(n) {
            *o = self.fifo.pop_front().unwrap_or(0.0);
        }
        for o in out.iter_mut().skip(n) {
            *o = 0.0;
        }
        if self.fifo.len() > 4800 {
            let excess = self.fifo.len() - 4800;
            self.fifo.drain(..excess);
        }
    }
}

pub(crate) struct RxCell {
    pub rx: RxStats,
    pub first: Option<Instant>,
    pub last_rate: u32,
    pub prev_transit: Option<i64>,
    // per-interval accounting for Stats/AUTO (the cumulative RxStats figures
    // stay untouched for the lifetime display)
    iv_received: u64,
    iv_expected: u64,
    iv_jit_sum: f64,
    iv_jit_n: u64,
}

/// One 1s window of receive quality: what AUTO must react to.
pub(crate) struct IntervalStats {
    pub received: u64,
    pub lost: u64,
    pub loss_pct: f64,
    pub jitter_ms: f64,
}

impl RxCell {
    pub(crate) fn note_jitter(&mut self, jit_ms: f32) {
        self.iv_jit_sum += jit_ms as f64;
        self.iv_jit_n += 1;
    }

    /// Deltas since the previous tick. `None` when the window saw no arrivals:
    /// `expected` only advances on received sequence numbers, so an empty
    /// window would read as 0% loss and let AUTO promote through a blackout.
    pub(crate) fn take_interval(&mut self) -> Option<IntervalStats> {
        let sm = self.rx.summary(0.0);
        let d_expected = sm.expected.saturating_sub(self.iv_expected);
        let d_received = sm.received.saturating_sub(self.iv_received);
        self.iv_expected = sm.expected;
        self.iv_received = sm.received;
        // mean of this window's per-packet transit deltas; spike-robust and
        // never carries an old dropout forward like the lifetime figure did
        let jitter_ms = if self.iv_jit_n > 0 {
            self.iv_jit_sum / self.iv_jit_n as f64
        } else {
            sm.jitter_ms
        };
        self.iv_jit_sum = 0.0;
        self.iv_jit_n = 0;
        if d_expected == 0 {
            return None;
        }
        let lost = d_expected.saturating_sub(d_received);
        Some(IntervalStats {
            received: d_received,
            lost,
            loss_pct: lost as f64 * 100.0 / d_expected as f64,
            jitter_ms,
        })
    }
}

pub(crate) struct RxStream {
    pub stream_id: u32,
    pub crypto: MediaCrypto,
    pub verify_freq: Option<f32>,
    pub is_spk: bool, // feeds the mixer sum (spk-recv on the provider)
    pub monitor: bool,
    /// Named output device this stream is ALSO rendered into (spec-m4c §B).
    pub bridge: Option<String>,
    /// This stream is ALSO written into the HAL bridge's mic ring, so
    /// "AudioHub Microphone" carries it (spec-round2 §B2). A third
    /// destination, not an alternative to `monitor` or `bridge`.
    pub hal: bool,
    pub ka_dest: SocketAddr,
    pub jbs: Mutex<JbState>,
    pub post: Mutex<PostMix>,
    /// post-JB 48k tap (2s) for per-stream verdicts; only allocated when a
    /// verdict was actually requested, so N streams cost N*0 rings by default
    pub ring: Option<Mutex<VecDeque<f32>>>,
    pub stats: Mutex<RxCell>,
    pub ka_seq: AtomicU32,
}

impl RxStream {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        stream_id: u32,
        key: &[u8; 32],
        media_salt: &[u8],
        verify_freq: Option<f32>,
        is_spk: bool,
        monitor: bool,
        bridge: Option<String>,
        hal: bool,
        ka_dest: SocketAddr,
    ) -> RxStream {
        RxStream {
            stream_id,
            // real streams always key off the opener's per-stream salt
            crypto: MediaCrypto::new_for_stream(key, stream_id, media_salt),
            verify_freq,
            is_spk,
            monitor,
            bridge,
            hal,
            ka_dest,
            jbs: Mutex::new(JbState {
                jb: JitterBuffer::new(2),
                rs_rate: 48000,
                rs: None,
                rs_last: 0.0,
                jit_win: Vec::new(),
                pushes: 0,
                last_dropped: 0,
                late_streak: 0,
            }),
            post: Mutex::new(PostMix { fifo: VecDeque::new() }),
            ring: verify_freq.map(|_| Mutex::new(VecDeque::new())),
            stats: Mutex::new(RxCell {
                rx: RxStats::new(),
                first: None,
                last_rate: 48000,
                prev_transit: None,
                iv_received: 0,
                iv_expected: 0,
                iv_jit_sum: 0.0,
                iv_jit_n: 0,
            }),
            ka_seq: AtomicU32::new(0),
        }
    }
}

#[derive(Clone, Copy, Default)]
pub(crate) struct RemoteStats {
    pub seq: u64, // bumps on every Stats msg; ticker evaluates AUTO once per bump
    // lifetime totals, accumulated from the per-interval deltas the receiver
    // reports — only for display
    pub received: u64,
    pub lost: u64,
    // the last reported 1s window; the only figures AUTO is allowed to see
    pub iv_loss_pct: f64,
    pub iv_jitter_ms: f64,
}

pub(crate) struct TxShared {
    pub rung: AtomicU32,
    pub rung_changes: AtomicU32,
    pub sent_packets: AtomicU64,
    pub sent_bytes: AtomicU64,
    pub ka_count: AtomicU64,
    /// keepalives dropped because their source IP was not the control-TCP peer
    pub ka_rejected: AtomicU64,
    ka_warned: AtomicBool,
    pub created: Instant,
    pub remote: Mutex<RemoteStats>,
    /// Receiver PORT learned from its keepalives; the IP always stays the
    /// control-TCP peer's (spec-m4a §3 freezes the destination IP).
    pub dest_override: Mutex<Option<SocketAddr>>,
}

impl TxShared {
    pub(crate) fn new() -> TxShared {
        TxShared {
            rung: AtomicU32::new(0),
            rung_changes: AtomicU32::new(0),
            sent_packets: AtomicU64::new(0),
            sent_bytes: AtomicU64::new(0),
            ka_count: AtomicU64::new(0),
            ka_rejected: AtomicU64::new(0),
            ka_warned: AtomicBool::new(false),
            created: Instant::now(),
            remote: Mutex::new(RemoteStats::default()),
            dest_override: Mutex::new(None),
        }
    }

    /// True the first time only: keepalive spoofing is logged once per stream
    /// so a flood cannot turn stderr into the amplifier.
    pub(crate) fn first_ka_warning(&self) -> bool {
        !self.ka_warned.swap(true, Ordering::Relaxed)
    }
}

// ---------------------------------------------------------------- reporting

pub(crate) fn snapshot_sessions(inner: &DaemonInner) -> Vec<SessionEntry> {
    let st = lk(&inner.state);
    let mut v: Vec<SessionEntry> = st.sessions.values().cloned().collect();
    drop(st);
    v.sort_by_key(|e| e.id);
    v
}

pub(crate) fn build_session_infos(inner: &DaemonInner) -> Vec<SessionInfo> {
    let entries = snapshot_sessions(inner);
    let mut mix_freqs: Vec<f32> = Vec::new();
    for e in &entries {
        if e.kind == KIND_SPK && e.dir == DIR_RECV {
            if let Some(f) = e.rx.as_ref().and_then(|r| r.verify_freq) {
                if !mix_freqs.iter().any(|x| x.to_bits() == f.to_bits()) {
                    mix_freqs.push(f);
                }
            }
        }
    }
    let mix_snap: Option<Vec<f32>> = if mix_freqs.is_empty() {
        None
    } else {
        Some(lk(&inner.mix_ring).iter().copied().collect())
    };
    entries
        .iter()
        .map(|e| build_session_info(e, &mix_freqs, mix_snap.as_deref()))
        .collect()
}

pub(crate) fn build_session_info(
    e: &SessionEntry,
    mix_freqs: &[f32],
    mix_snap: Option<&[f32]>,
) -> SessionInfo {
    let mut s = SessionStats {
        received: 0,
        lost: 0,
        loss_pct: 0.0,
        jitter_ms: 0.0,
        bitrate_kbps: 0.0,
        jb_depth_frames: 0,
        sent_packets: 0,
        rung: 0,
        rung_changes: 0,
        verdict: None,
        mix_verdicts: None,
        volume: if e.volume.enabled { *lk(&e.volume.state) } else { None },
    };
    if let Some(rx) = &e.rx {
        {
            let c = lk(&rx.stats);
            let dur = c.first.map(|f| f.elapsed().as_secs_f64()).unwrap_or(0.0);
            let sm = c.rx.summary(dur);
            s.received = sm.received;
            s.lost = sm.lost;
            s.loss_pct = sm.loss_pct;
            s.jitter_ms = sm.jitter_ms;
            s.bitrate_kbps = sm.bitrate_kbps;
            s.rung = AUTO_RATES
                .iter()
                .position(|&r| r == c.last_rate)
                .unwrap_or(0) as u32;
        }
        s.jb_depth_frames = lk(&rx.jbs).jb.depth();
        if let (Some(f), Some(ring)) = (rx.verify_freq, rx.ring.as_ref()) {
            let snap: Vec<f32> = lk(ring).iter().copied().collect();
            s.verdict = Some(dsp::verify_tone(&snap, 48000, f));
        }
        if e.kind == KIND_SPK && e.dir == DIR_RECV {
            if let Some(mix) = mix_snap {
                s.mix_verdicts = Some(
                    mix_freqs
                        .iter()
                        .map(|&f| engine::mix_tone_verdict(mix, 48000, f))
                        .collect(),
                );
            }
        }
    }
    if let Some(tx) = &e.tx {
        s.sent_packets = tx.sent_packets.load(Ordering::Relaxed);
        s.rung = tx.rung.load(Ordering::Relaxed);
        s.rung_changes = tx.rung_changes.load(Ordering::Relaxed);
        let r = *lk(&tx.remote);
        // lifetime totals for display; loss_pct is derived from them so the UI
        // still shows a session-wide figure while AUTO reacts to the last window
        s.received = r.received;
        s.lost = r.lost;
        let expected = r.received + r.lost;
        s.loss_pct = if expected > 0 {
            r.lost as f64 * 100.0 / expected as f64
        } else {
            0.0
        };
        s.jitter_ms = r.iv_jitter_ms;
        let el = tx.created.elapsed().as_secs_f64().max(1e-3);
        s.bitrate_kbps = tx.sent_bytes.load(Ordering::Relaxed) as f64 * 8.0 / el / 1000.0;
    }
    SessionInfo {
        id: e.id,
        peer_fingerprint: e.conn.peer.fingerprint.clone(),
        peer_name: e.conn.peer.name.clone(),
        kind: e.kind.clone(),
        dir: e.dir.clone(),
        sample_rate: 48000,
        channels: 1,
        stats: s,
    }
}

// ---------------------------------------------------------------- ticker

/// 1s cadence: receiver keepalives + Stats over control, sender-side AUTO.
fn ticker_loop(inner: Arc<DaemonInner>) {
    struct AutoCell {
        ladder: AutoLadder,
        last_seq: u64,
    }
    let mut autos: HashMap<u32, AutoCell> = HashMap::new();
    let mut dev_epoch = inner.dev_out_epoch.load(Ordering::Relaxed);
    loop {
        for _ in 0..5 {
            if inner.shutdown.load(Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(Duration::from_millis(200));
            // On the sub-tick, not the 1s one: a slider must not feel like it
            // lags a second behind the hand. Never on a media loop — this
            // writes the control socket and sends mach messages (§B2).
            hal_tick(&inner);
        }
        // spec-m4c §D: the output device the consumer's slider drives is a
        // different device now, so every volume_sync'd spk session must be told
        // what the NEW device reads (and whether it is adjustable at all).
        let renegotiate = {
            let e = inner.dev_out_epoch.load(Ordering::Relaxed);
            let changed = e != dev_epoch;
            dev_epoch = e;
            changed
        };
        conn::ping_and_reap(&inner);
        {
            let mut st = lk(&inner.state);
            if st
                .pairing
                .as_ref()
                .map_or(false, |p| Instant::now() >= p.until)
            {
                st.pairing = None;
            }
        }
        let entries = snapshot_sessions(&inner);
        autos.retain(|id, _| entries.iter().any(|e| e.id == *id && e.tx.is_some()));
        for e in &entries {
            let live = e.conn.alive.load(Ordering::SeqCst);
            if let Some(rx) = &e.rx {
                engine::send_pullreq(&inner, rx);
                // per-interval deltas, not lifetime totals: one early dropout
                // must not pin the sender to the lowest rung forever
                let iv = lk(&rx.stats).take_interval();
                if let (true, Some(iv)) = (live, iv) {
                    let _ = e.conn.send_msg(&SessionMsg::Stats {
                        stream_id: e.id,
                        received: iv.received,
                        lost: iv.lost,
                        loss_pct: iv.loss_pct,
                        jitter_ms: iv.jitter_ms,
                    });
                }
            }
            if let Some(tx) = &e.tx {
                let r = *lk(&tx.remote);
                let cell = autos.entry(e.id).or_insert_with(|| AutoCell {
                    ladder: AutoLadder::new(),
                    last_seq: 0,
                });
                if r.seq > cell.last_seq {
                    cell.last_seq = r.seq;
                    if let Some(new_rung) = cell.ladder.feed_stats(r.iv_loss_pct, r.iv_jitter_ms) {
                        tx.rung.store(new_rung, Ordering::Relaxed);
                    }
                    tx.rung_changes
                        .store(cell.ladder.rung_changes, Ordering::Relaxed);
                }
            }
            // spk provider: our real output device is the thing the consumer's
            // slider drives, so we are the only side that can observe it
            if e.volume.enabled && e.kind == KIND_SPK && e.dir == DIR_RECV {
                if renegotiate {
                    dlog!(
                        "[audiohubd] stream {}: default output changed, re-reporting VolumeState",
                        e.id
                    );
                }
                poll_provider_volume(e, live, renegotiate);
            }
        }
    }
}

/// Ticks between unconditional VolumeState refreshes. Changes go out
/// immediately; the refresh only exists so a consumer that missed one (or
/// joined after the last change) converges without touching its slider.
const VOLUME_REFRESH_TICKS: u32 = 5;

/// Provider side of a volume_sync'd spk stream (spec-m4b §A2): read the real
/// default output device, cache it for SessionStats, and report GENUINE local
/// changes back. A reading that merely echoes what the peer just asked for is
/// swallowed by VolumeSync — that is the anti-ping-pong rule.
///
/// `force` (a default-device change) sends the reading even when it did not
/// move: the consumer's cached state belongs to the OLD device.
fn poll_provider_volume(e: &SessionEntry, live: bool, force: bool) {
    let cur = match volume::get_default_output_volume() {
        Ok(v) => v,
        Err(err) => {
            if e.volume.first_read_warning() {
                dlog!(
                    "[audiohubd] stream {}: cannot read the default output volume: {err:#}",
                    e.id
                );
            }
            return;
        }
    };
    *lk(&e.volume.state) = Some(cur);
    if !live {
        return; // nothing to report to; leave the tracker armed for a reconnect
    }
    let ticks = e.volume.since_report.fetch_add(1, Ordering::Relaxed) + 1;
    let mut sync = lk(&e.volume.sync);
    if sync.poll(cur).is_none() && ticks < VOLUME_REFRESH_TICKS && !force {
        return;
    }
    sync.note_reported(cur);
    drop(sync);
    e.volume.since_report.store(0, Ordering::Relaxed);
    let _ = e.conn.send_msg(&SessionMsg::VolumeState {
        stream_id: e.id,
        scalar: cur.scalar,
        muted: cur.muted,
        adjustable: cur.adjustable,
    });
}

// ---------------------------------------------------------------- plumbing

fn bind_control_media(port: u16) -> Result<(TcpListener, UdpSocket, u16)> {
    if port != 0 {
        let tcp =
            TcpListener::bind(("0.0.0.0", port)).with_context(|| format!("bind tcp :{port}"))?;
        let udp =
            UdpSocket::bind(("0.0.0.0", port)).with_context(|| format!("bind udp :{port}"))?;
        return Ok((tcp, udp, port));
    }
    let mut last: Option<std::io::Error> = None;
    for _ in 0..16 {
        let tcp = TcpListener::bind(("0.0.0.0", 0)).context("bind tcp :0")?;
        let p = tcp.local_addr()?.port();
        match UdpSocket::bind(("0.0.0.0", p)) {
            Ok(udp) => return Ok((tcp, udp, p)),
            Err(e) => last = Some(e),
        }
    }
    bail!(
        "no matching tcp/udp port pair after 16 tries: {}",
        last.map(|e| e.to_string()).unwrap_or_default()
    )
}

fn gen_token() -> String {
    use rand_core::RngCore;
    let mut b = [0u8; 24];
    rand_core::OsRng.fill_bytes(&mut b);
    BASE64_URL_SAFE_NO_PAD.encode(b)
}

fn read_ipc_json(dir: &Path) -> Option<IpcEndpoint> {
    let bytes = std::fs::read(dir.join("ipc.json")).ok()?;
    serde_json::from_slice::<IpcEndpoint>(&bytes).ok()
}

/// True if the process that wrote ipc.json is still running.
fn owner_alive(ep: &IpcEndpoint) -> bool {
    #[cfg(unix)]
    {
        let _ = ep;
        // signal 0 only tests existence + permission; EPERM means the pid is
        // live but owned by another user
        unsafe {
            if libc::kill(ep.pid as libc::pid_t, 0) == 0 {
                return true;
            }
        }
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        // no cheap pid probe here: ask the recorded endpoint instead
        std::net::TcpStream::connect_timeout(
            &SocketAddr::from(([127, 0, 0, 1], ep.port)),
            Duration::from_millis(200),
        )
        .is_ok()
    }
}

/// Refuse to overwrite a live daemon's endpoint file: two daemons sharing a
/// config dir silently hijack each other's ipc.json and either exit deletes it.
fn ensure_endpoint_unowned(dir: &Path) -> Result<()> {
    let Some(ep) = read_ipc_json(dir) else { return Ok(()) };
    if owner_alive(&ep) {
        bail!(
            "another audiohubd (pid {}, ipc port {}) already owns {}; stop it first \
             (delete the file only if that process is gone)",
            ep.pid,
            ep.port,
            dir.join("ipc.json").display()
        );
    }
    Ok(())
}

fn remove_ipc_json_if_ours(dir: &Path) {
    let path = dir.join("ipc.json");
    match read_ipc_json(dir) {
        Some(ep) if ep.pid != std::process::id() => {
            dlog!(
                "[audiohubd] leaving {} in place: it now belongs to pid {}",
                path.display(),
                ep.pid
            );
        }
        _ => {
            let _ = std::fs::remove_file(&path);
        }
    }
}

fn write_ipc_json(dir: &Path, port: u16, token: &str) -> Result<()> {
    use std::io::Write;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let ep = IpcEndpoint {
        ipc_version: IPC_VERSION,
        port,
        token: token.to_string(),
        pid: std::process::id(),
    };
    let path = dir.join("ipc.json");
    // randomized name + create_new: the token must never exist at a guessable
    // path, and never be readable by anyone else, even briefly
    let tmp = dir.join(format!("ipc.json.{}.tmp", gen_token()));
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts
            .open(&tmp)
            .with_context(|| format!("create {}", tmp.display()))?;
        if let Err(e) = f
            .write_all(serde_json::to_string_pretty(&ep)?.as_bytes())
            .and_then(|()| f.sync_all())
        {
            drop(f);
            let _ = std::fs::remove_file(&tmp);
            return Err(e).with_context(|| format!("write {}", tmp.display()));
        }
    }
    if let Err(e) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("rename to {}", path.display()));
    }
    Ok(())
}
