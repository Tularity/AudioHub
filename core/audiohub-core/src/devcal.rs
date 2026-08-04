//! **一次开流标定**：Windows 输出端点「写进去 → 播出来」的真实帧数。
//!
//! 这个模块与 [`crate::devlat`] 是**故意分开的两个文件**，分界线就是那份纪律：
//! `devlat` 只做属性查询，一次 `Initialize` 都没有（它的 `IAudioClientVtbl`
//! 把 `initialize` / `start` / `get_service` 全部声明成 `usize`，让「顺手开个流
//! 看看」在那边**根本调不出来**）。本模块**要**开流，所以它必须住在另一栋房子里，
//! 并且自己带全套纪律。把两者合并会让 `devlat` 那道编译期封锁一夜之间失效。
//!
//! ## 为什么非开流不可（三条已实测证伪的替代路，别再试）
//!
//! 30-win 实测（`docs/spec-playdev-measurement.md` §1/§3，2026-08-04）：
//!
//! | 免开流的路 | 实测结果 | 判决 |
//! |---|---|---|
//! | `IAudioClient::GetStreamLatency` | 两个端点上**恒返回 0 hns**。`Initialize` 前后、事件驱动与否、四种 client properties 组合全是 0 | 在盒内 USB/HDMI 类驱动上是废的 |
//! | 端点属性存储 | 73 条逐条 dump，**没有任何一条是延迟** | 无此属性 |
//! | `IAudioClient::GetDevicePeriod` | 报 480 帧 = 10.00 ms，真值 41.92 ms | **低报 4.2 倍**，只能当下限 |
//!
//! 剩下的唯一一条是 `written − IAudioClock::GetPosition()`：
//! 我们交给渲染客户端的总帧数，减去时钟说「已经播出去」的帧数，差值就是
//! **此刻还压在 WASAPI 以下的那一段**。
//!
//! ## 这个数不是记账伪影 —— 已被「改缓冲大小」验证死
//!
//! 模型 `写到播 = padding + K`（`K` = 端点缓冲**以下**的固定管线）。
//! 把端点缓冲撑到 4.5 倍，`padding` 同比涨，`K` 应当纹丝不动：
//!
//! | `GetBufferSize` | `padding@event` | `written − position` | `K` |
//! |---|---|---|---|
//! | 1056 f (22 ms) | 576 f (12.00 ms) | 2012 f (41.92 ms) | **1436 f = 29.92 ms** |
//! | 2400 f (50 ms) | 1920 f (40.00 ms) | 3355 f (69.90 ms) | **1435 f = 29.90 ms** |
//! | 4800 f (100 ms) | 4320 f (90.00 ms) | 5755 f (119.90 ms) | **1435 f = 29.90 ms** |
//!
//! 换一条毫无共同硬件的通路（NVIDIA HDMI 端点）`K = 1498 f = 31.2 ms`，只差 1.3 ms
//! ⇒ 这 30 ms **不是设备特性，是 Windows 共享引擎 + KS 传输**。
//!
//! ## 四条不可让步的纪律
//!
//! 1. **共享模式，全静音，绝不改默认设备。** 标定流用
//!    `AUDCLNT_SHAREMODE_SHARED` + `AUDCLNT_BUFFERFLAGS_SILENT`：它与用户正在
//!    听的那条流并存，不抢占、不出声、不改路由。**独占模式在这里是禁术**——
//!    它会把用户正在放的音乐直接踢掉。
//! 2. **只做一次。** 结果由调用方缓存（`audiohubd::devlats`），键是端点名 + 速率。
//!    每次开流都标定 = 每次开流都多开一条流，纯代价。
//! 3. **复刻生产流的填充策略，否则这个数没有可比性。** 见下一节。
//! 4. **失败就是失败，返回 `Err`，绝不退回一个编出来的数。** 调用方据此保留
//!    `devlat` 那个 `Unreliable` 的下限——「有一个偏低 4.2 倍的下限」比
//!    「有一个来历不明的精确值」诚实。
//!
//! ## 纪律 3 展开：为什么标定流必须和 cpal 一样「把端点缓冲填满」
//!
//! `written − position` 拆成 `padding + K`，其中 `padding` **不是设备属性，
//! 是写法造成的**。cpal 的 WASAPI 后端每次事件都把缓冲顶满：
//!
//! ```text
//! frames_available = max_frames_in_buffer − padding      // cpal-0.16.0/src/host/wasapi/stream.rs:256-264
//! ```
//!
//! 于是稳态下事件时刻 `padding ≡ bufferSize − enginePeriod`（实测 1056 − 480 = 576）。
//! 若标定流改成「每次只写一个周期」，稳态 `padding` 会掉到 480，
//! 标定值就比生产流真实经历的少 2 ms —— 一个**看不出来的**系统性偏差。
//!
//! 所以本模块：事件驱动、`hnsBufferDuration = 0`（与 `BufferSize::Default` 同一条路）、
//! 每次事件 `GetBuffer(bufferSize − padding)`。三项与 cpal 逐条对齐。
//!
//! ## 这个数**不精确**，标签必须是 `Assumed` 而不是 `Api`
//!
//! 两处已知不确定度，加起来足以否掉 `is_exact()`：
//!
//! - **开流竞态**：`docs/spec-playdev-measurement.md` §3.4 留痕，一次
//!   `preroll=96` 的试验给出 2012 → 1627（−384 f = −8 ms）——预填不足时管线的
//!   填充点落到了另一个格点上。稳态三次复跑都是 2012，但**每次开流落在哪个格点
//!   可能差 384 帧**。
//! - **迁移假设**：标定流与生产流是两条不同的流。稳态相同是有依据的推断
//!   （同端点、同引擎周期、同填充策略），不是实测的同一条流。
//!
//! `Assumed` 的语义正是「按模型算的，可能偏几毫秒」。这里偏的是 8 ms，
//! 而它替换掉的 `GetDevicePeriod` 偏 31.9 ms —— 换来的是 4 倍的准确度，
//! **但换不来「≥」的消失**。

