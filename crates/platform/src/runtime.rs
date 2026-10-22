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

#[derive(Default)]
struct PauseState {
    reasons: PauseReasons,
    /// The value this runtime last wrote to `Shared::paused` (`None` until a
    /// `Shared` is attached).
    written: Option<bool>,
}

pub struct Runtime {
    clock: Arc<dyn Clock>,
    history: History,
    listeners: Listeners,
    next_listener: AtomicU64,
    events: Mutex<Option<mpsc::Sender<EngineEvent>>>,
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
            events: Mutex::new(None),
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

    pub fn subscribe(&self, listener: Box<dyn Fn(EngineEvent) + Send + Sync>) -> SubscriptionId {
        let id = self.next_listener.fetch_add(1, Ordering::Relaxed);
        self.listeners.write().push((id, Arc::from(listener)));
        SubscriptionId(id)
    }

    pub fn unsubscribe(&self, id: SubscriptionId) {
        self.listeners.write().retain(|(other, _)| *other != id.0);
    }

    fn emit(&self, event: EngineEvent) {
        if self.listeners.read().is_empty() {
            return;
        }
        let mut sender = self.events.lock();
        let event = match sender.as_ref() {
            Some(tx) => match tx.send(event) {
                Ok(()) => return,
                Err(mpsc::SendError(event)) => event,
            },
            None => event,
        };
        *sender = None;
        let (tx, rx) = mpsc::channel::<EngineEvent>();
        let listeners = Arc::clone(&self.listeners);
        let spawned = std::thread::Builder::new()
            .name("qt-engine-events".into())
            .spawn(move || {
                for event in rx {
                    deliver(&listeners, &event);
                }
            });
        match spawned {
            Ok(_) => {
                let _ = tx.send(event);
                *sender = Some(tx);
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
    pub fn publish_health(&self, snapshot: HealthSnapshot) {
        let engine_down = matches!(
            snapshot.state,
            HealthState::Recovering | HealthState::Failed
        );
        {
            // Emit under the lock so concurrent publishers cannot reorder.
            let mut health = self.health.lock();
            if *health != snapshot {
                *health = snapshot.clone();
                self.emit(EngineEvent::Health(snapshot));
            }
        }
        self.set_engine_down(engine_down);
        self.refresh_state();
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

    pub(crate) fn set_engine_down(&self, on: bool) {
        self.update_pause(|reasons| {
            if on {
                reasons.with(PauseReasons::ENGINE_DOWN)
            } else {
                reasons.without(PauseReasons::ENGINE_DOWN)
            }
        });
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
        let paused_reasons = self.pause_reasons();
        let health = self.health.lock().state;
        let busy = self
            .shared
            .load_full()
            .is_some_and(|shared| shared.busy.load(Ordering::SeqCst));
        let probe = self.process.read().clone();
        EngineState {
            listening: health.accepts_resume() && paused_reasons.is_empty(),
            paused_reasons,
            busy,
            current_process: probe.and_then(|probe| probe()),
        }
    }

    /// Recomputes the engine state and emits `State` if it changed. The
    /// emit happens under the lock so listeners see states in order.
    pub fn refresh_state(&self) {
        let mut last = self.last_state.lock();
        let state = self.engine_state();
        if last.as_ref() != Some(&state) {
            *last = Some(state.clone());
            self.emit(EngineEvent::State(state));
        }
    }

    /// The §7.2 mapping-table row for the current health, pause and busy.
    pub fn status_text(&self) -> StatusText {
        let state = self.engine_state();
        StatusText::resolve(self.health.lock().state, state.paused_reasons, state.busy)
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

/// Registers an event listener (P5 connects the Tauri EventBus here).
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
