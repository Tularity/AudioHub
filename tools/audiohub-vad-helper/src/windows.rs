use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::mem::{size_of, zeroed};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};
use std::ptr::{null, null_mut};
use std::thread;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use windows_sys::core::GUID;
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_DevNode_Status, SetupDiCallClassInstaller, SetupDiCreateDevRegKeyW,
    SetupDiCreateDeviceInfoList, SetupDiCreateDeviceInfoW, SetupDiDestroyDeviceInfoList,
    SetupDiEnumDeviceInfo, SetupDiGetClassDevsW, SetupDiGetDeviceInstallParamsW,
    SetupDiGetDeviceRegistryPropertyW, SetupDiGetINFClassW, SetupDiOpenDevRegKey,
    SetupDiSetDeviceRegistryPropertyW, SetupUninstallOEMInfW, UpdateDriverForPlugAndPlayDevicesW,
    CR_SUCCESS, DICD_GENERATE_ID, DICS_FLAG_GLOBAL, DIF_REGISTERDEVICE, DIF_REMOVE,
    DIGCF_ALLCLASSES, DIREG_DEV, DI_NEEDREBOOT, DI_NEEDRESTART, DN_HAS_PROBLEM, DN_STARTED,
    HDEVINFO, INSTALLFLAG_NONINTERACTIVE, SPDRP_HARDWAREID, SPDRP_SERVICE, SP_DEVINFO_DATA,
    SP_DEVINSTALL_PARAMS_W,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_FILE_NOT_FOUND, ERROR_INF_IN_USE_BY_DEVICES,
    ERROR_INSUFFICIENT_BUFFER, ERROR_INVALID_DATA, ERROR_NO_MORE_ITEMS,
    ERROR_SERVICE_DOES_NOT_EXIST, ERROR_SERVICE_MARKED_FOR_DELETE, FILETIME, HANDLE,
    INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, TokenElevation, SC_HANDLE, TOKEN_ELEVATION, TOKEN_QUERY,
};
use windows_sys::Win32::Storage::FileSystem::QueryDosDeviceW;
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegQueryValueExW, RegSetValueExW, HKEY, KEY_QUERY_VALUE, REG_EXPAND_SZ, REG_SZ,
};
use windows_sys::Win32::System::Services::{
    CloseServiceHandle, ControlService, DeleteService, OpenSCManagerW, OpenServiceW,
    SC_MANAGER_CONNECT, SERVICE_CONTROL_STOP, SERVICE_QUERY_STATUS, SERVICE_STATUS, SERVICE_STOP,
};
use windows_sys::Win32::System::SystemInformation::{
    GetSystemTimeAsFileTime, GetTickCount64, GetWindowsDirectoryW,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use crate::{DriverReport, DriverState};

const HARDWARE_ID: &str = r"ROOT\AudioHubVad";
const SERVICE_NAME: &str = "AudioHubVad";
const CONTROL_DOS_NAME: &str = "AudioHubVadCtl";
const DAEMON_VALUE: &str = "AudioHubDaemonImage";
const HELPER_NAME: &str = "audiohub-vad-helper.exe";
const RESOURCE_DIR_NAME: &str = "windows-driver";
const INF_NAME: &str = "AudioHubVad.inf";
const SYS_NAME: &str = "AudioHubVad.sys";
const CAT_NAME: &str = "AudioHubVad.cat";
const DAEMON_NAME: &str = "audiohubd.exe";
const REBOOT_MARKER_NAME: &str = "reboot-required";
const FILE_SHARE_READ_ONLY: u32 = 0x0000_0001;
const DELETE_ACCESS: u32 = 0x0001_0000;
const EXPECTED_INF_SHA256: &str = env!("AUDIOHUB_EXPECTED_INF_SHA256");
const EXPECTED_SYS_SHA256: &str = env!("AUDIOHUB_EXPECTED_SYS_SHA256");
const EXPECTED_CAT_SHA256: &str = env!("AUDIOHUB_EXPECTED_CAT_SHA256");
const EXPECTED_DAEMON_SHA256: &str = env!("AUDIOHUB_EXPECTED_DAEMON_SHA256");

const MEDIA_CLASS_GUID: GUID = GUID::from_u128(0x4d36e96c_e325_11ce_bfc1_08002be10318);

#[derive(Debug)]
struct WinFailure {
    code: Option<u32>,
    detail: String,
}

impl WinFailure {
    fn api(operation: &str) -> Self {
        Self {
            code: Some(unsafe { GetLastError() }),
            detail: format!("{operation} failed"),
        }
    }

    fn message(detail: impl Into<String>) -> Self {
        Self {
            code: None,
            detail: detail.into(),
        }
    }
}

type WinResult<T> = Result<T, WinFailure>;

struct DeviceInfoSet(HDEVINFO);

impl DeviceInfoSet {
    fn new(raw: HDEVINFO, operation: &str) -> WinResult<Self> {
        if raw == INVALID_HANDLE_VALUE {
            Err(WinFailure::api(operation))
        } else {
            Ok(Self(raw))
        }
    }
}

impl Drop for DeviceInfoSet {
    fn drop(&mut self) {
        if self.0 != INVALID_HANDLE_VALUE {
            unsafe { SetupDiDestroyDeviceInfoList(self.0) };
        }
    }
}

struct RegistryKey(HKEY);

impl RegistryKey {
    fn new(raw: HKEY, operation: &str) -> WinResult<Self> {
        if raw == INVALID_HANDLE_VALUE {
            Err(WinFailure::api(operation))
        } else {
            Ok(Self(raw))
        }
    }
}

impl Drop for RegistryKey {
    fn drop(&mut self) {
        if self.0 != INVALID_HANDLE_VALUE && self.0 != 0 {
            unsafe { RegCloseKey(self.0) };
        }
    }
}

struct KernelHandle(HANDLE);

impl Drop for KernelHandle {
    fn drop(&mut self) {
        if self.0 != 0 && self.0 != INVALID_HANDLE_VALUE {
            unsafe { CloseHandle(self.0) };
        }
    }
}

struct ServiceHandle(SC_HANDLE);

impl Drop for ServiceHandle {
    fn drop(&mut self) {
        if self.0 != 0 {
            unsafe { CloseServiceHandle(self.0) };
        }
    }
}

#[derive(Debug)]
struct Payload {
    inf: PathBuf,
    daemon: PathBuf,
    // The unelevated App already holds matching read locks during UAC. Keep an
    // elevated set as well so a direct helper invocation cannot hash one file
    // and then let SetupAPI consume a replacement.
    _locks: Vec<File>,
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn lock_and_verify(path: &Path, expected: &str) -> WinResult<File> {
    if !valid_digest(expected) {
        return Err(WinFailure::message(
            "this helper was built without a trusted driver payload manifest",
        ));
    }
    let mut file = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ_ONLY)
        .open(path)
        .map_err(|error| {
            WinFailure::message(format!(
                "cannot lock driver payload {}: {error}",
                path.display()
            ))
        })?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|error| {
            WinFailure::message(format!(
                "cannot hash driver payload {}: {error}",
                path.display()
            ))
        })?;
        if read == 0 {
            break;
        }
        hash.update(&buffer[..read]);
    }
    file.seek(SeekFrom::Start(0)).map_err(|error| {
        WinFailure::message(format!(
            "cannot rewind driver payload {}: {error}",
            path.display()
        ))
    })?;
    if format!("{:x}", hash.finalize()) != expected {
        return Err(WinFailure::message(format!(
            "driver payload does not match this helper build: {}",
            path.display()
        )));
    }
    Ok(file)
}

