//! plan §13 三模式互斥的**接线**测试。
//!
//! 不是枚举转换：两台真 daemon 在回环上跑完握手、配对、模式切换，然后断言产品
//! 代码真的拒绝 / 真的通告 / 真的不再要设备。
//!
//! 为什么非要起真 daemon：这一整套规格的失败形态全都是「函数写对了但没人调用
//! 它」。`refuse_being_used` 的单元测试对着一个从未被 `handle_remote_open` 调到的
//! 函数照样全绿——本项目已经为这一类绿灯付过四次学费（progress.md），最近一次是
//! 「渲染名里含扬声器」那条恒真断言。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use audiohub_ipc::{methods, Mode, KIND_SPK, SOURCE_TONE};

use crate::halbridge::HalBridgeMode;
use crate::{ipcserv, start_daemon, DaemonCfg, DaemonHandle};

struct Node {
    h: DaemonHandle,
    dir: PathBuf,
}

impl Drop for Node {
    fn drop(&mut self) {
        self.h.shutdown();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Node {
    fn start(tag: &str) -> Node {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("ahb-s13-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let h = start_daemon(DaemonCfg {
            control_port: 0, // ephemeral: the suite must not fight the real daemon
            ipc_port: 0,
            config_dir: Some(dir.clone()),
            announce: false,
            // Never `auto`: a test daemon that attaches evicts the user's real
            // one, and the two then oscillate (progress.md 2026-08-03 — the
            // root cause of a 200-minute underrun investigation).
            hal_bridge: Some(HalBridgeMode::Off),
            // Production and every test but the tier 2 starvation rig: whatever
            // the environment says (normally nothing, i.e. unlimited).
            tx_throttle_kbps: None,
        })
        .expect("start daemon");
        Node { h, dir }
    }

    /// Drives the SAME dispatcher a real client reaches. Calling
    /// `conn::open_session` by hand would skip the mode gate that lives in the
    /// dispatcher and pass while the product refused nothing.
    fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        ipcserv::dispatch_for_test(self.h.inner_for_test(), method, &params)
    }

    fn ok(&self, method: &str, params: Value) -> Value {
        self.call(method, params)
            .unwrap_or_else(|e| panic!("{method} failed: {e}"))
    }

    fn set_mode(&self, m: Mode) {
        let got = self.ok(methods::SETTINGS_SET, json!({ "mode": m.as_str() }));
        assert_eq!(
            got.get("mode").and_then(Value::as_str),
            Some(m.as_str()),
            "settings.set did not take the mode"
        );
    }

    fn fingerprint(&self) -> String {
        self.h.fingerprint.clone()
    }

    fn addr(&self) -> String {
        format!("127.0.0.1:{}", self.h.control_port)
    }

    fn peer(&self, fp: &str) -> Value {
        let list = self.ok(methods::PEERS_LIST, json!({}));
        list.as_array()
            .expect("peers.list is an array")
            .iter()
            .find(|p| p.get("fingerprint").and_then(Value::as_str) == Some(fp))
            .unwrap_or_else(|| panic!("{fp} is not in peers.list"))
            .clone()
    }

    fn sessions(&self) -> Vec<Value> {
        self.ok(methods::SESSION_LIST, json!({}))
            .as_array()
            .cloned()
            .unwrap_or_default()
    }
}

/// Pairs the two nodes for real (PIN + SPAKE2 + trust persistence) and leaves
/// `a` holding a live control channel to `b`.
fn pair(a: &Node, b: &Node) {
    let pin = b.ok(methods::PAIRING_ENABLE, json!({ "ttl_s": 60 }));
    let pin = pin
        .get("pin")
        .and_then(Value::as_str)
        .expect("pin")
        .to_string();
    a.ok(methods::PEERS_PAIR, json!({ "addr": b.addr(), "pin": pin }));
    // `peers.pair` only writes the trust; the control channel comes from a
    // connect — and that is what puts a ModeState on the wire.
    a.ok(
        methods::PEERS_CONNECT,
        json!({ "peer": b.fingerprint(), "addr": b.addr() }),
    );
}

