//! User-session lifecycle for the AudioHub daemon.
//!
//! Windows keeps the daemon beside the App. macOS ships an immutable source
//! copy inside AudioHub.app, then installs a machine-locally signed copy under
//! `/Library/Application Support/AudioHub` after one explicit authorization.
//! In both cases the daemon still runs as the interactive user and is spawned
//! by the App: CoreAudio/WASAPI and macOS Local Network/TCC must not move into a
//! root/SYSTEM service context.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[cfg(target_os = "macos")]
use std::fs::{File, OpenOptions};
#[cfg(target_os = "macos")]
use std::os::fd::AsRawFd;
#[cfg(target_os = "macos")]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

use serde::Serialize;
use tauri::AppHandle;

#[cfg(target_os = "macos")]
use super::{authenticated_endpoint_alive, read_endpoint_raw, IPC_VERSION};
use super::{
    cli_binary, command_output_with_timeout, config_dir, endpoint_alive, ensure_daemon_blocking,
    read_endpoint, spawn_without_console, DaemonError, IpcEndpointJson,
};

#[cfg(target_os = "macos")]
const MAC_LABEL: &str = "com.audiohub.app.autostart";
#[cfg(target_os = "macos")]
const MAC_APP: &str = "/Applications/AudioHub.app";
#[cfg(target_os = "macos")]
const MAC_SERVICE_BASE: &str = "/Library/Application Support/AudioHub/service";
#[cfg(target_os = "macos")]
const MAC_DAEMON_IDENTIFIER: &str = "com.audiohub.daemon";
#[cfg(target_os = "macos")]
const MAC_DAEMON_INSTALLER_SHA256: &str = env!("AUDIOHUB_DAEMON_INSTALLER_SHA256");
#[cfg(target_os = "macos")]
const MAC_USER_LIFECYCLE_LOCK: &str = "service-lifecycle.lock";
#[cfg(target_os = "windows")]
const WIN_TASK: &str = "AudioHubDaemon";
const INSTALLED_MARKER: &str = "service-installed-v1";

/// A per-user, cross-process lock covering the whole daemon lifecycle
/// transaction. The privileged driver package has its own root-owned machine
/// lock; this one closes the earlier user-space window in which another App
/// instance could start audiohubd after the first one stopped it but before
/// PackageKit restarted CoreAudio.
#[cfg(target_os = "macos")]
#[derive(Debug)]
pub(crate) struct MacLifecycleGuard {
    file: File,
}

