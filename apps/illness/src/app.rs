use std::sync::Arc;
use std::time::Duration;

use egui::{
    self, Align2, Color32, Event, FontId, Id, Key, Modifiers, Order, Rect, RichText, Sense, Stroke,
    StrokeKind, Vec2, ViewportCommand, pos2, vec2,
};
use illness_core::config::BackgroundMode;
use illness_core::{Config, Material, Profile, Rgb, Theme, detect_profiles};
use illness_term::{
    GridSize, SelectionType, Session, SessionEvent, Side, SpawnSpec, encode_paste,
    snapshot::CellFlags,
};
use illness_ui::chrome::{self, Action, ChromeState, TabInfo};
use illness_ui::overlays::{self, OverlayKind, OverlayState, SearchRequest};
use illness_ui::terminal::SearchState;
use illness_ui::{
    TerminalAction, TerminalView, Tokens, design, fonts, glyph::GlyphAtlas, gpu::GpuText,
};
use raw_window_handle::HasWindowHandle;
use winit::window::Window;

use crate::{backdrop, platform};

const DEFAULT_FONT_SIZE: f32 = 18.0;
const INITIAL_COLS: usize = 120;
const INITIAL_ROWS: usize = 30;
const BELL_FLASH_SECS: f64 = 0.18;
/// Coalesce settings writes; live values still apply immediately.
const SAVE_DEBOUNCE_MS: u64 = 300;

/// Pending delayed config write. A pointer release flushes immediately so a
/// click never waits for the timer.
#[derive(Debug, Default)]
struct SaveDebounce {
    due: Option<f64>,
}

impl SaveDebounce {
    fn mark(&mut self, now: f64) {
        self.due = Some(now + SAVE_DEBOUNCE_MS as f64 / 1_000.0);
    }

    fn clear(&mut self) {
        self.due = None;
    }

    fn take_due(&mut self, now: f64, pointer_released: bool) -> bool {
        if self.due.is_some_and(|due| pointer_released || now >= due) {
            self.due = None;
            true
        } else {
            false
        }
    }
}
/// DECSET 1004 focus reports.
const FOCUS_IN: &[u8] = b"\x1b[I";
const FOCUS_OUT: &[u8] = b"\x1b[O";

/// One live terminal tab: the PTY session plus everything the UI remembers.
struct Tab {
    session: Session,
    view: TerminalView,
    title: String,
    profile_name: String,
    /// Launch values captured at spawn time. Restart uses these when the
    /// profile was renamed or removed so it never silently starts another
    /// shell.
    program: String,
    args: Vec<String>,
    accent: Rgb,
    exited: bool,
    exit_code: Option<i32>,
    /// Whether the PTY currently believes this tab has focus (DECSET 1004).
    focus_reported: bool,
}

/// A slot in the tab strip. Settings is a real tab but has no PTY session.
enum TabEntry {
    Terminal(Box<Tab>),
    Settings,
}

/// The winit/egui application shell and all cross-module state.
pub struct IllnessApp {
    ctx: egui::Context,
    window: Arc<Window>,
    config: Config,
    profiles: Vec<Profile>,
    theme: &'static Theme,
    tokens: Tokens,
    atlas: GlyphAtlas,
    tabs: Vec<TabEntry>,
    active: usize,
    chrome: ChromeState,
    overlays: OverlayState,
    applied_theme: String,
    applied_font_family: String,
    applied_font_size: f32,
    applied_ppp: f32,
    applied_line_height: f32,
    applied_material: Material,
    /// `(mode, path)` that produced the current backdrop texture.
    applied_background: Option<(BackgroundMode, String)>,
    applied_background_opacity: f32,
    /// Family whose load failed, so the toast is not repeated every change.
    font_failure: Option<String>,
    /// GPU glyph compositor, when a wgpu device and `gpu_text` are available.
    gpu_text: Option<Arc<GpuText>>,
    window_title: String,
    backdrop: Option<backdrop::Backdrop>,

    window_focused: bool,
    last_search: Option<(String, bool)>,
    bell_flash_until: f64,
    pending_material: bool,
    save_debounce: SaveDebounce,
    saved: bool,
}

impl IllnessApp {
    pub fn new(
        ctx: &egui::Context,
        render_state: Option<&egui_wgpu::RenderState>,
        window: Arc<Window>,
    ) -> Self {
        let mut config = Config::load();
        let mut detected = detect_profiles();
        #[cfg(target_os = "windows")]
        detected.extend(platform::wsl_profiles());
        config.profiles = merge_profiles(
            std::mem::take(&mut config.profiles),
            detected,
            &config.hidden_profiles,
        );
        ensure_default_profile(&mut config);

        let theme = Theme::by_name(&config.theme);
        let tokens = Tokens::from_theme(theme);
        let ctx = ctx.clone();
        let profiles = config.profiles.clone();

        fonts::install(&ctx);
        design::install(&ctx, theme, &config);
        let mut atlas = GlyphAtlas::new(
            &ctx,
            fonts::CASCADIA_MONO,
            config.font_size,
            ctx.pixels_per_point(),
        );
        atlas.set_line_height(config.line_height);
        let mut applied_font_family = "Cascadia Mono".to_string();
        if config.font_family != applied_font_family {
            match fonts::load_terminal_font(&config.font_family) {
                Some((data, index)) => {
                    atlas.set_font(data, index);
                    applied_font_family = config.font_family.clone();
                }
                None => log::warn!(
                    "font family {:?} is unavailable; using bundled Cascadia Mono",
                    config.font_family
                ),
            }
        }
        let line_height = config.line_height;
        let background_opacity = config.background_opacity;
        let gpu_text = if config.gpu_text {
            render_state.map(|state| Arc::new(GpuText::new(state)))
        } else {
            None
        };
        if let Some(gpu) = &gpu_text {
            if gpu.failed() {
                log::warn!("GPU text unavailable on this device; using the CPU compositor");
            } else {
                log::info!("GPU text compositor enabled");
            }
        } else {
            log::info!("GPU text unavailable or disabled; using the CPU compositor");
        }

        let mut app = Self {
            ctx,
            window: Arc::clone(&window),
            config,
            profiles,
            theme,
            tokens,
            atlas,
            tabs: Vec::new(),
            active: 0,
            chrome: ChromeState::default(),
            overlays: OverlayState::default(),
            applied_theme: theme.name.to_string(),
            applied_font_family,
            applied_font_size: 0.0,
            applied_ppp: 0.0,
            applied_line_height: line_height,
            applied_material: Material::Mica,
            applied_background: None,
            applied_background_opacity: background_opacity,
            font_failure: None,
            gpu_text,
            window_title: "Illness".to_string(),
            backdrop: None,
            window_focused: false,
            last_search: None,
            bell_flash_until: 0.0,
            pending_material: false,
            save_debounce: SaveDebounce::default(),
            saved: false,
        };
        app.applied_font_size = app.config.font_size;
        app.applied_ppp = app.ctx.pixels_per_point();
        app.apply_material_to(window.as_ref());
        app.rebuild_backdrop();
        app.open_tab(None);
        app
    }

    /// Spawn a tab for `profile` (or the configured default) and make it active.
    pub fn open_tab(&mut self, profile: Option<&str>) {
        match self.build_tab(profile) {
            Ok(tab) => {
                let index = self.settings_index().unwrap_or(self.tabs.len());
                self.tabs.insert(index, TabEntry::Terminal(Box::new(tab)));
                self.active = index;
            }
            Err(error) => {
                self.overlays.toast = Some(error);
            }
        }
    }

    /// Focus the Settings tab, creating it after the terminal tabs if needed.
    fn open_settings(&mut self) {
        if let Some(index) = self.settings_index() {
            self.active = index;
        } else {
            self.tabs.push(TabEntry::Settings);
            self.active = self.tabs.len() - 1;
        }
        self.chrome.dropdown_open = false;
    }

    fn settings_index(&self) -> Option<usize> {
        self.tabs
            .iter()
            .position(|entry| matches!(entry, TabEntry::Settings))
    }

    fn active_terminal(&self) -> Option<&Tab> {
        match self.tabs.get(self.active) {
            Some(TabEntry::Terminal(tab)) => Some(tab),
            _ => None,
        }
    }

    fn active_terminal_mut(&mut self) -> Option<&mut Tab> {
        match self.tabs.get_mut(self.active) {
            Some(TabEntry::Terminal(tab)) => Some(tab.as_mut()),
            _ => None,
        }
    }

    fn active_terminal_index(&self) -> Option<usize> {
        matches!(self.tabs.get(self.active), Some(TabEntry::Terminal(_))).then_some(self.active)
    }

    fn build_tab(&self, requested: Option<&str>) -> Result<Tab, String> {
        let profile = self.profile_for(requested);
        self.build_tab_for(&profile)
    }

    fn build_tab_for(&self, profile: &Profile) -> Result<Tab, String> {
        let metrics = self.atlas.metrics();
        let cell_px = (cell_pixel(metrics.width_px), cell_pixel(metrics.height_px));
        let ctx = self.ctx.clone();
        let waker: Arc<dyn Fn() + Send + Sync> = Arc::new(move || ctx.request_repaint());

        let session = Session::spawn(
            spawn_spec(profile, &self.config),
            GridSize::new(INITIAL_COLS, INITIAL_ROWS),
            cell_px,
            waker,
        )
        .map_err(|error| format!("Could not start {}: {error}", profile.name))?;

        let (title, exited, exit_code) = restarted_state(profile);
        Ok(Tab {
            session,
            view: TerminalView::default(),
            title,
            profile_name: profile.name.clone(),
            program: profile.program.clone(),
            args: profile.args.clone(),
            accent: profile.accent,
            exited,
            exit_code,
            focus_reported: false,
        })
    }

