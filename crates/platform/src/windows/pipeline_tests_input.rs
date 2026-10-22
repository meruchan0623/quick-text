//! L2 input-pipeline tests: the hook and RawInput callback bodies, their
//! lanes and the input consumer, on fake probes. Nothing here installs a
//! hook, registers RawInput, reads the asynchronous key state or injects
//! input.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use super::actions::{execute_with_proof, Ports};
use super::executor::Candidate;
use super::hook::{
    classify, handle_key, handle_mouse, pointer_move, HookAction, HookPipe, HookRecord, HookSet,
    KeyInfo, KeyRecord, MouseInfo, Origin,
};
use super::input::{
    Consumer, Lane, Liveness, Monitor, OwnWindows, Registration, SourcePorts, Sources,
    RESYNC_SECOND_PASS,
};
use super::keystate::{
    track_own_input, AsyncKeyProbe, KeyBits, OwnInput, INJECTION_QUIET, MODIFIER_VERIFY_AFTER,
    PHYSICAL,
};
use super::liveness::{MATCH_WINDOW, MISS_LIMIT, REPEAT_RUN};
use super::rawinput::{raw_record, InputEvent, RawRecord, KEYBOARD_LAYOUT_CACHE_INTERVAL};
use super::recording::capture_gap;
use super::session::{end_message, SessionEnd, SessionSignals, Signal};
use super::*;
use crate::health::{Component, ComponentHealth, HealthState, ReasonCode};
use crate::mock::{FakeClock, InjectCall, MockForeground, MockInjector};
use crate::traits::{Clock, ForegroundProbe, InputContext};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::AtomicUsize;
use std::sync::{MutexGuard, PoisonError};

const FOCUS: u64 = 0x2a;
const APP: &str = "notepad.exe";

/// Counts allocations made by the current thread while tracking is on.
struct CountingAlloc;
thread_local! {
    static TRACKING: Cell<bool> = const { Cell::new(false) };
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}
fn note_allocation() {
    let _ = TRACKING.try_with(|tracking| {
        if tracking.get() {
            let _ = ALLOCATIONS.try_with(|count| count.set(count.get() + 1));
        }
    });
}
// SAFETY: every call forwards to the system allocator unchanged.
unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note_allocation();
        System.alloc(layout)
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note_allocation();
        System.alloc_zeroed(layout)
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note_allocation();
        System.realloc(ptr, layout, new_size)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout)
    }
}
#[global_allocator]
static COUNTING: CountingAlloc = CountingAlloc;

/// Runs `body` and returns how many allocations it made on this thread.
fn allocations<R>(body: impl FnOnce() -> R) -> (R, usize) {
    ALLOCATIONS.with(|count| count.set(0));
    TRACKING.with(|tracking| tracking.set(true));
    let value = body();
    TRACKING.with(|tracking| tracking.set(false));
    (value, ALLOCATIONS.with(Cell::get))
}

/// Captures `tracing` events as " field=value" lines.
mod capture {
    use std::fmt::{Debug, Write};
    use std::sync::{Arc, Mutex};
    use tracing::field::{Field, Visit};
    use tracing::subscriber::Interest;
    use tracing::{span, Event, Metadata, Subscriber};

    struct Line(String);
    impl Visit for Line {
        fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
            let _ = write!(self.0, " {}={:?}", field.name(), value);
        }
    }
    struct Capture(Arc<Mutex<Vec<String>>>);
    impl Subscriber for Capture {
        fn register_callsite(&self, _: &'static Metadata<'static>) -> Interest {
            Interest::always()
        }
        fn enabled(&self, _: &Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &span::Attributes<'_>) -> span::Id {
            span::Id::from_u64(1)
        }
        fn record(&self, _: &span::Id, _: &span::Record<'_>) {}
        fn record_follows_from(&self, _: &span::Id, _: &span::Id) {}
        fn event(&self, event: &Event<'_>) {
            let mut line = Line(event.metadata().level().to_string());
            event.record(&mut line);
            self.0.lock().unwrap().push(line.0);
        }
        fn enter(&self, _: &span::Id) {}
        fn exit(&self, _: &span::Id) {}
    }
    /// Runs `body` with a capturing subscriber on this thread.
    pub(super) fn logs<R>(body: impl FnOnce() -> R) -> (R, Vec<String>) {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let value = tracing::subscriber::with_default(Capture(lines.clone()), body);
        let captured = lines.lock().unwrap().clone();
        (value, captured)
    }
}
fn count_of(lines: &[String], needle: &str) -> usize {
    lines.iter().filter(|line| line.contains(needle)).count()
}

/// Lock-free probe with a fixed input context.
#[derive(Clone, Copy)]
struct FixedProbe(InputContext);
impl ForegroundProbe for FixedProbe {
    fn foreground(&self) -> Foreground {
        MockForeground::window(self.0.focus, APP)
    }
    fn protection(&self) -> Option<String> {
        None
    }
    fn input_context(&self) -> Option<InputContext> {
        Some(self.0)
    }
}
fn window() -> Foreground {
    MockForeground::window(FOCUS, APP)
}
fn context() -> InputContext {
    MockForeground::context_for(&window())
}

/// Hook tests consume the global HookPanic fault point; they must not
/// overlap.
static SERIAL: Mutex<()> = Mutex::new(());
fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(PoisonError::into_inner)
}
fn nothing() {}

/// An unpaused engine with one immediate literal rule.
fn engine(trigger: &str) -> Arc<Shared> {
    let shared = Shared::new();
    shared.paused.store(false, Ordering::SeqCst);
    let config = Config {
        rules: vec![Rule {
            id: trigger.into(),
            match_spec: MatchSpec {
                kind: MatchKind::Literal,
                value: trigger.into(),
            },
            trigger_mode: TriggerMode::Immediate,
            ..Default::default()
        }],
        ..Default::default()
    };
    *shared.compiled.lock().unwrap() = compile_config(&config).unwrap();
    *shared.config.lock().unwrap() = config;
    shared
}
/// Own-input tracking no injection ever touches.
static NO_INJECTIONS: OwnInput = OwnInput::new();
/// A seeded, empty key mirror owned by one test.
fn leaked_keys() -> &'static KeyBits {
    let keys = Box::leak(Box::new(KeyBits::new()));
    keys.mark_seeded();
    keys
}
fn hardware(vk: u16) -> KeyInfo {
    KeyInfo {
        vk,
        scan: 0,
        flags: 0,
        extra: 0,
        time: 0,
    }
}
fn injected(vk: u16) -> KeyInfo {
    KeyInfo {
        flags: LLKHF_INJECTED,
        ..hardware(vk)
    }
}
fn mouse(msg: u32, x: i32, y: i32) -> MouseInfo {
    MouseInfo {
        msg,
        data: 0,
        x,
        y,
        flags: 0,
        extra: 0,
        time: 0,
    }
}
/// What the RawInput callback captures for a hardware key typing `ch`.
fn raw_key(
    shared: &Shared,
    keys: &KeyBits,
    lane: &Lane<RawRecord>,
    probe: &impl ForegroundProbe,
    ch: char,
) -> bool {
    let event = InputEvent::key_down(ch.to_ascii_uppercase() as u16, &ch.to_string(), true);
    raw_record(shared, keys, &event, probe).is_some_and(|record| lane.push(record))
}
/// Drains both lanes once; returns (rule id, generation) per accepted candidate.
fn drain(
    consumer: &mut Consumer,
    shared: &Shared,
    hooks: &HookPipe,
    raw: &Lane<RawRecord>,
    probe: &impl ForegroundProbe,
) -> Vec<(String, u64)> {
    let mut found = Vec::new();
    consumer.drain(shared, hooks, raw, probe, &mut |candidate: Candidate| {
        found.push((candidate.rule.id.clone(), candidate.generation));
        true
    });
    found
}
fn records(hooks: &HookPipe) -> Vec<HookRecord> {
    std::iter::from_fn(|| hooks.lane().pop()).collect()
}
/// Types `word` as hardware keys through the RawInput lane.
fn type_word(
    shared: &Shared,
    keys: &KeyBits,
    lane: &Lane<RawRecord>,
    probe: &impl ForegroundProbe,
    word: &str,
) {
    for ch in word.chars() {
        assert!(raw_key(shared, keys, lane, probe, ch));
    }
}
/// Fake asynchronous key state: the listed codes are down; reads counted.
#[derive(Default)]
struct FakeAsyncKeys {
    down: Mutex<Vec<u16>>,
    reads: AtomicUsize,
}
impl FakeAsyncKeys {
    fn with(down: &[u16]) -> Self {
        let keys = Self::default();
        keys.set(down);
        keys
    }
    fn set(&self, down: &[u16]) {
        *self.down.lock().unwrap() = down.to_vec();
    }
    fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }
}
impl AsyncKeyProbe for FakeAsyncKeys {
    fn down(&self, vk: u16) -> bool {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.down.lock().unwrap().contains(&vk)
    }
}

#[test]
fn hook_and_raw_callback_bodies_neither_allocate_nor_block() {
    let _serial = serial();
    let shared = engine("btw");
    let keys = leaked_keys();
    let plain = Arc::new(HookPipe::new(keys, 64, false, nothing));
    let accept = Arc::new(HookPipe::new(keys, 64, true, nothing));
    let raw = Arc::new(Lane::<RawRecord>::new(64, nothing));
    let probe = FixedProbe(context());
    // Hold every engine lock a callback could reach: a body that takes one
    // never finishes.
    let compiled = shared.compiled.lock().unwrap();
    let config = shared.config.lock().unwrap();
    let recording = shared.recording.lock().unwrap();
    let last = shared.last.lock().unwrap();
    let (tx, rx) = mpsc::channel();
    let worker = {
        let (shared, plain, accept, raw) =
            (shared.clone(), plain.clone(), accept.clone(), raw.clone());
        std::thread::spawn(move || {
            let measured = allocations(|| {
                let event = InputEvent::key_down(0x42, "b", true);
                let up = InputEvent::key_up(0x42, true);
                let raw_queued = raw.contained(false, || {
                    raw_record(&shared, keys, &event, &probe).is_some_and(|r| raw.push(r))
                        && raw_record(&shared, keys, &up, &probe).is_some_and(|r| raw.push(r))
                });
                let actions = [
                    handle_key(&plain, &shared, &probe, WM_KEYDOWN, hardware(0x42)),
                    handle_key(&plain, &shared, &probe, WM_KEYUP, hardware(0x42)),
                    handle_key(&plain, &shared, &probe, WM_KEYDOWN, injected(0x43)),
                    handle_key(&accept, &shared, &probe, WM_KEYDOWN, injected(0x44)),
                    handle_key(&plain, &shared, &probe, WM_KEYDOWN, hardware(VK_LCONTROL)),
                    handle_key(&plain, &shared, &probe, WM_KEYDOWN, hardware(VK_LSHIFT)),
                    handle_key(&plain, &shared, &probe, WM_KEYDOWN, hardware(VK_ESCAPE)),
                ];
                handle_mouse(&plain, &shared, mouse(WM_MOUSEMOVE, -5, 7));
                handle_mouse(&plain, &shared, mouse(WM_LBUTTONDOWN, -5, 7));
                (raw_queued, actions)
            });
            tx.send(measured).unwrap();
        })
    };
    let ((raw_queued, actions), count) = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("a callback body blocked on an engine lock");
    drop((compiled, config, recording, last));
    worker.join().unwrap();
    assert_eq!(count, 0, "callback bodies must not allocate");
    assert!(raw_queued);
    use HookAction::{Pass, Swallow};
    assert_eq!(actions, [Pass, Pass, Pass, Pass, Pass, Pass, Swallow]);
    assert!(shared.cancel.load(Ordering::SeqCst));
    let plain = records(&plain);
    assert!(
        matches!(
            plain.as_slice(),
            [
                HookRecord::Key(KeyRecord {
                    vk: 0x42,
                    down: true,
                    acceptance: None,
                    ..
                }),
                HookRecord::Key(KeyRecord {
                    vk: 0x42,
                    down: false,
                    ..
                }),
                HookRecord::External,
                HookRecord::Key(KeyRecord {
                    vk: VK_LCONTROL,
                    ..
                }),
                HookRecord::Key(KeyRecord { vk: VK_LSHIFT, .. }),
                HookRecord::Cancel,
                HookRecord::Move { x: -5, y: 7, .. },
                HookRecord::Mouse {
                    msg: WM_LBUTTONDOWN,
                    x: -5,
                    y: 7,
                    ..
                },
            ]
        ),
        "{plain:?}"
    );
    assert!(matches!(
        records(&accept).as_slice(),
        [HookRecord::Key(KeyRecord {
            vk: 0x44,
            acceptance: Some(_),
            ..
        })]
    ));
    assert!(keys.held(VK_LBUTTON), "the click updated the mirror");
    assert!(raw.pop().is_some_and(|record| record.down()));
    assert!(raw.pop().is_some_and(|record| !record.down()));
}

