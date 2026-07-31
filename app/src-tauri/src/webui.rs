//! 网页访问（plan §7.5）：App 自己在一个独立端口上服静态前端 + `GET /ipc-endpoint`。
//!
//! **为什么在 App 而不是 daemon**：daemon 是音频与网络引擎，只暴露本机 IPC 一个面；
//! 发静态页面属于表现层。这件事一度实现在 daemon 上（复用对外控制端口 + 回环闸门），
//! 已作废并回退（268c9f5）——它打破分层，还让引擎多出静态文件服务与路径穿越防护
//! 这类本不该进来的攻击面。契约不变、提供方换人：同源 `GET /ipc-endpoint` 返回
//! `{ipc_version, port, token}`（常量见 audiohub-ipc 的 `IPC_ENDPOINT_PATH`），
//! 页面据此连本机 IPC——URL 里不带令牌，这正是前端第三种连接形态。
//!
//! **已知取舍**（plan §7.5 记在案）：托盘「退出界面（音频服务继续运行）」之后网页
//! 入口随之消失，音频不受影响。这是分离设计的必然结果，不是缺陷。
//!
//! **没有鉴权**（用户明示）。因此 `local_only` 默认开、且关掉时 UI 必须明说后果：
//! 那一刻整个局域网都能打开这个界面，而 `/ipc-endpoint` 会把 IPC 令牌明文交出去。
//! 代码这一侧能做的是让默认值安全：`local_only` 时**真的只 bind 127.0.0.1**，
//! 而不是 bind 0.0.0.0 再按来源过滤——没监听在那儿，就没有"过滤逻辑写错了"这一说。

use std::collections::HashSet;
use std::ffi::OsStr;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::AppHandle;

use crate::{config_dir, read_endpoint, warn};

/// 前端根目录的环境变量覆盖。改这一个常量就能把服务指向别处（`daemon_binary()`
/// 的 `AUDIOHUB_BIN` 是同一套思路）。
const ROOT_ENV: &str = "AUDIOHUB_WEBUI_ROOT";

/// 设置落在 App 自己的配置文件里，**不进 daemon 的 settings.json**：这是 App 级
/// 的事，daemon 不该为它长一个字段。
const SETTINGS_FILE: &str = "webui.json";

/// 默认端口。刻意避开 daemon 的 47810（对外控制）与回归脚本惯用的 4782x。
pub const DEFAULT_PORT: u16 = 47800;

/// 低于 1024 需要 root，绑定必然失败——与其让用户看一条 EACCES，不如直接拒绝。
const MIN_PORT: u16 = 1024;

/// **恒为回环**：无论 `webui.json` 里的 `local_only` 写成什么，都只 bind 127.0.0.1。
///
/// 这不是"多一层保险"，而是当前唯一正确的行为（plan §7.5，用户裁定）：放开局域网
/// 换不来一个能用的远程界面——实测（本机 ↔ 30-win）对端能拿到页面和 `/ipc-endpoint`
/// 里的 IPC 令牌，却连不上服务，因为 daemon 的 IPC 只监听回环；即便在本机改用局域网
/// 地址打开，浏览器也会按「私有网络访问」规则拦掉从局域网页面指向回环的连接。于是
/// 关掉 `local_only` 的净效果只剩"把令牌发出去"。
///
/// 闸门放在这里（真正 bind 的那一行）而不是只把 UI 开关禁用掉，是因为**这个文件是
/// 手写可改的**：某个旧版本存下的 `local_only:false` 会在下次启动时静悄悄地把令牌
/// 挂上局域网，而界面上既看不出来、也没有能把它关回去的控件。
///
/// 解除条件：App 提供一条把 IPC 转发出去的通路。那条通路一旦存在，「暂不做鉴权」
/// 就不能同时成立——远程可操作与无鉴权二者只能取其一。
const FORCE_LOCAL_ONLY: bool = true;

/// 存的值 → 真正生效的值。
fn effective_local_only(stored: bool) -> bool {
    stored || FORCE_LOCAL_ONLY
}

const READ_TIMEOUT: Duration = Duration::from_secs(10);
const WRITE_TIMEOUT: Duration = Duration::from_secs(15);
/// 请求头总量上限：超了直接断，不给人拿一条连接慢慢喂头的机会。
const MAX_HEAD: usize = 8 * 1024;
/// 同时在处理的连接数上限。局域网模式下这是唯一挡住"开一千条连接"的东西。
const MAX_INFLIGHT: usize = 32;
const MAX_SEGMENTS: usize = 24;
const MAX_PATH_BYTES: usize = 1024;

// ---------------------------------------------------------------- 设置与状态

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct WebUiSettings {
    /// 默认**关闭**：没人要求时不开监听端口。
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_port")]
    pub port: u16,
    /// 默认**开启**：只 bind 回环。
    #[serde(default = "default_true")]
    pub local_only: bool,
}

fn default_port() -> u16 {
    DEFAULT_PORT
}