#[derive(Debug, Default)]
struct Snapshot {
    matching_devices: u32,
    present_devices: u32,
    started_devices: u32,
    installed_devices: u32,
    configured_devices: u32,
    published_packages: u32,
    service_present: bool,
    reboot_pending: bool,
    control_link_present: bool,
    problem_codes: Vec<u32>,
}

impl Snapshot {
    fn package_installed(&self) -> bool {
        self.installed_devices != 0 || self.published_packages != 0 || self.service_present
    }

    fn available(&self) -> bool {
        self.installed_devices != 0 && self.started_devices != 0 && self.control_link_present
    }

    fn daemon_image_configured(&self) -> bool {
        self.installed_devices != 0 && self.configured_devices == self.installed_devices
    }

    fn state(&self) -> DriverState {
        if self.reboot_pending {
            DriverState::RebootRequired
        } else if self.available() {
            DriverState::Ready
        } else if self.package_installed() || self.matching_devices != 0 {
            DriverState::InstalledUnavailable
        } else {
            DriverState::Absent
        }
    }
}

struct CreatedDevice {
    set: DeviceInfoSet,
    data: SP_DEVINFO_DATA,
    registered: bool,
}

impl CreatedDevice {
    fn remove(&mut self) {
        if self.registered {
            unsafe {
                SetupDiCallClassInstaller(DIF_REMOVE, self.set.0, &self.data);
            }
            self.registered = false;
        }
    }
}

fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}

fn wide_str(value: &str) -> Vec<u16> {
    wide(OsStr::new(value))
}

fn guid_eq(a: &GUID, b: &GUID) -> bool {
    a.data1 == b.data1 && a.data2 == b.data2 && a.data3 == b.data3 && a.data4 == b.data4
}

fn file_name_is(path: &Path, expected: &str) -> bool {
    path.file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| name.eq_ignore_ascii_case(expected))
}

fn standard_dos_path(path: &Path) -> WinResult<PathBuf> {
    let text = path.as_os_str().to_string_lossy();
    let stripped = text.strip_prefix(r"\\?\").unwrap_or(&text);
    let result = PathBuf::from(stripped);
    let mut components = result.components();
    if !matches!(components.next(), Some(Component::Prefix(_)))
        || !matches!(components.next(), Some(Component::RootDir))
    {
        return Err(WinFailure::message(format!(
            "the daemon path is not an absolute DOS path: {}",
            result.display()
        )));
    }
    Ok(result)
}

fn derived_daemon_path(require_present: bool) -> WinResult<PathBuf> {
    let exe = std::env::current_exe()
        .map_err(|e| WinFailure::message(format!("cannot resolve helper executable: {e}")))?;
    if !file_name_is(&exe, HELPER_NAME) {
        return Err(WinFailure::message(format!(
            "unexpected helper filename: {}",
            exe.display()
        )));
    }
    let helper_dir = exe
        .parent()
        .ok_or_else(|| WinFailure::message("the helper executable has no parent directory"))?;
    if !file_name_is(helper_dir, RESOURCE_DIR_NAME) {
        return Err(WinFailure::message(format!(
            "the helper must be inside the fixed {RESOURCE_DIR_NAME} resource directory"
        )));
    }
    // Tauri places Windows resources beside the application executable (the
    // PathResolver resource_dir is the executable directory on Windows), so
    // windows-driver's parent is the per-machine installation directory.
    let install_dir = helper_dir
        .parent()
        .ok_or_else(|| WinFailure::message("the helper resource directory has no parent"))?;
    let daemon = install_dir.join(DAEMON_NAME);
    if require_present && !daemon.is_file() {
        return Err(WinFailure::message(format!(
            "bundled daemon is missing: {}",
            daemon.display()
        )));
    }
    if require_present {
        standard_dos_path(
            &daemon
                .canonicalize()
                .map_err(|e| WinFailure::message(format!("cannot resolve daemon path: {e}")))?,
        )
    } else {
        standard_dos_path(&daemon)
    }
}

fn helper_directory() -> WinResult<PathBuf> {
    let exe = std::env::current_exe().map_err(|error| {
        WinFailure::message(format!("cannot resolve helper executable: {error}"))
    })?;
    if !file_name_is(&exe, HELPER_NAME) {
        return Err(WinFailure::message(format!(
            "unexpected helper filename: {}",
            exe.display()
        )));
    }
    let directory = exe
        .parent()
        .ok_or_else(|| WinFailure::message("the helper executable has no parent directory"))?;
    if !file_name_is(directory, RESOURCE_DIR_NAME) {
        return Err(WinFailure::message(format!(
            "the helper must be inside the fixed {RESOURCE_DIR_NAME} resource directory"
        )));
    }
    Ok(directory.to_path_buf())
}

fn reboot_marker() -> WinResult<PathBuf> {
    Ok(helper_directory()?.join(REBOOT_MARKER_NAME))
}

fn current_boot_epoch_ms() -> u64 {
    let mut time: FILETIME = unsafe { zeroed() };
    unsafe { GetSystemTimeAsFileTime(&mut time) };
    let wall_100ns = (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
    wall_100ns / 10_000 - unsafe { GetTickCount64() }
}

fn marker_matches_boot(recorded_boot_epoch_ms: u64, current_boot_epoch_ms: u64) -> bool {
    recorded_boot_epoch_ms.abs_diff(current_boot_epoch_ms) < 120_000
}

fn reboot_pending() -> bool {
    let Ok(path) = reboot_marker() else {
        return false;
    };
    let Ok(text) = fs::read_to_string(path) else {
        return false;
    };
    let Ok(recorded) = text.trim().parse::<u64>() else {
        return false;
    };
    // Wall-clock adjustments can slightly move the derived boot epoch. A real
    // reboot changes it by at least the previous boot's uptime, which is much
    // larger than this tolerance in any normal driver-install flow.
    marker_matches_boot(recorded, current_boot_epoch_ms())
}

fn set_reboot_marker(required: bool) -> WinResult<()> {
    let path = reboot_marker()?;
    if required {
        fs::write(&path, format!("{}\n", current_boot_epoch_ms())).map_err(|error| {
            WinFailure::message(format!(
                "cannot persist the Windows restart requirement {}: {error}",
                path.display()
            ))
        })
    } else {
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(WinFailure::message(format!(
                "cannot clear the Windows restart requirement {}: {error}",
                path.display()
            ))),
        }
    }
}

fn decode_inf(bytes: &[u8]) -> String {
    if bytes.starts_with(&[0xff, 0xfe]) {
        let units = bytes[2..]
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]));
        String::from_utf16_lossy(&units.collect::<Vec<_>>())
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

