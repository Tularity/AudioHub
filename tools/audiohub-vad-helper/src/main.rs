use std::process::ExitCode;

use audiohub_vad_helper::{DriverReport, DriverState};

fn usage() -> DriverReport {
    DriverReport {
        operation: "usage",
        state: DriverState::Usage,
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
        detail: "usage: audiohub-vad-helper.exe status|install|uninstall".into(),
    }
}

fn main() -> ExitCode {
    let mut args = std::env::args_os();
    let _program = args.next();
    let command = args.next().and_then(|v| v.into_string().ok());
    let extra = args.next().is_some();

    let report = if extra {
        usage()
    } else {
        match command.as_deref() {
            Some("status") | Some("--status") => audiohub_vad_helper::status(),
            Some("install") => audiohub_vad_helper::install(),
            Some("uninstall") => audiohub_vad_helper::uninstall(),
            _ => usage(),
        }
    };

    // One complete JSON object is the stdout contract. The elevated App path
    // normally consumes the exit code, then invokes unelevated `status` for
    // details; keeping stdout also makes build-only validation on 30-win easy.
    println!(
        "{}",
        serde_json::to_string(&report).expect("DriverReport is serialisable")
    );
    ExitCode::from(report.exit_code())
}
