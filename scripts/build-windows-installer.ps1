<#
.SYNOPSIS
Build the single per-machine AudioHub NSIS installer on native Windows.

.DESCRIPTION
Builds and stages audiohub-app, audiohubd, audiohub, the fixed-purpose
AudioHubVad helper and exactly three driver package files (.inf/.sys/.cat).
It never installs the driver, imports a certificate, enables testsigning, or
copies private-key material.

Production builds must pass a signed package with -DriverPackageDir. A local
build-only artifact may use -AllowUnsignedDriver; this changes packaging only
and still never changes the machine's boot or driver state.

AuthenticodeMode Development is deliberately unsigned and passes --no-sign to
Tauri. AuthenticodeMode Release is fail-closed: a usable code-signing
certificate and RFC 3161 timestamp server are mandatory, every shipped
user-mode executable is signed, and all final signatures are verified.
#>

[CmdletBinding()]
param(
    [string]$DriverPackageDir,
    [switch]$AllowUnsignedDriver,
    [switch]$SkipDriverBuild,
    [ValidateSet('Development', 'Release')]
    [string]$AuthenticodeMode = 'Development',
    [string]$CertificateThumbprint,
    [string]$TimestampUrl
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$Root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$TauriDir = Join-Path $Root 'app\src-tauri'
$FrontendDir = Join-Path $Root 'app\frontend'
$Target = 'x86_64-pc-windows-msvc'
$Toolchain = '+stable-x86_64-pc-windows-msvc'

function Step([string]$Text) {
    Write-Host ("[audiohub] ==== {0} ====" -f $Text)
}

function Invoke-Checked([scriptblock]$Command, [string]$Description) {
    & $Command
    if ($LASTEXITCODE -ne 0) {
        throw "$Description failed with exit code $LASTEXITCODE"
    }
}

function Normalize-Thumbprint([string]$Value) {
    if (-not $Value) { return '' }
    return ($Value -replace '[^0-9A-Fa-f]', '').ToUpperInvariant()
}

function Get-Sha256Hex([string]$Path) {
    $Stream = [IO.File]::OpenRead($Path)
    $Sha256 = [Security.Cryptography.SHA256]::Create()
    try {
        return ([BitConverter]::ToString($Sha256.ComputeHash($Stream)) -replace '-', '').ToLowerInvariant()
    } finally {
        $Sha256.Dispose()
        $Stream.Dispose()
    }
}

function Find-SignTool {
    if ($env:TAURI_WINDOWS_SIGNTOOL_PATH -and
        (Test-Path -LiteralPath $env:TAURI_WINDOWS_SIGNTOOL_PATH -PathType Leaf)) {
        return (Resolve-Path -LiteralPath $env:TAURI_WINDOWS_SIGNTOOL_PATH).Path
    }

    $Command = Get-Command signtool.exe -ErrorAction SilentlyContinue
    if ($Command) { return $Command.Source }

    $Candidates = @()
    try {
        $KitsRoot = (Get-ItemProperty -LiteralPath 'HKLM:\SOFTWARE\Microsoft\Windows Kits\Installed Roots' -Name KitsRoot10 -ErrorAction Stop).KitsRoot10
        if ($KitsRoot) {
            $Candidates += Get-ChildItem -Path (Join-Path $KitsRoot 'bin\*\x64\signtool.exe') -File -ErrorAction SilentlyContinue
            $Legacy = Join-Path $KitsRoot 'bin\x64\signtool.exe'
            if (Test-Path -LiteralPath $Legacy -PathType Leaf) {
                $Candidates += Get-Item -LiteralPath $Legacy
            }
        }
    } catch {
        # The explicit path, PATH lookup, and repository SDK fallback below
        # remain authoritative; a missing Windows Kits registry key is normal
        # on a minimal builder.
    }

    $SdkPackages = Join-Path $Root 'drivers\windows-vad\packages'
    if (Test-Path -LiteralPath $SdkPackages -PathType Container) {
        $Candidates += Get-ChildItem -LiteralPath $SdkPackages -Filter signtool.exe -File -Recurse -ErrorAction SilentlyContinue |
            Where-Object FullName -Match '[\\/]x64[\\/]signtool\.exe$'
    }

    $Selected = $Candidates | Sort-Object FullName -Descending | Select-Object -First 1
    if (-not $Selected) {
        throw 'signtool.exe was not found; install the Windows SDK or set TAURI_WINDOWS_SIGNTOOL_PATH'
    }
    return $Selected.FullName
}

function Resolve-SigningCertificateStore([string]$Thumbprint) {
    foreach ($Store in @(
        @{ Name = 'CurrentUser'; Path = 'Cert:\CurrentUser\My' },
        @{ Name = 'LocalMachine'; Path = 'Cert:\LocalMachine\My' }
    )) {
        try {
            $Certificates = @(Get-ChildItem -LiteralPath $Store.Path -ErrorAction Stop |
                Where-Object { (Normalize-Thumbprint $_.Thumbprint) -eq $Thumbprint }
            )
        } catch {
            # A locked-down account may be unable to enumerate one store. The
            # other store is still valid if it contains a usable certificate.
            continue
        }
        $Now = Get-Date
        $Usable = @($Certificates | Where-Object {
            $EkuOids = @($_.EnhancedKeyUsageList | ForEach-Object { $_.ObjectId.Value })
            $_.HasPrivateKey -and $_.NotBefore -le $Now -and $_.NotAfter -gt $Now -and
                $EkuOids -contains '1.3.6.1.5.5.7.3.3'
        })
        if ($Usable.Count -ne 0) {
            return $Store.Name
        }
    }
    throw "no valid code-signing certificate with an accessible private key matches thumbprint $Thumbprint"
}

function Assert-AuthenticodeSignature([string]$Path, [string]$ExpectedThumbprint) {
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        throw "signed artifact is missing: $Path"
    }
    $Signature = Get-AuthenticodeSignature -FilePath $Path
    if ($Signature.Status -ne [System.Management.Automation.SignatureStatus]::Valid) {
        throw "Authenticode verification failed for $Path ($($Signature.Status): $($Signature.StatusMessage))"
    }
    if (-not $Signature.SignerCertificate -or
        (Normalize-Thumbprint $Signature.SignerCertificate.Thumbprint) -ne $ExpectedThumbprint) {
        throw "Authenticode signer mismatch for $Path"
    }
    if (-not $Signature.TimeStamperCertificate) {
        throw "Authenticode timestamp is missing for $Path"
    }
    Invoke-Checked {
        & $SignTool verify /pa /all /v /tw $Path
    } "signtool verification for $Path"
}

