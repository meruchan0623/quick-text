//! Low-level keyboard/mouse hooks and the foreground focus WinEvent hook.
//! Hooks never perform file I/O or execute actions.
use super::*;
use super::{
    keystate::{physical, PHYSICAL},
    recording::{record_event, stop_record_events},
};
pub(super) static HOOK_READY: AtomicBool = AtomicBool::new(false);
pub(super) fn test_input_enabled() -> bool {
    acceptance_test_input_enabled()
}
pub(super) unsafe fn expected_key_text(event: &KBDLLHOOKSTRUCT) -> Option<String> {
    if event.vkCode == VK_PACKET as u32 {
        return char::from_u32(event.scanCode).map(|ch| ch.to_string());
    }
    let mut state = [0u8; 256];
    if physical(VK_SHIFT) {
        state[VK_SHIFT as usize] = 0x80;
    }
    if GetKeyState(VK_CAPITAL as i32) & 1 != 0 {
        state[VK_CAPITAL as usize] = 1;
    }
    let thread = GetWindowThreadProcessId(GetForegroundWindow(), std::ptr::null_mut());
    let mut units = [0u16; 8];
    // This acceptance adapter preserves dead-key state; hardware translation
    // uses the vendored detector and its own upstream flags.
    let len = ToUnicodeEx(
        event.vkCode,
        event.scanCode,
        state.as_ptr(),
        units.as_mut_ptr(),
        units.len() as i32,
        4,
        GetKeyboardLayout(thread),
    );
    (len > 0 && len as usize <= units.len())
        .then(|| String::from_utf16(&units[..len as usize]).ok())
        .flatten()
}
unsafe extern "system" fn keyboard(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
    if code < 0 {
        return CallNextHookEx(std::ptr::null_mut(), code, w, l);
    }
    let event = &*(l as *const KBDLLHOOKSTRUCT);
    if event.dwExtraInfo == MARK {
        return CallNextHookEx(std::ptr::null_mut(), code, w, l);
    }
    let Some(shared) = ENGINE.get() else {
        return CallNextHookEx(std::ptr::null_mut(), code, w, l);
    };
    // no-regression #4: external injected input (LLKHF_INJECTED without MARK) invalidates the buffer.
    if event.flags & LLKHF_INJECTED != 0 && !test_input_enabled() {
        shared.generation.fetch_add(1, Ordering::SeqCst);
        observer::CARET_EPOCH.fetch_add(1, Ordering::SeqCst);
        gate("External simulated input filtered; matching invalidated");
        return CallNextHookEx(std::ptr::null_mut(), code, w, l);
    }
    let down = w as u32 == WM_KEYDOWN || w as u32 == WM_SYSKEYDOWN;
    PHYSICAL.set(event.vkCode as u16, down);
    if down && event.vkCode == VK_ESCAPE as u32 && physical(VK_CONTROL) && physical(VK_SHIFT) {
        shared.cancel.store(true, Ordering::SeqCst);
        shared.generation.fetch_add(1, Ordering::SeqCst);
        observer::CARET_EPOCH.fetch_add(1, Ordering::SeqCst);
        stop_record_events(shared);
        return 1;
    }
    record_event(
        shared,
        if down {
            Operation::KeyDown {
                key: Key::WindowsCode {
                    code: event.vkCode as u16,
                },
            }
        } else {
            Operation::KeyUp {
                key: Key::WindowsCode {
                    code: event.vkCode as u16,
                },
            }
        },
    );
    if !down {
        return CallNextHookEx(std::ptr::null_mut(), code, w, l);
    }
    let generation = if event.flags & LLKHF_INJECTED != 0 || shared.busy.load(Ordering::SeqCst) {
        shared.generation.fetch_add(1, Ordering::SeqCst) + 1
    } else {
        shared.generation.load(Ordering::SeqCst)
    };
    // Only explicit acceptance mode adapts external SendInput into the dispatcher.
    if event.flags & LLKHF_INJECTED != 0 && test_input_enabled() {
        rawinput::acceptance_key(shared, event, generation);
    }
    gate(if shared.paused.load(Ordering::SeqCst) {
        "Paused"
    } else {
        "Espanso RawInput capture active"
    });

    CallNextHookEx(std::ptr::null_mut(), code, w, l)
}
/// The mouse button (as a virtual-key code) a low-level mouse message
/// presses or releases, if any.
fn mouse_button(msg: u32, data: u32) -> Option<(u16, bool)> {
    let x = if data >> 16 == 1 {
        VK_XBUTTON1
    } else {
        VK_XBUTTON2
    };
    match msg {
        WM_LBUTTONDOWN => Some((VK_LBUTTON, true)),
        WM_LBUTTONUP => Some((VK_LBUTTON, false)),
        WM_RBUTTONDOWN => Some((VK_RBUTTON, true)),
        WM_RBUTTONUP => Some((VK_RBUTTON, false)),
        WM_MBUTTONDOWN => Some((VK_MBUTTON, true)),
        WM_MBUTTONUP => Some((VK_MBUTTON, false)),
        WM_XBUTTONDOWN => Some((x, true)),
        WM_XBUTTONUP => Some((x, false)),
        _ => None,
    }
}
unsafe extern "system" fn mouse(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
    if code >= 0 {
        let ev = &*(l as *const MSLLHOOKSTRUCT);
        if ev.dwExtraInfo != MARK {
            if let Some(s) = ENGINE.get() {
                if let Some((button, down)) = mouse_button(w as u32, ev.mouseData) {
                    PHYSICAL.set(button, down);
                }
                // Pointer movement alone cannot move the caret; only clicks and
                // wheel reset the typed-key buffer (as in espanso).
                if w as u32 != WM_MOUSEMOVE {
                    s.generation.fetch_add(1, Ordering::SeqCst);
                    observer::CARET_EPOCH.fetch_add(1, Ordering::SeqCst);
                }
                {
                    let op = match w as u32 {
                        WM_MOUSEMOVE => Some(Operation::MouseMove {
                            x: ev.pt.x as f64,
                            y: ev.pt.y as f64,
                            relative: false,
                            percent: false,
                        }),
                        WM_LBUTTONDOWN => Some(Operation::MouseButton {
                            button: MouseButton::Left,
                            state: ButtonState::Down,
                        }),
                        WM_LBUTTONUP => Some(Operation::MouseButton {
                            button: MouseButton::Left,
                            state: ButtonState::Up,
                        }),
                        WM_RBUTTONDOWN => Some(Operation::MouseButton {
                            button: MouseButton::Right,
                            state: ButtonState::Down,
                        }),
                        WM_RBUTTONUP => Some(Operation::MouseButton {
                            button: MouseButton::Right,
                            state: ButtonState::Up,
                        }),
                        WM_MBUTTONDOWN => Some(Operation::MouseButton {
                            button: MouseButton::Middle,
                            state: ButtonState::Down,
                        }),
                        WM_MBUTTONUP => Some(Operation::MouseButton {
                            button: MouseButton::Middle,
                            state: ButtonState::Up,
                        }),
                        WM_XBUTTONDOWN => Some(Operation::MouseButton {
                            button: if ev.mouseData >> 16 == 1 {
                                MouseButton::X1
                            } else {
                                MouseButton::X2
                            },
                            state: ButtonState::Down,
                        }),
                        WM_XBUTTONUP => Some(Operation::MouseButton {
                            button: if ev.mouseData >> 16 == 1 {
                                MouseButton::X1
                            } else {
                                MouseButton::X2
                            },
                            state: ButtonState::Up,
                        }),
                        WM_MOUSEWHEEL => Some(Operation::Wheel {
                            axis: WheelAxis::Vertical,
                            amount: (ev.mouseData >> 16) as i16 as i32,
                            unit: WheelUnit::Delta,
                        }),
                        WM_MOUSEHWHEEL => Some(Operation::Wheel {
                            axis: WheelAxis::Horizontal,
                            amount: (ev.mouseData >> 16) as i16 as i32,
                            unit: WheelUnit::Delta,
                        }),
                        _ => None,
                    };
                    if let Some(op) = op {
                        let hovered = WindowFromPoint(ev.pt);
                        let mut pid = 0;
                        GetWindowThreadProcessId(hovered, &mut pid);
                        if pid != GetCurrentProcessId() {
                            record_event(s, op);
                        }
                    }
                }
            }
        }
    }
    CallNextHookEx(std::ptr::null_mut(), code, w, l)
}
unsafe extern "system" fn focus_event(
    _: windows_sys::Win32::UI::Accessibility::HWINEVENTHOOK,
    _: u32,
    window: HWND,
    _: i32,
    _: i32,
    _: u32,
    _: u32,
) {
    // WinEvent hooks are global. Background accessibility focus notifications
    // must not invalidate the active foreground transaction.
    let foreground = GetForegroundWindow();
    if window.is_null()
        || foreground.is_null()
        || GetAncestor(window, GA_ROOT) != GetAncestor(foreground, GA_ROOT)
    {
        return;
    }
    observer::FOCUS_EPOCH.fetch_add(1, Ordering::SeqCst);
    observer::CARET_EPOCH.fetch_add(1, Ordering::SeqCst);
}
pub(super) fn spawn_hooks() {
    std::thread::spawn(|| unsafe {
        let focus_hook = windows_sys::Win32::UI::Accessibility::SetWinEventHook(
            EVENT_OBJECT_FOCUS,
            EVENT_OBJECT_FOCUS,
            std::ptr::null_mut(),
            Some(focus_event),
            0,
            0,
            WINEVENT_OUTOFCONTEXT,
        );
        let keyboard_hook =
            SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard), std::ptr::null_mut(), 0);
        let mouse_hook = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse), std::ptr::null_mut(), 0);
        if keyboard_hook.is_null() || mouse_hook.is_null() || focus_hook.is_null() {
            if !keyboard_hook.is_null() {
                UnhookWindowsHookEx(keyboard_hook);
            }
            if !mouse_hook.is_null() {
                UnhookWindowsHookEx(mouse_hook);
            }
            if !focus_hook.is_null() {
                windows_sys::Win32::UI::Accessibility::UnhookWinEvent(focus_hook);
            }
            if let Some(s) = ENGINE.get() {
                s.paused.store(true, Ordering::SeqCst);
                *s.last.lock().unwrap() = Some(ExecutionResult {
                    id: "engine".into(),
                    status: "failed".into(),
                    message: "Windows keyboard/mouse hook installation failed; automatic input remains paused".into(),
                    completed_steps: 0,
                    side_effects: false,
                })
            }
            return;
        }
        HOOK_READY.store(true, Ordering::SeqCst);
        let mut msg: MSG = zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        UnhookWindowsHookEx(keyboard_hook);
        windows_sys::Win32::UI::Accessibility::UnhookWinEvent(focus_hook);
        if !mouse_hook.is_null() {
            UnhookWindowsHookEx(mouse_hook);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mouse_buttons_map_to_virtual_keys() {
        assert_eq!(mouse_button(WM_LBUTTONDOWN, 0), Some((VK_LBUTTON, true)));
        assert_eq!(mouse_button(WM_RBUTTONUP, 0), Some((VK_RBUTTON, false)));
        assert_eq!(mouse_button(WM_MBUTTONDOWN, 0), Some((VK_MBUTTON, true)));
        assert_eq!(
            mouse_button(WM_XBUTTONDOWN, 1 << 16),
            Some((VK_XBUTTON1, true))
        );
        assert_eq!(
            mouse_button(WM_XBUTTONUP, 2 << 16),
            Some((VK_XBUTTON2, false))
        );
        assert_eq!(mouse_button(WM_MOUSEMOVE, 0), None);
        assert_eq!(mouse_button(WM_MOUSEWHEEL, 120 << 16), None);
    }
}
