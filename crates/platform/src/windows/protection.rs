//! Focused-control vetoes: native edit styles and the UIA protection sampler.
use super::foreground::focus_handle;
use super::*;
use ::windows::Win32::{
    System::{Com::*, Variant::*},
    UI::Accessibility::*,
};
pub(super) fn password_control() -> bool {
    unsafe {
        let h = GetForegroundWindow();
        let tid = GetWindowThreadProcessId(h, std::ptr::null_mut());
        let mut info: GUITHREADINFO = zeroed();
        info.cbSize = size_of::<GUITHREADINFO>() as u32;
        if GetGUIThreadInfo(tid, &mut info) == 0 {
            return true;
        }
        let mut cls = [0u16; 64];
        GetClassNameW(info.hwndFocus, cls.as_mut_ptr(), 64);
        let class =
            String::from_utf16_lossy(&cls[..cls.iter().position(|x| *x == 0).unwrap_or(64)]);
        class.eq_ignore_ascii_case("edit")
            && (GetWindowLongW(info.hwndFocus, GWL_STYLE) & 0x20) != 0
    }
}
// no-regression #5: password, read-only and disabled native edits veto injection.
fn native_edit_protection(class: &str, style: i32) -> Option<&'static str> {
    let class = class.to_ascii_lowercase();
    if class != "edit" && !class.starts_with("richedit") {
        return None;
    }
    if style & 0x20 != 0 {
        Some("Password controls are blocked")
    } else if style & 0x800 != 0 {
        Some("Read-only text control blocked")
    } else if style & 0x08000000 != 0 {
        Some("Disabled controls are blocked")
    } else {
        None
    }
}
pub(super) fn native_input_protection() -> Option<String> {
    native_input_verdict().and_then(Result::err)
}
/// Only genuine native edit classes have styles that independently establish
/// password, enabled and editable state. Browser HWND styles prove nothing.
fn native_input_verdict() -> Option<Result<(), String>> {
    unsafe {
        let focus = focus_handle(GetForegroundWindow());
        let mut class = [0u16; 128];
        let len = GetClassNameW(focus, class.as_mut_ptr(), class.len() as i32);
        if len <= 0 {
            return None;
        }
        let class = String::from_utf16_lossy(&class[..len as usize]);
        if !native_edit_class(&class) {
            return None;
        }
        Some(
            match native_edit_protection(&class, GetWindowLongW(focus, GWL_STYLE)) {
                Some(reason) => Err(reason.to_owned()),
                None => Ok(()),
            },
        )
    }
}
fn native_edit_class(class: &str) -> bool {
    let class = class.to_ascii_lowercase();
    class == "edit" || class.starts_with("richedit")
}
static PROTECTION: Mutex<Option<(u64, u64, String)>> = Mutex::new(None);
pub(super) fn protection_block() -> Option<String> {
    let (focus, epoch, reason) = PROTECTION.try_lock().ok()?.clone()?;
    (focus == focus_handle(unsafe { GetForegroundWindow() }) as u64
        && epoch == observer::FOCUS_EPOCH.load(Ordering::SeqCst))
    .then_some(reason)
}
unsafe fn text_readonly(element: &IUIAutomationElement) -> Option<&'static str> {
    let pattern = element
        .GetCurrentPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId)
        .ok()?;
    let range = pattern.DocumentRange().ok()?;
    let mut value = range.GetAttributeValue(UIA_IsReadOnlyAttributeId).ok()?;
    let readonly = value.Anonymous.Anonymous.vt == VT_BOOL
        && value.Anonymous.Anonymous.Anonymous.boolVal.0 != 0;
    let _ = VariantClear(&mut value);
    readonly.then_some("Read-only text control blocked")
}
// no-regression #5: UIA password/disabled/read-only verdicts veto injection.
pub(super) fn protection_worker(shared: Arc<Shared>) {
    std::thread::spawn(move || unsafe {
        if CoInitializeEx(None, COINIT_MULTITHREADED).is_err() {
            return;
        }
        if let Ok(automation) =
            CoCreateInstance::<_, IUIAutomation>(&CUIAutomation, None, CLSCTX_INPROC_SERVER)
        {
            while !shared.shutdown.load(Ordering::SeqCst) {
                let focus = focus_handle(GetForegroundWindow()) as u64;
                let epoch = observer::FOCUS_EPOCH.load(Ordering::SeqCst);
                if let Ok(element) = automation.GetFocusedElement() {
                    let reason = if element.CurrentIsPassword().is_ok_and(|b| b.as_bool()) {
                        Some("Password controls are blocked")
                    } else if element.CurrentIsEnabled().is_ok_and(|b| !b.as_bool()) {
                        Some("Disabled controls are blocked")
                    } else if element
                        .GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
                        .ok()
                        .is_some_and(|p| p.CurrentIsReadOnly().is_ok_and(|b| b.as_bool()))
                    {
                        Some("Read-only ValuePattern control blocked")
                    } else if element
                        .GetCurrentPatternAs::<IUIAutomationLegacyIAccessiblePattern>(
                            UIA_LegacyIAccessiblePatternId,
                        )
                        .ok()
                        .is_some_and(|p| {
                            p.CurrentState()
                                .is_ok_and(|state| state & (0x40 | 0x20000000) != 0)
                        })
                    {
                        Some("Read-only or protected accessibility control blocked")
                    } else {
                        text_readonly(&element)
                    };
                    if focus == focus_handle(GetForegroundWindow()) as u64
                        && epoch == observer::FOCUS_EPOCH.load(Ordering::SeqCst)
                    {
                        *PROTECTION.lock().unwrap() =
                            reason.map(|reason| (focus, epoch, reason.to_owned()));
                    }
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        }
        CoUninitialize();
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_protection_covers_edit_and_richedit_without_misreading_browser_styles() {
        for class in ["Edit", "RICHEDIT50W", "RichEdit20A"] {
            assert_eq!(
                native_edit_protection(class, 0x20),
                Some("Password controls are blocked")
            );
            assert_eq!(
                native_edit_protection(class, 0x800),
                Some("Read-only text control blocked")
            );
            assert_eq!(
                native_edit_protection(class, 0x08000000),
                Some("Disabled controls are blocked")
            );
            assert_eq!(native_edit_protection(class, 0), None);
        }
        assert_eq!(
            native_edit_protection("Chrome_RenderWidgetHostHWND", 0x820),
            None
        );
    }
}
