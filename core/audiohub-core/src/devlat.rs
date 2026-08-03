//! 设备固有延迟的平台查询：规格 §3.2 的级 2（`cap_dev`）与级 9（`play_dev`）。
//!
//! P0 把这两级恒填 `LatSource::Unavailable`，于是 `sum_ms` 恒为 `None`、UI 恒带
//! 「≥」。本模块是 P1 的那一半：**把下限换成真值**。规格 §3.4 已经给出这一步值
//! 多少——「声卡缓冲不可读（`BufferSize::Default`）10–20 ms 系统性低估，典型
//! CoreAudio 512 帧@48k = 10.7 ms、WASAPI 共享 ~10 ms」。在 1000 ms 的故障面前
//! 这点无所谓，在 186 ms 的正常态下它是 10%。
//!
//! ## 三条不可让步的纪律
//!
//! 1. **读不到就是 `Unavailable`，绝不用 0 冒充 `Api`。** 规格原话：用 0 填补会让
//!    蓝牙耳机（真实 +150~250 ms）看起来和模拟输出一样好。所以本模块的失败路径
//!    一律走 `DevLatencyParts::missing`，而不是「那一项按 0 算」。
//! 2. **只做属性查询。** 不开流、不建 IOProc、不改默认设备、不碰音频路径。
//!    daemon 正在服务用户音频，一次「顺手把设备打开看看」就可能踢掉正在播的流；
//!    在 macOS 上碰输入单元还会撞 TCC 弹窗（`audio.rs:75-79` 已经为同一个理由
//!    禁掉了 `default_input_config()`）。本模块**全部**走
//!    `AudioObjectGetPropertyData` / `IAudioClient` 的免初始化查询。
//! 3. **数从哪来就标哪个 `LatSource`。** 直接读平台属性 ⇒ `Api`；平台没有属性、
//!    靠模型推出来的 ⇒ `Assumed`（规格 §3.4 点名 cpal 的 macOS `playback` 是
//!    硬编码双缓冲假设，低估 5–20 ms，**本模块因此一个 cpal 的延迟数都不用**）；
//!    传输方式已知少报 ⇒ `Unreliable`。
//!
//! ## macOS：四项求和
//!
//! Core Audio 把一条输出路径的固有延迟拆成四段，缺一段就少算一段：
//!
//! | 分量 | 属性 | 含义 |
//! |---|---|---|
//! | `device` | `kAudioDevicePropertyLatency` | 设备自己声明的 DAC/ADC 延迟 |
//! | `safety_offset` | `kAudioDevicePropertySafetyOffset` | HAL 为防欠载额外提前的量 |
//! | `stream` | `kAudioStreamPropertyLatency` | 该方向的流再加的量（常为 0） |
//! | `io_buffer` | `kAudioDevicePropertyBufferFrameSize` | IOProc 一次交出的帧数 |
//!
//! `io_buffer` 必须在内：它正是规格 §3.4 说的那 512 帧。它与我们自己的
//! `play_ring` / `cap_ring` **不重复**——那一级数的是还没被 IOProc 取走的样本，
//! 这一级数的是已经交给 Core Audio、还没到 DAC 的样本。
//!
//! ## Windows：共享模式下这个数的确切语义
//!
//! WASAPI 能免初始化读到的只有 `IAudioClient::GetDevicePeriod`（引擎的周期性
//! 处理间隔）与 `GetMixFormat`（引擎速率）。真正的 `GetStreamLatency` **要求先
//! `Initialize`**，而 `Initialize` 会在音频引擎里建出一条流——纪律 2 不允许，
//! 并且对**采集**端点还会触发 Win10+ 的麦克风隐私门（既可能弹窗，也可能直接
//! `E_ACCESSDENIED`）。
//!
//! 所以 Windows 侧报的是**一个设备周期**，并且它是**下限**：共享模式的实际路径
//! 至少还有引擎自己那一档周期，加上端点硬件那一截——两者都不在 `GetDevicePeriod` 里。
//! 把 2× 周期硬写进来同样是编造，所以这里只报读到的那一份。
//!
//! ### 这个下限**低报 4.2 倍**（2026-08-04 于 30-win 实测，`Assumed` → `Unreliable`）
//!
//! 本模块此前把它标 `Assumed`（「按模型算的，可能偏几毫秒」）。实测否掉了那个量级：
//!
//! ```text
//! GetDevicePeriod 的 default              =  480 帧 = 10.00 ms   ← 本模块报的
//! written − IAudioClock::GetPosition()    = 2012 帧 = 41.92 ms   ← 写进去到播出来
//!   其中 padding@event (=bufferSize−period) = 576 帧 = 12.00 ms
//!   其中 引擎 + KS 传输 + 驱动 + 设备 `K`   = 1436 帧 = 29.92 ms
//! ```
//!
//! `K` 用「把端点缓冲撑到 4.5 倍」验证死：1056 / 2400 / 4800 帧三档下 `K` 分别是
//! 1436 / 1435 / 1435 帧——**纹丝不动**，所以 41.9 不是记账伪影。换一条完全不同的
//! 硬件通路（NVIDIA HDMI 端点）`K = 1498 帧 = 31.2 ms`，只差 1.3 ms ⇒ 这 30 ms
//! **不是设备特性，是 Windows 共享引擎 + KS 传输**。
//! 全部证据：`docs/spec-playdev-measurement.md` §3。
//!
//! 按本模块 `worse()` 的定义，「API 答了，而且已知差着一个数量级」正是 `Unreliable`
//! 而不是 `Assumed`。所以 Windows 的 `base_source` 改标 **`Unreliable`**：数照报
//! （它是真下限，比没有强），但**永远带「≥」，永远不许把总和升级成精确值**。
//!
//! ⚠ 两条**不要再试**的路（都已实测证伪，别浪费下一轮）：
//! - `GetStreamLatency`：这两个端点上**恒返回 0 hns**。`Initialize` 前后、事件驱动
//!   与否、四种 client properties 组合，全是 0。这个 API 在盒内 USB/HDMI 类驱动上是废的。
//! - 端点属性存储：73 条逐条 dump，**没有任何一条是延迟**。
//!
//! 要把它变成真值只剩一条路：开一条**共享模式静音流**，按
//! `written − IAudioClock::GetPosition()` 做一次性标定（`GetPosition` 的 `qpcPosition`
//! 实测陈旧度 ≈0，读数新鲜）。那违反纪律 2（要建流），属第二轮、需用户明确授权。
//! 在那之前，`Unreliable` + 「≥」是唯一诚实的形态。
//!
//! WASAPI 也没有 macOS 那个直白的传输方式属性，所以「这是不是蓝牙/HDMI」要从端点
//! 属性库里的 `PKEY_AudioEndpoint_FormFactor` + `PKEY_Device_EnumeratorName` 拼出来
//! （见 `imp::transport_of`）。少了这一步，Windows 上一副蓝牙耳机与一对模拟音箱的
//! 读数会**完全一样**——纪律 1 的同一个失败形态，只是发生在定性而非数值上。

use serde::{Deserialize, Serialize};

use crate::audio::DeviceKind;
use crate::latency::{DevLatency, LatSource};

/// Windows 侧 `IAudioClient::GetDevicePeriod` 那个读数的可信度标签。
///
/// **故意抽成一个不带 `cfg` 的常量，而不是写死在 `#[cfg(windows)] mod imp` 里。**
/// 理由是可测性：这个判定的依据是一次 30-win 实测，而**改坏它的人多半坐在 macOS 前**
/// （本项目的主开发机）。写在 `imp` 里，改回 `Assumed` 在 mac 上一条测试都不会红，
/// 要等到 Windows 那边跑测试才发现——而 Windows 侧的音频测试本来就跑得少。
/// 抽出来之后 `a_windows_device_period_is_a_floor_not_a_truth` 在 mac 上就能盯住它。
///
/// 为什么是 `Unreliable` 而不是 `Assumed`：见文件头「这个下限低报 4.2 倍」。
/// 一句话——`GetDevicePeriod` 报 10.00 ms，同一端点写到播实测 41.92 ms。
/// `Assumed` 的语义是「按模型算的，可能偏几毫秒」，这里偏了 31.9 ms。
pub const WINDOWS_DEVICE_PERIOD_SOURCE: LatSource = LatSource::Unreliable;

/// 设备的物理连接方式。**只用来给读数定性**，不参与换算。
///
/// 存在的唯一理由是 `underreports()`：蓝牙 A2DP 真实延迟 150–250 ms，而
/// `kAudioDevicePropertyLatency` 常只报 20–30 ms。那个 20 ms 是真读到的，
/// 不是缺项——所以它不能是 `Unavailable`；但采信它会给出一个漂亮且完全错误的
/// 数字——所以它也不能是 `Api`。`Unreliable` 就是为这一格存在的。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    BuiltIn,
    Usb,
    Thunderbolt,
    Pci,
    FireWire,
    Bluetooth,
    Hdmi,
    DisplayPort,
    AirPlay,
    /// 虚拟设备（含我们自己的 HAL 驱动）。
    Virtual,
    /// 聚合 / 多输出设备。**它报的不是它成员的延迟**，见 `underreports()`。
    Aggregate,
    /// 接力（Continuity Capture）的**有线**形态，如数据线连着的 iPhone 麦克风。
    ContinuityWired,
    /// 接力的**无线**形态。
    ContinuityWireless,
    /// 读到了一个我们没列举的取值。原始码见 `DevLatencyParts::transport_code`。
    Other,
    /// 没读到传输方式属性。
    Unknown,
}

