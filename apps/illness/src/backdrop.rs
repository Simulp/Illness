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

/// At most this many wallpapers decode at once. A rapid sequence of unique
/// picks queues behind the active decodes instead of starting every decode in
/// parallel; queued requests that are cancelled exit before opening a file.
const MAX_CONCURRENT_DECODES: usize = 2;

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

/// What a detached warm did to the cache.
///
/// `changed` is true when the cache now holds a different decode or a new
/// fallback. Revalidation warms usually report `false`, which lets the app skip
/// a redundant texture upload for an unchanged file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WarmOutcome {
    pub changed: bool,
}

impl WarmOutcome {
    const UNCHANGED: Self = Self { changed: false };
    const CHANGED: Self = Self { changed: true };
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
///
/// Test/small-caller wrapper that resolves the Desktop candidates itself. The
/// app resolves them once per rebuild and calls [`build_with`].
#[cfg(test)]
pub fn build(ctx: &Context, theme: &Theme, mode: BackgroundMode, path: &str) -> BackdropBuild {
    if mode == BackgroundMode::Desktop {
        return build_with(ctx, theme, mode, path, &desktop_candidates());
    }
    build_with(ctx, theme, mode, path, &[])
}

/// [`build`] with the Desktop candidate list resolved by the caller once per
/// rebuild. `desktop` is ignored for non-Desktop modes.
pub fn build_with(
    ctx: &Context,
    theme: &Theme,
    mode: BackgroundMode,
    path: &str,
    desktop: &[PathBuf],
) -> BackdropBuild {
    match mode {
        BackgroundMode::None => BackdropBuild::Ready(None),
        BackgroundMode::Desktop => build_desktop(ctx, theme, desktop),
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
        let image = desktop_gradient(theme);
        cache_put(gradient_key(theme), image.clone());
        return BackdropBuild::Ready(Some(upload(ctx, image, false)));
    }
    if all_candidates_failed(candidates) {
        // Every reported wallpaper source failed recently. The gradient is a
        // real fallback now; it must not short-circuit a cache lookup above.
        let key = gradient_key(theme);
        let image = cached_key(&key).unwrap_or_else(|| desktop_gradient(theme));
        cache_put(key, image.clone());
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
/// prewarm. Different paths warm concurrently up to the decode cap; duplicate
/// requests for one path join a single flight, so no process-wide lock spans
/// I/O. `theme` is used for the gradient fallback when Desktop reports no
/// usable wallpaper.
pub fn warm_cache(
    theme: &Theme,
    mode: BackgroundMode,
    path: &str,
    token: &WarmToken,
) -> Result<WarmOutcome, String> {
    if mode == BackgroundMode::None {
        return Ok(WarmOutcome::UNCHANGED);
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
            load_blurred(Path::new(trimmed), token).map(|loaded| WarmOutcome {
                changed: loaded.changed,
            })
        }
        BackgroundMode::None => Ok(WarmOutcome::UNCHANGED),
    }
}

fn warm_desktop(
    theme: &Theme,
    candidates: &[PathBuf],
    token: &WarmToken,
) -> Result<WarmOutcome, String> {
    if candidates.is_empty() {
        cache_gradient(theme);
        return Ok(WarmOutcome::CHANGED);
    }

    // Try highest priority first. If a lower-priority transcoded copy is cached
    // but the reported wallpaper is not, this still refreshes the preferred
    // candidate before falling back to the cached copy.
    let mut saw_failure = false;
    for candidate in candidates {
        if token.is_cancelled() {
            return Err(WARM_CANCELLED.to_string());
        }
        match load_blurred(candidate, token) {
            Ok(loaded) => {
                clear_desktop_failure(candidates, candidate);
                return Ok(WarmOutcome {
                    // A failed preferred source means the UI may still paint
                    // the old one; report a change so it switches to this one.
                    changed: loaded.changed || saw_failure,
                });
            }
            Err(error) if error == WARM_CANCELLED => return Err(error),
            Err(error) => {
                saw_failure = true;
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
    Ok(WarmOutcome::CHANGED)
}

/// Whether a detached warm is needed for this pair.
///
/// Path-only: it never stats the filesystem. For Desktop it returns true when a
/// higher-priority source than the cached one (or any source when nothing is
/// cached) is due for a retry.
pub fn needs_warm(mode: BackgroundMode, path: &str) -> bool {
    if mode == BackgroundMode::Desktop {
        return needs_warm_with(mode, path, &desktop_candidates());
    }
    needs_warm_with(mode, path, &[])
}

/// [`needs_warm`] with the Desktop candidate list resolved by the caller.
pub fn needs_warm_with(mode: BackgroundMode, path: &str, desktop: &[PathBuf]) -> bool {
    match mode {
        BackgroundMode::None => false,
        BackgroundMode::Desktop => needs_warm_for_candidates(desktop),
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
    let mut state = desktop_failures()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    sync_failure_candidates(&mut state, candidates);
    // One attempt tries every candidate. While that attempt's cooldown is
    // active no candidate is retried, even if an individual failure entry has
    // aged out.
    if state.all_failed_at.is_some() {
        return false;
    }
    let failed: Vec<bool> = candidates
        .iter()
        .map(|candidate| state.failed.contains_key(candidate))
        .collect();
    drop(state);
    let best = candidates
        .iter()
        .position(|candidate| cached_image_path(candidate).is_some());
    match best {
        Some(0) => false,
        Some(index) => (0..index).any(|candidate| !failed[candidate]),
        None => failed.iter().any(|failed| !failed),
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

pub fn desktop_candidates() -> Vec<PathBuf> {
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

/// Whether a load decoded a fresh image or reused the cache.
#[derive(Debug)]
struct LoadedImage {
    changed: bool,
}

/// Bounded decode admission. Held across `decode_image` (and the image blur),
/// never while waiting for another flight.
struct DecodeSemaphore {
    slots: Mutex<usize>,
    ready: Condvar,
}

impl DecodeSemaphore {
    fn new() -> Self {
        Self {
            slots: Mutex::new(MAX_CONCURRENT_DECODES),
            ready: Condvar::new(),
        }
    }
}

static DECODE_SEMAPHORE: OnceLock<DecodeSemaphore> = OnceLock::new();

fn decode_semaphore() -> &'static DecodeSemaphore {
    DECODE_SEMAPHORE.get_or_init(DecodeSemaphore::new)
}

struct DecodeSlot {
    semaphore: &'static DecodeSemaphore,
}

impl Drop for DecodeSlot {
    fn drop(&mut self) {
        let mut slots = self
            .semaphore
            .slots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *slots += 1;
        self.semaphore.ready.notify_one();
    }
}

/// Wait for a decode slot, waking often enough that a cancelled request exits
/// before it opens or decodes a file.
fn acquire_decode_slot(token: &WarmToken) -> Result<DecodeSlot, String> {
    let semaphore = decode_semaphore();
    let mut slots = semaphore
        .slots
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    loop {
        if token.is_cancelled() {
            return Err(WARM_CANCELLED.to_string());
        }
        if *slots > 0 {
            *slots -= 1;
            return Ok(DecodeSlot { semaphore });
        }
        let (next, _) = semaphore
            .ready
            .wait_timeout(slots, Duration::from_millis(25))
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        slots = next;
    }
}

/// Decode a wallpaper once per `(path, mtime)` and reuse the blurred result.
///
/// Warm thread only. Concurrent requests for the same path join one flight;
/// requests for other paths decode at most [`MAX_CONCURRENT_DECODES`] at a
/// time. The flight never holds the registry lock across open/decode/blur.
fn load_blurred(path: &Path, token: &WarmToken) -> Result<LoadedImage, String> {
    loop {
        if token.is_cancelled() {
            return Err(WARM_CANCELLED.to_string());
        }
        let (key, exists) = image_key_state(path);
        if !exists {
            evict_image_path(path);
            return Err(format!("Could not load \"{}\"", path.display()));
        }
        if let Some(cached) = cached_key(&key) {
            let _ = cached;
            return Ok(LoadedImage { changed: false });
        }

        let warm_key = WarmKey::Image(path.to_path_buf());
        let (flight, leader) = begin_flight(warm_key.clone());
        if !leader {
            match wait_for_flight(&flight, token) {
                Ok(outcome) => {
                    // Never hand a joiner an image cached for another mtime:
                    // the leader's result is only reusable when the current
                    // `(path, mtime)` key still matches the cache.
                    let (key, exists) = image_key_state(path);
                    if !exists {
                        evict_image_path(path);
                        return Err(format!("Could not load \"{}\"", path.display()));
                    }
                    if let Some(cached) = cached_key(&key) {
                        let _ = cached;
                        return Ok(LoadedImage {
                            changed: outcome.changed,
                        });
                    }
                    continue;
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
        let outcome = if token.is_cancelled() {
            Err(WARM_CANCELLED.to_string())
        } else {
            let (key, exists) = image_key_state(path);
            if !exists {
                evict_image_path(path);
                Err(format!("Could not load \"{}\"", path.display()))
            } else if let Some(cached) = cached_key(&key) {
                let _ = cached;
                Ok(LoadedImage { changed: false })
            } else {
                match acquire_decode_slot(token) {
                    Err(error) => Err(error),
                    Ok(_slot) => {
                        // A queued request may have waited long enough for the
                        // file to change or for another decode to fill the
                        // cache; re-resolve the mtime and check again.
                        let (key, exists) = image_key_state(path);
                        if token.is_cancelled() {
                            Err(WARM_CANCELLED.to_string())
                        } else if !exists {
                            evict_image_path(path);
                            Err(format!("Could not load \"{}\"", path.display()))
                        } else if let Some(cached) = cached_key(&key) {
                            let _ = cached;
                            Ok(LoadedImage { changed: false })
                        } else {
                            #[cfg(test)]
                            DECODE_COUNT.fetch_add(1, Ordering::Relaxed);
                            #[cfg(test)]
                            let _active = ActiveDecode::enter();
                            match decode_image(path) {
                                Ok(image) => {
                                    let image = blurred(image);
                                    // Cache completed work before honoring
                                    // cancellation, so a takeover/joiner can
                                    // reuse it instead of decoding twice.
                                    cache_put(key, image.clone());
                                    #[cfg(test)]
                                    if let Some(hook) = CANCEL_AFTER_DECODE
                                        .lock()
                                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                                        .take()
                                    {
                                        hook.cancel();
                                    }
                                    if token.is_cancelled() {
                                        Err(WARM_CANCELLED.to_string())
                                    } else {
                                        Ok(LoadedImage { changed: true })
                                    }
                                }
                                Err(error) => {
                                    evict_image_path(path);
                                    Err(error)
                                }
                            }
                        }
                    }
                }
            }
        };

        let published = match &outcome {
            Ok(loaded) => Ok(WarmOutcome {
                changed: loaded.changed,
            }),
            Err(error) => Err(error.clone()),
        };
        publish_flight(&flight, published);
        return outcome;
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
        theme: &'static str,
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
static DESKTOP_GRADIENT: OnceLock<Mutex<Option<(&'static str, ColorImage)>>> = OnceLock::new();

#[cfg(test)]
static DECODE_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
static METADATA_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
static ACTIVE_DECODES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
static PEAK_DECODES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
static DECODE_DELAY_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
#[cfg(test)]
static CANCEL_AFTER_DECODE: Mutex<Option<WarmToken>> = Mutex::new(None);

#[cfg(test)]
struct ActiveDecode;

#[cfg(test)]
impl ActiveDecode {
    fn enter() -> Self {
        let active = ACTIVE_DECODES.fetch_add(1, Ordering::SeqCst) + 1;
        PEAK_DECODES.fetch_max(active, Ordering::SeqCst);
        let delay = DECODE_DELAY_MS.load(Ordering::SeqCst);
        if delay > 0 {
            std::thread::sleep(Duration::from_millis(delay));
        }
        Self
    }
}

#[cfg(test)]
impl Drop for ActiveDecode {
    fn drop(&mut self) {
        ACTIVE_DECODES.fetch_sub(1, Ordering::SeqCst);
    }
}

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
    result: Mutex<Option<Result<WarmOutcome, String>>>,
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

fn publish_flight(flight: &Flight, result: Result<WarmOutcome, String>) {
    let mut slot = flight
        .result
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *slot = Some(result);
    flight.ready.notify_all();
}

fn wait_for_flight(flight: &Flight, token: &WarmToken) -> Result<WarmOutcome, String> {
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
    let now = Instant::now();
    // Re-stamp every candidate so a slow later candidate cannot let an earlier
    // failure age out mid-attempt and immediately re-warm the same UNC path.
    for candidate in candidates {
        state.failed.insert(candidate.clone(), now);
    }
    state.all_failed_at = Some(now);
}

fn clear_desktop_failure(candidates: &[PathBuf], path: &Path) {
    let mut state = desktop_failures()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    sync_failure_candidates(&mut state, candidates);
    state.failed.remove(path);
    state.all_failed_at = None;
}

fn all_candidates_failed(candidates: &[PathBuf]) -> bool {
    if candidates.is_empty() {
        return false;
    }
    let mut state = desktop_failures()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    sync_failure_candidates(&mut state, candidates);
    // `all_failed_at` is the authority: individual failure timestamps may have
    // been pruned during a slow multi-candidate attempt. It is cleared by the
    // cooldown or any candidate-list change.
    state.all_failed_at.is_some()
}

fn cache_gradient(theme: &Theme) {
    let key = gradient_key(theme);
    if cached_key(&key).is_none() {
        cache_put(key, desktop_gradient(theme));
    }
}

fn image_key_state(path: &Path) -> (CacheKey, bool) {
    #[cfg(test)]
    METADATA_COUNT.fetch_add(1, Ordering::Relaxed);
    let metadata = std::fs::metadata(path);
    let exists = metadata.is_ok();
    let modified = metadata.ok().and_then(|metadata| metadata.modified().ok());
    (
        CacheKey::Image {
            path: path.to_path_buf(),
            modified,
        },
        exists,
    )
}

#[cfg(test)]
fn image_key(path: &Path) -> CacheKey {
    image_key_state(path).0
}

fn evict_image_path(path: &Path) {
    let mut cache = cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    cache.retain(|entry| {
        !matches!(
            &entry.key,
            CacheKey::Image { path: cached, .. } if cached == path
        )
    });
}

fn gradient_key(theme: &Theme) -> CacheKey {
    CacheKey::Gradient { theme: theme.name }
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

/// Theme gradient reused by Desktop fallbacks. Kept outside the LRU so an
/// Image/other entry cannot evict it and force a 512×512 regeneration on the UI
/// thread.
fn desktop_gradient(theme: &Theme) -> ColorImage {
    let mut slot = DESKTOP_GRADIENT
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some((name, image)) = slot.as_ref()
        && *name == theme.name
    {
        return image.clone();
    }
    let image = gradient(theme);
    *slot = Some((theme.name, image.clone()));
    image
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

    fn write_solid(dir: &Path, name: &str, color: [u8; 4]) -> PathBuf {
        let path = dir.join(name);
        image::RgbaImage::from_pixel(16, 16, image::Rgba(color))
            .save(&path)
            .expect("write solid image");
        path
    }

    fn dominant(pixel: Color32) -> (u8, u8, u8) {
        (pixel.r(), pixel.g(), pixel.b())
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
        assert!(loaded.changed, "a fresh decode reports a cache change");
        let cached = cached_image_path(&path).expect("decoded image should be cached");
        assert_eq!(cached.size, [640, 360]);

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

        publish_flight(&flight_a, Ok(WarmOutcome::UNCHANGED));
        publish_flight(&flight_b, Ok(WarmOutcome::UNCHANGED));
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

    #[test]
    fn same_path_wallpaper_change_is_revalidated_and_refreshed() {
        let _test = test_cache_lock();
        let dir = temp_dir();
        let theme = Theme::by_name("Illness Dark");
        let path = write_solid(&dir, "wallpaper.png", [255, 0, 0, 255]);
        let text = path.to_string_lossy().into_owned();

        warm_cache(theme, BackgroundMode::Image, &text, &WarmToken::new()).expect("warm red");
        let red = cached_image_path(&path).expect("red cached");
        let (r, _, b) = dominant(red.pixels[0]);
        assert!(r > 200 && b < 60, "expected red, got {r}/{b}");

        // Rewrite the file in place at the same path (new mtime, same length).
        std::thread::sleep(Duration::from_millis(30));
        write_solid(&dir, "wallpaper.png", [0, 0, 255, 255]);

        let decodes = DECODE_COUNT.load(Ordering::Relaxed);
        let outcome = warm_cache(theme, BackgroundMode::Image, &text, &WarmToken::new())
            .expect("revalidate changed wallpaper");
        assert!(outcome.changed, "a new mtime must report a fresh decode");
        assert!(
            DECODE_COUNT.load(Ordering::Relaxed) > decodes,
            "the in-place change must be decoded off-thread"
        );
        let blue = cached_image_path(&path).expect("refreshed cache");
        let (r, _, b) = dominant(blue.pixels[0]);
        assert!(
            b > 200 && r < 60,
            "expected the new blue image, got {r}/{b}"
        );

        // A second revalidation at the same mtime is a path hit, not a decode.
        let decodes = DECODE_COUNT.load(Ordering::Relaxed);
        let outcome = warm_cache(theme, BackgroundMode::Image, &text, &WarmToken::new())
            .expect("revalidate unchanged wallpaper");
        assert!(!outcome.changed);
        assert_eq!(DECODE_COUNT.load(Ordering::Relaxed), decodes);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn deleting_the_wallpaper_evicts_the_stale_cache() {
        let _test = test_cache_lock();
        let dir = temp_dir();
        let theme = Theme::by_name("Illness Dark");
        let path = write_solid(&dir, "gone.png", [0, 200, 0, 255]);
        let text = path.to_string_lossy().into_owned();

        warm_cache(theme, BackgroundMode::Image, &text, &WarmToken::new()).expect("warm image");
        assert!(cached_image_path(&path).is_some());

        std::fs::remove_file(&path).expect("delete wallpaper");
        let error = warm_cache(theme, BackgroundMode::Image, &text, &WarmToken::new())
            .expect_err("a deleted wallpaper must fail revalidation");
        assert!(error.contains("Could not load"), "{error}");
        assert!(
            cached_image_path(&path).is_none(),
            "deletion must evict the stale blur instead of painting it forever"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn all_failed_cooldown_survives_a_slow_second_candidate() {
        let _test = test_cache_lock();
        let dir = temp_dir();
        let first = dir.join("slow-first.png");
        let second = dir.join("slow-second.png");
        let candidates = vec![first.clone(), second.clone()];

        record_desktop_failure(&candidates, &first);
        record_desktop_attempt_failed(&candidates);
        assert!(all_candidates_failed(&candidates));
        assert!(!needs_warm_for_candidates(&candidates));

        // Simulate the first candidate's timestamp ageing out while the second
        // candidate was still doing slow UNC I/O.
        {
            let mut state = desktop_failures()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.failed.insert(
                first.clone(),
                Instant::now() - DESKTOP_FAILURE_COOLDOWN - Duration::from_secs(1),
            );
        }
        assert!(
            all_candidates_failed(&candidates),
            "the attempt-level cooldown must govern after a slow second candidate"
        );
        assert!(
            !needs_warm_for_candidates(&candidates),
            "no repeat slow UNC I/O during the cooldown"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cancelled_decode_is_cached_for_a_takeover() {
        let _test = test_cache_lock();
        let dir = temp_dir();
        let path = write_image(&dir, "cancelled.png", 320, 200);
        let token = WarmToken::new();
        *CANCEL_AFTER_DECODE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(token.clone());

        let error = load_blurred(&path, &token).expect_err("cancellation must be reported");
        assert_eq!(error, WARM_CANCELLED);
        assert!(
            cached_image_path(&path).is_some(),
            "completed decode work must be cached before cancellation"
        );

        let decodes = DECODE_COUNT.load(Ordering::Relaxed);
        let loaded = load_blurred(&path, &WarmToken::new()).expect("takeover reuses the cache");
        assert!(!loaded.changed);
        assert_eq!(
            DECODE_COUNT.load(Ordering::Relaxed),
            decodes,
            "a joiner/takeover must not decode the same wallpaper twice"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn joiner_rejects_a_cache_entry_for_another_mtime() {
        let _test = test_cache_lock();
        let dir = temp_dir();
        let path = write_solid(&dir, "joined.png", [255, 0, 0, 255]);

        warm_cache(
            Theme::by_name("Illness Dark"),
            BackgroundMode::Image,
            &path.to_string_lossy(),
            &WarmToken::new(),
        )
        .expect("cache red");
        assert!(cached_image_path(&path).is_some());

        let key = WarmKey::Image(path.clone());
        let (flight, leader) = begin_flight(key.clone());
        assert!(leader);

        // The old flight is still "decoding" while the file changes at the
        // same path; the joiner must not accept the red cache entry for it.
        std::thread::sleep(Duration::from_millis(30));
        write_solid(&dir, "joined.png", [0, 0, 255, 255]);
        let joiner_path = path.clone();
        let joiner = std::thread::spawn(move || load_blurred(&joiner_path, &WarmToken::new()));
        std::thread::sleep(Duration::from_millis(25));
        let decodes = DECODE_COUNT.load(Ordering::Relaxed);
        publish_flight(&flight, Ok(WarmOutcome::UNCHANGED));
        drop(FlightRegistration { key });

        let loaded = joiner
            .join()
            .expect("joiner thread")
            .expect("joiner must decode the current mtime");
        assert!(loaded.changed);
        assert!(DECODE_COUNT.load(Ordering::Relaxed) > decodes);
        let blue = cached_image_path(&path).expect("current cache");
        let (r, _, b) = dominant(blue.pixels[0]);
        assert!(
            b > 200 && r < 60,
            "joiner accepted the stale image: {r}/{b}"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn concurrent_unique_decodes_are_capped() {
        let _test = test_cache_lock();
        let dir = temp_dir();
        let theme = Theme::by_name("Illness Dark");
        let paths: Vec<_> = (0..4)
            .map(|index| write_image(&dir, &format!("unique-{index}.png"), 1024, 768))
            .collect();
        PEAK_DECODES.store(0, Ordering::SeqCst);
        DECODE_DELAY_MS.store(60, Ordering::SeqCst);

        let workers: Vec<_> = paths
            .iter()
            .map(|path| {
                let text = path.to_string_lossy().into_owned();
                std::thread::spawn(move || {
                    warm_cache(theme, BackgroundMode::Image, &text, &WarmToken::new())
                })
            })
            .collect();
        for worker in workers {
            worker.join().expect("warm thread").expect("warm image");
        }
        let peak = PEAK_DECODES.load(Ordering::SeqCst);
        DECODE_DELAY_MS.store(0, Ordering::SeqCst);

        assert!(
            peak <= MAX_CONCURRENT_DECODES,
            "peak decode concurrency {peak} exceeded {MAX_CONCURRENT_DECODES}"
        );
        assert!(peak >= 2, "the cap should still allow concurrent work");
        let _ = std::fs::remove_dir_all(dir);
    }
}
