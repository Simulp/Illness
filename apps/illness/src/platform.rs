//! Win32 window material: DWM dark mode, rounded corners, Mica and Acrylic.
//!
//! All `unsafe` code in the workspace lives in this module. Each call documents why the
//! window handle is valid and why the raw pointer passed to Win32 is correctly sized.

#![allow(unsafe_code)]

use anyhow::{Context, Result};
use illness_core::{Material, Rgb};
use raw_window_handle::HasWindowHandle;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

#[cfg(target_os = "windows")]
use raw_window_handle::RawWindowHandle;
#[cfg(target_os = "windows")]
use windows_sys::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, HWND, LPARAM, LRESULT, WPARAM,
};
#[cfg(target_os = "windows")]
use windows_sys::Win32::Graphics::Dwm::{
    DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE, DwmExtendFrameIntoClientArea,
    DwmSetWindowAttribute,
};
#[cfg(target_os = "windows")]
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
#[cfg(target_os = "windows")]
use windows_sys::Win32::UI::Shell::{
    DefSubclassProc, RemoveWindowSubclass, SUBCLASSPROC, SetWindowSubclass,
};
#[cfg(target_os = "windows")]
use windows_sys::Win32::UI::WindowsAndMessaging::{
    HTCAPTION, SC_MOVE, SC_SIZE, WM_ENTERSIZEMOVE, WM_EXITSIZEMOVE, WM_NCDESTROY, WM_NCLBUTTONDOWN,
    WM_SYSCOMMAND,
};

/// Fallback attribute used by Windows 10 builds that predate the documented dark-mode id.
#[cfg(target_os = "windows")]
const DWMWA_USE_IMMERSIVE_DARK_MODE_OLD: u32 = 19;
/// `DWMWCP_ROUND`: round the window corners where the compositor supports it.
#[cfg(target_os = "windows")]
const DWMWCP_ROUND: i32 = 2;

/// `WCA_ACCENT_POLICY` from the undocumented `SetWindowCompositionAttribute` API.
#[cfg(target_os = "windows")]
const WCA_ACCENT_POLICY: u32 = 0x13;
/// `ACCENT_ENABLE_ACRYLICBLURBEHIND`: the legacy acrylic blur used by
/// Windows 10/11 for behind-window blur.
#[cfg(target_os = "windows")]
const ACCENT_ENABLE_ACRYLICBLURBEHIND: u32 = 4;
/// `ACCENT_ENABLE_BLURBEHIND`: a cheap Gaussian blur. Windows 1903+ recomputes
/// legacy acrylic on every window move; using this state only for the duration
/// of `WM_ENTERSIZEMOVE` avoids that DWM per-move cost while preserving the
/// same tint. It is restored to acrylic as soon as the move loop ends.
#[cfg(target_os = "windows")]
const ACCENT_ENABLE_BLURBEHIND: u32 = 3;
/// `ACCENT_DISABLED`: clears any previously applied accent policy.
#[cfg(target_os = "windows")]
const ACCENT_DISABLED: u32 = 0;

/// Unique id for the window subclass that observes the modal move loop.
#[cfg(target_os = "windows")]
const ACRYLIC_MOVE_SUBCLASS_ID: usize = 0x494C_4E53;

/// Alpha at 100% background opacity. The legacy acrylic blur needs a
/// translucent tint to show through; a low alpha keeps the verified strength.
#[cfg(target_os = "windows")]
const ACRYLIC_TINT_ALPHA_OPAQUE: f32 = 32.0;

/// Outcome of a bounded path probe.
///
/// A timeout is deliberately distinct from "not a directory": a cold SMB or
/// VPN folder may become available after the deadline, and callers must not
/// silently replace it with an unrelated process directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathKind {
    Dir,
    NotDir,
    Timeout,
}

/// Reuse a recent negative result so a dead folder is not re-probed every time
/// a tab starts. Kept short: a path can be created moments after it is checked.
const PATH_PROBE_NEGATIVE_TTL: Duration = Duration::from_secs(5);
/// Reuse a timeout long enough to cover window creation, and keep completed
/// positive results this long as generations that reject an older timeout.
const PATH_PROBE_TIMEOUT_TTL: Duration = Duration::from_secs(10);
/// Bound the cache so a long session cannot accumulate dead paths forever.
const PATH_PROBE_CACHE_CAP: usize = 64;
/// Concurrent network-path probes. A dead SMB share can hold one worker for the
/// full SMB timeout; another tab's unrelated local or network path must still
/// make progress.
const PATH_PROBE_WORKERS: usize = 4;
/// How often a cancellable batch checks its token while waiting.
const PATH_PROBE_CANCEL_POLL: Duration = Duration::from_millis(25);

/// One cached probe result plus the generation that produced it.
///
/// Generations are what keep a caller's deadline timeout from overwriting a
/// real `Dir` that completed after the deadline but before the timeout was
/// cached. A result always carries a newer generation than the request that
/// started it.
#[derive(Debug, Clone, Copy)]
struct PathProbeEntry {
    at: Instant,
    kind: PathKind,
    generation: u64,
}

#[derive(Debug, Default)]
struct PathProbeCache {
    entries: HashMap<PathBuf, PathProbeEntry>,
    generation: u64,
}

impl PathProbeCache {
    fn next_generation(&mut self) -> u64 {
        self.generation = self.generation.saturating_add(1);
        self.generation
    }

    fn get(&mut self, path: &Path, now: Instant) -> Option<PathKind> {
        let entry = *self.entries.get(path)?;
        let ttl = match entry.kind {
            // A positive result is cheap to re-check and can become stale
            // immediately when a directory is deleted, so it is never served.
            PathKind::Dir => {
                if now.saturating_duration_since(entry.at) >= PATH_PROBE_TIMEOUT_TTL {
                    self.entries.remove(path);
                }
                return None;
            }
            PathKind::NotDir => PATH_PROBE_NEGATIVE_TTL,
            PathKind::Timeout => PATH_PROBE_TIMEOUT_TTL,
        };
        if now.saturating_duration_since(entry.at) < ttl {
            Some(entry.kind)
        } else {
            self.entries.remove(path);
            None
        }
    }

    fn put_result(&mut self, path: &Path, kind: PathKind, now: Instant) {
        let generation = self.next_generation();
        self.insert(
            path,
            PathProbeEntry {
                at: now,
                kind,
                generation,
            },
            now,
        );
    }

    /// Cache a caller deadline only when no result newer than its request has
    /// landed. A late `Dir` result therefore wins and the next tab rechecks it
    /// instead of inheriting a false timeout.
    fn put_timeout(&mut self, path: &Path, request_generation: u64, now: Instant) {
        if self.entries.get(path).is_some_and(|entry| {
            entry.kind == PathKind::Dir || entry.generation > request_generation
        }) {
            return;
        }
        self.insert(
            path,
            PathProbeEntry {
                at: now,
                kind: PathKind::Timeout,
                generation: request_generation,
            },
            now,
        );
    }

    fn insert(&mut self, path: &Path, entry: PathProbeEntry, now: Instant) {
        if self.entries.len() >= PATH_PROBE_CACHE_CAP {
            self.entries.retain(|_, known| {
                now.saturating_duration_since(known.at) < PATH_PROBE_TIMEOUT_TTL
            });
        }
        if self.entries.len() < PATH_PROBE_CACHE_CAP {
            self.entries.insert(path.to_path_buf(), entry);
        }
    }
}

/// One network path handed to the probe pool. `index` is the caller's unique
/// path slot; the reply channel is per batch so unrelated batches never share
/// result slots.
struct PathProbeTask {
    index: usize,
    path: PathBuf,
    reply: crossbeam_channel::Sender<(usize, PathKind)>,
}

/// Long-lived probe pool plus its generation-safe cache.
///
/// Local paths are probed inline by [`start_probe_paths`] before a batch is
/// submitted, so an unrelated dead UNC path can never make them wait. Only
/// network paths use the pool, where a stuck SMB check occupies one worker.
struct PathProbePool {
    jobs: crossbeam_channel::Sender<PathProbeTask>,
    cache: Arc<Mutex<PathProbeCache>>,
}

fn path_probe_pool() -> &'static PathProbePool {
    static POOL: OnceLock<PathProbePool> = OnceLock::new();
    POOL.get_or_init(|| {
        let (jobs, receiver) = crossbeam_channel::unbounded::<PathProbeTask>();
        let cache = Arc::new(Mutex::new(PathProbeCache::default()));
        for index in 0..PATH_PROBE_WORKERS {
            let receiver = receiver.clone();
            let worker_cache = Arc::clone(&cache);
            let spawned = std::thread::Builder::new()
                .name(format!("illness-path-probe-{index}"))
                .spawn(move || {
                    while let Ok(task) = receiver.recv() {
                        let kind = if task.path.is_dir() {
                            PathKind::Dir
                        } else {
                            PathKind::NotDir
                        };
                        {
                            let mut cache = worker_cache
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner());
                            cache.put_result(&task.path, kind, Instant::now());
                        }
                        // The batch may have timed out or been cancelled; the
                        // probe still warms the cache for later requests.
                        let _ = task.reply.send((task.index, kind));
                    }
                });
            if let Err(error) = spawned {
                log::warn!("could not start path probe worker {index}: {error}");
            }
        }
        PathProbePool { jobs, cache }
    })
}

/// Whether a path may block on a network share and therefore belongs on the
/// probe pool rather than the caller's thread.
#[cfg(target_os = "windows")]
fn is_network_path(path: &Path) -> bool {
    use std::path::{Component, Prefix};
    use windows_sys::Win32::Storage::FileSystem::GetDriveTypeW;

    const DRIVE_REMOTE: u32 = 4;
    let Some(Component::Prefix(prefix)) = path.components().next() else {
        return false;
    };
    match prefix.kind() {
        Prefix::UNC(_, _) | Prefix::VerbatimUNC(_, _) => true,
        Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => {
            let root = [u16::from(letter), u16::from(b':'), u16::from(b'\\'), 0];
            // Safety: `root` is a live, null-terminated wide drive root.
            unsafe { GetDriveTypeW(root.as_ptr()) == DRIVE_REMOTE }
        }
        _ => false,
    }
}

#[cfg(not(target_os = "windows"))]
fn is_network_path(path: &Path) -> bool {
    path.to_string_lossy().starts_with(r"\\")
}

/// A submitted probe batch. Callers wait at most their own deadline and never
/// block the UI thread; late pool results keep warming the generation cache.
struct ProbeBatch {
    mapping: Vec<usize>,
    unique: Vec<PathBuf>,
    results: Vec<Option<PathKind>>,
    pending: Vec<(usize, u64)>,
    replies: Option<crossbeam_channel::Receiver<(usize, PathKind)>>,
    cache: Arc<Mutex<PathProbeCache>>,
}

impl ProbeBatch {
    fn wait(mut self, timeout: Duration, cancel: Option<&AtomicBool>) -> Vec<PathKind> {
        let deadline = Instant::now().checked_add(timeout);
        if let Some(receiver) = self.replies.take() {
            while !self.pending.is_empty() {
                if cancel.is_some_and(|token| token.load(std::sync::atomic::Ordering::Acquire)) {
                    break;
                }
                let now = Instant::now();
                let left = deadline
                    .map(|deadline| deadline.saturating_duration_since(now))
                    .unwrap_or(timeout);
                if left.is_zero() {
                    break;
                }
                let poll = if cancel.is_some() {
                    left.min(PATH_PROBE_CANCEL_POLL)
                } else {
                    left
                };
                match receiver.recv_timeout(poll) {
                    Ok((index, kind)) => {
                        if let Some(slot) = self.results.get_mut(index)
                            && slot.is_none()
                        {
                            *slot = Some(kind);
                        }
                        self.pending.retain(|(pending, _)| *pending != index);
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                }
            }
        }

        let now = Instant::now();
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for (index, generation) in self.pending.drain(..) {
            if self.results.get(index).is_some_and(Option::is_none) {
                self.results[index] = Some(PathKind::Timeout);
                cache.put_timeout(&self.unique[index], generation, now);
            }
        }
        drop(cache);

        self.mapping
            .iter()
            .map(|index| self.results[*index].unwrap_or(PathKind::Timeout))
            .collect()
    }
}

/// Submit one probe batch without waiting. Local paths complete inline; only
/// network paths are queued, and duplicate paths share one batch slot.
fn start_probe_paths(paths: &[PathBuf]) -> ProbeBatch {
    let (unique, mapping) = unique_probe_paths(paths);
    let pool = path_probe_pool();
    let mut results: Vec<Option<PathKind>> = vec![None; unique.len()];
    let mut pending = Vec::new();
    let (reply, receiver) = crossbeam_channel::unbounded();
    let now = Instant::now();

    {
        let mut cache = pool
            .cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for (index, path) in unique.iter().enumerate() {
            if let Some(kind) = cache.get(path, now) {
                results[index] = Some(kind);
                continue;
            }
            if !is_network_path(path) {
                // A local directory check is cheap and must never queue behind
                // a dead SMB probe.
                let kind = if path.is_dir() {
                    PathKind::Dir
                } else {
                    PathKind::NotDir
                };
                cache.put_result(path, kind, Instant::now());
                results[index] = Some(kind);
                continue;
            }
            let generation = cache.next_generation();
            let task = PathProbeTask {
                index,
                path: path.clone(),
                reply: reply.clone(),
            };
            if pool.jobs.send(task).is_ok() {
                pending.push((index, generation));
            } else {
                cache.put_timeout(path, generation, Instant::now());
                results[index] = Some(PathKind::Timeout);
            }
        }
    }
    drop(reply);

    ProbeBatch {
        mapping,
        unique,
        results,
        pending,
        replies: Some(receiver),
        cache: Arc::clone(&pool.cache),
    }
}

/// Collapse duplicate paths while preserving first-seen order.
fn unique_probe_paths(paths: &[PathBuf]) -> (Vec<PathBuf>, Vec<usize>) {
    let mut unique: Vec<PathBuf> = Vec::new();
    let mut mapping = Vec::with_capacity(paths.len());
    for path in paths {
        let index = unique
            .iter()
            .position(|known| known == path)
            .unwrap_or_else(|| {
                unique.push(path.clone());
                unique.len() - 1
            });
        mapping.push(index);
    }
    (unique, mapping)
}

/// Probe several candidate folders in one off-thread batch.
///
/// The caller waits at most `timeout` in total, not once per path. Duplicate
/// paths are checked once, recent negative/timeout results are reused, and
/// unresolved entries become [`PathKind::Timeout`]. That distinction lets a
/// cold launch folder survive while a dead live shell cwd falls through to a
/// usable stored/startup/process directory.
///
/// The app calls this only from its shell-spawn worker or before the window is
/// created; a network path never blocks the UI thread. Local paths are checked
/// inline, so they are unaffected by another tab's stuck UNC probe.
pub fn probe_paths(paths: &[PathBuf], timeout: Duration) -> Vec<PathKind> {
    start_probe_paths(paths).wait(timeout, None)
}

/// Cancellable form for a shell spawn. A closed/restarted tab sets the token,
/// so a worker waiting for a cold share stops before creating a process.
pub fn probe_paths_cancellable(
    paths: &[PathBuf],
    timeout: Duration,
    cancel: &AtomicBool,
) -> Vec<PathKind> {
    start_probe_paths(paths).wait(timeout, Some(cancel))
}

/// Single-path form of [`probe_paths`]. Multi-candidate callers use the batch
/// form so one deadline covers every candidate instead of one per folder.
pub fn path_kind_with_deadline(path: &Path, timeout: Duration) -> PathKind {
    probe_paths(&[path.to_path_buf()], timeout)
        .into_iter()
        .next()
        .unwrap_or(PathKind::Timeout)
}

/// Whether `path` is a directory, probed with a short deadline so an offline
/// network path cannot freeze startup or tab creation. A timeout is not a
/// directory for this boolean form; callers that must preserve a cold folder
/// use [`path_kind_with_deadline`] instead.
#[allow(dead_code)]
pub fn path_is_dir_with_deadline(path: &Path, timeout: Duration) -> bool {
    path_kind_with_deadline(path, timeout) == PathKind::Dir
}

/// Run a blocking probe on a detached thread and give up after `timeout`.
///
/// Kept for tests and small one-off probes; the app's folder checks use the
/// reusable [`probe_paths`] worker so an offline share does not leave a new
/// blocked thread behind on every request.
#[allow(dead_code)]
pub fn probe_with_deadline<T: Send + 'static>(
    timeout: Duration,
    probe: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    let (sender, receiver) = mpsc::channel();
    if std::thread::Builder::new()
        .name("illness-one-shot-probe".to_string())
        .spawn(move || {
            let _ = sender.send(probe());
        })
        .is_err()
    {
        return None;
    }
    receiver.recv_timeout(timeout).ok()
}

