#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod backdrop;
mod platform;

use eframe::egui;
use eframe::egui_wgpu::{WgpuConfiguration, WgpuSetup};
use eframe::wgpu;
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

fn main() -> eframe::Result<()> {
    init_logging();
    log::info!("Illness {} starting", env!("CARGO_PKG_VERSION"));

    let result = run();
    if let Err(error) = &result {
        log::error!("startup failed: {error}");
    }
    result
}

fn run() -> eframe::Result<()> {
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Illness")
        .with_inner_size([1180.0, 760.0])
        .with_min_inner_size([720.0, 480.0])
        .with_decorations(false)
        .with_transparent(false)
        .with_has_shadow(false)
        .with_resizable(true);
    if let Some(icon) = load_icon() {
        viewport = viewport.with_icon(icon);
    }

    // Terminal mask glyphs need dual-source blending for true per-channel
    // ClearType coverage. Request it when the adapter offers it; the GPU text
    // compositor checks the resulting device feature and falls back to CPU
    // text otherwise.
    let mut wgpu_options = WgpuConfiguration::default()
        .with_surface_config(eframe::egui_wgpu::SurfaceConfig::LOW_LATENCY);
    if let WgpuSetup::CreateNew(setup) = &mut wgpu_options.wgpu_setup {
        let base = Arc::clone(&setup.device_descriptor);
        setup.device_descriptor = Arc::new(move |adapter| {
            let mut descriptor = base(adapter);
            if adapter
                .features()
                .contains(wgpu::Features::DUAL_SOURCE_BLENDING)
            {
                descriptor.required_features |= wgpu::Features::DUAL_SOURCE_BLENDING;
            }
            descriptor
        });
    }

    let options = eframe::NativeOptions {
        viewport,
        renderer: eframe::Renderer::Wgpu,
        wgpu_options,
        // Keep Ctrl+C/V/X as terminal input. The app handles Ctrl+Shift+C/V
        // itself; letting egui-winit intercept them would swallow Ctrl+C
        // (0x03) and Ctrl+V (0x16) before the session sees them.
        clipboard_shortcuts: false,
        // Text is alpha-blended at 1:1 physical pixels; dithering adds noise to glyph edges.
        dithering: false,
        ..Default::default()
    };

    eframe::run_native(
        "Illness",
        options,
        Box::new(|cc| Ok(Box::new(app::IllnessApp::new(cc)))),
    )
}
