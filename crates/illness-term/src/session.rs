//! PTY-backed terminal session.
//!
//! One [`Session`] owns an alacritty [`Term`] behind a [`FairMutex`], the PTY event loop, and the
//! channel of [`SessionEvent`]s consumed by the UI.

use std::borrow::Cow;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Direction, Line, Point};
use alacritty_terminal::selection::{Selection, SelectionType as TermSelectionType};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::Hyperlink;
use alacritty_terminal::term::search::RegexSearch;
use alacritty_terminal::term::{Config as TermConfig, RenderableContent, Term, TermMode};
use alacritty_terminal::tty;
use alacritty_terminal::vte::ansi::{CursorShape, CursorStyle};
use anyhow::Context;
use crossbeam_channel::{Receiver, Sender, unbounded};
use illness_core::CursorKind;

use crate::snapshot::{
    Cursor, GridSize, SelectionSpan, SelectionType, Side, Snapshot, SnapshotCell,
};

/// Default scrollback used by [`SpawnSpec`].
const SCROLLBACK_LINES: usize = 10_000;

/// Description of the child process to launch.
#[derive(Debug, Clone)]
pub struct SpawnSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    /// Scrollback lines for the new terminal.
    pub scrollback: usize,
    /// Initial cursor shape, before any DECSCUSR override.
    pub cursor: CursorKind,
}

impl Default for SpawnSpec {
    fn default() -> Self {
        Self {
            program: String::new(),
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
            scrollback: SCROLLBACK_LINES,
            cursor: CursorKind::Block,
        }
    }
}

impl SpawnSpec {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            ..Self::default()
        }
    }
}

/// Terminal modes mirrored from `alacritty_terminal::term::TermMode`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TermModes {
    pub app_cursor: bool,
    pub bracketed_paste: bool,
    pub mouse_reporting: bool,
    pub sgr_mouse: bool,
    pub alternate_screen: bool,
    pub focus_reporting: bool,
}

/// Events emitted by the terminal and PTY event loop.
#[derive(Debug, Clone)]
pub enum SessionEvent {
    Title(String),
    ResetTitle,
    Bell,
    ClipboardStore(String),
    PtyWrite(Vec<u8>),
    /// Palette index to answer with the active theme color.
    ColorRequest(usize),
    /// The child asked for the text area size.
    TextAreaSizeRequest,
    ChildExit(i32),
    CursorBlinkingChange,
    MouseCursorDirty,
    Exit,
    Wakeup,
}

/// Forwards alacritty events into the UI channel and wakes the UI thread.
#[derive(Clone)]
struct EventProxy {
    events: Sender<SessionEvent>,
    waker: Arc<dyn Fn() + Send + Sync>,
    title: Arc<Mutex<Option<String>>>,
    exited: Arc<AtomicBool>,
}

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        let mapped = match event {
            Event::Title(title) => {
                set_title(&self.title, Some(title.clone()));
                Some(SessionEvent::Title(title))
            }
            Event::ResetTitle => {
                set_title(&self.title, None);
                Some(SessionEvent::ResetTitle)
            }
            Event::Bell => Some(SessionEvent::Bell),
            Event::ClipboardStore(_, text) => Some(SessionEvent::ClipboardStore(text)),
            Event::PtyWrite(text) => Some(SessionEvent::PtyWrite(text.into_bytes())),
            Event::ColorRequest(index, _) => Some(SessionEvent::ColorRequest(index)),
            Event::TextAreaSizeRequest(_) => Some(SessionEvent::TextAreaSizeRequest),
            Event::ChildExit(status) => {
                self.exited.store(true, Ordering::Release);
                Some(SessionEvent::ChildExit(status.code().unwrap_or(-1)))
            }
            Event::CursorBlinkingChange => Some(SessionEvent::CursorBlinkingChange),
            Event::MouseCursorDirty => Some(SessionEvent::MouseCursorDirty),
            Event::Exit => {
                self.exited.store(true, Ordering::Release);
                Some(SessionEvent::Exit)
            }
            Event::Wakeup => Some(SessionEvent::Wakeup),
            // OSC 52 paste is disabled by the default `Osc52::OnlyCopy`; nothing to answer.
            Event::ClipboardLoad(..) => None,
        };

        if let Some(event) = mapped {
            // A closed channel only means the UI dropped the session; the PTY must keep running
            // until the session itself is dropped.
            let _ = self.events.send(event);
        }
        (self.waker)();
    }
}