impl Transport {
    /// 这类链路**已知少报**固有延迟吗？
    ///
    /// 名单不是照着「感觉不靠谱」列的，四条各有依据：
    ///
    /// - **Bluetooth**：规格 §3.4 点名。真实 150–250 ms，API 常报 20–30 ms。
    /// - **HDMI / DisplayPort**：显示器侧的图像同步缓冲根本不向主机声明。本机
    ///   实测 Odyssey G8（HDMI）报 920 帧 = 19.2 ms，而这块屏的音频实际明显更迟。
    /// - **AirPlay**：规格 §3.4 点名，网络链路的缓冲不在设备属性里。
    /// - **ContinuityWireless**：**这一条是预防性的，不是实测的**。同族的有线形态
    ///   `ContinuityWired` 本机实测**如实申报** 6000 帧 = 125 ms（见
    ///   `a_continuity_device_declares_its_latency_honestly`），所以无线形态大概率
    ///   也诚实；但无线段没人量过，而这里的代价是不对称的——多标一个「≥」只是少
    ///   一位精度，漏标一个则给出一个自信的错数字。谁量到无线形态同样诚实，就把
    ///   这一行删掉。
    /// - **Aggregate**：机制与前四条不同，结论一样。聚合设备**只报自己的壳，不报
    ///   成员的流**：本机实测「Multi-Output Device」（聚合了 MacBook Pro Speakers）
    ///   报 device=65 / safety=74 / **stream=0** / io=512 = 651 帧，而同一只物理
    ///   喇叭直接问是 device=65 / safety=74 / **stream=639** / io=512 = 1290 帧。
    ///   成员那 639 帧（14.5 ms）被壳吞掉了。见
    ///   `an_aggregate_device_swallows_its_members_stream_latency`。
    pub fn underreports(self) -> bool {
        matches!(
            self,
            Transport::Bluetooth
                | Transport::Hdmi
                | Transport::DisplayPort
                | Transport::AirPlay
                | Transport::ContinuityWireless
                | Transport::Aggregate
        )
    }
}

