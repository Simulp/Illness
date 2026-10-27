<#
.SYNOPSIS
    Builds, registers, signs and removes the sparse MSIX package that puts
    Illness in the Windows 11 modern Explorer context menu.

.DESCRIPTION
    The package contains only the manifest and its logos. The COM server DLL and
    illness.exe stay in the app's install directory; the manifest reaches them
    through uap10:AllowExternalContent. Every install-related switch therefore
    needs an install directory that already holds illness.exe at its root and
    the DLL under appx\.

    Without switches the script only builds: cargo builds the COM server, the
    manifest and resources\ are staged, makeappx packs the result into
    dist\illness_explorer_command_injector.appx and the stage is removed.

    -Register is the local development path. It needs Windows Developer Mode
    and registers the loose manifest in place, so the stage (<install
    dir>\pkg) is kept: the signed-package path is the only one that can run
    from a disposable .appx.

    -Install is the signed path. It hands the packed .appx to Add-AppxPackage,
    which rejects unsigned packages (0x800B0100) and packages signed by a
    certificate whose root this machine does not trust (0x800B0109). The
    script reports both codes and the two supported ways forward.

.PARAMETER Register
    Register the staged manifest for the current user with -ExternalLocation.
    Requires Developer Mode. Keeps <install dir>\pkg as the stage.

.PARAMETER Install
    Register the packed .appx for the current user with -ExternalLocation.
    Requires the package to be signed by a certificate this machine trusts.

.PARAMETER Uninstall
    Remove the package from the current user. Does nothing if it is absent.

.PARAMETER Sign
    Sign the packed .appx with signtool.exe. Requires -CertThumbprint.

.PARAMETER InstallDir
    Install directory holding illness.exe and appx\illness_explorer_command_injector.dll.
    Defaults to $env:ProgramFiles\Illness.

.PARAMETER CertThumbprint
    Thumbprint of an already-installed code-signing certificate for -Sign.

.EXAMPLE
    pwsh -NoProfile -File packaging/msix/build.ps1

.EXAMPLE
    pwsh -NoProfile -File packaging/msix/build.ps1 -Register -InstallDir 'D:\Apps\Illness'

.EXAMPLE
    pwsh -NoProfile -File packaging/msix/build.ps1 -Sign -CertThumbprint 0123456789ABCDEF0123456789ABCDEF01234567

.EXAMPLE
    pwsh -NoProfile -File packaging/msix/build.ps1 -Install -InstallDir 'D:\Apps\Illness'
#>
[CmdletBinding()]
param(
    [switch]$Register,
    [switch]$Install,
    [switch]$Uninstall,
    [switch]$Sign,
    [string]$InstallDir = (Join-Path $env:ProgramFiles 'Illness'),
    [string]$CertThumbprint
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$PackageName = 'Simulp.Illness'
$InjectorPackage = 'illness_explorer_command_injector'
$AppxName = "$InjectorPackage.appx"
$DllName = "$InjectorPackage.dll"

$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$ManifestDir = $PSScriptRoot
$DistDir = Join-Path $RepoRoot 'dist'
$AppxPath = Join-Path $DistDir $AppxName
$BuiltDll = Join-Path $RepoRoot "target\release\$DllName"

function Resolve-AbsolutePath {
    param([string]$Path, [string]$What = 'path')
    if ([string]::IsNullOrWhiteSpace($Path)) {
        throw "No $What was given."
    }
    $candidate = $Path
    if (-not [System.IO.Path]::IsPathRooted($candidate)) {
        $candidate = Join-Path (Get-Location).ProviderPath $candidate
    }
    # Add-AppxPackage rejects relative paths with "Invalid URI"; resolve fully.
    return [System.IO.Path]::GetFullPath($candidate)
}

function Find-SdkTool {
    param([string]$Name)
    $kitsBin = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10\bin'
    if (-not (Test-Path -LiteralPath $kitsBin)) {
        throw "Windows SDK not found at '$kitsBin'. Install the Windows 10/11 SDK ($Name ships with it) and re-run."
    }
    # Newest SDK version first; the tools themselves sit in the x64 folder.
    $candidates = Get-ChildItem -LiteralPath $kitsBin -Directory |
        Sort-Object -Property Name -Descending |
        ForEach-Object { Join-Path $_.FullName "x64\$Name" } |
        Where-Object { Test-Path -LiteralPath $_ }
    if (-not $candidates) {
        throw "$Name not found under '$kitsBin'. Install the Windows 10/11 SDK 'Windows SDK for Desktop C++ x64 Apps' component and re-run."
    }
    return @($candidates)[0]
}

function Invoke-Native {
    param([string]$FilePath, [string[]]$Arguments, [string]$What)
    if ([string]::IsNullOrWhiteSpace($What)) {
        $What = $FilePath
    }
    & $FilePath @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "$What failed with exit code $LASTEXITCODE."
    }
}

