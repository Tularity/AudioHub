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

use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use audiohub_ipc::{methods, Mode, KIND_SPK, SOURCE_TONE};

use crate::halbridge::HalBridgeMode;
use crate::{ipcserv, lk, start_daemon, DaemonCfg, DaemonHandle};

/// 阶梯格号的助记名。位深进阶梯之后 `LADDER` 是六格（rung 0 = 48 kHz/32f 最好），
/// 用字面量写会在下次加档时静默错位——而错位的表现是「测试仍然绿，只是测了
/// 另一档」。
const RUNG_48K_16: u32 = 2;
const RUNG_32K: u32 = 3;
const RUNG_24K: u32 = 4;
const RUNG_16K: u32 = 5;

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
        Node::start_throttled(tag, None)
    }

    /// `tx_throttle_kbps` in kbit/s on this daemon's degraded-transport writers
    /// only. See `DaemonCfg::tx_throttle_kbps`.
    fn start_throttled(tag: &str, tx_kbps: Option<u64>) -> Node {
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
            tx_throttle_kbps: tx_kbps,
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

    /// plan §15：给**某一台对端某一个方向**设档位。
    ///
    /// `dir` 是**本机视角**：`"recv"` = 我收这台对端的音，`"send"` = 我发给它。
    /// 只有消费者调它——共享模式的机器不设置、只接受。
    fn set_transport(&self, fp: &str, dir: &str, key: &str, v: &str) -> Value {
        self.ok(
            methods::PEERS_SET_TRANSPORT,
            json!({ "peer": fp, "dir": dir, key: v }),
        )
    }

    /// 这台 daemon 上**每条接收流**的伺服现场，按 stream id 索引。
    fn by_stream(&self) -> serde_json::Map<String, Value> {
        self.servo()
            .get("by_stream")
            .and_then(|v| v.as_object().cloned())
            .expect("servo.by_stream 必须存在")
    }

    /// **某一条**接收流的伺服现场。测试里至多一条，多的话取第一条并在断言
    /// 里点名——`by_stream` 的键是 stream id，跨进程不可预测。
    ///
    /// 零流时返回 `Value::Null`：`.as_u64()` 于是恒为 `None`，断言会红在
    /// 「读不到」而不是悄悄拿一个别处的数。
    fn rx_servo(&self) -> Value {
        self.by_stream()
            .into_iter()
            .next()
            .map(|(_, v)| v)
            .unwrap_or(Value::Null)
    }

    /// 在**真的加密控制通道**上发一条原始 `SessionMsg`。
    ///
    /// 只给「伪造一个行为不端的对端」用：正常路径上 daemon 自己不会发出这样
    /// 的消息，而那正是那几道闸门存在的理由。走 `send_msg` 而不是直接调处理
    /// 函数——闸门在分发里，绕过分发就是在测一个没人调用的函数。
    fn send_raw(&self, to_fp: &str, msg: &audiohub_net::secure::SessionMsg) {
        let c = crate::lk(&self.h.inner_for_test().state)
            .conns
            .get(to_fp)
            .cloned()
            .unwrap_or_else(|| panic!("没有到 {to_fp} 的控制连接"));
        c.send_msg(msg).expect("send_msg");
    }

    /// 全部接收流累计的 `moves`。零流 = 0（**不是 None**：这里问的是
    /// 「回路动过几次手」，没有对象时答案确实是零次）。
    fn servo_moves(&self) -> u64 {
        self.by_stream()
            .values()
            .filter_map(|v| v["moves"].as_u64())
            .sum()
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

    /// **发送侧执行器**的固定档：这条流的 `TransportControl` 里那个**格号**。
    /// `None` = AUTO（阶梯当家）。
    ///
    /// 位深进阶梯之前这里存的是采样率。改存格号的理由见 `TransportControl`：
    /// 一档现在是 `(采样率, 位深)` 二元组，两个原子量之间会撕裂出一个阶梯上
    /// 根本不存在的组合。
    fn tx_quality_rung(&self) -> Option<u32> {
        crate::snapshot_sessions(self.h.inner_for_test())
            .iter()
            .find_map(|e| e.tx.as_ref().and_then(|t| t.transport.quality_rung()))
    }

    /// **接收侧执行器**的伺服输出（帧）。`None` = AUTO / 还没算出来。
    fn rx_servo_frames(&self) -> Option<u32> {
        crate::snapshot_sessions(self.h.inner_for_test())
            .iter()
            .find_map(|e| e.rx.as_ref().and_then(|rx| rx.transport.servo_frames()))
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

    /// **界面读到的**线上采样率：`session.list` 里 `SessionInfo.sample_rate`。
    ///
    /// 与 `tx_rung` 是两个东西，这一点是承重的：`tx_rung` 是执行器的内部序号，
    /// 用户从没见过；`sample_rate` 是统计页上那个数。这个字段此前是**硬编码的
    /// 48000**，于是「格号动了」与「界面上的数动了」之间没有任何联系——
    /// 一条只断言格号的测试对 UI 层的谎言完全免疫。
    fn session_wire_rate(&self) -> Option<u32> {
        self.ok(methods::SESSION_LIST, json!({}))
            .as_array()?
            .iter()
            .find_map(|s| s["sample_rate"].as_u64())
            .map(|v| v as u32)
    }

    /// 界面读到的**线上位深**：`SessionInfo.wire_depth`。
    /// `""` = 两侧都报不出（**不是 s16**）。
    fn session_wire_depth(&self) -> Option<String> {
        self.ok(methods::SESSION_LIST, json!({}))
            .as_array()?
            .iter()
            .find_map(|s| s["wire_depth"].as_str())
            .map(str::to_string)
    }

    /// One numeric field out of the first session's `stats`, **through IPC**.
    ///
    /// Going through `session.list` rather than reading the counters directly
    /// is load-bearing here: the whole defect being fixed was that
    /// `SessionStats.wire_bytes` was never assigned on the send side. A test
    /// that read `TxShared` directly would have been green throughout.
    fn stat_f64(&self, key: &str) -> Option<f64> {
        self.ok(methods::SESSION_LIST, json!({}))
            .as_array()?
            .iter()
            .find_map(|s| s["stats"][key].as_f64())
    }

    fn stat_u64(&self, key: &str) -> Option<u64> {
        self.ok(methods::SESSION_LIST, json!({}))
            .as_array()?
            .iter()
            .find_map(|s| s["stats"][key].as_u64())
    }

    /// Two `stats` counters out of **one** `session.list` call.
    ///
    /// Reading them in two calls skews the ratio by however many packets went
    /// out in between — about 1% per call gap here, which is the same order as
    /// the framing overhead the byte account is meant to detect. One snapshot
    /// removes the skew and lets the tolerance be tight enough to matter.
    fn stat_pair_u64(&self, a: &str, b: &str) -> Option<(u64, u64)> {
        self.ok(methods::SESSION_LIST, json!({}))
            .as_array()?
            .iter()
            .find_map(|s| Some((s["stats"][a].as_u64()?, s["stats"][b].as_u64()?)))
    }

    /// Steady-state payload bytes per delivered 10 ms frame, measured on **this**
    /// node's `wire_bytes` and this node's frame count.
    ///
    /// Works in both directions, which is the point: the send side has no
    /// jitter buffer, so frames are counted from `sent_packets` and the wire
    /// packets-per-frame of the rung in force. Expected values are 960 / 1440 /
    /// 1920 B for s16 / s24 / f32 at 48 kHz.
    fn tx_wire_bytes_per_frame(&self, window: Duration) -> Option<f64> {
        let (b0, p0) = self.stat_pair_u64("wire_bytes", "sent_packets")?;
        std::thread::sleep(window);
        let (b1, p1) = self.stat_pair_u64("wire_bytes", "sent_packets")?;
        let pkts = p1.checked_sub(p0)?;
        // A deep rung splits each 10 ms frame into two wire packets, so packets
        // alone do not equal frames; normalise by the split of the live rung.
        let parts = audiohub_net::media::rung_format(self.tx_rung()?).wire_packets_per_frame() as u64;
        let frames = pkts / parts.max(1);
        if frames < 50 {
            return None;
        }
        Some(b1.checked_sub(b0)? as f64 / frames as f64)
    }

    /// 音质原料里的位深：`QualityStats.wire_depth`（本机侧优先，回落对端回传）。
    fn quality_wire_depth(&self) -> Option<String> {
        self.ok(methods::SESSION_LIST, json!({}))
            .as_array()?
            .iter()
            .find_map(|s| {
                let st = &s["stats"];
                let q = st["quality"].as_object().or_else(|| st["peer_quality"].as_object())?;
                q.get("wire_depth")?.as_str().map(str::to_string)
            })
    }

    /// 音质那一格的两个数：`(wire_rate_hz, bandwidth_hz)`。
    /// 一级界面显示前者（与设置同量纲），展开明细显示后者。
    /// 本机侧 `quality` 优先，纯发送流回落到对端回传的 `peer_quality`。
    fn session_quality_rate(&self) -> Option<(u32, u32)> {
        self.ok(methods::SESSION_LIST, json!({}))
            .as_array()?
            .iter()
            .find_map(|s| {
                let st = &s["stats"];
                let q = st["quality"].as_object().or_else(|| st["peer_quality"].as_object())?;
                Some((
                    q.get("wire_rate_hz")?.as_u64()? as u32,
                    q.get("bandwidth_hz")?.as_u64()? as u32,
                ))
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

    /// **真的落在线上的载荷字节数**（累计，AEAD 解密后的明文长度之和）。
    ///
    /// 这是位深生效的**唯一硬证据**。上面 `session_wire_depth` 那一族读的全是
    /// 包头里对端**自称**的位深——发送侧把包头写对、载荷写错时它们一条都不会红
    /// （包头是 `Codec::for_depth(fmt.depth)`，与 `encode_pcm_into` 用的是同一个
    /// `fmt`，但那两处可以各自被改坏）。
    fn rx_payload_bytes(&self) -> Option<u64> {
        crate::snapshot_sessions(self.h.inner_for_test())
            .iter()
            .find_map(|e| e.rx.as_ref().map(|rx| lk(&rx.stats).rx.summary(1.0).bytes))
    }

    /// 这条 daemon 上任意一条接收流的 JB 已经交付出去多少**帧**（10 ms 音频）。
    fn jb_popped(&self) -> Option<u64> {
        crate::snapshot_sessions(self.h.inner_for_test())
            .iter()
            .find_map(|e| e.rx.as_ref().map(|rx| lk(&rx.jbs).jb.popped))
    }

    /// **每 10 ms 音频对应多少载荷字节** —— 由真实字节数导出，与包头无关。
    ///
    /// 拿 `jb.popped`（交付出去的帧数）而不是墙钟做分母：这样测量窗口里的调度
    /// 抖动被自动归一掉，判据变成「同样一段音频，线上花了多少字节」——那正是
    /// 位深这一维**唯一**的物理含义。
    ///
    /// 48 kHz 三档的期望值是 960 / 1440 / 1920，两两相差 1.5× 与 2×，
    /// ±15 % 的容差下三个区间互不重叠。
    fn wire_bytes_per_frame(&self, window: Duration) -> Option<f64> {
        let (b0, p0) = (self.rx_payload_bytes()?, self.jb_popped()?);
        std::thread::sleep(window);
        let (b1, p1) = (self.rx_payload_bytes()?, self.jb_popped()?);
        let frames = p1.checked_sub(p0)?;
        // 窗口里至少要有半秒音频，否则分母太小、一个包的取样错位就能歪 10 %。
        if frames < 50 {
            return None;
        }
        Some(b1.checked_sub(b0)? as f64 / frames as f64)
    }

    /// 伺服导出的**运行时证据**（`daemon.status.latency_guard.servo`）。
    ///
    /// 走的是 IPC 而不是直接读 `inner.servo_obs`：这一整块的存在意义就是
    /// 「现场能不能看见」，而现场只有 IPC 一条路。直接读结构体的测试会在
    /// `latency_guard_status` 忘记挂这个键时照样全绿。
    fn servo(&self) -> Value {
        self.ok(methods::DAEMON_STATUS, json!({}))
            .get("latency_guard")
            .and_then(|g| g.get("servo"))
            .cloned()
            .expect("daemon.status.latency_guard.servo 必须存在")
    }

    /// This daemon's live tier 1 media links (M8). Empty = every peer is on
    /// tier 0, which is a fact and not a missing reading.
    ///
    /// Lives under `latency_guard` beside `media_send_q`: both answer the same
    /// question — is the send side falling behind, and by how much — and the
    /// tier 1 stale gate is ratchet governance, which is what that block is.
    fn tcp_media(&self) -> Vec<Value> {
        self.ok(methods::DAEMON_STATUS, json!({}))
            .get("latency_guard")
            .and_then(|g| g.get("tcp_media"))
            .and_then(|v| v.as_array().cloned())
            .expect("daemon.status.latency_guard.tcp_media 必须存在")
    }

    fn tcp_link(&self) -> Option<Value> {
        self.tcp_media().into_iter().next()
    }

    /// This daemon's live tier 2 multiplexed connections (M8 P5).
    ///
    /// Beside `tcp_media` rather than inside it: the media half of a mux is a
    /// `tcp_media` row (same queue, same stale gate, same two numbers), and
    /// what is only here is the control-frame accounting — the one observable
    /// that tells a multiplexed connection from two separate ones.
    fn mux_links(&self) -> Vec<Value> {
        self.ok(methods::DAEMON_STATUS, json!({}))
            .get("latency_guard")
            .and_then(|g| g.get("mux"))
            .and_then(|v| v.as_array().cloned())
            .expect("daemon.status.latency_guard.mux 必须存在")
    }

    fn mux_link(&self) -> Option<Value> {
        self.mux_links().into_iter().next()
    }

    /// The address the **control channel's own socket** reports for a peer.
    ///
    /// On tier 2 this is the tunnel's, not the peer's, and that is the point:
    /// see `a_tier_two_pair_survives_the_source_address_being_lost`.
    fn conn_peer_addr(&self, fp: &str) -> Option<std::net::SocketAddr> {
        let c = lk(&self.h.inner_for_test().state).conns.get(fp).cloned()?;
        let addr = lk(&c.chan).peer_addr().ok();
        addr
    }

    fn control_port(&self) -> u16 {
        self.h.control_port
    }

    /// The last measured control-plane round trip, in milliseconds. `None`
    /// until a `Pong` has come back.
    fn rtt_ms(&self, fp: &str) -> Option<f64> {
        self.peer(fp)["rtt_ms"].as_f64()
    }

    /// One recv-side session's tone verdict, or `None` while none has been
    /// formed yet.
    fn recv_verdict(&self) -> Option<Value> {
        self.ok(methods::SESSION_LIST, json!({}))
            .as_array()?
            .iter()
            .find(|s| s["dir"].as_str() == Some("recv"))
            .and_then(|s| s["stats"]["verdict"].as_object().cloned())
            .map(Value::Object)
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

/// Pin a peer's connectivity tier (M8). **Must be called before the control
/// connection exists**: `tcpmedia::negotiate` runs once, inside `register_conn`,
/// so a tier set afterwards is a tier that only takes effect on the next
/// connection — which is the intended product semantics (design §5.1: never
/// switch transports inside a live stream) and therefore also the shape a test
/// has to respect.
///
/// Goes straight at `inner.peer_transport` rather than through IPC because P3
/// has no IPC verb for it: manual pinning is a developer/test affordance until
/// P4 gives it an automatic path and the UI a control.
fn pin_tier(n: &Node, fp: &str, tier: &str) {
    let inner = n.h.inner_for_test();
    let mut store = lk(&inner.peer_transport);
    let mut t = store.get(fp);
    t.transport_tier = tier.to_string();
    store.set(fp, t);
    assert_eq!(
        store.tier(fp),
        crate::peer_transport::TransportTier::parse(tier).expect("a tier this build knows"),
        "the tier did not stick; every assertion downstream would be about tier 0"
    );
}

fn pair(a: &Node, b: &Node) {
    pair_through(a, b, &b.addr());
}

/// Pair and connect A to B **at `addr`**, which need not be B's own address.
fn pair_through(a: &Node, b: &Node, addr: &str) {
    let pin = b.ok(methods::PAIRING_ENABLE, json!({ "ttl_s": 60 }));
    let pin = pin.get("pin").and_then(Value::as_str).expect("pin").to_string();
    a.ok(methods::PEERS_PAIR, json!({ "addr": addr, "pin": pin }));
    a.ok(
        methods::PEERS_CONNECT,
        json!({ "peer": b.fingerprint(), "addr": addr }),
    );
}

/// Set a peer's dial policy (M8 P5), the same way `pin_tier` sets its tier and
/// for the same reason: before the control connection exists.
fn set_dial_policy(n: &Node, fp: &str, policy: &str) {
    let inner = n.h.inner_for_test();
    let mut store = lk(&inner.peer_transport);
    let mut t = store.get(fp);
    t.dial_policy = policy.to_string();
    store.set(fp, t);
    assert_eq!(
        store.dial_policy(fp),
        crate::peer_transport::DialPolicy::parse(policy).expect("a policy this build knows"),
        "the dial policy did not stick"
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

    // `LADDER` = [48k/32f, 48k/24, 48k/16, 32k/16, 24k/16, 16k/16] ⇒ 格号 0..5。
    // **深档也要走一遍**：只测老四档的话，「新档被 `.min(3)` 静默钳回去」这个
    // 地雷会原样活下来（那正是位深进阶梯之前 `engine.rs` 里写死的那个字面量）。
    for (id, want_rung) in [
        ("pcm16k16", RUNG_16K),
        ("pcm24k16", RUNG_24K),
        ("pcm32k16", RUNG_32K),
        ("pcm48k16", RUNG_48K_16),
        ("pcm48k24", 1u32),
        ("pcm48k32f", 0),
    ] {
        a.set_transport(&b.fingerprint(), "send", "quality", id);
        eventually(&format!("tx rung to become {want_rung} for {id}"), || {
            a.tx_rung() == Some(want_rung)
        });
        // ⚠ **`tx_rung()` 一个人证明不了线上真的变了。** 它读的是 `TxShared.rung`
        // 那个原子量，而 `tx_loop` 在**读它之后**还有一道钳位
        // （`.min(LADDER.len()-1)`）。钳位写死成字面量时（位深进阶梯之前正是
        // `.min(3)`），这个原子量照样是 5，而线上跑的是格号 3 ——
        // 只断言原子量的测试对那颗地雷**完全免疫**（实测：把钳位改回 `.min(3)`，
        // 只有本断言的版本照样绿）。
        //
        // 所以判据落在**对端从包头读出来的东西**上：那是线路的事实。
        let want = audiohub_net::media::rung_format(want_rung);
        eventually(&format!("b 从包头读出 {id} 的格式"), || {
            b.session_wire_rate() == Some(want.rate_hz)
                && b.session_wire_depth().as_deref() == Some(want.depth.as_str())
        });
    }
}

/// **接线 ②：档位 id 里的那个数 == `session.list` 报出去的采样率。**
///
/// 上一条只证明格号跟着动，它证明不了**界面上那个数是对的**——格号是个内部序号
/// （`pcm48k` ⇒ 0），用户从没见过它。用户见到的是 `SessionInfo.sample_rate` 与
/// `QualityStats.wire_rate_hz`，而这两个此前都不可信：
///
///   - `sample_rate` 是**硬编码的 48000**，无论阶梯掉到哪一档都写 48000
///     （统计页那格「采样率 48000 Hz」因此恒为真、恒不含信息）；
///   - 一级界面那一格显示的是 `bandwidth_hz`（= 采样率/2），于是用户设 `pcm48k`
///     却读到 24 kHz，2026-08-04 据此报「设置没生效」。
///
/// 所以这一条比的是**同一个数**：`pcmNNk` 里的 NN，与 IPC 报出去的 Hz/1000。
/// 顺带钉死带宽恰是它的一半、且**两者不相等**——它们在界面上都写作 kHz。
///
/// 缺陷注入对照（都实跑过）：
///   1. `sample_rate: wire_rate.unwrap_or(0)` 改回 `48000` ⇒ 三档红在「采样率」；
///   2. `wire_rate_hz` 改成 `bandwidth_hz`（少乘 2）⇒ 四档全红；
///   3. `bandwidth_hz: last_rate / 2` 改成 `last_rate` ⇒ 红在「带宽是一半」与
///      「两者不相等」。
#[test]
fn the_number_in_the_quality_stop_id_is_the_number_ipc_reports() {
    let (a, b) = linked("qunits");
    let _id = tone_session(&a, &b);
    eventually("a to have a sending stream", || a.tx_rung().is_some());

    for (id, khz) in [
        ("pcm16k16", 16u32),
        ("pcm24k16", 24),
        ("pcm32k16", 32),
        ("pcm48k16", 48),
        ("pcm48k24", 48),
        ("pcm48k32f", 48),
    ] {
        a.set_transport(&b.fingerprint(), "send", "quality", id);
        let want = khz * 1000;
        eventually(&format!("session.list 的 sample_rate 变成 {want} （档 {id}）"), || {
            a.session_wire_rate() == Some(want)
        });
        // 正向对照：上面那条 `eventually` 若因为**根本没有会话**而恒为 None，
        // 它会超时而不是通过；这里再取一次并断言，好让失败信息带上实际值。
        assert_eq!(
            a.session_wire_rate(),
            Some(want),
            "档 {id} 的线上采样率不是 {want}——界面上那个数与用户设的那个数对不上",
        );
    }

    // 音质原料那一份（一级界面读 `wire_rate_hz`、明细读 `bandwidth_hz`）。
    // 它只在**接收**侧算得出来，所以看 b 那台：a 在发，b 在收。
    a.set_transport(&b.fingerprint(), "send", "quality", "pcm32k16");
    eventually("b 侧音质原料里的采样率变成 32000", || {
        b.session_quality_rate() == Some((32_000, 16_000))
    });
    let (rate, bw) = b.session_quality_rate().expect("b 侧应当有音质读数");
    assert_eq!(rate, 32_000, "音质那一格的采样率与档位对不上");
    assert_eq!(bw, rate / 2, "带宽必须恰是采样率的一半（奈奎斯特）");
    assert_ne!(
        rate, bw,
        "采样率与带宽成了同一个数——两者在界面上都写作 kHz，混同即 2026-08-04 那次误读",
    );
}

/// **接线 ③：位深真的走上了线，而且是收方从包头读出来的。**
///
/// 这一条是位深进阶梯的承重测试。它比「格号变了」严格得多：格号是发送侧的
/// 内部序号，`sample_rate` 在三个 48 kHz 档上**完全相同** —— 只有位深这一个
/// 维度会变。若线上仍然发 s16 而只有设置里写着 24 bit，
/// 这条会红在「收方读出来的位深」。
///
/// # ⚠ 只读包头是不够的——这条测试自己曾栽在这里
///
/// 下面第 1、3 条注入改的是包头 / 格号，收方从包头读出的位深因此变了 ⇒ 红。
/// 但**镜像的那次注入是绿的**：把 `encode_pcm_into` 的位深参数改成 `S16`
/// 而**包头保持诚实**（`Codec::for_depth(fmt.depth)` 一字不动），于是包头写
/// 「s24」、载荷是 s16 字节 —— 整条判据链（`c.last_depth` ← `h.codec.wire_depth()`
/// → `SessionInfo.wire_depth` / `QualityStats.wire_depth`）**全部只读包头**，
/// 一条断言都不会红，而声音是废的：收方按 s24 解 480 B 得 160 样本，
/// `160×2 ≠ 480` ⇒ 分包重组整个失效，每 10 ms 只交付 320 样本，
/// 界面三处却一致显示「48 kHz · 24 bit」。
///
/// ⇒ 必须有一条**由真实载荷字节导出**的断言，见下面的 `wire_bytes_per_frame`。
///
/// 缺陷注入对照（都实跑过）：
///   1. `tx_loop` 的 `Codec::for_depth(fmt.depth)` 改回 `Codec::PcmS16le`
///      ⇒ 红：深档收到的位深仍是 s16；
///   2. `tx_loop` 的 `encode_pcm_into(..., fmt.depth, ...)` 改成写死
///      `WireDepth::S16`（包头不动）⇒ 红在**字节账**那一条，且只红在那一条；
///   3. `engine.rs` 的钳位 `.min(LADDER.len()-1)` 改回 `.min(3)`
///      ⇒ 红：`pcm16k16`（格号 5）被钳成格号 3，采样率报 32000。
#[test]
fn the_bit_depth_really_travels_on_the_wire() {
    let (a, b) = linked("qdepth");
    let _id = tone_session(&a, &b); // a 发 -> b 收
    eventually("a to have a sending stream", || a.tx_rung().is_some());
    eventually("b to have a receiving stream", || b.jb_target().is_some());

    // 三个 **48 kHz** 档：采样率一模一样，只有位深不同。
    // 这正是「只看采样率的测试对位深完全免疫」的那一组。
    // `want_bytes` = 每 10 ms 音频的**明文载荷字节数** = 480 样本 × 位深宽度。
    for (id, want_depth, want_bytes) in [
        ("pcm48k16", "s16", 960.0f64),
        ("pcm48k24", "s24", 1440.0),
        ("pcm48k32f", "f32", 1920.0),
    ] {
        a.set_transport(&b.fingerprint(), "send", "quality", id);
        eventually(&format!("b 从包头读出的位深变成 {want_depth}（档 {id}）"), || {
            b.session_wire_depth().as_deref() == Some(want_depth)
        });
        assert_eq!(
            b.session_wire_rate(),
            Some(48_000),
            "档 {id} 的采样率不是 48000 —— 三个深档的采样率必须完全相同",
        );
        assert_eq!(
            b.session_wire_depth().as_deref(),
            Some(want_depth),
            "档 {id} 的线上位深不是 {want_depth}",
        );

        // ---- 字节账：位深生效的唯一硬证据 --------------------------------
        //
        // 上面三条读的都是对端**自称**的位深。这一条数的是真的落地的字节，
        // 与包头没有任何关系：同样一段音频（分母是交付出去的帧数），
        // 16 / 24 / 32 位分别要花 960 / 1440 / 1920 字节。
        let mut got = None;
        eventually(&format!("档 {id} 的载荷字节账稳定下来"), || {
            got = b.wire_bytes_per_frame(Duration::from_millis(800));
            got.is_some_and(|v| (v - want_bytes).abs() < want_bytes * 0.15)
        });
        let got = got.expect("窗口里必须有音频，否则这条断言等于没测");
        assert!(
            (got - want_bytes).abs() < want_bytes * 0.15,
            "档 {id}（{want_depth}）每帧实测 {got:.0} 字节，应为 {want_bytes:.0} —— \
             线上字节数与包头声明的位深对不上，声音是废的而界面三处都显示对的",
        );
        // 音质原料那一份也要带上位深，且**与 SessionInfo 的那一份一致**。
        // 两处不一致 = 界面上两格各说各话，而没有任何一处会报错。
        eventually("音质原料里的位深跟上", || {
            b.quality_wire_depth().as_deref() == Some(want_depth)
        });
        // 发送侧自己也报得出来（它的真值源是格号，不是包头）。
        assert_eq!(a.session_wire_depth().as_deref(), Some(want_depth), "发送侧的位深读数不对");
    }

    // 低采样率档必须仍是 16 位：阶梯上不存在「低采样率 + 高位深」的组合。
    a.set_transport(&b.fingerprint(), "send", "quality", "pcm16k16");
    eventually("回到 16 kHz / 16 bit", || {
        b.session_wire_rate() == Some(16_000) && b.session_wire_depth().as_deref() == Some("s16")
    });
}

/// **The send side reports the payload bytes it actually put on the wire, and
/// they track the bit depth.**
///
/// # The hole this closes
///
/// `SessionStats::wire_bytes` is documented as the one hard piece of evidence
/// that a bit depth took effect — and on the send side it was **never assigned**
/// (`snapshot_sessions` wrote it only in the `rx` branch). Every `spk/send`
/// session reported a constant `0`, while the field's own doc comment promised
/// it was differenceable proof. `the_bit_depth_really_travels_on_the_wire`
/// covers the same ground for the **receive** side only, so nothing caught this.
///
/// The assertion is derived from bytes, not from the header: the numerator is
/// the payload counter incremented where `send_to` returned `Ok`, and the
/// denominator is frames. A build that wrote an honest header over a
/// wrongly-encoded payload still fails here.
///
/// Injection checks (both run, see the report):
///   1. drop `s.wire_bytes = payload_total` from the tx branch of
///      `snapshot_sessions` (restoring the old behaviour) => red on "stayed 0";
///   2. make `udp_send_loop` count `slot.buf.len()` into `sent_payload_bytes`
///      instead of `slot.payload_len` => red on the byte account, because the
///      per-frame figure picks up the framing overhead.
#[test]
fn the_send_side_reports_the_payload_bytes_it_put_on_the_wire() {
    let (a, b) = linked("txbytes");
    let _id = tone_session(&a, &b); // a sends -> b receives
    eventually("a to have a sending stream", || a.tx_rung().is_some());

    // Positive control first: the counter must actually move. Without this, a
    // build that reports a constant 0 would sail through every ratio below
    // (0/n == 0/m), which is exactly the shape of the defect being fixed.
    eventually("a's send-side wire_bytes to become non-zero", || {
        a.stat_u64("wire_bytes").is_some_and(|v| v > 0)
    });

    for (id, want_depth, want_bytes) in [
        ("pcm48k16", "s16", 960.0f64),
        ("pcm48k24", "s24", 1440.0),
        ("pcm48k32f", "f32", 1920.0),
    ] {
        let depth = audiohub_core::dsp::WireDepth::parse(want_depth).expect("known depth");
        let want_rung = audiohub_net::media::rung_of(48_000, depth).expect("48 kHz rung");
        a.set_transport(&b.fingerprint(), "send", "quality", id);
        eventually(&format!("the wire to settle on {want_depth} for {id}"), || {
            a.tx_rung() == Some(want_rung)
        });

        // ±4%, not the ±15% the receive-side test uses. The three rungs are
        // 1.5x apart so a loose band still separates them — but framing
        // overhead is only +5.8% (s16) to +7.8% (s24) of the payload, so a
        // ±15% band cannot tell payload bytes from whole datagrams. Measured
        // and confirmed: at ±15% an injected `sent_payload_bytes +=
        // slot.buf.len()` sailed through this test. The counters come from one
        // snapshot, so the residual noise is ~1 packet in 150 frames.
        const TOL: f64 = 0.04;
        let mut got = None;
        eventually(&format!("the send-side byte account to settle for {id}"), || {
            got = a.tx_wire_bytes_per_frame(Duration::from_millis(1500));
            got.is_some_and(|v| (v - want_bytes).abs() < want_bytes * TOL)
        });
        let got = got.expect("the window must contain audio, or this asserts nothing");
        assert!(
            (got - want_bytes).abs() < want_bytes * TOL,
            "rung {id} ({want_depth}): the sender put {got:.1} payload bytes on the wire per \
             10 ms frame, expected {want_bytes:.0}. Either the header says one thing and the \
             bytes say another, or the counter is measuring datagrams rather than payload.",
        );
    }
}

/// **Both ends count the same thing, and the datagram figure is the larger one.**
///
/// The send side used to divide whole datagrams by session age while the
/// receive side divided plaintext by session age, under one field name. The
/// same stream therefore read 1525 kbps on one machine and 1458 on the other —
/// exactly 56 B/packet of header and AEAD tag — and no display could notice,
/// because both numbers were individually plausible.
///
/// Injection check: set `s.wire_bytes = tx.sent_bytes...` (the datagram
/// counter) in the tx branch => red on the cross-machine payload comparison,
/// because the sender then reports ~6% more payload than the receiver saw.
#[test]
fn the_two_ends_agree_on_what_a_wire_byte_is() {
    let (a, b) = linked("bytecal");
    let _id = tone_session(&a, &b); // a sends -> b receives
    eventually("a to have a sending stream", || a.tx_rung().is_some());
    eventually("b to have a receiving stream", || b.jb_target().is_some());
    a.set_transport(&b.fingerprint(), "send", "quality", "pcm48k24");
    eventually("both sides to be carrying payload", || {
        a.stat_u64("wire_bytes").is_some_and(|v| v > 100_000)
            && b.stat_u64("wire_bytes").is_some_and(|v| v > 100_000)
    });

    let (tx_pay, tx_dg) = (a.stat_u64("wire_bytes").unwrap(), a.stat_u64("datagram_bytes").unwrap());
    let (rx_pay, rx_dg) = (b.stat_u64("wire_bytes").unwrap(), b.stat_u64("datagram_bytes").unwrap());

    // Same numerator on both sides: whatever the sender calls payload, the
    // receiver decrypts the same count. Loss is possible, so the receiver may
    // trail; it must not *exceed*, and it must not trail by a framing-sized gap.
    assert!(rx_pay <= tx_pay, "the receiver saw more payload ({rx_pay}) than was sent ({tx_pay})");
    let shortfall = (tx_pay - rx_pay) as f64 / tx_pay as f64;
    assert!(
        shortfall < 0.02,
        "the two ends disagree on what `wire_bytes` counts: sender {tx_pay}, receiver {rx_pay} \
         ({:.1}% apart). A gap this size is a caliber difference (header + tag), not packet loss.",
        shortfall * 100.0,
    );

    // The datagram figure is strictly bigger on both sides: header + AEAD tag
    // are real bytes. Equality means one of the two is being reported twice.
    assert!(
        tx_dg > tx_pay,
        "send side: datagram_bytes ({tx_dg}) must exceed wire_bytes ({tx_pay}) by the framing"
    );
    assert!(
        rx_dg > rx_pay,
        "recv side: datagram_bytes ({rx_dg}) must exceed wire_bytes ({rx_pay}) by the framing"
    );
}

/// **`bitrate_kbps` follows the rung — on BOTH directions, within seconds.**
///
/// This is the reported contradiction, made executable. The user switched the
/// three 48 kHz rungs on a live `spk/send` session and read 1469.2 / 1464.3 /
/// 1467.0 kbps where the truth was 768 / 1152 / 1536. The bytes on the wire were
/// correct all along; the metric was a lifetime average and could not move.
///
/// So the assertion is not "the number is right once" but "**the three rungs are
/// separated**", which a lifetime average can never satisfy no matter how long
/// the test waits. The tolerance is 15%; the rungs are 1.5x and 2x apart, so the
/// three admissible bands do not overlap.
///
/// Injection check: change `s.bitrate_kbps` back to the lifetime form
/// (`payload_total * 8.0 / age`) on either side => red, since after the first
/// rung the average is pinned and the second rung's band is unreachable.
#[test]
fn the_reported_bitrate_follows_the_rung_in_both_directions() {
    let (a, b) = linked("brate");
    let _id = tone_session(&a, &b); // a sends -> b receives
    eventually("a to have a sending stream", || a.tx_rung().is_some());
    eventually("b to have a receiving stream", || b.jb_target().is_some());

    // Burn some session time on a rung that is NOT one of the three under test.
    // A lifetime average over a session that only ever ran the rung being
    // measured would look correct; this is what makes the average detectable.
    a.set_transport(&b.fingerprint(), "send", "quality", "pcm16k16");
    eventually("the priming rung to take", || a.tx_rung() == Some(RUNG_16K));
    std::thread::sleep(Duration::from_secs(3));

    for (id, want_kbps) in [("pcm48k16", 768.0f64), ("pcm48k24", 1152.0), ("pcm48k32f", 1536.0)] {
        a.set_transport(&b.fingerprint(), "send", "quality", id);
        for (who, node) in [("sender", &a), ("receiver", &b)] {
            eventually_within(
                Duration::from_secs(20),
                &format!("{who}'s bitrate_kbps to reach the {id} band"),
                || {
                    node.stat_f64("bitrate_kbps")
                        .is_some_and(|v| (v - want_kbps).abs() < want_kbps * 0.15)
                },
            );
            let got = node.stat_f64("bitrate_kbps").expect("a rate must be readable by now");
            assert!(
                (got - want_kbps).abs() < want_kbps * 0.15,
                "{who} reports {got:.1} kbps on rung {id}, expected ~{want_kbps:.0}. \
                 A reading that will not move between rungs 2x apart is a lifetime average, \
                 and it is what made three different wire formats read as one number.",
            );
        }
    }
}

/// **A stale quality id is refused at the RPC, and the wire does not move.**
///
/// The compatibility layer this replaces silently translated `pcm32k` to
/// `pcm32k16`. That translation had to be mirrored in the frontend, one of the
/// three read paths there forgot to apply it, and the same stored value then
/// rendered as `pcm32k` on the overview and "PCM 32 kHz - 16 bit" on the detail
/// page — a real regression manufactured by the compatibility code itself.
///
/// So the contract is now: unknown id in, error out, wire untouched. Resetting
/// a *stored* value to the default happens on load and is reported to the UI
/// (`peer_transport::StoredDir::sanitize`); an explicit set of an unknown id is
/// simply refused, because there is no user intent to preserve.
///
/// Injection check: put the translation back in `QualityTarget::parse`
/// (`if s == "pcm32k" { s = "pcm32k16" }`) and this goes red on "was accepted".
#[test]
fn a_stale_quality_id_is_refused_and_leaves_the_wire_alone() {
    let (a, b) = linked("qlegacy");
    let _id = tone_session(&a, &b);
    eventually("a to have a sending stream", || a.tx_rung().is_some());

    // Park on a known rung first, so "the wire did not move" is a real claim
    // rather than a comparison against an unknown starting point.
    a.set_transport(&b.fingerprint(), "send", "quality", "pcm48k24");
    eventually("the wire to settle on 48 kHz / s24", || {
        b.session_wire_rate() == Some(48_000) && b.session_wire_depth().as_deref() == Some("s24")
    });

    for old in ["pcm", "pcm48k", "pcm32k", "pcm24k", "pcm16k"] {
        let r = a.call(
            methods::PEERS_SET_TRANSPORT,
            json!({ "peer": b.fingerprint(), "dir": "send", "quality": old }),
        );
        assert!(
            r.is_err(),
            "stale id {old} was accepted; the silent translation is back and the UI can \
             once again draw a stop the daemon never executed"
        );
    }

    // Nothing the refusals touched: still the rung we parked on.
    assert_eq!(b.session_wire_rate(), Some(48_000), "a refused set still moved the wire");
    assert_eq!(
        b.session_wire_depth().as_deref(),
        Some("s24"),
        "a refused set still moved the wire"
    );
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

    a.set_transport(&b.fingerprint(), "send", "quality", "pcm16k16");
    eventually("the fixed rung to take", || a.tx_rung() == Some(RUNG_16K));

    // 跨过好几个 ticker 周期。阶梯若还在跑，干净链路会把格号一路升回天花板。
    let deadline = Instant::now() + Duration::from_secs(4);
    while Instant::now() < deadline {
        assert_eq!(
            a.tx_rung(),
            Some(RUNG_16K),
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

    // 用 32 kHz（天花板下面一格）而不是 16 kHz（最低一格）：`AutoLadder` 每升
    // 一格要 10 个干净周期 ≈ 10 s，从最低格升回天花板要三次共 ~30 s。
    // 测的是「阶梯是否重新掌权」，一格足以证明，三格只是在等。
    a.set_transport(&b.fingerprint(), "send", "quality", "pcm32k16");
    eventually("the fixed rung to take", || a.tx_rung() == Some(RUNG_32K));
    assert_eq!(
        a.tx_quality_rung(),
        Some(RUNG_32K),
        "固定档没有被推给音频线程"
    );

    a.set_transport(&b.fingerprint(), "send", "quality", "auto");
    assert_eq!(
        a.tx_quality_rung(),
        None,
        "切回 AUTO 之后固定档必须**立刻**撤销——等下一拍就是一段说不清归谁管的时间"
    );
    // 干净回环上阶梯会把格号升回**它的天花板**（10 个干净周期）。这是「阶梯
    // 真的重新在写 `tx.rung`」的唯一证据：只断言 `quality_rung() == None` 的话，
    // 一个把阶梯永久停掉的实现照样绿。
    //
    // ⚠ 目标是 `AUTO_TOP_RUNG` 而**不是 0**：AUTO 不许自己走进深档
    // （那会让所有 AUTO 用户的带宽静默翻倍）。写 0 的版本会在这里超时——
    // 这条断言同时是那条纪律的守门人。
    eventually_within(
        Duration::from_secs(30),
        "the ladder to promote back to its ceiling",
        || a.tx_rung() == Some(audiohub_net::media::AUTO_TOP_RUNG),
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
    a.set_transport(&b.fingerprint(), "send", "latency", "100");
    eventually_within(
        Duration::from_secs(20),
        "the buffer to be seeded at the 100ms depth",
        || b.jb_target().map_or(false, |t| t >= 8),
    );
    let envelope = b.jb_envelope().expect("envelope");

    // 20 ms 档：`max(ceil(20/10)+2, 12) == 12`，与上一档**同一个包络**。
    a.set_transport(&b.fingerprint(), "send", "latency", "20");
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
    a.set_transport(&b.fingerprint(), "send", "latency", "0");
    eventually("the buffer to be driven shallow", || {
        b.jb_target().map_or(false, |t| t <= 2)
    });
    let shallow = b.jb_target().expect("target");

    // 高档：伺服往上加。500 ms 远高于回环链路的地板，所以必须一路加深。
    a.set_transport(&b.fingerprint(), "send", "latency", "500");
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

    a.set_transport(&b.fingerprint(), "send", "latency", "750");
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
    a.set_transport(&b.fingerprint(), "send", "latency", "auto");
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

    a.set_transport(&b.fingerprint(), "send", "latency", "auto");
    // 给 ticker 几拍。
    std::thread::sleep(Duration::from_millis(2500));
    assert_eq!(
        b.rx_servo_frames(),
        None,
        "AUTO 下伺服写了深度：两条回路在抢同一个水位"
    );
}

// ------------------------------------------------------ 拒绝 / 契约

/// **Opus 三档在滑条上看得见，`peers.set_transport` 必须拒收。**
///
/// 收下它 = 界面显示「Opus 128k」而线上一个字节都没变。
/// 顺带断言**拒绝之后盘上的值没被改动**：一次被拒的写入不许留下半个副作用。
#[test]
fn an_unimplemented_quality_rung_is_refused_and_changes_nothing() {
    let (a, b) = linked("refuse");
    let fp = b.fingerprint();
    let before = a.peer(&fp)["transport"]["send"]["quality"]
        .as_str()
        .expect("send.quality")
        .to_string();

    for bad in ["opus64", "opus128", "opus256", "pcm96k", "", "PCM48K"] {
        let err = a
            .call(
                methods::PEERS_SET_TRANSPORT,
                json!({ "peer": &fp, "dir": "send", "quality": bad }),
            )
            .expect_err(&format!("quality '{bad}' 本 build 给不了，必须拒收"));
        assert!(
            err.contains("quality"),
            "拒绝理由要说清楚是哪个字段的问题：{err}"
        );
    }
    assert_eq!(
        a.peer(&fp)["transport"]["send"]["quality"].as_str(),
        Some(before.as_str()),
        "一次被拒的写入改动了盘上的值"
    );
    assert_eq!(a.tx_quality_rung(), None, "被拒的档位泄漏到了音频线程");
}

/// 档位表以外的毫秒数同样拒收，**不是就近吸附**。
#[test]
fn a_latency_value_off_the_ladder_is_refused() {
    let (a, b) = linked("refuse-lat");
    let fp = b.fingerprint();
    for bad in ["137", "1", "1001", "-5", "auto ", "min2"] {
        let err = a
            .call(
                methods::PEERS_SET_TRANSPORT,
                json!({ "peer": &fp, "dir": "recv", "latency": bad }),
            )
            .expect_err(&format!("latency '{bad}' 不是档位，必须拒收"));
        assert!(err.contains("latency"), "{err}");
    }
    // 每一个真档位都要收得下——否则上面的拒绝只是「什么都不接受」。
    for &ms in &audiohub_ipc::LATENCY_STOPS_MS {
        let got = a.set_transport(&fp, "recv", "latency", &ms.to_string());
        assert_eq!(
            got["recv"]["latency"].as_str(),
            Some(ms.to_string().as_str()),
            "{ms} ms 是档位表里的档，却没被收下"
        );
    }
    // 旧拼写要被**规范化**存下来，不是原样留着：盘上留两种写法会让下一个
    // 读者以为是两档。
    let got = a.set_transport(&fp, "recv", "latency", "min");
    assert_eq!(got["recv"]["latency"].as_str(), Some("0"), "旧的 \"min\" 要被规范化成 \"0\"");

    // **方向必须说清楚。** 缺 `dir` 时挑一个默认方向去写，就是替用户决定了
    // 「他改的是收还是发」——而那两件事的执行器在不同的机器上。
    a.call(methods::PEERS_SET_TRANSPORT, json!({ "peer": &fp, "latency": "100" }))
        .expect_err("缺 dir 必须报错，不许挑一个默认方向");
    a.call(
        methods::PEERS_SET_TRANSPORT,
        json!({ "peer": &fp, "dir": "in", "latency": "100" }),
    )
    .expect_err("dir 只有 recv/send 两个取值（UI 的 in/out 是它自己的事）");
}

/// plan §15：`settings.set` 收到 `latency` / `quality` 必须**报错**，
/// 不是静默忽略。
///
/// 静默忽略正是本项目栽过六次的那个形状——上一次的原话是
/// 「`settings.latency` 从未被读过」。一个还在用旧 API 的脚本必须**立刻**
/// 知道自己在对着空气说话。
#[test]
fn the_old_global_stops_are_refused_rather_than_silently_ignored() {
    let a = Node::start("gone");
    for (k, v) in [("latency", "300"), ("quality", "pcm32k16")] {
        let err = a
            .call(methods::SETTINGS_SET, json!({ k: v }))
            .expect_err(&format!("settings.set 不该再收 '{k}'"));
        assert!(
            err.contains(methods::PEERS_SET_TRANSPORT),
            "报错要指出新入口在哪：{err}"
        );
    }
    // 契约里也不该再有这两个键。
    let view = a.ok(methods::SETTINGS_GET, json!({}));
    assert!(view.get("latency").is_none(), "settings.get 还在报 latency");
    assert!(view.get("quality").is_none(), "settings.get 还在报 quality");
    assert!(
        view.get("transport_live").is_none(),
        "站点级 transport_live 双向拆开后没有指代对象，必须消失"
    );
    // 档**表**留下——档表是能力，档位是选择，两件事不该一起搬。
    assert!(view["latency_stops_ms"].is_array(), "档表被误删了");
    assert!(view["quality_stops"].is_array(), "档表被误删了");
}

/// `settings.get` 必须把**档位表**发出去：前端不许自己写一份。
/// 两边各存一份，分歧不会有任何报错——只会有一个选不中的档。
#[test]
fn the_settings_view_carries_the_ladders() {
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

    for (dir, key, v) in [
        ("send", "latency", "200"),
        ("send", "quality", "pcm24k16"),
        ("recv", "latency", "300"),
        ("recv", "quality", "pcm32k16"),
        ("send", "latency", "auto"),
        ("send", "quality", "auto"),
    ] {
        a.set_transport(&b.fingerprint(), dir, key, v);
        assert_eq!(count(), before, "{dir}.{key}={v} 关掉了已有会话");
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
        wire_rate_hz: 48_000, // 线上采样率：与带宽差 2 倍，是两个数
        wire_depth: "s16".to_string(),
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

/// **重启之后每对端的固定档仍然在盘上、并且被执行。**
///
/// 「盘上存着 pcm16k、回显 pcm16k、跑的是 AUTO」是这一整轮要消灭的形态，
/// 而重启是它最容易复发的地方。plan §15 之后持久化换了文件
/// （`peer_transport.json`），语义必须一字不改地保住。
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
        // Production and every test but the tier 2 starvation rig: whatever
        // the environment says (normally nothing, i.e. unlimited).
        tx_throttle_kbps: None,
    };

    // 一台真对端：`peers.set_transport` 要解析指纹，没有配对就没有指纹。
    let peer = Node::start("restart-peer");
    peer.set_mode(Mode::Share);
    let peer_fp = peer.fingerprint();
    let peer_addr = format!("127.0.0.1:{}", peer.h.control_port);

    let first = start_daemon(cfg()).expect("start");
    let call = |h: &DaemonHandle, m: &str, p: &Value| {
        ipcserv::dispatch_for_test(h.inner_for_test(), m, p).expect("call")
    };
    call(&first, methods::SETTINGS_SET, &json!({ "mode": "a" }));
    let pin = peer.ok(methods::PAIRING_ENABLE, json!({ "ttl_s": 60 }));
    let pin = pin["pin"].as_str().expect("pin").to_string();
    call(&first, methods::PEERS_PAIR, &json!({ "addr": peer_addr, "pin": pin }));
    call(
        &first,
        methods::PEERS_SET_TRANSPORT,
        &json!({ "peer": &peer_fp, "dir": "recv", "latency": "300", "quality": "pcm24k16" }),
    );
    call(
        &first,
        methods::PEERS_SET_TRANSPORT,
        &json!({ "peer": &peer_fp, "dir": "send", "latency": "100", "quality": "pcm32k16" }),
    );
    first.shutdown();

    let second = start_daemon(cfg()).expect("restart");
    let list = ipcserv::dispatch_for_test(second.inner_for_test(), methods::PEERS_LIST, &json!({}))
        .expect("peers.list");
    let p = list
        .as_array()
        .expect("array")
        .iter()
        .find(|p| p["fingerprint"].as_str() == Some(peer_fp.as_str()))
        .expect("peer survived the restart");
    assert_eq!(p["transport"]["recv"]["latency"].as_str(), Some("300"), "重启后收·延迟丢了");
    assert_eq!(p["transport"]["recv"]["quality"].as_str(), Some("pcm24k16"), "重启后收·音质丢了");
    assert_eq!(p["transport"]["send"]["latency"].as_str(), Some("100"), "重启后发·延迟丢了");
    assert_eq!(p["transport"]["send"]["quality"].as_str(), Some("pcm32k16"), "重启后发·音质丢了");
    // **盘上真的有这个文件**——只断言回显的话，一个把值留在内存里的实现
    // 在同一个进程内照样全绿。
    let raw = std::fs::read_to_string(dir.join("peer_transport.json")).expect("peer_transport.json");
    assert!(raw.contains("300") && raw.contains("pcm24k16"), "档位没落盘：{raw}");
    second.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

/// **解除配对把这台对端的档位一并清掉。**
///
/// 留着的话，重新配对同一台机器会静默继承上一段关系的档位——
/// 「我明明没设过 300」的又一种成因。
#[test]
fn unpairing_forgets_the_transport_choices_too() {
    let (a, b) = linked("forget");
    let fp = b.fingerprint();
    a.set_transport(&fp, "recv", "latency", "300");
    assert_eq!(a.peer(&fp)["transport"]["recv"]["latency"].as_str(), Some("300"));

    a.ok(methods::PEERS_UNPAIR, json!({ "peer": &fp }));
    // 重新配对同一台。
    pair(&a, &b);
    assert_eq!(
        a.peer(&fp)["transport"]["recv"]["latency"].as_str(),
        Some(audiohub_ipc::LATENCY_AUTO),
        "重新配对之后继承了上一段关系的档位"
    );
}

// -------------------------------------------------- 写入路径与运行时证据

/// **契约表上的每一个键，`settings.set` 都必须真的照做。**
///
/// 这是 [`audiohub_ipc::SETTINGS_WRITABLE_KEYS`] 的 daemon 半边；CLI 半边在
/// `audiohub-cli` 的 `ctl::tests::every_writable_setting_is_reachable_from_the_command_line`。
/// 两边合起来才盖得住那个真实缺口：**后端读得对、前端点得动、而命令行连
/// flag 都没有**，两侧测试各自全绿。
///
/// 判据是「写进去的值能被读回来」，不是「调用没报错」——一个把参数整个忽略
/// 的实现照样返回 200 OK。
#[test]
fn every_writable_setting_key_is_really_honoured() {
    let a = Node::start("keys");
    // 每个键给一对「与默认不同」的取值，逐个写、逐个读回。
    let cases: &[(&str, Value, &dyn Fn(&Value) -> Value)] = &[
        (
            "mode",
            json!("a"),
            &(|v: &Value| v.get("mode").cloned().unwrap_or(Value::Null)) as &dyn Fn(&Value) -> Value,
        ),
        (
            "remove_virtual_on_disconnect",
            json!(true),
            &|v: &Value| v.get("remove_virtual_on_disconnect").cloned().unwrap_or(Value::Null),
        ),
        (
            "mark_offline_devices",
            json!(false),
            &|v: &Value| v.get("mark_offline_devices").cloned().unwrap_or(Value::Null),
        ),
        (
            "latency",
            json!("200"),
            &|v: &Value| v.get("latency").cloned().unwrap_or(Value::Null),
        ),
        (
            "quality",
            json!("pcm32k16"),
            &|v: &Value| v.get("quality").cloned().unwrap_or(Value::Null),
        ),
    ];
    for key in audiohub_ipc::SETTINGS_WRITABLE_KEYS {
        let (_, want, read) = cases
            .iter()
            .find(|(k, _, _)| k == key)
            .unwrap_or_else(|| panic!("契约表里有 '{key}'，这条测试却没有覆盖它"));
        let got = a.ok(methods::SETTINGS_SET, json!({ *key: want.clone() }));
        assert_eq!(&read(&got), want, "settings.set 收下了 '{key}' 却没有照做");
        // 再从**一次独立的读**里确认，排除「回包是把请求原样抄回来」。
        let re = a.ok(methods::SETTINGS_GET, json!({}));
        assert_eq!(&read(&re), want, "'{key}' 只体现在回包里，没有真的存下来");
    }
}

/// **伺服必须导出「它此刻在做什么」，而且那份读数要随时间前进。**
///
/// 用户实测的形态是：设了 200 ms，卡片纹丝不动，而 `settings.get` 里
/// **一个字段都没有**能说明回路死没死。`latency_guard` 当时只有棘轮那几项。
///
/// 这条测的是**心跳**：没有会话时伺服照样每秒跑一拍（`servo_pass` 在
/// `ticker_loop` 里是无条件的），于是 `ticks` 必须涨。它把「回路没在跑」
/// 与「回路在跑但这台机器没有可控对象」分开——两者的下一步完全不同。
#[test]
fn the_servo_exports_a_heartbeat_even_with_no_sessions() {
    let a = Node::start("obs-idle");
    let first = a.servo();
    let t0 = first["ticks"].as_u64().expect("ticks 必须是个数");
    eventually_within(Duration::from_secs(6), "the servo tick counter to advance", || {
        a.servo()["ticks"].as_u64().unwrap_or(0) > t0
    });
    let now = a.servo();
    assert_eq!(
        now["streams"].as_u64(),
        Some(0),
        "没有会话时必须如实说 0 条流——档位此刻没有作用对象"
    );
    assert!(
        now["by_stream"].as_object().map_or(false, |m| m.is_empty()),
        "没有会话时 by_stream 必须是空对象：{now}"
    );
    // **顶层不许再有 `target_ms` / `sum_ms` / `jb_frames`。** 留一个「代表值」
    // 就是 plan §14 裁定 1 那个「每卡一个数字、不管取哪条都在替另一条撒谎」
    // 的 JSON 版本。读旧路径的人应当拿到 null 而不是一个静默错误的数。
    for gone in ["target", "target_ms", "sum_ms", "jb_frames", "want_frames", "closed_loop"] {
        assert!(
            now.get(gone).is_none(),
            "站点级 servo 还在报 `{gone}`：它在双向、多流下没有指代对象"
        );
    }
    assert_eq!(
        now["bad_transport_targets"].as_u64(),
        Some(0),
        "干净启动不该有任何被拒的外来档位"
    );
}

/// **没有接收流时，固定档也不许报出「执行量」——哪怕一帧。**
///
/// 这条是本轮的运行时读数自己抓出来的缺陷，形态值得原样记下来。加了
/// `latency_guard.servo` 之后，在一台**只发不收**的 daemon 上（使用端发 spk，
/// 接收方的 JB 在对端）设 200 ms，现场是这样的：
///
/// ```text
///   t  ticks moves target  jb want step str
///  0.0    97     3    200   4    5    1   0
///  2.1    99     5    200   4    5    1   0     ← moves 每秒 +1，永不停
///  4.2   101     7    200   4    5    1   0
/// ```
///
/// 那个 `jb=4` 是 `jb_frames.unwrap_or(lo)` 造出来的，不是任何一个真实缓冲的
/// 深度。回路拿它当「现状」、算出「想到 5」、于是每一拍都记一次「动了一帧」。
/// 读数看上去是一条正在收敛的活回路，而这台机器上根本没有可被伺服的东西。
///
/// **这比没有读数更坏**：它会让下一个排障的人认定回路是好的，然后去别处找病。
///
/// 判据必须是 `moves` 在一段时间里**一次都不涨**，而不是「某一拍 step 为 0」——
/// 后者在上面那张表里也成立过（如果恰好采样在动作之间）。
#[test]
fn a_daemon_with_nothing_to_servo_reports_no_movement_rather_than_a_phantom_one() {
    // `linked` 里 a 是使用端、b 是共享端；a 开一条 spk 把音**发**给 b。
    // 于是 **a 只有 tx、没有任何接收流**，正是用户那台使用端的形状。
    let (a, b) = linked("obs-nostream");
    let _id = tone_session(&a, &b);
    eventually("b to have a receiving stream", || b.jb_target().is_some());
    assert_eq!(
        a.jb_target(),
        None,
        "这条测试的前提是 a 没有接收流；前提不成立的话底下的 0 是白拿的"
    );

    // 使用端 a 设 `send.latency`：执行器在 **b** 的 JB 上，a 这台机器上一个
    // 都没有。plan §15 之前这里是 `a.settings.set(latency)`，于是 a 自己的
    // 全局回路会拿一个**造出来的** `jb=4` 当现状，每拍记一次「动了一帧」。
    a.set_transport(&b.fingerprint(), "send", "latency", "200");
    // 先等**对端**把新目标看进去，否则「a 这侧没动」可能只是因为消息还没到。
    eventually("b to adopt the pushed target", || {
        b.rx_servo()["target_ms"].as_u64() == Some(200)
    });
    let m0 = a.servo_moves();
    let t0 = a.servo()["ticks"].as_u64().expect("ticks");

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let s = a.servo();
        assert_eq!(
            a.servo_moves(),
            m0,
            "这台 daemon 上没有任何接收流，伺服却在记「又动了一帧」：{s}"
        );
        assert!(
            s["by_stream"].as_object().map_or(false, |m| m.is_empty()),
            "只发不收的机器上出现了接收流的伺服条目：{s}"
        );
        assert_eq!(s["streams"].as_u64(), Some(0));
        std::thread::sleep(Duration::from_millis(200));
    }
    // 正向对照：心跳照旧。否则上面那串 0 也可能只是因为回路整个停了——
    // 而「停了」与「在跑但没对象」正是这套读数要分开的两件事。
    assert!(
        a.servo()["ticks"].as_u64().unwrap_or(0) > t0,
        "回路根本没在跑，上面那些「没动」的断言什么都没证明"
    );
    // 而**同一时刻**真的有接收流的那一侧照旧走到位——否则这条测试可能只是
    // 证明了「我们把伺服整个关掉了」。
    eventually_within(
        Duration::from_secs(25),
        "the side that DOES have a buffer to still reach the target",
        || {
            b.rx_servo()["jb_frames"]
                .as_u64()
                .map_or(false, |f| (18..=22).contains(&f))
        },
    );
    // 而且那条目必须标明**这个目标是对端推来的**，不是 b 自己设的。
    assert_eq!(
        b.rx_servo()["target_from"].as_str(),
        Some("peer"),
        "共享端把一个被要求的档位报成了自己设的：{}",
        b.rx_servo()
    );
}

/// **设了 200 ms 之后，daemon 侧真的朝 200 走——不是「字段等于 200」。**
///
/// 这条是本轮的验收断言，形状按用户的原话定：延迟档是**端到端总延迟的目标**，
/// 设 200 就该主动往上填。所以断言分三层，缺一层都能被一个假实现骗过：
///
///   1. **目标被回路看见**：`servo.target_ms == 200`。只有这一层的话，
///      一个只写 `settings.json` 的实现就能通过——正是本轮之前的状态。
///   2. **回路在动**：`ticks` 前进、`moves` 前进。只有 1+2 的话，一个记了
///      账却不驱动执行器的实现能通过。
///   3. **执行器真的走到位**：`jb_frames`（从**真实 JB** 上读回来的深度）
///      落在 200 ms 对应的 20 帧附近。这一层才是「朝 200 动」。
///
/// **第 3 层不区分「换档预置」与「伺服逐帧走」**，两条路都到得了 20 帧，
/// 这条测试也不试图分开它们——用户要的是「设 200 就到 200」，两条路都算数。
/// 把它们隔开的是 `the_servo_output_is_really_executed_not_just_the_reseed`
/// （在共用同一个包络的两档之间切换，`reshape` 早退 ⇒ 只剩伺服能动深度）。
/// 缺陷注入 I4 已经证明一条测试盖不住两段。
///
/// # 为什么判据落在 `jb_frames` 而不是 `sum_ms`
///
/// 无设备的回环里输出尾级 `rate == 0`，按「绝不用 0 填补」的规矩 `local_ms`
/// 整体是 `None` ⇒ `sum_ms` 恒 `None` ⇒ 回路走**开环预置**（`closed_loop:false`）。
/// 那不是缺陷，是测量纪律的正确结果，而且开环下唯一能被观测的执行量就是
/// JB 深度。闭环收敛由 `servo::tests::the_loop_converges_onto_the_total_not_onto_a_buffer_size`
/// 与 `a_moving_floor_is_re_solved_every_tick_not_cached` 两条纯函数测试覆盖。
/// **这条测试因此顺带断言 `closed_loop == false`**：如果哪天它变成 true，
/// 说明测量条件变了，上面那句解释也就该重写了。
#[test]
fn setting_two_hundred_milliseconds_really_walks_the_daemon_toward_two_hundred() {
    let (a, b) = linked("obs200");
    let _id = tone_session(&a, &b);
    // 两级正向对照：先有接收流，再有真的音频进 JB。少了第二级，底下所有关于
    // 深度的断言都会因为「根本没有包」这同一个原因通过或失败。
    eventually("b to have a receiving stream", || b.jb_target().is_some());
    eventually("media to actually reach b's jitter buffer", || {
        b.jb_pushes().map_or(false, |n| n > 20)
    });

    let ticks0 = b.servo()["ticks"].as_u64().expect("ticks");
    let moves0 = b.servo_moves();

    a.set_transport(&b.fingerprint(), "send", "latency", "200");

    // 层 1：回路看见了这个目标。
    eventually("the servo to adopt the 200ms target", || {
        b.rx_servo()["target_ms"].as_u64() == Some(200)
    });

    // 层 3：执行器走到位。20 帧 = 200 ms；给 ±2 帧，因为死区是半帧、
    // 而 JB 自己的欠载惩罚也会在附近浮动。
    eventually_within(
        Duration::from_secs(25),
        "the real jitter buffer to reach the 200ms depth",
        || {
            b.rx_servo()["jb_frames"]
                .as_u64()
                .map_or(false, |f| (18..=22).contains(&f))
        },
    );

    let site = b.servo();
    let now = b.rx_servo();
    // 层 2：这段时间里回路确实在跑、确实动过手。
    assert!(
        site["ticks"].as_u64().unwrap_or(0) > ticks0,
        "伺服没有再跑过一拍，上面那些数字全是陈的：{site}"
    );
    assert!(
        b.servo_moves() > moves0,
        "伺服一次都没有改变过深度，深度却到位了 —— 那是别人干的：{now}"
    );
    assert_eq!(now["target"].as_str(), Some("200"));
    assert_eq!(
        site["streams"].as_u64(),
        Some(1),
        "有一条接收流在被伺服，读数却不是 1：{site}"
    );
    assert_eq!(
        now["closed_loop"].as_bool(),
        Some(false),
        "回环里没有真实输出设备，`sum_ms` 应当测不出来。这里变成 true 说明测量\
         条件变了（好事），但本测试的解释与判据要跟着重写"
    );
    // 开环下**绝不宣布**「已达物理下限 / 上限」：那两句是关于物理的断言，
    // 而开环把地板当成了 0。
    assert_eq!(now["at_floor"].as_bool(), Some(false));
    assert_eq!(now["at_ceiling"].as_bool(), Some(false));
    // 包络必须装得下 200 ms，否则「到位」只是撞在了天花板上。
    let env = now["envelope_frames"].as_array().expect("envelope_frames");
    assert!(
        env[1].as_u64().unwrap_or(0) >= 20,
        "包络上限装不下 200 ms 的目标：{now}"
    );

    // 反方向：拖回 50 ms，深度必须**下来**。只测一个方向的话，一个「一路加深」
    // 的实现（比如把目标当成了下限）照样全绿。
    a.set_transport(&b.fingerprint(), "send", "latency", "50");
    eventually_within(
        Duration::from_secs(25),
        "the buffer to come back down for the 50ms target",
        || {
            b.rx_servo()["jb_frames"]
                .as_u64()
                .map_or(false, |f| (3..=7).contains(&f))
        },
    );
    assert_eq!(b.rx_servo()["target_ms"].as_u64(), Some(50));
}

/// **AUTO 与「回路死了」在读数上必须能分开。**
///
/// 两者的界面表现完全一样（深度不随滑条动），下一步动作却相反：一个是
/// 「本来就该这样」，另一个是「去查 ticker」。分不开的读数等于没有读数。
#[test]
fn auto_is_distinguishable_from_a_dead_loop_in_the_readout() {
    let (a, b) = linked("obsauto");
    let _id = tone_session(&a, &b);
    eventually("b to have a receiving stream", || b.jb_target().is_some());

    a.set_transport(&b.fingerprint(), "send", "latency", "auto");
    let t0 = b.servo()["ticks"].as_u64().expect("ticks");
    eventually_within(Duration::from_secs(6), "the loop to keep ticking under AUTO", || {
        b.servo()["ticks"].as_u64().unwrap_or(0) > t0
    });
    let now = b.rx_servo();
    assert_eq!(now["target"].as_str(), Some(audiohub_ipc::LATENCY_AUTO));
    assert_eq!(
        now["step_frames"].as_i64(),
        Some(0),
        "AUTO 下伺服不许提出任何执行量：那一档归抖动公式管"
    );
    assert!(
        b.servo()["streams"].as_u64().unwrap_or(0) >= 1,
        "AUTO 也要如实报有几条流可控，否则「没在跑」与「没对象」还是分不开"
    );
}

/// **两个档位的作用对象是相反方向的流，读数必须分开报。**
///
/// 一台只发不收的使用端（模式 A 把系统声音送去对端扬声器）：本机没有任何
/// jitter buffer（延迟档在这台机器上没有执行器），而线上确有一条流、采样率
/// 也确实随质量滑条在变。
///
/// plan §15 之前这两件事被一个站点级 `transport_live` 糊在一起，UI 的质量
/// 读数照着 `streams` 说「当前没有正在传输的音频流」——而实测同一时刻
/// `pcm16k16/24k16/32k16/48k16` 分别把线上格号打到 5/4/3/2。一个正在生效的设置被
/// 界面说成「没有作用对象」，在用户那里与「设置没生效」是同一件事。
///
/// 拆成按流之后判据换成**执行器本身**：`SessionStats.latency_target` 只在有
/// 接收流的那一侧非空，`quality_target` 只在有发送流的那一侧非空。
#[test]
fn the_two_stops_report_the_streams_they_can_actually_act_on() {
    let (a, b) = linked("dirs");
    let _id = tone_session(&a, &b); // a 发 -> b 收
    eventually("b to have a receiving stream", || b.jb_target().is_some());
    eventually("a to have a sending stream", || a.tx_rung().is_some());

    let fp = b.fingerprint();
    a.set_transport(&fp, "send", "quality", "pcm32k16"); // 执行器在 a 的 tx
    a.set_transport(&fp, "send", "latency", "200"); // 执行器在 b 的 rx

    let stats = |n: &Node| n.ok(methods::SESSION_LIST, json!({}))[0]["stats"].clone();

    eventually("a's send stream to carry the quality target", || {
        stats(&a)["quality_target"].as_str() == Some("pcm32k16")
    });
    let sa = stats(&a);
    assert!(
        sa["latency_target"].is_null(),
        "a 没有接收流，延迟档在这台机器上没有执行器，却报出了目标：{sa}"
    );
    assert_eq!(sa["target_from"].as_str(), Some("local"), "a 是消费者，档位是自己设的");

    eventually("b's receive stream to carry the pushed latency target", || {
        stats(&b)["latency_target"].as_str() == Some("200")
    });
    let sb = stats(&b);
    assert!(
        sb["quality_target"].is_null(),
        "b 没有发送流，质量档在这台机器上没有执行器，却报出了目标：{sb}"
    );
    assert_eq!(
        sb["target_from"].as_str(),
        Some("peer"),
        "b 是提供者，这个 200 是被要求的，不是它自己设的：{sb}"
    );

    // 反向对照：两台机器读到的**不是同一对数字**。上面四条若都由同一个来源
    // 供给，这一条会红。
    assert_ne!(
        (sa["latency_target"].clone(), sa["quality_target"].clone()),
        (sb["latency_target"].clone(), sb["quality_target"].clone()),
        "两端读到了同一对档位：说明这两个字段其实还是同一个来源"
    );
}

// ------------------------------------ plan §15：交叉的那半边（承重四条）
//
// 这四条一起才能证明四个旋钮各自接到了**正确的执行器**上。任取三条通过、
// 第四条失败，都说明有一个方向的旋钮接错了端——而那种错误的自然表现是
// 「设了、存了、回显了，媒体面一个字节没变」，界面全绿。
//
// | 用户设的 | 执行器在 | 走线 |
// |---|---|---|
// | `recv.latency` | 本机 rx 的 JB | 本地 |
// | `recv.quality` | **对端** tx 的阶梯 | 推 |
// | `send.latency` | **对端** rx 的 JB | 推 |
// | `send.quality` | 本机 tx 的阶梯 | 本地 |

/// **承重 ①：`send.latency` 跨到对端，本机一动不动。**
///
/// `linked` 里 a 发 -> b 收，所以 JB 在 b 上。a 设 `send.latency` 必须治到
/// b 那条流；a 自己没有接收流，`by_stream` 必须一直是空的。
#[test]
fn the_send_latency_lands_on_the_peers_buffer_and_nowhere_local() {
    let (a, b) = linked("x-sendlat");
    let _id = tone_session(&a, &b);
    // 两级正向对照：先有接收流，再有真的音频进 JB。少了第二级，底下关于深度的
    // 断言会因为「根本没有包」这同一个原因通过或失败。
    eventually("b to have a receiving stream", || b.jb_target().is_some());
    eventually("media to actually reach b's jitter buffer", || {
        b.jb_pushes().map_or(false, |n| n > 20)
    });
    let depth0 = b.jb_target().expect("jb target");

    a.set_transport(&b.fingerprint(), "send", "latency", "300");
    eventually("the peer's buffer to adopt the pushed target", || {
        b.rx_servo()["target_ms"].as_u64() == Some(300)
    });
    assert_eq!(b.rx_servo()["target_from"].as_str(), Some("peer"));

    // **判据落在真实 JB 的深度上，不是「字段被写了」。**
    // 300 ms = 30 帧；给 ±3 帧（死区半帧 + JB 自己的欠载惩罚会在附近浮动）。
    // 只断言 `target_ms == 300` 的话，一个「收下了、记了账、没驱动执行器」的
    // 实现照样全绿——本项目栽过六次的正是那个形状。
    eventually_within(
        Duration::from_secs(25),
        "the peer's REAL jitter buffer to walk to the 300ms depth",
        || b.jb_target().map_or(false, |t| (27..=33).contains(&t)),
    );
    assert!(
        b.jb_target().expect("jb") > depth0,
        "缓冲深度一动没动（起点 {depth0} 帧）：目标被存下来了，执行器没被驱动"
    );

    assert!(
        a.servo()["by_stream"].as_object().map_or(false, |m| m.is_empty()),
        "本机出现了接收流的伺服条目——延迟档被误留在了本地"
    );
}

/// **承重 ②：`recv.latency` 只治本机，一个字节都不上线。**
///
/// 这条要一条**本机在收**的流：b 是共享端，让它开不了；改成让 a 取 b 的
/// 麦克风（`kind=mic`）⇒ a 有 rx、b 有 tx。
#[test]
fn the_recv_latency_stays_home_and_is_never_pushed() {
    let (a, b) = linked("x-recvlat");
    a.ok(
        methods::SESSION_OPEN,
        json!({ "peer": b.fingerprint(), "kind": "mic", "source": SOURCE_TONE, "freq": 1000.0 }),
    );
    eventually("a to have a receiving stream", || a.jb_target().is_some());

    a.set_transport(&b.fingerprint(), "recv", "latency", "300");
    eventually("the local buffer to adopt the target", || {
        a.rx_servo()["target_ms"].as_u64() == Some(300)
    });
    assert_eq!(
        a.rx_servo()["target_from"].as_str(),
        Some("local"),
        "本机自己设的档位被报成了对端推来的"
    );
    // b 那侧没有接收流，也不该收到任何被拒的外来档位。
    assert!(
        b.servo()["by_stream"].as_object().map_or(false, |m| m.is_empty()),
        "`recv.latency` 被推到了对端：那边没有 rx，它无处执行"
    );
    assert_eq!(
        b.servo()["bad_transport_targets"].as_u64(),
        Some(0),
        "对端收到了一个它执行不了的档位——交叉的那半边接反了"
    );
}

/// **承重 ③：`send.quality` 只改本机的线上采样率。**
#[test]
fn the_send_quality_acts_on_the_local_sender_only() {
    let (a, b) = linked("x-sendq");
    let _id = tone_session(&a, &b); // a 有 tx
    eventually("a to have a sending stream", || a.tx_rung().is_some());

    a.set_transport(&b.fingerprint(), "send", "quality", "pcm16k16");
    eventually("the local sender to move to the 16 kHz rung", || a.tx_rung() == Some(RUNG_16K));
    assert_eq!(a.tx_quality_rung(), Some(RUNG_16K));
    assert_eq!(b.tx_quality_rung(), None, "对端没有发送流，档位却落到了它身上");
    assert_eq!(b.servo()["bad_transport_targets"].as_u64(), Some(0));
}

/// **承重 ④：`recv.quality` 跨到对端的发送侧。**
///
/// 这是四条里最容易被写反的一条：它长得像「接收方向」的设置，执行器却在
/// **对端的发送**上——因为「我要收到多好的音质」只有发的人做得到。
#[test]
fn the_recv_quality_lands_on_the_peers_sender() {
    let (a, b) = linked("x-recvq");
    a.ok(
        methods::SESSION_OPEN,
        json!({ "peer": b.fingerprint(), "kind": "mic", "source": SOURCE_TONE, "freq": 1000.0 }),
    );
    eventually("b to have a sending stream", || b.tx_rung().is_some());

    a.set_transport(&b.fingerprint(), "recv", "quality", "pcm16k16");
    eventually("the peer's sender to move to the 16 kHz rung", || b.tx_rung() == Some(RUNG_16K));
    assert_eq!(b.tx_quality_rung(), Some(RUNG_16K));
    assert_eq!(
        a.tx_quality_rung(),
        None,
        "本机没有发送流，`recv.quality` 却留在了本地——它在这里无处执行"
    );
}

/// **先设档位、后开流：新建的流必须带着已有的档位起跑。**
///
/// 这条钉的是「档位灌入点必须**每拍**都跑」，而不是只在变更时跑一次。
/// 变更有三个入口（IPC、`SetTransport`、开流），而流可以在**任意时刻**出现
/// （模式 B 的设备协调器、断线重放）。只在变更时灌的话，一条**在变更之后才
/// 建立**的流会带着默认档位一路跑到下一次变更为止——一个只在特定时序下出现、
/// 且没有任何报错的失效。
///
/// 缺陷注入对照：把 ticker 里那句 `publish_targets(&inner, &entries);` 删掉。
/// 其余每一条传输测试都照旧全绿（它们都是「先开流、后设档位」），只有这条红。
#[test]
fn a_stream_opened_after_the_stops_were_set_starts_with_them_in_force() {
    let (a, b) = linked("late-open");
    // **先**设，此刻一条流都没有 —— 于是这两个档位没有任何作用对象。
    a.set_transport(&b.fingerprint(), "recv", "latency", "300");
    a.set_transport(&b.fingerprint(), "send", "quality", "pcm16k16");
    assert!(
        crate::snapshot_sessions(a.h.inner_for_test()).is_empty(),
        "这条测试的前提是设档位时还没有流；有流的话它测的就是别的东西了"
    );

    // **后**开：一条本机收（mic）+ 一条本机发（spk），两个执行器各一个。
    a.ok(
        methods::SESSION_OPEN,
        json!({ "peer": b.fingerprint(), "kind": "mic", "source": SOURCE_TONE, "freq": 1000.0 }),
    );
    tone_session(&a, &b);

    eventually("the freshly opened receive stream to carry the stored target", || {
        a.rx_servo()["target_ms"].as_u64() == Some(300)
    });
    eventually("the freshly opened send stream to carry the stored quality", || {
        a.tx_quality_rung() == Some(RUNG_16K)
    });
}

/// **多对端隔离：改 A 的档位不许碰 B 的伺服输出。**
///
/// 这条盯的是 `generation` 那条「档位真变了才作废旧输出」的逻辑：写成全局的
/// 话，改 A 对端的档位会把 B 对端的伺服输出一起清零，B 那条链路会掉回抖动
/// 公式一拍——一个只在多对端同时在线时出现、且没有任何报错的失效。
#[test]
fn changing_one_peers_stops_leaves_the_other_peers_loop_untouched() {
    let consumer = Node::start("iso-c");
    consumer.set_mode(Mode::A);
    let p1 = Node::start("iso-p1");
    p1.set_mode(Mode::Share);
    let p2 = Node::start("iso-p2");
    p2.set_mode(Mode::Share);
    pair(&consumer, &p1);
    pair(&consumer, &p2);
    tone_session(&consumer, &p1);
    tone_session(&consumer, &p2);
    eventually("p1 to have a receiving stream", || p1.jb_target().is_some());
    eventually("p2 to have a receiving stream", || p2.jb_target().is_some());

    // 两台都先走到 200，于是「没变」不会是「本来就没设过」的同义词。
    for p in [&p1, &p2] {
        consumer.set_transport(&p.fingerprint(), "send", "latency", "200");
    }
    for p in [&p1, &p2] {
        eventually_within(
            Duration::from_secs(25),
            "both peers to adopt 200",
            || p.rx_servo()["target_ms"].as_u64() == Some(200),
        );
    }
    // **等 p2 收敛完再取基线。** 收敛途中 `moves` 每拍都在涨，那时取的基线只是
    // 「它还在走」，底下的比较测不出任何东西（负载高时更是随机通过 / 随机失败）。
    let mut p2_moves0 = p2.servo_moves();
    eventually_within(Duration::from_secs(30), "p2's loop to settle", || {
        std::thread::sleep(Duration::from_millis(1500));
        let now = p2.servo_moves();
        let settled = now == p2_moves0;
        p2_moves0 = now;
        settled
    });

    // 只改 p1。
    consumer.set_transport(&p1.fingerprint(), "send", "latency", "500");
    eventually_within(Duration::from_secs(25), "p1 to adopt 500", || {
        p1.rx_servo()["target_ms"].as_u64() == Some(500)
    });
    assert_eq!(
        p2.rx_servo()["target_ms"].as_u64(),
        Some(200),
        "改 p1 的档位把 p2 的目标也改了：单例没拆干净"
    );
    // p2 的回路**不该因为 p1 换档而被清零重来**。`generation` 写成全局时
    // `servo_frames` 会被清 0，下一拍从抖动公式重新起步 ⇒ 一条已经收敛的链路
    // 会重新走一段，`moves` 跟着跳。基线已经是收敛值，所以这里的容差只需要
    // 覆盖「恰好在这两拍之间自然微调了一次」。
    let jumped = p2.servo_moves().saturating_sub(p2_moves0);
    assert!(jumped <= 1, "p2 的伺服被 p1 的换档惊动了 {jumped} 次（基线是收敛值）");
}

/// **断言 A：属于**另一条连接**的流，`SetTransport` 一律拒绝并计数。**
///
/// `stream_id` 在 daemon 内是**全局**的（媒体头里还是明文），只是按连接认领。
/// 不查归属的话，共享模式下同时服务两台机器时，其中一台可以去调另一台那条流
/// 的 jitter buffer——一次跨对端的静默篡改，被调的那一侧不会有任何报错。
///
/// 形状要害：注入用的 stream_id **必须是提供者真的持有的那一条**（属于另一条
/// 连接），不能是它压根没见过的号。后者被「查不到这条流」挡下，无论归属校验
/// 在不在都会被拒——那样的测试对着一个删掉了归属校验的实现照样全绿。
///
/// 注入走**真的加密控制通道**（`ConnShared::send_msg`），不是直接调处理函数：
/// 归属校验的位置在分发里，绕过分发去测它等于测了一个没人调用的函数。
#[test]
fn a_set_transport_for_another_connections_stream_is_refused_and_counted() {
    let provider = Node::start("own-p");
    provider.set_mode(Mode::Share);
    let c1 = Node::start("own-c1");
    c1.set_mode(Mode::A);
    let c2 = Node::start("own-c2");
    c2.set_mode(Mode::A);
    pair(&c1, &provider);
    pair(&c2, &provider);
    let victim = tone_session(&c2, &provider) as u32; // provider 上归 c2 的那条流
    eventually("the provider to have c2's receiving stream", || {
        provider.jb_target().is_some()
    });
    // c2 把它设到 300，于是「没变」是一个可观测的事实而不是「本来就没设过」。
    c2.set_transport(&provider.fingerprint(), "send", "latency", "300");
    eventually("c2's stream to adopt 300", || {
        provider.rx_servo()["target_ms"].as_u64() == Some(300)
    });
    // c1 也要有一条连接（pair 已建立），但没有会话——它无权碰 victim。
    let before = provider.servo()["bad_transport_targets"].as_u64().unwrap_or(0);

    c1.send_raw(
        &provider.fingerprint(),
        &audiohub_net::secure::SessionMsg::SetTransport {
            stream_id: victim,
            rx_latency: Some("1000".into()),
            tx_quality: None,
        },
    );
    eventually("the provider to count the cross-connection attempt", || {
        provider.servo()["bad_transport_targets"].as_u64().unwrap_or(0) > before
    });
    // 而 c2 那条真流**一个字都没变**：拒绝不该有副作用。
    // 多等几拍再断言——被采纳的话，伺服下一拍就会把 target_ms 改成 1000。
    std::thread::sleep(Duration::from_millis(2500));
    assert_eq!(
        provider.rx_servo()["target_ms"].as_u64(),
        Some(300),
        "另一条连接改掉了这条流的目标：{}",
        provider.rx_servo()
    );
}

/// **断言 C（§13 互斥）：处于使用端模式的机器拒绝一切外来档位，并且计数。**
///
/// 这条**理论上不可达**——§13 保证每条链路恰好一个消费者。所以它触发时是
/// 「互斥被击穿了」的唯一运行时证据，值一个计数器而不是一句 `debug_assert!`
/// （release 构建里后者什么都不是）。
///
/// 注入方式因此也必须绕开正常路径：直接在控制通道上发一条 `SetTransport`，
/// 模拟一个「不守 §13」的对端。这不是伪造数据，是伪造一个**行为不端的对端**
/// ——闸门存在的全部理由就是它。
///
/// 形状要害：被指挥的那台机器**必须真的持有这条流**（这里是它自己作为消费者
/// 开的那一条），否则归属校验会先把消息挡下，而这条测试就变成了断言 A 的重复
/// ——删掉 §13 闸门它照样全绿。
#[test]
fn a_consumer_mode_machine_refuses_pushed_stops_and_counts_them() {
    // provider = 共享端；consumer 处于模式 A 并**自己**开了一条发送流。
    // 于是 consumer 手上有一条它自己拥有的流，而它不该被任何人指挥。
    let provider = Node::start("gate-p");
    provider.set_mode(Mode::Share);
    let consumer = Node::start("gate-c");
    consumer.set_mode(Mode::A);
    pair(&consumer, &provider);
    let sid = tone_session(&consumer, &provider) as u32;
    eventually("the consumer to have a sending stream", || consumer.tx_rung().is_some());
    // 正向对照：这条流此刻跑在 AUTO 上（阶梯当家），固定档为 None。
    assert_eq!(consumer.tx_quality_rung(), None);
    let before = consumer.servo()["bad_transport_targets"].as_u64().unwrap_or(0);

    // 提供者反过来指挥消费者：这正是 §13 不允许的方向。
    provider.send_raw(
        &consumer.fingerprint(),
        &audiohub_net::secure::SessionMsg::SetTransport {
            stream_id: sid,
            rx_latency: None,
            tx_quality: Some("pcm16k16".into()),
        },
    );
    eventually("the refusal to be counted", || {
        consumer.servo()["bad_transport_targets"].as_u64().unwrap_or(0) > before
    });
    assert_eq!(
        consumer.tx_quality_rung(),
        None,
        "一台使用端接受了对端塞过来的档位：§13 的互斥线被击穿了"
    );
}

// ------------------------------------------------------- M8 tier 1 (TCP media)

/// **Acceptance 1 (design §6, P3): two real daemons, pinned to tier 1, carry a
/// 1 kHz tone over TCP.**
///
/// The unit tests in `tcpmedia.rs` prove the queue, the stale gate and the
/// completion rule against a fake sink. None of them proves the thing that has
/// gone wrong five times in this repository: that the code is **wired in** —
/// that a ticket is minted, a second TCP connection is dialled and accepted,
/// that `ConnShared.media_path` really becomes `Tcp`, and that a stream opened
/// afterwards really sends its frames down it.
///
/// So the assertions are:
///   1. a live link exists **on both sides** (one connection, two ends);
///   2. the receiver's tone verdict detects 1 kHz with a healthy SNR — audio
///      crossed, not just bytes;
///   3. nothing was lost, because loopback TCP cannot lose anything;
///   4. **the frames the receiver read off the TCP link account for every
///      packet its session received.** This is the one that makes the test
///      about tier 1 rather than about "audio works": without it, a wiring bug
///      that silently left media on UDP would pass every other assertion.
///
/// Injection controls (both run 2026-08-07, both red as described):
///   - drop `tcpmedia::negotiate(inner, &conn)` from `register_conn` ⇒ times
///     out at (1), "a tier 1 media link on the dialling side".
///   - make `conn.rs`'s four `conn.current_media_path()` call sites pass
///     `MediaPath::Udp(..)` instead ⇒ **(1), (2) and (3) all still pass** — the
///     link is up, the tone arrives, nothing is lost — and (4) goes red on
///     "the sender never wrote a frame to the tier 1 link". That is the whole
///     reason (4) exists: it is the only assertion here that can tell a
///     working downgrade from a decorative one.
#[test]
fn two_daemons_pinned_to_tier_one_carry_a_tone_over_tcp() {
    let a = Node::start("t1-a");
    let b = Node::start("t1-b");
    a.set_mode(Mode::A);
    b.set_mode(Mode::Share);
    // Before pairing: `negotiate` runs inside `register_conn`, which the pair
    // flow reaches immediately.
    pin_tier(&a, &b.fingerprint(), "tier1");
    pin_tier(&b, &a.fingerprint(), "tier1");
    pair(&a, &b);

    // (1) one link, two ends.
    eventually("a tier 1 media link on the dialling side", || {
        a.tcp_link().is_some_and(|l| l["alive"] == Value::Bool(true))
    });
    eventually("a tier 1 media link on the accepting side", || {
        b.tcp_link().is_some_and(|l| l["alive"] == Value::Bool(true))
    });
    assert_eq!(
        a.tcp_link().expect("checked above")["fingerprint"].as_str(),
        Some(b.fingerprint().as_str()),
        "the link is attached to the wrong peer"
    );

    // A tone A -> B, with B verifying it.
    a.ok(
        methods::SESSION_OPEN,
        json!({
            "peer": b.fingerprint(), "kind": KIND_SPK, "source": SOURCE_TONE,
            "freq": 1000.0, "verify_freq": 1000.0
        }),
    );

    // (2) the audio arrived, and it is the audio that was sent.
    eventually_within(Duration::from_secs(20), "B's 1 kHz verdict", || {
        b.recv_verdict().is_some_and(|v| v["detected"] == Value::Bool(true))
    });
    let verdict = b.recv_verdict().expect("checked above");
    let snr = verdict["snr_db"].as_f64().expect("a detected verdict carries an SNR");
    assert!(snr >= 40.0, "1 kHz over loopback TCP should be clean, got {snr:.1} dB SNR");

    // (3) loopback TCP loses nothing.
    let sessions = b.ok(methods::SESSION_LIST, json!({}));
    let recv = sessions
        .as_array()
        .and_then(|ss| ss.iter().find(|s| s["dir"].as_str() == Some("recv")))
        .expect("B must have a receiving session")
        .clone();
    assert_eq!(recv["stats"]["lost"].as_u64(), Some(0), "TCP lost a packet: {recv}");
    let received = recv["stats"]["received"].as_u64().expect("a received count");
    assert!(received > 0, "the session reports no packets at all");

    // (4) ...and they came off the TCP link, not off the UDP socket.
    let a_link = a.tcp_link().expect("still attached");
    let b_link = b.tcp_link().expect("still attached");
    let written = a_link["frames_written"].as_u64().expect("a written count");
    let read = b_link["frames_read"].as_u64().expect("a read count");
    assert!(written > 0, "the sender never wrote a frame to the tier 1 link");
    assert!(
        read >= received,
        "the session counted {received} packets but only {read} came off the tier 1 link, so \
         the rest arrived over UDP — the downgrade is decorative"
    );
    assert_eq!(
        a_link["stale_dropped"].as_u64(),
        Some(0),
        "the stale gate fired on an idle loopback link, which means it is measuring the wrong \
         thing: there is nothing here for a frame to wait behind"
    );
    assert_eq!(
        b_link["unexpected_kind"].as_u64(),
        Some(0),
        "a non-media frame arrived on the tier 1 link"
    );
}

/// **A peer pinned to tier 0 refuses to be attached to, and says nothing on the
/// media socket.**
///
/// "Advertisement is not authorisation" (design decision C) has exactly one
/// concrete form on this path, and this is it. Without the check, one side
/// pinning tier 1 would drag the other onto TCP regardless of what its own
/// operator chose — and `plan.md` §16.2 says a manual override is always
/// available, in both directions.
#[test]
fn a_peer_pinned_to_tier_zero_refuses_the_attach() {
    let a = Node::start("t0-a");
    let b = Node::start("t0-b");
    a.set_mode(Mode::A);
    b.set_mode(Mode::Share);
    pin_tier(&a, &b.fingerprint(), "tier1"); // the dialler wants tier 1
    pin_tier(&b, &a.fingerprint(), "tier0"); // ...the peer does not
    pair(&a, &b);

    // A tone still has to work — a refused downgrade must leave tier 0 intact.
    a.ok(
        methods::SESSION_OPEN,
        json!({
            "peer": b.fingerprint(), "kind": KIND_SPK, "source": SOURCE_TONE,
            "freq": 1000.0, "verify_freq": 1000.0
        }),
    );
    eventually_within(Duration::from_secs(20), "B's 1 kHz verdict over UDP", || {
        b.recv_verdict().is_some_and(|v| v["detected"] == Value::Bool(true))
    });

    // Checked after the tone, not before: a link that takes a moment to appear
    // would make an immediate assertion pass for the wrong reason.
    assert!(a.tcp_media().is_empty(), "a tier 0 peer granted a media attach: {:?}", a.tcp_media());
    assert!(b.tcp_media().is_empty(), "a tier 0 peer accepted a media attach: {:?}", b.tcp_media());
}

/// **A stream opened the instant the peers are paired still goes over TCP.**
///
/// This is the production ordering, and the e2e test above does not exercise
/// it: that one waits for the link before opening, which is a courtesy no
/// caller in the daemon extends. `session.open` to a peer that is not connected
/// yet dials and opens back to back, and `reconnect::replay_sessions` re-opens
/// every stream the moment `connect_peer` returns.
///
/// Measured on 2026-08-08 before the fix, with exactly this shape: both ends
/// reported `alive: true`, the session counted 513 packets, and the link
/// reported `frames_written: 0` — every byte went over UDP. Attach finished at
/// t=0.310 and the control connection at t=0.112, so the window was ~200 ms,
/// which is not a race to lose occasionally but the normal case.
///
/// That mattered most on the path that closes the loop: `tcpmedia::serve`'s
/// teardown drops the control connection *on purpose*, so the reconnect+replay
/// machinery rebuilds both — and replay is precisely the caller that does not
/// wait. So every tier 1 link death used to rebuild its streams pinned back
/// onto UDP, permanently.
///
/// Injection control (run 2026-08-08): make `tcpmedia::negotiate` return
/// without calling `await_attach` ⇒ red at `frames_written == 0`, with the
/// tone still audible and both links still `alive` — which is the whole point.
#[test]
fn a_stream_opened_without_waiting_for_the_link_still_goes_over_tcp() {
    let a = Node::start("t1-race-a");
    let b = Node::start("t1-race-b");
    a.set_mode(Mode::A);
    b.set_mode(Mode::Share);
    pin_tier(&a, &b.fingerprint(), "tier1");
    pin_tier(&b, &a.fingerprint(), "tier1");
    pair(&a, &b);

    // No `eventually` on the link: open immediately, the way replay does.
    a.ok(
        methods::SESSION_OPEN,
        json!({
            "peer": b.fingerprint(), "kind": KIND_SPK, "source": SOURCE_TONE,
            "freq": 1000.0, "verify_freq": 1000.0
        }),
    );
    eventually_within(Duration::from_secs(20), "B's 1 kHz verdict", || {
        b.recv_verdict().is_some_and(|v| v["detected"] == Value::Bool(true))
    });

    let written = a
        .tcp_link()
        .and_then(|l| l["frames_written"].as_u64())
        .unwrap_or(0);
    assert!(
        written > 0,
        "the tone arrived but no frame was written to the tier 1 link, so it went over UDP: \
         a={:?} b={:?}",
        a.tcp_media(),
        b.tcp_media()
    );
    // Sampling order is load-bearing: both counters are still climbing, so
    // `received` must be read FIRST. The other way round the link reading is
    // the older of the two and this assertion goes red on a perfectly healthy
    // link — measured once, at 6015 >= 6025. A tolerance would be the wrong
    // repair; it would also hide a stream that really did leak onto UDP.
    let received = b
        .ok(methods::SESSION_LIST, json!({}))
        .as_array()
        .and_then(|ss| ss.iter().find(|s| s["dir"].as_str() == Some("recv")).cloned())
        .and_then(|s| s["stats"]["received"].as_u64())
        .expect("a received count");
    let read = b.tcp_link().and_then(|l| l["frames_read"].as_u64()).unwrap_or(0);
    assert!(
        read >= received,
        "{received} packets reached the session but only {read} came off the tier 1 link, so \
         some of the stream opened onto UDP"
    );
}

/// **A peer that refuses tier 1 says so, instead of letting the asker time
/// out.**
///
/// `negotiate` blocks `register_conn` until the attach resolves, so silence
/// from a peer that has already decided costs the asker the whole
/// `ATTACH_TIMEOUT`. That is the cost this assertion bounds: without
/// `SessionMsg::MediaAttachRefused`, `peers.connect` below returns after the
/// full backstop rather than after one round trip.
///
/// The bound is deliberately loose (2 s against an 8 s backstop). A tight one
/// would be measuring loopback scheduling, and this test is about which of two
/// exits was taken, not about how fast the fast one is.
#[test]
fn a_tier_zero_peer_refuses_out_loud_rather_than_by_timing_out() {
    let a = Node::start("t1-refuse-a");
    let b = Node::start("t1-refuse-b");
    a.set_mode(Mode::A);
    b.set_mode(Mode::Share);
    pin_tier(&a, &b.fingerprint(), "tier1");
    pin_tier(&b, &a.fingerprint(), "tier0");

    let pin = b.ok(methods::PAIRING_ENABLE, json!({ "ttl_s": 60 }));
    let pin = pin.get("pin").and_then(Value::as_str).expect("pin").to_string();
    a.ok(methods::PEERS_PAIR, json!({ "addr": b.addr(), "pin": pin }));
    let t0 = Instant::now();
    a.ok(
        methods::PEERS_CONNECT,
        json!({ "peer": b.fingerprint(), "addr": b.addr() }),
    );
    let took = t0.elapsed();

    assert!(a.tcp_media().is_empty(), "a tier 0 peer granted a media attach");
    assert!(
        took < Duration::from_secs(2),
        "connecting took {took:?}; a refusal that has to be inferred from a timeout costs the \
         whole attach backstop, which is what MediaAttachRefused exists to avoid"
    );
}

/// **A second media attach is refused while one is installed.**
///
/// The check it guards used to read `media_path`, release the lock, and let
/// `serve` install the link some microseconds later — so two attaches arriving
/// together could both pass and the second would overwrite the first's
/// `media_path`, leaving a writer nobody reads and no counter saying so.
///
/// Driven from outside the daemon, through a real socket and a real ticket, so
/// what is tested is the frame handler and not a function called by nobody.
///
/// ⚠ **What this test cannot cover**: `claim` also requires the attaching
/// socket's source IP to equal `conn.peer_ip`. On loopback every address is
/// 127.0.0.1, so a refusal and an acceptance are indistinguishable here; that
/// half is guarded on the source text instead, in `tcpmedia.rs`.
#[test]
fn a_second_media_attach_is_refused_while_one_is_installed() {
    use audiohub_net::control::{read_frame, write_frame, ControlMsg};
    use std::net::TcpStream;

    let a = Node::start("t1-dup-a");
    let b = Node::start("t1-dup-b");
    a.set_mode(Mode::A);
    b.set_mode(Mode::Share);
    pin_tier(&a, &b.fingerprint(), "tier1");
    pin_tier(&b, &a.fingerprint(), "tier1");
    pair(&a, &b);
    eventually("a tier 1 media link on the accepting side", || {
        b.tcp_link().is_some_and(|l| l["alive"] == Value::Bool(true))
    });

    // A ticket B would have handed out itself. Minting it directly is the only
    // way to get a *second* one: `offer_ticket` deliberately suppresses a
    // second live ticket per peer.
    let ticket_b64 = crate::tcpmedia::mint_ticket_for_test(b.h.inner_for_test(), &a.fingerprint());
    let mut s = TcpStream::connect(b.addr()).expect("dial B's control port");
    s.set_read_timeout(Some(Duration::from_secs(5))).expect("read timeout");
    write_frame(&mut s, &ControlMsg::MediaAttach { ticket_b64 }).expect("send media_attach");
    match read_frame(&mut s).expect("read the reply") {
        ControlMsg::Error { message } => assert!(
            message.contains("already attached"),
            "refused for the wrong reason: {message}"
        ),
        ControlMsg::Ok {} => panic!(
            "a second media link was installed over the live one; the first link's writer now \
             has no reader and nothing counts it"
        ),
        other => panic!("unexpected reply: {other:?}"),
    }

    // ...and the original link is untouched.
    let link = b.tcp_link().expect("the first link survived");
    assert_eq!(link["alive"], Value::Bool(true), "the refusal killed the live link: {link}");
}

/// **A media frame that fails AEAD is counted, not merely dropped.**
///
/// `control.rs` promises that a stolen attach ticket buys nothing but "bytes
/// that fail AEAD and get counted and dropped". The dropping was real; the
/// counting was not — the arm was a bare `else { return }` and no counter in
/// the repository moved. On tier 1 the gap is worse than cosmetic:
/// `tcp_media.frames_read` increments for every `Kind::Media` frame off the
/// socket whether or not it authenticates, so injected traffic raises
/// `frames_read` while the session's `received` stays put, and the e2e
/// `read >= received` assertion stays green while it happens.
///
/// Injected over UDP because that needs no ticket and reaches the identical
/// `handle_datagram`; what is being tested is the counter, not the transport.
///
/// Injection control (run 2026-08-08): revert the arm to `else { return }` ⇒
/// red with `auth_failed == 0`.
#[test]
fn a_media_frame_that_fails_aead_is_counted() {
    use audiohub_net::packet::{Codec, Header, Kind};
    use std::net::UdpSocket;

    let (a, b) = linked("authfail");
    a.set_mode(Mode::A);
    b.set_mode(Mode::Share);
    let sid = tone_session(&a, &b) as u32;
    eventually("B to be receiving the tone", || {
        b.ok(methods::SESSION_LIST, json!({}))
            .as_array()
            .is_some_and(|ss| ss.iter().any(|s| s["stats"]["received"].as_u64().unwrap_or(0) > 0))
    });

    // A well-formed header for a stream that exists, over a payload that is
    // not its ciphertext. Everything up to the AEAD passes.
    // `local_addr` reports the wildcard bind (`0.0.0.0:port`), which is not a
    // destination anything can be sent to; only the port is wanted.
    let port = b.h.inner_for_test().udp.local_addr().expect("B's media port").port();
    let dest: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().expect("dest");
    let payload = [0u8; 64];
    let mut dg = Vec::new();
    Header {
        kind: Kind::Media,
        codec: Codec::PcmS16le,
        channels: 1,
        sample_rate: 48000,
        session_id: 0,
        stream_id: sid,
        seq: 1,
        timestamp_us: 0,
        payload_len: payload.len() as u32,
    }
    .encode_append(&payload, &mut dg);
    let sock = UdpSocket::bind("127.0.0.1:0").expect("probe socket");
    for _ in 0..3 {
        sock.send_to(&dg, dest).expect("inject");
    }

    eventually("the forged frames to be counted", || {
        b.ok(methods::SESSION_LIST, json!({}))
            .as_array()
            .is_some_and(|ss| {
                ss.iter().any(|s| s["stats"]["auth_failed"].as_u64().unwrap_or(0) >= 3)
            })
    });
    // ...and nothing was let through: `received` counts authenticated packets.
    let recv = b
        .ok(methods::SESSION_LIST, json!({}))
        .as_array()
        .and_then(|ss| ss.iter().find(|s| s["dir"].as_str() == Some("recv")).cloned())
        .expect("a receiving session");
    assert_eq!(
        recv["stats"]["lost"].as_u64(),
        Some(0),
        "a forged frame was admitted far enough to disturb the sequence accounting: {recv}"
    );
}

// ------------------------------------------------ M8 tier 2 (multiplexed)

/// A local application-layer TCP forwarder, and the whole of the tier 2 test
/// environment (design §7 level 3).
///
/// It reproduces all three of tier 2's premises with **zero privileges and zero
/// system configuration**: it forwards at the application layer only, the
/// daemon on the far side sees `127.0.0.1:<ephemeral>` instead of the dialler's
/// endpoint, and only one direction can originate a connection.
///
/// # ⚠ The port must not collide with a daemon's control port
///
/// `conn::is_self_endpoint` is `port == our control_port && ip_is_ours(ip)`,
/// and `ip_is_ours` returns **true unconditionally for loopback**. So a
/// forwarder that happened to listen on a participating daemon's control port
/// number would be refused as a self-dial — a failure that looks like a
/// downgrade bug and is not one. Bound to port 0 (the kernel picks) and
/// asserted against both daemons in every test that uses it.
struct Forwarder {
    port: u16,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Bytes carried, either direction. Only used to prove the throttle is
    /// really the thing limiting the link.
    carried: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl Drop for Forwarder {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Forwarder {
    fn start(to: std::net::SocketAddr) -> Forwarder {
        use std::net::TcpListener;
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
        use std::sync::Arc;

        let l = TcpListener::bind("127.0.0.1:0").expect("bind the forwarder");
        let port = l.local_addr().expect("forwarder addr").port();
        l.set_nonblocking(true).expect("nonblocking accept");
        let stop = Arc::new(AtomicBool::new(false));
        let carried = Arc::new(AtomicU64::new(0));

        let (s, c) = (stop.clone(), carried.clone());
        std::thread::spawn(move || {
            while !s.load(Ordering::SeqCst) {
                let Ok((down, _)) = l.accept() else {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                };
                let Ok(up) = std::net::TcpStream::connect(to) else { continue };
                // Nagle off on both legs. With it on, the forwarder itself
                // would add up to 40 ms to every small frame and the round-trip
                // figure this rig exists to measure would be measuring the rig.
                let _ = down.set_nodelay(true);
                let _ = up.set_nodelay(true);
                for (from, to) in [
                    (down.try_clone(), up.try_clone()),
                    (up.try_clone(), down.try_clone()),
                ] {
                    let (Ok(mut from), Ok(mut to)) = (from, to) else { continue };
                    let (s, c) = (s.clone(), c.clone());
                    std::thread::spawn(move || {
                        let _ = from.set_read_timeout(Some(Duration::from_millis(50)));
                        let mut buf = [0u8; 8192];
                        loop {
                            if s.load(Ordering::SeqCst) {
                                break;
                            }
                            let n = match from.read(&mut buf) {
                                Ok(0) => break,
                                Ok(n) => n,
                                Err(e)
                                    if matches!(
                                        e.kind(),
                                        std::io::ErrorKind::WouldBlock
                                            | std::io::ErrorKind::TimedOut
                                            | std::io::ErrorKind::Interrupted
                                    ) =>
                                {
                                    continue
                                }
                                Err(_) => break,
                            };
                            if to.write_all(&buf[..n]).is_err() {
                                break;
                            }
                            c.fetch_add(n as u64, Ordering::Relaxed);
                        }
                        let _ = to.shutdown(std::net::Shutdown::Both);
                    });
                }
            }
        });
        Forwarder { port, stop, carried }
    }

    fn addr(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    fn carried(&self) -> u64 {
        self.carried.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// The precondition written into every test that uses one. See the type's
    /// note about `is_self_endpoint`.
    fn assert_not_a_control_port(&self, nodes: [&Node; 2]) {
        for n in nodes {
            assert_ne!(
                self.port,
                n.control_port(),
                "the forwarder took a participating daemon's control port ({}), so \
                 `is_self_endpoint` would refuse the dial as a self-connection and the failure \
                 would look like a tier 2 bug",
                self.port
            );
        }
    }
}

/// Bring two daemons up on tier 2 through a forwarder, paired and connected.
fn tier_two_pair(tag: &str, tx_kbps: Option<u64>) -> (Node, Node, Forwarder) {
    let a = Node::start_throttled(&format!("{tag}-a"), tx_kbps);
    let b = Node::start_throttled(&format!("{tag}-b"), tx_kbps);
    a.set_mode(Mode::A);
    b.set_mode(Mode::Share);

    // The forwarder is never the throttle. Rate-limiting **downstream** of the
    // socket puts the backlog in the kernel send buffer, where nothing this
    // project writes can reorder it: measured 2026-08-08, a forwarder held at a
    // tenth of the media rate produced control round trips of **eight seconds**
    // with the scheduler working perfectly. The throttle that isolates the
    // scheduler is the one on our own writer, ahead of the socket.
    let fwd = Forwarder::start(
        format!("127.0.0.1:{}", b.control_port()).parse().expect("B's control addr"),
    );
    fwd.assert_not_a_control_port([&a, &b]);

    // Before pairing: the transport is chosen from the stored tier at the
    // moment of the dial, and `register_conn` installs the media path before
    // `conn_reader` starts.
    pin_tier(&a, &b.fingerprint(), "tier2");
    pin_tier(&b, &a.fingerprint(), "tier2");
    // B is on the far side of a one-way tunnel: it can be dialled and cannot
    // dial. This is the setting `reconnect` and `PeerState` both read.
    set_dial_policy(&b, &a.fingerprint(), "inbound_only");

    pair_through(&a, &b, &fwd.addr());
    (a, b, fwd)
}

/// **Acceptance 1 and 2 (design §6, P5): pairing and bidirectional media across
/// a forwarder that destroys the source address — and the fingerprint check
/// still holds.**
///
/// This is `plan.md` §4's "身份基于指纹不基于源地址" as something that runs.
/// The assertions that make it about tier 2 rather than about "loopback works":
///
///   1. the address A holds for B is the **forwarder's**, not B's — A has no
///      way to reach B directly and never learns one;
///   2. the address B sees for A is an ephemeral forwarder port, **not** A's
///      control port — the source attribute really is gone;
///   3. both daemons nevertheless have the other's real fingerprint, which is
///      the only thing that was ever authenticated;
///   4. control frames were carried **on the media connection** — the one
///      observable that separates one multiplexed connection from two;
///   5. the audio arrived, and the frames the receiver decoded account for the
///      packets its session counted, so no byte quietly took the UDP path.
///
/// Injection controls (run 2026-08-08, both red as described):
///   - make `connect_peer` ignore `TransportTier::Tier2` and dial plain TCP ⇒
///     red at (4): `latency_guard.mux` stays empty on both sides, while (1),
///     (2), (3) and the tone all still pass. That is the assertion's whole
///     purpose — everything else here is true of a plain forwarded connection.
///   - remove the `MAGIC` branch from `handle_inbound` ⇒ B refuses the
///     connection with "first control frame too large" and the pair never
///     forms, which is the loud failure `peek_looks_multiplexed`'s note
///     predicts.
#[test]
fn a_tier_two_pair_survives_the_source_address_being_lost() {
    let (a, b, fwd) = tier_two_pair("t2", None);

    eventually("the mux to come up on the dialling side", || {
        a.mux_link().is_some_and(|l| l["alive"] == Value::Bool(true))
    });
    eventually("the mux to come up on the accepting side", || {
        b.mux_link().is_some_and(|l| l["alive"] == Value::Bool(true))
    });

    // (1) A's route to B is the tunnel, and only the tunnel.
    let b_record = a.peer(&b.fingerprint());
    assert_eq!(
        b_record["port"].as_u64(),
        Some(fwd.port as u64),
        "A recorded something other than the forwarder as B's port, so it is not going through \
         the tunnel at all: {b_record}"
    );
    assert_ne!(
        fwd.port,
        b.control_port(),
        "the forwarder and B's control port coincided; (1) would be vacuous"
    );

    // (2) B cannot tell where A is. Same IP as every other loopback peer, and a
    // port belonging to the forwarder rather than to A.
    let seen = b.conn_peer_addr(&a.fingerprint()).expect("B has a live channel to A");
    assert!(seen.ip().is_loopback(), "the forwarder was expected on loopback, saw {seen}");
    assert_ne!(
        seen.port(),
        a.control_port(),
        "B saw A's own control port, so the forwarder is not in the path and the premise of \
         this test does not hold"
    );

    // (3) ...and identity survived anyway, in both directions.
    assert_eq!(
        b.peer(&a.fingerprint())["fingerprint"].as_str(),
        Some(a.fingerprint().as_str()),
        "B did not end up with A's fingerprint"
    );
    assert_eq!(a.peer(&b.fingerprint())["online"], Value::Bool(true));

    // Audio, both directions, on the one connection.
    a.ok(
        methods::SESSION_OPEN,
        json!({
            "peer": b.fingerprint(), "kind": KIND_SPK, "source": SOURCE_TONE,
            "freq": 1000.0, "verify_freq": 1000.0
        }),
    );
    a.ok(
        methods::SESSION_OPEN,
        json!({
            "peer": b.fingerprint(), "kind": audiohub_ipc::KIND_MIC, "source": SOURCE_TONE,
            "freq": 1000.0, "verify_freq": 1000.0
        }),
    );

    eventually_within(Duration::from_secs(25), "B's 1 kHz verdict over the mux", || {
        b.recv_verdict().is_some_and(|v| v["detected"] == Value::Bool(true))
    });
    eventually_within(Duration::from_secs(25), "A's 1 kHz verdict over the mux", || {
        a.recv_verdict().is_some_and(|v| v["detected"] == Value::Bool(true))
    });
    let snr = b
        .recv_verdict()
        .expect("checked above")["snr_db"]
        .as_f64()
        .expect("a detected verdict carries an SNR");
    assert!(snr >= 40.0, "1 kHz over a loopback mux should be clean, got {snr:.1} dB SNR");

    // (4) The control plane rode the same connection.
    let a_mux = a.mux_link().expect("still up");
    let b_mux = b.mux_link().expect("still up");
    for (who, m) in [("A", &a_mux), ("B", &b_mux)] {
        assert!(
            m["control_frames_written"].as_u64().unwrap_or(0) > 0,
            "{who} never wrote a control frame onto the mux, so its control plane is somewhere \
             else and this is not one connection: {m}"
        );
        assert!(
            m["control_frames_read"].as_u64().unwrap_or(0) > 0,
            "{who} never read a control frame off the mux: {m}"
        );
    }
    assert_eq!(
        a_mux["fingerprint"].as_str(),
        Some(b.fingerprint().as_str()),
        "the mux is attached to the wrong peer"
    );

    // (5) ...and so did the media. `received` first, then `frames_read`: both
    // are still climbing, and reading them the other way round reports a false
    // failure on a perfectly healthy link (measured in P3).
    let sessions = b.ok(methods::SESSION_LIST, json!({}));
    let recv = sessions
        .as_array()
        .and_then(|ss| ss.iter().find(|s| s["dir"].as_str() == Some("recv")))
        .expect("B must have a receiving session")
        .clone();
    let received = recv["stats"]["received"].as_u64().expect("a received count");
    let read = b
        .tcp_link()
        .expect("the mux's media half is a tcp_media row")["frames_read"]
        .as_u64()
        .expect("a read count");
    assert!(received > 0, "the session reports no packets at all");
    assert!(
        read >= received,
        "the session counted {received} packets but only {read} came off the mux, so the rest \
         arrived over UDP — the downgrade is decorative"
    );
    assert_eq!(recv["stats"]["lost"].as_u64(), Some(0), "loopback TCP lost a packet: {recv}");
    assert!(fwd.carried() > 0, "the forwarder carried nothing, so it is not in the path");
}

/// **Acceptance 3 (design §6, P5): media at full rate must not starve the
/// control plane. `Ping` → `Pong` p95 under 200 ms.**
///
/// The link is throttled **at the forwarder**, so the media queue is genuinely
/// backed up for the whole window and every control frame has to be let through
/// by the credit rather than by a gap in the traffic.
///
/// # ⚠ This test measures the number; it does **not** discriminate. Read on.
///
/// Deleting the `last_control.elapsed() >= CONTROL_CREDIT` disjunct from
/// `mux::control_may_go` — leaving strict priority alone — was expected to make
/// this go red. **It does not** (measured 2026-08-08: p95 129.7 ms with the
/// credit removed, against 62.1 ms with it). The reason is worth writing down,
/// because it is a real property of the design and not a defect in the rig:
///
/// **The media queue is self-emptying.** A frame the stale gate drops never
/// reaches the wire, so it never charges the token bucket — dropping is *free*
/// in link budget. Under any sustained oversubscription the gate therefore
/// clears the backlog faster than it can accumulate, the queue reaches empty
/// several times a second, and the `media.queued() == 0` half of the rule lets
/// control out on its own. Tuning the throttle does not escape this: below
/// saturation the queue empties because it drains, above it the queue empties
/// because the gate fires, and the only state in between is not a steady one.
///
/// So there are two independent reasons control does not starve here, and this
/// test cannot separate them. The credit's own injection control lives where it
/// *is* decisive, driving the scheduler directly with both queues held full:
/// `mux::tests::control_overtakes_media_once_the_credit_is_due` and
/// `a_saturated_media_queue_cannot_starve_control_past_the_credit`, both red
/// under exactly that deletion (verified 2026-08-08 — "an overdue control frame
/// did not overtake the media backlog" and "only 0 control frames got out").
///
/// What this test is still for: the acceptance names a number measured on real
/// daemons over a real connection, and no state-machine test can produce that.
/// It also covers the failure the scheduler tests cannot see, which is the one
/// that actually occurred here — before `write_one_queued` learned to take a
/// deadline cap, a media frame blocked in `write` held the wire for a whole
/// stale budget and the credit, checked only *between* frames, quietly became
/// "100 ms plus however long one frame blocks". That produced no round trip at
/// all for five seconds and `ping_and_reap` declared the channel dead.
///
/// # Why `#[ignore]`
///
/// **It is a wall-clock measurement and cannot share a machine with the
/// parallel suite.** Run alone it reports p95 56–91 ms across repeated runs;
/// run inside `cargo test --workspace`, alongside five hundred other tests and
/// several other daemon pairs, the same code reports **311 ms** — the 1 Hz
/// ticker and both audio loops slip under the load, and what the number then
/// measures is the host, not the scheduler.
///
/// Leaving it in the default set would mean either a flaky suite or a threshold
/// loosened until it stopped meaning anything. It runs deliberately, the way
/// P3's and P4's acceptance measurements do:
///
/// ```text
/// cargo test -p audiohubd --lib media_at_full_rate -- --ignored --nocapture
/// ```
///
/// The properties that must hold on every run are covered by `mux::tests`,
/// which are state-machine tests with no clock dependence beyond the credit
/// itself.
#[test]
#[ignore = "wall-clock measurement: run alone, not inside the parallel suite (see the doc comment)"]
fn media_at_full_rate_does_not_starve_the_control_plane() {
    // # Choosing the throttle: it has to be *just* under the offered rate
    //
    // Each writer here carries one rung-0 stream: 10 ms of 48 kHz/32-bit mono
    // is 1920 B, split into two 960 B halves, each +40 header +16 tag ⇒ 200
    // frames/s x 1016 B = 203 KB/s = 1.63 Mbit/s.
    //
    // The first version of this test throttled to 2 Mbit/s — **above** that —
    // and the injection control did not go red: the queue emptied between
    // ticks, so strict priority alone was enough. A far *lower* throttle fails
    // the same way for the opposite reason: every frame ages past
    // `STALE_BUDGET`, the gate drops it without writing, and the queue empties
    // just as thoroughly. Only a mild oversubscription keeps the queue
    // permanently non-empty **with frames still worth writing**, which is the
    // one state strict priority starves control in.
    //
    // 1500 kbit/s = 187.5 KB/s, about 92% of offered. The steady state is a
    // queue held at roughly the stale budget's worth of frames, the gate
    // trimming the excess, and never a moment when it is empty.
    let (a, b, fwd) = tier_two_pair("t2-starve", Some(1500));
    eventually("the mux to come up", || {
        a.mux_link().is_some_and(|l| l["alive"] == Value::Bool(true))
    });

    // Rung 0 in both directions: the most media this ladder can produce.
    for (n, peer, dir) in [(&a, b.fingerprint(), "send"), (&a, b.fingerprint(), "recv")] {
        n.ok(
            methods::PEERS_SET_TRANSPORT,
            json!({ "peer": peer, "dir": dir, "quality": "pcm48k32f" }),
        );
    }
    a.ok(
        methods::SESSION_OPEN,
        json!({ "peer": b.fingerprint(), "kind": KIND_SPK, "source": SOURCE_TONE, "freq": 1000.0 }),
    );
    a.ok(
        methods::SESSION_OPEN,
        json!({ "peer": b.fingerprint(), "kind": audiohub_ipc::KIND_MIC, "source": SOURCE_TONE,
                "freq": 1000.0 }),
    );

    // Wait until the media queue really is backed up. Without this the sample
    // window could open before saturation and measure an idle link — the shape
    // of "the test passed because the condition never happened".
    // The gauge, not the depth: `queued > 0` is true for an instant on any
    // link, and a window that opened on that instant would be measuring an idle
    // one. `writeq_ms` is how long the frame the writer just picked up had been
    // waiting, so a sustained reading is saturation itself rather than a
    // symptom of it.
    eventually_within(Duration::from_secs(25), "the media queue to back up", || {
        a.tcp_link().is_some_and(|l| l["writeq_ms"].as_f64().unwrap_or(0.0) > 50.0)
    });

    // Sample distinct round trips. `rtt_ms` is the last `Pong`'s, refreshed by
    // the 1 Hz ticker, so the same reading appearing twice is one sample.
    let mut samples: Vec<f64> = Vec::new();
    let mut last = f64::NAN;
    let until = Instant::now() + Duration::from_secs(20);
    while Instant::now() < until {
        if let Some(rtt) = a.rtt_ms(&b.fingerprint()) {
            if rtt != last {
                samples.push(rtt);
                last = rtt;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // Six, not twenty. `Ping` is driven by the 1 Hz ticker, and this test runs
    // two whole daemons plus their audio loops inside a test binary that is
    // running everything else in parallel — the ticker slips under that load
    // and the sample count is a property of the harness, not of the scheduler.
    // The bound still separates the two outcomes decisively: with the credit
    // removed the count is **zero** (no `Ping` ever reaches the wire) and the
    // channel dies of silence, which no amount of ticker slip resembles.
    assert!(
        samples.len() >= 6,
        "only {} round trips completed in 20 s of saturated media; the control plane is being \
         starved, which is precisely the failure the credit exists to prevent (samples: {:?})",
        samples.len(),
        samples
    );
    samples.sort_by(f64::total_cmp);
    let p95 = samples[((samples.len() as f64 * 0.95).ceil() as usize).min(samples.len()) - 1];
    // Printed, not merely asserted: this is a distribution, and the number the
    // acceptance names is only meaningful next to the sample count and the
    // spread it came from. Visible under `--nocapture`.
    eprintln!(
        "[p5-acceptance-3] control RTT over a saturated mux: n={} min={:.1} p50={:.1} \
         p95={:.1} max={:.1} ms",
        samples.len(),
        samples[0],
        samples[samples.len() / 2],
        p95,
        samples[samples.len() - 1]
    );
    assert!(
        p95 < 200.0,
        "control round trip p95 was {p95:.1} ms over a saturated mux (n={}, max {:.1} ms); \
         the credit is supposed to bound this at roughly one {:?} per side",
        samples.len(),
        samples.last().copied().unwrap_or(f64::NAN),
        Duration::from_millis(100)
    );

    // The throttle really was the constraint: the link moved far less than an
    // unthrottled loopback would have.
    let queued = a.tcp_link().expect("a media queue")["queued"].as_u64().unwrap_or(0);
    assert!(
        queued > 0 || fwd.carried() > 0,
        "nothing was in flight at all, so nothing was being prioritised over"
    );
}

/// **The third connection state**: an inbound-only peer that is not connected
/// is *waiting*, not offline, and nothing retries a dial toward it.
///
/// Both halves matter and they fail differently. Without the `reconnect` half,
/// the retry ladder dials a tunnel that cannot carry the connection, forever,
/// logging a failure every thirty seconds that is not one. Without the
/// `PeerState` half, a correctly configured machine is drawn with a permanent
/// fault marker and the user's only available conclusion is that the software
/// is broken.
#[test]
fn an_inbound_only_peer_is_awaited_rather_than_dialled() {
    let (a, b, _fwd) = tier_two_pair("t2-inbound", None);
    eventually("the mux to come up", || {
        b.mux_link().is_some_and(|l| l["alive"] == Value::Bool(true))
    });

    // While connected, the third state is not claimed: it answers "where is it
    // when it is not here", and it is here.
    assert_eq!(b.peer(&a.fingerprint())["online"], Value::Bool(true));
    assert_eq!(
        b.peer(&a.fingerprint())["awaiting_inbound"],
        Value::Bool(false),
        "a connected peer was reported as awaited"
    );

    // Now drop it from A's side, so B loses the channel it never dials.
    a.ok(methods::PEERS_DISCONNECT, json!({ "peer": b.fingerprint() }));
    eventually("B to notice A is gone", || {
        b.peer(&a.fingerprint())["online"] == Value::Bool(false)
    });

    let p = b.peer(&a.fingerprint());
    assert_eq!(
        p["awaiting_inbound"],
        Value::Bool(true),
        "an inbound-only peer that is not connected must be reported as awaited, not merely as \
         offline: {p}"
    );
    assert_eq!(
        p["reconnecting"],
        Value::Bool(false),
        "B armed a retry toward a peer it cannot dial: {p}"
    );

    // ...and an explicit connect attempt is refused by name rather than by
    // timing out on a dial the tunnel will not carry.
    let e = b
        .call(methods::PEERS_CONNECT, json!({ "peer": a.fingerprint() }))
        .expect_err("dialling an inbound-only peer must be refused");
    assert!(
        e.contains("inbound-only"),
        "the refusal does not name the reason, so it is indistinguishable from a dead peer: {e}"
    );
}

/// **Acceptance 4 (design §6, P5): on `MediaPath::Framed`, `send_pullreq` sends
/// nothing.**
///
/// **Counted, not grepped.** The P3 test for the same property on tier 1 asserts
/// the *source text* of `send_pullreq` still reads the path — which this
/// repository's own record says is the weaker form, because these guards are
/// blind to comments and have been satisfied by commented-out code before. So
/// this one binds a real socket and counts datagrams.
///
/// The positive control is what makes the zero mean something: the same stream,
/// the same call, the same socket, with a `Udp` path — if that did not deliver,
/// "zero on Framed" would be true because nothing works.
#[test]
fn a_tier_two_stream_sends_no_keepalive_datagrams() {
    use std::net::UdpSocket;

    let n = Node::start("t2-ka");
    let inner = n.h.inner_for_test().clone();

    // Somewhere a keepalive could go, and something that counts what arrives.
    let sink = UdpSocket::bind("127.0.0.1:0").expect("bind the keepalive sink");
    sink.set_read_timeout(Some(Duration::from_millis(200))).expect("timeout");
    let sink_addr = sink.local_addr().expect("sink addr");

    let drain = |sink: &UdpSocket| -> usize {
        let mut buf = [0u8; 2048];
        let mut n = 0;
        while sink.recv_from(&mut buf).is_ok() {
            n += 1;
        }
        n
    };

    const CALLS: usize = 5;

    // Positive control: a tier 0 stream really does put datagrams on the wire
    // through this exact call.
    let udp_rx = crate::RxStream::new(
        7,
        &[0u8; 32],
        &[0u8; 12],
        None,
        true,
        false,
        None,
        None,
        crate::tcpmedia::MediaPath::Udp(sink_addr),
    );
    for _ in 0..CALLS {
        crate::engine::send_pullreq(&inner, &udp_rx);
    }
    assert_eq!(
        drain(&sink),
        CALLS,
        "the tier 0 keepalive did not arrive, so the zero asserted below would prove nothing"
    );

    // The subject: a tier 2 stream has no UDP destination and must send nothing.
    //
    // **The mux's recorded peer address is the sink's**, deliberately. That is
    // the shape of the bug this asserts against: `ConnShared` used to compute
    // `media_dest` as peer IP + advertised port unconditionally, and on a
    // tunnel that address is well-formed, reachable and **someone else's**. If
    // `MediaPath::Framed` ever grew a `udp_dest`, this is where those datagrams
    // would land, and the counter would see them.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let link = crate::mux::MuxLink::new_for_test(
        std::sync::Arc::new(crate::tcpmedia::TcpMediaLink::new_for_test(
            "fp".into(),
            sink_addr,
        )),
        listener.local_addr().expect("addr"),
    );
    let mux_rx = crate::RxStream::new(
        8,
        &[0u8; 32],
        &[0u8; 12],
        None,
        true,
        false,
        None,
        None,
        crate::tcpmedia::MediaPath::Framed(link),
    );
    for _ in 0..CALLS {
        crate::engine::send_pullreq(&inner, &mux_rx);
    }
    assert_eq!(
        drain(&sink),
        0,
        "a tier 2 stream sent keepalive datagrams; there is no address they could correctly go \
         to, so they went to one that was invented"
    );
}

