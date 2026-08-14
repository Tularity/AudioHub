; Windows installer policy for AudioHub.
;
; The NSIS package is per-machine, so the application, audiohubd, audiohub and
; the fixed driver payload all land below Program Files. The virtual audio
; driver is deliberately NOT installed here: Mode B remains an explicit user
; action in the running App and gets its own UAC consent through
; audiohub-vad-helper.exe.

!define AUDIOHUB_WINDOWS_HOOK_DIR "${__FILEDIR__}"

LangString AudioHubDriverPayloadOptIn ${LANG_ENGLISH} "AudioHubVad payload copied; virtual audio activation remains opt-in."
LangString AudioHubDriverPayloadOptIn ${LANG_SIMPCHINESE} "已复制 AudioHubVad；虚拟音频仍需用户主动安装。"
LangString AudioHubProtectOwnerFailed ${LANG_ENGLISH} "Could not assign protected AudioHub ownership: $1"
LangString AudioHubProtectOwnerFailed ${LANG_SIMPCHINESE} "无法设置 AudioHub 受保护的所有者：$1"
LangString AudioHubProtectAclFailed ${LANG_ENGLISH} "Could not protect AudioHub files: $1"
LangString AudioHubProtectAclFailed ${LANG_SIMPCHINESE} "无法保护 AudioHub 文件：$1"
LangString AudioHubDriverRemovePrompt ${LANG_ENGLISH} "AudioHub virtual audio is installed.$\r$\n$\r$\nRemove its driver too? Choosing No cancels uninstall so the driver never loses its required daemon."
LangString AudioHubDriverRemovePrompt ${LANG_SIMPCHINESE} "已安装 AudioHub 虚拟音频。$\r$\n$\r$\n是否同时移除驱动？选择“否”将取消卸载，以免驱动失去所需的后台服务。"
LangString AudioHubUninstallCancelled ${LANG_ENGLISH} "AudioHub uninstall cancelled; the App, daemon, and driver remain installed."
LangString AudioHubUninstallCancelled ${LANG_SIMPCHINESE} "已取消卸载；App、后台服务和驱动均保持安装。"
LangString AudioHubDriverRemoveReboot ${LANG_ENGLISH} "Windows must restart before AudioHubVad can be removed safely.$\r$\n$\r$\nAudioHub, its daemon, scheduled task, and driver were kept. Restart Windows, then run uninstall again."
LangString AudioHubDriverRemoveReboot ${LANG_SIMPCHINESE} "必须重启 Windows，才能安全移除 AudioHubVad。$\r$\n$\r$\nAudioHub、后台服务、计划任务和驱动均已保留。请重启 Windows 后再次运行卸载程序。"
LangString AudioHubDriverRemoveFailed ${LANG_ENGLISH} "Windows could not safely remove the AudioHub virtual audio driver.$\r$\n$\r$\n$1$\r$\n$\r$\nThe App and daemon were kept so you can retry."
LangString AudioHubDriverRemoveFailed ${LANG_SIMPCHINESE} "Windows 无法安全移除 AudioHub 虚拟音频驱动。$\r$\n$\r$\n$1$\r$\n$\r$\nApp 和后台服务已保留，可稍后重试。"
LangString AudioHubUserCleanupFailed ${LANG_ENGLISH} "AudioHub could not remove this user's background-service lifecycle files. The App was kept so reinstall cannot silently inherit a broken startup state."
LangString AudioHubUserCleanupFailed ${LANG_SIMPCHINESE} "无法清理当前用户的后台服务生命周期文件。App 已保留，以免重装时继承错误的启动状态。"
LangString AudioHubDaemonStopFailed ${LANG_ENGLISH} "The existing AudioHub background service did not stop. No program files were changed; start the installer again to retry."
LangString AudioHubDaemonStopFailed ${LANG_SIMPCHINESE} "现有 AudioHub 后台服务未能停止。程序文件尚未更改；请重新运行安装程序重试。"
LangString AudioHubBootstrapFailed ${LANG_ENGLISH} "AudioHub was copied, but its background service could not be registered and started. Installation stopped instead of reporting a partial setup."
LangString AudioHubBootstrapFailed ${LANG_SIMPCHINESE} "AudioHub 已复制，但无法注册并启动后台服务。安装已停止，不会把不完整的状态报告为成功。"
LangString AudioHubInstallDirRejected ${LANG_ENGLISH} "AudioHub can only use its dedicated Program Files directory. The selected or existing directory is unsafe, redirected, or belongs to another product."
LangString AudioHubInstallDirRejected ${LANG_SIMPCHINESE} "AudioHub 只能安装到 Program Files 下的专用目录。当前目录不安全、被重定向或属于其它程序。"

