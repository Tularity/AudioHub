use scripting additions

on run
	set localeID to user locale of (system info)
	set chineseUI to localeID starts with "zh"

	if chineseUI then
		set dialogTitle to "卸载 AudioHub"
		set dialogText to "将移除 AudioHub App、后台服务、登录启动项与虚拟音频驱动。系统音频会短暂中断。\n\n设置、设备配对、身份数据与日志会保留。"
		set cancelText to "取消"
		set uninstallText to "卸载 AudioHub"
		set successText to "AudioHub 已卸载。用户配置和配对数据仍保留。"
		set cancelledText to "系统鉴权已取消，未卸载 AudioHub。"
		set failurePrefix to "无法完成卸载："
	else
		set dialogTitle to "Uninstall AudioHub"
		set dialogText to "This removes the AudioHub app, background service, login item, and virtual audio driver. System audio is briefly interrupted.\n\nSettings, pairing, identity, and logs are kept."
		set cancelText to "Cancel"
		set uninstallText to "Uninstall AudioHub"
		set successText to "AudioHub was uninstalled. Your settings and pairing data were kept."
		set cancelledText to "System authorization was cancelled. AudioHub was not uninstalled."
		set failurePrefix to "AudioHub could not be uninstalled:"
	end if

	try
		display dialog dialogText with title dialogTitle buttons {cancelText, uninstallText} default button uninstallText cancel button cancelText with icon caution
	on error number -128
		return
	end try

	set rootCommand to "set -eu
APP='/Applications/AudioHub.app'
CURRENT_LABEL='com.audiohub.app.autostart'
RETIRED_LABEL='com.audiohub.daemon'
RETIRED_SYSTEM_PLIST='/Library/LaunchDaemons/com.audiohub.daemon.plist'
DRIVER='/Library/Audio/Plug-Ins/HAL/AudioHubDriver.driver'
PRODUCT_SUPPORT='/Library/Application Support/AudioHub'
SERVICE='/Library/Application Support/AudioHub/service'
SERVICE_VERSIONS=\"$SERVICE/versions\"
SERVICE_CURRENT='/Library/Application Support/AudioHub/service/current/audiohubd'
SIGNING='/Library/Application Support/AudioHub/signing'
KEYCHAIN=\"$SIGNING/identity.keychain-db\"
KEYCHAIN_PASSWORD=\"$SIGNING/keychain.pass\"
CERTIFICATE_RECORD=\"$SIGNING/certificate.sha1\"
LIFECYCLE_LOCK='/var/run/com.audiohub.lifecycle.lock'

