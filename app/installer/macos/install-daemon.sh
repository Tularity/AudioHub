#!/bin/sh
# Install the daemon payload bundled in /Applications/AudioHub.app.
#
# This script is executed as root through the App's fixed authorization action.
# It accepts no arguments and reads no caller-controlled paths.  The App itself
# remains sealed: the bundled daemon is copied to a root-owned staging directory
# before a machine-local code-signing identity is applied.
set -eu

APP='/Applications/AudioHub.app'
SOURCE="$APP/Contents/MacOS/audiohubd"
BASE='/Library/Application Support/AudioHub/service'
VERSIONS="$BASE/versions"
SIGNING='/Library/Application Support/AudioHub/signing'
KEYCHAIN="$SIGNING/identity.keychain-db"
KEYCHAIN_PASSWORD="$SIGNING/keychain.pass"
CERTIFICATE_RECORD="$SIGNING/certificate.sha1"
LIFECYCLE_LOCK='/var/run/com.audiohub.lifecycle.lock'
IDENTITY_LABEL='AudioHub Local Code Signing'
DAEMON_IDENTIFIER='com.audiohub.daemon'
CONSOLE_USER=
CONSOLE_UID=
CONSOLE_GID=
CONSOLE_HOME=

fail() {
  echo "audiohub daemon install: $*" >&2
  exit 1
}

path_exists() {
  [ -e "$1" ] || [ -L "$1" ]
}

safe_root_dir() {
  path="$1"
  [ -d "$path" ] && [ ! -L "$path" ] || return 1
  [ "$(/usr/bin/stat -f '%u' "$path" 2>/dev/null || true)" = 0 ] || return 1
  mode=$(/usr/bin/stat -f '%Lp' "$path" 2>/dev/null || true)
  case "$mode" in ''|*[!0-7]*) return 1;; esac
  [ $((0$mode & 022)) -eq 0 ]
}

check_root_dir() {
  path="$1"
  safe_root_dir "$path" || fail "unsafe or non-root-owned directory: $path"
}

check_private_root_dir() {
  path="$1"
  check_root_dir "$path"
  mode=$(/usr/bin/stat -f '%Lp' "$path" 2>/dev/null || true)
  [ $((0$mode & 077)) -eq 0 ] || fail "private directory is accessible by another user: $path"
}

safe_root_file() {
  path="$1"
  [ -f "$path" ] && [ ! -L "$path" ] || return 1
  [ "$(/usr/bin/stat -f '%u' "$path" 2>/dev/null || true)" = 0 ] || return 1
  mode=$(/usr/bin/stat -f '%Lp' "$path" 2>/dev/null || true)
  case "$mode" in ''|*[!0-7]*) return 1;; esac
  [ $((0$mode & 022)) -eq 0 ]
}

check_root_file() {
  path="$1"
  safe_root_file "$path" || fail "unsafe or non-root-owned file: $path"
}

check_private_root_file() {
  path="$1"
  check_root_file "$path"
  mode=$(/usr/bin/stat -f '%Lp' "$path" 2>/dev/null || true)
  [ $((0$mode & 077)) -eq 0 ] || fail "private file is accessible by another user: $path"
}

root_wheel_owned() {
  [ "$(/usr/bin/stat -f '%u' "$1" 2>/dev/null || true)" = 0 ] \
    && [ "$(/usr/bin/stat -f '%g' "$1" 2>/dev/null || true)" = 0 ]
}

mode_is() {
  actual_mode=$(/usr/bin/stat -f '%Lp' "$1" 2>/dev/null || true)
  case "$actual_mode" in ''|*[!0-7]*) return 1;; esac
  [ $((0$actual_mode)) -eq $((0$2)) ]
}

private_root_wheel_file() {
  safe_root_file "$1" && root_wheel_owned "$1" && mode_is "$1" 0600
}

safe_console_file() {
  path="$1"
  expected_mode="$2"
  [ -f "$path" ] && [ ! -L "$path" ] || return 1
  [ "$(/usr/bin/stat -f '%u' "$path" 2>/dev/null || true)" = "$CONSOLE_UID" ] \
    || return 1
  mode_is "$path" "$expected_mode"
}

safe_console_dir() {
  path="$1"
  expected_mode="$2"
  [ -d "$path" ] && [ ! -L "$path" ] || return 1
  [ "$(/usr/bin/stat -f '%u' "$path" 2>/dev/null || true)" = "$CONSOLE_UID" ] \
    || return 1
  mode_is "$path" "$expected_mode"
}

private_tmp_is_safe() {
  [ -d /private/tmp ] && [ ! -L /private/tmp ] \
    && [ "$(/usr/bin/stat -f '%u:%g:%p' /private/tmp 2>/dev/null || true)" = '0:0:41777' ]
}

console_user_is_still_active() {
  [ "$(/usr/bin/stat -f '%u' /dev/console 2>/dev/null || true)" = "$CONSOLE_UID" ] \
    && /bin/launchctl print "gui/$CONSOLE_UID" >/dev/null 2>&1
}

