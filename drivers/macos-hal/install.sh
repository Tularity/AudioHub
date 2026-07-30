#!/bin/zsh
# AudioHub macOS HAL driver — INSTALL (spec-round2 §B3).
#
#   1. copy build/AudioHubDriver.driver -> /Library/Audio/Plug-Ins/HAL (SUDO)
#   2. restart coreaudiod so the plug-in is loaded                     (SUDO)
#
# Step 2 INTERRUPTS ALL SYSTEM AUDIO for a second or two.
#
# NO LAUNCHD SERVICE IS INSTALLED, on purpose. The plug-in now registers the
# mach name com.audiohub.driver itself (bootstrap_check_in from inside
# coreaudiod, declared under AudioServerPlugIn_MachServices in Info.plist) and
# audiohubd connects TO it. Earlier revisions installed audiohubd as a launchd
# job owning the name instead; both variants were measured on real hardware and
# both fail:
#   - user LaunchAgent (gui/<uid>): coreaudiod runs as _coreaudiod in the system
#     domain and cannot resolve a per-user name, so the bridge never connects.
#   - system LaunchDaemon: the bridge connects, but the daemon has no user
#     session and therefore no local-network consent — every outbound LAN
#     connect returns EHOSTUNREACH, while the same signed binary started from
#     the user's shell works.
# With the direction inverted, audiohubd is an ordinary user-session process:
# no launchd job required, no sudo, full network rights. Autostart for it (if
# wanted at all) belongs to the daemon/app side, not to this script.
#
# uninstall.sh reverses this AND still removes both retired launchd layouts, so
# a machine carrying either can be cleaned. Run it first if you want to be sure
# the exit exists before you take the entrance. Nothing happens without --yes:
# with no flag the plan is printed and the script exits 2.

set -uo pipefail
cd "${0:a:h}"

BUILT_BUNDLE="build/AudioHubDriver.driver"
PLUGIN_DIR=/Library/Audio/Plug-Ins/HAL
BUNDLE_NAME=AudioHubDriver.driver
INSTALLED_BUNDLE="$PLUGIN_DIR/$BUNDLE_NAME"
# Must match kAudioHubDriverMachServiceName in src/AudioHubBridge.h.
DRIVER_SERVICE=com.audiohub.driver

CONFIRMED=0
DRY_RUN=0

usage()
{
    print -- "usage: install.sh [--yes | --dry-run]"
    print -- ""
    print -- "  --yes       actually perform the install (required)"
    print -- "  --dry-run   print the plan and exit 0, change nothing"
    print -- ""
    print -- "Installs the HAL plug-in only. audiohubd needs no launchd service"
    print -- "and no sudo — it connects to the plug-in as a normal user process."
    exit 2
}

while (( $# > 0 )); do
    case "$1" in
        --yes)     CONFIRMED=1 ;;
        --dry-run) DRY_RUN=1 ;;
        -h|--help) usage ;;
        *) print -u2 -- "[audiohub] unknown argument: $1"; usage ;;
    esac
    shift
done

# Run as the USER, not under sudo: the copy and the coreaudiod restart sudo
# themselves. Under `sudo zsh install.sh` the reported "target user" is root,
# which is misleading here and actively wrong in uninstall.sh — keep both
# scripts refusing the same way so the pair cannot be used inconsistently.
if (( EUID == 0 )); then
    print -u2 -- "[audiohub] do not run this under sudo — run it as your own user"
    print -u2 -- "[audiohub] (the steps that need root call sudo themselves)"
    exit 2
fi

# --- preflight: everything that can be checked before touching the system -----
if [[ ! -d "$BUILT_BUNDLE" ]]; then
    print -u2 -- "[audiohub] $BUILT_BUNDLE does not exist — run 'zsh build.sh' first"
    exit 1
fi
archs="$(lipo -archs "$BUILT_BUNDLE/Contents/MacOS/AudioHubDriver" 2>/dev/null)"
for want in arm64 x86_64; do
    if [[ " $archs " != *" $want "* ]]; then
        print -u2 -- "[audiohub] built binary is missing the $want slice (has: ${archs:-none})"
        exit 1
    fi
done
if ! codesign --verify --deep --strict "$BUILT_BUNDLE" 2>/dev/null; then
    print -u2 -- "[audiohub] $BUILT_BUNDLE is not validly signed — run 'zsh build.sh'"
    exit 1