    fn profile_for(&self, requested: Option<&str>) -> Profile {
        if let Some(name) = requested
            && let Some(profile) = self
                .profiles
                .iter()
                .find(|profile| profile.name.eq_ignore_ascii_case(name))
        {
            return profile.clone();
        }

        if !self.config.default_profile.is_empty()
            && let Some(profile) = self.profiles.iter().find(|profile| {
                profile
                    .name
                    .eq_ignore_ascii_case(&self.config.default_profile)
            })
        {
            return profile.clone();
        }

        self.profiles.first().cloned().unwrap_or_default()
    }

    /// Invisible resize hit-testing for the frameless window.
    fn resize_edges(&self, ui: &mut egui::Ui) {
        let screen = ui.ctx().content_rect();
        let edge = 5.0;
        let corner = 12.0;
        let (left, top, right, bottom) =
            (screen.left(), screen.top(), screen.right(), screen.bottom());

        let zones = [
            (
                egui::ResizeDirection::North,
                Rect::from_min_max(pos2(left + corner, top), pos2(right - corner, top + edge)),
                egui::CursorIcon::ResizeVertical,
            ),
            (
                egui::ResizeDirection::South,
                Rect::from_min_max(
                    pos2(left + corner, bottom - edge),
                    pos2(right - corner, bottom),
                ),
                egui::CursorIcon::ResizeVertical,
            ),
            (
                egui::ResizeDirection::West,
                Rect::from_min_max(pos2(left, top + corner), pos2(left + edge, bottom - corner)),
                egui::CursorIcon::ResizeHorizontal,
            ),
            (
                egui::ResizeDirection::East,
                Rect::from_min_max(
                    pos2(right - edge, top + corner),
                    pos2(right, bottom - corner),
                ),
                egui::CursorIcon::ResizeHorizontal,
            ),
            (
                egui::ResizeDirection::NorthWest,
                Rect::from_min_max(pos2(left, top), pos2(left + corner, top + corner)),
                egui::CursorIcon::ResizeNwSe,
            ),
            (
                egui::ResizeDirection::NorthEast,
                Rect::from_min_max(pos2(right - corner, top), pos2(right, top + corner)),
                egui::CursorIcon::ResizeNeSw,
            ),
            (
                egui::ResizeDirection::SouthWest,
                Rect::from_min_max(pos2(left, bottom - corner), pos2(left + corner, bottom)),
                egui::CursorIcon::ResizeNeSw,
            ),
            (
                egui::ResizeDirection::SouthEast,
                Rect::from_min_max(pos2(right - corner, bottom - corner), pos2(right, bottom)),
                egui::CursorIcon::ResizeNwSe,
            ),
        ];

        for (index, (direction, zone, cursor)) in zones.into_iter().enumerate() {
            let response = ui.interact(
                zone,
                Id::unique(("illness_resize", index)),
                Sense::click_and_drag(),
            );
            if response.hovered() {
                ui.ctx().set_cursor_icon(cursor);
            }
            if response.drag_started_by(egui::PointerButton::Primary) {
                ui.ctx()
                    .send_viewport_cmd(ViewportCommand::BeginResize(direction));
            }
        }
    }

    fn apply_material_to(&mut self, window: &impl HasWindowHandle) {
        let dark = !self.theme.is_light();
        let opacity = self.config.background_opacity;
        let requested = self.config.material;
        if let Err(error) = platform::apply(window, requested, dark, opacity) {
            log::warn!(
                "window material {} (dark={dark}) failed: {error}",
                material_name(requested)
            );
            self.config.material = Material::Solid;
            if let Err(error) = platform::apply(window, Material::Solid, dark, opacity) {
                log::warn!("solid material unavailable: {error}");
            }
            self.overlays.toast = Some(format!(
                "{} is unavailable here; using Solid",
                material_name(requested)
            ));
        } else {
            log::info!(
                "window material {} (dark={dark}) applied",
                material_name(requested)
            );
        }
        self.applied_material = self.config.material;
        // The panel fill follows the material: opaque for Solid, transparent
        // so the native backdrop can show through otherwise. This also covers
        // a fallback to Solid when the requested material is unavailable.
        design::install(&self.ctx, self.theme, &self.config);
        self.ctx.request_repaint();
    }

    /// Reload the backdrop for the current mode and path.
    ///
    /// Called only when the mode, path, or a gradient-producing theme changes;
    /// opacity is applied at paint time. Images are decoded once per
    /// `(path, mtime)` and rejected when larger than the texture cap.
    fn rebuild_backdrop(&mut self) {
        let mode = self.config.background;
        let path = self.config.background_path.clone();
        // Native Acrylic already blurs the real desktop. Decoding and painting
        // the wallpaper here would cover that behind-window blur.
        if self.config.material == Material::Acrylic && mode == BackgroundMode::Desktop {
            self.backdrop = None;
            self.applied_background = Some((mode, path));
            self.applied_background_opacity = self.config.background_opacity;
            self.ctx.request_repaint();
            return;
        }
        match backdrop::build(&self.ctx, self.theme, mode, &path) {
            Ok(backdrop) => {
                self.backdrop = backdrop;
                if self.backdrop.is_none() && mode == BackgroundMode::Image {
                    self.overlays.toast = Some(if path.trim().is_empty() {
                        "Choose a background image".to_string()
                    } else {
                        format!("Could not load background \"{}\"", path)
                    });
                }
            }
            Err(error) => {
                self.backdrop = None;
                self.overlays.toast = Some(error);
            }
        }

        self.applied_background = Some((mode, path));
        self.applied_background_opacity = self.config.background_opacity;
        self.ctx.request_repaint();
    }

    /// Load an in-window backdrop after a material change when the new
    /// material/mode needs one and none is currently loaded.
    fn sync_backdrop_for_material(&mut self) {
        if needs_backdrop(self.config.material, self.config.background) && self.backdrop.is_none() {
            self.rebuild_backdrop();
        }
    }

    fn reapply_config(&mut self) {
        self.config = self.config.clone().sanitized();
        if self.config.profiles != self.profiles {
            self.profiles = self.config.profiles.clone();
        }

        let theme = Theme::by_name(&self.config.theme);
        let mut background_dirty = false;
        if theme.name != self.applied_theme.as_str() {
            design::install(&self.ctx, theme, &self.config);
            self.theme = theme;
            self.tokens = Tokens::from_theme(theme);
            self.applied_theme = theme.name.to_string();
            self.pending_material = true;
            // A loaded image does not depend on the theme; rebuild only the
            // gradient fallback so switching themes never re-decodes a file.
            background_dirty = self
                .backdrop
                .as_ref()
                .is_none_or(|backdrop| !backdrop.from_image);
            self.ctx.request_repaint();
        }

        if self.config.font_family != self.applied_font_family {
            match fonts::load_terminal_font(&self.config.font_family) {
                Some((data, index)) => {
                    self.atlas.set_font(data, index);
                    self.atlas.set_size(
                        &self.ctx,
                        self.config.font_size,
                        self.ctx.pixels_per_point(),
                    );
                    self.atlas.set_line_height(self.config.line_height);
                    self.applied_font_family = self.config.font_family.clone();
                    self.applied_font_size = self.config.font_size;
                    self.applied_line_height = self.config.line_height;
                    self.font_failure = None;
                    self.invalidate_terminal_surfaces();
                }
                None => {
                    let family = self.config.font_family.clone();
                    if self.font_failure.as_deref() != Some(family.as_str()) {
                        self.overlays.toast = Some(format!(
                            "Font \"{family}\" is unavailable; keeping {}",
                            self.applied_font_family
                        ));
                        self.font_failure = Some(family);
                    }
                }
            }
        }

        if (self.config.font_size - self.applied_font_size).abs() > f32::EPSILON {
            self.atlas.set_size(
                &self.ctx,
                self.config.font_size,
                self.ctx.pixels_per_point(),
            );
            self.applied_font_size = self.config.font_size;
        }

        if (self.config.line_height - self.applied_line_height).abs() > f32::EPSILON {
            self.atlas.set_line_height(self.config.line_height);
            if let Some(gpu) = &self.gpu_text {
                gpu.reset();
            }
            self.applied_line_height = self.config.line_height;
        }

        if !self.config.gpu_text {
            self.gpu_text = None;
        }

        let background = (self.config.background, self.config.background_path.clone());
        let opacity_changed =
            (self.config.background_opacity - self.applied_background_opacity).abs() > f32::EPSILON;
        if background_dirty || self.applied_background.as_ref() != Some(&background) {
            self.rebuild_backdrop();
        } else if opacity_changed {
            self.applied_background_opacity = self.config.background_opacity;
            self.ctx.request_repaint();
        }

        if self.config.material != self.applied_material {
            self.pending_material = true;
            // Acrylic+Desktop has no in-window backdrop, so switching to Mica
            // or Image must load one now. An existing texture is reused to
            // avoid decoding the wallpaper twice.
            self.sync_backdrop_for_material();
        }

        // The legacy acrylic accent carries its own tint, so opacity changes
        // must re-apply it even though the in-window tint is painted live.
        if opacity_changed && self.config.material == Material::Acrylic {
            self.pending_material = true;
        }

        self.schedule_persist();
    }

