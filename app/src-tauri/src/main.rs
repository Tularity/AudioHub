#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::io::{Read, Seek, SeekFrom};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager, RunEvent, WindowEvent};

mod webui;

/// Must match audiohub_ipc::IPC_VERSION (contract: core/audiohub-ipc/src/lib.rs).
const IPC_VERSION: u32 = 1;

/// How long a freshly spawned daemon gets to publish a connectable ipc.json.
const READY_TIMEOUT: Duration = Duration::from_secs(8);

const MAIN_WINDOW: &str = "main";

/// Menu bar glyph, raw RGBA straight from icons/make-icons.py. Raw instead of
/// PNG so the shell does not need tauri's image-png feature (which pulls the
/// whole `image` crate in for one 44x44 icon).
const TRAY_RGBA: &[u8] = include_bytes!("../icons/tray.rgba");
const TRAY_PX: u32 = 44;
const _: () = assert!(TRAY_RGBA.len() == (TRAY_PX * TRAY_PX * 4) as usize);

fn warn(msg: &str) {
    eprintln!("[audiohub] {msg}");
}

/// Mirror of audiohub_ipc::IpcEndpoint (`<config_dir>/ipc.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
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
        Self { kind, message: message.into(), detail: None }
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
    let name = if cfg!(windows) { "audiohub.exe" } else { "audiohub" };
    let exe_dir = std::env::current_exe().ok().and_then(|e| e.parent().map(PathBuf::from));
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

fn daemon_binary() -> Option<PathBuf> {
    // `audiohubd` FIRST. Both binaries run the same daemon (`audiohub daemon`
    // is the CLI subcommand that calls into it), but the process a user finds
    // in Activity Monitor / Task Manager should be named for what it is. A peer
    // that only ever showed `audiohub` reads as "they shipped me a CLI", which
    // is exactly the impression this app exists to correct. The CLI stays as
    // the fallback because that is what the macOS bundle ships as its sidecar.
    let names: [&str; 2] = if cfg!(windows) {
        ["audiohubd.exe", "audiohub.exe"]
    } else {
        ["audiohubd", "audiohub"]
    };
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(PathBuf::from));

    let mut candidates: Vec<PathBuf> = Vec::new();
    // First priority, so a bundled .app copied anywhere self-bootstraps from
    // the daemon shipped next to this executable (Contents/MacOS/audiohub).
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

#[tauri::command]
fn get_ipc_endpoint() -> Option<IpcEndpointJson> {
    read_endpoint()
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
    let f = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
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

    // Idempotence: a healthy daemon is never restarted.
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
        DaemonError::new("no-binary", "未找到 audiohub 服务程序").with_detail(format!(
            "已查找：本程序所在目录、环境变量 AUDIOHUB_BIN（当前 {}）",
            std::env::var("AUDIOHUB_BIN").unwrap_or_else(|_| "未设置".into())
        ))
    })?;

    let (log_sink, log_from, log_note) = match open_daemon_log() {
        Ok((f, at)) => (Stdio::from(f), at, format!("日志：{}", daemon_log_path().display())),
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

    // Reap the child so it is not a zombie for as long as this process lives.
    // The thread parks in wait() for the daemon's whole life and dies with us —
    // the daemon is then reparented to launchd and keeps running, as intended.
    std::thread::spawn(move || {
        let _ = child.wait();
    });

    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        if let Some(ep) = read_endpoint() {
            if endpoint_alive(&ep) {
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
            return Err(if busy {
                DaemonError::new("port-busy", "AudioHub 服务所需的端口已被占用")
                    .with_detail(detail)
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
    tauri::async_runtime::spawn_blocking(ensure_daemon_blocking)
        .await
        .map_err(|e| DaemonError::new("internal", format!("ensure_daemon 任务失败：{e}")))?
}

// ---- window / tray ----

/// The tray's status line, kept so the frontend can push connection changes.
struct TrayStatus(MenuItem<tauri::Wry>);

fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window(MAIN_WINDOW) {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
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
    if zoomed { window.unmaximize() } else { window.maximize() }.map_err(|e| e.to_string())
}

#[tauri::command]
fn set_tray_status(app: AppHandle, online: bool, port: Option<u16>) {
    if let Some(s) = app.try_state::<TrayStatus>() {
        let text = match (online, port) {
            (true, Some(p)) => format!("状态：在线 · 端口 {p}"),
            (true, None) => "状态：在线".to_string(),
            (false, _) => "状态：离线".to_string(),
        };
        let _ = s.0.set_text(text);
    }
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

fn build_tray(app: &AppHandle) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "显示主窗口", true, None::<&str>)?;
    // Informational only; disabled so it cannot be "clicked".
    let status = MenuItem::with_id(app, "status", "状态：连接中…", false, None::<&str>)?;
    let quit_ui_item = MenuItem::with_id(
        app,
        "quit_ui",
        "退出界面（音频服务继续运行）",
        true,
        None::<&str>,
    )?;
    let quit_all = MenuItem::with_id(app, "quit_all", "停止音频服务并退出", true, None::<&str>)?;
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

    app.manage(TrayStatus(status));

    TrayIconBuilder::with_id("main")
        .icon(tauri::image::Image::new(TRAY_RGBA, TRAY_PX, TRAY_PX))
        // macOS template image: AppKit keeps only the alpha channel and recolours
        // it for the current menu bar appearance.
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

fn main() {
    let app = tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            get_ipc_endpoint,
            ensure_daemon,
            set_tray_status,
            show_main_window,
            start_window_drag,
            toggle_window_zoom,
            quit_ui,
            stop_daemon_and_quit,
            webui::get_webui_status,
            webui::set_webui_settings
        ])
        .setup(|app| {
            build_tray(app.handle())?;
            // 网页访问（plan §7.5）：设置里开着才会真的开监听端口，默认关闭。
            webui::init(app.handle());
            Ok(())
        })
        // Closing the window hides to the menu bar; only the tray quit items
        // really exit. Never destroying the window is also what keeps the app
        // alive after the last close.
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
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
        if let RunEvent::Reopen { has_visible_windows, .. } = _event {
            if !has_visible_windows {
                show_main(_app);
            }
        }
    });
}
