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

type Listeners = Arc<RwLock<Vec<(u64, Listener)>>>;

/// Capacity of the queue between emitters and the event thread. Bounded so
/// a blocked listener cannot grow memory without limit; emitters never wait
/// on it (see [`Runtime::subscribe`] for what happens when it is full).
const EVENT_QUEUE_CAPACITY: usize = 1024;

/// What did not fit in the event queue, shared between emitters and the
/// event thread.
#[derive(Default)]
struct Overflow {
    /// Latest Health/State that could not be queued. While one is parked,
    /// later events of the same kind replace it instead of being queued, so
    /// the queue never holds a newer value than the parked one.
    health: Option<HealthSnapshot>,
    state: Option<EngineState>,
    /// Execution events dropped since the event thread last drained.
    dropped: u64,
}

#[derive(Default)]
struct EventQueue {
    tx: Option<mpsc::SyncSender<EngineEvent>>,
    overflow: Arc<Mutex<Overflow>>,
}

#[derive(Default)]
struct PauseState {
    reasons: PauseReasons,
    /// The value this runtime last wrote to `Shared::paused` (`None` until a
    /// `Shared` is attached).
    written: Option<bool>,
}

/// Lock order, for every path that holds more than one of these at a time:
/// `last_state` → `health` → `pause` → `events` → the event queue's
/// `Overflow` (the event thread takes only `Overflow`). `process` is only taken
/// with at most `last_state` held, and `history` alone. Listeners run on the
/// event thread with none of these held; the process probe never runs with
/// `health`, `pause` or `events` held.
pub struct Runtime {
    clock: Arc<dyn Clock>,
    history: History,
    listeners: Listeners,
    next_listener: AtomicU64,
    events: Mutex<EventQueue>,
    health: Mutex<HealthSnapshot>,
    pause: Mutex<PauseState>,
    last_state: Mutex<Option<EngineState>>,
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
            events: Mutex::default(),
            health: Mutex::new(HealthSnapshot::default()),
            pause: Mutex::new(PauseState::default()),
            last_state: Mutex::new(None),
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
    /// If a listener blocks anyway, memory stays bounded: emitters never
    /// wait, and the queue holds at most `EVENT_QUEUE_CAPACITY` (1024)
    /// events. When it is full, `Execution` events are dropped and counted
    /// (one `runtime.events_dropped` warning per backlog), while `Health` and
    /// `State` are coalesced to their latest value and delivered once the
    /// backlog drains. Every subscriber therefore eventually sees the latest
    /// Health and State, and never an older one after a newer one.
    pub fn subscribe(&self, listener: Box<dyn Fn(EngineEvent) + Send + Sync>) -> SubscriptionId {
        let id = self.next_listener.fetch_add(1, Ordering::Relaxed);
        self.listeners.write().push((id, Arc::from(listener)));
        SubscriptionId(id)
    }

    pub fn unsubscribe(&self, id: SubscriptionId) {
        self.listeners.write().retain(|(other, _)| *other != id.0);
    }

    /// Queues `event` for the event thread without blocking, starting the
    /// thread on first use (or after it exited).
    fn emit(&self, event: EngineEvent) {
        if self.listeners.read().is_empty() {
            return;
        }
        let mut queue = self.events.lock();
        let event = match queue.tx.as_ref() {
            Some(tx) => match offer(tx, &queue.overflow, event) {
                None => return,
                Some(event) => event,
            },
            None => event,
        };
        queue.tx = None;
        let (tx, rx) = mpsc::sync_channel::<EngineEvent>(EVENT_QUEUE_CAPACITY);
        let listeners = Arc::clone(&self.listeners);
        let overflow = Arc::clone(&queue.overflow);
        let spawned = std::thread::Builder::new()
            .name("qt-engine-events".into())
            .spawn(move || run_event_thread(&rx, &listeners, &overflow));
        match spawned {
            Ok(_) => {
                let _ = offer(&tx, &queue.overflow, event);
                queue.tx = Some(tx);
            }
            // Never deliver inline: callers emit while holding state locks
            // that a listener could re-enter. Listeners resynchronise from
            // `get_runtime_snapshot`; the next emit retries the spawn.
            Err(_) => tracing::warn!(
                event = "runtime.event_dropped",
                "event thread could not start; event dropped"
            ),
        }
    }

    // ---- health -------------------------------------------------------

    pub fn health(&self) -> HealthSnapshot {
        self.health.lock().clone()
    }

