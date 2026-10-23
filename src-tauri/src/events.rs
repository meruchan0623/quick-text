//! Backend → frontend events: the EventBus (B-44, docs/ipc-contract-0.4.md
//! §3).
//!
//! One thread (`qt-event-bus`) owns everything that is sent. The runtime's
//! listener only forwards each [`EngineEvent`] into an unbounded channel (it
//! never blocks the runtime's event thread); startup problems arrive on the
//! same channel. The bus thread merges and rate-limits ([`Throttle`]),
//! assigns the global `seq` and calls the [`Sink`] (Tauri `emit`, which may
//! block) with no runtime or store lock held. Because a single thread emits,
//! events leave in `seq` order.
//!
//! The bus also keeps the Health, State and store state (revision, safe
//! mode) it last emitted with their `seq` ([`BusView`]);
//! `get_runtime_snapshot` returns that view, so a snapshot is exactly the
//! event stream up to its `seq`, and the events after it continue from there
//! (nothing at or below the snapshot's `seq` is missing from it, nothing
//! above it is older than what it already shows).
//!
//! The store's revision and safe mode reach the bus through a [`StoreSink`]
//! (review A9). Setting the sink writes the current value straight into the
//! view as the baseline, on the calling thread, under the store lock, with
//! no `seq` and nothing sent (like the starting Health and State: the
//! frontend reads initial values from `load_state` /
//! `get_runtime_snapshot`). After that every change is queued and sent as
//! `store-state`, unthrottled, with the next `seq`, only when it differs
//! from the value in the view (the baseline or the last one sent).
//!
//! Idle cost: the bus thread sleeps in `recv` and only sets a timeout while
//! something is held back (a trailing Health/State, held executions, a busy
//! re-check) or the observer asked for a tick ([`Observer::due_in`]), so it
//! adds no periodic wake-ups.
use crate::{error::SafeModeReason, startup::StartupProblem};
use parking_lot::Mutex;
use quicker_platform::{
    health::{EngineEvent, EngineState, HealthSnapshot, HistoryEntry},
    runtime::Runtime,
};
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{
        mpsc::{self, RecvTimeoutError},
        Arc, Weak,
    },
    time::{Duration, Instant},
};

pub const ENGINE_HEALTH: &str = "engine-health";
pub const ENGINE_STATE: &str = "engine-state";
pub const EXECUTION_RESULT: &str = "execution-result";
pub const STARTUP_PROBLEM: &str = "startup-problem";
pub const STORE_STATE: &str = "store-state";
/// Legacy alias (0.4–0.5): the `"running" | "paused" | "listening"` string,
/// without `seq`.
pub const ENGINE_STATUS: &str = "engine-status";

/// A Health whose `state` did not change is sent at most this often.
pub const HEALTH_RESEND: Duration = Duration::from_secs(5);
/// State changes within this window are merged into one event.
pub const STATE_MERGE: Duration = Duration::from_millis(100);
/// At most [`EXECUTION_LIMIT`] `execution-result` events per this window.
pub const EXECUTION_WINDOW: Duration = Duration::from_secs(1);
pub const EXECUTION_LIMIT: usize = 10;
/// After an execution the engine's `busy` is re-read this often while it
/// still reads true, at most [`BUSY_RECHECKS`] times (see
/// [`Worker::after_execution`]).
const BUSY_RECHECK: Duration = Duration::from_millis(100);
const BUSY_RECHECKS: u32 = 10;

/// Delivers one event to the frontend. Production: [`tauri::AppHandle`].
/// Called only on the bus thread, with nothing locked.
pub trait Sink: Send + 'static {
    fn emit(&self, name: &'static str, payload: Value);
}

impl<R: tauri::Runtime> Sink for tauri::AppHandle<R> {
    fn emit(&self, name: &'static str, payload: Value) {
        if let Err(e) = tauri::Emitter::emit(self, name, payload) {
            tracing::warn!(event = "events.emit_failed", name, error = %e, "event not delivered");
        }
    }
}

/// Sees every engine change as it arrives, before any merging (the tray:
/// tooltip, pause/resume label, run notifications). Called on the bus
/// thread with nothing locked; it may block briefly (the tray waits for the
/// main thread), which only delays the events behind it.
pub trait Observer: Send + 'static {
    /// The latest Health and State, after either changed (and once at start).
    fn status(&mut self, health: &HealthSnapshot, state: &EngineState);
    /// One finished execution (every history entry, never rate-limited).
    fn executed(&mut self, entry: &HistoryEntry);
    /// Each store state the store reports ([`StoreSink::report`]), in order,
    /// before the `store-state` dedup: a value equal to the last one can
    /// arrive again. The value the bus started with is not repeated here.
    fn store(&mut self, _store: StoreState) {}
    /// How long until the observer next wants [`Observer::tick`] without any
    /// input arriving; `None` (the default) adds no wake-up. Asked after
    /// every wake-up of the bus thread.
    fn due_in(&self) -> Option<Duration> {
        None
    }
    /// Called after every wake-up of the bus thread (an input, or a
    /// deadline: the observer's own [`Observer::due_in`] or the bus's).
    fn tick(&mut self) {}
}

/// Several observers behind one (production: the tray, then the
/// last-known-good tracker). Each gets every callback in list order, each
/// guarded on its own, so one that panics does not starve the others. The
/// earliest [`Observer::due_in`] wins.
pub struct Observers(pub Vec<Box<dyn Observer>>);

