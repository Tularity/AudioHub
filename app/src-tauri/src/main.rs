#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::io::Write;
use std::io::{Read, Seek, SeekFrom};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
#[cfg(target_os = "macos")]
use tauri::menu::MenuItemKind;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
// macOS builds its status item on `tray-icon` + `muda` directly (see mac_tray);
// only the other platforms go through Tauri's tray wrapper.
#[cfg(not(target_os = "macos"))]
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager, RunEvent, WindowEvent};

mod driver_install;
mod icon;
#[cfg(target_os = "macos")]
mod mac_chrome;
#[cfg(target_os = "macos")]
mod mac_tray;
mod service;
mod webui;
#[cfg(target_os = "windows")]
mod win_chrome;

/// Must match audiohub_ipc::IPC_VERSION (contract: core/audiohub-ipc/src/lib.rs).
///
/// 这个 crate 不在根 workspace 的 members 里，所以 `cargo test --workspace` **不编译
/// 它** —— 该常量落后于 audiohub-ipc 时没有任何本地信号，只有装机之后 UI 弹
/// 「服务版本不兼容」才暴露（2026-08-01 实测：daemon v2 起来了、音频正常，两端
/// 界面同时被这个模态挡死）。守卫因此写在 audiohub-ipc 里，见那里的
/// `the_three_ipc_version_declarations_agree`。
const IPC_VERSION: u32 = 7;

/// How long a freshly spawned daemon gets to publish a connectable ipc.json.
const READY_TIMEOUT: Duration = Duration::from_secs(8);

const MAIN_WINDOW: &str = "main";

#[cfg(target_os = "macos")]
const SETTINGS_MENU_ID: &str = "open_settings";
#[cfg(target_os = "macos")]
const SETTINGS_MENU_EVENT: &str = "audiohub://navigate-settings";
#[cfg(target_os = "macos")]
const SETTINGS_MENU_ACCELERATOR: &str = "CmdOrCtrl+Comma";

/// Tray id, so `set_tray_status` can find the icon again to re-skin it.
const TRAY_ID: &str = "main";

fn warn(msg: &str) {
    eprintln!("[audiohub] {msg}");
    let directory = config_dir();
    if std::fs::create_dir_all(&directory).is_err() {
        return;
    }
    let path = directory.join("app.log");
    if std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) > 1 << 20 {
        let _ = std::fs::rename(&path, path.with_extension("log.1"));
    }
    if let Ok(mut log) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(log, "[audiohub] {msg}");
    }
}

#[cfg(target_os = "windows")]
fn command_line_path(option: &str) -> Option<PathBuf> {
    let mut arguments = std::env::args_os().skip(1);
    while let Some(argument) = arguments.next() {
        if argument == option {
            return arguments.next().map(PathBuf::from);
        }
    }
    None
}

/// Finish the asynchronous NSIS `RunAsUser` handshake.
///
/// The elevated installer creates this exact file first and grants the
/// interactive user write access to the file only. Refusing to create or follow
/// another object keeps a caller from turning the one-shot App mode into a
/// general arbitrary-file writer.
#[cfg(target_os = "windows")]
fn write_installer_result(path: Option<&std::path::Path>, success: bool) {
    let Some(path) = path else {
        warn("installer bootstrap has no result path");
        return;
    };
    let result = (|| -> std::io::Result<()> {
        let metadata = std::fs::symlink_metadata(path)?;
        if !metadata.file_type().is_file() {
            return Err(std::io::Error::other(
                "installer result path is not a regular pre-created file",
            ));
        }
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(path)?;
        output.write_all(if success { b"ok\r\n" } else { b"failed\r\n" })?;
        output.sync_all()
    })();
    if let Err(error) = result {
        warn(&format!("cannot write installer result: {error}"));
    }
}

/// Mirror of audiohub_ipc::IpcEndpoint (`<config_dir>/ipc.json`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct IpcEndpointJson {
    ipc_version: u32,
    port: u16,
    token: String,
    pid: u32,
}

/// Failure taxonomy for the UI: each kind maps to different actionable copy in
/// ui/app.js. Adding a kind here means adding a branch there.
#[derive(Debug, Serialize)]
struct DaemonError {
    kind: &'static str,
    message: String,
    detail: Option<String>,
}

impl DaemonError {
    fn new(kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            detail: None,
        }
    }

    fn with_detail(mut self, detail: impl Into<String>) -> Self {
        let d = detail.into();
        self.detail = (!d.trim().is_empty()).then_some(d);
        self
    }
}

// Replicates audiohub_net::identity::LocalIdentity::config_dir so the shell
// does not have to pull in the whole net crate.
fn platform_config_root() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support"))
    }
    #[cfg(windows)]
    {
        std::env::var_os("APPDATA").map(PathBuf::from)
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    }
}

fn config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("AUDIOHUB_CONFIG_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    platform_config_root()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("AudioHub")
}

/// Whatever ipc.json currently says, including a version we cannot speak.
fn read_endpoint_raw() -> Option<IpcEndpointJson> {
    let bytes = std::fs::read(config_dir().join("ipc.json")).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn read_endpoint() -> Option<IpcEndpointJson> {
    read_endpoint_raw().filter(|ep| ep.ipc_version == IPC_VERSION)
}

fn port_alive(port: u16) -> bool {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok()
}

fn endpoint_alive(ep: &IpcEndpointJson) -> bool {
    port_alive(ep.port)
}

/// True when `bin` is the standalone daemon rather than the CLI that carries it
/// as a subcommand.
/// The CLI specifically (`audiohub`), for the `ctl ...` subcommands the daemon
/// binary does not have. Same search order as `daemon_binary`, one name.
fn cli_binary() -> Option<PathBuf> {
    let name = if cfg!(windows) {
        "audiohub.exe"
    } else {
        "audiohub"
    };
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(PathBuf::from));

    // The packaged macOS CLI is part of the sealed App and is only an IPC tool;
    // release builds must never substitute a caller-provided AUDIOHUB_BIN for
    // registration or shutdown operations.
    #[cfg(target_os = "macos")]
    if let Some(abs) = exe_dir
        .as_ref()
        .and_then(|dir| std::fs::canonicalize(dir.join(name)).ok())
        .filter(|path| path.is_file() && path.is_absolute())
    {
        return Some(abs);
    }
    #[cfg(all(target_os = "macos", not(debug_assertions)))]
    return None;

    #[cfg(not(all(target_os = "macos", not(debug_assertions))))]
    {
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Some(dir) = &exe_dir {
            candidates.push(dir.join(name));
        }
        if let Some(p) = std::env::var_os("AUDIOHUB_BIN") {
            if !p.is_empty() {
                candidates.push(PathBuf::from(p));
            }
        }
        #[cfg(debug_assertions)]
        if let Some(dir) = &exe_dir {
            candidates.push(dir.join("../../../../target/release").join(name));
        }
        candidates.into_iter().find_map(|p| {
            let abs = std::fs::canonicalize(&p).ok()?;
            (abs.is_file() && abs.is_absolute()).then_some(abs)
        })
    }
}

