//! Immutable view of a terminal, built once per frame.
//!
//! Rendering code consumes [`Snapshot`] values and never touches the terminal lock.

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::cell::Flags as AlacrittyFlags;
use alacritty_terminal::vte::ansi::{Color, NamedColor};
use illness_core::CursorKind;

use crate::session::TermModes;

/// Requested terminal dimensions in cells.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GridSize {
    pub cols: usize,
    pub rows: usize,
}

impl GridSize {
    pub const fn new(cols: usize, rows: usize) -> Self {
        Self { cols, rows }
    }
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.rows
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

/// Cell attribute bits exposed to the UI.
///
/// The bit layout matches alacritty's cell flags exactly, so `bits()` can be compared against
/// the raw values the renderer documents, without forcing `illness-ui` to depend on
/// `alacritty_terminal`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct CellFlags(u16);

impl CellFlags {
    pub const INVERSE: Self = Self(0x0001);
    pub const BOLD: Self = Self(0x0002);
    pub const ITALIC: Self = Self(0x0004);
    pub const BOLD_ITALIC: Self = Self(Self::BOLD.0 | Self::ITALIC.0);
    pub const UNDERLINE: Self = Self(0x0008);
    pub const WRAPLINE: Self = Self(0x0010);
    pub const WIDE_CHAR: Self = Self(0x0020);
    pub const WIDE_CHAR_SPACER: Self = Self(0x0040);
    pub const DIM: Self = Self(0x0080);
    pub const DIM_BOLD: Self = Self(Self::DIM.0 | Self::BOLD.0);
    pub const HIDDEN: Self = Self(0x0100);
    pub const STRIKEOUT: Self = Self(0x0200);
    /// Alacritty spells this `STRIKEOUT`; the renderer-facing name is `STRIKETHROUGH`.
    pub const STRIKETHROUGH: Self = Self(0x0200);
    pub const LEADING_WIDE_CHAR_SPACER: Self = Self(0x0400);
    pub const DOUBLE_UNDERLINE: Self = Self(0x0800);
    pub const UNDERCURL: Self = Self(0x1000);
    pub const DOTTED_UNDERLINE: Self = Self(0x2000);
    pub const DASHED_UNDERLINE: Self = Self(0x4000);
    pub const ALL_UNDERLINES: Self = Self(
        Self::UNDERLINE.0
            | Self::DOUBLE_UNDERLINE.0
            | Self::UNDERCURL.0
            | Self::DOTTED_UNDERLINE.0
            | Self::DASHED_UNDERLINE.0,
    );

    /// All flags understood by [`CellFlags`].
    pub const ALL: Self = Self(0x7fff);

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn bits(self) -> u16 {
        self.0
    }

    pub const fn from_bits_truncate(bits: u16) -> Self {
        Self(bits & Self::ALL.0)
    }

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl std::ops::BitOr for CellFlags {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl From<AlacrittyFlags> for CellFlags {
    fn from(flags: AlacrittyFlags) -> Self {
        Self(flags.bits())
    }
}

/// Backwards-compatible alias for [`CellFlags`].
pub use self::CellFlags as Flags;

/// A single rendered cell.
#[derive(Debug, Clone, Copy)]
pub struct SnapshotCell {
    pub ch: char,
    pub fg: Color,
    pub bg: Color,
    pub flags: CellFlags,
    /// Session-local id resolved through [`crate::Session::hyperlink`].
    pub hyperlink: Option<u32>,
    /// Start offset into [`Snapshot::combining`] for zero-width marks.
    pub combining_start: u32,
    /// Number of zero-width marks stored for this cell.
    pub combining_len: u8,
}

impl Default for SnapshotCell {
    fn default() -> Self {
        Self {
            ch: ' ',
            fg: Color::Named(NamedColor::Foreground),
            bg: Color::Named(NamedColor::Background),
            flags: CellFlags::empty(),
            hyperlink: None,
            combining_start: 0,
            combining_len: 0,
        }
    }
}

/// Cursor state for one frame.
#[derive(Debug, Clone, Copy, Default)]
pub struct Cursor {
    pub col: usize,
    pub row: usize,
    pub kind: CursorKind,
    pub blinking: bool,
    pub visible: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SelectionType {
    #[default]
    Simple,
    Block,
    Semantic,
    Lines,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Side {
    #[default]
    Left,
    Right,
}

/// One row of a selection, inclusive, in viewport coordinates.
///
/// `start` and `end` are `(row, column)` pairs. Multi-row selections are emitted as one span per
/// visible row so the renderer can fill each row directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SelectionSpan {
    pub start: (usize, usize),
    pub end: (usize, usize),
}

/// Everything the UI needs to draw the viewport.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub cols: usize,
    pub rows: usize,
    /// Viewport cells in row-major order, `cols * rows` entries. Wide-character spacers are kept.
    pub cells: Vec<SnapshotCell>,
    /// Zero-width chars (combining marks) referenced by [`SnapshotCell::combining_start`].
    pub combining: Vec<char>,
    pub cursor: Cursor,
    pub selection: Vec<SelectionSpan>,
    pub display_offset: usize,
    pub history_size: usize,
    pub modes: TermModes,
    /// Window title from OSC 0/2, if the child set one.
    pub title: Option<String>,
}

impl Snapshot {
    pub fn empty(cols: usize, rows: usize) -> Self {
        Self {
            cols,
            rows,
            cells: vec![SnapshotCell::default(); cols.saturating_mul(rows)],
            ..Self::default()
        }
    }

    pub fn cell(&self, col: usize, row: usize) -> Option<&SnapshotCell> {
        if col >= self.cols || row >= self.rows {
            return None;
        }
        self.cells.get(row * self.cols + col)
    }

    /// Zero-width marks that belong to `cell`.
    pub fn zero_width(&self, cell: &SnapshotCell) -> &[char] {
        let start = cell.combining_start as usize;
        let end = start.saturating_add(cell.combining_len as usize);
        self.combining.get(start..end).unwrap_or(&[])
    }
}
