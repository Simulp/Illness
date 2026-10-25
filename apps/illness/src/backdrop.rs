//! In-window Mica/Acrylic backdrop.
//!
//! Mica and translucent image backgrounds still paint a blurred desktop
//! wallpaper behind the terminal and chrome, so they look identical on every
//! Windows 10/11 GPU backend. Acrylic instead uses native behind-window blur
//! (see `platform.rs`), so the desktop background is skipped for that material.

use egui::{Color32, ColorImage, Context, TextureHandle, TextureOptions};
use illness_core::config::BackgroundMode;
use illness_core::{Material, Theme};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

use crate::platform;

/// Largest accepted wallpaper side, matching common GPU texture limits.
pub const MAX_BACKDROP_SIDE: u32 = 8192;
/// Largest accepted wallpaper area (32 megapixels).
pub const MAX_BACKDROP_PIXELS: u64 = 32_000_000;

/// A loaded backdrop texture and whether it came from an image (as opposed to
/// the theme colour gradient). Theme changes only rebuild gradients.
pub struct Backdrop {
    pub texture: TextureHandle,
    pub from_image: bool,
}

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
/// `Ok(None)` means the caller should paint no backdrop. `Err` carries a
/// user-facing reason (missing or oversized image) for a toast. Desktop mode
/// never fails: it falls back to a theme gradient.
pub fn build(
    ctx: &Context,
    theme: &Theme,
    mode: BackgroundMode,
    path: &str,
) -> Result<Option<Backdrop>, String> {
    let (image, from_image) = match mode {
        BackgroundMode::Desktop => desktop_image(theme),
        BackgroundMode::Image => {
            if path.trim().is_empty() {
                return Err("Choose a background image".to_string());
            }
            let image = custom_image(path)?;
            (image, true)
        }
        BackgroundMode::None => return Ok(None),
    };
    Ok(Some(Backdrop {
        texture: ctx.load_texture("illness_backdrop", image, TextureOptions::LINEAR),
        from_image,
    }))
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

fn desktop_image(theme: &Theme) -> (ColorImage, bool) {
    let transcoded = std::env::var_os("APPDATA").map(|appdata| {
        PathBuf::from(appdata)
            .join("Microsoft")
            .join("Windows")
            .join("Themes")
            .join("TranscodedWallpaper")
    });
    let candidates = desktop_wallpaper_candidates(platform::current_wallpaper_path(), transcoded);

    for path in &candidates {
        match load_blurred(path) {
            Ok(image) => return (image, true),
            Err(error) => log::debug!("background {} unavailable: {error}", path.display()),
        }
    }
    (gradient(theme), false)
}

fn custom_image(path: &str) -> Result<ColorImage, String> {
    let path = Path::new(path.trim());
    if path.as_os_str().is_empty() {
        return Err("Choose a background image".to_string());
    }
    load_blurred(path)
}

/// Decode a wallpaper once per `(path, mtime)` and reuse the blurred result.
fn load_blurred(path: &Path) -> Result<ColorImage, String> {
    let modified = std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok();

    if let Some(cached) = cached_image(path, modified) {
        return Ok(cached);
    }

    let image = decode_image(path)?;
    let blurred = blurred(image);
    store_cached(path, modified, &blurred);
    Ok(blurred)
}

fn decode_image(path: &Path) -> Result<image::DynamicImage, String> {
    let (width, height) = image_dimensions(path)?;
    validate_dimensions(width, height)?;

    // `image::open` refuses files without an extension (the Windows
    // `TranscodedWallpaper` has none), so guess the format from the bytes.
    image::ImageReader::open(path)
        .map_err(|error| format!("Could not open \"{}\": {error}", path.display()))?
        .with_guessed_format()
        .map_err(|error| format!("Could not read \"{}\": {error}", path.display()))?
        .decode()
        .map_err(|error| format!("Could not decode \"{}\": {error}", path.display()))
}

fn image_dimensions(path: &Path) -> Result<(u32, u32), String> {
    image::ImageReader::open(path)
        .map_err(|error| format!("Could not open \"{}\": {error}", path.display()))?
        .with_guessed_format()
        .map_err(|error| format!("Could not read \"{}\": {error}", path.display()))?
        .into_dimensions()
        .map_err(|error| format!("Could not read \"{}\": {error}", path.display()))
}

/// Reject wallpapers that would allocate an unreasonable amount of memory.
pub fn validate_dimensions(width: u32, height: u32) -> Result<(), String> {
    let pixels = u64::from(width) * u64::from(height);
    if width > MAX_BACKDROP_SIDE || height > MAX_BACKDROP_SIDE || pixels > MAX_BACKDROP_PIXELS {
        return Err(format!(
            "Background is {width}×{height}; the limit is \
             {MAX_BACKDROP_SIDE} px per side and {} MP",
            MAX_BACKDROP_PIXELS / 1_000_000
        ));
    }
    Ok(())
}

struct CacheEntry {
    path: PathBuf,
    modified: Option<SystemTime>,
    image: ColorImage,
}

static CACHE: OnceLock<Mutex<Option<CacheEntry>>> = OnceLock::new();

fn cache() -> &'static Mutex<Option<CacheEntry>> {
    CACHE.get_or_init(|| Mutex::new(None))
}

fn cached_image(path: &Path, modified: Option<SystemTime>) -> Option<ColorImage> {
    let cache = cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let entry = cache.as_ref()?;
    (entry.path == path && entry.modified == modified).then(|| entry.image.clone())
}

fn store_cached(path: &Path, modified: Option<SystemTime>, image: &ColorImage) {
    let mut cache = cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *cache = Some(CacheEntry {
        path: path.to_path_buf(),
        modified,
        image: image.clone(),
    });
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
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "illness-backdrop-test-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

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
    fn dimensions_are_capped_by_side_and_area() {
        assert!(validate_dimensions(1920, 1080).is_ok());
        assert!(validate_dimensions(MAX_BACKDROP_SIDE, 100).is_ok());
        assert!(validate_dimensions(MAX_BACKDROP_SIDE + 1, 1).is_err());
        assert!(validate_dimensions(1, MAX_BACKDROP_SIDE + 1).is_err());
        assert!(validate_dimensions(8192, 4000).is_err(), "33 MP");
        assert!(
            validate_dimensions(8000, 4000).is_ok(),
            "exactly 32 MP is allowed"
        );
    }

    #[test]
    fn extensionless_wallpaper_decodes_with_a_guessed_format() {
        let dir = temp_dir();
        let path = dir.join("TranscodedWallpaper");
        let image = image::RgbaImage::from_pixel(8, 8, image::Rgba([20, 40, 60, 255]));
        image
            .save_with_format(&path, image::ImageFormat::Png)
            .expect("write extensionless png");

        let loaded = load_blurred(&path).expect("extensionless image should decode");
        assert_eq!(loaded.size, [640, 360]);

        let modified = std::fs::metadata(&path)
            .and_then(|metadata| metadata.modified())
            .ok();
        assert!(
            cached_image(&path, modified).is_some(),
            "decoded image should be cached by path and mtime"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn oversized_wallpaper_is_rejected_before_decoding() {
        let dir = temp_dir();
        let path = dir.join("huge.png");
        let image =
            image::RgbaImage::from_pixel(MAX_BACKDROP_SIDE + 1, 1, image::Rgba([0, 0, 0, 255]));
        image
            .save_with_format(&path, image::ImageFormat::Png)
            .expect("write wide png");

        let error = load_blurred(&path).expect_err("oversized image must be rejected");
        assert!(error.contains("limit"), "{error}");
        let _ = std::fs::remove_dir_all(dir);
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
