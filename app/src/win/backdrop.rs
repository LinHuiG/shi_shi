//! Native Mica composition. The page keeps a dark tint even if the system
//! underlay changes, preventing light pixels behind light text.
use windows_sys::Win32::{
    Graphics::Dwm::{
        DwmExtendFrameIntoClientArea, DwmSetWindowAttribute, DWMSBT_MAINWINDOW, DWMSBT_NONE, DWMWA_SYSTEMBACKDROP_TYPE,
        DWMWA_USE_IMMERSIVE_DARK_MODE,
    },
    UI::Controls::MARGINS,
};

/// Ask DWM for a dark Mica backdrop. Windows before 11 22H2 (build 22621)
/// rejects the attribute; the caller then keeps the opaque page. DWM itself
/// falls back to a solid colour when transparency effects are off, on battery
/// saver or while the window is inactive.
pub fn apply_mica(hwnd: isize) -> Result<(), String> {
    // Dark non-client area so the Mica tint matches the dark page.
    let result = set(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, 1)
        .and_then(|_| refresh(hwnd))
        .and_then(|_| set(hwnd, DWMWA_SYSTEMBACKDROP_TYPE, DWMSBT_MAINWINDOW));
    if result.is_err() {
        clear(hwnd);
    }
    result
}

fn set(hwnd: isize, attribute: i32, value: i32) -> Result<(), String> {
    let size = std::mem::size_of::<i32>() as u32;
    checked(unsafe { DwmSetWindowAttribute(hwnd as _, attribute as u32, (&value as *const i32).cast(), size) })
}

fn checked(result: i32) -> Result<(), String> {
    if result < 0 {
        Err(format!("背景合成失败（HRESULT 0x{result:08x}）"))
    } else {
        Ok(())
    }
}

fn frame(hwnd: isize, margin: i32) -> Result<(), String> {
    let margins = MARGINS { cxLeftWidth: margin, cxRightWidth: margin, cyTopHeight: margin, cyBottomHeight: margin };
    checked(unsafe { DwmExtendFrameIntoClientArea(hwnd as _, &margins) })
}

/// Reapply after activation/resize; the host must paint black for DWM alpha.
pub fn refresh(hwnd: isize) -> Result<(), String> {
    frame(hwnd, -1)
}

pub fn clear(hwnd: isize) {
    let _ = set(hwnd, DWMWA_SYSTEMBACKDROP_TYPE, DWMSBT_NONE);
    let _ = frame(hwnd, 0);
}

#[cfg(test)]
mod tests;
