//! 开机自启（plan M9「开机自启」）。
//!
//! # 拉起的是 App，不是 daemon
//!
//! Windows 侧的计划任务名叫 `AudioHubDaemon`，实际执行的却是 `audiohub-app.exe`
//! （`scripts/install-windows-autostart.ps1` 的决定 3）。macOS 照同一个模型：
//! 登录时启动 `AudioHub.app`，由它按 `app/src-tauri/src/main.rs::ensure_daemon`
//! 把 daemon 带起来。两端一致的理由不只是对称——App 才有托盘、才有那条
//! 「退出界面（音频服务继续运行）」的语义；直接拉 daemon 会得到一台**没有任何
//! 界面入口**的机器。
//!
//! # 为什么是 LaunchAgent，不是 `SMAppService`
//!
//! 1. `SMAppService.mainApp` 要求由 **App 自己的进程**调用，而这个开关按契约
//!    必须挂在 daemon 的 IPC 上（Windows 侧同一个键）。从 daemon 调它需要链
//!    ServiceManagement 框架并伪装成宿主 App，得不偿失。
//! 2. 它把状态存在系统里，只能通过同一套框架反查；plist 是一个 `stat()` 就能
//!    读到的文件，「想要」与「事实」的分离因此是**可断言**的，而不是又一个只能
//!    靠界面截图证明的东西。
//! 3. 撤销路径。`SMAppService` 注册失败或残留时，用户只能去「系统设置 → 登录项」
//!    里找；本项目的 bundle 会被 `scripts/sign-dev.sh` 反复重签，而重签过一次
//!    就已经切断过一次本地网络授权。plist 的撤销是 `rm` 一个文件，无需 sudo。
//!
//! `SMLoginItemSetEnabled` 已废弃（且只服务于 helper bundle），不考虑。
//!
//! # ⚠ 这不是 §四.28 那条被否决的 launchd 布局
//!
//! 被否决的是「**daemon** 由 launchd 拉起并持有 mach 名字」（系统域 LaunchDaemon
//! 拿不到本地网络授权，用户域 LaunchAgent 的名字 coreaudiod 又看不见，
//! 见 `docs/progress.md:166-176`）。这里注册的是**用户域、拉 App 的**登录项，
//! 不声明任何 `MachServices`，daemon 依旧由 App 以普通用户进程 spawn ——
//! 也就是说，今天已经拿到麦克风/本地网络授权的那条启动路径**一字未改**。
//!
//! # 没有「存下来的开关」，注册本身就是状态
//!
//! `settings.json` 里**没有** `autostart` 字段，这是刻意的：登录项必须活过重启，
//! 而能活过重启的东西是 plist / 计划任务本身。再存一个 bool 就有了两个真值源，
//! 且它们的分歧（文件被用户手删、`settings.json` 被拷到另一台机器）没有任何一处
//! 会报错。所以 `settings.get` 里的 `autostart` 是**探测出来的事实**。
//!
//! # 只有装好的形态才允许注册
//!
//! macOS 判据是「daemon 位于某个 `*.app/Contents/MacOS/` 下」，Windows 判据是
//! 「daemon 旁边有 `audiohub-app.exe`」。跑测试的二进制两个都不满足，于是
//! **测试进程无论如何写不进用户的登录项**——这条性质由
//! `transport_tests::every_writable_setting_key_is_really_honoured` 顶着。

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};

/// LaunchAgent 的 Label，也是 plist 的文件名。
///
/// 刻意**不等于** bundle id（`com.audiohub.app`）：那个 id 归 LaunchServices /
/// `SMAppService` 所有，用同一个字符串注册一个手写 plist 只会让「这条登录项是谁
/// 装的」在两套机制之间无法区分。
pub(crate) const MAC_LABEL: &str = "com.audiohub.app.autostart";

/// Windows 计划任务名。**与 `scripts/install-windows-autostart.ps1` 逐字相同**，
/// 所以脚本与 daemon 管的是同一个对象，不会变成两套。
/// 守卫见 `tests::the_task_name_matches_the_install_script`。
#[allow(dead_code)] // 见模块开头：另一半平台的载荷靠单测保活
pub(crate) const WIN_TASK: &str = "AudioHubDaemon";

/// 探测结果。`supported` / `enabled` 的关系与 `discovery_announce` /
/// `discovery_announcing` 同构：能不能做，和此刻做没做，是两件事。
///
/// **两者互不蕴含，四种组合全都真实存在**——尤其 `supported == false &&
/// enabled == true`：登录项活得比写下它的那个 bundle 长，用户在装好的 `.app`
/// 里打开过自启、之后换成裸二进制/开发构建跑 daemon，就正好落在这一格。把这一格
/// 塌缩成「关着」曾经是本文件的真实缺陷：界面报「已关闭」、开关置灰，而 plist
/// 还在盘上、每次登录照样把 App 拉起来（plan §16.4 第 5 条）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AutostartState {
    /// 这台机器的这个形态能不能**注册**登录项。注意只管注册——
    /// 撤销从不需要它（见 [`plan_set`]）。
    pub supported: bool,
    /// 此刻真的注册着。**探测出来的**，不是某个存盘 bool 的副本，
    /// 也**不是** `supported` 的推论。
    pub enabled: bool,
    /// 登录时会被拉起的东西（已注册时读自注册项，未注册时是「将会注册什么」）。
    /// 界面靠它看出登录项指向的是不是一个已经被移走的旧 bundle。
    pub target: Option<String>,
    /// `supported == false` 时的人话理由。开关置灰而界面答不出为什么，是本项目
    /// 反复付过代价的那个形状。
    pub reason: Option<String>,
}

