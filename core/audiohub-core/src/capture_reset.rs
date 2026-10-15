//! Disable and re-enable the default capture endpoint.
//!
//! A wireless receiver can stay listed and still refuse `IAudioClient::Initialize`
//! with `0x800705AA`, or leave that call blocked. The recovery that works is the
//! same one a person does in Sound settings: disable that endpoint and enable it
//! again. This is Windows-only and never persists the disabled state.

use anyhow::{anyhow, Result};

/// `SWD\MMDEVAPI\` + the id `IMMDevice::GetId` returns.
pub(crate) fn endpoint_instance_id(mmdevice_id: &str) -> String {
    format!(r"SWD\MMDEVAPI\{mmdevice_id}")
}

/// Disable then enable the current default capture endpoint.
///
/// The disabled bit is not persisted. If disable fails, enable is not called,
/// so an endpoint the user already turned off stays off.
#[cfg(windows)]
pub fn cycle_default_capture_endpoint() -> Result<()> {
    windows::cycle()
}

#[cfg(not(windows))]
pub fn cycle_default_capture_endpoint() -> Result<()> {
    Err(anyhow!(
        "cycling the default capture endpoint is only implemented on Windows"
    ))
}

#[cfg(windows)]
mod windows {
    use super::endpoint_instance_id;
    use anyhow::{anyhow, Result};
    use std::ffi::c_void;
    use std::ptr;
    use std::thread;
    use std::time::Duration;

    type HRESULT = i32;

    const CLSCTX_INPROC_SERVER: u32 = 0x1;
    const COINIT_MULTITHREADED: u32 = 0x0;
    const E_CAPTURE: u32 = 1;
    const E_CONSOLE: u32 = 0;
    /// Fail instead of showing UI. Do not persist across reboot.
    const CM_DISABLE_UI_NOT_OK: u32 = 0x4;
    const CR_SUCCESS: u32 = 0;

    #[repr(C)]
    #[derive(Clone, Copy, PartialEq)]
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
        fn CoTaskMemFree(ptr: *mut c_void);
    }

    #[link(name = "cfgmgr32")]
    extern "system" {
        fn CM_Locate_DevNodeW(dev: *mut u32, id: *const u16, flags: u32) -> u32;
        fn CM_Disable_DevNode(dev: u32, flags: u32) -> u32;
        fn CM_Enable_DevNode(dev: u32, flags: u32) -> u32;
    }

    #[repr(C)]
    struct IUnknownVtbl {
        query_interface: usize,
        add_ref: usize,
        release: unsafe extern "system" fn(*mut c_void) -> u32,
    }

    #[repr(C)]
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
    struct IMMDeviceVtbl {
        base: IUnknownVtbl,
        activate: usize,
        open_property_store: usize,
        get_id: unsafe extern "system" fn(*mut c_void, *mut *mut u16) -> HRESULT,
        get_state: usize,
    }

    struct ComPtr(*mut c_void);

    impl Drop for ComPtr {
        fn drop(&mut self) {
            if self.0.is_null() {
                return;
            }
            unsafe {
                let vtbl = *(self.0 as *const *const IUnknownVtbl);
                ((*vtbl).release)(self.0);
            }
        }
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

    struct TaskStr(*mut u16);

    impl Drop for TaskStr {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { CoTaskMemFree(self.0 as *mut c_void) };
            }
        }
    }

    impl TaskStr {
        fn as_str(&self) -> Result<String> {
            if self.0.is_null() {
                return Err(anyhow!("GetId returned a null endpoint id"));
            }
            let mut n = 0usize;
            unsafe {
                while *self.0.add(n) != 0 {
                    n += 1;
                    if n > 1024 {
                        return Err(anyhow!("endpoint id is unbounded"));
                    }
                }
                Ok(String::from_utf16_lossy(std::slice::from_raw_parts(
                    self.0, n,
                )))
            }
        }
    }

    pub fn cycle() -> Result<()> {
        let _apt = Apartment::enter();
        let mut enumerator = ComPtr(ptr::null_mut());
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
            return Err(anyhow!(
                "CoCreateInstance(MMDeviceEnumerator) failed: HRESULT 0x{:08X}",
                hr as u32
            ));
        }
        let mut device = ComPtr(ptr::null_mut());
        let hr = unsafe {
            let v = *(enumerator.0 as *const *const IMMDeviceEnumeratorVtbl);
            ((*v).get_default_audio_endpoint)(enumerator.0, E_CAPTURE, E_CONSOLE, &mut device.0)
        };
        if hr < 0 || device.0.is_null() {
            return Err(anyhow!(
                "GetDefaultAudioEndpoint(capture) failed: HRESULT 0x{:08X}",
                hr as u32
            ));
        }
        let mut id = TaskStr(ptr::null_mut());
        let hr = unsafe {
            let v = *(device.0 as *const *const IMMDeviceVtbl);
            ((*v).get_id)(device.0, &mut id.0)
        };
        if hr < 0 {
            return Err(anyhow!(
                "IMMDevice::GetId failed: HRESULT 0x{:08X}",
                hr as u32
            ));
        }
        let instance = endpoint_instance_id(&id.as_str()?);
        let wide: Vec<u16> = instance.encode_utf16().chain(std::iter::once(0)).collect();
        let mut dev = 0u32;
        let cr = unsafe { CM_Locate_DevNodeW(&mut dev, wide.as_ptr(), 0) };
        if cr != CR_SUCCESS {
            return Err(anyhow!(
                "CM_Locate_DevNodeW({instance}) failed: CONFIGRET {cr}"
            ));
        }
        let cr = unsafe { CM_Disable_DevNode(dev, CM_DISABLE_UI_NOT_OK) };
        if cr != CR_SUCCESS {
            return Err(anyhow!(
                "CM_Disable_DevNode({instance}) failed: CONFIGRET {cr}"
            ));
        }
        // Let MMDevAPI publish the disabled state before the matching enable.
        thread::sleep(Duration::from_millis(300));
        let cr = unsafe { CM_Enable_DevNode(dev, 0) };
        if cr != CR_SUCCESS {
            return Err(anyhow!(
                "CM_Enable_DevNode({instance}) failed: CONFIGRET {cr}"
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::endpoint_instance_id;

    #[test]
    fn instance_id_uses_the_mmdevapi_software_device() {
        assert_eq!(
            endpoint_instance_id("{0.0.1.00000000}.{AF12590B-84D7-4C0A-86B7-11B47EF6DAE3}"),
            r"SWD\MMDEVAPI\{0.0.1.00000000}.{AF12590B-84D7-4C0A-86B7-11B47EF6DAE3}"
        );
    }
}
