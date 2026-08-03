//! 传输档位的**接线**测试：改了滑条之后，daemon 侧的行为是不是真的变了。
//!
//! # 这个文件为什么必须存在
//!
//! `latency` / `quality` 在这一轮之前是两个被收下、写盘、原样回显、
//! **没有任何一行代码读**的字符串。当时也有测试（`settings.rs` 里的
//! round-trip），而且全绿——因为它们断言的是「字段写进去了」。
//! 字段确实写进去了。什么都没发生。
//!
//! 所以这里的每一条断言都盯着**执行器**：`TxShared.rung`（发送侧真的在用的
//! 采样率格号）与 `JitterBuffer::target()`（接收侧真的在排的深度）。
//! 两者都是音频线程每拍读的量，不是设置的副本。
//!
//! 起真 daemon（照 `mode_tests.rs` 的形状）的理由相同：本项目全部五次
//! 「一切都报成功，什么都没发生」里，没有一次是函数写错了，全都是
//! **函数没被调用**，而单元测试对着没人调用的函数照样绿。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use audiohub_ipc::{methods, Mode, KIND_SPK, SOURCE_TONE};

use crate::halbridge::HalBridgeMode;
use crate::{ipcserv, lk, start_daemon, DaemonCfg, DaemonHandle};

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
        let dir = std::env::temp_dir().join(format!("ahb-tr-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let h = start_daemon(DaemonCfg {
            control_port: 0,
            ipc_port: 0,
            config_dir: Some(dir.clone()),
            announce: false,
            // 与 mode_tests 同一条理由：`auto` 会把用户的真 daemon 从驱动上挤掉。
            hal_bridge: Some(HalBridgeMode::Off),
        })
        .expect("start daemon");
        Node { h, dir }
    }

    fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        ipcserv::dispatch_for_test(self.h.inner_for_test(), method, &params)
    }

    fn ok(&self, method: &str, params: Value) -> Value {
        self.call(method, params)
            .unwrap_or_else(|e| panic!("{method} failed: {e}"))
    }

    fn set_mode(&self, m: Mode) {
        self.ok(methods::SETTINGS_SET, json!({ "mode": m.as_str() }));
    }

    fn fingerprint(&self) -> String {
        self.h.fingerprint.clone()
    }

    fn peer(&self, fp: &str) -> Value {
        self.ok(methods::PEERS_LIST, json!({}))
            .as_array()
            .expect("peers.list is an array")
            .iter()
            // `PeerState.peer` 是 `#[serde(flatten)]` 的，指纹在**顶层**。
            .find(|p| p["fingerprint"].as_str() == Some(fp))
            .unwrap_or_else(|| panic!("{fp} is not in peers.list"))
            .clone()
    }

    fn addr(&self) -> String {
        format!("127.0.0.1:{}", self.h.control_port)
    }

    /// **发送侧执行器**：这条 daemon 上任意一条发送流当前真的在用的格号。
    /// 读的是 `TxShared.rung` 本身——音频线程每 10 ms 读的就是它。
    fn tx_rung(&self) -> Option<u32> {
        crate::snapshot_sessions(self.h.inner_for_test())
            .iter()
            .find_map(|e| {
                e.tx.as_ref()
                    .map(|t| t.rung.load(std::sync::atomic::Ordering::Relaxed))
            })
    }

    /// **接收侧执行器**：这条 daemon 上任意一条接收流的 JB 有效目标深度（帧）。
    fn jb_target(&self) -> Option<u32> {
        crate::snapshot_sessions(self.h.inner_for_test())
            .iter()
            .find_map(|e| e.rx.as_ref().map(|rx| lk(&rx.jbs).jb.target()))
    }

    /// 这条 daemon 上任意一条接收流的 JB 已经收到过多少个包。
    ///
    /// **每一条断言 JB 深度的测试都要先看它。** 一条没有媒体流入的 JB 的深度
    /// 永远是初始值，于是「深度没变」既可能是伺服坏了，也可能是根本没有音频——
    /// 两个完全不同的结论，而断言本身分不出来。
    fn jb_pushes(&self) -> Option<u32> {
        crate::snapshot_sessions(self.h.inner_for_test())
            .iter()
            .find_map(|e| e.rx.as_ref().map(|rx| lk(&rx.jbs).pushes))
    }

    /// JB 当前的包络（`min_target`, `max_target`）。
    fn jb_envelope(&self) -> Option<(u32, u32)> {
        crate::snapshot_sessions(self.h.inner_for_test())
            .iter()
            .find_map(|e| {
                e.rx.as_ref().map(|rx| {
                    let t = lk(&rx.jbs).jb.tuning();
                    (t.min_target, t.max_target)
                })
            })
    }
}

