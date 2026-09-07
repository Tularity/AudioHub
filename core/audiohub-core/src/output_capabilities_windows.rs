//! Read-only native capability scan for the default Windows render endpoint.

use std::ffi::c_void;
use std::mem;
use std::ptr;
use std::slice;

use super::{NativeOutputCapabilities, OutputMixFormat, SpatialAudioCapabilities};

#[path = "spatial_output_windows.rs"]
mod spatial_renderer;
pub(super) use spatial_renderer::start_spatial_output;

type Hresult = i32;

const COINIT_MULTITHREADED: u32 = 0;
const RPC_E_CHANGED_MODE: Hresult = 0x8001_0106u32 as i32;
const E_NOINTERFACE: Hresult = 0x8000_4002u32 as i32;
const ERROR_NOT_FOUND_HRESULT: Hresult = 0x8007_0490u32 as i32;
const REGDB_E_CLASSNOTREG: Hresult = 0x8004_0154u32 as i32;
const RO_E_METADATA_NAME_NOT_FOUND: Hresult = 0x8000_000Fu32 as i32;
const CLSCTX_ALL: u32 = 0x17;
const E_RENDER: u32 = 0;
const E_CONSOLE: u32 = 0;

const WAVE_FORMAT_PCM: u16 = 0x0001;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 0x0003;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
const EXTENSIBLE_EXTRA_BYTES: u16 = 22;
const VALID_BITS_OFFSET: usize = 18;
const CHANNEL_MASK_OFFSET: usize = 20;
const SUBFORMAT_OFFSET: usize = 24;

const CLSID_MMDEVICE_ENUMERATOR: Guid = Guid::new(
    0xBCDE0395,
    0xE52F,
    0x467C,
    [0x8E, 0x3D, 0xC4, 0x57, 0x92, 0x91, 0x69, 0x2E],
);
const IID_IMMDEVICE_ENUMERATOR: Guid = Guid::new(
    0xA95664D2,
    0x9614,
    0x4F35,
    [0xA7, 0x46, 0xDE, 0x8D, 0xB6, 0x36, 0x17, 0xE6],
);
const IID_IAUDIO_CLIENT: Guid = Guid::new(
    0x1CB9AD4C,
    0xDBFA,
    0x4C32,
    [0xB1, 0x78, 0xC2, 0xF5, 0x68, 0xA7, 0x03, 0xB2],
);
const IID_ISPATIAL_AUDIO_CLIENT: Guid = Guid::new(
    0xBBF8E066,
    0xAAAA,
    0x49BE,
    [0x9A, 0x4D, 0xFD, 0x2A, 0x85, 0x8E, 0xA2, 0x7F],
);
const IID_SPATIAL_CONFIGURATION_STATICS: Guid = Guid::new(
    0x3EC37F7B,
    0x936D,
    0x4E04,
    [0x97, 0x28, 0x28, 0x27, 0xD9, 0xF7, 0x58, 0xC4],
);
#[allow(dead_code)]
const IID_SPATIAL_CONFIGURATION: Guid = Guid::new(
    0xEE830034,
    0x61CF,
    0x5749,
    [0x9D, 0xA4, 0x10, 0xF0, 0xFE, 0x02, 0x81, 0x99],
);
const SUBTYPE_PCM: Guid = Guid::new(
    1,
    0,
    0x0010,
    [0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71],
);
const SUBTYPE_IEEE_FLOAT: Guid = Guid::new(
    3,
    0,
    0x0010,
    [0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71],
);

const SPATIAL_CONFIGURATION_CLASS: &str = "Windows.Media.Audio.SpatialAudioDeviceConfiguration";
const AUDIO_RENDER_INTERFACE_PREFIX: &str = r"\\?\SWD#MMDEVAPI#";
const AUDIO_RENDER_INTERFACE_SUFFIX: &str = "#{e6327cad-dcec-4949-ae8a-991e976a79d2}";

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
struct Guid {
    d1: u32,
    d2: u16,
    d3: u16,
    d4: [u8; 8],
}

impl Guid {
    const fn new(d1: u32, d2: u16, d3: u16, d4: [u8; 8]) -> Self {
        Self { d1, d2, d3, d4 }
    }
}

