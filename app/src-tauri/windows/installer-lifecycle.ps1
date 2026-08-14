[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateSet('ValidateInstallDir', 'StopProcesses')]
    [string]$Mode,
    [Parameter(Mandatory = $true)]
    [string]$InstallDir,
    [string]$ExpectedInstallDir
)

$ErrorActionPreference = 'Stop'

function Get-NormalizedPath([string]$Path) {
    return [IO.Path]::GetFullPath($Path.Trim().Trim('"')).TrimEnd('\')
}

if ($Mode -eq 'ValidateInstallDir') {
    if (-not $InstallDir -or -not $ExpectedInstallDir) {
        throw 'the fixed AudioHub install directory contract is missing'
    }
    $Root = Get-NormalizedPath $InstallDir
    $Expected = Get-NormalizedPath $ExpectedInstallDir
    if (-not [String]::Equals($Root, $Expected, [StringComparison]::OrdinalIgnoreCase)) {
        throw "AudioHub must be installed in its dedicated Program Files directory: $Expected"
    }

    $Cursor = [IO.DirectoryInfo]$Root
    while ($Cursor) {
        if ($Cursor.Exists -and ($Cursor.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
            throw "install path traverses a reparse point: $($Cursor.FullName)"
        }
        $Cursor = $Cursor.Parent
    }
    if (-not [IO.Directory]::Exists($Root)) {
        exit 0
    }

    # Enumerate each real directory exactly once and reject a junction/symlink
    # before enqueuing it. This never recursively follows user-controlled
    # reparse points while deciding whether icacls /T may safely touch the tree.
    $Queue = New-Object 'Collections.Generic.Queue[IO.DirectoryInfo]'
    $Queue.Enqueue([IO.DirectoryInfo]$Root)
    while ($Queue.Count -ne 0) {
        $Directory = $Queue.Dequeue()
        foreach ($Entry in $Directory.GetFileSystemInfos()) {
            if ($Entry.Attributes -band [IO.FileAttributes]::ReparsePoint) {
                throw "AudioHub install tree contains a reparse point: $($Entry.FullName)"
            }
            if ($Entry -is [IO.DirectoryInfo]) {
                $Queue.Enqueue($Entry)
            }
        }
    }

    if (([IO.DirectoryInfo]$Root).GetFileSystemInfos().Count -ne 0) {
        $Product = Get-ItemProperty -LiteralPath 'HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall\AudioHub' -ErrorAction SilentlyContinue
        $Registered = if ($Product) { Get-NormalizedPath ([string]$Product.InstallLocation) } else { '' }
        if (-not $Product -or $Product.DisplayName -ne 'AudioHub' -or
            -not [String]::Equals($Registered, $Root, [StringComparison]::OrdinalIgnoreCase)) {
            throw 'the dedicated AudioHub directory is non-empty but is not a registered AudioHub installation'
        }
    }
    exit 0
}

if ($Mode -eq 'StopProcesses') {
    if (-not $InstallDir) {
        throw 'InstallDir is missing'
    }

    $Root = Get-NormalizedPath $InstallDir
    $Wanted = @('audiohub-app', 'audiohubd', 'audiohub')
    function Test-AudioHubInstalledProcess($Process) {
        try {
            $Actual = $Process.Path
        } catch {
            return $false
        }
        if (-not $Actual) {
            return $false
        }
        $Expected = [IO.Path]::Combine($Root, $Process.ProcessName + '.exe')
        return [String]::Equals(
            [IO.Path]::GetFullPath($Actual),
            [IO.Path]::GetFullPath($Expected),
            [StringComparison]::OrdinalIgnoreCase
        )
    }

    @(Get-Process -Name $Wanted -ErrorAction SilentlyContinue |
        Where-Object { Test-AudioHubInstalledProcess $_ }) |
        ForEach-Object { Stop-Process -Id $_.Id -Force -ErrorAction SilentlyContinue }
    Start-Sleep -Milliseconds 500
    $Remaining = @(Get-Process -Name $Wanted -ErrorAction SilentlyContinue |
        Where-Object { Test-AudioHubInstalledProcess $_ })
    if ($Remaining.Count -ne 0) {
        throw 'an installed AudioHub process is still running'
    }
    exit 0
}
