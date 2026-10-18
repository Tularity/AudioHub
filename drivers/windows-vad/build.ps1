<#
.SYNOPSIS
    Build the AudioHubVad kernel-mode virtual audio driver from the command
    line, using the WDK NuGet packages only.

.DESCRIPTION
    Steps, in order:
      1. restore the WDK / SDK NuGet packages          (tools\restore-wdk.ps1)
      2. materialise a private VCTargetsPath with the
         WindowsKernelModeDriver10.0 toolset glue      (tools\wdk-vs-shim.ps1)
      3. run the 64-bit MSBuild on AudioHubVad.sln
      4. assert the expected artifacts exist, or fail

    Signing is deliberately switched off here (/p:SignMode=Off): the WDK would
    otherwise mint a per-user 'WDKTestCert <user>' certificate that cannot be
    reproduced from this repository. Run sign-test.ps1 afterwards to sign the
    package with a repository-controlled self-signed test certificate.

    WHY THE 64-BIT MSBUILD IS MANDATORY
    WindowsDriver.Common.targets picks its tool paths off $(Processor_Architecture),
    which is the MSBuild *process* architecture. Under the 32-bit MSBuild it
    matches neither 'AMD64' nor 'ARM64', InfToolPath is left empty, and the
    build dies with:
        TRACKER : error TRK0005: Failed to locate: "stampinf.exe".

.NOTES
    Pure ASCII on purpose (the Windows hosts read .ps1 as GBK).
