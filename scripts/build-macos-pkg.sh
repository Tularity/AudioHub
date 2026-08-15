#!/bin/zsh
# Assemble the one user-facing macOS installer. Its only installed payload is
# /Applications/AudioHub.app. The App owns the later, explicit authorization
# transaction that installs and machine-locally signs its daemon; the driver is
# a separate opt-in Settings action too.
set -euo pipefail

ROOT="$(cd "${0:a:h}/.." && pwd)"
TAURI="$ROOT/app/src-tauri"
APP="${1:-$TAURI/target/release/bundle/macos/AudioHub.app}"
OUT_DIR="$TAURI/target/release/bundle/pkg"
VERSION="${AUDIOHUB_VERSION:-0.1.0}"

die() { print -u2 -- "[audiohub] ERROR: $*"; exit 1; }

[[ "$(uname -s)" == Darwin ]] || die "macOS is required"

# Standalone package builds participate in build-app.sh's repository-wide
# lock. When called by build-app.sh, fd 8 is inherited and points at the same
# already-locked inode; otherwise acquire it here before reading AudioHub.app.
GIT_COMMON_DIR="$(git -C "$ROOT" rev-parse --path-format=absolute --git-common-dir 2>/dev/null || true)"
[[ -n "$GIT_COMMON_DIR" && -d "$GIT_COMMON_DIR" ]] \
  || die "could not locate Git metadata for the macOS build lock"
BUILD_LOCK="$GIT_COMMON_DIR/audiohub-macos-build.lock"
if [[ "${AUDIOHUB_MACOS_BUILD_LOCK_PATH:-}" == "$BUILD_LOCK" && -e /dev/fd/8 ]] && \
   [[ "$(/usr/bin/stat -f %i /dev/fd/8 2>/dev/null || true)" == \
      "$(/usr/bin/stat -f %i "$BUILD_LOCK" 2>/dev/null || true)" ]]; then
  :
else
  [[ ! -L "$BUILD_LOCK" ]] || die "refusing a symlinked macOS build lock: $BUILD_LOCK"
  exec 8>>"$BUILD_LOCK"
  /bin/chmod 0600 "$BUILD_LOCK"
  # See app/build-app.sh for why this is conditional: /usr/bin/lockf is
  # absent on macOS 14 (and so on the macos-14 runner) and present on 26.
  # Unconditional, its absence surfaced as "another build is already
  # running" on a fresh VM where nothing else was running at all.
  if [[ -x /usr/bin/lockf ]]; then
    /usr/bin/lockf -s -t 0 8 \
      || die "another macOS AudioHub build is already running"
    export AUDIOHUB_MACOS_BUILD_LOCK_PATH="$BUILD_LOCK"
  else
    print -ru2 -- "[audiohub] WARNING: /usr/bin/lockf is missing on this host; \
building without the concurrent-build lock."
  fi
fi

[[ -d "$APP" ]] || { print -u2 -- "missing app bundle: $APP"; exit 1; }
[[ -s "$APP/Contents/Resources/installer/AudioHubDriver.pkg" ]] \
  || { print -u2 -- "AudioHub.app does not contain its driver package"; exit 1; }
[[ -x "$APP/Contents/Resources/installer/install-daemon.sh" ]] \
  || { print -u2 -- "AudioHub.app does not contain its daemon installer"; exit 1; }
PLIST="$APP/Contents/Info.plist"
[[ "$(/usr/bin/plutil -extract CFBundleIdentifier raw -o - "$PLIST" 2>/dev/null || true)" == com.audiohub.app ]] \
  || { print -u2 -- "refusing an app with an unexpected bundle identifier"; exit 1; }
APP_EXE="$(/usr/bin/plutil -extract CFBundleExecutable raw -o - "$PLIST" 2>/dev/null || true)"
for executable in "$APP/Contents/MacOS/$APP_EXE" "$APP/Contents/MacOS/audiohubd" "$APP/Contents/MacOS/audiohub"; do
  [[ -x "$executable" ]] || { print -u2 -- "AudioHub.app is missing executable payload: $executable"; exit 1; }
done
/usr/bin/codesign --verify --deep --strict "$APP" \
  || { print -u2 -- "AudioHub.app failed code-signature verification"; exit 1; }
/usr/sbin/pkgutil --payload-files "$APP/Contents/Resources/installer/AudioHubDriver.pkg" \
  | /usr/bin/grep -q 'AudioHubDriver.driver/Contents/MacOS/AudioHubDriver' \
  || { print -u2 -- "embedded AudioHub driver package failed payload verification"; exit 1; }

if [[ "${AUDIOHUB_RELEASE:-0}" == 1 && "${AUDIOHUB_INSTALLER_IDENTITY:-}" != Developer\ ID\ Installer:* ]]; then
  print -u2 -- "release build requires AUDIOHUB_INSTALLER_IDENTITY='Developer ID Installer: …'"
  exit 1
fi

mkdir -p "$OUT_DIR"
# Keep the candidate and all of its verification state on the destination
# filesystem. Only the final rename publishes it, so a failure, notarisation
# rejection, or Ctrl-C cannot truncate the previous known-good package.
WORK="$(/usr/bin/mktemp -d "$OUT_DIR/.AudioHub-pkg-build.XXXXXX")"
cleanup() { /bin/rm -rf "$WORK"; }
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

APP_PKG="$WORK/AudioHubApp.pkg"
DIST="$WORK/Distribution.xml"
PAYLOAD_ROOT="$WORK/root"
COMPONENTS="$ROOT/app/installer/macos/app-components.plist"
if [[ "${AUDIOHUB_RELEASE:-0}" == 1 ]]; then
  FINAL="$OUT_DIR/AudioHub-${VERSION}.pkg"