resolve_console_user() {
  CONSOLE_UID=$(/usr/bin/stat -f '%u' /dev/console 2>/dev/null || true)
  CONSOLE_USER=$(/usr/bin/stat -f '%Su' /dev/console 2>/dev/null || true)
  case "$CONSOLE_UID" in ''|*[!0-9]*) fail 'could not identify the active macOS user';; esac
  [ "$CONSOLE_UID" -ge 500 ] \
    || fail 'AudioHub must be installed from an active macOS user session'
  case "$CONSOLE_USER" in
    ''|root|loginwindow|-*|_*|*[!A-Za-z0-9._-]*)
      fail 'the active macOS user name is unsafe'
      ;;
  esac
  [ "$(/usr/bin/id -u "$CONSOLE_USER" 2>/dev/null || true)" = "$CONSOLE_UID" ] \
    || fail 'the active macOS user identity changed'
  CONSOLE_GID=$(/usr/bin/id -g "$CONSOLE_USER" 2>/dev/null || true)
  case "$CONSOLE_GID" in ''|*[!0-9]*) fail 'could not identify the active macOS user group';; esac
  CONSOLE_HOME=$(/usr/bin/dscl . -read "/Users/$CONSOLE_USER" NFSHomeDirectory 2>/dev/null \
    | /usr/bin/awk 'NR == 1 && $1 == "NFSHomeDirectory:" {$1=""; sub(/^ /, ""); print; exit}')
  case "$CONSOLE_HOME" in /*) ;; *) fail 'the active macOS user home is invalid';; esac
  if printf '%s' "$CONSOLE_HOME" | /usr/bin/grep -q '[[:cntrl:]]'; then
    fail 'the active macOS user home contains unsafe characters'
  fi
  [ -d "$CONSOLE_HOME" ] && [ ! -L "$CONSOLE_HOME" ] \
    && [ "$(/usr/bin/stat -f '%u' "$CONSOLE_HOME" 2>/dev/null || true)" = "$CONSOLE_UID" ] \
    || fail 'the active macOS user home is unavailable or unsafe'
  /bin/launchctl print "gui/$CONSOLE_UID" >/dev/null 2>&1 \
    || fail 'the active macOS user session is unavailable'
}

as_console_user() {
  /bin/launchctl asuser "$CONSOLE_UID" \
    /usr/bin/sudo -n -H -u "#$CONSOLE_UID" -- \
    /usr/bin/env -i \
      HOME="$CONSOLE_HOME" \
      USER="$CONSOLE_USER" \
      LOGNAME="$CONSOLE_USER" \
      PATH='/usr/bin:/bin:/usr/sbin:/sbin' \
      "$@"
}

acquire_lifecycle_lock() {
  # Publish a fully initialized inode atomically. A check followed by a direct
  # create/truncate lets two first-run installers lock different inodes.
  if ! path_exists "$LIFECYCLE_LOCK"; then
    LOCK_CANDIDATE=$(/usr/bin/mktemp "${LIFECYCLE_LOCK}.candidate.XXXXXX") \
      || fail 'could not prepare the AudioHub lifecycle lock'
    /usr/sbin/chown root:wheel "$LOCK_CANDIDATE" \
      || { /bin/rm -f "$LOCK_CANDIDATE"; fail 'could not own the AudioHub lifecycle lock'; }
    /bin/chmod 0600 "$LOCK_CANDIDATE" \
      || { /bin/rm -f "$LOCK_CANDIDATE"; fail 'could not protect the AudioHub lifecycle lock'; }
    private_root_wheel_file "$LOCK_CANDIDATE" \
      || { /bin/rm -f "$LOCK_CANDIDATE"; fail 'the new AudioHub lifecycle lock is unsafe'; }
    if ! /bin/ln "$LOCK_CANDIDATE" "$LIFECYCLE_LOCK" 2>/dev/null \
      && ! path_exists "$LIFECYCLE_LOCK"; then
      /bin/rm -f "$LOCK_CANDIDATE"
      fail 'could not publish the AudioHub lifecycle lock'
    fi
    /bin/rm -f "$LOCK_CANDIDATE" \
      || fail 'could not finish preparing the AudioHub lifecycle lock'
  fi

  private_root_wheel_file "$LIFECYCLE_LOCK" \
    || fail 'the AudioHub lifecycle lock is a symlink or has unsafe metadata'
  LOCK_OBJECT_ID=$(/usr/bin/stat -f '%i' "$LIFECYCLE_LOCK" 2>/dev/null || true)
  [ -n "$LOCK_OBJECT_ID" ] || fail 'could not identify the AudioHub lifecycle lock'
  exec 9<> "$LIFECYCLE_LOCK" \
    || fail 'could not open the AudioHub lifecycle lock'
  LOCK_FD_OBJECT_ID=$(/usr/bin/stat -f '%i' /dev/fd/9 2>/dev/null || true)
  [ "$LOCK_FD_OBJECT_ID" = "$LOCK_OBJECT_ID" ] \
    || fail 'the AudioHub lifecycle lock changed while opening it'
  /usr/bin/lockf -s -t 0 9 \
    || fail 'another AudioHub install or uninstall operation is already running'
  private_root_wheel_file "$LIFECYCLE_LOCK" \
    && [ "$(/usr/bin/stat -f '%i' "$LIFECYCLE_LOCK" 2>/dev/null || true)" = "$LOCK_OBJECT_ID" ] \
    || fail 'the AudioHub lifecycle lock changed after it was acquired'
}

valid_sha1() {
  value="$1"
  case "$value" in ''|*[!0-9a-f]*) return 1;; esac
  [ "${#value}" -eq 40 ]
}

valid_sha256() {
  value="$1"
  case "$value" in ''|*[!0-9a-f]*) return 1;; esac
  [ "${#value}" -eq 64 ]
}

valid_version_target() {
  target="$1"
  case "$target" in
    versions/*) version_hash=${target#versions/} ;;
    *) return 1 ;;
  esac
  valid_sha256 "$version_hash"
}

[ "$(/usr/bin/id -u)" = 0 ] || fail 'administrator privileges are required'

# Validate every component that is traversed before invoking any privileged
# writer.  A valid outer signature does not make following an attacker-created
# directory symlink an acceptable installation operation.
[ -d "$APP" ] && [ ! -L "$APP" ] || fail 'the installed AudioHub app is unavailable'
check_root_dir "$APP"
check_root_dir "$APP/Contents"
check_root_dir "$APP/Contents/MacOS"
check_root_file "$APP/Contents/Info.plist"
APP_OBJECT_ID=$(/usr/bin/stat -f '%d:%i' "$APP" 2>/dev/null || true)
[ -n "$APP_OBJECT_ID" ] || fail 'could not identify the installed AudioHub app'

APP_ID=$(/usr/bin/plutil -extract CFBundleIdentifier raw -o - "$APP/Contents/Info.plist" 2>/dev/null || true)
[ "$APP_ID" = 'com.audiohub.app' ] || fail 'the installed app has an unexpected bundle identifier'
/usr/bin/codesign --verify --deep --strict "$APP" \
  || fail 'the installed AudioHub app has an invalid code signature'

[ -f "$SOURCE" ] && [ ! -L "$SOURCE" ] && [ -x "$SOURCE" ] \
  || fail 'the AudioHub daemon payload is missing or unsafe'
check_root_file "$SOURCE"
SOURCE_OBJECT_ID=$(/usr/bin/stat -f '%d:%i' "$SOURCE" 2>/dev/null || true)
[ -n "$SOURCE_OBJECT_ID" ] || fail 'could not identify the bundled daemon payload'
SOURCE_ID=$(/usr/bin/codesign -d --verbose=4 "$SOURCE" 2>&1 \
  | /usr/bin/sed -n 's/^Identifier=//p' | /usr/bin/head -n 1)
[ "$SOURCE_ID" = "$DAEMON_IDENTIFIER" ] || fail 'the bundled daemon has an unexpected identifier'
/usr/bin/codesign --verify --strict "$SOURCE" \
  || fail 'the bundled daemon has an invalid code signature'
SOURCE_SHA=$(/usr/bin/shasum -a 256 "$SOURCE" | /usr/bin/awk '{print $1}')
valid_sha256 "$SOURCE_SHA" || fail 'the bundled daemon hash is invalid'

# codesign depends on the login user's Security session and is unsupported from
# a sudo/root-only account context. Resolve that GUI session before making any
# machine-level change; root will still verify and publish every resulting byte.
resolve_console_user

old_umask=$(umask)
umask 077

# This fixed lock is outside the product tree, so uninstall cannot remove the
# inode while an install/repair transaction is using it. Acquire it before the
# first machine-level directory, keychain, or daemon mutation.
acquire_lifecycle_lock

# Never call install -d through an object that has not first been checked. Once
# an existing parent is known to be root-owned and non-writable, creating its
# immediate child and checking it again is race-free with respect to ordinary
# users.  Existing safe directories are tightened to the intended mode.
check_root_dir '/Library/Application Support'
AUDIOHUB_SUPPORT=${BASE%/service}
[ "$AUDIOHUB_SUPPORT/service" = "$BASE" ] \
  || fail 'the service directory layout is invalid'
if path_exists "$AUDIOHUB_SUPPORT"; then check_root_dir "$AUDIOHUB_SUPPORT"; fi
/usr/bin/install -d -o root -g wheel -m 0755 "$AUDIOHUB_SUPPORT"
check_root_dir "$AUDIOHUB_SUPPORT"

if path_exists "$BASE"; then check_root_dir "$BASE"; fi
if path_exists "$VERSIONS"; then check_root_dir "$VERSIONS"; fi
/usr/bin/install -d -o root -g wheel -m 0755 "$BASE" "$VERSIONS"
check_root_dir "$BASE"
check_root_dir "$VERSIONS"

if path_exists "$SIGNING"; then check_root_dir "$SIGNING"; fi
/usr/bin/install -d -o root -g wheel -m 0700 "$SIGNING"
check_private_root_dir "$SIGNING"

WORK=$(/usr/bin/mktemp -d "$BASE/.install.XXXXXX") \
  || fail 'could not create a private staging directory'
check_private_root_dir "$WORK"
PAYLOAD="$WORK/payload"
SEARCH_LIST_RAW="$WORK/keychain-search-list.raw"
SEARCH_LIST="$WORK/keychain-search-list"
FINAL="$VERSIONS/$SOURCE_SHA"
FINAL_BACKUP="$WORK/previous-version"
CURRENT="$BASE/current"
CURRENT_TMP="$WORK/current"
CURRENT_ROLLBACK="$WORK/current.rollback"
IDENTITY_BACKUP="$WORK/previous-identity"

SEARCH_CHANGED=0
IDENTITY_CREATED=0
IDENTITY_BACKED_UP=0
CERTIFICATE_RECORD_CREATED=0
FINAL_BACKED_UP=0
FINAL_INSTALLED=0
CURRENT_SWITCHED=0
CONSOLE_SEARCH_CHANGED=0
CONSOLE_SEARCH_LIST=
SIGN_SESSION_ROOT=
SIGN_SEQUENCE=0
PREVIOUS_CURRENT_KIND=absent
PREVIOUS_CURRENT_TARGET=
CERTIFICATE_SHA1=

restore_identity() {
  [ "$IDENTITY_BACKED_UP" -eq 1 ] || return 0

  # Remove any incomplete replacement first. delete-keychain also removes a
  # keychain it recognises from the active list; rm covers an only-partially
  # created database and the two ordinary record files.
  /usr/bin/security delete-keychain "$KEYCHAIN" >/dev/null 2>&1 || true
  /bin/rm -f "$KEYCHAIN" "$KEYCHAIN_PASSWORD" "$CERTIFICATE_RECORD" || return 1
  for name in identity.keychain-db keychain.pass certificate.sha1; do
    backup="$IDENTITY_BACKUP/$name"
    destination="$SIGNING/$name"
    if path_exists "$backup"; then
      safe_root_file "$backup" || return 1
      /bin/mv -h "$backup" "$destination" || return 1
    fi
  done
}

restore_search_list() {
  set --
  while IFS= read -r keychain; do
    [ -n "$keychain" ] && set -- "$@" "$keychain"
  done < "$SEARCH_LIST"
  /usr/bin/security list-keychains -d user -s "$@" >/dev/null 2>&1
}

restore_console_search_list() {
  [ "$CONSOLE_SEARCH_CHANGED" -eq 1 ] || return 0
  [ -n "$CONSOLE_SEARCH_LIST" ] && [ -f "$CONSOLE_SEARCH_LIST" ] \
    || return 1
  CONSOLE_RESTORE_RAW="$CONSOLE_SEARCH_LIST.restore.raw"
  CONSOLE_RESTORE_LIST="$CONSOLE_SEARCH_LIST.restore"
  as_console_user /usr/bin/security list-keychains -d user > "$CONSOLE_RESTORE_RAW" \
    || return 1
  parse_keychain_search_list "$CONSOLE_RESTORE_RAW" "$CONSOLE_RESTORE_LIST" \
    || return 1
  set --
  while IFS= read -r keychain; do
    [ -n "$keychain" ] && [ "$keychain" != "$CONSOLE_KEYCHAIN" ] \
      && set -- "$@" "$keychain"
  done < "$CONSOLE_RESTORE_LIST"
  as_console_user /usr/bin/security list-keychains -d user -s "$@" >/dev/null 2>&1 \
    || return 1
  CONSOLE_SEARCH_CHANGED=0
  CONSOLE_SEARCH_LIST=
}

remove_sign_session() {
  [ -n "$SIGN_SESSION_ROOT" ] || return 0
  case "$SIGN_SESSION_ROOT" in /private/tmp/com.audiohub.sign.*) ;; *) return 1;; esac
  safe_root_dir "$SIGN_SESSION_ROOT" || return 1
  /bin/rm -rfx "$SIGN_SESSION_ROOT" || return 1
  SIGN_SESSION_ROOT=
}

abort_sign_session() {
  restore_console_search_list \
    || echo 'audiohub daemon install: warning: could not restore the login user keychain search list' >&2
  if [ -n "${CONSOLE_KEYCHAIN:-}" ] && [ -f "$CONSOLE_KEYCHAIN" ] && [ ! -L "$CONSOLE_KEYCHAIN" ]; then
    as_console_user /usr/bin/security lock-keychain "$CONSOLE_KEYCHAIN" >/dev/null 2>&1 \
      || true
  fi
  remove_sign_session \
    || echo 'audiohub daemon install: warning: could not remove the temporary login-user signing session' >&2
  return 1
}

rollback_current() {
  [ "$CURRENT_SWITCHED" -eq 1 ] || return 0
  case "$PREVIOUS_CURRENT_KIND" in
    absent)
      if [ -L "$CURRENT" ] \
        && [ "$(/usr/bin/readlink "$CURRENT" 2>/dev/null || true)" = "versions/$SOURCE_SHA" ]; then
        /bin/rm -f "$CURRENT" || return 1
      elif path_exists "$CURRENT"; then
        return 1
      fi
      ;;
    symlink)
      if path_exists "$CURRENT" && [ ! -L "$CURRENT" ]; then return 1; fi
      /bin/rm -f "$CURRENT_ROLLBACK"
      /bin/ln -s "$PREVIOUS_CURRENT_TARGET" "$CURRENT_ROLLBACK" || return 1
      /usr/sbin/chown -h root:wheel "$CURRENT_ROLLBACK" || return 1
      /bin/chmod -h 0755 "$CURRENT_ROLLBACK" || return 1
      /bin/mv -f -h "$CURRENT_ROLLBACK" "$CURRENT" || return 1
      ;;
    *) return 1 ;;
  esac
}

rollback_final() {
  if [ "$FINAL_INSTALLED" -eq 1 ] && path_exists "$FINAL"; then
    safe_root_dir "$FINAL" || return 1
    /bin/rm -rf "$FINAL" || return 1
  fi
  if [ "$FINAL_BACKED_UP" -eq 1 ]; then
    if path_exists "$FINAL_BACKUP"; then
      [ ! -e "$FINAL" ] && [ ! -L "$FINAL" ] || return 1
      safe_root_dir "$FINAL_BACKUP" || return 1
      /bin/mv -h "$FINAL_BACKUP" "$FINAL" || return 1
    else
      # The backup rename did not happen; the checked original must still be
      # in place. This state is possible if mv failed or a signal arrived in
      # the deliberately pre-armed transaction window.
      path_exists "$FINAL" || return 1
      safe_root_dir "$FINAL" || return 1
    fi
  fi
}

cleanup() {
  status=$?
  trap - EXIT HUP INT TERM

  restore_console_search_list \
    || echo 'audiohub daemon install: warning: could not restore the login user keychain search list' >&2
  remove_sign_session \
    || echo 'audiohub daemon install: warning: could not remove the temporary login-user signing session' >&2

  if [ "$status" -ne 0 ]; then
    rollback_current \
      || echo 'audiohub daemon install: warning: could not restore the previous current link' >&2
    rollback_final \
      || echo 'audiohub daemon install: warning: could not restore the previous daemon version' >&2
  fi

  /usr/bin/security lock-keychain "$KEYCHAIN" >/dev/null 2>&1 || true
  if [ "$status" -ne 0 ] && [ "$IDENTITY_BACKED_UP" -eq 1 ]; then
    restore_identity \
      || echo 'audiohub daemon install: warning: could not restore the previous signing identity' >&2
  elif [ "$status" -ne 0 ] && [ "$IDENTITY_CREATED" -eq 1 ]; then
    /usr/bin/security delete-keychain "$KEYCHAIN" >/dev/null 2>&1 || true
    /bin/rm -f "$KEYCHAIN" "$KEYCHAIN_PASSWORD" "$CERTIFICATE_RECORD"
  elif [ "$status" -ne 0 ] && [ "$CERTIFICATE_RECORD_CREATED" -eq 1 ]; then
    /bin/rm -f "$CERTIFICATE_RECORD"
  fi

  # delete-keychain mutates the search list, so restore the caller's exact
  # snapshot only after identity rollback has finished.
  if [ "$SEARCH_CHANGED" -eq 1 ] && [ -f "$SEARCH_LIST" ]; then
    restore_search_list \
      || echo 'audiohub daemon install: warning: could not restore the keychain search list' >&2
  fi
  /usr/bin/security lock-keychain "$KEYCHAIN" >/dev/null 2>&1 || true

  /bin/rm -rf "$WORK"
  if [ -n "$old_umask" ]; then umask "$old_umask" 2>/dev/null || true; fi
  exit "$status"
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

# Capture the search list before create-keychain: that command may add the new
# keychain by itself.  Saving it afterwards would make the supposedly temporary
# signing keychain a permanent entry.  Reject escaped/unusual output rather
# than evaluating text emitted by the security tool in a root shell.
/usr/bin/security list-keychains -d user > "$SEARCH_LIST_RAW" \
  || fail 'could not read the keychain search list'
/usr/bin/awk '
  /^[[:space:]]*$/ { next }
  /^[[:space:]]*"[^"\\]*"[[:space:]]*$/ {
    sub(/^[[:space:]]*"/, "")
    sub(/"[[:space:]]*$/, "")
    print
    next
  }
  { exit 1 }
' "$SEARCH_LIST_RAW" > "$SEARCH_LIST" \
  || fail 'the keychain search list contains an unsafe path'
while IFS= read -r keychain; do
  case "$keychain" in /*) ;; *) fail 'the keychain search list contains a non-absolute path';; esac
done < "$SEARCH_LIST"

# Refuse an object at current unless it is the root-owned relative symlink this
# installer creates.  In particular, mv(1) follows a target symlink-to-directory
# unless -h is supplied; accepting a real directory here would make an atomic
# replacement silently turn into a move *inside* that directory.
if path_exists "$CURRENT"; then
  [ -L "$CURRENT" ] || fail 'the current service entry is not a symbolic link'
  root_wheel_owned "$CURRENT" \
    || fail 'the current service link is not owned by root:wheel'
  PREVIOUS_CURRENT_TARGET=$(/usr/bin/readlink "$CURRENT" 2>/dev/null || true)
  valid_version_target "$PREVIOUS_CURRENT_TARGET" \
    || fail 'the current service link has an unsafe target'
  PREVIOUS_CURRENT_KIND=symlink
fi

# Identity objects that are safe to replace may still be logically damaged: a
# partial pair, unreadable password, missing/ambiguous certificate, mismatched
# pin, or missing private key.  Unsafe objects (symlinks, non-root ownership, or
# group/world-readable secrets) are never moved.  Safe-but-damaged state is
# backed up inside this transaction and is restored if any later step fails.
for identity_path in "$KEYCHAIN" "$KEYCHAIN_PASSWORD" "$CERTIFICATE_RECORD"; do
  if path_exists "$identity_path"; then check_private_root_file "$identity_path"; fi
done

read_identity_certificate() {
  CERTIFICATE_INFO=$(/usr/bin/security find-certificate -a -c "$IDENTITY_LABEL" -Z "$KEYCHAIN") \
    || return 1
  CERTIFICATE_COUNT=$(printf '%s\n' "$CERTIFICATE_INFO" \
    | /usr/bin/grep -c '^SHA-1 hash:' || true)
  [ "$CERTIFICATE_COUNT" = 1 ] || return 1
  CERTIFICATE_SHA1=$(printf '%s\n' "$CERTIFICATE_INFO" \
    | /usr/bin/awk '/^SHA-1 hash:/{print tolower($3); exit}')
  valid_sha1 "$CERTIFICATE_SHA1"
}

parse_keychain_search_list() {
  input="$1"
  output="$2"
  /usr/bin/awk '
    /^[[:space:]]*$/ { next }
    /^[[:space:]]*"[^"\\]*"[[:space:]]*$/ {
      sub(/^[[:space:]]*"/, "")
      sub(/"[[:space:]]*$/, "")
      print
      next
    }
    { exit 1 }
  ' "$input" > "$output" || return 1
  while IFS= read -r keychain; do
    case "$keychain" in /*) ;; *) return 1;; esac
  done < "$output"
}

canonicalize_daemon_content() {
  canonical="$1"
  /usr/bin/codesign --remove-signature "$canonical" >/dev/null 2>&1 \
    || return 1
  /usr/bin/codesign --sign - --force --timestamp=none --options=0 \
    --pagesize=4096 --identifier com.audiohub.integrity-canonical \
    "$canonical" >/dev/null 2>&1
}

sign_as_console_user() {
  sign_input="$1"
  sign_output="$2"
  sign_mode="$3"
  [ -z "$SIGN_SESSION_ROOT" ] && [ "$CONSOLE_SEARCH_CHANGED" -eq 0 ] \
    || fail 'a previous login-user signing session was not cleaned up'
  CONSOLE_KEYCHAIN=
  CONSOLE_PAYLOAD=
  check_root_file "$sign_input"
  case "$sign_mode" in probe|sign) ;; *) fail 'the local signing mode is invalid';; esac
  if [ "$sign_mode" = sign ]; then
    case "$sign_output" in "$WORK"/*) ;; *) fail 'the signed daemon output path is invalid';; esac
    path_exists "$sign_output" && fail 'the signed daemon output already exists'
  fi

  private_root_wheel_file "$KEYCHAIN" \
    || fail 'the local signing keychain changed before signing'
  /usr/bin/security lock-keychain "$KEYCHAIN" >/dev/null 2>&1 \
    || return 1

  SIGN_SEQUENCE=$((SIGN_SEQUENCE + 1))
  private_tmp_is_safe || fail 'the macOS private temporary directory is unsafe'
  SIGN_SESSION_ROOT=$(/usr/bin/mktemp -d '/private/tmp/com.audiohub.sign.XXXXXX') \
    || return 1
  /usr/sbin/chown root:wheel "$SIGN_SESSION_ROOT" || return 1
  /bin/chmod 0711 "$SIGN_SESSION_ROOT" || return 1
  safe_root_dir "$SIGN_SESSION_ROOT" && mode_is "$SIGN_SESSION_ROOT" 0711 \
    || return 1
  SIGN_SESSION_OBJECT=$(/usr/bin/stat -f '%d:%i' "$SIGN_SESSION_ROOT" 2>/dev/null || true)
  [ -n "$SIGN_SESSION_OBJECT" ] || return 1

  CONSOLE_SESSION="$SIGN_SESSION_ROOT/session"
  /usr/bin/install -d -o root -g wheel -m 0700 "$CONSOLE_SESSION" || return 1
  /usr/sbin/chown "$CONSOLE_UID:$CONSOLE_GID" "$CONSOLE_SESSION" || return 1
  safe_console_dir "$CONSOLE_SESSION" 0700 || return 1
  CONSOLE_SESSION_OBJECT=$(/usr/bin/stat -f '%d:%i' "$CONSOLE_SESSION" 2>/dev/null || true)
  [ -n "$CONSOLE_SESSION_OBJECT" ] || return 1

  CONSOLE_KEYCHAIN="$CONSOLE_SESSION/identity.keychain-db"
  CONSOLE_PAYLOAD="$CONSOLE_SESSION/audiohubd"
  /usr/bin/install -o root -g wheel -m 0600 "$KEYCHAIN" "$CONSOLE_KEYCHAIN" \
    || return 1
  /usr/bin/install -o root -g wheel -m 0700 "$sign_input" "$CONSOLE_PAYLOAD" \
    || return 1
  /usr/sbin/chown "$CONSOLE_UID:$CONSOLE_GID" "$CONSOLE_KEYCHAIN" "$CONSOLE_PAYLOAD" \
    || return 1
  safe_console_file "$CONSOLE_KEYCHAIN" 0600 \
    && safe_console_file "$CONSOLE_PAYLOAD" 0700 \
    || return 1

  if [ "$sign_mode" = sign ]; then
    ORIGINAL_CANONICAL="$WORK/original-canonical.$SIGN_SEQUENCE"
    /usr/bin/install -o root -g wheel -m 0700 "$sign_input" "$ORIGINAL_CANONICAL" \
      || return 1
    canonicalize_daemon_content "$ORIGINAL_CANONICAL" || return 1
  fi

  CONSOLE_SEARCH_RAW="$WORK/console-search-list.$SIGN_SEQUENCE.raw"
  CONSOLE_SEARCH_LIST="$WORK/console-search-list.$SIGN_SEQUENCE"
  console_user_is_still_active || return 1
  as_console_user /usr/bin/security list-keychains -d user > "$CONSOLE_SEARCH_RAW" \
    || return 1
  parse_keychain_search_list "$CONSOLE_SEARCH_RAW" "$CONSOLE_SEARCH_LIST" \
    || return 1

  as_console_user /usr/bin/security unlock-keychain -p "$KEYCHAIN_PASS" "$CONSOLE_KEYCHAIN" \
    || return 1
  as_console_user /usr/bin/security set-key-partition-list \
    -S apple-tool:,apple:,codesign: -s -t private \
    -k "$KEYCHAIN_PASS" "$CONSOLE_KEYCHAIN" >/dev/null \
    || return 1
  CONSOLE_CERTIFICATE_INFO=$(as_console_user /usr/bin/security find-certificate \
    -a -c "$IDENTITY_LABEL" -Z "$CONSOLE_KEYCHAIN") || return 1
  CONSOLE_CERTIFICATE_COUNT=$(printf '%s\n' "$CONSOLE_CERTIFICATE_INFO" \
    | /usr/bin/grep -c '^SHA-1 hash:' || true)
  [ "$CONSOLE_CERTIFICATE_COUNT" = 1 ] || return 1
  CONSOLE_CERTIFICATE_SHA1=$(printf '%s\n' "$CONSOLE_CERTIFICATE_INFO" \
    | /usr/bin/awk '/^SHA-1 hash:/{print tolower($3); exit}')
  [ "$CONSOLE_CERTIFICATE_SHA1" = "$CERTIFICATE_SHA1" ] || return 1

  set -- "$CONSOLE_KEYCHAIN"
  while IFS= read -r keychain; do
    [ -n "$keychain" ] && [ "$keychain" != "$CONSOLE_KEYCHAIN" ] \
      && set -- "$@" "$keychain"
  done < "$CONSOLE_SEARCH_LIST"
  CONSOLE_SEARCH_CHANGED=1
  as_console_user /usr/bin/security list-keychains -d user -s "$@" \
    || return 1

  SIGN_RESULT=0
  if [ "$sign_mode" = probe ]; then
    as_console_user /usr/bin/codesign --sign "$CERTIFICATE_SHA1" \
      --dryrun --force --timestamp=none --options=0 --pagesize=4096 \
      --keychain "$CONSOLE_KEYCHAIN" \
      --identifier "$DAEMON_IDENTIFIER" "$CONSOLE_PAYLOAD" \
      || SIGN_RESULT=$?
  else
    as_console_user /usr/bin/codesign --force --sign "$CERTIFICATE_SHA1" \
      --timestamp=none --options=0 --pagesize=4096 --keychain "$CONSOLE_KEYCHAIN" \
      --identifier "$DAEMON_IDENTIFIER" "$CONSOLE_PAYLOAD" \
      || SIGN_RESULT=$?
  fi

  restore_console_search_list || SIGN_RESULT=1
  as_console_user /usr/bin/security lock-keychain "$CONSOLE_KEYCHAIN" >/dev/null 2>&1 \
    || SIGN_RESULT=1
  console_user_is_still_active || SIGN_RESULT=1
  [ "$SIGN_RESULT" -eq 0 ] || { remove_sign_session || true; return 1; }

  safe_root_dir "$SIGN_SESSION_ROOT" && mode_is "$SIGN_SESSION_ROOT" 0711 \
    && [ "$(/usr/bin/stat -f '%d:%i' "$SIGN_SESSION_ROOT" 2>/dev/null || true)" = "$SIGN_SESSION_OBJECT" ] \
    && safe_console_dir "$CONSOLE_SESSION" 0700 \
    && [ "$(/usr/bin/stat -f '%d:%i' "$CONSOLE_SESSION" 2>/dev/null || true)" = "$CONSOLE_SESSION_OBJECT" ] \
    || { remove_sign_session || true; return 1; }

  # Freeze directory entries before root snapshots the returned bytes. The
  # outer root-owned directory makes this chown target non-replaceable.
  /usr/sbin/chown root:wheel "$CONSOLE_SESSION" || return 1
  /bin/chmod 0700 "$CONSOLE_SESSION" || return 1
  safe_root_dir "$CONSOLE_SESSION" \
    && [ "$(/usr/bin/stat -f '%d:%i' "$CONSOLE_SESSION" 2>/dev/null || true)" = "$CONSOLE_SESSION_OBJECT" ] \
    && safe_console_file "$CONSOLE_KEYCHAIN" 0600 \
    && safe_console_file "$CONSOLE_PAYLOAD" 0700 \
    || return 1

  if [ "$sign_mode" = sign ]; then
    # Never verify or publish the user-writable inode itself. The login user
    # may retain a write descriptor even after chown; freeze the returned bytes
    # into a new root-only inode first and use only that snapshot from here on.
    SIGNED_SNAPSHOT="$WORK/signed-snapshot.$SIGN_SEQUENCE"
    /usr/bin/install -o root -g wheel -m 0700 "$CONSOLE_PAYLOAD" "$SIGNED_SNAPSHOT" \
      || return 1
    /bin/chmod -N "$SIGNED_SNAPSHOT" >/dev/null 2>&1 || return 1
    /usr/bin/xattr -c "$SIGNED_SNAPSHOT" >/dev/null 2>&1 || return 1
    check_root_file "$SIGNED_SNAPSHOT"
    /usr/bin/codesign --verify --strict=all \
      -R "=identifier \"$DAEMON_IDENTIFIER\" and certificate leaf = H\"$CERTIFICATE_SHA1\"" \
      "$SIGNED_SNAPSHOT" >/dev/null 2>&1 || return 1

    SIGNED_METADATA="$WORK/signed-metadata.$SIGN_SEQUENCE"
    /usr/bin/codesign -d --verbose=4 "$SIGNED_SNAPSHOT" \
      >/dev/null 2> "$SIGNED_METADATA" || return 1
    /usr/bin/grep -Eq '^CodeDirectory .* flags=0x0\(none\) ' "$SIGNED_METADATA" \
      && /usr/bin/grep -qx 'Page size=4096' "$SIGNED_METADATA" \
      && /usr/bin/grep -qx 'TeamIdentifier=not set' "$SIGNED_METADATA" \
      && /usr/bin/grep -Eq '^Internal requirements count=1 size=[0-9]+$' "$SIGNED_METADATA" \
      && ! /usr/bin/grep -Eq '^(Runtime Version|Launch Constraints?|Library Constraints?)=' "$SIGNED_METADATA" \
      || return 1

    SIGNED_ENTITLEMENTS="$WORK/signed-entitlements.$SIGN_SEQUENCE"
    /usr/bin/codesign -d --entitlements :- "$SIGNED_SNAPSHOT" \
      > "$SIGNED_ENTITLEMENTS" 2>/dev/null || return 1
    [ ! -s "$SIGNED_ENTITLEMENTS" ] || return 1

    SIGNED_CANONICAL="$WORK/signed-canonical.$SIGN_SEQUENCE"
    /usr/bin/install -o root -g wheel -m 0700 "$SIGNED_SNAPSHOT" "$SIGNED_CANONICAL" \
      || return 1
    canonicalize_daemon_content "$SIGNED_CANONICAL" || return 1
    /usr/bin/cmp -s "$ORIGINAL_CANONICAL" "$SIGNED_CANONICAL" \
      || return 1
    /usr/bin/install -o root -g wheel -m 0755 "$SIGNED_SNAPSHOT" "$sign_output" \
      || return 1
    /bin/chmod -N "$sign_output" >/dev/null 2>&1 || return 1
    /usr/bin/xattr -c "$sign_output" >/dev/null 2>&1 || return 1
    /usr/bin/codesign --verify --strict=all \
      -R "=identifier \"$DAEMON_IDENTIFIER\" and certificate leaf = H\"$CERTIFICATE_SHA1\"" \
      "$sign_output" >/dev/null 2>&1 || return 1
  fi

  remove_sign_session || return 1
  return 0
}

identity_metadata_and_unlock_are_valid() {
  check_private_root_file "$KEYCHAIN"
  check_private_root_file "$KEYCHAIN_PASSWORD"
  private_root_wheel_file "$KEYCHAIN" || return 1
  private_root_wheel_file "$KEYCHAIN_PASSWORD" || return 1
  if path_exists "$CERTIFICATE_RECORD"; then
    private_root_wheel_file "$CERTIFICATE_RECORD" || return 1
  fi
  KEYCHAIN_PASS=$(/bin/cat "$KEYCHAIN_PASSWORD") || return 1
  case "$KEYCHAIN_PASS" in ''|*[!A-F0-9-]*) return 1;; esac
  [ "${#KEYCHAIN_PASS}" -ge 36 ] && [ "${#KEYCHAIN_PASS}" -le 80 ] || return 1
  /usr/bin/security unlock-keychain -p "$KEYCHAIN_PASS" "$KEYCHAIN" >/dev/null 2>&1
}

identity_certificate_and_key_are_valid() {
  read_identity_certificate || return 1
  if path_exists "$CERTIFICATE_RECORD"; then
    RECORDED_CERTIFICATE=$(/bin/cat "$CERTIFICATE_RECORD") || return 1
    valid_sha1 "$RECORDED_CERTIFICATE" || return 1
    [ "$RECORDED_CERTIFICATE" = "$CERTIFICATE_SHA1" ] || return 1
  fi
  # codesign explicitly depends on the login user's Security context. A root
  # invocation can reject this same usable self-signed identity as "no identity
  # found" on current macOS, so exercise a private temporary copy in that user
  # session while root retains and validates the canonical identity.
  if sign_as_console_user "$SOURCE" '' probe; then
    return 0
  fi
  abort_sign_session || true
  return 1
}

backup_identity() {
  /usr/bin/install -d -o root -g wheel -m 0700 "$IDENTITY_BACKUP" \
    || fail 'could not prepare signing identity rollback'
  check_private_root_dir "$IDENTITY_BACKUP"
  IDENTITY_BACKED_UP=1
  for identity_path in "$KEYCHAIN" "$KEYCHAIN_PASSWORD" "$CERTIFICATE_RECORD"; do
    if path_exists "$identity_path"; then
      safe_root_file "$identity_path" \
        || fail 'the damaged signing identity changed before repair'
      /bin/mv -h "$identity_path" "$IDENTITY_BACKUP/${identity_path##*/}" \
        || fail 'could not preserve the damaged signing identity'
    fi
  done
}

