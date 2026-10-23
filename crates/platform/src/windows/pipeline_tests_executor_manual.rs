//! L2 tests of manual runs under the executor's supervision (F5-1) on the
//! [`ExecRig`]: a manual run (`actions::execute`, here on the rig's mock
//! ports) claims `busy`, its run adopts the claim on a thread of its own,
//! and it is fenced like a task when it stalls: the epoch moves on, `busy`
//! is freed, the key-ups it left are owed, the fence records the timeout
//! and the supervisor rebuilds; the stalled thread, once awake, sends
//! nothing, records nothing and publishes nothing. Faults come from the
//! same `fault-injection` points as the automatic path (`ExecutorHang`,
//! `ExecutorPanic`).
//!
//! The blocking thread acts with the claim's [`ManualTicket`]; the claim
//! itself ([`ManualBusy`]) stays with whoever made it, as it stays with the
//! command in production.
#![cfg_attr(
    not(feature = "fault-injection"),
    allow(dead_code, reason = "helpers of the fault-injection tests")
)]
use super::executor::ManualError;
use super::*;
use std::thread::JoinHandle;

/// The fence's text, which a fenced manual run returns.
const FENCED_TEXT: &str = "Executor task was fenced after a stall; stopped without further input";

/// The text of a manual run's failure. It stays inside the crate: what
/// leaves it is [`ManualFailure`], the reason code only.
fn failure_text(result: Result<ExecutionResult, ManualError>) -> Option<String> {
    result.err().map(|error| error.text)
}

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

    /// `ManualTicket::execute` on the mock ports: the supervised run of the
    /// ticket's claim, whose injector and clipboard port are made inside the
    /// run, as `execute_inner` makes them.
    fn run(&self, ticket: &ManualTicket, plan: ActionPlan) -> Result<ExecutionResult, ManualError> {
        let generation = self.shared.generation.load(Ordering::SeqCst);
        ticket.run(APP, || {
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

    /// Claims `busy` here, then runs `plan` with the claim's ticket on a
    /// thread of its own, which gives the claim back once the run returned
    /// (as `execute_action` drops it once its blocking task returned).
    fn spawn(&self, plan: ActionPlan) -> JoinHandle<Result<ExecutionResult, ManualError>> {
        let claim = self.claim().expect("busy is free");
        let ticket = claim.ticket();
        let ports = self.clone();
        std::thread::Builder::new()
            .name("qt-manual".into())
            .spawn(move || {
                let _track = keystate::track_own_input(ports.own);
                let result = ports.run(&ticket, plan);
                drop(claim);
                result
            })
            .expect("the manual thread starts")
    }
}

/// Holds the thread `name` at the executor's pause point `at` until the
/// returned hold is dropped.
fn hold_at(rig: &ExecRig, at: Pause, name: &'static str) -> Hold {
    let hold = Hold::new();
    let wait = hold.waiter();
    rig.core.on_pause(move |point| {
        if point == at && is_thread(name) {
            wait();
        }
    });
    hold
}

/// Rebuilds the fenced executor through the supervisor (Recovering, then
/// Healthy after the first backoff).
fn recover(rig: &ExecRig) {
    rig.supervisor.tick();
    assert_eq!(rig.state(), HealthState::Recovering);
    rig.clock.advance(BACKOFF[0]);
    rig.supervisor.tick();
    assert_eq!(rig.state(), HealthState::Healthy);
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
    assert_eq!(failure_text(returned).as_deref(), Some(FENCED_TEXT));
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
    assert_eq!(failure_text(returned).as_deref(), Some(FENCED_TEXT));
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
    let returned = manual
        .run(&claim.ticket(), hold_a_plan())
        .expect_err("the run panicked");
    assert_eq!(
        (returned.reason, returned.text.as_str()),
        (ReasonCode::ExecutorPanic, "Executor task panicked; stopped")
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

    let result = manual
        .run(&claim.ticket(), plan(vec![text(BODY)]))
        .expect("the plan ran");
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
/// side effects, and the claim leaves the executor with it. Its ticket
/// reports the fence; a new manual claim is granted at once; the old
/// ticket's late run is refused before its plan, without input and without
/// a second entry; giving the old claim back frees nothing (the newer claim
/// keeps `busy`), and the newer claim runs once the executor is rebuilt.
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
    let ticket = claim.ticket();
    assert!(ticket.fenced());
    assert_eq!(ticket.progress().err().as_deref(), Some(FENCED_TEXT));
    let newer = manual
        .claim()
        .expect("a fenced claim never holds off a new one");
    assert!(busy(&rig));

    let ran = AtomicBool::new(false);
    let late = ticket.run(APP, || {
        ran.store(true, Ordering::SeqCst);
        RunReport::text(Err("the plan must not run".into()))
    });
    assert_eq!(failure_text(late).as_deref(), Some(FENCED_TEXT));
    assert!(!ran.load(Ordering::SeqCst), "the plan never started");
    assert!(rig.injector.calls().is_empty(), "nothing was sent");
    assert_eq!(rig.history().len(), 1, "the fence wrote the only entry");
    drop(claim);
    assert!(
        busy(&rig),
        "the old claim's end leaves the newer one's busy"
    );

    recover(&rig);
    let result = manual
        .run(&newer.ticket(), plan(vec![text(BODY)]))
        .expect("the newer claim runs");
    assert_eq!(result.status, "unknown", "{}", result.message);
    assert_eq!(rig.injector.typed(), BODY);
    assert_eq!(rig.history().len(), 2);
    assert_eq!(rig.last_entry().outcome, Outcome::Succeeded);
    drop(newer);
    assert!(!busy(&rig));
    let auto = rig.run("kw");
    assert_eq!(auto.status, "unknown", "{}", auto.message);
}

/// F5-1: a supervised run that finds no claim of its own to adopt is
/// refused before its plan, sends nothing and is recorded as a failed
/// manual run; `busy` and the epoch are left alone.
#[test]
fn a_manual_run_without_a_claim_is_refused_and_recorded() {
    let rig = ExecRig::new(vec![]);
    let epoch = rig.core.epoch();
    let ran = AtomicBool::new(false);
    let refused = super::executor::run_manual(Some(Arc::clone(&rig.core)), None, APP, || {
        ran.store(true, Ordering::SeqCst);
        RunReport::text(Err("the plan must not run".into()))
    })
    .expect_err("refused");
    assert_eq!(
        (refused.reason, refused.text.as_str()),
        (
            ReasonCode::Internal,
            "Manual run holds no claim on the executor; refused without input"
        )
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
    assert_eq!(failure_text(returned).as_deref(), Some(FENCED_TEXT));
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

/// F5-1 B: the preparation's progress checkpoints (`execute_action`'s
/// delay, clipboard read and selection capture) restart the claim's
/// heartbeat. Three phases, each shorter than the budget and together far
/// longer, are never fenced, and the run that follows records its outcome.
#[test]
fn a_preparation_that_checks_in_is_never_fenced_however_long_it_takes() {
    let rig = ExecRig::new(vec![]);
    let manual = ManualPorts::of(&rig);
    let epoch = rig.core.epoch();
    let claim = manual.claim().expect("busy is free");
    let ticket = claim.ticket();
    let phase = STALL_BUDGET - Duration::from_secs(2);
    for name in ["delay", "clipboard read", "selection capture"] {
        rig.clock.advance(phase);
        rig.supervisor.tick();
        assert_eq!(rig.core.epoch(), epoch, "{name}: within the budget");
        ticket
            .progress()
            .unwrap_or_else(|error| panic!("{name}: {error}"));
    }
    rig.clock.advance(phase);
    rig.supervisor.tick();
    assert_eq!(rig.core.epoch(), epoch, "the run's start is within it too");
    assert!(!ticket.fenced());
    assert_eq!(rig.state(), HealthState::Healthy);

    let result = manual
        .run(&ticket, plan(vec![text(BODY)]))
        .expect("the plan ran");
    assert_eq!(result.status, "unknown", "{}", result.message);
    assert_eq!(rig.injector.typed(), BODY);
    let history = rig.history();
    assert_eq!(history.len(), 1, "one entry");
    assert_eq!(
        (history[0].trigger, history[0].outcome),
        (TriggerKind::Manual, Outcome::Succeeded)
    );
    drop(claim);
    assert!(!busy(&rig));
}

/// F5-1 C: a claim whose preparation never checks in again (stuck before
/// its run, say in a clipboard read) is fenced past the budget and leaves
/// the executor with the fence: once the executor is rebuilt, a new manual
/// claim is granted and runs. The stuck caller, woken afterwards, finds its
/// claim fenced at its checkpoint and at its run: refused, nothing sent,
/// nothing recorded, nothing published, and the newer claim (its `busy`,
/// its adoption) left alone.
#[test]
fn a_claim_stuck_in_its_preparation_never_holds_off_the_next_manual_run() {
    let mut rig = ExecRig::new(vec![text_rule("kw", OutputMethod::Keys, TYPED)]);
    let manual = ManualPorts::of(&rig);
    let epoch = rig.core.epoch();
    let claim = manual.claim().expect("busy is free");
    let ticket = claim.ticket();
    let (wake, woken) = std::sync::mpsc::channel::<()>();
    let ports = manual.clone();
    let stuck = std::thread::Builder::new()
        .name("qt-manual".into())
        .spawn(move || {
            let _track = keystate::track_own_input(ports.own);
            // Stuck in the preparation until the test wakes it.
            let _ = woken.recv();
            let progress = ticket.progress();
            let run = ports.run(&ticket, plan(vec![text(BODY)]));
            (progress, run)
        })
        .expect("the manual thread starts");

    rig.clock.advance(STALL_BUDGET + Duration::from_secs(1));
    rig.supervisor.tick();
    assert_eq!(rig.core.epoch(), epoch + 1, "fenced");
    assert!(!busy(&rig), "the fence freed busy");
    assert!(claim.ticket().fenced());
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
    // The command gives its claim back once it timed out: nothing to free.
    drop(claim);
    assert!(!busy(&rig));
    rig.clock.advance(BACKOFF[0]);
    rig.supervisor.tick();
    assert_eq!(rig.state(), HealthState::Healthy);

    let next = manual
        .claim()
        .expect("the stuck claim holds off no new one");
    wake.send(()).expect("the stuck thread waits");
    let (progress, late) = stuck.join().expect("the stuck thread returns");
    assert_eq!(progress.err().as_deref(), Some(FENCED_TEXT));
    assert_eq!(failure_text(late).as_deref(), Some(FENCED_TEXT));
    assert!(
        rig.injector.calls().is_empty(),
        "the stuck caller sent nothing"
    );
    assert_eq!(rig.history().len(), 1, "and recorded nothing");
    assert!(published(&rig).is_none(), "and published nothing");
    assert!(busy(&rig), "the newer claim keeps busy");
    next.ticket()
        .progress()
        .expect("the newer claim is still in force");

    let result = manual
        .run(&next.ticket(), plan(vec![text(TYPED)]))
        .expect("the newer claim runs");
    assert_eq!(result.status, "unknown", "{}", result.message);
    assert_eq!(rig.injector.typed(), TYPED);
    assert_eq!(rig.history().len(), 2);
    assert_eq!(rig.last_entry().outcome, Outcome::Succeeded);
    drop(next);
    assert!(!busy(&rig));
    let auto = rig.run("kw");
    assert_eq!(auto.status, "unknown", "{}", auto.message);
}

/// Arms a panic of the run's next send once the task port took `after`.
#[cfg(feature = "fault-injection")]
fn panic_after(rig: &ExecRig, after: InjectCall) {
    let armed = AtomicBool::new(false);
    rig.injector.on_call(move |call| {
        if *call == after && !armed.swap(true, Ordering::SeqCst) {
            fault::debug_inject_fault(fault::FaultPoint::ExecutorPanic, 0);
        }
    });
}

/// F5-1 D: the end of a manual run releases the keys it left pressed one
/// by one and checks its epoch before each call into the OS. Fenced while
/// its first key-up (A) is inside `SendInput`, it sends no further key-up:
/// B stays in the bitmap, owed, and the rebuild pays it (#6).
#[cfg(feature = "fault-injection")]
#[test]
fn a_fence_during_a_manual_runs_release_stops_it_and_keeps_the_rest_owed() {
    let rig = ExecRig::with_release(vec![]);
    let manual = ManualPorts::of(&rig);
    let epoch = rig.core.epoch();
    // A and B pressed, then the run panics: its end releases both.
    panic_after(&rig, key_down(KEY_B));
    let in_release = hold_first_send(&rig.release);
    let run = manual.spawn(plan(vec![
        Operation::KeyDown { key: code(KEY_A) },
        Operation::KeyDown { key: code(KEY_B) },
        Operation::KeyPress { key: code(KEY_C) },
        Operation::KeyUp { key: code(KEY_B) },
        Operation::KeyUp { key: code(KEY_A) },
    ]));
    in_release.wait_arrived("the run's first key-up");
    assert!(rig.core.fence(epoch));
    drop(in_release);
    let returned = run.join().expect("the manual thread returns");
    assert_eq!(failure_text(returned).as_deref(), Some(FENCED_TEXT));
    assert_eq!(
        rig.release.calls(),
        vec![key_up(KEY_A)],
        "no key-up after the fence"
    );
    assert_eq!(rig.core.held_keys(), vec![KEY_B], "B stays owed");
    assert!(rig.core.release_owed());
    assert_eq!(rig.history().len(), 1, "the fence wrote the only entry");

    recover(&rig);
    assert_eq!(ups(&rig.release, KEY_B), 1, "the rebuild paid B");
    assert_eq!(ups(&rig.release, KEY_A), 1, "and A only once");
    assert!(rig.core.held_keys().is_empty());
    assert!(!rig.core.release_owed());
}

/// F5-1 D, the task path: a task's busy guard releases the keys it left
/// pressed one by one and checks its epoch before each. Fenced while its
/// first key-up (A) is inside `SendInput`, it sends no further key-up: B
/// stays owed and the rebuild pays it (#6).
#[cfg(feature = "fault-injection")]
#[test]
fn a_fence_during_a_tasks_key_release_stops_it_and_keeps_the_rest_owed() {
    let mut rig = ExecRig::with_release(vec![hold_a_and_b()]);
    let epoch = rig.core.epoch();
    panic_after(&rig, key_down(KEY_B));
    let in_release = hold_first_send(&rig.release);
    assert!(rig.trigger("hold"));
    in_release.wait_arrived("the task's first key-up");
    assert!(rig.core.fence(epoch));
    drop(in_release);
    assert!(rig.core.join_retired(Duration::from_secs(5)));
    assert_eq!(
        rig.release.calls(),
        vec![key_up(KEY_A)],
        "no key-up after the fence"
    );
    assert_eq!(rig.core.held_keys(), vec![KEY_B], "B stays owed");
    assert!(rig.core.release_owed());

    recover(&rig);
    assert_eq!(ups(&rig.release, KEY_B), 1, "the rebuild paid B");
    assert_eq!(ups(&rig.release, KEY_A), 1, "and A only once");
    assert!(rig.core.held_keys().is_empty());
    assert!(!rig.core.release_owed());
}

/// F5-1 G: a manual send held right after its first epoch check, before
/// the send lock. Fenced there, it sends nothing once it resumes: the check
/// is made again in the lock's hold. The fence writes the only entry,
/// without side effects; nothing is published and the rebuilt executor
/// serves.
#[test]
fn a_manual_send_held_after_its_first_check_sends_nothing_once_fenced() {
    let mut rig = ExecRig::new(vec![text_rule("kw", OutputMethod::Keys, TYPED)]);
    let manual = ManualPorts::of(&rig);
    let epoch = rig.core.epoch();
    let checked = hold_at(&rig, Pause::BeforeSend, "qt-manual");
    let run = manual.spawn(plan(vec![text(BODY)]));
    checked.wait_arrived("the manual send, its first check made");
    assert!(rig.core.fence(epoch));
    drop(checked);
    let returned = run.join().expect("the manual thread returns");
    assert_eq!(failure_text(returned).as_deref(), Some(FENCED_TEXT));
    assert!(rig.injector.calls().is_empty(), "nothing was sent");
    let history = rig.history();
    assert_eq!(history.len(), 1, "the fence wrote the only entry");
    assert_eq!(
        (
            history[0].trigger,
            history[0].outcome,
            history[0].reason,
            history[0].side_effects
        ),
        (
            TriggerKind::Manual,
            Outcome::Failed,
            ReasonCode::ExecutorTimeout,
            false
        )
    );
    assert!(published(&rig).is_none());
    assert!(!busy(&rig));

    recover(&rig);
    let auto = rig.run("kw");
    assert_eq!(auto.status, "unknown", "{}", auto.message);
    assert_eq!(rig.injector.typed(), TYPED);
}

/// F5-1 G: a fenced manual run held right before it settles, while a newer
/// claim was granted meanwhile. Once it resumes it settles nothing (no
/// history, no `last`) and leaves the newer claim and its `busy` alone; the
/// newer claim runs once the executor is rebuilt.
#[test]
fn a_fenced_run_settling_after_a_newer_claim_leaves_it_alone() {
    let rig = ExecRig::new(vec![]);
    let manual = ManualPorts::of(&rig);
    let epoch = rig.core.epoch();
    let settling = hold_at(&rig, Pause::ManualBeforeSettle, "qt-manual");
    let run = manual.spawn(plan(vec![text(BODY)]));
    settling.wait_arrived("the manual run, about to settle");
    assert_eq!(rig.injector.typed(), BODY);
    assert!(rig.core.fence(epoch));
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
            true
        )
    );
    let newer = manual
        .claim()
        .expect("the fenced claim holds off no new one");
    drop(settling);
    let returned = run.join().expect("the manual thread returns");
    assert_eq!(failure_text(returned).as_deref(), Some(FENCED_TEXT));
    assert_eq!(rig.history().len(), 1, "the late run recorded nothing");
    assert!(published(&rig).is_none(), "and published nothing");
    assert!(busy(&rig), "the newer claim keeps busy");

    recover(&rig);
    let result = manual
        .run(&newer.ticket(), plan(vec![text(TYPED)]))
        .expect("the newer claim runs");
    assert_eq!(result.status, "unknown", "{}", result.message);
    assert_eq!(rig.history().len(), 2);
    assert_eq!(rig.last_entry().outcome, Outcome::Succeeded);
    assert!(published(&rig).is_some());
    drop(newer);
    assert!(!busy(&rig));
}

/// F5-1 H: a manual run's failure never echoes the rule's values. Plans
/// naming a sentinel (an unknown common function, a missing image, a
/// missing file, an unknown key name) fail in preflight with fixed texts
/// and the reason `PreflightFailed`; nothing is sent, and the sentinel
/// reaches neither the history nor the log. What leaves the crate is the
/// reason alone ([`ManualFailure`]).
#[test]
fn a_manual_runs_failure_never_echoes_the_rules_values() {
    const SENTINEL: &str = "SENTINEL-private-3c9e";
    assert!(
        capture::install(),
        "test collector is the global subscriber"
    );
    let rig = ExecRig::new(vec![]);
    let manual = ManualPorts::of(&rig);
    let mark = capture::mark();
    let missing = std::env::temp_dir()
        .join(format!("qt-missing-{SENTINEL}"))
        .to_string_lossy()
        .into_owned();
    let operations = [
        Operation::Common {
            function: SENTINEL.into(),
        },
        Operation::PasteImage {
            path: format!("{missing}.png"),
        },
        Operation::PasteFiles {
            paths: vec![format!("{missing}.txt")],
        },
        Operation::KeyPress {
            key: Key::named(SENTINEL),
        },
    ];
    for (index, operation) in operations.into_iter().enumerate() {
        let claim = manual.claim().expect("busy is free");
        let failure = manual
            .run(&claim.ticket(), plan(vec![operation]))
            .expect_err("preflight refuses the plan");
        assert!(
            !failure.text.contains(SENTINEL),
            "operation {index}: {}",
            failure.text
        );
        assert_eq!(
            failure.reason,
            ReasonCode::PreflightFailed,
            "operation {index}: {}",
            failure.text
        );
        drop(claim);
    }
    assert!(rig.injector.calls().is_empty(), "nothing was sent");
    let history = rig.history();
    assert_eq!(history.len(), 4);
    assert!(history
        .iter()
        .all(|entry| entry.reason == ReasonCode::PreflightFailed && !entry.side_effects));
    let serialized = serde_json::to_string(&history).expect("the history serializes");
    assert!(!serialized.contains(SENTINEL), "{serialized}");
    let events = capture::since(mark);
    assert!(events.iter().all(|event| !event.mentions(SENTINEL)));
}

/// F5-1 X1: a manual run whose send the send exclusion refuses (the lock
/// held past its wait, as by a sender stuck inside `SendInput`) gets the
/// executor's error, `ExecutorTimeout`, which leaves the crate as
/// `ManualFailure { reason: ExecutorTimeout }` and reaches the frontend as
/// `engine_unavailable`, not the "accepted 0/N" failed result the pipeline
/// made of the refusal. Its first send went out: the history entry and the
/// published result keep that side effect, and only that send reached the
/// port. The executor goes Down and the supervisor rebuilds it.
#[test]
fn a_manual_send_refused_by_the_send_exclusion_returns_the_executors_error() {
    let mut rig = ExecRig::new(vec![text_rule("kw", OutputMethod::Keys, TYPED)]);
    let manual = ManualPorts::of(&rig);
    let epoch = rig.core.epoch();
    // The run's second send waits at its first check until the test holds
    // the send lock.
    let (arrived, at_second) = std::sync::mpsc::channel::<()>();
    let (go, held) = std::sync::mpsc::channel::<()>();
    let held = std::sync::Mutex::new(held);
    let sends = std::sync::atomic::AtomicUsize::new(0);
    rig.core.on_pause(move |point| {
        if point == Pause::BeforeSend
            && is_thread("qt-manual")
            && sends.fetch_add(1, Ordering::SeqCst) == 1
        {
            let _ = arrived.send(());
            let _ = held.lock().unwrap_or_else(PoisonError::into_inner).recv();
        }
    });
    let run = manual.spawn(plan(vec![text("x"), text("y")]));
    at_second.recv().expect("the run reaches its second send");
    let stuck = rig.core.hold_send_lock();
    go.send(()).expect("the run waits at its second send");
    let returned = run.join().expect("the manual thread returns");
    drop(stuck);

    let error = returned.expect_err("a refused send is the executor's error");
    assert_eq!(error.reason, ReasonCode::ExecutorTimeout, "{}", error.text);
    assert_eq!(
        rig.injector.calls(),
        vec![InjectCall::Text("x".into())],
        "only the first send"
    );
    assert_eq!(rig.core.epoch(), epoch, "refused, not fenced");
    let history = rig.history();
    assert_eq!(history.len(), 1);
    assert_eq!(
        (
            history[0].trigger,
            history[0].outcome,
            history[0].reason,
            history[0].side_effects
        ),
        (
            TriggerKind::Manual,
            Outcome::Failed,
            ReasonCode::ExecutorTimeout,
            true
        )
    );
    let last = published(&rig).expect("the run published its result");
    assert_eq!((last.status.as_str(), last.side_effects), ("failed", true));
    assert!(!busy(&rig));

    recover(&rig);
    let auto = rig.run("kw");
    assert_eq!(auto.status, "unknown", "{}", auto.message);
}