/// Waits for `f` to hold, up to ~5s.
///
/// Mode advertisement crosses a real TCP connection and is read by the peer's
/// reader thread, so a bare assertion right after `connect` would be a race —
/// and a `sleep` long enough to be safe would also be long enough to hide a
/// regression that made the path slow instead of broken.
fn eventually(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for: {what}");
}

fn tone_spk(peer: &str) -> Value {
    json!({ "peer": peer, "kind": KIND_SPK, "source": SOURCE_TONE, "freq": 1000.0 })
}

// ------------------------------------------------------------------ 通告

/// 接线 ②：**对端是否真的收到通告。**
///
/// 两台真 daemon、真 TCP、真加密控制通道。断言的是 B 对 A 的看法，而这个看法只
/// 可能来自 A 发出、B 收下、B 解析、B 存进 `ConnShared::peer_mode`、`peers.list`
/// 读出来的那一条 `SessionMsg::ModeState`——中间任何一环断掉这条测试都变红。
#[test]
fn a_peer_learns_our_mode_and_learns_it_again_when_it_changes() {
    let a = Node::start("adv-a");
    let b = Node::start("adv-b");
    a.set_mode(Mode::Share);
    b.set_mode(Mode::Share);
    pair(&a, &b);

    let afp = a.fingerprint();
    // ① 建链即通告：B 看得见 A 在共享模式，且没有被标成不可用。
    eventually("b to learn a's initial mode", || {
        b.peer(&afp).get("peer_mode").and_then(Value::as_str) == Some("share")
    });
    assert_eq!(
        b.peer(&afp).get("peer_unusable").and_then(Value::as_bool),
        Some(false),
        "a share-mode peer must not be marked unusable"
    );

    // ② 模式变了要**重新**通告。只在建链时发一次的实现会永远停在 "share"，
    //    这一段就是那个实现的死刑判决。
    a.set_mode(Mode::A);
    eventually("b to learn a switched to mode A", || {
        b.peer(&afp).get("peer_mode").and_then(Value::as_str) == Some("a")
    });
    assert_eq!(
        b.peer(&afp).get("peer_unusable").and_then(Value::as_bool),
        Some(true),
        "a consumer-mode peer must be reported unusable, or the UI offers it and then fails"
    );

    // ③ 回到共享模式同样要通告：「一旦不可用就永远不可用」也是错的。
    a.set_mode(Mode::Share);
    eventually("b to learn a came back to share", || {
        b.peer(&afp).get("peer_unusable").and_then(Value::as_bool) == Some(false)
    });
}