#[repr(C)]
#[allow(dead_code)]
struct IUnknownVtbl {
    query_interface:
        unsafe extern "system" fn(*mut c_void, *const Guid, *mut *mut c_void) -> Hresult,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
}

#[repr(C)]
#[allow(dead_code)]
struct IInspectableVtbl {
    base: IUnknownVtbl,
    get_iids: unsafe extern "system" fn(*mut c_void, *mut u32, *mut *mut Guid) -> Hresult,
    get_runtime_class_name: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> Hresult,
    get_trust_level: unsafe extern "system" fn(*mut c_void, *mut i32) -> Hresult,
}

#[repr(C)]
#[allow(dead_code)]
struct IMMDeviceEnumeratorVtbl {
    base: IUnknownVtbl,
    enum_audio_endpoints: usize,
    get_default_audio_endpoint:
        unsafe extern "system" fn(*mut c_void, u32, u32, *mut *mut c_void) -> Hresult,
    get_device: usize,
    register_endpoint_notification_callback: usize,
    unregister_endpoint_notification_callback: usize,
}

#[repr(C)]
#[allow(dead_code)]
struct IMMDeviceVtbl {
    base: IUnknownVtbl,
    activate: unsafe extern "system" fn(
        *mut c_void,
        *const Guid,
        u32,
        *mut c_void,
        *mut *mut c_void,
    ) -> Hresult,
    open_property_store: usize,
    get_id: unsafe extern "system" fn(*mut c_void, *mut *mut u16) -> Hresult,
    get_state: usize,
}

#[repr(C)]
#[allow(dead_code)]
struct IAudioClientVtbl {
    base: IUnknownVtbl,
    initialize: usize,
    get_buffer_size: usize,
    get_stream_latency: usize,
    get_current_padding: usize,
    is_format_supported: usize,
    get_mix_format: unsafe extern "system" fn(*mut c_void, *mut *mut WaveFormatEx) -> Hresult,
    get_device_period: usize,
    start: usize,
    stop: usize,
    reset: usize,
    set_event_handle: usize,
    get_service: usize,
}

#[repr(C)]
#[allow(dead_code)]
struct ISpatialAudioClientVtbl {
    base: IUnknownVtbl,
    get_static_object_position: usize,
    get_native_static_object_type_mask: unsafe extern "system" fn(*mut c_void, *mut u32) -> Hresult,
    get_max_dynamic_object_count: unsafe extern "system" fn(*mut c_void, *mut u32) -> Hresult,
    get_supported_audio_object_format_enumerator: usize,
    get_max_frame_count: usize,
    is_audio_object_format_supported:
        unsafe extern "system" fn(*mut c_void, *const WaveFormatEx) -> Hresult,
    is_spatial_audio_stream_available: usize,
    activate_spatial_audio_stream: unsafe extern "system" fn(
        *mut c_void,
        *const c_void,
        *const Guid,
        *mut *mut c_void,
    ) -> Hresult,
}

#[repr(C)]
#[allow(dead_code)]
struct SpatialConfigurationStaticsVtbl {
    base: IInspectableVtbl,
    get_for_device_id:
        unsafe extern "system" fn(*mut c_void, *mut c_void, *mut *mut c_void) -> Hresult,
}

#[repr(C)]
#[allow(dead_code)]
struct SpatialConfigurationVtbl {
    base: IInspectableVtbl,
    get_device_id: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> Hresult,
    get_is_spatial_audio_supported: unsafe extern "system" fn(*mut c_void, *mut u8) -> Hresult,
    is_spatial_audio_format_supported:
        unsafe extern "system" fn(*mut c_void, *mut c_void, *mut u8) -> Hresult,
    get_active_spatial_audio_format:
        unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> Hresult,
    get_default_spatial_audio_format:
        unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> Hresult,
}

// mmreg.h gives WAVEFORMATEX two-byte packing and an 18-byte base size.
#[repr(C, packed(2))]
#[derive(Clone, Copy)]
#[allow(dead_code)]
struct WaveFormatEx {
    format_tag: u16,
    channels: u16,
    samples_per_sec: u32,
    avg_bytes_per_sec: u32,
    block_align: u16,
    bits_per_sample: u16,
    extra_size: u16,
}

