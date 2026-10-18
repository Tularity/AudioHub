//! Read-only Windows probe for `device_facts` (design §3.3).
//!
//! Path: default endpoint (`IMMDeviceEnumerator::GetDefaultAudioEndpoint`,
//! eConsole, as volume.rs and output_capabilities_windows.rs use) -> endpoint
//! property store -> `IDeviceTopology` connector 0 ->
//! `IConnector::GetDeviceIdConnectedTo` (the KS filter's device-interface
//! path) -> `CM_Get_Device_Interface_PropertyW(DEVPKEY_Device_InstanceId)` ->
//! the KS adapter devnode -> `CM_Get_Parent` up the tree.
//!
//! The endpoint's own `SWD\MMDEVAPI\...` devnode is deliberately NOT used:
//! every endpoint has one, so it says nothing about the hardware.
//!
//! No `IAudioClient` is activated and nothing is initialized: activating
//! `IDeviceTopology` and opening the property store read-only are metadata
//! queries that neither stream nor raise the microphone privacy gate.
//!
//! Hand-rolled vtables, as in devlat.rs / volume.rs: the windows-gnu build
//! keeps a raw-dylib-free dependency graph. Slots this module must not call
//! are declared `usize`.

use std::ffi::c_void;
use std::ptr;

use super::win_text::{
    guid_from_bytes, instance_id_from_interface_path, instance_id_from_numbered_prefix,
    string_from_bytes, string_list_from_bytes,
};
use super::{
    windows_chain_terminal, Direction, EndpointProbe, RawFactsSnapshot, WindowsDevnode,
    WindowsEndpointSnapshot,
};

type HRESULT = i32;
type CONFIGRET = u32;
type DEVINST = u32;
type DEVPROPTYPE = u32;

#[repr(C)]
#[derive(Clone, Copy)]
struct GUID {
    d1: u32,
    d2: u16,
    d3: u16,
    d4: [u8; 8],
}

/// PROPERTYKEY and DEVPROPKEY share this layout ({GUID, ULONG}).
#[repr(C)]
struct PropertyKey {
    fmtid: GUID,
    pid: u32,
}

const fn key(d1: u32, d2: u16, d3: u16, d4: [u8; 8], pid: u32) -> PropertyKey {
    PropertyKey {
        fmtid: GUID { d1, d2, d3, d4 },
        pid,
    }
}

const DEVICE_FMTID: (u32, u16, u16, [u8; 8]) = (
    0xA45C254E,
    0xDF1C,
    0x4EFD,
    [0x80, 0x20, 0x67, 0xD1, 0x46, 0xA8, 0x50, 0xE0],
);
const fn device_key(pid: u32) -> PropertyKey {
    key(DEVICE_FMTID.0, DEVICE_FMTID.1, DEVICE_FMTID.2, DEVICE_FMTID.3, pid)
}

// Endpoint property store (PROPERTYKEY).
const PKEY_DEVICE_DEVICE_DESC: PropertyKey = device_key(2);
const PKEY_DEVICE_FRIENDLY_NAME: PropertyKey = device_key(14);
const PKEY_DEVICE_ENUMERATOR_NAME: PropertyKey = device_key(24);
const PKEY_DEVICE_INTERFACE_FRIENDLY_NAME: PropertyKey = key(
    0x026E516E,
    0xB814,
    0x414B,
    [0x83, 0xCD, 0x85, 0x6D, 0x6F, 0xEF, 0x48, 0x22],
    2,
);
const PKEY_AUDIO_ENDPOINT_FORM_FACTOR: PropertyKey = key(
    0x1DA5D803,
    0xD492,
    0x4EDD,
    [0x8C, 0x23, 0xE0, 0xC0, 0xFF, 0xEE, 0x7F, 0x0E],
    0,
);
/// AudioHub's private endpoint identity (perpeer.cpp AH_EP_PEERKEY_W).
const PKEY_AUDIOHUB_PEER_KEY: PropertyKey = key(
    0x8CA48324,
    0x7D8A,
    0x4EFA,
    [0x8D, 0xD4, 0x7B, 0x75, 0x03, 0xAF, 0x96, 0x4B],
    2,
);
/// Undocumented endpoint property that commonly holds `{1}.<adapter instance
/// id>` (or the KS interface path). Fallback only, when the documented
/// topology walk fails; the result is still subject to the whole gate.
const PKEY_ENDPOINT_ADAPTER_FALLBACK: PropertyKey = key(
    0xB3F8FA53,
    0x0004,
    0x438E,
    [0x90, 0x03, 0x51, 0xA4, 0x6E, 0x13, 0x9B, 0xFC],
    2,
);