/// 要查哪一个设备。
///
/// `Uid` 单列是因为**模式 B 的虚拟设备名字是运行时生成的**（一对端一对设备），
/// 名字既不稳定也不唯一，`audiohubd` 本来就按 UID 开流（`audio.rs` 的
/// `start_on_uid`）。查延迟必须能问同一个问题，否则报出来的是另一张卡的数。
#[derive(Debug, Clone, Copy)]
pub enum DevTarget<'a> {
    /// 系统当前默认设备。
    Default,
    /// 按名字（与 `audio::list_output_devices` 报的名字同源）。
    Name(&'a str),
    /// 按 macOS 的 `kAudioDevicePropertyDeviceUID`。Windows 上没有对应物。
    Uid(&'a str),
}

/// 一次查询的**全部所得**，含读不到的那些。
///
/// 不是 `Result`：「这台设备的延迟读不到」是一个正常结论，不是错误。但**哪一项
/// 读不到**必须说得出来，否则排障时只剩一个 `Unavailable`，分不清是「这台机器
/// 不支持」还是「四项里有一项没答」。
///
/// 分配无所谓：本结构在控制面按需产生（每秒至多一次），**永远不在 10 ms 节拍上
/// 构造**——那条线上的约束见 `latency.rs` 文件头约束 3。
#[derive(Debug, Clone, PartialEq)]
pub struct DevLatencyParts {
    /// 读到的分量，`(名字, 帧数)`，按平台自己的分解方式。求和即总量。
    pub parts: Vec<(&'static str, u32)>,
    /// 本平台**声明存在、却没读到**的分量名。非空 ⇒ 总量 `Unavailable`。
    ///
    /// 与 `parts` 里一个值为 0 的分量是两件事：0 是「这一项真的是 0」
    /// （我们自己的虚拟设备就是），`missing` 是「这一项没答上来」。
    pub missing: Vec<&'static str>,
    /// 设备标称速率(Hz)。0 = 没读到 ⇒ 总量 `Unavailable`（帧数没有速率就换算不成
    /// 毫秒，这与 `StageDepth::ms()` 的 `rate == 0` 判据是同一条规则）。
    pub rate: u32,
    pub transport: Transport,
    /// 平台给出的**原始**传输/形态编码，0 = 没读到。
    /// macOS = `kAudioDevicePropertyTransportType` 的 fourcc；
    /// Windows = `PKEY_AudioEndpoint_FormFactor` 的枚举值。
    ///
    /// 有了 `transport` 为什么还要它：`Transport::Other` 只说得出「这是个我们
    /// 没列举的取值」，说不出是哪一个——而要判断「这一类该不该进
    /// `underreports()`」恰恰需要知道是哪一个。本机 iPhone 接力麦克风就落在这一
    /// 格（`'ccwd'`），没有原始码就只能靠猜。
    pub transport_code: u32,
    /// 实际落到的设备名，给报告用：按 UID 查时人眼没法核对自己问的是哪张卡。
    pub device: Option<String>,
    /// 这一组数**本身**的出处，尚未叠加传输方式的降级。
    /// macOS 直接读属性 ⇒ `Api`；Windows 走设备周期模型 ⇒ `Assumed`。
    pub base_source: LatSource,
    /// 查询失败的原因（设备不存在、属性全读不到……）。可安全打日志。
    pub error: Option<String>,
}

impl DevLatencyParts {
    /// 什么都没查到的形状：给不支持的平台与查询失败用。
    pub fn empty(error: impl Into<String>) -> DevLatencyParts {
        DevLatencyParts {
            parts: Vec::new(),
            missing: Vec::new(),
            rate: 0,
            transport: Transport::Unknown,
            transport_code: 0,
            device: None,
            base_source: LatSource::Unavailable,
            error: Some(error.into()),
        }
    }

    /// 各分量之和（帧）。**只在 `total()` 判定可用之后才有意义。**
    pub fn frames(&self) -> u32 {
        self.parts
            .iter()
            .fold(0u32, |acc, &(_, f)| acc.saturating_add(f))
    }

    /// 折成 `DevLatency`。四条判据，任何一条不满足就是 `Unavailable`：
    ///
    /// 1. `rate == 0` —— 帧数没有速率换算不成毫秒。
    /// 2. `missing` 非空 —— 少一个分量就少一段延迟，**求和是全有或全无**。
    ///    这是本模块最容易被「优化」掉的一行：把读到的三项加起来上报看着更有用，
    ///    实际上是把一个已知缺口伪装成一个完整读数，正是纪律 1 要杀死的形态。
    /// 3. `parts` 为空 —— 平台什么都没读到（stub 平台走这里）。
    /// 4. `base_source == Unavailable` —— 后端自陈这组数不可用。
    ///
    /// 通过之后再叠传输方式：链路已知少报 ⇒ 降为 `Unreliable`。
    pub fn total(&self) -> DevLatency {
        if self.rate == 0
            || !self.missing.is_empty()
            || self.parts.is_empty()
            || self.base_source == LatSource::Unavailable
        {
            return DevLatency::unavailable();
        }
        DevLatency {
            frames: self.frames(),
            rate: self.rate,
            source: worse(self.base_source, transport_source(self.transport)),
        }
    }
}

/// 传输方式单独给出的定性：正常链路不表态（`Api`），已知少报的降级。
fn transport_source(t: Transport) -> LatSource {
    if t.underreports() {
        LatSource::Unreliable
    } else {
        LatSource::Api
    }
}

/// 两个定性取**更不可信**的那个。序：`Api` < `Assumed` < `Unreliable` < `Unavailable`。
///
/// `Unreliable` 排在 `Assumed` 之后是因为两者错的方式不同：`Assumed` 是「按模型
/// 算的，可能偏几毫秒」，`Unreliable` 是「API 答了，而且已知差着一个数量级」。
/// 蓝牙上二者同时成立时，用户需要知道的是后者。
fn worse(a: LatSource, b: LatSource) -> LatSource {
    fn rank(s: LatSource) -> u8 {
        match s {
            LatSource::Api => 0,
            LatSource::Assumed => 1,
            LatSource::Unreliable => 2,
            LatSource::Unavailable => 3,
        }
    }
    if rank(a) >= rank(b) {
        a
    } else {
        b
    }
}

/// 查一个设备的固有延迟。**只读属性**，不开流、不改路由、不触发权限。
pub fn query(kind: DeviceKind, target: DevTarget<'_>) -> DevLatencyParts {
    imp::query(kind, target)
}

/// 级 9 `play_dev`：本机默认输出设备。
pub fn default_output() -> DevLatencyParts {
    query(DeviceKind::Output, DevTarget::Default)
}

/// 级 2 `cap_dev`：本机默认输入设备。
pub fn default_input() -> DevLatencyParts {
    query(DeviceKind::Input, DevTarget::Default)
}

// ---------------------------------------------------------------- macOS

#[cfg(target_os = "macos")]
mod imp {
    //! 全部走 `AudioObjectGetPropertyData`。没有一次 `AudioDeviceCreateIOProcID`、
    //! 没有一次 `AudioUnitInitialize`——那两个才是会动音频路径、会撞 TCC 的调用。
    //!
    //! FFI 声明是手写的，与 `volume.rs` / `audio.rs` 各自的那份并列而不共享：
    //! 这三处的属性集合互不相同，合并成一个「通用 CoreAudio 层」只会让每一处都
    //! 拖着另外两处用不到的 selector。

    use super::{DevLatencyParts, DevTarget, Transport};
    use crate::audio::DeviceKind;
    use crate::latency::LatSource;
    use std::ffi::c_void;
    use std::ptr::{null, null_mut};

    type OSStatus = i32;
    type AudioObjectID = u32;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct PropAddr {
        selector: u32,
        scope: u32,
        element: u32,
    }

    const SYSTEM_OBJECT: AudioObjectID = 1; // kAudioObjectSystemObject
    const ELEM_MAIN: u32 = 0; // kAudioObjectPropertyElementMain

    const fn fourcc(s: &[u8; 4]) -> u32 {
        u32::from_be_bytes(*s)
    }

    const SEL_DEFAULT_OUTPUT: u32 = fourcc(b"dOut"); // kAudioHardwarePropertyDefaultOutputDevice
    const SEL_DEFAULT_INPUT: u32 = fourcc(b"dIn "); // kAudioHardwarePropertyDefaultInputDevice
    const SEL_DEVICES: u32 = fourcc(b"dev#"); // kAudioHardwarePropertyDevices
    const SEL_NAME: u32 = fourcc(b"lnam"); // kAudioObjectPropertyName
    const SEL_UID: u32 = fourcc(b"uid "); // kAudioDevicePropertyDeviceUID
    const SEL_STREAMS: u32 = fourcc(b"stm#"); // kAudioDevicePropertyStreams
    /// 设备与流共用同一个 selector（`kAudioDevicePropertyLatency` ==
    /// `kAudioStreamPropertyLatency` == 'ltnc'），区别只在问的是哪个 AudioObject。
    const SEL_LATENCY: u32 = fourcc(b"ltnc");
    const SEL_SAFETY_OFFSET: u32 = fourcc(b"saft"); // kAudioDevicePropertySafetyOffset
    const SEL_BUFFER_FRAME_SIZE: u32 = fourcc(b"fsiz"); // kAudioDevicePropertyBufferFrameSize
    const SEL_NOMINAL_RATE: u32 = fourcc(b"nsrt"); // kAudioDevicePropertyNominalSampleRate
    const SEL_TRANSPORT: u32 = fourcc(b"tran"); // kAudioDevicePropertyTransportType
    const SEL_STREAM_CONFIG: u32 = fourcc(b"slay"); // kAudioDevicePropertyStreamConfiguration

    const SCOPE_GLOBAL: u32 = fourcc(b"glob");
    const SCOPE_INPUT: u32 = fourcc(b"inpt");
    const SCOPE_OUTPUT: u32 = fourcc(b"outp");

    // AudioHardwareBase.h 的 kAudioDeviceTransportType* 取值
    const TT_BUILTIN: u32 = fourcc(b"bltn");
    const TT_AGGREGATE: u32 = fourcc(b"grup");
    const TT_VIRTUAL: u32 = fourcc(b"virt");
    const TT_PCI: u32 = fourcc(b"pci ");
    const TT_USB: u32 = fourcc(b"usb ");
    const TT_FIREWIRE: u32 = fourcc(b"1394");
    const TT_BLUETOOTH: u32 = fourcc(b"blue");
    const TT_BLUETOOTH_LE: u32 = fourcc(b"blea");
    const TT_HDMI: u32 = fourcc(b"hdmi");
    const TT_DISPLAYPORT: u32 = fourcc(b"dprt");
    const TT_AIRPLAY: u32 = fourcc(b"airp");
    const TT_THUNDERBOLT: u32 = fourcc(b"thun");
    // kAudioDeviceTransportTypeContinuityCapture* —— 本机 iPhone 麦克风走 'ccwd'。
    const TT_CONTINUITY_WIRED: u32 = fourcc(b"ccwd");
    const TT_CONTINUITY_WIRELESS: u32 = fourcc(b"ccwl");

    const CF_UTF8: u32 = 0x0800_0100;

    #[link(name = "CoreAudio", kind = "framework")]
    extern "C" {
        fn AudioObjectGetPropertyDataSize(
            id: AudioObjectID,
            addr: *const PropAddr,
            qual_size: u32,
            qual: *const c_void,
            out_size: *mut u32,
        ) -> OSStatus;
        fn AudioObjectGetPropertyData(
            id: AudioObjectID,
            addr: *const PropAddr,
            qual_size: u32,
            qual: *const c_void,
            io_size: *mut u32,
            out: *mut c_void,
        ) -> OSStatus;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFRelease(cf: *const c_void);
        fn CFStringGetLength(s: *const c_void) -> isize;
        fn CFStringGetMaximumSizeForEncoding(len: isize, encoding: u32) -> isize;
        fn CFStringGetCString(s: *const c_void, buf: *mut u8, size: isize, encoding: u32) -> u8;
    }

    fn at(selector: u32, scope: u32) -> PropAddr {
        PropAddr { selector, scope, element: ELEM_MAIN }
    }

    fn get_u32(obj: AudioObjectID, a: &PropAddr) -> Option<u32> {
        let mut v: u32 = 0;
        let mut sz: u32 = 4;
        let st = unsafe {
            AudioObjectGetPropertyData(obj, a, 0, null(), &mut sz, &mut v as *mut u32 as *mut c_void)
        };
        (st == 0 && sz == 4).then_some(v)
    }

    fn get_f64(obj: AudioObjectID, a: &PropAddr) -> Option<f64> {
        let mut v: f64 = 0.0;
        let mut sz: u32 = 8;
        let st = unsafe {
            AudioObjectGetPropertyData(obj, a, 0, null(), &mut sz, &mut v as *mut f64 as *mut c_void)
        };
        (st == 0 && sz == 8).then_some(v)
    }

    fn prop_size(obj: AudioObjectID, a: &PropAddr) -> Option<u32> {
        let mut sz: u32 = 0;
        let st = unsafe { AudioObjectGetPropertyDataSize(obj, a, 0, null(), &mut sz) };
        (st == 0).then_some(sz)
    }

    /// AudioObject 的「get 一个 CF 对象」交出的是 +1 引用，释放是我们的事。
    fn get_cf_string(obj: AudioObjectID, a: &PropAddr) -> Option<String> {
        let mut cf: *const c_void = null_mut();
        let mut sz = std::mem::size_of::<*const c_void>() as u32;
        let st = unsafe {
            AudioObjectGetPropertyData(
                obj,
                a,
                0,
                null(),
                &mut sz,
                &mut cf as *mut *const c_void as *mut c_void,
            )
        };
        if st != 0 || cf.is_null() {
            return None;
        }
        let out = unsafe { cf_to_string(cf) };
        unsafe { CFRelease(cf) };
        out.filter(|s| !s.is_empty())
    }

    unsafe fn cf_to_string(cf: *const c_void) -> Option<String> {
        let max = CFStringGetMaximumSizeForEncoding(CFStringGetLength(cf), CF_UTF8);
        if max <= 0 {
            return Some(String::new());
        }
        let mut buf = vec![0u8; max as usize + 1];
        if CFStringGetCString(cf, buf.as_mut_ptr(), buf.len() as isize, CF_UTF8) == 0 {
            return None;
        }
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        buf.truncate(end);
        String::from_utf8(buf).ok()
    }

    fn device_ids() -> Vec<AudioObjectID> {
        let a = at(SEL_DEVICES, SCOPE_GLOBAL);
        let Some(bytes) = prop_size(SYSTEM_OBJECT, &a) else {
            return Vec::new();
        };
        let n = bytes as usize / std::mem::size_of::<AudioObjectID>();
        let mut ids = vec![0u32; n];
        let mut io = bytes;
        let st = unsafe {
            AudioObjectGetPropertyData(
                SYSTEM_OBJECT,
                &a,
                0,
                null(),
                &mut io,
                ids.as_mut_ptr() as *mut c_void,
            )
        };
        if st != 0 {
            return Vec::new();
        }
        ids.truncate(io as usize / std::mem::size_of::<AudioObjectID>());
        ids
    }

    fn scope_of(kind: DeviceKind) -> u32 {
        match kind {
            DeviceKind::Input => SCOPE_INPUT,
            DeviceKind::Output => SCOPE_OUTPUT,
        }
    }

    fn word(kind: DeviceKind) -> &'static str {
        match kind {
            DeviceKind::Input => "input",
            DeviceKind::Output => "output",
        }
    }

    /// 该方向上有没有流。与 `audio.rs` 的 `scope_channels` 同一个判据，只是这里
    /// 只需要「有没有」而不需要通道数，所以看属性字节数就够。
    fn does_direction(dev: AudioObjectID, scope: u32) -> bool {
        prop_size(dev, &at(SEL_STREAM_CONFIG, scope)).is_some_and(|n| n > 0)
            && prop_size(dev, &at(SEL_STREAMS, scope)).is_some_and(|n| n > 0)
    }

    fn device_name(dev: AudioObjectID) -> Option<String> {
        get_cf_string(dev, &at(SEL_NAME, SCOPE_GLOBAL))
    }

    fn resolve(kind: DeviceKind, target: DevTarget<'_>) -> Result<AudioObjectID, String> {
        let scope = scope_of(kind);
        match target {
            DevTarget::Default => {
                let sel = match kind {
                    DeviceKind::Input => SEL_DEFAULT_INPUT,
                    DeviceKind::Output => SEL_DEFAULT_OUTPUT,
                };
                match get_u32(SYSTEM_OBJECT, &at(sel, SCOPE_GLOBAL)) {
                    Some(0) | None => Err(format!("no default {} device", word(kind))),
                    Some(id) => Ok(id),
                }
            }
            DevTarget::Uid(uid) => device_ids()
                .into_iter()
                .find(|&d| get_cf_string(d, &at(SEL_UID, SCOPE_GLOBAL)).as_deref() == Some(uid))
                .filter(|&d| does_direction(d, scope))
                .ok_or_else(|| format!("no {} device with UID {uid:?}", word(kind))),
            DevTarget::Name(name) => {
                // 与 `audio.rs` 的解析规则一致：同名两张卡是歧义，不猜。
                let hits: Vec<AudioObjectID> = device_ids()
                    .into_iter()
                    .filter(|&d| does_direction(d, scope))
                    .filter(|&d| device_name(d).as_deref() == Some(name))
                    .collect();
                match hits.len() {
                    1 => Ok(hits[0]),
                    0 => Err(format!("no {} device named {name:?}", word(kind))),
                    n => Err(format!("{n} {} devices match {name:?}", word(kind))),
                }
            }
        }
    }

    fn transport_of(dev: AudioObjectID) -> (Transport, u32) {
        let Some(v) = get_u32(dev, &at(SEL_TRANSPORT, SCOPE_GLOBAL)) else {
            return (Transport::Unknown, 0);
        };
        let kind = match v {
            TT_BUILTIN => Transport::BuiltIn,
            TT_USB => Transport::Usb,
            TT_THUNDERBOLT => Transport::Thunderbolt,
            TT_PCI => Transport::Pci,
            TT_FIREWIRE => Transport::FireWire,
            TT_BLUETOOTH | TT_BLUETOOTH_LE => Transport::Bluetooth,
            TT_HDMI => Transport::Hdmi,
            TT_DISPLAYPORT => Transport::DisplayPort,
            TT_AIRPLAY => Transport::AirPlay,
            TT_VIRTUAL => Transport::Virtual,
            TT_AGGREGATE => Transport::Aggregate,
            TT_CONTINUITY_WIRED => Transport::ContinuityWired,
            TT_CONTINUITY_WIRELESS => Transport::ContinuityWireless,
            _ => Transport::Other,
        };
        (kind, v)
    }

    /// 该方向所有流里最大的那个 `kAudioStreamPropertyLatency`。
    ///
    /// **取 max 而不是求和**：多条流是同一时刻并行承载不同声道组的，串起来相加
    /// 会凭空报出 N 倍延迟——与 `StageId::is_output_tail` 那里「并行尾级取 max」
    /// 是同一条道理。返回 `None` = 有流但一条也没答上来。
    fn stream_latency(dev: AudioObjectID, scope: u32) -> Option<u32> {
        let a = at(SEL_STREAMS, scope);
        let bytes = prop_size(dev, &a)?;
        let n = bytes as usize / std::mem::size_of::<AudioObjectID>();
        if n == 0 {
            return None;
        }
        let mut ids = vec![0u32; n];
        let mut io = bytes;
        let st = unsafe {
            AudioObjectGetPropertyData(
                dev,
                &a,
                0,
                null(),
                &mut io,
                ids.as_mut_ptr() as *mut c_void,
            )
        };
        if st != 0 {
            return None;
        }
        ids.truncate(io as usize / std::mem::size_of::<AudioObjectID>());
        // 流的属性在 global scope 上问。
        ids.iter()
            .filter_map(|&s| get_u32(s, &at(SEL_LATENCY, SCOPE_GLOBAL)))
            .max()
    }

    /// 读一个「本平台声明存在」的 u32 分量：读到就进 `parts`（**0 也进**，那是
    /// 真读数），读不到就进 `missing`。两条路径都不产生 0 填补。
    fn take(
        name: &'static str,
        value: Option<u32>,
        parts: &mut Vec<(&'static str, u32)>,
        missing: &mut Vec<&'static str>,
    ) {
        match value {
            Some(v) => parts.push((name, v)),
            None => missing.push(name),
        }
    }

    pub fn query(kind: DeviceKind, target: DevTarget<'_>) -> DevLatencyParts {
        let dev = match resolve(kind, target) {
            Ok(d) => d,
            Err(e) => return DevLatencyParts::empty(e),
        };
        let scope = scope_of(kind);
        let name = device_name(dev);

        if !does_direction(dev, scope) {
            return DevLatencyParts {
                device: name,
                ..DevLatencyParts::empty(format!(
                    "AudioObjectID {dev} has no {} streams",
                    word(kind)
                ))
            };
        }

        let mut parts = Vec::with_capacity(4);
        let mut missing = Vec::new();

        take("device", get_u32(dev, &at(SEL_LATENCY, scope)), &mut parts, &mut missing);
        take(
            "safety_offset",
            get_u32(dev, &at(SEL_SAFETY_OFFSET, scope)),
            &mut parts,
            &mut missing,
        );
        take("stream", stream_latency(dev, scope), &mut parts, &mut missing);
        // BufferFrameSize 名义上是 global scope，但个别驱动只在方向 scope 上答；
        // 两个都试过再判缺项，免得把「问错 scope」记成「设备不支持」。
        let io_buf = get_u32(dev, &at(SEL_BUFFER_FRAME_SIZE, SCOPE_GLOBAL))
            .or_else(|| get_u32(dev, &at(SEL_BUFFER_FRAME_SIZE, scope)));
        take("io_buffer", io_buf, &mut parts, &mut missing);

        // 速率同理：先 global（`kAudioDevicePropertyNominalSampleRate` 的正式
        // scope），再方向 scope。
        let rate = get_f64(dev, &at(SEL_NOMINAL_RATE, SCOPE_GLOBAL))
            .or_else(|| get_f64(dev, &at(SEL_NOMINAL_RATE, scope)))
            .filter(|r| r.is_finite() && *r > 0.0)
            .map(|r| r.round() as u32)
            .unwrap_or(0);

        let (transport, transport_code) = transport_of(dev);
        DevLatencyParts {
            parts,
            missing,
            rate,
            transport,
            transport_code,
            device: name,
            // 四项全是直接读到的 CoreAudio 属性，**没有一项来自 cpal 的估计**
            // （规格 §3.4：cpal 的 macOS playback 是硬编码双缓冲假设）。
            base_source: LatSource::Api,
            error: (rate == 0).then(|| "nominal sample rate unreadable".to_string()),
        }
    }
}

// ---------------------------------------------------------------- Windows

#[cfg(windows)]
mod imp {
    //! `Activate(IAudioClient)` + `GetDevicePeriod` + `GetMixFormat`。
    //! **一次 `Initialize` 都没有**——见文件头「Windows：共享模式下这个数的确切
    //! 语义」。`Activate` 只是造一个 COM 对象，不与音频引擎建流。
    //!
    //! vtable 布局是 mmdeviceapi.h / audioclient.h 的冻结 ABI；用不到的槽声明成
    //! `usize`，这样谁也没法从它们身上误调出去——与 `volume.rs` 同一套写法。

    // `LatSource` 不再直接用：可信度标签走 `super::WINDOWS_DEVICE_PERIOD_SOURCE`
    // 那个常量，好让 macOS 上的测试也能盯住它（见该常量的文档）。
    use super::{DevLatencyParts, DevTarget, Transport};
    use crate::audio::DeviceKind;
    use std::ffi::c_void;
    use std::ptr;

    type HRESULT = i32;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct GUID {
        d1: u32,
        d2: u16,
        d3: u16,
        d4: [u8; 8],
    }

    const CLSID_MM_DEVICE_ENUMERATOR: GUID = GUID {
        d1: 0xBCDE0395,
        d2: 0xE52F,
        d3: 0x467C,
        d4: [0x8E, 0x3D, 0xC4, 0x57, 0x92, 0x91, 0x69, 0x2E],
    };
    const IID_IMM_DEVICE_ENUMERATOR: GUID = GUID {
        d1: 0xA95664D2,
        d2: 0x9614,
        d3: 0x4F35,
        d4: [0xA7, 0x46, 0xDE, 0x8D, 0xB6, 0x36, 0x17, 0xE6],
    };
    const IID_IAUDIO_CLIENT: GUID = GUID {
        d1: 0x1CB9AD4C,
        d2: 0xDBFA,
        d3: 0x4C32,
        d4: [0xB1, 0x78, 0xC2, 0xF5, 0x68, 0xA7, 0x03, 0xB2],
    };

    /// {A45C254E-DF1C-4EFD-8020-67D146A850E0} 是 functiondiscoverykeys_devpkey.h
    /// 的 PKEY_Device_* 族；14 = FriendlyName，24 = EnumeratorName。
    const PKEY_DEVICE_FRIENDLY_NAME: PropertyKey = PropertyKey {
        fmtid: GUID {
            d1: 0xA45C254E,
            d2: 0xDF1C,
            d3: 0x4EFD,
            d4: [0x80, 0x20, 0x67, 0xD1, 0x46, 0xA8, 0x50, 0xE0],
        },
        pid: 14,
    };
    /// PKEY_Device_EnumeratorName —— 驱动栈的名字（"USB" / "BTHENUM" / …）。
    const PKEY_DEVICE_ENUMERATOR_NAME: PropertyKey = PropertyKey {
        fmtid: GUID {
            d1: 0xA45C254E,
            d2: 0xDF1C,
            d3: 0x4EFD,
            d4: [0x80, 0x20, 0x67, 0xD1, 0x46, 0xA8, 0x50, 0xE0],
        },
        pid: 24,
    };
    /// PKEY_AudioEndpoint_FormFactor（mmdeviceapi.h）。
    const PKEY_AUDIO_ENDPOINT_FORM_FACTOR: PropertyKey = PropertyKey {
        fmtid: GUID {
            d1: 0x1DA5D803,
            d2: 0xD492,
            d3: 0x4EDD,
            d4: [0x8C, 0x23, 0xE0, 0xC0, 0xFF, 0xEE, 0x7F, 0x0E],
        },
        pid: 0,
    };
    /// EndpointFormFactor::DigitalAudioDisplayDevice —— HDMI / DisplayPort。
    const FORM_FACTOR_DIGITAL_DISPLAY: u32 = 9;

    const CLSCTX_INPROC_SERVER: u32 = 0x1;
    const CLSCTX_ALL: u32 = 0x17;
    const COINIT_MULTITHREADED: u32 = 0x0;
    const E_RENDER: u32 = 0;
    const E_CAPTURE: u32 = 1;
    const E_CONSOLE: u32 = 0;
    const DEVICE_STATE_ACTIVE: u32 = 0x1;
    const STGM_READ: u32 = 0x0;
    const VT_LPWSTR: u16 = 31;
    const VT_UI4: u16 = 19;

    #[repr(C)]
    struct PropertyKey {
        fmtid: GUID,
        pid: u32,
    }

    #[repr(C)]
    struct PropVariant {
        vt: u16,
        r1: u16,
        r2: u16,
        r3: u16,
        val: [u64; 2],
    }

    impl PropVariant {
        fn empty() -> PropVariant {
            PropVariant { vt: 0, r1: 0, r2: 0, r3: 0, val: [0; 2] }
        }
    }

    impl Drop for PropVariant {
        fn drop(&mut self) {
            unsafe { PropVariantClear(self) };
        }
    }

    /// WAVEFORMATEX。只读 `samples_per_sec`，其余字段在这里只为对齐布局。
    #[repr(C)]
    struct WaveFormatEx {
        format_tag: u16,
        channels: u16,
        samples_per_sec: u32,
        avg_bytes_per_sec: u32,
        block_align: u16,
        bits_per_sample: u16,
        cb_size: u16,
    }

    #[link(name = "ole32")]
    extern "system" {
        fn CoInitializeEx(reserved: *mut c_void, flags: u32) -> HRESULT;
        fn CoUninitialize();
        fn CoCreateInstance(
            clsid: *const GUID,
            outer: *mut c_void,
            ctx: u32,
            iid: *const GUID,
            out: *mut *mut c_void,
        ) -> HRESULT;
        fn CoTaskMemFree(p: *mut c_void);
        fn PropVariantClear(pvar: *mut PropVariant) -> HRESULT;
    }

    #[repr(C)]
    struct IUnknownVtbl {
        query_interface:
            unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
        add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
        release: unsafe extern "system" fn(*mut c_void) -> u32,
    }

    #[repr(C)]
    struct IMMDeviceEnumeratorVtbl {
        base: IUnknownVtbl,
        enum_audio_endpoints:
            unsafe extern "system" fn(*mut c_void, u32, u32, *mut *mut c_void) -> HRESULT,
        get_default_audio_endpoint:
            unsafe extern "system" fn(*mut c_void, u32, u32, *mut *mut c_void) -> HRESULT,
        get_device: usize,
        register_endpoint_notification_callback: usize,
        unregister_endpoint_notification_callback: usize,
    }

    #[repr(C)]
    struct IMMDeviceCollectionVtbl {
        base: IUnknownVtbl,
        get_count: unsafe extern "system" fn(*mut c_void, *mut u32) -> HRESULT,
        item: unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> HRESULT,
    }

    #[repr(C)]
    struct IMMDeviceVtbl {
        base: IUnknownVtbl,
        activate: unsafe extern "system" fn(
            *mut c_void,
            *const GUID,
            u32,
            *mut c_void,
            *mut *mut c_void,
        ) -> HRESULT,
        open_property_store:
            unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> HRESULT,
        get_id: usize,
        get_state: usize,
    }

    #[repr(C)]
    struct IPropertyStoreVtbl {
        base: IUnknownVtbl,
        get_count: usize,
        get_at: usize,
        get_value: unsafe extern "system" fn(
            *mut c_void,
            *const PropertyKey,
            *mut PropVariant,
        ) -> HRESULT,
        set_value: usize,
        commit: usize,
    }

    /// audioclient.h 的 IAudioClient。`initialize` 与 `get_stream_latency` 声明成
    /// `usize` **是故意的**：纪律 2 禁止在只读查询里建流，而 `GetStreamLatency`
    /// 必须先 `Initialize` 才有效。把它们做成不可调用的槽，比写一句「别调这个」
    /// 的注释更难违反。
    #[repr(C)]
    struct IAudioClientVtbl {
        base: IUnknownVtbl,
        initialize: usize,
        get_buffer_size: usize,
        get_stream_latency: usize,
        get_current_padding: usize,
        is_format_supported: usize,
        get_mix_format: unsafe extern "system" fn(*mut c_void, *mut *mut WaveFormatEx) -> HRESULT,
        get_device_period: unsafe extern "system" fn(*mut c_void, *mut i64, *mut i64) -> HRESULT,
        start: usize,
        stop: usize,
        reset: usize,
        set_event_handle: usize,
        get_service: usize,
    }

    struct Apartment {
        owned: bool,
    }

    impl Apartment {
        fn enter() -> Apartment {
            let hr = unsafe { CoInitializeEx(ptr::null_mut(), COINIT_MULTITHREADED) };
            Apartment { owned: hr >= 0 }
        }
    }

    impl Drop for Apartment {
        fn drop(&mut self) {
            if self.owned {
                unsafe { CoUninitialize() };
            }
        }
    }

    struct ComPtr(*mut c_void);

    impl ComPtr {
        fn null() -> ComPtr {
            ComPtr(ptr::null_mut())
        }

        /// Safety: 调用方必须说对这个指针到底是哪个接口。
        unsafe fn vtbl<V>(&self) -> *const V {
            *(self.0 as *const *const V)
        }
    }

    impl Drop for ComPtr {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    let v = self.vtbl::<IUnknownVtbl>();
                    ((*v).release)(self.0);
                }
            }
        }
    }

    unsafe fn wide_string(p: *const u16) -> Option<String> {
        if p.is_null() {
            return None;
        }
        let mut n = 0usize;
        while *p.add(n) != 0 {
            n += 1;
        }
        String::from_utf16(std::slice::from_raw_parts(p, n)).ok()
    }

    /// 端点属性库。`STGM_READ` 只读打开，不是「打开设备」——不建流、不占用、
    /// 不触发隐私门，与读友好名走的是同一个调用。
    fn property_store(device: &ComPtr) -> Option<ComPtr> {
        let mut store = ComPtr::null();
        let hr = unsafe {
            let v = device.vtbl::<IMMDeviceVtbl>();
            ((*v).open_property_store)(device.0, STGM_READ, &mut store.0)
        };
        (hr >= 0).then_some(store)
    }

    fn prop(store: &ComPtr, key: &PropertyKey) -> Option<PropVariant> {
        let mut pv = PropVariant::empty();
        let hr = unsafe {
            let v = store.vtbl::<IPropertyStoreVtbl>();
            ((*v).get_value)(store.0, key, &mut pv)
        };
        (hr >= 0).then_some(pv)
    }

    fn prop_string(store: &ComPtr, key: &PropertyKey) -> Option<String> {
        let pv = prop(store, key)?;
        if pv.vt != VT_LPWSTR {
            return None;
        }
        unsafe { wide_string(pv.val[0] as *const u16) }
    }

    fn prop_u32(store: &ComPtr, key: &PropertyKey) -> Option<u32> {
        let pv = prop(store, key)?;
        (pv.vt == VT_UI4).then(|| pv.val[0] as u32)
    }

    fn friendly_name(device: &ComPtr) -> Option<String> {
        prop_string(&property_store(device)?, &PKEY_DEVICE_FRIENDLY_NAME)
    }

    /// WASAPI 上的传输方式推断。
    ///
    /// **为什么要费这个事**：没有它，Windows 侧的 `transport` 恒为 `Unknown`，
    /// 于是 `underreports()` 恒 false，于是**一副蓝牙耳机与一对模拟音箱在读数上
    /// 完全一样**——正是规格纪律 1 点名要消灭的那个失败形态，只不过换到了
    /// 「定性」而不是「数值」上发生。
    ///
    /// WASAPI 没有 macOS 那个直白的 `kAudioDevicePropertyTransportType`，只能从
    /// 端点属性库里两条属性拼：
    /// - `PKEY_AudioEndpoint_FormFactor`：`DigitalAudioDisplayDevice`(9) 就是
    ///   HDMI / DisplayPort 那一类。这条最硬，优先。
    /// - `PKEY_Device_EnumeratorName`：驱动栈的名字。`BTH*` = 蓝牙、`USB*` = USB、
    ///   `HDAUDIO` = 板载、`MMDEVAPI` = 软件端点。
    ///
    /// 返回 `(定性, 原始 FormFactor 值)`。读不到就是 `Unknown` —— **不猜**：
    /// 猜错成 `Usb` 会让一副蓝牙耳机拿到 `Assumed` 的体面标签。
    fn transport_of(device: &ComPtr) -> (Transport, u32) {
        let Some(store) = property_store(device) else {
            return (Transport::Unknown, 0);
        };
        let form = prop_u32(&store, &PKEY_AUDIO_ENDPOINT_FORM_FACTOR);
        if form == Some(FORM_FACTOR_DIGITAL_DISPLAY) {
            return (Transport::Hdmi, FORM_FACTOR_DIGITAL_DISPLAY);
        }
        let raw = form.unwrap_or(0);
        let Some(en) = prop_string(&store, &PKEY_DEVICE_ENUMERATOR_NAME) else {
            return (Transport::Unknown, raw);
        };
        let en = en.to_ascii_uppercase();
        let kind = if en.starts_with("BTH") {
            // BTHENUM / BTHHFENUM / BTHLE —— A2DP 与 HFP 都在这下面。
            Transport::Bluetooth
        } else if en.starts_with("USB") {
            Transport::Usb
        } else if en.starts_with("HDAUDIO") {
            Transport::BuiltIn
        } else if en.starts_with("MMDEVAPI") {
            // 软件端点：远程桌面音频、各家虚拟声卡都在这里。
            Transport::Virtual
        } else {
            Transport::Other
        };
        (kind, raw)
    }

    fn flow_of(kind: DeviceKind) -> u32 {
        match kind {
            DeviceKind::Input => E_CAPTURE,
            DeviceKind::Output => E_RENDER,
        }
    }

    fn word(kind: DeviceKind) -> &'static str {
        match kind {
            DeviceKind::Input => "capture",
            DeviceKind::Output => "render",
        }
    }

    fn resolve(
        enumerator: &ComPtr,
        kind: DeviceKind,
        target: DevTarget<'_>,
    ) -> Result<(ComPtr, Option<String>), String> {
        let flow = flow_of(kind);
        match target {
            DevTarget::Default => {
                let mut device = ComPtr::null();
                let hr = unsafe {
                    let v = enumerator.vtbl::<IMMDeviceEnumeratorVtbl>();
                    ((*v).get_default_audio_endpoint)(enumerator.0, flow, E_CONSOLE, &mut device.0)
                };
                if hr < 0 {
                    return Err(format!(
                        "GetDefaultAudioEndpoint({}) failed: HRESULT 0x{:08X}",
                        word(kind),
                        hr as u32
                    ));
                }
                let name = friendly_name(&device);
                Ok((device, name))
            }
            // WASAPI 端点没有 macOS 那种 UID 属性，`DeviceEntry.uid` 在这边本来
            // 就恒为 None（`audio.rs` 的注释说明了原因）。凭名字编一个出来是撒谎，
            // 所以这里直接说不支持。
            DevTarget::Uid(uid) => {
                Err(format!("addressing devices by UID {uid:?} is macOS-only"))
            }
            DevTarget::Name(want) => {
                let mut coll = ComPtr::null();
                let hr = unsafe {
                    let v = enumerator.vtbl::<IMMDeviceEnumeratorVtbl>();
                    ((*v).enum_audio_endpoints)(
                        enumerator.0,
                        flow,
                        DEVICE_STATE_ACTIVE,
                        &mut coll.0,
                    )
                };
                if hr < 0 {
                    return Err(format!("EnumAudioEndpoints failed: HRESULT 0x{:08X}", hr as u32));
                }
                let mut count: u32 = 0;
                let hr = unsafe {
                    let v = coll.vtbl::<IMMDeviceCollectionVtbl>();
                    ((*v).get_count)(coll.0, &mut count)
                };
                if hr < 0 {
                    return Err(format!("GetCount failed: HRESULT 0x{:08X}", hr as u32));
                }
                let mut hits: Vec<(ComPtr, String)> = Vec::new();
                for i in 0..count {
                    let mut dev = ComPtr::null();
                    let hr = unsafe {
                        let v = coll.vtbl::<IMMDeviceCollectionVtbl>();
                        ((*v).item)(coll.0, i, &mut dev.0)
                    };
                    if hr < 0 {
                        continue;
                    }
                    if let Some(n) = friendly_name(&dev) {
                        if n == want {
                            hits.push((dev, n));
                        }
                    }
                }
                match hits.len() {
                    1 => {
                        let (dev, name) = hits.remove(0);
                        Ok((dev, Some(name)))
                    }
                    0 => Err(format!("no {} device named {want:?}", word(kind))),
                    n => Err(format!("{n} {} devices match {want:?}", word(kind))),
                }
            }
        }
    }

    pub fn query(kind: DeviceKind, target: DevTarget<'_>) -> DevLatencyParts {
        let _apt = Apartment::enter();
        let mut enumerator = ComPtr::null();
        let hr = unsafe {
            CoCreateInstance(
                &CLSID_MM_DEVICE_ENUMERATOR,
                ptr::null_mut(),
                CLSCTX_INPROC_SERVER,
                &IID_IMM_DEVICE_ENUMERATOR,
                &mut enumerator.0,
            )
        };
        if hr < 0 {
            return DevLatencyParts::empty(format!(
                "CoCreateInstance(MMDeviceEnumerator) failed: HRESULT 0x{:08X}",
                hr as u32
            ));
        }
        let (device, name) = match resolve(&enumerator, kind, target) {
            Ok(v) => v,
            Err(e) => return DevLatencyParts::empty(e),
        };

        // Activate 只造 COM 对象，不与音频引擎建流；建流的是 Initialize，而它
        // 在本模块里根本不可调用（见 IAudioClientVtbl 的注释）。
        let mut client = ComPtr::null();
        let hr = unsafe {
            let v = device.vtbl::<IMMDeviceVtbl>();
            ((*v).activate)(
                device.0,
                &IID_IAUDIO_CLIENT,
                CLSCTX_ALL,
                ptr::null_mut(),
                &mut client.0,
            )
        };
        if hr < 0 {
            return DevLatencyParts {
                device: name,
                ..DevLatencyParts::empty(format!(
                    "IMMDevice::Activate(IAudioClient) failed: HRESULT 0x{:08X}",
                    hr as u32
                ))
            };
        }
        let (transport, transport_code) = transport_of(&device);
        let v = unsafe { client.vtbl::<IAudioClientVtbl>() };

        let mut fmt: *mut WaveFormatEx = ptr::null_mut();
        let rate = if unsafe { ((*v).get_mix_format)(client.0, &mut fmt) } >= 0 && !fmt.is_null() {
            let r = unsafe { (*fmt).samples_per_sec };
            unsafe { CoTaskMemFree(fmt as *mut c_void) };
            r
        } else {
            0
        };

        let mut default_100ns: i64 = 0;
        let mut min_100ns: i64 = 0;
        let period_ok = unsafe {
            ((*v).get_device_period)(client.0, &mut default_100ns, &mut min_100ns) >= 0
        };

        let mut parts = Vec::with_capacity(1);
        let mut missing = Vec::new();
        // 100ns → 帧。截断而非四舍五入：这个数本来就是下限，向上凑没有依据。
        match (period_ok && default_100ns > 0 && rate > 0)
            .then(|| (default_100ns as u128 * rate as u128 / 10_000_000u128) as u32)
        {
            Some(f) => parts.push(("device_period", f)),
            None => missing.push("device_period"),
        }

        DevLatencyParts {
            parts,
            missing,
            rate,
            transport,
            transport_code,
            device: name,
            // 读到的是引擎周期，不是这条路径的固有延迟：共享模式下引擎自己那一档
            // 周期、KS 传输、驱动队列、设备 FIFO 都不在这个数里。
            //
            // **`Unreliable` 不是 `Assumed`**：30-win 实测 `GetDevicePeriod` 报
            // 10.00 ms，而同一端点写到播实测 41.92 ms —— 低报 4.2 倍，不是
            // 「模型偏几毫秒」，是「API 答了且已知差着一个数量级」。
            // 见文件头「这个下限低报 4.2 倍」与 `docs/spec-playdev-measurement.md` §3。
            //
            // 后果（必须保住）：`LatSource::is_exact()` 为假 ⇒ UI 永远带「≥」，
            // 总和永远不许升级成 `LatConfidence::Full`。
            base_source: super::WINDOWS_DEVICE_PERIOD_SOURCE,
            error: (rate == 0).then(|| "GetMixFormat failed; no engine sample rate".to_string()),
        }
    }
}

