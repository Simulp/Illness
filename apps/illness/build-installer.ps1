#!/usr/bin/env pwsh
# Build the Windows NSIS installer for Illness into dist/.
# The installer is per-machine: running it prompts for UAC elevation.
#
# Explorer integration follows the install scope. The per-machine installer
# writes HKLM\Software\Classes verbs and cleans legacy HKCU and loaded
# HKEY_USERS verbs; the uninstaller removes HKLM, HKCU, and loaded user-hive
# copies. A portable or current-user copy registers HKCU only when complete
# HKLM verbs do not exist. A normal launch repairs HKCU only from a stable
# installed location (Program Files or %LOCALAPPDATA%\Programs); a
# debug/temporary/USB copy will not repoint an installed menu. Use
#   "$INSTDIR\illness.exe" --register-context-menu
# to register deliberately from another location, and
#   "$INSTDIR\illness.exe" --unregister-context-menu
# to remove the HKCU fallback and, when present and permitted, the HKLM verbs.
$ErrorActionPreference = 'Stop'

if (-not (Get-Command cargo-packager -ErrorAction SilentlyContinue)) {
    throw @'
cargo-packager is not installed.
Install version 0.11.8 first:
  cargo install cargo-packager --version 0.11.8 --locked
'@
}

# Rust embeds the absolute path of every panic site. Without remapping, the
# shipped exe contains C:\Users\<you>\.cargo\registry\... and .rustup\... from
# dependency panic messages, which leaks the build machine's user name.
# CARGO_ENCODED_RUSTFLAGS is used because plain RUSTFLAGS is split on spaces and
# a profile path can contain them.
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$remaps = @(
    "--remap-path-prefix=$env:USERPROFILE\.cargo=C:\cargo",
    "--remap-path-prefix=$env:USERPROFILE\.rustup=C:\rust",
    "--remap-path-prefix=$repoRoot=."
)
$env:CARGO_ENCODED_RUSTFLAGS = ($remaps -join [char]0x1f)
if ($env:RUSTFLAGS) {
    throw 'RUSTFLAGS is set; unset it so the path remap is the only flag set.'
}

# The sparse MSIX package that adds "Open with Illness" to the Windows 11
# Explorer context menu rides along in <install dir>\appx, together with the COM
# server DLL. Build both before packaging; Packager.toml copies them from
# dist\ and target\release. The package needs makeappx.exe from the Windows
# SDK, which not every build machine has: without it a previously built .appx
# is reused and the installer simply keeps the classic registry verbs.
$msixBuild = Join-Path $repoRoot 'packaging\msix\build.ps1'
$appxPath = Join-Path $repoRoot 'dist\illness_explorer_command_injector.appx'
$sdkBin = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10\bin'
$hasMakeAppx = (Test-Path -LiteralPath $sdkBin) -and [bool](
    Get-ChildItem -LiteralPath $sdkBin -Directory -ErrorAction SilentlyContinue |
        Where-Object { Test-Path -LiteralPath (Join-Path $_.FullName 'x64\makeappx.exe') }
)
if ($hasMakeAppx) {
    # The MSIX build compiles the injector, so it inherits the remapped flags.
    $shell = if (Get-Command pwsh -ErrorAction SilentlyContinue) { 'pwsh' } else { 'powershell' }
    & $shell -NoProfile -File $msixBuild
    if ($LASTEXITCODE -ne 0) {
        throw "packaging/msix/build.ps1 failed with exit code $LASTEXITCODE; cannot package the Explorer context menu."
    }
}
else {
    Write-Warning "Windows SDK (makeappx.exe) not found; skipping packaging/msix/build.ps1. Reusing any existing .appx."
}
if (-not (Test-Path -LiteralPath $appxPath)) {
    throw @"
Missing Explorer context menu package: $appxPath
Install the Windows 10/11 SDK (makeappx.exe) so packaging/msix/build.ps1 can create it, or build it first:
  packaging/msix/build.ps1
"@
}

# Run from this directory so cargo-packager discovers only this Packager.toml.
Push-Location $PSScriptRoot
try {
    cargo packager --release
} finally {
    Pop-Location
}