#[link(name = "ole32")]
extern "system" {
    fn CoInitializeEx(reserved: *mut c_void, flags: u32) -> Hresult;
    fn CoUninitialize();
    fn CoCreateInstance(
        clsid: *const Guid,
        outer: *mut c_void,
        context: u32,
        iid: *const Guid,
        out: *mut *mut c_void,
    ) -> Hresult;
    fn CoTaskMemFree(memory: *mut c_void);
}

#[link(name = "kernel32")]
extern "system" {
    fn LoadLibraryW(name: *const u16) -> *mut c_void;
    fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
    fn FreeLibrary(module: *mut c_void) -> i32;
}

#[derive(Debug)]
enum ScanError {
    Unavailable,
    Hr(&'static str, Hresult),
    Message(&'static str),
}

impl ScanError {
    fn message(self) -> String {
        match self {
            Self::Unavailable => "default render endpoint unavailable".to_string(),
            Self::Hr(operation, hr) => {
                format!("{operation} failed: HRESULT 0x{:08X}", hr as u32)
            }
            Self::Message(message) => message.to_string(),
        }
    }
}

struct Apartment {
    owned: bool,
}

impl Apartment {
    fn enter() -> Result<Self, ScanError> {
        let hr = unsafe { CoInitializeEx(ptr::null_mut(), COINIT_MULTITHREADED) };
        if hr >= 0 {
            Ok(Self { owned: true })
        } else if hr == RPC_E_CHANGED_MODE {
            Ok(Self { owned: false })
        } else {
            Err(ScanError::Hr("CoInitializeEx", hr))
        }
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
    fn null() -> Self {
        Self(ptr::null_mut())
    }

    unsafe fn vtbl<T>(&self) -> *const T {
        *(self.0 as *const *const T)
    }
}

impl Drop for ComPtr {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                ((*self.vtbl::<IUnknownVtbl>()).release)(self.0);
            }
        }
    }
}

struct TaskMemory(*mut c_void);

impl Drop for TaskMemory {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CoTaskMemFree(self.0) };
        }
    }
}

type RoGetActivationFactoryFn =
    unsafe extern "system" fn(*mut c_void, *const Guid, *mut *mut c_void) -> Hresult;
type WindowsCreateStringFn =
    unsafe extern "system" fn(*const u16, u32, *mut *mut c_void) -> Hresult;
type WindowsDeleteStringFn = unsafe extern "system" fn(*mut c_void) -> Hresult;
type WindowsGetStringRawBufferFn = unsafe extern "system" fn(*mut c_void, *mut u32) -> *const u16;
type RoInitializeFn = unsafe extern "system" fn(u32) -> Hresult;
type RoUninitializeFn = unsafe extern "system" fn();

struct WinRtApi {
    module: *mut c_void,
    ro_get_activation_factory: RoGetActivationFactoryFn,
    windows_create_string: WindowsCreateStringFn,
    windows_delete_string: WindowsDeleteStringFn,
    windows_get_string_raw_buffer: WindowsGetStringRawBufferFn,
    ro_uninitialize: RoUninitializeFn,
    owns_runtime: bool,
}

impl WinRtApi {
    fn load() -> Result<Option<Self>, ScanError> {
        let dll: Vec<u16> = "combase.dll".encode_utf16().chain(Some(0)).collect();
        let module = unsafe { LoadLibraryW(dll.as_ptr()) };
        if module.is_null() {
            return Ok(None);
        }
        let ro = unsafe { GetProcAddress(module, b"RoGetActivationFactory\0".as_ptr()) };
        let create = unsafe { GetProcAddress(module, b"WindowsCreateString\0".as_ptr()) };
        let delete = unsafe { GetProcAddress(module, b"WindowsDeleteString\0".as_ptr()) };
        let raw = unsafe { GetProcAddress(module, b"WindowsGetStringRawBuffer\0".as_ptr()) };
        let initialize = unsafe { GetProcAddress(module, b"RoInitialize\0".as_ptr()) };
        let uninitialize = unsafe { GetProcAddress(module, b"RoUninitialize\0".as_ptr()) };
        if ro.is_null()
            || create.is_null()
            || delete.is_null()
            || raw.is_null()
            || initialize.is_null()
            || uninitialize.is_null()
        {
            unsafe { FreeLibrary(module) };
            return Ok(None);
        }
        let initialize: RoInitializeFn = unsafe { mem::transmute(initialize) };
        let mut hr = unsafe { initialize(1) };
        if hr == RPC_E_CHANGED_MODE {
            hr = unsafe { initialize(0) };
        }
        if hr < 0 {
            unsafe { FreeLibrary(module) };
            return Err(ScanError::Hr("RoInitialize", hr));
        }
        Ok(Some(Self {
            module,
            ro_get_activation_factory: unsafe { mem::transmute(ro) },
            windows_create_string: unsafe { mem::transmute(create) },
            windows_delete_string: unsafe { mem::transmute(delete) },
            windows_get_string_raw_buffer: unsafe { mem::transmute(raw) },
            ro_uninitialize: unsafe { mem::transmute(uninitialize) },
            owns_runtime: true,
        }))
    }