    /// Stores a new snapshot (from the supervisor). ENGINE_DOWN follows the
    /// state: set in Recovering/Failed, cleared otherwise. Emits `Health`
    /// when the snapshot changed and `State` when the engine state changed.
    ///
    /// The snapshot, the ENGINE_DOWN bit and both events are updated in one
    /// critical section (`last_state` → `health` → `pause`, see [`Runtime`]),
    /// so concurrent publishers cannot leave ENGINE_DOWN disagreeing with
    /// the stored health or enqueue their events out of order.
    pub fn publish_health(&self, snapshot: HealthSnapshot) {
        let engine_down = matches!(
            snapshot.state,
            HealthState::Recovering | HealthState::Failed
        );
        let mut last = self.last_state.lock();
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
            if *health != snapshot {
                *health = snapshot.clone();
                self.emit(EngineEvent::Health(snapshot));
            }
            (health.state, pause.reasons)
        };
        // `last_state` is still held: no other publish or refresh can slip
        // a State between this Health and this State.
        self.emit_state_if_changed(&mut last, self.state_from(health, reasons));
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

    /// Recomputes the engine state and emits `State` if it changed. The
    /// emit happens under `last_state` so listeners see states in order.
    pub fn refresh_state(&self) {
        let mut last = self.last_state.lock();
        let (health, reasons) = self.health_and_pause();
        self.emit_state_if_changed(&mut last, self.state_from(health, reasons));
    }

