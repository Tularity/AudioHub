; Windows installer policy for AudioHub.
;
; The NSIS package is per-machine, so the application, audiohubd, audiohub and
; the fixed driver payload all land below Program Files. The virtual audio
; driver is deliberately NOT installed here: Mode B remains an explicit user
; action in the running App and gets its own UAC consent through
; audiohub-vad-helper.exe.

!define AUDIOHUB_WINDOWS_HOOK_DIR "${__FILEDIR__}"

; Tauri includes hooks before MUI_LANGUAGE defines LANG_ENGLISH.
; Pin English (US) explicitly so early strings enter the correct table.

LangString AudioHubDriverPayloadOptIn 1033 "AudioHubVad payload copied; virtual audio activation remains opt-in."
LangString AudioHubProtectOwnerFailed 1033 "Could not assign protected AudioHub ownership: $1"
LangString AudioHubProtectAclFailed 1033 "Could not protect AudioHub files: $1"
LangString AudioHubDriverRemovePrompt 1033 "AudioHub virtual audio is installed.$\r$\n$\r$\nRemove its driver too? Choosing No cancels uninstall so the driver never loses its required daemon."
LangString AudioHubUninstallCancelled 1033 "AudioHub uninstall cancelled; the App, daemon, and driver remain installed."
LangString AudioHubDriverRemoveReboot 1033 "Windows must restart before AudioHubVad can be removed safely.$\r$\n$\r$\nAudioHub, its daemon, scheduled task, and driver were kept. Restart Windows, then run uninstall again."
LangString AudioHubDriverRemoveFailed 1033 "Windows could not safely remove the AudioHub virtual audio driver.$\r$\n$\r$\n$1$\r$\n$\r$\nThe App and daemon were kept so you can retry."
LangString AudioHubUserCleanupFailed 1033 "AudioHub could not remove this user's background-service lifecycle files. The App was kept so reinstall cannot silently inherit a broken startup state."
LangString AudioHubGracefulStopTimeout 1033 "The graceful shutdown request did not answer in time; falling back to the exact-path process sweep."
; One message used to cover six unrelated failures, which is exactly what made
; the 2026-08-16 uninstall report impossible to place. Same fix as the bootstrap
; strings above: each site says which step failed.
LangString AudioHubStopHandshakeSetupFailed 1033 "AudioHub could not prepare the shutdown handshake file. No program files were changed; start the installer again to retry."
LangString AudioHubStopHandshakeLaunchFailed 1033 "AudioHub could not run the shutdown request as the logged-on user. No program files were changed; start the installer again to retry."
LangString AudioHubProcessSweepFailed 1033 "An installed AudioHub process is still running and could not be stopped. No program files were changed; start the installer again to retry."
; Four different things can go wrong here and they used to share one message,
; which made a real failure report (2026-08-16) impossible to place: the text
; said the service could not be registered, when in fact it had been and only
; the answer was lost. Each path now names itself.
LangString AudioHubBootstrapFailed 1033 "AudioHub was copied, but its background service could not be registered and started. Installation stopped instead of reporting a partial setup."
LangString AudioHubBootstrapNoHandoff 1033 "AudioHub was copied, but the installer could not hand the setup step to your user account. Installation stopped instead of reporting a partial setup."
LangString AudioHubBootstrapNoLaunch 1033 "AudioHub was copied, but it could not be started to finish setup. Installation stopped instead of reporting a partial setup."
LangString AudioHubBootstrapTimeout 1033 "AudioHub was copied and started, but it did not report the result of registering its background service within 30 seconds. Installation stopped instead of reporting a partial setup. Check app.log in %APPDATA%\AudioHub, then run the installer again."
LangString AudioHubInstallDirRejected 1033 "AudioHub can only use its dedicated Program Files directory. The selected or existing directory is unsafe, redirected, or belongs to another product."

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
    ; $1 carries which process and, when the kill itself was refused, why.
    MessageBox MB_OK|MB_ICONSTOP "$(AudioHubProcessSweepFailed)$\r$\n$\r$\n$1" /SD IDOK
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
      MessageBox MB_OK|MB_ICONSTOP "$(AudioHubStopHandshakeSetupFailed)" /SD IDOK
      Abort
    ${EndIf}
    nsis_tauri_utils::RunAsUser "$SYSDIR\cmd.exe" '/d /s /c ""$INSTDIR\audiohub.exe" ctl shutdown --json >nul 2>&1 && echo ok>"$3" || echo failed>"$3""'
    Pop $0
    ${If} $0 != 0
      Delete $3
      MessageBox MB_OK|MB_ICONSTOP "$(AudioHubStopHandshakeLaunchFailed)" /SD IDOK
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
      ; The CLI returns as soon as the daemon *replies*; the daemon only then
      ; runs teardown, which is allowed BYE_BUDGET (1500ms, core/audiohubd) on
      ; top of releasing the driver. Waiting 500ms here opened fire on a daemon
      ; still inside its own budget and made the sweep race a healthy exit.
      Sleep 2000
      Goto audiohub_upgrade_stop_done
    ${ElseIf} $6 == "failed$\r$\n"
      Delete $3
      ; No live daemon/stale ipc.json also makes ctl fail. The exact-path sweep
      ; below is the authoritative proof that replacement is safe.
      Goto audiohub_upgrade_stop_done
    ${EndIf}
    ; `ctl shutdown` may legitimately block for its own 15s IPC read timeout
    ; (core/audiohub-cli/src/ctl.rs), so the former 10s budget declared a healthy
    ; child failed. A timeout is also *weaker* evidence than the "failed" branch
    ; above, which already defers to the sweep — so it must not be the one case
    ; that aborts. Fall through and let the exact-path sweep decide.
    ${If} $4 < 300
      Goto audiohub_upgrade_stop_wait
    ${EndIf}
    Delete $3
    DetailPrint "$(AudioHubGracefulStopTimeout)"