/// `ACCENT_POLICY` from `winuser.h`.
#[cfg(target_os = "windows")]
#[repr(C)]
struct AccentPolicy {
    accent_state: u32,
    accent_flags: u32,
    gradient_color: u32,
    animation_id: u32,
}

/// `WINDOWCOMPOSITIONATTRIBDATA` from `winuser.h`.
#[cfg(target_os = "windows")]
#[repr(C)]
struct WindowCompositionAttribData {
    attribute: u32,
    data: *mut core::ffi::c_void,
    size: usize,
}

#[cfg(target_os = "windows")]
type SetWindowCompositionAttributeFn =
    unsafe extern "system" fn(HWND, *mut WindowCompositionAttribData) -> i32;

/// Resolve the undocumented export once. It lives in `user32.dll` on every
/// supported Windows version; a missing export only disables Acrylic.
#[cfg(target_os = "windows")]
fn set_window_composition_attribute() -> Option<SetWindowCompositionAttributeFn> {
    static FUNCTION: OnceLock<Option<SetWindowCompositionAttributeFn>> = OnceLock::new();
    *FUNCTION.get_or_init(|| {
        // Safety: `user32.dll` is loaded in every GUI process. The module
        // handle is borrowed, never freed, and the export name is a
        // null-terminated ASCII string. The returned address is only called
        // with the signature documented for `SetWindowCompositionAttribute`.
        let module_name: Vec<u16> = "user32.dll\0".encode_utf16().collect();
        let module = unsafe { GetModuleHandleW(module_name.as_ptr()) };
        if module.is_null() {
            return None;
        }
        let function =
            unsafe { GetProcAddress(module, c"SetWindowCompositionAttribute".as_ptr().cast()) };
        // Safety: the export has the documented `BOOL WINAPI
        // SetWindowCompositionAttribute(HWND, WINDOWCOMPOSITIONATTRIBDATA*)`
        // signature; the cast only restores the concrete pointer type.
        function.map(|function| unsafe { std::mem::transmute(function) })
    })
}

/// Read WSL distributions from the per-user registry.
///
/// This never executes `wsl.exe`: on a machine without WSL that command can
/// open the Microsoft Store installer or a console window. An absent `Lxss`
/// key means there is nothing to offer.
///
/// # Safety
///
/// The registry key handle is opened and closed in this function. Every buffer
/// passed to Win32 is stack-owned, correctly sized in WCHARs, and its length is
/// passed in the units the API documents.
#[cfg(target_os = "windows")]
pub fn wsl_profiles() -> Vec<illness_core::Profile> {
    use std::path::PathBuf;
    use windows_sys::Win32::Foundation::{ERROR_NO_MORE_ITEMS, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_READ, RRF_RT_REG_SZ, RegCloseKey, RegEnumKeyExW, RegGetValueW,
        RegOpenKeyExW,
    };

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }

    let key_path = wide(r"Software\Microsoft\Windows\CurrentVersion\Lxss");
    let value_name = wide("DistributionName");
    let mut key: HKEY = std::ptr::null_mut();

    // Safety: key_path is null-terminated, key is a valid out-pointer, and the
    // handle is closed below before returning.
    let opened =
        unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, key_path.as_ptr(), 0, KEY_READ, &mut key) };
    if opened != ERROR_SUCCESS {
        return Vec::new();
    }

    let Some(wsl) = std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .map(|root| root.join("System32").join("wsl.exe"))
        .filter(|path| path.is_file())
    else {
        // Safety: key was opened successfully above.
        unsafe { RegCloseKey(key) };
        return Vec::new();
    };
    let wsl = wsl.to_string_lossy().into_owned();
    let mut profiles = Vec::new();
    let mut index = 0;

    loop {
        let mut name = [0u16; 256];
        let mut name_len = (name.len() - 1) as u32;
        // Safety: name is a 256-WCHAR stack buffer, name_len carries its
        // capacity in WCHARs, and all optional out-parameters are null.
        let status = unsafe {
            RegEnumKeyExW(
                key,
                index,
                name.as_mut_ptr(),
                &mut name_len,
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if status == ERROR_NO_MORE_ITEMS {
            break;
        }
        if status != ERROR_SUCCESS {
            break;
        }
        index += 1;

        let mut data = [0u16; 256];
        let mut data_len = u32::try_from(data.len() * 2).unwrap_or(u32::MAX);
        // Safety: name is a null-terminated subkey name from the same key,
        // data is a stack buffer, and data_len is its size in bytes.
        let status = unsafe {
            RegGetValueW(
                key,
                name.as_ptr(),
                value_name.as_ptr(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                data.as_mut_ptr().cast(),
                &mut data_len,
            )
        };
        if status != ERROR_SUCCESS {
            continue;
        }

        let chars = usize::try_from(data_len / 2)
            .unwrap_or(0)
            .saturating_sub(1)
            .min(data.len());
        let distro = String::from_utf16_lossy(&data[..chars]);
        if distro.is_empty()
            || profiles
                .iter()
                .any(|profile: &illness_core::Profile| profile.name == format!("WSL: {distro}"))
        {
            continue;
        }
        profiles.push(illness_core::Profile {
            name: format!("WSL: {distro}"),
            program: wsl.clone(),
            args: vec!["-d".to_string(), distro],
            accent: illness_core::Rgb::new(0xe9, 0x54, 0x20),
        });
    }

    // Safety: key was opened successfully above and is not used afterwards.
    unsafe { RegCloseKey(key) };
    profiles
}

/// Per-user Explorer verbs for opening a folder in Illness.
///
/// `Directory\Background` is the empty space inside a folder and receives
/// `%V`; `Directory` is a selected folder and `Drive` a drive root, both of
/// which receive `%1`. No file association is added, so double-clicking a file
/// is unchanged.
const CONTEXT_MENU_VERBS: &[(&str, &str)] = &[
    (r"Software\Classes\Directory\Background\shell\Illness", "%V"),
    (r"Software\Classes\Directory\shell\Illness", "%1"),
    (r"Software\Classes\Drive\shell\Illness", "%1"),
];

/// Build the Explorer command line for one verb.
///
/// Both the executable and the placeholder are quoted, so an install path
/// containing spaces works and `%V`/`%1` stays one argument. Windows paths
/// cannot contain `"`, so no escaping is required.
fn context_menu_command(executable: &OsStr, placeholder: &str) -> String {
    format!("\"{}\" \"{placeholder}\"", executable.to_string_lossy())
}

/// Case-insensitive component-prefix test for Windows paths.
fn path_has_prefix(path: &Path, root: &Path) -> bool {
    let path: Vec<_> = path.components().collect();
    let root: Vec<_> = root.components().collect();
    !root.is_empty()
        && root.len() <= path.len()
        && root.iter().zip(&path).all(|(left, right)| {
            left.as_os_str()
                .to_string_lossy()
                .eq_ignore_ascii_case(&right.as_os_str().to_string_lossy())
        })
}

/// A Cargo build tree (`target\debug`, `target\release`, or a target triple
/// variant) is a development copy, never an installed application.
fn is_dev_target_path(path: &Path) -> bool {
    let parts: Vec<String> = path
        .components()
        .map(|component| component.as_os_str().to_string_lossy().to_ascii_lowercase())
        .collect();
    parts.iter().enumerate().any(|(index, part)| {
        part == "target"
            && parts[index + 1..]
                .iter()
                .take(3)
                .any(|next| next == "debug" || next == "release")
    })
}

/// Roots that count as a real installation for automatic menu registration.
fn stable_install_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for variable in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(root) = std::env::var_os(variable) {
            roots.push(PathBuf::from(root));
        }
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        roots.push(PathBuf::from(local).join("Programs"));
    }
    roots
}

/// Whether this executable may rewrite the per-user Explorer verbs without an
/// explicit `--register-context-menu`.
///
/// A debug/portable/temporary/USB copy must not repoint an installed menu at a
/// path that later disappears. Only a stable installed root (Program Files or
/// `%LOCALAPPDATA%\Programs`) may update the verbs automatically; the explicit
/// flag is the escape hatch for a deliberate portable registration.
fn automatic_context_menu_allowed(
    executable: &Path,
    stable_roots: &[PathBuf],
    temp_root: &Path,
    removable: bool,
) -> bool {
    !removable
        && !is_dev_target_path(executable)
        && !path_has_prefix(executable, temp_root)
        && stable_roots
            .iter()
            .any(|root| path_has_prefix(executable, root))
}

/// A drive that Windows reports as removable (USB/flash media).
#[cfg(target_os = "windows")]
fn is_removable_path(path: &Path) -> bool {
    use std::path::{Component, Prefix};
    use windows_sys::Win32::Storage::FileSystem::GetDriveTypeW;

    const DRIVE_REMOVABLE: u32 = 2;
    let Some(Component::Prefix(prefix)) = path.components().next() else {
        return false;
    };
    let Prefix::Disk(letter) = prefix.kind() else {
        return false;
    };
    let root = [u16::from(letter), u16::from(b':'), u16::from(b'\\'), 0];
    // Safety: `root` is a live, null-terminated wide drive root.
    unsafe { GetDriveTypeW(root.as_ptr()) == DRIVE_REMOVABLE }
}

#[cfg(not(target_os = "windows"))]
fn is_removable_path(_path: &Path) -> bool {
    false
}

/// HKEY handle closed on every return path, including a failed value write.
#[cfg(target_os = "windows")]
struct RegistryKey(windows_sys::Win32::System::Registry::HKEY);

#[cfg(target_os = "windows")]
impl Drop for RegistryKey {
    fn drop(&mut self) {
        // Safety: `self.0` came from a successful `RegCreateKeyExW` and is
        // closed exactly once here.
        unsafe { windows_sys::Win32::System::Registry::RegCloseKey(self.0) };
    }
}

/// Null-terminated UTF-16 for a registry path, value name or value.
#[cfg(target_os = "windows")]
fn registry_wide(value: &OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;

    value.encode_wide().chain(std::iter::once(0)).collect()
}

/// Create or open one per-user key for writing.
///
/// # Safety
///
/// The subkey buffer is null-terminated and live for the call. `key` and
/// `disposition` are valid out-pointers, the class and security pointers are
/// null as the API allows, and the returned handle is owned by
/// [`RegistryKey`]. Only `HKEY_CURRENT_USER` is touched, so no elevation is
/// needed.
#[cfg(target_os = "windows")]
fn create_registry_key(path: &str) -> Result<RegistryKey> {
    use windows_sys::Win32::System::Registry::{
        HKEY, KEY_WRITE, REG_OPTION_NON_VOLATILE, RegCreateKeyExW,
    };

    let key_path = registry_wide(OsStr::new(path));
    let mut key: HKEY = std::ptr::null_mut();
    let mut disposition = 0;
    // Safety: see the function-level contract.
    let status = unsafe {
        RegCreateKeyExW(
            windows_sys::Win32::System::Registry::HKEY_CURRENT_USER,
            key_path.as_ptr(),
            0,
            std::ptr::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            std::ptr::null(),
            &mut key,
            &mut disposition,
        )
    };
    if status != ERROR_SUCCESS {
        anyhow::bail!("could not open HKCU\\{path}: Windows error {status}");
    }
    Ok(RegistryKey(key))
}

/// Write one `REG_SZ` value, including its terminating NUL.
///
/// # Safety
///
/// `key` is a live key owned by [`RegistryKey`]. Both wide strings are
/// null-terminated stack-owned buffers, and `data.len() * 2` is their exact
/// byte length.
#[cfg(target_os = "windows")]
fn set_registry_string(key: &RegistryKey, name: &str, value: &OsStr) -> Result<()> {
    use windows_sys::Win32::System::Registry::{REG_SZ, RegSetValueExW};

    let value_name = registry_wide(OsStr::new(name));
    let data = registry_wide(value);
    let size = u32::try_from(data.len() * std::mem::size_of::<u16>())
        .context("registry value exceeds the Win32 size limit")?;
    // Safety: see the function-level contract.
    let status = unsafe {
        RegSetValueExW(
            key.0,
            value_name.as_ptr(),
            0,
            REG_SZ,
            data.as_ptr().cast(),
            size,
        )
    };
    if status != ERROR_SUCCESS {
        anyhow::bail!("could not write registry value {name:?}: Windows error {status}");
    }
    Ok(())
}

/// What an app launch should do with the per-user Explorer verbs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContextMenuAction {
    /// Transient copy without an explicit flag: leave the installed menu alone.
    Skip,
    /// Machine-wide verbs are authoritative and valid; remove this user's
    /// legacy HKCU copy so it cannot shadow HKLM.
    RemoveStaleHkcu,
    /// Machine-wide verbs are absent or incomplete; write the HKCU fallback.
    WriteHkcu,
}

/// Whether a launch should skip, repair HKCU, or write HKCU.
///
/// `automatic` means the executable lives in a stable installed root and may
/// touch the user's menu without an explicit flag. A complete HKLM menu wins
/// over HKCU, but only when *all* three verbs exist and their command points at
/// an existing executable. An explicit `--register-context-menu` still repairs
/// in that case by deleting a stale shadowing HKCU copy.
fn context_menu_action(force: bool, automatic: bool, machine_complete: bool) -> ContextMenuAction {
    if !force && !automatic {
        ContextMenuAction::Skip
    } else if machine_complete {
        ContextMenuAction::RemoveStaleHkcu
    } else {
        ContextMenuAction::WriteHkcu
    }
}

/// Extract the executable from a registry verb command such as
/// `"C:\Program Files\Illness\illness.exe" "%V"`.
///
/// The installer and runtime always quote an executable path, but accept an
/// unquoted first token too so a manually created verb is still validated.
fn context_menu_command_executable(command: &str) -> Option<&str> {
    let command = command.trim();
    if let Some(rest) = command.strip_prefix('"') {
        let end = rest.find('"')?;
        (!rest[..end].is_empty()).then_some(&rest[..end])
    } else {
        command.split_whitespace().next()
    }
}

/// Whether a command points at a file that still exists.
fn context_menu_command_valid(command: &str) -> bool {
    context_menu_command_executable(command)
        .is_some_and(|executable| Path::new(executable).is_file())
}

/// `true` only when every verb has a readable command naming an existing
/// executable. A partial or stale HKLM registration is not authoritative.
fn context_menu_verbs_complete(commands: &[Option<String>]) -> bool {
    commands.len() == CONTEXT_MENU_VERBS.len()
        && commands
            .iter()
            .all(|command| command.as_deref().is_some_and(context_menu_command_valid))
}

/// Read one string value from a registry key.
#[cfg(target_os = "windows")]
fn read_registry_string(
    root: windows_sys::Win32::System::Registry::HKEY,
    path: &str,
    name: &str,
) -> Option<String> {
    use windows_sys::Win32::System::Registry::{RRF_RT_REG_SZ, RegGetValueW};

    let key_path = registry_wide(OsStr::new(path));
    let value_name = registry_wide(OsStr::new(name));
    let mut data = [0u16; 4096];
    let mut data_len = u32::try_from(data.len() * std::mem::size_of::<u16>()).ok()?;
    // Safety: key_path and value_name are null-terminated and live for the
    // call; data is a correctly sized WCHAR buffer and data_len is its byte
    // capacity.
    let status = unsafe {
        RegGetValueW(
            root,
            key_path.as_ptr(),
            value_name.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            data.as_mut_ptr().cast(),
            &mut data_len,
        )
    };
    if status != ERROR_SUCCESS {
        return None;
    }
    let chars = usize::try_from(data_len / 2)
        .unwrap_or(0)
        .saturating_sub(1)
        .min(data.len());
    Some(String::from_utf16_lossy(&data[..chars]))
}

/// Whether *all* machine-wide Explorer verbs exist and point at a real file.
///
/// The per-machine installer owns `HKLM\Software\Classes`; the application
/// must not leave a per-user copy shadowing it when the machine menu is healthy.
/// A single leftover key, or a command naming a deleted executable, is not
/// complete and falls back to repairing HKCU.
#[cfg(target_os = "windows")]
fn machine_context_menu_complete() -> bool {
    use windows_sys::Win32::System::Registry::HKEY_LOCAL_MACHINE;

    let commands: Vec<Option<String>> = CONTEXT_MENU_VERBS
        .iter()
        .map(|(path, _)| read_registry_string(HKEY_LOCAL_MACHINE, path, ""))
        .collect();
    context_menu_verbs_complete(&commands)
}