fn default_true() -> bool {
    true
}

impl Default for WebUiSettings {
    fn default() -> Self {
        Self { enabled: false, port: DEFAULT_PORT, local_only: true }
    }
}

/// UI 的写入面。三个字段各自可选：只改一个不必回传另外两个。
#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub struct WebUiPatch {
    pub enabled: Option<bool>,
    pub port: Option<u16>,
    pub local_only: Option<bool>,
}

/// 读写两个方向共用的回包形状：设置值 + 真实运行状态。UI 永远以它为准，不做乐观
/// 翻转——端口被占用时开关必须停在"没开起来"，而不是显示成已启用。
#[derive(Debug, Clone, Serialize)]
pub struct WebUiStatus {
    pub enabled: bool,
    pub port: u16,
    /// **生效值**，不是文件里存的值：`FORCE_LOCAL_ONLY` 期间恒为 true。界面显示的
    /// 必须是真正在发生的事，否则开关会指着一个并不成立的状态。
    pub local_only: bool,
    /// 这个开关当前被锁死（并附带原因，见 `FORCE_LOCAL_ONLY`）。由服务端下发而不是
    /// 前端自己写死，解锁那天两侧不会各改一处、漏一处。
    pub local_only_locked: bool,
    /// 端口真的在监听。`enabled && !running` 就是 `error` 里那条原因。
    pub running: bool,
    /// 本机可用的地址（running 时才有）。
    pub url: Option<String>,
    /// 局域网地址：仅 `!local_only` 且探到本机出口 IP 时给出。
    pub lan_url: Option<String>,
    /// 前端来源：`disk`（磁盘目录）/ `embedded`（编译进可执行文件的 frontendDist）。
    pub source: Option<&'static str>,
    pub root: Option<String>,
    pub error: Option<String>,
}

struct Running {
    stop: Arc<AtomicBool>,
    port: u16,
    join: JoinHandle<()>,
    source: &'static str,
    root: Option<String>,
}

struct Inner {
    settings: WebUiSettings,
    running: Option<Running>,
    error: Option<String>,
}

fn state() -> &'static Mutex<Inner> {
    static STATE: OnceLock<Mutex<Inner>> = OnceLock::new();
    STATE.get_or_init(|| {
        Mutex::new(Inner { settings: load_settings(), running: None, error: None })
    })
}

fn lock() -> MutexGuard<'static, Inner> {
    state().lock().unwrap_or_else(|e| e.into_inner())
}

fn settings_path() -> PathBuf {
    config_dir().join(SETTINGS_FILE)
}

fn load_settings() -> WebUiSettings {
    let Ok(bytes) = std::fs::read(settings_path()) else {
        return WebUiSettings::default();
    };
    match serde_json::from_slice::<WebUiSettings>(&bytes) {
        Ok(s) => s,
        Err(e) => {
            // 坏文件不该把网页服务变成"随机开着"：回到全默认（关闭 + 仅本机）。
            warn(&format!("webui: {} 解析失败（{e}），使用默认设置", settings_path().display()));
            WebUiSettings::default()
        }
    }
}

fn save_settings(s: &WebUiSettings) -> Result<(), String> {
    let dir = config_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("无法创建配置目录 {}：{e}", dir.display()))?;
    let body = serde_json::to_vec_pretty(s).map_err(|e| format!("序列化设置失败：{e}"))?;
    std::fs::write(settings_path(), body)
        .map_err(|e| format!("无法写入 {}：{e}", settings_path().display()))
}

// ---------------------------------------------------------------- 前端来源

/// 静态资源从哪来。
///
/// AppHandle 挂在 Embedded 分支里而不是 Ctx 上，是为了让磁盘分支完全不依赖 Tauri
/// 运行时——测试可以直接起一个真实监听、发真实请求，验证路径闸门与启停/改绑，
/// 不必先造一个 App。
enum Content {
    /// 磁盘目录。**已 canonicalize**，路径闸门以它为界。
    Dir(PathBuf),
    /// 编译进可执行文件的 frontendDist（Tauri 的 asset store）。keys 是允许命中的
    /// 全部资源名（带前导 `/`）——**必须先自己查表**，因为 `asset_resolver().get()`
    /// 对任何查不到的路径都会回落到 index.html，直接用它等于把 404 变成 200。
    Embedded(HashSet<String>, AppHandle),
}

