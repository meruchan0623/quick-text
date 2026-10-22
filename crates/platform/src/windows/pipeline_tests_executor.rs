//! L2 tests for the executor service (B-01, S-01) on mock ports and a fake
//! clock: panic isolation, the epoch fence on a stalled task, held-key
//! release, the clipboard ruling and supervisor recovery. Faults come from
//! the `fault-injection` points (`ExecutorPanic`, `ExecutorHang`).
#![cfg_attr(
    not(feature = "fault-injection"),
    allow(
        dead_code,
        unused_imports,
        reason = "helpers of the fault-injection tests"
    )
)]
use super::actions::Ports;
use super::executor::{run_candidate, Candidate, ExecutorCore, Runner, TaskScope, STALL_BUDGET};
use super::pipeline_tests::SERIAL;
use super::rawinput::{detect_candidate, Buffer, Event, Kind};
use super::*;
use crate::fault;
use crate::health::{Component, ComponentHealth, HealthState, ReasonCode};
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
        let core = ExecutorCore::new(
            Arc::clone(&shared),
            clock.clone(),
            mock_runner(&injector, &foreground, &clock, &clipboard),
            injector.clone(),
            STALL_BUDGET,
        );
        core.respawn().expect("worker starts");
        let runtime = Arc::new(Runtime::new(clock.clone()));
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
        self.buffer = Buffer::default();
        *self
            .shared
            .last
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
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
        self.core
            .enqueue(candidate.expect("abbreviation matches its rule"))
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
    rig.injector.on_call(|_| {});
    let result = rig.run("btw");
    assert_eq!(result.status, "unknown", "{}", result.message);
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
}