/// Whether any per-machine verb key exists, even if stale or incomplete.
///
/// Uninstall uses this broader check so it removes leftover keys, while the
/// runtime's decision to trust HKLM requires [`machine_context_menu_complete`].
#[cfg(target_os = "windows")]
fn machine_context_menu_present() -> bool {
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_LOCAL_MACHINE, KEY_READ, RegCloseKey, RegOpenKeyExW,
    };

    CONTEXT_MENU_VERBS.iter().any(|(path, _)| {
        let key_path = registry_wide(OsStr::new(path));
        let mut key: HKEY = std::ptr::null_mut();
        // Safety: key_path is null-terminated and live for the call; `key` is
        // a valid out-pointer and is closed below when open succeeded.
        let status =
            unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, key_path.as_ptr(), 0, KEY_READ, &mut key) };
        if status != ERROR_SUCCESS {
            return false;
        }
        // Safety: key was opened successfully above and is not used again.
        unsafe { RegCloseKey(key) };
        true
    })
}

/// Register the "Open with Illness" verbs for folders and drives.
///
/// `force` is the explicit `--register-context-menu` path and registers the
/// current executable from anywhere. Normal startup only updates the verbs
/// when running from a stable installed root; a `target\debug`, temporary,
/// portable or USB copy must not repoint an installed menu at a path that will
/// vanish.
///
/// A complete per-machine menu is authoritative: it must not be shadowed by a
/// stale HKCU verb from an older portable/current-user install, so this launch
/// deletes the current user's copy instead of writing another one. If HKLM is
/// absent, partial, or names a deleted executable, a normal stable install and
/// an explicit flag repair the user's menu with HKCU. The installer repairs
/// HKLM and every loaded user hive; an offline user's stale HKCU entry is
/// deleted on that user's next launch once HKLM is complete.
pub fn register_context_menu(force: bool) -> Result<()> {
    #[cfg(not(target_os = "windows"))]
    {
        let _ = force;
        Ok(())
    }

    #[cfg(target_os = "windows")]
    {
        use windows_sys::Win32::System::Registry::HKEY_CURRENT_USER;

        let executable =
            std::env::current_exe().context("could not resolve the Illness executable")?;
        let automatic = automatic_context_menu_allowed(
            &executable,
            &stable_install_roots(),
            &std::env::temp_dir(),
            is_removable_path(&executable),
        );
        let machine_complete = machine_context_menu_complete();
        match context_menu_action(force, automatic, machine_complete) {
            ContextMenuAction::Skip => {
                log::info!(
                    "skipping automatic Explorer registration from transient path {}",
                    executable.display()
                );
                return Ok(());
            }
            ContextMenuAction::RemoveStaleHkcu => {
                remove_context_menu_root(HKEY_CURRENT_USER, "HKCU")
                    .context("removing a stale per-user Explorer verb")?;
                log::info!(
                    "complete machine-wide Explorer verbs are authoritative; removed stale HKCU verbs"
                );
                return Ok(());
            }
            ContextMenuAction::WriteHkcu => {}
        }

        for (path, placeholder) in CONTEXT_MENU_VERBS {
            let key =
                create_registry_key(path).with_context(|| format!("registering HKCU\\{path}"))?;
            set_registry_string(&key, "MUIVerb", OsStr::new("Open with Illness"))
                .context("writing the context-menu label")?;
            set_registry_string(&key, "Icon", executable.as_os_str())
                .context("writing the context-menu icon")?;
            let command = context_menu_command(executable.as_os_str(), placeholder);
            set_registry_string(&key, "", OsStr::new(&command))
                .context("writing the context-menu command")?;
        }
        log::info!(
            "registered Explorer folder verbs for {}",
            executable.display()
        );
        Ok(())
    }
}

