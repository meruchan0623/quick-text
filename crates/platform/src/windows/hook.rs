//! Low-level keyboard/mouse hooks and the foreground focus WinEvent hook.
//!
//! B-02: the callbacks only read and update atomics, push one fixed-size
//! [`HookRecord`] onto a bounded lock-free queue and return. Macro recording,
//! gate text and acceptance translation run on the input consumer
//! (`input.rs`). A panic inside a callback is caught and counted, the event
//! still goes to `CallNextHookEx`, and the consumer logs `hook.panic`.
//! Hooks never perform file I/O or execute actions.
//!
//! B-06: the hooks live on one thread that can be told to (re)install them
//! at any time ([`install`]); a hook Windows removed silently is replaced
//! without restarting anything. Whether a hook still works is decided by
//! the input facade's cross-check, not here.
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
    keystate::{own_input, KeyBits, PHYSICAL},
    recording::recording_session,
};
use crate::fault::{self, FaultPoint};
use crate::traits::{ForegroundProbe, InputContext};
use std::sync::atomic::{AtomicU32, AtomicU8};
use windows_sys::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};

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
    /// When the event happened: the struct's `time`, a millisecond tick
    /// count that wraps every 49.7 days.
    pub(super) time: u32,
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
    /// When the event happened, as in [`KeyInfo::time`].
    pub(super) time: u32,
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
    /// Physical input ([`Origin::Hardware`]); only these take part in the
    /// RawInput cross-check (B-06).
    pub(super) hardware: bool,
    pub(super) generation: u64,
    pub(super) acceptance: Option<Acceptance>,
    /// Capture time ([`KeyInfo::time`]); macro delays are measured between
    /// capture times, not when the consumer gets to a record (M1).
    pub(super) time: u32,
    /// The recording session current when the key happened
    /// ([`recording_session`]): only that recording takes it (M2).
    pub(super) session: u64,
}
#[derive(Clone, Copy, Debug)]
pub(super) enum HookRecord {
    Key(KeyRecord),
    /// An external injected key was filtered (no-regression #4).
    External,
    /// Ctrl+Shift+Esc cancelled the running action and stops a recording
    /// here, in queue order: the recording of `session`, the recording
    /// session current when the gesture was pressed, and no later one. When
    /// this record does not fit, the stop travels as
    /// [`HookPipe::take_stop`] instead (M2).
    Cancel {
        session: u64,
    },
    /// A click or wheel message, with its capture time and recording
    /// session (as for [`KeyRecord`]).
    Mouse {
        msg: u32,
        data: u32,
        x: i32,
        y: i32,
        time: u32,
        session: u64,
    },
    /// A pointer move (macro recording only), with its capture time and
    /// recording session.
    Move {
        x: i32,
        y: i32,
        time: u32,
        session: u64,
    },
}