fn windows_inf_directory() -> WinResult<PathBuf> {
    // Do not trust an inherited SystemRoot/WINDIR environment variable in an
    // elevated helper. A crafted value could otherwise make a fake oemNN.inf
    // authorize deletion of an unrelated real driver-store package.
    let mut buffer = vec![0u16; 32_768];
    let length = unsafe { GetWindowsDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) };
    if length == 0 {
        return Err(WinFailure::api("GetWindowsDirectoryW"));
    }
    if length as usize >= buffer.len() {
        return Err(WinFailure::message(
            "the Windows directory exceeds the supported path length",
        ));
    }
    Ok(PathBuf::from(OsString::from_wide(&buffer[..length as usize])).join("INF"))
}

fn published_inf_name(name: &OsStr) -> Option<String> {
    let name = name.to_str()?;
    let lower = name.to_ascii_lowercase();
    let number = lower.strip_prefix("oem")?.strip_suffix(".inf")?;
    if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some(lower)
}

fn is_audiohub_driver_inf(contents: &str) -> bool {
    // Driver-store INFs are trusted machine files, but still require several
    // independent package identifiers before their published name is handed
    // to SetupUninstallOEMInfW. Whitespace and case are insignificant in INF.
    let compact: String = contents
        .chars()
        .filter(|character| !character.is_ascii_whitespace())
        .flat_map(char::to_lowercase)
        .collect();
    [
        r"root\audiohubvad",
        "classguid={4d36e96c-e325-11ce-bfc1-08002be10318}",
        "catalogfile=audiohubvad.cat",
        "addservice=audiohubvad,",
        r"servicebinary=%13%\audiohubvad.sys",
        "providername=\"audiohub\"",
    ]
    .iter()
    .all(|needle| compact.contains(needle))
}

fn published_audiohub_infs() -> WinResult<BTreeSet<String>> {
    let directory = windows_inf_directory()?;
    let mut packages = BTreeSet::new();
    for entry in fs::read_dir(&directory).map_err(|error| {
        WinFailure::message(format!(
            "cannot enumerate the Windows INF directory {}: {error}",
            directory.display()
        ))
    })? {
        let entry = entry.map_err(|error| {
            WinFailure::message(format!("cannot enumerate a Windows INF entry: {error}"))
        })?;
        let Some(name) = published_inf_name(&entry.file_name()) else {
            continue;
        };
        let bytes = match fs::read(entry.path()) {
            Ok(bytes) => bytes,
            // Driver-store cleanup can race a status poll. A vanished entry is
            // already the state we wanted; every other error stays visible.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(WinFailure::message(format!(
                    "cannot inspect published INF {}: {error}",
                    entry.path().display()
                )))
            }
        };
        if is_audiohub_driver_inf(&decode_inf(&bytes)) {
            packages.insert(name);
        }
    }
    Ok(packages)
}

fn locate_payload() -> WinResult<Payload> {
    let exe = std::env::current_exe()
        .map_err(|e| WinFailure::message(format!("cannot resolve helper executable: {e}")))?;
    let helper_dir = exe
        .parent()
        .ok_or_else(|| WinFailure::message("the helper executable has no parent directory"))?;
    // This call enforces the complete fixed installed layout, including the
    // exact helper filename and resources/windows-driver nesting.
    let daemon = derived_daemon_path(true)?;

    for entry in fs::read_dir(helper_dir)
        .map_err(|e| WinFailure::message(format!("cannot read driver resource directory: {e}")))?
    {
        let entry = entry.map_err(|e| {
            WinFailure::message(format!("cannot inspect driver resource directory: {e}"))
        })?;
        let extension = entry
            .path()
            .extension()
            .and_then(OsStr::to_str)
            .unwrap_or("")
            .to_ascii_lowercase();
        if matches!(extension.as_str(), "pfx" | "p12" | "pvk" | "key" | "pem") {
            return Err(WinFailure::message(format!(
                "private-key material is forbidden in the driver payload: {}",
                entry.path().display()
            )));
        }
    }

    let mut files = Vec::new();
    let mut locks = Vec::new();
    for (name, expected) in [
        (INF_NAME, EXPECTED_INF_SHA256),
        (SYS_NAME, EXPECTED_SYS_SHA256),
        (CAT_NAME, EXPECTED_CAT_SHA256),
    ] {
        let path = helper_dir.join(name);
        let canonical = path
            .canonicalize()
            .map_err(|e| WinFailure::message(format!("driver payload is missing {name}: {e}")))?;
        let canonical_parent = canonical.parent().ok_or_else(|| {
            WinFailure::message(format!(
                "driver payload has no parent: {}",
                canonical.display()
            ))
        })?;
        let canonical_helper = helper_dir.canonicalize().map_err(|e| {
            WinFailure::message(format!("cannot resolve driver resource directory: {e}"))
        })?;
        if canonical_parent != canonical_helper || !file_name_is(&canonical, name) {
            return Err(WinFailure::message(format!(
                "driver payload escaped its fixed resource directory: {}",
                path.display()
            )));
        }
        let metadata = fs::metadata(&canonical).map_err(|e| {
            WinFailure::message(format!("cannot inspect driver payload {name}: {e}"))
        })?;
        if !metadata.is_file() || metadata.len() == 0 {
            return Err(WinFailure::message(format!(
                "driver payload is empty or not a file: {}",
                canonical.display()
            )));
        }
        locks.push(lock_and_verify(&canonical, expected)?);
        files.push(canonical);
    }

    let inf_bytes = fs::read(&files[0])
        .map_err(|e| WinFailure::message(format!("cannot read {INF_NAME}: {e}")))?;
    let inf = decode_inf(&inf_bytes).to_ascii_lowercase();
    for expected in [
        r"root\audiohubvad",
        "audiohubvad.sys",
        "audiohubvad.cat",
        "servicebinary",
    ] {
        if !inf.contains(expected) {
            return Err(WinFailure::message(format!(
                "{INF_NAME} does not declare the fixed AudioHubVad package ({expected})"
            )));
        }
    }

    locks.push(lock_and_verify(&daemon, EXPECTED_DAEMON_SHA256)?);
    Ok(Payload {
        inf: standard_dos_path(&files.remove(0))?,
        daemon,
        _locks: locks,
    })
}

