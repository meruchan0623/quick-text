//! L2 tests of manual runs under the executor's supervision (F5-1) on the
//! [`ExecRig`]: a manual run (`actions::execute`, here on the rig's mock
//! ports) claims `busy`, its run adopts the claim on a thread of its own,
//! and it is fenced like a task when it stalls: the epoch moves on, `busy`
//! is freed, the key-ups it left are owed, the fence records the timeout
//! and the supervisor rebuilds; the stalled thread, once awake, sends
//! nothing, records nothing and publishes nothing. Faults come from the
//! same `fault-injection` points as the automatic path (`ExecutorHang`,
//! `ExecutorPanic`).
#![cfg_attr(
    not(feature = "fault-injection"),
    allow(dead_code, reason = "helpers of the fault-injection tests")
)]
use super::*;
use std::thread::JoinHandle;

/// The fence's text, which a fenced manual run returns.
const FENCED_TEXT: &str = "Executor task was fenced after a stall; stopped without further input";

fn plan(operations: Vec<Operation>) -> ActionPlan {
    ActionPlan {
        steps: operations
            .into_iter()
            .map(|operation| Step::new(operation, SourceSpan::default()))
            .collect(),
        ..Default::default()
    }
}

/// [`body_then_hold_a`]'s plan, run manually: types the body, then holds A
/// around a tap of B.
fn hold_a_plan() -> ActionPlan {
    plan(vec![
        text(BODY),
        Operation::KeyDown { key: code(KEY_A) },
        Operation::KeyPress { key: code(KEY_B) },
        Operation::KeyUp { key: code(KEY_A) },
    ])
}

fn busy(rig: &ExecRig) -> bool {
    rig.shared.busy.load(Ordering::SeqCst)
}

