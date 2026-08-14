#!/bin/zsh
# Build the distributable macOS AudioHub.app (spec-app.md §2).
#   0) npm install + vite build  (app/frontend -> app/ui, what Tauri embeds)
#   1) build audiohubd + internal CLI
#   2) build/sign the HAL driver and its privileged component package
#   3) icons: make-icons.py -> sips/iconutil -> icon.icns   (system tools only)
#   4) stage both Rust sidecars and run cargo tauri build --bundles app
#   5) verify the bundle, then assemble the AudioHub.pkg and final DMG
#
# Without release identities this is a development build: it is locally signed
# (or ad-hoc) and not notarised. AUDIOHUB_RELEASE=1 fails closed unless the
# Developer ID identities and notarytool keychain profile are all supplied.
set -euo pipefail

APP_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$APP_DIR/.." && pwd)"
TAURI_DIR="$APP_DIR/src-tauri"
ICONS="$TAURI_DIR/icons"

export PATH="$HOME/.cargo/bin:$PATH"

step() { print -ru2 -- "[audiohub] ==== $* ===="; }
die()  { print -ru2 -- "[audiohub] ERROR: $*"; exit 1; }

[[ "$(uname -s)" == "Darwin" ]] || die "build-app.sh builds the macOS .app; run it on macOS"

# Every official macOS build mutates shared inputs before the App is signed:
# Vite's app/ui, the target-suffixed sidecars, the driver package whose digest
# build.rs embeds, and Tauri's bundle directory. Serialise that whole sequence,
# rather than merely the final packaging commands, so two builds can never
# combine one process's compiled digest with the other process's resource.
#
# The lock lives in Git's private metadata, is opened without truncation, and
# is deliberately never unlinked. BSD lockf locks the open file description;
# merely finding this persistent file after a crash does not make it stale.
GIT_COMMON_DIR="$(git -C "$ROOT" rev-parse --path-format=absolute --git-common-dir 2>/dev/null || true)"
[[ -n "$GIT_COMMON_DIR" && -d "$GIT_COMMON_DIR" ]] \
  || die "could not locate Git metadata for the macOS build lock"
BUILD_LOCK="$GIT_COMMON_DIR/audiohub-macos-build.lock"
if [[ "${AUDIOHUB_MACOS_BUILD_LOCK_PATH:-}" == "$BUILD_LOCK" && -e /dev/fd/8 ]] && \
   [[ "$(/usr/bin/stat -f %i /dev/fd/8 2>/dev/null || true)" == \
      "$(/usr/bin/stat -f %i "$BUILD_LOCK" 2>/dev/null || true)" ]]; then
  : # A parent build-app.sh already owns this inherited descriptor.
else
  [[ ! -L "$BUILD_LOCK" ]] || die "refusing a symlinked macOS build lock: $BUILD_LOCK"
  exec 8>>"$BUILD_LOCK"
  /bin/chmod 0600 "$BUILD_LOCK"
  /usr/bin/lockf -s -t 0 8 \
    || die "another macOS AudioHub build is already running"
  export AUDIOHUB_MACOS_BUILD_LOCK_PATH="$BUILD_LOCK"
fi

TRIPLE="$(rustc -vV | awk '/^host: /{print $2}')"
[[ -n "$TRIPLE" ]] || die "could not determine host triple from rustc -vV"

# Keep the binary-distribution notices tied to the exact locked dependency
# graphs being built. The generator is deterministic and refuses unresolved
# licenses, unpinned cargo-about versions, and accidental local path leakage.
step "license inventory (locked Cargo graphs)"
command -v node >/dev/null 2>&1 || die "node not found — required to generate third-party notices"
node "$ROOT/scripts/generate-third-party-licenses.mjs"
[[ -s "$ROOT/THIRD-PARTY-LICENSES.html" ]] \
  || die "third-party license report was not generated"