/// 离线对端的模式是**不知道**，不是记忆里的那个值。
///
/// 记住上一次的模式会让一台已经关机的机器在列表里带着「可用」徽章——一个关于
/// 过去的陈述被当成了关于现在的承诺。
#[test]
fn an_offline_peer_reports_no_mode_at_all() {
    let a = Node::start("off-a");
    let b = Node::start("off-b");
    a.set_mode(Mode::Share);
    b.set_mode(Mode::Share);
    pair(&a, &b);
    let bfp = b.fingerprint();

    // 正向对照：在线时确实读得到模式，否则下面的 null 说明不了任何事情。
    eventually("a to learn b's mode while it is up", || {
        a.peer(&bfp).get("peer_mode").and_then(Value::as_str) == Some("share")
    });

    // 两条路径分开验，它们经过的代码不是同一段。
    //
    // ① 连接还**留在表里**但已判死。这是 `peers.list` 里那个 `.filter(alive)` 唯一
    //    存在的理由：`peer_mode` 就挂在这条 `ConnShared` 上，它记着的仍然是对端
    //    最后一次说的话。去掉过滤器，一台已经判死的机器会顶着「可用」继续挂在
    //    列表上，而用户点下去只会得到一个失败。
    //
    //    白盒地把 `alive` 按下去，因为这个中间态在测试里很难自然复现（显式
    //    disconnect 会把整条记录从表里摘掉，见 ②），而它在真机上每一次链路静默
    //    判死时都会出现。
    {
        let st = crate::lk(&a.h.inner_for_test().state);
        let c = st.conns.get(&bfp).expect("the conn must still be in the table").clone();
        drop(st);
        c.alive.store(false, std::sync::atomic::Ordering::SeqCst);
    }
    let p = a.peer(&bfp);
    assert_eq!(
        p.get("online").and_then(Value::as_bool),
        Some(false),
        "a dead conn is not online"
    );
    assert!(
        p.get("peer_mode").map(Value::is_null).unwrap_or(false),
        "a dead conn's remembered mode is a statement about the past and must not be reported"
    );
    assert_eq!(
        p.get("peer_unusable").and_then(Value::as_bool),
        Some(false),
        "unknown is not unusable"
    );

    // ② 显式断开：整条记录离开连接表。
    a.ok(methods::PEERS_DISCONNECT, json!({ "peer": bfp.clone() }));
    eventually("a to forget b's mode once the channel is gone", || {
        let p = a.peer(&bfp);
        p.get("online").and_then(Value::as_bool) == Some(false)
            && p.get("peer_mode").map(Value::is_null).unwrap_or(false)
            && p.get("peer_unusable").and_then(Value::as_bool) == Some(false)
    });
}

// ------------------------------------------------------------------ 拒绝服务

/// 接线 ①：**模式切换后本机是否真的拒绝服务。**
///
/// A 在 B 处于共享模式时先成功开一条会话（正向对照，证明这条链路本来是通的），
/// 然后 B 切走，A 再开同一条会话必须失败。没有那个正向对照，「失败了」可能只是
/// 因为回环上根本开不起来会话。
#[test]
fn a_peer_that_left_share_mode_really_refuses_to_be_used() {
    let a = Node::start("gate-a");
    let b = Node::start("gate-b");
    a.set_mode(Mode::A); // A 得是使用端才发得起会话（§13 的另一半）
    b.set_mode(Mode::Share);
    pair(&a, &b);

    let open = tone_spk(&b.fingerprint());

    // 正向对照。
    let sess = a.call(methods::SESSION_OPEN, open.clone()).expect(
        "a share-mode peer must accept the stream — otherwise the negative below proves nothing",
    );
    let id = sess.get("id").and_then(Value::as_u64).expect("session id");
    a.ok(methods::SESSION_CLOSE, json!({ "id": id }));

    // B 离开共享模式后，同一条会话必须被 **B** 拒掉。
    b.set_mode(Mode::A);
    let err = a
        .call(methods::SESSION_OPEN, open)
        .expect_err("b is a consumer now and must refuse to be used");
    assert!(
        err.contains("cannot be used as an audio device"),
        "the refusal has to come from B's §13 guard, not from some other failure: {err}"
    );
}

/// 拒绝是**本机对本机模式**的判断，没有任何 override 能穿过它。
///
/// `override:true` 只放行本机那道闸（`refuse_using_others`）；对端那道闸在对端
/// 进程里，本机的任何参数都够不着。把判据换成「对端通告的模式」，或者给
/// `refuse_being_used` 加一个开关，这条都会变红。
#[test]
fn the_inbound_refusal_cannot_be_overridden_by_the_caller() {
    let a = Node::start("ovr-a");
    let b = Node::start("ovr-b");
    a.set_mode(Mode::A);
    b.set_mode(Mode::A); // 两边都是使用端
    pair(&a, &b);

    let mut p = tone_spk(&b.fingerprint());
    p["override"] = json!(true);
    let err = a
        .call(methods::SESSION_OPEN, p)
        .expect_err("no flag on this side may talk the other machine out of §13");
    assert!(
        err.contains("cannot be used as an audio device"),
        "expected the peer's own refusal: {err}"
    );
}