// ---------------------------------------------------------------- 其它平台

#[cfg(not(any(target_os = "macos", windows)))]
mod imp {
    //! 本项目只支持 macOS 与 Windows（`plan.md` §1）。这里给一个**诚实的空实现**
    //! 而不是 `unimplemented!()`：延迟遥测的整条链路都建立在「读不到 ⇒ Unavailable
    //! ⇒ 总和 None ⇒ UI 带『≥』」之上，panic 只会让一个本该显示「未知」的格子
    //! 把 daemon 打死。

    use super::{DevLatencyParts, DevTarget};
    use crate::audio::DeviceKind;

    pub fn query(_kind: DeviceKind, _target: DevTarget<'_>) -> DevLatencyParts {
        DevLatencyParts::empty("device latency query is not implemented on this platform")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(list: &[(&'static str, u32)], rate: u32) -> DevLatencyParts {
        DevLatencyParts {
            parts: list.to_vec(),
            missing: Vec::new(),
            rate,
            transport: Transport::BuiltIn,
            transport_code: 0,
            device: Some("test".into()),
            base_source: LatSource::Api,
            error: None,
        }
    }

    /// 换算：帧数走**这台设备自己的**速率。与 `StageDepth::ms()` 的 44.1k/48k
    /// 断言是同一条纪律 —— 混用 48000 会引入 −8.8% 的系统性偏差。
    #[test]
    fn frames_convert_with_the_devices_own_rate() {
        let at48 = parts(&[("device", 0), ("safety_offset", 33), ("stream", 0), ("io_buffer", 512)], 48_000);
        assert!((at48.total().ms().unwrap() - 545.0 * 1000.0 / 48_000.0).abs() < 1e-9);
        let at44 = parts(&[("device", 0), ("safety_offset", 33), ("stream", 0), ("io_buffer", 512)], 44_100);
        assert!((at44.total().ms().unwrap() - 545.0 * 1000.0 / 44_100.0).abs() < 1e-9);
        // 同样 545 帧，44.1k 上比 48k 上多 8.8%
        let a = at48.total().ms().unwrap();
        let b = at44.total().ms().unwrap();
        assert!((b / a - 48_000.0 / 44_100.0).abs() < 1e-9);
    }

    /// **少一项 ⇒ 整条读数 Unavailable。** 这是本模块最容易被「优化」掉的一行：
    /// 把读到的三项加起来上报看着更有用，实际上是把一个已知缺口伪装成完整读数。
    #[test]
    fn a_missing_component_poisons_the_whole_reading() {
        let mut p = parts(&[("device", 100), ("safety_offset", 33), ("io_buffer", 512)], 48_000);
        p.missing.push("stream");
        assert_eq!(p.total().source, LatSource::Unavailable);
        assert_eq!(p.total().ms(), None, "绝不能变成读到的那三项之和");
        // 缺项补上之后才允许出数
        p.missing.clear();
        p.parts.push(("stream", 0));
        assert_eq!(p.total().source, LatSource::Api);
        assert_eq!(p.total().frames, 645);
    }

    /// 速率读不到与分项读不到同权：帧数没有速率换算不成毫秒。
    #[test]
    fn a_rateless_device_is_unavailable_not_zero() {
        let p = parts(&[("device", 512)], 0);
        assert_eq!(p.total().source, LatSource::Unavailable);
        assert_eq!(p.total().ms(), None);
    }

    /// 平台 stub / 查询失败：`parts` 空 ⇒ `Unavailable`，**不是 0 ms**。
    #[test]
    fn an_empty_query_is_unavailable_not_zero() {
        let e = DevLatencyParts::empty("no such device");
        assert!(e.missing.is_empty(), "什么都没查就谈不上『缺了哪一项』");
        assert_eq!(e.total().source, LatSource::Unavailable);
        assert_eq!(e.total().ms(), None);
        assert_eq!(e.frames(), 0, "帧数确实是 0 …");
        // …但 0 帧绝不能变成一个可用的 0 ms 读数
        assert!(e.total().ms().is_none(), "0 帧 + Unavailable 必须仍是 None");
    }

    /// **真读数 0 与「读不到」必须分得开。** 我们自己的 HAL 驱动就声明
    /// latency=0 / safetyOffset=0 / streamLatency=0（AudioHubDriver.c:1958-1962、
    /// :2206-2209）——那是正确答案（虚拟设备没有 DAC，样本不经任何模拟环节），
    /// 不是缺项。若把 0 当缺项，模式 B 的虚拟设备会永远报不出延迟；若把缺项当 0，
    /// 蓝牙耳机会看起来和模拟输出一样好。两个方向都得防。
    ///
    /// 夹具就是本机 2026-08-02 对默认输出「AudioHub – WIN-IR01HVEFU7G 扬声器」的
    /// 实测读数：三个真 0 + `io_buffer` 512 帧 @48k = 10.667 ms。**那 512 帧不是 0**
    /// ——它是应用写进 CoreAudio、驱动的 IOProc 还没搬走的那一段，与 `hal_spk`
    /// 数的那一段（已经在环里的）不重复。
    #[test]
    fn a_genuine_zero_is_not_a_missing_reading() {
        let virt = DevLatencyParts {
            parts: vec![("device", 0), ("safety_offset", 0), ("stream", 0), ("io_buffer", 512)],
            missing: Vec::new(),
            rate: 48_000,
            transport: Transport::Virtual,
            transport_code: u32::from_be_bytes(*b"virt"),
            device: Some("AudioHub – WIN-IR01HVEFU7G 扬声器".into()),
            base_source: LatSource::Api,
            error: None,
        };
        let t = virt.total();
        assert_eq!(t.source, LatSource::Api, "0 是驱动给出的真值，不是读不到");
        assert_eq!(t.frames, 512, "三个真 0 + 一个 512 帧的 IO 缓冲");
        assert!((t.ms().unwrap() - 10.6666).abs() < 1e-3);
    }

    /// **逐项注入**：把 macOS 那四个分量一次拿掉一个，每一次都必须把整条读数打成
    /// `Unavailable`。
    ///
    /// 这条是 `a_missing_component_poisons_the_whole_reading` 的加强版，防的是
    /// 「只对某一项判空」的实现——比如只检查 `device` 而放过 `safety_offset`。
    /// 本机实测 Odyssey G8 的 `safety_offset` 有 320 帧（6.7 ms），单独漏掉它就是
    /// 规格 §3.4 那 10–20 ms 里的一大半。
    #[test]
    fn losing_any_single_component_makes_the_whole_reading_unavailable() {
        const FULL: [(&str, u32); 4] =
            [("device", 88), ("safety_offset", 320), ("stream", 12), ("io_buffer", 512)];
        // 前提：四项齐全时是可用的真值
        let whole = parts(&FULL, 48_000);
        assert_eq!(whole.total().source, LatSource::Api);
        assert_eq!(whole.total().frames, 932);

        for drop_idx in 0..FULL.len() {
            let kept: Vec<(&'static str, u32)> = FULL
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != drop_idx)
                .map(|(_, &kv)| kv)
                .collect();
            let mut p = parts(&kept, 48_000);
            p.missing.push(FULL[drop_idx].0);
            let t = p.total();
            assert_eq!(
                t.source,
                LatSource::Unavailable,
                "少了 {} 却仍然出数",
                FULL[drop_idx].0
            );
            assert_eq!(t.ms(), None, "少了 {} 时不许退化成剩下三项之和", FULL[drop_idx].0);
        }
    }

    /// 蓝牙 / HDMI / AirPlay：API 有值但已知少报 ⇒ `Unreliable`，UI 永远带「≥」。
    ///
    /// **两个方向都断言**。只断言「该降的降了」会让「把所有传输方式都标成
    /// `Unreliable`」这种实现全绿，而那等于放弃了 P1 想要的那个真值。
    #[test]
    fn underreporting_transports_are_downgraded_not_trusted() {
        for t in [
            Transport::Bluetooth,
            Transport::Hdmi,
            Transport::DisplayPort,
            Transport::AirPlay,
            Transport::ContinuityWireless,
            Transport::Aggregate,
        ] {
            let mut p = parts(&[("device", 1_000), ("safety_offset", 33), ("stream", 0), ("io_buffer", 512)], 48_000);
            p.transport = t;
            assert_eq!(p.total().source, LatSource::Unreliable, "{t:?} 必须降级");
            // 降级不等于丢弃：数字还在，只是不可采信为真值
            assert!(p.total().ms().is_some(), "{t:?} 的读数仍应给出");
        }
        for t in [
            Transport::BuiltIn,
            Transport::Usb,
            Transport::Virtual,
            Transport::Thunderbolt,
            Transport::Pci,
            Transport::FireWire,
            Transport::ContinuityWired,
        ] {
            let mut p = parts(&[("device", 100), ("safety_offset", 33), ("stream", 0), ("io_buffer", 512)], 48_000);
            p.transport = t;
            assert_eq!(p.total().source, LatSource::Api, "{t:?} 不该被无端降级");
        }
    }

    /// 本机 2026-08-02 实测的 iPhone 接力麦克风：`'ccwd'`，device=6000 帧 =
    /// **125 ms**，如实申报。
    ///
    /// 这条断言在两个方向上都有用：
    /// - 它是 `underreports()` 里「`ContinuityWired` 不降级」的依据——一个愿意
    ///   说出自己 125 ms 的设备，不是那种需要被打「≥」的设备。
    /// - 它同时是「**大数不等于坏数**」的样板：125 ms 看着像故障，实际是正确读数。
    ///   若哪天有人加一条「超过 100 ms 就判 Unreliable」的启发式，这里会红。
    ///
    /// 夹具是**记录下来的**测量，不是现场重测：iPhone 插没插不该决定这条断言跑
    /// 不跑（那种测试在设备拔掉后会永远静默全绿）。
    #[test]
    fn a_continuity_device_declares_its_latency_honestly() {
        let iphone = DevLatencyParts {
            parts: vec![
                ("device", 6_000),
                ("safety_offset", 100),
                ("stream", 0),
                ("io_buffer", 512),
            ],
            missing: Vec::new(),
            rate: 48_000,
            transport: Transport::ContinuityWired,
            transport_code: u32::from_be_bytes(*b"ccwd"),
            device: Some("Score's iPhone Microphone".into()),
            base_source: LatSource::Api,
            error: None,
        };
        let t = iphone.total();
        assert_eq!(t.source, LatSource::Api, "如实申报的设备不该被打折扣");
        assert_eq!(t.frames, 6_612);
        assert!((t.ms().unwrap() - 137.75).abs() < 1e-6);
    }

    /// 本机 2026-08-02 实测：聚合设备**吞掉成员的流延迟**，这就是
    /// `Transport::Aggregate` 进 `underreports()` 的全部依据。
    ///
    /// 两组读数问的是同一只物理喇叭：一次经 Multi-Output Device 的壳，一次直接问
    /// MacBook Pro Speakers。壳把成员那 639 帧 `stream` 报成了 0。
    #[test]
    fn an_aggregate_device_swallows_its_members_stream_latency() {
        let common = |stream: u32, transport| DevLatencyParts {
            parts: vec![
                ("device", 65),
                ("safety_offset", 74),
                ("stream", stream),
                ("io_buffer", 512),
            ],
            missing: Vec::new(),
            rate: 44_100,
            transport,
            transport_code: 0,
            device: None,
            base_source: LatSource::Api,
            error: None,
        };
        let shell = common(0, Transport::Aggregate);
        let member = common(639, Transport::BuiltIn);
        assert_eq!(shell.total().frames, 651);
        assert_eq!(member.total().frames, 1_290);
        let hidden_ms = member.total().ms().unwrap() - shell.total().ms().unwrap();
        assert!(
            (hidden_ms - 639.0 * 1000.0 / 44_100.0).abs() < 1e-9,
            "壳比成员少报的正是那一段 stream 延迟, got {hidden_ms}"
        );
        assert!(hidden_ms > 10.0, "少报 {hidden_ms:.1} ms —— 与规格 §3.4 说的整个误差预算同量级");
        // 所以壳的读数必须带「≥」，成员的可以当真值
        assert_eq!(shell.total().source, LatSource::Unreliable);
        assert_eq!(member.total().source, LatSource::Api);
    }

    /// **Windows 的 `device_period` 永远不许被当成真值。**
    ///
    /// 30-win 实测（`docs/spec-playdev-measurement.md` §3）：
    /// `GetDevicePeriod` 报 480 帧 = 10.00 ms，而同一端点
    /// `written − IAudioClock::GetPosition()` = 2012 帧 = **41.92 ms**。
    /// 低报 4.2 倍——不是「模型偏几毫秒」（那是 `Assumed` 的语义），
    /// 而是「API 答了且已知差着一个数量级」（`Unreliable` 的语义）。
    ///
    /// 这条与传输方式**无关**：ADAM D3V 是 USB、Odyssey G8 是 HDMI，
    /// 两条毫无共同硬件的通路，`K` 只差 1.3 ms ⇒ 这 30 ms 是 Windows 共享引擎
    /// 加 KS 传输，不是链路特性。所以哪怕传输方式是最「干净」的 `BuiltIn`，
    /// 这个读数也必须带「≥」。
    ///
    /// 注入对照（**在 macOS 上就能红**，这是把标签抽成
    /// [`WINDOWS_DEVICE_PERIOD_SOURCE`] 的全部理由）：
    /// 把那个常量改回 `LatSource::Assumed` 或 `Api` ⇒ 本条红。
    #[test]
    fn a_windows_device_period_is_a_floor_not_a_truth() {
        assert_eq!(
            WINDOWS_DEVICE_PERIOD_SOURCE,
            LatSource::Unreliable,
            "实测低报 4.2 倍（10.00 vs 41.92 ms）⇒ 不是 Assumed 那个量级的偏差"
        );
        let mut win = parts(&[("device_period", 480)], 48_000);
        win.base_source = WINDOWS_DEVICE_PERIOD_SOURCE;
        for t in [Transport::BuiltIn, Transport::Usb, Transport::Hdmi, Transport::Bluetooth] {
            win.transport = t;
            let total = win.total();
            assert_eq!(total.ms(), Some(10.0), "{t:?}：读数本身照报，它是个真下限");
            assert!(
                !total.source.is_exact(),
                "{t:?}：10.00 ms 对着实测的 41.92 ms —— 绝不许清掉「≥」"
            );
        }
    }

    /// Windows 后端**自己**报的标签必须是 `Unreliable`。
    ///
    /// 上一条测的是「拿到 `Unreliable` 之后不会被洗白」，这一条测的是
    /// 「后端确实给了 `Unreliable`」。两条缺一条，改坏了都不会红。
    /// 在 macOS 上编译掉——`cargo check --target x86_64-pc-windows-gnu --tests` 覆盖它。
    #[cfg(windows)]
    #[test]
    fn the_windows_backend_disowns_its_own_number() {
        for p in [default_output(), default_input()] {
            if p.parts.is_empty() {
                continue; // 没有默认设备的机器：本条无从判定，交给冒烟测试
            }
            assert_eq!(
                p.base_source, WINDOWS_DEVICE_PERIOD_SOURCE,
                "后端绕过了那个常量，mac 上的测试就盯不住它了: {p:?}"
            );
            assert!(!p.total().source.is_exact());
        }
    }

    /// `Assumed` 不能被有线传输「洗白」成 `Api`，蓝牙也不能把 `Assumed` 洗成比
    /// `Unreliable` 更好看的东西：取更不可信的那个。
    #[test]
    fn the_less_trustworthy_label_wins() {
        assert_eq!(worse(LatSource::Api, LatSource::Assumed), LatSource::Assumed);
        assert_eq!(worse(LatSource::Assumed, LatSource::Api), LatSource::Assumed);
        assert_eq!(worse(LatSource::Assumed, LatSource::Unreliable), LatSource::Unreliable);
        assert_eq!(worse(LatSource::Unreliable, LatSource::Unavailable), LatSource::Unavailable);

        let mut win = parts(&[("device_period", 480)], 48_000);
        win.base_source = LatSource::Assumed;
        win.transport = Transport::BuiltIn;
        assert_eq!(win.total().source, LatSource::Assumed, "有线不代表这个模型变成了真值");
        win.transport = Transport::Bluetooth;
        assert_eq!(win.total().source, LatSource::Unreliable);
    }

    /// 后端自陈不可用时，哪怕分项看着齐全也不许出数。
    #[test]
    fn a_backend_that_disowns_its_numbers_is_believed() {
        let mut p = parts(&[("device", 512)], 48_000);
        p.base_source = LatSource::Unavailable;
        assert_eq!(p.total().ms(), None);
    }

    /// 求和溢出不许回绕成一个小数字（回绕出来的会是一个看着正常的读数）。
    #[test]
    fn the_sum_saturates_instead_of_wrapping() {
        let p = parts(&[("a", u32::MAX), ("b", 4_800)], 48_000);
        assert_eq!(p.frames(), u32::MAX);
    }

    /// **真机冒烟**：默认输出/输入的查询不得 panic、不得挂起，且返回的形状自洽。
    /// 不断言具体数值——那取决于插了什么设备——只断言不变量：
    /// 出了数就必须有速率，没出数就必须说得出原因。
    #[test]
    fn querying_the_real_default_devices_keeps_its_own_invariants() {
        for p in [default_output(), default_input()] {
            let t = p.total();
            match t.source {
                LatSource::Unavailable => {
                    assert_eq!(t.ms(), None);
                    assert!(
                        p.error.is_some() || !p.missing.is_empty() || p.parts.is_empty(),
                        "报不可用就得说得出是哪一种不可用: {p:?}"
                    );
                }
                _ => {
                    assert!(p.rate > 0, "出了数就必须有速率: {p:?}");
                    assert!(p.missing.is_empty(), "有缺项就不该出数: {p:?}");
                    let ms = t.ms().expect("source 非 Unavailable 时必有毫秒值");
                    assert!(ms.is_finite() && ms >= 0.0, "{ms} 不是一个合法的毫秒值");
                    assert!(ms < 1_000.0, "单台设备的固有延迟不该到秒级: {p:?}");
                }
            }
        }
    }
}