/// Remove only the Illness Explorer verbs.
///
/// Each key is deleted recursively, but the `shell` parent keys are left in
/// place so verbs from other applications survive. Portable/current-user keys
/// are always removed from HKCU. If machine-wide verbs exist, the explicit
/// `--unregister-context-menu` also removes HKLM and reports an elevation
/// error rather than silently claiming success.
#[cfg(target_os = "windows")]
fn remove_context_menu_root(
    root: windows_sys::Win32::System::Registry::HKEY,
    name: &str,
) -> Result<()> {
    use windows_sys::Win32::System::Registry::{RegDeleteKeyW, RegDeleteTreeW};

    let mut failure = None;
    for (path, _) in CONTEXT_MENU_VERBS {
        let key_path = registry_wide(OsStr::new(path));
        // Safety: key_path is null-terminated and live for the call.
        let tree_status = unsafe { RegDeleteTreeW(root, key_path.as_ptr()) };
        if tree_status != ERROR_SUCCESS && tree_status != ERROR_FILE_NOT_FOUND {
            failure.get_or_insert_with(|| {
                anyhow::anyhow!("could not clear {name}\\{path}: Windows error {tree_status}")
            });
        }
        // RegDeleteTree removes values and subkeys; this removes the now empty
        // verb key itself. Safety: same null-terminated path.
        let key_status = unsafe { RegDeleteKeyW(root, key_path.as_ptr()) };
        if key_status != ERROR_SUCCESS && key_status != ERROR_FILE_NOT_FOUND {
            failure.get_or_insert_with(|| {
                anyhow::anyhow!("could not remove {name}\\{path}: Windows error {key_status}")
            });
        }
    }
    match failure {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// Remove the Illness Explorer verbs created by [`register_context_menu`].
///
/// HKCU is the portable fallback. When any machine-wide key exists, the
/// explicit uninstall/flag path also removes HKLM, including partial or stale
/// registrations; running it without elevation returns an error instead of
/// pretending the menu is gone.
pub fn unregister_context_menu() -> Result<()> {
    #[cfg(not(target_os = "windows"))]
    {
        Ok(())
    }

    #[cfg(target_os = "windows")]
    {
        use windows_sys::Win32::System::Registry::HKEY_CURRENT_USER;

        let mut failure = None;
        if let Err(error) = remove_context_menu_root(HKEY_CURRENT_USER, "HKCU") {
            failure = Some(error);
        }
        if machine_context_menu_present()
            && let Err(error) = remove_context_menu_root(
                windows_sys::Win32::System::Registry::HKEY_LOCAL_MACHINE,
                "HKLM",
            )
        {
            failure.get_or_insert(error);
        }
        if let Some(error) = failure {
            return Err(error);
        }
        log::info!("removed Explorer folder verbs");
        Ok(())
    }
}

/// PEB/process-parameter offsets for one target bitness.
#[cfg(target_os = "windows")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PebLayout {
    process_parameters: usize,
    current_directory: usize,
    pointer_size: usize,
}

#[cfg(target_os = "windows")]
const PEB_LAYOUT_64: PebLayout = PebLayout {
    process_parameters: 0x20,
    current_directory: 0x38,
    pointer_size: 8,
};

#[cfg(target_os = "windows")]
const PEB_LAYOUT_32: PebLayout = PebLayout {
    process_parameters: 0x10,
    current_directory: 0x24,
    pointer_size: 4,
};

/// A 32-bit (WOW64) target uses 32-bit PEB/`RTL_USER_PROCESS_PARAMETERS`
/// layouts even when Illness itself is 64-bit. On a 32-bit build every
/// reachable target is 32-bit.
#[cfg(target_os = "windows")]
fn peb_layout_for_target(wow64: bool) -> PebLayout {
    if wow64 || cfg!(target_pointer_width = "32") {
        PEB_LAYOUT_32
    } else {
        PEB_LAYOUT_64
    }
}

/// Whether `IsWow64Process2` reported a 32-bit x86 target.
#[cfg(target_os = "windows")]
fn machine_is_wow64(process_machine: u16) -> bool {
    windows_sys::Win32::System::SystemInformation::IMAGE_FILE_MACHINE_I386 == process_machine
}

/// Owned process handle used by [`process_cwd`] and the priority boost.
#[cfg(target_os = "windows")]
struct ProcessHandle(windows_sys::Win32::Foundation::HANDLE);

#[cfg(target_os = "windows")]
impl Drop for ProcessHandle {
    fn drop(&mut self) {
        // Safety: the handle came from a successful `OpenProcess` and is
        // closed exactly once here.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
    }
}

/// Detect whether the target is a 32-bit process running under WOW64.
///
/// `IsWow64Process2` is the authoritative API and distinguishes x86 from
/// ARM64; older Windows falls back to `IsWow64Process`. `None` means the
/// bitness could not be established, in which case reading the PEB with the
/// wrong offsets is worse than returning no cwd.
#[cfg(target_os = "windows")]
fn target_is_wow64(process: &ProcessHandle) -> Option<bool> {
    use windows_sys::Win32::System::SystemInformation::IMAGE_FILE_MACHINE_UNKNOWN;
    use windows_sys::Win32::System::Threading::{IsWow64Process, IsWow64Process2};

    let mut process_machine: u16 = IMAGE_FILE_MACHINE_UNKNOWN;
    let mut native_machine: u16 = IMAGE_FILE_MACHINE_UNKNOWN;
    // Safety: both out-pointers are live 16-bit machine values.
    let status = unsafe { IsWow64Process2(process.0, &mut process_machine, &mut native_machine) };
    if status != 0 {
        return Some(machine_is_wow64(process_machine));
    }

    let mut wow64: windows_sys::core::BOOL = 0;
    // Safety: `wow64` is a live BOOL out-pointer.
    let status = unsafe { IsWow64Process(process.0, &mut wow64) };
    (status != 0).then_some(wow64 != 0)
}

/// Read `buffer.len()` bytes from another process.
///
/// # Safety
///
/// `process` owns a handle opened with `PROCESS_VM_READ`. `address` is only
/// interpreted as a pointer inside that process; `buffer` is a live writable
/// slice and its full length is passed to `ReadProcessMemory`.
#[cfg(target_os = "windows")]
fn read_process_memory(process: &ProcessHandle, address: usize, buffer: &mut [u8]) -> Option<()> {
    use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;

    let mut read = 0usize;
    // Safety: see the function-level contract.
    let ok = unsafe {
        ReadProcessMemory(
            process.0,
            address as *const core::ffi::c_void,
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut read,
        )
    };
    (ok != 0 && read == buffer.len()).then_some(())
}

#[cfg(target_os = "windows")]
fn read_process_usize(process: &ProcessHandle, address: usize) -> Option<usize> {
    let mut bytes = [0u8; std::mem::size_of::<usize>()];
    read_process_memory(process, address, &mut bytes)?;
    Some(usize::from_ne_bytes(bytes))
}

#[cfg(target_os = "windows")]
fn read_process_u32(process: &ProcessHandle, address: usize) -> Option<u32> {
    let mut bytes = [0u8; std::mem::size_of::<u32>()];
    read_process_memory(process, address, &mut bytes)?;
    Some(u32::from_ne_bytes(bytes))
}

#[cfg(target_os = "windows")]
fn read_process_pointer(
    process: &ProcessHandle,
    address: usize,
    pointer_size: usize,
) -> Option<usize> {
    if pointer_size == 4 {
        read_process_u32(process, address).map(|value| value as usize)
    } else {
        read_process_usize(process, address)
    }
}

/// Strip the Win32 object-manager prefix from a `DosPath`.
fn strip_nt_prefix(path: &str) -> PathBuf {
    let path = path
        .strip_prefix(r"\??\")
        .or_else(|| path.strip_prefix(r"\\?\"))
        .unwrap_or(path);
    if let Some(unc) = path.strip_prefix(r"UNC\") {
        PathBuf::from(format!(r"\\{unc}"))
    } else {
        PathBuf::from(path)
    }
}

/// Accept only a plausible absolute DOS/UNC path from the PEB.
///
/// A wrong offset, a torn read or a non-file provider can yield a valid-looking
/// but nonsensical string. Reject anything that is not `X:\...` or `\\...`
/// instead of handing a garbage directory to ConPTY.
fn plausible_process_path(text: &str) -> Option<PathBuf> {
    let text = text.trim_end_matches('\0');
    if text.is_empty() || text.encode_utf16().count() > 32_767 {
        return None;
    }
    if text
        .chars()
        .any(|character| character == '\0' || character.is_control())
    {
        return None;
    }
    let path = strip_nt_prefix(text);
    let display = path.to_string_lossy();
    let bytes = display.as_bytes();
    let drive_absolute = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/');
    let unc = display.starts_with(r"\\") && display.len() > 2;
    (drive_absolute || unc).then_some(path)
}

/// Size and buffer-pointer offset of the `CURDIR` header for one target
/// pointer size.
///
/// `CURDIR` starts with a `UNICODE_STRING` (`Length`, `MaximumLength`, pointer)
/// and omits only the handle. On 64-bit targets the pointer is 8-byte aligned,
/// so four bytes of padding follow `MaximumLength`; on 32-bit targets it starts
/// immediately after them.
#[cfg(target_os = "windows")]
fn curdir_header_layout(pointer_size: usize) -> Option<(usize, usize)> {
    match pointer_size {
        4 => Some((4, 8)),
        8 => Some((8, 16)),
        _ => None,
    }
}

/// Decode `Length` and the path buffer pointer from one `CURDIR` header read.
///
/// Requiring a non-empty, even byte length and a consistent maximum length
/// rejects torn reads before the buffer address is dereferenced.
#[cfg(target_os = "windows")]
fn decode_curdir_header(header: &[u8], pointer_size: usize) -> Option<(u16, usize)> {
    let (pointer_offset, header_size) = curdir_header_layout(pointer_size)?;
    if header.len() < header_size {
        return None;
    }
    let length = u16::from_le_bytes([header[0], header[1]]);
    let maximum_length = u16::from_le_bytes([header[2], header[3]]);
    if length < 2 || !length.is_multiple_of(2) {
        return None;
    }
    if maximum_length != 0 && maximum_length < length {
        return None;
    }
    let pointer = &header[pointer_offset..pointer_offset + pointer_size];
    let buffer = if pointer_size == 4 {
        u32::from_ne_bytes(pointer.try_into().ok()?) as usize
    } else {
        usize::from_ne_bytes(pointer.try_into().ok()?)
    };
    (buffer != 0).then_some((length, buffer))
}

/// Length of the path bytes including its NUL terminator.
///
/// Windows normally reports `CURDIR.Length` without the terminator, but some
/// providers include it. Accept either representation only when a NUL is
/// actually present; a fake or torn string with junk past `Length` is rejected
/// before it reaches [`decode_curdir_text`].
#[cfg(target_os = "windows")]
fn terminated_curdir_len(bytes: &[u8], declared: usize) -> Option<usize> {
    if declared == 0 || !declared.is_multiple_of(2) || declared > bytes.len() {
        return None;
    }
    let units = bytes.as_chunks::<2>().0;
    if units[declared / 2 - 1] == [0, 0] {
        Some(declared)
    } else if declared.checked_add(2)? <= bytes.len() && units.get(declared / 2) == Some(&[0, 0]) {
        Some(declared + 2)
    } else {
        None
    }
}

/// Decode the `CURDIR` DOS path, which must be NUL-terminated inside the slice.
///
/// Interior NULs, unpaired surrogates and non-DOS/UNC paths are rejected:
/// those indicate a wrong offset or junk in the target's PEB, and passing them
/// to ConPTY would fail in a confusing place.
#[cfg(target_os = "windows")]
fn decode_curdir_text(bytes: &[u8]) -> Option<PathBuf> {
    if bytes.is_empty() || !bytes.len().is_multiple_of(2) {
        return None;
    }
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .collect();
    if units.last().copied() != Some(0) {
        return None;
    }
    let path_units = &units[..units.len() - 1];
    if path_units.is_empty() || path_units.contains(&0) {
        return None;
    }
    let text = String::from_utf16(path_units).ok()?;
    plausible_process_path(&text)
}

/// Read a process's current working directory from its PEB.
///
/// Returns `None` when the process is gone, belongs to another user/elevation
/// level, exposes no usable DOS path, or its bitness cannot be established.
/// Callers must keep a stored fallback cwd.
#[cfg(target_os = "windows")]
pub fn process_cwd(pid: u32) -> Option<PathBuf> {
    use windows_sys::Wdk::System::Threading::{
        NtQueryInformationProcess, ProcessBasicInformation, ProcessWow64Information,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_BASIC_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
    };

    if pid == 0 {
        return None;
    }
    // Safety: no pointer arguments; the returned handle is null-checked and
    // owned by ProcessHandle. Limited query plus VM read is the least access
    // that lets a same-user process's PEB be read.
    let process =
        unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ, 0, pid) };
    if process.is_null() {
        return None;
    }
    let process = ProcessHandle(process);

    // Safety: PROCESS_BASIC_INFORMATION is a plain C out-structure for which
    // all-zero is valid; the API fills it and reports the length used.
    let mut basic: PROCESS_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    let mut returned = 0u32;
    let size = u32::try_from(std::mem::size_of::<PROCESS_BASIC_INFORMATION>()).ok()?;
    // Safety: `basic` and `returned` are live out-pointers with their exact
    // sizes; the process handle grants the required query access.
    let status = unsafe {
        NtQueryInformationProcess(
            process.0,
            ProcessBasicInformation,
            (&raw mut basic).cast(),
            size,
            &mut returned,
        )
    };
    if status < 0 {
        return None;
    }

    let wow64 = target_is_wow64(&process)?;
    let layout = peb_layout_for_target(wow64);
    let peb = if wow64 && cfg!(target_pointer_width = "64") {
        // A WOW64 target's PEB lives in its 32-bit address space. Ask for the
        // 32-bit PEB address rather than reusing the native PebBaseAddress.
        let mut peb32: usize = 0;
        let size = u32::try_from(std::mem::size_of::<usize>()).ok()?;
        // Safety: `peb32` is a live ULONG_PTR-sized out-pointer and `returned`
        // is live; ProcessWow64Information writes the 32-bit PEB address.
        let status = unsafe {
            NtQueryInformationProcess(
                process.0,
                ProcessWow64Information,
                (&raw mut peb32).cast(),
                size,
                &mut returned,
            )
        };
        if status < 0 || peb32 == 0 {
            return None;
        }
        peb32
    } else {
        basic.PebBaseAddress as usize
    };
    if peb == 0 {
        return None;
    }

    let parameters = read_process_pointer(
        &process,
        peb.checked_add(layout.process_parameters)?,
        layout.pointer_size,
    )?;
    if parameters == 0 {
        return None;
    }
    let current_directory = parameters.checked_add(layout.current_directory)?;
    // Read the whole CURDIR header in one operation. Separate Length and
    // Buffer reads could observe a torn structure while the shell changes
    // directory, pairing one call's length with another call's pointer.
    let (_, header_size) = curdir_header_layout(layout.pointer_size)?;
    let mut header = [0u8; 16];
    read_process_memory(&process, current_directory, &mut header[..header_size])?;
    let (length, buffer) = decode_curdir_header(&header[..header_size], layout.pointer_size)?;

    // Windows usually reports `Length` without the terminating NUL, but some
    // providers include it. Read one extra unit and require a terminator at
    // `Length` or just after it; a junk string without one is rejected.
    let declared = usize::from(length);
    let mut text = vec![0u8; declared.checked_add(2)?];
    read_process_memory(&process, buffer, &mut text)?;
    let terminated_at = terminated_curdir_len(&text, declared)?;
    decode_curdir_text(&text[..terminated_at])
}

/// Process inspection is Windows-only; other platforms use the stored cwd.
#[cfg(not(target_os = "windows"))]
pub fn process_cwd(_pid: u32) -> Option<PathBuf> {
    None
}

/// Whether `ILLNESS_CONPTY` asks for the inbox pseudoconsole.
///
/// `inbox` (the default when unset or empty) pins the inbox host; `auto` leaves
/// the process DLL search order alone. Any other value warns and falls back to
/// `inbox` instead of silently picking a host.
fn conpty_pin_inbox(value: Option<&str>) -> bool {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return true;
    };
    if value.eq_ignore_ascii_case("inbox") {
        return true;
    }
    if value.eq_ignore_ascii_case("auto") {
        return false;
    }
    log::warn!(
        "ILLNESS_CONPTY={value:?} is not recognized; expected inbox (default) or auto, using inbox"
    );
    true
}

/// Whether `ILLNESS_NO_PRIORITY_BOOST` disables the boost.
///
/// Only an affirmative value (`1`, `true`, `yes`, `on`) disables it. `0`,
/// `false`, `no`, `off`, and an empty or unset value keep boosting, so a bare
/// `ILLNESS_NO_PRIORITY_BOOST=1` still works.
#[cfg(target_os = "windows")]
fn priority_boost_disabled(value: Option<&str>) -> bool {
    matches!(
        value.map(str::trim).map(str::to_ascii_lowercase).as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

/// Pin the pseudoconsole API to the inbox kernel32 `CreatePseudoConsole`.
///
/// `alacritty_terminal` loads the pseudoconsole API with a bare
/// `LoadLibraryW("conpty.dll")`. There is no `conpty.dll` in `System32`, so that
/// load fails and the terminal falls back to kernel32's `CreatePseudoConsole`,
/// which spawns the inbox `conhost.exe`. A bare name also searches every `PATH`
/// entry first, so an installed Windows Terminal or WezTerm can supply its own
/// `conpty.dll`; that host is `OpenConsole.exe`, whose keystroke-to-echo floor
/// is roughly two orders of magnitude worse. Restricting the default search to
/// `System32` makes the bare-name load fail predictably instead of depending on
/// `PATH`.
///
/// `SetDefaultDllDirectories` is process-global: it changes the default search
/// for every later bare-name `LoadLibraryW` in this process. A `conpty.dll`
/// shipped next to our executable is therefore deliberately ignored.
///
/// `ILLNESS_CONPTY=auto` leaves the DLL search alone. Idempotent and never
/// panics: the Win32 call runs once and later calls return immediately. Call it
/// at startup, before the first PTY is created.
pub fn pin_inbox_conpty() {
    static PINNED: OnceLock<bool> = OnceLock::new();
    PINNED.get_or_init(|| {
        if !conpty_pin_inbox(std::env::var("ILLNESS_CONPTY").ok().as_deref()) {
            log::info!(
                "ConPTY host: auto (DLL search order unchanged; a PATH conpty.dll may choose OpenConsole.exe)"
            );
            return false;
        }
        #[cfg(target_os = "windows")]
        {
            use windows_sys::Win32::System::LibraryLoader::{
                LOAD_LIBRARY_SEARCH_SYSTEM32, SetDefaultDllDirectories,
            };

            // Safety: the call takes a plain flag bitfield and no pointers.
            let pinned = unsafe { SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32) } != 0;
            if pinned {
                log::info!(
                    "ConPTY pinned to the inbox conhost.exe (ILLNESS_CONPTY=inbox, the default)"
                );
            } else {
                log::info!(
                    "could not pin the DLL search to System32 ({}); a PATH conpty.dll may be used",
                    std::io::Error::last_os_error()
                );
            }
            pinned
        }
        #[cfg(not(target_os = "windows"))]
        {
            false
        }
    });
}

/// Scans for the freshly created ConPTY host before giving up.
#[cfg(target_os = "windows")]
const CONSOLE_PRIORITY_ATTEMPTS: usize = 8;
/// Attempts that retry almost immediately, while the user may already be typing.
#[cfg(target_os = "windows")]
const CONSOLE_PRIORITY_FAST_ATTEMPTS: usize = 3;
/// Attempt at which the retry worker refreshes the shell's own threads.
#[cfg(target_os = "windows")]
const CONSOLE_PRIORITY_THREAD_REFRESH_ATTEMPT: usize = 1;
/// Delay between the opening scans.
#[cfg(target_os = "windows")]
const CONSOLE_PRIORITY_FAST_RETRY: Duration = Duration::from_millis(15);
/// Delay between the later scans, for a 525 ms ceiling in total.
#[cfg(target_os = "windows")]
const CONSOLE_PRIORITY_RETRY: Duration = Duration::from_millis(120);

/// Delay before the next host scan, or `None` once the schedule is exhausted.
///
/// Front-loaded on purpose: the host may appear only after the user has already
/// started typing, so the first scans are 15 ms apart. The tail uses 120 ms and
/// keeps the whole schedule (`15 ms × 3 + 120 ms × 4`) at roughly 525 ms for a
/// heavily loaded machine.
#[cfg(target_os = "windows")]
fn console_priority_delay(attempt: usize) -> Option<Duration> {
    if attempt + 1 >= CONSOLE_PRIORITY_ATTEMPTS {
        return None;
    }
    Some(if attempt < CONSOLE_PRIORITY_FAST_ATTEMPTS {
        CONSOLE_PRIORITY_FAST_RETRY
    } else {
        CONSOLE_PRIORITY_RETRY
    })
}

/// Whether a process still sits below the target priority class.
///
/// `SetPriorityClass` succeeds on a process that is already above normal, so the
/// current class must be read first: otherwise one tab's boosted host would look
/// like a success and stop another tab from finding its own host. The classes are
/// not numerically ordered (`HIGH_PRIORITY_CLASS` is `0x80`, below
/// `ABOVE_NORMAL_PRIORITY_CLASS` at `0x8000`), so membership decides.
#[cfg(target_os = "windows")]
fn needs_priority_boost(current_class: u32) -> bool {
    use windows_sys::Win32::System::Threading::{
        ABOVE_NORMAL_PRIORITY_CLASS, HIGH_PRIORITY_CLASS, REALTIME_PRIORITY_CLASS,
    };

    !matches!(
        current_class,
        ABOVE_NORMAL_PRIORITY_CLASS | HIGH_PRIORITY_CLASS | REALTIME_PRIORITY_CLASS
    )
}

/// Whether a scanned host is this spawn's own and still needs raising.
///
/// A host created before the spawn attempt belongs to another tab (or a stale
/// worker) and must not end this scan; an already boosted host must not either.
#[cfg(target_os = "windows")]
fn host_is_ours(
    host_pid: u32,
    created_at: Option<u64>,
    spawn_at: u64,
    already_boosted: bool,
) -> bool {
    host_pid != 0 && !already_boosted && created_at.is_some_and(|created_at| created_at >= spawn_at)
}

/// Convert a wall-clock instant to the 100 ns `FILETIME` scale.
#[cfg(target_os = "windows")]
fn system_time_to_filetime(time: std::time::SystemTime) -> u64 {
    /// 100 ns ticks between 1601-01-01 and 1970-01-01.
    const UNIX_EPOCH_FILETIME: u64 = 116_444_736_000_000_000;
    match time.duration_since(std::time::UNIX_EPOCH) {
        Ok(elapsed) => {
            let ticks = u64::try_from(elapsed.as_nanos() / 100).unwrap_or(u64::MAX);
            UNIX_EPOCH_FILETIME.saturating_add(ticks)
        }
        Err(_) => 0,
    }
}

/// Raise the shell's threads and the ConPTY host above normal priority.
///
/// Under full CPU saturation the shell and its console host are starved at
/// normal priority and keystroke echo degrades to hundreds of milliseconds.
/// Measured p50 echo under 8/8 CPU saturation is ~302 ms unboosted, 30.0 ms with
/// the shell process raised, and 29.8 ms with only the shell's threads raised.
///
/// The shell is boosted per thread, not with `SetPriorityClass`: a process
/// priority class is inherited by every child process, so `cargo build`, `npm`,
/// or a browser started from this shell would then outrank Illness' own normal
/// threads and make echo worse while a build runs. Thread priority is not
/// inherited, so the user's builds stay at normal priority while the shell's
/// own input threads still get scheduled.
///
/// `spawn_at` is the instant the shell was spawned. Only ConPTY hosts created
/// at or after it belong to this tab; older hosts (another tab's, or a leftover
/// worker's) never end the scan. The shell is boosted synchronously while its
/// PID is certainly ours; the later refresh re-checks liveness first, because a
/// shell that exits within the first few milliseconds can have its PID reused by
/// an unrelated process. The host is created slightly after the shell, so it is
/// found on a short-lived detached thread: up to [`CONSOLE_PRIORITY_ATTEMPTS`] scans on the
/// [`console_priority_delay`] schedule, with the shell's threads refreshed once
/// at [`CONSOLE_PRIORITY_THREAD_REFRESH_ATTEMPT`] to cover threads created just
/// after spawn. `ILLNESS_NO_PRIORITY_BOOST` disables the whole thing. Never
/// panics.
pub fn boost_console_priority(
    shell_pid: Option<u32>,
    spawn_at: std::time::SystemTime,
    cancelled: bool,
) {
    #[cfg(target_os = "windows")]
    {
        if cancelled || !priority_boost_enabled() {
            return;
        }
        if let Some(pid) = shell_pid {
            boost_shell_threads(pid);
        }
        let spawn_at = system_time_to_filetime(spawn_at);
        let spawned = std::thread::Builder::new()
            .name("illness-console-priority".to_string())
            .spawn(move || boost_console_priority_blocking(shell_pid, spawn_at));
        if let Err(error) = spawned {
            log::debug!("could not start the console priority worker: {error}");
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (shell_pid, spawn_at, cancelled);
    }
}

/// Effective `ILLNESS_NO_PRIORITY_BOOST` setting, decided and logged once.
#[cfg(target_os = "windows")]
fn priority_boost_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        let disabled =
            priority_boost_disabled(std::env::var("ILLNESS_NO_PRIORITY_BOOST").ok().as_deref());
        if disabled {
            log::info!("console priority boost disabled by ILLNESS_NO_PRIORITY_BOOST");
        } else {
            log::info!("console priority boost enabled: shell threads run above normal priority");
        }
        !disabled
    })
}

/// How a scanned host responded to the boost attempt.
#[cfg(target_os = "windows")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HostBoost {
    /// Freshly raised to above normal.
    Raised,
    /// Already at or above the target.
    AlreadyBoosted,
    /// Created before this spawn, so it belongs to another tab.
    Older,
    /// Could not open, inspect, or raise it.
    Failed,
}

/// Blocking half of [`boost_console_priority`], run on its own thread.
///
/// Only ConPTY hosts are handled here; the shell's threads were boosted
/// synchronously and are refreshed once shortly after.
#[cfg(target_os = "windows")]
fn boost_console_priority_blocking(shell_pid: Option<u32>, spawn_at: u64) {
    let our_pid = std::process::id();
    let mut scanned = 0usize;
    let mut already_boosted = 0usize;
    let mut skipped = 0usize;
    for attempt in 0..CONSOLE_PRIORITY_ATTEMPTS {
        if attempt == CONSOLE_PRIORITY_THREAD_REFRESH_ATTEMPT
            && let Some(pid) = shell_pid
        {
            // The shell was boosted synchronously at spawn; this refresh only
            // covers threads created since. A shell that exited in the
            // meantime may have had its PID reused, so ask before touching it.
            if process_is_alive(pid) {
                boost_shell_threads(pid);
            }
        }
        let mut raised_this_spawn = false;
        for pid in console_tree_pids(our_pid) {
            scanned += 1;
            match boost_console_host(pid, spawn_at) {
                HostBoost::Raised => raised_this_spawn = true,
                HostBoost::AlreadyBoosted => already_boosted += 1,
                HostBoost::Older | HostBoost::Failed => skipped += 1,
            }
        }
        if raised_this_spawn {
            return;
        }
        match console_priority_delay(attempt) {
            Some(delay) => std::thread::sleep(delay),
            None => break,
        }
    }
    if scanned == 0 {
        log::debug!(
            "no ConPTY host was found after {CONSOLE_PRIORITY_ATTEMPTS} scans; leaving priorities alone"
        );
    } else {
        // Per observation, not per process: a host that stays put is seen again
        // on every scan.
        log::debug!(
            "no ConPTY host needed a boost after {CONSOLE_PRIORITY_ATTEMPTS} scans ({already_boosted} already above normal, {skipped} older or unreadable)"
        );
    }
}

/// Whether an open process handle refers to a process that is still running.
///
/// A PID taken from a Toolhelp snapshot or from `spawn` can already be gone, and
/// Windows may have recycled that number for an unrelated process. An exited or
/// unreadable process must therefore never be touched.
#[cfg(target_os = "windows")]
fn handle_is_alive(handle: windows_sys::Win32::Foundation::HANDLE) -> bool {
    use windows_sys::Win32::Foundation::STILL_ACTIVE;
    use windows_sys::Win32::System::Threading::GetExitCodeProcess;

    let mut exit_code = 0u32;
    // Safety: `exit_code` is a live out-pointer and the caller's handle grants
    // PROCESS_QUERY_LIMITED_INFORMATION.
    (unsafe { GetExitCodeProcess(handle, &raw mut exit_code) }) != 0
        && exit_code as i32 == STILL_ACTIVE
}

