//! First-run permission gate model (用户指令：macOS App 首屏先把权限授权完再进主界面).
//!
//! Everything here is split by ONE property — whether it can raise a system
//! consent dialog:
//!
//!   * `probe_all` / `probe_one` are pure queries. The gate page paints from
//!     them and may poll them; they must never prompt, on any platform.
//!   * `request` is the ONLY function that deliberately prompts. Nothing else
//!     in this crate may call it, and it is only ever reached from
//!     `daemon.request_permission`, i.e. from a user clicking a button.
//!
//! macOS offers a preflight for exactly one of the three permissions we need
//! (the microphone). System-audio recording and local network have no public
//! query API at all, so `granted: None` is the honest answer until an attempt
//! teaches us otherwise. What an attempt teaches is remembered in process
//! memory only and dies with the daemon, deliberately: the user can revoke in
//! System Settings at any time, and a stale "granted" persisted to disk would
//! be worse than an honest "unknown".
//!
//! A permission with no usage string in the bundle's Info.plist is not
//! prompted, it is denied outright — so the app bundle must declare
//! NSMicrophoneUsageDescription, NSLocalNetworkUsageDescription and (for the
//! Core Audio process tap) NSAudioCaptureUsageDescription. That is the app
//! target's business, not this module's, but it is the reason the states below
//! can come back denied without the user ever seeing a dialog.

use std::sync::Mutex;
use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// Wire values of `PermissionState::kind` (the UI switches on these).
pub const KIND_MICROPHONE: &str = "microphone";
pub const KIND_LOCAL_NETWORK: &str = "local_network";
pub const KIND_SYSTEM_AUDIO: &str = "system_audio";

/// How long `request` waits for the user to answer the microphone dialog
/// before giving up and letting the caller re-poll. Bounded because it blocks
/// one IPC connection: the UI is expected to keep polling `daemon.permissions`
/// (which never prompts) afterwards.
#[cfg(target_os = "macos")]
const MIC_PROMPT_WAIT: Duration = Duration::from_secs(20);
#[cfg(target_os = "macos")]
const POLL: Duration = Duration::from_millis(100);
/// Long enough for a granted tap to run its IOProc at least once, short enough
/// that a UI click does not feel stuck.
#[cfg(target_os = "macos")]
const TAP_HOLD: Duration = Duration::from_millis(200);
/// One-shot mDNS query + answer window.
const LOCAL_NET_WAIT: Duration = Duration::from_millis(1200);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionKind {
    Microphone,
    LocalNetwork,
    SystemAudio,
}

impl PermissionKind {
    /// Gate order: what the UI shows top to bottom.
    pub const ALL: [PermissionKind; 3] = [
        PermissionKind::Microphone,
        PermissionKind::LocalNetwork,
        PermissionKind::SystemAudio,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            PermissionKind::Microphone => KIND_MICROPHONE,
            PermissionKind::LocalNetwork => KIND_LOCAL_NETWORK,
            PermissionKind::SystemAudio => KIND_SYSTEM_AUDIO,
        }
    }

    pub fn parse(s: &str) -> Option<PermissionKind> {
        PermissionKind::ALL.iter().copied().find(|k| k.as_str() == s)
    }
}

/// One row of the gate page.
///
/// `granted`:
///   - `Some(true)`  — confirmed granted
///   - `Some(false)` — confirmed denied/restricted; only System Settings can
///                     undo it, asking again does nothing
///   - `None`        — unknown. On macOS this is the NORMAL steady state for
///                     local network and system audio: no query API exists.
///                     The UI must treat it as "尚未确认，点一下去询问", never
///                     as a permanent block, or the gate can never be passed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionState {
    pub kind: String,
    pub granted: Option<bool>,
    /// Core functionality depends on it. `false` = feature-scoped, the gate
    /// may let the user in without it.
    pub required: bool,
    /// Why AudioHub needs it, in the user's language.
    pub why: String,
    pub settings_url: Option<String>,
    /// What is known right now, including how to recover from a denial.
    pub note: String,
}