    fn create_string(&self, value: &str) -> Result<HString<'_>, ScanError> {
        let wide: Vec<u16> = value.encode_utf16().collect();
        let length = u32::try_from(wide.len())
            .map_err(|_| ScanError::Message("WinRT string exceeds native length limit"))?;
        let mut handle = ptr::null_mut();
        let hr = unsafe { (self.windows_create_string)(wide.as_ptr(), length, &mut handle) };
        let string = HString { handle, api: self };
        if hr < 0 {
            Err(ScanError::Hr("WindowsCreateString", hr))
        } else if string.handle.is_null() && length != 0 {
            Err(ScanError::Message(
                "WindowsCreateString returned a null HSTRING",
            ))
        } else {
            Ok(string)
        }
    }

    fn read_string(
        &self,
        string: &HString<'_>,
        maximum: usize,
        operation: &'static str,
    ) -> Result<Option<String>, ScanError> {
        if string.handle.is_null() {
            return Ok(None);
        }
        let mut length = 0u32;
        let raw = unsafe { (self.windows_get_string_raw_buffer)(string.handle, &mut length) };
        if length == 0 {
            return Ok(None);
        }
        if raw.is_null() || length as usize > maximum {
            return Err(ScanError::Message(operation));
        }
        let value = String::from_utf16(unsafe { slice::from_raw_parts(raw, length as usize) })
            .map_err(|_| ScanError::Message(operation))?;
        if value.len() > maximum || value.chars().any(char::is_control) {
            return Err(ScanError::Message(operation));
        }
        Ok(Some(value))
    }
}

impl Drop for WinRtApi {
    fn drop(&mut self) {
        if self.owns_runtime {
            unsafe { (self.ro_uninitialize)() };
        }
        unsafe { FreeLibrary(self.module) };
    }
}

struct HString<'a> {
    handle: *mut c_void,
    api: &'a WinRtApi,
}

impl Drop for HString<'_> {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe {
                (self.api.windows_delete_string)(self.handle);
            }
        }
    }
}

fn check(hr: Hresult, operation: &'static str) -> Result<(), ScanError> {
    if hr < 0 {
        Err(ScanError::Hr(operation, hr))
    } else {
        Ok(())
    }
}

fn create_enumerator() -> Result<ComPtr, ScanError> {
    let mut enumerator = ComPtr::null();
    let hr = unsafe {
        CoCreateInstance(
            &CLSID_MMDEVICE_ENUMERATOR,
            ptr::null_mut(),
            CLSCTX_ALL,
            &IID_IMMDEVICE_ENUMERATOR,
            &mut enumerator.0,
        )
    };
    check(hr, "CoCreateInstance(MMDeviceEnumerator)")?;
    if enumerator.0.is_null() {
        return Err(ScanError::Message(
            "CoCreateInstance(MMDeviceEnumerator) returned a null interface",
        ));
    }
    Ok(enumerator)
}

fn default_render_endpoint(enumerator: &ComPtr) -> Result<ComPtr, ScanError> {
    let mut device = ComPtr::null();
    let hr = unsafe {
        ((*enumerator.vtbl::<IMMDeviceEnumeratorVtbl>()).get_default_audio_endpoint)(
            enumerator.0,
            E_RENDER,
            E_CONSOLE,
            &mut device.0,
        )
    };
    if hr == ERROR_NOT_FOUND_HRESULT {
        return Err(ScanError::Unavailable);
    }
    check(hr, "GetDefaultAudioEndpoint(eRender, eConsole)")?;
    if device.0.is_null() {
        return Err(ScanError::Message(
            "GetDefaultAudioEndpoint returned a null endpoint",
        ));
    }
    Ok(device)
}

