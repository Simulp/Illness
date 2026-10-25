#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod backdrop;
mod host;
mod platform;

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::sync::Arc;

/// Initialise stderr logging in debug builds and a file log in release builds,
/// where no console exists. Logging failures must never block startup.
fn init_logging() {
    #[cfg(debug_assertions)]
    env_logger::init();

    #[cfg(not(debug_assertions))]
    install_file_logger();
}

/// Append release logs to `%LOCALAPPDATA%\illness\illness.log`. If the file
/// cannot be opened the app still starts; only diagnostics are lost.
#[cfg(not(debug_assertions))]
fn install_file_logger() {
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::sync::Mutex;

    struct FileLogger {
        file: Mutex<std::fs::File>,
    }

    impl log::Log for FileLogger {
        fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
            metadata.level() <= log::max_level()
        }

        fn log(&self, record: &log::Record<'_>) {
            if !self.enabled(record.metadata()) {
                return;
            }
            let Ok(mut file) = self.file.lock() else {
                return;
            };
            let seconds = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_secs())
                .unwrap_or(0);
            let _ = writeln!(
                file,
                "{seconds} [{}] {}: {}",
                record.level(),
                record.target(),
                record.args()
            );
        }

        fn flush(&self) {
            if let Ok(mut file) = self.file.lock() {
                let _ = file.flush();
            }
        }
    }

    let directory = std::env::var_os("LOCALAPPDATA")
        .map(std::path::PathBuf::from)
        .map(|base| base.join("illness"))
        .unwrap_or_else(|| std::env::temp_dir().join("illness"));
    if std::fs::create_dir_all(&directory).is_err() {
        return;
    }
    let path = directory.join("illness.log");
    let Ok(file) = OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    if log::set_boxed_logger(Box::new(FileLogger {
        file: Mutex::new(file),
    }))
    .is_ok()
    {
        // Default to info. `ILLNESS_LOG=trace` enables presentation-rate and
        // surface-cost measurements in release builds.
        let level = std::env::var("ILLNESS_LOG")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(log::LevelFilter::Info);
        log::set_max_level(level);
    }
}

/// Decode the bundled PNG icon; a missing or corrupt icon must not prevent startup.
fn load_icon() -> Option<egui::IconData> {
    const ICON_PNG: &[u8] = include_bytes!("../../../assets/illness.png");

    match image::load_from_memory(ICON_PNG) {
        Ok(image) => {
            let rgba = image.to_rgba8();
            let (width, height) = rgba.dimensions();
            Some(egui::IconData {
                rgba: rgba.into_raw(),
                width,
                height,
            })
        }
        Err(error) => {
            log::warn!("failed to decode assets/illness.png: {error}");
            None
        }
    }
}

/// What the command line asks the process to do.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Startup {
    /// Open the window. `folder` is the first positional directory argument.
    Run { folder: Option<PathBuf> },
    /// Register the per-user Explorer verbs without opening a window.
    RegisterContextMenu,
    /// Remove the per-user Explorer verbs without opening a window.
    UnregisterContextMenu,
}

/// Parse the command line, ignoring the executable path.
///
/// Arguments are `OsString`s so non-Unicode paths survive. A positional
/// argument is only used when it resolves to a directory; files and flags are
/// ignored rather than reported as errors. The two registry flags are
/// maintenance entry points for the installer and uninstaller.
fn parse_command_line(args: impl IntoIterator<Item = OsString>) -> Startup {
    let args: Vec<OsString> = args.into_iter().collect();
    if args
        .iter()
        .any(|arg| arg.to_str() == Some("--unregister-context-menu"))
    {
        return Startup::UnregisterContextMenu;
    }
    if args
        .iter()
        .any(|arg| arg.to_str() == Some("--register-context-menu"))
    {
        return Startup::RegisterContextMenu;
    }
    Startup::Run {
        folder: args.iter().find_map(|arg| directory_argument(arg)),
    }
}

