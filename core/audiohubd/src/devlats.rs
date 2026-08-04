//! 把 [`audiohub_core::devlat`] 与 [`audiohub_core::devcal`] **接进遥测**：
//! 规格 §3.2 的级 2（`cap_dev`）与级 9（`play_dev`）。
//!
//! ## 这个文件为什么存在 —— 它补的是一个「有定义、无生产者」的洞
//!
//! 到 `3ed03ff` 为止，`StageId::PlayDev` / `StageId::CapDev` 在
//! `core/audiohub-core/src/latency.rs` 里**只有枚举定义、字符串映射、序号映射
//! 和两条单测**，全仓库没有任何一处生产代码写入它们；`devlat` 模块除了
//! `pub mod devlat;` 也没有被任何地方调用（`lib.rs` 亲口写着「此刻 `dev` 恒
//! `Unavailable`」）。结果是 `sum_ms` **系统性少一整段**：
//!
//! ```text
//! spk/send 那条链实测   sum_ms  p50 = 110.96 ms      ← 上报的
//!                    + play_dev      41.92 ms      ← 从来没上报过
//!                    真实 e2e ≈      153    ms
//! ```
//!
//! ⚠ **接线之后 `sum_ms` 会往上跳，那不是延迟变大**——是一直存在的那一段
//! 终于被算进来了。这句话必须原样传到 UI 上，否则用户会把它读成一次退化。
//!
//! ## 三条纪律（与 `devlat` 的三条并列，不是复述）
//!
//! 1. **读不到就是 `Unavailable`，绝不用 0 冒充。** 本模块不产生任何兜底值：
//!    查询失败 ⇒ `DevLatency::unavailable()` ⇒ 那条流的 `sum_ms` 变 `None`。
//!    「没有总数」比「一个漂亮且错误的总数」诚实。
//! 2. **「这条流上没有这台设备」与「这台设备读不到」是两件事。**
//!    前者是 `None`（本级不存在，不进求和），后者是 `Some(unavailable())`
//!    （本级存在但读不到，毒化求和）。混成一个的后果：模式 B 的虚拟扬声器源
//!    没有采集声卡，若按「读不到」处理，它的 `sum_ms` 会**整个消失**——
//!    一个今天在用的功能会静默失去延迟显示。判据见 [`stream_dev`]。
//! 3. **查询不上音频节拍。** `devlat::query` 是 CoreAudio / WASAPI 的属性读，
//!    微秒级但要过 IPC 到 `coreaudiod` / 音频引擎；它只在 1 s 的 ticker 上跑，
//!    而且带缓存（见 [`DevLatCache`]）。10 ms 线程上的约束见
//!    `latency.rs` 文件头约束 3。
//!
//! ## Windows 的那次开流标定
//!
//! `devlat` 在 Windows 上只能报 `GetDevicePeriod`（实测**低报 4.2 倍**），
//! 真值要开一条流才读得到。那次开流由本模块**在后台线程上做一次**，
//! 结果缓存在 [`CalSlot`] 里（键 = 端点名 + 速率）。不放在 ticker 线程上的
//! 理由很实在：标定要走 60 个 10 ms 事件 ≈ 0.6 s，而 ticker 的节拍就是 1 s。

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use audiohub_core::audio::DeviceKind;
use audiohub_core::devcal::OutputCalibration;
use audiohub_core::devlat::{self, DevLatencyParts, DevTarget};
use audiohub_core::latency::DevLatency;

use crate::lk;
#[cfg(windows)]
use crate::dlog;

#[cfg(windows)]
use audiohub_core::devcal;
#[cfg(test)]
use audiohub_core::latency::LatSource;
#[cfg(windows)]
use std::sync::atomic::Ordering;

/// 缓存有效期。默认设备**换**了由 epoch 立刻打掉（见 [`DevLatCache::read`]），
/// 这个 TTL 管的是另一件事：同一台设备上 `BufferFrameSize` 被别的 App 改小
/// （CoreAudio 允许任何客户端改它）、采样率被切换。那些不 bump epoch。
///
/// 5 s 是在「跟得上变化」与「别每秒问四次 CoreAudio」之间取的：上报节拍 1 s，
/// 于是最多五拍显示一个旧值，而那五拍里 UI 显示的每一个数都曾经是真的。
const TTL: Duration = Duration::from_secs(5);

