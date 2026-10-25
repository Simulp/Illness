//! Win32 window material: DWM dark mode, rounded corners, Mica and Acrylic.
//!
//! All `unsafe` code in the workspace lives in this module. Each call documents why the
//! window handle is valid and why the raw pointer passed to Win32 is correctly sized.

#![allow(unsafe_code)]

use anyhow::{Context, Result};
use illness_core::{Material, Rgb};
use raw_window_handle::HasWindowHandle;
use std::ffi::OsStr;
use std::path::PathBuf;

#[cfg(target_os = "windows")]
use raw_window_handle::RawWindowHandle;
#[cfg(target_os = "windows")]
use std::sync::OnceLock;
#[cfg(target_os = "windows")]
use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, HWND};
#[cfg(target_os = "windows")]
use windows_sys::Win32::Graphics::Dwm::{
    DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE, DwmExtendFrameIntoClientArea,
    DwmSetWindowAttribute,
};
#[cfg(target_os = "windows")]
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};

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
/// `ACCENT_DISABLED`: clears any previously applied accent policy.
#[cfg(target_os = "windows")]
const ACCENT_DISABLED: u32 = 0;

/// Alpha at 100% background opacity. The legacy acrylic blur needs a
/// translucent tint to show through; a low alpha keeps the verified strength.
#[cfg(target_os = "windows")]
const ACRYLIC_TINT_ALPHA_OPAQUE: f32 = 32.0;

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
#[cfg(target_os = "windows")]
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

/// Register the per-user "Open with Illness" verbs for folders and drives.
///
/// The current executable is written every time, so running an installer or
/// switching between debug and release builds repairs a stale path. Existing
/// unrelated classes and verbs are never touched.
pub fn register_context_menu() -> Result<()> {
    #[cfg(not(target_os = "windows"))]
    {
        Ok(())
    }

    #[cfg(target_os = "windows")]
    {
        let executable =
            std::env::current_exe().context("could not resolve the Illness executable")?;
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

/// Remove only the Illness verbs created by [`register_context_menu`].
///
/// Each key is deleted recursively, but the `shell` parent keys are left in
/// place so verbs from other applications survive.
pub fn unregister_context_menu() -> Result<()> {
    #[cfg(not(target_os = "windows"))]
    {
        Ok(())
    }

    #[cfg(target_os = "windows")]
    {
        use windows_sys::Win32::System::Registry::{
            HKEY_CURRENT_USER, RegDeleteKeyW, RegDeleteTreeW,
        };

        for (path, _) in CONTEXT_MENU_VERBS {
            let key_path = registry_wide(OsStr::new(path));
            // Safety: key_path is null-terminated and live for the call.
            let tree_status = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, key_path.as_ptr()) };
            if tree_status != ERROR_SUCCESS && tree_status != ERROR_FILE_NOT_FOUND {
                anyhow::bail!("could not clear HKCU\\{path}: Windows error {tree_status}");
            }
            // RegDeleteTree removes values and subkeys; this removes the now
            // empty verb key itself. Safety: same null-terminated path.
            let key_status = unsafe { RegDeleteKeyW(HKEY_CURRENT_USER, key_path.as_ptr()) };
            if key_status != ERROR_SUCCESS && key_status != ERROR_FILE_NOT_FOUND {
                anyhow::bail!("could not remove HKCU\\{path}: Windows error {key_status}");
            }
        }
        log::info!("removed Explorer folder verbs");
        Ok(())
    }
}

/// `PEB.ProcessParameters` offset on the native pointer size.
#[cfg(target_os = "windows")]
const PEB_PROCESS_PARAMETERS_OFFSET: usize = if cfg!(target_pointer_width = "64") {
    0x20
} else {
    0x10
};
/// `RTL_USER_PROCESS_PARAMETERS.CurrentDirectory` offset.
#[cfg(target_os = "windows")]
const CURRENT_DIRECTORY_OFFSET: usize = if cfg!(target_pointer_width = "64") {
    0x38
} else {
    0x24
};
/// `UNICODE_STRING.Buffer` offset inside `CURDIR.DosPath`.
#[cfg(target_os = "windows")]
const CURRENT_DIRECTORY_BUFFER_OFFSET: usize = if cfg!(target_pointer_width = "64") {
    8
} else {
    4
};

