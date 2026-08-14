//! Privileged installation of the virtual-audio driver bundled with the App.
//!
//! The WebView can request exactly one fixed operation. It cannot supply a
//! path, command, certificate, or shell fragment. Platform code resolves the
//! payload under Tauri's signed resource directory and verifies the result
//! after the OS installer returns.

use serde::Serialize;
use tauri::{AppHandle, Manager};

#[derive(Debug, Clone, Serialize)]
pub(crate) struct DriverInstallerStatus {
    pub platform: &'static str,
    pub supported: bool,
    pub bundled: bool,
    pub installed: bool,
    /// A Windows PnP operation requested a system restart and the same boot is
    /// still running. Persisted by the fixed helper, not inferred from HAL.
    pub reboot_required: bool,
    /// The bound Windows device pins the expected bundled daemon image. False
    /// on other platforms where that Windows-only integrity check is absent.
    pub daemon_image_configured: bool,
    /// absent, installed, installed_unavailable, reboot_required, ready, or unsupported.
    pub state: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct DriverInstallResult {
    pub state: &'static str,
    pub reboot_required: bool,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct DriverInstallError {
    pub(crate) kind: &'static str,
    pub(crate) message: String,
    pub(crate) detail: Option<String>,
}

#[cfg(target_os = "macos")]
fn mac_finish_admin_shell(
    result_present: bool,
    error: Option<(Option<i64>, Option<String>)>,
) -> Result<(), DriverInstallError> {
    if let Some((number, message)) = error {
        if number == Some(-128) {
            return Err(DriverInstallError::new(
                "user-cancelled",
                "System authorization was cancelled",
            ));
        }
        return Err(DriverInstallError::new(
            "install-failed",
            "macOS could not complete the authorized operation",
        )
        .with_detail(message.unwrap_or_else(|| format!("AppleScript error {number:?}"))));
    }
    if !result_present {
        return Err(DriverInstallError::new(
            "install-failed",
            "macOS authorization returned no result",
        ));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn mac_run_admin_shell_on_main(shell: &str) -> Result<(), DriverInstallError> {
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2::AnyThread;
    use objc2_foundation::{
        NSAppleEventDescriptor, NSAppleScript, NSDictionary, NSNumber, NSString,
    };

    fn apple_string(value: &str) -> String {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('\"', "\\\""))
    }

    fn apple_error(info: &NSDictionary<NSString, AnyObject>) -> (Option<i64>, Option<String>) {
        let number_key = NSString::from_str("NSAppleScriptErrorNumber");
        let message_key = NSString::from_str("NSAppleScriptErrorMessage");
        let number = info.objectForKey(&number_key).and_then(|value| {
            value
                .downcast_ref::<NSNumber>()
                .map(|n| n.integerValue() as i64)
        });
        let message = info.objectForKey(&message_key).and_then(|value| {
            value
                .downcast_ref::<NSString>()
                .map(|text| text.to_string())
        });
        (number, message)
    }

    let source = format!(
        "do shell script {} with administrator privileges",
        apple_string(shell)
    );
    let source = NSString::from_str(&source);
    let script =
        NSAppleScript::initWithSource(NSAppleScript::alloc(), &source).ok_or_else(|| {
            DriverInstallError::new("internal", "Could not prepare macOS authorization")
        })?;
    let mut error: Option<Retained<NSDictionary<NSString, AnyObject>>> = None;
    // Apple documents a nil result when execution fails and fills errorInfo.
    // The generated objc2-foundation 0.3.2 declaration incorrectly models the
    // return as nonnull, so calling it panics before we can inspect errorInfo.
    // Send the same selector with the real nullable ABI contract instead.
    //
    // SAFETY: `script` is a live NSAppleScript, the selector takes exactly one
    // NSDictionary** out-parameter, and both returned Cocoa objects are owned
    // through Retained. This runs on the AppKit main thread as required.
    let result: Option<Retained<NSAppleEventDescriptor>> =
        unsafe { objc2::msg_send![&script, executeAndReturnError: Some(&mut error)] };
    mac_finish_admin_shell(result.is_some(), error.as_deref().map(apple_error))
}

/// NSAppleScript is AppKit/Foundation UI and must be created and executed on
/// the macOS main thread. Driver and service installation themselves run on a
/// blocking worker, so marshal only the authorization sheet to the main loop
/// and wait for its small owned result here.
#[cfg(target_os = "macos")]
pub(crate) fn mac_run_admin_shell(app: &AppHandle, shell: &str) -> Result<(), DriverInstallError> {
    let (send, recv) = std::sync::mpsc::sync_channel(1);
    let shell = shell.to_owned();
    app.run_on_main_thread(move || {
        let _ = send.send(mac_run_admin_shell_on_main(&shell));
    })
    .map_err(|error| {
        DriverInstallError::new("internal", "Could not show macOS authorization")
            .with_detail(error.to_string())
    })?;
    recv.recv().map_err(|error| {
        DriverInstallError::new("internal", "macOS authorization did not return a result")
            .with_detail(error.to_string())
    })?
}

impl DriverInstallError {
    fn new(kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            detail: None,
        }
    }

    fn with_detail(mut self, detail: impl Into<String>) -> Self {
        let detail = detail.into();
        self.detail = (!detail.trim().is_empty()).then_some(detail);
        self
    }
}

#[cfg(all(test, target_os = "macos"))]
mod mac_authorization_tests {
    use super::*;

    #[test]
    fn nullable_apple_script_result_preserves_all_outcomes() {
        assert!(mac_finish_admin_shell(true, None).is_ok());

        let cancelled =
            mac_finish_admin_shell(false, Some((Some(-128), Some("User canceled".into()))))
                .expect_err("cancellation must remain actionable");
        assert_eq!(cancelled.kind, "user-cancelled");

        let failed = mac_finish_admin_shell(
            false,
            Some((Some(1), Some("privileged command failed".into()))),
        )
        .expect_err("AppleScript errorInfo must be returned instead of panicking");
        assert_eq!(failed.kind, "install-failed");
        assert_eq!(failed.detail.as_deref(), Some("privileged command failed"));

        let missing = mac_finish_admin_shell(false, None)
            .expect_err("nil without errorInfo is still a controlled failure");
        assert_eq!(missing.kind, "install-failed");
        assert!(missing.detail.is_none());
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use std::collections::BTreeSet;
    use std::ffi::CString;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{Duration, Instant};

    use super::*;

    const INSTALLED_DRIVER: &str = "/Library/Audio/Plug-Ins/HAL/AudioHubDriver.driver";
    const HAL_SERVICE_NAME: &str = "com.audiohub.driver";
    const COREAUDIOD_PROCESS_NAME: &str = "coreaudiod";
    const DRIVER_HELPER_PROCESS_NAME: &str = "com.apple.audio.Core-Audio-Driver-Service.helper";
    // Once the generic CoreAudio helper loads an AudioServerPlugIn, macOS can
    // rename its process to identify the hosted bundle. `ps -o comm=` then
    // reports this display name instead of the helper executable path/name.
    const AUDIOHUB_DRIVER_HOST_PROCESS_NAME: &str = "Core Audio Driver (AudioHubDriver.driver)";
    const PROCESS_SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(2);
    const EXPECTED_PKG_SHA256: &str = env!("AUDIOHUB_DRIVER_PKG_SHA256");
    const LIFECYCLE_LOCK: &str = "/var/run/com.audiohub.lifecycle.lock";
    const MAC_LIFECYCLE_LOCK_SHELL: &str = r#"lifecycle_fail() {
  echo "audiohub driver install: $*" >&2
  exit 1
}
lifecycle_path_exists() {
  [ -e "$1" ] || [ -L "$1" ]
}
lifecycle_lock_is_safe() {
  path="$1"
  [ -f "$path" ] && [ ! -L "$path" ] || return 1
  [ "$(/usr/bin/stat -f '%u' "$path" 2>/dev/null || true)" = 0 ] || return 1
  [ "$(/usr/bin/stat -f '%g' "$path" 2>/dev/null || true)" = 0 ] || return 1
  mode=$(/usr/bin/stat -f '%Lp' "$path" 2>/dev/null || true)
  case "$mode" in ''|*[!0-7]*) return 1;; esac
  [ $((0$mode)) -eq $((0600)) ]
}
acquire_lifecycle_lock() {
  if ! lifecycle_path_exists "$LIFECYCLE_LOCK"; then
    candidate=$(/usr/bin/mktemp "${LIFECYCLE_LOCK}.candidate.XXXXXX") \
      || lifecycle_fail 'could not prepare the AudioHub lifecycle lock'
    /usr/sbin/chown root:wheel "$candidate" \
      || { /bin/rm -f "$candidate"; lifecycle_fail 'could not own the AudioHub lifecycle lock'; }
    /bin/chmod 0600 "$candidate" \
      || { /bin/rm -f "$candidate"; lifecycle_fail 'could not protect the AudioHub lifecycle lock'; }
    lifecycle_lock_is_safe "$candidate" \
      || { /bin/rm -f "$candidate"; lifecycle_fail 'the new AudioHub lifecycle lock is unsafe'; }
    if ! /bin/ln "$candidate" "$LIFECYCLE_LOCK" 2>/dev/null \
      && ! lifecycle_path_exists "$LIFECYCLE_LOCK"; then
      /bin/rm -f "$candidate"
      lifecycle_fail 'could not publish the AudioHub lifecycle lock'
    fi
    /bin/rm -f "$candidate" \
      || lifecycle_fail 'could not finish preparing the AudioHub lifecycle lock'
  fi
  lifecycle_lock_is_safe "$LIFECYCLE_LOCK" \
    || lifecycle_fail 'the AudioHub lifecycle lock is a symlink or has unsafe metadata'
  lock_object_id=$(/usr/bin/stat -f '%i' "$LIFECYCLE_LOCK" 2>/dev/null || true)
  [ -n "$lock_object_id" ] || lifecycle_fail 'could not identify the AudioHub lifecycle lock'
  exec 9<> "$LIFECYCLE_LOCK" \
    || lifecycle_fail 'could not open the AudioHub lifecycle lock'
  lock_fd_object_id=$(/usr/bin/stat -f '%i' /dev/fd/9 2>/dev/null || true)
  [ "$lock_fd_object_id" = "$lock_object_id" ] \
    || lifecycle_fail 'the AudioHub lifecycle lock changed while opening it'
  /usr/bin/lockf -s -t 0 9 \
    || lifecycle_fail 'another AudioHub install or uninstall operation is already running'
  lifecycle_lock_is_safe "$LIFECYCLE_LOCK" \
    && [ "$(/usr/bin/stat -f '%i' "$LIFECYCLE_LOCK" 2>/dev/null || true)" = "$lock_object_id" ] \
    || lifecycle_fail 'the AudioHub lifecycle lock changed after it was acquired'
}
acquire_lifecycle_lock"#;

    fn sha256(path: &Path) -> Result<String, DriverInstallError> {
        let output = Command::new("/usr/bin/shasum")
            .args(["-a", "256"])
            .arg(path)
            .output()
            .map_err(|e| {
                DriverInstallError::new(
                    "payload-invalid",
                    "Could not verify the bundled driver package",
                )
                .with_detail(e.to_string())
            })?;
        if !output.status.success() {
            return Err(DriverInstallError::new(
                "payload-invalid",
                "Could not verify the bundled driver package",
            )
            .with_detail(String::from_utf8_lossy(&output.stderr).into_owned()));
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase())
    }

    fn payload(app: &AppHandle) -> Result<PathBuf, DriverInstallError> {
        let resources = app.path().resource_dir().map_err(|e| {
            DriverInstallError::new("payload-missing", "AudioHub driver package is unavailable")
                .with_detail(e.to_string())
        })?;
        let resources = std::fs::canonicalize(&resources).map_err(|e| {
            DriverInstallError::new(
                "payload-missing",
                "AudioHub resource directory is unavailable",
            )
            .with_detail(e.to_string())
        })?;
        let pkg =
            std::fs::canonicalize(resources.join("installer/AudioHubDriver.pkg")).map_err(|e| {
                DriverInstallError::new(
                    "payload-missing",
                    "AudioHub does not contain its driver package",
                )
                .with_detail(e.to_string())
            })?;
        if !pkg.starts_with(&resources)
            || pkg.file_name().and_then(|v| v.to_str()) != Some("AudioHubDriver.pkg")
            || !pkg.is_file()
        {
            return Err(DriverInstallError::new(
                "payload-invalid",
                "AudioHub refused an invalid driver package path",
            ));
        }
        // The package digest is embedded in audiohub-app while compiling, then
        // this exact file is copied into the bundle before the outer signature
        // seals Resources. A same-user replacement in a writable .app cannot
        // turn AudioHub's familiar authorization prompt into a root execution
        // primitive.
        if EXPECTED_PKG_SHA256.len() != 64 || sha256(&pkg)? != EXPECTED_PKG_SHA256 {
            return Err(DriverInstallError::new(
                "payload-invalid",
                "The bundled driver package does not match this AudioHub build",
            ));
        }
        let check = Command::new("/usr/sbin/pkgutil")
            .args(["--payload-files"])
            .arg(&pkg)
            .output()
            .map_err(|e| {
                DriverInstallError::new("payload-invalid", "Could not inspect the driver package")
                    .with_detail(e.to_string())
            })?;
        let listing = String::from_utf8_lossy(&check.stdout);
        if !check.status.success()
            || !listing.contains("AudioHubDriver.driver/Contents/MacOS/AudioHubDriver")
        {
            return Err(DriverInstallError::new(
                "payload-invalid",
                "The bundled driver package failed verification",
            )
            .with_detail(String::from_utf8_lossy(&check.stderr).into_owned()));
        }
        Ok(pkg)
    }

    fn shell_quote(path: &Path) -> String {
        format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
    }

    fn driver_install_shell(pkg: &Path) -> String {
        // Re-copy and re-hash after elevation. The authorization sheet gives a
        // same-user process time to replace a writable App resource, so the
        // preflight hash alone is not a privilege boundary. `mktemp -d` is
        // root-owned/0700, and installer reads only that immutable private copy.
        // The lifecycle lock is acquired before that first machine mutation
        // and remains held through PackageKit's complete transaction.
        format!(
            "set -eu\n\
             LIFECYCLE_LOCK='{lifecycle_lock}'\n\
             {MAC_LIFECYCLE_LOCK_SHELL}\n\
             d=$(/usr/bin/mktemp -d /private/tmp/audiohub-driver.XXXXXX); \
             trap '/bin/rm -rf \"$d\"' EXIT; \
             /bin/cp -p {source} \"$d/AudioHubDriver.pkg\"; \
             /usr/sbin/chown root:wheel \"$d/AudioHubDriver.pkg\"; \
             /bin/chmod 0600 \"$d/AudioHubDriver.pkg\"; \
             got=$(/usr/bin/shasum -a 256 \"$d/AudioHubDriver.pkg\" | /usr/bin/awk '{{print $1}}'); \
             [ \"$got\" = {digest} ]; \
             /usr/sbin/installer -pkg \"$d/AudioHubDriver.pkg\" -target /",
            source = shell_quote(pkg),
            digest = EXPECTED_PKG_SHA256,
            lifecycle_lock = LIFECYCLE_LOCK,
        )
    }

    fn install_pkg(app: &AppHandle, pkg: &Path) -> Result<(), DriverInstallError> {
        let shell = driver_install_shell(pkg);
        // NSAppleScript executes in-process, so the authorization sheet is
        // attributed to AudioHub instead of a generic osascript helper.
        mac_run_admin_shell(app, &shell)
    }

    pub(super) fn installed_valid() -> bool {
        let driver = Path::new(INSTALLED_DRIVER);
        driver.join("Contents/MacOS/AudioHubDriver").is_file()
            && Command::new("/usr/bin/codesign")
                .args(["--verify", "--deep", "--strict"])
                .arg(driver)
                .status()
                .map(|status| status.success())
                .unwrap_or(false)
    }

    type MachPort = u32;
    type KernReturn = i32;
    const MACH_PORT_NULL: MachPort = 0;
    const KERN_SUCCESS: KernReturn = 0;
    const TASK_BOOTSTRAP_PORT: i32 = 4;
    const BOOTSTRAP_SUCCESS: KernReturn = 0;
    const BOOTSTRAP_UNKNOWN_SERVICE: KernReturn = 1102;

    extern "C" {
        static mach_task_self_: MachPort;
        fn task_get_special_port(task: MachPort, which: i32, port: *mut MachPort) -> KernReturn;
        fn bootstrap_look_up(
            bootstrap: MachPort,
            name: *const i8,
            service: *mut MachPort,
        ) -> KernReturn;
        fn mach_port_deallocate(task: MachPort, name: MachPort) -> KernReturn;
    }

    /// Use the same bootstrap lookup as audiohubd's HAL bridge. A successful
    /// PackageKit transaction means only that the files are in place; the
    /// AudioServerPlugIn helper publishes this name later, after coreaudiod has
    /// restarted and loaded the bundle.
    fn hal_service_published() -> Result<bool, String> {
        let name = CString::new(HAL_SERVICE_NAME).map_err(|error| error.to_string())?;
        let task = unsafe { mach_task_self_ };
        let mut bootstrap = MACH_PORT_NULL;
        let kr = unsafe { task_get_special_port(task, TASK_BOOTSTRAP_PORT, &mut bootstrap) };
        if kr != KERN_SUCCESS || bootstrap == MACH_PORT_NULL {
            return Err(format!("task_get_special_port(bootstrap) failed: {kr}"));
        }
        let mut service = MACH_PORT_NULL;
        let lookup = unsafe { bootstrap_look_up(bootstrap, name.as_ptr(), &mut service) };
        unsafe {
            let _ = mach_port_deallocate(task, bootstrap);
        }
        // A failed lookup normally leaves service null, but the MIG contract
        // does not make a non-null out value ours forever only on success. Drop
        // any right returned by the call before classifying its status.
        if service != MACH_PORT_NULL {
            unsafe {
                let _ = mach_port_deallocate(task, service);
            }
        }
        match lookup {
            BOOTSTRAP_SUCCESS if service != MACH_PORT_NULL => Ok(true),
            BOOTSTRAP_SUCCESS => Err(format!(
                "bootstrap_look_up('{HAL_SERVICE_NAME}') returned a null service"
            )),
            BOOTSTRAP_UNKNOWN_SERVICE => Ok(false),
            _ => Err(format!(
                "bootstrap_look_up('{HAL_SERVICE_NAME}') failed: {lookup}"
            )),
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(super) struct CoreAudioProcessSnapshot {
        coreaudiod: BTreeSet<u32>,
        driver_helpers: BTreeSet<u32>,
    }

    fn process_name(command: &str) -> &str {
        command.rsplit('/').next().unwrap_or(command)
    }

    fn is_audiohub_driver_helper(command: &str) -> bool {
        matches!(
            process_name(command),
            DRIVER_HELPER_PROCESS_NAME | AUDIOHUB_DRIVER_HOST_PROCESS_NAME
        )
    }

    fn parse_coreaudio_process_snapshot(stdout: &str) -> Result<CoreAudioProcessSnapshot, String> {
        let mut snapshot = CoreAudioProcessSnapshot {
            coreaudiod: BTreeSet::new(),
            driver_helpers: BTreeSet::new(),
        };
        for (index, line) in stdout.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let split = line
                .find(char::is_whitespace)
                .ok_or_else(|| format!("ps line {} has no command", index + 1))?;
            let pid = line[..split]
                .parse::<u32>()
                .ok()
                .filter(|pid| *pid > 0)
                .ok_or_else(|| format!("ps line {} has an invalid pid", index + 1))?;
            let command = line[split..].trim();
            if command.is_empty() {
                return Err(format!("ps line {} has an empty command", index + 1));
            }
            if process_name(command) == COREAUDIOD_PROCESS_NAME {
                snapshot.coreaudiod.insert(pid);
            } else if is_audiohub_driver_helper(command) {
                snapshot.driver_helpers.insert(pid);
            }
        }
        Ok(snapshot)
    }

    /// Capture both CoreAudio generations from one bounded process-table read.
    /// Using separate pgrep calls permits a restart between reads and silently
    /// treating a failed query as "no process" can make an old helper look new.
    pub(super) fn coreaudio_process_snapshot() -> Result<CoreAudioProcessSnapshot, String> {
        let mut command = Command::new("/bin/ps");
        command
            .args(["-axww", "-o", "pid=", "-o", "comm="])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let output = crate::command_output_with_timeout(&mut command, PROCESS_SNAPSHOT_TIMEOUT)
            .map_err(|error| {
                format!("could not query the CoreAudio process generation: {error}")
            })?;
        if !output.status.success() {
            return Err(format!(
                "CoreAudio process query exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let stdout = String::from_utf8(output.stdout)
            .map_err(|error| format!("CoreAudio process query returned invalid UTF-8: {error}"))?;
        parse_coreaudio_process_snapshot(&stdout)
    }

    fn coreaudio_generation_pending(
        previous: &CoreAudioProcessSnapshot,
        current: &CoreAudioProcessSnapshot,
    ) -> Option<String> {
        if current.coreaudiod.is_empty() {
            return Some("CoreAudio is not running yet".to_owned());
        }
        let stale_coreaudiod = previous
            .coreaudiod
            .intersection(&current.coreaudiod)
            .copied()
            .collect::<Vec<_>>();
        if !stale_coreaudiod.is_empty() {
            return Some(format!(
                "pre-install coreaudiod pid(s) are still running: {stale_coreaudiod:?}"
            ));
        }
        let stale_helpers = previous
            .driver_helpers
            .intersection(&current.driver_helpers)
            .copied()
            .collect::<Vec<_>>();
        if !stale_helpers.is_empty() {
            return Some(format!(
                "pre-install Core Audio driver helper pid(s) are still running: {stale_helpers:?}"
            ));
        }
        if current.driver_helpers.is_empty() {
            return Some("the replacement Core Audio driver helper is not running yet".to_owned());
        }
        None
    }

    pub(super) fn wait_for_driver_service(
        previous: &CoreAudioProcessSnapshot,
        timeout: Duration,
    ) -> Result<(), DriverInstallError> {
        let deadline = Instant::now() + timeout;
        loop {
            let current = coreaudio_process_snapshot().map_err(|error| {
                DriverInstallError::new(
                    "installed-unavailable",
                    "The driver installed, but AudioHub could not inspect CoreAudio",
                )
                .with_detail(error)
            })?;
            let last = if let Some(pending) = coreaudio_generation_pending(previous, &current) {
                pending
            } else {
                match hal_service_published() {
                    Ok(true) => return Ok(()),
                    Ok(false) => format!(
                            "CoreAudio and its driver helper restarted as pid(s) {:?} and {:?}, but Mach service '{HAL_SERVICE_NAME}' is not published",
                            current.coreaudiod, current.driver_helpers
                        ),
                    Err(error) => error,
                }
            };
            if Instant::now() >= deadline {
                return Err(DriverInstallError::new(
                    "installed-unavailable",
                    "The driver installed, but CoreAudio did not load it",
                )
                .with_detail(last));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    pub(super) fn preflight(app: &AppHandle) -> Result<(), DriverInstallError> {
        payload(app).map(|_| ())
    }

    pub(super) fn status(app: &AppHandle) -> DriverInstallerStatus {
        let bundled = payload(app).is_ok();
        let installed = installed_valid();
        DriverInstallerStatus {
            platform: "macos",
            supported: true,
            bundled,
            installed,
            reboot_required: false,
            daemon_image_configured: false,
            state: if installed { "installed" } else { "absent" },
        }
    }

    pub(super) fn install(app: &AppHandle) -> Result<DriverInstallResult, DriverInstallError> {
        let pkg = payload(app)?;
        install_pkg(app, &pkg)?;
        if !installed_valid() {
            return Err(DriverInstallError::new(
                "installed-unavailable",
                "The package completed, but the AudioHub driver is not usable",
            ));
        }
        Ok(DriverInstallResult {
            state: "installed",
            reboot_required: false,
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn authorized_driver_install_uses_the_shared_lock_before_mutation() {
            let shell = driver_install_shell(Path::new("/Applications/AudioHub.app/driver.pkg"));
            assert!(shell.contains(&format!("LIFECYCLE_LOCK='{LIFECYCLE_LOCK}'")));
            assert!(shell.contains("[ ! -L \"$path\" ]"));
            assert!(shell.contains("stat -f '%u'"));
            assert!(shell.contains("stat -f '%g'"));
            assert!(shell.contains("$((0600))"));
            assert!(shell.contains("exec 9<> \"$LIFECYCLE_LOCK\""));
            let lock = shell
                .find("/usr/bin/lockf -s -t 0 9")
                .expect("shared lifecycle lock");
            let stage = shell
                .find("mktemp -d /private/tmp/audiohub-driver")
                .expect("private driver stage");
            let packagekit = shell
                .find("/usr/sbin/installer -pkg")
                .expect("PackageKit invocation");
            assert!(lock < stage, "the lock must precede private staging");
            assert!(stage < packagekit, "staging must precede PackageKit");
        }

        #[test]
        fn process_snapshot_separates_coreaudio_generations_from_one_listing() {
            let snapshot = parse_coreaudio_process_snapshot(
                "  123 /usr/sbin/coreaudiod\n\
                 456 /System/Library/Frameworks/CoreAudio.framework/XPCServices/com.apple.audio.Core-Audio-Driver-Service.helper\n\
                 457 Core Audio Driver (AudioHubDriver.driver)\n\
                 789 /Applications/Unrelated App.app/Contents/MacOS/Unrelated App\n\
                 654 com.apple.audio.Core-Audio-Driver-Service.helper\n\
                 655 Core Audio Driver (OtherDriver.driver)\n",
            )
            .expect("valid ps snapshot");
            assert_eq!(snapshot.coreaudiod, BTreeSet::from([123]));
            assert_eq!(snapshot.driver_helpers, BTreeSet::from([456, 457, 654]));
        }

        #[test]
        fn renamed_audiohub_driver_host_participates_in_generation_checks() {
            let before = parse_coreaudio_process_snapshot(
                "22560 /usr/sbin/coreaudiod\n\
                 22611 Core Audio Driver (AudioHubDriver.driver)\n",
            )
            .expect("pre-install ps snapshot");
            let stale = parse_coreaudio_process_snapshot(
                "22700 /usr/sbin/coreaudiod\n\
                 22611 Core Audio Driver (AudioHubDriver.driver)\n",
            )
            .expect("stale post-install ps snapshot");
            assert!(coreaudio_generation_pending(&before, &stale).is_some());

            let replacement = parse_coreaudio_process_snapshot(
                "22700 /usr/sbin/coreaudiod\n\
                 22701 Core Audio Driver (AudioHubDriver.driver)\n",
            )
            .expect("replacement post-install ps snapshot");
            assert_eq!(coreaudio_generation_pending(&before, &replacement), None);
        }

        #[test]
        fn malformed_process_snapshot_is_an_error() {
            assert!(parse_coreaudio_process_snapshot("not-a-pid /usr/sbin/coreaudiod\n").is_err());
            assert!(parse_coreaudio_process_snapshot("123\n").is_err());
        }

        #[test]
        fn readiness_requires_new_coreaudiod_and_helper_generations() {
            let before = CoreAudioProcessSnapshot {
                coreaudiod: BTreeSet::from([123]),
                driver_helpers: BTreeSet::from([456]),
            };
            let same_core = CoreAudioProcessSnapshot {
                coreaudiod: BTreeSet::from([123]),
                driver_helpers: BTreeSet::from([789]),
            };
            assert!(coreaudio_generation_pending(&before, &same_core).is_some());

            let stale_helper = CoreAudioProcessSnapshot {
                coreaudiod: BTreeSet::from([124]),
                driver_helpers: BTreeSet::from([456, 789]),
            };
            assert!(coreaudio_generation_pending(&before, &stale_helper).is_some());

            let no_helper = CoreAudioProcessSnapshot {
                coreaudiod: BTreeSet::from([124]),
                driver_helpers: BTreeSet::new(),
            };
            assert!(coreaudio_generation_pending(&before, &no_helper).is_some());

            let ready = CoreAudioProcessSnapshot {
                coreaudiod: BTreeSet::from([124]),
                driver_helpers: BTreeSet::from([789]),
            };
            assert_eq!(coreaudio_generation_pending(&before, &ready), None);

            let empty_before = CoreAudioProcessSnapshot {
                coreaudiod: BTreeSet::new(),
                driver_helpers: BTreeSet::new(),
            };
            assert_eq!(coreaudio_generation_pending(&empty_before, &ready), None);
        }
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use std::fs::{File, OpenOptions};
    use std::io::{Read, Seek, SeekFrom};
    use std::mem::size_of;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::OpenOptionsExt;
    use std::path::{Path, PathBuf};

    use audiohub_vad_helper::{exit, DriverReport, DriverState, WIN32_ERROR_UAC_CANCELLED};
    use sha2::{Digest, Sha256};
    use windows::core::{HRESULT, PCWSTR};
    use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
    use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE};
    use windows::Win32::UI::Shell::{
        ShellExecuteExW, SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS,
        SHELLEXECUTEINFOW,
    };
    use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;

    use super::*;

    const DRIVER_RESOURCE_DIR: &str = "windows-driver";
    const HELPER_NAME: &str = "audiohub-vad-helper.exe";
    const DRIVER_FILES: [&str; 3] = ["AudioHubVad.inf", "AudioHubVad.sys", "AudioHubVad.cat"];
    const FILE_SHARE_READ_ONLY: u32 = 0x0000_0001;
    const EXPECTED_HELPER_SHA256: &str = env!("AUDIOHUB_WIN_HELPER_SHA256");
    const EXPECTED_INF_SHA256: &str = env!("AUDIOHUB_WIN_INF_SHA256");
    const EXPECTED_SYS_SHA256: &str = env!("AUDIOHUB_WIN_SYS_SHA256");
    const EXPECTED_CAT_SHA256: &str = env!("AUDIOHUB_WIN_CAT_SHA256");
    const EXPECTED_DAEMON_SHA256: &str = env!("AUDIOHUB_WIN_DAEMON_SHA256");

    struct ProcessHandle(HANDLE);

    impl Drop for ProcessHandle {
        fn drop(&mut self) {
            if !self.0 .0.is_null() {
                // SAFETY: ShellExecuteExW returned this owned process handle
                // because SEE_MASK_NOCLOSEPROCESS was set. This Drop runs once.
                let _ = unsafe { CloseHandle(self.0) };
            }
        }
    }

    struct Payload {
        helper: PathBuf,
        // Keep every verified image/package handle open through UAC and until
        // the elevated helper exits. FILE_SHARE_READ lets the loader/SetupAPI
        // consume them but denies write/delete/rename, closing the prompt-time
        // replacement race.
        _locks: Vec<File>,
    }

    fn valid_digest(value: &str) -> bool {
        value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
    }

    fn digest(file: &mut File) -> Result<String, DriverInstallError> {
        file.seek(SeekFrom::Start(0)).map_err(|error| {
            DriverInstallError::new("payload-invalid", "Could not verify driver resources")
                .with_detail(error.to_string())
        })?;
        let mut hash = Sha256::new();
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let read = file.read(&mut buffer).map_err(|error| {
                DriverInstallError::new("payload-invalid", "Could not verify driver resources")
                    .with_detail(error.to_string())
            })?;
            if read == 0 {
                break;
            }
            hash.update(&buffer[..read]);
        }
        file.seek(SeekFrom::Start(0)).map_err(|error| {
            DriverInstallError::new("payload-invalid", "Could not verify driver resources")
                .with_detail(error.to_string())
        })?;
        Ok(format!("{:x}", hash.finalize()))
    }

    fn reject_writable_install(directory: &Path) -> Result<(), DriverInstallError> {
        let probe = directory.join(format!(
            ".audiohub-write-probe-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        match OpenOptions::new().write(true).create_new(true).open(&probe) {
            Ok(_) => {
                let _ = std::fs::remove_file(&probe);
                Err(DriverInstallError::new(
                    "payload-invalid",
                    "AudioHub must be installed in a protected system location before installing its driver",
                ))
            }
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => Ok(()),
            Err(error) => Err(DriverInstallError::new(
                "payload-invalid",
                "Could not verify AudioHub installation permissions",
            )
            .with_detail(error.to_string())),
        }
    }

    fn payload(app: &AppHandle) -> Result<Payload, DriverInstallError> {
        let resources = app.path().resource_dir().map_err(|error| {
            DriverInstallError::new(
                "payload-missing",
                "AudioHub resource directory is unavailable",
            )
            .with_detail(error.to_string())
        })?;
        let resources = std::fs::canonicalize(&resources).map_err(|error| {
            DriverInstallError::new(
                "payload-missing",
                "AudioHub resource directory is unavailable",
            )
            .with_detail(error.to_string())
        })?;
        reject_writable_install(&resources)?;
        let driver_dir =
            std::fs::canonicalize(resources.join(DRIVER_RESOURCE_DIR)).map_err(|error| {
                DriverInstallError::new(
                    "payload-missing",
                    "AudioHub does not contain its Windows driver package",
                )
                .with_detail(error.to_string())
            })?;
        if driver_dir.parent() != Some(resources.as_path())
            || driver_dir.file_name().and_then(|name| name.to_str()) != Some(DRIVER_RESOURCE_DIR)
        {
            return Err(DriverInstallError::new(
                "payload-invalid",
                "AudioHub refused an invalid Windows driver resource path",
            ));
        }

        let expected = [
            (HELPER_NAME, EXPECTED_HELPER_SHA256),
            (DRIVER_FILES[0], EXPECTED_INF_SHA256),
            (DRIVER_FILES[1], EXPECTED_SYS_SHA256),
            (DRIVER_FILES[2], EXPECTED_CAT_SHA256),
        ];
        let mut helper = None;
        let mut locks = Vec::with_capacity(5);
        for (name, expected_digest) in expected {
            let (path, lock) = checked_file(&driver_dir, name, expected_digest)?;
            if name == HELPER_NAME {
                helper = Some(path);
            }
            locks.push(lock);
        }
        let (_daemon, daemon_lock) =
            checked_file(&resources, "audiohubd.exe", EXPECTED_DAEMON_SHA256)?;
        locks.push(daemon_lock);
        Ok(Payload {
            helper: helper.expect("fixed helper entry is present"),
            _locks: locks,
        })
    }

    fn checked_file(
        directory: &Path,
        name: &str,
        expected_digest: &str,
    ) -> Result<(PathBuf, File), DriverInstallError> {
        if !valid_digest(expected_digest) {
            return Err(DriverInstallError::new(
                "payload-invalid",
                "This AudioHub build has no trusted Windows driver manifest",
            ));
        }
        let path = std::fs::canonicalize(directory.join(name)).map_err(|error| {
            DriverInstallError::new(
                "payload-missing",
                format!("AudioHub driver resource is missing: {name}"),
            )
            .with_detail(error.to_string())
        })?;
        let valid = path.parent() == Some(directory)
            && path.file_name().and_then(|value| value.to_str()) == Some(name)
            && path
                .metadata()
                .map(|metadata| metadata.is_file() && metadata.len() != 0)
                .unwrap_or(false);
        if !valid {
            return Err(DriverInstallError::new(
                "payload-invalid",
                format!("AudioHub refused an invalid driver resource: {name}"),
            ));
        }
        let mut lock = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ_ONLY)
            .open(&path)
            .map_err(|error| {
                DriverInstallError::new(
                    "payload-invalid",
                    format!("AudioHub could not lock driver resource: {name}"),
                )
                .with_detail(error.to_string())
            })?;
        if digest(&mut lock)? != expected_digest {
            return Err(DriverInstallError::new(
                "payload-invalid",
                format!("AudioHub driver resource does not match this build: {name}"),
            ));
        }
        Ok((path, lock))
    }

    fn status_state(report: &DriverReport) -> &'static str {
        match report.state {
            DriverState::Ready => "ready",
            DriverState::RebootRequired => "reboot_required",
            DriverState::InstalledUnavailable => "installed_unavailable",
            DriverState::Absent => "absent",
            _ if report.package_installed => "installed_unavailable",
            _ => "absent",
        }
    }

    pub(super) fn status(app: &AppHandle) -> DriverInstallerStatus {
        let report = audiohub_vad_helper::status();
        DriverInstallerStatus {
            platform: "windows",
            supported: true,
            bundled: payload(app).is_ok(),
            installed: report.package_installed,
            reboot_required: report.reboot_required || report.state == DriverState::RebootRequired,
            daemon_image_configured: report.daemon_image_configured,
            state: status_state(&report),
        }
    }

    fn wide(value: &std::ffi::OsStr) -> Vec<u16> {
        value.encode_wide().chain(std::iter::once(0)).collect()
    }

    fn run_elevated(helper: &Path) -> Result<u32, DriverInstallError> {
        let verb = wide(std::ffi::OsStr::new("runas"));
        let file = wide(helper.as_os_str());
        let parameters = wide(std::ffi::OsStr::new("install"));
        let directory = helper.parent().map(Path::as_os_str).map(wide);

        let mut execute = SHELLEXECUTEINFOW {
            cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
            lpVerb: PCWSTR::from_raw(verb.as_ptr()),
            lpFile: PCWSTR::from_raw(file.as_ptr()),
            lpParameters: PCWSTR::from_raw(parameters.as_ptr()),
            lpDirectory: directory
                .as_ref()
                .map(|value| PCWSTR::from_raw(value.as_ptr()))
                .unwrap_or_else(PCWSTR::null),
            nShow: SW_HIDE.0,
            ..Default::default()
        };

        // SAFETY: all PCWSTR buffers above remain alive through this blocking
        // call. No caller-controlled path, argument or verb reaches the shell.
        if let Err(error) = unsafe { ShellExecuteExW(&mut execute) } {
            if error.code() == HRESULT::from_win32(WIN32_ERROR_UAC_CANCELLED) {
                return Err(DriverInstallError::new(
                    "user-cancelled",
                    "Driver installation was cancelled",
                ));
            }
            return Err(DriverInstallError::new(
                "install-failed",
                "Windows could not start the AudioHub driver installer",
            )
            .with_detail(error.to_string()));
        }
        if execute.hProcess.0.is_null() {
            return Err(DriverInstallError::new(
                "install-failed",
                "Windows started no driver installer process",
            ));
        }
        let process = ProcessHandle(execute.hProcess);
        // SAFETY: process is a valid owned process handle and remains alive
        // until after both the wait and exit-code query.
        let waited = unsafe { WaitForSingleObject(process.0, INFINITE) };
        if waited != WAIT_OBJECT_0 {
            return Err(DriverInstallError::new(
                "install-failed",
                "Waiting for the Windows driver installer failed",
            )
            .with_detail(format!("WaitForSingleObject returned 0x{:08x}", waited.0)));
        }
        let mut code = 0u32;
        // SAFETY: the process is signalled, and code points to writable u32.
        unsafe { GetExitCodeProcess(process.0, &mut code) }.map_err(|error| {
            DriverInstallError::new(
                "install-failed",
                "Windows did not return the driver installer result",
            )
            .with_detail(error.to_string())
        })?;
        Ok(code)
    }

    fn unavailable(report: &DriverReport) -> DriverInstallError {
        let mut error = DriverInstallError::new(
            "installed-unavailable",
            "The AudioHub driver package is installed but not usable",
        );
        let mut detail = report.detail.clone();
        if !report.problem_codes.is_empty() {
            detail.push_str(&format!("; problem codes: {:?}", report.problem_codes));
        }
        if let Some(code) = report.win32_error {
            detail.push_str(&format!("; Win32 error: {code}"));
        }
        error = error.with_detail(detail);
        error
    }

    pub(super) fn install(app: &AppHandle) -> Result<DriverInstallResult, DriverInstallError> {
        let payload = payload(app)?;
        let code = run_elevated(&payload.helper)?;
        let report = audiohub_vad_helper::status();
        match code {
            code if code == u32::from(exit::READY) => {
                if report.state != DriverState::Ready {
                    return Err(unavailable(&report));
                }
                Ok(DriverInstallResult {
                    state: "installed",
                    reboot_required: false,
                })
            }
            code if code == u32::from(exit::REBOOT_REQUIRED) => {
                if !report.package_installed {
                    return Err(DriverInstallError::new(
                        "install-failed",
                        "Windows requested a restart but no AudioHub driver package is installed",
                    )
                    .with_detail(report.detail));
                }
                Ok(DriverInstallResult {
                    state: "installed",
                    reboot_required: true,
                })
            }
            code if code == u32::from(exit::INSTALLED_UNAVAILABLE) => Err(unavailable(&report)),
            code if code == u32::from(exit::PAYLOAD_INVALID) => Err(DriverInstallError::new(
                "payload-invalid",
                "The bundled Windows driver package failed verification",
            )
            .with_detail(report.detail)),
            code if code == u32::from(exit::NOT_ELEVATED) => Err(DriverInstallError::new(
                "install-failed",
                "The Windows driver installer did not receive administrator access",
            )),
            other => Err(DriverInstallError::new(
                "install-failed",
                "Windows could not install the AudioHub driver",
            )
            .with_detail(format!(
                "helper exit code {other}; post-install state: {}; {}",
                status_state(&report),
                report.detail
            ))),
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod platform {
    use super::*;

    pub(super) fn status(_app: &AppHandle) -> DriverInstallerStatus {
        DriverInstallerStatus {
            platform: "other",
            supported: false,
            bundled: false,
            installed: false,
            reboot_required: false,
            daemon_image_configured: false,
            state: "unsupported",
        }
    }

    pub(super) fn install(_app: &AppHandle) -> Result<DriverInstallResult, DriverInstallError> {
        Err(DriverInstallError::new(
            "unsupported",
            "Virtual driver installation is not supported on this platform",
        ))
    }
}

#[tauri::command]
pub(crate) async fn driver_installer_status(app: AppHandle) -> DriverInstallerStatus {
    let fallback = DriverInstallerStatus {
        platform: std::env::consts::OS,
        supported: cfg!(any(target_os = "macos", target_os = "windows")),
        bundled: false,
        installed: false,
        reboot_required: false,
        daemon_image_configured: false,
        state: "unsupported",
    };
    tauri::async_runtime::spawn_blocking(move || platform::status(&app))
        .await
        .unwrap_or(fallback)
}

#[tauri::command]
pub(crate) async fn install_driver(
    app: AppHandle,
) -> Result<DriverInstallResult, DriverInstallError> {
    let worker_app = app.clone();
    let result =
        match tauri::async_runtime::spawn_blocking(move || install_driver_blocking(&worker_app))
            .await
        {
            Ok(result) => result,
            Err(error) => Err(DriverInstallError::new(
                "internal",
                format!("driver install task failed: {error}"),
            )),
        };
    if let Err(error) = &result {
        super::warn(&driver_install_diagnostic(error));
    }
    result
}

#[cfg(target_os = "macos")]
fn install_driver_blocking(app: &AppHandle) -> Result<DriverInstallResult, DriverInstallError> {
    // Validate the sealed payload before touching a healthy daemon. `install`
    // repeats this check after authorization and root hashes its own private
    // snapshot again, so this is an availability optimization, not the security
    // boundary.
    platform::preflight(app)?;
    // Hold the user-session lifecycle lock across the complete generation
    // transition. Public start/stop entry points acquire this same lock, so use
    // their explicit unlocked variants while this guard is alive.
    let lifecycle_guard = super::service::lock_mac_lifecycle().map_err(|error| {
        DriverInstallError::new(
            "service-conflict",
            "AudioHub could not reserve its service lifecycle for driver installation",
        )
        .with_detail(error.detail.unwrap_or(error.message))
    })?;
    // Snapshot both CoreAudio process generations before stopping the daemon.
    // A failed process query is not equivalent to an empty generation and must
    // not leave a previously healthy service stopped.
    let coreaudio_before = platform::coreaudio_process_snapshot().map_err(|error| {
        DriverInstallError::new(
            "install-failed",
            "AudioHub could not inspect CoreAudio before installing the driver",
        )
        .with_detail(error)
    })?;
    let daemon_was_running = super::service::stop_mac_daemon_for_driver_install_unlocked(
        &lifecycle_guard,
    )
    .map_err(|error| {
        DriverInstallError::new(
            "stop-failed",
            "AudioHub could not safely stop its service before restarting CoreAudio",
        )
        .with_detail(error.detail.unwrap_or(error.message))
    })?;

    let result = match platform::install(app) {
        Ok(result) => result,
        Err(mut install_error) => {
            // Cancellation and pre-transaction failures must not turn a working
            // service into a stopped one. If PackageKit partially installed the
            // driver, give its asynchronously restarted HAL a chance to publish
            // before restoring the daemon; Auto mode will keep retrying after
            // that regardless.
            if daemon_was_running {
                let package_may_have_mutated = matches!(
                    install_error.kind,
                    "install-failed" | "installed-unavailable"
                );
                if package_may_have_mutated && platform::installed_valid() {
                    if let Err(wait_error) = platform::wait_for_driver_service(
                        &coreaudio_before,
                        std::time::Duration::from_secs(12),
                    ) {
                        let readiness = format!(
                            "post-install CoreAudio check failed: {}",
                            wait_error.detail.unwrap_or(wait_error.message)
                        );
                        install_error.detail = Some(match install_error.detail.take() {
                            Some(detail) => format!("{detail}; {readiness}"),
                            None => readiness,
                        });
                    }
                }
                if let Err(start_error) =
                    super::service::start_mac_daemon_blocking_unlocked(&lifecycle_guard)
                {
                    let restore = format!(
                        "service restore failed: {}",
                        start_error.detail.unwrap_or(start_error.message)
                    );
                    install_error.detail = Some(match install_error.detail.take() {
                        Some(detail) => format!("{detail}; {restore}"),
                        None => restore,
                    });
                }
            }
            return Err(install_error);
        }
    };

    // PackageKit's postinstall has only requested a coreaudiod restart. Do not
    // start any CoreAudio-using daemon until the plug-in helper has actually
    // loaded the bundle and published its Mach service.
    if let Err(mut ready_error) =
        platform::wait_for_driver_service(&coreaudio_before, std::time::Duration::from_secs(20))
    {
        if let Err(start_error) =
            super::service::start_mac_daemon_blocking_unlocked(&lifecycle_guard)
        {
            let restore = format!(
                "service restore failed: {}",
                start_error.detail.unwrap_or(start_error.message)
            );
            ready_error.detail = Some(match ready_error.detail.take() {
                Some(detail) => format!("{detail}; {restore}"),
                None => restore,
            });
        }
        return Err(ready_error);
    }

    super::service::start_mac_daemon_blocking_unlocked(&lifecycle_guard).map_err(|error| {
        DriverInstallError::new(
            "installed-unavailable",
            "The driver installed, but AudioHub could not restart its service",
        )
        .with_detail(error.detail.unwrap_or(error.message))
    })?;
    verify_driver_ready()?;
    Ok(result)
}

#[cfg(not(target_os = "macos"))]
fn install_driver_blocking(app: &AppHandle) -> Result<DriverInstallResult, DriverInstallError> {
    let result = platform::install(app)?;
    if result.reboot_required || result.state == "installed_unavailable" {
        return Ok(result);
    }

    // Windows cannot hot-add a bridge which was absent at daemon startup, so
    // restart the same bundled daemon after the PnP transaction completes.
    super::shutdown_daemon_blocking();
    if !super::service::wait_for_daemon_exit(std::time::Duration::from_secs(8)) {
        return Err(DriverInstallError::new(
            "installed-unavailable",
            "The driver installed, but the existing AudioHub service did not stop",
        ));
    }
    super::ensure_daemon_blocking().map_err(|error| {
        DriverInstallError::new(
            "installed-unavailable",
            "The driver installed, but AudioHub could not restart its service",
        )
        .with_detail(error.detail.unwrap_or(error.message))
    })?;
    verify_driver_ready()?;
    Ok(result)
}

fn driver_install_diagnostic(error: &DriverInstallError) -> String {
    serde_json::json!({
        "event": "driver_install_failed",
        "kind": error.kind,
        "message": &error.message,
        "detail": error.detail.as_deref(),
    })
    .to_string()
}

const DRIVER_READY_STABLE_FOR: std::time::Duration = std::time::Duration::from_secs(2);
const DRIVER_READY_MIN_POLLS: u32 = 3;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct DriverReadinessWindow {
    ready_since: Option<std::time::Duration>,
    ready_polls: u32,
}

/// Advance a readiness window from one explicitly-timed observation. Keeping
/// this free of wall-clock reads makes reset and minimum-sample semantics easy
/// to prove without sleeping in tests.
fn advance_driver_readiness(
    current: DriverReadinessWindow,
    observed_at: std::time::Duration,
    ready: bool,
) -> (DriverReadinessWindow, bool) {
    if !ready {
        return (DriverReadinessWindow::default(), false);
    }
    let next = match current.ready_since {
        Some(ready_since) => DriverReadinessWindow {
            ready_since: Some(ready_since),
            ready_polls: current.ready_polls.saturating_add(1),
        },
        None => DriverReadinessWindow {
            ready_since: Some(observed_at),
            ready_polls: 1,
        },
    };
    let stable_for = observed_at.saturating_sub(next.ready_since.unwrap_or(observed_at));
    let stable =
        stable_for >= DRIVER_READY_STABLE_FOR && next.ready_polls >= DRIVER_READY_MIN_POLLS;
    (next, stable)
}

fn verify_driver_ready() -> Result<(), DriverInstallError> {
    let cli = super::cli_binary().ok_or_else(|| {
        DriverInstallError::new(
            "installed-unavailable",
            "The driver installed, but AudioHub cannot verify its service",
        )
    })?;
    let started = std::time::Instant::now();
    let deadline = started + std::time::Duration::from_secs(12);
    let mut readiness = DriverReadinessWindow::default();
    let mut last = "daemon has not returned driver status yet".to_owned();
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(DriverInstallError::new(
                "installed-unavailable",
                "The driver files installed, but AudioHub could not use them",
            )
            .with_detail(last));
        }
        let mut command = std::process::Command::new(&cli);
        command
            .args(["ctl", "status", "--json"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        super::spawn_without_console(&mut command);
        let (ready, detail) = match super::command_output_with_timeout(
            &mut command,
            remaining.min(std::time::Duration::from_secs(2)),
        ) {
            Ok(output) if output.status.success() => {
                match serde_json::from_slice::<serde_json::Value>(&output.stdout) {
                    Ok(value) => {
                        let connected = value
                            .pointer("/hal/driver_connected")
                            .and_then(serde_json::Value::as_bool)
                            == Some(true);
                        let registered = value
                            .pointer("/hal/registered")
                            .and_then(serde_json::Value::as_bool)
                            == Some(true);
                        let ready = connected && registered;
                        let detail = value
                            .pointer("/hal/status_reason")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or(if ready {
                                "driver readiness has not been stable long enough"
                            } else {
                                "driver is not connected and registered"
                            })
                            .to_owned();
                        (ready, detail)
                    }
                    Err(error) => (false, format!("invalid daemon status: {error}")),
                }
            }
            Ok(output) => (
                false,
                String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            ),
            Err(error) => (false, error.to_string()),
        };
        let observed_at = started.elapsed();
        let (next, stable) = advance_driver_readiness(readiness, observed_at, ready);
        readiness = next;
        if stable {
            return Ok(());
        }
        last = if ready {
            let stable_for = observed_at
                .saturating_sub(readiness.ready_since.unwrap_or(observed_at))
                .as_millis();
            format!(
                "driver readiness has held for {stable_for} ms across {} polls; {detail}",
                readiness.ready_polls
            )
        } else if detail.trim().is_empty() {
            "daemon did not return a usable driver status".to_owned()
        } else {
            detail
        };
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

#[cfg(test)]
mod driver_readiness_tests {
    use super::*;

    #[test]
    fn readiness_requires_two_seconds_and_multiple_polls() {
        let (window, ready) = advance_driver_readiness(
            DriverReadinessWindow::default(),
            std::time::Duration::ZERO,
            true,
        );
        assert!(!ready);
        let (window, ready) =
            advance_driver_readiness(window, std::time::Duration::from_secs(1), true);
        assert!(!ready);
        let (window, ready) =
            advance_driver_readiness(window, std::time::Duration::from_secs(2), true);
        assert!(ready);
        assert_eq!(window.ready_polls, DRIVER_READY_MIN_POLLS);
    }

    #[test]
    fn readiness_failure_resets_the_continuous_window() {
        let (window, _) = advance_driver_readiness(
            DriverReadinessWindow::default(),
            std::time::Duration::ZERO,
            true,
        );
        let (window, ready) =
            advance_driver_readiness(window, std::time::Duration::from_millis(1500), false);
        assert!(!ready);
        assert_eq!(window, DriverReadinessWindow::default());

        let (window, _) = advance_driver_readiness(window, std::time::Duration::from_secs(2), true);
        let (window, _) = advance_driver_readiness(window, std::time::Duration::from_secs(3), true);
        let (_, ready) = advance_driver_readiness(window, std::time::Duration::from_secs(4), true);
        assert!(ready);
    }

    #[test]
    fn elapsed_time_without_enough_polls_is_not_stable() {
        let (window, _) = advance_driver_readiness(
            DriverReadinessWindow::default(),
            std::time::Duration::ZERO,
            true,
        );
        let (_, ready) = advance_driver_readiness(window, std::time::Duration::from_secs(3), true);
        assert!(!ready);
    }
}
