use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use egui::{
    self, Color32, Event, Id, Key, Modifiers, Rect, Sense, Stroke, StrokeKind, Vec2,
    ViewportCommand, pos2, vec2,
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
/// After the short retry cap, a dead source is still probed slowly so an
/// outage that clears later can recover without an explicit background change.
const WARM_RECOVERY_INTERVAL: Duration = Duration::from_secs(60);
/// Consecutive slow recovery probes allowed after the short cap. Repeated
/// failures eventually stop automatic probing.
const WARM_RECOVERY_CAP: u32 = 5;
/// Low-frequency revalidation of a painted Image/Desktop backdrop while the
/// window is visible. Focus regain also revalidates, but not more often than
/// this floor so alt-tab spam cannot exhaust the warm retry cap.
const BACKDROP_REVALIDATE_INTERVAL: Duration = Duration::from_secs(45);
const FOCUS_REVALIDATE_FLOOR: Duration = Duration::from_secs(8);
/// A shell that fails during this startup window is kept with a recovery card
/// instead of closing the window. After the window an unattended normal exit is
/// treated as expected; a deliberate `exit` after input closes immediately.
const STARTUP_RECOVERY_WINDOW: Duration = Duration::from_secs(5);
/// One deadline for all stored/startup/live folder probes before spawning. A
/// dead UNC share can otherwise block the UI for the SMB timeout.
const SPAWN_PATH_PROBE_TIMEOUT: Duration = Duration::from_millis(250);
/// A shell start that has not produced a session by this deadline is abandoned
/// and kept as a recovery tab. The worker may be stuck inside CreateProcess on
/// a cold UNC folder; the UI must still offer Restart/Close.
const SPAWN_START_DEADLINE: Duration = Duration::from_secs(10);
/// One cwd attempt gets at most this long before the worker tries the next
/// candidate. A stuck CreateProcess thread cannot block the tab forever.
const SPAWN_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(8);
/// Show the lightweight "Starting…" card only when a normal start is slower
/// than this, so fast shells do not flash a modal overlay.
const SPAWN_STARTING_CARD_DELAY: Duration = Duration::from_millis(750);
/// How often a spawn attempt checks its cancellation token while waiting.
const SPAWN_CANCEL_POLL: Duration = Duration::from_millis(25);
/// Cap keys buffered while a shell is still starting, keeping the newest.
const MAX_PENDING_INPUT_EVENTS: usize = 512;

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

/// Count a failed, superseded or timed-out warm and compute the next automatic
/// retry. Short exponential retries stop at [`WARM_RETRY_CAP`]; after that a
/// slow recovery probe keeps running so a temporary outage (for example a UNC
/// share that returns) recovers without a manual background change. Repeated
/// slow failures stop after [`WARM_RECOVERY_CAP`] probes.
fn next_warm_retry(failures: u32, now: Instant) -> (u32, Option<Instant>) {
    let failures = failures.saturating_add(1);
    let retry_at = if failures < WARM_RETRY_CAP {
        Some(now + WARM_RETRY_BASE * 2u32.pow(failures.saturating_sub(1)))
    } else if failures < WARM_RETRY_CAP + WARM_RECOVERY_CAP {
        Some(now + WARM_RECOVERY_INTERVAL)
    } else {
        None
    };
    (failures, retry_at)
}

/// Whether a new warm must wait for its backoff or the failure cap. Revalidation
/// never resets the streak; an explicit rebuild does, before calling this.
/// Above the cap only a recovery probe whose slow deadline is due may run.
fn warm_start_blocked(
    failures: u32,
    retry_at: Option<Instant>,
    revalidate: bool,
    now: Instant,
) -> bool {
    if failures >= WARM_RETRY_CAP {
        return !(revalidate && retry_at.is_some_and(|at| now >= at));
    }
    !revalidate && retry_at.is_some_and(|at| now < at)
}

/// Focus regain is debounced so a rapid alt-tab sequence cannot burn through the
/// warm failure cap with back-to-back revalidations.
fn focus_revalidation_due(last: Option<Instant>, now: Instant) -> bool {
    last.is_none_or(|at| now.saturating_duration_since(at) >= FOCUS_REVALIDATE_FLOOR)
}

/// Whether a warm result or path-only lookup must upload a texture over the one
/// already painted. A changed painted key wins even when `changed` is false: a
/// cancelled leader can cache a newer decode after the timeout rebuild already
/// painted the old entry.
fn backdrop_upload_needed(
    painted: Option<&backdrop::CacheKey>,
    current: Option<&backdrop::CacheKey>,
    changed: bool,
) -> bool {
    changed || current.is_some_and(|current| painted != Some(current))
}

/// Whether a counted warm failure should surface a toast. Painted failures
/// toast once; a failure for a pair that paints nothing still counts but stays
/// silent because the user cannot see its image.
fn warm_failure_toasts(paints: bool, toast_allowed: bool, failures: u32) -> bool {
    paints && toast_allowed && failures == 1
}

/// A visible window revalidates a painted or kept Image/Desktop source when
/// the low-frequency deadline is due (or has never run).
fn revalidation_due(next: Option<Instant>, now: Instant, visible: bool, eligible: bool) -> bool {
    visible && eligible && next.is_none_or(|at| now >= at)
}

/// Whether a material change needs a backdrop rebuild. The previous material
/// must be captured before the platform apply overwrites `applied_material`;
/// otherwise leaving Acrylic is invisible to the sync.
fn material_change_needs_rebuild(
    previous: Material,
    current: Material,
    backdrop_present: bool,
) -> bool {
    (previous == Material::Acrylic && current != Material::Acrylic) || !backdrop_present
}

/// DECSET 1004 focus reports.
const FOCUS_IN: &[u8] = b"\x1b[I";
const FOCUS_OUT: &[u8] = b"\x1b[O";

/// One terminal tab: the PTY session plus everything the UI remembers.
///
/// `session` is optional while the shell is starting on a worker thread and
/// after a spawn failure. A failed spawn stays as a real tab so Restart,
/// Settings and Close remain available instead of leaving an empty window.
struct Tab {
    /// Stable across reordering, closing other tabs, and Settings focus.
    id: u64,
    session: Option<Session>,
    view: TerminalView,
    title: String,
    profile_name: String,
    /// Program captured at spawn time, used to avoid trusting a Windows-side
    /// cwd for WSL shells.
    program: String,
    /// Working directory used at spawn and last read from the live shell.
    cwd: Option<PathBuf>,
    accent: Rgb,
    /// Whether the PTY currently believes this tab has focus (DECSET 1004).
    focus_reported: bool,
    /// Spawn time and whether the user has interacted with this tab. A shell
    /// that exits before use is a failed profile, not a normal exit.
    spawned_at: Instant,
    user_acted: bool,
    /// Set when an immediate/spontaneous exit or a spawn failure is kept for
    /// recovery instead of being removed by [`IllnessApp::remove_exited_tabs`].
    recovery: bool,
    /// Child exit status, when ConPTY reported one.
    exit_status: Option<i32>,
    /// Spawn error shown by the recovery card, when there is one.
    error: Option<String>,
    /// Cancellation token for a detached spawn that has not attached yet.
    spawn_cancel: Option<Arc<AtomicBool>>,
    /// Keys/text typed while this tab was still waiting for its session. They
    /// are encoded and written as soon as the session attaches, so a deliberate
    /// quick exit still counts as user interaction.
    pending_input: Vec<Event>,
}

impl Tab {
    /// Placeholder tab shown while its shell starts on the spawn worker.
    fn pending(id: u64, profile: &Profile, cwd: Option<PathBuf>) -> Self {
        Self {
            id,
            session: None,
            view: TerminalView::default(),
            title: profile.name.clone(),
            profile_name: profile.name.clone(),
            program: profile.program.clone(),
            cwd,
            accent: profile.accent,
            focus_reported: false,
            spawned_at: Instant::now(),
            user_acted: false,
            recovery: false,
            exit_status: None,
            error: None,
            spawn_cancel: None,
            pending_input: Vec::new(),
        }
    }
}

/// A slot in the tab strip. Settings is a real tab but has no PTY session.
enum TabEntry {
    Terminal(Box<Tab>),
    Settings,
}

/// Choice offered by the failed-shell recovery card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecoveryAction {
    Restart,
    Settings,
    Close,
}

/// A session created on the spawn worker, with the cwd that actually worked.
struct SpawnedSession {
    session: Session,
    cwd: Option<PathBuf>,
}

/// Result of one tab's detached spawn attempts.
struct SpawnResult {
    tab_id: u64,
    outcome: Result<SpawnedSession, String>,
    /// Source tab and live path that a successful spawn inherited, so its
    /// stored cwd can be refreshed after the off-thread probe validates it.
    inherited: Option<(u64, PathBuf)>,
}

/// Outcome of waiting for one detached `Session::spawn` attempt.
#[derive(Debug, PartialEq, Eq)]
enum SpawnWait<T> {
    Ready(T),
    Timeout,
    Cancelled,
    Disconnected,
}

/// Wait for a spawn attempt while honouring a tab's cancellation token.
///
/// Polling keeps a closed tab from waiting out an SMB/CreateProcess hang; the
/// abandoned attempt thread drops the session (and its child) when its send
/// eventually fails.
fn wait_for_spawn<T>(
    receiver: &Receiver<T>,
    cancel: &AtomicBool,
    timeout: Duration,
) -> SpawnWait<T> {
    let deadline = Instant::now().checked_add(timeout);
    loop {
        if cancel.load(Ordering::Acquire) {
            return SpawnWait::Cancelled;
        }
        let now = Instant::now();
        let left = deadline
            .map(|deadline| deadline.saturating_duration_since(now))
            .unwrap_or(timeout);
        if left.is_zero() {
            return SpawnWait::Timeout;
        }
        match receiver.recv_timeout(left.min(SPAWN_CANCEL_POLL)) {
            Ok(value) => return SpawnWait::Ready(value),
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return SpawnWait::Disconnected,
        }
    }
}

/// Run one `Session::spawn` on a detached thread and give up after
/// [`SPAWN_ATTEMPT_TIMEOUT`].
///
/// The inner thread may still be blocked inside ConPTY/CreateProcess on a cold
/// UNC folder when the deadline fires. Its result channel is dropped, so when
/// the attempt eventually returns the session is dropped immediately and its
/// child is shut down. The outer spawn worker can then try the next cwd or
/// report the failure without ever leaving an unresponsive blank tab.
fn spawn_with_deadline(
    spec: SpawnSpec,
    cell_px: (u16, u16),
    waker: Arc<dyn Fn() + Send + Sync>,
    cancel: &AtomicBool,
    tab_id: u64,
) -> Result<Session, String> {
    let (sender, receiver) = mpsc::channel();
    // Capture the attempt instant before `CreateProcess`: the ConPTY host is
    // created during the spawn, so a host created at or after this belongs to
    // this tab (see `platform::boost_console_priority`).
    let spawn_at = std::time::SystemTime::now();
    let spawned = std::thread::Builder::new()
        .name(format!("illness-shell-attempt-{tab_id}"))
        .spawn(move || {
            let _ = sender.send(Session::spawn(
                spec,
                GridSize::new(INITIAL_COLS, INITIAL_ROWS),
                cell_px,
                waker,
            ));
        });
    if let Err(error) = spawned {
        return Err(format!("could not create a spawn attempt thread: {error}"));
    }
    match wait_for_spawn(&receiver, cancel, SPAWN_ATTEMPT_TIMEOUT) {
        SpawnWait::Ready(Ok(session)) => {
            // A tab closed between the result arriving and this line is already
            // cancelled; boosting its dead shell would only touch a reused PID.
            platform::boost_console_priority(
                session.child_pid(),
                spawn_at,
                cancel.load(Ordering::Acquire),
            );
            Ok(session)
        }
        SpawnWait::Ready(Err(error)) => Err(error.to_string()),
        SpawnWait::Timeout => Err(format!(
            "the shell did not start within {} seconds (the folder may be on an offline network share)",
            SPAWN_ATTEMPT_TIMEOUT.as_secs()
        )),
        SpawnWait::Cancelled => Err("the tab was closed while its shell was starting".to_string()),
        SpawnWait::Disconnected => Err("the spawn attempt stopped without a result".to_string()),
    }
}

/// Apply a detached spawn result to its placeholder tab.
///
/// Failure is represented in the tab itself (no session, recovery card) so the
/// window never becomes empty and Restart can retry from the same tab.
fn apply_spawn_outcome(tab: &mut Tab, outcome: Result<SpawnedSession, String>) {
    match outcome {
        Ok(spawned) => {
            tab.session = Some(spawned.session);
            tab.cwd = spawned.cwd;
            // Interaction with the placeholder is not shell use. Reset both
            // fields so a shell that dies immediately still gets the card.
            tab.spawned_at = Instant::now();
            tab.user_acted = false;
            tab.recovery = false;
            tab.exit_status = None;
            tab.error = None;
        }
        Err(error) => {
            log::warn!("{} failed to start: {error}", tab.profile_name);
            tab.session = None;
            tab.recovery = true;
            tab.error = Some(error);
            tab.pending_input.clear();
        }
    }
}