    /// Queue a config write so sliders and text edits do not hit the disk on
    /// every frame. Live values are already applied in memory.
    fn schedule_persist(&mut self) {
        let now = self.ctx.input(|input| input.time);
        self.save_debounce.mark(now);
        self.ctx
            .request_repaint_after(Duration::from_millis(SAVE_DEBOUNCE_MS));
    }

    fn persist(&mut self) {
        self.save_debounce.clear();
        // An empty name only exists while its text field is being edited.
        // Keep the last valid file rather than persisting an invalid profile.
        if self.config.has_empty_profile_name() {
            log::debug!("skipping config save while a profile name is empty");
            return;
        }
        if let Err(error) = self.config.save() {
            log::warn!("failed to save config: {error}");
            self.overlays.toast = Some(format!("Could not save config: {error}"));
        }
    }

    pub fn shutdown_and_save(&mut self) {
        if self.saved {
            return;
        }
        self.saved = true;

        for entry in &self.tabs {
            let TabEntry::Terminal(tab) = entry else {
                continue;
            };
            tab.session.shutdown();
        }

        // If the window closes while a name field is focused and empty, restore
        // the name it had when editing began so the exit save stays valid.
        if let Some((index, start)) = self.overlays.profile_name_edit.take()
            && let Some(profile) = self.config.profiles.get_mut(index)
            && profile.name.trim().is_empty()
        {
            profile.name = start;
        }

        if let Err(error) = self.config.save() {
            log::warn!("failed to save config on exit: {error}");
        }
    }

    fn set_font_size(&mut self, size: f32) {
        let size = zoom_size(size, 0.0);
        if (size - self.config.font_size).abs() < f32::EPSILON {
            return;
        }
        self.config.font_size = size;
        self.atlas
            .set_size(&self.ctx, size, self.ctx.pixels_per_point());
        if let Some(gpu) = &self.gpu_text {
            gpu.reset();
        }
        self.applied_font_size = size;
        self.persist();
    }

    /// Drops per-tab software surfaces after a font swap.
    ///
    /// The surface cache key covers font geometry but not the face identity, so
    /// two faces with identical metrics would otherwise keep the old pixels.
    /// Public view state (scroll, search, pointer/IME anchors) is preserved.
    fn invalidate_terminal_surfaces(&mut self) {
        if let Some(gpu) = &self.gpu_text {
            gpu.reset();
        }
        for entry in &mut self.tabs {
            let TabEntry::Terminal(tab) = entry else {
                continue;
            };
            let previous = std::mem::take(&mut tab.view);
            tab.view.scroll_target = previous.scroll_target;
            tab.view.search = previous.search;
            tab.view.last_click = previous.last_click;
            tab.view.ime_pos = previous.ime_pos;
        }
    }

    fn close_tab(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        if self.tabs.len() == 1 {
            self.tabs.remove(index);
            self.active = 0;
            // The host saves and exits after this pass; shutting down here
            // would break a later CancelClose.
            self.ctx.send_viewport_cmd(ViewportCommand::Close);
            return;
        }

        self.tabs.remove(index);
        self.active = next_active_after_close(self.active, index, self.tabs.len());
    }

    fn restart_tab(&mut self, index: usize) {
        let Some(TabEntry::Terminal(tab)) = self.tabs.get(index) else {
            return;
        };
        let profile = restart_profile(
            &tab.profile_name,
            &tab.program,
            &tab.args,
            tab.accent,
            &self.profiles,
        );

        match self.build_tab_for(&profile) {
            Ok(replacement) => {
                if let Some(slot) = self.tabs.get_mut(index) {
                    *slot = TabEntry::Terminal(Box::new(replacement));
                    self.active = index;
                }
            }
            Err(error) => {
                self.overlays.toast = Some(error);
            }
        }
    }

    fn move_tab(&mut self, from: usize, to: usize) {
        let len = self.tabs.len();
        if from >= len || to >= len || from == to {
            return;
        }
        self.active = active_after_move(self.active, from, to);
        let tab = self.tabs.remove(from);
        self.tabs.insert(to, tab);
    }

    fn copy_selection(&mut self) {
        let Some(text) = self.selection_text() else {
            return;
        };
        self.ctx.copy_text(text);
    }

    fn selection_text(&self) -> Option<String> {
        let text = self.active_terminal()?.session.selection_to_string()?;
        let text = trim_copy(&text);
        (!text.is_empty()).then_some(text)
    }

    fn paste_clipboard(&mut self) {
        let text = match arboard::Clipboard::new().and_then(|mut clipboard| clipboard.get_text()) {
            Ok(text) => text,
            Err(error) => {
                self.overlays.toast = Some(format!("Clipboard unavailable: {error}"));
                return;
            }
        };
        if text.is_empty() {
            return;
        }

        if let Some(tab) = self.active_terminal() {
            let bracketed = tab.session.snapshot().modes.bracketed_paste;
            tab.session.write(&format_paste(&text, bracketed));
        }
    }

    fn open_link(&mut self, url: &str) {
        if let Err(error) = open::that(url) {
            self.overlays.toast = Some(format!("Could not open link: {error}"));
        }
    }

    fn select_all(&mut self) {
        if let Some(tab) = self.active_terminal() {
            let snapshot = tab.session.snapshot();
            if snapshot.cols > 0 && snapshot.rows > 0 {
                tab.session
                    .selection_start(SelectionType::Simple, 0, 0, Side::Left);
                tab.session
                    .selection_update(snapshot.cols - 1, snapshot.rows - 1, Side::Right);
            }
        }
    }

    fn clear_view(&mut self) {
        if let Some(tab) = self.active_terminal_mut() {
            tab.session.selection_clear();
            let offset = i32::try_from(tab.session.snapshot().display_offset).unwrap_or(i32::MAX);
            if offset > 0 {
                tab.session.scroll(-offset);
            }
            tab.view.scroll_target = 0;
        }
    }

    fn run_search(&mut self, request: SearchRequest) {
        let query = request.query.trim().to_string();
        if query.is_empty() {
            self.overlays.search_index = None;
            self.last_search = None;
            if let Some(TabEntry::Terminal(tab)) = self.tabs.get_mut(self.active) {
                tab.view.search = None;
            }
            return;
        }

        let Some(TabEntry::Terminal(tab)) = self.tabs.get_mut(self.active) else {
            self.overlays.search_index = Some((0, 0));
            return;
        };
        let snapshot = tab.session.snapshot();
        let count = count_visible_matches(&snapshot, &query, request.case_sensitive);
        let repeated = self
            .last_search
            .as_ref()
            .is_some_and(|(last, case)| last == &query && *case == request.case_sensitive);
        let current = if count == 0 {
            0
        } else {
            match self.overlays.search_index {
                Some((current, total)) if repeated && total == count && current >= 1 => {
                    cycle_search(current, count, request.forward)
                }
                _ => 1,
            }
        };

        tab.view.search = Some(SearchState {
            query: query.clone(),
            case_sensitive: request.case_sensitive,
        });
        self.overlays.search_index = Some((current, count));
        self.last_search = Some((query.clone(), request.case_sensitive));

        let found = tab.session.search(&query, request.forward);
        if found.is_none() {
            self.overlays.toast = Some(format!("No matches for \"{query}\""));
        }
        if let Some((start_row, _, _, _)) = found
            && let Some(delta) = search_scroll_delta(start_row, snapshot.rows)
        {
            tab.session.scroll(delta);
            let after = tab.session.snapshot();
            tab.view.scroll_target = i32::try_from(after.display_offset).unwrap_or(i32::MAX);
            let total = count_visible_matches(&after, &query, request.case_sensitive);
            let current = if total == 0 {
                0
            } else {
                current.min(total).max(1)
            };
            self.overlays.search_index = Some((current, total));
        }
    }

    /// Mirrors the search bar's live query into the active terminal view so
    /// highlights and the match count update while typing.
    fn sync_live_search(&mut self) {
        if self.overlays.kind != OverlayKind::Search {
            self.overlays.search_index = None;
            self.last_search = None;
            if let Some(TabEntry::Terminal(tab)) = self.tabs.get_mut(self.active)
                && tab.view.search.is_some()
            {
                tab.view.search = None;
            }
            return;
        }

        let query = self.overlays.search.clone();
        let case_sensitive = self.overlays.case_sensitive;
        if query.is_empty() {
            self.overlays.search_index = None;
            self.last_search = None;
            if let Some(TabEntry::Terminal(tab)) = self.tabs.get_mut(self.active) {
                tab.view.search = None;
            }
            return;
        }

        let Some(TabEntry::Terminal(tab)) = self.tabs.get_mut(self.active) else {
            self.overlays.search_index = Some((0, 0));
            return;
        };
        let count = count_visible_matches(&tab.session.snapshot(), &query, case_sensitive);
        let same_query =
            tab.view.search.as_ref().is_some_and(|search| {
                search.query == query && search.case_sensitive == case_sensitive
            });
        tab.view.search = Some(SearchState {
            query,
            case_sensitive,
        });
        let current = if count == 0 {
            0
        } else {
            match self.overlays.search_index {
                Some((current, total)) if same_query && total == count && current >= 1 => {
                    current.min(count)
                }
                _ => 1,
            }
        };
        self.overlays.search_index = Some((current, count));
    }