function Sign-ReleaseArtifact([string]$Path) {
    Invoke-Checked {
        & $SignTool @ReleaseSignArguments $Path
    } "Authenticode signing for $Path"
    Assert-AuthenticodeSignature $Path $NormalizedThumbprint
}

$ReleaseSigning = $AuthenticodeMode -eq 'Release'
$NormalizedThumbprint = Normalize-Thumbprint $CertificateThumbprint
$SignTool = $null
$ReleaseSignArguments = @()

# Development installers are exercised repeatedly on isolated test hosts and
# must never disappear into Tauri's network-downloaded WebView2 bootstrapper.
# Release artifacts remain self-contained: the full Evergreen offline runtime
# is embedded and installed silently. Keep this build-mode decision here (not
# in tauri.windows.conf.json) so a direct Tauri invocation cannot accidentally
# make a production-looking package with the development-only skip policy.
$TauriWindowsConfig = @{
    webviewInstallMode = if ($ReleaseSigning) {
        @{ type = 'offlineInstaller'; silent = $true }
    } else {
        @{ type = 'skip' }
    }
}

if ($ReleaseSigning) {
    if ($AllowUnsignedDriver) {
        throw '-AllowUnsignedDriver cannot be used with -AuthenticodeMode Release'
    }
    if ($NormalizedThumbprint -notmatch '^[0-9A-F]{40}$') {
        throw '-CertificateThumbprint must be a 40-digit SHA-1 certificate thumbprint in Release mode'
    }
    if (-not $TimestampUrl) {
        throw '-TimestampUrl is required in Release mode'
    }
    $ParsedTimestamp = $null
    if (-not [Uri]::TryCreate($TimestampUrl, [UriKind]::Absolute, [ref]$ParsedTimestamp) -or
        $ParsedTimestamp.Scheme -notin @('http', 'https')) {
        throw '-TimestampUrl must be an absolute HTTP(S) RFC 3161 timestamp URL'
    }
    if (-not $DriverPackageDir) {
        throw '-DriverPackageDir must point to a separately production-signed driver package in Release mode'
    }
} else {
    if ($CertificateThumbprint -or $TimestampUrl) {
        throw 'Authenticode parameters require -AuthenticodeMode Release'
    }
    Write-Warning 'DEVELOPMENT artifact: user-mode Authenticode signatures are optional and Tauri will run with --no-sign.'
}