fn is_daemon_binary(bin: &std::path::Path) -> bool {
    bin.file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.eq_ignore_ascii_case("audiohubd"))
        .unwrap_or(false)
}

/// Keep the daemon out of the user's face on Windows.
///
/// `audiohubd` is a CONSOLE-subsystem binary and this app is a GUI one, so
/// Windows hands the child a brand-new console window unless told otherwise —
/// measured on the peer: a cmd window sat on the desktop for as long as the
/// daemon ran, which is what made a correctly-working install look like someone
/// had shipped a bare script. CREATE_NO_WINDOW suppresses it while keeping the
/// process a normal child (the app still reaps it, and it still outlives the
/// window on purpose — see the tray's 「退出界面（音频服务继续运行）」).
fn spawn_without_console(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = cmd;
}

/// Run one of AudioHub's small bundled helper commands with a real wall-clock
/// bound. `Command::output()` has no timeout: if the daemon accepts a socket and
/// then wedges before replying, an App worker otherwise waits forever and every
/// UI retry adds another stuck process.
enum ChildPipeCapture {
    Stdout(std::io::Result<Vec<u8>>),
    Stderr(std::io::Result<Vec<u8>>),
}

fn capture_child_pipe_async<R>(
    mut reader: R,
    capture: fn(std::io::Result<Vec<u8>>) -> ChildPipeCapture,
    sender: std::sync::mpsc::Sender<ChildPipeCapture>,
) -> std::io::Result<()>
where
    R: Read + Send + 'static,
{
    std::thread::Builder::new()
        .name("audiohub-helper-output".to_string())
        .spawn(move || {
            let mut bytes = Vec::new();
            let result = reader.read_to_end(&mut bytes).map(|_| bytes);
            let _ = sender.send(capture(result));
        })
        .map(|_| ())
}

