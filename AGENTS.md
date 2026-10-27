# AGENTS.md

Illness is a Windows-first terminal emulator: Alacritty's VT core plus an egui shell.

## Commands

- Build: `cargo build -p illness`
- Run: `cargo run -p illness`
- Test: `cargo test --workspace`
- Lint: `cargo clippy --workspace --all-targets -- -D warnings`
- Format: `cargo fmt --all`

## Architecture

- `crates/illness-core`: `Config` (`%APPDATA%\illness\config.toml`), built-in `Theme`s,
  profile detection and `Rgb`.
- `crates/illness-term`: `Session` owns the ConPTY and `alacritty_terminal::Term`; `snapshot`
  is the immutable per-frame grid; `input` is xterm key/mouse/paste encoding.
- `crates/illness-ui`: `design` tokens/visuals, `fonts`, vector `icons`, `glyph` swash atlas,
  `soft` grayscale compositor, `gpu` wgpu glyph compositor, `terminal` grid view, `chrome`
  titlebar/tabs, `overlays` palette/search/settings.
- `apps/illness`: winit/egui host, shortcuts, event drain, blurred background image, Win32
  material (`platform.rs`).

## Rules

- Terminal text is rendered by `GlyphAtlas` at physical pixels. Do not replace it with
  `Painter::text` for grid cells. Keep hinting on, subpixel binning off, nearest sampling.
- Colors come from `Theme`/`Tokens`; do not hard-code UI hex values in widgets.
- No `unsafe` outside `apps/illness/src/platform.rs` and
  `crates/illness_explorer_command_injector/src/` (Win32 COM interop).
- Keep `clipboard_shortcuts: false`: the terminal owns Ctrl+C/V; the app handles
  Ctrl+Shift+C/V itself.
- Scrollback and cursor settings are captured in `SpawnSpec`; they apply to new tabs.
- No panics for user-visible failures; surface a toast and keep the app alive.
- Keep commits short.
- `docs/` and `.superlite/` are local-only (gitignored).