if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {
    throw 'build-windows-installer.ps1 must run on native Windows'
}

# These checks guard lifecycle rules which otherwise fail only on a destructive
# upgrade/uninstall path. Keep the test independent from NSIS rendering so a
# hook refactor cannot silently turn `/UPDATE` into a full uninstall or restore
# an elevated traversal of user-controlled profile directories.
$NsisHooksText = [IO.File]::ReadAllText((Join-Path $TauriDir 'windows\nsis-hooks.nsh'))
$LifecycleText = [IO.File]::ReadAllText((Join-Path $TauriDir 'windows\installer-lifecycle.ps1'))
$QuietUninstallPath = Join-Path $TauriDir 'windows\quiet-uninstall.ps1'
if (-not (Test-Path -LiteralPath $QuietUninstallPath -PathType Leaf)) {
    throw 'waitable quiet-uninstall wrapper is missing'
}
$QuietUninstallText = [IO.File]::ReadAllText($QuietUninstallPath)
$UpdateBranch = $NsisHooksText.IndexOf('${If} $UpdateMode == 1', [StringComparison]::Ordinal)
$DriverDecision = $NsisHooksText.IndexOf('audiohub-vad-helper.exe" status', [StringComparison]::Ordinal)
if ($UpdateBranch -lt 0 -or $DriverDecision -lt 0 -or $UpdateBranch -gt $DriverDecision -or
    $NsisHooksText.IndexOf('Goto audiohub_preuninstall_done', [StringComparison]::Ordinal) -lt 0) {
    throw 'NSIS `/UPDATE` must bypass the full driver/lifecycle uninstall before any destructive decision'
}
foreach ($Forbidden in @('CleanupProfiles', 'ProfileList')) {
    if ($NsisHooksText.Contains($Forbidden) -or $LifecycleText.Contains($Forbidden)) {
        throw "installer lifecycle must not traverse user profiles as SYSTEM: $Forbidden"
    }
}
foreach ($Required in @('ValidateInstallDir', 'ExpectedInstallDir', 'ReparsePoint', 'StrCpy $INSTDIR "$PROGRAMFILES64\${PRODUCTNAME}"')) {
    if (-not $NsisHooksText.Contains($Required) -and -not $LifecycleText.Contains($Required)) {
        throw "fixed Program Files installer boundary is missing: $Required"
    }
}
if ($NsisHooksText.Contains('SetEnvironmentVariableW') -or
    $LifecycleText.Contains('AUDIOHUB_INSTALL_DIR')) {
    throw 'installer lifecycle paths must be fixed arguments, not mutable inherited environment variables'
}
if (-not $NsisHooksText.Contains('$WINDIR\Sysnative\WindowsPowerShell\v1.0\powershell.exe') -or
    $NsisHooksText.Contains('$SYSDIR\WindowsPowerShell\v1.0\powershell.exe')) {
    throw 'per-machine lifecycle checks must use 64-bit PowerShell through Sysnative'
}
$UnsafeMessageBoxes = @(
    [regex]::Matches($NsisHooksText, '(?m)^\s*MessageBox\s+.*$') |
        Where-Object { -not $_.Value.Contains('/SD ') }
)
if ($UnsafeMessageBoxes.Count -ne 0) {
    throw 'every custom installer MessageBox must define /SD so silent/session-0 work cannot hang'
}
$RebootBranchStart = $NsisHooksText.IndexOf('${If} $0 == 25', [StringComparison]::Ordinal)
$RebootBranchEnd = if ($RebootBranchStart -ge 0) {
    $NsisHooksText.IndexOf('${ElseIf} $0 != 0', $RebootBranchStart, [StringComparison]::Ordinal)
} else {
    -1
}
if ($RebootBranchStart -lt 0 -or $RebootBranchEnd -lt 0) {
    throw 'driver reboot-required uninstall branch is missing'
}
$RebootBranch = $NsisHooksText.Substring($RebootBranchStart, $RebootBranchEnd - $RebootBranchStart)
foreach ($Required in @('SetRebootFlag true', 'SetErrorLevel 25', 'Quit')) {
    if (-not $RebootBranch.Contains($Required)) {
        throw "driver exit 25 must abort the whole uninstall and preserve its retry environment: $Required"
    }
}
foreach ($Required in @(
    'StrCpy $INSTDIR "$PROGRAMFILES64\${PRODUCTNAME}"',
    'QuietUninstallString',
    'quiet-uninstall.ps1'
)) {
    if (-not $NsisHooksText.Contains($Required)) {
        throw "standard waitable Windows uninstall contract is missing: $Required"
    }
}
foreach ($Required in @(
    '[IO.Path]::GetFullPath',
    '[Microsoft.Win32.RegistryView]::Registry64',
    '[Security.Principal.WindowsBuiltInRole]::Administrator',
    "`$Start.Arguments = '/S _?=' + `$CanonicalInstallDir",
    'if ($ExitCode -eq 0)',
    "`$Machine.OpenSubKey(`$UninstallKeyPath)",
    'Remove-Item -LiteralPath $Uninstaller',
    'exit $ExitCode'
)) {
    if (-not $QuietUninstallText.Contains($Required)) {
        throw "quiet-uninstall wrapper cannot safely propagate the inner NSIS result: $Required"
    }
}