fn endpoint_id(device: &ComPtr) -> Result<String, ScanError> {
    let mut raw = ptr::null_mut();
    let hr = unsafe { ((*device.vtbl::<IMMDeviceVtbl>()).get_id)(device.0, &mut raw) };
    let allocation = TaskMemory(raw as *mut c_void);
    check(hr, "IMMDevice::GetId")?;
    if allocation.0.is_null() {
        return Err(ScanError::Message("IMMDevice::GetId returned a null ID"));
    }
    let raw = allocation.0 as *const u16;
    let mut length = None;
    for index in 0..=1024 {
        if unsafe { *raw.add(index) } == 0 {
            length = Some(index);
            break;
        }
    }
    let length = length.ok_or(ScanError::Message("endpoint ID exceeds 1024 UTF-16 units"))?;
    let id = String::from_utf16(unsafe { slice::from_raw_parts(raw, length) })
        .map_err(|_| ScanError::Message("endpoint ID is not valid UTF-16"))?;
    if id.is_empty() || id.len() > 1024 || id.chars().any(char::is_control) {
        return Err(ScanError::Message("endpoint ID is invalid"));
    }
    Ok(id)
}

fn activate(device: &ComPtr, iid: &Guid, operation: &'static str) -> Result<ComPtr, ScanError> {
    let mut interface = ComPtr::null();
    let hr = unsafe {
        ((*device.vtbl::<IMMDeviceVtbl>()).activate)(
            device.0,
            iid,
            CLSCTX_ALL,
            ptr::null_mut(),
            &mut interface.0,
        )
    };
    check(hr, operation)?;
    if interface.0.is_null() {
        return Err(ScanError::Message(
            "endpoint activation returned a null interface",
        ));
    }
    Ok(interface)
}

fn sample_format(tag: u16, bits: u16, valid_bits: Option<u16>, subtype: Option<Guid>) -> String {
    let effective_bits = valid_bits.filter(|value| *value != 0).unwrap_or(bits);
    match (tag, subtype, effective_bits) {
        (WAVE_FORMAT_IEEE_FLOAT, None, 32)
        | (WAVE_FORMAT_EXTENSIBLE, Some(SUBTYPE_IEEE_FLOAT), 32) => "f32".to_string(),
        (WAVE_FORMAT_IEEE_FLOAT, None, 64)
        | (WAVE_FORMAT_EXTENSIBLE, Some(SUBTYPE_IEEE_FLOAT), 64) => "f64".to_string(),
        (WAVE_FORMAT_PCM, None, 16) | (WAVE_FORMAT_EXTENSIBLE, Some(SUBTYPE_PCM), 16) => {
            "i16".to_string()
        }
        (WAVE_FORMAT_PCM, None, 24) | (WAVE_FORMAT_EXTENSIBLE, Some(SUBTYPE_PCM), 24) => {
            "i24".to_string()
        }
        (WAVE_FORMAT_PCM, None, 32) | (WAVE_FORMAT_EXTENSIBLE, Some(SUBTYPE_PCM), 32) => {
            "i32".to_string()
        }
        _ => "unknown".to_string(),
    }
}

