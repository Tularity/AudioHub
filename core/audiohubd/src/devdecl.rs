//! **向系统如实声明设备延迟**——把一条链路量出来的 `sum_ms` 送进驱动，
//! 换掉那句「你交给我的帧立刻就响」。
//!
//! ## 这不是延迟优化，一毫秒都没省
//!
//! 三份可核实的消费者源码（Chromium / VLC / AVFoundation）读设备延迟属性
//! **全是为了音视频同步的时间戳**，Chromium 的注释亲口写 `a/v sync`；
//! **没有任何播放器用它加大预缓冲**。所以本模块兑现的是：
//!
//! - 播放器把画面推后对齐（今天偏约 121 ms，而播放器**无从得知**）；
//! - `AVAudioPlayerNodeCompletionDataPlayedBack` 回调时刻正确；
//! - 网页 `AudioContext.outputLatency` 从 0 变真值。
//!
//! **不兑现**的是「播放/暂停变快」。用户观察到的 AirPlay 瞬时响应来自
//! AirPlay 2 自己的 Buffered Audio 流 + PTP 时钟锚定，与这条属性无因果关系。
//! 详见 `docs/research-device-latency-property.md`。
//!
//! ## 口径：声明多少
//!
//! 虚拟**扬声器**（App 把音频放进「AudioHub – 对端 扬声器」，我们发给对端播）
//! 那条发送流的 `sum_ms`，一个数不多一个数不少：
//!
//! ```text
//! hal_spk + send_pace + network + 对端(jitter_buf + post_mix + play_ring + play_dev)
//! ```
//!
//! 三处**容易多算**的地方，逐条说明为什么不算：
//!
//! 1. **`kAudioDevicePropertyBufferFrameSize`（本机 512 帧 = 10.7 ms）不算。**
//!    Apple 自己的公式（`AUAudioUnit.h:1487`）是「呈现延迟 = IO 缓冲 + 安全偏移
//!    + 设备延迟」——**IO 缓冲是独立一项，消费者自己会加**。把它折进来就是
//!    重复计入 10.7 ms。这也正好说明了分界线在哪：App 把样本交给 CoreAudio、
//!    等我们的 IOProc 被叫醒的那一截 **就是** 那个 buffer 项，属于 CoreAudio
//!    自己的账；IOProc 之后的一切（`hal_spk` 环、组帧、网络、对端的一切）
//!    才是我们要声明的。
//! 2. **本侧 `cap_dev` 不算**，而它本来也不在：`hal_spk` 源的发送流上
//!    `stream_dev` 判 `None`（这条流没经过本机麦克风），贡献 0。
//!    所以直接取 `sum_ms` 是安全的，不必再减。
//! 3. **`SafetyOffset` 一个帧都不动。** 它参与 IO 调度（「提前多少帧做 IO
//!    才安全」），改它会**真的**加延迟并打乱 `hal_spk` 背后的 DLL 伺服。
//!    执行点在驱动里（两个 `case` 已拆开），这里只是记账上不碰它。
//!
//! ## 三条纪律
//!
//! 1. **没测到就不声明。** 本模块不产生任何兜底值：`sum_ms` 是 `None`
//!    （对端没上报分项 / 设备读不到）⇒ 什么都不发，驱动保持它上一个**测出来的**
//!    值，从未测过就是 0。0 在这里的含义是「从来没人量过这条路」，
//!    **不是**「这台设备是即时的」。编一个「保守估计」= 把一个没有测量支撑的
//!    数字摆到每一个消费者面前，比今天那个诚实的 0 更糟。
//! 2. **只声明扬声器方向。** 虚拟麦克风那条链的尾级是 `hal_mic`，
//!    `docs/spec-hal-mic-latency.md` 记着它曾是 `[0, 500 ms]` 的**无回复力
//!    自由参数**。在它稳定之前声明麦克风方向，等于把一个随机数广播给系统。
//! 3. **闭环，不是「发出去就算数」。** 驱动回一条 ack，说的是那条属性
//!    **此刻真的返回什么**；本模块比对「要的」与「回的」，不一致就下一拍重发。
//!    「mach 发送返回 KERN_SUCCESS」不是属性变了的证据——本项目已经八次栽在
//!    这个替换上。
//!
//! ## 为什么会话内不改
//!
//! VLC（`auhal.c:1629` 的 `Start()`）与 Chromium（`Open()`）**都只在开流时读
//! 一次，都没有装 latency 监听器**；而改这个属性的正规途径
//! （`RequestDeviceConfigurationChange`）会让宿主停掉再重启 IO。
//! 实时伺服 = 频繁打断 IO，换来没有人会读到的更新，纯代价零收益。
//! 所以：**开流前设定，会话内不改**——mac 侧由驱动的锁存实现，
//! Windows 侧由每条流建流时抓一份自己的副本实现。
//!
//! 死区（[`DEADBAND_FRAMES`]）在这之上再加一层：`sum_ms` 每秒都在抖，
//! 而每一次变更在 mac 上都可能招来一次配置变更。