#[cfg(target_os = "macos")]
impl Drop for MacLifecycleGuard {
    fn drop(&mut self) {
        // Closing the descriptor also drops the flock. Unlock explicitly so the
        // ownership boundary is visible and testable; ignore failure during Drop.
        unsafe {
            let _ = libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

#[cfg(target_os = "macos")]
fn open_mac_lifecycle_lock(path: &Path, nonblocking: bool) -> std::io::Result<MacLifecycleGuard> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "AudioHub lifecycle lock is not a regular file owned by this user",
        ));
    }
    if metadata.mode() & 0o077 != 0 {
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }

    let operation = libc::LOCK_EX | if nonblocking { libc::LOCK_NB } else { 0 };
    loop {
        if unsafe { libc::flock(file.as_raw_fd(), operation) } == 0 {
            return Ok(MacLifecycleGuard { file });
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn lock_mac_lifecycle() -> Result<MacLifecycleGuard, DaemonError> {
    open_mac_lifecycle_lock(&config_dir().join(MAC_USER_LIFECYCLE_LOCK), true).map_err(|error| {
        let message = if error.kind() == std::io::ErrorKind::WouldBlock {
            "Another AudioHub service operation is still in progress"
        } else {
            "AudioHub could not enter its service lifecycle transaction"
        };
        DaemonError::new("service-conflict", message).with_detail(error.to_string())
    })
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct DaemonServiceStatus {
    /// The App contains the immutable daemon payload and its internal CLI.
    pub payload_present: bool,
    /// macOS setup is valid only from the stable Applications path.
    pub canonical: bool,
    /// This user completed setup before. This is distinct from the login item:
    /// disabling autostart must not make the next manual launch claim that the
    /// service was uninstalled.
    pub installed: bool,
    /// "missing", "current", or "stale".
    pub registration: &'static str,
    /// The target read from launchd/Task Scheduler, when available.
    pub target: Option<String>,
    /// A same-IPC daemon is accepting local connections now.
    pub running: bool,
}

fn app_bundle_of(exe: &Path) -> Option<PathBuf> {
    let macos = exe.parent()?;
    (macos.file_name()? == "MacOS").then_some(())?;
    let contents = macos.parent()?;
    (contents.file_name()? == "Contents").then_some(())?;
    let app = contents.parent()?;
    (app.extension()? == "app").then(|| app.to_path_buf())
}

fn same_path(actual: &Path, expected: &Path) -> bool {
    match (
        std::fs::canonicalize(actual),
        std::fs::canonicalize(expected),
    ) {
        (Ok(actual), Ok(expected)) => actual == expected,
        _ => actual == expected,
    }
}

fn expected_app_target() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    #[cfg(target_os = "macos")]
    {
        app_bundle_of(&exe)
    }
    #[cfg(target_os = "windows")]
    {
        Some(exe)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = exe;
        None
    }
}

#[cfg(target_os = "macos")]
fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

#[cfg(target_os = "macos")]
fn mac_daemon_installer_command(installer: &Path) -> Option<String> {
    is_lower_hex(MAC_DAEMON_INSTALLER_SHA256, 64).then_some(())?;
    Some(format!(
        "set -eu; \
         d=$(/usr/bin/mktemp -d /private/tmp/audiohub-daemon-installer.XXXXXX); \
         trap '/bin/rm -rf \"$d\"' EXIT; \
         trap 'exit 129' HUP; trap 'exit 130' INT; trap 'exit 143' TERM; \
         /usr/bin/install -o root -g wheel -m 0700 {source} \"$d/install-daemon.sh\"; \
         got=$(/usr/bin/shasum -a 256 \"$d/install-daemon.sh\" | /usr/bin/awk '{{print $1}}'); \
         [ \"$got\" = '{digest}' ]; \
         /bin/sh \"$d/install-daemon.sh\"",
        source = shell_quote(installer),
        digest = MAC_DAEMON_INSTALLER_SHA256,
    ))
}

#[cfg(target_os = "macos")]
fn is_lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Signature and digest probes are part of the UI's service-status path. Keep
/// each external inspection bounded so a damaged filesystem helper cannot
/// outlive the frontend's connection attempt and later create a ghost socket.
#[cfg(target_os = "macos")]
fn bounded_mac_inspection(command: &mut Command) -> Option<std::process::Output> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    spawn_without_console(command);
    command_output_with_timeout(command, std::time::Duration::from_secs(5)).ok()
}

#[cfg(target_os = "macos")]
fn mac_sha256(path: &Path) -> Option<String> {
    let mut command = Command::new("/usr/bin/shasum");
    command.args(["-a", "256"]).arg(path);
    let output = bounded_mac_inspection(&mut command)?;
    if !output.status.success() {
        return None;
    }
    let hash = String::from_utf8(output.stdout)
        .ok()?
        .split_whitespace()
        .next()?
        .to_ascii_lowercase();
    is_lower_hex(&hash, 64).then_some(hash)
}

#[cfg(target_os = "macos")]
fn mac_codesign_identifier(path: &Path) -> Option<String> {
    let mut command = Command::new("/usr/bin/codesign");
    command.args(["-d", "--verbose=4"]).arg(path);
    let output = bounded_mac_inspection(&mut command)?;
    // `codesign -d` writes its display data to stderr and may return success
    // only after the signature structure was parsed.
    output.status.success().then_some(())?;
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .find_map(|line| line.strip_prefix("Identifier="))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

#[cfg(target_os = "macos")]
fn bundled_mac_daemon() -> Option<(PathBuf, String)> {
    use std::os::unix::fs::MetadataExt;

    let app = expected_app_target()?;
    let mut app_check = Command::new("/usr/bin/codesign");
    app_check
        .args(["--verify", "--deep", "--strict"])
        .arg(&app);
    let app_verified = bounded_mac_inspection(&mut app_check)?.status.success();
    app_verified.then_some(())?;
    let daemon = app.join("Contents/MacOS/audiohubd");
    let metadata = std::fs::symlink_metadata(&daemon).ok()?;
    if !metadata.file_type().is_file()
        || metadata.file_type().is_symlink()
        || metadata.mode() & 0o111 == 0
        || mac_codesign_identifier(&daemon).as_deref() != Some(MAC_DAEMON_IDENTIFIER)
    {
        return None;
    }
    let mut daemon_check = Command::new("/usr/bin/codesign");
    daemon_check
        .args(["--verify", "--strict"])
        .arg(&daemon);
    let verified = bounded_mac_inspection(&mut daemon_check)?.status.success();
    verified.then_some(())?;
    let hash = mac_sha256(&daemon)?;
    Some((daemon, hash))
}

#[cfg(target_os = "macos")]
fn mac_root_owned_safe(path: &Path, expect_dir: bool) -> Option<std::fs::Metadata> {
    use std::os::unix::fs::MetadataExt;

    let metadata = std::fs::symlink_metadata(path).ok()?;
    let kind_ok = if expect_dir {
        metadata.file_type().is_dir()
    } else {
        metadata.file_type().is_file()
    };
    (kind_ok
        && !metadata.file_type().is_symlink()
        && metadata.uid() == 0
        && metadata.mode() & 0o022 == 0)
        .then_some(metadata)
}

#[cfg(target_os = "macos")]
fn mac_root_record(path: &Path, expected_length: usize) -> Option<String> {
    mac_root_owned_safe(path, false)?;
    let value = std::fs::read_to_string(path)
        .ok()?
        .trim()
        .to_ascii_lowercase();
    is_lower_hex(&value, expected_length).then_some(value)
}

#[cfg(target_os = "macos")]
fn verified_mac_daemon_version(source_hash: &str) -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;

    is_lower_hex(source_hash, 64).then_some(())?;
    let base = Path::new(MAC_SERVICE_BASE);
    let versions = base.join("versions");
    mac_root_owned_safe(base.parent()?, true)?;
    mac_root_owned_safe(base, true)?;
    mac_root_owned_safe(&versions, true)?;

    let version = versions.join(source_hash);
    mac_root_owned_safe(&version, true)?;
    if std::fs::canonicalize(&version).ok()? != version {
        return None;
    }
    let daemon = version.join("audiohubd");
    let daemon_metadata = mac_root_owned_safe(&daemon, false)?;
    if daemon_metadata.mode() & 0o111 == 0 {
        return None;
    }

    let recorded_source = mac_root_record(&version.join("source.sha256"), 64)?;
    let recorded_signed = mac_root_record(&version.join("signed.sha256"), 64)?;
    let certificate = mac_root_record(&version.join("certificate.sha1"), 40)?;
    if recorded_source != source_hash || mac_sha256(&daemon)? != recorded_signed {
        return None;
    }
    let requirement = format!(
        "=identifier \"{MAC_DAEMON_IDENTIFIER}\" and certificate leaf = H\"{certificate}\""
    );
    let mut daemon_check = Command::new("/usr/bin/codesign");
    daemon_check
        .args(["--verify", "--strict", "-R", &requirement])
        .arg(&daemon);
    let verified = bounded_mac_inspection(&mut daemon_check)?.status.success();
    verified.then_some(daemon)
}

/// Return only a fully verified runtime daemon matching this exact App build.
/// A stale version, changed symlink, writable file, changed digest, or changed
/// local signing certificate is treated as not installed and is repaired only
/// through the explicit authorized install action.
#[cfg(target_os = "macos")]
fn installed_mac_daemon_for_source(source_hash: &str) -> Option<PathBuf> {
    let base = Path::new(MAC_SERVICE_BASE);
    mac_root_owned_safe(base.parent()?, true)?;
    mac_root_owned_safe(base, true)?;

    let current = base.join("current");
    let current_metadata = std::fs::symlink_metadata(&current).ok()?;
    if !current_metadata.file_type().is_symlink() || current_metadata.uid() != 0 {
        return None;
    }
    let expected_link = PathBuf::from("versions").join(source_hash);
    if std::fs::read_link(&current).ok()? != expected_link {
        return None;
    }
    verified_mac_daemon_version(source_hash)
}

#[cfg(target_os = "macos")]
pub(crate) fn installed_daemon_binary() -> Option<PathBuf> {
    let (_, source_hash) = bundled_mac_daemon()?;
    installed_mac_daemon_for_source(&source_hash)
}

#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MacEndpointState {
    /// No live process and no listener remain for this stale endpoint file.
    Gone,
    /// The endpoint belongs to the exact verified daemon selected by this App.
    CurrentReady,
    /// The selected daemon process exists but its authenticated IPC is not ready.
    CurrentUnreachable,
    /// A different, still-verified AudioHub version owns authenticated IPC.
    ManagedOldReady,
    /// A verified old process exists, but IPC cannot authenticate a shutdown.
    ManagedOldUnreachable,
    /// PID/path/listener evidence does not describe a verified AudioHub daemon.
    Foreign,
}

#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MacImageClass {
    Current,
    ManagedOld,
    Foreign,
}

#[cfg(target_os = "macos")]
#[derive(Debug, Clone)]
struct MacEndpointInspection {
    state: MacEndpointState,
    image: Option<PathBuf>,
    started: Option<MacProcessStart>,
}

/// PID alone is not an identity: after a process exits macOS may reuse it. Keep
/// the kernel-reported start timestamp beside the verified executable path and
/// require both immediately before every signal.
#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MacProcessStart {
    seconds: u64,
    micros: u64,
}

/// Resolve a PID to the executable image which macOS is actually running.
///
/// `ipc.json` already carries the daemon PID. Binding that PID to the exact
/// root-owned, signed version path closes the gap left by a port-only probe: a
/// stale endpoint cannot make an older or unrelated process look current merely
/// because something accepts TCP on the recorded port.
#[cfg(target_os = "macos")]
fn mac_process_image(pid: u32) -> Option<PathBuf> {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let pid = i32::try_from(pid).ok().filter(|pid| *pid > 0)?;
    let mut buffer = vec![0_u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: `buffer` is writable for exactly the supplied byte length and
    // remains alive for the call. proc_pidpath writes at most that many bytes.
    let length = unsafe {
        libc::proc_pidpath(
            pid,
            buffer.as_mut_ptr().cast(),
            u32::try_from(buffer.len()).ok()?,
        )
    };
    if length <= 0 {
        return None;
    }
    let length = usize::try_from(length).ok()?.min(buffer.len());
    let bytes = &buffer[..length];
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    (end > 0).then(|| PathBuf::from(OsString::from_vec(bytes[..end].to_vec())))
}

#[cfg(target_os = "macos")]
fn mac_process_start(pid: u32) -> Option<MacProcessStart> {
    let pid = i32::try_from(pid).ok().filter(|pid| *pid > 0)?;
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let expected = i32::try_from(std::mem::size_of::<libc::proc_bsdinfo>()).ok()?;
    // SAFETY: `info` is writable for exactly `expected` bytes and the flavor's
    // documented result is `proc_bsdinfo`.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            expected,
        )
    };
    (written == expected).then_some(MacProcessStart {
        seconds: info.pbi_start_tvsec,
        micros: info.pbi_start_tvusec,
    })
}

