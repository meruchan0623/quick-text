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
use super::executor::Candidate;
use super::hook::{
    classify, handle_key, handle_mouse, HookAction, HookPipe, HookRecord, KeyInfo, KeyRecord,
    MouseInfo, Origin,
};
use super::input::{Consumer, Lane, Monitor, RESYNC_SECOND_PASS};
use super::keystate::{AsyncKeyProbe, KeyBits, MODIFIER_VERIFY_AFTER};
use super::rawinput::{raw_record, InputEvent, RawRecord, KEYBOARD_LAYOUT_CACHE_INTERVAL};
use super::session::{SessionSignals, Signal};
use super::*;
use crate::mock::{FakeClock, MockForeground};
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
                let raw_queued = raw.contained(false, || {
                    raw_record(&shared, keys, &event, &probe).is_some_and(|r| raw.push(r))
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
                HookRecord::Move { x: -5, y: 7 },
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
    assert!(raw.pop().is_some());
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
            HookRecord::Move { x: -1920, y: 1080 },
            HookRecord::Mouse {
                msg: WM_RBUTTONDOWN,
                x: 3,
                y: 4,
                ..
            }
        ]
    ));
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
    let mut monitor = Monitor::new(keys, &released, &clock, &signals);
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
    let mut monitor = Monitor::new(keys, &held, &clock, &signals);
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
    let mut monitor = Monitor::new(keys, &async_keys, &clock, &signals);
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
