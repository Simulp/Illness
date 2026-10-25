use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

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
/// Base delay before retrying a native material after an apply failure. Each
/// consecutive failure doubles it (2s, 4s, 8s...), and automatic retries stop
/// after [`MATERIAL_RETRY_CAP`] failures.
const MATERIAL_RETRY_BASE: Duration = Duration::from_secs(2);
/// Consecutive apply failures after which automatic retries stop. Only an
/// explicit material choice, a real availability transition, or a successful
/// apply resets the streak.
const MATERIAL_RETRY_CAP: u32 = 3;
/// A warm running longer than this is treated as stuck (for example a dead UNC
/// path) and may be superseded by a later rebuild.
const WARM_STUCK_AFTER: Duration = Duration::from_secs(30);
/// Background warm failures are retried at most this many times automatically.
const WARM_RETRY_CAP: u32 = 3;
const WARM_RETRY_BASE: Duration = Duration::from_secs(2);

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

/// Bounded automatic retry of a native material whose apply failed.
///
/// The retry deadline uses host `Instant`s rather than egui's last-frame input
/// time, so arming after idle still waits the full backoff. Consecutive
/// failures abort when they reach [`MATERIAL_RETRY_CAP`]; every automatic retry
/// is also just a confusing repeated toast, so the app only surfaces the first.
#[derive(Debug, Default)]
struct MaterialRetryState {
    failures: u32,
    retry_at: Option<Instant>,
}

impl MaterialRetryState {
    fn reset(&mut self) {
        self.failures = 0;
        self.retry_at = None;
    }

    fn note_failure(&mut self, now: Instant, available: bool, requested: Material) {
        self.failures = self.failures.saturating_add(1);
        self.retry_at = if available && requested != Material::Solid {
            material_retry_backoff(self.failures).and_then(|delay| now.checked_add(delay))
        } else {
            None
        };
    }

    fn retry_due(
        &self,
        fallback: bool,
        available: bool,
        requested: Material,
        now: Instant,
    ) -> bool {
        fallback
            && available
            && requested != Material::Solid
            && !self.exhausted()
            && self.retry_at.is_none_or(|at| now >= at)
    }

    fn exhausted(&self) -> bool {
        self.failures >= MATERIAL_RETRY_CAP
    }

    #[cfg(test)]
    fn first_failure(&self) -> bool {
        self.failures == 1
    }

    fn clear_deadline(&mut self) {
        self.retry_at = None;
    }
}

fn material_retry_backoff(failures: u32) -> Option<Duration> {
    (failures > 0 && failures < MATERIAL_RETRY_CAP)
        .then(|| MATERIAL_RETRY_BASE * 2u32.pow(failures.saturating_sub(1)))
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

/// Result of a detached [`backdrop::warm_cache`] request.
#[derive(Debug)]
struct BackdropWarmResult {
    /// Monotonic request id. A result from a superseded background/path is
    /// ignored even if its `(mode, path)` happens to match again.
    request: u64,
    mode: BackgroundMode,
    path: String,
    result: Result<backdrop::WarmOutcome, String>,
}

/// One in-flight background backdrop warm.
struct BackdropWarm {
    request: u64,
    mode: BackgroundMode,
    path: String,
    started: Instant,
}

impl BackdropWarm {
    fn is_stuck(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.started) >= WARM_STUCK_AFTER
    }
}

/// The winit/egui application shell and all cross-module state.
pub struct IllnessApp {
    ctx: egui::Context,
    window: Arc<Window>,
    config: Config,
    profiles: Vec<Profile>,
    /// Whether the window can currently present native translucent materials:
    /// the swapchain exposes a transparent alpha mode and Windows transparency
    /// effects are enabled. The host keeps the static surface-alpha support and
    /// re-checks the effects setting when focus, theme or occlusion changes.
    native_transparency: bool,
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
    /// The user's material request last sent to the platform.
    applied_requested_material: Material,
    /// True while the requested material is rendered as Solid because native
    /// transparency is unavailable (or the native apply failed). This is never
    /// persisted and is cleared whenever the user explicitly chooses.
    material_fallback: bool,
    /// `(mode, path)` that produced the current backdrop texture.
    applied_background: Option<(BackgroundMode, String)>,
    applied_background_opacity: f32,
    /// Detached cache-warm result channel. The UI never decodes an image; it
    /// requests a warm and rebuilds only when the result arrives.
    backdrop_warm_tx: Sender<BackdropWarmResult>,
    backdrop_warm_rx: Receiver<BackdropWarmResult>,
    /// The `(request id, mode, path)` currently warming on a detached thread.
    backdrop_warming: Option<BackdropWarm>,
    /// Monotonic id for backdrop warm requests.
    backdrop_warm_seq: u64,
    /// Cancellation handle for the current warm. Superseded requests exit
    /// before opening or decoding a file.
    backdrop_warm_token: backdrop::WarmToken,
    /// Consecutive backdrop warm failures and the next automatic retry. The
    /// retry stops after [`WARM_RETRY_CAP`] so a deleted/dead source cannot
    /// loop forever.
    backdrop_warm_failures: u32,
    backdrop_warm_retry_at: Option<Instant>,
    /// Bounded automatic retry of a failed native material.
    material_retry: MaterialRetryState,
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
        native_transparency: bool,
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
        design::install(
            &ctx,
            theme,
            material_for_surface(config.material, native_transparency, false),
        );
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