fn set_title(title: &Mutex<Option<String>>, value: Option<String>) {
    let mut guard = title
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *guard = value;
}

/// A live terminal session: PTY, child process, terminal state, and event channel.
pub struct Session {
    term: Arc<FairMutex<Term<EventProxy>>>,
    sender: Option<EventLoopSender>,
    events_tx: Sender<SessionEvent>,
    events_rx: Receiver<SessionEvent>,
    waker: Arc<dyn Fn() + Send + Sync>,
    title: Arc<Mutex<Option<String>>>,
    exited: Arc<AtomicBool>,
    hyperlinks: Mutex<HashMap<u32, String>>,
}

impl Session {
    /// Spawn a child process attached to a fresh ConPTY/PTY.
    ///
    /// `waker` is invoked from the event-loop thread for every terminal event, sometimes while
    /// the terminal lock is held. It must be cheap, thread-safe, and must not call back into
    /// [`Session`]; request a UI repaint instead.
    pub fn spawn(
        spec: SpawnSpec,
        grid: GridSize,
        cell_px: (u16, u16),
        waker: Arc<dyn Fn() + Send + Sync>,
    ) -> anyhow::Result<Self> {
        let grid = sanitized_grid(grid);
        let (events_tx, events_rx) = unbounded();
        let title = Arc::new(Mutex::new(None));
        let exited = Arc::new(AtomicBool::new(false));
        let proxy = EventProxy {
            events: events_tx.clone(),
            waker: waker.clone(),
            title: title.clone(),
            exited: exited.clone(),
        };

        let config = term_config(spec.scrollback, spec.cursor);
        let term = Term::new(config, &grid, proxy.clone());
        let term = Arc::new(FairMutex::new(term));

        let options = tty::Options {
            shell: Some(tty::Shell::new(spec.program, spec.args)),
            working_directory: spec.cwd,
            drain_on_exit: true,
            env: spec.env.into_iter().collect(),
            #[cfg(windows)]
            escape_args: true,
        };
        let window_size = window_size(grid, cell_px);
        let pty = tty::new(&options, window_size, 0).context("failed to create pty")?;
        let event_loop = EventLoop::new(term.clone(), proxy, pty, true, false)
            .context("failed to start terminal event loop")?;
        let sender = event_loop.channel();
        event_loop.spawn();

        Ok(Self {
            term,
            sender: Some(sender),
            events_tx,
            events_rx,
            waker,
            title,
            exited,
            hyperlinks: Mutex::new(HashMap::new()),
        })
    }

    /// Send raw bytes to the child process.
    pub fn write(&self, bytes: &[u8]) {
        self.send(Msg::Input(Cow::Owned(bytes.to_vec())));
    }

    /// Resize the terminal grid and the PTY.
    pub fn resize(&self, grid: GridSize, cell_px: (u16, u16)) {
        let grid = sanitized_grid(grid);
        {
            let mut term = self.term.lock();
            term.resize(grid);
        }
        self.send(Msg::Resize(window_size(grid, cell_px)));
    }

    /// Scroll the viewport by `delta` lines, positive toward the top.
    pub fn scroll(&self, delta: i32) {
        let mut term = self.term.lock();
        term.scroll_display(Scroll::Delta(delta));
    }

    /// Copy the visible viewport into an owned snapshot.
    pub fn snapshot(&self) -> Snapshot {
        let title = self.title();
        let mut hyperlinks = Vec::new();

        let snapshot = {
            let term = self.term.lock();
            let cols = term.columns();
            let rows = term.screen_lines();
            let history_size = term.history_size();

            let RenderableContent {
                display_iter,
                selection,
                cursor: render_cursor,
                display_offset,
                mode,
                ..
            } = term.renderable_content();

            let mut cells = Vec::with_capacity(cols.saturating_mul(rows));
            for indexed in display_iter {
                let cell = indexed.cell;
                let hyperlink = cell.hyperlink().map(|link| {
                    let id = hyperlink_id(&link);
                    hyperlinks.push((id, link.uri().to_owned()));
                    id
                });
                cells.push(SnapshotCell {
                    ch: cell.c,
                    fg: cell.fg,
                    bg: cell.bg,
                    flags: cell.flags.into(),
                    hyperlink,
                });
            }
            cells.resize(cols.saturating_mul(rows), SnapshotCell::default());

            let style = term.cursor_style();
            let cursor = cursor_from(
                render_cursor.shape,
                style.blinking,
                render_cursor.point,
                display_offset,
                cols,
                rows,
            );

            Snapshot {
                cols,
                rows,
                cells,
                cursor,
                selection: selection_spans(selection, display_offset, cols, rows),
                display_offset,
                history_size,
                modes: modes_from(mode),
                title,
            }
        };

        let mut guard = self
            .hyperlinks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.clear();
        guard.extend(hyperlinks);

        snapshot
    }

