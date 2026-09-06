#!/bin/zsh
# Build AudioHub's final macOS delivery image. The DMG contains exactly the
# signed installer package, a double-clickable uninstaller, and a short readme.
# The uninstaller preserves user configuration while removing the App and its
# dependent virtual audio driver as one complete product.
set -euo pipefail

ROOT="$(cd "${0:a:h}/.." && pwd)"
TAURI="$ROOT/app/src-tauri"
# Derived, never written twice; see the note in build-macos-pkg.sh.
VERSION="${AUDIOHUB_VERSION:-}"
if [[ -z "$VERSION" ]]; then
  VERSION="$(/usr/bin/plutil -extract version raw -o - "$TAURI/tauri.conf.json")" \
    || { print -u2 -- "[audiohub] ERROR: cannot read version from $TAURI/tauri.conf.json"; exit 1; }
  [[ -n "$VERSION" ]] \
    || { print -u2 -- "[audiohub] ERROR: empty version in $TAURI/tauri.conf.json"; exit 1; }
fi
if (( $# > 0 )); then
  PKG="$1"
else
  PKG="$TAURI/target/release/bundle/pkg/AudioHub-${VERSION}.pkg"
fi
OUT_DIR="$TAURI/target/release/bundle/dmg"
SOURCE="$ROOT/app/installer/macos/uninstall.applescript"

die() { print -u2 -- "[audiohub] ERROR: $*"; exit 1; }

[[ "$(uname -s)" == Darwin ]] || die "macOS is required"

# Use the same persistent repository lock as build-app.sh. A child inherits
# fd 8; a directly invoked DMG build acquires the lock before inspecting the
# PKG so it cannot race a build that is replacing the App/package resources.
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

[[ -s "$PKG" ]] || { print -u2 -- "missing installer package: $PKG"; exit 1; }
[[ -f "$SOURCE" ]] || { print -u2 -- "missing uninstaller source: $SOURCE"; exit 1; }

mkdir -p "$OUT_DIR"
# The whole staging tree and the unpublished image live below the final output
# directory. Moving the verified candidate to FINAL therefore stays on one
# filesystem and is atomic; cleanup never names or removes FINAL.
WORK="$(/usr/bin/mktemp -d "$OUT_DIR/.AudioHub-dmg-build.XXXXXX")"
MOUNT=""
cleanup() {
  if [[ -n "$MOUNT" && -d "$MOUNT" ]]; then
    /usr/bin/hdiutil detach "$MOUNT" -quiet 2>/dev/null || true
  fi
  /bin/rm -rf "$WORK"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# A DMG is one release unit: never wrap an older, script-bearing or relocatable
# PKG with a freshly compiled uninstaller. The outer installer has one job:
# place AudioHub.app in /Applications. Service and driver setup are explicit
# App actions after launch, so PackageKit must have no hidden lifecycle hooks.
/usr/sbin/pkgutil --expand-full "$PKG" "$WORK/pkg-preflight"
if /usr/bin/find "$WORK/pkg-preflight" -type d -name Scripts -print -quit \
  | /usr/bin/grep -q .; then
  print -u2 -- "installer package contains lifecycle scripts; rebuild the PKG"
  exit 1
fi
if /usr/bin/find "$WORK/pkg-preflight" -type f -name PackageInfo \
  -exec /usr/bin/grep -El '<scripts>|<relocate>' {} + | /usr/bin/grep -q .; then
  print -u2 -- "installer package contains scripts or relocate metadata; rebuild the PKG"
  exit 1
fi
PKG_PAYLOAD="$WORK/pkg-preflight-payload-files"
/usr/sbin/pkgutil --payload-files "$PKG" >"$PKG_PAYLOAD"
APP_ROOT_SEEN=0
APP_EXECUTABLE_SEEN=0
while IFS= read -r payload; do
  case "$payload" in
    ._*|*/._*)
      print -u2 -- "installer package contains AppleDouble metadata: $payload"
      exit 1
      ;;
  esac
  case "$payload" in
    .) ;;
    ./AudioHub.app) APP_ROOT_SEEN=1 ;;
    ./AudioHub.app/Contents/MacOS/audiohub-app) APP_EXECUTABLE_SEEN=1 ;;
    ./AudioHub.app/*) ;;
    *)
      print -u2 -- "installer must contain only AudioHub.app; unexpected payload: $payload"
      exit 1
      ;;
  esac
done <"$PKG_PAYLOAD"
[[ "$APP_ROOT_SEEN" == 1 && "$APP_EXECUTABLE_SEEN" == 1 ]] \
  || { print -u2 -- "installer must contain only AudioHub.app with its main executable"; exit 1; }
EXPANDED_APP="$WORK/pkg-preflight/AudioHubApp.pkg/Payload/AudioHub.app"
[[ -d "$EXPANDED_APP" ]] \
  || { print -u2 -- "installer package has no expanded AudioHub.app"; exit 1; }
/usr/bin/codesign --verify --deep --strict "$EXPANDED_APP" \
  || { print -u2 -- "installer package contains an invalid AudioHub.app signature"; exit 1; }

if [[ "${AUDIOHUB_RELEASE:-0}" == 1 ]]; then
  [[ "${AUDIOHUB_SIGN_IDENTITY:-}" == Developer\ ID\ Application:* ]] \
    || { print -u2 -- "release DMG requires AUDIOHUB_SIGN_IDENTITY='Developer ID Application: …'"; exit 1; }
  [[ "${AUDIOHUB_INSTALLER_IDENTITY:-}" == Developer\ ID\ Installer:* ]] \
    || { print -u2 -- "release DMG requires AUDIOHUB_INSTALLER_IDENTITY='Developer ID Installer: …'"; exit 1; }
  [[ "${AUDIOHUB_NOTARY_PROFILE:-}" != "" ]] \
    || { print -u2 -- "release DMG requires AUDIOHUB_NOTARY_PROFILE for xcrun notarytool"; exit 1; }
fi

STAGE="$WORK/AudioHub"
UNINSTALLER="$STAGE/Uninstall AudioHub.app"
mkdir -p "$STAGE"
/usr/bin/ditto --norsrc --noextattr --noacl --noqtn \
  "$PKG" "$STAGE/Install AudioHub.pkg"
/usr/bin/cmp -s "$PKG" "$STAGE/Install AudioHub.pkg" \
  || { print -u2 -- "staged installer differs from the verified package"; exit 1; }

# A script application gives Finder-native double-click behavior and the native
# Authorization Services password sheet without shipping a second framework or
# exposing a Terminal window.
/usr/bin/osacompile -o "$UNINSTALLER" "$SOURCE"
PLIST="$UNINSTALLER/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Add :CFBundleIdentifier string com.audiohub.uninstaller" "$PLIST" 2>/dev/null \
  || /usr/libexec/PlistBuddy -c "Set :CFBundleIdentifier com.audiohub.uninstaller" "$PLIST"
/usr/libexec/PlistBuddy -c "Set :CFBundleName Uninstall AudioHub" "$PLIST"
/usr/libexec/PlistBuddy -c "Add :CFBundleDisplayName string Uninstall AudioHub" "$PLIST" 2>/dev/null \
  || /usr/libexec/PlistBuddy -c "Set :CFBundleDisplayName Uninstall AudioHub" "$PLIST"
/usr/libexec/PlistBuddy -c "Add :CFBundleShortVersionString string $VERSION" "$PLIST" 2>/dev/null \
  || /usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $VERSION" "$PLIST"
/usr/libexec/PlistBuddy -c "Add :CFBundleVersion string $VERSION" "$PLIST" 2>/dev/null \
  || /usr/libexec/PlistBuddy -c "Set :CFBundleVersion $VERSION" "$PLIST"
/usr/libexec/PlistBuddy -c "Add :LSMinimumSystemVersion string 11.0" "$PLIST" 2>/dev/null \
  || /usr/libexec/PlistBuddy -c "Set :LSMinimumSystemVersion 11.0" "$PLIST"
/usr/libexec/PlistBuddy -c "Set :NSSystemAdministrationUsageDescription AudioHub needs administrator authorization to remove its installed application, background service, and virtual audio driver." "$PLIST"
for key in NSAppleEventsUsageDescription NSAppleMusicUsageDescription NSCalendarsUsageDescription \
  NSCameraUsageDescription NSContactsUsageDescription NSHomeKitUsageDescription \
  NSMicrophoneUsageDescription NSPhotoLibraryUsageDescription NSRemindersUsageDescription \
  NSSiriUsageDescription; do
  /usr/libexec/PlistBuddy -c "Delete :$key" "$PLIST" 2>/dev/null || true
done
/usr/bin/plutil -lint "$PLIST" >/dev/null

mkdir -p "$UNINSTALLER/Contents/Resources/en.lproj" "$UNINSTALLER/Contents/Resources/zh-Hans.lproj"
print -r -- 'CFBundleDisplayName = "Uninstall AudioHub";' \
  >"$UNINSTALLER/Contents/Resources/en.lproj/InfoPlist.strings"
print -r -- 'CFBundleDisplayName = "卸载 AudioHub";' \
  >"$UNINSTALLER/Contents/Resources/zh-Hans.lproj/InfoPlist.strings"

IDENTITY="${AUDIOHUB_SIGN_IDENTITY:--}"
sign_args=(--force --sign "$IDENTITY" --identifier com.audiohub.uninstaller)
if [[ "$IDENTITY" == Developer\ ID\ Application:* ]]; then
  sign_args+=(--options runtime --timestamp)
fi
/usr/bin/codesign "${sign_args[@]}" "$UNINSTALLER"
/usr/bin/codesign --verify --deep --strict --verbose=2 "$UNINSTALLER"

cat >"$STAGE/Read Me.txt" <<'README'
安装 AudioHub.pkg 只会将 AudioHub App 安装到「应用程序」，不会静默安装或启动
后台服务。首次打开 AudioHub 后，请在 App 提示中明确选择「安装」；系统鉴权
通过后，App 会自动安装并启动后台服务。虚拟音频驱动仍需在「设置 > 模式」中由
用户明确安装。

双击卸载 AudioHub.app 可移除 App、后台服务、后台启动项及虚拟音频驱动，并保留设置、身份、
配对数据与日志。卸载驱动时系统音频会短暂重启。

Install AudioHub.pkg installs only AudioHub.app in Applications; it does not
silently install or start the background service. Open AudioHub, then explicitly
choose Install when prompted. After system authorization, the App
installs and starts the service automatically. The virtual audio driver stays
opt-in under AudioHub Settings > Mode.

Uninstall AudioHub.app removes the app, background service, startup item, and dependent
virtual audio driver. It keeps settings, identity, paired-device data, and
logs. System audio briefly restarts while the driver is removed.
README

if [[ -n "${AUDIOHUB_INSTALLER_IDENTITY:-}" ]]; then
  /usr/sbin/pkgutil --check-signature "$STAGE/Install AudioHub.pkg" >/dev/null
elif [[ "${AUDIOHUB_RELEASE:-0}" == 1 ]]; then
  print -u2 -- "release DMG contains an unsigned installer package"
  exit 1
fi

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
FINAL="$OUT_DIR/AudioHub-${VERSION}.dmg"
rm -f "$OUT_DIR/AudioHub-${VERSION}-dev.dmg"
[[ ! -L "$FINAL" && ( ! -e "$FINAL" || -f "$FINAL" ) ]] \
  || { print -u2 -- "refusing a non-regular final disk image path: $FINAL"; exit 1; }
CANDIDATE="$WORK/${FINAL:t}"
# Disk-image contents are public distribution files, not private build state.
/usr/bin/find "$STAGE" -type d -exec /bin/chmod 0755 {} +
/usr/bin/find "$STAGE" -type f -perm -100 -exec /bin/chmod 0755 {} +
/usr/bin/find "$STAGE" -type f ! -perm -100 -exec /bin/chmod 0644 {} +
/usr/bin/codesign --verify --deep --strict "$UNINSTALLER"
/usr/bin/hdiutil create -quiet -ov -format UDZO -imagekey zlib-level=9 \
  -volname "AudioHub $VERSION" -srcfolder "$STAGE" "$CANDIDATE"

if [[ "$IDENTITY" != "-" ]]; then
  dmg_sign=(--force --sign "$IDENTITY")
  [[ "$IDENTITY" == Developer\ ID\ Application:* ]] && dmg_sign+=(--timestamp)
  /usr/bin/codesign "${dmg_sign[@]}" "$CANDIDATE"
  /usr/bin/codesign --verify --verbose=2 "$CANDIDATE"
fi

if [[ -n "${AUDIOHUB_NOTARY_PROFILE:-}" ]]; then
  /usr/bin/xcrun notarytool submit "$CANDIDATE" \
    --keychain-profile "$AUDIOHUB_NOTARY_PROFILE" --wait
  /usr/bin/xcrun stapler staple "$CANDIDATE"
  /usr/bin/xcrun stapler validate "$CANDIDATE"
  /usr/bin/codesign --verify --verbose=2 "$CANDIDATE"
fi

# Verify both the UDIF checksum and the actual read-only Finder surface. Merely
# checking that hdiutil produced bytes misses malformed filesystems and missing
# top-level artifacts.
/usr/bin/hdiutil verify "$CANDIDATE" >/dev/null
ATTACH_PLIST="$WORK/attach.plist"
/usr/bin/hdiutil attach -readonly -nobrowse -noautoopen -plist "$CANDIDATE" >"$ATTACH_PLIST"
for index in {0..9}; do
  candidate="$(/usr/libexec/PlistBuddy -c "Print :system-entities:$index:mount-point" "$ATTACH_PLIST" 2>/dev/null || true)"
  if [[ -n "$candidate" ]]; then MOUNT="$candidate"; break; fi
done
[[ -n "$MOUNT" && -d "$MOUNT" ]] || { print -u2 -- "DMG attached without a mounted volume"; exit 1; }
[[ -s "$MOUNT/Install AudioHub.pkg" ]] || { print -u2 -- "mounted DMG has no installer"; exit 1; }
[[ -d "$MOUNT/Uninstall AudioHub.app" ]] || { print -u2 -- "mounted DMG has no uninstaller"; exit 1; }
[[ -s "$MOUNT/Read Me.txt" ]] || { print -u2 -- "mounted DMG has no readme"; exit 1; }
/usr/bin/codesign --verify --deep --strict "$MOUNT/Uninstall AudioHub.app"
MOUNT_PAYLOAD="$WORK/mounted-payload-files"
/usr/sbin/pkgutil --payload-files "$MOUNT/Install AudioHub.pkg" >"$MOUNT_PAYLOAD"
MOUNT_APP_ROOT_SEEN=0
MOUNT_APP_EXECUTABLE_SEEN=0
while IFS= read -r payload; do
  case "$payload" in
    ._*|*/._*)
      print -u2 -- "mounted installer contains AppleDouble metadata: $payload"
      exit 1
      ;;
  esac
  case "$payload" in
    .) ;;
    ./AudioHub.app) MOUNT_APP_ROOT_SEEN=1 ;;
    ./AudioHub.app/Contents/MacOS/audiohub-app) MOUNT_APP_EXECUTABLE_SEEN=1 ;;
    ./AudioHub.app/*) ;;
    *)
      print -u2 -- "mounted installer contains payload outside AudioHub.app: $payload"
      exit 1
      ;;
  esac
done <"$MOUNT_PAYLOAD"
[[ "$MOUNT_APP_ROOT_SEEN" == 1 && "$MOUNT_APP_EXECUTABLE_SEEN" == 1 ]] \
  || { print -u2 -- "mounted installer has no complete AudioHub.app payload"; exit 1; }
/usr/bin/cmp -s "$PKG" "$MOUNT/Install AudioHub.pkg" \
  || { print -u2 -- "mounted installer differs from the verified package"; exit 1; }
/usr/bin/hdiutil detach "$MOUNT" -quiet
MOUNT=""

# Everything above succeeds before this one publication step. The previous
# image remains untouched on every earlier failure or cancellation.
/bin/mv -f "$CANDIDATE" "$FINAL"
[[ -s "$FINAL" ]] || { print -u2 -- "final disk image was not published: $FINAL"; exit 1; }
HASH="$(/usr/bin/shasum -a 256 "$FINAL" | /usr/bin/awk '{print $1}')"
print -u2 -- "[audiohub] dmg:    $FINAL"
print -u2 -- "[audiohub] bytes:  $(/usr/bin/stat -f %z "$FINAL")"
print -u2 -- "[audiohub] sha256: $HASH"
print -- "$FINAL"
