#!/usr/bin/env pwsh
# Build the Windows NSIS installer for Illness into dist/.
# The installer is per-machine: running it prompts for UAC elevation.
#
# Explorer integration is per-user and registered by the application on every
# normal launch, using its own executable path. A first launch from
# $INSTDIR therefore points the verbs at the installed illness.exe. The NSIS
# uninstaller does not execute the app, so run
#   "$INSTDIR\illness.exe" --unregister-context-menu
# before uninstalling if the "Open with Illness" verbs should be removed too.
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