// Devnode properties (DEVPROPKEY).
const DEVPKEY_DEVICE_INSTANCE_ID: PropertyKey = key(
    0x78C34FC8,
    0x104A,
    0x4ACA,
    [0x9E, 0xA4, 0x52, 0x4D, 0x52, 0x99, 0x6E, 0x57],
    256,
);
const DEVPKEY_DEVICE_HARDWARE_IDS: PropertyKey = device_key(3);
const DEVPKEY_DEVICE_SERVICE: PropertyKey = device_key(6);
const DEVPKEY_DEVICE_MANUFACTURER: PropertyKey = device_key(13);
const DEVPKEY_DEVICE_ENUMERATOR_NAME: PropertyKey = device_key(24);
const DEVPKEY_DEVICE_CONTAINER_ID: PropertyKey = key(
    0x8C7ED206,
    0x3F8A,
    0x4827,
    [0xB3, 0xAB, 0xAE, 0x9E, 0x1F, 0xAE, 0xFC, 0x6C],
    2,
);

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
const IID_IDEVICE_TOPOLOGY: GUID = GUID {
    d1: 0x2A07407E,
    d2: 0x6497,
    d3: 0x4A18,
    d4: [0x97, 0x87, 0x32, 0xF7, 0x9B, 0xD0, 0xD9, 0x8F],
};

const CLSCTX_INPROC_SERVER: u32 = 0x1;
const CLSCTX_ALL: u32 = 0x17;
const COINIT_MULTITHREADED: u32 = 0x0;
const E_RENDER: u32 = 0;
const E_CAPTURE: u32 = 1;
const E_CONSOLE: u32 = 0;
const STGM_READ: u32 = 0x0;
/// HRESULT_FROM_WIN32(ERROR_NOT_FOUND): no default endpoint in this flow.
const E_NOTFOUND: HRESULT = 0x8007_0490u32 as i32;

const VT_LPWSTR: u16 = 31;
const VT_UI4: u16 = 19;

const CR_SUCCESS: CONFIGRET = 0;
const CR_BUFFER_SMALL: CONFIGRET = 0x1A;
const CM_LOCATE_DEVNODE_NORMAL: u32 = 0;
const DEVPROP_TYPE_GUID: DEVPROPTYPE = 0x0D;
const DEVPROP_TYPE_STRING: DEVPROPTYPE = 0x12;
const DEVPROP_TYPE_STRING_LIST: DEVPROPTYPE = 0x2012;
/// Device trees are shallow (a USB headset sits about six levels deep).
const MAX_ANCESTORS: usize = 32;
const MAX_PROPERTY_BYTES: u32 = 64 * 1024;

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
        PropVariant {
            vt: 0,
            r1: 0,
            r2: 0,
            r3: 0,
            val: [0; 2],
        }
    }
}

impl Drop for PropVariant {
    fn drop(&mut self) {
        unsafe { PropVariantClear(self) };
    }
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

#[link(name = "cfgmgr32")]
extern "system" {
    fn CM_Locate_DevNodeW(dn: *mut DEVINST, id: *const u16, flags: u32) -> CONFIGRET;
    fn CM_Get_Parent(parent: *mut DEVINST, dn: DEVINST, flags: u32) -> CONFIGRET;
    fn CM_Get_DevNode_PropertyW(
        dn: DEVINST,
        key: *const PropertyKey,
        ty: *mut DEVPROPTYPE,
        buf: *mut u8,
        size: *mut u32,
        flags: u32,
    ) -> CONFIGRET;
    fn CM_Get_Device_Interface_PropertyW(
        iface: *const u16,
        key: *const PropertyKey,
        ty: *mut DEVPROPTYPE,
        buf: *mut u8,
        size: *mut u32,
        flags: u32,
    ) -> CONFIGRET;
}

#[repr(C)]
#[allow(dead_code)]
struct IUnknownVtbl {
    query_interface: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
}

#[repr(C)]
#[allow(dead_code)]
struct IMMDeviceEnumeratorVtbl {
    base: IUnknownVtbl,
    enum_audio_endpoints: usize,
    get_default_audio_endpoint:
        unsafe extern "system" fn(*mut c_void, u32, u32, *mut *mut c_void) -> HRESULT,
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
        *const GUID,
        u32,
        *mut c_void,
        *mut *mut c_void,
    ) -> HRESULT,
    open_property_store: unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> HRESULT,
    get_id: unsafe extern "system" fn(*mut c_void, *mut *mut u16) -> HRESULT,
    get_state: usize,
}

