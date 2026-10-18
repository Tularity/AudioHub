//! Is the process which wrote `ipc.json` still the one running under that PID?
//!
//! Mirrors `audiohub_ipc::procid` (the App does not link the IPC crate). The
//! daemon records its kernel start time beside its PID; units match its writer:
//! Windows FILETIME ticks, macOS microseconds since the epoch. After a crash or
//! power loss the OS reuses the PID, and a PID that merely exists proves nothing.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProcStart {
    Missing,
    Unknown,
    Started(u64),
}

/// True only when the file is proven to belong to a process which is gone or to
/// an unrelated process that inherited the PID. Unproven is NOT stale.
pub(crate) fn is_stale(recorded: Option<u64>, actual: ProcStart) -> bool {
    match (actual, recorded) {
        (ProcStart::Missing, _) => true,
        (ProcStart::Started(a), Some(r)) => a != r,
        _ => false,
    }
}

pub(crate) fn process_start(pid: u32) -> ProcStart {
    imp::process_start(pid)
}

#[cfg(windows)]
mod imp {
    use super::ProcStart;
    use windows::Win32::Foundation::{CloseHandle, FILETIME};
    use windows::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    // HRESULT_FROM_WIN32(ERROR_INVALID_PARAMETER): OpenProcess for a PID that
    // does not exist.
    const NO_SUCH_PROCESS: i32 = 0x8007_0057_u32 as i32;

    fn ticks(t: FILETIME) -> u64 {
        ((t.dwHighDateTime as u64) << 32) | t.dwLowDateTime as u64
    }

    pub fn process_start(pid: u32) -> ProcStart {
        if pid == 0 {
            return ProcStart::Missing;
        }
        let handle = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
            Ok(h) => h,
            Err(e) if e.code().0 == NO_SUCH_PROCESS => return ProcStart::Missing,
            Err(_) => return ProcStart::Unknown,
        };
        let (mut c, mut e, mut k, mut u) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        let result = unsafe { GetProcessTimes(handle, &mut c, &mut e, &mut k, &mut u) };
        let _ = unsafe { CloseHandle(handle) };
        match result {
            Err(_) => ProcStart::Unknown,
            Ok(()) if ticks(e) != 0 => ProcStart::Missing,
            Ok(()) => ProcStart::Started(ticks(c)),
        }
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::ProcStart;

    pub fn process_start(pid: u32) -> ProcStart {
        let Some(pid) = i32::try_from(pid).ok().filter(|p| *p > 0) else {
            return ProcStart::Missing;
        };
        // SAFETY: `info` is writable for exactly `expected` bytes.
        unsafe {
            let mut info: libc::proc_bsdinfo = std::mem::zeroed();
            let expected = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
            let n = libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut info as *mut libc::proc_bsdinfo).cast(),
                expected,
            );
            if n == expected {
                return ProcStart::Started(
                    info.pbi_start_tvsec
                        .saturating_mul(1_000_000)
                        .saturating_add(info.pbi_start_tvusec),
                );
            }
            if libc::kill(pid, 0) != 0
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                ProcStart::Missing
            } else {
                ProcStart::Unknown
            }
        }
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod imp {
    use super::ProcStart;
    pub fn process_start(_pid: u32) -> ProcStart {
        ProcStart::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reused_pid_with_another_start_is_stale() {
        assert!(is_stale(Some(5), ProcStart::Started(9)));
        assert!(is_stale(Some(5), ProcStart::Missing));
        assert!(is_stale(None, ProcStart::Missing));
    }

    #[test]
    fn matching_or_unprovable_is_not_stale() {
        assert!(!is_stale(Some(5), ProcStart::Started(5)));
        assert!(!is_stale(Some(5), ProcStart::Unknown));
        assert!(!is_stale(None, ProcStart::Started(5)));
        assert!(!is_stale(None, ProcStart::Unknown));
    }

    #[test]
    fn own_process_reads_back_consistently() {
        let pid = std::process::id();
        match process_start(pid) {
            ProcStart::Started(v) => {
                assert_eq!(process_start(pid), ProcStart::Started(v));
                assert!(!is_stale(Some(v), ProcStart::Started(v)));
                assert!(is_stale(Some(v + 1), process_start(pid)));
            }
            ProcStart::Unknown => {}
            ProcStart::Missing => panic!("own process cannot be missing"),
        }
    }
}