fn published(rig: &ExecRig) -> Option<ExecutionResult> {
    rig.shared
        .last
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

/// What a manual run uses of the rig, movable onto a thread of its own.
#[derive(Clone)]
struct ManualPorts {
    core: Arc<ExecutorCore>,
    shared: Arc<Shared>,
    injector: Arc<MockInjector>,
    foreground: MockForeground,
    clock: Arc<FakeClock>,
    clipboard: MockClipboard,
    own: &'static keystate::OwnInput,
}

impl ManualPorts {
    fn of(rig: &ExecRig) -> Self {
        Self {
            core: Arc::clone(&rig.core),
            shared: Arc::clone(&rig.shared),
            injector: Arc::clone(&rig.injector),
            foreground: rig.foreground.clone(),
            clock: Arc::clone(&rig.clock),
            clipboard: rig.clipboard.clone(),
            own: rig.own,
        }
    }

    /// `execute_action`'s claim on `busy` ([`ManualBusy::claim`]).
    fn claim(&self) -> Option<ManualBusy> {
        ManualBusy::claim_on(Some(Arc::clone(&self.core)), &self.shared)
    }

    /// `actions::execute` on the mock ports: the supervised run
    /// (`run_manual`) whose injector and clipboard port are made inside the
    /// run, as `execute_inner` makes them.
    fn run(&self, plan: ActionPlan) -> Result<ExecutionResult, String> {
        let generation = self.shared.generation.load(Ordering::SeqCst);
        super::executor::run_manual(Some(Arc::clone(&self.core)), APP, || {
            let injector = Serialized::new(&*self.injector, Some(Arc::clone(&self.core)));
            let clipboard = self.clipboard.clone();
            super::actions::run_plan(
                &Ports {
                    injector: &injector,
                    foreground: &self.foreground,
                    clock: &*self.clock,
                    clipboard: move || super::executor::manual_clipboard(|| Ok(clipboard.clone())),
                },
                &self.shared,
                plan,
                MockForeground::window(FOCUS, APP),
                generation,
                None,
                None,
                1.0,
            )
        })
    }

    /// Claims `busy` here, then runs `plan` on a thread of its own, which
    /// gives the claim back once the run returned (as `execute_action`
    /// drops it after its blocking task).
    fn spawn(&self, plan: ActionPlan) -> JoinHandle<Result<ExecutionResult, String>> {
        let claim = self.claim().expect("busy is free");
        let ports = self.clone();
        std::thread::Builder::new()
            .name("qt-manual".into())
            .spawn(move || {
                let _track = keystate::track_own_input(ports.own);
                let result = ports.run(plan);
                drop(claim);
                result
            })
            .expect("the manual thread starts")
    }
}

/// F5-1, the model case: a manual run stuck in a send is fenced like a
/// task once its heartbeat is older than the budget, and not before: the
/// epoch moves on, `busy` is freed, the fence records the run as timed out,
/// the held key's MARK key-up is owed (the tick sends nothing), the
/// executor reports Down and Health Recovering; the rebuild pays the key-up
/// and automatic runs work again. Woken up, the stalled thread sends
/// nothing, records nothing, publishes nothing and returns the fence's
/// error.
#[cfg(feature = "fault-injection")]
#[test]
fn hung_manual_run_is_fenced_and_automatic_runs_take_over() {
    assert!(
        capture::install(),
        "test collector is the global subscriber"
    );
    let mut rig = ExecRig::new(vec![text_rule("kw", OutputMethod::Keys, TYPED)]);
    let manual = ManualPorts::of(&rig);
    let epoch = rig.core.epoch();
    let mark = capture::mark();
    let armed = rig.stall_after(key_down(KEY_A), 1_000);
    let run = manual.spawn(hold_a_plan());
    rig.wait_stalled(&armed);
    assert_eq!(rig.core.held_keys(), vec![KEY_A]);
    assert!(busy(&rig), "the manual run holds busy");

    rig.clock.advance(STALL_BUDGET - Duration::from_secs(1));
    rig.supervisor.tick();
    assert_eq!(rig.core.epoch(), epoch, "still within the budget");
    assert_eq!(rig.state(), HealthState::Healthy);
    rig.clock.advance(Duration::from_secs(2));
    rig.supervisor.tick();
    assert_eq!(rig.core.epoch(), epoch + 1, "fenced");
    assert!(!busy(&rig), "the fence freed busy");
    // The fence records the stalled run on its behalf.
    let entry = rig.last_entry();
    assert_eq!(
        (entry.trigger, entry.outcome, entry.reason),
        (
            TriggerKind::Manual,
            Outcome::Failed,
            ReasonCode::ExecutorTimeout
        )
    );
    assert_eq!(entry.process.as_deref(), Some(APP));
    assert!(entry.side_effects, "the body and A's key-down were sent");
    // The supervisor's tick sent nothing: the key-up is owed.
    assert_eq!(rig.core.held_keys(), vec![KEY_A]);
    assert!(rig.core.release_owed());
    assert!(!rig.injector.calls().contains(&key_up(KEY_A)));
    assert!(matches!(
        rig.executor_status(),
        Status::Down(ReasonCode::ExecutorTimeout)
    ));
    assert_eq!(rig.state(), HealthState::Recovering);
    let events = capture::since(mark);
    for name in ["executor.stalled", "executor.fenced"] {
        let event = events
            .iter()
            .find(|event| event.event == name)
            .unwrap_or_else(|| panic!("{name} is logged"));
        assert_eq!(event.field("manual"), Some("true"), "{name}");
    }
    assert!(events.iter().all(|event| !event.mentions(BODY)));

    rig.clock.advance(BACKOFF[0]);
    rig.supervisor.tick();
    assert_eq!(rig.state(), HealthState::Healthy);
    let paid = rig.injector.calls();
    assert_eq!(paid.last(), Some(&key_up(KEY_A)), "the rebuild paid it");
    assert!(rig.core.held_keys().is_empty());
    assert!(!rig.core.release_owed());

    let returned = run.join().expect("the manual thread returns");
    assert_eq!(returned.err().as_deref(), Some(FENCED_TEXT));
    assert_eq!(rig.injector.calls(), paid, "the woken thread sent nothing");
    assert_eq!(rig.history().len(), 1, "the woken thread recorded nothing");
    assert!(published(&rig).is_none(), "and published nothing");
    assert!(!busy(&rig), "its claim, given back late, freed nothing");

    rig.injector.on_call(|_| {});
    let result = rig.run("kw");
    assert_eq!(result.status, "unknown", "{}", result.message);
    assert!(rig.injector.typed().ends_with(TYPED));
    let entry = rig.last_entry();
    assert_eq!(
        (entry.rule_id.as_str(), entry.outcome),
        ("rule-kw", Outcome::Succeeded)
    );
    assert_eq!(rig.history().len(), 2);
    assert!(manual.claim().is_some(), "a new manual run may claim busy");
}

/// F5-1: a manual run fenced while its send is still inside the injector
/// (the send lock held) is recorded with side effects: that send is in
/// flight and may reach the target. Its plan is that one send, so when it
/// returns nothing is left to send; whatever the run then reports, it
/// writes no entry (no success), publishes no result and returns the
/// fence's error: nothing contradicts the fence's entry. Only the send that
/// was in flight reached the port.
#[test]
fn a_manual_run_returning_after_its_fence_records_no_success() {
    let mut rig = ExecRig::new(vec![text_rule("kw", OutputMethod::Keys, TYPED)]);
    let manual = ManualPorts::of(&rig);
    let epoch = rig.core.epoch();
    let in_send = hold_first_send(&rig.injector);
    let run = manual.spawn(plan(vec![text("x")]));
    in_send.wait_arrived("the manual run's send");

    rig.clock.advance(STALL_BUDGET + Duration::from_secs(1));
    rig.supervisor.tick();
    assert_eq!(rig.core.epoch(), epoch + 1, "fenced");
    assert!(!busy(&rig));
    let entry = rig.last_entry();
    assert_eq!(
        (entry.trigger, entry.outcome, entry.reason),
        (
            TriggerKind::Manual,
            Outcome::Failed,
            ReasonCode::ExecutorTimeout
        )
    );
    assert!(entry.side_effects, "a send was in flight");
    assert_eq!(rig.state(), HealthState::Recovering);

    drop(in_send);
    let returned = run.join().expect("the manual thread returns");
    assert_eq!(returned.err().as_deref(), Some(FENCED_TEXT));
    assert_eq!(
        rig.injector.calls(),
        vec![InjectCall::Text("x".into())],
        "only the send in flight"
    );
    assert_eq!(rig.history().len(), 1, "no success after the timeout");
    assert!(published(&rig).is_none());

    rig.clock.advance(BACKOFF[0]);
    rig.supervisor.tick();
    assert_eq!(rig.state(), HealthState::Healthy);
    let result = rig.run("kw");
    assert_eq!(result.status, "unknown", "{}", result.message);
    assert_eq!(rig.last_entry().outcome, Outcome::Succeeded);
}

/// F5-1: the panic points act on manual runs too. A manual run that panics
/// is caught, returns the panic's error and is recorded once as
/// `ExecutorPanic` (location and kind logged, no content); its end releases
/// the key the panic left pressed; the executor reports Down, so the
/// supervisor rebuilds it, and automatic runs keep working.
#[cfg(feature = "fault-injection")]
#[test]
fn a_manual_run_that_panics_releases_its_keys_and_reports_down() {
    assert!(
        capture::install(),
        "test collector is the global subscriber"
    );
    let mut rig = ExecRig::new(vec![text_rule("kw", OutputMethod::Keys, TYPED)]);
    let manual = ManualPorts::of(&rig);
    let mark = capture::mark();
    let armed = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&armed);
    rig.injector.on_call(move |call| {
        if *call == key_down(KEY_A) && !flag.swap(true, Ordering::SeqCst) {
            fault::debug_inject_fault(fault::FaultPoint::ExecutorPanic, 0);
        }
    });

    let claim = manual.claim().expect("busy is free");
    let returned = manual.run(hold_a_plan());
    assert_eq!(
        returned.err().as_deref(),
        Some("Executor task panicked; stopped")
    );
    assert_eq!(rig.history().len(), 1);
    let entry = rig.last_entry();
    assert_eq!(
        (entry.trigger, entry.outcome, entry.reason),
        (
            TriggerKind::Manual,
            Outcome::Failed,
            ReasonCode::ExecutorPanic
        )
    );
    assert!(entry.side_effects);
    // The panic skipped the plan's own cleanup: the run's end released A.
    assert_eq!(rig.injector.calls().last(), Some(&key_up(KEY_A)));
    assert!(rig.core.held_keys().is_empty());
    assert!(busy(&rig), "the claim keeps busy until it is given back");
    drop(claim);
    assert!(!busy(&rig));
    let events = capture::since(mark);
    let panics: Vec<_> = events
        .iter()
        .filter(|event| event.event == "executor.panic")
        .collect();
    assert_eq!(panics.len(), 1);
    assert_eq!(panics[0].field("manual"), Some("true"));
    assert_eq!(panics[0].field("stage"), Some("output"));
    assert_eq!(panics[0].field("kind"), Some("str"));
    assert!(events.iter().all(|event| !event.mentions(BODY)));

    assert!(matches!(
        rig.executor_status(),
        Status::Down(ReasonCode::ExecutorPanic)
    ));
    rig.supervisor.tick();
    assert_eq!(rig.state(), HealthState::Recovering);
    rig.clock.advance(BACKOFF[0]);
    rig.supervisor.tick();
    assert_eq!(rig.state(), HealthState::Healthy);
    let result = rig.run("kw");
    assert_eq!(result.status, "unknown", "{}", result.message);
    assert_eq!(rig.last_entry().outcome, Outcome::Succeeded);
}