use audiohub_ipc::PipelineLatency;

/// 送进驱动的采样率。两侧的环、两个驱动的虚拟设备格式都是它，
/// 而 `sum_ms` 是毫秒——换算得有个速率，这里就是那一个。
pub(crate) const DECL_RATE: u32 = 48_000;

/// 死区：变化不到这么多帧就不重发。20 ms @ 48k。
///
/// 20 ms 的来历不是四舍五入：它比 `sum_ms` 的逐拍抖动大（本机实测同一条链
/// 相邻两拍差几毫秒），又远小于这条属性存在的理由——音画同步的可察觉门限
/// （ITU-R BT.1359 常引的音频滞后约 125 ms）。介于两者之间的任何值都行，
/// 取 20 只是因为它同时是一个整帧数（960）。
pub(crate) const DEADBAND_FRAMES: u32 = DECL_RATE / 50;

/// 驱动侧的上限，两个驱动各有一份（`kDevice_MaxDeclaredLatency` /
/// `AH_LATENCY_MAX_FRAMES`）。这里再放一份是为了**在花掉一次 IPC 之前**就
/// 拒绝掉荒唐值，并且让这个数在 Rust 侧也有一处可被测试指着的定义。
pub(crate) const MAX_FRAMES: u32 = DECL_RATE * 4;

/// 一个端点的声明状态。**「要的」与「驱动回的」分开存，这是闭环的全部结构。**
///
/// 合成一个字段（「已声明 = 我发过」）就退回了「发出去就算数」，
/// 而那正是本模块存在的理由。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DeclState {
    /// 最近一次算出来、想让驱动声明的帧数。`None` = 还没算出过任何数。
    pub want: Option<u32>,
    /// 驱动最近一次**自陈**的属性值。`None` = 它从没答过。
    pub acked: Option<u32>,
    /// 驱动收下了但还装不上（mac：有 App 正在这台设备上跑 IO）。
    pub pending: bool,
    /// 已经发出去多少次而没等到与 `want` 一致的 ack。
    ///
    /// 存在的唯一理由是把「旧驱动/不支持」与「刚发出去还没回」分开：
    /// 两者在某一拍看起来完全一样，只有次数能区分。
    pub tries: u32,
}

/// 试到第几次还没对上，就该说「这个驱动不认这条消息」而不是继续沉默重试。
///
/// 5 拍 = 5 秒。mac 侧一次 notify 的 mach 发送超时是 500 ms，
/// Windows 侧 IOCTL 是同步的——两边都远小于一拍，所以连续 5 拍拿不到
/// 一致的 ack，不是「慢」，是「不会答」。
pub(crate) const GIVE_UP_AFTER: u32 = 5;

/// 这一拍该不该把 `want` 发下去？
///
/// 三种要发的情形，缺一不可：
/// - 驱动从没答过（`acked == None`）——包括刚 attach、刚 bind；
/// - 答的值与要的差出死区——真的变了；
/// - **答的值与要的不一致但差在死区内**，且还没试满次数——这一条是闭环的
///   牙齿：没有它，一次丢失的 notify 会永远停在「差 3 帧」上不再重发，
///   而 UI 会显示「已声明」。
///
/// 不发的情形只有一个：ack 与 want **相等**。相等就是到位了。
///
/// # 两档重试策略不对称，这是有意的
///
/// `acked == None`（**一次都没答过**）无限重试，`acked == Some(≠want)`
/// 试满 [`GIVE_UP_AFTER`] 就停。理由是两者说的是不同的事：
///
/// - **一次都没答过**只有两种解释——驱动太旧（不认这条消息），或者消息还没
///   到。前者一秒一条 mach 消息的代价可以忽略，而**后者会自己好**：用户装了
///   新驱动、coreaudiod 重启、桥重新 attach，下一拍就接上了。这里若停下，
///   那台机器要等到下次 bind 才会再试一次。
/// - **答了但对不上**说明驱动在听、在答，只是没装上我们要的值。那不是版本
///   问题，每秒撞同一堵墙不会撞出别的结果，只会把一个静态故障变成持续负载。
///   而只要 `want` 真的动了（跨过死区），`advance_want` 会把计数清零，
///   于是新目标一定会被试。
pub(crate) fn should_send(st: &DeclState, want: u32) -> bool {
    match st.acked {
        None => true,
        Some(acked) if acked == want => false,
        Some(acked) => {
            acked.abs_diff(want) >= DEADBAND_FRAMES || st.tries < GIVE_UP_AFTER
        }
    }
}