/// Whether `pid` currently names a running process.
///
/// [`boost_console_priority_blocking`] refreshes the shell's threads ~15 ms after
/// spawn; a shell that exited immediately may already have had its PID reused,
/// so the refresh asks first instead of trusting the number from `spawn`.
#[cfg(target_os = "windows")]
fn process_is_alive(pid: u32) -> bool {
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

    if pid == 0 {
        return false;
    }
    // Safety: no pointer arguments; the handle is null-checked and owns only
    // PROCESS_QUERY_LIMITED_INFORMATION.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return false;
    }
    let handle = ProcessHandle(handle);
    handle_is_alive(handle.0)
}

/// Owned Toolhelp snapshot handle.
#[cfg(target_os = "windows")]
struct SnapshotHandle(windows_sys::Win32::Foundation::HANDLE);

#[cfg(target_os = "windows")]
impl Drop for SnapshotHandle {
    fn drop(&mut self) {
        // Safety: the handle came from a successful
        // `CreateToolhelp32Snapshot` and is closed exactly once here.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
    }
}

/// Whether one Toolhelp process entry names a ConPTY host.
///
/// Compares the fixed `szExeFile` buffer as UTF-16 without allocating: this runs
/// for every process in the snapshot, up to eight times per worker.
#[cfg(target_os = "windows")]
fn entry_is_conpty_host(
    entry: &windows_sys::Win32::System::Diagnostics::ToolHelp::PROCESSENTRY32W,
) -> bool {
    let length = entry
        .szExeFile
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(entry.szExeFile.len());
    let name = &entry.szExeFile[..length];
    wide_eq_ignore_ascii_case(name, "conhost.exe")
        || wide_eq_ignore_ascii_case(name, "OpenConsole.exe")
}

/// ASCII-only case-insensitive comparison against a UTF-16 process name.
#[cfg(target_os = "windows")]
fn wide_eq_ignore_ascii_case(name: &[u16], expected: &str) -> bool {
    name.len() == expected.len()
        && name
            .iter()
            .zip(expected.as_bytes())
            .all(|(&unit, &byte)| unit < 0x80 && (unit as u8).eq_ignore_ascii_case(&byte))
}

/// Our ConPTY hosts, direct children of this process spawned by winconpty.
#[cfg(target_os = "windows")]
fn console_tree_pids(our_pid: u32) -> Vec<u32> {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };

    let mut hosts = Vec::new();
    // Safety: no pointer arguments; the returned handle is checked below and
    // owned by SnapshotHandle.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot.is_null() || snapshot == INVALID_HANDLE_VALUE {
        log::debug!("could not snapshot processes to boost the console tree");
        return hosts;
    }
    let snapshot = SnapshotHandle(snapshot);

    // Safety: PROCESSENTRY32W is a plain C out-structure for which all-zero is
    // valid; `dwSize` is set to its exact size as the API requires.
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    // Safety: `entry` is a live, correctly sized out-buffer for the snapshot's
    // full lifetime.
    let mut more = unsafe { Process32FirstW(snapshot.0, &raw mut entry) } != 0;
    while more {
        // The parent check is cheap and rejects almost every entry before the
        // name is compared, so no string is built for unrelated processes.
        if entry.th32ParentProcessID == our_pid && entry_is_conpty_host(&entry) {
            hosts.push(entry.th32ProcessID);
        }
        // Safety: same live buffer, advanced to the next entry.
        more = unsafe { Process32NextW(snapshot.0, &raw mut entry) } != 0;
    }
    hosts
}

/// Raise every thread the shell already owns to `THREAD_PRIORITY_ABOVE_NORMAL`.
///
/// Thread priority is not inherited by child processes, so the user's builds
/// keep running at their own priority. Only the shell's existing threads are
/// touched; a later scan in the retry worker covers threads created just after
/// spawn. Per-thread failures are logged at debug and never fail the tab.
#[cfg(target_os = "windows")]
fn boost_shell_threads(pid: u32) {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows_sys::Win32::System::Threading::{
        OpenThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL, THREAD_SET_INFORMATION,
    };

    if pid == 0 {
        return;
    }
    // Safety: no pointer arguments; the returned handle is checked below and
    // owned by SnapshotHandle.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot.is_null() || snapshot == INVALID_HANDLE_VALUE {
        log::debug!(
            "could not snapshot threads to boost the shell: {}",
            std::io::Error::last_os_error()
        );
        return;
    }
    let snapshot = SnapshotHandle(snapshot);

    // Safety: THREADENTRY32 is a plain C out-structure for which all-zero is
    // valid; `dwSize` is set to its exact size as the API requires.
    let mut entry = THREADENTRY32 {
        dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
        ..Default::default()
    };
    // Safety: `entry` is a live, correctly sized out-buffer for the snapshot's
    // full lifetime.
    let mut more = unsafe { Thread32First(snapshot.0, &raw mut entry) } != 0;
    while more {
        if entry.th32OwnerProcessID == pid {
            // Safety: no pointer arguments; the handle is null-checked, owns
            // only THREAD_SET_INFORMATION, and is closed exactly once.
            let thread = unsafe { OpenThread(THREAD_SET_INFORMATION, 0, entry.th32ThreadID) };
            if thread.is_null() {
                log::debug!(
                    "could not open shell thread {} to raise its priority: {}",
                    entry.th32ThreadID,
                    std::io::Error::last_os_error()
                );
            } else {
                let thread = ThreadHandle(thread);
                // Safety: the handle grants THREAD_SET_INFORMATION.
                let raised =
                    unsafe { SetThreadPriority(thread.0, THREAD_PRIORITY_ABOVE_NORMAL) } != 0;
                if !raised {
                    log::debug!(
                        "could not raise shell thread {} to above normal: {}",
                        entry.th32ThreadID,
                        std::io::Error::last_os_error()
                    );
                }
            }
        }
        // Safety: same live buffer, advanced to the next entry.
        more = unsafe { Thread32Next(snapshot.0, &raw mut entry) } != 0;
    }
}

/// Owned thread handle for [`boost_shell_threads`].
#[cfg(target_os = "windows")]
struct ThreadHandle(windows_sys::Win32::Foundation::HANDLE);

#[cfg(target_os = "windows")]
impl Drop for ThreadHandle {
    fn drop(&mut self) {
        // Safety: the handle came from a successful `OpenThread` and is closed
        // exactly once here.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
    }
}

/// Raise one live ConPTY host owned by this spawn to `ABOVE_NORMAL_PRIORITY_CLASS`.
///
/// A host created before `spawn_at` belongs to another tab and is left alone.
/// An already-boosted or older host is not a success, otherwise one tab's host
/// would stop another tab's scan.
#[cfg(target_os = "windows")]
fn boost_console_host(pid: u32, spawn_at: u64) -> HostBoost {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{
        ABOVE_NORMAL_PRIORITY_CLASS, GetPriorityClass, GetProcessTimes, OpenProcess,
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_INFORMATION, SetPriorityClass,
    };

    if pid == 0 {
        return HostBoost::Failed;
    }
    // Safety: no pointer arguments; the handle is null-checked and owned by
    // ProcessHandle. Only the priority class is changed.
    let handle = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SET_INFORMATION,
            0,
            pid,
        )
    };
    if handle.is_null() {
        log::debug!(
            "could not open process {pid} to raise its priority: {}",
            std::io::Error::last_os_error()
        );
        return HostBoost::Failed;
    }
    let handle = ProcessHandle(handle);

    // A snapshot entry can outlive its process; a stale PID may already belong
    // to something else, so a dead or failed query is never boosted.
    if !handle_is_alive(handle.0) {
        return HostBoost::Failed;
    }

    // Safety: the same handle grants PROCESS_QUERY_LIMITED_INFORMATION.
    let current = unsafe { GetPriorityClass(handle.0) };
    if current == 0 {
        log::debug!(
            "could not read the priority of process {pid}: {}",
            std::io::Error::last_os_error()
        );
        return HostBoost::Failed;
    }
    let already_boosted = !needs_priority_boost(current);

    // Safety: the same handle grants PROCESS_QUERY_LIMITED_INFORMATION and the
    // FILETIME out-parameters are live stack values.
    let mut created = FILETIME::default();
    let mut exited = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    let times = unsafe {
        GetProcessTimes(
            handle.0,
            &raw mut created,
            &raw mut exited,
            &raw mut kernel,
            &raw mut user,
        )
    } != 0;
    let created_at =
        times.then(|| (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime));

    if !host_is_ours(pid, created_at, spawn_at, already_boosted) {
        return if already_boosted {
            HostBoost::AlreadyBoosted
        } else {
            HostBoost::Older
        };
    }

    // Safety: the handle grants PROCESS_SET_INFORMATION and is closed exactly
    // once by ProcessHandle.
    let raised = unsafe { SetPriorityClass(handle.0, ABOVE_NORMAL_PRIORITY_CLASS) } != 0;
    if raised {
        log::debug!("raised process {pid} to above-normal priority");
        HostBoost::Raised
    } else {
        log::debug!(
            "could not raise process {pid} to above-normal priority: {}",
            std::io::Error::last_os_error()
        );
        HostBoost::Failed
    }
}

/// Folder a tab opens in when no launch folder, live shell cwd, or stored tab
/// cwd is known.
///
/// Explorer, the Start Menu and desktop shortcuts start the process in the
/// executable's own folder, so that value says nothing about where the user was
/// working. The user profile is the meaningful default there; a launch from a
/// real console in another folder is still honoured.
pub fn fallback_startup_dir() -> Option<PathBuf> {
    let process = std::env::current_dir().ok();
    let executable = std::env::current_exe().ok();
    pick_startup_dir(
        process.as_deref(),
        executable.as_deref(),
        home_dir().as_deref(),
    )
}

/// Choose between the process directory and the user profile.
///
/// The profile only replaces the process directory when the process was started
/// in the executable's own folder, and it never wins over a missing value when
/// no profile exists.
fn pick_startup_dir(
    process: Option<&Path>,
    executable: Option<&Path>,
    home: Option<&Path>,
) -> Option<PathBuf> {
    let from_executable_folder = process
        .zip(executable)
        .is_some_and(|(dir, exe)| is_executable_folder(dir, exe));
    if from_executable_folder && let Some(home) = home {
        return Some(home.to_path_buf());
    }
    process.or(home).map(Path::to_path_buf)
}

/// Whether `dir` is the folder holding `executable`.
///
/// Compared as text so this can run on the UI thread: `current_dir` is a stored
/// string and canonicalizing it can block on a dead network share. A launcher
/// that reports an 8.3 form of the same folder simply keeps the process
/// directory, which is the safe direction.
fn is_executable_folder(dir: &Path, executable: &Path) -> bool {
    let Some(exe_dir) = executable.parent() else {
        return false;
    };
    let dir = normalized(dir);
    let exe_dir = normalized(exe_dir);
    if cfg!(target_os = "windows") {
        dir.eq_ignore_ascii_case(&exe_dir)
    } else {
        dir == exe_dir
    }
}

/// Path text without a trailing separator, so `C:\folder\` and `C:\folder`
/// compare equal.
fn normalized(path: &Path) -> String {
    let text = path.to_string_lossy();
    let trimmed = text.trim_end_matches(['\\', '/']);
    if trimmed.is_empty() {
        text.into_owned()
    } else {
        trimmed.to_string()
    }
}

/// The signed-in user's profile folder.
fn home_dir() -> Option<PathBuf> {
    fn non_empty(name: &str) -> Option<std::ffi::OsString> {
        std::env::var_os(name).filter(|value| !value.is_empty())
    }

    #[cfg(target_os = "windows")]
    {
        if let Some(profile) = non_empty("USERPROFILE") {
            return Some(PathBuf::from(profile));
        }
        let drive = non_empty("HOMEDRIVE")?;
        let path = non_empty("HOMEPATH")?;
        let mut home = PathBuf::from(drive);
        home.push(path);
        Some(home)
    }
    #[cfg(not(target_os = "windows"))]
    {
        non_empty("HOME").map(PathBuf::from)
    }
}

/// Path of the wallpaper Windows currently has registered, if any.
///
/// Reads `SPI_GETDESKWALLPAPER` rather than guessing a file name, so a stock
/// Windows wallpaper is only used when the OS actually reports it. Returns
/// `None` for a solid-colour desktop or when the query fails.
#[cfg(target_os = "windows")]
pub fn current_wallpaper_path() -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SPI_GETDESKWALLPAPER, SystemParametersInfoW,
    };

    // The documented path limit is MAX_PATH, but Windows 11 can report longer
    // user paths; 32K WCHARs is the widest value the API accepts and costs only
    // 64 KiB of heap.
    let mut buffer = vec![0u16; 32_768];
    let size = u32::try_from(buffer.len()).ok()?;

    // Safety: buffer is a live, writable Vec<u16> with `size` WCHARs of
    // capacity, which is the length passed to the API. SystemParametersInfoW
    // writes at most that many and NUL-terminates its result. The pointer does
    // not outlive the call.
    let ok =
        unsafe { SystemParametersInfoW(SPI_GETDESKWALLPAPER, size, buffer.as_mut_ptr().cast(), 0) };
    if ok == 0 {
        return None;
    }

    let len = buffer
        .iter()
        .position(|&character| character == 0)
        .unwrap_or(buffer.len());
    if len == 0 {
        return None;
    }

    Some(PathBuf::from(std::ffi::OsString::from_wide(&buffer[..len])))
}

/// Path of the wallpaper Windows currently has registered, if any.
#[cfg(not(target_os = "windows"))]
pub fn current_wallpaper_path() -> Option<PathBuf> {
    None
}

/// Add up DWM composition and the user's transparency-effects setting.
///
/// A missing `EnableTransparency` value predates the setting and defaults to
/// enabled. `Some(0)` is the only value that disables the effects.
#[cfg(target_os = "windows")]
fn transparency_allowed(composition_enabled: bool, enable_transparency: Option<u32>) -> bool {
    composition_enabled && enable_transparency != Some(0)
}

/// Whether Windows will actually draw a native Mica/Acrylic backdrop.
///
/// Surface alpha capabilities only prove that a transparent backbuffer can be
/// created; they say nothing about DWM. `apply_mica` can also return `S_OK`
/// while Settings > Personalization > Colors > Transparency effects is off,
/// leaving an opaque window behind the material. Require DWM composition and
/// the per-user transparency value as well.
#[cfg(target_os = "windows")]
pub fn transparency_effects_enabled() -> bool {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::Graphics::Dwm::DwmIsCompositionEnabled;
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};

    let mut composition: windows_sys::core::BOOL = 0;
    // Safety: `composition` is a live BOOL and the call only writes to it.
    let result = unsafe { DwmIsCompositionEnabled(&mut composition) };
    let composition_enabled = result >= 0 && composition != 0;

    let key_path: Vec<u16> = r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let value_name: Vec<u16> = "EnableTransparency"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut value: u32 = 1;
    let mut size = u32::try_from(std::mem::size_of::<u32>()).unwrap_or(4);
    // Safety: both strings are null-terminated, `value` is a live u32, and
    // `size` is its size in bytes. RRF_RT_REG_DWORD makes the API reject a
    // value of another type instead of writing past the buffer.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key_path.as_ptr(),
            value_name.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            (&raw mut value).cast(),
            &mut size,
        )
    };
    let enable_transparency = (status == ERROR_SUCCESS).then_some(value);

    transparency_allowed(composition_enabled, enable_transparency)
}

/// Native materials are Windows-only.
#[cfg(not(target_os = "windows"))]
pub fn transparency_effects_enabled() -> bool {
    false
}

