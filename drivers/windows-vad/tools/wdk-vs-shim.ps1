<#
.SYNOPSIS
    Build a private VCTargetsPath that knows the 'WindowsKernelModeDriver10.0'
    platform toolset, so the driver can be built with the WDK NuGet packages
    alone - no Visual Studio WDK component, no WDK MSI, no reboot.

.DESCRIPTION
    THE PROBLEM

    The WDK NuGet package ships all the driver MSBuild logic
    (WindowsDriver.*.props/.targets, stampinf, Inf2Cat, devcon...), but it does
    NOT ship the Visual Studio glue. That glue normally arrives with the VS
    installer component 'Component.Microsoft.Windows.DriverKit.BuildTools',
    which drops two families of files into the VC targets tree:

        Platforms\<plat>\ImportAfter\WDK.<plat>.*.Platform.props
        Platforms\<plat>\PlatformToolsets\WindowsKernelModeDriver10.0\Toolset.props / .targets

    Without them MSBuild stops at:

        error MSB8020: The build tools for WindowsKernelModeDriver10.0 cannot be found.

    (This is also why the upstream Windows-driver-samples BuildEnvironment.ps1
    insists on a VS installation carrying the DriverKit component even in its
    'NuGet' mode.)

    THE FIX

    Copy the stock VC targets tree (VS 2022 v170, ~175 files / 2.4 MB) to a
    private location and add the missing glue there. The real Visual Studio
    installation is never modified: builds pass /p:VCTargetsPath=<private copy>.

    The ImportAfter files are taken verbatim from the WDK NuGet package - they
    are the ones the VS component would have installed, and they self-guard on
    $(IsKernelModeToolset). Only Toolset.props / Toolset.targets are authored
    here; they are modelled on the stock v143 Toolset.props/.targets, plus:

      * IsKernelModeToolset=true              - unlocks the ImportAfter chain
      * WDKContentRoot trailing-slash fixup   - the NuGet props emits '...\c'
                                                with no trailing separator while
                                                every consumer writes
                                                '$(WDKContentRoot)build\...'
      * WDKBuildFolder = TargetPlatformVersion - selects c\build\10.0.26100.0
      * DesignTime WDK.props / UAP.props      - define KM_IncludePath,
                                                CRT_IncludePath, UM_IncludePath,
                                                KIT_SHARED_IncludePath, which
                                                WindowsDriver.KernelMode.props
                                                folds into IncludePath
      * WindowsDriver.Default.props           - defines TargetPlatformVersion_*
                                                constants that
                                                WindowsDriver.OS.props compares
                                                against (missing => MSB4086)

.NOTES
    Pure ASCII on purpose (the Windows hosts read .ps1 as GBK).
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)] [string] $OutDir,
    [string] $WdkVersion   = '10.0.26100.6584',
    [string] $KitVersion   = '10.0.26100.0',
    [string] $PackagesRoot = "$env:UserProfile\.nuget\packages",
    [switch] $Force
)

$ErrorActionPreference = 'Stop'

$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
if (-not (Test-Path $vswhere)) { throw "vswhere.exe not found at $vswhere - Visual Studio 2017 or later is required." }

$vsInstall = & $vswhere -latest -products * -requires Microsoft.Component.MSBuild -property installationPath
if (-not $vsInstall) { throw "No Visual Studio installation with MSBuild was found." }

$srcVct = Join-Path $vsInstall 'MSBuild\Microsoft\VC\v170'
if (-not (Test-Path $srcVct)) { throw "VC targets tree not found at $srcVct" }

$wdkRoot = Join-Path $PackagesRoot "microsoft.windows.wdk.x64\$WdkVersion\c"
if (-not (Test-Path $wdkRoot)) { throw "WDK NuGet package not found at $wdkRoot - run tools\restore-wdk.ps1 first." }

$dstVct = Join-Path $OutDir 'v170'

# Reuse an existing copy rather than deleting it. MSBuild worker processes can
# still hold a lock on Microsoft.Build.CPPTasks.Common.dll inside this tree
# (build.ps1 passes /nr:false to avoid that, but a build run by hand, or by an
# IDE, will not), and a blind Remove-Item then fails with
# "Access to the path ... is denied". The copied tree is a verbatim copy of a
# read-only VS install, so reusing it is safe; -Force forces a fresh copy.
$haveTree = Test-Path (Join-Path $dstVct 'Microsoft.Cpp.props')
if ($Force -and (Test-Path $dstVct)) {
    Remove-Item $dstVct -Recurse -Force
    $haveTree = $false
}
if (-not $haveTree) {
    New-Item -ItemType Directory -Path $dstVct -Force | Out-Null
    Copy-Item (Join-Path $srcVct '*') $dstVct -Recurse -Force
}
else {
    Write-Host "wdk-vs-shim: reusing existing VC targets copy at $dstVct"
}
# The glue files below are always rewritten - they are small, never locked, and
# must track this script.