#[cfg(target_os = "macos")]
fn mac_pid_alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal 0 performs no mutation; it only probes process existence.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(target_os = "macos")]
fn mac_managed_version_hash(path: &Path) -> Option<&str> {
    (path.file_name()? == "audiohubd").then_some(())?;
    let version = path.parent()?;
    (version.parent()? == Path::new(MAC_SERVICE_BASE).join("versions")).then_some(())?;
    let hash = version.file_name()?.to_str()?;
    is_lower_hex(hash, 64).then_some(hash)
}

#[cfg(target_os = "macos")]
fn verified_managed_mac_daemon(path: &Path) -> Option<PathBuf> {
    let hash = mac_managed_version_hash(path)?;
    let verified = verified_mac_daemon_version(hash)?;
    same_path(&verified, path).then_some(verified)
}

#[cfg(target_os = "macos")]
fn classify_mac_image(
    expected: Option<&Path>,
    image: Option<&Path>,
    managed_old: bool,
) -> MacImageClass {
    match image {
        Some(image) if expected.is_some_and(|expected| same_path(image, expected)) => {
            MacImageClass::Current
        }
        Some(_) if managed_old => MacImageClass::ManagedOld,
        _ => MacImageClass::Foreign,
    }
}

#[cfg(target_os = "macos")]
fn inspect_mac_endpoint(endpoint: &IpcEndpointJson) -> MacEndpointInspection {
    let expected = installed_daemon_binary();
    let image = mac_process_image(endpoint.pid);
    let started = mac_process_start(endpoint.pid);
    // A TCP accept proves only that something owns the port. The App must see a
    // token-authenticated daemon hello before it calls the service ready; in the
    // failure this guards, CoreAudio was wedged while the socket kept accepting
    // connections indefinitely.
    let reachable = authenticated_endpoint_alive(endpoint);
    let pid_alive = image.is_some() || mac_pid_alive(endpoint.pid);
    if !pid_alive && !reachable {
        return MacEndpointInspection {
            state: MacEndpointState::Gone,
            image: None,
            started: None,
        };
    }
    let managed_old = image
        .as_deref()
        .and_then(verified_managed_mac_daemon)
        .is_some();
    let class = classify_mac_image(expected.as_deref(), image.as_deref(), managed_old);
    let state = match (class, reachable) {
        (MacImageClass::Current, true) => MacEndpointState::CurrentReady,
        (MacImageClass::Current, false) => MacEndpointState::CurrentUnreachable,
        (MacImageClass::ManagedOld, true) => MacEndpointState::ManagedOldReady,
        (MacImageClass::ManagedOld, false) => MacEndpointState::ManagedOldUnreachable,
        (MacImageClass::Foreign, _) => MacEndpointState::Foreign,
    };
    MacEndpointInspection {
        state,
        image,
        started,
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn mac_endpoint_state(endpoint: &IpcEndpointJson) -> MacEndpointState {
    inspect_mac_endpoint(endpoint).state
}

#[cfg(not(target_os = "macos"))]
fn bundled_payload_present() -> bool {
    super::daemon_binary().is_some() && cli_binary().is_some()
}

#[cfg(target_os = "macos")]
fn install_mac_daemon(app: &AppHandle) -> Result<bool, DaemonError> {
    let current = expected_app_target().ok_or_else(|| {
        DaemonError::new(
            "install-failed",
            "AudioHub is not running from a complete App bundle",
        )
    })?;
    if !same_target(&current.display().to_string(), Path::new(MAC_APP)) {
        return Err(DaemonError::new(
            "install-failed",
            "Install and open AudioHub using AudioHub.pkg before installing its service",
        ));
    }
    if installed_daemon_binary().is_some() {
        return Ok(false);
    }

    let installer = current.join("Contents/Resources/installer/install-daemon.sh");
    let metadata = std::fs::symlink_metadata(&installer).map_err(|error| {
        DaemonError::new("no-binary", "AudioHub is missing its service installer")
            .with_detail(error.to_string())
    })?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(DaemonError::new(
            "no-binary",
            "AudioHub service installer is unsafe or incomplete",
        ));
    }
    if !is_lower_hex(MAC_DAEMON_INSTALLER_SHA256, 64)
        || mac_sha256(&installer).as_deref() != Some(MAC_DAEMON_INSTALLER_SHA256)
    {
        return Err(DaemonError::new(
            "no-binary",
            "AudioHub service installer does not match this App build",
        ));
    }
    // The authorization dialog is a scheduling window: never let the elevated
    // shell parse a script directly from /Applications after that pause. Root
    // copies the fixed resource into its own 0700 mktemp directory, verifies
    // the compile-time digest again, and executes only that private snapshot.
    let command = mac_daemon_installer_command(&installer).ok_or_else(|| {
        DaemonError::new(
            "no-binary",
            "AudioHub was built without a service installer digest",
        )
    })?;
    super::driver_install::mac_run_admin_shell(app, &command).map_err(|error| {
        DaemonError::new(
            if error.kind == "user-cancelled" {
                "user-cancelled"
            } else {
                "install-failed"
            },
            "Could not sign and install the AudioHub service",
        )
        .with_detail(error.detail.unwrap_or(error.message))
    })?;
    installed_daemon_binary().ok_or_else(|| {
        DaemonError::new(
            "install-failed",
            "The authorized service install completed, but verification failed",
        )
    })?;
    Ok(true)
}

#[cfg(target_os = "macos")]
#[derive(Debug, Clone)]
struct MacManagedOwner {
    endpoint: Option<IpcEndpointJson>,
    pid: u32,
    image: PathBuf,
    started: MacProcessStart,
}

#[cfg(target_os = "macos")]
fn mac_owner_matches_with<F, G>(owner: &MacManagedOwner, image_for_pid: F, start_for_pid: G) -> bool
where
    F: FnOnce(u32) -> Option<PathBuf>,
    G: FnOnce(u32) -> Option<MacProcessStart>,
{
    image_for_pid(owner.pid)
        .as_deref()
        .is_some_and(|image| same_path(image, &owner.image))
        && start_for_pid(owner.pid) == Some(owner.started)
}

#[cfg(target_os = "macos")]
fn mac_owner_still_running(owner: &MacManagedOwner) -> bool {
    mac_owner_matches_with(owner, mac_process_image, mac_process_start)
}

#[cfg(target_os = "macos")]
fn wait_for_mac_owner_exit(owner: &MacManagedOwner, timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while mac_owner_still_running(owner) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    !mac_owner_still_running(owner)
}

#[cfg(target_os = "macos")]
fn mac_service_conflict(message: impl Into<String>, detail: impl Into<String>) -> DaemonError {
    DaemonError::new("service-conflict", message).with_detail(detail)
}

#[cfg(target_os = "macos")]
fn mac_state_is_verified(state: MacEndpointState) -> bool {
    matches!(
        state,
        MacEndpointState::CurrentReady
            | MacEndpointState::CurrentUnreachable
            | MacEndpointState::ManagedOldReady
            | MacEndpointState::ManagedOldUnreachable
    )
}

#[cfg(target_os = "macos")]
fn mac_process_owned_by_current_user(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let Ok(expected) = i32::try_from(std::mem::size_of::<libc::proc_bsdinfo>()) else {
        return false;
    };
    // SAFETY: `info` is writable for the exact structure size supplied to the
    // documented PROC_PIDTBSDINFO flavor.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            expected,
        )
    };
    written == expected && info.pbi_uid == unsafe { libc::geteuid() }
}