    fn handle_action(&mut self, action: Action) {
        match action {
            Action::NewTab(profile) => self.open_tab(profile.as_deref()),
            Action::CloseTab(index) => self.close_tab(index),
            Action::ActivateTab(index) => {
                if index < self.tabs.len() {
                    self.active = index;
                }
            }
            Action::MoveTab(from, to) => self.move_tab(from, to),
            Action::OpenPalette => {
                self.chrome.dropdown_open = false;
                self.overlays.kind = OverlayKind::Palette;
            }
            Action::OpenSearch => {
                self.chrome.dropdown_open = false;
                self.overlays.kind = OverlayKind::Search;
            }
            Action::OpenSettings => {
                self.open_settings();
            }
            Action::Minimize => self.ctx.send_viewport_cmd(ViewportCommand::Minimized(true)),
            Action::Maximize => {
                let maximized = self
                    .ctx
                    .input(|input| input.viewport().maximized.unwrap_or(false));
                self.ctx
                    .send_viewport_cmd(ViewportCommand::Maximized(!maximized));
            }
            Action::CloseWindow => {
                // Let the host run the pass, collect the Close command and
                // save/exit afterwards so a CancelClose can still win.
                self.ctx.send_viewport_cmd(ViewportCommand::Close);
            }
            Action::ConfigChanged => self.reapply_config(),
            Action::ToggleMaterial => {
                self.config.material = next_material(self.config.material);
                self.pending_material = true;
                self.sync_backdrop_for_material();
                self.persist();
            }
            Action::Copy => self.copy_selection(),
            Action::Paste => self.paste_clipboard(),
            Action::SelectAll => self.select_all(),
            Action::Clear => self.clear_view(),
            Action::Search(request) => self.run_search(request),
        }
    }

    fn handle_terminal_action(&mut self, action: TerminalAction) {
        match action {
            TerminalAction::Copy(text) => self.ctx.copy_text(trim_copy(&text)),
            TerminalAction::Paste => self.paste_clipboard(),
            TerminalAction::OpenLink(url) => self.open_link(&url),
            TerminalAction::RequestSearch => self.overlays.kind = OverlayKind::Search,
            TerminalAction::Bell => self.flash_bell(),
        }
    }

    fn flash_bell(&mut self) {
        self.overlays.toast = Some("Terminal bell".to_string());
        let now = self.ctx.input(|input| input.time);
        self.bell_flash_until = now + BELL_FLASH_SECS;
        self.ctx.request_repaint_after(Duration::from_millis(200));
    }

    fn drain_events(&mut self) {
        let ctx = self.ctx.clone();
        let theme = self.theme;
        let mut bell = false;

        for entry in &mut self.tabs {
            let TabEntry::Terminal(tab) = entry else {
                continue;
            };
            while let Ok(event) = tab.session.events().try_recv() {
                match event {
                    SessionEvent::Title(title) => {
                        tab.title = display_title(&title, &tab.profile_name);
                    }
                    SessionEvent::ResetTitle => tab.title = tab.profile_name.clone(),
                    SessionEvent::Bell => bell = true,
                    SessionEvent::ClipboardStore(text) => ctx.copy_text(text),
                    SessionEvent::PtyWrite(bytes) => tab.session.write(&bytes),
                    SessionEvent::ColorRequest(index) => {
                        tab.session.write(color_response(index, theme).as_bytes());
                    }
                    SessionEvent::TextAreaSizeRequest => {
                        let snapshot = tab.session.snapshot();
                        let reply = format!("\x1b[8;{};{}t", snapshot.rows, snapshot.cols);
                        tab.session.write(reply.as_bytes());
                    }
                    SessionEvent::ChildExit(code) => {
                        tab.exited = true;
                        tab.exit_code = Some(code);
                    }
                    SessionEvent::Exit => tab.exited = true,
                    SessionEvent::CursorBlinkingChange
                    | SessionEvent::MouseCursorDirty
                    | SessionEvent::Wakeup => {}
                }
            }
        }

        if bell {
            self.flash_bell();
        }
    }

    fn sync_tabs(&mut self) {
        for entry in &mut self.tabs {
            let TabEntry::Terminal(tab) = entry else {
                continue;
            };
            if tab.session.is_exited() {
                tab.exited = true;
            }
            if let Some(title) = tab.session.title() {
                tab.title = display_title(&title, &tab.profile_name);
            }
        }
    }

    /// Send focus in/out reports when the window focus or active tab changes,
    /// but only to sessions that enabled DECSET 1004.
    fn sync_focus(&mut self) {
        let active = self.active_terminal_index();
        let window_focused = self.window_focused;
        for (index, entry) in self.tabs.iter_mut().enumerate() {
            let TabEntry::Terminal(tab) = entry else {
                continue;
            };
            let desired = window_focused && Some(index) == active;
            if !tab.session.focus_reporting() {
                tab.focus_reported = false;
                continue;
            }
            if tab.focus_reported != desired {
                tab.session
                    .write(if desired { FOCUS_IN } else { FOCUS_OUT });
                tab.focus_reported = desired;
            }
        }
    }

    fn clamp_active(&mut self) {
        if self.tabs.is_empty() {
            self.active = 0;
        } else if self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
        }
    }

    fn tab_infos(&self) -> Vec<TabInfo> {
        self.tabs
            .iter()
            .map(|entry| match entry {
                TabEntry::Terminal(tab) => TabInfo {
                    title: tab.title.clone(),
                    profile: tab.profile_name.clone(),
                    accent: tab.accent,
                    exited: tab.exited,
                },
                TabEntry::Settings => TabInfo {
                    title: "Settings".to_string(),
                    profile: "Settings".to_string(),
                    accent: self.theme.accent,
                    exited: false,
                },
            })
            .collect()
    }

    fn update_window_title(&mut self, ctx: &egui::Context) {
        let title = match self.tabs.get(self.active) {
            Some(TabEntry::Terminal(tab)) => Some(tab.title.as_str()),
            Some(TabEntry::Settings) => Some("Settings"),
            None => None,
        };
        let desired = window_title_for(title);
        if desired != self.window_title {
            self.window_title = desired.clone();
            ctx.send_viewport_cmd(ViewportCommand::Title(desired));
        }
    }

    fn handle_zoom_wheel(&mut self, ctx: &egui::Context) {
        let notches = ctx.input(|input| {
            input.events.iter().fold(0.0_f32, |sum, event| match event {
                Event::MouseWheel {
                    unit,
                    delta,
                    modifiers,
                    ..
                } if modifiers.ctrl || modifiers.command => {
                    sum + match unit {
                        egui::MouseWheelUnit::Line => delta.y,
                        egui::MouseWheelUnit::Point => delta.y / 50.0,
                        egui::MouseWheelUnit::Page => delta.y,
                    }
                }
                _ => sum,
            })
        });

        if let Some(direction) = wheel_zoom_direction(notches) {
            self.set_font_size(self.config.font_size + direction);
        }
    }

    fn handle_shortcuts(&mut self, ctx: &egui::Context) -> bool {
        let candidates: Vec<(Key, Modifiers)> = ctx.input(|input| {
            input
                .events
                .iter()
                .filter_map(|event| match event {
                    Event::Key {
                        key,
                        pressed: true,
                        modifiers,
                        ..
                    } => Some((*key, *modifiers)),
                    _ => None,
                })
                .collect()
        });
        // Only the unshifted Ctrl+C shortcut depends on selection, so avoid
        // copying the selection string on every frame for every other key.
        let needs_selection = candidates.iter().any(|(key, modifiers)| {
            *key == Key::C && !modifiers.shift && (modifiers.ctrl || modifiers.command)
        });
        let has_selection = needs_selection && self.selection_text().is_some();

        let mut shortcuts: Vec<Shortcut> = candidates
            .iter()
            .filter_map(|(key, modifiers)| shortcut_for(*key, *modifiers, has_selection))
            .collect();
        if shortcuts.is_empty() {
            return false;
        }

        ctx.input_mut(|input| {
            input.events.retain(|event| match event {
                Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } => shortcut_for(*key, *modifiers, has_selection).is_none(),
                _ => true,
            });
        });

        for shortcut in shortcuts.drain(..) {
            self.execute_shortcut(shortcut);
        }
        true
    }

    fn execute_shortcut(&mut self, shortcut: Shortcut) {
        match shortcut {
            Shortcut::NewTab => self.open_tab(None),
            Shortcut::CloseTab => self.close_tab(self.active),
            Shortcut::NextTab => self.cycle_tab(1),
            Shortcut::PrevTab => self.cycle_tab(-1),
            Shortcut::SelectTab(index) => {
                if index < self.tabs.len() {
                    self.active = index;
                }
            }
            Shortcut::Palette => self.overlays.kind = OverlayKind::Palette,
            Shortcut::Search => self.overlays.kind = OverlayKind::Search,
            Shortcut::Settings => self.open_settings(),
            Shortcut::ZoomIn => self.set_font_size(self.config.font_size + 1.0),
            Shortcut::ZoomOut => self.set_font_size(self.config.font_size - 1.0),
            Shortcut::ZoomReset => self.set_font_size(DEFAULT_FONT_SIZE),
            Shortcut::Copy => self.copy_selection(),
            Shortcut::Paste => self.paste_clipboard(),
        }
    }

    fn cycle_tab(&mut self, delta: isize) {
        if self.tabs.is_empty() {
            return;
        }
        let len = self.tabs.len() as isize;
        self.active = (self.active as isize + delta).rem_euclid(len) as usize;
    }
}

