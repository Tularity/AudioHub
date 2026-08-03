//! audiohubd — daemon assembly (spec-m4a §1/§4).
//! Frozen lib entry: `DaemonCfg` / `DaemonHandle` / `start_daemon`.

mod conn;
mod engine;
pub mod haldev;
pub mod halbridge;
/// The Windows control-plane contract and transport. The `wire` half is
/// compiled EVERYWHERE so its encoding is tested on the machine this is
/// developed on, not only on the target.
pub mod halbridge_win;
mod ipcserv;
/// plan §13 三模式互斥的接线测试（两台真 daemon 跑回环）。
#[cfg(test)]
mod mode_tests;
mod quality;
pub mod reconnect;
mod settings;

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
use audiohub_core::latency::{
    DevLatency, DriftTracker, DropMode, StageDepth, StageId, StageSlot,
};
use audiohub_core::sysaudio::{self, VirtualCard};
use audiohub_core::volume::{self, VolumeState, VolumeSync};
use audiohub_ipc::{
    IpcEndpoint, LatConfidence, MixHealth, OpenSessionParams, PipelineLatency, PipelineStage,
    QualityStats, SessionInfo, SessionStats, IPC_VERSION, KIND_SPK, ORIGIN_HAL, ORIGIN_PEER,
    ORIGIN_USER,
};
use audiohub_net::discovery::{self, AnnounceGuard};
use audiohub_net::identity::{LocalIdentity, PairedPeer};
use audiohub_net::media::{AutoLadder, JitterBuffer, MediaCrypto, AUTO_RATES};
use audiohub_net::secure::{SecureChannel, SessionMsg, StageReading};
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
///
/// **每行带进程单调时间戳**（`[   1234.567]`，秒，原点 = 本进程第一次记日志）。
/// 没有它，日志只剩下**顺序**而没有**时刻**：上一轮排查 `hal_spk` 欠载时，
/// 33 条跳 tick 记录与 30 次欠载谁先谁后、隔了多久，在一份 21 小时的日志里
/// 完全无法回答，只能从头再跑一遍。时间戳与 IPC 的 `uptime_s` 同一条时基
/// （两者都是进程启动后的单调秒），所以日志行可以直接和外部采样对齐。
pub fn logln(args: std::fmt::Arguments<'_>) {
    use std::io::Write;
    // one write per line: interleaved threads must not tear a line apart
    let mut line = format!("[{:11.3}] ", log_uptime().as_secs_f64());
    use std::fmt::Write as _;
    let _ = line.write_fmt(args);
    line.push('\n');
    let _ = std::io::stderr().write_all(line.as_bytes());
}

/// 日志时间戳的原点。第一次记日志时钉住，之后只读——比 `Instant::now()` 每行
/// 取一次系统时间便宜，也让「第 0 秒」在日志里有确定含义。
fn log_uptime() -> std::time::Duration {
    static T0: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    T0.get_or_init(Instant::now).elapsed()
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

    /// The daemon's own state, for in-crate tests that need to drive the IPC
    /// dispatcher without standing up a WebSocket client.
    #[cfg(test)]
    pub(crate) fn inner_for_test(&self) -> &Arc<DaemonInner> {
        &self.inner
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
    // Named before it is moved: the line used to print the DEFAULT service name
    // whatever `AUDIOHUB_HAL_SERVICE` said, which makes it useless for the one
    // thing it is read for — checking that the daemon is looking for the driver
    // you just installed.
    let hal_name = hal_cfg.service_name.clone();
    let hal_bridge = match halbridge::HalBridge::start(hal_cfg) {
        Ok(b) => {
            if let Some(br) = &b {
                let st = br.status();
                dlog!("hal bridge: driver_found={} name={hal_name}", st.driver_found);
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
    let cfg_dir_for_state = cfg_dir.clone();
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
        settings: Mutex::new(settings::StoredSettings::load(&cfg_dir_for_state)),
        haldev: Mutex::new(haldev::HalDevState::new(haldev::SlotTable::load(
            &cfg_dir_for_state,
        ))),
        hal_sess: Mutex::new(None),
        hal_mic_io: std::array::from_fn(|_| AtomicBool::new(true)),
        preauth: AtomicUsize::new(0),
        recon: Mutex::new(HashMap::new()),
        dev_in_epoch: AtomicU64::new(0),
        dev_out_epoch: AtomicU64::new(0),
        devices: Mutex::new(None),
        play_ring: StageSlot::new(),
        play_drift: Mutex::new(DriftTracker::new()),
        mix_clip: quality::ClipMeter::new(),
        mix_meter: quality::MixMeter::new(),
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
    // Only where there IS a bridge: on every other host these two threads would
    // wake 5 times a second to find nothing, forever.
    if inner.hal().is_some() {
        let (sess_tx, sess_rx) = mpsc::channel::<haldev::SessCmd>();
        *lk(&inner.hal_sess) = Some(sess_tx.clone());
        let i = inner.clone();
        threads.push(spawn(
            "ahb-haldev",
            Box::new(move || haldev::coordinator_loop(i, sess_tx)),
        )?);
        let i = inner.clone();
        threads.push(spawn(
            "ahb-halsess",
            Box::new(move || haldev::session_worker(i, sess_rx)),
        )?);
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
    /// Written to ipc.json so clients can find us; nothing in the daemon reads
    /// it back. It was briefly live while the daemon served the web UI itself —
    /// that moved to the App (plan §7.5), so the allow is correct again.
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
    /// Daemon-owned settings (spec-m5b §6.1). The consumer mode in particular
    /// is a property of this MACHINE, so it cannot live in a UI's localStorage
    /// where two windows can disagree and the daemon is never told.
    pub settings: Mutex<settings::StoredSettings>,
    /// Which peer owns which pair of virtual devices, plus everything the
    /// device/session coordinator tracks per slot (spec-m5b §5.1).
    pub haldev: Mutex<haldev::HalDevState>,
    /// Command queue to the session worker. `None` where there is no bridge.
    pub hal_sess: Mutex<Option<mpsc::Sender<haldev::SessCmd>>>,
    /// Is an application actually reading slot N's virtual microphone? Driven
    /// by the driver's IoState reports (which it replays on every idempotent
    /// re-Set, so this re-syncs by itself). It gates the mixer's ring writes
    /// for a latency reason, not a correctness one: only the ring's CONSUMER
    /// may move read_idx, so a ring we fill while nobody drains it stays full,
    /// and the app that eventually starts recording would then read 500ms
    /// behind us — permanently. Starts `true` because "not told yet" must never
    /// mean silence: writes made before the driver attaches are dropped by the
    /// handshake flush on its side anyway.
    ///
    /// PER SLOT: one flag would let an app recording peer A's microphone open
    /// the write path for every other peer's ring as well.
    pub hal_mic_io: [AtomicBool; haldev::HAL_MAX_SLOTS],
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
    /// 真实默认输出的播放环深度（规格 §3.2 的级 8 `play_ring`），由混音线程
    /// 每 10 ms 发布。
    ///
    /// 挂在 daemon 上而不是每条流上，因为**它就是一个环**：所有送本机扬声器
    /// 的流共用同一个 `AudioTx`。多条流报出同一个读数不是重复，是物理事实
    /// （规格 §7.2 R7）——但同样地，**不可跨流求和**。
    ///
    /// 这一级是全链路唯一「丢最新」且此前**完全无遥测**的丢弃点：
    /// `let _ = prod.push_slice(..)` 静默丢尾、零计数、零日志。
    pub play_ring: StageSlot,
    /// 播放环深度的 30 s 漂移窗口（站点级，同上）。
    pub play_drift: Mutex<DriftTracker>,
    /// 求和**之后**的削顶（站点级，不可归属到会话）。三个计入的调用点见
    /// `engine::mixer_loop`；`push_mix` 那个探针 tap **不计入**（规格 §0.6）。
    pub mix_clip: quality::ClipMeter,
    /// 混音形态：同时混入几路、前两路是不是同一份内容。
    pub mix_meter: quality::MixMeter,
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

// The single-pair volume relay that used to live here (`hal_tick`,
// `drain_hal_events`, `hal_push_peer_volume` and the one global `hal_vol`
// cell) is gone: every one of them assumed exactly one virtual device pair.
// The per-slot versions are in haldev.rs, on the coordinator's own thread —
// the forward relay now filters by the peer that OWNS the slot, which the old
// fan-out did not do at all (it drove every volume_sync'd spk session, so with
// two peers bound, one slider moved both machines).

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
            // The three headline counters are the SUMS of the per-slot ones in
            // `devices`, so a client that only reads them sees what it always
            // did while a client that wants per-device detail has it.
            spk_frames: s.spk_frames,
            mic_frames: s.mic_frames,
            mic_dropped: s.mic_dropped,
            last_driver_msg_secs: s.last_driver_msg_secs,
            protocol_version: halbridge::PROTOCOL_VERSION,
            driver_protocol_version: s.driver_protocol_version,
            status_reason: s.status_reason.clone(),
            bind_failures: s.bind_failures,
            last_bind_error: s.last_bind_error.clone(),
            endpoint_name_fallbacks: s.endpoint_name_fallbacks,
            devices: lk(&inner.haldev).device_infos(&s.slots),
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
        obj.insert("latency_guard".to_string(), latency_guard_status(inner)?);
    }
    Ok(v)
}

/// 棘轮治理（治法 A + B）的现场读数。**必须经 IPC 暴露**：这一整类病的特征就是
/// 「除了水位读数本身没有一个数字会动」，上一轮花了整轮调查才把它挖出来。埋点留在
/// 进程里不导出，等于下一次复发时又得重来一遍。
///
/// `hal` 里那份 `audiohub_ipc::HalStatus` 是**发布过的**结构，它的字段被
/// `test/tests/hal_wiring.rs` 冻结；trim/underrun/skip 是新量，挂在自己的键下，
/// 一个字段都不动老结构。
fn latency_guard_status(inner: &DaemonInner) -> Result<serde_json::Value> {
    let hal = inner.hal().map(|h| {
        let s = h.status();
        serde_json::json!({
            "trim": s.trim,
            "underrun": s.underrun,
            "skip_drained_frames": s.skip_drained_frames,
        })
    });
    Ok(serde_json::json!({
        "skip": {
            "tx": engine::tx_skip_counters(),
            "mixer": engine::mixer_skip_counters(),
        },
        // 发送侧：`tx_loop` 唤醒周期的二阶 DLL（`halbridge::dll`）。它是
        // `hal_spk` 水位的**常规执行器**，`trim` 只是它够不着那一档的兜底。
        //
        // 现场怎么读这四个数：
        // - `corr_ppm` 长期贴在 +500 或 −500 ⇒ 要么真有一大笔存量在被斜坡排空
        //   （几分钟内应当回落），要么**误差符号写反了**（永不回落，且 `hal_spk`
        //   同向发散）。这两种情形靠「`corr_ppm` 是否随时间回到 0 附近」区分。
        // - 稳态下 `clamped` 仍在涨 ⇒ 观测噪声已经超出 ±0.8 ms 的线性区，
        //   环路一直工作在压摆率限制段。
        // - `resyncs` 涨得快 ⇒ 跳 tick / 驱动重附着在反复发生，病不在环路里。
        // - `bw_hz` 应当在开流约 4 s 后从 0.5 落到 0.05；一直停在 0.5 说明
        //   `resync` 被反复触发（与 `resyncs` 互相印证）。
        //
        // 没有这一项就没法判断环路到底在不在工作——`hal_spk.ms` 平稳既可能是
        // 环路在起作用，也可能是这段时间恰好没有扰动。
        "dll": engine::tx_dll_counters(),
        "hal_spk": hal,
        // 接收侧：`play_ring` 的跨时钟速率伺服。这一级是全链路上**唯一真正
        // 跨时钟**的一段（mac 的发送节拍 vs Windows 声卡晶振，两个独立振荡器），
        // 也是治法 A 落地之后唯一还在无界积累的病灶。
        //
        // 现场怎么读这三个数：
        // - `corr_ppm` 稳态值 ≈ **两端晶振的实际失配**。它本身就是一个硬件读数。
        // - `clamped` 持续增长 ⇒ 失配超出 ±500 ppm 可校正范围，该查设备而不是调参。
        // - `resync_events` / `dev_underruns` 持续增长 ⇒ 目标水位定低了，
        //   或者上游（JB / mixer）在周期性卡顿，环路只是在替它们收拾。
        //
        // 口径警告（进程级聚合、瞬时字段最后写者赢）见
        // `audiohub_core::audio::PlayServoCounters` 的文档。
        "play_servo": audio::play_servo_counters(),
    }))
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
    /// P1：本条控制通道的 Ping/Pong 时钟滤波器（min-RTT 窗口 + 时钟偏移 θ）。
    ///
    /// 挂在**连接**上而不是会话上：θ 与 RTT 是两台主机之间的属性，一条连接上
    /// 的 N 条会话共用同一份估计。这与「分项按流」（R8）不矛盾——分项是队列，
    /// 队列属于流；时钟属于主机。
    pub(crate) clock: Mutex<ClockFilter>,
    /// One stderr line per connection for Pongs we could not have caused.
    clock_warned: AtomicBool,
    /// What the peer last said its mode is (plan §13 推论 1), from
    /// `SessionMsg::ModeState`.
    ///
    /// Lives on the CONNECTION, not on the peer record, and is therefore gone
    /// the moment the channel is: a mode remembered across a disconnect is a
    /// claim about the past, and this value is only ever used to decide what to
    /// offer the user *now*. Persisting it would produce the one failure this
    /// field exists to prevent, just delayed — an entry that says "usable"
    /// about a machine that has since become a consumer.
    pub(crate) peer_mode: Mutex<PeerModeCell>,
}

/// The peer's advertised mode, with "unknown" and "unrecognised" kept apart.
///
/// They are different answers to different questions and they need opposite
/// treatment in the UI: nothing advertised yet ⇒ say nothing; a mode we cannot
/// name ⇒ do not offer the peer. Collapsing them into `Option<Mode>` would force
/// one of the two to be wrong, and the wrong one would be silent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum PeerModeCell {
    /// No `ModeState` has arrived on this channel yet.
    #[default]
    Unheard,
    Known(audiohub_ipc::Mode),
    /// The peer named a mode this build does not define. Only reachable from a
    /// hand-built frame — the protocol version is equality-checked, so a peer
    /// that got this far runs this build — but it is still represented rather
    /// than folded into `Unheard`, because "it told us something we could not
    /// read" argues for caution and "it has not spoken yet" does not.
    Unrecognised,
}

impl PeerModeCell {
    pub(crate) fn mode(self) -> Option<audiohub_ipc::Mode> {
        match self {
            PeerModeCell::Known(m) => Some(m),
            _ => None,
        }
    }

    /// True only when the peer has actually told us it cannot serve. `Unheard`
    /// is NOT unusable: an offline peer, or one whose first advertisement is
    /// still in flight, must not be painted as refusing.
    pub(crate) fn unusable(self) -> bool {
        match self {
            PeerModeCell::Known(m) => !m.serves_peers(),
            PeerModeCell::Unrecognised => true,
            PeerModeCell::Unheard => false,
        }
    }
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

    /// True the first time only: a peer that floods nonsense Pongs must not be
    /// able to turn stderr into the amplifier (same rule as `first_ka_warning`).
    pub(crate) fn first_clock_warning(&self) -> bool {
        !self.clock_warned.swap(true, Ordering::Relaxed)
    }
}

// -------------------------------------------------- P1：时钟偏移与网络单程

/// 滤波窗口长度（样本数）。1 s 的 ping 节拍 ⇒ 16 s 历史。
///
/// min-RTT 估计的品质随窗口变长而变好（窗口越长，撞上一个「队列全空」的瞬间
/// 的机会越大，那个样本的 θ 最干净），但窗口太长会让一次真实的路径切换迟迟
/// 反映不出来。16 是这两头之间的取舍。
const CLOCK_WINDOW: usize = 16;

/// 出结论所需的最小样本数（规格 §3.3 明确要求 ≥8）。
///
/// 在此之前 `estimate()` 返回 `None`：**`net_ms` 与 θ 一起等**。
/// 理由是红线——RTT 只能是六段里最小的那一段，而 min-RTT 在样本少时**系统性
/// 偏大**（min 随样本增加只会下降，不会上升）。宁可这 8 秒里报
/// `Converging`（UI：测量中），也不把一个偏大的网络段塞进总数。
const CLOCK_MIN_SAMPLES: usize = 8;

/// 超过这个 RTT 的样本直接丢弃：对端回抄了一个我们不可能发过的 `t_us`
/// （或时钟出了大事）。控制面 ping 走的是同一条已建立的 TCP，2 秒的往返
/// 已经远在「这条连接还活着」的判据（`CONTROL_SILENCE_LIMIT` = 5 s）之内。
const CLOCK_MAX_RTT_US: u64 = 2_000_000;

/// θ 阶跃门限：与当前估计差过这个数就清窗重来（规格 §5 P1 列的自检）。
/// 典型触发场景是对端 daemon 重启（时基从 0 重新开始）或系统休眠恢复。
const CLOCK_STEP_US: i64 = 50_000;

#[derive(Clone, Copy)]
struct RttSample {
    /// t4 − t1，**全程本机时基**。
    rtt_us: u64,
    /// θ = (t1 + t4)/2 − t2。`None` = 对端是 P1 之前的版本，只贡献了 RTT。
    theta_us: Option<i64>,
}

/// 一次成立的时钟/网络估计。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ClockEstimate {
    /// 窗口内最小 RTT（本机时基）。`net_ms` = 它的一半。
    pub min_rtt_us: u64,
    /// 最新一个样本的 RTT，用于交叉校验（规格 §6.4）。
    pub last_rtt_us: u64,
    /// θ = (t1 + t4)/2 − t2，单位 µs。**「对端时戳 + θ = 本机时戳」**。
    /// `None` = 对端不报 `peer_t_us`（P1 之前的版本）。
    pub offset_us: Option<i64>,
    /// θ 的不确定度上界 = min_RTT / 2（规格 §3.4）。
    pub unc_us: u32,
}

/// 控制面 Ping/Pong 的 min-RTT 滤波器（规格 §3.3 P1 第 2 条）。
///
/// # 单一时基纪律（这是本文件最容易再次踩进去的坑）
///
/// 上一轮排查栽在「两个不一致时基相除」上：系统误差底 143 ppm，比待测效应还大。
/// 所以这里把规矩写死：
///
/// 1. **`t1` 与 `t4` 都取自 `DaemonInner::start`**，一个 `Instant` 基准。
///    `rtt = t4 − t1` 因此是纯本机量，任何对端行为都改变不了它的时基。
///    `t1` 不需要我们记账——`Pong` 原样回抄，省掉一张待答表。
/// 2. **`t2` 是对端时基**，全daemon只有 θ 这一处允许两个时基相遇，而 θ 的定义
///    本身就是「两个时基之差」——这是它的用途，不是它的缺陷。
/// 3. **两个时基之间只做差，绝不做商。** 任何「用两次测量之差推算速率/漂移」
///    的写法都禁止（规格 §3.4 也明令禁止用连续两次测量之差推算时钟漂移）：
///    分子分母来自不同时基时，商里会混进两个基准的相对速率，而那正是 143 ppm
///    的来源。本结构里没有任何除法涉及对端时戳——`unc_us` 除的是 `min_rtt_us`
///    （本机量），`net_ms` 除的也是它。
/// 4. **读数年龄不在这里算**，在 `PeerLatCell` 里用本机 `Instant` 量。拿本机
///    `now_us − peer_seq_us` 会得到两个 daemon 启动时刻之差，一个长得很像
///    「年龄」的垃圾数。
pub(crate) struct ClockFilter {
    win: VecDeque<RttSample>,
}

/// 一条 `Pong` 的去向。返回给调用方而不是就地 `dlog!`：这个结构不持有对端指纹，
/// 而且「样本被丢了」必须是**可断言**的——静默丢弃会让 `Converging` 永远挂着，
/// 却没有任何东西说得出为什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PongOutcome {
    /// 样本进窗。
    Ok,
    /// 时戳不合理（负 RTT，或往返超过 `CLOCK_MAX_RTT_US`），已丢弃。
    Implausible,
    /// θ 相对当前估计发生了阶跃：整窗作废，本样本成为新窗的第一个。
    Stepped,
}

impl ClockFilter {
    pub(crate) fn new() -> ClockFilter {
        ClockFilter { win: VecDeque::new() }
    }

    /// 收到一条 `Pong`。三个时戳的含义见结构体文档。
    ///
    /// `t1_us` 是对端**回抄**给我们的值，所以要先做合理性检查：负 RTT 或荒谬
    /// 大的 RTT 一律丢弃。注意这里不做「防对端谎报」的强保证——一个已配对的
    /// 对端本来就能对自己的分项深度撒谎；而谎报 RTT 只能把 min 拉**低**，
    /// 即让网络段更小，这是保守方向（红线要求 RTT 是最小的一段）。
    pub(crate) fn note_pong(
        &mut self,
        t1_us: u64,
        t4_us: u64,
        peer_t2_us: Option<u64>,
    ) -> PongOutcome {
        let Some(rtt_us) = t4_us.checked_sub(t1_us) else {
            return PongOutcome::Implausible;
        };
        if rtt_us > CLOCK_MAX_RTT_US {
            return PongOutcome::Implausible;
        }
        // (t1 + t4)/2 写成 t1 + rtt/2：同一个数，但不会因为两个大时戳相加溢出。
        let mid_us = t1_us + rtt_us / 2;
        let theta_us = peer_t2_us.map(|t2| mid_us as i64 - t2 as i64);
        // 阶跃自检：对端 daemon 重启会让它的时基从 0 重来，θ 整体平移。窗口里
        // 混着新旧两批 θ 时，min-RTT 挑中哪一批全看运气——必须整窗作废。
        let mut outcome = PongOutcome::Ok;
        if let (Some(new), Some(cur)) = (theta_us, self.estimate().and_then(|e| e.offset_us)) {
            if (new - cur).abs() > CLOCK_STEP_US {
                self.win.clear();
                outcome = PongOutcome::Stepped;
            }
        }
        if self.win.len() == CLOCK_WINDOW {
            self.win.pop_front();
        }
        self.win.push_back(RttSample { rtt_us, theta_us });
        outcome
    }

    /// `None` = 样本还不够（<8），**什么都不报**。
    ///
    /// 挑 min-RTT 的那个样本，而不是取平均：排队延迟是**单边**噪声（只会让
    /// 往返变长，不会变短），所以最小的那次往返是排队最少、θ 最准的一次。
    /// 平均会把所有排队噪声原样吃进 θ。
    pub(crate) fn estimate(&self) -> Option<ClockEstimate> {
        if self.win.len() < CLOCK_MIN_SAMPLES {
            return None;
        }
        let best = self.win.iter().min_by_key(|s| s.rtt_us)?;
        Some(ClockEstimate {
            min_rtt_us: best.rtt_us,
            last_rtt_us: self.win.back()?.rtt_us,
            offset_us: best.theta_us,
            unc_us: (best.rtt_us / 2) as u32,
        })
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.win.len()
    }
}

/// WHO asked for this session (spec-m5b §5.6).
///
/// The distinction is load-bearing: the device coordinator closes a `Hal`
/// session when the application stops using the virtual device, and it must
/// never close a `User` one — a CLI or UI session belongs to whoever opened it,
/// and a device selection somewhere else on the machine is not consent to end
/// it. It runs the other way too: a `Hal` session exists because an application
/// selected a device, so the UI does not offer to close it either.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SessionOrigin {
    /// IPC (`session.open`), from the UI or the CLI.
    User,
    /// A virtual device started doing IO. `slot` is diagnostics only.
    Hal { slot: u8 },
    /// The peer opened it; we are the provider.
    Peer,
}

