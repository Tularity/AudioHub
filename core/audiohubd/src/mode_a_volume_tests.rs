//! plan §7.1 模式 A 两个音量开关的**接线**测试。
//!
//! # 为什么这一组是「读源码」而不是「跑起来看」
//!
//! 这条通路的执行器是**本机真实默认输出设备的音量与静音**。跑一次端到端就
//! 等于在测试里把这台机器的音量拧一遍、把喇叭静音一次——本机此刻正有一条
//! 模式 B 链路在服务用户的真实音频，那不是「测试环境」，那是现场。所以：
//!
//!   - **判定**是纯函数，单元测试在 `audiohub_core::volume`（含注入对照）；
//!   - **落盘**在 `settings.rs` 的测试里；
//!   - **写入契约**（`settings.set` 真的照做、命令行真的够得到）分别在
//!     `transport_tests.rs` 与 `audiohub-cli` 的 `ctl::tests` 里；
//!   - 中间只剩一件事没人盯着：**判定函数到底有没有被调用**。
//!
//! 本项目已经为「函数写对了但没人调用它」付过多次学费（`mode_tests.rs` 开头
//! 记着四次），而这一轮补的缺口本身就是其中一例——`set_default_output_volume`
//! 存在了整整一个里程碑，消费端那半边一次都没调过它（终审 §二.1）。这一组
//! 守卫盯的正是那件事：三个调用点少任何一个都变红。
//!
//! 判据刻意取**调用表达式**而不是函数名：`use` 一行、注释里提一句、或者
//! 定义处的名字都不算数。

use std::path::PathBuf;

fn read(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src").join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "读不到 {}（{e}）。文件被改名/挪走了就把这条测试一起更新，\
             不要让它退化成一条恒真断言",
            path.display()
        )
    })
}

/// 一个片段在文件里出现的次数。断言用「至少一次」而不是「恰好一次」：
/// 多一个调用点不是失效，少一个才是。
fn has(src: &str, needle: &str) -> bool {
    src.contains(needle)
}

/// **对端音量 → 本机默认输出**这条路必须真的接在 `VolumeState` 的处理里。
///
/// 终审 §二.1 记下的原始缺口逐字是：「收到 `VolumeState` 只写进会话内的显示
/// 单元格，**从不调用** `set_default_output_volume`」。所以这里同时盯两件事：
/// 处理器调了 `follow_peer_volume`，而 `follow_peer_volume` 里真的有那个写。
#[test]
fn the_inbound_half_reaches_the_local_output_device() {
    let src = read("conn.rs");
    assert!(
        has(&src, "follow_peer_volume(inner, &e, state)"),
        "SessionMsg::VolumeState 的处理里没有调用 follow_peer_volume —— \
         对端的音量又一次只到了显示单元格为止（终审 §二.1）"
    );
    assert!(
        has(&src, "volume::classify_follow(consumer,"),
        "follow_peer_volume 没有走 classify_follow，模式/开关/静音例外三条判定就都绕过去了"
    );
    assert!(
        has(&src, "volume::set_default_output_volume(w.scalar)"),
        "follow_peer_volume 没有真的写本机默认输出设备的音量"
    );
    assert!(
        has(&src, "volume::set_default_output_mute(m)"),
        "follow_peer_volume 没有真的写本机默认输出设备的静音"
    );
    assert!(
        has(&src, "lk(&e.volume.sync).note_peer_apply(w.scalar, hold)"),
        "写之前没有给防乒乓上膛：本机随后那一拍会把对端刚下来的值当成本机的变化推回去"
    );
}

/// **本机默认输出 → 对端**这半边必须挂在 1 s 协调线程上。
///
/// 少了它，「互相跟随」就只剩单向：对端拧音量本机跟随，本机拧音量对端一无所知。
#[test]
fn the_outbound_half_runs_on_the_ticker() {
    let src = read("lib.rs");
    assert!(
        has(&src, "poll_consumer_volume(&inner, e, live)"),
        "1 s 协调线程里没有消费端的音量轮询：本机拧音量对端永远不会知道"
    );
    assert!(
        has(&src, "e.kind == KIND_SPK && e.dir == DIR_SEND"),
        "消费端那一支必须按 (kind, dir) 挑流——只按 kind 会把本机是提供者的那条也算进来"
    );
    assert!(
        has(&src, "volume::classify_follow(true, mode_a, opt, changed)"),
        "外发那半边没有走 classify_follow：静音例外只在入站方向成立的话，\
         「静音本机」照样会把对端静掉"
    );
    assert!(
        has(&src, "SessionMsg::VolumeSet {"),
        "轮询发现的本机变化没有变成 VolumeSet 送出去"
    );
}

