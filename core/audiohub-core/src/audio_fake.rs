//! Test-only fake audio backend for the regression suite.
//!
//! A *fake world* is a process-wide set of pretend devices. While one is
//! installed, every stream the product opens through `LivePlayback` /
//! `LiveCapture`, every default-device query, listing, volume read/write and
//! default-change watcher is served from it instead of the platform. Devices
//! are driven by real-time "pseudo-callback" threads that feed the very same
//! ring buffers, clocks and health flags the real cpal callbacks feed, so the
//! product code above `audio.rs` cannot tell the difference.
//!
//! What the fake can inject that a real machine cannot do on demand: a stream
//! death with the default device unchanged, device removal, a default-device
//! change, a format change, clock offset/stalls and open failures.
//!
//! Every native entry point that has no fake implementation calls
//! [`refuse_native`] first. With a world installed it does NOT touch the
//! platform: it counts the attempt (`native_touches`, the tripwire), logs it
//! and fails. A non-zero count therefore means "some product path tried to
//! reach real audio hardware during a fake test", which is never a pass.
//!
//! The module is compiled only with the `fake-audio` feature and refuses to
//! compile without debug assertions, so the product release profile cannot
//! contain it. The marker string [`MARKER`] exists only here; release
//! artefacts are byte-scanned for it.

#[cfg(not(debug_assertions))]
compile_error!(
    "fake-audio requires debug assertions; it must never be built with the release profile \
     (build it with --profile regress)"
);

use super::*;

use std::collections::VecDeque;
use std::f64::consts::TAU;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{MutexGuard, Weak};
use std::thread::JoinHandle;

use serde::{Deserialize, Serialize};

use crate::devlat::{DevLatencyParts, DevTarget, Transport};
use crate::latency::LatSource;
use crate::output_capabilities::{NativeOutputCapabilities, OutputMixFormat, SpatialAudioCapabilities};
use crate::volume::VolumeState;

/// Present in fake builds only. Release artefacts are scanned for it and must
/// not contain it; test-feature artefacts must.
pub const MARKER: &str = "audiohub-fake-audio-v1";

/// Keeps the marker in the object code even if no reachable path printed it.
#[used]
static MARKER_IN_BINARY: &[u8] = MARKER.as_bytes();

/// Schema tag of a fake world description file.
pub const SCHEMA: &str = "ahr.fake.v1";

/// How long a sink keeps mono history for analysis.
const SINK_HISTORY_SECS: usize = 2;

/// A pseudo-callback that has fallen further behind than this resynchronises
/// instead of bursting, the same way a host would drop the missed periods.
const MAX_CATCH_UP: Duration = Duration::from_millis(200);

// ------------------------------------------------------------ configuration

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FakeKind {
    Input,
    Output,
}

impl FakeKind {
    fn of(kind: DeviceKind) -> FakeKind {
        match kind {
            DeviceKind::Input => FakeKind::Input,
            DeviceKind::Output => FakeKind::Output,
        }
    }

