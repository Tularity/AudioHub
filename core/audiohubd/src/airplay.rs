//! Daemon-owned AirPlay receiver lifecycle, local playback and volume glue.
//!
//! The protocol crate intentionally stops at "network in, full-scale PCM and
//! events out". This module owns the product decisions around it:
//!
//! - the stored switch is a wish; the listener only runs while this daemon is
//!   in Share mode;
//! - production starts only the AudioHub-owned AirPlay 2 listener, reusing its
//!   persisted control port when available, and advertises exactly the TXT
//!   records produced by that implementation;
//! - the daemon always plays incoming AirPlay on this machine; [`PcmReader`]
//!   keeps the realtime mixer decoupled from the protocol thread;
//! - AirPlay volume never scales PCM. Its linear `-30..=0 dB` slider protocol
//!   maps to the default output's `0..=1` system slider (with readback), while
//!   `-144 dB` maps to system slider zero plus the explicit mute control, so a
//!   sender whose zero-percent endpoint uses the mute sentinel still directly
//!   synchronizes the visible system slider;
//! - native output changes are sent back over the authenticated AirPlay 2
//!   event channel as an exact `0..=1` slider plus an independent mute flag;
//!   authenticated DACP remains a compatibility fallback for older senders.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use audiohub_airplay::{
    is_control_port_bind_error, AirPlayConfig, AirPlayEvent, AirPlayRuntime, MdnsService, PcmBus,
    PcmRead, PcmReader, Protocol, ReceiverVolumeControlSnapshot, RemoteControlInfo,
    RemoteControlSnapshot, RuntimePhase, SenderVolumeSnapshot, SessionInfo as RuntimeSessionInfo,
};
use audiohub_ipc::AirPlaySessionInfo;
use mdns_sd::{DaemonEvent, IfKind, Receiver, ServiceDaemon, ServiceEvent, ServiceInfo};

use crate::{dlog, lk, DaemonInner};

const MDNS_CONFIRM_TIMEOUT: Duration = Duration::from_secs(3);
const DNS_LABEL_MAX_BYTES: usize = 63;
const AIRPLAY_PICKER_NAME_MAX_BYTES: usize = DNS_LABEL_MAX_BYTES;
const VOLUME_RETRY_LIMIT: u8 = 3;
const DACP_VOLUME_RETRY_LIMIT: u8 = 3;
const DACP_SERVICE_TYPE: &str = "_dacp._tcp.local.";
const DACP_IO_TIMEOUT: Duration = Duration::from_millis(700);
const DACP_GUARD_POLL: Duration = Duration::from_millis(100);
const DACP_RESPONSE_LIMIT: usize = 8 * 1024;
// DACP serializes device-volume with six fractional decimal places. Allow the
// f32 parse/format round trip, but never use the native-device 0.75 dB settle
// tolerance here: a sender's nearby slider step is a distinct command.
const DACP_ECHO_DB_EPSILON: f32 = 0.000_01;
// Some DACP servers return HTTP 204 before their asynchronously scheduled
// player command reflects through RTSP. Keep only a short, bounded history of
// successful writes so those late echoes cannot roll a newer local change back.
const DACP_COMPLETED_ECHO_GRACE: Duration = Duration::from_secs(3);
const DACP_COMPLETED_ECHO_LIMIT: usize = 32;
const DACP_SERVICE_CACHE_LIMIT: usize = 64;
const DACP_BROWSE_POLL: Duration = Duration::from_millis(100);
const AIRPLAY_VOLUME_MIN_DB: f32 = -30.0;
const AIRPLAY_VOLUME_MAX_DB: f32 = 0.0;
const AIRPLAY_VOLUME_MUTE_DB: f32 = -144.0;
// Programmatic scalar writes were measured to read back within 1e-6 on both
// supported platforms. Keep this below a visible one-percent slider move: a
// local 1% change is a real command, not sender-write echo.
const SYSTEM_VOLUME_SCALAR_EPSILON: f32 = 0.000_1;
const MODE_REASON: &str = "AirPlay 接收仅在共享模式运行；切回共享模式后会按已保存的开关自动恢复";
const AIRPLAY2_PORT_FILE: &str = "airplay2-port";
const AIRPLAY2_PORT_FILE_MAX_BYTES: u64 = 16;
const AIRPLAY2_PORT_TMP_ATTEMPTS: usize = 16;
static AIRPLAY2_PORT_TMP_SEQ: AtomicU64 = AtomicU64::new(1);

fn load_airplay2_port(path: &Path) -> io::Result<Option<u16>> {
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if file.metadata()?.len() > AIRPLAY2_PORT_FILE_MAX_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "persisted AirPlay control port is oversized",
        ));
    }

    let mut encoded = String::new();
    file.read_to_string(&mut encoded)?;
    let encoded = encoded.strip_suffix('\n').unwrap_or(&encoded);
    if encoded.is_empty() || !encoded.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "persisted AirPlay control port is not a decimal integer",
        ));
    }
    let port = encoded.parse::<u16>().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "persisted AirPlay control port is outside the u16 range",
        )
    })?;
    if port == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "persisted AirPlay control port must be nonzero",
        ));
    }
    Ok(Some(port))
}

fn persist_airplay2_port(path: &Path, port: u16) -> io::Result<()> {
    persist_airplay2_port_with_sequences(path, port, || {
        AIRPLAY2_PORT_TMP_SEQ.fetch_add(1, Ordering::Relaxed)
    })
}

fn persist_airplay2_port_with_sequences(
    path: &Path,
    port: u16,
    mut next_sequence: impl FnMut() -> u64,
) -> io::Result<()> {
    if port == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "AirPlay control port must be nonzero",
        ));
    }
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "AirPlay control port path has no parent",
        )
    })?;
    std::fs::create_dir_all(parent)?;

    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    // A unique same-directory staging file keeps concurrent test daemons from
    // sharing a fixed `.tmp` name and keeps the final rename on one volume.
    // A crashed prior incarnation may leave the same PID/sequence name behind;
    // skip only create_new collisions and propagate every other I/O failure.
    let (tmp, mut file) = (0..AIRPLAY2_PORT_TMP_ATTEMPTS)
        .find_map(|_| {
            let sequence = next_sequence();
            let tmp = parent.join(format!(
                ".{AIRPLAY2_PORT_FILE}.{}.{}.tmp",
                std::process::id(),
                sequence
            ));
            match options.open(&tmp) {
                Ok(file) => Some(Ok((tmp, file))),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => None,
                Err(error) => Some(Err(error)),
            }
        })
        .transpose()?
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "cannot allocate a unique AirPlay control port staging file",
            )
        })?;
    let result = (|| -> io::Result<()> {
        file.write_all(format!("{port}\n").as_bytes())?;
        file.sync_all()?;
        drop(file);
        atomic_replace_file(&tmp, path)?;
        sync_parent_directory(parent)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(windows)]
fn atomic_replace_file(source: &Path, destination: &Path) -> io::Result<()> {
    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;
    #[link(name = "kernel32")]
    extern "system" {
        fn MoveFileExW(
            existing_file_name: *const u16,
            new_file_name: *const u16,
            flags: u32,
        ) -> i32;
    }

    let source = windows_verbatim_path(source)?;
    let destination = windows_verbatim_path(destination)?;
    // SAFETY: both path buffers are NUL-terminated and remain alive for the
    // call. The files are in the same directory, so this is an atomic replace
    // rather than a cross-volume copy.
    if unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn windows_verbatim_path(path: &Path) -> io::Result<Vec<u16>> {
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Component, Prefix};

    // `canonicalize` supplies the std path normalization and extended-length
    // prefix that raw Win32 calls do not. Resolve only the existing parent:
    // resolving the leaf would follow a destination reparse point and replace
    // its target instead of this state-file directory entry.
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Windows path has no parent"))?;
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "Windows path has no file name")
    })?;
    let normalized = std::fs::canonicalize(parent)?.join(name);

    // On Windows `canonicalize` returns an extended-length (`\\?\`) path.
    // Check that contract before handing the buffer to raw Win32 rather than
    // rebuilding it through a lossy UTF-8 intermediary.
    let verbatim = matches!(
        normalized.components().next(),
        Some(Component::Prefix(prefix))
            if matches!(
                prefix.kind(),
                Prefix::Verbatim(_) | Prefix::VerbatimDisk(_) | Prefix::VerbatimUNC(_, _)
            )
    );
    if !verbatim {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Windows AirPlay state path is not verbatim absolute",
        ));
    }
    let encoded: Vec<u16> = normalized.as_os_str().encode_wide().collect();
    if encoded.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Windows AirPlay state path contains NUL",
        ));
    }
    Ok(encoded.into_iter().chain(Some(0)).collect())
}

#[cfg(not(windows))]
fn atomic_replace_file(source: &Path, destination: &Path) -> io::Result<()> {
    std::fs::rename(source, destination)
}

#[cfg(unix)]
fn sync_parent_directory(parent: &Path) -> io::Result<()> {
    std::fs::File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent_directory(_parent: &Path) -> io::Result<()> {
    // MoveFileExW's WRITE_THROUGH flag provides the Windows durability edge.
    Ok(())
}

fn persisted_port_unavailable(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::AddrInUse
            | io::ErrorKind::AddrNotAvailable
            | io::ErrorKind::PermissionDenied
    )
}

fn is_control_port_bind_failure(error: &io::Error) -> bool {
    is_control_port_bind_error(error)
}

fn start_airplay_runtime(
    base_config: AirPlayConfig,
    port_path: &Path,
) -> io::Result<AirPlayRuntime> {
    start_airplay_runtime_after_port_load(base_config, port_path, load_airplay2_port(port_path))
}

fn start_airplay_runtime_after_port_load(
    base_config: AirPlayConfig,
    port_path: &Path,
    loaded_port: io::Result<Option<u16>>,
) -> io::Result<AirPlayRuntime> {
    let persisted_port = match loaded_port {
        Ok(port) => port,
        Err(error) if error.kind() == io::ErrorKind::InvalidData => {
            // The file contains no secret and its contents are deliberately
            // absent from this diagnostic. A corrupt record is repaired only
            // after a replacement listener has started successfully.
            dlog!("[audiohubd] ignoring invalid AirPlay control port record: {error}");
            None
        }
        Err(error) => {
            return Err(io::Error::new(
                error.kind(),
                format!("cannot read persisted AirPlay control port: {error}"),
            ))
        }
    };

    let mut requested = base_config.clone();
    requested.airplay2_port = persisted_port.unwrap_or(0);
    let runtime = match AirPlayRuntime::start(requested) {
        Ok(runtime) => runtime,
        Err(error)
            if persisted_port.is_some()
                && is_control_port_bind_failure(&error)
                && persisted_port_unavailable(&error) =>
        {
            dlog!(
                "[audiohubd] persisted AirPlay control port {} is unavailable; selecting a replacement",
                persisted_port.unwrap_or(0)
            );
            let mut fallback = base_config;
            fallback.airplay2_port = 0;
            AirPlayRuntime::start(fallback).map_err(|fallback_error| {
                io::Error::new(
                    fallback_error.kind(),
                    format!(
                        "AirPlay fallback listener failed after the persisted port was unavailable: {fallback_error}"
                    ),
                )
            })?
        }
        Err(error) => return Err(error),
    };

    let selected_port = runtime
        .status()
        .airplay2_port
        .filter(|port| *port != 0)
        .ok_or_else(|| io::Error::other("AirPlay runtime reported no control port"))?;
    if persisted_port != Some(selected_port) {
        if let Err(error) = persist_airplay2_port(port_path, selected_port) {
            let _ = runtime.stop();
            return Err(io::Error::new(
                error.kind(),
                format!("cannot persist AirPlay control port: {error}"),
            ));
        }
    }
    Ok(runtime)
}

struct AirPlayNetworkLogger;

impl log::Log for AirPlayNetworkLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.target().starts_with("audiohub_airplay") && metadata.level() <= log::Level::Info
    }

    fn log(&self, record: &log::Record<'_>) {
        if self.enabled(record.metadata()) {
            crate::logln(format_args!(
                "[audiohubd] AirPlay network {}: {}",
                record.level(),
                record.args()
            ));
        }
    }

    fn flush(&self) {}
}

static AIRPLAY_NETWORK_LOGGER: AirPlayNetworkLogger = AirPlayNetworkLogger;

/// Route bounded, redacted receiver-edge warnings into the daemon's safe log
/// writer. A logger may already be installed by a host test; in that case the
/// existing one wins.
pub(crate) fn init_logging() {
    if log::set_logger(&AIRPLAY_NETWORK_LOGGER).is_ok() {
        log::set_max_level(log::LevelFilter::Info);
    }
}

#[derive(Debug, Clone, Copy)]
struct SessionVolume {
    session_id: u64,
    db: f32,
}

impl PartialEq for SessionVolume {
    fn eq(&self, other: &Self) -> bool {
        self.session_id == other.session_id && self.db.to_bits() == other.db.to_bits()
    }
}

impl Eq for SessionVolume {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct VolumeRetry {
    target: SessionVolume,
    attempts: u8,
}

#[derive(Debug, Clone, Copy)]
struct SystemVolumeState {
    scalar: f32,
    muted: bool,
}

impl PartialEq for SystemVolumeState {
    fn eq(&self, other: &Self) -> bool {
        self.scalar.to_bits() == other.scalar.to_bits() && self.muted == other.muted
    }
}

impl Eq for SystemVolumeState {}

impl SystemVolumeState {
    fn from_sender_db(db: f32) -> Self {
        let muted = db <= AIRPLAY_VOLUME_MUTE_DB;
        Self {
            scalar: if muted {
                0.0
            } else {
                airplay_db_to_system_scalar(db)
            },
            muted,
        }
    }

