//! L2 tests for the executor service (B-01, S-01) on mock ports and a fake
//! clock: panic isolation, the epoch fence on a stalled task, held-key
//! release, the clipboard ruling and supervisor recovery. Faults come from
//! the `fault-injection` points (`ExecutorPanic`, `ExecutorHang`). Also the
//! history wiring (B-35): one entry per run with its real outcome.
#![cfg_attr(
    not(feature = "fault-injection"),
    allow(
        dead_code,
        unused_imports,
        reason = "helpers of the fault-injection tests"
    )
)]
use super::actions::{Ports, Stage, StageMarks};
use super::executor::{
    classify, run_candidate, stage_timings, Candidate, ExecutorCore, Pause, Runner, Serialized,
    TaskScope, STALL_BUDGET,
};
use super::pipeline_tests::SERIAL;
use super::rawinput::{detect_candidate, Buffer, Event, Kind};
use super::*;
use crate::fault;
use crate::health::{
    Component, ComponentHealth, HealthState, HistoryEntry, Outcome, ReasonCode, TriggerKind,
};
use crate::mock::{FakeClock, InjectCall, MockClipboard, MockForeground, MockInjector};
use crate::runtime::Runtime;
use crate::supervisor::{ExecutorControl, InputSources, Status, Supervisor, BACKOFF};
use crate::traits::{Clock, Injector};
use std::sync::{MutexGuard, PoisonError};

const FOCUS: u64 = 0x2a;
const APP: &str = "notepad.exe";
const KEY_A: u16 = 0x41;
const KEY_B: u16 = 0x42;
/// Rule body; must never reach logs or history.
const BODY: &str = "sentinel-body-7f3a";
const TYPED: &str = "typed output";

fn utf16(text: &str) -> Vec<u8> {
    wide(text).iter().flat_map(|v| v.to_le_bytes()).collect()
}
fn rule(abbreviation: &str, action: Action) -> Rule {
    Rule {
        id: format!("rule-{abbreviation}"),
        match_spec: MatchSpec {
            kind: MatchKind::Literal,
            value: abbreviation.into(),
        },
        trigger_mode: TriggerMode::Immediate,
        action,
        ..Default::default()
    }
}
fn text_rule(abbreviation: &str, method: OutputMethod, body: &str) -> Rule {
    rule(
        abbreviation,
        Action::Text {
            method,
            source: ContentSource::inline(body),
        },
    )
}
fn steps_rule(abbreviation: &str, operations: Vec<Operation>) -> Rule {
    rule(
        abbreviation,
        Action::Steps {
            plan: ActionPlan {
                steps: operations
                    .into_iter()
                    .map(|operation| Step::new(operation, SourceSpan::default()))
                    .collect(),
                ..Default::default()
            },
        },
    )
}
fn text(value: &str) -> Operation {
    Operation::Text {
        text: value.into(),
        method: OutputMethod::Keys,
    }
}
fn code(code: u16) -> Key {
    Key::WindowsCode { code }
}
/// Types the body, then holds A around a tap of B.
fn body_then_hold_a() -> Rule {
    steps_rule(
        "btw",
        vec![
            text(BODY),
            Operation::KeyDown { key: code(KEY_A) },
            Operation::KeyPress { key: code(KEY_B) },
            Operation::KeyUp { key: code(KEY_A) },
        ],
    )
}
fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Input sources that always report healthy.
struct HealthyInputs(Arc<FakeClock>);
impl InputSources for HealthyInputs {
    fn input_health(&self) -> Vec<ComponentHealth> {
        [
            Component::KeyboardHook,
            Component::MouseHook,
            Component::RawInput,
        ]
        .into_iter()
        .map(|component| ComponentHealth {
            component,
            state: HealthState::Healthy,
            reason: None,
            since_ms: self.0.wall_ms(),
        })
        .collect()
    }
    fn reattach(&self, _component: Component) -> Result<(), String> {
        Ok(())
    }
}

/// Hook run on every sleep of a task, after the fake time advanced.
type SleepHook = Arc<Mutex<Option<Box<dyn FnMut(Duration) + Send>>>>;

/// The tasks' clock: the rig's fake clock plus a sleep hook (`FakeClock`
/// has none), so a test can act inside a delay of the action path.
struct HookedClock {
    inner: Arc<FakeClock>,
    hook: SleepHook,
}
impl Clock for HookedClock {
    fn now(&self) -> Instant {
        self.inner.now()
    }
    fn wall_ms(&self) -> i64 {
        self.inner.wall_ms()
    }
    fn sleep(&self, duration: Duration) {
        self.inner.sleep(duration);
        let mut hook = self.hook.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(hook) = hook.as_mut() {
            hook(duration);
        }
    }
}

/// The executor runner on mock ports (production: `run_on_windows`).
fn mock_runner(
    injector: &Arc<MockInjector>,
    foreground: &MockForeground,
    clock: &Arc<FakeClock>,
    clipboard: &MockClipboard,
    sleeps: &SleepHook,
) -> Runner {
    let (injector, foreground, clipboard) =
        (Arc::clone(injector), foreground.clone(), clipboard.clone());
    let clock = HookedClock {
        inner: Arc::clone(clock),
        hook: Arc::clone(sleeps),
    };
    Arc::new(move |candidate: &Candidate, scope: &TaskScope| {
        let fenced = scope.fence(&*injector);
        run_candidate(
            &Ports {
                injector: &fenced,
                foreground: &foreground,
                clock: &clock,
                clipboard: || scope.claim_clipboard(|| Ok(clipboard.clone())),
            },
            scope.shared(),
            candidate,
            VariableContext::default(),
        )
    })
}

/// The executor's release port in tests: records every call through its
/// mock (so hooks and journals see it), then reports the key-up refused
/// (0 accepted) while `refuse_all` is set or its key is in `refused`, as
/// `SendInput` does when it rejects the input.
struct ReleasePort {
    mock: Arc<MockInjector>,
    refuse_all: AtomicBool,
    refused: Mutex<BTreeSet<u16>>,
}
impl ReleasePort {
    fn new(mock: Arc<MockInjector>) -> Self {
        Self {
            mock,
            refuse_all: AtomicBool::new(false),
            refused: Mutex::new(BTreeSet::new()),
        }
    }
    /// Refuses every key-up from now on (`false`: accepts them again).
    fn refuse_all(&self, refuse: bool) {
        self.refuse_all.store(refuse, Ordering::SeqCst);
    }
    /// Refuses the key-ups of `keys` only.
    fn refuse(&self, keys: &[u16]) {
        *self.refused.lock().unwrap_or_else(PoisonError::into_inner) =
            keys.iter().copied().collect();
    }
    fn answer(&self, key: Option<u16>, accepted: u32) -> u32 {
        let refused = self.refuse_all.load(Ordering::SeqCst)
            || key.is_some_and(|key| {
                self.refused
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .contains(&key)
            });
        if refused {
            0
        } else {
            accepted
        }
    }
}
impl std::ops::Deref for ReleasePort {
    type Target = MockInjector;
    fn deref(&self) -> &MockInjector {
        &self.mock
    }
}
impl Injector for ReleasePort {
    fn send_text(&self, text: &str) -> u32 {
        let accepted = self.mock.send_text(text);
        self.answer(None, accepted)
    }
    fn send_keys(&self, keys: &[u16]) -> u32 {
        let accepted = self.mock.send_keys(keys);
        self.answer(None, accepted)
    }
    fn send_chord(&self, keys: &[u16]) -> u32 {
        let accepted = self.mock.send_chord(keys);
        self.answer(None, accepted)
    }
    fn send_key(&self, key: u16, up: bool) -> u32 {
        let accepted = self.mock.send_key(key, up);
        self.answer(Some(key), accepted)
    }
    fn send_mouse(&self, flags: u32, data: u32, x: i32, y: i32) -> u32 {
        let accepted = self.mock.send_mouse(flags, data, x, y);
        self.answer(None, accepted)
    }
}

