#!/bin/zsh
# Build the distributable macOS AudioHub.app (spec-app.md §2).
#   1) cargo build --release -p audiohub-cli      (the daemon that ships inside)
#   2) icons: make-icons.py -> sips/iconutil -> icon.icns   (system tools only)
#   3) stage the daemon as a Tauri externalBin sidecar
#   4) cargo tauri build --bundles app
#   5) verify the bundle self-bootstraps (daemon present next to the executable)
#
# This is a DEVELOPMENT build: unsigned, ad-hoc only, not notarised. Real
# signing + notarisation is M7. Gatekeeper will quarantine a downloaded copy;
# locally built bundles run fine.
set -euo pipefail

APP_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$APP_DIR/.." && pwd)"
TAURI_DIR="$APP_DIR/src-tauri"
ICONS="$TAURI_DIR/icons"

export PATH="$HOME/.cargo/bin:$PATH"

step() { print -ru2 -- "[audiohub] ==== $* ===="; }
die()  { print -ru2 -- "[audiohub] ERROR: $*"; exit 1; }

[[ "$(uname -s)" == "Darwin" ]] || die "build-app.sh builds the macOS .app; run it on macOS"

TRIPLE="$(rustc -vV | awk '/^host: /{print $2}')"
[[ -n "$TRIPLE" ]] || die "could not determine host triple from rustc -vV"

# ------------------------------------------------------------------ 1) daemon
step "1/5 build daemon (cargo build --release -p audiohub-cli)"
cargo build --release --manifest-path "$ROOT/Cargo.toml" -p audiohub-cli
DAEMON="$ROOT/target/release/audiohub"
[[ -x "$DAEMON" ]] || die "daemon binary missing after build: $DAEMON"

# ------------------------------------------------------------------- 2) icons
step "2/5 icons (make-icons.py + sips/iconutil)"
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

# --------------------------------------------------------------- 3) sidecar
# Tauri's externalBin resolves "binaries/audiohub" to binaries/audiohub-<triple>
# and drops it in AudioHub.app/Contents/MacOS/audiohub — exactly where
# daemon_binary() looks first, so a copied .app self-bootstraps.
step "3/5 stage daemon as sidecar (binaries/audiohub-$TRIPLE)"
mkdir -p "$TAURI_DIR/binaries"
cp -f "$DAEMON" "$TAURI_DIR/binaries/audiohub-$TRIPLE"
chmod +x "$TAURI_DIR/binaries/audiohub-$TRIPLE"

# ------------------------------------------------------------- 4) tauri build
step "4/5 cargo tauri build --bundles app"
if ! cargo tauri --version >/dev/null 2>&1; then
  print -ru2 -- "[audiohub] cargo-tauri not installed; installing (pure Rust, takes a while)"
  cargo install tauri-cli --version "^2" --locked
fi
( cd "$TAURI_DIR" && cargo tauri build --bundles app )

BUNDLE="$TAURI_DIR/target/release/bundle/macos/AudioHub.app"
[[ -d "$BUNDLE" ]] || die "bundle not found at $BUNDLE"

# ------------------------------------------------------------------ 5) verify
# Sign with the STABLE dev identity before verifying. An ad-hoc signature's
# identity is derived from the file's own bytes, so every build produced a new
# code identity — and macOS records Local Network consent against that identity,
# so each rebuild silently revoked a permission the user had already granted.
# Measured: after a rebuild the bundled daemon got `No route to host (os error
# 65)` on every LAN connect while `nc` from a shell reached the same host:port.
# Skipped with a warning when the identity does not exist, so a fresh clone
# still builds.
if security find-identity -p codesigning 2>/dev/null | grep -q '"AudioHub Dev"'; then
  zsh "$APP_DIR/../scripts/sign-dev.sh" || die "signing failed"
else
  print -u2 -- "[audiohub] WARNING: no 'AudioHub Dev' identity — bundle stays ad-hoc,"
  print -u2 -- "[audiohub]          and macOS will drop its Local Network consent on"
  print -u2 -- "[audiohub]          every rebuild. See scripts/sign-dev.sh."
fi

step "5/5 verify bundle"
# The app executable is named after the crate's bin (audiohub-app), NOT after
# productName — and the volume is case-insensitive, so a naive
# Contents/MacOS/AudioHub check silently resolves to the daemon sidecar
# instead. Read the name out of Info.plist and prove the two are distinct files.
PLIST="$BUNDLE/Contents/Info.plist"
[[ -f "$PLIST" ]] || die "no Info.plist in $BUNDLE"
EXE_NAME="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleExecutable' "$PLIST")"
EXE="$BUNDLE/Contents/MacOS/$EXE_NAME"
SIDE="$BUNDLE/Contents/MacOS/audiohub"
[[ -x "$EXE" ]] || die "missing app executable $EXE"
[[ -x "$SIDE" ]] || die "daemon not bundled next to the executable ($SIDE) — the .app cannot self-bootstrap"
[[ ! "$EXE" -ef "$SIDE" ]] || die "app executable and daemon resolve to the SAME file — nothing was bundled"
[[ -f "$BUNDLE/Contents/Resources/icon.icns" ]] || die "icon.icns not in the bundle"
# Throwaway config dir: `id` would otherwise materialise a key in the real one.
PROBE="$(mktemp -d)"
AUDIOHUB_CONFIG_DIR="$PROBE" "$SIDE" id --json 2>/dev/null | python3 -c \
  'import json,sys; sys.exit(0 if json.load(sys.stdin).get("fingerprint") else 1)' \
  || { rm -rf "$PROBE"; die "bundled daemon did not answer 'id --json' — it is not the audiohub CLI"; }
rm -rf "$PROBE"

print -ru2 -- "[audiohub] bundle:  $BUNDLE"
print -ru2 -- "[audiohub] app exe: $EXE"
print -ru2 -- "[audiohub] daemon:  $SIDE ($(stat -f %z "$SIDE") bytes)"
print -ru2 -- "[audiohub] open it:  open '$BUNDLE'"
print -ru2 -- "[audiohub] NOTE: development build — unsigned, not notarised (M7)."