impl IllnessApp {
    pub fn ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();

        let ppp = ctx.pixels_per_point();
        if (ppp - self.applied_ppp).abs() > f32::EPSILON {
            self.atlas.set_size(&ctx, self.config.font_size, ppp);
            if let Some(gpu) = &self.gpu_text {
                gpu.reset();
            }
            self.applied_ppp = ppp;
            ctx.request_repaint();
        }

        ui.spacing_mut().item_spacing = Vec2::ZERO;

        // Acrylic blurs the actual windows behind this one; painting the
        // desktop wallpaper here would hide that. Image backgrounds still
        // draw, because the user explicitly chose them.
        if self.config.material != Material::Solid
            && self.config.background != BackgroundMode::None
            && !(self.config.material == Material::Acrylic
                && self.config.background == BackgroundMode::Desktop)
            && let Some(backdrop) = &self.backdrop
        {
            let screen = ui.max_rect();
            ui.painter().image(
                backdrop.texture.id(),
                screen,
                Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                Color32::WHITE,
            );
            let tint = backdrop::tint_alpha(self.config.material, self.config.background_opacity);
            ui.painter().rect_filled(
                screen,
                0.0,
                Color32::from_rgba_unmultiplied(
                    self.theme.bg.r,
                    self.theme.bg.g,
                    self.theme.bg.b,
                    tint,
                ),
            );
        }

        self.drain_events();
        self.sync_tabs();
        self.clamp_active();
        self.sync_live_search();
        self.window_focused = ctx
            .input(|input| input.viewport().focused)
            .unwrap_or(self.window_focused);
        self.update_window_title(&ctx);

        let tab_infos = self.tab_infos();
        let mut actions = chrome::show(
            &ctx,
            &mut self.chrome,
            &self.tokens,
            &tab_infos,
            &self.profiles,
            self.active,
            self.config.material,
        );

        let overlay_open = !matches!(self.overlays.kind, OverlayKind::None);
        let settings_active = matches!(self.tabs.get(self.active), Some(TabEntry::Settings));
        let keyboard_input = ctx.egui_wants_keyboard_input();
        route_clipboard_to_text_fields(&ctx);
        let mut shortcut_used = false;
        if shortcuts_allowed(overlay_open, settings_active, keyboard_input) {
            shortcut_used = self.handle_shortcuts(&ctx);
            self.handle_zoom_wheel(&ctx);
        }

        let titlebar_height = if self.tokens.titlebar_h > 0.0 {
            self.tokens.titlebar_h
        } else {
            40.0
        };
        let _ = ui.allocate_exact_size(vec2(ui.available_width(), titlebar_height), Sense::hover());

        let frame_now = ctx.input(|input| input.time);
        let mut terminal_actions = Vec::new();
        let mut terminal_rect = None;
        let mut context_menu_pos = None;
        let mut restart_requested = false;
        let mut close_requested = false;
        let mut settings_changed = false;

        if settings_active {
            let tokens = &self.tokens;
            let theme = self.theme;
            let config = &mut self.config;
            let overlay_state = &mut self.overlays;
            egui::CentralPanel::no_frame().show(ui, |ui| {
                settings_changed =
                    overlays::show_settings_panel(ui, tokens, theme, config, overlay_state);
            });
        } else {
            let active = self.active;
            let config = &self.config;
            let tokens = &self.tokens;
            let theme = self.theme;
            let atlas = &mut self.atlas;
            let tabs = &mut self.tabs;
            let gpu_text = self.gpu_text.as_ref();

            egui::CentralPanel::no_frame().show(ui, |ui| {
                let Some(TabEntry::Terminal(tab)) = tabs.get_mut(active) else {
                    let rect = ui.available_rect_before_wrap();
                    ui.painter().rect_filled(rect, 0.0, tokens.bg);
                    ui.painter().text(
                        rect.center(),
                        Align2::CENTER_CENTER,
                        "No sessions — Ctrl+Shift+T opens one",
                        FontId::proportional(14.0),
                        tokens.text_dim,
                    );
                    return;
                };

                let response =
                    tab.view
                        .show(ui, &tab.session, theme, config, tokens, atlas, gpu_text);
                terminal_rect = Some(response.response.rect);
                terminal_actions = response.actions;
                if response.response.secondary_clicked()
                    && (!tab.view.mouse_reporting() || ui.input(|input| input.modifiers.shift))
                {
                    context_menu_pos = response.response.interact_pointer_pos();
                }

                if !overlay_open
                    && !shortcut_used
                    && !tab.exited
                    && let Some(bytes) = tab.view.take_input(ui)
                {
                    tab.session.write(&bytes);
                }

                if tab.exited {
                    let card_size = vec2(360.0, 150.0);
                    let card = card_rect(response.response.rect, card_size);
                    egui::Area::new(Id::unique(("illness_exit_card", active)))
                        .order(Order::Middle)
                        .fixed_pos(card.min)
                        .show(ui.ctx(), |ui| {
                            ui.set_width(card.width());
                            egui::Frame::new()
                                .fill(tokens.bg_elev)
                                .stroke(Stroke::new(1.0, tokens.border))
                                .corner_radius(tokens.radius_md)
                                .inner_margin(16.0)
                                .show(ui, |ui| {
                                    ui.set_width((card.width() - 32.0).max(80.0));
                                    ui.set_min_height((card.height() - 32.0).max(40.0));
                                    ui.label(
                                        RichText::new("Session ended")
                                            .size(16.0)
                                            .color(tokens.text),
                                    );
                                    ui.add_space(4.0);
                                    ui.label(
                                        RichText::new(exit_detail(tab.exit_code))
                                            .size(12.0)
                                            .color(tokens.text_dim),
                                    );
                                    ui.add_space(12.0);
                                    ui.horizontal(|ui| {
                                        let restart = egui::Button::new(
                                            RichText::new("Restart").size(13.0).color(tokens.bg),
                                        )
                                        .fill(tokens.accent)
                                        .corner_radius(6.0)
                                        .min_size(vec2(88.0, 28.0));
                                        if ui.add(restart).clicked() {
                                            restart_requested = true;
                                        }

                                        let close = egui::Button::new(
                                            RichText::new("Close").size(13.0).color(tokens.text),
                                        )
                                        .fill(tokens.bg_hover)
                                        .corner_radius(6.0)
                                        .min_size(vec2(88.0, 28.0));
                                        if ui.add(close).clicked() {
                                            close_requested = true;
                                        }
                                    });
                                });
                        });
                }
            });
        }

        if settings_changed {
            self.reapply_config();
        }

        if self.bell_flash_until > frame_now
            && let Some(rect) = terminal_rect
        {
            let remaining = (self.bell_flash_until - frame_now) / BELL_FLASH_SECS;
            let alpha = (remaining.clamp(0.0, 1.0) * 180.0) as u8;
            let accent = self.tokens.accent;
            let color = Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), alpha);
            ui.painter()
                .rect_stroke(rect, 0.0, Stroke::new(2.0, color), StrokeKind::Inside);
            ctx.request_repaint_after(Duration::from_millis(40));
        }

        for action in terminal_actions {
            self.handle_terminal_action(action);
        }

        if restart_requested {
            self.restart_tab(self.active);
        }
        if close_requested {
            self.close_tab(self.active);
        }

        if let Some(pos) = context_menu_pos
            && matches!(self.overlays.kind, OverlayKind::None)
        {
            self.overlays.kind = OverlayKind::ContextMenu(pos);
        }

        if !overlay_open {
            self.resize_edges(ui);
        }

        actions.extend(overlays::show(
            &ctx,
            &mut self.overlays,
            &self.tokens,
            &mut self.config,
            &self.profiles,
            &tab_infos,
        ));

        for action in actions {
            self.handle_action(action);
        }

        let now = ctx.input(|input| input.time);
        let pointer_released = ctx.input(|input| input.pointer.any_released());
        if self.save_debounce.take_due(now, pointer_released) {
            self.persist();
        }

        if self.pending_material {
            self.pending_material = false;
            let window = Arc::clone(&self.window);
            self.apply_material_to(window.as_ref());
        }

        self.clamp_active();
        self.update_window_title(&ctx);
        self.sync_focus();
    }

    pub fn clear_color(&self) -> [f32; 4] {
        match self.config.material {
            Material::Solid => design::color(self.theme.bg).to_normalized_gamma_f32(),
            // Mica without a backdrop paints nothing in the UI, so a
            // transparent clear would leave a fully see-through window.
            Material::Mica if self.backdrop.is_none() => {
                design::color(self.theme.bg).to_normalized_gamma_f32()
            }
            Material::Mica | Material::Acrylic => [0.0, 0.0, 0.0, 0.0],
        }
    }
}

/// Whether the app should hold an in-window backdrop texture for this
/// material/mode pair. Acrylic+Desktop uses native behind-window blur.
fn needs_backdrop(material: Material, mode: BackgroundMode) -> bool {
    material != Material::Solid
        && mode != BackgroundMode::None
        && !(material == Material::Acrylic && mode == BackgroundMode::Desktop)
}

