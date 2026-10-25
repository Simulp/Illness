//! In-window Mica/Acrylic backdrop.
//!
//! Native DWM transparency is not available through the wgpu surface used by
//! eframe (the DX12 surface reports no transparent composite alpha mode), so
//! Illness paints a blurred desktop wallpaper behind its semi-transparent
//! terminal and chrome. It looks like Mica and, unlike a transparent window,
//! works identically on every Windows 10/11 GPU backend.

use eframe::egui::{Color32, ColorImage, Context, TextureHandle, TextureOptions};
use illness_core::config::BackgroundMode;
use illness_core::{Material, Theme};
use std::path::{Path, PathBuf};

use crate::platform;

/// Tint alpha painted over the backdrop. `opacity` 0 hides the image behind a
/// solid theme colour; 100 keeps today's material strength.
pub fn tint_alpha(material: Material, opacity: f32) -> u8 {
    let base: u8 = match material {
        Material::Mica => 90,
        Material::Acrylic => 50,
        Material::Solid => 255,
    };
    let opacity = if opacity.is_finite() {
        opacity.clamp(0.0, 100.0) / 100.0
    } else {
        1.0
    };
    let alpha = 255.0 + (f32::from(base) - 255.0) * opacity;
    alpha.round().clamp(0.0, 255.0) as u8
}

/// Load or synthesize the blurred backdrop for `mode`.
///
/// `None` means the caller should paint no backdrop: the mode opts out, a
/// custom image is missing, or no image could be loaded. Desktop mode always
/// yields a texture because it falls back to a theme gradient.
pub fn build(
    ctx: &Context,
    theme: &Theme,
    mode: BackgroundMode,
    path: &str,
) -> Option<TextureHandle> {
    let image = match mode {
        BackgroundMode::Desktop => Some(desktop_image(theme)),
        BackgroundMode::Image => custom_image(path),
        BackgroundMode::None => None,
    }?;
    Some(ctx.load_texture("illness_backdrop", image, TextureOptions::LINEAR))
}

/// Ordered wallpaper sources for Desktop mode: the live wallpaper first, then
/// the last transcoded copy, and finally the theme gradient. The stock Windows
/// `Web\Wallpaper` files are deliberately absent: they must only be used when
/// the OS reports them as the current wallpaper.
fn desktop_wallpaper_candidates(
    reported: Option<PathBuf>,
    transcoded: Option<PathBuf>,
) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(reported) = reported {
        candidates.push(reported);
    }
    if let Some(transcoded) = transcoded
        && !candidates.contains(&transcoded)
    {
        candidates.push(transcoded);
    }
    candidates
}

fn desktop_image(theme: &Theme) -> ColorImage {
    let transcoded = std::env::var_os("APPDATA").map(|appdata| {
        PathBuf::from(appdata)
            .join("Microsoft")
            .join("Windows")
            .join("Themes")
            .join("TranscodedWallpaper")
    });
    let candidates = desktop_wallpaper_candidates(platform::current_wallpaper_path(), transcoded);

    candidates
        .iter()
        .find_map(|path| load_blurred(path))
        .unwrap_or_else(|| gradient(theme))
}

fn custom_image(path: &str) -> Option<ColorImage> {
    let path = Path::new(path.trim());
    if path.as_os_str().is_empty() {
        return None;
    }
    load_blurred(path)
}

fn load_blurred(path: &Path) -> Option<ColorImage> {
    match image::open(path) {
        Ok(image) => Some(blurred(image)),
        Err(error) => {
            log::debug!("background {} unavailable: {error}", path.display());
            None
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opacity_zero_is_solid_and_full_keeps_material_strength() {
        for material in [Material::Mica, Material::Acrylic, Material::Solid] {
            assert_eq!(tint_alpha(material, 0.0), 255);
        }
        assert_eq!(tint_alpha(Material::Mica, 100.0), 90);
        assert_eq!(tint_alpha(Material::Acrylic, 100.0), 50);
        assert_eq!(tint_alpha(Material::Solid, 100.0), 255);
    }

    #[test]
    fn tint_alpha_interpolates_and_clamps() {
        assert_eq!(tint_alpha(Material::Mica, 50.0), 173);
        assert_eq!(tint_alpha(Material::Acrylic, 50.0), 153);
        assert_eq!(tint_alpha(Material::Mica, -20.0), 255);
        assert_eq!(tint_alpha(Material::Mica, 400.0), 90);
        assert_eq!(tint_alpha(Material::Mica, f32::NAN), 90);
    }

    #[test]
    fn desktop_candidates_prefer_the_reported_wallpaper() {
        let reported = PathBuf::from(r"C:\Pictures\wall paper.png");
        let transcoded = PathBuf::from(
            r"C:\Users\me\AppData\Roaming\Microsoft\Windows\Themes\TranscodedWallpaper",
        );
        let candidates =
            desktop_wallpaper_candidates(Some(reported.clone()), Some(transcoded.clone()));

        assert_eq!(candidates, vec![reported, transcoded]);
        assert!(candidates.iter().all(|path| {
            !path
                .to_string_lossy()
                .to_ascii_lowercase()
                .contains(r"\web\wallpaper")
        }));
    }

    #[test]
    fn desktop_candidates_fall_back_to_transcoded() {
        let transcoded = PathBuf::from("transcoded");
        assert_eq!(
            desktop_wallpaper_candidates(None, Some(transcoded.clone())),
            vec![transcoded]
        );
        assert!(desktop_wallpaper_candidates(None, None).is_empty());
    }

    #[test]
    fn desktop_candidates_do_not_repeat_the_same_path() {
        let path = PathBuf::from("same.jpg");
        assert_eq!(
            desktop_wallpaper_candidates(Some(path.clone()), Some(path.clone())),
            vec![path]
        );
    }
}