impl AutostartState {
    /// 「这台机器的状态**探测不出来**」。
    ///
    /// ⚠ 只用于真的读不到任何东西的路径（读不到本进程路径、读不到 HOME）。
    /// **不要**用它表达「这个形态注册不了」——那种情况下登录项可能好端端地
    /// 存在着，此处的 `enabled: false` 就成了一句谎话；那条路走
    /// [`mac_state_uninstallable`] / Windows 分支里的对应构造。
    fn unsupported(reason: impl Into<String>) -> AutostartState {
        AutostartState {
            supported: false,
            enabled: false,
            target: None,
            reason: Some(reason.into()),
        }
    }
}

// ---------------------------------------------------------------- 载荷（纯函数）
//
// 两个平台的载荷都**在每个平台上编译**，与 `halbridge_win` 的 `wire` 半边同一条
// 理由：它们的编码得在开发者这台机器上被测到，而不是只在目标机器上。执行那一半
// 才 `#[cfg]`。

/// XML 文本节点转义。路径里出现 `&` 或 `<` 时，不转义会产出一个 launchd 直接
/// 拒绝解析的 plist —— 而 launchd 拒绝解析的表现是**静默不启动**。
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// 登录时运行的 LaunchAgent plist。
///
/// 走 `/usr/bin/open` 而不是直接 exec `Contents/MacOS/audiohub-app`：`open` 让
/// 这个 App 以**与用户双击时完全相同**的方式被 LaunchServices 拉起，也就是那条
/// 已经握有麦克风与本地网络授权的身份。本项目在 launchd 身份上已经栽过两次
/// （`docs/progress.md:166-176`），不值得为省一个进程再赌一次。
///
/// `-g` = 不抢焦点：登录时把用户的注意力抢走一次，是这类工具最招人烦的行为。
///
/// `KeepAlive` **不写**（默认 false）：登录拉一次即可。写成 true 的话，用户从
/// 托盘点「停止音频服务并退出」之后 launchd 会立刻把它拽回来——一个用户明确
/// 表达过的意图被系统撤销，比不自启严重得多。
#[allow(dead_code)] // 见模块开头：另一半平台的载荷靠单测保活
fn mac_plist(label: &str, app: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{label}</string>
	<key>ProgramArguments</key>
	<array>
		<string>/usr/bin/open</string>
		<string>-g</string>
		<string>{app}</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>ProcessType</key>
	<string>Interactive</string>
</dict>
</plist>
"#,
        label = xml_escape(label),
        app = xml_escape(&app.display().to_string()),
    )
}

/// 从一个已装好的 plist 里读回它会启动什么。
///
/// 判据是 `ProgramArguments` 的**最后一个** `<string>`：前两个固定是
/// `/usr/bin/open` 与 `-g`。读不出来时返回 `None`，而不是把「注册着」也一起
/// 否掉——文件在就是注册着，读不懂它只说明界面少显示一行路径。
#[allow(dead_code)] // 见模块开头：另一半平台的载荷靠单测保活
fn mac_plist_target(body: &str) -> Option<String> {
    let args = body.split("<key>ProgramArguments</key>").nth(1)?;
    let arr = args.split("</array>").next()?;
    let last = arr.rsplit("<string>").find_map(|seg| seg.split("</string>").next())?;
    let t = last.trim();
    (!t.is_empty() && t != "-g" && t != "/usr/bin/open")
        .then(|| t.replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">"))
}

/// `schtasks /Create` 的参数表。
///
/// 走 `/XML` 而不是 `/SC ONLOGON` 一把梭：安装脚本用 `New-ScheduledTaskSettingsSet`
/// 设了一串行为（掉电不停、错过就补、隐藏、无执行时限），`/SC` 那条路一个都表达
/// 不了。同一个任务名被两条路径写成两种行为，正是这个函数存在要避免的事。
#[allow(dead_code)] // 见模块开头：另一半平台的载荷靠单测保活
fn win_schtasks_create_argv(task: &str, xml: &Path) -> Vec<String> {
    vec![
        "/Create".into(),
        "/TN".into(),
        task.into(),
        "/XML".into(),
        xml.display().to_string(),
        // 覆盖既有任务。没有它，装过一次的机器（30-win 就是）永远更新不了。
        "/F".into(),
    ]
}

#[allow(dead_code)] // 见模块开头：另一半平台的载荷靠单测保活
fn win_schtasks_delete_argv(task: &str) -> Vec<String> {
    vec!["/Delete".into(), "/TN".into(), task.into(), "/F".into()]
}

#[allow(dead_code)] // 见模块开头：另一半平台的载荷靠单测保活
fn win_schtasks_query_argv(task: &str) -> Vec<String> {
    vec!["/Query".into(), "/TN".into(), task.into()]
}

/// 计划任务定义。字段照 `scripts/install-windows-autostart.ps1` 抄，逐条对应：
/// `LogonTrigger` ← `-AtLogOn`，`InteractiveToken`/`LeastPrivilege` ←
/// `-LogonType Interactive -RunLevel Limited`，其余落在 `<Settings>` 里。
#[allow(dead_code)] // 见模块开头：另一半平台的载荷靠单测保活
fn win_task_xml(app_exe: &Path, user: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>AudioHub at logon</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{user}</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <StartWhenAvailable>true</StartWhenAvailable>
    <Hidden>true</Hidden>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <RestartOnFailure>
      <Interval>PT1M</Interval>
      <Count>999</Count>
    </RestartOnFailure>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{exe}</Command>
    </Exec>
  </Actions>
</Task>
"#,
        user = xml_escape(user),
        exe = xml_escape(&app_exe.display().to_string()),
    )
}

// ------------------------------------------------------------------ 形态判定

