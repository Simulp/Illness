pub mod config;
pub mod profiles;
pub mod theme;

pub use config::{Config, CursorKind, Material};
pub use profiles::{Profile, detect_profiles};
pub use theme::{Rgb, Theme};