/// Status of every permission, without prompting for any of them.
pub fn probe_all() -> Vec<PermissionState> {
    PermissionKind::ALL.iter().copied().map(probe_one).collect()
}

/// Status of one permission, without prompting.
pub fn probe_one(kind: PermissionKind) -> PermissionState {
    let (granted, note) = probe_impl(kind);
    PermissionState {
        kind: kind.as_str().to_string(),
        granted,
        required: required(kind),
        why: why(kind).to_string(),
        settings_url: settings_url(kind),
        note,
    }
}

/// The system-audio tap only backs 模式 A（系统音频捕获）; the other two are
/// load-bearing for every mode, so the gate insists on them.
fn required(kind: PermissionKind) -> bool {
    !matches!(kind, PermissionKind::SystemAudio)
}

fn why(kind: PermissionKind) -> &'static str {
    match kind {
        PermissionKind::Microphone => "把本机麦克风共享给对端主机时，需要采集麦克风声音。",
        PermissionKind::LocalNetwork => "在局域网内发现其它主机、并与对端建立音频连接。",
        PermissionKind::SystemAudio => {
            "「模式 A（免驱动·系统捕获）」把本机正在播放的声音送到对端输出时，需要录制系统音频。"
        }
    }
}

// ------------------------------------------------------------ platform probe

#[cfg(target_os = "macos")]
fn settings_url(kind: PermissionKind) -> Option<String> {
    // Anchor names verified against the pane that actually consumes them on
    // this machine (macOS 26.5.2 / 25F84):
    //   strings /System/Library/ExtensionKit/Extensions/SecurityPrivacyExtension.appex\
    //           /Contents/MacOS/SecurityPrivacyExtension | grep '^Privacy_'
    // `Privacy_Microphone` and `Privacy_AudioCapture` (kTCCServiceAudioCapture,
    // the "屏幕录制与系统录音" section) are both in that table.
    // `Privacy_LocalNetwork` is NOT — the local-network pane only shows up as
    // the node id `privacy-localnetwork`. It is kept here because it is the
    // documented spelling and an unknown anchor degrades to the Privacy &
    // Security root, which is one click away from the right row; swap it for
    // "com.apple.settings.PrivacySecurity.extension?privacy-localnetwork" if
    // a user-present check shows the root is where it lands.
    const BASE: &str = "x-apple.systempreferences:com.apple.preference.security?";
    let anchor = match kind {
        PermissionKind::Microphone => "Privacy_Microphone",
        PermissionKind::LocalNetwork => "Privacy_LocalNetwork",
        PermissionKind::SystemAudio => "Privacy_AudioCapture",
    };
    Some(format!("{BASE}{anchor}"))
}

#[cfg(not(target_os = "macos"))]
fn settings_url(_kind: PermissionKind) -> Option<String> {
    None
}

#[cfg(not(target_os = "macos"))]
fn probe_impl(_kind: PermissionKind) -> (Option<bool>, String) {
    (Some(true), "本平台无需授权".to_string())
}

#[cfg(target_os = "macos")]
fn probe_impl(kind: PermissionKind) -> (Option<bool>, String) {
    match kind {
        PermissionKind::Microphone => mac::mic_status().report(),
        PermissionKind::LocalNetwork => (
            localnet::seen().then_some(true),
            if localnet::seen() {
                "本次运行已收到局域网内的 mDNS 响应，权限可用。".to_string()
            } else {
                concat!(
                    "macOS 没有提供查询接口，状态无法预先确认：首次访问局域网时系统会询问。",
                    "点击授权会发一次 mDNS 查询来触发弹窗；若此前已被拒绝，系统不会再弹，",
                    "需要到「系统设置 > 隐私与安全性 > 本地网络」手动开启。"
                )
                .to_string()
            },
        ),
        PermissionKind::SystemAudio => sysaudio_consent(),
    }
}