#[test]
fn a_full_hook_queue_drops_records_instead_of_blocking_the_hook() {
    let _serial = serial();
    let shared = engine("btw");
    let hooks = HookPipe::new(leaked_keys(), 2, false, nothing);
    let raw = Lane::new(2, nothing);
    let probe = FixedProbe(context());
    for vk in [0x41, 0x42, 0x43] {
        assert_eq!(
            handle_key(&hooks, &shared, &probe, WM_KEYDOWN, hardware(vk)),
            HookAction::Pass
        );
    }
    assert_eq!(hooks.lane().dropped(), 1);
    let mut consumer = Consumer::default();
    let (_, lines) = capture::logs(|| drain(&mut consumer, &shared, &hooks, &raw, &probe));
    assert_eq!(
        count_of(&lines, "event=\"input.queue_full\" source=\"hook\" count=1"),
        1,
        "{lines:?}"
    );
    let (_, lines) = capture::logs(|| drain(&mut consumer, &shared, &hooks, &raw, &probe));
    assert_eq!(count_of(&lines, "input.queue_full"), 0, "reported once");
}

#[test]
fn raw_keys_flow_through_the_lane_and_consumer_into_a_candidate() {
    let shared = engine("btw");
    let keys = leaked_keys();
    let hooks = HookPipe::new(keys, 8, false, nothing);
    let raw = Lane::new(8, nothing);
    let probe = MockForeground::new(window());
    for ch in "btw".chars() {
        assert!(raw_key(&shared, keys, &raw, &probe, ch));
    }
    let mut consumer = Consumer::default();
    assert_eq!(
        drain(&mut consumer, &shared, &hooks, &raw, &probe),
        [("btw".to_string(), 3)]
    );
    assert!(raw.pop().is_none());
}

#[test]
fn a_dropped_raw_key_breaks_contiguity_instead_of_joining_its_neighbours() {
    let probe = MockForeground::new(window());
    let mut consumer = Consumer::default();
    // Control: "b" then "w" are contiguous and match.
    let shared = engine("bw");
    let keys = leaked_keys();
    let hooks = HookPipe::new(keys, 8, false, nothing);
    let raw = Lane::new(2, nothing);
    assert!(raw_key(&shared, keys, &raw, &probe, 'b'));
    assert!(raw_key(&shared, keys, &raw, &probe, 'w'));
    assert_eq!(drain(&mut consumer, &shared, &hooks, &raw, &probe).len(), 1);
    // "t" between them is lost to a full queue: "b" + "w" must not match.
    let shared = engine("bw");
    let raw = Lane::new(1, nothing);
    let mut consumer = Consumer::default();
    assert!(raw_key(&shared, keys, &raw, &probe, 'b'));
    assert!(!raw_key(&shared, keys, &raw, &probe, 't'), "queue full");
    assert_eq!(raw.dropped(), 1);
    assert!(drain(&mut consumer, &shared, &hooks, &raw, &probe).is_empty());
    assert!(raw_key(&shared, keys, &raw, &probe, 'w'));
    assert!(drain(&mut consumer, &shared, &hooks, &raw, &probe).is_empty());
}

#[test]
fn a_panicking_callback_passes_the_event_and_is_logged_once_by_the_consumer() {
    let _serial = serial();
    let shared = engine("btw");
    let hooks = HookPipe::new(leaked_keys(), 8, false, nothing);
    let raw = Lane::new(8, nothing);
    let probe = FixedProbe(context());
    let action = hooks
        .lane()
        .contained(HookAction::Pass, || panic!("callback bug"));
    assert_eq!(action, HookAction::Pass);
    assert_eq!(hooks.lane().panics(), 1);
    let mut consumer = Consumer::default();
    let (_, lines) = capture::logs(|| drain(&mut consumer, &shared, &hooks, &raw, &probe));
    assert_eq!(
        count_of(&lines, "event=\"hook.panic\" count=1"),
        1,
        "{lines:?}"
    );
    assert!(lines.iter().all(|line| !line.contains("callback bug")));
    let (_, lines) = capture::logs(|| drain(&mut consumer, &shared, &hooks, &raw, &probe));
    assert_eq!(count_of(&lines, "hook.panic"), 0);
}

#[cfg(feature = "fault-injection")]
#[test]
fn injected_hook_panic_is_contained_and_the_hook_keeps_working() {
    use crate::fault::{self, FaultPoint};
    let _serial = serial();
    let shared = engine("btw");
    let hooks = HookPipe::new(leaked_keys(), 8, false, nothing);
    let raw = Lane::new(8, nothing);
    let probe = FixedProbe(context());
    fault::debug_inject_fault(FaultPoint::HookPanic, 0);
    let first = handle_key(&hooks, &shared, &probe, WM_KEYDOWN, hardware(0x41));
    fault::disarm(FaultPoint::HookPanic);
    assert_eq!(
        first,
        HookAction::Pass,
        "the event still reaches CallNextHookEx"
    );
    assert_eq!(hooks.lane().panics(), 1);
    assert!(hooks.lane().pop().is_none(), "nothing half-recorded");
    assert_eq!(
        handle_key(&hooks, &shared, &probe, WM_KEYDOWN, hardware(0x42)),
        HookAction::Pass
    );
    let mut consumer = Consumer::default();
    let (_, lines) = capture::logs(|| drain(&mut consumer, &shared, &hooks, &raw, &probe));
    assert_eq!(count_of(&lines, "event=\"hook.panic\""), 1, "{lines:?}");
    assert_eq!(hooks.lane().panics(), 1, "the fault fires once");
    assert!(
        hooks.lane().pop().is_none(),
        "the second key was queued and drained"
    );
}

#[test]
fn pointer_moves_are_queued_and_only_clicks_break_contiguity() {
    let _serial = serial();
    let shared = engine("btw");
    let keys = leaked_keys();
    let hooks = HookPipe::new(keys, 8, false, nothing);
    let before = shared.generation.load(Ordering::SeqCst);
    handle_mouse(&hooks, &shared, mouse(WM_MOUSEMOVE, -1920, 1080));
    assert_eq!(shared.generation.load(Ordering::SeqCst), before);
    handle_mouse(
        &hooks,
        &shared,
        MouseInfo {
            extra: MARK,
            ..mouse(WM_LBUTTONDOWN, 0, 0)
        },
    );
    assert_eq!(
        shared.generation.load(Ordering::SeqCst),
        before,
        "our own clicks are ignored"
    );
    handle_mouse(&hooks, &shared, mouse(WM_RBUTTONDOWN, 3, 4));
    assert_eq!(shared.generation.load(Ordering::SeqCst), before + 1);
    assert!(keys.held(VK_RBUTTON) && !keys.held(VK_LBUTTON));
    assert!(matches!(
        records(&hooks).as_slice(),
        [
            HookRecord::Move {
                x: -1920,
                y: 1080,
                ..
            },
            HookRecord::Mouse {
                msg: WM_RBUTTONDOWN,
                x: 3,
                y: 4,
                ..
            }
        ]
    ));
}

/// No window counts as ours (recording tests).
const NOT_OURS: OwnWindows = OwnWindows {
    foreground: || false,
    at: |_, _| false,
};
fn key_at(vk: u16, time: u32) -> KeyInfo {
    KeyInfo {
        time,
        ..hardware(vk)
    }
}
fn mouse_at(msg: u32, x: i32, y: i32, time: u32) -> MouseInfo {
    MouseInfo {
        time,
        ..mouse(msg, x, y)
    }
}
/// Starts a recording on `shared` (callers hold [`serial`]: the recorder's
/// bookkeeping is global).
fn begin_recording(shared: &Shared) {
    *shared.recording.lock().unwrap() = Some(vec![]);
}
/// Ends the recording on `shared` and returns what it recorded.
fn recorded(shared: &Shared) -> Vec<Operation> {
    let steps = shared.recording.lock().unwrap().take().unwrap();
    steps.into_iter().map(|step| step.operation).collect()
}
fn key_down(code: u16) -> Operation {
    Operation::KeyDown {
        key: Key::WindowsCode { code },
    }
}
fn key_up(code: u16) -> Operation {
    Operation::KeyUp {
        key: Key::WindowsCode { code },
    }
}
fn left(state: ButtonState) -> Operation {
    Operation::MouseButton {
        button: MouseButton::Left,
        state,
    }
}
fn delay(ms: u64) -> Operation {
    Operation::Delay { ms }
}

/// M1: macro delays are the gaps between capture times, so a consumer that
/// gets to the records late (here all of them in one drain, after the
/// fact) keeps the user's timing, across a tick-count wrap.
#[test]
fn recorded_delays_keep_capture_intervals_when_the_consumer_drains_late() {
    let _serial = serial();
    let shared = engine("btw");
    begin_recording(&shared);
    let hooks = HookPipe::new(leaked_keys(), 16, false, nothing);
    let raw = Lane::new(4, nothing);
    let probe = FixedProbe(context());
    // The tick count wraps between the key-up and the pointer move.
    let at = |ms: u32| (u32::MAX - 99).wrapping_add(ms);
    handle_key(&hooks, &shared, &probe, WM_KEYDOWN, key_at(0x41, at(0)));
    handle_key(&hooks, &shared, &probe, WM_KEYUP, key_at(0x41, at(70)));
    handle_mouse(&hooks, &shared, mouse_at(WM_MOUSEMOVE, 10, 20, at(570)));
    handle_mouse(&hooks, &shared, mouse_at(WM_LBUTTONDOWN, 10, 20, at(1_820)));
    handle_mouse(&hooks, &shared, mouse_at(WM_LBUTTONUP, 10, 20, at(1_900)));
    handle_key(&hooks, &shared, &probe, WM_KEYDOWN, key_at(0x42, at(1_900)));
    let mut consumer = Consumer::with_windows(NOT_OURS);
    drain(&mut consumer, &shared, &hooks, &raw, &probe);
    assert_eq!(
        recorded(&shared),
        [
            key_down(0x41),
            delay(70),
            key_up(0x41),
            delay(500),
            pointer_move(10, 20),
            delay(1_250),
            left(ButtonState::Down),
            delay(80),
            left(ButtonState::Up),
            key_down(0x42),
        ]
    );
}

#[test]
fn capture_gaps_wrap_never_go_negative_and_are_capped() {
    assert_eq!(capture_gap(10, 80), 70);
    assert_eq!(capture_gap(u32::MAX - 9, 20), 30, "across the wrap");
    assert_eq!(capture_gap(80, 10), 0, "out of order");
    assert_eq!(capture_gap(0, 5_000_000), MAX_DELAY_MS);
}