#[repr(C)]
#[allow(dead_code)]
struct IPropertyStoreVtbl {
    base: IUnknownVtbl,
    get_count: usize,
    get_at: usize,
    get_value: unsafe extern "system" fn(*mut c_void, *const PropertyKey, *mut PropVariant) -> HRESULT,
    set_value: usize,
    commit: usize,
}

/// devicetopology.h IDeviceTopology.
#[repr(C)]
#[allow(dead_code)]
struct IDeviceTopologyVtbl {
    base: IUnknownVtbl,
    get_connector_count: usize,
    get_connector: unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> HRESULT,
    get_subunit_count: usize,
    get_subunit: usize,
    get_part_by_id: usize,
    get_device_id: usize,
    get_signal_path: usize,
}

/// devicetopology.h IConnector. `connect_to` / `disconnect` stay uncallable.
#[repr(C)]
#[allow(dead_code)]
struct IConnectorVtbl {
    base: IUnknownVtbl,
    get_type: usize,
    get_data_flow: usize,
    connect_to: usize,
    disconnect: usize,
    is_connected: usize,
    get_connected_to: usize,
    get_connector_id_connected_to: usize,
    get_device_id_connected_to: unsafe extern "system" fn(*mut c_void, *mut *mut u16) -> HRESULT,
}

struct Apartment {
    owned: bool,
}

