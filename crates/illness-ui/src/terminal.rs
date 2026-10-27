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

/// Motion dedupe key: cell, button, and ctrl/alt/shift state.
type MotionKey = ((usize, usize), MouseButton, bool, bool, bool);

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

/// Pointer button events for one frame, read once so the mouse path does not
/// query egui input repeatedly on every painted frame.
#[derive(Debug, Clone, Copy, Default)]
struct PointerFrame {
    primary_pressed: bool,
    middle_pressed: bool,
    secondary_pressed: bool,
    primary_released: bool,
    middle_released: bool,
    secondary_released: bool,
    any_pressed: bool,
    any_released: bool,
    any_down: bool,
}

/// Buttons whose press report the child received and that are still down.
///
/// Two buttons can be down at once. A single slot lost the first press, so the
/// drag was attributed to the wrong button and one mouse-up went missing. The
/// newest press stays last, which makes it the drag button for motion.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ReportedButtons {
    down: [Option<MouseButton>; 3],
}

impl ReportedButtons {
    /// Records a press once; a repeated press does not duplicate the button.
    fn insert(&mut self, button: MouseButton) {
        if self.contains(button) {
            return;
        }
        if let Some(slot) = self.down.iter_mut().find(|slot| slot.is_none()) {
            *slot = Some(button);
        }
    }

    /// Drops a released button; returns whether its press was tracked.
    fn remove(&mut self, button: MouseButton) -> bool {
        let Some(slot) = self.down.iter_mut().find(|slot| **slot == Some(button)) else {
            return false;
        };
        *slot = None;
        self.compact();
        true
    }

    /// Keeps the newest press last after a hole was punched in the set.
    fn compact(&mut self) {
        let mut next = [None; 3];
        for (index, button) in self.down.iter().flatten().enumerate() {
            next[index] = Some(*button);
        }
        self.down = next;
    }

    fn contains(&self, button: MouseButton) -> bool {
        self.down.contains(&Some(button))
    }

    fn is_empty(&self) -> bool {
        self.down.iter().all(Option::is_none)
    }

    /// Most recently pressed button that is still down, if any.
    fn latest(&self) -> Option<MouseButton> {
        self.down.iter().rev().flatten().next().copied()
    }

    fn iter(&self) -> impl Iterator<Item = MouseButton> + '_ {
        self.down.iter().flatten().copied()
    }

    fn clear(&mut self) {
        self.down = [None; 3];
    }
}

/// Mouse-reporting modes from the previous frame, so a mode toggle can drop
/// the hover dedupe even if the pointer never moved.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct MouseModes {
    reporting: bool,
    motion: bool,
    drag: bool,
    sgr: bool,
}