#[cfg(windows)]
use crate::devlat::{DevLatencyParts, DevTarget};
#[cfg(not(windows))]
use crate::devlat::DevTarget;
use crate::latency::LatSource;

/// 标定值在 [`DevLatencyParts::base_source`] 上的可信度标签。
///
/// **与 [`crate::devlat::WINDOWS_DEVICE_PERIOD_SOURCE`] 同一条理由抽成常量**：
/// 判定它的依据是一次 30-win 实测，而改坏它的人多半坐在 macOS 前。写死在
/// `#[cfg(windows)]` 里，把它改成 `Api` 在 mac 上一条测试都不会红。
///
/// 为什么是 `Assumed` 而不是 `Api`：见文件头「这个数**不精确**」。一句话——
/// 开流竞态实测可以差 384 帧（8 ms），而 `Api` 的语义是「平台 API 给出的真值」。
pub const WINDOWS_CALIBRATED_SOURCE: LatSource = LatSource::Assumed;

/// `DevLatencyParts.parts` 里标定值那一项的名字。
///
/// **故意不叫 `device_period`**：那是它替换掉的那个量的名字，两者差 4.2 倍。
/// 排障时看到 `write_to_play` 就知道这一格是标定来的，看到 `device_period`
/// 就知道标定没成功、当前是那个偏低的下限。名字本身就是出处。
pub const CAL_PART: &str = "write_to_play";

/// 一次标定的**全部所得**。
///
/// 不只有那一个帧数：`buffer_frames` / `padding_frames` / `k_frames` 是
/// 「这个数不是记账伪影」那条论证的原料（文件头的表），排障时缺一项就重现不了。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputCalibration {
    /// `written − position` 的**中位数**（帧）。这就是 `play_dev`。
    ///
    /// 取中位数不取均值：开流阶段的头几拍必然偏小（管线还没填满），
    /// 均值会被它们拉下来，而中位数在稳态样本占多数时不受影响。
    pub frames: u32,
    /// 端点引擎速率（Hz）。**标定值只对这个速率有效**——换了速率，帧数的含义就变了。
    pub rate: u32,
    /// 标定落到的端点友好名。缓存键的一半（另一半是 `rate`）。
    pub device: Option<String>,
    /// `GetBufferSize`（帧）。实测 1056。
    pub buffer_frames: u32,
    /// 稳态事件时刻的 `GetCurrentPadding` 中位数（帧）。实测 576 = 1056 − 480。
    pub padding_frames: u32,
    /// 引擎周期（帧），来自 `GetDevicePeriod` 的 default。实测 480。
    pub period_frames: u32,
    /// 取中位数用的稳态样本数。太少就不该信——调用方可以据此拒收。
    pub samples: u32,
    /// 样本的全幅（max − min，帧）。**开流竞态的可见形态**：文件头说的那 384 帧
    /// 台阶若发生在标定期间，这里会很大。
    pub spread_frames: u32,
}

