//! Windows things the interface toolkit does not do.

use std::path::Path;
use std::process::Command;

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, IsIconic, IsWindowVisible, MB_ICONERROR, MB_OK, MessageBoxW, SW_RESTORE,
    SetForegroundWindow, ShowWindow,
};
use windows::core::HSTRING;

fn hwnd_of(window: &slint::Window) -> Option<HWND> {
    match window.window_handle().window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(handle) => Some(HWND(handle.hwnd.get() as *mut _)),
        _ => None,
    }
}

/// Shows the window, restores it if it is minimized and brings it to the front. Returns whether
/// it ended up in the foreground; Windows may refuse that to a program that was not started by
/// the user's own action.
pub fn show_and_raise(window: &slint::Window) -> bool {
    let _ = window.show();
    let Some(hwnd) = hwnd_of(window) else {
        return false;
    };
    // SAFETY: plain Win32 calls on the handle of a window that exists.
    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        let _ = SetForegroundWindow(hwnd);
        IsWindowVisible(hwnd).as_bool() && GetForegroundWindow() == hwnd
    }
}

/// Opens a folder in Explorer.
pub fn open_folder(folder: &Path) {
    let _ = std::fs::create_dir_all(folder);
    let _ = Command::new("explorer.exe").arg(folder).spawn();
}

/// Tells the user that the program cannot run, because a program without a console has nowhere
/// else to say so.
pub fn show_error(message: &str) {
    // SAFETY: a modal message box without an owner.
    unsafe {
        MessageBoxW(
            None,
            &HSTRING::from(message),
            &HSTRING::from("cxtune"),
            MB_OK | MB_ICONERROR,
        );
    }
}