else
  FINAL="$OUT_DIR/AudioHub-${VERSION}-dev.pkg"
fi
[[ ! -L "$FINAL" && ( ! -e "$FINAL" || -f "$FINAL" ) ]] \
  || { print -u2 -- "refusing a non-regular final package path: $FINAL"; exit 1; }
CANDIDATE="$WORK/${FINAL:t}"
[[ -f "$COMPONENTS" ]] || { print -u2 -- "missing component policy: $COMPONENTS"; exit 1; }
/usr/bin/plutil -lint "$COMPONENTS" >/dev/null
/bin/mkdir -p "$PAYLOAD_ROOT"
# PackageKit can serialize harmless Finder/provenance xattrs as visible `._*`
# AppleDouble payload files. They are not part of the signed product and make
# the otherwise App-only BOM noisy, so stage only ordinary filesystem data.
/usr/bin/ditto --norsrc --noextattr --noacl --noqtn \
  "$APP" "$PAYLOAD_ROOT/AudioHub.app"
/usr/bin/cmp -s "$APP/Contents/MacOS/audiohubd" "$PAYLOAD_ROOT/AudioHub.app/Contents/MacOS/audiohubd" \
  || { print -u2 -- "staged daemon differs from the signed App"; exit 1; }

app_args=(
  --root "$PAYLOAD_ROOT"
  --install-location /Applications
  --ownership recommended
  --identifier com.audiohub.app.pkg
  --version "$VERSION"
  --component-plist "$COMPONENTS"
)
if [[ -n "${AUDIOHUB_INSTALLER_IDENTITY:-}" ]]; then
  app_args+=(--sign "$AUDIOHUB_INSTALLER_IDENTITY")
fi
pkgbuild "${app_args[@]}" "$APP_PKG"

# The HAL package stays nested in AudioHub.app. Installing this outer package
# must not silently create system audio devices; Settings > Mode owns that
# explicit operation.
productbuild --synthesize --package "$APP_PKG" "$DIST"
product_args=(--distribution "$DIST" --package-path "$WORK")
if [[ -n "${AUDIOHUB_INSTALLER_IDENTITY:-}" ]]; then
  product_args+=(--sign "$AUDIOHUB_INSTALLER_IDENTITY")
fi
productbuild "${product_args[@]}" "$CANDIDATE"

[[ -s "$CANDIDATE" ]] || { print -u2 -- "package candidate was not produced: $CANDIDATE"; exit 1; }
PAYLOAD_LIST="$WORK/payload-files"
/usr/sbin/pkgutil --payload-files "$CANDIDATE" > "$PAYLOAD_LIST"
/usr/bin/grep -q 'AudioHub.app/Contents/MacOS/audiohubd' "$PAYLOAD_LIST" \
  || { print -u2 -- "final package is missing the bundled daemon"; exit 1; }
while IFS= read -r payload; do
  case "$payload" in
    ._*|*/._*) print -u2 -- "final package contains AppleDouble metadata: $payload"; exit 1 ;;
  esac
  case "$payload" in
    .|./AudioHub.app|./AudioHub.app/*) ;;
    *) print -u2 -- "final package contains a payload outside AudioHub.app: $payload"; exit 1 ;;
  esac
done < "$PAYLOAD_LIST"

EXPANDED="$WORK/expanded"
/usr/sbin/pkgutil --expand-full "$CANDIDATE" "$EXPANDED"
if /usr/bin/find "$EXPANDED" -type d -name Scripts -print -quit | /usr/bin/grep -q .; then
  print -u2 -- "final package contains lifecycle scripts; it must install only the App"
  exit 1
fi
if /usr/bin/find "$EXPANDED" -type f -name PackageInfo -exec /usr/bin/grep -El '<scripts>|<relocate>' {} + \
  | /usr/bin/grep -q .; then
  print -u2 -- "final package contains scripts or a relocatable App rule"
  exit 1
fi
EXPANDED_APP="$EXPANDED/AudioHubApp.pkg/Payload/AudioHub.app"
[[ -d "$EXPANDED_APP" ]] || { print -u2 -- "expanded package has no AudioHub.app"; exit 1; }
/usr/bin/codesign --verify --deep --strict "$EXPANDED_APP" \
  || { print -u2 -- "packaged AudioHub.app failed code-signature verification"; exit 1; }
if [[ -n "${AUDIOHUB_INSTALLER_IDENTITY:-}" ]]; then
  pkgutil --check-signature "$CANDIDATE"
else
  print -u2 -- "[audiohub] development installer: unsigned and not for distribution"
fi

if [[ -n "${AUDIOHUB_NOTARY_PROFILE:-}" ]]; then
  /usr/bin/xcrun notarytool submit "$CANDIDATE" \
    --keychain-profile "$AUDIOHUB_NOTARY_PROFILE" --wait
  /usr/bin/xcrun stapler staple "$CANDIDATE"
  /usr/bin/xcrun stapler validate "$CANDIDATE"
  /usr/sbin/pkgutil --check-signature "$CANDIDATE" >/dev/null
elif [[ "${AUDIOHUB_RELEASE:-0}" == 1 ]]; then
  print -u2 -- "release package requires AUDIOHUB_NOTARY_PROFILE for xcrun notarytool"
  exit 1
fi

# rename(2) on the shared output filesystem is the sole publication point.
/bin/mv -f "$CANDIDATE" "$FINAL"
[[ -s "$FINAL" ]] || { print -u2 -- "final package was not published: $FINAL"; exit 1; }
print -- "$FINAL"