fn command_output_with_timeout(
    command: &mut Command,
    timeout: Duration,
) -> std::io::Result<std::process::Output> {
    let mut child = command.spawn()?;
    let pid = child.id();
    let (send, receive) = std::sync::mpsc::channel();
    let mut pending_pipes = 0_u8;
    if let Some(stdout) = child.stdout.take() {
        pending_pipes += 1;
        if let Err(error) = capture_child_pipe_async(stdout, ChildPipeCapture::Stdout, send.clone())
        {
            let _ = terminate_spawned_child_async(child, "helper with unreadable stdout");
            return Err(error);
        }
    }
    if let Some(stderr) = child.stderr.take() {
        pending_pipes += 1;
        if let Err(error) = capture_child_pipe_async(stderr, ChildPipeCapture::Stderr, send.clone())
        {
            let _ = terminate_spawned_child_async(child, "helper with unreadable stderr");
            return Err(error);
        }
    }
    drop(send);

    let deadline = Instant::now() + timeout;
    let mut status = None;
    let mut stdout = None;
    let mut stderr = None;
    loop {
        while let Ok(capture) = receive.try_recv() {
            pending_pipes = pending_pipes.saturating_sub(1);
            match capture {
                ChildPipeCapture::Stdout(result) => stdout = Some(result),
                ChildPipeCapture::Stderr(result) => stderr = Some(result),
            }
        }
        if status.is_none() {
            match child.try_wait() {
                Ok(result) => status = result,
                Err(error) => {
                    let _ = terminate_spawned_child_async(child, "uninspectable helper");
                    return Err(error);
                }
            }
        }
        if let Some(status) = status {
            if pending_pipes == 0 {
                return Ok(std::process::Output {
                    status,
                    stdout: stdout.unwrap_or_else(|| Ok(Vec::new()))?,
                    stderr: stderr.unwrap_or_else(|| Ok(Vec::new()))?,
                });
            }
        }
        if Instant::now() >= deadline {
            // `wait()` and pipe reads are intentionally NOT on this thread. A
            // child stuck in an uninterruptible wait may not reap promptly, and
            // an already-exited helper may leave a pipe inherited by a wedged
            // descendant. Neither is allowed to extend the advertised bound.
            let kill_error = if status.is_none() {
                let error = child.kill().err();
                reap_child_async(child, "timed-out helper");
                error
            } else {
                // try_wait already reaped the direct child; detached reader
                // threads own any inherited pipe descriptors still open.
                drop(child);
                None
            };
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                match kill_error {
                    Some(error) => format!(
                        "command pid {pid} did not finish within {} ms; kill request failed: {error}",
                        timeout.as_millis()
                    ),
                    None => format!(
                        "command pid {pid} did not finish or close its output within {} ms",
                        timeout.as_millis()
                    ),
                },
            ));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Reap a child without ever making the caller's deadline depend on `wait()`.
///
/// For a healthy daemon this thread normally parks for the daemon's whole life.
/// For a timed-out helper/daemon the caller sends the kill first and this thread
/// owns only the potentially unbounded kernel reap. If thread creation itself
/// fails, dropping `Child` is still safer than blocking the UI indefinitely; the
/// OS will reclaim it when this App exits.
fn reap_child_async(mut child: std::process::Child, label: &'static str) {
    let pid = child.id();
    if let Err(error) = std::thread::Builder::new()
        .name("audiohub-child-reap".to_string())
        .spawn(move || {
            let _ = child.wait();
        })
    {
        warn(&format!(
            "could not start {label} reaper for pid {pid}: {error}"
        ));
    }
}

/// Terminate only the exact child handle returned by our own `spawn`, then hand
/// its possibly slow reap away. `Child::kill` is a non-waiting signal operation;
/// no executable discovered from ipc.json reaches this function.
fn terminate_spawned_child_async(
    mut child: std::process::Child,
    label: &'static str,
) -> Option<std::io::Error> {
    let error = child.kill().err();
    reap_child_async(child, label);
    error
}

/// A listening TCP socket is only transport readiness. The first WebSocket
/// frame is token authentication, so prove that exchange before calling a
/// daemon healthy. Re-check ipc.json on both sides of the probe so a replacement
/// cannot accidentally authenticate while we are classifying the captured PID.
#[cfg(target_os = "macos")]
fn authenticated_endpoint_alive(ep: &IpcEndpointJson) -> bool {
    if read_endpoint().as_ref() != Some(ep) || !endpoint_alive(ep) {
        return false;
    }
    let Some(cli) = cli_binary() else {
        return false;
    };
    let mut command = Command::new(cli);
    command
        .args(["ctl", "status", "--json"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    spawn_without_console(&mut command);
    command_output_with_timeout(&mut command, Duration::from_secs(2))
        .is_ok_and(|output| output.status.success())
        && read_endpoint().as_ref() == Some(ep)
}

fn daemon_binary() -> Option<PathBuf> {
    // A release macOS App never executes the source payload sealed inside its
    // own bundle. That file exists only so the explicit authorized installer
    // can copy and machine-locally sign it outside the App. This also makes a
    // missing, stale, or tampered service visible to the UI instead of silently
    // bypassing setup with the bundled source.
    #[cfg(target_os = "macos")]
    if let Some(installed) = service::installed_daemon_binary() {
        return Some(installed);
    }
    #[cfg(all(target_os = "macos", not(debug_assertions)))]
    return None;

    #[cfg(not(all(target_os = "macos", not(debug_assertions))))]
    {
        // `audiohubd` FIRST. Both binaries run the same daemon (`audiohub daemon`
        // is the CLI subcommand that calls into it), but the process a user finds
        // in Activity Monitor / Task Manager should be named for what it is. A peer
        // that only ever showed `audiohub` reads as "they shipped me a CLI", which
        // is exactly the impression this app exists to correct. The CLI stays as
        // the fallback for developer and compatibility layouts.
        let names: [&str; 2] = if cfg!(windows) {
            ["audiohubd.exe", "audiohub.exe"]
        } else {
            ["audiohubd", "audiohub"]
        };
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(PathBuf::from));

        let mut candidates: Vec<PathBuf> = Vec::new();
        // Windows runs the packaged sidecar directly. A debug macOS build may also
        // use it as a development fallback, but release macOS returned above.
        if let Some(dir) = &exe_dir {
            for n in names {
                candidates.push(dir.join(n));
            }
        }
        if let Some(p) = std::env::var_os("AUDIOHUB_BIN") {
            if !p.is_empty() {
                candidates.push(PathBuf::from(p));
            }
        }
        // Dev layout only: `cargo run` in app/src-tauri puts the shell at
        // app/src-tauri/target/<profile>/, the daemon at <repo>/target/release/.
        // Resolved from the executable, never from the cwd, and never in release
        // builds: a cwd-relative candidate lets whoever controls the launch
        // directory decide which binary we spawn.
        #[cfg(debug_assertions)]
        if let Some(dir) = &exe_dir {
            for n in names {
                candidates.push(dir.join("../../../../target/release").join(n));
            }
        }
        candidates.into_iter().find_map(|p| {
            let abs = std::fs::canonicalize(&p).ok()?;
            (abs.is_file() && abs.is_absolute()).then_some(abs)
        })
    }
}

#[tauri::command]
fn get_ipc_endpoint() -> Option<IpcEndpointJson> {
    let endpoint = read_endpoint()?;
    #[cfg(target_os = "macos")]
    {
        (service::mac_endpoint_state(&endpoint) == service::MacEndpointState::CurrentReady)
            .then_some(endpoint)
    }
    #[cfg(not(target_os = "macos"))]
    {
        Some(endpoint)
    }
}

fn daemon_log_path() -> PathBuf {
    config_dir().join("daemon.log")
}

/// Sink for the spawned daemon's stderr. NEVER a pipe: the daemon outlives this
/// process on purpose (tray item 「退出界面（音频服务继续运行）」), and once the
/// read end dies with us every `eprintln!` in the daemon fails with EPIPE —
/// which `std::io::_eprint` turns into a panic, so its threads die one at a
/// time (observed: the signal thread panicked before begin_shutdown, so the
/// daemon ignored SIGTERM and left a stale ipc.json). A file's write end
/// outlives us, and doubles as the log to look at when things break.
/// Returns the handle plus the offset from which this run's output starts.
fn open_daemon_log() -> std::io::Result<(std::fs::File, u64)> {
    std::fs::create_dir_all(config_dir())?;
    let path = daemon_log_path();
    // One rotation, only at spawn time (so no live daemon is writing to it):
    // this file is appended to for the whole life of every daemon this app ever
    // starts, and nothing else would ever trim it.
    if std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) > 1 << 20 {
        let _ = std::fs::rename(&path, path.with_extension("log.1"));
    }
    let f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    let at = f.metadata().map(|m| m.len()).unwrap_or(0);
    Ok((f, at))
}

/// Tail of what the daemon logged since `from`, for the failure detail the UI
/// shows. Best effort: diagnostics must never fail the caller.
fn daemon_log_tail(from: u64, max: usize) -> String {
    let Ok(mut f) = std::fs::File::open(daemon_log_path()) else {
        return String::new();
    };
    if f.seek(SeekFrom::Start(from)).is_err() {
        return String::new();
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return String::new();
    }
    String::from_utf8_lossy(&buf[buf.len().saturating_sub(max)..]).into_owned()
}

/// Serialises concurrent auto-start attempts: the window may retry while an
/// earlier attempt is still polling, and two spawns would fight over the
/// control port.
fn spawn_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Blocking: connects sockets and sleeps up to ~8s. Never call on the main
/// thread — `ensure_daemon` hands it to the blocking pool.
fn ensure_daemon_blocking() -> Result<IpcEndpointJson, DaemonError> {
    let _serialised = spawn_lock().lock().unwrap_or_else(|e| e.into_inner());

    // Idempotence: a healthy daemon is never restarted. On macOS a listening
    // port alone is insufficient: ipc.json already carries the PID, so bind it
    // to the exact root-owned, signed runtime image selected by this App. The
    // explicit Start/install paths can replace a verified older version through
    // authenticated IPC; this low-level spawn primitive never stops anything.
    #[cfg(target_os = "macos")]
    if let Some(ep) = read_endpoint() {
        match service::mac_endpoint_state(&ep) {
            service::MacEndpointState::Gone => {}
            service::MacEndpointState::CurrentReady => return Ok(ep),
            service::MacEndpointState::ManagedOldReady => {
                return Err(DaemonError::new(
                    "service-conflict",
                    "An older AudioHub service is still running",
                )
                .with_detail("Use the App's Start action to replace the verified older service"));
            }
            service::MacEndpointState::CurrentUnreachable => {
                return Err(DaemonError::new(
                    "service-conflict",
                    "The installed AudioHub service is not responding",
                )
                .with_detail("AudioHub refused to start a duplicate process"));
            }
            service::MacEndpointState::ManagedOldUnreachable => {
                return Err(DaemonError::new(
                    "service-conflict",
                    "An older AudioHub service is not responding",
                )
                .with_detail("Authenticated shutdown is unavailable; no process was killed"));
            }
            service::MacEndpointState::Foreign => {
                return Err(DaemonError::new(
                    "service-conflict",
                    "Another process owns the AudioHub service endpoint",
                )
                .with_detail("AudioHub refused to stop or replace an unverified process"));
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    if let Some(ep) = read_endpoint() {
        if endpoint_alive(&ep) {
            return Ok(ep);
        }
    }
    // A *live* daemon speaking another protocol version owns the ports; a
    // second one could not bind them, and restarting it is not our call.
    if let Some(raw) = read_endpoint_raw() {
        if raw.ipc_version != IPC_VERSION && port_alive(raw.port) {
            return Err(DaemonError::new(
                "version",
                format!(
                    "正在运行的 daemon 使用 IPC 协议 v{}，本界面需要 v{IPC_VERSION}",
                    raw.ipc_version
                ),
            ));
        }
    }

    let bin = daemon_binary().ok_or_else(|| {
        #[cfg(target_os = "macos")]
        let detail = "未找到通过校验的已安装服务；请在 App 中执行安装或修复".to_string();
        #[cfg(not(target_os = "macos"))]
        let detail = format!(
            "已查找：本程序所在目录、环境变量 AUDIOHUB_BIN（当前 {}）",
            std::env::var("AUDIOHUB_BIN").unwrap_or_else(|_| "未设置".into())
        );
        DaemonError::new("no-binary", "未找到 audiohub 服务程序").with_detail(detail)
    })?;

    let (log_sink, log_from, log_note) = match open_daemon_log() {
        Ok((f, at)) => (
            Stdio::from(f),
            at,
            format!("日志：{}", daemon_log_path().display()),
        ),
        Err(e) => (
            Stdio::null(),
            0,
            format!("无法写入日志 {}：{e}", daemon_log_path().display()),
        ),
    };

    let mut cmd = Command::new(&bin);
    // `audiohubd` IS the daemon; `audiohub` needs the subcommand. Passing
    // "daemon" to audiohubd makes it exit on an unknown argument, which would
    // present as "the app starts and nothing ever comes up".
    if !is_daemon_binary(&bin) {
        cmd.arg("daemon");
    }
    spawn_without_console(&mut cmd);
    let mut child = cmd
        .stdin(Stdio::null())
        // The daemon's one stdout JSON line only appears with --json, which is
        // not passed here; stderr is its log and goes to a file, never a pipe.
        .stdout(Stdio::null())
        .stderr(log_sink)
        .spawn()
        .map_err(|e| {
            DaemonError::new("spawn-failed", format!("无法启动 {}", bin.display()))
                .with_detail(e.to_string())
        })?;
    let child_pid = child.id();

    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let logged = daemon_log_tail(log_from, 4096);
                let detail = if logged.trim().is_empty() {
                    format!("daemon pid {child_pid} exited with {status}; {log_note}")
                } else {
                    format!("daemon pid {child_pid} exited with {status}\n{logged}\n{log_note}")
                };
                return Err(
                    DaemonError::new("spawn-failed", "AudioHub 服务进程在就绪前退出")
                        .with_detail(detail),
                );
            }
            Ok(None) => {}
            Err(error) => {
                let _ = terminate_spawned_child_async(child, "uninspectable daemon");
                return Err(DaemonError::new(
                    "spawn-failed",
                    "AudioHub 无法确认刚启动的服务是否仍在运行",
                )
                .with_detail(error.to_string()));
            }
        }

        if let Some(ep) = read_endpoint() {
            #[cfg(target_os = "macos")]
            if service::mac_endpoint_state(&ep) == service::MacEndpointState::CurrentReady {
                if ep.pid != child_pid {
                    let _ = terminate_spawned_child_async(child, "duplicate daemon");
                    return Err(DaemonError::new(
                        "service-conflict",
                        "另一个 AudioHub 服务在本次启动期间取得了端点",
                    )
                    .with_detail(format!(
                        "spawned pid {child_pid}, authenticated endpoint pid {}",
                        ep.pid
                    )));
                }
                // Reap the child so it is not a zombie for as long as this
                // process lives. The thread normally parks for the daemon's
                // whole life; when the App exits it is reparented to launchd.
                reap_child_async(child, "daemon");
                return Ok(ep);
            }
            #[cfg(not(target_os = "macos"))]
            if endpoint_alive(&ep) {
                if ep.pid != child_pid {
                    let _ = terminate_spawned_child_async(child, "duplicate daemon");
                    return Err(DaemonError::new(
                        "service-conflict",
                        "另一个 AudioHub 服务在本次启动期间取得了端点",
                    ));
                }
                reap_child_async(child, "daemon");
                return Ok(ep);
            }
        }
        if Instant::now() >= deadline {
            let logged = daemon_log_tail(log_from, 4096);
            let busy = logged.contains("in use")
                || logged.contains("Address already")
                || logged.contains("EADDRINUSE")
                || logged.contains("占用");
            let detail = if logged.trim().is_empty() {
                log_note
            } else {
                format!("{logged}\n{log_note}")
            };
            let kill_error = terminate_spawned_child_async(child, "unready daemon");
            let detail = match kill_error {
                Some(error) => {
                    format!("{detail}\nfailed to terminate spawned pid {child_pid}: {error}")
                }
                None => format!("{detail}\nterminated unready spawned pid {child_pid}"),
            };
            return Err(if busy {
                DaemonError::new("port-busy", "AudioHub 服务所需的端口已被占用").with_detail(detail)
            } else {
                DaemonError::new(
                    "timeout",
                    format!("服务已启动，但 {} 秒内未就绪", READY_TIMEOUT.as_secs()),
                )
                .with_detail(detail)
            });
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[tauri::command]
async fn ensure_daemon() -> Result<IpcEndpointJson, DaemonError> {
    #[cfg(target_os = "macos")]
    let task = tauri::async_runtime::spawn_blocking(service::start_mac_daemon_blocking);
    #[cfg(not(target_os = "macos"))]
    let task = tauri::async_runtime::spawn_blocking(ensure_daemon_blocking);
    task.await
        .map_err(|e| DaemonError::new("internal", format!("ensure_daemon 任务失败：{e}")))?
}

// ---- window / tray ----

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NativeLocale {
    ZhCn,
    EnUs,
}

impl NativeLocale {
    fn parse(value: Option<&str>) -> Self {
        match value {
            Some("en-US") => Self::EnUs,
            _ => Self::ZhCn,
        }
    }

    fn show(self) -> &'static str {
        match self {
            Self::ZhCn => "显示主窗口",
            Self::EnUs => "Open AudioHub",
        }
    }

    fn connecting(self) -> &'static str {
        match self {
            Self::ZhCn => "状态：连接中…",
            Self::EnUs => "Status: Connecting…",
        }
    }

    fn settings(self) -> &'static str {
        match self {
            Self::ZhCn => "设置…",
            Self::EnUs => "Settings…",
        }
    }

    /// Caption above the tray volume slider. The slider drives the *peer's*
    /// output level, not this machine's, so the copy says so — the row appears
    /// only while a peer session with an adjustable volume exists.
    fn volume(self) -> &'static str {
        match self {
            Self::ZhCn => "对端音量",
            Self::EnUs => "Peer volume",
        }
    }

    fn quit_ui(self) -> &'static str {
        match self {
            Self::ZhCn => "退出界面（音频服务继续运行）",
            Self::EnUs => "Quit App (Audio Stays On)",
        }
    }

    fn quit_all(self) -> &'static str {
        match self {
            Self::ZhCn => "停止音频服务并退出",
            Self::EnUs => "Stop Audio & Quit",
        }
    }

    fn status(self, online: bool, port: Option<u16>) -> String {
        match (self, online, port) {
            (Self::ZhCn, true, Some(p)) => format!("状态：在线 · 端口 {p}"),
            (Self::ZhCn, true, None) => "状态：在线".to_string(),
            (Self::ZhCn, false, _) => "状态：离线".to_string(),
            (Self::EnUs, true, Some(p)) => format!("Status: Online · Port {p}"),
            (Self::EnUs, true, None) => "Status: Online".to_string(),
            (Self::EnUs, false, _) => "Status: Offline".to_string(),
        }
    }
}