/// F5-1: a supervised manual run that ends in time records one entry, its
/// real outcome, and publishes its result. The heartbeat starts at the
/// claim and restarts when the run adopts it, so the time before the run
/// (`execute_action`'s delay and selection capture) counts against the
/// budget only until the run starts. Settled, the run is no longer a
/// suspect: its claim may be given back late without a fence.
#[test]
fn a_manual_run_in_time_records_its_outcome_once() {
    let mut rig = ExecRig::new(vec![text_rule("kw", OutputMethod::Keys, TYPED)]);
    let manual = ManualPorts::of(&rig);
    let epoch = rig.core.epoch();
    let claim = manual.claim().expect("busy is free");
    rig.clock.advance(STALL_BUDGET - Duration::from_secs(1));
    rig.supervisor.tick();
    assert_eq!(rig.core.epoch(), epoch, "within the budget since the claim");

    let result = manual.run(plan(vec![text(BODY)])).expect("the plan ran");
    assert_eq!(result.status, "unknown", "{}", result.message);
    assert_eq!(rig.injector.typed(), BODY);
    let history = rig.history();
    assert_eq!(history.len(), 1, "one entry");
    let entry = &history[0];
    assert_eq!(
        (entry.trigger, entry.outcome, entry.reason),
        (TriggerKind::Manual, Outcome::Succeeded, ReasonCode::Ok)
    );
    assert_eq!(entry.process.as_deref(), Some(APP));
    assert!(entry.side_effects);
    let last = published(&rig).expect("the run published its result");
    assert_eq!(
        (last.status.as_str(), last.message.as_str()),
        (result.status.as_str(), result.message.as_str())
    );

    rig.clock.advance(STALL_BUDGET * 2);
    rig.supervisor.tick();
    assert_eq!(rig.core.epoch(), epoch, "a settled run is no suspect");
    assert_eq!(rig.state(), HealthState::Healthy);
    assert!(busy(&rig), "busy is held until the claim is given back");
    drop(claim);
    assert!(!busy(&rig));
    assert_eq!(rig.history().len(), 1);
    let auto = rig.run("kw");
    assert_eq!(auto.status, "unknown", "{}", auto.message);
}

