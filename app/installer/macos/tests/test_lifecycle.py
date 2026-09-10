#!/usr/bin/env python3
"""Non-privileged fixtures for the macOS package/service lifecycle."""

import hashlib
import os
import plistlib
import shlex
import subprocess
import tempfile
import textwrap
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[4]
APP_BUILDER = ROOT / "app/build-app.sh"
DAEMON_INSTALLER = ROOT / "app/installer/macos/install-daemon.sh"
APP_COMPONENTS = ROOT / "app/installer/macos/app-components.plist"
PKG_BUILDER = ROOT / "scripts/build-macos-pkg.sh"
DMG_BUILDER = ROOT / "scripts/build-macos-dmg.sh"
UNINSTALLER = ROOT / "app/installer/macos/uninstall.applescript"
DRIVER_INSTALLER = ROOT / "app/src-tauri/src/driver_install.rs"
DRIVER_POSTINSTALL = ROOT / "app/installer/macos/driver-scripts/postinstall"
APP_PREINSTALL = ROOT / "app/installer/macos/pkg-scripts/preinstall"
PKG_VERIFIER = ROOT / "scripts/verify-macos-app-pkg.sh"
LIFECYCLE_LOCK = "/var/run/com.audiohub.lifecycle.lock"


def write_executable(path: Path, body: str) -> None:
    path.write_text("#!/bin/sh\n" + textwrap.dedent(body), encoding="utf-8")
    path.chmod(0o755)


def extract_applescript_string(source: str, variable: str) -> str:
    marker = f'set {variable} to "'
    start = source.index(marker) + len(marker)
    output = []
    escaped = {"n": "\n", "r": "\r", "t": "\t", '"': '"', "\\": "\\"}
    i = start
    while i < len(source):
        char = source[i]
        if char == '"':
            return "".join(output)
        if char == "\\":
            i += 1
            if i >= len(source):
                raise AssertionError("unterminated AppleScript escape")
            output.append(escaped.get(source[i], source[i]))
        else:
            output.append(char)
        i += 1
    raise AssertionError(f"unterminated AppleScript string for {variable}")


def replace_commands(script: str, replacements: dict[str, Path]) -> str:
    # Longer paths first: /usr/bin/killall contains /bin/kill.
    for command in sorted(replacements, key=len, reverse=True):
        script = script.replace(command, shlex.quote(str(replacements[command])))
    return script


class LifecycleStaticTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.app_builder = APP_BUILDER.read_text(encoding="utf-8")
        cls.daemon_installer = DAEMON_INSTALLER.read_text(encoding="utf-8")
        cls.pkg_builder = PKG_BUILDER.read_text(encoding="utf-8")
        cls.dmg_builder = DMG_BUILDER.read_text(encoding="utf-8")
        cls.applescript = UNINSTALLER.read_text(encoding="utf-8")
        cls.driver_installer = DRIVER_INSTALLER.read_text(encoding="utf-8")
        cls.driver_postinstall = DRIVER_POSTINSTALL.read_text(encoding="utf-8")
        cls.root_shell = extract_applescript_string(cls.applescript, "rootCommand")

    def test_sources_compile_without_running_installer_or_authorization(self) -> None:
        subprocess.run(["/bin/sh", "-n", str(DAEMON_INSTALLER)], check=True)
        subprocess.run(["/bin/sh", "-n"], input=self.root_shell, text=True, check=True)
        with tempfile.TemporaryDirectory(prefix="audiohub-osa-") as tmp:
            subprocess.run(
                ["/usr/bin/osacompile", "-o", str(Path(tmp) / "Uninstall AudioHub.app"), str(UNINSTALLER)],
                check=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
            )

    def test_dmg_builder_stages_pkg_and_double_clickable_uninstaller(self) -> None:
        dmg = self.dmg_builder
        expand = dmg.index('pkgutil --expand-full "$PKG"')
        stage = dmg.index('"$PKG" "$STAGE/Install AudioHub.pkg"')
        self.assertLess(expand, stage)
        self.assertNotIn("PKG_PREINSTALL", dmg)
        self.assertNotIn("PKG_POSTINSTALL", dmg)
        self.assertNotIn("app-scripts", dmg)
        self.assertIn('verify-macos-app-pkg.sh', dmg)
        self.assertIn("installer must contain only AudioHub.app", dmg)
        self.assertIn('ditto --norsrc --noextattr --noacl --noqtn', dmg)
        self.assertIn('"$PKG" "$STAGE/Install AudioHub.pkg"', dmg)
        self.assertIn('UNINSTALLER="$STAGE/Uninstall AudioHub.app"', dmg)
        self.assertIn('osacompile -o "$UNINSTALLER" "$SOURCE"', dmg)
        self.assertIn('[[ -d "$MOUNT/Uninstall AudioHub.app" ]]', dmg)
        self.assertIn('[[ -s "$MOUNT/Install AudioHub.pkg" ]]', dmg)
        self.assertIn('pkgutil --payload-files "$MOUNT/Install AudioHub.pkg"', dmg)
        self.assertGreaterEqual(dmg.count('._*|*/._*'), 2)

    def test_macos_build_lock_and_artifact_publication_are_atomic(self) -> None:
        for source in (self.app_builder, self.pkg_builder, self.dmg_builder):
            self.assertIn('audiohub-macos-build.lock', source)
            self.assertIn('/usr/bin/lockf -s -t 0 8', source)
            self.assertIn('AUDIOHUB_MACOS_BUILD_LOCK_PATH', source)

        for source, suffix in ((self.pkg_builder, "pkg"), (self.dmg_builder, "dmg")):
            self.assertIn(f'mktemp -d "$OUT_DIR/.AudioHub-{suffix}-build.XXXXXX"', source)
            self.assertIn('/bin/mv -f "$CANDIDATE" "$FINAL"', source)
            self.assertNotIn('/bin/rm -f "$FINAL"', source)

            publish = source.index('/bin/mv -f "$CANDIDATE" "$FINAL"')
            self.assertLess(source.index('codesign --verify', 0), publish)
            self.assertLess(source.index('CANDIDATE=', 0), publish)

        self.assertIn('pkgutil --expand-full "$CANDIDATE"', self.pkg_builder)
        self.assertIn('notarytool submit "$CANDIDATE"', self.pkg_builder)
        self.assertIn('hdiutil verify "$CANDIDATE"', self.dmg_builder)
        self.assertIn('notarytool submit "$CANDIDATE"', self.dmg_builder)

    def test_pkg_builder_has_an_app_only_payload_and_shutdown_preinstall(self) -> None:
        source = self.pkg_builder
        self.assertIn('--scripts "$WORK/scripts"', source)
        self.assertIn('pkg-scripts/preinstall', source)
        self.assertIn('--install-location /Applications', source)
        self.assertIn('--component-plist "$COMPONENTS"', source)
        self.assertIn('--norsrc --noextattr --noacl --noqtn', source)
        self.assertIn('case "$payload" in', source)
        self.assertIn('.|./AudioHub.app|./AudioHub.app/*)', source)
        self.assertIn('._*|*/._*', source)
        self.assertIn('pkgutil --expand-full "$CANDIDATE"', source)
        self.assertIn('verify-macos-app-pkg.sh', source)
        self.assertIn('codesign --verify --deep --strict "$EXPANDED_APP"', source)

        with APP_COMPONENTS.open("rb") as stream:
            components = plistlib.load(stream)
        self.assertEqual(len(components), 1)
        self.assertEqual(components[0]["RootRelativeBundlePath"], "AudioHub.app")
        self.assertIs(components[0]["BundleIsRelocatable"], False)
        self.assertIs(components[0]["BundleHasStrictIdentifier"], True)

    def test_package_verifier_rejects_additional_or_modified_lifecycle_hooks(self) -> None:
        with tempfile.TemporaryDirectory(prefix="audiohub-pkg-check-") as tmp:
            component = Path(tmp) / "AudioHubApp.pkg"
            scripts = component / "Scripts"
            scripts.mkdir(parents=True)
            hook = scripts / "preinstall"
            original = APP_PREINSTALL.read_bytes()
            hook.write_bytes(original)
            (component / "PackageInfo").write_text(
                '<pkg-info relocatable="false"><relocate/><scripts><preinstall file="./preinstall"/></scripts></pkg-info>'
            )
            def verify() -> int:
                return subprocess.run(["/bin/zsh", str(PKG_VERIFIER), tmp],
                                      capture_output=True).returncode
            self.assertEqual(verify(), 0)
            (scripts / "postinstall").write_text("#!/bin/sh\nexit 0\n")
            self.assertNotEqual(verify(), 0)
            (scripts / "postinstall").unlink()
            hook.write_bytes(original + b"\necho modified\n")
            self.assertNotEqual(verify(), 0)
            hook.write_bytes(original)
            (component / "PackageInfo").write_text(
                '<pkg-info relocatable="false"><relocate><bundle id="com.audiohub.app"/></relocate>'
                '<scripts><preinstall file="./preinstall"/></scripts></pkg-info>'
            )
            self.assertNotEqual(verify(), 0)

    def test_preinstall_refuses_another_volume_before_process_actions(self) -> None:
        subprocess.run(["/bin/sh", "-n", str(APP_PREINSTALL)], check=True)
        result = subprocess.run(["/bin/sh", str(APP_PREINSTALL), "unused.pkg",
                                 "/Applications", "/Volumes/OtherSystem"], capture_output=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(b"running macOS system volume", result.stderr)

    def test_daemon_installer_is_fixed_argument_free_and_never_resigns_the_app(self) -> None:
        source = self.daemon_installer
        self.assertIn("APP='/Applications/AudioHub.app'", source)
        self.assertIn("SOURCE=\"$APP/Contents/MacOS/audiohubd\"", source)
        self.assertIn("BASE='/Library/Application Support/AudioHub/service'", source)
        self.assertIn("SIGNING='/Library/Application Support/AudioHub/signing'", source)
        self.assertIn("DAEMON_IDENTIFIER='com.audiohub.daemon'", source)
        self.assertNotIn("eval ", source)

        self.assertIn('codesign --verify --deep --strict "$APP"', source)
        self.assertIn('[ -f "$SOURCE" ] && [ ! -L "$SOURCE" ] && [ -x "$SOURCE" ]', source)
        self.assertIn('[ "$SOURCE_ID" = "$DAEMON_IDENTIFIER" ]', source)
        self.assertIn('install -o root -g wheel -m 0755 "$SOURCE" "$PAYLOAD/audiohubd"', source)
        self.assertIn('[ "$COPIED_SHA" = "$SOURCE_SHA" ]', source)
        self.assertIn('--identifier "$DAEMON_IDENTIFIER" "$CONSOLE_PAYLOAD"', source)
        self.assertIn(r'certificate leaf = H\"$CERTIFICATE_SHA1\"', source)
        self.assertNotIn('--force --sign "$CERTIFICATE_SHA1" --keychain "$KEYCHAIN" \\\n    --identifier "$DAEMON_IDENTIFIER" "$SOURCE"', source)

        certificate_sign_lines = [
            line
            for line in source.splitlines()
            if "/usr/bin/codesign" in line and '--sign "$CERTIFICATE_SHA1"' in line
        ]
        self.assertGreaterEqual(len(certificate_sign_lines), 2)
        self.assertTrue(
            all("as_console_user /usr/bin/codesign" in line for line in certificate_sign_lines),
            certificate_sign_lines,
        )
        self.assertIn('/bin/launchctl asuser "$CONSOLE_UID" \\', source)
        self.assertIn('/usr/bin/sudo -n -H -u "#$CONSOLE_UID" -- \\', source)
        self.assertIn('/usr/bin/env -i \\', source)
        self.assertIn('--timestamp=none --options=0 --pagesize=4096', source)

        snapshot = source.index(
            'install -o root -g wheel -m 0700 "$CONSOLE_PAYLOAD" "$SIGNED_SNAPSHOT"'
        )
        snapshot_check = source.index('check_root_file "$SIGNED_SNAPSHOT"', snapshot)
        snapshot_verify = source.index(
            '/usr/bin/codesign --verify --strict=all', snapshot_check
        )
        self.assertLess(snapshot, snapshot_check)
        self.assertLess(snapshot_check, snapshot_verify)
        self.assertIn('/bin/chmod -N "$SIGNED_SNAPSHOT"', source)
        self.assertIn('/usr/bin/xattr -c "$SIGNED_SNAPSHOT"', source)
        self.assertIn('canonicalize_daemon_content "$ORIGINAL_CANONICAL"', source)
        self.assertIn('canonicalize_daemon_content "$SIGNED_CANONICAL"', source)
        self.assertIn('/usr/bin/cmp -s "$ORIGINAL_CANONICAL" "$SIGNED_CANONICAL"', source)
        self.assertIn('/usr/bin/codesign -d --entitlements :- "$SIGNED_SNAPSHOT"', source)
        self.assertIn('[ ! -s "$SIGNED_ENTITLEMENTS" ]', source)
        self.assertIn("flags=0x0\\(none\\)", source)
        self.assertIn("Page size=4096", source)
        self.assertIn("TeamIdentifier=not set", source)
        self.assertIn("Runtime Version|Launch Constraints?|Library Constraints?", source)

    def test_daemon_installer_secures_identity_and_rolls_back_activation(self) -> None:
        source = self.daemon_installer
        self.assertIn('install -d -o root -g wheel -m 0700 "$SIGNING"', source)
        self.assertIn('check_private_root_dir "$SIGNING"', source)
        self.assertIn('check_private_root_file "$KEYCHAIN"', source)
        self.assertIn('check_private_root_file "$KEYCHAIN_PASSWORD"', source)
        self.assertIn('CERTIFICATE_RECORD="$SIGNING/certificate.sha1"', source)
        self.assertIn(f"LIFECYCLE_LOCK='{LIFECYCLE_LOCK}'", source)
        self.assertIn('exec 9<> "$LIFECYCLE_LOCK"', source)
        self.assertIn('/usr/bin/lockf -s -t 0 9', source)
        self.assertIn("CERTIFICATE_COUNT=", source)
        self.assertIn('[ "$CERTIFICATE_COUNT" = 1 ]', source)
        self.assertIn('security list-keychains -d user', source)
        self.assertIn('security list-keychains -d user -s "$@"', source)
        self.assertIn('security lock-keychain "$KEYCHAIN"', source)
        self.assertIn('[ "$status" -ne 0 ] && [ "$IDENTITY_CREATED" -eq 1 ]', source)
        self.assertIn('security delete-keychain "$KEYCHAIN"', source)

        rollback = source[source.index("rollback_current()") : source.index("cleanup()")]
        self.assertIn('/bin/chmod -h 0755 "$CURRENT_ROLLBACK"', rollback)
        self.assertIn('/bin/mv -f -h "$CURRENT_ROLLBACK" "$CURRENT"', rollback)
        self.assertIn('/bin/mv -h "$FINAL_BACKUP" "$FINAL"', rollback)
        self.assertIn('rollback_current', source[source.index("cleanup()") : source.index("trap cleanup EXIT")])
        self.assertIn('rollback_final', source[source.index("cleanup()") : source.index("trap cleanup EXIT")])
        self.assertIn('/bin/mv -h "$FINAL" "$FINAL_BACKUP"', source)
        self.assertIn('if /bin/mv "$PAYLOAD" "$FINAL"; then', source)
        self.assertIn('/bin/chmod -h 0755 "$CURRENT_TMP"', source)
        self.assertIn('/bin/mv -f -h "$CURRENT_TMP" "$CURRENT"', source)
        self.assertIn('"$BASE/current/audiohubd"', source)

    def test_process_matching_is_exact_and_machine_wide(self) -> None:
        source = self.root_shell
        self.assertIn("ps -axo pid=,comm=", source)
        self.assertNotIn("pkill", source)
        self.assertNotIn("pgrep", source)
        self.assertNotIn("-U \"$USER_UID\"", source)
        for executable in ("audiohub-app", "audiohubd", "audiohub"):
            self.assertIn(f'/Contents/MacOS/{executable}"', source)
        self.assertIn("SERVICE_CURRENT='/Library/Application Support/AudioHub/service/current/audiohubd'", source)
        self.assertIn("signal_product_images TERM", source)
        self.assertIn("signal_product_images KILL", source)
        self.assertIn("[ -n \"$(product_pids)\" ]", source)

    def test_uninstaller_scope_and_product_identity_are_explicit(self) -> None:
        self.assertIn("dscl /Search -list /Users UniqueID", self.root_shell)
        self.assertIn("uid >= 500", self.root_shell)
        self.assertIn("NFSHomeDirectory", self.root_shell)
        self.assertIn("directory_chain_is_real", self.root_shell)
        self.assertIn("[ ! -L \"$current\" ]", self.root_shell)
        self.assertIn("[ \"$home_owner\" = \"$USER_UID\" ]", self.root_shell)
        self.assertIn("com.audiohub.app.autostart", self.root_shell)
        self.assertIn("com.audiohub.daemon", self.root_shell)
        self.assertIn("service-installed-v1", self.root_shell)
        self.assertIn("ipc.json", self.root_shell)
        self.assertIn("APP_ID", self.root_shell)
        self.assertIn("com.audiohub.app", self.root_shell)
        self.assertIn("DRIVER_ID", self.root_shell)
        self.assertIn("com.audiohub.driver", self.root_shell)
        self.assertIn("PRODUCT_SUPPORT='/Library/Application Support/AudioHub'", self.root_shell)
        self.assertIn("SERVICE='/Library/Application Support/AudioHub/service'", self.root_shell)
        self.assertIn("SIGNING='/Library/Application Support/AudioHub/signing'", self.root_shell)
        self.assertIn(f"LIFECYCLE_LOCK='{LIFECYCLE_LOCK}'", self.root_shell)

        removal_lines = "\n".join(
            line for line in self.root_shell.splitlines() if "/bin/rm" in line
        )
        for preserved in ("settings.json", "identity", "paired_peers.json", "logs"):
            self.assertNotIn(preserved, removal_lines)

    def test_all_privileged_macos_lifecycle_paths_share_one_safe_lock(self) -> None:
        for source in (self.daemon_installer, self.root_shell, self.driver_installer):
            self.assertIn(LIFECYCLE_LOCK, source)
            self.assertIn("/usr/bin/lockf -s -t 0 9", source)
            self.assertIn("[ ! -L", source)
            self.assertIn("0600", source)

        uninstall_lock = self.root_shell.index("/usr/bin/lockf -s -t 0 9")
        machine_preflight = self.root_shell.index(
            'check_root_product_dir "$PRODUCT_SUPPORT" || exit 33'
        )
        first_launch_mutation = self.root_shell.index(
            'launchctl bootout "gui/$USER_UID/$CURRENT_LABEL"'
        )
        first_driver_mutation = self.root_shell.index('/bin/rm -rf "$DRIVER"')
        self.assertLess(uninstall_lock, machine_preflight)
        self.assertLess(machine_preflight, first_launch_mutation)
        self.assertLess(machine_preflight, first_driver_mutation)

        daemon_lock = self.daemon_installer.index("/usr/bin/lockf -s -t 0 9")
        first_service_mutation = self.daemon_installer.index(
            'install -d -o root -g wheel -m 0755 "$AUDIOHUB_SUPPORT"'
        )
        self.assertLess(daemon_lock, first_service_mutation)

    def test_standard_uninstall_does_not_leave_a_daemonless_driver(self) -> None:
        self.assertNotIn("App Only", self.applescript)
        self.assertNotIn("仅卸载 App", self.applescript)
        self.assertNotIn("removeDriver", self.applescript)
        self.assertIn("buttons {cancelText, uninstallText}", self.applescript)
        self.assertIn("DRIVER_REMOVED=0", self.root_shell)
        driver_remove = self.root_shell.index('/bin/rm -rf "$DRIVER"')
        coreaudio_reload = self.root_shell.index("launchctl kickstart -kp system/com.apple.audio.coreaudiod")
        app_remove = self.root_shell.index('/bin/rm -rf "$APP"')
        app_forget = self.root_shell.index("pkgutil --forget com.audiohub.app.pkg")
        driver_forget = self.root_shell.index("pkgutil --forget com.audiohub.driver.pkg")
        self.assertLess(driver_remove, coreaudio_reload)
        self.assertLess(coreaudio_reload, app_remove)
        self.assertLess(app_remove, app_forget)
        self.assertLess(app_remove, driver_forget)

    def test_driver_reload_retires_the_old_hal_helper_first(self) -> None:
        helper = "com.apple.audio.Core-Audio-Driver-Service.helper"
        audiohub_helper = "Core Audio Driver (AudioHubDriver.driver)"
        exact_match = (
            '[ "$image" = "$HELPER" ] || '
            '[ "$image" = "$AUDIOHUB_HELPER_NAME" ]'
        )
        for source in (self.driver_postinstall, self.root_shell):
            self.assertIn(helper, source)
            self.assertIn(audiohub_helper, source)
            self.assertIn("is_audiohub_hal_helper_image", source)
            self.assertIn(exact_match, source)
            self.assertIn("while read -r pid image", source)
            self.assertNotIn("Core Audio Driver (*", source)

        helper_stop = self.driver_postinstall.index('/bin/kill -TERM "$helper_pid"')
        self.assertIn('/bin/kill -KILL "$helper_pid"', self.driver_postinstall)
        coreaudio_reload = self.driver_postinstall.index(
            "launchctl kickstart -kp system/com.apple.audio.coreaudiod"
        )
        self.assertLess(helper_stop, coreaudio_reload)

        uninstall_helper_stop = self.root_shell.index(
            '/bin/kill -TERM "$helper_pid"'
        )
        self.assertIn('/bin/kill -KILL "$helper_pid"', self.root_shell)
        uninstall_coreaudio_reload = self.root_shell.index(
            "launchctl kickstart -kp system/com.apple.audio.coreaudiod"
        )
        self.assertLess(uninstall_helper_stop, uninstall_coreaudio_reload)

    def test_coreaudio_fallback_is_pid_and_path_bound(self) -> None:
        for source in (self.driver_postinstall, self.root_shell):
            self.assertIn("COREAUDIOD='/usr/sbin/coreaudiod'", source)
            self.assertIn('ps -p "$core_pid" -o comm=', source)
            self.assertIn('[ "$current" != "$COREAUDIOD" ]', source)
            self.assertIn('/bin/kill -TERM "$core_pid"', source)
            self.assertIn('/bin/kill -KILL "$core_pid"', source)
            self.assertNotIn("killall coreaudiod", source)


class LifecycleFixtureTests(unittest.TestCase):
    def _process_stubs(self, stub_dir: Path) -> dict[str, Path]:
        write_executable(
            stub_dir / "ps",
            r"""
            if [ "$1" = "-axo" ]; then
              /usr/bin/awk -F '|' '{ print $1 " " $2 }' "$FIXTURE_PROCESS_STATE"
              exit 0
            fi
            if [ "$1" = "-p" ]; then
              /usr/bin/awk -F '|' -v pid="$2" '$1 == pid { print $2 }' "$FIXTURE_PROCESS_STATE"
              exit 0
            fi
            exit 2
            """,
        )
        write_executable(
            stub_dir / "kill",
            r"""
            signal="$1"
            pid="$2"
            printf '%s %s\n' "$signal" "$pid" >>"$FIXTURE_SIGNAL_LOG"
            behavior=$(/usr/bin/awk -F '|' -v pid="$pid" '$1 == pid { print $3 }' "$FIXTURE_PROCESS_STATE")
            if { [ "$signal" = "-KILL" ] && [ "$behavior" != "kill-survives" ]; } || [ "$behavior" = "term-exits" ]; then
              /usr/bin/awk -F '|' -v pid="$pid" '$1 != pid' "$FIXTURE_PROCESS_STATE" >"$FIXTURE_PROCESS_STATE.next"
              /bin/mv -f "$FIXTURE_PROCESS_STATE.next" "$FIXTURE_PROCESS_STATE"
            fi
            """,
        )
        write_executable(stub_dir / "sleep", ":\n")
        return {
            "/bin/ps": stub_dir / "ps",
            "/bin/kill": stub_dir / "kill",
            "/bin/sleep": stub_dir / "sleep",
        }

    def _make_daemon_install_fixture(
        self,
        tmp: Path,
        *,
        existing_identity: bool = False,
        existing_payload: bool = False,
        fail_sign: bool = False,
        fail_activate: bool = False,
        lock_busy: bool = False,
    ) -> tuple[str, dict[str, str], dict[str, Path]]:
        stub = tmp / "stub"
        stub.mkdir()
        fixture_root = tmp / "root"
        app = fixture_root / "Applications/AudioHub.app"
        source = app / "Contents/MacOS/audiohubd"
        source.parent.mkdir(parents=True)
        (app / "Contents/Info.plist").write_text("fixture\n", encoding="utf-8")
        source.write_bytes(b"audiohub daemon fixture\n")
        source.chmod(0o755)

        application_support = fixture_root / "Library/Application Support"
        run_dir = fixture_root / "var/run"
        run_dir.mkdir(parents=True)
        lifecycle_lock = run_dir / "com.audiohub.lifecycle.lock"
        system_root = application_support / "AudioHub"
        base = system_root / "service"
        versions = base / "versions"
        signing = system_root / "signing"
        keychain = signing / "identity.keychain-db"
        keychain_password = signing / "keychain.pass"
        application_support.mkdir(parents=True)
        source_sha = hashlib.sha256(source.read_bytes()).hexdigest()
        final = versions / source_sha

        if existing_identity:
            signing.mkdir(parents=True)
            keychain.write_text("keychain fixture\n", encoding="utf-8")
            keychain_password.write_text(
                "AAAAAAAA-AAAA-AAAA-AAAA-AAAAAAAAAAAA\n", encoding="utf-8"
            )
            keychain.chmod(0o600)
            keychain_password.chmod(0o600)
        if existing_payload:
            final.mkdir(parents=True)
            (final / "audiohubd").write_bytes(b"previous signed daemon\n")
            (final / "source.sha256").write_text("stale\n", encoding="utf-8")
            (final / "signed.sha256").write_text("old-signed\n", encoding="utf-8")
            (final / "certificate.sha1").write_text("old-cert\n", encoding="utf-8")

        action_log = tmp / "actions"
        login_keychain = tmp / "login.keychain-db"
        login_keychain.write_text("login keychain fixture\n", encoding="utf-8")

        console_user = "audiohubtest"
        console_uid = 501
        console_gid = 20
        console_home = fixture_root / "Users" / console_user
        console_home.mkdir(parents=True)
        session_rooted_marker = tmp / "session-rooted"
        console_search_state = tmp / "console-search-list"
        concurrent_keychain = tmp / "concurrent-login.keychain-db"
        concurrent_keychain.write_text("concurrent keychain fixture\n", encoding="utf-8")
        console_search_state.write_text(str(login_keychain) + "\n", encoding="utf-8")

        write_executable(
            stub / "id",
            r"""
            case "$1:$2" in
              -u:) echo 0 ;;
              -u:"$FIXTURE_CONSOLE_USER") echo "$FIXTURE_CONSOLE_UID" ;;
              -g:"$FIXTURE_CONSOLE_USER") echo "$FIXTURE_CONSOLE_GID" ;;
              *) exit 2 ;;
            esac
            """,
        )
        write_executable(
            stub / "dscl",
            r"""
            [ "$1" = "." ] && [ "$2" = "-read" ] \
              && [ "$3" = "/Users/$FIXTURE_CONSOLE_USER" ] \
              && [ "$4" = "NFSHomeDirectory" ] || exit 2
            printf 'NFSHomeDirectory: %s\n' "$FIXTURE_CONSOLE_HOME"
            """,
        )
        write_executable(
            stub / "stat",
            r"""
            if [ "$1" = "-f" ] && [ "$2" = "%u:%g:%p" ] \
              && [ "$3" = "/private/tmp" ]; then
              echo '0:0:41777'
              exit 0
            fi
            if [ "$1" = "-f" ] && [ "$2" = "%u" ]; then
              case "$3" in
                /dev/console|"$FIXTURE_CONSOLE_HOME")
                  echo "$FIXTURE_CONSOLE_UID"
                  ;;
                /private/tmp/com.audiohub.sign.*/session)
                  if [ -f "$FIXTURE_SESSION_ROOTED_MARKER" ] \
                    && [ "$(/bin/cat "$FIXTURE_SESSION_ROOTED_MARKER")" = "$3" ]; then
                    echo 0
                  else
                    echo "$FIXTURE_CONSOLE_UID"
                  fi
                  ;;
                /private/tmp/com.audiohub.sign.*/session/*)
                  echo "$FIXTURE_CONSOLE_UID"
                  ;;
                *) echo 0 ;;
              esac
              exit 0
            fi
            if [ "$1" = "-f" ] && [ "$2" = "%Su" ] && [ "$3" = "/dev/console" ]; then
              echo "$FIXTURE_CONSOLE_USER"
              exit 0
            fi
            if [ "$1" = "-f" ] && [ "$2" = "%g" ]; then echo 0; exit 0; fi
            exec /usr/bin/stat "$@"
            """,
        )
        write_executable(
            stub / "plutil",
            r"""
            [ "$1:$2" = "-extract:CFBundleIdentifier" ] || exit 2
            echo com.audiohub.app
            """,
        )
        write_executable(
            stub / "install",
            r"""
            directory=0
            [ "$1" = "-d" ] && { directory=1; shift; }
            mode=
            while [ "$#" -gt 0 ]; do
              case "$1" in
                -o|-g) shift 2 ;;
                -m) mode="$2"; shift 2 ;;
                *) break ;;
              esac
            done
            if [ "$directory" -eq 1 ]; then
              for path in "$@"; do /usr/bin/install -d -m "$mode" "$path" || exit; done
            else
              [ "$#" -eq 2 ] || exit 2
              /usr/bin/install -m "$mode" "$1" "$2"
            fi
            """,
        )
        write_executable(
            stub / "chown",
            r"""
            { printf 'chown'; printf ' %s' "$@"; printf '\n'; } >>"$FIXTURE_ACTION_LOG"
            if [ "$1" = "root:wheel" ]; then
              shift
              for target in "$@"; do
                case "$target" in
                  /private/tmp/com.audiohub.sign.*/session)
                    printf '%s\n' "$target" >"$FIXTURE_SESSION_ROOTED_MARKER"
                    ;;
                esac
              done
            fi
            exit 0
            """,
        )
        write_executable(
            stub / "launchctl",
            r"""
            { printf 'launchctl'; printf ' %s' "$@"; printf '\n'; } >>"$FIXTURE_ACTION_LOG"
            case "$1" in
              print)
                [ "$2" = "gui/$FIXTURE_CONSOLE_UID" ]
                ;;
              asuser)
                [ "$2" = "$FIXTURE_CONSOLE_UID" ] || exit 2
                shift 2
                exec "$@"
                ;;
              *) exit 2 ;;
            esac
            """,
        )
        write_executable(
            stub / "sudo",
            r"""
            [ "$1" = "-n" ] || exit 2
            shift
            [ "$1" = "-H" ] && shift
            [ "$1" = "-u" ] || exit 2
            selected_user="$2"
            case "$selected_user" in
              "$FIXTURE_CONSOLE_USER"|"#$FIXTURE_CONSOLE_UID") ;;
              *) exit 2 ;;
            esac
            shift 2
            [ "$1" = "--" ] || exit 2
            shift
            printf 'sudo %s\n' "$selected_user" >>"$FIXTURE_ACTION_LOG"
            exec "$@"
            """,
        )
        write_executable(
            stub / "env",
            r"""
            [ "$1" = "-i" ] || exit 2
            printf 'env -i\n' >>"$FIXTURE_ACTION_LOG"
            shift
            exec /usr/bin/env -i \
              FIXTURE_ACTION_LOG="$FIXTURE_ACTION_LOG" \
              FIXTURE_LOGIN_KEYCHAIN="$FIXTURE_LOGIN_KEYCHAIN" \
              FIXTURE_FAIL_SIGN="$FIXTURE_FAIL_SIGN" \
              FIXTURE_FAIL_ACTIVATE="$FIXTURE_FAIL_ACTIVATE" \
              FIXTURE_LOCK_BUSY="$FIXTURE_LOCK_BUSY" \
              FIXTURE_FINAL="$FIXTURE_FINAL" \
              FIXTURE_CONSOLE_USER="$FIXTURE_CONSOLE_USER" \
              FIXTURE_CONSOLE_UID="$FIXTURE_CONSOLE_UID" \
              FIXTURE_CONSOLE_GID="$FIXTURE_CONSOLE_GID" \
              FIXTURE_CONSOLE_HOME="$FIXTURE_CONSOLE_HOME" \
              FIXTURE_SESSION_ROOTED_MARKER="$FIXTURE_SESSION_ROOTED_MARKER" \
              FIXTURE_CONSOLE_SEARCH_STATE="$FIXTURE_CONSOLE_SEARCH_STATE" \
              FIXTURE_CONCURRENT_KEYCHAIN="$FIXTURE_CONCURRENT_KEYCHAIN" \
              "$@"
            """,
        )
        write_executable(
            stub / "lockf",
            '[ "$FIXTURE_LOCK_BUSY" = 1 ] && exit 75\nexec /usr/bin/lockf "$@"\n',
        )
        write_executable(stub / "uuidgen", "echo AAAAAAAA-AAAA-AAAA-AAAA-AAAAAAAAAAAA\n")
        write_executable(
            stub / "openssl",
            r"""
            { printf 'openssl'; printf ' %s' "$@"; printf '\n'; } >>"$FIXTURE_ACTION_LOG"
            previous=
            for argument in "$@"; do
              case "$previous" in -keyout|-out) printf 'fixture\n' >"$argument";; esac
              previous="$argument"
            done
            """,
        )
        write_executable(
            stub / "security",
            r"""
            { printf 'security'; printf ' %s' "$@"; printf '\n'; } >>"$FIXTURE_ACTION_LOG"
            action="$1"
            case "$action" in
              list-keychains)
                set_search=0
                for argument in "$@"; do
                  [ "$argument" = "-s" ] && set_search=1
                done
                if [ "$USER" = "$FIXTURE_CONSOLE_USER" ]; then
                  if [ "$set_search" -eq 1 ]; then
                    : >"$FIXTURE_CONSOLE_SEARCH_STATE"
                    copy=0
                    temporary_present=0
                    for argument in "$@"; do
                      if [ "$copy" -eq 1 ]; then
                        printf '%s\n' "$argument" >>"$FIXTURE_CONSOLE_SEARCH_STATE"
                        case "$argument" in
                          /private/tmp/com.audiohub.sign.*/session/identity.keychain-db)
                            temporary_present=1
                            ;;
                        esac
                      elif [ "$argument" = "-s" ]; then
                        copy=1
                      fi
                    done
                    if [ "$temporary_present" -eq 1 ] \
                      && ! /usr/bin/grep -Fxq "$FIXTURE_CONCURRENT_KEYCHAIN" \
                        "$FIXTURE_CONSOLE_SEARCH_STATE"; then
                      printf '%s\n' "$FIXTURE_CONCURRENT_KEYCHAIN" \
                        >>"$FIXTURE_CONSOLE_SEARCH_STATE"
                    fi
                    exit 0
                  fi
                  while IFS= read -r keychain; do
                    printf '    "%s"\n' "$keychain"
                  done <"$FIXTURE_CONSOLE_SEARCH_STATE"
                else
                  [ "$set_search" -eq 1 ] && exit 0
                  printf '    "%s"\n' "$FIXTURE_LOGIN_KEYCHAIN"
                fi
                ;;
              create-keychain)
                last=
                for argument in "$@"; do last="$argument"; done
                printf 'keychain fixture\n' >"$last"
                ;;
              delete-keychain)
                /bin/rm -f "$2"
                ;;
              set-key-partition-list)
                # macOS 26 does not copy the PKCS#12 friendly name onto the
                # private key's keychain label. Reproduce that real behavior:
                # a label-filtered lookup finds no key, while selecting the
                # only sign-capable private key in this dedicated keychain is
                # accepted.
                for argument in "$@"; do
                  if [ "$argument" = "-l" ]; then
                    echo 'SecItemCopyMatching: The specified item could not be found.' >&2
                    exit 44
                  fi
                done
                ;;
              find-certificate)
                echo 'SHA-1 hash: AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA'
                ;;
            esac
            """,
        )
        write_executable(
            stub / "codesign",
            r"""
            { printf 'codesign'; printf ' %s' "$@"; printf '\n'; } >>"$FIXTURE_ACTION_LOG"
            if [ "$1" = "-d" ]; then
              for argument in "$@"; do
                [ "$argument" = "--entitlements" ] && exit 0
              done
              {
                echo 'Identifier=com.audiohub.daemon'
                echo 'CodeDirectory v=20500 size=123 flags=0x0(none) hashes=1+2 location=embedded'
                echo 'Page size=4096'
                echo 'TeamIdentifier=not set'
                echo 'Internal requirements count=1 size=88'
              } >&2
              exit 0
            fi

            target=
            sign_identity=
            previous=
            dryrun=0
            for argument in "$@"; do
              target="$argument"
              [ "$previous" = "--sign" ] && sign_identity="$argument"
              [ "$argument" = "--dryrun" ] && dryrun=1
              previous="$argument"
            done

            if [ "$1" = "--remove-signature" ]; then
              /usr/bin/sed '/^FIXTURE-CODESIGN:/d' "$target" >"$target.fixture-next" || exit
              /bin/cat "$target.fixture-next" >"$target" || exit
              /bin/rm -f "$target.fixture-next"
              exit 0
            fi

            if [ -n "$sign_identity" ]; then
              if [ "$sign_identity" != "-" ]; then
                [ "$USER" = "$FIXTURE_CONSOLE_USER" ] || exit 8
                [ "$FIXTURE_FAIL_SIGN" = 1 ] && exit 9
              fi
              [ "$dryrun" = 1 ] && exit 0
              printf 'FIXTURE-CODESIGN:%s\n' "$sign_identity" >>"$target"
            fi
            exit 0
            """,
        )
        write_executable(
            stub / "mv",
            r"""
            { printf 'mv'; printf ' %s' "$@"; printf '\n'; } >>"$FIXTURE_ACTION_LOG"
            source="$1"
            [ "$source" = "-f" ] && { shift; source="$1"; }
            destination="$2"
            if [ "$FIXTURE_FAIL_ACTIVATE" = 1 ] \
              && [ "${source##*/}" = payload ] \
              && [ "$destination" = "$FIXTURE_FINAL" ]; then
              exit 10
            fi
            exec /bin/mv "$@"
            """,
        )

        script = DAEMON_INSTALLER.read_text(encoding="utf-8")
        script = script.replace(
            "APP='/Applications/AudioHub.app'", f"APP={shlex.quote(str(app))}"
        )
        script = script.replace(
            "BASE='/Library/Application Support/AudioHub/service'",
            f"BASE={shlex.quote(str(base))}",
        )
        script = script.replace(
            "SIGNING='/Library/Application Support/AudioHub/signing'",
            f"SIGNING={shlex.quote(str(signing))}",
        )
        script = script.replace(
            "LIFECYCLE_LOCK='/var/run/com.audiohub.lifecycle.lock'",
            f"LIFECYCLE_LOCK={shlex.quote(str(lifecycle_lock))}",
        )
        script = script.replace(
            "'/Library/Application Support/AudioHub'", shlex.quote(str(system_root))
        )
        script = script.replace(
            "check_root_dir '/Library/Application Support'",
            f"check_root_dir {shlex.quote(str(application_support))}",
        )
        script = script.replace(
            "check_root_dir '/Library/Application Support/AudioHub'",
            f"check_root_dir {shlex.quote(str(system_root))}",
        )
        script = replace_commands(
            script,
            {
                "/usr/bin/codesign": stub / "codesign",
                "/usr/bin/security": stub / "security",
                "/usr/bin/openssl": stub / "openssl",
                "/usr/bin/install": stub / "install",
                "/usr/bin/plutil": stub / "plutil",
                "/usr/bin/uuidgen": stub / "uuidgen",
                "/usr/bin/stat": stub / "stat",
                "/usr/bin/id": stub / "id",
                "/usr/bin/dscl": stub / "dscl",
                "/usr/bin/sudo": stub / "sudo",
                "/usr/bin/env": stub / "env",
                "/usr/bin/lockf": stub / "lockf",
                "/usr/sbin/chown": stub / "chown",
                "/bin/launchctl": stub / "launchctl",
                "/bin/mv": stub / "mv",
            },
        )
        env = os.environ | {
            "FIXTURE_ACTION_LOG": str(action_log),
            "FIXTURE_LOGIN_KEYCHAIN": str(login_keychain),
            "FIXTURE_FAIL_SIGN": "1" if fail_sign else "0",
            "FIXTURE_FAIL_ACTIVATE": "1" if fail_activate else "0",
            "FIXTURE_LOCK_BUSY": "1" if lock_busy else "0",
            "FIXTURE_FINAL": str(final),
            "FIXTURE_CONSOLE_USER": console_user,
            "FIXTURE_CONSOLE_UID": str(console_uid),
            "FIXTURE_CONSOLE_GID": str(console_gid),
            "FIXTURE_CONSOLE_HOME": str(console_home),
            "FIXTURE_SESSION_ROOTED_MARKER": str(session_rooted_marker),
            "FIXTURE_CONSOLE_SEARCH_STATE": str(console_search_state),
            "FIXTURE_CONCURRENT_KEYCHAIN": str(concurrent_keychain),
            "AUDIOHUB_BIN": str(tmp / "attacker-controlled-daemon"),
            "PATH": str(tmp / "hostile-path"),
            "HOME": str(tmp / "hostile-home"),
            "BASH_ENV": "/dev/null",
            "ENV": "/dev/null",
        }
        paths = {
            "app": app,
            "source": source,
            "base": base,
            "versions": versions,
            "signing": signing,
            "keychain": keychain,
            "keychain_password": keychain_password,
            "final": final,
            "current": base / "current",
            "lifecycle_lock": lifecycle_lock,
            "system_root": system_root,
            "actions": action_log,
            "console_home": console_home,
            "console_search_state": console_search_state,
            "concurrent_keychain": concurrent_keychain,
        }
        return script, env, paths

    def test_daemon_install_fixture_self_signs_once_and_reuses_the_version(self) -> None:
        with tempfile.TemporaryDirectory(prefix="audiohub-daemon-install-", dir="/private/tmp") as raw_tmp:
            script, env, paths = self._make_daemon_install_fixture(Path(raw_tmp))
            source_before = hashlib.sha256(paths["source"].read_bytes()).hexdigest()

            first = subprocess.run(
                ["/bin/sh"], input=script, text=True, env=env, capture_output=True, timeout=30
            )
            self.assertEqual(first.returncode, 0, first.stderr)
            final = paths["final"]
            self.assertTrue((final / "audiohubd").is_file())
            self.assertEqual((final / "source.sha256").read_text().strip(), source_before)
            self.assertEqual(
                (final / "certificate.sha1").read_text().strip(), "a" * 40
            )
            self.assertEqual((final / "audiohubd").stat().st_mode & 0o777, 0o555)
            self.assertTrue(paths["current"].is_symlink())
            self.assertEqual(os.readlink(paths["current"]), f"versions/{source_before}")
            self.assertEqual(os.lstat(paths["current"]).st_mode & 0o777, 0o755)
            self.assertEqual(paths["keychain"].stat().st_mode & 0o777, 0o600)
            self.assertEqual(paths["keychain_password"].stat().st_mode & 0o777, 0o600)
            self.assertEqual(hashlib.sha256(paths["source"].read_bytes()).hexdigest(), source_before)

            actions = paths["actions"].read_text(encoding="utf-8")
            signed_actions = [
                line
                for line in actions.splitlines()
                if line.startswith("codesign --force --sign")
            ]
            self.assertEqual(len(signed_actions), 1)
            self.assertIn("--identifier com.audiohub.daemon", actions)
            self.assertNotIn(f"--force --sign {paths['source']}", actions)
            self.assertIn("launchctl asuser 501", actions)
            self.assertIn("sudo #501", actions)
            self.assertIn("env -i", actions)
            console_search = paths["console_search_state"].read_text().splitlines()
            self.assertIn(str(paths["concurrent_keychain"]), console_search)
            self.assertFalse(
                any("/private/tmp/com.audiohub.sign." in item for item in console_search)
            )

            second = subprocess.run(
                ["/bin/sh"], input=script, text=True, env=env, capture_output=True, timeout=30
            )
            self.assertEqual(second.returncode, 0, second.stderr)
            actions = paths["actions"].read_text(encoding="utf-8")
            self.assertEqual(
                sum(
                    line.startswith("codesign --force --sign")
                    for line in actions.splitlines()
                ),
                1,
            )
            self.assertFalse(any(paths["base"].glob(".install.*")))
            self.assertFalse(any(paths["base"].glob(".previous.*")))

    def test_daemon_install_rebuilds_version_when_signed_record_is_damaged(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="audiohub-daemon-signed-record-", dir="/private/tmp"
        ) as raw_tmp:
            script, env, paths = self._make_daemon_install_fixture(Path(raw_tmp))
            first = subprocess.run(
                ["/bin/sh"], input=script, text=True, env=env, capture_output=True, timeout=30
            )
            self.assertEqual(first.returncode, 0, first.stderr)

            signed_record = paths["final"] / "signed.sha256"
            actual_signed = hashlib.sha256(
                (paths["final"] / "audiohubd").read_bytes()
            ).hexdigest()
            self.assertEqual(signed_record.read_text().strip(), actual_signed)
            signed_record.chmod(0o644)
            signed_record.write_text("0" * 64 + "\n", encoding="utf-8")
            signed_record.chmod(0o444)

            repaired = subprocess.run(
                ["/bin/sh"], input=script, text=True, env=env, capture_output=True, timeout=30
            )
            self.assertEqual(repaired.returncode, 0, repaired.stderr)
            repaired_signed = hashlib.sha256(
                (paths["final"] / "audiohubd").read_bytes()
            ).hexdigest()
            self.assertEqual(signed_record.read_text().strip(), repaired_signed)
            self.assertEqual(signed_record.stat().st_mode & 0o777, 0o444)
            actions = paths["actions"].read_text(encoding="utf-8")
            self.assertEqual(
                sum(
                    line.startswith("codesign --force --sign")
                    for line in actions.splitlines()
                ),
                2,
            )
            self.assertFalse(any(paths["base"].glob(".install.*")))

    def test_daemon_install_rebuilds_a_partial_safe_identity(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="audiohub-daemon-partial-identity-", dir="/private/tmp"
        ) as raw_tmp:
            script, env, paths = self._make_daemon_install_fixture(
                Path(raw_tmp), existing_identity=True
            )
            previous_password = paths["keychain_password"].read_bytes()
            paths["keychain"].unlink()

            repaired = subprocess.run(
                ["/bin/sh"], input=script, text=True, env=env, capture_output=True, timeout=30
            )
            self.assertEqual(repaired.returncode, 0, repaired.stderr)
            self.assertTrue(paths["keychain"].is_file())
            self.assertNotEqual(paths["keychain_password"].read_bytes(), previous_password)
            self.assertEqual(paths["keychain"].stat().st_mode & 0o777, 0o600)
            self.assertEqual(paths["keychain_password"].stat().st_mode & 0o777, 0o600)
            self.assertTrue((paths["signing"] / "certificate.sha1").is_file())
            actions = paths["actions"].read_text(encoding="utf-8")
            self.assertEqual(actions.count("security create-keychain"), 1)
            self.assertFalse(any(paths["base"].glob(".install.*")))

    def test_daemon_install_restores_identity_when_repair_then_activation_fails(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="audiohub-daemon-identity-rollback-", dir="/private/tmp"
        ) as raw_tmp:
            script, env, paths = self._make_daemon_install_fixture(
                Path(raw_tmp),
                existing_identity=True,
                existing_payload=True,
                fail_activate=True,
            )
            previous_daemon = (paths["final"] / "audiohubd").read_bytes()
            previous_keychain = paths["keychain"].read_bytes()
            paths["keychain_password"].write_text(
                "damaged-but-safe\n", encoding="utf-8"
            )
            previous_password = paths["keychain_password"].read_bytes()

            failed = subprocess.run(
                ["/bin/sh"], input=script, text=True, env=env, capture_output=True, timeout=30
            )
            self.assertNotEqual(failed.returncode, 0)
            self.assertIn("could not activate", failed.stderr)
            self.assertEqual((paths["final"] / "audiohubd").read_bytes(), previous_daemon)
            self.assertEqual(paths["keychain"].read_bytes(), previous_keychain)
            self.assertEqual(paths["keychain_password"].read_bytes(), previous_password)
            self.assertFalse((paths["signing"] / "certificate.sha1").exists())
            self.assertFalse(os.path.lexists(paths["current"]))
            actions = paths["actions"].read_text(encoding="utf-8")
            self.assertEqual(actions.count("security create-keychain"), 1)
            self.assertIn("security delete-keychain", actions)
            self.assertFalse(any(paths["base"].glob(".install.*")))

    def test_daemon_install_sign_failure_removes_new_identity_and_partial_payload(self) -> None:
        with tempfile.TemporaryDirectory(prefix="audiohub-daemon-sign-fail-", dir="/private/tmp") as raw_tmp:
            script, env, paths = self._make_daemon_install_fixture(Path(raw_tmp), fail_sign=True)
            source_before = paths["source"].read_bytes()
            result = subprocess.run(
                ["/bin/sh"], input=script, text=True, env=env, capture_output=True, timeout=30
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("could not self-sign", result.stderr)
            self.assertFalse(paths["final"].exists())
            self.assertFalse(os.path.lexists(paths["current"]))
            self.assertFalse(paths["keychain"].exists())
            self.assertFalse(paths["keychain_password"].exists())
            self.assertEqual(paths["source"].read_bytes(), source_before)
            self.assertFalse(any(paths["base"].glob(".install.*")))

    def test_daemon_install_activation_failure_restores_previous_version(self) -> None:
        with tempfile.TemporaryDirectory(prefix="audiohub-daemon-rollback-", dir="/private/tmp") as raw_tmp:
            script, env, paths = self._make_daemon_install_fixture(
                Path(raw_tmp),
                existing_identity=True,
                existing_payload=True,
                fail_activate=True,
            )
            previous = (paths["final"] / "audiohubd").read_bytes()
            result = subprocess.run(
                ["/bin/sh"], input=script, text=True, env=env, capture_output=True, timeout=30
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("could not activate", result.stderr)
            self.assertEqual((paths["final"] / "audiohubd").read_bytes(), previous)
            self.assertTrue(paths["keychain"].is_file())
            self.assertTrue(paths["keychain_password"].is_file())
            self.assertFalse(any(paths["base"].glob(".install.*")))
            self.assertFalse(any(paths["base"].glob(".previous.*")))

    def test_daemon_install_rejects_a_symlink_payload_before_identity_changes(self) -> None:
        with tempfile.TemporaryDirectory(prefix="audiohub-daemon-symlink-", dir="/private/tmp") as raw_tmp:
            script, env, paths = self._make_daemon_install_fixture(Path(raw_tmp))
            real_payload = Path(raw_tmp) / "outside-daemon"
            real_payload.write_bytes(b"must remain untouched\n")
            paths["source"].unlink()
            paths["source"].symlink_to(real_payload)
            result = subprocess.run(
                ["/bin/sh"], input=script, text=True, env=env, capture_output=True, timeout=30
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("payload is missing or unsafe", result.stderr)
            self.assertEqual(real_payload.read_bytes(), b"must remain untouched\n")
            self.assertFalse(paths["signing"].exists())
            actions = paths["actions"].read_text(encoding="utf-8")
            self.assertIn("codesign --verify --deep --strict", actions)
            self.assertNotIn("security", actions)
            self.assertNotIn("openssl", actions)

    def test_daemon_install_rejects_an_unsafe_or_busy_lifecycle_lock_before_mutation(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="audiohub-daemon-lock-link-", dir="/private/tmp"
        ) as raw_tmp:
            script, env, paths = self._make_daemon_install_fixture(Path(raw_tmp))
            outside = Path(raw_tmp) / "outside-lock"
            outside.write_text("do not follow\n", encoding="utf-8")
            paths["lifecycle_lock"].symlink_to(outside)
            result = subprocess.run(
                ["/bin/sh"], input=script, text=True, env=env, capture_output=True
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("lifecycle lock is a symlink or has unsafe metadata", result.stderr)
            self.assertEqual(outside.read_text(encoding="utf-8"), "do not follow\n")
            self.assertFalse(paths["system_root"].exists())
            actions = paths["actions"].read_text(encoding="utf-8")
            self.assertNotIn("security", actions)
            self.assertNotIn("openssl", actions)

        with tempfile.TemporaryDirectory(
            prefix="audiohub-daemon-lock-busy-", dir="/private/tmp"
        ) as raw_tmp:
            script, env, paths = self._make_daemon_install_fixture(
                Path(raw_tmp), lock_busy=True
            )
            result = subprocess.run(
                ["/bin/sh"], input=script, text=True, env=env, capture_output=True
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("another AudioHub install or uninstall", result.stderr)
            self.assertTrue(paths["lifecycle_lock"].is_file())
            self.assertEqual(paths["lifecycle_lock"].stat().st_mode & 0o777, 0o600)
            self.assertFalse(paths["system_root"].exists())
            actions = paths["actions"].read_text(encoding="utf-8")
            self.assertNotIn("security", actions)
            self.assertNotIn("openssl", actions)

    def _make_uninstall_fixture(
        self,
        tmp: Path,
        app_id: str = "com.audiohub.app",
        driver_id: str = "com.audiohub.driver",
        include_untrusted: bool = False,
        include_offline_home: bool = False,
        fail_coreaudio: bool = False,
        fail_retired_bootout: bool = False,
        survive_product_kill: bool = False,
        lock_busy: bool = False,
    ) -> tuple[str, dict[str, str], dict[str, Path]]:
        stub_dir = tmp / "stub"
        stub_dir.mkdir()
        fixture_root = tmp / "root"
        run_dir = fixture_root / "var/run"
        run_dir.mkdir(parents=True)
        lifecycle_lock = run_dir / "com.audiohub.lifecycle.lock"
        app = fixture_root / "Applications/AudioHub.app"
        driver = fixture_root / "Library/Audio/Plug-Ins/HAL/AudioHubDriver.driver"
        retired_system = fixture_root / "Library/LaunchDaemons/com.audiohub.daemon.plist"
        system_root = fixture_root / "Library/Application Support/AudioHub"
        service = system_root / "service"
        signing = system_root / "signing"
        installed_service = service / "current/audiohubd"
        for bundle, bundle_id in ((app, app_id), (driver, driver_id)):
            (bundle / "Contents").mkdir(parents=True)
            with (bundle / "Contents/Info.plist").open("wb") as plist:
                plistlib.dump({"CFBundleIdentifier": bundle_id}, plist)
        retired_system.parent.mkdir(parents=True, exist_ok=True)
        retired_system.write_text("retired\n", encoding="utf-8")
        installed_service.parent.mkdir(parents=True)
        installed_service.write_text("installed daemon fixture\n", encoding="utf-8")
        signing.mkdir(parents=True)
        (signing / "identity.keychain-db").write_text("keychain fixture\n", encoding="utf-8")
        (signing / "keychain.pass").write_text("password fixture\n", encoding="utf-8")
        (signing / "identity.keychain-db").chmod(0o600)
        (signing / "keychain.pass").chmod(0o600)
        retired_job_state = tmp / "retired-system-job"
        retired_job_state.write_text("loaded\n", encoding="utf-8")

        homes = {
            "alice": fixture_root / "Users/alice",
            "netuser": fixture_root / "Network/Users/netuser",
            "escape": fixture_root / "escape",
        }
        for name in ("alice", "netuser", "escape"):
            home = homes[name]
            (home / "Library/LaunchAgents").mkdir(parents=True)
            config = home / "Library/Application Support/AudioHub"
            (config / "logs").mkdir(parents=True)
            for label in ("com.audiohub.app.autostart.plist", "com.audiohub.daemon.plist"):
                (home / "Library/LaunchAgents" / label).write_text("agent\n", encoding="utf-8")
            for lifecycle in ("service-installed-v1", "ipc.json"):
                (config / lifecycle).write_text("lifecycle\n", encoding="utf-8")
            for preserved in ("settings.json", "identity.json", "paired_peers.json"):
                (config / preserved).write_text("preserve\n", encoding="utf-8")
            (config / "logs/audiohub.log").write_text("preserve\n", encoding="utf-8")
        symlink_home = fixture_root / "Users/linkhome"
        symlink_home.parent.mkdir(parents=True, exist_ok=True)
        symlink_home.symlink_to(homes["escape"])

        process_state = tmp / "processes"
        signal_log = tmp / "signals"
        process_state.write_text(
            f"201|{app}/Contents/MacOS/audiohub-app|term-exits\n"
            f"202|{app}/Contents/MacOS/audiohubd|{'kill-survives' if survive_product_kill else 'stubborn'}\n"
            f"204|{installed_service}|stubborn\n"
            "203|/opt/unrelated/audiohubd|stubborn\n"
            "205|/System/Library/Frameworks/CoreAudio.framework/Versions/A/XPCServices/com.apple.audio.Core-Audio-Driver-Service.helper.xpc/Contents/MacOS/com.apple.audio.Core-Audio-Driver-Service.helper|stubborn\n"
            "206|Core Audio Driver (AudioHubDriver.driver)|stubborn\n"
            "207|Core Audio Driver (UnrelatedAudio.driver)|stubborn\n",
            encoding="utf-8",
        )
        commands = self._process_stubs(stub_dir)

        write_executable(
            stub_dir / "dscl",
            r"""
            if [ "$2" = "-list" ]; then
              if [ "$FIXTURE_INCLUDE_UNTRUSTED" = "1" ]; then
                printf '%s\n' 'root 0' '_system 499' 'alice 501' 'netuser 502' 'linkuser 503' 'traversal 504' 'wrongowner 505'
              elif [ "$FIXTURE_INCLUDE_OFFLINE_HOME" = "1" ]; then
                printf '%s\n' 'root 0' '_system 499' 'alice 501' 'netuser 502' 'offline 506'
              else
                printf '%s\n' 'root 0' '_system 499' 'alice 501' 'netuser 502'
              fi
              exit 0
            fi
            name=${3#/Users/}
            case "$name" in
              alice) uid=501; home="$FIXTURE_ROOT/Users/alice" ;;
              netuser) uid=502; home="$FIXTURE_ROOT/Network/Users/netuser" ;;
              linkuser) uid=503; home="$FIXTURE_ROOT/Users/linkhome" ;;
              traversal) uid=504; home="$FIXTURE_ROOT/Users/alice/../escape" ;;
              wrongowner) uid=505; home="$FIXTURE_ROOT/escape" ;;
              offline) uid=506; home="$FIXTURE_ROOT/Network/Servers/offline/Users/offline" ;;
              *) exit 1 ;;
            esac
            printf 'NFSHomeDirectory: %s\nUniqueID: %s\n' "$home" "$uid"
            """,
        )
        write_executable(
            stub_dir / "id",
            r"""
            case "$1:$2" in
              -nu:501) echo alice;; -nu:502) echo netuser;; -nu:503) echo linkuser;;
              -nu:504) echo traversal;; -nu:505) echo wrongowner;;
              -nu:506) echo offline;;
              -u:alice) echo 501;; -u:netuser) echo 502;; -u:linkuser) echo 503;;
              -u:traversal) echo 504;; -u:wrongowner) echo 505;;
              -u:offline) echo 506;;
              *) exit 1;;
            esac
            """,
        )
        write_executable(
            stub_dir / "stat",
            r"""
            if [ "$1" = "-f" ] && [ "$2" = "%u" ]; then
              case "$3" in
                "$FIXTURE_ROOT/Users/alice") echo 501; exit 0;;
                "$FIXTURE_ROOT/Network/Users/netuser") echo 502; exit 0;;
                "$FIXTURE_ROOT/escape") echo 999; exit 0;;
                "$FIXTURE_SYSTEM_ROOT"|"$FIXTURE_SYSTEM_ROOT"/*) echo 0; exit 0;;
                "$FIXTURE_LIFECYCLE_LOCK"|"$FIXTURE_LIFECYCLE_LOCK".candidate.*) echo 0; exit 0;;
              esac
            fi
            if [ "$1" = "-f" ] && [ "$2" = "%g" ]; then echo 0; exit 0; fi
            exec /usr/bin/stat "$@"
            """,
        )
        write_executable(
            stub_dir / "sudo",
            r"""
            [ "$1" = "-H" ] && shift
            [ "$1" = "-u" ] || exit 2
            user="$2"
            shift 2
            printf 'sudo %s\n' "$user" >>"$FIXTURE_ACTION_LOG"
            exec "$@"
            """,
        )
        write_executable(
            stub_dir / "rm",
            r"""
            if [ "$1" = "-f" ]; then
              shift
              for target in "$@"; do
                case "$target" in
                  "$FIXTURE_ROOT"/*) /bin/rm -f "$target" ;;
                  *) exit 2 ;;
                esac
              done
              exit 0
            fi
            if [ "$1" = "-rf" ]; then
              shift
              for target in "$@"; do
                case "$target" in
                  "$FIXTURE_ROOT/Applications/AudioHub.app"|\
                  "$FIXTURE_ROOT/Library/Audio/Plug-Ins/HAL/AudioHubDriver.driver"|\
                  "$FIXTURE_SYSTEM_ROOT/service"|"$FIXTURE_SYSTEM_ROOT/signing")
                    /bin/rm -rf "$target" ;;
                  *) exit 2 ;;
                esac
              done
              exit 0
            fi
            exit 2
            """,
        )
        write_executable(
            stub_dir / "launchctl",
            r"""
            { printf 'launchctl'; printf ' %s' "$@"; printf '\n'; } >>"$FIXTURE_ACTION_LOG"
            if [ "$1:$2" = "print:system/com.audiohub.daemon" ]; then
              [ -f "$FIXTURE_RETIRED_JOB_STATE" ]
              exit $?
            fi
            if [ "$1:$2" = "bootout:system/com.audiohub.daemon" ]; then
              [ "$FIXTURE_FAIL_RETIRED_BOOTOUT" = "1" ] && exit 1
              /bin/rm -f "$FIXTURE_RETIRED_JOB_STATE"
              exit 0
            fi
            if [ "$1" = "kickstart" ] && [ "$FIXTURE_FAIL_COREAUDIO" = "1" ]; then exit 1; fi
            if [ "$1" = "print" ] && [ "$2" = "system/com.apple.audio.coreaudiod" ]; then
              [ "$FIXTURE_FAIL_COREAUDIO" = "1" ] && exit 1
              echo 'state = running'
            fi
            """,
        )
        write_executable(
            stub_dir / "pkgutil",
            '{ printf \'pkgutil\'; printf \' %s\' "$@"; printf \'\\n\'; } >>"$FIXTURE_ACTION_LOG"\n',
        )
        write_executable(
            stub_dir / "security",
            r"""
            { printf 'security'; printf ' %s' "$@"; printf '\n'; } >>"$FIXTURE_ACTION_LOG"
            if [ "$1" = delete-keychain ]; then /bin/rm -f "$2"; fi
            """,
        )
        write_executable(stub_dir / "chown", ":\n")
        write_executable(
            stub_dir / "lockf",
            '[ "$FIXTURE_LOCK_BUSY" = 1 ] && exit 75\nexec /usr/bin/lockf "$@"\n',
        )
        write_executable(stub_dir / "killall", '[ "$FIXTURE_FAIL_COREAUDIO" = "1" ] && exit 1\nexit 0\n')

        shell = extract_applescript_string(UNINSTALLER.read_text(encoding="utf-8"), "rootCommand")
        shell = shell.replace("APP='/Applications/AudioHub.app'", f"APP={shlex.quote(str(app))}")
        shell = shell.replace(
            "DRIVER='/Library/Audio/Plug-Ins/HAL/AudioHubDriver.driver'",
            f"DRIVER={shlex.quote(str(driver))}",
        )
        shell = shell.replace(
            "RETIRED_SYSTEM_PLIST='/Library/LaunchDaemons/com.audiohub.daemon.plist'",
            f"RETIRED_SYSTEM_PLIST={shlex.quote(str(retired_system))}",
        )
        shell = shell.replace(
            "PRODUCT_SUPPORT='/Library/Application Support/AudioHub'",
            f"PRODUCT_SUPPORT={shlex.quote(str(system_root))}",
        )
        shell = shell.replace(
            "SERVICE='/Library/Application Support/AudioHub/service'",
            f"SERVICE={shlex.quote(str(service))}",
        )
        shell = shell.replace(
            "SERVICE_VERSIONS='/Library/Application Support/AudioHub/service/versions'",
            f"SERVICE_VERSIONS={shlex.quote(str(service / 'versions'))}",
        )
        shell = shell.replace(
            "SERVICE_CURRENT='/Library/Application Support/AudioHub/service/current/audiohubd'",
            f"SERVICE_CURRENT={shlex.quote(str(installed_service))}",
        )
        shell = shell.replace(
            "SIGNING='/Library/Application Support/AudioHub/signing'",
            f"SIGNING={shlex.quote(str(signing))}",
        )
        shell = shell.replace(
            "LIFECYCLE_LOCK='/var/run/com.audiohub.lifecycle.lock'",
            f"LIFECYCLE_LOCK={shlex.quote(str(lifecycle_lock))}",
        )
        shell = shell.replace(
            "/Users/?*|/Network/Users/?*|/Network/Servers/?*",
            f"{fixture_root}/Users/?*|{fixture_root}/Network/Users/?*|{fixture_root}/Network/Servers/?*",
        )
        commands |= {
            "/usr/bin/dscl": stub_dir / "dscl",
            "/usr/bin/id": stub_dir / "id",
            "/usr/bin/stat": stub_dir / "stat",
            "/usr/bin/sudo": stub_dir / "sudo",
            "/bin/launchctl": stub_dir / "launchctl",
            "/usr/sbin/pkgutil": stub_dir / "pkgutil",
            "/usr/bin/killall": stub_dir / "killall",
            "/usr/bin/security": stub_dir / "security",
            "/usr/bin/lockf": stub_dir / "lockf",
            "/usr/sbin/chown": stub_dir / "chown",
            "/bin/rm": stub_dir / "rm",
        }
        shell = replace_commands(shell, commands)
        action_log = tmp / "actions"
        env = os.environ | {
            "FIXTURE_ROOT": str(fixture_root),
            "FIXTURE_SYSTEM_ROOT": str(system_root),
            "FIXTURE_LIFECYCLE_LOCK": str(lifecycle_lock),
            "FIXTURE_PROCESS_STATE": str(process_state),
            "FIXTURE_SIGNAL_LOG": str(signal_log),
            "FIXTURE_ACTION_LOG": str(action_log),
            "FIXTURE_INCLUDE_UNTRUSTED": "1" if include_untrusted else "0",
            "FIXTURE_INCLUDE_OFFLINE_HOME": "1" if include_offline_home else "0",
            "FIXTURE_FAIL_COREAUDIO": "1" if fail_coreaudio else "0",
            "FIXTURE_FAIL_RETIRED_BOOTOUT": "1" if fail_retired_bootout else "0",
            "FIXTURE_RETIRED_JOB_STATE": str(retired_job_state),
            "FIXTURE_LOCK_BUSY": "1" if lock_busy else "0",
            "BASH_ENV": "/dev/null",
        }
        paths = {
            "app": app,
            "driver": driver,
            "retired_system": retired_system,
            "retired_job_state": retired_job_state,
            "system_root": system_root,
            "service": service,
            "signing": signing,
            "installed_service": installed_service,
            "lifecycle_lock": lifecycle_lock,
            "alice": homes["alice"],
            "netuser": homes["netuser"],
            "escape": homes["escape"],
            "signals": signal_log,
            "actions": action_log,
        }
        return shell, env, paths

    def test_uninstaller_cleans_all_trusted_users_and_preserves_configuration(self) -> None:
        with tempfile.TemporaryDirectory(prefix="audiohub-uninstall-", dir="/private/tmp") as raw_tmp:
            shell, env, paths = self._make_uninstall_fixture(Path(raw_tmp))
            result = subprocess.run(
                ["/bin/sh"], input=shell, text=True, env=env, capture_output=True, timeout=60
            )
            self.assertEqual(result.returncode, 0, result.stderr)

            for key in ("alice", "netuser"):
                home = paths[key]
                agents = home / "Library/LaunchAgents"
                config = home / "Library/Application Support/AudioHub"
                self.assertFalse(os.path.lexists(agents / "com.audiohub.app.autostart.plist"))
                self.assertFalse(os.path.lexists(agents / "com.audiohub.daemon.plist"))
                self.assertFalse(os.path.lexists(config / "service-installed-v1"))
                self.assertFalse(os.path.lexists(config / "ipc.json"))
                for preserved in ("settings.json", "identity.json", "paired_peers.json", "logs/audiohub.log"):
                    self.assertTrue((config / preserved).is_file(), preserved)

            self.assertFalse(paths["app"].exists())
            self.assertFalse(paths["driver"].exists())
            self.assertFalse(paths["retired_system"].exists())
            self.assertFalse(paths["retired_job_state"].exists())
            self.assertFalse(paths["service"].exists())
            self.assertFalse(paths["signing"].exists())

            actions = paths["actions"].read_text(encoding="utf-8")
            for uid in range(501, 503):
                for label in ("com.audiohub.app.autostart", "com.audiohub.daemon"):
                    self.assertIn(f"bootout gui/{uid}/{label}", actions)
            self.assertNotIn("gui/0/", actions)
            self.assertNotIn("gui/499/", actions)
            self.assertIn("kickstart -kp system/com.apple.audio.coreaudiod", actions)

            events = paths["signals"].read_text(encoding="utf-8").splitlines()
            self.assertIn("-TERM 201", events)
            self.assertIn("-TERM 202", events)
            self.assertIn("-KILL 202", events)
            self.assertIn("-TERM 204", events)
            self.assertIn("-KILL 204", events)
            self.assertIn("-TERM 205", events)
            self.assertIn("-KILL 205", events)
            self.assertIn("-TERM 206", events)
            self.assertIn("-KILL 206", events)
            self.assertFalse(any(line.endswith(" 203") for line in events))
            self.assertFalse(any(line.endswith(" 207") for line in events))

    def test_untrusted_home_stops_uninstall_without_following_paths(self) -> None:
        with tempfile.TemporaryDirectory(prefix="audiohub-untrusted-", dir="/private/tmp") as raw_tmp:
            shell, env, paths = self._make_uninstall_fixture(Path(raw_tmp), include_untrusted=True)
            result = subprocess.run(["/bin/sh"], input=shell, text=True, env=env, capture_output=True)
            self.assertEqual(result.returncode, 24, result.stderr)
            self.assertIn("Could not validate an AudioHub user home", result.stderr)

            # A symlink home, traversal spelling, and UID-owner mismatch all point
            # at this fixture. None may turn into privileged deletion there.
            escape_config = paths["escape"] / "Library/Application Support/AudioHub"
            self.assertTrue((escape_config / "service-installed-v1").is_file())
            self.assertTrue((escape_config / "ipc.json").is_file())
            self.assertTrue((paths["escape"] / "Library/LaunchAgents/com.audiohub.daemon.plist").is_file())
            self.assertTrue(paths["app"].exists())
            self.assertTrue(paths["driver"].exists())
            self.assertTrue(paths["service"].exists())
            self.assertTrue(paths["signing"].exists())

    def test_unreachable_network_home_without_visible_lifecycle_state_is_skipped(self) -> None:
        with tempfile.TemporaryDirectory(prefix="audiohub-offline-home-", dir="/private/tmp") as raw_tmp:
            shell, env, paths = self._make_uninstall_fixture(
                Path(raw_tmp), include_offline_home=True
            )
            result = subprocess.run(
                ["/bin/sh"], input=shell, text=True, env=env, capture_output=True, timeout=60
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertFalse(paths["app"].exists())
            self.assertFalse(paths["driver"].exists())
            self.assertFalse(paths["retired_system"].exists())
            actions = paths["actions"].read_text(encoding="utf-8")
            self.assertIn("bootout gui/506/com.audiohub.app.autostart", actions)
            self.assertNotIn("Could not validate an AudioHub user home", result.stderr)

    def test_retired_system_job_bootout_is_verified_before_payload_removal(self) -> None:
        with tempfile.TemporaryDirectory(prefix="audiohub-retired-absent-", dir="/private/tmp") as raw_tmp:
            shell, env, paths = self._make_uninstall_fixture(Path(raw_tmp))
            paths["retired_job_state"].unlink()
            result = subprocess.run(
                ["/bin/sh"], input=shell, text=True, env=env, capture_output=True, timeout=60
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            actions = paths["actions"].read_text(encoding="utf-8")
            self.assertIn("print system/com.audiohub.daemon", actions)
            self.assertNotIn("bootout system/com.audiohub.daemon", actions)
            self.assertFalse(paths["retired_system"].exists())

        with tempfile.TemporaryDirectory(prefix="audiohub-retired-loaded-", dir="/private/tmp") as raw_tmp:
            shell, env, paths = self._make_uninstall_fixture(
                Path(raw_tmp), fail_retired_bootout=True
            )
            result = subprocess.run(
                ["/bin/sh"], input=shell, text=True, env=env, capture_output=True, timeout=60
            )
            self.assertEqual(result.returncode, 27, result.stderr)
            self.assertIn("Could not unload the retired AudioHub system daemon", result.stderr)
            self.assertTrue(paths["retired_job_state"].is_file())
            self.assertTrue(paths["retired_system"].is_file())
            self.assertTrue(paths["app"].exists())
            self.assertTrue(paths["driver"].exists())
            self.assertTrue(paths["service"].exists())
            self.assertTrue(paths["signing"].exists())

    def test_uninstaller_preflights_app_and_driver_identity_before_cleanup(self) -> None:
        for app_id, driver_id, expected_code in (
            ("example.not-audiohub", "com.audiohub.driver", 30),
            ("com.audiohub.app", "example.not-audiohub", 31),
        ):
            with self.subTest(app_id=app_id, driver_id=driver_id):
                with tempfile.TemporaryDirectory(prefix="audiohub-identity-", dir="/private/tmp") as raw_tmp:
                    shell, env, paths = self._make_uninstall_fixture(
                        Path(raw_tmp), app_id=app_id, driver_id=driver_id
                    )
                    result = subprocess.run(["/bin/sh"], input=shell, text=True, env=env, capture_output=True)
                    self.assertEqual(result.returncode, expected_code, result.stderr)
                    self.assertTrue(paths["app"].exists())
                    self.assertTrue(paths["driver"].exists())
                    self.assertTrue(paths["service"].exists())
                    self.assertTrue(paths["signing"].exists())
                    marker = paths["alice"] / "Library/Application Support/AudioHub/service-installed-v1"
                    self.assertTrue(marker.is_file())
                    self.assertFalse(paths["actions"].exists())

    def test_uninstaller_rejects_busy_lock_before_any_product_mutation(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="audiohub-uninstall-lock-busy-", dir="/private/tmp"
        ) as raw_tmp:
            shell, env, paths = self._make_uninstall_fixture(
                Path(raw_tmp), lock_busy=True
            )
            result = subprocess.run(
                ["/bin/sh"], input=shell, text=True, env=env, capture_output=True
            )
            self.assertEqual(result.returncode, 35, result.stderr)
            self.assertIn("another AudioHub install or uninstall", result.stderr)
            self.assertTrue(paths["app"].exists())
            self.assertTrue(paths["driver"].exists())
            self.assertTrue(paths["service"].exists())
            self.assertTrue(paths["signing"].exists())
            marker = paths["alice"] / "Library/Application Support/AudioHub/service-installed-v1"
            self.assertTrue(marker.is_file())
            self.assertFalse(paths["actions"].exists())

    def test_uninstaller_rejects_damaged_machine_state_before_any_product_mutation(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="audiohub-uninstall-service-link-", dir="/private/tmp"
        ) as raw_tmp:
            shell, env, paths = self._make_uninstall_fixture(Path(raw_tmp))
            outside_service = Path(raw_tmp) / "outside-service"
            paths["service"].rename(outside_service)
            paths["service"].symlink_to(outside_service, target_is_directory=True)
            result = subprocess.run(
                ["/bin/sh"], input=shell, text=True, env=env, capture_output=True
            )
            self.assertEqual(result.returncode, 33, result.stderr)
            self.assertIn("unsafe path", result.stderr)
            self.assertTrue(outside_service.exists())
            self.assertTrue(paths["app"].exists())
            self.assertTrue(paths["driver"].exists())
            marker = paths["alice"] / "Library/Application Support/AudioHub/service-installed-v1"
            self.assertTrue(marker.is_file())
            self.assertFalse(paths["actions"].exists())

        with tempfile.TemporaryDirectory(
            prefix="audiohub-uninstall-signing-mode-", dir="/private/tmp"
        ) as raw_tmp:
            shell, env, paths = self._make_uninstall_fixture(Path(raw_tmp))
            keychain = paths["signing"] / "identity.keychain-db"
            keychain.chmod(0o644)
            result = subprocess.run(
                ["/bin/sh"], input=shell, text=True, env=env, capture_output=True
            )
            self.assertEqual(result.returncode, 34, result.stderr)
            self.assertIn("signing file is not private", result.stderr)
            self.assertTrue(paths["app"].exists())
            self.assertTrue(paths["driver"].exists())
            self.assertTrue(paths["service"].exists())
            marker = paths["alice"] / "Library/Application Support/AudioHub/service-installed-v1"
            self.assertTrue(marker.is_file())
            self.assertFalse(paths["actions"].exists())

    def test_coreaudio_reload_failure_keeps_app_and_receipts(self) -> None:
        with tempfile.TemporaryDirectory(prefix="audiohub-coreaudio-", dir="/private/tmp") as raw_tmp:
            shell, env, paths = self._make_uninstall_fixture(Path(raw_tmp), fail_coreaudio=True)
            result = subprocess.run(
                ["/bin/sh"], input=shell, text=True, env=env, capture_output=True, timeout=60
            )
            self.assertEqual(result.returncode, 32, result.stderr)
            self.assertTrue(paths["app"].exists())
            self.assertFalse(paths["driver"].exists())
            self.assertTrue(paths["service"].exists())
            self.assertTrue(paths["signing"].exists())
            actions = paths["actions"].read_text(encoding="utf-8")
            self.assertNotIn("pkgutil --forget com.audiohub.app.pkg", actions)
            self.assertNotIn("pkgutil --forget com.audiohub.driver.pkg", actions)

    def test_uninstaller_keeps_app_and_driver_when_product_survives_sigkill(self) -> None:
        with tempfile.TemporaryDirectory(prefix="audiohub-stuck-process-", dir="/private/tmp") as raw_tmp:
            shell, env, paths = self._make_uninstall_fixture(
                Path(raw_tmp), survive_product_kill=True
            )
            result = subprocess.run(
                ["/bin/sh"], input=shell, text=True, env=env, capture_output=True, timeout=60
            )
            self.assertEqual(result.returncode, 26, result.stderr)
            self.assertIn("Could not stop every AudioHub product process", result.stderr)
            self.assertTrue(paths["app"].exists())
            self.assertTrue(paths["driver"].exists())
            self.assertTrue(paths["service"].exists())
            self.assertTrue(paths["signing"].exists())
            actions = paths["actions"].read_text(encoding="utf-8")
            self.assertNotIn("pkgutil --forget com.audiohub.app.pkg", actions)
            self.assertNotIn("pkgutil --forget com.audiohub.driver.pkg", actions)


if __name__ == "__main__":
    unittest.main(verbosity=2)