#[cfg(target_os = "macos")]
fn verified_mac_owner_from_endpoint(
    endpoint: IpcEndpointJson,
    inspection: &MacEndpointInspection,
) -> Result<MacManagedOwner, DaemonError> {
    if !mac_state_is_verified(inspection.state) {
        return Err(mac_service_conflict(
            "Another process owns the AudioHub service endpoint",
            "AudioHub refused to capture an unverified process",
        ));
    }
    let image = inspection.image.clone().ok_or_else(|| {
        mac_service_conflict(
            "Could not identify the AudioHub service",
            "The endpoint PID has no executable image",
        )
    })?;
    let started = inspection.started.ok_or_else(|| {
        mac_service_conflict(
            "Could not identify the AudioHub service",
            "The endpoint PID has no stable process start time",
        )
    })?;
    if verified_managed_mac_daemon(&image).is_none()
        || !mac_process_owned_by_current_user(endpoint.pid)
    {
        return Err(mac_service_conflict(
            "Another process owns the AudioHub service endpoint",
            "The endpoint PID is not this user's verified managed AudioHub daemon",
        ));
    }
    Ok(MacManagedOwner {
        pid: endpoint.pid,
        endpoint: Some(endpoint),
        image,
        started,
    })
}

#[cfg(target_os = "macos")]
fn mac_all_pids_with<F>(mut list_all: F) -> std::io::Result<Vec<u32>>
where
    F: FnMut(*mut libc::c_void, libc::c_int) -> libc::c_int,
{
    // A zero-sized call returns the current process count. Leave growth room:
    // process creation between the two calls may otherwise truncate the tail.
    let estimate = list_all(std::ptr::null_mut(), 0);
    if estimate <= 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut capacity = usize::try_from(estimate)
        .map_err(|_| std::io::Error::other("invalid macOS process count"))?
        .checked_add(64)
        .ok_or_else(|| std::io::Error::other("macOS process count overflow"))?;
    for _ in 0..4 {
        let mut pids = vec![0_i32; capacity];
        let bytes = pids
            .len()
            .checked_mul(std::mem::size_of::<i32>())
            .and_then(|bytes| i32::try_from(bytes).ok())
            .ok_or_else(|| std::io::Error::other("macOS process buffer is too large"))?;
        let count = list_all(pids.as_mut_ptr().cast(), bytes);
        if count < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let count = usize::try_from(count)
            .map_err(|_| std::io::Error::other("invalid macOS process result"))?;
        if count < capacity {
            pids.truncate(count);
            return Ok(pids
                .into_iter()
                .filter_map(|pid| u32::try_from(pid).ok().filter(|pid| *pid > 0))
                .collect());
        }
        capacity = capacity
            .checked_mul(2)
            .ok_or_else(|| std::io::Error::other("macOS process count overflow"))?;
    }
    Err(std::io::Error::other(
        "macOS process list kept growing while AudioHub inspected it",
    ))
}

#[cfg(target_os = "macos")]
fn mac_all_pids() -> std::io::Result<Vec<u32>> {
    mac_all_pids_with(|buffer, size| {
        // SAFETY: mac_all_pids_with provides either the documented null query
        // or a writable i32 vector covering exactly `size` bytes.
        unsafe { libc::proc_listallpids(buffer, size) }
    })
}

/// Discover daemon processes which can no longer publish ipc.json because they
/// are already inside shutdown. The scan is intentionally much stricter than a
/// name match: only this user's process at the root-owned
/// `versions/<64hex>/audiohubd` image, passing the existing signature/hash
/// verifier, is eligible for a signal.
#[cfg(target_os = "macos")]
fn scanned_verified_mac_owners(
    exclude_pid: Option<u32>,
) -> Result<Vec<MacManagedOwner>, DaemonError> {
    let mut owners = Vec::new();
    let pids = mac_all_pids().map_err(|error| {
        mac_service_conflict(
            "AudioHub could not inspect existing service processes",
            format!("Refused to start or restart while orphan detection was unavailable: {error}"),
        )
    })?;
    for pid in pids {
        if exclude_pid == Some(pid) || !mac_process_owned_by_current_user(pid) {
            continue;
        }
        let Some(image) = mac_process_image(pid) else {
            continue;
        };
        if verified_managed_mac_daemon(&image).is_none() {
            continue;
        }
        let Some(started) = mac_process_start(pid) else {
            continue;
        };
        owners.push(MacManagedOwner {
            endpoint: None,
            pid,
            image,
            started,
        });
    }
    owners.sort_by_key(|owner| owner.pid);
    owners.dedup_by_key(|owner| owner.pid);
    Ok(owners)
}

#[cfg(target_os = "macos")]
fn signal_verified_mac_owner(owner: &MacManagedOwner, signal: i32) -> Result<(), DaemonError> {
    if !mac_owner_still_running(owner) {
        return Ok(());
    }
    // Re-run the full path/signature/hash verifier and the user check directly
    // before every signal. A PID from a stale endpoint or scan is never enough.
    if verified_managed_mac_daemon(&owner.image).is_none()
        || !mac_process_owned_by_current_user(owner.pid)
        || !mac_owner_still_running(owner)
    {
        return Err(mac_service_conflict(
            "The AudioHub service identity changed before it could be stopped",
            format!(
                "AudioHub refused to signal pid {} because its verified path or start time changed",
                owner.pid
            ),
        ));
    }
    let pid = i32::try_from(owner.pid).map_err(|_| {
        mac_service_conflict(
            "The AudioHub service identity is invalid",
            format!("Refused to signal out-of-range pid {}", owner.pid),
        )
    })?;
    // SAFETY: positive PID and signal are fixed from a captured, re-verified
    // same-user process. No PID parsed from an external command is used here.
    if unsafe { libc::kill(pid, signal) } == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) && !mac_owner_still_running(owner) {
        return Ok(());
    }
    Err(
        DaemonError::new("stop-failed", "Could not stop the AudioHub service").with_detail(
            format!(
                "signal {signal} to verified pid {} failed: {error}",
                owner.pid
            ),
        ),
    )
}

/// Stop one captured verified owner. Authenticated IPC gets the first chance
/// when the captured current-version endpoint is still ready. If that helper
/// times out, continue by the captured PID/path/start identity: ipc.json is
/// removed at the *start* of shutdown and cannot be used as exit evidence.
#[cfg(target_os = "macos")]
fn stop_verified_mac_owner(owner: &MacManagedOwner) -> Result<(), DaemonError> {
    if !mac_owner_still_running(owner) {
        return Ok(());
    }

    let mut diagnostics = Vec::new();
    let mut graceful_requested = false;
    if let Some(endpoint) = owner.endpoint.as_ref().filter(|endpoint| {
        endpoint.ipc_version == IPC_VERSION && read_endpoint_raw().as_ref() == Some(*endpoint)
    }) {
        let inspection = inspect_mac_endpoint(endpoint);
        let ready = matches!(
            inspection.state,
            MacEndpointState::CurrentReady | MacEndpointState::ManagedOldReady
        ) && inspection
            .image
            .as_deref()
            .is_some_and(|image| same_path(image, &owner.image))
            && inspection.started == Some(owner.started);
        if ready {
            if let Some(cli) = cli_binary() {
                let mut command = Command::new(cli);
                command
                    .args(["ctl", "shutdown", "--json"])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped());
                spawn_without_console(&mut command);
                graceful_requested = true;
                match command_output_with_timeout(&mut command, std::time::Duration::from_secs(3)) {
                    Ok(output) if output.status.success() => {}
                    Ok(output) => diagnostics.push(format!(
                        "authenticated shutdown failed: {}",
                        String::from_utf8_lossy(&output.stderr).trim()
                    )),
                    Err(error) => diagnostics.push(format!("authenticated shutdown: {error}")),
                }
            } else {
                diagnostics.push("authenticated shutdown CLI is unavailable".to_string());
            }
        }
    }

    if graceful_requested && wait_for_mac_owner_exit(owner, std::time::Duration::from_secs(3)) {
        return Ok(());
    }
    signal_verified_mac_owner(owner, libc::SIGTERM)?;
    if wait_for_mac_owner_exit(owner, std::time::Duration::from_secs(3)) {
        return Ok(());
    }
    signal_verified_mac_owner(owner, libc::SIGKILL)?;
    if wait_for_mac_owner_exit(owner, std::time::Duration::from_secs(3)) {
        return Ok(());
    }

    let mut detail = format!(
        "verified pid {} is still running {}",
        owner.pid,
        owner.image.display()
    );
    if !diagnostics.is_empty() {
        detail.push_str("; ");
        detail.push_str(&diagnostics.join("; "));
    }
    Err(DaemonError::new("stop-failed", "The AudioHub service did not stop").with_detail(detail))
}