/// 共享模式下本机**不使用**别人：`session.open` 在本机这一侧就被拒。
/// 与上一条互为镜像，两条合起来才是完整的互斥。
#[test]
fn share_mode_refuses_to_open_a_session_of_its_own() {
    let a = Node::start("out-a");
    let b = Node::start("out-b");
    a.set_mode(Mode::Share);
    b.set_mode(Mode::Share);
    pair(&a, &b);

    let err = a
        .call(methods::SESSION_OPEN, tone_spk(&b.fingerprint()))
        .expect_err("share mode does not use other machines");
    assert!(err.contains("share mode"), "{err}");
}

// ------------------------------------------------------------------ 过渡

/// 接线 ②b（plan §13 推论 2）：**切走共享模式会真的关掉对端已经开着的会话。**
///
/// 断言落在**提供方自己**的会话表上：那条会话是对端发起的（`origin = "peer"`），
/// 切模式之后必须一条不剩。只拒绝新会话、放任已有的继续跑，会让本机在切换之后
/// 仍然既共享又使用——正是 §13 要消灭的状态，而且是被那个开关亲手造出来的。
#[test]
fn leaving_share_mode_closes_the_sessions_peers_already_had() {
    let a = Node::start("trans-a");
    let b = Node::start("trans-b");
    a.set_mode(Mode::A);
    b.set_mode(Mode::Share);
    pair(&a, &b);

    a.ok(methods::SESSION_OPEN, tone_spk(&b.fingerprint()));
    // 正向对照：B 这边确实有一条**对端发起的**会话在跑。
    eventually("b to be serving a peer-originated session", || {
        b.sessions()
            .iter()
            .any(|s| s.get("origin").and_then(Value::as_str) == Some("peer"))
    });

    b.set_mode(Mode::A);
    eventually("b to drop every peer-originated session", || {
        !b.sessions()
            .iter()
            .any(|s| s.get("origin").and_then(Value::as_str) == Some("peer"))
    });
}

/// 镜像：切**进**共享模式会关掉本机自己开的会话。
///
/// 少了这一半，一台在模式 A 下正送着音频的机器切到共享模式后会继续送——照样是
/// 「既共享又使用」，只是这次不合法的那条腿在本机。
#[test]
fn entering_share_mode_closes_the_sessions_we_opened_ourselves() {
    let a = Node::start("in-a");
    let b = Node::start("in-b");
    a.set_mode(Mode::A);
    b.set_mode(Mode::Share);
    pair(&a, &b);

    a.ok(methods::SESSION_OPEN, tone_spk(&b.fingerprint()));
    assert!(
        !a.sessions().is_empty(),
        "the session has to exist before switching, or the assertion below is vacuous"
    );

    a.set_mode(Mode::Share);
    eventually("a to drop its own sessions", || a.sessions().is_empty());
}

// ------------------------------------------------------------------ 设备