impl SessionOrigin {
    fn label(self) -> &'static str {
        match self {
            SessionOrigin::User => ORIGIN_USER,
            SessionOrigin::Hal { .. } => ORIGIN_HAL,
            SessionOrigin::Peer => ORIGIN_PEER,
        }
    }

    pub(crate) fn slot(self) -> Option<u8> {
        match self {
            SessionOrigin::Hal { slot } => Some(slot),
            _ => None,
        }
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
    /// `Some` = WE opened this session, and these are the exact params that
    /// would re-create it (spec-m4c §C). A peer-originated session is `None`:
    /// the peer re-opens it.
    ///
    /// 注意：`Some` 只说明「本机开的 + 这是它的参数」，**不代表**断线后该由
    /// `reconnect` 的通用重放机制救它——那个判断是
    /// `reconnect::recoverable_by_replay(origin)`。模式 B 的 `Hal` 会话有值，
    /// 但归设备协调器恢复；两边都恢复一次就会开出两路一模一样的流。参数在这里
    /// 仍然要留着：重放去重（`same_media_intent`）靠它认出「这条链路已经在线」。
    pub replay: Option<Arc<OpenSessionParams>>,
    pub origin: SessionOrigin,
    /// P0b：**这一条流**的对端分项。见 `PeerLatCell` 上的 R8 说明——格子按流走，
    /// 是那条「不可跨流求和」约束的结构性载体。`Arc` 是因为 `SessionEntry` 每
    /// 秒被克隆做快照（与 `volume` 同一理由）。
    pub peer_lat: Arc<PeerLatCell>,
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

// ------------------------------------------------- P0b：对端分项（每条流一份）

/// 中位数窗口长度。规格 §3.4 点名 **5 点中位数**：对端分项的采样时刻与本机不
/// 同步（最多晚 1 s），稳态下无影响，瞬态由它吸收。
const PEER_REPORT_WINDOW: usize = 5;

/// 超过这个年龄的对端读数不再是关于「现在」的证据。
///
/// 上报节拍是 1 s，所以 10 s = 连丢十拍。到这一步就整体退回 `LocalOnly`
/// （「对端未上报」），而不是继续拿一个十秒前的数去合成总和——那正是
/// 「用陈旧值冒充测量值」。注意这与 `peer_age_s > 3 ⇒ UI 标注陈旧` 是两层：
/// 3 s 是提醒，这里是**停止使用**。
const PEER_REPORT_MAX_AGE: Duration = Duration::from_secs(10);

/// 对端自报 `local_ms` 与本机重算值的容差。超过就说明两端的**求和口径**分了岔
/// （并行尾级取 max、`rate==0` 判缺项……），值得一条日志。
const PEER_SUM_MISMATCH_MS: f64 = 1.0;

/// 对端一次上报的落地形态。
struct PeerReport {
    /// ⚠ **本机**时基的到达时刻。年龄一律从这里量。
    ///
    /// 报文里的 `seq_us` 是对端时基，拿它和本机时钟相减得到的是两个 daemon
    /// 启动时刻之差——一个长得很像「年龄」的垃圾数，而且它恒定不变，所以
    /// 「陈旧」永远不会被触发。这一行是那个坑的封口。
    at: Instant,
    /// 对端时基。**只**与同一对端的其它 `seq_us` 比较（判乱序/重复）。
    seq_us: u64,
    stages: Vec<PipelineStage>,
    /// 本机按 `sum_stage_ms` **重算**的对端 Σ（不是对端自报的那个）。
    local_ms: Option<f64>,
    dev: Option<DevLatency>,
}

/// 取出去用的快照。
pub(crate) struct PeerLatSnapshot {
    pub stages: Vec<PipelineStage>,
    /// 窗口内的**中位数**，不是最新值（规格 §3.4）。
    pub local_ms: Option<f64>,
    pub dev: Option<DevLatency>,
    pub age_s: f64,
}

/// 一条流的对端分项（P0b）。
///
/// # R8：**按流一份，永不跨流求和**（规格 §7.2）
///
/// 扇出时一个 `SourceEnt` 被 N 条流引用，物理队列只有一份，于是 N 条流的
/// `src_fifo` 读数**相同**——正确的物理事实。由此得出的硬约束是：分项只能按
/// 流合成，把 N 张卡片的读数相加会得到 N 倍假延迟。
///
/// 这个约束在这里是**结构性**的，不是靠人记得：
///
/// - 格子挂在 `SessionEntry` 上（每条流一个 `Arc<PeerLatCell>`），**不是**挂在
///   `ConnShared` 上的一张 `HashMap<stream_id, _>`。没有那张表，就没有可以
///   `values().sum()` 的东西。
/// - 合成入口 `compose_sum_ms` 只收三个**标量**，收不了切片。
/// - 窗口用的是**中位数**而不是和：即便有人把同一条流的 5 次上报错当成 5 条流
///   的读数塞进来，中位数也只会给出一份的量（`peer_window_median` 上的测试
///   就是钉这一点的）。
pub(crate) struct PeerLatCell {
    win: Mutex<VecDeque<PeerReport>>,
    /// 求和口径分歧只报一次：上报节拍 1 s，否则 stderr 会被刷屏。
    mismatch_warned: AtomicBool,
}

impl PeerLatCell {
    pub(crate) fn new() -> PeerLatCell {
        PeerLatCell { win: Mutex::new(VecDeque::new()), mismatch_warned: AtomicBool::new(false) }
    }

    /// 落一条对端上报。返回 `Some(说明)` 表示两端求和口径对不上，值得记一条日志
    /// （只在第一次返回，之后恒 `None`）。
    ///
    /// `stages` 已由调用方转成 IPC 形状；`claimed_ms` 是对端自报的 Σ，**只作
    /// 交叉校验**：权威值是本机用同一个 `sum_stage_ms` 重算出来的那个。为什么
    /// 不直接信对端——三条规则（并行尾级取 max、`rate == 0` 判缺项、空列表判
    /// `None`）必须在**本机**执行，否则一个 `{rate:0, local_ms: 0.0}` 的报文就
    /// 能把「测不到」变成「没有延迟」。
    pub(crate) fn accept(
        &self,
        seq_us: u64,
        stages: Vec<PipelineStage>,
        claimed_ms: Option<f64>,
        dev: Option<DevLatency>,
    ) -> Option<String> {
        self.accept_at(Instant::now(), seq_us, stages, claimed_ms, dev)
    }

    /// `accept` 的全部内容，只是到达时刻由调用方给。
    ///
    /// 存在的唯一理由是**陈旧判定可测**：`PEER_REPORT_MAX_AGE` 是这套遥测里
    /// 「不拿旧数冒充现在」那条规矩的执行点，而它若只能靠 `Instant::now()`
    /// 触发，验证它就得等十秒——于是没人验证它。注意这不是 `#[cfg(test)]`
    /// 分支：生产路径就走这个函数，测试只是换了个入参。
    fn accept_at(
        &self,
        at: Instant,
        seq_us: u64,
        stages: Vec<PipelineStage>,
        claimed_ms: Option<f64>,
        dev: Option<DevLatency>,
    ) -> Option<String> {
        let local_ms = sum_stage_ms(&stages);
        let mut win = lk(&self.win);
        // 乱序/重复：`seq_us` 与窗口里的 `seq_us` 同属对端时基，这个比较合法。
        if win.back().map_or(false, |p| seq_us <= p.seq_us) {
            return None;
        }
        if win.len() == PEER_REPORT_WINDOW {
            win.pop_front();
        }
        win.push_back(PeerReport { at, seq_us, stages, local_ms, dev });
        drop(win);

        match (claimed_ms, local_ms) {
            (Some(claimed), Some(ours)) if (claimed - ours).abs() > PEER_SUM_MISMATCH_MS => {
                (!self.mismatch_warned.swap(true, Ordering::Relaxed)).then(|| {
                    format!(
                        "对端自报本侧 Σ={claimed:.1} ms，本机按同一份分项重算得 {ours:.1} ms；\
                         两端求和口径不一致（多半是对端有本版本不认识的级）。以本机重算值为准"
                    )
                })
            }
            _ => None,
        }
    }

    /// `None` = 没有可用的对端读数（从未收到，或全部超过 `PEER_REPORT_MAX_AGE`）
    /// ⇒ 上层保持 `LocalOnly`，**不猜、不用 RTT 顶替**。
    pub(crate) fn snapshot(&self) -> Option<PeerLatSnapshot> {
        let mut win = lk(&self.win);
        // 陈到不能用的整条丢掉。年龄用本机 `Instant`，见 `PeerReport::at`。
        while win.front().map_or(false, |p| p.at.elapsed() > PEER_REPORT_MAX_AGE) {
            win.pop_front();
        }
        let newest = win.back()?;
        // **最新一条测不到 ⇒ 整体测不到。** 中位数是抗瞬态的滤波，不是「拿旧
        // 的好数盖住新的坏数」的借口：对端此刻报不出深度，本机就不该报总和。
        let local_ms = newest.local_ms.and(peer_window_median(&win));
        Some(PeerLatSnapshot {
            stages: newest.stages.clone(),
            local_ms,
            dev: newest.dev,
            age_s: newest.at.elapsed().as_secs_f64(),
        })
    }
}

/// 窗口内 `local_ms` 的中位数（规格 §3.4 的 5 点中位数）。
///
/// **中位数，不是和**：窗口里的 5 条是**同一条流**在 5 个时刻的读数，把它们
/// 加起来就是 R8 那个 N 倍错误的时间轴版本。历史上测不到的点（`None`）跳过而
/// 不是让整体作废——「三秒前那一拍有个洞」不影响「现在这一拍是多少」；至于
/// 「现在这一拍有洞」，由 `snapshot` 里那个 `newest.local_ms.and(..)` 拦下。
fn peer_window_median(win: &VecDeque<PeerReport>) -> Option<f64> {
    let mut v: Vec<f64> = win.iter().filter_map(|p| p.local_ms).collect();
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = v.len();
    Some(if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    })
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
    /// Q1 的 10 s 非消费型窗口（规格 §4.6）。放在这里而不是 `JitterBuffer` 里，
    /// 是为了让 `audiohub-net` 保持纯累计——它不需要知道窗口的存在。
    pub conceal: quality::ConcealWindow,
}

impl JbState {
    /// 当前 JB 的五个 lifetime 计数器快照。
    pub(crate) fn counts(&self) -> quality::JbCounts {
        quality::JbCounts {
            popped: self.jb.popped,
            plc: self.jb.plc_count,
            silence: self.jb.silence_count,
            underruns: self.jb.underruns,
            dropped: self.jb.dropped,
        }
    }

    /// 给 Q1 窗口补一个采样点。幂等、非消费型，谁调都不改变结论。
    pub(crate) fn sample_conceal(&mut self) {
        let c = self.counts();
        self.conceal.sample(Instant::now(), c);
    }
}

pub(crate) struct PostMix {
    pub fifo: VecDeque<f32>, // absorbs resampler length wobble -> exact 480/frame
    /// FIFO 溢出丢掉的样本数。方向是 **`DropMode::Oldest`**（`drain(..excess)`
    /// 从头删），与播放环的丢最新恰好相反——两者深度读数简并，听感不同。
    pub dropped: u64,
}

/// PostMix 的上限，100 ms @ 48k（规格 §3.2 的级 7）。
pub(crate) const POST_MIX_CAP: usize = 4800;

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
        if self.fifo.len() > POST_MIX_CAP {
            let excess = self.fifo.len() - POST_MIX_CAP;
            self.fifo.drain(..excess);
            self.dropped += excess as u64; // 丢弃行为未改，只是现在数得出来
        }
    }

    /// 规格 §3.2 的级 7 读数。
    pub(crate) fn depth(&self) -> StageDepth {
        StageDepth {
            id: StageId::PostMix,
            samples: self.fifo.len() as u32,
            capacity: POST_MIX_CAP as u32,
            rate: 48_000, // 解码后固定 48k，与设备速率无关
            dropped: Some(self.dropped),
            drop_mode: DropMode::Oldest,
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
    /// This stream is ALSO written into ONE slot's mic ring, so the virtual
    /// microphone that belongs to this peer carries it (spec-m5b §5.4). A third
    /// destination, not an alternative to `monitor` or `bridge`.
    ///
    /// The SLOT, not a bare flag: the mixer routes by it, and a boolean here
    /// is what let every peer's audio end up in one ring.
    pub hal_slot: Option<u8>,
    pub ka_dest: SocketAddr,
    pub jbs: Mutex<JbState>,
    pub post: Mutex<PostMix>,
    /// post-JB 48k tap (2s) for per-stream verdicts; only allocated when a
    /// verdict was actually requested, so N streams cost N*0 rings by default
    pub ring: Option<Mutex<VecDeque<f32>>>,
    pub stats: Mutex<RxCell>,
    pub ka_seq: AtomicU32,
    /// Q2 的可归属那一半：本流在**加进混音之前**的电平/削顶（规格 §4.6）。
    /// 测点在 `PostMix::advance` 之后、各目的地之前，所以它回答的是「我这一路
    /// 送进来多响」，与求和后的站点级削顶是两个不同的问题。
    pub clip: quality::ClipMeter,
    /// 级 8′ `bridge_ring`：本流被桥接到的那张虚拟声卡的播放环（1 s，丢最新）。
    ///
    /// 与站点级的 `DaemonInner::play_ring` **不是同一个环**：桥接流走的是
    /// `BridgeOut::tx`（每个桥一个 `AudioTx`）。没有这一槽时，一条纯桥接流上报的
    /// `local_ms` 只有 jitter_buf + post_mix，**静默漏掉整整一秒**，而且不降
    /// confidence、不标注缺席——正是这套遥测存在的理由所反对的那种失败形态。
    pub bridge_ring: StageSlot,
    /// 级 8″ `hal_mic`：模式 B 虚拟麦克风环（500 ms，丢最新）。
    ///
    /// 同上，只是目的地换成 HAL mic ring：本流写进去、驱动交给选了这个虚拟麦克风
    /// 的 App。它同样在送音频的路径上，同样此前一级都没建模。
    pub hal_mic: StageSlot,
    /// 本条接收流各级深度的 30 s 漂移窗口。由 1 s 的 ticker 喂点。
    pub drift: Mutex<DriftTracker>,
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
        hal_slot: Option<u8>,
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
            hal_slot,
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
                conceal: quality::ConcealWindow::new(),
            }),
            post: Mutex::new(PostMix { fifo: VecDeque::new(), dropped: 0 }),
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
            clip: quality::ClipMeter::new(),
            bridge_ring: StageSlot::new(),
            hal_mic: StageSlot::new(),
            drift: Mutex::new(DriftTracker::new()),
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
    /// 发送侧各级深度的发布槽，由 tx_loop 每 10 ms 写一次（只有原子 store）。
    ///
    /// 最多两级：`MicSource` 是采集环 + 发送 FIFO，`SysAudio` 只有发送 FIFO，
    /// `HalSpeaker` 只有虚拟扬声器环，`ToneSource` 一级都没有。
    ///
    /// ⚠ **扇出时这些深度是共享的**（规格 §7.2 R8）：一个 `SourceEnt` 被 N 条
    /// 流引用，物理队列只有一份，N 条流读到的是同一个数——这是**正确的物理
    /// 事实**，不是重复计数。但由此得出一条硬约束：**分项只能按流展示，
    /// 不可跨流求和**，否则 N 张卡片相加会得到 N 倍的假延迟。
    ///
    /// 第三槽固定给级 4 `send_pace`（常数 5 ms）：那一级不属于任何一个源，而是
    /// **调度器自己**——`tx_loop` 每 10 ms 一次性取走 480 个样本，把连续到达量化
    /// 到打包边界的那半个 tick 由这个循环造成，所以由 `tx_loop` 发射，不由
    /// `depths()` 发射。见 `engine::SEND_PACE_SLOT`。
    pub stages: [StageSlot; 3],
    /// 本条发送流各级深度的 30 s 漂移窗口，由 1 s 的 ticker 喂点。
    pub drift: Mutex<DriftTracker>,
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
            stages: [StageSlot::new(), StageSlot::new(), StageSlot::new()],
            drift: Mutex::new(DriftTracker::new()),
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
    // One lock for the whole batch: `session.list` is on the stats event path
    // and takes this every second per subscriber.
    let names: HashMap<u8, (String, String)> = {
        let st = lk(&inner.haldev);
        st.slots
            .iter()
            .enumerate()
            .filter(|(_, r)| !r.fingerprint.is_empty())
            .map(|(s, r)| (s as u8, (r.sent_out_name.clone(), r.sent_in_name.clone())))
            .collect()
    };
    // 逐级延迟会计**全部**在装配层一次算完（见 `assemble_pipelines`）：
    // N 条流进，N 条读数出，第 i 条只由第 i 条流自己的队列决定。
    let pipelines = assemble_pipelines(
        &inner.play_ring,
        &inner.play_drift,
        entries.iter().map(StreamLat::of).collect(),
    );
    // ⚠ 两个 `into_iter()` 都不是风格选择，是 R8 在这一层的封口：`entries` 与
    // `pipelines` 双双被**移进**迭代器，于是闭包里的
    // `entries.iter().filter_map(..).sum()` / `pipelines.iter().sum()`
    // 根本编译不过。这一层此前握着一整个 `&[SessionEntry]`，跨流求和随手可写
    // 而 253 条测试全绿——`compose_sum_ms` 只收标量封住的是合成函数，不是这里。
    entries
        .into_iter()
        .zip(pipelines)
        .map(|(e, pipeline)| {
            // The DEVICE this session exists for: a spk session carries what an
            // app played into the virtual speaker, a mic session feeds the
            // virtual microphone. Reporting one name for both would put the
            // wrong device on half the stats page.
            let dev = e.origin.slot().and_then(|s| names.get(&s)).map(|(o, i)| {
                if e.kind == KIND_SPK {
                    o.as_str()
                } else {
                    i.as_str()
                }
            });
            build_session_info_with(inner, &e, &mix_freqs, mix_snap.as_deref(), dev, pipeline)
        })
        .collect()
}

/// 一条流里**与延迟有关的全部东西**——装配层看得见的唯一投影。
///
/// 刻意不含 `conn`、不含 `SessionEntry`：见 `assemble_pipelines` 的文档。
/// 这也是这一层能被测试驱动的原因——`SessionEntry.conn` 要一条真 TCP 与一次
/// 完整握手才造得出来，收它当参数就等于把装配层永久挡在测试之外。
pub(crate) struct StreamLat<'a> {
    is_send: bool,
    tx: Option<&'a TxShared>,
    rx: Option<&'a RxStream>,
    /// **这一条流**的对端分项（R8：格子按流走，见 `PeerLatCell`）。
    peer: Option<PeerLatSnapshot>,
    /// 这条流所在**连接**的时钟估计。队列属于流，时钟属于主机。
    clock: Option<ClockEstimate>,
}

