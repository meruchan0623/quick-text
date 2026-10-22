//! Engine runtime (B-03, B-35): the process-wide hub that owns the published
//! health snapshot, the pause-reason bitmap, the execution history and the
//! event listeners.
//!
//! The global is an `ArcSwapOption<Runtime>` (replacing the 0.3 `OnceLock`
//! globals): it is created lazily on first use and lives for the process.
//! What the supervisor rebuilds (the executor worker and the supervisor's
//! own driver thread) is swapped in their own `ArcSwapOption`s, so a rebuild
//! never loses history, pause reasons or subscriptions.
//!
//! Nothing here depends on Tauri. Events go to listeners registered with
//! [`subscribe`]; they are delivered on a dedicated thread, so a slow or
//! panicking listener never runs on (or blocks) an input, executor or
//! supervisor thread.
use crate::health::{
    EngineEvent, EngineState, HealthSnapshot, HealthState, HistoryEntry, PauseReasons, StatusText,
};
use crate::history::{History, MissExplanation, Record};
use crate::traits::{Clock, SystemClock};
use crate::Shared;
use arc_swap::ArcSwapOption;
use parking_lot::{Mutex, RwLock};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};

/// Receives every [`EngineEvent`]. Called on the runtime's event thread.
pub type Listener = Arc<dyn Fn(EngineEvent) + Send + Sync>;
/// Returns the executable name of the foreground process, if known.
pub type ProcessProbe = Arc<dyn Fn() -> Option<String> + Send + Sync>;

/// Handle returned by [`Runtime::subscribe`], used to unsubscribe.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SubscriptionId(u64);

type Listeners = Arc<RwLock<Vec<Arc<Subscription>>>>;

/// One subscriber and what the event thread has handed to it so far.
struct Subscription {
    id: u64,
    listener: Listener,
    /// Taken only by the event thread, with nothing else held.
    delivered: Mutex<Delivered>,
}

/// The Health and State one subscriber was last given (each `None` until
/// the first delivery) and the version it is fully up to date with. A
/// value whose callback panicked is not recorded, so a later round offers
/// it again.
#[derive(Default)]
struct Delivered {
    version: Option<u64>,
    health: Option<HealthSnapshot>,
    state: Option<EngineState>,
}

impl Subscription {
    /// Calls the listener; false if it panicked (logged, never propagated).
    fn call(&self, event: EngineEvent) -> bool {
        let ok = catch_unwind(AssertUnwindSafe(|| (self.listener)(event))).is_ok();
        if !ok {
            tracing::warn!(
                event = "runtime.listener_panic",
                "event listener panicked; other listeners still run"
            );
        }
        ok
    }

    /// Brings this subscriber up to `latest`: Health first when it has none
    /// yet (the default snapshot included) or it differs, then State. A
    /// panic stops there without recording that value, so a State is never
    /// given without the Health it was computed with.
    fn catch_up(&self, latest: &Latest) {
        let mut delivered = self.delivered.lock();
        if delivered.version == Some(latest.version) {
            return;
        }
        if delivered.health.as_ref() != Some(&latest.health) {
            if !self.call(EngineEvent::Health(latest.health.clone())) {
                return;
            }
            delivered.health = Some(latest.health.clone());
        }
        if let Some(state) = &latest.state {
            if delivered.state.as_ref() != Some(state) {
                if !self.call(EngineEvent::State(state.clone())) {
                    return;
                }
                delivered.state = Some(state.clone());
            }
        }
        delivered.version = Some(latest.version);
    }
}

/// Capacity of the `Execution` queue between [`Runtime::record`] and the
/// event thread. Bounded so a blocked listener cannot grow memory without
/// limit; emitters never wait on it (see [`Runtime::subscribe`] for what
/// happens when it is full). Health and State do not use it.
const EVENT_QUEUE_CAPACITY: usize = 1024;

/// Most `Execution` events the event thread delivers in one round before it
/// goes back to the latest Health/State.
const EXECUTION_BATCH: usize = 64;

/// The latest published Health and State. Every change bumps `version` in
/// the same critical section that changes the values, and the event thread
/// copies all three at once, so the Health and State it delivers always
/// belong to one version and a newer version can never be followed by an
/// older one.
#[derive(Clone, Default)]
struct Latest {
    version: u64,
    health: HealthSnapshot,
    /// `None` until the first publish or refresh.
    state: Option<EngineState>,
}

/// Shared between the runtime and its event thread.
#[derive(Default)]
struct Outbox {
    latest: Mutex<Latest>,
    /// `Execution` events dropped on a full queue since the event thread
    /// last reported them.
    dropped: AtomicU64,
}

/// The emitters' side of a running event thread.
struct EventThread {
    /// Capacity 1: a full channel means a wake-up is already pending, which
    /// will pick up whatever changed since.
    wake: mpsc::SyncSender<()>,
    executions: mpsc::SyncSender<HistoryEntry>,
}

impl EventThread {
    /// Queues `execution` (a full queue drops and counts it) and wakes the
    /// thread, never blocking. Returns false if the thread is gone; an
    /// execution that could not be handed over is then left in `execution`.
    fn post(&self, execution: &mut Option<HistoryEntry>, dropped: &AtomicU64) -> bool {
        if let Some(entry) = execution.take() {
            match self.executions.try_send(entry) {
                Ok(()) => {}
                Err(mpsc::TrySendError::Full(_)) => {
                    dropped.fetch_add(1, Ordering::Relaxed);
                }
                Err(mpsc::TrySendError::Disconnected(entry)) => {
                    *execution = Some(entry);
                    return false;
                }
            }
        }
        !matches!(
            self.wake.try_send(()),
            Err(mpsc::TrySendError::Disconnected(()))
        )
    }
}

#[derive(Default)]
struct PauseState {
    reasons: PauseReasons,
    /// The value this runtime last wrote to `Shared::paused` (`None` until a
    /// `Shared` is attached).
    written: Option<bool>,
}

/// Lock order, for every path that holds more than one of these at a time:
/// the outbox's `latest` → `health` → `pause`. `events` is never held
/// together with any of them, and the event thread takes only `latest`,
/// just long enough to copy it. `process` is only taken with at most
/// `latest` held, and `history` alone. Listeners run on the event thread
/// with none of these held; the process probe never runs with `health` or
/// `pause` held. Each subscription's `delivered` is taken only by the event
/// thread, with none of these held.
pub struct Runtime {
    clock: Arc<dyn Clock>,
    history: History,
    listeners: Listeners,
    next_listener: AtomicU64,
    outbox: Arc<Outbox>,
    /// `None` until the first event with a listener (or after the event
    /// thread exited).
    events: Mutex<Option<EventThread>>,
    health: Mutex<HealthSnapshot>,
    pause: Mutex<PauseState>,
    shared: ArcSwapOption<Shared>,
    process: RwLock<Option<ProcessProbe>>,
}