/// `exe` 是不是躺在某个 `*.app/Contents/MacOS/` 里；是就返回那个 bundle。
///
/// 判据是**三层结构**而不是「路径里含 .app」：后者会把
/// `~/build/Foo.app/tmp/audiohubd` 也认成 bundle，于是往登录项里写一条永远拉不
/// 起来的记录。
#[allow(dead_code)] // 见模块开头：另一半平台的载荷靠单测保活
fn app_bundle_of(exe: &Path) -> Option<PathBuf> {
    let macos = exe.parent()?;
    if macos.file_name()? != "MacOS" {
        return None;
    }
    let contents = macos.parent()?;
    if contents.file_name()? != "Contents" {
        return None;
    }
    let app = contents.parent()?;
    if app.extension()? != "app" {
        return None;
    }
    Some(app.to_path_buf())
}

/// Windows 的对应判据：daemon 旁边有没有 App 的可执行文件。
/// 与 `main.rs::daemon_binary` 反向——那边是 App 找 daemon，这边是 daemon 找 App。
#[allow(dead_code)] // 见模块开头：另一半平台的载荷靠单测保活
fn app_exe_beside(exe: &Path) -> Option<PathBuf> {
    let p = exe.parent()?.join("audiohub-app.exe");
    p.is_file().then_some(p)
}

// ------------------------------------------------------------------ 执行

/// macOS 的 `~/Library/LaunchAgents`。
#[allow(dead_code)] // 见模块开头：另一半平台的载荷靠单测保活
fn mac_agents_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/LaunchAgents"))
}

#[allow(dead_code)] // 见模块开头：另一半平台的载荷靠单测保活
fn mac_plist_path(dir: &Path, label: &str) -> PathBuf {
    dir.join(format!("{label}.plist"))
}

/// 把 plist 写进 `dir`（先写临时文件再 rename：launchd 有可能正好在读它）。
///
/// **不调 `launchctl bootstrap`。** 想要的效果本来就只在下次登录发生；现在
/// bootstrap 一下的唯一可见后果，是立刻再开一个 App 窗口。
#[allow(dead_code)] // 见模块开头：另一半平台的载荷靠单测保活
fn mac_install(dir: &Path, label: &str, app: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = mac_plist_path(dir, label);
    let tmp = path.with_extension("plist.tmp");
    std::fs::write(&tmp, mac_plist(label, app).as_bytes())?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// 删掉 plist。文件本来就不在也算成功——「关掉」这个动作是幂等的。
///
/// 同样不调 `launchctl bootout`：我们从没 bootstrap 过。上一次登录时被 launchd
/// 载入的那份是 `RunAtLoad` 一次性任务，早已跑完，留在内存里不做任何事。
#[allow(dead_code)] // 见模块开头：另一半平台的载荷靠单测保活
fn mac_uninstall(dir: &Path, label: &str) -> Result<()> {
    match std::fs::remove_file(mac_plist_path(dir, label)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// 盘上此刻**有没有**这条登录项，以及它指着什么。
///
/// 对「本进程配不配注册登录项」不持任何意见——这是本文件那条缺陷的修法核心：
/// 事实读自文件系统，形态判定是另一个正交的量。
#[allow(dead_code)] // 见模块开头：另一半平台的载荷靠单测保活
fn mac_registered(dir: &Path, label: &str) -> (bool, Option<String>) {
    match std::fs::read_to_string(mac_plist_path(dir, label)) {
        Ok(body) => (true, mac_plist_target(&body)),
        Err(_) => (false, None),
    }
}

#[allow(dead_code)] // 见模块开头：另一半平台的载荷靠单测保活
fn mac_state(dir: &Path, label: &str, app: &Path) -> AutostartState {
    let (enabled, target) = mac_registered(dir, label);
    AutostartState {
        supported: true,
        enabled,
        // 装着时读自**已装好的那份**，不是当前 bundle：两者不同正是「登录项还
        // 指着一个被移走的旧 bundle」这件事，界面得看得见。没装时退回当前
        // bundle，那是「将会注册什么」。
        target: target.or_else(|| Some(app.display().to_string())),
        reason: None,
    }
}

/// 当前形态**注册不了**登录项时的状态。
///
/// 关键在于 `enabled` 依旧读自盘上：用户完全可能在装好的 `.app` 里打开过自启，
/// 之后从开发构建/被移走的 bundle 里跑起 daemon。这一格若报成「关着」，用户看到
/// 的是一个置灰的关闭开关，而系统每次登录仍然把 App 拉起来——
/// 「明明是开的却报成关的」正是 plan §16.4 第 5 条点名的那个形状。
///
/// `target` 不退回当前程序：这个形态没有「将会注册什么」可言，能报的只有登录项
/// 里真写着的那一个。
#[allow(dead_code)] // 见模块开头：另一半平台的载荷靠单测保活
fn mac_state_uninstallable(dir: &Path, label: &str, reason: &str) -> AutostartState {
    let (enabled, target) = mac_registered(dir, label);
    AutostartState {
        supported: false,
        enabled,
        target,
        reason: Some(reason.to_string()),
    }
}

#[cfg(windows)]
fn schtasks(args: &[String]) -> std::io::Result<std::process::Output> {
    use std::os::windows::process::CommandExt;
    // daemon 被 App 以 CREATE_NO_WINDOW 拉起时自己没有控制台，此时 spawn 一个
    // 控制台子进程就会在用户桌面上弹一个黑框——安装脚本的决定 3 记的就是这个坑，
    // 只不过那次弹框的是 daemon 自己。
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    std::process::Command::new("schtasks.exe")
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .output()
}

// ------------------------------------------------------------------ 对外接口

/// 探测结果的短命缓存。
///
/// macOS 那边是一次 `stat`，无所谓；Windows 那边每次都要 spawn 一个
/// `schtasks.exe`，而 `settings.get` 是界面刷新时顺手调的。没有这层缓存，
/// 打开设置页就变成一串进程创建。
fn cache() -> &'static Mutex<Option<(Instant, AutostartState)>> {
    static C: std::sync::OnceLock<Mutex<Option<(Instant, AutostartState)>>> =
        std::sync::OnceLock::new();
    C.get_or_init(|| Mutex::new(None))
}

const CACHE_TTL: Duration = Duration::from_secs(5);

/// 此刻的登录项状态。
pub(crate) fn state() -> AutostartState {
    if let Ok(g) = cache().lock() {
        if let Some((at, s)) = g.as_ref() {
            if at.elapsed() < CACHE_TTL {
                return s.clone();
            }
        }
    }
    let s = probe();
    if let Ok(mut g) = cache().lock() {
        *g = Some((Instant::now(), s.clone()));
    }
    s
}

fn probe() -> AutostartState {
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => return AutostartState::unsupported(format!("读不到本进程的路径：{e}")),
    };

    // 两个平台同一条顺序：**先问系统「注册着没有」，再问自己「配不配注册」**。
    // 反过来（形态不合格就直接短路）会把一条真实存在的登录项报成不存在。
    #[cfg(target_os = "macos")]
    {
        let Some(dir) = mac_agents_dir() else {
            return AutostartState::unsupported("读不到 HOME，定位不了 ~/Library/LaunchAgents");
        };
        match app_bundle_of(&exe) {
            Some(app) => mac_state(&dir, MAC_LABEL, &app),
            None => mac_state_uninstallable(
                &dir,
                MAC_LABEL,
                "当前服务不是从 AudioHub.app 里运行的（例如直接跑了构建产物里的裸二进制），\
                 没有一个稳定的启动目标可以写进登录项",
            ),
        }
    }

    #[cfg(windows)]
    {
        let enabled = schtasks(&win_schtasks_query_argv(WIN_TASK))
            .map(|o| o.status.success())
            .unwrap_or(false);
        match app_exe_beside(&exe) {
            Some(app) => AutostartState {
                supported: true,
                enabled,
                target: Some(app.display().to_string()),
                reason: None,
            },
            // macOS 那一格的同构：计划任务活得比它旁边那个 App 长。
            None => AutostartState {
                supported: false,
                enabled,
                target: None,
                reason: Some(
                    "当前服务旁边没有 audiohub-app.exe，没有一个稳定的启动目标可以写进计划任务"
                        .into(),
                ),
            },
        }
    }

    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = exe;
        AutostartState::unsupported("这个平台没有实现开机自启")
    }
}

