//! Low-level keyboard/mouse hooks and the foreground focus WinEvent hook.
//!
//! B-02: the callbacks only read and update atomics, push one fixed-size
//! [`HookRecord`] onto a bounded lock-free queue and return. Macro recording,
//! gate text and acceptance translation run on the input consumer
//! (`input.rs`). A panic inside a callback is caught and counted, the event
//! still goes to `CallNextHookEx`, and the consumer logs `hook.panic`.
//! Hooks never perform file I/O or execute actions.
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use super::*;
use super::{
    foreground::WinForeground,
    input::{self, Lane},
    keystate::{KeyBits, PHYSICAL},
};
use crate::fault::{self, FaultPoint};
use crate::traits::{ForegroundProbe, InputContext};
use std::sync::PoisonError;

pub(super) static HOOK_READY: AtomicBool = AtomicBool::new(false);
/// Hook queue capacity in records. 待实测: sized for about a second of key
/// autorepeat plus clicks while the consumer is descheduled. A full queue
/// drops records; it never blocks the hook thread.
pub(super) const HOOK_QUEUE_CAPACITY: usize = 1024;
static PIPE: OnceLock<HookPipe<'static>> = OnceLock::new();

pub(super) fn test_input_enabled() -> bool {
    acceptance_test_input_enabled()
}