/// 接线 ③（plan §13 推论 3）：**虚拟设备只在模式 B 下存在。**
///
/// 调的是设备协调器每一拍真正跑的那个 `compute_desired`，喂进去一个**真实的**
/// daemon（真配对库、真 settings、真 `effective_mode`），只把「驱动有几个槽位」
/// 当参数给出去——跑测试的机器上没有驱动，而要验的分支与驱动无关。
///
/// 断言两件事，缺一不可：
///   · `desired` 为空 ⇒ 协调器的 diff 会把已发布的设备**删掉**（§13 推论 3 的
///     「无条件移除」正是靠这个，而不是靠某处单独的删除代码）；
///   · `reasons` 给出的是这个模式自己的理由 ⇒ 卡片上说的是实话。
///
/// 这条不能只测 `no_device_reason`：那个函数写对了但 `compute_desired` 不调用它，
/// 单元测试照样全绿，而设备会留在系统里。
#[test]
fn no_virtual_devices_are_desired_outside_mode_b() {
    let a = Node::start("dev-a");
    let b = Node::start("dev-b");
    pair(&a, &b);
    let bfp = b.fingerprint();
    let inner = a.h.inner_for_test();

    // 正向对照放在最前面：先证明这套输入**能**产出设备，否则下面每一条
    // 「desired 为空」都可能只是因为根本没有对端。
    {
        let mut table = crate::haldev::SlotTable::new();
        // 直接把生效模式按到 B——effective_mode 需要一条 HAL 桥，而测试机上没有。
        // 这里绕过的只是「有没有驱动」，`compute_desired` 里那条模式分支照跑。
        crate::lk(&inner.settings).mode = Mode::B;
        let pass = crate::haldev::compute_desired(inner, 4, &mut table);
        // 没有桥 ⇒ effective_mode 从 B 落回 A（这本身就是被冻结的降级行为），
        // 所以这里期望的是「按 A 处理」。真正的正向对照是下面那条断言：
        // 理由确实是 mode_a 而不是 mode_share —— 说明分支读的是生效模式。
        assert_eq!(
            pass.reasons.get(&bfp).map(String::as_str),
            Some("mode_a"),
            "requested B with no bridge runs as A, and the reason has to say A"
        );
    }

    for (m, want) in [(Mode::Share, "mode_share"), (Mode::A, "mode_a")] {
        a.set_mode(m);
        let mut table = crate::haldev::SlotTable::new();
        let pass = crate::haldev::compute_desired(inner, 4, &mut table);
        assert!(
            pass.desired.is_empty(),
            "mode {m} must desire no virtual devices — the reconcile diff is what removes the \
             published ones, so a non-empty set here means they survive the switch"
        );
        assert_eq!(
            pass.reasons.get(&bfp).map(String::as_str),
            Some(want),
            "mode {m} must explain itself with its own reason"
        );
    }

    // ...and the peer list agrees: no devices are reported to any client.
    assert!(
        a.peer(&bfp)
            .get("hal_device")
            .map(Value::is_null)
            .unwrap_or(true),
        "no virtual devices may exist outside mode B"
    );
}

/// 一台全新的 daemon 落在共享模式（plan §13 的默认值裁定）。
///
/// 断言走的是 `settings.get` 的真实回包，而不是 `StoredSettings::default()`——
/// 后者已经有单元测试了，而这里要证明的是那个默认值**真的到达了产品表面**：
/// 中间任何一处把它翻译成别的模式（比如某处遗留的「未知即模式 A」）都会被抓住。
#[test]
fn a_fresh_daemon_starts_in_share_mode() {
    let a = Node::start("fresh");
    let s = a.ok(methods::SETTINGS_GET, json!({}));
    assert_eq!(s.get("mode").and_then(Value::as_str), Some("share"));
    assert_eq!(
        s.get("effective_mode").and_then(Value::as_str),
        Some("share"),
        "share needs no driver, so it can never be downgraded"
    );
}

/// `settings.set` 只认这三个模式，认不出的一律报错——**不静默回落**。
///
/// 静默回落是这里最坏的失败：用户点了一个模式，界面显示另一个，而两者恰好在
/// 互斥关系的两侧。
#[test]
fn an_unknown_mode_is_rejected_rather_than_quietly_replaced() {
    let a = Node::start("badmode");
    a.set_mode(Mode::A);
    let err = a
        .call(methods::SETTINGS_SET, json!({ "mode": "provider" }))
        .expect_err("an undefined mode must be an error");
    assert!(err.contains("mode must be"), "{err}");
    // ...而且失败不得留下副作用。
    assert_eq!(
        a.ok(methods::SETTINGS_GET, json!({}))
            .get("mode")
            .and_then(Value::as_str),
        Some("a"),
        "a rejected write must not have moved the mode"
    );
}