unsafe fn decode_wave_format(format: *const WaveFormatEx) -> Result<OutputMixFormat, ScanError> {
    if format.is_null() {
        return Err(ScanError::Message("GetMixFormat returned a null format"));
    }
    let base = ptr::read_unaligned(format);
    let tag = base.format_tag;
    let channels = base.channels;
    let sample_rate = base.samples_per_sec;
    let bits = base.bits_per_sample;
    let extra_size = base.extra_size;
    let (valid_bits, channel_mask, subtype) = if tag == WAVE_FORMAT_EXTENSIBLE {
        if extra_size < EXTENSIBLE_EXTRA_BYTES {
            return Err(ScanError::Message(
                "WAVEFORMATEXTENSIBLE has a truncated extension",
            ));
        }
        let bytes = format as *const u8;
        (
            Some(ptr::read_unaligned(
                bytes.add(VALID_BITS_OFFSET) as *const u16
            )),
            Some(ptr::read_unaligned(
                bytes.add(CHANNEL_MASK_OFFSET) as *const u32
            )),
            Some(ptr::read_unaligned(
                bytes.add(SUBFORMAT_OFFSET) as *const Guid
            )),
        )
    } else {
        (None, None, None)
    };
    if !(8_000..=768_000).contains(&sample_rate) {
        return Err(ScanError::Message("mix format sample rate is out of range"));
    }
    if !(1..=64).contains(&channels) {
        return Err(ScanError::Message(
            "mix format channel count is out of range",
        ));
    }
    Ok(OutputMixFormat {
        sample_rate,
        channels,
        sample_format: sample_format(tag, bits, valid_bits, subtype),
        channel_mask,
    })
}

fn mix_format(device: &ComPtr) -> Result<OutputMixFormat, ScanError> {
    let client = activate(
        device,
        &IID_IAUDIO_CLIENT,
        "IMMDevice::Activate(IAudioClient)",
    )?;
    let mut raw = ptr::null_mut();
    let hr = unsafe { ((*client.vtbl::<IAudioClientVtbl>()).get_mix_format)(client.0, &mut raw) };
    let allocation = TaskMemory(raw as *mut c_void);
    check(hr, "IAudioClient::GetMixFormat")?;
    unsafe { decode_wave_format(allocation.0 as *const WaveFormatEx) }
}

fn render_interface_id(endpoint_id: &str) -> String {
    // WinRT binds to DEVINTERFACE_AUDIO_RENDER, not the raw MMDevice ID. Passing
    // the raw ID succeeds but silently reports zero format GUIDs on real hosts.
    format!("{AUDIO_RENDER_INTERFACE_PREFIX}{endpoint_id}{AUDIO_RENDER_INTERFACE_SUFFIX}")
}

fn is_zero_guid(value: &str) -> bool {
    let value = value.trim().trim_start_matches('{').trim_end_matches('}');
    let mut zeroes = 0usize;
    for byte in value.bytes() {
        match byte {
            b'0' => zeroes += 1,
            b'-' => {}
            _ => return false,
        }
    }
    zeroes == 32
}

fn normalize_format_guid(value: Option<String>) -> Option<String> {
    value.filter(|value| !is_zero_guid(value))
}

fn winrt_unavailable(hr: Hresult) -> bool {
    hr == REGDB_E_CLASSNOTREG || hr == RO_E_METADATA_NAME_NOT_FOUND || hr == E_NOINTERFACE
}