/// 这一拍要不要把 `want` 换成新算出来的值？
///
/// **死区在这里，不在 [`should_send`]。** 两者分工：这一个决定「目标动不动」，
/// 那一个决定「目标已经达成了没有」。合在一处的后果是，一个在死区内漂移的
/// 目标会让重试计数永远清零，于是「驱动根本不答」这件事永远显不出来。
pub(crate) fn advance_want(prev: Option<u32>, fresh: u32) -> Option<u32> {
    match prev {
        Some(p) if p.abs_diff(fresh) < DEADBAND_FRAMES => None,
        _ => Some(fresh),
    }
}

/// `sum_ms` ⇒ 要声明的帧数。`None` = 这一拍没有可声明的数。
///
/// 拒绝的三种输入，每一种都对应一次「差点编出一个数」：
/// - `sum_ms` 是 `None`：对端没上报分项，或某一级的声卡读不到。
///   **不许拿 `local_ms` 顶**——那是一条链路的一半。
/// - 非有限（NaN / inf）：算术炸了，比没有数更糟。
/// - 超过 [`MAX_FRAMES`]：驱动会拒，先在这里拒掉省一次 IPC。
pub(crate) fn declared_frames(sum_ms: Option<f64>) -> Option<u32> {
    let ms = sum_ms?;
    if !ms.is_finite() || ms < 0.0 {
        return None;
    }
    let frames = (ms * DECL_RATE as f64 / 1000.0).round();
    if frames > MAX_FRAMES as f64 {
        return None;
    }
    Some(frames as u32)
}

/// 一条流的 `PipelineLatency` ⇒ 要声明的帧数。
///
/// 单独一层，因为「取哪个字段」本身是个会写错而不报错的决定：
/// `local_ms` 编译得过、看着也像个延迟，只是它是这条链的**本侧那一半**，
/// 声明它等于宣称网络与对端不存在。
pub(crate) fn frames_for(p: &PipelineLatency) -> Option<u32> {
    declared_frames(p.sum_ms)
}

/// 排障用的一行。`daemon.status` 与 stderr 共用。
pub(crate) fn line(what: &str, st: &DeclState) -> String {
    let want = st.want.map_or("-".to_string(), |f| format!("{f}f ({:.1} ms)", ms_of(f)));
    let acked = match st.acked {
        None => "never answered".to_string(),
        Some(f) => format!("{f}f ({:.1} ms)", ms_of(f)),
    };
    let note = if st.pending {
        " [held: an application is using the device; takes effect when it stops]"
    } else if st.acked.is_none() && st.tries >= GIVE_UP_AFTER {
        " [this driver never acknowledged a latency: too old, or the message is not reaching it]"
    } else {
        ""
    };
    format!("{what}: want={want} declared={acked} tries={}{note}", st.tries)
}

fn ms_of(frames: u32) -> f64 {
    frames as f64 * 1000.0 / DECL_RATE as f64
}

/// **两个驱动的源码审计。**
///
/// # 为什么是审计而不是行为测试
///
/// 这一段规则住在 C / C++ 里：mac 的属性 getter 跑在 `coreaudiod` 的沙箱宿主
/// 进程里，Windows 的那一段跑在内核。两者都不可能从 `cargo test` 里调起来——
/// 装一次 HAL 插件要 sudo + 重启 coreaudiod（会打断全机音频），装一次内核驱动
/// 要签名与一台靶机。
///
/// `test/tests/halwire.rs` 对**线上契约**用的就是这一招（它的第 3 条：
/// 「集成测试链接不到的部分，做源码审计」），理由一模一样，而那次它抓到的是
/// 一个已经上过机、代价是一整轮安装/卸载的真实缺陷。
///
/// 审计能抓什么、不能抓什么，说清楚：**能**抓「有人把两个 `case` 又合并了」
/// 「有人把锁存条件删了」「有人把帧数塞进浮点打包器」这类**结构性**倒退——
/// 而这三条恰好就是本轮最容易被下一个人「顺手简化」掉的三条。**不能**抓
/// 「锁存后的值算错了」这类算术错误；那一半由 [`tests`] 里的纯函数测试盯着。
#[cfg(test)]
mod driver_audit {
    use std::path::{Path, PathBuf};

    fn repo_root() -> PathBuf {
        // CARGO_MANIFEST_DIR = <repo>/core/audiohubd
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .expect("repo root")
            .to_path_buf()
    }

    /// mac 驱动里答属性值的那个函数。同名 `case` 在别的两个 switch 里也在，
    /// 但它们只回答「有没有这个属性」「能不能写」。
    const GETTER: &str = "static OSStatus AudioHub_GetDevicePropertyData(";

