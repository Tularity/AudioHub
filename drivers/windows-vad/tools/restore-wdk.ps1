<#
.SYNOPSIS
    Restore the WDK / Windows SDK NuGet packages used to build AudioHubVad.

.DESCRIPTION
    The WDK is consumed as NuGet packages, NOT as the MSI installer. Nothing is
    written to the registry, no machine-wide install happens, and no reboot is
    ever required - the package's entry props derives WDKContentRoot purely from
    its own directory:

        <WDKContentRoot>$(NuGetPackageFolder)\c</WDKContentRoot>

    Restore goes through `dotnet restore` on a throwaway project. Two details
    are load-bearing:

      * ExcludeAssets="all" - these are native packages with no lib/ for any
        managed TFM; without it the restore fails the framework-compat check.
      * GeneratePathProperty="true" - makes the resolved path observable.

    Everything lands in the normal global package folder
    (%UserProfile%\.nuget\packages), so a second call is a no-op.

.NOTES
    Pure ASCII on purpose: both Windows boxes in this project interpret .ps1
    files as GBK, and non-ASCII bytes break the parse.
#>
[CmdletBinding()]
param(
    [string] $WdkVersion    = '10.0.26100.6584',
    [string] $SdkCppVersion = '10.0.26100.1',
    [string] $PackagesRoot  = "$env:UserProfile\.nuget\packages"
)

$ErrorActionPreference = 'Stop'

$wdkDir = Join-Path $PackagesRoot "microsoft.windows.wdk.x64\$WdkVersion"
$sdkDir = Join-Path $PackagesRoot "microsoft.windows.sdk.cpp\$SdkCppVersion"

if ((Test-Path (Join-Path $wdkDir 'c\Include\10.0.26100.0\km\portcls.h')) -and
    (Test-Path (Join-Path $sdkDir 'build\native\Microsoft.Windows.SDK.cpp.props'))) {
    Write-Host "restore-wdk: already present, nothing to do."
    Write-Output $PackagesRoot
    return
}

$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("audiohub-wdk-restore-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tmp -Force | Out-Null
try {
    $proj = @"
<Project Sdk="Microsoft.NET.Sdk">
  <PropertyGroup>
    <TargetFramework>net8.0</TargetFramework>
    <RestorePackagesPath>$PackagesRoot</RestorePackagesPath>
  </PropertyGroup>
  <ItemGroup>
    <PackageReference Include="Microsoft.Windows.WDK.x64" Version="$WdkVersion" ExcludeAssets="all" GeneratePathProperty="true" />
    <PackageReference Include="Microsoft.Windows.SDK.CPP" Version="$SdkCppVersion" ExcludeAssets="all" GeneratePathProperty="true" />
    <PackageReference Include="Microsoft.Windows.SDK.CPP.x64" Version="$SdkCppVersion" ExcludeAssets="all" GeneratePathProperty="true" />
  </ItemGroup>
</Project>
"@
    Set-Content -Path (Join-Path $tmp 'restore.csproj') -Value $proj -Encoding ASCII
    Write-Host "restore-wdk: dotnet restore -> $PackagesRoot"
    & dotnet restore (Join-Path $tmp 'restore.csproj') | Write-Host
    if ($LASTEXITCODE -ne 0) { throw "dotnet restore failed with exit code $LASTEXITCODE" }
}
finally {
    Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
}

foreach ($probe in @(
        (Join-Path $wdkDir 'c\Include\10.0.26100.0\km\portcls.h'),
        (Join-Path $wdkDir 'c\Lib\10.0.26100.0\km\x64\portcls.lib'),
        (Join-Path $wdkDir 'c\bin\10.0.26100.0\x86\Inf2Cat.exe'),
        (Join-Path $wdkDir 'c\tools\10.0.26100.0\x64\devcon.exe'),
        (Join-Path $sdkDir 'build\native\Microsoft.Windows.SDK.cpp.props'))) {
    if (-not (Test-Path $probe)) { throw "restore-wdk: expected file missing after restore: $probe" }
}

Write-Host "restore-wdk: OK"
Write-Output $PackagesRoot