/// One engine: a real executor service and supervisor over mock ports.
struct ExecRig {
    shared: Arc<Shared>,
    buffer: Buffer,
    generation: u64,
    injector: Arc<MockInjector>,
    /// The executor's release port (WinInjector, MARK, in production); over
    /// the same mock as `injector` unless built by [`ExecRig::with_release`].
    release: Arc<ReleasePort>,
    foreground: MockForeground,
    clipboard: MockClipboard,
    clock: Arc<FakeClock>,
    /// Sleep hook of the tasks' clock (see [`ExecRig::on_sleep`]).
    sleeps: SleepHook,
    core: Arc<ExecutorCore>,
    runtime: Arc<Runtime>,
    supervisor: Arc<Supervisor>,
    _serial: MutexGuard<'static, ()>,
}
impl ExecRig {
    fn new(rules: Vec<Rule>) -> Self {
        Self::build(rules, false)
    }
    /// A rig whose release port is a mock of its own.
    fn with_release(rules: Vec<Rule>) -> Self {
        Self::build(rules, true)
    }
    fn build(rules: Vec<Rule>, separate_release: bool) -> Self {
        let serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
        keystate::PHYSICAL.set(Mutex::new(BTreeSet::new())).ok();
        let shared = Shared::new();
        shared.paused.store(false, Ordering::SeqCst);
        let config = Config {
            rules,
            ..Default::default()
        };
        *shared.compiled.lock().unwrap() = compile_config(&config).unwrap();
        *shared.config.lock().unwrap() = config;
        let injector = Arc::new(MockInjector::new());
        let release = Arc::new(ReleasePort::new(if separate_release {
            Arc::new(MockInjector::new())
        } else {
            Arc::clone(&injector)
        }));
        let foreground = MockForeground::new(MockForeground::window(FOCUS, APP));
        let clock = Arc::new(FakeClock::new());
        let clipboard = MockClipboard::new(7, vec![(13, utf16("original"))]);
        let sleeps = SleepHook::default();
        let runtime = Arc::new(Runtime::new(clock.clone()));
        let core = ExecutorCore::new(
            Arc::clone(&shared),
            clock.clone(),
            mock_runner(&injector, &foreground, &clock, &clipboard, &sleeps),
            release.clone(),
            STALL_BUDGET,
            Arc::clone(&runtime),
        );
        core.rebuild().expect("worker starts");
        let supervisor = Supervisor::new(
            clock.clone(),
            Arc::clone(&runtime),
            core.clone(),
            Arc::new(HealthyInputs(Arc::clone(&clock))),
        );
        let rig = Self {
            shared,
            buffer: Buffer::default(),
            generation: 0,
            injector,
            release,
            foreground,
            clipboard,
            clock,
            sleeps,
            core,
            runtime,
            supervisor,
            _serial: serial,
        };
        rig.supervisor.tick();
        assert_eq!(rig.state(), HealthState::Healthy);
        rig
    }
    fn state(&self) -> HealthState {
        self.runtime.health().state
    }
    fn history(&self) -> Vec<HistoryEntry> {
        self.runtime.list_history(None)
    }
    /// The newest history entry.
    fn last_entry(&self) -> HistoryEntry {
        self.history().pop().expect("a history entry")
    }
    fn component(&self, component: Component) -> Option<(HealthState, Option<ReasonCode>)> {
        self.runtime
            .health()
            .components
            .into_iter()
            .find(|health| health.component == component)
            .map(|health| (health.state, health.reason))
    }
    fn executor_status(&self) -> Status {
        self.core
            .probe(self.clock.now())
            .into_iter()
            .find(|(component, _)| *component == Component::Executor)
            .map(|(_, status)| status)
            .unwrap()
    }
    /// Types `abbreviation` key by key (as the RawInput callback would) and
    /// hands the match to the executor; false when it was dropped.
    fn trigger(&mut self, abbreviation: &str) -> bool {
        *self
            .shared
            .last
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
        let candidate = self.detect(abbreviation);
        self.core.enqueue(candidate)
    }
    /// Types `abbreviation` key by key; the match (not yet queued).
    fn detect(&mut self, abbreviation: &str) -> Candidate {
        self.buffer = Buffer::default();
        let mut candidate = None;
        for ch in abbreviation.chars() {
            self.generation += 1;
            self.shared
                .generation
                .store(self.generation, Ordering::SeqCst);
            let stamp = rawinput::stamp(&self.shared, &self.foreground).unwrap();
            let event = Event {
                kind: Kind::Text(ch.to_string()),
                stamp,
                generation: self.generation,
                source: "fixture",
                captured_allowed: true,
            };
            candidate = detect_candidate(&self.shared, &mut self.buffer, event, &self.foreground);
        }
        candidate.expect("abbreviation matches its rule")
    }
    fn wait_idle(&self) {
        wait_until("the executor to go idle", || {
            !self.shared.busy.load(Ordering::SeqCst)
        });
    }
    /// One automatic run to completion; its result.
    fn run(&mut self, abbreviation: &str) -> ExecutionResult {
        assert!(self.trigger(abbreviation), "candidate was queued");
        self.wait_idle();
        self.shared
            .last
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .expect("the run published a result")
    }
    /// Runs `hook` on every sleep of a task (on the worker thread).
    fn on_sleep(&self, hook: impl FnMut(Duration) + Send + 'static) {
        *self.sleeps.lock().unwrap_or_else(PoisonError::into_inner) = Some(Box::new(hook));
    }
    /// Arms a real `ms` stall of the worker on the send after `after`.
    #[cfg(feature = "fault-injection")]
    fn stall_after(&self, after: InjectCall, ms: u64) -> Arc<AtomicBool> {
        let armed = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&armed);
        self.injector.on_call(move |call| {
            if *call == after && !flag.load(Ordering::SeqCst) {
                fault::debug_inject_fault(fault::FaultPoint::ExecutorHang, ms);
                flag.store(true, Ordering::SeqCst);
            }
        });
        armed
    }
    /// Waits until the worker consumed the armed stall (it is now asleep).
    fn wait_stalled(&self, armed: &AtomicBool) {
        wait_until("the worker to stall", || {
            armed.load(Ordering::SeqCst) && fault::peek(fault::FaultPoint::ExecutorHang).is_none()
        });
    }
}
impl Drop for ExecRig {
    fn drop(&mut self) {
        self.core.shutdown();
    }
}

/// Minimal global `tracing` collector: keeps every event's fields so tests
/// can assert event names and that no content reached the log.
mod capture {
    use std::sync::{Mutex, OnceLock, PoisonError};
    use tracing::field::{Field, Visit};
    use tracing::span::{Attributes, Id, Record};
    use tracing::{Event, Metadata, Subscriber};

    #[derive(Clone, Debug, Default)]
    pub(super) struct Captured {
        pub(super) event: String,
        pub(super) fields: Vec<(String, String)>,
    }
    impl Captured {
        pub(super) fn field(&self, name: &str) -> Option<&str> {
            self.fields
                .iter()
                .find(|(field, _)| field == name)
                .map(|(_, value)| value.as_str())
        }
        pub(super) fn mentions(&self, needle: &str) -> bool {
            self.fields.iter().any(|(_, value)| value.contains(needle))
        }
    }

    static EVENTS: Mutex<Vec<Captured>> = Mutex::new(Vec::new());
    static INSTALLED: OnceLock<bool> = OnceLock::new();

    struct Fields(Captured);
    impl Visit for Fields {
        fn record_str(&mut self, field: &Field, value: &str) {
            if field.name() == "event" {
                self.0.event = value.into();
            }
            self.0.fields.push((field.name().into(), value.into()));
        }
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.0
                .fields
                .push((field.name().into(), format!("{value:?}")));
        }
    }

    struct Collector;
    impl Subscriber for Collector {
        fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _span: &Attributes<'_>) -> Id {
            Id::from_u64(1)
        }
        fn record(&self, _span: &Id, _values: &Record<'_>) {}
        fn record_follows_from(&self, _span: &Id, _follows: &Id) {}
        fn event(&self, event: &Event<'_>) {
            let mut fields = Fields(Captured::default());
            event.record(&mut fields);
            EVENTS
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(fields.0);
        }
        fn enter(&self, _span: &Id) {}
        fn exit(&self, _span: &Id) {}
    }

    /// Installs the collector process-wide (once); false if another global
    /// subscriber was installed first.
    pub(super) fn install() -> bool {
        *INSTALLED.get_or_init(|| tracing::subscriber::set_global_default(Collector).is_ok())
    }
    /// Position to read new events from.
    pub(super) fn mark() -> usize {
        EVENTS.lock().unwrap_or_else(PoisonError::into_inner).len()
    }
    /// Events captured after `mark`.
    pub(super) fn since(mark: usize) -> Vec<Captured> {
        EVENTS.lock().unwrap_or_else(PoisonError::into_inner)[mark..].to_vec()
    }
}

/// S-01: a task panics once; the panic is logged as `executor.panic`
/// (location and kind only), the keys it held are released by the busy
/// guard, the worker keeps serving, the supervisor takes the executor
/// through Recovering back to Healthy and ten triggers in a row succeed.
#[cfg(feature = "fault-injection")]
#[test]
fn s01_panic_is_isolated_and_the_executor_keeps_serving() {
    assert!(
        capture::install(),
        "test collector is the global subscriber"
    );
    let mut rig = ExecRig::new(vec![body_then_hold_a()]);
    let mark = capture::mark();
    let armed = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&armed);
    rig.injector.on_call(move |call| {
        if *call
            == (InjectCall::Key {
                key: KEY_A,
                up: false,
            })
            && !flag.swap(true, Ordering::SeqCst)
        {
            fault::debug_inject_fault(fault::FaultPoint::ExecutorPanic, 0);
        }
    });

    let failed = rig.run("btw");
    assert_eq!(failed.status, "failed");
    let entry = rig.last_entry();
    assert_eq!(
        (entry.outcome, entry.reason),
        (Outcome::Failed, ReasonCode::ExecutorPanic)
    );
    assert!(entry.side_effects);
    // The panic skipped the plan's own cleanup: the busy guard released A.
    assert_eq!(
        rig.injector.calls().last(),
        Some(&InjectCall::Key {
            key: KEY_A,
            up: true
        })
    );
    assert!(rig.core.held_keys().is_empty());
    let events = capture::since(mark);
    let panics: Vec<_> = events
        .iter()
        .filter(|event| event.event == "executor.panic")
        .collect();
    assert_eq!(panics.len(), 1);
    assert_eq!(panics[0].field("stage"), Some("output"));
    assert_eq!(panics[0].field("kind"), Some("str"));
    assert!(events.iter().all(|event| !event.mentions(BODY)));

    // The same worker serves the next trigger before the supervisor reacts.
    assert_eq!(rig.run("btw").status, "unknown");
    assert!(matches!(
        rig.executor_status(),
        Status::Down(ReasonCode::ExecutorPanic)
    ));
    rig.supervisor.tick();
    assert_eq!(rig.state(), HealthState::Recovering);
    rig.clock.advance(BACKOFF[0]);
    rig.supervisor.tick();
    assert_eq!(rig.state(), HealthState::Healthy);

    for _ in 0..10 {
        let result = rig.run("btw");
        assert_eq!(result.status, "unknown", "{}", result.message);
        assert_eq!(result.completed_steps, 4);
    }
    assert_eq!(rig.injector.typed(), BODY.repeat(12));
    let history = rig.history();
    assert_eq!(history.len(), 12, "one entry per run");
    assert!(history[1..]
        .iter()
        .all(|entry| (entry.outcome, entry.reason) == (Outcome::Succeeded, ReasonCode::Ok)));
}