/// TCC has no preflight for system-audio recording, and the call that would
/// answer the question (creating the process tap) IS the prompt. The only
/// honest source is the memo `sysaudio` keeps from its last `start_backend`,
/// which it publishes through the mac-catap backend note.
#[cfg(target_os = "macos")]
fn sysaudio_consent() -> (Option<bool>, String) {
    let info = match crate::sysaudio::resolve_backend(crate::sysaudio::BACKEND_MAC_CATAP) {
        Ok(b) => b,
        Err(e) => return (None, format!("无法查询系统音频后端：{e:#}")),
    };
    if !info.available {
        return (None, format!("本机不支持系统音频捕获：{}", info.note));
    }
    match classify_catap_note(&info.note) {
        Some(Some(true)) => (Some(true), "已授权系统音频录制。".to_string()),
        Some(Some(false)) => (
            Some(false),
            concat!(
                "上一次尝试被拒绝：请到「系统设置 > 隐私与安全性 > 屏幕录制与系统录音」",
                "勾选本 App 后重试。"
            )
            .to_string(),
        ),
        _ => (
            None,
            concat!(
                "macOS 没有提供查询接口，状态无法预先确认：首次创建音频进程 Tap 时系统会询问。",
                "点击授权会真的去建一次 Tap（随即关闭）来触发弹窗。"
            )
            .to_string(),
        ),
    }
}

/// `sysaudio::mac::CONSENT` is private to that module, which is not ours to
/// widen, so its note is the public surface we have. Classify it explicitly:
/// an unrecognised note means the wording changed, and the macOS test below
/// fails loudly instead of this quietly reporting "unknown" forever.
///
/// `Some(x)` = recognised, carrying the tri-state; `None` = unrecognised.
#[cfg(target_os = "macos")]
fn classify_catap_note(note: &str) -> Option<Option<bool>> {
    if note.contains("(consent granted)") {
        Some(Some(true))
    } else if note.contains("the last attempt was refused") {
        Some(Some(false))
    } else if note.contains("asks for system-audio-recording consent") {
        Some(None)
    } else {
        None
    }
}

// ------------------------------------------------------------------- request

/// Deliberately raise the OS consent dialog for `kind`.
///
/// THE ONLY PROMPTING PATH IN THE CRATE. Blocks: the microphone case waits up
/// to `MIC_PROMPT_WAIT` for the user's answer so the caller can report a
/// settled state, and the system-audio case blocks inside Core Audio for as
/// long as the dialog is up. Callers should re-`probe_one` afterwards rather
/// than trust `Ok(())` to mean "granted" — a timeout is also `Ok`.
///
/// `Err` means the attempt itself failed (device missing, already denied so
/// macOS will not ask again, tap refused). It is user-facing text.
pub fn request(kind: PermissionKind) -> Result<()> {
    // Two clicks in the UI must not open two capture streams / two taps.
    static LOCK: Mutex<()> = Mutex::new(());
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    match kind {
        PermissionKind::Microphone => request_microphone(),
        PermissionKind::SystemAudio => request_system_audio(),
        PermissionKind::LocalNetwork => localnet::browse_once(LOCAL_NET_WAIT).map(|_| ()),
    }
}

/// Opening the default input is what raises the microphone dialog — the same
/// call a real mic session makes (spec-m0 §2: "macOS 首次调用会触发 TCC 弹窗").
/// AVCaptureDevice.requestAccessForMediaType:completionHandler: would be the
/// canonical path, but it takes an Objective-C block and `block2` is not in the
/// dependency whitelist (spec-m0 §0); polling the (non-prompting)
/// authorizationStatus around the open gets the same settled answer without
/// hand-rolling the block ABI.
#[cfg(target_os = "macos")]
fn request_microphone() -> Result<()> {
    use mac::MicStatus;
    use std::time::Instant;

    match mac::mic_status() {
        MicStatus::Authorized => return Ok(()),
        MicStatus::Denied | MicStatus::Restricted => anyhow::bail!(
            "麦克风权限此前已被拒绝，macOS 不会再次弹窗：请到「系统设置 > 隐私与安全性 > 麦克风」勾选本 App 后重试"
        ),
        MicStatus::NotDetermined | MicStatus::Unavailable => {}
    }

    let opened = crate::audio::LiveCapture::start();
    let open_err = opened.as_ref().err().map(|e| format!("{e:#}"));
    // Wait for the answer whether or not the stream opened: cpal can fail to
    // read a microphone it is not yet allowed to touch while the dialog is up.
    let deadline = Instant::now() + MIC_PROMPT_WAIT;
    let settled = loop {
        let s = mac::mic_status();
        if !matches!(s, MicStatus::NotDetermined) || Instant::now() >= deadline {
            break s;
        }
        std::thread::sleep(POLL);
    };
    drop(opened); // stop capturing the moment we have an answer

    match settled {
        MicStatus::Authorized => Ok(()),
        MicStatus::Denied | MicStatus::Restricted => {
            anyhow::bail!("用户拒绝了麦克风权限；可在「系统设置 > 隐私与安全性 > 麦克风」中重新开启")
        }
        // Still unanswered (or AVFoundation unavailable): only a failure to
        // open is worth reporting as an error — the caller re-probes anyway.
        _ => match open_err {
            Some(e) => anyhow::bail!("打开默认输入设备失败：{e}"),
            None => Ok(()),
        },
    }
}

