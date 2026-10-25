//! In-window backdrop for translucent materials.
//!
//! Mica/Image backgrounds paint a blurred wallpaper behind the terminal and
//! chrome, tinted by the configured opacity. Acrylic+Desktop keeps a prewarmed
//! texture but never paints it: `platform.rs` applies native behind-window blur
//! instead.

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

/// What a UI-thread backdrop build could do for this material/mode pair.
pub enum BackdropBuild {
    /// A paintable texture is ready (or this pair paints nothing).
    Ready(Option<Backdrop>),
    /// The blurred image is not cached yet. A background warm was started by
    /// the app; paint native Mica/theme until it reports back.
    Warming,
    /// The image is unusable and `String` is the user-facing reason.
    Failed(String),
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

/// Build a backdrop without decoding on the UI thread.
///
/// Desktop prefers a cached wallpaper, then a cached theme gradient. A cold
/// wallpaper reports [`BackdropBuild::Warming`] so the caller can start a
/// detached [`warm_cache`] and paint the native material meanwhile. Image mode
/// is cache-only: a warmed image uploads in a few milliseconds, a cold one
/// warms in the background.
pub fn build(ctx: &Context, theme: &Theme, mode: BackgroundMode, path: &str) -> BackdropBuild {
    match mode {
        BackgroundMode::None => BackdropBuild::Ready(None),
        BackgroundMode::Desktop => {
            if let Some((image, from_image)) = cached_desktop(theme) {
                return BackdropBuild::Ready(Some(upload(ctx, image, from_image)));
            }
            let candidates = desktop_candidates();
            if candidates.is_empty() {
                // No wallpaper source is reported; a theme gradient is cheap
                // and is the only thing this machine can paint.
                let image = gradient(theme);
                cache_put(gradient_key(theme), image.clone());
                return BackdropBuild::Ready(Some(upload(ctx, image, false)));
            }
            BackdropBuild::Warming
        }
        BackgroundMode::Image => {
            let trimmed = path.trim();
            if trimmed.is_empty() {
                return BackdropBuild::Failed("Choose a background image".to_string());
            }
            match cached_image(&image_key(Path::new(trimmed))) {
                Some(image) => BackdropBuild::Ready(Some(upload(ctx, image, true))),
                None => BackdropBuild::Warming,
            }
        }
    }
}

fn upload(ctx: &Context, image: ColorImage, from_image: bool) -> Backdrop {
    Backdrop {
        texture: ctx.load_texture("illness_backdrop", image, TextureOptions::LINEAR),
        from_image,
    }
}

/// Decode and blur `mode` into the shared cache without creating a texture.
///
/// This is the detached-thread prewarm used while the effective material does
/// not paint an in-window backdrop (Solid, fallback Solid, Acrylic+Desktop).
/// A process-wide lock serializes decodes, so two warm requests for the same
/// image reuse the first result instead of decoding twice. `theme` is used for
/// the gradient fallback when Desktop reports no usable wallpaper.
pub fn warm_cache(theme: &Theme, mode: BackgroundMode, path: &str) -> Result<(), String> {
    if mode == BackgroundMode::None {
        return Ok(());
    }

    // Background-only: holding this lock never blocks a UI paint, and the
    // cache lookup after acquiring it makes duplicate requests cheap.
    let _warm = warm_lock();
    match mode {
        BackgroundMode::Desktop => {
            for candidate in desktop_candidates() {
                if cached_image(&image_key(&candidate)).is_some() {
                    return Ok(());
                }
                match load_blurred(&candidate) {
                    Ok(_) => return Ok(()),
                    Err(error) => {
                        log::debug!("background {} unavailable: {error}", candidate.display())
                    }
                }
            }
            let key = gradient_key(theme);
            if cached_key(&key).is_none() {
                cache_put(key, gradient(theme));
            }
            Ok(())
        }
        BackgroundMode::Image => {
            let trimmed = path.trim();
            if trimmed.is_empty() {
                return Err("Choose a background image".to_string());
            }
            let path = Path::new(trimmed);
            if cached_image(&image_key(path)).is_some() {
                return Ok(());
            }
            load_blurred(path).map(|_| ())
        }
        BackgroundMode::None => Ok(()),
    }
}

/// Whether [`build`] can paint this pair right now without a decode. `None`
/// and a desktop with no wallpaper source are immediately paintable.
pub fn is_cached(theme: &Theme, mode: BackgroundMode, path: &str) -> bool {
    match mode {
        BackgroundMode::None => true,
        BackgroundMode::Desktop => {
            cached_desktop(theme).is_some() || desktop_candidates().is_empty()
        }
        BackgroundMode::Image => {
            let trimmed = path.trim();
            !trimmed.is_empty() && cached_image(&image_key(Path::new(trimmed))).is_some()
        }
    }
}

fn cached_desktop(theme: &Theme) -> Option<(ColorImage, bool)> {
    for candidate in desktop_candidates() {
        if let Some(image) = cached_image(&image_key(&candidate)) {
            return Some((image, true));
        }
    }
    cached_key(&gradient_key(theme)).map(|image| (image, false))
}

fn desktop_candidates() -> Vec<PathBuf> {
    let transcoded = std::env::var_os("APPDATA").map(|appdata| {
        PathBuf::from(appdata)
            .join("Microsoft")
            .join("Windows")
            .join("Themes")
            .join("TranscodedWallpaper")
    });
    desktop_wallpaper_candidates(platform::current_wallpaper_path(), transcoded)
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

/// Decode a wallpaper once per `(path, mtime)` and reuse the blurred result.
fn load_blurred(path: &Path) -> Result<ColorImage, String> {
    let key = image_key(path);
    if let Some(cached) = cached_key(&key) {
        return Ok(cached);
    }

    #[cfg(test)]
    DECODE_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let image = decode_image(path)?;
    let blurred = blurred(image);
    cache_put(key, blurred.clone());
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

/// Cache identity: `(image path, mtime)` for decoded wallpapers, or the theme
/// name for a synthesized gradient. Blurred/downscaled images are ~0.9 MB, so a
/// small LRU keeps a Desktop wallpaper alive across an Image load without
/// retaining full-size decodes.
#[derive(Clone, PartialEq, Eq)]
enum CacheKey {
    Image {
        path: PathBuf,
        modified: Option<SystemTime>,
    },
    Gradient {
        theme: String,
    },
}

struct CacheEntry {
    key: CacheKey,
    image: ColorImage,
}

/// Bounded LRU: entries are ordered least- to most-recently used.
const CACHE_CAPACITY: usize = 4;

static CACHE: OnceLock<Mutex<Vec<CacheEntry>>> = OnceLock::new();
static WARM_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
static DECODE_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn cache() -> &'static Mutex<Vec<CacheEntry>> {
    CACHE.get_or_init(|| Mutex::new(Vec::new()))
}

fn warm_lock() -> std::sync::MutexGuard<'static, ()> {
    WARM_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn image_key(path: &Path) -> CacheKey {
    let modified = std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok();
    CacheKey::Image {
        path: path.to_path_buf(),
        modified,
    }
}

fn gradient_key(theme: &Theme) -> CacheKey {
    CacheKey::Gradient {
        theme: theme.name.to_string(),
    }
}

fn cached_key(key: &CacheKey) -> Option<ColorImage> {
    let mut cache = cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let index = cache.iter().position(|entry| &entry.key == key)?;
    let entry = cache.remove(index);
    let image = entry.image.clone();
    cache.push(entry);
    Some(image)
}

fn cached_image(key: &CacheKey) -> Option<ColorImage> {
    cached_key(key)
}

fn cache_put(key: CacheKey, image: ColorImage) {
    let mut cache = cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    cache.retain(|entry| entry.key != key);
    cache.push(CacheEntry { key, image });
    while cache.len() > CACHE_CAPACITY {
        cache.remove(0);
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
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Instant;

    use super::*;

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);
    /// Serialize cache-sensitive tests: the cache and decode counter are
    /// process-wide and cargo runs tests in parallel.
    static TEST_CACHE_LOCK: Mutex<()> = Mutex::new(());

    fn test_cache_lock() -> std::sync::MutexGuard<'static, ()> {
        TEST_CACHE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

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
        let _test = test_cache_lock();
        let dir = temp_dir();
        let path = dir.join("TranscodedWallpaper");
        let image = image::RgbaImage::from_pixel(8, 8, image::Rgba([20, 40, 60, 255]));
        image
            .save_with_format(&path, image::ImageFormat::Png)
            .expect("write extensionless png");

        let loaded = load_blurred(&path).expect("extensionless image should decode");
        assert_eq!(loaded.size, [640, 360]);

        assert!(
            cached_key(&image_key(&path)).is_some(),
            "decoded image should be cached by path and mtime"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn oversized_wallpaper_is_rejected_before_decoding() {
        let _test = test_cache_lock();
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
    fn duplicate_warms_decode_once() {
        let _test = test_cache_lock();
        let dir = temp_dir();
        let path = dir.join("shared.png");
        let image = image::RgbaImage::from_fn(1920, 1080, |x, y| {
            image::Rgba([(x % 256) as u8, (y % 256) as u8, 90, 255])
        });
        image.save(&path).expect("write image");
        let text = path.to_string_lossy().into_owned();
        let theme = Theme::by_name("Illness Dark");
        let before = DECODE_COUNT.load(Ordering::Relaxed);

        let workers: Vec<_> = (0..4)
            .map(|_| {
                let text = text.clone();
                std::thread::spawn(move || warm_cache(theme, BackgroundMode::Image, &text))
            })
            .collect();
        for worker in workers {
            worker.join().expect("warm thread").expect("warm image");
        }

        assert_eq!(
            DECODE_COUNT.load(Ordering::Relaxed) - before,
            1,
            "concurrent warms must share one decode"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cache_is_a_small_lru() {
        let _test = test_cache_lock();
        for index in 0..=CACHE_CAPACITY {
            let key = CacheKey::Image {
                path: PathBuf::from(format!("lru-{index}.png")),
                modified: None,
            };
            cache_put(
                key,
                ColorImage::new([1, 1], vec![Color32::from_rgb(index as u8, 0, 0)]),
            );
        }
        let first = CacheKey::Image {
            path: PathBuf::from("lru-0.png"),
            modified: None,
        };
        assert!(cached_key(&first).is_none(), "oldest entry is evicted");
        let last = CacheKey::Image {
            path: PathBuf::from(format!("lru-{CACHE_CAPACITY}.png")),
            modified: None,
        };
        assert!(cached_key(&last).is_some());
    }

    #[test]
    fn warmed_image_upload_never_decodes_on_the_ui_path() {
        let _test = test_cache_lock();
        let dir = temp_dir();
        let path = dir.join("warm.png");
        let image = image::RgbaImage::from_fn(2560, 1440, |x, y| {
            image::Rgba([(x % 256) as u8, (y % 256) as u8, 40, 255])
        });
        image.save(&path).expect("write image");
        let text = path.to_string_lossy().into_owned();
        let theme = Theme::by_name("Illness Dark");
        let ctx = Context::default();

        warm_cache(theme, BackgroundMode::Image, &text).expect("warm image");
        let before = DECODE_COUNT.load(Ordering::Relaxed);
        let started = Instant::now();
        let build = build(&ctx, theme, BackgroundMode::Image, &text);
        let elapsed = started.elapsed();

        assert!(matches!(build, BackdropBuild::Ready(Some(_))));
        assert_eq!(
            DECODE_COUNT.load(Ordering::Relaxed),
            before,
            "the UI-thread build must only upload the warmed image"
        );
        println!("warmed image upload: {elapsed:?}");
        assert!(
            elapsed.as_millis() < 50,
            "warmed upload took {elapsed:?}, expected a short upload"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn warmed_desktop_switch_never_decodes_on_the_ui_path() {
        let _test = test_cache_lock();
        let theme = Theme::by_name("Illness Dark");
        let ctx = Context::default();

        warm_cache(theme, BackgroundMode::Desktop, "").expect("warm desktop");
        assert!(is_cached(theme, BackgroundMode::Desktop, ""));
        let before = DECODE_COUNT.load(Ordering::Relaxed);
        let started = Instant::now();
        let build = build(&ctx, theme, BackgroundMode::Desktop, "");
        let elapsed = started.elapsed();

        assert!(matches!(build, BackdropBuild::Ready(Some(_))));
        assert_eq!(
            DECODE_COUNT.load(Ordering::Relaxed),
            before,
            "a warmed Acrylic+Desktop -> Mica+Desktop switch must not decode"
        );
        println!("warmed desktop upload: {elapsed:?}");
        assert!(
            elapsed.as_millis() < 50,
            "warmed upload took {elapsed:?}, expected a short upload"
        );
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
