//! In-window backdrop for translucent materials.
//!
//! Mica/Image backgrounds paint a blurred wallpaper behind the terminal and
//! chrome, tinted by the configured opacity. Acrylic+Desktop keeps a prewarmed
//! texture but never paints it: `platform.rs` applies native behind-window blur
//! instead.
//!
//! UI-thread cache checks are path-only and never touch the filesystem. A
//! detached warm resolves the path's mtime, decodes and blurs it, then uploads
//! on the next frame. Solid/fallback and None start no warm at all: a backdrop
//! that will never paint must never be decoded.

use egui::{Color32, ColorImage, Context, TextureHandle, TextureOptions};
use illness_core::config::BackgroundMode;
use illness_core::{Material, Theme};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use crate::platform;

/// Largest accepted wallpaper side, matching common GPU texture limits.
pub const MAX_BACKDROP_SIDE: u32 = 8192;
/// Largest accepted wallpaper area (32 megapixels).
pub const MAX_BACKDROP_PIXELS: u64 = 32_000_000;

/// How long a failed Desktop candidate is skipped before a rebuild may retry it.
/// This breaks the "cached lower candidate -> warm preferred -> fails -> rebuild"
/// loop without masking a wallpaper that becomes available later.
const DESKTOP_FAILURE_COOLDOWN: Duration = Duration::from_secs(10);

/// Error string returned when a superseded warm exits. The app treats it as
/// stale rather than a user-visible image failure.
pub const WARM_CANCELLED: &str = "background warm cancelled";

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

/// Cancellation handle for one detached warm.
///
/// The app cancels the previous token when the background, path or effective
/// material changes. A warm checks it before opening or decoding, so a stale
/// request is dropped instead of queued behind unrelated work.
#[derive(Clone, Debug, Default)]
pub struct WarmToken {
    cancelled: Arc<AtomicBool>,
}

impl WarmToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }
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

/// Build a backdrop without decoding, blurring or stat-ing on the UI thread.
///
/// Desktop prefers a cached wallpaper; a cold one reports
/// [`BackdropBuild::Warming`] so the caller can start a detached
/// [`warm_cache`] and paint the native material meanwhile. A theme gradient is
/// only reported as ready when there is no wallpaper source, or every reported
/// source failed recently. Image mode is cache-only: a warmed image uploads in
/// a few milliseconds, a cold one warms in the background.
pub fn build(ctx: &Context, theme: &Theme, mode: BackgroundMode, path: &str) -> BackdropBuild {
    match mode {
        BackgroundMode::None => BackdropBuild::Ready(None),
        BackgroundMode::Desktop => build_desktop(ctx, theme, &desktop_candidates()),
        BackgroundMode::Image => {
            let trimmed = path.trim();
            if trimmed.is_empty() {
                return BackdropBuild::Failed("Choose a background image".to_string());
            }
            match cached_image_path(Path::new(trimmed)) {
                Some(image) => BackdropBuild::Ready(Some(upload(ctx, image, true))),
                None => BackdropBuild::Warming,
            }
        }
    }
}

fn build_desktop(ctx: &Context, theme: &Theme, candidates: &[PathBuf]) -> BackdropBuild {
    if let Some((image, _)) = first_cached_desktop(candidates) {
        return BackdropBuild::Ready(Some(upload(ctx, image, true)));
    }
    if candidates.is_empty() {
        let image = gradient(theme);
        cache_put(gradient_key(theme), image.clone());
        return BackdropBuild::Ready(Some(upload(ctx, image, false)));
    }
    if all_candidates_failed(candidates) {
        // Every reported wallpaper source failed recently. The gradient is a
        // real fallback now; it must not short-circuit a cache lookup above.
        let key = gradient_key(theme);
        let image = cached_key(&key).unwrap_or_else(|| {
            let image = gradient(theme);
            cache_put(key, image.clone());
            image
        });
        return BackdropBuild::Ready(Some(upload(ctx, image, false)));
    }
    BackdropBuild::Warming
}

fn upload(ctx: &Context, image: ColorImage, from_image: bool) -> Backdrop {
    Backdrop {
        texture: ctx.load_texture("illness_backdrop", image, TextureOptions::LINEAR),
        from_image,
    }
}

