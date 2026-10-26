use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use egui::{
    Color32, ColorImage, CursorIcon, Event, ImeEvent, Key as EguiKey, Modifiers, MouseWheelUnit,
    PointerButton, Pos2, Rect, Sense, TextureHandle, TextureOptions, Ui, ViewportCommand, pos2,
    vec2,
};
use illness_core::{Config, CursorKind, Material, Theme};
use illness_term::snapshot::CellFlags;
use illness_term::{
    Color, GridSize, Key, KeyInput, KeyModes, Mods, MouseButton, MousePhase, SelectionSpan,
    SelectionType, Session, Side, Snapshot, SnapshotCell, TermModes, encode_key, encode_mouse,
    encode_paste,
};

use crate::design::{self, Tokens};
use crate::glyph::{CellMetrics, GlyphAtlas, GlyphStyle};
use crate::gpu::GpuText;
use crate::soft::{self, SurfaceSpec};

/// Unique texture names, since every tab owns its own cached surface.
static NEXT_SOFT_TEXTURE: AtomicU64 = AtomicU64::new(0);

/// `TextureHandle` has no `Debug` implementation; keep `TerminalView` debuggable.
#[derive(Default)]
struct SoftTexture(Option<TextureHandle>);

impl std::fmt::Debug for SoftTexture {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SoftTexture")
    }
}

/// Last composited surface, kept for row-band diffing before uploads.
#[derive(Default)]
struct SoftImage(Option<ColorImage>);

impl std::fmt::Debug for SoftImage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SoftImage")
    }
}

/// Cost of the last partial surface upload, for the debug log and tests.
#[derive(Debug, Default, Clone, Copy)]
struct UploadStats {
    bands: usize,
    bytes: usize,
    elapsed: Duration,
}

/// Grid size plus physical cell size; any change needs a PTY resize.
type LayoutKey = (usize, usize, u32, u32);

/// Delay before committing a drag-resize. Intermediate sizes are dropped so
/// the 10k-line scrollback is not reflowed every pointer frame.
const RESIZE_DEBOUNCE_SECS: f64 = 0.09;

#[derive(Debug, Clone, Copy, PartialEq)]
enum ResizePoll {
    Idle,
    Apply,
    Wait(f64),
}

/// Debounces terminal resizes until the size is stable or the pointer is
/// released, while the cached surface keeps painting.
#[derive(Debug, Default)]
struct ResizeDebounce {
    pending: Option<(LayoutKey, f64)>,
}

impl ResizeDebounce {
    fn poll(
        &mut self,
        current: LayoutKey,
        applied: Option<LayoutKey>,
        now: f64,
        immediate: bool,
    ) -> ResizePoll {
        if applied == Some(current) {
            self.pending = None;
            return ResizePoll::Idle;
        }
        // Startup and pointer release apply the exact final size at once.
        if immediate {
            self.pending = None;
            return ResizePoll::Apply;
        }
        match self.pending {
            Some((pending, deadline)) if pending == current => {
                if now >= deadline {
                    self.pending = None;
                    ResizePoll::Apply
                } else {
                    ResizePoll::Wait(deadline)
                }
            }
            _ => {
                let deadline = now + RESIZE_DEBOUNCE_SECS;
                self.pending = Some((current, deadline));
                ResizePoll::Wait(deadline)
            }
        }
    }
}

/// Seconds to wait before compositing, or `None` to render now.
///
/// Only coalesce inside the first half of the measured display interval. That
/// removes the render/skip cadence at 75 Hz without ever delaying a changed
/// frame by more than half a refresh.
pub(crate) fn coalesce_delay(elapsed: f64, frame_dt: f64) -> Option<f64> {
    let interval = frame_dt.clamp(1.0 / 240.0, 1.0 / 24.0);
    let half = interval * 0.5;
    if elapsed >= 0.0 && elapsed < half {
        Some((half - elapsed).max(0.001))
    } else {
        None
    }
}

/// Width of the scrollbar hit strip, in logical points.
const SCROLLBAR_WIDTH: f32 = 10.0;

/// Gap kept between the last cell and the strip. egui's `Rect::intersects`
/// counts touching edges as overlapping, so the strip starts strictly right
/// of the grid's cell area.
const SCROLLBAR_GAP: f32 = 1.0;

/// Smallest grabbable thumb, so a million lines of scrollback still drags.
const SCROLLBAR_MIN_THUMB: f32 = 24.0;

/// Width of the fallback overlay strip used when the padding gutter is too
/// narrow for the full strip. It lives just inside the last column and only
/// paints (and takes input) while the pointer is on it.
const SCROLLBAR_OVERLAY_WIDTH: f32 = 4.0;

/// Vertical scrollbar layout overlaid on the padding gutter right of the grid.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Scrollbar {
    track: Rect,
    thumb: Rect,
    /// True for the narrow fallback that sits inside the grid's right edge.
    /// It is invisible until hovered, so it never permanently covers cells.
    overlay: bool,
}

impl Scrollbar {
    /// `None` when there is no scrollback, there is no gutter to live in, or
    /// the grid has no usable height. The alternate screen and short
    /// sessions therefore keep the old hit-testing exactly.
    fn for_grid(
        content_rect: Rect,
        grid_rect: Rect,
        viewport: usize,
        history: usize,
        display_offset: usize,
    ) -> Option<Self> {
        if history == 0 {
            return None;
        }
        // A gutter that can hold the full strip always wins. A narrower one
        // (including padding 0/1, where no gutter exists) falls back to a
        // narrow overlay inside the last column instead of losing the
        // scroll affordance entirely.
        let right = content_rect.right();
        let left = (grid_rect.right() + SCROLLBAR_GAP).max(content_rect.left());
        let gutter = right - left;
        let overlay = gutter < SCROLLBAR_WIDTH;
        let track = if overlay {
            let width = SCROLLBAR_OVERLAY_WIDTH.min(grid_rect.width()).max(0.0);
            if width <= 0.0 {
                return None;
            }
            Rect::from_min_max(
                pos2(grid_rect.right() - width, grid_rect.top()),
                pos2(grid_rect.right(), grid_rect.bottom()),
            )
        } else {
            Rect::from_min_max(
                pos2(right - SCROLLBAR_WIDTH, grid_rect.top()),
                pos2(right, grid_rect.bottom()),
            )
        };
        if track.height() <= 0.0 {
            return None;
        }
        Some(Self::with_thumb_height(
            track,
            scrollbar_thumb_length(track.height(), viewport, history),
            history,
            display_offset,
            overlay,
        ))
    }

    /// Layout for an already-established gutter track rectangle.
    #[cfg(test)]
    fn new(track: Rect, viewport: usize, history: usize, display_offset: usize) -> Self {
        let thumb_height = scrollbar_thumb_length(track.height(), viewport, history);
        Self::with_thumb_height(track, thumb_height, history, display_offset, false)
    }

    /// Layout with an explicit thumb length, used while a drag paints the
    /// offset it just applied instead of the stale snapshot's.
    fn with_thumb_height(
        track: Rect,
        thumb_height: f32,
        history: usize,
        display_offset: usize,
        overlay: bool,
    ) -> Self {
        let thumb_height = thumb_height.clamp(0.0, track.height());
        let travel = (track.height() - thumb_height).max(0.0);
        let fraction = if history == 0 {
            0.0
        } else {
            (display_offset.min(history) as f32 / history as f32).clamp(0.0, 1.0)
        };
        // Offset 0 is the live bottom, so the thumb starts at the far end.
        let top = track.top() + travel * (1.0 - fraction);
        let thumb = Rect::from_min_size(pos2(track.left(), top), vec2(track.width(), thumb_height));
        Self {
            track,
            thumb,
            overlay,
        }
    }

    /// Whether the strip is visible while nobody is pointing at it. Only the
    /// gutter strip is; the overlay stays hidden so it does not permanently
    /// cover the last column.
    fn paints_when_idle(&self) -> bool {
        !self.overlay
    }
}

/// Scrollbar geometry captured when the thumb is pressed.
///
/// The history, thumb height, and track origin are frozen for the whole drag:
/// recomputing them from the live layout would shift the view under a
/// stationary pointer every time a line arrives or the window resizes.
#[derive(Debug, Clone, Copy, PartialEq)]
struct ScrollbarGrab {
    /// Pointer offset inside the thumb at press time.
    grab: f32,
    track_top: f32,
    track_height: f32,
    thumb_height: f32,
    history: usize,
}

/// Which side of the widget delivered the press that owns the pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PressOwner {
    Grid,
    Scrollbar,
}

/// Where a fresh press landed, independent of whether a bar currently exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PressTarget {
    Grid,
    Track,
    Thumb,
}

/// Hit-test a press against the bar, if any.
///
/// Every press gets an owner even with no bar (`history_size == 0`), so a
/// scrollback that appears mid-drag cannot steal a press that began on the
/// cells and silently drop copy-on-select or the child's mouse-up.
fn press_target(bar: Option<&Scrollbar>, pos: Pos2, primary_pressed: bool) -> PressTarget {
    let Some(bar) = bar.filter(|_| primary_pressed) else {
        return PressTarget::Grid;
    };
    if !bar.track.contains(pos) {
        PressTarget::Grid
    } else if bar.thumb.contains(pos) {
        PressTarget::Thumb
    } else {
        PressTarget::Track
    }
}

/// Whether this view owes the grid a mouse release this frame.
///
/// Only a press this view delivered may produce a release: a release that
/// lands on the cells after a scrollbar drag (or after the tab was hidden
/// during the press) must not reach a mouse-reporting child, or it would see
/// a mouse-up with no matching down.
fn owes_grid_release(owner: Option<PressOwner>, any_pressed: bool) -> bool {
    owner == Some(PressOwner::Grid) || any_pressed
}