impl Observer for Observers {
    fn status(&mut self, health: &HealthSnapshot, state: &EngineState) {
        for observer in &mut self.0 {
            guarded("status", || observer.status(health, state));
        }
    }
    fn executed(&mut self, entry: &HistoryEntry) {
        for observer in &mut self.0 {
            guarded("executed", || observer.executed(entry));
        }
    }
    fn store(&mut self, store: StoreState) {
        for observer in &mut self.0 {
            guarded("store", || observer.store(store));
        }
    }
    fn due_in(&self) -> Option<Duration> {
        self.0.iter().filter_map(|observer| observer.due_in()).min()
    }
    fn tick(&mut self) {
        for observer in &mut self.0 {
            guarded("tick", || observer.tick());
        }
    }
}

/// Receives each startup problem found after the store was loaded
/// (`Store::add_problem`, a Locked retry). Never blocks.
pub type ProblemSink = Arc<dyn Fn(StartupProblem) + Send + Sync>;

/// The store's active revision and safe mode: the `store-state` payload
/// (with `seq`) and part of `get_runtime_snapshot`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct StoreState {
    pub revision: u64,
    pub safe_mode: Option<SafeModeReason>,
}

/// Where the store gives its [`StoreState`] ([`EventBus::store_sink`],
/// [`crate::store::Store::set_store_sink`]). Both calls are made with the
/// store lock held, so they follow the order of the changes, and neither
/// blocks: [`StoreSink::baseline`] takes the bus's view lock (a leaf: held
/// by the bus only to number and record one event, never while sending or
/// taking another lock), [`StoreSink::report`] only queues.
#[derive(Clone)]
pub struct StoreSink {
    view: Arc<Mutex<BusView>>,
    tx: mpsc::Sender<Input>,
}

impl StoreSink {
    /// The value when the sink is set: written into the view as the
    /// baseline at once, on the calling thread. No `seq`, nothing sent; a
    /// snapshot taken from now on has it, and a later report equal to it is
    /// not sent.
    pub fn baseline(&self, store: StoreState) {
        self.view.lock().store = store;
    }

    /// The value after a change: queued, and sent as `store-state` with the
    /// next `seq` if it differs from the view's (the baseline or the last
    /// one sent) when the bus gets to it.
    pub fn report(&self, store: StoreState) {
        let _ = self.tx.send(Input::Store(store));
    }
}

/// The emitted stream at one point: the last `seq` and the Health, State
/// and store state sent up to it. Before the first event: `seq` 0 and the
/// Health and State the bus started with; `store` is the value the bus
/// started with until the store sink's baseline replaces it (setup does
/// both under one store lock).
#[derive(Clone, Debug, PartialEq)]
pub struct BusView {
    pub seq: u64,
    pub health: HealthSnapshot,
    pub state: EngineState,
    pub store: StoreState,
}

/// The legacy `engine-status` value (and `load_state.engine_status`).
pub fn legacy_status(state: &EngineState) -> &'static str {
    if state.busy {
        "running"
    } else if !state.listening {
        "paused"
    } else {
        "listening"
    }
}

enum Input {
    Engine(EngineEvent),
    Problem(StartupProblem),
    Store(StoreState),
}

/// What the throttle lets through, in order.
#[derive(Clone, Debug, PartialEq)]
enum Out {
    Health(HealthSnapshot),
    State(EngineState),
    Execution {
        entry: HistoryEntry,
        suppressed: u64,
    },
    Problem(StartupProblem),
    Store(StoreState),
}

pub struct EventBus {
    view: Arc<Mutex<BusView>>,
    tx: mpsc::Sender<Input>,
    /// Whether the bus thread runs. If it did not start, nothing is ever
    /// sent and the view never moves past the starting point.
    started: bool,
}

impl EventBus {
    /// Starts the bus thread and subscribes it to `runtime`. The current
    /// Health and State and `store` (the store's current value) are the
    /// starting point (not sent: the frontend reads them from `load_state` /
    /// `get_runtime_snapshot`). If the thread cannot start, nothing is ever
    /// sent; the frontend still works from commands (logged, and
    /// [`EventBus::started`] is false).
    pub fn start(
        runtime: &Arc<Runtime>,
        store: StoreState,
        sink: impl Sink,
        observer: Box<dyn Observer>,
    ) -> Self {
        let health = runtime.health();
        let state = runtime.engine_state();
        let view = Arc::new(Mutex::new(BusView {
            seq: 0,
            health: health.clone(),
            state: state.clone(),
            store,
        }));
        let (tx, rx) = mpsc::channel();
        let worker = Worker {
            rx,
            view: Arc::clone(&view),
            sink: Box::new(sink),
            observer,
            runtime: Arc::downgrade(runtime),
            throttle: Throttle::new(health.clone(), state.clone()),
            health,
            state,
            busy_check: None,
            observer_due: None,
        };
        let spawned = std::thread::Builder::new()
            .name("qt-event-bus".into())
            .spawn(move || worker.run());
        let started = match spawned {
            Ok(_) => {
                let forward = tx.clone();
                runtime.subscribe(Box::new(move |event| {
                    let _ = forward.send(Input::Engine(event));
                }));
                true
            }
            Err(e) => {
                tracing::error!(
                    event = "events.start_failed",
                    error = %e,
                    "event bus thread did not start; no events will be sent"
                );
                false
            }
        };
        Self { view, tx, started }
    }

    /// A bus whose thread did not start (tests of what the commands do
    /// then): nothing is ever sent.
    #[cfg(test)]
    pub(crate) fn not_started(runtime: &Runtime, store: StoreState) -> Self {
        let (tx, _) = mpsc::channel();
        Self {
            view: Arc::new(Mutex::new(BusView {
                seq: 0,
                health: runtime.health(),
                state: runtime.engine_state(),
                store,
            })),
            tx,
            started: false,
        }
    }

    /// The emitted stream so far (`get_runtime_snapshot`).
    pub fn view(&self) -> BusView {
        self.view.lock().clone()
    }

