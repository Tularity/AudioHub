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

# DriverVer date, in UTC, deliberately NOT "today on this machine".
#
# stampinf's default (%(Inf.TimeStamp) = "*") writes the builder's LOCAL date,
# while inf2cat rejects a DriverVer it considers postdated against UTC. 30-win
# runs at UTC+10, so every build started before 10:00 local time produced a
# driver that compiled and linked and then failed packaging with
#
#   22.9.7: DriverVer set to a date in the future (postdated DriverVer not allowed)
#
# i.e. the build was reproducible only after mid-morning. One day back from the
# UTC date is unambiguously in the past for any timezone on earth, which makes
# the packaging step independent of when and where it runs.
$driverVerDate = [DateTime]::UtcNow.AddDays(-1).ToString('MM/dd/yyyy')

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
foreach ($needle in @('ROOT\AudioHubVad', 'AudioHub Virtual Audio', 'AudioHubVad.cat')) {
    if ($inf -notmatch [regex]::Escape($needle)) { throw "build: generated INF does not contain '$needle'" }
}

Write-Host ""
Write-Host "build: OK -> $pkgDir"
Get-ChildItem $pkgDir -File | ForEach-Object { "  {0,10}  {1}" -f $_.Length, $_.Name } | Write-Host
Write-Output $pkgDir