/// A task stuck inside a send exceeds the budget: the supervisor bumps the
/// epoch and owes the held key's MARK key-up (sending nothing itself); its
/// rebuild pays it from the bitmap and starts a new worker; the stuck thread
/// wakes up, sends nothing and exits.
#[cfg(feature = "fault-injection")]
#[test]
fn hung_task_is_fenced_and_a_new_worker_takes_over() {
    let mut rig = ExecRig::new(vec![body_then_hold_a()]);
    let epoch = rig.core.epoch();
    let armed = rig.stall_after(
        InjectCall::Key {
            key: KEY_A,
            up: false,
        },
        600,
    );
    assert!(rig.trigger("btw"));
    rig.wait_stalled(&armed);
    assert_eq!(rig.core.held_keys(), vec![KEY_A]);

    rig.clock.advance(STALL_BUDGET - Duration::from_secs(1));
    rig.supervisor.tick();
    assert_eq!(rig.core.epoch(), epoch, "still within the budget");
    rig.clock.advance(Duration::from_secs(2));
    rig.supervisor.tick();
    assert_eq!(rig.core.epoch(), epoch + 1);
    // The fence records the stalled run on its behalf.
    let entry = rig.last_entry();
    assert_eq!(
        (entry.outcome, entry.reason),
        (Outcome::Failed, ReasonCode::ExecutorTimeout)
    );
    assert_eq!(entry.rule_id, "rule-btw");
    assert!(entry.side_effects);
    // The supervisor's tick sent nothing: the key-up is owed.
    assert_eq!(rig.core.held_keys(), vec![KEY_A]);
    assert!(rig.core.release_owed());
    assert!(!rig.injector.calls().contains(&key_up(KEY_A)));
    assert!(!rig.shared.busy.load(Ordering::SeqCst));
    assert!(matches!(
        rig.executor_status(),
        Status::Down(ReasonCode::ExecutorTimeout)
    ));
    assert_eq!(rig.state(), HealthState::Recovering);
    rig.clock.advance(BACKOFF[0]);
    rig.supervisor.tick();
    assert_eq!(rig.state(), HealthState::Healthy);
    // The rebuild paid it.
    let fenced = rig.injector.calls();
    assert_eq!(fenced.last(), Some(&key_up(KEY_A)));
    assert!(rig.core.held_keys().is_empty());
    assert!(!rig.core.release_owed());

    assert!(rig.core.join_retired(Duration::from_secs(5)));
    assert_eq!(
        rig.injector.calls(),
        fenced,
        "the woken thread sent nothing"
    );
    assert_eq!(rig.history().len(), 1, "the woken thread recorded nothing");
    rig.injector.on_call(|_| {});
    let result = rig.run("btw");
    assert_eq!(result.status, "unknown", "{}", result.message);
    assert_eq!(rig.history().len(), 2);
    assert_eq!(rig.last_entry().outcome, Outcome::Succeeded);
}

/// B-01 (5), ruled for no-regression #2: while a fenced task still holds
/// the clipboard, paste fails with nothing sent (no typing fallback) and the
/// clipboard reports Degraded; key output keeps working.
#[cfg(feature = "fault-injection")]
#[test]
fn clipboard_held_by_a_stalled_task_refuses_paste_without_typing() {
    let mut rig = ExecRig::new(vec![
        text_rule("btw", OutputMethod::Paste, BODY),
        text_rule("kw", OutputMethod::Keys, TYPED),
    ]);
    let armed = rig.stall_after(InjectCall::Keys(vec![VK_BACK]), 1_500);
    assert!(rig.trigger("btw"));
    rig.wait_stalled(&armed);
    rig.clock.advance(STALL_BUDGET + Duration::from_secs(1));
    rig.supervisor.tick();
    assert_eq!(rig.state(), HealthState::Recovering);
    rig.clock.advance(BACKOFF[0]);
    rig.supervisor.tick();
    assert_eq!(rig.state(), HealthState::Degraded);
    assert_eq!(
        rig.component(Component::Clipboard),
        Some((HealthState::Degraded, Some(ReasonCode::ClipboardBusy)))
    );

    let sent = rig.injector.calls().len();
    let refused = rig.run("btw");
    assert_eq!(refused.status, "failed");
    assert!(refused.message.contains("no typing fallback"));
    assert_eq!(rig.injector.calls().len(), sent, "no erase, no typing");
    let entry = rig.last_entry();
    assert_eq!(
        (entry.outcome, entry.reason),
        (Outcome::Failed, ReasonCode::ClipboardBusy)
    );
    assert!(!entry.side_effects);
    let typed = rig.run("kw");
    assert_eq!(typed.status, "unknown", "{}", typed.message);
    assert_eq!(rig.injector.typed(), TYPED);

    assert!(rig.core.join_retired(Duration::from_secs(5)));
    rig.supervisor.tick();
    assert_eq!(rig.state(), HealthState::Healthy);
    let pasted = rig.run("btw");
    assert_eq!(pasted.status, "unknown", "{}", pasted.message);
}

/// Fail-stop after an epoch change: the guard's checkpoint stops a fenced
/// task before its next input, and it no longer publishes a result.
#[test]
fn task_fenced_mid_plan_stops_before_the_next_input() {
    let mut rig = ExecRig::new(vec![steps_rule("btw", vec![text("one"), text("two")])]);
    let epoch = rig.core.epoch();
    let core = Arc::downgrade(&rig.core);
    let fenced = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&fenced);
    rig.injector.on_call(move |call| {
        if matches!(call, InjectCall::Text(_)) && !flag.load(Ordering::SeqCst) {
            if let Some(core) = core.upgrade() {
                core.fence(core.epoch());
            }
            flag.store(true, Ordering::SeqCst);
        }
    });
    assert!(rig.trigger("btw"));
    wait_until("the fence", || fenced.load(Ordering::SeqCst));
    rig.core.shutdown();
    assert!(rig.core.join_retired(Duration::from_secs(5)));

    assert_eq!(rig.core.epoch(), epoch + 1);
    let calls = rig.injector.calls();
    let first_text = calls
        .iter()
        .position(|call| matches!(call, InjectCall::Text(_)))
        .unwrap();
    assert_eq!(first_text, calls.len() - 1, "nothing sent after the fence");
    assert!(!rig.injector.typed().contains("two"));
    assert!(rig
        .shared
        .last
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .is_none());
    assert!(matches!(
        rig.executor_status(),
        Status::Down(ReasonCode::ExecutorTimeout)
    ));
    // Recorded once, by the fence; the stopped thread adds nothing.
    let history = rig.history();
    assert_eq!(history.len(), 1);
    assert_eq!(
        (history[0].outcome, history[0].reason),
        (Outcome::Failed, ReasonCode::ExecutorTimeout)
    );
    assert!(history[0].side_effects, "a send was in flight");
}