    /// False if the bus thread did not start: no event will ever be sent,
    /// so `get_runtime_snapshot` reads the live values instead of the view.
    pub fn started(&self) -> bool {
        self.started
    }

    /// Where the store reports problems found from now on: each becomes one
    /// `startup-problem` event.
    pub fn problem_sink(&self) -> ProblemSink {
        let tx = self.tx.clone();
        Arc::new(move |problem| {
            let _ = tx.send(Input::Problem(problem));
        })
    }

    /// Where the store gives its revision and safe mode
    /// ([`crate::store::Store::set_store_sink`]): the baseline when the sink
    /// is set (not sent), then each change that differs from the view
    /// becomes one `store-state` event.
    pub fn store_sink(&self) -> StoreSink {
        StoreSink {
            view: Arc::clone(&self.view),
            tx: self.tx.clone(),
        }
    }
}

/// The bus thread's state.
struct Worker {
    rx: mpsc::Receiver<Input>,
    view: Arc<Mutex<BusView>>,
    sink: Box<dyn Sink>,
    observer: Box<dyn Observer>,
    /// Weak: the runtime holds this bus's listener, so a strong reference
    /// would keep both alive forever.
    runtime: Weak<Runtime>,
    throttle: Throttle,
    /// The latest Health and State received (for the observer).
    health: HealthSnapshot,
    state: EngineState,
    /// Next `busy` re-check and how many are left.
    busy_check: Option<(Instant, u32)>,
    /// When the observer next wants a tick ([`Observer::due_in`]).
    observer_due: Option<Instant>,
}

impl Worker {
    fn run(mut self) {
        self.observe_status();
        self.tick_observer(Instant::now());
        let mut out = Vec::new();
        loop {
            let wake = [
                self.throttle.deadline(),
                self.busy_check.map(|(at, _)| at),
                self.observer_due,
            ]
            .into_iter()
            .flatten()
            .min();
            let input = match wake {
                Some(at) => match self
                    .rx
                    .recv_timeout(at.saturating_duration_since(Instant::now()))
                {
                    Ok(input) => Some(input),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                },
                None => match self.rx.recv() {
                    Ok(input) => Some(input),
                    Err(_) => return,
                },
            };
            let now = Instant::now();
            if let Some(input) = input {
                self.take(input, now, &mut out);
            }
            self.recheck_busy(now);
            self.throttle.due(now, &mut out);
            for item in out.drain(..) {
                deliver(&self.view, &*self.sink, item);
            }
            self.tick_observer(now);
        }
    }

    /// Gives the observer its tick and asks when it wants the next one.
    fn tick_observer(&mut self, now: Instant) {
        let observer = &mut self.observer;
        let mut due = None;
        guarded("tick", || {
            observer.tick();
            due = observer.due_in();
        });
        self.observer_due = due.map(|after| now + after);
    }

    fn take(&mut self, input: Input, now: Instant, out: &mut Vec<Out>) {
        match input {
            Input::Engine(EngineEvent::Health(health)) => {
                self.health.clone_from(&health);
                self.observe_status();
                self.throttle.health(health, now, out);
            }
            Input::Engine(EngineEvent::State(state)) => {
                self.state.clone_from(&state);
                self.observe_status();
                self.throttle.state(state, now, out);
            }
            Input::Engine(EngineEvent::Execution(entry)) => {
                let observer = &mut self.observer;
                guarded("executed", || observer.executed(&entry));
                self.throttle.execution(entry, now, out);
                self.after_execution(now);
            }
            Input::Problem(problem) => out.push(Out::Problem(problem)),
            // Unthrottled; `deliver` drops a report equal to the view's
            // (the baseline or the last one sent).
            Input::Store(store) => {
                let observer = &mut self.observer;
                guarded("store", || observer.store(store));
                out.push(Out::Store(store));
            }
        }
    }

    fn observe_status(&mut self) {
        let (observer, health, state) = (&mut self.observer, &self.health, &self.state);
        guarded("status", || observer.status(health, state));
    }

    /// The runtime recomputes `EngineState.busy` only when it publishes
    /// (health, pause, attach); the executor flipping `busy` does not. A run
    /// ending is signalled by its history entry, so `busy` is refreshed here
    /// and, while it still reads true (the executor clears it right after
    /// recording, or the next run started), re-read every 100ms for up to a
    /// second. Beyond that the supervisor's 1s tick, which republishes, keeps
    /// it fresh (`busy` rising for a run is seen by that tick, or by the next
    /// refresh here). Manual runs refresh from `execute_action`.
    fn after_execution(&mut self, now: Instant) {
        let Some(runtime) = self.runtime.upgrade() else {
            return;
        };
        runtime.refresh_state();
        self.busy_check = runtime
            .engine_state()
            .busy
            .then_some((now + BUSY_RECHECK, BUSY_RECHECKS));
    }

    fn recheck_busy(&mut self, now: Instant) {
        let Some((at, left)) = self.busy_check else {
            return;
        };
        if now < at {
            return;
        }
        self.busy_check = None;
        let Some(runtime) = self.runtime.upgrade() else {
            return;
        };
        runtime.refresh_state();
        if runtime.engine_state().busy && left > 1 {
            self.busy_check = Some((now + BUSY_RECHECK, left - 1));
        }
    }
}

/// Runs an observer callback; a panic is logged and the bus keeps running.
fn guarded(what: &'static str, f: impl FnOnce()) {
    if catch_unwind(AssertUnwindSafe(f)).is_err() {
        tracing::warn!(
            event = "events.observer_panic",
            callback = what,
            "event observer panicked"
        );
    }
}