/// Decode and blur `mode` into the shared cache without creating a texture.
///
/// This is the detached-thread warm used only for a backdrop that actually
/// paints (Mica+Desktop/Image cold switch) or the Acrylic+Desktop cache-only
/// prewarm. Different paths warm concurrently; duplicate requests for one path
/// join a single flight, so no process-wide lock spans I/O. `theme` is used for
/// the gradient fallback when Desktop reports no usable wallpaper.
pub fn warm_cache(
    theme: &Theme,
    mode: BackgroundMode,
    path: &str,
    token: &WarmToken,
) -> Result<(), String> {
    if mode == BackgroundMode::None {
        return Ok(());
    }
    if token.is_cancelled() {
        return Err(WARM_CANCELLED.to_string());
    }

    match mode {
        BackgroundMode::Desktop => warm_desktop(theme, &desktop_candidates(), token),
        BackgroundMode::Image => {
            let trimmed = path.trim();
            if trimmed.is_empty() {
                return Err("Choose a background image".to_string());
            }
            load_blurred(Path::new(trimmed), token).map(|_| ())
        }
        BackgroundMode::None => Ok(()),
    }
}

fn warm_desktop(theme: &Theme, candidates: &[PathBuf], token: &WarmToken) -> Result<(), String> {
    if candidates.is_empty() {
        cache_gradient(theme);
        return Ok(());
    }

    // Try highest priority first. If a lower-priority transcoded copy is cached
    // but the reported wallpaper is not, this still refreshes the preferred
    // candidate before falling back to the cached copy.
    for candidate in candidates {
        if token.is_cancelled() {
            return Err(WARM_CANCELLED.to_string());
        }
        match load_blurred(candidate, token) {
            Ok(_) => {
                clear_desktop_failure(candidates, candidate);
                return Ok(());
            }
            Err(error) if error == WARM_CANCELLED => return Err(error),
            Err(error) => {
                record_desktop_failure(candidates, candidate);
                log::debug!("background {} unavailable: {error}", candidate.display());
            }
        }
    }

    // No reported wallpaper could be used. Cache the theme gradient so the
    // native/theme fallback stays stable; record that this whole candidate set
    // failed, which lets the next warm after the cooldown retry a source that
    // has become available.
    record_desktop_attempt_failed(candidates);
    cache_gradient(theme);
    Ok(())
}

/// Whether a detached warm is needed for this pair.
///
/// Path-only: it never stats the filesystem. For Desktop it returns true when a
/// higher-priority source than the cached one (or any source when nothing is
/// cached) is due for a retry.
pub fn needs_warm(mode: BackgroundMode, path: &str) -> bool {
    match mode {
        BackgroundMode::None => false,
        BackgroundMode::Desktop => needs_warm_for_candidates(&desktop_candidates()),
        BackgroundMode::Image => {
            let trimmed = path.trim();
            !trimmed.is_empty() && cached_image_path(Path::new(trimmed)).is_none()
        }
    }
}

fn needs_warm_for_candidates(candidates: &[PathBuf]) -> bool {
    if candidates.is_empty() {
        return false;
    }
    let best = candidates
        .iter()
        .position(|candidate| cached_image_path(candidate).is_some());
    match best {
        Some(0) => false,
        Some(index) => candidates[..index]
            .iter()
            .any(|candidate| !candidate_failed_recently(candidates, candidate)),
        None => candidates
            .iter()
            .any(|candidate| !candidate_failed_recently(candidates, candidate)),
    }
}

/// Whether [`build`] can paint this pair right now without a decode. `None`
/// and a desktop with no wallpaper source are immediately paintable.
#[cfg(test)]
pub fn is_cached(_theme: &Theme, mode: BackgroundMode, path: &str) -> bool {
    match mode {
        BackgroundMode::None => true,
        BackgroundMode::Desktop => {
            let candidates = desktop_candidates();
            first_cached_desktop(&candidates).is_some()
                || candidates.is_empty()
                || all_candidates_failed(&candidates)
        }
        BackgroundMode::Image => {
            let trimmed = path.trim();
            !trimmed.is_empty() && cached_image_path(Path::new(trimmed)).is_some()
        }
    }
}