fn is_elevated() -> WinResult<bool> {
    let mut raw: HANDLE = 0;
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw) } == 0 {
        return Err(WinFailure::api("OpenProcessToken"));
    }
    let token = KernelHandle(raw);
    let mut elevation: TOKEN_ELEVATION = unsafe { zeroed() };
    let mut returned = 0u32;
    let ok = unsafe {
        GetTokenInformation(
            token.0,
            TokenElevation,
            &mut elevation as *mut _ as *mut _,
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        )
    };
    if ok == 0 {
        return Err(WinFailure::api("GetTokenInformation(TokenElevation)"));
    }
    Ok(elevation.TokenIsElevated != 0)
}

fn get_device_property(
    set: HDEVINFO,
    data: &SP_DEVINFO_DATA,
    property: u32,
) -> WinResult<Option<Vec<u8>>> {
    let mut kind = 0u32;
    let mut needed = 0u32;
    let ok = unsafe {
        SetupDiGetDeviceRegistryPropertyW(
            set,
            data,
            property,
            &mut kind,
            null_mut(),
            0,
            &mut needed,
        )
    };
    if ok != 0 && needed == 0 {
        return Ok(Some(Vec::new()));
    }
    let error = unsafe { GetLastError() };
    if error == ERROR_INVALID_DATA || error == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    if error != ERROR_INSUFFICIENT_BUFFER || needed == 0 {
        return Err(WinFailure {
            code: Some(error),
            detail: format!("SetupDiGetDeviceRegistryPropertyW({property}) failed"),
        });
    }
    let mut bytes = vec![0u8; needed as usize];
    if unsafe {
        SetupDiGetDeviceRegistryPropertyW(
            set,
            data,
            property,
            &mut kind,
            bytes.as_mut_ptr(),
            bytes.len() as u32,
            &mut needed,
        )
    } == 0
    {
        return Err(WinFailure::api("SetupDiGetDeviceRegistryPropertyW"));
    }
    bytes.truncate(needed as usize);
    Ok(Some(bytes))
}