/// Input aimed at our own windows stays out of a recording: keys while one
/// has the focus, pointer events over one.
#[test]
fn input_aimed_at_our_own_windows_is_not_recorded() {
    let _serial = serial();
    let raw = Lane::new(4, nothing);
    let probe = FixedProbe(context());
    let focused = OwnWindows {
        foreground: || true,
        at: |_, _| false,
    };
    let shared = engine("btw");
    begin_recording(&shared);
    let hooks = HookPipe::new(leaked_keys(), 16, false, nothing);
    handle_key(&hooks, &shared, &probe, WM_KEYDOWN, key_at(0x41, 0));
    handle_mouse(&hooks, &shared, mouse_at(WM_LBUTTONDOWN, 5, 5, 10));
    drain(
        &mut Consumer::with_windows(focused),
        &shared,
        &hooks,
        &raw,
        &probe,
    );
    assert!(recorded(&shared).is_empty());
    let left_half = OwnWindows {
        foreground: || false,
        at: |x, _| x < 0,
    };
    let shared = engine("btw");
    begin_recording(&shared);
    handle_key(&hooks, &shared, &probe, WM_KEYDOWN, key_at(0x41, 0));
    handle_mouse(&hooks, &shared, mouse_at(WM_RBUTTONDOWN, -5, 5, 10));
    handle_mouse(&hooks, &shared, mouse_at(WM_LBUTTONDOWN, 5, 5, 30));
    drain(
        &mut Consumer::with_windows(left_half),
        &shared,
        &hooks,
        &raw,
        &probe,
    );
    assert_eq!(
        recorded(&shared),
        [key_down(0x41), delay(30), left(ButtonState::Down)]
    );
}

/// no-regression #7: the detector never caches the layout, and each key
/// carries the layout read when it was typed.
#[test]
fn every_raw_key_reads_the_layout_and_a_switch_breaks_the_match() {
    assert_eq!(KEYBOARD_LAYOUT_CACHE_INTERVAL, 0);
    let shared = engine("btw");
    let keys = leaked_keys();
    let hooks = HookPipe::new(keys, 8, false, nothing);
    let raw = Lane::new(8, nothing);
    let probe = MockForeground::new(window());
    let mut consumer = Consumer::default();
    assert!(raw_key(&shared, keys, &raw, &probe, 'b'));
    assert!(raw_key(&shared, keys, &raw, &probe, 't'));
    probe.update_input_context(|context| context.layout = 0x0407);
    assert!(raw_key(&shared, keys, &raw, &probe, 'w'));
    assert_eq!(probe.input_context_reads(), 3, "one read per key");
    assert!(drain(&mut consumer, &shared, &hooks, &raw, &probe).is_empty());
}

/// no-regression #4: one pure function decides own / external / hardware
/// from the injected flag and our `dwExtraInfo` tag.
#[test]
fn input_origin_depends_only_on_the_injected_flag_and_our_tag() {
    let cases = [
        (false, 0, Origin::Hardware),
        (false, 0xdead, Origin::Hardware),
        (true, 0, Origin::External),
        (true, MARK + 1, Origin::External),
        (true, MARK - 1, Origin::External),
        (true, MARK, Origin::Own),
        (false, MARK, Origin::Own),
    ];
    for (injected, extra, origin) in cases {
        assert_eq!(
            classify(injected, extra),
            origin,
            "injected={injected} extra={extra:#x}"
        );
    }
    assert_eq!(MARK, 0x5154_5854, "the tag our executor writes");
}

/// no-regression #4: a key injected by another program between hardware
/// keys prevents the expansion, and its RawInput echo never becomes text.
#[test]
fn an_external_injected_key_inside_a_word_prevents_the_expansion() {
    let _serial = serial();
    let probe = MockForeground::new(window());
    let keys = leaked_keys();
    let hooks = HookPipe::new(keys, 8, false, nothing);
    let raw = Lane::new(8, nothing);
    // Control: hardware "btw" expands.
    let shared = engine("btw");
    let mut consumer = Consumer::default();
    for ch in "btw".chars() {
        assert!(raw_key(&shared, keys, &raw, &probe, ch));
    }
    assert_eq!(drain(&mut consumer, &shared, &hooks, &raw, &probe).len(), 1);
    // "bt", then an external injected X (hook event plus its RawInput
    // echo without a hardware source), then "w".
    let shared = engine("btw");
    let mut consumer = Consumer::default();
    assert!(raw_key(&shared, keys, &raw, &probe, 'b'));
    assert!(raw_key(&shared, keys, &raw, &probe, 't'));
    let before = shared.generation.load(Ordering::SeqCst);
    let x = injected(0x58);
    assert_eq!(
        handle_key(&hooks, &shared, &probe, WM_KEYDOWN, x),
        HookAction::Pass
    );
    assert!(
        !keys.held(0x58),
        "injected keys stay out of the physical mirror"
    );
    let echo = InputEvent::key_down(0x58, "x", false);
    assert!(raw_record(&shared, keys, &echo, &probe).is_none());
    assert_eq!(
        handle_key(&hooks, &shared, &probe, WM_KEYUP, x),
        HookAction::Pass
    );
    assert!(shared.generation.load(Ordering::SeqCst) > before);
    assert!(raw_key(&shared, keys, &raw, &probe, 'w'));
    assert!(drain(&mut consumer, &shared, &hooks, &raw, &probe).is_empty());
}

/// no-regression #4: our own tagged input is invisible to detection in both
/// normal and acceptance mode (no record, no mirror change, no generation
/// bump; its RawInput echo is not text), so it neither enters nor breaks
/// the word typed around it. External injected keys are still seen.
#[test]
fn our_own_tagged_input_never_enters_the_detection_buffer() {
    let _serial = serial();
    let probe = MockForeground::new(window());
    let keys = leaked_keys();
    let raw = Lane::new(8, nothing);
    let own = KeyInfo {
        extra: MARK,
        ..injected(0x58)
    };
    let click = MouseInfo {
        flags: LLMHF_INJECTED,
        extra: MARK,
        ..mouse(WM_LBUTTONDOWN, 1, 2)
    };
    let echo = InputEvent::key_down(0x58, "x", false);
    for test_mode in [false, true] {
        let shared = engine("btw");
        let hooks = HookPipe::new(keys, 8, test_mode, nothing);
        let mut consumer = Consumer::default();
        assert!(raw_key(&shared, keys, &raw, &probe, 'b'));
        assert!(raw_key(&shared, keys, &raw, &probe, 't'));
        let before = shared.generation.load(Ordering::SeqCst);
        assert_eq!(
            handle_key(&hooks, &shared, &probe, WM_KEYDOWN, own),
            HookAction::Pass
        );
        handle_mouse(&hooks, &shared, click);
        assert!(
            !keys.held(0x58) && !keys.held(VK_LBUTTON),
            "mode {test_mode}"
        );
        assert!(raw_record(&shared, keys, &echo, &probe).is_none());
        assert_eq!(
            handle_key(&hooks, &shared, &probe, WM_KEYUP, own),
            HookAction::Pass
        );
        assert_eq!(shared.generation.load(Ordering::SeqCst), before);
        assert!(hooks.lane().pop().is_none(), "nothing recorded");
        assert!(raw_key(&shared, keys, &raw, &probe, 'w'));
        assert_eq!(
            drain(&mut consumer, &shared, &hooks, &raw, &probe),
            [("btw".to_string(), before + 1)],
            "mode {test_mode}"
        );
        // The same key without our tag is recorded.
        handle_key(&hooks, &shared, &probe, WM_KEYDOWN, injected(0x58));
        let recorded = records(&hooks);
        if test_mode {
            assert!(matches!(
                recorded.as_slice(),
                [HookRecord::Key(KeyRecord {
                    vk: 0x58,
                    acceptance: Some(_),
                    ..
                })]
            ));
        } else {
            assert!(matches!(recorded.as_slice(), [HookRecord::External]));
        }
        assert!(shared.generation.load(Ordering::SeqCst) > before + 1);
    }
}

/// S-05: Ctrl goes down and its key-up is lost. Expansion is refused (a
/// chord is held) until the 30 s check on the fake clock finds Ctrl up in
/// the asynchronous key state and heals the mirror.
#[test]
fn a_lost_ctrl_key_up_heals_after_thirty_seconds() {
    let _serial = serial();
    let shared = engine("btw");
    let keys = leaked_keys();
    let hooks = HookPipe::new(keys, 16, false, nothing);
    let raw = Lane::new(16, nothing);
    let probe = MockForeground::new(window());
    let clock = FakeClock::new();
    let released = FakeAsyncKeys::default();
    let signals = SessionSignals::new();
    let mut monitor = Monitor::new(keys, &released, &clock, &signals, &NO_INJECTIONS);
    let mut consumer = Consumer::default();
    handle_key(&hooks, &shared, &probe, WM_KEYDOWN, hardware(VK_LCONTROL));
    assert!(drain(&mut consumer, &shared, &hooks, &raw, &probe).is_empty());
    assert_eq!(
        monitor.tick(&shared),
        Some(clock.now() + MODIFIER_VERIFY_AFTER)
    );
    type_word(&shared, keys, &raw, &probe, "btw");
    assert!(
        drain(&mut consumer, &shared, &hooks, &raw, &probe).is_empty(),
        "Ctrl+B, Ctrl+T, Ctrl+W are shortcuts, not text"
    );
    clock.advance(MODIFIER_VERIFY_AFTER - Duration::from_millis(1));
    monitor.tick(&shared);
    assert!(keys.held(VK_CONTROL));
    assert_eq!(released.reads(), 0, "not verified before 30 s");
    clock.advance(Duration::from_millis(1));
    let (next, lines) = capture::logs(|| monitor.tick(&shared));
    assert_eq!(next, None, "nothing left to watch");
    assert_eq!(
        count_of(&lines, "event=\"keystate.heal\" modifier=\"lctrl\""),
        1,
        "{lines:?}"
    );
    assert_eq!(released.reads(), 1, "only the stuck modifier is probed");
    assert!(!keys.held(VK_CONTROL));
    type_word(&shared, keys, &raw, &probe, "btw");
    assert_eq!(drain(&mut consumer, &shared, &hooks, &raw, &probe).len(), 1);
}

/// S-05: a modifier the user really holds is verified, kept and checked
/// again 30 s later; the probe is never consulted while nothing is held.
#[test]
fn a_ctrl_really_held_survives_the_check() {
    let _serial = serial();
    let shared = engine("btw");
    let keys = leaked_keys();
    let hooks = HookPipe::new(keys, 16, false, nothing);
    let probe = FixedProbe(context());
    let clock = FakeClock::new();
    let held = FakeAsyncKeys::with(&[VK_RCONTROL]);
    let signals = SessionSignals::new();
    let mut monitor = Monitor::new(keys, &held, &clock, &signals, &NO_INJECTIONS);
    assert_eq!(monitor.tick(&shared), None);
    handle_key(&hooks, &shared, &probe, WM_KEYDOWN, hardware(VK_RCONTROL));
    monitor.tick(&shared);
    clock.advance(MODIFIER_VERIFY_AFTER);
    assert_eq!(
        monitor.tick(&shared),
        Some(clock.now() + MODIFIER_VERIFY_AFTER)
    );
    assert!(keys.held(VK_CONTROL));
    assert_eq!(held.reads(), 1);
    handle_key(&hooks, &shared, &probe, WM_KEYUP, hardware(VK_RCONTROL));
    assert_eq!(monitor.tick(&shared), None);
    assert_eq!(held.reads(), 1, "a normal release needs no probe");
}