foreach ($plat in @('x64')) {
    $wdkImportAfter = Join-Path $wdkRoot "build\$KitVersion\$plat\ImportAfter"
    if (-not (Test-Path $wdkImportAfter)) { throw "WDK ImportAfter files not found at $wdkImportAfter" }

    $platImportAfter = Join-Path $dstVct "Platforms\$plat\ImportAfter"
    New-Item -ItemType Directory -Path $platImportAfter -Force | Out-Null
    Copy-Item (Join-Path $wdkImportAfter '*') $platImportAfter -Force

    $tsDir = Join-Path $dstVct "Platforms\$plat\PlatformToolsets\WindowsKernelModeDriver10.0"
    New-Item -ItemType Directory -Path $tsDir -Force | Out-Null

    $toolsetProps = @"
<Project xmlns="http://schemas.microsoft.com/developer/msbuild/2003">
  <!-- AudioHub: stand-in for the Visual Studio WDK component's toolset props. -->
  <PropertyGroup>
    <IsKernelModeToolset>true</IsKernelModeToolset>
    <WDKContentRoot Condition="'`$(WDKContentRoot)' != '' and !HasTrailingSlash('`$(WDKContentRoot)')">`$(WDKContentRoot)\</WDKContentRoot>
    <WDKBuildFolder Condition="'`$(WDKBuildFolder)' == '' and Exists('`$(WDKContentRoot)build\`$(TargetPlatformVersion)')">`$(TargetPlatformVersion)</WDKBuildFolder>
  </PropertyGroup>

  <!-- DDKPlatform -->
  <Import Condition="Exists('`$(WDKContentRoot)build\`$(WDKBuildFolder)\`$(Platform)\WindowsKernelModeDriver\WDK.`$(Platform).WindowsKernelModeDriver.props')"
          Project="`$(WDKContentRoot)build\`$(WDKBuildFolder)\`$(Platform)\WindowsKernelModeDriver\WDK.`$(Platform).WindowsKernelModeDriver.props" />

  <!-- UM_IncludePath / KIT_SHARED_IncludePath, then KM_IncludePath / CRT_IncludePath -->
  <Import Condition="Exists('`$(WindowsSdkDir)DesignTime\CommonConfiguration\Neutral\UAP\`$(TargetPlatformVersion)\UAP.props')"
          Project="`$(WindowsSdkDir)DesignTime\CommonConfiguration\Neutral\UAP\`$(TargetPlatformVersion)\UAP.props" />
  <Import Condition="Exists('`$(WDKContentRoot)DesignTime\CommonConfiguration\Neutral\WDK\`$(TargetPlatformVersion)\WDK.props')"
          Project="`$(WDKContentRoot)DesignTime\CommonConfiguration\Neutral\WDK\`$(TargetPlatformVersion)\WDK.props" />

  <!-- TargetPlatformVersion_* constants used by WindowsDriver.OS.props -->
  <Import Condition="Exists('`$(WDKContentRoot)build\`$(WDKBuildFolder)\WindowsDriver.Default.props')"
          Project="`$(WDKContentRoot)build\`$(WDKBuildFolder)\WindowsDriver.Default.props" />

  <!-- From here on: same shape as the stock v143 Toolset.props -->
  <Import Project="`$(MSBuildThisFileDirectory)ImportBefore\*.props" Condition="Exists('`$(MSBuildThisFileDirectory)ImportBefore')" />
  <Import Project="`$(VCTargetsPath)\Microsoft.Cpp.MSVC.Toolset.$plat.props" />
  <Import Project="`$(MSBuildThisFileDirectory)ImportAfter\*.props" Condition="Exists('`$(MSBuildThisFileDirectory)ImportAfter')" />
  <Import Project="`$(_PlatformFolder)Platform.Common.props" />
</Project>
"@

    $toolsetTargets = @'
<Project xmlns="http://schemas.microsoft.com/developer/msbuild/2003">
  <!-- AudioHub: stand-in for the Visual Studio WDK component's toolset targets. -->
  <Import Project="$(MSBuildThisFileDirectory)ImportBefore\*.targets" Condition="Exists('$(MSBuildThisFileDirectory)ImportBefore')" />

  <Import Project="$(VCTargetsPath)\Microsoft.CppCommon.targets" />
  <Import Project="$(VCTargetsPath)\Microsoft.Cpp.WindowsSDK.targets" />

  <Import Condition="Exists('$(WDKContentRoot)build\$(WDKBuildFolder)\WindowsDriver.Common.targets')"
          Project="$(WDKContentRoot)build\$(WDKBuildFolder)\WindowsDriver.Common.targets" />

  <Import Project="$(MSBuildThisFileDirectory)ImportAfter\*.targets" Condition="Exists('$(MSBuildThisFileDirectory)ImportAfter')" />
</Project>
'@

    Set-Content -Path (Join-Path $tsDir 'Toolset.props')   -Value $toolsetProps   -Encoding ASCII
    Set-Content -Path (Join-Path $tsDir 'Toolset.targets') -Value $toolsetTargets -Encoding ASCII
}

Write-Host "wdk-vs-shim: private VCTargetsPath ready at $dstVct"
Write-Output ($dstVct + '\')
