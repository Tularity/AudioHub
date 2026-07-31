# Install the AudioHub APP (and its daemon) to auto-start on Windows.
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File install-windows-autostart.ps1
#
# Undo:  Unregister-ScheduledTask -TaskName AudioHubDaemon -Confirm:$false
#        Get-Process audiohub-app, audiohubd | Stop-Process -Force
#
# Installs the APP and registers IT at logon; the app brings the daemon up
# itself (`ensure_daemon`), exactly as it does on macOS. The daemon outlives the
# window on purpose — the tray item is 「退出界面（音频服务继续运行）」.
#
# Three decisions worth keeping:
#
# 1. A per-user SCHEDULED TASK at logon, NOT a Windows Service. A service runs
#    in session 0, and session 0 isolation leaves it with no access to audio
#    endpoints at all — WASAPI render and capture both need an interactive
#    session. A service would start cleanly and never move a single sample:
#    healthy-looking and completely silent.
#
# 2. `audiohubd.exe`, not `audiohub.exe daemon`. Both run the same daemon, but
#    the process a user finds in Task Manager should be named for what it is,
#    and should match the name it has on macOS.
#
# 3. The task launches the APP, which is a GUI-subsystem binary and therefore
#    shows no console. An earlier daemon-only layout ran `audiohubd` from a .cmd
#    and left a cmd window on the peer's desktop for as long as it ran — a
#    console-subsystem binary pops a window on ANY interactive launch, and the
#    task's own `Hidden` flag does not cover a window the child creates.
#
# The binary is copied OUT of the build tree, because Windows locks a running
# .exe and a daemon resident on target\release would make the project's own
# sync/build scripts fail to link on the next rebuild.

$ErrorActionPreference = 'Stop'

$SrcDir = 'C:\Users\Administrator\audiohub-src\target\release'
$Dir = 'C:\Users\Administrator\AudioHub'
$LogDir = Join-Path $Dir 'logs'
$TaskName = 'AudioHubDaemon'
$Port = 47810
$User = "$env:COMPUTERNAME\$env:USERNAME"

$SrcD = Join-Path $SrcDir 'audiohubd.exe'
$SrcC = Join-Path $SrcDir 'audiohub.exe'
$SrcA = 'C:\Users\Administrator\audiohub-src\app\src-tauri\target\release\audiohub-app.exe'
if (-not (Test-Path $SrcD)) { throw "build output not found: $SrcD" }
if (-not (Test-Path $SrcA)) { throw "app not built: $SrcA (build it with the msvc toolchain)" }

$ExeD = Join-Path $Dir 'audiohubd.exe'
$ExeC = Join-Path $Dir 'audiohub.exe'
$ExeA = Join-Path $Dir 'audiohub-app.exe'

New-Item -ItemType Directory -Force -Path $Dir | Out-Null
New-Item -ItemType Directory -Force -Path $LogDir | Out-Null

Stop-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
Get-Process audiohubd, audiohub, audiohub-app -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Seconds 2

Copy-Item $SrcD $ExeD -Force
if (Test-Path $SrcC) { Copy-Item $SrcC $ExeC -Force }
Copy-Item $SrcA $ExeA -Force
Write-Output ("installed: " + $ExeA + " (" + (Get-Item $ExeA).Length + " bytes)")
Write-Output ("           " + $ExeD)

# The app is a GUI-subsystem binary, so it needs no console and no shim — the
# VBS wrapper the daemon-only layout required is gone with it.
Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false -ErrorAction SilentlyContinue

$action = New-ScheduledTaskAction -Execute $ExeA
$trigger = New-ScheduledTaskTrigger -AtLogOn -User $User
$principal = New-ScheduledTaskPrincipal -UserId $User -LogonType Interactive -RunLevel Limited
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -StartWhenAvailable -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1) -ExecutionTimeLimit ([TimeSpan]::Zero)
$settings.MultipleInstances = 'IgnoreNew'
$settings.Hidden = $true

Register-ScheduledTask -TaskName $TaskName -Action $action -Trigger $trigger -Principal $principal -Settings $settings | Out-Null
Write-Output ("task     : " + $TaskName + " at logon for " + $User + " (interactive)")

Start-ScheduledTask -TaskName $TaskName
Start-Sleep -Seconds 8

$pa = Get-Process audiohub-app -ErrorAction SilentlyContinue
$pd = Get-Process audiohubd -ErrorAction SilentlyContinue
if ($pa) { Write-Output ("app      : pid " + $pa[0].Id) } else { Write-Output "app      : NOT RUNNING" }
if ($pd) { Write-Output ("daemon   : pid " + $pd[0].Id) } else { Write-Output "daemon   : NOT RUNNING" }
Write-Output ("state    : " + (Get-ScheduledTask -TaskName $TaskName).State)
