<#
AudioHub's waitable quiet-uninstall entry.

NSIS's ordinary uninstall.exe starts a temporary copy and lets its original
process exit immediately. Windows management tools therefore observe success
before the real uninstaller can report a reboot requirement or a fail-closed
lifecycle error. NSIS also documents a waitable form: execute the protected
installed uninstaller directly with `_?=<install directory>` last, wait for its
result, then let the caller remove the uninstaller that could not delete itself.

This script is installed below protected Program Files and registered as
QuietUninstallString. The user-facing UninstallString remains uninstall.exe so
Tauri's updater can continue appending `/UPDATE _?=...` to that entry.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$InstallDir
)

$ErrorActionPreference = 'Stop'
$ExitCode = 2

try {
    $Identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $Principal = [Security.Principal.WindowsPrincipal]::new($Identity)
    if (-not $Principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw 'AudioHub quiet uninstall requires an elevated administrator process'
    }

    # Read the 64-bit machine view explicitly. GetFolderPath('ProgramFiles')
    # returns Program Files (x86) in a 32-bit PowerShell host, and inherited
    # environment variables are caller-controlled rather than an install-root
    # authority.
    $Machine = [Microsoft.Win32.RegistryKey]::OpenBaseKey(
        [Microsoft.Win32.RegistryHive]::LocalMachine,
        [Microsoft.Win32.RegistryView]::Registry64
    )
    try {
        $CurrentVersion = $Machine.OpenSubKey('SOFTWARE\Microsoft\Windows\CurrentVersion')
        if (-not $CurrentVersion) { throw 'Windows Program Files registration is unavailable' }
        try {
            $ProgramFiles64 = [string]$CurrentVersion.GetValue(
                'ProgramFilesDir',
                $null,
                [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames
            )
        } finally {
            $CurrentVersion.Dispose()
        }
        if (-not $ProgramFiles64) { throw 'Windows 64-bit Program Files path is unavailable' }

        $CanonicalInstallDir = [IO.Path]::GetFullPath($InstallDir.Trim().Trim('"')).TrimEnd('\')
        $ExpectedInstallDir = [IO.Path]::GetFullPath((Join-Path $ProgramFiles64 'AudioHub')).TrimEnd('\')
        if (-not [String]::Equals(
            $CanonicalInstallDir,
            $ExpectedInstallDir,
            [StringComparison]::OrdinalIgnoreCase
        )) {
            throw "AudioHub is not installed at its canonical Program Files path: $ExpectedInstallDir"
        }

        # Reject a redirected parent or payload before launching anything with
        # the caller's elevated token. The installer applies a protected ACL to
        # this whole tree; only administrators/SYSTEM can replace the checked
        # executable between this test and CreateProcess.
        $Cursor = [IO.DirectoryInfo]$CanonicalInstallDir
        while ($Cursor) {
            if (-not $Cursor.Exists) { throw "AudioHub install path is missing: $($Cursor.FullName)" }
            if ($Cursor.Attributes -band [IO.FileAttributes]::ReparsePoint) {
                throw "AudioHub install path traverses a reparse point: $($Cursor.FullName)"
            }
            $Cursor = $Cursor.Parent
        }

        $UninstallKeyPath = 'SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\AudioHub'
        $Product = $Machine.OpenSubKey($UninstallKeyPath)
        if (-not $Product) { throw 'AudioHub uninstall registration is missing' }
        try {
            $RegisteredName = [string]$Product.GetValue('DisplayName', '')
            $RegisteredLocation = [string]$Product.GetValue('InstallLocation', '')
        } finally {
            $Product.Dispose()
        }
        $RegisteredLocation = [IO.Path]::GetFullPath(
            $RegisteredLocation.Trim().Trim('"')
        ).TrimEnd('\')
        if ($RegisteredName -ne 'AudioHub' -or -not [String]::Equals(
            $RegisteredLocation,
            $CanonicalInstallDir,
            [StringComparison]::OrdinalIgnoreCase
        )) {
            throw 'AudioHub uninstall registration does not match the protected product directory'
        }

        $Uninstaller = Join-Path $CanonicalInstallDir 'uninstall.exe'
        if (-not (Test-Path -LiteralPath $Uninstaller -PathType Leaf)) {
            throw "AudioHub uninstaller is missing: $Uninstaller"
        }
        if ((Get-Item -LiteralPath $Uninstaller -Force).Attributes -band
            [IO.FileAttributes]::ReparsePoint) {
            throw 'AudioHub uninstaller must not be a reparse point'
        }

        # `_?=` must be the final, deliberately unquoted raw NSIS argument even
        # when the directory contains spaces. It disables NSIS's detached
        # self-copy, so this process can wait for the authoritative inner code.
        $Start = [Diagnostics.ProcessStartInfo]::new()
        $Start.FileName = $Uninstaller
        $Start.Arguments = '/S _?=' + $CanonicalInstallDir
        $Start.UseShellExecute = $false
        $Start.CreateNoWindow = $true
        $Process = [Diagnostics.Process]::Start($Start)
        if (-not $Process) { throw 'Could not start the waitable AudioHub uninstaller' }
        $Process.WaitForExit()
        $ExitCode = $Process.ExitCode

        # Non-zero means fail-closed or reboot-required. Preserve the exact
        # retry environment untouched. Only a zero result enters the
        # independently verified commit cleanup below.
        if ($ExitCode -eq 0) {
            # The direct `_?=` executable cannot delete itself. Remove it only
            # after independently proving the actual product uninstall committed.
            $RemainingProduct = $Machine.OpenSubKey($UninstallKeyPath)
            if ($RemainingProduct) {
                $RemainingProduct.Dispose()
                throw 'AudioHub still has an uninstall registration after a reported success'
            }
            foreach ($Name in @('audiohub-app.exe', 'audiohubd.exe', 'audiohub.exe')) {
                if (Test-Path -LiteralPath (Join-Path $CanonicalInstallDir $Name)) {
                    throw "AudioHub program file remains after a reported success: $Name"
                }
            }

            $Self = $MyInvocation.MyCommand.Path
            $AllowedFiles = @($Uninstaller)
            if ($Self) { $AllowedFiles += $Self }
            $Unexpected = @(Get-ChildItem -LiteralPath $CanonicalInstallDir -Force -Recurse -ErrorAction Stop |
                Where-Object {
                    $_.PSIsContainer -or $_.FullName -notin $AllowedFiles
                })
            if ($Unexpected.Count -ne 0) {
                throw 'Unexpected files remain in the AudioHub program directory after uninstall'
            }

            # Keep uninstall.exe until every other cleanup check/action has
            # succeeded. If deleting the script fails, the standard retry
            # entry still exists; deleting the uninstaller first would destroy
            # the only recovery path merely because a non-product file lock
            # lingered.
            if ($Self -and (Test-Path -LiteralPath $Self -PathType Leaf)) {
                Remove-Item -LiteralPath $Self -Force -ErrorAction Stop
            }
            Remove-Item -LiteralPath $Uninstaller -Force -ErrorAction Stop
            [IO.Directory]::Delete($CanonicalInstallDir, $false)
            $ExitCode = 0
        }
    } finally {
        $Machine.Dispose()
    }
} catch {
    [Console]::Error.WriteLine($_.Exception.Message)
    $ExitCode = 2
}

exit $ExitCode
