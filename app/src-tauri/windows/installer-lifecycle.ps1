[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateSet('ValidateInstallDir', 'StopProcesses', 'PurgeInstallDir')]
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
        if ($Product) {
            $Registered = Get-NormalizedPath ([string]$Product.InstallLocation)
            if ($Product.DisplayName -ne 'AudioHub' -or
                -not [String]::Equals($Registered, $Root, [StringComparison]::OrdinalIgnoreCase)) {
                throw 'the existing product registration does not match the fixed AudioHub directory'
            }
        }
        # NSIS can remove the registration before its running uninstaller is
        # deleted. A cancelled/failed install can leave files too. Missing
        # registration is therefore a recoverable state, not proof of another
        # product. The fixed-path and whole-tree reparse checks above still
        # apply. Let the normal payload copy replace remnants; never execute
        # a leftover uninstaller or purge user configuration to repair setup.
    }
    exit 0
}

if ($Mode -eq 'PurgeInstallDir') {
    # Uninstall leaves nothing behind in the dedicated directory.
    #
    # NSIS removes only what it recorded, so anything else — a log a user
    # dropped in, a hand-copied binary, a .bak from a manual swap — survives an
    # uninstall and then trips `ValidateInstallDir` on the next install, which
    # refuses a non-empty unregistered directory. The user is then stuck with a
    # product that will neither install nor tell them why. Uninstall owns the
    # whole directory, so uninstall empties it.
    #
    # The path is re-derived and re-checked here rather than trusted from the
    # caller: this deletes recursively, and a reparse point anywhere in the tree
    # would take the deletion outside Program Files.
    if (-not $InstallDir -or -not $ExpectedInstallDir) {
        throw 'the fixed AudioHub install directory contract is missing'
    }
    $Root = Get-NormalizedPath $InstallDir
    $Expected = Get-NormalizedPath $ExpectedInstallDir
    if (-not [String]::Equals($Root, $Expected, [StringComparison]::OrdinalIgnoreCase)) {
        throw "refusing to purge a directory that is not the dedicated one: $Root"
    }
    if (-not [IO.Directory]::Exists($Root)) {
        exit 0
    }
    $Cursor = [IO.DirectoryInfo]$Root
    while ($Cursor) {
        if ($Cursor.Exists -and ($Cursor.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
            throw "refusing to purge through a reparse point: $($Cursor.FullName)"
        }
        $Cursor = $Cursor.Parent
    }
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
    # Best effort by design: the uninstaller is still running from inside this
    # directory, so its own image cannot go yet. NSIS removes that last. What
    # matters is that nothing it does not know about is left to block the next
    # install, and a failure here must not fail an otherwise clean uninstall.
    Remove-Item -LiteralPath $Root -Recurse -Force -ErrorAction SilentlyContinue
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

    # A single kill plus a fixed 500ms settle used to decide this. That is not
    # long enough: the graceful `ctl shutdown` above only makes the daemon *begin*
    # draining (Bye to peers, drop the announce, release the HAL, tear down
    # AirPlay sessions), and a kill can also lose a race against a process that
    # is already exiting. Worse, the kill error was swallowed outright, so "still
    # running" and "not allowed to stop it" reported the same way. Retry for 10s
    # and keep the last real error for the message.
    $Deadline = (Get-Date).AddSeconds(10)
    $LastError = $null
    while ($true) {
        $Remaining = @(Get-Process -Name $Wanted -ErrorAction SilentlyContinue |
            Where-Object { Test-AudioHubInstalledProcess $_ })
        if ($Remaining.Count -eq 0) {
            exit 0
        }
        foreach ($Process in $Remaining) {
            try {
                Stop-Process -Id $Process.Id -Force -ErrorAction Stop
            } catch {
                $LastError = $_.Exception.Message
            }
        }
        if ((Get-Date) -ge $Deadline) {
            break
        }
        Start-Sleep -Milliseconds 250
    }
    # The loop breaks right after issuing a kill round, so $Remaining still
    # describes the state BEFORE that round. Settle and look once more, or a
    # process the final kill actually terminated gets reported as surviving.
    Start-Sleep -Milliseconds 250
    $Remaining = @(Get-Process -Name $Wanted -ErrorAction SilentlyContinue |
        Where-Object { Test-AudioHubInstalledProcess $_ })
    if ($Remaining.Count -eq 0) {
        exit 0
    }
    $Names = ($Remaining | ForEach-Object { "$($_.Name) (pid $($_.Id))" }) -join ', '
    if ($LastError) {
        throw "an installed AudioHub process is still running: $Names -- $LastError"
    }
    throw "an installed AudioHub process is still running: $Names"
}