#[cfg(target_os = "macos")]
fn stop_scanned_verified_mac_owners(exclude_pid: Option<u32>) -> Result<usize, DaemonError> {
    let owners = scanned_verified_mac_owners(exclude_pid)?;
    let count = owners.len();
    for owner in &owners {
        stop_verified_mac_owner(owner)?;
    }
    Ok(count)
}

/// Stop the exact authenticated macOS daemon before PackageKit restarts
/// coreaudiod. Keeping the old daemon alive across that transition can leave its
/// CoreAudio calls parked in the dying server and make the replacement look
/// ready merely because its IPC port accepts TCP.
#[cfg(target_os = "macos")]
pub(crate) fn stop_mac_daemon_for_driver_install_unlocked(
    _guard: &MacLifecycleGuard,
) -> Result<bool, DaemonError> {
    let Some(endpoint) = read_endpoint_raw() else {
        return Ok(stop_scanned_verified_mac_owners(None)? > 0);
    };
    let inspection = inspect_mac_endpoint(&endpoint);
    if inspection.state == MacEndpointState::Foreign {
        return Err(mac_service_conflict(
            "Another process owns the AudioHub service endpoint",
            "Driver installation refused to stop an unverified process",
        ));
    }
    if inspection.state == MacEndpointState::Gone {
        return Ok(stop_scanned_verified_mac_owners(None)? > 0);
    }
    let owner = verified_mac_owner_from_endpoint(endpoint, &inspection)?;
    stop_verified_mac_owner(&owner)?;
    stop_scanned_verified_mac_owners(None)?;
    Ok(true)
}

#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MacActivationPlan {
    Spawn,
    Keep,
    ReplaceVerified(MacEndpointState),
    Conflict,
}

#[cfg(target_os = "macos")]
fn mac_activation_plan(
    state: Option<MacEndpointState>,
    restart_current: bool,
) -> MacActivationPlan {
    match state {
        None | Some(MacEndpointState::Gone) => MacActivationPlan::Spawn,
        Some(MacEndpointState::CurrentReady) if !restart_current => MacActivationPlan::Keep,
        Some(
            state @ (MacEndpointState::CurrentReady
            | MacEndpointState::CurrentUnreachable
            | MacEndpointState::ManagedOldReady
            | MacEndpointState::ManagedOldUnreachable),
        ) => MacActivationPlan::ReplaceVerified(state),
        Some(MacEndpointState::Foreign) => MacActivationPlan::Conflict,
    }
}

/// Activate the selected macOS runtime without ever killing an unverified
/// process. Replacement tries token-authenticated IPC when compatible, then
/// remains bound to the captured PID + executable image + start time for any
/// signal fallback. `restart_current` is deliberately separate from image
/// identity: a repair can replace bytes/metadata at the same content path while
/// a pre-repair process is still running from that exact path.
#[cfg(target_os = "macos")]
fn activate_mac_daemon_blocking_unlocked(
    _guard: &MacLifecycleGuard,
    restart_current: bool,
) -> Result<IpcEndpointJson, DaemonError> {
    installed_daemon_binary().ok_or_else(|| {
        DaemonError::new(
            "no-binary",
            "No verified installed AudioHub service is available",
        )
        .with_detail("Use the App's Install action before starting the service")
    })?;
    let endpoint = read_endpoint_raw();
    let inspection = endpoint.as_ref().map(inspect_mac_endpoint);
    let state = inspection.as_ref().map(|inspection| inspection.state);
    match mac_activation_plan(state, restart_current) {
        MacActivationPlan::Spawn => {
            stop_scanned_verified_mac_owners(None)?;
        }
        MacActivationPlan::Keep => {
            let endpoint = endpoint.ok_or_else(|| {
                DaemonError::new("internal", "AudioHub lost its current service endpoint")
            })?;
            stop_scanned_verified_mac_owners(Some(endpoint.pid))?;
            return Ok(endpoint);
        }
        MacActivationPlan::ReplaceVerified(_expected_state) => {
            let endpoint = endpoint.ok_or_else(|| {
                DaemonError::new(
                    "internal",
                    "AudioHub lost its service endpoint before restart",
                )
            })?;
            let inspection = inspection.as_ref().ok_or_else(|| {
                DaemonError::new("internal", "AudioHub lost its service inspection")
            })?;
            let owner = verified_mac_owner_from_endpoint(endpoint, inspection)?;
            stop_verified_mac_owner(&owner)?;
            stop_scanned_verified_mac_owners(None)?;
        }
        MacActivationPlan::Conflict => {
            return Err(mac_service_conflict(
                "Another process owns the AudioHub service endpoint",
                "AudioHub refused to stop or replace an unverified process",
            ));
        }
    }
    ensure_daemon_blocking()
}

#[cfg(target_os = "macos")]
pub(crate) fn start_mac_daemon_blocking() -> Result<IpcEndpointJson, DaemonError> {
    let guard = lock_mac_lifecycle()?;
    start_mac_daemon_blocking_unlocked(&guard)
}

#[cfg(target_os = "macos")]
fn restart_mac_daemon_blocking() -> Result<IpcEndpointJson, DaemonError> {
    let guard = lock_mac_lifecycle()?;
    restart_mac_daemon_blocking_unlocked(&guard)
}

#[cfg(target_os = "macos")]
pub(crate) fn start_mac_daemon_blocking_unlocked(
    guard: &MacLifecycleGuard,
) -> Result<IpcEndpointJson, DaemonError> {
    activate_mac_daemon_blocking_unlocked(guard, false)
}

#[cfg(target_os = "macos")]
fn restart_mac_daemon_blocking_unlocked(
    guard: &MacLifecycleGuard,
) -> Result<IpcEndpointJson, DaemonError> {
    activate_mac_daemon_blocking_unlocked(guard, true)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RegistrationInspection {
    target: Option<String>,
    current: bool,
}

#[cfg(target_os = "macos")]
fn inspect_mac_registration(body: &str, expected: Option<&Path>) -> RegistrationInspection {
    let inspection = audiohub_security::inspect_macos_launch_agent_plist(body, MAC_LABEL, expected);
    RegistrationInspection {
        target: inspection.target,
        current: inspection.current,
    }
}

#[cfg(target_os = "macos")]
fn registered_target() -> Option<RegistrationInspection> {
    let home = std::env::var_os("HOME")?;
    let path = PathBuf::from(home)
        .join("Library/LaunchAgents")
        .join(format!("{MAC_LABEL}.plist"));
    let body = match std::fs::read_to_string(&path) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        // The registration is present but unreadable. It is stale, not missing:
        // package bootstrap must not mistake it for the user's explicit opt-out.
        Err(_) => {
            return Some(RegistrationInspection {
                target: None,
                current: false,
            })
        }
    };
    let expected = expected_app_target();
    Some(inspect_mac_registration(&body, expected.as_deref()))
}