fn pair(a: &Node, b: &Node) {
    let pin = b.ok(methods::PAIRING_ENABLE, json!({ "ttl_s": 60 }));
    let pin = pin.get("pin").and_then(Value::as_str).expect("pin").to_string();
    a.ok(methods::PEERS_PAIR, json!({ "addr": b.addr(), "pin": pin }));
    a.ok(
        methods::PEERS_CONNECT,
        json!({ "peer": b.fingerprint(), "addr": b.addr() }),
    );
}

fn eventually(what: &str, f: impl FnMut() -> bool) {
    // 8 s：格号与 JB 深度都由 1 s 的 ticker 推动，5 s 在负载高的 CI 上偏紧。
    eventually_within(Duration::from_secs(8), what, f)
}

fn eventually_within(budget: Duration, what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for: {what}");
}

/// A 用 B：A 发一路 1 kHz 音到 B 的输出。于是 **A 有 tx、B 有 rx**。
fn tone_session(a: &Node, b: &Node) -> u64 {
    let s = a.ok(
        methods::SESSION_OPEN,
        json!({ "peer": b.fingerprint(), "kind": KIND_SPK, "source": SOURCE_TONE, "freq": 1000.0 }),
    );
    s.get("id").and_then(Value::as_u64).expect("session id")
}

fn linked(tag: &str) -> (Node, Node) {
    let a = Node::start(&format!("{tag}-a"));
    let b = Node::start(&format!("{tag}-b"));
    // A 是使用端（才能开会话），B 是共享端（才能被使用）。
    a.set_mode(Mode::A);
    b.set_mode(Mode::Share);
    pair(&a, &b);
    (a, b)
}

// --------------------------------------------------------------- 质量档

/// **接线 ①：把质量滑条拖到某一档，发送侧真的换了采样率格号。**
///
/// 断言的是 `TxShared.rung`——`tx_loop` 每 10 ms 读它、按它重建重采样器、
/// 按它写包头的 `sample_rate`。它不是设置的副本，是执行器本身。
///
/// 缺陷注入对照：把 `ticker_loop` 里那句 `tx.rung.store(rung, …)` 删掉
/// （只留 `autos.remove`），这条立刻红在「格号没动」。
#[test]
fn moving_the_quality_slider_really_changes_the_wire_rate() {
    let (a, b) = linked("q");
    let _id = tone_session(&a, &b);
    // 正向对照：先确认默认（AUTO）下确实有一条发送流，否则底下的断言证明不了
    // 任何事——一条永远是 `None` 的读数会让每一次比较都「通过」。
    eventually("a to have a sending stream", || a.tx_rung().is_some());

    // AUTO_RATES = [48000, 32000, 24000, 16000] ⇒ 格号 0..3。
    for (id, want_rung) in
        [("pcm16k", 3u32), ("pcm24k", 2), ("pcm32k", 1), ("pcm48k", 0)]
    {
        a.ok(methods::SETTINGS_SET, json!({ "quality": id }));
        eventually(&format!("tx rung to become {want_rung} for {id}"), || {
            a.tx_rung() == Some(want_rung)
        });
    }
}

