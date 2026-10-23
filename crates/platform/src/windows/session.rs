//! Session window (B-05).
//!
//! A hidden top-level window (broadcasts such as `WM_POWERBROADCAST` never
//! reach message-only windows) receives session changes (lock, unlock,
//! console and remote connects) and power resume; a WinEvent hook reports
//! desktop switches (UAC and the secure desktop). Each of them can swallow
//! key-ups, so they ask the input consumer to resynchronise the key mirror.
//!
//! The window procedure only sets a bit in [`SessionSignals`] and wakes the
//! consumer; the resync itself (which reads the asynchronous key state)
//! runs on the consumer. Tests use [`signal_for`] and local
//! [`SessionSignals`]; nothing in this module runs a window in tests.
//!
//! The same window answers the shutdown messages: `WM_QUERYENDSESSION` is
//! answered TRUE at once (never vetoes or delays a shutdown), and
//! `WM_ENDSESSION` for a session that really ends runs the callbacks
//! registered with [`on_session_end`] once ([`SessionEnd`]).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use super::*;
use crate::fault::{self, FaultPoint};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::AtomicU32;
use std::sync::PoisonError;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::RemoteDesktop::{
    WTSRegisterSessionNotification, WTSUnRegisterSessionNotification, NOTIFY_FOR_THIS_SESSION,
};
use windows_sys::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};

/// Why the key mirror needs a resync.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Signal {
    Lock,
    Unlock,
    /// A console or remote connection attached to this session.
    Connect,
    Logon,
    RemoteControl,
    /// Resume from sleep or hibernation.
    Resume,
    /// The input desktop changed (UAC prompt, Ctrl+Alt+Del screen).
    DesktopSwitch,
    /// A lost hook or RawInput source was reattached (B-06); key-ups may
    /// have gone unseen while it was missing. Raised by the input facade.
    InputReattached,
}
impl Signal {
    const ALL: [Signal; 8] = [
        Signal::Lock,
        Signal::Unlock,
        Signal::Connect,
        Signal::Logon,
        Signal::RemoteControl,
        Signal::Resume,
        Signal::DesktopSwitch,
        Signal::InputReattached,
    ];
    const fn bit(self) -> u32 {
        1 << self as u32
    }
    pub(super) const fn name(self) -> &'static str {
        match self {
            Signal::Lock => "lock",
            Signal::Unlock => "unlock",
            Signal::Connect => "connect",
            Signal::Logon => "logon",
            Signal::RemoteControl => "remote_control",
            Signal::Resume => "resume",
            Signal::DesktopSwitch => "desktop_switch",
            Signal::InputReattached => "input_reattached",
        }
    }
}

/// The resync signal a session-window message carries, if any.
/// Disconnects and logoffs are ignored: the connect or unlock that follows
/// them resyncs.
pub(super) fn signal_for(msg: u32, wparam: usize) -> Option<Signal> {
    let code = u32::try_from(wparam).ok()?;
    match msg {
        WM_WTSSESSION_CHANGE => match code {
            WTS_SESSION_LOCK => Some(Signal::Lock),
            WTS_SESSION_UNLOCK => Some(Signal::Unlock),
            WTS_CONSOLE_CONNECT | WTS_REMOTE_CONNECT => Some(Signal::Connect),
            WTS_SESSION_LOGON => Some(Signal::Logon),
            WTS_SESSION_REMOTE_CONTROL => Some(Signal::RemoteControl),
            _ => None,
        },
        WM_POWERBROADCAST => {
            matches!(code, PBT_APMRESUMEAUTOMATIC | PBT_APMRESUMESUSPEND).then_some(Signal::Resume)
        }
        _ => None,
    }
}

/// Signals raised by the session window and not yet handled by the
/// consumer. Lock-free; raising never blocks the window procedure.
pub(super) struct SessionSignals(AtomicU32);
impl SessionSignals {
    pub(super) const fn new() -> Self {
        Self(AtomicU32::new(0))
    }
    pub(super) fn raise(&self, signal: Signal) {
        self.0.fetch_or(signal.bit(), Ordering::SeqCst);
    }
    /// Whether any signal is waiting, without taking it.
    pub(super) fn pending(&self) -> bool {
        self.0.load(Ordering::SeqCst) != 0
    }
    /// Takes every pending signal (each at most once).
    pub(super) fn take(&self) -> impl Iterator<Item = Signal> {
        let pending = self.0.swap(0, Ordering::SeqCst);
        Signal::ALL
            .into_iter()
            .filter(move |signal| pending & signal.bit() != 0)
    }
}
pub(super) static SIGNALS: SessionSignals = SessionSignals::new();