#[cfg(target_os = "windows")]
fn registered_target() -> Option<RegistrationInspection> {
    let mut cmd = Command::new("schtasks.exe");
    cmd.args(["/Query", "/TN", WIN_TASK, "/XML"]);
    spawn_without_console(&mut cmd);
    let output = cmd.output().ok()?;
    if !output.status.success() {
        return None;
    }
    // schtasks emits UTF-16LE XML on many Windows versions. Decode it without
    // trusting the current console code page.
    let bytes = output.stdout;
    let body =
        if bytes.starts_with(&[0xff, 0xfe]) || bytes.iter().skip(1).step_by(2).all(|b| *b == 0) {
            let words: Vec<u16> = bytes
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .skip_while(|word| *word == 0xfeff)
                .collect();
            String::from_utf16_lossy(&words)
        } else {
            String::from_utf8_lossy(&bytes).into_owned()
        };
    let Some((expected, sid)) =
        expected_app_target().zip(audiohub_security::current_user_sid_string().ok())
    else {
        return Some(RegistrationInspection {
            target: None,
            current: false,
        });
    };
    let inspection = audiohub_security::inspect_windows_task_xml(&body, &expected, &sid);
    Some(RegistrationInspection {
        target: inspection.target,
        current: inspection.current,
    })
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn registered_target() -> Option<RegistrationInspection> {
    None
}

fn registration_kind(inspection: Option<&RegistrationInspection>) -> &'static str {
    match inspection {
        None => "missing",
        Some(inspection) if inspection.current => "current",
        Some(_) => "stale",
    }
}

fn same_target(actual: &str, expected: &Path) -> bool {
    same_path(Path::new(actual), expected)
}