/// Apply the requested backdrop to `window`.
///
/// `theme_bg` is the theme background tint used by the legacy acrylic accent;
/// `opacity` is the user's background-opacity percentage (0-100). Mica failing
/// on Windows 10 is expected; the caller falls back to [`Material::Solid`] and
/// reports a toast. On non-Windows platforms this is a no-op.
pub fn apply(
    window: &impl HasWindowHandle,
    material: Material,
    dark: bool,
    theme_bg: Rgb,
    opacity: f32,
) -> Result<()> {
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (window, material, dark, theme_bg, opacity);
        Ok(())
    }

    #[cfg(target_os = "windows")]
    {
        let handle = window.window_handle()?;
        let RawWindowHandle::Win32(win32) = handle.as_raw() else {
            anyhow::bail!("window does not expose a Win32 handle");
        };
        let hwnd = win32.hwnd.get() as HWND;
        apply_theme(hwnd, dark);
        // Installed on the first material application: every later apply and
        // tint update writes into the state the subclass reads.
        install_acrylic_move_subclass(hwnd);

        match material {
            Material::Mica => {
                // Mica is the wallpaper-derived system backdrop. Disable any
                // legacy accent first, then extend the frame so DWM paints
                // Mica through the whole client area, and apply it last.
                set_acrylic_move_tint(hwnd, None);
                let _ = apply_accent(hwnd, ACCENT_DISABLED, 0, 0);
                if !extend_frame(hwnd, true) {
                    anyhow::bail!("DWM frame extension is unavailable");
                }
                window_vibrancy::apply_mica(window, Some(dark))?;
            }
            Material::Acrylic => {
                // The system backdrop only samples the wallpaper; the legacy
                // accent policy blurs whatever is actually behind the window.
                // Never extend the frame here: DWM would paint an opaque layer
                // over the accent.
                let _ = window_vibrancy::clear_mica(window);
                let _ = window_vibrancy::clear_acrylic(window);
                extend_frame(hwnd, false);
                let tint = acrylic_tint(theme_bg, opacity);
                let moving = set_acrylic_move_tint(hwnd, Some(tint));
                if !apply_accent(hwnd, acrylic_state(moving), 0, tint) {
                    anyhow::bail!("legacy acrylic accent policy is unavailable");
                }
            }
            Material::Solid => {
                // Clearing an effect that was never applied is not an error.
                set_acrylic_move_tint(hwnd, None);
                let _ = apply_accent(hwnd, ACCENT_DISABLED, 0, 0);
                let _ = window_vibrancy::clear_mica(window);
                let _ = window_vibrancy::clear_acrylic(window);
                extend_frame(hwnd, false);
            }
        }

        Ok(())
    }
}

/// Re-apply only the legacy acrylic tint after an opacity change.
///
/// This deliberately avoids `design::install`, a backdrop rebuild, or any
/// other material work so dragging the opacity slider stays cheap. `dark` is
/// accepted for symmetry with [`apply`]; the tint itself follows `theme_bg`.
pub fn apply_acrylic_tint(
    window: &impl HasWindowHandle,
    theme_bg: Rgb,
    dark: bool,
    opacity: f32,
) -> Result<()> {
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (window, theme_bg, dark, opacity);
        Ok(())
    }

    #[cfg(target_os = "windows")]
    {
        let _ = dark;
        let started = std::time::Instant::now();
        let handle = window.window_handle()?;
        let RawWindowHandle::Win32(win32) = handle.as_raw() else {
            anyhow::bail!("window does not expose a Win32 handle");
        };
        let hwnd = win32.hwnd.get() as HWND;
        let tint = acrylic_tint(theme_bg, opacity);
        let moving = set_acrylic_move_tint(hwnd, Some(tint));
        if !apply_accent(hwnd, acrylic_state(moving), 0, tint) {
            anyhow::bail!("legacy acrylic accent policy is unavailable");
        }
        log::debug!(
            "acrylic tint re-applied in {:?} (alpha={})",
            started.elapsed(),
            tint >> 24
        );
        Ok(())
    }
}

/// Build the `AABBGGRR` gradient colour for the legacy acrylic policy.
///
/// The RGB tint comes from the theme background. The alpha follows the
/// background-opacity slider: 255 at 0% (opaque theme colour) down to
/// [`ACRYLIC_TINT_ALPHA_OPAQUE`] at 100% (full blur).
#[cfg(target_os = "windows")]
fn acrylic_tint(theme_bg: Rgb, opacity: f32) -> u32 {
    let opacity = if opacity.is_finite() {
        opacity.clamp(0.0, 100.0) / 100.0
    } else {
        1.0
    };
    let alpha = (255.0 + (ACRYLIC_TINT_ALPHA_OPAQUE - 255.0) * opacity).round() as u32;
    (alpha << 24)
        | (u32::from(theme_bg.b) << 16)
        | (u32::from(theme_bg.g) << 8)
        | u32::from(theme_bg.r)
}

/// Show a blocking native error dialog for a failure that prevents startup.
///
/// Uses `MessageBoxW` so a debug build without a console and a release build
/// without any console both report the cause. The dialog owns no Rust memory
/// after it returns.
#[cfg(target_os = "windows")]
pub fn show_startup_error(message: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};

    let text: Vec<u16> = format!("{message}\0").encode_utf16().collect();
    let caption: Vec<u16> = "Illness\0".encode_utf16().collect();
    // Safety: both wide buffers are live, null-terminated, and outlive the
    // call; the owner handle is null because no app window exists yet.
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            caption.as_ptr(),
            MB_OK | MB_ICONERROR,
        );
    }
}

/// Show a startup failure when no native dialog API is available.
#[cfg(not(target_os = "windows"))]
pub fn show_startup_error(message: &str) {
    log::error!("{message}");
}

/// Set DWM dark mode and rounded corners. Failures are ignored: older Windows
/// versions reject individual attributes without affecting the rest.
#[cfg(target_os = "windows")]
fn apply_theme(hwnd: HWND, dark: bool) {
    let value = i32::from(dark);
    let attribute = u32::try_from(DWMWA_USE_IMMERSIVE_DARK_MODE).unwrap_or(20);
    if !set_attribute(hwnd, attribute, value) {
        let _ = set_attribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE_OLD, value);
    }
    let corners = u32::try_from(DWMWA_WINDOW_CORNER_PREFERENCE).unwrap_or(33);
    let _ = set_attribute(hwnd, corners, DWMWCP_ROUND);
}

/// Extend or reset the DWM frame. Returns whether DWM accepted the request.
///
/// Extending (negative margins) lets the system Mica backdrop reach the whole
/// client area. Resetting to zero margins undoes that when switching to a
/// material that must composite its own backdrop.
#[cfg(target_os = "windows")]
fn extend_frame(hwnd: HWND, extend: bool) -> bool {
    let margin = if extend { -1 } else { 0 };
    let margins = windows_sys::Win32::UI::Controls::MARGINS {
        cxLeftWidth: margin,
        cxRightWidth: margin,
        cyTopHeight: margin,
        cyBottomHeight: margin,
    };
    // Safety: hwnd is live for the duration of this call; `margins` is a
    // correctly laid out MARGINS owned by this stack frame.
    let result = unsafe { DwmExtendFrameIntoClientArea(hwnd, &margins) };
    result >= 0
}

/// Apply one accent policy. Returns `false` when the undocumented API is
/// unavailable on this Windows build.
///
/// # Safety
///
/// The call itself is the only unsafe operation. `hwnd` is a live window
/// handle supplied by the caller. `WINDOWCOMPOSITIONATTRIBDATA` points at a
/// stack `AccentPolicy` whose address and size are passed in the format the
/// API documents. The resolved function address comes from `user32.dll` and
/// is called with its documented signature.
#[cfg(target_os = "windows")]
fn apply_accent(hwnd: HWND, state: u32, flags: u32, gradient_color: u32) -> bool {
    let Some(function) = set_window_composition_attribute() else {
        return false;
    };
    let mut policy = AccentPolicy {
        accent_state: state,
        accent_flags: flags,
        gradient_color,
        animation_id: 0,
    };
    let mut data = WindowCompositionAttribData {
        attribute: WCA_ACCENT_POLICY,
        data: (&raw mut policy).cast(),
        size: std::mem::size_of::<AccentPolicy>(),
    };
    // Safety: see the function-level safety contract.
    unsafe { function(hwnd, &raw mut data) != 0 }
}

/// Legacy acrylic accent currently applied to one window.
///
/// The tint is shared with the move subclass: it must swap the accent state
/// without re-reading the config, and an opacity change during a drag must
/// keep the blurred state until the move loop ends.
#[cfg(target_os = "windows")]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct MoveAccent {
    /// Gradient colour of the active acrylic accent; `None` when the applied
    /// material is not legacy acrylic (Mica/Solid).
    tint: Option<u32>,
    /// A `WM_ENTERSIZEMOVE`/`WM_EXITSIZEMOVE` pair is in progress and the
    /// accent has been swapped to [`ACCENT_ENABLE_BLURBEHIND`].
    moving: bool,
    /// The pending modal loop is a caption drag (not a resize). Windows sends
    /// `WM_ENTERSIZEMOVE` for both, so the hit test is recorded beforehand.
    pending_move: bool,
}

#[cfg(target_os = "windows")]
impl MoveAccent {
    fn set_tint(&mut self, tint: Option<u32>) {
        self.tint = tint;
        if tint.is_none() {
            // Mica/Solid clear the accent; an in-flight move has nothing left
            // to restore.
            self.moving = false;
            self.pending_move = false;
        }
    }

    /// Accent state and tint to apply for a move transition. `None` means the
    /// message is not actionable for this window (not acrylic, already in the
    /// requested state, no tint recorded, or the pending loop is a resize).
    fn move_action(&self, entering: bool) -> Option<(u32, u32)> {
        if entering {
            if self.moving || !self.pending_move {
                return None;
            }
            let tint = self.tint?;
            accent_state_for_move(true).map(|state| (state, tint))
        } else {
            if !self.moving {
                return None;
            }
            let tint = self.tint?;
            Some((ACCENT_ENABLE_ACRYLICBLURBEHIND, tint))
        }
    }
}

/// Accent state to use while a legacy acrylic window is being moved.
///
/// `active` is whether acrylic (state 4) is currently applied. The cheap blur
/// is only valid for an active accent with a recorded tint.
#[cfg(target_os = "windows")]
fn accent_state_for_move(active: bool) -> Option<u32> {
    active.then_some(ACCENT_ENABLE_BLURBEHIND)
}

/// State to apply when setting an acrylic tint: stay in the cheap blur state
/// if a move is already in progress (opacity slider changed mid-drag).
#[cfg(target_os = "windows")]
fn acrylic_state(moving: bool) -> u32 {
    accent_state_for_move(moving).unwrap_or(ACCENT_ENABLE_ACRYLICBLURBEHIND)
}

#[cfg(target_os = "windows")]
fn move_accents() -> &'static Mutex<HashMap<usize, MoveAccent>> {
    static ACCENTS: OnceLock<Mutex<HashMap<usize, MoveAccent>>> = OnceLock::new();
    ACCENTS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(target_os = "windows")]
fn move_subclasses() -> &'static Mutex<HashMap<usize, ()>> {
    static SUBCLASSES: OnceLock<Mutex<HashMap<usize, ()>>> = OnceLock::new();
    SUBCLASSES.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(target_os = "windows")]
fn hwnd_key(hwnd: HWND) -> usize {
    hwnd as usize
}

/// Record the tint of the applied acrylic accent and return whether a move is
/// in progress, so callers can preserve state 3 until the move ends.
#[cfg(target_os = "windows")]
fn set_acrylic_move_tint(hwnd: HWND, tint: Option<u32>) -> bool {
    let mut accents = move_accents()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let accent = accents.entry(hwnd_key(hwnd)).or_default();
    accent.set_tint(tint);
    accent.moving
}

/// Install the modal-move subclass once per window. A failure disables only
/// this optimization; the acrylic accent remains in its normal state.
#[cfg(target_os = "windows")]
fn install_acrylic_move_subclass(hwnd: HWND) {
    let key = hwnd_key(hwnd);
    {
        let subclasses = move_subclasses()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if subclasses.contains_key(&key) {
            return;
        }
    }
    let subclass: SUBCLASSPROC = Some(acrylic_move_subclass);
    // Safety: `hwnd` is the live window handle supplied by `apply`; the
    // subclass proc is a valid `extern "system"` function and the id is
    // unique to this module. `comctl32.dll` owns the subclass until the
    // window is destroyed.
    let installed = unsafe { SetWindowSubclass(hwnd, subclass, ACRYLIC_MOVE_SUBCLASS_ID, 0) } != 0;
    if installed {
        move_subclasses()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(key, ());
        move_accents()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(key)
            .or_default();
        log::debug!("acrylic move subclass installed");
    } else {
        log::warn!("could not install the acrylic move subclass; moves keep the acrylic accent");
    }
}

/// Record whether the gesture Windows is about to enter is a caption drag or a
/// resize. `WM_ENTERSIZEMOVE` covers both, and only a move benefits from the
/// cheaper blur.
#[cfg(target_os = "windows")]
fn set_pending_move(hwnd: HWND, pending: bool) {
    let mut accents = move_accents()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(accent) = accents.get_mut(&hwnd_key(hwnd)) {
        accent.pending_move = pending;
    }
}

/// Drop per-window state when the window is destroyed. comctl32 removes the
/// subclass itself; the maps must not outlive the HWND or a recycled handle
/// would inherit stale state.
#[cfg(target_os = "windows")]
fn forget_window(hwnd: HWND) {
    let key = hwnd_key(hwnd);
    move_accents()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(&key);
    move_subclasses()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(&key);
}

/// Swap between acrylic (4) and blur (3) while a modal move loop runs.
///
/// The state lock is never held across `SetWindowCompositionAttribute`: the
/// call runs inside the window procedure, and a synchronous re-entry would
/// deadlock the non-reentrant mutex.
#[cfg(target_os = "windows")]
fn swap_accent_for_move(hwnd: HWND, entering: bool) {
    let action = {
        let mut accents = move_accents()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(accent) = accents.get_mut(&hwnd_key(hwnd)) else {
            return;
        };
        let action = accent.move_action(entering);
        if !entering {
            // The move loop is over either way. Never leave a failed restore
            // latched: the next apply must return to acrylic.
            accent.moving = false;
            accent.pending_move = false;
        }
        action
    };
    let Some((state, tint)) = action else {
        return;
    };
    let applied = apply_accent(hwnd, state, 0, tint);
    {
        let mut accents = move_accents()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(accent) = accents.get_mut(&hwnd_key(hwnd))
            && entering
        {
            accent.moving = applied;
        }
    }
    if applied {
        log::debug!(
            "legacy acrylic accent {} for window move (state={state}, alpha={})",
            if entering {
                "swapped to blur"
            } else {
                "restored"
            },
            tint >> 24
        );
    } else {
        log::warn!(
            "could not {} legacy acrylic accent for window move",
            if entering { "swap" } else { "restore" }
        );
    }
}

/// Observe the modal move messages and the gesture that precedes them; every
/// message reaches `DefSubclassProc`, including the ones handled here.
#[cfg(target_os = "windows")]
unsafe extern "system" fn acrylic_move_subclass(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    match message {
        WM_NCLBUTTONDOWN => set_pending_move(hwnd, wparam == HTCAPTION as usize),
        WM_SYSCOMMAND => {
            let command = (wparam as u32) & 0xFFF0;
            if command == SC_MOVE {
                set_pending_move(hwnd, true);
            } else if command == SC_SIZE {
                set_pending_move(hwnd, false);
            }
        }
        WM_ENTERSIZEMOVE => swap_accent_for_move(hwnd, true),
        WM_EXITSIZEMOVE => swap_accent_for_move(hwnd, false),
        WM_NCDESTROY => {
            // Safety: removing this subclass from its own WM_NCDESTROY is the
            // documented cleanup path; the proc and id match the install.
            unsafe {
                RemoveWindowSubclass(hwnd, Some(acrylic_move_subclass), ACRYLIC_MOVE_SUBCLASS_ID);
            }
            forget_window(hwnd);
        }
        _ => {}
    }
    // Safety: forwards the original message to the rest of the subclass
    // chain/window procedure with the untouched parameters.
    unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
}