fn utf16_units(bytes: &[u8]) -> Vec<u16> {
    bytes
        .chunks_exact(2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .collect()
}

fn property_string(
    set: HDEVINFO,
    data: &SP_DEVINFO_DATA,
    property: u32,
) -> WinResult<Option<String>> {
    let Some(bytes) = get_device_property(set, data, property)? else {
        return Ok(None);
    };
    let units = utf16_units(&bytes);
    let end = units.iter().position(|&v| v == 0).unwrap_or(units.len());
    Ok(Some(String::from_utf16_lossy(&units[..end])))
}

fn hardware_matches(set: HDEVINFO, data: &SP_DEVINFO_DATA) -> WinResult<bool> {
    let Some(bytes) = get_device_property(set, data, SPDRP_HARDWAREID)? else {
        return Ok(false);
    };
    let units = utf16_units(&bytes);
    Ok(units
        .split(|&v| v == 0)
        .filter(|part| !part.is_empty())
        .map(String::from_utf16_lossy)
        .any(|id| id.eq_ignore_ascii_case(HARDWARE_ID)))
}

fn with_matching_devices(
    mut callback: impl FnMut(HDEVINFO, &SP_DEVINFO_DATA) -> WinResult<()>,
) -> WinResult<u32> {
    let set = DeviceInfoSet::new(
        unsafe { SetupDiGetClassDevsW(null(), null(), 0, DIGCF_ALLCLASSES) },
        "SetupDiGetClassDevsW",
    )?;
    let mut matched = 0u32;
    let mut index = 0u32;
    loop {
        let mut data: SP_DEVINFO_DATA = unsafe { zeroed() };
        data.cbSize = size_of::<SP_DEVINFO_DATA>() as u32;
        if unsafe { SetupDiEnumDeviceInfo(set.0, index, &mut data) } == 0 {
            let error = unsafe { GetLastError() };
            if error == ERROR_NO_MORE_ITEMS {
                break;
            }
            return Err(WinFailure {
                code: Some(error),
                detail: "SetupDiEnumDeviceInfo failed".into(),
            });
        }
        index += 1;
        if hardware_matches(set.0, &data)? {
            matched += 1;
            callback(set.0, &data)?;
        }
    }
    Ok(matched)
}

fn query_device_value(set: HDEVINFO, data: &SP_DEVINFO_DATA, name: &str) -> Option<String> {
    let raw =
        unsafe { SetupDiOpenDevRegKey(set, data, DICS_FLAG_GLOBAL, 0, DIREG_DEV, KEY_QUERY_VALUE) };
    let key = RegistryKey::new(raw, "SetupDiOpenDevRegKey").ok()?;
    let name = wide_str(name);
    let mut kind = 0u32;
    let mut needed = 0u32;
    if unsafe {
        RegQueryValueExW(
            key.0,
            name.as_ptr(),
            null(),
            &mut kind,
            null_mut(),
            &mut needed,
        )
    } != 0
        || needed == 0
        || (kind != REG_SZ && kind != REG_EXPAND_SZ)
    {
        return None;
    }
    let mut bytes = vec![0u8; needed as usize];
    if unsafe {
        RegQueryValueExW(
            key.0,
            name.as_ptr(),
            null(),
            &mut kind,
            bytes.as_mut_ptr(),
            &mut needed,
        )
    } != 0
    {
        return None;
    }
    bytes.truncate(needed as usize);
    let units = utf16_units(&bytes);
    let end = units.iter().position(|&v| v == 0).unwrap_or(units.len());
    Some(
        OsString::from_wide(&units[..end])
            .to_string_lossy()
            .into_owned(),
    )
}

fn set_daemon_image(set: HDEVINFO, data: &SP_DEVINFO_DATA, daemon: &Path) -> WinResult<()> {
    // A freshly registered root devnode does not necessarily have its DIREG_DEV
    // hardware key yet. Opening it made every first install fail with
    // ERROR_KEY_DOES_NOT_EXIST (0xe0000204) and then roll the devnode back.
    // SetupDiCreateDevRegKeyW is create-or-open, so the same path is correct for
    // both a new device and an idempotent update of an existing one.
    let raw = unsafe {
        SetupDiCreateDevRegKeyW(set, data, DICS_FLAG_GLOBAL, 0, DIREG_DEV, null(), null())
    };
    let key = RegistryKey::new(raw, "SetupDiCreateDevRegKeyW(DIREG_DEV)")?;
    let name = wide_str(DAEMON_VALUE);
    let value = wide(daemon.as_os_str());
    let result = unsafe {
        RegSetValueExW(
            key.0,
            name.as_ptr(),
            0,
            REG_SZ,
            value.as_ptr() as *const u8,
            (value.len() * size_of::<u16>()) as u32,
        )
    };
    if result != 0 {
        return Err(WinFailure {
            code: Some(result),
            detail: format!("RegSetValueExW({DAEMON_VALUE}) failed"),
        });
    }
    Ok(())
}

fn control_link_present() -> bool {
    let name = wide_str(CONTROL_DOS_NAME);
    let mut target = vec![0u16; 1024];
    let found = unsafe { QueryDosDeviceW(name.as_ptr(), target.as_mut_ptr(), target.len() as u32) };
    found != 0
}

fn open_service(access: u32) -> WinResult<Option<(ServiceHandle, ServiceHandle)>> {
    let manager = unsafe { OpenSCManagerW(null(), null(), SC_MANAGER_CONNECT) };
    if manager == 0 {
        return Err(WinFailure::api("OpenSCManagerW"));
    }
    let manager = ServiceHandle(manager);
    let name = wide_str(SERVICE_NAME);
    let service = unsafe { OpenServiceW(manager.0, name.as_ptr(), access) };
    if service == 0 {
        let code = unsafe { GetLastError() };
        if code == ERROR_SERVICE_DOES_NOT_EXIST {
            return Ok(None);
        }
        return Err(WinFailure {
            code: Some(code),
            detail: format!("OpenServiceW({SERVICE_NAME}) failed"),
        });
    }
    Ok(Some((manager, ServiceHandle(service))))
}

fn service_present() -> WinResult<bool> {
    match open_service(SERVICE_QUERY_STATUS) {
        Ok(service) => Ok(service.is_some()),
        // DeleteService makes a service name unavailable before the last SCM
        // handle closes. It is still observable state and still requires the
        // caller to report a pending removal/restart instead of "absent".
        Err(error) if error.code == Some(ERROR_SERVICE_MARKED_FOR_DELETE) => Ok(true),
        Err(error) => Err(error),
    }
}

fn remove_service() -> WinResult<bool> {
    let Some((_manager, service)) =
        open_service(SERVICE_QUERY_STATUS | SERVICE_STOP | DELETE_ACCESS)?
    else {
        return Ok(false);
    };
    let mut status: SERVICE_STATUS = unsafe { zeroed() };
    // Best effort: DIF_REMOVE normally stopped a bound PnP driver. For a pure
    // SCM orphan, ask it to stop but still allow DeleteService to mark it for
    // deletion when Windows cannot unload it until restart.
    unsafe { ControlService(service.0, SERVICE_CONTROL_STOP, &mut status) };
    if unsafe { DeleteService(service.0) } == 0 {
        let code = unsafe { GetLastError() };
        if code != ERROR_SERVICE_MARKED_FOR_DELETE && code != ERROR_SERVICE_DOES_NOT_EXIST {
            return Err(WinFailure {
                code: Some(code),
                detail: format!("DeleteService({SERVICE_NAME}) failed"),
            });
        }
    }
    drop(service);
    drop(_manager);
    // A still-open/marked service means Windows will finish after handles close
    // or after reboot. The caller surfaces that distinction instead of claiming
    // a clean removal while SCM still resolves the fixed service name.
    Ok(service_present()?)
}

fn scan(expected_daemon: Option<&Path>) -> WinResult<Snapshot> {
    let mut snapshot = Snapshot::default();
    snapshot.control_link_present = control_link_present();
    snapshot.matching_devices = with_matching_devices(|set, data| {
        let service = property_string(set, data, SPDRP_SERVICE)?;
        let package_bound = service
            .as_deref()
            .is_some_and(|name| name.eq_ignore_ascii_case(SERVICE_NAME));
        let mut status = 0u32;
        let mut problem = 0u32;
        let config_result =
            unsafe { CM_Get_DevNode_Status(&mut status, &mut problem, data.DevInst, 0) };
        let present = config_result == CR_SUCCESS;
        if present {
            snapshot.present_devices += 1;
        }
        if package_bound {
            snapshot.installed_devices += 1;
            if let Some(expected) = expected_daemon {
                if query_device_value(set, data, DAEMON_VALUE)
                    .as_deref()
                    .is_some_and(|actual| actual.eq_ignore_ascii_case(&expected.to_string_lossy()))
                {
                    snapshot.configured_devices += 1;
                }
            } else if query_device_value(set, data, DAEMON_VALUE).is_some() {
                snapshot.configured_devices += 1;
            }
        }
        let has_problem = status & DN_HAS_PROBLEM != 0 || problem != 0;
        if present && package_bound && status & DN_STARTED != 0 && !has_problem {
            snapshot.started_devices += 1;
        }
        if present && package_bound && problem != 0 && !snapshot.problem_codes.contains(&problem) {
            snapshot.problem_codes.push(problem);
        }
        Ok(())
    })?;
    snapshot.published_packages = published_audiohub_infs()?.len() as u32;
    snapshot.service_present = service_present()?;
    snapshot.reboot_pending = reboot_pending();
    snapshot.problem_codes.sort_unstable();
    Ok(snapshot)
}

fn report_from_snapshot(
    operation: &'static str,
    snapshot: Snapshot,
    expected_daemon: Option<&Path>,
    reboot_required: bool,
) -> DriverReport {
    let reboot_required = reboot_required || snapshot.reboot_pending;
    let package_installed = snapshot.package_installed();
    let driver_available = snapshot.available();
    let daemon_image_configured = snapshot.daemon_image_configured();
    let mut state = snapshot.state();
    if reboot_required {
        state = DriverState::RebootRequired;
    }
    let detail = match state {
        DriverState::Ready => {
            "driver package is installed and the device/control link is available".into()
        }
        DriverState::RebootRequired => {
            if operation == "uninstall" {
                "Windows removed the AudioHubVad device and requested a restart to finish driver removal"
                    .into()
            } else {
                "Windows installed the driver package and requested a restart before it is fully usable"
                    .into()
            }
        }
        DriverState::InstalledUnavailable => {
            if snapshot.matching_devices != 0
                && snapshot.installed_devices == 0
                && snapshot.published_packages == 0
            {
                "an unconfigured AudioHubVad root device remains without a bound package".into()
            } else if snapshot.installed_devices == 0 && snapshot.published_packages != 0 {
                "an AudioHubVad package remains in the Windows driver store without a bound device"
                    .into()
            } else if snapshot.service_present
                && snapshot.installed_devices == 0
                && snapshot.published_packages == 0
            {
                "the AudioHubVad SCM service remains without a bound device or driver-store package"
                    .into()
            } else if snapshot.problem_codes.is_empty() {
                "driver package is installed but the device or control link is unavailable".into()
            } else {
                format!(
                    "driver package is installed but Device Manager reports problem code(s) {:?}",
                    snapshot.problem_codes
                )
            }
        }
        DriverState::Absent => "no AudioHubVad device, service, or package is installed".into(),
        _ => unreachable!("snapshot only maps to observable driver states"),
    };
    DriverReport {
        operation,
        state,
        package_installed,
        driver_available,
        reboot_required,
        matching_devices: snapshot.matching_devices,
        present_devices: snapshot.present_devices,
        started_devices: snapshot.started_devices,
        control_link_present: snapshot.control_link_present,
        problem_codes: snapshot.problem_codes,
        daemon_image_configured,
        expected_daemon_image: expected_daemon.map(|path| path.to_string_lossy().into_owned()),
        win32_error: None,
        detail,
    }
}

fn failure_report(
    operation: &'static str,
    state: DriverState,
    failure: WinFailure,
    snapshot: Option<Snapshot>,
    expected_daemon: Option<&Path>,
) -> DriverReport {
    let snapshot = snapshot.unwrap_or_default();
    let package_installed = snapshot.package_installed();
    let driver_available = snapshot.available();
    let daemon_image_configured = snapshot.daemon_image_configured();
    DriverReport {
        operation,
        state,
        package_installed,
        driver_available,
        reboot_required: false,
        matching_devices: snapshot.matching_devices,
        present_devices: snapshot.present_devices,
        started_devices: snapshot.started_devices,
        control_link_present: snapshot.control_link_present,
        problem_codes: snapshot.problem_codes,
        daemon_image_configured,
        expected_daemon_image: expected_daemon.map(|path| path.to_string_lossy().into_owned()),
        win32_error: failure.code,
        detail: failure.detail,
    }
}

fn configure_matching_devices(daemon: &Path) -> WinResult<u32> {
    let mut present = 0u32;
    with_matching_devices(|set, data| {
        let mut status = 0u32;
        let mut problem = 0u32;
        if unsafe { CM_Get_DevNode_Status(&mut status, &mut problem, data.DevInst, 0) }
            == CR_SUCCESS
        {
            present += 1;
            set_daemon_image(set, data, daemon)?;
        }
        Ok(())
    })?;
    Ok(present)
}

fn remove_matching_devices() -> WinResult<bool> {
    let mut reboot_required = false;
    with_matching_devices(|set, data| {
        if unsafe { SetupDiCallClassInstaller(DIF_REMOVE, set, data) } == 0 {
            return Err(WinFailure::api("SetupDiCallClassInstaller(DIF_REMOVE)"));
        }
        let mut params: SP_DEVINSTALL_PARAMS_W = unsafe { zeroed() };
        params.cbSize = size_of::<SP_DEVINSTALL_PARAMS_W>() as u32;
        if unsafe { SetupDiGetDeviceInstallParamsW(set, data, &mut params) } == 0 {
            return Err(WinFailure::api("SetupDiGetDeviceInstallParamsW"));
        }
        reboot_required |= params.Flags & (DI_NEEDREBOOT as u32 | DI_NEEDRESTART as u32) != 0;
        Ok(())
    })?;
    Ok(reboot_required)
}

fn uninstall_published_inf(name: &str) -> WinResult<()> {
    // The file name was obtained from the trusted Windows INF directory and
    // passed without a path. Flags remain zero: SUOI_FORCEDELETE must never be
    // used because another live device could still depend on the package.
    let name = wide_str(name);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if unsafe { SetupUninstallOEMInfW(name.as_ptr(), 0, null()) } != 0 {
            return Ok(());
        }
        let code = unsafe { GetLastError() };
        if code == ERROR_FILE_NOT_FOUND {
            return Ok(());
        }
        if code == ERROR_INF_IN_USE_BY_DEVICES && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(100));
            continue;
        }
        return Err(WinFailure {
            code: Some(code),
            detail: format!("SetupUninstallOEMInfW({}) failed", name_to_string(&name)),
        });
    }
}

