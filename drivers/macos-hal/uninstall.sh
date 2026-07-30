#!/bin/zsh
# AudioHub macOS HAL driver — UNINSTALL (spec-round2 §B3).
#
# Removes everything install.sh can put on the system, plus the two RETIRED
# daemon launchd layouts that earlier revisions installed:
#   1. unload + delete ~/Library/LaunchAgents/com.audiohub.daemon.plist  (no sudo)
#   2. unload + delete /Library/LaunchDaemons/com.audiohub.daemon.plist  (SUDO)
#   3. delete  /Library/Audio/Plug-Ins/HAL/AudioHubDriver.driver         (SUDO)
#   4. restart coreaudiod so the virtual devices disappear               (SUDO)
#
# Steps 1 and 2 exist only for cleanup: the current install.sh registers NO
# launchd service at all (the plug-in owns the mach name and audiohubd is a
# plain user-session process). A machine may still be carrying either retired
# layout, and the system-domain one in particular must go — it is what left the
# daemon without local-network access.
#
# Step 4 INTERRUPTS ALL SYSTEM AUDIO for a second or two — every app playing or
# recording is dropped and has to be restarted by the user in the worst case.
#
# Nothing happens without --yes: with no flag the plan is printed and the script
# exits 2. Every step is idempotent, so running it twice, or on a machine where
# only half of the install landed, is safe and still exits 0.

set -uo pipefail   # deliberately NOT -e: one failed step must not skip cleanup
cd "${0:a:h}"

PLUGIN_DIR=/Library/Audio/Plug-Ins/HAL
BUNDLE_NAME=AudioHubDriver.driver
INSTALLED_BUNDLE="$PLUGIN_DIR/$BUNDLE_NAME"
AGENT_LABEL=com.audiohub.daemon
# Both retired layouts are removed unconditionally: the user-domain LaunchAgent
# shipped first (coreaudiod cannot see a user-domain mach name), the
# system-domain LaunchDaemon replaced it (the daemon then lost local-network
# access), and a machine may carry either or both.
AGENT_PLIST="$HOME/Library/LaunchAgents/$AGENT_LABEL.plist"
SYSTEM_PLIST="/Library/LaunchDaemons/$AGENT_LABEL.plist"

DO_DRIVER=1
DO_AGENT=1
CONFIRMED=0
DRY_RUN=0

usage()
{
    print -- "usage: uninstall.sh [--yes | --dry-run] [--driver-only | --agent-only]"
    print -- ""
    print -- "  --yes          actually perform the removal (required)"
    print -- "  --dry-run      print the plan and exit 0, change nothing"
    print -- "  --driver-only  only the HAL plug-in (leaves retired launchd jobs alone)"
    print -- "  --agent-only   only the retired launchd jobs (leaves the plug-in alone)"
    exit 2
}

while (( $# > 0 )); do
    case "$1" in
        --yes)         CONFIRMED=1 ;;
        --dry-run)     DRY_RUN=1 ;;
        --driver-only) DO_AGENT=0 ;;
        --agent-only)  DO_DRIVER=0 ;;
        -h|--help)     usage ;;
        *) print -u2 -- "[audiohub] unknown argument: $1"; usage ;;
    esac
    shift
done

if (( DO_DRIVER == 0 && DO_AGENT == 0 )); then
    print -u2 -- "[audiohub] --driver-only and --agent-only are mutually exclusive"
    exit 2
fi

# Run as the USER, not under sudo: the script sudo's the two steps that need it.
# Under `sudo zsh uninstall.sh` the shell has UID=0 and HOME=/var/root, so every
# user-domain step below targets root instead of the person who installed —
# gui/0 is booted out, /var/root/Library/LaunchAgents is inspected, nothing is
# found, and the script prints "verified: no LaunchAgent plist" while the real
# user's retired agent is still loaded. A false clean bill of health on exactly
# the cleanup path the retired system-domain layout makes important.
if (( EUID == 0 )); then
    print -u2 -- "[audiohub] do not run this under sudo — run it as your own user"
    print -u2 -- "[audiohub] (the steps that need root call sudo themselves)"
    if [[ -n "${SUDO_USER-}" ]]; then
        print -u2 -- "[audiohub] try: sudo -u $SUDO_USER zsh ${0:a}"
    fi
    exit 2