; Keep non-trivial PowerShell in a real UTF-8 script. Passing nested quoted
; source through NSIS -> CreateProcess -> powershell.exe -Command strips the
; string-literal quotes on Windows PowerShell 5 and turns a safe exact-path
; process sweep into a parser failure.
!macro AudioHubExtractLifecycleScript
  InitPluginsDir
  SetOutPath "$PLUGINSDIR"
  File "/oname=audiohub-installer-lifecycle.ps1" "${AUDIOHUB_WINDOWS_HOOK_DIR}\installer-lifecycle.ps1"
!macroend

!macro AudioHubValidateInstallDirectory
  ; Tauri's maintenance/update page can retain its internal placeholder in
  ; fully silent mode. AudioHub is intentionally non-relocatable: resolve the
  ; runtime target to the one supported per-machine location before touching
  ; any directory, regardless of a UI or `/D` selection.
  StrCpy $INSTDIR "$PROGRAMFILES64\${PRODUCTNAME}"
  !insertmacro AudioHubExtractLifecycleScript
  ; The child receives only this compile-time canonical path, so a hostile
  ; command-line install-dir string can never become PowerShell syntax.
  ; NSIS is a 32-bit process. Sysnative is required here: $SYSDIR would be
  ; redirected to 32-bit PowerShell, whose HKLM:\Software view cannot see the
  ; 64-bit per-machine Tauri registration and would reject a valid upgrade.
  nsExec::ExecToStack '"$WINDIR\Sysnative\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "$PLUGINSDIR\audiohub-installer-lifecycle.ps1" -Mode ValidateInstallDir -InstallDir "$PROGRAMFILES64\${PRODUCTNAME}" -ExpectedInstallDir "$PROGRAMFILES64\${PRODUCTNAME}"'
  Pop $0
  Pop $1
  ${If} $0 != 0
    MessageBox MB_OK|MB_ICONSTOP "$(AudioHubInstallDirRejected)$\r$\n$\r$\n$1" /SD IDOK
    Abort
  ${EndIf}
  SetOutPath "$INSTDIR"
!macroend

; Stop only processes whose kernel-reported executable path is one of the three
; fixed binaries below this exact $INSTDIR. This is the authoritative upgrade /
; uninstall boundary after the current user's graceful IPC attempt: it also
; covers another logged-on user's process without touching a same-named dev
; build or another product directory.
!macro AudioHubStopInstalledProcesses
  !insertmacro AudioHubExtractLifecycleScript
  ${If} $INSTDIR != "$PROGRAMFILES64\${PRODUCTNAME}"
    MessageBox MB_OK|MB_ICONSTOP "$(AudioHubInstallDirRejected)" /SD IDOK
    Abort
  ${EndIf}
  nsExec::ExecToStack '"$WINDIR\Sysnative\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "$PLUGINSDIR\audiohub-installer-lifecycle.ps1" -Mode StopProcesses -InstallDir "$PROGRAMFILES64\${PRODUCTNAME}"'
  Pop $0
  Pop $1
  ${If} $0 != 0
    MessageBox MB_OK|MB_ICONSTOP "$(AudioHubDaemonStopFailed)" /SD IDOK
    Abort
  ${EndIf}
  SetOutPath "$INSTDIR"
!macroend