impl OutputCalibration {
    /// `K` = 端点缓冲**以下**的固定管线（帧）。`None` = 分解不出来（padding 比总量还大）。
    ///
    /// 只用于报告与排障：它是「这 30 ms 换台声卡也一样」那条结论的量。
    /// **不参与上报**——上报的是 `frames`（含 padding），因为生产流真实经历的
    /// 就是那一整段。
    pub fn k_frames(&self) -> Option<u32> {
        self.frames.checked_sub(self.padding_frames)
    }

    pub fn ms(&self) -> Option<f64> {
        (self.rate > 0).then(|| self.frames as f64 * 1000.0 / self.rate as f64)
    }

    /// 这份标定还适用于**现在**这台默认输出吗？
    ///
    /// 端点名 + 速率两项全等才算数。少比一项的后果是具体的：用户把默认输出从
    /// USB 音箱切到 HDMI 显示器，两者 `K` 差 1.3 ms 而 `padding` 可能完全不同，
    /// 拿旧标定往新端点上贴就是一个**没有任何迹象**的错数。
    ///
    /// `device == None`（连友好名都没读到）一律判不匹配：无法核对的缓存不许命中。
    pub fn matches(&self, device: Option<&str>, rate: u32) -> bool {
        match (&self.device, device) {
            (Some(a), Some(b)) => a == b && self.rate == rate && rate > 0,
            _ => false,
        }
    }
}

/// 标定一次默认输出端点。**开一条共享模式静音流**，见文件头的四条纪律。
///
/// 非 Windows 平台返回 `Err`：macOS 的 `kAudioDevicePropertyLatency` /
/// `SafetyOffset` / `kAudioStreamPropertyLatency` **免开流可读**
/// （`devlat::imp` 已经在读），根本不需要标定。给 mac 也开一条流是纯风险。
pub fn calibrate_output(target: DevTarget<'_>) -> Result<OutputCalibration, String> {
    imp::calibrate_output(target)
}

/// 把标定值折进一份 `devlat::query` 的读数里，**替换**掉那个偏低 4.2 倍的
/// `device_period`。
///
/// 三条判据，任一不满足就原样返回（**不合并、不取平均、不「取较大者」**）：
///
/// 1. 标定的端点名 + 速率与这份读数对不上 ⇒ 那是另一台设备的标定。
/// 2. 这份读数本身就没读到速率 ⇒ 帧数换算不成毫秒，标定值同样无处安放。
/// 3. 这份读数已经是 `Unavailable`（设备都没解析到）⇒ 没有可替换的对象。
///
/// 返回 `true` = 真的替换了。调用方据此决定日志说哪一句。
#[cfg(windows)]
pub fn apply_output_calibration(parts: &mut DevLatencyParts, cal: &OutputCalibration) -> bool {
    if !cal.matches(parts.device.as_deref(), parts.rate) {
        return false;
    }
    if parts.base_source == LatSource::Unavailable || parts.rate == 0 {
        return false;
    }
    // **整组替换而不是追加**：`device_period` 与 `write_to_play` 量的是同一段
    // （从我们交出样本到它播出来），追加就是把这一段算两遍。
    parts.parts = vec![(CAL_PART, cal.frames)];
    parts.missing.clear();
    parts.base_source = WINDOWS_CALIBRATED_SOURCE;
    true
}

// ---------------------------------------------------------------- Windows

#[cfg(windows)]
mod imp {
    //! 全套 WASAPI 手写 FFI。vtable 是 audioclient.h / mmdeviceapi.h 的冻结 ABI。
    //!
    //! 与 `devlat::imp` 的 `IAudioClientVtbl` **不共用**：那一份把 `initialize`
    //! 等槽声明成 `usize` 是它的核心纪律，本模块要调它们，所以自己声明一份完整的。
    //! 两份并存不是重复，是两条纪律各自的编译期形态。

    use super::OutputCalibration;
    use crate::devlat::DevTarget;
    use std::ffi::c_void;
    use std::ptr;

    type HRESULT = i32;
    type HANDLE = *mut c_void;

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
    /// IID_IAudioRenderClient {F294ACFC-3146-4483-A7BF-ADDCA7C260E2}
    const IID_IAUDIO_RENDER_CLIENT: GUID = GUID {
        d1: 0xF294ACFC,
        d2: 0x3146,
        d3: 0x4483,
        d4: [0xA7, 0xBF, 0xAD, 0xDC, 0xA7, 0xC2, 0x60, 0xE2],
    };
    /// IID_IAudioClock {CD63314F-3FBA-4A1B-812C-EF96358728E7}
    const IID_IAUDIO_CLOCK: GUID = GUID {
        d1: 0xCD63314F,
        d2: 0x3FBA,
        d3: 0x4A1B,
        d4: [0x81, 0x2C, 0xEF, 0x96, 0x35, 0x87, 0x28, 0xE7],
    };
    const PKEY_DEVICE_FRIENDLY_NAME: PropertyKey = PropertyKey {
        fmtid: GUID {
            d1: 0xA45C254E,
            d2: 0xDF1C,
            d3: 0x4EFD,
            d4: [0x80, 0x20, 0x67, 0xD1, 0x46, 0xA8, 0x50, 0xE0],
        },
        pid: 14,
    };