/// One drag frame: the offset requested by the pointer and the single delta to
/// hand to `Session::scroll`.
///
/// The end condition is checked first. A grab whose button, pointer, or focus
/// is gone returns `None` before any pointer-derived offset is computed, so a
/// release that happened while this tab was hidden cannot make the view jump
/// when the tab is shown again.
fn scrollbar_drag_frame(
    grab: &ScrollbarGrab,
    current: i32,
    pointer_y: Option<f32>,
    ended: bool,
) -> Option<ScrollbarDragFrame> {
    if ended {
        return None;
    }
    let (target, delta) = apply_scrollbar_drag(grab, current, pointer_y?);
    Some(ScrollbarDragFrame { target, delta })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ScrollbarDragFrame {
    target: i32,
    delta: i32,
}

/// One drag frame against the frozen track origin.
///
/// The caller applies `delta` exactly once and skips the easing path while a
/// drag frame was painted, so a drag can never double-apply through both.
fn apply_scrollbar_drag(grab: &ScrollbarGrab, current: i32, pointer_y: f32) -> (i32, i32) {
    let offset = drag_offset(
        grab.track_height,
        grab.thumb_height,
        grab.history,
        pointer_y - grab.track_top,
        grab.grab,
    );
    (offset, offset - current)
}

/// Track rectangle frozen at press time. Only the horizontal extent comes from
/// the live bar, since it does not affect the vertical mapping.
fn frozen_track_rect(grab: &ScrollbarGrab, live: Rect) -> Rect {
    Rect::from_min_max(
        pos2(live.left(), grab.track_top),
        pos2(live.right(), grab.track_top + grab.track_height),
    )
}

/// Whether the scrollbar, rather than the grid, owns the pointer this frame.
///
/// A press that began on the grid keeps ownership until release, so a
/// drag-select or mouse-report drag that crosses the strip still ends in the
/// grid and its mouse-up is delivered there.
fn scrollbar_owns_pointer(grab_active: bool, grid_press: bool, on_track: bool) -> bool {
    grab_active || (!grid_press && on_track)
}

/// Whether a scrollbar drag must end because its button or pointer is gone,
/// or the viewport lost focus.
///
/// winit has no mouse capture and egui keeps `primary_down` set after
/// `PointerGone`; without this the grab would latch and the bar would keep
/// owning the pointer until the next click.
fn scrollbar_drag_ended(any_down: bool, pointer_pos: Option<Pos2>, focused: bool) -> bool {
    !any_down || pointer_pos.is_none() || !focused
}

/// Whether a track click may page the scrollback. Clicking below the thumb
/// while already at the live bottom has nothing to reveal.
fn track_click_pages(step: i32, display_offset: i32) -> bool {
    step > 0 || display_offset > 0
}

/// Thumb length for `viewport` visible rows above `history` rows of scrollback.
fn scrollbar_thumb_length(track_height: f32, viewport: usize, history: usize) -> f32 {
    if !track_height.is_finite() || track_height <= 0.0 {
        return 0.0;
    }
    if history == 0 {
        return track_height;
    }
    let visible = viewport.max(1) as f32;
    let total = visible + history as f32;
    (track_height * visible / total).clamp(SCROLLBAR_MIN_THUMB.min(track_height), track_height)
}

/// Display offset (0 = live bottom) for a thumb whose top sits `pointer_y`
/// points below the track top, grabbed `grab` points from its own top.
/// Always clamped to both ends, so dragging past them stops there.
fn drag_offset(
    track_height: f32,
    thumb_height: f32,
    history: usize,
    pointer_y: f32,
    grab: f32,
) -> i32 {
    if history == 0 || !track_height.is_finite() || !pointer_y.is_finite() || !grab.is_finite() {
        return 0;
    }
    let max = i32::try_from(history).unwrap_or(i32::MAX);
    let travel = (track_height - thumb_height.max(0.0)).max(0.0);
    if travel <= 0.0 {
        return 0;
    }
    let local = (pointer_y - grab).clamp(0.0, travel);
    let fraction = (1.0 - local / travel).clamp(0.0, 1.0);
    let offset = (fraction * history as f32).round() as i64;
    offset.clamp(0, i64::from(max)) as i32
}

fn paint_scrollbar(
    painter: &egui::Painter,
    bar: &Scrollbar,
    tokens: &Tokens,
    ppp: f32,
    active: bool,
) {
    if bar.thumb.height() <= 0.0 || bar.track.width() <= 0.0 {
        return;
    }
    // The overlay fallback is invisible while idle so it never permanently
    // covers the last column; hover or an active drag reveals it.
    if !active && !bar.paints_when_idle() {
        return;
    }
    let radius = bar.track.width() * 0.5;
    if active {
        // The track only shows while the pointer is on the strip, so text
        // under it is not permanently boxed in.
        painter.rect_filled(bar.track, radius, design::fade(tokens.border, 0.55));
    }
    let opacity = if active { 0.85 } else { 0.35 };
    painter.rect_filled(
        snap_rect(bar.thumb, ppp),
        radius,
        design::fade(tokens.text_dim, opacity),
    );
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalAction {
    Copy(String),
    Paste,
    OpenLink(String),
    RequestSearch,
    Bell,
}

pub struct TerminalResponse {
    pub response: egui::Response,
    pub actions: Vec<TerminalAction>,
    pub cols: usize,
    pub rows: usize,
    /// Whether the view wrote input bytes (mouse reports, alternate-screen
    /// wheel keys) straight to this session during the frame.
    pub input_sent: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchState {
    pub query: String,
    pub case_sensitive: bool,
}

#[derive(Debug, Default)]
pub struct TerminalView {
    /// Display offset target, in lines above the live bottom.
    pub scroll_target: i32,
    pub search: Option<SearchState>,
    pub last_click: Option<(f64, Pos2, u8)>,
    pub ime_pos: Option<Pos2>,
    /// Modes from the last painted snapshot, used for mouse reporting and
    /// wheel encoding between frames. Keyboard/paste encoding takes the live
    /// modes from the session instead.
    modes: TermModes,
    last_layout: Option<(usize, usize, u32, u32)>,
    blink_epoch: f64,
    wheel_accum: f32,
    selecting: bool,
    /// Scrollbar drag state; freezes the thumb/track geometry for the drag.
    scrollbar_grab: Option<ScrollbarGrab>,
    /// Which side delivered the press that owns the pointer. A press that
    /// began on the grid keeps ownership until release, even if the pointer
    /// crosses the scrollbar strip or a bar appears mid-drag.
    press_owner: Option<PressOwner>,
    /// CPU-composited surface and its GPU texture, reused until the grid changes.
    soft_texture: SoftTexture,
    soft_image: SoftImage,
    soft_size: Option<[usize; 2]>,
    soft_key: Option<u64>,
    soft_last_render: f64,
    /// Logical rect the cached surface was composed for. Held during a
    /// debounced resize so the old pixels are painted instead of re-rendering.
    soft_rect: Option<Rect>,
    /// Surface rect frozen for the duration of a drag resize.
    frozen_rect: Option<Rect>,
    soft_texture_name: String,
    /// Grid geometry actually applied to the session; drag resizes are
    /// debounced before replacing it.
    resize: ResizeDebounce,
    /// Rolling frame window for trace-level presentation-rate measurements.
    frame_count: u32,
    frame_elapsed: Duration,
    frame_last: Option<Instant>,
    /// Viewport IME commands are cached; sending them every frame forces egui
    /// to repaint at the display refresh rate even when nothing changed.
    ime_allowed: bool,
    ime_purpose: bool,
    ime_rect: Option<Rect>,
}

impl TerminalView {
    #[allow(clippy::too_many_arguments)]
    pub fn show(
        &mut self,
        ui: &mut Ui,
        session: &Session,
        theme: &Theme,
        config: &Config,
        tokens: &Tokens,
        atlas: &mut GlyphAtlas,
        gpu_text: Option<&Arc<GpuText>>,
        material: Material,
    ) -> TerminalResponse {
        let (response, painter) = ui.allocate_painter(ui.available_size(), Sense::click_and_drag());
        let ppp = ui.ctx().pixels_per_point();
        let rect = soft::snap_rect(response.rect, ppp);
        let metrics = atlas.metrics();
        let padding = config.padding.max(0.0);
        let (target_cols, target_rows) = Self::grid_size(rect, &metrics, padding);
        let first_layout = self.last_layout.is_none();

        if target_cols == 0 || target_rows == 0 || metrics.width <= 0.0 || metrics.height <= 0.0 {
            return TerminalResponse {
                response,
                actions: Vec::new(),
                cols: target_cols,
                rows: target_rows,
                input_sent: false,
            };
        }

        let now = ui.input(|input| input.time);
        let any_input = ui.input(|input| !input.events.is_empty());
        if first_layout || any_input {
            self.blink_epoch = now;
        }

        let layout = (
            target_cols,
            target_rows,
            metrics.width_px,
            metrics.height_px,
        );
        let released = ui.input(|input| input.pointer.any_released());
        let resize_was_pending = self.resize.pending.is_some();
        match self
            .resize
            .poll(layout, self.last_layout, now, first_layout || released)
        {
            ResizePoll::Apply => {
                session.resize(
                    GridSize::new(target_cols, target_rows),
                    (
                        metrics.width_px.min(u32::from(u16::MAX)) as u16,
                        metrics.height_px.min(u32::from(u16::MAX)) as u16,
                    ),
                );
                self.last_layout = Some(layout);
            }
            ResizePoll::Wait(deadline) => {
                ui.ctx()
                    .request_repaint_after(Duration::from_secs_f64((deadline - now).max(0.001)));
            }
            ResizePoll::Idle => {}
        }

        // A debounced resize keeps compositing at the size applied when the
        // drag started, so intermediate sizes never re-render the surface.
        if !resize_was_pending && self.resize.pending.is_some() {
            self.frozen_rect = self.soft_rect.or(Some(rect));
        }
        if self.resize.pending.is_none() {
            self.frozen_rect = None;
        }
        let content_rect = if self.resize.pending.is_some() {
            self.frozen_rect.unwrap_or(rect)
        } else {
            rect
        };

        // While a resize is debounced, paint the snapshot and grid that the
        // session actually has so cells, selection, and search stay aligned.
        let snapshot_started = Instant::now();
        let snapshot = session.snapshot();
        let snapshot_time = snapshot_started.elapsed();
        if snapshot_time.as_millis() > 4 && log::log_enabled!(log::Level::Debug) {
            log::debug!(
                "session snapshot {}x{} took {:?}",
                snapshot.cols,
                snapshot.rows,
                snapshot_time
            );
        }
        self.modes = snapshot.modes;
        let (cols, rows) = (snapshot.cols, snapshot.rows);
        if cols == 0 || rows == 0 {
            return TerminalResponse {
                response,
                actions: Vec::new(),
                cols,
                rows,
                input_sent: false,
            };
        }
        let grid_rect = Rect::from_min_size(
            pos2(content_rect.min.x + padding, content_rect.min.y + padding),
            vec2(metrics.width * cols as f32, metrics.height * rows as f32),
        );

        let origin = grid_rect.min;
        let selection = selection_ranges(&snapshot, cols, rows);
        let row_texts: Vec<String> = (0..rows)
            .map(|row| row_text(&snapshot, row, cols))
            .collect();

        let blinking = config.cursor_blink && snapshot.cursor.blinking;
        let show_cursor =
            snapshot.cursor.visible && (!blinking || blink_phase(now - self.blink_epoch) < 0.53);

        let search = self.search.clone();
        let mut spec = SurfaceSpec {
            cols,
            rows,
            rect: content_rect,
            ppp,
            padding,
            material,
            show_cursor,
            search: search.as_ref(),
            text: true,
            fills: true,
            theme,
            tokens,
            font_size: config.font_size,
            line_height: config.line_height,
        };
        // Pack glyphs and fills before the CPU pass so an atlas failure can
        // turn text back on without drawing the same glyphs twice.
        let gpu_frame = gpu_text
            .filter(|gpu| !gpu.failed())
            .and_then(|gpu| gpu.build_frame(atlas, &snapshot, &spec));
        let gpu_fills = gpu_frame.as_ref().is_some_and(|frame| !frame.is_empty());
        spec.text = gpu_frame.is_none();
        spec.fills = !gpu_fills;
        let soft_painted = self.paint_soft(ui, &painter, atlas, &snapshot, &spec);
        if !soft_painted {
            painter.rect_filled(rect, 0.0, design::color(theme.bg));
            paint_backgrounds(&painter, &snapshot, theme, origin, &metrics, cols, rows);
            paint_selection(&painter, &selection, origin, &metrics, tokens);
            paint_search(
                &painter,
                &row_texts,
                search.as_ref(),
                origin,
                &metrics,
                tokens,
            );
            paint_cells(
                &painter, atlas, &snapshot, theme, origin, &metrics, cols, rows, ppp,
            );
            if show_cursor {
                paint_cursor(
                    &painter, atlas, &snapshot, theme, origin, &metrics, cols, rows, ppp,
                );
            }
        } else if let Some(frame) = gpu_frame
            && !frame.is_empty()
        {
            painter.add(frame.into_shape(content_rect));
        }
        if blinking && snapshot.cursor.visible {
            // Wake up when the cursor needs to flip.
            let phase = blink_phase(now - self.blink_epoch);
            let remaining = if phase < 0.53 {
                0.53 - phase
            } else {
                1.06 - phase
            };
            ui.ctx()
                .request_repaint_after(Duration::from_secs_f64(remaining.max(0.001)));
        }

        let pointer = response
            .interact_pointer_pos()
            .or_else(|| response.hover_pos());
        let mods = ui.input(|input| input.modifiers);
        let ctrl = mods.ctrl || mods.command;
        let mouse_mode = self.modes.mouse_reporting && !mods.shift;

        self.ime_pos = pointer
            .and_then(|pos| Self::cell_at(pos, grid_rect, &metrics, (cols, rows)))
            .map(|(row, col)| cell_rect(origin, &metrics, row, col).min);
        if !self.ime_allowed {
            ui.ctx()
                .send_viewport_cmd(ViewportCommand::IMEAllowed(true));
            self.ime_allowed = true;
        }
        if !self.ime_purpose {
            ui.ctx()
                .send_viewport_cmd(ViewportCommand::IMEPurpose(egui::IMEPurpose::Terminal));
            self.ime_purpose = true;
        }
        if response.has_focus() {
            let ime_rect = self
                .ime_pos
                .map(|pos| Rect::from_min_size(pos, vec2(metrics.width, metrics.height)))
                .unwrap_or_else(|| {
                    cell_rect(origin, &metrics, snapshot.cursor.row, snapshot.cursor.col)
                });
            if self.ime_rect != Some(ime_rect) {
                ui.ctx()
                    .send_viewport_cmd(ViewportCommand::IMERect(ime_rect));
                self.ime_rect = Some(ime_rect);
            }
        }

        let mut actions = Vec::new();

        if response.clicked() || response.drag_started() {
            response.request_focus();
        }

        // Vertical scrollbar laid out in the padding gutter right of the grid,
        // falling back to a narrow overlay on the grid's right edge when the
        // gutter is too narrow. It only exists when the session has
        // scrollback, so the alternate screen and short histories keep the old
        // hit-testing exactly.
        let scrollbar = Scrollbar::for_grid(
            content_rect,
            grid_rect,
            rows,
            snapshot.history_size,
            snapshot.display_offset,
        );
        let history = i32::try_from(snapshot.history_size).unwrap_or(i32::MAX);
        let display_offset = i32::try_from(snapshot.display_offset).unwrap_or(i32::MAX);
        let primary_pressed =
            ui.input(|input| input.pointer.button_pressed(PointerButton::Primary));
        let primary_released =
            ui.input(|input| input.pointer.button_released(PointerButton::Primary));
        let any_pressed = ui.input(|input| input.pointer.any_pressed());
        let pointer_present = ui.input(|input| input.pointer.latest_pos());
        let any_down = ui.input(|input| input.pointer.any_down());
        let focused = ui.ctx().input(|input| input.focused);

        // A grab is only valid while its button is down, and that is checked
        // before any delta is computed. A release that lands while another tab
        // is shown is never observed there, so the stale grab must expire
        // instead of applying this tab's pointer position on return. The
        // release frame itself keeps ownership so its mouse-up is delivered.
        let drag_lost = scrollbar_drag_ended(any_down, pointer_present, focused) && !released;
        if drag_lost {
            self.scrollbar_grab = None;
            self.press_owner = None;
            self.selecting = false;
        }

        // Every fresh press records its owner before hit testing, even with no
        // bar: scrollback can appear mid-drag, and the bar must not steal a
        // press that began on the cells (copy-on-select / mouse-up would be
        // lost).
        if any_pressed && let Some(pos) = pointer {
            match (
                press_target(scrollbar.as_ref(), pos, primary_pressed),
                scrollbar,
            ) {
                (PressTarget::Thumb, Some(bar)) => {
                    // Freeze the geometry for the duration of the drag,
                    // including the track origin so a relayout cannot shift
                    // the inverse mapping under the pointer.
                    self.press_owner = Some(PressOwner::Scrollbar);
                    self.scrollbar_grab = Some(ScrollbarGrab {
                        grab: pos.y - bar.thumb.top(),
                        track_top: bar.track.top(),
                        track_height: bar.track.height(),
                        thumb_height: bar.thumb.height(),
                        history: snapshot.history_size,
                    });
                }
                (PressTarget::Track, Some(bar)) => {
                    // A track click pages toward the pointer, one viewport at
                    // a time. Clicking below the thumb at the live bottom has
                    // nothing to reveal.
                    self.press_owner = Some(PressOwner::Scrollbar);
                    let page = i32::try_from(rows).unwrap_or(i32::MAX);
                    let step = if pos.y < bar.thumb.center().y {
                        page
                    } else {
                        -page
                    };
                    if track_click_pages(step, display_offset) {
                        self.scroll_target =
                            self.scroll_target.saturating_add(step).clamp(0, history);
                    }
                }
                _ => {
                    // Sticky: the grid owns this press until release, so a
                    // selection crossing the strip does not lose the pointer.
                    self.press_owner = Some(PressOwner::Grid);
                }
            }
        }

        let on_track =
            scrollbar.is_some_and(|bar| pointer.is_some_and(|pos| bar.track.contains(pos)));
        let mut bar_hovered = false;
        let mut bar_dragging = false;
        // Offset the drag applied this frame; the thumb is painted from it
        // instead of the stale frame-start snapshot.
        let mut painted_offset = None;
        let mut painted_grab = None;
        if scrollbar.is_some() {
            bar_hovered = on_track;
            if let Some(grab) = self.scrollbar_grab {
                let ended =
                    primary_released || scrollbar_drag_ended(any_down, pointer_present, focused);
                match scrollbar_drag_frame(&grab, display_offset, pointer.map(|pos| pos.y), ended) {
                    Some(frame) => {
                        bar_dragging = true;
                        self.scroll_target = frame.target;
                        painted_offset = Some(frame.target);
                        painted_grab = Some(grab);
                        // Apply the full delta exactly once here; the easing
                        // path is skipped for a drag frame.
                        if frame.delta != 0 {
                            session.scroll(frame.delta);
                            ui.ctx().request_repaint();
                        }
                    }
                    // Released outside the window, focus lost, or the button
                    // came up: drop the grab before any delta is applied.
                    None => self.scrollbar_grab = None,
                }
            }
            if bar_hovered || bar_dragging {
                ui.ctx().set_cursor_icon(if bar_dragging {
                    CursorIcon::Grabbing
                } else {
                    CursorIcon::Grab
                });
            }
        } else {
            self.scrollbar_grab = None;
        }
        // The strip claims pointer input only inside its own track, plus the
        // rest of a drag that began there. A press that began on the grid keeps
        // ownership until release.
        let grid_press = self.press_owner == Some(PressOwner::Grid);
        let bar_owns_pointer =
            scrollbar_owns_pointer(self.scrollbar_grab.is_some(), grid_press, on_track);
        if let Some(bar) = scrollbar {
            let painted = match (painted_offset, painted_grab) {
                (Some(offset), Some(grab)) => Scrollbar::with_thumb_height(
                    frozen_track_rect(&grab, bar.track),
                    grab.thumb_height,
                    grab.history,
                    usize::try_from(offset.max(0)).unwrap_or(0),
                    bar.overlay,
                ),
                _ => bar,
            };
            paint_scrollbar(&painter, &painted, tokens, ppp, bar_hovered || bar_dragging);
        }

        if let Some(pos) = pointer
            && !bar_owns_pointer
        {
            let hover_cell = Self::cell_at(pos, grid_rect, &metrics, (cols, rows));
            let hover_link = if ctrl {
                hover_cell.and_then(|(row, col)| {
                    let text = row_texts.get(row)?;
                    link_at(session, &snapshot, text, row, col)
                })
            } else {
                None
            };
            if ctrl && !mouse_mode && hover_link.is_some() {
                ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
            }

            if !mouse_mode
                && response.clicked_by(PointerButton::Primary)
                && ctrl
                && let Some(url) = hover_link
            {
                actions.push(TerminalAction::OpenLink(url));
            }
        }

        let mut input_sent = false;
        let wheel = wheel_notches(ui, rows);
        if !ctrl && response.hovered() && wheel != 0.0 {
            self.wheel_accum += wheel;
            let whole = self.wheel_accum.trunc();
            self.wheel_accum -= whole;
            let notches = whole as i32;
            if notches != 0 && self.modes.mouse_reporting && !mods.shift {
                let (row, col) = pointer
                    .and_then(|pos| Self::cell_at_clamped(pos, grid_rect, &metrics, (cols, rows)))
                    .unwrap_or((0, 0));
                let mouse_mods = Mods {
                    ctrl: mods.ctrl,
                    alt: mods.alt,
                    shift: mods.shift,
                };
                for report in wheel_reports(notches, col, row, mouse_mods, self.modes.sgr_mouse) {
                    session.write(&report);
                }
                input_sent = true;
            } else if notches != 0 && self.modes.alternate_screen && !self.modes.mouse_reporting {
                let count = notches.unsigned_abs().saturating_mul(3);
                let key = if notches > 0 { Key::Up } else { Key::Down };
                let bytes = encode_key(
                    KeyInput::new(key, Mods::default(), None),
                    KeyModes {
                        app_cursor: self.modes.app_cursor,
                        bracketed_paste: self.modes.bracketed_paste,
                    },
                );
                for _ in 0..count {
                    session.write(&bytes);
                }
                input_sent = true;
            } else if notches != 0 {
                self.scroll_target = apply_wheel(self.scroll_target, notches);
                self.scroll_target = self
                    .scroll_target
                    .clamp(0, i32::try_from(snapshot.history_size).unwrap_or(i32::MAX));
            }
        }

        if let Some(pos) = pointer {
            if !bar_owns_pointer && owes_grid_release(self.press_owner, any_pressed) {
                input_sent |= self.handle_mouse(
                    ui,
                    &response,
                    session,
                    &snapshot,
                    &row_texts,
                    grid_rect,
                    &metrics,
                    (cols, rows),
                    pos,
                    &mods,
                    config.copy_on_select,
                    &mut actions,
                );
            } else {
                self.selecting = false;
            }
        }

        if self.selecting
            && response.dragged_by(PointerButton::Primary)
            && let Some(pos) = response.interact_pointer_pos()
            && !grid_rect.contains(pos)
        {
            let delta = selection_autoscroll_delta(pos.y, grid_rect.top(), grid_rect.bottom());
            if delta != 0 {
                self.scroll_target = apply_wheel(self.scroll_target, delta);
            }
            ui.ctx()
                .request_repaint_after(Duration::from_secs_f64(f64::from(frame_interval(ui))));
        }

        // Release ends the press's sticky ownership, whether it arrived inside
        // the window or was lost outside it. It stays set until after
        // `handle_mouse` above, which is what delivers the matching mouse-up.
        if released || scrollbar_drag_ended(any_down, pointer_present, focused) {
            self.press_owner = None;
        }

        // A scrollbar drag applies its own full delta above; easing it again in
        // the same frame would move twice and oscillate against the pointer.
        if self.scrollbar_grab.is_none() && painted_offset.is_none() {
            self.animate_scroll(ui, session, &snapshot);
        }
        self.record_frame();

        TerminalResponse {
            response,
            actions,
            cols,
            rows,
            input_sent,
        }
    }

    /// Emits one trace-level presentation-rate sample per 60 consecutive
    /// frames. Gaps longer than 100 ms (idle) restart the window.
    fn record_frame(&mut self) {
        let now = Instant::now();
        if let Some(last) = self.frame_last {
            let delta = now.saturating_duration_since(last);
            if delta < Duration::from_millis(100) {
                self.frame_elapsed += delta;
                self.frame_count += 1;
                if self.frame_count >= 60 {
                    let seconds = self.frame_elapsed.as_secs_f64();
                    if seconds > 0.0 && log::log_enabled!(log::Level::Trace) {
                        log::trace!(
                            "terminal presented 60 frames in {seconds:.3}s ({:.1} fps)",
                            60.0 / seconds
                        );
                    }
                    self.frame_count = 0;
                    self.frame_elapsed = Duration::ZERO;
                }
            } else {
                self.frame_count = 0;
                self.frame_elapsed = Duration::ZERO;
            }
        }
        self.frame_last = Some(now);
    }

    fn animate_scroll(&mut self, ui: &Ui, session: &Session, snapshot: &Snapshot) {
        let history = i32::try_from(snapshot.history_size).unwrap_or(i32::MAX);
        self.scroll_target = self.scroll_target.clamp(0, history);
        let current = i32::try_from(snapshot.display_offset).unwrap_or(i32::MAX);
        let diff = self.scroll_target - current;
        if diff != 0 {
            let step = (diff.abs() / 3).max(1) * diff.signum();
            session.scroll(step);
            ui.ctx().request_repaint();
        }
    }

    /// Paints the cached software surface, re-rendering only when its
    /// fingerprint changes. Returns `false` when the mesh fallback should run.
    fn paint_soft(
        &mut self,
        ui: &Ui,
        painter: &egui::Painter,
        atlas: &mut GlyphAtlas,
        snapshot: &Snapshot,
        spec: &SurfaceSpec<'_>,
    ) -> bool {
        let key = soft::fingerprint(snapshot, spec);
        let size = soft::surface_size(spec.rect, spec.ppp);
        let needs_render =
            self.soft_key != Some(key) || self.soft_size != size || self.soft_texture.0.is_none();
        if needs_render {
            let now = ui.ctx().input(|input| input.time);
            let frame_dt = f64::from(frame_interval(ui));
            // Defer only inside half a refresh; rendering on every changed
            // fingerprint lets vsync pace the app at 75 Hz.
            if self.soft_texture.0.is_some()
                && let Some(delay) = coalesce_delay(now - self.soft_last_render, frame_dt)
            {
                ui.ctx()
                    .request_repaint_after(Duration::from_secs_f64(delay));
            } else {
                let started = Instant::now();
                match soft::render_surface(atlas, snapshot, spec) {
                    Some(image) => {
                        let render_time = started.elapsed();
                        self.soft_key = Some(key);
                        self.soft_size = Some(image.size);
                        self.soft_rect = Some(spec.rect);
                        self.soft_last_render = now;
                        let stats = self.upload_soft(ui.ctx(), image);
                        if log::log_enabled!(log::Level::Debug) {
                            log::debug!(
                                "soft surface {}x{} render {:?} upload {:?} bands {} bytes {}",
                                self.soft_size.map_or(0, |size| size[0]),
                                self.soft_size.map_or(0, |size| size[1]),
                                render_time,
                                stats.elapsed,
                                stats.bands,
                                stats.bytes
                            );
                        }
                    }
                    None => {
                        self.soft_key = None;
                        self.soft_size = None;
                        self.soft_rect = None;
                        self.soft_texture.0 = None;
                        self.soft_image = SoftImage(None);
                    }
                }
            }
        }

        let Some(texture) = self.soft_texture.0.as_ref() else {
            return false;
        };
        painter.image(
            texture.id(),
            spec.rect,
            Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
            Color32::WHITE,
        );
        true
    }

    /// Uploads only the row bands that changed since the last surface.
    fn upload_soft(&mut self, ctx: &egui::Context, image: ColorImage) -> UploadStats {
        if self.soft_texture_name.is_empty() {
            self.soft_texture_name = format!(
                "illness-terminal-soft-{}",
                NEXT_SOFT_TEXTURE.fetch_add(1, Ordering::Relaxed)
            );
        }
        let width = image.size[0];
        let height = image.size[1];
        let bands = match self.soft_image.0.as_ref() {
            Some(previous) if previous.size == image.size => soft::dirty_bands(previous, &image),
            _ => vec![(0, height)],
        };
        let bytes = bands
            .iter()
            .map(|(_, rows)| rows.saturating_mul(width).saturating_mul(4))
            .sum();
        let full = bands.len() == 1 && bands[0] == (0, height);
        let started = Instant::now();

        if let Some(texture) = self.soft_texture.0.as_mut() {
            if full {
                texture.set(image.clone(), TextureOptions::NEAREST);
            } else {
                for &(row, rows) in &bands {
                    if rows == 0 {
                        continue;
                    }
                    let start = row * width;
                    let end = start + rows * width;
                    texture.set_partial(
                        [0, row],
                        ColorImage::new([width, rows], image.pixels[start..end].to_vec()),
                        TextureOptions::NEAREST,
                    );
                }
            }
        } else {
            self.soft_texture.0 = Some(ctx.load_texture(
                self.soft_texture_name.clone(),
                image.clone(),
                TextureOptions::NEAREST,
            ));
        }

        let elapsed = started.elapsed();
        if log::log_enabled!(log::Level::Debug) {
            log::debug!("soft upload bands {bands:?}");
        }
        self.soft_image = SoftImage(Some(image));
        UploadStats {
            bands: bands.len(),
            bytes,
            elapsed,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_mouse(
        &mut self,
        ui: &Ui,
        response: &egui::Response,
        session: &Session,
        snapshot: &Snapshot,
        row_texts: &[String],
        grid_rect: Rect,
        metrics: &CellMetrics,
        grid: (usize, usize),
        pos: Pos2,
        mods: &Modifiers,
        copy_on_select: bool,
        actions: &mut Vec<TerminalAction>,
    ) -> bool {
        let cell = Self::cell_at(pos, grid_rect, metrics, grid);
        let mouse_mode = self.modes.mouse_reporting && !mods.shift;
        let mouse_mods = Mods {
            ctrl: mods.ctrl,
            alt: mods.alt,
            shift: mods.shift,
        };
        let mut wrote = false;

        if mouse_mode {
            // `encode_mouse` takes zero-based coordinates and emits xterm's
            // one-based `col;row` payload.
            let Some((row, col)) = Self::cell_at_clamped(pos, grid_rect, metrics, grid) else {
                return false;
            };
            let sgr = self.modes.sgr_mouse;
            if ui.input(|input| input.pointer.button_pressed(PointerButton::Primary)) {
                session.write(&encode_mouse(
                    MousePhase::Press,
                    MouseButton::Left,
                    col,
                    row,
                    mouse_mods,
                    sgr,
                ));
                wrote = true;
            }
            if ui.input(|input| input.pointer.button_pressed(PointerButton::Middle)) {
                session.write(&encode_mouse(
                    MousePhase::Press,
                    MouseButton::Middle,
                    col,
                    row,
                    mouse_mods,
                    sgr,
                ));
                wrote = true;
            }
            if ui.input(|input| input.pointer.button_pressed(PointerButton::Secondary)) {
                session.write(&encode_mouse(
                    MousePhase::Press,
                    MouseButton::Right,
                    col,
                    row,
                    mouse_mods,
                    sgr,
                ));
                wrote = true;
            }
            if ui.input(|input| input.pointer.button_released(PointerButton::Primary)) {
                session.write(&encode_mouse(
                    MousePhase::Release,
                    MouseButton::Left,
                    col,
                    row,
                    mouse_mods,
                    sgr,
                ));
                wrote = true;
            }
            if ui.input(|input| input.pointer.button_released(PointerButton::Middle)) {
                session.write(&encode_mouse(
                    MousePhase::Release,
                    MouseButton::Middle,
                    col,
                    row,
                    mouse_mods,
                    sgr,
                ));
                wrote = true;
            }
            if ui.input(|input| input.pointer.button_released(PointerButton::Secondary)) {
                session.write(&encode_mouse(
                    MousePhase::Release,
                    MouseButton::Right,
                    col,
                    row,
                    mouse_mods,
                    sgr,
                ));
                wrote = true;
            }
            if response.dragged_by(PointerButton::Primary) {
                session.write(&encode_mouse(
                    MousePhase::Motion,
                    MouseButton::Left,
                    col,
                    row,
                    mouse_mods,
                    sgr,
                ));
                wrote = true;
            }
            self.selecting = false;
            return wrote;
        }

        let primary_pressed =
            ui.input(|input| input.pointer.button_pressed(PointerButton::Primary));
        if primary_pressed {
            let ctrl = mods.ctrl || mods.command;
            let link = cell.and_then(|(row, col)| {
                let text = row_texts.get(row)?;
                link_at(session, snapshot, text, row, col)
            });
            if ctrl && link.is_some() {
                self.selecting = false;
                return false;
            }
            if let Some((row, col)) = cell {
                let now = ui.input(|input| input.time);
                let count = self.click_count(now, pos);
                self.last_click = Some((now, pos, count));
                let kind = match count {
                    1 => SelectionType::Simple,
                    2 => SelectionType::Semantic,
                    _ => SelectionType::Lines,
                };
                session.selection_clear();
                session.selection_start(kind, col, row, Side::Left);
                self.selecting = true;
            }
        }

        if self.selecting
            && response.dragged_by(PointerButton::Primary)
            && let Some(pos) = response.interact_pointer_pos()
            && let Some((row, col)) = Self::cell_at_clamped(pos, grid_rect, metrics, grid)
        {
            let cell_rect = cell_rect(grid_rect.min, metrics, row, col);
            let side = side_at(pos, cell_rect, metrics);
            session.selection_update(col, row, side);
        }

        let primary_released =
            ui.input(|input| input.pointer.button_released(PointerButton::Primary));
        if self.selecting && primary_released {
            if let Some(pos) = response.interact_pointer_pos()
                && let Some((row, col)) = Self::cell_at_clamped(pos, grid_rect, metrics, grid)
            {
                let cell_rect = cell_rect(grid_rect.min, metrics, row, col);
                session.selection_update(col, row, side_at(pos, cell_rect, metrics));
            }
            self.selecting = false;
            if let Some(text) = session.selection_to_string() {
                let text = trim_selection(&text);
                if copy_on_select && !text.is_empty() {
                    actions.push(TerminalAction::Copy(text));
                }
            }
        }
        false
    }

    fn click_count(&mut self, now: f64, pos: Pos2) -> u8 {
        match self.last_click {
            Some((time, last_pos, count)) if now - time <= 0.4 && last_pos.distance(pos) <= 4.0 => {
                count.saturating_add(1).min(3)
            }
            _ => 1,
        }
    }

    /// Whether the child asked for mouse reports in the most recent frame.
    pub fn mouse_reporting(&self) -> bool {
        self.modes.mouse_reporting
    }

    /// Encode keyboard, text, IME, and paste events into PTY bytes.
    ///
    /// `modes` must be read from the live session in the same frame
    /// ([`Session::term_modes`]): the app encodes input before it builds the
    /// snapshot, so a mode change in this frame is already visible here. The app
    /// filters its global shortcuts first and calls this only when no overlay
    /// wants the keyboard.
    pub fn encode_input(
        &self,
        events: &[Event],
        modifiers: Modifiers,
        modes: TermModes,
    ) -> Option<Vec<u8>> {
        let has_text = events
            .iter()
            .any(|event| matches!(event, Event::Text(_) | Event::Ime(ImeEvent::Commit(_))));
        let modes = KeyModes {
            app_cursor: modes.app_cursor,
            bracketed_paste: modes.bracketed_paste,
        };
        let mut bytes = Vec::new();

        for event in events {
            match event {
                Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } => {
                    let mods = key_mods(modifiers);
                    if let Some(key) = map_key(*key) {
                        bytes.extend_from_slice(&encode_key(KeyInput::new(key, mods, None), modes));
                    } else if mods.ctrl
                        && (!mods.alt || !has_text)
                        && let Some(ch) = printable_char(*key)
                    {
                        // Ctrl+Alt is AltGr on many layouts. When it produces a
                        // text event, that event carries the composed
                        // character and encoding the physical key here would
                        // double it.
                        bytes.extend_from_slice(&encode_key(
                            KeyInput::new(Key::Char(ch), mods, None),
                            modes,
                        ));
                    } else if !has_text
                        && !mods.ctrl
                        && !mods.alt
                        && let Some(ch) = printable_char(*key)
                    {
                        bytes.extend_from_slice(&encode_key(
                            KeyInput::new(Key::Char(ch), mods, None),
                            modes,
                        ));
                    }
                }
                Event::Text(text) => {
                    if modifiers.ctrl && !modifiers.alt {
                        continue;
                    }
                    if modifiers.alt && !modifiers.ctrl {
                        for ch in text.chars() {
                            bytes.extend_from_slice(&encode_key(
                                KeyInput::new(Key::Char(ch), key_mods(&modifiers), None),
                                modes,
                            ));
                        }
                    } else {
                        bytes.extend_from_slice(text.as_bytes());
                    }
                }
                Event::Ime(ImeEvent::Commit(text)) => {
                    bytes.extend_from_slice(text.as_bytes());
                }
                Event::Paste(text) => {
                    bytes.extend_from_slice(&encode_paste(text, modes.bracketed_paste));
                }
                _ => {}
            }
        }

        if bytes.is_empty() { None } else { Some(bytes) }
    }

    /// Compute the grid size that fits inside `rect`, accounting for padding.
    pub fn grid_size(rect: Rect, metrics: &CellMetrics, pad: f32) -> (usize, usize) {
        let pad = pad.max(0.0);
        let width = (rect.width() - pad * 2.0).max(0.0);
        let height = (rect.height() - pad * 2.0).max(0.0);
        let cols = if metrics.width.is_finite() && metrics.width > 0.0 {
            (width / metrics.width).floor() as usize
        } else {
            1
        };
        let rows = if metrics.height.is_finite() && metrics.height > 0.0 {
            (height / metrics.height).floor() as usize
        } else {
            1
        };
        (cols.max(1), rows.max(1))
    }

    /// Map a pointer position inside the padded grid rectangle to `(row, col)`.
    /// Positions on the right or bottom edge clamp to the last cell.
    pub fn cell_at(
        pos: Pos2,
        rect: Rect,
        metrics: &CellMetrics,
        grid: (usize, usize),
    ) -> Option<(usize, usize)> {
        let (cols, rows) = grid;
        if cols == 0
            || rows == 0
            || !rect.contains(pos)
            || !metrics.width.is_finite()
            || !metrics.height.is_finite()
            || metrics.width <= 0.0
            || metrics.height <= 0.0
        {
            return None;
        }
        let local = pos - rect.min;
        let col = ((local.x / metrics.width).floor() as usize).min(cols - 1);
        let row = ((local.y / metrics.height).floor() as usize).min(rows - 1);
        Some((row, col))
    }

    fn cell_at_clamped(
        pos: Pos2,
        rect: Rect,
        metrics: &CellMetrics,
        grid: (usize, usize),
    ) -> Option<(usize, usize)> {
        let clamped = pos.clamp(rect.min, rect.max);
        Self::cell_at(clamped, rect, metrics, grid)
    }
}

/// Accumulate wheel notches into a scroll target. One notch is three lines.
pub fn apply_wheel(current: i32, notches: i32) -> i32 {
    current.saturating_add(notches.saturating_mul(3)).max(0)
}

/// Delta that drag-autoscroll applies when the pointer leaves the grid.
/// Positive scrolls into history (up); negative scrolls toward newer output.
pub fn selection_autoscroll_delta(pos_y: f32, top: f32, bottom: f32) -> i32 {
    if pos_y < top {
        1
    } else if pos_y > bottom {
        -1
    } else {
        0
    }
}

/// Encode one wheel report per notch, in the order the wheel moved.
pub fn wheel_reports(notches: i32, col: usize, row: usize, mods: Mods, sgr: bool) -> Vec<Vec<u8>> {
    if notches == 0 {
        return Vec::new();
    }
    let button = if notches > 0 {
        MouseButton::WheelUp
    } else {
        MouseButton::WheelDown
    };
    (0..notches.unsigned_abs())
        .map(|_| encode_mouse(MousePhase::Press, button, col, row, mods, sgr))
        .collect()
}

/// Convert selection spans into one rectangle per covered row.
pub fn selection_rects(
    spans: &[SelectionSpan],
    grid: (usize, usize),
    rect: Rect,
    metrics: &CellMetrics,
) -> Vec<Rect> {
    let (cols, rows) = grid;
    if cols == 0
        || rows == 0
        || !metrics.width.is_finite()
        || !metrics.height.is_finite()
        || metrics.width <= 0.0
        || metrics.height <= 0.0
    {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (row, start, end) in selection_ranges_from_spans(spans, cols, rows) {
        let origin = cell_rect(rect.min, metrics, row, start).min;
        let size = vec2(metrics.width * (end - start) as f32, metrics.height);
        out.push(Rect::from_min_size(origin, size));
    }
    out
}

/// Find the first `http://` or `https://` URL in a line of text.
pub fn find_url(text: &str) -> Option<&str> {
    let (start, end) = *url_ranges(text).first()?;
    let start_byte = text.char_indices().nth(start).map(|(idx, _)| idx)?;
    let end_byte = text
        .char_indices()
        .nth(end)
        .map(|(idx, _)| idx)
        .unwrap_or(text.len());
    text.get(start_byte..end_byte)
}

fn wheel_notches(ui: &Ui, rows: usize) -> f32 {
    ui.input(|input| {
        let mut notches = 0.0;
        for event in &input.events {
            if let Event::MouseWheel {
                unit,
                delta,
                modifiers,
                ..
            } = event
            {
                if modifiers.ctrl || modifiers.command {
                    continue;
                }
                notches += match unit {
                    MouseWheelUnit::Line => delta.y,
                    MouseWheelUnit::Point => delta.y / 50.0,
                    MouseWheelUnit::Page => delta.y * rows as f32 / 3.0,
                };
            }
        }
        notches
    })
}

fn blink_phase(elapsed: f64) -> f64 {
    if elapsed.is_finite() {
        elapsed.rem_euclid(1.06)
    } else {
        0.0
    }
}

fn paint_backgrounds(
    painter: &egui::Painter,
    snapshot: &Snapshot,
    theme: &Theme,
    origin: Pos2,
    metrics: &CellMetrics,
    cols: usize,
    rows: usize,
) {
    let default_bg = design::color(theme.bg);
    for row in 0..rows {
        let mut run_start = 0;
        let mut run_color = background_at(snapshot, theme, row, 0);
        for col in 1..=cols {
            let color = if col == cols {
                None
            } else {
                Some(background_at(snapshot, theme, row, col))
            };
            if color != Some(run_color) {
                if run_color != default_bg {
                    let start = cell_rect(origin, metrics, row, run_start).min;
                    let width = metrics.width * (col - run_start) as f32;
                    painter.rect_filled(
                        Rect::from_min_size(start, vec2(width, metrics.height)),
                        0.0,
                        run_color,
                    );
                }
                run_start = col;
                if let Some(color) = color {
                    run_color = color;
                }
            }
        }
    }
}

fn paint_selection(
    painter: &egui::Painter,
    ranges: &[(usize, usize, usize)],
    origin: Pos2,
    metrics: &CellMetrics,
    tokens: &Tokens,
) {
    for &(row, start, end) in ranges {
        if start >= end {
            continue;
        }
        let rect = Rect::from_min_size(
            cell_rect(origin, metrics, row, start).min,
            vec2(metrics.width * (end - start) as f32, metrics.height),
        );
        painter.rect_filled(rect, 0.0, tokens.selection);
    }
}

fn paint_search(
    painter: &egui::Painter,
    row_texts: &[String],
    search: Option<&SearchState>,
    origin: Pos2,
    metrics: &CellMetrics,
    tokens: &Tokens,
) {
    let Some(search) = search else {
        return;
    };
    if search.query.is_empty() {
        return;
    }
    let highlight = Color32::from_rgba_unmultiplied(
        tokens.accent.r(),
        tokens.accent.g(),
        tokens.accent.b(),
        96,
    );
    for (row, text) in row_texts.iter().enumerate() {
        for (start, end) in find_matches(text, &search.query, search.case_sensitive) {
            let rect = Rect::from_min_size(
                cell_rect(origin, metrics, row, start).min,
                vec2(metrics.width * (end - start) as f32, metrics.height),
            );
            painter.rect_filled(rect, 0.0, highlight);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_cells(
    painter: &egui::Painter,
    atlas: &mut GlyphAtlas,
    snapshot: &Snapshot,
    theme: &Theme,
    origin: Pos2,
    metrics: &CellMetrics,
    cols: usize,
    rows: usize,
    ppp: f32,
) {
    for row in 0..rows {
        for col in 0..cols {
            let Some(cell) = snapshot.cell(col, row) else {
                continue;
            };
            let flags = cell.flags;
            if flags.intersects(
                CellFlags::HIDDEN
                    | CellFlags::WIDE_CHAR_SPACER
                    | CellFlags::LEADING_WIDE_CHAR_SPACER,
            ) {
                continue;
            }
            let has_decoration = flags.intersects(CellFlags::ALL_UNDERLINES | CellFlags::STRIKEOUT);
            if cell.ch == ' ' && !has_decoration {
                continue;
            }
            let (fg, _) = cell_colors(cell, theme);
            let fg = if flags.contains(CellFlags::DIM) {
                dim(fg)
            } else {
                fg
            };
            let style = glyph_style(flags);
            let mut rect = cell_rect(origin, metrics, row, col);
            if flags.contains(CellFlags::WIDE_CHAR) {
                rect.set_width(metrics.width * 2.0);
            }
            if let Some(glyph) = atlas.glyph(cell.ch, style) {
                atlas.paint_glyph(painter, rect.min, &glyph, fg);
            }
            for &mark in snapshot.zero_width(cell) {
                if let Some(glyph) = atlas.glyph(mark, style) {
                    atlas.paint_glyph(painter, rect.min, &glyph, fg);
                }
            }
            if flags.intersects(CellFlags::ALL_UNDERLINES) {
                paint_line(painter, rect, ppp, fg, false);
            }
            if flags.contains(CellFlags::STRIKEOUT) {
                paint_line(painter, rect, ppp, fg, true);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_cursor(
    painter: &egui::Painter,
    atlas: &mut GlyphAtlas,
    snapshot: &Snapshot,
    theme: &Theme,
    origin: Pos2,
    metrics: &CellMetrics,
    cols: usize,
    rows: usize,
    ppp: f32,
) {
    let mut col = snapshot.cursor.col.min(cols.saturating_sub(1));
    let row = snapshot.cursor.row.min(rows.saturating_sub(1));
    if let Some(cell) = snapshot.cell(col, row)
        && cell.flags.contains(CellFlags::WIDE_CHAR_SPACER)
        && col > 0
    {
        col -= 1;
    }
    let mut rect = cell_rect(origin, metrics, row, col);
    if let Some(cell) = snapshot.cell(col, row)
        && cell.flags.contains(CellFlags::WIDE_CHAR)
    {
        rect.set_width(metrics.width * 2.0);
    }
    rect = snap_rect(rect, ppp);
    let color = design::color(theme.cursor);
    match snapshot.cursor.kind {
        CursorKind::Block => {
            painter.rect_filled(rect, 0.0, color);
            if let Some(cell) = snapshot.cell(col, row) {
                let flags = cell.flags;
                if !flags.intersects(
                    CellFlags::HIDDEN
                        | CellFlags::WIDE_CHAR_SPACER
                        | CellFlags::LEADING_WIDE_CHAR_SPACER,
                ) && cell.ch != ' '
                    && let Some(glyph) = atlas.glyph(cell.ch, glyph_style(flags))
                {
                    atlas.paint_glyph(painter, rect.min, &glyph, design::color(theme.bg));
                    for &mark in snapshot.zero_width(cell) {
                        if let Some(glyph) = atlas.glyph(mark, glyph_style(flags)) {
                            atlas.paint_glyph(painter, rect.min, &glyph, design::color(theme.bg));
                        }
                    }
                }
            }
        }
        CursorKind::Beam => {
            let width = (2.0 / ppp).max(1.0 / ppp);
            painter.rect_filled(
                Rect::from_min_size(rect.min, vec2(width, rect.height())),
                0.0,
                color,
            );
        }
        CursorKind::Underline => {
            let height = (2.0 / ppp).max(1.0 / ppp);
            painter.rect_filled(
                Rect::from_min_size(
                    pos2(rect.left(), rect.bottom() - height),
                    vec2(rect.width(), height),
                ),
                0.0,
                color,
            );
        }
    }
}

pub(crate) fn cell_colors(cell: &SnapshotCell, theme: &Theme) -> (Color32, Color32) {
    let mut fg = resolve_color(cell.fg, theme);
    let mut bg = resolve_color(cell.bg, theme);
    if cell.flags.contains(CellFlags::INVERSE) {
        std::mem::swap(&mut fg, &mut bg);
    }
    (fg, bg)
}

fn background_at(snapshot: &Snapshot, theme: &Theme, row: usize, col: usize) -> Color32 {
    snapshot
        .cell(col, row)
        .map(|cell| cell_colors(cell, theme).1)
        .unwrap_or_else(|| design::color(theme.bg))
}

fn resolve_color(color: Color, theme: &Theme) -> Color32 {
    match color {
        Color::Spec(rgb) => Color32::from_rgb(rgb.r, rgb.g, rgb.b),
        Color::Indexed(index) => indexed_color(index, theme),
        Color::Named(named) => resolve_named(named as u16, theme),
    }
}

/// xterm-256 palette: 0-15 theme ANSI, 16-231 the 6x6x6 cube, 232-255 grays.
fn indexed_color(index: u8, theme: &Theme) -> Color32 {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    match index {
        0..=15 => design::color(theme.ansi[index as usize]),
        16..=231 => {
            let offset = index - 16;
            let red = offset / 36;
            let green = (offset % 36) / 6;
            let blue = offset % 6;
            Color32::from_rgb(
                LEVELS[red as usize],
                LEVELS[green as usize],
                LEVELS[blue as usize],
            )
        }
        _ => {
            let gray = 8u8.saturating_add(10u8.saturating_mul(index - 232));
            Color32::from_rgb(gray, gray, gray)
        }
    }
}

/// Map a named color discriminant onto the active theme.
///
/// The numeric values are the stable `NamedColor` discriminants: 0-15 ANSI,
/// 256-258 foreground/background/cursor, 259-266 the dim variants, and
/// 267/276 the bright/dim foreground aliases.
fn resolve_named(index: u16, theme: &Theme) -> Color32 {
    match index {
        0..=15 => design::color(theme.ansi[index as usize]),
        256 | 267 | 276 => design::color(theme.fg),
        257 => design::color(theme.bg),
        258 => design::color(theme.cursor),
        259..=266 => blend_toward(
            design::color(theme.ansi[(index - 259) as usize]),
            design::color(theme.bg),
            0.6,
        ),
        268..=275 => design::color(theme.ansi[(index - 268 + 8) as usize]),
        _ => design::color(theme.fg),
    }
}

fn blend_toward(color: Color32, target: Color32, amount: f32) -> Color32 {
    let amount = amount.clamp(0.0, 1.0);
    let mix = |from: u8, to: u8| {
        (f32::from(from) * (1.0 - amount) + f32::from(to) * amount).round() as u8
    };
    Color32::from_rgb(
        mix(color.r(), target.r()),
        mix(color.g(), target.g()),
        mix(color.b(), target.b()),
    )
}

pub(crate) fn glyph_style(flags: CellFlags) -> GlyphStyle {
    match (
        flags.contains(CellFlags::BOLD),
        flags.contains(CellFlags::ITALIC),
    ) {
        (true, true) => GlyphStyle::BoldItalic,
        (true, false) => GlyphStyle::Bold,
        (false, true) => GlyphStyle::Italic,
        (false, false) => GlyphStyle::Regular,
    }
}

pub(crate) fn dim(color: Color32) -> Color32 {
    Color32::from_rgb(
        (f32::from(color.r()) * 0.6) as u8,
        (f32::from(color.g()) * 0.6) as u8,
        (f32::from(color.b()) * 0.6) as u8,
    )
}

fn paint_line(painter: &egui::Painter, rect: Rect, ppp: f32, color: Color32, strike: bool) {
    let thickness = 1.0 / ppp;
    let x0 = snap(rect.left(), ppp);
    let x1 = snap(rect.right(), ppp).max(x0 + thickness);
    let y = if strike {
        snap(rect.center().y, ppp)
    } else {
        snap(rect.bottom() - thickness, ppp)
    };
    painter.rect_filled(
        Rect::from_min_max(pos2(x0, y), pos2(x1, y + thickness)),
        0.0,
        color,
    );
}

fn snap(value: f32, ppp: f32) -> f32 {
    if ppp > 0.0 {
        (value * ppp).round() / ppp
    } else {
        value
    }
}

fn snap_rect(rect: Rect, ppp: f32) -> Rect {
    Rect::from_min_max(
        pos2(snap(rect.min.x, ppp), snap(rect.min.y, ppp)),
        pos2(snap(rect.max.x, ppp), snap(rect.max.y, ppp)),
    )
}

fn cell_rect(origin: Pos2, metrics: &CellMetrics, row: usize, col: usize) -> Rect {
    Rect::from_min_size(
        pos2(
            origin.x + metrics.width * col as f32,
            origin.y + metrics.height * row as f32,
        ),
        vec2(metrics.width, metrics.height),
    )
}

fn side_at(pos: Pos2, rect: Rect, metrics: &CellMetrics) -> Side {
    if pos.x - rect.left() > metrics.width * 0.5 {
        Side::Right
    } else {
        Side::Left
    }
}

pub(crate) fn selection_ranges(
    snapshot: &Snapshot,
    cols: usize,
    rows: usize,
) -> Vec<(usize, usize, usize)> {
    let mut ranges = selection_ranges_from_spans(&snapshot.selection, cols, rows);
    for (row, start, end) in &mut ranges {
        if *start > 0
            && snapshot
                .cell(*start, *row)
                .is_some_and(|cell| cell.flags.contains(CellFlags::WIDE_CHAR_SPACER))
        {
            *start -= 1;
        }
        if *end < cols
            && *end > 0
            && snapshot
                .cell(*end - 1, *row)
                .is_some_and(|cell| cell.flags.contains(CellFlags::WIDE_CHAR))
        {
            *end += 1;
        }
    }
    ranges
}

fn selection_ranges_from_spans(
    spans: &[SelectionSpan],
    cols: usize,
    rows: usize,
) -> Vec<(usize, usize, usize)> {
    if cols == 0 || rows == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    for span in spans {
        let (start_row, start_col) = span.start;
        let (end_row, end_col) = span.end;
        if start_row > end_row || (start_row == end_row && start_col > end_col) {
            continue;
        }
        if start_row >= rows {
            continue;
        }
        let clipped_end = end_row >= rows;
        let last_row = end_row.min(rows - 1);
        for row in start_row..=last_row {
            let start = if row == start_row {
                start_col.min(cols)
            } else {
                0
            };
            // Snapshot spans are inclusive on both ends.
            let end = if clipped_end || row < end_row {
                cols
            } else {
                end_col.saturating_add(1).min(cols)
            };
            if start < end {
                out.push((row, start, end));
            }
        }
    }
    out
}

pub(crate) fn row_text(snapshot: &Snapshot, row: usize, cols: usize) -> String {
    let mut text = String::with_capacity(cols);
    for col in 0..cols {
        match snapshot.cell(col, row) {
            Some(cell) if !cell.flags.contains(CellFlags::WIDE_CHAR_SPACER) => {
                text.push(cell.ch);
            }
            // A spacer is still one grid column. Pushing a placeholder keeps
            // char indices aligned with columns so search highlights and hit
            // testing stay correct on rows containing wide characters.
            _ => text.push(' '),
        }
    }
    text
}

fn frame_interval(ui: &Ui) -> f32 {
    ui.ctx()
        .input(|input| input.stable_dt)
        .clamp(1.0 / 240.0, 1.0 / 24.0)
}

fn link_at(
    session: &Session,
    snapshot: &Snapshot,
    row_text: &str,
    row: usize,
    col: usize,
) -> Option<String> {
    let cell_col = if col > 0
        && snapshot
            .cell(col, row)
            .is_some_and(|cell| cell.flags.contains(CellFlags::WIDE_CHAR_SPACER))
    {
        col - 1
    } else {
        col
    };
    if let Some(cell) = snapshot.cell(cell_col, row)
        && let Some(id) = cell.hyperlink
        && let Some(url) = session.hyperlink(id)
    {
        return Some(url);
    }
    url_at(row_text, col)
}

fn url_at(text: &str, col: usize) -> Option<String> {
    let chars: Vec<char> = text.chars().collect();
    for (start, end) in url_ranges(text) {
        if col >= start && col < end {
            return Some(chars[start..end].iter().collect());
        }
    }
    None
}

/// URL spans as character indices, so they line up with terminal columns.
fn url_ranges(text: &str) -> Vec<(usize, usize)> {
    let chars: Vec<char> = text.chars().collect();
    let mut ranges = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let prefix_len = if starts_with_chars(&chars, i, "https://") {
            8
        } else if starts_with_chars(&chars, i, "http://") {
            7
        } else {
            i += 1;
            continue;
        };
        let host_start = i + prefix_len;
        let mut end = host_start;
        while end < chars.len() && !is_url_terminator(chars[end]) {
            end += 1;
        }
        while end > host_start && matches!(chars[end - 1], '.' | ',' | ';' | ':' | '!' | '?') {
            end -= 1;
        }
        if end > host_start {
            ranges.push((i, end));
            i = end;
        } else {
            i += 1;
        }
    }
    ranges
}

fn starts_with_chars(chars: &[char], start: usize, needle: &str) -> bool {
    needle
        .chars()
        .enumerate()
        .all(|(offset, ch)| chars.get(start + offset) == Some(&ch))
}

fn is_url_terminator(ch: char) -> bool {
    ch.is_whitespace()
        || matches!(
            ch,
            '"' | '\'' | '<' | '>' | '(' | ')' | '[' | ']' | '{' | '}'
        )
}

pub(crate) fn find_matches(text: &str, needle: &str, case_sensitive: bool) -> Vec<(usize, usize)> {
    let haystack: Vec<char> = text.chars().collect();
    let needle: Vec<char> = needle.chars().collect();
    if needle.is_empty() || needle.len() > haystack.len() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut i = 0;
    while i + needle.len() <= haystack.len() {
        let matched = needle.iter().enumerate().all(|(offset, wanted)| {
            let got = haystack[i + offset];
            if case_sensitive {
                got == *wanted
            } else if got.is_ascii() && wanted.is_ascii() {
                got.eq_ignore_ascii_case(wanted)
            } else {
                got.to_lowercase().eq(wanted.to_lowercase())
            }
        });
        if matched {
            out.push((i, i + needle.len()));
            i += needle.len();
        } else {
            i += 1;
        }
    }
    out
}

fn trim_selection(text: &str) -> String {
    text.split('\n')
        .map(|line| line.trim_end_matches([' ', '\t', '\r']))
        .collect::<Vec<_>>()
        .join("\n")
}

fn map_key(key: EguiKey) -> Option<Key> {
    Some(match key {
        EguiKey::ArrowUp => Key::Up,
        EguiKey::ArrowDown => Key::Down,
        EguiKey::ArrowLeft => Key::Left,
        EguiKey::ArrowRight => Key::Right,
        EguiKey::Enter => Key::Enter,
        EguiKey::Tab => Key::Tab,
        EguiKey::Backspace => Key::Backspace,
        EguiKey::Escape => Key::Escape,
        EguiKey::Insert => Key::Insert,
        EguiKey::Delete => Key::Delete,
        EguiKey::Home => Key::Home,
        EguiKey::End => Key::End,
        EguiKey::PageUp => Key::PageUp,
        EguiKey::PageDown => Key::PageDown,
        EguiKey::F1 => Key::F1,
        EguiKey::F2 => Key::F2,
        EguiKey::F3 => Key::F3,
        EguiKey::F4 => Key::F4,
        EguiKey::F5 => Key::F5,
        EguiKey::F6 => Key::F6,
        EguiKey::F7 => Key::F7,
        EguiKey::F8 => Key::F8,
        EguiKey::F9 => Key::F9,
        EguiKey::F10 => Key::F10,
        EguiKey::F11 => Key::F11,
        EguiKey::F12 => Key::F12,
        _ => return None,
    })
}

fn printable_char(key: EguiKey) -> Option<char> {
    Some(match key {
        EguiKey::A => 'a',
        EguiKey::B => 'b',
        EguiKey::C => 'c',
        EguiKey::D => 'd',
        EguiKey::E => 'e',
        EguiKey::F => 'f',
        EguiKey::G => 'g',
        EguiKey::H => 'h',
        EguiKey::I => 'i',
        EguiKey::J => 'j',
        EguiKey::K => 'k',
        EguiKey::L => 'l',
        EguiKey::M => 'm',
        EguiKey::N => 'n',
        EguiKey::O => 'o',
        EguiKey::P => 'p',
        EguiKey::Q => 'q',
        EguiKey::R => 'r',
        EguiKey::S => 's',
        EguiKey::T => 't',
        EguiKey::U => 'u',
        EguiKey::V => 'v',
        EguiKey::W => 'w',
        EguiKey::X => 'x',
        EguiKey::Y => 'y',
        EguiKey::Z => 'z',
        EguiKey::Num0 => '0',
        EguiKey::Num1 => '1',
        EguiKey::Num2 => '2',
        EguiKey::Num3 => '3',
        EguiKey::Num4 => '4',
        EguiKey::Num5 => '5',
        EguiKey::Num6 => '6',
        EguiKey::Num7 => '7',
        EguiKey::Num8 => '8',
        EguiKey::Num9 => '9',
        EguiKey::Space => ' ',
        EguiKey::Colon => ':',
        EguiKey::Comma => ',',
        EguiKey::Backslash => '\\',
        EguiKey::Slash => '/',
        EguiKey::Pipe => '|',
        EguiKey::Questionmark => '?',
        EguiKey::Exclamationmark => '!',
        EguiKey::OpenBracket => '[',
        EguiKey::CloseBracket => ']',
        EguiKey::OpenCurlyBracket => '{',
        EguiKey::CloseCurlyBracket => '}',
        EguiKey::Backtick => '`',
        EguiKey::Minus => '-',
        EguiKey::Period => '.',
        EguiKey::Plus => '+',
        EguiKey::Equals => '=',
        EguiKey::Semicolon => ';',
        EguiKey::Quote => '\'',
        _ => return None,
    })
}

fn key_mods(modifiers: &Modifiers) -> Mods {
    Mods {
        ctrl: modifiers.ctrl,
        alt: modifiers.alt,
        shift: modifiers.shift,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics() -> CellMetrics {
        CellMetrics {
            width: 9.0,
            height: 18.0,
            ascent: 14.0,
            baseline: 14.0,
            width_px: 9,
            height_px: 18,
        }
    }

    #[test]
    fn grid_size_uses_floor_and_minimum() {
        let rect = Rect::from_min_size(Pos2::ZERO, vec2(800.0, 480.0));
        assert_eq!(TerminalView::grid_size(rect, &metrics(), 8.0), (87, 25));

        let tiny = Rect::from_min_size(Pos2::ZERO, vec2(4.0, 4.0));
        assert_eq!(TerminalView::grid_size(tiny, &metrics(), 8.0), (1, 1));
    }

    #[test]
    fn cell_at_clamps_edges() {
        let rect = Rect::from_min_size(pos2(8.0, 16.0), vec2(90.0, 36.0));
        assert_eq!(
            TerminalView::cell_at(pos2(8.0, 16.0), rect, &metrics(), (10, 2)),
            Some((0, 0))
        );
        assert_eq!(
            TerminalView::cell_at(pos2(97.9, 51.9), rect, &metrics(), (10, 2)),
            Some((1, 9))
        );
        assert_eq!(
            TerminalView::cell_at(pos2(98.0, 52.0), rect, &metrics(), (10, 2)),
            Some((1, 9))
        );
        assert_eq!(
            TerminalView::cell_at(pos2(7.9, 16.0), rect, &metrics(), (10, 2)),
            None
        );
    }

    #[test]
    fn selection_span_converts_to_two_row_rects() {
        let rect = Rect::from_min_size(pos2(10.0, 20.0), vec2(90.0, 54.0));
        let span = SelectionSpan {
            start: (0, 2),
            end: (1, 4),
        };
        let rects = selection_rects(&[span], (10, 3), rect, &metrics());
        assert_eq!(rects.len(), 2);
        assert_eq!(
            rects[0],
            Rect::from_min_max(pos2(28.0, 20.0), pos2(100.0, 38.0))
        );
        assert_eq!(
            rects[1],
            Rect::from_min_max(pos2(10.0, 38.0), pos2(55.0, 56.0))
        );
    }

    #[test]
    fn cell_flags_map_to_glyph_styles() {
        assert_eq!(glyph_style(CellFlags::empty()), GlyphStyle::Regular);
        assert_eq!(glyph_style(CellFlags::BOLD), GlyphStyle::Bold);
        assert_eq!(glyph_style(CellFlags::ITALIC), GlyphStyle::Italic);
        assert_eq!(glyph_style(CellFlags::BOLD_ITALIC), GlyphStyle::BoldItalic);
    }

    #[test]
    fn scroll_target_accumulates_three_lines_per_notch() {
        assert_eq!(apply_wheel(0, 3), 9);
        assert_eq!(apply_wheel(9, -2), 3);
        assert_eq!(apply_wheel(1, -1), 0);
    }

    #[test]
    fn scrollbar_thumb_length_tracks_the_viewport_ratio() {
        // No scrollback: the thumb fills the track (and the bar is hidden).
        assert_eq!(scrollbar_thumb_length(200.0, 24, 0), 200.0);
        // Half the document visible: half the track.
        assert_eq!(scrollbar_thumb_length(200.0, 24, 24), 100.0);
        // A huge scrollback keeps a grabbable minimum.
        assert_eq!(
            scrollbar_thumb_length(200.0, 24, 1_000_000),
            SCROLLBAR_MIN_THUMB
        );
        // A track shorter than the minimum clamps to the track itself.
        assert_eq!(scrollbar_thumb_length(10.0, 24, 1_000), 10.0);
        // Degenerate geometry is zero, never negative or NaN.
        assert_eq!(scrollbar_thumb_length(0.0, 24, 100), 0.0);
        assert_eq!(scrollbar_thumb_length(-5.0, 24, 100), 0.0);
    }

    #[test]
    fn scrollbar_thumb_top_tracks_display_offset() {
        let track = Rect::from_min_max(pos2(0.0, 0.0), pos2(10.0, 200.0));
        // display_offset 0 is the live bottom.
        assert_eq!(Scrollbar::new(track, 24, 24, 0).thumb.top(), 100.0);
        assert_eq!(Scrollbar::new(track, 24, 24, 12).thumb.top(), 50.0);
        assert_eq!(Scrollbar::new(track, 24, 24, 24).thumb.top(), 0.0);
        // Offsets past the ends still clamp onto the track.
        assert_eq!(Scrollbar::new(track, 24, 24, 99).thumb.top(), 0.0);
    }

    #[test]
    fn scrollbar_is_hidden_without_scrollback() {
        let content = Rect::from_min_size(pos2(0.0, 0.0), vec2(400.0, 200.0));
        let grid = Rect::from_min_size(pos2(8.0, 8.0), vec2(380.0, 184.0));
        assert_eq!(Scrollbar::for_grid(content, grid, 24, 0, 0), None);
        let bar =
            Scrollbar::for_grid(content, grid, 24, 100, 0).expect("scrollback shows a scrollbar");
        assert_eq!(bar.track.right(), content.right());
        assert_eq!(bar.track.width(), SCROLLBAR_WIDTH);
        assert!(bar.track.top() >= grid.top() && bar.track.bottom() <= grid.bottom());
        // Zero-height grids and zero-height tracks are ignored.
        let flat = Rect::from_min_size(pos2(8.0, 8.0), vec2(384.0, 0.0));
        assert_eq!(Scrollbar::for_grid(content, flat, 24, 100, 0), None);
    }

    #[test]
    fn scrollbar_track_never_overlaps_the_cell_area() {
        let content = Rect::from_min_size(Pos2::ZERO, vec2(800.0, 480.0));
        let metrics = metrics();
        for padding in [0.0, 1.0, 2.0, 4.0, 8.0, 12.0] {
            let (cols, rows) = TerminalView::grid_size(content, &metrics, padding);
            let cell_area = Rect::from_min_size(
                pos2(content.min.x + padding, content.min.y + padding),
                vec2(metrics.width * cols as f32, metrics.height * rows as f32),
            );
            let bar = Scrollbar::for_grid(content, cell_area, rows, 100, 0)
                .unwrap_or_else(|| panic!("padding {padding} lost its scroll affordance"));
            assert!(bar.track.width() > 0.0);
            assert!(bar.track.right() <= content.right());
            assert!(bar.track.height() > 0.0);
            if bar.overlay {
                // No usable gutter: the fallback lives inside the grid's right
                // edge, hidden until hovered, so it does not permanently cover
                // the last column.
                assert!(
                    !bar.paints_when_idle(),
                    "fallback must not paint idle at padding {padding}"
                );
                assert!(bar.track.left() >= cell_area.right() - SCROLLBAR_OVERLAY_WIDTH);
                assert!(bar.track.right() <= cell_area.right());
                assert!(bar.track.width() <= SCROLLBAR_OVERLAY_WIDTH);
            } else {
                assert!(
                    !bar.track.intersects(cell_area),
                    "the strip overlaps cells at padding {padding}"
                );
                assert!(bar.track.left() >= cell_area.right());
                assert_eq!(bar.track.width(), SCROLLBAR_WIDTH);
            }
        }
        // Padding 0 and 1 have no gutter at all: the fallback must be in
        // effect rather than silently dropping the scrollbar.
        for padding in [0.0, 1.0] {
            let (cols, rows) = TerminalView::grid_size(content, &metrics, padding);
            let cell_area = Rect::from_min_size(
                pos2(padding, padding),
                vec2(metrics.width * cols as f32, metrics.height * rows as f32),
            );
            let bar = Scrollbar::for_grid(content, cell_area, rows, 100, 0).unwrap();
            assert!(
                bar.overlay,
                "padding {padding} must use the overlay fallback"
            );
            assert!(!bar.paints_when_idle());
        }
        // A gutter wide enough still gets the full comfortable strip.
        let (cols, rows) = TerminalView::grid_size(content, &metrics, 12.0);
        let cell_area = Rect::from_min_size(
            pos2(12.0, 12.0),
            vec2(metrics.width * cols as f32, metrics.height * rows as f32),
        );
        let bar = Scrollbar::for_grid(content, cell_area, rows, 100, 0).unwrap();
        assert!(!bar.overlay);
        assert_eq!(bar.track.width(), SCROLLBAR_WIDTH);
    }

    #[test]
    fn scrollbar_ownership_is_sticky_to_the_grid() {
        // Hovering the strip owns the pointer when no press began on the grid.
        assert!(scrollbar_owns_pointer(false, false, true));
        // A press that began on the grid keeps ownership across the strip.
        assert!(!scrollbar_owns_pointer(false, true, true));
        // An active drag owns the pointer even outside the track.
        assert!(scrollbar_owns_pointer(true, true, false));
    }

    #[test]
    fn scrollbar_drag_ends_when_button_pointer_or_focus_is_gone() {
        let pos = pos2(4.0, 4.0);
        assert!(!scrollbar_drag_ended(true, Some(pos), true));
        assert!(scrollbar_drag_ended(false, Some(pos), true));
        assert!(scrollbar_drag_ended(true, None, true));
        assert!(scrollbar_drag_ended(true, Some(pos), false));
    }

    #[test]
    fn drag_frame_applies_its_delta_once() {
        // 200 pt track, 40 pt thumb, 80 lines: dragging to the top asks for 80.
        let grab = ScrollbarGrab {
            grab: 0.0,
            track_top: 0.0,
            track_height: 200.0,
            thumb_height: 40.0,
            history: 80,
        };
        // Exercise the real per-frame path used by `show`, not just the
        // formula. The first frame reports the delta exactly once.
        let first = scrollbar_drag_frame(&grab, 0, Some(0.0), false).expect("active drag");
        assert_eq!((first.target, first.delta), (80, 80));
        // `show` applies `delta` once to the session and paints the thumb from
        // `target`, skipping easing. Replaying the same pointer against the
        // updated offset must therefore be a no-op; re-introducing a second
        // application would make this 80 again.
        let replay =
            scrollbar_drag_frame(&grab, first.target, Some(0.0), false).expect("active drag");
        assert_eq!((replay.target, replay.delta), (80, 0));
        // The end condition is evaluated before any delta: a stale grab on
        // return to a hidden tab reports nothing at all.
        assert_eq!(scrollbar_drag_frame(&grab, 0, Some(0.0), true), None);
        assert_eq!(scrollbar_drag_frame(&grab, 0, None, false), None);
    }

    #[test]
    fn drag_geometry_is_frozen_at_press_time() {
        // Offsets come from the grab's history, not the live one, so lines
        // arriving during a drag cannot shift the view under the pointer.
        let frozen = ScrollbarGrab {
            grab: 0.0,
            track_top: 0.0,
            track_height: 200.0,
            thumb_height: 40.0,
            history: 80,
        };
        let grown = ScrollbarGrab {
            history: 800,
            ..frozen
        };
        assert_eq!(apply_scrollbar_drag(&frozen, 0, 80.0).0, 40);
        assert_eq!(apply_scrollbar_drag(&grown, 0, 80.0).0, 400);
    }

    #[test]
    fn drag_geometry_freezes_the_track_origin_too() {
        let grab = ScrollbarGrab {
            grab: 10.0,
            track_top: 40.0,
            track_height: 200.0,
            thumb_height: 40.0,
            history: 80,
        };
        // A resize moved the live track down by 50 pt; the inverse mapping and
        // the painted thumb must both keep using the press-time origin.
        let moved = Rect::from_min_max(pos2(390.0, 90.0), pos2(400.0, 290.0));
        let frozen = frozen_track_rect(&grab, moved);
        assert_eq!(frozen.top(), 40.0);
        assert_eq!(frozen.bottom(), 240.0);
        assert_eq!(frozen.left(), moved.left());
        // Pointer 60 pt below the *frozen* top maps to 55 lines into history,
        // regardless of where the live track now starts.
        assert_eq!(apply_scrollbar_drag(&grab, 0, 100.0).0, 55);
        let frame = scrollbar_drag_frame(&grab, 0, Some(100.0), false).unwrap();
        let painted = Scrollbar::with_thumb_height(
            frozen,
            grab.thumb_height,
            grab.history,
            usize::try_from(frame.target.max(0)).unwrap_or(0),
            false,
        );
        assert_eq!(painted.thumb.top(), 90.0);
    }

    #[test]
    fn grid_press_survives_scrollback_appearing_mid_drag() {
        // No scrollback: there is no bar, but a press still belongs to the
        // grid so copy-on-select and mouse-up keep working.
        let pos = pos2(395.0, 100.0);
        assert_eq!(press_target(None, pos, true), PressTarget::Grid);
        // Scrollback arrives and the pointer now happens to be over the bar.
        let content = Rect::from_min_size(Pos2::ZERO, vec2(400.0, 200.0));
        let grid = Rect::from_min_size(pos2(8.0, 8.0), vec2(380.0, 184.0));
        let bar = Scrollbar::for_grid(content, grid, 24, 100, 0).expect("bar exists");
        assert!(bar.track.contains(pos));
        // The existing grid press keeps ownership; the bar cannot steal it.
        assert!(!scrollbar_owns_pointer(false, true, true));
        // The press itself was never re-hit-tested, so the target is still the
        // grid and the release will be delivered there.
        assert_eq!(press_target(Some(&bar), pos, false), PressTarget::Grid);
        assert!(owes_grid_release(Some(PressOwner::Grid), false));
    }

    #[test]
    fn release_is_only_delivered_for_a_press_this_view_saw() {
        // A grid press owns and owes its release.
        assert!(owes_grid_release(Some(PressOwner::Grid), false));
        // A thumb drag that ends over the cells owes the grid nothing.
        assert!(!owes_grid_release(Some(PressOwner::Scrollbar), false));
        // A release whose press landed while another tab was shown is not
        // delivered either: mouse-reporting apps must not see a bare mouse-up.
        assert!(!owes_grid_release(None, false));
        // A same-frame click still reaches the grid.
        assert!(owes_grid_release(None, true));
    }

    #[test]
    fn track_click_does_not_page_past_the_live_bottom() {
        // Downward paging at offset 0 has nothing above to reveal.
        assert!(!track_click_pages(-24, 0));
        // Upward paging and paging back down from history are both allowed.
        assert!(track_click_pages(24, 0));
        assert!(track_click_pages(-24, 10));
    }

    #[test]
    fn scrollbar_drag_maps_position_to_offset_and_clamps() {
        // 40 px thumb on a 200 px track, 80 lines of history: 160 px travel.
        assert_eq!(drag_offset(200.0, 40.0, 80, 160.0, 0.0), 0);
        assert_eq!(drag_offset(200.0, 40.0, 80, 80.0, 0.0), 40);
        assert_eq!(drag_offset(200.0, 40.0, 80, 0.0, 0.0), 80);
        // Dragging past either end clamps instead of overshooting.
        assert_eq!(drag_offset(200.0, 40.0, 80, 500.0, 0.0), 0);
        assert_eq!(drag_offset(200.0, 40.0, 80, -500.0, 0.0), 80);
        // The grab offset keeps the thumb under the pointer.
        assert_eq!(drag_offset(200.0, 40.0, 80, 100.0, 20.0), 40);
        // Degenerate geometry never panics.
        assert_eq!(drag_offset(0.0, 0.0, 1_000, 50.0, 10.0), 0);
        assert_eq!(drag_offset(20.0, 40.0, 1_000, 5.0, 0.0), 0);
        assert_eq!(drag_offset(200.0, 40.0, 0, 50.0, 10.0), 0);
        assert_eq!(drag_offset(f32::NAN, 40.0, 80, 50.0, 10.0), 0);
    }

    #[test]
    fn find_url_returns_the_url_only() {
        assert_eq!(find_url("see https://x.y ok"), Some("https://x.y"));
        assert_eq!(
            find_url("http://example.com/a_b."),
            Some("http://example.com/a_b")
        );
        assert_eq!(find_url("no link here"), None);
    }

    #[test]
    fn url_at_finds_the_clicked_column() {
        let text = "go to https://x.y now";
        assert_eq!(url_at(text, 0), None);
        assert_eq!(url_at(text, 6), Some("https://x.y".to_string()));
        assert_eq!(url_at(text, 17), None);
        assert_eq!(url_at(text, 16), Some("https://x.y".to_string()));
    }

    #[test]
    fn search_finds_non_overlapping_matches() {
        assert_eq!(find_matches("aaaa", "aa", true), vec![(0, 2), (2, 4)]);
        assert_eq!(
            find_matches("Hello hello", "HELLO", false),
            vec![(0, 5), (6, 11)]
        );
    }

    #[test]
    fn indexed_palette_uses_cube_and_grayscale() {
        let theme = Theme::by_name("Illness Dark");
        assert_eq!(
            resolve_color(Color::Indexed(196), theme),
            Color32::from_rgb(255, 0, 0)
        );
        assert_eq!(
            resolve_color(Color::Indexed(244), theme),
            Color32::from_rgb(128, 128, 128)
        );
        assert_eq!(
            resolve_color(Color::Indexed(16), theme),
            Color32::from_rgb(0, 0, 0)
        );
        assert_eq!(
            resolve_color(Color::Indexed(231), theme),
            Color32::from_rgb(255, 255, 255)
        );
        assert_eq!(
            resolve_color(Color::Indexed(1), theme),
            design::color(theme.ansi[1])
        );
    }

    #[test]
    fn bright_blue_named_maps_to_ansi_12() {
        let theme = Theme::by_name("Illness Dark");
        // 12 is `NamedColor::BrightBlue`.
        assert_eq!(resolve_named(12, theme), design::color(theme.ansi[12]));
        assert_eq!(resolve_named(257, theme), design::color(theme.bg));
        assert_eq!(resolve_named(258, theme), design::color(theme.cursor));
    }

    #[test]
    fn wheel_reports_both_encoding_modes() {
        let mods = Mods::default();
        assert_eq!(
            wheel_reports(1, 0, 0, mods, true),
            vec![b"\x1b[<64;1;1M".to_vec()]
        );
        assert_eq!(
            wheel_reports(-2, 5, 4, mods, true),
            vec![b"\x1b[<65;6;5M".to_vec(), b"\x1b[<65;6;5M".to_vec()]
        );
        assert_eq!(
            wheel_reports(1, 0, 0, mods, false),
            vec![b"\x1b[M`!!".to_vec()]
        );
        assert!(wheel_reports(0, 0, 0, mods, true).is_empty());
    }

    #[test]
    fn selection_autoscroll_moves_toward_the_drag() {
        assert_eq!(selection_autoscroll_delta(99.0, 0.0, 50.0), -1);
        assert_eq!(selection_autoscroll_delta(-4.0, 0.0, 50.0), 1);
        assert_eq!(selection_autoscroll_delta(25.0, 0.0, 50.0), 0);
    }

    #[test]
    fn ctrl_c_without_selection_sends_etx() {
        assert_eq!(
            encode_input_with(vec![key_event(EguiKey::C, ctrl_mods())]),
            Some(vec![0x03])
        );
    }

    #[test]
    fn ctrl_alt_keys_are_encoded() {
        let alt = Modifiers {
            alt: true,
            ..ctrl_mods()
        };
        assert_eq!(
            encode_input_with(vec![key_event(EguiKey::C, alt)]),
            Some(vec![0x1b, 0x03])
        );
    }

    #[test]
    fn altgr_text_is_not_double_encoded() {
        let altgr = Modifiers {
            alt: true,
            ..ctrl_mods()
        };
        assert_eq!(
            encode_input_with(vec![
                key_event(EguiKey::Q, altgr),
                Event::Text("@".to_string())
            ]),
            Some(b"@".to_vec())
        );
    }

    #[test]
    fn explicit_modes_drive_cursor_keys_and_bracketed_paste() {
        let modes = TermModes {
            app_cursor: true,
            bracketed_paste: true,
            ..TermModes::default()
        };
        let view = TerminalView::default();

        let cursor = view.encode_input(
            &[key_event(EguiKey::ArrowUp, Modifiers::default())],
            Modifiers::default(),
            modes,
        );
        assert_eq!(cursor, Some(b"\x1bOA".to_vec()));
        assert_ne!(cursor, Some(b"\x1b[A".to_vec()));

        let pasted = view.encode_input(
            &[Event::Paste("hello".to_string())],
            Modifiers::default(),
            modes,
        );
        assert_eq!(pasted, Some(b"\x1b[200~hello\x1b[201~".to_vec()));
    }

    fn ctrl_mods() -> Modifiers {
        Modifiers {
            ctrl: true,
            command: true,
            ..Modifiers::default()
        }
    }

    fn key_event(key: EguiKey, modifiers: Modifiers) -> Event {
        Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    fn encode_input_with(events: Vec<Event>) -> Option<Vec<u8>> {
        TerminalView::default().encode_input(&events, Modifiers::default(), TermModes::default())
    }

    #[test]
    fn coalescing_at_75_hz_defers_at_most_half_a_frame() {
        let dt = 1.0 / 75.0;
        assert_eq!(coalesce_delay(0.0, dt), Some(dt * 0.5));
        assert!(
            coalesce_delay(0.003, dt).is_some_and(|delay| delay < dt * 0.5),
            "an early change waits only until the half-frame boundary"
        );
        assert_eq!(coalesce_delay(dt * 0.5, dt), None);
        assert_eq!(coalesce_delay(dt, dt), None);
        // A stall must not stretch the coalescing window past a display frame.
        assert_eq!(coalesce_delay(0.0, 0.2), Some(1.0 / 48.0));
    }

    #[test]
    fn resize_debounce_waits_for_stable_size_then_applies_exact_grid() {
        let start = 10.0;
        let mut debounce = ResizeDebounce::default();
        let applied = Some((80, 24, 8, 18));
        let target = (100, 30, 8, 18);

        assert_eq!(
            debounce.poll(target, applied, start, false),
            ResizePoll::Wait(start + RESIZE_DEBOUNCE_SECS)
        );
        // The same target at 75 Hz keeps the original deadline.
        assert_eq!(
            debounce.poll(target, applied, start + 0.04, false),
            ResizePoll::Wait(start + RESIZE_DEBOUNCE_SECS)
        );
        assert_eq!(
            debounce.poll(target, applied, start + RESIZE_DEBOUNCE_SECS, false),
            ResizePoll::Apply
        );
        assert_eq!(
            debounce.poll(target, Some(target), start + 0.2, false),
            ResizePoll::Idle
        );

        // A moving drag restarts the debounce for the new size.
        let moved = (110, 31, 8, 18);
        assert_eq!(
            debounce.poll(moved, Some(target), start + 0.3, false),
            ResizePoll::Wait(start + 0.3 + RESIZE_DEBOUNCE_SECS)
        );
        assert_eq!(
            debounce.poll(moved, Some(target), start + 0.31, false),
            ResizePoll::Wait(start + 0.3 + RESIZE_DEBOUNCE_SECS)
        );

        // Pointer release applies the final size immediately.
        assert_eq!(
            debounce.poll(moved, Some(target), start + 0.32, true),
            ResizePoll::Apply
        );
        assert_eq!(
            debounce.poll((112, 32, 8, 18), Some(moved), start + 0.4, true),
            ResizePoll::Apply
        );
    }
}