/// 两个方向的设备固有延迟读数。**按方向一份，不按流一份**——它是设备属性，
/// 不是队列：同时有 3 条流送本机默认输出时，它们经历的是**同一个** `play_dev`
/// （与 `play_ring` 是站点级读数同一条道理，规格 §7.2 R7）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct DevLats {
    /// 级 2 `cap_dev`：本机默认**输入**设备。
    pub input: DevLatency,
    /// 级 9 `play_dev`：本机默认**输出**设备。
    pub output: DevLatency,
}

impl DevLats {
    /// 什么都没查到的形状。**两个都 `Unavailable`，不是 0。**
    pub(crate) fn unavailable() -> DevLats {
        DevLats { input: DevLatency::unavailable(), output: DevLatency::unavailable() }
    }
}

/// **这一条流**的设备级读数，`None` = 这条流的路径上没有这台设备。
///
/// # 为什么必须按流判，不能一律挂上去
///
/// `dev` 的语义是「**这条流**经过的那台声卡」。本机的默认输入是一台真实设备，
/// 但一条以**虚拟扬声器**为源的发送流（模式 B：App 把音频放进
/// 「AudioHub – 对端 扬声器」，我们从 `hal_spk` 环读出来发走）**根本没经过它**。
/// 把默认麦克风的延迟挂到那条流上，是往总数里加一段这条链路上不存在的时间——
/// 与「用 0 填补缺项」是同一类谎，只是符号相反。
///
/// # 判据取自**实际存在的级**，不是取自配置
///
/// - 发送侧：`cap_ring`（级 1）在不在。它由 `MicSource::depths()` 发射，
///   而 `SysAudioFrames::depths()` 只发 `src_fifo`、`HalSpeakerSource` 只发
///   `hal_spk`、`ToneSource` 一级都不发。**`cap_ring` 在 ⟺ 源是真实采集设备。**
/// - 接收侧：`is_spk || monitor`。这与 `attach_output_tails` 里决定要不要挂
///   `play_ring` 的判据**是同一个**——刻意如此：`play_ring` 与 `play_dev`
///   是同一条物理路径上前后相邻的两级，一个在而另一个不在是不可能的。
///
/// # 那些「没有设备级」的流，总数仍然是完整的
///
/// 纪律 2：`None` 不进求和，也**不**把求和打成 `None`。一条 `hal_spk` 源的流
/// 上，链路真的从虚拟扬声器环开始；说它「缺了采集声卡那一段」是不对的。
///
/// ⚠ 这条留了一个已知的**未闭合项**：模式 B 的源侧其实还有一段——App 把样本写进
/// CoreAudio、我们的驱动 IOProc 还没搬走的那一截（本机实测 512 帧 = 10.7 ms 的
/// `io_buffer`）。要报它就得按 UID 查我们自己那张虚拟卡，而虚拟卡的 UID 是
/// 运行时按对端生成的。本轮**不报**，并且如实按「本级不存在」处理而不是编一个数。
/// 见 `docs/research-device-latency-property.md` §4.2 第 1 步。
///
/// ⚠⚠ **第二个未闭合项：默认设备恰好是我们自己那张虚拟卡时，读到的是一句谎。**
/// 本机 2026-08-04 实测就是这个配置——默认输出与默认输入都是
/// 「AudioHub – WIN-IR01HVEFU7G 扬声器 / 麦克风」，两者都报
/// `device=0 safety=0 stream=0 io_buffer=512` = 10.667 ms，标 `Api`。
/// 这个 10.667 ms 对**本模块**是如实转述（驱动确实这么声明），但那个声明本身
/// 是错的：`AudioHubDriver.c` 的 `case kAudioDevicePropertyLatency` 硬编码 0，
/// 而真相是约 150 ms 后在另一台机器上响（两处 PHASE 2 MARKER 记着这件事）。
/// **本轮不在这里打补丁**：判据只能是「驱动声明了多少」，在 daemon 侧按名字认出
/// 自家的卡再偷偷改数，等于让同一个量有两个互相矛盾的来源，而排障的人只会看到
/// 其中一个。修在驱动那一侧，一次修好，两个平台对称。
pub(crate) fn stream_dev(
    is_send: bool,
    has_capture_device: bool,
    has_output_device: bool,
    lats: DevLats,
) -> Option<DevLatency> {
    if is_send {
        has_capture_device.then_some(lats.input)
    } else {
        has_output_device.then_some(lats.output)
    }
}