/// Find the terminal tab a detached result belongs to. Missing ids are closed
/// tabs; their late session is dropped instead of attaching to a new tab.
fn terminal_tab_mut(tabs: &mut [TabEntry], tab_id: u64) -> Option<&mut Tab> {
    tabs.iter_mut().find_map(|entry| match entry {
        TabEntry::Terminal(tab) if tab.id == tab_id => Some(tab.as_mut()),
        _ => None,
    })
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
    /// Folder supplied on the command line. It seeds the first tab and is the
    /// fallback when no live terminal cwd can be read.
    startup_folder: Option<PathBuf>,
    /// Stable id of the most recently active terminal tab. New tabs requested
    /// while Settings is active still inherit that terminal's live cwd.
    last_terminal_id: Option<u64>,
    /// Monotonic id source for terminal tabs.
    next_terminal_id: u64,
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
    /// Detached shell starts. Spawning can block on a dead UNC cwd, so it must
    /// not run on the UI thread.
    spawn_tx: Sender<SpawnResult>,
    spawn_rx: Receiver<SpawnResult>,
    /// Consecutive backdrop warm failures and the next automatic retry. The
    /// retry stops after [`WARM_RETRY_CAP`] so a deleted/dead source cannot
    /// loop forever.
    backdrop_warm_failures: u32,
    backdrop_warm_retry_at: Option<Instant>,
    /// When the current painted Image/Desktop source was last revalidated
    /// off-thread. `None` revalidates on the next visible pass.
    backdrop_revalidate_at: Option<Instant>,
    /// When a revalidation last actually ran. Focus regain is debounced against
    /// this so rapid alt-tabs cannot exhaust the warm retry cap.
    backdrop_revalidated_at: Option<Instant>,
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
    /// Set when the user confirmed closing through the confirmation card.
    ///
    /// Invariant: this is only true for the close that follows the
    /// confirmation. It is cleared by [`Self::dismiss_close_confirmation`] on
    /// cancel or dismissal, and by [`Self::reapply_config`] when
    /// `confirm_close_with_tabs` is turned off. A sent
    /// `ViewportCommand::Close` normally beats a `CancelClose`, so leaving it
    /// set looked harmless, but any future veto (a host-side cancel, a modal,
    /// a shutdown failure) would then close the window without asking again.
    close_confirmed: bool,
    /// Shared with the host: whether a window close would currently show the
    /// confirmation card. The host reads it when a close arrives for a window
    /// that cannot paint (minimized/hidden) and restores the window instead of
    /// exiting. Refreshed from the setting plus the terminal-tab count every
    /// frame.
    close_guarded: Arc<AtomicBool>,
}