/// **固定档必须让 AUTO 阶梯闭嘴。**
///
/// 阶梯继续跑的实现能过上一条（设定的那一刻格号确实变了），但用户选的档会在
/// 下一次丢包时被悄悄改回去。这条把「谁是权威」钉死：固定档期间反复推进
/// ticker，格号一步都不许动。
#[test]
fn a_fixed_quality_rung_is_not_overwritten_by_the_auto_ladder() {
    let (a, b) = linked("qfix");
    let _id = tone_session(&a, &b);
    eventually("a to have a sending stream", || a.tx_rung().is_some());

    a.ok(methods::SETTINGS_SET, json!({ "quality": "pcm16k" }));
    eventually("the fixed rung to take", || a.tx_rung() == Some(3));

    // 跨过好几个 ticker 周期。阶梯若还在跑，干净链路会把格号一路升回 0。
    let deadline = Instant::now() + Duration::from_secs(4);
    while Instant::now() < deadline {
        assert_eq!(
            a.tx_rung(),
            Some(3),
            "固定质量档被 AUTO 阶梯改掉了：界面显示 16 kHz，线上跑的是别的"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// 切回 AUTO 之后阶梯**重新掌权**。
///
/// 「固定档一旦选过就永远固定」是另一半失效：用户切回 AUTO，界面显示 AUTO，
/// 而线上永远停在上次那一档。
#[test]
fn switching_back_to_auto_hands_the_ladder_back_its_authority() {
    let (a, b) = linked("qauto");
    let _id = tone_session(&a, &b);
    eventually("a to have a sending stream", || a.tx_rung().is_some());

    // 用 32 kHz（格号 1）而不是 16 kHz（格号 3）：`AutoLadder` 每升一格要 10 个
    // 干净周期 ≈ 10 s，从格号 3 升回 0 要三次共 ~30 s。测的是「阶梯是否重新掌权」，
    // 一格足以证明，三格只是在等。
    a.ok(methods::SETTINGS_SET, json!({ "quality": "pcm32k" }));
    eventually("the fixed rung to take", || a.tx_rung() == Some(1));
    assert_eq!(
        a.h.inner_for_test().transport.quality_rate(),
        Some(32_000),
        "固定档没有被推给音频线程"
    );

    a.ok(methods::SETTINGS_SET, json!({ "quality": "auto" }));
    assert_eq!(
        a.h.inner_for_test().transport.quality_rate(),
        None,
        "切回 AUTO 之后固定档必须**立刻**撤销——等下一拍就是一段说不清归谁管的时间"
    );
    // 干净回环上阶梯会把格号升回 0（10 个干净周期）。这是「阶梯真的重新在写
    // `tx.rung`」的唯一证据：只断言 `quality_rate() == None` 的话，一个把阶梯
    // 永久停掉的实现照样绿。
    eventually_within(
        Duration::from_secs(30),
        "the ladder to promote back to rung 0",
        || a.tx_rung() == Some(0),
    );
}

// --------------------------------------------------------------- 延迟档

/// **接线 ③：伺服的输出真的被执行——不只是「换档时预置了一下」。**
///
/// # 这条测试是缺陷注入逼出来的
///
/// 注入 I4（`engine.rs` 拿到伺服输出后什么都不做）时，下面那条
/// `raising_the_latency_target_really_deepens_the_receive_buffer` **仍然是绿的**：
/// 换档会重建包络并把 JB **预置**到目标深度，光靠预置就足以让深度变化。
/// 那条测试证明的是「预置接对了」，不是「伺服接对了」。两段是两条独立的接线。
///
/// # 怎么把伺服单独隔出来
///
/// 包络是 `max(ceil(target/10) + 2, 默认上限 12)`，所以**目标 ≤ 100 ms 的几档
/// 共用同一个包络**（1..12 帧）。在这些档之间切换 ⇒ `reshape` 早退、不重建、
/// 不预置 ⇒ 深度还能不能动，就**只**取决于伺服的输出有没有被执行。
///
/// # 为什么不测闭环本身
///
/// 闭环要 `sum_ms`，而它 = 本侧 Σ + 网络段 + 对端 Σ。无设备的回环测试里本侧的
/// 输出尾级 `rate == 0`（没有真实输出设备），按「绝不用 0 填补」的规矩整个
/// `local_ms` 就是 `None` ⇒ `sum_ms` 恒 `None`。这不是缺陷，是测量纪律的正确
/// 结果；闭环的收敛性由 `servo::tests` 里的纯函数测试覆盖
/// （尤其 `a_moving_floor_is_re_solved_every_tick_not_cached`）。
#[test]
fn the_servo_output_is_really_executed_not_just_the_reseed() {
    let (a, b) = linked("steer");
    let _id = tone_session(&a, &b);
    eventually("b to have a receiving stream", || b.jb_target().is_some());
    eventually("media to actually reach b's jitter buffer", || {
        b.jb_pushes().map_or(false, |n| n > 20)
    });

    // 100 ms 档：包络变成 1..12（从默认的 4..12），重建一次并预置到 10 帧。
    b.ok(methods::SETTINGS_SET, json!({ "latency": "100" }));
    eventually_within(
        Duration::from_secs(20),
        "the buffer to be seeded at the 100ms depth",
        || b.jb_target().map_or(false, |t| t >= 8),
    );
    let envelope = b.jb_envelope().expect("envelope");

    // 20 ms 档：`max(ceil(20/10)+2, 12) == 12`，与上一档**同一个包络**。
    b.ok(methods::SETTINGS_SET, json!({ "latency": "20" }));
    eventually_within(
        Duration::from_secs(25),
        "the servo to walk the buffer down to the 20ms depth",
        || b.jb_target().map_or(false, |t| t <= 4),
    );
    assert_eq!(
        b.jb_envelope(),
        Some(envelope),
        "包络在这两档之间变了 —— 那这条测试就又被预置盖住了，隔离失效"
    );
}

/// **接线 ②：把延迟滑条拖高，接收侧的 JB 真的排得更深。**
///
/// 注意这条覆盖的是**换档预置**那一段（重建包络 + 落到目标深度）。
/// 伺服输出的执行由 `the_servo_output_is_really_executed_not_just_the_reseed`
/// 单独覆盖——缺陷注入 I4 证明了一条测试盖不住两段。
///
/// 注意这条覆盖的是**换档预置**那一段（重建包络 + 落到目标深度）。
/// 闭环收敛由 `the_closed_loop_converges_below_the_open_loop_seed` 覆盖——
/// 两段是两条独立的接线，缺陷注入 I4 证明了一条测试盖不住两段。
///
/// 断言的是 `JitterBuffer::target()`——`pop()` 每 10 ms 拿它当收敛判据。
///
/// 目标从 0 拖到 500 ms，JB 必须显著变深；若只是把目标数字存起来，深度不会动。
#[test]
fn raising_the_latency_target_really_deepens_the_receive_buffer() {
    let (a, b) = linked("lat");
    let _id = tone_session(&a, &b);
    // B 是接收侧，它的 JB 才是执行器。**两级正向对照**：先有流，再有音频。
    // 只确认「有流」是不够的——一条没有包流入的 JB 深度永远是初始值，
    // 底下所有关于深度的断言都会因为同一个原因通过或失败。
    eventually("b to have a receiving stream", || b.jb_target().is_some());
    eventually("media to actually reach b's jitter buffer", || {
        b.jb_pushes().map_or(false, |n| n > 20)
    });

    // 最低档：伺服往下压。
    b.ok(methods::SETTINGS_SET, json!({ "latency": "0" }));
    eventually("the buffer to be driven shallow", || {
        b.jb_target().map_or(false, |t| t <= 2)
    });
    let shallow = b.jb_target().expect("target");

    // 高档：伺服往上加。500 ms 远高于回环链路的地板，所以必须一路加深。
    b.ok(methods::SETTINGS_SET, json!({ "latency": "500" }));
    eventually("the buffer to be driven deep", || {
        b.jb_target().map_or(false, |t| t > shallow + 5)
    });
    let deep = b.jb_target().expect("target");
    assert!(
        deep > shallow,
        "延迟目标从 0 拖到 500 ms，JB 深度却没动（{shallow} -> {deep} 帧）——\
         设置被存下来了，媒体面没有读它"
    );
}

/// **包络必须跟着目标走。**
///
/// 默认包络是 4..12 帧 = 40..120 ms（`JbTuning::DEFAULT` 的实测整定）。
/// 不重建包络的实现能过上一条测试的前半段（0 档往下压确实会动），
/// 但 500 ms 会被 `clamp` 砍在 12 帧上，滑条右半边全部失效，
/// 而 UI 会显示「已达物理上限」——一个**我们自己造的**上限。
#[test]
fn a_high_target_widens_the_envelope_instead_of_reporting_a_fake_ceiling() {
    let (a, b) = linked("env");
    let _id = tone_session(&a, &b);
    eventually("b to have a receiving stream", || b.jb_envelope().is_some());
    let (_, default_hi) = b.jb_envelope().expect("envelope");

    b.ok(methods::SETTINGS_SET, json!({ "latency": "750" }));
    eventually("the envelope to widen for a 750ms target", || {
        b.jb_envelope().map_or(false, |(_, hi)| hi > default_hi)
    });
    let (lo, hi) = b.jb_envelope().expect("envelope");
    assert!(
        hi as f64 * 10.0 >= 750.0,
        "包络上限 {hi} 帧 = {} ms，装不下 750 ms 的目标",
        hi * 10
    );
    assert_eq!(lo, 1, "固定档下下限要放开，否则最低档也够不到");

    // 切回 AUTO ⇒ 恢复实测默认整定。固定档期间放开的下限不许留给 AUTO，
    // 那会悄悄改掉 plan §5 里 AUTO 的整定。
    b.ok(methods::SETTINGS_SET, json!({ "latency": "auto" }));
    eventually("the envelope to return to the measured default", || {
        b.jb_envelope() == Some((
            audiohub_net::media::JitterBuffer::MIN_TARGET,
            audiohub_net::media::JitterBuffer::MAX_TARGET,
        ))
    });
}

/// AUTO 档下伺服**一步都不许走**——那一档按 plan §5 归抖动公式管。
#[test]
fn auto_latency_leaves_the_servo_silent() {
    let (a, b) = linked("latauto");
    let _id = tone_session(&a, &b);
    eventually("b to have a receiving stream", || b.jb_target().is_some());

    b.ok(methods::SETTINGS_SET, json!({ "latency": "auto" }));
    // 给 ticker 几拍。
    std::thread::sleep(Duration::from_millis(2500));
    assert_eq!(
        b.h.inner_for_test().transport.servo_frames(),
        None,
        "AUTO 下伺服写了深度：两条回路在抢同一个水位"
    );
}

// ------------------------------------------------------ 拒绝 / 契约

/// **Opus 三档在滑条上看得见，`settings.set` 必须拒收。**
///
/// 收下它 = 界面显示「Opus 128k」而线上一个字节都没变。
/// 顺带断言**拒绝之后盘上的值没被改动**：一次被拒的写入不许留下半个副作用。
#[test]
fn an_unimplemented_quality_rung_is_refused_and_changes_nothing() {
    let a = Node::start("refuse");
    let before = a.ok(methods::SETTINGS_GET, json!({}));
    let before_q = before.get("quality").and_then(Value::as_str).unwrap().to_string();

    for bad in ["opus64", "opus128", "opus256", "pcm96k", "", "PCM48K"] {
        let err = a
            .call(methods::SETTINGS_SET, json!({ "quality": bad }))
            .expect_err(&format!("quality '{bad}' 本 build 给不了，必须拒收"));
        assert!(
            err.contains("quality"),
            "拒绝理由要说清楚是哪个字段的问题：{err}"
        );
    }
    let after = a.ok(methods::SETTINGS_GET, json!({}));
    assert_eq!(
        after.get("quality").and_then(Value::as_str),
        Some(before_q.as_str()),
        "一次被拒的写入改动了盘上的值"
    );
    assert_eq!(
        a.h.inner_for_test().transport.quality_rate(),
        None,
        "被拒的档位泄漏到了音频线程"
    );
}

/// 档位表以外的毫秒数同样拒收，**不是就近吸附**。
#[test]
fn a_latency_value_off_the_ladder_is_refused() {
    let a = Node::start("refuse-lat");
    for bad in ["137", "1", "1001", "-5", "auto ", "min2"] {
        let err = a
            .call(methods::SETTINGS_SET, json!({ "latency": bad }))
            .expect_err(&format!("latency '{bad}' 不是档位，必须拒收"));
        assert!(err.contains("latency"), "{err}");
    }
    // 每一个真档位都要收得下——否则上面的拒绝只是「什么都不接受」。
    for &ms in &audiohub_ipc::LATENCY_STOPS_MS {
        let got = a.ok(methods::SETTINGS_SET, json!({ "latency": ms.to_string() }));
        assert_eq!(
            got.get("latency").and_then(Value::as_str),
            Some(ms.to_string().as_str()),
            "{ms} ms 是档位表里的档，却没被收下"
        );
    }
    a.ok(methods::SETTINGS_SET, json!({ "latency": "auto" }));
    // 旧拼写要被**规范化**存下来，不是原样留着：盘上留两种写法会让下一个
    // 读者以为是两档。
    let got = a.ok(methods::SETTINGS_SET, json!({ "latency": "min" }));
    assert_eq!(
        got.get("latency").and_then(Value::as_str),
        Some("0"),
        "旧的 \"min\" 要被规范化成 \"0\""
    );
}

/// `settings.get` 必须把**档位表**发出去：前端不许自己写一份。
/// 两边各存一份，分歧不会有任何报错——只会有一个选不中的档。
#[test]
fn the_settings_view_carries_the_ladders_and_the_live_readout() {
    let a = Node::start("catalog");
    let v = a.ok(methods::SETTINGS_GET, json!({}));

    let stops: Vec<u64> = v["latency_stops_ms"]
        .as_array()
        .expect("latency_stops_ms 必须在 settings.get 里")
        .iter()
        .map(|x| x.as_u64().expect("毫秒数"))
        .collect();
    assert_eq!(
        stops,
        audiohub_ipc::LATENCY_STOPS_MS
            .iter()
            .map(|&m| m as u64)
            .collect::<Vec<_>>(),
        "发出去的延迟档位表与契约不一致"
    );

    let q = v["quality_stops"].as_array().expect("quality_stops 必须在");
    assert_eq!(q.len(), audiohub_ipc::transport::quality_stops().len());
    for stop in q {
        for f in ["id", "available"] {
            assert!(stop.get(f).is_some(), "质量档缺 {f}：{stop}");
        }
    }
    // 不可用档必须**出现在表里**（UI 要画灰刻度），且如实标注原因。
    let opus: Vec<&Value> = q
        .iter()
        .filter(|s| s["blocked_by"].as_str() == Some("opus"))
        .collect();
    assert_eq!(opus.len(), 3, "plan §5 的 Opus 三档要在表里可见");
    for s in opus {
        assert_eq!(s["available"].as_bool(), Some(false));
    }

    // 读数区必须存在，且**没有会话时如实说没有**——不是一个好看的 0。
    let live = &v["transport_live"];
    assert_eq!(live["streams"].as_u64(), Some(0), "没有会话就该报 0 条流");
    assert!(
        live["achieved_ms"].is_null(),
        "没有测量结果时必须是 null，不是 0——0 会显示成「延迟极低」"
    );
    assert_eq!(live["at_floor"].as_bool(), Some(false));
}

/// **热生效：改设置不重启、不重连，也不动任何已开的会话。**
///
/// 模式切换会关会话（plan §13 推论 2），传输档位不该。一个顺手把会话关掉的
/// 实现「也能生效」，但用户每拖一次滑条就断一次音。
#[test]
fn changing_transport_settings_keeps_existing_sessions_open() {
    let (a, b) = linked("keep");
    let id = tone_session(&a, &b);
    let count = || {
        a.ok(methods::SESSION_LIST, json!({}))
            .as_array()
            .map(|v| v.len())
            .unwrap_or(0)
    };
    let before = count();
    assert!(before > 0, "正向对照：先得真有会话");

    for patch in [
        json!({ "latency": "200" }),
        json!({ "quality": "pcm24k" }),
        json!({ "latency": "auto", "quality": "auto" }),
    ] {
        a.ok(methods::SETTINGS_SET, patch.clone());
        assert_eq!(count(), before, "{patch} 关掉了已有会话");
    }
    // 会话确实还是原来那一条，不是关掉又重开的新的。
    assert!(
        a.ok(methods::SESSION_LIST, json!({}))
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["id"].as_u64() == Some(id)),
        "会话 id 变了：这条链路被重开过"
    );
}

// ------------------------------------------------ 方向性盲区（音质 / 网络段）

/// **对端那一侧的音质真的回传过来了**，纯发送的流不再永远空着。
///
/// # 这条测试的被测对象是「回传」，不是「字段加了」
///
/// 断言写在 `session.list` 的 `peer_quality` 上，而那个值只可能来自：
/// B 的接收会话算出原料 → `SessionMsg::StageReport.quality` → 真 TCP →
/// A 的 `owned_session` 安全边界 → `PeerLatCell` → `grade_peer_quality`。
/// 中间任何一环断掉这条就变红。
///
/// 正向对照必须先成立：**A 这一侧本地音质确实是 `None`**。没有这一句，
/// 「`peer_quality` 有值」也可能只是因为随便哪里都能算出音质，
/// 而这条改动要解决的病恰恰是「发送侧算不出」。
#[test]
fn the_peers_own_quality_measurement_crosses_the_wire_to_the_sender() {
    let (a, b) = linked("pq");
    let _id = tone_session(&a, &b);

    // 正向对照 ①：B 是接收侧，它自己算得出音质。
    eventually_within(
        Duration::from_secs(25),
        "b to measure quality on its own receiving session",
        || {
            b.ok(methods::SESSION_LIST, json!({}))
                .as_array()
                .map_or(false, |v| v.iter().any(|s| !s["stats"]["quality"].is_null()))
        },
    );

    // 正向对照 ②：A 是纯发送侧，本地音质**必然**是 null——这正是那个盲区。
    let sess = a.ok(methods::SESSION_LIST, json!({}));
    let row = &sess.as_array().expect("sessions")[0];
    assert!(
        row["stats"]["quality"].is_null(),
        "发送侧居然自己算出了音质？那这条测试就证明不了回传：{}",
        row["stats"]["quality"]
    );

    // 被测：对端的测量到达了发送侧。
    eventually_within(
        Duration::from_secs(25),
        "the peer's quality to reach the sending side",
        || {
            a.ok(methods::SESSION_LIST, json!({}))
                .as_array()
                .map_or(false, |v| {
                    v.iter().any(|s| !s["stats"]["peer_quality"].is_null())
                })
        },
    );

    let sess = a.ok(methods::SESSION_LIST, json!({}));
    let pq = &sess.as_array().expect("sessions")[0]["stats"]["peer_quality"];
    // 等级是**本机门限**评出来的，不是对端算好送来的字符串。
    let grade = pq["grade"].as_str().expect("grade");
    assert!(
        ["excellent", "good", "fair", "poor", "unknown"].contains(&grade),
        "评级不是本机口径的取值：{grade}"
    );
    assert!(pq["window_s"].as_f64().unwrap_or(0.0) > 0.0, "窗口跨度没过来");
    assert!(pq["popped_ticks"].as_u64().unwrap_or(0) > 0, "原料计数没过来");
    // 「还没测」与「测了是 0」的区别必须活着穿过线缆。
    assert!(
        pq["clip_ratio"].is_null() || pq["clip_ratio"].is_f64(),
        "clip_ratio 被压成了别的东西：{}",
        pq["clip_ratio"]
    );
}

/// **对端送来的「还没测」在评级时不许变成「测了是 0」。**
///
/// 这是本机侧栽过一次的坑，现在多了一条线缆，于是多了一个复发点：
/// `grade_clip(0.0) = Excellent`，而 `Excellent` 是 `Ord` 的最大值，
/// min 合成下它**永远拉不低总分** ⇒ 一条正在爆音的流报「良好」。
///
/// 纯函数测试，不需要起 daemon —— 但它盯的是真实的生产路径
/// （`grade_peer_quality` 就是 `session.list` 里那一格的来源）。
#[test]
fn a_peer_reading_without_a_clip_page_does_not_become_excellent() {
    use audiohub_net::secure::QualityReading;
    let base = QualityReading {
        window_s: 10.0,
        conceal_ratio: 0.0, // Q1 完美
        plc_ticks: 0,
        silence_ticks: 0,
        popped_ticks: 1000,
        underruns: 0,
        jb_dropped: 0,
        clip_ratio: None,
        clip_excess_db: None,
        bandwidth_hz: 24_000, // Q3 满带宽
        duplicate: false,
    };
    let unmeasured = crate::grade_peer_quality(&base);
    assert!(
        unmeasured.clip_ratio.is_none(),
        "「还没测」被填成了一个数：{:?}",
        unmeasured.clip_ratio
    );
    assert!(
        unmeasured.partial,
        "缺一个分量却没标 partial —— 用户会以为这是一个完整的判断"
    );
    assert_eq!(
        unmeasured.grade, "unknown",
        "分量缺席时等级必须承认不确定，而不是拿在场分量的 min 冒充完整评级"
    );

    // 正向对照：**测出来确实是 0** 时，等级就该成立。没有这一句，上面的
    // "unknown" 也可能只是因为这个函数根本评不出等级来。
    let measured = crate::grade_peer_quality(&QualityReading {
        clip_ratio: Some(0.0),
        clip_excess_db: Some(-120.0),
        ..base.clone()
    });
    assert_eq!(measured.clip_ratio, Some(0.0));
    assert!(!measured.partial);
    assert_ne!(measured.grade, "unknown", "三个分量都在，等级必须成立");

    // 重复流是一票否决，且**不依赖本流的削顶页**（规格 §4.4）。
    let dup = crate::grade_peer_quality(&QualityReading { duplicate: true, ..base.clone() });
    assert_eq!(dup.grade, "poor", "对端判定的重复流没有被一票否决");
}

/// **只要连上就报得出网络段——不需要任何媒体会话。**
///
/// 用户的原话是「只要与对端连接上以后就能显示能显示的延迟（通常只有网络）」。
/// 改动前 `sum_ms` 在没有会话时是 `None`，整块不渲染，界面上一个数字都没有。
///
/// 这条**故意不开任何会话**：一个把 `net_ms` 挂在会话上的实现会立刻变红。
#[test]
fn a_connected_peer_reports_its_network_leg_with_no_session_at_all() {
    let (a, b) = linked("net");
    let bfp = b.fingerprint();

    // 前提：确实一条会话都没有。否则这条测的是别的东西。
    assert!(
        a.ok(methods::SESSION_LIST, json!({})).as_array().map_or(true, |v| v.is_empty()),
        "这条测试的前提是没有会话"
    );

    // min-RTT 窗口要攒够样本（Ping 每秒一拍）。
    eventually_within(
        Duration::from_secs(30),
        "the network leg to become available without any session",
        || a.peer(&bfp)["net_ms"].as_f64().is_some(),
    );

    let p = a.peer(&bfp);
    let net = p["net_ms"].as_f64().expect("net_ms");
    let rtt = p["rtt_ms"].as_f64().expect("rtt_ms");
    assert!(net >= 0.0 && net < 1000.0, "回环上的单程延迟不该是 {net} ms");
    assert!(rtt >= 0.0, "rtt {rtt}");
    // 单程 = min-RTT/2，所以它**不可能**比最近一次 RTT 还大出一截。
    // 这条挡的是「把 RTT 直接当成单程报上去」——那会让用户看到的数字翻一倍。
    assert!(
        net <= rtt.max(0.001) * 2.0 + 1.0,
        "net_ms {net} 与 rtt_ms {rtt} 对不上：单程被当成往返报了？"
    );
    // 会话数仍然是 0：网络段确实与会话无关。
    assert!(
        a.ok(methods::SESSION_LIST, json!({})).as_array().map_or(true, |v| v.is_empty()),
        "测量过程中冒出了会话，前提被破坏"
    );
}

/// **离线对端不许报网络段。** `ClockFilter` 里还留着上一次的窗口，
/// 报出去就是拿一个关于过去的陈述冒充现在——与 `peer_mode` 同一条规矩。
#[test]
fn an_offline_peer_reports_no_network_leg() {
    let (a, b) = linked("netoff");
    let bfp = b.fingerprint();
    // 正向对照：在线时确实读得到，否则下面的 null 说明不了任何事。
    eventually_within(
        Duration::from_secs(30),
        "the network leg while the peer is up",
        || a.peer(&bfp)["net_ms"].as_f64().is_some(),
    );
    drop(b);
    eventually_within(Duration::from_secs(20), "a to notice b is gone", || {
        a.peer(&bfp)["online"].as_bool() == Some(false)
    });
    let p = a.peer(&bfp);
    assert!(
        p["net_ms"].is_null() && p["rtt_ms"].is_null(),
        "离线对端仍在报网络段：{p}"
    );
}

/// **重启之后固定档仍然在执行**，不只是仍然被显示。
///
/// 「盘上存着 pcm16k、回显 pcm16k、跑的是 AUTO」是这一整轮要消灭的形态，
/// 而重启是它最容易复发的地方——启动时忘了 publish 就是这个结果。
#[test]
fn a_fixed_choice_is_still_in_force_after_a_restart() {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("ahb-tr-restart-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    let cfg = || DaemonCfg {
        control_port: 0,
        ipc_port: 0,
        config_dir: Some(dir.clone()),
        announce: false,
        hal_bridge: Some(HalBridgeMode::Off),
    };

    let first = start_daemon(cfg()).expect("start");
    ipcserv::dispatch_for_test(
        first.inner_for_test(),
        methods::SETTINGS_SET,
        &json!({ "latency": "300", "quality": "pcm24k" }),
    )
    .expect("set");
    first.shutdown();

    let second = start_daemon(cfg()).expect("restart");
    let t = &second.inner_for_test().transport;
    assert_eq!(
        t.quality_rate(),
        Some(24_000),
        "重启后固定质量档没有被推给音频线程：回显是对的，执行的是 AUTO"
    );
    assert_eq!(
        t.latency_target(),
        audiohub_ipc::LatencyTarget::TotalMs(300),
        "重启后固定延迟档没有生效"
    );
    second.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}