function Build-Injector {
    Push-Location $RepoRoot
    try {
        Invoke-Native -FilePath 'cargo' -Arguments @('build', '--release', '-p', $InjectorPackage) `
            -What "cargo build --release -p $InjectorPackage"
    }
    finally {
        Pop-Location
    }
    if (-not (Test-Path -LiteralPath $BuiltDll)) {
        throw "Expected the built COM server at '$BuiltDll', but it is missing."
    }
}

function Copy-PackageFiles {
    param([string]$Destination)
    New-Item -ItemType Directory -Force -Path $Destination | Out-Null
    $manifest = Join-Path $ManifestDir 'AppxManifest.xml'
    $resources = Join-Path $ManifestDir 'resources'
    if (-not (Test-Path -LiteralPath $manifest)) {
        throw "Manifest '$manifest' is missing."
    }
    if (-not (Test-Path -LiteralPath $resources)) {
        throw "Resources directory '$resources' is missing."
    }
    Copy-Item -LiteralPath $manifest -Destination $Destination -Force
    Copy-Item -LiteralPath $resources -Destination $Destination -Recurse -Force
}

function New-SparsePackage {
    $makeAppx = Find-SdkTool -Name 'makeappx.exe'
    Write-Host "Using $makeAppx"

    New-Item -ItemType Directory -Force -Path $DistDir | Out-Null

    $stage = Join-Path ([System.IO.Path]::GetTempPath()) ("illness-msix-" + [guid]::NewGuid().ToString('N'))
    try {
        Copy-PackageFiles -Destination $stage
        if (Test-Path -LiteralPath $AppxPath) {
            Remove-Item -LiteralPath $AppxPath -Force
        }
        Invoke-Native -FilePath $makeAppx -Arguments @('pack', '/d', $stage, '/p', $AppxPath, '/nv') `
            -What 'makeappx pack' | Out-Null
    }
    finally {
        if (Test-Path -LiteralPath $stage) {
            Remove-Item -LiteralPath $stage -Recurse -Force
        }
    }

    if (-not (Test-Path -LiteralPath $AppxPath)) {
        throw "makeappx reported success but '$AppxPath' does not exist."
    }
    Write-Host "Package written to $AppxPath"
    return $AppxPath
}

function Get-ExternalLocation {
    param([string]$ResolvedInstallDir)
    $external = Join-Path $ResolvedInstallDir 'appx'
    if (-not (Test-Path -LiteralPath $external)) {
        throw "Expected the external content directory '$external'. Copy the COM server there or install the app into '$ResolvedInstallDir'."
    }
    return $external
}

function Assert-InstallLayout {
    param([string]$ResolvedInstallDir)
    $exe = Join-Path $ResolvedInstallDir 'illness.exe'
    if (-not (Test-Path -LiteralPath $exe)) {
        throw "Expected illness.exe at '$exe'. Install the app into '$ResolvedInstallDir' or pass -InstallDir."
    }
    $dll = Join-Path (Join-Path $ResolvedInstallDir 'appx') $DllName
    if (-not (Test-Path -LiteralPath $dll)) {
        throw "Expected the COM server at '$dll'. Copy it from target\release into '$ResolvedInstallDir\appx' or install the app there."
    }
}

function Register-LoosePackage {
    param([string]$ResolvedInstallDir)
    Assert-InstallLayout -ResolvedInstallDir $ResolvedInstallDir
    $external = Get-ExternalLocation -ResolvedInstallDir $ResolvedInstallDir

    # Registered manifests stay referenced by location, so the stage lives in
    # the install directory instead of a temporary folder.
    $stage = Join-Path $ResolvedInstallDir 'pkg'
    Copy-PackageFiles -Destination $stage
    $stage = (Resolve-Path -LiteralPath $stage).Path

    Write-Host "Registering loose package: $stage\AppxManifest.xml"
    Write-Host "External location:        $external"
    try {
        Add-AppxPackage -Register (Join-Path $stage 'AppxManifest.xml') -ExternalLocation $external
    }
    catch {
        throw @"
Add-AppxPackage -Register failed for '$stage\AppxManifest.xml' (external location '$external'): $($_.Exception.Message)
-Register needs Windows Developer Mode (Settings > System > For developers > Developer Mode) and needs '$ResolvedInstallDir' to contain illness.exe plus appx\$DllName.
"@
    }

    Confirm-Registration
    Write-Host "Local development registration only. For distribution use a signed package with -Install."
}