# ---------------------------------------------------------------- 0) frontend
# The UI is Vite + React (source: app/frontend, output: app/ui). It is built
# FIRST and explicitly, not left to Tauri's beforeBuildCommand, for two reasons:
#   · a type error should fail the build in 10 seconds, not after cargo has
#     spent two minutes linking the shell;
#   · `frontendDist` is a plain directory, so a stale or empty app/ui would be
#     bundled silently — the app would ship whatever was there last time.
# beforeBuildCommand in tauri.conf.json runs it a second time (cheap, ~1s) so a
# bare `cargo tauri build` is safe too.
FRONTEND="$APP_DIR/frontend"
step "0/5 frontend (npm + vite build -> app/ui)"
command -v npm >/dev/null 2>&1 || die "npm not found — the UI is a Vite/React build now; install Node 18+"
if [[ ! -d "$FRONTEND/node_modules" ]]; then
  print -ru2 -- "[audiohub] node_modules missing; running npm install"
  ( cd "$FRONTEND" && npm install --no-audit --no-fund )
fi
( cd "$FRONTEND" && npm run build )
[[ -f "$APP_DIR/ui/index.html" ]] || die "vite produced no app/ui/index.html"
print -ru2 -- "[audiohub] frontend: $(ls "$APP_DIR/ui/assets" | tr '\n' ' ')"

# ------------------------------------------------------------- 1) daemon + CLI
step "1/5 build daemon and internal CLI"
cargo build --release --manifest-path "$ROOT/Cargo.toml" -p audiohub-cli -p audiohubd
CLI="$ROOT/target/release/audiohub"
DAEMON="$ROOT/target/release/audiohubd"
[[ -x "$CLI" ]] || die "CLI binary missing after build: $CLI"
[[ -x "$DAEMON" ]] || die "daemon binary missing after build: $DAEMON"

# ---------------------------------------------------------- 2) HAL driver pkg
step "2/5 build signed HAL driver component package"
zsh "$ROOT/scripts/build-macos-driver-pkg.sh"
DRIVER_PKG="$ROOT/drivers/macos-hal/build/AudioHubDriver.pkg"
[[ -s "$DRIVER_PKG" ]] || die "driver package missing after build: $DRIVER_PKG"
DAEMON_INSTALLER="$ROOT/app/installer/macos/install-daemon.sh"
[[ -x "$DAEMON_INSTALLER" ]] || die "daemon installer is missing or not executable: $DAEMON_INSTALLER"

# ------------------------------------------------------------------- 3) icons
step "3/5 icons (make-icons.py + sips/iconutil)"
python3 "$ICONS/make-icons.py"
[[ -f "$ICONS/icon.png" && -f "$ICONS/tray.png" ]] || die "icon generation produced nothing"