/// 磁盘上的前端根目录。顺序与 `daemon_binary()` 一致：环境变量 → 可执行文件旁
/// （打包形态）→ 开发树（仅 debug）。
///
/// 开发树那一条**只在 debug 构建里**参与：release 的 .app 装到 /Applications 后，
/// `exe/../../..` 就是 /Applications 本身，让谁能在那儿建个 `ui/` 谁就能决定这个
/// 服务发什么——`daemon_binary()` 出于同样的理由把开发态候选关在 debug 里。
fn disk_root() -> Option<PathBuf> {
    let exe_dir = std::env::current_exe().ok().and_then(|e| e.parent().map(PathBuf::from));
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(p) = std::env::var_os(ROOT_ENV) {
        if !p.is_empty() {
            candidates.push(PathBuf::from(p));
        }
    }
    if let Some(dir) = &exe_dir {
        // macOS 包：Contents/MacOS/<exe> → Contents/Resources/ui。
        candidates.push(dir.join("../Resources/ui"));
        // 便携/Windows 布局：可执行文件旁边一个 ui/。
        candidates.push(dir.join("ui"));
    }
    #[cfg(debug_assertions)]
    if let Some(dir) = &exe_dir {
        // app/src-tauri/target/<profile>/<exe> → app/ui
        candidates.push(dir.join("../../../ui"));
    }
    candidates.into_iter().find_map(|p| {
        let abs = std::fs::canonicalize(&p).ok()?;
        // 没有 index.html 的目录不算前端：宁可回落到内嵌资源，也不要服出一个空壳。
        (abs.is_dir() && abs.join("index.html").is_file()).then_some(abs)
    })
}

impl Content {
    fn resolve(app: &AppHandle) -> Self {
        if let Some(dir) = disk_root() {
            return Content::Dir(dir);
        }
        // 打包形态的常态：frontendDist 被编译进了可执行文件，磁盘上并没有 ui/。
        let keys: HashSet<String> =
            app.asset_resolver().iter().map(|(k, _)| k.into_owned()).collect();
        Content::Embedded(keys, app.clone())
    }

    fn kind(&self) -> &'static str {
        match self {
            Content::Dir(_) => "disk",
            Content::Embedded(..) => "embedded",
        }
    }

    fn root(&self) -> Option<String> {
        match self {
            Content::Dir(p) => Some(p.display().to_string()),
            Content::Embedded(..) => None,
        }
    }
}

// ---------------------------------------------------------------- 路径闸门

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// 百分号解码。**必须在拆分之前做**：`..%2f..` 只有先解码才会变成两级上跳，
/// 拆完再解码就成了一个名叫 `..%2f..` 的普通文件名，闸门形同虚设。
/// 转义写错（`%zz`、结尾半个 `%`）一律返回 None → 400，不做"尽力而为"的容错。
fn percent_decode(s: &str) -> Option<String> {
    let raw = s.as_bytes();
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'%' {
            if i + 2 >= raw.len() {
                return None;
            }
            out.push(hex(raw[i + 1])? * 16 + hex(raw[i + 2])?);
            i += 3;
        } else {
            out.push(raw[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// 请求路径 → 一串**纯文件名**段。任何一段不是 `Component::Normal` 就整体拒绝
/// （`.`、`..`、盘符前缀、绝对路径全在此落网），调用方据此回 400。
///
/// `\` 与 `/` 同等视作分隔符：Windows 上 `..\..\x` 是货真价实的上跳，而 macOS 上
/// 反斜杠是合法文件名字符——统一当分隔符只会更严，不会漏。
fn sanitize(path: &str) -> Option<Vec<String>> {
    if path.len() > MAX_PATH_BYTES {
        return None;
    }
    let decoded = percent_decode(path)?;
    if decoded.contains('\0') {
        return None;
    }
    let unified = decoded.replace('\\', "/");
    let mut out: Vec<String> = Vec::new();
    for seg in unified.split('/') {
        if seg.is_empty() {
            continue; // `//` 只是多余的分隔符
        }
        let mut comps = Path::new(seg).components();
        match (comps.next(), comps.next()) {
            (Some(Component::Normal(s)), None) if s == OsStr::new(seg) => {
                out.push(seg.to_string());
            }
            _ => return None,
        }
        if out.len() > MAX_SEGMENTS {
            return None;
        }
    }
    Some(out)
}

fn mime_for(name: &str) -> &'static str {
    let ext = Path::new(name).extension().and_then(OsStr::to_str).unwrap_or("");
    match ext.to_ascii_lowercase().as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "wasm" => "application/wasm",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

// ---------------------------------------------------------------- HTTP

struct Ctx {
    content: Content,
    local_only: bool,
    inflight: AtomicUsize,
}

struct InflightGuard<'a>(&'a AtomicUsize);

impl Drop for InflightGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn write_response(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &[u8],
    head_only: bool,
    extra: &[(&str, &str)],
) -> std::io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         X-Content-Type-Options: nosniff\r\n\
         Connection: close\r\n",
        body.len()
    );
    for (k, v) in extra {
        head.push_str(k);
        head.push_str(": ");
        head.push_str(v);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    if !head_only {
        stream.write_all(body)?;
    }
    stream.flush()
}

fn text_response(
    stream: &mut TcpStream,
    status: &str,
    msg: &str,
    head_only: bool,
) -> std::io::Result<()> {
    write_response(stream, status, "text/plain; charset=utf-8", msg.as_bytes(), head_only, &[])
}

/// 读到请求头结束（`\r\n\r\n`）为止，带上限。返回头部字节。
fn read_head(stream: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Ok(buf); // 对端提前关闭：交给调用方判空
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            return Ok(buf);
        }
        if buf.len() > MAX_HEAD {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "请求头过大"));
        }
    }
}

fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.split("\r\n").skip(1).find_map(|line| {
        let (k, v) = line.split_once(':')?;
        k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
    })
}

/// DNS rebinding 闸门：仅本机模式下，只认回环字面量与 localhost 作为 Host。
///
/// 没有鉴权的回环服务最现实的攻击就是这个——恶意站点把一个自己控制的域名解析到
/// 127.0.0.1，用户一打开，那个页面就与我们同源，于是它能读 `/ipc-endpoint` 里的
/// IPC 令牌。局域网模式不设这道闸：用户已经明确接受"整个局域网可见"，而那时
/// `mymac.local` 这类名字是正当访问路径，卡掉它只会让开关看起来是坏的。
fn host_allowed(head: &str) -> bool {
    let Some(host) = header(head, "host") else {
        return false; // HTTP/1.1 必带 Host
    };
    let name = match host.rsplit_once(':') {
        // IPv6 字面量 `[::1]:8080`，以及 `host:port`
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() => h,
        _ => host,
    };
    let name = name.trim_matches(|c| c == '[' || c == ']');
    if name.eq_ignore_ascii_case("localhost") {
        return true;
    }
    name.parse::<IpAddr>().map(|ip| ip.is_loopback()).unwrap_or(false)
}

fn handle(stream: &mut TcpStream, ctx: &Ctx) -> std::io::Result<()> {
    stream.set_read_timeout(Some(READ_TIMEOUT))?;
    stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
    let _ = stream.set_nodelay(true);

    // local_only 时端口就 bind 在回环上，非本机的包根本到不了这里。再核一遍是
    // 兜底：万一将来 bind 逻辑被改坏，这一行仍然拦得住。
    if ctx.local_only && !stream.peer_addr().map(|a| a.ip().is_loopback()).unwrap_or(false) {
        return text_response(stream, "403 Forbidden", "403 仅允许本机访问", false);
    }

    let raw = read_head(stream)?;
    if raw.is_empty() {
        return Ok(()); // 探活连接（含停服时自捅的那一下）
    }
    let head = String::from_utf8_lossy(&raw);
    let Some(request_line) = head.split("\r\n").next() else {
        return text_response(stream, "400 Bad Request", "400 请求格式错误", false);
    };
    let mut parts = request_line.split(' ');
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return text_response(stream, "400 Bad Request", "400 请求格式错误", false);
    };
    let head_only = method.eq_ignore_ascii_case("HEAD");
    if !head_only && !method.eq_ignore_ascii_case("GET") {
        return write_response(
            stream,
            "405 Method Not Allowed",
            "text/plain; charset=utf-8",
            "405 只支持 GET / HEAD".as_bytes(),
            false,
            &[("Allow", "GET, HEAD")],
        );
    }
    if ctx.local_only && !host_allowed(&head) {
        return text_response(stream, "421 Misdirected Request", "421 Host 不是本机地址", head_only);
    }

    // 查询串与片段不参与路由。
    let path = target.split(['?', '#']).next().unwrap_or("");

    if path == audiohub_ipc_endpoint_path() {
        return match read_endpoint() {
            Some(ep) => {
                // ipc.json 的三个值，**不含 pid**：pid 是文件所有者的存活细节，
                // 对一个已经够到所有者的客户端没有意义。
                let body = format!(
                    "{{\"ipc_version\":{},\"port\":{},\"token\":{}}}",
                    ep.ipc_version,
                    ep.port,
                    serde_json::to_string(&ep.token).unwrap_or_else(|_| "\"\"".into())
                );
                write_response(
                    stream,
                    "200 OK",
                    "application/json; charset=utf-8",
                    body.as_bytes(),
                    head_only,
                    &[("Cache-Control", "no-store")],
                )
            }
            None => write_response(
                stream,
                "503 Service Unavailable",
                "application/json; charset=utf-8",
                b"{\"error\":\"daemon not running\"}",
                head_only,
                &[("Cache-Control", "no-store")],
            ),
        };
    }

    let Some(segments) = sanitize(path) else {
        return text_response(stream, "400 Bad Request", "400 请求路径非法", head_only);
    };
    let rel = if segments.is_empty() { "index.html".to_string() } else { segments.join("/") };

    // 内容哈希过的 assets/* 可以长缓存；index.html 不行（换版本就换内容）。
    let cache = if rel.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };

    match &ctx.content {
        Content::Dir(root) => {
            let joined = root.join(&rel);
            // **两侧都 canonicalize**：macOS 上 /var 是 /private/var 的符号链接，
            // 只规范化一边会把合法文件判成越界（也会把符号链接逃逸判成合法）。
            let Ok(real) = std::fs::canonicalize(&joined) else {
                return text_response(stream, "404 Not Found", "404 没有这个文件", head_only);
            };
            if !real.starts_with(root) || !real.is_file() {
                return text_response(stream, "404 Not Found", "404 没有这个文件", head_only);
            }
            match std::fs::read(&real) {
                Ok(bytes) => write_response(
                    stream,
                    "200 OK",
                    mime_for(&rel),
                    &bytes,
                    head_only,
                    &[("Cache-Control", cache)],
                ),
                Err(_) => text_response(stream, "404 Not Found", "404 没有这个文件", head_only),
            }
        }
        Content::Embedded(keys, app) => {
            let key = format!("/{rel}");
            if !keys.contains(&key) {
                // 先自己查表，正是为了让这里能回 404：asset_resolver 自带的回落链
                // 会把任何未知路径答成 index.html。
                return text_response(stream, "404 Not Found", "404 没有这个文件", head_only);
            }
            match app.asset_resolver().get(key) {
                Some(asset) => write_response(
                    stream,
                    "200 OK",
                    &asset.mime_type,
                    &asset.bytes,
                    head_only,
                    &[("Cache-Control", cache)],
                ),
                None => text_response(stream, "404 Not Found", "404 没有这个文件", head_only),
            }
        }
    }
}