    /// **注释先剥掉，再审计。** 不剥的后果这一轮就撞上了：本文件里那条
    /// 「`SafetyOffset` 曾经和它共用一个 case」的说明，本身就写在 latency 的
    /// `case` 体里，于是「这个 case 里不许出现 SafetyOffset」当场误报。
    ///
    /// 更糟的是反方向：**一条把规则写清楚的注释，会让审计永远通过**——
    /// 有人真的合并了两个 case，而审计因为注释里本来就有那个词而看不出来。
    /// 审计的对象是代码，注释不是代码。
    ///
    /// 字符串字面量要认，否则 `"https://…"` 里的 `//` 会把半个文件吃掉。
    fn strip_comments(src: &str) -> String {
        let b = src.as_bytes();
        let mut out = String::with_capacity(src.len());
        let (mut i, n) = (0usize, b.len());
        while i < n {
            match b[i] {
                b'"' | b'\'' => {
                    let q = b[i];
                    out.push(q as char);
                    i += 1;
                    while i < n && b[i] != q {
                        if b[i] == b'\\' && i + 1 < n {
                            i += 1;
                        }
                        i += 1;
                    }
                    if i < n {
                        out.push(q as char);
                        i += 1;
                    }
                }
                b'/' if i + 1 < n && b[i + 1] == b'/' => {
                    while i < n && b[i] != b'\n' {
                        i += 1;
                    }
                }
                b'/' if i + 1 < n && b[i + 1] == b'*' => {
                    i += 2;
                    while i + 1 < n && !(b[i] == b'*' && b[i + 1] == b'/') {
                        i += 1;
                    }
                    i = (i + 2).min(n);
                }
                _ => {
                    // 多字节字符整段搬过去，别按字节拆
                    let start = i;
                    i += 1;
                    while i < n && (b[i] & 0xC0) == 0x80 {
                        i += 1;
                    }
                    out.push_str(&src[start..i]);
                }
            }
        }
        out
    }

    fn read(rel: &str) -> String {
        let p = repo_root().join(rel);
        let raw = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
        strip_comments(&raw)
    }