    const CLSCTX_INPROC_SERVER: u32 = 0x1;
    const CLSCTX_ALL: u32 = 0x17;
    const COINIT_MULTITHREADED: u32 = 0x0;
    const E_RENDER: u32 = 0;
    const E_CONSOLE: u32 = 0;
    const DEVICE_STATE_ACTIVE: u32 = 0x1;
    const STGM_READ: u32 = 0x0;
    const VT_LPWSTR: u16 = 31;

    const AUDCLNT_SHAREMODE_SHARED: u32 = 0;
    /// `AUDCLNT_STREAMFLAGS_EVENTCALLBACK` —— 事件驱动，与 cpal 同一条路。
    const AUDCLNT_STREAMFLAGS_EVENTCALLBACK: u32 = 0x0004_0000;
    /// `AUDCLNT_BUFFERFLAGS_SILENT` —— 纪律 1：这条流一个声音都不出。
    const AUDCLNT_BUFFERFLAGS_SILENT: u32 = 0x2;
    const WAIT_OBJECT_0: u32 = 0;

    /// 一次事件等待的上限。引擎周期 10 ms，200 ms 还没来就是这条流废了。
    const WAIT_MS: u32 = 200;
    /// 总共走多少个事件。10 ms 一个 ⇒ 约 0.6 s。
    ///
    /// 为什么不更长：这个函数跑在一条一次性线程上，而 daemon 关停时要等它。
    /// 为什么不更短：前 `WARMUP_EVENTS` 拍必须丢掉（管线还没填满），
    /// 留给稳态的样本要够算中位数。
    const TOTAL_EVENTS: u32 = 60;
    /// 丢掉的开头拍数。实测管线在 10 拍内进入稳态（`padding` 四个分位数同一个值）。
    const WARMUP_EVENTS: u32 = 20;

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

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateEventW(
            attrs: *mut c_void,
            manual_reset: i32,
            initial_state: i32,
            name: *const u16,
        ) -> HANDLE;
        fn WaitForSingleObject(h: HANDLE, ms: u32) -> u32;
        fn CloseHandle(h: HANDLE) -> i32;
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

    /// **完整**的 IAudioClient —— 与 `devlat::imp` 那份故意残废的不是一回事。
    /// 这里每个槽都要调，所以每个槽都得是真的函数指针。
    #[repr(C)]
    struct IAudioClientVtbl {
        base: IUnknownVtbl,
        initialize: unsafe extern "system" fn(
            *mut c_void,
            u32,
            u32,
            i64,
            i64,
            *const WaveFormatEx,
            *const GUID,
        ) -> HRESULT,
        get_buffer_size: unsafe extern "system" fn(*mut c_void, *mut u32) -> HRESULT,
        get_stream_latency: unsafe extern "system" fn(*mut c_void, *mut i64) -> HRESULT,
        get_current_padding: unsafe extern "system" fn(*mut c_void, *mut u32) -> HRESULT,
        is_format_supported: usize,
        get_mix_format: unsafe extern "system" fn(*mut c_void, *mut *mut WaveFormatEx) -> HRESULT,
        get_device_period: unsafe extern "system" fn(*mut c_void, *mut i64, *mut i64) -> HRESULT,
        start: unsafe extern "system" fn(*mut c_void) -> HRESULT,
        stop: unsafe extern "system" fn(*mut c_void) -> HRESULT,
        reset: usize,
        set_event_handle: unsafe extern "system" fn(*mut c_void, HANDLE) -> HRESULT,
        get_service:
            unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    }

    #[repr(C)]
    struct IAudioRenderClientVtbl {
        base: IUnknownVtbl,
        get_buffer: unsafe extern "system" fn(*mut c_void, u32, *mut *mut u8) -> HRESULT,
        release_buffer: unsafe extern "system" fn(*mut c_void, u32, u32) -> HRESULT,
    }

    #[repr(C)]
    struct IAudioClockVtbl {
        base: IUnknownVtbl,
        get_frequency: unsafe extern "system" fn(*mut c_void, *mut u64) -> HRESULT,
        get_position: unsafe extern "system" fn(*mut c_void, *mut u64, *mut u64) -> HRESULT,
        get_characteristics: usize,
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

