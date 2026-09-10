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
# One source of truth for the version: the same file .github/workflows/build.yml
# reads with jq to derive the tag and the release name. A literal here is a
# second source that drifts silently — it agreed with the workflow at 1.0.0 only
# by coincidence, and the first bump that misses one of them ships a dmg whose
# filename contradicts its own tag. plutil parses JSON and ships with macOS, so
# this adds no dependency to a script that already refuses to run anywhere else.
VERSION="${AUDIOHUB_VERSION:-}"
if [[ -z "$VERSION" ]]; then
  VERSION="$(/usr/bin/plutil -extract version raw -o - "$TAURI/tauri.conf.json")" \
    || { print -u2 -- "[audiohub] ERROR: cannot read version from $TAURI/tauri.conf.json"; exit 1; }
  [[ -n "$VERSION" ]] \
    || { print -u2 -- "[audiohub] ERROR: empty version in $TAURI/tauri.conf.json"; exit 1; }
fi

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
# Development and Release artifacts share a final filename.
#
# They used to not: an unsigned build got a `-dev` suffix so it could never
# silently take the place of a signed one in a handoff directory. That
# separation was dropped on 2026-08-16 — every artifact this project ships is
# unsigned today, so the suffix only ever appeared, and the released names had
# to be corrected by hand.
#
# ⚠ The hazard it guarded is real and comes back the day a signed build exists:
# from then on a local unsigned build and a release installer are the same
# filename, and the only thing telling them apart is the signature itself. If
# signing identities ever land, restore the distinction here — by directory or
# by suffix — before the first signed artifact is produced.
FINAL="$OUT_DIR/AudioHub-${VERSION}.pkg"
rm -f "$OUT_DIR/AudioHub-${VERSION}-dev.pkg"
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
# The package becomes root-owned at install time. A private build umask must
# not make its public application payload unreadable to the installing user.
/usr/bin/find "$PAYLOAD_ROOT" -type d -exec /bin/chmod 0755 {} +
/usr/bin/find "$PAYLOAD_ROOT" -type f -perm -100 -exec /bin/chmod 0755 {} +
/usr/bin/find "$PAYLOAD_ROOT" -type f ! -perm -100 -exec /bin/chmod 0644 {} +
/usr/bin/cmp -s "$APP/Contents/MacOS/audiohubd" "$PAYLOAD_ROOT/AudioHub.app/Contents/MacOS/audiohubd" \
  || { print -u2 -- "staged daemon differs from the signed App"; exit 1; }
/usr/bin/codesign --verify --deep --strict "$PAYLOAD_ROOT/AudioHub.app" \
  || { print -u2 -- "staged App signature differs after permission normalization"; exit 1; }

app_args=(
  --root "$PAYLOAD_ROOT"
  --install-location /Applications
  --ownership recommended
  --identifier com.audiohub.app.pkg
  --version "$VERSION"
  --component-plist "$COMPONENTS"
  --scripts "$WORK/scripts"
)
# Do not enable the historical app-scripts directory: its service installer is
# outside the App-only authorization boundary. Stage exactly this one hook.
/bin/mkdir "$WORK/scripts"
/usr/bin/install -m 0755 "$ROOT/app/installer/macos/pkg-scripts/preinstall" "$WORK/scripts/preinstall"
if [[ -n "${AUDIOHUB_INSTALLER_IDENTITY:-}" ]]; then
  app_args+=(--sign "$AUDIOHUB_INSTALLER_IDENTITY")
fi
pkgbuild "${app_args[@]}" "$APP_PKG"

# The HAL package stays nested in AudioHub.app. Installing this outer package
# must not silently create system audio devices; Settings > Mode owns that
# explicit operation.
productbuild --synthesize --package "$APP_PKG" "$DIST"

# Put the brand mark behind the installer's left pane.
#
# `--synthesize` writes a plain distribution with no presentation, so the image
# is injected afterwards rather than maintained as a checked-in XML the
# synthesizer would have to be kept in step with. The file is generated by
# app/src-tauri/icons/make-icons.py from the same geometry as the app icon, so
# the setup window cannot drift from the application it is installing.
#
# `alignment="left" scaling="proportional"` is what Installer.app expects for a
# pane image; `mime-type` is required or the element is ignored silently.
PKG_BG="$ROOT/assets/installer/macos-pkg-background.png"
if [[ -f "$PKG_BG" ]]; then
  RES="$WORK/resources"
  mkdir -p "$RES"
  cp "$PKG_BG" "$RES/background.png"
  /usr/bin/python3 - "$DIST" <<'PYEOF'
import sys
path = sys.argv[1]
text = open(path, encoding="utf-8").read()
tag = ('<background file="background.png" mime-type="image/png" '
       'alignment="left" scaling="proportional"/>\n'
       '<background-darkAqua file="background.png" mime-type="image/png" '
       'alignment="left" scaling="proportional"/>\n')
marker = "</installer-gui-script>"
if "<background" not in text and marker in text:
    open(path, "w", encoding="utf-8").write(text.replace(marker, tag + marker, 1))
PYEOF
fi

product_args=(--distribution "$DIST" --package-path "$WORK")
[[ -d "$WORK/resources" ]] && product_args+=(--resources "$WORK/resources")
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
/bin/zsh "$ROOT/scripts/verify-macos-app-pkg.sh" "$EXPANDED"
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
