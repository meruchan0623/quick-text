//! Test doubles for the platform ports (S-23). Built for unit tests and, with
//! `--features mock`, for downstream pipeline tests. Never touches the OS.
use crate::health::{HealthState, ReasonCode};
use crate::traits::{ClipboardBackend, Clock, ForegroundProbe, Injector, InputContext};
use crate::Foreground;
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// One recorded [`Injector`] call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InjectCall {
    Text(String),
    Keys(Vec<u16>),
    Chord(Vec<u16>),
    Key {
        key: u16,
        up: bool,
    },
    Mouse {
        flags: u32,
        data: u32,
        x: i32,
        y: i32,
    },
}

impl InjectCall {
    /// Events the call requests under the [`Injector`] counting contract.
    pub fn requested(&self) -> u32 {
        match self {
            Self::Text(text) => text.encode_utf16().count() as u32 * 2,
            Self::Keys(keys) | Self::Chord(keys) => keys.len() as u32 * 2,
            Self::Key { .. } | Self::Mouse { .. } => 1,
        }
    }
}

type CallHook = Box<dyn FnMut(&InjectCall) + Send>;

/// Records every call and accepts all requested events unless a short count
/// is configured for a call index.
#[derive(Default)]
pub struct MockInjector {
    calls: Mutex<Vec<InjectCall>>,
    short: Mutex<Vec<(usize, u32)>>,
    hook: Mutex<Option<CallHook>>,
}