/// 契约常量的本地副本。app/ 是独立 crate（不进主 workspace，见 spec-ui §0），
/// 为一个字符串把 audiohub-ipc 连同它的依赖拖进来不划算——值必须与
/// `audiohub_ipc::IPC_ENDPOINT_PATH` 一致。
fn audiohub_ipc_endpoint_path() -> &'static str {
    "/ipc-endpoint"
}

// ---------------------------------------------------------------- 生命周期

fn start_with(settings: WebUiSettings, content: Content) -> Result<Running, String> {
    if settings.port < MIN_PORT {
        return Err(format!("端口需不小于 {MIN_PORT}"));
    }
    // 生效值可能与存的值不同：FORCE_LOCAL_ONLY 一票否决（见其文档注释）。覆盖发生时
    // 必须说出来——一个被无声改写的设置，用户既查不出也修不了。
    let local_only = effective_local_only(settings.local_only);
    if local_only && !settings.local_only {
        warn(&format!(
            "webui: {} 里 local_only=false，但本版本恒按「仅本机」处理（远程界面尚不可用，\
             放开只会把 IPC 令牌发到局域网）；设置值原样保留，不做改写",
            settings_path().display()
        ));
    }
    let bind_ip = if local_only {
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    } else {
        IpAddr::V4(Ipv4Addr::UNSPECIFIED)
    };
    let addr = SocketAddr::new(bind_ip, settings.port);
    let listener = TcpListener::bind(addr).map_err(|e| format!("无法监听 {addr}：{e}"))?;

    let source = content.kind();
    let root = content.root();
    let ctx = Arc::new(Ctx {
        content,
        local_only,
        inflight: AtomicUsize::new(0),
    });
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();

    let join = std::thread::spawn(move || {
        for conn in listener.incoming() {
            if stop_thread.load(Ordering::Acquire) {
                break;
            }
            let Ok(mut stream) = conn else { continue };
            if ctx.inflight.fetch_add(1, Ordering::AcqRel) >= MAX_INFLIGHT {
                ctx.inflight.fetch_sub(1, Ordering::AcqRel);
                let _ = text_response(&mut stream, "503 Service Unavailable", "503 连接过多", false);
                continue;
            }
            let ctx = ctx.clone();
            // 每条连接一个线程 + 读写超时：一条卡住的连接不拖住 accept 循环，
            // 也不拖住停服（停服只 join accept 线程，处理线程自己超时退出）。
            std::thread::spawn(move || {
                let _guard = InflightGuard(&ctx.inflight);
                if let Err(e) = handle(&mut stream, &ctx) {
                    if e.kind() != std::io::ErrorKind::BrokenPipe {
                        warn(&format!("webui: 连接处理失败：{e}"));
                    }
                }
                let _ = stream.shutdown(std::net::Shutdown::Both);
            });
        }
    });

    warn(&format!(
        "webui: 监听 {addr}（{}{}）{}",
        if local_only { "仅本机" } else { "局域网可见 · 无鉴权" },
        root.as_ref().map(|r| format!(" · {r}")).unwrap_or_else(|| " · 内嵌资源".into()),
        // 放开局域网时把探到的出口地址一并写进日志：界面上那一行同源自这里，
        // 日志里有它才能在不开界面的情况下确认探测确实成功了。
        if local_only {
            String::new()
        } else {
            lan_ip()
                .map(|ip| format!(" 局域网入口 http://{ip}:{}/", settings.port))
                .unwrap_or_else(|| " 局域网入口：未能探测到出口地址".into())
        }
    ));
    Ok(Running { stop, port: settings.port, join, source, root })
}