/// plan §7.1「**初始对齐时本机采用对端当前值**」：对端没开过口之前，本机这一侧
/// 的读数不是「变化」，一个字都不许往外发。
///
/// 少了这一条，「以对端为准」就整个倒过来了：本机在连上的第一拍就把自己的音量
/// 推给对端，对端的旋钮被一台刚刚才认识它的机器覆盖掉——而这正是 §7.1 用「对端
/// 优先」明文排除的那个方向。
///
/// 注入对照（2026-08-09 实跑）：删掉这行早退，`mode_a_volume_tests` + `mode_tests`
/// + core 的 `volume::` 全都照绿——这条判定此前零覆盖。
#[test]
fn the_consumer_does_not_speak_before_the_peer_has() {
    let src = read("lib.rs");
    assert!(
        has(&src, "if lk(&e.volume.state).is_none() {"),
        "poll_consumer_volume 里没有「对端还没说过话就不发」这条早退：\
         §7.1 的「初始对齐时本机采用对端当前值」被反过来了，本机会先推自己的值"
    );
}

/// 「静音本机」必须在**会话真的建立之后**开火，而且只此一次。
#[test]
fn the_one_shot_mute_fires_where_the_stream_becomes_established() {
    let src = read("conn.rs");
    assert!(
        has(&src, "mute_local_on_connect(inner, capture_backend.as_deref())"),
        "open_session_from 里没有 「静音本机」 的调用点：开关存在但什么都不做"
    );
    assert!(
        has(&src, "volume::classify_mute_on_connect("),
        "mute_local_on_connect 没有走 classify_mute_on_connect：模式、开关与\
         捕获点位前提三条判定全被绕过"
    );
    assert!(
        has(&src, "sysaudio::capture_survives_local_mute"),
        "没有问过捕获后端的前提（plan §7.1）：在 post-mix 的后端上静音会把镜像一起静掉"
    );
    assert!(
        has(&src, "volume::set_default_output_mute(true)"),
        "mute_local_on_connect 没有真的静音"
    );
    // 一次性：这条路上不许有任何维持机制。plan §7.1 明说用户取消静音即尊重。
    let ticker = read("lib.rs");
    assert!(
        !has(&ticker, "mute_local_on_connect"),
        "「静音本机」 出现在 1 s 协调线程里 —— 那就成了一个会把用户的取消静音\
         按回去的**状态**，而 plan §7.1 冻结的是一次性**动作**"
    );
}

/// 断线重放不是「下一次连接建立」。
///
/// plan §7.1 把用户手动取消静音解释为「本机也要出声」，予以尊重，**直到下一次
/// 连接建立**。而 `open_session_from` 是断线重连恢复会话的唯一路径——网络抖一下、
/// 自动降级 `retier()`、用户自己改一次档位，走的都是它。一次性静音要是不认这件
/// 事，用户刚取消的静音就会被按回去，而界面上没有任何地方说得出为什么。
///
/// `SessionOrigin` 答不了这个问题：重放的用户会话仍然是用户会话。所以判据是
/// 另一个入参 `OpenCause`。
#[test]
fn the_one_shot_mute_does_not_fire_on_a_reconnect_replay() {
    let src = read("conn.rs");
    assert!(
        has(&src, "cause == OpenCause::Fresh"),
        "「静音本机」的调用点没有区分开流的**起因**：断线重放会把用户刚取消的静音\
         按回去（plan §7.1「用户手动取消静音即解释为本机也要出声，予以尊重」）"
    );
    let replay = read("reconnect.rs");
    assert!(
        has(&replay, "conn::OpenCause::Replay"),
        "重放路径没有把自己标成 Replay —— 它会被当成一次全新的连接建立"
    );
    let hal = read("haldev.rs");
    assert!(
        has(&hal, "conn::OpenCause::Fresh"),
        "模式 B 设备协调器那条开流路径被标成了重放：那是一次真正的建立，\
         该开火的时候不开火同样是缺陷"
    );
}