        let (backdrop_warm_tx, backdrop_warm_rx) = mpsc::channel();
        let mut app = Self {
            ctx,
            window: Arc::clone(&window),
            config,
            profiles,
            native_transparency,
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
            applied_material: Material::Solid,
            applied_requested_material: Material::Solid,
            material_fallback: false,
            applied_background: None,
            applied_background_opacity: background_opacity,
            backdrop_warm_tx,
            backdrop_warm_rx,
            backdrop_warming: None,
            backdrop_warm_seq: 0,
            backdrop_warm_token: backdrop::WarmToken::new(),
            backdrop_warm_failures: 0,
            backdrop_warm_retry_at: None,
            material_retry: MaterialRetryState::default(),
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
                self.overlays.set_toast(error);
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
        let theme_bg = self.theme.bg;
        let opacity = self.config.background_opacity;
        // `config.material` is always the user's request. A session fallback is
        // represented by `material_fallback` and never rewritten into config,
        // so an explicit Solid is the only thing that persists as Solid.
        let requested = self.config.material;
        let attempted_native = requested != Material::Solid;
        let unavailable = attempted_native && !self.native_transparency;
        let was_fallback = self.material_fallback;
        // A theme change or other pending apply while the failure streak is
        // exhausted must stay on Solid; only an explicit selection or a real
        // availability transition may start a new native attempt.
        let exhausted_fallback = self.material_fallback && self.material_retry.exhausted();
        let mut effective =
            material_for_surface(requested, self.native_transparency, exhausted_fallback);
        if unavailable {
            self.material_fallback = true;
            if !was_fallback {
                self.overlays
                    .set_toast("Native materials unavailable; using Solid");
                log::warn!(
                    "surface alpha or Windows transparency effects unavailable; {} forced to Solid",
                    material_name(requested)
                );
            } else {
                log::debug!(
                    "{} still unavailable; Solid fallback remains",
                    material_name(requested)
                );
            }
        }
        let now = Instant::now();
        if let Err(error) = platform::apply(window, effective, dark, theme_bg, opacity) {
            if effective != Material::Solid {
                let first = self.material_retry.failures == 0;
                self.material_fallback = true;
                self.material_retry
                    .note_failure(now, self.native_transparency, requested);
                if first {
                    log::warn!(
                        "window material {} (dark={dark}) failed: {error}",
                        material_name(effective)
                    );
                    self.overlays.set_toast(format!(
                        "{} is unavailable here; using Solid",
                        material_name(requested)
                    ));
                } else {
                    log::debug!(
                        "window material {} retry {} failed: {error}",
                        material_name(effective),
                        self.material_retry.failures
                    );
                }
                effective = Material::Solid;
            } else {
                // A failed Solid apply cannot be improved by another attempt.
                log::warn!("solid material unavailable: {error}");
                self.material_retry.reset();
            }
            if let Err(error) = platform::apply(window, Material::Solid, dark, theme_bg, opacity) {
                log::warn!("solid material unavailable: {error}");
            }
        } else {
            log::info!(
                "window material {} (dark={dark}) applied",
                material_name(effective)
            );
            // Only a successful native apply (or an explicit Solid request)
            // ends the failure streak; applying the fallback Solid must not
            // reset the retry cap.
            if effective != Material::Solid || !attempted_native {
                self.material_fallback = false;
                self.material_retry.reset();
            }
        }
        if !attempted_native {
            self.material_fallback = false;
            self.material_retry.reset();
        }
        self.applied_material = effective;
        self.applied_requested_material = requested;
        if self.material_fallback {
            self.arm_material_retry();
        } else {
            self.material_retry.clear_deadline();
        }
        // A fallback to Solid leaves any prewarmed texture unused; drop it
        // without decoding or toasting. Acrylic+Desktop keeps its texture.
        self.drop_unneeded_backdrop();
        // The panel fill follows the effective material: opaque for Solid,
        // transparent so the native backdrop can show through otherwise. This
        // also covers a fallback to Solid when the request is unavailable.
        design::install(&self.ctx, self.theme, effective);
        self.ctx.request_repaint();
    }

    /// The material the window actually renders. `config.material` keeps the
    /// user's request; Solid is substituted while a native material cannot be
    /// presented or its apply failed.
    fn effective_material(&self) -> Material {
        material_for_surface(
            self.config.material,
            self.native_transparency,
            self.material_fallback,
        )
    }

    /// Re-check native transparency after the host observes a theme, focus or
    /// occlusion change. Turning effects off falls back immediately; turning
    /// them back on retries the requested Mica/Acrylic without a restart.
    ///
    /// A failed apply can leave `available == true` unchanged, so focus/theme
    /// notifications still retry after the bounded backoff rather than
    /// returning early forever. Only a real availability transition resets the
    /// failure streak; unchanged availability keeps counting toward the cap.
    pub fn set_native_transparency(&mut self, available: bool) -> bool {
        let now = Instant::now();
        let transition = self.native_transparency != available;
        let retry = self.material_retry.retry_due(
            self.material_fallback,
            available,
            self.config.material,
            now,
        );
        if !transition && !retry {
            return false;
        }
        if transition {
            self.material_retry.reset();
        } else {
            self.material_retry.clear_deadline();
        }
        self.native_transparency = available;
        // A stale fallback must not block a retry when effects return.
        self.material_fallback = false;
        let window = Arc::clone(&self.window);
        self.apply_material_to(window.as_ref());
        // A retry may need the painted backdrop that the Solid fallback
        // dropped; falling back drops it again without decoding.
        self.sync_backdrop_for_material(false);
        true
    }

    /// Schedule a repaint for the retry deadline `note_failure` computed.
    /// Effects being off means availability, not apply failure, is the cause;
    /// recovering waits for the host's transparency re-check instead.
    fn arm_material_retry(&mut self) {
        if let Some(at) = self.material_retry.retry_at {
            self.ctx
                .request_repaint_after(at.saturating_duration_since(Instant::now()));
        }
    }

    /// Retry a fallback material once its backoff expires. This covers sessions
    /// where no focus/theme event follows the failure, without polling. Stops
    /// permanently once the consecutive-failure cap is reached.
    fn maybe_retry_material(&mut self) {
        if !self.material_fallback
            || !self.native_transparency
            || self.config.material == Material::Solid
            || self.material_retry.exhausted()
        {
            self.material_retry.clear_deadline();
            return;
        }
        match self.material_retry.retry_at {
            Some(at) => {
                let now = Instant::now();
                if now >= at {
                    self.material_retry.clear_deadline();
                    self.pending_material = true;
                } else {
                    self.ctx.request_repaint_after(at - now);
                }
            }
            // A fallback without an armed deadline retries immediately.
            None => self.pending_material = true,
        }
    }