!macro NSIS_HOOK_PREINSTALL
  ; Refuse custom, redirected, shared, or reparse-point directories before any
  ; program file or ACL is changed. The per-machine x64 contract has exactly
  ; one canonical target.
  !insertmacro AudioHubValidateInstallDirectory
  ; Tauri's stock gate normally runs immediately *after* this hook. Run the
  ; exact same gate first so Cancel cannot leave the old App stopped, then ask
  ; the original interactive user's fixed installed CLI to stop its own daemon
  ; over authenticated IPC. This is deliberately path/user scoped: never kill
  ; every machine process merely because its image is named audiohubd.exe.
  !insertmacro CheckIfAppIsRunning "${MAINBINARYNAME}.exe" "${PRODUCTNAME}"
  ${If} ${FileExists} "$INSTDIR\audiohub.exe"
    ; Program Files is traversable but not writable by an unelevated user. The
    ; random pre-created result file receives a write ACE on that file only;
    ; the installer keeps polling until the CLI exits.
    GetTempFileName $3 "$INSTDIR"
    FileOpen $4 $3 w
    FileWrite $4 "pending$\r$\n"
    FileClose $4
    nsExec::ExecToStack '"$SYSDIR\icacls.exe" "$3" /grant "*S-1-5-32-545:W" /Q'
    Pop $0
    Pop $1
    ${If} $0 != 0
      Delete $3
      MessageBox MB_OK|MB_ICONSTOP "$(AudioHubDaemonStopFailed)" /SD IDOK
      Abort
    ${EndIf}
    nsis_tauri_utils::RunAsUser "$SYSDIR\cmd.exe" '/d /s /c ""$INSTDIR\audiohub.exe" ctl shutdown --json >nul 2>&1 && echo ok>"$3" || echo failed>"$3""'
    Pop $0
    ${If} $0 != 0
      Delete $3
      MessageBox MB_OK|MB_ICONSTOP "$(AudioHubDaemonStopFailed)" /SD IDOK
      Abort
    ${EndIf}
    StrCpy $4 0
audiohub_upgrade_stop_wait:
    IntOp $4 $4 + 1
    Sleep 100
    StrCpy $6 ""
    FileOpen $5 $3 r
    FileRead $5 $6
    FileClose $5
    ${If} $6 == "ok$\r$\n"
      Delete $3
      Sleep 500
      Goto audiohub_upgrade_stop_done
    ${ElseIf} $6 == "failed$\r$\n"
      Delete $3
      ; No live daemon/stale ipc.json also makes ctl fail. The exact-path sweep
      ; below is the authoritative proof that replacement is safe.
      Goto audiohub_upgrade_stop_done
    ${EndIf}
    ${If} $4 < 100
      Goto audiohub_upgrade_stop_wait
    ${EndIf}
    Delete $3
    MessageBox MB_OK|MB_ICONSTOP "$(AudioHubDaemonStopFailed)" /SD IDOK
    Abort
audiohub_upgrade_stop_done:
  ${EndIf}
  !insertmacro AudioHubStopInstalledProcesses
!macroend