function Install-Package {
    param([string]$ResolvedInstallDir)
    if (-not (Test-Path -LiteralPath $AppxPath)) {
        throw "Package '$AppxPath' is missing. Run this script without switches to build it first."
    }
    Assert-InstallLayout -ResolvedInstallDir $ResolvedInstallDir
    $external = Get-ExternalLocation -ResolvedInstallDir $ResolvedInstallDir

    $message = $null
    try {
        Add-AppxPackage -Path $AppxPath -ExternalLocation $external
    }
    catch {
        $message = $_.Exception.Message
        $inner = $_.Exception.InnerException
        while ($inner) {
            $message = "$message $($inner.Message)"
            $inner = $inner.InnerException
        }
    }

    if ($message) {
        # 0x800B0100 TRUST_E_NOSIGNATURE, 0x800B0109 CERT_E_UNTRUSTEDROOT.
        if ($message -match '0x800B0100|0x800B0109|-2146762496|-2146762487' -or
            $message -match 'signature|not trusted|certificate|trust') {
            Write-Host ''
            Write-Host "Add-AppxPackage rejected '$AppxPath' (external location '$external'):"
            Write-Host "  $message"
            Write-Host ''
            Write-Host 'Windows only installs a packed sparse package that it trusts. Two supported options:'
            Write-Host '  1. Enable Developer Mode and register the loose manifest instead:'
            Write-Host "       pwsh -NoProfile -File packaging/msix/build.ps1 -Register -InstallDir '$ResolvedInstallDir'"
            Write-Host '  2. Sign the package with a code-signing certificate this machine trusts'
            Write-Host '     (see the Signing section of packaging/msix/README.md), then either:'
            Write-Host "       pwsh -NoProfile -File packaging/msix/build.ps1 -Sign -CertThumbprint <thumbprint>"
            Write-Host "       pwsh -NoProfile -File packaging/msix/build.ps1 -Install -InstallDir '$ResolvedInstallDir'"
            throw "Add-AppxPackage rejected the package signature: $message"
        }
        throw "Add-AppxPackage failed for '$AppxPath' (external location '$external'): $message"
    }

    Confirm-Registration
}

function Confirm-Registration {
    $package = Get-AppxPackage -Name $PackageName
    if (-not $package) {
        throw "Add-AppxPackage returned without error but Get-AppxPackage -Name $PackageName found nothing."
    }
    Write-Host "Registered $($package.Name) $($package.Version) [$($package.PackageFullName)]"
    Write-Host "Install location: $($package.InstallLocation)"
    Write-Host "Status: $($package.Status)"
    if ($package.Status -ne 'Ok') {
        throw "$PackageName registered with status '$($package.Status)' instead of 'Ok'."
    }
}

function Uninstall-Package {
    $packages = @(Get-AppxPackage -Name $PackageName)
    if (-not $packages) {
        Write-Host "$PackageName is not installed for this user; nothing to remove."
        return
    }
    foreach ($package in $packages) {
        try {
            Remove-AppxPackage -Package $package.PackageFullName
        }
        catch {
            throw "Remove-AppxPackage failed for '$($package.PackageFullName)': $($_.Exception.Message)"
        }
    }
    if (Get-AppxPackage -Name $PackageName) {
        throw "$PackageName is still registered after Remove-AppxPackage."
    }
    Write-Host "Removed $PackageName."
}

function Sign-Package {
    if ([string]::IsNullOrWhiteSpace($CertThumbprint)) {
        throw "-Sign needs -CertThumbprint <thumbprint> of a code-signing certificate already installed on this machine. The script never creates certificates."
    }
    if (-not (Test-Path -LiteralPath $AppxPath)) {
        throw "Package '$AppxPath' is missing. Run this script without switches to build it first."
    }
    $signTool = Find-SdkTool -Name 'signtool.exe'
    Write-Host "Using $signTool"
    $thumb = ($CertThumbprint -replace '\s', '')
    Invoke-Native -FilePath $signTool `
        -Arguments @('sign', '/sha1', $thumb, '/fd', 'SHA256', '/td', 'SHA256', $AppxPath) `
        -What 'signtool sign'
    Write-Host "Signed $AppxPath with certificate $thumb"
}

$requested = @()
if ($Register) { $requested += '-Register' }
if ($Install) { $requested += '-Install' }
if ($Uninstall) { $requested += '-Uninstall' }
if ($Sign) { $requested += '-Sign' }
if ($requested.Count -gt 1) {
    throw "Pass at most one of -Register, -Install, -Uninstall and -Sign (got $($requested -join ', '))."
}

try {
    if ($Uninstall) {
        Uninstall-Package
        exit 0
    }

    $resolvedInstallDir = Resolve-AbsolutePath -Path $InstallDir -What 'install directory'

    if ($Sign) {
        if (-not (Test-Path -LiteralPath $AppxPath)) {
            New-SparsePackage | Out-Null
        }
        Sign-Package
        exit 0
    }

    if ($Register) {
        Register-LoosePackage -ResolvedInstallDir $resolvedInstallDir
        exit 0
    }

    if ($Install) {
        Install-Package -ResolvedInstallDir $resolvedInstallDir
        exit 0
    }

    Build-Injector
    $packed = New-SparsePackage
    Write-Host ''
    Write-Host "Packed: $packed"
    Write-Host "Local development install: pwsh -NoProfile -File packaging/msix/build.ps1 -Register -InstallDir '$resolvedInstallDir'"
    Write-Host 'Distribution install:      pwsh -NoProfile -File packaging/msix/build.ps1 -Sign -CertThumbprint <thumbprint> then -Install'
}
catch {
    Write-Error $_.Exception.Message -ErrorAction Continue
    exit 1
}