create_identity() {
  # Arm cleanup before the first persistent write. In particular, a failed
  # create-keychain must not leave keychain.pass behind and poison every retry.
  IDENTITY_CREATED=1
  KEYCHAIN_PASS="$(/usr/bin/uuidgen)-$(/usr/bin/uuidgen)"
  P12_PASS="$(/usr/bin/uuidgen)-$(/usr/bin/uuidgen)"
  PASSWORD_STAGE="$WORK/keychain.pass"
  printf '%s\n' "$KEYCHAIN_PASS" > "$PASSWORD_STAGE"
  /usr/bin/install -o root -g wheel -m 0600 "$PASSWORD_STAGE" "$KEYCHAIN_PASSWORD" \
    || fail 'could not store the local signing keychain password'

  OPENSSL_CONFIG="$WORK/codesign.cnf"
  PRIVATE_KEY="$WORK/identity.key"
  CERTIFICATE="$WORK/identity.pem"
  PKCS12="$WORK/identity.p12"
  /bin/cat > "$OPENSSL_CONFIG" <<'OPENSSL_EOF'
[req]
prompt = no
distinguished_name = subject
x509_extensions = codesign

[subject]
CN = AudioHub Local Code Signing
O = AudioHub Local

[codesign]
keyUsage = critical,digitalSignature
extendedKeyUsage = critical,codeSigning
subjectKeyIdentifier = hash
OPENSSL_EOF

  /usr/bin/openssl req -new -x509 -newkey rsa:3072 -sha256 -days 7300 -nodes \
    -config "$OPENSSL_CONFIG" -extensions codesign \
    -keyout "$PRIVATE_KEY" -out "$CERTIFICATE" \
    || fail 'could not generate the local code-signing identity'
  /usr/bin/openssl pkcs12 -export -descert -name "$IDENTITY_LABEL" \
    -inkey "$PRIVATE_KEY" -in "$CERTIFICATE" -out "$PKCS12" \
    -passout "pass:$P12_PASS" \
    || fail 'could not package the local code-signing identity'

  # create-keychain can mutate the search list even if a later identity step
  # fails, so arm restoration before invoking it.
  SEARCH_CHANGED=1
  /usr/bin/security create-keychain -p "$KEYCHAIN_PASS" "$KEYCHAIN" \
    || fail 'could not create the local signing keychain'
  /usr/bin/security set-keychain-settings -lut 300 "$KEYCHAIN" \
    || fail 'could not configure the local signing keychain'
  /usr/bin/security unlock-keychain -p "$KEYCHAIN_PASS" "$KEYCHAIN" \
    || fail 'could not unlock the local signing keychain'
  /usr/bin/security import "$PKCS12" -k "$KEYCHAIN" -f pkcs12 -P "$P12_PASS" \
    -x -T /usr/bin/codesign \
    || fail 'could not import the local code-signing identity'
  # `openssl pkcs12 -name` assigns the friendly name to the imported
  # identity/certificate, but current macOS does not guarantee that the
  # corresponding private key has the same keychain `label`. Filtering on
  # `-l "$IDENTITY_LABEL"` therefore finds no key on macOS 26 even though the
  # import succeeded. This dedicated root-only keychain contains exactly one
  # sign-capable private key; select it by capability/type, then let the
  # certificate-count, dry-run and pinned fingerprint checks below bind it to
  # the expected identity.
  /usr/bin/security set-key-partition-list -S apple-tool:,apple:,codesign: -s \
    -t private -k "$KEYCHAIN_PASS" "$KEYCHAIN" >/dev/null \
    || fail 'could not restrict the local code-signing private key'
  /bin/chmod 0600 "$KEYCHAIN"
  /usr/sbin/chown root:wheel "$KEYCHAIN"
  private_root_wheel_file "$KEYCHAIN" \
    || fail 'the new local signing keychain has unsafe metadata'
  private_root_wheel_file "$KEYCHAIN_PASSWORD" \
    || fail 'the new local signing password has unsafe metadata'
}

