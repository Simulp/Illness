//! In-window Mica/Acrylic backdrop.
//!
//! Native DWM transparency is not available through the wgpu surface used by
//! eframe (the DX12 surface reports no transparent composite alpha mode), so
//! Illness paints a blurred desktop wallpaper behind its semi-transparent
//! terminal and chrome. It looks like Mica and, unlike a transparent window,
//! works identically on every Windows 10/11 GPU backend.

use eframe::egui::{Color32, ColorImage, Context, TextureHandle, TextureOptions};
use illness_core::Theme;
use std::path::PathBuf;

/// Load or synthesize the blurred wallpaper backdrop once.
pub fn build(ctx: &Context, theme: &Theme) -> TextureHandle {
    let image = wallpaper().unwrap_or_else(|| gradient(theme));
    ctx.load_texture("illness_backdrop", image, TextureOptions::LINEAR)
}

fn wallpaper() -> Option<ColorImage> {
    #[cfg(target_os = "windows")]
    {
        let mut candidates = Vec::new();
        if let Some(appdata) = std::env::var_os("APPDATA") {
            candidates.push(
                PathBuf::from(appdata)
                    .join("Microsoft")
                    .join("Windows")
                    .join("Themes")
                    .join("TranscodedWallpaper"),
            );
        }
        if let Some(windir) = std::env::var_os("WINDIR") {
            let web = PathBuf::from(windir).join("Web").join("Wallpaper");
            candidates.push(web.join("Windows").join("img0.jpg"));
            candidates.push(web.join("Theme1").join("img1.jpg"));
            candidates.push(web.join("Theme2").join("img2.jpg"));
        }

        for path in candidates {
            if let Ok(image) = image::open(&path) {
                return Some(blurred(image));
            }
        }
    }

    None
}

fn blurred(image: image::DynamicImage) -> ColorImage {
    // Low resolution + a real blur keeps startup cheap and makes the backdrop
    // read as a soft material rather than a stretched photograph.
    let small = image
        .resize_exact(640, 360, image::imageops::FilterType::Triangle)
        .to_rgba8();
    let blurred = image::imageops::blur(&small, 9.0);
    ColorImage::from_rgba_unmultiplied([640, 360], blurred.as_raw())
}

fn gradient(theme: &Theme) -> ColorImage {
    const SIZE: usize = 512;
    let bg = theme.bg;
    let accent = theme.accent;
    let mut pixels = Vec::with_capacity(SIZE * SIZE);
    for y in 0..SIZE {
        for x in 0..SIZE {
            let dx = x as f32 / SIZE as f32 - 0.25;
            let dy = y as f32 / SIZE as f32 - 0.35;
            let glow = (1.0 - (dx * dx + dy * dy).sqrt() * 1.7).clamp(0.0, 1.0);
            let amount = glow * 0.16;
            let blend = |base: u8, highlight: u8| {
                (f32::from(base) * (1.0 - amount) + f32::from(highlight) * amount) as u8
            };
            pixels.push(Color32::from_rgb(
                blend(bg.r, accent.r),
                blend(bg.g, accent.g),
                blend(bg.b, accent.b),
            ));
        }
    }
    ColorImage::new([SIZE, SIZE], pixels)
}