#[derive(Deserialize)]
struct NativeSettingsJson {
    native_locale: Option<String>,
}

fn stored_native_locale() -> NativeLocale {
    let locale = std::fs::read(config_dir().join("settings.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<NativeSettingsJson>(&bytes).ok())
        .and_then(|s| s.native_locale);
    NativeLocale::parse(locale.as_deref())
}

/// Every tray string, kept so a frontend locale change updates the complete
/// native menu rather than only translating its status line.
///
/// Not macOS: there the status item is raw `muda`, whose items are neither
/// `Send` nor `Sync` and so cannot be `manage`d at all. `mac_tray` keeps the
/// equivalent handles in its own main-thread-bound state. Two concrete types
/// rather than one trait: they have nothing in common but their field names.
#[cfg(not(target_os = "macos"))]
struct TrayItems {
    show: MenuItem<tauri::Wry>,
    status: MenuItem<tauri::Wry>,
    quit_ui: MenuItem<tauri::Wry>,
    quit_all: MenuItem<tauri::Wry>,
}

/// Handles for copy that lives in macOS' application menu (the menu headed by
/// "AudioHub" beside the Apple menu), not the status item built by
/// `build_tray`. Keeping the custom item lets an in-app locale switch update
/// both native surfaces in the same `set_tray_status` round trip.
#[cfg(target_os = "macos")]
struct MacAppMenuItems {
    settings: MenuItem<tauri::Wry>,
}

fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window(MAIN_WINDOW) {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

#[cfg(target_os = "macos")]
fn open_native_settings(app: &AppHandle) {
    // The menu remains available while the only window is hidden. Restore it
    // before routing so Settings is immediately visible rather than changing
    // a hidden webview and leaving the user wondering whether the item worked.
    show_main(app);
    if let Some(window) = app.get_webview_window(MAIN_WINDOW) {
        // A fixed DOM event avoids adding the frontend Tauri package merely to
        // route one native command. JSON quoting keeps the JavaScript literal
        // correct if the event name ever gains punctuation.
        if let Ok(event) = serde_json::to_string(SETTINGS_MENU_EVENT) {
            let _ = window.eval(format!("window.dispatchEvent(new Event({event}))"));
        }
    }
}

/// Add the one application-specific item to Tauri's native macOS menu.
///
/// Starting from `Menu::default` deliberately preserves the standard Edit,
/// View and Window commands (including their AppKit-backed accelerators). The
/// Settings item belongs after About in the first/application submenu and uses
/// the platform-standard Command-comma key equivalent.
#[cfg(target_os = "macos")]
fn build_macos_app_menu(app: &AppHandle) -> tauri::Result<()> {
    let locale = stored_native_locale();
    let menu = Menu::default(app)?;
    let settings = MenuItem::with_id(
        app,
        SETTINGS_MENU_ID,
        locale.settings(),
        true,
        Some(SETTINGS_MENU_ACCELERATOR),
    )?;
    let separator = PredefinedMenuItem::separator(app)?;

    let application_menu = menu
        .items()?
        .into_iter()
        .find_map(|item| match item {
            MenuItemKind::Submenu(submenu) => Some(submenu),
            _ => None,
        })
        .ok_or_else(|| std::io::Error::other("default macOS menu has no application submenu"))?;
    application_menu.insert_items(&[&settings, &separator], 2)?;
    menu.set_as_app_menu()?;
    app.manage(MacAppMenuItems {
        settings: settings.clone(),
    });
    Ok(())
}

#[tauri::command]
fn show_main_window(app: AppHandle) {
    show_main(&app);
}

/// Drag the window, driven explicitly from a `mousedown` in the frontend
/// (`app/frontend/src/lib/drag.ts`) instead of `-webkit-app-region: drag`.
///
/// The CSS drag region was reported twice as "draggable right after launch,
/// dead for the rest of the session once you click anything else or drag any
/// text". The previously-suspected cause — a text selection poisoning the
/// region — was already guarded against in CSS (`user-select: none` on the
/// region, `no-drag` on its children) and the symptom survived that guard, so
/// the region itself is what cannot be relied on. This is the same call Tauri's
/// own `data-tauri-drag-region` makes internally; routing it through an app
/// command keeps it working without a capabilities file, since app commands
/// registered in `invoke_handler` are not subject to the plugin ACL.
#[tauri::command]
fn start_window_drag(window: tauri::Window) -> Result<(), String> {
    window.start_dragging().map_err(|e| e.to_string())
}

/// Double-click on the title area. macOS toggles zoom there, and
/// `start_dragging` on mousedown would otherwise swallow it — so the frontend
/// routes `detail >= 2` here instead. `performWindowDragWithEvent:` does not
/// consume the follow-up click when the pointer has not moved, which is why the
/// second mousedown still arrives; Tauri's own drag region splits on exactly
/// the same signal.
#[tauri::command]
fn toggle_window_zoom(window: tauri::Window) -> Result<(), String> {
    let zoomed = window.is_maximized().map_err(|e| e.to_string())?;
    if zoomed {
        window.unmaximize()
    } else {
        window.maximize()
    }
    .map_err(|e| e.to_string())
}

/// Right-click on the title strip. Windows shows the system window menu there;
/// with `decorations: false` the strip is client area, so no `WM_NCRBUTTONUP`
/// is ever generated and the app has to raise the menu itself.
///
/// A no-op on macOS, which has no equivalent gesture — the frontend suppresses
/// the browser menu on that strip either way, so right-clicking the title bar
/// does nothing there, exactly as it does in a stock AppKit window.
#[tauri::command]
fn show_window_menu(window: tauri::Window) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    win_chrome::show_system_menu(&window);
    #[cfg(not(target_os = "windows"))]
    let _ = window;
    Ok(())
}

/// Hand a URL to the user's default browser.
///
/// Every explanatory link in the UI ends up here, so it has to actually leave
/// the app. It previously did not: `lib/external.ts` reached for
/// `window.__TAURI__.opener` / `.shell`, and neither plugin is a dependency of
/// this crate — so every wiki link fell through to `window.open`, which the
/// webview refuses, and then to "copied to clipboard". A link that silently
/// becomes a clipboard write is worse than no link, and the UI now leans on
/// these links for everything it no longer explains inline.
///
/// An app command rather than `tauri-plugin-opener` for the reason
/// `start_window_drag` gives: commands registered in `invoke_handler` are not
/// subject to the plugin ACL, so the bundle keeps working without a
/// capabilities file. The scheme allow-list that the plugin would have given us
/// is therefore written out here — this hands an arbitrary string to the
/// platform's URL dispatcher, and `file://` or a custom scheme registered by
/// some other installed app is not something a documentation link may reach.
#[tauri::command]
fn open_external_url(url: String) -> Result<(), String> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(format!("refused non-http url: {url}"));
    }
    // No shell on either platform: `cmd /C start` would need the URL quoted
    // against `&`, and `rundll32 url.dll,FileProtocolHandler` takes it verbatim.
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = Command::new("/usr/bin/open");
        c.arg(&url);
        c
    };
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = Command::new("rundll32.exe");
        c.arg("url.dll,FileProtocolHandler").arg(&url);
        c
    };
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let mut cmd = {
        let mut c = Command::new("xdg-open");
        c.arg(&url);
        c
    };
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Windows caption buttons. macOS keeps its real traffic lights, so the
/// frontend only renders these where the OS puts its controls on the trailing
/// edge (`lib/platform.ts`); the commands themselves are platform-neutral.
///
/// Close means **hide**, matching `CloseRequested` below — the daemon keeps
/// running and the tray is the way back. Routing it through an app command
/// rather than `core:window:allow-close` keeps the no-capabilities-file
/// property that `start_window_drag` explains.
#[tauri::command]
fn minimize_window(window: tauri::Window) -> Result<(), String> {
    window.minimize().map_err(|e| e.to_string())
}

