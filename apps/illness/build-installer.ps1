#!/usr/bin/env pwsh
# Build the Windows NSIS installer for Illness into dist/.
# The installer is per-machine: running it prompts for UAC elevation.
#
# Explorer integration is per-user. A normal launch registers it only from a
# stable installed location (Program Files or %LOCALAPPDATA%\Programs); a
# debug/temporary/USB copy will not repoint an installed menu. Use
#   "$INSTDIR\illness.exe" --register-context-menu
# to register deliberately from another location, and
#   "$INSTDIR\illness.exe" --unregister-context-menu
# to remove it for the current user. The per-machine uninstaller also removes
# the verbs from HKCU and every loaded HKEY_USERS hive before deleting
# $INSTDIR, so no next launch is required.
$ErrorActionPreference = 'Stop'

if (-not (Get-Command cargo-packager -ErrorAction SilentlyContinue)) {
    throw @'
cargo-packager is not installed.
Install version 0.11.8 first:
  cargo install cargo-packager --version 0.11.8 --locked
'@
}

# Run from this directory so cargo-packager discovers only this Packager.toml.
Push-Location $PSScriptRoot
try {
    cargo packager --release
} finally {
    Pop-Location
}
