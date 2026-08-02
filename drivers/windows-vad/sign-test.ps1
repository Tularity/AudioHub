<#
.SYNOPSIS
    Sign the AudioHubVad driver package with a repository-controlled,
    self-signed TEST certificate. Does not install anything.

.DESCRIPTION
    Test signing is NOT optional. Even with `bcdedit /set testsigning on`, a
    kernel driver image must still carry a valid Authenticode signature -
    test-signing mode only relaxes *who* may issue it, not *whether* there is
    one. `nointegritychecks` does not substitute for a signature either.

    Order matters: the catalog hashes every file in the package directory, so
    the .sys must be signed BEFORE Inf2Cat runs, otherwise the catalog records
    the hash of the unsigned image and the package fails validation at install
    time.

        1. create (or reuse) the test certificate, export .pfx + .cer
        2. signtool sign  -> AudioHubVad.sys
        3. Inf2Cat        -> AudioHubVad.cat   (regenerated, not reused)
        4. signtool sign  -> AudioHubVad.cat
        5. verify

    Verification note: `signtool verify /pa` walks a real trust chain, so on a
    machine that has not imported the test root it necessarily ends in
    "terminated in a root certificate which is not trusted" and exits non-zero.
    That is the correct result on a build machine and says nothing bad about
    the signature. This script therefore asserts what CAN be asserted without
    touching any trust store:

        * a signature is present on both .sys and .cat
        * its signer thumbprint equals the certificate we just used
        * the file digest is SHA256

    It deliberately does NOT import the test root anywhere. Importing into
    Cert:\CurrentUser\Root raises an interactive confirmation dialog (it fails
    outright over SSH with "UI is not allowed in this operation"), and
    importing into Cert:\LocalMachine\Root would change the whole build
    machine's code-signing trust. A full `/pa` PASS belongs on the test
    machine, which imports the certificate anyway as part of installation:

        Import-Certificate -FilePath AudioHubVad-TestCert.cer -CertStoreLocation Cert:\LocalMachine\Root
        Import-Certificate -FilePath AudioHubVad-TestCert.cer -CertStoreLocation Cert:\LocalMachine\TrustedPublisher
        signtool verify /pa /v AudioHubVad.sys

.NOTES
    Pure ASCII on purpose (the Windows hosts read .ps1 as GBK).