impl Runtime {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            clock,
            history: History::new(),
            listeners: Arc::default(),
            next_listener: AtomicU64::new(1),
            outbox: Arc::default(),
            events: Mutex::new(None),
            health: Mutex::new(HealthSnapshot::default()),
            pause: Mutex::new(PauseState::default()),
            shared: ArcSwapOption::empty(),
            process: RwLock::new(None),
        }
    }

    pub fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }

    /// Connects the engine's `Shared` state. The legacy `Shared::paused` flag
    /// becomes a mirror of "any pause reason is set"; its current value is
    /// adopted as the MANUAL reason (the engine starts paused).
    pub fn attach(&self, shared: &Arc<Shared>) {
        self.shared.store(Some(Arc::clone(shared)));
        {
            let mut pause = self.pause.lock();
            pause.written = None;
            self.sync_legacy(&mut pause);
        }
        self.refresh_state();
    }

    pub fn shared(&self) -> Option<Arc<Shared>> {
        self.shared.load_full()
    }

    /// True once the attached engine has been asked to shut down.
    pub fn shutting_down(&self) -> bool {
        self.shared
            .load_full()
            .is_some_and(|shared| shared.shutdown.load(Ordering::SeqCst))
    }

    /// Sets the source of [`EngineState::current_process`].
    pub fn set_process_probe(&self, probe: ProcessProbe) {
        *self.process.write() = Some(probe);
    }

    // ---- events -------------------------------------------------------

    /// Registers `listener` for every [`EngineEvent`].
    ///
    /// **Contract: a listener must not block** (no waiting on I/O, other
    /// threads or sleeps). All listeners run one after another on the single
    /// event thread, so one that blocks holds up every subscriber. Rate
    /// limiting (P5) belongs outside the listener, e.g. forward events to
    /// your own channel and throttle there.
    ///
    /// Delivery semantics:
    /// - `Health` and `State` carry **latest values**, tracked per
    ///   subscriber. A new subscriber is first given the current Health (the
    ///   default snapshot if nothing was published yet) and then the current
    ///   State, without waiting for a change. Each subscriber eventually
    ///   sees the latest Health and State, and they only move forward: an
    ///   older value never arrives after a newer one, and a State never
    ///   arrives before the Health it was computed with. Values that are
    ///   replaced quickly (a fast round trip, or anything published while a
    ///   listener blocks) may be merged away and never delivered. A value
    ///   whose callback panicked counts as not delivered to that subscriber
    ///   and is offered again on a later wake-up.
    /// - `Execution` events are delivered in queue order, in batches of at
    ///   most `EXECUTION_BATCH` (64) with a Health/State check between
    ///   batches, so a stream of executions never delays the latest Health
    ///   and State by more than one batch. If a listener blocks, at most
    ///   `EVENT_QUEUE_CAPACITY` (1024) of them wait; beyond that they are
    ///   dropped and counted (one `runtime.events_dropped` warning per
    ///   backlog). Read the history to catch up.
    /// - There is no ordering between `Execution` and `Health`/`State`.
    ///
    /// Emitters never wait on listeners, and a blocked listener holds at
    /// most a full `Execution` queue plus one Health/State snapshot.
    pub fn subscribe(&self, listener: Box<dyn Fn(EngineEvent) + Send + Sync>) -> SubscriptionId {
        let id = self.next_listener.fetch_add(1, Ordering::Relaxed);
        self.listeners.write().push(Arc::new(Subscription {
            id,
            listener: Arc::from(listener),
            delivered: Mutex::default(),
        }));
        // The next round gives it the current Health and State.
        self.post(None);
        SubscriptionId(id)
    }

    pub fn unsubscribe(&self, id: SubscriptionId) {
        self.listeners
            .write()
            .retain(|subscription| subscription.id != id.0);
    }

    /// Wakes the event thread without blocking, first queueing `execution`
    /// if given; starts the thread on first use (or after it exited). Never
    /// called with `latest`, `health` or `pause` held.
    fn post(&self, mut execution: Option<HistoryEntry>) {
        if self.listeners.read().is_empty() {
            return;
        }
        let mut thread = self.events.lock();
        if let Some(running) = thread.as_ref() {
            if running.post(&mut execution, &self.outbox.dropped) {
                return;
            }
        }
        *thread = self.start_event_thread();
        if let Some(started) = thread.as_ref() {
            started.post(&mut execution, &self.outbox.dropped);
        }
    }

    fn start_event_thread(&self) -> Option<EventThread> {
        let (wake, wake_rx) = mpsc::sync_channel::<()>(1);
        let (executions, executions_rx) = mpsc::sync_channel(EVENT_QUEUE_CAPACITY);
        let listeners = Arc::clone(&self.listeners);
        let outbox = Arc::clone(&self.outbox);
        let spawned = std::thread::Builder::new()
            .name("qt-engine-events".into())
            .spawn(move || run_event_thread(&wake_rx, &executions_rx, &outbox, &listeners));
        match spawned {
            Ok(_) => Some(EventThread { wake, executions }),
            // Never deliver inline: a listener could re-enter the runtime.
            // Health/State stay in the outbox for the next attempt;
            // listeners resynchronise from `get_runtime_snapshot`.
            Err(_) => {
                tracing::warn!(
                    event = "runtime.event_dropped",
                    "event thread could not start; event dropped"
                );
                None
            }
        }
    }

    /// Stores `health` (if given) and `state` as the latest values, bumping
    /// the version if either changed. Returns whether it did.
    fn store_latest(
        latest: &mut Latest,
        health: Option<HealthSnapshot>,
        state: EngineState,
    ) -> bool {
        let mut changed = false;
        if let Some(health) = health {
            if latest.health != health {
                latest.health = health;
                changed = true;
            }
        }
        if latest.state.as_ref() != Some(&state) {
            latest.state = Some(state);
            changed = true;
        }
        if changed {
            latest.version += 1;
        }
        changed
    }

    // ---- health -------------------------------------------------------

    pub fn health(&self) -> HealthSnapshot {
        self.health.lock().clone()
    }

    /// Stores a new snapshot (from the supervisor). ENGINE_DOWN follows the
    /// state: set in Recovering/Failed, cleared otherwise. Subscribers get
    /// `Health` if the snapshot changed and `State` if the engine state
    /// changed (latest-value semantics, see [`Runtime::subscribe`]).
    ///
    /// The snapshot, the ENGINE_DOWN bit and the latest Health/State (with
    /// their version) are updated in one critical section (`latest` →
    /// `health` → `pause`, see [`Runtime`]), so concurrent publishers cannot
    /// leave ENGINE_DOWN disagreeing with the stored health, nor pair a
    /// Health with another publish's State.
    pub fn publish_health(&self, snapshot: HealthSnapshot) {
        let engine_down = matches!(
            snapshot.state,
            HealthState::Recovering | HealthState::Failed
        );
        let changed = {
            let mut latest = self.outbox.latest.lock();
            let (health, reasons) = {
                let mut health = self.health.lock();
                let mut pause = self.pause.lock();
                self.sync_legacy(&mut pause);
                pause.reasons = if engine_down {
                    pause.reasons.with(PauseReasons::ENGINE_DOWN)
                } else {
                    pause.reasons.without(PauseReasons::ENGINE_DOWN)
                };
                self.sync_legacy(&mut pause);
                health.clone_from(&snapshot);
                (health.state, pause.reasons)
            };
            let state = self.state_from(health, reasons);
            Self::store_latest(&mut latest, Some(snapshot), state)
        };
        if changed {
            self.post(None);
        }
    }

    // ---- pause --------------------------------------------------------

    /// Current pause reasons (adopting any legacy write to `Shared::paused`).
    pub fn pause_reasons(&self) -> PauseReasons {
        let mut pause = self.pause.lock();
        self.sync_legacy(&mut pause);
        pause.reasons
    }

    /// Sets or clears user-facing pause reasons and returns the result.
    /// ENGINE_DOWN is owned by the supervisor and ignored here.
    pub fn set_pause(&self, reason: PauseReasons, on: bool) -> PauseReasons {
        let reason = reason.without(PauseReasons::ENGINE_DOWN);
        let reasons = self.update_pause(|reasons| {
            if on {
                reasons.with(reason)
            } else {
                reasons.without(reason)
            }
        });
        self.refresh_state();
        reasons
    }

    fn update_pause(&self, change: impl FnOnce(PauseReasons) -> PauseReasons) -> PauseReasons {
        let mut pause = self.pause.lock();
        self.sync_legacy(&mut pause);
        pause.reasons = change(pause.reasons);
        self.sync_legacy(&mut pause);
        pause.reasons
    }

    /// Two-way bridge to the legacy `Shared::paused` flag, which detection
    /// and `guarded_context` still read and older callers (main.rs
    /// `set_paused`/publish, `start_recording`, the RawInput loop) still
    /// write. A value that differs from what this runtime last wrote came
    /// from such a writer and is adopted as MANUAL on/off; then the flag is
    /// set to "any reason present". The compare-exchange retries if a legacy
    /// writer races the update.
    ///
    /// A legacy write of the value the flag already holds is invisible (for
    /// example a legacy pause while ENGINE_DOWN already holds the flag), and
    /// main.rs `publish` restores its saved value afterwards; both are
    /// reasons for callers to move to [`Runtime::set_pause`].
    fn sync_legacy(&self, pause: &mut PauseState) {
        let Some(shared) = self.shared.load_full() else {
            return;
        };
        loop {
            let current = shared.paused.load(Ordering::SeqCst);
            if pause.written != Some(current) {
                pause.reasons = if current {
                    pause.reasons.with(PauseReasons::MANUAL)
                } else {
                    pause.reasons.without(PauseReasons::MANUAL)
                };
            }
            let want = !pause.reasons.is_empty();
            if shared
                .paused
                .compare_exchange(current, want, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                pause.written = Some(want);
                return;
            }
        }
    }

    // ---- engine state -------------------------------------------------

    pub fn engine_state(&self) -> EngineState {
        let (health, reasons) = self.health_and_pause();
        self.state_from(health, reasons)
    }

    /// Health state and pause reasons read together (`health` → `pause`),
    /// so the pair is one that `publish_health` actually left behind.
    fn health_and_pause(&self) -> (HealthState, PauseReasons) {
        let health = self.health.lock();
        let mut pause = self.pause.lock();
        self.sync_legacy(&mut pause);
        (health.state, pause.reasons)
    }

    fn busy(&self) -> bool {
        self.shared
            .load_full()
            .is_some_and(|shared| shared.busy.load(Ordering::SeqCst))
    }

    /// Builds the engine state; calls the process probe, so never call it
    /// with `health` or `pause` held.
    fn state_from(&self, health: HealthState, paused_reasons: PauseReasons) -> EngineState {
        let probe = self.process.read().clone();
        EngineState {
            listening: health.accepts_resume() && paused_reasons.is_empty(),
            paused_reasons,
            busy: self.busy(),
            current_process: probe.and_then(|probe| probe()),
        }
    }

    /// Recomputes the engine state and, if it changed, stores it as the
    /// latest State (under `latest`, so it is versioned with the health it
    /// was computed from) and wakes the event thread.
    pub fn refresh_state(&self) {
        let changed = {
            let mut latest = self.outbox.latest.lock();
            let (health, reasons) = self.health_and_pause();
            let state = self.state_from(health, reasons);
            Self::store_latest(&mut latest, None, state)
        };
        if changed {
            self.post(None);
        }
    }

    /// The §7.2 mapping-table row for the current health, pause and busy.
    pub fn status_text(&self) -> StatusText {
        let (health, reasons) = self.health_and_pause();
        StatusText::resolve(health, reasons, self.busy())
    }

    // ---- history ------------------------------------------------------

    /// Appends one execution to the history and emits `Execution`.
    pub fn record(&self, record: Record) -> HistoryEntry {
        let entry = self
            .history
            .push(record, self.clock.now(), self.clock.wall_ms());
        self.post(Some(entry.clone()));
        entry
    }

    pub fn list_history(&self, since_seq: Option<u64>) -> Vec<HistoryEntry> {
        self.history.list(since_seq, self.clock.now())
    }

    pub fn clear_history(&self) {
        self.history.clear();
    }

    pub fn explain_last_miss(&self) -> Option<MissExplanation> {
        self.history.last_miss(self.clock.now())
    }

    /// Highest history `seq` assigned so far (`get_runtime_snapshot`).
    pub fn history_seq(&self) -> u64 {
        self.history.last_seq()
    }
}