    fn airplay_db(self) -> f32 {
        if self.muted {
            AIRPLAY_VOLUME_MUTE_DB
        } else {
            system_scalar_to_airplay_db(self.scalar)
        }
    }
}

fn airplay_db_to_system_scalar(db: f32) -> f32 {
    (db.clamp(AIRPLAY_VOLUME_MIN_DB, AIRPLAY_VOLUME_MAX_DB) - AIRPLAY_VOLUME_MIN_DB)
        / (AIRPLAY_VOLUME_MAX_DB - AIRPLAY_VOLUME_MIN_DB)
}

fn system_scalar_to_airplay_db(scalar: f32) -> f32 {
    AIRPLAY_VOLUME_MIN_DB + scalar.clamp(0.0, 1.0) * (AIRPLAY_VOLUME_MAX_DB - AIRPLAY_VOLUME_MIN_DB)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RemoteRetry {
    desired: SystemVolumeState,
    attempts: u8,
}

const DACP_WRITE_PENDING: u8 = 0;
const DACP_WRITE_AUTHORIZED: u8 = 1;
const DACP_WRITE_FULL_REQUEST: u8 = 2;
const DACP_WRITE_CANCELLED: u8 = 3;

#[derive(Debug, Clone)]
struct DacpWriteGate {
    phase: Arc<AtomicU8>,
}

impl DacpWriteGate {
    fn new() -> Self {
        Self {
            phase: Arc::new(AtomicU8::new(DACP_WRITE_PENDING)),
        }
    }

    /// Commit the worker to a possibly visible request. Cancellation and
    /// commitment race through one CAS, so the controller can distinguish a
    /// guaranteed zero-byte supersession from a write that may reach AirPlay.
    fn commit(&self, still_current: impl FnOnce() -> bool) -> bool {
        still_current()
            && self
                .phase
                .compare_exchange(
                    DACP_WRITE_PENDING,
                    DACP_WRITE_AUTHORIZED,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
    }

    /// Returns true when the worker won the race and the old request must be
    /// treated as possibly visible. False means the worker can no longer pass
    /// its pre-write gate and therefore cannot create an RTSP echo.
    fn cancel(&self) -> bool {
        match self.phase.compare_exchange(
            DACP_WRITE_PENDING,
            DACP_WRITE_CANCELLED,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => false,
            Err(phase) => matches!(phase, DACP_WRITE_AUTHORIZED | DACP_WRITE_FULL_REQUEST),
        }
    }

    fn mark_full_request_written(&self) {
        let _ = self
            .phase
            .fetch_update(Ordering::Release, Ordering::Acquire, |phase| {
                matches!(phase, DACP_WRITE_PENDING | DACP_WRITE_AUTHORIZED)
                    .then_some(DACP_WRITE_FULL_REQUEST)
            });
    }

    /// Re-arm the same logical flight after an event-channel attempt proved
    /// that no bytes were written. This is used only for the authenticated
    /// DACP fallback, and only while every session/output lease is still
    /// current. A concurrent invalidation wins through `still_current` before
    /// the fallback can commit its own write.
    fn retry_after_unwritten(&self, still_current: impl FnOnce() -> bool) -> bool {
        still_current()
            && self
                .phase
                .compare_exchange(
                    DACP_WRITE_AUTHORIZED,
                    DACP_WRITE_PENDING,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
    }

    fn may_have_written(&self) -> bool {
        self.phase.load(Ordering::Acquire) == DACP_WRITE_FULL_REQUEST
    }

    fn write_authorized_but_unresolved(&self) -> bool {
        self.phase.load(Ordering::Acquire) == DACP_WRITE_AUTHORIZED
    }
}

#[derive(Debug, Clone)]
struct RemoteInFlight {
    serial: u64,
    desired: SystemVolumeState,
    db: f32,
    attempt: u8,
    echo_confirmed: bool,
    sender_revision_at_queue: Option<u64>,
    write_gate: DacpWriteGate,
}

#[derive(Debug, Clone)]
struct RemoteCompletion {
    flight: RemoteInFlight,
    delivered: bool,
}

#[derive(Debug, Clone)]
struct CompletedRemoteEcho {
    serial: u64,
    session_id: u64,
    db: f32,
    desired: SystemVolumeState,
    write_gate: DacpWriteGate,
    expected_revision: u64,
    expires_at: Instant,
}

#[derive(Clone)]
struct OutputEpochLease {
    expected: u64,
    current: Arc<AtomicU64>,
}

impl OutputEpochLease {
    fn new(expected: u64, current: &Arc<AtomicU64>) -> Self {
        Self {
            expected,
            current: Arc::clone(current),
        }
    }

    fn is_current(&self) -> bool {
        self.current.load(Ordering::Acquire) == self.expected
    }

    fn observed(&self) -> u64 {
        self.current.load(Ordering::Acquire)
    }
}

#[derive(Clone)]
struct DacpEndpointSnapshot {
    address: SocketAddr,
    revision: u64,
    current_revision: Arc<AtomicU64>,
}

impl DacpEndpointSnapshot {
    fn is_current(&self) -> bool {
        self.current_revision.load(Ordering::Acquire) == self.revision
    }
}

struct DacpJob {
    serial: u64,
    generation: u64,
    session_id: u64,
    endpoint: Option<DacpEndpointSnapshot>,
    remote_control: Option<RemoteControlSnapshot>,
    event_control: Option<ReceiverVolumeControlSnapshot>,
    output_epoch: OutputEpochLease,
    db: f32,
    scalar: f32,
    muted: bool,
    write_gate: DacpWriteGate,
}

struct DacpOutcome {
    serial: u64,
    generation: u64,
    session_id: u64,
    db: f32,
    write_gate: DacpWriteGate,
    /// Carries the exact revocable lease used by the worker so a refresh
    /// after worker classification but before ticker drain is still noticed.
    remote_control: Option<RemoteControlSnapshot>,
    event_control: Option<ReceiverVolumeControlSnapshot>,
    /// The native value belongs to exactly one default-output epoch. Keep the
    /// lease through outcome consumption so a replacement can never count an
    /// old read as sender convergence.
    output_epoch: OutputEpochLease,
    /// Endpoint is independently mutable through mDNS even when DACP-ID and
    /// Active-Remote remain unchanged. Re-resolve it before accepting 2xx.
    endpoint: Option<DacpEndpointSnapshot>,
    route: ReverseVolumeRoute,
    result: DacpOutcomeResult,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReverseVolumeRoute {
    Event,
    Dacp,
}

enum DacpOutcomeResult {
    Sent(std::result::Result<(), String>),
    Superseded,
    /// The attempt may already have written the old bearer request before a
    /// credential/session revision revoked it. Never count this as
    /// convergence; reassert through the newest snapshot.
    RevokedAfterPossibleWrite,
}

impl DacpOutcomeResult {
    fn may_have_written_request(&self, write_gate: &DacpWriteGate) -> bool {
        matches!(self, Self::Sent(Ok(_)) | Self::RevokedAfterPossibleWrite)
            || matches!(self, Self::Sent(Err(_))) && write_gate.may_have_written()
    }
}

struct DacpWorker {
    jobs: Option<mpsc::SyncSender<DacpJob>>,
    outcomes: Mutex<mpsc::Receiver<DacpOutcome>>,
    current_serial: Arc<AtomicU64>,
    thread: Option<JoinHandle<()>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DacpTrySendError {
    Full,
    Disconnected,
}

impl DacpWorker {
    fn new() -> Self {
        let (job_tx, job_rx) = mpsc::sync_channel::<DacpJob>(1);
        let (outcome_tx, outcome_rx) = mpsc::channel();
        let current_serial = Arc::new(AtomicU64::new(0));
        let worker_serial = Arc::clone(&current_serial);
        let thread = thread::Builder::new()
            .name("ahb-airplay-volume".to_string())
            .spawn(move || {
                while let Ok(job) = job_rx.recv() {
                    let mut route = if event_job_authority_current(&job, &worker_serial) {
                        Some(ReverseVolumeRoute::Event)
                    } else if dacp_job_authority_current(&job, &worker_serial) {
                        Some(ReverseVolumeRoute::Dacp)
                    } else {
                        None
                    };
                    let result = if route.is_none() {
                        DacpOutcomeResult::Superseded
                    } else {
                        let attempt: Result<bool> = match route {
                            Some(ReverseVolumeRoute::Event) => {
                                let event = job
                                    .event_control
                                    .as_ref()
                                    .expect("event route has a control lease");
                                if !job.write_gate.commit(|| {
                                    event_job_authority_current(&job, &worker_serial)
                                }) {
                                    Ok(false)
                                } else {
                                    match event.send_device_volume(job.scalar, job.muted) {
                                        Ok(()) => {
                                            job.write_gate.mark_full_request_written();
                                            Ok(true)
                                        }
                                        Err(event_error) if event_error.may_have_written() => {
                                            // The sender can already have applied this value even
                                            // when its RTSP acknowledgement is lost. Preserve the
                                            // existing echo/tombstone semantics and never double-send
                                            // through DACP in this ambiguous state.
                                            job.write_gate.mark_full_request_written();
                                            Err(anyhow::Error::from(event_error))
                                        }
                                        Err(event_error)
                                            if job.write_gate.retry_after_unwritten(|| {
                                                dacp_job_authority_current(&job, &worker_serial)
                                            }) =>
                                        {
                                            route = Some(ReverseVolumeRoute::Dacp);
                                            let endpoint = job
                                                .endpoint
                                                .as_ref()
                                                .expect("DACP fallback has an endpoint");
                                            let remote = job
                                                .remote_control
                                                .as_ref()
                                                .expect("DACP fallback has credentials");
                                            send_dacp_volume_guarded(
                                                endpoint.address,
                                                &remote.info().active_remote,
                                                job.db,
                                                &job.write_gate,
                                                || {
                                                    dacp_job_authority_current(
                                                        &job,
                                                        &worker_serial,
                                                    )
                                                },
                                            )
                                            .with_context(|| {
                                                format!(
                                                    "AP2 event command was not written ({event_error}); DACP fallback failed"
                                                )
                                            })
                                        }
                                        Err(event_error) => Err(anyhow::Error::from(event_error)),
                                    }
                                }
                            }
                            Some(ReverseVolumeRoute::Dacp) => {
                                let endpoint = job
                                    .endpoint
                                    .as_ref()
                                    .expect("DACP route has an endpoint");
                                let remote = job
                                    .remote_control
                                    .as_ref()
                                    .expect("DACP route has credentials");
                                send_dacp_volume_guarded(
                                    endpoint.address,
                                    &remote.info().active_remote,
                                    job.db,
                                    &job.write_gate,
                                    || dacp_job_authority_current(&job, &worker_serial),
                                )
                            }
                            None => unreachable!("route absence was handled above"),
                        };
                        match attempt {
                            Ok(false) => DacpOutcomeResult::Superseded,
                            Ok(true)
                                if !reverse_job_authority_current(
                                    &job,
                                    &worker_serial,
                                    route.expect("attempt selected a route"),
                                ) =>
                            {
                                DacpOutcomeResult::RevokedAfterPossibleWrite
                            }
                            Err(_)
                                if !reverse_job_authority_current(
                                    &job,
                                    &worker_serial,
                                    route.expect("attempt selected a route"),
                                )
                                    && job.write_gate.may_have_written() =>
                            {
                                DacpOutcomeResult::RevokedAfterPossibleWrite
                            }
                            Err(_)
                                if !reverse_job_authority_current(
                                    &job,
                                    &worker_serial,
                                    route.expect("attempt selected a route"),
                                ) =>
                            {
                                DacpOutcomeResult::Superseded
                            }
                            Ok(true) => DacpOutcomeResult::Sent(Ok(())),
                            Err(error) => DacpOutcomeResult::Sent(Err(format!("{error:#}"))),
                        }
                    };
                    if outcome_tx
                        .send(DacpOutcome {
                            serial: job.serial,
                            generation: job.generation,
                            session_id: job.session_id,
                            db: job.db,
                            write_gate: job.write_gate,
                            remote_control: job.remote_control,
                            output_epoch: job.output_epoch,
                            endpoint: job.endpoint,
                            event_control: job.event_control,
                            route: route.unwrap_or(ReverseVolumeRoute::Event),
                            result,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .expect("spawn AirPlay reverse-volume worker");
        Self {
            jobs: Some(job_tx),
            outcomes: Mutex::new(outcome_rx),
            current_serial,
            thread: Some(thread),
        }
    }

    fn try_send(&self, job: DacpJob) -> std::result::Result<(), DacpTrySendError> {
        let serial = job.serial;
        self.current_serial.store(serial, Ordering::Release);
        let result = self
            .jobs
            .as_ref()
            .expect("DACP worker sender exists until drop")
            .try_send(job)
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => DacpTrySendError::Full,
                mpsc::TrySendError::Disconnected(_) => DacpTrySendError::Disconnected,
            });
        if result.is_err() {
            let _ = self.current_serial.compare_exchange(
                serial,
                0,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        }
        result
    }

    fn invalidate(&self) {
        self.current_serial.store(0, Ordering::Release);
    }

    fn drain_outcomes(&self) -> Vec<DacpOutcome> {
        lk(&self.outcomes).try_iter().collect()
    }
}

fn reverse_job_base_authority_current(job: &DacpJob, current_serial: &AtomicU64) -> bool {
    current_serial.load(Ordering::Acquire) == job.serial && job.output_epoch.is_current()
}

fn event_job_authority_current(job: &DacpJob, current_serial: &AtomicU64) -> bool {
    reverse_job_base_authority_current(job, current_serial)
        && job
            .event_control
            .as_ref()
            .is_some_and(ReceiverVolumeControlSnapshot::is_current)
}

fn dacp_job_authority_current(job: &DacpJob, current_serial: &AtomicU64) -> bool {
    reverse_job_base_authority_current(job, current_serial)
        && job
            .remote_control
            .as_ref()
            .is_some_and(RemoteControlSnapshot::is_current)
        && job
            .endpoint
            .as_ref()
            .is_some_and(DacpEndpointSnapshot::is_current)
}

fn reverse_job_authority_current(
    job: &DacpJob,
    current_serial: &AtomicU64,
    route: ReverseVolumeRoute,
) -> bool {
    match route {
        ReverseVolumeRoute::Event => event_job_authority_current(job, current_serial),
        ReverseVolumeRoute::Dacp => dacp_job_authority_current(job, current_serial),
    }
}

impl Drop for DacpWorker {
    fn drop(&mut self) {
        // Revoke the active and queued job before closing the channel. The
        // active response loop observes this within `DACP_GUARD_POLL`; the one
        // bounded queued item exits before opening a socket.
        self.current_serial.store(0, Ordering::Release);
        self.jobs.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[derive(Debug, Default)]
struct VolumeSync {
    target: Option<SessionVolume>,
    /// Last volume observed from RTSP/runtime status. `target` may advance to a
    /// native value after a successful DACP write; keeping this separately
    /// prevents the unchanged runtime snapshot from overwriting that value.
    sender_reported: Option<SessionVolume>,
    /// Lossless runtime command identity last consumed by the ticker. This is
    /// deliberately independent of dB: Y after a reverse-synchronized X is a
    /// new sender command even when Y equals the sender's previous value.
    sender_revision: Option<u64>,
    retry: Option<VolumeRetry>,
    /// What the sender is known to represent in its AirPlay destination UI.
    /// It is the sender command after inbound writes, and the native readback
    /// after a successful DACP update. A mismatch with the current system
    /// slider scalar is therefore a genuine local change (or one clamp correction).
    sender_known_system: Option<SystemVolumeState>,
    remote_retry: Option<RemoteRetry>,
    remote_inflight: Option<RemoteInFlight>,
    /// Successful DACP writes whose matching RTSP reflection may arrive after
    /// the HTTP outcome. Multiple entries are required while a slider is
    /// moving: an old X1 echo must not overwrite an already accepted X2.
    completed_remote_echoes: VecDeque<CompletedRemoteEcho>,
    /// Last non-secret runtime authority revision observed by the 200 ms
    /// controller ticker. A change forces one fresh-credential reassertion
    /// after the sender has established its initial volume.
    remote_revision: Option<(u64, u64)>,
    /// A superseded DACP write may have reached the sender after a newer
    /// command was already accepted. Reassert the current native value once so
    /// an acknowledged write or an uncertain post-write failure cannot leave a
    /// stale value visible in the sender UI.
    force_remote: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VolumeTickPlan {
    None,
    ClearedInactive,
    Apply {
        target: SessionVolume,
        retry_attempt: Option<u8>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SenderSnapshotDecision {
    Unchanged,
    DeferredRemoteOutcome,
    ReverseEcho {
        target: SessionVolume,
    },
    Apply {
        target: SessionVolume,
        replaced_session: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VolumeApplyReason {
    SenderEvent,
    StatusReconcile,
    OutputChanged,
    Retry(u8),
}

impl VolumeApplyReason {
    fn retry_attempt(self) -> Option<u8> {
        match self {
            Self::Retry(attempt) => Some(attempt),
            Self::SenderEvent | Self::StatusReconcile | Self::OutputChanged => None,
        }
    }
}

impl VolumeSync {
    fn remember(&mut self, target: SessionVolume) -> bool {
        let replaced_session = self
            .target
            .is_some_and(|old| old.session_id != target.session_id);
        self.target = Some(target);
        self.sender_reported = Some(target);
        self.retry = None;
        self.remote_retry = None;
        if let Some(flight) = self.remote_inflight.take() {
            let _ = flight.write_gate.cancel();
        }
        // Every new sender command temporarily disarms reverse observation.
        // A level write changes the slider and then unmutes; polling between
        // those calls must not treat the intermediate device state as an
        // independent local change. An explicit mute never changes the slider.
        self.sender_known_system = None;
        if replaced_session {
            self.completed_remote_echoes.clear();
            self.force_remote = false;
        }
        replaced_session
    }

    fn ended(&mut self, session_id: Option<u64>) -> bool {
        let Some(session_id) = session_id else {
            return false;
        };
        if !self
            .target
            .is_some_and(|target| target.session_id == session_id)
        {
            return false;
        }
        self.clear();
        true
    }

    fn clear(&mut self) {
        self.target = None;
        self.sender_reported = None;
        self.sender_revision = None;
        self.retry = None;
        self.sender_known_system = None;
        self.remote_retry = None;
        if let Some(flight) = self.remote_inflight.take() {
            let _ = flight.write_gate.cancel();
        }
        self.completed_remote_echoes.clear();
        self.remote_revision = None;
        self.force_remote = false;
    }

    fn is_current(&self, target: SessionVolume, active: Option<u64>) -> bool {
        self.target == Some(target) && active == Some(target.session_id)
    }

    fn remember_snapshot(&mut self, target: SessionVolume, revision: u64) -> bool {
        let replaced_session = self.remember(target);
        self.sender_revision = Some(revision);
        replaced_session
    }

    fn needs_sender_reconcile(&self, target: SessionVolume, revision: u64) -> bool {
        self.sender_reported != Some(target) || self.sender_revision != Some(revision)
    }

    fn reconcile_sender_snapshot(
        &mut self,
        authoritative: Option<(SessionVolume, u64)>,
    ) -> SenderSnapshotDecision {
        self.reconcile_sender_snapshot_at(authoritative, Instant::now())
    }

    fn reconcile_sender_snapshot_at(
        &mut self,
        authoritative: Option<(SessionVolume, u64)>,
        now: Instant,
    ) -> SenderSnapshotDecision {
        let Some((reported, revision)) = authoritative else {
            return SenderSnapshotDecision::Unchanged;
        };
        if !self.needs_sender_reconcile(reported, revision) {
            return SenderSnapshotDecision::Unchanged;
        }

        if let Some(target) = self.confirm_remote_echo_at(Some((reported, revision)), None, now) {
            return SenderSnapshotDecision::ReverseEcho { target };
        }
        if self.unresolved_remote_echo_matches(reported, revision, now) {
            // The worker won write authorization but has not yet proved that a
            // complete HTTP request left this process. Do not consume the
            // revision or perform native IO: a full-request transition makes
            // this an echo, while a zero/partial-write outcome removes the
            // provisional guard and leaves it as a genuine sender command.
            return SenderSnapshotDecision::DeferredRemoteOutcome;
        }

        if self
            .target
            .is_some_and(|target| target.session_id == reported.session_id)
        {
            // This revision is a genuine sender command (or an uncorrelatable
            // coalesced snapshot). A single next revision definitely occupies
            // one slot before any pending async echoes, so reserve that slot.
            // A larger jump can already contain an unobserved echo; rebasing in
            // that case would create a ghost tombstone capable of swallowing a
            // later genuine command. Drop this session's ambiguous guards and
            // prefer the sender snapshot instead.
            self.advance_completed_echoes_after_sender_command(reported.session_id, revision);
            self.cancel_remote_inflight_after_sender_command(reported.session_id, revision, now);
        }
        let replaced_session = self.remember_snapshot(reported, revision);
        SenderSnapshotDecision::Apply {
            target: reported,
            replaced_session,
        }
    }

    /// Consume a lossless RTSP snapshot only when it confirms a recent reverse
    /// DACP write. `required_serial` binds the outcome-drain fast path to the
    /// exact in-flight worker result. Ordinary event/ticker reconciliation
    /// passes `None` and may also consume one bounded post-outcome echo.
    fn confirm_remote_echo(
        &mut self,
        authoritative: Option<(SessionVolume, u64)>,
        required_serial: Option<u64>,
    ) -> Option<SessionVolume> {
        self.confirm_remote_echo_at(authoritative, required_serial, Instant::now())
    }

    fn confirm_remote_echo_at(
        &mut self,
        authoritative: Option<(SessionVolume, u64)>,
        required_serial: Option<u64>,
        now: Instant,
    ) -> Option<SessionVolume> {
        let (reported, revision) = authoritative?;
        if !self.needs_sender_reconcile(reported, revision) {
            return None;
        }

        self.prune_completed_remote_echoes(now);

        // A server may acknowledge HTTP first and reflect the player command
        // through RTSP later. Consume the matching completed write without
        // touching newer receiver authority or an in-flight X2. Reflections
        // are allowed to arrive out of DACP request order: when a later guard's
        // value occupies the very next sender revision, consume that guard and
        // move only its older siblings one slot forward. A later sibling keeps
        // its original slot. This prevents a reordered X2/X1 pair from leaving
        // a ghost X2 guard that could swallow a genuine command afterwards.
        //
        // The pre-send revision and short expiry make this a bounded response
        // correlation, not a permanent same-value exemption. RTSP does not
        // return our DACP serial, so a genuine command with the exact bounded
        // revision and dB is information-theoretically indistinguishable; the
        // one-shot slot and TTL strictly bound that ambiguity.
        if required_serial.is_none() {
            let next_revision =
                self.sender_revision.and_then(|seen| seen.checked_add(1)) == Some(revision);
            let exact = next_revision.then(|| {
                self.completed_remote_echoes.iter().position(|echo| {
                    echo.session_id == reported.session_id
                        && echo.write_gate.may_have_written()
                        && echo.expected_revision == revision
                        && same_dacp_echo_volume(reported.db, echo.db)
                })
            });
            let exact = exact.flatten();
            let reordered = exact.is_none() && next_revision;
            let completed = exact.map(|index| (index, false)).or_else(|| {
                if !reordered {
                    return None;
                }
                self.completed_remote_echoes
                    .iter()
                    .position(|echo| {
                        echo.session_id == reported.session_id
                            && echo.write_gate.may_have_written()
                            && echo.expected_revision > revision
                            && same_dacp_echo_volume(reported.db, echo.db)
                    })
                    .map(|index| (index, true))
            });
            if let Some((index, arrived_early)) = completed {
                let echo = self
                    .completed_remote_echoes
                    .remove(index)
                    .expect("completed echo index came from the same queue");
                if arrived_early {
                    for pending in &mut self.completed_remote_echoes {
                        if pending.session_id == echo.session_id && pending.serial < echo.serial {
                            pending.expected_revision = pending.expected_revision.saturating_add(1);
                        }
                    }
                }
                let newer_authority = self.target.is_some_and(|target| {
                    target.session_id == reported.session_id
                        && !same_dacp_echo_volume(target.db, echo.db)
                }) || self
                    .remote_inflight
                    .as_ref()
                    .is_some_and(|flight| !same_dacp_echo_volume(flight.db, echo.db));
                let stronger_force = self.force_remote;
                self.sender_reported = Some(reported);
                self.sender_revision = Some(revision);
                if !newer_authority {
                    // A response failure after a complete request leaves this
                    // guard awaiting proof. Its echo promotes the same current
                    // native intent to known sender state and ends that retry.
                    self.sender_known_system = Some(echo.desired);
                    self.remote_retry = None;
                    if let Some(flight) = self.remote_inflight.as_mut() {
                        if !stronger_force && same_dacp_echo_volume(flight.db, echo.db) {
                            flight.echo_confirmed = true;
                        }
                    }
                }
                // This tombstone proves only that one completed DACP value
                // reflected. It must not clear a stronger correction armed by
                // an output/credential/endpoint change or another uncertain
                // write.
                self.force_remote |= newer_authority;
                return Some(reported);
            }
        }

        // A DACP receiver can install its reflected RTSP SET_PARAMETER before
        // our control ticker drains the HTTP outcome, even when the bounded
        // event pump has not handled its wakeup yet. Consume that exact
        // snapshot before clearing the flight. Otherwise it becomes a false
        // sender command later in the same tick and can overwrite a newer
        // receiver-side slider or mute action.
        let next_revision = self.sender_revision.and_then(|seen| seen.checked_add(1));
        let mut flight = self
            .remote_inflight
            .as_ref()
            .filter(|flight| {
                required_serial.is_none_or(|serial| flight.serial == serial)
                    && self
                        .target
                        .is_some_and(|target| target.session_id == reported.session_id)
                    && next_revision == Some(revision)
                    && flight.write_gate.may_have_written()
                    && same_dacp_echo_volume(reported.db, flight.db)
            })?
            .clone();
        // An in-flight X2 may reflect before an older completed X1 even though
        // the HTTP requests themselves were serialized. X2 consumed the next
        // RTSP revision, so move only its older guards one slot forward.
        for pending in &mut self.completed_remote_echoes {
            if pending.session_id == reported.session_id
                && pending.serial < flight.serial
                && pending.expected_revision >= revision
            {
                pending.expected_revision = pending.expected_revision.saturating_add(1);
            }
        }
        flight.echo_confirmed = true;
        self.remote_inflight = Some(flight.clone());
        let synchronized = SessionVolume {
            session_id: reported.session_id,
            db: flight.db,
        };
        self.target = Some(synchronized);
        self.sender_reported = Some(reported);
        self.sender_revision = Some(revision);
        self.retry = None;
        self.sender_known_system = Some(flight.desired);
        // The RTSP echo itself proves that the sender reached this value. Keep
        // the flight for the worker outcome, but do not leave a retry or a
        // prior forced reassertion armed if the HTTP response fails.
        self.remote_retry = None;
        self.force_remote = false;
        Some(synchronized)
    }

    fn unresolved_remote_echo_matches(
        &mut self,
        reported: SessionVolume,
        revision: u64,
        now: Instant,
    ) -> bool {
        self.prune_completed_remote_echoes(now);
        self.sender_revision.and_then(|seen| seen.checked_add(1)) == Some(revision)
            && (self.completed_remote_echoes.iter().any(|echo| {
                echo.session_id == reported.session_id
                    && echo.write_gate.write_authorized_but_unresolved()
                    && echo.expected_revision >= revision
                    && same_dacp_echo_volume(reported.db, echo.db)
            }) || self.remote_inflight.as_ref().is_some_and(|flight| {
                self.target
                    .is_some_and(|target| target.session_id == reported.session_id)
                    && flight.write_gate.write_authorized_but_unresolved()
                    && same_dacp_echo_volume(reported.db, flight.db)
            }))
    }

    fn plan_tick(&mut self, active: Option<u64>, output_changed: bool) -> VolumeTickPlan {
        let Some(target) = self.target else {
            self.retry = None;
            return VolumeTickPlan::None;
        };
        if active != Some(target.session_id) {
            self.clear();
            return VolumeTickPlan::ClearedInactive;
        }
        if output_changed {
            return VolumeTickPlan::Apply {
                target,
                retry_attempt: None,
            };
        }
        let Some(retry) = self.retry.as_mut() else {
            return VolumeTickPlan::None;
        };
        if retry.target != target || retry.attempts >= VOLUME_RETRY_LIMIT {
            self.retry = None;
            return VolumeTickPlan::None;
        }
        retry.attempts += 1;
        VolumeTickPlan::Apply {
            target,
            retry_attempt: Some(retry.attempts),
        }
    }

    fn succeeded(&mut self, target: SessionVolume) {
        if self.target == Some(target) {
            self.retry = None;
            // Use what the sender asked for, not what the device happened to
            // read back. If native hardware clamps it, the next poll observes
            // one real mismatch and corrects the sender UI over DACP.
            self.sender_known_system = Some(SystemVolumeState::from_sender_db(target.db));
            self.remote_retry = None;
            if let Some(flight) = self.remote_inflight.take() {
                let _ = flight.write_gate.cancel();
            }
        }
    }

    fn failed(&mut self, target: SessionVolume, retry_attempt: Option<u8>) {
        if self.target != Some(target) {
            return;
        }
        if retry_attempt.is_some_and(|attempt| attempt >= VOLUME_RETRY_LIMIT) {
            self.retry = None;
            return;
        }
        if self.retry.is_none_or(|retry| retry.target != target) {
            self.retry = Some(VolumeRetry {
                target,
                attempts: retry_attempt.unwrap_or(0),
            });
        }
    }

    #[cfg(test)]
    fn plan_remote(
        &mut self,
        active: Option<u64>,
        current: SystemVolumeState,
        serial: u64,
    ) -> Option<RemoteInFlight> {
        self.plan_remote_for_route(active, current, serial, false)
    }

    fn plan_remote_for_route(
        &mut self,
        active: Option<u64>,
        current: SystemVolumeState,
        serial: u64,
        preserve_muted_scalar: bool,
    ) -> Option<RemoteInFlight> {
        self.prune_completed_remote_echoes(Instant::now());
        let target = self.target?;
        if active != Some(target.session_id)
            || self.remote_inflight.is_some()
            || self
                .completed_remote_echoes
                .iter()
                .any(|echo| echo.write_gate.write_authorized_but_unresolved())
        {
            return None;
        }
        let known = self.sender_known_system?;
        if same_reverse_volume(known, current, preserve_muted_scalar) && !self.force_remote {
            self.remote_retry = None;
            return None;
        }
        if self.remote_retry.is_some_and(|retry| {
            !same_reverse_volume(retry.desired, current, preserve_muted_scalar)
        }) {
            self.remote_retry = None;
        }
        let attempts = self.remote_retry.map_or(0, |retry| retry.attempts);
        if attempts >= DACP_VOLUME_RETRY_LIMIT {
            return None;
        }
        Some(RemoteInFlight {
            serial,
            desired: current,
            db: current.airplay_db(),
            attempt: attempts + 1,
            echo_confirmed: false,
            sender_revision_at_queue: self.sender_revision,
            write_gate: DacpWriteGate::new(),
        })
    }

    #[cfg(test)]
    fn settle_remote_if_converged(
        &mut self,
        active: Option<u64>,
        current: SystemVolumeState,
    ) -> bool {
        self.settle_remote_if_converged_for_route(active, current, false)
    }

    fn settle_remote_if_converged_for_route(
        &mut self,
        active: Option<u64>,
        current: SystemVolumeState,
        preserve_muted_scalar: bool,
    ) -> bool {
        let Some(target) = self.target else {
            return false;
        };
        if active != Some(target.session_id)
            || self.remote_inflight.is_some()
            || self.force_remote
            || !self
                .sender_known_system
                .is_some_and(|known| same_reverse_volume(known, current, preserve_muted_scalar))
        {
            return false;
        }
        self.remote_retry = None;
        true
    }

    fn remote_queued(&mut self, flight: RemoteInFlight) {
        self.remote_retry = Some(RemoteRetry {
            desired: flight.desired,
            attempts: flight.attempt,
        });
        self.remote_inflight = Some(flight);
    }

    #[cfg(test)]
    fn remote_result(&mut self, serial: u64, http_success: bool) -> Option<RemoteCompletion> {
        self.remote_result_classified_at(serial, http_success, http_success, Instant::now())
    }

    #[cfg(test)]
    fn remote_result_at(
        &mut self,
        serial: u64,
        http_success: bool,
        now: Instant,
    ) -> Option<RemoteCompletion> {
        self.remote_result_classified_at(serial, http_success, http_success, now)
    }

    fn remote_result_classified_at(
        &mut self,
        serial: u64,
        http_success: bool,
        may_have_written: bool,
        now: Instant,
    ) -> Option<RemoteCompletion> {
        let flight = self
            .remote_inflight
            .as_ref()
            .filter(|flight| flight.serial == serial)?
            .clone();
        self.remote_inflight = None;
        let delivered = http_success || flight.echo_confirmed;
        if may_have_written && !flight.echo_confirmed {
            flight.write_gate.mark_full_request_written();
            self.remember_completed_remote_echo(flight.clone(), now);
            // The local endpoint value that originated this request remains
            // receiver authority even when the response is uncertain. Keep
            // sender_known_system unchanged so the retry remains armed until
            // either an echo confirms delivery or a later attempt succeeds.
            if let Some(target) = self.target.as_mut() {
                target.db = flight.db;
            }
        }
        if delivered {
            if let Some(target) = self.target.as_mut() {
                target.db = flight.db;
            }
            self.sender_known_system = Some(flight.desired);
            self.remote_retry = None;
            self.force_remote = false;
        }
        Some(RemoteCompletion { flight, delivered })
    }

    fn remember_completed_remote_echo(&mut self, flight: RemoteInFlight, now: Instant) {
        self.prune_completed_remote_echoes(now);
        let Some(session_id) = self.target.map(|target| target.session_id) else {
            return;
        };
        let first_unconsumed_revision = flight
            .sender_revision_at_queue
            .into_iter()
            .chain(self.sender_revision)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        self.push_completed_remote_echo(flight, session_id, first_unconsumed_revision, now);
    }

    fn push_completed_remote_echo(
        &mut self,
        flight: RemoteInFlight,
        session_id: u64,
        first_unconsumed_revision: u64,
        now: Instant,
    ) {
        self.prune_completed_remote_echoes(now);
        while self.completed_remote_echoes.len() >= DACP_COMPLETED_ECHO_LIMIT {
            self.completed_remote_echoes.pop_front();
        }
        debug_assert!(
            self.completed_remote_echoes
                .back()
                .is_none_or(|echo| echo.session_id != session_id || echo.serial < flight.serial),
            "the single DACP worker must complete writes in serial order"
        );
        let expected_revision = self
            .completed_remote_echoes
            .back()
            .filter(|echo| echo.session_id == session_id)
            .map_or(first_unconsumed_revision, |echo| {
                first_unconsumed_revision.max(echo.expected_revision.saturating_add(1))
            });
        self.completed_remote_echoes.push_back(CompletedRemoteEcho {
            serial: flight.serial,
            session_id,
            db: flight.db,
            desired: flight.desired,
            write_gate: flight.write_gate,
            expected_revision,
            expires_at: now + DACP_COMPLETED_ECHO_GRACE,
        });
    }

    fn cancel_remote_inflight_after_sender_command(
        &mut self,
        session_id: u64,
        revision: u64,
        now: Instant,
    ) {
        let Some(flight) = self.remote_inflight.take() else {
            return;
        };
        self.remote_retry = None;
        if flight.echo_confirmed || !flight.write_gate.cancel() {
            return;
        }
        self.force_remote = true;
        if self.sender_revision.and_then(|seen| seen.checked_add(1)) != Some(revision) {
            // A coalesced gap cannot reveal whether this committed request
            // already reflected in a skipped revision. Do not invent a future
            // exact slot; sender authority wins and one latest-value reassertion
            // remains armed for the uncertain old write.
            return;
        }
        self.push_completed_remote_echo(flight, session_id, revision.saturating_add(1), now);
    }

    fn cancel_remote_inflight_for_external_change(&mut self, active: Option<u64>, now: Instant) {
        let Some(flight) = self.remote_inflight.take() else {
            return;
        };
        self.remote_retry = None;
        if flight.echo_confirmed || !flight.write_gate.cancel() {
            return;
        }
        let Some(session_id) = self
            .target
            .map(|target| target.session_id)
            .filter(|session_id| active == Some(*session_id))
        else {
            return;
        };
        let first_unconsumed_revision = self.sender_revision.unwrap_or(0).saturating_add(1);
        self.push_completed_remote_echo(flight, session_id, first_unconsumed_revision, now);
        self.force_remote = true;
    }

    fn prune_completed_remote_echoes(&mut self, now: Instant) {
        self.completed_remote_echoes
            .retain(|echo| echo.expires_at > now);
    }

    fn advance_completed_echoes_after_sender_command(&mut self, session_id: u64, revision: u64) {
        let Some(previous) = self.sender_revision else {
            return;
        };
        if previous.checked_add(1) == Some(revision) {
            for echo in &mut self.completed_remote_echoes {
                if echo.session_id == session_id && echo.expected_revision > previous {
                    echo.expected_revision = echo.expected_revision.saturating_add(1);
                }
            }
            return;
        }

        self.completed_remote_echoes
            .retain(|echo| echo.session_id != session_id);
    }

    fn remote_superseded(&mut self, serial: u64) {
        if self
            .remote_inflight
            .as_ref()
            .is_some_and(|flight| flight.serial == serial)
        {
            if let Some(flight) = self.remote_inflight.take() {
                let _ = flight.write_gate.cancel();
            }
            // Credential replacement did not consume an HTTP retry. The next
            // native poll can immediately plan against the fresh snapshot.
            self.remote_retry = None;
        }
        self.completed_remote_echoes
            .retain(|echo| echo.serial != serial);
    }

    fn remote_revoked_after_possible_write(&mut self, serial: u64) {
        if self
            .remote_inflight
            .as_ref()
            .is_some_and(|flight| flight.serial == serial)
        {
            let flight = self
                .remote_inflight
                .take()
                .expect("matching flight was checked above");
            self.remote_retry = None;
            if !flight.echo_confirmed && flight.write_gate.may_have_written() {
                if let Some(session_id) = self.target.map(|target| target.session_id) {
                    let expected = self.sender_revision.unwrap_or(0).saturating_add(1);
                    self.push_completed_remote_echo(flight, session_id, expected, Instant::now());
                }
            }
        }
        // The sender may have issued a newer command after these bytes went on
        // the wire, which deliberately cleared `remote_inflight`. The old write
        // can still land last, so every possibly-written outcome for the current
        // session/generation requires one reassertion of current authority even
        // when its serial no longer matches the logical flight.
        self.force_remote = true;
    }

    fn observe_remote_revision(
        &mut self,
        active: Option<u64>,
        remote: Option<(u64, u64, u64)>,
    ) -> bool {
        let revision = remote
            .filter(|(session_id, _, _)| active == Some(*session_id))
            .map(|(_, credential_revision, endpoint_revision)| {
                (credential_revision, endpoint_revision)
            });
        if self.remote_revision == revision {
            return false;
        }
        self.remote_revision = revision;
        self.cancel_remote_inflight_for_external_change(active, Instant::now());
        self.force_remote |= revision.is_some()
            && self
                .target
                .is_some_and(|target| active == Some(target.session_id))
            && self.sender_known_system.is_some();
        true
    }

    fn observe_output_change(&mut self, active: Option<u64>) {
        let sender_authority_exists = self
            .target
            .is_some_and(|target| active == Some(target.session_id))
            && self.sender_known_system.is_some();
        self.cancel_remote_inflight_for_external_change(active, Instant::now());
        // Even an outcome accepted just before the epoch increment may have
        // changed sender state. Replay current authority onto the new output,
        // then reassert it to the sender once.
        self.force_remote |= sender_authority_exists;
    }

    fn stale_remote_may_have_reached_sender(&mut self) {
        self.force_remote = true;
    }
}

fn reject_revoked_dacp_outcome(
    volume: &mut VolumeSync,
    serial: u64,
    result: &DacpOutcomeResult,
    write_gate: &DacpWriteGate,
    credential_current: bool,
) -> bool {
    if credential_current {
        return false;
    }
    if result.may_have_written_request(write_gate) {
        volume.remote_revoked_after_possible_write(serial);
    } else {
        volume.remote_superseded(serial);
    }
    true
}

fn reverse_outcome_authority_current(outcome: &DacpOutcome) -> bool {
    if !outcome.output_epoch.is_current() {
        return false;
    }
    match outcome.route {
        ReverseVolumeRoute::Event => outcome
            .event_control
            .as_ref()
            .is_some_and(ReceiverVolumeControlSnapshot::is_current),
        ReverseVolumeRoute::Dacp => {
            outcome
                .remote_control
                .as_ref()
                .is_some_and(RemoteControlSnapshot::is_current)
                && outcome
                    .endpoint
                    .as_ref()
                    .is_some_and(DacpEndpointSnapshot::is_current)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Desired {
    effective_enabled: bool,
    effective_name: String,
    mac: [u8; 6],
    password_set: bool,
}

struct ReconcileInput {
    effective_enabled: bool,
    effective_name: String,
    mac: [u8; 6],
    password: Option<String>,
    secret_changed: bool,
    stopped_reason: Option<String>,
}

#[derive(Debug)]
struct ControllerState {
    desired: Desired,
    generation: u64,
    runtime: Option<AirPlayRuntime>,
    bus: Option<PcmBus>,
    mdns: Option<AirPlayMdnsGuard>,
    event_thread: Option<JoinHandle<()>>,
    error: Option<String>,
    native_volume_warning: Option<String>,
    dacp_warning: Option<String>,
    volume: VolumeSync,
}

/// Actual receiver facts used by `settings.get`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LiveStatus {
    pub listening: bool,
    pub error: Option<String>,
    pub warning: Option<String>,
    pub airplay2_port: Option<u16>,
}

/// Owns the currently running receiver. Reconfiguration is serialized because
/// `settings.set` is served by one thread per IPC connection.
pub(crate) struct AirPlayController {
    advertise: bool,
    /// Stable AP2 identity is kept under the exact daemon config root (which
    /// may be test- or environment-overridden), never under a second platform
    /// default and never in the daemon's unrelated peer identity file.
    airplay2_identity_path: PathBuf,
    /// The AP2 control endpoint is persisted beside the identity so sender
    /// discovery caches do not retain a now-dead ephemeral port after a daemon
    /// restart or an in-place app redeploy.
    airplay2_port_path: PathBuf,
    // Changes whenever `state.bus` is replaced. The 10 ms mixer path checks
    // this atomic first and only takes the controller mutex on a replacement.
    bus_epoch: AtomicU64,
    // System volume writes can be initiated by the protocol event pump and the
    // ordinary 200 ms ticker. Serialize them without holding `state` across OS IO.
    volume_apply_lock: Mutex<()>,
    dacp_serial: AtomicU64,
    dacp: DacpWorker,
    reconcile_lock: Mutex<()>,
    state: Mutex<ControllerState>,
}

impl AirPlayController {
    pub(crate) fn new(advertise: bool, config_dir: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            advertise,
            airplay2_identity_path: config_dir.join("airplay2-identity"),
            airplay2_port_path: config_dir.join(AIRPLAY2_PORT_FILE),
            bus_epoch: AtomicU64::new(1),
            volume_apply_lock: Mutex::new(()),
            dacp_serial: AtomicU64::new(1),
            dacp: DacpWorker::new(),
            reconcile_lock: Mutex::new(()),
            state: Mutex::new(ControllerState {
                desired: Desired {
                    effective_enabled: false,
                    effective_name: String::new(),
                    mac: [0; 6],
                    password_set: false,
                },
                generation: 0,
                runtime: None,
                bus: None,
                mdns: None,
                event_thread: None,
                error: None,
                native_volume_warning: None,
                dacp_warning: None,
                volume: VolumeSync::default(),
            }),
        })
    }

    /// Apply the daemon-owned settings synchronously. `secret_changed` matters
    /// even if password presence did not change: replacing one non-empty
    /// password with another must restart protocol authentication.
    fn reconcile(self: &Arc<Self>, input: ReconcileInput) {
        let ReconcileInput {
            effective_enabled,
            effective_name,
            mac,
            password,
            secret_changed,
            stopped_reason,
        } = input;
        let _serial = lk(&self.reconcile_lock);

        let desired = Desired {
            effective_enabled,
            effective_name,
            mac,
            password_set: password.is_some(),
        };
        let restart = {
            let state = lk(&self.state);
            let runtime_unhealthy = desired.effective_enabled
                && state
                    .runtime
                    .as_ref()
                    .is_none_or(|runtime| runtime.status().phase != RuntimePhase::Listening);
            secret_changed || state.desired != desired || runtime_unhealthy
        };
        if !restart {
            // Effective-disabled can have different reasons (plain off,
            // non-Share mode, unreadable configured secret). The runtime
            // shape is unchanged, but the live status still must tell the
            // caller why its saved enabled switch is not in force.
            if !desired.effective_enabled {
                lk(&self.state).error = stopped_reason;
            }
            return;
        }

        // Withdraw discovery before stopping the listener. A sender must not
        // discover a port in the interval where it is being torn down.
        let (old_mdns, old_runtime, old_events, generation) = {
            let mut state = lk(&self.state);
            state.generation = state.generation.wrapping_add(1).max(1);
            state.desired = desired.clone();
            state.bus = None;
            self.bus_epoch.fetch_add(1, Ordering::Release);
            state.error = None;
            state.native_volume_warning = None;
            state.dacp_warning = None;
            state.volume.clear();
            // Polling plans and queues DACP work under this same state lock.
            // Invalidate only after clearing state so every older enqueue is
            // ordered before (and therefore cancelled by) this store.
            self.dacp.invalidate();
            (
                state.mdns.take(),
                state.runtime.take(),
                state.event_thread.take(),
                state.generation,
            )
        };
        drop(old_mdns);
        if let Some(runtime) = old_runtime {
            if let Err(e) = runtime.stop() {
                dlog!("[audiohubd] AirPlay receiver stop failed: {e}");
            }
        }
        if let Some(events) = old_events {
            let _ = events.join();
        }

        if !desired.effective_enabled {
            lk(&self.state).error = stopped_reason;
            return;
        }

        // The legacy receiver is preserved by the archive branch/tag, not
        // advertised beside the AudioHub-owned implementation. Keeping this
        // deployment AP2-only makes every device test an unambiguous test of
        // our own listener and prevents a sender from silently falling back.
        let mut config = AirPlayConfig::new(desired.effective_name.clone());
        // The protocol library's cross-platform fallback MAC is intentionally
        // constant. Supplying the daemon identity derivative is therefore not
        // optional: two AudioHub receivers on one LAN must not advertise the
        // same protocol identity.
        config.mac = Some(desired.mac);
        config.password = password;
        config.enable_airplay2 = true;
        config.airplay2_identity_path = Some(self.airplay2_identity_path.clone());
        config.initial_volume_db = match read_system_volume() {
            Ok(volume) => Some(volume.airplay_db()),
            Err(error) => {
                dlog!("[audiohubd] AirPlay initial system volume unavailable: {error:#}");
                None
            }
        };
        #[cfg(test)]
        config.use_ephemeral_ptp_ports_for_tests();

        let runtime = match start_airplay_runtime(config, &self.airplay2_port_path) {
            Ok(runtime) => runtime,
            Err(e) => {
                lk(&self.state).error = Some(format!("AirPlay 接收器启动失败：{e}"));
                return;
            }
        };
        let events = runtime.subscribe_events();
        let bus = runtime.pcm_bus();

        let mdns = if self.advertise {
            match AirPlayMdnsGuard::register(runtime.mdns_services()) {
                Ok(guard) => Some(guard),
                Err(e) => {
                    let _ = runtime.stop();
                    lk(&self.state).error = Some(format!("AirPlay 广播失败：{e:#}"));
                    return;
                }
            }
        } else {
            None
        };

        {
            let mut state = lk(&self.state);
            // Reconcile calls are serialized, so a generation mismatch can
            // only mean shutdown replaced our state while startup was on the
            // wire. Do not resurrect a stopped receiver.
            if state.generation != generation || !state.desired.effective_enabled {
                drop(state);
                drop(mdns);
                let _ = runtime.stop();
                return;
            }
            state.bus = Some(bus);
            self.bus_epoch.fetch_add(1, Ordering::Release);
            state.runtime = Some(runtime);
            state.mdns = mdns;
            state.error = None;
        }
        match self.spawn_event_pump(generation, events) {
            Ok(handle) => lk(&self.state).event_thread = Some(handle),
            Err(e) => {
                let (mdns, runtime) = {
                    let mut state = lk(&self.state);
                    state.bus = None;
                    state.volume.clear();
                    state.native_volume_warning = None;
                    state.dacp_warning = None;
                    // A ticker can observe the bus during the narrow interval
                    // between installing the runtime and spawning this pump.
                    // Cancel any job it planned with that now-defunct token.
                    self.dacp.invalidate();
                    self.bus_epoch.fetch_add(1, Ordering::Release);
                    state.error = Some(format!("AirPlay 事件线程启动失败：{e}"));
                    (state.mdns.take(), state.runtime.take())
                };
                drop(mdns);
                if let Some(runtime) = runtime {
                    let _ = runtime.stop();
                }
                return;
            }
        }
        let status = self.live_status();
        dlog!(
            "[audiohubd] AirPlay 2 receiver listening name={:?} port={:?} advertised={}",
            desired.effective_name,
            status.airplay2_port,
            self.advertise
        );
    }

    fn spawn_event_pump(
        self: &Arc<Self>,
        generation: u64,
        events: mpsc::Receiver<AirPlayEvent>,
    ) -> io::Result<JoinHandle<()>> {
        let controller = Arc::clone(self);
        thread::Builder::new()
            .name("ahb-airplay-events".to_string())
            .spawn(move || {
                while let Ok(event) = events.recv() {
                    if lk(&controller.state).generation != generation {
                        return;
                    }
                    match event {
                        AirPlayEvent::Volume {
                            session_id: event_session_id,
                            db: event_db,
                            ..
                        } => {
                            let target = {
                                let mut state = lk(&controller.state);
                                if state.generation != generation {
                                    return;
                                }
                                let active_session =
                                    state.bus.as_ref().and_then(PcmBus::active_session);
                                let sender_snapshot = state
                                    .runtime
                                    .as_ref()
                                    .and_then(AirPlayRuntime::sender_volume_snapshot);
                                let authoritative = authoritative_volume_target(
                                    sender_snapshot,
                                    active_session,
                                );
                                let Some((target, revision)) = authoritative else {
                                    dlog!(
                                        "[audiohubd] ignoring AirPlay volume wakeup db={event_db:.2} for session {:?} (active {:?}, no authoritative sender snapshot)",
                                        event_session_id,
                                        active_session
                                    );
                                    continue;
                                };
                                let (target, replaced_session) = match state
                                    .volume
                                    .reconcile_sender_snapshot(Some((target, revision)))
                                {
                                    SenderSnapshotDecision::Unchanged => {
                                        // The event queue is bounded and asynchronous.
                                        // Treat its payload only as a wakeup: a delayed
                                        // event whose lossless revision was already
                                        // consumed must not roll a newer reverse-volume
                                        // success back to the old sender value.
                                        continue;
                                    }
                                    SenderSnapshotDecision::ReverseEcho { target } => {
                                        // The sender reflected our in-flight DACP value
                                        // before completing the HTTP response. The state
                                        // machine consumed its revision while preserving
                                        // the worker flight, so neither native IO nor
                                        // serial invalidation belongs on this path.
                                        state.dacp_warning = None;
                                        dlog!(
                                            "[audiohubd] AirPlay reverse volume echo confirmed session={} db={:.2}",
                                            target.session_id,
                                            target.db
                                        );
                                        continue;
                                    }
                                    SenderSnapshotDecision::DeferredRemoteOutcome => {
                                        // The worker has write authorization but
                                        // has not yet proved a complete request.
                                        // Leave the lossless snapshot untouched;
                                        // the next outcome/tick resolves whether
                                        // this was an echo or a sender command.
                                        continue;
                                    }
                                    SenderSnapshotDecision::Apply {
                                        target,
                                        replaced_session,
                                    } => (target, replaced_session),
                                };
                                // Runtime installs the lossless snapshot before
                                // publishing the event. Adopting that snapshot
                                // under `state` gives the current command a stable
                                // identity and keeps the event path low-latency.
                                // Invalidate every older queued reverse write only
                                // when a new snapshot revision was actually adopted.
                                controller.dacp.invalidate();
                                if replaced_session {
                                    // A replacement sender must not inherit a
                                    // warning produced for the previous one.
                                    state.native_volume_warning = None;
                                }
                                // The sender just established a newer value, so
                                // any reverse-direction failure is no longer a
                                // pending user action.
                                state.dacp_warning = None;
                                target
                            };
                            controller.apply_volume_target(
                                generation,
                                target,
                                VolumeApplyReason::SenderEvent,
                            );
                        }
                        AirPlayEvent::SessionEnded { session_id, .. } => {
                            let mut state = lk(&controller.state);
                            if state.generation != generation {
                                return;
                            }
                            if state.volume.ended(session_id) {
                                controller.dacp.invalidate();
                                state.native_volume_warning = None;
                                state.dacp_warning = None;
                            }
                        }
                        AirPlayEvent::RuntimeFailed { message } => {
                            let mdns = {
                                let mut state = lk(&controller.state);
                                if state.generation != generation {
                                    return;
                                }
                                state.error = Some(format!("AirPlay 接收器运行失败：{message}"));
                                state.bus = None;
                                state.volume.clear();
                                state.native_volume_warning = None;
                                state.dacp_warning = None;
                                // Runtime failure ends the authority of this
                                // session and its bearer token. Polling queues
                                // under the same state lock, so this cancels
                                // every older job before the lock is released.
                                controller.dacp.invalidate();
                                controller.bus_epoch.fetch_add(1, Ordering::Release);
                                state.mdns.take()
                            };
                            // Stop advertising a listener that died.
                            drop(mdns);
                        }
                        _ => {}
                    }
                }
            })
    }

    fn apply_volume_target(
        &self,
        generation: u64,
        target: SessionVolume,
        reason: VolumeApplyReason,
    ) {
        let _serial = lk(&self.volume_apply_lock);
        {
            let state = lk(&self.state);
            let active = state.bus.as_ref().and_then(PcmBus::active_session);
            if state.generation != generation || !state.volume.is_current(target, active) {
                return;
            }
        }

        let result = apply_system_volume(target.db);
        let mut state = lk(&self.state);
        let active = state.bus.as_ref().and_then(PcmBus::active_session);
        if state.generation != generation || !state.volume.is_current(target, active) {
            return;
        }
        match result {
            Ok(applied) => {
                state.volume.succeeded(target);
                state.native_volume_warning = None;
                match reason {
                    VolumeApplyReason::SenderEvent => dlog!(
                        "[audiohubd] AirPlay volume session={} db={:.2} applied_scalar={applied:.6}",
                        target.session_id,
                        target.db
                    ),
                    VolumeApplyReason::StatusReconcile => dlog!(
                        "[audiohubd] AirPlay volume reconciled from runtime status session={} db={:.2} applied_scalar={applied:.6}",
                        target.session_id,
                        target.db
                    ),
                    VolumeApplyReason::OutputChanged => dlog!(
                        "[audiohubd] AirPlay volume reapplied after default output change session={} db={:.2} applied_scalar={applied:.6}",
                        target.session_id,
                        target.db
                    ),
                    VolumeApplyReason::Retry(attempt) => dlog!(
                        "[audiohubd] AirPlay volume retry {attempt}/{VOLUME_RETRY_LIMIT} succeeded session={} db={:.2} applied_scalar={applied:.6}",
                        target.session_id,
                        target.db
                    ),
                }
            }
            Err(error) => {
                state.volume.failed(target, reason.retry_attempt());
                let message = format!("AirPlay 音量同步失败：{error:#}");
                dlog!(
                    "[audiohubd] {message} (session={} db={:.2} reason={reason:?})",
                    target.session_id,
                    target.db
                );
                state.native_volume_warning = Some(message);
                if reason == VolumeApplyReason::Retry(VOLUME_RETRY_LIMIT) {
                    dlog!(
                        "[audiohubd] AirPlay volume retry limit reached for session={} db={:.2}",
                        target.session_id,
                        target.db
                    );
                }
            }
        }
    }

    pub(crate) fn stop(&self) {
        let _serial = lk(&self.reconcile_lock);
        let (mdns, runtime, events) = {
            let mut state = lk(&self.state);
            state.generation = state.generation.wrapping_add(1).max(1);
            state.bus = None;
            self.bus_epoch.fetch_add(1, Ordering::Release);
            state.error = None;
            state.native_volume_warning = None;
            state.dacp_warning = None;
            state.volume.clear();
            // See `reconcile`: this must be later than every old enqueue under
            // the same state lock, including one racing with shutdown.
            self.dacp.invalidate();
            (
                state.mdns.take(),
                state.runtime.take(),
                state.event_thread.take(),
            )
        };
        drop(mdns);
        if let Some(runtime) = runtime {
            if let Err(e) = runtime.stop() {
                dlog!("[audiohubd] AirPlay receiver shutdown failed: {e}");
            }
        }
        if let Some(events) = events {
            let _ = events.join();
        }
    }

    pub(crate) fn live_status(&self) -> LiveStatus {
        let state = lk(&self.state);
        let runtime_status = state.runtime.as_ref().map(AirPlayRuntime::status);
        let listening = runtime_status
            .as_ref()
            .is_some_and(|s| s.phase == RuntimePhase::Listening);
        LiveStatus {
            listening,
            error: state
                .error
                .clone()
                .or_else(|| runtime_status.as_ref().and_then(|s| s.last_error.clone())),
            warning: state
                .native_volume_warning
                .clone()
                .or_else(|| state.dacp_warning.clone()),
            airplay2_port: runtime_status.as_ref().and_then(|s| s.airplay2_port),
        }
    }

    pub(crate) fn sessions(&self) -> Vec<AirPlaySessionInfo> {
        let now = unix_ms();
        let state = lk(&self.state);
        let Some(runtime) = state.runtime.as_ref() else {
            return Vec::new();
        };
        runtime
            .status()
            .sessions
            .into_iter()
            .filter(|session| session.protocol == Protocol::AirPlay2)
            .map(|session| session_view(session, now))
            .collect()
    }

    pub(crate) fn has_active_session(&self) -> bool {
        lk(&self.state)
            .bus
            .as_ref()
            .and_then(PcmBus::active_session)
            .is_some()
    }

    /// Ordinary 200 ms control-ticker hook. Runtime status is authoritative for
    /// the latest sender volume, so this also repairs a final volume event that
    /// could not enter the bounded observer queue. A default-output replacement
    /// reapplies the active sender's last command; otherwise it only performs a
    /// bounded retry that an earlier native write explicitly left pending.
    pub(crate) fn volume_tick(
        &self,
        output_epoch: &Arc<AtomicU64>,
        expected_output_epoch: u64,
        output_changed: bool,
    ) {
        let output_lease = OutputEpochLease::new(expected_output_epoch, output_epoch);
        // The watcher can fire after the ticker sampled `expected_output_epoch`
        // but before control reaches us. Treat that exactly like a change seen
        // by the ticker: replay sender authority before any reverse read.
        let output_changed = output_changed || !output_lease.is_current();
        self.refresh_dacp_authority();
        self.finish_dacp_outcomes();
        let (generation, plan, reconciled) = {
            let mut state = lk(&self.state);
            let active = state.bus.as_ref().and_then(PcmBus::active_session);
            if output_changed {
                self.dacp.invalidate();
                state.volume.observe_output_change(active);
            }
            let generation = state.generation;
            let authoritative = authoritative_volume_target(
                state
                    .runtime
                    .as_ref()
                    .and_then(AirPlayRuntime::sender_volume_snapshot),
                active,
            );
            let decision = state.volume.reconcile_sender_snapshot(authoritative);
            let (plan, reconciled) = match decision {
                SenderSnapshotDecision::Apply {
                    target,
                    replaced_session,
                } => {
                    self.dacp.invalidate();
                    if replaced_session {
                        state.native_volume_warning = None;
                    }
                    state.dacp_warning = None;
                    (
                        VolumeTickPlan::Apply {
                            target,
                            retry_attempt: None,
                        },
                        true,
                    )
                }
                SenderSnapshotDecision::ReverseEcho { .. }
                | SenderSnapshotDecision::DeferredRemoteOutcome => {
                    state.dacp_warning = None;
                    let plan = if output_changed {
                        // Replay the current target to the replacement endpoint
                        // before any native read. The echoed/deferred payload may
                        // be an older value than the authority retained in target.
                        state.volume.plan_tick(active, true)
                    } else {
                        VolumeTickPlan::None
                    };
                    (plan, false)
                }
                SenderSnapshotDecision::Unchanged => {
                    (state.volume.plan_tick(active, output_changed), false)
                }
            };
            if plan == VolumeTickPlan::ClearedInactive {
                // SessionEnded is a bounded observer event and can be dropped.
                // This ticker fallback must revoke queued bearer use as well as
                // clear logical volume state.
                self.dacp.invalidate();
                state.native_volume_warning = None;
                state.dacp_warning = None;
            }
            (generation, plan, reconciled)
        };
        if let VolumeTickPlan::Apply {
            target,
            retry_attempt,
        } = plan
        {
            let reason = if reconciled {
                VolumeApplyReason::StatusReconcile
            } else {
                retry_attempt.map_or(VolumeApplyReason::OutputChanged, VolumeApplyReason::Retry)
            };
            self.apply_volume_target(generation, target, reason);
            return;
        }
        if plan == VolumeTickPlan::ClearedInactive {
            return;
        }
        if let Some(observed_epoch) = self.poll_native_volume_for_sender(output_lease.clone()) {
            // A replacement raced this reverse poll. Re-enter only the normal
            // output-change branch (which applies sender authority and returns
            // without polling) after releasing the native-volume lock.
            self.volume_tick(output_epoch, observed_epoch, true);
        }
    }

    fn refresh_dacp_authority(&self) {
        let mut state = lk(&self.state);
        let active = state.bus.as_ref().and_then(PcmBus::active_session);
        let remote = state.runtime.as_ref().and_then(|runtime| {
            runtime
                .receiver_volume_control_snapshot()
                .filter(ReceiverVolumeControlSnapshot::is_current)
                .map(|snapshot| (snapshot.session_id(), snapshot.revision(), u64::MAX))
                .or_else(|| {
                    let snapshot = runtime
                        .active_remote_control_snapshot()
                        .filter(RemoteControlSnapshot::is_current)?;
                    let endpoint = state.mdns.as_ref()?.dacp_endpoint(snapshot.info())?;
                    Some((
                        snapshot.info().session_id,
                        snapshot.revision(),
                        endpoint.revision,
                    ))
                })
        });
        if state.volume.observe_remote_revision(active, remote) {
            // Jobs queued under any previous revision must fail their serial
            // guard even if the secret refresh raced between network return
            // and outcome consumption. The revision tracker also forces one
            // current-native reassertion on the fresh lease next tick.
            self.dacp.invalidate();
            state.dacp_warning = None;
        }
    }

    fn finish_dacp_outcomes(&self) {
        for outcome in self.dacp.drain_outcomes() {
            let mut state = lk(&self.state);
            let active = state.bus.as_ref().and_then(PcmBus::active_session);
            if state.generation != outcome.generation || active != Some(outcome.session_id) {
                // A request from the previous session/generation can finish
                // after a fast reconnect. If it may have reached the sender,
                // arm one harmless correction using the current session's
                // credentials once that sender has established its value.
                if outcome.result.may_have_written_request(&outcome.write_gate) {
                    state.volume.stale_remote_may_have_reached_sender();
                }
                continue;
            }
            if reject_revoked_dacp_outcome(
                &mut state.volume,
                outcome.serial,
                &outcome.result,
                &outcome.write_gate,
                reverse_outcome_authority_current(&outcome),
            ) {
                continue;
            }
            let may_have_written = outcome.result.may_have_written_request(&outcome.write_gate);
            let result = match outcome.result {
                DacpOutcomeResult::Superseded => {
                    state.volume.remote_superseded(outcome.serial);
                    continue;
                }
                DacpOutcomeResult::RevokedAfterPossibleWrite => {
                    state
                        .volume
                        .remote_revoked_after_possible_write(outcome.serial);
                    continue;
                }
                DacpOutcomeResult::Sent(result) => result,
            };
            // The runtime installs its lossless sender snapshot before it
            // publishes the bounded event wakeup. If the matching RTSP echo is
            // already present, consume it while this exact flight still owns
            // its serial. `remote_result` clears the flight below; waiting
            // until the ordinary reconciliation phase would misclassify the
            // echo as a fresh sender command.
            let authoritative = authoritative_volume_target(
                state
                    .runtime
                    .as_ref()
                    .and_then(AirPlayRuntime::sender_volume_snapshot),
                active,
            );
            if may_have_written {
                if let Some(target) = state
                    .volume
                    .confirm_remote_echo(authoritative, Some(outcome.serial))
                {
                    state.dacp_warning = None;
                    dlog!(
                        "[audiohubd] AirPlay reverse volume echo confirmed before DACP outcome session={} db={:.2}",
                        target.session_id,
                        target.db
                    );
                }
            }
            let http_success = result.is_ok();
            let Some(completion) = state.volume.remote_result_classified_at(
                outcome.serial,
                http_success,
                may_have_written,
                Instant::now(),
            ) else {
                // The job was already on the wire when a newer sender command
                // invalidated it. Both an HTTP success and an error after a
                // possible `write_all` are uncertain stale writes; force one
                // correction to the current native value after that newer
                // command finishes applying. `Superseded` was filtered above
                // and is the only outcome known not to have written bytes.
                if may_have_written {
                    state.volume.stale_remote_may_have_reached_sender();
                } else {
                    state.volume.remote_superseded(outcome.serial);
                }
                continue;
            };
            let flight = completion.flight;
            match result {
                Ok(()) => {
                    state.dacp_warning = None;
                    let route = match outcome.route {
                        ReverseVolumeRoute::Event => "AP2 event",
                        ReverseVolumeRoute::Dacp => "DACP",
                    };
                    dlog!(
                        "[audiohubd] AirPlay {route} volume request accepted session={} db={:.2}",
                        outcome.session_id,
                        outcome.db
                    );
                }
                Err(error) if completion.delivered => {
                    // Some DACP receivers reflect the accepted value through
                    // RTSP before the HTTP response is complete. That echo is
                    // stronger delivery evidence than a later response-read
                    // failure, so do not show a false warning or retry it.
                    state.dacp_warning = None;
                    dlog!(
                        "[audiohubd] AirPlay DACP volume confirmed by RTSP echo session={} db={:.2} (HTTP completion failed: {error})",
                        outcome.session_id,
                        outcome.db
                    );
                }
                Err(error) => {
                    let message = format!("AirPlay 反向音量同步失败：{error}");
                    state.dacp_warning = Some(message.clone());
                    dlog!(
                        "[audiohubd] {message} (session={} db={:.2} attempt={}/{})",
                        outcome.session_id,
                        outcome.db,
                        flight.attempt,
                        DACP_VOLUME_RETRY_LIMIT
                    );
                }
            }
        }
    }

    /// Returns the newly observed output epoch when a replacement raced the
    /// poll. The caller then takes the ordinary output-change replay path.
    fn poll_native_volume_for_sender(&self, output_epoch: OutputEpochLease) -> Option<u64> {
        // Serialize the read and reverse planning with sender-originated native
        // writes. A level apply performs slider and unmute as two OS calls;
        // observing between them would manufacture a local change that never
        // existed and echo it back over DACP.
        let _serial = lk(&self.volume_apply_lock);
        let should_poll = {
            let state = lk(&self.state);
            let active = state.bus.as_ref().and_then(PcmBus::active_session);
            state
                .volume
                .target
                .is_some_and(|target| active == Some(target.session_id))
                && state.volume.sender_known_system.is_some()
        };
        if !should_poll {
            return None;
        }

        let current = match read_system_volume_for_epoch(&output_epoch, read_system_volume) {
            Ok(EpochRead::Stable(current)) => current,
            Ok(EpochRead::Changed(epoch)) => return Some(epoch),
            Err(error) => {
                dlog!("[audiohubd] AirPlay native volume read failed: {error:#}");
                return None;
            }
        };

        let mut state = lk(&self.state);
        if !output_epoch.is_current() {
            return Some(output_epoch.observed());
        }
        let generation = state.generation;
        let active = state.bus.as_ref().and_then(PcmBus::active_session);
        let event_control = state
            .runtime
            .as_ref()
            .and_then(AirPlayRuntime::receiver_volume_control_snapshot)
            .filter(|control| active == Some(control.session_id()) && control.is_current());
        let dacp_route = state
            .runtime
            .as_ref()
            .and_then(AirPlayRuntime::active_remote_control_snapshot)
            .filter(|remote| active == Some(remote.info().session_id) && remote.is_current())
            .and_then(|remote| {
                let endpoint = state
                    .mdns
                    .as_ref()
                    .and_then(|mdns| mdns.dacp_endpoint(remote.info()))?;
                Some((remote, endpoint))
            });
        let (remote_control, endpoint) = match dacp_route {
            Some((remote, endpoint)) => (Some(remote), Some(endpoint)),
            None => (None, None),
        };
        if event_control.is_none() && remote_control.is_none() {
            return None;
        }
        let preserve_muted_scalar = event_control.is_some();
        if state
            .volume
            .settle_remote_if_converged_for_route(active, current, preserve_muted_scalar)
        {
            // A failed reverse write is no longer a pending user action once
            // the native endpoint naturally returns to the value the sender
            // already represents.
            state.dacp_warning = None;
            return None;
        }
        let serial = self.dacp_serial.fetch_add(1, Ordering::Relaxed).max(1);
        let flight =
            state
                .volume
                .plan_remote_for_route(active, current, serial, preserve_muted_scalar)?;
        if !output_epoch.is_current() {
            return Some(output_epoch.observed());
        }
        let job = DacpJob {
            serial,
            generation,
            session_id: active.expect("reverse volume plan requires an active session"),
            endpoint,
            remote_control,
            event_control,
            output_epoch,
            db: flight.db,
            scalar: current.scalar,
            muted: current.muted,
            write_gate: flight.write_gate.clone(),
        };
        match self.dacp.try_send(job) {
            Ok(()) => state.volume.remote_queued(flight),
            Err(DacpTrySendError::Full) => {
                // Latest native read wins: leave the state unarmed and retry
                // after the one in-flight network request has completed.
            }
            Err(DacpTrySendError::Disconnected) => {
                state.dacp_warning = Some("AirPlay 反向音量线程已停止".to_string());
            }
        }
        None
    }

    /// A mixer-owned reader that follows runtime replacements. Reusing a raw
    /// `PcmReader` would leave local playback attached to the old bus forever
    /// after a name/password change restarted the receiver.
    pub(crate) fn local_reader(self: &Arc<Self>) -> LocalReader {
        LocalReader {
            controller: Arc::clone(self),
            bus_epoch: 0,
            reader: None,
        }
    }

    fn bus_snapshot(&self) -> (u64, Option<PcmBus>) {
        // The epoch is bumped while holding `state`, so seeing the same value
        // on both sides proves this clone corresponds to that epoch.
        loop {
            let before = self.bus_epoch.load(Ordering::Acquire);
            let bus = lk(&self.state).bus.clone();
            let after = self.bus_epoch.load(Ordering::Acquire);
            if before == after {
                return (after, bus);
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn event_thread_for_test(&self) -> bool {
        lk(&self.state).event_thread.is_some()
    }
}

pub(crate) fn reconcile(inner: &Arc<DaemonInner>, secret_changed: bool) {
    let stored = lk(&inner.settings).clone();
    let identity = inner.identity();
    let name = effective_name(&stored.airplay_name, &identity.name);
    let mac = airplay_mac_from_fingerprint(&identity.fingerprint)
        .expect("LocalIdentity fingerprints are always 16 lowercase hex digits");
    let share_mode = crate::haldev::effective_mode(inner).serves_peers();
    let mut effective_enabled = stored.airplay_enabled && share_mode;
    let mut stopped_reason =
        (stored.airplay_enabled && !share_mode).then(|| MODE_REASON.to_string());
    let password = match crate::settings::load_airplay_password(&inner.cfg_dir) {
        Ok(password) => password,
        Err(error) => {
            // A secret that exists but cannot be read must never turn a
            // protected receiver into an open receiver. Keep the user's wish,
            // stop the actual listener, and expose the actionable failure.
            if effective_enabled {
                effective_enabled = false;
                stopped_reason = Some(format!("AirPlay 密码读取失败：{error:#}"));
            }
            None
        }
    };
    inner.airplay.reconcile(ReconcileInput {
        effective_enabled,
        effective_name: name,
        mac,
        password,
        secret_changed,
        stopped_reason,
    });
}

/// Derive a stable, per-installation locally-administered unicast identifier
/// from the daemon's persisted Ed25519 fingerprint. Protocol discovery and
/// authentication use this as receiver identity rather than as a
/// network-interface discovery hint.
fn airplay_mac_from_fingerprint(fingerprint: &str) -> Option<[u8; 6]> {
    if fingerprint.len() < 12 {
        return None;
    }
    let mut mac = [0u8; 6];
    for (index, byte) in mac.iter_mut().enumerate() {
        let start = index * 2;
        *byte = u8::from_str_radix(&fingerprint[start..start + 2], 16).ok()?;
    }
    // IEEE bit 0 = multicast (must be clear), bit 1 = locally administered
    // (must be set because this is not a hardware-assigned address).
    mac[0] = (mac[0] | 0x02) & 0xfe;
    Some(mac)
}

/// Name the sender really sees. An AirPlay 2 DNS label is at most 63 octets;
/// truncating by Unicode scalar count would still let 48 CJK characters become
/// 144 octets and be truncated elsewhere. Both runtime config and
/// `settings.get.airplay_effective_name` call this one helper, so the UI never
/// promises a longer name than the picker receives.
pub(crate) fn effective_name(custom: &str, daemon_name: &str) -> String {
    let requested = if custom.trim().is_empty() {
        format!("AudioHub — {daemon_name}")
    } else {
        custom.trim().to_string()
    };
    let fitted = utf8_prefix(&requested, AIRPLAY_PICKER_NAME_MAX_BYTES)
        .trim()
        .to_string();
    if fitted.is_empty() {
        "AudioHub".to_string()
    } else {
        fitted
    }
}

fn utf8_prefix(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn authoritative_volume_target(
    snapshot: Option<SenderVolumeSnapshot>,
    active_session: Option<u64>,
) -> Option<(SessionVolume, u64)> {
    let active_session = active_session?;
    let snapshot = snapshot
        .filter(|snapshot| snapshot.session_id == active_session && snapshot.db.is_finite())?;
    Some((
        SessionVolume {
            session_id: active_session,
            db: snapshot.db,
        },
        snapshot.revision,
    ))
}

fn session_view(session: RuntimeSessionInfo, now_unix_ms: u64) -> AirPlaySessionInfo {
    debug_assert_eq!(session.protocol, Protocol::AirPlay2);
    AirPlaySessionInfo {
        id: session.id,
        protocol: "airplay2".to_string(),
        peer: session
            .peer
            .map(|peer| peer.to_string())
            .unwrap_or_else(|| "unknown".to_string()),
        sample_rate: session.sample_rate,
        channels: session.channels as u16,
        paused: session.paused,
        title: session.title,
        artist: session.artist,
        album: session.album,
        connected_ms: now_unix_ms.saturating_sub(session.started_unix_ms),
    }
}

/// Reader used by the local mixer. It re-subscribes after every runtime
/// generation change and otherwise remains an independent bus cursor.
pub(crate) struct LocalReader {
    controller: Arc<AirPlayController>,
    bus_epoch: u64,
    reader: Option<PcmReader>,
}

impl LocalReader {
    pub(crate) fn read_into(&mut self, out: &mut [f32]) -> PcmRead {
        let current_epoch = self.controller.bus_epoch.load(Ordering::Acquire);
        if current_epoch != self.bus_epoch {
            let (bus_epoch, bus) = self.controller.bus_snapshot();
            self.bus_epoch = bus_epoch;
            self.reader = bus.map(|bus| bus.subscribe());
        }
        match self.reader.as_mut() {
            Some(reader) => reader.read_into(out),
            None => {
                out.fill(0.0);
                PcmRead {
                    copied: 0,
                    silence: out.len(),
                    dropped: 0,
                    session_id: None,
                    epoch: 0,
                }
            }
        }
    }
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

// ----------------------------------------------------------- system volume

#[derive(Debug, Clone, Copy, PartialEq)]
enum VolumePlan {
    Level { scalar: f32 },
    Mute,
}

fn volume_plan(db: f32) -> Result<VolumePlan> {
    if !db.is_finite() {
        return Err(anyhow!("AirPlay volume dB must be finite"));
    }
    if db <= AIRPLAY_VOLUME_MUTE_DB {
        return Ok(VolumePlan::Mute);
    }
    Ok(VolumePlan::Level {
        scalar: airplay_db_to_system_scalar(db),
    })
}

trait VolumeBackend {
    fn set_scalar(&mut self, scalar: f32) -> Result<f32>;
    fn set_mute(&mut self, muted: bool) -> Result<()>;
    fn read_state(&mut self) -> Result<SystemVolumeState>;
}

struct SystemVolume;

impl VolumeBackend for SystemVolume {
    fn set_scalar(&mut self, scalar: f32) -> Result<f32> {
        audiohub_core::volume::set_default_output_volume(scalar)?;
        Ok(self.read_state()?.scalar)
    }

    fn set_mute(&mut self, muted: bool) -> Result<()> {
        audiohub_core::volume::set_default_output_mute(muted)
    }

    fn read_state(&mut self) -> Result<SystemVolumeState> {
        let state = audiohub_core::volume::get_default_output_volume()
            .context("read back default output system slider")?;
        if !state.adjustable {
            return Err(anyhow!("default output has no writable system slider"));
        }
        if !state.scalar.is_finite() {
            return Err(anyhow!(
                "default output returned a non-finite system slider"
            ));
        }
        Ok(SystemVolumeState {
            scalar: state.scalar.clamp(0.0, 1.0),
            muted: state.muted,
        })
    }
}

fn apply_volume_with(backend: &mut impl VolumeBackend, db: f32) -> Result<f32> {
    match volume_plan(db)? {
        VolumePlan::Mute => {
            // Native Music uses -144 dB for the visible zero-percent endpoint,
            // not just for a separate mute button. Move the receiver's visible
            // slider to zero as well as muting it; otherwise sender 0% leaves
            // (for example) a 50% system slider behind and violates AudioHub's
            // direct-sync contract. A receiver-originated mute is different:
            // its reflected -144 dB RTSP value is consumed by VolumeSync as a
            // DACP echo before it reaches this function, preserving the local
            // slider for an explicit native mute/unmute cycle.
            let scalar_setter = backend.set_scalar(0.0);
            let mute_setter = backend.set_mute(true);
            let state = backend
                .read_state()
                .context("read system output state after zero and mute")?;
            if state.scalar > SYSTEM_VOLUME_SCALAR_EPSILON {
                return if let Err(error) = scalar_setter {
                    Err(error).context("set default output slider to zero")
                } else {
                    Err(anyhow!(
                        "default output volume setter succeeded without moving the endpoint to zero"
                    ))
                };
            }
            if !state.muted {
                return if let Err(error) = mute_setter {
                    Err(error).context("mute default output")
                } else {
                    Err(anyhow!(
                        "default output mute setter succeeded without muting the endpoint"
                    ))
                };
            }
            Ok(state.scalar)
        }
        VolumePlan::Level { scalar } => {
            // Set the level before unmuting, avoiding a transient at the old
            // level. The backend performs the mandatory scalar readback.
            let _applied = backend
                .set_scalar(scalar)
                .context("set system output slider from AirPlay volume")?;
            let setter = backend.set_mute(false);
            // Some valid CoreAudio outputs expose a writable volume scalar but
            // no writable mute property. Setter errors are acceptable only
            // when readback proves the complete requested postcondition; a
            // silent successful no-op is rejected by the same check.
            let state = backend
                .read_state()
                .context("read system output state after level and unmute")?;
            if state.muted {
                return if let Err(error) = setter {
                    Err(error).context("unmute default output")
                } else {
                    Err(anyhow!(
                        "default output unmute setter succeeded without unmuting the endpoint"
                    ))
                };
            }
            Ok(state.scalar)
        }
    }
}

fn apply_system_volume(db: f32) -> Result<f32> {
    apply_volume_with(&mut SystemVolume, db)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EpochRead<T> {
    Stable(T),
    Changed(u64),
}

fn read_system_volume_for_epoch<T>(
    output_epoch: &OutputEpochLease,
    read: impl FnOnce() -> Result<T>,
) -> Result<EpochRead<T>> {
    if !output_epoch.is_current() {
        return Ok(EpochRead::Changed(output_epoch.observed()));
    }
    let result = read();
    // Check the lease before propagating a native read error: a replacement is
    // actionable (replay the sender value) and is commonly why the old handle
    // failed in the first place.
    if !output_epoch.is_current() {
        return Ok(EpochRead::Changed(output_epoch.observed()));
    }
    result.map(EpochRead::Stable)
}

fn read_system_volume() -> Result<SystemVolumeState> {
    let state = audiohub_core::volume::get_default_output_volume()
        .context("read default output system slider volume")?;
    if !state.adjustable {
        return Err(anyhow!(
            "default output has no writable system slider volume"
        ));
    }
    if !state.scalar.is_finite() {
        return Err(anyhow!(
            "default output returned a non-finite system slider volume"
        ));
    }
    Ok(SystemVolumeState {
        scalar: state.scalar.clamp(0.0, 1.0),
        muted: state.muted,
    })
}

/// Event-channel `dvlc` carries the slider and mute flag independently, so a
/// muted slider movement remains observable and must be synchronized. Legacy
/// DACP can express only the `-144 dB` mute sentinel and deliberately ignores
/// scalar changes while both sides are muted to avoid an endless no-op loop.
fn same_reverse_volume(
    left: SystemVolumeState,
    right: SystemVolumeState,
    preserve_muted_scalar: bool,
) -> bool {
    if left.muted != right.muted {
        return false;
    }
    if left.muted && !preserve_muted_scalar {
        return true;
    }
    left.scalar.is_finite()
        && right.scalar.is_finite()
        && (left.scalar - right.scalar).abs() <= SYSTEM_VOLUME_SCALAR_EPSILON
}

fn same_dacp_echo_volume(reported_db: f32, requested_db: f32) -> bool {
    let reported_muted = reported_db <= -144.0;
    let requested_muted = requested_db <= -144.0;
    if reported_muted || requested_muted {
        return reported_muted == requested_muted;
    }
    reported_db.is_finite()
        && requested_db.is_finite()
        && (reported_db - requested_db).abs() <= DACP_ECHO_DB_EPSILON
}

#[cfg(test)]
fn send_dacp_volume(endpoint: SocketAddr, active_remote: &str, db: f32) -> Result<()> {
    let write_gate = DacpWriteGate::new();
    if send_dacp_volume_guarded(endpoint, active_remote, db, &write_gate, || true)? {
        Ok(())
    } else {
        Err(anyhow!("DACP volume request was superseded"))
    }
}

/// Returns `Ok(false)` when a newer control decision invalidated this job
/// before any HTTP bytes were written. The second check closes the potentially
/// long `connect_timeout` window; once `write_all` starts, the outcome path
/// repairs a successful stale request instead of pretending TCP is cancellable.
fn send_dacp_volume_guarded(
    endpoint: SocketAddr,
    active_remote: &str,
    db: f32,
    write_gate: &DacpWriteGate,
    still_current: impl Fn() -> bool,
) -> Result<bool> {
    if active_remote.is_empty()
        || active_remote.len() > 20
        || !active_remote.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(anyhow!("invalid Active-Remote token"));
    }
    if !db.is_finite() || !((-144.0..=0.0).contains(&db)) {
        return Err(anyhow!("invalid AirPlay volume dB {db}"));
    }
    if !still_current() {
        return Ok(false);
    }

    // A socket read timeout is per syscall; a peer dripping one byte just
    // before each timeout could otherwise retain the single worker for hours.
    // One absolute deadline bounds connect + write + the complete response.
    let deadline = Instant::now() + DACP_IO_TIMEOUT;
    let mut stream = TcpStream::connect_timeout(&endpoint, dacp_remaining(deadline)?)
        .with_context(|| format!("connect sender DACP service at {endpoint}"))?;
    stream.set_nodelay(true).context("set DACP TCP_NODELAY")?;
    let request = format!(
        "GET /ctrl-int/1/setproperty?dmcp.device-volume={db:.6} HTTP/1.1\r\n\
         Host: {endpoint}\r\n\
         Active-Remote: {active_remote}\r\n\
         Connection: close\r\n\r\n"
    );
    if !write_gate.commit(&still_current) {
        return Ok(false);
    }
    let mut written = 0;
    while written < request.len() {
        if !still_current() {
            if written == 0 {
                return Ok(false);
            }
            return Err(anyhow!("DACP volume request revoked during write"));
        }
        let remaining = dacp_remaining(deadline)?;
        stream
            .set_write_timeout(Some(remaining.min(DACP_GUARD_POLL)))
            .context("set DACP write timeout")?;
        match stream.write(&request.as_bytes()[written..]) {
            Ok(0) => return Err(anyhow!("DACP request socket closed during write")),
            Ok(count) => written += count,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) && Instant::now() < deadline =>
            {
                continue;
            }
            Err(error) => return Err(error).context("write DACP volume request"),
        }
    }
    write_gate.mark_full_request_written();

    let mut response = Vec::with_capacity(512);
    let mut chunk = [0u8; 1024];
    while !response.windows(4).any(|window| window == b"\r\n\r\n") {
        if !still_current() {
            return Err(anyhow!("DACP volume request revoked after write"));
        }
        if response.len() == DACP_RESPONSE_LIMIT {
            return Err(anyhow!(
                "DACP response headers exceed {DACP_RESPONSE_LIMIT} bytes"
            ));
        }
        let available = chunk.len().min(DACP_RESPONSE_LIMIT - response.len());
        let remaining = dacp_remaining(deadline)?;
        stream
            .set_read_timeout(Some(remaining.min(DACP_GUARD_POLL)))
            .context("set DACP read timeout")?;
        let read = match stream.read(&mut chunk[..available]) {
            Ok(read) => read,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) && Instant::now() < deadline =>
            {
                continue;
            }
            Err(error) => return Err(error).context("read DACP volume response"),
        };
        if read == 0 {
            break;
        }
        response.extend_from_slice(&chunk[..read]);
    }
    if !response.windows(4).any(|window| window == b"\r\n\r\n") {
        return Err(anyhow!("DACP response ended before complete headers"));
    }
    let status_line = response
        .split(|byte| *byte == b'\n')
        .next()
        .and_then(|line| std::str::from_utf8(line).ok())
        .map(str::trim_end)
        .ok_or_else(|| anyhow!("DACP response has no valid HTTP status line"))?;
    let mut parts = status_line.split_whitespace();
    let version = parts.next().unwrap_or_default();
    let code = parts
        .next()
        .and_then(|value| value.parse::<u16>().ok())
        .ok_or_else(|| anyhow!("DACP response has an invalid HTTP status"))?;
    if !version.starts_with("HTTP/1.") || !(200..300).contains(&code) {
        return Err(anyhow!("DACP volume request returned HTTP {code}"));
    }
    Ok(true)
}

fn dacp_remaining(deadline: Instant) -> Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| anyhow!("DACP volume request exceeded {DACP_IO_TIMEOUT:?} total deadline"))
}

// --------------------------------------------------------------- mDNS guard

struct AirPlayMdnsGuard {
    daemon: ServiceDaemon,
    fullnames: Vec<String>,
    dacp_services: Arc<Mutex<HashMap<String, ServiceInfo>>>,
    dacp_revision: Arc<AtomicU64>,
    dacp_stop: Arc<AtomicBool>,
    dacp_thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for AirPlayMdnsGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AirPlayMdnsGuard")
            .field("fullnames", &self.fullnames)
            .field("dacp_service_count", &lk(&self.dacp_services).len())
            .field("dacp_revision", &self.dacp_revision.load(Ordering::Acquire))
            .finish()
    }
}

impl AirPlayMdnsGuard {
    fn register(services: &[MdnsService]) -> Result<Self> {
        if services.is_empty() {
            return Err(anyhow!("receiver returned no DNS-SD services"));
        }
        let host = local_host_name();
        let mut registrations = Vec::with_capacity(services.len());
        let mut protocols = HashSet::with_capacity(services.len());
        for service in services {
            if service.protocol != Protocol::AirPlay2 {
                return Err(anyhow!(
                    "receiver returned unsupported AirPlay protocol {:?}",
                    service.protocol
                ));
            }
            if service.service_type != "_airplay._tcp.local." {
                return Err(anyhow!(
                    "AirPlay protocol {:?} returned mismatched DNS-SD type {:?}",
                    service.protocol,
                    service.service_type,
                ));
            }
            if !protocols.insert(service.protocol) {
                return Err(anyhow!(
                    "receiver returned duplicate {:?} DNS-SD service",
                    service.protocol
                ));
            }
            let props = txt_pairs(&service.txt_records)?;
            let info = ServiceInfo::new(
                &service.service_type,
                &service.instance_name,
                &host,
                "",
                service.port,
                props.as_slice(),
            )
            .context("build AirPlay DNS-SD service")?
            .enable_addr_auto();
            registrations.push((info.get_fullname().to_string(), info));
        }

        let daemon = ServiceDaemon::new().context("start AirPlay mDNS daemon")?;
        // The native receiver currently binds an IPv4 control socket and its
        // PTP/media path is likewise IPv4-only. Keep address auto-refresh, but
        // restrict this dedicated daemon before registering or browsing so it
        // cannot publish an unreachable AAAA record (or resolve an IPv6-only
        // DACP route for a control session that necessarily arrived over IPv4).
        if let Err(error) = daemon.disable_interface(IfKind::IPv6) {
            let _ = daemon.shutdown();
            return Err(error).context("restrict AirPlay mDNS to reachable IPv4 interfaces");
        }
        let monitor = match daemon.monitor() {
            Ok(monitor) => monitor,
            Err(error) => {
                let _ = daemon.shutdown();
                return Err(error).context("monitor AirPlay mDNS daemon");
            }
        };
        let dacp_events = match daemon.browse(DACP_SERVICE_TYPE) {
            Ok(events) => events,
            Err(error) => {
                let _ = daemon.shutdown();
                return Err(error).context("browse sender DACP services");
            }
        };
        let dacp_services = Arc::new(Mutex::new(HashMap::new()));
        let dacp_revision = Arc::new(AtomicU64::new(1));
        let dacp_stop = Arc::new(AtomicBool::new(false));
        let thread_services = Arc::clone(&dacp_services);
        let thread_revision = Arc::clone(&dacp_revision);
        let thread_stop = Arc::clone(&dacp_stop);
        let dacp_thread = match thread::Builder::new()
            .name("ahb-airplay-dacp-mdns".to_string())
            .spawn(move || {
                track_dacp_services(dacp_events, thread_services, thread_revision, thread_stop)
            }) {
            Ok(thread) => thread,
            Err(error) => {
                let _ = daemon.stop_browse(DACP_SERVICE_TYPE);
                let _ = daemon.shutdown();
                return Err(error).context("spawn AirPlay DACP mDNS event pump");
            }
        };
        let mut guard = Self {
            daemon,
            fullnames: Vec::with_capacity(registrations.len()),
            dacp_services,
            dacp_revision,
            dacp_stop,
            dacp_thread: Some(dacp_thread),
        };
        for (fullname, info) in registrations {
            guard
                .daemon
                .register(info)
                .context("register AirPlay DNS-SD service")?;
            guard.fullnames.push(fullname);
        }

        let pending: HashSet<String> = guard.fullnames.iter().cloned().collect();
        let mut confirmed = HashSet::new();
        let deadline = std::time::Instant::now() + MDNS_CONFIRM_TIMEOUT;
        while confirmed.len() != pending.len() {
            let now = std::time::Instant::now();
            if now >= deadline {
                return Err(anyhow!(
                    "AirPlay mDNS announce not confirmed within {MDNS_CONFIRM_TIMEOUT:?}"
                ));
            }
            match monitor.recv_timeout(deadline - now) {
                Ok(DaemonEvent::Announce(name, _)) if pending.contains(&name) => {
                    confirmed.insert(name);
                }
                Ok(DaemonEvent::Error(error)) => {
                    return Err(anyhow!("AirPlay mDNS daemon error: {error}"));
                }
                Ok(_) | Err(_) => {}
            }
        }
        Ok(guard)
    }

    fn dacp_endpoint(&self, remote: &RemoteControlInfo) -> Option<DacpEndpointSnapshot> {
        loop {
            let before = self.dacp_revision.load(Ordering::Acquire);
            let address = dacp_endpoint_from_services(&lk(&self.dacp_services), remote)?;
            let after = self.dacp_revision.load(Ordering::Acquire);
            if before == after {
                return Some(DacpEndpointSnapshot {
                    address,
                    revision: after,
                    current_revision: Arc::clone(&self.dacp_revision),
                });
            }
        }
    }
}

impl Drop for AirPlayMdnsGuard {
    fn drop(&mut self) {
        self.dacp_stop.store(true, Ordering::Release);
        let _ = self.daemon.stop_browse(DACP_SERVICE_TYPE);
        if let Some(thread) = self.dacp_thread.take() {
            let _ = thread.join();
        }
        for fullname in &self.fullnames {
            if let Ok(done) = self.daemon.unregister(fullname) {
                let _ = done.recv_timeout(Duration::from_millis(500));
            }
        }
        let _ = self.daemon.shutdown();
    }
}

fn track_dacp_services(
    events: Receiver<ServiceEvent>,
    services: Arc<Mutex<HashMap<String, ServiceInfo>>>,
    revision: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
) {
    while !stop.load(Ordering::Acquire) {
        match events.recv_timeout(DACP_BROWSE_POLL) {
            Ok(event) => update_dacp_service_cache(&mut lk(&services), &revision, event),
            Err(_) if events.is_disconnected() => break,
            Err(_) => {}
        }
    }
}

fn update_dacp_service_cache(
    services: &mut HashMap<String, ServiceInfo>,
    revision: &AtomicU64,
    event: ServiceEvent,
) {
    let mut changed = false;
    match event {
        ServiceEvent::ServiceResolved(info) => {
            let fullname = info.get_fullname().to_string();
            if services.len() >= DACP_SERVICE_CACHE_LIMIT && !services.contains_key(&fullname) {
                if let Some(evicted) = services.keys().next().cloned() {
                    services.remove(&evicted);
                    changed = true;
                }
            }
            changed |= services
                .get(&fullname)
                .is_none_or(|previous| !same_dacp_service_route(previous, &info));
            services.insert(fullname, info);
        }
        ServiceEvent::ServiceRemoved(_, fullname) => {
            changed = services.remove(&fullname).is_some();
        }
        _ => {}
    }
    if changed {
        revision.fetch_add(1, Ordering::Release);
    }
}

fn same_dacp_service_route(left: &ServiceInfo, right: &ServiceInfo) -> bool {
    left.get_port() == right.get_port() && left.get_addresses() == right.get_addresses()
}

fn dacp_id_u64(value: &str) -> Option<u64> {
    (!value.is_empty() && value.len() <= 16 && value.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| u64::from_str_radix(value, 16).ok())
        .flatten()
}

fn service_dacp_id(fullname: &str) -> Option<u64> {
    const PREFIX: &str = "itunes_ctrl_";
    const SUFFIX: &str = "._dacp._tcp.local.";
    let lower = fullname.to_ascii_lowercase();
    let id = lower.strip_prefix(PREFIX)?.strip_suffix(SUFFIX)?;
    dacp_id_u64(id)
}

fn dacp_endpoint_from_services(
    services: &HashMap<String, ServiceInfo>,
    remote: &RemoteControlInfo,
) -> Option<SocketAddr> {
    let wanted = dacp_id_u64(&remote.dacp_id)?;
    services
        .values()
        .find_map(|service| dacp_service_endpoint(service, remote, wanted))
}

fn dacp_service_endpoint(
    service: &ServiceInfo,
    remote: &RemoteControlInfo,
    wanted: u64,
) -> Option<SocketAddr> {
    (service_dacp_id(service.get_fullname()) == Some(wanted)
        && service.get_port() != 0
        && service
            .get_addresses()
            .iter()
            .copied()
            .map(normalize_ip)
            .any(|address| address == normalize_ip(remote.peer.ip())))
    .then(|| {
        let mut endpoint = normalize_socket_addr(remote.peer);
        endpoint.set_port(service.get_port());
        endpoint
    })
}

fn normalize_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(ip) => ip
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(ip)),
        IpAddr::V4(_) => ip,
    }
}

fn normalize_socket_addr(addr: SocketAddr) -> SocketAddr {
    match addr {
        SocketAddr::V6(v6) => v6
            .ip()
            .to_ipv4_mapped()
            .map(|ip| SocketAddr::new(IpAddr::V4(ip), v6.port()))
            .unwrap_or(SocketAddr::V6(v6)),
        SocketAddr::V4(_) => addr,
    }
}

fn local_host_name() -> String {
    // `identity::local_hostname()` is deliberately the human ComputerName on
    // macOS (it may be "客厅 Mac"). An SRV target is a DNS host label instead:
    // use the OS hostname source, then make its label constraints explicit so
    // the same code is registerable through mdns-sd on macOS and Windows.
    #[cfg(windows)]
    let raw = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "audiohub-host".into());
    #[cfg(not(windows))]
    let raw = std::process::Command::new("hostname")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .unwrap_or_else(|| "audiohub-host".into());
    let base = raw.trim_end_matches('.');
    let base = base.strip_suffix(".local").unwrap_or(base);
    format!("{}.local.", dns_host_label(base))
}

fn dns_host_label(value: &str) -> String {
    let mut label = String::with_capacity(value.len().min(DNS_LABEL_MAX_BYTES));
    let mut separator = false;
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            if separator && !label.is_empty() && label.len() < DNS_LABEL_MAX_BYTES {
                label.push('-');
            }
            separator = false;
            if label.len() < DNS_LABEL_MAX_BYTES {
                label.push(ch.to_ascii_lowercase());
            }
        } else {
            separator = !label.is_empty();
        }
        if label.len() == DNS_LABEL_MAX_BYTES {
            break;
        }
    }
    while label.ends_with('-') {
        label.pop();
    }
    if label.is_empty() {
        "audiohub-host".to_string()
    } else {
        label
    }
}

fn txt_pairs(records: &[String]) -> Result<Vec<(String, String)>> {
    records
        .iter()
        .map(|record| {
            let (key, value) = record
                .split_once('=')
                .ok_or_else(|| anyhow!("invalid AirPlay TXT record {record:?}"))?;
            if key.is_empty() {
                return Err(anyhow!("AirPlay TXT record has an empty key"));
            }
            Ok((key.to_string(), value.to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct PortTestDir(PathBuf);

    impl PortTestDir {
        fn new(label: &str) -> Self {
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let sequence = AIRPLAY2_PORT_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
            Self(std::env::temp_dir().join(format!(
                "audiohub-airplay-port-{label}-{}-{stamp}-{sequence}",
                std::process::id()
            )))
        }

        fn port_path(&self) -> PathBuf {
            self.0.join(AIRPLAY2_PORT_FILE)
        }

        fn runtime_config(&self) -> AirPlayConfig {
            let mut config = AirPlayConfig::new("AudioHub port test");
            config.mac = Some([0x02, 0, 0, 0, 0, 1]);
            config.airplay2_identity_path = Some(self.0.join("airplay2-identity"));
            config.use_ephemeral_ptp_ports_for_tests();
            config
        }
    }

    impl Drop for PortTestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn running_port(runtime: &AirPlayRuntime) -> u16 {
        runtime
            .status()
            .airplay2_port
            .filter(|port| *port != 0)
            .expect("test runtime must expose its nonzero control port")
    }

    #[test]
    fn missing_airplay2_port_is_selected_and_persisted() {
        let dir = PortTestDir::new("missing");
        let path = dir.port_path();
        assert_eq!(load_airplay2_port(&path).unwrap(), None);

        let runtime = start_airplay_runtime(dir.runtime_config(), &path).unwrap();
        let port = running_port(&runtime);
        assert_eq!(load_airplay2_port(&path).unwrap(), Some(port));
        runtime.stop().unwrap();
    }

    #[test]
    fn invalid_airplay2_port_is_replaced_after_successful_start() {
        let dir = PortTestDir::new("invalid");
        std::fs::create_dir_all(&dir.0).unwrap();
        let path = dir.port_path();
        std::fs::write(&path, b"not-a-port\n").unwrap();
        assert_eq!(
            load_airplay2_port(&path).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );

        let runtime = start_airplay_runtime(dir.runtime_config(), &path).unwrap();
        let port = running_port(&runtime);
        assert_eq!(load_airplay2_port(&path).unwrap(), Some(port));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), format!("{port}\n"));
        runtime.stop().unwrap();
    }

    #[test]
    fn airplay2_port_persistence_atomically_replaces_the_record() {
        let dir = PortTestDir::new("persist");
        let path = dir.port_path();
        persist_airplay2_port(&path, 49_152).unwrap();
        assert_eq!(load_airplay2_port(&path).unwrap(), Some(49_152));
        persist_airplay2_port(&path, 49_153).unwrap();
        assert_eq!(load_airplay2_port(&path).unwrap(), Some(49_153));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "49153\n");
        assert!(std::fs::read_dir(&dir.0).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")
        }));
    }

    #[test]
    fn stale_airplay2_port_staging_collision_is_skipped_without_deletion() {
        let dir = PortTestDir::new("stale-stage");
        std::fs::create_dir_all(&dir.0).unwrap();
        let collision = 41;
        let fresh = 42;
        let stale = dir.0.join(format!(
            ".{AIRPLAY2_PORT_FILE}.{}.{}.tmp",
            std::process::id(),
            collision
        ));
        std::fs::write(&stale, b"crash residue").unwrap();
        let mut sequences = [collision, fresh].into_iter();

        persist_airplay2_port_with_sequences(&dir.port_path(), 49_154, || {
            sequences.next().expect("allocator used at most two names")
        })
        .unwrap();
        assert_eq!(load_airplay2_port(&dir.port_path()).unwrap(), Some(49_154));
        assert_eq!(
            std::fs::read(&stale).unwrap(),
            b"crash residue",
            "a create_new collision belongs to an older process incarnation"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_long_config_path_atomically_replaces_airplay2_port() {
        let dir = PortTestDir::new("windows-long-path");
        let mut long_dir = dir.0.clone();
        for index in 0..4 {
            long_dir.push(format!("segment-{index}-{}", "x".repeat(72)));
        }
        std::fs::create_dir_all(&long_dir).unwrap();
        let path = long_dir.join(AIRPLAY2_PORT_FILE);
        assert!(path.as_os_str().len() > 260);

        persist_airplay2_port(&path, 49_152).unwrap();
        persist_airplay2_port(&path, 49_153).unwrap();
        assert_eq!(load_airplay2_port(&path).unwrap(), Some(49_153));
    }

    #[test]
    fn airplay2_runtime_reuses_its_persisted_control_port() {
        let dir = PortTestDir::new("reuse");
        let path = dir.port_path();
        let first = start_airplay_runtime(dir.runtime_config(), &path).unwrap();
        let first_port = running_port(&first);
        first.stop().unwrap();

        let second = start_airplay_runtime(dir.runtime_config(), &path).unwrap();
        assert_eq!(running_port(&second), first_port);
        assert_eq!(load_airplay2_port(&path).unwrap(), Some(first_port));
        second.stop().unwrap();
    }

    #[test]
    fn accepted_control_connection_does_not_rotate_port_on_immediate_restart() {
        let dir = PortTestDir::new("accepted-restart");
        let path = dir.port_path();
        let first = start_airplay_runtime(dir.runtime_config(), &path).unwrap();
        let first_port = running_port(&first);

        let mut sender = TcpStream::connect(("127.0.0.1", first_port)).unwrap();
        sender
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        for (request, status) in [
            (
                b"OPTIONS * RTSP/1.0\r\nCSeq: 1\r\nContent-Length: 0\r\n\r\n".as_slice(),
                b"RTSP/1.0 200".as_slice(),
            ),
            // A duplicate CSeq is rejected with close-after-response. This
            // makes the receiver, rather than the test client, actively close
            // a real accepted connection before the listener is restarted.
            (
                b"OPTIONS * RTSP/1.0\r\nCSeq: 2\r\nCSeq: 3\r\nContent-Length: 0\r\n\r\n".as_slice(),
                b"RTSP/1.0 400".as_slice(),
            ),
        ] {
            sender.write_all(request).unwrap();
            let mut response = Vec::new();
            let mut chunk = [0u8; 512];
            while !response.windows(4).any(|window| window == b"\r\n\r\n") {
                let count = sender.read(&mut chunk).unwrap();
                assert_ne!(count, 0, "receiver closed before its RTSP response");
                response.extend_from_slice(&chunk[..count]);
            }
            assert!(response.starts_with(status));
        }
        let mut eof = [0u8; 1];
        assert_eq!(
            sender.read(&mut eof).unwrap(),
            0,
            "receiver-authored RTSP rejection must close"
        );
        drop(sender);
        first.stop().unwrap();

        let second = start_airplay_runtime(dir.runtime_config(), &path).unwrap();
        assert_eq!(running_port(&second), first_port);
        assert_eq!(load_airplay2_port(&path).unwrap(), Some(first_port));
        second.stop().unwrap();
    }

    #[test]
    fn occupied_persisted_airplay2_port_falls_back_and_replaces_record() {
        let dir = PortTestDir::new("conflict");
        let path = dir.port_path();
        let occupied = std::net::TcpListener::bind(("0.0.0.0", 0)).unwrap();
        let occupied_port = occupied.local_addr().unwrap().port();
        persist_airplay2_port(&path, occupied_port).unwrap();

        let runtime = start_airplay_runtime(dir.runtime_config(), &path).unwrap();
        let replacement = running_port(&runtime);
        assert_ne!(replacement, occupied_port);
        assert_eq!(load_airplay2_port(&path).unwrap(), Some(replacement));
        runtime.stop().unwrap();
        drop(occupied);
    }

    #[test]
    fn only_explicit_control_bind_errors_are_rotation_eligible() {
        for kind in [io::ErrorKind::AddrInUse, io::ErrorKind::PermissionDenied] {
            let later_startup = io::Error::new(kind, "identity or PTP startup failed");
            assert!(persisted_port_unavailable(&later_startup));
            assert!(!is_control_port_bind_failure(&later_startup));
        }

        let occupied = std::net::TcpListener::bind(("0.0.0.0", 0)).unwrap();
        let dir = PortTestDir::new("bind-classification");
        let mut config = dir.runtime_config();
        config.airplay2_port = occupied.local_addr().unwrap().port();
        let marked = AirPlayRuntime::start(config).unwrap_err();
        assert_eq!(marked.kind(), io::ErrorKind::AddrInUse);
        assert!(is_control_port_bind_failure(&marked));
    }

    #[test]
    fn unreadable_airplay2_port_record_never_starts_or_rotates() {
        let dir = PortTestDir::new("unreadable");
        std::fs::create_dir_all(&dir.0).unwrap();
        let path = dir.port_path();
        std::fs::write(&path, b"49152\n").unwrap();

        let error = start_airplay_runtime_after_port_load(
            dir.runtime_config(),
            &path,
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "simulated sharing or permission failure",
            )),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(std::fs::read(&path).unwrap(), b"49152\n");
        assert!(
            !dir.0.join("airplay2-identity").exists(),
            "an unreadable port record must fail before receiver startup"
        );
    }

    fn full_write_gate() -> DacpWriteGate {
        let gate = DacpWriteGate::new();
        assert!(gate.commit(|| true));
        gate.mark_full_request_written();
        gate
    }

    #[test]
    fn guaranteed_unwritten_event_attempt_can_rearm_the_dacp_write_gate() {
        let gate = DacpWriteGate::new();
        assert!(gate.commit(|| true));
        assert!(gate.retry_after_unwritten(|| true));
        assert!(gate.commit(|| true));
        gate.mark_full_request_written();
        assert!(gate.may_have_written());

        let stale = DacpWriteGate::new();
        assert!(stale.commit(|| true));
        assert!(!stale.retry_after_unwritten(|| false));
        assert!(!stale.may_have_written());
    }

    struct FakeVolume {
        actions: Vec<(String, f32)>,
        scalar: f32,
        muted: bool,
        mute_writable: bool,
        mute_noop: bool,
    }

    impl Default for FakeVolume {
        fn default() -> Self {
            Self {
                actions: Vec::new(),
                scalar: 0.0,
                muted: false,
                mute_writable: true,
                mute_noop: false,
            }
        }
    }

    impl VolumeBackend for FakeVolume {
        fn set_scalar(&mut self, scalar: f32) -> Result<f32> {
            self.actions.push(("scalar".to_string(), scalar));
            self.scalar = (scalar * 100.0).round() / 100.0;
            Ok(self.scalar)
        }

        fn set_mute(&mut self, muted: bool) -> Result<()> {
            self.actions
                .push(("mute".to_string(), if muted { 1.0 } else { 0.0 }));
            if !self.mute_writable {
                return Err(anyhow!("mute property is not writable"));
            }
            if !self.mute_noop {
                self.muted = muted;
            }
            Ok(())
        }

        fn read_state(&mut self) -> Result<SystemVolumeState> {
            Ok(SystemVolumeState {
                scalar: self.scalar,
                muted: self.muted,
            })
        }
    }

    #[test]
    fn airplay_volume_drives_system_slider_then_mute_without_pcm_gain() {
        let mut volume = FakeVolume::default();
        let applied = apply_volume_with(&mut volume, -12.0).unwrap();
        let scalar = 0.6;
        assert_eq!(
            volume.actions,
            vec![("scalar".into(), scalar), ("mute".into(), 0.0)]
        );
        assert_eq!(applied, scalar);

        volume.actions.clear();
        let muted_zero = apply_volume_with(&mut volume, -144.0).unwrap();
        assert_eq!(
            volume.actions,
            vec![("scalar".into(), 0.0), ("mute".into(), 1.0)]
        );
        assert_eq!(muted_zero, 0.0);
    }

    #[test]
    fn level_succeeds_on_an_already_unmuted_output_without_a_mute_property() {
        let mut volume = FakeVolume {
            scalar: 0.25,
            mute_writable: false,
            ..FakeVolume::default()
        };
        assert_eq!(apply_volume_with(&mut volume, -15.0).unwrap(), 0.5);
        assert_eq!(
            volume.actions,
            vec![("scalar".into(), 0.5), ("mute".into(), 0.0)]
        );
        assert!(!volume.muted);

        volume.actions.clear();
        assert!(apply_volume_with(&mut volume, -144.0).is_err());
        assert_eq!(
            volume.actions,
            vec![("scalar".into(), 0.0), ("mute".into(), 1.0)]
        );
        assert_eq!(volume.scalar, 0.0);

        volume.actions.clear();
        volume.muted = true;
        assert!(apply_volume_with(&mut volume, -12.0).is_err());
        assert!(volume.muted);
    }

    #[test]
    fn successful_noop_native_setters_fail_their_volume_postconditions() {
        let mut silent_mute = FakeVolume {
            scalar: 0.5,
            mute_noop: true,
            ..FakeVolume::default()
        };
        assert!(apply_volume_with(&mut silent_mute, -144.0).is_err());
        assert!(!silent_mute.muted);
        assert_eq!(silent_mute.scalar, 0.0);

        let mut silent_unmute = FakeVolume {
            scalar: 0.5,
            muted: true,
            mute_noop: true,
            ..FakeVolume::default()
        };
        assert!(apply_volume_with(&mut silent_unmute, -15.0).is_err());
        assert!(silent_unmute.muted);
    }

    #[test]
    fn airplay_volume_clamps_protocol_range_and_rejects_nan_without_device_io() {
        assert_eq!(
            volume_plan(-90.0).unwrap(),
            VolumePlan::Level { scalar: 0.0 }
        );
        assert_eq!(volume_plan(3.0).unwrap(), VolumePlan::Level { scalar: 1.0 });
        let mut volume = FakeVolume::default();
        assert!(apply_volume_with(&mut volume, f32::NAN).is_err());
        assert!(volume.actions.is_empty());
    }

    #[test]
    fn airplay_slider_db_and_system_scalar_are_exact_inverses() {
        let cases = [
            (0.0, -30.0),
            (0.01, -29.7),
            (0.05, -28.5),
            (0.1, -27.0),
            (0.25, -22.5),
            (0.5, -15.0),
            (0.75, -7.5),
            (1.0, 0.0),
        ];
        for (scalar, db) in cases {
            assert!((system_scalar_to_airplay_db(scalar) - db).abs() < 1e-5);
            assert!((airplay_db_to_system_scalar(db) - scalar).abs() < 1e-5);
            let state = SystemVolumeState::from_sender_db(db);
            assert!(!state.muted);
            assert!((state.scalar - scalar).abs() < 1e-5);
            assert!((state.airplay_db() - db).abs() < 1e-5);
        }
    }

    #[test]
    fn zero_percent_and_explicit_mute_remain_distinct() {
        let zero = SystemVolumeState::from_sender_db(-30.0);
        assert_eq!(zero.scalar, 0.0);
        assert!(!zero.muted);
        assert_eq!(zero.airplay_db(), -30.0);

        let mute = SystemVolumeState::from_sender_db(-144.0);
        assert_eq!(mute.scalar, 0.0);
        assert!(mute.muted);
        assert_eq!(mute.airplay_db(), -144.0);
    }

    #[test]
    fn event_volume_preserves_muted_slider_changes_while_dacp_does_not_loop() {
        let target = SessionVolume {
            session_id: 9,
            db: -144.0,
        };
        let changed_while_muted = SystemVolumeState {
            scalar: 0.2,
            muted: true,
        };

        let mut dacp = VolumeSync::default();
        dacp.remember(target);
        dacp.succeeded(target);
        assert!(dacp
            .plan_remote(Some(target.session_id), changed_while_muted, 1)
            .is_none());

        let mut event = VolumeSync::default();
        event.remember(target);
        event.succeeded(target);
        let flight = event
            .plan_remote_for_route(Some(target.session_id), changed_while_muted, 2, true)
            .expect("dvlc carries scalar and mute independently");
        assert_eq!(flight.desired, changed_while_muted);
        assert_eq!(flight.db, -144.0);
    }

    #[test]
    fn airplay_volume_requires_the_concrete_active_stream() {
        let snapshot = SenderVolumeSnapshot {
            session_id: 7,
            db: -6.0,
            revision: 1,
        };
        assert!(authoritative_volume_target(Some(snapshot), Some(7)).is_some());
        assert_eq!(authoritative_volume_target(None, Some(7)), None);
        assert_eq!(authoritative_volume_target(Some(snapshot), Some(8)), None);
        assert_eq!(authoritative_volume_target(Some(snapshot), None), None);
    }

    #[test]
    fn runtime_snapshot_repairs_a_dropped_final_volume_event() {
        let snapshot = SenderVolumeSnapshot {
            session_id: 7,
            db: -6.0,
            revision: 5,
        };
        let (latest, revision) = authoritative_volume_target(Some(snapshot), Some(7)).unwrap();
        assert_eq!(
            latest,
            SessionVolume {
                session_id: 7,
                db: -6.0,
            }
        );
        assert_eq!(revision, 5);

        let mut sync = VolumeSync::default();
        sync.remember_snapshot(
            SessionVolume {
                session_id: 7,
                db: -18.0,
            },
            4,
        );
        assert_ne!(sync.target, Some(latest));
        assert!(sync.needs_sender_reconcile(latest, revision));
        sync.remember_snapshot(latest, revision);
        assert_eq!(sync.target, Some(latest));
        assert!(!sync.needs_sender_reconcile(latest, revision));

        assert_eq!(authoritative_volume_target(Some(snapshot), Some(8)), None);
    }

    #[test]
    fn stale_session_end_cannot_clear_replacement_volume_state() {
        let old = SessionVolume {
            session_id: 7,
            db: -18.0,
        };
        let new = SessionVolume {
            session_id: 8,
            db: -9.0,
        };
        let mut sync = VolumeSync::default();
        sync.remember(old);
        sync.failed(old, None);
        assert!(sync.remember(new));
        sync.failed(new, None);

        assert!(!sync.ended(Some(old.session_id)));
        assert_eq!(sync.target, Some(new));
        assert_eq!(sync.retry.map(|retry| retry.target), Some(new));

        assert!(sync.ended(Some(new.session_id)));
        assert_eq!(sync.target, None);
        assert_eq!(sync.retry, None);
    }

    #[test]
    fn default_output_replay_requires_the_same_active_session() {
        let target = SessionVolume {
            session_id: 11,
            db: -6.0,
        };
        let mut sync = VolumeSync::default();
        sync.remember(target);
        assert_eq!(
            sync.plan_tick(Some(12), true),
            VolumeTickPlan::ClearedInactive
        );
        assert_eq!(sync.target, None);

        sync.remember(target);
        assert_eq!(
            sync.plan_tick(Some(target.session_id), true),
            VolumeTickPlan::Apply {
                target,
                retry_attempt: None,
            }
        );
    }

    #[test]
    fn transient_volume_retry_is_pending_only_bounded_and_success_clears_it() {
        let target = SessionVolume {
            session_id: 21,
            db: -12.0,
        };
        let mut sync = VolumeSync::default();
        sync.remember(target);
        assert_eq!(
            sync.plan_tick(Some(target.session_id), false),
            VolumeTickPlan::None,
            "a healthy volume command must not be rewritten on every poll"
        );

        sync.failed(target, None);
        for attempt in 1..=VOLUME_RETRY_LIMIT {
            assert_eq!(
                sync.plan_tick(Some(target.session_id), false),
                VolumeTickPlan::Apply {
                    target,
                    retry_attempt: Some(attempt),
                }
            );
            sync.failed(target, Some(attempt));
        }
        assert_eq!(sync.retry, None);
        assert_eq!(
            sync.plan_tick(Some(target.session_id), false),
            VolumeTickPlan::None
        );

        sync.failed(target, None);
        let plan = sync.plan_tick(Some(target.session_id), false);
        assert!(matches!(plan, VolumeTickPlan::Apply { .. }));
        sync.succeeded(target);
        assert_eq!(sync.retry, None);
    }

    #[test]
    fn native_volume_feedback_is_armed_only_after_sender_volume_and_does_not_echo() {
        let target = SessionVolume {
            session_id: 31,
            db: -12.0,
        };
        let mut sync = VolumeSync::default();
        assert!(sync
            .plan_remote(
                Some(target.session_id),
                SystemVolumeState::from_sender_db(-18.0),
                1,
            )
            .is_none());

        sync.remember(target);
        assert!(sync
            .plan_remote(
                Some(target.session_id),
                SystemVolumeState::from_sender_db(-18.0),
                2,
            )
            .is_none());
        sync.succeeded(target);
        assert!(sync
            .plan_remote(
                Some(target.session_id),
                SystemVolumeState {
                    scalar: SystemVolumeState::from_sender_db(-12.0).scalar + 0.000_01,
                    muted: false,
                },
                3,
            )
            .is_none());

        let changed = sync
            .plan_remote(
                Some(target.session_id),
                SystemVolumeState::from_sender_db(-18.0),
                4,
            )
            .expect("a real native change must go back to the sender");
        assert_eq!(changed.db, -18.0);
        assert_eq!(changed.attempt, 1);
        sync.remote_queued(changed.clone());
        assert!(sync.remote_result(changed.serial, true).is_some());
        assert!(sync
            .plan_remote(Some(target.session_id), changed.desired, 5,)
            .is_none());
    }

    #[test]
    fn one_five_and_ten_percent_system_steps_do_not_collapse_to_zero() {
        let target = SessionVolume {
            session_id: 30,
            db: -30.0,
        };
        let mut sync = VolumeSync::default();
        sync.remember(target);
        sync.succeeded(target);

        for (serial, scalar, expected_db) in [(1, 0.01, -29.7), (2, 0.05, -28.5), (3, 0.10, -27.0)]
        {
            let current = SystemVolumeState {
                scalar,
                muted: false,
            };
            let flight = sync
                .plan_remote(Some(target.session_id), current, serial)
                .expect("every visible low-end slider step must reach the sender");
            assert!((flight.db - expected_db).abs() < 1e-5);
            sync.remote_queued(flight.clone());
            assert!(sync.remote_result(flight.serial, true).unwrap().delivered);
        }
    }

    #[test]
    fn explicit_mute_and_unmute_preserve_the_half_volume_slider() {
        let target = SessionVolume {
            session_id: 32,
            db: -15.0,
        };
        let mut sync = VolumeSync::default();
        sync.remember(target);
        sync.succeeded(target);

        let muted = SystemVolumeState {
            scalar: 0.5,
            muted: true,
        };
        let mute_flight = sync
            .plan_remote(Some(target.session_id), muted, 10)
            .expect("explicit system mute must reach the sender");
        assert_eq!(mute_flight.db, -144.0);
        sync.remote_queued(mute_flight.clone());
        sync.remote_result(mute_flight.serial, true).unwrap();

        let unmuted = SystemVolumeState {
            scalar: 0.5,
            muted: false,
        };
        let unmute_flight = sync
            .plan_remote(Some(target.session_id), unmuted, 11)
            .expect("unmute must restore the same slider position");
        assert!((unmute_flight.db - (-15.0)).abs() < 1e-5);
    }

    #[test]
    fn installed_mute_echo_is_consumed_before_outcome_clears_its_flight() {
        let sender_half = SessionVolume {
            session_id: 33,
            db: -15.0,
        };
        let receiver_muted = SystemVolumeState {
            scalar: 0.5,
            muted: true,
        };
        let mut sync = VolumeSync::default();
        sync.remember_snapshot(sender_half, 2000);
        sync.succeeded(sender_half);

        let reverse_mute = sync
            .plan_remote(Some(sender_half.session_id), receiver_muted, 20)
            .expect("receiver mute must create a DACP flight");
        assert_eq!(reverse_mute.db, -144.0);
        reverse_mute.write_gate.mark_full_request_written();
        sync.remote_queued(reverse_mute.clone());

        // Runtime has already installed the RTSP echo, but its bounded event
        // wakeup has not run when the HTTP outcome is about to be drained.
        let echoed_mute = SessionVolume {
            session_id: sender_half.session_id,
            db: -144.0,
        };
        assert_eq!(
            sync.confirm_remote_echo(Some((echoed_mute, 2001)), Some(reverse_mute.serial)),
            Some(echoed_mute)
        );
        assert!(
            sync.remote_result(reverse_mute.serial, true)
                .unwrap()
                .delivered
        );
        assert!(sync.remote_inflight.is_none());
        assert_eq!(
            sync.reconcile_sender_snapshot(Some((echoed_mute, 2001))),
            SenderSnapshotDecision::Unchanged,
            "the same-tick reconciliation must not turn the consumed echo into native IO"
        );

        // If the user has already unmuted the receiver, the old mute echo must
        // not re-mute it; the preserved 50% scalar becomes the next DACP value.
        let restored = sync
            .plan_remote(
                Some(sender_half.session_id),
                SystemVolumeState {
                    scalar: 0.5,
                    muted: false,
                },
                21,
            )
            .expect("unmute must restore the preserved 50% slider to the sender");
        assert!((restored.db - (-15.0)).abs() < 1e-5);
    }

    #[test]
    fn post_204_echo_cannot_cancel_or_rollback_a_newer_native_flight() {
        let now = Instant::now();
        let sender = SessionVolume {
            session_id: 34,
            db: -15.0,
        };
        let x1 = SystemVolumeState {
            scalar: 0.2,
            muted: false,
        };
        let x2 = SystemVolumeState {
            scalar: 0.8,
            muted: false,
        };
        let mut sync = VolumeSync::default();
        sync.remember_snapshot(sender, 100);
        sync.succeeded(sender);

        let first = sync.plan_remote(Some(sender.session_id), x1, 101).unwrap();
        sync.remote_queued(first.clone());
        sync.remote_result_at(first.serial, true, now).unwrap();
        assert_eq!(
            sync.completed_remote_echoes
                .front()
                .map(|echo| echo.expected_revision),
            Some(101)
        );

        let second = sync.plan_remote(Some(sender.session_id), x2, 102).unwrap();
        sync.remote_queued(second.clone());
        let echoed_x1 = SessionVolume {
            session_id: sender.session_id,
            db: first.db,
        };
        assert!(matches!(
            sync.reconcile_sender_snapshot_at(Some((echoed_x1, 101)), now),
            SenderSnapshotDecision::ReverseEcho { .. }
        ));
        assert_eq!(
            sync.remote_inflight.as_ref().map(|flight| flight.serial),
            Some(102)
        );
        assert_eq!(sync.remote_retry.map(|retry| retry.desired), Some(x2));
        assert_eq!(sync.target.unwrap().db, first.db);
        assert_eq!(sync.sender_known_system, Some(x1));
        assert!(sync.force_remote);

        sync.remote_result_at(second.serial, true, now).unwrap();
        assert_eq!(sync.target.unwrap().db, second.db);
        assert_eq!(sync.sender_known_system, Some(x2));
        assert!(!sync.force_remote);
        assert_eq!(
            sync.completed_remote_echoes
                .front()
                .map(|echo| echo.expected_revision),
            Some(102)
        );
    }

    #[test]
    fn multiple_post_204_echoes_are_correlated_in_revision_order() {
        let now = Instant::now();
        let sender = SessionVolume {
            session_id: 35,
            db: -15.0,
        };
        let x1 = SystemVolumeState {
            scalar: 0.2,
            muted: false,
        };
        let x2 = SystemVolumeState {
            scalar: 0.8,
            muted: false,
        };
        let mut sync = VolumeSync::default();
        sync.remember_snapshot(sender, 200);
        sync.succeeded(sender);
        let first = sync.plan_remote(Some(sender.session_id), x1, 201).unwrap();
        sync.remote_queued(first.clone());
        sync.remote_result_at(first.serial, true, now).unwrap();
        let second = sync.plan_remote(Some(sender.session_id), x2, 202).unwrap();
        sync.remote_queued(second.clone());
        sync.remote_result_at(second.serial, true, now).unwrap();
        assert_eq!(
            sync.completed_remote_echoes
                .iter()
                .map(|echo| echo.expected_revision)
                .collect::<Vec<_>>(),
            vec![201, 202]
        );

        for (db, revision) in [(first.db, 201), (second.db, 202)] {
            let echo = SessionVolume {
                session_id: sender.session_id,
                db,
            };
            assert!(matches!(
                sync.reconcile_sender_snapshot_at(Some((echo, revision)), now),
                SenderSnapshotDecision::ReverseEcho { .. }
            ));
        }
        assert!(sync.completed_remote_echoes.is_empty());
        assert_eq!(sync.target.unwrap().db, second.db);
        assert_eq!(sync.sender_known_system, Some(x2));
        let conservative_reassert = sync
            .plan_remote(Some(sender.session_id), x2, 203)
            .expect("an older echo may require one harmless latest-value reassertion");
        assert_eq!(conservative_reassert.db, second.db);
    }

    #[test]
    fn reordered_x2_then_x1_echoes_leave_no_guard_to_swallow_real_x2() {
        let now = Instant::now();
        let sender = SessionVolume {
            session_id: 39,
            db: -15.0,
        };
        let x1 = SystemVolumeState {
            scalar: 0.2,
            muted: false,
        };
        let x2 = SystemVolumeState {
            scalar: 0.8,
            muted: false,
        };
        let mut sync = VolumeSync::default();
        sync.remember_snapshot(sender, 100);
        sync.succeeded(sender);

        let first = sync.plan_remote(Some(sender.session_id), x1, 101).unwrap();
        sync.remote_queued(first.clone());
        sync.remote_result_at(first.serial, true, now).unwrap();
        let second = sync.plan_remote(Some(sender.session_id), x2, 102).unwrap();
        sync.remote_queued(second.clone());
        sync.remote_result_at(second.serial, true, now).unwrap();

        let early_x2 = SessionVolume {
            session_id: sender.session_id,
            db: second.db,
        };
        assert!(matches!(
            sync.reconcile_sender_snapshot_at(Some((early_x2, 101)), now),
            SenderSnapshotDecision::ReverseEcho { .. }
        ));
        assert_eq!(
            sync.completed_remote_echoes
                .iter()
                .map(|echo| (echo.serial, echo.expected_revision))
                .collect::<Vec<_>>(),
            vec![(first.serial, 102)]
        );

        let late_x1 = SessionVolume {
            session_id: sender.session_id,
            db: first.db,
        };
        assert!(matches!(
            sync.reconcile_sender_snapshot_at(Some((late_x1, 102)), now),
            SenderSnapshotDecision::ReverseEcho { .. }
        ));
        assert!(sync.completed_remote_echoes.is_empty());

        // A real sender command that happens to reuse X2 now has no ghost
        // correlation slot and therefore retains sender authority.
        assert_eq!(
            sync.reconcile_sender_snapshot_at(Some((early_x2, 103)), now),
            SenderSnapshotDecision::Apply {
                target: early_x2,
                replaced_session: false,
            }
        );
        assert_eq!(sync.sender_revision, Some(103));
    }

    #[test]
    fn genuine_sender_command_reserves_revision_before_an_older_late_echo() {
        let now = Instant::now();
        let sender_y = SessionVolume {
            session_id: 36,
            db: -15.0,
        };
        let x1 = SystemVolumeState {
            scalar: 0.2,
            muted: false,
        };
        let sender_z = SessionVolume {
            session_id: sender_y.session_id,
            db: -9.0,
        };
        let mut sync = VolumeSync::default();
        sync.remember_snapshot(sender_y, 300);
        sync.succeeded(sender_y);
        let first = sync
            .plan_remote(Some(sender_y.session_id), x1, 301)
            .unwrap();
        sync.remote_queued(first.clone());
        sync.remote_result_at(first.serial, true, now).unwrap();

        assert_eq!(
            sync.reconcile_sender_snapshot_at(Some((sender_z, 301)), now),
            SenderSnapshotDecision::Apply {
                target: sender_z,
                replaced_session: false,
            }
        );
        assert_eq!(
            sync.completed_remote_echoes
                .front()
                .map(|echo| echo.expected_revision),
            Some(302)
        );
        sync.succeeded(sender_z);

        let late_x1 = SessionVolume {
            session_id: sender_y.session_id,
            db: first.db,
        };
        assert!(matches!(
            sync.reconcile_sender_snapshot_at(Some((late_x1, 302)), now),
            SenderSnapshotDecision::ReverseEcho { .. }
        ));
        assert_eq!(sync.target, Some(sender_z));
        assert_eq!(
            sync.sender_known_system,
            Some(SystemVolumeState::from_sender_db(sender_z.db))
        );
        assert!(sync.force_remote);
        let repair = sync
            .plan_remote(
                Some(sender_z.session_id),
                SystemVolumeState::from_sender_db(sender_z.db),
                302,
            )
            .expect("stale X1 echo must reassert newer sender Z");
        assert_eq!(repair.db, sender_z.db);
    }

    #[test]
    fn coalesced_revision_gap_drops_ambiguous_echo_guard_before_real_sender_x() {
        let now = Instant::now();
        let sender_y = SessionVolume {
            session_id: 37,
            db: -15.0,
        };
        let receiver_x = SystemVolumeState {
            scalar: 0.2,
            muted: false,
        };
        let sender_z = SessionVolume {
            session_id: sender_y.session_id,
            db: -9.0,
        };
        let mut sync = VolumeSync::default();
        sync.remember_snapshot(sender_y, 100);
        sync.succeeded(sender_y);

        let reverse_x = sync
            .plan_remote(Some(sender_y.session_id), receiver_x, 101)
            .unwrap();
        sync.remote_queued(reverse_x.clone());
        sync.remote_result_at(reverse_x.serial, true, now).unwrap();
        assert_eq!(
            sync.completed_remote_echoes
                .front()
                .map(|echo| echo.expected_revision),
            Some(101)
        );

        // The RTSP watch may coalesce X@101 and Z@102 into only Z@102. The
        // skipped revision could already have consumed the tombstone, so it is
        // unsafe to move that guard forward to revision 103.
        assert_eq!(
            sync.reconcile_sender_snapshot_at(Some((sender_z, 102)), now),
            SenderSnapshotDecision::Apply {
                target: sender_z,
                replaced_session: false,
            }
        );
        assert!(sync.completed_remote_echoes.is_empty());
        sync.succeeded(sender_z);

        let genuine_x = SessionVolume {
            session_id: sender_y.session_id,
            db: reverse_x.db,
        };
        assert_eq!(
            sync.reconcile_sender_snapshot_at(Some((genuine_x, 103)), now),
            SenderSnapshotDecision::Apply {
                target: genuine_x,
                replaced_session: false,
            }
        );
        assert_eq!(sync.target, Some(genuine_x));
        assert_eq!(sync.sender_revision, Some(103));
    }

    #[test]
    fn gap_that_lands_on_a_later_guard_value_still_prefers_sender_authority() {
        let now = Instant::now();
        let sender = SessionVolume {
            session_id: 40,
            db: -15.0,
        };
        let x1 = SystemVolumeState {
            scalar: 0.2,
            muted: false,
        };
        let x2 = SystemVolumeState {
            scalar: 0.8,
            muted: false,
        };
        let mut sync = VolumeSync::default();
        sync.remember_snapshot(sender, 100);
        sync.succeeded(sender);

        let first = sync.plan_remote(Some(sender.session_id), x1, 101).unwrap();
        sync.remote_queued(first.clone());
        sync.remote_result_at(first.serial, true, now).unwrap();
        let second = sync.plan_remote(Some(sender.session_id), x2, 102).unwrap();
        sync.remote_queued(second.clone());
        sync.remote_result_at(second.serial, true, now).unwrap();

        // The watch may have coalesced X1@101 and X2@102. Even though the last
        // value exactly equals g2, the revision gap means correlation history
        // is incomplete; treating it as an echo would leave g1 behind time.
        let only_x2 = SessionVolume {
            session_id: sender.session_id,
            db: second.db,
        };
        assert_eq!(
            sync.reconcile_sender_snapshot_at(Some((only_x2, 102)), now),
            SenderSnapshotDecision::Apply {
                target: only_x2,
                replaced_session: false,
            }
        );
        assert!(sync.completed_remote_echoes.is_empty());
        assert_eq!(sync.sender_revision, Some(102));
    }

    #[test]
    fn full_cancelled_x_echo_cannot_overwrite_newer_sender_z() {
        let now = Instant::now();
        let sender_y = SessionVolume {
            session_id: 41,
            db: -15.0,
        };
        let native_x = SystemVolumeState {
            scalar: 0.2,
            muted: false,
        };
        let sender_z = SessionVolume {
            session_id: sender_y.session_id,
            db: -6.0,
        };
        let mut sync = VolumeSync::default();
        sync.remember_snapshot(sender_y, 1200);
        sync.succeeded(sender_y);
        let old_x = sync
            .plan_remote(Some(sender_y.session_id), native_x, 50)
            .unwrap();
        old_x.write_gate.mark_full_request_written();
        sync.remote_queued(old_x.clone());

        assert_eq!(
            sync.reconcile_sender_snapshot_at(Some((sender_z, 1201)), now),
            SenderSnapshotDecision::Apply {
                target: sender_z,
                replaced_session: false,
            }
        );
        sync.succeeded(sender_z);
        assert_eq!(
            sync.completed_remote_echoes
                .front()
                .map(|echo| (echo.serial, echo.expected_revision)),
            Some((old_x.serial, 1202))
        );

        let late_x = SessionVolume {
            session_id: sender_y.session_id,
            db: old_x.db,
        };
        assert!(matches!(
            sync.reconcile_sender_snapshot_at(Some((late_x, 1202)), now),
            SenderSnapshotDecision::ReverseEcho { .. }
        ));
        assert_eq!(sync.target, Some(sender_z));
        assert_eq!(
            sync.sender_known_system,
            Some(SystemVolumeState::from_sender_db(sender_z.db))
        );
        assert!(sync.force_remote);
        assert_eq!(
            sync.plan_tick(Some(sender_z.session_id), true),
            VolumeTickPlan::Apply {
                target: sender_z,
                retry_attempt: None,
            },
            "an output replacement must replay Z, never the older echoed X"
        );
        let repair = sync
            .plan_remote(
                Some(sender_z.session_id),
                SystemVolumeState::from_sender_db(sender_z.db),
                51,
            )
            .expect("the stale X echo must force one Z reassertion");
        assert_eq!(repair.db, sender_z.db);
    }

    #[test]
    fn authorized_but_incomplete_x_defers_until_superseded_then_applies_as_sender() {
        let now = Instant::now();
        let sender_y = SessionVolume {
            session_id: 42,
            db: -15.0,
        };
        let native_x = SystemVolumeState {
            scalar: 0.2,
            muted: false,
        };
        let sender_z = SessionVolume {
            session_id: sender_y.session_id,
            db: -6.0,
        };
        let mut sync = VolumeSync::default();
        sync.remember_snapshot(sender_y, 1200);
        sync.succeeded(sender_y);
        let old_x = sync
            .plan_remote(Some(sender_y.session_id), native_x, 60)
            .unwrap();
        assert!(old_x.write_gate.commit(|| true));
        sync.remote_queued(old_x.clone());

        assert!(matches!(
            sync.reconcile_sender_snapshot_at(Some((sender_z, 1201)), now),
            SenderSnapshotDecision::Apply { .. }
        ));
        sync.succeeded(sender_z);
        let possible_x = SessionVolume {
            session_id: sender_y.session_id,
            db: old_x.db,
        };
        assert_eq!(
            sync.reconcile_sender_snapshot_at(Some((possible_x, 1202)), now),
            SenderSnapshotDecision::DeferredRemoteOutcome
        );
        assert_eq!(sync.sender_revision, Some(1201));
        assert!(sync
            .plan_remote(Some(sender_z.session_id), native_x, 61)
            .is_none());
        assert_eq!(
            sync.plan_tick(Some(sender_z.session_id), true),
            VolumeTickPlan::Apply {
                target: sender_z,
                retry_attempt: None,
            },
            "Deferred plus output change must replay current Z before polling"
        );

        // The worker proves it never completed a request. The same lossless
        // snapshot is now a genuine sender command and must not be swallowed.
        sync.remote_superseded(old_x.serial);
        assert_eq!(
            sync.reconcile_sender_snapshot_at(Some((possible_x, 1202)), now),
            SenderSnapshotDecision::Apply {
                target: possible_x,
                replaced_session: false,
            }
        );
    }

    #[test]
    fn only_full_post_write_error_gets_an_echo_guard_and_can_converge() {
        let now = Instant::now();
        let sender_y = SessionVolume {
            session_id: 43,
            db: -15.0,
        };
        let native_x = SystemVolumeState {
            scalar: 0.2,
            muted: false,
        };

        let mut partial = VolumeSync::default();
        partial.remember_snapshot(sender_y, 100);
        partial.succeeded(sender_y);
        let partial_x = partial
            .plan_remote(Some(sender_y.session_id), native_x, 70)
            .unwrap();
        assert!(partial_x.write_gate.commit(|| true));
        partial.remote_queued(partial_x.clone());
        partial
            .remote_result_classified_at(partial_x.serial, false, false, now)
            .unwrap();
        assert!(partial.completed_remote_echoes.is_empty());
        let reported_x = SessionVolume {
            session_id: sender_y.session_id,
            db: partial_x.db,
        };
        assert!(matches!(
            partial.reconcile_sender_snapshot_at(Some((reported_x, 101)), now),
            SenderSnapshotDecision::Apply { .. }
        ));

        let mut full = VolumeSync::default();
        full.remember_snapshot(sender_y, 100);
        full.succeeded(sender_y);
        let full_x = full
            .plan_remote(Some(sender_y.session_id), native_x, 71)
            .unwrap();
        full_x.write_gate.mark_full_request_written();
        full.remote_queued(full_x.clone());
        let completion = full
            .remote_result_classified_at(full_x.serial, false, true, now)
            .unwrap();
        assert!(!completion.delivered);
        assert_eq!(full.target.unwrap().db, full_x.db);
        assert_ne!(full.sender_known_system, Some(native_x));
        assert!(matches!(
            full.reconcile_sender_snapshot_at(Some((reported_x, 101)), now),
            SenderSnapshotDecision::ReverseEcho { .. }
        ));
        assert_eq!(full.sender_known_system, Some(native_x));
        assert_eq!(full.remote_retry, None);
    }

    #[test]
    fn completed_x1_and_live_x2_echoes_correlate_in_either_order() {
        fn setup(now: Instant) -> (VolumeSync, SessionVolume, RemoteInFlight, RemoteInFlight) {
            let sender = SessionVolume {
                session_id: 44,
                db: -15.0,
            };
            let x1 = SystemVolumeState {
                scalar: 0.2,
                muted: false,
            };
            let x2 = SystemVolumeState {
                scalar: 0.8,
                muted: false,
            };
            let mut sync = VolumeSync::default();
            sync.remember_snapshot(sender, 100);
            sync.succeeded(sender);
            let first = sync.plan_remote(Some(sender.session_id), x1, 80).unwrap();
            sync.remote_queued(first.clone());
            sync.remote_result_at(first.serial, true, now).unwrap();
            let second = sync.plan_remote(Some(sender.session_id), x2, 81).unwrap();
            second.write_gate.mark_full_request_written();
            sync.remote_queued(second.clone());
            (sync, sender, first, second)
        }

        let now = Instant::now();
        let (mut newer_first, sender, first, second) = setup(now);
        for (flight, revision) in [(&second, 101), (&first, 102)] {
            let echo = SessionVolume {
                session_id: sender.session_id,
                db: flight.db,
            };
            assert!(matches!(
                newer_first.reconcile_sender_snapshot_at(Some((echo, revision)), now),
                SenderSnapshotDecision::ReverseEcho { .. }
            ));
        }
        assert_eq!(newer_first.target.unwrap().db, second.db);
        assert!(newer_first.completed_remote_echoes.is_empty());
        assert!(
            newer_first
                .remote_result(second.serial, true)
                .unwrap()
                .delivered
        );

        let (mut older_first, sender, first, second) = setup(now);
        for (flight, revision) in [(&first, 101), (&second, 102)] {
            let echo = SessionVolume {
                session_id: sender.session_id,
                db: flight.db,
            };
            assert!(matches!(
                older_first.reconcile_sender_snapshot_at(Some((echo, revision)), now),
                SenderSnapshotDecision::ReverseEcho { .. }
            ));
        }
        assert_eq!(older_first.target.unwrap().db, second.db);
        assert!(older_first.completed_remote_echoes.is_empty());
        assert!(
            older_first
                .remote_result(second.serial, true)
                .unwrap()
                .delivered
        );
    }

    #[test]
    fn post_write_error_echo_confirms_same_value_retry_without_second_guard() {
        let now = Instant::now();
        let sender_y = SessionVolume {
            session_id: 45,
            db: -15.0,
        };
        let native_x = SystemVolumeState {
            scalar: 0.2,
            muted: false,
        };
        let mut sync = VolumeSync::default();
        sync.remember_snapshot(sender_y, 1300);
        sync.succeeded(sender_y);
        let first = sync
            .plan_remote(Some(sender_y.session_id), native_x, 90)
            .unwrap();
        first.write_gate.mark_full_request_written();
        sync.remote_queued(first.clone());
        sync.remote_result_classified_at(first.serial, false, true, now)
            .unwrap();

        let retry = sync
            .plan_remote(Some(sender_y.session_id), native_x, 91)
            .expect("uncertain response must retain a bounded retry");
        retry.write_gate.mark_full_request_written();
        sync.remote_queued(retry.clone());
        let echo_x = SessionVolume {
            session_id: sender_y.session_id,
            db: first.db,
        };
        assert!(matches!(
            sync.reconcile_sender_snapshot_at(Some((echo_x, 1301)), now),
            SenderSnapshotDecision::ReverseEcho { .. }
        ));
        assert!(sync
            .remote_inflight
            .as_ref()
            .is_some_and(|flight| flight.serial == retry.serial && flight.echo_confirmed));
        assert_eq!(sync.sender_known_system, Some(native_x));
        let completion = sync
            .remote_result_classified_at(retry.serial, false, true, now)
            .unwrap();
        assert!(completion.delivered);
        assert!(sync.completed_remote_echoes.is_empty());
    }

    #[test]
    fn completed_echo_cannot_clear_a_stronger_output_epoch_reassertion() {
        let now = Instant::now();
        let sender = SessionVolume {
            session_id: 38,
            db: -15.0,
        };
        let x = SystemVolumeState {
            scalar: 0.2,
            muted: false,
        };
        let mut sync = VolumeSync::default();
        sync.remember_snapshot(sender, 700);
        sync.succeeded(sender);
        let flight = sync.plan_remote(Some(sender.session_id), x, 701).unwrap();
        sync.remote_queued(flight.clone());
        sync.remote_result_at(flight.serial, true, now).unwrap();

        sync.observe_output_change(Some(sender.session_id));
        assert!(sync.force_remote);
        let new_epoch = sync
            .plan_remote(Some(sender.session_id), x, 702)
            .expect("the new output epoch must get its own reassertion");
        sync.remote_queued(new_epoch.clone());
        let echo = SessionVolume {
            session_id: sender.session_id,
            db: flight.db,
        };
        assert!(matches!(
            sync.reconcile_sender_snapshot_at(Some((echo, 701)), now),
            SenderSnapshotDecision::ReverseEcho { .. }
        ));
        assert!(sync.force_remote);
        assert!(sync
            .remote_inflight
            .as_ref()
            .is_some_and(|flight| { flight.serial == new_epoch.serial && !flight.echo_confirmed }));
        assert!(
            !sync
                .remote_result_classified_at(new_epoch.serial, false, false, now)
                .unwrap()
                .delivered
        );
        assert!(sync.force_remote);
        assert!(sync.plan_remote(Some(sender.session_id), x, 703).is_some());
    }

    #[test]
    fn completed_echo_guard_is_exact_one_shot_expiring_session_bound_and_bounded() {
        let now = Instant::now();
        let sender = SessionVolume {
            session_id: 37,
            db: -15.0,
        };
        let x = SystemVolumeState {
            scalar: 0.2,
            muted: false,
        };
        let mut sync = VolumeSync::default();
        sync.remember_snapshot(sender, 400);
        sync.succeeded(sender);
        let flight = sync.plan_remote(Some(sender.session_id), x, 401).unwrap();
        sync.remote_queued(flight.clone());

        for (serial, db, session_id) in [
            (999, flight.db, sender.session_id),
            (flight.serial, flight.db + 0.5, sender.session_id),
            (flight.serial, flight.db, sender.session_id + 1),
        ] {
            assert!(sync
                .confirm_remote_echo_at(
                    Some((SessionVolume { session_id, db }, 401)),
                    Some(serial),
                    now,
                )
                .is_none());
        }
        assert_eq!(
            sync.remote_inflight.as_ref().map(|current| current.serial),
            Some(flight.serial)
        );
        sync.remote_result_at(flight.serial, true, now).unwrap();

        let echo = SessionVolume {
            session_id: sender.session_id,
            db: flight.db,
        };
        assert!(matches!(
            sync.reconcile_sender_snapshot_at(Some((echo, 401)), now),
            SenderSnapshotDecision::ReverseEcho { .. }
        ));
        assert_eq!(
            sync.reconcile_sender_snapshot_at(Some((echo, 402)), now),
            SenderSnapshotDecision::Apply {
                target: echo,
                replaced_session: false,
            },
            "a consumed tombstone cannot swallow a real same-value command"
        );

        let mut expired = VolumeSync::default();
        expired.remember_snapshot(sender, 500);
        expired.succeeded(sender);
        let old = expired
            .plan_remote(Some(sender.session_id), x, 501)
            .unwrap();
        expired.remote_queued(old.clone());
        expired.remote_result_at(old.serial, true, now).unwrap();
        let expired_echo = SessionVolume {
            session_id: sender.session_id,
            db: old.db,
        };
        assert!(matches!(
            expired.reconcile_sender_snapshot_at(
                Some((expired_echo, 501)),
                now + DACP_COMPLETED_ECHO_GRACE + Duration::from_nanos(1),
            ),
            SenderSnapshotDecision::Apply { .. }
        ));

        let mut bounded = VolumeSync::default();
        bounded.target = Some(sender);
        bounded.sender_revision = Some(600);
        for serial in 1..=(DACP_COMPLETED_ECHO_LIMIT as u64 + 1) {
            bounded.remember_completed_remote_echo(
                RemoteInFlight {
                    serial,
                    desired: x,
                    db: x.airplay_db(),
                    attempt: 1,
                    echo_confirmed: false,
                    sender_revision_at_queue: Some(600),
                    write_gate: full_write_gate(),
                },
                now,
            );
        }
        assert_eq!(
            bounded.completed_remote_echoes.len(),
            DACP_COMPLETED_ECHO_LIMIT
        );
        assert_eq!(bounded.completed_remote_echoes.front().unwrap().serial, 2);
        bounded.remember_snapshot(
            SessionVolume {
                session_id: sender.session_id + 1,
                db: sender.db,
            },
            1,
        );
        assert!(bounded.completed_remote_echoes.is_empty());
    }

    #[test]
    fn reverse_success_becomes_the_value_replayed_after_output_replacement() {
        let sender_y = SessionVolume {
            session_id: 31,
            db: -12.0,
        };
        let native_x = SystemVolumeState::from_sender_db(-18.0);
        let mut sync = VolumeSync::default();
        sync.remember_snapshot(sender_y, 900);
        sync.succeeded(sender_y);
        let epoch = Arc::new(AtomicU64::new(90));
        let accepted_epoch = OutputEpochLease::new(90, &epoch);

        let reverse = sync
            .plan_remote(Some(sender_y.session_id), native_x, 40)
            .expect("a native change must be sent only after sender authority exists");
        sync.remote_queued(reverse.clone());
        assert!(
            accepted_epoch.is_current(),
            "outcome convergence check at E0"
        );
        assert!(sync.remote_result(reverse.serial, true).is_some());

        let synchronized_x = SessionVolume {
            session_id: sender_y.session_id,
            db: native_x.airplay_db(),
        };
        assert_eq!(sync.target, Some(synchronized_x));
        assert_eq!(sync.sender_reported, Some(sender_y));
        assert!(
            !sync.needs_sender_reconcile(sender_y, 900),
            "the unchanged runtime RTSP snapshot must not roll X back to Y"
        );
        // The output can switch immediately after the outcome's final lease
        // check. The next ticker observation must still revoke convergence,
        // replay X to E1 and reassert X to the sender.
        epoch.store(91, Ordering::Release);
        assert!(!accepted_epoch.is_current());
        sync.observe_output_change(Some(sender_y.session_id));
        assert_eq!(
            sync.plan_tick(Some(sender_y.session_id), true),
            VolumeTickPlan::Apply {
                target: synchronized_x,
                retry_attempt: None,
            },
            "a replacement output must receive synchronized X, not old sender Y"
        );
        sync.succeeded(synchronized_x);
        assert_eq!(sync.sender_known_system, Some(native_x));
        let reassert = sync
            .plan_remote(Some(sender_y.session_id), native_x, 41)
            .expect("E0 -> E1 after outcome acceptance must force one reassertion");
        assert_eq!(reassert.db, native_x.airplay_db());
    }

    #[test]
    fn lossless_revision_recovers_dropped_same_value_sender_command_after_reverse_x() {
        let sender_y = SessionVolume {
            session_id: 44,
            db: -12.0,
        };
        let native_x = SystemVolumeState::from_sender_db(-18.0);
        let mut sync = VolumeSync::default();
        sync.remember_snapshot(sender_y, 1000);
        sync.succeeded(sender_y);
        let reverse_x = sync
            .plan_remote(Some(sender_y.session_id), native_x, 45)
            .unwrap();
        sync.remote_queued(reverse_x.clone());
        assert!(sync.remote_result(reverse_x.serial, true).is_some());
        assert_eq!(sync.target.unwrap().db, native_x.airplay_db());

        // EventHub dropped the command, and its dB is the same Y that the
        // sender reported before reverse X. Only the lossless command revision
        // proves this is a new sender-authoritative instruction.
        assert!(
            !sync.needs_sender_reconcile(sender_y, 1000),
            "old Y snapshot must not roll synchronized X back"
        );
        let SenderSnapshotDecision::Apply {
            target: fallback_y,
            replaced_session,
        } = sync.reconcile_sender_snapshot(Some((sender_y, 1001)))
        else {
            panic!("same-value Y at a higher revision must survive event loss");
        };
        assert!(!replaced_session);
        assert_eq!(fallback_y, sender_y);
        assert_eq!(sync.target, Some(sender_y));
        assert!(
            sync.plan_remote(Some(sender_y.session_id), native_x, 46)
                .is_none(),
            "fallback must disarm reverse polling until Y is applied"
        );
        sync.succeeded(fallback_y);
        assert_eq!(
            sync.sender_known_system,
            Some(SystemVolumeState::from_sender_db(sender_y.db))
        );
    }

    #[test]
    fn delayed_old_volume_event_cannot_rollback_a_reverse_success() {
        let y = SessionVolume {
            session_id: 47,
            db: -12.0,
        };
        let native_x = SystemVolumeState::from_sender_db(-18.0);
        let z = SessionVolume {
            session_id: 47,
            db: -6.0,
        };
        let mut sync = VolumeSync::default();
        let SenderSnapshotDecision::Apply {
            target: first_y, ..
        } = sync.reconcile_sender_snapshot(Some((y, 1100)))
        else {
            panic!("the first event wakeup must adopt the runtime snapshot immediately");
        };
        sync.succeeded(first_y);

        let reverse_x = sync
            .plan_remote(Some(y.session_id), native_x, 47)
            .expect("a native volume change must be sent back to the sender");
        sync.remote_queued(reverse_x.clone());
        assert!(sync.remote_result(reverse_x.serial, true).is_some());
        let synchronized_x = SessionVolume {
            session_id: y.session_id,
            db: native_x.airplay_db(),
        };
        assert_eq!(sync.target, Some(synchronized_x));

        // Y@1100 was already consumed by the ticker before this bounded event
        // reached the event pump. The event is only a wakeup, so reconciling
        // its current lossless snapshot is a no-op and cannot restore old Y.
        assert_eq!(
            sync.reconcile_sender_snapshot(Some((y, 1100))),
            SenderSnapshotDecision::Unchanged
        );
        assert_eq!(sync.target, Some(synchronized_x));

        // A genuinely newer snapshot remains on the low-latency event path:
        // the wakeup adopts Z@1101 immediately rather than waiting 200 ms.
        let SenderSnapshotDecision::Apply {
            target: latest_z,
            replaced_session,
        } = sync.reconcile_sender_snapshot(Some((z, 1101)))
        else {
            panic!("a new event revision must be applied immediately");
        };
        assert!(!replaced_session);
        assert_eq!(latest_z, z);
        assert_eq!(sync.target, Some(z));
    }

    #[test]
    fn matching_rtsp_echo_preserves_dacp_flight_until_2xx_and_settles() {
        let sender_y = SessionVolume {
            session_id: 48,
            db: -12.0,
        };
        let native_x = SystemVolumeState::from_sender_db(-18.0);
        let mut sync = VolumeSync::default();
        sync.remember_snapshot(sender_y, 1150);
        sync.succeeded(sender_y);

        let reverse_x = sync
            .plan_remote(Some(sender_y.session_id), native_x, 48)
            .expect("native X must create one reverse DACP flight");
        reverse_x.write_gate.mark_full_request_written();
        sync.remote_queued(reverse_x.clone());
        assert_eq!(
            sync.remote_inflight.as_ref().map(|flight| flight.serial),
            Some(48)
        );

        // OwnTone-style DACP handling reflects X through RTSP before its HTTP
        // 204 reaches our worker. This is an acknowledgement of flight 48,
        // not a new sender command that may cancel it.
        let echoed_x = SessionVolume {
            session_id: sender_y.session_id,
            db: reverse_x.db,
        };
        assert_eq!(
            sync.reconcile_sender_snapshot(Some((echoed_x, 1151))),
            SenderSnapshotDecision::ReverseEcho { target: echoed_x }
        );
        assert_eq!(sync.sender_revision, Some(1151));
        assert_eq!(sync.target, Some(echoed_x));
        assert_eq!(sync.sender_known_system, Some(native_x));
        assert_eq!(
            sync.remote_inflight.as_ref().map(|flight| flight.serial),
            Some(48)
        );
        assert_eq!(sync.remote_retry, None);
        assert!(!sync.force_remote);

        let completion = sync
            .remote_result(reverse_x.serial, true)
            .expect("the HTTP 2xx must complete the preserved flight");
        assert!(completion.delivered);
        assert!(completion.flight.echo_confirmed);
        assert!(sync.remote_inflight.is_none());
        assert!(sync
            .plan_remote(Some(sender_y.session_id), native_x, 49)
            .is_none());
    }

    #[test]
    fn rtsp_echo_confirmation_overrides_a_later_http_completion_error() {
        let sender_y = SessionVolume {
            session_id: 50,
            db: -12.0,
        };
        let native_x = SystemVolumeState::from_sender_db(-18.0);
        let mut sync = VolumeSync::default();
        sync.remember_snapshot(sender_y, 1170);
        sync.succeeded(sender_y);
        let reverse_x = sync
            .plan_remote(Some(sender_y.session_id), native_x, 51)
            .expect("native X must create one reverse DACP flight");
        reverse_x.write_gate.mark_full_request_written();
        sync.remote_queued(reverse_x.clone());
        let echoed_x = SessionVolume {
            session_id: sender_y.session_id,
            db: reverse_x.db,
        };
        assert!(matches!(
            sync.reconcile_sender_snapshot(Some((echoed_x, 1171))),
            SenderSnapshotDecision::ReverseEcho { .. }
        ));

        let completion = sync
            .remote_result(reverse_x.serial, false)
            .expect("the echoed serial must still own the logical flight");
        assert!(completion.flight.echo_confirmed);
        assert!(
            completion.delivered,
            "RTSP echo proves delivery despite an HTTP response-read error"
        );
        assert!(sync.remote_inflight.is_none());
        assert_eq!(sync.remote_retry, None);
        assert!(!sync.force_remote);
        assert!(sync
            .plan_remote(Some(sender_y.session_id), native_x, 52)
            .is_none());
    }

    #[test]
    fn nearby_sender_step_is_not_misclassified_as_a_dacp_echo() {
        let sender_y = SessionVolume {
            session_id: 49,
            db: -12.0,
        };
        let native_x = SystemVolumeState::from_sender_db(-18.0);
        let sender_z = SessionVolume {
            session_id: sender_y.session_id,
            db: -17.5,
        };
        let mut sync = VolumeSync::default();
        sync.remember_snapshot(sender_y, 1160);
        sync.succeeded(sender_y);
        let reverse_x = sync
            .plan_remote(Some(sender_y.session_id), native_x, 50)
            .expect("native X must create one reverse DACP flight");
        sync.remote_queued(reverse_x.clone());

        assert_eq!(
            sync.reconcile_sender_snapshot(Some((sender_z, 1161))),
            SenderSnapshotDecision::Apply {
                target: sender_z,
                replaced_session: false,
            },
            "a real 0.5 dB sender step must not use native settle tolerance"
        );
        assert!(sync.remote_inflight.is_none());
        assert_eq!(sync.target, Some(sender_z));
    }

    #[test]
    fn output_epoch_change_during_native_read_rejects_the_sample() {
        let epoch = Arc::new(AtomicU64::new(7));
        let lease = OutputEpochLease::new(7, &epoch);
        let sampled_new_device = SystemVolumeState::from_sender_db(-3.0);

        let result = read_system_volume_for_epoch(&lease, || {
            // Deterministically model the watcher firing after the controller's
            // pre-read check but before native readback completes.
            epoch.store(8, Ordering::Release);
            Ok(sampled_new_device)
        })
        .unwrap();
        assert_eq!(result, EpochRead::Changed(8));
        assert_ne!(result, EpochRead::Stable(sampled_new_device));
    }

    #[test]
    fn output_epoch_lease_revokes_a_job_between_read_and_queue() {
        let epoch = Arc::new(AtomicU64::new(11));
        let lease = OutputEpochLease::new(11, &epoch);
        assert!(lease.is_current());
        epoch.store(12, Ordering::Release);
        assert!(!lease.is_current());
        assert_eq!(lease.observed(), 12);
    }

    #[test]
    fn a_new_sender_command_disarms_reverse_polling_until_its_native_write_finishes() {
        let first = SessionVolume {
            session_id: 32,
            db: -12.0,
        };
        let second = SessionVolume {
            session_id: 32,
            db: -6.0,
        };
        let mut sync = VolumeSync::default();
        sync.remember(first);
        sync.succeeded(first);

        sync.remember(second);
        assert!(sync
            .plan_remote(
                Some(second.session_id),
                SystemVolumeState {
                    // This can be the transient level between the slider and
                    // unmute calls. It must not become a DACP request.
                    scalar: SystemVolumeState::from_sender_db(-6.0).scalar,
                    muted: true,
                },
                6,
            )
            .is_none());
        sync.succeeded(second);
        assert!(sync
            .plan_remote(
                Some(second.session_id),
                SystemVolumeState {
                    scalar: SystemVolumeState::from_sender_db(-6.0).scalar + 0.000_01,
                    muted: false,
                },
                7,
            )
            .is_none());
    }

    #[test]
    fn any_possibly_written_superseded_dacp_request_forces_one_latest_correction() {
        let pending = DacpWriteGate::new();
        let full = full_write_gate();
        assert!(DacpOutcomeResult::Sent(Ok(())).may_have_written_request(&pending));
        assert!(!DacpOutcomeResult::Sent(Err("connect failed".into()))
            .may_have_written_request(&pending));
        assert!(
            DacpOutcomeResult::Sent(Err("response lost".into())).may_have_written_request(&full)
        );
        assert!(!DacpOutcomeResult::Superseded.may_have_written_request(&full));
        assert!(DacpOutcomeResult::RevokedAfterPossibleWrite.may_have_written_request(&pending));

        let target = SessionVolume {
            session_id: 33,
            db: -6.0,
        };
        let current = SystemVolumeState::from_sender_db(-6.0);
        let mut sync = VolumeSync::default();
        sync.remember(target);
        sync.succeeded(target);
        sync.stale_remote_may_have_reached_sender();

        let correction = sync
            .plan_remote(Some(target.session_id), current, 8)
            .expect("a stale write may have overwritten the sender UI");
        assert_eq!(correction.db, -6.0);
        sync.remote_queued(correction.clone());
        sync.remote_result(correction.serial, true);
        assert!(sync
            .plan_remote(Some(target.session_id), current, 9)
            .is_none());
    }

    #[test]
    fn credential_revocation_after_possible_write_never_counts_as_convergence() {
        let target = SessionVolume {
            session_id: 35,
            db: -12.0,
        };
        let changed = SystemVolumeState::from_sender_db(-18.0);
        let mut sync = VolumeSync::default();
        sync.remember(target);
        sync.succeeded(target);
        let old = sync
            .plan_remote(Some(target.session_id), changed, 20)
            .unwrap();
        sync.remote_queued(old.clone());

        // This represents a credential revision racing after write_all but
        // before the worker reports its HTTP result.
        sync.remote_revoked_after_possible_write(old.serial);
        assert!(sync.remote_inflight.is_none());
        assert!(sync.force_remote);
        let repair = sync
            .plan_remote(Some(target.session_id), changed, 21)
            .expect("fresh credentials must reassert even after old HTTP 2xx");
        assert_eq!(repair.attempt, 1, "revocation does not spend a retry");
        assert_eq!(repair.db, old.db);
    }

    #[test]
    fn unmatched_possibly_written_x_after_new_sender_z_forces_z_reassertion() {
        let sender_y = SessionVolume {
            session_id: 35,
            db: -12.0,
        };
        let native_x = SystemVolumeState::from_sender_db(-18.0);
        let sender_z = SessionVolume {
            session_id: sender_y.session_id,
            db: -6.0,
        };
        let mut sync = VolumeSync::default();
        sync.remember_snapshot(sender_y, 1200);
        sync.succeeded(sender_y);
        let old_x = sync
            .plan_remote(Some(sender_y.session_id), native_x, 50)
            .unwrap();
        sync.remote_queued(old_x.clone());

        // X is already on the socket. A newer sender command Z correctly
        // disarms the old logical flight and wins on the native endpoint.
        assert_eq!(
            sync.reconcile_sender_snapshot(Some((sender_z, 1201))),
            SenderSnapshotDecision::Apply {
                target: sender_z,
                replaced_session: false,
            }
        );
        assert!(sync.remote_inflight.is_none());
        sync.succeeded(sender_z);

        // The worker can only now report that old X may have been written. Its
        // serial is unmatched, but X could still land after Z in sender UI.
        sync.remote_revoked_after_possible_write(old_x.serial);
        assert!(sync.force_remote);
        let current_z = SystemVolumeState::from_sender_db(sender_z.db);
        let repair = sync
            .plan_remote(Some(sender_z.session_id), current_z, 51)
            .expect("possibly-written old X must force current sender Z");
        assert_eq!(repair.db, sender_z.db);
    }

    #[test]
    fn refresh_after_worker_success_but_before_outcome_drain_forces_repair() {
        let target = SessionVolume {
            session_id: 36,
            db: -12.0,
        };
        let current = SystemVolumeState::from_sender_db(-18.0);
        let mut sync = VolumeSync::default();
        sync.remember(target);
        sync.succeeded(target);
        let old = sync
            .plan_remote(Some(target.session_id), current, 30)
            .unwrap();
        sync.remote_queued(old.clone());

        // The worker already classified HTTP 2xx, then the runtime revoked
        // its lease before the ticker drained the outcome.
        let classified = DacpOutcomeResult::Sent(Ok(()));
        let full = full_write_gate();
        assert!(reject_revoked_dacp_outcome(
            &mut sync,
            old.serial,
            &classified,
            &full,
            false,
        ));
        assert!(sync.force_remote);
        assert!(sync.remote_inflight.is_none());
        assert!(sync
            .plan_remote(Some(target.session_id), current, 31)
            .is_some());
    }

    #[test]
    fn ticker_revision_change_closes_every_outcome_toctou() {
        let target = SessionVolume {
            session_id: 37,
            db: -12.0,
        };
        let current = SystemVolumeState::from_sender_db(target.db);
        let mut sync = VolumeSync::default();

        // Learning credentials before the sender's first volume must not
        // manufacture a reverse command.
        assert!(sync.observe_remote_revision(Some(target.session_id), Some((37, 100, 200))));
        assert!(!sync.force_remote);
        sync.remember(target);
        sync.succeeded(target);
        assert!(!sync.observe_remote_revision(Some(target.session_id), Some((37, 100, 200))));

        // Even if an old outcome slipped through every same-tick check, the
        // next 200 ms observation sees an endpoint-only revision and reasserts
        // the current native value through the fresh endpoint.
        assert!(sync.observe_remote_revision(Some(target.session_id), Some((37, 100, 201))));
        assert!(sync.force_remote);
        let repair = sync
            .plan_remote(Some(target.session_id), current, 32)
            .expect("new authority revision must force one fresh request");
        assert_eq!(repair.db, target.db);
    }

    #[test]
    fn a_failed_reverse_warning_can_settle_when_native_returns_to_sender_value() {
        let target = SessionVolume {
            session_id: 34,
            db: -12.0,
        };
        let known = SystemVolumeState::from_sender_db(target.db);
        let mut sync = VolumeSync::default();
        sync.remember(target);
        sync.succeeded(target);

        let changed = sync
            .plan_remote(
                Some(target.session_id),
                SystemVolumeState::from_sender_db(-18.0),
                10,
            )
            .unwrap();
        sync.remote_queued(changed.clone());
        sync.remote_result(changed.serial, false);
        assert!(sync.remote_retry.is_some());
        assert!(sync.settle_remote_if_converged(Some(target.session_id), known));
        assert!(sync.remote_retry.is_none());
    }

    #[test]
    fn system_slider_and_mute_are_reported_in_airplay_db_exactly_once() {
        let target = SessionVolume {
            session_id: 41,
            db: -30.0,
        };
        let mut sync = VolumeSync::default();
        sync.remember(target);
        sync.succeeded(target);
        let clamp = sync
            .plan_remote(
                Some(target.session_id),
                SystemVolumeState::from_sender_db(-20.0),
                11,
            )
            .expect("system slider change must correct sender UI");
        assert_eq!(clamp.db, -20.0);
        sync.remote_queued(clamp.clone());
        sync.remote_result(clamp.serial, true);
        assert!(sync
            .plan_remote(Some(target.session_id), clamp.desired, 12)
            .is_none());

        let muted = sync
            .plan_remote(
                Some(target.session_id),
                SystemVolumeState {
                    scalar: 0.0,
                    muted: true,
                },
                13,
            )
            .expect("native mute must reach sender");
        assert_eq!(muted.db, -144.0);
    }

    #[test]
    fn failed_dacp_feedback_is_bounded_and_a_new_native_value_rearms_it() {
        let target = SessionVolume {
            session_id: 51,
            db: -12.0,
        };
        let mut sync = VolumeSync::default();
        sync.remember(target);
        sync.succeeded(target);
        let desired = SystemVolumeState::from_sender_db(-18.0);
        for attempt in 1..=DACP_VOLUME_RETRY_LIMIT {
            let flight = sync
                .plan_remote(Some(target.session_id), desired, attempt as u64)
                .expect("bounded retry");
            assert_eq!(flight.attempt, attempt);
            sync.remote_queued(flight.clone());
            sync.remote_result(flight.serial, false);
        }
        assert!(sync
            .plan_remote(Some(target.session_id), desired, 99)
            .is_none());
        let next = sync
            .plan_remote(
                Some(target.session_id),
                SystemVolumeState::from_sender_db(-6.0),
                100,
            )
            .expect("a distinct user change rearms feedback");
        assert_eq!(next.attempt, 1);
    }

    #[test]
    fn dacp_service_match_is_exact_numeric_and_bound_to_rtsp_peer() {
        let peer_ip: IpAddr = "192.0.2.10".parse().unwrap();
        let peer = SocketAddr::new(peer_ip, 7000);
        let remote = RemoteControlInfo {
            session_id: 7,
            peer,
            dacp_id: "0000A1B2C3D4E5F6".into(),
            active_remote: "1986535575".into(),
        };
        let service = ServiceInfo::new(
            DACP_SERVICE_TYPE,
            "iTunes_Ctrl_A1B2C3D4E5F6",
            "iphone.local.",
            "192.0.2.10",
            49152,
            None::<HashMap<String, String>>,
        )
        .unwrap();
        let wanted = dacp_id_u64(&remote.dacp_id).unwrap();
        assert_eq!(
            dacp_service_endpoint(&service, &remote, wanted),
            Some("192.0.2.10:49152".parse().unwrap())
        );

        let substring = ServiceInfo::new(
            DACP_SERVICE_TYPE,
            "fake_iTunes_Ctrl_A1B2C3D4E5F6_extra",
            "attacker.local.",
            "192.0.2.10",
            49153,
            None::<HashMap<String, String>>,
        )
        .unwrap();
        assert_eq!(dacp_service_endpoint(&substring, &remote, wanted), None);
        let wrong_peer = ServiceInfo::new(
            DACP_SERVICE_TYPE,
            "iTunes_Ctrl_A1B2C3D4E5F6",
            "attacker.local.",
            "192.0.2.11",
            49154,
            None::<HashMap<String, String>>,
        )
        .unwrap();
        assert_eq!(dacp_service_endpoint(&wrong_peer, &remote, wanted), None);

        let link_local_ip = "fe80::1234".parse().unwrap();
        let link_local_peer =
            SocketAddr::V6(std::net::SocketAddrV6::new(link_local_ip, 7000, 0, 7));
        let link_local_remote = RemoteControlInfo {
            session_id: 8,
            peer: link_local_peer,
            dacp_id: "0000A1B2C3D4E5F6".into(),
            active_remote: "1986535575".into(),
        };
        let link_local_service = ServiceInfo::new(
            DACP_SERVICE_TYPE,
            "iTunes_Ctrl_A1B2C3D4E5F6",
            "iphone.local.",
            "fe80::1234",
            49155,
            None::<HashMap<String, String>>,
        )
        .unwrap();
        assert_eq!(
            dacp_service_endpoint(&link_local_service, &link_local_remote, wanted),
            Some(SocketAddr::V6(std::net::SocketAddrV6::new(
                link_local_ip,
                49155,
                0,
                7,
            ))),
            "DACP must retain the authenticated RTSP interface scope"
        );
    }

    #[test]
    fn dacp_mdns_cache_is_bounded_and_honors_removals() {
        let mut services = HashMap::new();
        let revision = AtomicU64::new(1);
        for index in 0..(DACP_SERVICE_CACHE_LIMIT + 5) {
            let info = ServiceInfo::new(
                DACP_SERVICE_TYPE,
                &format!("iTunes_Ctrl_{index:016X}"),
                "sender.local.",
                "192.0.2.10",
                40_000 + index as u16,
                None::<HashMap<String, String>>,
            )
            .unwrap();
            update_dacp_service_cache(
                &mut services,
                &revision,
                ServiceEvent::ServiceResolved(info),
            );
        }
        assert_eq!(services.len(), DACP_SERVICE_CACHE_LIMIT);

        let removed = services.keys().next().unwrap().clone();
        update_dacp_service_cache(
            &mut services,
            &revision,
            ServiceEvent::ServiceRemoved(DACP_SERVICE_TYPE.into(), removed.clone()),
        );
        assert!(!services.contains_key(&removed));
        assert_eq!(services.len(), DACP_SERVICE_CACHE_LIMIT - 1);
    }

    #[test]
    fn dacp_http_success_on_replaced_endpoint_cannot_count_as_convergence() {
        let remote = RemoteControlInfo {
            session_id: 73,
            peer: "192.0.2.10:7000".parse().unwrap(),
            dacp_id: "A1B2C3D4E5F6".into(),
            active_remote: "1986535575".into(),
        };
        let mut services = HashMap::new();
        let revision = Arc::new(AtomicU64::new(1));
        let service = |port| {
            ServiceInfo::new(
                DACP_SERVICE_TYPE,
                "iTunes_Ctrl_A1B2C3D4E5F6",
                "sender.local.",
                "192.0.2.10",
                port,
                None::<HashMap<String, String>>,
            )
            .unwrap()
        };
        update_dacp_service_cache(
            &mut services,
            &revision,
            ServiceEvent::ServiceResolved(service(49_152)),
        );
        let endpoint_e1 = DacpEndpointSnapshot {
            address: dacp_endpoint_from_services(&services, &remote).unwrap(),
            revision: revision.load(Ordering::Acquire),
            current_revision: Arc::clone(&revision),
        };

        let sender = SessionVolume {
            session_id: remote.session_id,
            db: -12.0,
        };
        let native = SystemVolumeState::from_sender_db(-18.0);
        let mut sync = VolumeSync::default();
        sync.remember(sender);
        sync.succeeded(sender);
        let old = sync
            .plan_remote(Some(sender.session_id), native, 74)
            .unwrap();
        sync.remote_queued(old.clone());

        // E1 has already returned HTTP 2xx, but mDNS replaced the same
        // DACP-ID/token service with E2 before the ticker consumes the result.
        update_dacp_service_cache(
            &mut services,
            &revision,
            ServiceEvent::ServiceResolved(service(49_153)),
        );
        let endpoint_e2 = DacpEndpointSnapshot {
            address: dacp_endpoint_from_services(&services, &remote).unwrap(),
            revision: revision.load(Ordering::Acquire),
            current_revision: Arc::clone(&revision),
        };
        assert_ne!(endpoint_e1.address, endpoint_e2.address);
        assert!(!endpoint_e1.is_current());
        assert!(endpoint_e2.is_current());
        let accepted_from_e1 = DacpOutcomeResult::Sent(Ok(()));
        let full = full_write_gate();
        assert!(reject_revoked_dacp_outcome(
            &mut sync,
            old.serial,
            &accepted_from_e1,
            &full,
            endpoint_e1.is_current(),
        ));
        assert!(sync.force_remote);
        let repair = sync
            .plan_remote(Some(sender.session_id), native, 75)
            .expect("possible E1 write must be reasserted through resolved E2");
        assert_eq!(repair.db, native.airplay_db());
        assert_eq!(endpoint_e2.address.port(), 49_153);
    }

    #[test]
    fn endpoint_revision_after_accepted_success_forces_e2_reassertion() {
        let remote = RemoteControlInfo {
            session_id: 76,
            peer: "192.0.2.10:7000".parse().unwrap(),
            dacp_id: "A1B2C3D4E5F6".into(),
            active_remote: "1986535575".into(),
        };
        let service = |port| {
            ServiceInfo::new(
                DACP_SERVICE_TYPE,
                "iTunes_Ctrl_A1B2C3D4E5F6",
                "sender.local.",
                "192.0.2.10",
                port,
                None::<HashMap<String, String>>,
            )
            .unwrap()
        };
        let mut services = HashMap::new();
        let revision = AtomicU64::new(1);
        update_dacp_service_cache(
            &mut services,
            &revision,
            ServiceEvent::ServiceResolved(service(49_152)),
        );
        let e1_revision = revision.load(Ordering::Acquire);
        // Identical re-resolution must not create perpetual lease churn.
        update_dacp_service_cache(
            &mut services,
            &revision,
            ServiceEvent::ServiceResolved(service(49_152)),
        );
        assert_eq!(revision.load(Ordering::Acquire), e1_revision);

        let sender = SessionVolume {
            session_id: remote.session_id,
            db: -12.0,
        };
        let native = SystemVolumeState::from_sender_db(-18.0);
        let mut sync = VolumeSync::default();
        assert!(sync.observe_remote_revision(
            Some(sender.session_id),
            Some((sender.session_id, 500, e1_revision)),
        ));
        sync.remember(sender);
        sync.succeeded(sender);
        let sent_e1 = sync
            .plan_remote(Some(sender.session_id), native, 77)
            .unwrap();
        sync.remote_queued(sent_e1.clone());
        assert!(sync.remote_result(sent_e1.serial, true).is_some());
        assert!(!sync.force_remote);

        // E1 was legitimately accepted and only then moved to E2. The cache's
        // monotonic revision closes the gap no outcome-time equality can see.
        update_dacp_service_cache(
            &mut services,
            &revision,
            ServiceEvent::ServiceResolved(service(49_153)),
        );
        let e2_revision = revision.load(Ordering::Acquire);
        assert_ne!(e1_revision, e2_revision);
        assert!(sync.observe_remote_revision(
            Some(sender.session_id),
            Some((sender.session_id, 500, e2_revision)),
        ));
        let repair = sync
            .plan_remote(Some(sender.session_id), native, 78)
            .expect("endpoint-only E2 revision must reassert accepted native X");
        assert_eq!(repair.db, native.airplay_db());
        assert_eq!(
            dacp_endpoint_from_services(&services, &remote)
                .unwrap()
                .port(),
            49_153
        );
    }

    #[test]
    fn dacp_device_volume_request_uses_mapped_slider_db_and_secret_header() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let endpoint = listener.local_addr().unwrap();
        let (request_tx, request_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 512];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let count = stream.read(&mut chunk).unwrap();
                assert_ne!(count, 0);
                request.extend_from_slice(&chunk[..count]);
            }
            request_tx
                .send(String::from_utf8(request).unwrap())
                .unwrap();
            stream
                .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
        });

        let mapped = SystemVolumeState {
            scalar: 0.4,
            muted: false,
        }
        .airplay_db();
        assert!((mapped - (-18.0)).abs() < 1e-5);
        send_dacp_volume(endpoint, "1986535575", mapped).unwrap();
        let request = request_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(request
            .starts_with("GET /ctrl-int/1/setproperty?dmcp.device-volume=-18.000000 HTTP/1.1\r\n"));
        assert!(request.contains(&format!("Host: {endpoint}\r\n")));
        assert!(request.contains("Active-Remote: 1986535575\r\n"));
        assert!(request.ends_with("Connection: close\r\n\r\n"));
        server.join().unwrap();
    }

    #[test]
    fn dacp_slow_drip_cannot_extend_the_absolute_request_deadline() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let endpoint = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 512];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let count = stream.read(&mut chunk).unwrap();
                if count == 0 {
                    return;
                }
                request.extend_from_slice(&chunk[..count]);
            }
            // Each byte arrives well inside the old 700 ms per-read timeout,
            // but the response as a whole never completes within 700 ms.
            for _ in 0..12 {
                thread::sleep(Duration::from_millis(90));
                if stream.write_all(b"H").is_err() {
                    break;
                }
            }
        });

        let started = Instant::now();
        let gate = DacpWriteGate::new();
        let result = send_dacp_volume_guarded(endpoint, "1986535575", -18.0, &gate, || true);
        let elapsed = started.elapsed();
        assert!(result.is_err(), "an incomplete drip response must fail");
        assert!(
            elapsed < Duration::from_secs(2),
            "single-byte progress extended the total deadline to {elapsed:?}"
        );
        server.join().unwrap();
    }

    #[test]
    fn guarded_dacp_response_wait_observes_revocation_promptly() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let endpoint = listener.local_addr().unwrap();
        let (seen_tx, seen_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 512];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let count = stream.read(&mut chunk).unwrap();
                if count == 0 {
                    return;
                }
                request.extend_from_slice(&chunk[..count]);
            }
            seen_tx.send(()).unwrap();
            let _ = release_rx.recv_timeout(Duration::from_secs(1));
        });
        let current = Arc::new(AtomicBool::new(true));
        let worker_current = Arc::clone(&current);
        let gate = DacpWriteGate::new();
        let client = thread::spawn(move || {
            send_dacp_volume_guarded(endpoint, "1986535575", -18.0, &gate, || {
                worker_current.load(Ordering::Acquire)
            })
        });

        seen_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let revoked_at = Instant::now();
        current.store(false, Ordering::Release);
        let result = client.join().unwrap();
        assert!(
            result.is_err(),
            "bytes were written, so revocation is uncertain"
        );
        assert!(
            revoked_at.elapsed() < Duration::from_millis(400),
            "guarded response wait ignored revocation for {:?}",
            revoked_at.elapsed()
        );
        let _ = release_tx.send(());
        server.join().unwrap();
    }

    #[test]
    fn superseded_dacp_job_writes_no_http_bytes() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let endpoint = listener.local_addr().unwrap();
        let gate = DacpWriteGate::new();
        assert!(!send_dacp_volume_guarded(endpoint, "1986535575", -18.0, &gate, || false).unwrap());
        listener.set_nonblocking(true).unwrap();
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock)
        );
    }

    #[test]
    fn default_output_epoch_is_checked_before_each_airplay_reverse_poll() {
        let daemon = include_str!("lib.rs");
        let ticker = &daemon[daemon.find("fn ticker_loop").unwrap()..];
        let sub_tick = &ticker[..ticker.find("conn::ping_and_reap").unwrap()];
        let epoch = sub_tick
            .find("inner.dev_out_epoch.load")
            .expect("the 200 ms sub-tick must observe output replacements");
        let airplay = sub_tick
            .find(".volume_tick(&inner.dev_out_epoch, epoch, output_changed)")
            .expect("AirPlay must receive the sampled epoch and sub-tick decision");
        assert!(
            epoch < airplay,
            "new output volume was polled before replay"
        );
        assert!(!sub_tick.contains(".volume_tick(&inner.dev_out_epoch, epoch, false)"));
        assert!(!ticker.contains(".volume_tick(&inner.dev_out_epoch, epoch, true)"));
    }

    #[test]
    fn mdns_uses_upstream_txt_records_without_inventing_or_leaking_fields() {
        let records = vec![
            "txtvers=1".to_string(),
            "cn=0,1".to_string(),
            "pw=true".to_string(),
        ];
        assert_eq!(
            txt_pairs(&records).unwrap(),
            vec![
                ("txtvers".into(), "1".into()),
                ("cn".into(), "0,1".into()),
                ("pw".into(), "true".into()),
            ]
        );
        assert!(txt_pairs(&["broken".to_string()]).is_err());
    }

    #[test]
    fn effective_picker_name_is_one_shared_utf8_byte_bounded_value() {
        let custom = "客".repeat(48);
        let fitted = effective_name(&custom, "ignored");
        assert_eq!(fitted.as_bytes().len(), DNS_LABEL_MAX_BYTES);
        assert_eq!(fitted.chars().count(), 21);

        let followed = effective_name("", &"房".repeat(48));
        assert!(followed.len() <= AIRPLAY_PICKER_NAME_MAX_BYTES);
        assert!(followed.starts_with("AudioHub — "));
    }

    #[test]
    fn airplay_mac_is_stable_unique_material_with_local_unicast_bits() {
        let mac = airplay_mac_from_fingerprint("59857220d4b67e6e").expect("valid fingerprint");
        assert_eq!(mac, [0x5a, 0x85, 0x72, 0x20, 0xd4, 0xb6]);
        assert_eq!(mac[0] & 0x01, 0, "AirPlay identity must be unicast");
        assert_eq!(
            mac[0] & 0x02,
            0x02,
            "AirPlay identity must be locally administered"
        );
        assert_eq!(
            airplay_mac_from_fingerprint("58857220d4b67e6e"),
            Some([0x5a, 0x85, 0x72, 0x20, 0xd4, 0xb6]),
            "hardware multicast/local bits are identity metadata, not entropy"
        );
        assert!(airplay_mac_from_fingerprint("short").is_none());
        assert!(airplay_mac_from_fingerprint("zz857220d4b67e6e").is_none());
    }

    #[test]
    fn session_connected_ms_is_elapsed_duration_and_never_a_daemon_timestamp() {
        let session = RuntimeSessionInfo {
            id: 7,
            protocol: Protocol::AirPlay2,
            peer: Some("192.0.2.4".parse().unwrap()),
            sample_rate: 44_100,
            channels: 2,
            paused: false,
            volume_db: Some(-12.0),
            title: Some("Track".into()),
            artist: None,
            album: None,
            elapsed_ms: None,
            duration_ms: None,
            started_unix_ms: 1_000,
            selected_for_output: true,
        };
        let view = session_view(session.clone(), 4_250);
        assert_eq!(view.connected_ms, 3_250);
        assert_eq!(view.peer, "192.0.2.4");
        assert_eq!(session_view(session, 900).connected_ms, 0);
    }

    #[test]
    fn mdns_host_is_a_cross_platform_dns_label() {
        assert_eq!(dns_host_label("Living Room Mac"), "living-room-mac");
        assert_eq!(dns_host_label("WIN_PC__01"), "win-pc-01");
        assert_eq!(dns_host_label("客厅 Mac"), "mac");
        assert_eq!(dns_host_label("客厅"), "audiohub-host");
        let long = dns_host_label(&"A".repeat(100));
        assert_eq!(long.len(), DNS_LABEL_MAX_BYTES);
        assert!(long.bytes().all(|b| b.is_ascii_lowercase()));
    }
}