fn raise(signal: Signal) {
    SIGNALS.raise(signal);
    super::input::wake();
}

/// A callback run when the Windows session ends (shutdown, restart or
/// logoff).
pub(super) type SessionEndCallback = Box<dyn Fn() + Send + Sync>;

/// Callbacks for the end of the session (B-04), run at most once per
/// process: Windows may end the process at any time after `WM_ENDSESSION`
/// returns, so nothing after the first end is ever needed.
pub(super) struct SessionEnd {
    callbacks: Mutex<Vec<SessionEndCallback>>,
    fired: AtomicBool,
}
impl SessionEnd {
    pub(super) const fn new() -> Self {
        Self {
            callbacks: Mutex::new(Vec::new()),
            fired: AtomicBool::new(false),
        }
    }
    pub(super) fn register(&self, callback: SessionEndCallback) {
        self.callbacks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(callback);
    }
    /// Runs every registered callback once, in registration order, on the
    /// calling thread. A panicking callback is logged and does not stop
    /// the others. Returns false when the session already ended.
    pub(super) fn fire(&self, source: &'static str) -> bool {
        if self.fired.swap(true, Ordering::SeqCst) {
            return false;
        }
        // Taken out of the lock so a callback that registers another one
        // cannot deadlock.
        let callbacks = std::mem::take(
            &mut *self
                .callbacks
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        tracing::info!(event = "session.end", source, callbacks = callbacks.len());
        for (index, callback) in callbacks.iter().enumerate() {
            if catch_unwind(AssertUnwindSafe(callback)).is_err() {
                tracing::error!(event = "session.end_callback_panic", index);
            }
        }
        true
    }
}
pub(super) static SESSION_END: SessionEnd = SessionEnd::new();

/// Registers a callback for the end of the Windows session (shutdown,
/// restart or logoff). Callbacks run once, in registration order, on the
/// session window thread while it handles `WM_ENDSESSION`, and Windows may
/// end the process as soon as they return: keep them short and limited to
/// the I/O that must happen before the process dies (no waiting on other
/// threads, no UI). A callback registered after the session ended never
/// runs.
pub fn on_session_end(callback: Box<dyn Fn() + Send + Sync>) {
    SESSION_END.register(callback);
}

/// The answer to a shutdown message, or `None` for any other message.
/// `WM_QUERYENDSESSION` is answered TRUE at once: the shutdown is never
/// vetoed or delayed. `WM_ENDSESSION` runs the callbacks when the session
/// really ends (wParam TRUE), not when another application cancelled it.
pub(super) fn end_message(
    end: &SessionEnd,
    msg: u32,
    wparam: WPARAM,
    source: &'static str,
) -> Option<LRESULT> {
    match msg {
        WM_QUERYENDSESSION => Some(1),
        WM_ENDSESSION => {
            if wparam != 0 {
                end.fire(source);
            }
            Some(0)
        }
        _ => None,
    }
}

/// The `session_end` fault (B-04): replays the shutdown messages through
/// [`end_message`] as if Windows had sent them. Like a real shutdown it
/// latches [`SessionEnd`]; the scenario ends the process right after.
pub(super) fn inject_fault(end: &SessionEnd) {
    if fault::take(FaultPoint::SessionEnd).is_some() {
        tracing::warn!(event = "fault.injected", point = "session_end");
        end_message(end, WM_QUERYENDSESSION, 0, "fault");
        end_message(end, WM_ENDSESSION, 1, "fault");
    }
}

/// How often a failed session-notification registration is retried: early
/// at boot the session service may not accept it yet (待实测).
const REGISTER_RETRY: Duration = Duration::from_secs(5);
/// Registration attempts before giving up (one minute).
const REGISTER_ATTEMPTS: u32 = 12;

unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    // 待实测: that the hidden window receives both messages, and how much of
    // the shutdown time budget the callbacks may use.
    if let Some(answer) = end_message(&SESSION_END, msg, w, "system") {
        return answer;
    }
    if let Some(signal) = signal_for(msg, w) {
        raise(signal);
    }
    DefWindowProcW(hwnd, msg, w, l)
}

unsafe extern "system" fn desktop_switch(
    _: HWINEVENTHOOK,
    _: u32,
    _: HWND,
    _: i32,
    _: i32,
    _: u32,
    _: u32,
) {
    raise(Signal::DesktopSwitch);
}

/// Starts the session window thread. A failure is logged and leaves only
/// the 30 s modifier check (B-05) to heal lost key-ups.
pub(super) fn start() {
    let spawned = std::thread::Builder::new()
        .name("quicker-session".into())
        .spawn(run);
    if spawned.is_err() {
        tracing::warn!(event = "session.start_failed", stage = "thread");
    }
}