fn status_blocking() -> DaemonServiceStatus {
    #[cfg(target_os = "macos")]
    let bundled = bundled_mac_daemon();
    #[cfg(target_os = "macos")]
    let payload_present = bundled.is_some() && cli_binary().is_some();
    #[cfg(not(target_os = "macos"))]
    let payload_present = bundled_payload_present();
    #[cfg(target_os = "macos")]
    let canonical = expected_app_target()
        .as_deref()
        .is_some_and(|app| same_target(&app.display().to_string(), Path::new(MAC_APP)));
    #[cfg(not(target_os = "macos"))]
    let canonical = true;
    let inspection = registered_target();
    let registration = registration_kind(inspection.as_ref());
    let target = inspection.and_then(|inspection| inspection.target);
    #[cfg(target_os = "macos")]
    let runtime_daemon = bundled
        .as_ref()
        .and_then(|(_, hash)| installed_mac_daemon_for_source(hash));
    #[cfg(target_os = "macos")]
    let runtime_installed = runtime_daemon.is_some();
    #[cfg(not(target_os = "macos"))]
    let runtime_installed = payload_present;
    let installed = canonical
        && runtime_installed
        && (config_dir().join(INSTALLED_MARKER).is_file() || registration == "current");
    #[cfg(target_os = "macos")]
    let running = runtime_daemon.is_some()
        && read_endpoint()
            .as_ref()
            .is_some_and(|endpoint| mac_endpoint_state(endpoint) == MacEndpointState::CurrentReady);
    #[cfg(not(target_os = "macos"))]
    let running = read_endpoint()
        .as_ref()
        .map(endpoint_alive)
        .unwrap_or(false);
    DaemonServiceStatus {
        payload_present,
        canonical,
        installed,
        registration,
        target,
        running,
    }
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn daemon_running() -> bool {
    read_endpoint()
        .as_ref()
        .map(endpoint_alive)
        .unwrap_or(false)
}

/// Wait until the endpoint written by the currently running daemon stops
/// accepting connections. A stale endpoint file is expected after a clean
/// shutdown; the listening socket, not file removal, is the authoritative
/// boundary before a same-version replacement may be started.
#[cfg(not(target_os = "macos"))]
pub(crate) fn wait_for_daemon_exit(timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while daemon_running() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    !daemon_running()
}

#[tauri::command]
pub(crate) async fn daemon_service_status() -> DaemonServiceStatus {
    tauri::async_runtime::spawn_blocking(status_blocking)
        .await
        .unwrap_or_else(|_| DaemonServiceStatus {
            payload_present: false,
            canonical: false,
            installed: false,
            registration: "missing",
            target: None,
            running: false,
        })
}

fn set_autostart_once(want: bool) -> Result<(), DaemonError> {
    let cli = cli_binary().ok_or_else(|| {
        DaemonError::new(
            "no-binary",
            "AudioHub is missing its internal service tools",
        )
    })?;
    let mut cmd = Command::new(&cli);
    let setting = if want {
        "--autostart=true"
    } else {
        "--autostart=false"
    };
    cmd.args(["ctl", "settings", setting, "--json"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    spawn_without_console(&mut cmd);
    let output =
        command_output_with_timeout(&mut cmd, std::time::Duration::from_secs(3)).map_err(|e| {
            DaemonError::new(
                "install-failed",
                "Could not register the AudioHub background service",
            )
            .with_detail(e.to_string())
        })?;
    if !output.status.success() {
        return Err(DaemonError::new(
            "install-failed",
            "Could not register the AudioHub background service",
        )
        .with_detail(String::from_utf8_lossy(&output.stderr).into_owned()));
    }
    Ok(())
}

fn set_autostart_via_daemon() -> Result<(), DaemonError> {
    // The daemon's current state machine treats an existing-but-stale entry as
    // Apply, so one `true` repairs it in place. Never pre-delete it: if repair
    // then failed, a two-step false->true sequence would destroy the user's
    // existing login choice. A missing entry plus an existing install marker is
    // handled by package_bootstrap_blocking and remains the explicit opt-out.
    set_autostart_once(true)?;
    let state = status_blocking();
    if state.registration != "current" {
        return Err(DaemonError::new(
            "install-failed",
            "AudioHub registered a background entry, but verification failed",
        )
        .with_detail(
            state
                .target
                .unwrap_or_else(|| "no registered target".into()),
        ));
    }
    Ok(())
}

#[cfg_attr(target_os = "macos", allow(dead_code))]
pub(crate) fn complete_install_blocking() -> Result<IpcEndpointJson, DaemonError> {
    let was_running = read_endpoint()
        .as_ref()
        .map(endpoint_alive)
        .unwrap_or(false);
    let ep = ensure_daemon_blocking()?;
    if let Err(error) = set_autostart_via_daemon() {
        if !was_running {
            super::shutdown_daemon_blocking();
        }
        return Err(error);
    }
    let record = std::fs::create_dir_all(config_dir())
        .map_err(|e| {
            DaemonError::new("install-failed", "Could not record AudioHub service setup")
                .with_detail(e.to_string())
        })
        .and_then(|_| {
            std::fs::write(config_dir().join(INSTALLED_MARKER), b"1\n").map_err(|e| {
                DaemonError::new("install-failed", "Could not record AudioHub service setup")
                    .with_detail(e.to_string())
            })
        });
    if let Err(error) = record {
        // Do not leave a successful-looking login entry behind when setup
        // could not record completion. The native frontend gates every future
        // connection on this marker/registration state and will offer Install
        // again instead of silently connecting around the failure.
        let _ = set_autostart_once(false);
        if !was_running {
            super::shutdown_daemon_blocking();
        }
        return Err(error);
    }
    Ok(ep)
}

/// Complete per-user setup after the OS package (macOS) or application
/// installer (Windows) placed immutable program files. Fresh installs default
/// login startup on; an upgrade respects marker + missing registration as the
/// user's explicit opt-out.
fn should_register_on_bootstrap(marker_exists: bool, registration: &str) -> bool {
    !marker_exists || registration != "missing"
}

fn finish_package_bootstrap(
    before: DaemonServiceStatus,
    marker_exists: bool,
    should_register: bool,
    ep: IpcEndpointJson,
) -> Result<IpcEndpointJson, DaemonError> {
    let was_running = before.running;
    if should_register {
        if let Err(error) = set_autostart_via_daemon() {
            if !was_running {
                super::shutdown_daemon_blocking();
            }
            return Err(error);
        }
    }
    if let Err(error) = std::fs::create_dir_all(config_dir())
        .and_then(|_| std::fs::write(config_dir().join(INSTALLED_MARKER), b"1\n"))
    {
        if !marker_exists && should_register {
            let _ = set_autostart_once(false);
        }
        if !was_running {
            super::shutdown_daemon_blocking();
        }
        return Err(
            DaemonError::new("install-failed", "Could not record AudioHub service setup")
                .with_detail(error.to_string()),
        );
    }
    Ok(ep)
}

#[cfg(target_os = "macos")]
fn package_bootstrap_blocking_unlocked(
    guard: &MacLifecycleGuard,
) -> Result<IpcEndpointJson, DaemonError> {
    let before = status_blocking();
    let marker_exists = config_dir().join(INSTALLED_MARKER).is_file();
    let should_register = should_register_on_bootstrap(marker_exists, before.registration);
    let ep = start_mac_daemon_blocking_unlocked(guard)?;
    finish_package_bootstrap(before, marker_exists, should_register, ep)
}

#[cfg(target_os = "macos")]
pub(crate) fn package_bootstrap_blocking() -> Result<IpcEndpointJson, DaemonError> {
    let guard = lock_mac_lifecycle()?;
    package_bootstrap_blocking_unlocked(&guard)
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn package_bootstrap_blocking() -> Result<IpcEndpointJson, DaemonError> {
    let before = status_blocking();
    let marker_exists = config_dir().join(INSTALLED_MARKER).is_file();
    let should_register = should_register_on_bootstrap(marker_exists, before.registration);
    let ep = ensure_daemon_blocking()?;
    finish_package_bootstrap(before, marker_exists, should_register, ep)
}

fn install_blocking(app: &AppHandle) -> Result<IpcEndpointJson, DaemonError> {
    #[cfg(target_os = "macos")]
    {
        let guard = lock_mac_lifecycle()?;
        // The privileged transaction completes before any live process is
        // touched, so cancelling authorization leaves working audio alone.
        // A changed runtime is then restarted only after binding its PID to a
        // verified AudioHub image and authenticating over IPC; foreign owners
        // are reported and are never signalled or killed.
        let runtime_changed = install_mac_daemon(app)?;
        if runtime_changed {
            // A repair can replace a version directory in place. Its content
            // path then still classifies the pre-repair process as current, so
            // force an authenticated restart instead of relying on path change.
            restart_mac_daemon_blocking_unlocked(&guard)?;
        }
        // First setup enables login startup. An upgrade respects an existing
        // marker plus missing registration as the user's explicit opt-out.
        return package_bootstrap_blocking_unlocked(&guard);
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = app;
        complete_install_blocking()
    }
}

fn daemon_install_diagnostic(error: &DaemonError) -> String {
    // Keep this deliberately narrower than a command/process dump: the
    // authorized shell command and its environment can contain installation
    // internals, while DaemonError already carries the fixed operation's safe
    // diagnostic result. JSON encoding also keeps multiline stderr on one log
    // record without losing its original detail.
    serde_json::json!({
        "event": "daemon_service_install_failed",
        "kind": error.kind,
        "message": &error.message,
        "detail": error.detail.as_deref(),
    })
    .to_string()
}

#[tauri::command]
pub(crate) async fn install_daemon_service(app: AppHandle) -> Result<IpcEndpointJson, DaemonError> {
    let worker_app = app.clone();
    let result =
        match tauri::async_runtime::spawn_blocking(move || install_blocking(&worker_app)).await {
            Ok(result) => result,
            Err(error) => Err(DaemonError::new(
                "internal",
                format!("service install task failed: {error}"),
            )),
        };
    if let Err(error) = &result {
        super::warn(&daemon_install_diagnostic(error));
    }
    result
}

#[tauri::command]
pub(crate) async fn start_daemon_service() -> Result<IpcEndpointJson, DaemonError> {
    #[cfg(target_os = "macos")]
    let task = tauri::async_runtime::spawn_blocking(start_mac_daemon_blocking);
    #[cfg(not(target_os = "macos"))]
    let task = tauri::async_runtime::spawn_blocking(ensure_daemon_blocking);
    task.await
        .map_err(|e| DaemonError::new("internal", format!("service start task failed: {e}")))?
}

#[tauri::command]
pub(crate) async fn restart_daemon_service() -> Result<IpcEndpointJson, DaemonError> {
    #[cfg(target_os = "macos")]
    let task = tauri::async_runtime::spawn_blocking(restart_mac_daemon_blocking);
    #[cfg(not(target_os = "macos"))]
    let task = tauri::async_runtime::spawn_blocking(|| {
        super::shutdown_daemon_blocking();
        if !wait_for_daemon_exit(std::time::Duration::from_secs(8)) {
            return Err(DaemonError::new(
                "stop-failed",
                "The AudioHub service did not stop, so it was not restarted",
            ));
        }
        ensure_daemon_blocking()
    });
    task.await
        .map_err(|e| DaemonError::new("internal", format!("service restart task failed: {e}")))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_bundle_shape_is_strict() {
        assert_eq!(
            app_bundle_of(Path::new(
                "/Applications/AudioHub.app/Contents/MacOS/audiohub-app"
            )),
            Some(PathBuf::from("/Applications/AudioHub.app"))
        );
        assert_eq!(
            app_bundle_of(Path::new("/tmp/AudioHub.app/audiohub-app")),
            None
        );
    }

    #[test]
    fn target_comparison_has_a_safe_nonexistent_fallback() {
        assert!(same_target(
            Path::new("/definitely/missing").to_str().unwrap(),
            Path::new("/definitely/missing")
        ));
        assert!(!same_target(
            "/definitely/missing-a",
            Path::new("/definitely/missing-b")
        ));
    }

    #[test]
    fn a_present_invalid_registration_is_stale_even_with_the_expected_target() {
        let inspection = RegistrationInspection {
            target: Some("/Applications/AudioHub.app".into()),
            current: false,
        };
        assert_eq!(registration_kind(Some(&inspection)), "stale");
        assert_eq!(registration_kind(None), "missing");
    }

    #[test]
    fn bootstrap_preserves_an_explicit_autostart_opt_out() {
        assert!(
            !should_register_on_bootstrap(true, "missing"),
            "an existing install marker plus no registration is the user's explicit opt-out"
        );
        assert!(should_register_on_bootstrap(false, "missing"));
        assert!(should_register_on_bootstrap(true, "stale"));
        assert!(should_register_on_bootstrap(true, "current"));
    }

    #[test]
    fn daemon_install_diagnostic_is_structured_and_preserves_raw_detail() {
        let error = DaemonError::new("install-failed", "generic UI-safe summary")
            .with_detail("first line\nsecond line: \"quoted\"");
        let diagnostic = daemon_install_diagnostic(&error);
        let value: serde_json::Value =
            serde_json::from_str(&diagnostic).expect("diagnostic must be one JSON record");

        assert_eq!(value["event"], "daemon_service_install_failed");
        assert_eq!(value["kind"], "install-failed");
        assert_eq!(value["message"], "generic UI-safe summary");
        assert_eq!(value["detail"], "first line\nsecond line: \"quoted\"");
        assert_eq!(
            value.as_object().expect("diagnostic object").len(),
            4,
            "do not add the authorized command, environment, or credentials to this record"
        );
        assert!(
            !diagnostic.contains('\n'),
            "one failure must stay on one log line"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn repair_restarts_even_when_the_running_image_path_is_still_current() {
        assert_eq!(
            mac_activation_plan(Some(MacEndpointState::CurrentReady), false),
            MacActivationPlan::Keep
        );
        assert_eq!(
            mac_activation_plan(Some(MacEndpointState::CurrentReady), true),
            MacActivationPlan::ReplaceVerified(MacEndpointState::CurrentReady),
            "runtime_changed=true must replace a pre-repair process at the same content path"
        );
        assert_eq!(
            mac_activation_plan(Some(MacEndpointState::ManagedOldReady), false),
            MacActivationPlan::ReplaceVerified(MacEndpointState::ManagedOldReady)
        );
        assert_eq!(
            mac_activation_plan(Some(MacEndpointState::Foreign), true),
            MacActivationPlan::Conflict
        );
        assert_eq!(
            mac_activation_plan(Some(MacEndpointState::CurrentUnreachable), true),
            MacActivationPlan::ReplaceVerified(MacEndpointState::CurrentUnreachable)
        );
        assert_eq!(
            mac_activation_plan(Some(MacEndpointState::ManagedOldUnreachable), false),
            MacActivationPlan::ReplaceVerified(MacEndpointState::ManagedOldUnreachable)
        );
        assert_eq!(
            mac_activation_plan(Some(MacEndpointState::Gone), true),
            MacActivationPlan::Spawn
        );
        assert_eq!(mac_activation_plan(None, true), MacActivationPlan::Spawn);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn endpoint_image_classes_keep_current_old_and_foreign_distinct() {
        let current_hash = "1".repeat(64);
        let old_hash = "2".repeat(64);
        let current = Path::new(MAC_SERVICE_BASE)
            .join("versions")
            .join(current_hash)
            .join("audiohubd");
        let old = Path::new(MAC_SERVICE_BASE)
            .join("versions")
            .join(old_hash)
            .join("audiohubd");
        assert_eq!(
            classify_mac_image(Some(&current), Some(&current), true),
            MacImageClass::Current
        );
        assert_eq!(
            classify_mac_image(Some(&current), Some(&old), true),
            MacImageClass::ManagedOld
        );
        assert_eq!(
            classify_mac_image(Some(&current), Some(Path::new("/tmp/audiohubd")), false),
            MacImageClass::Foreign
        );
        assert_eq!(
            classify_mac_image(Some(&current), None, false),
            MacImageClass::Foreign
        );
        assert_eq!(
            mac_managed_version_hash(&old),
            old.parent()
                .and_then(Path::file_name)
                .and_then(|v| v.to_str())
        );
        assert!(mac_managed_version_hash(Path::new(
            "/Library/Application Support/AudioHub/service/current/audiohubd"
        ))
        .is_none());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn proc_pidpath_binds_the_current_pid_to_its_real_image() {
        let actual = mac_process_image(std::process::id()).expect("proc_pidpath current process");
        let expected = std::env::current_exe().expect("current executable");
        assert!(
            same_path(&actual, &expected),
            "actual={actual:?} expected={expected:?}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn owner_wait_identity_does_not_follow_pid_reuse() {
        let started = MacProcessStart {
            seconds: 123,
            micros: 456,
        };
        let owner = MacManagedOwner {
            endpoint: Some(IpcEndpointJson {
                ipc_version: 7,
                port: 47_810,
                token: "test".into(),
                pid: 42,
            }),
            pid: 42,
            image: PathBuf::from("/managed/old/audiohubd"),
            started,
        };
        assert!(mac_owner_matches_with(
            &owner,
            |_| Some(PathBuf::from("/managed/old/audiohubd")),
            |_| Some(started)
        ));
        assert!(!mac_owner_matches_with(
            &owner,
            |_| Some(PathBuf::from("/other/process")),
            |_| Some(started)
        ));
        assert!(!mac_owner_matches_with(
            &owner,
            |_| Some(PathBuf::from("/managed/old/audiohubd")),
            |_| Some(MacProcessStart {
                seconds: 124,
                micros: 456,
            })
        ));
        assert!(!mac_owner_matches_with(&owner, |_| None, |_| Some(started)));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn process_enumeration_failure_is_not_treated_as_an_empty_list() {
        let error = mac_all_pids_with(|_, _| -1)
            .expect_err("a failed process query must block lifecycle mutation");
        assert!(!error.to_string().is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn process_enumeration_filters_invalid_pid_slots() {
        let calls = std::cell::Cell::new(0_u32);
        let pids = mac_all_pids_with(|buffer, _| {
            calls.set(calls.get() + 1);
            if buffer.is_null() {
                return 3;
            }
            // SAFETY: the production helper allocated considerably more than
            // these three i32 slots from the query result above.
            unsafe {
                let slots = std::slice::from_raw_parts_mut(buffer.cast::<i32>(), 3);
                slots.copy_from_slice(&[11, 0, 22]);
            }
            3
        })
        .expect("a bounded process snapshot");
        assert_eq!(pids, vec![11, 22]);
        assert_eq!(calls.get(), 2);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn lifecycle_lock_is_nonblocking_across_file_descriptors() {
        let directory = std::env::temp_dir().join(format!(
            "audiohub-service-lock-test-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("unnamed")
        ));
        std::fs::create_dir_all(&directory).expect("create lock test directory");
        let path = directory.join("lifecycle.lock");
        let first = open_mac_lifecycle_lock(&path, false).expect("first lock");
        let error = open_mac_lifecycle_lock(&path, true)
            .expect_err("a second process-style descriptor must not wait");
        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
        drop(first);
        let second = open_mac_lifecycle_lock(&path, true).expect("lock released");
        drop(second);
        std::fs::remove_file(&path).expect("remove lock file");
        std::fs::remove_dir(&directory).expect("remove lock test directory");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn authorized_command_hashes_a_root_private_copy_before_parsing_it() {
        let installer =
            Path::new("/Applications/AudioHub.app/Contents/Resources/installer/install-daemon.sh");
        let command = mac_daemon_installer_command(installer).expect("compiled installer digest");
        assert!(command.contains("mktemp -d /private/tmp/audiohub-daemon-installer.XXXXXX"));
        assert!(command.contains("-m 0700"));
        assert!(command.contains("shasum -a 256 \"$d/install-daemon.sh\""));
        assert!(command.contains(&format!("[ \"$got\" = '{MAC_DAEMON_INSTALLER_SHA256}' ]")));
        assert!(command.contains("/bin/sh \"$d/install-daemon.sh\""));
        assert!(
            !command.contains(&format!("/bin/sh {}", shell_quote(installer))),
            "the elevated shell must never parse the App resource directly"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_app_rejects_a_matching_mac_path_with_a_broken_launch_contract() {
        let expected = Path::new("/Applications/AudioHub.app");
        let valid = format!(
            r#"<plist version="1.0"><dict>
<key>Label</key><string>{MAC_LABEL}</string>
<key>ProgramArguments</key><array><string>/usr/bin/open</string><string>-g</string><string>/Applications/AudioHub.app</string><string>--args</string><string>--background</string></array>
<key>RunAtLoad</key><true/>
<key>ProcessType</key><string>Interactive</string>
</dict></plist>"#
        );
        let current = inspect_mac_registration(&valid, Some(expected));
        assert!(current.current, "{current:?}");
        assert_eq!(registration_kind(Some(&current)), "current");

        let broken = valid.replace("<true/>", "<false/>");
        let stale = inspect_mac_registration(&broken, Some(expected));
        assert_eq!(stale.target.as_deref(), Some("/Applications/AudioHub.app"));
        assert!(
            !stale.current,
            "RunAtLoad=false 仍被误报为 current：{stale:?}"
        );
        assert_eq!(registration_kind(Some(&stale)), "stale");
    }
}
