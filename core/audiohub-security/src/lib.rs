//! Cross-platform protection for AudioHub files containing private material.
//!
//! Unix callers get owner-only modes. Windows callers get a protected DACL
//! containing exactly the current process user, BUILTIN\Administrators and
//! LocalSystem. The Windows implementation calls the security APIs directly;
//! no shell or PowerShell process participates in secret persistence.

use std::fs;
use std::io;
use std::path::Path;

/// Create `path` if necessary and restrict the directory and inheriting
/// children to AudioHub's private-principal set.
pub fn secure_private_directory(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    platform::secure(path, true)
}

/// Replace an existing file's inherited permissions with AudioHub's private
/// principal set. Call this immediately after creating a secret temporary file
/// and before writing private bytes to it.
pub fn secure_private_file(path: &Path) -> io::Result<()> {
    platform::secure(path, false)
}

#[cfg(unix)]
mod platform {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    pub(super) fn secure(path: &Path, directory: bool) -> io::Result<()> {
        let mode = if directory { 0o700 } else { 0o600 };
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;

    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, LocalFree, ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, HANDLE,
        PSID,
    };
    use windows_sys::Win32::Security::Authorization::{
        SetEntriesInAclW, SetNamedSecurityInfoW, EXPLICIT_ACCESS_W, NO_MULTIPLE_TRUSTEE,
        SET_ACCESS, SE_FILE_OBJECT, TRUSTEE_IS_SID, TRUSTEE_IS_USER, TRUSTEE_IS_WELL_KNOWN_GROUP,
        TRUSTEE_W,
    };
    use windows_sys::Win32::Security::{
        CreateWellKnownSid, EqualSid, GetTokenInformation, TokenUser, WinBuiltinAdministratorsSid,
        WinLocalSystemSid, ACL, DACL_SECURITY_INFORMATION, NO_INHERITANCE,
        OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
        SUB_CONTAINERS_AND_OBJECTS_INHERIT, TOKEN_QUERY, TOKEN_USER, WELL_KNOWN_SID_TYPE,
    };
    use windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    struct OwnedHandle(HANDLE);

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            // SAFETY: this handle is returned by OpenProcessToken and is owned
            // by this guard exactly once.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    struct OwnedAcl(*mut ACL);

    impl Drop for OwnedAcl {
        fn drop(&mut self) {
            // SAFETY: SetEntriesInAclW allocates this ACL with LocalAlloc and
            // documents LocalFree as the matching release function.
            unsafe {
                LocalFree(self.0.cast::<c_void>());
            }
        }
    }

