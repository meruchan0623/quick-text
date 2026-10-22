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
    classify, run_candidate, stage_timings, Candidate, ExecutorCore, Runner, TaskScope,
    STALL_BUDGET,
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
use crate::traits::Clock;
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

/// The executor runner on mock ports (production: `run_on_windows`).
fn mock_runner(
    injector: &Arc<MockInjector>,
    foreground: &MockForeground,
    clock: &Arc<FakeClock>,
    clipboard: &MockClipboard,
) -> Runner {
    let (injector, foreground, clock, clipboard) = (
        Arc::clone(injector),
        foreground.clone(),
        Arc::clone(clock),
        clipboard.clone(),
    );
    Arc::new(move |candidate: &Candidate, scope: &TaskScope| {
        let fenced = scope.fence(&*injector);
        run_candidate(
            &Ports {
                injector: &fenced,
                foreground: &foreground,
                clock: &*clock,
                clipboard: || scope.claim_clipboard(|| Ok(clipboard.clone())),
            },
            scope.shared(),
            candidate,
            VariableContext::default(),
        )
    })
}

/// One engine: a real executor service and supervisor over mock ports.
struct ExecRig {
    shared: Arc<Shared>,
    buffer: Buffer,
    generation: u64,
    injector: Arc<MockInjector>,
    foreground: MockForeground,
    clipboard: MockClipboard,
    clock: Arc<FakeClock>,
    core: Arc<ExecutorCore>,
    runtime: Arc<Runtime>,
    supervisor: Arc<Supervisor>,
    _serial: MutexGuard<'static, ()>,
}
impl ExecRig {
    fn new(rules: Vec<Rule>) -> Self {
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
        let foreground = MockForeground::new(MockForeground::window(FOCUS, APP));
        let clock = Arc::new(FakeClock::new());
        let clipboard = MockClipboard::new(7, vec![(13, utf16("original"))]);
        let runtime = Arc::new(Runtime::new(clock.clone()));
        let core = ExecutorCore::new(
            Arc::clone(&shared),
            clock.clone(),
            mock_runner(&injector, &foreground, &clock, &clipboard),
            injector.clone(),
            STALL_BUDGET,
            Arc::clone(&runtime),
        );
        core.respawn().expect("worker starts");
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
            foreground,
            clipboard,
            clock,
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
/// epoch, re-sends the held key's MARK key-up from the bitmap and starts a
/// new worker; the stuck thread wakes up, sends nothing and exits.
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
    let fenced = rig.injector.calls();
    assert_eq!(
        fenced.last(),
        Some(&InjectCall::Key {
            key: KEY_A,
            up: true
        })
    );
    assert!(rig.core.held_keys().is_empty());
    assert!(!rig.shared.busy.load(Ordering::SeqCst));
    assert!(matches!(
        rig.executor_status(),
        Status::Down(ReasonCode::ExecutorTimeout)
    ));
    assert_eq!(rig.state(), HealthState::Recovering);
    rig.clock.advance(BACKOFF[0]);
    rig.supervisor.tick();
    assert_eq!(rig.state(), HealthState::Healthy);

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