!macro NSIS_HOOK_POSTINSTALL
  DetailPrint "$(AudioHubDriverPayloadOptIn)"

  ; Re-check after Tauri copied the payload and wrote its product registration.
  ; This proves there is still no junction for recursive ACL operations to
  ; traverse and that a non-empty tree is registered to this product.
  !insertmacro AudioHubValidateInstallDirectory

  ; NSIS normally copies uninstall.exe to %TEMP% and returns from the original
  ; process before the real uninstaller finishes. That makes the standard
  ; `uninstall.exe /S` entry report success even when the inner process safely
  ; retained the product (for example, driver removal needs a reboot). Keep the
  ; familiar interactive UninstallString, but give Windows management tools a
  ; waitable QuietUninstallString. The protected script performs NSIS's
  ; documented direct `_?=` invocation and propagates every inner exit code to
  ; its caller, then removes uninstall.exe only after a proven success.
  WriteRegStr HKLM "${UNINSTKEY}" "QuietUninstallString" "$\"$WINDIR\System32\WindowsPowerShell\v1.0\powershell.exe$\" -NoProfile -NonInteractive -ExecutionPolicy Bypass -File $\"$INSTDIR\quiet-uninstall.ps1$\" -InstallDir $\"$PROGRAMFILES64\${PRODUCTNAME}$\""

  ; The daemon owns every inbound socket. Keep the exception tied to its exact
  ; Program Files image and trusted network profiles, and make upgrades
  ; idempotent by replacing the stable rule name.
  nsExec::ExecToLog '"$SYSDIR\netsh.exe" advfirewall firewall delete rule name="AudioHub daemon (Domain, Private)"'
  Pop $0
  nsExec::ExecToLog '"$SYSDIR\netsh.exe" advfirewall firewall add rule name="AudioHub daemon (Domain, Private)" dir=in action=allow program="$INSTDIR\audiohubd.exe" enable=yes profile=domain,private'
  Pop $0

  ; Make the privilege boundary true even if the user chose a custom folder:
  ; SYSTEM owns the complete tree; administrators and SYSTEM may change it;
  ; ordinary/unelevated users can only read and execute. The App independently
  ; hashes and read-locks every elevated payload before showing UAC.
  ; Grant item rights and inheritable child rights before removing inherited
  ; Program Files ACLs. Keeping inheritance during /T guarantees descendants
  ; remain reachable while icacls walks the tree; the second command freezes
  ; the complete explicit policy after every item has received its own ACEs.
  nsExec::ExecToStack '"$SYSDIR\icacls.exe" "$INSTDIR" /grant:r "*S-1-5-18:F" "*S-1-5-32-544:F" "*S-1-5-32-545:RX" "*S-1-5-18:(OI)(CI)F" "*S-1-5-32-544:(OI)(CI)F" "*S-1-5-32-545:(OI)(CI)RX" /T /C /Q'
  Pop $0
  Pop $1
  ${If} $0 != 0
    DetailPrint "$(AudioHubProtectAclFailed)"
    Abort
  ${EndIf}
  nsExec::ExecToStack '"$SYSDIR\icacls.exe" "$INSTDIR" /inheritance:r /T /C /Q'
  Pop $0
  Pop $1
  ${If} $0 != 0
    DetailPrint "$(AudioHubProtectAclFailed)"
    Abort
  ${EndIf}
  nsExec::ExecToStack '"$SYSDIR\icacls.exe" "$INSTDIR" /setowner "*S-1-5-18" /T /C /Q'
  Pop $0
  Pop $1
  ${If} $0 != 0
    DetailPrint "$(AudioHubProtectOwnerFailed)"
    Abort
  ${EndIf}

  ; Drop back to the interactive user's token. AudioHub performs the per-user
  ; marker and Scheduled Task transaction itself, then exits while its daemon
  ; remains in that user's audio session. Fresh install defaults autostart on;
  ; upgrades preserve marker+missing-task as an explicit opt-out. RunAsUser is
  ; asynchronous, so wait for an App-written terminal result and refuse to
  ; report a partial setup as a successful installer run.
  GetTempFileName $3 "$INSTDIR"
  FileOpen $4 $3 w
  FileWrite $4 "pending$\r$\n"
  FileClose $4
  nsExec::ExecToStack '"$SYSDIR\icacls.exe" "$3" /grant "*S-1-5-32-545:W" /Q'
  Pop $0
  Pop $1
  ${If} $0 != 0
    Delete $3
    MessageBox MB_OK|MB_ICONSTOP "$(AudioHubBootstrapFailed)" /SD IDOK
    Abort
  ${EndIf}
  nsis_tauri_utils::RunAsUser "$INSTDIR\${MAINBINARYNAME}.exe" '--installer-bootstrap --installer-result "$3"'
  Pop $0
  ${If} $0 != 0
    Delete $3
    MessageBox MB_OK|MB_ICONSTOP "$(AudioHubBootstrapFailed)" /SD IDOK
    Abort
  ${EndIf}
  StrCpy $4 0
audiohub_bootstrap_wait:
  IntOp $4 $4 + 1
  Sleep 100
  StrCpy $6 ""
  FileOpen $5 $3 r
  FileRead $5 $6
  FileClose $5
  ${If} $6 == "ok$\r$\n"
    Delete $3
    Goto audiohub_bootstrap_done
  ${ElseIf} $6 == "failed$\r$\n"
    Delete $3
    MessageBox MB_OK|MB_ICONSTOP "$(AudioHubBootstrapFailed)" /SD IDOK
    Abort
  ${EndIf}
  ${If} $4 < 300
    Goto audiohub_bootstrap_wait
  ${EndIf}
  Delete $3
  MessageBox MB_OK|MB_ICONSTOP "$(AudioHubBootstrapFailed)" /SD IDOK
  Abort
audiohub_bootstrap_done:
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  ; AudioHub is deliberately non-relocatable. NSIS's documented waitable
  ; uninstaller form passes `_?=<install-dir>` and can preserve an equivalent
  ; spelling (including a trailing slash). Resolve it to the one canonical
  ; location before *any* lifecycle check or mutation instead of rejecting a
  ; safe official invocation on string-shape alone.
  StrCpy $INSTDIR "$PROGRAMFILES64\${PRODUCTNAME}"

  ; `/UPDATE` is Tauri's replacement contract. It must stop only the old
  ; installed processes and then let the stock uninstaller replace program
  ; files. Driver, task, firewall, marker, settings, identity and opt-out state
  ; intentionally survive for the new installer's postinstall repair.
  ${If} $UpdateMode == 1
    !insertmacro CheckIfAppIsRunning "${MAINBINARYNAME}.exe" "${PRODUCTNAME}"
    !insertmacro AudioHubStopInstalledProcesses
    SetOutPath "$TEMP"
    Goto audiohub_preuninstall_done
  ${EndIf}

  ; Driver removal is explicit for an interactive uninstall. Declining cancels
  ; the whole uninstall: deleting audiohubd.exe while a retained AudioHubVad
  ; device points at that exact image would leave a broken privileged setup.
  ; Silent enterprise uninstall removes the same-product driver automatically.
  StrCpy $2 0
  nsExec::ExecToStack '"$INSTDIR\windows-driver\audiohub-vad-helper.exe" status'
  Pop $0
  Pop $1
  ${If} $0 != 20
    IfSilent audiohub_remove_driver audiohub_driver_prompt