#>
[CmdletBinding()]
param(
    [ValidateSet('Debug', 'Release')] [string] $Configuration = 'Release',
    [ValidateSet('x64')]              [string] $Platform      = 'x64',
    [string] $PackageDir,
    [string] $CertSubject   = 'CN=AudioHub Test Signing',
    [string] $CertFriendly  = 'AudioHub Test Signing',
    [string] $PfxPassword   = 'audiohub-test',
    [string] $WdkVersion    = '10.0.26100.6584',
    [string] $SdkCppVersion = '10.0.26100.1',
    [string] $KitVersion    = '10.0.26100.0',
    [string] $PackagesRoot  = "$env:UserProfile\.nuget\packages",
    [switch] $ForceNewCert
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $MyInvocation.MyCommand.Path

if (-not $PackageDir) { $PackageDir = Join-Path $root "$Platform\$Configuration\package" }
if (-not (Test-Path $PackageDir)) { throw "package directory not found: $PackageDir (run build.ps1 first)" }

$sys = Join-Path $PackageDir 'AudioHubVad.sys'
$inf = Join-Path $PackageDir 'AudioHubVad.inf'
$cat = Join-Path $PackageDir 'AudioHubVad.cat'
foreach ($f in @($sys, $inf)) { if (-not (Test-Path $f)) { throw "missing input: $f" } }

$signtool = Join-Path $PackagesRoot "microsoft.windows.sdk.cpp\$SdkCppVersion\c\bin\$KitVersion\x64\signtool.exe"
$inf2cat  = Join-Path $PackagesRoot "microsoft.windows.wdk.x64\$WdkVersion\c\bin\$KitVersion\x86\Inf2Cat.exe"
foreach ($t in @($signtool, $inf2cat)) { if (-not (Test-Path $t)) { throw "tool not found: $t (run tools\restore-wdk.ps1)" } }

# ------------------------------------------------------------------ 1. cert
$certDir = Join-Path $root '_build\testcert'
New-Item -ItemType Directory -Path $certDir -Force | Out-Null
$pfxPath = Join-Path $certDir 'AudioHubVad-TestCert.pfx'
$cerPath = Join-Path $certDir 'AudioHubVad-TestCert.cer'

$cert = Get-ChildItem Cert:\CurrentUser\My |
        Where-Object { $_.Subject -eq $CertSubject } |
        Sort-Object NotAfter -Descending |
        Select-Object -First 1

if ($ForceNewCert -or -not $cert) {
    Write-Host "sign-test: creating self-signed test certificate $CertSubject"
    $cert = New-SelfSignedCertificate `
                -Type CodeSigningCert `
                -Subject $CertSubject `
                -FriendlyName $CertFriendly `
                -CertStoreLocation Cert:\CurrentUser\My `
                -KeyExportPolicy Exportable `
                -KeyUsage DigitalSignature `
                -HashAlgorithm SHA256 `
                -KeyLength 2048 `
                -NotAfter (Get-Date).AddYears(10)
}
else {
    Write-Host "sign-test: reusing existing certificate $($cert.Thumbprint)"
}

$pw = ConvertTo-SecureString -String $PfxPassword -Force -AsPlainText
Export-PfxCertificate  -Cert $cert -FilePath $pfxPath -Password $pw -Force | Out-Null
Export-Certificate     -Cert $cert -FilePath $cerPath -Type CERT -Force   | Out-Null
Write-Host "sign-test: thumbprint $($cert.Thumbprint)"
Write-Host "sign-test: exported $cerPath"

# ------------------------------------------------------------------ 2. .sys
# /ph adds page hashes, which is what the WDK's own signing step does for
# kernel images. No /t: test signatures are not timestamped (and timestamping
# would make the build depend on network reachability).
& $signtool sign /v /fd sha256 /ph /f $pfxPath /p $PfxPassword $sys
if ($LASTEXITCODE -ne 0) { throw "signtool failed on $sys (exit $LASTEXITCODE)" }

# ---------------------------------------------------------------- 3. catalog
if (Test-Path $cat) { Remove-Item $cat -Force }
& $inf2cat /driver:$PackageDir /os:10_X64 /verbose
if ($LASTEXITCODE -ne 0) { throw "Inf2Cat failed (exit $LASTEXITCODE)" }
if (-not (Test-Path $cat)) {
    # Inf2Cat names the catalog after the INF's CatalogFile= directive; be loud
    # rather than silently shipping a package with no catalog.
    throw "Inf2Cat did not produce $cat"
}

# ------------------------------------------------------------------ 4. .cat
& $signtool sign /v /fd sha256 /f $pfxPath /p $PfxPassword $cat
if ($LASTEXITCODE -ne 0) { throw "signtool failed on $cat (exit $LASTEXITCODE)" }

# ----------------------------------------------------------------- 5. verify
# Trust-independent assertions only - see the header. Nothing here modifies a
# certificate store.
foreach ($f in @($sys, $cat)) {
    $leaf = Split-Path $f -Leaf

    $sig = Get-AuthenticodeSignature -FilePath $f
    if (-not $sig.SignerCertificate) { throw "no signature found on $f" }
    if ($sig.SignerCertificate.Thumbprint -ne $cert.Thumbprint) {
        throw "$f is signed by $($sig.SignerCertificate.Thumbprint), expected $($cert.Thumbprint)"
    }

    # signtool exits non-zero here purely because the self-signed root is not
    # trusted on this machine, so the exit code is intentionally ignored; the
    # assertions are made against the printed chain instead.
    #
    # $ErrorActionPreference must drop to Continue for the call: with 'Stop',
    # merging a native command's non-empty stderr via 2>&1 surfaces as a
    # terminating NativeCommandError, and signtool always writes to stderr here.
    $prevEap = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try   { $out = (& $signtool verify /pa /v $f 2>&1 | Out-String) }
    finally { $ErrorActionPreference = $prevEap }
    if ($out -notmatch 'Hash of file \(sha256\)') { throw "$leaf : signature is not SHA256`n$out" }
    if ($out -notmatch [regex]::Escape($cert.Thumbprint)) { throw "$leaf : signtool did not report the expected signer thumbprint`n$out" }

    $trusted = ($LASTEXITCODE -eq 0)
    Write-Host ("sign-test: {0} signed sha256 by {1} [{2}] - chain trusted here: {3}" -f `
                 $leaf, $sig.SignerCertificate.Subject, $cert.Thumbprint, $trusted)
    if (-not $trusted) {
        Write-Host ("             (expected on a machine without the test root; import {0} to change that)" -f (Split-Path $cerPath -Leaf))
    }
}

# ------------------------------------------------------------------- 6. dist
# Everything the install step needs, in one directory: the signed package plus
# the certificate that has to be imported on the target machine first.
$distDir = Join-Path $root '_build\dist'
if (Test-Path $distDir) { Remove-Item $distDir -Recurse -Force }
New-Item -ItemType Directory -Path $distDir -Force | Out-Null
Get-ChildItem $PackageDir -File | Copy-Item -Destination $distDir -Force
Copy-Item $cerPath -Destination $distDir -Force

Write-Host ""
Write-Host "sign-test: OK -> $distDir"
Get-ChildItem $distDir -File | ForEach-Object {
    "  {0,10}  {1}  sha256={2}" -f $_.Length, $_.Name, (Get-FileHash $_.FullName -Algorithm SHA256).Hash
} | Write-Host
Write-Output $distDir