$Cargo = (Get-Command cargo.exe -ErrorAction Stop).Source
$Node = (Get-Command node.exe -ErrorAction Stop).Source
$Npm = (Get-Command npm.cmd -ErrorAction Stop).Source

if ($ReleaseSigning) {
    $SignTool = Find-SignTool
    $env:TAURI_WINDOWS_SIGNTOOL_PATH = $SignTool
    $CertificateStore = Resolve-SigningCertificateStore $NormalizedThumbprint
    $ReleaseSignArguments = @('sign', '/fd', 'SHA256', '/sha1', $NormalizedThumbprint)
    if ($CertificateStore -eq 'LocalMachine') {
        $ReleaseSignArguments += '/sm'
    }
    $ReleaseSignArguments += @('/tr', $TimestampUrl, '/td', 'SHA256', '/d', 'AudioHub')
    $TauriWindowsConfig.digestAlgorithm = 'sha256'
    $TauriWindowsConfig.certificateThumbprint = $NormalizedThumbprint
    $TauriWindowsConfig.timestampUrl = $TimestampUrl
    $TauriWindowsConfig.tsp = $true
    $TauriWindowsConfig.signCommand = @{
        cmd = $SignTool
        args = @($ReleaseSignArguments + '%1')
    }
    Write-Host ("[audiohub] RELEASE Authenticode: signer {0} from {1}\My, RFC 3161 timestamping enabled" -f $NormalizedThumbprint, $CertificateStore)
}

$TauriBuildConfig = @{
    bundle = @{
        windows = $TauriWindowsConfig
    }
} | ConvertTo-Json -Depth 8 -Compress