fn stop_running(running: Option<Running>) {
    let Some(r) = running else { return };
    r.stop.store(true, Ordering::Release);
    // 捅一下自己把 accept 唤醒。bind 在 0.0.0.0 时回环同样属于它，所以这里恒用
    // 127.0.0.1——比对 0.0.0.0 发起连接更稳。
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, r.port));
    let _ = TcpStream::connect_timeout(&addr, Duration::from_millis(500));
    let _ = r.join.join();
}

/// 本机出口 IP。**不发包**：connect 一个不可路由的 UDP 目标只让内核选路由，
/// 用来回答"局域网里该用哪个地址找我"。拿不到就不显示，不猜。
fn lan_ip() -> Option<IpAddr> {
    let sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    sock.connect((Ipv4Addr::new(192, 0, 2, 1), 9)).ok()?; // TEST-NET-1，永不可达
    let ip = sock.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
}

fn status_locked(inner: &Inner) -> WebUiStatus {
    let running = inner.running.as_ref();
    let local_only = effective_local_only(inner.settings.local_only);
    WebUiStatus {
        enabled: inner.settings.enabled,
        port: inner.settings.port,
        local_only,
        local_only_locked: FORCE_LOCAL_ONLY,
        running: running.is_some(),
        url: running.map(|r| format!("http://127.0.0.1:{}/", r.port)),
        lan_url: running.filter(|_| !local_only).and_then(|r| {
            lan_ip().map(|ip| format!("http://{ip}:{}/", r.port))
        }),
        source: running.map(|r| r.source),
        root: running.and_then(|r| r.root.clone()),
        error: inner.error.clone(),
    }
}

/// 停掉当前实例，再按设置重开。改端口/改绑定范围都走这一条路——用户点一下就该
/// 立刻生效，不该要求重启 App。
///
/// `make` 是资源来源的工厂而不是现成的 `Content`：关着的时候不该白白去解析一遍前端
/// 目录，而测试可以塞一个磁盘目录进来，于是「落盘 → 停 → 改绑」这条真正的路径能在
/// 没有 GUI 的情况下被完整执行一遍。
fn reapply(inner: &mut Inner, make: &dyn Fn() -> Content) {
    stop_running(inner.running.take());
    inner.error = None;
    if !inner.settings.enabled {
        return;
    }
    match start_with(inner.settings, make()) {
        Ok(r) => inner.running = Some(r),
        Err(e) => {
            warn(&format!("webui: 启动失败：{e}"));
            inner.error = Some(e);
        }
    }
}

/// 合并补丁 → 落盘 → 立即生效。命令与测试共用这一条路径。
fn apply_patch(
    inner: &mut Inner,
    patch: WebUiPatch,
    make: &dyn Fn() -> Content,
) -> Result<WebUiStatus, String> {
    let mut next = inner.settings;
    if let Some(v) = patch.enabled {
        next.enabled = v;
    }
    if let Some(v) = patch.port {
        if v < MIN_PORT {
            return Err(format!("端口需在 {MIN_PORT}–65535 之间"));
        }
        next.port = v;
    }
    if let Some(v) = patch.local_only {
        next.local_only = v;
    }
    inner.settings = next;
    // 先落盘再应用：绑定失败（端口被占）也要留住用户的意图，否则重开 App 时设置会
    // 莫名其妙地退回去。失败原因走 status.error 报给界面。
    save_settings(&next)?;
    reapply(inner, make);
    Ok(status_locked(inner))
}

/// App 启动时调用：设置里开着就把服务拉起来，关着就什么都不做。
pub fn init(app: &AppHandle) {
    let mut inner = lock();
    if inner.settings.enabled {
        reapply(&mut inner, &|| Content::resolve(app));
    }
}

/// App 退出前调用：把监听端口交回系统。
pub fn shutdown() {
    let mut inner = lock();
    stop_running(inner.running.take());
}

// ---------------------------------------------------------------- 命令

#[tauri::command]
pub fn get_webui_status() -> WebUiStatus {
    status_locked(&lock())
}