    fn word(self) -> &'static str {
        match self {
            FakeKind::Input => "input",
            FakeKind::Output => "output",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FakeSine {
    pub freq: f64,
    #[serde(default = "default_amp")]
    pub amp: f32,
}

/// What an input device "hears". Exactly one of `sine` / `file` may be set;
/// neither means silence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct FakeSourceCfg {
    #[serde(default)]
    pub sine: Option<FakeSine>,
    /// WAV file (PCM i16 or float32). Its rate must equal the device rate; a
    /// relative path is resolved against the world `dir`.
    #[serde(default)]
    pub file: Option<PathBuf>,
    #[serde(default = "default_true", rename = "loop")]
    pub looped: bool,
    #[serde(default)]
    pub silence: bool,
}

impl FakeSourceCfg {
    pub fn sine(freq: f64, amp: f32) -> FakeSourceCfg {
        FakeSourceCfg {
            sine: Some(FakeSine { freq, amp }),
            looped: true,
            ..FakeSourceCfg::default()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FakeDeviceCfg {
    pub uid: String,
    /// Display name; defaults to the UID.
    #[serde(default)]
    pub name: Option<String>,
    pub kind: FakeKind,
    #[serde(default = "default_rate")]
    pub rate: u32,
    #[serde(default = "default_channels")]
    pub channels: u16,
    /// Frames per pseudo-callback; defaults to 10 ms of the device rate.
    #[serde(default)]
    pub period_frames: Option<u32>,
    /// Reported output latency (the `playback - callback` lag of a real host).
    #[serde(default = "default_latency_ms")]
    pub latency_ms: f64,
    /// Device clock offset from real time, parts per million.
    #[serde(default)]
    pub ppm: f64,
    #[serde(default)]
    pub source: Option<FakeSourceCfg>,
    #[serde(default = "default_volume")]
    pub volume: f32,
    #[serde(default)]
    pub muted: bool,
}

impl FakeDeviceCfg {
    pub fn input(uid: &str, rate: u32, channels: u16, source: FakeSourceCfg) -> FakeDeviceCfg {
        FakeDeviceCfg {
            source: Some(source),
            ..FakeDeviceCfg::bare(uid, FakeKind::Input, rate, channels)
        }
    }

    pub fn output(uid: &str, rate: u32, channels: u16) -> FakeDeviceCfg {
        FakeDeviceCfg::bare(uid, FakeKind::Output, rate, channels)
    }

    fn bare(uid: &str, kind: FakeKind, rate: u32, channels: u16) -> FakeDeviceCfg {
        FakeDeviceCfg {
            uid: uid.to_string(),
            name: None,
            kind,
            rate,
            channels,
            period_frames: None,
            latency_ms: default_latency_ms(),
            ppm: 0.0,
            source: None,
            volume: default_volume(),
            muted: false,
        }
    }

    fn display_name(&self) -> String {
        self.name.clone().unwrap_or_else(|| self.uid.clone())
    }

    fn period(&self) -> u32 {
        self.period_frames.unwrap_or((self.rate / 100).max(1))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FakeWorldCfg {
    #[serde(default)]
    pub schema: Option<String>,
    /// Directory for `events.jsonl` and the sink WAV files. `None` = keep
    /// everything in memory only.
    #[serde(default)]
    pub dir: Option<PathBuf>,
    pub devices: Vec<FakeDeviceCfg>,
    #[serde(default)]
    pub default_input: Option<String>,
    #[serde(default)]
    pub default_output: Option<String>,
}

impl FakeWorldCfg {
    /// The world a fake daemon gets when no description file is given: one
    /// 48 kHz stereo microphone hearing a 1 kHz tone and one 48 kHz stereo
    /// output.
    pub fn default_world() -> FakeWorldCfg {
        FakeWorldCfg {
            schema: Some(SCHEMA.to_string()),
            dir: None,
            devices: vec![
                FakeDeviceCfg::input("fake-mic-1", 48_000, 2, FakeSourceCfg::sine(1000.0, 0.25)),
                FakeDeviceCfg::output("fake-spk-1", 48_000, 2),
            ],
            default_input: Some("fake-mic-1".to_string()),
            default_output: Some("fake-spk-1".to_string()),
        }
    }
}

fn default_amp() -> f32 {
    0.25
}
fn default_true() -> bool {
    true
}
fn default_rate() -> u32 {
    48_000
}
fn default_channels() -> u16 {
    2
}
fn default_latency_ms() -> f64 {
    10.0
}
fn default_volume() -> f32 {
    1.0
}

/// One injected event. JSON form: `{"action":"kill_stream","uid":"fake-mic-1"}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum FakeInject {
    /// Every live stream on the device dies; the default is unchanged and no
    /// notification is sent.
    KillStream { uid: String },
    /// The device disappears and its streams die. If it was a default, the
    /// default moves to `fallback` (or to none) and watchers are notified.
    RemoveDevice {
        uid: String,
        #[serde(default)]
        fallback: Option<String>,
    },
    /// Adds a device, or brings a removed one back with a new description.
    AddDevice { device: FakeDeviceCfg },
    /// Changes the default device of one direction and notifies watchers.
    /// `uid: null` leaves the direction without a default.
    SetDefault {
        kind: FakeKind,
        #[serde(default)]
        uid: Option<String>,
    },
    /// The device's format changes and its live streams die. `notify=false`
    /// (the default) models a format change with the default device unchanged.
    ChangeFormat {
        uid: String,
        rate: u32,
        channels: u16,
        #[serde(default)]
        notify: bool,
    },
    SetPpm { uid: String, ppm: f64 },
    /// Pseudo-callbacks on the device stop for `ms`; the missed periods are
    /// dropped, not replayed.
    Stall { uid: String, ms: u64 },
    SetVolume {
        uid: String,
        #[serde(default)]
        scalar: Option<f32>,
        #[serde(default)]
        muted: Option<bool>,
    },
    /// The next `count` opens of the device fail.
    FailNextOpen { uid: String, count: u32 },
}

impl FakeInject {
    fn name(&self) -> &'static str {
        match self {
            FakeInject::KillStream { .. } => "kill_stream",
            FakeInject::RemoveDevice { .. } => "remove_device",
            FakeInject::AddDevice { .. } => "add_device",
            FakeInject::SetDefault { .. } => "set_default",
            FakeInject::ChangeFormat { .. } => "change_format",
            FakeInject::SetPpm { .. } => "set_ppm",
            FakeInject::Stall { .. } => "stall",
            FakeInject::SetVolume { .. } => "set_volume",
            FakeInject::FailNextOpen { .. } => "fail_next_open",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct InjectOutcome {
    pub action: &'static str,
    /// Live streams that were killed by this action.
    pub killed_streams: u32,
    /// Whether default-change watchers were notified.
    pub notified: bool,
}

// ------------------------------------------------------------- inspection

#[derive(Debug, Clone, Serialize)]
pub struct FakeStreamStatus {
    pub id: u64,
    pub uid: String,
    pub kind: FakeKind,
    pub gen: u64,
    pub alive: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct FakeDeviceStatus {
    pub uid: String,
    pub name: String,
    pub kind: FakeKind,
    pub present: bool,
    pub rate: u32,
    pub channels: u16,
    pub period_frames: u32,
    pub ppm: f64,
    pub volume: f32,
    pub muted: bool,
    /// Successful opens so far; the latest stream carries this as its `gen`.
    pub opens: u64,
    pub live_streams: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct FakeStatus {
    pub marker: &'static str,
    pub schema: &'static str,
    pub native_touches: u64,
    pub native_touch_sites: Vec<String>,
    pub default_input: Option<String>,
    pub default_output: Option<String>,
    pub devices: Vec<FakeDeviceStatus>,
    pub streams: Vec<FakeStreamStatus>,
    pub dir: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct ToneFit {
    pub freq_hz: f64,
    pub amp: f64,
    pub snr_db: f64,
    pub detected: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SinkStats {
    pub uid: String,
    /// Stream generation the history below belongs to (0 = never opened).
    pub gen: u64,
    pub rate: u32,
    /// Frames consumed by every generation of this device.
    pub frames_total: u64,
    /// Frames consumed by the current generation.
    pub gen_frames: u64,
    /// Frames the analysis window actually covered.
    pub window_frames: u64,
    pub rms: f64,
    pub tone: Option<ToneFit>,
}

/// Least-squares fit of a sinusoid near `freq` (searched over +-2 Hz, so a
/// small servo correction does not read as distortion). `snr_db` is fitted
/// tone energy over residual energy; `detected` requires a tone that is both
/// audible and clean.
pub fn fit_tone(x: &[f32], rate: u32, freq: f64) -> ToneFit {
    let n = x.len();
    let none = ToneFit { freq_hz: freq, amp: 0.0, snr_db: f64::NEG_INFINITY, detected: false };
    if n < 16 || rate == 0 {
        return none;
    }
    let mean = x.iter().map(|&v| v as f64).sum::<f64>() / n as f64;
    let total: f64 = x.iter().map(|&v| (v as f64 - mean).powi(2)).sum();
    let mut best = none;
    let mut best_fit = -1.0f64;
    let mut f = freq - 2.0;
    while f <= freq + 2.0 + 1e-9 {
        let w = TAU * f / rate as f64;
        let (mut scc, mut sss, mut scs, mut sxc, mut sxs) = (0.0, 0.0, 0.0, 0.0, 0.0);
        for (i, &v) in x.iter().enumerate() {
            let (s, c) = (w * i as f64).sin_cos();
            let v = v as f64 - mean;
            scc += c * c;
            sss += s * s;
            scs += c * s;
            sxc += v * c;
            sxs += v * s;
        }
        let det = scc * sss - scs * scs;
        if det.abs() > 1e-12 {
            let a = (sxc * sss - sxs * scs) / det;
            let b = (sxs * scc - sxc * scs) / det;
            let fit = a * sxc + b * sxs;
            if fit > best_fit {
                best_fit = fit;
                let resid = (total - fit).max(1e-20);
                let amp = (a * a + b * b).sqrt();
                let snr_db = 10.0 * (fit.max(1e-20) / resid).log10();
                best = ToneFit { freq_hz: f, amp, snr_db, detected: amp >= 0.005 && snr_db >= 20.0 };
            }
        }
        f += 0.25;
    }
    best
}

// ------------------------------------------------------------- the world

static WORLD: Mutex<Option<Arc<World>>> = Mutex::new(None);
/// Serialises in-process installs: one fake world per process at a time.
static INSTALL: Mutex<()> = Mutex::new(());

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// The installed world, if any. Cheap enough for open/query paths; never
/// called from a real-time callback.
pub fn active() -> Option<Arc<World>> {
    lock(&WORLD).clone()
}

/// Gate in front of a native entry point that has no fake implementation.
/// Without a world: `Ok(())`, the real path runs. With one: the attempt is
/// counted as a native touch and refused, so the platform is never reached.
pub fn refuse_native(site: &str) -> Result<()> {
    match active() {
        None => Ok(()),
        Some(world) => {
            world.native_touch(site);
            bail!("fake audio backend: native audio entry point {site} is not available while a fake world is active")
        }
    }
}

/// Holds the world installed by [`install`] and the in-process install lock.
/// Dropping it uninstalls the world. Declare it before any daemon in a test so
/// the daemons are dropped first.
pub struct WorldGuard {
    world: Arc<World>,
    _serial: MutexGuard<'static, ()>,
}

impl WorldGuard {
    pub fn world(&self) -> &Arc<World> {
        &self.world
    }
}

impl Drop for WorldGuard {
    fn drop(&mut self) {
        let mut slot = lock(&WORLD);
        if slot.as_ref().is_some_and(|w| Arc::ptr_eq(w, &self.world)) {
            *slot = None;
        }
        self.world.log_event("world_uninstalled", "", "");
    }
}

/// Installs a world for the duration of the returned guard. Blocks while
/// another guard is alive in this process.
pub fn install(cfg: FakeWorldCfg) -> Result<WorldGuard> {
    let serial = lock(&INSTALL);
    let world = World::build(cfg)?;
    *lock(&WORLD) = Some(Arc::clone(&world));
    Ok(WorldGuard { world, _serial: serial })
}

/// Installs a world for the rest of the process (daemon binaries). Refuses
/// when one is already active.
pub fn install_for_process(cfg: FakeWorldCfg) -> Result<Arc<World>> {
    let world = World::build(cfg)?;
    let mut slot = lock(&WORLD);
    if slot.is_some() {
        bail!("a fake audio world is already installed in this process");
    }
    *slot = Some(Arc::clone(&world));
    Ok(world)
}

/// Selects a device the way the real `start*` entry points do.
#[derive(Debug, Clone, Copy)]
pub enum Target<'a> {
    Default,
    Name(&'a str),
    Uid(&'a str),
}

#[derive(Clone)]
enum SourceData {
    Silence,
    Sine { freq: f64, amp: f32 },
    File { samples: Arc<Vec<f32>>, channels: u16, looped: bool },
}

/// Per-device values the pseudo-callback threads read every period.
struct DevLive {
    ppm_bits: AtomicU64,
    /// Milliseconds since the world was built; 0 = not stalled.
    stall_until_ms: AtomicU64,
}

struct Sink {
    gen: u64,
    rate: u32,
    frames_total: u64,
    gen_frames: u64,
    history: VecDeque<f32>,
}

struct Dev {
    cfg: FakeDeviceCfg,
    present: bool,
    source: SourceData,
    live: Arc<DevLive>,
    volume: f32,
    muted: bool,
    fail_next_open: u32,
    opens: u64,
    sink: Arc<Mutex<Sink>>,
}

struct StreamCtl {
    stop: AtomicBool,
}

struct StreamRec {
    id: u64,
    uid: String,
    kind: FakeKind,
    gen: u64,
    health: Arc<StreamHealth>,
}

struct Watcher {
    id: u64,
    kind: FakeKind,
    fanout: Weak<Fanout>,
}

struct State {
    devices: Vec<Dev>,
    default_input: Option<String>,
    default_output: Option<String>,
    watchers: Vec<Watcher>,
    streams: Vec<StreamRec>,
    touch_sites: Vec<String>,
}

impl State {
    fn dev(&self, uid: &str) -> Option<&Dev> {
        self.devices.iter().find(|d| d.cfg.uid == uid)
    }

    fn dev_mut(&mut self, uid: &str) -> Option<&mut Dev> {
        self.devices.iter_mut().find(|d| d.cfg.uid == uid)
    }

    fn require_mut(&mut self, uid: &str) -> Result<&mut Dev> {
        self.dev_mut(uid).ok_or_else(|| anyhow!("fake: no device with UID {uid:?}"))
    }

    fn default_uid(&self, kind: FakeKind) -> Option<&String> {
        match kind {
            FakeKind::Input => self.default_input.as_ref(),
            FakeKind::Output => self.default_output.as_ref(),
        }
    }

    fn set_default_uid(&mut self, kind: FakeKind, uid: Option<String>) {
        match kind {
            FakeKind::Input => self.default_input = uid,
            FakeKind::Output => self.default_output = uid,
        }
    }

    /// Present devices of one direction.
    fn usable(&self, kind: FakeKind) -> impl Iterator<Item = &Dev> {
        self.devices.iter().filter(move |d| d.present && d.cfg.kind == kind)
    }

    /// Resolves a target to a UID with the real resolution rules: the default
    /// must exist, names go through the same `resolve_name`, UIDs are exact.
    fn resolve(&self, kind: FakeKind, target: Target<'_>) -> Result<String> {
        let dk = match kind {
            FakeKind::Input => DeviceKind::Input,
            FakeKind::Output => DeviceKind::Output,
        };
        match target {
            Target::Default => self
                .default_uid(kind)
                .filter(|uid| self.dev(uid).is_some_and(|d| d.present && d.cfg.kind == kind))
                .cloned()
                .ok_or_else(|| anyhow!("no default {} device", kind.word())),
            Target::Name(query) => {
                let names: Vec<String> = self.usable(kind).map(|d| d.cfg.display_name()).collect();
                let resolved = resolve_name(&names, query, dk)?;
                self.usable(kind)
                    .find(|d| d.cfg.display_name() == resolved)
                    .map(|d| d.cfg.uid.clone())
                    .ok_or_else(|| anyhow!("audio device {resolved:?} vanished between listing and open"))
            }
            Target::Uid(uid) => self
                .usable(kind)
                .find(|d| d.cfg.uid == uid)
                .map(|d| d.cfg.uid.clone())
                .ok_or_else(|| anyhow!("no {} device has UID {uid:?}", kind.word())),
        }
    }

    /// Kills every live stream on `uid` with `why`. Returns how many died.
    fn kill_streams(&mut self, uid: &str, why: &str) -> u32 {
        let mut n = 0;
        for s in self.streams.iter().filter(|s| s.uid == uid) {
            if s.health.is_alive() {
                s.health.fail_message(format!("fake: {why} ({uid}, stream {})", s.id));
                n += 1;
            }
        }
        n
    }

    /// Wakes every live watcher of `kind`, as a platform default-change
    /// notification would.
    fn notify(&mut self, kind: FakeKind) -> bool {
        self.watchers.retain(|w| w.fanout.strong_count() > 0);
        let mut any = false;
        for w in self.watchers.iter().filter(|w| w.kind == kind) {
            if let Some(f) = w.fanout.upgrade() {
                f.signal();
                any = true;
            }
        }
        any
    }
}

pub struct World {
    dir: Option<PathBuf>,
    started: Instant,
    state: Mutex<State>,
    native_touches: AtomicU64,
    next_id: AtomicU64,
    log: Option<Mutex<File>>,
}

impl World {
    fn build(cfg: FakeWorldCfg) -> Result<Arc<World>> {
        if let Some(schema) = &cfg.schema {
            if schema != SCHEMA {
                bail!("fake world schema {schema:?} is not {SCHEMA:?}");
            }
        }
        let mut devices: Vec<Dev> = Vec::new();
        for d in cfg.devices {
            if devices.iter().any(|e| e.cfg.uid == d.uid) {
                bail!("fake world lists device {:?} twice", d.uid);
            }
            devices.push(Dev::build(d, cfg.dir.as_deref())?);
        }
        let mut state = State {
            devices,
            default_input: None,
            default_output: None,
            watchers: Vec::new(),
            streams: Vec::new(),
            touch_sites: Vec::new(),
        };
        for (kind, uid) in [(FakeKind::Input, cfg.default_input), (FakeKind::Output, cfg.default_output)] {
            if let Some(uid) = uid {
                match state.dev(&uid) {
                    Some(d) if d.cfg.kind == kind => state.set_default_uid(kind, Some(uid)),
                    Some(_) => bail!("default {} {uid:?} is not an {} device", kind.word(), kind.word()),
                    None => bail!("default {} {uid:?} is not a listed device", kind.word()),
                }
            }
        }
        let log = match &cfg.dir {
            Some(dir) => {
                std::fs::create_dir_all(dir.join("sink"))
                    .with_context(|| format!("create fake world dir {}", dir.display()))?;
                let f = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(dir.join("events.jsonl"))
                    .with_context(|| format!("open {}", dir.join("events.jsonl").display()))?;
                Some(Mutex::new(f))
            }
            None => None,
        };
        let world = Arc::new(World {
            dir: cfg.dir,
            started: Instant::now(),
            state: Mutex::new(state),
            native_touches: AtomicU64::new(0),
            next_id: AtomicU64::new(1),
            log,
        });
        world.log_event("world_installed", "", MARKER);
        Ok(world)
    }

    fn state(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    fn now_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    /// Appends one line to `events.jsonl` (when the world has a directory).
    fn log_event(&self, event: &str, uid: &str, detail: &str) {
        let Some(log) = &self.log else { return };
        let line = format!(
            "{{\"t_ms\":{},\"event\":{},\"uid\":{},\"detail\":{}}}\n",
            self.now_ms(),
            json_str(event),
            json_str(uid),
            json_str(detail)
        );
        let _ = lock(log).write_all(line.as_bytes());
    }

    /// The tripwire. See the module documentation.
    pub fn native_touch(&self, site: &str) {
        let n = self.native_touches.fetch_add(1, Ordering::SeqCst) + 1;
        eprintln!("[audiohub] fake audio: native entry point {site} reached (native_touches={n})");
        {
            let mut st = self.state();
            if !st.touch_sites.iter().any(|s| s == site) {
                st.touch_sites.push(site.to_string());
            }
        }
        self.log_event("native_touch", "", site);
    }

    pub fn native_touches(&self) -> u64 {
        self.native_touches.load(Ordering::SeqCst)
    }

    // ------------------------------------------------ queries (audio.rs etc.)

    pub(crate) fn default_device_presence(&self) -> DefaultDevicePresence {
        let st = self.state();
        DefaultDevicePresence {
            input: st.resolve(FakeKind::Input, Target::Default).is_ok(),
            output: st.resolve(FakeKind::Output, Target::Default).is_ok(),
        }
    }

    pub(crate) fn list_detailed(&self) -> Vec<DeviceEntry> {
        self.state()
            .devices
            .iter()
            .filter(|d| d.present)
            .map(|d| DeviceEntry {
                name: d.cfg.display_name(),
                uid: Some(d.cfg.uid.clone()),
                id: None,
                is_input: d.cfg.kind == FakeKind::Input,
                is_output: d.cfg.kind == FakeKind::Output,
            })
            .collect()
    }

    pub(crate) fn list_names(&self, kind: DeviceKind) -> Vec<String> {
        self.state().usable(FakeKind::of(kind)).map(|d| d.cfg.display_name()).collect()
    }

    pub(crate) fn all_device_names(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for d in self.state().devices.iter().filter(|d| d.present) {
            let n = d.cfg.display_name();
            if !out.contains(&n) {
                out.push(n);
            }
        }
        out
    }

    pub(crate) fn name_for_uid(&self, kind: DeviceKind, uid: &str) -> Result<String> {
        let st = self.state();
        let uid = st.resolve(FakeKind::of(kind), Target::Uid(uid))?;
        Ok(st.dev(&uid).map(|d| d.cfg.display_name()).unwrap_or_default())
    }

    pub(crate) fn devices_report(&self) -> DevicesReport {
        let (default_output, default_input, output_config) = {
            let st = self.state();
            let name = |kind| {
                st.resolve(kind, Target::Default)
                    .ok()
                    .and_then(|uid| st.dev(&uid).map(|d| d.cfg.display_name()))
            };
            let output_config = st
                .resolve(FakeKind::Output, Target::Default)
                .ok()
                .and_then(|uid| st.dev(&uid).map(|d| format!("{}Hz {}ch f32", d.cfg.rate, d.cfg.channels)));
            (name(FakeKind::Output), name(FakeKind::Input), output_config)
        };
        DevicesReport {
            default_output,
            default_input,
            output_config,
            // Same contract as the real report: never an input config.
            input_config: None,
            devices: self.list_detailed(),
        }
    }

    pub(crate) fn devlat_query(&self, kind: DeviceKind, target: DevTarget<'_>) -> DevLatencyParts {
        let st = self.state();
        let fk = FakeKind::of(kind);
        let resolved = match target {
            DevTarget::Default => st.resolve(fk, Target::Default),
            DevTarget::Name(n) => st.resolve(fk, Target::Name(n)),
            DevTarget::Uid(u) => st.resolve(fk, Target::Uid(u)),
        };
        let dev = match resolved.ok().and_then(|uid| st.dev(&uid)) {
            Some(d) => d,
            None => return DevLatencyParts::empty(format!("fake: no matching {} device", fk.word())),
        };
        let frames = (dev.cfg.latency_ms.max(0.0) * dev.cfg.rate as f64 / 1000.0).round() as u32;
        DevLatencyParts {
            parts: vec![("fake_latency", frames)],
            missing: Vec::new(),
            rate: dev.cfg.rate,
            transport: Transport::Virtual,
            transport_code: 0,
            device: Some(dev.cfg.display_name()),
            base_source: LatSource::Api,
            error: None,
        }
    }

    pub(crate) fn output_capabilities(&self) -> NativeOutputCapabilities {
        let st = self.state();
        match st.resolve(FakeKind::Output, Target::Default).ok().and_then(|uid| st.dev(&uid)) {
            Some(d) => NativeOutputCapabilities::Available {
                endpoint_id: d.cfg.uid.clone(),
                mix_format: OutputMixFormat {
                    sample_rate: d.cfg.rate,
                    channels: d.cfg.channels,
                    sample_format: "f32".to_string(),
                    channel_mask: None,
                },
                spatial_audio: SpatialAudioCapabilities::Unsupported,
            },
            None => NativeOutputCapabilities::Unavailable,
        }
    }

    // --------------------------------------------------------------- volume

    fn volume_target(st: &State, input: bool, dev: Option<&str>) -> Result<String> {
        let kind = if input { FakeKind::Input } else { FakeKind::Output };
        match dev {
            None => st.resolve(kind, Target::Default),
            Some(q) => st
                .usable(kind)
                .find(|d| d.cfg.uid == q || d.cfg.display_name() == q)
                .map(|d| d.cfg.uid.clone())
                .ok_or_else(|| anyhow!("no {} device named {q:?}", kind.word())),
        }
    }

    pub(crate) fn volume_get(&self, input: bool, dev: Option<&str>) -> Result<VolumeState> {
        let st = self.state();
        let uid = Self::volume_target(&st, input, dev)?;
        let d = st.dev(&uid).ok_or_else(|| anyhow!("fake: device {uid:?} vanished"))?;
        Ok(VolumeState { scalar: d.volume, muted: d.muted, adjustable: true, mute_adjustable: true })
    }

    pub(crate) fn volume_set(
        &self,
        input: bool,
        dev: Option<&str>,
        scalar: Option<f32>,
        muted: Option<bool>,
    ) -> Result<()> {
        let uid = {
            let mut st = self.state();
            let uid = Self::volume_target(&st, input, dev)?;
            let d = st.require_mut(&uid)?;
            if let Some(s) = scalar {
                d.volume = s.clamp(0.0, 1.0);
            }
            if let Some(m) = muted {
                d.muted = m;
            }
            uid
        };
        self.log_event("volume_set", &uid, &format!("scalar={scalar:?} muted={muted:?}"));
        Ok(())
    }

    // ------------------------------------------------------------- watchers

    pub(super) fn register_watcher(&self, kind: DeviceKind, fanout: Arc<Fanout>) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        self.state().watchers.push(Watcher {
            id,
            kind: FakeKind::of(kind),
            fanout: Arc::downgrade(&fanout),
        });
        id
    }

    pub(super) fn unregister_watcher(&self, id: u64) {
        self.state().watchers.retain(|w| w.id != id);
    }

    // -------------------------------------------------------------- streams

    fn take_open(&self, st: &mut State, kind: FakeKind, target: Target<'_>) -> Result<String> {
        let uid = st.resolve(kind, target)?;
        let d = st.require_mut(&uid)?;
        if d.fail_next_open > 0 {
            d.fail_next_open -= 1;
            self.log_event("open_failed", &uid, "injected");
            bail!("fake: injected open failure on {} device {uid:?}", kind.word());
        }
        d.opens += 1;
        Ok(uid)
    }

    fn register_stream(&self, st: &mut State, uid: &str, kind: FakeKind, gen: u64, health: &Arc<StreamHealth>) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        st.streams.push(StreamRec { id, uid: uid.to_string(), kind, gen, health: Arc::clone(health) });
        id
    }

    fn remove_stream(&self, id: u64) {
        self.state().streams.retain(|s| s.id != id);
    }

    pub(crate) fn open_output(
        self: &Arc<Self>,
        target: Target<'_>,
        src_rate: u32,
        channels: u8,
    ) -> Result<(LivePlayback, AudioTx)> {
        let channels = channels.clamp(1, 2);
        let mut st = self.state();
        let uid = self.take_open(&mut st, FakeKind::Output, target)?;
        let d = st.dev(&uid).ok_or_else(|| anyhow!("fake: device {uid:?} vanished"))?;
        let gen = d.opens;
        let dev_rate = d.cfg.rate;
        let dev_channels = d.cfg.channels.max(1);
        let period = d.cfg.period() as usize;
        let latency = Duration::from_secs_f64(d.cfg.latency_ms.max(0.0) / 1000.0);
        let live = Arc::clone(&d.live);
        let sink = Arc::clone(&d.sink);
        {
            let mut s = lock(&sink);
            s.gen = gen;
            s.rate = dev_rate;
            s.gen_frames = 0;
            s.history.clear();
        }
        let config_snapshot = PlaybackConfigSnapshot {
            source_rate_hz: src_rate,
            source_channels: channels,
            device_rate_hz: dev_rate,
            device_channels: dev_channels,
            sample_format: "F32".to_string(),
            buffer_size: format!("Fixed({period})"),
        };
        // Same ring as `LivePlayback::on_device`: 1 s of device-rate frames.
        let rb = HeapRb::<f32>::new(dev_rate.max(8000) as usize * channels as usize);
        let (prod, cons) = rb.split();
        let health = StreamHealth::new();
        let dev = DeviceClock::new();
        let id = self.register_stream(&mut st, &uid, FakeKind::Output, gen, &health);
        drop(st);

        let ctl = Arc::new(StreamCtl { stop: AtomicBool::new(false) });
        let wav = self.open_wav(&uid, gen, dev_rate, dev_channels);
        let job = OutputJob {
            cons,
            dev: Arc::clone(&dev),
            health: Arc::clone(&health),
            ctl: Arc::clone(&ctl),
            live,
            sink,
            wav,
            rate: dev_rate,
            src_channels: channels as usize,
            dev_channels: dev_channels as usize,
            period,
            latency,
            world_started: self.started,
        };
        let thread = std::thread::Builder::new()
            .name(format!("fake-out-{uid}"))
            .spawn(move || job.run())
            .context("spawn fake output thread")?;
        self.log_event("stream_opened", &uid, &format!("output gen={gen} stream={id} rate={dev_rate} ch={dev_channels}"));
        let guard = StreamGuard { world: Arc::clone(self), id, uid, ctl, thread: Some(thread) };
        Ok((
            LivePlayback { _stream: StreamHold::Fake(guard), health, config: config_snapshot },
            AudioTx {
                prepared: Arc::new(AtomicBool::new(false)),
                prod,
                channels,
                resampler: Some(InterleavedVarResampler::new(src_rate, dev_rate, channels)),
                staging: Vec::new(),
                servo: PlayServo::new(dev_rate, true),
                dev,
                dev_rate,
                src_rate,
                dropped: Arc::new(AtomicU64::new(0)),
            },
        ))
    }

    pub(crate) fn open_input(self: &Arc<Self>, target: Target<'_>) -> Result<(LiveCapture, AudioRx, u32)> {
        let mut st = self.state();
        let uid = self.take_open(&mut st, FakeKind::Input, target)?;
        let d = st.dev(&uid).ok_or_else(|| anyhow!("fake: device {uid:?} vanished"))?;
        let gen = d.opens;
        let rate = d.cfg.rate;
        let channels = d.cfg.channels.max(1) as usize;
        let period = d.cfg.period() as usize;
        let source = d.source.clone();
        let live = Arc::clone(&d.live);
        // Same ring as `LiveCapture::on_device`: 2 s of mono at the device rate.
        let rb = HeapRb::<f32>::new((rate as usize) * 2);
        let (prod, cons) = rb.split();
        let dropped = Arc::new(AtomicU64::new(0));
        let health = StreamHealth::new();
        let id = self.register_stream(&mut st, &uid, FakeKind::Input, gen, &health);
        drop(st);

        let ctl = Arc::new(StreamCtl { stop: AtomicBool::new(false) });
        let job = InputJob {
            prod,
            dropped: Arc::clone(&dropped),
            health: Arc::clone(&health),
            ctl: Arc::clone(&ctl),
            live,
            source,
            rate,
            channels,
            period,
            world_started: self.started,
        };
        let thread = std::thread::Builder::new()
            .name(format!("fake-in-{uid}"))
            .spawn(move || job.run())
            .context("spawn fake input thread")?;
        self.log_event("stream_opened", &uid, &format!("input gen={gen} stream={id} rate={rate} ch={channels}"));
        let guard = StreamGuard { world: Arc::clone(self), id, uid, ctl, thread: Some(thread) };
        Ok((
            LiveCapture { _stream: StreamHold::Fake(guard), health },
            AudioRx { cons, rate, dropped },
            rate,
        ))
    }

    fn open_wav(&self, uid: &str, gen: u64, rate: u32, channels: u16) -> Option<WavOut> {
        let dir = self.dir.as_ref()?;
        let path = dir.join("sink").join(format!("{}-gen{gen}.wav", file_safe(uid)));
        match WavOut::create(&path, rate, channels) {
            Ok(w) => Some(w),
            Err(e) => {
                eprintln!("[audiohub] fake audio: sink file {} not written: {e:#}", path.display());
                None
            }
        }
    }

    // ------------------------------------------------------------ injection

    pub fn inject(&self, action: FakeInject) -> Result<InjectOutcome> {
        let name = action.name();
        let mut killed = 0;
        let mut notified = false;
        let uid_for_log: String;
        {
            let mut st = self.state();
            match &action {
                FakeInject::KillStream { uid } => {
                    st.require_mut(uid)?;
                    killed = st.kill_streams(uid, "injected stream death");
                    uid_for_log = uid.clone();
                }
                FakeInject::RemoveDevice { uid, fallback } => {
                    let kind = st.require_mut(uid)?.cfg.kind;
                    if let Some(fb) = fallback {
                        match st.dev(fb) {
                            Some(d) if d.present && d.cfg.kind == kind && fb != uid => {}
                            _ => bail!("fake: fallback {fb:?} is not a present {} device", kind.word()),
                        }
                    }
                    st.require_mut(uid)?.present = false;
                    killed = st.kill_streams(uid, "device removed");
                    if st.default_uid(kind).is_some_and(|d| d == uid) {
                        st.set_default_uid(kind, fallback.clone());
                        notified = st.notify(kind);
                    }
                    uid_for_log = uid.clone();
                }
                FakeInject::AddDevice { device } => {
                    let built = Dev::build(device.clone(), self.dir.as_deref())?;
                    match st.dev_mut(&device.uid) {
                        Some(existing) => {
                            if existing.cfg.kind != device.kind {
                                bail!("fake: device {:?} cannot change direction", device.uid);
                            }
                            existing.cfg = built.cfg;
                            existing.source = built.source;
                            existing.present = true;
                            existing.live.ppm_bits.store(device.ppm.to_bits(), Ordering::Relaxed);
                        }
                        None => st.devices.push(built),
                    }
                    uid_for_log = device.uid.clone();
                }
                FakeInject::SetDefault { kind, uid } => {
                    if let Some(u) = uid {
                        match st.dev(u) {
                            Some(d) if d.present && d.cfg.kind == *kind => {}
                            _ => bail!("fake: {u:?} is not a present {} device", kind.word()),
                        }
                    }
                    st.set_default_uid(*kind, uid.clone());
                    notified = st.notify(*kind);
                    uid_for_log = uid.clone().unwrap_or_default();
                }
                FakeInject::ChangeFormat { uid, rate, channels, notify } => {
                    if !(8_000..=384_000).contains(rate) || !(1..=8).contains(channels) {
                        bail!("fake: format {rate} Hz / {channels} ch is out of range");
                    }
                    let d = st.require_mut(uid)?;
                    let kind = d.cfg.kind;
                    let old_period = d.cfg.period_frames;
                    let old_rate = d.cfg.rate;
                    d.cfg.rate = *rate;
                    d.cfg.channels = *channels;
                    // A configured period keeps its duration, not its frame count.
                    d.cfg.period_frames =
                        old_period.map(|p| ((p as u64 * *rate as u64) / old_rate.max(1) as u64).max(1) as u32);
                    killed = st.kill_streams(uid, "device format changed");
                    if *notify {
                        notified = st.notify(kind);
                    }
                    uid_for_log = uid.clone();
                }
                FakeInject::SetPpm { uid, ppm } => {
                    if !ppm.is_finite() || ppm.abs() > 100_000.0 {
                        bail!("fake: ppm {ppm} is out of range");
                    }
                    let d = st.require_mut(uid)?;
                    d.cfg.ppm = *ppm;
                    d.live.ppm_bits.store(ppm.to_bits(), Ordering::Relaxed);
                    uid_for_log = uid.clone();
                }
                FakeInject::Stall { uid, ms } => {
                    let until = self.now_ms() + (*ms).max(1);
                    st.require_mut(uid)?.live.stall_until_ms.store(until, Ordering::Relaxed);
                    uid_for_log = uid.clone();
                }
                FakeInject::SetVolume { uid, scalar, muted } => {
                    let d = st.require_mut(uid)?;
                    if let Some(s) = scalar {
                        if !s.is_finite() {
                            bail!("fake: volume must be finite");
                        }
                        d.volume = s.clamp(0.0, 1.0);
                    }
                    if let Some(m) = muted {
                        d.muted = *m;
                    }
                    uid_for_log = uid.clone();
                }
                FakeInject::FailNextOpen { uid, count } => {
                    st.require_mut(uid)?.fail_next_open = *count;
                    uid_for_log = uid.clone();
                }
            }
        }
        self.log_event(
            &format!("inject:{name}"),
            &uid_for_log,
            &format!("killed={killed} notified={notified}"),
        );
        Ok(InjectOutcome { action: name, killed_streams: killed, notified })
    }

    // ----------------------------------------------------------- inspection

    pub fn status(&self) -> FakeStatus {
        let st = self.state();
        let devices = st
            .devices
            .iter()
            .map(|d| FakeDeviceStatus {
                uid: d.cfg.uid.clone(),
                name: d.cfg.display_name(),
                kind: d.cfg.kind,
                present: d.present,
                rate: d.cfg.rate,
                channels: d.cfg.channels,
                period_frames: d.cfg.period(),
                ppm: d.cfg.ppm,
                volume: d.volume,
                muted: d.muted,
                opens: d.opens,
                live_streams: st
                    .streams
                    .iter()
                    .filter(|s| s.uid == d.cfg.uid && s.health.is_alive())
                    .count() as u32,
            })
            .collect();
        let streams = st
            .streams
            .iter()
            .map(|s| FakeStreamStatus {
                id: s.id,
                uid: s.uid.clone(),
                kind: s.kind,
                gen: s.gen,
                alive: s.health.is_alive(),
            })
            .collect();
        FakeStatus {
            marker: MARKER,
            schema: SCHEMA,
            native_touches: self.native_touches(),
            native_touch_sites: st.touch_sites.clone(),
            default_input: st.default_input.clone(),
            default_output: st.default_output.clone(),
            devices,
            streams,
            dir: self.dir.as_ref().map(|d| d.display().to_string()),
        }
    }

    /// What the output device `uid` has played. Tone analysis covers the last
    /// `window_ms` of the current generation only, so audio from a stream
    /// that has since died can never satisfy a recovery check.
    pub fn sink_stats(&self, uid: &str, freq: Option<f64>, window_ms: u32) -> Result<SinkStats> {
        let sink = {
            let st = self.state();
            let d = st.dev(uid).ok_or_else(|| anyhow!("fake: no device with UID {uid:?}"))?;
            if d.cfg.kind != FakeKind::Output {
                bail!("fake: {uid:?} is not an output device");
            }
            Arc::clone(&d.sink)
        };
        let s = lock(&sink);
        let want = (s.rate as u64 * window_ms as u64 / 1000) as usize;
        let take = want.min(s.history.len());
        let window: Vec<f32> = s.history.iter().skip(s.history.len() - take).copied().collect();
        let rms = if window.is_empty() {
            0.0
        } else {
            (window.iter().map(|&v| (v as f64).powi(2)).sum::<f64>() / window.len() as f64).sqrt()
        };
        let tone = freq.map(|f| fit_tone(&window, s.rate, f));
        Ok(SinkStats {
            uid: uid.to_string(),
            gen: s.gen,
            rate: s.rate,
            frames_total: s.frames_total,
            gen_frames: s.gen_frames,
            window_frames: window.len() as u64,
            rms,
            tone,
        })
    }
}

impl Dev {
    fn build(cfg: FakeDeviceCfg, dir: Option<&Path>) -> Result<Dev> {
        if cfg.uid.trim().is_empty() {
            bail!("fake device with an empty UID");
        }
        if !(8_000..=384_000).contains(&cfg.rate) {
            bail!("fake device {:?}: rate {} is out of range", cfg.uid, cfg.rate);
        }
        if !(1..=8).contains(&cfg.channels) {
            bail!("fake device {:?}: {} channels is out of range", cfg.uid, cfg.channels);
        }
        if cfg.period() == 0 || cfg.period() > cfg.rate {
            bail!("fake device {:?}: period of {} frames is out of range", cfg.uid, cfg.period());
        }
        if !cfg.ppm.is_finite() || cfg.ppm.abs() > 100_000.0 {
            bail!("fake device {:?}: ppm {} is out of range", cfg.uid, cfg.ppm);
        }
        let source = match (&cfg.kind, &cfg.source) {
            (FakeKind::Output, Some(_)) => bail!("fake device {:?}: an output device has no source", cfg.uid),
            (FakeKind::Output, None) | (FakeKind::Input, None) => SourceData::Silence,
            (FakeKind::Input, Some(s)) => match (&s.sine, &s.file, s.silence) {
                (None, None, _) => SourceData::Silence,
                (Some(sine), None, false) => {
                    if !(sine.freq.is_finite() && sine.freq > 0.0 && sine.freq < cfg.rate as f64 / 2.0) {
                        bail!("fake device {:?}: sine frequency {} is out of range", cfg.uid, sine.freq);
                    }
                    SourceData::Sine { freq: sine.freq, amp: sine.amp.clamp(0.0, 1.0) }
                }
                (None, Some(path), false) => {
                    let path = match dir {
                        Some(d) if path.is_relative() => d.join(path),
                        _ => path.clone(),
                    };
                    let (rate, channels, samples) = read_wav(&path)?;
                    if rate != cfg.rate {
                        bail!(
                            "fake device {:?}: {} is {rate} Hz but the device runs at {} Hz",
                            cfg.uid,
                            path.display(),
                            cfg.rate
                        );
                    }
                    SourceData::File { samples: Arc::new(samples), channels, looped: s.looped }
                }
                _ => bail!("fake device {:?}: a source names exactly one of sine/file/silence", cfg.uid),
            },
        };
        let live = Arc::new(DevLive { ppm_bits: AtomicU64::new(cfg.ppm.to_bits()), stall_until_ms: AtomicU64::new(0) });
        let sink = Arc::new(Mutex::new(Sink {
            gen: 0,
            rate: cfg.rate,
            frames_total: 0,
            gen_frames: 0,
            history: VecDeque::new(),
        }));
        Ok(Dev {
            volume: cfg.volume.clamp(0.0, 1.0),
            muted: cfg.muted,
            cfg,
            present: true,
            source,
            live,
            fail_next_open: 0,
            opens: 0,
            sink,
        })
    }
}

// ---------------------------------------------------------- stream handles

/// Owns a fake stream's pseudo-callback thread. Dropping it stops and joins
/// the thread, so (as with a dropped cpal stream) no callback runs after the
/// owner let go.
pub struct StreamGuard {
    world: Arc<World>,
    id: u64,
    uid: String,
    ctl: Arc<StreamCtl>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for StreamGuard {
    fn drop(&mut self) {
        self.ctl.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        self.world.remove_stream(self.id);
        self.world.log_event("stream_closed", &self.uid, &format!("stream={}", self.id));
    }
}

/// Sleeps until the next pseudo-callback is due. `false` = stop.
fn pace(next: &mut Instant, period: usize, rate: u32, live: &DevLive, ctl: &StreamCtl, health: &StreamHealth) -> bool {
    let ppm = f64::from_bits(live.ppm_bits.load(Ordering::Relaxed));
    let rate = rate as f64 * (1.0 + ppm * 1e-6);
    *next += Duration::from_secs_f64(period as f64 / rate.max(1.0));
    let now = Instant::now();
    if *next > now {
        std::thread::sleep(*next - now);
    } else if now - *next > MAX_CATCH_UP {
        *next = now;
    }
    !ctl.stop.load(Ordering::SeqCst) && health.is_alive()
}

fn stalled(live: &DevLive, started: Instant) -> bool {
    let until = live.stall_until_ms.load(Ordering::Relaxed);
    until != 0 && (started.elapsed().as_millis() as u64) < until
}

struct OutputJob {
    cons: HeapCons<f32>,
    dev: Arc<DeviceClock>,
    health: Arc<StreamHealth>,
    ctl: Arc<StreamCtl>,
    live: Arc<DevLive>,
    sink: Arc<Mutex<Sink>>,
    wav: Option<WavOut>,
    rate: u32,
    src_channels: usize,
    dev_channels: usize,
    period: usize,
    latency: Duration,
    world_started: Instant,
}

impl OutputJob {
    fn run(mut self) {
        let mut source = vec![0.0f32; self.period * self.src_channels];
        let mut device = vec![0.0f32; self.period * self.dev_channels];
        let mut next = Instant::now();
        while pace(&mut next, self.period, self.rate, &self.live, &self.ctl, &self.health) {
            if stalled(&self.live, self.world_started) {
                continue;
            }
            // Mirrors the real output callback in `LivePlayback::on_device`.
            {
                let _phase = self.dev.begin_output_callback();
                let got = self.cons.pop_slice(&mut source);
                for m in &mut source[got..] {
                    *m = 0.0; // underrun -> silence
                }
                self.dev.note_callback_with_native(
                    Instant::now(),
                    self.period,
                    got / self.src_channels,
                    Some(self.latency),
                    None,
                );
            }
            for (dst, src) in device.chunks_mut(self.dev_channels).zip(source.chunks_exact(self.src_channels)) {
                map_interleaved_frame(src, dst);
            }
            {
                let mut s = lock(&self.sink);
                let cap = self.rate as usize * SINK_HISTORY_SECS;
                for frame in source.chunks_exact(self.src_channels) {
                    let mono = frame.iter().sum::<f32>() / self.src_channels as f32;
                    while s.history.len() >= cap {
                        s.history.pop_front();
                    }
                    s.history.push_back(mono);
                }
                s.frames_total += self.period as u64;
                s.gen_frames += self.period as u64;
            }
            if let Some(w) = self.wav.as_mut() {
                if let Err(e) = w.write(&device) {
                    eprintln!("[audiohub] fake audio: sink write failed: {e:#}");
                    self.wav = None;
                }
            }
        }
        if let Some(w) = self.wav.take() {
            if let Err(e) = w.finish() {
                eprintln!("[audiohub] fake audio: sink finalize failed: {e:#}");
            }
        }
    }
}

struct InputJob {
    prod: HeapProd<f32>,
    dropped: Arc<AtomicU64>,
    health: Arc<StreamHealth>,
    ctl: Arc<StreamCtl>,
    live: Arc<DevLive>,
    source: SourceData,
    rate: u32,
    channels: usize,
    period: usize,
    world_started: Instant,
}

impl InputJob {
    fn run(mut self) {
        let mut block = vec![0.0f32; self.period * self.channels];
        let mut mono: Vec<f32> = Vec::new();
        let mut phase = 0.0f64;
        let mut pos = 0usize;
        let mut next = Instant::now();
        while pace(&mut next, self.period, self.rate, &self.live, &self.ctl, &self.health) {
            if stalled(&self.live, self.world_started) {
                continue;
            }
            self.fill(&mut block, &mut phase, &mut pos);
            // Same fold and same ring write as the real capture callback.
            fold_to_mono_f32(&block, self.channels, &mut mono);
            let wrote = self.prod.push_slice(&mono);
            if wrote < mono.len() {
                self.dropped.fetch_add((mono.len() - wrote) as u64, Ordering::Relaxed);
            }
        }
    }

    fn fill(&self, block: &mut [f32], phase: &mut f64, pos: &mut usize) {
        let ch = self.channels;
        match &self.source {
            SourceData::Silence => block.fill(0.0),
            SourceData::Sine { freq, amp } => {
                let step = TAU * freq / self.rate as f64;
                for frame in block.chunks_mut(ch) {
                    frame.fill(*amp * phase.sin() as f32);
                    *phase += step;
                    if *phase >= TAU {
                        *phase -= TAU;
                    }
                }
            }
            SourceData::File { samples, channels, looped } => {
                let fc = (*channels).max(1) as usize;
                let frames = samples.len() / fc;
                for frame in block.chunks_mut(ch) {
                    if frames == 0 || (*pos >= frames && !*looped) {
                        frame.fill(0.0);
                        continue;
                    }
                    if *pos >= frames {
                        *pos = 0;
                    }
                    let base = *pos * fc;
                    for (c, v) in frame.iter_mut().enumerate() {
                        *v = samples[base + c.min(fc - 1)];
                    }
                    *pos += 1;
                }
            }
        }
    }
}

// ------------------------------------------------------------------- WAV

/// Streaming float32 WAV writer; sizes are patched in `finish`.
struct WavOut {
    w: BufWriter<File>,
    data_bytes: u64,
}

impl WavOut {
    fn create(path: &Path, rate: u32, channels: u16) -> Result<WavOut> {
        // Never overwrite: each generation gets its own file.
        let f = OpenOptions::new().write(true).create_new(true).open(path)?;
        let mut w = BufWriter::new(f);
        let block_align = channels as u32 * 4;
        w.write_all(b"RIFF")?;
        w.write_all(&0u32.to_le_bytes())?;
        w.write_all(b"WAVEfmt ")?;
        w.write_all(&16u32.to_le_bytes())?;
        w.write_all(&3u16.to_le_bytes())?; // WAVE_FORMAT_IEEE_FLOAT
        w.write_all(&channels.to_le_bytes())?;
        w.write_all(&rate.to_le_bytes())?;
        w.write_all(&(rate * block_align).to_le_bytes())?;
        w.write_all(&(block_align as u16).to_le_bytes())?;
        w.write_all(&32u16.to_le_bytes())?;
        w.write_all(b"data")?;
        w.write_all(&0u32.to_le_bytes())?;
        Ok(WavOut { w, data_bytes: 0 })
    }

    fn write(&mut self, samples: &[f32]) -> Result<()> {
        for s in samples {
            self.w.write_all(&s.to_le_bytes())?;
        }
        self.data_bytes += samples.len() as u64 * 4;
        Ok(())
    }

    fn finish(mut self) -> Result<()> {
        let data = self.data_bytes.min(u32::MAX as u64 - 36) as u32;
        self.w.flush()?;
        let f = self.w.get_mut();
        f.seek(SeekFrom::Start(4))?;
        f.write_all(&(36 + data).to_le_bytes())?;
        f.seek(SeekFrom::Start(40))?;
        f.write_all(&data.to_le_bytes())?;
        f.flush()?;
        Ok(())
    }
}

/// Reads a PCM i16 or IEEE float32 WAV file: `(rate, channels, interleaved)`.
fn read_wav(path: &Path) -> Result<(u32, u16, Vec<f32>)> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let bad = || anyhow!("{} is not a PCM16/float32 WAV file", path.display());
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(bad());
    }
    let u16_at = |i: usize| u16::from_le_bytes([bytes[i], bytes[i + 1]]);
    let u32_at = |i: usize| u32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
    let mut fmt: Option<(u16, u16, u32, u16)> = None;
    let mut i = 12;
    while i + 8 <= bytes.len() {
        let id = &bytes[i..i + 4];
        let size = u32_at(i + 4) as usize;
        let body = i + 8;
        let end = body.checked_add(size).filter(|&e| e <= bytes.len()).ok_or_else(bad)?;
        if id == b"fmt " && size >= 16 {
            fmt = Some((u16_at(body), u16_at(body + 2), u32_at(body + 4), u16_at(body + 14)));
        } else if id == b"data" {
            let (format, channels, rate, bits) = fmt.ok_or_else(bad)?;
            let data = &bytes[body..end];
            let samples: Vec<f32> = match (format, bits) {
                (1, 16) => data.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0).collect(),
                (3, 32) => data.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect(),
                _ => return Err(bad()),
            };
            if channels == 0 || rate == 0 {
                return Err(bad());
            }
            return Ok((rate, channels, samples));
        }
        i = end + (size & 1);
    }
    Err(bad())
}

fn file_safe(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tone_fit_finds_a_clean_tone_and_rejects_silence() {
        let rate = 48_000;
        let x: Vec<f32> = (0..4800).map(|i| 0.3 * (TAU * 1000.0 * i as f64 / rate as f64).sin() as f32).collect();
        let fit = fit_tone(&x, rate, 1000.0);
        assert!(fit.detected && fit.snr_db > 60.0 && (fit.amp - 0.3).abs() < 0.01, "{fit:?}");
        let silence = vec![0.0f32; 4800];
        assert!(!fit_tone(&silence, rate, 1000.0).detected);
    }

    #[test]
    fn wav_round_trip() {
        let dir = std::env::temp_dir().join(format!("ahfake-wav-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.wav");
        let _ = std::fs::remove_file(&path);
        let mut w = WavOut::create(&path, 44_100, 2).unwrap();
        w.write(&[0.5, -0.5, 0.25, -0.25]).unwrap();
        w.finish().unwrap();
        let (rate, ch, s) = read_wav(&path).unwrap();
        assert_eq!((rate, ch, s), (44_100, 2, vec![0.5, -0.5, 0.25, -0.25]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn world_description_parses_the_documented_shape() {
        let cfg: FakeWorldCfg = serde_json::from_str(
            r#"{"schema":"ahr.fake.v1","devices":[
                {"uid":"fake-mic-1","kind":"input","rate":48000,"channels":2,"period_frames":480,
                 "source":{"sine":{"freq":1000,"amp":0.25}}},
                {"uid":"fake-spk-1","kind":"output","rate":48000,"channels":2,"latency_ms":10}],
               "default_input":"fake-mic-1","default_output":"fake-spk-1"}"#,
        )
        .unwrap();
        assert_eq!(cfg.devices.len(), 2);
        let inject: FakeInject = serde_json::from_str(r#"{"action":"kill_stream","uid":"fake-mic-1"}"#).unwrap();
        assert_eq!(inject.name(), "kill_stream");
    }
}