/// Owned process handle used by [`process_cwd`].
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
fn read_process_u16(process: &ProcessHandle, address: usize) -> Option<u16> {
    let mut bytes = [0u8; std::mem::size_of::<u16>()];
    read_process_memory(process, address, &mut bytes)?;
    Some(u16::from_ne_bytes(bytes))
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

/// Read a process's current working directory from its PEB.
///
/// Returns `None` when the process is gone, belongs to another user/elevation
/// level, is a WOW64 target whose native PEB cannot be read, or exposes no
/// usable DOS path. Callers must keep a stored fallback cwd.
#[cfg(target_os = "windows")]
pub fn process_cwd(pid: u32) -> Option<PathBuf> {
    use windows_sys::Wdk::System::Threading::{NtQueryInformationProcess, ProcessBasicInformation};
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

    let peb = basic.PebBaseAddress as usize;
    if peb == 0 {
        return None;
    }
    let parameters = read_process_usize(&process, peb.checked_add(PEB_PROCESS_PARAMETERS_OFFSET)?)?;
    if parameters == 0 {
        return None;
    }
    let current_directory = parameters.checked_add(CURRENT_DIRECTORY_OFFSET)?;
    let length = read_process_u16(&process, current_directory)?;
    if length == 0 || length % 2 != 0 {
        return None;
    }
    let buffer = read_process_usize(
        &process,
        current_directory.checked_add(CURRENT_DIRECTORY_BUFFER_OFFSET)?,
    )?;
    if buffer == 0 {
        return None;
    }

    let mut text = vec![0u8; usize::from(length)];
    read_process_memory(&process, buffer, &mut text)?;
    let units: Vec<u16> = text
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .collect();
    let text = String::from_utf16_lossy(&units);
    let text = text.trim_end_matches('\0');
    if text.is_empty() {
        return None;
    }
    Some(strip_nt_prefix(text))
}

/// Process inspection is Windows-only; other platforms use the stored cwd.
#[cfg(not(target_os = "windows"))]
pub fn process_cwd(_pid: u32) -> Option<PathBuf> {
    None
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

        match material {
            Material::Mica => {
                // Mica is the wallpaper-derived system backdrop. Disable any
                // legacy accent first, then extend the frame so DWM paints
                // Mica through the whole client area, and apply it last.
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
                if !apply_accent(hwnd, ACCENT_ENABLE_ACRYLICBLURBEHIND, 0, tint) {
                    anyhow::bail!("legacy acrylic accent policy is unavailable");
                }
            }
            Material::Solid => {
                // Clearing an effect that was never applied is not an error.
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
        if !apply_accent(hwnd, ACCENT_ENABLE_ACRYLICBLURBEHIND, 0, tint) {
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
        CONTEXT_MENU_VERBS, acrylic_tint, context_menu_command, process_cwd, strip_nt_prefix,
        transparency_allowed,
    };
    use illness_core::Rgb;
    use std::ffi::OsStr;
    use std::path::PathBuf;

    #[test]
    fn peb_dos_paths_lose_object_manager_prefixes() {
        assert_eq!(
            strip_nt_prefix(r"\??\C:\Users\Simulp\folder"),
            PathBuf::from(r"C:\Users\Simulp\folder")
        );
        assert_eq!(strip_nt_prefix(r"\\?\E:\Files"), PathBuf::from(r"E:\Files"));
        assert_eq!(
            strip_nt_prefix(r"\??\UNC\server\share"),
            PathBuf::from(r"\\server\share")
        );
        assert_eq!(strip_nt_prefix(r"C:\plain"), PathBuf::from(r"C:\plain"));
    }

    #[test]
    fn process_cwd_reads_this_process() {
        let expected = std::env::current_dir().expect("test process cwd");
        let actual = process_cwd(std::process::id()).expect("own PEB is readable");
        assert_eq!(actual, expected);
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
}