/// Assigns the next `seq`, records Health/State/store state in the view
/// (one critical section, so a snapshot sees the `seq` and the values
/// together) and emits with the lock released. A State that changes the
/// legacy status string is followed by `engine-status`. A store state
/// equal to the view's (the baseline the store sink wrote, or the last one
/// sent) is dropped there, before it takes a `seq`.
fn deliver(view: &Mutex<BusView>, sink: &dyn Sink, item: Out) {
    let (seq, legacy) = {
        let mut view = view.lock();
        if matches!(&item, Out::Store(store) if *store == view.store) {
            return;
        }
        view.seq += 1;
        let legacy = match &item {
            Out::Health(health) => {
                view.health.clone_from(health);
                None
            }
            Out::State(state) => {
                let before = legacy_status(&view.state);
                view.state.clone_from(state);
                let after = legacy_status(state);
                (before != after).then_some(after)
            }
            Out::Store(store) => {
                view.store = *store;
                None
            }
            Out::Execution { .. } | Out::Problem(_) => None,
        };
        (view.seq, legacy)
    };
    let (name, payload) = match item {
        Out::Health(health) => (ENGINE_HEALTH, with_seq(seq, &health)),
        Out::State(state) => (ENGINE_STATE, with_seq(seq, &state)),
        Out::Execution { entry, suppressed } => (
            EXECUTION_RESULT,
            json!({ "seq": seq, "entry": entry, "suppressed": suppressed }),
        ),
        Out::Problem(problem) => (STARTUP_PROBLEM, with_seq(seq, &problem)),
        Out::Store(store) => (STORE_STATE, with_seq(seq, &store)),
    };
    guarded("emit", || sink.emit(name, payload));
    if let Some(status) = legacy {
        guarded("emit", || sink.emit(ENGINE_STATUS, Value::from(status)));
    }
}

/// `value`'s JSON object with `seq` added at the top level.
fn with_seq(seq: u64, value: &impl Serialize) -> Value {
    let mut payload = serde_json::to_value(value).unwrap_or_else(|_| json!({}));
    if let Value::Object(map) = &mut payload {
        map.insert("seq".into(), seq.into());
    }
    payload
}

/// Merging and rate limiting (contract §3), driven by explicit instants so
/// it can be tested without sleeping. It remembers what was last let
/// through, so a value equal to it is never sent again.
///
/// - Health: a different `state` goes out at once. The same `state` with
///   other components goes out at once if the last Health is at least
///   [`HEALTH_RESEND`] old, otherwise the latest such value is held until
///   then. Nothing is resent without a change (no heartbeat).
/// - State: leading and trailing edge over [`STATE_MERGE`]: a change goes
///   out at once if the last State is at least that old; otherwise the
///   latest value is held until then, so a burst becomes at most one event
///   per 100ms and a change undone within the window sends nothing.
/// - Execution: at most [`EXECUTION_LIMIT`] per sliding [`EXECUTION_WINDOW`].
///   Beyond that only the newest entry is held; when the window has room it
///   goes out with `suppressed` = the number of entries merged into it (not
///   sent on their own). The frontend reads those with `list_history`.
struct Throttle {
    health: HealthSnapshot,
    health_at: Option<Instant>,
    health_held: Option<HealthSnapshot>,
    state: EngineState,
    state_at: Option<Instant>,
    state_held: Option<EngineState>,
    sent: VecDeque<Instant>,
    execution_held: Option<HistoryEntry>,
    merged: u64,
}

impl Throttle {
    fn new(health: HealthSnapshot, state: EngineState) -> Self {
        Self {
            health,
            health_at: None,
            health_held: None,
            state,
            state_at: None,
            state_held: None,
            sent: VecDeque::with_capacity(EXECUTION_LIMIT),
            execution_held: None,
            merged: 0,
        }
    }

    fn health(&mut self, health: HealthSnapshot, now: Instant, out: &mut Vec<Out>) {
        if health == self.health {
            self.health_held = None;
            return;
        }
        let recent = self
            .health_at
            .is_some_and(|at| now.saturating_duration_since(at) < HEALTH_RESEND);
        if health.state == self.health.state && recent {
            self.health_held = Some(health);
        } else {
            self.send_health(health, now, out);
        }
    }

    fn send_health(&mut self, health: HealthSnapshot, now: Instant, out: &mut Vec<Out>) {
        self.health_held = None;
        self.health.clone_from(&health);
        self.health_at = Some(now);
        out.push(Out::Health(health));
    }

    fn state(&mut self, state: EngineState, now: Instant, out: &mut Vec<Out>) {
        if state == self.state {
            self.state_held = None;
            return;
        }
        let recent = self
            .state_at
            .is_some_and(|at| now.saturating_duration_since(at) < STATE_MERGE);
        if recent {
            self.state_held = Some(state);
        } else {
            self.send_state(state, now, out);
        }
    }

    fn send_state(&mut self, state: EngineState, now: Instant, out: &mut Vec<Out>) {
        self.state_held = None;
        self.state.clone_from(&state);
        self.state_at = Some(now);
        out.push(Out::State(state));
    }

    fn execution(&mut self, entry: HistoryEntry, now: Instant, out: &mut Vec<Out>) {
        self.expire(now);
        if self.sent.len() < EXECUTION_LIMIT {
            let suppressed = self.merged + u64::from(self.execution_held.take().is_some());
            self.send_execution(entry, suppressed, now, out);
        } else if self.execution_held.replace(entry).is_some() {
            self.merged += 1;
        }
    }

    fn send_execution(
        &mut self,
        entry: HistoryEntry,
        suppressed: u64,
        now: Instant,
        out: &mut Vec<Out>,
    ) {
        self.merged = 0;
        self.sent.push_back(now);
        out.push(Out::Execution { entry, suppressed });
    }