#[tauri::command]
fn hide_window(window: tauri::Window) -> Result<(), String> {
    window.hide().map_err(|e| e.to_string())
}

#[tauri::command]
fn is_window_maximized(window: tauri::Window) -> Result<bool, String> {
    window.is_maximized().map_err(|e| e.to_string())
}

/// One call carries everything the chrome shows about connection state: the
/// tray menu's status line, the tray glyph, and the dock tile.
///
/// They are deliberately not three commands. The frontend dedupes on a single
/// key covering all of it (`syncTray` in `state/connection.ts`), so splitting
/// them would mean three round trips per transition and three chances for the
/// three surfaces to disagree mid-flight.
///
/// `state` and `theme` are optional so that a frontend built before this change
/// still updates the status line instead of failing argument deserialisation.
///
/// `volume`/`muted` drive the macOS tray's volume row and follow the same rule.
/// `volume: None` means "no adjustable peer session right now" and takes the
/// whole row out of the menu; the frontend sends `Some` only in mode A with a
/// live session that actually has a volume to move. `muted` greys the slider out
/// without moving it — muting is not a volume of zero.
#[allow(clippy::too_many_arguments)]
#[tauri::command]
fn set_tray_status(
    app: AppHandle,
    online: bool,
    port: Option<u16>,
    state: Option<String>,
    theme: Option<String>,
    locale: Option<String>,
    volume: Option<f64>,
    muted: Option<bool>,
) {
    let locale = locale
        .as_deref()
        .map(|value| NativeLocale::parse(Some(value)))
        .unwrap_or_else(stored_native_locale);
    // Parsed up front so the one main-thread hop below carries the icon too.
    // Both parsers are pure, so hoisting them changes nothing.
    let st = state.as_deref().map(icon::IconState::parse);
    let th = theme
        .as_deref()
        .map(icon::IconTheme::parse)
        .unwrap_or(icon::IconTheme::Dark);

    #[cfg(target_os = "macos")]
    mac_tray::apply(
        &app,
        mac_tray::TrayUpdate {
            locale,
            online,
            port,
            icon: st.map(|state| (state, th)),
            volume,
            muted: muted.unwrap_or(false),
        },
    );
    #[cfg(not(target_os = "macos"))]
    {
        // No volume row outside macOS: this change is AppKit-only, and the
        // Windows tray deliberately keeps the behaviour it shipped with.
        let _ = (volume, muted);
        if let Some(items) = app.try_state::<TrayItems>() {
            let _ = items.show.set_text(locale.show());
            let _ = items.status.set_text(locale.status(online, port));
            let _ = items.quit_ui.set_text(locale.quit_ui());
            let _ = items.quit_all.set_text(locale.quit_all());
        }
    }
    #[cfg(target_os = "macos")]
    if let Some(items) = app.try_state::<MacAppMenuItems>() {
        let _ = items.settings.set_text(locale.settings());
    }

    // A report without `state` refreshes copy only — unchanged from before.
    let Some(st) = st else { return };

    // macOS' status item glyph was already updated inside `mac_tray::apply`,
    // which owns the tray handle; only the dock tile is left, at the bottom.
    #[cfg(not(target_os = "macos"))]
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let _ = tray.set_icon(Some(icon::tray_image(st, th)));
    }

    // The Windows taskbar shows the *window* icon, so the equivalent of the
    // macOS dock tile is set here rather than through AppKit. macOS ignores the
    // window icon entirely — `set_dock_icon` above is the only path that works
    // there, which is why this is not one branch for both.
    #[cfg(target_os = "windows")]
    if let Some(w) = app.get_webview_window(MAIN_WINDOW) {
        let px = icon::dock_rgba(st, th);
        let _ = w.set_icon(tauri::image::Image::new_owned(
            px,
            icon::DOCK_PX,
            icon::DOCK_PX,
        ));
    }

    icon::set_dock_icon(st, th);
}

