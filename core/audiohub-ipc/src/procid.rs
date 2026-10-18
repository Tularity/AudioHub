//! Process identity for `ipc.json` ownership.
//!
//! A PID alone proves nothing: after a crash or power loss the OS reuses it for
//! an unrelated process. The daemon records its own kernel start time next to
//! its PID, and every reader compares that against the live process.
//!
//! Units are platform-defined but identical for writer and readers on one host:
//! Windows FILETIME ticks (100 ns), macOS microseconds since the epoch.

/// What the OS says about a PID right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcStart {
    /// No such process (or it has already exited).
    Missing,
    /// The process exists but its start time cannot be read here.
    Unknown,
    Started(u64),
}

/// Verdict on whether the process which wrote `ipc.json` still owns it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerProof {
    /// The recorded PID is running and started when the file says it did.
    Alive,
    /// The file belongs to a process that is gone or to an unrelated process
    /// which now holds the same PID.
    Stale,
    /// Neither proven: the caller falls back to its own liveness probe.
    Unproven,
}

/// Pure decision, separated from the OS probe so it can be tested exhaustively.
pub fn judge_owner(recorded: Option<u64>, actual: ProcStart) -> OwnerProof {
    match (actual, recorded) {
        (ProcStart::Missing, _) => OwnerProof::Stale,
        (ProcStart::Unknown, _) => OwnerProof::Unproven,
        (ProcStart::Started(_), None) => OwnerProof::Unproven,
        (ProcStart::Started(a), Some(r)) if a == r => OwnerProof::Alive,
        (ProcStart::Started(_), Some(_)) => OwnerProof::Stale,
    }
}

/// Start identity of `pid` as the OS reports it.
pub fn process_start(pid: u32) -> ProcStart {
    imp::process_start(pid)
}

/// Start identity of the calling process, for recording in `ipc.json`.
pub fn own_start_id() -> Option<u64> {
    match process_start(std::process::id()) {
        ProcStart::Started(v) => Some(v),
        _ => None,
    }
}

/// Ownership proof for a recorded `(pid, started)` pair against the live OS.
pub fn owner_proof(pid: u32, recorded: Option<u64>) -> OwnerProof {
    judge_owner(recorded, process_start(pid))
}

#[cfg(windows)]
mod imp {
    use super::ProcStart;
    use std::ffi::c_void;

    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct FileTime {
        low: u32,
        high: u32,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
        fn GetProcessTimes(
            h: *mut c_void,
            created: *mut FileTime,
            exited: *mut FileTime,
            kernel: *mut FileTime,
            user: *mut FileTime,
        ) -> i32;
        fn CloseHandle(h: *mut c_void) -> i32;
        fn GetLastError() -> u32;
    }

    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const ERROR_INVALID_PARAMETER: u32 = 87;

    fn ticks(t: FileTime) -> u64 {
        ((t.high as u64) << 32) | t.low as u64
    }

    pub fn process_start(pid: u32) -> ProcStart {
        if pid == 0 {
            return ProcStart::Missing;
        }
        // SAFETY: plain Win32 calls; the handle is closed on every path.
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                return if GetLastError() == ERROR_INVALID_PARAMETER {
                    ProcStart::Missing
                } else {
                    ProcStart::Unknown
                };
            }
            let (mut c, mut e, mut k, mut u) = (
                FileTime::default(),
                FileTime::default(),
                FileTime::default(),
                FileTime::default(),
            );
            let ok = GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u);
            CloseHandle(h);
            if ok == 0 {
                ProcStart::Unknown
            } else if ticks(e) != 0 {
                // exited, handle kept alive by someone else
                ProcStart::Missing
            } else {
                ProcStart::Started(ticks(c))
            }
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

#[cfg(all(unix, not(target_os = "macos")))]
mod imp {
    use super::ProcStart;

    pub fn process_start(pid: u32) -> ProcStart {
        let Some(pid) = i32::try_from(pid).ok().filter(|p| *p > 0) else {
            return ProcStart::Missing;
        };
        // SAFETY: signal 0 only probes existence.
        unsafe {
            if libc::kill(pid, 0) != 0
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                return ProcStart::Missing;
            }
        }
        ProcStart::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn judge_matrix() {
        use OwnerProof::*;
        use ProcStart::*;
        assert_eq!(judge_owner(Some(5), Started(5)), Alive);
        // PID reused by an unrelated process: different start time.
        assert_eq!(judge_owner(Some(5), Started(9)), Stale);
        assert_eq!(judge_owner(Some(5), Missing), Stale);
        assert_eq!(judge_owner(None, Missing), Stale);
        assert_eq!(judge_owner(Some(5), Unknown), Unproven);
        assert_eq!(judge_owner(None, Started(5)), Unproven);
    }

    #[test]
    fn own_process_is_alive_and_wrong_start_is_stale() {
        let Some(me) = own_start_id() else {
            return; // platform without start-time support
        };
        let pid = std::process::id();
        assert_eq!(owner_proof(pid, Some(me)), OwnerProof::Alive);
        assert_eq!(owner_proof(pid, Some(me.wrapping_add(1))), OwnerProof::Stale);
    }
}