path_exists() {
  [ -e \"$1\" ] || [ -L \"$1\" ]
}
lifecycle_fail() {
  echo \"AudioHub lifecycle lock error: $*\" >&2
  exit 35
}
lifecycle_lock_is_safe() {
  path=\"$1\"
  [ -f \"$path\" ] && [ ! -L \"$path\" ] || return 1
  [ \"$(/usr/bin/stat -f '%u' \"$path\" 2>/dev/null || true)\" = 0 ] || return 1
  [ \"$(/usr/bin/stat -f '%g' \"$path\" 2>/dev/null || true)\" = 0 ] || return 1
  mode=$(/usr/bin/stat -f '%Lp' \"$path\" 2>/dev/null || true)
  case \"$mode\" in ''|*[!0-7]*) return 1;; esac
  [ $((0$mode)) -eq $((0600)) ]
}
acquire_lifecycle_lock() {
  if ! path_exists \"$LIFECYCLE_LOCK\"; then
    candidate=$(/usr/bin/mktemp \"$LIFECYCLE_LOCK.candidate.XXXXXX\") || lifecycle_fail 'could not prepare the lock'
    /usr/sbin/chown root:wheel \"$candidate\" || { /bin/rm -f \"$candidate\"; lifecycle_fail 'could not own the lock'; }
    /bin/chmod 0600 \"$candidate\" || { /bin/rm -f \"$candidate\"; lifecycle_fail 'could not protect the lock'; }
    lifecycle_lock_is_safe \"$candidate\" || { /bin/rm -f \"$candidate\"; lifecycle_fail 'the new lock is unsafe'; }
    if ! /bin/ln \"$candidate\" \"$LIFECYCLE_LOCK\" 2>/dev/null && ! path_exists \"$LIFECYCLE_LOCK\"; then
      /bin/rm -f \"$candidate\"
      lifecycle_fail 'could not publish the lock'
    fi
    /bin/rm -f \"$candidate\" || lifecycle_fail 'could not finish preparing the lock'
  fi
  lifecycle_lock_is_safe \"$LIFECYCLE_LOCK\" || lifecycle_fail 'the lock is a symlink or has unsafe metadata'
  lock_object_id=$(/usr/bin/stat -f '%i' \"$LIFECYCLE_LOCK\" 2>/dev/null || true)
  [ -n \"$lock_object_id\" ] || lifecycle_fail 'could not identify the lock'
  exec 9<> \"$LIFECYCLE_LOCK\" || lifecycle_fail 'could not open the lock'
  lock_fd_object_id=$(/usr/bin/stat -f '%i' /dev/fd/9 2>/dev/null || true)
  [ \"$lock_fd_object_id\" = \"$lock_object_id\" ] || lifecycle_fail 'the lock changed while opening it'
  /usr/bin/lockf -s -t 0 9 || lifecycle_fail 'another AudioHub install or uninstall operation is already running'
  lifecycle_lock_is_safe \"$LIFECYCLE_LOCK\" || lifecycle_fail 'the lock changed after it was acquired'
  [ \"$(/usr/bin/stat -f '%i' \"$LIFECYCLE_LOCK\" 2>/dev/null || true)\" = \"$lock_object_id\" ] || lifecycle_fail 'the lock changed after it was acquired'
}

# The fixed lock is outside PRODUCT_SUPPORT and remains open until receipt
# cleanup finishes. No daemon, driver, App, launch item, or keychain mutation
# happens before it is acquired.
acquire_lifecycle_lock

# A symlink is damaged product state, not a successful uninstall candidate:
# unlinking only it could leave a live target behind while reporting success.
if [ -L \"$APP\" ]; then
  echo 'Refusing to uninstall a symlink at /Applications/AudioHub.app' >&2
  exit 30
elif [ -e \"$APP\" ]; then
  APP_ID=$(/usr/bin/plutil -extract CFBundleIdentifier raw -o - \"$APP/Contents/Info.plist\" 2>/dev/null || true)
  [ \"$APP_ID\" = 'com.audiohub.app' ] || { echo 'Refusing to remove an unrecognized /Applications/AudioHub.app' >&2; exit 30; }
fi
if [ -L \"$DRIVER\" ]; then
  echo 'Refusing to uninstall a symlink at AudioHubDriver.driver' >&2
  exit 31
elif [ -e \"$DRIVER\" ]; then
  DRIVER_ID=$(/usr/bin/plutil -extract CFBundleIdentifier raw -o - \"$DRIVER/Contents/Info.plist\" 2>/dev/null || true)
  [ \"$DRIVER_ID\" = 'com.audiohub.driver' ] || { echo 'Refusing to remove an unrecognized AudioHubDriver.driver' >&2; exit 31; }
fi

# Verify every component rather than trusting a /Users glob. The physical-path
# equality check also rejects stable symlinks; the lexical checks reject path
# traversal before any privileged removal is attempted.
directory_chain_is_real() {
  candidate=\"$1\"
  case \"$candidate\" in /*) ;; *) return 1;; esac
  case \"$candidate\" in /|*//*|*/./*|*/.|*/../*|*/..) return 1;; esac
  remaining=${candidate#/}
  current=
  while [ -n \"$remaining\" ]; do
    case \"$remaining\" in
      */*) component=${remaining%%/*}; remaining=${remaining#*/} ;;
      *) component=$remaining; remaining= ;;
    esac
    [ -n \"$component\" ] || return 1
    current=\"$current/$component\"
    [ -d \"$current\" ] && [ ! -L \"$current\" ] || return 1
  done
  resolved=$(/bin/sh -c 'cd -P \"$1\" && /bin/pwd -P' sh \"$candidate\" 2>/dev/null || true)
  [ \"$resolved\" = \"$candidate\" ] || return 1
  # Confirm the same inode through a directory descriptor after the pathname
  # walk. Removal later repeats this identity check from its opened parent.
  path_identity=$(/usr/bin/stat -f '%d:%i' \"$candidate\" 2>/dev/null || true)
  cwd_identity=$(/bin/sh -c 'cd -P \"$1\" && /usr/bin/stat -f %d:%i .' sh \"$candidate\" 2>/dev/null || true)
  [ -n \"$path_identity\" ] && [ \"$path_identity\" = \"$cwd_identity\" ]
}

check_root_product_dir() {
  path=\"$1\"
  directory_chain_is_real \"$path\" || { echo \"AudioHub machine directory has an unsafe path: $path\" >&2; return 1; }
  [ -d \"$path\" ] && [ ! -L \"$path\" ] || { echo \"Unsafe AudioHub machine directory: $path\" >&2; return 1; }
  [ \"$(/usr/bin/stat -f '%u' \"$path\" 2>/dev/null || true)\" = 0 ] || { echo \"AudioHub machine directory is not owned by root: $path\" >&2; return 1; }
  mode=$(/usr/bin/stat -f '%Lp' \"$path\" 2>/dev/null || true)
  case \"$mode\" in ''|*[!0-7]*) echo \"AudioHub machine directory has an invalid mode: $path\" >&2; return 1;; esac
  [ $((0$mode & 022)) -eq 0 ] || { echo \"AudioHub machine directory is writable by another user: $path\" >&2; return 1; }
}

check_private_signing_file() {
  path=\"$1\"
  [ -f \"$path\" ] && [ ! -L \"$path\" ] || { echo \"Unsafe AudioHub signing file: $path\" >&2; return 1; }
  [ \"$(/usr/bin/stat -f '%u' \"$path\" 2>/dev/null || true)\" = 0 ] || { echo \"AudioHub signing file is not owned by root:wheel: $path\" >&2; return 1; }
  [ \"$(/usr/bin/stat -f '%g' \"$path\" 2>/dev/null || true)\" = 0 ] || { echo \"AudioHub signing file is not owned by root:wheel: $path\" >&2; return 1; }
  mode=$(/usr/bin/stat -f '%Lp' \"$path\" 2>/dev/null || true)
  case \"$mode\" in ''|*[!0-7]*) echo \"AudioHub signing file has an invalid mode: $path\" >&2; return 1;; esac
  [ $((0$mode)) -eq $((0600)) ] || { echo \"AudioHub signing file is not private: $path\" >&2; return 1; }
}

# Validate the complete machine tree before bootout, process signalling, or
# driver removal. Damaged or linked state must fail without a half-uninstall.
if path_exists \"$PRODUCT_SUPPORT\"; then
  check_root_product_dir \"$PRODUCT_SUPPORT\" || exit 33
  if path_exists \"$SERVICE\"; then check_root_product_dir \"$SERVICE\" || exit 33; fi
  if path_exists \"$SIGNING\"; then check_root_product_dir \"$SIGNING\" || exit 34; fi
fi
for signing_file in \"$KEYCHAIN\" \"$KEYCHAIN_PASSWORD\" \"$CERTIFICATE_RECORD\"; do
  if path_exists \"$signing_file\"; then check_private_signing_file \"$signing_file\" || exit 34; fi
done

trusted_user_home() {
  case \"$USER_HOME\" in
    /Users/?*|/Network/Users/?*|/Network/Servers/?*) ;;
    *) return 1 ;;
  esac
  directory_chain_is_real \"$USER_HOME\" || return 1
  home_owner=$(/usr/bin/stat -f '%u' \"$USER_HOME\" 2>/dev/null || true)
  [ \"$home_owner\" = \"$USER_UID\" ]
}

# The normal path runs as the owning user. The root fallback is only for exact
# lifecycle filenames left by a retired installer. It enters the already
# verified parent directory, compares directory identity through both handles,
# and removes a fixed basename relative to that open working directory. Thus a
# user cannot redirect root through a raced parent symlink.
safe_root_unlink() {
  target=\"$1\"
  case \"$target\" in
    \"$CURRENT_AGENT\") parent=\"$USER_HOME/Library/LaunchAgents\"; leaf=\"$CURRENT_LABEL.plist\" ;;
    \"$RETIRED_AGENT\") parent=\"$USER_HOME/Library/LaunchAgents\"; leaf=\"$RETIRED_LABEL.plist\" ;;
    \"$CONFIG_DIR/service-installed-v1\") parent=\"$CONFIG_DIR\"; leaf='service-installed-v1' ;;
    \"$CONFIG_DIR/ipc.json\") parent=\"$CONFIG_DIR\"; leaf='ipc.json' ;;
    *) return 1 ;;
  esac
  directory_chain_is_real \"$parent\" || return 1
  (
    cd -P \"$parent\" || exit 1
    [ \"$(/bin/pwd -P)\" = \"$parent\" ] || exit 1
    cwd_identity=$(/usr/bin/stat -f '%d:%i' . 2>/dev/null || true)
    path_identity=$(/usr/bin/stat -f '%d:%i' \"$parent\" 2>/dev/null || true)
    [ -n \"$cwd_identity\" ] && [ \"$cwd_identity\" = \"$path_identity\" ] || exit 1
    /bin/rm -f \"./$leaf\"
  )
}

remove_user_lifecycle() {
  CURRENT_AGENT=\"$USER_HOME/Library/LaunchAgents/$CURRENT_LABEL.plist\"
  RETIRED_AGENT=\"$USER_HOME/Library/LaunchAgents/$RETIRED_LABEL.plist\"
  CONFIG_DIR=\"$USER_HOME/Library/Application Support/AudioHub\"

  # Identity, paired_peers.json, settings.json, AirPlay identity, and logs are
  # configuration, not lifecycle state, and deliberately remain untouched.
  /usr/bin/sudo -H -u \"$USER_NAME\" /bin/rm -f \"$CURRENT_AGENT\" \"$RETIRED_AGENT\" \"$CONFIG_DIR/service-installed-v1\" \"$CONFIG_DIR/ipc.json\" 2>/dev/null || true
  for lifecycle_path in \"$CURRENT_AGENT\" \"$RETIRED_AGENT\" \"$CONFIG_DIR/service-installed-v1\" \"$CONFIG_DIR/ipc.json\"; do
    if [ -e \"$lifecycle_path\" ] || [ -L \"$lifecycle_path\" ]; then
      safe_root_unlink \"$lifecycle_path\" || { echo \"Could not safely remove AudioHub lifecycle state: $lifecycle_path\" >&2; return 1; }
    fi
    if [ -e \"$lifecycle_path\" ] || [ -L \"$lifecycle_path\" ]; then
      echo \"AudioHub lifecycle state is still present: $lifecycle_path\" >&2
      return 1
    fi
  done
}

user_lifecycle_is_visible() {
  for lifecycle_path in \"$USER_HOME/Library/LaunchAgents/$CURRENT_LABEL.plist\" \"$USER_HOME/Library/LaunchAgents/$RETIRED_LABEL.plist\" \"$USER_HOME/Library/Application Support/AudioHub/service-installed-v1\" \"$USER_HOME/Library/Application Support/AudioHub/ipc.json\"; do
    if [ -e \"$lifecycle_path\" ] || [ -L \"$lifecycle_path\" ]; then return 0; fi
  done
  return 1
}

# /Search covers the configured local and network directory-service nodes. UIDs
# below 500 and underscore-prefixed/root-style records are macOS system users.
USER_LIST=$(/usr/bin/dscl /Search -list /Users UniqueID 2>/dev/null) || { echo 'Could not enumerate AudioHub user accounts' >&2; exit 20; }
USER_UIDS=$(printf '%s\n' \"$USER_LIST\" | /usr/bin/awk 'NF >= 2 { uid=$NF; if (uid ~ /^[0-9]+$/ && uid >= 500) print uid }' | /usr/bin/sort -nu)
cleanup_failed=0
for USER_UID in $USER_UIDS; do
  case \"$USER_UID\" in ''|*[!0-9]*) continue;; esac
  USER_NAME=$(/usr/bin/id -nu \"$USER_UID\" 2>/dev/null || true)
  case \"$USER_NAME\" in ''|root|daemon|nobody|_*) continue;; esac
  [ \"$(/usr/bin/id -u \"$USER_NAME\" 2>/dev/null || true)\" = \"$USER_UID\" ] || continue

  # Stop both known labels for every GUI domain even when a network home is
  # currently offline. bootout is harmless when that user is not logged in.
  /bin/launchctl bootout \"gui/$USER_UID/$CURRENT_LABEL\" 2>/dev/null || true
  /bin/launchctl bootout \"gui/$USER_UID/$RETIRED_LABEL\" 2>/dev/null || true

  USER_RECORD=$(/usr/bin/dscl /Search -read \"/Users/$USER_NAME\" UniqueID NFSHomeDirectory 2>/dev/null || true)
  RECORD_UID=$(printf '%s\n' \"$USER_RECORD\" | /usr/bin/sed -n 's/^UniqueID: //p' | /usr/bin/head -n 1)
  USER_HOME=$(printf '%s\n' \"$USER_RECORD\" | /usr/bin/sed -n 's/^NFSHomeDirectory: //p' | /usr/bin/head -n 1)
  [ \"$RECORD_UID\" = \"$USER_UID\" ] || continue
  if trusted_user_home; then
    remove_user_lifecycle || cleanup_failed=1
  elif user_lifecycle_is_visible; then
    echo 'Could not validate an AudioHub user home; lifecycle state was not removed' >&2
    cleanup_failed=1
  fi
done

# With every launch trigger disabled, stop every remaining process executing
# from either the App or an exact immutable service version. A version path is
# accepted only as service/versions/<64 lowercase hex>/audiohubd; a same-named
# executable elsewhere is never touched. Re-check each PID immediately before
# signalling to avoid PID-reuse races.
is_lower_hex_hash() {
  value=\"$1\"
  [ \"${#value}\" -eq 64 ] || return 1
  case \"$value\" in *[!0-9a-f]*) return 1;; esac
  return 0
}
is_service_daemon_image() {
  image=\"$1\"
  [ \"$image\" = \"$SERVICE_CURRENT\" ] && return 0
  case \"$image\" in
    \"$SERVICE_VERSIONS\"/*/audiohubd)
      suffix=${image#\"$SERVICE_VERSIONS\"/}
      hash=${suffix%/audiohubd}
      [ \"$suffix\" = \"$hash/audiohubd\" ] && is_lower_hex_hash \"$hash\"
      ;;
    *) return 1 ;;
  esac
}
is_product_image() {
  image=\"$1\"
  case \"$image\" in
    \"$APP/Contents/MacOS/audiohub-app\"|\"$APP/Contents/MacOS/audiohubd\"|\"$APP/Contents/MacOS/audiohub\") return 0 ;;
  esac
  is_service_daemon_image \"$image\"
}
product_pids() {
  # read assigns the remaining fields to image, preserving the space in
  # /Library/Application Support unlike a field-two-only awk match.
  /bin/ps -axo pid=,comm= 2>/dev/null | while read -r pid image; do
    case \"$pid\" in ''|*[!0-9]*) continue;; esac
    is_product_image \"$image\" && printf '%s\n' \"$pid\"
  done
}
signal_product_images() {
  signal=\"$1\"
  for pid in $(product_pids); do
    image=$(/bin/ps -p \"$pid\" -o comm= 2>/dev/null | /usr/bin/sed 's/^[[:space:]]*//;s/[[:space:]]*$//' || true)
    if is_product_image \"$image\"; then
      /bin/kill \"-$signal\" \"$pid\" 2>/dev/null || true
    fi
  done
}
signal_product_images TERM
attempt=0
while [ \"$attempt\" -lt 20 ] && [ -n \"$(product_pids)\" ]; do
  /bin/sleep 0.25
  attempt=$((attempt + 1))
done
signal_product_images KILL

attempt=0
while [ \"$attempt\" -lt 20 ] && [ -n \"$(product_pids)\" ]; do
  /bin/sleep 0.25
  attempt=$((attempt + 1))
done
[ -z \"$(product_pids)\" ] || { echo 'Could not stop every AudioHub product process' >&2; exit 26; }

[ \"$cleanup_failed\" -eq 0 ] || { echo 'Could not remove AudioHub lifecycle state for every trusted user' >&2; exit 24; }

# Retired builds briefly used a system LaunchDaemon. It is not installed by the
# current product, but leaving it behind would leave a headless daemon running.
if /bin/launchctl print \"system/$RETIRED_LABEL\" >/dev/null 2>&1; then
  /bin/launchctl bootout \"system/$RETIRED_LABEL\" 2>/dev/null || { echo 'Could not unload the retired AudioHub system daemon' >&2; exit 27; }
  ! /bin/launchctl print \"system/$RETIRED_LABEL\" >/dev/null 2>&1 || { echo 'The retired AudioHub system daemon is still loaded' >&2; exit 27; }
fi
/bin/rm -f \"$RETIRED_SYSTEM_PLIST\"

# The virtual device has no useful standalone lifecycle: it relies on the
# AudioHub service. A standard uninstall therefore removes it as one product
# instead of leaving a dead audio device in System Settings.
DRIVER_REMOVED=0
if path_exists \"$DRIVER\"; then
  [ ! -L \"$DRIVER\" ] || { echo 'AudioHubDriver.driver became a symlink during uninstall' >&2; exit 31; }
  DRIVER_ID=$(/usr/bin/plutil -extract CFBundleIdentifier raw -o - \"$DRIVER/Contents/Info.plist\" 2>/dev/null || true)
  [ \"$DRIVER_ID\" = 'com.audiohub.driver' ] || { echo 'Refusing to remove an unrecognized AudioHubDriver.driver' >&2; exit 31; }
  /bin/rm -rf \"$DRIVER\"
  DRIVER_REMOVED=1
fi
if [ \"$DRIVER_REMOVED\" -eq 1 ]; then
  HELPER='/System/Library/Frameworks/CoreAudio.framework/Versions/A/XPCServices/com.apple.audio.Core-Audio-Driver-Service.helper.xpc/Contents/MacOS/com.apple.audio.Core-Audio-Driver-Service.helper'
  AUDIOHUB_HELPER_NAME='Core Audio Driver (AudioHubDriver.driver)'
  COREAUDIOD='/usr/sbin/coreaudiod'
  is_audiohub_hal_helper_image() {
    image=\"$1\"
    [ \"$image\" = \"$HELPER\" ] || [ \"$image\" = \"$AUDIOHUB_HELPER_NAME\" ]
  }
  audiohub_helper_pids() {
    # read assigns all remaining fields to image, preserving the spaces and
    # parentheses in the AudioHub-specific process name.
    /bin/ps -axo pid=,comm= 2>/dev/null | while read -r pid image; do
      case \"$pid\" in ''|*[!0-9]*) continue;; esac
      if is_audiohub_hal_helper_image \"$image\"; then
        printf '%s\\n' \"$pid\"
      fi
    done
  }
  OLD_HELPERS=$(audiohub_helper_pids)
  OLD_COREAUDIOD=$(/bin/ps -axo pid=,comm= | /usr/bin/awk -v daemon=\"$COREAUDIOD\" '$2 == daemon { print $1 }')
  for helper_pid in $OLD_HELPERS; do
    current=$(/bin/ps -p \"$helper_pid\" -o comm= 2>/dev/null | /usr/bin/sed 's/^[[:space:]]*//;s/[[:space:]]*$//' || true)
    if is_audiohub_hal_helper_image \"$current\"; then
      /bin/kill -TERM \"$helper_pid\" 2>/dev/null || true
    fi
  done
  attempt=0
  while [ \"$attempt\" -lt 20 ]; do
    old_alive=0
    for helper_pid in $OLD_HELPERS; do
      current=$(/bin/ps -p \"$helper_pid\" -o comm= 2>/dev/null | /usr/bin/sed 's/^[[:space:]]*//;s/[[:space:]]*$//' || true)
      is_audiohub_hal_helper_image \"$current\" && old_alive=1
    done
    [ \"$old_alive\" -eq 0 ] && break
    /bin/sleep 0.1
    attempt=$((attempt + 1))
  done
  for helper_pid in $OLD_HELPERS; do
    current=$(/bin/ps -p \"$helper_pid\" -o comm= 2>/dev/null | /usr/bin/sed 's/^[[:space:]]*//;s/[[:space:]]*$//' || true)
    if is_audiohub_hal_helper_image \"$current\"; then
      /bin/kill -KILL \"$helper_pid\" 2>/dev/null || true
    fi
  done
  if ! /bin/launchctl kickstart -kp system/com.apple.audio.coreaudiod 2>/dev/null; then
    for core_pid in $OLD_COREAUDIOD; do
      current=$(/bin/ps -p \"$core_pid\" -o comm= 2>/dev/null | /usr/bin/xargs || true)
      [ \"$current\" != \"$COREAUDIOD\" ] || /bin/kill -TERM \"$core_pid\" 2>/dev/null || true
    done
    /bin/sleep 0.2
    for core_pid in $OLD_COREAUDIOD; do
      current=$(/bin/ps -p \"$core_pid\" -o comm= 2>/dev/null | /usr/bin/xargs || true)
      [ \"$current\" != \"$COREAUDIOD\" ] || /bin/kill -KILL \"$core_pid\" 2>/dev/null || true
    done
  fi
  attempt=0
  while [ \"$attempt\" -lt 20 ]; do
    /bin/launchctl print system/com.apple.audio.coreaudiod 2>/dev/null |
      /usr/bin/grep -q 'state = running' && break
    /bin/sleep 0.25
    attempt=$((attempt + 1))
  done
  /bin/launchctl print system/com.apple.audio.coreaudiod 2>/dev/null |
    /usr/bin/grep -q 'state = running' || { echo 'Driver removed, but Core Audio could not be restarted' >&2; exit 32; }
fi

# Remove the machine-level daemon payload and its dedicated local signing
# identity only after no accepted daemon image remains and driver teardown has
# completed. If Core Audio recovery failed above, the still-installed App keeps
# its service and signing identity and can be repaired or uninstalled again.
# User configuration lives under each home directory and remains out of scope.
if path_exists \"$PRODUCT_SUPPORT\"; then
  # Repeat the preflight immediately before recursive removal. The shared lock
  # excludes AudioHub operations; a privileged out-of-band replacement fails
  # closed instead of being followed.
  check_root_product_dir \"$PRODUCT_SUPPORT\" || exit 33

  if path_exists \"$SERVICE\"; then
    check_root_product_dir \"$SERVICE\" || exit 33
    /bin/rm -rf \"$SERVICE\"
  fi

  if path_exists \"$SIGNING\"; then
    check_root_product_dir \"$SIGNING\" || exit 34
    if path_exists \"$KEYCHAIN\"; then
      check_private_signing_file \"$KEYCHAIN\" || exit 34
      /usr/bin/security lock-keychain \"$KEYCHAIN\" >/dev/null 2>&1 || true
      /usr/bin/security delete-keychain \"$KEYCHAIN\" >/dev/null 2>&1 || /bin/rm -f \"$KEYCHAIN\"
    fi
    if path_exists \"$KEYCHAIN_PASSWORD\"; then
      check_private_signing_file \"$KEYCHAIN_PASSWORD\" || exit 34
      /bin/rm -f \"$KEYCHAIN_PASSWORD\"
    fi
    /bin/rm -rf \"$SIGNING\"
  fi

  # Remove only an empty product support directory. Future machine-level state
  # unknown to this uninstaller is retained rather than recursively erased.
  /bin/rmdir \"$PRODUCT_SUPPORT\" 2>/dev/null || true
fi

if [ -e \"$APP\" ]; then
  [ ! -L \"$APP\" ] || { echo 'AudioHub.app became a symlink during uninstall' >&2; exit 30; }
  # Re-check immediately before the recursive removal as a defense in depth.
  APP_ID=$(/usr/bin/plutil -extract CFBundleIdentifier raw -o - \"$APP/Contents/Info.plist\" 2>/dev/null || true)
  [ \"$APP_ID\" = 'com.audiohub.app' ] || { echo 'Refusing to remove an unrecognized /Applications/AudioHub.app' >&2; exit 30; }
  /bin/rm -rf \"$APP\"
fi
/usr/sbin/pkgutil --forget com.audiohub.app.pkg >/dev/null 2>&1 || true
/usr/sbin/pkgutil --forget com.audiohub.driver.pkg >/dev/null 2>&1 || true
"

	try
		do shell script rootCommand with administrator privileges
	on error errorMessage number errorNumber
		if errorNumber is -128 then
			display dialog cancelledText with title dialogTitle buttons {"OK"} default button 1 with icon caution
		else
			display dialog failurePrefix & return & errorMessage with title dialogTitle buttons {"OK"} default button 1 with icon stop
		end if
		return
	end try

	display dialog successText with title dialogTitle buttons {"OK"} default button 1 with icon note
end run