audiohub_upgrade_stop_done:
  ${EndIf}
  !insertmacro AudioHubStopInstalledProcesses
!macroend

!macro NSIS_HOOK_POSTINSTALL
  DetailPrint "$(AudioHubDriverPayloadOptIn)"

  ; Re-check after Tauri copied the payload and wrote its product registration.
  ; This proves there is still no junction for recursive ACL operations to
  ; traverse and that any product registration matches this fixed directory.
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
    MessageBox MB_OK|MB_ICONSTOP "$(AudioHubBootstrapNoHandoff)" /SD IDOK
    Abort
  ${EndIf}
  nsis_tauri_utils::RunAsUser "$INSTDIR\${MAINBINARYNAME}.exe" '--installer-bootstrap --installer-result "$3"'
  Pop $0
  ${If} $0 != 0
    Delete $3
    MessageBox MB_OK|MB_ICONSTOP "$(AudioHubBootstrapNoLaunch)" /SD IDOK
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
  ; Timed out with the file still "pending". The App may well have finished its
  ; work and only failed to deliver the answer, so this message sends the user
  ; to app.log rather than asserting that the service is unregistered.
  Delete $3
  MessageBox MB_OK|MB_ICONSTOP "$(AudioHubBootstrapTimeout)" /SD IDOK
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
      MessageBox MB_OK|MB_ICONSTOP "$(AudioHubStopHandshakeSetupFailed)" /SD IDOK
      Abort
    ${EndIf}
    nsis_tauri_utils::RunAsUser "$SYSDIR\cmd.exe" '/d /s /c ""$INSTDIR\audiohub.exe" ctl shutdown --json >nul 2>&1 && echo ok>"$3" || echo failed>"$3""'
    Pop $0
    ${If} $0 != 0
      Delete $3
      MessageBox MB_OK|MB_ICONSTOP "$(AudioHubStopHandshakeLaunchFailed)" /SD IDOK
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
      ; The CLI returns as soon as the daemon *replies*; the daemon only then
      ; runs teardown, which is allowed BYE_BUDGET (1500ms, core/audiohubd) on
      ; top of releasing the driver. Waiting 500ms here opened fire on a daemon
      ; still inside its own budget and made the sweep race a healthy exit.
      Sleep 2000
      Goto audiohub_uninstall_stop_done
    ${ElseIf} $6 == "failed$\r$\n"
      Delete $3
      Goto audiohub_uninstall_stop_done
    ${EndIf}
    ; Same budget inversion as the upgrade path: a live daemon takes longer to
    ; drain than the old 10s allowance, which made the first uninstall fail every
    ; time (the second saw no daemon, failed to connect instantly, and passed).
    ${If} $4 < 300
      Goto audiohub_uninstall_stop_wait
    ${EndIf}
    Delete $3
    DetailPrint "$(AudioHubGracefulStopTimeout)"
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
    ; Reset before every read, like the three sibling loops. The stop loop above
    ; leaves "ok" in $6; without this, a FileOpen that fails would re-read that
    ; stale answer and call this cleanup successful without the child replying.
    StrCpy $6 ""
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

!macro NSIS_HOOK_POSTUNINSTALL
  ; Leave nothing in the dedicated directory.
  ;
  ; NSIS deletes only the files it recorded at install time. Anything else that
  ; ended up there outlives the uninstall, and the next install then hits
  ; ValidateInstallDir's "non-empty but not a registered installation" and
  ; stops — a product that will neither install nor say which file is the
  ; problem. Since the directory is exclusively AudioHub's, uninstall owns
  ; clearing it.
  ;
  ; Runs after NSIS has removed its own files, so this is normally a no-op that
  ; catches the remainder. It is best effort: the still-running uninstaller
  ; image lives here and NSIS removes that itself afterwards, and a leftover
  ; must not turn a clean uninstall into a failed one.
  ;
  ; Not during an update. Tauri's template inserts this hook outside all three
  ; of its own `${If} $UpdateMode <> 1` guards, so without this one the /UPDATE
  ; replacement flow would purge the driver payload that the preuninstall hook
  ; deliberately preserves for the new installer to repair — and an installer
  ; that then aborts would strand an installed AudioHubVad with no helper left
  ; on disk to remove it. Unreachable today (nothing passes /UPDATE), but the
  ; preserve contract is stated two hooks up and must hold here too.
  ${If} $UpdateMode <> 1
    !insertmacro AudioHubExtractLifecycleScript
    nsExec::ExecToLog '"$WINDIR\Sysnative\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "$PLUGINSDIR\audiohub-installer-lifecycle.ps1" -Mode PurgeInstallDir -InstallDir "$PROGRAMFILES64\${PRODUCTNAME}" -ExpectedInstallDir "$PROGRAMFILES64\${PRODUCTNAME}"'
    Pop $0
  ${EndIf}
!macroend