    fn emit_state_if_changed(&self, last: &mut Option<EngineState>, state: EngineState) {
        if last.as_ref() != Some(&state) {
            *last = Some(state.clone());
            self.emit(EngineEvent::State(state));
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
        self.emit(EngineEvent::Execution(entry.clone()));
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

/// Hands `event` to the event thread without blocking. A full queue drops
/// an `Execution` (counted) and parks a `Health`/`State` in `overflow`.
/// Returns the event back only if the event thread is gone.
///
/// Parking and the full check happen under the `overflow` lock, which the
/// event thread takes before it goes back to waiting on an empty queue. So
/// a value is only ever parked while the queue is full (or another value of
/// its kind is already parked), and the event thread always wakes up again
/// to deliver it.
fn offer(
    tx: &mpsc::SyncSender<EngineEvent>,
    overflow: &Mutex<Overflow>,
    event: EngineEvent,
) -> Option<EngineEvent> {
    let mut overflow = overflow.lock();
    let event = match event {
        EngineEvent::Health(health) if overflow.health.is_some() => {
            overflow.health = Some(health);
            return None;
        }
        EngineEvent::State(state) if overflow.state.is_some() => {
            overflow.state = Some(state);
            return None;
        }
        event => event,
    };
    match tx.try_send(event) {
        Ok(()) => None,
        Err(mpsc::TrySendError::Full(event)) => {
            match event {
                EngineEvent::Health(health) => overflow.health = Some(health),
                EngineEvent::State(state) => overflow.state = Some(state),
                EngineEvent::Execution(_) => overflow.dropped += 1,
            }
            None
        }
        Err(mpsc::TrySendError::Disconnected(event)) => Some(event),
    }
}

/// The event thread: delivers queued events in order; whenever the backlog
/// is drained, delivers what was coalesced meanwhile before waiting again.
fn run_event_thread(
    rx: &mpsc::Receiver<EngineEvent>,
    listeners: &RwLock<Vec<(u64, Listener)>>,
    overflow: &Mutex<Overflow>,
) {
    loop {
        let event = match rx.try_recv() {
            Ok(event) => event,
            Err(mpsc::TryRecvError::Empty) => {
                if flush_overflow(listeners, overflow) {
                    continue;
                }
                match rx.recv() {
                    Ok(event) => event,
                    Err(mpsc::RecvError) => return,
                }
            }
            Err(mpsc::TryRecvError::Disconnected) => return,
        };
        deliver(listeners, &event);
    }
}

/// Delivers the parked Health/State (latest values) and reports dropped
/// executions once. Returns whether anything was parked.
fn flush_overflow(listeners: &RwLock<Vec<(u64, Listener)>>, overflow: &Mutex<Overflow>) -> bool {
    let Overflow {
        health,
        state,
        dropped,
    } = std::mem::take(&mut *overflow.lock());
    if dropped > 0 {
        tracing::warn!(
            event = "runtime.events_dropped",
            dropped,
            "event queue was full; execution events dropped"
        );
    }
    let parked = [
        health.map(EngineEvent::Health),
        state.map(EngineEvent::State),
    ];
    let mut any = false;
    for event in parked.into_iter().flatten() {
        deliver(listeners, &event);
        any = true;
    }
    any
}

fn deliver(listeners: &RwLock<Vec<(u64, Listener)>>, event: &EngineEvent) {
    let targets: Vec<Listener> = listeners
        .read()
        .iter()
        .map(|(_, listener)| Arc::clone(listener))
        .collect();
    for listener in targets {
        if catch_unwind(AssertUnwindSafe(|| listener(event.clone()))).is_err() {
            tracing::warn!(
                event = "runtime.listener_panic",
                "event listener panicked; other listeners still run"
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
        runtime.publish_health(snapshot(HealthState::Healthy));
        runtime.publish_health(snapshot(HealthState::Healthy));
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
        wait_until("both entries", || events.lock().len() == 2);
        runtime.unsubscribe(bad);
        runtime.record(Record::new(
            "c",
            TriggerKind::Immediate,
            Outcome::Succeeded,
            ReasonCode::Ok,
        ));
        wait_until("third entry", || events.lock().len() == 3);
        let seqs: Vec<u64> = events
            .lock()
            .iter()
            .map(|event| match event {
                EngineEvent::Execution(entry) => entry.seq,
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(seqs, [1, 2, 3]);
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
        wait_until("execution event", || !events.lock().is_empty());
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(*events.lock(), vec![EngineEvent::Execution(entry)]);
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

    /// One listener blocks the event thread while events keep coming: the
    /// queue stays bounded, executions beyond it are dropped, and once the
    /// listener returns every subscriber sees the latest Health and State,
    /// with Health never going backwards.
    #[test]
    fn blocked_listener_keeps_queue_bounded_and_coalesces_health_state() {
        use std::sync::atomic::AtomicBool;
        let (runtime, _) = runtime();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = Mutex::new(release_rx);
        let blocked_once = AtomicBool::new(false);
        runtime.subscribe(Box::new(move |_| {
            if !blocked_once.swap(true, Ordering::SeqCst) {
                entered_tx.send(()).unwrap();
                release_rx.lock().recv().unwrap();
            }
        }));
        let events = collect(&runtime);
        let execution = |rule: &str| {
            Record::new(
                rule,
                TriggerKind::Immediate,
                Outcome::Succeeded,
                ReasonCode::Ok,
            )
        };
        runtime.record(execution("first"));
        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("event thread blocked in the listener");

        let executions = 3 * EVENT_QUEUE_CAPACITY;
        let states = [
            HealthState::Healthy,
            HealthState::Recovering,
            HealthState::Degraded,
        ];
        for i in 0..executions {
            runtime.record(execution("burst"));
            if i % 4 == 0 {
                runtime.publish_health(numbered(states[i % 3], i));
                runtime.set_pause(PauseReasons::MANUAL, i % 8 == 0);
            }
        }
        let final_health = numbered(HealthState::Degraded, executions + 1);
        runtime.publish_health(final_health.clone());
        runtime.set_pause(PauseReasons::TIMED, true);
        let final_state = runtime.engine_state();
        assert!(!final_state.listening);
        release_tx.send(()).unwrap();

        let last_of = |events: &[EngineEvent]| {
            let health = events.iter().rev().find_map(|event| match event {
                EngineEvent::Health(health) => Some(health.clone()),
                _ => None,
            });
            let state = events.iter().rev().find_map(|event| match event {
                EngineEvent::State(state) => Some(state.clone()),
                _ => None,
            });
            (health, state)
        };
        wait_until("latest health and state", || {
            last_of(&events.lock()) == (Some(final_health.clone()), Some(final_state.clone()))
        });
        std::thread::sleep(Duration::from_millis(50));
        let events = events.lock().clone();
        assert_eq!(
            last_of(&events),
            (Some(final_health), Some(final_state)),
            "nothing older arrives after the latest values"
        );
        // Bounded: one event in the blocked listener, at most a full queue,
        // plus the parked Health and State.
        assert!(
            events.len() <= 1 + EVENT_QUEUE_CAPACITY + 2,
            "{} events delivered",
            events.len()
        );
        let delivered_executions = events
            .iter()
            .filter(|event| matches!(event, EngineEvent::Execution(_)))
            .count();
        assert!(delivered_executions <= 1 + EVENT_QUEUE_CAPACITY);
        assert!(delivered_executions < 1 + executions, "some were dropped");
        let health_order: Vec<i64> = events
            .iter()
            .filter_map(|event| match event {
                EngineEvent::Health(health) => Some(health.components[0].since_ms),
                _ => None,
            })
            .collect();
        assert!(
            health_order.windows(2).all(|pair| pair[0] < pair[1]),
            "{health_order:?}"
        );
        let seqs: Vec<u64> = events
            .iter()
            .filter_map(|event| match event {
                EngineEvent::Execution(entry) => Some(entry.seq),
                _ => None,
            })
            .collect();
        assert!(seqs.windows(2).all(|pair| pair[0] < pair[1]));

        // The thread keeps working normally once the backlog is gone.
        let sink = collect(&runtime);
        let late = runtime.record(execution("late"));
        wait_until("late execution", || !sink.lock().is_empty());
        assert_eq!(*sink.lock(), vec![EngineEvent::Execution(late)]);
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