/// `set(want)` 在这个状态下该做什么。**纯函数**，好让这条策略本身能被断言，
/// 而不是只能靠「在一台碰巧是某个形态的机器上跑一遍」来证明。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SetPlan {
    /// 去动系统。
    Apply,
    /// 已经是想要的样子，什么都不用碰（撤销是幂等的，重复关一次不该报错）。
    AlreadyThere,
    /// 拒绝，并把理由回给调用方。
    Refuse,
}

/// **注册要形态，撤销不要。**
///
/// 这条不对称是有意的，也是本文件那条缺陷的另一半：注册需要一个稳定的启动目标，
/// 所以形态不合格就得拒绝（否则会写进一条指向不存在东西的登录项）；而撤销只是
/// 删掉一个已经在盘上的文件，它**恰恰是形态不合格的那台机器唯一需要的动作**。
/// 两个方向共用一个 `!supported` 闸门时，用户会得到一条自己开的、界面却关不掉的
/// 登录项——没有任何途径撤销，除非手工去 `~/Library/LaunchAgents` 删文件。
fn plan_set(want: bool, cur: &AutostartState) -> SetPlan {
    match (want, cur.supported, cur.enabled) {
        // 已经是想要的样子。
        (w, _, e) if w == e => SetPlan::AlreadyThere,
        // 开：必须有一个稳定的启动目标。
        (true, false, _) => SetPlan::Refuse,
        (true, true, _) => SetPlan::Apply,
        // 关：永远允许。
        (false, _, _) => SetPlan::Apply,
    }
}

/// 打开/关闭开机自启。
///
/// 形态不支持时**报错**而不是静默收下：一个「点了开、回包 200、什么都没发生」
/// 的开关正是本项目栽过六次的那个形状。反过来，关的方向从不因形态被拒——
/// 见 [`plan_set`]。
pub(crate) fn set(want: bool) -> Result<AutostartState> {
    let cur = probe();
    match plan_set(want, &cur) {
        SetPlan::Refuse => bail!(
            "这台机器的当前形态无法设置开机自启：{}",
            cur.reason.as_deref().unwrap_or("原因未知")
        ),
        SetPlan::AlreadyThere => {
            // `cur` 刚探测出来，就是事实；顺手把可能更旧的缓存丢掉。
            if let Ok(mut g) = cache().lock() {
                *g = None;
            }
            return Ok(cur);
        }
        SetPlan::Apply => {}
    }
    apply(want)?;
    if let Ok(mut g) = cache().lock() {
        *g = None;
    }
    let now = state();
    if now.enabled != want {
        bail!("开机自启没有变成 {want}：写完之后读回来仍是 {}", now.enabled);
    }
    Ok(now)
}