impl<'a> StreamLat<'a> {
    fn of(e: &'a SessionEntry) -> StreamLat<'a> {
        StreamLat {
            is_send: e.dir == DIR_SEND,
            tx: e.tx.as_deref(),
            rx: e.rx.as_deref(),
            peer: e.peer_lat.snapshot(),
            clock: lk(&e.conn.clock).estimate(),
        }
    }
}

/// 装配层：**N 条流进，N 条读数出，第 i 条只由第 i 条流的队列决定。**
///
/// # 这一层为什么必须单独存在（R8 的最后一处缺口）
///
/// 规格 §7.2 R8 说分项「只能按流合成，跨流求和会得到 N 倍假延迟」，而此前这条
/// 约束的全部执行力只有两处：格子挂在 `SessionEntry` 上（没有可以 `values()`
/// 求和的表），以及 `compose_sum_ms` 只收标量。两者封住的都是**合成函数**。
/// 真正把 N 条流的读数摆在一起的是**装配循环**，而它手上握着整个
/// `&[SessionEntry]`——`entries.iter().filter_map(|e| …local_ms).sum()` 写进去，
/// 253 条测试没有一条会红，因为它们全都直接调 `build_pipeline_from`，绕过了这里。
///
/// 所以这一层收 `Vec<StreamLat>` 并 `into_iter()`：**`streams` 被移进迭代器**，
/// 闭包里再写 `streams.iter()…sum()` 编译不过。加上
/// `the_assembly_layer_gives_each_stream_its_own_depth_never_the_fleet_sum`
/// 那条走这条真路径的测试，「按流一份」这次才既是结构性的、又是被测的。
///
/// # 为什么连 `attach_peer_and_net` 也在这里
///
/// 一条流的读数由三块拼成：本侧逐级 + 输出尾级 + 对端分项/网络段。三块若散在
/// 三个调用点，「装配层」就不是一个可以被指着说「跨流求和只可能写在这里」的
/// 地方。全部收进来之后，这一个函数就是全部的答案。
fn assemble_pipelines(
    play_ring: &StageSlot,
    play_drift: &Mutex<DriftTracker>,
    streams: Vec<StreamLat<'_>>,
) -> Vec<Option<PipelineLatency>> {
    streams
        .into_iter()
        .map(|s| {
            let mut p = build_pipeline_from(s.is_send, s.tx, s.rx)?;
            if let Some(rx) = s.rx {
                attach_output_tails(play_ring, play_drift, rx, &mut p);
            }
            attach_peer_and_net(&mut p, s.peer, s.clock);
            Some(p)
        })
        .collect()
}

// ------------------------------------------------- 遥测组装 (P0a / P0q)

/// 1 s 心跳：给每条流的漂移窗口喂一个点，并给 Q1 窗口补一个采样点。
///
/// 为什么 Q1 要在这里再补一次（接收线程已经每 10 次 push 采过了）：**断流时
/// 接收线程根本不跑**。只挂在 push 上，窗口会在黑屏期间冻结，报出黑屏之前那
/// 10 秒的漂亮数字——而黑屏正是 JB 疯狂 underrun、Q1 最该报警的时候。
fn sample_telemetry(inner: &DaemonInner, entries: &[SessionEntry]) {
    let now_s = inner.start.elapsed().as_secs_f32();
    // 站点级：播放环。所有送本机输出的流共用同一个环（规格 §7.2 R7），
    // 所以漂移窗口也只有一份。
    match inner.play_ring.load() {
        Some(d) => lk(&inner.play_drift).push(now_s, d.id, d.samples),
        // 环不存在时清历史，否则下次开流会继承上一次会话的斜率。
        None => lk(&inner.play_drift).clear(StageId::PlayRing),
    }
    for e in entries {
        if let Some(tx) = &e.tx {
            sample_tx_drift(now_s, tx);
        }
        if let Some(rx) = &e.rx {
            // ⚠ 锁序：**先把读数取出来并释放各自的锁，再去拿 drift**。
            // 曾经这里是 `jbs -> drift` 嵌套，而 `build_pipeline` 是
            // `drift -> jbs` 嵌套，两条路径一跑就是 ABBA 死锁。现在两边都
            // 「一次只持一把」，锁序问题从根上不存在。
            let jb_samples = {
                let mut st = lk(&rx.jbs);
                st.sample_conceal();
                st.jb.contiguous() * F48_PER_FRAME
            };
            let post = lk(&rx.post).fifo.len() as u32;
            // 两条并行尾级的槽（原子读，不上锁），与上面同一条纪律：槽空就清历史，
            // 否则下一次开桥会继承上一次的斜率。
            let tails = [rx.bridge_ring.load(), rx.hal_mic.load()];
            let mut w = lk(&rx.drift);
            w.push(now_s, StageId::JitterBuf, jb_samples);
            w.push(now_s, StageId::PostMix, post);
            for (d, id) in tails.iter().zip([StageId::BridgeRing, StageId::HalMic]) {
                match d {
                    Some(d) => w.push(now_s, d.id, d.samples),
                    None => w.clear(id),
                }
            }
        }
    }
}

/// 一条发送流的漂移窗口：喂在场的级，**清不在场的级**。
///
/// 拆出来只为**可测**：`sample_telemetry` 要一个 `DaemonInner` 和一串
/// `SessionEntry`（后者里的 `conn: Arc<ConnShared>` 要一条真 TCP 连接才造得出
/// 来），而这里的全部内容只需要一个 `TxShared`。上一版把「清」这一步漏在了
/// 调用点里，单测又只覆盖 `DriftTracker` 本身，于是漏掉的是**接线**而不是逻辑
/// ——拆开之后接线本身就能被断言。
///
/// ## 为什么必须清
///
/// `TxShared` 的生命周期比源长（会话表还持有它，报告线程还在读）。源被换掉时
/// ——默认输入设备一变就重建 `MicSource`，或 `TxCmd::Remove` 之后另一条流挂上来
/// ——新源会直接接着读旧源留下的、最长 30 s 的斜率。而 `drift_sps` 的全部用途
/// 就是判「这一级在不在漂」，继承来的斜率会把一个刚开的干净流报成正在走向饱和。
/// 接收侧的 `play_ring` 与两条尾级早就是 `None => clear` 了，发送侧一直没有。
///
/// ## 为什么不是照抄 `None => w.clear(id)`
///
/// 发送侧的槽是**匿名**的：`StageSlot` 空的时候只剩一个 0，说不出它上一轮装的
/// 是哪一级。所以反过来说——本 tick 在场的就这几级，其余一律清。这顺带盖住了
/// 「槽没空、只是换了 id」（`cap_ring`+`src_fifo` → `hal_spk`）那一种源切换，
/// 而逐 id 清的写法对它无感。
fn sample_tx_drift(now_s: f32, tx: &TxShared) {
    // `slot.load()` 是原子读，不上锁——所以全程只持有 drift 一把。
    let mut present: [Option<StageId>; 3] = [None; 3];
    let mut w = lk(&tx.drift);
    for (slot, seen) in tx.stages.iter().zip(present.iter_mut()) {
        if let Some(d) = slot.load() {
            w.push(now_s, d.id, d.samples);
            *seen = Some(d.id);
        }
    }
    w.retain_only(&present);
}

/// 一个 10 ms 帧的样本数 @48k。漂移的单位是**样本/秒**，所以 JB 的帧数要先
/// 换成样本数，否则同一个物理漂移在 JB 上会比在 FIFO 上小 480 倍。
const F48_PER_FRAME: u32 = 480;

/// `StageDepth` + 漂移斜率 → IPC 的 `PipelineStage`。
///
/// ms 换算与字符串化全部发生在这里（报告线程），不在 10 ms 节拍上。
fn to_ipc_stage(d: StageDepth, drift: Option<f64>) -> PipelineStage {
    PipelineStage {
        id: d.id.as_str().to_string(),
        samples: d.samples,
        capacity: d.capacity,
        rate: d.rate,
        ms: d.ms(),
        dropped: d.dropped,
        drop_mode: d.drop_mode,
        saturated: d.saturated(),
        drift_sps: drift,
    }
}

/// Σ 各级。**任一级读不到（`ms()` 为 `None`）⇒ 整体 `None`。**
///
/// 绝不用 0 填补：一个测不到的 200 ms 蓝牙缓冲若按 0 计入，读数会和模拟输出
/// 一样漂亮，而那正是这套遥测存在的理由。空列表同样是 `None`——「一级都没有」
/// 不等于「延迟为 0」。
///
/// ## 串联求和，**并行尾级取 max**
///
/// 一帧解码结果会被**同时**送进真实输出 / 桥接虚拟声卡 / 虚拟麦克风
/// （`engine.rs` 的 mixer：三者是独立目的地，不是互斥选项）。它们在时间上并联，
/// 用户从任一条听到的延迟是**那一条**的驻留，不是几条之和。一条同时开了「监听」
/// 与「桥接到虚拟声卡」的会话会有两条 1 秒环，直接相加就报出 2 秒的假延迟。
/// 所以尾级（`StageId::is_output_tail`）只取最大的一条计入总数。
fn sum_stage_ms(stages: &[PipelineStage]) -> Option<f64> {
    if stages.is_empty() {
        return None;
    }
    let mut total = 0.0;
    let mut tail_max: Option<f64> = None;
    for s in stages {
        let ms = s.ms?;
        // id 反解析失败 = 一个本文件不认识的级。按串联计入是保守的（宁可多算
        // 也不静默漏掉），而认不出来本身就该是下一次改动要处理的信号。
        if StageId::from_id_str(&s.id).map_or(false, StageId::is_output_tail) {
            tail_max = Some(tail_max.map_or(ms, |m: f64| m.max(ms)));
        } else {
            total += ms;
        }
    }
    Some(total + tail_max.unwrap_or(0.0))
}

/// 组装一条会话的逐级延迟会计（P0a：只有本侧）。
///
/// `SessionEntry` 拆出去只为**可测**：它里面的 `conn: Arc<ConnShared>` 要一条
/// 真的 TCP 连接与一次完整握手才造得出来，而这个函数真正的内容（JB 用
/// contiguous 还是 depth、容量取 target+6、丢弃方向、缺项如何变成 None）一条
/// 都不需要连接。下面这一行是全部的胶水，逻辑全在 `build_pipeline_from` 里。
fn build_pipeline(e: &SessionEntry) -> Option<PipelineLatency> {
    build_pipeline_from(e.dir == DIR_SEND, e.tx.as_deref(), e.rx.as_deref())
}

fn build_pipeline_from(
    is_send: bool,
    tx: Option<&TxShared>,
    rx: Option<&RxStream>,
) -> Option<PipelineLatency> {
    let mut stages: Vec<PipelineStage> = Vec::new();
    let side = if is_send { "send" } else { "recv" };

    if let Some(tx) = tx {
        // `slot.load()` 是原子读；drift 一把锁独立持有（见 `sample_telemetry`
        // 上的锁序说明）。
        let w = lk(&tx.drift);
        for slot in tx.stages.iter() {
            if let Some(d) = slot.load() {
                let drift = w.slope(d.id);
                stages.push(to_ipc_stage(d, drift));
            }
        }
    }
    if let Some(rx) = rx {
        // ⚠ 锁序：先把两个深度读出来（各自短锁、不嵌套），最后才拿 drift。
        //
        // 级 6：抖动缓冲。用 **contiguous** 而不是 depth——乱序时 depth 把
        // 「洞之后的帧」也算进去，那些帧并不会以 100 帧/秒被放出来，拿它算驻留
        // 时间会高估（规格 §7.2 R10）。
        let (contiguous, target) = {
            let st = lk(&rx.jbs);
            (st.jb.contiguous(), st.jb.target())
        };
        let post = lk(&rx.post).depth();
        let jb = StageDepth {
            id: StageId::JitterBuf,
            samples: contiguous * F48_PER_FRAME,
            // 硬上限是 target+6 帧（`pop` 里的修剪条件），不是某个固定常数。
            capacity: (target + 6) * F48_PER_FRAME,
            rate: 48_000,
            // JB 的 dropped 是**帧**计数（late arrivals + catch-up drops），
            // 与本结构的「样本」单位不同，换算过去会造成量纲错觉；它以
            // `jb_dropped` 单独上报，这里如实说「本级的样本级丢弃观测不到」。
            dropped: None,
            drop_mode: DropMode::Oldest, // 修剪时 `frames.iter().next()` 丢最旧
        };
        let w = lk(&rx.drift);
        stages.push(to_ipc_stage(jb, w.slope(StageId::JitterBuf)));
        // 级 7：混音对齐缓冲。
        stages.push(to_ipc_stage(post, w.slope(StageId::PostMix)));
    }

    if stages.is_empty() {
        return None; // 这条会话没有任何可读的级：报 None，不报一个空壳
    }

    Some(PipelineLatency {
        side: side.to_string(),
        local_ms: sum_stage_ms(&stages),
        stages,
        // P0：平台设备延迟属性还没查（那是 P1 的活）。**恒 Unavailable，
        // 绝不填 0** —— 正因为它缺，下面的 confidence 才不可能是 Full。
        dev: Some(DevLatency::unavailable()),
        // 对端分项与网络段由 `attach_peer_and_net` 在**这一层之外**填（P0b/P1）：
        // 这个函数只认识本侧的队列，它连一条连接都拿不到。默认值是「什么都没有」
        // ——一条从未收到过对端上报的流会原样保持这个形态。
        peer_stages: Vec::new(),
        peer_local_ms: None,
        peer_dev: None,
        peer_age_s: None,
        // RTT 的地位（红线）：min-RTT 窗口没攒够 8 个样本之前**宁可 None 也不
        // 拿一个未滤波的 RTT 冒充**——实测 RTT 0.58 ms vs 感知 ~1000 ms，
        // 比值 1700 倍，两者之间不存在任何单调关系。
        net_ms: None,
        rtt_cross_check_ms: None,
        // 对端分项缺失 ⇒ 总和无从谈起。UI 这一期显示的是 local_ms。
        sum_ms: None,
        e2e_ms: None,
        residual_ms: None,
        clock_offset_us: None,
        clock_unc_us: None,
        confidence: LatConfidence::LocalOnly,
    })
}

/// 把一条接收流的**输出尾级**挂上去：真实播放环 / 桥接虚拟声卡环 / 虚拟麦克风环。
///
/// 三者是**并行**的目的地，不是三选一：同一帧解码结果可以同时进监听输出和桥接
/// 虚拟声卡（`engine.rs` 的 mixer 明确写着「a third destination, not an
/// alternative」）。所以三条都要挂出来给排障看，但 `sum_stage_ms` 只把最大的那条
/// 计入总数（见那里的「并行尾级取 max」）。
///
/// - `play_ring` 是**站点级**的：它属于设备，不属于会话，所以多条送本机输出的流
///   报同一个读数是物理事实（规格 §7.2 R7）。
/// - `bridge_ring` / `hal_mic` 是**每流**的槽，由 mixer 每 tick 写（含 `None`）。
///
/// 少了后两条，一条纯桥接流或纯虚拟麦克风流的 `local_ms` 就只有
/// jitter_buf + post_mix ——**静默漏掉整整一秒**，且不降 confidence、不标缺席。
///
/// 取 `&StageSlot` + `&Mutex<DriftTracker>` 而不是 `&DaemonInner`：与
/// `publish_play_ring` / `sample_tx_drift` 同一条理由——`DaemonInner` 要一个 UDP
/// socket、一堆线程通道和一个真实设备才造得出来，收它当参数就把**「一级被灌满
/// 之后 local_ms 到底动没动」**这件事永久挡在测试之外，而那正是这套遥测唯一要
/// 回答的问题。调用方传 `&inner.play_ring, &inner.play_drift`。
fn attach_output_tails(
    play_ring: &StageSlot,
    play_drift: &Mutex<DriftTracker>,
    rx: &RxStream,
    p: &mut PipelineLatency,
) {
    let mut pushed = false;
    // 真实默认输出：只有真的往它送音频的流才经历这一级。
    if rx.is_spk || rx.monitor {
        if let Some(d) = play_ring.load() {
            let drift = lk(play_drift).slope(d.id);
            p.stages.push(to_ipc_stage(d, drift));
            pushed = true;
        }
    }
    // 桥接环与虚拟麦克风环：槽为空就是「本流这一 tick 没有这条尾级」。
    for slot in [&rx.bridge_ring, &rx.hal_mic] {
        if let Some(d) = slot.load() {
            let drift = lk(&rx.drift).slope(d.id);
            p.stages.push(to_ipc_stage(d, drift));
            pushed = true;
        }
    }
    if pushed {
        p.local_ms = sum_stage_ms(&p.stages);
    }
}

// ------------------------------------------- P0b/P1：合成对端分项 + 网络单程

/// 一条流的总延迟：**本侧 Σ + 网络单程 + 对端 Σ**（规格 §3.3）。
///
/// # 只收标量，收不了切片——这是 R8 的类型级封口
///
/// 规格 §7.2 R8：扇出时一个源被 N 条流引用，物理队列只有一份，N 条流报的
/// `src_fifo` 是同一个数。把 N 条流的读数相加会得到 N 倍假延迟。这个签名让
/// 那件事写不出来：没有 `&[PipelineLatency]`，没有 `sum()`，调用点必须先挑出
/// **一条流**的三个数。合成因此只能是逐流的。
///
/// # 三项缺一即 `None`，绝不用 0 填补
///
/// - `local_ms = None`：本侧有一级读不到（`rate == 0`）。
/// - `peer_local_ms = None`：对端没回传，或它此刻也有一级读不到。
/// - `net_ms = None`：min-RTT 窗口还没攒够 8 个样本。
///
/// 任何一项按 0 计入都会让读数变漂亮而错误——蓝牙耳机那 150~250 ms 就是这么
/// 消失的。
fn compose_sum_ms(
    local_ms: Option<f64>,
    net_ms: Option<f64>,
    peer_local_ms: Option<f64>,
) -> Option<f64> {
    Some(local_ms? + net_ms? + peer_local_ms?)
}

/// 把对端分项（P0b）与网络单程 / 时钟偏移（P1）挂到一条流的读数上。
///
/// 拆成自由函数、只收 `Option` 参数，与本文件其它遥测函数同一条理由：
/// `SessionEntry` 里的 `conn` 要一条真 TCP 与一次完整握手才造得出来，而这里的
/// 全部内容（缺项如何变成 `None`、confidence 怎么爬梯子、RTT 只当一段）
/// 一条连接都不需要。
///
/// # confidence 的梯子（自下而上）
///
/// | 状态 | 取值 | UI 含义 |
/// |---|---|---|
/// | 对端没回传 / 回传已过期 | `LocalOnly` | 「对端未上报」，只画本机段 |
/// | 对端有数，但 min-RTT 窗口没收敛 | `Converging` | 「测量中」（约 8 s） |
/// | 都有了，但某一级读不到 | `Unavailable` | 「无法测量」 |
/// | 都有了 | `LowerBound` | 「≥ N ms」——设备固有延迟仍缺 |
///
/// `Full` 在这一期**不可达**，故意的：它要求设备固有延迟项齐全，而那是 P1 的
/// 另一半（`audiohub_core::devlat` 查平台属性），此刻 `dev` 恒 `Unavailable`。
/// 让它可达就是在说谎。
///
/// ⚠ **接上设备固有延迟时必须同时改三处**，少改一处就是一个静默的谎：
/// 1. `compose_sum_ms`：把 `dev` / `peer_dev` 的 ms 加进总数；
/// 2. 这里的 `LowerBound`：设备项齐全后它不再是下限；
/// 3. `the_total_is_both_sides_plus_exactly_one_network_segment` 那条测试。
///
/// 第 3 条里有一句 `assert!(p.dev…ms().is_none())`，它就是这个前提的看门狗：
/// 设备项一旦真有值，那条测试会先红，把改动逼到该改的两个地方去。
///
/// θ 未收敛**不**单独降级：本期显示的 `sum_ms` 里没有 θ 的份（它只服务于 P1
/// 的 `e2e_ms`）。拿一个与显示值无关的量去把读数标成「测量中」，是另一种形式
/// 的不诚实。`Converging` 在这里的触发条件是 `net_ms` 缺席，而 `net_ms` 与 θ
/// 共用同一个 8 样本窗口——所以时间上它仍然是「约 8 秒后收敛」。
fn attach_peer_and_net(
    p: &mut PipelineLatency,
    peer: Option<PeerLatSnapshot>,
    clock: Option<ClockEstimate>,
) {
    // 网络单程 = min-RTT / 2。**只作一段，绝不作总数**（规格 §3.1 红线）。
    // 单位换算：µs → ms 要除 1000，取一半再除 2 ⇒ /2000。
    p.net_ms = clock.map(|c| c.min_rtt_us as f64 / 2000.0);
    // 交叉校验（规格 §6.4）：最新一次往返与窗口最小值差多少。稳态下 ≈0；
    // 持续偏大 = 路径在排队，此刻的 `net_ms` 只是一个下限。
    p.rtt_cross_check_ms =
        clock.map(|c| (c.last_rtt_us as f64 - c.min_rtt_us as f64).abs() / 2000.0);
    // θ 只有在对端真的报了 `peer_t_us` 时才存在。不确定度跟着 θ 走：单独报一个
    // 「不确定度」而没有它所修饰的那个量，读者只能误解。
    p.clock_offset_us = clock.and_then(|c| c.offset_us);
    p.clock_unc_us = clock.filter(|c| c.offset_us.is_some()).map(|c| c.unc_us);

    let Some(peer) = peer else {
        // 对端未上报 ⇒ 保持 `build_pipeline_from` 给的 `LocalOnly`。
        // **不猜、不用 RTT 顶替**：RTT 与总延迟之间不存在任何单调关系。
        return;
    };
    p.peer_stages = peer.stages;
    p.peer_local_ms = peer.local_ms;
    p.peer_dev = peer.dev;
    p.peer_age_s = Some(peer.age_s);
    p.sum_ms = compose_sum_ms(p.local_ms, p.net_ms, p.peer_local_ms);
    p.confidence = match (p.sum_ms, p.net_ms) {
        // 齐了。仍带「≥」：设备固有延迟项还没查（`dev` 恒 `Unavailable`）。
        (Some(_), _) => LatConfidence::LowerBound,
        // 网络段还没收敛（约 8 s）。UI：测量中。
        (None, None) => LatConfidence::Converging,
        // 网络段有了，却仍合不出总数 ⇒ 某一级读不到。这与「对端没回传」是两个
        // 不同的用户动作，所以不能共用 `LocalOnly`。
        (None, Some(_)) => LatConfidence::Unavailable,
    };
}

/// 一条会话**本侧**的完整读数：逐级会计 + 输出尾级。
///
/// 抽出来是因为它现在有两个调用点，而两者必须逐字节一致：
/// - `build_session_info`：给 UI 看的那一份；
/// - ticker 的 `StageReport`：**回传给对端**的那一份。
///
/// 两边若各拼各的，对端拿到的分项会与本机 UI 显示的不是同一个东西，而这种
/// 分歧只会在两台机器的截图并排放时才被发现。
fn local_pipeline(inner: &DaemonInner, e: &SessionEntry) -> Option<PipelineLatency> {
    let mut p = build_pipeline(e)?;
    if let Some(rx) = &e.rx {
        attach_output_tails(&inner.play_ring, &inner.play_drift, rx, &mut p);
    }
    Some(p)
}

/// IPC 的 `PipelineStage` → 线上的 `StageReading`（回传给对端）。
///
/// `ms` 不上线：见 `StageReading` 的文档（收方必须自己按 `rate == 0 ⇒ None`
/// 的规则重算，否则一条 `{rate:0, ms:0.0}` 就能把「测不到」伪装成「没有延迟」）。
fn to_wire_stage(s: &PipelineStage) -> StageReading {
    StageReading {
        id: s.id.clone(),
        samples: s.samples,
        capacity: s.capacity,
        rate: s.rate,
        dropped: s.dropped,
        drop_mode: s.drop_mode,
        saturated: s.saturated,
        drift_sps: s.drift_sps,
    }
}

/// 线上的 `StageReading` → IPC 的 `PipelineStage`（对端回传的分项）。
///
/// # `ms` 在这里重算，规则与 `StageDepth::ms()` 逐字相同
///
/// 为什么不直接调 `StageDepth::ms()`：构造 `StageDepth` 需要一个 `StageId`
/// **枚举**，而对端完全可能带来本版本不认识的级 id（它更新了，我们没有）。
/// 那种情况下这一级仍然要能换算、能显示、能计入总数——认不出 id 不等于测不到。
/// 所以这里按 `rate` 直接算，并由 `wire_and_local_agree_on_ms` 那条测试把两份
/// 实现钉在一起。
fn from_wire_stage(w: &StageReading) -> PipelineStage {
    PipelineStage {
        // `rate == 0` 即判该级读数无效 ⇒ `None`，**不是 0 ms**。
        ms: (w.rate != 0).then(|| w.samples as f64 * 1000.0 / w.rate as f64),
        id: w.id.clone(),
        samples: w.samples,
        capacity: w.capacity,
        rate: w.rate,
        dropped: w.dropped,
        drop_mode: w.drop_mode,
        saturated: w.saturated,
        drift_sps: w.drift_sps,
    }
}

/// 组装一条接收会话的音质三分量（规格 §4）。
///
/// `rung` 由调用方给出（它已经从 `RxCell.last_rate` 或 `TxShared` 解析过一次）。
/// `duplicate` 是站点级的一票否决：`MixHealth.duplicate_suspect` 为真时，此刻
/// 的削顶不是素材响，是叠加 bug。
fn build_quality(rx: &RxStream, rung: u32, duplicate: bool) -> Option<QualityStats> {
    // Q1：10 s 非消费型窗口的差分。窗口不够长 ⇒ 整个 QualityStats 为 None，
    // 而不是给一个分母只有几帧的随机比率。
    let (window_s, d) = lk(&rx.jbs).conceal.window()?;
    let conceal = quality::conceal_ratio(&d)?;
    // Q2：本流送进混音前的削顶。**还没攒够一整页 ⇒ `None`，不是 0**。
    //
    // 这里曾经是 `None => (0.0, -120.0)`，而 `grade_clip(0.0) = Excellent`，
    // 于是 min 合成拿到一个永远拉不低总分的分量：任何流开头约 10~20 秒，一条
    // 正在爆音的流报「良好」。更要命的是上报出去的 `clip_ratio: f64` 让
    // 「还没测」与「测了，确实静音」**完全无法区分**——而这正好与上一行 Q1
    // 「窗口不够就整体 None」的口径自相矛盾。
    //
    // 中间还有过一版**看起来已经修好、其实没有**的写法：缺席「不进 min」外加
    // 一个 `partial` 标注。`Grade::Excellent` 是 `Ord` 的最大值，
    // `min(q1, Excellent, q3) ≡ min(q1, q3)` —— 与填 Excellent **逐值相同**，
    // 用户读到的那个「良好」一个字都没变。
    //
    // 现在：缺席原样传给 UI（`Option`），并且让**等级本身**承认不确定
    // （`compose` 返回 `Option<Grade>`，这里落成 `"unknown"`）。这与 Q1
    // 「窗口不够就整体 None」是同一个口径：测不出来就说测不出来。
    let clip = rx.clip.window();
    let bandwidth_hz = audiohub_net::media::rung_rate(rung) / 2;

    let q1 = quality::grade_conceal(conceal);
    let q2 = if duplicate {
        // 一票否决（规格 §4.4）：两路重复流相加把整段波形 ×2，此时的破音不是
        // 素材响。即便本流自己的削顶率很低，用户听到的也是烂的。
        // 这是一个**站点级实测结论**，不依赖本流的削顶页，所以它照样成立。
        Some(quality::Grade::Poor)
    } else {
        clip.map(|w| quality::grade_clip(w.ratio()))
    };
    let q3 = quality::grade_bandwidth(bandwidth_hz);
    let (grade, worst, partial) = quality::compose(q1, q2, q3);

    Some(QualityStats {
        window_s,
        conceal_ratio: conceal,
        plc_ticks: d.plc,
        silence_ticks: d.silence,
        popped_ticks: d.popped,
        underruns: d.underruns,
        jb_dropped: d.dropped,
        clip_ratio: clip.map(|w| w.ratio()),
        clip_excess_db: clip.map(|w| w.excess_db()),
        bandwidth_hz,
        // 等级不成立时是 `"unknown"`，**不是在场分量的 min**。IPC 契约上
        // `grade` 早就把 "unknown" 列为合法取值，此前没有任何路径产生它——
        // 那个空位就是这个缺陷藏身的地方。
        grade: grade.map_or("unknown", quality::Grade::as_str).to_string(),
        worst: worst.to_string(),
        partial,
    })
}

/// 站点级混音健康。`None` = 本窗口内混音器没有输出过。
pub(crate) fn build_mix_health(inner: &DaemonInner) -> Option<MixHealth> {
    let m = inner.mix_meter.window()?;
    let c = inner.mix_clip.window();
    Some(MixHealth {
        window_s: m.span_s,
        // 削顶页还没攒满就是**还没测**，不是「测了，0%」。`unwrap_or(0.0)` 会把
        // 启动后头 10 秒的空窗报成「混音正常」——与 `build_quality` 里那个
        // 0 填补同病同源。
        clip_ratio: c.map(|w| w.ratio()),
        clip_excess_db: c.map(|w| w.excess_db()),
        max_contrib: m.max_contrib,
        corr_peak: m.corr_peak,
        duplicate_suspect: m.duplicate_suspect(),
    })
}

/// 单条会话的读数（`conn.rs` 的 `session.opened` 事件走这条路径）。
///
/// 它**也**走装配层，只是流的条数是 1：两个入口若各拼各的，事件里的读数与
/// UI 上显示的就会不是同一个东西，而这种分歧只在两处截图并排放时才被发现。
pub(crate) fn build_session_info(
    inner: &DaemonInner,
    e: &SessionEntry,
    mix_freqs: &[f32],
    mix_snap: Option<&[f32]>,
    hal_device: Option<&str>,
) -> SessionInfo {
    let pipeline = assemble_pipelines(&inner.play_ring, &inner.play_drift, vec![StreamLat::of(e)])
        .pop()
        .flatten();
    build_session_info_with(inner, e, mix_freqs, mix_snap, hal_device, pipeline)
}

/// `build_session_info` 的全部内容，只是**逐级延迟会计由调用方给**。
///
/// 拆开的理由与本文件其它遥测函数同一条：让「谁算的这条读数」变成签名上的
/// 事实。这一层收的是一条**已经算完**的 `PipelineLatency`，它没有任何机会
/// 去看别的流——装配循环因此只剩搬运，不再做延迟算术。
fn build_session_info_with(
    inner: &DaemonInner,
    e: &SessionEntry,
    mix_freqs: &[f32],
    mix_snap: Option<&[f32]>,
    hal_device: Option<&str>,
    pipeline: Option<PipelineLatency>,
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
        pipeline: None,
        quality: None,
        jb_popped: 0,
        jb_underruns: 0,
        jb_dropped: 0,
        jb_plc: 0,
        jb_silence: 0,
        jb_target_frames: 0,
        jb_prebuffering: false,
        jb_contiguous_frames: 0,
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
        {
            // JitterBuffer 早就有这五个 `pub` 计数器（media.rs），一个都没导出过。
            // 零成本补齐——它们是 **lifetime** 累计值，窗口化由 `quality` 承担。
            let st = lk(&rx.jbs);
            s.jb_depth_frames = st.jb.depth();
            s.jb_contiguous_frames = st.jb.contiguous();
            s.jb_popped = st.jb.popped;
            s.jb_plc = st.jb.plc_count;
            s.jb_silence = st.jb.silence_count;
            s.jb_dropped = st.jb.dropped;
            s.jb_underruns = st.jb.underruns;
            s.jb_target_frames = st.jb.target();
            s.jb_prebuffering = st.jb.prebuffering();
        }
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
    // ---- P0a：逐级延迟会计（本侧）+ P0b/P1：对端分项与网络单程 ----
    //
    // 已经由装配层（`assemble_pipelines`）按流算完，这里只是原样装进去。
    // **这一行不许出现任何算术**：一旦它开始加工延迟数字，加工的对象就可能
    // 来自别的流，而 R8 说那是 N 倍假延迟。
    s.pipeline = pipeline;
    // ---- P0q：音质三分量 ----
    if let Some(rx) = &e.rx {
        // 站点级重复流嫌疑只对**真的进了混音求和**的流成立。一条只喂虚拟麦克风
        // 的会话没参与那次求和，不该替它背这口锅。
        let duplicate = (rx.is_spk || rx.monitor)
            && build_mix_health(inner).map_or(false, |h| h.duplicate_suspect);
        s.quality = build_quality(rx, s.rung, duplicate);
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
        origin: e.origin.label().to_string(),
        hal_slot: e.origin.slot(),
        hal_device: hal_device.map(str::to_string),
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
            // The HAL half of the sub-tick moved to haldev's own thread: the
            // device reconcile, the volume relay and the session coordinator
            // share one 200ms cadence there, and none of them may run on a
            // thread that also has 1s work to do.
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
        // 遥测的 1 s 心跳：喂漂移窗口、补 Q1 采样点。放在 ticker 而不是音频
        // 节拍上，是因为线性回归与 `Mutex` 都不允许出现在 10 ms 循环里
        // （规格附录约束 3）。
        sample_telemetry(&inner, &entries);
        for e in &entries {
            let live = e.conn.alive.load(Ordering::SeqCst);
            // P0b：把**本侧**这条流的分项回传给对端，好让它合成总延迟。
            //
            // 按流一条，**永不合并**（规格 §7.2 R8）：扇出时 N 条流共用一个源，
            // 各自报同一个 `src_fifo` 深度是正确的物理事实；合并成一条报文就等
            // 于替对端把它们加起来。
            //
            // 与 `Stats` 同一个 1 s 节拍、同一条控制通道。用的是与本机 UI **同
            // 一个** `local_pipeline`，两边不会各拼各的。
            if live {
                if let Some(p) = local_pipeline(&inner, e) {
                    let _ = e.conn.send_msg(&SessionMsg::StageReport {
                        stream_id: e.id,
                        stages: p.stages.iter().map(to_wire_stage).collect(),
                        local_ms: p.local_ms,
                        dev: p.dev,
                        // 本机时基。对端只拿它与我们之前发的 `seq_us` 相比
                        // （判乱序），绝不与它自己的时钟相减。
                        seq_us: inner.start.elapsed().as_micros() as u64,
                    });
                }
            }
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

#[cfg(test)]
mod telemetry_tests {
    use super::*;

    fn stage(id: StageId, samples: u32, rate: u32) -> PipelineStage {
        to_ipc_stage(
            StageDepth::new(id, samples, 48_000, rate, DropMode::Oldest),
            None,
        )
    }

    /// **规格附录约束 1 的执行点。** 任一已声明存在的分项测不到 ⇒ 整体 `None`。
    ///
    /// 用 0 填补会让蓝牙耳机（真实 +150~250 ms）看起来和模拟输出一样好——那正是
    /// 「RTT 冒充音频延迟」的同类错误，也是这整套遥测存在的理由。
    #[test]
    fn one_unreadable_stage_makes_the_whole_sum_none() {
        let good = vec![
            stage(StageId::JitterBuf, 960, 48_000),  // 20 ms
            stage(StageId::PostMix, 480, 48_000),    // 10 ms
        ];
        assert_eq!(sum_stage_ms(&good), Some(30.0));

        let mut with_hole = good.clone();
        with_hole.push(stage(StageId::PlayRing, 48_000, 0)); // rate=0 = 读不到
        assert_eq!(
            sum_stage_ms(&with_hole),
            None,
            "有一级读不到就必须整体 None —— 绝不能悄悄按 30 ms 报出去"
        );
    }

    /// 「一级都没有」不等于「延迟为 0」。
    #[test]
    fn an_empty_stage_list_is_none_not_zero() {
        assert_eq!(sum_stage_ms(&[]), None);
    }

    /// 播放环那一级的**接线**：`publish_play_ring` → `StageSlot` →
    /// `to_ipc_stage` 全程走生产代码，环是真的 `HeapRb`，深度由真的
    /// `AudioTx::push` 推出来。
    ///
    /// 上一版这条测试自己造了一个 44100 的 `StageDepth` 塞进 `to_ipc_stage`，
    /// 再断言出来还是 44100——生产代码把 `tx.dev_rate()` 换成 `48_000` 它一声
    /// 不吭。而设备真是 44.1k 时那个替换会让 1.000 秒的环报成 918 ms，
    /// −8.8% 的系统性低估，小到不会有人发现。
    #[test]
    fn the_play_ring_converts_at_the_device_rate_not_48k() {
        // 44.1k 设备：环容量恰好 = dev_rate = 1.000 秒。
        let (mut tx, mut sink) = audio::AudioTx::detached_for_test(44_100);
        let slot = StageSlot::new();

        // 上一轮 tick 推进去 100 ms 的音频（4410 @44.1k），声卡还一个都没取走；
        // 本轮 tick 在 push **之前**读深度，与 mixer_loop 的相位一致。
        tx.push(&vec![0.25f32; 4_410]);
        engine::publish_play_ring(&slot, &tx);
        let d = slot.load().expect("发布过就必须读得到");
        let s = to_ipc_stage(d, None);
        assert_eq!(s.id, "play_ring");
        assert_eq!(s.samples, 4_410, "环里排着的就是刚 push 进去的那些");
        assert_eq!(s.capacity, 44_100, "播放环容量 = 1 秒设备速率");
        assert_eq!(s.rate, 44_100, "**设备**速率，不是 48000");
        assert_eq!(s.ms, Some(100.0), "4410 / 44100 = 100 ms（按 48k 算会是 91.875）");
        assert_eq!(s.drop_mode, DropMode::Newest, "push_slice 短写：丢的是新样本");
        assert_eq!(s.dropped, Some(0));
        assert!(!s.saturated);

        // 声卡取走 4000 之后深度必须跟着落下去——`samples` 若接的是
        // `capacity()` 或别的常量，这一步不会动。
        assert_eq!(sink.drain(4_000), 4_000);
        engine::publish_play_ring(&slot, &tx);
        let s = to_ipc_stage(slot.load().unwrap(), None);
        assert_eq!(s.samples, 410, "取走 4000 后只剩 410");
        assert_eq!(s.capacity, 44_100, "容量不随深度变");
        assert!(s.ms.unwrap() < 10.0);
    }

    /// 播放环满了以后丢的是**新**样本，而且现在数得出来——这里此前是全链路
    /// 唯一完全无遥测的丢弃点（`let _ = push_slice(..)` 静默丢尾）。
    ///
    /// 与源侧 FIFO 的「丢最旧」在深度读数上完全简并（两者饱和时都恰好
    /// 1000 ms），只有 `drop_mode` + `dropped` 的增长能把它们分开：丢最旧是
    /// 「恒定迟到但连续」，丢最新是「迟到 + 周期性断续」（规格 §0.2）。
    #[test]
    fn a_full_play_ring_drops_the_newest_and_counts_it() {
        let (mut tx, _sink) = audio::AudioTx::detached_for_test(48_000);
        let slot = StageSlot::new();
        // 环 = 48000。灌 60000 进去。
        tx.push(&vec![0.25f32; 60_000]);
        engine::publish_play_ring(&slot, &tx);
        let s = to_ipc_stage(slot.load().unwrap(), None);
        assert_eq!(s.samples, s.capacity, "环被灌满了");
        assert!(s.saturated);
        assert_eq!(s.ms, Some(1000.0), "满环 = 1.000 秒排队");
        assert_eq!(
            s.dropped,
            Some(60_000 - s.capacity as u64),
            "写不进去的部分必须数得出来"
        );
        assert_eq!(s.drop_mode, DropMode::Newest);
    }

    /// 没有播放环的那一 tick 必须清槽，否则报告线程会一直读到最后一次的陈旧
    /// 深度——那是「静默缺项」的另一种形态：设备已经没了，UI 还在显示 400 ms。
    #[test]
    fn clearing_the_slot_reports_absence_not_the_last_reading() {
        let (mut tx, _sink) = audio::AudioTx::detached_for_test(48_000);
        let slot = StageSlot::new();
        tx.push(&vec![0.25f32; 4_800]);
        engine::publish_play_ring(&slot, &tx);
        assert!(slot.load().is_some());
        slot.store(None); // mixer_loop 在没有流 / 没有设备时做的事
        assert_eq!(slot.load(), None);
    }

    /// PostMix 溢出丢的是**最旧的**样本，且现在数得出来。
    /// 丢弃行为本身与改动前逐字相同（`drain(..excess)`）。
    #[test]
    fn post_mix_overflow_drops_oldest_and_counts_it() {
        let mut pm = PostMix { fifo: VecDeque::new(), dropped: 0 };
        let mut out = [0.0f32; 480];
        // 灌进远超 100 ms 上限的音频：6000 样本 -> 取走 480 -> 剩 5520 > 4800
        pm.advance(Some(vec![0.5; 6000]), &mut out);
        assert_eq!(pm.fifo.len(), POST_MIX_CAP, "上限未变，仍是 100 ms");
        assert_eq!(pm.dropped, 720, "5520 - 4800 = 720 个最旧的样本被丢掉");

        let d = pm.depth();
        assert_eq!(d.drop_mode, DropMode::Oldest);
        assert_eq!(d.dropped, Some(720));
        assert_eq!(d.ms(), Some(100.0), "满的 PostMix 恰好 100 ms");
        assert!(d.saturated());
    }

    /// 没溢出时不能凭空记丢弃。
    #[test]
    fn post_mix_within_budget_drops_nothing() {
        let mut pm = PostMix { fifo: VecDeque::new(), dropped: 0 };
        let mut out = [0.0f32; 480];
        pm.advance(Some(vec![0.5; 960]), &mut out);
        assert_eq!(pm.dropped, 0);
        assert_eq!(pm.depth().samples, 480);
        assert_eq!(pm.depth().ms(), Some(10.0));
    }

    /// 一条不需要 TCP 连接的真接收流：`RxStream::new` 是生产构造器，里面的
    /// `JitterBuffer` / `PostMix` / `ConcealWindow` 全是真的。
    fn rx_stream() -> RxStream {
        RxStream::new(
            7,
            &[0u8; 32],
            &[0u8; 12],
            None,
            true,  // is_spk
            false, // monitor
            None,
            None,
            "127.0.0.1:1".parse().unwrap(),
        )
    }

    fn jb_frame() -> Vec<f32> {
        vec![0.1; 480]
    }

    /// 给 Q1 的 10 s 非消费型窗口塞两个点，跨度足够长。
    fn seed_conceal(rx: &RxStream, popped: u64, plc: u64) {
        let now = Instant::now();
        let then = now.checked_sub(Duration::from_secs(3)).unwrap_or(now);
        let mut st = lk(&rx.jbs);
        st.conceal.sample(then, quality::JbCounts::default());
        st.conceal.sample(
            now,
            quality::JbCounts { popped, plc, silence: 0, underruns: 0, dropped: 0 },
        );
    }

    /// 把一条 `ClipMeter` 喂到翻页，内容是**全程越界**的响帧。
    fn flip_a_loud_clip_page(m: &quality::ClipMeter) {
        let loud = [0.9f32; 480]; // 0.9 > 0.8 阈值 ⇒ 每个样本都算越界
        for t in 0..10u64 {
            m.feed(1_000 + t * 1_000, &loud);
        }
        m.feed(11_500, &[]); // 空帧只推时间，干净地翻一页
    }

    /// **Q2 缺席时不许填 0，也不许把在场分量的 min 当成结论。**
    ///
    /// `ClipMeter` 是 10 s 双缓冲，流开头那一页还没攒满。这里先后有过两版错法，
    /// 而且第二版看上去像已经修好了：
    ///
    /// 1. `None => (0.0, -120.0)`，于是 `grade_clip(0.0) = Excellent`。
    /// 2. 缺席「不进 min」，另加 `partial: true` 标注。
    ///
    /// `Grade::Excellent` 是 `Ord` 的最大值 ⇒ `min(q1, Excellent, q3)` 与
    /// `min(q1, q3)` **逐值相同**。第二版只是把缺席标注了出来，`grade` 这个
    /// 用户唯一会读到的字段照旧写着「良好」，原缺陷（流开头 10~20 秒里一条正在
    /// 爆音的流报「良好」）分毫未动。
    ///
    /// 所以这里断言的是 `grade == "unknown"`：它对上面两版**都**变红。
    #[test]
    fn an_unmeasured_clip_component_leaves_the_grade_undecided() {
        let rx = rx_stream();
        seed_conceal(&rx, 996, 4); // Q1 = (4+0)/1000 = 0.4% -> Good
        // 削顶页还没攒满
        assert!(rx.clip.window().is_none(), "前提：这一页还没完成");

        let q = build_quality(&rx, 0, false).expect("Q1/Q3 有读数 ⇒ 分量明细照给");
        assert_eq!(q.clip_ratio, None, "还没测 ⇒ None。填 0 会说成『测了，一点没削』");
        assert_eq!(q.clip_excess_db, None);
        assert!(q.partial, "木桶少了一块板，必须说出来");
        assert_eq!(
            q.grade, "unknown",
            "上面两版错法在这里都会给出 \"good\" —— 那就是用户看到的那个『良好』"
        );
        assert_eq!(q.worst, "none", "等级都没定，谈不上谁拖后腿");
        // 缺的是等级，不是全部：已经测出来的两个分量照常上报，UI 才能在
        // 「测量中」的同时把连续性与带宽画出来。
        assert!(q.conceal_ratio > 0.0 && q.bandwidth_hz == 24_000);

        // 同一条流，页攒满之后：Q2 立刻把等级拉到底，并指名是电平的问题。
        flip_a_loud_clip_page(&rx.clip);
        let q = build_quality(&rx, 0, false).expect("三分量齐全");
        assert_eq!(q.clip_ratio, Some(1.0), "整页都在越界");
        assert!(!q.partial);
        assert_eq!(q.grade, "poor");
        assert_eq!(q.worst, "level");
        assert!(q.clip_excess_db.unwrap() > 0.0, "越过 0.8 ⇒ 正的 dB 余量");
    }

    /// 缺席造成的「测量中」有一个边界：**在场分量已经触底时等级照报**。
    ///
    /// 否则就走到了另一个极端——把一条**已经确定是「差」**的流藏进「测量中」，
    /// 用不确定性掩盖一个确定的坏消息。区间 `[差, 差]` 退化成一个点，缺的那块
    /// 板再短也改不了结论。
    #[test]
    fn a_floored_grade_is_still_reported_while_the_clip_page_is_missing() {
        let rx = rx_stream();
        seed_conceal(&rx, 900, 100); // Q1 = 100/1000 = 10% -> Poor
        assert!(rx.clip.window().is_none(), "前提：削顶页仍未完成");

        let q = build_quality(&rx, 0, false).expect("有结论");
        assert_eq!(q.grade, "poor", "断续已经触底，削顶再差也压不下去");
        assert_eq!(q.worst, "continuity");
        assert!(q.partial, "等级确定，但木桶确实少了一块板 —— 两件事都要说");
        assert_eq!(q.clip_ratio, None);
    }

    /// 站点级混音健康同病：`unwrap_or(0.0)` 把启动后头 10 秒的空窗报成
    /// 「混音正常」。改成 `Option` 之后，「还没测」在 JSON 上就是 `null`。
    #[test]
    fn mix_health_reports_an_unmeasured_clip_page_as_null() {
        let h = MixHealth {
            window_s: 10.0,
            clip_ratio: None,
            clip_excess_db: None,
            max_contrib: 2,
            corr_peak: None,
            duplicate_suspect: false,
        };
        let v: serde_json::Value = serde_json::to_value(&h).unwrap();
        assert!(v["clip_ratio"].is_null(), "还没测 ⇒ null，不是 0");
        assert!(v["clip_excess_db"].is_null());
        let measured = MixHealth { clip_ratio: Some(0.0), clip_excess_db: Some(-120.0), ..h };
        let v: serde_json::Value = serde_json::to_value(&measured).unwrap();
        assert_eq!(v["clip_ratio"], 0.0, "真的 0 必须与 null 区分得开");
    }

    /// 重复流的一票否决**不依赖**本流的削顶页：它是站点级的实测结论。
    /// 所以哪怕 Q2 还没测，这条判据照样把等级钉到底——而且不算 partial。
    #[test]
    fn the_duplicate_veto_still_bites_before_the_clip_page_completes() {
        let rx = rx_stream();
        seed_conceal(&rx, 1000, 0); // Q1 = 0 -> Excellent
        assert!(rx.clip.window().is_none());
        let q = build_quality(&rx, 0, true).expect("有结论");
        assert_eq!(q.grade, "poor", "两路重复流相加把整段波形 ×2");
        assert_eq!(q.worst, "level");
        assert!(!q.partial, "一票否决是实测结论，不是缺席");
        assert_eq!(q.clip_ratio, None, "但本流自己的削顶率确实还没测出来");
    }

    /// **抖动缓冲那一级必须用 `contiguous()`，不是 `depth()`。**
    ///
    /// 上一版这条测试自己造了两个 `StageDepth` 字面量（一个装 2 帧一个装 5 帧）
    /// 再比较它们的 ms，注释却写着「防止将来有人换回 depth()」——真换回去它全绿。
    /// 这一版让 `build_pipeline_from` 去读一个**真的**、队首有洞的
    /// `JitterBuffer`：`depth()` 会把洞之后的帧也算进排队，谎报 30 ms。
    #[test]
    fn the_jitter_stage_uses_contiguous_so_a_hole_cannot_inflate_it() {
        let rx = rx_stream();
        {
            let mut st = lk(&rx.jbs);
            // 起播：next_seq 落在 11。
            st.jb.push(10, jb_frame());
            st.jb.push(11, jb_frame());
            assert!(st.jb.pop().is_some());
            // 12 缺失，13/14 提前到达 ⇒ 表里 3 帧，连续的只有 11 那一帧。
            st.jb.push(13, jb_frame());
            st.jb.push(14, jb_frame());
            assert_eq!(st.jb.depth(), 3, "BTreeMap 里确实有 3 帧");
            assert_eq!(st.jb.contiguous(), 1, "但只有 1 帧真的排得上队");
        }

        let p = build_pipeline_from(false, None, Some(&rx)).expect("接收侧必须有分项");
        let jb = p
            .stages
            .iter()
            .find(|s| s.id == "jitter_buf")
            .expect("抖动缓冲这一级");
        assert_eq!(
            jb.samples,
            1 * F48_PER_FRAME,
            "1 帧连续 = 480 样本；用 depth() 会报 1440"
        );
        assert_eq!(
            jb.ms,
            Some(10.0),
            "10 ms。用 depth() 会谎报 30 ms —— 下一个 tick 其实一定 underrun"
        );
        assert_eq!(jb.rate, 48_000);
        assert_eq!(
            jb.capacity,
            (2 + 6) * F48_PER_FRAME,
            "硬上限是 target+6 帧（pop 里的修剪条件），不是某个固定常数"
        );
        assert_eq!(
            jb.dropped, None,
            "JB 的 dropped 是**帧**计数，量纲不同 —— 单独以 jb_dropped 上报，这里如实说观测不到"
        );
        assert_eq!(jb.drop_mode, DropMode::Oldest);
    }

    /// 队首本身缺失 = 一个样本都排不上队。此时 `depth()` 谎报 30 ms，
    /// 而下一个 tick 一定 underrun——排队深度实际是 0。
    #[test]
    fn a_jitter_buffer_stalled_behind_a_hole_reports_zero_queue() {
        let rx = rx_stream();
        {
            let mut st = lk(&rx.jbs);
            // 100/101 连续 + 103 提前到，还没起播 ⇒ 队首是 100。
            st.jb.push(100, jb_frame());
            st.jb.push(101, jb_frame());
            st.jb.push(103, jb_frame());
            assert_eq!(st.jb.depth(), 3);
            assert_eq!(st.jb.contiguous(), 2);
        }
        let p = build_pipeline_from(false, None, Some(&rx)).unwrap();
        let jb = p.stages.iter().find(|s| s.id == "jitter_buf").unwrap();
        assert_eq!(jb.ms, Some(20.0), "100/101 连续 = 20 ms；depth 会报 30 ms");
    }

    /// 接收侧管线的整体形状：两级、顺序、Σ、confidence、设备项恒缺。
    /// 走的是 `build_pipeline_from`，与 `build_pipeline(&SessionEntry)` 同一段
    /// 代码——后者只是把 `SessionEntry` 拆开的一行胶水。
    #[test]
    fn a_receive_pipeline_carries_the_jitter_buffer_then_the_post_mix() {
        let rx = rx_stream();
        {
            let mut st = lk(&rx.jbs);
            for seq in 0..4 {
                st.jb.push(seq, jb_frame());
            }
            assert!(st.jb.pop().is_some()); // 起播，放掉一帧 ⇒ 连续 3 帧 = 30 ms
        }
        // PostMix 里压着 960 个样本 = 20 ms（灌 1440 进去、取走 480）。
        {
            let mut out = [0.0f32; 480];
            lk(&rx.post).advance(Some(vec![0.5; 1_440]), &mut out);
        }

        let p = build_pipeline_from(false, None, Some(&rx)).expect("有分项");
        assert_eq!(p.side, "recv");
        let ids: Vec<&str> = p.stages.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["jitter_buf", "post_mix"], "按数据流顺序");
        assert_eq!(p.stages[0].ms, Some(30.0));
        assert_eq!(p.stages[1].ms, Some(20.0));
        assert_eq!(p.local_ms, Some(50.0), "Σ 本侧各级");
        assert!(
            matches!(p.confidence, LatConfidence::LocalOnly),
            "P0a 只有本侧分项，对端没上报 —— 不猜、不用 RTT 顶替"
        );
        assert!(p.net_ms.is_none(), "RTT 只能当一段，且现在还没有可信的单程值");
        assert!(p.sum_ms.is_none(), "对端分项缺失 ⇒ 总和无从谈起，绝不用 0 填补");
        assert_eq!(
            p.dev.expect("设备项这个结构必须在").ms(),
            None,
            "P0 读不到声卡固有缓冲 ⇒ None，绝不填 0（否则蓝牙耳机看起来和模拟输出一样好）"
        );
    }

    /// 发送侧：`TxShared` 的两个原子槽里发布了什么，管线里就出现什么，
    /// 顺序、id、ms 一路穿到 IPC。空槽不占位。
    #[test]
    fn a_send_pipeline_emits_exactly_the_slots_that_were_published() {
        let tx = TxShared::new();
        // 只发布一级（`SysAudio` / `HalSpeaker` 就是这种源）。
        tx.stages[0].store(Some(StageDepth {
            id: StageId::SrcFifo,
            samples: 24_000,
            capacity: 48_000,
            rate: 48_000,
            dropped: Some(11),
            drop_mode: DropMode::Oldest,
        }));
        let p = build_pipeline_from(true, Some(&tx), None).expect("有分项");
        assert_eq!(p.side, "send");
        assert_eq!(p.stages.len(), 1, "第二个槽是空的 ⇒ 不占位，不报 0 样本的假读数");
        assert_eq!(p.stages[0].id, "src_fifo");
        assert_eq!(p.stages[0].ms, Some(500.0));
        assert_eq!(p.stages[0].dropped, Some(11));
        assert_eq!(p.local_ms, Some(500.0));

        // 补上第二级（`MicSource` 那种两级源），顺序必须是槽序。
        tx.stages[1].store(Some(StageDepth {
            id: StageId::CapRing,
            samples: 4_410,
            capacity: 88_200,
            rate: 44_100,
            dropped: Some(0),
            drop_mode: DropMode::Newest,
        }));
        let p = build_pipeline_from(true, Some(&tx), None).unwrap();
        let ids: Vec<&str> = p.stages.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["src_fifo", "cap_ring"]);
        assert_eq!(p.local_ms, Some(600.0), "500 + 100，两级速率不同也照样求和");
    }

    /// **发送侧：源换掉之后，新源不许继承旧源的斜率。**
    ///
    /// 接收侧的 `play_ring` 与两条并行尾级早就写着「槽空就清历史，否则下一次
    /// 开桥会继承上一次的斜率」，而同一个函数里的发送侧分支只有
    /// `if let Some(d) = slot.load() { push }` —— 一个 `else` 都没有。
    /// `TxShared` 比源活得长（会话表还持有它），所以这条捷径的代价是：默认输入
    /// 设备一变、`MicSource` 重建，新源开头 30 s 都背着旧源的斜率上报
    /// `drift_sps`，把一条刚开的干净流说成正在走向饱和。
    ///
    /// 这条测试走的是**接线**（`sample_tx_drift` → `build_pipeline_from`），不是
    /// `DriftTracker` 本身：漏掉的正是接线，只测 `DriftTracker` 的用例全绿。
    #[test]
    fn a_send_stream_drops_its_drift_history_when_the_source_is_replaced() {
        let fifo = |samples: u32| {
            Some(StageDepth::new(
                StageId::SrcFifo,
                samples,
                48_000,
                48_000,
                DropMode::Oldest,
            ))
        };
        let slope_now = |tx: &TxShared| {
            build_pipeline_from(true, Some(tx), None).unwrap().stages[0].drift_sps
        };

        let tx = TxShared::new();
        // 旧源：发送 FIFO 一路涨向饱和，30 s 里 +480 样本/秒。
        for i in 0..=30u32 {
            tx.stages[0].store(fifo(480 * i));
            sample_tx_drift(i as f32, &tx);
        }
        let before = slope_now(&tx).expect("旧源确实在漂");
        assert!((before - 480.0).abs() < 1.0, "前提：旧源斜率 ≈ +480, got {before}");

        // 源被收尸：`tx_loop` 的 clear_send_stages 把槽清空，`TxShared` 还活着。
        tx.stages[0].store(None);
        sample_tx_drift(31.0, &tx);
        assert!(
            build_pipeline_from(true, Some(&tx), None).is_none(),
            "槽空了就没有任何可读的级 —— 这一刻正是历史该被断掉的那一刻"
        );

        // 新源接上，稳态完全不漂。
        for i in 32..=46u32 {
            tx.stages[0].store(fifo(9_600));
            sample_tx_drift(i as f32, &tx);
        }
        let after = slope_now(&tx).expect("新源已攒够点");
        assert!(
            after.abs() < 1e-6,
            "新源是平的，斜率必须 ≈ 0；不清历史时旧源那段上升还压在窗口里，\
             最小二乘会给出 ≈ -15 样本/秒, got {after}"
        );
    }

    /// 一条没有任何可读级的会话报 `None`，不是一个空壳 `PipelineLatency`。
    #[test]
    fn a_session_with_no_readable_stage_reports_no_pipeline_at_all() {
        assert!(build_pipeline_from(true, None, None).is_none());
        // ...空的 `TxShared`（源还没发布过任何一级）同理。
        let tx = TxShared::new();
        assert!(build_pipeline_from(true, Some(&tx), None).is_none());
    }

    /// 每一级的 id 都必须能被前端认出来。漏一条就是那一级静默显示「未知」。
    ///
    /// ## 这条测试为什么不再自己列一张「已知 id」表
    ///
    /// 上一版在这里放了一个手抄的 `known = ["cap_ring", ...]` 字面量，声称它代表
    /// 前端的 `LATENCY_STAGES`。**那样的断言永远不可能为它命名的那个漂移变红**：
    /// 抄件与被抄件是同一只手写的，前端少三级它照样全绿（它当时就是全绿的，还
    /// 带着一条「前端缺三级」的注释）。
    ///
    /// 现在拆成两条各自可证伪的链子，中间没有任何手抄环节：
    ///   1. `audiohub-core` 的 `the_frontend_stage_table_matches_the_rust_enum_exactly`
    ///      **去读 metrics.ts**，断言前端表 ≡ `StageId` 全集；
    ///   2. 这里断言「我们发射出去的每个 id 都能反解析回 `StageId`」——即
    ///      发射集 ⊆ 枚举全集。
    /// 两条相乘就是「发射的每一级前端都认识」，而每一条单独都会为真实的漂移变红。
    ///
    /// 反解析本身也是**运行时**必需的：`sum_stage_ms` 靠它把并行尾级从串联链里
    /// 认出来，认不出来就会把两条 1 秒环相加，报出 2 秒的假延迟。
    #[test]
    fn every_stage_we_emit_carries_a_frontend_known_id() {
        for id in [
            StageId::CapRing,
            StageId::SrcFifo,
            StageId::HalSpk,
            StageId::SendPace,
            StageId::JitterBuf,
            StageId::PostMix,
            StageId::PlayRing,
            StageId::BridgeRing,
            StageId::HalMic,
        ] {
            let s = to_ipc_stage(StageDepth::new(id, 0, 0, 48_000, DropMode::None), None);
            assert_eq!(
                StageId::from_id_str(&s.id),
                Some(id),
                "{} 的 id 无法反解析 —— 前端认不出它，并行尾级的判定也会失效",
                s.id
            );
        }
    }

    /// **并行尾级取 max，不相加。**
    ///
    /// 一条同时开了「监听接收音频」与「桥接到虚拟声卡」的会话有两条 1 秒环
    /// （站点播放环 + 该桥自己的 `AudioTx`），一帧解码结果被**同时**送进两者。
    /// 用户从任一条听到的延迟是那一条的驻留，不是两条之和；直接相加会报出
    /// 2000 ms 的假延迟，比它要诊断的那个 1 秒故障还大一倍。
    #[test]
    fn parallel_output_tails_take_the_max_instead_of_summing() {
        let serial = vec![
            stage(StageId::JitterBuf, 960, 48_000), // 20 ms
            stage(StageId::PostMix, 480, 48_000),   // 10 ms
        ];
        assert_eq!(sum_stage_ms(&serial), Some(30.0));

        let mut both = serial.clone();
        both.push(stage(StageId::PlayRing, 48_000, 48_000)); // 1000 ms
        both.push(stage(StageId::BridgeRing, 24_000, 48_000)); // 500 ms
        assert_eq!(
            sum_stage_ms(&both),
            Some(1030.0),
            "30 + max(1000, 500)；相加会给出 1530 ms 的假延迟"
        );

        // 顺序无关（max 不看谁先出现）
        let mut swapped = serial.clone();
        swapped.push(stage(StageId::BridgeRing, 24_000, 48_000));
        swapped.push(stage(StageId::PlayRing, 48_000, 48_000));
        assert_eq!(sum_stage_ms(&swapped), sum_stage_ms(&both));

        // 只有一条尾级时与从前逐字相同
        let mut one = serial.clone();
        one.push(stage(StageId::HalMic, 24_000, 48_000));
        assert_eq!(sum_stage_ms(&one), Some(530.0));

        // 串联级仍然逐条相加（尾级的例外不能溢出到别的级）
        let mut three_serial = serial.clone();
        three_serial.push(stage(StageId::SrcFifo, 4_800, 48_000)); // 100 ms
        three_serial.push(stage(StageId::CapRing, 4_800, 48_000)); // 100 ms
        assert_eq!(sum_stage_ms(&three_serial), Some(230.0));
    }

    /// 一条**纯桥接**流的尾级此前完全没有建模：它不碰站点播放环
    /// （`is_spk || monitor` 都是 false），走的是每个桥自己的 `AudioTx`
    /// ——同样 1 秒。结果是 `local_ms` 只有 jitter_buf + post_mix，
    /// **静默漏掉整整一秒**，且不降 confidence、不标注缺席。
    #[test]
    fn a_bridge_only_stream_no_longer_loses_a_whole_second() {
        let before = vec![
            stage(StageId::JitterBuf, 960, 48_000), // 20 ms
            stage(StageId::PostMix, 480, 48_000),   // 10 ms
        ];
        assert_eq!(sum_stage_ms(&before), Some(30.0), "这就是从前上报的数字");
        let mut after = before.clone();
        after.push(stage(StageId::BridgeRing, 48_000, 48_000)); // 满环 = 1 s
        assert_eq!(
            sum_stage_ms(&after),
            Some(1030.0),
            "少这一级就等于把 1000 ms 报成 30 ms —— 相差 34 倍"
        );
    }

    /// 级 4 `send_pace` 真的出现在组装结果里。
    ///
    /// 它此前在 `StageId` 里声明、在规格 §3.2 里编号，**全仓库零发布点**：
    /// 发送侧的 `local_ms` 因此系统性短 5 ms，而且没有任何字段标出它缺席
    /// ——「静默缺席」正是这套遥测存在的理由所反对的那种失败形态。
    #[test]
    fn a_send_pipeline_carries_the_framing_pace_stage() {
        let tx = TxShared::new();
        tx.stages[0].store(Some(StageDepth::new(
            StageId::SrcFifo,
            4_800, // 100 ms
            48_000,
            48_000,
            DropMode::Oldest,
        )));
        tx.stages[2].store(Some(StageDepth::send_pace()));
        let p = build_pipeline_from(true, Some(&tx), None).expect("有分项");
        let ids: Vec<&str> = p.stages.iter().map(|s| s.id.as_str()).collect();
        assert!(ids.contains(&"send_pace"), "组帧节拍必须出现在分项里, got {ids:?}");
        assert_eq!(p.local_ms, Some(105.0), "100 + 5；漏掉节拍会报 100");
        let pace = p.stages.iter().find(|s| s.id == "send_pace").unwrap();
        assert_eq!(pace.ms, Some(5.0));
        assert!(!pace.saturated, "它不是队列，不该被判饱和");
        assert_eq!(pace.drop_mode, DropMode::None);
    }

    /// P0a 的置信度只能是 LocalOnly，且序列化成前端字面量里那个 camelCase。
    #[test]
    fn confidence_serialises_to_the_frontend_literal() {
        let j = |c: LatConfidence| serde_json::to_string(&c).unwrap();
        assert_eq!(j(LatConfidence::LocalOnly), "\"localOnly\"");
        assert_eq!(j(LatConfidence::LowerBound), "\"lowerBound\"");
        assert_eq!(j(LatConfidence::Full), "\"full\"");
        assert_eq!(j(LatConfidence::Converging), "\"converging\"");
        assert_eq!(j(LatConfidence::Unavailable), "\"unavailable\"");
    }

    /// 冻结 UI 真正会看到的 JSON 形状。前端 `lib/metrics.ts` 的读取入口按这些
    /// 键名取值，改名即断线——所以键名进断言，不进注释。
    ///
    /// 用的样例数据是规格 §6.3 的**故障注入 C**：`hal_spk` 里恒定驻留 400 ms
    /// （19200 / 24000 帧）。这是全规格最有教育意义的一组读数——
    /// **不饱和（80%）、不丢弃、进出速率严格相等，但恒定迟到 400 ms**。
    /// 父任务原有的证据（90.25 s 内写入 = 读出，均 9025 帧）在结构上**无法
    /// 证伪**这个场景，而新遥测直接把它显示成 400 ms。所以这里断言
    /// `saturated == false` 不是将就，正是要点：靠「是否饱和」判断这一级健不
    /// 健康，恰好会漏掉它。
    #[test]
    fn the_emitted_json_shape_is_what_the_frontend_reads() {
        let p = PipelineLatency {
            side: "send".into(),
            stages: vec![to_ipc_stage(
                StageDepth {
                    id: StageId::HalSpk,
                    samples: 19_200,
                    capacity: 24_000,
                    rate: 48_000,
                    dropped: None,
                    drop_mode: DropMode::Newest,
                },
                Some(0.0),
            )],
            local_ms: Some(400.0),
            dev: Some(DevLatency::unavailable()),
            peer_stages: Vec::new(),
            peer_local_ms: None,
            peer_dev: None,
            peer_age_s: None,
            net_ms: None,
            rtt_cross_check_ms: None,
            sum_ms: None,
            e2e_ms: None,
            residual_ms: None,
            clock_offset_us: None,
            clock_unc_us: None,
            confidence: LatConfidence::LocalOnly,
        };
        let v: serde_json::Value = serde_json::to_value(&p).unwrap();
        assert_eq!(v["confidence"], "localOnly");
        assert_eq!(v["side"], "send");
        assert_eq!(v["local_ms"], 400.0);
        assert!(v["sum_ms"].is_null(), "对端分项缺失 ⇒ 总和必须是 null");
        let st = &v["stages"][0];
        assert_eq!(st["id"], "hal_spk", "级 id 与 metrics.ts 的 LATENCY_STAGES 一致");
        assert_eq!(st["ms"], 400.0);
        assert_eq!(st["drop_mode"], "newest");
        assert_eq!(
            st["saturated"], false,
            "80% 不算饱和 —— 这一级恒定迟到 400 ms 却完全不饱和，正是旧证据看不见它的原因"
        );
        // drift ≈ 0 + 不饱和 + dropped 不可观测 ⇒ 「收支平衡但永远迟到」。
        assert_eq!(st["drift_sps"], 0.0);
        assert!(st["dropped"].is_null(), "驱动侧的丢弃观测不到 ⇒ null，不是 0");
        assert_eq!(v["dev"]["source"], "unavailable");

        let q = QualityStats {
            window_s: 10.0,
            conceal_ratio: 0.004,
            plc_ticks: 4,
            silence_ticks: 0,
            popped_ticks: 996,
            underruns: 1,
            jb_dropped: 0,
            clip_ratio: Some(0.31),
            clip_excess_db: Some(6.02),
            bandwidth_hz: 24_000,
            grade: "poor".into(),
            worst: "level".into(),
            partial: false,
        };
        let v: serde_json::Value = serde_json::to_value(&q).unwrap();
        assert_eq!(v["grade"], "poor");
        assert_eq!(v["worst"], "level");
        assert_eq!(v["bandwidth_hz"], 24_000);
        assert_eq!(v["window_s"], 10.0);
        assert_eq!(v["clip_ratio"], 0.31);
        assert_eq!(v["partial"], false);

        // 削顶页还没攒满：**null 而不是 0**，并且 partial 说出「木桶少了一块板」。
        // 这两个字段是「还没测」与「测了，确实静音」之间唯一的区别。
        let unmeasured = QualityStats { clip_ratio: None, clip_excess_db: None, partial: true, ..q };
        let v: serde_json::Value = serde_json::to_value(&unmeasured).unwrap();
        assert!(v["clip_ratio"].is_null(), "还没测 ⇒ null，绝不是 0");
        assert!(v["clip_excess_db"].is_null());
        assert_eq!(v["partial"], true);
    }

    /// 新增字段全部 `#[serde(default)]`：v1 客户端存的旧 JSON 仍能反序列化。
    /// 这是「IPC_VERSION 升到 2 只是能力标记、不是不兼容变更」那句话的凭据。
    #[test]
    fn a_v1_shaped_session_stats_still_deserialises() {
        let old = r#"{
            "received": 10, "lost": 0, "loss_pct": 0.0, "jitter_ms": 0.4,
            "bitrate_kbps": 742.0, "jb_depth_frames": 3, "sent_packets": 0,
            "rung": 0, "rung_changes": 0, "verdict": null, "mix_verdicts": null
        }"#;
        let s: SessionStats = serde_json::from_str(old).expect("v1 形状必须仍可解析");
        assert_eq!(s.received, 10);
        assert!(s.pipeline.is_none(), "缺席的新字段是 None，不是 0 值的空壳");
        assert!(s.quality.is_none());
        assert_eq!(s.jb_popped, 0);
        assert_eq!(s.jb_contiguous_frames, 0);
    }

    // ================================================== P1：时钟偏移与网络单程

    /// 一次 Ping/Pong 往返的**物理**过程，翻译成 `note_pong` 的三个入参。
    ///
    /// - `depart_us`：本机时基下发出 Ping 的时刻。
    /// - `theta_us`：**真值** θ = 同一物理瞬间「本机时钟读数 − 对端时钟读数」。
    ///   这正是待估量，也正是 `SessionMsg::Pong` 文档里那个符号约定。
    /// - `fwd_us` / `rev_us`：去程 / 回程单程时延。
    ///
    /// 由此 `θ̂ = (t1+t4)/2 − t2 = θ + (rev − fwd)/2`：**误差只来自路径不对称**，
    /// 与两台机器的时钟差多大完全无关。min-RTT 滤波要抓的就是 `rev ≈ fwd` 的
    /// 那一次往返。
    fn round_trip(
        depart_us: u64,
        theta_us: i64,
        fwd_us: u64,
        rev_us: u64,
    ) -> (u64, u64, Option<u64>) {
        let t1 = depart_us;
        let t4 = depart_us + fwd_us + rev_us;
        // 对端在物理瞬间「发出后 fwd」读自己的钟。那一刻本机会读到 t1 + fwd，
        // 而对端读数 = 本机读数 − θ。
        let t2 = (t1 + fwd_us) as i64 - theta_us;
        (t1, t4, Some(t2 as u64))
    }

    fn feed(f: &mut ClockFilter, n: usize, theta_us: i64, fwd_us: u64, rev_us: u64) {
        for i in 0..n {
            let (t1, t4, t2) = round_trip(1_000_000 * (i as u64 + 1), theta_us, fwd_us, rev_us);
            assert_eq!(f.note_pong(t1, t4, t2), PongOutcome::Ok);
        }
    }

    /// 窗口没攒够 8 个样本之前**什么都不报**（规格 §3.3 明确要求 ≥8）。
    ///
    /// 这一条同时管着 `net_ms`：min-RTT 在样本少时系统性偏大（min 随样本增加
    /// 只降不升），而红线要求 RTT 只能是六段里最小的那一段。把门槛调低会让
    /// 一个偏大的网络段在连接刚建立的几秒里被塞进总数。
    #[test]
    fn nothing_is_reported_before_the_window_holds_eight_samples() {
        let mut f = ClockFilter::new();
        feed(&mut f, 7, 0, 150, 150);
        assert!(f.estimate().is_none(), "7 个样本 ⇒ 不出结论");
        feed(&mut f, 1, 0, 150, 150);
        assert!(f.estimate().is_some(), "第 8 个样本到齐 ⇒ 可以出结论");
    }

    /// **θ 取自 min-RTT 那一个样本，不是平均。**
    ///
    /// 排队延迟是单边噪声（只让往返变长），所以最小的那次往返排队最少、θ 最准。
    /// 这里 7 次往返严重不对称（去 10 ms / 回 0.2 ms ⇒ 每个样本偏 −4.9 ms），
    /// 只有 1 次干净（去回各 0.15 ms）。改成平均 / 取最新，θ 会偏出 4 ms 以上，
    /// 而 `clock_unc_us` 报的却仍是 0.15 ms —— 一个既错又自称很准的读数。
    ///
    /// ⚠ 干净的那一次**故意夹在中间**。第一版把它放在最后，于是
    /// 「取最新」与「取 min-RTT」给出同一个答案，这条测试对那个 bug 完全无感
    /// （变异检验当场抓到：注入 `win.back()` 之后它照样绿）。
    #[test]
    fn the_offset_comes_from_the_smallest_round_trip_not_the_average() {
        const TRUTH_US: i64 = -3_000_000; // 对端 daemon 比本机早启动 3 s
        let mut f = ClockFilter::new();
        feed(&mut f, 4, TRUTH_US, 10_000, 200); // 脏样本：每个偏 −4900 µs
        feed(&mut f, 1, TRUTH_US, 150, 150); // 干净样本：偏 0，夹在中间
        feed(&mut f, 3, TRUTH_US, 10_000, 200);
        let e = f.estimate().expect("8 个样本");
        assert_eq!(e.min_rtt_us, 300, "最小往返 = 干净那一次");
        assert_eq!(e.offset_us, Some(TRUTH_US), "θ 必须是干净样本给出的真值");
        assert_eq!(e.unc_us, 150, "不确定度 = min_RTT/2（规格 §3.4）");
        // 若换成平均，θ 会落在这里 —— 与真值差 4 ms 以上。
        let mean = TRUTH_US - 4_900 * 7 / 8;
        assert!(
            (e.offset_us.unwrap() - mean).abs() > 4_000,
            "报出来的 θ 不能是平均值（平均 = {mean}）"
        );
    }

    /// **RTT 全程只用一个时基。**
    ///
    /// 上一轮排查栽在「两个不一致时基相除」上（系统误差底 143 ppm，比待测效应
    /// 还大）。这里把对端时钟推开 3 天：`t1`/`t4` 都取自本机 `DaemonInner::start`，
    /// 所以 RTT 必须**纹丝不动**仍是 300 µs。谁要是拿 `peer_t_us` 参与 RTT
    /// （`t2 − t1` 之类），这个数会变成 2.6e11 µs，被判 implausible 后
    /// `estimate()` 直接为 `None` —— 这条测试当场变红。
    #[test]
    fn the_round_trip_is_measured_entirely_in_the_local_time_base() {
        const THREE_DAYS_US: i64 = 3 * 24 * 3_600 * 1_000_000;
        let mut f = ClockFilter::new();
        feed(&mut f, 8, -THREE_DAYS_US, 150, 150);
        let e = f.estimate().expect("对端时钟差多远都不影响本机量的 RTT");
        assert_eq!(e.min_rtt_us, 300, "RTT 是纯本机量");
        assert_eq!(e.unc_us, 150);
        assert_eq!(e.offset_us, Some(-THREE_DAYS_US), "θ 才是那个跨时基的量");
    }

    /// 对端是 P1 之前的版本（不发 `peer_t_us`）：网络段照给，θ **不编**。
    ///
    /// 规格草案里 `peer_t_us` 是 `#[serde(default)] u64`，缺席落成 0；那样
    /// θ 会被算成 (t1+t4)/2 ≈ 本机启动至今的微秒数——一个长得完全像正常读数的
    /// 垃圾。这里断言它是 `None`，并且**不确定度也跟着不报**：单独给一个
    /// 「不确定度」而没有它所修饰的量，读者只能误解。
    #[test]
    fn a_peer_without_peer_t_us_yields_a_network_segment_but_never_a_fabricated_offset() {
        let mut f = ClockFilter::new();
        for i in 0..8u64 {
            assert_eq!(
                f.note_pong(1_000_000 * (i + 1), 1_000_000 * (i + 1) + 300, None),
                PongOutcome::Ok
            );
        }
        let e = f.estimate().expect("RTT 不需要对端时戳");
        assert_eq!(e.min_rtt_us, 300);
        assert!(e.offset_us.is_none(), "对端没给时戳 ⇒ θ 必须是 None，绝不是 0");

        let mut p = send_pipeline(9_600);
        attach_peer_and_net(&mut p, None, Some(e));
        assert_eq!(p.net_ms, Some(0.15), "网络段照常成立");
        assert!(p.clock_offset_us.is_none());
        assert!(p.clock_unc_us.is_none(), "θ 不存在 ⇒ 它的不确定度也不该出现");
    }

    /// 对端 daemon 重启（时基从 0 重来）⇒ θ 整体平移 ⇒ 整窗作废。
    ///
    /// 不清窗的话，窗口里会同时躺着新旧两批 θ，而 min-RTT 挑中哪一批全看运气：
    /// 读数会在两个相差几百毫秒的值之间随机跳，且没有任何字段说得出为什么。
    #[test]
    fn a_clock_step_throws_the_whole_window_away() {
        let mut f = ClockFilter::new();
        feed(&mut f, 8, 0, 150, 150);
        assert_eq!(f.estimate().unwrap().offset_us, Some(0));

        // 阶跃 200 ms（> CLOCK_STEP_US = 50 ms）。
        let (t1, t4, t2) = round_trip(20_000_000, 200_000, 150, 150);
        assert_eq!(f.note_pong(t1, t4, t2), PongOutcome::Stepped);
        assert_eq!(f.len(), 1, "旧样本全部作废，新样本是新窗的第一个");
        assert!(f.estimate().is_none(), "阶跃之后退回「测量中」，不给旧 θ");

        feed(&mut f, 7, 200_000, 150, 150);
        assert_eq!(
            f.estimate().unwrap().offset_us,
            Some(200_000),
            "重新收敛到新的 θ"
        );
    }

    /// 对端回抄了一个我们不可能发过的 `t_us`：丢弃，且**不进窗**。
    ///
    /// 返回值是 `Implausible` 而不是静默 `return`：静默丢弃会让 confidence 永远
    /// 挂在「测量中」，却没有任何东西说得出为什么（调用方据此打一条日志）。
    #[test]
    fn a_pong_echoing_an_impossible_timestamp_is_refused_not_silently_swallowed() {
        let mut f = ClockFilter::new();
        feed(&mut f, 8, 0, 150, 150);
        let before = f.len();
        // 未来的 t1（负 RTT）
        assert_eq!(f.note_pong(9_000_000, 8_000_000, None), PongOutcome::Implausible);
        // 荒谬大的往返（> 2 s）
        assert_eq!(f.note_pong(0, 5_000_000, None), PongOutcome::Implausible);
        assert_eq!(f.len(), before, "坏样本一个都不许进窗");
        assert_eq!(f.estimate().unwrap().min_rtt_us, 300, "估计不受影响");
    }

    // ============================================ P0b：对端分项回传与总和合成

    /// 一条发送侧读数：`src_fifo` + 组帧节拍。`local_ms = samples/48 + 5`。
    fn send_pipeline(src_fifo_samples: u32) -> PipelineLatency {
        let tx = TxShared::new();
        tx.stages[0].store(Some(StageDepth::new(
            StageId::SrcFifo,
            src_fifo_samples,
            48_000,
            48_000,
            DropMode::Oldest,
        )));
        tx.stages[2].store(Some(StageDepth::send_pace()));
        build_pipeline_from(true, Some(&tx), None).expect("有分项")
    }

    fn clock(min_rtt_us: u64) -> ClockEstimate {
        ClockEstimate {
            min_rtt_us,
            last_rtt_us: min_rtt_us,
            offset_us: Some(0),
            unc_us: (min_rtt_us / 2) as u32,
        }
    }

    /// 一条「对端上报」：单级 `jitter_buf`，深度换算成 `ms`。
    fn peer_stage_ms(ms: f64) -> Vec<PipelineStage> {
        vec![stage(StageId::JitterBuf, (ms * 48.0) as u32, 48_000)]
    }

    fn cell_reporting(ms_values: &[f64]) -> PeerLatCell {
        let c = PeerLatCell::new();
        for (i, ms) in ms_values.iter().enumerate() {
            c.accept(i as u64 + 1, peer_stage_ms(*ms), Some(*ms), None);
        }
        c
    }

    /// **对端还没回传 ⇒ 总和 `None`，读数停在 `LocalOnly`。**
    ///
    /// 关键在最后两条断言：网络段**已经有了**，总和仍然必须是 `None`。
    /// 规格 §3.1 红线——实测 RTT 0.58 ms vs 感知 ~1000 ms，比值 1700 倍，
    /// 两者之间不存在任何单调关系。谁给 `sum_ms` 加一条「其它项缺就退回
    /// net_ms」的兜底，这条当场变红。
    #[test]
    fn without_a_peer_report_there_is_no_total_and_rtt_never_stands_in_for_it() {
        let mut p = send_pipeline(48_000); // 1000 ms + 5 ms 节拍
        attach_peer_and_net(&mut p, None, Some(clock(300)));
        assert_eq!(p.local_ms, Some(1005.0));
        assert!(p.peer_stages.is_empty());
        assert!(p.peer_local_ms.is_none());
        assert!(p.peer_age_s.is_none());
        assert_eq!(p.confidence, LatConfidence::LocalOnly);
        assert_eq!(p.net_ms, Some(0.15), "网络段自己是成立的");
        assert!(p.sum_ms.is_none(), "对端那一半缺席 ⇒ 总和必须 None，不许拿 RTT 顶");
    }

    /// 总和 = 本侧 Σ + **恰好一段**网络 + 对端 Σ。
    ///
    /// 最后一条断言把网络段单独拎出来验：`sum − local − peer` 必须精确等于
    /// **一个**单程，不是往返、也不是两段。规格 §3.1 要求 RTT 单独列为一段，
    /// 混进别的段里就再也验不出来了。
    #[test]
    fn the_total_is_both_sides_plus_exactly_one_network_segment() {
        let mut p = send_pipeline(9_600); // 200 + 5 = 205 ms
        let cell = cell_reporting(&[180.0]);
        attach_peer_and_net(&mut p, cell.snapshot(), Some(clock(580)));
        assert_eq!(p.peer_local_ms, Some(180.0));
        assert_eq!(p.net_ms, Some(0.29), "580 µs 往返 ⇒ 单程 0.29 ms");
        assert!(
            (p.sum_ms.unwrap() - 385.29).abs() < 1e-9,
            "205 + 0.29 + 180，实得 {:?}",
            p.sum_ms
        );
        let net_share = p.sum_ms.unwrap() - p.local_ms.unwrap() - p.peer_local_ms.unwrap();
        assert!(
            (net_share - 0.29).abs() < 1e-9,
            "网络只能占一段（单程），实得 {net_share}"
        );
        // 「带『≥』」这个结论的**前提**：设备固有延迟还读不到。前提与结论一起
        // 断言，否则前提变了而结论没跟着变，读数会静默地从下限变成谎话。
        assert!(
            p.dev.and_then(|d| d.ms()).is_none(),
            "设备固有延迟已经有值了 ⇒ 必须同时改 compose_sum_ms（把它加进总数）\
             与 attach_peer_and_net 的 confidence 梯子；见后者的文档"
        );
        assert_eq!(
            p.confidence,
            LatConfidence::LowerBound,
            "分项齐了，但设备固有延迟仍缺 ⇒ 带「≥」"
        );
        assert!(p.peer_age_s.unwrap() < 1.0);
    }

    /// **对端那一侧有一级读不到 ⇒ 总和 `None`，绝不按 0 计入。**
    ///
    /// 用 0 填补会让蓝牙耳机（真实 +150~250 ms）看起来和模拟输出一样好。
    /// 置信度也不能停在 `LocalOnly`：对端**回传了**，只是它此刻测不出来——
    /// 这与「对端没回传」对应的是两个不同的用户动作。
    #[test]
    fn an_unreadable_peer_stage_is_never_filled_with_zero() {
        let mut p = send_pipeline(9_600);
        let cell = PeerLatCell::new();
        let mut stages = peer_stage_ms(180.0);
        stages.push(stage(StageId::PlayRing, 48_000, 0)); // rate=0 = 读不到
        cell.accept(1, stages, None, None);
        attach_peer_and_net(&mut p, cell.snapshot(), Some(clock(580)));
        assert!(p.peer_local_ms.is_none(), "对端 Σ 里有洞 ⇒ None");
        assert!(!p.peer_stages.is_empty(), "分项本身照样展示给排障看");
        assert!(p.sum_ms.is_none(), "缺一项就整体 None");
        assert_ne!(p.sum_ms, Some(205.29), "更不许把缺的那项当 0 加进去");
        assert_eq!(p.confidence, LatConfidence::Unavailable);
    }

    /// **本侧有一级读不到 ⇒ 总和 `None`，绝不按 0 计入。**
    ///
    /// 这条与 `an_unreadable_peer_stage_is_never_filled_with_zero` 严格对称，
    /// 补的是同一条纪律在**本侧**的那一半。此前
    /// `compose_sum_ms(local_ms?, net_ms?, peer_local_ms?)` 三个入参里只有
    /// `peer_local_ms` 和 `net_ms` 有专门用例：把 `local_ms?` 改成
    /// `local_ms.unwrap_or(0.0)` 全绿——一条「本侧测不到」的流会报出
    /// 「总延迟 = 网络 + 对端」这个漂亮而错误的数，而且它比真值小得多，
    /// 正好是这套遥测要消灭的那种失败形态。
    ///
    /// 今天难触发（`rate` 全来自真实设备）不是不测的理由：纪律的强度由它**最弱
    /// 的那个入口**决定，而三个入口里有两个被测、一个没有，与它自称的
    /// 「三项缺一即 None」不是一回事。
    #[test]
    fn an_unreadable_local_stage_is_never_filled_with_zero() {
        let tx = TxShared::new();
        tx.stages[0].store(Some(StageDepth::new(
            StageId::SrcFifo,
            9_600,
            48_000,
            48_000,
            DropMode::Oldest,
        )));
        // 采集环读不到（`rate == 0`：设备速率还没协商出来 / 源刚被换掉）。
        tx.stages[1].store(Some(StageDepth::new(
            StageId::CapRing,
            4_800,
            96_000,
            0,
            DropMode::Newest,
        )));
        tx.stages[2].store(Some(StageDepth::send_pace()));

        let mut p = build_pipeline_from(true, Some(&tx), None).expect("有分项");
        assert!(p.local_ms.is_none(), "前提：本侧 Σ 里有洞");
        assert!(!p.stages.is_empty(), "分项本身照样展示给排障看");

        let cell = cell_reporting(&[180.0]);
        attach_peer_and_net(&mut p, cell.snapshot(), Some(clock(580)));
        assert_eq!(p.peer_local_ms, Some(180.0), "对端那一半自己是成立的");
        assert_eq!(p.net_ms, Some(0.29), "网络段也是");
        assert!(p.sum_ms.is_none(), "本侧缺一项 ⇒ 整体 None");
        assert_ne!(p.sum_ms, Some(180.29), "更不许把本侧当 0 加进去");
        assert_eq!(
            p.confidence,
            LatConfidence::Unavailable,
            "对端**回传了**、网络段**有了**，只是本机测不出来 —— 这与「对端没回传」\
             对应的是两个不同的用户动作，不能共用 LocalOnly"
        );
    }

    /// **三个入参一视同仁**：任意一个缺席都必须让总和 `None`。
    ///
    /// 上面三条测试各自走一条真路径，这一条是它们的判据表：把
    /// 「绝不用 0 填补」写成一个对称的、一眼可查的断言，谁给任何一个入参加上
    /// `unwrap_or(0.0)`，这里当场红。
    #[test]
    fn every_one_of_the_three_terms_can_veto_the_total() {
        let (l, n, pe) = (Some(205.0), Some(0.29), Some(180.0));
        assert!(
            (compose_sum_ms(l, n, pe).unwrap() - 385.29).abs() < 1e-9,
            "三项齐了才有总和"
        );
        for (name, got, if_zeroed) in [
            ("local_ms", compose_sum_ms(None, n, pe), 180.29),
            ("net_ms", compose_sum_ms(l, None, pe), 385.0),
            ("peer_local_ms", compose_sum_ms(l, n, None), 205.29),
        ] {
            assert_eq!(
                got, None,
                "{name} 缺席 ⇒ 总和必须 None；按 0 填补会给出 {if_zeroed} ms —— \
                 一个比真值小得多、看起来却完全正常的数字"
            );
        }
    }

    /// min-RTT 窗口还没收敛（约 8 s）⇒ `Converging`，UI 显示「测量中」。
    #[test]
    fn a_peer_report_without_a_converged_network_segment_reads_as_measuring() {
        let mut p = send_pipeline(9_600);
        let cell = cell_reporting(&[180.0]);
        attach_peer_and_net(&mut p, cell.snapshot(), None);
        assert_eq!(p.peer_local_ms, Some(180.0), "对端那一半照样展示");
        assert!(p.net_ms.is_none());
        assert!(p.sum_ms.is_none());
        assert_eq!(p.confidence, LatConfidence::Converging);
    }

    /// **窗口取中位数，不是求和。**
    ///
    /// 窗口里的 5 条是**同一条流**在 5 个时刻的读数。把它们加起来就是 R8 那个
    /// N 倍错误的时间轴版本：一条稳定在 100 ms 的对端会被报成 500 ms，而且
    /// 数字越稳定错得越离谱。
    #[test]
    fn the_peer_window_takes_the_median_never_the_sum() {
        let five = cell_reporting(&[100.0; 5]);
        assert_eq!(five.snapshot().unwrap().local_ms, Some(100.0));
        // 窗口只有 3 条时同样是「一份的量」。
        let three = cell_reporting(&[100.0; 3]);
        assert_eq!(three.snapshot().unwrap().local_ms, Some(100.0));
    }

    /// 瞬态由 5 点中位数吸收（规格 §3.4：对端分项采样非同时，≤1 s 前）。
    ///
    /// 中间那一拍飙到 900 ms（对端刚好在采样瞬间被调度打断），总和不该跟着跳。
    #[test]
    fn a_single_spike_is_absorbed_by_the_five_point_median() {
        let c = cell_reporting(&[100.0, 100.0, 900.0, 100.0, 100.0]);
        assert_eq!(c.snapshot().unwrap().local_ms, Some(100.0));
        // 但**持续**升高会被如实报出来——中位数是滤瞬态，不是掩盖趋势。
        let rising = cell_reporting(&[100.0, 300.0, 500.0, 700.0, 900.0]);
        assert_eq!(rising.snapshot().unwrap().local_ms, Some(500.0));
    }

    /// 最新一拍测不到 ⇒ 整体测不到，**不拿旧的好数盖住**。
    #[test]
    fn a_hole_in_the_newest_peer_report_wins_over_the_older_good_ones() {
        let c = cell_reporting(&[100.0; 4]);
        let mut holed = peer_stage_ms(100.0);
        holed.push(stage(StageId::PlayRing, 4_800, 0));
        c.accept(99, holed, None, None);
        assert!(
            c.snapshot().unwrap().local_ms.is_none(),
            "对端此刻报不出深度，本机就不该报总和"
        );
    }

    /// 对端不再上报之后，旧读数**停止**充当证据。
    ///
    /// 两级门限：`peer_age_s > 3` 只是让 UI 标「陈旧」；超过
    /// `PEER_REPORT_MAX_AGE` 就整体退回 `LocalOnly`。否则一条十秒前死掉的
    /// 对端会永远贡献一个漂亮的总和。
    #[test]
    fn a_peer_report_that_stopped_arriving_stops_being_evidence() {
        let now = Instant::now();

        let fresh = PeerLatCell::new();
        fresh.accept_at(now - Duration::from_secs(4), 1, peer_stage_ms(180.0), None, None);
        let s = fresh.snapshot().expect("4 秒前的读数仍可用");
        assert!(s.age_s > 3.0, "但要标成陈旧：age_s={}", s.age_s);

        let dead = PeerLatCell::new();
        dead.accept_at(now - Duration::from_secs(20), 1, peer_stage_ms(180.0), None, None);
        assert!(dead.snapshot().is_none(), "20 秒前的读数不再是关于「现在」的证据");

        let mut p = send_pipeline(9_600);
        attach_peer_and_net(&mut p, dead.snapshot(), Some(clock(580)));
        assert_eq!(p.confidence, LatConfidence::LocalOnly);
        assert!(p.sum_ms.is_none());
    }

    /// 乱序 / 重复的上报被丢掉（`seq_us` 与窗口里的同属对端时基，可比）。
    #[test]
    fn an_out_of_order_peer_report_is_dropped() {
        let c = PeerLatCell::new();
        c.accept(100, peer_stage_ms(100.0), None, None);
        c.accept(50, peer_stage_ms(900.0), None, None); // 迟到的旧报文
        c.accept(100, peer_stage_ms(900.0), None, None); // 重复
        assert_eq!(c.snapshot().unwrap().local_ms, Some(100.0));
    }

    /// **规格 §7.2 R8：扇出时 `src_fifo` 深度是共享的，分项只能按流展示。**
    ///
    /// 走的是真接线：一个源的 `SourceDepths` 被 `publish_send_stages` 广播到
    /// N 条流各自的 `TxShared`（`engine.rs` 的 tx_loop 就是这么干的）。物理队列
    /// 只有一份，所以两条流报同一个数是**正确的**；错的是把它们加起来。
    ///
    /// 最后一条断言是这条约束的真正执行点：合成入口 `compose_sum_ms` 只收标量，
    /// 一条流一次。谁要把它改成收一串会话再 `sum()`，这里立刻是 2010 而不是
    /// 1005 —— 一倍的假延迟，而且流越多越像真的。
    #[test]
    fn fan_out_streams_carry_the_shared_queue_once_each_and_are_never_summed() {
        // 一个源，1 秒的发送 FIFO 灌满。
        let depths: audiohub_core::latency::SourceDepths = [
            Some(StageDepth::new(
                StageId::SrcFifo,
                48_000,
                48_000,
                48_000,
                DropMode::Oldest,
            )),
            None,
        ];
        let a = TxShared::new();
        let b = TxShared::new();
        engine::publish_send_stages(&a.stages, &depths);
        engine::publish_send_stages(&b.stages, &depths);

        let pa = build_pipeline_from(true, Some(&a), None).expect("流 A 有分项");
        let pb = build_pipeline_from(true, Some(&b), None).expect("流 B 有分项");
        assert_eq!(pa.local_ms, Some(1005.0), "1000 ms 队列 + 5 ms 节拍");
        assert_eq!(pb.local_ms, pa.local_ms, "同一个队列，两条流读到同一个数");

        // 每条流各自合成，各自拿到 1005 + 网络 + 对端。
        let peer = Some(20.0);
        let net = Some(0.29);
        for sum in [
            compose_sum_ms(pa.local_ms, net, peer),
            compose_sum_ms(pb.local_ms, net, peer),
        ] {
            assert!((sum.unwrap() - 1025.29).abs() < 1e-9, "每条流各自 1005+0.29+20，实得 {sum:?}");
        }
        // 跨流相加会得到这个数。它不该出现在任何一条流的读数里。
        let n_fold = pa.local_ms.unwrap() + pb.local_ms.unwrap();
        assert_eq!(n_fold, 2010.0);
        for p in [&pa, &pb] {
            assert_ne!(p.local_ms, Some(n_fold), "分项不可跨流求和");
        }
    }

    // ------------------------------------------- R8：装配层（不是合成函数）

    /// 一条只有 `src_fifo` 的发送流，深度由调用方给。
    fn tx_with_fifo(samples: u32) -> TxShared {
        let tx = TxShared::new();
        tx.stages[0].store(Some(StageDepth::new(
            StageId::SrcFifo,
            samples,
            48_000,
            48_000,
            DropMode::Oldest,
        )));
        tx.stages[2].store(Some(StageDepth::send_pace()));
        tx
    }

    fn send_lat<'a>(tx: &'a TxShared, peer: Option<PeerLatSnapshot>, clock: Option<ClockEstimate>) -> StreamLat<'a> {
        StreamLat { is_send: true, tx: Some(tx), rx: None, peer, clock }
    }

    /// **装配层：N 条流进，N 条读数出，谁的深度都不许流进别人的读数。**
    ///
    /// 上面那条 `fan_out_streams_…` 走的是 `build_pipeline_from`——一次一条流，
    /// 手上根本没有第二条流可加，所以它证明不了这件事。真正把 N 条流的读数摆在
    /// 一起的是**装配层**，而它此前零覆盖：在 `build_session_infos` 的循环里写
    /// `entries.iter().filter_map(|e| …local_ms).sum()` 再把结果塞进每条会话，
    /// 253 条测试没有一条会红。`compose_sum_ms` 只收标量封住的是合成函数，
    /// 不是这里。
    ///
    /// 这条测试走的是那条**真路径**：`assemble_pipelines` 是生产代码里唯一算
    /// 逐级延迟的地方，`build_session_infos`（N 条）与 `build_session_info`
    /// （1 条，`conn.rs` 的 `session.opened` 事件）都只经它。
    ///
    /// 三条流深度**各不相同**是刻意的：全用同一个深度时，「每条报自己的」与
    /// 「每条报平均值」给出同一个数，断言就分辨不出来了。
    #[test]
    fn the_assembly_layer_gives_each_stream_its_own_depth_never_the_fleet_sum() {
        // 扇出：a、b 共用一个源（物理队列只有一份 ⇒ 报同一个数是**正确的**）。
        let shared: audiohub_core::latency::SourceDepths = [
            Some(StageDepth::new(StageId::SrcFifo, 48_000, 48_000, 48_000, DropMode::Oldest)),
            None,
        ];
        let a = TxShared::new();
        let b = TxShared::new();
        engine::publish_send_stages(&a.stages, &shared);
        engine::publish_send_stages(&b.stages, &shared);
        a.stages[2].store(Some(StageDepth::send_pace()));
        b.stages[2].store(Some(StageDepth::send_pace()));
        // 第三条流是**另一个**源，深度差一个数量级。
        let c = tx_with_fifo(4_800);

        let play_ring = StageSlot::new();
        let play_drift = Mutex::new(DriftTracker::new());
        let out = assemble_pipelines(
            &play_ring,
            &play_drift,
            vec![send_lat(&a, None, None), send_lat(&b, None, None), send_lat(&c, None, None)],
        );

        assert_eq!(out.len(), 3, "N 条流进，必须 N 条读数出，一一对应");
        let ms: Vec<Option<f64>> = out.iter().map(|p| p.as_ref().and_then(|p| p.local_ms)).collect();
        assert_eq!(
            ms,
            vec![Some(1005.0), Some(1005.0), Some(105.0)],
            "每条流只报**自己**那一份：a/b 共用队列所以相同（R7/R8 的物理事实），\
             c 是另一个源所以不同"
        );

        // 跨流求和会得到这些数，它们不该出现在任何一条流的读数里。
        let fleet_sum = 1005.0 + 1005.0 + 105.0; // 2115
        let fleet_avg = fleet_sum / 3.0; // 705
        for (i, m) in ms.iter().enumerate() {
            assert_ne!(*m, Some(fleet_sum), "第 {i} 条流报出了全站总和 —— 三倍假延迟");
            assert_ne!(*m, Some(fleet_avg), "第 {i} 条流报出了全站均值 —— 同一类错误的平均版");
        }
    }

    /// 对端分项也**按流落位**：一条流的对端读数不许渗进邻居的总和。
    ///
    /// 与上一条同一层、同一条路径，钉的是另一半——`attach_peer_and_net` 也在
    /// 装配层里，所以「谁的对端」同样是这一层的责任。
    #[test]
    fn a_peer_report_on_one_stream_does_not_leak_into_its_neighbours() {
        let a = tx_with_fifo(9_600); // 200 + 5 = 205 ms
        let b = tx_with_fifo(9_600);
        let cell = cell_reporting(&[180.0]);
        let play_ring = StageSlot::new();
        let play_drift = Mutex::new(DriftTracker::new());

        let out = assemble_pipelines(
            &play_ring,
            &play_drift,
            vec![
                send_lat(&a, cell.snapshot(), Some(clock(580))), // 只有 a 收到了对端上报
                send_lat(&b, None, Some(clock(580))),
            ],
        );
        let (pa, pb) = (out[0].as_ref().unwrap(), out[1].as_ref().unwrap());
        assert!((pa.sum_ms.unwrap() - 385.29).abs() < 1e-9, "a：205 + 0.29 + 180");
        assert_eq!(pa.confidence, LatConfidence::LowerBound);
        assert!(pb.sum_ms.is_none(), "b 没有对端上报 ⇒ 总和 None，不许借用邻居的");
        assert!(pb.peer_local_ms.is_none());
        assert_eq!(pb.confidence, LatConfidence::LocalOnly);
        assert_eq!(pb.local_ms, Some(205.0), "b 自己那一半照样成立");
    }

    /// 站点级播放环（规格 §7.2 R7）：多条流报**同一个**读数是物理事实，
    /// 而把它按流数乘出去是那个 N 倍错误的另一种写法。
    ///
    /// 走装配层，因为「同一个 `StageSlot` 被 N 条流读」这件事只在这一层发生。
    #[test]
    fn the_site_play_ring_is_reported_once_per_stream_never_multiplied() {
        let (mut tx, mut sink) = audio::AudioTx::detached_for_test(48_000);
        let play_ring = StageSlot::new();
        let play_drift = Mutex::new(DriftTracker::new());
        // 环里压 4800 个样本 = 100 ms（推之前发布，见 `ring_depth_before_push`）。
        tx.push(&vec![0.25f32; 4_800]);
        engine::publish_play_ring(&play_ring, &tx);
        sink.drain(0);

        let mk = || {
            RxStream::new(
                1,
                &[0u8; 32],
                &[0u8; 12],
                None,
                true,  // is_spk：真的往本机默认输出送
                false, // monitor
                None,
                None,
                "127.0.0.1:1".parse().unwrap(),
            )
        };
        let (r1, r2) = (mk(), mk());
        let out = assemble_pipelines(
            &play_ring,
            &play_drift,
            vec![
                StreamLat { is_send: false, tx: None, rx: Some(&r1), peer: None, clock: None },
                StreamLat { is_send: false, tx: None, rx: Some(&r2), peer: None, clock: None },
            ],
        );
        let ring_ms = |p: &Option<PipelineLatency>| {
            p.as_ref()
                .unwrap()
                .stages
                .iter()
                .find(|s| s.id == "play_ring")
                .expect("送本机输出的流必须有 play_ring 这一级")
                .ms
        };
        assert_eq!(ring_ms(&out[0]), Some(100.0));
        assert_eq!(
            ring_ms(&out[1]),
            ring_ms(&out[0]),
            "同一个环，两条流读到同一个数 —— 这是对的"
        );
        for p in &out {
            assert_eq!(
                p.as_ref().unwrap().local_ms,
                Some(100.0),
                "……但每条流的 Σ 里它只能算**一份**（JB/PostMix 此刻都是 0）"
            );
        }
    }

    /// 对端上报落在**它指名的那条流**的格子里，流与流之间互不串扰。
    ///
    /// 格子挂在 `SessionEntry` 上（每流一个 `Arc<PeerLatCell>`），而不是挂在
    /// `ConnShared` 上的一张 `HashMap<stream_id, _>` —— 后者既能被 `values()`
    /// 求和（R8 的坑），也更容易在路由上串号。这条测试钉的是「互不串扰」这个
    /// 可观测后果。
    #[test]
    fn peer_reports_land_in_the_stream_that_named_them() {
        let one = cell_reporting(&[100.0]);
        let two = cell_reporting(&[500.0]);
        assert_eq!(one.snapshot().unwrap().local_ms, Some(100.0));
        assert_eq!(two.snapshot().unwrap().local_ms, Some(500.0));
    }

    /// 线上 → IPC 的换算与 `StageDepth::ms()` 是同一条规则。
    ///
    /// 两份实现（core 那份走枚举，这份走裸 `rate`，因为对端可能带来本版本不
    /// 认识的级 id）必须给出同一个数，这条测试是把它们钉在一起的那颗钉子。
    #[test]
    fn the_wire_round_trip_preserves_the_reading_and_recomputes_ms_locally() {
        let original = stage(StageId::SrcFifo, 24_000, 48_000);
        let back = from_wire_stage(&to_wire_stage(&original));
        assert_eq!(back.ms, original.ms, "500 ms 原样回来");
        assert_eq!(back.ms, Some(500.0));
        assert_eq!(back.id, "src_fifo");
        assert_eq!(back.samples, 24_000);
        assert_eq!(back.rate, 48_000);
        assert_eq!(back.drop_mode, original.drop_mode);

        // 44.1k 设备：拿 48000 去除会得到 459 ms，−8.8% 的系统性低估。
        let dev = stage(StageId::PlayRing, 44_100, 44_100);
        assert_eq!(from_wire_stage(&to_wire_stage(&dev)).ms, Some(1000.0));

        // 本版本不认识的级：照样能换算、能展示。
        let mut unknown = to_wire_stage(&original);
        unknown.id = "some_future_stage".into();
        assert_eq!(from_wire_stage(&unknown).ms, Some(500.0));

        // `rate == 0` ⇒ 读不到 ⇒ `None`，**不是 0 ms**。
        let mut hole = to_wire_stage(&original);
        hole.rate = 0;
        assert!(
            from_wire_stage(&hole).ms.is_none(),
            "读不到的一级绝不能变成 0 ms —— 蓝牙耳机那 150~250 ms 就是这么消失的"
        );
    }

    /// **整条 P0b 接线**：本机读数 → `StageReport` → JSON → 对端格子 → 总和。
    ///
    /// 前面那些测试各自钉住一个环节，这一条钉的是**接线**——本仓库反复吃亏的
    /// 地方从来是接线而不是逻辑（`send_pace` 曾经在枚举里声明、在规格里编号、
    /// 全仓库零发布点）。少序列化一个字段、`to_wire_stage` 漏抄一项，单独看
    /// 每个环节都对，合起来对端拿到的就是个空壳。
    ///
    /// 顺带交叉校验两端的求和口径：`accept` 返回 `None` 就说明**发送方算的
    /// Σ 与接收方重算的 Σ 一致**。这两份实现（一份走 `PipelineStage`，一份走
    /// 线上的 `StageReading`）必须永远给出同一个数。
    #[test]
    fn a_peer_report_survives_the_wire_and_completes_the_total() {
        // 对端（发送侧）：200 ms 队列 + 5 ms 组帧节拍。
        let sender = send_pipeline(9_600);
        assert_eq!(sender.local_ms, Some(205.0));
        let msg = SessionMsg::StageReport {
            stream_id: 7,
            stages: sender.stages.iter().map(to_wire_stage).collect(),
            local_ms: sender.local_ms,
            dev: sender.dev,
            seq_us: 1,
        };
        let back: SessionMsg =
            serde_json::from_str(&serde_json::to_string(&msg).unwrap()).expect("报文往返");
        let SessionMsg::StageReport { stages, local_ms, dev, seq_us, .. } = back else {
            panic!("变体在往返中变了")
        };

        let cell = PeerLatCell::new();
        let why = cell.accept(seq_us, stages.iter().map(from_wire_stage).collect(), local_ms, dev);
        assert!(
            why.is_none(),
            "同一份分项走了一圈回来，两端求和口径必须一致，实得分歧：{why:?}"
        );

        // 本机（接收侧）：300 ms + 5 ms。
        let mut local = send_pipeline(14_400);
        attach_peer_and_net(&mut local, cell.snapshot(), Some(clock(580)));
        assert_eq!(local.peer_local_ms, Some(205.0), "对端那一半原样过来了");
        let ids: Vec<&str> = local.peer_stages.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["src_fifo", "send_pace"], "对端分项逐级可见，不是一个总数");
        assert_eq!(local.peer_stages[0].ms, Some(200.0));
        assert!(
            (local.sum_ms.unwrap() - 510.29).abs() < 1e-9,
            "305 + 0.29 + 205，实得 {:?}",
            local.sum_ms
        );
        assert_eq!(local.confidence, LatConfidence::LowerBound);
    }

    /// 求和口径分歧会被指出来，而且只说一次。
    ///
    /// 权威值始终是**本机**用 `sum_stage_ms` 重算的那个：三条规则（并行尾级取
    /// max、`rate==0` 判缺项、空列表判 None）必须在本机执行，否则一条
    /// `{rate:0, local_ms:0.0}` 的报文就能把「测不到」变成「没有延迟」。
    #[test]
    fn a_peer_that_sums_differently_is_reported_once_and_never_believed() {
        let c = PeerLatCell::new();
        // 对端自称 0 ms，实际分项摆着 100 ms。
        let why = c.accept(1, peer_stage_ms(100.0), Some(0.0), None);
        assert!(why.is_some(), "口径对不上必须说出来");
        assert!(c.accept(2, peer_stage_ms(100.0), Some(0.0), None).is_none(), "只说一次");
        assert_eq!(
            c.snapshot().unwrap().local_ms,
            Some(100.0),
            "以本机重算值为准，绝不采信对端自报的 0"
        );
    }
}