impl Apartment {
    fn enter() -> Apartment {
        // RPC_E_CHANGED_MODE leaves the caller's apartment in place; COM still
        // works, it just is not ours to uninitialize.
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

    /// Safety: the caller must name the interface this pointer really is.
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

fn hr_text(what: &str, hr: HRESULT) -> String {
    format!("{what} failed: HRESULT 0x{:08X}", hr as u32)
}

fn cr_text(what: &str, cr: CONFIGRET) -> String {
    format!("{what} failed: CONFIGRET 0x{cr:X}")
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

/// Takes ownership of a CoTaskMem-allocated wide string.
unsafe fn take_co_string(p: *mut u16) -> Option<String> {
    if p.is_null() {
        return None;
    }
    let s = wide_string(p);
    CoTaskMemFree(p as *mut c_void);
    s
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Two-call cfgmgr32 property read (size query, then data).
fn cm_property(
    read: impl Fn(*mut DEVPROPTYPE, *mut u8, *mut u32) -> CONFIGRET,
) -> Option<(DEVPROPTYPE, Vec<u8>)> {
    let mut ty: DEVPROPTYPE = 0;
    let mut size: u32 = 0;
    let cr = read(&mut ty, ptr::null_mut(), &mut size);
    if cr != CR_BUFFER_SMALL || size == 0 || size > MAX_PROPERTY_BYTES {
        return None;
    }
    let mut buf = vec![0u8; size as usize];
    let cr = read(&mut ty, buf.as_mut_ptr(), &mut size);
    if cr != CR_SUCCESS {
        return None;
    }
    buf.truncate(size as usize);
    Some((ty, buf))
}

fn devnode_property(dn: DEVINST, key: &PropertyKey) -> Option<(DEVPROPTYPE, Vec<u8>)> {
    cm_property(|ty, buf, size| unsafe { CM_Get_DevNode_PropertyW(dn, key, ty, buf, size, 0) })
}

fn devnode_string(dn: DEVINST, key: &PropertyKey) -> Option<String> {
    match devnode_property(dn, key)? {
        (DEVPROP_TYPE_STRING, bytes) => string_from_bytes(&bytes),
        _ => None,
    }
}

fn devnode_string_list(dn: DEVINST, key: &PropertyKey) -> Vec<String> {
    match devnode_property(dn, key) {
        Some((DEVPROP_TYPE_STRING_LIST, bytes)) => string_list_from_bytes(&bytes),
        _ => Vec::new(),
    }
}

fn devnode_guid(dn: DEVINST, key: &PropertyKey) -> Option<String> {
    match devnode_property(dn, key)? {
        (DEVPROP_TYPE_GUID, bytes) => guid_from_bytes(&bytes),
        _ => None,
    }
}

/// `None` when the instance ID itself is unreadable: without it a node cannot
/// be identified, and the classifier treats a short chain as incomplete.
fn read_devnode(dn: DEVINST) -> Option<WindowsDevnode> {
    Some(WindowsDevnode {
        instance_id: devnode_string(dn, &DEVPKEY_DEVICE_INSTANCE_ID)?,
        enumerator: devnode_string(dn, &DEVPKEY_DEVICE_ENUMERATOR_NAME),
        hardware_ids: devnode_string_list(dn, &DEVPKEY_DEVICE_HARDWARE_IDS),
        service: devnode_string(dn, &DEVPKEY_DEVICE_SERVICE),
        manufacturer: devnode_string(dn, &DEVPKEY_DEVICE_MANUFACTURER),
        container_id: devnode_guid(dn, &DEVPKEY_DEVICE_CONTAINER_ID),
    })
}

fn locate_devnode(instance_id: &str) -> Result<DEVINST, String> {
    let wide = to_wide(instance_id);
    let mut dn: DEVINST = 0;
    let cr = unsafe { CM_Locate_DevNodeW(&mut dn, wide.as_ptr(), CM_LOCATE_DEVNODE_NORMAL) };
    if cr == CR_SUCCESS {
        Ok(dn)
    } else {
        Err(cr_text("CM_Locate_DevNodeW", cr))
    }
}

fn interface_instance_id(path: &str) -> Option<String> {
    let wide = to_wide(path);
    let from_cm = cm_property(|ty, buf, size| unsafe {
        CM_Get_Device_Interface_PropertyW(wide.as_ptr(), &DEVPKEY_DEVICE_INSTANCE_ID, ty, buf, size, 0)
    })
    .and_then(|(ty, bytes)| (ty == DEVPROP_TYPE_STRING).then(|| string_from_bytes(&bytes)).flatten());
    from_cm.or_else(|| instance_id_from_interface_path(path))
}

/// `{1}.HDAUDIO\...` -> `HDAUDIO\...`; an interface path is resolved as one.
fn instance_id_from_fallback(value: &str) -> Option<String> {
    if value.starts_with(r"\\?\") {
        return interface_instance_id(value);
    }
    instance_id_from_numbered_prefix(value)
}

fn property_store(device: &ComPtr) -> Option<ComPtr> {
    let mut store = ComPtr::null();
    let hr = unsafe {
        let v = device.vtbl::<IMMDeviceVtbl>();
        ((*v).open_property_store)(device.0, STGM_READ, &mut store.0)
    };
    (hr >= 0 && !store.0.is_null()).then_some(store)
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
    unsafe { wide_string(pv.val[0] as *const u16) }.filter(|s| !s.is_empty())
}

fn prop_u32(store: &ComPtr, key: &PropertyKey) -> Option<u32> {
    let pv = prop(store, key)?;
    (pv.vt == VT_UI4).then(|| pv.val[0] as u32)
}

/// The KS filter interface path the endpoint's topology connects to.
fn connected_filter_path(device: &ComPtr) -> Result<String, String> {
    let mut topology = ComPtr::null();
    let hr = unsafe {
        let v = device.vtbl::<IMMDeviceVtbl>();
        ((*v).activate)(device.0, &IID_IDEVICE_TOPOLOGY, CLSCTX_ALL, ptr::null_mut(), &mut topology.0)
    };
    if hr < 0 || topology.0.is_null() {
        return Err(hr_text("IMMDevice::Activate(IDeviceTopology)", hr));
    }
    let mut connector = ComPtr::null();
    let hr = unsafe {
        let v = topology.vtbl::<IDeviceTopologyVtbl>();
        ((*v).get_connector)(topology.0, 0, &mut connector.0)
    };
    if hr < 0 || connector.0.is_null() {
        return Err(hr_text("IDeviceTopology::GetConnector(0)", hr));
    }
    let mut id: *mut u16 = ptr::null_mut();
    let hr = unsafe {
        let v = connector.vtbl::<IConnectorVtbl>();
        ((*v).get_device_id_connected_to)(connector.0, &mut id)
    };
    let path = unsafe { take_co_string(id) };
    if hr < 0 {
        return Err(hr_text("IConnector::GetDeviceIdConnectedTo", hr));
    }
    path.filter(|p| !p.is_empty())
        .ok_or_else(|| "IConnector::GetDeviceIdConnectedTo returned no id".to_string())
}

/// Adapter instance ID via the documented topology walk, else the fallback
/// endpoint property.
fn adapter_instance_id(device: &ComPtr, store: Option<&ComPtr>) -> Result<String, String> {
    let primary = connected_filter_path(device).and_then(|path| {
        interface_instance_id(&path).ok_or_else(|| "KS filter interface has no instance id".to_string())
    });
    match primary {
        Ok(id) => Ok(id),
        Err(e) => store
            .and_then(|s| prop_string(s, &PKEY_ENDPOINT_ADAPTER_FALLBACK))
            .and_then(|v| instance_id_from_fallback(&v))
            .ok_or(e),
    }
}

/// Adapter devnode and its ancestors, parent first, stopping after the first
/// chain-terminal node. A read failure ends the list early; the classifier
/// then sees an unterminated chain and fails closed.
fn adapter_and_ancestors(
    instance_id: &str,
) -> Result<(WindowsDevnode, Vec<WindowsDevnode>, Option<String>), String> {
    let dn = locate_devnode(instance_id)?;
    let adapter = read_devnode(dn).ok_or_else(|| "adapter devnode unreadable".to_string())?;
    let mut ancestors = Vec::new();
    let mut cur = dn;
    let mut note = None;
    for _ in 0..MAX_ANCESTORS {
        let mut parent: DEVINST = 0;
        let cr = unsafe { CM_Get_Parent(&mut parent, cur, 0) };
        if cr != CR_SUCCESS {
            note = Some(cr_text("CM_Get_Parent", cr));
            break;
        }
        let Some(node) = read_devnode(parent) else {
            note = Some("ancestor devnode unreadable".to_string());
            break;
        };
        let terminal = windows_chain_terminal(&node);
        ancestors.push(node);
        if terminal {
            return Ok((adapter, ancestors, None));
        }
        cur = parent;
    }
    Ok((adapter, ancestors, note.or_else(|| Some("ancestor walk exceeded depth bound".into()))))
}

fn snapshot(device: &ComPtr, direction: Direction) -> WindowsEndpointSnapshot {
    let endpoint_id = unsafe {
        let mut id: *mut u16 = ptr::null_mut();
        let v = device.vtbl::<IMMDeviceVtbl>();
        let hr = ((*v).get_id)(device.0, &mut id);
        let s = take_co_string(id);
        if hr >= 0 { s } else { None }
    };
    let store = property_store(device);
    let read = |k: &PropertyKey| store.as_ref().and_then(|s| prop_string(s, k));
    let mut snap = WindowsEndpointSnapshot {
        direction,
        endpoint_id,
        friendly_name: read(&PKEY_DEVICE_FRIENDLY_NAME),
        device_desc: read(&PKEY_DEVICE_DEVICE_DESC),
        interface_friendly_name: read(&PKEY_DEVICE_INTERFACE_FRIENDLY_NAME),
        form_factor: store.as_ref().and_then(|s| prop_u32(s, &PKEY_AUDIO_ENDPOINT_FORM_FACTOR)),
        endpoint_enumerator: read(&PKEY_DEVICE_ENUMERATOR_NAME),
        audiohub_peer_key: read(&PKEY_AUDIOHUB_PEER_KEY),
        adapter: None,
        ancestors: Vec::new(),
        error: store.is_none().then(|| "endpoint property store unreadable".to_string()),
    };
    match adapter_instance_id(device, store.as_ref()).and_then(|id| adapter_and_ancestors(&id)) {
        Ok((adapter, ancestors, note)) => {
            snap.adapter = Some(adapter);
            snap.ancestors = ancestors;
            if note.is_some() {
                snap.error = note;
            }
        }
        Err(e) => snap.error = Some(e),
    }
    snap
}

fn probe_direction(enumerator: &ComPtr, direction: Direction) -> EndpointProbe<WindowsEndpointSnapshot> {
    let flow = match direction {
        Direction::Input => E_CAPTURE,
        Direction::Output => E_RENDER,
    };
    let mut device = ComPtr::null();
    let hr = unsafe {
        let v = enumerator.vtbl::<IMMDeviceEnumeratorVtbl>();
        ((*v).get_default_audio_endpoint)(enumerator.0, flow, E_CONSOLE, &mut device.0)
    };
    if hr == E_NOTFOUND {
        return EndpointProbe::Absent;
    }
    if hr < 0 || device.0.is_null() {
        return EndpointProbe::Failed {
            error: hr_text("GetDefaultAudioEndpoint", hr),
        };
    }
    EndpointProbe::Present {
        snapshot: snapshot(&device, direction),
    }
}

pub(super) fn probe_raw() -> RawFactsSnapshot {
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
    if hr < 0 || enumerator.0.is_null() {
        let error = hr_text("CoCreateInstance(MMDeviceEnumerator)", hr);
        return RawFactsSnapshot::Windows {
            input: EndpointProbe::Failed { error: error.clone() },
            output: EndpointProbe::Failed { error },
        };
    }
    RawFactsSnapshot::Windows {
        input: probe_direction(&enumerator, Direction::Input),
        output: probe_direction(&enumerator, Direction::Output),
    }
}