/// S-05: Win+L locks the session; the Win key-up happens on the secure
/// desktop and the hook never sees it. Unlock resyncs at once, so a
/// trigger works immediately instead of after the 30 s check. A second
/// resync 200 ms later only refreshes the mirror and does not break a word
/// typed across it.
#[test]
fn after_an_unlock_a_trigger_works_immediately() {
    let _serial = serial();
    let shared = engine("btw");
    let keys = leaked_keys();
    let hooks = HookPipe::new(keys, 16, false, nothing);
    let raw = Lane::new(16, nothing);
    let probe = MockForeground::new(window());
    let clock = FakeClock::new();
    let async_keys = FakeAsyncKeys::with(&[VK_LWIN]);
    let signals = SessionSignals::new();
    let mut monitor = Monitor::new(keys, &async_keys, &clock, &signals, &NO_INJECTIONS);
    let mut consumer = Consumer::default();
    handle_key(&hooks, &shared, &probe, WM_KEYDOWN, hardware(VK_LWIN));
    handle_key(&hooks, &shared, &probe, WM_KEYDOWN, hardware(0x4c));
    drain(&mut consumer, &shared, &hooks, &raw, &probe);
    monitor.tick(&shared);
    // Lock: Win is still physically down when the first resync runs.
    signals.raise(Signal::Lock);
    let before = shared.generation.load(Ordering::SeqCst);
    let (_, lines) = capture::logs(|| monitor.tick(&shared));
    assert_eq!(count_of(&lines, "signal=\"lock\""), 1, "{lines:?}");
    assert!(keys.held(VK_LWIN));
    assert_eq!(shared.generation.load(Ordering::SeqCst), before + 1);
    // Released on the secure desktop; unlock 10 s later (before any 30 s
    // check could help).
    async_keys.set(&[]);
    clock.advance(Duration::from_secs(10));
    signals.raise(Signal::Unlock);
    let (next, lines) = capture::logs(|| monitor.tick(&shared));
    assert_eq!(
        count_of(&lines, "event=\"session.signal\" signal=\"unlock\""),
        1
    );
    assert_eq!(
        count_of(&lines, "event=\"keystate.resync\" pass=\"first\""),
        1
    );
    assert_eq!(next, Some(clock.now() + RESYNC_SECOND_PASS));
    assert!(!keys.held(VK_LWIN) && !keys.held(0x4c));
    type_word(&shared, keys, &raw, &probe, "btw");
    assert_eq!(
        drain(&mut consumer, &shared, &hooks, &raw, &probe).len(),
        1,
        "the first word after the unlock expands"
    );
    // A word typed across the second pass still expands.
    type_word(&shared, keys, &raw, &probe, "bt");
    drain(&mut consumer, &shared, &hooks, &raw, &probe);
    clock.advance(RESYNC_SECOND_PASS - Duration::from_millis(1));
    let (_, lines) = capture::logs(|| monitor.tick(&shared));
    assert_eq!(count_of(&lines, "keystate.resync"), 0, "not yet");
    clock.advance(Duration::from_millis(1));
    let (next, lines) = capture::logs(|| monitor.tick(&shared));
    assert_eq!(
        count_of(&lines, "event=\"keystate.resync\" pass=\"second\""),
        1
    );
    assert_eq!(next, None);
    type_word(&shared, keys, &raw, &probe, "w");
    assert_eq!(drain(&mut consumer, &shared, &hooks, &raw, &probe).len(), 1);
}

/// The executor's clock in [`an_action_releases_its_key_across_the_second_resync`]:
/// every executor sleep advances the shared fake clock and then lets the
/// input consumer's monitor run, interleaving the two threads
/// deterministically.
struct ConsumerTicks<'a> {
    clock: &'a FakeClock,
    shared: &'a Shared,
    monitor: Mutex<Monitor<'a, FakeAsyncKeys, FakeClock>>,
}
impl Clock for ConsumerTicks<'_> {
    fn now(&self) -> Instant {
        self.clock.now()
    }
    fn wall_ms(&self) -> i64 {
        self.clock.wall_ms()
    }
    fn sleep(&self, duration: Duration) {
        self.clock.sleep(duration);
        self.monitor.lock().unwrap().tick(self.shared);
    }
}

/// H1: an action holds A (KeyDown, Delay, KeyUp) while the second resync
/// after an unlock runs. The asynchronous key state shows A down because we
/// injected it; the resync must not import it, so the KeyUp and the cleanup
/// really release A and the mirror never reports A as physical.
#[test]
fn an_action_releases_its_key_across_the_second_resync() {
    const A: u16 = 0x41;
    let own: &'static OwnInput = Box::leak(Box::new(OwnInput::new()));
    let _track = track_own_input(own);
    // `physical()` in the executor reads the global mirror; nothing in the
    // tests ever sets a bit there, and this test leaves it all up.
    PHYSICAL.mark_seeded();
    let shared = engine("btw");
    let clock = FakeClock::new();
    let async_keys = Arc::new(FakeAsyncKeys::default());
    let signals = SessionSignals::new();
    let injector = MockInjector::new();
    {
        // The asynchronous state follows our injected key.
        let async_keys = async_keys.clone();
        injector.on_call(move |call| {
            if let InjectCall::Key { key, up } = *call {
                let mut down = async_keys.down.lock().unwrap();
                down.retain(|&vk| vk != key);
                if !up {
                    down.push(key);
                }
            }
        });
    }
    let ticks = ConsumerTicks {
        clock: &clock,
        shared: &shared,
        monitor: Mutex::new(Monitor::new(&PHYSICAL, &async_keys, &clock, &signals, own)),
    };
    // Unlock: the first resync runs now, the second is due in 200 ms.
    signals.raise(Signal::Unlock);
    assert_eq!(
        ticks.monitor.lock().unwrap().tick(&shared),
        Some(clock.now() + RESYNC_SECOND_PASS)
    );
    let start = clock.now();
    let plan = ActionPlan {
        steps: [
            Operation::KeyDown {
                key: Key::named("A"),
            },
            Operation::Delay { ms: 1000 },
            Operation::KeyUp {
                key: Key::named("A"),
            },
        ]
        .into_iter()
        .map(|operation| Step::new(operation, SourceSpan::default()))
        .collect(),
        ..Default::default()
    };
    let foreground = MockForeground::new(window());
    let ports = Ports {
        injector: &injector,
        foreground: &foreground,
        clock: &ticks,
        clipboard: || -> Result<crate::mock::MockClipboard, String> { Err("unused".into()) },
    };
    let generation = shared.generation.load(Ordering::SeqCst);
    let (result, lines) = capture::logs(|| {
        execute_with_proof(&ports, &shared, plan, window(), generation, None, None, 1.0)
    });
    let result = result.unwrap();
    // The second resync really ran while A was held, after the quiet period
    // that followed the KeyDown.
    assert_eq!(
        count_of(&lines, "event=\"keystate.resync\" pass=\"second\""),
        1,
        "{lines:?}"
    );
    assert!(clock.now() - start >= RESYNC_SECOND_PASS + INJECTION_QUIET);
    assert_eq!(result.status, "unknown", "{}", result.message);
    assert_eq!(
        injector.calls(),
        [
            InjectCall::Key { key: A, up: false },
            InjectCall::Key { key: A, up: true },
        ],
        "the KeyUp released A"
    );
    assert!(!PHYSICAL.held(A), "our own A never became physical");
    assert!(!own.holds(A));
}

/// H1: a resync never samples while our injections are in flight or within
/// the quiet period after them (the asynchronous state lags behind a
/// key-up we sent), and a sample an injection overlapped is thrown away.
#[test]
fn a_resync_waits_for_our_injections_to_settle() {
    const V: u16 = 0x56;
    let shared = engine("btw");
    let keys = leaked_keys();
    let own: &'static OwnInput = Box::leak(Box::new(OwnInput::new()));
    let clock = FakeClock::new();
    let async_keys = InjectWhileProbed {
        keys: FakeAsyncKeys::default(),
        own,
        armed: AtomicBool::new(false),
    };
    let signals = SessionSignals::new();
    let mut monitor = Monitor::new(keys, &async_keys, &clock, &signals, own);
    // The executor is mid Ctrl+V when the session unlocks.
    let chord = own.begin();
    async_keys.keys.set(&[VK_LCONTROL, V]);
    signals.raise(Signal::Unlock);
    let (next, lines) = capture::logs(|| monitor.tick(&shared));
    assert_eq!(count_of(&lines, "keystate.resync"), 0, "{lines:?}");
    assert_eq!(next, Some(clock.now() + INJECTION_QUIET));
    assert_eq!(async_keys.keys.reads(), 0, "not even sampled");
    // The chord ends 10 ms later; the asynchronous state still shows it for
    // another 100 ms.
    clock.advance(Duration::from_millis(10));
    drop(chord);
    clock.advance(Duration::from_millis(10));
    let ended = clock.now();
    assert_eq!(monitor.tick(&shared), Some(ended + INJECTION_QUIET));
    clock.advance(Duration::from_millis(90));
    monitor.tick(&shared);
    async_keys.keys.set(&[]);
    clock.advance(INJECTION_QUIET - Duration::from_millis(91));
    monitor.tick(&shared);
    assert_eq!(async_keys.keys.reads(), 0, "still inside the quiet period");
    assert!(!keys.held(VK_CONTROL) && !keys.held(V));
    // Quiet at last, but another injection lands in the middle of the scan:
    // the sample is discarded and retried after a fresh quiet period.
    clock.advance(Duration::from_millis(1));
    async_keys.armed.store(true, Ordering::SeqCst);
    let (next, lines) = capture::logs(|| monitor.tick(&shared));
    assert_eq!(
        count_of(&lines, "event=\"keystate.resync_deferred\" pass=\"first\""),
        1,
        "{lines:?}"
    );
    assert_eq!(next, Some(clock.now() + INJECTION_QUIET));
    assert!(
        !keys.held(0x41),
        "the lagging A from that injection was not imported"
    );
    async_keys.keys.set(&[]);
    clock.advance(INJECTION_QUIET);
    let (next, lines) = capture::logs(|| monitor.tick(&shared));
    assert_eq!(
        count_of(&lines, "event=\"keystate.resync\" pass=\"first\""),
        1,
        "{lines:?}"
    );
    assert_eq!(next, Some(clock.now() + RESYNC_SECOND_PASS));
    assert!(!keys.held(VK_CONTROL) && !keys.held(V) && !keys.held(0x41));
}

/// Fake asynchronous state that, once armed, runs one of our injections
/// (A pressed and released) in the middle of a scan; A then reads down for
/// the rest of the scan, as the real state lags behind the key-up.
struct InjectWhileProbed {
    keys: FakeAsyncKeys,
    own: &'static OwnInput,
    armed: AtomicBool,
}
impl AsyncKeyProbe for InjectWhileProbed {
    fn down(&self, vk: u16) -> bool {
        if vk == 0x30 && self.armed.swap(false, Ordering::SeqCst) {
            let injecting = self.own.begin();
            injecting.pressed(0x41);
            injecting.released(0x41);
            self.keys.set(&[0x41]);
        }
        self.keys.down(vk)
    }
}

/// The hook kinds behind [`FakeSources::hooked`], in order.
const HOOK_KINDS: [HookSet; 3] = [HookSet::KEYBOARD, HookSet::MOUSE, HookSet::FOCUS];