#[tauri::command]
pub fn set_webui_settings(app: AppHandle, settings: WebUiPatch) -> Result<WebUiStatus, String> {
    let mut inner = lock();
    apply_patch(&mut inner, settings, &|| Content::resolve(&app))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traversal_is_rejected() {
        for bad in [
            "/../etc/passwd",
            "/assets/../../etc/passwd",
            "/%2e%2e/etc/passwd",
            "/..%2fetc%2fpasswd",
            "/..\\..\\windows\\win.ini",
            "/%2e%2e%5c%2e%2e%5cwindows",
            "/.",
            "/a/./b",
            "/%zz",
            "/%2",
        ] {
            assert!(sanitize(bad).is_none(), "应当拒绝：{bad}");
        }
    }

    #[test]
    fn plain_paths_survive() {
        assert_eq!(sanitize("/").unwrap(), Vec::<String>::new());
        assert_eq!(sanitize("/index.html").unwrap(), vec!["index.html"]);
        assert_eq!(
            sanitize("/assets/index-BHJpio2U.css").unwrap(),
            vec!["assets", "index-BHJpio2U.css"]
        );
        // 百分号解码后仍是普通文件名。
        assert_eq!(sanitize("/assets/a%20b.js").unwrap(), vec!["assets", "a b.js"]);
    }

    /// 一次完整的请求-应答，读到服务端关连接为止（我们恒发 Connection: close）。
    fn req(port: u16, raw: &str) -> String {
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).expect("连接失败");
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        s.write_all(raw.as_bytes()).unwrap();
        let mut out = Vec::new();
        let _ = s.read_to_end(&mut out);
        String::from_utf8_lossy(&out).into_owned()
    }

    fn get(port: u16, path: &str) -> String {
        req(port, &format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"))
    }

    fn free_port() -> u16 {
        let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        l.local_addr().unwrap().port()
    }

    /// 端到端：真监听、真 socket、真文件。覆盖静态服务、`/ipc-endpoint`、路径闸门、
    /// 以及停服后端口确实被交回（改端口重开正是靠这一条成立）。
    #[test]
    fn end_to_end_over_a_real_socket() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let tmp = std::env::temp_dir().join(format!("audiohub-webui-{stamp}"));
        let root = tmp.join("ui");
        std::fs::create_dir_all(root.join("assets")).unwrap();
        std::fs::write(root.join("index.html"), "<!doctype html><title>AH-INDEX</title>").unwrap();
        std::fs::write(root.join("assets/app.js"), "export const AH='JS';").unwrap();
        // 闸门之外的文件：任何一次穿越尝试只要拿到它，测试就该红。
        std::fs::write(tmp.join("secret.txt"), "AH-SECRET-LEAKED").unwrap();

        let cfg = tmp.join("cfg");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(
            cfg.join("ipc.json"),
            br#"{"ipc_version":1,"port":54321,"token":"tok-abc","pid":4242}"#,
        )
        .unwrap();
        std::env::set_var("AUDIOHUB_CONFIG_DIR", &cfg);

        let port = free_port();
        let canonical = std::fs::canonicalize(&root).unwrap();
        let running = start_with(
            WebUiSettings { enabled: true, port, local_only: true },
            Content::Dir(canonical),
        )
        .expect("监听失败");

        // 静态资源
        let index = get(port, "/");
        assert!(index.starts_with("HTTP/1.1 200 OK"), "{index}");
        assert!(index.contains("AH-INDEX"), "{index}");
        assert!(index.contains("text/html"), "{index}");
        let js = get(port, "/assets/app.js");
        assert!(js.starts_with("HTTP/1.1 200 OK") && js.contains("text/javascript"), "{js}");
        assert!(get(port, "/nope.js").starts_with("HTTP/1.1 404"));

        // /ipc-endpoint：三个字段，**不含 pid**
        let ep = get(port, "/ipc-endpoint");
        assert!(ep.starts_with("HTTP/1.1 200 OK"), "{ep}");
        assert!(ep.contains(r#""ipc_version":1"#) && ep.contains(r#""port":54321"#), "{ep}");
        assert!(ep.contains(r#""token":"tok-abc""#), "{ep}");
        assert!(!ep.contains("pid"), "pid 不该出现在回包里：{ep}");

        // 路径穿越：四种写法都必须是错误，且绝不能带出闸门外的内容
        for bad in [
            "/../secret.txt",
            "/assets/../../secret.txt",
            "/%2e%2e/secret.txt",
            "/..%2fsecret.txt",
            "/..\\secret.txt",
            "/%2e%2e%5csecret.txt",
        ] {
            let r = get(port, bad);
            assert!(r.starts_with("HTTP/1.1 400"), "{bad} 应当 400，实际：{r}");
            assert!(!r.contains("AH-SECRET-LEAKED"), "{bad} 漏出了闸门外的文件");
        }

        // Host 闸门（仅本机模式）与方法白名单
        let rebind = req(port, &format!("GET / HTTP/1.1\r\nHost: evil.example:{port}\r\n\r\n"));
        assert!(rebind.starts_with("HTTP/1.1 421"), "{rebind}");
        let post = req(port, &format!("POST / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"));
        assert!(post.starts_with("HTTP/1.1 405"), "{post}");

        // 停服：端口必须真的交回来，否则「改端口立即生效」只是看起来生效
        stop_running(Some(running));
        let reuse = TcpListener::bind((Ipv4Addr::LOCALHOST, port));
        assert!(reuse.is_ok(), "停服后端口仍被占用：{:?}", reuse.err());
        drop(reuse);

        // ---- 第二段：UI 那条写入路径（落盘 → 停 → 改绑），不重启进程 ----
        // `set_webui_settings` 命令除了取 AppHandle 之外就是这一句，所以这里跑到的
        // 是同一段代码：合并补丁、写 webui.json、原地重开监听。
        let root_for_apply = std::fs::canonicalize(&root).unwrap();
        let make = move || Content::Dir(root_for_apply.clone());
        let mut inner = lock();

        let p1 = free_port();
        let st = apply_patch(
            &mut inner,
            WebUiPatch { enabled: Some(true), port: Some(p1), local_only: Some(true) },
            &make,
        )
        .expect("启用失败");
        assert!(st.running && st.enabled && st.local_only, "{st:?}");
        assert_eq!(st.url.as_deref(), Some(format!("http://127.0.0.1:{p1}/").as_str()));
        assert!(get(p1, "/").contains("AH-INDEX"));
        // 落盘了，而且落的是 App 自己的文件——不是 daemon 的 settings.json。
        let saved = std::fs::read_to_string(cfg.join("webui.json")).unwrap();
        assert!(saved.contains("\"enabled\": true") && saved.contains(&p1.to_string()), "{saved}");

        // 改端口：旧端口必须当场空出来，新端口当场能服务。
        let p2 = free_port();
        let st = apply_patch(&mut inner, WebUiPatch { port: Some(p2), ..Default::default() }, &make)
            .expect("改端口失败");
        assert!(st.running && st.port == p2);
        assert!(get(p2, "/").contains("AH-INDEX"));
        assert!(TcpListener::bind((Ipv4Addr::LOCALHOST, p1)).is_ok(), "旧端口没被交回");

        // local_only 关：**必须依然只绑回环**（FORCE_LOCAL_ONLY，plan §7.5 用户裁定）。
        // 判据是两次 bind 的结果，不是我们自己报的字段——回环被占住、通配地址空着，
        // 才证明监听真的在 127.0.0.1 上。
        let st =
            apply_patch(&mut inner, WebUiPatch { local_only: Some(false), ..Default::default() }, &make)
                .expect("写 local_only=false 失败");
        assert!(st.running, "{st:?}");
        assert!(st.local_only && st.local_only_locked, "生效值应当仍是「仅本机」：{st:?}");
        assert!(st.lan_url.is_none(), "锁死期间不该报出局域网地址：{st:?}");
        assert!(
            TcpListener::bind((Ipv4Addr::LOCALHOST, p2)).is_err(),
            "回环端口没被占——监听根本没起来？"
        );
        let wildcard = TcpListener::bind((Ipv4Addr::UNSPECIFIED, p2));
        assert!(wildcard.is_ok(), "local_only=false 竟然绑上了对外地址：{:?}", wildcard.err());
        drop(wildcard);
        // 配置**原样保留**（schema 不动，将来解锁是一行的事，不是一次迁移）。
        let saved = std::fs::read_to_string(cfg.join("webui.json")).unwrap();
        assert!(saved.contains("\"local_only\": false"), "存的值被悄悄改写了：{saved}");

        // 再写回 true：生效值不变，服务照常。
        let st =
            apply_patch(&mut inner, WebUiPatch { local_only: Some(true), ..Default::default() }, &make)
                .expect("写 local_only=true 失败");
        assert!(st.running && st.local_only);
        assert!(get(p2, "/").contains("AH-INDEX"));

        // 关掉：端口彻底交回。
        let st =
            apply_patch(&mut inner, WebUiPatch { enabled: Some(false), ..Default::default() }, &make)
                .expect("关闭失败");
        assert!(!st.running && !st.enabled && st.url.is_none());
        assert!(TcpListener::bind((Ipv4Addr::LOCALHOST, p2)).is_ok(), "关闭后端口仍被占用");

        // 端口下限：拒绝且不改变正在运行的状态。
        assert!(apply_patch(&mut inner, WebUiPatch { port: Some(80), ..Default::default() }, &make)
            .is_err());
        drop(inner);

        std::env::remove_var("AUDIOHUB_CONFIG_DIR");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 出口 IP 探测不该 panic，也不该把回环/未指定地址当成"局域网地址"报出去。
    /// **不断言一定探得到**：没有默认路由的机器（离线 CI）返回 None 是正确行为。
    #[test]
    fn lan_ip_is_sane_or_absent() {
        if let Some(ip) = lan_ip() {
            assert!(!ip.is_loopback() && !ip.is_unspecified(), "探到的地址不可用：{ip}");
        }
    }

    #[test]
    fn host_gate() {
        let mk = |h: &str| format!("GET / HTTP/1.1\r\nHost: {h}\r\n\r\n");
        for good in ["127.0.0.1:47800", "localhost:47800", "localhost", "[::1]:47800", "127.0.0.1"] {
            assert!(host_allowed(&mk(good)), "应当放行：{good}");
        }
        for bad in ["evil.example", "evil.example:47800", "10.130.32.236:47800", "mymac.local"] {
            assert!(!host_allowed(&mk(bad)), "应当拒绝：{bad}");
        }
        assert!(!host_allowed("GET / HTTP/1.1\r\n\r\n"));
    }
}