IDENTITY_REUSED=0
if [ -f "$KEYCHAIN" ] && [ ! -L "$KEYCHAIN" ] \
  && [ -f "$KEYCHAIN_PASSWORD" ] && [ ! -L "$KEYCHAIN_PASSWORD" ] \
  && identity_metadata_and_unlock_are_valid; then
  if identity_certificate_and_key_are_valid; then IDENTITY_REUSED=1; fi
fi

if [ "$IDENTITY_REUSED" -eq 0 ]; then
  /usr/bin/security lock-keychain "$KEYCHAIN" >/dev/null 2>&1 || true
  if path_exists "$KEYCHAIN" || path_exists "$KEYCHAIN_PASSWORD" \
    || path_exists "$CERTIFICATE_RECORD"; then
    backup_identity
  fi
  create_identity
  # create-keychain can add the private machine keychain to this process's user
  # search domain. Root never signs from it, so restore the exact snapshot
  # before entering the login user's independent Security session.
  restore_search_list || fail 'could not restore the keychain search list after identity creation'
  SEARCH_CHANGED=0
  read_identity_certificate \
    || fail 'the new local code-signing certificate is unavailable or ambiguous'
  if ! sign_as_console_user "$SOURCE" '' probe; then
    abort_sign_session || true
    fail 'could not self-sign with the new local code-signing identity'
  fi