/// **界面上必须真的有这两个开关，而且写的是契约表上的那两个键。**
///
/// 这是 `SETTINGS_WRITABLE_KEYS` 缺的第三条腿。前两条（daemon 真的照做、命令行
/// 够得到）已经各有一条测试，而本轮补的这个缺口恰恰出在第三条腿上：终审
/// §二.1 记的原话是「`volume_sync: true` **硬编码**，用户关不掉」——daemon 侧
/// 读得对、前端也在传，两边都绿，中间少的是一个开关。
#[test]
fn both_switches_exist_in_the_settings_view() {
    const TSX: &str = "app/frontend/src/views/Settings.tsx";
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").join(TSX);
    let src = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "读不到 {TSX}（{e}）。文件被改名/挪走了就把这条测试一起更新，\
             不要让它退化成一条恒真断言"
        )
    });
    for key in ["mode_a_volume_sync", "mode_a_mute_local"] {
        assert!(
            has(&src, &format!("{{ {key}: want }}")),
            "设置页里没有一个开关把 '{key}' 写出去 —— 这正是「硬编码、用户关不掉」\
             那个缺口的形状（终审 §二.1）"
        );
        assert!(
            has(&src, &format!("ds.{key}")),
            "设置页没有把 '{key}' 的当前值读回来显示：开关会一直停在它自己的初值上"
        );
    }
    // §7.2 的例外必须在界面上说得出来，而不是只活在 Rust 注释里。
    assert!(
        has(&src, "settings.modeAVolume.syncMuteException"),
        "两个开关同开时「只同步音量、不同步静音」这条例外，界面上没有任何地方解释"
    );
}

/// 两个开关的判定都必须读**生效中**的模式，不是用户请求的那个。
///
/// 一台请求了 B 却没有可用驱动的机器**正在跑模式 A**，它的系统输出就是放镜像
/// 的那个设备。按请求模式判会让这两个开关在那台机器上永远不生效，而界面上
/// 它们是开着的。
#[test]
fn the_switches_are_judged_against_the_mode_actually_in_force() {
    let src = read("lib.rs");
    assert!(
        has(&src, "haldev::effective_mode(inner) == Mode::A"),
        "mode_a_in_force 必须取 effective_mode"
    );
    let conn = read("conn.rs");
    assert!(
        has(&conn, "crate::mode_a_in_force(inner)"),
        "conn.rs 的两个调用点必须经 mode_a_in_force 判模式"
    );
}

/// §7.2 的软件增益兜底与 §7.1 的「与对端音量同步」是**互斥**的两条路，分界就是
/// 生效中的模式，而分界只写在一处：`VolumeState` 处理里那个 `&& mode_b_in_force`。
///
/// 少了它，模式 A 上两件事同时发生：发送侧施加软件增益（§7.2），本机默认输出又
/// 跟着对端走（§7.1）——两个机制在动同一个响度，正是 plan §12.5 拿来做星标断言
/// 的那个双重衰减形状。而且 `fallback` 会恒真，`else` 那一支再也不执行，模式 A
/// 的同步整个**静默失效**。
///
/// 注入对照（2026-08-09 实跑）：删掉 `&& crate::mode_b_in_force(inner)`，
/// `mode_a_volume_tests` + `mode_tests` + core 的 `volume::`/`dsp::` 全绿——
/// 这条分界此前零覆盖。
#[test]
fn the_software_gain_fallback_is_fenced_off_from_mode_a() {
    let src = read("conn.rs");
    assert!(
        has(&src, "&& crate::mode_b_in_force(inner)"),
        "§7.2 兜底的接手判据没有模式门：模式 A 上会同时跑软件增益与「与对端音量同步」，\
         两个机制动同一个响度（plan §12.5 的双重衰减），而 §7.1 的那条同步会静默失效"
    );
    assert!(
        has(&src, "volume::authority_for(Some(state))"),
        "接手判据没有走 authority_for：对端设备可不可调音量这件事就没人问了"
    );
}