/// Opens a gate when dropped, so a worker held inside a send never outlives
/// a failing test.
struct Gate(Arc<AtomicBool>);
impl Drop for Gate {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// B-35: a match that reaches the executor while another action runs is
/// dropped and recorded as `dropped_busy`, with identifiers only; the
/// running action records its own entry.
#[test]
fn trigger_dropped_while_busy_is_recorded() {
    let mut rig = ExecRig::new(vec![
        text_rule("btw", OutputMethod::Keys, BODY),
        text_rule("kw", OutputMethod::Keys, TYPED),
    ]);
    // Matched just before the executor turned busy (the dispatcher race).
    let late = rig.detect("kw");
    let entered = Arc::new(AtomicBool::new(false));
    let gate = Gate(Arc::new(AtomicBool::new(false)));
    let (inside, open) = (Arc::clone(&entered), Arc::clone(&gate.0));
    rig.injector.on_call(move |_| {
        if !inside.swap(true, Ordering::SeqCst) {
            while !open.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    });
    assert!(rig.trigger("btw"));
    wait_until("the worker's first send", || entered.load(Ordering::SeqCst));
    assert!(!rig.core.enqueue(late), "dropped while busy");
    let dropped = rig.last_entry();
    assert_eq!(
        (dropped.outcome, dropped.reason),
        (Outcome::DroppedBusy, ReasonCode::DroppedBusy)
    );
    assert_eq!(dropped.rule_id, "rule-kw");
    assert_eq!(dropped.trigger, TriggerKind::Immediate);
    assert_eq!(dropped.process.as_deref(), Some(APP));
    assert!(!dropped.side_effects);

    drop(gate);
    rig.wait_idle();
    let history = rig.history();
    assert_eq!(history.len(), 2, "one entry per trigger");
    assert_eq!(history[1].rule_id, "rule-btw");
    assert_eq!(history[1].outcome, Outcome::Succeeded);
}

/// B-35: automatic runs record their real outcome (no longer always
/// "unknown") with per-stage timings.
#[test]
fn automatic_runs_record_their_real_outcome_and_stage_timings() {
    let mut rig = ExecRig::new(vec![
        text_rule("btw", OutputMethod::Keys, BODY),
        text_rule("pst", OutputMethod::Paste, BODY),
    ]);
    let typed = rig.run("btw");
    assert_eq!(typed.status, "unknown", "the UI status stays unverified");
    let entry = rig.last_entry();
    assert_eq!(
        (entry.outcome, entry.reason),
        (Outcome::Succeeded, ReasonCode::Ok)
    );
    assert_eq!(entry.rule_id, "rule-btw");
    assert_eq!(entry.trigger, TriggerKind::Immediate);
    assert_eq!(entry.process.as_deref(), Some(APP));
    assert_eq!(entry.completed_steps, typed.completed_steps);
    assert!(entry.side_effects);
    let timings = entry.timings;
    for (stage, value) in [
        ("queue", timings.queue_us),
        ("resolve", timings.resolve_us),
        ("erase", timings.erase_us),
        ("output", timings.output_us),
        ("total", timings.total_us),
    ] {
        assert!(value.is_some(), "{stage} timing missing");
    }
    assert_eq!(timings.restore_us, None, "no clipboard lease");

    assert_eq!(rig.run("pst").status, "unknown");
    let pasted = rig.last_entry();
    assert_eq!(pasted.outcome, Outcome::Succeeded);
    assert!(pasted.timings.restore_us.is_some());

    // Fail-stop: the first typed character is cut short after the erase.
    rig.injector.short_count(rig.injector.calls().len() + 3, 1);
    assert_eq!(rig.run("btw").status, "failed");
    let partial = rig.last_entry();
    assert_eq!(
        (partial.outcome, partial.reason),
        (Outcome::Failed, ReasonCode::InjectPartial)
    );
    assert!(partial.side_effects);

    // A clipboard that cannot be restored: refused before any input.
    let sent = rig.injector.calls().len();
    rig.clipboard.take_over(9, vec![(2, vec![1])]);
    assert_eq!(rig.run("pst").status, "failed");
    let refused = rig.last_entry();
    assert_eq!(
        (refused.outcome, refused.reason),
        (Outcome::Skipped, ReasonCode::ClipboardFormatSkipped)
    );
    assert!(!refused.side_effects);
    assert_eq!(rig.injector.calls().len(), sent);
    assert_eq!(rig.history().len(), 4);
}

/// B-35: history rows carry identifiers only. Sentinel body, abbreviations
/// and clipboard text never reach them, whatever the outcome.
#[test]
fn history_never_contains_the_body_or_the_abbreviation() {
    let mut keys = text_rule("zqv", OutputMethod::Keys, BODY);
    keys.id = "rule-keys".into();
    let mut paste = text_rule("zqp", OutputMethod::Paste, BODY);
    paste.id = "rule-paste".into();
    let mut rig = ExecRig::new(vec![keys, paste]);
    rig.run("zqv");
    rig.run("zqp");
    rig.injector.short_count(rig.injector.calls().len() + 3, 1);
    rig.run("zqv");
    let late = rig.detect("zqp");
    rig.shared.busy.store(true, Ordering::SeqCst);
    assert!(!rig.core.enqueue(late));
    rig.shared.busy.store(false, Ordering::SeqCst);

    let history = rig.history();
    assert_eq!(history.len(), 4);
    let json = serde_json::to_string(&history).unwrap();
    assert!(json.contains("rule-keys") && json.contains("rule-paste"));
    for secret in [BODY, "zqv", "zqp", "original"] {
        assert!(!json.contains(secret), "{secret} reached the history");
    }
}

/// Outcome mapping of the real path: refusals with nothing sent are
/// skipped, engine failures failed, completed plans succeeded; the stage
/// decides resolve and preflight failures.
#[test]
fn verdicts_follow_the_real_path() {
    let run = |status: &str, message: &str, sent: bool| -> Result<ExecutionResult, String> {
        Ok(ExecutionResult {
            id: String::new(),
            status: status.into(),
            message: message.into(),
            completed_steps: 0,
            side_effects: sent,
        })
    };
    let verdict = |result: Result<ExecutionResult, String>, stage: Stage, sent: bool| {
        let verdict = classify(&result, stage, sent);
        (verdict.outcome, verdict.reason)
    };
    let failed = |message: &str| Err::<ExecutionResult, String>(message.into());
    let cases = [
        (
            verdict(run("unknown", "Input was sent", true), Stage::Done, true),
            (Outcome::Succeeded, ReasonCode::Ok),
        ),
        (
            verdict(run("cancelled", "Cancelled", true), Stage::Done, true),
            (Outcome::Cancelled, ReasonCode::Cancelled),
        ),
        (
            verdict(
                run("failed", "Password controls are blocked", false),
                Stage::Erase,
                false,
            ),
            (Outcome::Skipped, ReasonCode::ProtectedControl),
        ),
        (
            verdict(
                run("failed", "Password controls are blocked", true),
                Stage::Erase,
                true,
            ),
            (Outcome::Failed, ReasonCode::ProtectedControl),
        ),
        // A cancel during erasure comes back as a failed result.
        (
            verdict(run("failed", "Cancelled", false), Stage::Erase, false),
            (Outcome::Cancelled, ReasonCode::Cancelled),
        ),
        (
            verdict(run("failed", "Cancelled", true), Stage::Erase, true),
            (Outcome::Cancelled, ReasonCode::Cancelled),
        ),
        (
            verdict(failed("no such snippet"), Stage::Resolve, false),
            (Outcome::Skipped, ReasonCode::ResolveFailed),
        ),
        (
            verdict(failed("Replay speed"), Stage::Preflight, false),
            (Outcome::Skipped, ReasonCode::PreflightFailed),
        ),
        (
            verdict(
                failed("Foreground window/process changed; stopped safely"),
                Stage::Guard,
                false,
            ),
            (Outcome::Skipped, ReasonCode::FocusChanged),
        ),
        (
            verdict(failed("Engine paused; input stopped"), Stage::Guard, false),
            (Outcome::Skipped, ReasonCode::Paused),
        ),
        (
            verdict(failed("Clipboard is busy"), Stage::Clipboard, false),
            (Outcome::Failed, ReasonCode::ClipboardBusy),
        ),
        (
            verdict(failed("Cancelled"), Stage::Output, true),
            (Outcome::Cancelled, ReasonCode::Cancelled),
        ),
        (
            verdict(failed("unexpected"), Stage::Output, false),
            (Outcome::Failed, ReasonCode::Internal),
        ),
    ];
    for (index, (got, want)) in cases.into_iter().enumerate() {
        assert_eq!(got, want, "case {index}");
    }
}

/// Stage timings in microseconds from the stage entry marks: a stage ends
/// where the next measured one starts; absent stages stay `None`.
#[test]
fn stage_timings_measure_each_stage_in_microseconds() {
    let origin = Instant::now();
    let at = |ms| origin + Duration::from_millis(ms);
    let mut marks = StageMarks::default();
    marks[Stage::Resolve as usize] = Some(at(1));
    marks[Stage::Preflight as usize] = Some(at(3));
    marks[Stage::Erase as usize] = Some(at(4));
    marks[Stage::Output as usize] = Some(at(6));
    marks[Stage::Cleanup as usize] = Some(at(9));
    marks[Stage::Done as usize] = Some(at(10));
    let timings = stage_timings(Some(origin), at(1), at(11), &marks);
    assert_eq!(timings.queue_us, Some(1_000));
    assert_eq!(timings.resolve_us, Some(2_000));
    assert_eq!(timings.erase_us, Some(2_000));
    assert_eq!(timings.output_us, Some(4_000));
    assert_eq!(timings.restore_us, None);
    assert_eq!(timings.total_us, Some(11_000));

    // Stopped inside the erase: it ends at the run's end, no output.
    let mut stopped = StageMarks::default();
    stopped[Stage::Resolve as usize] = Some(at(1));
    stopped[Stage::Erase as usize] = Some(at(2));
    let timings = stage_timings(None, at(1), at(5), &stopped);
    assert_eq!(timings.queue_us, None);
    assert_eq!(timings.erase_us, Some(3_000));
    assert_eq!(timings.output_us, None);
    assert_eq!(timings.total_us, Some(4_000));
}

/// Holds one thread at a chosen point: the thread calls the `waiter` (from
/// a hook), which marks its arrival and blocks until the hold is dropped.
struct Hold {
    arrived: Arc<AtomicBool>,
    gate: Gate,
}
impl Hold {
    fn new() -> Self {
        Self {
            arrived: Arc::new(AtomicBool::new(false)),
            gate: Gate(Arc::new(AtomicBool::new(false))),
        }
    }
    fn waiter(&self) -> impl Fn() + Send + Sync + 'static {
        let (arrived, open) = (Arc::clone(&self.arrived), Arc::clone(&self.gate.0));
        move || {
            arrived.store(true, Ordering::SeqCst);
            while !open.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }
    fn wait_arrived(&self, what: &str) {
        wait_until(what, || self.arrived.load(Ordering::SeqCst));
    }
}

/// Holds the first injector call (on whichever thread makes it).
fn hold_first_send(injector: &MockInjector) -> Hold {
    let hold = Hold::new();
    let wait = hold.waiter();
    let first = AtomicBool::new(false);
    injector.on_call(move |_| {
        if !first.swap(true, Ordering::SeqCst) {
            wait();
        }
    });
    hold
}

fn is_thread(name: &str) -> bool {
    std::thread::current().name() == Some(name)
}

/// Lifecycle ruling: the fence decides ownership in one hold of the
/// lifecycle lock. Held right after the bump, it lets the old task finish:
/// that task records nothing and publishes nothing; the fence then writes
/// the only history entry of the run.
#[test]
fn a_fence_held_after_the_bump_writes_the_only_entry() {
    let mut rig = ExecRig::new(vec![text_rule("btw", OutputMethod::Keys, BODY)]);
    let in_send = hold_first_send(&rig.injector);
    let decided = Hold::new();
    let wait = decided.waiter();
    rig.core.on_pause(move |at| {
        if at == Pause::FenceDecided {
            wait();
        }
    });
    assert!(rig.trigger("btw"));
    in_send.wait_arrived("the task's first send");
    let epoch = rig.core.epoch();
    let core = Arc::clone(&rig.core);
    let fencer = std::thread::spawn(move || core.fence(epoch));
    decided.wait_arrived("the fence, after the bump");
    assert_eq!(rig.core.epoch(), epoch + 1);
    assert!(!rig.shared.busy.load(Ordering::SeqCst));
    assert_eq!(rig.core.running_task(), None, "the fence took the run");

    drop(in_send);
    assert!(
        rig.core.join_retired(Duration::from_secs(5)),
        "the old task finished"
    );
    assert!(rig.history().is_empty(), "the old task recorded nothing");
    assert!(rig
        .shared
        .last
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .is_none());

    drop(decided);
    assert!(fencer.join().unwrap());
    let history = rig.history();
    assert_eq!(history.len(), 1, "exactly one entry");
    assert_eq!(
        (history[0].outcome, history[0].reason),
        (Outcome::Failed, ReasonCode::ExecutorTimeout)
    );
    assert_eq!(history[0].rule_id, "rule-btw");
    assert!(history[0].side_effects);
}

/// Lifecycle ruling: the busy guard compares (epoch and run id) before it
/// clears. Held just before that, while a fence retires its run and a
/// replacement starts, it must leave the replacement's `running` and `busy`
/// alone, and write neither history nor `last`.
#[test]
fn a_guard_held_before_the_clear_leaves_the_replacement_alone() {
    let mut rig = ExecRig::new(vec![
        text_rule("btw", OutputMethod::Keys, BODY),
        text_rule("kw", OutputMethod::Keys, TYPED),
    ]);
    let before_clear = Hold::new();
    let wait = before_clear.waiter();
    rig.core.on_pause(move |at| {
        if at == Pause::GuardBeforeClear && is_thread("qt-executor-0") {
            wait();
        }
    });
    assert!(rig.trigger("btw"));
    before_clear.wait_arrived("the first task's guard");
    assert!(rig.core.fence(rig.core.epoch()));
    rig.core.rebuild().expect("a new worker");
    let in_send = hold_first_send(&rig.injector);
    assert!(rig.trigger("kw"));
    in_send.wait_arrived("the replacement's first send");
    let replacement = rig.core.running_task().expect("the replacement runs");
    assert_eq!(replacement.1, "rule-kw");

    drop(before_clear);
    assert!(
        rig.core.join_retired(Duration::from_secs(5)),
        "the old worker exited"
    );
    assert_eq!(rig.core.running_task(), Some(replacement));
    assert!(rig.shared.busy.load(Ordering::SeqCst), "busy untouched");
    assert!(rig
        .shared
        .last
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .is_none());
    let history = rig.history();
    assert_eq!(history.len(), 1, "only the fence's entry");
    assert_eq!(
        (history[0].outcome, history[0].reason),
        (Outcome::Failed, ReasonCode::ExecutorTimeout)
    );

    drop(in_send);
    rig.wait_idle();
    let entry = rig.last_entry();
    assert_eq!(
        (entry.rule_id.as_str(), entry.outcome),
        ("rule-kw", Outcome::Succeeded)
    );
    assert_eq!(rig.history().len(), 2);
}

/// Both injectors' calls in send order, labeled by port ("task" or
/// "release"). Hooks run inside the send, so under the send lock.
type Journal = Arc<std::sync::Mutex<Vec<(&'static str, InjectCall)>>>;

/// Journals every call of the rig's task and release ports; `also` runs on
/// each task call after it was journaled.
fn journal(rig: &ExecRig, mut also: impl FnMut(&InjectCall) + Send + 'static) -> Journal {
    let journal = Journal::default();
    let log = Arc::clone(&journal);
    rig.release.on_call(move |call| {
        let mut log = log.lock().unwrap_or_else(PoisonError::into_inner);
        log.push(("release", call.clone()));
    });
    let log = Arc::clone(&journal);
    rig.injector.on_call(move |call| {
        let entry = ("task", call.clone());
        log.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(entry);
        also(call);
    });
    journal
}

fn journaled(journal: &Journal) -> Vec<(&'static str, InjectCall)> {
    journal
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

fn key_up(key: u16) -> InjectCall {
    InjectCall::Key { key, up: true }
}

fn press_b() -> Rule {
    steps_rule("kb", vec![Operation::KeyPress { key: code(KEY_B) }])
}

/// Send exclusion (a): a task stopped after its epoch check and before the
/// send, then fenced, submits nothing once let go: the check is repeated
/// inside the send lock, right before the call. The fence only owes the
/// MARK key-up; the rebuild pays it (release port) before the new epoch's
/// input.
#[test]
fn a_send_checked_before_the_fence_submits_nothing_after_it() {
    let mut rig = ExecRig::with_release(vec![
        steps_rule(
            "hold",
            vec![
                Operation::KeyDown { key: code(KEY_A) },
                Operation::KeyPress { key: code(KEY_B) },
                Operation::KeyUp { key: code(KEY_A) },
            ],
        ),
        text_rule("kw", OutputMethod::Keys, TYPED),
    ]);
    let journal = journal(&rig, |_| {});
    let a_down = InjectCall::Key {
        key: KEY_A,
        up: false,
    };
    let checked = Hold::new();
    let wait = checked.waiter();
    let (injector, pressed) = (Arc::clone(&rig.injector), a_down.clone());
    rig.core.on_pause(move |at| {
        // The send after A's key-down: the tap of B.
        if at == Pause::BeforeSend
            && is_thread("qt-executor-0")
            && injector.calls().contains(&pressed)
        {
            wait();
        }
    });
    assert!(rig.trigger("hold"));
    checked.wait_arrived("the old task, checked but not yet locked");
    assert_eq!(rig.core.held_keys(), vec![KEY_A]);

    assert!(rig.core.fence(rig.core.epoch()));
    assert_eq!(rig.core.held_keys(), vec![KEY_A], "the fence sent nothing");
    assert!(rig.core.release_owed());
    assert!(rig.release.calls().is_empty());
    rig.core.rebuild().expect("a new worker");
    assert!(rig.core.held_keys().is_empty(), "the rebuild paid");
    assert!(!rig.core.release_owed());
    let result = rig.run("kw");
    assert_eq!(result.status, "unknown", "{}", result.message);

    drop(checked);
    assert!(
        rig.core.join_retired(Duration::from_secs(5)),
        "the old worker exited"
    );
    let entries = journaled(&journal);
    assert!(
        !entries
            .iter()
            .any(|(_, call)| *call == InjectCall::Keys(vec![KEY_B])),
        "the old task submitted nothing: {entries:?}"
    );
    let at = entries
        .iter()
        .position(|entry| *entry == ("task", a_down.clone()))
        .expect("A was pressed");
    assert_eq!(entries[at + 1], ("release", key_up(KEY_A)), "{entries:?}");
    let after = &entries[at + 2..];
    assert!(after.iter().all(|(port, _)| *port == "task"), "{entries:?}");
    assert!(after
        .iter()
        .any(|(_, call)| matches!(call, InjectCall::Text(_))));
    assert_eq!(rig.injector.typed(), TYPED);
    let ups = entries.iter().filter(|(_, call)| *call == key_up(KEY_A));
    assert_eq!(ups.count(), 1);
    let history = rig.history();
    assert_eq!(history.len(), 2);
    assert_eq!(
        (
            history[0].rule_id.as_str(),
            history[0].outcome,
            history[0].reason
        ),
        ("rule-hold", Outcome::Failed, ReasonCode::ExecutorTimeout)
    );
    assert_eq!(
        (history[1].rule_id.as_str(), history[1].outcome),
        ("rule-kw", Outcome::Succeeded)
    );
}

/// Send exclusion (b): the injector takes a tap's key-down, then blocks
/// inside the send. The fence cannot get the send lock: it owes the key-ups,
/// the executor reports Down and the rebuild refuses (fail closed). The send
/// then returns a partial count after the epoch moved on: the key stays
/// held, the old task's own cleanup is refused, and the owed MARK key-up
/// goes out exactly once through the release port, before any new input.
#[test]
fn a_partial_send_returning_after_the_fence_gets_its_key_up() {
    let mut rig = ExecRig::with_release(vec![press_b()]);
    let tap = InjectCall::Keys(vec![KEY_B]);
    let warm = rig.run("kb");
    assert_eq!(warm.status, "unknown", "{}", warm.message);
    // The same run again takes the same path: short-count its tap.
    let calls = rig.injector.calls();
    let at = calls.iter().position(|call| *call == tap).expect("the tap");
    rig.injector.short_count(calls.len() + at, 1);
    let in_send = Hold::new();
    let wait = in_send.waiter();
    let (blocked, target) = (AtomicBool::new(false), tap.clone());
    let journal = journal(&rig, move |call| {
        if *call == target && !blocked.swap(true, Ordering::SeqCst) {
            wait();
        }
    });
    assert!(rig.trigger("kb"));
    in_send.wait_arrived("the tap inside SendInput");
    assert_eq!(rig.core.held_keys(), vec![KEY_B], "marked before the call");

    assert!(rig.core.fence(rig.core.epoch()));
    assert!(rig.core.release_owed(), "send lock held: key-ups owed");
    assert!(matches!(
        rig.executor_status(),
        Status::Down(ReasonCode::ExecutorTimeout)
    ));
    let fenced = rig.last_entry();
    assert_eq!(
        (fenced.outcome, fenced.reason),
        (Outcome::Failed, ReasonCode::ExecutorTimeout)
    );
    assert!(fenced.side_effects, "a send was in flight");
    assert!(
        rig.core.rebuild().is_err(),
        "no worker while a send is stuck"
    );
    assert!(!rig.trigger("kb"), "the queue stays closed");
    assert_eq!(rig.last_entry().outcome, Outcome::Skipped);
    assert!(
        rig.release.calls().is_empty(),
        "nothing sent without the lock"
    );

    drop(in_send);
    assert!(
        rig.core.join_retired(Duration::from_secs(5)),
        "the old worker exited"
    );
    let entries = journaled(&journal);
    assert_eq!(
        entries.last(),
        Some(&("release", key_up(KEY_B))),
        "{entries:?}"
    );
    let ups = entries.iter().filter(|(_, call)| *call == key_up(KEY_B));
    assert_eq!(ups.count(), 1, "{entries:?}");
    assert!(rig.core.held_keys().is_empty());
    assert!(!rig.core.release_owed());

    rig.core.rebuild().expect("the lock is free again");
    assert!(matches!(rig.executor_status(), Status::Ok));
    let result = rig.run("kb");
    assert_eq!(result.status, "unknown", "{}", result.message);
    let entries = journaled(&journal);
    let released = entries
        .iter()
        .position(|entry| *entry == ("release", key_up(KEY_B)))
        .unwrap();
    let taps = entries[released + 1..]
        .iter()
        .filter(|(_, call)| *call == tap);
    assert_eq!(taps.count(), 1, "the new tap follows the key-up");
}

/// Send exclusion (c): a panic inside the send of a tap leaves the key-down
/// marked before the call in the bitmap (no usable count), and the busy
/// guard releases it.
#[test]
fn a_panic_inside_a_tap_keeps_the_key_until_it_is_released() {
    let mut rig = ExecRig::new(vec![press_b()]);
    let core = Arc::downgrade(&rig.core);
    let held_at_panic = Arc::new(std::sync::Mutex::new(None));
    let seen = Arc::clone(&held_at_panic);
    let fired = AtomicBool::new(false);
    rig.injector.on_call(move |call| {
        if *call == InjectCall::Keys(vec![KEY_B]) && !fired.swap(true, Ordering::SeqCst) {
            let held = core.upgrade().map(|core| core.held_keys());
            *seen.lock().unwrap_or_else(PoisonError::into_inner) = held;
            panic!("injector panicked inside SendInput");
        }
    });
    let failed = rig.run("kb");
    assert_eq!(failed.status, "failed");
    assert_eq!(
        *held_at_panic.lock().unwrap_or_else(PoisonError::into_inner),
        Some(vec![KEY_B])
    );
    let calls = rig.injector.calls();
    let tap = calls
        .iter()
        .position(|call| *call == InjectCall::Keys(vec![KEY_B]))
        .unwrap();
    assert_eq!(&calls[tap + 1..], &[key_up(KEY_B)]);
    assert!(rig.core.held_keys().is_empty());
    let entry = rig.last_entry();
    assert_eq!(
        (entry.outcome, entry.reason),
        (Outcome::Failed, ReasonCode::ExecutorPanic)
    );
    assert!(entry.side_effects);
}

/// Send exclusion (d) and fail closed: while the send lock stays held (a
/// send stuck inside SendInput) nothing is sent at all. The task's sends are
/// refused and it fails fast (ExecutorTimeout), its key-ups are owed, the
/// executor reports Down, every rebuild refuses until the supervisor gives
/// up (Failed), and manual output is refused too. Once the lock is free the
/// rebuild succeeds and output works again.
#[test]
fn nothing_is_sent_without_the_send_lock() {
    let mut rig = ExecRig::new(vec![text_rule("kw", OutputMethod::Keys, TYPED)]);
    let manual = MockInjector::new();
    let serialized = Serialized::new(&manual, Some(Arc::clone(&rig.core)));
    let core = Arc::clone(&rig.core);
    let stuck = core.hold_send_lock();
    assert!(rig.trigger("kw"));
    rig.wait_idle();
    assert!(rig.injector.calls().is_empty(), "the task sent nothing");
    let entry = rig.last_entry();
    assert_eq!(
        (entry.outcome, entry.reason),
        (Outcome::Failed, ReasonCode::ExecutorTimeout)
    );
    assert!(!entry.side_effects);
    assert!(rig.core.release_owed());
    assert!(matches!(
        rig.executor_status(),
        Status::Down(ReasonCode::ExecutorTimeout)
    ));
    assert_eq!(serialized.send_text("manual"), 0);
    assert!(manual.calls().is_empty(), "manual output refused too");

    rig.supervisor.tick();
    assert_eq!(rig.state(), HealthState::Recovering);
    for delay in BACKOFF {
        rig.clock.advance(delay);
        rig.supervisor.tick();
    }
    assert_eq!(rig.state(), HealthState::Failed);
    assert!(rig.injector.calls().is_empty());

    drop(stuck);
    rig.core.rebuild().expect("the lock is free again");
    assert!(!rig.core.release_owed());
    assert!(matches!(rig.executor_status(), Status::Ok));
    let result = rig.run("kw");
    assert_eq!(result.status, "unknown", "{}", result.message);
    assert_eq!(rig.injector.typed(), TYPED);
    assert_eq!(serialized.send_text("manual"), 12);
    assert_eq!(manual.calls(), vec![InjectCall::Text("manual".into())]);
}

const KEY_C: u16 = 0x43;

fn key_down(key: u16) -> InjectCall {
    InjectCall::Key { key, up: false }
}

/// Presses A, then B, taps C, releases B and A.
fn hold_a_and_b() -> Rule {
    steps_rule(
        "hold",
        vec![
            Operation::KeyDown { key: code(KEY_A) },
            Operation::KeyDown { key: code(KEY_B) },
            Operation::KeyPress { key: code(KEY_C) },
            Operation::KeyUp { key: code(KEY_B) },
            Operation::KeyUp { key: code(KEY_A) },
        ],
    )
}

/// Holds the rig's first worker at `BeforeSend` once the task port took
/// `after`: epoch checked, send lock not yet taken.
fn hold_before_send_after(rig: &ExecRig, after: InjectCall) -> Hold {
    let checked = Hold::new();
    let wait = checked.waiter();
    let injector = Arc::clone(&rig.injector);
    rig.core.on_pause(move |at| {
        if at == Pause::BeforeSend
            && is_thread("qt-executor-0")
            && injector.calls().contains(&after)
        {
            wait();
        }
    });
    checked
}

/// Runs `call` on a fresh thread; its value when it returned within
/// `limit`. A call that never returns leaves its thread blocked until the
/// test's gates open (on drop).
fn within<T: Send + 'static>(
    limit: Duration,
    call: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    let (done, finished) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = done.send(call());
    });
    finished.recv_timeout(limit).ok()
}

fn ups(port: &MockInjector, key: u16) -> usize {
    port.calls()
        .iter()
        .filter(|call| **call == key_up(key))
        .count()
}

/// Round 2, issue 2: key-ups `SendInput` refuses stay owed, and while they
/// are owed no input is sent at all. The rebuild returns Err (the supervisor
/// backs off and ends in Failed); the replacement worker's task fails with
/// no input; manual output is refused, recorded like a refused task. Once
/// `SendInput` accepts again, the next task pays first and then outputs as
/// usual, and repair heals the engine.
#[test]
fn refused_key_ups_stay_owed_and_no_input_is_sent_until_they_are_paid() {
    let mut rig = ExecRig::with_release(vec![
        hold_a_and_b(),
        text_rule("kw", OutputMethod::Keys, TYPED),
    ]);
    let journal = journal(&rig, |_| {});
    rig.release.refuse_all(true);
    let checked = hold_before_send_after(&rig, key_down(KEY_B));
    assert!(rig.trigger("hold"));
    checked.wait_arrived("the task, A and B pressed");
    assert!(rig.core.fence(rig.core.epoch()));
    drop(checked);
    assert!(rig.core.join_retired(Duration::from_secs(5)));
    assert_eq!(rig.core.held_keys(), vec![KEY_A, KEY_B]);
    assert!(rig.core.release_owed());

    // ① Refused: the rebuild fails; the replacement worker sends nothing.
    let error = rig.core.rebuild().expect_err("key-ups still refused");
    assert!(error.contains("still refused"), "{error}");
    assert_eq!(rig.core.held_keys(), vec![KEY_A, KEY_B]);
    assert!(rig.core.release_owed());
    assert!(matches!(
        rig.executor_status(),
        Status::Down(ReasonCode::ExecutorTimeout)
    ));
    let sent = rig.injector.calls();
    let refused = rig.run("kw");
    assert_eq!(refused.status, "failed");
    assert!(
        refused.message.contains("still owed"),
        "{}",
        refused.message
    );
    assert!(!refused.side_effects);
    assert_eq!(rig.injector.calls(), sent, "the task sent nothing");
    let entry = rig.last_entry();
    assert_eq!(
        (entry.rule_id.as_str(), entry.outcome, entry.reason),
        ("rule-kw", Outcome::Failed, ReasonCode::ExecutorTimeout)
    );
    assert!(!entry.side_effects);
    let manual = MockInjector::new();
    let serialized = Serialized::new(&manual, Some(Arc::clone(&rig.core)));
    assert_eq!(serialized.send_text("manual"), 0);
    assert!(manual.calls().is_empty(), "manual output refused");
    let process = "qt-owed-manual.exe";
    super::executor::record_manual(
        process,
        &Err("SendInput accepted 0/12 events".into()),
        Instant::now(),
    );
    let recorded = crate::runtime::current()
        .list_history(None)
        .into_iter()
        .rfind(|entry| entry.process.as_deref() == Some(process))
        .expect("the manual run is recorded");
    assert_eq!(
        (recorded.trigger, recorded.outcome, recorded.reason),
        (
            TriggerKind::Manual,
            Outcome::Failed,
            ReasonCode::ExecutorTimeout
        )
    );
    // Only key-ups ever reached the release port, every one refused.
    let releases = rig.release.calls();
    assert!(releases
        .iter()
        .all(|call| *call == key_up(KEY_A) || *call == key_up(KEY_B)));
    rig.supervisor.tick();
    assert_eq!(rig.state(), HealthState::Recovering);
    for delay in BACKOFF {
        rig.clock.advance(delay);
        rig.supervisor.tick();
    }
    assert_eq!(rig.state(), HealthState::Failed, "retries ran out");
    assert_eq!(rig.core.held_keys(), vec![KEY_A, KEY_B]);
    assert!(rig.injector.typed().is_empty());

    // ② Accepted again: the next task pays first, then outputs normally.
    rig.release.refuse_all(false);
    let result = rig.run("kw");
    assert_eq!(result.status, "unknown", "{}", result.message);
    assert_eq!(rig.injector.typed(), TYPED);
    assert!(rig.core.held_keys().is_empty());
    assert!(!rig.core.release_owed());
    let entries = journaled(&journal);
    let paid = entries
        .iter()
        .rposition(|(port, _)| *port == "release")
        .unwrap();
    let typed = entries
        .iter()
        .position(|(_, call)| matches!(call, InjectCall::Text(_)))
        .unwrap();
    assert!(paid < typed, "the payment comes first: {entries:?}");
    assert!(matches!(rig.executor_status(), Status::Ok));
    rig.supervisor.repair();
    rig.supervisor.tick();
    assert_eq!(rig.state(), HealthState::Healthy);
}

/// Round 2, issue 2 ③: a payment `SendInput` accepts only in part settles
/// nothing. The accepted key-up's bit is cleared; the refused key stays held
/// and owed (rebuild Err); the next payment sends only that key's key-up.
#[test]
fn a_partly_accepted_payment_keeps_the_refused_key_owed() {
    let mut rig = ExecRig::with_release(vec![hold_a_and_b()]);
    rig.release.refuse(&[KEY_B]);
    let checked = hold_before_send_after(&rig, key_down(KEY_B));
    assert!(rig.trigger("hold"));
    checked.wait_arrived("the task, A and B pressed");
    assert!(rig.core.fence(rig.core.epoch()));
    assert_eq!(rig.core.held_keys(), vec![KEY_A, KEY_B]);

    assert!(rig.core.rebuild().is_err(), "B refused: still owed");
    assert_eq!(rig.core.held_keys(), vec![KEY_B]);
    assert!(rig.core.release_owed());
    assert_eq!(ups(&rig.release, KEY_A), 1);
    assert!(ups(&rig.release, KEY_B) >= 1);
    assert!(matches!(
        rig.executor_status(),
        Status::Down(ReasonCode::ExecutorTimeout)
    ));

    rig.release.refuse(&[]);
    rig.core.rebuild().expect("B accepted: paid");
    assert!(rig.core.held_keys().is_empty());
    assert!(!rig.core.release_owed());
    assert_eq!(ups(&rig.release, KEY_A), 1, "A is not sent again");
    assert_eq!(rig.release.calls().last(), Some(&key_up(KEY_B)));
    assert!(matches!(rig.executor_status(), Status::Ok));
    drop(checked);
    assert!(rig.core.join_retired(Duration::from_secs(5)));
    assert!(!rig
        .injector
        .calls()
        .contains(&InjectCall::Keys(vec![KEY_C])));
}

/// Round 2, issue 3: the supervisor's calls never reach the injector. With
/// the release port blocking inside every key-up, `enforce_budget` (fencing
/// a stalled task) and `probe` return within 50 ms on real threads, sending
/// nothing. The rebuild, a supervisor attempt that may block, is the payer:
/// it blocks inside the key-up holding only the send lock, so the
/// supervisor's next attempt fails fast instead of hanging and no thread is
/// started (the queue stays closed). Unblocked, the rebuild completes and
/// the engine recovers.
#[test]
fn supervisor_calls_return_while_a_key_up_blocks() {
    const LIMIT: Duration = Duration::from_millis(50);
    let mut rig = ExecRig::with_release(vec![
        hold_a_and_b(),
        text_rule("kw", OutputMethod::Keys, TYPED),
    ]);
    let in_key_up = Hold::new();
    let wait = in_key_up.waiter();
    rig.release.on_call(move |_| wait());
    let checked = hold_before_send_after(&rig, key_down(KEY_B));
    assert!(rig.trigger("hold"));
    checked.wait_arrived("the task, A and B pressed");
    let epoch = rig.core.epoch();

    rig.clock.advance(STALL_BUDGET + Duration::from_secs(1));
    let (core, now) = (Arc::clone(&rig.core), rig.clock.now());
    assert!(
        within(LIMIT, move || core.enforce_budget(now)).is_some(),
        "enforce_budget returned"
    );
    assert_eq!(rig.core.epoch(), epoch + 1, "it fenced the stalled task");
    assert!(rig.core.release_owed());
    assert!(rig.release.calls().is_empty(), "and sent nothing");
    let (core, now) = (Arc::clone(&rig.core), rig.clock.now());
    let probed = within(LIMIT, move || core.probe(now)).expect("probe returned");
    assert!(probed.contains(&(
        Component::Executor,
        Status::Down(ReasonCode::ExecutorTimeout)
    )));

    // The rebuild pays, and blocks inside the first key-up.
    let core = Arc::clone(&rig.core);
    let rebuild = std::thread::spawn(move || core.rebuild());
    in_key_up.wait_arrived("the rebuild's key-up");
    let (core, now) = (Arc::clone(&rig.core), rig.clock.now());
    assert!(within(LIMIT, move || core.enforce_budget(now)).is_some());
    let (core, now) = (Arc::clone(&rig.core), rig.clock.now());
    assert!(within(LIMIT, move || core.probe(now)).is_some());
    rig.supervisor.tick();
    assert_eq!(rig.state(), HealthState::Recovering);
    rig.clock.advance(BACKOFF[0]);
    rig.supervisor.tick();
    assert_eq!(
        rig.state(),
        HealthState::Recovering,
        "that attempt failed fast"
    );
    assert!(!rig.trigger("kw"), "no worker was started meanwhile");
    assert_eq!(rig.last_entry().outcome, Outcome::Skipped);

    drop(in_key_up);
    rebuild
        .join()
        .unwrap()
        .expect("paid once SendInput returned");
    assert!(rig.core.held_keys().is_empty());
    assert!(!rig.core.release_owed());
    rig.clock.advance(BACKOFF[1]);
    rig.supervisor.tick();
    assert_eq!(rig.state(), HealthState::Healthy);
    let result = rig.run("kw");
    assert_eq!(result.status, "unknown", "{}", result.message);
    drop(checked);
    assert!(rig.core.join_retired(Duration::from_secs(5)));
    assert!(!rig
        .injector
        .calls()
        .contains(&InjectCall::Keys(vec![KEY_C])));
}

/// Sources of the failure texts the executor's reason table maps.
const INJECT_RS: &str = include_str!("inject.rs");
const ACTIONS_RS: &str = include_str!("actions.rs");
const CLIPBOARD_RS: &str = include_str!("clipboard.rs");
const PROTECTION_RS: &str = include_str!("protection.rs");
const RAWINPUT_RS: &str = include_str!("rawinput.rs");
const EXECUTOR_RS: &str = include_str!("executor.rs");

/// Every failure text the reason table maps, quoted verbatim (format
/// arguments as written) with the file that produces it and the reason it
/// must map to. A reworded text fails
/// `every_mapped_failure_text_is_quoted_from_its_source` (the quote is gone
/// from its source) instead of silently falling back to `Internal`. A new
/// failure text that should not be `Internal` belongs here too.
const ORIGINALS: &[(&str, &str, ReasonCode)] = &[
    (
        INJECT_RS,
        "SendInput accepted {accepted}/{requested} events (possibly elevated target / UIPI); partial effects and target consumption unknown",
        ReasonCode::InjectPartial,
    ),
    (
        INJECT_RS,
        "Physical key conflicts with release; action stopped",
        ReasonCode::ModifierHeld,
    ),
    (
        INJECT_RS,
        "Physical key conflicts with requested hotkey; action stopped",
        ReasonCode::ModifierHeld,
    ),
    (
        ACTIONS_RS,
        "Engine paused; input stopped",
        ReasonCode::Paused,
    ),
    (ACTIONS_RS, "Cancelled", ReasonCode::Cancelled),
    (
        ACTIONS_RS,
        "User input changed during action; stopped safely",
        ReasonCode::StaleInput,
    ),
    (
        ACTIONS_RS,
        "Foreground window/process changed; stopped safely",
        ReasonCode::FocusChanged,
    ),
    (
        ACTIONS_RS,
        "Physical modifier is held; input stopped",
        ReasonCode::ModifierHeld,
    ),
    (ACTIONS_RS, "Execution timeout", ReasonCode::ExecutorTimeout),
    (
        ACTIONS_RS,
        "Declared next target is not foreground",
        ReasonCode::FocusChanged,
    ),
    (
        ACTIONS_RS,
        "Automatic shortcut action changed its declared target; stopped safely",
        ReasonCode::FocusChanged,
    ),
    (
        ACTIONS_RS,
        "Requested key is physically held",
        ReasonCode::ModifierHeld,
    ),
    (
        ACTIONS_RS,
        "Physical mouse button conflicts with action",
        ReasonCode::ModifierHeld,
    ),
    (
        RAWINPUT_RS,
        "Focus, caret, layout or configuration changed; input stopped",
        ReasonCode::StaleInput,
    ),
    (
        PROTECTION_RS,
        "Password controls are blocked",
        ReasonCode::ProtectedControl,
    ),
    (
        PROTECTION_RS,
        "Read-only text control blocked",
        ReasonCode::ProtectedControl,
    ),
    (
        PROTECTION_RS,
        "Disabled controls are blocked",
        ReasonCode::ProtectedControl,
    ),
    (
        PROTECTION_RS,
        "Read-only ValuePattern control blocked",
        ReasonCode::ProtectedControl,
    ),
    (
        PROTECTION_RS,
        "Read-only or protected accessibility control blocked",
        ReasonCode::ProtectedControl,
    ),
    (CLIPBOARD_RS, "Clipboard is busy", ReasonCode::ClipboardBusy),
    (
        CLIPBOARD_RS,
        "Clipboard is busy; trigger preserved",
        ReasonCode::ClipboardBusy,
    ),
    (
        CLIPBOARD_RS,
        "Clipboard owner changed externally; clipboard preserved",
        ReasonCode::ClipboardChanged,
    ),
    (
        CLIPBOARD_RS,
        "Clipboard formats changed externally; clipboard preserved",
        ReasonCode::ClipboardChanged,
    ),
    (
        CLIPBOARD_RS,
        "Clipboard payload changed externally; clipboard preserved",
        ReasonCode::ClipboardChanged,
    ),
    (
        CLIPBOARD_RS,
        "Clipboard changed externally before paste; no paste was sent",
        ReasonCode::ClipboardChanged,
    ),
    (
        CLIPBOARD_RS,
        "Clipboard changed externally; stopped without overwriting it",
        ReasonCode::ClipboardChanged,
    ),
    (
        CLIPBOARD_RS,
        "Existing clipboard contains handle-based rich formats that cannot be safely restored; trigger preserved",
        ReasonCode::ClipboardFormatSkipped,
    ),
    (
        CLIPBOARD_RS,
        "Clipboard format cannot be safely backed up; trigger preserved",
        ReasonCode::ClipboardFormatSkipped,
    ),
    (
        CLIPBOARD_RS,
        "Existing clipboard has an unsupported handle format",
        ReasonCode::ClipboardFormatSkipped,
    ),
    (
        EXECUTOR_RS,
        "Executor task was fenced after a stall; stopped without further input",
        ReasonCode::ExecutorTimeout,
    ),
    (
        EXECUTOR_RS,
        "Key-ups of a stopped action are still owed; refused without input",
        ReasonCode::ExecutorTimeout,
    ),
    (
        EXECUTOR_RS,
        "Executor task panicked; stopped",
        ReasonCode::ExecutorPanic,
    ),
    (
        EXECUTOR_RS,
        "Clipboard is still held by a stalled action; paste refused (no typing fallback)",
        ReasonCode::ClipboardBusy,
    ),
];

/// The reason table against the texts it depends on: each quoted text is
/// still a whole string literal of its source and maps to its reason,
/// whether it comes back as an error or as a failed result.
#[test]
fn every_mapped_failure_text_is_quoted_from_its_source() {
    for (source, text, want) in ORIGINALS {
        assert!(
            source.contains(&format!("\"{text}\"")),
            "{text:?} is no longer produced verbatim: reword the reason table and ORIGINALS with it"
        );
        assert_ne!(*want, ReasonCode::Internal, "{text:?}");
        let failed = ExecutionResult {
            id: String::new(),
            status: "failed".into(),
            message: (*text).into(),
            completed_steps: 0,
            side_effects: false,
        };
        assert_eq!(
            classify(&Ok(failed), Stage::Done, false).reason,
            *want,
            "{text:?}"
        );
        assert_eq!(
            classify(&Err((*text).into()), Stage::Output, false).reason,
            *want,
            "{text:?}"
        );
    }
}

/// A cancel landing in the delay before the first backspace: the erase
/// stage returns a failed result ("Cancelled"); it is recorded as
/// Cancelled, with nothing sent.
#[test]
fn a_cancel_in_the_delay_before_erasing_is_cancelled_with_nothing_sent() {
    let mut rig = ExecRig::new(vec![text_rule("brb", OutputMethod::Keys, BODY)]);
    let (shared, injector) = (Arc::clone(&rig.shared), Arc::clone(&rig.injector));
    let in_delay = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&in_delay);
    rig.on_sleep(move |_| {
        if super::actions::current_stage() == Stage::Erase && injector.calls().is_empty() {
            shared.cancel.store(true, Ordering::SeqCst);
            flag.store(true, Ordering::SeqCst);
        }
    });
    let result = rig.run("brb");
    assert!(
        in_delay.load(Ordering::SeqCst),
        "the cancel landed in the delay"
    );
    assert_eq!(
        (
            result.status.as_str(),
            result.message.as_str(),
            result.side_effects
        ),
        ("failed", "Cancelled", false)
    );
    assert!(rig.injector.calls().is_empty(), "no backspace, no output");
    let entry = rig.last_entry();
    assert_eq!(
        (entry.outcome, entry.reason),
        (Outcome::Cancelled, ReasonCode::Cancelled)
    );
    assert!(!entry.side_effects);
}