#[allow(unused_variables)]
fn apply(want: bool) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let dir = mac_agents_dir()
            .ok_or_else(|| anyhow::anyhow!("读不到 HOME"))?;
        // 撤销不问形态（`plan_set` 的不对称在这里落地）：要删的那个文件是上一个
        // 形态写下的，此刻是什么形态与它无关。
        if !want {
            return mac_uninstall(&dir, MAC_LABEL);
        }
        let exe = std::env::current_exe()?;
        let app = app_bundle_of(&exe)
            .ok_or_else(|| anyhow::anyhow!("当前服务不在 AudioHub.app 里"))?;
        mac_install(&dir, MAC_LABEL, &app)
    }

    #[cfg(windows)]
    {
        // 同上：`schtasks /Delete` 不需要 App 在旁边。
        if !want {
            let out = schtasks(&win_schtasks_delete_argv(WIN_TASK))?;
            if !out.status.success() {
                bail!(
                    "schtasks /Delete 失败：{}",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            return Ok(());
        }
        let exe = std::env::current_exe()?;
        let app = app_exe_beside(&exe)
            .ok_or_else(|| anyhow::anyhow!("当前服务旁边没有 audiohub-app.exe"))?;
        let args = {
            let user = format!(
                "{}\\{}",
                std::env::var("USERDOMAIN").unwrap_or_default(),
                std::env::var("USERNAME").unwrap_or_default()
            );
            let xml_path = std::env::temp_dir().join("audiohub-autostart-task.xml");
            // UTF-16LE + BOM：`schtasks /XML` 只吃这一种编码，UTF-8 会被报成
            // 「任务 XML 包含意外节点」，而那句错误与真正的原因毫无关系。
            let mut bytes = vec![0xFF, 0xFE];
            for u in win_task_xml(&app, &user).encode_utf16() {
                bytes.extend_from_slice(&u.to_le_bytes());
            }
            std::fs::write(&xml_path, &bytes)?;
            win_schtasks_create_argv(WIN_TASK, &xml_path)
        };
        let out = schtasks(&args)?;
        if !out.status.success() {
            bail!(
                "schtasks {:?} 失败：{}",
                args,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }

    #[cfg(not(any(target_os = "macos", windows)))]
    {
        bail!("这个平台没有实现开机自启")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let p = std::env::temp_dir().join(format!("ahb-auto-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&p).expect("mkdir");
        p
    }

    /// **只有 `*.app/Contents/MacOS/x` 这一种三层结构算数。**
    ///
    /// 判据写松一格（例如「路径里含 `.app`」）的后果不是少一条登录项，而是多一
    /// 条**指向不存在目标**的登录项：用户打开了开关，界面显示已开启，下次登录
    /// 什么都不会发生，而没有任何一处会报错。
    #[test]
    fn only_a_real_bundle_layout_counts_as_installable() {
        assert_eq!(
            app_bundle_of(Path::new("/Applications/AudioHub.app/Contents/MacOS/audiohub")),
            Some(PathBuf::from("/Applications/AudioHub.app")),
        );
        // 构建树里的 bundle 同样算数——用户此刻跑的就是这一个。
        assert_eq!(
            app_bundle_of(Path::new(
                "/repo/app/src-tauri/target/release/bundle/macos/AudioHub.app/Contents/MacOS/audiohub"
            )),
            Some(PathBuf::from(
                "/repo/app/src-tauri/target/release/bundle/macos/AudioHub.app"
            )),
        );
        for bad in [
            // 测试二进制：这条就是「测试进程写不进用户登录项」的全部依据。
            "/repo/target/debug/deps/audiohubd-1234abcd",
            "/repo/target/release/audiohubd",
            // 路径里含 .app，但不是 bundle 布局。
            "/home/me/AudioHub.app/tmp/audiohubd",
            "/home/me/AudioHub.app/Contents/Helpers/audiohubd",
            // Contents 层名字不对。
            "/home/me/AudioHub.app/Resources/MacOS/audiohubd",
            // 没有扩展名。
            "/home/me/AudioHub/Contents/MacOS/audiohubd",
        ] {
            assert_eq!(app_bundle_of(Path::new(bad)), None, "{bad} 不该被当成可注册形态");
        }
    }

    /// Windows 的形态判据：daemon **旁边**得真的有 App 的可执行文件。
    ///
    /// 判据是 `is_file()` 而不是「拼得出这个路径」：拼路径永远成功，于是每一台
    /// 只装了 daemon 的机器都会被判成可注册，然后往计划任务里写一条指向不存在
    /// 文件的记录。
    #[test]
    fn the_windows_layout_needs_the_app_exe_next_door() {
        let dir = tmp("winlayout");
        let daemon = dir.join("audiohubd.exe");
        std::fs::write(&daemon, b"").expect("write");
        assert_eq!(app_exe_beside(&daemon), None, "旁边没有 App 却被判成可注册");
        let app = dir.join("audiohub-app.exe");
        std::fs::write(&app, b"").expect("write");
        assert_eq!(app_exe_beside(&daemon), Some(app), "旁边有 App 却没认出来");
        // 目录不算文件。
        std::fs::remove_file(dir.join("audiohub-app.exe")).expect("rm");
        std::fs::create_dir(dir.join("audiohub-app.exe")).expect("mkdir");
        assert_eq!(app_exe_beside(&daemon), None, "一个同名目录被当成了 App");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// plist 必须逐条含有那几个**承重**元素。
    ///
    /// 断言的是元素而不是整段文本：整段比对会在下次改一个空格时红，而漏掉
    /// `RunAtLoad` 会产出一个 launchd 收下、登录时什么都不做的 plist ——
    /// 那种失败在界面上与「已开启」完全一样。
    #[test]
    fn the_login_item_launches_the_app_and_runs_at_load() {
        let body = mac_plist(MAC_LABEL, Path::new("/Applications/AudioHub.app"));
        assert!(body.contains(&format!("<string>{MAC_LABEL}</string>")), "{body}");
        assert!(body.contains("<key>RunAtLoad</key>\n\t<true/>"), "没有 RunAtLoad：{body}");
        assert!(body.contains("<string>/usr/bin/open</string>"), "{body}");
        assert!(body.contains("<string>-g</string>"), "登录时会抢焦点：{body}");
        assert!(
            body.contains("<string>/Applications/AudioHub.app</string>"),
            "登录项没有指向 App bundle：{body}"
        );
        // 拉的是 App，**不是 daemon**（Windows 侧同一个模型）。
        assert!(
            !body.contains("audiohubd"),
            "登录项直接拉了 daemon —— 那样起来的机器没有任何界面入口：{body}"
        );
        // KeepAlive 写进去 = 用户点了「停止音频服务并退出」也会被 launchd 拽回来。
        assert!(!body.contains("KeepAlive"), "不许 KeepAlive：{body}");
    }

    /// 路径里的 XML 元字符必须被转义。
    ///
    /// 不转义产出的是一个 launchd **拒绝解析**的 plist，而拒绝解析的表现是
    /// 静默不启动：开关是开着的，登录时什么都不发生。
    #[test]
    fn a_path_with_xml_metacharacters_does_not_produce_a_broken_plist() {
        let body = mac_plist(MAC_LABEL, Path::new("/Users/a&b/<x>/AudioHub.app"));
        assert!(body.contains("/Users/a&amp;b/&lt;x&gt;/AudioHub.app"), "{body}");
        // 转义之后，除 DTD 那一行的 URL 外不该再有裸 `&`。
        for line in body.lines().filter(|l| !l.contains("DOCTYPE")) {
            for (i, _) in line.match_indices('&') {
                assert!(
                    line[i..].starts_with("&amp;")
                        || line[i..].starts_with("&lt;")
                        || line[i..].starts_with("&gt;")
                        || line[i..].starts_with("&quot;")
                        || line[i..].starts_with("&apos;"),
                    "裸 & 会让 launchd 拒绝解析：{line}"
                );
            }
        }
        assert_eq!(
            mac_plist_target(&body).as_deref(),
            Some("/Users/a&b/<x>/AudioHub.app"),
            "转义之后读不回原路径"
        );
    }

    /// 开→关→开都要真的落在文件系统上，而且**两个方向都测**。
    ///
    /// 只测 `true` 的话，一个「`enabled` 恒为 false」的实现照样能过（默认就是
    /// 关）；只测 `false` 同理。这与 `settings.rs` 里那条
    /// `the_discovery_switch_survives_the_file_both_ways` 是同一条纪律。
    #[test]
    fn the_login_item_round_trips_through_the_filesystem() {
        let dir = tmp("roundtrip");
        let app = PathBuf::from("/Applications/AudioHub.app");

        let off = mac_state(&dir, MAC_LABEL, &app);
        assert!(off.supported && !off.enabled, "空目录里不该有登录项：{off:?}");

        mac_install(&dir, MAC_LABEL, &app).expect("install");
        let on = mac_state(&dir, MAC_LABEL, &app);
        assert!(on.enabled, "装完之后读不回来：{on:?}");
        assert_eq!(on.target.as_deref(), Some("/Applications/AudioHub.app"));
        assert!(dir.join(format!("{MAC_LABEL}.plist")).is_file());
        // 临时文件不许留下：launchd 会读整个目录，一个半截 plist 会被它抱怨。
        assert!(!dir.join(format!("{MAC_LABEL}.plist.tmp")).exists());

        mac_uninstall(&dir, MAC_LABEL).expect("uninstall");
        assert!(!mac_state(&dir, MAC_LABEL, &app).enabled, "关掉之后文件还在");
        // 幂等：再关一次不许报错。
        mac_uninstall(&dir, MAC_LABEL).expect("second uninstall must be a no-op");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 登录项指着一个**已经被移走的旧 bundle** 时，报出来的必须是登录项里那一个。
    ///
    /// 报当前 bundle 会把这个场景显示成一切正常，而它恰恰是「开关是开的、登录
    /// 时却什么都不会发生」的唯一可见线索。
    #[test]
    fn a_stale_login_item_reports_the_path_it_actually_holds() {
        let dir = tmp("stale");
        mac_install(&dir, MAC_LABEL, Path::new("/old/AudioHub.app")).expect("install");
        let st = mac_state(&dir, MAC_LABEL, Path::new("/new/AudioHub.app"));
        assert!(st.enabled);
        assert_eq!(
            st.target.as_deref(),
            Some("/old/AudioHub.app"),
            "报的是当前 bundle，不是登录项里真写着的那个"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **一条活得比自己 bundle 长的登录项，必须照实报出来，并且关得掉。**
    ///
    /// 走到这一格的路径很普通：在装好的 `.app` 里打开自启（plist 落盘），之后从
    /// 开发构建里跑 daemon，或者把 bundle 挪走。此前这里返回的是
    /// `AutostartState::unsupported(...)`——一个硬编码 `enabled: false` 的构造，
    /// **根本没去 stat 那个 plist**。于是界面报「已关闭」、开关置灰，而系统每次
    /// 登录照样把 App 拉起来，且 `set()` 因 `!supported` 直接报错 ⇒ 用户没有任何
    /// 途径关掉它。plan §16.4 第 5 条禁止的正是这种「事实与显示相反」。
    #[test]
    fn a_login_item_outliving_its_bundle_is_reported_and_can_still_be_switched_off() {
        let dir = tmp("orphan");
        const WHY: &str = "当前服务不是从 AudioHub.app 里运行的";

        // 前提：这个形态确实注册不了——测试二进制就是这样一个形态。
        assert_eq!(app_bundle_of(Path::new("/repo/target/debug/audiohubd")), None);

        // 空目录：没装就是没装，两个量都是 false，理由照给。
        let none = mac_state_uninstallable(&dir, MAC_LABEL, WHY);
        assert!(!none.supported && !none.enabled, "{none:?}");
        assert_eq!(none.reason.as_deref(), Some(WHY));

        // 上一个形态写下的那条。
        mac_install(&dir, MAC_LABEL, Path::new("/Applications/AudioHub.app")).expect("install");
        let st = mac_state_uninstallable(&dir, MAC_LABEL, WHY);
        assert!(
            st.enabled,
            "登录项在盘上却被报成关着 —— 界面会显示「已关闭」，而每次登录仍会拉起 App：{st:?}"
        );
        assert!(!st.supported, "这个形态注册不了新的，这一点不该被 enabled 带偏：{st:?}");
        assert_eq!(
            st.target.as_deref(),
            Some("/Applications/AudioHub.app"),
            "报不出登录项指着谁，用户就看不出它已经过期：{st:?}"
        );
        assert!(st.reason.is_some(), "说不出为什么开不了新的");

        // 关得掉：策略允许，动作也真的落到文件系统上。
        assert_eq!(plan_set(false, &st), SetPlan::Apply, "关一条已注册的登录项被拒了");
        assert_eq!(plan_set(true, &st), SetPlan::AlreadyThere, "已经开着，再开一次不该动系统");
        mac_uninstall(&dir, MAC_LABEL).expect("uninstall");
        assert!(
            !mac_state_uninstallable(&dir, MAC_LABEL, WHY).enabled,
            "关完之后 plist 还在"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **注册要形态，撤销不要。**
    ///
    /// 这条不对称是 `plan_set` 存在的全部理由，所以它自己被逐格断言，而不是靠
    /// 「在某台碰巧是某个形态的机器上跑一遍」。四格里最要紧的是
    /// `(want=false, supported=false, enabled=true)`：两个方向共用一个
    /// `!supported` 闸门时，那一格就是一条用户关不掉的登录项。
    #[test]
    fn turning_autostart_off_is_never_refused_over_the_layout() {
        let st = |supported: bool, enabled: bool| AutostartState {
            supported,
            enabled,
            target: None,
            reason: None,
        };
        for (want, supported, enabled, expect, why) in [
            (false, false, true, SetPlan::Apply, "形态不合格 ⇒ 关不掉自己开的登录项"),
            (false, true, true, SetPlan::Apply, "正常的关"),
            (false, false, false, SetPlan::AlreadyThere, "本来就没有，不该去碰文件系统"),
            (false, true, false, SetPlan::AlreadyThere, "本来就没有"),
            (true, false, false, SetPlan::Refuse, "没有稳定启动目标却收下了「开」"),
            (true, true, false, SetPlan::Apply, "正常的开"),
            (true, true, true, SetPlan::AlreadyThere, "已经开着"),
            (true, false, true, SetPlan::AlreadyThere, "已经开着，形态不合格也无需再写一次"),
        ] {
            assert_eq!(
                plan_set(want, &st(supported, enabled)),
                expect,
                "want={want} supported={supported} enabled={enabled}：{why}"
            );
        }
    }

    /// Windows 的任务名必须与安装脚本逐字相同。
    ///
    /// 不同 = 同一台机器上两条登录项，用户在界面里关掉一条，另一条继续在登录时
    /// 拉起 App，而界面显示「已关闭」。
    #[test]
    fn the_task_name_matches_the_install_script() {
        let p = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../scripts/install-windows-autostart.ps1");
        let src = std::fs::read_to_string(&p)
            .unwrap_or_else(|e| panic!("读不到 {}：{e}", p.display()));
        assert!(
            src.contains(&format!("$TaskName = '{WIN_TASK}'")),
            "{} 里的任务名与 autostart::WIN_TASK（{WIN_TASK}）不一致",
            p.display()
        );
    }

    /// `schtasks` 的三条参数表：名字送到位、创建走 `/XML` 且带 `/F`。
    ///
    /// `/F` 缺席时，装过一次的机器（30-win 就是）再也更新不了那条任务，
    /// 而 `schtasks` 的报错是「任务已存在」——看上去像成功的近义词。
    #[test]
    fn the_scheduled_task_argv_carries_the_name_and_forces_an_overwrite() {
        let create = win_schtasks_create_argv(WIN_TASK, Path::new(r"C:\tmp\t.xml"));
        assert_eq!(create[0], "/Create");
        assert!(create.contains(&WIN_TASK.to_string()), "{create:?}");
        assert!(create.contains(&"/XML".to_string()), "没走 XML：{create:?}");
        assert!(create.contains(&r"C:\tmp\t.xml".to_string()), "{create:?}");
        assert!(create.contains(&"/F".to_string()), "没有 /F，装过的机器更新不了：{create:?}");

        let del = win_schtasks_delete_argv(WIN_TASK);
        assert_eq!(del[0], "/Delete");
        assert!(del.contains(&"/F".to_string()), "删除会停在一个交互确认上：{del:?}");

        let q = win_schtasks_query_argv(WIN_TASK);
        assert_eq!(q[0], "/Query");
        assert!(q.contains(&WIN_TASK.to_string()));
    }

    /// 任务 XML 拉的是 **App**，触发器是登录，且它带着安装脚本里那几条设置。
    #[test]
    fn the_scheduled_task_xml_launches_the_app_at_logon() {
        let xml = win_task_xml(Path::new(r"C:\Users\a\AudioHub\audiohub-app.exe"), r"PC\a");
        assert!(xml.contains("<LogonTrigger>"), "{xml}");
        assert!(xml.contains("<Command>C:\\Users\\a\\AudioHub\\audiohub-app.exe</Command>"), "{xml}");
        assert!(xml.contains("<UserId>PC\\a</UserId>"), "{xml}");
        // session 0 隔离拿不到音频端点，所以必须是交互式登录令牌（安装脚本决定 1）。
        assert!(xml.contains("<LogonType>InteractiveToken</LogonType>"), "{xml}");
        assert!(xml.contains("<RunLevel>LeastPrivilege</RunLevel>"), "{xml}");
        assert!(xml.contains("<Hidden>true</Hidden>"), "{xml}");
        assert!(xml.contains("<StartWhenAvailable>true</StartWhenAvailable>"), "{xml}");
        // 拉 App，不是 daemon —— 与 macOS 同一个模型。
        assert!(
            !xml.contains("audiohubd.exe"),
            "计划任务直接拉了 daemon —— 那样起来的机器没有托盘、没有任何界面入口：{xml}"
        );
    }

    /// `needle` 出现在至少一行**不是注释**的代码上。
    ///
    /// 存量的源码文本守卫（`mode_a_volume_tests::has`）是裸 `contains`，于是把开关
    /// 整行注释掉照样绿——`docs/status-audit.md` §2.7 逐条记了哪几处有这个洞。
    /// 这里按行判而不是写一个 JS 注释剥离器：JSX 里既有 `{/* */}` 也有字符串里的
    /// `//`（URL），逐行看「这一行是不是注释」既挡得住「被注释掉」这个真实失败，
    /// 又不会被一个 URL 骗到。
    fn contains_uncommented(src: &str, needle: &str) -> bool {
        src.lines().filter(|l| l.contains(needle)).any(|l| {
            let t = l.trim_start();
            !(t.starts_with("//") || t.starts_with("*") || t.starts_with("/*") || t.starts_with("{/*"))
        })
    }

    /// **界面上必须真的有这个开关，而且写的是契约表上那个键。**
    ///
    /// 这是 `SETTINGS_WRITABLE_KEYS` 的第三条腿（前两条：daemon 真的照做、命令行
    /// 够得到）。少了它，一个「daemon 实现完整、CLI 够得到、设置页里没有开关」的
    /// 提交会全绿通过——而 plan M9 的验收对象是**用户看得见的那个开关**。
    #[test]
    fn the_settings_page_really_has_the_autostart_switch() {
        let read = |rel: &str| {
            let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").join(rel);
            std::fs::read_to_string(&path).unwrap_or_else(|e| {
                panic!("读不到 {rel}（{e}）。文件被改名/挪走了就把这条测试一起更新，不要让它退化成恒真断言")
            })
        };
        let tsx = read("app/frontend/src/views/Settings.tsx");
        assert!(
            contains_uncommented(&tsx, "{ autostart: want }"),
            "设置页里没有一个开关把 'autostart' 写出去 —— plan M9 验收的正是这个开关"
        );

        // 三个字段的读取住在 `lib/autostart.ts`（那边有 vitest 单测在跑真逻辑，
        // 比这里的文本刮取强得多）。这条守卫跟着代码走，但**不放松**：少读任何
        // 一个字段，界面就会退回那个「点不动又说不出为什么」的形状。
        let lib = read("app/frontend/src/lib/autostart.ts");
        assert!(
            contains_uncommented(&tsx, "autostartView(ds)"),
            "设置页没有把 autostart 的当前值读回来：开关会一直停在它自己的初值上"
        );
        assert!(
            contains_uncommented(&lib, "ds.autostart"),
            "lib/autostart.ts 没读 autostart：开关会一直停在它自己的初值上"
        );
        // 置灰而说不出为什么，是这个项目反复付过代价的形状。
        assert!(
            contains_uncommented(&lib, "autostart_supported"),
            "没读 autostart_supported：不能注册登录项的机器上，开关会假装能点"
        );
        assert!(
            contains_uncommented(&lib, "autostart_reason"),
            "没读 autostart_reason：开关被置灰而界面答不出为什么"
        );
        // `supported` 与 `enabled` 塌缩成一个量，正是本轮修掉的那条缺陷的界面半边：
        // 一条已注册的登录项会变成用户关不掉的开关。
        assert!(
            contains_uncommented(&lib, "supported || on"),
            "开关的可用性又变回只看 supported：已注册但形态不合格时，用户关不掉它"
        );
    }

    /// **测试进程绝不能有能力写进用户的登录项。**
    ///
    /// 这条测试跑在 `target/…/deps/` 下的测试二进制里，两个平台的形态判据都不
    /// 满足，所以 `set(true)` 必须在碰任何系统状态之前就报错。它同时是本轮那条
    /// 「不得实际启用到用户机器上」的机械化守卫。
    ///
    /// ⚠ **`enabled` 不在断言范围内**：自从 `probe()` 改成如实读盘，这个字段报的
    /// 是**跑测试的这台机器**此刻有没有登录项——开发者自己开着自启是完全正当的，
    /// 断言它会把测试的绿/红绑到用户的个人设置上。这里要守的是能力，不是那台机器
    /// 的状态。
    ///
    /// ⚠ 同理，`set(false)` 只在探测结果说「没有东西可删」时才被执行。撤销路径
    /// 现在**故意**不受形态限制（见 `plan_set`），所以在一台真开着自启的开发机上
    /// 调它会删掉用户的登录项——`cargo test` 不许有这种副作用。
    #[test]
    fn a_test_binary_can_never_register_a_login_item() {
        let st = state();
        assert!(
            !st.supported,
            "测试二进制被判成了可注册形态（target={:?}）——再往下一步就会往用户的登录项里写东西",
            st.target
        );
        assert!(st.reason.is_some(), "置灰了却说不出理由");

        // 用户真实登录项的存在性，`set` 前后必须一模一样。
        let real = mac_agents_dir().map(|d| mac_plist_path(&d, MAC_LABEL));
        let before = real.as_ref().map(|p| p.exists());

        if st.enabled {
            // 这台机器真的开着自启：唯一安全的断言是「开的方向也不会去改写它」。
            let _ = set(true).map(|s| assert!(s.enabled));
        } else {
            let e = set(true).expect_err("形态不支持时 set(true) 必须报错，而不是静默收下");
            assert!(
                format!("{e:#}").contains("无法设置开机自启"),
                "错误信息说不清是什么挡住了：{e:#}"
            );
            // 关的方向不该被形态拒绝，而且在没有登录项时它必须只凭探测结果作答、
            // 一次文件系统写都不发生。
            let off = set(false).expect("关的方向不该因形态被拒");
            assert!(!off.enabled, "{off:?}");
        }

        assert_eq!(
            real.as_ref().map(|p| p.exists()),
            before,
            "跑一遍单元测试动了用户真实的 ~/Library/LaunchAgents"
        );
    }
}