    /// `marker` 之后最多 `len` 字节，**按字符边界收缩**。
    ///
    /// 注释是中文的，按字节切会在多字节字符中间断开而 panic——那种 panic 会
    /// 把一条本该判定「代码倒退了」的测试变成「测试自己坏了」，两者在 CI 上
    /// 长得一样红，但只有一种需要改代码。
    fn window<'a>(text: &'a str, marker: &str, len: usize) -> &'a str {
        let at = text
            .find(marker)
            .unwrap_or_else(|| panic!("找不到 `{marker}`——它被改名或删掉了"));
        let mut end = (at + len).min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        &text[at..end]
    }

    /// 从 `text` 里切出 `case <label>:` 之后、下一个 `case ` 之前的那一段。
    ///
    /// `after` 把搜索限定在某个函数之内，这是必需的而不是讲究：同一个
    /// `case kAudioDevicePropertyLatency:` 在这个文件里出现三次（属性存在性、
    /// 可写性、取值），而只有最后一处答的是那个数。不限定就会审计到
    /// `AudioHub_HasDeviceProperty` 的那一行，然后**永远通过**。
    fn case_body<'a>(text: &'a str, after: &str, label: &str) -> &'a str {
        let from = text
            .find(after)
            .unwrap_or_else(|| panic!("找不到函数 `{after}`"));
        let scope = &text[from..];
        let needle = format!("case {label}:");
        let start = scope
            .find(&needle)
            .unwrap_or_else(|| panic!("`{after}` 里找不到 `{needle}`"))
            + needle.len();
        let rest = &scope[start..];
        let end = rest.find("\n        case ").unwrap_or(rest.len());
        &rest[..end]
    }

    /// **`SafetyOffset` 必须有自己的 `case`，并且必须返回字面量 0。**
    ///
    /// 这是本轮的红线，也是最容易被合并回去的一处：两个 `case` 挨在一起、
    /// 两者都是 `UInt32`、合并之后编译得过、`devlat-probe` 读出来的两个数
    /// 还都「有值」。而后果是**真的多出那么多延迟**——头文件
    /// （AudioHardwareBase.h:706）说它是「相对硬件当前位置，提前多少帧做 IO
    /// 才安全」，即它**参与 IO 调度**，HAL 真的会提前那么多叫我们，
    /// 顺带打乱 `hal_spk` 背后那套整定好的 DLL 伺服。
    ///
    /// 注入对照：把 `case kAudioDevicePropertySafetyOffset:` 挪回
    /// `case kAudioDevicePropertyLatency:` 的上一行（fallthrough）⇒ 本条红。
    #[test]
    fn the_safety_offset_never_shares_a_case_with_the_declared_latency() {
        let src = read("drivers/macos-hal/src/AudioHubDriver.c");
        let lat = case_body(&src, GETTER, "kAudioDevicePropertyLatency");
        assert!(
            !lat.contains("kAudioDevicePropertySafetyOffset"),
            "SafetyOffset 又和 Latency 共用一个 case 了。它参与 IO 调度，\
             跟着声明值一起涨会真的多出那么多延迟，并打乱 hal_spk 的 DLL 伺服"
        );
        let safety = case_body(&src, GETTER, "kAudioDevicePropertySafetyOffset");
        assert!(
            safety.contains("*((UInt32*)outData) = 0;"),
            "SafetyOffset 必须返回字面 0，它不是诚实性属性"
        );
        assert!(
            !safety.contains("Latch") && !safety.contains("latency"),
            "SafetyOffset 里不许出现任何声明值的来源"
        );
    }

    /// **声明值必须走锁存，不许直接读 `latencyWanted`，也不许再是字面量。**
    ///
    /// 注入对照两条，都是真人会写的「简化」：
    /// - `*((UInt32*)outData) = inDevice->latencyWanted;`
    ///   （少一层锁存 ⇒ 属性在流开着的时候会变，而消费者只在开流时读一次，
    ///   于是它读到的数与它整条流实际经历的数不是同一个）
    /// - `*((UInt32*)outData) = 0;`（整个功能被退回去）
    #[test]
    fn the_declared_latency_comes_from_the_latch_and_not_from_a_literal() {
        let src = read("drivers/macos-hal/src/AudioHubDriver.c");
        let lat = case_body(&src, GETTER, "kAudioDevicePropertyLatency");
        assert!(
            lat.contains("AudioHub_LatchLatency(inDevice"),
            "延迟属性必须由 AudioHub_LatchLatency 回答"
        );
        assert!(
            !lat.contains("*((UInt32*)outData) = 0;"),
            "延迟属性又变回硬编码 0 了"
        );
        assert!(
            !lat.contains("latencyWanted"),
            "绕过锁存直接答 latencyWanted：会话内这个值就会变，而消费者只在开流时读一次"
        );
    }

    /// **锁存的条件是「IO 没在跑」，而且值只能从 `latencyWanted` 来。**
    ///
    /// 注入对照：把 `if(inDevice->ioRunning == 0)` 删掉（即无条件赋值）⇒ 本条红。
    /// 那个写法让声明值在流跑着的时候变，而改这个属性的正规途径会让宿主停掉
    /// 再重启 IO——正是「实时伺服是纯代价零收益」那条结论要避开的。
    #[test]
    fn the_latch_only_installs_a_new_value_while_io_is_stopped() {
        let src = read("drivers/macos-hal/src/AudioHubDriver.c");
        let body = window(&src, "static UInt32 AudioHub_LatchLatency", 1400);
        let guard = body.find("if(inDevice->ioRunning == 0)").expect(
            "锁存必须以 ioRunning == 0 为条件：没有这一条，声明值会在流跑着的时候变",
        );
        let assign = body
            .find("inDevice->latencyFrames = inDevice->latencyWanted;")
            .expect("锁存必须把 latencyWanted 装进 latencyFrames");
        assert!(guard < assign, "赋值必须在那个 if 之内");
    }

    /// **ack 里的帧数必须按整数写进 `scalar_bits`，不许走浮点打包器。**
    ///
    /// `AudioHub_Post` 的第二个参数是 `Float32`，走它会把 5808 变成
    /// `0x45B58000`，daemon 读出来是 11 亿帧——驱动那道上限会拒掉，于是
    /// 「一切都报成功，什么都没发生」。类型系统在这里帮不上忙：
    /// `AudioHub_Post(&d->lat, (Float32)frames, 0)` 编译得过。
    #[test]
    fn the_acked_frame_count_is_written_as_an_integer_not_through_the_float_packer() {
        let src = read("drivers/macos-hal/src/AudioHubDriver.c");
        let body = window(&src, "static void bridge_latency_state", 1400);
        assert!(
            !body.contains("AudioHub_Post("),
            "ack 走了 AudioHub_Post：那是 Float32 打包器，帧数会被当成浮点位模式"
        );
        assert!(
            body.contains("(uint64_t)theFrames"),
            "帧数必须原样进低 32 位"
        );
    }

    /// **Windows：呈现位置必须减去 D 并在 0 处饱和，且 linear position 不动。**
    ///
    /// 注入对照三条：
    /// - 去掉减法（退回 sysvad 原样）⇒ 第 1 行红；
    /// - 用 `framesAccepted - m_AhPresentationOffsetFrames` 不做饱和
    ///   （`ULONGLONG` 下溢成 1.8e19，呈现时钟瞬间跳到宇宙尽头）⇒ 第 2 行红；
    /// - 顺手把 `m_ullLinearPosition` 也减掉 ⇒ 第 3 行红。那个数答的是
    ///   「DMA 读到缓冲区哪儿了」，引擎自己的记账压在它上面。
    #[test]
    fn the_windows_presentation_position_subtracts_and_saturates() {
        let src = read("drivers/windows-vad/Source/Main/minwavertstream.cpp");
        let body = window(&src, "NTSTATUS CMiniportWaveRTStream::GetPresentationPosition", 5200);
        assert!(
            body.contains("framesAccepted - m_AhPresentationOffsetFrames"),
            "呈现位置没有减去下游延迟——这就是 sysvad 原样，即「收下就等于响了」"
        );
        assert!(
            body.contains("(framesAccepted > m_AhPresentationOffsetFrames)"),
            "必须先比较再减：ULONGLONG 直接相减会下溢成 1.8e19，呈现时钟当场作废"
        );
        assert!(
            !body.contains("m_ullLinearPosition -") && !body.contains("ullLinearPosition -"),
            "linear position 不许动：它答的是 DMA 读到哪儿了，引擎的缓冲记账压在它上面"
        );
    }

    /// **Windows：偏移只在建流时读一次，且只读渲染方向。**
    ///
    /// 注入对照：
    /// - 把 `AhSlotLatencyGet` 搬进 `GetPresentationPosition`（「跟着槽走，
    ///   这样新值立刻生效」）⇒ 第 1 行红。那个写法让呈现时钟的偏移在跑动中
    ///   变化，偏移变小时钟就**倒着走**——呈现位置唯一不能丢的性质。
    /// - 去掉 `if (!m_bCapture)` ⇒ 第 2 行红。采集方向那段延迟发生在样本到达
    ///   本机**之前**，从采集位置里减掉它等于声称我们还没交出 App 已经拿到的音频。
    #[test]
    fn the_windows_offset_is_sampled_once_at_open_and_only_for_render() {
        let src = read("drivers/windows-vad/Source/Main/minwavertstream.cpp");
        let hits: Vec<_> = src.match_indices("AhSlotLatencyGet(").collect();
        assert_eq!(
            hits.len(),
            1,
            "这个 getter 在流里只该被调一次（建流时）；出现 {} 次意味着有人让偏移跟着槽走了，\
             而偏移变小会让呈现时钟倒退",
            hits.len()
        );
        let at = hits[0].0;
        let before = &src[at.saturating_sub(400)..at];
        assert!(
            before.contains("if (!m_bCapture)"),
            "只有渲染方向减这个偏移：采集侧那段延迟在样本到达本机之前就发生了"
        );
        assert!(
            src[..at].contains("::Init"),
            "这一次读取必须在 Init 里，不能在任何按帧调用的路径上"
        );
    }

    /// **`sum_ms` 与被声明的那个数必须来自同一次装配。**
    ///
    /// 注入对照：让 `declare_pass` 自己调一次 `assemble_pipelines`（而不是收
    /// `latency_pass` 传进来的那一份）⇒ 本条红。两处各算一份不会有任何报错，
    /// 只会出现「界面说 121 ms、伺服认为已达标、CoreAudio 被告知 79 ms」。
    #[test]
    fn the_declaration_and_the_servo_read_one_and_the_same_assembly() {
        let src = read("core/audiohubd/src/lib.rs");
        let body = window(&src, "fn latency_pass(", 1600);
        assert_eq!(
            body.matches("assemble_pipelines(").count(),
            1,
            "latency_pass 里必须只装配一次"
        );
        assert!(body.contains("declare_pass(inner, entries, &pipelines)"));
        assert!(body.contains("servo_pass(inner, entries, &pipelines)"));
        let decl_body = window(&src, "fn declare_pass(", 3600);
        assert!(
            !decl_body.contains("assemble_pipelines("),
            "declare_pass 自己又装配了一次：那一份与 UI/伺服看到的不是同一次采样"
        );
    }

    /// **mac 侧收 ack 时，`scalar_bits` 必须按整数读。**
    ///
    /// `CTL_VOLUME` 与 `CTL_LATENCY_STATE` 在同一个 `match` 里、取的是同一个
    /// 32 位字段，而两者的解读**互不兼容**。把 latency 那一支写成
    /// `f32::from_bits(msg.scalar_bits)` 是一行看起来在「统一风格」的改动：
    /// 5808 帧读成 8.1e-42，`is_finite()` 放行，`as u32` 得 0——于是驱动答的
    /// 「我现在声明 5808」被记成「我现在声明 0」，daemon 永远重发，
    /// UI 永远显示「没答」。没有任何一处会报错。
    #[test]
    fn the_mac_ack_reads_its_frame_count_as_an_integer() {
        let src = read("core/audiohubd/src/halbridge.rs");
        // 缩进是锚点的一部分：外层那句 `CTL_VOLUME | CTL_IO_STATE |
        // CTL_LATENCY_STATE =>` 也含这个名字，而它下面就是**音量**那一支的
        // `f32::from_bits`。锚错了，这条审计会一直红着说反话。
        let body = window(&src, "\n                    CTL_LATENCY_STATE => {", 700);
        assert!(
            body.contains("frames: msg.scalar_bits"),
            "帧数必须原样取，不许过 f32::from_bits"
        );
        assert!(
            !body.contains("f32::from_bits"),
            "latency 那一支读成了浮点位模式：5808 会变成 8.1e-42，然后是 0"
        );
        assert!(
            body.contains("FLAG_LATENCY_PENDING"),
            "「按住了」与「没答」必须区分得开"
        );
    }

    /// **只声明扬声器方向。**
    ///
    /// 注入对照：把 `HalEndpoint::out(..)` 换成按方向循环（「对称一下更整齐」）
    /// ⇒ 本条红。麦克风那条链的尾级 `hal_mic` 在
    /// `docs/spec-hal-mic-latency.md` 里还是一个 `[0, 500 ms]` 的无回复力自由
    /// 参数，声明它等于把一个随机数广播给系统。
    #[test]
    fn only_the_speaker_direction_is_ever_declared() {
        let src = read("core/audiohubd/src/lib.rs");
        let body = window(&src, "fn declare_pass(", 3600);
        assert!(
            body.contains("HalEndpoint::out(slot as u8)"),
            "端点必须固定是 out"
        );
        // `HalEndpoint::mic(` 是真会被写出来的那一个（`out` 的兄弟构造器，
        // 就在它上面三行）。挑一个**不存在**的名字来断言，等于让这条测试
        // 只在编译失败时才「红」，而编译失败什么也没证明。
        assert!(
            !body.contains("HalEndpoint::mic("),
            "麦克风方向不许声明：hal_mic 还是一个无回复力的自由参数"
        );
        assert!(
            body.contains("e.kind == KIND_SPK") && body.contains("e.dir == DIR_SEND"),
            "挑流的判据必须是 spk + send"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `local_ms` 刻意给一个**看着也像延迟**的数：`frames_for` 若取错字段
    /// （`p.local_ms` 而不是 `p.sum_ms`），第一条测试的 `sum_ms = None` 那行
    /// 会拿到 `Some(528)` 而不是 `None`——测试因此变红，而不是恰好也对。
    fn pipe(sum_ms: Option<f64>) -> PipelineLatency {
        PipelineLatency {
            side: "send".into(),
            stages: Vec::new(),
            local_ms: Some(11.0),
            dev: None,
            peer_stages: Vec::new(),
            peer_local_ms: Some(50.0),
            peer_dev: None,
            peer_age_s: None,
            net_ms: Some(2.0),
            rtt_cross_check_ms: None,
            sum_ms,
            e2e_ms: None,
            residual_ms: None,
            clock_offset_us: None,
            clock_unc_us: None,
            confidence: audiohub_ipc::LatConfidence::LowerBound,
        }
    }

    /// **`sum_ms` 缺席 ⇒ 什么都不声明，绝不用别的字段顶。**
    ///
    /// 注入对照（两行都是有人「让它至少有个数」的写法）：
    ///
    /// | 写法 | 后果 | 本条会 |
    /// |---|---|---|
    /// | `p.sum_ms.or(p.local_ms)` | 对端分项一缺，就把本侧那一半当整条链声明 | **红**（第 1 行） |
    /// | `declared_frames(sum_ms.unwrap_or(0.0))` | 退回今天那句谎，还带上「已声明」的外观 | **红**（第 1 行） |
    #[test]
    fn nothing_is_declared_until_the_whole_chain_has_been_measured() {
        assert_eq!(frames_for(&pipe(None)), None, "对端分项缺席 ⇒ 无从声明");
        assert_eq!(frames_for(&pipe(Some(f64::NAN))), None);
        assert_eq!(frames_for(&pipe(Some(f64::INFINITY))), None);
        assert_eq!(frames_for(&pipe(Some(-1.0))), None, "负延迟只可能是算错了");
        // 而测得到的时候，换算是直白的
        assert_eq!(frames_for(&pipe(Some(121.0))), Some(5808));
        assert_eq!(frames_for(&pipe(Some(0.0))), Some(0), "真测到 0 与没测到不是一回事");
    }

    /// 荒唐值在花掉一次 IPC 之前就被拒。上限与两个驱动里的常量是同一个数。
    #[test]
    fn an_absurd_number_is_refused_here_rather_than_by_the_driver() {
        assert_eq!(MAX_FRAMES, 192_000, "= AH_LATENCY_MAX_FRAMES = kDevice_MaxDeclaredLatency");
        assert_eq!(declared_frames(Some(4_000.0)), Some(192_000), "正好 4 秒，还收");
        assert_eq!(declared_frames(Some(4_000.1)), None);
    }

    /// **ack 与 want 相等才算到位；只发过一次不算。**
    ///
    /// 这条是闭环的核心。注入对照：把 `should_send` 改成
    /// `st.want != Some(want)`（即「我发过就不再发」）⇒ 第 2、4 行红。
    /// 那个写法在一次丢包之后会永远停在「已声明」的外观上。
    #[test]
    fn a_send_is_never_evidence_that_the_property_moved() {
        let never = DeclState::default();
        assert!(should_send(&never, 5808), "驱动从没答过 ⇒ 必发");

        let acked_short = DeclState { want: Some(5808), acked: Some(0), tries: 1, ..Default::default() };
        assert!(should_send(&acked_short, 5808), "答的是 0 而要的是 5808 ⇒ 没到位，继续发");

        let done = DeclState { want: Some(5808), acked: Some(5808), tries: 3, ..Default::default() };
        assert!(!should_send(&done, 5808), "答的与要的相等 ⇒ 到位，不再发");

        // 差在死区内，但确实没对上：还没试满次数就继续试。
        let near = DeclState { want: Some(5808), acked: Some(5805), tries: 2, ..Default::default() };
        assert!(should_send(&near, 5808), "差 3 帧也是没对上，重试次数没用完就还得试");
        let exhausted = DeclState { tries: GIVE_UP_AFTER, ..near };
        assert!(!should_send(&exhausted, 5808), "试满了就别再每秒撞同一堵墙");

        // 而「一次都没答过」是**另一档**：无限重试，因为它会自己好
        // （装了新驱动 / coreaudiod 重启 / 桥重新 attach）。
        let never_answered = DeclState { want: Some(5808), acked: None, tries: 9_999, pending: false };
        assert!(
            should_send(&never_answered, 5808),
            "一次都没答过要一直试：停下就得等到下次 bind，而这条路会自己好"
        );
    }

    /// **目标跨过死区 ⇒ 重试计数清零 ⇒ 新目标一定会被试。**
    ///
    /// 这一条把上一条的「试满了就停」封住，不让它变成「这台设备从此再也不更新
    /// 声明值」。两条合起来才是完整的策略；只有其中一条都会出问题：
    /// 少了停止条件是持续负载，少了这一条是永久卡死。
    #[test]
    fn a_target_that_really_moved_always_gets_tried_again() {
        let mut st = DeclState { want: Some(5808), acked: Some(5805), tries: GIVE_UP_AFTER, pending: false };
        assert!(!should_send(&st, 5808), "先确认它确实卡住了");
        // 链路真的变长了（对端换了网络）
        let fresh = 5808 + DEADBAND_FRAMES;
        let next = advance_want(st.want, fresh).expect("跨过死区 ⇒ 换目标");
        st.want = Some(next);
        st.tries = 0; // 调用点做的事，与 `declare_pass` 里那一行一致
        assert!(should_send(&st, next), "新目标必须被试");
    }

    /// **死区管的是目标，不是重试。**
    ///
    /// 注入对照：把死区搬进 `should_send`（即「差不到 20 ms 就当已到位」）⇒
    /// 上一条的第 4 行红，而且真实后果是一个差 19 ms 的错误声明被永久接受。
    #[test]
    fn the_deadband_moves_the_target_and_never_forgives_a_miss() {
        assert_eq!(DEADBAND_FRAMES, 960, "20 ms @ 48k");
        assert_eq!(advance_want(None, 5808), Some(5808), "第一次总是要有个目标");
        assert_eq!(advance_want(Some(5808), 5810), None, "抖 2 帧 ⇒ 目标不动");
        assert_eq!(advance_want(Some(5808), 5808 + 960), Some(6768), "跨过死区 ⇒ 换目标");
        assert_eq!(advance_want(Some(5808), 5808 - 960), Some(4848), "反方向同样");
        assert_eq!(advance_want(Some(5808), 5808 + 959), None, "差一帧不算跨过");
    }

    /// 排障串必须把「按住了」「没人答」「已到位」三种状态说清楚——
    /// 它们在数值上都可能长成「acked != want」。
    #[test]
    fn the_status_line_tells_held_apart_from_ignored() {
        let held = DeclState { want: Some(5808), acked: Some(0), pending: true, tries: 1 };
        assert!(line("play", &held).contains("takes effect when it stops"));

        let ignored = DeclState { want: Some(5808), acked: None, pending: false, tries: GIVE_UP_AFTER };
        assert!(line("play", &ignored).contains("never acknowledged"));

        let ok = DeclState { want: Some(5808), acked: Some(5808), pending: false, tries: 1 };
        let s = line("play", &ok);
        assert!(!s.contains("never acknowledged") && !s.contains("takes effect"));
        assert!(s.contains("121.0 ms"), "帧数得换算成人读得懂的毫秒: {s}");
    }
}