fn run() {
    let class = wide("QuicktextSessionWindow");
    // SAFETY: the class name outlives every call below; the window and the
    // WinEvent hook belong to this thread, which pumps their messages.
    unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        let window_class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            lpszClassName: class.as_ptr(),
            ..zeroed()
        };
        // Zero when the class already exists; CreateWindowExW reports real
        // failures.
        RegisterClassW(&window_class);
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            class.as_ptr(),
            class.as_ptr(),
            WS_POPUP,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance,
            std::ptr::null(),
        );
        if hwnd.is_null() {
            tracing::warn!(event = "session.start_failed", stage = "window");
            return;
        }
        let desktop = SetWinEventHook(
            EVENT_SYSTEM_DESKTOPSWITCH,
            EVENT_SYSTEM_DESKTOPSWITCH,
            std::ptr::null_mut(),
            Some(desktop_switch),
            0,
            0,
            WINEVENT_OUTOFCONTEXT,
        );
        if desktop.is_null() {
            tracing::warn!(event = "session.start_failed", stage = "desktop_switch");
        }
        let mut attempts = 0;
        let mut registered = false;
        'pump: loop {
            if !registered && attempts < REGISTER_ATTEMPTS {
                attempts += 1;
                registered = WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION) != 0;
                if registered {
                    tracing::info!(event = "session.registered", attempts);
                } else if attempts == REGISTER_ATTEMPTS {
                    tracing::warn!(event = "session.start_failed", stage = "wts", attempts);
                }
            }
            let wait = if !registered && attempts < REGISTER_ATTEMPTS {
                u32::try_from(REGISTER_RETRY.as_millis()).unwrap_or(INFINITE)
            } else {
                INFINITE
            };
            MsgWaitForMultipleObjectsEx(
                0,
                std::ptr::null(),
                wait,
                QS_ALLINPUT,
                MWMO_INPUTAVAILABLE,
            );
            let mut msg: MSG = zeroed();
            while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                if msg.message == WM_QUIT {
                    break 'pump;
                }
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        if registered {
            WTSUnRegisterSessionNotification(hwnd);
        }
        if !desktop.is_null() {
            UnhookWinEvent(desktop);
        }
        DestroyWindow(hwnd);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn session_changes_and_resume_map_to_resync_signals() {
        let session = |code: u32| signal_for(WM_WTSSESSION_CHANGE, code as usize);
        assert_eq!(session(WTS_SESSION_LOCK), Some(Signal::Lock));
        assert_eq!(session(WTS_SESSION_UNLOCK), Some(Signal::Unlock));
        assert_eq!(session(WTS_CONSOLE_CONNECT), Some(Signal::Connect));
        assert_eq!(session(WTS_REMOTE_CONNECT), Some(Signal::Connect));
        assert_eq!(session(WTS_SESSION_LOGON), Some(Signal::Logon));
        assert_eq!(
            session(WTS_SESSION_REMOTE_CONTROL),
            Some(Signal::RemoteControl)
        );
        for ignored in [
            WTS_CONSOLE_DISCONNECT,
            WTS_REMOTE_DISCONNECT,
            WTS_SESSION_LOGOFF,
        ] {
            assert_eq!(session(ignored), None);
        }
        let power = |code: u32| signal_for(WM_POWERBROADCAST, code as usize);
        assert_eq!(power(PBT_APMRESUMEAUTOMATIC), Some(Signal::Resume));
        assert_eq!(power(PBT_APMRESUMESUSPEND), Some(Signal::Resume));
        assert_eq!(power(PBT_APMSUSPEND), None);
        assert_eq!(signal_for(WM_KEYDOWN, WTS_SESSION_UNLOCK as usize), None);
        assert_eq!(signal_for(WM_WTSSESSION_CHANGE, usize::MAX), None);
    }

    #[test]
    fn pending_signals_are_taken_once() {
        let signals = SessionSignals::new();
        assert_eq!(signals.take().count(), 0);
        signals.raise(Signal::Unlock);
        signals.raise(Signal::Resume);
        signals.raise(Signal::Unlock);
        assert_eq!(
            signals.take().collect::<Vec<_>>(),
            [Signal::Unlock, Signal::Resume]
        );
        assert_eq!(signals.take().count(), 0);
        let names: BTreeSet<_> = Signal::ALL.iter().map(|signal| signal.name()).collect();
        assert_eq!(names.len(), Signal::ALL.len(), "distinct log names");
    }
}