/// 规格 §6.3 的核心验收：**「重启后那 1 秒消失了，怎么证明遥测确实能报出它」**。
///
/// 这一整模块的存在理由是一句话：**不能等故障自己复现，必须主动制造。**
/// 用户原本遇到的 mac→win 约 1 秒延迟，现场已在 2026-08-01 的重签事故中丢失
/// （会话归零、缓冲排空）。所以这里按 §6.3 把那个病理**造出来**，再断言新遥测
/// 报出对应的量级。
///
/// ## 纪律：只灌**真的**缓冲，不写 `StageDepth` 字面量
///
/// 本模块里每一条读数都必须由**生产代码**从一个**真实的数据结构**里读出来：
/// 真的 `HeapRb` 播放环（`AudioTx::detached_for_test`，与 `on_device` 逐字同构
/// 的一行）、真的 `JitterBuffer`、真的 `PostMix`、真的 `RxStream::new`，再经
/// `publish_play_ring` / `attach_output_tails` / `sum_stage_ms` 这条生产链路
/// 汇总。手写一个 `samples: 48_000` 塞进 `to_ipc_stage` 再断言它等于 1000 ms，
/// 证明的只是除法还在工作 —— 生产代码把 `queued()` 接成 `capacity()`、把
/// `dev_rate()` 换成 48000、或者干脆忘了发布某一级，那种测试一条都不会红。
///
/// ## 相位约定（别把它当成误差）
///
/// 六级读数一律在**推之前**取（见 `engine::ring_depth_before_push`）：读到的是
/// 「排在这一帧前面的样本数」，也就是这一帧真正要等的时间。所以一个被灌满的
/// 1.000 秒环在稳态每 tick 报的是 `cap − 480` = **990 ms** 而不是 1000 ms
/// —— 那 480 是本 tick 刚被声卡取走、还没被补上的那一帧。990 与 1000 的差别
/// 在这里无关紧要（要区分的是 990 ms 与 30 ms），但断言必须写成它真实的样子，
/// 不能为了好看去凑整。
#[cfg(test)]
mod fault_injection {
    use super::*;
    use audiohub_core::audio::{AudioTx, PlayRingSink};