fn name_to_string(name: &[u16]) -> String {
    let end = name
        .iter()
        .position(|&value| value == 0)
        .unwrap_or(name.len());
    String::from_utf16_lossy(&name[..end])
}

fn removed_report(snapshot: Snapshot, expected_daemon: &Path) -> DriverReport {
    DriverReport {
        operation: "uninstall",
        state: DriverState::Removed,
        package_installed: false,
        driver_available: false,
        reboot_required: false,
        matching_devices: snapshot.matching_devices,
        present_devices: snapshot.present_devices,
        started_devices: snapshot.started_devices,
        control_link_present: snapshot.control_link_present,
        problem_codes: snapshot.problem_codes,
        daemon_image_configured: false,
        expected_daemon_image: Some(expected_daemon.to_string_lossy().into_owned()),
        win32_error: None,
        detail: "AudioHubVad devices and published driver-store packages were removed".into(),
    }
}

fn create_root_device(inf: &Path, daemon: &Path) -> WinResult<CreatedDevice> {
    let inf = wide(inf.as_os_str());
    let mut class_guid: GUID = unsafe { zeroed() };
    let mut class_name = vec![0u16; 256];
    let mut needed = 0u32;
    if unsafe {
        SetupDiGetINFClassW(
            inf.as_ptr(),
            &mut class_guid,
            class_name.as_mut_ptr(),
            class_name.len() as u32,
            &mut needed,
        )
    } == 0
    {
        return Err(WinFailure::api("SetupDiGetINFClassW"));
    }
    if !guid_eq(&class_guid, &MEDIA_CLASS_GUID) {
        return Err(WinFailure::message(
            "AudioHubVad.inf does not declare the Windows MEDIA setup class",
        ));
    }
    let end = class_name
        .iter()
        .position(|&v| v == 0)
        .unwrap_or(class_name.len());
    let class_name_text = String::from_utf16_lossy(&class_name[..end]);
    if !class_name_text.eq_ignore_ascii_case("MEDIA") {
        return Err(WinFailure::message(format!(
            "AudioHubVad.inf declares unexpected setup class {class_name_text}"
        )));
    }

    let set = DeviceInfoSet::new(
        unsafe { SetupDiCreateDeviceInfoList(&class_guid, 0) },
        "SetupDiCreateDeviceInfoList",
    )?;
    let mut data: SP_DEVINFO_DATA = unsafe { zeroed() };
    data.cbSize = size_of::<SP_DEVINFO_DATA>() as u32;
    if unsafe {
        SetupDiCreateDeviceInfoW(
            set.0,
            class_name.as_ptr(),
            &class_guid,
            null(),
            0,
            DICD_GENERATE_ID,
            &mut data,
        )
    } == 0
    {
        return Err(WinFailure::api("SetupDiCreateDeviceInfoW"));
    }
    let hardware_id = wide_str(HARDWARE_ID);
    // SPDRP_HARDWAREID is REG_MULTI_SZ: append the second terminator rather
    // than relying on SetupAPI to repair a REG_SZ-shaped buffer.
    let mut hardware_multisz = hardware_id;
    hardware_multisz.push(0);
    if unsafe {
        SetupDiSetDeviceRegistryPropertyW(
            set.0,
            &mut data,
            SPDRP_HARDWAREID,
            hardware_multisz.as_ptr() as *const u8,
            (hardware_multisz.len() * size_of::<u16>()) as u32,
        )
    } == 0
    {
        return Err(WinFailure::api(
            "SetupDiSetDeviceRegistryPropertyW(SPDRP_HARDWAREID)",
        ));
    }
    if unsafe { SetupDiCallClassInstaller(DIF_REGISTERDEVICE, set.0, &data) } == 0 {
        return Err(WinFailure::api(
            "SetupDiCallClassInstaller(DIF_REGISTERDEVICE)",
        ));
    }
    let mut created = CreatedDevice {
        set,
        data,
        registered: true,
    };
    if let Err(error) = set_daemon_image(created.set.0, &created.data, daemon) {
        created.remove();
        return Err(error);
    }
    Ok(created)
}