/// 一次查询的原始形态 + 折出来的读数，只为报告/日志留着。
#[derive(Debug, Clone)]
pub(crate) struct DevLatReport {
    pub parts: DevLatencyParts,
    pub calibrated: bool,
}

impl DevLatReport {
    /// 一行人读的排障串。`None` 的那些字段不编。
    pub fn line(&self, what: &str) -> String {
        let p = &self.parts;
        let t = p.total();
        let ms = t.ms().map_or("unavailable".to_string(), |v| format!("{v:.2} ms"));
        let items: Vec<String> =
            p.parts.iter().map(|(n, f)| format!("{n}={f}f")).collect();
        let miss = if p.missing.is_empty() {
            String::new()
        } else {
            format!(" missing=[{}]", p.missing.join(","))
        };
        let cal = if self.calibrated { " (calibrated)" } else { "" };
        let err = p.error.as_deref().map_or(String::new(), |e| format!(" err={e}"));
        format!(
            "{what}: {ms} [{}]{miss} rate={} transport={:?} source={:?}{cal} dev={:?}{err}",
            items.join(" "),
            p.rate,
            p.transport,
            t.source,
            p.device,
            err = err,
        )
    }
}

struct State {
    /// 上次刷新时看到的设备代号（`dev_in_epoch + dev_out_epoch`）。
    epoch: u64,
    at: Option<Instant>,
    lats: DevLats,
    input: Option<DevLatReport>,
    output: Option<DevLatReport>,
}

/// 标定结果的落点。`Some(Ok)` = 标定成功；`Some(Err)` = 试过并失败了
/// （**不再重试**，否则每 5 s 开一条流）；`None` = 还没试。
///
/// `allow(dead_code)`：只有 Windows 读它（mac 的四项属性免开流可读，
/// 见 `devcal` 文件头）。**结构本身在两个平台都编译**，与
/// `halbridge_win::wire` 同一条理由——只在目标平台存在的类型，就只在目标平台
/// 才发现写错了。
#[cfg_attr(not(windows), allow(dead_code))]
struct CalSlot {
    result: Option<Result<OutputCalibration, String>>,
    /// 标定跑在哪个设备代号上。代号变了就作废重来——用户换了默认输出，
    /// 旧标定属于另一台端点。
    epoch: u64,
}

/// 设备固有延迟的缓存。
///
/// # 刷新时机：epoch 变化**或**超过 [`TTL`]
///
/// 只靠 TTL 会让「换了默认输出」最多迟 5 s 才反映到读数上；只靠 epoch 会漏掉
/// 不 bump epoch 的变化（别的 App 改了 `BufferFrameSize`、设备切了采样率）。
/// 两条都要。
pub(crate) struct DevLatCache {
    st: Mutex<State>,
    /// `allow(dead_code)`：见 [`CalSlot`]，只有 Windows 走标定。
    #[cfg_attr(not(windows), allow(dead_code))]
    cal: Arc<Mutex<CalSlot>>,
    /// 标定线程在不在跑。**不是**「标定完没完」——那是 `CalSlot::result`。
    /// 存在的唯一理由是防止 ticker 每 5 s 又开一条标定流。
    #[cfg_attr(not(windows), allow(dead_code))]
    cal_running: Arc<AtomicBool>,
}

impl DevLatCache {
    pub(crate) fn new() -> DevLatCache {
        DevLatCache {
            st: Mutex::new(State {
                epoch: u64::MAX, // 与任何真实 epoch 都不等 ⇒ 第一次读必刷新
                at: None,
                lats: DevLats::unavailable(),
                input: None,
                output: None,
            }),
            cal: Arc::new(Mutex::new(CalSlot { result: None, epoch: u64::MAX })),
            cal_running: Arc::new(AtomicBool::new(false)),
        }
    }

    /// 取两个方向的读数，必要时刷新。**在 1 s 的 ticker 上调，不在音频线程上。**
    pub(crate) fn read(&self, epoch: u64) -> DevLats {
        self.read_at(epoch, Instant::now())
    }