    /// Retry the requested native material after the user re-selects it while
    /// the session is in a Solid fallback.
    fn retry_material(&mut self) {
        if self.config.material == Material::Solid {
            return;
        }
        let requested = self.config.material;
        choose_material(
            &mut self.config.material,
            &mut self.material_fallback,
            requested,
        );
        self.material_retry.reset();
        self.pending_material = true;
        self.sync_backdrop_for_material(false);
    }

    /// Reload the backdrop for the current mode and path.
    ///
    /// Called when the mode, path, a gradient-producing theme, or the effective
    /// material changes; opacity is applied at paint time. Images are never
    /// decoded here: a warmed `(path, mtime)` entry uploads in a few
    /// milliseconds. A cold painted backdrop starts a detached warm and this
    /// pass paints the native material/theme with no texture, then the warm
    /// result requests a rebuild. A pair that paints no backdrop -- Solid,
    /// fallback Solid, any material with `BackgroundMode::None` -- drops the
    /// texture and starts no warm: a backdrop that never paints is never
    /// decoded. Acrylic+Desktop keeps/warms only for a later switch.
    fn rebuild_backdrop(&mut self) {
        self.rebuild_backdrop_inner(true);
    }

    /// `revalidate` schedules an mtime/existence check even when the path-only
    /// cache already produced a texture. Warm-result rebuilds pass `false` so an
    /// unchanged revalidation does not validate itself forever.
    fn rebuild_backdrop_inner(&mut self, revalidate: bool) {
        let mode = self.config.background;
        let path = self.config.background_path.clone();
        let material = self.effective_material();
        let plan = backdrop_plan(material, mode);
        if revalidate {
            self.backdrop_warm_failures = 0;
            self.backdrop_warm_retry_at = None;
        }
        // Resolve the live wallpaper sources once for this rebuild. The warm
        // thread resolves its own copy; the UI lookup stays path-only.
        let desktop = if mode == BackgroundMode::Desktop {
            backdrop::desktop_candidates()
        } else {
            Vec::new()
        };
        let stale_warm = self
            .backdrop_warming
            .as_ref()
            .is_some_and(|warming| warming.mode != mode || warming.path != path);
        if plan == BackdropPlan::Drop || stale_warm {
            self.cancel_backdrop_warm();
        }

        match plan {
            BackdropPlan::Drop => {
                // Solid never paints a texture and a None background has
                // nothing to paint. Drop it without decoding the wallpaper or
                // reporting a misleading image failure.
                self.backdrop = None;
                self.applied_background = Some((mode, path));
                self.applied_background_opacity = self.config.background_opacity;
                self.ctx.request_repaint();
                log::debug!("backdrop dropped for {material:?}+{mode:?}: not needed");
                return;
            }
            BackdropPlan::Keep => {
                // Acrylic+Desktop paints native blur instead, but an existing
                // texture is retained for a later switch to Mica/Image. The
                // cache is warmed with no texture upload so leaving Acrylic
                // only pays the upload.
                self.applied_background = Some((mode, path.clone()));
                self.applied_background_opacity = self.config.background_opacity;
                self.start_backdrop_warm_with(mode, path, false, &desktop);
                self.ctx.request_repaint();
                log::debug!("backdrop kept for {material:?}+{mode:?}: not painted");
                return;
            }
            BackdropPlan::Build => {}
        }

        let started = Instant::now();
        match backdrop::build_with(&self.ctx, self.theme, mode, &path, &desktop) {
            backdrop::BackdropBuild::Ready(backdrop) => {
                let from_image = backdrop
                    .as_ref()
                    .is_some_and(|backdrop| backdrop.from_image);
                self.backdrop = backdrop;
                if self.backdrop.is_none() && mode == BackgroundMode::Image {
                    self.overlays.set_toast(if path.trim().is_empty() {
                        "Choose a background image".to_string()
                    } else {
                        format!("Could not load background \"{}\"", path)
                    });
                }
                // A lower-priority Desktop hit still needs its preferred
                // wallpaper warmed; Image and gradient hits are complete. A
                // cached image is revalidated off-thread so an in-place
                // wallpaper replacement at the same path becomes visible.
                self.start_backdrop_warm_with(
                    mode,
                    path.clone(),
                    revalidate && from_image,
                    &desktop,
                );
            }
            backdrop::BackdropBuild::Warming => {
                self.backdrop = None;
                self.start_backdrop_warm_with(mode, path.clone(), false, &desktop);
            }
            backdrop::BackdropBuild::Failed(error) => {
                self.backdrop = None;
                self.overlays.set_toast(error);
            }
        }

        self.applied_background = Some((mode, path));
        self.applied_background_opacity = self.config.background_opacity;
        self.ctx.request_repaint();
        log::debug!(
            "backdrop rebuilt for {mode:?} in {:?} (image={})",
            started.elapsed(),
            self.backdrop
                .as_ref()
                .is_some_and(|backdrop| backdrop.from_image)
        );
    }

    /// Decode/blur the current backdrop on a detached thread.
    ///
    /// Used only when the effective material paints the texture now, or for the
    /// Acrylic+Desktop cache-only prewarm. A cold Mica/Image switch paints the
    /// native material/theme until the warm result uploads. Solid/fallback and
    /// None never reach this method with a request that would decode.
    fn start_backdrop_warm(&mut self, mode: BackgroundMode, path: String) {
        let desktop = if mode == BackgroundMode::Desktop {
            backdrop::desktop_candidates()
        } else {
            Vec::new()
        };
        self.start_backdrop_warm_with(mode, path, false, &desktop);
    }

