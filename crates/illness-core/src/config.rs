use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::profiles::Profile;
use crate::theme::Theme;

/// Window backdrop material.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Material {
    #[default]
    Mica,
    Acrylic,
    Solid,
}

/// Terminal cursor shape.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CursorKind {
    #[default]
    Block,
    Beam,
    Underline,
}

/// What is painted behind the terminal when the window material is translucent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackgroundMode {
    /// Follow the wallpaper Windows currently reports.
    #[default]
    Desktop,
    /// Use a user-chosen image.
    Image,
    /// Paint no backdrop; the theme background is solid.
    None,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub theme: String,
    pub font_family: String,
    pub font_size: f32,
    pub line_height: f32,
    pub material: Material,
    pub background: BackgroundMode,
    pub background_path: String,
    pub background_opacity: f32,
    pub cursor: CursorKind,
    pub cursor_blink: bool,
    pub scrollback: usize,
    pub padding: f32,
    pub default_profile: String,
    pub copy_on_select: bool,
    pub profiles: Vec<Profile>,
    /// Detected profile names the user removed or renamed away. They are
    /// filtered out when built-in profiles are merged on the next launch.
    pub hidden_profiles: Vec<String>,
    /// Draw terminal glyphs with the GPU compositor when a wgpu device is
    /// available. `false` always uses the CPU grayscale compositor.
    pub gpu_text: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            theme: "Illness Dark".to_string(),
            font_family: "Cascadia Mono".to_string(),
            font_size: 18.0,
            line_height: 1.2,
            material: Material::Mica,
            background: BackgroundMode::Desktop,
            background_path: String::new(),
            background_opacity: 100.0,
            cursor: CursorKind::Block,
            cursor_blink: true,
            scrollback: 10_000,
            padding: 8.0,
            default_profile: String::new(),
            copy_on_select: false,
            profiles: Vec::new(),
            hidden_profiles: Vec::new(),
            gpu_text: true,
        }
        .sanitized()
    }
}

impl Config {
    /// Load configuration from `%APPDATA%\illness\config.toml`.
    ///
    /// Missing or malformed files never fail; they log and yield defaults.
    pub fn load() -> Self {
        Self::load_from(&Self::path())
    }

    /// Load from an explicit path. Useful for tests and portable installs.
    pub fn load_from(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(contents) => match toml::from_str::<Self>(&contents) {
                // Duplicate names are only collapsed when loading: doing it on
                // every settings edit would delete a profile mid-rename.
                Ok(config) => config.sanitized().deduplicated(),
                Err(error) => {
                    log::warn!(
                        "failed to parse {}: {error}; using defaults",
                        path.display()
                    );
                    Self::default()
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(error) => {
                log::warn!("failed to read {}: {error}; using defaults", path.display());
                Self::default()
            }
        }
    }

    /// Persist configuration atomically.
    pub fn save(&self) -> Result<()> {
        self.save_to(&Self::path())
    }

    /// Persist to an explicit path, creating parent directories.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        let contents = toml::to_string_pretty(self).context("serializing configuration")?;

        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }

        let mut temp_name = path
            .file_name()
            .map(OsString::from)
            .unwrap_or_else(|| OsString::from("config.toml"));
        temp_name.push(format!(".{}.tmp", std::process::id()));
        let temp_path = path.with_file_name(temp_name);

        std::fs::write(&temp_path, contents)
            .with_context(|| format!("writing {}", temp_path.display()))?;

        if let Err(error) = std::fs::rename(&temp_path, path) {
            let _ = std::fs::remove_file(&temp_path);
            return Err(anyhow::Error::new(error).context(format!("replacing {}", path.display())));
        }