pub fn status() -> DriverReport {
    let expected = derived_daemon_path(false).ok();
    match scan(expected.as_deref()) {
        Ok(snapshot) => report_from_snapshot("status", snapshot, expected.as_deref(), false),
        Err(error) => failure_report(
            "status",
            DriverState::InstalledUnavailable,
            error,
            None,
            expected.as_deref(),
        ),
    }
}

pub fn install() -> DriverReport {
    let payload = match locate_payload() {
        Ok(payload) => payload,
        Err(error) => {
            return failure_report(
                "install",
                DriverState::PayloadInvalid,
                error,
                scan(None).ok(),
                None,
            )
        }
    };
    match is_elevated() {
        Ok(true) => {}
        Ok(false) => {
            return failure_report(
                "install",
                DriverState::NotElevated,
                WinFailure::message("installation requires ShellExecuteExW with the runas verb"),
                scan(Some(&payload.daemon)).ok(),
                Some(&payload.daemon),
            )
        }
        Err(error) => {
            return failure_report(
                "install",
                DriverState::NotElevated,
                error,
                scan(Some(&payload.daemon)).ok(),
                Some(&payload.daemon),
            )
        }
    }

    let mut created: Option<CreatedDevice> = None;
    match configure_matching_devices(&payload.daemon) {
        Ok(0) => match create_root_device(&payload.inf, &payload.daemon) {
            Ok(device) => created = Some(device),
            Err(error) => {
                return failure_report(
                    "install",
                    DriverState::InstallFailed,
                    error,
                    scan(Some(&payload.daemon)).ok(),
                    Some(&payload.daemon),
                )
            }
        },
        Ok(_) => {}
        Err(error) => {
            return failure_report(
                "install",
                DriverState::InstallFailed,
                error,
                scan(Some(&payload.daemon)).ok(),
                Some(&payload.daemon),
            )
        }
    }

    let inf = wide(payload.inf.as_os_str());
    let hardware = wide_str(HARDWARE_ID);
    let mut reboot = 0;
    // NONINTERACTIVE makes an untrusted/unsigned package fail closed. FORCE is
    // intentionally absent: the App must not downgrade a newer installed
    // driver merely because an older AudioHub build was opened later.
    let updated = unsafe {
        UpdateDriverForPlugAndPlayDevicesW(
            0,
            hardware.as_ptr(),
            inf.as_ptr(),
            INSTALLFLAG_NONINTERACTIVE,
            &mut reboot,
        )
    };
    if updated == 0 {
        let code = unsafe { GetLastError() };
        // Idempotent install: NewDev documents ERROR_NO_MORE_ITEMS for an
        // exact/equal-or-newer driver already bound to the matching device.
        // That is success only if a fresh SetupAPI scan proves a package is
        // really bound; never turn the error code alone into a green state.
        if code == ERROR_NO_MORE_ITEMS {
            if let Ok(snapshot) = scan(Some(&payload.daemon)) {
                if snapshot.installed_devices != 0 {
                    if let Err(error) = set_reboot_marker(false) {
                        return failure_report(
                            "install",
                            DriverState::InstallFailed,
                            error,
                            Some(snapshot),
                            Some(&payload.daemon),
                        );
                    }
                    return report_from_snapshot("install", snapshot, Some(&payload.daemon), false);
                }
            }
        }
        let error = WinFailure {
            code: Some(code),
            detail: "UpdateDriverForPlugAndPlayDevicesW failed".into(),
        };
        if let Some(device) = created.as_mut() {
            device.remove();
        }
        return failure_report(
            "install",
            DriverState::InstallFailed,
            error,
            scan(Some(&payload.daemon)).ok(),
            Some(&payload.daemon),
        );
    }

    // PnP start/control-device publication is asynchronous. Ten seconds is
    // long enough for a normal root device without hiding a stuck install.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match scan(Some(&payload.daemon)) {
            Ok(snapshot) if snapshot.available() || Instant::now() >= deadline => {
                if let Err(error) = set_reboot_marker(reboot != 0) {
                    return failure_report(
                        "install",
                        DriverState::InstallFailed,
                        error,
                        Some(snapshot),
                        Some(&payload.daemon),
                    );
                }
                return report_from_snapshot(
                    "install",
                    snapshot,
                    Some(&payload.daemon),
                    reboot != 0,
                );
            }
            Ok(_) => thread::sleep(Duration::from_millis(100)),
            Err(error) => {
                return failure_report(
                    "install",
                    DriverState::InstallFailed,
                    error,
                    None,
                    Some(&payload.daemon),
                )
            }
        }
    }
}

