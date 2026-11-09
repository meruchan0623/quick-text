//! The beginner hint overlay (crate::hint): where the caret is, and showing a window without
//! taking the focus from the target.
use super::*;
use windows_sys::Win32::Graphics::Gdi::ClientToScreen;

/// The caret of the foreground window in screen pixels, else the centre of that window.
pub fn hint_anchor() -> Option<(i32, i32)> {
    unsafe {
        let window = GetForegroundWindow();
        if window.is_null() {
            return None;
        }
        let tid = GetWindowThreadProcessId(window, std::ptr::null_mut());
        let mut info: GUITHREADINFO = zeroed();
        info.cbSize = size_of::<GUITHREADINFO>() as u32;
        if GetGUIThreadInfo(tid, &mut info) != 0 && !info.hwndCaret.is_null() {
            let mut point = POINT {
                x: info.rcCaret.left,
                y: info.rcCaret.bottom,
            };
            if ClientToScreen(info.hwndCaret, &mut point) != 0 {
                return Some((point.x, point.y));
            }
        }
        let mut rect: RECT = zeroed();
        (GetWindowRect(window, &mut rect) != 0)
            .then(|| ((rect.left + rect.right) / 2, (rect.top + rect.bottom) / 2))
    }
}

/// Places `hwnd` topmost at the given screen pixels and shows it, never activating it: the
/// target keeps the focus and the keys.
pub fn show_without_activation(hwnd: isize, x: i32, y: i32, width: i32, height: i32) -> bool {
    let hwnd = hwnd as HWND;
    unsafe {
        let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let wanted = style | (WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TOPMOST) as isize;
        if wanted != style {
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, wanted);
        }
        SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            x,
            y,
            width,
            height,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        ) != 0
    }
}