# Cargo invokes link.exe for every shipped binary. Import the x64 developer
# environment without mutating the global machine environment.
$VsWhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
if (-not (Test-Path $VsWhere)) { throw "vswhere not found: $VsWhere" }
$VsInstall = (& $VsWhere -latest -products * -requires Microsoft.Component.MSBuild -property installationPath | Select-Object -First 1)
if (-not $VsInstall) { throw 'Visual Studio 2022 Build Tools were not found' }
$VcVars = Join-Path $VsInstall 'VC\Auxiliary\Build\vcvars64.bat'
if (-not (Test-Path $VcVars)) { throw "vcvars64.bat not found: $VcVars" }
cmd.exe /d /s /c "`"$VcVars`" >nul && set" | ForEach-Object {
    $At = $_.IndexOf('=')
    if ($At -gt 0) {
        [Environment]::SetEnvironmentVariable($_.Substring(0, $At), $_.Substring($At + 1), 'Process')
    }
}
if ($LASTEXITCODE -ne 0) { throw 'vcvars64.bat failed' }

Step 'Windows-native frontend dependencies'
# The license inventory reads every locked production package from
# node_modules, so merely checking that the directory exists is insufficient:
# a previous checkout can leave a valid-looking but incomplete dependency
# tree. `npm install` is lockfile-aware and cheap when the tree is current.
Invoke-Checked { & $Npm --prefix $FrontendDir install --no-audit --no-fund } 'npm install'

# cargo-about resolves the union of target-specific dependency graphs while it
# runs frozen. A target-qualified fetch only downloads that one graph, so a
# clean Windows builder would otherwise be missing Linux-only crates such as
# alsa even though none of them are compiled into the Windows artifact.
Step 'locked Cargo license sources'
Invoke-Checked { & $Cargo fetch --locked --manifest-path (Join-Path $Root 'Cargo.toml') } 'core Cargo fetch'
Invoke-Checked { & $Cargo fetch --locked --manifest-path (Join-Path $TauriDir 'Cargo.toml') } 'App Cargo fetch'

Step 'license inventory'
Invoke-Checked { & $Node (Join-Path $Root 'scripts\generate-third-party-licenses.mjs') } 'license generation'

if (-not $DriverPackageDir) {
    if ($SkipDriverBuild) {
        throw '-SkipDriverBuild requires -DriverPackageDir'
    }
    Step 'build AudioHubVad package (build only)'
    & (Join-Path $Root 'drivers\windows-vad\build.ps1')
    if ($LASTEXITCODE -ne 0) { throw "AudioHubVad build failed with exit code $LASTEXITCODE" }
    $DriverPackageDir = Join-Path $Root 'drivers\windows-vad\x64\Release\package'
}
$DriverPackageDir = (Resolve-Path $DriverPackageDir).Path

Step 'validate fixed driver package'
$DriverFiles = @{}
foreach ($Name in @('AudioHubVad.inf', 'AudioHubVad.sys', 'AudioHubVad.cat')) {
    $Path = Join-Path $DriverPackageDir $Name
    if (-not (Test-Path $Path -PathType Leaf)) { throw "driver package is missing $Path" }
    if ((Get-Item $Path).Length -eq 0) { throw "driver package file is empty: $Path" }
    $DriverFiles[$Name] = $Path
}
$InfText = [IO.File]::ReadAllText($DriverFiles['AudioHubVad.inf'])
foreach ($Needle in @('ROOT\AudioHubVad', 'AudioHubVad.sys', 'AudioHubVad.cat', 'ServiceBinary')) {
    if ($InfText.IndexOf($Needle, [StringComparison]::OrdinalIgnoreCase) -lt 0) {
        throw "AudioHubVad.inf does not contain required declaration: $Needle"
    }
}
$Unsigned = @()
if (-not $AllowUnsignedDriver) {
    foreach ($Name in @('AudioHubVad.sys', 'AudioHubVad.cat')) {
        $Signature = Get-AuthenticodeSignature $DriverFiles[$Name]
        if ($Signature.Status -ne [System.Management.Automation.SignatureStatus]::Valid) {
            $Unsigned += ("{0} ({1})" -f $Name, $Signature.Status)
        }
    }
}
if ($Unsigned.Count -ne 0 -and -not $AllowUnsignedDriver) {
    throw ("refusing unsigned/untrusted driver payload: {0}. Pass a production-signed package with -DriverPackageDir; -AllowUnsignedDriver is only for a build-only artifact." -f ($Unsigned -join ', '))
}
if ($AllowUnsignedDriver) {
    Write-Warning 'BUILD-ONLY installer: driver trust validation was explicitly bypassed with -AllowUnsignedDriver.'
}

Step 'build CLI and daemon (MSVC x64)'
Push-Location $Root
try {
    Invoke-Checked {
        & $Cargo $Toolchain build --release --target $Target -p audiohub-cli -p audiohubd
    } 'CLI/daemon build'
} finally {
    Pop-Location
}

$RootRelease = Join-Path $Root "target\$Target\release"
if ($ReleaseSigning) {
    Step 'sign and verify CLI and daemon'
    Sign-ReleaseArtifact (Join-Path $RootRelease 'audiohub.exe')
    Sign-ReleaseArtifact (Join-Path $RootRelease 'audiohubd.exe')
}
$env:AUDIOHUB_WINDOWS_DRIVER_PACKAGE_DIR = $DriverPackageDir
$env:AUDIOHUB_WINDOWS_DAEMON_PATH = Join-Path $RootRelease 'audiohubd.exe'
Step 'build fixed-purpose elevated helper with payload digests'
Push-Location $Root
try {
    Invoke-Checked {
        & $Cargo $Toolchain build --release --target $Target -p audiohub-vad-helper
    } 'elevated helper build'
} finally {
    Pop-Location
}
if ($ReleaseSigning) {
    Step 'sign and verify fixed-purpose elevated helper'
    Sign-ReleaseArtifact (Join-Path $RootRelease 'audiohub-vad-helper.exe')
}

$Binaries = Join-Path $TauriDir 'binaries'
$DriverStage = Join-Path $TauriDir 'windows\driver-payload'
New-Item -ItemType Directory -Force $Binaries | Out-Null
if (Test-Path $DriverStage) { Remove-Item -Recurse -Force $DriverStage }
New-Item -ItemType Directory -Force $DriverStage | Out-Null

Copy-Item (Join-Path $RootRelease 'audiohub.exe') (Join-Path $Binaries "audiohub-$Target.exe") -Force
Copy-Item (Join-Path $RootRelease 'audiohubd.exe') (Join-Path $Binaries "audiohubd-$Target.exe") -Force
Copy-Item (Join-Path $RootRelease 'audiohub-vad-helper.exe') (Join-Path $DriverStage 'audiohub-vad-helper.exe') -Force
foreach ($Name in $DriverFiles.Keys) {
    Copy-Item $DriverFiles[$Name] (Join-Path $DriverStage $Name) -Force
}
if (Get-ChildItem $DriverStage -File | Where-Object Extension -Match '^\.(pfx|p12|pvk|key|pem)$') {
    throw 'private-key material entered the driver staging directory'
}

Step 'build per-machine NSIS installer'
if (-not (Get-Command cargo-tauri.exe -ErrorAction SilentlyContinue)) {
    Write-Host '[audiohub] cargo-tauri not installed; installing tauri-cli 2.x'
    Invoke-Checked {
        & $Cargo $Toolchain install tauri-cli --version '^2' --locked
    } 'cargo-tauri installation'
}
$TauriBuildConfigDir = Join-Path $TauriDir 'target'
New-Item -ItemType Directory -Force $TauriBuildConfigDir | Out-Null
$TauriBuildConfigPath = Join-Path $TauriBuildConfigDir (
    'audiohub-tauri-build-{0}.json' -f [Guid]::NewGuid().ToString('N')
)
[IO.File]::WriteAllText($TauriBuildConfigPath, $TauriBuildConfig, [Text.UTF8Encoding]::new($false))
$BundleStartedUtc = [DateTime]::UtcNow
Push-Location $TauriDir
try {
    $TauriArgs = @($Toolchain, 'tauri', 'build', '--target', $Target, '--bundles', 'nsis')
    # Windows PowerShell 5's legacy native-argument marshalling strips the
    # quotes from inline JSON. A real config file keeps direct, SSH, Scheduled
    # Task and CI invocations byte-for-byte equivalent.
    $TauriArgs += @('--config', $TauriBuildConfigPath)
    if (-not $ReleaseSigning) {
        $TauriArgs += '--no-sign'
    }
    Invoke-Checked { & $Cargo @TauriArgs } 'Tauri NSIS build'
} finally {
    Pop-Location
    Remove-Item -LiteralPath $TauriBuildConfigPath -Force -ErrorAction SilentlyContinue
}

$NsisDir = Join-Path $TauriDir "target\$Target\release\bundle\nsis"
$Installer = Get-ChildItem $NsisDir -Filter '*-setup.exe' -File |
    Sort-Object LastWriteTimeUtc -Descending |
    Select-Object -First 1
if (-not $Installer -or $Installer.Length -eq 0) {
    throw "NSIS installer was not produced under $NsisDir"
}
if ($Installer.LastWriteTimeUtc -lt $BundleStartedUtc) {
    throw "NSIS installer is stale and was not produced by this build: $($Installer.FullName)"
}

# Prove that Tauri honored the mode-specific WebView2 contract. This is
# intentionally checked against the generated NSIS source, not merely the JSON
# we passed on its command line: schema drift or merge precedence must fail the
# build before an incorrectly dependent Release installer can be published.
$GeneratedNsis = Get-ChildItem (Join-Path $TauriDir "target\$Target\release\nsis") -Filter installer.nsi -File -Recurse |
    Sort-Object LastWriteTimeUtc -Descending |
    Select-Object -First 1
if (-not $GeneratedNsis -or $GeneratedNsis.LastWriteTimeUtc -lt $BundleStartedUtc) {
    throw 'cannot verify WebView2 policy because the generated NSIS script is missing or stale'
}
$GeneratedNsisText = Get-Content -LiteralPath $GeneratedNsis.FullName -Raw
$ExpectedWebviewMode = if ($ReleaseSigning) { 'offlineInstaller' } else { '' }
$WebviewModeMatch = [regex]::Match(
    $GeneratedNsisText,
    '(?m)^!define INSTALLWEBVIEW2MODE\s+"([^"]*)"\s*$'
)
$WebviewArgsMatch = [regex]::Match(
    $GeneratedNsisText,
    '(?m)^!define WEBVIEW2INSTALLERARGS\s+"([^"]*)"\s*$'
)
if (-not $WebviewModeMatch.Success -or
    $WebviewModeMatch.Groups[1].Value -ne $ExpectedWebviewMode -or
    -not $WebviewArgsMatch.Success -or
    $WebviewArgsMatch.Groups[1].Value -ne '/silent') {
    throw "generated NSIS WebView2 policy mismatch (expected mode '$ExpectedWebviewMode' with /silent)"
}
if ($ReleaseSigning) {
    $WebviewPathMatch = [regex]::Match(
        $GeneratedNsisText,
        '(?m)^!define WEBVIEW2INSTALLERPATH\s+"([^"]+)"\s*$'
    )
    if (-not $WebviewPathMatch.Success -or
        -not (Test-Path -LiteralPath $WebviewPathMatch.Groups[1].Value -PathType Leaf)) {
        throw 'Release NSIS did not embed a resolved WebView2 offline installer'
    }
}
foreach ($Required in @('quiet-uninstall.ps1')) {
    if (-not $GeneratedNsisText.Contains($Required)) {
        throw "generated NSIS package omitted the waitable quiet-uninstall contract: $Required"
    }
}

# Development and Release artifacts must never share a final filename: an
# unsigned local build must not silently replace a release installer in the
# handoff directory (or vice versa). Remove the opposite-mode artifact and give
# Development an unmistakable suffix; Release retains the normal product name.
$BaseInstallerName = $Installer.Name
if ($ReleaseSigning) {
    $DevelopmentName = [IO.Path]::GetFileNameWithoutExtension($BaseInstallerName) + '-dev.exe'
    Remove-Item -LiteralPath (Join-Path $NsisDir $DevelopmentName) -Force -ErrorAction SilentlyContinue
} else {
    $DevelopmentName = [IO.Path]::GetFileNameWithoutExtension($BaseInstallerName) + '-dev.exe'
    $DevelopmentPath = Join-Path $NsisDir $DevelopmentName
    Remove-Item -LiteralPath $DevelopmentPath -Force -ErrorAction SilentlyContinue
    Move-Item -LiteralPath $Installer.FullName -Destination $DevelopmentPath
    $Installer = Get-Item -LiteralPath $DevelopmentPath
}

Step 'verify staged payload and installer'
foreach ($Path in @(
    (Join-Path $Binaries "audiohub-$Target.exe"),
    (Join-Path $Binaries "audiohubd-$Target.exe"),
    (Join-Path $DriverStage 'audiohub-vad-helper.exe'),
    (Join-Path $DriverStage 'AudioHubVad.inf'),
    (Join-Path $DriverStage 'AudioHubVad.sys'),
    (Join-Path $DriverStage 'AudioHubVad.cat')
)) {
    if (-not (Test-Path $Path -PathType Leaf) -or (Get-Item $Path).Length -eq 0) {
        throw "staged payload verification failed: $Path"
    }
}

if ($ReleaseSigning) {
    # Tauri signs the patched main executable that it compresses into NSIS,
    # then intentionally restores the unpatched Cargo output after bundling.
    # Sign that restored standalone build artifact too so every executable
    # left by this release command is independently distributable/verifiable.
    Step 'sign and verify restored standalone App executable'
    Sign-ReleaseArtifact (Join-Path $TauriDir "target\$Target\release\audiohub-app.exe")

    Step 'verify final release signatures'
    foreach ($Path in @(
        (Join-Path $RootRelease 'audiohub.exe'),
        (Join-Path $RootRelease 'audiohubd.exe'),
        (Join-Path $RootRelease 'audiohub-vad-helper.exe'),
        (Join-Path $Binaries "audiohub-$Target.exe"),
        (Join-Path $Binaries "audiohubd-$Target.exe"),
        (Join-Path $DriverStage 'audiohub-vad-helper.exe'),
        (Join-Path $TauriDir "target\$Target\release\audiohub-app.exe"),
        $Installer.FullName
    )) {
        Assert-AuthenticodeSignature $Path $NormalizedThumbprint
    }

    # Tauri injects this command into !uninstfinalize, which signs the actual
    # uninstall.exe before it is compressed into the outer NSIS installer.
    $UninstallerSigningLine = [regex]::Match(
        $GeneratedNsisText,
        '(?m)^!define UNINSTALLERSIGNCOMMAND\s+"(.+)"\s*$'
    )
    if (-not $UninstallerSigningLine.Success -or
        $UninstallerSigningLine.Groups[1].Value.IndexOf($NormalizedThumbprint, [StringComparison]::OrdinalIgnoreCase) -lt 0 -or
        $UninstallerSigningLine.Groups[1].Value.IndexOf($TimestampUrl, [StringComparison]::OrdinalIgnoreCase) -lt 0 -or
        $GeneratedNsisText.IndexOf('!uninstfinalize', [StringComparison]::OrdinalIgnoreCase) -lt 0) {
        throw 'generated NSIS script does not sign uninstall.exe with the required signer and timestamp server'
    }
}

$Hash = Get-Sha256Hex $Installer.FullName
Write-Host ("[audiohub] installer: {0}" -f $Installer.FullName)
Write-Host ("[audiohub] bytes:     {0}" -f $Installer.Length)
Write-Host ("[audiohub] sha256:    {0}" -f $Hash)
Write-Host ("[audiohub] Authenticode mode: {0}" -f $AuthenticodeMode)
Write-Host '[audiohub] driver installation was NOT attempted; testsigning and certificate stores were NOT changed.'