impl IllnessApp {
    pub fn new(
        ctx: &egui::Context,
        render_state: Option<&egui_wgpu::RenderState>,
        window: Arc<Window>,
        native_transparency: bool,
        startup_folder: Option<PathBuf>,
        close_guarded: Arc<AtomicBool>,
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
            match fonts::load_terminal_font_faces(&config.font_family) {
                Some(font) => {
                    atlas.set_font_faces(font.regular, font.bold, font.italic, font.bold_italic);
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
        let (spawn_tx, spawn_rx) = mpsc::channel();
        let mut app = Self {
            ctx,
            window: Arc::clone(&window),
            config,
            profiles,
            startup_folder,
            last_terminal_id: None,
            next_terminal_id: 1,
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
            spawn_tx,
            spawn_rx,
            backdrop_warm_failures: 0,
            backdrop_warm_retry_at: None,
            backdrop_revalidate_at: None,
            backdrop_revalidated_at: None,
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
            close_confirmed: false,
            close_guarded,
        };
        app.applied_font_size = app.config.font_size;
        app.applied_ppp = app.ctx.pixels_per_point();
        app.apply_material_to(window.as_ref());
        app.rebuild_backdrop();
        // Before the first PTY exists: force the kernel32 `CreatePseudoConsole`
        // fallback instead of a `PATH` `conpty.dll` host.
        platform::pin_inbox_conpty();
        app.open_tab(None);
        app
    }

    /// Spawn a tab for `profile` (or the configured default) and make it active.
    ///
    /// The tab is inserted immediately and its session starts on a worker:
    /// the cwd probe and `Session::spawn` can both block on a dead UNC cwd and
    /// must never stall the UI. If every attempt fails or exceeds the start
    /// deadline, the tab is kept with a cancel/restart card.
    pub fn open_tab(&mut self, profile: Option<&str>) {
        let profile = self.profile_for(profile);
        let (candidates, inherited) = self.spawn_candidates_for_new_tab();
        log::debug!(
            "opening {} tab with cwd candidates {:?}",
            profile.name,
            candidates
                .iter()
                .map(|candidate| candidate.path.as_path())
                .collect::<Vec<_>>()
        );
        let tab = self.start_tab(&profile, candidates, inherited);
        let id = tab.id;
        let index = self.settings_index().unwrap_or(self.tabs.len());
        self.tabs.insert(index, TabEntry::Terminal(Box::new(tab)));
        self.active = index;
        self.last_terminal_id = Some(id);
    }

    /// Collect new-tab cwd candidates at the moment it is requested.
    ///
    /// Only a cheap PEB read happens here; probing and `Session::spawn` run on
    /// the spawn worker. A live path that was validated there refreshes the
    /// source tab's stored cwd after a successful attach. WSL, elevated shells
    /// and dead processes cannot be read and use stored/startup paths.
    fn spawn_candidates_for_new_tab(&mut self) -> (Vec<SpawnCandidate>, Option<(u64, PathBuf)>) {
        let process = platform::fallback_startup_dir();
        if !self.config.inherit_cwd {
            return (
                spawn_candidates(None, None, self.startup_folder.clone(), process),
                None,
            );
        }

        let terminal_id = match self.tabs.get(self.active) {
            Some(TabEntry::Terminal(tab)) => Some(tab.id),
            _ => self.last_terminal_id,
        };
        let (live, stored) = terminal_id
            .and_then(|id| self.tab_index_by_id(id))
            .and_then(|index| match self.tabs.get(index) {
                Some(TabEntry::Terminal(tab)) => Some((tab_live_cwd(tab), tab.cwd.clone())),
                _ => None,
            })
            .unwrap_or((None, None));
        let inherited = terminal_id.zip(live.clone());
        (
            spawn_candidates(live, stored, self.startup_folder.clone(), process),
            inherited,
        )
    }

    fn tab_index_by_id(&self, id: u64) -> Option<usize> {
        self.tabs
            .iter()
            .position(|entry| matches!(entry, TabEntry::Terminal(tab) if tab.id == id))
    }

    /// Remember the active terminal for tabs opened while Settings is shown.
    fn note_active_terminal(&mut self) {
        if let Some(TabEntry::Terminal(tab)) = self.tabs.get(self.active) {
            self.last_terminal_id = Some(tab.id);
            return;
        }
        if self
            .last_terminal_id
            .is_some_and(|id| self.tab_index_by_id(id).is_none())
        {
            self.last_terminal_id = None;
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

    /// Insert a placeholder tab and start planning/spawning on a worker.
    fn start_tab(
        &mut self,
        profile: &Profile,
        candidates: Vec<SpawnCandidate>,
        inherited: Option<(u64, PathBuf)>,
    ) -> Tab {
        let id = self.next_terminal_id;
        self.next_terminal_id = self.next_terminal_id.saturating_add(1);
        let cwd = candidates
            .iter()
            .find(|candidate| !candidate.process || candidate.explicit)
            .map(|candidate| candidate.path.clone());
        let cancel = Arc::new(AtomicBool::new(false));
        let mut tab = Tab::pending(id, profile, cwd);
        tab.spawn_cancel = Some(Arc::clone(&cancel));
        self.launch_spawn(
            id,
            profile.clone(),
            self.config.clone(),
            candidates,
            inherited,
            cancel,
        );
        tab
    }

    /// Probe cwd candidates, build specs, and try each one on a worker thread.
    ///
    /// Nothing here runs on the UI thread. Each `Session::spawn` attempt has its
    /// own deadline and checks the tab's cancellation token, so a closed tab
    /// does not wait out an SMB timeout or leave a child behind.
    fn launch_spawn(
        &self,
        tab_id: u64,
        profile: Profile,
        config: Config,
        candidates: Vec<SpawnCandidate>,
        inherited: Option<(u64, PathBuf)>,
        cancel: Arc<AtomicBool>,
    ) {
        let metrics = self.atlas.metrics();
        let cell_px = (cell_pixel(metrics.width_px), cell_pixel(metrics.height_px));
        let ctx = self.ctx.clone();
        let waker: Arc<dyn Fn() + Send + Sync> = Arc::new({
            let ctx = ctx.clone();
            move || ctx.request_repaint()
        });
        let done = Arc::clone(&waker);
        let sender = self.spawn_tx.clone();
        let profile_name = profile.name.clone();
        let thread_error =
            format!("Could not start {profile_name}: could not create the spawn worker");
        let worker_error = thread_error.clone();
        let reported_inherited = inherited.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("illness-shell-spawn-{tab_id}"))
            .spawn(move || {
                let dirs = plan_spawn_cwds(&candidates, Some(&cancel));
                if cancel.load(Ordering::Acquire) {
                    return;
                }
                let specs = spawn_specs(&profile, &config, &dirs);
                let mut last_error = worker_error;
                for spec in specs {
                    if cancel.load(Ordering::Acquire) {
                        return;
                    }
                    let cwd = spec.cwd.clone();
                    match spawn_with_deadline(spec, cell_px, Arc::clone(&waker), &cancel, tab_id) {
                        Ok(session) => {
                            if cancel.load(Ordering::Acquire) {
                                session.shutdown();
                                return;
                            }
                            let _ = sender.send(SpawnResult {
                                tab_id,
                                outcome: Ok(SpawnedSession { session, cwd }),
                                inherited: inherited.clone(),
                            });
                            done();
                            return;
                        }
                        Err(error) => {
                            last_error = format!("Could not start {profile_name}: {error}");
                        }
                    }
                }
                if cancel.load(Ordering::Acquire) {
                    return;
                }
                let _ = sender.send(SpawnResult {
                    tab_id,
                    outcome: Err(last_error),
                    inherited: inherited.clone(),
                });
                done();
            });
        if spawned.is_err() {
            let _ = self.spawn_tx.send(SpawnResult {
                tab_id,
                outcome: Err(thread_error),
                inherited: reported_inherited,
            });
            self.ctx.request_repaint();
        }
    }

    /// Drain completed detached spawns and fill or mark their tabs.
    ///
    /// This runs before this frame's input is read, so a result that arrived
    /// while the tab was pending attaches its session and any buffered keys
    /// first. Results for tabs closed meanwhile are discarded (the session is
    /// dropped, shutting its child down).
    fn apply_spawn_results(&mut self) {
        let modifiers = self.ctx.input(|input| input.modifiers);
        let mut cwd_updates = Vec::new();
        while let Ok(result) = self.spawn_rx.try_recv() {
            let Some(tab) = terminal_tab_mut(&mut self.tabs, result.tab_id) else {
                continue;
            };
            let inherited = result.inherited.clone();
            tab.spawn_cancel = None;
            apply_spawn_outcome(tab, result.outcome);
            let Some(session) = tab.session.as_ref() else {
                continue;
            };
            if let Some((source, live)) = &inherited
                && tab.cwd.as_ref() == Some(live)
            {
                cwd_updates.push((*source, live.clone()));
            }
            let pending = std::mem::take(&mut tab.pending_input);
            if pending.is_empty() || session.is_exited() {
                continue;
            }
            if let Some(bytes) = tab
                .view
                .encode_input(&pending, modifiers, session.term_modes())
            {
                session.write(&bytes);
                tab.user_acted = true;
            }
        }
        for (source, live) in cwd_updates {
            if let Some(index) = self.tab_index_by_id(source)
                && let Some(TabEntry::Terminal(tab)) = self.tabs.get_mut(index)
            {
                tab.cwd = Some(live);
            }
        }
    }

    /// Give a tab that never receives a session a visible, cancellable state.
    fn expire_stalled_spawns(&mut self) {
        let now = Instant::now();
        for entry in &mut self.tabs {
            let TabEntry::Terminal(tab) = entry else {
                continue;
            };
            if tab.session.is_some() || tab.recovery {
                continue;
            }
            if now.saturating_duration_since(tab.spawned_at) < SPAWN_START_DEADLINE {
                continue;
            }
            tab.recovery = true;
            tab.pending_input.clear();
            if let Some(cancel) = tab.spawn_cancel.take() {
                cancel.store(true, Ordering::Release);
            }
            tab.error = Some(format!(
                "{} did not start within {} seconds. The launch folder may be on an offline network share.",
                tab.profile_name,
                SPAWN_START_DEADLINE.as_secs()
            ));
            log::warn!(
                "{} did not start within {:?}; offering recovery actions",
                tab.profile_name,
                SPAWN_START_DEADLINE
            );
        }
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

    /// The material this pass should paint. While a material apply is pending,
    /// the platform still renders the previously applied one; painting that
    /// keeps the panel fill, clear colour and native surface in step until the
    /// deferred apply runs at the start of the next pass.
    fn surface_material(&self) -> Material {
        if self.pending_material {
            self.applied_material
        } else {
            self.effective_material()
        }
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
        let previous = self.applied_material;
        let window = Arc::clone(&self.window);
        self.apply_material_to(window.as_ref());
        // A retry may need the painted backdrop that the Solid fallback
        // dropped; falling back drops it again without decoding.
        self.sync_backdrop_for_material(previous);
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
                    self.ctx.request_repaint();
                } else {
                    self.ctx.request_repaint_after(at - now);
                }
            }
            // A fallback without an armed deadline retries immediately.
            None => {
                self.pending_material = true;
                self.ctx.request_repaint();
            }
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
        // Apply and sync before the next paint so the retried material, panel
        // fill, clear colour and backdrop agree.
        self.ctx.request_repaint();
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
            self.backdrop_revalidated_at = Some(Instant::now());
            // A warm will resolve this source now; do not also revalidate on
            // the first visible pass.
            if keeps_backdrop(material, mode) {
                self.backdrop_revalidate_at = Some(Instant::now() + BACKDROP_REVALIDATE_INTERVAL);
                self.ctx.request_repaint_after(BACKDROP_REVALIDATE_INTERVAL);
            }
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
        if mode == BackgroundMode::Desktop && backdrop::all_candidates_failed(desktop) {
            // A failed candidate set is cooling down. Revalidation must not
            // stat the same dead UNC path again before the cooldown expires.
            return;
        }
        let now = Instant::now();
        if warm_start_blocked(
            self.backdrop_warm_failures,
            self.backdrop_warm_retry_at,
            revalidate,
            now,
        ) {
            log::debug!(
                "backdrop warm {mode:?} deferred after {} failure(s)",
                self.backdrop_warm_failures
            );
            return;
        }
        if let Some(warming) = &self.backdrop_warming
            && warming.mode == mode
            && warming.path == path
        {
            let elapsed = now.saturating_duration_since(warming.started);
            if !warming.is_stuck(now) {
                self.ctx.request_repaint_after(WARM_STUCK_AFTER - elapsed);
                return;
            }
            log::warn!("backdrop warm {mode:?} has been running for {elapsed:?}; superseding it");
            self.cancel_backdrop_warm();
            // A warm that outlived the timeout is a failure even when it is
            // replaced instead of reporting an error; count it so a dead
            // source cannot bypass the retry cap.
            self.note_backdrop_warm_superseded(mode, &path);
            if warm_start_blocked(
                self.backdrop_warm_failures,
                self.backdrop_warm_retry_at,
                revalidate,
                Instant::now(),
            ) {
                return;
            }
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
    /// path cannot keep a pair marked as warming forever. Completed work is
    /// drained first: a warm that finished while minimized/occluded must
    /// upload instead of being cancelled and dropped.
    fn poll_backdrop_warm(&mut self) {
        self.drain_backdrop_warm();
        let Some(warming) = self.backdrop_warming.as_ref() else {
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
        self.note_backdrop_warm_superseded(mode, &path);
        // A cancelled leader may have cached its completed decode just before
        // observing the token. Rebuild path-only so that result uploads; a
        // cache miss paints the fallback and waits for the armed retry.
        if paints_backdrop(self.effective_material(), mode) {
            self.rebuild_backdrop_inner(false);
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
        let mode = self.config.background;
        if !keeps_backdrop(self.effective_material(), mode) {
            self.backdrop_warm_retry_at = None;
            return;
        }
        let path = self.config.background_path.clone();
        let desktop = if mode == BackgroundMode::Desktop {
            backdrop::desktop_candidates()
        } else {
            Vec::new()
        };
        // Upload a newer cache entry even when the painted texture is present.
        // A cancelled leader can fill the cache after the post-timeout rebuild
        // already painted the old entry; `needs_warm` is false for that state.
        let current = backdrop::current_key(self.theme, mode, &path, &desktop);
        if backdrop_upload_needed(
            self.backdrop.as_ref().map(|backdrop| &backdrop.key),
            current.as_ref(),
            false,
        ) {
            self.rebuild_backdrop_inner(false);
            // The retry deadline was consumed: the rebuild either warmed the
            // pair or painted everything the cache already held.
            self.backdrop_warm_retry_at = None;
            return;
        }
        if !backdrop::needs_warm_with(mode, &path, &desktop) {
            self.backdrop_warm_retry_at = None;
            return;
        }
        let recovery = self.backdrop_warm_failures >= WARM_RETRY_CAP;
        self.start_backdrop_warm_with(mode, path, recovery, &desktop);
        if self.backdrop_warming.is_some() {
            self.backdrop_warm_retry_at = None;
        }
    }

    /// Revalidate a visible Image/Desktop source off-thread on a low-frequency
    /// timer. The UI path stays path-only: metadata and decoding happen on the
    /// warm thread, and the app uploads only when the warm reports a change.
    fn maybe_revalidate_backdrop(&mut self) {
        let material = self.surface_material();
        let mode = self.config.background;
        let recovery_available =
            self.backdrop_warm_failures < WARM_RETRY_CAP || self.backdrop_warm_retry_at.is_some();
        let eligible = keeps_backdrop(material, mode)
            && recovery_available
            && match mode {
                BackgroundMode::None => false,
                BackgroundMode::Image => !self.config.background_path.trim().is_empty(),
                BackgroundMode::Desktop => true,
            };
        let visible = self.window.is_visible().unwrap_or(true)
            && !self.window.is_minimized().unwrap_or(false)
            && !self
                .ctx
                .input(|input| input.viewport().occluded)
                .unwrap_or(false);
        let now = Instant::now();
        if !revalidation_due(self.backdrop_revalidate_at, now, visible, eligible) {
            if visible
                && eligible
                && let Some(at) = self.backdrop_revalidate_at
            {
                self.ctx
                    .request_repaint_after(at.saturating_duration_since(now));
            }
            return;
        }
        self.revalidate_backdrop_now(now);
    }

    /// Run one revalidation now (timer or focus regain). Shares the warm
    /// failure cap: a dead source clears the deadline instead of waking again.
    fn revalidate_backdrop_now(&mut self, now: Instant) {
        if !self.window.is_visible().unwrap_or(true) || self.window.is_minimized().unwrap_or(false)
        {
            return;
        }
        let material = self.surface_material();
        let mode = self.config.background;
        let path = self.config.background_path.clone();
        if !keeps_backdrop(material, mode) {
            self.backdrop_revalidate_at = None;
            return;
        }
        if mode == BackgroundMode::Image && path.trim().is_empty() {
            self.backdrop_revalidate_at = None;
            return;
        }
        let desktop = if mode == BackgroundMode::Desktop {
            backdrop::desktop_candidates()
        } else {
            Vec::new()
        };
        if mode == BackgroundMode::Desktop && desktop.is_empty() {
            // No wallpaper source to stat, but a wallpaper can appear while the
            // app is running. Keep the low-frequency deadline (without arming a
            // warm) so this is checked again instead of looping every frame.
            self.backdrop_revalidate_at = Some(now + BACKDROP_REVALIDATE_INTERVAL);
            self.ctx.request_repaint_after(BACKDROP_REVALIDATE_INTERVAL);
            return;
        }
        if self.backdrop_warm_failures >= WARM_RETRY_CAP && self.backdrop_warm_retry_at.is_none() {
            // The short cap and every slow recovery probe failed. Only an
            // explicit background/material change probes again.
            self.backdrop_revalidate_at = None;
            return;
        }
        if mode == BackgroundMode::Desktop && backdrop::all_candidates_failed(&desktop) {
            // Honor the Desktop candidate cooldown: do not stat a dead UNC path
            // on every revalidation while it is still cooling down.
            self.backdrop_revalidate_at = Some(now + BACKDROP_REVALIDATE_INTERVAL);
            self.ctx.request_repaint_after(BACKDROP_REVALIDATE_INTERVAL);
            return;
        }
        self.backdrop_revalidated_at = Some(now);
        self.backdrop_revalidate_at = Some(now + BACKDROP_REVALIDATE_INTERVAL);
        self.ctx.request_repaint_after(BACKDROP_REVALIDATE_INTERVAL);
        self.start_backdrop_warm_with(mode, path, true, &desktop);
    }

    /// Upload a newer cache entry for the current pair if the painted texture is
    /// stale. Path-only: it never stats or decodes, so it is safe even when the
    /// warm that filled the cache was already superseded.
    fn upload_cached_backdrop_if_changed(&mut self, mode: BackgroundMode, path: &str) -> bool {
        if mode != self.config.background
            || path != self.config.background_path
            || !paints_backdrop(self.effective_material(), mode)
        {
            return false;
        }
        let desktop = if mode == BackgroundMode::Desktop {
            backdrop::desktop_candidates()
        } else {
            Vec::new()
        };
        let current = backdrop::current_key(self.theme, mode, path, &desktop);
        if !backdrop_upload_needed(
            self.backdrop.as_ref().map(|backdrop| &backdrop.key),
            current.as_ref(),
            false,
        ) {
            return false;
        }
        self.rebuild_backdrop_inner(false);
        true
    }

    fn note_backdrop_warm_failure(&mut self, mode: BackgroundMode, path: &str, error: String) {
        self.record_backdrop_warm_failure(mode, path, error, true);
    }

    /// Count a warm failure without a toast (a Desktop fallback gradient is a
    /// valid paint, but the all-candidates-failed attempt still consumes the
    /// retry budget).
    fn note_backdrop_warm_failure_silent(
        &mut self,
        mode: BackgroundMode,
        path: &str,
        error: String,
    ) {
        self.record_backdrop_warm_failure(mode, path, error, false);
    }

    fn record_backdrop_warm_failure(
        &mut self,
        mode: BackgroundMode,
        path: &str,
        error: String,
        toast_allowed: bool,
    ) {
        // Count the failure before the paint gate. An Acrylic+Desktop prewarm
        // paints nothing (native blur does), but its failures and timeouts must
        // still reach the shared cap/backoff instead of retrying forever.
        let paints = paints_backdrop(self.effective_material(), mode);
        let (failures, retry_at) = next_warm_retry(self.backdrop_warm_failures, Instant::now());
        self.backdrop_warm_failures = failures;
        self.backdrop_warm_retry_at = retry_at;
        if warm_failure_toasts(paints, toast_allowed, self.backdrop_warm_failures) {
            log::warn!("backdrop warm {mode:?} for \"{path}\" failed: {error}");
            warm_error_toast(&self.ctx, &mut self.overlays, error);
        } else if !paints {
            log::debug!("backdrop warm {mode:?} failed while unpainted: {error}");
        } else {
            log::debug!(
                "backdrop warm {mode:?} retry {} failed: {error}",
                self.backdrop_warm_failures
            );
        }
        if let Some(at) = self.backdrop_warm_retry_at {
            self.ctx
                .request_repaint_after(at.saturating_duration_since(Instant::now()));
        }
    }

    /// Count a warm that was replaced after [`WARM_STUCK_AFTER`] instead of
    /// reporting an error. It shares the failure streak and backoff with real
    /// failures so a blocked UNC source cannot respawn every 30 seconds.
    fn note_backdrop_warm_superseded(&mut self, mode: BackgroundMode, path: &str) {
        let (failures, retry_at) = next_warm_retry(self.backdrop_warm_failures, Instant::now());
        self.backdrop_warm_failures = failures;
        self.backdrop_warm_retry_at = retry_at;
        log::warn!(
            "backdrop warm {mode:?} for \"{path}\" timed out (failure {}/{WARM_RETRY_CAP})",
            self.backdrop_warm_failures
        );
        if let Some(at) = self.backdrop_warm_retry_at {
            self.ctx
                .request_repaint_after(at.saturating_duration_since(Instant::now()));
        }
    }

    /// Apply finished detached warm results. A matching result uploads the
    /// warmed texture. A superseded result is normally stale, but its completed
    /// decode may have replaced the cache; uploading that newer key is still
    /// path-only and fixes a texture painted before the late decode landed.
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
            let matches_config =
                result.mode == self.config.background && result.path == self.config.background_path;
            if !current {
                // A superseded leader can finish after the post-timeout rebuild
                // painted the older cache entry. Upload its completed decode
                // now instead of waiting for the next 45 s revalidation.
                if matches_config {
                    self.upload_cached_backdrop_if_changed(result.mode, &result.path);
                }
                continue;
            }
            if !matches_config {
                continue;
            }
            match result.result {
                Ok(outcome) => {
                    if outcome.failed {
                        // A Desktop candidate set that all failed still returns
                        // the stable gradient; count it toward the cap without
                        // toasting a fallback the user can already see.
                        self.note_backdrop_warm_failure_silent(
                            result.mode,
                            &result.path,
                            "Desktop wallpaper unavailable".to_string(),
                        );
                    } else {
                        self.backdrop_warm_failures = 0;
                        self.backdrop_warm_retry_at = None;
                    }
                    // The warm just resolved the current source; arm the next
                    // low-frequency revalidation instead of validating twice.
                    let now = Instant::now();
                    self.backdrop_revalidated_at = Some(now);
                    self.backdrop_revalidate_at = Some(now + BACKDROP_REVALIDATE_INTERVAL);
                    self.ctx.request_repaint_after(BACKDROP_REVALIDATE_INTERVAL);
                    // A cold build painted no texture while another warm filled
                    // the cache, so upload even when the decode was unchanged.
                    // A painted key that differs from the result is a late
                    // decode that replaced the cache after the timeout rebuild.
                    let upload = backdrop_upload_needed(
                        self.backdrop.as_ref().map(|backdrop| &backdrop.key),
                        outcome.key.as_ref(),
                        outcome.changed,
                    );
                    if paints_backdrop(self.effective_material(), result.mode) && upload {
                        self.rebuild_backdrop_inner(false);
                    }
                }
                Err(error) if error == backdrop::WARM_CANCELLED => {
                    // A superseded leader can hand this live request a
                    // cancellation; take over rather than toasting it. If the
                    // cancelled leader still cached its completed decode,
                    // upload that instead of decoding again.
                    if keeps_backdrop(self.effective_material(), result.mode) {
                        let desktop = if result.mode == BackgroundMode::Desktop {
                            backdrop::desktop_candidates()
                        } else {
                            Vec::new()
                        };
                        let current =
                            backdrop::current_key(self.theme, result.mode, &result.path, &desktop);
                        let upload = backdrop_upload_needed(
                            self.backdrop.as_ref().map(|backdrop| &backdrop.key),
                            current.as_ref(),
                            false,
                        );
                        if upload {
                            self.rebuild_backdrop_inner(false);
                        } else if backdrop::needs_warm(result.mode, &result.path) {
                            self.start_backdrop_warm(result.mode, result.path.clone());
                        } else {
                            self.rebuild_backdrop_inner(false);
                        }
                    }
                }
                Err(error) => {
                    if paints_backdrop(self.effective_material(), result.mode)
                        && result.mode == BackgroundMode::Image
                    {
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
    /// Leaving for Solid or None drops it without decoding. `previous_material`
    /// is the material that was applied before the platform call; it must be
    /// captured by the caller because `applied_material` already holds the new
    /// one by the time this runs.
    fn sync_backdrop_for_material(&mut self, previous_material: Material) {
        let material = self.effective_material();
        let mode = self.config.background;
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
        if material_change_needs_rebuild(previous_material, material, self.backdrop.is_some()) {
            self.rebuild_backdrop();
        }
    }

    fn reapply_config(&mut self) {
        self.config = self.config.clone().sanitized();
        if !self.config.confirm_close_with_tabs {
            // The guard is off, so a stale confirmation must not survive a
            // later re-enable and close the window without asking.
            self.close_confirmed = false;
        }
        if self.config.profiles != self.profiles {
            self.profiles = self.config.profiles.clone();
        }

        let theme = Theme::by_name(&self.config.theme);
        let mut background_dirty = false;
        if theme.name != self.applied_theme.as_str() {
            design::install(&self.ctx, theme, self.surface_material());
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
            match fonts::load_terminal_font_faces(&self.config.font_family) {
                Some(font) => {
                    self.atlas.set_font_faces(
                        font.regular,
                        font.bold,
                        font.italic,
                        font.bold_italic,
                    );
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
            // Apply and sync at the start of the next pass, before any shape is
            // built, so the panel fill, clear colour and native material change
            // in the same frame instead of temporarily disagreeing.
            self.ctx.request_repaint();
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
                // The fallback apply is deferred to the next pass, before
                // painting, so this frame keeps a coherent Acrylic surface.
                self.ctx.request_repaint();
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

    /// Hold back an OS close request when the setting asks for confirmation.
    ///
    /// One terminal tab, a disabled setting, or an already-confirmed close
    /// falls through to the host, which saves and exits as before. Otherwise
    /// the request is cancelled and the confirmation card is shown.
    fn handle_close_request(&mut self, ctx: &egui::Context) {
        if !ctx.input(|input| input.viewport().close_requested()) {
            return;
        }

        // A second press while the card is up must not stack another prompt.
        // It still re-checks the count: a background shell may have exited, so
        // the card's number and the veto must not disagree.
        if matches!(self.overlays.kind, OverlayKind::ConfirmClose) {
            if self.would_confirm_close() {
                ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            } else {
                // Only one terminal is left, so the guard no longer applies:
                // dismiss the stale card and let this close proceed unguarded.
                self.dismiss_close_confirmation();
            }
            return;
        }

        if !self.would_confirm_close() {
            return;
        }

        self.open_close_confirmation();
        ctx.send_viewport_cmd(ViewportCommand::CancelClose);
        ctx.request_repaint();
    }

    /// Every terminal tab, whether or not its shell attached.
    ///
    /// The Settings tab owns no session, so counting it would turn one shell
    /// into a "2 tabs are open" prompt. A pending or failed spawn *is* counted:
    /// the card covers the tabs the user can see, and such a tab is still one
    /// of them.
    fn terminal_tab_count(&self) -> usize {
        terminal_tab_count(&self.tabs)
    }

    /// Whether a close request would currently show the confirmation card.
    fn would_confirm_close(&self) -> bool {
        should_confirm_close(
            self.terminal_tab_count(),
            self.config.confirm_close_with_tabs,
            self.close_confirmed,
        )
    }

    /// Dismiss the close card without closing, forgetting the confirmation.
    ///
    /// Clearing `close_confirmed` keeps the invariant that a confirmation
    /// covers only the close request it answered.
    fn dismiss_close_confirmation(&mut self) {
        self.close_confirmed = false;
        self.overlays.close();
    }

    /// Replace whatever overlay is open with the close confirmation.
    ///
    /// The previous overlay is dismissed rather than restored: a palette
    /// query or search field typed before the prompt is not kept hidden behind
    /// a modal, and cancelling therefore leaves no stale overlay state.
    fn open_close_confirmation(&mut self) {
        self.overlays.close();
        self.overlays.kind = OverlayKind::ConfirmClose;
        self.ctx.request_repaint();
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
            if let Some(cancel) = &tab.spawn_cancel {
                cancel.store(true, Ordering::Release);
            }
            if let Some(session) = &tab.session {
                session.shutdown();
            }
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
        self.remove_tab(index);
    }

    /// Remove one tab and keep the strip usable.
    ///
    /// Manual close and shell exit share this path. Removing the active tab
    /// selects the nearest remaining terminal or Settings tab; removing the
    /// last tab asks the host to close the window.
    fn remove_tab(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        if let Some(TabEntry::Terminal(tab)) = self.tabs.get_mut(index)
            && let Some(cancel) = tab.spawn_cancel.take()
        {
            cancel.store(true, Ordering::Release);
        }
        self.tabs.remove(index);
        if removal_closes_window(self.tabs.len()) {
            self.active = 0;
            self.note_active_terminal();
            // The host saves and exits after this pass; shutting down here
            // would break a later CancelClose.
            self.ctx.send_viewport_cmd(ViewportCommand::Close);
            return;
        }

        self.active = next_active_after_close(self.active, index, self.tabs.len());
        self.note_active_terminal();
    }

    /// Drop every terminal whose shell has exited, including several that
    /// exited between two frames.
    ///
    /// A profile that fails immediately is the exception: removing it used to
    /// leave no tab at all, so the window closed before Settings could be
    /// reached. Keep that tab, mark it for recovery, and let the card offer
    /// Restart/Settings/Close. A shell that exits after use is removed normally.
    fn remove_exited_tabs(&mut self) {
        let now = Instant::now();
        let mut exited = Vec::new();
        for (index, entry) in self.tabs.iter_mut().enumerate() {
            let TabEntry::Terminal(tab) = entry else {
                continue;
            };
            let Some(session) = &tab.session else {
                // A pending or failed spawn has no process to remove.
                continue;
            };
            if !session.is_exited() || tab.recovery {
                continue;
            }
            let elapsed = now.saturating_duration_since(tab.spawned_at);
            if needs_recovery(elapsed, tab.user_acted, tab.exit_status) {
                tab.recovery = true;
                log::warn!(
                    "keeping exited {} tab for recovery (spawned {:?} ago, user acted: {}, exit: {:?})",
                    tab.profile_name,
                    elapsed,
                    tab.user_acted,
                    tab.exit_status
                );
            } else {
                exited.push(index);
            }
        }
        for index in exited.into_iter().rev() {
            log::debug!("removing exited tab {index}");
            self.remove_tab(index);
        }
    }

    /// Restart a failed tab in place, preserving its profile and last cwd.
    ///
    /// The replacement is inserted immediately as a pending tab; a second
    /// failed spawn keeps the recovery card instead of leaving a toast-only
    /// dead end.
    fn restart_tab(&mut self, index: usize) {
        let Some(TabEntry::Terminal(tab)) = self.tabs.get(index) else {
            return;
        };
        let profile_name = tab.profile_name.clone();
        let cwd = tab.cwd.clone();
        let profile = self.profile_for(Some(&profile_name));
        if let Some(TabEntry::Terminal(old)) = self.tabs.get_mut(index)
            && let Some(cancel) = old.spawn_cancel.take()
        {
            cancel.store(true, Ordering::Release);
        }
        let candidates = spawn_candidates(
            None,
            cwd,
            self.startup_folder.clone(),
            platform::fallback_startup_dir(),
        );
        let tab = self.start_tab(&profile, candidates, None);
        let id = tab.id;
        self.tabs[index] = TabEntry::Terminal(Box::new(tab));
        self.active = index;
        self.last_terminal_id = Some(id);
    }

    fn move_tab(&mut self, from: usize, to: usize) {
        let len = self.tabs.len();
        if from >= len || to >= len || from == to {
            return;
        }
        self.active = active_after_move(self.active, from, to);
        let tab = self.tabs.remove(from);
        self.tabs.insert(to, tab);
        self.note_active_terminal();
    }

    fn copy_selection(&mut self) {
        let Some(text) = self.selection_text() else {
            return;
        };
        self.ctx.copy_text(text);
    }

    fn selection_text(&self) -> Option<String> {
        let text = self
            .active_terminal()?
            .session
            .as_ref()?
            .selection_to_string()?;
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

        if let Some(tab) = self.active_terminal_mut()
            && let Some(session) = &tab.session
            && !session.is_exited()
        {
            let bracketed = session.snapshot().modes.bracketed_paste;
            session.write(&format_paste(&text, bracketed));
            // Clipboard paste is real input to this shell, unlike a titlebar
            // click or a Settings control.
            tab.user_acted = true;
        }
    }

    fn open_link(&mut self, url: &str) {
        if let Err(error) = open::that(url) {
            self.overlays
                .set_toast(format!("Could not open link: {error}"));
        }
    }

    fn select_all(&mut self) {
        if let Some(tab) = self.active_terminal()
            && let Some(session) = &tab.session
        {
            let snapshot = session.snapshot();
            if snapshot.cols > 0 && snapshot.rows > 0 {
                session.selection_start(SelectionType::Simple, 0, 0, Side::Left);
                session.selection_update(snapshot.cols - 1, snapshot.rows - 1, Side::Right);
            }
        }
    }

    fn clear_view(&mut self) {
        if let Some(tab) = self.active_terminal_mut()
            && let Some(session) = &tab.session
        {
            session.selection_clear();
            let offset = i32::try_from(session.snapshot().display_offset).unwrap_or(i32::MAX);
            if offset > 0 {
                session.scroll(-offset);
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
        let Some(session) = tab.session.as_ref() else {
            self.overlays.search_index = Some((0, 0));
            return;
        };
        let snapshot = session.snapshot();
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

        let found = session.search(&query, request.forward);
        if found.is_none() {
            self.overlays
                .set_toast(format!("No matches for \"{query}\""));
        }
        if let Some((start_row, _, _, _)) = found
            && let Some(delta) = search_scroll_delta(start_row, snapshot.rows)
        {
            session.scroll(delta);
            let after = session.snapshot();
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
        let Some(session) = tab.session.as_ref() else {
            self.overlays.search_index = Some((0, 0));
            return;
        };
        let count = count_visible_matches(&session.snapshot(), &query, case_sensitive);
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
            Action::NewTab(profile) => {
                log::debug!("new-tab action profile={profile:?}");
                self.open_tab(profile.as_deref());
            }
            Action::CloseTab(index) => {
                log::debug!("close-tab action index={index}");
                self.close_tab(index);
            }
            Action::ActivateTab(index) => {
                if index < self.tabs.len() {
                    self.active = index;
                    self.note_active_terminal();
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
                log::debug!("close-window action");
                // The titlebar button must ask the same question as the OS
                // close frame instead of bypassing it.
                if self.would_confirm_close() {
                    self.open_close_confirmation();
                } else {
                    // Let the host run the pass, collect the Close command and
                    // save/exit afterwards so a CancelClose can still win.
                    self.ctx.send_viewport_cmd(ViewportCommand::Close);
                }
            }
            Action::ConfirmCloseWindow => {
                log::debug!("close-window confirmed");
                self.close_confirmed = true;
                self.overlays.close();
                self.ctx.send_viewport_cmd(ViewportCommand::Close);
            }
            Action::CancelCloseWindow => {
                log::debug!("close-window cancelled");
                self.dismiss_close_confirmation();
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
                // The next pass applies the material and then syncs the
                // backdrop once, before painting, so a toggle never uploads or
                // drops a texture for the superseded material.
                self.pending_material = true;
                self.ctx.request_repaint();
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
            let Some(session) = tab.session.as_mut() else {
                continue;
            };
            while let Ok(event) = session.events().try_recv() {
                match event {
                    SessionEvent::Title(title) => {
                        tab.title = display_title(&title, &tab.profile_name);
                    }
                    SessionEvent::ResetTitle => tab.title = tab.profile_name.clone(),
                    SessionEvent::Bell => bell = true,
                    SessionEvent::ClipboardStore(text) => ctx.copy_text(text),
                    SessionEvent::PtyWrite(bytes) => session.write(&bytes),
                    SessionEvent::ColorRequest(index) => {
                        session.write(color_response(index, theme).as_bytes());
                    }
                    SessionEvent::TextAreaSizeRequest => {
                        let snapshot = session.snapshot();
                        let reply = format!("\x1b[8;{};{}t", snapshot.rows, snapshot.cols);
                        session.write(reply.as_bytes());
                    }
                    // `is_exited` is set before the event is sent; the tab is
                    // handled in `sync_tabs`, so only the status is recorded.
                    SessionEvent::ChildExit(status) => tab.exit_status = Some(status),
                    SessionEvent::Exit => {}
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
            if let Some(title) = tab.session.as_ref().and_then(Session::title) {
                tab.title = display_title(&title, &tab.profile_name);
            }
        }
        self.remove_exited_tabs();
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
            let Some(session) = tab.session.as_ref() else {
                tab.focus_reported = false;
                continue;
            };
            let desired = window_focused && Some(index) == active;
            if !session.focus_reporting() {
                tab.focus_reported = false;
                continue;
            }
            if tab.focus_reported != desired {
                session.write(if desired { FOCUS_IN } else { FOCUS_OUT });
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
        self.note_active_terminal();
    }

    fn tab_infos(&self) -> Vec<TabInfo> {
        self.tabs
            .iter()
            .map(|entry| match entry {
                TabEntry::Terminal(tab) => TabInfo {
                    id: tab.id,
                    title: tab.title.clone(),
                    profile: tab.profile_name.clone(),
                    accent: tab.accent,
                },
                TabEntry::Settings => TabInfo {
                    // Settings has no terminal id. Keep one stable sentinel so
                    // it can still be dragged and identity-checked.
                    id: u64::MAX,
                    title: "Settings".to_string(),
                    profile: "Settings".to_string(),
                    accent: self.theme.accent,
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
                    self.note_active_terminal();
                }
            }
            Shortcut::Palette => {
                self.chrome.dropdown_open = false;
                self.overlays.kind = OverlayKind::Palette;
            }
            Shortcut::Search => {
                self.chrome.dropdown_open = false;
                self.overlays.kind = OverlayKind::Search;
            }
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
        self.note_active_terminal();
    }
}

impl IllnessApp {
    pub fn ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        // Attach completed spawns before reading this frame's input: buffered
        // pre-attach keys are written first, and chrome/overlay activity is
        // never counted as shell use.
        self.apply_spawn_results();
        self.expire_stalled_spawns();
        // Apply a requested material change before building any shape. The
        // platform call, design tokens, backdrop drop/upload and clear colour
        // then describe one material for the whole pass.
        if self.pending_material {
            self.pending_material = false;
            let previous = self.applied_material;
            let window = Arc::clone(&self.window);
            self.apply_material_to(window.as_ref());
            self.sync_backdrop_for_material(previous);
        }
        self.poll_backdrop_warm();
        self.maybe_retry_backdrop_warm();
        self.maybe_revalidate_backdrop();
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
        let surface_material = self.surface_material();
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
        let focused = ctx
            .input(|input| input.viewport().focused)
            .unwrap_or(self.window_focused);
        if focused
            && !self.window_focused
            && focus_revalidation_due(self.backdrop_revalidated_at, Instant::now())
        {
            // Re-check a replaced image or a Windows slideshow wallpaper as
            // soon as the user returns; the stat/decode stays off-thread. The
            // floor debounces alt-tab spam.
            self.revalidate_backdrop_now(Instant::now());
        }
        self.window_focused = focused;
        self.update_window_title(&ctx);

        let tab_infos = self.tab_infos();
        // Publish the guard decision for the host before handling this frame's
        // close request. The host reads it when the window is minimized or
        // hidden and therefore cannot present the card itself.
        self.close_guarded
            .store(self.would_confirm_close(), Ordering::Relaxed);
        // Runs before the overlay pass so the card paints this same frame and
        // `overlay_open` already sees it.
        self.handle_close_request(&ctx);
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

        // Terminal input invariant: keyboard/text/IME/paste bytes are written
        // to the PTY here, before this frame builds a snapshot or composites
        // anything, so a keystroke never waits on paint work. Because the grid
        // panel below reads the same frame's events, they must not be encoded
        // a second time per frame; `TerminalView::show` must not encode input.
        // The gate is `terminal_input_allowed`, which ignores egui focus: the
        // grid requests focus when clicked, so focus is not "someone else owns
        // the keyboard" here.
        if let Some(TabEntry::Terminal(tab)) = self.tabs.get_mut(self.active)
            && let Some(session) = &tab.session
            && terminal_input_allowed(
                overlay_open,
                shortcut_used,
                session.is_exited(),
                keyboard_input,
            )
        {
            // Idle frames (no key/text/IME/paste event) must not clone the
            // event vector or take the terminal lock just to learn that.
            let events = ctx.input(|input| {
                input
                    .events
                    .iter()
                    .any(is_terminal_input_event)
                    .then(|| input.events.clone())
            });
            if let Some(events) = events {
                let modifiers = ctx.input(|input| input.modifiers);
                // Read modes from the live session so this frame's DECCKM /
                // bracketed-paste transition already applies to this frame's keys.
                let modes = session.term_modes();
                if let Some(bytes) = tab.view.encode_input(&events, modifiers, modes) {
                    session.write(&bytes);
                    // Only bytes actually delivered to this shell count as use.
                    tab.user_acted = true;
                }
            }
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
        let mut recovery_action = None;
        let mut starting_action = None;
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
                    // The host closes the window when the last tab is
                    // removed; paint only the material surface for this pass.
                    ui.painter().rect_filled(
                        ui.available_rect_before_wrap(),
                        0.0,
                        placeholder_fill(tokens, surface_material),
                    );
                    return;
                };

                if let Some(session) = &tab.session {
                    let response = tab.view.show(
                        ui,
                        session,
                        theme,
                        config,
                        tokens,
                        atlas,
                        gpu_text,
                        surface_material,
                    );
                    terminal_rect = Some(response.response.rect);
                    let input_sent = response.input_sent && !session.is_exited();
                    terminal_actions = response.actions;
                    if input_sent {
                        tab.user_acted = true;
                    }
                    if response.response.secondary_clicked()
                        && (!tab.view.mouse_reporting() || ui.input(|input| input.modifiers.shift))
                    {
                        context_menu_pos = response.response.interact_pointer_pos();
                    }
                } else {
                    // Pending spawns and failed spawns have no grid yet. Keep a
                    // material-aware surface (translucent under Mica/Acrylic)
                    // instead of an opaque block over the native backdrop.
                    let rect = ui.available_rect_before_wrap();
                    ui.painter()
                        .rect_filled(rect, 0.0, placeholder_fill(tokens, surface_material));
                    terminal_rect = Some(rect);
                    if !tab.recovery {
                        let elapsed = Instant::now().saturating_duration_since(tab.spawned_at);
                        if elapsed >= SPAWN_STARTING_CARD_DELAY {
                            starting_action = show_starting_card(
                                ui.ctx(),
                                tokens,
                                tab.id,
                                &tab.profile_name,
                                elapsed,
                            )
                            .map(|action| (tab.id, action));
                        }
                        if !overlay_open && !shortcut_used {
                            buffer_pending_input(ui, &mut tab.pending_input);
                        }
                        let wait_until = if elapsed < SPAWN_STARTING_CARD_DELAY {
                            SPAWN_STARTING_CARD_DELAY - elapsed
                        } else {
                            SPAWN_START_DEADLINE.saturating_sub(elapsed)
                        };
                        if !wait_until.is_zero() {
                            ui.ctx().request_repaint_after(wait_until);
                        }
                    }
                }
                if tab.recovery {
                    recovery_action = show_recovery_card(
                        ui.ctx(),
                        tokens,
                        tab.id,
                        &tab.profile_name,
                        &tab.title,
                        tab.error.as_deref(),
                        tab.exit_status,
                    )
                    .map(|action| (tab.id, action));
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

        if let Some((id, action)) = starting_action.or(recovery_action) {
            let index = self
                .tab_index_by_id(id)
                .or_else(|| self.active_terminal_index());
            match (action, index) {
                (RecoveryAction::Restart, Some(index)) => self.restart_tab(index),
                (RecoveryAction::Settings, _) => self.open_settings(),
                (RecoveryAction::Close, Some(index)) => self.close_tab(index),
                (RecoveryAction::Restart | RecoveryAction::Close, None) => {}
            }
        }

        if let Some(pos) = context_menu_pos
            && matches!(self.overlays.kind, OverlayKind::None)
        {
            self.overlays.kind = OverlayKind::ContextMenu(pos);
        }

        if !overlay_open {
            self.resize_edges(ui);
        }

        let terminal_tabs = self.terminal_tab_count();
        actions.extend(overlays::show(
            &ctx,
            &mut self.overlays,
            &self.tokens,
            &mut self.config,
            &self.profiles,
            &tab_infos,
            terminal_tabs,
        ));

        for action in actions {
            self.handle_action(action);
        }

        let now = ctx.input(|input| input.time);
        let pointer_released = ctx.input(|input| input.pointer.any_released());
        if self.save_debounce.take_due(now, pointer_released) {
            self.persist();
        }

        self.drain_backdrop_warm();

        self.clamp_active();
        self.update_window_title(&ctx);
        self.sync_focus();
    }

    pub fn clear_color(&self) -> [f32; 4] {
        clear_color_for(
            self.surface_material(),
            self.native_transparency,
            self.theme.bg,
        )
    }
}

/// Surface painted behind a tab that has no grid yet (starting or failed).
///
/// Solid stays opaque; Mica/Acrylic use the same alpha as chrome and terminal
/// surfaces so the native backdrop shows through instead of an opaque block.
fn placeholder_fill(tokens: &Tokens, material: Material) -> Color32 {
    let alpha = design::material_alpha(material);
    if alpha == 255 {
        tokens.bg
    } else {
        Color32::from_rgba_unmultiplied(tokens.bg.r(), tokens.bg.g(), tokens.bg.b(), alpha)
    }
}

/// Input events that belong to a terminal session. Pointer/titlebar/window
/// events are deliberately excluded from pre-attach buffering.
fn session_input_events(events: &[Event]) -> Vec<Event> {
    events
        .iter()
        .filter(|event| {
            matches!(
                event,
                Event::Key { pressed: true, .. }
                    | Event::Text(_)
                    | Event::Ime(egui::ImeEvent::Commit(_))
                    | Event::Paste(_)
            )
        })
        .cloned()
        .collect()
}

/// Buffer keys typed while a shell is starting; the newest events win when the
/// cap is exceeded. They are encoded and written by `apply_spawn_results`.
fn buffer_pending_input(ui: &egui::Ui, pending: &mut Vec<Event>) {
    let events = ui.input(|input| session_input_events(&input.events));
    push_pending_input(pending, events);
}

fn push_pending_input(pending: &mut Vec<Event>, events: Vec<Event>) {
    if events.is_empty() {
        return;
    }
    pending.extend(events);
    if pending.len() > MAX_PENDING_INPUT_EVENTS {
        pending.drain(..pending.len() - MAX_PENDING_INPUT_EVENTS);
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

/// PowerShell's `Set-Location`/`cd` changes its provider location without
/// updating the Win32 current directory, so a PEB read would keep reporting
/// the startup folder. PowerShell 7 exposes `LocationChangedAction`, which
/// syncs the two without touching `prompt` and therefore without changing the
/// `$?` a custom prompt reports. Windows PowerShell 5.1 has no such hook; there
/// only the built-in/default prompt is wrapped, and a custom prompt is left
/// untouched (its cwd is not synced). The original prompt still produces the
/// visible text.
const POWERSHELL_CWD_HOOK: &str = concat!(
    "$global:__illness_sync = { try { [Environment]::CurrentDirectory = ",
    "$ExecutionContext.SessionState.Path.CurrentLocation.ProviderPath } catch {} }; ",
    "$global:__illness_invoke = $ExecutionContext.SessionState.InvokeCommand; ",
    "if ($global:__illness_invoke.PSObject.Properties['LocationChangedAction']) { ",
    "$global:__illness_invoke.LocationChangedAction = $global:__illness_sync ",
    "} elseif ($function:prompt -and ($function:prompt.ToString() -like ",
    "'*PS $($executionContext.SessionState.Path.CurrentLocation)*$nestedPromptLevel*')) { ",
    "$global:__illness_prompt = $function:prompt; ",
    "function global:prompt { & $global:__illness_sync; & $global:__illness_prompt } ",
    "}"
);

/// Recognize `pwsh`/`powershell` with or without the `.exe` suffix.
fn is_power_shell(program: &str) -> bool {
    Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name.eq_ignore_ascii_case("pwsh")
                || name.eq_ignore_ascii_case("pwsh.exe")
                || name.eq_ignore_ascii_case("powershell")
                || name.eq_ignore_ascii_case("powershell.exe")
        })
}

/// A PowerShell invocation that runs a command and exits does not need the
/// interactive cwd hook, and appending `-Command` there would change its
/// meaning.
fn powershell_runs_a_command(args: &[String]) -> bool {
    args.iter().any(|arg| {
        [
            "-command",
            "-c",
            "-encodedcommand",
            "-e",
            "-ec",
            "-file",
            "-f",
            "-commandwithargs",
        ]
        .iter()
        .any(|flag| arg.eq_ignore_ascii_case(flag))
    })
}

fn shell_args(profile: &Profile, inherit_cwd: bool) -> Vec<String> {
    let mut args = profile.args.clone();
    if inherit_cwd && is_power_shell(&profile.program) && !powershell_runs_a_command(&args) {
        args.push("-NoExit".to_string());
        args.push("-Command".to_string());
        args.push(POWERSHELL_CWD_HOOK.to_string());
    }
    args
}

fn spawn_spec(profile: &Profile, config: &Config, cwd: Option<&Path>) -> SpawnSpec {
    SpawnSpec {
        program: profile.program.clone(),
        args: shell_args(profile, config.inherit_cwd),
        // The last-ditch attempt (`cwd = None`) keeps the real process
        // directory: it always exists, while a profile folder may be redirected
        // to an unreachable share.
        cwd: cwd
            .map(Path::to_path_buf)
            .or_else(|| std::env::current_dir().ok()),
        env: vec![
            ("TERM".to_string(), "xterm-256color".to_string()),
            ("COLORTERM".to_string(), "truecolor".to_string()),
        ],
        scrollback: config.scrollback,
        cursor: config.cursor,
    }
}

/// One cwd candidate and whether an unverifiable (timed-out) value should be
/// kept. Only the explicit command-line launch folder is kept on timeout.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SpawnCandidate {
    path: PathBuf,
    startup: bool,
    process: bool,
    /// Came from the live shell, stored tab cwd, or launch folder rather than
    /// being only the process fallback.
    explicit: bool,
}

/// Collect live/stored/startup/process candidates once, in priority order.
/// Duplicates are collapsed and keep every role the path played.
fn spawn_candidates(
    live: Option<PathBuf>,
    stored: Option<PathBuf>,
    startup: Option<PathBuf>,
    process: Option<PathBuf>,
) -> Vec<SpawnCandidate> {
    let mut candidates: Vec<SpawnCandidate> = Vec::new();
    for (path, is_startup, is_process, explicit) in [
        (live, false, false, true),
        (stored, false, false, true),
        (startup, true, false, true),
        (process, false, true, false),
    ] {
        let Some(path) = path else {
            continue;
        };
        if let Some(existing) = candidates
            .iter_mut()
            .find(|candidate| candidate.path == path)
        {
            existing.startup |= is_startup;
            existing.process |= is_process;
            existing.explicit |= explicit;
        } else {
            candidates.push(SpawnCandidate {
                path,
                startup: is_startup,
                process: is_process,
                explicit,
            });
        }
    }
    candidates
}

/// Pick the ordered spawn attempts from probe results.
///
/// Any verified directory wins over the unverified process directory. A
/// timed-out *launch* folder is kept ahead of the process fallback, so a cold
/// SMB/VPN folder still opens where Explorer asked. A timed-out live or stored
/// path is dropped: a dead UNC shell cwd must not stall CreateProcess.
fn plan_from_probes(candidates: &[SpawnCandidate], kinds: &[platform::PathKind]) -> Vec<PathBuf> {
    let mut plan: Vec<PathBuf> = Vec::new();

    // Verified non-process directories, including the launch folder.
    for (candidate, kind) in candidates.iter().zip(kinds) {
        let process_only = candidate.process && !candidate.explicit;
        if !process_only && *kind == platform::PathKind::Dir {
            push_unique_path(&mut plan, &candidate.path);
        }
    }
    // A cold launch folder is more specific than the process fallback.
    if plan.is_empty() {
        for (candidate, kind) in candidates.iter().zip(kinds) {
            if candidate.startup && *kind == platform::PathKind::Timeout {
                push_unique_path(&mut plan, &candidate.path);
            }
        }
    }
    // The process directory is the final verified fallback.
    for (candidate, kind) in candidates.iter().zip(kinds) {
        if candidate.process && *kind == platform::PathKind::Dir {
            push_unique_path(&mut plan, &candidate.path);
        }
    }
    plan
}

fn push_unique_path(plan: &mut Vec<PathBuf>, path: &Path) {
    if !plan.iter().any(|known| known == path) {
        plan.push(path.to_path_buf());
    }
}

/// Probe every candidate once on the spawn worker and return the ordered plan.
///
/// Local candidates are checked inline; only network paths use the probe pool,
/// so this worker never blocks the UI and an unrelated local path never waits
/// behind a dead UNC share. The cancellation token stops a closed tab before
/// any `CreateProcess` attempt.
fn plan_spawn_cwds(candidates: &[SpawnCandidate], cancel: Option<&AtomicBool>) -> Vec<PathBuf> {
    let paths: Vec<PathBuf> = candidates
        .iter()
        .map(|candidate| candidate.path.clone())
        .collect();
    let kinds = match cancel {
        Some(cancel) => platform::probe_paths_cancellable(&paths, SPAWN_PATH_PROBE_TIMEOUT, cancel),
        None => platform::probe_paths(&paths, SPAWN_PATH_PROBE_TIMEOUT),
    };
    plan_from_probes(candidates, &kinds)
}

/// The full attempt list for one tab: the probed plan plus a final
/// process-directory attempt (`cwd = None`), deduplicated.
fn spawn_specs(profile: &Profile, config: &Config, dirs: &[PathBuf]) -> Vec<SpawnSpec> {
    let mut specs = Vec::new();
    let mut seen = Vec::new();
    for cwd in dirs
        .iter()
        .map(|path| Some(path.as_path()))
        .chain(std::iter::once(None))
    {
        let spec = spawn_spec(profile, config, cwd);
        if seen.contains(&spec.cwd) {
            continue;
        }
        seen.push(spec.cwd.clone());
        specs.push(spec);
    }
    specs
}

/// WSL's Windows-side process cwd is not the Linux shell's cwd, so a live
/// query would silently point new tabs at the wrong directory.
fn is_wsl_program(program: &str) -> bool {
    Path::new(program)
        .file_name()
        .is_some_and(|name| name.eq_ignore_ascii_case("wsl.exe"))
}

fn tab_live_cwd(tab: &Tab) -> Option<PathBuf> {
    let session = tab.session.as_ref()?;
    if session.is_exited() || is_wsl_program(&tab.program) {
        return None;
    }
    platform::process_cwd(session.child_pid()?)
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

fn removal_closes_window(remaining: usize) -> bool {
    remaining == 0
}

/// Count terminal tabs, ignoring the Settings tab.
///
/// Settings is a real slot in the tab strip but has no session to lose, so it
/// must not decide whether closing the window needs confirmation. Every
/// [`TabEntry::Terminal`] counts, including one whose spawn is pending or has
/// failed: it is still a tab the user sees.
fn terminal_tab_count(tabs: &[TabEntry]) -> usize {
    tabs.iter()
        .filter(|entry| matches!(entry, TabEntry::Terminal(_)))
        .count()
}

/// Whether a window close request must be confirmed first.
///
/// One tab closes straight away. A disabled setting keeps today's behaviour,
/// and a close the user already confirmed must never prompt twice.
fn should_confirm_close(tab_count: usize, setting_enabled: bool, already_confirmed: bool) -> bool {
    setting_enabled && !already_confirmed && tab_count > 1
}

/// Opaque, themed frame shared by the recovery card and tests.
fn recovery_frame(tokens: &Tokens) -> egui::Frame {
    egui::Frame::new()
        .fill(tokens.bg_elev)
        .stroke(Stroke::new(1.0, tokens.border))
        .corner_radius(tokens.radius_md)
        .inner_margin(egui::Margin::symmetric(18, 14))
}

/// Lightweight card shown while a shell is unusually slow to attach.
///
/// It only offers Cancel, which closes the tab and cancels the worker. At the
/// hard deadline the tab switches to the normal recovery card, so there is
/// never an unexplained blank terminal with no action.
fn show_starting_card(
    ctx: &egui::Context,
    tokens: &Tokens,
    tab_id: u64,
    profile: &str,
    elapsed: Duration,
) -> Option<RecoveryAction> {
    let mut action = None;
    egui::Window::new("Starting shell")
        .id(Id::unique(("illness-starting", tab_id)))
        .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
        .collapsible(false)
        .resizable(false)
        .frame(recovery_frame(tokens))
        .show(ctx, |ui| {
            ui.set_max_width(360.0);
            ui.label(
                egui::RichText::new(format!("Starting {profile}…"))
                    .size(tokens.font_body)
                    .color(tokens.text),
            );
            ui.add_space(tokens.gap_sm);
            ui.label(
                egui::RichText::new(format!(
                    "Waiting {:.1}s. A launch folder on an offline network share can delay this.",
                    elapsed.as_secs_f32()
                ))
                .size(tokens.font_small)
                .color(tokens.text_dim),
            );
            ui.add_space(tokens.gap_md);
            if ui.button("Cancel").clicked() {
                action = Some(RecoveryAction::Close);
            }
        });
    action
}

/// Centered recovery card for a shell that failed to start or exited during
/// startup. Returns the clicked action; the tab stays open until Close.
fn show_recovery_card(
    ctx: &egui::Context,
    tokens: &Tokens,
    tab_id: u64,
    profile: &str,
    title: &str,
    error: Option<&str>,
    exit_status: Option<i32>,
) -> Option<RecoveryAction> {
    let mut action = None;
    let failed_to_start = error.is_some();
    let window_title = if failed_to_start {
        "Shell failed to start"
    } else {
        "Shell ended"
    };
    egui::Window::new(window_title)
        .id(Id::unique(("illness-recovery", tab_id)))
        .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
        .collapsible(false)
        .resizable(false)
        .frame(recovery_frame(tokens))
        .show(ctx, |ui| {
            ui.set_max_width(380.0);
            let heading = if failed_to_start {
                format!("{profile} could not start.")
            } else {
                format!("{profile} exited during startup.")
            };
            ui.label(
                egui::RichText::new(heading)
                    .size(tokens.font_body)
                    .color(tokens.text),
            );
            if !title.trim().is_empty() && !title.eq_ignore_ascii_case(profile) {
                ui.label(
                    egui::RichText::new(title)
                        .size(tokens.font_small)
                        .color(tokens.text_dim),
                );
            }
            ui.add_space(tokens.gap_sm);
            if let Some(error) = error {
                ui.label(
                    egui::RichText::new(error)
                        .size(tokens.font_small)
                        .color(tokens.text_dim),
                );
            } else {
                let reason = match exit_status {
                    Some(code) if code != 0 => format!("Exit code {code}."),
                    _ => "The shell closed before it could be used.".to_string(),
                };
                ui.label(
                    egui::RichText::new(reason)
                        .size(tokens.font_small)
                        .color(tokens.text_dim),
                );
            }
            ui.add_space(tokens.gap_sm);
            ui.label(
                egui::RichText::new(
                    "The saved profile may point at a missing or uninstalled program.",
                )
                .size(tokens.font_small)
                .color(tokens.text_dim),
            );
            ui.add_space(tokens.gap_md);
            ui.horizontal(|ui| {
                if ui.button("Restart").clicked() {
                    action = Some(RecoveryAction::Restart);
                }
                if ui.button("Settings").clicked() {
                    action = Some(RecoveryAction::Settings);
                }
                if ui.button("Close").clicked() {
                    action = Some(RecoveryAction::Close);
                }
            });
        });
    action
}

/// Whether an exited shell should be kept with a recovery card instead of
/// being removed.
///
/// Recovery is limited to the startup window: an unattended shell that exits
/// ten minutes later is normal, not a broken profile. Inside the window only a
/// confirmed clean exit (`Some(0)`) after real input closes the tab. A missing
/// status (`None`) is always recovery because the child-exit event may not have
/// reached the UI yet, and treating that race as a normal exit hid failures.
fn needs_recovery(elapsed: Duration, user_acted: bool, exit_status: Option<i32>) -> bool {
    if elapsed > STARTUP_RECOVERY_WINDOW {
        return false;
    }
    match exit_status {
        Some(0) => !user_acted,
        Some(_) | None => true,
    }
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

/// Gate for global shortcuts only. **Never** gate terminal bytes on this.
///
/// `keyboard_input` is `Context::egui_wants_keyboard_input()`, which egui
/// implements as "some widget holds keyboard focus" — not "a text field is
/// focused". Shortcuts must yield to any focused widget (a text field, a
/// slider, the search box), so that is the right question here.
///
/// It is the wrong question for terminal input: `TerminalView::show` calls
/// `request_focus()` on the grid as soon as the user clicks it, so this gate
/// stays closed forever and every later keystroke is dropped. Terminal bytes
/// therefore use [`terminal_input_allowed`], which ignores focus on purpose.
/// The two gates are intentionally separate; do not merge them.
fn shortcuts_allowed(overlay_open: bool, settings_active: bool, keyboard_input: bool) -> bool {
    !overlay_open && !settings_active && !keyboard_input
}

/// Gate for writing encoded bytes to the active shell's PTY.
///
/// Deliberately focus-blind: no `egui_wants_keyboard_input()` here. Clicking the
/// grid focuses it (see [`shortcuts_allowed`]), and the grid *is* the terminal,
/// so focus on it must never suppress input. Only an open overlay, a shortcut
/// that already consumed this frame's key, or a dead PTY block the bytes.
///
/// `settings_active` is not a parameter because a Settings tab and a terminal
/// tab cannot occupy the active tab at the same time; Settings never reaches
/// this gate.
///
/// `keyboard_input` is accepted and deliberately ignored. Passing the frame's
/// `egui_wants_keyboard_input()` keeps this gate's call site identical in shape
/// to [`shortcuts_allowed`], so the focus state cannot silently reappear in the
/// comparison; `clicking_the_grid_does_not_block_terminal_input` fails if it
/// does.
fn terminal_input_allowed(
    overlay_open: bool,
    shortcut_used: bool,
    exited: bool,
    keyboard_input: bool,
) -> bool {
    let _ = keyboard_input;
    !overlay_open && !shortcut_used && !exited
}

/// Whether an event carries bytes for the active shell.
///
/// Must match what [`TerminalView::encode_input`] consumes; anything else on an
/// idle frame must not allocate or take the terminal lock.
fn is_terminal_input_event(event: &Event) -> bool {
    matches!(
        event,
        Event::Key { pressed: true, .. }
            | Event::Text(_)
            | Event::Ime(egui::ImeEvent::Commit(_))
            | Event::Paste(_)
    )
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

    fn cache_key(name: &str, digest: u64) -> backdrop::CacheKey {
        backdrop::CacheKey::Image {
            path: std::path::PathBuf::from(name),
            modified: None,
            len: Some(16),
            digest: Some(digest),
        }
    }

    #[test]
    fn settings_tab_does_not_count_toward_close_confirmation() {
        let terminal = || TabEntry::Terminal(Box::new(Tab::pending(1, &Profile::default(), None)));
        let tabs = vec![
            terminal(),
            TabEntry::Settings,
            TabEntry::Terminal(Box::new(Tab::pending(2, &Profile::default(), None))),
        ];
        assert_eq!(terminal_tab_count(&tabs), 2);
        assert!(
            should_confirm_close(terminal_tab_count(&tabs), true, false),
            "two shells plus Settings is two sessions, so it prompts"
        );

        // One shell plus Settings must take the single-tab fast path.
        let one_terminal = vec![terminal(), TabEntry::Settings];
        assert_eq!(terminal_tab_count(&one_terminal), 1);
        assert!(
            !should_confirm_close(terminal_tab_count(&one_terminal), true, false),
            "Settings alone must not turn one shell into a close prompt"
        );
        // Settings alone is not a terminal tab at all.
        assert_eq!(terminal_tab_count(&[TabEntry::Settings]), 0);
        assert!(!should_confirm_close(0, true, false));
    }

    #[test]
    fn close_confirmation_needs_tabs_setting_and_no_confirmation() {
        assert!(
            !should_confirm_close(1, true, false),
            "a single tab closes immediately"
        );
        assert!(
            should_confirm_close(2, true, false),
            "two tabs ask before closing"
        );
        assert!(
            !should_confirm_close(2, false, false),
            "the disabled setting keeps the old close path"
        );
        assert!(
            !should_confirm_close(2, true, true),
            "an already confirmed close must not prompt again"
        );
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
    fn shell_exit_selects_a_neighbor_or_closes_the_window() {
        // Removing a non-active tab keeps the same active terminal; its
        // index shifts left.
        assert_eq!(next_active_after_close(1, 0, 2), 0);
        // Removing the active tab picks the next tab when one exists.
        assert_eq!(next_active_after_close(1, 1, 2), 1);
        // Removing the active last tab picks the previous one.
        assert_eq!(next_active_after_close(2, 2, 2), 1);
        // No tab remains: the host closes the window instead of showing an
        // empty app.
        assert!(removal_closes_window(0));
        assert!(!removal_closes_window(1));
    }

    #[test]
    fn last_terminal_exit_leaves_the_settings_tab_active() {
        // [Settings, Terminal]: the active terminal was last.
        assert_eq!(next_active_after_close(1, 1, 1), 0);
        // [Terminal, Settings]: the terminal was first.
        assert_eq!(next_active_after_close(0, 0, 1), 0);
        assert!(!removal_closes_window(1));
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
    fn timed_out_warms_count_toward_the_retry_cap() {
        let now = Instant::now();
        let mut failures = 0;
        for attempt in 1..=WARM_RETRY_CAP {
            let (next, retry_at) = next_warm_retry(failures, now);
            assert_eq!(next, attempt);
            if attempt < WARM_RETRY_CAP {
                assert_eq!(
                    retry_at,
                    now.checked_add(WARM_RETRY_BASE * 2u32.pow(attempt - 1)),
                    "each supersede arms the same exponential backoff as a failure"
                );
            } else {
                assert_eq!(
                    retry_at,
                    now.checked_add(WARM_RECOVERY_INTERVAL),
                    "the cap must fall back to a slow recovery probe, not stop forever"
                );
            }
            failures = next;
        }
        assert_eq!(failures, WARM_RETRY_CAP);
    }

    #[test]
    fn superseded_warm_backoff_blocks_an_immediate_respawn() {
        let now = Instant::now();
        let (failures, retry_at) = next_warm_retry(0, now);
        assert!(
            warm_start_blocked(failures, retry_at, false, now),
            "a timed-out warm must wait for its backoff"
        );
        assert!(!warm_start_blocked(
            failures,
            retry_at,
            false,
            now + WARM_RETRY_BASE
        ));
        assert!(
            !warm_start_blocked(failures, retry_at, true, now),
            "revalidation ignores the short backoff but not the cap"
        );

        let (capped, recovery_at) = next_warm_retry(WARM_RETRY_CAP - 1, now);
        assert_eq!(capped, WARM_RETRY_CAP);
        assert!(
            warm_start_blocked(capped, recovery_at, false, now + WARM_RECOVERY_INTERVAL),
            "a capped source must not respawn on the 30 s timeout path"
        );
        assert!(
            warm_start_blocked(capped, recovery_at, true, now),
            "a recovery probe must wait for its slow deadline"
        );
        assert!(
            !warm_start_blocked(capped, recovery_at, true, now + WARM_RECOVERY_INTERVAL),
            "the slow probe must run once its deadline is due"
        );
    }

    #[test]
    fn cap_recovers_after_an_outage_and_stops_after_repeated_slow_failures() {
        let now = Instant::now();
        let (mut failures, mut retry_at) = next_warm_retry(0, now);
        while failures < WARM_RETRY_CAP {
            (failures, retry_at) = next_warm_retry(failures, now);
        }
        assert_eq!(retry_at, now.checked_add(WARM_RECOVERY_INTERVAL));

        // The outage clears: the first recovery probe succeeds and resets the
        // streak, so the next visible revalidation runs normally.
        assert!(!warm_start_blocked(
            failures,
            retry_at,
            true,
            now + WARM_RECOVERY_INTERVAL
        ));
        assert!(!warm_start_blocked(0, None, true, now));

        // Repeated slow failures eventually stop automatic probing.
        let mut last_retry = retry_at;
        while failures < WARM_RETRY_CAP + WARM_RECOVERY_CAP {
            (failures, last_retry) = next_warm_retry(failures, now);
        }
        assert_eq!(failures, WARM_RETRY_CAP + WARM_RECOVERY_CAP);
        assert_eq!(last_retry, None);
        assert!(
            warm_start_blocked(failures, last_retry, true, now + Duration::from_secs(3600)),
            "only an explicit rebuild may probe after the recovery cap"
        );
    }

    #[test]
    fn late_cache_after_supersede_uploads_over_the_painted_texture() {
        let painted = cache_key("wallpaper.png", 111);
        let late = cache_key("wallpaper.png", 222);
        assert!(
            backdrop_upload_needed(Some(&painted), Some(&late), false),
            "a cache key that changed after the timeout rebuild must upload"
        );
        assert!(
            !backdrop_upload_needed(Some(&painted), Some(&painted), false),
            "the same painted key must stay unchanged"
        );
        assert!(
            backdrop_upload_needed(None, Some(&painted), false),
            "a missing texture uploads a cached result"
        );
        assert!(
            !backdrop_upload_needed(None, None, false),
            "a cold miss must warm instead of rebuilding nothing"
        );
        assert!(
            backdrop_upload_needed(Some(&painted), Some(&painted), true),
            "a fresh decode still uploads"
        );
    }

    #[test]
    fn warm_failures_count_before_the_paint_gate() {
        let now = Instant::now();
        let acrylic_desktop = paints_backdrop(Material::Acrylic, BackgroundMode::Desktop);
        assert!(
            !acrylic_desktop,
            "Acrylic+Desktop prewarms without painting a texture"
        );
        let (failures, retry_at) = next_warm_retry(0, now);
        assert_eq!(
            failures, 1,
            "an unpainted prewarm failure still consumes the shared cap"
        );
        assert_eq!(retry_at, now.checked_add(WARM_RETRY_BASE));
        assert!(
            !warm_failure_toasts(acrylic_desktop, true, failures),
            "an unpainted failure must not toast"
        );
        assert!(warm_failure_toasts(true, true, 1));
    }

    #[test]
    fn focus_regain_revalidation_is_debounced() {
        let now = Instant::now();
        assert!(focus_revalidation_due(None, now), "first focus revalidates");
        assert!(!focus_revalidation_due(
            Some(now - Duration::from_secs(1)),
            now
        ));
        assert!(focus_revalidation_due(
            Some(now - FOCUS_REVALIDATE_FLOOR),
            now
        ));
    }

    #[test]
    fn focus_and_timer_revalidate_only_visible_backgrounds() {
        let now = Instant::now();
        assert!(revalidation_due(None, now, true, true), "first focus");
        assert!(revalidation_due(
            Some(now - Duration::from_secs(1)),
            now,
            true,
            true
        ));
        assert!(!revalidation_due(
            Some(now + Duration::from_secs(1)),
            now,
            true,
            true
        ));
        assert!(
            !revalidation_due(None, now, false, true),
            "minimized/occluded windows do not wake"
        );
        assert!(
            !revalidation_due(None, now, true, false),
            "Solid/None never revalidate"
        );
    }

    #[test]
    fn leaving_acrylic_rebuilds_after_the_apply_updates_applied_material() {
        // The previous material must be captured before the apply; comparing
        // with the already-updated field made this transition unreachable.
        assert!(material_change_needs_rebuild(
            Material::Acrylic,
            Material::Mica,
            true
        ));
        assert!(!material_change_needs_rebuild(
            Material::Acrylic,
            Material::Acrylic,
            true
        ));
        assert!(!material_change_needs_rebuild(
            Material::Mica,
            Material::Mica,
            true
        ));
        assert!(
            material_change_needs_rebuild(Material::Mica, Material::Mica, false),
            "a missing texture still rebuilds"
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
        let spec = spawn_spec(&profile, &config, None);
        assert_eq!(spec.scrollback, 25_000);
        assert_eq!(spec.cursor, illness_core::CursorKind::Beam);
    }

    #[test]
    fn new_tabs_start_in_the_command_line_folder() {
        let profile = profile("PowerShell");
        let config = Config::default();
        let startup = std::env::temp_dir();

        let spec = spawn_spec(&profile, &config, Some(&startup));

        assert_eq!(spec.cwd.as_deref(), Some(startup.as_path()));
    }

    #[test]
    fn no_command_line_folder_keeps_the_process_directory() {
        let profile = profile("PowerShell");
        let config = Config::default();

        let spec = spawn_spec(&profile, &config, None);

        assert_eq!(spec.cwd, std::env::current_dir().ok());
    }

    #[test]
    fn spawn_attempts_try_each_cwd_then_the_process_directory_once() {
        let profile = profile("PowerShell");
        let config = Config::default();
        let first = PathBuf::from(r"C:\first");
        let second = PathBuf::from(r"C:\second");

        let specs = spawn_specs(&profile, &config, &[first.clone(), second.clone()]);
        assert_eq!(specs.len(), 3);
        assert_eq!(specs[0].cwd.as_ref(), Some(&first));
        assert_eq!(specs[1].cwd.as_ref(), Some(&second));
        assert_eq!(specs[2].cwd, std::env::current_dir().ok());

        let process = std::env::current_dir().expect("test process cwd");
        let specs = spawn_specs(&profile, &config, std::slice::from_ref(&process));
        assert_eq!(
            specs.len(),
            1,
            "the final process fallback must not repeat a candidate"
        );
        assert_eq!(specs[0].cwd.as_ref(), Some(&process));
    }

    #[test]
    fn live_shell_cwd_wins_for_a_new_tab() {
        let live = PathBuf::from(r"C:\live");
        let stored = PathBuf::from(r"C:\stored");
        let startup = PathBuf::from(r"C:\startup");
        let process = PathBuf::from(r"C:\process");
        let candidates = spawn_candidates(
            Some(live.clone()),
            Some(stored.clone()),
            Some(startup.clone()),
            Some(process.clone()),
        );
        let kinds = [
            platform::PathKind::Dir,
            platform::PathKind::Dir,
            platform::PathKind::Dir,
            platform::PathKind::Dir,
        ];

        assert_eq!(
            plan_from_probes(&candidates, &kinds),
            vec![live, stored, startup, process],
            "verified cwds keep priority order; process is the final fallback"
        );
    }

    #[test]
    fn deleted_live_cwd_falls_back_instead_of_failing_create_process() {
        let live = PathBuf::from(r"C:\deleted");
        let stored = PathBuf::from(r"C:\stored");
        let startup = PathBuf::from(r"C:\startup");
        let process = PathBuf::from(r"C:\process");
        let candidates = spawn_candidates(
            Some(live),
            Some(stored.clone()),
            Some(startup.clone()),
            Some(process.clone()),
        );
        let kinds = [
            platform::PathKind::NotDir,
            platform::PathKind::Dir,
            platform::PathKind::Dir,
            platform::PathKind::Dir,
        ];

        assert_eq!(
            plan_from_probes(&candidates, &kinds),
            vec![stored, startup, process]
        );
    }

    #[test]
    fn dead_unc_live_cwd_is_not_handed_to_conpty() {
        let live = PathBuf::from(r"\\offline\share\folder");
        let startup = PathBuf::from(r"C:\startup");
        let process = PathBuf::from(r"C:\process");
        let candidates = spawn_candidates(
            Some(live.clone()),
            Some(live.clone()),
            Some(startup.clone()),
            Some(process.clone()),
        );
        let kinds = [
            platform::PathKind::Timeout,
            platform::PathKind::Dir,
            platform::PathKind::Dir,
        ];

        assert_eq!(
            plan_from_probes(&candidates, &kinds),
            vec![startup, process.clone()],
            "a timed-out shell cwd must not stall or fail the new tab"
        );

        // Even a cold launch folder is preferred over the process fallback.
        let candidates = spawn_candidates(None, None, Some(live.clone()), Some(process.clone()));
        let kinds = [platform::PathKind::Timeout, platform::PathKind::Dir];
        assert_eq!(
            plan_from_probes(&candidates, &kinds),
            vec![live, process],
            "a launch folder timeout is kept, not replaced by C:\\Windows"
        );
    }

    #[test]
    fn spawn_candidates_dedupe_and_keep_launch_priority() {
        let shared = PathBuf::from(r"C:\shared");
        let process = PathBuf::from(r"C:\process");
        let candidates = spawn_candidates(
            Some(shared.clone()),
            Some(shared.clone()),
            Some(shared.clone()),
            Some(process.clone()),
        );

        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].path, shared);
        assert!(candidates[0].startup);
        assert!(candidates[0].explicit);
        assert!(!candidates[0].process);
        assert_eq!(candidates[1].path, process);
        assert!(candidates[1].process);
    }

    #[test]
    fn inherit_cwd_setting_chooses_active_folder_or_launch_folder() {
        let live = PathBuf::from(r"C:\live");
        let startup = PathBuf::from(r"C:\startup");
        let process = PathBuf::from(r"C:\process");
        let inherited = spawn_candidates(
            Some(live.clone()),
            Some(live.clone()),
            Some(startup.clone()),
            Some(process.clone()),
        );
        assert_eq!(inherited[0].path, live);
        assert!(inherited[0].explicit, "inheritance keeps the shell cwd");

        let ignored = spawn_candidates(None, None, Some(startup.clone()), Some(process.clone()));
        assert_eq!(ignored[0].path, startup);
        assert!(
            ignored[0].startup,
            "disabled inheritance uses the launch folder"
        );
    }

    #[test]
    fn wsl_live_cwd_is_not_used() {
        assert!(is_wsl_program(r"C:\Windows\System32\wsl.exe"));
        assert!(is_wsl_program("WSL.EXE"));
        assert!(!is_wsl_program(r"C:\Program Files\PowerShell\7\pwsh.exe"));
    }

    #[test]
    fn interactive_powershell_gets_the_cwd_prompt_hook() {
        let mut profile = profile("PowerShell");
        profile.program = r"C:\Program Files\PowerShell\7\pwsh.exe".to_string();

        let args = shell_args(&profile, true);

        assert_eq!(&args[..2], ["-NoExit", "-Command"]);
        assert_eq!(args[2], POWERSHELL_CWD_HOOK);
        assert_eq!(
            shell_args(&profile, false),
            profile.args,
            "disabled inheritance needs no prompt hook"
        );
    }

    #[test]
    fn powershell_command_invocations_keep_their_arguments() {
        let mut profile = profile("PowerShell");
        profile.program = "pwsh.exe".to_string();
        profile.args = vec!["-NoProfile".into(), "-Command".into(), "Get-Date".into()];

        assert_eq!(shell_args(&profile, true), profile.args);
    }

    #[test]
    fn other_shells_keep_their_arguments() {
        let mut profile = profile("Command Prompt");
        profile.program = "cmd.exe".to_string();
        profile.args = vec!["/k".into(), "echo hello".into()];

        assert_eq!(shell_args(&profile, true), profile.args);
    }

    #[test]
    fn terminal_input_needs_no_overlay_no_shortcut_and_a_live_shell() {
        assert!(terminal_input_allowed(false, false, false, false));
        assert!(
            !terminal_input_allowed(true, false, false, false),
            "overlay owns the keys"
        );
        assert!(
            !terminal_input_allowed(false, true, false, false),
            "shortcut consumed the key"
        );
        assert!(
            !terminal_input_allowed(false, false, true, false),
            "exited shell has no PTY"
        );
        assert!(!terminal_input_allowed(true, true, true, false));
    }

    /// Regression: the grid takes egui focus when clicked, and terminal input
    /// used to be gated on `egui_wants_keyboard_input()`, so one click in the
    /// terminal killed all later keystrokes. This runs a real `egui::Context`
    /// so `request_focus()` produces the same memory state the app sees.
    #[test]
    fn clicking_the_grid_does_not_block_terminal_input() {
        let ctx = egui::Context::default();
        // Two frames, because focus is only observable through the memory the
        // following frame reads; the widget stays alive so focus is not
        // surrendered as "uninterested".
        for _ in 0..2 {
            let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
                let response = ui.allocate_response(ui.available_size(), Sense::click());
                response.request_focus();
            });
            // No renderer in a headless test; the font texture delta must be
            // acknowledged or `epaint` panics when it is dropped.
            output.textures_delta.clear();
        }

        assert!(
            ctx.egui_wants_keyboard_input(),
            "the grid-like widget really holds egui focus"
        );
        assert!(
            terminal_input_allowed(false, false, false, ctx.egui_wants_keyboard_input()),
            "a focused grid must never suppress terminal bytes"
        );
        assert!(
            !shortcuts_allowed(false, false, ctx.egui_wants_keyboard_input()),
            "shortcuts still yield to a focused widget"
        );
    }

    #[test]
    fn pending_input_keeps_newest_events_and_encodes_an_exit() {
        let mut pending = Vec::new();
        let mut events = Vec::new();
        for index in 0..(MAX_PENDING_INPUT_EVENTS + 10) {
            events.push(Event::Text(format!("{index}")));
        }
        push_pending_input(&mut pending, events);
        assert_eq!(pending.len(), MAX_PENDING_INPUT_EVENTS);
        assert!(
            matches!(pending.first(), Some(Event::Text(text)) if text == "10"),
            "the oldest pre-attach keys are discarded, not the newest"
        );

        let exit = session_input_events(&[
            Event::Text("exit".to_string()),
            Event::Key {
                key: Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::default(),
            },
        ]);
        let view = TerminalView::default();
        assert_eq!(
            view.encode_input(
                &exit,
                Modifiers::default(),
                illness_term::TermModes::default()
            ),
            Some(b"exit\r".to_vec()),
            "buffered pre-attach typing is written once the session attaches"
        );
    }

    #[test]
    fn recovery_is_bounded_and_respects_a_deliberate_exit() {
        // A profile that fails during startup must not become "no tabs".
        assert!(needs_recovery(Duration::ZERO, false, None));
        assert!(needs_recovery(Duration::from_millis(100), false, Some(0)));
        assert!(needs_recovery(
            STARTUP_RECOVERY_WINDOW - Duration::from_millis(1),
            false,
            Some(0)
        ));
        // A deliberate `exit` after typing closes only with a confirmed 0.
        assert!(!needs_recovery(Duration::from_millis(200), true, Some(0)));
        // A missing status is a race, never a normal exit.
        assert!(needs_recovery(Duration::from_millis(200), true, None));
        // A non-zero status inside the window is still a failure.
        assert!(needs_recovery(Duration::from_millis(200), true, Some(1)));
        // After the startup window an unattended normal exit is expected.
        assert!(!needs_recovery(Duration::from_secs(600), false, Some(0)));
        assert!(!needs_recovery(
            STARTUP_RECOVERY_WINDOW + Duration::from_millis(1),
            false,
            None
        ));
    }

    #[test]
    fn chrome_clicks_are_not_shell_activity() {
        let key = Event::Key {
            key: Key::A,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::default(),
        };
        let text = Event::Text("a".to_string());
        let pointer = Event::PointerButton {
            pos: pos2(10.0, 10.0),
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: Modifiers::default(),
        };
        let wheel = Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: vec2(0.0, 20.0),
            phase: egui::TouchPhase::Move,
            source: egui::MouseWheelSource::Unknown,
            modifiers: Modifiers::default(),
        };
        let moved = Event::PointerMoved(pos2(5.0, 5.0));

        let session = session_input_events(&[key.clone(), text.clone(), pointer, wheel, moved]);
        assert_eq!(session, vec![key, text]);
        assert!(
            session_input_events(&[
                Event::PointerButton {
                    pos: pos2(1.0, 1.0),
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: Modifiers::default(),
                },
                Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Line,
                    delta: vec2(0.0, 1.0),
                    phase: egui::TouchPhase::Move,
                    source: egui::MouseWheelSource::Unknown,
                    modifiers: Modifiers::default(),
                },
            ])
            .is_empty(),
            "titlebar/plus/settings clicks must not mark the shell used"
        );
    }

    #[test]
    fn spawn_wait_times_out_and_cancels() {
        let (sender, receiver) = mpsc::channel::<u8>();
        let cancel = AtomicBool::new(false);
        let started = Instant::now();
        assert_eq!(
            wait_for_spawn(&receiver, &cancel, Duration::from_millis(30)),
            SpawnWait::Timeout
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(sender);

        let (_sender, receiver) = mpsc::channel::<u8>();
        let cancel = AtomicBool::new(true);
        let started = Instant::now();
        assert_eq!(
            wait_for_spawn(&receiver, &cancel, Duration::from_secs(5)),
            SpawnWait::Cancelled
        );
        assert!(started.elapsed() < Duration::from_millis(500));

        let (sender, receiver) = mpsc::channel::<u8>();
        sender.send(7).expect("send");
        let cancel = AtomicBool::new(false);
        assert_eq!(
            wait_for_spawn(&receiver, &cancel, Duration::from_secs(1)),
            SpawnWait::Ready(7)
        );
    }

    #[test]
    fn cold_unc_cancel_and_closed_tab_results_are_discarded() {
        // A stuck CreateProcess on a cold share is abandoned as soon as the tab
        // is closed; the wait does not run out the full attempt deadline.
        let (sender, receiver) = mpsc::channel::<u8>();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            let _ = sender.send(1);
        });
        let cancel = Arc::new(AtomicBool::new(false));
        let setter = Arc::clone(&cancel);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            setter.store(true, Ordering::Release);
        });
        let started = Instant::now();
        assert_eq!(
            wait_for_spawn(&receiver, &cancel, Duration::from_secs(5)),
            SpawnWait::Cancelled
        );
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "closing the tab must interrupt the spawn wait quickly"
        );

        // A tab that was closed has no target: the late session is dropped
        // rather than attached to another tab.
        let profile = profile("PowerShell");
        let mut tabs = vec![TabEntry::Terminal(Box::new(Tab::pending(
            4,
            &profile,
            Some(PathBuf::from(r"\\cold\share")),
        )))];
        assert!(terminal_tab_mut(&mut tabs, 4).is_some());
        assert!(terminal_tab_mut(&mut tabs, 5).is_none());
    }

    #[test]
    fn pending_input_is_cleared_when_a_spawn_fails() {
        let profile = profile("Missing");
        let mut tab = Tab::pending(9, &profile, Some(PathBuf::from(r"C:\gone")));
        tab.pending_input.push(Event::Text("exit\r".to_string()));
        apply_spawn_outcome(&mut tab, Err("program not found".to_string()));
        assert!(
            tab.pending_input.is_empty(),
            "keys typed while starting must not survive a failed spawn"
        );
    }

    #[test]
    fn sessionless_placeholder_keeps_the_material_alpha() {
        for theme in Theme::all() {
            let tokens = Tokens::from_theme(theme);
            assert_eq!(placeholder_fill(&tokens, Material::Solid), tokens.bg);
            for (material, alpha) in [
                (Material::Mica, design::material_alpha(Material::Mica)),
                (Material::Acrylic, design::material_alpha(Material::Acrylic)),
            ] {
                let fill = placeholder_fill(&tokens, material);
                assert_eq!(fill.a(), alpha, "{} {material:?}", theme.name);
                let [red, green, blue, _] = fill.to_srgba_unmultiplied();
                assert!(
                    red.abs_diff(tokens.bg.r()) <= 2
                        && green.abs_diff(tokens.bg.g()) <= 2
                        && blue.abs_diff(tokens.bg.b()) <= 2,
                    "{} {material:?} must keep the theme background color",
                    theme.name
                );
                assert_ne!(fill.a(), 255, "the native backdrop must show through");
            }
        }
    }

    #[test]
    fn spawn_result_is_kept_for_recovery_without_a_status() {
        // `remove_exited_tabs` uses this predicate before the ChildExit event
        // can be drained; a missing status must never close the tab.
        assert!(needs_recovery(Duration::ZERO, true, None));
        assert!(needs_recovery(STARTUP_RECOVERY_WINDOW, true, None));
    }

    #[test]
    fn spawn_failure_is_a_recovery_tab_without_a_session() {
        let profile = profile("Missing");
        let mut tab = Tab::pending(9, &profile, Some(PathBuf::from(r"C:\gone")));
        assert!(tab.session.is_none());
        assert!(!tab.recovery);

        apply_spawn_outcome(
            &mut tab,
            Err("Could not start Missing: program not found".to_string()),
        );

        assert!(tab.recovery);
        assert!(tab.session.is_none());
        assert_eq!(
            tab.error.as_deref(),
            Some("Could not start Missing: program not found")
        );
        assert_eq!(tab.profile_name, "Missing");
    }

    #[test]
    fn recovery_card_is_opaque_in_every_theme() {
        for theme in Theme::all() {
            let tokens = Tokens::from_theme(theme);
            let frame = recovery_frame(&tokens);
            assert_eq!(frame.fill, tokens.bg_elev);
            assert_eq!(
                frame.fill.a(),
                255,
                "card must cover the terminal in {}",
                theme.name
            );
            assert_eq!(frame.stroke.color, tokens.border);
        }
    }

    #[test]
    fn powershell_is_recognized_without_the_exe_suffix() {
        assert!(is_power_shell("pwsh"));
        assert!(is_power_shell("pwsh.exe"));
        assert!(is_power_shell("powershell"));
        assert!(is_power_shell(r"C:\Program Files\PowerShell\7\pwsh.exe"));
        assert!(!is_power_shell("cmd.exe"));
        assert!(!is_power_shell("my-pwsh-wrapper.exe"));
    }

    #[test]
    fn pwsh_without_exe_gets_the_cwd_hook() {
        let mut profile = profile("PowerShell");
        profile.program = "pwsh".to_string();

        let args = shell_args(&profile, true);

        assert_eq!(&args[..2], ["-NoExit", "-Command"]);
        assert_eq!(args[2], POWERSHELL_CWD_HOOK);
    }

    #[test]
    fn prompt_hook_targets_location_changes_and_default_prompts_only() {
        assert!(POWERSHELL_CWD_HOOK.contains("LocationChangedAction"));
        assert!(POWERSHELL_CWD_HOOK.contains("$nestedPromptLevel"));
        // The custom prompt assignment is inside the 5.1-only `elseif` guard.
        let guard = POWERSHELL_CWD_HOOK
            .find("elseif")
            .expect("the prompt fallback is guarded");
        let wrap = POWERSHELL_CWD_HOOK
            .find("$global:__illness_prompt = $function:prompt")
            .expect("the default prompt fallback exists");
        assert!(guard < wrap);
    }

    #[cfg(target_os = "windows")]
    fn test_powershells() -> Vec<std::path::PathBuf> {
        use std::process::Command;

        let mut shells = Vec::new();
        if Command::new("pwsh")
            .args(["-NoLogo", "-NoProfile", "-Command", "exit 0"])
            .status()
            .is_ok_and(|status| status.success())
        {
            shells.push(std::path::PathBuf::from("pwsh"));
        }
        if let Some(powershell) = std::env::var_os("SystemRoot").map(|root| {
            std::path::PathBuf::from(root)
                .join("System32")
                .join("WindowsPowerShell")
                .join("v1.0")
                .join("powershell.exe")
        }) && powershell.is_file()
        {
            shells.push(powershell);
        }
        shells
    }

    #[cfg(target_os = "windows")]
    fn run_powershell(shell: &std::path::Path, script: &str) -> String {
        use std::process::Command;

        let output = Command::new(shell)
            .args(["-NoLogo", "-NoProfile", "-Command", script])
            .output()
            .expect("run PowerShell");
        assert!(
            output.status.success(),
            "PowerShell failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn prompt_hook_preserves_a_custom_prompts_success_status() {
        for shell in test_powershells() {
            let script = format!(
                "function global:prompt {{ \"CUSTOM ok=$? \" }}; {POWERSHELL_CWD_HOOK}; \
                 try {{ throw 'illness-test' }} catch {{}}; prompt"
            );
            let output = run_powershell(&shell, &script);
            assert!(
                output.contains("CUSTOM ok=False"),
                "a custom prompt must still see the failed command in {}, got {output:?}",
                shell.display()
            );
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn prompt_hook_syncs_cwd_for_the_default_prompt() {
        for shell in test_powershells() {
            let script = format!(
                "{POWERSHELL_CWD_HOOK}; Set-Location $env:TEMP; prompt | Out-Null; \
                 if ([Environment]::CurrentDirectory -ieq [IO.Path]::GetFullPath($env:TEMP)) {{ 'MATCH' }} else {{ [Environment]::CurrentDirectory }}"
            );
            let output = run_powershell(&shell, &script);
            assert!(
                output.contains("MATCH"),
                "the cwd hook must update the process directory in {}, got {output:?}",
                shell.display()
            );
        }
    }
}