fn winrt_formats(endpoint_id: &str) -> Result<Option<(Option<String>, Option<String>)>, ScanError> {
    let Some(api) = WinRtApi::load()? else {
        return Ok(None);
    };
    let class_name = api.create_string(SPATIAL_CONFIGURATION_CLASS)?;
    let mut statics = ComPtr::null();
    let hr = unsafe {
        (api.ro_get_activation_factory)(
            class_name.handle,
            &IID_SPATIAL_CONFIGURATION_STATICS,
            &mut statics.0,
        )
    };
    if winrt_unavailable(hr) {
        return Ok(None);
    }
    check(
        hr,
        "RoGetActivationFactory(SpatialAudioDeviceConfiguration)",
    )?;
    if statics.0.is_null() {
        return Err(ScanError::Message(
            "RoGetActivationFactory returned a null statics interface",
        ));
    }

    let requested_id = render_interface_id(endpoint_id);
    if requested_id.len() > 1024 {
        return Err(ScanError::Message(
            "WinRT render interface ID exceeds 1024 bytes",
        ));
    }
    let requested = api.create_string(&requested_id)?;
    let mut configuration = ComPtr::null();
    let hr = unsafe {
        ((*statics.vtbl::<SpatialConfigurationStaticsVtbl>()).get_for_device_id)(
            statics.0,
            requested.handle,
            &mut configuration.0,
        )
    };
    check(hr, "SpatialAudioDeviceConfiguration::GetForDeviceId")?;
    if configuration.0.is_null() {
        return Err(ScanError::Message(
            "GetForDeviceId returned a null spatial configuration",
        ));
    }
    let vtbl = unsafe { configuration.vtbl::<SpatialConfigurationVtbl>() };

    let mut returned_id = ptr::null_mut();
    let hr = unsafe { ((*vtbl).get_device_id)(configuration.0, &mut returned_id) };
    let returned_id = HString {
        handle: returned_id,
        api: &api,
    };
    check(hr, "SpatialAudioDeviceConfiguration::get_DeviceId")?;
    let actual = api
        .read_string(
            &returned_id,
            1024,
            "WinRT spatial configuration device ID is invalid",
        )?
        .ok_or(ScanError::Message(
            "WinRT spatial configuration returned no device ID",
        ))?;
    if !actual.eq_ignore_ascii_case(&requested_id) {
        return Err(ScanError::Message(
            "WinRT spatial configuration is bound to a different endpoint",
        ));
    }

    let mut active = ptr::null_mut();
    let hr = unsafe { ((*vtbl).get_active_spatial_audio_format)(configuration.0, &mut active) };
    let active = HString {
        handle: active,
        api: &api,
    };
    check(
        hr,
        "SpatialAudioDeviceConfiguration::get_ActiveSpatialAudioFormat",
    )?;
    let active = normalize_format_guid(api.read_string(
        &active,
        128,
        "WinRT active spatial format is invalid",
    )?);

    let mut configured = ptr::null_mut();
    let hr =
        unsafe { ((*vtbl).get_default_spatial_audio_format)(configuration.0, &mut configured) };
    let configured = HString {
        handle: configured,
        api: &api,
    };
    check(
        hr,
        "SpatialAudioDeviceConfiguration::get_DefaultSpatialAudioFormat",
    )?;
    let configured = normalize_format_guid(api.read_string(
        &configured,
        128,
        "WinRT default spatial format is invalid",
    )?);
    Ok(Some((active, configured)))
}

fn spatial_state(
    formats: Option<(Option<String>, Option<String>)>,
    max_dynamic_objects: u32,
    static_object_mask: u32,
) -> SpatialAudioCapabilities {
    match formats {
        None => SpatialAudioCapabilities::Unknown,
        Some((None, configured_format)) => SpatialAudioCapabilities::Inactive { configured_format },
        Some((active_format, configured_format)) => SpatialAudioCapabilities::Available {
            active_format,
            configured_format,
            max_dynamic_objects,
            static_object_mask,
        },
    }
}

fn spatial_audio(device: &ComPtr, endpoint_id: &str) -> SpatialAudioCapabilities {
    let mut client = ComPtr::null();
    let hr = unsafe {
        ((*device.vtbl::<IMMDeviceVtbl>()).activate)(
            device.0,
            &IID_ISPATIAL_AUDIO_CLIENT,
            CLSCTX_ALL,
            ptr::null_mut(),
            &mut client.0,
        )
    };
    if hr == E_NOINTERFACE {
        return SpatialAudioCapabilities::Unsupported;
    }
    if hr < 0 {
        return SpatialAudioCapabilities::Error {
            message: ScanError::Hr("IMMDevice::Activate(ISpatialAudioClient)", hr).message(),
        };
    }
    if client.0.is_null() {
        return SpatialAudioCapabilities::Error {
            message: "ISpatialAudioClient activation returned a null interface".to_string(),
        };
    }
    let vtbl = unsafe { client.vtbl::<ISpatialAudioClientVtbl>() };
    let mut max_dynamic_objects = 0u32;
    let hr = unsafe { ((*vtbl).get_max_dynamic_object_count)(client.0, &mut max_dynamic_objects) };
    if hr < 0 {
        return SpatialAudioCapabilities::Error {
            message: ScanError::Hr("ISpatialAudioClient::GetMaxDynamicObjectCount", hr).message(),
        };
    }
    if max_dynamic_objects > 1024 {
        return SpatialAudioCapabilities::Error {
            message: "spatial dynamic object count exceeds 1024".to_string(),
        };
    }
    let mut static_object_mask = 0u32;
    let hr =
        unsafe { ((*vtbl).get_native_static_object_type_mask)(client.0, &mut static_object_mask) };
    if hr < 0 {
        return SpatialAudioCapabilities::Error {
            message: ScanError::Hr("ISpatialAudioClient::GetNativeStaticObjectTypeMask", hr)
                .message(),
        };
    }
    let formats = match winrt_formats(endpoint_id) {
        Ok(formats) => formats,
        Err(error) => {
            return SpatialAudioCapabilities::Error {
                message: error.message(),
            };
        }
    };
    spatial_state(formats, max_dynamic_objects, static_object_mask)
}