    /// 一个 10 ms 帧 @48k。
    const F: usize = 480;

    /// 造一条**真的**接收流。`RxStream::new` 是生产构造器，里面的
    /// `JitterBuffer` / `PostMix` / `ConcealWindow` / 两个尾级槽全是真的。
    fn spk_stream() -> RxStream {
        RxStream::new(
            1,
            &[0u8; 32],
            &[0u8; 12],
            None,
            true,  // is_spk：这条流真的往本机默认输出送音频
            false, // monitor
            None,
            None,
            "127.0.0.1:1".parse().unwrap(),
        )
    }

    /// 给接收流一个**已知的、小的**上游基线：JB 里连续 3 帧（30 ms）、
    /// PostMix 里 960 个样本（20 ms）。合计 50 ms —— 与后面要注入的秒级量
    /// 差一个数量级，好让「总数动没动」一眼可判。
    fn seed_upstream_50ms(rx: &RxStream) {
        {
            let mut st = lk(&rx.jbs);
            for seq in 0..4 {
                st.jb.push(seq, vec![0.1; F]);
            }
            assert!(st.jb.pop().is_some(), "起播，放掉一帧 ⇒ 连续 3 帧 = 30 ms");
        }
        let mut out = [0.0f32; F];
        lk(&rx.post).advance(Some(vec![0.5; 1_440]), &mut out); // 1440 − 480 = 960 = 20 ms
    }