audiohub_driver_prompt:
    MessageBox MB_YESNO|MB_ICONQUESTION "$(AudioHubDriverRemovePrompt)" /SD IDNO IDYES audiohub_remove_driver IDNO audiohub_cancel_uninstall
audiohub_remove_driver:
    StrCpy $2 1
    Goto audiohub_uninstall_decisions_done
audiohub_cancel_uninstall:
    DetailPrint "$(AudioHubUninstallCancelled)"
    Abort
  ${EndIf}

audiohub_uninstall_decisions_done:
  ; Resolve every cancel path before mutating App/daemon/driver state. The
  ; stock gate later in Tauri's template becomes a harmless no-op after this
  ; identical early gate has closed the installed App.
  !insertmacro CheckIfAppIsRunning "${MAINBINARYNAME}.exe" "${PRODUCTNAME}"

  ; Stop only this interactive user's daemon via the CLI in this exact install
  ; directory. Do not kill unrelated processes by image name.
  ${If} ${FileExists} "$INSTDIR\audiohub.exe"
    GetTempFileName $3 "$INSTDIR"
    FileOpen $4 $3 w
    FileWrite $4 "pending$\r$\n"
    FileClose $4
    nsExec::ExecToStack '"$SYSDIR\icacls.exe" "$3" /grant "*S-1-5-32-545:W" /Q'
    Pop $0
    Pop $1
    ${If} $0 != 0
      Delete $3
      MessageBox MB_OK|MB_ICONSTOP "$(AudioHubDaemonStopFailed)" /SD IDOK
      Abort
    ${EndIf}
    nsis_tauri_utils::RunAsUser "$SYSDIR\cmd.exe" '/d /s /c ""$INSTDIR\audiohub.exe" ctl shutdown --json >nul 2>&1 && echo ok>"$3" || echo failed>"$3""'
    Pop $0
    ${If} $0 != 0
      Delete $3
      MessageBox MB_OK|MB_ICONSTOP "$(AudioHubDaemonStopFailed)" /SD IDOK
      Abort
    ${EndIf}
    StrCpy $4 0
audiohub_uninstall_stop_wait:
    IntOp $4 $4 + 1
    Sleep 100
    StrCpy $6 ""
    FileOpen $5 $3 r
    FileRead $5 $6
    FileClose $5
    ${If} $6 == "ok$\r$\n"
      Delete $3
      Sleep 500
      Goto audiohub_uninstall_stop_done
    ${ElseIf} $6 == "failed$\r$\n"
      Delete $3
      Goto audiohub_uninstall_stop_done
    ${EndIf}
    ${If} $4 < 100
      Goto audiohub_uninstall_stop_wait
    ${EndIf}
    Delete $3
    MessageBox MB_OK|MB_ICONSTOP "$(AudioHubDaemonStopFailed)" /SD IDOK
    Abort
audiohub_uninstall_stop_done:
  ${EndIf}
  !insertmacro AudioHubStopInstalledProcesses

  ${If} $2 == 1
    nsExec::ExecToStack '"$INSTDIR\windows-driver\audiohub-vad-helper.exe" uninstall'
    Pop $0
    Pop $1
    ${If} $0 == 25
      DetailPrint "$(AudioHubDriverRemoveReboot)"
      SetRebootFlag true
      ; A pending driver removal is not a successful product uninstall. Keep
      ; the exact helper/daemon/task layout intact so the user can reboot and
      ; safely retry instead of leaving a daemonless kernel driver behind.
      SetErrorLevel 25
      IfSilent audiohub_driver_reboot_abort audiohub_driver_reboot_prompt
audiohub_driver_reboot_prompt:
      MessageBox MB_OK|MB_ICONEXCLAMATION "$(AudioHubDriverRemoveReboot)" /SD IDOK