impl MockInjector {
    pub fn new() -> Self {
        Self::default()
    }
    /// Makes call number `index` (0-based, across all methods) report only
    /// `accepted` events.
    pub fn short_count(&self, index: usize, accepted: u32) {
        self.short.lock().push((index, accepted));
    }
    /// Runs `hook` after each call is recorded (e.g. to simulate user input,
    /// a focus change or a clipboard listener reacting to the output).
    pub fn on_call(&self, hook: impl FnMut(&InjectCall) + Send + 'static) {
        *self.hook.lock() = Some(Box::new(hook));
    }
    pub fn calls(&self) -> Vec<InjectCall> {
        self.calls.lock().clone()
    }
    /// Concatenated text of all `send_text` calls.
    pub fn typed(&self) -> String {
        self.calls
            .lock()
            .iter()
            .filter_map(|call| match call {
                InjectCall::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }
    /// Number of `send_keys` taps of `key`.
    pub fn taps(&self, key: u16) -> usize {
        self.calls
            .lock()
            .iter()
            .map(|call| match call {
                InjectCall::Keys(keys) => keys.iter().filter(|k| **k == key).count(),
                _ => 0,
            })
            .sum()
    }
    fn record(&self, call: InjectCall) -> u32 {
        let index = {
            let mut calls = self.calls.lock();
            calls.push(call.clone());
            calls.len() - 1
        };
        // Run the hook unlocked so it may inspect this injector.
        let hook = self.hook.lock().take();
        if let Some(mut hook) = hook {
            hook(&call);
            self.hook.lock().get_or_insert(hook);
        }
        self.short
            .lock()
            .iter()
            .find(|(at, _)| *at == index)
            .map_or_else(|| call.requested(), |(_, accepted)| *accepted)
    }
}

impl Injector for MockInjector {
    fn send_text(&self, text: &str) -> u32 {
        self.record(InjectCall::Text(text.into()))
    }
    fn send_keys(&self, keys: &[u16]) -> u32 {
        self.record(InjectCall::Keys(keys.to_vec()))
    }
    fn send_chord(&self, keys: &[u16]) -> u32 {
        self.record(InjectCall::Chord(keys.to_vec()))
    }
    fn send_key(&self, key: u16, up: bool) -> u32 {
        self.record(InjectCall::Key { key, up })
    }
    fn send_mouse(&self, flags: u32, data: u32, x: i32, y: i32) -> u32 {
        self.record(InjectCall::Mouse { flags, data, x, y })
    }
}

pub type ClipboardFormats = Vec<(u32, Vec<u8>)>;

type LockHook = Box<dyn FnMut(usize) + Send>;

struct ClipboardModel {
    owner: u64,
    sequence: u32,
    formats: ClipboardFormats,
    open: bool,
    busy: usize,
    fail_set: usize,
    churn: bool,
    clears: usize,
    locks: usize,
    lock_hook: Option<LockHook>,
    /// Virtual time: advanced by `pause` and by slow reads, never by the OS.
    base: Instant,
    elapsed: Duration,
    pauses: Vec<Duration>,
    read_delay: Duration,
    health: Option<(HealthState, Option<ReasonCode>)>,
    /// Registered format names; ids start at 0xC000 like Win32's.
    registry: Vec<String>,
}

/// In-memory clipboard following Win32 rules: clearing takes ownership and
/// every clear/set advances the sequence. Clones share one clipboard, so a
/// test can script external changes while a lease holds another handle.
#[derive(Clone)]
pub struct MockClipboard {
    state: Arc<Mutex<ClipboardModel>>,
}

impl MockClipboard {
    /// Owner identity of every lease opened on the mock.
    pub const LEASE: u64 = 0x1EA5E;

    pub fn new(owner: u64, formats: ClipboardFormats) -> Self {
        Self {
            state: Arc::new(Mutex::new(ClipboardModel {
                owner,
                sequence: 1,
                formats,
                open: false,
                busy: 0,
                fail_set: 0,
                churn: false,
                clears: 0,
                locks: 0,
                lock_hook: None,
                base: Instant::now(),
                elapsed: Duration::ZERO,
                pauses: vec![],
                read_delay: Duration::ZERO,
                health: None,
                registry: vec![],
            })),
        }
    }
    pub fn content(&self) -> ClipboardFormats {
        self.state.lock().formats.clone()
    }
    pub fn owner_now(&self) -> u64 {
        self.state.lock().owner
    }
    pub fn sequence_now(&self) -> u32 {
        self.state.lock().sequence
    }
    /// Number of successful clears (each is a destructive write).
    pub fn clears(&self) -> usize {
        self.state.lock().clears
    }
    /// A listener or synthesized format advances only the sequence.
    pub fn bump_sequence(&self) {
        self.state.lock().sequence += 1;
    }
    /// Another application takes ownership and writes `formats`.
    pub fn take_over(&self, owner: u64, formats: ClipboardFormats) {
        let mut state = self.state.lock();
        state.owner = owner;
        state.formats = formats;
        state.sequence += 2;
    }
    /// The payload changes without an owner change.
    pub fn tamper(&self, formats: ClipboardFormats) {
        let mut state = self.state.lock();
        state.formats = formats;
        state.sequence += 1;
    }
    /// While on, every lock release advances the sequence (never settles).
    pub fn churn_sequence(&self, on: bool) {
        self.state.lock().churn = on;
    }
    /// The next `attempts` lock attempts report the clipboard as busy.
    pub fn set_busy(&self, attempts: usize) {
        self.state.lock().busy = attempts;
    }
    /// The next `count` `set` calls fail.
    pub fn fail_set(&self, count: usize) {
        self.state.lock().fail_set = count;
    }
    /// Runs `hook` with the 0-based index of each successful lock, just
    /// before the clipboard opens (e.g. to change the world mid-lease).
    pub fn on_lock(&self, hook: impl FnMut(usize) + Send + 'static) {
        self.state.lock().lock_hook = Some(Box::new(hook));
    }
    fn open(&self) -> parking_lot::MutexGuard<'_, ClipboardModel> {
        let state = self.state.lock();
        assert!(state.open, "clipboard accessed outside locked()");
        state
    }
    /// Whether a lease currently holds the clipboard open (e.g. checked from
    /// an injector hook: input must never be sent inside a critical section).
    pub fn is_open(&self) -> bool {
        self.state.lock().open
    }
    /// Every [`ClipboardBackend::pause`] request, in order.
    pub fn pauses(&self) -> Vec<Duration> {
        self.state.lock().pauses.clone()
    }
    /// Each read with the clipboard open takes `delay` of virtual time (a
    /// slow owner rendering data), to exercise the lock-hold budget.
    pub fn set_read_delay(&self, delay: Duration) {
        self.state.lock().read_delay = delay;
    }
    /// Latest [`ClipboardBackend::report_health`] call, if any.
    pub fn health(&self) -> Option<(HealthState, Option<ReasonCode>)> {
        self.state.lock().health
    }
    /// Id [`ClipboardBackend::register_format`] returned for `name`.
    pub fn registered(&self, name: &str) -> Option<u32> {
        let state = self.state.lock();
        let index = state.registry.iter().position(|known| known == name)?;
        Some(0xC000 + index as u32)
    }
}

impl ClipboardBackend for MockClipboard {
    type Staged = Vec<u8>;
    fn identity(&self) -> u64 {
        Self::LEASE
    }
    fn owner(&self) -> u64 {
        self.state.lock().owner
    }
    fn sequence(&self) -> u32 {
        self.state.lock().sequence
    }
    fn locked<R>(&self, f: impl FnOnce() -> R) -> Option<R> {
        let (index, hook) = {
            let mut state = self.state.lock();
            if state.busy > 0 {
                state.busy -= 1;
                return None;
            }
            state.locks += 1;
            (state.locks - 1, state.lock_hook.take())
        };
        // Run the hook unlocked so it may script this clipboard.
        if let Some(mut hook) = hook {
            hook(index);
            self.state.lock().lock_hook.get_or_insert(hook);
        }
        {
            let mut state = self.state.lock();
            assert!(!state.open, "clipboard opened twice");
            state.open = true;
        }
        let result = f();
        let mut state = self.state.lock();
        state.open = false;
        if state.churn {
            state.sequence += 1;
        }
        Some(result)
    }
    fn formats(&self) -> Vec<u32> {
        self.open()
            .formats
            .iter()
            .map(|(format, _)| *format)
            .collect()
    }
    fn read(&self, format: u32, limit: usize) -> Result<Option<Vec<u8>>, String> {
        let mut state = self.open();
        let delay = state.read_delay;
        state.elapsed += delay;
        Ok(state
            .formats
            .iter()
            .find(|(f, bytes)| *f == format && !bytes.is_empty() && bytes.len() <= limit)
            .map(|(_, bytes)| bytes.clone()))
    }
    fn clear(&self) -> bool {
        let mut state = self.open();
        state.formats.clear();
        state.owner = Self::LEASE;
        state.sequence += 1;
        state.clears += 1;
        true
    }
    fn stage(&self, bytes: &[u8]) -> Result<(Vec<u8>, Vec<u8>), String> {
        // Allocation belongs before the critical section, never inside it.
        assert!(
            !self.state.lock().open,
            "payload staged with the clipboard open"
        );
        Ok((bytes.to_vec(), bytes.to_vec()))
    }
    fn set(&self, format: u32, staged: Vec<u8>) -> bool {
        let mut state = self.open();
        if state.fail_set > 0 {
            state.fail_set -= 1;
            return false;
        }
        state.formats.retain(|(f, _)| *f != format);
        state.formats.push((format, staged));
        state.sequence += 1;
        true
    }
    fn pause(&self, duration: Duration) {
        let mut state = self.state.lock();
        assert!(!state.open, "paused with the clipboard open");
        state.elapsed += duration;
        state.pauses.push(duration);
    }
    fn now(&self) -> Instant {
        let state = self.state.lock();
        state.base + state.elapsed
    }
    fn report_health(&self, state: HealthState, reason: Option<ReasonCode>) {
        let mut model = self.state.lock();
        assert!(!model.open, "health reported with the clipboard open");
        model.health = Some((state, reason));
    }
    fn register_format(&self, name: &str) -> u32 {
        let mut state = self.state.lock();
        let index = match state.registry.iter().position(|known| known == name) {
            Some(index) => index,
            None => {
                state.registry.push(name.to_string());
                state.registry.len() - 1
            }
        };
        0xC000 + index as u32
    }
}

struct ForegroundModel {
    foreground: Foreground,
    protection: Option<String>,
    context: Option<InputContext>,
    context_reads: usize,
}

/// Scriptable foreground window, protection verdict and input context.
/// Clones share state.
#[derive(Clone)]
pub struct MockForeground {
    state: Arc<Mutex<ForegroundModel>>,
}

impl MockForeground {
    /// Starts with [`MockForeground::context_for`] of `foreground`.
    pub fn new(foreground: Foreground) -> Self {
        let context = Self::context_for(&foreground);
        Self {
            state: Arc::new(Mutex::new(ForegroundModel {
                foreground,
                protection: None,
                context: Some(context),
                context_reads: 0,
            })),
        }
    }
    /// Input context focused on `foreground`: epochs 1, US English layout.
    pub fn context_for(foreground: &Foreground) -> InputContext {
        InputContext {
            focus: foreground.id,
            focus_epoch: 1,
            caret_epoch: 1,
            layout: 0x0409,
        }
    }
    /// Snapshot helper: a Windows window `id` owned by `process`.
    pub fn window(id: u64, process: &str) -> Foreground {
        Foreground {
            platform: "windows".into(),
            bundle_id: String::new(),
            id,
            process: process.into(),
            title: String::new(),
        }
    }
    /// Replaces the window snapshot only; the input context is separate.
    pub fn set_foreground(&self, foreground: Foreground) {
        self.state.lock().foreground = foreground;
    }
    pub fn set_protection(&self, reason: Option<&str>) {
        self.state.lock().protection = reason.map(str::to_owned);
    }
    /// `None` simulates an unreadable context.
    pub fn set_input_context(&self, context: Option<InputContext>) {
        self.state.lock().context = context;
    }
    /// Applies `change` to the current context, if any.
    pub fn update_input_context(&self, change: impl FnOnce(&mut InputContext)) {
        if let Some(context) = self.state.lock().context.as_mut() {
            change(context);
        }
    }
    /// How often [`ForegroundProbe::input_context`] was read.
    pub fn input_context_reads(&self) -> usize {
        self.state.lock().context_reads
    }
}

impl ForegroundProbe for MockForeground {
    fn foreground(&self) -> Foreground {
        self.state.lock().foreground.clone()
    }
    fn protection(&self) -> Option<String> {
        self.state.lock().protection.clone()
    }
    fn input_context(&self) -> Option<InputContext> {
        let mut state = self.state.lock();
        state.context_reads += 1;
        state.context
    }
}

/// Manual clock: `sleep` returns immediately and advances time.
pub struct FakeClock {
    base: Instant,
    state: Mutex<(Duration, Vec<Duration>)>,
}

impl FakeClock {
    /// Wall time at creation: 2026-01-01T00:00:00Z.
    pub const WALL_START_MS: i64 = 1_767_225_600_000;