/// A cancel landing between two backspaces: Cancelled as well, and the
/// backspace already sent stays a side effect.
#[test]
fn a_cancel_between_backspaces_is_cancelled_and_keeps_its_side_effect() {
    let mut rig = ExecRig::new(vec![text_rule("brb", OutputMethod::Keys, BODY)]);
    let shared = Arc::clone(&rig.shared);
    rig.injector.on_call(move |call| {
        if *call == InjectCall::Keys(vec![VK_BACK]) {
            shared.cancel.store(true, Ordering::SeqCst);
        }
    });
    let result = rig.run("brb");
    assert_eq!(
        (
            result.status.as_str(),
            result.message.as_str(),
            result.side_effects
        ),
        ("failed", "Cancelled", true)
    );
    assert_eq!(
        rig.injector.calls(),
        vec![InjectCall::Keys(vec![VK_BACK])],
        "one backspace, then nothing"
    );
    let entry = rig.last_entry();
    assert_eq!(
        (entry.outcome, entry.reason),
        (Outcome::Cancelled, ReasonCode::Cancelled)
    );
    assert!(entry.side_effects);
}

/// Sets `key` in the physical key mirror (the hooks' view of the user's
/// keyboard).
fn set_physical(key: u16, held: bool) {
    let mirror = keystate::PHYSICAL
        .get()
        .expect("the rig installed the mirror");
    let mut keys = mirror.lock().unwrap_or_else(PoisonError::into_inner);
    if held {
        keys.insert(key);
    } else {
        keys.remove(&key);
    }
}
/// Lets go of a physical key when dropped, even when the test fails.
struct LetGo(u16);
impl Drop for LetGo {
    fn drop(&mut self) {
        set_physical(self.0, false);
    }
}
fn ctrl_b() -> Operation {
    Operation::Hotkey {
        keys: vec![Key::named("CTRL"), code(KEY_B)],
    }
}

