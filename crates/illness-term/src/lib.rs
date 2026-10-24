pub mod input;
pub mod session;
pub mod snapshot;

pub use alacritty_terminal::vte::ansi::Color;
pub use input::{
    Key, KeyInput, KeyModes, Mods, MouseButton, MousePhase, encode_key, encode_mouse, encode_paste,
};
pub use session::{Session, SessionEvent, SpawnSpec, TermModes};
pub use snapshot::{Cursor, GridSize, SelectionSpan, SelectionType, Side, Snapshot, SnapshotCell};
