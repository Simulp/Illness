//! Win32 window material: DWM dark mode, rounded corners, Mica and Acrylic.
//!
//! All `unsafe` code in the workspace lives in this module. Each call documents why the
//! window handle is valid and why the raw pointer passed to Win32 is correctly sized.

#![allow(unsafe_code)]

use anyhow::Result;
use illness_core::Material;
use raw_window_handle::HasWindowHandle;

#[cfg(target_os = "windows")]
use raw_window_handle::RawWindowHandle;
#[cfg(target_os = "windows")]
use windows_sys::Win32::Foundation::HWND;
#[cfg(target_os = "windows")]
use windows_sys::Win32::Graphics::Dwm::{
    DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE, DwmExtendFrameIntoClientArea,
    DwmSetWindowAttribute,
};

/// Fallback attribute used by Windows 10 builds that predate the documented dark-mode id.
#[cfg(target_os = "windows")]
const DWMWA_USE_IMMERSIVE_DARK_MODE_OLD: u32 = 19;
/// `DWMWCP_ROUND`: round the window corners where the compositor supports it.
#[cfg(target_os = "windows")]
const DWMWCP_ROUND: i32 = 2;

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

/// Apply the requested backdrop to `window`.
///
/// Mica failing on Windows 10 is expected; the caller falls back to [`Material::Solid`] and
/// reports a toast. On non-Windows platforms this is a no-op.
pub fn apply(window: &impl HasWindowHandle, material: Material, dark: bool) -> Result<()> {
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (window, material, dark);
        Ok(())
    }

    #[cfg(target_os = "windows")]
    {
        let handle = window.window_handle()?;
        let RawWindowHandle::Win32(win32) = handle.as_raw() else {
            anyhow::bail!("window does not expose a Win32 handle");
        };
        let hwnd = win32.hwnd.get() as HWND;
        apply_dwm(hwnd, dark);

        match material {
            Material::Mica => window_vibrancy::apply_mica(window, Some(dark))?,
            Material::Acrylic => {
                let tint = if dark {
                    (0x0c, 0x0c, 0x0f, 180)
                } else {
                    (0xf5, 0xf5, 0xf5, 180)
                };
                window_vibrancy::apply_acrylic(window, Some(tint))?;
            }
            Material::Solid => {
                // Clearing an effect that was never applied is not an error.
                let _ = window_vibrancy::clear_mica(window);
                let _ = window_vibrancy::clear_acrylic(window);
            }
        }

        Ok(())
    }
}

/// Apply DWM dark mode and rounded corners. Failures are ignored: older Windows versions
/// reject individual attributes without affecting the rest of the window.
#[cfg(target_os = "windows")]
fn apply_dwm(hwnd: HWND, dark: bool) {
    let value = i32::from(dark);
    let attribute = u32::try_from(DWMWA_USE_IMMERSIVE_DARK_MODE).unwrap_or(20);
    if !set_attribute(hwnd, attribute, value) {
        let _ = set_attribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE_OLD, value);
    }
    let corners = u32::try_from(DWMWA_WINDOW_CORNER_PREFERENCE).unwrap_or(33);
    let _ = set_attribute(hwnd, corners, DWMWCP_ROUND);

    // Let DWM composite the system backdrop through the whole client area.
    // Without this, DWMWA_SYSTEMBACKDROP_TYPE only paints under the frame.
    let margins = windows_sys::Win32::UI::Controls::MARGINS {
        cxLeftWidth: -1,
        cxRightWidth: -1,
        cyTopHeight: -1,
        cyBottomHeight: -1,
    };
    // Safety: hwnd is live for the duration of this call; `margins` is a
    // correctly laid out MARGINS owned by this stack frame.
    unsafe {
        let _ = DwmExtendFrameIntoClientArea(hwnd, &margins);
    }
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