/// Quit the window only. The daemon is deliberately left running so audio keeps
/// flowing — see the tray copy.
#[tauri::command]
fn quit_ui(app: AppHandle) {
    app.exit(0);
}

/// Best effort `daemon.shutdown` over IPC (that is exactly what `ctl shutdown`
/// sends), then quit. A failure here must not trap the user in the app.
fn shutdown_daemon_blocking() {
    // `ctl shutdown` is a CLI subcommand: audiohubd does not have it. Since
    // daemon_binary() now prefers audiohubd, pick the CLI explicitly here — the
    // two sit side by side in every layout that ships them.
    let Some(bin) = cli_binary() else {
        warn("stop-daemon: no audiohub CLI found; quitting anyway");
        return;
    };
    let mut cmd = Command::new(&bin);
    cmd.args(["ctl", "shutdown", "--json"]);
    spawn_without_console(&mut cmd);
    match cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            let deadline = Instant::now() + Duration::from_secs(6);
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    _ => {
                        warn("stop-daemon: ctl shutdown did not finish in time");
                        let _ = child.kill();
                        break;
                    }
                }
            }
        }
        Err(e) => warn(&format!("stop-daemon: spawn failed: {e}")),
    }
}

#[tauri::command]
async fn stop_daemon_and_quit(app: AppHandle) {
    let _ = tauri::async_runtime::spawn_blocking(shutdown_daemon_blocking).await;
    app.exit(0);
}