SET="$ICONS/icon.iconset"
rm -rf "$SET"
mkdir -p "$SET"
# The classic .icns ladder; iconutil rejects a set with any size missing.
for spec in 16:1 16:2 32:1 32:2 128:1 128:2 256:1 256:2 512:1 512:2; do
  base=${spec%%:*}; scale=${spec##*:}
  px=$((base * scale))
  if [[ $scale == 1 ]]; then name="icon_${base}x${base}.png"; else name="icon_${base}x${base}@2x.png"; fi
  sips -z "$px" "$px" "$ICONS/icon.png" --out "$SET/$name" >/dev/null
done
iconutil -c icns "$SET" -o "$ICONS/icon.icns"
rm -rf "$SET"
[[ -s "$ICONS/icon.icns" ]] || die "iconutil produced no icon.icns"
print -ru2 -- "[audiohub] icon.icns $(stat -f %z "$ICONS/icon.icns") bytes"

# --------------------------------------------------------------- 4) sidecars
# Tauri resolves each externalBin from a target-suffixed source and drops it
# beside audiohub-app. audiohubd is the actual background process; audiohub is
# retained as an internal diagnostics/shutdown CLI and is never exposed as a
# separate product to the user.
step "4/5 stage daemon and CLI sidecars"
mkdir -p "$TAURI_DIR/binaries"
cp -f "$CLI" "$TAURI_DIR/binaries/audiohub-$TRIPLE"
chmod +x "$TAURI_DIR/binaries/audiohub-$TRIPLE"
cp -f "$DAEMON" "$TAURI_DIR/binaries/audiohubd-$TRIPLE"
chmod +x "$TAURI_DIR/binaries/audiohubd-$TRIPLE"

# ------------------------------------------------------------- 4) tauri build
if ! cargo tauri --version >/dev/null 2>&1; then
  print -ru2 -- "[audiohub] cargo-tauri not installed; installing (pure Rust, takes a while)"
  cargo install tauri-cli --version "^2" --locked
fi
( cd "$TAURI_DIR" && cargo tauri build --bundles app )

BUNDLE="$TAURI_DIR/target/release/bundle/macos/AudioHub.app"
[[ -d "$BUNDLE" ]] || die "bundle not found at $BUNDLE"

# The generated component package is copied only after Tauri has assembled the
# .app. Keeping it out of tauri.macos.conf.json lets `cargo check` work in a
# fresh checkout where build/ does not exist. build.rs embedded this exact
# file's SHA-256 in audiohub-app, so replacement after compilation is rejected
# before the App asks macOS for administrator authorization.
mkdir -p "$BUNDLE/Contents/Resources/installer"
/usr/bin/ditto "$DRIVER_PKG" "$BUNDLE/Contents/Resources/installer/AudioHubDriver.pkg"
cmp -s "$DRIVER_PKG" "$BUNDLE/Contents/Resources/installer/AudioHubDriver.pkg" \
  || die "embedded driver package differs from the package compiled into AudioHub"
/usr/bin/install -m 0555 "$DAEMON_INSTALLER" \
  "$BUNDLE/Contents/Resources/installer/install-daemon.sh"
cmp -s "$DAEMON_INSTALLER" "$BUNDLE/Contents/Resources/installer/install-daemon.sh" \
  || die "embedded daemon installer differs from its reviewed source"

# ------------------------------------------------------------------ 5) verify
# Sign with the STABLE dev identity before verifying. An ad-hoc signature's
# identity is derived from the file's own bytes, so every build produced a new
# code identity — and macOS records Local Network consent against that identity,
# so each rebuild silently revoked a permission the user had already granted.
# Measured: after a rebuild the bundled daemon got `No route to host (os error
# 65)` on every LAN connect while `nc` from a shell reached the same host:port.
# Skipped with a warning when the identity does not exist, so a fresh clone
# still builds.
if [[ "${AUDIOHUB_RELEASE:-0}" == 1 ]]; then
  [[ "${AUDIOHUB_SIGN_IDENTITY:-}" == Developer\ ID\ Application:* ]] \
    || die "release build requires AUDIOHUB_SIGN_IDENTITY='Developer ID Application: …'"
  [[ "${AUDIOHUB_INSTALLER_IDENTITY:-}" == Developer\ ID\ Installer:* ]] \
    || die "release build requires AUDIOHUB_INSTALLER_IDENTITY='Developer ID Installer: …'"
  [[ -n "${AUDIOHUB_NOTARY_PROFILE:-}" ]] \
    || die "release build requires AUDIOHUB_NOTARY_PROFILE for xcrun notarytool"
elif [[ -z "${AUDIOHUB_SIGN_IDENTITY:-}" ]]; then
  if security find-identity -p codesigning 2>/dev/null | grep -q '"AudioHub Dev"' || \
     codesign --dryrun --force --sign 'AudioHub Dev' "$BUNDLE" >/dev/null 2>&1; then
    export AUDIOHUB_SIGN_IDENTITY="AudioHub Dev"
  else
    export AUDIOHUB_SIGN_IDENTITY="-"
    print -u2 -- "[audiohub] WARNING: no 'AudioHub Dev' identity — using an ad-hoc development signature."
  fi
fi
zsh "$APP_DIR/../scripts/sign-dev.sh" || die "signing failed"
codesign --verify --deep --strict --verbose=2 "$BUNDLE" \
  || die "final AudioHub.app signature verification failed"

step "5/5 verify bundle"
# The app executable is named after the crate's bin (audiohub-app), NOT after
# productName — and the volume is case-insensitive, so a naive
# Contents/MacOS/AudioHub check silently resolves to the daemon sidecar
# instead. Read the name out of Info.plist and prove the two are distinct files.
PLIST="$BUNDLE/Contents/Info.plist"
[[ -f "$PLIST" ]] || die "no Info.plist in $BUNDLE"
EN_INFO_STRINGS="$BUNDLE/Contents/Resources/en.lproj/InfoPlist.strings"
ZH_INFO_STRINGS="$BUNDLE/Contents/Resources/zh-Hans.lproj/InfoPlist.strings"
for LOCALISED_PLIST in "$PLIST" "$EN_INFO_STRINGS" "$ZH_INFO_STRINGS"; do
  [[ -f "$LOCALISED_PLIST" ]] || die "missing macOS localisation: $LOCALISED_PLIST"
  plutil -lint "$LOCALISED_PLIST" >/dev/null \
    || die "invalid macOS localisation plist: $LOCALISED_PLIST"
  for KEY in NSMicrophoneUsageDescription NSAudioCaptureUsageDescription NSLocalNetworkUsageDescription; do
    VALUE="$(plutil -extract "$KEY" raw -o - "$LOCALISED_PLIST" 2>/dev/null || true)"
    [[ -n "$VALUE" ]] || die "$LOCALISED_PLIST has no non-empty $KEY"
  done
done
cmp -s "$TAURI_DIR/locales/en.lproj/InfoPlist.strings" "$EN_INFO_STRINGS" \
  || die "English permission localisation is missing or changed in the bundle"
cmp -s "$TAURI_DIR/locales/zh-Hans.lproj/InfoPlist.strings" "$ZH_INFO_STRINGS" \
  || die "Simplified Chinese permission localisation is missing or changed in the bundle"
EXE_NAME="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleExecutable' "$PLIST")"
EXE="$BUNDLE/Contents/MacOS/$EXE_NAME"
SIDE="$BUNDLE/Contents/MacOS/audiohubd"
SIDE_CLI="$BUNDLE/Contents/MacOS/audiohub"
[[ -x "$EXE" ]] || die "missing app executable $EXE"
[[ -x "$SIDE" ]] || die "daemon not bundled next to the executable ($SIDE) — the .app cannot self-bootstrap"
[[ -x "$SIDE_CLI" ]] || die "internal CLI not bundled next to the executable ($SIDE_CLI)"
[[ ! "$EXE" -ef "$SIDE" ]] || die "app executable and daemon resolve to the SAME file — nothing was bundled"
[[ -s "$BUNDLE/Contents/Resources/installer/AudioHubDriver.pkg" ]] \
  || die "driver repair package is missing from the App bundle"
[[ -x "$BUNDLE/Contents/Resources/installer/install-daemon.sh" ]] \
  || die "daemon installer is missing from the App bundle"
[[ -f "$BUNDLE/Contents/Resources/icon.icns" ]] || die "icon.icns not in the bundle"
LICENSES="$BUNDLE/Contents/Resources/licenses"
cmp -s "$ROOT/LICENSE" "$LICENSES/AudioHub-LICENSE.txt" \
  || die "AudioHub license is missing or changed in the bundle"
cmp -s "$ROOT/NOTICE.md" "$LICENSES/AudioHub-NOTICE.md" \
  || die "AudioHub notices are missing or changed in the bundle"
cmp -s "$ROOT/THIRD-PARTY-LICENSES.html" "$LICENSES/THIRD-PARTY-LICENSES.html" \
  || die "third-party license report is missing or changed in the bundle"
# Throwaway config dir: `id` would otherwise materialise a key in the real one.
PROBE="$(mktemp -d)"
AUDIOHUB_CONFIG_DIR="$PROBE" "$SIDE_CLI" id --json 2>/dev/null | python3 -c \
  'import json,sys; sys.exit(0 if json.load(sys.stdin).get("fingerprint") else 1)' \
  || { rm -rf "$PROBE"; die "bundled CLI did not answer 'id --json'"; }
rm -rf "$PROBE"

# The package installs only the App. On first launch, the App validates its
# bundled daemon and asks for one explicit authorization before installing and
# machine-locally signing the independent runtime service. The driver remains
# a separate Settings action. The outer DMG adds an authenticated uninstaller.
PKG="$(zsh "$ROOT/scripts/build-macos-pkg.sh" "$BUNDLE" | tail -n 1)"
[[ -s "$PKG" ]] || die "installer package was not produced"
DMG="$(zsh "$ROOT/scripts/build-macos-dmg.sh" "$PKG" | tail -n 1)"
[[ -s "$DMG" ]] || die "final disk image was not produced"

print -ru2 -- "[audiohub] bundle:  $BUNDLE"
print -ru2 -- "[audiohub] app exe: $EXE"
print -ru2 -- "[audiohub] daemon:  $SIDE ($(stat -f %z "$SIDE") bytes)"
print -ru2 -- "[audiohub] pkg:     $PKG"
print -ru2 -- "[audiohub] dmg:     $DMG"
print -ru2 -- "[audiohub] open it:  open '$BUNDLE'"
print -ru2 -- "[audiohub] NOTE: without Developer ID identities and a notary profile this is a development image, not notarised distribution."