/// The event thread. Each wake-up first takes the latest Health/State (one
/// copy under `latest`, so both come from the same version) and the
/// subscriber list (once, so a Health and its State go to the same
/// subscribers), and brings every listed subscriber up to date, Health
/// before State (see [`Subscription::catch_up`]); then it drains the
/// `Execution` queue, at most [`EXECUTION_BATCH`] entries, and reports
/// drops. If that left entries queued, the next round starts at once
/// instead of waiting for a wake-up, so a steady stream of executions
/// cannot hold back the latest Health/State. The wake-up token is consumed
/// before `latest` is read, so a change made after that read (or a
/// subscriber added after the list was taken, whose `subscribe` sends a
/// token) is picked up by the next round. Each `Execution` goes to the
/// subscribers listed when it is taken from the queue, so a subscriber gets
/// every execution recorded after its `subscribe` returned. Exits when the
/// runtime (and with it the sending side) is gone.
fn run_event_thread(
    wake: &mpsc::Receiver<()>,
    executions: &mpsc::Receiver<HistoryEntry>,
    outbox: &Outbox,
    listeners: &RwLock<Vec<Arc<Subscription>>>,
) {
    let mut backlog = false;
    loop {
        if backlog {
            // Take a pending wake-up (if any) before reading `latest`.
            if matches!(wake.try_recv(), Err(mpsc::TryRecvError::Disconnected)) {
                return;
            }
        } else if wake.recv().is_err() {
            return;
        }
        let latest = outbox.latest.lock().clone();
        // Bound first: no listener runs with the list locked.
        let subscribers = listeners.read().clone();
        for subscriber in &subscribers {
            subscriber.catch_up(&latest);
        }
        let mut drained = 0;
        while drained < EXECUTION_BATCH {
            let Ok(entry) = executions.try_recv() else {
                break;
            };
            drained += 1;
            let event = EngineEvent::Execution(entry);
            let subscribers = listeners.read().clone();
            for subscriber in &subscribers {
                subscriber.call(event.clone());
            }
        }
        backlog = drained == EXECUTION_BATCH;
        let dropped = outbox.dropped.swap(0, Ordering::Relaxed);
        if dropped > 0 {
            tracing::warn!(
                event = "runtime.events_dropped",
                dropped,
                "execution queue was full; execution events dropped"
            );
        }
    }
}

