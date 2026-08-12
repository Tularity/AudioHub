//! plan M3 「同网段互见」: the wiring around mDNS announcing.
//!
//! The gap these cover is not "does mDNS work" — it did, and
//! `audiohub m3 announce` proved it by hand for months. The gap was that
//! **nothing in the shipping product ever set the flag**: the daemon defaulted
//! to not announcing, the app spawned it with no arguments, and so two machines
//! on one switch never saw each other. So what is asserted here is the decision
//! path — what a daemon started the way the app starts it decides to do, and
//! whether the user can change and keep that decision.
//!
//! **No test in this file may announce.** A test daemon that announced would
//! publish a service instance on the user's real LAN, under this machine's real
//! name, pointing at a port that disappears when the test ends — and the other
//! machines' scan lists would offer it as a peer. Every daemon here is either
//! pinned off or faulted; the one case that has to be a real announcement lives
//! in `regress/r2b_discovery.sh`, out of process and off the suite's path.

use std::path::PathBuf;

use serde_json::{json, Value};

use audiohub_ipc::methods;

use crate::halbridge::HalBridgeMode;
use crate::{ipcserv, start_daemon, DaemonCfg, DaemonHandle};

struct Node {
    h: DaemonHandle,
    dir: PathBuf,
    keep_dir: bool,
}

