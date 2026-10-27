# Explorer context menu package

Sparse MSIX package that puts **Open with Illness** in the Windows 11 modern
Explorer context menu for folders, folder backgrounds and drives.

The modern menu is only reachable through a package: the manifest declares
`windows.fileExplorerContextMenus` verbs and a `windows.comServer` surrogate,
both backed by `illness_explorer_command_injector.dll`. The legacy
registry-based verb that the installer writes still only shows up under
**Show more options**.

## What is in the package

The package ships no binaries. `uap10:AllowExternalContent=true` lets it point
at the normal install directory, so the install directory must contain:

- `illness.exe` — the executable declared by the package, resolved from the
  external location.
- `appx\illness_explorer_command_injector.dll` — the COM server
  (`clsid 8c4f2b16-7d3a-4e58-9b71-2f6c9d0a5e43`) the surrogate server loads
  when a verb is clicked. The installer keeps the DLL in the `appx`
  subdirectory, so that path is what an install has to preserve.

Identity: `Simulp.Illness`, publisher `CN=Simulp`, version `0.1.0.0`.

## Build

```powershell
pwsh -NoProfile -File packaging/msix/build.ps1
```

Builds `illness_explorer_command_injector.dll` (`cargo build --release -p
illness_explorer_command_injector`), stages the manifest plus `resources\`,
packs with `makeappx pack /nv`, deletes the stage, and prints the result:
`dist\illness_explorer_command_injector.appx`.

Requires the Windows 10/11 SDK for `makeappx.exe` and `signtool.exe`; the script
looks under `%ProgramFiles(x86)%\Windows Kits\10\bin\*\x64` (newest SDK first)
and stops with a clear message if a tool is missing. Every tool call is checked
against `$LASTEXITCODE`.

## Switches

| Switch | What it does |
| --- | --- |
| *(none)* | Build the DLL, pack the `.appx`, print its path. |
| `-Register` | Register the loose manifest (Developer Mode path). |
| `-Install` | Register the packed `.appx` (signed path). |
| `-Sign` | Sign the packed `.appx` with `signtool`. |
| `-Uninstall` | Remove the package for the current user. |
| `-InstallDir <dir>` | Install directory, default `%ProgramFiles%\Illness`. |
| `-CertThumbprint <thumb>` | Certificate for `-Sign`, required there. |

Pass at most one of `-Register`, `-Install`, `-Uninstall`, `-Sign`.
`-InstallDir` is resolved to an absolute path because `Add-AppxPackage` rejects
relative paths.

## Local development: `-Register`

```powershell
pwsh -NoProfile -File packaging/msix/build.ps1 -Register
pwsh -NoProfile -File packaging/msix/build.ps1 -Register -InstallDir 'D:\Apps\Illness'
```

This needs **Windows Developer Mode** (Settings → System → For developers →
Developer Mode). It stages the manifest plus `resources\` into
`<install dir>\pkg` and runs:

```powershell
Add-AppxPackage -Register '<dir>\pkg\AppxManifest.xml' -ExternalLocation '<dir>\appx'
```

The stage is kept because a registered loose package is referenced by location;
deleting it breaks the registration. The install directory must contain
`illness.exe` and `appx\illness_explorer_command_injector.dll`; the script
refuses to run otherwise. It finishes by printing `Get-AppxPackage -Name
Simulp.Illness` with `Status` (and fails if the status is not `Ok`).

## Packed install: `-Install`

```powershell
pwsh -NoProfile -File packaging/msix/build.ps1 -Sign -CertThumbprint <thumbprint>
pwsh -NoProfile -File packaging/msix/build.ps1 -Install
pwsh -NoProfile -File packaging/msix/build.ps1 -Install -InstallDir 'D:\Apps\Illness'
```

Runs `Add-AppxPackage -Path <appx> -ExternalLocation <dir>\appx` and verifies
with `Get-AppxPackage -Name Simulp.Illness`.

Windows refuses an unsigned package (`0x800B0100`) and a package signed by a
self-signed certificate whose root this machine does not trust (`0x800B0109`).
On either code the script prints the two supported options: enable Developer
Mode and use `-Register`, or sign with a certificate the machine trusts.

## Uninstall

```powershell
pwsh -NoProfile -File packaging/msix/build.ps1 -Uninstall
```

Runs `Remove-AppxPackage` for `Simulp.Illness` and reports that nothing was
installed when the package is absent. Nothing is written to the registry by any
switch.

## Signing

`-Sign -CertThumbprint <thumbprint>` only signs; it never creates a
certificate, because trusting a self-signed root is a decision for the machine's
owner. Manual equivalent:

```powershell
signtool sign /sha1 <thumbprint> /fd SHA256 /td SHA256 dist\illness_explorer_command_injector.appx
```

For a package that installs on someone else's machine you need a real
code-signing certificate whose subject matches the manifest `Publisher`
(`CN=Simulp`) and whose chain Windows already trusts — or a Store listing, which
re-signs the package for you. Only a certificate with that exact subject can be
used, so a generic test certificate will fail signature validation even when its
root is trusted. Developer Mode plus `-Register` is the local development path
and is not distributable.