fn scan() -> Result<NativeOutputCapabilities, ScanError> {
    let _apartment = Apartment::enter()?;
    let enumerator = create_enumerator()?;
    let device = default_render_endpoint(&enumerator)?;
    let first_id = endpoint_id(&device)?;
    let mix_format = mix_format(&device)?;
    let spatial_audio = spatial_audio(&device, &first_id);

    let current_device = default_render_endpoint(&enumerator).map_err(|error| match error {
        ScanError::Unavailable => {
            ScanError::Message("default render endpoint changed during capability scan")
        }
        other => other,
    })?;
    let current_id = endpoint_id(&current_device)?;
    if current_id != first_id {
        return Err(ScanError::Message(
            "default render endpoint changed during capability scan",
        ));
    }

    let result = NativeOutputCapabilities::Available {
        endpoint_id: first_id,
        mix_format,
        spatial_audio,
    };
    if result.is_valid() {
        Ok(result)
    } else {
        Err(ScanError::Message(
            "native output capability result failed validation",
        ))
    }
}

pub(super) fn query() -> NativeOutputCapabilities {
    match scan() {
        Ok(result) => result,
        Err(ScanError::Unavailable) => NativeOutputCapabilities::Unavailable,
        Err(error) => NativeOutputCapabilities::Error {
            message: error.message(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_format_and_dynamic_object_resources_are_independent() {
        let active = "{1459AC38-3875-49BF-BB59-0FE80F4D395D}".to_string();
        assert!(matches!(
            spatial_state(Some((Some(active), None)), 0, 3),
            SpatialAudioCapabilities::Available {
                max_dynamic_objects: 0,
                ..
            }
        ));
        assert!(matches!(
            spatial_state(Some((None, None)), 128, 3),
            SpatialAudioCapabilities::Inactive { .. }
        ));
        assert_eq!(
            spatial_state(None, 128, 3),
            SpatialAudioCapabilities::Unknown
        );
    }

    #[test]
    fn winrt_render_id_uses_the_device_interface_binding() {
        assert_eq!(
            render_interface_id("{0.0.0.00000000}.{example}"),
            r"\\?\SWD#MMDEVAPI#{0.0.0.00000000}.{example}#{e6327cad-dcec-4949-ae8a-991e976a79d2}"
        );
    }

    #[test]
    fn zero_guid_is_not_published_as_a_format() {
        assert_eq!(
            normalize_format_guid(Some("{00000000-0000-0000-0000-000000000000}".to_string())),
            None
        );
    }

    #[test]
    fn extensible_pcm_decodes_valid_bits_and_channel_mask() {
        let mut bytes = [0u8; 40];
        bytes[0..2].copy_from_slice(&WAVE_FORMAT_EXTENSIBLE.to_le_bytes());
        bytes[2..4].copy_from_slice(&2u16.to_le_bytes());
        bytes[4..8].copy_from_slice(&48_000u32.to_le_bytes());
        bytes[14..16].copy_from_slice(&32u16.to_le_bytes());
        bytes[16..18].copy_from_slice(&22u16.to_le_bytes());
        bytes[18..20].copy_from_slice(&24u16.to_le_bytes());
        bytes[20..24].copy_from_slice(&3u32.to_le_bytes());
        unsafe {
            ptr::write_unaligned(bytes[24..].as_mut_ptr() as *mut Guid, SUBTYPE_PCM);
            let decoded = decode_wave_format(bytes.as_ptr() as *const WaveFormatEx).unwrap();
            assert_eq!(decoded.sample_rate, 48_000);
            assert_eq!(decoded.channels, 2);
            assert_eq!(decoded.sample_format, "i24");
            assert_eq!(decoded.channel_mask, Some(3));
        }
    }
}
