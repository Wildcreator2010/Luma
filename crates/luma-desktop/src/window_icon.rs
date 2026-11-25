//! 窗口图标收尾。
//!
//! Slint/winit 在 Windows 上只设置了 `ICON_SMALL`，任务栏按钮取的是
//! `ICON_BIG`，缺失时会退回进程默认图标（看起来就是"没有 logo"）。
//! 这里从 exe 资源里加载同一张 ico（见 `build.rs` 生成的资源脚本）并补齐两个尺寸。

use slint::ComponentHandle;

use crate::{AppWindow, LoginWindow};

pub fn apply(ui: &AppWindow) {
    #[cfg(windows)]
    unsafe {
        if let Some(hwnd) = hwnd_of(ui.window()) {
            set_icons(hwnd);
        }
    }
}

pub fn apply_login(login: &LoginWindow) {
    #[cfg(windows)]
    unsafe {
        if let Some(hwnd) = hwnd_of(login.window()) {
            set_icons(hwnd);
        }
    }
}

#[cfg(windows)]
fn hwnd_of(window: &slint::Window) -> Option<isize> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let handle = window.window_handle();
    let raw = handle.window_handle().ok()?;
    match raw.as_raw() {
        RawWindowHandle::Win32(win32) => Some(win32.hwnd.get()),
        _ => None,
    }
}

#[cfg(windows)]
#[allow(clippy::manual_dangling_ptr)]
unsafe fn set_icons(hwnd: isize) {
    let instance = win32::GetModuleHandleW(std::ptr::null());
    // (边长, WM_SETICON 的 wParam)
    for (size, which) in [(32, 1isize), (16, 0isize)] {
        let icon = win32::LoadImageW(
            instance,
            1 as *const u16,
            win32::IMAGE_ICON,
            size,
            size,
            win32::LR_DEFAULTCOLOR,
        );
        if icon != 0 {
            win32::SendMessageW(hwnd, win32::WM_SETICON, which, icon);
        }
    }
}

#[cfg(windows)]
mod win32 {
    #[link(name = "user32")]
    extern "system" {
        pub fn GetModuleHandleW(name: *const u16) -> isize;
        pub fn LoadImageW(
            instance: isize,
            name: *const u16,
            kind: u32,
            width: i32,
            height: i32,
            flags: u32,
        ) -> isize;
        pub fn SendMessageW(hwnd: isize, msg: u32, wparam: isize, lparam: isize) -> isize;
    }

    pub const IMAGE_ICON: u32 = 1;
    pub const LR_DEFAULTCOLOR: u32 = 0;
    pub const WM_SETICON: u32 = 0x0080;
}
