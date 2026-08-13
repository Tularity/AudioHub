# Install the AudioHub APP (and its daemon) to auto-start on Windows.
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File install-windows-autostart.ps1
#
# Undo:  Unregister-ScheduledTask -TaskName AudioHubDaemon -Confirm:$false
#        Get-Process audiohub-app, audiohubd | Stop-Process -Force
#        Remove-NetFirewallRule -Name 'AudioHub-Daemon-Inbound' -Confirm:$false -ErrorAction SilentlyContinue
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

param(
    [string]$SrcRoot = 'C:\Users\Administrator\audiohub-src'
)

$ErrorActionPreference = 'Stop'

$SrcDir = Join-Path $SrcRoot 'target\release'
$Dir = 'C:\Users\Administrator\AudioHub'
$LogDir = Join-Path $Dir 'logs'
$LicenseDir = Join-Path $Dir 'licenses'
$TaskName = 'AudioHubDaemon'
$FirewallRuleName = 'AudioHub-Daemon-Inbound'
$User = "$env:COMPUTERNAME\$env:USERNAME"

$SrcD = Join-Path $SrcDir 'audiohubd.exe'
$SrcC = Join-Path $SrcDir 'audiohub.exe'
# The Windows app build deliberately uses an explicit MSVC target. Cargo
# therefore places the executable under the target-triple directory;
# `target\release` can contain an older host/default-toolchain build and must
# never be installed just because it happens to exist.
$SrcA = Join-Path $SrcRoot 'app\src-tauri\target\x86_64-pc-windows-msvc\release\audiohub-app.exe'
if (-not (Test-Path $SrcD)) { throw "build output not found: $SrcD" }
if (-not (Test-Path $SrcA)) { throw "app not built: $SrcA (build it with the msvc toolchain)" }

$ExeD = Join-Path $Dir 'audiohubd.exe'
$ExeC = Join-Path $Dir 'audiohub.exe'
$ExeA = Join-Path $Dir 'audiohub-app.exe'

New-Item -ItemType Directory -Force -Path $Dir | Out-Null
New-Item -ItemType Directory -Force -Path $LogDir | Out-Null
New-Item -ItemType Directory -Force -Path $LicenseDir | Out-Null
foreach ($RetiredLicense in @('openairplay1-LICENSE.txt', 'openairplay1-NOTICE.md')) {
    Remove-Item (Join-Path $LicenseDir $RetiredLicense) -Force -ErrorAction SilentlyContinue
}

Stop-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
Get-Process audiohubd, audiohub, audiohub-app -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Seconds 2

Copy-Item $SrcD $ExeD -Force
if (Test-Path $SrcC) { Copy-Item $SrcC $ExeC -Force }
Copy-Item $SrcA $ExeA -Force
Copy-Item (Join-Path $SrcRoot 'LICENSE') (Join-Path $LicenseDir 'AudioHub-LICENSE.txt') -Force
Copy-Item (Join-Path $SrcRoot 'NOTICE.md') (Join-Path $LicenseDir 'AudioHub-NOTICE.md') -Force
Copy-Item (Join-Path $SrcRoot 'THIRD-PARTY-LICENSES.html') (Join-Path $LicenseDir 'THIRD-PARTY-LICENSES.html') -Force
Write-Output ("installed: " + $ExeA + " (" + (Get-Item $ExeA).Length + " bytes)")
Write-Output ("           " + $ExeD)
Write-Output ("licenses   : " + $LicenseDir)

# The daemon, not the app, owns every inbound listener. Scope the exception to
# that exact installed executable and to trusted Windows network profiles; a
# port-only rule would also admit an unrelated process that later binds the
# same port, while Public would expose AudioHub on untrusted networks.
#
# Replace by the stable internal rule name instead of adding another display
# name. This makes re-installation idempotent and removes the application filter
# that may still point at an older installation path before writing the current
# final path.
Get-NetFirewallRule -Name $FirewallRuleName -ErrorAction SilentlyContinue |
    Remove-NetFirewallRule -Confirm:$false -ErrorAction SilentlyContinue
New-NetFirewallRule `
    -Name $FirewallRuleName `
    -DisplayName 'AudioHub daemon (Domain, Private)' `
    -Description 'Allow inbound AudioHub daemon traffic on trusted networks.' `
    -Direction Inbound `
    -Action Allow `
    -Program $ExeD `
    -Profile Domain,Private `
    -Enabled True | Out-Null
Write-Output ("firewall : " + $FirewallRuleName + " -> " + $ExeD + " (Domain, Private)")

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
