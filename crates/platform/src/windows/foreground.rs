//! Foreground window identity and cursor geometry.
use super::protection::{native_input_protection, protection_block};
use super::*;
use crate::traits::ForegroundProbe;
pub fn cursor() -> serde_json::Value {
    unsafe {
        let mut p: POINT = zeroed();
        GetCursorPos(&mut p);
        serde_json::json!({"x":p.x,"y":p.y,"width":GetSystemMetrics(SM_CXVIRTUALSCREEN),"height":GetSystemMetrics(SM_CYVIRTUALSCREEN),"left":GetSystemMetrics(SM_XVIRTUALSCREEN),"top":GetSystemMetrics(SM_YVIRTUALSCREEN)})
    }
}
pub fn foreground() -> Foreground {
    unsafe {
        let h = GetForegroundWindow();
        let mut title = [0u16; 512];
        GetWindowTextW(h, title.as_mut_ptr(), 512);
        let mut pid = 0;
        GetWindowThreadProcessId(h, &mut pid);
        let p = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        let mut path = [0u16; 32768];
        let mut count = path.len() as u32;
        let process = if !p.is_null()
            && QueryFullProcessImageNameW(p, 0, path.as_mut_ptr(), &mut count) != 0
        {
            std::path::Path::new(&String::from_utf16_lossy(&path[..count as usize]))
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        } else {
            String::new()
        };
        if !p.is_null() {
            CloseHandle(p);
        }
        Foreground {
            id: focus_handle(h) as u64,
            platform: "windows".into(),
            bundle_id: String::new(),
            process,
            title: String::from_utf16_lossy(
                &title[..title.iter().position(|x| *x == 0).unwrap_or(512)],
            ),
        }
    }
}
pub(super) fn focus_handle(window: HWND) -> HWND {
    unsafe {
        let tid = GetWindowThreadProcessId(window, std::ptr::null_mut());
        let mut info: GUITHREADINFO = zeroed();
        info.cbSize = size_of::<GUITHREADINFO>() as u32;
        if GetGUIThreadInfo(tid, &mut info) != 0 && !info.hwndFocus.is_null() {
            info.hwndFocus
        } else {
            window
        }
    }
}
/// Production [`ForegroundProbe`]: the focused window plus the native-edit
/// and cached UIA protection verdicts.
pub(super) struct WinForeground;
impl ForegroundProbe for WinForeground {
    fn foreground(&self) -> Foreground {
        foreground()
    }
    fn protection(&self) -> Option<String> {
        native_input_protection().or_else(protection_block)
    }
}