fn spawn_spec(profile: &Profile, config: &Config) -> SpawnSpec {
    SpawnSpec {
        program: profile.program.clone(),
        args: profile.args.clone(),
        cwd: std::env::current_dir().ok(),
        env: vec![
            ("TERM".to_string(), "xterm-256color".to_string()),
            ("COLORTERM".to_string(), "truecolor".to_string()),
        ],
        scrollback: config.scrollback,
        cursor: config.cursor,
    }
}

/// Keep profiles saved in `config.toml` and append detected built-ins whose
/// names are not present yet. Saved values win on case-insensitive name
/// collisions, saved profiles stay first, and names the user removed or
/// renamed away are never re-added.
fn merge_profiles(saved: Vec<Profile>, detected: Vec<Profile>, hidden: &[String]) -> Vec<Profile> {
    let mut merged = saved;
    illness_core::config::dedupe_profiles(&mut merged);
    for profile in detected {
        let hidden = hidden
            .iter()
            .any(|name| name.trim().eq_ignore_ascii_case(profile.name.trim()));
        let present = merged.iter().any(|existing| {
            existing
                .name
                .trim()
                .eq_ignore_ascii_case(profile.name.trim())
        });
        if !hidden && !present {
            merged.push(profile);
        }
    }
    merged
}

/// Resolve which profile a restart should launch.
///
/// When the profile still exists, its current program and args win. If it was
/// renamed or removed, use the values captured when the tab was spawned so
/// Restart keeps the same shell instead of falling back to the default.
fn restart_profile(
    name: &str,
    program: &str,
    args: &[String],
    accent: Rgb,
    profiles: &[Profile],
) -> Profile {
    profiles
        .iter()
        .find(|profile| profile.name.eq_ignore_ascii_case(name))
        .cloned()
        .unwrap_or_else(|| Profile {
            name: name.to_string(),
            program: program.to_string(),
            args: args.to_vec(),
            accent,
        })
}

/// Point `default_profile` at a profile whose name still exists.
///
/// This runs on load only. Running it during edits would silently switch the
/// default shell when the current profile's Program field is briefly empty.
fn ensure_default_profile(config: &mut Config) {
    let current = config.default_profile.trim();
    let valid = !current.is_empty()
        && config
            .profiles
            .iter()
            .any(|profile| profile.name.eq_ignore_ascii_case(current));
    if valid {
        return;
    }

    config.default_profile = config
        .profiles
        .iter()
        .find(|profile| !profile.program.trim().is_empty())
        .or_else(|| config.profiles.first())
        .map(|profile| profile.name.clone())
        .unwrap_or_default();
}

fn restarted_state(profile: &Profile) -> (String, bool, Option<i32>) {
    (profile.name.clone(), false, None)
}

fn cell_pixel(value: u32) -> u16 {
    u16::try_from(value.clamp(1, u32::from(u16::MAX))).unwrap_or(u16::MAX)
}

fn material_name(material: Material) -> &'static str {
    match material {
        Material::Mica => "Mica",
        Material::Acrylic => "Acrylic",
        Material::Solid => "Solid",
    }
}

fn next_material(material: Material) -> Material {
    match material {
        Material::Mica => Material::Acrylic,
        Material::Acrylic => Material::Solid,
        Material::Solid => Material::Mica,
    }
}

