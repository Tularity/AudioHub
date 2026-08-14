//! Stable process contract for the Windows AudioHubVad installer helper.
//!
//! The helper deliberately accepts no filesystem path. `install` resolves the
//! driver package and daemon from the signed application layout, and the App
//! launches this executable with `ShellExecuteExW` and the `runas` verb.

use serde::Serialize;

/// `ShellExecuteExW(..., lpVerb = L"runas")` did not create a process because
/// the user cancelled the UAC consent/credential UI. This is a LAUNCH error,
/// never a helper exit code; callers must check it before waiting on hProcess.
pub const WIN32_ERROR_UAC_CANCELLED: u32 = 1223;

pub mod exit {
    /// The requested state-changing operation completed successfully. For an
    /// install report this means ready; for an uninstall report it means the
    /// AudioHub devnode and its published driver-store package are gone.
    pub const READY: u8 = 0;
    /// No AudioHubVad driver package is bound to the root device.
    pub const ABSENT: u8 = 20;
    /// A package is bound, but the device/control link is not usable.
    pub const INSTALLED_UNAVAILABLE: u8 = 21;
    /// Installation failed before a usable package was established.
    pub const INSTALL_FAILED: u8 = 22;
    /// The helper was not launched elevated.
    pub const NOT_ELEVATED: u8 = 23;
    /// The fixed application resource layout is missing or invalid.
    pub const PAYLOAD_INVALID: u8 = 24;
    /// Windows installed the package but requested a restart before use.
    pub const REBOOT_REQUIRED: u8 = 25;
    /// Driver removal failed and AudioHub must remain installed so the fixed
    /// helper can be retried safely.
    pub const REMOVE_FAILED: u8 = 26;
    /// This executable was built for a non-Windows host.
    pub const UNSUPPORTED_PLATFORM: u8 = 69;
    /// The command line was not one of the fixed public commands.
    pub const USAGE: u8 = 64;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DriverState {
    Absent,
    InstalledUnavailable,
    Ready,
    Removed,
    RebootRequired,
    InstallFailed,
    RemoveFailed,
    NotElevated,
    PayloadInvalid,
    UnsupportedPlatform,
    Usage,
}

impl DriverState {
    pub fn exit_code(self) -> u8 {
        match self {
            Self::Ready | Self::Removed => exit::READY,
            Self::Absent => exit::ABSENT,
            Self::InstalledUnavailable => exit::INSTALLED_UNAVAILABLE,
            Self::InstallFailed => exit::INSTALL_FAILED,
            Self::NotElevated => exit::NOT_ELEVATED,
            Self::PayloadInvalid => exit::PAYLOAD_INVALID,
            Self::RebootRequired => exit::REBOOT_REQUIRED,
            Self::RemoveFailed => exit::REMOVE_FAILED,
            Self::UnsupportedPlatform => exit::UNSUPPORTED_PLATFORM,
            Self::Usage => exit::USAGE,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DriverReport {
    pub operation: &'static str,
    pub state: DriverState,
    pub package_installed: bool,
    pub driver_available: bool,
    pub reboot_required: bool,
    pub matching_devices: u32,
    pub present_devices: u32,
    pub started_devices: u32,
    pub control_link_present: bool,
    pub problem_codes: Vec<u32>,
    pub daemon_image_configured: bool,
    pub expected_daemon_image: Option<String>,
    pub win32_error: Option<u32>,
    pub detail: String,
}

impl DriverReport {
    pub fn exit_code(&self) -> u8 {
        self.state.exit_code()
    }
}

/// Classify the only launch failure that represents an intentional user
/// decision. Every other Win32 error is an actual launch failure.
pub fn uac_was_cancelled(shell_execute_error: u32) -> bool {
    shell_execute_error == WIN32_ERROR_UAC_CANCELLED
}

#[cfg(windows)]
mod windows;

#[cfg(windows)]
pub use windows::{install, status, uninstall};

#[cfg(not(windows))]
pub fn status() -> DriverReport {
    unsupported("status")
}

#[cfg(not(windows))]
pub fn install() -> DriverReport {
    unsupported("install")
}

#[cfg(not(windows))]
pub fn uninstall() -> DriverReport {
    unsupported("uninstall")
}

#[cfg(not(windows))]
fn unsupported(operation: &'static str) -> DriverReport {
    DriverReport {
        operation,
        state: DriverState::UnsupportedPlatform,
        package_installed: false,
        driver_available: false,
        reboot_required: false,
        matching_devices: 0,
        present_devices: 0,
        started_devices: 0,
        control_link_present: false,
        problem_codes: Vec::new(),
        daemon_image_configured: false,
        expected_daemon_image: None,
        win32_error: None,
        detail: "AudioHubVad installation is available only on Windows".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_exit_codes_are_unique_and_u8_sized() {
        let states = [
            DriverState::Absent,
            DriverState::InstalledUnavailable,
            DriverState::Ready,
            DriverState::Removed,
            DriverState::RebootRequired,
            DriverState::InstallFailed,
            DriverState::RemoveFailed,
            DriverState::NotElevated,
            DriverState::PayloadInvalid,
            DriverState::UnsupportedPlatform,
            DriverState::Usage,
        ];
        let mut codes = states.map(DriverState::exit_code).to_vec();
        codes.sort_unstable();
        codes.dedup();
        // Ready and Removed are the two operation-specific spellings of
        // successful process exit 0. Every failure/observable status remains
        // distinct so the App and NSIS can branch without parsing text.
        assert_eq!(codes.len() + 1, states.len());
        assert_eq!(DriverState::Ready.exit_code(), 0);
        assert_eq!(DriverState::Removed.exit_code(), 0);
    }

    #[test]
    fn uac_cancel_is_a_launch_result_not_a_process_exit() {
        assert!(uac_was_cancelled(1223));
        assert!(!uac_was_cancelled(exit::INSTALL_FAILED.into()));
        assert!(![
            exit::READY,
            exit::ABSENT,
            exit::INSTALLED_UNAVAILABLE,
            exit::INSTALL_FAILED,
            exit::NOT_ELEVATED,
            exit::PAYLOAD_INVALID,
            exit::REBOOT_REQUIRED,
            exit::REMOVE_FAILED,
            exit::UNSUPPORTED_PLATFORM,
            exit::USAGE,
        ]
        .contains(&(WIN32_ERROR_UAC_CANCELLED as u8)));
    }

    #[test]
    fn report_serialises_stable_machine_readable_state() {
        let report = DriverReport {
            operation: "status",
            state: DriverState::InstalledUnavailable,
            package_installed: true,
            driver_available: false,
            reboot_required: false,
            matching_devices: 1,
            present_devices: 1,
            started_devices: 0,
            control_link_present: false,
            problem_codes: vec![52],
            daemon_image_configured: true,
            expected_daemon_image: Some(r"C:\Program Files\AudioHub\audiohubd.exe".into()),
            win32_error: None,
            detail: "driver package is installed but the device is unavailable".into(),
        };
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains(r#""state":"installed_unavailable""#));
        assert!(json.contains(r#""problem_codes":[52]"#));
    }
}
