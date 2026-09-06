#!/bin/zsh
# Build the HAL driver and turn that exact signed bundle into the component pkg
# embedded in AudioHub.app. The outer AudioHub.pkg installs only the App; this
# nested package is used later by the explicit Settings > Mode driver action.
set -euo pipefail

ROOT="$(cd "${0:a:h}/.." && pwd)"
DRIVER_DIR="$ROOT/drivers/macos-hal"
OUT="$DRIVER_DIR/build/AudioHubDriver.pkg"
SCRIPTS="$ROOT/app/installer/macos/driver-scripts"
# Derived, never written twice; see the note in build-macos-pkg.sh.
VERSION="${AUDIOHUB_VERSION:-}"
if [[ -z "$VERSION" ]]; then
  VERSION="$(/usr/bin/plutil -extract version raw -o - "$ROOT/app/src-tauri/tauri.conf.json")" \
    || { print -u2 -- "[audiohub] ERROR: cannot read version from app/src-tauri/tauri.conf.json"; exit 1; }
  [[ -n "$VERSION" ]] \
    || { print -u2 -- "[audiohub] ERROR: empty version in app/src-tauri/tauri.conf.json"; exit 1; }
fi

if [[ "${AUDIOHUB_RELEASE:-0}" == 1 ]]; then
  [[ "${AUDIOHUB_DRIVER_SIGN_IDENTITY:-}" == Developer\ ID\ Application:* ]] \
    || { print -u2 -- "release driver requires AUDIOHUB_DRIVER_SIGN_IDENTITY='Developer ID Application: …'"; exit 1; }
  [[ "${AUDIOHUB_INSTALLER_IDENTITY:-}" == Developer\ ID\ Installer:* ]] \
    || { print -u2 -- "release driver package requires AUDIOHUB_INSTALLER_IDENTITY='Developer ID Installer: …'"; exit 1; }
fi

[[ "$(uname -s)" == "Darwin" ]] || { print -u2 -- "macOS is required"; exit 1; }
zsh "$DRIVER_DIR/build.sh"

STAGE="$(mktemp -d)"
cleanup() { rm -rf "$STAGE"; }
trap cleanup EXIT

DEST="$STAGE/Library/Audio/Plug-Ins/HAL"
mkdir -p "$DEST"
/usr/bin/ditto "$DRIVER_DIR/build/AudioHubDriver.driver" "$DEST/AudioHubDriver.driver"
# CoreAudio's driver host must read the root-owned payload after installation,
# even when the build was invoked from a private-umask test workspace.
/usr/bin/find "$STAGE" -type d -exec /bin/chmod 0755 {} +
/usr/bin/find "$STAGE" -type f -perm -100 -exec /bin/chmod 0755 {} +
/usr/bin/find "$STAGE" -type f ! -perm -100 -exec /bin/chmod 0644 {} +
/usr/bin/codesign --verify --deep --strict "$DEST/AudioHubDriver.driver"

args=(
  --root "$STAGE"
  --identifier com.audiohub.driver.pkg
  --version "$VERSION"
  --install-location /
  --scripts "$SCRIPTS"
)
if [[ -n "${AUDIOHUB_INSTALLER_IDENTITY:-}" ]]; then
  args+=(--sign "$AUDIOHUB_INSTALLER_IDENTITY")
fi
pkgbuild "${args[@]}" "$OUT"

[[ -s "$OUT" ]] || { print -u2 -- "driver package was not produced: $OUT"; exit 1; }
pkgutil --payload-files "$OUT" | grep -q 'AudioHubDriver.driver/Contents/MacOS/AudioHubDriver' \
  || { print -u2 -- "driver package has no AudioHubDriver payload"; exit 1; }
print -- "$OUT"
