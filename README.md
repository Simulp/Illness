# ILLNESS

### The Terminal Emulator for the Terminally Ill.

A Windows-first terminal emulator built on Alacritty's VT core and egui, for people who think
Alacritty's UI is too basic.

Illness keeps the parts of Alacritty that are actually good: the parser, grid, scrollback,
selection and ConPTY -- and replaces the window you have to look at with a clean, dark, tabbed
shell inspired by Windows Terminal. It renders terminal text through its own physical-pixel
glyph atlas: hinted, pixel-snapped, nearest-sampled, with neutral grayscale
antialiasing (no ClearType color fringing).

<p align="center"><img src="assets/illness.png" width="96" alt="Illness logo"></p>

![Illness terminal under Acrylic](assets/ls_acrylic.jpg)

![Command palette](assets/palette.jpg)

![Settings - appearance and terminal](assets/settings_1.jpg)

![Settings - behavior, profiles and about](assets/settings_2.jpg)

Every view under the three window materials:

| | Solid | Mica | Acrylic |
| --- | --- | --- | --- |
| Directory listing | ![Solid](assets/ls_solid.jpg) | ![Mica](assets/ls_mica.jpg) | ![Acrylic](assets/ls_acrylic.jpg) |
| Coding agent | ![Solid](assets/oc_solid.jpg) | ![Mica](assets/oc_mica.jpg) | ![Acrylic](assets/oc_acrylic.jpg) |
| Close confirmation | ![Solid](assets/exit_solid.jpg) | ![Mica](assets/exit_mica.jpg) | ![Acrylic](assets/exit_acrylic.jpg) |

![New-tab dropdown with detected profiles](assets/dropdown.jpg)

## Why?