fn first_cached_desktop(candidates: &[PathBuf]) -> Option<(ColorImage, usize)> {
    candidates
        .iter()
        .enumerate()
        .find_map(|(index, candidate)| cached_image_path(candidate).map(|image| (image, index)))
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
///
/// Warm thread only. Concurrent requests for the same path join one flight;
/// requests for other paths proceed independently. The flight never holds the
/// registry lock across open/decode/blur.
fn load_blurred(path: &Path, token: &WarmToken) -> Result<ColorImage, String> {
    loop {
        if token.is_cancelled() {
            return Err(WARM_CANCELLED.to_string());
        }
        let key = image_key(path);
        if let Some(cached) = cached_key(&key) {
            return Ok(cached);
        }

        let warm_key = WarmKey::Image(path.to_path_buf());
        let (flight, leader) = begin_flight(warm_key.clone());
        if !leader {
            match wait_for_flight(&flight, token) {
                Ok(()) => {
                    return cached_image_path(path)
                        .ok_or_else(|| format!("Could not load \"{}\"", path.display()));
                }
                // The leader was superseded. If this request is still live,
                // take over instead of surfacing a stale cancellation.
                Err(error) if error == WARM_CANCELLED && !token.is_cancelled() => continue,
                Err(error) => return Err(error),
            }
        }

        let _registration = FlightRegistration { key: warm_key };
        // Re-check after becoming leader: another flight may have completed
        // between the lookup above and registration.
        let outcome = if let Some(cached) = cached_key(&key) {
            Ok(cached)
        } else if token.is_cancelled() {
            Err(WARM_CANCELLED.to_string())
        } else {
            #[cfg(test)]
            DECODE_COUNT.fetch_add(1, Ordering::Relaxed);
            match decode_image(path) {
                Ok(image) => {
                    let image = blurred(image);
                    if token.is_cancelled() {
                        Err(WARM_CANCELLED.to_string())
                    } else {
                        Ok(image)
                    }
                }
                Err(error) => Err(error),
            }
        };

        let result = match outcome {
            Ok(image) => {
                cache_put(key, image.clone());
                Ok(image)
            }
            Err(error) => Err(error),
        };
        let published = match &result {
            Ok(_) => Ok(()),
            Err(error) => Err(error.clone()),
        };
        publish_flight(&flight, published);
        return result;
    }
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
/// retaining full-size decodes. The mtime is resolved on the warm thread only.
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
static WARMS: OnceLock<Mutex<HashMap<WarmKey, Arc<Flight>>>> = OnceLock::new();
static DESKTOP_FAILURES: OnceLock<Mutex<DesktopFailureState>> = OnceLock::new();

#[cfg(test)]
static DECODE_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
static METADATA_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn cache() -> &'static Mutex<Vec<CacheEntry>> {
    CACHE.get_or_init(|| Mutex::new(Vec::new()))
}

fn warms() -> &'static Mutex<HashMap<WarmKey, Arc<Flight>>> {
    WARMS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[derive(Clone, PartialEq, Eq, Hash)]
enum WarmKey {
    Image(PathBuf),
}

#[derive(Default)]
struct Flight {
    result: Mutex<Option<Result<(), String>>>,
    ready: Condvar,
}

/// Removes a flight from the registry when the leader finishes, even on an
/// early error return.
struct FlightRegistration {
    key: WarmKey,
}

impl Drop for FlightRegistration {
    fn drop(&mut self) {
        let mut flights = warms()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        flights.remove(&self.key);
    }
}

/// Returns the shared flight for `key` and whether this caller is its leader.
fn begin_flight(key: WarmKey) -> (Arc<Flight>, bool) {
    let mut flights = warms()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(flight) = flights.get(&key) {
        return (Arc::clone(flight), false);
    }
    let flight = Arc::new(Flight::default());
    flights.insert(key, Arc::clone(&flight));
    (flight, true)
}

fn publish_flight(flight: &Flight, result: Result<(), String>) {
    let mut slot = flight
        .result
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *slot = Some(result);
    flight.ready.notify_all();
}

fn wait_for_flight(flight: &Flight, token: &WarmToken) -> Result<(), String> {
    let mut slot = flight
        .result
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    loop {
        if let Some(result) = slot.as_ref() {
            return result.clone();
        }
        if token.is_cancelled() {
            return Err(WARM_CANCELLED.to_string());
        }
        let (next, _) = flight
            .ready
            .wait_timeout(slot, Duration::from_millis(25))
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        slot = next;
    }
}

#[derive(Default)]
struct DesktopFailureState {
    candidates: Vec<PathBuf>,
    failed: HashMap<PathBuf, Instant>,
    /// Set when one warm tried every candidate and none worked. Anchoring the
    /// cooldown to the end of that attempt prevents a slow/UNC decode from
    /// immediately retrying because its first failure has already expired.
    all_failed_at: Option<Instant>,
}

fn desktop_failures() -> &'static Mutex<DesktopFailureState> {
    DESKTOP_FAILURES.get_or_init(|| Mutex::new(DesktopFailureState::default()))
}