impl Drop for Node {
    fn drop(&mut self) {
        self.h.shutdown();
        if !self.keep_dir {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

fn tmpdir(tag: &str) -> PathBuf {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("ahb-m3-{tag}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir
}

impl Node {
    /// `announce` is passed through verbatim so each test can say which of the
    /// three states it is exercising; `announce_fault` keeps every one of them
    /// off the wire regardless.
    fn start_at(dir: PathBuf, announce: Option<bool>, fault: bool) -> Node {
        let h = start_daemon(DaemonCfg {
            control_port: 0, // ephemeral: the suite must not fight the real daemon
            ipc_port: 0,
            config_dir: Some(dir.clone()),
            announce,
            // Never `auto`: a test daemon that attaches evicts the user's real
            // one, and the two then oscillate.
            hal_bridge: Some(HalBridgeMode::Off),
            tx_throttle_kbps: None,
            block_udp: None,
            announce_fault: fault,
            airplay_advertise: false,
        })
        .expect("start daemon");
        Node { h, dir, keep_dir: false,
        }
    }

    fn ok(&self, method: &str, params: Value) -> Value {
        ipcserv::dispatch_for_test(self.h.inner_for_test(), method, &params)
            .unwrap_or_else(|e| panic!("{method} failed: {e}"))
    }

    fn settings(&self) -> Value {
        self.ok(methods::SETTINGS_GET, json!({}))
    }

    fn flag(&self, key: &str) -> bool {
        self.settings()
            .get(key)
            .and_then(Value::as_bool)
            .unwrap_or_else(|| panic!("settings.get 没有 '{key}' 这个布尔字段"))
    }
}

fn as_bool(v: &Value, key: &str) -> bool {
    v.get(key)
        .and_then(Value::as_bool)
        .unwrap_or_else(|| panic!("回包里没有 '{key}' 这个布尔字段：{v}"))
}

/// **A daemon started the way the app starts it wants to be announced.**
///
/// `announce: None` is not an incidental choice in this test — it is literally
/// what `audiohubd` with no arguments passes, which is what
/// `app/src-tauri/src/main.rs` spawns and what both autostart entries run. The
/// old default was "do not announce", and that single value is the whole of
/// 🟠4: every other piece of discovery was built, tested and working.
#[test]
fn a_daemon_started_the_way_the_app_starts_it_asks_to_be_announced() {
    let n = Node::start_at(tmpdir("factory"), None, /*fault=*/ true);
    assert!(
        n.flag("discovery_announce"),
        "出厂形态（无参数启动、配置目录里还没有 settings.json）不广播自己 —— \
         同网段的两台机器于是永远互相看不见，M3 的验收标准落空"
    );
}

/// **`discovery_announcing` is read off the live announcement, not copied from
/// the wish.**
///
/// This is the pair of values that keeps the interface honest on the one
/// machine where it matters: multicast blocked, or macOS's local-network
/// permission not granted yet. With real mDNS working the two agree, so a copy
/// and a reading are indistinguishable — `announce_fault` is what tells them
/// apart.
///
/// And the daemon must still be **up**: announcing is a convenience layered on
/// a daemon that works without it, so a failure to announce cannot be allowed
/// to fail startup. Every other call in this test is evidence of that; the
/// `start_daemon` inside `Node::start_at` would have panicked otherwise.
#[test]
fn an_announce_that_fails_leaves_the_daemon_running_and_says_so() {
    let n = Node::start_at(tmpdir("fault"), Some(true), /*fault=*/ true);
    let s = n.settings();
    assert!(
        as_bool(&s, "discovery_announce"),
        "被要求广播，wish 却没有如实报出来"
    );
    assert!(
        !as_bool(&s, "discovery_announcing"),
        "广播根本没有建立起来，daemon 却报告自己正在广播 —— \
         界面会告诉用户「别人能看见这台机器」，而没有人能"
    );
    // Not a corpse: the daemon answers other methods too. (`start_daemon`
    // returning at all is the first half of the same claim — it would have
    // panicked inside `Node::start_at`.)
    let peers = n.ok(methods::PEERS_LIST, json!({}));
    assert!(
        peers.is_array(),
        "广播失败把 daemon 一起带走了：一台组播被封的机器（或一次尚未授权的 \
         macOS 首次运行）会变成「App 根本起不来」"
    );
}

/// **The privacy switch works, persists, and a restart honours it.**
///
/// Three claims in one test because they only mean anything together: a switch
/// that takes effect but is forgotten, or is remembered but not read at
/// startup, is a switch that turns itself back on. The restart is the half that
/// covers `start_daemon` reading the setting at all — which is exactly the
/// wiring 🟠4 was missing, in the other direction.
#[test]
fn turning_announcing_off_persists_and_the_next_start_honours_it() {
    let dir = tmpdir("privacy");
    {
        let mut n = Node::start_at(dir.clone(), None, /*fault=*/ true);
        n.keep_dir = true; // the second daemon reads this same config dir
        assert!(n.flag("discovery_announce"), "起点必须是「想广播」");
        let reply = n.ok(
            methods::SETTINGS_SET,
            json!({ "discovery_announce": false }),
        );
        assert!(
            !as_bool(&reply, "discovery_announce"),
            "settings.set 收下了关闭请求却没有照做"
        );
        assert!(!as_bool(&reply, "discovery_announcing"));
        // Independent read, so a reply that merely echoes the request cannot
        // pass.
        assert!(!n.flag("discovery_announce"), "读回来又变成开着的了");
    }
    // On disk, and the file is what the next daemon reads.
    let body = std::fs::read_to_string(dir.join("settings.json")).expect("settings.json");
    assert!(
        body.contains("\"discovery_announce\": false"),
        "隐私开关没有落盘：{body}"
    );
    {
        // `None` again — the app's own startup shape, on a config dir that says
        // no. A daemon that ignored the setting here would put the machine back
        // on the network the user took it off.
        let n2 = Node::start_at(dir.clone(), None, /*fault=*/ false);
        assert!(
            !n2.flag("discovery_announce"),
            "重启后隐私开关自己弹回去了"
        );
        assert!(
            !n2.flag("discovery_announcing"),
            "用户关掉了广播，重启之后这台机器又开始广播自己"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// **A startup override does not rewrite the stored preference.**
///
/// `--announce` / `--no-announce` are for this run. Letting either of them
/// write settings.json would mean one `ctl daemon --no-announce` (or one probe
/// script) silently, permanently changing a choice the user made in the
/// interface — the one thing this project's settings are not allowed to do.
///
/// Deterministic on any machine: it asserts what the *stored wish* is, which no
/// amount of working or broken multicast can move.
#[test]
fn a_startup_override_does_not_rewrite_the_stored_preference() {
    let dir = tmpdir("override");
    {
        // Establish an explicit stored `true` (rather than an absent key) so a
        // failure cannot be explained away as "it just read the default".
        let n = Node::start_at(dir.clone(), Some(false), /*fault=*/ true);
        n.ok(methods::SETTINGS_SET, json!({ "discovery_announce": true }));
    }
    {
        let mut n = Node::start_at(dir.clone(), Some(false), /*fault=*/ true);
        n.keep_dir = true;
        assert!(
            n.flag("discovery_announce"),
            "--no-announce 把用户存下来的偏好也改掉了：下一次不带这个参数启动，\
             机器就再也不广播了，而用户从未做过这个决定"
        );
        assert!(
            !n.flag("discovery_announcing"),
            "--no-announce 说的是「这一次不要广播」，它没有被执行"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// `discover.run` must answer, and must not answer with **this machine**.
///
/// The self-filter had no observable behaviour while nobody announced; now that
/// every daemon does, a daemon whose config dir is not the ambient one used to
/// list itself as a discoverable peer (see `discovery::browse`). Kept short
/// because it is a live network scan: what it proves is that the call is wired
/// and that our own fingerprint is never in the result.
#[test]
fn a_scan_never_returns_this_machine() {
    let n = Node::start_at(tmpdir("selfscan"), Some(false), /*fault=*/ true);
    let my_fp = n.h.inner_for_test().identity().fingerprint.clone();
    let found = n.ok(methods::DISCOVER_RUN, json!({ "secs": 0.5 }));
    let list = found.as_array().expect("discover.run 应当返回一个数组");
    for p in list {
        assert_ne!(
            p.get("fingerprint").and_then(Value::as_str),
            Some(my_fp.as_str()),
            "扫描结果里出现了本机自己：{p}"
        );
    }
}