To solve a personal problem: There's nothing good in the Windows terminal scene. During my search, it was either bloated Electron or barebones as hell. There's also a long-standing [issue](https://github.com/microsoft/terminal/issues/11916) where Windows Terminal bogs down the moment any CPU-intensive task such as software compilation is done.

## AI Disclosure

DeepSeek v4.1 Flash 1017 used extensively for development.

### Latency workarounds

Illness pins the pseudoconsole API to the inbox `conhost.exe`. `alacritty_terminal` loads
`conpty.dll` by bare name, and a Windows Terminal or WezTerm install can put its own
`conpty.dll` on `PATH`; that host, `OpenConsole.exe`, echoes keystrokes far more slowly. The DLL
search is restricted to `System32` so the load falls back to kernel32's `CreatePseudoConsole`.
The restriction is process-global: a `conpty.dll` shipped next to `illness.exe` is ignored.
Set `ILLNESS_CONPTY=auto` to leave the DLL search alone.

Illness raises the shell's own threads and its console host to above-normal priority so a saturated
machine still echoes at a usable latency (about 302 ms unboosted versus 30 ms under 8/8 CPU
saturation). The shell is boosted per thread, not per process: thread priority is not inherited,
so long builds started inside Illness keep running at normal priority. `ILLNESS_NO_PRIORITY_BOOST=1`
(unset, empty, `0`, `false`, `no` or `off` keeps the boost) disables it.

Echo latency is measured with `crates/illness-term/examples/latency_probe.rs`.

Known limitations in this dose: no split panes, no sixel/kitty graphics, no vi mode, and no
tested Unix build. Mica is the Windows 11 system backdrop. Acrylic uses the legacy native
behind-window accent blur where Windows 10 and 11 expose it; either material falls back to
Solid if its API is unavailable. The VT core is Alacritty's, so the usual escape-sequence
coverage applies.

## Symptoms

- Custom winit/egui window host with a frameless window, a real tab strip, profile accents and
  native caption buttons. Drag tabs to reorder them: the tab follows the pointer, leaves a
  landing slot, autoscrolls an overflowing strip, and activates when dropped. Escape cancels;
  dropping outside the titlebar restores the original order.
- Settings opens as its own tab, with right-aligned dropdowns, an editable shell profile list,
  an installed-font picker, and background controls.
- Solid by default; Mica or Acrylic in Settings. Mica is the Windows 11 system backdrop and
  falls back to Solid when unavailable. Acrylic uses the legacy native behind-window accent
  blur where Windows 10/11 expose it, and also falls back to Solid. Acrylic's Desktop mode is
  native behind-window blur; Mica's Desktop mode paints a decoded, blurred snapshot of the
  current wallpaper that is refreshed when the backdrop is rebuilt, not continuously live.
  Image uses the chosen custom image. Wallpapers are decoded and blurred on a detached thread;
  Solid and fallback Solid never decode one, Acrylic's Desktop mode prewarms for a later switch,
  and a cold Mica/Image switch paints the native material until the warmed texture uploads.
  The opacity slider affects a painted backdrop (Mica with Desktop or a loaded image) and
  Acrylic (including Acrylic with Background set to None, through its accent tint); Solid,
  Mica with no background, and Mica with an image that failed to load ignore it. Native
  materials also require Windows transparency effects to be on; otherwise Illness stays Solid
  but keeps the request in Settings as "(unavailable)". Turning transparency effects back on,
  or re-selecting the material, retries it without a restart.
- Command palette (`Ctrl+Shift+P`), inline search (`Ctrl+Shift+F`) and Settings (`Ctrl+,`).
- Seven themes: Illness Dark, Campbell, One Half Dark, Nord, Gruvbox Dark, Solarized Dark, Light.
- Crisp terminal grid with system fallback for CJK and emoji; wide cells, combining marks,
  underline/strikethrough, OSC 8 hyperlinks and all cursor shapes.
- Full xterm key and mouse encoding, bracketed paste, IME commit, focus reporting and DECSCUSR.
- 10,000 lines of scrollback, smooth wheel scrolling, word/line selection and a jump-to-bottom pill.
- A shell that exits normally after use closes its tab (and the window when it was the last tab).
  A profile that fails immediately keeps its tab and shows a Restart / Settings / Close recovery
  card, so a missing shell can be fixed without relaunching.
- Pixel-snapped, hinted glyphs on a nearest-sampled physical-pixel atlas. Glyphs are drawn on
  the GPU from a grayscale mask atlas (plus color emoji), while backgrounds, selection, search
  hits and the cursor stay pixel-exact; the CPU grayscale compositor remains as a fallback.

## Dosage

Requires Windows 10 1809+ and a Rust 1.98 toolchain with the MSVC target.

```pwsh
cargo run -p illness
```

Release build:

```pwsh
cargo build --release -p illness
```

The binary is `target\release\illness.exe`. Release builds use the Windows GUI subsystem, so
double-clicking it opens Illness directly instead of hosting a console in Windows Terminal.

### Opening a folder

```pwsh
illness.exe "E:\Files"
illness.exe C:\CLI
```

The positional folder is the launch folder. With **New tabs inherit current folder** enabled
(the default, `inherit_cwd = true` in `%APPDATA%\illness\config.toml`), a new tab starts in the
active shell's current directory; otherwise it uses the launch folder. The setting is captured
when a tab is spawned: enabling it later does not retrofit already-open tabs.

Launched without a folder — a Start Menu entry, a desktop shortcut, or the installed executable —
Illness starts in `%USERPROFILE%` (`C:\Users\<you>`). Explorer and shortcuts set the process
directory to the executable's own folder, which says nothing about where you were working, so that
value is ignored there. A real console launch from another folder is honoured.

PowerShell 7 syncs its cwd through `LocationChangedAction` without touching `prompt`. Windows
PowerShell 5.1 has no equivalent hook, so Illness only wraps the built-in/default prompt there.
A custom 5.1 prompt is left untouched, which preserves its `$?` but means its folder may not be
inherited; the tab keeps the last known folder instead. A cheap PEB read supplies the live shell
cwd; every folder probe and shell attempt then runs on the spawn worker. Local candidates are
checked inline on that worker; only network paths use a small probe pool, so an unrelated local path
never waits behind a dead UNC share. A per-tab deadline and cancellation token mean a closed or
restarted tab stops waiting, and a shell that has not started within ten seconds becomes a visible
recovery card instead of a blank tab. A shell that has `cd`ed to a deleted folder or offline UNC
share falls back, while a cold launch folder survives a probe timeout. Keys typed while a shell is
starting are buffered and delivered when it attaches.

### Explorer context menu

The per-machine installer adds **Open with Illness** verbs for folder backgrounds, selected folders
and drive roots (no file association) under `HKLM\Software\Classes`, so they work for every user
without a first launch. While those machine verbs are complete, they are authoritative: a normal
launch deletes the current user's stale HKCU copy so an older portable install cannot shadow HKLM.
If HKLM is missing, partial, or names a deleted executable, a launch from a stable installed
location repairs the user's menu with a per-user HKCU copy. A normal launch only touches the menu
when running from a stable installed location (`%ProgramFiles%`, `%ProgramFiles(x86)%`, or
`%LOCALAPPDATA%\Programs`); running a `target\debug`, temporary or USB copy will not repoint an
installed menu. Explicitly register or remove the current copy with:

```pwsh
illness.exe --register-context-menu
illness.exe --unregister-context-menu
```

An explicit registration also repairs a stale shadowing HKCU entry when complete machine verbs
exist. The installer and uninstaller write/remove HKLM and clean HKCU plus every loaded
`HKEY_USERS` hive, so upgrades and uninstalls do not leave other logged-on users' old verbs behind.
An offline account's hive is not mounted; its stale HKCU entry is removed on that user's next launch
once HKLM is complete. `--unregister-context-menu` removes the HKCU fallback and any HKLM keys; run it
elevated for the latter.

### Windows installer

Build a machine-wide NSIS installer with [cargo-packager](https://github.com/crabnebula-dev/cargo-packager) 0.11.8:

```pwsh
cargo install cargo-packager --version 0.11.8 --locked   # once
.\apps\illness\build-installer.ps1
```

The result is `dist\illness_0.1.0_x64-setup.exe`. It installs to `%ProgramFiles%\Illness` for all
users (UAC prompt), creates an All Users Start Menu shortcut, and registers an uninstaller in
Programs and Features. Settings remain per-user in `%APPDATA%\illness`; the uninstaller offers to
remove them but keeps them by default. Before deleting the install directory, the uninstaller
removes the machine-wide Explorer verbs from `HKLM\Software\Classes`, plus HKCU and every loaded
`HKEY_USERS` hive so other logged-on users' legacy per-user verbs do not survive. It does not depend
on the app being launched again; an offline user's hive is repaired by the app on that user's next
launch while the complete HKLM menu is authoritative.

## Prescription

| Action | Shortcut |
| --- | --- |
| New tab | `Ctrl+Shift+T` |
| Close tab | `Ctrl+Shift+W` |
| Next / previous tab | `Ctrl+Tab` / `Ctrl+Shift+Tab` |
| Select tab 1–9 | `Ctrl+1` … `Ctrl+9` |
| Command palette | `Ctrl+Shift+P` |
| Find | `Ctrl+Shift+F` |
| Settings | `Ctrl+,` |
| Copy / paste | `Ctrl+Shift+C` / `Ctrl+Shift+V` |
| Font size | `Ctrl+=` / `Ctrl+-` / `Ctrl+0` |
| Scroll | Wheel, `Shift+PageUp/PageDown` |

## Side effects

Configuration lives at `%APPDATA%\illness\config.toml` and is written on exit. Profiles are
detected from the shells installed on the machine: PowerShell 7, Windows PowerShell, Command
Prompt and WSL.

## Prognosis

Apache-2.0 OR MIT. See `NOTICE` for upstream attribution and the licenses in `assets/fonts/`
for Cascadia Code and Inter.