fn sync_failure_candidates(state: &mut DesktopFailureState, candidates: &[PathBuf]) {
    if state.candidates != candidates {
        state.candidates = candidates.to_vec();
        state.failed.clear();
        state.all_failed_at = None;
    }
    if state
        .all_failed_at
        .is_some_and(|at| at.elapsed() >= DESKTOP_FAILURE_COOLDOWN)
    {
        state.failed.clear();
        state.all_failed_at = None;
    }
    state
        .failed
        .retain(|_, failed_at| failed_at.elapsed() < DESKTOP_FAILURE_COOLDOWN);
}

fn record_desktop_failure(candidates: &[PathBuf], path: &Path) {
    let mut state = desktop_failures()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    sync_failure_candidates(&mut state, candidates);
    state.failed.insert(path.to_path_buf(), Instant::now());
}

fn record_desktop_attempt_failed(candidates: &[PathBuf]) {
    let mut state = desktop_failures()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    sync_failure_candidates(&mut state, candidates);
    state.all_failed_at = Some(Instant::now());
}

fn clear_desktop_failure(candidates: &[PathBuf], path: &Path) {
    let mut state = desktop_failures()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    sync_failure_candidates(&mut state, candidates);
    state.failed.remove(path);
    state.all_failed_at = None;
}

fn candidate_failed_recently(candidates: &[PathBuf], path: &Path) -> bool {
    let mut state = desktop_failures()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    sync_failure_candidates(&mut state, candidates);
    state.failed.contains_key(path)
}

fn all_candidates_failed(candidates: &[PathBuf]) -> bool {
    if candidates.is_empty() {
        return false;
    }
    let mut state = desktop_failures()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    sync_failure_candidates(&mut state, candidates);
    state.all_failed_at.is_some()
        && candidates
            .iter()
            .all(|candidate| state.failed.contains_key(candidate))
}

fn cache_gradient(theme: &Theme) {
    let key = gradient_key(theme);
    if cached_key(&key).is_none() {
        cache_put(key, gradient(theme));
    }
}