#>
[CmdletBinding()]
param(
    [ValidateSet('Debug', 'Release')] [string] $Configuration = 'Release',
    [ValidateSet('x64')]              [string] $Platform      = 'x64',
    [string] $WdkVersion    = '10.0.26100.6584',
    [string] $SdkCppVersion = '10.0.26100.1',
    [string] $KitVersion    = '10.0.26100.0',
    [string] $PackagesRoot  = "$env:UserProfile\.nuget\packages",
    # Product version x.y.z for DriverVer. Empty: read app\src-tauri\tauri.conf.json.
    [string] $VersionBase   = '',
    [switch] $Clean
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $MyInvocation.MyCommand.Path
$sln  = Join-Path $root 'AudioHubVad.sln'
if (-not (Test-Path $sln)) { throw "AudioHubVad.sln not found next to build.ps1 ($root)" }

# ---------------------------------------------------------------- 1. packages
& (Join-Path $root 'tools\restore-wdk.ps1') -WdkVersion $WdkVersion -SdkCppVersion $SdkCppVersion -PackagesRoot $PackagesRoot | Out-Null

# ------------------------------------------------------------------- 2. shim
$shimRoot = Join-Path $root '_build\vctargets'
New-Item -ItemType Directory -Path $shimRoot -Force | Out-Null
$vct = & (Join-Path $root 'tools\wdk-vs-shim.ps1') -OutDir $shimRoot -WdkVersion $WdkVersion -KitVersion $KitVersion -PackagesRoot $PackagesRoot |
       Select-Object -Last 1

# ---------------------------------------------------------------- 3. msbuild
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$vsInstall = & $vswhere -latest -products * -requires Microsoft.Component.MSBuild -property installationPath
$msbuild = Join-Path $vsInstall 'MSBuild\Current\Bin\amd64\MSBuild.exe'
if (-not (Test-Path $msbuild)) { throw "64-bit MSBuild not found at $msbuild (it is required, see the note in this script's header)." }

$target = if ($Clean) { 'Rebuild' } else { 'Build' }

# DriverVer: date AND four-part version, both derived from one UTC instant T.
#
# Windows ranks equally-matching driver packages by DriverVer date first and
# version second. Two packages with the same DriverVer are a tie, and PnP may
# then keep (or go back to) the OLDER package's files: T7 saw a reboot load a
# stale AudioHubVad.sys after two same-day builds both stamped 1.0.2.1. So every
# build with different driver sources must stamp a strictly larger DriverVer,
# without anyone editing a version by hand.
#
#   T       = SOURCE_DATE_EPOCH, if set (explicit, e.g. a build harness)
#             else the git HEAD committer time, if drivers\windows-vad has no
#                  uncommitted changes to tracked files (deterministic: the
#                  same commit always gets the same DriverVer)
#             else the current UTC time (dirty or non-git tree; warned)
#   date    = UTC calendar date of T, minus one day
#   version = <x.y.z>.<floor(UTC seconds-of-day of T / 2)>   (0..43199)
#
# (date, version) therefore orders exactly like T at 2-second resolution, so a
# later commit (or a later explicit epoch) always yields a larger DriverVer. x.y.z
# is the product version from tauri.conf.json.
#
# Why the date is one day BEFORE T's UTC date: stampinf's default writes the
# builder's LOCAL date, while inf2cat rejects a DriverVer it considers postdated
# against UTC. 30-win runs at UTC+10, so every build started before 10:00 local
# time once failed packaging with
#
#   22.9.7: DriverVer set to a date in the future (postdated DriverVer not allowed)
#
# One day back from a UTC date at or before "now" is in the past for any
# timezone and tolerates small clock skew between the commit and build hosts.
$inv = [Globalization.CultureInfo]::InvariantCulture
$driverVerSource = $null
$driverVerEpoch = $null
if ($env:SOURCE_DATE_EPOCH) {
    if ($env:SOURCE_DATE_EPOCH -notmatch '^[0-9]{1,12}$') { throw "SOURCE_DATE_EPOCH must be a non-negative integer of seconds, got '$env:SOURCE_DATE_EPOCH'" }
    $driverVerEpoch = [int64]$env:SOURCE_DATE_EPOCH
    $driverVerSource = 'SOURCE_DATE_EPOCH'
} elseif (Get-Command git.exe -ErrorAction SilentlyContinue) {
    # Native stderr must not become a terminating error under 'Stop'.
    $savedEap = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    $ct = & git.exe -C $root log -1 --format=%ct HEAD 2>$null
    $ctExit = $LASTEXITCODE
    $dirty = & git.exe -C $root status --porcelain --untracked-files=no -- . 2>$null
    $dirtyExit = $LASTEXITCODE
    $ErrorActionPreference = $savedEap
    if ($ctExit -eq 0 -and $dirtyExit -eq 0 -and "$ct" -match '^[0-9]+$') {
        if (-not $dirty) {
            $driverVerEpoch = [int64]"$ct"
            $driverVerSource = 'git HEAD committer time'
        } else {
            Write-Warning 'build: drivers\windows-vad has uncommitted changes; DriverVer uses the current UTC time (not reproducible).'
        }
    } else {
        Write-Warning 'build: no usable git HEAD; DriverVer uses the current UTC time (not reproducible).'
    }
}
if ($null -eq $driverVerEpoch) {
    $driverVerEpoch = [DateTimeOffset]::UtcNow.ToUnixTimeSeconds()
    $driverVerSource = 'current UTC time'
}
$driverVerInstant = [DateTimeOffset]::FromUnixTimeSeconds($driverVerEpoch).UtcDateTime
$driverVerDay = $driverVerInstant.Date.AddDays(-1)
if ($driverVerDay -gt [DateTime]::UtcNow.Date) {
    throw ("build: DriverVer instant {0:o} ({1}) lies more than a day in the future of this machine's UTC clock; Inf2Cat would reject it" -f $driverVerInstant, $driverVerSource)
}
$driverVerDate = $driverVerDay.ToString('MM/dd/yyyy', $inv)
$driverVerRevision = [int][Math]::Floor($driverVerInstant.TimeOfDay.TotalSeconds / 2)

if (-not $VersionBase) {
    $tauriConf = Join-Path $root '..\..\app\src-tauri\tauri.conf.json'
    if (-not (Test-Path -LiteralPath $tauriConf)) { throw "build: $tauriConf not found; pass -VersionBase x.y.z" }
    $VersionBase = [string]((Get-Content -LiteralPath $tauriConf -Raw | ConvertFrom-Json).version)
}
if ($VersionBase -notmatch '^([0-9]{1,5})\.([0-9]{1,5})\.([0-9]{1,5})$' -or
    [int]$Matches[1] -gt 65535 -or [int]$Matches[2] -gt 65535 -or [int]$Matches[3] -gt 65535) {
    throw "build: product version '$VersionBase' is not x.y.z with fields 0..65535"
}
$driverVersion = '{0}.{1}' -f $VersionBase, $driverVerRevision
Write-Host ("build: DriverVer = {0},{1}  (T = {2:yyyy-MM-ddTHH:mm:ssZ} from {3})" -f $driverVerDate, $driverVersion, $driverVerInstant, $driverVerSource)

$msbuildArgs = @(
    $sln,
    "/t:$target",
    "/p:Configuration=$Configuration",
    "/p:Platform=$Platform",
    "/p:VCTargetsPath=$vct",
    "/p:AudioHubNuGetRoot=$PackagesRoot",
    "/p:AudioHubWdkVersion=$WdkVersion",
    "/p:AudioHubSdkCppVersion=$SdkCppVersion",
    '/p:SignMode=Off',
    "/p:AudioHubDriverVerDate=$driverVerDate",
    "/p:AudioHubDriverVersion=$driverVersion",
    '/m',
    # /nr:false is not optional here. With node reuse on (the default) MSBuild
    # leaves worker processes alive after the build, and those workers keep a
    # file lock on _build\vctargets\v170\Microsoft.Build.CPPTasks.Common.dll -
    # so the NEXT run cannot refresh the private VCTargetsPath and dies with
    # "Access to the path ... is denied".
    '/nr:false',
    '/v:minimal',
    '/nologo'
)

Write-Host "build: $msbuild $($msbuildArgs -join ' ')"
& $msbuild @msbuildArgs
if ($LASTEXITCODE -ne 0) { throw "MSBuild failed with exit code $LASTEXITCODE" }

# ------------------------------------------------------------- 4. verify out
# Any check that fails here must fail the build - a half-produced driver package
# that silently "succeeds" is exactly how a bad .sys reaches a test machine.
$pkgDir = Join-Path $root "$Platform\$Configuration\package"
$expected = @('AudioHubVad.sys', 'AudioHubVad.inf', 'AudioHubVad.cat')
foreach ($f in $expected) {
    $p = Join-Path $pkgDir $f
    if (-not (Test-Path $p)) { throw "build: expected artifact missing: $p" }
    if ((Get-Item $p).Length -le 0) { throw "build: artifact is empty: $p" }
}

$inf = Get-Content (Join-Path $pkgDir 'AudioHubVad.inf') -Raw
# v11: the root devnode is the bus, every peer presentation unit a child.
foreach ($needle in @('ROOT\AudioHubVad', 'AUDIOHUB\PeerEndpoint', 'AUDIOHUBVAD_SA.DeviceDesc="AudioHub Network Audio"', 'AUDIOHUBBUS.DeviceDesc="AudioHub Audio Bus"', 'AudioHubVad.cat', '%AudioHubMfgName%=AUDIOHUBVAD')) {
    if ($inf -notmatch [regex]::Escape($needle)) { throw "build: generated INF does not contain '$needle'" }
}
# W6: a manufacturer key shared with an Include= INF (wdmaudio.inf defines
# MfgName="Microsoft") made DEVPKEY_Device_Manufacturer read "Microsoft".
if ($inf -match '(?im)^\s*MfgName\s*=') { throw "build: generated INF defines the string key MfgName, which collides with wdmaudio.inf" }
# The stamped DriverVer must be exactly the one computed above.
$stamped = [regex]::Match($inf, '(?im)^\s*DriverVer\s*=\s*([0-9/]+)\s*,\s*([0-9.]+)\s*$')
if (-not $stamped.Success -or $stamped.Groups[1].Value -ne $driverVerDate -or $stamped.Groups[2].Value -ne $driverVersion) {
    throw "build: generated INF DriverVer '$($stamped.Value)' differs from the computed $driverVerDate,$driverVersion"
}

Write-Host ""
Write-Host "build: OK -> $pkgDir (DriverVer $driverVerDate,$driverVersion)"
Get-ChildItem $pkgDir -File | ForEach-Object { "  {0,10}  {1}" -f $_.Length, $_.Name } | Write-Host
Write-Output $pkgDir