    pub(super) fn secure(path: &Path, directory: bool) -> io::Result<()> {
        let mut user = current_user_token_information()?;
        let mut administrators = well_known_sid(WinBuiltinAdministratorsSid)?;
        let mut system = well_known_sid(WinLocalSystemSid)?;

        // SAFETY: all three aligned buffers remain alive until after the ACL
        // has been built and installed. TOKEN_USER::User.Sid points inside
        // `user`; the well-known SID starts at byte zero of its buffer.
        let user_sid = unsafe { (*(user.as_mut_ptr().cast::<TOKEN_USER>())).User.Sid };
        let administrators_sid = administrators.as_mut_ptr().cast::<c_void>();
        let system_sid = system.as_mut_ptr().cast::<c_void>();
        let inheritance = if directory {
            SUB_CONTAINERS_AND_OBJECTS_INHERIT
        } else {
            NO_INHERITANCE
        };
        let mut entries = vec![
            explicit_full_control(user_sid, TRUSTEE_IS_USER, inheritance),
            explicit_full_control(administrators_sid, TRUSTEE_IS_WELL_KNOWN_GROUP, inheritance),
        ];
        // A service may itself run as LocalSystem. Avoid installing the same
        // trustee twice while preserving the exact permitted-principal set.
        // SAFETY: both SIDs were returned by Windows and remain alive here.
        if unsafe { EqualSid(user_sid, system_sid) } == 0 {
            entries.push(explicit_full_control(
                system_sid,
                TRUSTEE_IS_USER,
                inheritance,
            ));
        }

        let mut raw_acl = ptr::null_mut();
        // SAFETY: entries and their SID pointers are valid for the duration of
        // the call. A null old ACL requests a new ACL containing only these
        // three explicit entries.
        let status = unsafe {
            SetEntriesInAclW(
                entries.len() as u32,
                entries.as_ptr(),
                ptr::null(),
                &mut raw_acl,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(win32_error(status));
        }
        let acl = OwnedAcl(raw_acl);

        let wide_path = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        // Set the owner as well as the DACL. Windows grants an object's owner
        // implicit WRITE_DAC even when no ACE names that SID, so retaining an
        // unexpected legacy owner would let a principal outside this allowlist
        // restore its own access after the repair.
        //
        // SAFETY: wide_path is NUL-terminated; user_sid and acl remain alive for
        // the call; the remaining optional descriptor components are null.
        let status = unsafe {
            SetNamedSecurityInfoW(
                wide_path.as_ptr(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION
                    | DACL_SECURITY_INFORMATION
                    | PROTECTED_DACL_SECURITY_INFORMATION,
                user_sid,
                ptr::null_mut(),
                acl.0,
                ptr::null(),
            )
        };
        if status != ERROR_SUCCESS {
            return Err(win32_error(status));
        }
        Ok(())
    }

    fn explicit_full_control(sid: PSID, trustee_type: i32, inheritance: u32) -> EXPLICIT_ACCESS_W {
        EXPLICIT_ACCESS_W {
            grfAccessPermissions: FILE_ALL_ACCESS,
            grfAccessMode: SET_ACCESS,
            grfInheritance: inheritance,
            Trustee: TRUSTEE_W {
                pMultipleTrustee: ptr::null_mut(),
                MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: trustee_type,
                ptstrName: sid.cast::<u16>(),
            },
        }
    }

    fn current_user_token_information() -> io::Result<Vec<usize>> {
        let mut raw_token = 0;
        // SAFETY: raw_token points to writable storage for the returned handle;
        // GetCurrentProcess returns the documented process pseudo-handle.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw_token) } == 0 {
            return Err(last_os_error());
        }
        let token = OwnedHandle(raw_token);

        let mut required = 0;
        // SAFETY: a zero-length probe is the documented way to obtain the
        // TOKEN_USER buffer size.
        let probe =
            unsafe { GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &mut required) };
        if probe != 0 {
            return Err(io::Error::other(
                "GetTokenInformation unexpectedly accepted an empty TOKEN_USER buffer",
            ));
        }
        let error = unsafe { GetLastError() };
        if error != ERROR_INSUFFICIENT_BUFFER {
            return Err(win32_error(error));
        }
        if required == 0 {
            return Err(io::Error::other(
                "GetTokenInformation returned no TOKEN_USER size",
            ));
        }

        let mut buffer = aligned_buffer(required);
        // SAFETY: the aligned allocation is at least `required` bytes and the
        // TOKEN_USER result remains self-contained in that allocation.
        if unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                buffer.as_mut_ptr().cast::<c_void>(),
                required,
                &mut required,
            )
        } == 0
        {
            return Err(last_os_error());
        }
        Ok(buffer)
    }

    fn well_known_sid(kind: WELL_KNOWN_SID_TYPE) -> io::Result<Vec<usize>> {
        let mut required = 0;
        // SAFETY: a null-buffer probe asks CreateWellKnownSid for its required
        // size. No domain SID is needed for these machine-local well-known SIDs.
        let probe =
            unsafe { CreateWellKnownSid(kind, ptr::null_mut(), ptr::null_mut(), &mut required) };
        if probe != 0 {
            return Err(io::Error::other(
                "CreateWellKnownSid unexpectedly accepted an empty SID buffer",
            ));
        }
        let error = unsafe { GetLastError() };
        if error != ERROR_INSUFFICIENT_BUFFER {
            return Err(win32_error(error));
        }
        if required == 0 {
            return Err(io::Error::other("CreateWellKnownSid returned no SID size"));
        }

        let mut buffer = aligned_buffer(required);
        // SAFETY: the aligned allocation is at least `required` bytes and the
        // API writes one self-contained SID into it.
        if unsafe {
            CreateWellKnownSid(
                kind,
                ptr::null_mut(),
                buffer.as_mut_ptr().cast::<c_void>(),
                &mut required,
            )
        } == 0
        {
            return Err(last_os_error());
        }
        Ok(buffer)
    }

    fn aligned_buffer(required: u32) -> Vec<usize> {
        let word = std::mem::size_of::<usize>();
        let words = (required as usize).div_ceil(word);
        vec![0; words.max(1)]
    }

    fn last_os_error() -> io::Error {
        // SAFETY: GetLastError has no preconditions and is read immediately
        // after the failed Windows API call.
        win32_error(unsafe { GetLastError() })
    }

    fn win32_error(code: u32) -> io::Error {
        io::Error::from_raw_os_error(code as i32)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use windows_sys::Win32::Security::Authorization::{
            GetExplicitEntriesFromAclW, GetNamedSecurityInfoW, GRANT_ACCESS,
        };
        use windows_sys::Win32::Security::{GetSecurityDescriptorControl, SE_DACL_PROTECTED};

        struct OwnedSecurityDescriptor(*mut c_void);

        impl Drop for OwnedSecurityDescriptor {
            fn drop(&mut self) {
                // SAFETY: GetNamedSecurityInfoW allocates the descriptor with
                // LocalAlloc and transfers it to this guard exactly once.
                unsafe {
                    LocalFree(self.0);
                }
            }
        }

        struct OwnedExplicitEntries(*mut EXPLICIT_ACCESS_W);

        impl Drop for OwnedExplicitEntries {
            fn drop(&mut self) {
                // SAFETY: GetExplicitEntriesFromAclW allocates this array with
                // LocalAlloc and transfers it to this guard exactly once.
                unsafe {
                    LocalFree(self.0.cast::<c_void>());
                }
            }
        }

        #[test]
        fn native_acl_has_exact_owner_protection_and_principal_set() {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let directory = std::env::temp_dir().join(format!(
                "audiohub-private-fs-windows-{}-{nonce}",
                std::process::id()
            ));
            secure_private_directory(&directory).expect("secure directory");
            let file = directory.join("secret");
            fs::write(&file, b"secret").expect("write file");
            secure_private_file(&file).expect("secure file");

            assert_security(&directory, SUB_CONTAINERS_AND_OBJECTS_INHERIT);
            assert_security(&file, NO_INHERITANCE);
            fs::remove_dir_all(directory).expect("cleanup");
        }

        fn assert_security(path: &Path, expected_inheritance: u32) {
            let wide_path = path
                .as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect::<Vec<_>>();
            let mut owner = ptr::null_mut();
            let mut dacl = ptr::null_mut();
            let mut raw_descriptor = ptr::null_mut();
            // SAFETY: wide_path is NUL-terminated and every output pointer
            // refers to writable storage. The returned descriptor owns the SID
            // and ACL views until the guard below is dropped.
            let status = unsafe {
                GetNamedSecurityInfoW(
                    wide_path.as_ptr(),
                    SE_FILE_OBJECT,
                    OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                    &mut owner,
                    ptr::null_mut(),
                    &mut dacl,
                    ptr::null_mut(),
                    &mut raw_descriptor,
                )
            };
            assert_eq!(
                status,
                ERROR_SUCCESS,
                "read security for {}",
                path.display()
            );
            assert!(!raw_descriptor.is_null(), "missing security descriptor");
            let _descriptor = OwnedSecurityDescriptor(raw_descriptor);
            assert!(!owner.is_null(), "missing owner SID");
            assert!(!dacl.is_null(), "missing DACL");

            let mut control = 0;
            let mut revision = 0;
            // SAFETY: raw_descriptor remains owned by `_descriptor` and both
            // outputs point to writable scalars.
            assert_ne!(
                unsafe {
                    GetSecurityDescriptorControl(raw_descriptor, &mut control, &mut revision)
                },
                0,
                "GetSecurityDescriptorControl failed: {}",
                last_os_error()
            );
            assert_ne!(control & SE_DACL_PROTECTED, 0, "DACL must be protected");

            let mut user = current_user_token_information().expect("current user SID");
            let mut administrators =
                well_known_sid(WinBuiltinAdministratorsSid).expect("Administrators SID");
            let mut system = well_known_sid(WinLocalSystemSid).expect("SYSTEM SID");
            // SAFETY: the aligned buffers contain the structures/SIDs written
            // by Windows and stay alive for all comparisons below.
            let user_sid = unsafe { (*(user.as_mut_ptr().cast::<TOKEN_USER>())).User.Sid };
            let administrators_sid = administrators.as_mut_ptr().cast::<c_void>();
            let system_sid = system.as_mut_ptr().cast::<c_void>();
            // SAFETY: both SIDs are valid and live.
            assert_ne!(
                unsafe { EqualSid(owner, user_sid) },
                0,
                "owner is not current user"
            );

            let mut expected = vec![user_sid, administrators_sid];
            // SAFETY: both SIDs are valid and live.
            if unsafe { EqualSid(user_sid, system_sid) } == 0 {
                expected.push(system_sid);
            }

            let mut entry_count = 0;
            let mut raw_entries = ptr::null_mut();
            // SAFETY: dacl is part of the live descriptor and both output
            // pointers refer to writable storage.
            let status =
                unsafe { GetExplicitEntriesFromAclW(dacl, &mut entry_count, &mut raw_entries) };
            assert_eq!(status, ERROR_SUCCESS, "read explicit DACL entries");
            assert!(!raw_entries.is_null(), "missing explicit DACL entries");
            let _entries = OwnedExplicitEntries(raw_entries);
            assert_eq!(entry_count as usize, expected.len(), "unexpected ACE count");
            // SAFETY: Windows returned an array of exactly entry_count items and
            // `_entries` owns it for the lifetime of this slice.
            let entries = unsafe { std::slice::from_raw_parts(raw_entries, entry_count as usize) };
            let mut seen = vec![false; expected.len()];
            for entry in entries {
                assert_eq!(
                    entry.grfAccessPermissions, FILE_ALL_ACCESS,
                    "ACE is not full control"
                );
                assert_eq!(
                    entry.grfAccessMode, GRANT_ACCESS,
                    "ACE is not allow/set access"
                );
                assert_eq!(
                    entry.Trustee.TrusteeForm, TRUSTEE_IS_SID,
                    "trustee is not a SID"
                );
                let actual_sid = entry.Trustee.ptstrName.cast::<c_void>();
                let index = expected
                    .iter()
                    .position(|expected_sid| {
                        // SAFETY: actual_sid points into the live explicit-entry
                        // allocation and every expected SID buffer is live.
                        unsafe { EqualSid(actual_sid, *expected_sid) != 0 }
                    })
                    .expect("unexpected trustee SID");
                assert_eq!(
                    entry.grfInheritance, expected_inheritance,
                    "wrong inheritance flags"
                );
                assert!(!seen[index], "duplicate trustee SID");
                seen[index] = true;
            }
            assert!(seen.into_iter().all(|value| value), "missing trustee SID");
        }
    }
}

#[cfg(not(any(unix, windows)))]
mod platform {
    use super::*;

    pub(super) fn secure(_path: &Path, _directory: bool) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> std::path::PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "audiohub-private-fs-{}-{nonce}",
            std::process::id()
        ))
    }

    #[test]
    fn secures_a_directory_and_existing_file() {
        let directory = scratch();
        secure_private_directory(&directory).expect("secure directory");
        let file = directory.join("secret");
        fs::write(&file, b"secret").expect("write file");
        secure_private_file(&file).expect("secure file");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&directory)
                    .expect("directory metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(&file)
                    .expect("file metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }

        fs::remove_dir_all(directory).expect("cleanup");
    }
}