/// macOS has its own status item built on the crates underneath Tauri, because
/// the volume slider needs the real `NSMenu` — see `mac_tray`. Everything else
/// keeps `tauri::tray::TrayIconBuilder` exactly as it was.
#[cfg(target_os = "macos")]
fn build_tray(app: &AppHandle) -> tauri::Result<()> {
    mac_tray::build(app)
}

#[cfg(not(target_os = "macos"))]
fn build_tray(app: &AppHandle) -> tauri::Result<()> {
    let locale = stored_native_locale();
    let show = MenuItem::with_id(app, "show", locale.show(), true, None::<&str>)?;
    // Informational only; disabled so it cannot be "clicked".
    let status = MenuItem::with_id(app, "status", locale.connecting(), false, None::<&str>)?;
    let quit_ui_item = MenuItem::with_id(app, "quit_ui", locale.quit_ui(), true, None::<&str>)?;
    let quit_all = MenuItem::with_id(app, "quit_all", locale.quit_all(), true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &show,
            &PredefinedMenuItem::separator(app)?,
            &status,
            &PredefinedMenuItem::separator(app)?,
            &quit_ui_item,
            &quit_all,
        ],
    )?;

    app.manage(TrayItems {
        show: show.clone(),
        status: status.clone(),
        quit_ui: quit_ui_item.clone(),
        quit_all: quit_all.clone(),
    });

    // Before the first frontend report, claim nothing: `Connecting` is what the
    // store starts at too (`ConnState` initial is 'connecting').
    TrayIconBuilder::with_id(TRAY_ID)
        .icon(icon::tray_image(
            icon::IconState::Connecting,
            icon::IconTheme::Dark,
        ))
        // macOS template image: AppKit keeps only the alpha channel and recolours
        // it for the current menu bar appearance. See icon.rs on why this is the
        // right answer for the menu bar and the wrong one for the dock.
        .icon_as_template(true)
        .tooltip("AudioHub")
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "show" => show_main(app),
            "quit_ui" => app.exit(0),
            "quit_all" => {
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    let _ = tauri::async_runtime::spawn_blocking(shutdown_daemon_blocking).await;
                    app.exit(0);
                });
            }
            _ => {}
        })
        .build(app)?;
    Ok(())
}

#[cfg(test)]
mod native_locale_tests {
    use super::NativeLocale;
    #[cfg(target_os = "macos")]
    use super::{SETTINGS_MENU_ACCELERATOR, SETTINGS_MENU_EVENT, SETTINGS_MENU_ID};

    #[test]
    fn tray_copy_is_complete_in_both_published_locales() {
        let zh = NativeLocale::parse(Some("zh-CN"));
        assert_eq!(zh.show(), "显示主窗口");
        assert_eq!(zh.settings(), "设置…");
        assert_eq!(zh.volume(), "对端音量");
        assert_eq!(zh.status(true, Some(47810)), "状态：在线 · 端口 47810");
        assert!(zh.quit_ui().contains("音频服务"));
        assert!(zh.quit_all().contains("退出"));

        let en = NativeLocale::parse(Some("en-US"));
        assert_eq!(en.show(), "Open AudioHub");
        assert_eq!(en.settings(), "Settings…");
        assert_eq!(en.volume(), "Peer volume");
        assert_eq!(en.status(true, Some(47810)), "Status: Online · Port 47810");
        assert_eq!(en.status(false, None), "Status: Offline");
        assert!(en.quit_ui().is_ascii());
        assert!(en.quit_all().is_ascii());
    }

    #[test]
    fn unknown_or_missing_persisted_locale_preserves_the_chinese_default() {
        assert_eq!(NativeLocale::parse(None), NativeLocale::ZhCn);
        assert_eq!(NativeLocale::parse(Some("fr-FR")), NativeLocale::ZhCn);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn settings_uses_the_native_macos_contract() {
        assert_eq!(SETTINGS_MENU_ID, "open_settings");
        assert_eq!(SETTINGS_MENU_EVENT, "audiohub://navigate-settings");
        assert_eq!(SETTINGS_MENU_ACCELERATOR, "CmdOrCtrl+Comma");
    }
}

#[cfg(all(test, unix))]
mod command_timeout_tests {
    use super::*;

    #[test]
    fn bounded_helper_returns_completed_output() {
        let mut command = Command::new("/usr/bin/true");
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let output = command_output_with_timeout(&mut command, Duration::from_secs(1))
            .expect("a completed helper returns normally");
        assert!(output.status.success());
    }