fi

if path_exists "$CERTIFICATE_RECORD"; then
  private_root_wheel_file "$CERTIFICATE_RECORD" \
    || fail 'the recorded local certificate fingerprint has unsafe metadata'
  RECORDED_CERTIFICATE=$(/bin/cat "$CERTIFICATE_RECORD")
  valid_sha1 "$RECORDED_CERTIFICATE" \
    || fail 'the recorded local certificate fingerprint is invalid'
  [ "$RECORDED_CERTIFICATE" = "$CERTIFICATE_SHA1" ] \
    || fail 'the local signing identity changed unexpectedly'
else
  CERTIFICATE_STAGE="$WORK/certificate.sha1"
  printf '%s\n' "$CERTIFICATE_SHA1" > "$CERTIFICATE_STAGE"
  CERTIFICATE_RECORD_CREATED=1
  /usr/bin/install -o root -g wheel -m 0600 "$CERTIFICATE_STAGE" "$CERTIFICATE_RECORD" \
    || fail 'could not pin the local signing identity'
  private_root_wheel_file "$CERTIFICATE_RECORD" \
    || fail 'the new local certificate record has unsafe metadata'
fi

version_state_is_valid() {
  version="$1"
  daemon="$version/audiohubd"
  source_record="$version/source.sha256"
  signed_record="$version/signed.sha256"
  certificate_record="$version/certificate.sha1"

  safe_root_dir "$version" && root_wheel_owned "$version" && mode_is "$version" 0755 \
    || return 1
  safe_root_file "$daemon" && root_wheel_owned "$daemon" && mode_is "$daemon" 0555 \
    || return 1
  for record in "$source_record" "$signed_record" "$certificate_record"; do
    safe_root_file "$record" && root_wheel_owned "$record" && mode_is "$record" 0444 \
      || return 1
  done

  STORED_SOURCE_SHA=$(/bin/cat "$source_record") || return 1
  STORED_SIGNED_SHA=$(/bin/cat "$signed_record") || return 1
  STORED_CERTIFICATE_SHA1=$(/bin/cat "$certificate_record") || return 1
  valid_sha256 "$STORED_SOURCE_SHA" || return 1
  valid_sha256 "$STORED_SIGNED_SHA" || return 1
  valid_sha1 "$STORED_CERTIFICATE_SHA1" || return 1
  [ "$STORED_SOURCE_SHA" = "$SOURCE_SHA" ] || return 1
  [ "$STORED_CERTIFICATE_SHA1" = "$CERTIFICATE_SHA1" ] || return 1
  ACTUAL_SIGNED_SHA=$(/usr/bin/shasum -a 256 "$daemon" | /usr/bin/awk '{print $1}') \
    || return 1
  [ "$STORED_SIGNED_SHA" = "$ACTUAL_SIGNED_SHA" ] || return 1
  /usr/bin/codesign --verify --strict \
    -R "=identifier \"$DAEMON_IDENTIFIER\" and certificate leaf = H\"$CERTIFICATE_SHA1\"" \
    "$daemon" >/dev/null 2>&1
}