/// Interpret one positional argument as a startup folder.
///
/// Explorer passes the placeholder as a quoted command-line argument. Windows
/// normally strips those quotes, but wrappers can leave literal quote marks,
/// and a quoted drive root such as `"C:\"` loses its final backslash to
/// command-line escaping. Directory names cannot contain `"`, so trimming
/// stray quotes is safe; restore the root slash when that trim leaves a bare
/// drive letter that still carries the quote artifact.
#[cfg(target_os = "windows")]
fn directory_argument(argument: &OsStr) -> Option<PathBuf> {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};

    const QUOTE: u16 = b'"' as u16;
    let mut units: Vec<u16> = argument.encode_wide().collect();
    let mut removed_trailing_quote = false;
    while units.last() == Some(&QUOTE) {
        units.pop();
        removed_trailing_quote = true;
    }
    while units.first() == Some(&QUOTE) {
        units.remove(0);
    }

    let mut path = PathBuf::from(OsString::from_wide(&units));
    if removed_trailing_quote
        && let Some(text) = path.to_str()
        && is_bare_drive(text)
    {
        path = PathBuf::from(format!("{text}\\"));
    }
    path.is_dir().then_some(path)
}

/// A bare drive prefix such as `C:` with no root separator.
#[cfg(target_os = "windows")]
fn is_bare_drive(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

#[cfg(not(target_os = "windows"))]
fn directory_argument(argument: &OsStr) -> Option<PathBuf> {
    let path = PathBuf::from(argument);
    path.is_dir().then_some(path)
}

fn main() -> anyhow::Result<()> {
    init_logging();
    log::info!("Illness {} starting", env!("CARGO_PKG_VERSION"));

    match parse_command_line(std::env::args_os().skip(1)) {
        Startup::RegisterContextMenu => {
            if let Err(error) = platform::register_context_menu() {
                log::error!("context-menu registration failed: {error:#}");
                eprintln!("Illness context-menu registration failed: {error:#}");
                std::process::exit(1);
            }
            log::info!("context menu registered");
            Ok(())
        }
        Startup::UnregisterContextMenu => {
            if let Err(error) = platform::unregister_context_menu() {
                log::error!("context-menu removal failed: {error:#}");
                eprintln!("Illness context-menu removal failed: {error:#}");
                std::process::exit(1);
            }
            log::info!("context menu removed");
            Ok(())
        }
        Startup::Run { folder } => {
            // Explorer verbs are per-user and best-effort: a locked registry
            // must not prevent the terminal from starting. The explicit
            // `--register-context-menu` path above is the authoritative one.
            if let Err(error) = platform::register_context_menu() {
                log::warn!("could not register the folder context menu: {error:#}");
            }
            run(folder)
        }
    }
}

fn run(startup_folder: Option<PathBuf>) -> anyhow::Result<()> {
    let icon = load_icon().map(Arc::new);
    let result = host::run(icon, startup_folder);
    if let Err(error) = &result {
        log::error!("startup failed: {error}");
        // `host::run` already reports window/surface creation failures. The
        // event-loop build and run errors only surface here, so show the same
        // native dialog for them instead of failing silently in a GUI build.
        platform::show_startup_error(&format!("Illness could not start.\n\n{error}"));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::{Startup, directory_argument, parse_command_line};
    use std::ffi::{OsStr, OsString};
    use std::path::PathBuf;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn positional_directory_starts_there() {
        let temp = std::env::temp_dir();
        let temp_text = temp.to_string_lossy().into_owned();
        let startup = parse_command_line(args(&["--ignored-flag", "", &temp_text]));
        assert_eq!(startup, Startup::Run { folder: Some(temp) });
    }

    #[test]
    fn quoted_directory_with_spaces_is_unquoted() {
        let temp = std::env::temp_dir();
        let quoted = format!("\"{}\"", temp.display());
        assert_eq!(directory_argument(OsStr::new(&quoted)), Some(temp));
    }

    #[test]
    fn non_directories_and_unknown_flags_are_ignored() {
        let file = std::env::current_exe().expect("test executable");
        let startup = parse_command_line(vec![
            OsString::from("--not-a-registry-action"),
            file.into_os_string(),
        ]);
        assert_eq!(startup, Startup::Run { folder: None });
    }

    #[test]
    fn maintenance_flags_never_open_a_window() {
        let temp = std::env::temp_dir();
        assert_eq!(
            parse_command_line(vec![
                temp.clone().into_os_string(),
                OsString::from("--unregister-context-menu"),
            ]),
            Startup::UnregisterContextMenu
        );
        assert_eq!(
            parse_command_line(vec![
                OsString::from("--register-context-menu"),
                temp.into_os_string(),
            ]),
            Startup::RegisterContextMenu
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn quoted_drive_root_restores_the_separator() {
        // `cmd /c tool "C:\"` hands the child `C:"` because the backslash
        // escapes the closing quote. It still names the drive root.
        let path = directory_argument(OsStr::new("C:\"")).expect("drive root exists");
        assert_eq!(path, PathBuf::from(r"C:\"));
    }
}