/// The fields the keyboard hook reads from `KBDLLHOOKSTRUCT`.
#[derive(Clone, Copy, Debug)]
pub(super) struct KeyInfo {
    pub(super) vk: u16,
    pub(super) scan: u32,
    pub(super) flags: u32,
    pub(super) extra: usize,
}
/// The fields the mouse hook reads from `MSLLHOOKSTRUCT`.
#[derive(Clone, Copy, Debug)]
pub(super) struct MouseInfo {
    pub(super) msg: u32,
    pub(super) data: u32,
    pub(super) x: i32,
    pub(super) y: i32,
    pub(super) flags: u32,
    pub(super) extra: usize,
}
/// Where a low-level input event came from (no-regression #4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Origin {
    /// Our own injection, tagged with [`MARK`] in `dwExtraInfo`. Never
    /// observed: no mirror update, no generation bump, no record.
    Own,
    /// Injected by someone else (the injected flag without our tag).
    External,
    /// Physical input.
    Hardware,
}
/// Classifies an event from its injected flag (`LLKHF_INJECTED` /
/// `LLMHF_INJECTED`) and `dwExtraInfo`. The tag alone identifies our own
/// input, as it always has.
pub(super) const fn classify(injected: bool, extra: usize) -> Origin {
    if extra == MARK {
        Origin::Own
    } else if injected {
        Origin::External
    } else {
        Origin::Hardware
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HookAction {
    /// Hand the event to `CallNextHookEx`.
    Pass,
    /// Return 1: the event is consumed (Ctrl+Shift+Esc cancel).
    Swallow,
}
/// Acceptance mode only: what the consumer needs to translate an injected
/// key, captured when the key happened.
#[derive(Clone, Copy, Debug)]
pub(super) struct Acceptance {
    pub(super) context: InputContext,
    pub(super) shift: bool,
    pub(super) modified: bool,
    pub(super) allowed: bool,
}
#[derive(Clone, Copy, Debug)]
pub(super) struct KeyRecord {
    pub(super) vk: u16,
    pub(super) scan: u32,
    pub(super) down: bool,
    pub(super) generation: u64,
    pub(super) acceptance: Option<Acceptance>,
}
#[derive(Clone, Copy, Debug)]
pub(super) enum HookRecord {
    Key(KeyRecord),
    /// An external injected key was filtered (no-regression #4).
    External,
    /// Ctrl+Shift+Esc cancelled the running action.
    Cancel,
    /// A click or wheel message.
    Mouse {
        msg: u32,
        data: u32,
        x: i32,
        y: i32,
    },
    /// A pointer move (macro recording only).
    Move {
        x: i32,
        y: i32,
    },
}

/// Hook side of the input pipeline: the record queue and the physical key
/// mirror the hooks update.
pub(super) struct HookPipe<'k> {
    lane: Lane<HookRecord>,
    keys: &'k KeyBits,
    /// Acceptance mode, fixed at startup (reading it may allocate).
    test_mode: bool,
}
impl<'k> HookPipe<'k> {
    pub(super) fn new(keys: &'k KeyBits, capacity: usize, test_mode: bool, wake: fn()) -> Self {
        Self {
            lane: Lane::new(capacity, wake),
            keys,
            test_mode,
        }
    }
    pub(super) fn lane(&self) -> &Lane<HookRecord> {
        &self.lane
    }
}
/// The production pipe over the global key mirror; created once, before the
/// hooks are installed.
pub(super) fn pipe(test_mode: bool) -> &'static HookPipe<'static> {
    PIPE.get_or_init(|| HookPipe::new(&PHYSICAL, HOOK_QUEUE_CAPACITY, test_mode, input::wake))
}

#[cold]
#[inline(never)]
#[allow(clippy::panic, reason = "fault injection point HookPanic (B-52)")]
fn injected_panic() -> ! {
    panic!("injected fault: hook_panic")
}

/// Keyboard hook body: atomics and one lock-free push, nothing else.
pub(super) fn on_key(
    pipe: &HookPipe,
    shared: &Shared,
    probe: &impl ForegroundProbe,
    msg: u32,
    key: KeyInfo,
) -> HookAction {
    let origin = classify(key.flags & LLKHF_INJECTED != 0, key.extra);
    if origin == Origin::Own {
        return HookAction::Pass;
    }
    if fault::take(FaultPoint::HookPanic).is_some() {
        injected_panic();
    }
    let injected = origin == Origin::External;
    // no-regression #4: external injected input (LLKHF_INJECTED without MARK) invalidates the buffer.
    if injected && !pipe.test_mode {
        shared.generation.fetch_add(1, Ordering::SeqCst);
        observer::CARET_EPOCH.fetch_add(1, Ordering::SeqCst);
        pipe.lane.push(HookRecord::External);
        return HookAction::Pass;
    }
    let down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
    let keys = pipe.keys;
    keys.set(key.vk, down);
    if down && key.vk == VK_ESCAPE && keys.held(VK_CONTROL) && keys.held(VK_SHIFT) {
        shared.cancel.store(true, Ordering::SeqCst);
        shared.generation.fetch_add(1, Ordering::SeqCst);
        observer::CARET_EPOCH.fetch_add(1, Ordering::SeqCst);
        pipe.lane.push(HookRecord::Cancel);
        return HookAction::Swallow;
    }
    let generation = if down && (injected || shared.busy.load(Ordering::SeqCst)) {
        shared
            .generation
            .fetch_add(1, Ordering::SeqCst)
            .wrapping_add(1)
    } else {
        shared.generation.load(Ordering::SeqCst)
    };
    // Only explicit acceptance mode adapts external SendInput into the matcher.
    let acceptance = if down && injected && pipe.test_mode {
        probe.input_context().map(|context| Acceptance {
            context,
            shift: keys.held(VK_SHIFT),
            modified: keys.modified(),
            allowed: !shared.paused.load(Ordering::SeqCst) && !shared.busy.load(Ordering::SeqCst),
        })
    } else {
        None
    };
    pipe.lane.push(HookRecord::Key(KeyRecord {
        vk: key.vk,
        scan: key.scan,
        down,
        generation,
        acceptance,
    }));
    HookAction::Pass
}
/// What the keyboard hook runs: [`on_key`] with panics contained.
pub(super) fn handle_key(
    pipe: &HookPipe,
    shared: &Shared,
    probe: &impl ForegroundProbe,
    msg: u32,
    key: KeyInfo,
) -> HookAction {
    pipe.lane
        .contained(HookAction::Pass, || on_key(pipe, shared, probe, msg, key))
}
unsafe extern "system" fn keyboard(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
    if code >= 0 {
        if let (Some(pipe), Some(shared)) = (PIPE.get(), ENGINE.get()) {
            // SAFETY: for HC_ACTION, lParam points to a KBDLLHOOKSTRUCT valid during this call.
            let event = &*(l as *const KBDLLHOOKSTRUCT);
            let key = KeyInfo {
                vk: event.vkCode as u16,
                scan: event.scanCode,
                flags: event.flags,
                extra: event.dwExtraInfo,
            };
            if handle_key(pipe, shared, &WinForeground, w as u32, key) == HookAction::Swallow {
                return 1;
            }
        }
    }
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
/// Mouse hook body: atomics and one lock-free push.
pub(super) fn on_mouse(pipe: &HookPipe, shared: &Shared, mouse: MouseInfo) {
    // External injected clicks still move the caret, so only our own input
    // is skipped.
    if classify(mouse.flags & LLMHF_INJECTED != 0, mouse.extra) == Origin::Own {
        return;
    }
    if let Some((button, down)) = mouse_button(mouse.msg, mouse.data) {
        pipe.keys.set(button, down);
    }
    if mouse.msg == WM_MOUSEMOVE {
        pipe.lane.push(HookRecord::Move {
            x: mouse.x,
            y: mouse.y,
        });
        return;
    }
    // Pointer movement alone cannot move the caret; only clicks and
    // wheel reset the typed-key buffer (as in espanso).
    shared.generation.fetch_add(1, Ordering::SeqCst);
    observer::CARET_EPOCH.fetch_add(1, Ordering::SeqCst);
    pipe.lane.push(HookRecord::Mouse {
        msg: mouse.msg,
        data: mouse.data,
        x: mouse.x,
        y: mouse.y,
    });
}
/// What the mouse hook runs: [`on_mouse`] with panics contained.
pub(super) fn handle_mouse(pipe: &HookPipe, shared: &Shared, mouse: MouseInfo) {
    pipe.lane.contained((), || on_mouse(pipe, shared, mouse));
}
unsafe extern "system" fn mouse(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
    if code >= 0 {
        if let (Some(pipe), Some(shared)) = (PIPE.get(), ENGINE.get()) {
            // SAFETY: for HC_ACTION, lParam points to an MSLLHOOKSTRUCT valid during this call.
            let event = &*(l as *const MSLLHOOKSTRUCT);
            let mouse = MouseInfo {
                msg: w as u32,
                data: event.mouseData,
                x: event.pt.x,
                y: event.pt.y,
                flags: event.flags,
                extra: event.dwExtraInfo,
            };
            handle_mouse(pipe, shared, mouse);
        }
    }
    CallNextHookEx(std::ptr::null_mut(), code, w, l)
}
/// Consumer side: the macro-recording operation for a click or wheel message.
pub(super) fn mouse_operation(msg: u32, data: u32) -> Option<Operation> {
    let x = if data >> 16 == 1 {
        MouseButton::X1
    } else {
        MouseButton::X2
    };
    let button = |button, state| Some(Operation::MouseButton { button, state });
    let wheel = |axis| {
        Some(Operation::Wheel {
            axis,
            amount: (data >> 16) as i16 as i32,
            unit: WheelUnit::Delta,
        })
    };
    match msg {
        WM_LBUTTONDOWN => button(MouseButton::Left, ButtonState::Down),
        WM_LBUTTONUP => button(MouseButton::Left, ButtonState::Up),
        WM_RBUTTONDOWN => button(MouseButton::Right, ButtonState::Down),
        WM_RBUTTONUP => button(MouseButton::Right, ButtonState::Up),
        WM_MBUTTONDOWN => button(MouseButton::Middle, ButtonState::Down),
        WM_MBUTTONUP => button(MouseButton::Middle, ButtonState::Up),
        WM_XBUTTONDOWN => button(x, ButtonState::Down),
        WM_XBUTTONUP => button(x, ButtonState::Up),
        WM_MOUSEWHEEL => wheel(WheelAxis::Vertical),
        WM_MOUSEHWHEEL => wheel(WheelAxis::Horizontal),
        _ => None,
    }
}
/// Consumer side: the macro-recording operation for a pointer position.
pub(super) fn pointer_move(x: i32, y: i32) -> Operation {
    Operation::MouseMove {
        x: f64::from(x),
        y: f64::from(y),
        relative: false,
        percent: false,
    }
}
/// Acceptance mode only, on the consumer: the text an injected key types in
/// `layout`. Preserves dead-key state (flag 4); hardware translation uses
/// the vendored detector and its own upstream flags.
pub(super) unsafe fn expected_key_text(
    vk: u16,
    scan: u32,
    shift: bool,
    layout: HKL,
) -> Option<String> {
    if vk == VK_PACKET {
        return char::from_u32(scan).map(|ch| ch.to_string());
    }
    let mut state = [0u8; 256];
    if shift {
        if let Some(slot) = state.get_mut(usize::from(VK_SHIFT)) {
            *slot = 0x80;
        }
    }
    if GetKeyState(i32::from(VK_CAPITAL)) & 1 != 0 {
        if let Some(slot) = state.get_mut(usize::from(VK_CAPITAL)) {
            *slot = 1;
        }
    }
    let mut units = [0u16; 8];
    let len = ToUnicodeEx(
        u32::from(vk),
        scan,
        state.as_ptr(),
        units.as_mut_ptr(),
        units.len() as i32,
        4,
        layout,
    );
    let len = usize::try_from(len).ok().filter(|len| *len > 0)?;
    String::from_utf16(units.get(..len)?).ok()
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
                *s.last.lock().unwrap_or_else(PoisonError::into_inner) = Some(ExecutionResult {
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
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
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

    #[test]
    fn every_click_and_wheel_message_has_a_recording_operation() {
        for msg in [
            WM_LBUTTONDOWN,
            WM_LBUTTONUP,
            WM_RBUTTONDOWN,
            WM_RBUTTONUP,
            WM_MBUTTONDOWN,
            WM_MBUTTONUP,
            WM_XBUTTONDOWN,
            WM_XBUTTONUP,
            WM_MOUSEWHEEL,
            WM_MOUSEHWHEEL,
        ] {
            assert!(mouse_operation(msg, 1 << 16).is_some(), "{msg:#x}");
        }
        assert!(mouse_operation(WM_MOUSEMOVE, 0).is_none());
        assert!(matches!(
            mouse_operation(WM_MOUSEWHEEL, (-120i16 as u16 as u32) << 16),
            Some(Operation::Wheel { amount: -120, .. })
        ));
    }
}