    /// [`read`] 的全部内容，只是「现在」由调用方给。
    ///
    /// 存在的唯一理由是 **TTL 可测**：一条只能靠 `Instant::now()` 触发的过期
    /// 规则，验证它就得真等 5 秒——于是没人验证它。生产路径走的就是这个函数。
    fn read_at(&self, epoch: u64, now: Instant) -> DevLats {
        {
            let st = lk(&self.st);
            if !stale(st.epoch, st.at, epoch, now) {
                return st.lats;
            }
        }
        // 查询在锁外做：`devlat::query` 要过 IPC 到 coreaudiod，而这把锁也被
        // `daemon.status` 的报告路径拿。锁里做 IPC = 报告线程陪等音频服务。
        let (input, output) = self.query_both(epoch);
        let mut st = lk(&self.st);
        st.epoch = epoch;
        st.at = Some(now);
        st.lats = DevLats { input: input.parts.total(), output: output.parts.total() };
        st.input = Some(input);
        st.output = Some(output);
        st.lats
    }

    fn query_both(&self, epoch: u64) -> (DevLatReport, DevLatReport) {
        let input = DevLatReport {
            parts: devlat::query(DeviceKind::Input, DevTarget::Default),
            calibrated: false,
        };
        let mut out_parts = devlat::query(DeviceKind::Output, DevTarget::Default);
        let calibrated = self.fold_calibration(&mut out_parts, epoch);
        (input, DevLatReport { parts: out_parts, calibrated })
    }

    /// Windows：把那次开流标定折进输出读数；顺带在需要时把标定踢起来。
    ///
    /// 非 Windows 平台这个函数**什么都不做**——mac 的四项属性免开流可读，
    /// 给它开一条标定流是纯风险（`devcal` 文件头）。
    #[cfg(windows)]
    fn fold_calibration(&self, parts: &mut DevLatencyParts, epoch: u64) -> bool {
        let hit = {
            let slot = lk(&self.cal);
            match (&slot.result, slot.epoch == epoch) {
                (Some(Ok(c)), true) => Some(c.clone()),
                _ => None,
            }
        };
        match hit {
            Some(c) => devcal::apply_output_calibration(parts, &c),
            None => {
                self.kick_calibration(epoch);
                false
            }
        }
    }

    #[cfg(not(windows))]
    fn fold_calibration(&self, _parts: &mut DevLatencyParts, _epoch: u64) -> bool {
        false
    }

    /// 开一条一次性线程做标定。**至多一条在跑**，且同一个 epoch 至多试一次。
    ///
    /// 线程是脱管的（不 join）：daemon 关停时它最多再活 0.6 s，而它持有的
    /// 只有一条共享模式静音流与一个 COM 套间，进程退出会清干净。用 join 会
    /// 把关停路径挂在一个 WASAPI 事件等待上——那是拿关停的确定性换一点整洁。
    #[cfg(windows)]
    fn kick_calibration(&self, epoch: u64) {
        {
            let slot = lk(&self.cal);
            // 同一个 epoch 已经有结论（成功或失败）⇒ 不重试。失败也不重试：
            // 每 5 s 开一条流去撞同一个错误，是把一次故障变成一个持续负载。
            if slot.epoch == epoch && slot.result.is_some() {
                return;
            }
        }
        if self.cal_running.swap(true, Ordering::SeqCst) {
            return;
        }
        let cal = Arc::clone(&self.cal);
        let running = Arc::clone(&self.cal_running);
        // 闸的**第二份**句柄：线程里那一份被 `move` 进闭包了，而建线程失败时
        // 闭包根本不存在，得有另一份才放得回去。
        let gate = Arc::clone(&self.cal_running);
        let spawned = std::thread::Builder::new()
            .name("devcal".into())
            .spawn(move || {
                let r = devcal::calibrate_output(DevTarget::Default);
                match &r {
                    Ok(c) => dlog!(
                        "devcal {:?}: write_to_play={}f ({:.2} ms) = padding {}f + K {}f；\
                         引擎周期 {}f，端点缓冲 {}f，稳态样本 {} 个、全幅 {}f",
                        c.device,
                        c.frames,
                        c.ms().unwrap_or(f64::NAN),
                        c.padding_frames,
                        c.k_frames().unwrap_or(0),
                        c.period_frames,
                        c.buffer_frames,
                        c.samples,
                        c.spread_frames
                    ),
                    // 失败不是致命的：`play_dev` 退回 `GetDevicePeriod` 那个
                    // **偏低 4.2 倍**的下限，而它标 `Unreliable` ⇒ UI 永远带「≥」。
                    // 一个带「≥」的下限比一个来历不明的精确值诚实。
                    Err(e) => dlog!(
                        "devcal 标定失败，play_dev 保留 GetDevicePeriod 那个偏低 4.2 倍的下限: {e}"
                    ),
                }
                *lk(&cal) = CalSlot { result: Some(r), epoch };
                running.store(false, Ordering::SeqCst);
            });
        // ⚠ 建线程失败也必须把闸放回去。漏掉这一行，`cal_running` 会**永远**停在
        // true，于是这台机器在本次进程生命周期内**再也不会标定**——而症状只是
        // `play_dev` 一直显示那个偏低 4.2 倍的下限，没有任何报错。
        if let Err(e) = spawned {
            gate.store(false, Ordering::SeqCst);
            dlog!("devcal 无法开标定线程: {e}");
        }
    }