static RUNTIME: ArcSwapOption<Runtime> = ArcSwapOption::const_empty();

/// The process-wide runtime, created on first use with the system clock.
pub fn current() -> Arc<Runtime> {
    if let Some(runtime) = RUNTIME.load_full() {
        return runtime;
    }
    static INIT: Mutex<()> = parking_lot::const_mutex(());
    let _init = INIT.lock();
    if let Some(runtime) = RUNTIME.load_full() {
        return runtime;
    }
    let runtime = Arc::new(Runtime::new(Arc::new(SystemClock)));
    RUNTIME.store(Some(Arc::clone(&runtime)));
    runtime
}

/// Health snapshot for `get_health` / `get_runtime_snapshot`.
pub fn current_health() -> HealthSnapshot {
    current().health()
}

/// Whether the user may resume listening; replaces `native_ready()`.
pub fn accepts_resume() -> bool {
    current().health().state.accepts_resume()
}

pub fn pause_reasons() -> PauseReasons {
    current().pause_reasons()
}

/// Sets or clears a pause reason (`set_paused` maps to MANUAL).
/// ENGINE_DOWN is ignored: the supervisor owns it.
pub fn set_pause(reason: PauseReasons, on: bool) -> PauseReasons {
    current().set_pause(reason, on)
}

pub fn engine_state() -> EngineState {
    current().engine_state()
}

pub fn status_text() -> StatusText {
    current().status_text()
}

/// Registers an event listener (P5 connects the Tauri EventBus here). The
/// listener must not block; see [`Runtime::subscribe`] for the contract.
pub fn subscribe(listener: Box<dyn Fn(EngineEvent) + Send + Sync>) -> SubscriptionId {
    current().subscribe(listener)
}

pub fn unsubscribe(id: SubscriptionId) {
    current().unsubscribe(id)
}

pub fn list_history(since_seq: Option<u64>) -> Vec<HistoryEntry> {
    current().list_history(since_seq)
}

pub fn clear_history() {
    current().clear_history()
}

pub fn explain_last_miss() -> Option<MissExplanation> {
    current().explain_last_miss()
}