    /// 事件句柄的 RAII。**不是可有可无的讲究**：`Initialize` 之后每条早退路径
    /// 都要放它，手写 `CloseHandle` 漏一条就是一个每次标定失败泄漏一个内核对象
    /// 的 daemon。
    struct Event(HANDLE);

    impl Event {
        fn create() -> Option<Event> {
            // auto-reset、初始未置位：WASAPI 每消耗完一个周期置位一次。
            let h = unsafe { CreateEventW(ptr::null_mut(), 0, 0, ptr::null()) };
            (!h.is_null()).then_some(Event(h))
        }
    }

    impl Drop for Event {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { CloseHandle(self.0) };
            }
        }
    }

    /// `Stop()` 的 RAII。同上：`Start` 之后有五条早退路径。
    struct Started<'a>(&'a ComPtr);

    impl Drop for Started<'_> {
        fn drop(&mut self) {
            unsafe {
                let v = self.0.vtbl::<IAudioClientVtbl>();
                ((*v).stop)(self.0 .0);
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

    fn friendly_name(device: &ComPtr) -> Option<String> {
        let mut store = ComPtr::null();
        let hr = unsafe {
            let v = device.vtbl::<IMMDeviceVtbl>();
            ((*v).open_property_store)(device.0, STGM_READ, &mut store.0)
        };
        if hr < 0 {
            return None;
        }
        let mut pv = PropVariant::empty();
        let hr = unsafe {
            let v = store.vtbl::<IPropertyStoreVtbl>();
            ((*v).get_value)(store.0, &PKEY_DEVICE_FRIENDLY_NAME, &mut pv)
        };
        if hr < 0 || pv.vt != VT_LPWSTR {
            return None;
        }
        unsafe { wide_string(pv.val[0] as *const u16) }
    }

    fn resolve(enumerator: &ComPtr, target: DevTarget<'_>) -> Result<ComPtr, String> {
        match target {
            DevTarget::Default => {
                let mut device = ComPtr::null();
                let hr = unsafe {
                    let v = enumerator.vtbl::<IMMDeviceEnumeratorVtbl>();
                    ((*v).get_default_audio_endpoint)(
                        enumerator.0,
                        E_RENDER,
                        E_CONSOLE,
                        &mut device.0,
                    )
                };
                if hr < 0 {
                    return Err(format!(
                        "GetDefaultAudioEndpoint(render) failed: HRESULT 0x{:08X}",
                        hr as u32
                    ));
                }
                Ok(device)
            }
            DevTarget::Uid(uid) => Err(format!("addressing endpoints by UID {uid:?} is macOS-only")),
            DevTarget::Name(want) => {
                let mut coll = ComPtr::null();
                let hr = unsafe {
                    let v = enumerator.vtbl::<IMMDeviceEnumeratorVtbl>();
                    ((*v).enum_audio_endpoints)(
                        enumerator.0,
                        E_RENDER,
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
                let mut hit: Option<ComPtr> = None;
                let mut hits = 0usize;
                for i in 0..count {
                    let mut dev = ComPtr::null();
                    let hr = unsafe {
                        let v = coll.vtbl::<IMMDeviceCollectionVtbl>();
                        ((*v).item)(coll.0, i, &mut dev.0)
                    };
                    if hr < 0 {
                        continue;
                    }
                    if friendly_name(&dev).as_deref() == Some(want) {
                        hits += 1;
                        hit = Some(dev);
                    }
                }
                match hits {
                    1 => Ok(hit.expect("hits==1 时必有")),
                    0 => Err(format!("no render device named {want:?}")),
                    n => Err(format!("{n} render devices match {want:?}")),
                }
            }
        }
    }

    fn median(v: &mut [u32]) -> u32 {
        v.sort_unstable();
        v[v.len() / 2]
    }

    pub fn calibrate_output(target: DevTarget<'_>) -> Result<OutputCalibration, String> {
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
            return Err(format!(
                "CoCreateInstance(MMDeviceEnumerator) failed: HRESULT 0x{:08X}",
                hr as u32
            ));
        }
        let device = resolve(&enumerator, target)?;
        let name = friendly_name(&device);

        let mut client = ComPtr::null();
        let hr = unsafe {
            let v = device.vtbl::<IMMDeviceVtbl>();
            ((*v).activate)(device.0, &IID_IAUDIO_CLIENT, CLSCTX_ALL, ptr::null_mut(), &mut client.0)
        };
        if hr < 0 {
            return Err(format!("Activate(IAudioClient) failed: HRESULT 0x{:08X}", hr as u32));
        }
        let v = unsafe { client.vtbl::<IAudioClientVtbl>() };

        let mut fmt: *mut WaveFormatEx = ptr::null_mut();
        if unsafe { ((*v).get_mix_format)(client.0, &mut fmt) } < 0 || fmt.is_null() {
            return Err("GetMixFormat failed".into());
        }
        // 引擎的混音格式原样用：改一个字段就换了一条重采样路径，标定的就不是
        // 生产流走的那一条了。
        let (rate, block_align) =
            unsafe { ((*fmt).samples_per_sec, (*fmt).block_align as u32) };
        let free_fmt = || unsafe { CoTaskMemFree(fmt as *mut c_void) };
        if rate == 0 || block_align == 0 {
            free_fmt();
            return Err(format!("mix format is unusable: rate={rate} block_align={block_align}"));
        }

        let mut default_100ns: i64 = 0;
        let mut min_100ns: i64 = 0;
        let period_frames = if unsafe {
            ((*v).get_device_period)(client.0, &mut default_100ns, &mut min_100ns)
        } >= 0
            && default_100ns > 0
        {
            (default_100ns as u128 * rate as u128 / 10_000_000u128) as u32
        } else {
            0
        };

        let Some(event) = Event::create() else {
            free_fmt();
            return Err("CreateEventW failed".into());
        };

        // 纪律 1 + 纪律 3：共享模式、事件驱动、`hnsBufferDuration = 0`
        // （= 让引擎自己挑，与 cpal 的 `BufferSize::Default` 同一条路）。
        let hr = unsafe {
            ((*v).initialize)(
                client.0,
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                0,
                0,
                fmt,
                ptr::null(),
            )
        };
        free_fmt();
        if hr < 0 {
            return Err(format!("IAudioClient::Initialize failed: HRESULT 0x{:08X}", hr as u32));
        }
        if unsafe { ((*v).set_event_handle)(client.0, event.0) } < 0 {
            return Err("SetEventHandle failed".into());
        }

        let mut buffer_frames: u32 = 0;
        if unsafe { ((*v).get_buffer_size)(client.0, &mut buffer_frames) } < 0 || buffer_frames == 0
        {
            return Err("GetBufferSize failed".into());
        }

        let mut render = ComPtr::null();
        if unsafe { ((*v).get_service)(client.0, &IID_IAUDIO_RENDER_CLIENT, &mut render.0) } < 0 {
            return Err("GetService(IAudioRenderClient) failed".into());
        }
        let mut clock = ComPtr::null();
        if unsafe { ((*v).get_service)(client.0, &IID_IAUDIO_CLOCK, &mut clock.0) } < 0 {
            return Err("GetService(IAudioClock) failed".into());
        }
        let rv = unsafe { render.vtbl::<IAudioRenderClientVtbl>() };
        let cv = unsafe { clock.vtbl::<IAudioClockVtbl>() };

        let mut freq: u64 = 0;
        if unsafe { ((*cv).get_frequency)(clock.0, &mut freq) } < 0 || freq == 0 {
            return Err("IAudioClock::GetFrequency failed".into());
        }

        // 预填一整个缓冲，然后 Start —— 与 cpal 同序。先 Start 后填会让开头几拍
        // 欠载，把稳态点推到更后面（还可能在用户的扬声器上留下一声爆音）。
        let mut written: u64 = 0;
        written += write_silence(rv, &render, buffer_frames)?;

        let hr = unsafe { ((*v).start)(client.0) };
        if hr < 0 {
            return Err(format!("IAudioClient::Start failed: HRESULT 0x{:08X}", hr as u32));
        }
        let _stop = Started(&client);

        let mut lag: Vec<u32> = Vec::with_capacity(TOTAL_EVENTS as usize);
        let mut pads: Vec<u32> = Vec::with_capacity(TOTAL_EVENTS as usize);
        for i in 0..TOTAL_EVENTS {
            if unsafe { WaitForSingleObject(event.0, WAIT_MS) } != WAIT_OBJECT_0 {
                return Err(format!("event never fired (tick {i}/{TOTAL_EVENTS})"));
            }
            let mut padding: u32 = 0;
            if unsafe { ((*v).get_current_padding)(client.0, &mut padding) } < 0 {
                return Err("GetCurrentPadding failed".into());
            }
            let mut pos: u64 = 0;
            let mut qpc: u64 = 0;
            if unsafe { ((*cv).get_position)(clock.0, &mut pos, &mut qpc) } < 0 {
                return Err("IAudioClock::GetPosition failed".into());
            }
            // `GetFrequency` 在共享模式下是「每秒多少个 position 单位」，
            // 实测 384000 = 48000 × 8 B/帧 ⇒ position 的单位是字节。
            // 不硬写 /block_align：换个格式那条常数就错了，而 freq 是问来的。
            let pos_frames = (pos as u128 * rate as u128 / freq as u128) as u64;
            if i >= WARMUP_EVENTS {
                lag.push(written.saturating_sub(pos_frames) as u32);
                pads.push(padding);
            }
            // 纪律 3：把缓冲顶满，与 cpal 的 `frames_available = max − padding` 逐字一致。
            let want = buffer_frames.saturating_sub(padding);
            if want > 0 {
                written += write_silence(rv, &render, want)?;
            }
        }

        if lag.is_empty() {
            return Err("no steady-state samples collected".into());
        }
        let samples = lag.len() as u32;
        let spread = lag.iter().max().copied().unwrap_or(0) - lag.iter().min().copied().unwrap_or(0);
        Ok(OutputCalibration {
            frames: median(&mut lag),
            rate,
            device: name,
            buffer_frames,
            padding_frames: median(&mut pads),
            period_frames,
            samples,
            spread_frames: spread,
        })
    }

    /// 写 `frames` 帧**静音**。用 `AUDCLNT_BUFFERFLAGS_SILENT` 而不是自己填 0：
    /// 那个标志让引擎跳过整块内存写，且语义上明说「这一段是静音」。
    fn write_silence(
        rv: *const IAudioRenderClientVtbl,
        render: &ComPtr,
        frames: u32,
    ) -> Result<u64, String> {
        let mut buf: *mut u8 = ptr::null_mut();
        let hr = unsafe { ((*rv).get_buffer)(render.0, frames, &mut buf) };
        if hr < 0 {
            return Err(format!("IAudioRenderClient::GetBuffer failed: HRESULT 0x{:08X}", hr as u32));
        }
        let hr = unsafe { ((*rv).release_buffer)(render.0, frames, AUDCLNT_BUFFERFLAGS_SILENT) };
        if hr < 0 {
            return Err(format!("ReleaseBuffer failed: HRESULT 0x{:08X}", hr as u32));
        }
        Ok(frames as u64)
    }
}

// ---------------------------------------------------------------- 其它平台

#[cfg(not(windows))]
mod imp {
    //! macOS **不需要**标定：`kAudioDevicePropertyLatency` / `SafetyOffset` /
    //! `kAudioStreamPropertyLatency` / `BufferFrameSize` 四项免开流可读，
    //! `devlat::imp` 已经在读，标 `LatSource::Api`。
    //!
    //! 所以这里返回 `Err` 而不是 `unimplemented!()`：调用方的失败路径本来就要
    //! 保留 `devlat` 的读数，panic 只会让一个「本平台用不着这条路」的事实
    //! 把 daemon 打死。

    use super::OutputCalibration;
    use crate::devlat::DevTarget;

    pub fn calibrate_output(_t: DevTarget<'_>) -> Result<OutputCalibration, String> {
        Err("output calibration is Windows-only; CoreAudio exposes the latency properties directly"
            .into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cal(device: Option<&str>, rate: u32, frames: u32, padding: u32) -> OutputCalibration {
        OutputCalibration {
            frames,
            rate,
            device: device.map(str::to_string),
            buffer_frames: 1056,
            padding_frames: padding,
            period_frames: 480,
            samples: 40,
            spread_frames: 2,
        }
    }

    /// 30-win 2026-08-04 实测那一组的形状：`written − position` = 2012 帧 @48k
    /// = 41.92 ms，`padding` 576 ⇒ `K` = 1436 帧 = 29.92 ms。
    ///
    /// 夹具是**记录下来的**测量，不是现场重测：这台是 macOS，跑不出 WASAPI 读数，
    /// 而「41.9 ms 这个量级」正是本模块存在的全部理由，它不该只活在文档里。
    #[test]
    fn the_measured_windows_output_lag_decomposes_the_way_the_model_says() {
        let c = cal(Some("Speakers (ADAM Audio D3V  )"), 48_000, 2012, 576);
        assert!((c.ms().unwrap() - 41.9166).abs() < 1e-3, "实测 41.92 ms");
        assert_eq!(c.k_frames(), Some(1436), "K = 2012 − 576");
        // 换台声卡也逃不掉的那 30 ms
        let k_ms = c.k_frames().unwrap() as f64 * 1000.0 / 48_000.0;
        assert!((k_ms - 29.9166).abs() < 1e-3);
        // 与它替换掉的那个下限（GetDevicePeriod 480 帧 = 10 ms）差 4.2 倍
        let period_ms = c.period_frames as f64 * 1000.0 / 48_000.0;
        assert!(
            (c.ms().unwrap() / period_ms - 4.19).abs() < 0.02,
            "低报倍数应当是 4.2，got {}",
            c.ms().unwrap() / period_ms
        );
    }

    /// **标定值绝不许被当成真值。** `Assumed` ⇒ `is_exact()` 为假 ⇒ UI 永远带「≥」。
    ///
    /// 注入对照（在 macOS 上就能红，这是把标签抽成常量的全部理由）：
    /// 把 [`WINDOWS_CALIBRATED_SOURCE`] 改成 `LatSource::Api` ⇒ 本条红。
    #[test]
    fn a_calibrated_number_is_still_not_an_exact_one() {
        assert_eq!(
            WINDOWS_CALIBRATED_SOURCE,
            LatSource::Assumed,
            "开流竞态实测可差 384 帧（8 ms），且标定流≠生产流 —— 够不上 Api"
        );
        assert!(
            !WINDOWS_CALIBRATED_SOURCE.is_exact(),
            "标定值一旦被当成精确值，Windows 侧的「≥」就会消失，而 §9.1 明说 41.9 ms 的内部构成未分解"
        );
        // 但它确实比 `GetDevicePeriod` 那个下限可信：`worse()` 的序里 Assumed < Unreliable
        assert_eq!(
            crate::devlat::WINDOWS_DEVICE_PERIOD_SOURCE,
            LatSource::Unreliable,
            "标定值的对照物没变的话，本条的『更可信』就无从谈起"
        );
    }

    /// 缓存命中判据：端点名 + 速率**两项全等**才算数。
    ///
    /// 逐项注入：每次只改一个字段，都必须判不匹配。少比一项的后果是具体的——
    /// 用户把默认输出从 USB 音箱切到 HDMI 显示器，两者 `K` 差 1.3 ms、
    /// `padding` 可能完全不同，而错数不会有任何迹象。
    #[test]
    fn a_calibration_only_matches_the_endpoint_it_was_taken_on() {
        let c = cal(Some("Speakers (ADAM Audio D3V  )"), 48_000, 2012, 576);
        assert!(c.matches(Some("Speakers (ADAM Audio D3V  )"), 48_000));
        assert!(!c.matches(Some("Odyssey G8 (2- NVIDIA High Definition Audio)"), 48_000), "换端点");
        assert!(!c.matches(Some("Speakers (ADAM Audio D3V  )"), 44_100), "换速率");
        assert!(!c.matches(None, 48_000), "对方名字读不到 ⇒ 无法核对 ⇒ 不许命中");
        assert!(!c.matches(Some("Speakers (ADAM Audio D3V  )"), 0), "速率 0 ⇒ 换算不成毫秒");
        // 标定自己没读到名字时，任何比对都不许命中
        let anon = cal(None, 48_000, 2012, 576);
        assert!(!anon.matches(Some("Speakers (ADAM Audio D3V  )"), 48_000));
        assert!(!anon.matches(None, 48_000));
    }

    /// `k_frames()` 不许回绕。padding 比总量还大是一个荒谬输入（只可能来自
    /// 采样期间设备被拔掉一类的现场），但 `2012u32 - 5000u32` 会 panic 或
    /// 在 release 下回绕成 42 亿帧 —— 那个数会一路走到 UI。
    #[test]
    fn an_absurd_decomposition_yields_none_not_a_wrapped_number() {
        let c = cal(Some("x"), 48_000, 100, 5_000);
        assert_eq!(c.k_frames(), None);
    }

    /// 速率读不到 ⇒ `ms()` 是 `None`，**不是 0**。与 `DevLatency::ms()` 同一条规则。
    #[test]
    fn a_rateless_calibration_has_no_milliseconds() {
        let c = cal(Some("x"), 0, 2012, 576);
        assert_eq!(c.ms(), None);
    }

    /// 非 Windows 平台**明确拒绝**，而不是返回一个空壳成功。
    ///
    /// 返回 `Ok(zeroed)` 会让 mac 上的 `play_dev` 变成 0 ms 且自称标定过——
    /// 正是「读不到就用 0 冒充」的那个失败形态，只不过换了个入口。
    #[cfg(not(windows))]
    #[test]
    fn a_non_windows_calibration_fails_loudly() {
        let r = calibrate_output(DevTarget::Default);
        assert!(r.is_err(), "mac 上不该有标定值：CoreAudio 的属性免开流可读");
    }
}