    /// Forgets sends that left the window.
    fn expire(&mut self, now: Instant) {
        while self
            .sent
            .front()
            .is_some_and(|at| now.saturating_duration_since(*at) >= EXECUTION_WINDOW)
        {
            self.sent.pop_front();
        }
    }

    /// Lets through whatever was held and is due at `now`.
    fn due(&mut self, now: Instant, out: &mut Vec<Out>) {
        let elapsed = |at: Option<Instant>, window| {
            at.is_some_and(|at| now.saturating_duration_since(at) >= window)
        };
        let health_due = elapsed(self.health_at, HEALTH_RESEND);
        if let Some(health) = self.health_held.take_if(|_| health_due) {
            self.send_health(health, now, out);
        }
        let state_due = elapsed(self.state_at, STATE_MERGE);
        if let Some(state) = self.state_held.take_if(|_| state_due) {
            self.send_state(state, now, out);
        }
        if self.execution_held.is_some() {
            self.expire(now);
            let room = self.sent.len() < EXECUTION_LIMIT;
            if let Some(entry) = self.execution_held.take_if(|_| room) {
                let suppressed = self.merged;
                self.send_execution(entry, suppressed, now, out);
            }
        }
    }

    /// When something held becomes due; `None` when nothing is held.
    fn deadline(&self) -> Option<Instant> {
        let health = self
            .health_held
            .as_ref()
            .and(self.health_at)
            .map(|at| at + HEALTH_RESEND);
        let state = self
            .state_held
            .as_ref()
            .and(self.state_at)
            .map(|at| at + STATE_MERGE);
        let execution = self
            .execution_held
            .as_ref()
            .and(self.sent.front().copied())
            .map(|at| at + EXECUTION_WINDOW);
        [health, state, execution].into_iter().flatten().min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::startup::ProblemKind;
    use quicker_platform::{
        health::{
            Component, ComponentHealth, HealthState, Outcome, PauseReasons, ReasonCode,
            StageTimings, TriggerKind,
        },
        history::Record,
        traits::SystemClock,
    };
    use std::sync::mpsc::Receiver;

    fn health(state: HealthState, clipboard: Option<ReasonCode>) -> HealthSnapshot {
        HealthSnapshot {
            state,
            components: clipboard
                .map(|reason| ComponentHealth {
                    component: Component::Clipboard,
                    state: HealthState::Degraded,
                    reason: Some(reason),
                    since_ms: 1,
                })
                .into_iter()
                .collect(),
        }
    }

    fn state(listening: bool, busy: bool) -> EngineState {
        EngineState {
            listening,
            paused_reasons: if listening {
                PauseReasons::empty()
            } else {
                PauseReasons::MANUAL
            },
            busy,
            current_process: None,
        }
    }

    fn entry(seq: u64) -> HistoryEntry {
        HistoryEntry {
            seq,
            at_ms: 0,
            rule_id: format!("rule-{seq}"),
            trigger: TriggerKind::Immediate,
            outcome: Outcome::Succeeded,
            reason: ReasonCode::Ok,
            process: None,
            completed_steps: 1,
            side_effects: true,
            skipped_formats: vec![],
            timings: StageTimings::default(),
        }
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn store(revision: u64, safe_mode: Option<SafeModeReason>) -> StoreState {
        StoreState {
            revision,
            safe_mode,
        }
    }

    #[test]
    fn health_state_changes_go_out_at_once_and_same_state_changes_at_most_every_5s() {
        let t0 = Instant::now();
        let mut throttle = Throttle::new(health(HealthState::Starting, None), state(false, false));
        let mut out = vec![];
        throttle.health(health(HealthState::Healthy, None), t0, &mut out);
        throttle.health(health(HealthState::Degraded, None), t0 + ms(10), &mut out);
        assert_eq!(
            out,
            [
                Out::Health(health(HealthState::Healthy, None)),
                Out::Health(health(HealthState::Degraded, None)),
            ]
        );
        out.clear();
        // Same state, other components: held until 5s after the last send,
        // and only the latest one goes out.
        let busy = health(HealthState::Degraded, Some(ReasonCode::ClipboardBusy));
        let changed = health(HealthState::Degraded, Some(ReasonCode::ClipboardChanged));
        throttle.health(busy, t0 + ms(20), &mut out);
        throttle.health(changed.clone(), t0 + ms(30), &mut out);
        assert!(out.is_empty());
        assert_eq!(throttle.deadline(), Some(t0 + ms(10) + HEALTH_RESEND));
        throttle.due(t0 + ms(4000), &mut out);
        assert!(out.is_empty());
        throttle.due(t0 + ms(5010), &mut out);
        assert_eq!(out, [Out::Health(changed.clone())]);
        out.clear();
        // Nothing held: no heartbeat, no deadline.
        assert_eq!(throttle.deadline(), None);
        throttle.due(t0 + ms(60_000), &mut out);
        assert!(out.is_empty());
        // A change undone before it was sent sends nothing.
        throttle.health(health(HealthState::Degraded, None), t0 + ms(6000), &mut out);
        throttle.health(changed, t0 + ms(6001), &mut out);
        throttle.due(t0 + ms(20_000), &mut out);
        assert!(out.is_empty());
        // Long after the last send a same-state change goes out at once.
        throttle.health(
            health(HealthState::Degraded, None),
            t0 + ms(20_000),
            &mut out,
        );
        assert_eq!(out, [Out::Health(health(HealthState::Degraded, None))]);
    }

    #[test]
    fn state_changes_within_100ms_merge_into_one() {
        let t0 = Instant::now();
        let mut throttle = Throttle::new(HealthSnapshot::default(), state(false, false));
        let mut out = vec![];
        throttle.state(state(true, false), t0, &mut out);
        assert_eq!(out, [Out::State(state(true, false))]);
        out.clear();
        throttle.state(state(true, true), t0 + ms(10), &mut out);
        throttle.state(state(false, true), t0 + ms(20), &mut out);
        assert!(out.is_empty());
        assert_eq!(throttle.deadline(), Some(t0 + STATE_MERGE));
        throttle.due(t0 + ms(99), &mut out);
        assert!(out.is_empty());
        throttle.due(t0 + ms(100), &mut out);
        assert_eq!(out, [Out::State(state(false, true))]);
        out.clear();
        // A blip undone within the window sends nothing.
        throttle.state(state(false, false), t0 + ms(150), &mut out);
        throttle.state(state(false, true), t0 + ms(160), &mut out);
        throttle.due(t0 + ms(1000), &mut out);
        assert!(out.is_empty());
        assert_eq!(throttle.deadline(), None);
    }

    #[test]
    fn executions_beyond_10_per_second_fold_into_the_next_push() {
        let t0 = Instant::now();
        let mut throttle = Throttle::new(HealthSnapshot::default(), EngineState::default());
        let mut out = vec![];
        for seq in 1..=15 {
            throttle.execution(entry(seq), t0 + ms(seq), &mut out);
        }
        assert_eq!(out.len(), EXECUTION_LIMIT);
        assert!(out
            .iter()
            .all(|o| matches!(o, Out::Execution { suppressed: 0, .. })));
        out.clear();
        // Entries 11..=14 are merged into 15, which goes out when the first
        // send leaves the window.
        assert_eq!(throttle.deadline(), Some(t0 + ms(1) + EXECUTION_WINDOW));
        throttle.due(t0 + ms(1000), &mut out);
        assert!(out.is_empty());
        throttle.due(t0 + ms(1001), &mut out);
        assert_eq!(
            out,
            [Out::Execution {
                entry: entry(15),
                suppressed: 4
            }]
        );
        out.clear();
        // Still a full window (sends at 2..=10ms and 1001ms): 16 is held,
        // 17 replaces it, and once room opens (the 2ms send expires at
        // 1002ms) the next entry carries both.
        for seq in 16..=17 {
            throttle.execution(entry(seq), t0 + ms(1001), &mut out);
        }
        assert!(out.is_empty());
        throttle.execution(entry(18), t0 + ms(1002), &mut out);
        assert_eq!(
            out,
            [Out::Execution {
                entry: entry(18),
                suppressed: 2
            }]
        );
        out.clear();
        // Then a quiet second: the next one is sent alone.
        throttle.execution(entry(19), t0 + ms(5000), &mut out);
        assert_eq!(
            out,
            [Out::Execution {
                entry: entry(19),
                suppressed: 0
            }]
        );
    }

    /// Records what the bus emits and lets a test wait for it.
    struct Channel(mpsc::Sender<(&'static str, Value)>);
    impl Sink for Channel {
        fn emit(&self, name: &'static str, payload: Value) {
            let _ = self.0.send((name, payload));
        }
    }

    #[derive(Default)]
    struct Seen {
        statuses: Vec<(HealthState, bool)>,
        executed: Vec<u64>,
    }
    struct Recorder(Arc<Mutex<Seen>>);
    impl Observer for Recorder {
        fn status(&mut self, health: &HealthSnapshot, state: &EngineState) {
            self.0.lock().statuses.push((health.state, state.listening));
        }
        fn executed(&mut self, entry: &HistoryEntry) {
            self.0.lock().executed.push(entry.seq);
        }
    }

    fn next(rx: &Receiver<(&'static str, Value)>) -> (&'static str, Value) {
        rx.recv_timeout(Duration::from_secs(10))
            .expect("the bus emitted nothing")
    }

    #[test]
    fn deliver_numbers_every_event_and_adds_the_legacy_status_on_change() {
        let view = Mutex::new(BusView {
            seq: 0,
            health: HealthSnapshot::default(),
            state: state(false, false),
            store: store(1, None),
        });
        let (tx, rx) = mpsc::channel();
        let sink = Channel(tx);
        let problem = StartupProblem::from(ProblemKind::AutostartRepaired {});
        deliver(
            &view,
            &sink,
            Out::Health(health(HealthState::Healthy, None)),
        );
        deliver(&view, &sink, Out::State(state(true, false)));
        deliver(&view, &sink, Out::State(state(true, false)));
        deliver(
            &view,
            &sink,
            Out::Execution {
                entry: entry(7),
                suppressed: 3,
            },
        );
        deliver(&view, &sink, Out::Problem(problem));
        let sent: Vec<_> = rx.try_iter().collect();
        assert_eq!(
            sent,
            [
                (
                    ENGINE_HEALTH,
                    json!({"seq": 1, "state": "healthy", "components": []})
                ),
                (
                    ENGINE_STATE,
                    json!({"seq": 2, "listening": true, "paused_reasons": [], "busy": false, "current_process": null})
                ),
                (ENGINE_STATUS, json!("listening")),
                (
                    ENGINE_STATE,
                    json!({"seq": 3, "listening": true, "paused_reasons": [], "busy": false, "current_process": null})
                ),
                (
                    EXECUTION_RESULT,
                    json!({"seq": 4, "entry": serde_json::to_value(entry(7)).unwrap(), "suppressed": 3})
                ),
                (
                    STARTUP_PROBLEM,
                    json!({"seq": 5, "kind": "autostart_repaired", "severity": "info", "args": {}})
                ),
            ]
        );
        let view = view.lock().clone();
        assert_eq!(view.seq, 5);
        assert_eq!(view.health, health(HealthState::Healthy, None));
        assert_eq!(view.state, state(true, false));
        assert_eq!(view.store, store(1, None));
    }

    #[test]
    fn the_bus_forwards_runtime_events_and_problems_in_seq_order() {
        let runtime = Arc::new(Runtime::new(Arc::new(SystemClock)));
        let (tx, rx) = mpsc::channel();
        let seen = Arc::new(Mutex::new(Seen::default()));
        let bus = EventBus::start(
            &runtime,
            store(1, None),
            Channel(tx),
            Box::new(Recorder(seen.clone())),
        );
        assert!(bus.started());
        // The starting point is not sent.
        assert_eq!(bus.view().seq, 0);
        assert_eq!(bus.view().health.state, HealthState::Starting);

        runtime.publish_health(health(HealthState::Healthy, None));
        let (name, payload) = next(&rx);
        assert_eq!(
            (name, &payload["state"]),
            (ENGINE_HEALTH, &json!("healthy"))
        );
        assert_eq!(payload["seq"], 1);
        // Healthy without a reason: listening, and the legacy alias follows.
        let (name, payload) = next(&rx);
        assert_eq!((name, &payload["listening"]), (ENGINE_STATE, &json!(true)));
        assert_eq!(payload["seq"], 2);
        assert_eq!(next(&rx), (ENGINE_STATUS, json!("listening")));

        runtime.record(Record {
            rule_id: "r".into(),
            trigger: TriggerKind::Terminator,
            outcome: Outcome::Failed,
            reason: ReasonCode::ClipboardBusy,
            process: None,
            completed_steps: 0,
            side_effects: false,
            skipped_formats: vec![],
            timings: StageTimings::default(),
        });
        let (name, payload) = next(&rx);
        assert_eq!(name, EXECUTION_RESULT);
        assert_eq!(payload["seq"], 3);
        assert_eq!(payload["suppressed"], 0);
        assert_eq!(payload["entry"]["reason"], "clipboard_busy");

        (bus.problem_sink())(ProblemKind::AutostartRepaired {}.into());
        let (name, payload) = next(&rx);
        assert_eq!(
            (name, &payload["kind"]),
            (STARTUP_PROBLEM, &json!("autostart_repaired"))
        );
        assert_eq!(payload["seq"], 4);

        let view = bus.view();
        assert_eq!(view.seq, 4);
        assert_eq!(view.health.state, HealthState::Healthy);
        assert!(view.state.listening);
        let seen = seen.lock();
        assert_eq!(seen.statuses.first(), Some(&(HealthState::Starting, false)));
        assert_eq!(seen.statuses.last(), Some(&(HealthState::Healthy, true)));
        assert_eq!(seen.executed, [1]);
    }

    /// Everything sent before the next `startup-problem`, a marker queued
    /// behind the step under test.
    fn until_marker(rx: &Receiver<(&'static str, Value)>) -> Vec<(&'static str, Value)> {
        let mut sent = vec![];
        loop {
            let (name, payload) = next(rx);
            if name == STARTUP_PROBLEM {
                return sent;
            }
            sent.push((name, payload));
        }
    }

    /// Review A4/A9: `store-state` goes out at once (no merging, even for
    /// reports back to back), numbered from the same `seq` as the rest, and
    /// only when the reported value differs from the view's: the baseline
    /// first, then the last one sent. The view keeps the last one sent.
    #[test]
    fn store_state_goes_out_unthrottled_once_per_change() {
        let runtime = Arc::new(Runtime::new(Arc::new(SystemClock)));
        let (tx, rx) = mpsc::channel();
        let bus = EventBus::start(
            &runtime,
            store(1, None),
            Channel(tx),
            Box::new(Recorder(Arc::default())),
        );
        let sink = bus.store_sink();
        let locked = store(0, Some(SafeModeReason::Locked));
        let recovered = store(2, None);
        sink.baseline(locked);
        sink.report(locked);
        sink.report(locked);
        sink.report(recovered);
        sink.report(recovered);
        sink.report(store(3, None));
        (bus.problem_sink())(ProblemKind::AutostartRepaired {}.into());
        assert_eq!(
            until_marker(&rx),
            [
                (
                    STORE_STATE,
                    json!({"seq": 1, "revision": 2, "safe_mode": null})
                ),
                (
                    STORE_STATE,
                    json!({"seq": 2, "revision": 3, "safe_mode": null})
                ),
            ]
        );
        let view = bus.view();
        assert_eq!(view.seq, 3);
        assert_eq!(view.store, store(3, None));
    }

    /// Review A9: the baseline is in the view as soon as `baseline` returns
    /// (on the caller's thread, no `seq`), it is not sent, and with no
    /// change after it no `store-state` is ever sent.
    #[test]
    fn the_baseline_is_in_the_view_at_once_and_never_sent() {
        let runtime = Arc::new(Runtime::new(Arc::new(SystemClock)));
        let (tx, rx) = mpsc::channel();
        let bus = EventBus::start(
            &runtime,
            store(1, None),
            Channel(tx),
            Box::new(Recorder(Arc::default())),
        );
        let sink = bus.store_sink();
        let locked = store(4, Some(SafeModeReason::Locked));
        sink.baseline(locked);
        let view = bus.view();
        assert_eq!((view.seq, view.store), (0, locked));
        // Reports of the same value (an unchanged reload) send nothing.
        sink.report(locked);
        sink.report(locked);
        (bus.problem_sink())(ProblemKind::AutostartRepaired {}.into());
        let sent = until_marker(&rx);
        assert!(sent.is_empty(), "nothing before the marker: {sent:?}");
        let view = bus.view();
        assert_eq!((view.seq, view.store), (1, locked), "only the marker");
    }

    /// Review A3: the observer (tray run notifications) gets every history
    /// entry, before the `execution-result` limit folds a burst.
    #[test]
    fn the_observer_sees_every_entry_of_a_burst_the_push_folds() {
        let runtime = Arc::new(Runtime::new(Arc::new(SystemClock)));
        let (tx, rx) = mpsc::channel();
        let seen = Arc::new(Mutex::new(Seen::default()));
        let _bus = EventBus::start(
            &runtime,
            store(1, None),
            Channel(tx),
            Box::new(Recorder(seen.clone())),
        );
        for _ in 0..15 {
            runtime.record(Record {
                rule_id: "r".into(),
                trigger: TriggerKind::Terminator,
                outcome: Outcome::Failed,
                reason: ReasonCode::ClipboardBusy,
                process: None,
                completed_steps: 0,
                side_effects: false,
                skipped_formats: vec![],
                timings: StageTimings::default(),
            });
        }
        let mut pushed = vec![];
        while pushed.last().map(|(seq, _)| *seq) != Some(15) {
            let (name, payload) = next(&rx);
            if name == EXECUTION_RESULT {
                let seq = payload["entry"]["seq"].as_u64().unwrap();
                pushed.push((seq, payload["suppressed"].as_u64().unwrap()));
            }
        }
        // The push: ten at once, the other five folded into one.
        assert_eq!(pushed.len(), EXECUTION_LIMIT + 1);
        assert_eq!(pushed.last(), Some(&(15, 4)));
        // The observer: each of the fifteen, in order.
        assert_eq!(seen.lock().executed, (1..=15).collect::<Vec<u64>>());
    }

    /// Reports every store state it gets and asks for one tick a little
    /// after each, which it reports when it comes.
    struct Timed {
        tx: mpsc::Sender<Result<StoreState, ()>>,
        armed: Option<Instant>,
    }
    impl Observer for Timed {
        fn status(&mut self, _: &HealthSnapshot, _: &EngineState) {}
        fn executed(&mut self, _: &HistoryEntry) {}
        fn store(&mut self, store: StoreState) {
            let _ = self.tx.send(Ok(store));
            self.armed = Some(Instant::now() + ms(20));
        }
        fn due_in(&self) -> Option<Duration> {
            self.armed
                .map(|at| at.saturating_duration_since(Instant::now()))
        }
        fn tick(&mut self) {
            if self.armed.is_some_and(|at| Instant::now() >= at) {
                self.armed = None;
                let _ = self.tx.send(Err(()));
            }
        }
    }

    /// The observer gets every store report in order (repeats included: the
    /// `store-state` dedup is the bus's), and a tick at the time it asked
    /// for, with no input to wake the bus.
    #[test]
    fn the_observer_gets_store_reports_and_the_ticks_it_asks_for() {
        let runtime = Arc::new(Runtime::new(Arc::new(SystemClock)));
        let (tx, _rx) = mpsc::channel();
        let (seen_tx, seen) = mpsc::channel();
        let bus = EventBus::start(
            &runtime,
            store(1, None),
            Channel(tx),
            Box::new(Timed {
                tx: seen_tx,
                armed: None,
            }),
        );
        let sink = bus.store_sink();
        let next_seen = || seen.recv_timeout(Duration::from_secs(10)).unwrap();
        for report in [
            store(2, None),
            store(2, None),
            store(3, Some(SafeModeReason::CrashLoop)),
        ] {
            sink.report(report);
            assert_eq!(next_seen(), Ok(report));
            assert_eq!(next_seen(), Err(()), "the tick it asked for");
        }
        assert!(seen.try_recv().is_err(), "one tick per request");
    }

    /// Logs each callback it gets as `(name, callback)`, then panics if
    /// told to; asks for a tick after `due`.
    struct Child {
        name: &'static str,
        log: Arc<Mutex<Vec<(&'static str, &'static str)>>>,
        panics: bool,
        due: Option<Duration>,
    }
    impl Child {
        fn got(&self, callback: &'static str) {
            self.log.lock().push((self.name, callback));
            assert!(!self.panics, "{} panics in {callback}", self.name);
        }
    }
    impl Observer for Child {
        fn status(&mut self, _: &HealthSnapshot, _: &EngineState) {
            self.got("status");
        }
        fn executed(&mut self, _: &HistoryEntry) {
            self.got("executed");
        }
        fn store(&mut self, _: StoreState) {
            self.got("store");
        }
        fn due_in(&self) -> Option<Duration> {
            self.due
        }
        fn tick(&mut self) {
            self.got("tick");
        }
    }

    /// The tray and the last-known-good tracker share the bus: each gets
    /// every callback, in list order, one panicking does not starve the
    /// others, and the earliest tick request wins.
    #[test]
    fn observers_each_get_every_callback_even_when_one_panics() {
        let log = Arc::new(Mutex::new(vec![]));
        let child = |name, panics, due| -> Box<dyn Observer> {
            Box::new(Child {
                name,
                log: Arc::clone(&log),
                panics,
                due,
            })
        };
        let mut all = Observers(vec![
            child("tray", true, Some(ms(50))),
            child("lkg", false, Some(ms(20))),
            child("idle", false, None),
        ]);
        all.status(&HealthSnapshot::default(), &EngineState::default());
        all.executed(&entry(1));
        all.store(store(2, None));
        all.tick();
        assert_eq!(all.due_in(), Some(ms(20)));
        let expected: Vec<_> = ["status", "executed", "store", "tick"]
            .into_iter()
            .flat_map(|callback| ["tray", "lkg", "idle"].map(|name| (name, callback)))
            .collect();
        assert_eq!(*log.lock(), expected);
        assert_eq!(Observers(vec![child("idle", false, None)]).due_in(), None);
    }
}
