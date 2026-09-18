//! Match a running Windows daemon to the signed payload selected at App build time.
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::sync::Mutex;

use sha2::{Digest, Sha256};
use windows::core::PWSTR;
use windows::Win32::Foundation::{FILETIME, HANDLE, WAIT_TIMEOUT};
use windows::Win32::Storage::FileSystem::{
    GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_SHARE_READ,
};
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, QueryFullProcessImageNameW, WaitForSingleObject,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
};

use super::{DaemonError, IpcEndpointJson};

const EXPECTED: &str = env!("AUDIOHUB_WIN_DAEMON_SHA256");

struct VerifiedImage {
    pid: u32,
    started: u64,
    process: OwnedHandle,
    // Keep the verified file open without write/delete sharing. Cached bytes
    // cannot be replaced or modified while this entry is reused.
    _file: File,
    identity: [u64; 5],
    matches: bool,
}

static CACHE: Mutex<Option<VerifiedImage>> = Mutex::new(None);

fn handle(process: &OwnedHandle) -> HANDLE {
    HANDLE(process.as_raw_handle())
}

fn live(process: &OwnedHandle) -> bool {
    unsafe { WaitForSingleObject(handle(process), 0) == WAIT_TIMEOUT }
}

fn ticks(time: FILETIME) -> u64 {
    ((time.dwHighDateTime as u64) << 32) | time.dwLowDateTime as u64
}

fn identity(file: &File) -> Option<[u64; 5]> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info).ok()? };
    Some([
        info.dwVolumeSerialNumber as u64,
        ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64,
        ((info.nFileSizeHigh as u64) << 32) | info.nFileSizeLow as u64,
        ticks(info.ftCreationTime),
        ticks(info.ftLastWriteTime),
    ])
}

fn locked_file(path: &Path) -> Option<File> {
    OpenOptions::new().read(true).share_mode(FILE_SHARE_READ.0).open(path).ok()
}

fn digest(file: &mut File) -> Option<String> {
    let mut hash = Sha256::new();
    let mut bytes = [0u8; 64 * 1024];
    loop {
        let count = file.read(&mut bytes).ok()?;
        if count == 0 { break; }
        hash.update(&bytes[..count]);
    }
    Some(format!("{:x}", hash.finalize()))
}

pub(super) fn package_available() -> bool {
    EXPECTED.len() == 64 && EXPECTED.bytes().all(|b| b.is_ascii_hexdigit())
}

pub(super) fn release_exited_image() {
    if let Ok(mut cache) = CACHE.lock() {
        if cache.as_ref().is_some_and(|entry| !live(&entry.process)) {
            *cache = None;
        }
    }
}

fn conflict() -> DaemonError {
    DaemonError::new("service-conflict", "The running AudioHub service does not match this App build")
        .with_detail("Exit the existing service from its owning App, then start or repair this installation. No unverified process was stopped.")
}

/// Empty build metadata is deliberately rejected, including unpackaged debug
/// layouts and the legacy CLI-as-daemon fallback. IPC compatibility is not build identity.
pub(super) fn check(ep: &IpcEndpointJson) -> Result<(), DaemonError> {
    if !package_available() {
        return Err(DaemonError::new("no-binary", "This App has no packaged Windows daemon digest")
            .with_detail("Build the signed daemon payload before building the App, including development layouts."));
    }
    inspect(ep).filter(|matches| *matches).map(|_| ()).ok_or_else(conflict)
}

fn inspect(ep: &IpcEndpointJson) -> Option<bool> {
    let mut cache = CACHE.lock().ok()?;
    if cache.as_ref().is_some_and(|entry| entry.pid != ep.pid || !live(&entry.process)) {
        *cache = None;
    }
    let raw = unsafe {
        OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE, false, ep.pid).ok()?
    };
    let process = unsafe { OwnedHandle::from_raw_handle(raw.0) };
    if !live(&process) { return None; }
    let (mut created, mut exited, mut kernel, mut user) = (
        FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default(),
    );
    unsafe { GetProcessTimes(raw, &mut created, &mut exited, &mut kernel, &mut user).ok()? };
    let started = ticks(created);
    let mut path = vec![0u16; 32768];
    let mut length = path.len() as u32;
    unsafe { QueryFullProcessImageNameW(raw, PROCESS_NAME_WIN32, PWSTR(path.as_mut_ptr()), &mut length).ok()? };
    let path = std::path::PathBuf::from(std::ffi::OsString::from_wide(&path[..length as usize]));
    let mut file = locked_file(&path)?;
    let id = identity(&file)?;
    // A newly written path cannot establish the build of an older process.
    if id[3] > started || id[4] > started { return None; }
    if let Some(entry) = cache.as_ref() {
        if entry.pid == ep.pid && entry.started == started && entry.identity == id && live(&entry.process) {
            return Some(entry.matches && live(&process));
        }
    }
    *cache = None;
    let matches = digest(&mut file)? == EXPECTED;
    if identity(&file)? != id || !live(&process) { return None; }
    *cache = Some(VerifiedImage { pid: ep.pid, started, process, _file: file, identity: id, matches });
    Some(matches)
}

/// Pin the selected spawn file through readiness. The expected package digest
/// is independent of both this path and the actual running process image.
pub(super) fn check_spawn(path: &Path) -> Result<File, DaemonError> {
    release_exited_image();
    if !package_available() {
        return Err(DaemonError::new("no-binary", "This App has no packaged Windows daemon digest"));
    }
    let mut file = locked_file(path).ok_or_else(conflict)?;
    if digest(&mut file).as_deref() != Some(EXPECTED) { return Err(conflict()); }
    Ok(file)
}