fi

# --- what is actually present -------------------------------------------------
have_bundle=0
[[ -d "$INSTALLED_BUNDLE" ]] && have_bundle=1
have_plist=0
[[ -f "$AGENT_PLIST" ]] && have_plist=1
have_agent=0
if launchctl print "gui/$UID/$AGENT_LABEL" >/dev/null 2>&1; then
    have_agent=1
fi
have_sys_plist=0
[[ -f "$SYSTEM_PLIST" ]] && have_sys_plist=1
have_sys_agent=0
# `launchctl print system/...` needs root, so the plist's presence is the signal
# a normal user can rely on. `sudo -n` is a free second chance that catches a
# job still loaded after its plist was deleted by hand; it never prompts, so it
# costs nothing when there is no cached sudo timestamp. bootout is idempotent
# either way.
if (( have_sys_plist )); then
    have_sys_agent=1
elif sudo -n launchctl print "system/$AGENT_LABEL" >/dev/null 2>&1; then
    have_sys_agent=1
fi

# --- the plan, printed verbatim before anything is touched --------------------
print -- "[audiohub] uninstall plan"
print -- "  target user     : $USER (uid $UID)"
if (( DO_AGENT )); then
    if (( have_agent )); then
        print -- "  1. launchctl bootout gui/$UID/$AGENT_LABEL      (no sudo)  [retired agent is LOADED]"
    else
        print -- "  1. launchctl bootout gui/$UID/$AGENT_LABEL      (no sudo)  [not loaded, skip]"
    fi
    if (( have_plist )); then
        print -- "  2. rm -f $AGENT_PLIST      (no sudo)  [present]"
    else
        print -- "  2. rm -f $AGENT_PLIST      (no sudo)  [absent, skip]"
    fi
    if (( have_sys_agent )); then
        print -- "  2b. sudo launchctl bootout system/$AGENT_LABEL   [retired system daemon PRESENT]"
        print -- "      sudo rm -f $SYSTEM_PLIST"
    else
        print -- "  2b. retired system-domain LaunchDaemon      [absent, skip]"
    fi
else
    print -- "  1-2. retired launchd steps skipped (--driver-only)"
fi
if (( DO_DRIVER )); then
    if (( have_bundle )); then
        print -- "  3. sudo rm -rf $INSTALLED_BUNDLE   [present]"
        print -- "  4. sudo launchctl kickstart -kp system/com.apple.audio.coreaudiod"
        print -- "     ^^ THIS INTERRUPTS ALL SYSTEM AUDIO BRIEFLY"
    else
        print -- "  3. sudo rm -rf $INSTALLED_BUNDLE   [absent, skip]"
        print -- "  4. coreaudiod restart skipped (nothing was removed)"
    fi
else
    print -- "  3-4. driver steps skipped (--agent-only)"
fi
print -- ""

if (( DRY_RUN )); then
    print -- "[audiohub] --dry-run: nothing was changed"
    exit 0
fi
if (( CONFIRMED == 0 )); then
    print -u2 -- "[audiohub] refusing to run without --yes (see the plan above)"
    exit 2
fi

failures=0
step()
{
    print -- "[audiohub] $*"
}

