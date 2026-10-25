# ILLNESS

### The Terminal Emulator for the Terminally Ill.

A Windows-first terminal emulator built on Alacritty's VT core and egui, for people who think
Alacritty's UI is a war crime.

Illness keeps the parts of Alacritty that are actually good - the parser, grid, scrollback,
selection and ConPTY - and replaces the window you have to look at with a clean, dark, tabbed
shell inspired by Windows Terminal. It renders terminal text through its own physical-pixel
glyph atlas: hinted, pixel-snapped, nearest-sampled, with neutral grayscale
antialiasing (no ClearType color fringing).

![Illness](assets/screenshot.png)

![Illness command palette](assets/palette.png)

![Illness settings tab](assets/settings.png)

## Symptoms

- Custom winit/egui window host with a frameless window, a real tab strip, profile accents and
  native caption buttons.
- Settings opens as its own tab, with right-aligned dropdowns, an editable shell profile list,
  an installed-font picker (Nerd Fonts included), and background controls.
- Mica by default; Acrylic or Solid in Settings. Mica is the Windows 11 system backdrop and
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

PowerShell 7 syncs its cwd through `LocationChangedAction` without touching `prompt`. Windows
PowerShell 5.1 has no equivalent hook, so Illness only wraps the built-in/default prompt there.
A custom 5.1 prompt is left untouched, which preserves its `$?` but means its folder may not be
inherited; the tab keeps the last known folder instead. Stored and launch-folder checks run on a
detached thread with a short deadline, so an offline UNC share falls back instead of freezing the
UI.

### Explorer context menu

Illness adds per-user **Open with Illness** verbs for folder backgrounds, selected folders and
drive roots (no file association). A normal launch only updates them when running from a stable
installed location (`%ProgramFiles%`, `%ProgramFiles(x86)%`, or `%LOCALAPPDATA%\Programs`);
running a `target\debug`, temporary or USB copy will not repoint an installed menu. Explicitly
register or remove the current copy with:

```pwsh
illness.exe --register-context-menu
illness.exe --unregister-context-menu
```

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
removes the per-user Explorer verbs from HKCU and every loaded `HKEY_USERS` hive, so it does not
depend on the app being launched again.

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

Known limitations in this dose: no split panes, no sixel/kitty graphics, no vi mode, and no
tested Unix build. Mica is the Windows 11 system backdrop. Acrylic uses the legacy native
behind-window accent blur where Windows 10 and 11 expose it; either material falls back to
Solid if its API is unavailable. The VT core is Alacritty's, so the usual escape-sequence
coverage applies.

## Prognosis

Apache-2.0 OR MIT. See `NOTICE` for upstream attribution and the licenses in `assets/fonts/`
for Cascadia Code and Inter.
