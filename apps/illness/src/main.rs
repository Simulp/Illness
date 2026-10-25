#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod backdrop;
mod host;
mod platform;

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

fn main() -> anyhow::Result<()> {
    init_logging();
    log::info!("Illness {} starting", env!("CARGO_PKG_VERSION"));

    let result = run();
    if let Err(error) = &result {
        log::error!("startup failed: {error}");
        // `host::run` already reports window/surface creation failures. The
        // event-loop build and run errors only surface here, so show the same
        // native dialog for them instead of failing silently in a GUI build.
        platform::show_startup_error(&format!("Illness could not start.\n\n{error}"));
    }
    result
}

fn run() -> anyhow::Result<()> {
    let icon = load_icon().map(Arc::new);
    host::run(icon)
}