/// Hook side of the input pipeline: the record queue and the physical key
/// mirror the hooks update.
pub(super) struct HookPipe<'k> {
    lane: Lane<HookRecord>,
    keys: &'k KeyBits,
    /// Acceptance mode, fixed at startup (reading it may allocate).
    test_mode: bool,
    /// A recording stop whose [`HookRecord::Cancel`] did not fit in the
    /// queue (M2): the recording session it was pressed in, 0 for none. It
    /// cannot be dropped; the consumer takes it before it accepts the next
    /// record.
    stop: AtomicU64,
}
impl<'k> HookPipe<'k> {
    pub(super) fn new(keys: &'k KeyBits, capacity: usize, test_mode: bool, wake: fn()) -> Self {
        Self {
            lane: Lane::new(capacity, wake),
            keys,
            test_mode,
            stop: AtomicU64::new(0),
        }
    }
    pub(super) fn lane(&self) -> &Lane<HookRecord> {
        &self.lane
    }
    /// Consumer side: the recording session of a stop gesture that
    /// overflowed the queue since the last call, or 0.
    pub(super) fn take_stop(&self) -> u64 {
        self.stop.swap(0, Ordering::SeqCst)
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
    // M2: the event belongs to the recording session current when this
    // callback started. Read once, before anything is handled, and every
    // record of this event (key, stop gesture) carries it: if the app ends
    // that recording and starts another while the callback runs, the
    // consumer drops the event as stale instead of giving it to the new one.
    let session = recording_session();
    if fault::take(FaultPoint::HookPanic).is_some() {
        injected_panic();
    }
    let injected = origin == Origin::External;
    let down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
    if !down {
        // H1: a key-up that is not ours ends our hold of the key, whoever
        // pressed it (the key state is shared).
        own_input().released_elsewhere(key.vk);
    }
    // no-regression #4: external injected input (LLKHF_INJECTED without MARK) invalidates the buffer.
    if injected && !pipe.test_mode {
        shared.generation.fetch_add(1, Ordering::SeqCst);
        observer::CARET_EPOCH.fetch_add(1, Ordering::SeqCst);
        pipe.lane.push(HookRecord::External);
        return HookAction::Pass;
    }
    let keys = pipe.keys;
    keys.set(key.vk, down);
    if down && key.vk == VK_ESCAPE && keys.held(VK_CONTROL) && keys.held(VK_SHIFT) {
        shared.cancel.store(true, Ordering::SeqCst);
        shared.generation.fetch_add(1, Ordering::SeqCst);
        observer::CARET_EPOCH.fetch_add(1, Ordering::SeqCst);
        // M2: the stop belongs to the session read at callback start; the
        // consumer drops it if another recording has started since.
        if !pipe.lane.push(HookRecord::Cancel { session }) {
            // A full queue must not swallow the stop. Set before any later
            // record is pushed, so the consumer sees it no later than the
            // first record that follows the gesture.
            pipe.stop.store(session, Ordering::SeqCst);
            pipe.lane.wake();
        }
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
        hardware: origin == Origin::Hardware,
        generation,
        acceptance,
        time: key.time,
        session,
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
                time: event.time,
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
    // M2: as for keys, the event belongs to the recording session current
    // when this callback started, read before anything is handled.
    let session = recording_session();
    if let Some((button, down)) = mouse_button(mouse.msg, mouse.data) {
        if !down {
            // H1: as for keys, a button-up not ours ends our hold.
            own_input().released_elsewhere(button);
        }
        pipe.keys.set(button, down);
    }
    if mouse.msg == WM_MOUSEMOVE {
        pipe.lane.push(HookRecord::Move {
            x: mouse.x,
            y: mouse.y,
            time: mouse.time,
            session,
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
        time: mouse.time,
        session,
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
                time: event.time,
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
    _: HWINEVENTHOOK,
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
/// A set of hooks (bit set): what to install, or what is installed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct HookSet(u8);
impl HookSet {
    pub(super) const NONE: Self = Self(0);
    pub(super) const KEYBOARD: Self = Self(1);
    pub(super) const MOUSE: Self = Self(2);
    /// The foreground focus WinEvent hook.
    pub(super) const FOCUS: Self = Self(4);
    /// The two low-level hooks, which Windows can remove silently.
    pub(super) const LOW_LEVEL: Self = Self(1 | 2);
    pub(super) const ALL: Self = Self(1 | 2 | 4);
    pub(super) const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
    #[must_use]
    pub(super) const fn and(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }
    const fn from_bits(bits: usize) -> Self {
        Self((bits & 7) as u8)
    }
}

/// The hook thread's id once its message queue exists,
/// [`HOOK_THREAD_STARTING`] while it is being created, 0 when not running.
static HOOK_THREAD: AtomicU32 = AtomicU32::new(0);
const HOOK_THREAD_STARTING: u32 = u32::MAX;
/// Install requests issued, and the last one the hook thread answered
/// (request sequence numbers; a late answer to a request that timed out
/// never satisfies a newer one).
static REQUESTS: AtomicU64 = AtomicU64::new(0);
static ANSWERED: AtomicU64 = AtomicU64::new(0);
/// The hooks the last answered request left installed ([`HookSet`] bits).
/// A hook Windows removed silently still counts until it is replaced.
static INSTALLED: AtomicU8 = AtomicU8::new(0);
/// Hook-thread message: (re)install the hooks in `wParam` ([`HookSet`]
/// bits) for request number `lParam`.
const WM_HOOK_INSTALL: u32 = WM_APP + 0x51;
/// Hook-thread message, fault injection only (HookUnhook): remove the
/// low-level hooks without updating [`INSTALLED`], as Windows does after a
/// callback timeout. Ignored in builds without `fault-injection`.
const WM_HOOK_DROP: u32 = WM_APP + 0x53;
/// How long [`install`] waits for the hook thread.
const INSTALL_TIMEOUT: Duration = Duration::from_secs(2);
const INSTALL_POLL: Duration = Duration::from_millis(2);

/// The hooks the hook thread owns.
struct Hooks {
    keyboard: HHOOK,
    mouse: HHOOK,
    focus: HWINEVENTHOOK,
    installed: HookSet,
}
/// Installs a fresh low-level hook and only then removes the one it
/// replaces. Both happen inside one hook-thread message, so none of our
/// callbacks runs in between (待实测: an event already in flight to the old
/// hook). A failed install keeps the old handle.
unsafe fn replace_hook(old: &mut HHOOK, id: WINDOWS_HOOK_ID, proc: HOOKPROC) -> bool {
    let new = SetWindowsHookExW(id, proc, std::ptr::null_mut(), 0);
    if new.is_null() {
        return false;
    }
    if !old.is_null() {
        // Fails harmlessly for a hook Windows already removed.
        UnhookWindowsHookEx(*old);
    }
    *old = new;
    true
}
impl Hooks {
    const fn new() -> Self {
        Self {
            keyboard: std::ptr::null_mut(),
            mouse: std::ptr::null_mut(),
            focus: std::ptr::null_mut(),
            installed: HookSet::NONE,
        }
    }
    fn mark(&mut self, kind: HookSet, ok: bool) {
        self.installed = HookSet(if ok {
            self.installed.0 | kind.0
        } else {
            self.installed.0 & !kind.0
        });
        INSTALLED.store(self.installed.0, Ordering::SeqCst);
    }
    unsafe fn install(&mut self, which: HookSet) {
        if which.contains(HookSet::KEYBOARD) {
            let ok = replace_hook(&mut self.keyboard, WH_KEYBOARD_LL, Some(keyboard));
            self.mark(HookSet::KEYBOARD, ok);
        }
        if which.contains(HookSet::MOUSE) {
            let ok = replace_hook(&mut self.mouse, WH_MOUSE_LL, Some(mouse));
            self.mark(HookSet::MOUSE, ok);
        }
        if which.contains(HookSet::FOCUS) {
            let new = SetWinEventHook(
                EVENT_OBJECT_FOCUS,
                EVENT_OBJECT_FOCUS,
                std::ptr::null_mut(),
                Some(focus_event),
                0,
                0,
                WINEVENT_OUTOFCONTEXT,
            );
            if !new.is_null() && !self.focus.is_null() {
                UnhookWinEvent(self.focus);
            }
            if !new.is_null() {
                self.focus = new;
            }
            self.mark(HookSet::FOCUS, !new.is_null());
        }
    }
    unsafe fn drop_low_level(&mut self) {
        for hook in [&mut self.keyboard, &mut self.mouse] {
            if !hook.is_null() {
                UnhookWindowsHookEx(*hook);
                *hook = std::ptr::null_mut();
            }
        }
    }
    unsafe fn remove_all(&mut self) {
        self.drop_low_level();
        if !self.focus.is_null() {
            UnhookWinEvent(self.focus);
            self.focus = std::ptr::null_mut();
        }
        self.mark(HookSet::ALL, false);
    }
}

/// The hook thread: installs `initial` for request `seq`, then pumps
/// messages (which is when Windows calls the hooks) and answers install
/// requests.
fn run_hooks(initial: HookSet, seq: u64) {
    // SAFETY: the hooks belong to this thread, which pumps their messages
    // until it exits and removes them.
    unsafe {
        let mut msg: MSG = zeroed();
        // Creates the message queue before the id is published, so the
        // first posted request is not lost.
        PeekMessageW(
            &mut msg,
            std::ptr::null_mut(),
            WM_USER,
            WM_USER,
            PM_NOREMOVE,
        );
        HOOK_THREAD.store(GetCurrentThreadId(), Ordering::SeqCst);
        let mut hooks = Hooks::new();
        hooks.install(initial);
        ANSWERED.store(seq, Ordering::SeqCst);
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            match (msg.hwnd.is_null(), msg.message) {
                (true, WM_HOOK_INSTALL) => {
                    hooks.install(HookSet::from_bits(msg.wParam));
                    ANSWERED.store(msg.lParam as u64, Ordering::SeqCst);
                }
                // Another process can post thread messages too; only fault
                // builds honour the drop request.
                (true, WM_HOOK_DROP) if cfg!(feature = "fault-injection") => {
                    hooks.drop_low_level();
                }
                _ => {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
        }
        hooks.remove_all();
        HOOK_THREAD.store(0, Ordering::SeqCst);
    }
}

enum Request {
    Sent,
    /// The hook thread is starting; try again shortly.
    Wait,
    Failed,
}
/// Hands request `seq` for `which` to the hook thread, starting it when it
/// is not running.
fn request(which: HookSet, seq: u64) -> Request {
    match HOOK_THREAD.load(Ordering::SeqCst) {
        0 => {
            if HOOK_THREAD
                .compare_exchange(0, HOOK_THREAD_STARTING, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
            {
                return Request::Wait;
            }
            let spawned = std::thread::Builder::new()
                .name("quicker-hooks".into())
                .spawn(move || run_hooks(which, seq));
            if spawned.is_err() {
                HOOK_THREAD.store(0, Ordering::SeqCst);
                return Request::Failed;
            }
            Request::Sent
        }
        HOOK_THREAD_STARTING => Request::Wait,
        thread => {
            // SAFETY: posting to a thread that has exited fails harmlessly.
            let posted = unsafe {
                PostThreadMessageW(thread, WM_HOOK_INSTALL, usize::from(which.0), seq as isize) != 0
            };
            if posted {
                Request::Sent
            } else {
                // The thread is gone; the next round starts a new one.
                let _ = HOOK_THREAD.compare_exchange(thread, 0, Ordering::SeqCst, Ordering::SeqCst);
                Request::Wait
            }
        }
    }
}
/// Installs (or re-installs) the hooks in `which` on the hook thread,
/// starting the thread if needed, and waits up to [`INSTALL_TIMEOUT`] for
/// the answer. Returns the requested hooks that are installed afterwards.
/// Callers serialise (the input facade's work lock); never call this from
/// a hook callback.
pub(super) fn install(which: HookSet) -> HookSet {
    let seq = REQUESTS.fetch_add(1, Ordering::SeqCst).wrapping_add(1);
    let deadline = Instant::now() + INSTALL_TIMEOUT;
    let mut sent = false;
    while Instant::now() < deadline {
        if !sent {
            match request(which, seq) {
                Request::Sent => sent = true,
                Request::Wait => {}
                Request::Failed => {
                    tracing::error!(event = "hook.thread_failed");
                    return HookSet::NONE;
                }
            }
        }
        if sent && ANSWERED.load(Ordering::SeqCst) == seq {
            return HookSet::from_bits(usize::from(INSTALLED.load(Ordering::SeqCst))).and(which);
        }
        std::thread::sleep(INSTALL_POLL);
    }
    tracing::warn!(event = "hook.install_timeout");
    HookSet::NONE
}
/// Fault injection (HookUnhook): removes the low-level hooks the way
/// Windows does after a callback timeout, without telling anyone.
pub(super) fn drop_hooks() {
    let thread = HOOK_THREAD.load(Ordering::SeqCst);
    if thread != 0 && thread != HOOK_THREAD_STARTING {
        // SAFETY: posting to a thread that has exited fails harmlessly.
        unsafe {
            PostThreadMessageW(thread, WM_HOOK_DROP, 0, 0);
        }
    }
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