    /// Start a selection at a viewport position.
    pub fn selection_start(&self, kind: SelectionType, col: usize, row: usize, side: Side) {
        let mut term = self.term.lock();
        let display_offset = term.grid().display_offset() as i32;
        let point = Point::new(
            Line(row_to_line(row, display_offset)),
            Column(clamp_column(term.columns(), col)),
        );
        term.selection = Some(Selection::new(selection_type(kind), point, side_of(side)));
    }

    /// Move the active selection's endpoint to a viewport position.
    pub fn selection_update(&self, col: usize, row: usize, side: Side) {
        let mut term = self.term.lock();
        let display_offset = term.grid().display_offset() as i32;
        let point = Point::new(
            Line(row_to_line(row, display_offset)),
            Column(clamp_column(term.columns(), col)),
        );
        if let Some(selection) = term.selection.as_mut() {
            selection.update(point, side_of(side));
        }
    }

    pub fn selection_clear(&self) {
        let mut term = self.term.lock();
        term.selection = None;
    }

    pub fn selection_to_string(&self) -> Option<String> {
        let term = self.term.lock();
        term.selection_to_string()
    }

    /// Search for a literal string.
    ///
    /// `forward` searches from the top of the viewport, otherwise from the bottom. Returned
    /// coordinates are `(start_row, start_col, end_row, end_col)` in viewport space.
    pub fn search(&self, needle: &str, forward: bool) -> Option<(i32, usize, i32, usize)> {
        if needle.is_empty() {
            return None;
        }
        let pattern = escape_literal(needle);
        let mut regex = RegexSearch::new(&pattern).ok()?;

        let term = self.term.lock();
        let cols = term.columns();
        let rows = term.screen_lines();
        let display_offset = term.grid().display_offset() as i32;

        let (origin, direction, side) = if forward {
            (
                Point::new(Line(-display_offset), Column(0)),
                Direction::Right,
                Direction::Left,
            )
        } else {
            (
                Point::new(
                    Line(rows_to_line(rows, display_offset)),
                    Column(cols.saturating_sub(1)),
                ),
                Direction::Left,
                Direction::Right,
            )
        };

        let found = term.search_next(&mut regex, origin, direction, side, None)?;
        let start = *found.start();
        let end = *found.end();
        Some((
            start.line.0 + display_offset,
            start.column.0,
            end.line.0 + display_offset,
            end.column.0,
        ))
    }

    pub fn events(&self) -> &Receiver<SessionEvent> {
        &self.events_rx
    }

    /// Resolve a hyperlink id from the most recent snapshot.
    pub fn hyperlink(&self, id: u32) -> Option<String> {
        let guard = self
            .hyperlinks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.get(&id).cloned()
    }

    pub fn is_exited(&self) -> bool {
        self.exited.load(Ordering::Acquire)
    }

    /// Whether the child asked for focus in/out reports (DECSET 1004).
    pub fn focus_reporting(&self) -> bool {
        self.term.lock().mode().contains(TermMode::FOCUS_IN_OUT)
    }

    /// Set the window title locally and notify the UI.
    pub fn write_title(&self, title: &str) {
        set_title(&self.title, Some(title.to_owned()));
        let _ = self.events_tx.send(SessionEvent::Title(title.to_owned()));
        (self.waker)();
    }

    /// Current window title, if the child set one.
    pub fn title(&self) -> Option<String> {
        let guard = self
            .title
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.clone()
    }

    /// Ask the event loop to stop, which closes the PTY and ends the child.
    pub fn shutdown(&self) {
        self.send(Msg::Shutdown);
    }

    fn send(&self, message: Msg) {
        let Some(sender) = &self.sender else {
            return;
        };
        if let Err(err) = sender.send(message) {
            log::warn!("failed to send message to terminal event loop: {err}");
        }
    }