/// inject.rs refuses a key the user holds ("Physical key conflicts with
/// ..."): ModifierHeld on every real path. A manual hotkey refused before
/// anything was sent is Skipped; automatic runs that already erased their
/// trigger (or pressed a key) are Failed.
#[test]
fn physical_key_conflicts_are_modifier_held_on_the_real_paths() {
    let mut rig = ExecRig::new(vec![
        steps_rule("hk", vec![ctrl_b()]),
        steps_rule(
            "kr",
            vec![
                Operation::KeyDown { key: code(KEY_B) },
                Operation::KeyUp { key: code(KEY_B) },
            ],
        ),
    ]);
    let _let_go = LetGo(KEY_B);
    set_physical(KEY_B, true);

    // Manual run (actions::execute without its Windows ports): the hotkey
    // is refused before any input.
    super::actions::begin_trace();
    let manual = Serialized::new(&*rig.injector, Some(Arc::clone(&rig.core)));
    let clipboard = rig.clipboard.clone();
    let result = super::actions::execute_with_proof(
        &Ports {
            injector: &manual,
            foreground: &rig.foreground,
            clock: &*rig.clock,
            clipboard: move || Ok(clipboard.clone()),
        },
        &rig.shared,
        ActionPlan {
            steps: vec![Step::new(ctrl_b(), SourceSpan::default())],
            ..Default::default()
        },
        MockForeground::window(FOCUS, APP),
        rig.shared.generation.load(Ordering::SeqCst),
        None,
        None,
        1.0,
    );
    let before = crate::runtime::history_seq();
    super::executor::record_manual(APP, &result, Instant::now());
    let run = result.expect("the plan ran");
    assert_eq!(
        (run.status.as_str(), run.message.as_str(), run.side_effects),
        (
            "failed",
            "Physical key conflicts with requested hotkey; action stopped",
            false
        )
    );
    assert!(rig.injector.calls().is_empty(), "nothing was sent");
    let entry = crate::runtime::list_history(Some(before))
        .pop()
        .expect("the manual run was recorded");
    assert_eq!(entry.trigger, TriggerKind::Manual);
    assert_eq!(
        (entry.outcome, entry.reason, entry.side_effects),
        (Outcome::Skipped, ReasonCode::ModifierHeld, false)
    );

    // Automatic hotkey: the trigger was erased, then the chord refused.
    let result = rig.run("hk");
    assert!(
        result.message.starts_with("Physical key conflicts"),
        "{}",
        result.message
    );
    assert_eq!(
        rig.injector.calls(),
        vec![InjectCall::Keys(vec![VK_BACK]); 2],
        "no chord"
    );
    let entry = rig.last_entry();
    assert_eq!(
        (entry.outcome, entry.reason, entry.side_effects),
        (Outcome::Failed, ReasonCode::ModifierHeld, true)
    );

    // Automatic release: the user takes over B after the plan pressed it,
    // so neither the plan nor the executor releases it.
    set_physical(KEY_B, false);
    rig.injector.on_call(|call| {
        if *call
            == (InjectCall::Key {
                key: KEY_B,
                up: false,
            })
        {
            set_physical(KEY_B, true);
        }
    });
    let sent = rig.injector.calls().len();
    let result = rig.run("kr");
    assert_eq!(
        result.message,
        "Physical key conflicts with release; action stopped"
    );
    assert_eq!(
        rig.injector.calls()[sent..],
        [
            InjectCall::Keys(vec![VK_BACK]),
            InjectCall::Keys(vec![VK_BACK]),
            InjectCall::Key {
                key: KEY_B,
                up: false
            },
        ],
        "no key-up of the user's key"
    );
    let entry = rig.last_entry();
    assert_eq!(
        (entry.outcome, entry.reason, entry.side_effects),
        (Outcome::Failed, ReasonCode::ModifierHeld, true)
    );
}