/// F5-1: a claim whose run has not started yet (`execute_action`'s delay
/// and selection capture) is supervised from the claim on: past the budget
/// the fence frees `busy` and records the manual run as timed out, with no
/// side effects. The claim stays as a fenced marker until it is given back:
/// its late run is refused before its plan, without input and without a
/// second entry; a new manual claim waits for the marker, automatic runs do
/// not.
#[test]
fn a_claim_fenced_before_its_run_refuses_that_run_without_input() {
    let mut rig = ExecRig::new(vec![text_rule("kw", OutputMethod::Keys, TYPED)]);
    let manual = ManualPorts::of(&rig);
    let epoch = rig.core.epoch();
    let claim = manual.claim().expect("busy is free");
    rig.clock.advance(STALL_BUDGET - Duration::from_secs(1));
    rig.supervisor.tick();
    assert_eq!(rig.core.epoch(), epoch, "within the budget since the claim");
    rig.clock.advance(Duration::from_secs(2));
    rig.supervisor.tick();
    assert_eq!(rig.core.epoch(), epoch + 1, "fenced");
    assert!(!busy(&rig), "the fence freed busy");
    let entry = rig.last_entry();
    assert_eq!(
        (
            entry.trigger,
            entry.outcome,
            entry.reason,
            entry.side_effects
        ),
        (
            TriggerKind::Manual,
            Outcome::Failed,
            ReasonCode::ExecutorTimeout,
            false
        )
    );
    assert!(matches!(
        rig.executor_status(),
        Status::Down(ReasonCode::ExecutorTimeout)
    ));
    assert_eq!(rig.state(), HealthState::Recovering);
    assert!(manual.claim().is_none(), "the marker holds off a new claim");

    let ran = AtomicBool::new(false);
    let late = super::executor::run_manual(Some(Arc::clone(&rig.core)), APP, || {
        ran.store(true, Ordering::SeqCst);
        RunReport::text(Err("the plan must not run".into()))
    });
    assert_eq!(late.err().as_deref(), Some(FENCED_TEXT));
    assert!(!ran.load(Ordering::SeqCst), "the plan never started");
    assert!(rig.injector.calls().is_empty(), "nothing was sent");
    assert_eq!(rig.history().len(), 1, "the fence wrote the only entry");

    rig.clock.advance(BACKOFF[0]);
    rig.supervisor.tick();
    assert_eq!(rig.state(), HealthState::Healthy);
    let result = rig.run("kw");
    assert_eq!(result.status, "unknown", "{}", result.message);
    drop(claim);
    assert!(!busy(&rig), "the marker's end leaves busy alone");
    let again = manual.claim().expect("the marker is gone");
    assert!(busy(&rig));
    drop(again);
    assert!(!busy(&rig));
}