fi
if ! plutil -lint "$BUILT_BUNDLE/Contents/Info.plist" >/dev/null 2>&1; then
    print -u2 -- "[audiohub] $BUILT_BUNDLE/Contents/Info.plist is malformed"
    exit 1
fi
# A plug-in whose Info.plist does not declare its own mach service loads fine
# and then can never be reached by the daemon — catch that here, not at 2am.
# The value must be the DRIVER's name; the daemon's name being there is the
# retired layout and would silently reproduce the old failure.
declared="$(plutil -extract AudioServerPlugIn_MachServices xml1 -o - "$BUILT_BUNDLE/Contents/Info.plist" 2>/dev/null)"
if [[ "$declared" != *"<string>$DRIVER_SERVICE</string>"* ]]; then
    print -u2 -- "[audiohub] Info.plist does not declare AudioServerPlugIn_MachServices=$DRIVER_SERVICE"
    print -u2 -- "[audiohub] (found: ${declared:-nothing})"
    exit 1
fi

# --- the plan -----------------------------------------------------------------
print -- "[audiohub] install plan"
print -- "  target user     : $USER (uid $UID)"
print -- "  mach service    : $DRIVER_SERVICE   (registered BY THE PLUG-IN)"
print -- "  launchd service : none — audiohubd runs in the user session"
print -- "  1. sudo rm -rf $INSTALLED_BUNDLE"
print -- "     sudo cp -R $BUILT_BUNDLE $PLUGIN_DIR/"
print -- "     sudo chown -R root:wheel $INSTALLED_BUNDLE"
print -- "  2. sudo launchctl kickstart -kp system/com.apple.audio.coreaudiod"
print -- "     ^^ THIS INTERRUPTS ALL SYSTEM AUDIO BRIEFLY"
print -- ""
print -- "  undo with: zsh uninstall.sh --yes"
print -- ""

if (( DRY_RUN )); then
    print -- "[audiohub] --dry-run: nothing was changed"
    exit 0
fi
if (( CONFIRMED == 0 )); then
    print -u2 -- "[audiohub] refusing to run without --yes (see the plan above)"
    exit 2
fi

step() { print -- "[audiohub] $*" }

# --- 1/2: the HAL plug-in -----------------------------------------------------
# cp -R over an existing .driver MERGES directories and can leave a stale
# executable behind, so the old bundle must go first.
step "installing $BUNDLE_NAME into $PLUGIN_DIR (sudo)"
sudo rm -rf "$INSTALLED_BUNDLE" || exit 1
sudo mkdir -p "$PLUGIN_DIR" || exit 1
sudo cp -R "$BUILT_BUNDLE" "$PLUGIN_DIR/" || exit 1
sudo chown -R root:wheel "$INSTALLED_BUNDLE" || exit 1

step "restarting coreaudiod (sudo) — system audio drops for a moment"
if ! sudo launchctl kickstart -kp system/com.apple.audio.coreaudiod 2>/dev/null; then
    if ! sudo killall coreaudiod 2>/dev/null; then
        print -u2 -- "[audiohub] WARNING: could not restart coreaudiod; the devices will"
        print -u2 -- "[audiohub]          not appear until it restarts on its own"
    fi
fi

# --- verification -------------------------------------------------------------
print -- ""
rc=0
if [[ -x "$INSTALLED_BUNDLE/Contents/MacOS/AudioHubDriver" ]]; then
    print -- "[audiohub] verified: $INSTALLED_BUNDLE ($(lipo -archs "$INSTALLED_BUNDLE/Contents/MacOS/AudioHubDriver"))"
else
    print -u2 -- "[audiohub] FAIL: $INSTALLED_BUNDLE did not install"
    rc=1
fi
print -- "[audiohub] coreaudiod needs a moment; then 'AudioHub Speaker' and"
print -- "[audiohub] 'AudioHub Microphone' should appear in Audio MIDI Setup."
print -- "[audiohub] the plug-in registers $DRIVER_SERVICE once coreaudiod loads it;"
print -- "[audiohub] start audiohubd normally (user session, no sudo) and it will attach."
print -- "[audiohub] undo everything with: zsh uninstall.sh --yes"
exit $rc