# --- 1/2: retired launchd layouts ---------------------------------------------
if (( DO_AGENT )); then
    if (( have_agent )); then
        step "unloading the retired user-domain $AGENT_LABEL"
        if ! launchctl bootout "gui/$UID/$AGENT_LABEL" 2>/dev/null; then
            # bootout is the modern spelling; older systems only know unload
            if ! launchctl unload -w "$AGENT_PLIST" 2>/dev/null; then
                print -u2 -- "[audiohub] WARNING: could not unload $AGENT_LABEL"
                (( failures++ ))
            fi
        fi
    fi
    if (( have_plist )); then
        step "removing $AGENT_PLIST"
        if ! rm -f "$AGENT_PLIST"; then
            print -u2 -- "[audiohub] WARNING: could not remove $AGENT_PLIST"
            (( failures++ ))
        fi
    fi
    if (( have_sys_agent )); then
        step "unloading the retired system-domain $AGENT_LABEL (sudo)"
        sudo launchctl bootout "system/$AGENT_LABEL" 2>/dev/null \
            || sudo launchctl unload -w "$SYSTEM_PLIST" 2>/dev/null || true
        if (( have_sys_plist )); then
            step "removing $SYSTEM_PLIST (sudo)"
            if ! sudo rm -f "$SYSTEM_PLIST"; then
                print -u2 -- "[audiohub] WARNING: could not remove $SYSTEM_PLIST"
                (( failures++ ))
            fi
        fi
    fi
fi

# --- 3/4: the HAL plug-in (needs sudo, restarts coreaudiod) -------------------
if (( DO_DRIVER && have_bundle )); then
    # Refuse to rm -rf anything that is not exactly the bundle we install.
    if [[ "$INSTALLED_BUNDLE" != "/Library/Audio/Plug-Ins/HAL/AudioHubDriver.driver" ]]; then
        print -u2 -- "[audiohub] refusing to delete unexpected path: $INSTALLED_BUNDLE"
        exit 1
    fi
    step "removing $INSTALLED_BUNDLE (sudo)"
    if sudo rm -rf "$INSTALLED_BUNDLE"; then
        step "restarting coreaudiod (sudo) — system audio drops for a moment"
        if ! sudo launchctl kickstart -kp system/com.apple.audio.coreaudiod 2>/dev/null; then
            if ! sudo killall coreaudiod 2>/dev/null; then
                print -u2 -- "[audiohub] WARNING: could not restart coreaudiod; the virtual"
                print -u2 -- "[audiohub]          devices stay listed until the next reboot"
                (( failures++ ))
            fi
        fi
    else
        print -u2 -- "[audiohub] WARNING: could not remove $INSTALLED_BUNDLE"
        (( failures++ ))
    fi
fi

# --- verification -------------------------------------------------------------
print -- ""
if (( DO_DRIVER )); then
    if [[ -d "$INSTALLED_BUNDLE" ]]; then
        print -u2 -- "[audiohub] FAIL: $INSTALLED_BUNDLE is still present"
        (( failures++ ))
    else
        print -- "[audiohub] verified: no HAL plug-in at $INSTALLED_BUNDLE"
    fi
fi
if (( DO_AGENT )); then
    if [[ -f "$AGENT_PLIST" ]]; then
        print -u2 -- "[audiohub] FAIL: $AGENT_PLIST is still present"
        (( failures++ ))
    else
        print -- "[audiohub] verified: no LaunchAgent plist at $AGENT_PLIST"
    fi
    if [[ -f "$SYSTEM_PLIST" ]]; then
        print -u2 -- "[audiohub] FAIL: $SYSTEM_PLIST is still present"
        (( failures++ ))
    else
        print -- "[audiohub] verified: no LaunchDaemon plist at $SYSTEM_PLIST"
    fi
    if launchctl print "gui/$UID/$AGENT_LABEL" >/dev/null 2>&1; then
        print -u2 -- "[audiohub] FAIL: $AGENT_LABEL is still loaded in launchd"
        (( failures++ ))
    else
        print -- "[audiohub] verified: $AGENT_LABEL is not loaded"
    fi
fi

if (( failures > 0 )); then
    print -u2 -- "[audiohub] uninstall finished with $failures problem(s)"
    exit 1
fi
print -- "[audiohub] uninstall complete"