pub fn history_seq() -> u64 {
    current().history_seq()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::{Outcome, ReasonCode, TriggerKind};
    use crate::mock::FakeClock;
    use std::time::{Duration, Instant};

    fn runtime() -> (Arc<Runtime>, Arc<FakeClock>) {
        let clock = Arc::new(FakeClock::new());
        (Arc::new(Runtime::new(clock.clone())), clock)
    }

    fn collect(runtime: &Runtime) -> Arc<Mutex<Vec<EngineEvent>>> {
        let events = Arc::new(Mutex::new(vec![]));
        let sink = Arc::clone(&events);
        runtime.subscribe(Box::new(move |event| sink.lock().push(event)));
        events
    }

    fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn snapshot(state: HealthState) -> HealthSnapshot {
        HealthSnapshot {
            state,
            components: vec![],
        }
    }

    #[test]
    fn engine_down_follows_health_and_keeps_user_reasons() {
        let (runtime, _) = runtime();
        runtime.set_pause(PauseReasons::MANUAL, true);
        runtime.publish_health(snapshot(HealthState::Recovering));
        assert_eq!(
            runtime.pause_reasons(),
            PauseReasons::MANUAL.with(PauseReasons::ENGINE_DOWN)
        );
        runtime.publish_health(snapshot(HealthState::Failed));
        assert!(runtime.pause_reasons().contains(PauseReasons::ENGINE_DOWN));
        runtime.publish_health(snapshot(HealthState::Healthy));
        assert_eq!(runtime.pause_reasons(), PauseReasons::MANUAL);
        // Starting carries no pause reason (mapping table row 1).
        runtime.publish_health(snapshot(HealthState::Starting));
        assert_eq!(runtime.pause_reasons(), PauseReasons::MANUAL);
    }

    /// Two publishers alternate Recovering/Healthy while a reader samples
    /// health and pause together: ENGINE_DOWN always matches the stored
    /// health (during and after), and MANUAL never moves.
    #[test]
    fn concurrent_publishers_keep_engine_down_with_health() {
        use std::sync::atomic::AtomicBool;
        use HealthState::{Healthy, Recovering};
        let (runtime, _) = runtime();
        let _events = collect(&runtime);
        for manual in [false, true] {
            runtime.set_pause(PauseReasons::MANUAL, manual);
            let stop = Arc::new(AtomicBool::new(false));
            let reader = {
                let (runtime, stop) = (Arc::clone(&runtime), Arc::clone(&stop));
                std::thread::spawn(move || {
                    let mut reads = 0u64;
                    while !stop.load(Ordering::SeqCst) || reads == 0 {
                        let (health, reasons) = runtime.health_and_pause();
                        assert_eq!(
                            reasons.contains(PauseReasons::ENGINE_DOWN),
                            health == Recovering,
                            "{health:?} {reasons:?}"
                        );
                        assert_eq!(reasons.contains(PauseReasons::MANUAL), manual);
                        reads += 1;
                    }
                    reads
                })
            };
            let publishers: Vec<_> = [(Recovering, Healthy), (Healthy, Recovering)]
                .into_iter()
                .map(|(even, odd)| {
                    let runtime = Arc::clone(&runtime);
                    std::thread::spawn(move || {
                        for i in 0..2000 {
                            let state = if i % 2 == 0 { even } else { odd };
                            runtime.publish_health(snapshot(state));
                        }
                    })
                })
                .collect();
            for publisher in publishers {
                publisher.join().unwrap();
            }
            stop.store(true, Ordering::SeqCst);
            assert!(reader.join().unwrap() > 0);
            let last = runtime.health().state;
            let reasons = runtime.pause_reasons();
            assert_eq!(
                reasons.contains(PauseReasons::ENGINE_DOWN),
                last == Recovering,
                "final {last:?} {reasons:?}"
            );
            assert_eq!(reasons.contains(PauseReasons::MANUAL), manual);
            assert_eq!(
                runtime.status_text(),
                match (last, manual) {
                    (Recovering, _) => StatusText::Recovering,
                    (_, true) => StatusText::Paused,
                    _ => StatusText::Listening,
                }
            );
        }
    }

    #[test]
    fn set_pause_cannot_touch_engine_down() {
        let (runtime, _) = runtime();
        runtime.publish_health(snapshot(HealthState::Recovering));
        let all = PauseReasons::from_bits_truncate(u32::MAX);
        assert_eq!(
            runtime.set_pause(all, false),
            PauseReasons::ENGINE_DOWN,
            "clearing every bit keeps the supervisor's ENGINE_DOWN"
        );
        runtime.publish_health(snapshot(HealthState::Healthy));
        assert_eq!(
            runtime.set_pause(PauseReasons::ENGINE_DOWN, true),
            PauseReasons::empty()
        );
        assert_eq!(
            runtime.set_pause(PauseReasons::TIMED.with(PauseReasons::MANUAL), true),
            PauseReasons::TIMED.with(PauseReasons::MANUAL)
        );
        assert_eq!(
            runtime.set_pause(PauseReasons::MANUAL, false),
            PauseReasons::TIMED
        );
    }

    #[test]
    fn legacy_paused_flag_is_mirrored_and_adopted_as_manual() {
        let (runtime, _) = runtime();
        let shared = Shared::new();
        assert!(shared.paused.load(Ordering::SeqCst), "engine starts paused");
        runtime.attach(&shared);
        assert_eq!(runtime.pause_reasons(), PauseReasons::MANUAL);

        // A legacy resume (main.rs set_paused(false)) clears MANUAL.
        shared.paused.store(false, Ordering::SeqCst);
        assert_eq!(runtime.pause_reasons(), PauseReasons::empty());
        assert!(!runtime.engine_state().listening, "still Starting");

        // ENGINE_DOWN holds the flag even against a legacy resume.
        runtime.publish_health(snapshot(HealthState::Recovering));
        assert!(shared.paused.load(Ordering::SeqCst));
        shared.paused.store(false, Ordering::SeqCst);
        assert_eq!(runtime.pause_reasons(), PauseReasons::ENGINE_DOWN);
        assert!(shared.paused.load(Ordering::SeqCst), "re-asserted");

        // Recovery clears only ENGINE_DOWN; a MANUAL pause set meanwhile stays.
        runtime.set_pause(PauseReasons::MANUAL, true);
        runtime.publish_health(snapshot(HealthState::Healthy));
        assert_eq!(runtime.pause_reasons(), PauseReasons::MANUAL);
        assert!(shared.paused.load(Ordering::SeqCst));

        // A legacy pause/resume while healthy toggles MANUAL only.
        runtime.set_pause(PauseReasons::TIMED, true);
        shared.paused.store(false, Ordering::SeqCst);
        assert_eq!(runtime.pause_reasons(), PauseReasons::TIMED);
        assert!(shared.paused.load(Ordering::SeqCst), "TIMED still holds it");
        runtime.set_pause(PauseReasons::TIMED, false);
        assert!(!shared.paused.load(Ordering::SeqCst));
        shared.paused.store(true, Ordering::SeqCst);
        assert_eq!(runtime.pause_reasons(), PauseReasons::MANUAL);
        runtime.set_pause(PauseReasons::MANUAL, false);
        assert!(!shared.paused.load(Ordering::SeqCst));
        let state = runtime.engine_state();
        assert!(state.listening);
        assert_eq!(state.paused_reasons, PauseReasons::empty());
    }

    #[test]
    fn events_are_delivered_once_per_change() {
        let (runtime, _) = runtime();
        let events = collect(&runtime);
        // A new subscriber first gets the current (here default) Health.
        wait_until("initial health", || !events.lock().is_empty());
        assert_eq!(
            events.lock().drain(..).collect::<Vec<_>>(),
            [EngineEvent::Health(HealthSnapshot::default())]
        );
        runtime.publish_health(snapshot(HealthState::Healthy));
        runtime.publish_health(snapshot(HealthState::Healthy));
        // Health/State are latest values: wait for this State before the
        // next change, or the two could be merged into one delivery.
        wait_until("health and state", || events.lock().len() >= 2);
        runtime.set_pause(PauseReasons::MANUAL, true);
        runtime.set_pause(PauseReasons::MANUAL, true);
        wait_until("three events", || events.lock().len() >= 3);
        std::thread::sleep(Duration::from_millis(20));
        let events = events.lock().clone();
        assert_eq!(events.len(), 3, "{events:?}");
        assert_eq!(
            events[0],
            EngineEvent::Health(snapshot(HealthState::Healthy))
        );
        let EngineEvent::State(listening) = &events[1] else {
            panic!("{events:?}")
        };
        assert!(listening.listening);
        let EngineEvent::State(paused) = &events[2] else {
            panic!("{events:?}")
        };
        assert!(!paused.listening);
        assert_eq!(paused.paused_reasons, PauseReasons::MANUAL);
    }

    #[test]
    fn panicking_listener_does_not_stop_others_and_unsubscribe_works() {
        let (runtime, _) = runtime();
        let bad = runtime.subscribe(Box::new(|_| panic!("listener bug")));
        let events = collect(&runtime);
        runtime.record(Record::new(
            "a",
            TriggerKind::Immediate,
            Outcome::Succeeded,
            ReasonCode::Ok,
        ));
        runtime.record(Record::new(
            "b",
            TriggerKind::Immediate,
            Outcome::Succeeded,
            ReasonCode::Ok,
        ));
        wait_until("both entries", || entries(&events.lock()).len() == 2);
        runtime.unsubscribe(bad);
        runtime.record(Record::new(
            "c",
            TriggerKind::Immediate,
            Outcome::Succeeded,
            ReasonCode::Ok,
        ));
        wait_until("third entry", || entries(&events.lock()).len() == 3);
        let seqs: Vec<u64> = entries(&events.lock())
            .iter()
            .map(|entry| entry.seq)
            .collect();
        assert_eq!(seqs, [1, 2, 3]);
        // The other subscriber also got its initial Health, once.
        assert_eq!(
            healths(&events.lock()),
            [HealthSnapshot::default()],
            "{:?}",
            events.lock()
        );
    }

    /// `record` is the "new history entry" callback: it emits `Execution`
    /// carrying exactly the entry it returns.
    #[test]
    fn record_emits_execution_with_the_returned_entry() {
        let (runtime, _) = runtime();
        let events = collect(&runtime);
        let entry = runtime.record(Record::new(
            "a",
            TriggerKind::Terminator,
            Outcome::Failed,
            ReasonCode::FocusChanged,
        ));
        wait_until("execution event", || !entries(&events.lock()).is_empty());
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(entries(&events.lock()), [entry]);
    }

    /// A unique snapshot per `n` (carried in `since_ms`), so the order in
    /// which listeners see them can be checked.
    fn numbered(state: HealthState, n: usize) -> HealthSnapshot {
        HealthSnapshot {
            state,
            components: vec![crate::health::ComponentHealth {
                component: crate::health::Component::RawInput,
                state,
                reason: None,
                since_ms: n as i64,
            }],
        }
    }

    fn execution(rule: &str) -> Record {
        Record::new(
            rule,
            TriggerKind::Immediate,
            Outcome::Succeeded,
            ReasonCode::Ok,
        )
    }

    fn healths(events: &[EngineEvent]) -> Vec<HealthSnapshot> {
        events
            .iter()
            .filter_map(|event| match event {
                EngineEvent::Health(health) => Some(health.clone()),
                _ => None,
            })
            .collect()
    }

    fn states(events: &[EngineEvent]) -> Vec<EngineState> {
        events
            .iter()
            .filter_map(|event| match event {
                EngineEvent::State(state) => Some(state.clone()),
                _ => None,
            })
            .collect()
    }

    fn entries(events: &[EngineEvent]) -> Vec<HistoryEntry> {
        events
            .iter()
            .filter_map(|event| match event {
                EngineEvent::Execution(entry) => Some(entry.clone()),
                _ => None,
            })
            .collect()
    }

    /// Subscribes a listener that records every event and blocks the event
    /// thread inside the first one (its initial Health). Send on the
    /// returned sender to let it go.
    fn block_event_thread(runtime: &Runtime) -> (mpsc::Sender<()>, Arc<Mutex<Vec<EngineEvent>>>) {
        use std::sync::atomic::AtomicBool;
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = Mutex::new(release_rx);
        let blocked_once = AtomicBool::new(false);
        let events = Arc::new(Mutex::new(vec![]));
        let sink = Arc::clone(&events);
        runtime.subscribe(Box::new(move |event| {
            sink.lock().push(event);
            if !blocked_once.swap(true, Ordering::SeqCst) {
                entered_tx.send(()).unwrap();
                release_rx.lock().recv().unwrap();
            }
        }));
        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("event thread blocked in the listener");
        (release_tx, events)
    }

    /// The event thread is held up before it reads the snapshot while
    /// H1/S1 and then H2/S2 are published: once it runs again, subscribers
    /// get exactly H2 and S2. H1/S1 are merged away, so H1 can never show up
    /// after H2.
    #[test]
    fn delivery_after_a_stall_starts_from_the_latest_snapshot() {
        let (runtime, _) = runtime();
        let (release, _) = block_event_thread(&runtime);
        let events = collect(&runtime);
        let h1 = numbered(HealthState::Recovering, 1);
        runtime.publish_health(h1);
        let s1 = runtime.engine_state();
        let h2 = numbered(HealthState::Healthy, 2);
        runtime.publish_health(h2.clone());
        let s2 = runtime.engine_state();
        assert_ne!(s1, s2);
        release.send(()).unwrap();

        wait_until("H2 and S2", || {
            let events = events.lock();
            healths(&events).last() == Some(&h2) && states(&events).last() == Some(&s2)
        });
        std::thread::sleep(Duration::from_millis(50));
        let events = events.lock().clone();
        assert_eq!(healths(&events), [h2]);
        assert_eq!(states(&events), [s2]);
    }

    /// A subscriber that arrives after the values were delivered to another
    /// one still gets them, Health first, without any further change; the
    /// earlier subscriber gets nothing again.
    #[test]
    fn a_late_subscriber_first_gets_the_current_health_then_state() {
        let (runtime, _) = runtime();
        let early = collect(&runtime);
        let h1 = numbered(HealthState::Healthy, 1);
        runtime.publish_health(h1.clone());
        let s1 = runtime.engine_state();
        wait_until("early subscriber up to date", || {
            let early = early.lock();
            healths(&early).last() == Some(&h1) && states(&early).last() == Some(&s1)
        });
        let early_seen = early.lock().len();

        let late = collect(&runtime);
        wait_until("late subscriber initialised", || late.lock().len() >= 2);
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(
            *late.lock(),
            [EngineEvent::Health(h1), EngineEvent::State(s1)]
        );
        assert_eq!(early.lock().len(), early_seen, "nothing sent again");
    }

    /// B subscribes while A is paused inside its Health callback: B is not
    /// part of that round, so it never gets the State without the Health;
    /// the next round gives it both, Health first.
    #[test]
    fn a_subscriber_joining_during_a_health_callback_gets_health_before_state() {
        let (runtime, _) = runtime();
        let h1 = numbered(HealthState::Healthy, 1);
        let (entered_tx, entered) = mpsc::channel();
        let (release, release_rx) = mpsc::channel::<()>();
        let release_rx = Mutex::new(release_rx);
        let pause_on = EngineEvent::Health(h1.clone());
        runtime.subscribe(Box::new(move |event| {
            if event == pause_on {
                let _ = entered_tx.send(());
                let _ = release_rx.lock().recv();
            }
        }));
        runtime.publish_health(h1.clone());
        let s1 = runtime.engine_state();
        entered
            .recv_timeout(Duration::from_secs(5))
            .expect("A paused in its Health callback");

        let late = collect(&runtime);
        release.send(()).unwrap();
        wait_until("B gets H1 and S1", || late.lock().len() >= 2);
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(
            *late.lock(),
            [EngineEvent::Health(h1), EngineEvent::State(s1)]
        );
    }

    /// A Health whose callback panicked does not count as delivered: no
    /// State follows it, and the next wake-up offers both again.
    #[test]
    fn a_value_whose_callback_panicked_is_offered_again() {
        use std::sync::atomic::AtomicBool;
        let (runtime, _) = runtime();
        let h1 = numbered(HealthState::Healthy, 1);
        runtime.publish_health(h1.clone());
        let s1 = runtime.engine_state();
        let panicked = Arc::new(AtomicBool::new(false));
        let events = Arc::new(Mutex::new(vec![]));
        let (once, sink) = (Arc::clone(&panicked), Arc::clone(&events));
        runtime.subscribe(Box::new(move |event| {
            if matches!(event, EngineEvent::Health(_)) && !once.swap(true, Ordering::SeqCst) {
                panic!("listener bug on the first Health");
            }
            sink.lock().push(event);
        }));
        wait_until("first Health panicked", || panicked.load(Ordering::SeqCst));
        std::thread::sleep(Duration::from_millis(20));
        assert!(events.lock().is_empty(), "{:?}", events.lock());

        let entry = runtime.record(execution("wake"));
        wait_until("offered again", || events.lock().len() >= 3);
        assert_eq!(
            *events.lock(),
            [
                EngineEvent::Health(h1),
                EngineEvent::State(s1),
                EngineEvent::Execution(entry)
            ]
        );
    }

    /// Publishers race Recovering/Healthy and MANUAL on/off against a slow
    /// listener. Every State the listener gets agrees with the Health it got
    /// last (ENGINE_DOWN iff Recovering), so no State ever overtakes its
    /// Health, and the last values delivered are the runtime's final ones.
    #[test]
    fn health_and_state_are_delivered_from_one_version() {
        use HealthState::{Healthy, Recovering};
        let (runtime, _) = runtime();
        let mismatches = Arc::new(Mutex::new(Vec::<String>::new()));
        let events = Arc::new(Mutex::new(Vec::<EngineEvent>::new()));
        {
            let (mismatches, events) = (Arc::clone(&mismatches), Arc::clone(&events));
            let last_health = Mutex::new(HealthSnapshot::default());
            runtime.subscribe(Box::new(move |event| {
                match &event {
                    EngineEvent::Health(health) => *last_health.lock() = health.clone(),
                    EngineEvent::State(state) => {
                        let health = last_health.lock().state;
                        if state.paused_reasons.contains(PauseReasons::ENGINE_DOWN)
                            != (health == Recovering)
                        {
                            mismatches.lock().push(format!("{health:?} {state:?}"));
                        }
                    }
                    EngineEvent::Execution(_) => {}
                }
                events.lock().push(event);
                std::thread::sleep(Duration::from_micros(200));
            }));
        }
        let mut workers: Vec<_> = [(Recovering, Healthy), (Healthy, Recovering)]
            .into_iter()
            .map(|(even, odd)| {
                let runtime = Arc::clone(&runtime);
                std::thread::spawn(move || {
                    for i in 0..2000 {
                        runtime.publish_health(snapshot(if i % 2 == 0 { even } else { odd }));
                    }
                })
            })
            .collect();
        workers.push({
            let runtime = Arc::clone(&runtime);
            std::thread::spawn(move || {
                for i in 0..2000 {
                    runtime.set_pause(PauseReasons::MANUAL, i % 2 == 0);
                }
            })
        });
        for worker in workers {
            worker.join().unwrap();
        }
        let (final_health, final_state) = (runtime.health(), runtime.engine_state());
        wait_until("final health and state", || {
            let events = events.lock();
            healths(&events).last() == Some(&final_health)
                && states(&events).last() == Some(&final_state)
        });
        std::thread::sleep(Duration::from_millis(50));
        let events = events.lock().clone();
        assert_eq!(healths(&events).last(), Some(&final_health));
        assert_eq!(states(&events).last(), Some(&final_state));
        assert!(
            states(&events).len() > 1,
            "{} states",
            states(&events).len()
        );
        let mismatches = mismatches.lock();
        assert!(mismatches.is_empty(), "{:?}", &mismatches[..]);
    }

    /// One listener blocks the event thread while events keep coming. The
    /// backlog is bounded: at most `EVENT_QUEUE_CAPACITY` executions wait
    /// (the rest are dropped) and Health/State occupy one snapshot, so after
    /// the block every subscriber gets the queued executions in order plus
    /// exactly one Health and one State, the latest (the blocked one also
    /// had its initial Health, where it blocked).
    #[test]
    fn blocked_listener_holds_a_bounded_backlog() {
        let (runtime, _) = runtime();
        let (release, blocked) = block_event_thread(&runtime);
        let events = collect(&runtime);
        let executions = 3 * EVENT_QUEUE_CAPACITY;
        let states_cycle = [
            HealthState::Healthy,
            HealthState::Recovering,
            HealthState::Degraded,
        ];
        for i in 0..executions {
            runtime.record(execution("burst"));
            if i % 4 == 0 {
                runtime.publish_health(numbered(states_cycle[i % 3], i));
                runtime.set_pause(PauseReasons::MANUAL, i % 8 == 0);
            }
        }
        let final_health = numbered(HealthState::Degraded, executions + 1);
        runtime.publish_health(final_health.clone());
        runtime.set_pause(PauseReasons::TIMED, true);
        let final_state = runtime.engine_state();
        assert!(!final_state.listening);
        // Health/State wait as one snapshot, whatever happened meanwhile.
        assert_eq!(runtime.outbox.latest.lock().health, final_health);
        assert_eq!(
            runtime.outbox.latest.lock().state.as_ref(),
            Some(&final_state)
        );
        release.send(()).unwrap();

        wait_until("latest health and state, queued executions", || {
            let events = events.lock();
            healths(&events).last() == Some(&final_health)
                && states(&events).last() == Some(&final_state)
                && entries(&events).len() == EVENT_QUEUE_CAPACITY
        });
        std::thread::sleep(Duration::from_millis(50));
        let events = events.lock().clone();
        assert_eq!(healths(&events), std::slice::from_ref(&final_health));
        assert_eq!(states(&events), std::slice::from_ref(&final_state));
        let seqs: Vec<u64> = entries(&events).iter().map(|entry| entry.seq).collect();
        // Exactly a full queue: the start of the burst, in order; the rest
        // was dropped.
        let capacity = u64::try_from(EVENT_QUEUE_CAPACITY).unwrap();
        assert_eq!(seqs, (1..=capacity).collect::<Vec<_>>());
        assert_eq!(events.len(), EVENT_QUEUE_CAPACITY + 2);
        assert_eq!(
            runtime.outbox.dropped.load(Ordering::Relaxed),
            0,
            "reported"
        );
        // The blocked subscriber: its initial Health, then the same.
        let blocked = blocked.lock().clone();
        assert_eq!(
            healths(&blocked),
            [HealthSnapshot::default(), final_health.clone()]
        );
        assert_eq!(states(&blocked), std::slice::from_ref(&final_state));
        assert_eq!(entries(&blocked), entries(&events));

        // The thread keeps working normally once the backlog is gone.
        let sink = collect(&runtime);
        let late = runtime.record(execution("late"));
        wait_until("late subscriber caught up", || {
            let sink = sink.lock();
            !entries(&sink).is_empty() && !states(&sink).is_empty()
        });
        std::thread::sleep(Duration::from_millis(20));
        let sink = sink.lock().clone();
        assert_eq!(entries(&sink), [late]);
        assert_eq!(healths(&sink), [final_health]);
        assert_eq!(states(&sink), [final_state]);
    }

    /// Every Execution callback records another execution, so the queue is
    /// never empty. A Health published meanwhile still goes out while that
    /// stream keeps running.
    #[test]
    fn a_stream_of_executions_does_not_hold_back_health() {
        use std::sync::atomic::{AtomicBool, AtomicUsize};
        let (runtime, _) = runtime();
        let final_health = numbered(HealthState::Degraded, 9);
        let seen = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let delivered = Arc::new(AtomicBool::new(false));
        {
            let producer = Arc::downgrade(&runtime);
            let seen = Arc::clone(&seen);
            let stop = Arc::clone(&stop);
            let delivered = Arc::clone(&delivered);
            let final_health = final_health.clone();
            runtime.subscribe(Box::new(move |event| match event {
                EngineEvent::Execution(_) => {
                    seen.fetch_add(1, Ordering::SeqCst);
                    if stop.load(Ordering::SeqCst) {
                        return;
                    }
                    if let Some(runtime) = producer.upgrade() {
                        runtime.record(execution("refill"));
                    }
                }
                EngineEvent::Health(health) if health == final_health => {
                    delivered.store(true, Ordering::SeqCst);
                }
                _ => {}
            }));
        }
        runtime.record(execution("first"));
        wait_until("a running stream", || {
            seen.load(Ordering::SeqCst) > 4 * EXECUTION_BATCH
        });
        runtime.publish_health(final_health);
        wait_until("the final health while the stream runs", || {
            delivered.load(Ordering::SeqCst)
        });
        let at_delivery = seen.load(Ordering::SeqCst);
        wait_until("the stream still running", || {
            seen.load(Ordering::SeqCst) > at_delivery + EXECUTION_BATCH
        });
        stop.store(true, Ordering::SeqCst);
        let stopped = seen.load(Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(20));
        assert!(
            seen.load(Ordering::SeqCst) <= stopped + 1,
            "producer stopped"
        );
    }

    #[test]
    fn history_api_uses_the_runtime_clock() {
        let (runtime, clock) = runtime();
        runtime.record(Record::new(
            "miss",
            TriggerKind::Terminator,
            Outcome::Failed,
            ReasonCode::FocusChanged,
        ));
        clock.advance(Duration::from_millis(7));
        let ok = runtime.record(Record::new(
            "ok",
            TriggerKind::Immediate,
            Outcome::Succeeded,
            ReasonCode::Ok,
        ));
        assert_eq!(ok.at_ms, FakeClock::WALL_START_MS + 7);
        assert_eq!(runtime.history_seq(), 2);
        assert_eq!(runtime.list_history(Some(1)), vec![ok]);
        let miss = runtime.explain_last_miss().unwrap();
        assert_eq!((miss.seq, miss.reason), (1, ReasonCode::FocusChanged));
        clock.advance(crate::history::MAX_AGE);
        assert_eq!(runtime.list_history(None).len(), 1, "the miss aged out");
        assert_eq!(runtime.explain_last_miss(), None);
        runtime.clear_history();
        assert!(runtime.list_history(None).is_empty());
        assert_eq!(runtime.history_seq(), 2);
    }

    #[test]
    fn status_text_and_state_follow_health_pause_and_busy() {
        let (runtime, _) = runtime();
        let shared = Shared::new();
        shared.paused.store(false, Ordering::SeqCst);
        runtime.attach(&shared);
        runtime.set_process_probe(Arc::new(|| Some("notepad.exe".into())));
        assert_eq!(runtime.status_text(), StatusText::Starting);
        runtime.publish_health(snapshot(HealthState::Healthy));
        assert_eq!(runtime.status_text(), StatusText::Listening);
        shared.busy.store(true, Ordering::SeqCst);
        assert_eq!(runtime.status_text(), StatusText::Executing);
        let state = runtime.engine_state();
        assert!(state.busy && state.listening);
        assert_eq!(state.current_process.as_deref(), Some("notepad.exe"));
        runtime.publish_health(snapshot(HealthState::Recovering));
        assert_eq!(runtime.status_text(), StatusText::Recovering);
        runtime.publish_health(snapshot(HealthState::Degraded));
        runtime.set_pause(PauseReasons::SAFE_MODE, true);
        assert_eq!(runtime.status_text(), StatusText::Paused);
    }
}
