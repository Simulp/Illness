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
    DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE, DwmSetWindowAttribute,
};

/// Fallback attribute used by Windows 10 builds that predate the documented dark-mode id.
#[cfg(target_os = "windows")]
const DWMWA_USE_IMMERSIVE_DARK_MODE_OLD: u32 = 19;
/// `DWMWCP_ROUND`: round the window corners where the compositor supports it.
#[cfg(target_os = "windows")]
const DWMWCP_ROUND: i32 = 2;

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