#[cfg(not(target_os = "macos"))]
fn request_microphone() -> Result<()> {
    let cap = crate::audio::LiveCapture::start()?;
    drop(cap);
    Ok(())
}

/// Creating the process tap IS the prompt (spec-round2 §A1). Held briefly so a
/// granted tap is proven live rather than merely created, then torn down: this
/// must not leave a capture running behind the gate page.
#[cfg(target_os = "macos")]
fn request_system_audio() -> Result<()> {
    let cap = crate::sysaudio::start_backend(crate::sysaudio::BACKEND_MAC_CATAP)?;
    std::thread::sleep(TAP_HOLD);
    drop(cap);
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn request_system_audio() -> Result<()> {
    Ok(())
}

// ------------------------------------------------------------- local network

mod localnet {
    use std::io::ErrorKind;
    use std::net::{Ipv4Addr, UdpSocket};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use anyhow::Result;

    const GROUP: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
    const PORT: u16 = 5353;

    /// Set once an mDNS answer comes back. Positive evidence only: silence
    /// proves nothing (an empty LAN looks exactly like a denial, because macOS
    /// drops the traffic without an error).
    static SEEN: AtomicBool = AtomicBool::new(false);

    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn seen() -> bool {
        SEEN.load(Ordering::Relaxed)
    }

    fn encode_name(labels: &[&str], out: &mut Vec<u8>) {
        for l in labels {
            out.push(l.len() as u8);
            out.extend_from_slice(l.as_bytes());
        }
        out.push(0);
    }

    /// A one-shot multicast DNS query (RFC 6762 §5.4) for the service
    /// enumeration PTR, so anything Bonjour on the LAN is a candidate answer.
    ///
    /// The QU bit (top bit of QCLASS) is what makes this usable from an
    /// ephemeral port: without it responders multicast their answers to
    /// 224.0.0.251:5353, which only a socket bound to 5353 — mDNSResponder's —
    /// would ever see.
    pub fn query_packet() -> Vec<u8> {
        let mut p = Vec::with_capacity(46);
        p.extend_from_slice(&[0, 0]); // id: mDNS ignores it
        p.extend_from_slice(&[0, 0]); // flags: standard query
        p.extend_from_slice(&[0, 1]); // qdcount
        p.extend_from_slice(&[0, 0, 0, 0, 0, 0]); // ancount/nscount/arcount
        encode_name(&["_services", "_dns-sd", "_udp", "local"], &mut p);
        p.extend_from_slice(&[0, 12]); // QTYPE = PTR
        p.extend_from_slice(&[0x80, 0x01]); // QU | QCLASS = IN
        p
    }

    /// Sends one query and waits `total` for any answer.
    ///
    /// PROMPTS: on macOS this is the local-network access that raises the
    /// "允许查找本地网络设备" dialog. Reached only from `request`.
    ///
    /// `Ok(false)` is not a denial — a denied app still gets `Ok(())` from
    /// `send_to`, macOS just discards the packet, and a LAN with nothing to
    /// answer looks identical.
    pub fn browse_once(total: Duration) -> Result<bool> {
        let sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
        sock.set_read_timeout(Some(Duration::from_millis(200)))?;
        let _ = sock.set_multicast_loop_v4(true);
        // Best effort: joining is itself local-network activity, and it lets
        // us catch a multicast answer if we are handed one.
        let _ = sock.join_multicast_v4(&GROUP, &Ipv4Addr::UNSPECIFIED);
        sock.send_to(&query_packet(), (GROUP, PORT))?;

        let deadline = Instant::now() + total;
        let mut buf = [0u8; 1500];
        while Instant::now() < deadline {
            match sock.recv_from(&mut buf) {
                Ok((n, _)) if n > 0 => {
                    SEEN.store(true, Ordering::Relaxed);
                    return Ok(true);
                }
                Ok(_) => {}
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(false)
    }
}

// -------------------------------------------------------------- macos bridge

#[cfg(target_os = "macos")]
mod mac {
    use std::ffi::{c_char, c_int, c_void};
    use std::sync::OnceLock;

    use objc2::msg_send;
    use objc2::rc::Retained;
    use objc2::runtime::AnyClass;
    use objc2_foundation::NSString;

    /// AVAuthorizationStatus (AVCaptureDevice.h).
    const NOT_DETERMINED: isize = 0;
    const RESTRICTED: isize = 1;
    const DENIED: isize = 2;
    const AUTHORIZED: isize = 3;

    #[derive(Clone, Copy, PartialEq, Eq)]
    pub enum MicStatus {
        NotDetermined,
        Authorized,
        Denied,
        Restricted,
        /// AVFoundation would not load, or the class is gone. Not a denial.
        Unavailable,
    }

    impl MicStatus {
        pub fn report(self) -> (Option<bool>, String) {
            match self {
                MicStatus::Authorized => (Some(true), "已授权。".to_string()),
                MicStatus::Denied => (
                    Some(false),
                    concat!(
                        "已被拒绝：macOS 不会再次弹窗，请到「系统设置 > 隐私与安全性 > 麦克风」",
                        "勾选本 App 后重试。"
                    )
                    .to_string(),
                ),
                MicStatus::Restricted => (
                    Some(false),
                    "受系统策略限制（描述文件 / 屏幕使用时间等），本机无法授予麦克风权限。"
                        .to_string(),
                ),
                MicStatus::NotDetermined => (
                    None,
                    "尚未询问：点击授权会弹出系统对话框。".to_string(),
                ),
                MicStatus::Unavailable => (
                    None,
                    "无法加载 AVFoundation，当前状态未知；首次采集时系统会询问。".to_string(),
                ),
            }
        }
    }

    extern "C" {
        fn dlopen(path: *const c_char, mode: c_int) -> *mut c_void;
        fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    }
    const RTLD_LAZY: c_int = 1;

    /// Nothing else in the tree links AVFoundation (cpal brings in
    /// AudioToolbox/CoreAudio only), so `AVCaptureDevice` is not registered
    /// with the ObjC runtime until the framework is loaded. dlopen keeps it off
    /// the daemon's load commands until someone actually asks about
    /// permissions; the handle is cached, and loading a framework prompts for
    /// nothing.
    ///
    /// The handle is kept as `usize` because a raw pointer is not `Sync`; it is
    /// never freed (dlclose on a system framework is pointless and unsafe).
    fn av_foundation() -> Option<usize> {
        static HANDLE: OnceLock<Option<usize>> = OnceLock::new();
        *HANDLE.get_or_init(|| {
            let path = c"/System/Library/Frameworks/AVFoundation.framework/AVFoundation";
            let h = unsafe { dlopen(path.as_ptr(), RTLD_LAZY) };
            (!h.is_null()).then_some(h as usize)
        })
    }

    /// `NSString * const AVMediaTypeAudio` — dlsym hands back the address of
    /// the variable, not of the string. Falls back to its documented value.
    fn media_type_audio() -> Retained<NSString> {
        if let Some(h) = av_foundation() {
            let sym = unsafe { dlsym(h as *mut c_void, c"AVMediaTypeAudio".as_ptr()) };
            if !sym.is_null() {
                let s = unsafe { *(sym as *const *const NSString) };
                if let Some(r) = unsafe { Retained::retain(s as *mut NSString) } {
                    return r;
                }
            }
        }
        NSString::from_str("soun")
    }

    /// `+[AVCaptureDevice authorizationStatusForMediaType:]`.
    ///
    /// Apple documents this as a pure read of the TCC decision: it never
    /// prompts, which is exactly why the gate page can poll it.
    pub fn mic_status() -> MicStatus {
        if av_foundation().is_none() {
            return MicStatus::Unavailable;
        }
        let Some(cls) = AnyClass::get(c"AVCaptureDevice") else {
            return MicStatus::Unavailable;
        };
        let media = media_type_audio();
        let raw: isize = unsafe { msg_send![cls, authorizationStatusForMediaType: &*media] };
        match raw {
            AUTHORIZED => MicStatus::Authorized,
            DENIED => MicStatus::Denied,
            RESTRICTED => MicStatus::Restricted,
            NOT_DETERMINED => MicStatus::NotDetermined,
            _ => MicStatus::Unavailable, // a case this OS grew and we do not know
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_strings_round_trip() {
        for k in PermissionKind::ALL {
            assert_eq!(PermissionKind::parse(k.as_str()), Some(k));
        }
        assert_eq!(PermissionKind::parse("mic"), None);
        assert_eq!(PermissionKind::parse(""), None);
    }

    /// The contract the UI paints from. Must never prompt — everything it
    /// touches is a class lookup, an atomic, or a cached note.
    #[test]
    fn probe_all_covers_every_kind_without_prompting() {
        let states = probe_all();
        assert_eq!(states.len(), PermissionKind::ALL.len());
        for (state, kind) in states.iter().zip(PermissionKind::ALL) {
            assert_eq!(state.kind, kind.as_str());
            assert!(!state.why.is_empty(), "{} has no why", state.kind);
            assert!(!state.note.is_empty(), "{} has no note", state.kind);
        }
        assert!(states[0].required && states[1].required);
        // System audio can never be confirmed ahead of time, so gating entry on
        // it would deadlock the gate page.
        assert!(!states[2].required);
        // Stable across calls: probing has no side effect.
        let again = probe_all();
        for (a, b) in states.iter().zip(&again) {
            assert_eq!(a.kind, b.kind);
            assert_eq!(a.granted, b.granted);
        }
    }

    #[test]
    fn mdns_query_is_a_qu_ptr_for_service_enumeration() {
        let p = localnet::query_packet();
        assert_eq!(&p[4..6], &[0, 1], "one question");
        assert_eq!(&p[6..12], &[0, 0, 0, 0, 0, 0], "no answer sections");
        let name = &p[12..p.len() - 4];
        assert_eq!(
            name,
            b"\x09_services\x07_dns-sd\x04_udp\x05local\x00",
            "service enumeration PTR name"
        );
        assert_eq!(&p[p.len() - 4..], &[0, 12, 0x80, 0x01], "PTR + QU|IN");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn every_kind_has_a_settings_deep_link() {
        for k in PermissionKind::ALL {
            let url = settings_url(k).expect("macOS deep link");
            assert!(url.starts_with("x-apple.systempreferences:"), "{url}");
        }
    }

    /// Guards the string coupling to `sysaudio`'s consent memo: if that module
    /// rewords its note, this fails instead of the gate silently reporting
    /// "unknown" forever.
    #[cfg(target_os = "macos")]
    #[test]
    fn catap_note_classification_tracks_sysaudio() {
        assert_eq!(
            classify_catap_note("Core Audio process tap, excluding this process (consent granted)"),
            Some(Some(true))
        );
        assert_eq!(
            classify_catap_note("Core Audio process tap; the last attempt was refused — allow …"),
            Some(Some(false))
        );
        assert_eq!(classify_catap_note("something else entirely"), None);

        let info = crate::sysaudio::resolve_backend(crate::sysaudio::BACKEND_MAC_CATAP).unwrap();
        if info.available {
            assert!(
                classify_catap_note(&info.note).is_some(),
                "unrecognised mac-catap note: {:?}",
                info.note
            );
        }
    }
}
