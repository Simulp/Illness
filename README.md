# ILLNESS

### The Terminal Emulator for the Terminally Ill.

A Windows-first terminal emulator built on Alacritty's VT core and egui, for people who think
Alacritty's UI is a war crime.

Illness keeps the parts of Alacritty that are actually good — the parser, grid, scrollback,
selection and ConPTY — and replaces the window you have to look at with a clean, dark, tabbed
shell inspired by Windows Terminal. It renders terminal text through its own physical-pixel
glyph atlas: hinted, pixel-snapped, nearest-sampled, with no subpixel smearing.

![Illness](assets/screenshot.png)

![Illness command palette](assets/palette.png)

## Symptoms

- Custom frameless window with a real tab strip, profile accents and native caption buttons.
- Mica by default; Acrylic or Solid in Settings. Windows 10 falls back to Solid. Mica and
  Acrylic render a blurred wallpaper backdrop inside the window, because the GPU surface does
  not expose native DWM transparency.
- Command palette (`Ctrl+Shift+P`), inline search (`Ctrl+Shift+F`) and a live Settings sheet
  (`Ctrl+,`).
- Seven themes: Illness Dark, Campbell, One Half Dark, Nord, Gruvbox Dark, Solarized Dark, Light.
- Crisp Cascadia Mono grid with system fallback for CJK and emoji; wide cells, combining marks,
  underline/strikethrough, OSC 8 hyperlinks and all cursor shapes.
- Full xterm key and mouse encoding, bracketed paste, IME commit, focus reporting and DECSCUSR.
- 10,000 lines of scrollback, smooth wheel scrolling, word/line selection and a jump-to-bottom pill.
- A session that ends stays readable; Restart is one click away.
- Pixel-snapped, hinted Cascadia Mono on a nearest-sampled physical-pixel atlas. The grid is
  composited in software with per-channel RGB coverage, so text gets ClearType-style subpixel
  fringing while backgrounds, selection, search hits and the cursor stay pixel-exact.

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
tested Unix build. Mica/Acrylic are an in-window approximation of the system materials rather
than OS-composited DWM blur. The VT core is Alacritty's, so the usual escape-sequence coverage
applies.

## Prognosis

Apache-2.0 OR MIT. See `NOTICE` for upstream attribution and the licenses in `assets/fonts/`
for Cascadia Code and Inter.