audiohub_driver_reboot_abort:
      ; `Abort` inside the uninstall section leaves NSIS's process result at
      ; success even after SetErrorLevel. `Quit` preserves the product just as
      ; atomically but also returns 25 to automation and Windows management.
      Quit
    ${ElseIf} $0 != 0
      MessageBox MB_OK|MB_ICONSTOP "$(AudioHubDriverRemoveFailed)" /SD IDOK
      Abort
    ${EndIf}
  ${EndIf}

  ; Remove only lifecycle files; preserve settings, identity and pairings.
  ; RunAsUser gives over-the-shoulder UAC the original interactive token.
  ; The child overwrites a pre-created Public result file so this elevated
  ; uninstaller can verify completion instead of racing a fire-and-forget
  ; process. Do not delete the initial "pending" file while polling: doing so
  ; would also remove the only user-writable ACL in an OTS-UAC install.
  GetTempFileName $3 "$INSTDIR"
  FileOpen $4 $3 w
  FileWrite $4 "pending$\r$\n"
  FileClose $4
  nsExec::ExecToStack '"$SYSDIR\icacls.exe" "$3" /grant "*S-1-5-32-545:W" /Q'
  Pop $0
  Pop $1
  ${If} $0 != 0
    Delete $3
    MessageBox MB_OK|MB_ICONSTOP "$(AudioHubUserCleanupFailed)" /SD IDOK
    Abort
  ${EndIf}
  nsis_tauri_utils::RunAsUser "$SYSDIR\cmd.exe" '/d /s /c "del /f /q "%APPDATA%\AudioHub\service-installed-v1" "%APPDATA%\AudioHub\ipc.json" 2>nul & if exist "%APPDATA%\AudioHub\service-installed-v1" (echo failed>"$3") else if exist "%APPDATA%\AudioHub\ipc.json" (echo failed>"$3") else (echo ok>"$3")"'
  Pop $0
  ${If} $0 != 0
    Delete $3
    MessageBox MB_OK|MB_ICONSTOP "$(AudioHubUserCleanupFailed)" /SD IDOK
    Abort
  ${EndIf}
  StrCpy $4 0
audiohub_cleanup_wait:
  IntOp $4 $4 + 1
  Sleep 100
  ${If} ${FileExists} "$3"
    FileOpen $5 $3 r
    FileRead $5 $6
    FileClose $5
    ${If} $6 == "ok$\r$\n"
      Delete $3
      Goto audiohub_cleanup_done
    ${ElseIf} $6 == "failed$\r$\n"
      Delete $3
      MessageBox MB_OK|MB_ICONSTOP "$(AudioHubUserCleanupFailed)" /SD IDOK
      Abort
    ${EndIf}
  ${EndIf}
  ${If} $4 < 100
    Goto audiohub_cleanup_wait
  ${EndIf}
  Delete $3
  MessageBox MB_OK|MB_ICONSTOP "$(AudioHubUserCleanupFailed)" /SD IDOK
  Abort
audiohub_cleanup_done:
  ; Never traverse every user's profile as SYSTEM: AppData may contain a
  ; user-owned junction. The verified RunAsUser transaction above cleans only
  ; the initiating user's lifecycle files; settings, pairings and logs remain.
  ; The per-user autostart implementation uses this stable task name. Delete it
  ; only after the user lifecycle files are gone, then verify it cannot still be
  ; queried. A retained task would point at files NSIS is about to remove.
  nsExec::ExecToStack '"$SYSDIR\schtasks.exe" /Query /TN "AudioHubDaemon"'
  Pop $0
  Pop $1
  ${If} $0 == 0
    nsExec::ExecToLog '"$SYSDIR\schtasks.exe" /Delete /TN "AudioHubDaemon" /F'
    Pop $0
    nsExec::ExecToStack '"$SYSDIR\schtasks.exe" /Query /TN "AudioHubDaemon"'
    Pop $0
    Pop $1
    ${If} $0 == 0
      MessageBox MB_OK|MB_ICONSTOP "$(AudioHubUserCleanupFailed)" /SD IDOK
      Abort
    ${EndIf}
  ${EndIf}
  nsExec::ExecToLog '"$SYSDIR\netsh.exe" advfirewall firewall delete rule name="AudioHub daemon (Domain, Private)"'
  Pop $0
  SetOutPath "$TEMP"
audiohub_preuninstall_done:
!macroend