impl MouseModes {
    fn from_modes(modes: TermModes) -> Self {
        Self {
            reporting: modes.mouse_reporting,
            motion: modes.mouse_motion,
            drag: modes.mouse_drag,
            sgr: modes.sgr_mouse,
        }
    }
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

/// Whether the child gets mouse reports this frame.
///
/// Shift normally bypasses reporting so the press becomes a local selection.
/// A press that was already reported keeps reporting until its release, or the
/// child would never see the mouse-up and the tracked button would leak. A
/// press that starts with Shift held is never reported, so `held` stays false
/// and the latch still wins.
fn reports_mouse(reporting: bool, shift: bool, held: bool) -> bool {
    reporting && (!shift || held)
}

/// Whether Shift suppresses motion this frame.
///
/// A press that was already reported keeps reporting motion until it ends, so
/// holding Shift mid-drag cannot stall a vim/tmux drag. A press that started
/// with Shift holds the latch and must stay suppressed even after Shift is
/// released, or releasing it would flip a selection into reporting.
fn shift_blocks_motion(shift: bool, latch: bool, held: bool) -> bool {
    (shift || latch) && !held
}

/// Whether a release frame ends the tracked press.
///
/// Another reported button may still be down, so a single release does not end
/// the drag. egui's `any_down` wins over the tracked set when a per-button
/// release was missed, or the press would latch forever.
fn press_ended(reported: &ReportedButtons, any_down: bool) -> bool {
    reported.is_empty() || !any_down
}

/// Whether this frame ends a gesture whose release must be honored.
///
/// A local press never reports a button, so it is always the primary button
/// that owns the drag: another button coming up mid-drag (a right click while
/// the left button is still down) must not freeze the selection. A reported
/// press tracks its own buttons, so any release may be the end frame and
/// [`press_ended`] decides whether the tracked set is empty.
fn gesture_released(reported: &ReportedButtons, released: bool, primary_released: bool) -> bool {
    if reported.is_empty() {
        primary_released
    } else {
        released
    }
}

/// Whether this frame ends the press's sticky ownership of the pointer.
fn gesture_ended(
    reported: &ReportedButtons,
    released: bool,
    primary_released: bool,
    any_down: bool,
    focused: bool,
) -> bool {
    (gesture_released(reported, released, primary_released) || !any_down || !focused)
        && press_ended(reported, any_down)
}

/// Whether button-less hover motion may be written this frame.
///
/// A background window must not drive a child's hover reports, so a repaint
/// caused by another window cannot feed the PTY. A drag that began while the
/// window was focused may finish.
fn motion_allowed(focused: bool, dragging: bool) -> bool {
    focused || dragging
}

/// Buttons whose press must reach the child this frame, in a stable order.
fn mouse_presses(pointer: PointerFrame) -> Vec<MouseButton> {
    let mut buttons = Vec::new();
    if pointer.primary_pressed {
        buttons.push(MouseButton::Left);
    }
    if pointer.middle_pressed {
        buttons.push(MouseButton::Middle);
    }
    if pointer.secondary_pressed {
        buttons.push(MouseButton::Right);
    }
    buttons
}

/// Releases to report this frame.
///
/// The release is reported whenever reporting is on, even if Shift went down
/// after the matching press: dropping it would leave the child waiting for a
/// mouse-up that never arrives.
fn mouse_releases(pointer: PointerFrame) -> Vec<MouseButton> {
    let mut releases = Vec::new();
    for (released, button) in [
        (pointer.primary_released, MouseButton::Left),
        (pointer.middle_released, MouseButton::Middle),
        (pointer.secondary_released, MouseButton::Right),
    ] {
        if released {
            releases.push(button);
        }
    }
    releases
}

/// Mouse-ups to write this frame: only buttons whose press this view reported.
///
/// egui reports every button it saw come up, including one it owned elsewhere:
/// a drag that began on the scrollbar thumb or on another tab never produced a
/// press report, so writing its release would hand the child a mouse-up with
/// no matching down. The button leaves the tracked set first, so a repeated
/// release frame writes nothing.
fn tracked_releases(reported: &mut ReportedButtons, pointer: PointerFrame) -> Vec<MouseButton> {
    let mut writes = Vec::new();
    for button in mouse_releases(pointer) {
        if reported.remove(button) {
            writes.push(button);
        }
    }
    writes
}

/// Mouse-up reports a press lost outside the window still owes the child.
///
/// winit delivers no release when the button comes up off the window, so a
/// 1002/1003 child would keep the button latched and later hover would arrive
/// button-less. Every tracked button gets one release at the last reported
/// cell, or at `fallback` when nothing was reported yet. Reporting that has
/// since been turned off means the child stopped listening, so nothing is due.
fn lost_release_reports(
    reporting: bool,
    reported: &ReportedButtons,
    last_cell: Option<(usize, usize)>,
    fallback: (usize, usize),
    mods: Mods,
    sgr: bool,
) -> Vec<Vec<u8>> {
    if !reporting {
        return Vec::new();
    }
    let (row, col) = last_cell.unwrap_or(fallback);
    reported
        .iter()
        .map(|button| encode_mouse(MousePhase::Release, button, col, row, mods, sgr))
        .collect()
}

/// Whether a press that lost the pointer must defer its owed mouse-up.
///
/// `PointerGone` is not a release: winit keeps capture, a `CursorMoved`
/// follows, and the real release arrives later. Writing the mouse-up at the
/// window edge now would put it on the wrong cell and leave a second, orphan
/// one for the real release. Only losing focus or the button itself ends it.
fn defers_release(any_down: bool, focused: bool) -> bool {
    any_down && focused
}

/// Whether this frame must pay a lost press's owed mouse-up before writing a
/// new press or release.
///
/// A frame can carry both a press and a release. Skipping the payment on that
/// frame let it write down, down, up, so the child never matched the first
/// press. Paying whenever the debt is owed keeps the order down, up, down, up;
/// the paid-up buttons leave the tracked set, so the same frame's release path
/// cannot write a second, orphan mouse-up.
fn pays_deferred_release(any_pressed: bool, deferred: bool) -> bool {
    any_pressed && deferred
}

/// Clamp a stored cell into a grid that may have shrunk since it was written.
fn clamp_cell(cell: (usize, usize), grid: (usize, usize)) -> (usize, usize) {
    let (cols, rows) = grid;
    (
        cell.0.min(rows.saturating_sub(1)),
        cell.1.min(cols.saturating_sub(1)),
    )
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
    /// Whether the view wrote non-motion input bytes (press, release, wheel,
    /// alternate-screen wheel keys) straight to this session during the frame.
    /// A mouse-up this view owed a press it lost outside the window is
    /// bookkeeping and never sets this flag.
    pub input_sent: bool,
    /// Whether the view wrote a hover or drag motion report. Motion is not a
    /// user action: it must not hide the startup recovery card or count as use,
    /// and it never sets [`Self::input_sent`].
    pub motion_sent: bool,
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
    /// Cell, button, and ctrl/alt/shift state of the last hover-motion report,
    /// so a stationary pointer (or a repaint) does not flood the PTY with
    /// duplicates, while a modifier change still reports.
    motion_cell: Option<MotionKey>,
    /// `(row, col)` of the last mouse report written, so a mouse-up lost
    /// outside the window still lands on the cell the child last heard about.
    last_report: Option<(usize, usize)>,
    /// Buttons whose press report the child received and that are still down.
    /// Tracked here because egui keeps `pointer.down` set after `PointerGone`.
    reported: ReportedButtons,
    /// True while a grid press is tracked, reported or local. It is the
    /// gesture's lifetime, so a second press cannot retake the Shift latch.
    press_active: bool,
    /// Shift override latched when a grid press starts. Releasing Shift
    /// mid-drag must not flip an in-progress selection into reporting.
    press_shift_override: bool,
    /// A press whose release egui never saw still owes the child a mouse-up.
    /// The write waits for the real release, focus loss, or a new press.
    deferred_release: bool,
    /// Buttons the host saw come up that egui never delivered, because
    /// egui-winit drops mouse events while it has no pointer position. Only
    /// these buttons are paid on the next painted frame, and only the tracked
    /// press bookkeeping changes: a local selection drag is not touched.
    host_release_pending: ReportedButtons,
    /// Buttons whose press was reported but whose tracking ended without a
    /// release, such as a focus loss. Their mouse-up is still owed and is paid
    /// by the next painted pass at the last reported cell, clamped into the
    /// live grid.
    host_owed: ReportedButtons,
    /// Set when a host release covered the primary button while a local
    /// selection was still live. The next normal frame finalizes it, because
    /// egui never delivered the release frame that would have.
    host_selection_release: bool,
    /// Modifiers from the last painted frame, used by `motion_wanted_at` to
    /// rebuild the motion dedupe key without querying input.
    last_mods: Mods,
    /// Grid geometry of the last painted frame, so `motion_wanted_at` can map
    /// a pointer position between frames.
    grid_rect: Option<Rect>,
    grid_metrics: Option<CellMetrics>,
    grid_cells: Option<(usize, usize)>,
    /// Mouse-reporting modes seen on the previous frame; a change drops the
    /// stale hover dedupe.
    observed_mouse: Option<MouseModes>,
    /// Window focus seen on the previous frame; losing or regaining it drops
    /// the stale hover dedupe.
    observed_focus: Option<bool>,
    last_layout: Option<(usize, usize, u32, u32)>,
    /// Whether the last painted snapshot had scrollback, so the app's gate can
    /// ask without building a second snapshot.
    scrollback: bool,
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
                motion_sent: false,
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
        self.observe_mouse_modes(snapshot.modes);
        let (cols, rows) = (snapshot.cols, snapshot.rows);
        if cols == 0 || rows == 0 {
            return TerminalResponse {
                response,
                actions: Vec::new(),
                cols,
                rows,
                input_sent: false,
                motion_sent: false,
            };
        }
        let grid_rect = Rect::from_min_size(
            pos2(content_rect.min.x + padding, content_rect.min.y + padding),
            vec2(metrics.width * cols as f32, metrics.height * rows as f32),
        );
        // Remember the mapping this frame used, so the app's repaint gate can
        // answer "would a move here report?" between frames.
        self.grid_rect = Some(grid_rect);
        self.grid_metrics = Some(metrics);
        self.grid_cells = Some((cols, rows));

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
        let mouse_mode = self.mouse_mode(mods.shift);

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
        // The snapshot is already in hand, so the app's gate reads this
        // instead of building a second one every frame.
        self.record_scrollback(snapshot.history_size);
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
        self.observe_focus(focused);
        let fallback_cell = pointer_present
            .and_then(|pos| Self::cell_at_clamped(pos, grid_rect, &metrics, (cols, rows)))
            .unwrap_or((0, 0));
        let report_mods = Mods {
            ctrl: mods.ctrl,
            alt: mods.alt,
            shift: mods.shift,
        };
        self.last_mods = report_mods;

        // The host saw a real button release that egui never delivered (winit
        // drops mouse events with no pointer position). Pay what the press
        // owes and end it before anything else reads the latch. The bytes are
        // owed bookkeeping, so they never count as user input.
        for report in self.take_host_release(fallback_cell, report_mods, (cols, rows)) {
            session.write(&report);
        }

        // A local selection has no reported press, so the hook above paid
        // nothing for it and egui never delivered its release frame. The first
        // frame in the normal (non-reporting) path finalizes it here.
        if self.host_selection_release {
            let selection = session.selection_to_string();
            self.finalize_host_selection(
                !mouse_mode,
                selection,
                config.copy_on_select,
                &mut actions,
            );
        }

        // A grab is only valid while its button is down, and that is checked
        // before any delta is computed. A release that lands while another tab
        // is shown is never observed there, so the stale grab must expire
        // instead of applying this tab's pointer position on return. The
        // release frame itself keeps ownership so its mouse-up is delivered.
        let drag_lost = scrollbar_drag_ended(any_down, pointer_present, focused) && !released;
        if drag_lost {
            self.scrollbar_grab = None;
            if defers_release(any_down, focused) {
                // Pointer leave is not a release: keep the press and its
                // dues until egui sees one or focus is lost.
                self.defer_lost_press();
            } else {
                // A press that was reported and is now lost (focus lost or
                // button up outside the window) still owes the child its
                // mouse-up, or a 1002/1003 drag stays latched. The write is
                // bookkeeping, not user action.
                self.release_lost_press(session, fallback_cell, report_mods, (cols, rows));
                // Only the press bookkeeping dies here. This branch runs on
                // every idle frame (nothing is down), so clearing the motion
                // dedupe would make a 1003 child re-report a stationary
                // pointer once per frame.
                self.lose_press();
            }
        }

        // A new press supersedes a press whose release was never seen: pay the
        // owed mouse-up first, or the child sees two downs in a row that it
        // cannot match to ups. A frame that also carries a release pays too,
        // and the cleared buttons keep its own release path from writing a
        // second, orphan mouse-up.
        if pays_deferred_release(any_pressed, self.deferred_release) {
            self.release_lost_press(session, fallback_cell, report_mods, (cols, rows));
            self.lose_press();
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
                    self.begin_press(None, mods.shift);
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
            if self.link_hover_wanted(ctrl, mods.shift) && hover_link.is_some() {
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
        let mut motion_sent = false;
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
                        shift_enter_newline: config.shift_enter_newline,
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
            // Hover motion also needs a call, so the grid runs whenever the
            // child gets mouse reports even without a press.
            let owes = owes_grid_release(self.press_owner, any_pressed) || mouse_mode;
            if !bar_owns_pointer && owes {
                let (sent, motion) = self.handle_mouse(
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
                    focused,
                    config.copy_on_select,
                    &mut actions,
                );
                input_sent |= sent;
                motion_sent |= motion;
            } else {
                self.selecting = false;
                if self.modes.mouse_reporting {
                    self.clear_motion();
                } else {
                    self.end_press();
                }
            }
        } else {
            // Pointer gone is not a release. A reported press keeps its
            // bookkeeping and owes its mouse-up when egui actually sees the
            // release, on focus loss, or when a new press supersedes it.
            if defers_release(any_down, focused) {
                self.defer_lost_press();
            } else {
                self.release_lost_press(session, fallback_cell, report_mods, (cols, rows));
                self.end_press();
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
        // A matching release already cleared the tracked button there; this
        // ends a press that never produced a report. The hover dedupe survives
        // because this branch also runs on every frame with no button down.
        // End the press only when no reported button is left down. A release
        // of one button while another is still held keeps the drag alive and
        // attributed to the held button; its mouse-up was written above.
        if gesture_ended(
            &self.reported,
            released,
            primary_released,
            any_down,
            focused,
        ) {
            if !any_down {
                // Bookkeeping only: an owed mouse-up must not mark the tab as
                // user-acted, so it never feeds `input_sent`.
                self.release_lost_press(session, fallback_cell, report_mods, (cols, rows));
            }
            self.lose_press();
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
            motion_sent,
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

    /// Forgets the last reported motion cell, so the next move reports again.
    /// Called on press, release, and whenever reporting stops.
    fn clear_motion(&mut self) {
        self.motion_cell = None;
    }

    /// Starts a grid press: latches the Shift override once per gesture and
    /// records the button only once a press report reaches the child.
    ///
    /// A second press during the same gesture keeps the existing latch, so
    /// Shift+Left selection followed by a Right press cannot flip a live
    /// selection into reporting after Shift was released.
    fn begin_press(&mut self, button: Option<MouseButton>, shift: bool) {
        if let Some(button) = button {
            self.reported.insert(button);
        }
        if !self.press_active {
            self.press_shift_override = shift;
        }
        self.press_active = true;
        self.clear_motion();
    }

    /// Ends a grid press after its matching release, after the pointer leaves,
    /// or when mouse reporting stops. No drag can follow it.
    fn end_press(&mut self) {
        self.reported.clear();
        self.press_active = false;
        self.press_shift_override = false;
        self.deferred_release = false;
        self.clear_motion();
    }

    /// Drops the press bookkeeping when a press was lost (no button down,
    /// pointer gone, or focus lost) without forgetting the motion dedupe.
    ///
    /// This runs on every idle frame, so it must not clear `motion_cell`: the
    /// dedupe is only reset when reporting actually stops or the pointer leaves
    /// its cell.
    fn lose_press(&mut self) {
        self.reported.clear();
        self.press_active = false;
        self.press_shift_override = false;
        self.deferred_release = false;
        self.selecting = false;
        self.press_owner = None;
    }

    /// Keeps a press whose release egui never saw alive, and marks its
    /// mouse-up as owed. The write waits for the real release, focus loss, or
    /// a new press; it must not happen merely because the pointer left.
    ///
    /// Only the reported press is deferred here. A local selection drag that
    /// crosses the window edge keeps `selecting`, so it can still extend when
    /// the pointer returns and finalize on the real release.
    fn defer_lost_press(&mut self) {
        if !self.reported.is_empty() {
            self.deferred_release = true;
        }
    }

    /// Records that the host saw a real button release.
    ///
    /// egui-winit drops `MouseInput` events while it has no pointer position,
    /// so a drag released off the window never reaches [`Self::show`] and the
    /// press latch would stick: the next click would be swallowed as a
    /// selection, or every later hover would be encoded as a drag. The next
    /// painted frame pays the mouse-up this button owes, if its press was
    /// reported, and ends the press once no reported button is left.
    ///
    /// A local selection is not finalized here: it has no reported button, so
    /// there is no mouse-up to pay, but its release frame never arrived either.
    /// The hook remembers it and the next normal (non-reporting) frame ends the
    /// selection and queues its copy-on-select text.
    ///
    /// Idempotent: a release already paid, or one with nothing tracked, writes
    /// nothing.
    pub fn host_release(&mut self, button: MouseButton) {
        self.host_release_pending.insert(button);
    }

    /// Consumes host-observed releases: the mouse-ups still owed to the child
    /// for the buttons the host reported or this view stopped tracking, with
    /// the press bookkeeping ended.
    ///
    /// These bytes are bookkeeping, not new input, so the caller must not count
    /// them as user action. Only a button whose press this view reported is
    /// paid; releasing one button of a multi-button press leaves the others
    /// tracked and the press alive. A primary release is proof that button is
    /// up, so it also drops a scrollbar grab and the pointer ownership that
    /// came with it. A selection being dragged is only finalized later, by the
    /// next normal frame.
    fn take_host_release(
        &mut self,
        fallback: (usize, usize),
        mods: Mods,
        grid: (usize, usize),
    ) -> Vec<Vec<u8>> {
        let pending = std::mem::take(&mut self.host_release_pending);
        let owed = std::mem::take(&mut self.host_owed);
        let primary = pending.contains(MouseButton::Left);
        // A live local selection has no reported press, so the loop below has
        // nothing to pay. Its release frame never arrived either, so flag it for
        // the next normal frame to finalize.
        if primary && self.selecting {
            self.host_selection_release = true;
        }
        // The button is physically up, so a grab that froze its geometry can
        // no longer follow the pointer, and a stale ownership would swallow
        // the next grid click.
        if primary {
            self.scrollbar_grab = None;
            self.press_owner = None;
        }
        // A pending release pays only a press this view reported; an owed one
        // already lost its tracking, so it always pays. Keep the first order
        // and pay each button once.
        let mut pay: Vec<MouseButton> = Vec::new();
        for button in pending.iter().chain(owed.iter()) {
            if pay.contains(&button) {
                continue;
            }
            if !owed.contains(button) && !self.reported.contains(button) {
                continue;
            }
            pay.push(button);
        }
        let mut reports = Vec::new();
        let mut paid = false;
        for button in pay {
            self.reported.remove(button);
            self.deferred_release = false;
            paid = true;
            if self.modes.mouse_reporting {
                let (row, col) = self
                    .last_report
                    .map(|cell| clamp_cell(cell, grid))
                    .unwrap_or(fallback);
                reports.push(encode_mouse(
                    MousePhase::Release,
                    button,
                    col,
                    row,
                    mods,
                    self.modes.sgr_mouse,
                ));
            }
        }
        if paid && self.reported.is_empty() {
            self.end_press();
        }
        reports
    }

    /// Forgets a press when the window loses focus, even if no frame painted
    /// the unfocused state.
    ///
    /// egui's `any_down` stays set across that gap, so a 1002/1003 child would
    /// otherwise receive button-held motion on every later hover, and a live
    /// selection drag would keep the autoscroll repaint loop running. Tracked
    /// buttons move to the owed set, so the next painted pass pays their
    /// mouse-up at the last reported cell. A local selection is left alone: it
    /// has no reported press, and a later release still finalizes it.
    ///
    /// Idempotent: a second call has no tracked press left to move.
    pub fn host_focus_lost(&mut self) {
        for button in self.reported.iter() {
            self.host_owed.insert(button);
        }
        self.reported.clear();
        self.press_active = false;
        self.press_shift_override = false;
        self.deferred_release = false;
        self.scrollbar_grab = None;
        self.press_owner = None;
    }

    /// Finalizes a local selection whose real release the host saw but egui
    /// never delivered.
    ///
    /// Only the normal (non-reporting) path owns a local selection, so a
    /// reporting frame leaves it pending. Returns whether it finalized; the
    /// pending flag is cleared first, so a second frame copies nothing.
    fn finalize_host_selection(
        &mut self,
        normal_path: bool,
        selection: Option<String>,
        copy_on_select: bool,
        actions: &mut Vec<TerminalAction>,
    ) -> bool {
        if !self.host_selection_release || !normal_path {
            return false;
        }
        self.host_selection_release = false;
        self.end_selection(selection, copy_on_select, actions);
        self.end_press();
        self.press_owner = None;
        true
    }

    /// Writes the mouse-up a press lost outside the window still owes the
    /// child, then leaves ending the press to the caller.
    ///
    /// Returns nothing on purpose: the bytes are owed bookkeeping, not new
    /// input, so no caller can fold them into `input_sent`.
    fn release_lost_press(
        &self,
        session: &Session,
        fallback: (usize, usize),
        mods: Mods,
        grid: (usize, usize),
    ) {
        for report in self.lost_release_reports(fallback, mods, grid) {
            session.write(&report);
        }
    }

    /// Release reports due for a lost press, at the last reported cell clamped
    /// into the live grid so a shrink cannot encode a cell outside it.
    fn lost_release_reports(
        &self,
        fallback: (usize, usize),
        mods: Mods,
        grid: (usize, usize),
    ) -> Vec<Vec<u8>> {
        let last_cell = self.last_report.map(|cell| clamp_cell(cell, grid));
        lost_release_reports(
            self.modes.mouse_reporting,
            &self.reported,
            last_cell,
            fallback,
            mods,
            self.modes.sgr_mouse,
        )
    }

    /// Drops the hover dedupe when the window gained or lost focus, so the
    /// cell under the pointer is re-reported on return.
    fn observe_focus(&mut self, focused: bool) {
        if self.observed_focus.is_some_and(|last| last != focused) {
            self.clear_motion();
        }
        self.observed_focus = Some(focused);
    }

    /// Drops the hover dedupe when mouse reporting modes changed, so turning
    /// tracking back on re-reports the cell under the pointer.
    fn observe_mouse_modes(&mut self, modes: TermModes) {
        let current = MouseModes::from_modes(modes);
        if self.observed_mouse.is_some_and(|last| last != current) {
            self.clear_motion();
        }
        self.observed_mouse = Some(current);
    }

    /// Hover motion for a cell with no modifiers; the tests' entry point.
    #[cfg(test)]
    fn motion_report(
        &mut self,
        col: usize,
        row: usize,
        button: Option<MouseButton>,
    ) -> Option<Vec<u8>> {
        self.motion_report_with(col, row, button, Mods::default())
    }

    /// Encodes one xterm motion report, or `None` when the child did not ask
    /// for motion on this cell.
    ///
    /// Mode 1003 reports every move, with or without a held button; mode 1002
    /// reports only while a button is down. Click-only mode (1000) never gets
    /// motion, and Shift suppresses it only while no press was reported. A cell
    /// already reported with the same button and ctrl/alt/shift state is not
    /// reported again until the pointer leaves it or those modifiers change.
    fn motion_report_with(
        &mut self,
        col: usize,
        row: usize,
        button: Option<MouseButton>,
        mods: Mods,
    ) -> Option<Vec<u8>> {
        let wanted = self.modes.mouse_motion || (self.modes.mouse_drag && button.is_some());
        let held = !self.reported.is_empty();
        let shift = shift_blocks_motion(mods.shift, self.press_shift_override, held);
        if !self.modes.mouse_reporting || shift || !wanted {
            self.motion_cell = None;
            return None;
        }
        let button = button.unwrap_or(MouseButton::None);
        let key = ((col, row), button, mods.ctrl, mods.alt, mods.shift);
        if self.motion_cell == Some(key) {
            return None;
        }
        self.motion_cell = Some(key);
        self.last_report = Some((row, col));
        Some(encode_mouse(
            MousePhase::Motion,
            button,
            col,
            row,
            mods,
            self.modes.sgr_mouse,
        ))
    }

    /// Motion report for a pointer position.
    ///
    /// A tracked button means a drag: the pointer may leave the grid through
    /// the padding gutter, so the position clamps to the nearest cell and the
    /// report keeps flowing. Button-less hover uses the unclamped mapping,
    /// because the gutter is not a cell and must write nothing instead of
    /// reporting an edge cell.
    fn motion_report_at(
        &mut self,
        pos: Pos2,
        rect: Rect,
        metrics: &CellMetrics,
        grid: (usize, usize),
        button: Option<MouseButton>,
        mods: Mods,
    ) -> Option<Vec<u8>> {
        let mapped = if button.is_some() {
            Self::cell_at_clamped(pos, rect, metrics, grid)
        } else {
            Self::cell_at(pos, rect, metrics, grid)
        };
        match mapped {
            Some((row, col)) => self.motion_report_with(col, row, button, mods),
            None => {
                self.clear_motion();
                None
            }
        }
    }

    /// Returns `(input_sent, motion_sent)`: non-motion bytes written to the
    /// child (press, release, wheel), and whether a hover/drag motion report was
    /// written. Motion must not count as user action.
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
        focused: bool,
        copy_on_select: bool,
        actions: &mut Vec<TerminalAction>,
    ) -> (bool, bool) {
        let cell = Self::cell_at(pos, grid_rect, metrics, grid);
        // Shift latched at press time keeps a selection drag from flipping
        // into reporting when Shift is released before the button, while a
        // press that was already reported still delivers its release.
        let mouse_mode = self.mouse_mode(mods.shift);
        let mouse_mods = Mods {
            ctrl: mods.ctrl,
            alt: mods.alt,
            shift: mods.shift,
        };
        let pointer = ui.input(|input| {
            let pointer = &input.pointer;
            PointerFrame {
                primary_pressed: pointer.button_pressed(PointerButton::Primary),
                middle_pressed: pointer.button_pressed(PointerButton::Middle),
                secondary_pressed: pointer.button_pressed(PointerButton::Secondary),
                primary_released: pointer.button_released(PointerButton::Primary),
                middle_released: pointer.button_released(PointerButton::Middle),
                secondary_released: pointer.button_released(PointerButton::Secondary),
                any_pressed: pointer.any_pressed(),
                any_released: pointer.any_released(),
                any_down: pointer.any_down(),
            }
        });
        let mut input_sent = false;
        let mut motion_sent = false;

        if mouse_mode {
            // Click-only reporting has nothing to do on a frame without a
            // press or release, so skip the coordinate work entirely.
            if !self.modes.mouse_motion
                && !self.modes.mouse_drag
                && !pointer.any_pressed
                && !pointer.any_released
            {
                self.clear_motion();
                self.selecting = false;
                return (false, false);
            }
            // `encode_mouse` takes zero-based coordinates and emits xterm's
            // one-based `col;row` payload.
            let Some((row, col)) = Self::cell_at_clamped(pos, grid_rect, metrics, grid) else {
                return (false, false);
            };
            let sgr = self.modes.sgr_mouse;
            for button in mouse_presses(pointer) {
                session.write(&encode_mouse(
                    MousePhase::Press,
                    button,
                    col,
                    row,
                    mouse_mods,
                    sgr,
                ));
                self.begin_press(Some(button), mods.shift);
                self.last_report = Some((row, col));
                input_sent = true;
            }
            // Each released button gets its own mouse-up, but only when this
            // view reported its press: a release that egui owned elsewhere
            // (scrollbar drag, another tab) must not reach the child. The
            // press survives while another reported button is still down.
            for button in tracked_releases(&mut self.reported, pointer) {
                session.write(&encode_mouse(
                    MousePhase::Release,
                    button,
                    col,
                    row,
                    mouse_mods,
                    sgr,
                ));
                self.last_report = Some((row, col));
                input_sent = true;
            }
            if !self.reported.is_empty() && press_ended(&self.reported, pointer.any_down) {
                self.end_press();
            }
            // Press and release frames already carried the button state, so
            // they must not also emit a motion report. Every other frame may
            // move the pointer: 1003 reports hover, 1002 reports a drag.
            if pointer.any_pressed || pointer.any_released {
                self.clear_motion();
            } else if motion_allowed(focused, !self.reported.is_empty())
                && let Some(report) = self.motion_report_at(
                    pos,
                    grid_rect,
                    metrics,
                    grid,
                    self.reported.latest(),
                    mouse_mods,
                )
            {
                session.write(&report);
                motion_sent = true;
            }
            self.selecting = false;
            return (input_sent, motion_sent);
        }

        // Selection and links take over whenever the child is not getting
        // mouse reports; a remembered motion cell would be stale by the time
        // reporting resumes. The press latch itself is cleared by `show` when
        // the press ends.
        self.clear_motion();
        if !self.modes.mouse_reporting {
            self.end_press();
        }

        let primary_pressed = pointer.primary_pressed;
        if primary_pressed {
            self.begin_press(None, mods.shift);
            let ctrl = mods.ctrl || mods.command;
            let link = cell.and_then(|(row, col)| {
                let text = row_texts.get(row)?;
                link_at(session, snapshot, text, row, col)
            });
            if ctrl && link.is_some() {
                self.selecting = false;
                return (false, false);
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

        let primary_released = pointer.primary_released;
        if self.selecting && primary_released {
            if let Some(pos) = response.interact_pointer_pos()
                && let Some((row, col)) = Self::cell_at_clamped(pos, grid_rect, metrics, grid)
            {
                let cell_rect = cell_rect(grid_rect.min, metrics, row, col);
                session.selection_update(col, row, side_at(pos, cell_rect, metrics));
            }
            self.end_selection(session.selection_to_string(), copy_on_select, actions);
        }
        (false, false)
    }

    /// Ends a live local selection after its release and queues the
    /// copy-on-select action for its text.
    ///
    /// Split out of [`Self::handle_mouse`] so a test can finalize a drag that
    /// the host repaired before egui saw its release.
    fn end_selection(
        &mut self,
        selection: Option<String>,
        copy_on_select: bool,
        actions: &mut Vec<TerminalAction>,
    ) {
        self.selecting = false;
        if let Some(text) = selection {
            let text = trim_selection(&text);
            if copy_on_select && !text.is_empty() {
                actions.push(TerminalAction::Copy(text));
            }
        }
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

    /// Whether the child gets mouse reports this frame, given the raw Shift
    /// state: Shift bypasses reporting unless a press this view reported is
    /// still held, and the Shift latched at press time keeps a local selection
    /// out of reporting after Shift is released.
    fn mouse_mode(&self, shift: bool) -> bool {
        reports_mouse(
            self.modes.mouse_reporting,
            shift || self.press_shift_override,
            !self.reported.is_empty(),
        )
    }

    /// Whether a Ctrl hover over a link should show the pointing hand.
    ///
    /// This is exactly the view's own condition, so the app's gate and the
    /// painted cursor icon cannot disagree. The caller still has to check that
    /// the pointer is over a link; this only answers the mouse-mode half.
    pub fn link_hover_wanted(&self, ctrl: bool, shift: bool) -> bool {
        ctrl && !self.mouse_mode(shift)
    }

    /// Whether the last painted frame had scrollback, i.e. whether a scrollbar
    /// exists. The app's repaint gate uses this instead of building a second
    /// snapshot every frame.
    pub fn has_scrollback(&self) -> bool {
        self.scrollback
    }

    /// Records the snapshot's history size for [`Self::has_scrollback`].
    fn record_scrollback(&mut self, history: usize) {
        self.scrollback = history > 0;
    }

    /// Whether the child asked for hover motion (DECSET 1003) in the most
    /// recent frame.
    ///
    /// Drag-only mode (1002) writes nothing on a button-less move, so it must
    /// not ask the app to repaint for cursor movement. Its reports during a
    /// press are covered by the app's pointer-down escape hatch.
    pub fn wants_motion(&self) -> bool {
        self.modes.mouse_reporting && self.modes.mouse_motion
    }

    /// Whether a pointer move to `pos` could write a motion report this frame.
    ///
    /// The app's repaint gate uses this so a move that would be deduped, land
    /// outside the grid, or hit a click-only child does not force a paint.
    /// Geometry comes from the last painted frame; before any frame recorded
    /// it, the caller paints.
    pub fn motion_wanted_at(&self, pos: egui::Pos2) -> bool {
        let (Some(rect), Some(metrics), Some(grid)) =
            (self.grid_rect, self.grid_metrics, self.grid_cells)
        else {
            return true;
        };
        let held = !self.reported.is_empty();
        let wanted = self.modes.mouse_reporting
            && (self.modes.mouse_motion || (self.modes.mouse_drag && held));
        if !wanted || shift_blocks_motion(self.last_mods.shift, self.press_shift_override, held) {
            return false;
        }
        // A held reported button is a drag, and the report path clamps through
        // the padding gutter, so the gate must clamp the same way or a repaint
        // would be skipped even though a paint would write.
        let mapped = if held {
            Self::cell_at_clamped(pos, rect, &metrics, grid)
        } else {
            Self::cell_at(pos, rect, &metrics, grid)
        };
        let Some((row, col)) = mapped else {
            return false;
        };
        let button = self.reported.latest().unwrap_or(MouseButton::None);
        let key = (
            (col, row),
            button,
            self.last_mods.ctrl,
            self.last_mods.alt,
            self.last_mods.shift,
        );
        self.motion_cell != Some(key)
    }

    /// Encode keyboard, text, IME, and paste events into PTY bytes.
    ///
    /// `modes` must be read from the live session in the same frame
    /// ([`Session::term_modes`]): the app encodes input before it builds the
    /// snapshot, so a mode change in this frame is already visible here. The app
    /// filters its global shortcuts first and calls this only when no overlay
    /// wants the keyboard. `shift_enter_newline` comes from
    /// [`Config::shift_enter_newline`]: when false, Shift+Enter sends a plain CR.
    pub fn encode_input(
        &self,
        events: &[Event],
        modifiers: Modifiers,
        modes: TermModes,
        shift_enter_newline: bool,
    ) -> Option<Vec<u8>> {
        let has_text = events
            .iter()
            .any(|event| matches!(event, Event::Text(_) | Event::Ime(ImeEvent::Commit(_))));
        let modes = KeyModes {
            app_cursor: modes.app_cursor,
            bracketed_paste: modes.bracketed_paste,
            shift_enter_newline,
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

    fn motion_modes(motion: bool, drag: bool) -> TermModes {
        TermModes {
            mouse_reporting: true,
            mouse_motion: motion,
            mouse_drag: drag,
            sgr_mouse: true,
            ..TermModes::default()
        }
    }

    #[test]
    fn motion_reports_only_in_motion_mode() {
        let mut view = TerminalView {
            modes: TermModes {
                mouse_reporting: true,
                mouse_motion: true,
                sgr_mouse: true,
                ..TermModes::default()
            },
            ..TerminalView::default()
        };
        assert_eq!(
            view.motion_report(3, 1, None).as_deref(),
            Some(b"\x1b[<35;4;2M".as_slice())
        );
        assert_eq!(view.motion_report(3, 1, None), None);
        view.modes = TermModes {
            mouse_reporting: true,
            sgr_mouse: true,
            ..TermModes::default()
        };
        assert_eq!(view.motion_report(5, 1, None), None);
    }

    #[test]
    fn drag_motion_needs_drag_mode() {
        let mut view = TerminalView {
            modes: motion_modes(false, false),
            ..TerminalView::default()
        };
        assert_eq!(view.motion_report(3, 1, Some(MouseButton::Left)), None);

        view.modes = motion_modes(false, true);
        assert_eq!(
            view.motion_report(3, 1, Some(MouseButton::Left)).as_deref(),
            Some(b"\x1b[<32;4;2M".as_slice())
        );
        // 1002 still has no button-less hover motion.
        assert_eq!(view.motion_report(5, 1, None), None);
    }

    #[test]
    fn motion_with_button_uses_that_button() {
        let mut view = TerminalView {
            modes: motion_modes(true, false),
            ..TerminalView::default()
        };
        assert_eq!(
            view.motion_report(0, 0, Some(MouseButton::Left)).as_deref(),
            Some(b"\x1b[<32;1;1M".as_slice())
        );
        assert_eq!(
            view.motion_report(0, 0, Some(MouseButton::Middle))
                .as_deref(),
            Some(b"\x1b[<33;1;1M".as_slice())
        );
        assert_eq!(
            view.motion_report(0, 0, Some(MouseButton::Right))
                .as_deref(),
            Some(b"\x1b[<34;1;1M".as_slice())
        );
    }

    #[test]
    fn motion_cleared_on_release_and_mode_off() {
        let mut view = TerminalView {
            modes: motion_modes(true, false),
            ..TerminalView::default()
        };
        assert!(view.motion_report(3, 1, None).is_some());
        assert_eq!(view.motion_report(3, 1, None), None);
        // A release forgets the cell, so the next move re-reports it.
        view.clear_motion();
        assert!(view.motion_report(3, 1, None).is_some());
        // Reporting off clears the memory; turning it back on re-reports.
        view.modes.mouse_reporting = false;
        assert_eq!(view.motion_report(3, 1, None), None);
        view.modes.mouse_reporting = true;
        assert!(view.motion_report(3, 1, None).is_some());
    }

    #[test]
    fn shift_overrides_reporting() {
        let mut view = TerminalView {
            modes: motion_modes(true, false),
            ..TerminalView::default()
        };
        let shift = Mods {
            shift: true,
            ..Mods::default()
        };
        assert_eq!(view.motion_report_with(3, 1, None, shift), None);
        // The override did not remember the cell as reported.
        assert!(view.motion_report(3, 1, None).is_some());
    }

    #[test]
    fn wants_motion_needs_reporting_and_a_motion_mode() {
        let mut view = TerminalView {
            modes: motion_modes(true, false),
            ..TerminalView::default()
        };
        assert!(view.wants_motion());
        // Drag-only mode writes nothing on a button-less hover.
        view.modes = motion_modes(false, true);
        assert!(!view.wants_motion());
        view.modes = motion_modes(false, false);
        assert!(!view.wants_motion());
        view.modes = motion_modes(true, false);
        view.modes.mouse_reporting = false;
        assert!(!view.wants_motion());
    }

    #[test]
    fn lost_press_keeps_the_motion_dedupe() {
        let mut view = TerminalView {
            modes: motion_modes(true, false),
            ..TerminalView::default()
        };
        assert!(view.motion_report(3, 1, None).is_some());
        // State of a press that is still pending when the idle frame arrives.
        view.reported.insert(MouseButton::Left);
        view.press_shift_override = true;
        view.selecting = true;
        view.press_owner = Some(PressOwner::Grid);
        // The press-loss path runs on every frame with no button down; it must
        // drop the press but keep the reported cell.
        view.lose_press();
        assert!(view.reported.is_empty());
        assert!(!view.press_shift_override);
        assert!(!view.selecting);
        assert_eq!(view.press_owner, None);
        // A stationary pointer stays deduped instead of re-reporting per frame.
        assert_eq!(view.motion_report(3, 1, None), None);
        // Moving to another cell still reports.
        assert!(view.motion_report(4, 1, None).is_some());
    }

    #[test]
    fn shift_after_a_reported_press_still_releases() {
        let mut view = TerminalView {
            modes: motion_modes(false, true),
            ..TerminalView::default()
        };
        let reporting = view.modes.mouse_reporting;
        // A press that starts without Shift is reported and tracked.
        let presses = mouse_presses(PointerFrame {
            primary_pressed: true,
            any_pressed: true,
            ..PointerFrame::default()
        });
        assert_eq!(presses, vec![MouseButton::Left]);
        assert!(reports_mouse(reporting, false, !view.reported.is_empty()));
        for button in presses {
            view.begin_press(Some(button), false);
        }
        assert_eq!(view.reported.latest(), Some(MouseButton::Left));
        // Shift goes down mid-drag: the press keeps owning reporting.
        assert!(reports_mouse(reporting, true, !view.reported.is_empty()));
        // Release with Shift still held: exactly one release, no leak.
        let releases = mouse_releases(PointerFrame {
            primary_released: true,
            any_released: true,
            ..PointerFrame::default()
        });
        assert_eq!(releases, vec![MouseButton::Left]);
        for button in releases {
            view.reported.remove(button);
        }
        if !view.reported.is_empty() && press_ended(&view.reported, false) {
            view.end_press();
        }
        assert!(view.reported.is_empty());
        // A leaked button would make this button-less hover look like a drag.
        assert_eq!(view.motion_report(3, 1, None), None);
        // A press that starts with Shift held never reports.
        view.begin_press(None, true);
        assert!(view.reported.is_empty());
        assert!(!reports_mouse(reporting, true, !view.reported.is_empty()));
    }

    #[test]
    fn drag_motion_clamps_through_the_gutter() {
        let rect = Rect::from_min_size(pos2(8.0, 16.0), vec2(90.0, 36.0));
        let mut view = TerminalView {
            modes: motion_modes(false, true),
            ..TerminalView::default()
        };
        let mods = Mods::default();
        // A drag that leaves the grid keeps reporting the nearest cell.
        assert_eq!(
            view.motion_report_at(
                pos2(5.0, 20.0),
                rect,
                &metrics(),
                (10, 2),
                Some(MouseButton::Left),
                mods
            )
            .as_deref(),
            Some(b"\x1b[<32;1;1M".as_slice())
        );
        // Button-less hover in the same gutter is not a cell.
        view.clear_motion();
        assert_eq!(
            view.motion_report_at(pos2(5.0, 20.0), rect, &metrics(), (10, 2), None, mods),
            None
        );
    }

    #[test]
    fn unfocused_window_reports_drag_but_not_hover() {
        assert!(motion_allowed(true, false));
        assert!(motion_allowed(true, true));
        assert!(motion_allowed(false, true));
        assert!(!motion_allowed(false, false));
    }

    #[test]
    fn pointer_loss_reports_one_release_and_clears_the_press() {
        let mut view = TerminalView {
            modes: motion_modes(true, false),
            ..TerminalView::default()
        };
        view.begin_press(Some(MouseButton::Left), false);
        view.last_report = Some((4, 2));
        assert_eq!(view.reported.latest(), Some(MouseButton::Left));
        // PointerGone arrives with no release event; the press still owes one.
        let reports = view.lost_release_reports((0, 0), Mods::default(), (10, 5));
        assert_eq!(reports, vec![b"\x1b[<0;3;5m".to_vec()]);
        view.lose_press();
        assert!(view.reported.is_empty());
        assert_eq!(view.reported.latest(), None);
        // A second loss frame writes nothing.
        assert!(
            view.lost_release_reports((0, 0), Mods::default(), (10, 5))
                .is_empty()
        );
    }

    #[test]
    fn lost_release_falls_back_to_the_clamped_cell() {
        let mut view = TerminalView {
            modes: motion_modes(true, false),
            ..TerminalView::default()
        };
        view.begin_press(Some(MouseButton::Right), false);
        // Nothing was reported yet, so the clamped cell under the pointer wins.
        assert_eq!(
            view.lost_release_reports((3, 0), Mods::default(), (10, 4)),
            vec![b"\x1b[<2;1;4m".to_vec()]
        );
        // Reporting off means the child is no longer listening.
        view.modes.mouse_reporting = false;
        assert!(
            view.lost_release_reports((3, 0), Mods::default(), (10, 4))
                .is_empty()
        );
    }

    #[test]
    fn shift_latch_suppresses_until_the_press_ends() {
        let mut view = TerminalView {
            modes: motion_modes(true, false),
            ..TerminalView::default()
        };
        // Press starts with Shift held: a selection, not a report.
        view.begin_press(None, true);
        let button = view.reported.latest();
        // Shift is released while the same press continues.
        assert_eq!(view.motion_report_with(3, 1, button, Mods::default()), None);
        let button = view.reported.latest();
        assert_eq!(view.motion_report_with(3, 1, button, Mods::default()), None);
        view.end_press();
        // A fresh press without Shift reports normally.
        view.begin_press(Some(MouseButton::Left), false);
        let button = view.reported.latest();
        assert_eq!(
            view.motion_report_with(3, 1, button, Mods::default())
                .as_deref(),
            Some(b"\x1b[<32;4;2M".as_slice())
        );
    }

    #[test]
    fn shift_after_a_reported_press_keeps_motion_flowing() {
        let mut view = TerminalView {
            modes: motion_modes(false, true),
            ..TerminalView::default()
        };
        view.begin_press(Some(MouseButton::Left), false);
        let shift = Mods {
            shift: true,
            ..Mods::default()
        };
        // Shift went down after the press was reported: the drag must not stall.
        assert_eq!(
            view.motion_report_with(3, 1, view.reported.latest(), shift)
                .as_deref(),
            Some(b"\x1b[<36;4;2M".as_slice())
        );
        // The opposite direction: a press that started with Shift never reports,
        // even after Shift is released.
        view.end_press();
        view.begin_press(None, true);
        assert_eq!(view.motion_report_with(3, 1, None, Mods::default()), None);
    }

    #[test]
    fn shift_blocks_motion_only_without_a_reported_press() {
        // Shift held with no reported press blocks.
        assert!(shift_blocks_motion(true, false, false));
        // No Shift, no latch: motion flows.
        assert!(!shift_blocks_motion(false, false, false));
        // The latch blocks a press that began with Shift.
        assert!(shift_blocks_motion(false, true, false));
        // A reported press keeps reporting, with or without Shift.
        assert!(!shift_blocks_motion(true, false, true));
        assert!(!shift_blocks_motion(false, true, true));
    }

    #[test]
    fn shift_release_mid_selection_keeps_reporting_off() {
        let mut view = TerminalView {
            modes: motion_modes(true, false),
            ..TerminalView::default()
        };
        // The press began with Shift, so it is a local selection.
        view.begin_press(None, true);
        // Shift is now released; the latch keeps the shared predicate false.
        let shift = false || view.press_shift_override;
        assert!(!reports_mouse(
            view.modes.mouse_reporting,
            shift,
            !view.reported.is_empty()
        ));
    }

    #[test]
    fn tracked_buttons_keep_a_press_alive_until_all_are_up() {
        let mut reported = ReportedButtons::default();
        reported.insert(MouseButton::Left);
        reported.insert(MouseButton::Right);
        // A repeated press does not duplicate the button.
        reported.insert(MouseButton::Right);
        // The newest press is the drag button.
        assert_eq!(reported.latest(), Some(MouseButton::Right));
        // The Right release leaves Left down: exactly one release, press alive.
        let releases = mouse_releases(PointerFrame {
            secondary_released: true,
            any_released: true,
            any_down: true,
            ..PointerFrame::default()
        });
        assert_eq!(releases, vec![MouseButton::Right]);
        assert!(reported.remove(releases[0]));
        assert_eq!(reported.latest(), Some(MouseButton::Left));
        assert!(!press_ended(&reported, true));
        // The Left release ends it.
        assert!(reported.remove(MouseButton::Left));
        assert!(reported.is_empty());
        assert!(press_ended(&reported, true));
        // A stale per-button array cannot latch: any_down false ends it.
        reported.insert(MouseButton::Left);
        assert!(press_ended(&reported, false));
        // Removing an untracked button changes nothing.
        assert!(!reported.remove(MouseButton::Middle));
        assert_eq!(reported.latest(), Some(MouseButton::Left));
        reported.clear();
        assert!(reported.is_empty());
    }

    #[test]
    fn partial_release_keeps_the_drag_on_the_held_button() {
        let mut view = TerminalView {
            modes: motion_modes(false, true),
            ..TerminalView::default()
        };
        // Left then Right, both reported, newest last.
        view.begin_press(Some(MouseButton::Left), false);
        view.begin_press(Some(MouseButton::Right), false);
        assert_eq!(view.reported.latest(), Some(MouseButton::Right));
        // Right comes up while Left is still down: one release, press alive.
        let releases = mouse_releases(PointerFrame {
            secondary_released: true,
            any_released: true,
            any_down: true,
            ..PointerFrame::default()
        });
        assert_eq!(releases, vec![MouseButton::Right]);
        assert!(view.reported.remove(MouseButton::Right));
        assert!(!press_ended(&view.reported, true));
        // A partial release must not clear the press: drag motion is still
        // attributed to the held Left button.
        assert_eq!(
            view.motion_report_with(2, 1, view.reported.latest(), Mods::default())
                .as_deref(),
            Some(b"\x1b[<32;3;2M".as_slice())
        );
        // Left coming up ends the press.
        assert!(view.reported.remove(MouseButton::Left));
        assert!(press_ended(&view.reported, true));
        view.end_press();
        assert!(view.reported.is_empty());
        assert!(!view.press_active);
    }

    #[test]
    fn pointer_loss_defers_the_release_to_the_real_one() {
        let mut view = TerminalView {
            modes: motion_modes(true, false),
            ..TerminalView::default()
        };
        view.begin_press(Some(MouseButton::Left), false);
        view.last_report = Some((2, 3));
        // The button is still down and the window is focused: defer the write.
        assert!(defers_release(true, true));
        view.defer_lost_press();
        assert!(view.deferred_release);
        assert_eq!(view.reported.latest(), Some(MouseButton::Left));
        // Focus loss, or the button coming up, writes immediately instead.
        assert!(!defers_release(true, false));
        assert!(!defers_release(false, true));
        // The pointer returns and the real release is seen once; nothing is
        // left owed at the stale reported cell.
        let releases = mouse_releases(PointerFrame {
            primary_released: true,
            any_released: true,
            any_down: false,
            ..PointerFrame::default()
        });
        assert_eq!(releases, vec![MouseButton::Left]);
        for button in releases {
            assert!(view.reported.remove(button));
        }
        assert!(
            view.lost_release_reports((0, 0), Mods::default(), (10, 5))
                .is_empty()
        );
        view.lose_press();
        assert!(!view.deferred_release);
        assert!(view.reported.is_empty());
    }

    #[test]
    fn release_without_a_reported_press_writes_nothing() {
        let mut reported = ReportedButtons::default();
        let frame = PointerFrame {
            primary_released: true,
            any_released: true,
            ..PointerFrame::default()
        };
        // Press owned by the scrollbar thumb, or by another tab: egui still
        // reports the button coming up, but this view never told the child.
        assert!(tracked_releases(&mut reported, frame).is_empty());
        // A press this view did report gets exactly one matching mouse-up.
        reported.insert(MouseButton::Left);
        assert_eq!(
            tracked_releases(&mut reported, frame),
            vec![MouseButton::Left]
        );
        assert!(reported.is_empty());
        // A duplicate release frame cannot write a second, orphan mouse-up.
        assert!(tracked_releases(&mut reported, frame).is_empty());
        // Two buttons down: only the one that comes up is written.
        reported.insert(MouseButton::Left);
        reported.insert(MouseButton::Right);
        let frame = PointerFrame {
            secondary_released: true,
            any_released: true,
            any_down: true,
            ..PointerFrame::default()
        };
        assert_eq!(
            tracked_releases(&mut reported, frame),
            vec![MouseButton::Right]
        );
        assert_eq!(reported.latest(), Some(MouseButton::Left));
    }

    #[test]
    fn host_release_pays_the_owed_mouse_up_once() {
        let mut view = TerminalView {
            modes: motion_modes(true, false),
            ..TerminalView::default()
        };
        // Shift+drag that left the window: the press is reported and its
        // release never arrived, so it is deferred and the latch is set.
        view.begin_press(Some(MouseButton::Left), true);
        view.last_report = Some((2, 3));
        view.defer_lost_press();
        view.scrollbar_grab = Some(ScrollbarGrab {
            grab: 0.0,
            track_top: 0.0,
            track_height: 10.0,
            thumb_height: 4.0,
            history: 10,
        });
        assert!(view.deferred_release);
        assert!(view.press_active);
        assert!(view.press_shift_override);
        view.press_owner = Some(PressOwner::Scrollbar);
        // The host saw the release: the next painted pass pays one mouse-up at
        // the last reported cell and ends the press.
        view.host_release(MouseButton::Left);
        let reports = view.take_host_release((0, 0), Mods::default(), (10, 5));
        assert_eq!(reports, vec![b"\x1b[<0;4;3m".to_vec()]);
        assert!(view.reported.is_empty());
        assert!(!view.press_active);
        assert!(!view.press_shift_override);
        assert!(!view.deferred_release);
        // A primary host release proves the button is up: the grab and its
        // pointer ownership cannot outlive it.
        assert!(view.scrollbar_grab.is_none());
        assert_eq!(view.press_owner, None);
        // Idempotent: a second release has nothing left to pay.
        view.host_release(MouseButton::Left);
        assert!(
            view.take_host_release((0, 0), Mods::default(), (10, 5))
                .is_empty()
        );
        assert!(view.reported.is_empty());
        // A release with no host event pending writes nothing at all.
        assert!(
            view.take_host_release((0, 0), Mods::default(), (10, 5))
                .is_empty()
        );
    }

    #[test]
    fn host_release_ends_a_press_that_was_never_deferred() {
        let mut view = TerminalView {
            modes: motion_modes(true, false),
            ..TerminalView::default()
        };
        view.begin_press(Some(MouseButton::Right), false);
        // Still reported, nothing deferred: the host release is the only sign
        // the button came up, so the child still gets its mouse-up.
        assert!(!view.deferred_release);
        view.host_release(MouseButton::Right);
        assert_eq!(
            view.take_host_release((0, 0), Mods::default(), (10, 5)),
            vec![b"\x1b[<2;1;1m".to_vec()]
        );
        assert!(view.reported.is_empty());
        assert!(!view.press_active);
    }

    #[test]
    fn host_release_finalizes_a_live_selection_and_copies() {
        let mut view = TerminalView::default();
        // A local drag: press, then a selection that is still live. Nothing was
        // reported, so the host hook has no mouse-up to pay.
        view.begin_press(None, false);
        view.selecting = true;
        view.press_owner = Some(PressOwner::Grid);
        view.host_release(MouseButton::Left);
        assert!(
            view.take_host_release((0, 0), Mods::default(), (10, 5))
                .is_empty()
        );
        // The press hook must not kill the drag the release frame finalizes. The
        // primary release does drop the pointer ownership it held.
        assert!(view.selecting);
        assert_eq!(view.press_owner, None);
        let mut actions = Vec::new();
        view.end_selection(Some("copied".to_owned()), true, &mut actions);
        assert!(!view.selecting);
        assert_eq!(actions, vec![TerminalAction::Copy("copied".to_owned())]);
        // Copy-on-select off, or an empty selection, queues nothing.
        let mut actions = Vec::new();
        view.end_selection(Some("copied".to_owned()), false, &mut actions);
        view.end_selection(Some("   ".to_owned()), true, &mut actions);
        view.end_selection(None, true, &mut actions);
        assert!(actions.is_empty());
    }

    #[test]
    fn host_release_finalizes_a_selection_without_a_reported_press() {
        let mut view = TerminalView::default();
        // A plain drag-select: no press ever reached the child, so nothing is
        // tracked and the host hook has no mouse-up to pay.
        view.begin_press(None, false);
        view.selecting = true;
        view.press_owner = Some(PressOwner::Grid);
        // A non-primary host release belongs to no selection.
        view.host_release(MouseButton::Middle);
        assert!(
            view.take_host_release((0, 0), Mods::default(), (10, 5))
                .is_empty()
        );
        assert!(!view.host_selection_release);
        assert!(view.selecting);
        view.host_release(MouseButton::Left);
        assert!(
            view.take_host_release((0, 0), Mods::default(), (10, 5))
                .is_empty()
        );
        // The hook must not kill the drag that the next frame finalizes, but the
        // primary release does end the ownership it held.
        assert!(view.selecting);
        assert_eq!(view.press_owner, None);
        assert!(view.host_selection_release);
        // A reporting frame does not own the selection and leaves it alone.
        let mut actions = Vec::new();
        assert!(!view.finalize_host_selection(
            false,
            Some("copied".to_owned()),
            true,
            &mut actions
        ));
        assert!(actions.is_empty());
        assert!(view.selecting);
        assert!(view.host_selection_release);
        // The next normal frame finalizes the selection and copies it.
        assert!(view.finalize_host_selection(true, Some("copied".to_owned()), true, &mut actions));
        assert_eq!(actions, vec![TerminalAction::Copy("copied".to_owned())]);
        assert!(!view.selecting);
        assert!(view.press_owner.is_none());
        assert!(!view.press_active);
        assert!(!view.host_selection_release);
        // Idempotent: a second frame has nothing left to finalize or copy.
        assert!(!view.finalize_host_selection(true, Some("copied".to_owned()), true, &mut actions));
        assert_eq!(actions, vec![TerminalAction::Copy("copied".to_owned())]);
    }

    #[test]
    fn a_new_press_pays_a_deferred_release_first() {
        // Nothing owed, or no press this frame: nothing is paid early.
        assert!(!pays_deferred_release(false, true));
        assert!(!pays_deferred_release(true, false));
        // A frame that carries a press and a release still pays the owed
        // mouse-up first, so the child never sees two downs in a row.
        assert!(pays_deferred_release(true, true));
    }

    #[test]
    fn scrollback_state_is_recorded_for_the_gate() {
        let mut view = TerminalView::default();
        assert!(!view.has_scrollback());
        view.record_scrollback(0);
        assert!(!view.has_scrollback());
        view.record_scrollback(12);
        assert!(view.has_scrollback());
        // A session that drops its history clears the flag again.
        view.record_scrollback(0);
        assert!(!view.has_scrollback());
    }

    #[test]
    fn host_release_spends_only_its_own_button() {
        let mut view = TerminalView {
            modes: motion_modes(true, false),
            ..TerminalView::default()
        };
        // Left drag with a second Right press on top of it.
        view.begin_press(Some(MouseButton::Left), false);
        view.begin_press(Some(MouseButton::Right), false);
        view.last_report = Some((1, 2));
        // Releasing Right must not write the Left mouse-up too.
        view.host_release(MouseButton::Right);
        assert_eq!(
            view.take_host_release((0, 0), Mods::default(), (10, 5)),
            vec![b"\x1b[<2;3;2m".to_vec()]
        );
        assert_eq!(view.reported.latest(), Some(MouseButton::Left));
        assert!(view.press_active);
        // A button the host released but this view never reported writes
        // nothing and changes nothing.
        view.host_release(MouseButton::Middle);
        assert!(
            view.take_host_release((0, 0), Mods::default(), (10, 5))
                .is_empty()
        );
        assert_eq!(view.reported.latest(), Some(MouseButton::Left));
    }

    #[test]
    fn host_release_without_reporting_drops_the_bookkeeping() {
        let mut view = TerminalView {
            modes: motion_modes(true, false),
            ..TerminalView::default()
        };
        view.begin_press(Some(MouseButton::Left), false);
        // The child stopped listening: nothing is due, but the press must not
        // latch forever.
        view.modes.mouse_reporting = false;
        view.host_release(MouseButton::Left);
        assert!(
            view.take_host_release((0, 0), Mods::default(), (10, 5))
                .is_empty()
        );
        assert!(view.reported.is_empty());
        assert!(!view.press_active);
    }

    #[test]
    fn deferred_pointer_loss_keeps_the_selection_alive() {
        let mut view = TerminalView {
            modes: motion_modes(true, false),
            ..TerminalView::default()
        };
        view.begin_press(Some(MouseButton::Left), false);
        view.selecting = true;
        view.press_owner = Some(PressOwner::Grid);
        view.defer_lost_press();
        assert!(view.deferred_release);
        assert_eq!(view.reported.latest(), Some(MouseButton::Left));
        // A selection that crosses the window edge must still extend.
        assert!(view.selecting);
        assert_eq!(view.press_owner, Some(PressOwner::Grid));
    }

    #[test]
    fn another_buttons_release_does_not_end_a_local_selection() {
        let mut view = TerminalView::default();
        // A local drag has no reported press, so the primary button owns it.
        view.begin_press(None, false);
        view.selecting = true;
        view.press_owner = Some(PressOwner::Grid);
        // A middle or right release arrives mid-drag: the gesture's own button
        // is still down and focused, so nothing may end.
        assert!(!gesture_released(&view.reported, true, false));
        assert!(!gesture_ended(&view.reported, true, false, true, true));
        assert!(view.selecting);
        assert!(view.press_active);
        assert_eq!(view.press_owner, Some(PressOwner::Grid));
        // The primary release ends the gesture and the selection still copies.
        assert!(gesture_released(&view.reported, true, true));
        assert!(gesture_ended(&view.reported, true, true, false, true));
        let mut actions = Vec::new();
        view.end_selection(Some("copied".to_owned()), true, &mut actions);
        assert_eq!(actions, vec![TerminalAction::Copy("copied".to_owned())]);
        view.lose_press();
        assert!(!view.selecting);
        // A reported press keeps the shared release flag and still needs the
        // tracked set to be empty before it ends.
        view.begin_press(Some(MouseButton::Left), false);
        assert!(gesture_released(&view.reported, true, false));
        assert!(!gesture_ended(&view.reported, true, false, true, true));
        assert!(gesture_ended(&view.reported, true, false, false, true));
        // A local press ends on focus loss too, since it tracks no button.
        assert!(gesture_ended(
            &ReportedButtons::default(),
            false,
            false,
            true,
            false
        ));
    }

    #[test]
    fn host_focus_lost_pays_the_reported_press() {
        let mut view = TerminalView {
            modes: motion_modes(false, true),
            ..TerminalView::default()
        };
        // Two reported buttons and a scrollbar grab, as after a thumb press
        // that turned into a multi-button drag.
        view.begin_press(Some(MouseButton::Left), false);
        view.begin_press(Some(MouseButton::Right), false);
        view.last_report = Some((10, 40));
        view.deferred_release = true;
        view.press_owner = Some(PressOwner::Scrollbar);
        view.scrollbar_grab = Some(ScrollbarGrab {
            grab: 0.0,
            track_top: 0.0,
            track_height: 10.0,
            thumb_height: 4.0,
            history: 10,
        });
        view.host_focus_lost();
        assert!(view.reported.is_empty());
        assert!(!view.press_active);
        assert!(!view.press_shift_override);
        assert!(!view.deferred_release);
        assert!(view.scrollbar_grab.is_none());
        assert_eq!(view.press_owner, None);
        // No tracked button means a 1002 child gets no drag motion on hover.
        assert_eq!(view.motion_report(3, 1, view.reported.latest()), None);
        assert_eq!(view.reported.latest(), None);
        // The next painted pass pays both owed mouse-ups at the last reported
        // cell, clamped into the shrunken grid.
        assert_eq!(
            view.take_host_release((0, 0), Mods::default(), (10, 5)),
            vec![b"\x1b[<0;10;5m".to_vec(), b"\x1b[<2;10;5m".to_vec()]
        );
        // Idempotent: a second focus loss and a second pass pay nothing.
        view.host_focus_lost();
        assert!(
            view.take_host_release((0, 0), Mods::default(), (10, 5))
                .is_empty()
        );
    }

    #[test]
    fn host_focus_lost_keeps_a_live_local_selection() {
        let mut view = TerminalView::default();
        view.begin_press(None, true);
        view.selecting = true;
        view.press_owner = Some(PressOwner::Grid);
        view.host_focus_lost();
        // Nothing was reported, so nothing is owed, but the drag must survive.
        assert!(view.selecting);
        assert!(!view.press_active);
        assert!(!view.press_shift_override);
        assert_eq!(view.press_owner, None);
        assert!(
            view.take_host_release((0, 0), Mods::default(), (10, 5))
                .is_empty()
        );
        assert!(view.selecting);
        // Its later release still finalizes and copies it.
        let mut actions = Vec::new();
        view.end_selection(Some("copied".to_owned()), true, &mut actions);
        assert_eq!(actions, vec![TerminalAction::Copy("copied".to_owned())]);
    }

    #[test]
    fn link_hover_wanted_matches_the_pointing_hand_gate() {
        let mut view = TerminalView {
            modes: motion_modes(true, false),
            ..TerminalView::default()
        };
        // A child that does not report: Ctrl alone shows the hand.
        view.modes.mouse_reporting = false;
        assert!(view.link_hover_wanted(true, false));
        assert!(!view.link_hover_wanted(false, false));
        // A reporting child owns the hover unless Shift takes it back.
        view.modes.mouse_reporting = true;
        assert!(!view.link_hover_wanted(true, false));
        assert!(view.link_hover_wanted(true, true));
        // The latch keeps Shift's answer after Shift is released.
        view.begin_press(None, true);
        assert!(view.link_hover_wanted(true, false));
        view.end_press();
        // A reported press holds reporting, so Shift cannot take it back.
        view.begin_press(Some(MouseButton::Left), false);
        assert!(!view.link_hover_wanted(true, true));
    }

    #[test]
    fn second_press_keeps_the_shift_latch() {
        let mut view = TerminalView {
            modes: motion_modes(true, false),
            ..TerminalView::default()
        };
        // Shift+Left starts a local selection and latches Shift.
        view.begin_press(None, true);
        assert!(view.press_active);
        assert!(view.press_shift_override);
        // Shift is released, then Right is pressed during the same gesture.
        view.begin_press(None, false);
        assert!(view.press_shift_override);
        assert!(!reports_mouse(
            view.modes.mouse_reporting,
            view.press_shift_override,
            !view.reported.is_empty()
        ));
        assert_eq!(view.motion_report_with(3, 1, None, Mods::default()), None);
        // A new gesture latches the current Shift state again.
        view.end_press();
        view.begin_press(Some(MouseButton::Left), false);
        assert!(!view.press_shift_override);
        assert!(
            view.motion_report_with(3, 1, view.reported.latest(), Mods::default())
                .is_some()
        );
    }

    #[test]
    fn motion_dedupe_includes_shift() {
        let mut view = TerminalView {
            modes: motion_modes(false, true),
            ..TerminalView::default()
        };
        view.begin_press(Some(MouseButton::Left), false);
        assert!(
            view.motion_report_with(3, 1, view.reported.latest(), Mods::default())
                .is_some()
        );
        // Shift going down over the same cell is a new report, not a duplicate.
        let shift = Mods {
            shift: true,
            ..Mods::default()
        };
        assert_eq!(
            view.motion_report_with(3, 1, view.reported.latest(), shift)
                .as_deref(),
            Some(b"\x1b[<36;4;2M".as_slice())
        );
        assert_eq!(
            view.motion_report_with(3, 1, view.reported.latest(), shift),
            None
        );
    }

    #[test]
    fn lost_release_clamps_to_the_shrunk_grid() {
        let mut view = TerminalView {
            modes: motion_modes(true, false),
            ..TerminalView::default()
        };
        view.begin_press(Some(MouseButton::Left), false);
        // Reported in a large grid that has since shrunk to 10x5.
        view.last_report = Some((10, 40));
        assert_eq!(
            view.lost_release_reports((0, 0), Mods::default(), (10, 5)),
            vec![b"\x1b[<0;10;5m".to_vec()]
        );
    }

    #[test]
    fn motion_wanted_at_matches_the_report_path() {
        let rect = Rect::from_min_size(pos2(8.0, 16.0), vec2(90.0, 36.0));
        let cell = pos2(12.5, 25.0);
        let mut view = TerminalView {
            modes: motion_modes(true, false),
            grid_rect: Some(rect),
            grid_metrics: Some(metrics()),
            grid_cells: Some((10, 2)),
            ..TerminalView::default()
        };
        // Nothing reported yet: a move would write.
        assert!(view.motion_wanted_at(cell));
        view.motion_cell = Some(((0, 0), MouseButton::None, false, false, false));
        assert!(!view.motion_wanted_at(cell));
        assert!(view.motion_wanted_at(pos2(21.5, 25.0)));
        // A modifier change over the same cell is a new report.
        view.last_mods = Mods {
            ctrl: true,
            ..Mods::default()
        };
        assert!(view.motion_wanted_at(cell));
        view.last_mods = Mods::default();
        // The padding gutter is not a cell.
        assert!(!view.motion_wanted_at(pos2(4.0, 25.0)));
        // Shift with no reported press suppresses motion.
        view.last_mods = Mods {
            shift: true,
            ..Mods::default()
        };
        assert!(!view.motion_wanted_at(pos2(21.5, 25.0)));
        view.last_mods = Mods::default();
        // Click-only mode never wants motion.
        view.modes = motion_modes(false, false);
        assert!(!view.motion_wanted_at(pos2(21.5, 25.0)));
        // Drag-only mode needs a tracked button.
        view.modes = motion_modes(false, true);
        assert!(!view.motion_wanted_at(pos2(21.5, 25.0)));
        view.reported.insert(MouseButton::Left);
        assert!(view.motion_wanted_at(pos2(21.5, 25.0)));
        // A held button clamps through the gutter exactly like the report path,
        // so a drag that leaves the grid still needs a paint.
        assert!(view.motion_wanted_at(pos2(4.0, 25.0)));
        assert!(view.motion_wanted_at(pos2(120.0, 25.0)));
        // No geometry recorded yet: the caller paints.
        let bare = TerminalView {
            modes: motion_modes(true, false),
            ..TerminalView::default()
        };
        assert!(bare.motion_wanted_at(cell));
    }

    #[test]
    fn focus_flip_clears_the_hover_dedupe() {
        let mut view = TerminalView {
            modes: motion_modes(true, false),
            ..TerminalView::default()
        };
        view.observe_focus(true);
        assert!(view.motion_report(3, 1, None).is_some());
        // Unchanged focus keeps the dedupe.
        view.observe_focus(true);
        assert_eq!(view.motion_report(3, 1, None), None);
        // Losing focus drops it, so regaining it re-reports the same cell.
        view.observe_focus(false);
        assert!(view.motion_report(3, 1, None).is_some());
    }

    #[test]
    fn mode_toggle_clears_the_hover_dedupe() {
        let mut view = TerminalView {
            modes: motion_modes(true, false),
            ..TerminalView::default()
        };
        view.observe_mouse_modes(view.modes);
        assert!(view.motion_report(3, 1, None).is_some());
        // A repaint with unchanged modes keeps the dedupe.
        view.observe_mouse_modes(view.modes);
        assert_eq!(view.motion_report(3, 1, None), None);
        // Disabling and re-enabling tracking drops it, so hover re-reports.
        view.observe_mouse_modes(motion_modes(false, false));
        view.observe_mouse_modes(motion_modes(true, false));
        assert!(view.motion_report(3, 1, None).is_some());
    }

    #[test]
    fn motion_ignores_the_padding_gutter() {
        let rect = Rect::from_min_size(pos2(8.0, 16.0), vec2(90.0, 36.0));
        let mut view = TerminalView {
            modes: motion_modes(true, false),
            ..TerminalView::default()
        };
        let cell = pos2(8.0 + 4.5, 16.0 + 9.0);
        let mods = Mods::default();
        assert!(
            view.motion_report_at(cell, rect, &metrics(), (10, 2), None, mods)
                .is_some()
        );
        // A stationary pointer is deduped.
        assert_eq!(
            view.motion_report_at(cell, rect, &metrics(), (10, 2), None, mods),
            None
        );
        // The gutter left of the grid is not a cell and reports nothing.
        assert_eq!(
            view.motion_report_at(pos2(7.0, 20.0), rect, &metrics(), (10, 2), None, mods),
            None
        );
        // Re-entering the cell reports again.
        assert!(
            view.motion_report_at(cell, rect, &metrics(), (10, 2), None, mods)
                .is_some()
        );
    }

    #[test]
    fn motion_dedupe_includes_ctrl_and_alt() {
        let mut view = TerminalView {
            modes: motion_modes(true, false),
            ..TerminalView::default()
        };
        assert!(view.motion_report(3, 1, None).is_some());
        let ctrl = Mods {
            ctrl: true,
            ..Mods::default()
        };
        assert_eq!(
            view.motion_report_with(3, 1, None, ctrl).as_deref(),
            Some(b"\x1b[<51;4;2M".as_slice())
        );
        assert_eq!(view.motion_report_with(3, 1, None, ctrl), None);
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
            true,
        );
        assert_eq!(cursor, Some(b"\x1bOA".to_vec()));
        assert_ne!(cursor, Some(b"\x1b[A".to_vec()));

        let pasted = view.encode_input(
            &[Event::Paste("hello".to_string())],
            Modifiers::default(),
            modes,
            true,
        );
        assert_eq!(pasted, Some(b"\x1b[200~hello\x1b[201~".to_vec()));
    }

    #[test]
    fn shift_enter_follows_the_newline_setting() {
        let shift = Modifiers {
            shift: true,
            ..Modifiers::default()
        };
        let view = TerminalView::default();
        let enter = [key_event(EguiKey::Enter, shift)];

        assert_eq!(
            view.encode_input(&enter, Modifiers::default(), TermModes::default(), true),
            Some(vec![0x1b, 0x0d])
        );
        assert_eq!(
            view.encode_input(&enter, Modifiers::default(), TermModes::default(), false),
            Some(vec![0x0d])
        );
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
        TerminalView::default().encode_input(
            &events,
            Modifiers::default(),
            TermModes::default(),
            true,
        )
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