/// Fake input-source system calls (B-06): which hooks are in place, the
/// detector window and the process RawInput registration. Records every
/// install, registration and rebuild; never touches Windows.
struct FakeSources {
    /// Hook kinds an install succeeds for.
    installable: Mutex<HookSet>,
    /// Keyboard, mouse and focus hook in place (and seeing input).
    hooked: Mutex<[bool; 3]>,
    alive: AtomicBool,
    registration: Mutex<Registration>,
    /// Whether `register_raw` takes the registration back.
    register_sticks: AtomicBool,
    /// Registered, but no key arrives (待实测: remote sessions).
    raw_muted: AtomicBool,
    installs: Mutex<Vec<HookSet>>,
    registers: AtomicUsize,
    rebuilds: AtomicUsize,
    /// The engine whose generation [`FakeSources::restored_at`] records.
    engine: OnceLock<Arc<Shared>>,
    restored: Mutex<Vec<u64>>,
}
impl FakeSources {
    /// What `rawinput::start` leaves: a live, registered detector window.
    fn new() -> Self {
        Self {
            installable: Mutex::new(HookSet::ALL),
            hooked: Mutex::new([false; 3]),
            alive: AtomicBool::new(true),
            registration: Mutex::new(Registration::Ours),
            register_sticks: AtomicBool::new(true),
            raw_muted: AtomicBool::new(false),
            installs: Mutex::new(Vec::new()),
            registers: AtomicUsize::new(0),
            rebuilds: AtomicUsize::new(0),
            engine: OnceLock::new(),
            restored: Mutex::new(Vec::new()),
        }
    }
    /// Records `shared`'s generation whenever delivery is restored.
    fn watch(&self, shared: &Arc<Shared>) {
        let _ = self.engine.set(shared.clone());
    }
    fn note_restored(&self) {
        if let Some(shared) = self.engine.get() {
            let generation = shared.generation.load(Ordering::SeqCst);
            self.restored.lock().unwrap().push(generation);
        }
    }
    /// The engine generation at each restore: a key captured that moment
    /// takes the next one.
    fn restored_at(&self) -> Vec<u64> {
        self.restored.lock().unwrap().clone()
    }
    fn hooked(&self, kind: HookSet) -> bool {
        HOOK_KINDS
            .iter()
            .zip(self.hooked.lock().unwrap().iter())
            .any(|(hooked, on)| *hooked == kind && *on)
    }
    /// Whether a physical key reaches the detector.
    fn raw_delivers(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
            && *self.registration.lock().unwrap() == Registration::Ours
            && !self.raw_muted.load(Ordering::SeqCst)
    }
    /// Whether both sources see physical keys.
    fn whole(&self) -> bool {
        self.hooked(HookSet::KEYBOARD) && self.raw_delivers()
    }
    fn set_installable(&self, which: HookSet) {
        *self.installable.lock().unwrap() = which;
    }
    fn set_registration(&self, registration: Registration) {
        *self.registration.lock().unwrap() = registration;
    }
    fn installs(&self) -> Vec<HookSet> {
        self.installs.lock().unwrap().clone()
    }
    fn registers(&self) -> usize {
        self.registers.load(Ordering::SeqCst)
    }
    fn rebuilds(&self) -> usize {
        self.rebuilds.load(Ordering::SeqCst)
    }
}
impl SourcePorts for FakeSources {
    fn install_hooks(&self, which: HookSet) -> HookSet {
        self.installs.lock().unwrap().push(which);
        let installed = which.and(*self.installable.lock().unwrap());
        let mut hooked = self.hooked.lock().unwrap();
        for (kind, on) in HOOK_KINDS.iter().zip(hooked.iter_mut()) {
            if which.contains(*kind) {
                *on = installed.contains(*kind);
            }
        }
        if installed != HookSet::NONE {
            self.note_restored();
        }
        installed
    }
    fn raw_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }
    fn raw_registration(&self) -> Registration {
        *self.registration.lock().unwrap()
    }
    fn register_raw(&self) -> bool {
        self.registers.fetch_add(1, Ordering::SeqCst);
        if !self.alive.load(Ordering::SeqCst) {
            return false;
        }
        if self.register_sticks.load(Ordering::SeqCst) {
            self.set_registration(Registration::Ours);
            self.note_restored();
        }
        true
    }
    fn rebuild_raw(&self) -> bool {
        self.rebuilds.fetch_add(1, Ordering::SeqCst);
        self.alive.store(true, Ordering::SeqCst);
        self.set_registration(Registration::Ours);
        self.note_restored();
        true
    }
    fn drop_hooks(&self) {
        let mut hooked = self.hooked.lock().unwrap();
        hooked[0] = false;
        hooked[1] = false;
    }
    fn unregister_raw(&self) {
        self.set_registration(Registration::Missing);
    }
    fn close_raw(&self) {
        self.alive.store(false, Ordering::SeqCst);
    }
}

/// Time between two physical keys in the B-06 tests.
const GAP: Duration = Duration::from_millis(50);

/// The consumer side of one engine for the B-06 tests: both lanes, the key
/// mirror and the consumer, fed by physical keys that reach only the
/// sources a [`FakeSources`] says work.
struct Pipeline {
    shared: Arc<Shared>,
    keys: &'static KeyBits,
    hooks: HookPipe<'static>,
    raw: Lane<RawRecord>,
    probe: FixedProbe,
    consumer: Consumer,
    /// Keys pressed so far; picks the next letter, so no key repeats.
    pressed: u32,
}
impl Pipeline {
    fn new() -> Self {
        let keys = leaked_keys();
        Self {
            shared: engine("btw"),
            keys,
            hooks: HookPipe::new(keys, 256, false, nothing),
            raw: Lane::new(256, nothing),
            probe: FixedProbe(context()),
            consumer: Consumer::default(),
            pressed: 0,
        }
    }
    /// One physical key (down and up), then one consumer pass.
    fn press(&mut self, fake: &FakeSources) {
        let ch = char::from(b'a' + (self.pressed % 26) as u8);
        self.pressed += 1;
        self.type_text(fake, &ch.to_string());
        self.drain();
    }
    /// Physical keys typing `text` (down and up each), captured by every
    /// source that works; not drained.
    fn type_text(&mut self, fake: &FakeSources, text: &str) {
        for ch in text.chars() {
            let (vk, text) = (ch.to_ascii_uppercase() as u16, ch.to_string());
            self.key(fake, vk, &text, true);
            self.key(fake, vk, &text, false);
        }
    }
    /// One physical key-down typing `text`, or key-up, of `vk`, captured
    /// by every source that works; not drained.
    fn key(&mut self, fake: &FakeSources, vk: u16, text: &str, down: bool) {
        if fake.hooked(HookSet::KEYBOARD) {
            let msg = if down { WM_KEYDOWN } else { WM_KEYUP };
            handle_key(&self.hooks, &self.shared, &self.probe, msg, hardware(vk));
        }
        if fake.raw_delivers() {
            let event = if down {
                InputEvent::key_down(vk, text, true)
            } else {
                InputEvent::key_up(vk, true)
            };
            let record = raw_record(&self.shared, self.keys, &event, &self.probe).unwrap();
            assert!(self.raw.push(record));
        }
    }
    fn drain(&mut self) {
        self.consumer.drain(
            &self.shared,
            &self.hooks,
            &self.raw,
            &self.probe,
            &mut |_| true,
        );
    }
    /// One consumer pass; the rule ids of the candidates it detected.
    fn candidates(&mut self) -> Vec<String> {
        let mut found = Vec::new();
        self.consumer.drain(
            &self.shared,
            &self.hooks,
            &self.raw,
            &self.probe,
            &mut |candidate: Candidate| {
                found.push(candidate.rule.id.clone());
                true
            },
        );
        found
    }
    /// One consumer pass in production order: every candidate goes through
    /// [`Monitor::admits`] first. The rule ids it admitted.
    fn admitted(&mut self, monitor: &mut Monitor<'_, FakeAsyncKeys, FakeClock>) -> Vec<String> {
        let mut admitted = Vec::new();
        let shared = &self.shared;
        self.consumer.drain(
            shared,
            &self.hooks,
            &self.raw,
            &self.probe,
            &mut |candidate: Candidate| {
                let ok = monitor.admits(shared, candidate.generation);
                if ok {
                    admitted.push(candidate.rule.id.clone());
                }
                ok
            },
        );
        admitted
    }
    fn generation(&self) -> u64 {
        self.shared.generation.load(Ordering::SeqCst)
    }
    fn tick(&mut self, liveness: &mut Liveness<'_, FakeSources, FakeClock>) -> Option<Instant> {
        liveness.tick(&self.shared, self.consumer.seen())
    }
    /// `count` physical keys [`GAP`] apart, ticking the cross-check after
    /// each.
    fn type_keys(
        &mut self,
        fake: &FakeSources,
        clock: &FakeClock,
        liveness: &mut Liveness<'_, FakeSources, FakeClock>,
        count: u32,
    ) {
        for _ in 0..count {
            self.press(fake);
            self.tick(liveness);
            clock.advance(GAP);
        }
    }
    /// Lets every pending sighting expire and ticks once.
    fn settle(
        &mut self,
        clock: &FakeClock,
        liveness: &mut Liveness<'_, FakeSources, FakeClock>,
    ) -> Option<Instant> {
        clock.advance(MATCH_WINDOW);
        self.tick(liveness)
    }
}
fn health_of(
    health: &[ComponentHealth],
    component: Component,
) -> (HealthState, Option<ReasonCode>) {
    let entry = health
        .iter()
        .find(|entry| entry.component == component)
        .unwrap();
    (entry.state, entry.reason)
}
fn line(text: &str) -> String {
    text.to_string()
}