FINAL_VALID=0
EXISTING="$FINAL/audiohubd"
if path_exists "$FINAL"; then
  [ -d "$FINAL" ] && [ ! -L "$FINAL" ] \
    || fail 'the versioned daemon path is not a real directory'
  check_root_dir "$FINAL"
  for entry in "$EXISTING" "$FINAL/source.sha256" "$FINAL/signed.sha256" "$FINAL/certificate.sha1"; do
    if path_exists "$entry"; then check_root_file "$entry"; fi
  done

  if version_state_is_valid "$FINAL"; then FINAL_VALID=1; fi
fi

if [ "$FINAL_VALID" -eq 1 ]; then
  : # Identity usability and every persisted byte were verified above.
else
  /usr/bin/install -d -o root -g wheel -m 0755 "$PAYLOAD"
  /usr/bin/install -o root -g wheel -m 0755 "$SOURCE" "$PAYLOAD/audiohubd"
  # /Applications is intentionally writable by the admin group on macOS.
  # Bind this copy to the exact root-owned App and payload objects verified at
  # the start, then verify the copied signature before replacing it locally.
  [ "$(/usr/bin/stat -f '%d:%i' "$APP" 2>/dev/null || true)" = "$APP_OBJECT_ID" ] \
    && [ "$(/usr/bin/stat -f '%d:%i' "$SOURCE" 2>/dev/null || true)" = "$SOURCE_OBJECT_ID" ] \
    || fail 'the installed App changed while daemon setup was running'
  check_root_dir "$APP"
  check_root_dir "$APP/Contents"
  check_root_dir "$APP/Contents/MacOS"
  check_root_file "$SOURCE"
  COPIED_SHA=$(/usr/bin/shasum -a 256 "$PAYLOAD/audiohubd" | /usr/bin/awk '{print $1}')
  [ "$COPIED_SHA" = "$SOURCE_SHA" ] || fail 'the staged daemon differs from the App payload'
  COPIED_SOURCE_ID=$(/usr/bin/codesign -d --verbose=4 "$PAYLOAD/audiohubd" 2>&1 \
    | /usr/bin/sed -n 's/^Identifier=//p' | /usr/bin/head -n 1)
  [ "$COPIED_SOURCE_ID" = "$DAEMON_IDENTIFIER" ] \
    && /usr/bin/codesign --verify --strict "$PAYLOAD/audiohubd" \
    || fail 'the staged App payload has an invalid original signature'

  SIGNED_PAYLOAD="$WORK/signed-payload"
  if ! sign_as_console_user "$PAYLOAD/audiohubd" "$SIGNED_PAYLOAD" sign; then
    abort_sign_session || true
    fail 'could not self-sign the AudioHub daemon'
  fi
  /bin/mv -f "$SIGNED_PAYLOAD" "$PAYLOAD/audiohubd" \
    || fail 'could not activate the self-signed AudioHub daemon staging copy'
  /usr/bin/codesign --verify --strict \
    -R "=identifier \"$DAEMON_IDENTIFIER\" and certificate leaf = H\"$CERTIFICATE_SHA1\"" \
    "$PAYLOAD/audiohubd" \
    || fail 'the self-signed AudioHub daemon failed verification'
  SIGNED_SHA=$(/usr/bin/shasum -a 256 "$PAYLOAD/audiohubd" | /usr/bin/awk '{print $1}')
  valid_sha256 "$SIGNED_SHA" || fail 'the signed daemon hash is invalid'
  printf '%s\n' "$SOURCE_SHA" > "$PAYLOAD/source.sha256"
  printf '%s\n' "$SIGNED_SHA" > "$PAYLOAD/signed.sha256"
  printf '%s\n' "$CERTIFICATE_SHA1" > "$PAYLOAD/certificate.sha1"
  /usr/sbin/chown -R root:wheel "$PAYLOAD"
  /bin/chmod 0555 "$PAYLOAD/audiohubd"
  /bin/chmod 0444 "$PAYLOAD/source.sha256" "$PAYLOAD/signed.sha256" "$PAYLOAD/certificate.sha1"
  version_state_is_valid "$PAYLOAD" \
    || fail 'the staged daemon failed metadata, signature, or hash verification'

  if path_exists "$FINAL"; then
    FINAL_BACKED_UP=1
    /bin/mv -h "$FINAL" "$FINAL_BACKUP" \
      || fail 'could not preserve the previous daemon version'
  fi
  FINAL_INSTALLED=1
  if /bin/mv "$PAYLOAD" "$FINAL"; then
    :
  else
    fail 'could not activate the staged AudioHub daemon'
  fi
  version_state_is_valid "$FINAL" \
    || fail 'the installed versioned daemon failed metadata, signature, or hash verification'