        Ok(())
    }

    /// `%APPDATA%\illness\config.toml`.
    pub fn path() -> PathBuf {
        directories::BaseDirs::new()
            .map(|dirs| dirs.config_dir().join("illness").join("config.toml"))
            .or_else(|| {
                env::var_os("APPDATA")
                    .map(|appdata| PathBuf::from(appdata).join("illness").join("config.toml"))
            })
            .unwrap_or_else(|| PathBuf::from("config.toml"))
    }

    /// Clamp values into supported ranges and resolve the theme.
    ///
    /// Profile names are deliberately left untouched: this runs on every
    /// settings frame, and deduplicating here would delete a profile while the
    /// user is still typing its new name. Uniqueness is enforced by the editor,
    /// while [`Self::deduplicated`] runs only when loading a file.
    pub fn sanitized(mut self) -> Self {
        if self.font_family.trim().is_empty() {
            self.font_family = "Cascadia Mono".to_string();
        } else {
            self.font_family = self.font_family.trim().to_string();
        }
        self.font_size = finite_or(self.font_size, 18.0).clamp(8.0, 40.0);
        self.line_height = finite_or(self.line_height, 1.2).clamp(1.0, 2.0);
        self.padding = finite_or(self.padding, 8.0).clamp(0.0, 32.0);
        self.background_path = self.background_path.trim().to_string();
        self.background_opacity = finite_or(self.background_opacity, 100.0).clamp(0.0, 100.0);
        self.scrollback = if self.scrollback == 0 {
            10_000
        } else {
            self.scrollback.min(200_000)
        };
        self.theme = Theme::by_name(self.theme.trim()).name.to_string();

        let mut seen: Vec<String> = Vec::new();
        self.hidden_profiles.retain(|name| {
            let name = name.trim();
            if name.is_empty()
                || seen
                    .iter()
                    .any(|existing| existing.eq_ignore_ascii_case(name))
            {
                false
            } else {
                seen.push(name.to_string());
                true
            }
        });

        self
    }

    /// Collapse case-insensitive duplicate profile names, keeping the first.
    ///
    /// This is a load/merge-time operation, never an edit-time one.
    pub fn deduplicated(mut self) -> Self {
        dedupe_profiles(&mut self.profiles);
        self
    }

    /// Rewrite `default_profile` when a profile is renamed from `old` to `new`.
    ///
    /// Keeping this in sync while the user types means a rename never leaves
    /// the default pointing at a name that no longer exists.
    pub fn note_profile_rename(&mut self, old: &str, new: &str) {
        let new = new.trim();
        if old.trim().is_empty() || new.is_empty() {
            return;
        }
        if self.default_profile.eq_ignore_ascii_case(old.trim()) {
            self.default_profile = new.to_string();
        }
    }

    /// Remember a detected profile name that should not be merged back in.
    pub fn hide_profile_name(&mut self, name: &str) {
        let name = name.trim();
        if name.is_empty()
            || self
                .hidden_profiles
                .iter()
                .any(|hidden| hidden.eq_ignore_ascii_case(name))
        {
            return;
        }
        self.hidden_profiles.push(name.to_string());
    }

    /// Whether `name` is already used by a profile other than `except`.
    pub fn profile_name_taken(&self, name: &str, except: Option<usize>) -> bool {
        let name = name.trim();
        !name.is_empty()
            && self.profiles.iter().enumerate().any(|(index, profile)| {
                Some(index) != except && profile.name.trim().eq_ignore_ascii_case(name)
            })
    }

    /// Whether every profile has a non-empty name. Empty names only exist
    /// transiently while a text field is being edited.
    pub fn has_empty_profile_name(&self) -> bool {
        self.profiles
            .iter()
            .any(|profile| profile.name.trim().is_empty())
    }
}

/// Collapse case-insensitive duplicate profile names, keeping the first.
pub fn dedupe_profiles(profiles: &mut Vec<Profile>) {
    let mut seen: Vec<String> = Vec::new();
    profiles.retain(|profile| {
        if seen
            .iter()
            .any(|name| name.eq_ignore_ascii_case(profile.name.trim()))
        {
            false
        } else {
            seen.push(profile.name.trim().to_string());
            true
        }
    });
}