pub fn uninstall() -> DriverReport {
    // Reuse the same authenticated, read-locked Program Files layout as
    // install. The normal NSIS uninstaller invokes this before deleting any
    // application files, so a failed removal leaves a complete retry path.
    let payload = match locate_payload() {
        Ok(payload) => payload,
        Err(error) => {
            return failure_report(
                "uninstall",
                DriverState::PayloadInvalid,
                error,
                scan(None).ok(),
                None,
            )
        }
    };
    match is_elevated() {
        Ok(true) => {}
        Ok(false) => {
            return failure_report(
                "uninstall",
                DriverState::NotElevated,
                WinFailure::message("driver removal requires an elevated NSIS uninstaller"),
                scan(Some(&payload.daemon)).ok(),
                Some(&payload.daemon),
            )
        }
        Err(error) => {
            return failure_report(
                "uninstall",
                DriverState::NotElevated,
                error,
                scan(Some(&payload.daemon)).ok(),
                Some(&payload.daemon),
            )
        }
    }

    let mut packages = match published_audiohub_infs() {
        Ok(packages) => packages,
        Err(error) => {
            return failure_report(
                "uninstall",
                DriverState::RemoveFailed,
                error,
                scan(Some(&payload.daemon)).ok(),
                Some(&payload.daemon),
            )
        }
    };
    let mut reboot_required = match remove_matching_devices() {
        Ok(reboot) => reboot,
        Err(error) => {
            return failure_report(
                "uninstall",
                DriverState::RemoveFailed,
                error,
                scan(Some(&payload.daemon)).ok(),
                Some(&payload.daemon),
            )
        }
    };
    // Remove the fixed SCM service before asking SetupAPI to evict its package.
    // An orphan service can keep the image/package in use even after the root
    // devnode disappeared; DeleteService either clears it now or explicitly
    // turns the operation into a restart-required removal.
    match remove_service() {
        Ok(pending) => reboot_required |= pending,
        Err(error) => {
            return failure_report(
                "uninstall",
                DriverState::RemoveFailed,
                error,
                scan(Some(&payload.daemon)).ok(),
                Some(&payload.daemon),
            )
        }
    }
    // Include packages that appeared between the initial scan and devnode
    // removal. The BTreeSet also makes repeated/upgrade packages deterministic.
    match published_audiohub_infs() {
        Ok(after) => packages.extend(after),
        Err(error) => {
            return failure_report(
                "uninstall",
                DriverState::RemoveFailed,
                error,
                scan(Some(&payload.daemon)).ok(),
                Some(&payload.daemon),
            )
        }
    }
    for package in packages {
        if let Err(error) = uninstall_published_inf(&package) {
            return failure_report(
                "uninstall",
                DriverState::RemoveFailed,
                error,
                scan(Some(&payload.daemon)).ok(),
                Some(&payload.daemon),
            );
        }
    }
    if let Err(error) = set_reboot_marker(reboot_required) {
        return failure_report(
            "uninstall",
            DriverState::RemoveFailed,
            error,
            scan(Some(&payload.daemon)).ok(),
            Some(&payload.daemon),
        );
    }

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match scan(Some(&payload.daemon)) {
            Ok(snapshot)
                if snapshot.matching_devices == 0
                    && snapshot.published_packages == 0
                    && ((!snapshot.control_link_present && !snapshot.service_present)
                        || reboot_required) =>
            {
                if reboot_required {
                    return report_from_snapshot(
                        "uninstall",
                        snapshot,
                        Some(&payload.daemon),
                        true,
                    );
                }
                return removed_report(snapshot, &payload.daemon);
            }
            Ok(snapshot) if Instant::now() >= deadline => {
                return failure_report(
                    "uninstall",
                    DriverState::RemoveFailed,
                    WinFailure::message(
                        "AudioHubVad devices, control link, or driver-store package remained after removal",
                    ),
                    Some(snapshot),
                    Some(&payload.daemon),
                )
            }
            Ok(_) => thread::sleep(Duration::from_millis(100)),
            Err(error) => {
                return failure_report(
                    "uninstall",
                    DriverState::RemoveFailed,
                    error,
                    None,
                    Some(&payload.daemon),
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_identifiers_match_the_driver_contract() {
        assert_eq!(HARDWARE_ID, r"ROOT\AudioHubVad");
        assert_eq!(SERVICE_NAME, "AudioHubVad");
        assert_eq!(CONTROL_DOS_NAME, "AudioHubVadCtl");
        assert_eq!(DAEMON_VALUE, "AudioHubDaemonImage");
        assert!(guid_eq(
            &MEDIA_CLASS_GUID,
            &GUID::from_u128(0x4d36e96c_e325_11ce_bfc1_08002be10318)
        ));
    }

    #[test]
    fn utf16_inf_decoder_handles_the_checked_in_encoding() {
        let bytes = [0xff, 0xfe, b'R', 0, b'O', 0, b'O', 0, b'T', 0];
        assert_eq!(decode_inf(&bytes), "ROOT");
    }

    #[test]
    fn published_inf_names_are_strictly_oem_number_inf() {
        assert_eq!(
            published_inf_name(OsStr::new("OEM42.INF")).as_deref(),
            Some("oem42.inf")
        );
        for rejected in ["oem.inf", "oem1.pnf", "AudioHubVad.inf", r"..\oem1.inf"] {
            assert!(published_inf_name(OsStr::new(rejected)).is_none());
        }
    }

    #[test]
    fn driver_store_match_requires_the_complete_audiohub_identity() {
        let inf = r#"
            ROOT\AudioHubVad
            ClassGuid = {4D36E96C-E325-11CE-BFC1-08002BE10318}
            CatalogFile = AudioHubVad.cat
            AddService = AudioHubVad, 2, install
            ServiceBinary = %13%\AudioHubVad.sys
            ProviderName = "AudioHub"
        "#;
        assert!(is_audiohub_driver_inf(inf));
        assert!(!is_audiohub_driver_inf(
            &inf.replace("ROOT\\AudioHubVad", "ROOT\\SomeOtherDevice")
        ));
    }

    #[test]
    fn restart_marker_only_applies_to_the_boot_that_created_it() {
        let boot = 13_000_000_u64;
        assert!(marker_matches_boot(boot, boot));
        assert!(marker_matches_boot(boot, boot + 119_999));
        assert!(!marker_matches_boot(boot, boot + 120_000));
        assert!(!marker_matches_boot(boot, boot + 4 * 60 * 60 * 1_000));
    }
}
