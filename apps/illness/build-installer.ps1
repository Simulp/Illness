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

# Run from this directory so cargo-packager discovers only this Packager.toml.
Push-Location $PSScriptRoot
try {
    cargo packager --release
} finally {
    Pop-Location
}