fn image_key(path: &Path) -> CacheKey {
    #[cfg(test)]
    METADATA_COUNT.fetch_add(1, Ordering::Relaxed);
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

/// Path-only image lookup for the UI thread. Deliberately does not stat: a
/// disconnected UNC/%APPDATA% path must never block a frame.
fn cached_image_path(path: &Path) -> Option<ColorImage> {
    let mut cache = cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let index = cache.iter().rposition(
        |entry| matches!(&entry.key, CacheKey::Image { path: cached, .. } if cached == path),
    )?;
    let entry = cache.remove(index);
    let image = entry.image.clone();
    cache.push(entry);
    Some(image)
}

fn cache_put(key: CacheKey, image: ColorImage) {
    let mut cache = cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // One slot per image path: a refreshed mtime replaces the stale decode
    // rather than accumulating entries the path-only lookup could pick up.
    cache.retain(|entry| match (&entry.key, &key) {
        (CacheKey::Image { path: cached, .. }, CacheKey::Image { path: new, .. }) => cached != new,
        _ => entry.key != key,
    });
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

    fn write_image(dir: &Path, name: &str, width: u32, height: u32) -> PathBuf {
        let path = dir.join(name);
        let image = image::RgbaImage::from_fn(width, height, |x, y| {
            image::Rgba([(x % 256) as u8, (y % 256) as u8, 70, 255])
        });
        image.save(&path).expect("write image");
        path
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

        let loaded =
            load_blurred(&path, &WarmToken::new()).expect("extensionless image should decode");
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

        let error =
            load_blurred(&path, &WarmToken::new()).expect_err("oversized image must be rejected");
        assert!(error.contains("limit"), "{error}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn duplicate_warms_decode_once() {
        let _test = test_cache_lock();
        let dir = temp_dir();
        let path = write_image(&dir, "shared.png", 1920, 1080);
        let text = path.to_string_lossy().into_owned();
        let theme = Theme::by_name("Illness Dark");
        let before = DECODE_COUNT.load(Ordering::Relaxed);

        let workers: Vec<_> = (0..4)
            .map(|_| {
                let text = text.clone();
                std::thread::spawn(move || {
                    warm_cache(theme, BackgroundMode::Image, &text, &WarmToken::new())
                })
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
    fn single_flight_is_per_key_not_process_wide() {
        let _test = test_cache_lock();
        let (flight_a, leader_a) = begin_flight(WarmKey::Image(PathBuf::from("independent-a.png")));
        assert!(leader_a);
        // A second key must not wait for A's leader. Before the per-key flight
        // registry this would have blocked on the process-wide warm lock.
        let (flight_b, leader_b) = begin_flight(WarmKey::Image(PathBuf::from("independent-b.png")));
        assert!(leader_b);
        assert!(!Arc::ptr_eq(&flight_a, &flight_b));

        // A joiner for A shares A's flight and can be cancelled without waiting
        // for the leader's I/O.
        let (joined, joined_leader) =
            begin_flight(WarmKey::Image(PathBuf::from("independent-a.png")));
        assert!(!joined_leader);
        assert!(Arc::ptr_eq(&flight_a, &joined));
        let token = WarmToken::new();
        token.cancel();
        assert_eq!(
            wait_for_flight(&joined, &token),
            Err(WARM_CANCELLED.to_string())
        );

        publish_flight(&flight_a, Ok(()));
        publish_flight(&flight_b, Ok(()));
        drop(FlightRegistration {
            key: WarmKey::Image(PathBuf::from("independent-a.png")),
        });
        drop(FlightRegistration {
            key: WarmKey::Image(PathBuf::from("independent-b.png")),
        });
    }

    #[test]
    fn a_live_joiner_takes_over_after_a_cancelled_leader() {
        let _test = test_cache_lock();
        let dir = temp_dir();
        let path = write_image(&dir, "takeover.png", 256, 256);
        let key = WarmKey::Image(path.clone());
        let (flight, leader) = begin_flight(key.clone());
        assert!(leader);

        let joined_path = path.clone();
        let joiner = std::thread::spawn(move || load_blurred(&joined_path, &WarmToken::new()));
        std::thread::sleep(Duration::from_millis(25));
        publish_flight(&flight, Err(WARM_CANCELLED.to_string()));
        drop(FlightRegistration { key });

        joiner
            .join()
            .expect("joiner thread")
            .expect("a live joiner must take over instead of surfacing cancellation");
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
    fn cold_image_build_warms_without_ui_decode_or_metadata() {
        let _test = test_cache_lock();
        let dir = temp_dir();
        let path = write_image(&dir, "cold.png", 2560, 1440);
        let text = path.to_string_lossy().into_owned();
        let theme = Theme::by_name("Illness Dark");
        let ctx = Context::default();
        let decodes = DECODE_COUNT.load(Ordering::Relaxed);
        let metadata = METADATA_COUNT.load(Ordering::Relaxed);

        let started = Instant::now();
        let build = build(&ctx, theme, BackgroundMode::Image, &text);
        let elapsed = started.elapsed();

        assert!(matches!(build, BackdropBuild::Warming));
        assert!(needs_warm(BackgroundMode::Image, &text));
        assert_eq!(
            DECODE_COUNT.load(Ordering::Relaxed),
            decodes,
            "a cold paint must not decode on the UI thread"
        );
        assert_eq!(
            METADATA_COUNT.load(Ordering::Relaxed),
            metadata,
            "the UI cache check must be path-only"
        );
        println!("cold image build: {elapsed:?}");
        assert!(
            elapsed.as_millis() < 20,
            "cold build took {elapsed:?}, expected no decode/stat stall"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn warmed_image_upload_never_decodes_on_the_ui_path() {
        let _test = test_cache_lock();
        let dir = temp_dir();
        let path = write_image(&dir, "warm.png", 2560, 1440);
        let text = path.to_string_lossy().into_owned();
        let theme = Theme::by_name("Illness Dark");
        let ctx = Context::default();

        warm_cache(theme, BackgroundMode::Image, &text, &WarmToken::new()).expect("warm image");
        let before = DECODE_COUNT.load(Ordering::Relaxed);
        let metadata = METADATA_COUNT.load(Ordering::Relaxed);
        let started = Instant::now();
        let build = build(&ctx, theme, BackgroundMode::Image, &text);
        let elapsed = started.elapsed();

        assert!(matches!(build, BackdropBuild::Ready(Some(_))));
        assert_eq!(
            DECODE_COUNT.load(Ordering::Relaxed),
            before,
            "the UI-thread build must only upload the warmed image"
        );
        assert_eq!(
            METADATA_COUNT.load(Ordering::Relaxed),
            metadata,
            "a warmed UI build must not stat the image"
        );
        println!("warmed image upload: {elapsed:?}");
        assert!(
            elapsed.as_millis() < 20,
            "warmed upload took {elapsed:?}, expected a short upload"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn warmed_desktop_switch_never_decodes_on_the_ui_path() {
        let _test = test_cache_lock();
        let theme = Theme::by_name("Illness Dark");
        let ctx = Context::default();

        warm_cache(theme, BackgroundMode::Desktop, "", &WarmToken::new()).expect("warm desktop");
        assert!(is_cached(theme, BackgroundMode::Desktop, ""));
        let before = DECODE_COUNT.load(Ordering::Relaxed);
        let metadata = METADATA_COUNT.load(Ordering::Relaxed);
        let started = Instant::now();
        let build = build(&ctx, theme, BackgroundMode::Desktop, "");
        let elapsed = started.elapsed();

        assert!(matches!(build, BackdropBuild::Ready(Some(_))));
        assert_eq!(
            DECODE_COUNT.load(Ordering::Relaxed),
            before,
            "a warmed Acrylic+Desktop -> Mica+Desktop switch must not decode"
        );
        assert_eq!(
            METADATA_COUNT.load(Ordering::Relaxed),
            metadata,
            "a warmed desktop build must not stat candidates"
        );
        println!("warmed desktop upload: {elapsed:?}");
        assert!(
            elapsed.as_millis() < 20,
            "warmed upload took {elapsed:?}, expected a short upload"
        );
    }

    #[test]
    fn cached_gradient_never_masks_a_reported_wallpaper() {
        let _test = test_cache_lock();
        let dir = temp_dir();
        let theme = Theme::by_name("Illness Dark");
        let ctx = Context::default();
        let missing = dir.join("not-there-yet.png");
        let candidates = vec![missing.clone()];

        // A gradient from an earlier theme/load must not short-circuit the
        // candidate list.
        cache_put(gradient_key(theme), gradient(theme));
        assert!(matches!(
            build_desktop(&ctx, theme, &candidates),
            BackdropBuild::Warming
        ));
        assert!(needs_warm_for_candidates(&candidates));

        // Once the reported wallpaper exists, the next warm uses it.
        let image = image::RgbaImage::from_pixel(16, 16, image::Rgba([1, 2, 3, 255]));
        image.save(&missing).expect("write late wallpaper");
        warm_desktop(theme, &candidates, &WarmToken::new()).expect("warm preferred");
        let build = build_desktop(&ctx, theme, &candidates);
        assert!(matches!(build, BackdropBuild::Ready(Some(_))));
        assert!(!needs_warm_for_candidates(&candidates));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn preferred_wallpaper_refreshes_after_a_cached_transcoded_hit() {
        let _test = test_cache_lock();
        let dir = temp_dir();
        let theme = Theme::by_name("Illness Dark");
        let ctx = Context::default();
        let preferred = write_image(&dir, "preferred.png", 16, 16);
        let transcoded = write_image(&dir, "transcoded.png", 16, 16);
        let candidates = vec![preferred.clone(), transcoded.clone()];

        // Pretend the reported wallpaper was unavailable during an earlier
        // launch: only the lower-priority copy is cached.
        load_blurred(&transcoded, &WarmToken::new()).expect("cache transcoded");
        assert!(matches!(
            build_desktop(&ctx, theme, &candidates),
            BackdropBuild::Ready(Some(_))
        ));
        assert!(
            needs_warm_for_candidates(&candidates),
            "a lower-priority hit must still ask for the preferred candidate"
        );

        let decodes = DECODE_COUNT.load(Ordering::Relaxed);
        warm_desktop(theme, &candidates, &WarmToken::new()).expect("warm preferred");
        assert_eq!(
            DECODE_COUNT.load(Ordering::Relaxed) - decodes,
            1,
            "only the preferred candidate should be decoded"
        );
        assert!(!needs_warm_for_candidates(&candidates));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn failed_desktop_candidates_fall_back_but_retry_later() {
        let _test = test_cache_lock();
        let dir = temp_dir();
        let theme = Theme::by_name("Illness Dark");
        let ctx = Context::default();
        let missing = dir.join("disconnected.png");
        let candidates = vec![missing];

        cache_put(gradient_key(theme), gradient(theme));
        assert!(matches!(
            build_desktop(&ctx, theme, &candidates),
            BackdropBuild::Warming
        ));
        warm_desktop(theme, &candidates, &WarmToken::new()).expect("desktop fallback");
        assert!(matches!(
            build_desktop(&ctx, theme, &candidates),
            BackdropBuild::Ready(Some(_))
        ));
        assert!(
            !needs_warm_for_candidates(&candidates),
            "the cooldown must stop a rebuild/warm loop after a failure"
        );
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
