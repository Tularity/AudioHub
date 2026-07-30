#!/bin/zsh
# AudioHub macOS HAL driver — build script (M5 scaffold).
#
# Builds build/AudioHubDriver.driver (bundle: Contents/MacOS/AudioHubDriver +
# Contents/Info.plist) as a UNIVERSAL binary (arm64 + x86_64), ad-hoc codesigns
# it, then verifies the signature and the plist. This script does NOT install.
#
# ---- round-2 MANUAL install steps (never run automatically) ----
# coreaudiod loads the bundle it finds; a plain `cp -R` over an existing
# .driver MERGES directories and can leave a stale (e.g. single-arch)
# executable behind, so always remove the old bundle first:
#   sudo rm -rf /Library/Audio/Plug-Ins/HAL/AudioHubDriver.driver
#   sudo cp -R build/AudioHubDriver.driver /Library/Audio/Plug-Ins/HAL/
#   sudo chown -R root:wheel /Library/Audio/Plug-Ins/HAL/AudioHubDriver.driver
#   sudo launchctl kickstart -kp system/com.apple.audio.coreaudiod
#     (older macOS: sudo killall coreaudiod)
# confirm what actually landed (must list both arm64 and x86_64):
#   lipo -archs /Library/Audio/Plug-Ins/HAL/AudioHubDriver.driver/Contents/MacOS/AudioHubDriver
# uninstall:
#   sudo rm -rf /Library/Audio/Plug-Ins/HAL/AudioHubDriver.driver
#   sudo launchctl kickstart -kp system/com.apple.audio.coreaudiod
#
# ---- daemon bridge notes ----
# The plug-in REGISTERS the mach service com.audiohub.driver (its own name,
# declared under AudioServerPlugIn_MachServices in Info.plist) and audiohubd
# looks it up; see src/AudioHubBridge.h for why that direction and not the
# reverse. The plug-in also owns both shared rings and hands out mach memory
# entries for them at handshake time. audiohubd needs NO launchd service and NO
# sudo — it is an ordinary user-session process. install.sh therefore installs
# the driver only. With no daemon attached the devices stay listed, discard
# output and render input as silence.

set -euo pipefail
cd "${0:a:h}"

BUILD_DIR=build
BUNDLE="$BUILD_DIR/AudioHubDriver.driver"
BIN="$BUNDLE/Contents/MacOS/AudioHubDriver"
PLIST="$BUNDLE/Contents/Info.plist"
# 11.0 is the oldest macOS with an arm64 slice; both slices target the same floor.
MACOS_MIN=11.0

rm -rf "$BUNDLE"
mkdir -p "$BUNDLE/Contents/MacOS"

clang -bundle \
    -Wall -Wextra -O2 \
    -arch arm64 -arch x86_64 \
    -mmacosx-version-min="$MACOS_MIN" \
    -framework CoreAudio -framework CoreFoundation \
    -lbsm \
    -o "$BIN" \
    src/AudioHubDriver.c src/AudioHubBridge.c

cp Info.plist "$PLIST"

codesign --force --sign - "$BUNDLE"

# --- verification: any failure here must fail the build -----------------------
archs="$(lipo -archs "$BIN")"
for want in arm64 x86_64; do
    if [[ " $archs " != *" $want "* ]]; then
        print -u2 -- "FAIL: $BIN is missing the $want slice (has: $archs)"
        exit 1
    fi
done

codesign --verify --deep --strict --verbose=2 "$BUNDLE" || {
    print -u2 -- "FAIL: codesign --verify --deep --strict rejected $BUNDLE"
    exit 1
}

plutil -lint "$PLIST" || {
    print -u2 -- "FAIL: plutil -lint rejected $PLIST"
    exit 1
}

echo "built: $BUNDLE ($archs, macOS $MACOS_MIN+), signature and plist verified"
echo "install is manual (round 2), see comments at the top of this script"