fn trim_copy(text: &str) -> String {
    text.split('\n')
        .map(|line| line.trim_end_matches([' ', '\r']))
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_paste(text: &str, bracketed: bool) -> Vec<u8> {
    encode_paste(text, bracketed)
}

fn window_title_for(title: Option<&str>) -> String {
    match title.filter(|title| !title.is_empty()) {
        Some(title) => format!("{title} — Illness"),
        None => "Illness".to_string(),
    }
}

/// ConPTY sometimes reports the executable path instead of a window title.
/// Fall back to the profile name when the raw value is empty or looks like a
/// program path rather than a user-facing title.
fn display_title(raw: &str, profile: &str) -> String {
    let trimmed = raw.trim();
    let lower = trimmed.to_ascii_lowercase();
    let is_program = trimmed.is_empty()
        || lower.ends_with(".exe")
        || lower.contains("\\powershell\\")
        || lower.contains("/powershell/");
    if is_program {
        profile.to_string()
    } else {
        trimmed.to_string()
    }
}

/// Advance the search match counter, wrapping in either direction.
fn cycle_search(current: usize, total: usize, forward: bool) -> usize {
    if total == 0 {
        return 0;
    }
    if forward {
        current % total + 1
    } else if current <= 1 {
        total
    } else {
        current - 1
    }
}

/// Lines to scroll so a match returned in viewport coordinates becomes
/// visible. Positive scrolls into history, negative toward newer output.
fn search_scroll_delta(row: i32, rows: usize) -> Option<i32> {
    if rows == 0 {
        return None;
    }
    if row < 0 {
        Some(row.saturating_neg())
    } else if row >= i32::try_from(rows).unwrap_or(i32::MAX) {
        let last = i32::try_from(rows.saturating_sub(1)).unwrap_or(i32::MAX);
        Some(last.saturating_sub(row))
    } else {
        None
    }
}

fn count_substring(haystack: &str, needle: &str, case_sensitive: bool) -> usize {
    let haystack: Vec<char> = haystack.chars().collect();
    let needle: Vec<char> = needle.chars().collect();
    if needle.is_empty() || needle.len() > haystack.len() {
        return 0;
    }

    let mut count = 0;
    let mut index = 0;
    while index + needle.len() <= haystack.len() {
        let matched = needle.iter().enumerate().all(|(offset, wanted)| {
            let got = haystack[index + offset];
            if case_sensitive {
                got == *wanted
            } else {
                got.eq_ignore_ascii_case(wanted)
            }
        });
        if matched {
            count += 1;
            index += needle.len();
        } else {
            index += 1;
        }
    }
    count
}

/// Non-overlapping match count across every visible row, mirroring the
/// highlight semantics in `terminal::paint_search`.
fn count_visible_matches(
    snapshot: &illness_term::Snapshot,
    query: &str,
    case_sensitive: bool,
) -> usize {
    if query.is_empty() || snapshot.cols == 0 {
        return 0;
    }
    snapshot
        .cells
        .chunks(snapshot.cols)
        .take(snapshot.rows)
        .map(|cells| {
            let row: String = cells
                .iter()
                .map(|cell| {
                    if cell.flags.contains(CellFlags::WIDE_CHAR_SPACER) {
                        ' '
                    } else {
                        cell.ch
                    }
                })
                .collect();
            count_substring(&row, query, case_sensitive)
        })
        .sum()
}

fn exit_detail(exit_code: Option<i32>) -> String {
    match exit_code {
        Some(code) => format!("Exit code {code}"),
        None => "Process exited".to_string(),
    }
}

fn zoom_size(size: f32, delta: f32) -> f32 {
    if size.is_finite() {
        (size + delta).clamp(8.0, 40.0)
    } else {
        DEFAULT_FONT_SIZE
    }
}

fn wheel_zoom_direction(notches: f32) -> Option<f32> {
    if notches.is_finite() && notches.abs() >= 0.5 {
        Some(notches.signum())
    } else {
        None
    }
}

fn color_response(index: usize, theme: &Theme) -> String {
    let (prefix, color) = match index {
        256 => ("10".to_string(), theme.fg),
        257 => ("11".to_string(), theme.bg),
        258 => ("12".to_string(), theme.cursor),
        palette => (format!("4;{}", palette % 256), theme.ansi[palette % 16]),
    };
    let red = u16::from(color.r) * 257;
    let green = u16::from(color.g) * 257;
    let blue = u16::from(color.b) * 257;
    format!("\x1b]{prefix};rgb:{red:04x}/{green:04x}/{blue:04x}\x07")
}

fn card_rect(bounds: Rect, size: Vec2) -> Rect {
    let width = size.x.min((bounds.width() - 16.0).max(1.0));
    let height = size.y.min((bounds.height() - 16.0).max(1.0));
    let left = bounds.left() + 8.0;
    let top = bounds.top() + 8.0;
    let max_x = (bounds.right() - width - 8.0).max(left);
    let max_y = (bounds.bottom() - height - 8.0).max(top);
    Rect::from_min_size(
        pos2(
            (bounds.center().x - width * 0.5).clamp(left, max_x),
            (bounds.center().y - height * 0.5).clamp(top, max_y),
        ),
        vec2(width, height),
    )
}

/// New active strip index after removing `closing` from `remaining` entries.
fn next_active_after_close(active: usize, closing: usize, remaining: usize) -> usize {
    if remaining == 0 {
        0
    } else if closing < active {
        active - 1
    } else if closing == active {
        closing.min(remaining - 1)
    } else {
        active
    }
}

fn active_after_move(active: usize, from: usize, to: usize) -> usize {
    if active == from {
        to
    } else if from < active && to >= active {
        active - 1
    } else if from > active && to <= active {
        active + 1
    } else {
        active
    }
}

/// Global shortcuts stay out of the way while a text field owns the keyboard
/// or while Settings is open (Ctrl+W must not close Settings, Ctrl+1..9 must
/// not switch tabs, and Ctrl+Shift+F must not steal the search key).
fn shortcuts_allowed(overlay_open: bool, settings_active: bool, keyboard_input: bool) -> bool {
    !overlay_open && !settings_active && !keyboard_input
}

/// Translate Ctrl+C/X/V into the clipboard events `TextEdit` listens for.
///
/// The app runs with `clipboard_shortcuts: false` so the terminal can keep
/// Ctrl+C as input; that also disables egui-winit's translation. When a text
/// field has focus, consume the key and synthesize the event instead.
fn route_clipboard_to_text_fields(ctx: &egui::Context) {
    if !ctx.text_edit_focused() {
        return;
    }

    let (mut copy, mut cut, mut paste) = (false, false, false);
    ctx.input_mut(|input| {
        input.events.retain(|event| {
            let Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } = event
            else {
                return true;
            };
            if !(modifiers.ctrl || modifiers.command) || modifiers.alt {
                return true;
            }
            match key {
                Key::C => {
                    copy = true;
                    false
                }
                Key::X => {
                    cut = true;
                    false
                }
                Key::V => {
                    paste = true;
                    false
                }
                _ => true,
            }
        });
    });

    if cut || copy {
        ctx.input_mut(|input| {
            input
                .events
                .push(if cut { Event::Cut } else { Event::Copy });
        });
    }
    if paste {
        match arboard::Clipboard::new().and_then(|mut clipboard| clipboard.get_text()) {
            Ok(text) if !text.is_empty() => {
                ctx.input_mut(|input| input.events.push(Event::Paste(text)));
            }
            Ok(_) => {}
            Err(error) => log::debug!("clipboard paste unavailable: {error}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shortcut {
    NewTab,
    CloseTab,
    NextTab,
    PrevTab,
    SelectTab(usize),
    Palette,
    Search,
    Settings,
    ZoomIn,
    ZoomOut,
    ZoomReset,
    Copy,
    Paste,
}

/// Map a key press to an app shortcut. Exact modifier matching keeps Ctrl+Shift
/// shortcuts from being swallowed by their Ctrl-only relatives.
fn shortcut_for(key: Key, modifiers: Modifiers, has_selection: bool) -> Option<Shortcut> {
    let ctrl = modifiers.ctrl || modifiers.command;
    if !ctrl || modifiers.alt {
        return None;
    }
    let shift = modifiers.shift;

    match key {
        Key::T if shift => Some(Shortcut::NewTab),
        Key::W if shift => Some(Shortcut::CloseTab),
        Key::P if shift => Some(Shortcut::Palette),
        Key::F if shift => Some(Shortcut::Search),
        Key::C if shift => Some(Shortcut::Copy),
        Key::V if shift => Some(Shortcut::Paste),
        Key::Tab if shift => Some(Shortcut::PrevTab),
        Key::Tab => Some(Shortcut::NextTab),
        Key::C if !shift && has_selection => Some(Shortcut::Copy),
        Key::Comma if !shift => Some(Shortcut::Settings),
        Key::Equals | Key::Plus => Some(Shortcut::ZoomIn),
        Key::Minus => Some(Shortcut::ZoomOut),
        Key::Num0 if !shift => Some(Shortcut::ZoomReset),
        Key::Num1 if !shift => Some(Shortcut::SelectTab(0)),
        Key::Num2 if !shift => Some(Shortcut::SelectTab(1)),
        Key::Num3 if !shift => Some(Shortcut::SelectTab(2)),
        Key::Num4 if !shift => Some(Shortcut::SelectTab(3)),
        Key::Num5 if !shift => Some(Shortcut::SelectTab(4)),
        Key::Num6 if !shift => Some(Shortcut::SelectTab(5)),
        Key::Num7 if !shift => Some(Shortcut::SelectTab(6)),
        Key::Num8 if !shift => Some(Shortcut::SelectTab(7)),
        Key::Num9 if !shift => Some(Shortcut::SelectTab(8)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctrl() -> Modifiers {
        Modifiers {
            alt: false,
            ctrl: true,
            shift: false,
            mac_cmd: false,
            command: true,
        }
    }

    fn ctrl_shift() -> Modifiers {
        Modifiers {
            shift: true,
            ..ctrl()
        }
    }

    fn profile(name: &str) -> Profile {
        Profile {
            name: name.to_string(),
            program: "pwsh.exe".to_string(),
            args: Vec::new(),
            accent: Rgb::new(0x7e, 0xe7, 0x87),
        }
    }

    #[test]
    fn merge_keeps_saved_profiles_first() {
        let saved = vec![profile("Mine")];
        let detected = vec![profile("Command Prompt")];
        let merged = merge_profiles(saved, detected, &[]);
        assert_eq!(
            merged
                .iter()
                .map(|profile| profile.name.as_str())
                .collect::<Vec<_>>(),
            ["Mine", "Command Prompt"]
        );
    }

    #[test]
    fn merge_saved_wins_on_same_name() {
        let mut saved = profile("PowerShell");
        saved.program = "custom-pwsh.exe".to_string();
        let mut detected = profile("powershell");
        detected.program = "detected-pwsh.exe".to_string();

        let merged = merge_profiles(vec![saved], vec![detected], &[]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].name, "PowerShell");
        assert_eq!(merged[0].program, "custom-pwsh.exe");
    }

    #[test]
    fn merge_appends_missing_detected_profiles() {
        let saved = vec![profile("PowerShell")];
        let detected = vec![
            profile("PowerShell"),
            profile("Command Prompt"),
            profile("Ubuntu"),
        ];

        let merged = merge_profiles(saved, detected, &[]);
        assert_eq!(
            merged
                .iter()
                .map(|profile| profile.name.as_str())
                .collect::<Vec<_>>(),
            ["PowerShell", "Command Prompt", "Ubuntu"]
        );
    }

    #[test]
    fn merge_uses_detected_without_saved_profiles() {
        let merged = merge_profiles(Vec::new(), vec![profile("PowerShell")], &[]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].name, "PowerShell");
    }

    #[test]
    fn merge_does_not_readd_hidden_detected_profiles() {
        let detected = vec![
            profile("PowerShell"),
            profile("Windows PowerShell"),
            profile("Command Prompt"),
        ];
        let hidden = vec!["powershell".to_string(), "COMMAND PROMPT".to_string()];

        let merged = merge_profiles(Vec::new(), detected, &hidden);
        assert_eq!(
            merged
                .iter()
                .map(|profile| profile.name.as_str())
                .collect::<Vec<_>>(),
            ["Windows PowerShell"]
        );
    }

    #[test]
    fn merge_deduplicates_saved_profiles() {
        let saved = vec![profile("Mine"), profile("mine")];
        let merged = merge_profiles(saved, Vec::new(), &[]);
        assert_eq!(merged.len(), 1);
    }

    #[test]
    fn ensure_default_repairs_missing_names_but_not_empty_programs() {
        let mut config = Config {
            profiles: vec![
                Profile {
                    program: String::new(),
                    ..profile("Broken")
                },
                profile("PowerShell"),
            ],
            default_profile: "Broken".to_string(),
            ..Config::default()
        };

        // The name exists, so an empty Program field must not repoint the shell.
        ensure_default_profile(&mut config);
        assert_eq!(config.default_profile, "Broken");

        let mut missing = Config {
            profiles: vec![profile("PowerShell")],
            default_profile: "missing".to_string(),
            ..Config::default()
        };
        ensure_default_profile(&mut missing);
        assert_eq!(missing.default_profile, "PowerShell");

        let mut empty = Config {
            default_profile: "missing".to_string(),
            ..Config::default()
        };
        ensure_default_profile(&mut empty);
        assert!(empty.default_profile.is_empty());
    }

    #[test]
    fn restart_falls_back_to_the_spawned_program_after_a_rename() {
        let profiles = vec![profile("Renamed")];
        let args = vec!["-d".to_string(), "Ubuntu".to_string()];
        let fallback = restart_profile(
            "PowerShell",
            "pwsh.exe",
            &args,
            Rgb::new(0x7e, 0xe7, 0x87),
            &profiles,
        );
        assert_eq!(fallback.name, "PowerShell");
        assert_eq!(fallback.program, "pwsh.exe");
        assert_eq!(fallback.args, args);

        let resolved = restart_profile("Renamed", "old.exe", &[], Rgb::new(0, 0, 0), &profiles);
        assert_eq!(resolved.program, "pwsh.exe");
    }

    #[test]
    fn save_debounce_waits_or_flushes_on_pointer_release() {
        let mut debounce = SaveDebounce::default();
        assert!(!debounce.take_due(1.0, false));

        debounce.mark(10.0);
        assert!(!debounce.take_due(10.1, false));
        assert!(debounce.take_due(10.31, false));
        assert!(!debounce.take_due(10.32, false));

        debounce.mark(20.0);
        assert!(debounce.take_due(20.05, true), "release flushes early");
    }

    #[test]
    fn shortcuts_are_off_in_settings_and_text_fields() {
        assert!(shortcuts_allowed(false, false, false));
        assert!(!shortcuts_allowed(true, false, false));
        assert!(!shortcuts_allowed(false, true, false));
        assert!(!shortcuts_allowed(false, false, true));
    }

    #[test]
    fn shortcut_table_matches_the_spec() {
        assert_eq!(
            shortcut_for(Key::T, ctrl_shift(), false),
            Some(Shortcut::NewTab)
        );
        assert_eq!(
            shortcut_for(Key::W, ctrl_shift(), false),
            Some(Shortcut::CloseTab)
        );
        assert_eq!(
            shortcut_for(Key::Tab, ctrl(), false),
            Some(Shortcut::NextTab)
        );
        assert_eq!(
            shortcut_for(Key::Tab, ctrl_shift(), false),
            Some(Shortcut::PrevTab)
        );
        assert_eq!(
            shortcut_for(Key::Num1, ctrl(), false),
            Some(Shortcut::SelectTab(0))
        );
        assert_eq!(
            shortcut_for(Key::Num9, ctrl(), false),
            Some(Shortcut::SelectTab(8))
        );
        assert_eq!(
            shortcut_for(Key::P, ctrl_shift(), false),
            Some(Shortcut::Palette)
        );
        assert_eq!(
            shortcut_for(Key::F, ctrl_shift(), false),
            Some(Shortcut::Search)
        );
        assert_eq!(
            shortcut_for(Key::Comma, ctrl(), false),
            Some(Shortcut::Settings)
        );
        assert_eq!(
            shortcut_for(Key::Equals, ctrl(), false),
            Some(Shortcut::ZoomIn)
        );
        assert_eq!(
            shortcut_for(Key::Plus, ctrl_shift(), false),
            Some(Shortcut::ZoomIn)
        );
        assert_eq!(
            shortcut_for(Key::Minus, ctrl(), false),
            Some(Shortcut::ZoomOut)
        );
        assert_eq!(
            shortcut_for(Key::Num0, ctrl(), false),
            Some(Shortcut::ZoomReset)
        );
        assert_eq!(
            shortcut_for(Key::C, ctrl_shift(), false),
            Some(Shortcut::Copy)
        );
        assert_eq!(
            shortcut_for(Key::V, ctrl_shift(), false),
            Some(Shortcut::Paste)
        );
    }

    #[test]
    fn ctrl_c_is_only_a_shortcut_when_text_is_selected() {
        assert_eq!(shortcut_for(Key::C, ctrl(), false), None);
        assert_eq!(shortcut_for(Key::C, ctrl(), true), Some(Shortcut::Copy));
        assert_eq!(shortcut_for(Key::T, ctrl(), false), None);
        assert_eq!(
            shortcut_for(
                Key::T,
                Modifiers {
                    alt: true,
                    ..ctrl_shift()
                },
                false
            ),
            None
        );
    }

    #[test]
    fn test_close_last_tab() {
        assert_eq!(next_active_after_close(0, 0, 0), 0);
        assert_eq!(next_active_after_close(0, 0, 1), 0);
        assert_eq!(next_active_after_close(1, 1, 1), 0);
        assert_eq!(next_active_after_close(1, 0, 2), 0);
        assert_eq!(next_active_after_close(2, 1, 3), 1);
        assert_eq!(next_active_after_close(2, 3, 3), 2);
        assert_eq!(next_active_after_close(0, 1, 2), 0);
    }

    #[test]
    fn test_title_update() {
        assert_eq!(window_title_for(Some("PowerShell")), "PowerShell — Illness");
        assert_eq!(window_title_for(None), "Illness");
        assert_eq!(window_title_for(Some("")), "Illness");
    }

    #[test]
    fn test_display_title_falls_back_to_profile() {
        assert_eq!(
            display_title("C:\\Program Files\\PowerShell\\7\\pwsh.exe", "PowerShell"),
            "PowerShell"
        );
        assert_eq!(
            display_title("C:\\Users\\me", "PowerShell"),
            "C:\\Users\\me"
        );
        assert_eq!(display_title("", "PowerShell"), "PowerShell");
        assert_eq!(display_title("   ", "PowerShell"), "PowerShell");
        assert_eq!(display_title("  ~ illness  ", "PowerShell"), "~ illness");
    }

    #[test]
    fn count_substring_is_non_overlapping() {
        assert_eq!(count_substring("aaaa", "aa", true), 2);
        assert_eq!(count_substring("AbcABC", "abc", false), 2);
        assert_eq!(count_substring("AbcABC", "abc", true), 0);
        assert_eq!(count_substring("anything", "", false), 0);
    }

    #[test]
    fn count_visible_matches_reads_rows() {
        let mut snapshot = illness_term::Snapshot::empty(4, 2);
        for (index, ch) in ['a', 'b', 'a', 'b', 'a', 'a', 'a', 'b']
            .into_iter()
            .enumerate()
        {
            snapshot.cells[index].ch = ch;
        }
        assert_eq!(count_visible_matches(&snapshot, "ab", false), 3);
        assert_eq!(count_visible_matches(&snapshot, "aa", false), 1);
    }

    #[test]
    fn test_exit_restart() {
        assert_eq!(exit_detail(Some(3)), "Exit code 3");
        assert_eq!(exit_detail(None), "Process exited");

        let source = profile("PowerShell");
        let (title, exited, exit_code) = restarted_state(&source);
        assert_eq!(title, "PowerShell");
        assert!(!exited);
        assert_eq!(exit_code, None);
    }

    #[test]
    fn test_material_switch() {
        assert_eq!(next_material(Material::Mica), Material::Acrylic);
        assert_eq!(next_material(Material::Acrylic), Material::Solid);
        assert_eq!(next_material(Material::Solid), Material::Mica);
    }

    #[test]
    fn backdrop_is_needed_except_for_native_acrylic_desktop() {
        assert!(!needs_backdrop(Material::Acrylic, BackgroundMode::Desktop));
        assert!(!needs_backdrop(Material::Solid, BackgroundMode::Desktop));
        assert!(!needs_backdrop(Material::Mica, BackgroundMode::None));
        assert!(needs_backdrop(Material::Mica, BackgroundMode::Desktop));
        assert!(needs_backdrop(Material::Mica, BackgroundMode::Image));
        assert!(needs_backdrop(Material::Acrylic, BackgroundMode::Image));
    }

    #[test]
    fn test_trim_copy() {
        assert_eq!(trim_copy("abc   \r\ndef \r\n\r\n"), "abc\ndef\n\n");
        assert_eq!(trim_copy("a\tb  \n"), "a\tb\n");
        assert_eq!(trim_copy("nothing"), "nothing");
    }

    #[test]
    fn clipboard_paste_is_bracketed_when_requested() {
        assert_eq!(format_paste("a\nb", true), b"\x1b[200~a\nb\x1b[201~");
        assert_eq!(format_paste("a\nb", false), b"a\rb");
        assert_eq!(format_paste("a\r\nb", false), b"a\rb");
        assert_eq!(
            format_paste("a\x1b[201~\x03b", true),
            b"\x1b[200~a[201~b\x1b[201~"
        );
    }

    #[test]
    fn zoom_clamps_and_resets() {
        assert_eq!(zoom_size(14.0, 1.0), 15.0);
        assert_eq!(zoom_size(40.0, 1.0), 40.0);
        assert_eq!(zoom_size(8.0, -1.0), 8.0);
        assert_eq!(zoom_size(f32::NAN, 0.0), DEFAULT_FONT_SIZE);
        assert_eq!(wheel_zoom_direction(1.0), Some(1.0));
        assert_eq!(wheel_zoom_direction(-2.0), Some(-1.0));
        assert_eq!(wheel_zoom_direction(0.2), None);
        assert_eq!(wheel_zoom_direction(f32::NAN), None);
    }

    #[test]
    fn active_tab_follows_reorder() {
        assert_eq!(active_after_move(2, 2, 0), 0);
        assert_eq!(active_after_move(1, 0, 2), 0);
        assert_eq!(active_after_move(1, 2, 0), 2);
        assert_eq!(active_after_move(1, 0, 1), 0);
    }

    #[test]
    fn color_response_uses_sixteen_bit_channels() {
        let theme = Theme::by_name("Illness Dark");
        assert_eq!(color_response(256, theme), "\x1b]10;rgb:d6d6/d6d6/dddd\x07");
        assert_eq!(color_response(1, theme), "\x1b]4;1;rgb:f7f7/7676/8e8e\x07");
        assert_eq!(color_response(257, theme), "\x1b]11;rgb:0c0c/0c0c/0f0f\x07");
    }

    #[test]
    fn exit_card_stays_inside_the_terminal() {
        let bounds = Rect::from_min_size(pos2(0.0, 0.0), vec2(200.0, 100.0));
        let card = card_rect(bounds, vec2(360.0, 150.0));
        assert!(bounds.contains_rect(card));
    }

    #[test]
    fn search_counter_wraps_in_both_directions() {
        assert_eq!(cycle_search(1, 3, true), 2);
        assert_eq!(cycle_search(3, 3, true), 1);
        assert_eq!(cycle_search(1, 3, false), 3);
        assert_eq!(cycle_search(2, 3, false), 1);
        assert_eq!(cycle_search(1, 0, true), 0);
    }

    #[test]
    fn search_scrolls_toward_offscreen_matches() {
        assert_eq!(search_scroll_delta(2, 10), None);
        assert_eq!(search_scroll_delta(-5, 10), Some(5));
        assert_eq!(search_scroll_delta(12, 10), Some(-3));
        assert_eq!(search_scroll_delta(10, 10), Some(-1));
        assert_eq!(search_scroll_delta(0, 0), None);
    }

    #[test]
    fn new_tabs_use_configured_scrollback_and_cursor() {
        let profile = profile("PowerShell");
        let config = Config {
            scrollback: 25_000,
            cursor: illness_core::CursorKind::Beam,
            ..Config::default()
        };
        let spec = spawn_spec(&profile, &config);
        assert_eq!(spec.scrollback, 25_000);
        assert_eq!(spec.cursor, illness_core::CursorKind::Beam);
    }
}