fn finite_or(value: f32, fallback: f32) -> f32 {
    if value.is_finite() { value } else { fallback }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::theme::Rgb;

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> PathBuf {
        let dir = env::temp_dir().join(format!(
            "illness-core-test-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn sample_profile(name: &str) -> Profile {
        Profile {
            name: name.to_string(),
            program: "pwsh.exe".to_string(),
            args: Vec::new(),
            accent: Rgb::new(0x7e, 0xe7, 0x87),
        }
    }

    #[test]
    fn defaults_match_the_spec() {
        let config = Config::default();
        assert_eq!(config.theme, "Illness Dark");
        assert_eq!(config.font_family, "Cascadia Mono");
        assert_eq!(config.font_size, 18.0);
        assert_eq!(config.line_height, 1.2);
        assert_eq!(config.material, Material::Mica);
        assert_eq!(config.background, BackgroundMode::Desktop);
        assert!(config.background_path.is_empty());
        assert_eq!(config.background_opacity, 100.0);
        assert_eq!(config.cursor, CursorKind::Block);
        assert!(config.cursor_blink);
        assert_eq!(config.scrollback, 10_000);
        assert_eq!(config.padding, 8.0);
        assert!(config.default_profile.is_empty());
        assert!(!config.copy_on_select);
        assert!(config.profiles.is_empty());
        assert!(config.hidden_profiles.is_empty());
        assert!(config.gpu_text);
    }

    #[test]
    fn config_roundtrip() {
        let dir = temp_dir();
        let path = dir.join("nested").join("config.toml");
        let config = Config {
            theme: "Nord".to_string(),
            font_size: 15.5,
            line_height: 1.35,
            material: Material::Acrylic,
            background: BackgroundMode::Image,
            background_path: r"C:\Users\me\AppData\Roaming\illness\backgrounds\wall 1.png"
                .to_string(),
            background_opacity: 42.5,
            default_profile: "PowerShell".to_string(),
            profiles: vec![sample_profile("PowerShell")],
            ..Config::default()
        };

        config.save_to(&path).expect("save config");
        config.save_to(&path).expect("replace config");
        assert_eq!(
            std::fs::read_dir(path.parent().expect("parent"))
                .expect("read dir")
                .count(),
            1,
            "save should leave only the config file"
        );

        let loaded = Config::load_from(&path);
        assert_eq!(loaded, config);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_gpu_text_defaults_to_enabled() {
        let parsed: Config = toml::from_str("theme = \"Nord\"\n").expect("parse config");
        assert!(parsed.gpu_text);
    }

    #[test]
    fn missing_file_returns_defaults() {
        let dir = temp_dir();
        let loaded = Config::load_from(&dir.join("absent.toml"));
        assert_eq!(loaded, Config::default());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn bad_toml_returns_defaults() {
        let dir = temp_dir();
        let path = dir.join("config.toml");
        std::fs::write(&path, "font_size = [\n").expect("write bad toml");
        let loaded = Config::load_from(&path);
        assert_eq!(loaded, Config::default());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn background_mode_serializes_lowercase() {
        let config = Config {
            background: BackgroundMode::Image,
            ..Config::default()
        };
        let text = toml::to_string_pretty(&config).expect("serialize config");
        assert!(text.contains("background = \"image\""), "{text}");

        let parsed: Config = toml::from_str(&text).expect("parse config");
        assert_eq!(parsed.background, BackgroundMode::Image);
        assert_eq!(parsed, config);
    }

    #[test]
    fn sanitize_clamps_background_opacity_and_path() {
        let low = Config {
            background_opacity: -12.0,
            background_path: "  C:\\wallpapers\\one.png  ".to_string(),
            ..Config::default()
        }
        .sanitized();
        assert_eq!(low.background_opacity, 0.0);
        assert_eq!(low.background_path, "C:\\wallpapers\\one.png");

        let high = Config {
            background_opacity: 250.0,
            ..Config::default()
        }
        .sanitized();
        assert_eq!(high.background_opacity, 100.0);

        let nan = Config {
            background_opacity: f32::NAN,
            ..Config::default()
        }
        .sanitized();
        assert_eq!(nan.background_opacity, 100.0);
    }

    #[test]
    fn sanitize_clamps_and_normalizes() {
        let config = Config {
            theme: "nord".to_string(),
            font_size: 2.0,
            line_height: 0.2,
            scrollback: 0,
            padding: -4.0,
            ..Config::default()
        }
        .sanitized();

        assert_eq!(config.theme, "Nord");
        assert_eq!(config.font_size, 8.0);
        assert_eq!(config.line_height, 1.0);
        assert_eq!(config.scrollback, 10_000);
        assert_eq!(config.padding, 0.0);

        let high = Config {
            font_size: 200.0,
            line_height: 3.0,
            scrollback: 999_999,
            padding: 64.0,
            ..Config::default()
        }
        .sanitized();

        assert_eq!(high.font_size, 40.0);
        assert_eq!(high.line_height, 2.0);
        assert_eq!(high.scrollback, 200_000);
        assert_eq!(high.padding, 32.0);
    }

    #[test]
    fn sanitize_keeps_duplicate_profiles_until_load_or_merge() {
        let config = Config {
            profiles: vec![sample_profile("PowerShell"), sample_profile("powershell")],
            ..Config::default()
        }
        .sanitized();
        assert_eq!(
            config.profiles.len(),
            2,
            "editing must not delete a profile with a colliding name"
        );

        let dir = temp_dir();
        let path = dir.join("config.toml");
        config.save_to(&path).expect("save duplicated config");
        let loaded = Config::load_from(&path);
        assert_eq!(loaded.profiles.len(), 1);
        assert_eq!(loaded.profiles[0].name, "PowerShell");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rename_updates_the_default_profile() {
        let mut config = Config {
            profiles: vec![
                sample_profile("PowerShell"),
                sample_profile("Command Prompt"),
            ],
            default_profile: "PowerShell".to_string(),
            ..Config::default()
        };

        config.note_profile_rename("PowerShell", "PS");
        assert_eq!(config.default_profile, "PS");

        // The caller passes both the previous committed name and the
        // edit-start name, so consecutive keystrokes keep following the rename.
        config.note_profile_rename("PS", "PS7");
        assert_eq!(config.default_profile, "PS7");

        config.note_profile_rename("Command Prompt", "cmd");
        assert_eq!(config.default_profile, "PS7");

        // Clearing a name mid-edit must not repoint the default.
        config.note_profile_rename("PS7", "");
        assert_eq!(config.default_profile, "PS7");
    }

    #[test]
    fn hidden_profile_names_are_unique_and_roundtrip() {
        let dir = temp_dir();
        let path = dir.join("config.toml");
        let mut config = Config::default();
        config.hide_profile_name("PowerShell");
        config.hide_profile_name(" powershell ");
        config.hide_profile_name("Command Prompt");
        assert_eq!(config.hidden_profiles, ["PowerShell", "Command Prompt"]);

        config.save_to(&path).expect("save hidden profiles");
        let loaded = Config::load_from(&path);
        assert_eq!(loaded.hidden_profiles, config.hidden_profiles);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn duplicate_profile_names_are_reported() {
        let config = Config {
            profiles: vec![
                sample_profile("PowerShell"),
                sample_profile("Command Prompt"),
            ],
            ..Config::default()
        };
        assert!(config.profile_name_taken("powershell", Some(1)));
        assert!(!config.profile_name_taken("powershell", Some(0)));
        assert!(!config.profile_name_taken("", None));
    }

    #[test]
    fn sanitize_unknown_theme_falls_back() {
        let config = Config {
            theme: "not a theme".to_string(),
            ..Config::default()
        }
        .sanitized();
        assert_eq!(config.theme, "Illness Dark");
    }

    #[test]
    fn sanitize_font_family_keeps_a_name() {
        let empty = Config {
            font_family: "   ".to_string(),
            ..Config::default()
        }
        .sanitized();
        assert_eq!(empty.font_family, "Cascadia Mono");

        let named = Config {
            font_family: "  CaskaydiaCove Nerd Font  ".to_string(),
            ..Config::default()
        }
        .sanitized();
        assert_eq!(named.font_family, "CaskaydiaCove Nerd Font");
    }
}