    fn start_backdrop_warm_with(
        &mut self,
        mode: BackgroundMode,
        path: String,
        revalidate: bool,
        desktop: &[std::path::PathBuf],
    ) {
        if mode == BackgroundMode::None {
            return;
        }
        if mode == BackgroundMode::Image && path.trim().is_empty() {
            return;
        }
        if !revalidate && !backdrop::needs_warm_with(mode, &path, desktop) {
            return;
        }
        if let Some(warming) = &self.backdrop_warming
            && warming.mode == mode
            && warming.path == path
        {
            let now = Instant::now();
            let elapsed = now.saturating_duration_since(warming.started);
            if !warming.is_stuck(now) {
                self.ctx.request_repaint_after(WARM_STUCK_AFTER - elapsed);
                return;
            }
            log::warn!("backdrop warm {mode:?} has been running for {elapsed:?}; superseding it");
        }
        // A different request supersedes the old one. Cancelling lets its
        // worker exit before opening another file instead of decoding work the
        // UI can no longer use.
        self.cancel_backdrop_warm();

        self.backdrop_warm_seq = self.backdrop_warm_seq.wrapping_add(1);
        let request = self.backdrop_warm_seq;
        let token = backdrop::WarmToken::new();
        self.backdrop_warm_token = token.clone();
        let theme = self.theme;
        let ctx = self.ctx.clone();
        let tx = self.backdrop_warm_tx.clone();
        let worker_path = path.clone();
        self.backdrop_warming = Some(BackdropWarm {
            request,
            mode,
            path: path.clone(),
            started: Instant::now(),
        });
        let spawned = std::thread::Builder::new()
            .name("illness-backdrop-warm".to_string())
            .spawn(move || {
                let started = Instant::now();
                // A panic in the decoder must not leave the pair marked as
                // warming forever; turn it into a normal error result.
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    backdrop::warm_cache(theme, mode, &worker_path, &token)
                }))
                .unwrap_or_else(|_| Err("Background warm failed unexpectedly".to_string()));
                log::debug!(
                    "backdrop warm {mode:?} finished in {:?}: {result:?}",
                    started.elapsed()
                );
                let _ = tx.send(BackdropWarmResult {
                    request,
                    mode,
                    path: worker_path,
                    result,
                });
                ctx.request_repaint();
            });
        if let Err(error) = spawned {
            log::warn!("could not start the backdrop warm thread: {error}");
            self.backdrop_warming = None;
            self.note_backdrop_warm_failure(
                mode,
                &path,
                format!("Could not start the background load: {error}"),
            );
        } else {
            self.ctx.request_repaint_after(WARM_STUCK_AFTER);
        }
    }

    /// Cancel the active backdrop warm, if any. Its result is stale and ignored.
    fn cancel_backdrop_warm(&mut self) {
        if self.backdrop_warming.take().is_some() {
            self.backdrop_warm_token.cancel();
        }
    }

    /// Supersede a warm that has outlived [`WARM_STUCK_AFTER`], so a dead UNC
    /// path cannot keep a pair marked as warming forever. The old worker is
    /// cancelled and its result ignored; a still-missing source can start over.
    fn poll_backdrop_warm(&mut self) {
        let Some(warming) = &self.backdrop_warming else {
            return;
        };
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(warming.started);
        if !warming.is_stuck(now) {
            self.ctx.request_repaint_after(WARM_STUCK_AFTER - elapsed);
            return;
        }
        let mode = warming.mode;
        let path = warming.path.clone();
        log::warn!("backdrop warm {mode:?} timed out after {elapsed:?}; retrying");
        self.cancel_backdrop_warm();
        let desktop = if mode == BackgroundMode::Desktop {
            backdrop::desktop_candidates()
        } else {
            Vec::new()
        };
        if paints_backdrop(self.effective_material(), mode) {
            self.start_backdrop_warm_with(mode, path, false, &desktop);
        }
    }

    /// Retry a failed background warm a bounded number of times. A deleted
    /// image or unreachable UNC is retried after a short delay, then left
    /// alone until the user rebuilds the background.
    fn maybe_retry_backdrop_warm(&mut self) {
        let Some(at) = self.backdrop_warm_retry_at else {
            return;
        };
        if self.backdrop_warming.is_some() {
            return;
        }
        let now = Instant::now();
        if now < at {
            self.ctx.request_repaint_after(at - now);
            return;
        }
        self.backdrop_warm_retry_at = None;
        let mode = self.config.background;
        if !paints_backdrop(self.effective_material(), mode) {
            return;
        }
        let desktop = if mode == BackgroundMode::Desktop {
            backdrop::desktop_candidates()
        } else {
            Vec::new()
        };
        self.start_backdrop_warm_with(mode, self.config.background_path.clone(), false, &desktop);
    }

    fn note_backdrop_warm_failure(&mut self, mode: BackgroundMode, path: &str, error: String) {
        // A warm for a material that will render Solid must not toast an image
        // error the user cannot see, nor keep retrying unseen work.
        if !paints_backdrop(self.effective_material(), mode) {
            log::debug!("backdrop warm {mode:?} failed while unpainted: {error}");
            return;
        }
        self.backdrop_warm_failures = self.backdrop_warm_failures.saturating_add(1);
        if self.backdrop_warm_failures == 1 {
            log::warn!("backdrop warm {mode:?} for \"{path}\" failed: {error}");
            warm_error_toast(&self.ctx, &mut self.overlays, error);
        } else {
            log::debug!(
                "backdrop warm {mode:?} retry {} failed: {error}",
                self.backdrop_warm_failures
            );
        }
        self.backdrop_warm_retry_at = if self.backdrop_warm_failures < WARM_RETRY_CAP {
            Some(
                Instant::now()
                    + WARM_RETRY_BASE * 2u32.pow(self.backdrop_warm_failures.saturating_sub(1)),
            )
        } else {
            None
        };
        if let Some(at) = self.backdrop_warm_retry_at {
            self.ctx
                .request_repaint_after(at.saturating_duration_since(Instant::now()));
        }
    }

    /// Apply finished detached warm results. A matching result uploads the
    /// warmed texture; a stale result is ignored because the background or
    /// material changed while it ran.
    fn drain_backdrop_warm(&mut self) {
        while let Ok(result) = self.backdrop_warm_rx.try_recv() {
            let current = self.backdrop_warming.as_ref().is_some_and(|warming| {
                warming.request == result.request
                    && warming.mode == result.mode
                    && warming.path == result.path
            });
            if current {
                self.backdrop_warming = None;
            }
            if !current
                || result.mode != self.config.background
                || result.path != self.config.background_path
            {
                continue;
            }
            match result.result {
                Ok(outcome) => {
                    self.backdrop_warm_failures = 0;
                    self.backdrop_warm_retry_at = None;
                    // A cold build painted no texture while another warm filled
                    // the cache, so upload even when the decode was unchanged.
                    if paints_backdrop(self.effective_material(), result.mode)
                        && (outcome.changed || self.backdrop.is_none())
                    {
                        self.rebuild_backdrop_inner(false);
                    }
                }
                Err(error) if error == backdrop::WARM_CANCELLED => {
                    // A superseded leader can hand this live request a
                    // cancellation; take over rather than toasting it. If the
                    // cancelled leader still cached its completed decode,
                    // upload that instead of decoding again.
                    if paints_backdrop(self.effective_material(), result.mode) {
                        if backdrop::needs_warm(result.mode, &result.path) {
                            self.start_backdrop_warm(result.mode, result.path.clone());
                        } else {
                            self.rebuild_backdrop_inner(false);
                        }
                    }
                }
                Err(error) => {
                    if paints_backdrop(self.effective_material(), result.mode) {
                        if result.mode == BackgroundMode::Image {
                            // The cached image was evicted by the failed
                            // revalidation; do not keep painting a deleted or
                            // corrupt wallpaper.
                            self.backdrop = None;
                        }
                        self.note_backdrop_warm_failure(result.mode, &result.path, error);
                    }
                }
            }
        }
    }

    /// Drop a backdrop texture the current effective material/mode cannot keep.
    ///
    /// Returns whether a texture was dropped. No decode or toast occurs.
    fn drop_unneeded_backdrop(&mut self) -> bool {
        let material = self.effective_material();
        let mode = self.config.background;
        if keeps_backdrop(material, mode) {
            return false;
        }
        // A warm for a texture that will never paint is speculative now; stop
        // it before it opens or decodes a wallpaper.
        self.cancel_backdrop_warm();
        if self.backdrop.is_none() {
            return false;
        }
        self.backdrop = None;
        self.applied_background = Some((mode, self.config.background_path.clone()));
        self.applied_background_opacity = self.config.background_opacity;
        self.ctx.request_repaint();
        log::debug!("backdrop dropped for {material:?}+{mode:?}: not needed");
        true
    }

    /// Keep the in-window backdrop in step with a material change.
    ///
    /// Entering Acrylic+Desktop keeps the existing texture (native blur paints
    /// instead) and never builds one. Leaving Acrylic rebuilds only when the
    /// new effective pair actually paints a backdrop, reusing the warmed cache.
    /// Leaving for Solid or None drops it without decoding.
    /// `background_changed` means `reapply_config` already rebuilt for the new
    /// background; rebuilding again here would upload the same wallpaper twice.
    fn sync_backdrop_for_material(&mut self, background_changed: bool) {
        let material = self.effective_material();
        let mode = self.config.background;
        let leaving_acrylic =
            self.applied_material == Material::Acrylic && material != Material::Acrylic;
        if self.drop_unneeded_backdrop() {
            return;
        }
        if !paints_backdrop(material, mode) {
            // Acrylic+Desktop keeps/warms the Desktop cache without ever
            // uploading it, so a later switch to Mica+Desktop only uploads.
            if keeps_backdrop(material, mode) {
                self.start_backdrop_warm(mode, self.config.background_path.clone());
            }
            return;
        }
        if background_changed {
            return;
        }
        if leaving_acrylic || self.backdrop.is_none() {
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
            design::install(&self.ctx, theme, self.effective_material());
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
                        self.overlays.set_toast(format!(
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
        let background_changed =
            background_dirty || self.applied_background.as_ref() != Some(&background);
        let opacity_changed =
            (self.config.background_opacity - self.applied_background_opacity).abs() > f32::EPSILON;
        if background_changed {
            self.rebuild_backdrop();
        } else if opacity_changed {
            self.applied_background_opacity = self.config.background_opacity;
            self.ctx.request_repaint();
        }

        if self.config.material != self.applied_requested_material {
            self.pending_material = true;
            // An explicit new request abandons any earlier session fallback and
            // its bounded retry streak; `apply_material_to` re-establishes the
            // failure state if the request still fails.
            self.material_fallback = false;
            self.material_retry.reset();
            // Acrylic+Desktop has no painted in-window backdrop, so switching
            // to Mica or Image must load one now. A warmed texture is uploaded,
            // and a simultaneous background change is not rebuilt twice.
            self.sync_backdrop_for_material(background_changed);
        }

        // The legacy acrylic accent carries its own tint, so opacity changes
        // must re-apply it. This is intentionally minimal: no design token
        // install and no backdrop rebuild, keeping a live slider drag cheap.
        if opacity_changed
            && self.effective_material() == Material::Acrylic
            && self.applied_material == Material::Acrylic
        {
            let window = Arc::clone(&self.window);
            if let Err(error) = platform::apply_acrylic_tint(
                window.as_ref(),
                self.theme.bg,
                !self.theme.is_light(),
                self.config.background_opacity,
            ) {
                // Do not leave the slider looking live when the tint cannot be
                // reapplied: fall back exactly like a failed material apply.
                // The request stays Acrylic and is retried on a healthy launch.
                let first = self.material_retry.failures == 0;
                self.material_fallback = true;
                self.pending_material = true;
                self.material_retry.note_failure(
                    Instant::now(),
                    self.native_transparency,
                    self.config.material,
                );
                if first {
                    log::warn!("failed to re-apply the acrylic tint: {error}");
                    self.overlays
                        .set_toast("Acrylic tint is unavailable here; using Solid");
                } else {
                    log::debug!(
                        "acrylic tint retry {} failed: {error}",
                        self.material_retry.failures
                    );
                }
                self.arm_material_retry();
                self.sync_backdrop_for_material(false);
            }
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

    /// Save the config. `config.material` is always the user's request now, so
    /// a session-only Solid fallback is never written.
    fn save_config(&mut self) -> anyhow::Result<()> {
        self.config.save()
    }

    fn persist(&mut self) {
        self.save_debounce.clear();
        // An empty name only exists while its text field is being edited.
        // Keep the last valid file rather than persisting an invalid profile.
        if self.config.has_empty_profile_name() {
            log::debug!("skipping config save while a profile name is empty");
            return;
        }
        if let Err(error) = self.save_config() {
            log::warn!("failed to save config: {error}");
            self.overlays
                .set_toast(format!("Could not save config: {error}"));
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

        if let Err(error) = self.save_config() {
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
                self.overlays.set_toast(error);
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
                self.overlays
                    .set_toast(format!("Clipboard unavailable: {error}"));
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
            self.overlays
                .set_toast(format!("Could not open link: {error}"));
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
            self.overlays
                .set_toast(format!("No matches for \"{query}\""));
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
                let chosen = next_material(self.config.material);
                choose_material(
                    &mut self.config.material,
                    &mut self.material_fallback,
                    chosen,
                );
                self.material_retry.reset();
                // The end-of-frame pending apply chooses the effective
                // material and then syncs the backdrop once, so a toggle never
                // uploads/drops a texture for the superseded material.
                self.pending_material = true;
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
        self.overlays.set_toast("Terminal bell");
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
        self.poll_backdrop_warm();
        self.maybe_retry_backdrop_warm();
        self.maybe_retry_material();

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
        let surface_material = self.effective_material();
        if paints_backdrop(surface_material, self.config.background)
            && let Some(backdrop) = &self.backdrop
        {
            let screen = ui.max_rect();
            ui.painter().image(
                backdrop.texture.id(),
                screen,
                Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                Color32::WHITE,
            );
            let tint = backdrop::tint_alpha(surface_material, self.config.background_opacity);
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
            surface_material,
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
        let mut settings_outcome = overlays::SettingsOutcome::default();

        if settings_active {
            let backdrop_present = self.backdrop.is_some();
            let tokens = &self.tokens;
            let theme = self.theme;
            let config = &mut self.config;
            let overlay_state = &mut self.overlays;
            let material_fallback = self.material_fallback;
            egui::CentralPanel::no_frame().show(ui, |ui| {
                settings_outcome = overlays::show_settings_panel(
                    ui,
                    tokens,
                    theme,
                    config,
                    overlay_state,
                    backdrop_present,
                    material_fallback,
                );
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

                let response = tab.view.show(
                    ui,
                    &tab.session,
                    theme,
                    config,
                    tokens,
                    atlas,
                    gpu_text,
                    surface_material,
                );
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

        if settings_outcome.changed {
            self.reapply_config();
        }
        if settings_outcome.retry_material {
            self.retry_material();
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
            // Apply may have succeeded after a fallback (or failed into one);
            // build/drop the in-window backdrop for the material that is now
            // actually rendered. A warmed entry only costs an upload.
            self.sync_backdrop_for_material(false);
        }
        self.drain_backdrop_warm();

        self.clamp_active();
        self.update_window_title(&ctx);
        self.sync_focus();
    }

    pub fn clear_color(&self) -> [f32; 4] {
        clear_color_for(
            self.effective_material(),
            self.native_transparency,
            self.theme.bg,
        )
    }
}

/// Swapchain clear colour for a material.
///
/// Transparent only when the surface really can present a transparent
/// backbuffer and the material is not Solid. Otherwise DWM paints Mica or the
/// acrylic accent over whatever the ignored clear alpha leaves behind, which
/// is opaque black; keeping the theme background opaque at least matches Solid.
fn clear_color_for(material: Material, native_transparency: bool, bg: Rgb) -> [f32; 4] {
    if native_transparency && material != Material::Solid {
        [0.0, 0.0, 0.0, 0.0]
    } else {
        design::color(bg).to_normalized_gamma_f32()
    }
}

/// The material actually rendered for a request.
///
/// Solid is the surface whenever the user requested it, native transparency is
/// unavailable, or this session already failed a native apply. The request in
/// `config.material` is left untouched so Settings can show it and a later
/// healthy launch can use it.
fn material_for_surface(
    requested: Material,
    native_transparency: bool,
    fallback: bool,
) -> Material {
    if requested == Material::Solid || fallback || !native_transparency {
        Material::Solid
    } else {
        requested
    }
}

/// Surface a warm failure even when no cursor blink or other repaint is due.
fn warm_error_toast(ctx: &egui::Context, overlays: &mut OverlayState, error: String) {
    overlays.set_toast(error);
    ctx.request_repaint();
}

/// Whether the app should keep an in-window backdrop texture for this
/// material/mode pair. Acrylic+Desktop keeps one for a later switch but never
/// paints it (native behind-window blur draws instead); Solid and None keep
/// none.
fn keeps_backdrop(material: Material, mode: BackgroundMode) -> bool {
    material != Material::Solid && mode != BackgroundMode::None
}

/// Whether this material/mode pair paints the in-window texture. This is the
/// paint guard from [`IllnessApp::ui`]: native Acrylic+Desktop would hide the
/// desktop blur behind a painted wallpaper.
fn paints_backdrop(material: Material, mode: BackgroundMode) -> bool {
    keeps_backdrop(material, mode)
        && !(material == Material::Acrylic && mode == BackgroundMode::Desktop)
}

/// What a backdrop rebuild should do for this material/mode pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackdropPlan {
    /// No texture is kept: Solid or `BackgroundMode::None`.
    Drop,
    /// Keep the current texture but decode nothing: Acrylic+Desktop.
    Keep,
    /// Decode (or reuse the path+mtime cache) and paint the texture.
    Build,
}

/// Acrylic+Desktop is the only pair that keeps without painting: it warms the
/// cache in the background but never uploads. All other non-painting pairs
/// (Solid/fallback and None) drop the texture.
fn backdrop_plan(material: Material, mode: BackgroundMode) -> BackdropPlan {
    if paints_backdrop(material, mode) {
        BackdropPlan::Build
    } else if keeps_backdrop(material, mode) {
        BackdropPlan::Keep
    } else {
        BackdropPlan::Drop
    }
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

/// Record an explicit material choice. The request is always the chosen
/// material (including Solid); an earlier session fallback is abandoned so a
/// native choice retries immediately.
fn choose_material(requested: &mut Material, fallback: &mut bool, chosen: Material) {
    *requested = chosen;
    *fallback = false;
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
    fn backdrop_is_kept_only_for_non_solid_materials_with_a_background() {
        assert!(!keeps_backdrop(Material::Solid, BackgroundMode::Desktop));
        assert!(!keeps_backdrop(Material::Solid, BackgroundMode::Image));
        assert!(!keeps_backdrop(Material::Solid, BackgroundMode::None));
        for material in [Material::Mica, Material::Acrylic] {
            assert!(!keeps_backdrop(material, BackgroundMode::None));
            assert!(keeps_backdrop(material, BackgroundMode::Desktop));
            assert!(keeps_backdrop(material, BackgroundMode::Image));
        }
    }

    #[test]
    fn acrylic_desktop_keeps_a_backdrop_but_never_paints_it() {
        assert!(keeps_backdrop(Material::Acrylic, BackgroundMode::Desktop));
        assert!(!paints_backdrop(Material::Acrylic, BackgroundMode::Desktop));
        assert!(paints_backdrop(Material::Acrylic, BackgroundMode::Image));
        assert!(!paints_backdrop(Material::Acrylic, BackgroundMode::None));
        assert!(paints_backdrop(Material::Mica, BackgroundMode::Desktop));
        assert!(paints_backdrop(Material::Mica, BackgroundMode::Image));
        assert!(!paints_backdrop(Material::Solid, BackgroundMode::Image));
    }

    #[test]
    fn entering_acrylic_desktop_at_runtime_never_decodes() {
        // Solid/None drop the texture and start no warm; Acrylic+Desktop keeps
        // and cache-warms for a later switch. They never build a texture on
        // the UI thread.
        assert_eq!(
            backdrop_plan(Material::Acrylic, BackgroundMode::Desktop),
            BackdropPlan::Keep
        );
        // Leaving Acrylic for a painted pair rebuilds from the warmed cache.
        assert_eq!(
            backdrop_plan(Material::Mica, BackgroundMode::Desktop),
            BackdropPlan::Build
        );
        assert_eq!(
            backdrop_plan(Material::Mica, BackgroundMode::Image),
            BackdropPlan::Build
        );
        // Solid and None drop without decoding.
        assert_eq!(
            backdrop_plan(Material::Solid, BackgroundMode::Desktop),
            BackdropPlan::Drop
        );
        assert_eq!(
            backdrop_plan(Material::Solid, BackgroundMode::Image),
            BackdropPlan::Drop
        );
        assert_eq!(
            backdrop_plan(Material::Acrylic, BackgroundMode::None),
            BackdropPlan::Drop
        );
    }

    #[test]
    fn effective_solid_never_plans_a_backdrop_build() {
        // A requested Mica image with native transparency off must not decode:
        // the effective material is Solid, whose plan is Drop even at startup.
        let effective = material_for_surface(Material::Mica, false, false);
        assert_eq!(effective, Material::Solid);
        assert_eq!(
            backdrop_plan(effective, BackgroundMode::Image),
            BackdropPlan::Drop
        );
        // Acrylic+Desktop is native blur only; it keeps/warms but never builds.
        let effective = material_for_surface(Material::Acrylic, true, false);
        assert_eq!(
            backdrop_plan(effective, BackgroundMode::Desktop),
            BackdropPlan::Keep
        );
        // The same request with a healthy surface paints Mica.
        let effective = material_for_surface(Material::Mica, true, false);
        assert_eq!(
            backdrop_plan(effective, BackgroundMode::Desktop),
            BackdropPlan::Build
        );
    }

    #[test]
    fn explicit_solid_after_fallback_persists_solid() {
        let mut requested = Material::Mica;
        let mut fallback = true;
        choose_material(&mut requested, &mut fallback, Material::Solid);

        // config.material is the only persisted material, so an explicit Solid
        // is saved rather than the stale Mica request.
        let config = Config {
            material: requested,
            ..Config::default()
        };
        assert_eq!(config.material, Material::Solid);
        assert!(!fallback);
        assert_eq!(
            material_for_surface(config.material, true, fallback),
            Material::Solid
        );
    }

    #[test]
    fn fallback_retries_the_requested_material_without_rewriting_it() {
        let mut requested = Material::Mica;
        let mut fallback = true;
        choose_material(&mut requested, &mut fallback, Material::Mica);

        assert_eq!(requested, Material::Mica);
        assert!(!fallback, "an explicit retry abandons the old fallback");
        // Still Solid while transparency effects are off...
        assert_eq!(
            material_for_surface(requested, false, fallback),
            Material::Solid
        );
        // ...and Mica when the live retry succeeds.
        assert_eq!(
            material_for_surface(requested, true, fallback),
            Material::Mica
        );
    }

    #[test]
    fn live_transparency_toggle_changes_the_effective_material() {
        // Turning effects off falls back immediately.
        assert_eq!(
            material_for_surface(Material::Acrylic, false, false),
            Material::Solid
        );
        // Turning them on retries the same request without a restart.
        assert_eq!(
            material_for_surface(Material::Acrylic, true, false),
            Material::Acrylic
        );
        // A recorded apply failure still forces Solid until an explicit retry.
        assert_eq!(
            material_for_surface(Material::Mica, true, true),
            Material::Solid
        );
    }

    #[test]
    fn mica_with_no_background_keeps_the_native_transparent_path() {
        // With DWM/transparency available, Mica+None must stay Mica: no Solid
        // panel fill, no opaque clear color, and no painted backdrop.
        let requested = Material::Mica;
        let effective = material_for_surface(requested, true, false);
        assert_eq!(effective, Material::Mica);
        assert_eq!(
            backdrop_plan(effective, BackgroundMode::None),
            BackdropPlan::Drop
        );
        assert!(!paints_backdrop(effective, BackgroundMode::None));
        assert_eq!(
            clear_color_for(effective, true, Rgb::new(0x12, 0x34, 0x56)),
            [0.0; 4]
        );
        // Only a genuinely unavailable surface falls back to Solid.
        assert_eq!(
            material_for_surface(requested, false, false),
            Material::Solid
        );
    }

    #[test]
    fn clear_color_is_transparent_only_for_verified_native_transparency() {
        let bg = Rgb::new(0x12, 0x34, 0x56);
        let opaque = design::color(bg).to_normalized_gamma_f32();
        for material in [Material::Solid, Material::Mica, Material::Acrylic] {
            assert_eq!(
                clear_color_for(material, false, bg),
                opaque,
                "{material:?} must stay opaque without surface alpha support"
            );
        }
        assert_eq!(
            clear_color_for(Material::Solid, true, bg),
            opaque,
            "Solid never needs a transparent backbuffer"
        );
        assert_eq!(clear_color_for(Material::Mica, true, bg), [0.0; 4]);
        assert_eq!(clear_color_for(Material::Acrylic, true, bg), [0.0; 4]);
    }

    #[test]
    fn native_materials_require_verified_surface_transparency() {
        for material in [Material::Mica, Material::Acrylic] {
            assert_eq!(
                material_for_surface(material, false, false),
                Material::Solid,
                "{material:?} is unavailable without surface alpha/effects"
            );
            assert_eq!(material_for_surface(material, true, false), material);
            assert_eq!(
                material_for_surface(material, true, true),
                Material::Solid,
                "{material:?} stays Solid after an apply failure"
            );
        }
        assert_eq!(
            material_for_surface(Material::Solid, false, false),
            Material::Solid
        );
        assert_eq!(
            material_for_surface(Material::Solid, true, false),
            Material::Solid
        );
        assert_eq!(
            material_for_surface(Material::Solid, true, true),
            Material::Solid
        );
    }

    #[test]
    fn fallback_material_retry_waits_for_backoff_and_availability() {
        let now = Instant::now();
        let mut retry = MaterialRetryState::default();
        retry.note_failure(now, true, Material::Mica);
        assert_eq!(retry.failures, 1);
        assert!(retry.first_failure(), "only the first failure toasts");
        assert_eq!(retry.retry_at, now.checked_add(Duration::from_secs(2)));
        assert!(
            !retry.retry_due(true, true, Material::Mica, now + Duration::from_secs(1)),
            "a fresh failure must not be retried on every focus event"
        );
        assert!(retry.retry_due(true, true, Material::Mica, now + Duration::from_secs(2)));
        retry.clear_deadline();
        assert!(
            retry.retry_due(true, true, Material::Mica, now),
            "a fallback without an armed deadline retries immediately"
        );
        assert!(!retry.retry_due(false, true, Material::Mica, now));
        assert!(
            !retry.retry_due(true, false, Material::Mica, now),
            "transparency effects off is not an apply-failure retry"
        );
        assert!(
            !retry.retry_due(true, true, Material::Solid, now),
            "an explicit Solid request never retries"
        );
    }

    #[test]
    fn material_retry_backoff_caps_after_consecutive_failures() {
        let now = Instant::now();
        let mut retry = MaterialRetryState::default();

        retry.note_failure(now, true, Material::Mica);
        assert_eq!(retry.retry_at, now.checked_add(Duration::from_secs(2)));
        retry.note_failure(now, true, Material::Mica);
        assert_eq!(retry.retry_at, now.checked_add(Duration::from_secs(4)));
        retry.note_failure(now, true, Material::Mica);

        assert!(
            retry.exhausted(),
            "the third consecutive failure must reach the cap"
        );
        assert_eq!(retry.retry_at, None, "the cap arms no further deadline");
        assert!(
            !retry.retry_due(true, true, Material::Mica, now + Duration::from_secs(3600)),
            "a permanent apply failure must stop retrying forever"
        );
        assert!(!retry.first_failure(), "retries never re-toast");
    }

    #[test]
    fn explicit_solid_resets_the_material_retry_streak() {
        let now = Instant::now();
        let mut retry = MaterialRetryState::default();
        for _ in 0..MATERIAL_RETRY_CAP {
            retry.note_failure(now, true, Material::Mica);
        }
        assert!(retry.exhausted());

        // The user explicitly chooses Solid: reset and never arm another retry.
        retry.reset();
        assert!(!retry.exhausted());
        assert_eq!(retry.retry_at, None);
        retry.note_failure(now, true, Material::Solid);
        assert_eq!(retry.retry_at, None);
        assert!(
            !retry.retry_due(true, true, Material::Solid, now),
            "explicit Solid never schedules a native retry"
        );

        // An explicit native selection clears the old failed streak too.
        retry.reset();
        retry.note_failure(now, true, Material::Mica);
        assert!(
            retry.first_failure(),
            "a new streak toasts its first failure"
        );
        assert_eq!(retry.failures, 1);
    }

    #[test]
    fn retry_deadlines_use_host_time_not_last_frame_input() {
        let armed_at = Instant::now();
        let mut retry = MaterialRetryState::default();
        retry.note_failure(armed_at, true, Material::Acrylic);

        // Simulate an idle pause before a focus/theme event: the deadline is an
        // absolute Instant, so the retry still waits the full backoff.
        assert!(!retry.retry_due(
            true,
            true,
            Material::Acrylic,
            armed_at + Duration::from_millis(1)
        ));
        assert!(retry.retry_due(
            true,
            true,
            Material::Acrylic,
            armed_at + Duration::from_secs(2)
        ));
    }

    #[test]
    fn stuck_warm_can_be_superseded_after_the_timeout() {
        let now = Instant::now();
        let fresh = BackdropWarm {
            request: 1,
            mode: BackgroundMode::Image,
            path: "slow.png".to_string(),
            started: now - Duration::from_secs(1),
        };
        assert!(!fresh.is_stuck(now));

        let stuck = BackdropWarm {
            request: 2,
            mode: BackgroundMode::Image,
            path: "unc-slow.png".to_string(),
            started: now - WARM_STUCK_AFTER - Duration::from_secs(1),
        };
        assert!(
            stuck.is_stuck(now),
            "a dead UNC warm must not block its pair forever"
        );
    }

    #[test]
    fn warm_error_toast_requests_a_repaint_and_restarts_its_deadline() {
        let ctx = egui::Context::default();
        let mut overlays = OverlayState {
            toast_deadline: Some(0.0),
            ..OverlayState::default()
        };

        warm_error_toast(&ctx, &mut overlays, "Could not decode image".to_string());

        assert_eq!(
            overlays.toast.as_deref(),
            Some("Could not decode image"),
            "the warm failure must remain visible"
        );
        assert_eq!(
            overlays.toast_deadline, None,
            "a replaced toast must get a fresh deadline so it survives the frame"
        );
        assert!(
            ctx.has_requested_repaint(),
            "setting the toast must schedule the frame that draws it"
        );
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