    pub fn new() -> Self {
        Self {
            base: Instant::now(),
            state: Mutex::new((Duration::ZERO, vec![])),
        }
    }
    pub fn advance(&self, by: Duration) {
        self.state.lock().0 += by;
    }
    /// Time advanced since creation.
    pub fn elapsed(&self) -> Duration {
        self.state.lock().0
    }
    /// Every `sleep` request, in order.
    pub fn sleeps(&self) -> Vec<Duration> {
        self.state.lock().1.clone()
    }
}

impl Default for FakeClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Instant {
        self.base + self.state.lock().0
    }
    fn wall_ms(&self) -> i64 {
        Self::WALL_START_MS + self.state.lock().0.as_millis() as i64
    }
    fn sleep(&self, duration: Duration) {
        let mut state = self.state.lock();
        state.0 += duration;
        state.1.push(duration);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injector_records_calls_and_reports_short_counts() {
        let injector = MockInjector::new();
        injector.short_count(1, 1);
        assert_eq!(injector.send_text("ab"), 4);
        assert_eq!(injector.send_keys(&[8]), 1);
        assert_eq!(injector.send_chord(&[17, 86]), 4);
        assert_eq!(injector.typed(), "ab");
        assert_eq!(injector.taps(8), 1);
        assert_eq!(injector.calls().len(), 3);
    }

    #[test]
    fn clipboard_follows_win32_ownership_and_sequence_rules() {
        let clipboard = MockClipboard::new(7, vec![(13, vec![1, 0])]);
        let before = clipboard.sequence_now();
        assert!(clipboard
            .locked(|| clipboard.clear() && clipboard.set(13, vec![2, 0]))
            .unwrap());
        assert_eq!(clipboard.owner_now(), MockClipboard::LEASE);
        assert_eq!(clipboard.sequence_now(), before + 2);
        clipboard.set_busy(1);
        assert!(clipboard.locked(|| ()).is_none());
        assert_eq!(
            clipboard.locked(|| clipboard.read(13, 1)).unwrap(),
            Ok(None)
        );
    }

    #[test]
    #[should_panic(expected = "paused with the clipboard open")]
    fn clipboard_rejects_waiting_inside_the_critical_section() {
        let clipboard = MockClipboard::new(7, vec![]);
        clipboard.locked(|| clipboard.pause(Duration::from_millis(1)));
    }

    #[test]
    fn fake_clock_advances_only_when_asked() {
        let clock = FakeClock::new();
        let start = clock.now();
        clock.sleep(Duration::from_millis(25));
        clock.advance(Duration::from_millis(5));
        assert_eq!(clock.now() - start, Duration::from_millis(30));
        assert_eq!(clock.sleeps(), vec![Duration::from_millis(25)]);
        assert_eq!(clock.wall_ms(), FakeClock::WALL_START_MS + 30);
    }
}