/// F5-1: a supervised run that finds no claim of its own to adopt is
/// refused before its plan, sends nothing and is recorded as a failed
/// manual run; `busy` and the epoch are left alone.
#[test]
fn a_manual_run_without_a_claim_is_refused_and_recorded() {
    let rig = ExecRig::new(vec![]);
    let epoch = rig.core.epoch();
    let ran = AtomicBool::new(false);
    let refused = super::executor::run_manual(Some(Arc::clone(&rig.core)), APP, || {
        ran.store(true, Ordering::SeqCst);
        RunReport::text(Err("the plan must not run".into()))
    });
    assert_eq!(
        refused.err().as_deref(),
        Some("Manual run holds no claim on the executor; refused without input")
    );
    assert!(!ran.load(Ordering::SeqCst), "the plan never started");
    assert!(rig.injector.calls().is_empty());
    let entry = rig.last_entry();
    assert_eq!(
        (
            entry.trigger,
            entry.outcome,
            entry.reason,
            entry.side_effects
        ),
        (
            TriggerKind::Manual,
            Outcome::Failed,
            ReasonCode::Internal,
            false
        )
    );
    assert_eq!(entry.process.as_deref(), Some(APP));
    assert_eq!(rig.core.epoch(), epoch);
    assert!(!busy(&rig));
    assert!(matches!(rig.executor_status(), Status::Ok));
}

