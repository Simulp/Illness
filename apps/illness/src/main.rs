mod app;
mod platform;

use eframe::egui;

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
    env_logger::init();

    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Illness")
        .with_inner_size([1180.0, 760.0])
        .with_min_inner_size([720.0, 480.0])
        .with_decorations(false)
        .with_transparent(true)
        .with_has_shadow(false)
        .with_resizable(true);
    if let Some(icon) = load_icon() {
        viewport = viewport.with_icon(icon);
    }

    let options = eframe::NativeOptions {
        viewport,
        renderer: eframe::Renderer::Wgpu,
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