/// Set one 32-bit DWM attribute. Returns `true` when the compositor accepted it.
///
/// # Safety
///
/// `hwnd` comes from a live `RawWindowHandle::Win32` owned by the caller and remains valid
/// for the duration of the call. `pvattribute` points at `value`, a stack `i32`, and
/// `cbattribute` is exactly `size_of::<i32>()`, as required by `DwmSetWindowAttribute`.
#[cfg(target_os = "windows")]
fn set_attribute(hwnd: HWND, attribute: u32, value: i32) -> bool {
    let size = u32::try_from(std::mem::size_of::<i32>()).unwrap_or(4);
    let result =
        unsafe { DwmSetWindowAttribute(hwnd, attribute, &value as *const i32 as *const _, size) };
    result >= 0
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use super::{
        ACCENT_ENABLE_ACRYLICBLURBEHIND, ACCENT_ENABLE_BLURBEHIND, CONSOLE_PRIORITY_ATTEMPTS,
        CONSOLE_PRIORITY_FAST_ATTEMPTS, CONSOLE_PRIORITY_FAST_RETRY, CONSOLE_PRIORITY_RETRY,
        CONTEXT_MENU_VERBS, ContextMenuAction, MoveAccent, PEB_LAYOUT_32, PEB_LAYOUT_64, PathKind,
        PathProbeCache, ProbeBatch, accent_state_for_move, acrylic_state, acrylic_tint,
        automatic_context_menu_allowed, conpty_pin_inbox, console_priority_delay,
        context_menu_action, context_menu_command, context_menu_command_executable,
        context_menu_command_valid, context_menu_verbs_complete, curdir_header_layout,
        decode_curdir_header, decode_curdir_text, entry_is_conpty_host, host_is_ours,
        is_executable_folder, is_network_path, machine_is_wow64, needs_priority_boost,
        path_has_prefix, path_kind_with_deadline, peb_layout_for_target, pick_startup_dir,
        plausible_process_path, priority_boost_disabled, probe_paths, probe_with_deadline,
        process_cwd, strip_nt_prefix, terminated_curdir_len, transparency_allowed,
        unique_probe_paths,
    };
    use illness_core::Rgb;
    use std::ffi::OsStr;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    #[test]
    fn shortcut_launch_uses_the_user_profile_not_the_executable_folder() {
        let exe = Path::new(r"C:\Program Files\Illness\illness.exe");
        let home = PathBuf::from(r"C:\Users\Simulp");

        let picked = pick_startup_dir(
            Some(Path::new(r"C:\Program Files\Illness")),
            Some(exe),
            Some(&home),
        );

        assert_eq!(picked.as_deref(), Some(home.as_path()));
    }

    #[test]
    fn console_launch_folder_is_kept() {
        let exe = Path::new(r"C:\Program Files\Illness\illness.exe");
        let home = PathBuf::from(r"C:\Users\Simulp");
        let console = PathBuf::from(r"C:\work\project");

        let picked = pick_startup_dir(Some(&console), Some(exe), Some(&home));

        assert_eq!(picked.as_deref(), Some(console.as_path()));
    }

    #[test]
    fn executable_folder_without_a_profile_falls_back_to_the_process_directory() {
        let exe = Path::new(r"C:\Program Files\Illness\illness.exe");
        let folder = PathBuf::from(r"C:\Program Files\Illness");

        assert_eq!(
            pick_startup_dir(Some(&folder), Some(exe), None).as_deref(),
            Some(folder.as_path())
        );
        assert_eq!(pick_startup_dir(None, Some(exe), None), None);
    }

    #[test]
    fn a_missing_process_directory_still_prefers_the_profile() {
        let exe = Path::new(r"C:\Program Files\Illness\illness.exe");
        let home = PathBuf::from(r"C:\Users\Simulp");

        assert_eq!(
            pick_startup_dir(None, Some(exe), Some(&home)).as_deref(),
            Some(home.as_path())
        );
    }

    #[test]
    fn executable_folder_matching_ignores_case_and_trailing_separators() {
        let exe = Path::new(r"C:\Program Files\Illness\illness.exe");

        assert!(is_executable_folder(
            Path::new(r"c:\program files\illness\"),
            exe
        ));
        assert!(!is_executable_folder(
            Path::new(r"C:\Program Files\Illness\bin"),
            exe
        ));
        assert!(!is_executable_folder(Path::new(r"C:\"), exe));
    }

    #[test]
    fn peb_dos_paths_lose_object_manager_prefixes() {
        assert_eq!(
            strip_nt_prefix(r"\??\C:\CLI\folder"),
            PathBuf::from(r"C:\CLI\folder")
        );
        assert_eq!(strip_nt_prefix(r"\\?\E:\Files"), PathBuf::from(r"E:\Files"));
        assert_eq!(
            strip_nt_prefix(r"\??\UNC\server\share"),
            PathBuf::from(r"\\server\share")
        );
        assert_eq!(strip_nt_prefix(r"C:\plain"), PathBuf::from(r"C:\plain"));
    }

    /// Build the exact fixture `console_tree_pids` hands to the matcher: a
    /// zeroed `PROCESSENTRY32W` whose fixed `szExeFile` buffer holds `exe_name`
    /// followed by NUL padding.
    fn process_entry(
        exe_name: &str,
    ) -> windows_sys::Win32::System::Diagnostics::ToolHelp::PROCESSENTRY32W {
        let mut entry = windows_sys::Win32::System::Diagnostics::ToolHelp::PROCESSENTRY32W {
            dwSize: std::mem::size_of::<
                windows_sys::Win32::System::Diagnostics::ToolHelp::PROCESSENTRY32W,
            >() as u32,
            ..Default::default()
        };
        for (slot, unit) in entry.szExeFile.iter_mut().zip(exe_name.encode_utf16()) {
            *slot = unit;
        }
        entry
    }

    #[test]
    fn conpty_hosts_are_recognized_by_the_shipping_matcher() {
        // Exercises `entry_is_conpty_host` itself, not a test-only copy of the
        // name list, so production matching cannot regress unnoticed.
        assert!(entry_is_conpty_host(&process_entry("conhost.exe")));
        assert!(entry_is_conpty_host(&process_entry("OpenConsole.exe")));
        assert!(entry_is_conpty_host(&process_entry("OPENCONSOLE.EXE")));
        assert!(!entry_is_conpty_host(&process_entry("cmd.exe")));
        assert!(!entry_is_conpty_host(&process_entry("")));
        assert!(!entry_is_conpty_host(&process_entry("conhost.exe.bak")));
        // The buffer is NUL-terminated in practice, but trailing units after the
        // terminator must be ignored rather than compared.
        assert!(entry_is_conpty_host(&process_entry(
            "conhost.exe\0junk.exe"
        )));
        // A full buffer with no terminator at all must not panic.
        let mut full = process_entry("");
        full.szExeFile.fill(u16::from(b'x'));
        assert!(!entry_is_conpty_host(&full));
    }

    #[test]
    fn only_a_class_below_above_normal_needs_a_boost() {
        use windows_sys::Win32::System::Threading::{
            ABOVE_NORMAL_PRIORITY_CLASS, BELOW_NORMAL_PRIORITY_CLASS, HIGH_PRIORITY_CLASS,
            IDLE_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS, REALTIME_PRIORITY_CLASS,
        };

        for below in [
            IDLE_PRIORITY_CLASS,
            BELOW_NORMAL_PRIORITY_CLASS,
            NORMAL_PRIORITY_CLASS,
        ] {
            assert!(needs_priority_boost(below), "{below:#x} must be raised");
        }
        for above in [
            ABOVE_NORMAL_PRIORITY_CLASS,
            HIGH_PRIORITY_CLASS,
            REALTIME_PRIORITY_CLASS,
        ] {
            assert!(
                !needs_priority_boost(above),
                "{above:#x} is already above normal"
            );
        }
    }

    #[test]
    fn console_priority_schedule_matches_its_documented_ceiling() {
        let mut delays = Vec::new();
        for attempt in 0..CONSOLE_PRIORITY_ATTEMPTS {
            match console_priority_delay(attempt) {
                Some(delay) => delays.push(delay),
                None => break,
            }
        }

        let mut expected = vec![CONSOLE_PRIORITY_FAST_RETRY; CONSOLE_PRIORITY_FAST_ATTEMPTS];
        expected.resize(CONSOLE_PRIORITY_ATTEMPTS - 1, CONSOLE_PRIORITY_RETRY);
        assert_eq!(delays, expected);
        assert_eq!(
            delays.iter().sum::<Duration>(),
            Duration::from_millis(525),
            "15 ms x 3 + 120 ms x 4"
        );
        assert_eq!(console_priority_delay(CONSOLE_PRIORITY_ATTEMPTS), None);
    }

    #[test]
    fn only_this_spawns_unboosted_host_ends_the_scan() {
        let spawn_at = 100;
        assert!(
            host_is_ours(42, Some(spawn_at), spawn_at, false),
            "our own fresh host counts"
        );
        assert!(
            !host_is_ours(42, Some(spawn_at - 1), spawn_at, false),
            "an older host belongs to another tab"
        );
        assert!(
            !host_is_ours(42, Some(spawn_at + 1), spawn_at, true),
            "an already boosted host must not end the scan"
        );
        assert!(
            !host_is_ours(42, None, spawn_at, false),
            "an unknown creation time is not this spawn's"
        );
        assert!(!host_is_ours(0, Some(spawn_at + 1), spawn_at, false));
    }

    #[test]
    fn env_switches_are_parsed_by_value() {
        assert!(conpty_pin_inbox(None), "unset defaults to inbox");
        assert!(conpty_pin_inbox(Some("")), "empty defaults to inbox");
        assert!(conpty_pin_inbox(Some("inbox")));
        assert!(!conpty_pin_inbox(Some("auto")));
        assert!(!conpty_pin_inbox(Some(" AUTO ")));
        assert!(
            conpty_pin_inbox(Some("OpenConsole")),
            "an unknown value falls back to inbox"
        );

        assert!(!priority_boost_disabled(None));
        for keep in ["", "0", "false", "no", "off", " FALSE ", " No "] {
            assert!(
                !priority_boost_disabled(Some(keep)),
                "{keep:?} must keep boosting"
            );
        }
        for disable in ["1", "true", "on", "yes", " YES "] {
            assert!(
                priority_boost_disabled(Some(disable)),
                "{disable:?} disables"
            );
        }
    }

    #[test]
    fn process_cwd_reads_this_process() {
        let expected = std::env::current_dir().expect("test process cwd");
        let actual = process_cwd(std::process::id()).expect("own PEB is readable");
        assert_eq!(actual, expected);
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn process_cwd_reads_a_32_bit_wow64_process() {
        use std::process::{Command, Stdio};

        let Some(system_root) = std::env::var_os("SystemRoot") else {
            return;
        };
        let cmd = PathBuf::from(system_root).join("SysWOW64").join("cmd.exe");
        if !cmd.is_file() {
            return;
        }
        let expected = std::env::current_dir().expect("test process cwd");
        let mut child = Command::new(cmd)
            .args(["/c", "ping", "-n", "5", "127.0.0.1"])
            .current_dir(&expected)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn 32-bit cmd.exe");

        let mut actual = None;
        for _ in 0..40 {
            if let Some(cwd) = process_cwd(child.id()) {
                actual = Some(cwd);
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(actual, Some(expected), "WOW64 PEB offsets must be selected");
    }

    #[test]
    fn context_menu_commands_quote_the_executable_and_placeholder() {
        let exe = OsStr::new(r"C:\Program Files\Illness\illness.exe");

        assert_eq!(
            context_menu_command(exe, "%V"),
            r#""C:\Program Files\Illness\illness.exe" "%V""#
        );
        assert_eq!(
            context_menu_command(exe, "%1"),
            r#""C:\Program Files\Illness\illness.exe" "%1""#
        );
    }

    #[test]
    fn context_menu_covers_folder_background_folder_and_drive() {
        assert!(CONTEXT_MENU_VERBS.iter().any(|(key, placeholder)| {
            key.ends_with(r"Directory\Background\shell\Illness") && *placeholder == "%V"
        }));
        assert!(CONTEXT_MENU_VERBS.iter().any(|(key, placeholder)| {
            key.ends_with(r"Directory\shell\Illness") && *placeholder == "%1"
        }));
        assert!(CONTEXT_MENU_VERBS.iter().any(|(key, placeholder)| {
            key.ends_with(r"Drive\shell\Illness") && *placeholder == "%1"
        }));
    }

    #[test]
    fn automatic_registration_only_runs_from_a_stable_install() {
        let roots = vec![
            PathBuf::from(r"C:\Program Files"),
            PathBuf::from(r"C:\Users\me\AppData\Local\Programs"),
        ];
        let temp = Path::new(r"C:\Users\me\AppData\Local\Temp");

        assert!(automatic_context_menu_allowed(
            Path::new(r"C:\Program Files\Illness\illness.exe"),
            &roots,
            temp,
            false
        ));
        assert!(automatic_context_menu_allowed(
            Path::new(r"C:\Users\me\AppData\Local\Programs\Illness\illness.exe"),
            &roots,
            temp,
            false
        ));
        // A debug/release build must not repoint an installed verb.
        assert!(!automatic_context_menu_allowed(
            Path::new(r"C:\Users\me\src\Illness\target\debug\illness.exe"),
            &roots,
            temp,
            false
        ));
        assert!(!automatic_context_menu_allowed(
            Path::new(r"C:\Users\me\src\Illness\target\x86_64-pc-windows-msvc\release\illness.exe"),
            &roots,
            temp,
            false
        ));
        // Temporary and USB copies are also transient.
        assert!(!automatic_context_menu_allowed(
            Path::new(r"C:\Users\me\AppData\Local\Temp\illness\illness.exe"),
            &roots,
            temp,
            false
        ));
        assert!(!automatic_context_menu_allowed(
            Path::new(r"C:\Program Files\Illness\illness.exe"),
            &roots,
            temp,
            true
        ));
    }

    #[test]
    fn path_prefixes_compare_case_insensitively() {
        assert!(path_has_prefix(
            Path::new(r"c:\program files\illness\illness.exe"),
            Path::new(r"C:\Program Files")
        ));
        assert!(!path_has_prefix(
            Path::new(r"C:\Program Files (x86)\Illness\illness.exe"),
            Path::new(r"C:\Program Files\Illness")
        ));
    }

    #[test]
    fn context_menu_command_paths_are_extracted_and_validated() {
        let exe = std::env::current_exe().expect("test executable");
        let quoted = context_menu_command(exe.as_os_str(), "%V");
        assert_eq!(
            context_menu_command_executable(&quoted),
            Some(exe.to_str().expect("unicode test path"))
        );
        assert!(context_menu_command_valid(&quoted));
        assert_eq!(
            context_menu_command_executable("illness.exe \"%1\""),
            Some("illness.exe")
        );
        assert_eq!(
            context_menu_command_executable("\"C:\\missing\\illness.exe\" \"%V\""),
            Some(r"C:\missing\illness.exe")
        );
        assert!(!context_menu_command_valid(""));
        assert!(!context_menu_command_valid("\"\" \"%V\""));
        assert!(
            !context_menu_command_valid(r#""C:\definitely-missing-illness.exe" "%V""#),
            "a command naming a deleted executable is not authoritative"
        );
    }

    #[test]
    fn complete_machine_verbs_need_all_three_valid_commands() {
        let exe = std::env::current_exe().expect("test executable");
        let command = context_menu_command(exe.as_os_str(), "%V");
        let complete = vec![Some(command.clone()), Some(command.clone()), Some(command)];
        assert!(context_menu_verbs_complete(&complete));

        let partial = vec![Some(complete[0].clone().unwrap()), None, None];
        assert!(
            !context_menu_verbs_complete(&partial),
            "a single leftover HKLM verb must not suppress HKCU repair"
        );
        let stale = vec![
            Some(r#""C:\missing\illness.exe" "%V""#.to_string()),
            Some(complete[0].clone().unwrap()),
            Some(complete[0].clone().unwrap()),
        ];
        assert!(!context_menu_verbs_complete(&stale));
        assert!(!context_menu_verbs_complete(&[]));
    }

    #[test]
    fn installer_registers_hklm_and_cleans_loaded_user_hives() {
        const TEMPLATE: &str = include_str!("../installer.nsi");
        assert!(TEMPLATE.contains("!insertmacro RegisterExplorerVerbs HKLM"));
        assert!(TEMPLATE.contains("!insertmacro UnregisterExplorerVerbs HKLM"));
        assert!(TEMPLATE.contains("!macro RegisterExplorerVerb ROOT CLASS PLACEHOLDER"));
        assert!(
            TEMPLATE.contains(r#"WriteRegStr ${ROOT} "Software\Classes\${CLASS}\shell\Illness""#),
            "the macro must write classes relative to its root"
        );
        assert!(
            TEMPLATE.contains("EnumRegKey $1 HKU"),
            "per-machine install/uninstall must clean loaded per-user hives"
        );
        assert!(
            TEMPLATE.contains(r#"DeleteRegKey HKU "$1\Software\Classes\Directory\shell\Illness""#),
            "the enumerated hives must have their Illness verbs removed"
        );
        assert!(
            TEMPLATE.contains("offline"),
            "the installer must document the offline-hive limit"
        );
        let install = TEMPLATE
            .find("Section Install")
            .expect("install section exists");
        let install_tail = &TEMPLATE[install..];
        let hklm = install_tail
            .find("RegisterExplorerVerbs HKLM")
            .expect("per-machine registration exists");
        let hkcu_cleanup = install_tail
            .find("UnregisterExplorerVerbsFromLoadedHives")
            .expect("installer cleans loaded hives");
        assert!(hklm < hkcu_cleanup);

        let call = TEMPLATE
            .find("Call un.RemoveExplorerVerbs")
            .expect("uninstall calls the verb cleanup");
        let delete = TEMPLATE
            .find(r#"Delete "$INSTDIR\${MAINBINARYNAME}.exe""#)
            .expect("uninstall deletes the binary");
        assert!(call < delete, "verbs must be removed before $INSTDIR");
    }

    #[test]
    fn runtime_repairs_hkcu_only_while_machine_verbs_are_incomplete() {
        // A complete machine menu is authoritative; repair the current user's
        // stale copy instead of shadowing it with another verb.
        assert_eq!(
            context_menu_action(false, true, true),
            ContextMenuAction::RemoveStaleHkcu
        );
        // An explicit --register-context-menu also repairs instead of skipping.
        assert_eq!(
            context_menu_action(true, false, true),
            ContextMenuAction::RemoveStaleHkcu
        );
        // Program Files with a partial/stale HKLM writes the HKCU repair.
        assert_eq!(
            context_menu_action(false, true, false),
            ContextMenuAction::WriteHkcu
        );
        assert_eq!(
            context_menu_action(true, false, false),
            ContextMenuAction::WriteHkcu
        );
        // A transient debug/USB copy still leaves an installed menu alone.
        assert_eq!(
            context_menu_action(false, false, false),
            ContextMenuAction::Skip
        );
        assert_eq!(
            context_menu_action(false, false, true),
            ContextMenuAction::Skip
        );
    }

    #[test]
    fn network_paths_use_the_probe_pool_and_local_paths_do_not() {
        assert!(!is_network_path(&std::env::temp_dir()));
        assert!(is_network_path(Path::new(r"\\server\share\folder")));
        assert!(is_network_path(Path::new(r"\\?\UNC\server\share")));
    }

    #[test]
    fn deadline_probe_gives_up_on_a_blocked_probe() {
        assert_eq!(
            probe_with_deadline(Duration::from_millis(20), || {
                std::thread::sleep(Duration::from_millis(250));
                true
            }),
            None
        );
        assert_eq!(
            probe_with_deadline(Duration::from_millis(250), || true),
            Some(true)
        );
    }

    #[test]
    fn probe_paths_dedupes_and_reports_tri_state() {
        let dir = std::env::temp_dir();
        let missing = std::env::temp_dir().join(format!(
            "illness-probe-missing-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|time| time.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&missing);

        let kinds = probe_paths(
            &[dir.clone(), dir.clone(), missing.clone(), missing.clone()],
            Duration::from_secs(2),
        );
        assert_eq!(
            kinds,
            vec![
                PathKind::Dir,
                PathKind::Dir,
                PathKind::NotDir,
                PathKind::NotDir
            ],
            "duplicate candidates share one probe and keep the timeout distinction"
        );
        assert_eq!(
            path_kind_with_deadline(&missing, Duration::from_secs(2)),
            PathKind::NotDir
        );
    }

    #[test]
    fn unique_probe_paths_keeps_priority_order() {
        let a = PathBuf::from(r"C:\a");
        let b = PathBuf::from(r"C:\b");
        let (unique, mapping) = unique_probe_paths(&[a.clone(), b.clone(), a.clone(), b.clone()]);
        assert_eq!(unique, vec![a, b]);
        assert_eq!(mapping, vec![0, 1, 0, 1]);
    }

    #[test]
    fn path_probe_cache_reuses_only_short_negative_and_timeout_results() {
        let mut cache = PathProbeCache::default();
        let now = Instant::now();
        let path = Path::new(r"C:\dead");

        cache.put_result(path, PathKind::NotDir, now);
        assert_eq!(
            cache.get(path, now + Duration::from_secs(1)),
            Some(PathKind::NotDir)
        );
        assert_eq!(cache.get(path, now + Duration::from_secs(6)), None);

        cache.put_result(path, PathKind::Timeout, now);
        assert_eq!(
            cache.get(path, now + Duration::from_secs(1)),
            Some(PathKind::Timeout)
        );
        assert_eq!(cache.get(path, now + Duration::from_secs(11)), None);

        cache.put_result(path, PathKind::Dir, now);
        assert_eq!(
            cache.get(path, now),
            None,
            "a directory must be re-checked after deletion"
        );
    }

    #[test]
    fn stale_timeout_cannot_overwrite_a_newer_completed_probe() {
        let mut cache = PathProbeCache::default();
        let now = Instant::now();
        let path = Path::new(r"\\cold\share");

        // Tab A starts a request, then the pool completes a Dir after the
        // caller deadline. A stale timeout for A must not replace that result.
        let request = cache.next_generation();
        cache.put_result(path, PathKind::Dir, now);
        cache.put_timeout(path, request, now + Duration::from_millis(1));
        assert_eq!(
            cache.entries.get(path).map(|entry| entry.kind),
            Some(PathKind::Dir),
            "a completed Dir must survive an older request's timeout"
        );

        // A timeout from an even newer request still must not replace a known
        // completed directory; positive results are rechecked, never overridden.
        let newer = cache.next_generation();
        cache.put_timeout(path, newer, now + Duration::from_millis(2));
        assert_eq!(
            cache.entries.get(path).map(|entry| entry.kind),
            Some(PathKind::Dir)
        );

        // A path without a completed positive result still caches its timeout
        // until a real result arrives.
        let fresh = Path::new(r"\\cold\share\fresh");
        cache.put_timeout(fresh, newer, now + Duration::from_millis(2));
        assert_eq!(
            cache.entries.get(fresh).map(|entry| entry.kind),
            Some(PathKind::Timeout)
        );
        cache.put_result(fresh, PathKind::Dir, now + Duration::from_millis(3));
        assert_eq!(
            cache.entries.get(fresh).map(|entry| entry.kind),
            Some(PathKind::Dir)
        );
    }

    fn pending_batch(
        path: PathBuf,
        request: u64,
    ) -> (
        ProbeBatch,
        crossbeam_channel::Sender<(usize, PathKind)>,
        Arc<Mutex<PathProbeCache>>,
    ) {
        let (reply, receiver) = crossbeam_channel::unbounded();
        let cache = Arc::new(Mutex::new(PathProbeCache::default()));
        (
            ProbeBatch {
                mapping: vec![0],
                unique: vec![path],
                results: vec![None],
                pending: vec![(0, request)],
                replies: Some(receiver),
                cache: Arc::clone(&cache),
            },
            reply,
            cache,
        )
    }

    #[test]
    fn probe_batch_deadline_is_bounded_and_kind_is_timeout() {
        let (batch, _reply, _cache) = pending_batch(PathBuf::from(r"\\cold\share"), 1);
        let started = Instant::now();
        let kinds = batch.wait(Duration::from_millis(30), None);
        assert_eq!(kinds, vec![PathKind::Timeout]);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "a dead network probe must not hold the spawn worker past its own deadline"
        );
    }

    #[test]
    fn probe_batch_cancellation_stops_the_wait() {
        let (batch, _reply, _cache) = pending_batch(PathBuf::from(r"\\cold\share"), 1);
        let cancel = AtomicBool::new(true);
        let started = Instant::now();
        let kinds = batch.wait(Duration::from_secs(5), Some(&cancel));
        assert_eq!(kinds, vec![PathKind::Timeout]);
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn curdir_header_is_decoded_from_one_read() {
        assert_eq!(curdir_header_layout(4), Some((4, 8)));
        assert_eq!(curdir_header_layout(8), Some((8, 16)));
        assert_eq!(curdir_header_layout(16), None);

        let mut header = [0u8; 16];
        header[0..2].copy_from_slice(&8u16.to_le_bytes());
        header[2..4].copy_from_slice(&10u16.to_le_bytes());
        header[8..16].copy_from_slice(&0x1234usize.to_le_bytes());
        assert_eq!(decode_curdir_header(&header, 8), Some((8, 0x1234)));
        // The 32-bit view of this 64-bit header points at zero.
        assert_eq!(decode_curdir_header(&header, 4), None);

        let mut header32 = [0u8; 8];
        header32[0..2].copy_from_slice(&6u16.to_le_bytes());
        header32[2..4].copy_from_slice(&6u16.to_le_bytes());
        header32[4..8].copy_from_slice(&0x5678u32.to_le_bytes());
        assert_eq!(decode_curdir_header(&header32, 4), Some((6, 0x5678)));

        header[0..2].copy_from_slice(&7u16.to_le_bytes());
        assert_eq!(decode_curdir_header(&header, 8), None, "odd length");
        header[0..2].copy_from_slice(&0u16.to_le_bytes());
        assert_eq!(decode_curdir_header(&header, 8), None, "empty length");
        assert_eq!(decode_curdir_header(&header[..8], 8), None, "truncated");
    }

    #[test]
    fn curdir_text_requires_a_nul_terminator_and_rejects_junk() {
        fn utf16(text: &str) -> Vec<u8> {
            text.encode_utf16()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>()
        }

        let with_nul = utf16("C:\\Users\\Public\u{0}");
        assert_eq!(
            decode_curdir_text(&with_nul),
            Some(PathBuf::from(r"C:\Users\Public"))
        );
        assert_eq!(decode_curdir_text(&utf16("C:\\missing-nul")), None);
        assert_eq!(decode_curdir_text(&utf16("C:\\bad\u{0}junk\u{0}")), None);
        assert_eq!(decode_curdir_text(&[0xff, 0x00]), None);
        assert_eq!(decode_curdir_text(&[]), None);
        assert_eq!(decode_curdir_text(&utf16("relative\u{0}")), None);

        // CURDIR.Length may or may not include the terminator; accept exactly
        // those two forms and reject a non-NUL extra unit.
        let path_without_nul = utf16("C:\\Users\\Public");
        let declared = path_without_nul.len();
        let mut terminated_after = path_without_nul.clone();
        terminated_after.extend_from_slice(&[0, 0]);
        assert_eq!(
            terminated_curdir_len(&terminated_after, declared),
            Some(terminated_after.len())
        );
        assert_eq!(
            terminated_curdir_len(&with_nul, with_nul.len()),
            Some(with_nul.len()),
            "Length may include the NUL"
        );
        let mut junk_after = path_without_nul;
        junk_after.extend_from_slice(&[b'X', 0]);
        assert_eq!(terminated_curdir_len(&junk_after, declared), None);
        assert_eq!(terminated_curdir_len(&terminated_after, declared + 1), None);
    }

    #[test]
    fn wow64_targets_use_32_bit_peb_offsets() {
        assert!(machine_is_wow64(
            windows_sys::Win32::System::SystemInformation::IMAGE_FILE_MACHINE_I386
        ));
        assert!(!machine_is_wow64(
            windows_sys::Win32::System::SystemInformation::IMAGE_FILE_MACHINE_AMD64
        ));

        let target32 = peb_layout_for_target(true);
        assert_eq!(target32, PEB_LAYOUT_32);
        assert_eq!(target32.process_parameters, 0x10);
        assert_eq!(target32.current_directory, 0x24);
        assert_eq!(target32.pointer_size, 4);
        if cfg!(target_pointer_width = "64") {
            assert_eq!(peb_layout_for_target(false), PEB_LAYOUT_64);
        } else {
            assert_eq!(peb_layout_for_target(false), PEB_LAYOUT_32);
        }
    }

    #[test]
    fn implausible_process_paths_are_rejected() {
        assert_eq!(plausible_process_path(""), None, "empty path");
        assert_eq!(plausible_process_path("\u{0}"), None);
        assert_eq!(
            plausible_process_path(r"\Device\HarddiskVolume1\Windows"),
            None
        );
        assert_eq!(plausible_process_path(r"C:relative"), None);
        assert_eq!(plausible_process_path(r"garbage"), None);
        assert_eq!(plausible_process_path("C:\\bad\ttab"), None);
        assert_eq!(
            plausible_process_path(r"\??\C:\Users\Simulp\folder"),
            Some(PathBuf::from(r"C:\Users\Simulp\folder"))
        );
        assert_eq!(
            plausible_process_path(r"\??\UNC\server\share"),
            Some(PathBuf::from(r"\\server\share"))
        );
        assert_eq!(
            plausible_process_path("E:\\Files\u{0}\u{0}"),
            Some(PathBuf::from(r"E:\Files"))
        );
    }

    #[test]
    fn transparency_requires_composition_and_the_user_setting() {
        assert!(!transparency_allowed(false, None));
        assert!(!transparency_allowed(false, Some(1)));
        assert!(
            transparency_allowed(true, None),
            "missing value defaults on"
        );
        assert!(transparency_allowed(true, Some(1)));
        assert!(!transparency_allowed(true, Some(0)));
    }

    #[test]
    fn acrylic_tint_alpha_follows_opacity() {
        let theme_bg = Rgb::new(0x20, 0x20, 0x30);

        assert_eq!(acrylic_tint(theme_bg, 0.0) >> 24, 255, "0% must be opaque");
        assert_eq!(acrylic_tint(theme_bg, 50.0) >> 24, 144, "50% is halfway");
        assert_eq!(
            acrylic_tint(theme_bg, 100.0) >> 24,
            32,
            "100% keeps the verified Acrylic strength"
        );
        assert_ne!(acrylic_tint(theme_bg, 0.0), acrylic_tint(theme_bg, 100.0));
        assert_eq!(
            acrylic_tint(theme_bg, f32::NAN) >> 24,
            32,
            "non-finite opacity is full strength"
        );
    }

    #[test]
    fn acrylic_tint_uses_theme_rgb() {
        let theme_bg = Rgb::new(0x12, 0x34, 0x56);

        assert_eq!(acrylic_tint(theme_bg, 0.0) & 0x00ff_ffff, 0x0056_3412);
        assert_eq!(acrylic_tint(theme_bg, 50.0) & 0x00ff_ffff, 0x0056_3412);
        assert_eq!(acrylic_tint(theme_bg, 100.0) & 0x00ff_ffff, 0x0056_3412);
    }

    #[test]
    fn move_accent_swaps_only_an_active_acrylic_accent() {
        let tint = 0x20_11_22_33;
        let mut accent = MoveAccent::default();

        assert_eq!(
            accent.move_action(true),
            None,
            "Mica/Solid have no tint to swap"
        );
        accent.set_tint(Some(tint));
        accent.pending_move = true;
        assert_eq!(
            accent.move_action(false),
            None,
            "an exit without an enter is a no-op"
        );
        assert_eq!(
            accent.move_action(true),
            Some((ACCENT_ENABLE_BLURBEHIND, tint)),
            "the move keeps the current tint but uses the cheap blur"
        );
        assert_eq!(accent_state_for_move(true), Some(ACCENT_ENABLE_BLURBEHIND));
        assert_eq!(accent_state_for_move(false), None);

        // The caller commits the transition only after the Win32 call succeeds.
        accent.moving = true;
        assert_eq!(
            accent.move_action(true),
            None,
            "a repeated enter is idempotent"
        );
        assert_eq!(
            accent.move_action(false),
            Some((ACCENT_ENABLE_ACRYLICBLURBEHIND, tint)),
            "exit restores acrylic with the stored tint"
        );
    }

    #[test]
    fn move_accent_ignores_a_pending_resize() {
        let tint = 0x20_11_22_33;
        let mut accent = MoveAccent {
            tint: Some(tint),
            moving: false,
            pending_move: false,
        };

        // `WM_ENTERSIZEMOVE` fires for border drags too; a resize must keep
        // acrylic because only a move benefits from the cheap blur.
        assert_eq!(accent.move_action(true), None);
        accent.pending_move = true;
        assert_eq!(
            accent.move_action(true),
            Some((ACCENT_ENABLE_BLURBEHIND, tint))
        );
    }

    #[test]
    fn move_accent_keeps_the_latest_tint_and_clears_with_the_material() {
        let mut accent = MoveAccent {
            tint: Some(0x20_11_22_33),
            moving: true,
            pending_move: false,
        };

        // An opacity change during the drag updates the tint but must stay in
        // the blur state so the move loop never pays acrylic recomputation.
        accent.set_tint(Some(0x80_44_55_66));
        assert!(accent.moving);
        assert_eq!(
            accent.move_action(false),
            Some((ACCENT_ENABLE_ACRYLICBLURBEHIND, 0x80_44_55_66))
        );
        assert_eq!(acrylic_state(true), ACCENT_ENABLE_BLURBEHIND);
        assert_eq!(acrylic_state(false), ACCENT_ENABLE_ACRYLICBLURBEHIND);

        // Switching to Mica/Solid clears both the tint and any in-flight move.
        accent.set_tint(None);
        assert_eq!(accent.tint, None);
        assert!(!accent.moving);
        assert_eq!(accent.move_action(false), None);
    }
}