/// F5-1, clipboard: a manual paste fenced while its lease is open keeps the
/// clipboard claim until that lease drops, as a fenced task does
/// (no-regression #2): meanwhile automatic paste fails with nothing sent
/// and no typing fallback, the clipboard reports Degraded(ClipboardBusy)
/// and key output keeps working. Woken up, the manual run sends nothing;
/// its lease restores the user's clipboard and gives the claim back.
#[cfg(feature = "fault-injection")]
#[test]
fn clipboard_held_by_a_fenced_manual_run_refuses_paste_without_typing() {
    let mut rig = ExecRig::new(vec![
        text_rule("btw", OutputMethod::Paste, BODY),
        text_rule("kw", OutputMethod::Keys, TYPED),
    ]);
    let manual = ManualPorts::of(&rig);
    // The run's first send, made with its lease open, stalls.
    fault::debug_inject_fault(fault::FaultPoint::ExecutorHang, 1_500);
    let run = manual.spawn(plan(vec![Operation::Text {
        text: BODY.into(),
        method: OutputMethod::Paste,
    }]));
    rig.wait_stalled(&AtomicBool::new(true));
    assert!(rig.injector.calls().is_empty());
    rig.clock.advance(STALL_BUDGET + Duration::from_secs(1));
    rig.supervisor.tick();
    assert_eq!(rig.state(), HealthState::Recovering);
    let entry = rig.last_entry();
    assert_eq!(
        (entry.trigger, entry.outcome, entry.reason),
        (
            TriggerKind::Manual,
            Outcome::Failed,
            ReasonCode::ExecutorTimeout
        )
    );
    rig.clock.advance(BACKOFF[0]);
    rig.supervisor.tick();
    assert_eq!(rig.state(), HealthState::Degraded);
    assert_eq!(
        rig.component(Component::Clipboard),
        Some((HealthState::Degraded, Some(ReasonCode::ClipboardBusy)))
    );

    let refused = rig.run("btw");
    assert_eq!(refused.status, "failed");
    assert!(
        refused.message.contains("no typing fallback"),
        "{}",
        refused.message
    );
    assert!(rig.injector.calls().is_empty(), "no erase, no typing");
    let entry = rig.last_entry();
    assert_eq!(
        (entry.outcome, entry.reason),
        (Outcome::Failed, ReasonCode::ClipboardBusy)
    );
    assert!(!entry.side_effects);
    let typed = rig.run("kw");
    assert_eq!(typed.status, "unknown", "{}", typed.message);
    assert_eq!(rig.injector.typed(), TYPED);

    let sent = rig.injector.calls();
    let returned = run.join().expect("the manual thread returns");
    assert_eq!(returned.err().as_deref(), Some(FENCED_TEXT));
    assert_eq!(rig.injector.calls(), sent, "the woken run sent nothing");
    assert_eq!(
        rig.clipboard.content(),
        vec![(13, utf16("original"))],
        "its lease restored the user's clipboard"
    );
    rig.supervisor.tick();
    assert_eq!(rig.state(), HealthState::Healthy);
    let pasted = rig.run("btw");
    assert_eq!(pasted.status, "unknown", "{}", pasted.message);
}