/// S-06: Windows silently removes the keyboard hook. RawInput keeps seeing
/// the keys; after K of them go unmatched for N ms the hook is reinstalled
/// (well within 10 s and 50 keys), the mirror is resynced and the next
/// keys pair up again. The log carries no key content.
#[test]
fn a_silently_removed_keyboard_hook_is_reinstalled_after_k_unmatched_keys() {
    let _serial = serial();
    let (fake, clock, signals) = (FakeSources::new(), FakeClock::new(), SessionSignals::new());
    let sources = Sources::new(&fake, &clock, &signals);
    assert_eq!(sources.start(), HookSet::ALL);
    let mut liveness = Liveness::new(&sources);
    let mut pipe = Pipeline::new();
    pipe.type_keys(&fake, &clock, &mut liveness, 3);
    assert_eq!(
        pipe.settle(&clock, &mut liveness),
        None,
        "both sources agree"
    );
    fake.drop_hooks();
    let (lost_at, pressed) = (clock.elapsed(), pipe.pressed);
    let (_, lines) = capture::logs(|| {
        pipe.type_keys(&fake, &clock, &mut liveness, MISS_LIMIT - 1);
        pipe.settle(&clock, &mut liveness)
    });
    assert_eq!(count_of(&lines, "input."), 0, "{lines:?}");
    assert!(sources.hooks_ready(), "not detected yet");
    pipe.type_keys(&fake, &clock, &mut liveness, 1);
    let generation = pipe.shared.generation.load(Ordering::SeqCst);
    let (_, lines) = capture::logs(|| pipe.settle(&clock, &mut liveness));
    assert_eq!(
        lines,
        [
            line(r#"WARN event="input.check" source="keyboard_hook" result="lost""#),
            line(
                r#"WARN event="input.reattach" component="keyboard_hook" reason="hook_missing" ok=true"#
            ),
        ]
    );
    assert!(clock.elapsed() - lost_at < Duration::from_secs(10));
    assert!(pipe.pressed - pressed <= 50);
    assert_eq!(fake.installs(), [HookSet::ALL, HookSet::LOW_LEVEL]);
    assert!(fake.hooked(HookSet::KEYBOARD) && fake.hooked(HookSet::MOUSE));
    assert!(sources.hooks_ready());
    assert!(
        pipe.shared.generation.load(Ordering::SeqCst) > generation,
        "the keys around the loss are not one word"
    );
    assert_eq!(
        signals.take().collect::<Vec<_>>(),
        [Signal::InputReattached],
        "the key mirror is resynced"
    );
    let (_, lines) = capture::logs(|| {
        pipe.type_keys(&fake, &clock, &mut liveness, 3 * MISS_LIMIT);
        pipe.settle(&clock, &mut liveness)
    });
    assert_eq!(count_of(&lines, "input."), 0, "{lines:?}");
    assert_eq!(fake.installs().len(), 2);
}

/// The HookUnhook fault drops both low-level hooks the way Windows does;
/// the cross-check finds and reinstalls them.
#[cfg(feature = "fault-injection")]
#[test]
fn the_hook_unhook_fault_is_detected_and_repaired() {
    use crate::fault::{self, FaultPoint};
    let _serial = serial();
    let (fake, clock, signals) = (FakeSources::new(), FakeClock::new(), SessionSignals::new());
    let sources = Sources::new(&fake, &clock, &signals);
    sources.start();
    let mut liveness = Liveness::new(&sources);
    let mut pipe = Pipeline::new();
    fault::debug_inject_fault(FaultPoint::HookUnhook, 0);
    let (_, lines) = capture::logs(|| pipe.tick(&mut liveness));
    fault::disarm(FaultPoint::HookUnhook);
    assert_eq!(
        lines,
        [line(r#"WARN event="fault.injected" point="hook_unhook""#)]
    );
    assert!(!fake.hooked(HookSet::KEYBOARD) && !fake.hooked(HookSet::MOUSE));
    assert!(fake.hooked(HookSet::FOCUS));
    let (_, lines) = capture::logs(|| {
        pipe.type_keys(&fake, &clock, &mut liveness, MISS_LIMIT);
        pipe.settle(&clock, &mut liveness)
    });
    assert_eq!(
        count_of(
            &lines,
            r#"event="input.reattach" component="keyboard_hook" reason="hook_missing" ok=true"#
        ),
        1,
        "{lines:?}"
    );
    assert!(fake.hooked(HookSet::KEYBOARD) && sources.hooks_ready());
}

/// The registration is removed, or taken over by another window (tao, G6):
/// the hook keeps seeing keys RawInput never gets, and the registration is
/// checked and restored. When registering again does not stick, the
/// detector window is rebuilt.
#[test]
fn a_lost_raw_input_registration_is_restored() {
    let _serial = serial();
    let (fake, clock, signals) = (FakeSources::new(), FakeClock::new(), SessionSignals::new());
    let sources = Sources::new(&fake, &clock, &signals);
    sources.start();
    assert_eq!(fake.registers(), 0, "registration already ours");
    let mut liveness = Liveness::new(&sources);
    let mut pipe = Pipeline::new();
    fake.unregister_raw();
    let (_, lines) = capture::logs(|| {
        pipe.type_keys(&fake, &clock, &mut liveness, MISS_LIMIT);
        pipe.settle(&clock, &mut liveness)
    });
    assert_eq!(
        lines,
        [
            line(r#"WARN event="input.check" source="raw_input" result="unregistered""#),
            line(
                r#"WARN event="input.reattach" component="raw_input" reason="raw_input_missing" ok=true"#
            ),
        ]
    );
    assert_eq!((fake.registers(), fake.rebuilds()), (1, 0));
    assert_eq!(fake.raw_registration(), Registration::Ours);
    assert_eq!(
        health_of(&sources.health(), Component::RawInput),
        (HealthState::Healthy, None)
    );
    assert_eq!(
        signals.take().collect::<Vec<_>>(),
        [Signal::InputReattached]
    );
    // Both sources agree again, which clears the backoff.
    pipe.type_keys(&fake, &clock, &mut liveness, 2);
    pipe.settle(&clock, &mut liveness);
    // Another window takes the registration and keeps it.
    fake.set_registration(Registration::Elsewhere);
    fake.register_sticks.store(false, Ordering::SeqCst);
    let (_, lines) = capture::logs(|| {
        pipe.type_keys(&fake, &clock, &mut liveness, MISS_LIMIT);
        pipe.settle(&clock, &mut liveness)
    });
    assert_eq!(count_of(&lines, r#"result="unregistered""#), 1, "{lines:?}");
    assert_eq!(count_of(&lines, r#"component="raw_input""#), 1);
    assert_eq!((fake.registers(), fake.rebuilds()), (2, 1));
    assert!(fake.raw_delivers());
    assert_eq!(fake.installs(), [HookSet::ALL], "the hooks were left alone");
}

/// The RawinputUnregister fault removes the process registration.
#[cfg(feature = "fault-injection")]
#[test]
fn the_rawinput_unregister_fault_is_detected_and_repaired() {
    use crate::fault::{self, FaultPoint};
    let _serial = serial();
    let (fake, clock, signals) = (FakeSources::new(), FakeClock::new(), SessionSignals::new());
    let sources = Sources::new(&fake, &clock, &signals);
    sources.start();
    let mut liveness = Liveness::new(&sources);
    let mut pipe = Pipeline::new();
    fault::debug_inject_fault(FaultPoint::RawinputUnregister, 0);
    let (_, lines) = capture::logs(|| pipe.tick(&mut liveness));
    fault::disarm(FaultPoint::RawinputUnregister);
    assert_eq!(
        lines,
        [line(
            r#"WARN event="fault.injected" point="rawinput_unregister""#
        )]
    );
    assert_eq!(fake.raw_registration(), Registration::Missing);
    let (_, lines) = capture::logs(|| {
        pipe.type_keys(&fake, &clock, &mut liveness, MISS_LIMIT);
        pipe.settle(&clock, &mut liveness)
    });
    assert_eq!(
        count_of(
            &lines,
            r#"event="input.reattach" component="raw_input" reason="raw_input_missing" ok=true"#
        ),
        1,
        "{lines:?}"
    );
    assert!(fake.raw_delivers());
}

/// S-03: the detector window is closed from outside. Health reports
/// RawInput missing at once (no key needed); the cross-check only reports
/// the window gone and leaves the rebuild to the supervisor, whose
/// `reattach(RawInput)` builds a new detector.
#[test]
fn a_closed_detector_window_is_reported_and_rebuilt_on_reattach() {
    let _serial = serial();
    let (fake, clock, signals) = (FakeSources::new(), FakeClock::new(), SessionSignals::new());
    let sources = Sources::new(&fake, &clock, &signals);
    sources.start();
    let mut liveness = Liveness::new(&sources);
    let mut pipe = Pipeline::new();
    fake.close_raw();
    assert_eq!(
        health_of(&sources.health(), Component::RawInput),
        (HealthState::Recovering, Some(ReasonCode::RawInputMissing))
    );
    let (_, lines) = capture::logs(|| {
        pipe.type_keys(&fake, &clock, &mut liveness, MISS_LIMIT);
        pipe.settle(&clock, &mut liveness)
    });
    assert_eq!(
        lines,
        [line(
            r#"WARN event="input.check" source="raw_input" result="window_missing""#
        )]
    );
    assert_eq!((fake.registers(), fake.rebuilds()), (0, 0));
    let (result, lines) = capture::logs(|| sources.reattach(Component::RawInput));
    assert_eq!(result, Ok(()));
    assert_eq!(
        lines,
        [line(
            r#"WARN event="input.reattach" component="raw_input" reason="raw_input_missing" ok=true"#
        )]
    );
    assert_eq!(fake.rebuilds(), 1);
    assert_eq!(
        health_of(&sources.health(), Component::RawInput),
        (HealthState::Healthy, None)
    );
    assert!(fake.raw_delivers());
}

/// The RawinputClose fault closes the window as an outside WM_CLOSE would;
/// the detector thread's report (`raw_lost`) marks RawInput missing.
#[cfg(feature = "fault-injection")]
#[test]
fn the_rawinput_close_fault_is_reported_and_rebuilt() {
    use crate::fault::{self, FaultPoint};
    let _serial = serial();
    let (fake, clock, signals) = (FakeSources::new(), FakeClock::new(), SessionSignals::new());
    let sources = Sources::new(&fake, &clock, &signals);
    sources.start();
    let mut liveness = Liveness::new(&sources);
    let mut pipe = Pipeline::new();
    fault::debug_inject_fault(FaultPoint::RawinputClose, 0);
    let (_, lines) = capture::logs(|| pipe.tick(&mut liveness));
    fault::disarm(FaultPoint::RawinputClose);
    assert_eq!(
        lines,
        [line(
            r#"WARN event="fault.injected" point="rawinput_close""#
        )]
    );
    assert!(!fake.raw_alive());
    sources.raw_lost();
    assert_eq!(
        health_of(&sources.health(), Component::RawInput),
        (HealthState::Recovering, Some(ReasonCode::RawInputMissing))
    );
    assert_eq!(sources.reattach(Component::RawInput), Ok(()));
    assert_eq!(fake.rebuilds(), 1);
    assert!(fake.raw_delivers());
}

/// Normal typing: autorepeat that only one source reports and sided versus
/// generic modifier codes still pair up; nothing is reattached.
#[test]
fn keys_both_sources_see_never_trigger_a_reattach() {
    let _serial = serial();
    let (fake, clock, signals) = (FakeSources::new(), FakeClock::new(), SessionSignals::new());
    let sources = Sources::new(&fake, &clock, &signals);
    sources.start();
    let mut liveness = Liveness::new(&sources);
    let mut pipe = Pipeline::new();
    let (_, lines) = capture::logs(|| {
        let p = &mut pipe;
        for _ in 0..3 {
            // Shift held with autorepeat: the hook reports LShift three
            // times, RawInput generic Shift once.
            handle_key(
                &p.hooks,
                &p.shared,
                &p.probe,
                WM_KEYDOWN,
                hardware(VK_LSHIFT),
            );
        }
        let shift = InputEvent::key_down(VK_SHIFT, "", true);
        assert!(p
            .raw
            .push(raw_record(&p.shared, p.keys, &shift, &p.probe).unwrap()));
        handle_key(&p.hooks, &p.shared, &p.probe, WM_KEYUP, hardware(VK_LSHIFT));
        p.drain();
        p.tick(&mut liveness);
        p.type_keys(&fake, &clock, &mut liveness, 4 * MISS_LIMIT);
        p.settle(&clock, &mut liveness)
    });
    assert_eq!(count_of(&lines, "input."), 0, "{lines:?}");
    assert_eq!(fake.installs(), [HookSet::ALL]);
    assert_eq!((fake.registers(), fake.rebuilds()), (0, 0));
    assert_eq!(signals.take().count(), 0);
}

/// Breaks a source the way the HookUnhook or RawinputUnregister fault does:
/// through the fault point when it is compiled in, through the port it
/// calls otherwise.
fn break_source(
    pipe: &mut Pipeline,
    liveness: &mut Liveness<'_, FakeSources, FakeClock>,
    fake: &FakeSources,
    point: crate::fault::FaultPoint,
) {
    use crate::fault::FaultPoint;
    #[cfg(feature = "fault-injection")]
    {
        crate::fault::debug_inject_fault(point, 0);
        pipe.tick(liveness);
        crate::fault::disarm(point);
    }
    #[cfg(not(feature = "fault-injection"))]
    {
        let _ = (pipe, liveness);
        match point {
            FaultPoint::HookUnhook => fake.drop_hooks(),
            FaultPoint::RawinputUnregister => fake.unregister_raw(),
            _ => unreachable!("not a source fault"),
        }
    }
    let broken = match point {
        FaultPoint::HookUnhook => !fake.hooked(HookSet::KEYBOARD) && fake.raw_delivers(),
        _ => fake.hooked(HookSet::KEYBOARD) && !fake.raw_delivers(),
    };
    assert!(broken, "{point:?}");
}
/// The source faults H4 is tested under, with the reattach each one ends in.
const SOURCE_FAULTS: [(crate::fault::FaultPoint, &str); 2] = [
    (
        crate::fault::FaultPoint::HookUnhook,
        r#"event="input.reattach" component="keyboard_hook" reason="hook_missing" ok=true"#,
    ),
    (
        crate::fault::FaultPoint::RawinputUnregister,
        r#"event="input.reattach" component="raw_input" reason="raw_input_missing" ok=true"#,
    ),
];

/// H4, S-06: after the HookUnhook or RawinputUnregister fault, the user
/// presses Backspace fifty times. Each press is a down and an up on the
/// source that still works, so each one counts: the lost source is found
/// and repaired after K presses plus one match window, well within fifty
/// keys and 10 s, and nothing more is reattached for the rest.
#[test]
fn fifty_presses_of_one_key_find_the_lost_source() {
    let _serial = serial();
    for (point, reattach) in SOURCE_FAULTS {
        let (fake, clock, signals) = (FakeSources::new(), FakeClock::new(), SessionSignals::new());
        let sources = Sources::new(&fake, &clock, &signals);
        sources.start();
        let mut liveness = Liveness::new(&sources);
        let mut pipe = Pipeline::new();
        break_source(&mut pipe, &mut liveness, &fake, point);
        let (repaired, lines) = capture::logs(|| {
            let mut repaired = None;
            for press in 1..=50u32 {
                pipe.key(&fake, VK_BACK, "\u{8}", true);
                pipe.key(&fake, VK_BACK, "\u{8}", false);
                pipe.drain();
                pipe.tick(&mut liveness);
                if repaired.is_none() && fake.whole() {
                    repaired = Some((press, clock.elapsed()));
                }
                clock.advance(GAP);
            }
            repaired
        });
        assert_eq!(count_of(&lines, reattach), 1, "{point:?} {lines:?}");
        assert_eq!(count_of(&lines, "input.reattach"), 1, "{point:?} {lines:?}");
        let (press, elapsed) = repaired.unwrap();
        let window = (MATCH_WINDOW.as_millis() / GAP.as_millis()) as u32;
        assert!(press <= MISS_LIMIT + window, "{point:?}: press {press}");
        assert!(elapsed < Duration::from_secs(10));
    }
}

/// Autorepeat interval of a held key (30 per second).
const REPEAT_GAP: Duration = Duration::from_millis(33);

/// H4: a key held down with autorepeat. While both sources work its
/// repeats merge the same way on each side and pair up, so nothing is
/// reattached however long it is held. After the HookUnhook or
/// RawinputUnregister fault, holding it alone (its up comes only at the
/// end) still finds the lost source within fifty repeats: every
/// [`REPEAT_RUN`] downs with no up count as one press.
#[test]
fn a_held_autorepeating_key_pairs_up_and_still_finds_a_lost_source() {
    let _serial = serial();
    for (point, reattach) in SOURCE_FAULTS {
        let (fake, clock, signals) = (FakeSources::new(), FakeClock::new(), SessionSignals::new());
        let sources = Sources::new(&fake, &clock, &signals);
        sources.start();
        let mut liveness = Liveness::new(&sources);
        let mut pipe = Pipeline::new();
        let hold = |pipe: &mut Pipeline,
                    liveness: &mut Liveness<'_, FakeSources, FakeClock>,
                    downs: u32| {
            let mut repaired = None;
            for down in 1..=downs {
                let broken = !fake.whole();
                pipe.key(&fake, VK_BACK, "\u{8}", true);
                pipe.drain();
                pipe.tick(liveness);
                if broken && repaired.is_none() && fake.whole() {
                    repaired = Some(down);
                }
                clock.advance(REPEAT_GAP);
            }
            pipe.key(&fake, VK_BACK, "", false);
            pipe.drain();
            pipe.tick(liveness);
            repaired
        };
        let (_, lines) = capture::logs(|| {
            hold(&mut pipe, &mut liveness, 50 * REPEAT_RUN);
            pipe.settle(&clock, &mut liveness)
        });
        assert_eq!(count_of(&lines, "input."), 0, "{point:?} {lines:?}");
        break_source(&mut pipe, &mut liveness, &fake, point);
        let (repaired, lines) = capture::logs(|| {
            let repaired = hold(&mut pipe, &mut liveness, 50);
            pipe.settle(&clock, &mut liveness);
            repaired
        });
        assert_eq!(count_of(&lines, reattach), 1, "{point:?} {lines:?}");
        assert_eq!(count_of(&lines, "input.reattach"), 1, "{point:?} {lines:?}");
        assert!(repaired.is_some_and(|down| down <= 50), "{point:?}");
        assert!(fake.whole());
    }
}

/// no-regression #4: our own tagged input reaches neither source as a
/// physical key (the hook skips it, its RawInput echo has no device), and
/// neither does input another program injects; no amount of it is a
/// verdict against either source.
#[test]
fn injected_keys_are_invisible_to_the_cross_check() {
    let _serial = serial();
    let (fake, clock, signals) = (FakeSources::new(), FakeClock::new(), SessionSignals::new());
    let sources = Sources::new(&fake, &clock, &signals);
    sources.start();
    let mut liveness = Liveness::new(&sources);
    let mut pipe = Pipeline::new();
    let own = |vk| KeyInfo {
        extra: MARK,
        ..injected(vk)
    };
    let (_, lines) = capture::logs(|| {
        for i in 0..4 * MISS_LIMIT as u16 {
            let vk = 0x41 + i % 26;
            for key in [own(vk), injected(vk)] {
                let p = &mut pipe;
                handle_key(&p.hooks, &p.shared, &p.probe, WM_KEYDOWN, key);
                let echo = InputEvent::key_down(vk, "x", false);
                assert!(raw_record(&p.shared, p.keys, &echo, &p.probe).is_none());
                handle_key(&p.hooks, &p.shared, &p.probe, WM_KEYUP, key);
            }
            pipe.drain();
            assert!(pipe.consumer.seen().is_empty());
            pipe.tick(&mut liveness);
            clock.advance(GAP);
        }
        pipe.settle(&clock, &mut liveness)
    });
    assert_eq!(count_of(&lines, "input."), 0, "{lines:?}");
    assert_eq!(fake.installs(), [HookSet::ALL]);
}

/// `reattach` before the engine starts and for components that are not
/// input sources is refused without a system call; a component that is
/// already healthy is left alone.
#[test]
fn reattach_refuses_unstarted_sources_and_other_components() {
    let _serial = serial();
    let (fake, clock, signals) = (FakeSources::new(), FakeClock::new(), SessionSignals::new());
    let sources = Sources::new(&fake, &clock, &signals);
    assert!(sources.reattach(Component::KeyboardHook).is_err());
    assert!(sources
        .health()
        .iter()
        .all(|entry| entry.state == HealthState::Starting));
    assert!(fake.installs().is_empty());
    assert!(!sources.hooks_ready());
    sources.start();
    assert!(sources.hooks_ready());
    assert!(sources.reattach(Component::Executor).is_err());
    let (result, lines) = capture::logs(|| sources.reattach(Component::MouseHook));
    assert_eq!(result, Ok(()));
    assert!(lines.is_empty(), "{lines:?}");
    assert_eq!(fake.installs(), [HookSet::ALL], "a healthy hook stays");
    assert_eq!(signals.take().count(), 0);
}

/// The reinstall itself fails: both low-level hook components stay
/// recovering, injection stays refused (`hooks_ready`), the cross-check
/// backs off (2K keys for the next attempt) and the supervisor's retry
/// restores the hooks once installing works again.
#[test]
fn a_failed_reinstall_backs_off_and_a_later_retry_restores_the_hooks() {
    let _serial = serial();
    let (fake, clock, signals) = (FakeSources::new(), FakeClock::new(), SessionSignals::new());
    let sources = Sources::new(&fake, &clock, &signals);
    sources.start();
    let mut liveness = Liveness::new(&sources);
    let mut pipe = Pipeline::new();
    fake.drop_hooks();
    fake.set_installable(HookSet::FOCUS);
    let (_, lines) = capture::logs(|| {
        pipe.type_keys(&fake, &clock, &mut liveness, MISS_LIMIT);
        pipe.settle(&clock, &mut liveness)
    });
    assert_eq!(
        count_of(
            &lines,
            r#"event="input.reattach" component="keyboard_hook" reason="hook_missing" ok=false"#
        ),
        1,
        "{lines:?}"
    );
    let health = sources.health();
    for component in [Component::KeyboardHook, Component::MouseHook] {
        assert_eq!(
            health_of(&health, component),
            (HealthState::Recovering, Some(ReasonCode::HookInstallFailed))
        );
    }
    assert!(!sources.hooks_ready(), "injection is refused");
    assert_eq!(signals.take().count(), 0);
    // K more unmatched keys: not yet (the next verdict needs 2K).
    let (_, lines) = capture::logs(|| {
        pipe.type_keys(&fake, &clock, &mut liveness, MISS_LIMIT);
        pipe.settle(&clock, &mut liveness)
    });
    assert_eq!(count_of(&lines, "input."), 0, "{lines:?}");
    let (_, lines) = capture::logs(|| {
        pipe.type_keys(&fake, &clock, &mut liveness, MISS_LIMIT);
        pipe.settle(&clock, &mut liveness)
    });
    assert_eq!(count_of(&lines, r#"ok=false"#), 1, "{lines:?}");
    assert_eq!(fake.installs().len(), 3);
    // The supervisor retries once installing works again.
    fake.set_installable(HookSet::ALL);
    let (result, lines) = capture::logs(|| sources.reattach(Component::KeyboardHook));
    assert_eq!(result, Ok(()));
    assert_eq!(
        lines,
        [line(
            r#"WARN event="input.reattach" component="keyboard_hook" reason="hook_install_failed" ok=true"#
        )]
    );
    assert!(sources.hooks_ready());
    assert_eq!(sources.reattach(Component::MouseHook), Ok(()));
    assert_eq!(fake.installs().len(), 4, "the mouse hook came back with it");
}

/// A hook that fails at startup is reported on its own component; the
/// others stay installed, and reattaching it installs only that hook.
#[test]
fn a_partial_startup_install_is_reported_per_component() {
    let _serial = serial();
    let (fake, clock, signals) = (FakeSources::new(), FakeClock::new(), SessionSignals::new());
    let sources = Sources::new(&fake, &clock, &signals);
    fake.set_installable(HookSet::LOW_LEVEL);
    assert_eq!(sources.start(), HookSet::LOW_LEVEL);
    let health = sources.health();
    assert_eq!(
        health_of(&health, Component::KeyboardHook),
        (HealthState::Healthy, None)
    );
    assert_eq!(
        health_of(&health, Component::FocusEvents),
        (HealthState::Recovering, Some(ReasonCode::HookInstallFailed))
    );
    assert_eq!(
        health_of(&health, Component::RawInput),
        (HealthState::Healthy, None)
    );
    assert!(!sources.hooks_ready());
    fake.set_installable(HookSet::ALL);
    let (result, lines) = capture::logs(|| sources.reattach(Component::FocusEvents));
    assert_eq!(result, Ok(()));
    assert_eq!(
        lines,
        [line(
            r#"WARN event="input.reattach" component="focus_events" reason="hook_install_failed" ok=true"#
        )]
    );
    assert_eq!(fake.installs(), [HookSet::ALL, HookSet::FOCUS]);
    assert!(sources.hooks_ready());
}

/// G6: tao registers RawInput for its own window when its event loop is
/// created and the last registration wins; startup takes it back once the
/// detector runs.
#[test]
fn startup_takes_back_a_raw_input_registration_another_window_made() {
    let _serial = serial();
    let (fake, clock, signals) = (FakeSources::new(), FakeClock::new(), SessionSignals::new());
    let sources = Sources::new(&fake, &clock, &signals);
    fake.set_registration(Registration::Elsewhere);
    let (_, lines) = capture::logs(|| sources.start());
    assert_eq!(
        lines,
        [line(
            r#"WARN event="input.reattach" component="raw_input" reason="raw_input_missing" ok=true"#
        )]
    );
    assert_eq!(fake.registers(), 1);
    assert_eq!(fake.raw_registration(), Registration::Ours);
    assert_eq!(
        health_of(&sources.health(), Component::RawInput),
        (HealthState::Healthy, None)
    );
}

/// The other window keeps the registration at startup: RawInput is reported
/// missing, and the supervisor's reattach rebuilds the detector.
#[test]
fn a_registration_startup_cannot_take_back_is_reported_missing() {
    let _serial = serial();
    let (fake, clock, signals) = (FakeSources::new(), FakeClock::new(), SessionSignals::new());
    let sources = Sources::new(&fake, &clock, &signals);
    fake.set_registration(Registration::Elsewhere);
    fake.register_sticks.store(false, Ordering::SeqCst);
    let (_, lines) = capture::logs(|| sources.start());
    assert_eq!(
        lines,
        [line(
            r#"WARN event="input.reattach" component="raw_input" reason="raw_input_missing" ok=false"#
        )]
    );
    assert_eq!(
        health_of(&sources.health(), Component::RawInput),
        (HealthState::Recovering, Some(ReasonCode::RawInputMissing))
    );
    assert!(sources.hooks_ready(), "the hooks do not depend on RawInput");
    assert_eq!(sources.reattach(Component::RawInput), Ok(()));
    assert_eq!((fake.registers(), fake.rebuilds()), (2, 1));
    assert_eq!(
        health_of(&sources.health(), Component::RawInput),
        (HealthState::Healthy, None)
    );
}

/// RawInput registered to our window but silent (待实测: a remote session
/// whose keys carry no device handle): only logged, nothing reattached,
/// and logged ever more rarely while it lasts.
#[test]
fn a_silent_but_registered_raw_input_is_only_logged() {
    let _serial = serial();
    let (fake, clock, signals) = (FakeSources::new(), FakeClock::new(), SessionSignals::new());
    let sources = Sources::new(&fake, &clock, &signals);
    sources.start();
    let mut liveness = Liveness::new(&sources);
    let mut pipe = Pipeline::new();
    fake.raw_muted.store(true, Ordering::SeqCst);
    let (_, lines) = capture::logs(|| {
        pipe.type_keys(&fake, &clock, &mut liveness, MISS_LIMIT);
        pipe.settle(&clock, &mut liveness)
    });
    assert_eq!(
        lines,
        [line(
            r#"INFO event="input.check" source="raw_input" result="registered""#
        )]
    );
    let (_, lines) = capture::logs(|| {
        pipe.type_keys(&fake, &clock, &mut liveness, MISS_LIMIT);
        pipe.settle(&clock, &mut liveness)
    });
    assert!(lines.is_empty(), "{lines:?}");
    assert_eq!((fake.registers(), fake.rebuilds()), (0, 0));
    assert_eq!(
        health_of(&sources.health(), Component::RawInput),
        (HealthState::Healthy, None)
    );
}

/// RawInput runs before the hooks are installed; keys it sees then are no
/// evidence against the hook.
#[test]
fn keys_before_start_are_not_held_against_the_hooks() {
    let _serial = serial();
    let (fake, clock, signals) = (FakeSources::new(), FakeClock::new(), SessionSignals::new());
    let sources = Sources::new(&fake, &clock, &signals);
    let mut liveness = Liveness::new(&sources);
    let mut pipe = Pipeline::new();
    let (_, lines) = capture::logs(|| {
        pipe.type_keys(&fake, &clock, &mut liveness, 4 * MISS_LIMIT);
        pipe.settle(&clock, &mut liveness)
    });
    assert_eq!(count_of(&lines, "input."), 0, "{lines:?}");
    assert!(fake.installs().is_empty());
    sources.start();
    assert_eq!(pipe.settle(&clock, &mut liveness), None);
    assert!(sources.hooks_ready());
}

/// H3 (no-regression #3): "bt" is buffered, RawInput's registration is
/// taken away and typing goes on (only the hook sees it), the cross-check
/// restores the registration, and a "w" is captured before the consumer
/// runs again. The prefix was invalidated when the loss was found and
/// before delivery came back, so "btw" never matches across the gap.
#[test]
fn a_prefix_from_before_a_raw_input_loss_never_completes_after_it() {
    let _serial = serial();
    let (fake, clock, signals) = (FakeSources::new(), FakeClock::new(), SessionSignals::new());
    let sources = Sources::new(&fake, &clock, &signals);
    sources.start();
    let mut liveness = Liveness::new(&sources);
    let mut pipe = Pipeline::new();
    sources.bind(&pipe.shared);
    fake.watch(&pipe.shared);
    // Control: without a loss the rule matches.
    pipe.type_text(&fake, "btw");
    assert_eq!(pipe.candidates(), ["btw"]);
    pipe.tick(&mut liveness);
    pipe.type_text(&fake, "bt");
    assert!(pipe.candidates().is_empty());
    assert_eq!(pipe.tick(&mut liveness), None, "both sources agree");
    let prefix = pipe.generation();
    fake.unregister_raw();
    for ch in "cdefg".chars().take(MISS_LIMIT as usize) {
        pipe.type_text(&fake, &ch.to_string());
        assert!(pipe.candidates().is_empty());
        pipe.tick(&mut liveness);
        clock.advance(GAP);
    }
    assert_eq!(
        pipe.generation(),
        prefix,
        "the hook alone never ends a word"
    );
    let (_, lines) = capture::logs(|| pipe.settle(&clock, &mut liveness));
    assert_eq!(
        count_of(
            &lines,
            r#"event="input.reattach" component="raw_input" reason="raw_input_missing" ok=true"#
        ),
        1,
        "{lines:?}"
    );
    // A key captured the moment delivery came back already leaves a gap.
    let restored = fake.restored_at();
    assert_eq!(restored.len(), 1);
    assert!(restored[0] > prefix, "{restored:?} {prefix}");
    assert!(
        signals.pending(),
        "the consumer has not handled the reattach"
    );
    pipe.type_text(&fake, "w");
    assert!(pipe.candidates().is_empty(), "no candidate across the loss");
    pipe.type_text(&fake, "btw");
    assert_eq!(pipe.candidates(), ["btw"], "a whole new word still works");
}

/// H3: the detector window disappears (seen by the supervisor's health
/// poll, then reported by the detector thread) and the supervisor rebuilds
/// it on its own thread. Every step invalidates the prefix synchronously,
/// the rebuild before the new window delivers; and the consumer handles the
/// reattach signal before it admits any candidate.
#[test]
fn a_rebuilt_detector_never_completes_an_old_prefix_and_its_signal_comes_first() {
    let _serial = serial();
    let (fake, clock, signals) = (FakeSources::new(), FakeClock::new(), SessionSignals::new());
    let sources = Sources::new(&fake, &clock, &signals);
    sources.start();
    let mut pipe = Pipeline::new();
    sources.bind(&pipe.shared);
    fake.watch(&pipe.shared);
    let async_keys = FakeAsyncKeys::default();
    let mut monitor = Monitor::new(pipe.keys, &async_keys, &clock, &signals, &NO_INJECTIONS);
    pipe.type_text(&fake, "bt");
    assert!(pipe.admitted(&mut monitor).is_empty());
    let prefix = pipe.generation();
    fake.close_raw();
    assert_eq!(
        health_of(&sources.health(), Component::RawInput),
        (HealthState::Recovering, Some(ReasonCode::RawInputMissing))
    );
    let polled = pipe.generation();
    assert!(polled > prefix, "invalidated when the poll found it");
    sources.raw_lost();
    let reported = pipe.generation();
    assert!(reported > polled, "and when the detector reported it");
    pipe.type_text(&fake, "x");
    assert!(pipe.admitted(&mut monitor).is_empty());
    assert_eq!(sources.reattach(Component::RawInput), Ok(()));
    assert_eq!(fake.rebuilds(), 1);
    let restored = fake.restored_at();
    assert_eq!(restored.len(), 1);
    assert!(restored[0] > reported, "invalidated before the rebuild");
    // "w" is captured after the rebuild, before the consumer runs.
    pipe.type_text(&fake, "w");
    assert!(pipe.admitted(&mut monitor).is_empty());
    assert!(
        signals.pending(),
        "no candidate, so nothing consumed it yet"
    );
    // A whole word typed before the consumer saw the signal: the signal is
    // handled first and the word does not cross it.
    pipe.type_text(&fake, "btw");
    let (admitted, lines) = capture::logs(|| pipe.admitted(&mut monitor));
    assert!(admitted.is_empty());
    assert!(!signals.pending());
    assert_eq!(
        count_of(
            &lines,
            r#"event="session.signal" signal="input_reattached""#
        ),
        1,
        "{lines:?}"
    );
    assert_eq!(
        count_of(&lines, r#"event="keystate.resync" pass="first""#),
        1
    );
    pipe.type_text(&fake, "btw");
    assert_eq!(pipe.admitted(&mut monitor), ["btw"]);
}

/// H3: a hook component reattached by the supervisor invalidates the
/// prefix before it is installed again (focus changes went unseen).
#[test]
fn a_reattached_hook_invalidates_the_prefix_before_it_is_back() {
    let _serial = serial();
    let (fake, clock, signals) = (FakeSources::new(), FakeClock::new(), SessionSignals::new());
    let sources = Sources::new(&fake, &clock, &signals);
    fake.set_installable(HookSet::LOW_LEVEL);
    sources.start();
    let mut pipe = Pipeline::new();
    sources.bind(&pipe.shared);
    fake.watch(&pipe.shared);
    pipe.type_text(&fake, "bt");
    assert!(pipe.candidates().is_empty());
    let prefix = pipe.generation();
    fake.set_installable(HookSet::ALL);
    assert_eq!(sources.reattach(Component::FocusEvents), Ok(()));
    let restored = fake.restored_at();
    assert_eq!(restored.len(), 1);
    assert!(restored[0] > prefix, "{restored:?} {prefix}");
    pipe.type_text(&fake, "w");
    assert!(pipe.candidates().is_empty());
}

/// A session-end callback that records `name` when it runs.
fn recorder(
    ran: &Arc<Mutex<Vec<&'static str>>>,
    name: &'static str,
) -> Box<dyn Fn() + Send + Sync> {
    let ran = ran.clone();
    Box::new(move || ran.lock().unwrap().push(name))
}

/// B-04: WM_QUERYENDSESSION is answered TRUE at once and runs nothing, a
/// cancelled shutdown (WM_ENDSESSION with wParam FALSE) runs nothing, and
/// the real end runs every callback once, in registration order.
#[test]
fn the_session_end_runs_callbacks_once_and_never_vetoes_a_shutdown() {
    let end = SessionEnd::new();
    let ran = Arc::new(Mutex::new(Vec::new()));
    end.register(recorder(&ran, "first"));
    end.register(recorder(&ran, "second"));
    let (answers, lines) = capture::logs(|| {
        [
            end_message(&end, WM_QUERYENDSESSION, 0, "system"),
            end_message(&end, WM_ENDSESSION, 0, "system"),
        ]
    });
    assert_eq!(answers, [Some(1), Some(0)]);
    assert!(lines.is_empty(), "{lines:?}");
    assert!(ran.lock().unwrap().is_empty());
    let (answers, lines) = capture::logs(|| {
        [
            end_message(&end, WM_QUERYENDSESSION, 0, "system"),
            end_message(&end, WM_ENDSESSION, 1, "system"),
            end_message(&end, WM_ENDSESSION, 1, "system"),
        ]
    });
    assert_eq!(answers, [Some(1), Some(0), Some(0)]);
    assert_eq!(
        lines,
        [line(
            r#"INFO event="session.end" source="system" callbacks=2"#
        )]
    );
    assert_eq!(*ran.lock().unwrap(), ["first", "second"]);
    // A callback registered after the end never runs.
    end.register(recorder(&ran, "late"));
    assert!(!end.fire("system"));
    assert_eq!(*ran.lock().unwrap(), ["first", "second"]);
}

/// Every other message goes on to the resync signals and the default
/// window procedure.
#[test]
fn other_messages_are_not_session_end_messages() {
    let end = SessionEnd::new();
    let ran = Arc::new(Mutex::new(Vec::new()));
    end.register(recorder(&ran, "only"));
    for (msg, wparam) in [
        (WM_POWERBROADCAST, PBT_APMSUSPEND as usize),
        (WM_POWERBROADCAST, PBT_APMRESUMESUSPEND as usize),
        (WM_WTSSESSION_CHANGE, WTS_SESSION_LOGOFF as usize),
        (WM_CLOSE, 1),
        (WM_DESTROY, 1),
    ] {
        assert_eq!(end_message(&end, msg, wparam, "system"), None);
    }
    assert!(ran.lock().unwrap().is_empty());
}

/// A panicking callback is logged and the rest still run; a callback that
/// registers another one does not deadlock.
#[test]
fn a_panicking_or_registering_callback_does_not_stop_the_others() {
    static END: SessionEnd = SessionEnd::new();
    let ran = Arc::new(Mutex::new(Vec::new()));
    END.register(Box::new(|| panic!("session-end callback failed")));
    END.register(Box::new(|| END.register(Box::new(nothing))));
    END.register(recorder(&ran, "last"));
    let (fired, lines) = capture::logs(|| END.fire("system"));
    assert!(fired);
    assert_eq!(
        lines,
        [
            line(r#"INFO event="session.end" source="system" callbacks=3"#),
            line(r#"ERROR event="session.end_callback_panic" index=0"#),
        ]
    );
    assert_eq!(*ran.lock().unwrap(), ["last"]);
}

/// The session_end fault replays both shutdown messages once.
#[cfg(feature = "fault-injection")]
#[test]
fn the_session_end_fault_replays_the_shutdown_messages() {
    use super::session::inject_fault;
    use crate::fault::{self, FaultPoint};
    let _serial = serial();
    let end = SessionEnd::new();
    let ran = Arc::new(Mutex::new(Vec::new()));
    end.register(recorder(&ran, "marker"));
    let (_, lines) = capture::logs(|| inject_fault(&end));
    assert!(lines.is_empty(), "{lines:?}");
    assert!(ran.lock().unwrap().is_empty());
    fault::debug_inject_fault(FaultPoint::SessionEnd, 0);
    let (_, lines) = capture::logs(|| {
        inject_fault(&end);
        inject_fault(&end);
    });
    fault::disarm(FaultPoint::SessionEnd);
    assert_eq!(
        lines,
        [
            line(r#"WARN event="fault.injected" point="session_end""#),
            line(r#"INFO event="session.end" source="fault" callbacks=1"#),
        ]
    );
    assert_eq!(*ran.lock().unwrap(), ["marker"]);
}