    #[test]
    fn bounded_helper_kills_a_stuck_process() {
        let mut command = Command::new("/bin/sleep");
        command
            .arg("10")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let started = Instant::now();
        let error = command_output_with_timeout(&mut command, Duration::from_millis(100))
            .expect_err("a stuck helper must time out");
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn bounded_helper_does_not_wait_for_an_inherited_output_pipe() {
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "/bin/sleep 1 &"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let started = Instant::now();
        let error = command_output_with_timeout(&mut command, Duration::from_millis(100))
            .expect_err("a descendant-held output pipe must not outlive the deadline");
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_millis(750));
    }
}

fn main() {
    let background_launch = std::env::args_os().any(|arg| arg == "--background");
    #[cfg(target_os = "windows")]
    let installer_bootstrap = std::env::args_os().any(|arg| arg == "--installer-bootstrap");
    #[cfg(not(target_os = "windows"))]
    let installer_bootstrap = false;
    #[cfg(target_os = "windows")]
    let installer_result = command_line_path("--installer-result");
    let app = tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            get_ipc_endpoint,
            ensure_daemon,
            service::daemon_service_status,
            service::install_daemon_service,
            service::start_daemon_service,
            service::restart_daemon_service,
            driver_install::driver_installer_status,
            driver_install::install_driver,
            set_tray_status,
            show_main_window,
            start_window_drag,
            toggle_window_zoom,
            show_window_menu,
            open_external_url,
            minimize_window,
            hide_window,
            is_window_maximized,
            quit_ui,
            stop_daemon_and_quit,
            webui::get_webui_status,
            webui::set_webui_settings
        ])
        // Tauri hands every muda menu event to these global listeners without
        // consulting its own id registry (`tauri-2.11.5/src/app.rs:2588`), which
        // is what lets the macOS status item — built on raw muda in `mac_tray`,
        // so none of its items are registered — be handled right here. Windows
        // is untouched: its tray keeps `TrayIconBuilder::on_menu_event`, and the
        // arms below are `cfg`-gated so nothing is dispatched twice there.
        .on_menu_event(|app, event| {
            #[cfg(target_os = "macos")]
            match event.id().as_ref() {
                SETTINGS_MENU_ID => open_native_settings(app),
                mac_tray::SHOW_ID => show_main(app),
                mac_tray::QUIT_UI_ID => app.exit(0),
                mac_tray::QUIT_ALL_ID => {
                    let app = app.clone();
                    tauri::async_runtime::spawn(async move {
                        let _ =
                            tauri::async_runtime::spawn_blocking(shutdown_daemon_blocking).await;
                        app.exit(0);
                    });
                }
                _ => {}
            }
            #[cfg(not(target_os = "macos"))]
            let _ = (app, event);
        })
        .setup(move |app| {
            #[cfg(target_os = "macos")]
            build_macos_app_menu(app.handle())?;
            build_tray(app.handle())?;
            // 网页访问（plan §7.5）：设置里开着才会真的开监听端口，默认关闭。
            webui::init(app.handle());
            // `.window()` rather than `Manager::get_window`: the latter is
            // gated behind tauri's `unstable` feature, and the chrome helpers
            // take a `Window` because `on_window_event` hands them one.
            if let Some(_wv) = app.get_webview_window(MAIN_WINDOW) {
                if background_launch || installer_bootstrap {
                    let _ = _wv.hide();
                }
                #[cfg(target_os = "windows")]
                win_chrome::silence_webview_context_menu(&_wv);
                let _w = AsRef::<tauri::Webview>::as_ref(&_wv).window();
                #[cfg(target_os = "macos")]
                mac_chrome::apply(&_w);
                #[cfg(target_os = "windows")]
                win_chrome::install(app.handle(), &_w);
            }
            if installer_bootstrap {
                // The privileged OS installer copied only immutable program
                // files. It launches this one-shot App back in the interactive
                // user's unelevated token so per-user task/marker writes and
                // audio/TCC context stay correct. Exit after setup; the daemon
                // intentionally outlives this bootstrap process.
                let handle = app.handle().clone();
                #[cfg(target_os = "windows")]
                let result_path = installer_result.clone();
                tauri::async_runtime::spawn(async move {
                    let bootstrap_success = match tauri::async_runtime::spawn_blocking(
                        service::package_bootstrap_blocking,
                    )
                    .await
                    {
                        Ok(Ok(_)) => true,
                        Ok(Err(error)) => {
                            warn(&format!(
                                "package service bootstrap failed: {}",
                                error.message
                            ));
                            false
                        }
                        Err(error) => {
                            warn(&format!("package bootstrap task failed: {error}"));
                            false
                        }
                    };
                    #[cfg(target_os = "windows")]
                    write_installer_result(result_path.as_deref(), bootstrap_success);
                    #[cfg(not(target_os = "windows"))]
                    let _ = bootstrap_success;
                    handle.exit(0);
                });
            } else if background_launch {
                // A login/startup trigger is the one context in which starting
                // without a click is itself the user's saved request. Spawn
                // from the App so the daemon retains the interactive user's
                // audio, local-network and permission context.
                tauri::async_runtime::spawn(async move {
                    #[cfg(target_os = "macos")]
                    let task =
                        tauri::async_runtime::spawn_blocking(service::start_mac_daemon_blocking);
                    #[cfg(not(target_os = "macos"))]
                    let task = tauri::async_runtime::spawn_blocking(ensure_daemon_blocking);
                    match task.await {
                        Ok(Ok(_)) => {}
                        Ok(Err(error)) => warn(&format!(
                            "background daemon start failed: {}",
                            error.message
                        )),
                        Err(error) => warn(&format!("background daemon task failed: {error}")),
                    }
                });
            }
            Ok(())
        })
        // Closing the window hides to the menu bar; only the tray quit items
        // really exit. Never destroying the window is also what keeps the app
        // alive after the last close.
        .on_window_event(|window, event| {
            match event {
                WindowEvent::CloseRequested { api, .. } => {
                    api.prevent_close();
                    let _ = window.hide();
                }
                // No macOS arm here on purpose. `mac_chrome` no longer writes
                // any geometry — it declares the window's *style* once, and
                // AppKit derives the corner radius and the traffic-light
                // placement from it. A style is a persistent property of the
                // window rather than something a layout pass recomputes, so
                // there is nothing to re-seat after a resize or a fullscreen
                // round trip. See `mac_chrome` for the measurements.
                _ => {}
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    app.run(|_app, _event| {
        // 退出前把网页服务的监听端口交回系统。进程退出时内核也会回收，但显式停一下
        // 才能保证「退出界面」后端口立刻可被下一次启动重新绑定。
        if let RunEvent::Exit = _event {
            webui::shutdown();
        }
        // Dock click with every window hidden must bring the UI back. macOS
        // only: `RunEvent::Reopen` is the dock's own event and the variant does
        // not exist in the Windows build of tauri, so referring to it at all is
        // a compile error there. Windows has no dock — the tray icon is the way
        // back, and that path is platform-independent.
        #[cfg(target_os = "macos")]
        if let RunEvent::Reopen {
            has_visible_windows,
            ..
        } = _event
        {
            if !has_visible_windows {
                show_main(_app);
            }
        }
    });
}