    /// 排障用的两行读数。`daemon.status` 与 stderr 共用。
    pub(crate) fn report(&self) -> Vec<String> {
        let st = lk(&self.st);
        let mut v = Vec::new();
        if let Some(r) = &st.input {
            v.push(r.line("cap_dev"));
        }
        if let Some(r) = &st.output {
            v.push(r.line("play_dev"));
        }
        v
    }
}

/// 缓存该刷新了吗？**纯函数，好让 TTL 与 epoch 两条规则各自可测。**
fn stale(cached_epoch: u64, at: Option<Instant>, epoch: u64, now: Instant) -> bool {
    match at {
        None => true,
        Some(t) => cached_epoch != epoch || now.duration_since(t) >= TTL,
    }
}

/// 两个方向的设备读数，够不够格把总和升级成一个**精确**的端到端物理量？
///
/// 判据是 `LatSource::is_exact()`（即两侧都是 `Api`），**不是「有没有值」**。
/// 这条区别是 `lib.rs` 里那段 ⚠⚠ 的执行点：Windows 的 `GetDevicePeriod` 有值，
/// 而它低报 4.2 倍；标定值也有值，而它带着 8 ms 的开流竞态（`devcal` 文件头）。
/// 「有值」和「是真值」是两件事。
///
/// `None`（这条流上没有这台设备）**同样不够格**。理由不是它不可信，而是
/// 「这条链路上没有设备级」目前只在两种情形下成立，两种都还有未建模的一截：
/// 模式 B 的虚拟扬声器源漏掉 App→驱动那 512 帧，桥接/虚拟麦克风尾级漏掉
/// 下游那张虚拟卡自己的缓冲。宣布 `Full` 就是宣布那两截不存在。
///
/// ⚠ **旧版本对端会让总数变成「无法测量」，这是刻意的。**
/// `3ed03ff` 及更早的对端在线上恒发 `dev: Some(unavailable)`（「我有这一级，
/// 但我读不到」）。按本模块的规则，那会让 `compose_sum_ms` 返回 `None`，
/// UI 显示「无法测量」而不是接线前的「≥111 ms」。
///
/// 为什么不给旧对端开个后门（把 `Some(unavailable)` 当 `None` 放行）：
/// **线上没有任何字段能把「旧对端」与「新对端但设备真的读不到」分开。**
/// 开了这个后门，一个真实的设备故障会被当成版本差异静默放行——正是本项目
/// 反复栽的那种形态。两端一起升级即恢复，而这个代价是一次性的、可见的。
pub(crate) fn both_exact(local: Option<DevLatency>, peer: Option<DevLatency>) -> bool {
    matches!((local, peer), (Some(l), Some(p)) if l.source.is_exact() && p.source.is_exact())
}