    /// 走完整条生产汇总链路，返回这条会话此刻上报的 `PipelineLatency`。
    fn report(play_ring: &StageSlot, play_drift: &Mutex<DriftTracker>, rx: &RxStream) -> PipelineLatency {
        let mut p = build_pipeline_from(false, None, Some(rx)).expect("这条流有可读的级");
        attach_output_tails(play_ring, play_drift, rx, &mut p);
        p
    }

    fn stage_of<'a>(p: &'a PipelineLatency, id: &str) -> &'a PipelineStage {
        p.stages
            .iter()
            .find(|s| s.id == id)
            .unwrap_or_else(|| panic!("分项里没有 {id}，实际有 {:?}", p.stages.iter().map(|s| &s.id).collect::<Vec<_>>()))
    }

    /// 一个 tick 的**真实**播放路径：声卡取走 480（上一个 10 ms 里发生的），
    /// mixer 在 push 之前发布深度，然后 push 480。顺序与 `mixer_loop` 逐字一致。
    fn play_tick(sink: &mut PlayRingSink, tx: &mut AudioTx, slot: &StageSlot, drain: usize) {
        sink.drain(drain);
        engine::publish_play_ring(slot, tx);
        tx.push(&vec![0.25f32; F]);
    }

    // ================================================================ 注入 A
    //
    // 规格 §6.3 注入 A：**一次性灌满播放环**。
    //
    // 手段：让 mixer 继续按 10 ms 节拍推，而声卡侧停摆 1.2 s（现实里就是一次
    // 调度抖动 / 设备重配 / 睡眠恢复）。恢复后收支重新平衡，但环里从此恒定
    // 压着一整秒 —— 全链路六级**无一做深度伺服**（规格 §0.7），没有任何机制
    // 把它收敛回去。
    //
    // 期望遥测：`play_ring.samples ≈ capacity`、`drop_mode = Newest`、
    // `dropped` **跳增一次后冻结**、`drift_sps ≈ 0`、总数 ≈ 1000 ms。

    #[test]
    fn injection_a_a_once_flooded_play_ring_puts_a_full_second_into_the_total() {
        let (mut tx, mut sink) = AudioTx::detached_for_test(48_000);
        let slot = StageSlot::new();
        let drift = Mutex::new(DriftTracker::new());
        let rx = spk_stream();
        seed_upstream_50ms(&rx);

        // ---- 1) 健康稳态：推多少、取多少 ----
        for _ in 0..50 {
            play_tick(&mut sink, &mut tx, &slot, F);
        }
        let before = report(&slot, &drift, &rx);
        let pr = stage_of(&before, "play_ring");
        assert!(
            pr.ms.unwrap() <= 10.0,
            "健康稳态下播放环应该几乎是空的, got {:?} ms",
            pr.ms
        );
        assert_eq!(pr.dropped, Some(0), "还没丢过");
        assert_eq!(
            before.local_ms,
            Some(50.0),
            "基线 = jitter_buf 30 + post_mix 20 + 空播放环"
        );

        // ---- 2) 注入：声卡停摆 1.2 s（120 个 tick），mixer 照推不误 ----
        for _ in 0..120 {
            play_tick(&mut sink, &mut tx, &slot, 0);
        }
        let dropped_after_stall = stage_of(&report(&slot, &drift, &rx), "play_ring")
            .dropped
            .expect("播放环的丢弃是可观测的");
        assert!(
            dropped_after_stall > 0,
            "1.2 s 的停顿必须把环灌爆并**数出来** —— 这里此前是全链路唯一完全无遥测的丢弃点"
        );

        // ---- 3) 恢复：收支重新平衡，每 tick 取 480 推 480 ----
        // 这一段同时喂真实的 `DriftTracker`（1 s 一个点，与 ticker 同频）。
        let mut dropped_seen = Vec::new();
        for sec in 0..40 {
            for _ in 0..100 {
                play_tick(&mut sink, &mut tx, &slot, F);
            }
            let d = slot.load().expect("环还在");
            lk(&drift).push(sec as f32, d.id, d.samples);
            dropped_seen.push(d.dropped.expect("可观测"));
        }

        // ---- 4) 断言：遥测把这一秒报了出来 ----
        let after = report(&slot, &drift, &rx);
        let pr = stage_of(&after, "play_ring");

        assert!(
            (985.0..=1000.0).contains(&pr.ms.unwrap()),
            "被灌满的 1.000 秒环必须报出接近 1000 ms（相位见模块头：推前读 ⇒ cap−480 = 990）, got {:?}",
            pr.ms
        );
        assert!(pr.saturated, "深度贴着容量");
        assert_eq!(pr.drop_mode, DropMode::Newest, "push_slice 短写：丢最新 ⇒ 听感是迟到 + 断续");
        assert_eq!(
            pr.drift_sps.map(|v| v.abs() < 1e-9),
            Some(true),
            "收支已经重新平衡：斜率必须是**测到的 0**，而不是 None, got {:?}",
            pr.drift_sps
        );
        assert_eq!(
            dropped_seen.first(),
            dropped_seen.last(),
            "**丢弃跳增一次后冻结** —— 这是「曾被一次卡顿灌满」区别于「稳态速率失配」的唯一判据（规格 §3.3）"
        );

        // 而这才是本次验收真正要证的那一句：**总数动了**。
        let total = after.local_ms.expect("各级齐全");
        assert!(
            (1035.0..=1050.0).contains(&total),
            "总延迟必须把这一秒算进去（30 + 20 + ~990）, got {total}"
        );
        assert!(
            total - before.local_ms.unwrap() > 900.0,
            "同一条会话，注入前后总数至少要相差 900 ms —— 这就是用户听到的那一秒"
        );
    }

    // ================================================================ 注入 B
    //
    // 规格 §6.3 注入 B：**稳态速率失配**（生产快于消费）。见 engine.rs 的
    // `injection_b_a_steady_rate_mismatch_climbs_then_keeps_dropping`：那里有
    // 真的 `SysAudioFrames`（真 FIFO + 真重采样器），所以注入放在它旁边。
    // 这里只钉死**两种病理在读数上如何区分**，因为它们的标量深度完全简并。

    /// 饱和的「丢最旧」与饱和的「丢最新」**深度一模一样**，`drop_mode` 是唯一的
    /// 区别；而「灌满一次」与「持续失配」的 `drop_mode` 可以相同，靠 `dropped`
    /// 是否还在增长区分。两个维度都必须在 IPC 报文里，缺一个就只能说「有一秒卡
    /// 在某处」，说不出它是怎么卡的。
    #[test]
    fn the_two_pathologies_are_told_apart_by_drop_mode_and_dropped_not_by_depth() {
        // 真的源侧 FIFO（丢最旧）：engine.rs 的 `SysAudioFrames` 灌满后 47_520。
        // 真的播放环（丢最新）：灌满后同样 47_520。这里直接用两条真环对照。
        let (mut oldest_like, _s1) = AudioTx::detached_for_test(48_000);
        let (mut newest_like, _s2) = AudioTx::detached_for_test(48_000);
        oldest_like.push(&vec![0.5; 60_000]);
        newest_like.push(&vec![0.5; 60_000]);
        let a = engine::ring_depth_before_push(StageId::SrcFifo, &oldest_like);
        let b = engine::ring_depth_before_push(StageId::PlayRing, &newest_like);
        assert_eq!(a.ms(), b.ms(), "深度读数完全简并 —— 这正是规格 §0.2 的论点");
        assert_eq!(a.saturated(), b.saturated());

        // 「一次灌满后冻结」 vs 「持续失配」：**两条真环**，两段真的 dropped 序列。
        // 深度读数在两种病理下同样简并（都贴着容量），只有 `dropped` 的斜率不同。
        let sample_dropped = |drain: usize| -> Vec<u64> {
            let (mut tx, mut sink) = AudioTx::detached_for_test(48_000);
            let slot = StageSlot::new();
            tx.push(&vec![0.5; 60_000]); // 先一次性灌满
            let mut seen = Vec::new();
            for i in 0..40 {
                play_tick(&mut sink, &mut tx, &slot, drain);
                if i >= 10 {
                    seen.push(slot.load().unwrap().dropped.unwrap());
                }
            }
            seen
        };
        // 收支平衡（取 480 推 480）：灌满那一次之后**再也不丢**。
        let frozen = sample_dropped(F);
        // 消费慢 2%（取 470 推 480）：稳态**持续丢**。
        let growing = sample_dropped(470);
        assert_eq!(
            frozen.first(),
            frozen.last(),
            "收支平衡后丢弃冻结 ⇒ 曾被一次卡顿灌满、之后永远迟到（无深度伺服）"
        );
        assert!(
            growing.windows(2).all(|w| w[1] > w[0]),
            "持续增长 ⇒ 稳态产销速率失配，修法与前者完全不同, got {growing:?}"
        );
        // 而两者此刻的**深度**（也就是 UI 上那个延迟数字）分毫不差。
        assert_eq!(
            AudioTx::detached_for_test(48_000).0.capacity(),
            48_000,
            "两条序列跑的是同一种 1.000 秒环"
        );
    }

    // ================================================================ 注入 C
    //
    // 规格 §6.3 注入 C（**最重要的一条**）：HAL 环里恒定驻留 400 ms
    // —— 不饱和、不丢弃、进出速率严格相等，但每个样本恒定迟到 400 ms。
    //
    // 父任务原有的证据（90.25 s 内驱动写入 = tx_loop 读出，均 9025 帧）
    // **结构上无法证伪**这个形态。C 通过，才说明新遥测真的比旧证据强。
    //
    // 真实的 HAL 环 + `HalSpeakerSource::depths()` 那一半在 halbridge.rs
    // （FakeDriverRing 在那里），见
    // `injection_c_a_ring_that_never_drops_still_shows_up_as_half_a_second`。
    // 这里接的是它的**下半程**：那一级如何穿过 `publish_send_stages` 进入
    // 发送侧的 `local_ms`。

    #[test]
    fn injection_c_a_constant_hal_backlog_reaches_the_send_total() {
        use audiohub_core::latency::SourceDepths;
        let tx = TxShared::new();

        // `HalSpeakerSource::depths()` 在 400 ms 驻留时返回的东西（那一步由
        // halbridge.rs 的真环测试钉死）。这里验的是**接线**：它经
        // `publish_send_stages` 进槽、经 `build_pipeline_from` 汇总。
        let hal = StageDepth {
            id: StageId::HalSpk,
            samples: 19_200, // 400 ms @48k
            capacity: 24_000,
            rate: 48_000,
            dropped: None, // 环满时写不进去的是驱动，计数在它那一侧
            drop_mode: DropMode::Newest,
        };
        let depths: SourceDepths = [Some(hal), None];
        engine::publish_send_stages(&tx.stages, &depths);

        let p = build_pipeline_from(true, Some(&tx), None).expect("有分项");
        let spk = stage_of(&p, "hal_spk");
        assert_eq!(spk.ms, Some(400.0));
        assert!(
            !spk.saturated,
            "80% 不算饱和 —— **靠「是否饱和」判断这一级健不健康恰好会漏掉它**"
        );
        assert_eq!(spk.dropped, None, "观测不到就报 None，不报 0（0 是「很健康」的假保证）");
        assert_eq!(
            p.local_ms,
            Some(405.0),
            "400（HAL 环）+ 5（组帧节拍）—— 这一级不进总数，用户的 400 ms 就没人报"
        );

        // 对照：环是空的时候这一级**仍然存在**，只是 0 ms。空 ≠ 不存在。
        let empty = StageDepth { samples: 0, ..hal };
        engine::publish_send_stages(&tx.stages, &[Some(empty), None]);
        let p = build_pipeline_from(true, Some(&tx), None).unwrap();
        assert_eq!(stage_of(&p, "hal_spk").ms, Some(0.0));
        assert_eq!(p.local_ms, Some(5.0), "只剩组帧节拍");
    }

    // ================================================================ 注入 D
    //
    // 规格 §6.3 注入 D：**未建模的缓冲级**。在 mixer 与 `AudioTx::push` 之间
    // 插一个 600 ms 的额外队列，**不给它注册任何 `PipelineStage`**。
    //
    // 规格明写这一条必须在 P0 阶段**故意失败**（证明逐级会计确有盲区，我们没有
    // 自欺），在 P1 阶段（`residual_ms = e2e_ms − Σ`）才成功。下面这条测试因此
    // 断言的是**失败本身**：总数纹丝不动，且没有任何字段说「我漏了 600 ms」。
    // 它同时是 P1 的守卫 —— P1 落地那天这条会红，提醒把它改成断言
    // `residual_ms ≈ 600`。

    #[test]
    fn injection_d_an_unregistered_600ms_buffer_is_invisible_at_p0() {
        let (mut tx, mut sink) = AudioTx::detached_for_test(48_000);
        let slot = StageSlot::new();
        let drift = Mutex::new(DriftTracker::new());
        let rx = spk_stream();
        seed_upstream_50ms(&rx);

        for _ in 0..20 {
            play_tick(&mut sink, &mut tx, &slot, F);
        }
        let baseline = report(&slot, &drift, &rx).local_ms.expect("有总数");

        // 未建模的一级：一个真的、装着 600 ms 音频的队列，横在 mixer 与 push
        // 之间。它有真实的物理效果（每个样本多等 600 ms），但没有 `StageSlot`。
        let mut hidden: VecDeque<f32> = VecDeque::new();
        for _ in 0..60 {
            hidden.extend(std::iter::repeat(0.25f32).take(F)); // 先攒够 600 ms
        }
        assert_eq!(hidden.len(), 28_800, "600 ms @48k");
        for _ in 0..30 {
            hidden.extend(std::iter::repeat(0.25f32).take(F));
            let out: Vec<f32> = hidden.drain(..F).collect();
            sink.drain(F);
            engine::publish_play_ring(&slot, &tx);
            tx.push(&out);
        }
        assert_eq!(hidden.len(), 28_800, "队列稳态驻留恒为 600 ms");

        let after = report(&slot, &drift, &rx);
        assert_eq!(
            after.local_ms,
            Some(baseline),
            "**P0 的已知盲区**：没注册的级对逐级会计完全不可见（规格 §6.3 注入 D 要求这里失败）"
        );
        assert!(
            after.residual_ms.is_none() && after.e2e_ms.is_none(),
            "P1 的残差探测器还不存在 —— 所以这 600 ms 现在既测不到、也**没有任何字段说它可能存在**"
        );
        assert!(
            !after.stages.iter().any(|s| s.id == "residual"),
            "更不许凭空造一个 residual 分项来假装覆盖了它"
        );
        // 这条测试将在 P1 落地当天变红。那是它的用途，不是它的缺陷。
    }

    // ======================================================= 注入线性度（§6.1）

    /// 规格 §6.1 第一条发布前断言：**注入线性度**。人为往某一级注入 K ms 的额外
    /// 驻留，上报的该级与**总数**必须各移动 K ± 3 ms。不满足 ⇒ 测量本身坏了。
    ///
    /// 注入点取播放环（容量 1.000 秒，能覆盖到本次要诊断的整个量级），K 一路取到
    /// 990 ms。用真环 + 真 `push`：注入的是**样本**，不是一个 ms 数字。
    #[test]
    fn injected_latency_is_reported_linearly_all_the_way_to_one_second() {
        for k_ms in [50u32, 100, 200, 500, 990] {
            let (mut tx, mut sink) = AudioTx::detached_for_test(48_000);
            let slot = StageSlot::new();
            let drift = Mutex::new(DriftTracker::new());
            let rx = spk_stream();
            seed_upstream_50ms(&rx);

            // 注入：一次性多推 K ms 的音频，之后收支平衡 —— 这就是「一次卡顿
            // 灌进去、再也不收敛」的最小复刻。
            let extra = (k_ms as usize) * 48; // 48 样本/ms @48k
            tx.push(&vec![0.25f32; extra]);
            for _ in 0..30 {
                play_tick(&mut sink, &mut tx, &slot, F);
            }

            let p = report(&slot, &drift, &rx);
            let got_stage = stage_of(&p, "play_ring").ms.expect("有读数");
            let got_total = p.local_ms.expect("有总数");
            // 相位：推前读 ⇒ 恒少一帧（10 ms）。这是约定不是误差，所以把它写进
            // 期望值，而不是靠放宽容差吞掉。
            let want_stage = k_ms as f64 - 10.0;
            assert!(
                (got_stage - want_stage).abs() <= 3.0,
                "注入 {k_ms} ms，该级应报 {want_stage} ± 3，got {got_stage}"
            );
            assert!(
                (got_total - (50.0 + want_stage)).abs() <= 3.0,
                "注入 {k_ms} ms，总数应报 {} ± 3，got {got_total}",
                50.0 + want_stage
            );
        }
    }

    // =============================================== 四个 1 秒 FIFO / 500 ms 环
    //
    // 任务点名要逐个灌满、确认遥测报出 ~1000 / ~500 ms **而不是沉默**。
    // 源侧三个 FIFO 与两个 HAL 环各自在自己的 crate 里灌（那里才有真的数据结构）：
    //   - `MicSource.fifo`      -> audiohub-net/src/media.rs
    //   - `SysAudioSource.fifo` -> audiohub-net/src/media.rs
    //   - `SysAudioFrames.fifo` -> audiohubd/src/engine.rs
    //   - HAL spk / mic ring    -> audiohubd/src/halbridge.rs
    // 下面这两条负责**接收侧的两条 1 秒环**，并且都走到 `local_ms`。

    /// 桥接虚拟声卡的播放环（每个桥一个 `AudioTx`，同样 1.000 秒）。
    /// 一条纯桥接流不碰站点播放环，所以这一级要是不报，它的 `local_ms` 就只有
    /// jitter_buf + post_mix —— 把 1000 ms 报成 50 ms。
    #[test]
    fn a_flooded_bridge_ring_reports_a_full_second_in_the_total() {
        let (mut bridge_tx, _sink) = AudioTx::detached_for_test(48_000);
        let rx = RxStream::new(
            2,
            &[0u8; 32],
            &[0u8; 12],
            None,
            false, // 纯桥接：不送本机扬声器
            false,
            Some("some-usb-dac".to_string()),
            None,
            "127.0.0.1:1".parse().unwrap(),
        );
        seed_upstream_50ms(&rx);
        let empty_site = StageSlot::new();
        let no_drift = Mutex::new(DriftTracker::new());

        // 灌爆这张桥的环，再按 mixer 的相位发布（推之前读）。
        bridge_tx.push(&vec![0.25f32; 60_000]);
        rx.bridge_ring
            .store(Some(engine::ring_depth_before_push(StageId::BridgeRing, &bridge_tx)));

        let p = report(&empty_site, &no_drift, &rx);
        let br = stage_of(&p, "bridge_ring");
        assert_eq!(br.ms, Some(1000.0), "满环 = 1.000 秒");
        assert_eq!(br.capacity, 48_000);
        assert_eq!(br.drop_mode, DropMode::Newest);
        assert!(br.dropped.unwrap() > 0, "短写的部分数得出来");
        assert_eq!(
            p.local_ms,
            Some(1_050.0),
            "30 + 20 + 1000；漏掉这一级会把它报成 50 ms —— 相差 21 倍"
        );
        assert!(
            !p.stages.iter().any(|s| s.id == "play_ring"),
            "纯桥接流不经过站点播放环，不该凭空多一级"
        );
    }

    // ================================================================ 注入 E
    //
    // **跨时钟速率失配** —— 治法 A 落地之后全链路上唯一还在无界积累的病灶。
    //
    // 与注入 B 的区别：B 那条是「产销速率失配」在**同一台机器内部**的形态，
    // 靠治法 A 收敛。E 是**真跨时钟**：写侧是 mac 的发送节拍经 JB 定拍之后的
    // 本地 tick（严格 48000 样本/本地秒），读侧是 Windows 声卡的**晶振**
    // （48000·(1+ε) 样本/物理秒）。两个独立振荡器，ε 不可能为零，而 `play_ring`
    // 是 **drop-newest** —— 饱和之后丢的是最新的音频，听感是「迟到 + 周期性
    // 断续」而不是「恒定迟到但连续」。
    //
    // 50 ppm ⇒ 180 ms/小时 ⇒ 1.000 秒的环约 5.4 小时灌满。
    //
    // 控制律与收敛性由 `audiohub-core` 的 `audio::rate_servo` 那一组测试钉死
    // （那里有真的两个时钟与二十分钟虚拟时间）。**这里钉的是另一件事**：
    // 病理与治法在**这条生产遥测链路**（`publish_play_ring` →
    // `attach_output_tails` → `sum_stage_ms`）上报出来的是什么。

    /// 一个 10 ms tick 的跨时钟播放：声卡按自己的晶振取，mixer 按本地时钟推。
    /// `ppm > 0` = 声卡快。走的是 `push_at` / `drain_at`，即生产路径 + 虚拟时间。
    fn drift_tick(
        sink: &mut PlayRingSink,
        tx: &mut AudioTx,
        slot: &StageSlot,
        t0: Instant,
        tick: u64,
        cb_period_ns: &mut f64,
        next_cb_ns: &mut f64,
    ) {
        let end_ns = (tick + 1) * 10_000_000;
        while *next_cb_ns <= end_ns as f64 {
            sink.drain_at(512, t0 + Duration::from_nanos(*next_cb_ns as u64), None);
            *next_cb_ns += *cb_period_ns;
        }
        let now = t0 + Duration::from_nanos(end_ns);
        engine::publish_play_ring(slot, tx);
        tx.push_at(&vec![0.25f32; F], now);
    }

    /// 跑 `secs` 秒虚拟时间，返回这条流每 30 秒上报一次的 `play_ring.ms`。
    fn drift_trace(ppm: f64, servo: bool, secs: u64) -> Vec<f64> {
        let (mut tx, mut sink) = if servo {
            AudioTx::detached_for_test_with_servo(48_000)
        } else {
            AudioTx::detached_for_test(48_000)
        };
        let slot = StageSlot::new();
        let drift = Mutex::new(DriftTracker::new());
        let rx = spk_stream();
        seed_upstream_50ms(&rx);
        let t0 = Instant::now();
        let mut cb = 512.0 / (48_000.0 * (1.0 + ppm * 1e-6)) * 1e9;
        let mut next = 0.0f64;
        let mut out = Vec::new();
        for tick in 0..secs * 100 {
            drift_tick(&mut sink, &mut tx, &slot, t0, tick, &mut cb, &mut next);
            if (tick + 1) % 3_000 == 0 {
                out.push(stage_of(&report(&slot, &drift, &rx), "play_ring").ms.expect("有读数"));
            }
        }
        out
    }

    /// **修复前**：声卡晶振慢 200 ppm，遥测报出的 `play_ring` 单调爬升，
    /// 而全链路其余五级纹丝不动 —— 这正是「除了水位读数本身没有一个数字会动」
    /// 那一类病的签名。
    #[test]
    fn injection_e_without_the_servo_the_reported_play_ring_climbs_forever() {
        let trace = drift_trace(-200.0, false, 600); // 10 分钟
        assert!(
            trace.windows(2).all(|w| w[1] > w[0]),
            "无伺服 ⇒ 上报的 play_ring 只涨不落，got {trace:?}"
        );
        let climb = trace.last().unwrap() - trace[0];
        // 9.5 分钟 × 200 ppm = 114 ms。留 ±25% 给相位锯齿（读侧 512 帧一块、
        // 写侧 480 一块，裸深度带 ±10.7 ms 的抖动）。
        assert!(
            (climb - 114.0).abs() < 30.0,
            "应当涨约 114 ms，实测 {climb:.0} ms（全程 {trace:?}）"
        );
    }

    /// **修复后**：同样两个时钟，上报的 `play_ring` 稳在目标附近不再爬。
    #[test]
    fn injection_e_the_servo_pins_the_reported_play_ring_at_its_target() {
        let trace = drift_trace(-200.0, true, 600);
        let span = trace.iter().cloned().fold(f64::MIN, f64::max)
            - trace.iter().cloned().fold(f64::MAX, f64::min);
        assert!(
            span < 25.0,
            "十分钟内上报读数的跨度必须收在相位锯齿量级，实测 {span:.0} ms（{trace:?}）"
        );
        for &ms in &trace {
            assert!(
                (10.0..80.0).contains(&ms),
                "上报的 play_ring 应当停在几十毫秒的目标附近，实测 {ms:.0} ms（{trace:?}）"
            );
        }
    }

    /// 模式 B 的虚拟麦克风环（500 ms）。同样是一条不碰站点播放环的流。
    #[test]
    fn a_flooded_hal_mic_ring_reports_half_a_second_in_the_total() {
        let rx = RxStream::new(
            3,
            &[0u8; 32],
            &[0u8; 12],
            None,
            false,
            false,
            None,
            Some(0), // 只写虚拟麦克风
            "127.0.0.1:1".parse().unwrap(),
        );
        seed_upstream_50ms(&rx);
        // 真环的读数由 halbridge.rs 的 `HalBridge::mic_depth` 测试钉死；这里验
        // 的是它满载时穿过槽与汇总的那一段。
        rx.hal_mic.store(Some(StageDepth {
            id: StageId::HalMic,
            samples: 24_000, // 满 = 500 ms
            capacity: 24_000,
            rate: 48_000,
            dropped: Some(1_234),
            drop_mode: DropMode::Newest,
        }));
        let p = report(&StageSlot::new(), &Mutex::new(DriftTracker::new()), &rx);
        let m = stage_of(&p, "hal_mic");
        assert_eq!(m.ms, Some(500.0));
        assert!(m.saturated);
        assert_eq!(p.local_ms, Some(550.0), "30 + 20 + 500");
    }

    /// **三条尾级同时满载时不许相加。** 一条同时监听 + 桥接 + 写虚拟麦克风的
    /// 会话有三条独立的输出环；相加会报出 2.5 秒的假延迟，比要诊断的那 1 秒还大。
    #[test]
    fn three_flooded_tails_do_not_add_up() {
        let (mut site, _s1) = AudioTx::detached_for_test(48_000);
        let (mut bridge, _s2) = AudioTx::detached_for_test(48_000);
        site.push(&vec![0.25f32; 60_000]);
        bridge.push(&vec![0.25f32; 60_000]);

        let rx = RxStream::new(
            4,
            &[0u8; 32],
            &[0u8; 12],
            None,
            true, // 送本机输出
            false,
            Some("dac".to_string()), // 同时桥接
            Some(1),                 // 同时写虚拟麦克风
            "127.0.0.1:1".parse().unwrap(),
        );
        seed_upstream_50ms(&rx);
        let slot = StageSlot::new();
        engine::publish_play_ring(&slot, &site);
        rx.bridge_ring
            .store(Some(engine::ring_depth_before_push(StageId::BridgeRing, &bridge)));
        rx.hal_mic.store(Some(StageDepth {
            id: StageId::HalMic,
            samples: 24_000,
            capacity: 24_000,
            rate: 48_000,
            dropped: Some(0),
            drop_mode: DropMode::Newest,
        }));

        let p = report(&slot, &Mutex::new(DriftTracker::new()), &rx);
        assert_eq!(p.stages.len(), 5, "两条串联级 + 三条并行尾级都要列出来给排障看");
        assert_eq!(
            p.local_ms,
            Some(1_050.0),
            "30 + 20 + max(1000, 1000, 500)；相加会给出 2550 ms"
        );
    }

    // ============================================================ 沉默的反面
    //
    // 「报不出来」有两种：读数是 0，和这一级根本不在报文里。后者更危险 ——
    // 0 至少还能被看见。下面这条把「一级都读不到时必须整体 None」钉死。

    #[test]
    fn a_stage_that_cannot_be_read_makes_the_total_none_not_a_smaller_number() {
        let rx = spk_stream();
        seed_upstream_50ms(&rx);
        let slot = StageSlot::new();
        // 一个 rate == 0 的播放环（设备速率查不到）：这一级**读不到**。
        slot.store(Some(StageDepth {
            id: StageId::PlayRing,
            samples: 48_000,
            capacity: 48_000,
            rate: 0,
            dropped: Some(0),
            drop_mode: DropMode::Newest,
        }));
        let p = report(&slot, &Mutex::new(DriftTracker::new()), &rx);
        assert_eq!(stage_of(&p, "play_ring").ms, None);
        assert_eq!(
            p.local_ms, None,
            "**绝不用 0 填补**：读不到的那一级若按 0 计入，总数会从 1050 掉回 50，看起来完全健康"
        );
    }
}
