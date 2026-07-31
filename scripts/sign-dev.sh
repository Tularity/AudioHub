#!/bin/zsh
# Sign the dev binaries with a STABLE local identity.
#
# WHY THIS EXISTS (measured on the user's machine, 2026-07-30):
# an ad-hoc / linker-signed binary gets an identifier derived from its content,
# so EVERY `cargo build` produces a new code identity. macOS records the Local
# Network grant against that identity, so after each rebuild the daemon was
# silently denied and every outbound LAN connection failed with EHOSTUNREACH
# ("No route to host") — and a system-domain LaunchDaemon has no GUI session in
# which to re-prompt, so it could never recover on its own.
#
# Signing with a real certificate pins the designated requirement to the CERT,
# not the content hash:
#     designated => identifier "com.audiohub.daemon" and certificate leaf = H"..."
# The grant then survives every rebuild. The certificate is a locally created,
# self-signed one — no Apple account of any kind is involved. Release builds
# switch to a Developer ID + notarisation (M7); this is a development aid.
#
# The identifier is pinned explicitly because codesign otherwise derives it from
# the FILE NAME, which would silently change identity if a binary were renamed.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

IDENTITY="${AUDIOHUB_SIGN_IDENTITY:-AudioHub Dev}"

# Two explicit calls, no data structure: an associative array silently iterated
# one pair, and a "path|id" split then hit zsh's `|` pattern alternation. Both
# failures were quiet — the CLI just stayed ad-hoc signed while the script
# reported success. Two targets do not need a loop.
sign_one() {  # $1 binary  $2 pinned identifier
  [[ -f "$1" ]] || return 1
  # --force replaces the linker-signed ad-hoc signature cargo leaves behind.
  # --identifier is pinned because codesign otherwise derives it from the FILE
  # NAME, so a rename would silently change the code identity.
  codesign --force --sign "$IDENTITY" --identifier "$2" "$1"
  print -- "[audiohub] signed $(basename "$1") as $2"
  print -- "[audiohub]   $(codesign -d -r- "$1" 2>&1 | grep -o 'designated =>.*')"
}

if ! security find-identity -p codesigning 2>/dev/null | grep -q "\"$IDENTITY\""; then
  print -u2 -- "[audiohub] no code-signing identity named '$IDENTITY'."
  print -u2 -- "[audiohub] create one: Keychain Access > Certificate Assistant >"
  print -u2 -- "[audiohub]   Create a Certificate…  name '$IDENTITY',"
  print -u2 -- "[audiohub]   Identity Type 'Self Signed Root', Type 'Code Signing'."
  exit 1
fi

signed=0
sign_one "$ROOT/target/release/audiohubd" com.audiohub.daemon && (( signed++ )) || true
sign_one "$ROOT/target/release/audiohub"  com.audiohub.cli    && (( signed++ )) || true

# The BUNDLE too, and its embedded daemon in particular. macOS records Local
# Network consent against a code identity, and an ad-hoc signature's identity is
# derived from the file's own contents — so every `build-app.sh` produced a
# brand-new identity and silently dropped a consent the user had already given.
# Measured: after a rebuild the bundled daemon got `No route to host (os error
# 65)` on every LAN connect while `nc` from a shell reached the same host and
# port fine. Inner binaries first, then the bundle: codesign seals what it
# contains, so signing the wrapper before its contents invalidates it.
BUNDLE="$ROOT/app/src-tauri/target/release/bundle/macos/AudioHub.app"
if [[ -d "$BUNDLE" ]]; then
  sign_one "$BUNDLE/Contents/MacOS/audiohub"     com.audiohub.daemon && (( signed++ )) || true
  sign_one "$BUNDLE/Contents/MacOS/audiohub-app" com.audiohub.app    && (( signed++ )) || true
  if codesign --force --sign "$IDENTITY" --identifier com.audiohub.app "$BUNDLE" 2>/dev/null; then
    print -- "[audiohub] signed $BUNDLE"
    print -- "[audiohub]   $(codesign -d -r- "$BUNDLE" 2>&1 | grep -o 'designated =>.*')"
    (( signed++ ))
  else
    print -u2 -- "[audiohub] WARNING: could not sign $BUNDLE"
  fi
fi

if (( signed == 0 )); then
  print -u2 -- "[audiohub] nothing to sign; run cargo build --release first"
  exit 1
fi