    #[cfg(test)]
    fn from_term_for_test_with_waker(
        cols: usize,
        rows: usize,
        waker: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        let grid = sanitized_grid(GridSize::new(cols, rows));
        let (events_tx, events_rx) = unbounded();
        let title = Arc::new(Mutex::new(None));
        let exited = Arc::new(AtomicBool::new(false));
        let proxy = EventProxy {
            events: events_tx.clone(),
            waker: waker.clone(),
            title: title.clone(),
            exited: exited.clone(),
        };
        let config = term_config(SCROLLBACK_LINES, CursorKind::Block);
        let term = Term::new(config, &grid, proxy);

        Self {
            term: Arc::new(FairMutex::new(term)),
            sender: None,
            events_tx,
            events_rx,
            waker,
            title,
            exited,
            hyperlinks: Mutex::new(HashMap::new()),
        }
    }

    /// Build an in-memory session without a PTY for tests.
    #[cfg(test)]
    fn from_term_for_test(cols: usize, rows: usize) -> Self {
        Self::from_term_for_test_with_waker(cols, rows, Arc::new(|| {}))
    }

    /// Feed raw VT bytes into the test terminal.
    #[cfg(test)]
    fn feed_for_test(&self, bytes: &[u8]) {
        let mut processor = alacritty_terminal::vte::ansi::Processor::<
            alacritty_terminal::vte::ansi::StdSyncHandler,
        >::new();
        let mut term = self.term.lock();
        processor.advance(&mut *term, bytes);
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn sanitized_grid(grid: GridSize) -> GridSize {
    // Alacritty requires at least two columns to hold a wide character.
    GridSize::new(grid.cols.max(2), grid.rows.max(1))
}

fn term_config(scrollback: usize, cursor: CursorKind) -> TermConfig {
    TermConfig {
        scrolling_history: scrollback.max(1),
        default_cursor_style: CursorStyle {
            shape: cursor_shape(cursor),
            blinking: true,
        },
        ..TermConfig::default()
    }
}

fn cursor_shape(cursor: CursorKind) -> CursorShape {
    match cursor {
        CursorKind::Block => CursorShape::Block,
        CursorKind::Beam => CursorShape::Beam,
        CursorKind::Underline => CursorShape::Underline,
    }
}

fn window_size(grid: GridSize, cell_px: (u16, u16)) -> WindowSize {
    WindowSize {
        num_lines: u16::try_from(grid.rows).unwrap_or(u16::MAX),
        num_cols: u16::try_from(grid.cols).unwrap_or(u16::MAX),
        cell_width: cell_px.0,
        cell_height: cell_px.1,
    }
}

fn clamp_column(cols: usize, col: usize) -> usize {
    col.min(cols.saturating_sub(1))
}

fn row_to_line(row: usize, display_offset: i32) -> i32 {
    i32::try_from(row)
        .unwrap_or(i32::MAX)
        .saturating_sub(display_offset)
}

fn rows_to_line(rows: usize, display_offset: i32) -> i32 {
    i32::try_from(rows.saturating_sub(1))
        .unwrap_or(i32::MAX)
        .saturating_sub(display_offset)
}

fn selection_type(kind: SelectionType) -> TermSelectionType {
    match kind {
        SelectionType::Simple => TermSelectionType::Simple,
        SelectionType::Block => TermSelectionType::Block,
        SelectionType::Semantic => TermSelectionType::Semantic,
        SelectionType::Lines => TermSelectionType::Lines,
    }
}

fn side_of(side: Side) -> Direction {
    match side {
        Side::Left => Direction::Left,
        Side::Right => Direction::Right,
    }
}

fn modes_from(mode: TermMode) -> TermModes {
    TermModes {
        app_cursor: mode.contains(TermMode::APP_CURSOR),
        bracketed_paste: mode.contains(TermMode::BRACKETED_PASTE),
        mouse_reporting: mode.intersects(TermMode::MOUSE_MODE),
        sgr_mouse: mode.contains(TermMode::SGR_MOUSE),
        alternate_screen: mode.contains(TermMode::ALT_SCREEN),
        focus_reporting: mode.contains(TermMode::FOCUS_IN_OUT),
    }
}

fn cursor_from(
    shape: CursorShape,
    blinking: bool,
    point: Point,
    display_offset: usize,
    cols: usize,
    rows: usize,
) -> Cursor {
    let row = point.line.0.saturating_add(display_offset as i32);
    let visible = shape != CursorShape::Hidden && row >= 0 && (row as usize) < rows;
    let kind = match shape {
        CursorShape::Underline => illness_core::CursorKind::Underline,
        CursorShape::Beam => illness_core::CursorKind::Beam,
        CursorShape::Block | CursorShape::HollowBlock | CursorShape::Hidden => {
            illness_core::CursorKind::Block
        }
    };
    Cursor {
        col: clamp_column(cols, point.column.0),
        row: usize::try_from(row)
            .unwrap_or(0)
            .min(rows.saturating_sub(1)),
        kind,
        blinking,
        visible,
    }
}

fn selection_spans(
    selection: Option<alacritty_terminal::selection::SelectionRange>,
    display_offset: usize,
    cols: usize,
    rows: usize,
) -> Vec<SelectionSpan> {
    let Some(range) = selection else {
        return Vec::new();
    };
    if cols == 0 || rows == 0 {
        return Vec::new();
    }

    let offset = i32::try_from(display_offset).unwrap_or(i32::MAX);
    let start_row = range.start.line.0.saturating_add(offset);
    let end_row = range.end.line.0.saturating_add(offset);
    if end_row < 0 || start_row >= rows as i32 {
        return Vec::new();
    }

    let first = start_row.max(0) as usize;
    let last = end_row.min(rows as i32 - 1) as usize;
    let start_col = range.start.column.0.min(cols - 1);
    let end_col = range.end.column.0.min(cols - 1);

    (first..=last)
        .map(|row| {
            let row_start = if range.is_block || (row == first && start_row >= 0) {
                start_col
            } else {
                0
            };
            let row_end = if range.is_block || (row == last && end_row < rows as i32) {
                end_col
            } else {
                cols - 1
            };
            SelectionSpan {
                start: (row, row_start.min(row_end)),
                end: (row, row_end.max(row_start)),
            }
        })
        .collect()
}

fn hyperlink_id(link: &Hyperlink) -> u32 {
    if let Ok(id) = link.id().parse::<u32>() {
        return id;
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    link.id().hash(&mut hasher);
    hasher.finish() as u32
}

fn escape_literal(needle: &str) -> String {
    let mut escaped = String::with_capacity(needle.len());
    for ch in needle.chars() {
        if matches!(
            ch,
            '\\' | '.' | '+' | '*' | '?' | '(' | ')' | '|' | '[' | ']' | '{' | '}' | '^' | '$'
        ) {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use alacritty_terminal::vte::ansi::Rgb;
    use illness_core::CursorKind;

    use super::*;
    use crate::snapshot::{CellFlags, SelectionType, Side};

    fn session(cols: usize, rows: usize) -> Session {
        Session::from_term_for_test(cols, rows)
    }

    #[test]
    fn test_snapshot_shape() {
        let snapshot = session(80, 24).snapshot();
        assert_eq!(snapshot.cols, 80);
        assert_eq!(snapshot.rows, 24);
        assert_eq!(snapshot.cells.len(), 80 * 24);
        assert_eq!(snapshot.cells.len(), 1920);
        assert!(snapshot.selection.is_empty());
        assert!(!snapshot.cursor.visible || snapshot.cursor.row < 24);
    }

    #[test]
    fn test_color_passthrough() {
        let session = session(20, 5);
        session.feed_for_test(b"\x1b[38;2;10;20;30mX");
        let snapshot = session.snapshot();
        let cell = snapshot.cell(0, 0).expect("cell");
        assert_eq!(cell.ch, 'X');
        assert_eq!(cell.fg, spec(10, 20, 30));
    }

    fn spec(r: u8, g: u8, b: u8) -> alacritty_terminal::vte::ansi::Color {
        alacritty_terminal::vte::ansi::Color::Spec(Rgb { r, g, b })
    }

    #[test]
    fn test_selection_range() {
        let session = session(20, 5);
        session.feed_for_test(b"abc");
        session.selection_start(SelectionType::Simple, 0, 0, Side::Left);
        session.selection_update(3, 0, Side::Left);

        assert_eq!(session.selection_to_string().as_deref(), Some("abc"));
        let spans = session.snapshot().selection;
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].start, (0, 0));
        assert_eq!(spans[0].end, (0, 2));

        session.selection_clear();
        assert_eq!(session.selection_to_string(), None);
    }

    #[test]
    fn cell_attributes_and_wide_cells_are_mirrored() {
        // Bit values are part of the contract with the renderer.
        assert_eq!(CellFlags::INVERSE.bits(), 0x0001);
        assert_eq!(CellFlags::BOLD.bits(), 0x0002);
        assert_eq!(CellFlags::ITALIC.bits(), 0x0004);
        assert_eq!(CellFlags::UNDERLINE.bits(), 0x0008);
        assert_eq!(CellFlags::WIDE_CHAR.bits(), 0x0020);
        assert_eq!(CellFlags::WIDE_CHAR_SPACER.bits(), 0x0040);
        assert_eq!(CellFlags::DIM.bits(), 0x0080);
        assert_eq!(CellFlags::HIDDEN.bits(), 0x0100);
        assert_eq!(CellFlags::STRIKETHROUGH.bits(), 0x0200);
        assert_eq!(CellFlags::ALL_UNDERLINES.bits(), 0x7808);

        let session = session(20, 5);
        session.feed_for_test(b"\x1b[1;2;3;4;7;8;9mX");
        let flags = session.snapshot().cell(0, 0).expect("styled cell").flags;
        for flag in [
            CellFlags::BOLD,
            CellFlags::ITALIC,
            CellFlags::UNDERLINE,
            CellFlags::STRIKETHROUGH,
            CellFlags::INVERSE,
            CellFlags::DIM,
            CellFlags::HIDDEN,
        ] {
            assert!(flags.contains(flag), "missing {flag:?}");
        }

        let wide_session = Session::from_term_for_test(20, 5);
        wide_session.feed_for_test("中".as_bytes());
        let snapshot = wide_session.snapshot();
        let wide = snapshot.cell(0, 0).expect("wide cell");
        assert_eq!(wide.ch, '中');
        assert!(wide.flags.contains(CellFlags::WIDE_CHAR));
        let spacer = snapshot.cell(1, 0).expect("spacer cell");
        assert!(spacer.flags.contains(CellFlags::WIDE_CHAR_SPACER));
    }

    #[test]
    fn cells_are_copied_row_major() {
        let session = session(20, 5);
        session.feed_for_test(b"ab\r\ncd");
        let snapshot = session.snapshot();
        assert_eq!(snapshot.cell(0, 0).map(|cell| cell.ch), Some('a'));
        assert_eq!(snapshot.cell(1, 0).map(|cell| cell.ch), Some('b'));
        assert_eq!(snapshot.cell(0, 1).map(|cell| cell.ch), Some('c'));
        assert_eq!(snapshot.cell(1, 1).map(|cell| cell.ch), Some('d'));
        assert_eq!(snapshot.cell(19, 4).map(|cell| cell.ch), Some(' '));
    }

    #[test]
    fn block_selection_emits_one_span_per_row() {
        let session = session(20, 5);
        session.feed_for_test(b"abcdef\r\nghijkl");
        session.selection_start(SelectionType::Block, 1, 0, Side::Left);
        session.selection_update(3, 1, Side::Right);

        let spans = session.snapshot().selection;
        assert_eq!(
            spans,
            vec![
                SelectionSpan {
                    start: (0, 1),
                    end: (0, 3)
                },
                SelectionSpan {
                    start: (1, 1),
                    end: (1, 3)
                },
            ]
        );
    }

    #[test]
    fn test_title_event() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let session = Session::from_term_for_test_with_waker(
            20,
            5,
            Arc::new(move || {
                counter.fetch_add(1, Ordering::Relaxed);
            }),
        );
        session.feed_for_test(b"\x1b]2;illness\x07");

        assert_eq!(session.title().as_deref(), Some("illness"));
        assert!(calls.load(Ordering::Relaxed) > 0);
        let events: Vec<_> = session.events().try_iter().collect();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, SessionEvent::Title(title) if title == "illness"))
        );
    }

    #[test]
    fn test_resize_reflows() {
        let session = session(80, 24);
        session.feed_for_test(b"hello world");
        session.resize(GridSize::new(40, 10), (8, 16));

        let snapshot = session.snapshot();
        assert_eq!(snapshot.cols, 40);
        assert_eq!(snapshot.rows, 10);
        assert_eq!(snapshot.cells.len(), 400);
        let row: String = snapshot.cells[..11].iter().map(|cell| cell.ch).collect();
        assert_eq!(row, "hello world");
    }

    #[test]
    fn literal_search_finds_matches() {
        let session = session(40, 5);
        session.feed_for_test(b"hello a.b world");

        assert_eq!(session.search("a.b", true), Some((0, 6, 0, 8)));
        assert_eq!(session.search("a.b", false), Some((0, 6, 0, 8)));
        assert_eq!(session.search("aXb", true), None);
        assert_eq!(session.search("world", true), Some((0, 10, 0, 14)));
        assert_eq!(session.search("", true), None);
    }

    #[test]
    fn scrollback_maps_selection_into_viewport() {
        let session = session(20, 5);
        for _ in 0..10 {
            session.feed_for_test(b"line\r\n");
        }
        session.scroll(3);
        let snapshot = session.snapshot();
        assert_eq!(snapshot.display_offset, 3);
        assert!(snapshot.history_size >= 3);

        let top_row = snapshot.cells[..4]
            .iter()
            .map(|cell| cell.ch)
            .collect::<String>();
        assert_eq!(top_row, "line");

        session.selection_start(SelectionType::Simple, 0, 0, Side::Left);
        session.selection_update(4, 0, Side::Left);
        assert_eq!(session.selection_to_string().as_deref(), Some("line"));
        let spans = session.snapshot().selection;
        assert_eq!(
            spans,
            vec![SelectionSpan {
                start: (0, 0),
                end: (0, 3)
            }]
        );
    }

    #[test]
    fn modes_reflect_terminal_sequences() {
        let session = session(20, 5);
        session.feed_for_test(b"\x1b[?1h\x1b[?2004h\x1b[?1000h\x1b[?1006h\x1b[?1049h");
        let modes = session.snapshot().modes;
        assert!(modes.app_cursor);
        assert!(modes.bracketed_paste);
        assert!(modes.mouse_reporting);
        assert!(modes.sgr_mouse);
        assert!(modes.alternate_screen);
    }

    #[test]
    fn focus_reporting_tracks_declaration_mode() {
        let session = session(20, 5);
        assert!(!session.focus_reporting());
        session.feed_for_test(b"\x1b[?1004h");
        assert!(session.focus_reporting());
        assert!(session.snapshot().modes.focus_reporting);
        session.feed_for_test(b"\x1b[?1004l");
        assert!(!session.focus_reporting());
    }

    #[test]
    fn spawn_spec_defaults_and_term_config() {
        let spec = SpawnSpec::default();
        assert_eq!(spec.scrollback, SCROLLBACK_LINES);
        assert_eq!(spec.cursor, CursorKind::Block);

        let config = term_config(2_500, CursorKind::Beam);
        assert_eq!(config.scrolling_history, 2_500);
        assert_eq!(config.default_cursor_style.shape, CursorShape::Beam);
    }

    #[test]
    fn hyperlink_ids_resolve() {
        let session = session(20, 5);
        session.feed_for_test(b"\x1b]8;;https://example.com\x1b\\X\x1b]8;;\x1b\\");
        let snapshot = session.snapshot();
        let id = snapshot
            .cell(0, 0)
            .and_then(|cell| cell.hyperlink)
            .expect("hyperlink");
        assert_eq!(
            session.hyperlink(id).as_deref(),
            Some("https://example.com")
        );
    }

    #[test]
    fn cursor_defaults_to_block() {
        let session = session(20, 5);
        let snapshot = session.snapshot();
        assert_eq!(snapshot.cursor.kind, CursorKind::Block);
        assert!(snapshot.cursor.visible);
    }

    #[test]
    #[ignore = "spawns a real ConPTY process"]
    fn test_child_exit() {
        let session = Session::spawn(
            SpawnSpec {
                program: "cmd.exe".to_owned(),
                args: vec![
                    "/c".to_owned(),
                    "echo illness & timeout /t 1 >nul & exit 3".to_owned(),
                ],
                cwd: None,
                env: Vec::new(),
                scrollback: 10_000,
                cursor: CursorKind::Block,
            },
            GridSize::new(80, 24),
            (8, 16),
            Arc::new(|| {}),
        )
        .expect("spawn cmd.exe");

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut exit_code = None;
        let mut text = String::new();
        while Instant::now() < deadline {
            while let Ok(event) = session.events().try_recv() {
                if let SessionEvent::ChildExit(code) = event {
                    exit_code = Some(code);
                }
            }
            text = session
                .snapshot()
                .cells
                .iter()
                .map(|cell| cell.ch)
                .collect::<String>();
            if exit_code.is_some() && text.contains("illness") {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        assert_eq!(exit_code, Some(3));
        assert!(text.contains("illness"), "snapshot text was {text:?}");
        assert!(session.is_exited());
    }
}