/// 求和里设备级的那一份。`None` ⇒ 这条流没有这一级 ⇒ 贡献 0（**不是缺项**）；
/// `Some` ⇒ 读得到就贡献它，读不到就把整个求和打成 `None`。
///
/// 返回 `Option<f64>`：`None` = 毒化。与 `DevLatency::ms()` 的 `None` 语义一致，
/// 只是这里多了一层「本级不存在」。
///
/// ⚠ 那句 `Some(0.0)` 是这个文件里最危险的一行，它离「用 0 填补缺项」只有一步。
/// 分界线是：`None`（外层）说的是**本级不存在**，`Some(unavailable())` 说的是
/// **本级存在但读不到**。前者贡献 0 是对的（没有的东西不占时间），后者贡献 0
/// 是撒谎。两条路径在下面是分开的，`dev_contributes_zero_only_when_absent`
/// 那条测试盯着它们不合流。
pub(crate) fn dev_sum_ms(dev: Option<DevLatency>) -> Option<f64> {
    match dev {
        None => Some(0.0),
        Some(d) => d.ms(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api(frames: u32) -> DevLatency {
        DevLatency { frames, rate: 48_000, source: LatSource::Api }
    }
    fn lats(inp: DevLatency, out: DevLatency) -> DevLats {
        DevLats { input: inp, output: out }
    }

    /// **发送流拿输入设备，接收流拿输出设备。** 拿反了不会有任何报错——
    /// 两个都是合法的 `DevLatency`，只是数字错了。本机实测两者差 3 倍
    /// （MacBook Pro Microphone 41.69 ms vs Speakers 29.25 ms），
    /// 而在一台接了蓝牙耳机的机器上会差一个数量级。
    #[test]
    fn a_send_stream_takes_the_capture_device_and_a_recv_stream_the_output_one() {
        let l = lats(api(2001), api(1403));
        assert_eq!(stream_dev(true, true, true, l).unwrap().frames, 2001, "send ⇒ 输入设备");
        assert_eq!(stream_dev(false, true, true, l).unwrap().frames, 1403, "recv ⇒ 输出设备");
    }

    /// **路径上没有那台设备 ⇒ `None`，而不是把默认设备的数挂上去。**
    ///
    /// 这条盯的是模式 B 那个真实场景：源是虚拟扬声器环（`hal_spk`），
    /// 一条采集流都没开，而本机同时有一只默认麦克风。把它的 41.69 ms
    /// 加进去，就是往一条不经过麦克风的链路里塞 41.69 ms。
    #[test]
    fn a_stream_without_that_device_reports_no_device_stage_at_all() {
        let l = lats(api(2001), api(1403));
        assert_eq!(stream_dev(true, false, true, l), None, "hal_spk / sysaudio 源没有采集声卡");
        assert_eq!(stream_dev(false, true, false, l), None, "纯桥接 / 纯虚拟麦克风尾级没走真实输出");
    }

    /// **「本级不存在」贡献 0，「本级存在但读不到」毒化求和。**
    ///
    /// 注入对照（每一行都是一次「有人把两者合流」的写法）：
    ///
    /// | 写法 | 后果 | 本条会 |
    /// |---|---|---|
    /// | `dev.map_or(Some(0.0), \|d\| d.ms())` … 再对 `None` 兜底成 0 | 读不到的蓝牙耳机按 0 计入 | **红**（第 2 行断言） |
    /// | `dev.and_then(\|d\| d.ms())` | 没有设备级的流整条 `sum_ms` 消失 | **红**（第 1 行断言） |
    #[test]
    fn dev_contributes_zero_only_when_absent() {
        assert_eq!(dev_sum_ms(None), Some(0.0), "本级不存在 ⇒ 不占时间，也不毒化");
        assert_eq!(
            dev_sum_ms(Some(DevLatency::unavailable())),
            None,
            "本级存在但读不到 ⇒ 必须毒化；按 0 计入会让蓝牙耳机看着和模拟输出一样好"
        );
        assert_eq!(dev_sum_ms(Some(api(480))), Some(10.0));
        // 速率 0 与 Unavailable 同权：帧数没有速率换算不成毫秒
        assert_eq!(dev_sum_ms(Some(DevLatency { frames: 480, rate: 0, source: LatSource::Api })), None);
    }

    /// **只有两侧都 `Api` 才够格叫「精确」。** 四种不够格的取值逐条钉死。
    ///
    /// 特别是 `Assumed`：Windows 的标定值就是这一档（开流竞态实测可差 8 ms）。
    /// 把它算作精确，Windows 侧的「≥」会消失，而 41.9 ms 的内部构成从未分解过
    /// （`docs/spec-playdev-measurement.md` §9.1）。
    #[test]
    fn only_two_api_readings_earn_the_word_exact() {
        let ok = api(512);
        assert!(both_exact(Some(ok), Some(ok)));
        for bad in [LatSource::Assumed, LatSource::Unreliable, LatSource::Unavailable] {
            let d = DevLatency { frames: 512, rate: 48_000, source: bad };
            assert!(!both_exact(Some(d), Some(ok)), "{bad:?} 在本侧");
            assert!(!both_exact(Some(ok), Some(d)), "{bad:?} 在对端");
        }
        assert!(!both_exact(None, Some(ok)), "本侧没有设备级 ⇒ 那一截未建模 ⇒ 不许叫 Full");
        assert!(!both_exact(Some(ok), None), "对端没有设备级同理");
        assert!(!both_exact(None, None));
    }

    /// 缓存两条失效规则各自可测：epoch 变了立刻失效，没变也最多 [`TTL`]。
    #[test]
    fn the_cache_expires_on_both_a_device_change_and_the_ttl() {
        let t0 = Instant::now();
        assert!(stale(7, None, 7, t0), "从来没查过 ⇒ 必查");
        assert!(!stale(7, Some(t0), 7, t0 + Duration::from_secs(1)), "同一台设备、一秒内 ⇒ 命中");
        assert!(stale(7, Some(t0), 8, t0 + Duration::from_millis(1)), "换了默认设备 ⇒ 立刻失效");
        assert!(stale(7, Some(t0), 7, t0 + TTL), "到点 ⇒ 失效");
        assert!(!stale(7, Some(t0), 7, t0 + TTL - Duration::from_millis(1)));
    }

    /// **旧版本对端 ⇒ 总数变「无法测量」，而不是悄悄按 0 算。**
    ///
    /// `3ed03ff` 及更早的对端恒发 `dev: Some(unavailable)`。本条把那个形态钉死：
    /// 它必须毒化求和。看着像退步（UI 从「≥111 ms」变成「无法测量」），
    /// 但那个 111 里本来就少了对端声卡那一段，而旧对端**说不出**它是多少。
    ///
    /// 注入对照：给 `dev_sum_ms` 加一句「`Unavailable` 就按 0 放行」⇒ 本条红。
    /// 那一句正是「给旧对端开后门」的写法，而它会同时放行真实的设备故障。
    #[test]
    fn an_old_peer_that_cannot_read_its_device_makes_the_total_unmeasurable() {
        let old_peer = Some(DevLatency::unavailable());
        assert_eq!(dev_sum_ms(old_peer), None, "旧对端自陈读不到 ⇒ 总数没有，不是少一段");
        // 而它与「这条流没有设备级」在类型上只差一层 `Some`
        assert_eq!(dev_sum_ms(None), Some(0.0));
        assert!(!both_exact(Some(api(512)), old_peer), "更不许升级成 Full");
    }

    /// **真机冒烟**：走真的 `devlat::query`，只断言不变量。
    ///
    /// 不断言具体数值（取决于插了什么设备），断言的是本模块的合同：
    /// 出了数就有速率，出不了数就是 `Unavailable` 而不是一个 0 ms 的读数。
    #[test]
    fn reading_the_real_default_devices_never_fabricates_a_zero() {
        let c = DevLatCache::new();
        let l = c.read(1);
        for (what, d) in [("input", l.input), ("output", l.output)] {
            match d.source {
                LatSource::Unavailable => {
                    assert_eq!(d.ms(), None, "{what}: Unavailable 必须没有毫秒值");
                }
                _ => {
                    let ms = d.ms().unwrap_or_else(|| panic!("{what}: 非 Unavailable 必有毫秒值"));
                    assert!(ms.is_finite() && ms >= 0.0, "{what}: {ms} 不是合法毫秒值");
                    assert!(ms < 1_000.0, "{what}: 单台设备的固有延迟不该到秒级");
                }
            }
        }
        // 同一个 epoch 再读一次必须命中缓存（同一个值，且不再查设备）
        assert_eq!(c.read(1), l);
        assert!(!c.report().is_empty(), "查过就得说得出查到了什么");
    }
}