fi

# Build the replacement link on the same filesystem, then use rename(2) via
# mv -h.  -h is essential because current itself points to a directory.
/bin/ln -s "versions/$SOURCE_SHA" "$CURRENT_TMP"
/usr/sbin/chown -h root:wheel "$CURRENT_TMP"
/bin/chmod -h 0755 "$CURRENT_TMP"
[ -L "$CURRENT_TMP" ] \
  && [ "$(/usr/bin/readlink "$CURRENT_TMP" 2>/dev/null || true)" = "versions/$SOURCE_SHA" ] \
  && mode_is "$CURRENT_TMP" 0755 \
  || fail 'could not prepare the current service link'
CURRENT_SWITCHED=1
/bin/mv -f -h "$CURRENT_TMP" "$CURRENT" \
  || fail 'could not switch to the installed AudioHub daemon'
[ -L "$CURRENT" ] \
  && [ "$(/usr/bin/readlink "$CURRENT" 2>/dev/null || true)" = "versions/$SOURCE_SHA" ] \
  && root_wheel_owned "$CURRENT" \
  && mode_is "$CURRENT" 0755 \
  || fail 'the current service link failed final verification'

version_state_is_valid "$FINAL" \
  || fail 'the installed AudioHub daemon state changed during activation'
/usr/bin/codesign --verify --strict \
  -R "=identifier \"$DAEMON_IDENTIFIER\" and certificate leaf = H\"$CERTIFICATE_SHA1\"" \
  "$BASE/current/audiohubd" \
  || fail 'the installed AudioHub daemon failed final verification'
FINAL_RECORDED_SHA=$(/bin/cat "$BASE/current/signed.sha256")
valid_sha256 "$FINAL_RECORDED_SHA" \
  && [ "$(/usr/bin/shasum -a 256 "$BASE/current/audiohubd" | /usr/bin/awk '{print $1}')" = "$FINAL_RECORDED_SHA" ] \
  || fail 'the installed AudioHub daemon failed final hash verification'

# Search-list restoration and relocking are part of success, not best-effort
# cleanup. Only write the root search list when this transaction actually
# changed it; an unconditional restore could overwrite an unrelated concurrent
# update even though the existing-identity path never touched the list.
if [ "$SEARCH_CHANGED" -eq 1 ]; then
  restore_search_list || fail 'could not restore the keychain search list'
fi
/usr/bin/security lock-keychain "$KEYCHAIN" \
  || fail 'could not relock the local signing keychain'
SEARCH_CHANGED=0

exit 0
