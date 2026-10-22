//! Production wiring of the supervisor (B-03, P5): builds it on this
//! platform's adapters, installs it as the process-wide instance, starts its
//! driver and a watchdog thread, and stops both when the engine shuts down.
//!
//! Until this runs nothing publishes Health: it stays Starting,
//! `runtime::accepts_resume()` is false and the engine cannot be resumed.
//!
//! Everything except [`platform_adapters`] is platform-neutral and tested
//! with fake adapters and a fake clock.
//!
//! Shutdown (`RunEvent::Exit`, session end) is [`begin_shutdown`]: it sets
//! `Shared::shutdown`, which `Runtime::shutting_down` reads. The driver and
//! the watchdog stop at their next turn, and until then the adapters are
//! frozen (the guards below): probes repeat the last value read before the
//! shutdown, rebuilds and reattaches do nothing, and one that was already
//! running when the shutdown began reports success whatever it returned.
//! The engine's own teardown (the Windows input consumer closes the RawInput
//! window once it sees `shutdown`) is therefore never read as a failure: no
//! incident, no failed attempt, no Failed during exit.
use crate::health::{Component, ComponentHealth, HealthSnapshot, HealthState, ReasonCode};
use crate::runtime::Runtime;
use crate::supervisor::{self, ExecutorControl, InputSources, Status, Supervisor};
use crate::traits::Clock;
use arc_swap::ArcSwapOption;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How often the watchdog thread checks the supervisor driver.
pub const WATCHDOG_INTERVAL: Duration = Duration::from_secs(5);

/// The Windows input components, in `input_health` order.
pub const INPUT_COMPONENTS: [Component; 4] = [
    Component::KeyboardHook,
    Component::MouseHook,
    Component::FocusEvents,
    Component::RawInput,
];

/// Non-blocking read of the input health: `None` while it is busy.
pub type HealthProbe = Box<dyn Fn() -> Option<Vec<ComponentHealth>> + Send + Sync>;
/// Re-registers one input component.
pub type Reattach = Box<dyn Fn(Component) -> Result<(), String> + Send + Sync>;
/// Whether a self-managed input backend is ready (`native_ready`).
pub type ReadyProbe = Box<dyn Fn() -> Result<(), String> + Send + Sync>;

/// Builds the supervisor on `executor` and `inputs`, makes it the
/// process-wide instance ([`supervisor::install`], used by `repair_engine`
/// and `check_supervisor`), then starts its driver (`qt-supervisor`) and the
/// `qt-watchdog` thread, which checks the driver every [`WATCHDOG_INTERVAL`].
/// Both are threads of their own: the caller (the Tauri main thread) never
/// blocks on them. A thread that cannot be spawned is logged (see [`run`]).
pub fn install_supervisor(
    clock: Arc<dyn Clock>,
    runtime: Arc<Runtime>,
    executor: Arc<dyn ExecutorControl>,
    inputs: Arc<dyn InputSources>,
) -> Arc<Supervisor> {
    let supervisor = build(clock, Arc::clone(&runtime), executor, inputs);
    supervisor::install(Arc::clone(&supervisor));
    match run(&supervisor, runtime, WATCHDOG_INTERVAL) {
        Ok(_watchdog) => tracing::info!(event = "supervisor.installed"),
        Err(error) => tracing::error!(
            event = "supervisor.watchdog_spawn_failed",
            %error,
            "supervisor runs without its watchdog"
        ),
    }
    supervisor
}

/// The supervisor on `executor` and `inputs` wrapped in the shutdown guards;
/// not installed and not started.
pub fn build(
    clock: Arc<dyn Clock>,
    runtime: Arc<Runtime>,
    executor: Arc<dyn ExecutorControl>,
    inputs: Arc<dyn InputSources>,
) -> Arc<Supervisor> {
    let executor = Arc::new(GuardedExecutor {
        runtime: Arc::clone(&runtime),
        inner: executor,
        last: ArcSwapOption::empty(),
    });
    let inputs = Arc::new(GuardedInputs {
        runtime: Arc::clone(&runtime),
        inner: inputs,
        last: ArcSwapOption::empty(),
    });
    Supervisor::new(clock, runtime, executor, inputs)
}

/// Starts the driver and the watchdog (checking every `interval`); returns
/// the watchdog's handle. Both end once `runtime` is shutting down.
///
/// A driver that could not be spawned is spawned again by the watchdog's
/// first check. Only when neither thread starts would nothing ever publish:
/// Health is then reported Failed (supervisor `internal`) rather than left
/// Starting, and the error returned.
pub fn run(
    supervisor: &Arc<Supervisor>,
    runtime: Arc<Runtime>,
    interval: Duration,
) -> std::io::Result<JoinHandle<()>> {
    let driver = supervisor.spawn_driver();
    if let Err(error) = &driver {
        tracing::error!(
            event = "supervisor.driver_spawn_failed",
            %error,
            "the watchdog retries"
        );
    }
    let watchdog = spawn_watchdog(supervisor, Arc::clone(&runtime), interval);
    if let (Err(_), Err(error)) = (&driver, &watchdog) {
        tracing::error!(
            event = "supervisor.start_failed",
            %error,
            "no supervisor thread could start; engine failed"
        );
        runtime.publish_health(unsupervised(runtime.clock().wall_ms()));
    }
    watchdog
}

/// Health when no supervisor runs at all.
fn unsupervised(wall_ms: i64) -> HealthSnapshot {
    HealthSnapshot {
        state: HealthState::Failed,
        components: vec![ComponentHealth {
            component: Component::Supervisor,
            state: HealthState::Failed,
            reason: Some(ReasonCode::Internal),
            since_ms: wall_ms,
        }],
    }
}

fn spawn_watchdog(
    supervisor: &Arc<Supervisor>,
    runtime: Arc<Runtime>,
    interval: Duration,
) -> std::io::Result<JoinHandle<()>> {
    let supervisor = Arc::clone(supervisor);
    std::thread::Builder::new()
        .name("qt-watchdog".into())
        .spawn(move || {
            while !runtime.shutting_down() {
                std::thread::sleep(interval);
                if runtime.shutting_down() {
                    break;
                }
                // The installed instance: the same as `check_supervisor()`.
                supervisor.check();
            }
            tracing::info!(
                event = "supervisor.watchdog_stopped",
                "engine shutting down"
            );
        })
}

/// Starts the engine shutdown (`RunEvent::Exit`, session end): sets
/// `Shared::shutdown` of the engine attached to `runtime`, which stops the
/// supervisor driver, the watchdog and the engine's own threads. Idempotent
/// and non-blocking; false when no engine is attached.
pub fn begin_shutdown(runtime: &Runtime) -> bool {
    let Some(shared) = runtime.shared() else {
        return false;
    };
    if !shared.shutdown.swap(true, Ordering::SeqCst) {
        tracing::info!(event = "engine.shutdown", "engine shutting down");
    }
    true
}

/// The executor as the supervisor sees it while the engine shuts down:
/// probes repeat the last value read before, rebuilds do nothing, and a
/// rebuild that returns after the shutdown began is discarded (`Ok`), so no
/// failure is committed after it. The budget fence keeps running.
struct GuardedExecutor {
    runtime: Arc<Runtime>,
    inner: Arc<dyn ExecutorControl>,
    last: ArcSwapOption<Vec<(Component, Status)>>,
}

impl ExecutorControl for GuardedExecutor {
    fn enforce_budget(&self, now: Instant) {
        self.inner.enforce_budget(now);
    }
    fn probe(&self, now: Instant) -> Vec<(Component, Status)> {
        if self.runtime.shutting_down() {
            if let Some(last) = self.last.load_full() {
                return last.as_ref().clone();
            }
        }
        let status = self.inner.probe(now);
        self.last.store(Some(Arc::new(status.clone())));
        status
    }
    fn rebuild(&self) -> Result<(), String> {
        if self.runtime.shutting_down() {
            tracing::info!(event = "supervisor.rebuild_skipped", "engine shutting down");
            return Ok(());
        }
        let result = self.inner.rebuild();
        if self.runtime.shutting_down() {
            // Started before the shutdown, returned after it: the teardown
            // may be what made it fail, so its result is not counted.
            tracing::info!(
                event = "supervisor.rebuild_discarded",
                failed = result.is_err(),
                "engine shut down while the rebuild ran"
            );
            return Ok(());
        }
        result
    }
}

/// The input sources as the supervisor sees them while the engine shuts
/// down: health repeats the last value read before, reattaches do nothing,
/// and a reattach that returns after the shutdown began is discarded (`Ok`).
struct GuardedInputs {
    runtime: Arc<Runtime>,
    inner: Arc<dyn InputSources>,
    last: ArcSwapOption<Vec<ComponentHealth>>,
}

impl InputSources for GuardedInputs {
    fn input_health(&self) -> Vec<ComponentHealth> {
        if self.runtime.shutting_down() {
            if let Some(last) = self.last.load_full() {
                return last.as_ref().clone();
            }
        }
        let health = self.inner.input_health();
        self.last.store(Some(Arc::new(health.clone())));
        health
    }
    fn reattach(&self, component: Component) -> Result<(), String> {
        if self.runtime.shutting_down() {
            tracing::info!(
                event = "supervisor.reattach_skipped",
                component = ?component,
                "engine shutting down"
            );
            return Ok(());
        }
        let result = self.inner.reattach(component);
        if self.runtime.shutting_down() {
            tracing::info!(
                event = "supervisor.reattach_discarded",
                component = ?component,
                failed = result.is_err(),
                "engine shut down while the reattach ran"
            );
            return Ok(());
        }
        result
    }
}

/// An executor that did not start (or a platform without one): always down
/// with `reason`, and every rebuild fails. Starting therefore ends in Failed
/// once STARTUP_GRACE and the recovery attempts have run out (10 s + 1 + 2 +
/// 5 + 15 s ≈ 33 s), with `reason` on the executor component, instead of
/// staying Starting.
pub struct UnavailableExecutor {
    reason: ReasonCode,
    error: String,
}

impl UnavailableExecutor {
    pub fn new(reason: ReasonCode, error: impl Into<String>) -> Self {
        Self {
            reason,
            error: error.into(),
        }
    }
}

impl ExecutorControl for UnavailableExecutor {
    fn enforce_budget(&self, _now: Instant) {}
    fn probe(&self, _now: Instant) -> Vec<(Component, Status)> {
        vec![(Component::Executor, Status::Down(self.reason))]
    }
    fn rebuild(&self) -> Result<(), String> {
        Err(self.error.clone())
    }
}

/// An executor that needs no supervision (macOS runs its own worker and has
/// no budget fence): always healthy; a rebuild does nothing.
pub struct NullExecutor;

impl ExecutorControl for NullExecutor {
    fn enforce_budget(&self, _now: Instant) {}
    fn probe(&self, _now: Instant) -> Vec<(Component, Status)> {
        vec![(Component::Executor, Status::Ok)]
    }
    fn rebuild(&self) -> Result<(), String> {
        Ok(())
    }
}

/// The started engine's executor, or an [`UnavailableExecutor`]
/// (`engine_down`) when the engine did not start or has no executor.
pub fn executor_for(
    started: &Result<(), String>,
    executor: Option<Arc<dyn ExecutorControl>>,
) -> Arc<dyn ExecutorControl> {
    let error = match (started, executor) {
        (Ok(()), Some(executor)) => return executor,
        (Ok(()), None) => "the engine started without an executor".to_string(),
        (Err(error), _) => format!("the engine did not start: {error}"),
    };
    tracing::error!(
        event = "supervisor.engine_unavailable",
        error = %error,
        "engine reported down; Health ends in Failed"
    );
    Arc::new(UnavailableExecutor::new(ReasonCode::EngineDown, error))
}

/// Input sources whose health is read without waiting (Windows:
/// `try_input_health`). While the read finds the health table busy the
/// last value is returned again; before the first successful read every
/// component is reported Starting, which the supervisor counts as down, so
/// a busy table can never make the engine look healthy.
pub struct CachedInputs {
    try_health: HealthProbe,
    reattach: Reattach,
    unknown: Vec<Component>,
    last: ArcSwapOption<Vec<ComponentHealth>>,
}

impl CachedInputs {
    pub fn new(try_health: HealthProbe, reattach: Reattach, unknown: Vec<Component>) -> Self {
        Self {
            try_health,
            reattach,
            unknown,
            last: ArcSwapOption::empty(),
        }
    }
}

impl InputSources for CachedInputs {
    fn input_health(&self) -> Vec<ComponentHealth> {
        if let Some(health) = (self.try_health)() {
            self.last.store(Some(Arc::new(health.clone())));
            return health;
        }
        match self.last.load_full() {
            Some(last) => last.as_ref().clone(),
            // `since_ms` is not used: the supervisor stamps its own.
            None => self
                .unknown
                .iter()
                .map(|&component| ComponentHealth {
                    component,
                    state: HealthState::Starting,
                    reason: None,
                    since_ms: 0,
                })
                .collect(),
        }
    }
    fn reattach(&self, component: Component) -> Result<(), String> {
        (self.reattach)(component)
    }
}

/// An input backend that installs and reinstalls its own tap and only
/// reports whether it is ready (macOS `native_ready`, the unsupported
/// stub): the keyboard hook is healthy while `ready` is Ok and down with
/// `reason` otherwise. A reattach only checks again.
pub struct ReadinessInputs {
    ready: ReadyProbe,
    reason: ReasonCode,
}

impl ReadinessInputs {
    pub fn new(ready: ReadyProbe, reason: ReasonCode) -> Self {
        Self { ready, reason }
    }
}

impl InputSources for ReadinessInputs {
    fn input_health(&self) -> Vec<ComponentHealth> {
        let (state, reason) = match (self.ready)() {
            Ok(()) => (HealthState::Healthy, None),
            Err(_) => (HealthState::Recovering, Some(self.reason)),
        };
        vec![ComponentHealth {
            component: Component::KeyboardHook,
            state,
            reason,
            since_ms: 0,
        }]
    }
    fn reattach(&self, _component: Component) -> Result<(), String> {
        (self.ready)()
    }
}

/// This platform's adapters, given what `start_engine` returned.
///
/// Windows: the executor service (`executor_control()`, which exists only
/// after `start_engine`) and the four input sources read with
/// `try_input_health`. A failed start or a missing executor becomes an
/// [`UnavailableExecutor`].
#[cfg(windows)]
pub fn platform_adapters(
    started: &Result<(), String>,
) -> (Arc<dyn ExecutorControl>, Arc<dyn InputSources>) {
    let executor = executor_for(started, crate::executor_control());
    let inputs = CachedInputs::new(
        Box::new(crate::try_input_health),
        Box::new(crate::reattach),
        INPUT_COMPONENTS.to_vec(),
    );
    (executor, Arc::new(inputs))
}

/// macOS (experimental): no executor supervision ([`NullExecutor`]) and the
/// keyboard hook healthy while `native_ready()` is Ok, `hook_missing`
/// otherwise, so Health can reach Healthy and listening can resume.
/// 待实测: `native_ready()` also fails for missing permissions, Secure Input
/// and IME layouts; while it fails Health stays Starting or goes
/// Recovering, and after the recovery attempts Failed (repair needed).
#[cfg(target_os = "macos")]
pub fn platform_adapters(
    started: &Result<(), String>,
) -> (Arc<dyn ExecutorControl>, Arc<dyn InputSources>) {
    let executor = executor_for(started, Some(Arc::new(NullExecutor)));
    let inputs = ReadinessInputs::new(Box::new(crate::native_ready), ReasonCode::HookMissing);
    (executor, Arc::new(inputs))
}

/// Other platforms have no input backend: everything is `unsupported`, and
/// Health ends in Failed after the startup attempts.
#[cfg(not(any(windows, target_os = "macos")))]
pub fn platform_adapters(
    started: &Result<(), String>,
) -> (Arc<dyn ExecutorControl>, Arc<dyn InputSources>) {
    let error = started
        .clone()
        .err()
        .unwrap_or_else(|| "no input backend on this platform".into());
    let executor = UnavailableExecutor::new(ReasonCode::Unsupported, error);
    let inputs = ReadinessInputs::new(Box::new(crate::native_ready), ReasonCode::Unsupported);
    (Arc::new(executor), Arc::new(inputs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::PauseReasons;
    use crate::mock::FakeClock;
    use crate::supervisor::{BACKOFF, CIRCUIT_FAILURES, STARTUP_GRACE, TICK};
    use crate::Shared;
    use parking_lot::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::sync::mpsc;

    struct Executor {
        status: Mutex<Status>,
        budget_checks: AtomicUsize,
        rebuilds: AtomicUsize,
    }

    impl Executor {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                status: Mutex::new(Status::Ok),
                budget_checks: AtomicUsize::new(0),
                rebuilds: AtomicUsize::new(0),
            })
        }
    }

    impl ExecutorControl for Executor {
        fn enforce_budget(&self, _now: Instant) {
            self.budget_checks.fetch_add(1, Ordering::SeqCst);
        }
        fn probe(&self, _now: Instant) -> Vec<(Component, Status)> {
            vec![(Component::Executor, *self.status.lock())]
        }
        fn rebuild(&self) -> Result<(), String> {
            self.rebuilds.fetch_add(1, Ordering::SeqCst);
            Err("rebuild refused".into())
        }
    }

    struct Inputs {
        /// The keyboard hook is down with this reason.
        down: Mutex<Option<ReasonCode>>,
        reattaches: AtomicUsize,
    }

    impl Inputs {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                down: Mutex::new(None),
                reattaches: AtomicUsize::new(0),
            })
        }
    }

    impl InputSources for Inputs {
        fn input_health(&self) -> Vec<ComponentHealth> {
            let down = *self.down.lock();
            INPUT_COMPONENTS
                .iter()
                .map(|&component| {
                    let reason = down.filter(|_| component == Component::KeyboardHook);
                    ComponentHealth {
                        component,
                        state: if reason.is_some() {
                            HealthState::Recovering
                        } else {
                            HealthState::Healthy
                        },
                        reason,
                        since_ms: 0,
                    }
                })
                .collect()
        }
        fn reattach(&self, _component: Component) -> Result<(), String> {
            self.reattaches.fetch_add(1, Ordering::SeqCst);
            Err("reattach refused".into())
        }
    }

    struct Rig {
        clock: Arc<FakeClock>,
        runtime: Arc<Runtime>,
        shared: Arc<Shared>,
    }

    impl Rig {
        /// A runtime with an attached engine, which starts paused (MANUAL).
        fn new() -> Self {
            let clock = Arc::new(FakeClock::new());
            let runtime = Arc::new(Runtime::new(clock.clone()));
            let shared = Shared::new();
            runtime.attach(&shared);
            Self {
                clock,
                runtime,
                shared,
            }
        }
        fn build(
            &self,
            executor: Arc<dyn ExecutorControl>,
            inputs: Arc<dyn InputSources>,
        ) -> Arc<Supervisor> {
            build(self.clock.clone(), self.runtime.clone(), executor, inputs)
        }
        fn state(&self) -> HealthState {
            self.runtime.health().state
        }
        fn component(&self, component: Component) -> ComponentHealth {
            self.runtime
                .health()
                .components
                .into_iter()
                .find(|c| c.component == component)
                .unwrap()
        }
    }

    fn wait_until(what: &str, done: impl Fn() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !done() {
            assert!(std::time::Instant::now() < deadline, "timed out: {what}");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Longest Starting may last when the engine never comes up: the grace
    /// period, then attempts after BACKOFF[1..] (the 5th failure opens the
    /// circuit), plus one tick.
    fn startup_failure_bound() -> Duration {
        STARTUP_GRACE + BACKOFF[1..].iter().sum::<Duration>() + TICK
    }

    /// Ticks in 100 ms steps until Failed; returns how long it took.
    fn tick_until_failed(rig: &Rig, supervisor: &Supervisor) -> Duration {
        let step = Duration::from_millis(100);
        let mut elapsed = Duration::ZERO;
        while rig.state() != HealthState::Failed {
            assert!(
                elapsed <= startup_failure_bound(),
                "still {:?} after {elapsed:?}",
                rig.state()
            );
            supervisor.tick();
            rig.clock.advance(step);
            elapsed += step;
        }
        elapsed
    }

    /// B-03: once installed, the driver publishes Healthy, `accepts_resume`
    /// (the `set_paused(false)` criterion) holds, and clearing the manual
    /// pause starts listening.
    #[test]
    fn the_running_supervisor_reaches_healthy_and_resume_is_allowed() {
        let rig = Rig::new();
        let supervisor = rig.build(Executor::new(), Inputs::new());
        assert_eq!(rig.state(), HealthState::Starting);
        assert!(!rig.state().accepts_resume());

        let watchdog = run(&supervisor, rig.runtime.clone(), Duration::from_millis(5)).unwrap();
        wait_until("Healthy published", || rig.state() == HealthState::Healthy);
        assert!(rig.state().accepts_resume());
        assert!(!rig.runtime.engine_state().listening, "MANUAL still set");
        rig.runtime.set_pause(PauseReasons::MANUAL, false);
        assert!(rig.runtime.engine_state().listening);
        assert!(!rig.shared.paused.load(Ordering::SeqCst));
        // The watchdog finds a live driver and leaves it alone.
        std::thread::sleep(Duration::from_millis(30));
        assert!(supervisor.check());

        assert!(begin_shutdown(&rig.runtime));
        wait_until("watchdog stopped", || watchdog.is_finished());
        watchdog.join().unwrap();
    }

    /// A failed `start_engine`, or no executor after it, never leaves
    /// Health in Starting: it ends in Failed with `engine_down` on the
    /// executor within the startup bound.
    #[test]
    fn an_engine_that_did_not_start_ends_in_failed_not_starting() {
        let failed: Result<(), String> = Err("hooks unavailable".into());
        let started: Arc<dyn ExecutorControl> = Executor::new();
        let cases: [(&str, Arc<dyn ExecutorControl>); 2] = [
            ("no executor", executor_for(&Ok(()), None)),
            ("start failed", executor_for(&failed, Some(started))),
        ];
        for (case, executor) in cases {
            let rig = Rig::new();
            let supervisor = rig.build(executor, Inputs::new());
            let elapsed = tick_until_failed(&rig, &supervisor);
            assert!(elapsed >= STARTUP_GRACE, "{case}: grace period first");
            let component = rig.component(Component::Executor);
            assert_eq!(component.state, HealthState::Failed, "{case}");
            assert_eq!(component.reason, Some(ReasonCode::EngineDown), "{case}");
            assert!(rig
                .runtime
                .pause_reasons()
                .contains(PauseReasons::ENGINE_DOWN));
            assert!(!rig.runtime.engine_state().listening);
        }
    }

    #[test]
    fn a_started_engine_keeps_its_executor() {
        let executor: Arc<dyn ExecutorControl> = Executor::new();
        let chosen = executor_for(&Ok(()), Some(Arc::clone(&executor)));
        assert!(Arc::ptr_eq(&chosen, &executor));
        let unavailable = UnavailableExecutor::new(ReasonCode::Unsupported, "none");
        assert_eq!(
            unavailable.probe(Instant::now()),
            [(Component::Executor, Status::Down(ReasonCode::Unsupported))]
        );
        assert_eq!(unavailable.rebuild(), Err("none".into()));
    }

    /// After `begin_shutdown` the watchdog ends at its next turn and the
    /// driver after its current wait (at most one TICK): no more ticks,
    /// and the watchdog check never respawns it.
    #[test]
    fn the_driver_and_the_watchdog_stop_when_the_engine_shuts_down() {
        let rig = Rig::new();
        let executor = Executor::new();
        let supervisor = rig.build(executor.clone(), Inputs::new());
        let watchdog = run(&supervisor, rig.runtime.clone(), Duration::from_millis(5)).unwrap();
        wait_until("driver ticking", || {
            executor.budget_checks.load(Ordering::SeqCst) > 0
        });

        assert!(begin_shutdown(&rig.runtime));
        assert!(rig.runtime.shutting_down());
        assert!(begin_shutdown(&rig.runtime), "idempotent");
        wait_until("watchdog stopped", || watchdog.is_finished());
        watchdog.join().unwrap();

        let margin = Duration::from_millis(300);
        std::thread::sleep(TICK + margin);
        let ticks = executor.budget_checks.load(Ordering::SeqCst);
        assert!(supervisor.check(), "no respawn while shutting down");
        std::thread::sleep(TICK + margin);
        assert_eq!(
            executor.budget_checks.load(Ordering::SeqCst),
            ticks,
            "the driver ended"
        );
        assert_eq!(rig.state(), HealthState::Healthy);
    }

    #[test]
    fn shutdown_without_an_engine_does_nothing() {
        let runtime = Runtime::new(Arc::new(FakeClock::new()));
        assert!(!begin_shutdown(&runtime));
        assert!(!runtime.shutting_down());
    }

    /// The engine's teardown makes its components look down. While
    /// shutting down the supervisor keeps seeing the values from before:
    /// no incident, no rebuild or reattach, never Failed, not even for a
    /// repair requested meanwhile.
    #[test]
    fn shutdown_freezes_the_adapters() {
        let rig = Rig::new();
        let executor = Executor::new();
        let inputs = Inputs::new();
        let supervisor = rig.build(executor.clone(), inputs.clone());
        supervisor.tick();
        assert_eq!(rig.state(), HealthState::Healthy);

        begin_shutdown(&rig.runtime);
        *inputs.down.lock() = Some(ReasonCode::RawInputMissing);
        *executor.status.lock() = Status::Down(ReasonCode::ExecutorPanic);
        for _ in 0..600 {
            rig.clock.advance(Duration::from_millis(100));
            supervisor.tick();
            assert_eq!(rig.state(), HealthState::Healthy);
        }
        assert!(!rig
            .runtime
            .pause_reasons()
            .contains(PauseReasons::ENGINE_DOWN));

        supervisor.repair();
        for _ in 0..100 {
            rig.clock.advance(Duration::from_millis(100));
            supervisor.tick();
            assert_ne!(rig.state(), HealthState::Failed);
        }
        assert_eq!(rig.state(), HealthState::Healthy);
        assert_eq!(executor.rebuilds.load(Ordering::SeqCst), 0);
        assert_eq!(inputs.reattaches.load(Ordering::SeqCst), 0);
    }

    /// Holds the `at`-th call until the test releases it (bounded waits on
    /// both sides, so a failing test cannot hang).
    struct Gate {
        calls: AtomicUsize,
        at: usize,
        entered: (Mutex<mpsc::Sender<()>>, Mutex<mpsc::Receiver<()>>),
        release: (Mutex<mpsc::Sender<()>>, Mutex<mpsc::Receiver<()>>),
    }

    impl Gate {
        fn new(at: usize) -> Self {
            let (entered_tx, entered_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel();
            Self {
                calls: AtomicUsize::new(0),
                at,
                entered: (Mutex::new(entered_tx), Mutex::new(entered_rx)),
                release: (Mutex::new(release_tx), Mutex::new(release_rx)),
            }
        }
        fn pass(&self) {
            if self.calls.fetch_add(1, Ordering::SeqCst) + 1 == self.at {
                let _ = self.entered.0.lock().send(());
                let _ = self.release.1.lock().recv_timeout(Duration::from_secs(10));
            }
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
        fn wait_entered(&self) -> bool {
            self.entered
                .1
                .lock()
                .recv_timeout(Duration::from_secs(5))
                .is_ok()
        }
        fn open(&self) {
            let _ = self.release.0.lock().send(());
        }
    }

    /// Every rebuild fails, the gated one once it is released.
    struct GatedExecutor {
        status: Mutex<Status>,
        gate: Gate,
    }

    impl ExecutorControl for GatedExecutor {
        fn enforce_budget(&self, _now: Instant) {}
        fn probe(&self, _now: Instant) -> Vec<(Component, Status)> {
            vec![(Component::Executor, *self.status.lock())]
        }
        fn rebuild(&self) -> Result<(), String> {
            self.gate.pass();
            Err("rebuild refused".into())
        }
    }

    /// Every reattach fails, the gated one once it is released.
    struct GatedInputs {
        inputs: Arc<Inputs>,
        gate: Gate,
    }

    impl InputSources for GatedInputs {
        fn input_health(&self) -> Vec<ComponentHealth> {
            self.inputs.input_health()
        }
        fn reattach(&self, _component: Component) -> Result<(), String> {
            self.gate.pass();
            Err("reattach refused".into())
        }
    }

    /// A rebuild or reattach that was running when the shutdown began and
    /// fails after it is discarded: the failure that would open the circuit
    /// (the CIRCUIT_FAILURES-th) is not committed, so Health is not Failed
    /// during exit. Without the shutdown the same sequence ends in Failed.
    /// Only `repair()` leaves Failed, so not Failed at the end means it never
    /// was.
    #[test]
    fn an_attempt_that_fails_after_the_shutdown_never_commits_failed() {
        for adapter in ["executor", "inputs"] {
            for shutdown in [false, true] {
                let case = format!("{adapter}, shutdown {shutdown}");
                let rig = Rig::new();
                let executor = Arc::new(GatedExecutor {
                    status: Mutex::new(Status::Ok),
                    gate: Gate::new(CIRCUIT_FAILURES),
                });
                let inputs = Arc::new(GatedInputs {
                    inputs: Inputs::new(),
                    gate: Gate::new(CIRCUIT_FAILURES),
                });
                let supervisor = rig.build(executor.clone(), inputs.clone());
                supervisor.tick();
                assert_eq!(rig.state(), HealthState::Healthy, "{case}");
                let gate = if adapter == "executor" {
                    *executor.status.lock() = Status::Down(ReasonCode::ExecutorPanic);
                    &executor.gate
                } else {
                    *inputs.inputs.down.lock() = Some(ReasonCode::HookMissing);
                    &inputs.gate
                };

                let during = std::thread::scope(|scope| {
                    // Attempts 1..4 fail at once; the 5th blocks in the gate.
                    let ticker = scope.spawn(|| {
                        for _ in 0..1200 {
                            if gate.calls() >= CIRCUIT_FAILURES {
                                break;
                            }
                            rig.clock.advance(Duration::from_millis(100));
                            supervisor.tick();
                        }
                    });
                    let entered = gate.wait_entered();
                    let during = rig.state();
                    if shutdown {
                        begin_shutdown(&rig.runtime);
                    }
                    gate.open();
                    ticker.join().unwrap();
                    assert!(entered, "{case}: the gated attempt ran");
                    during
                });
                assert_eq!(during, HealthState::Recovering, "{case}");
                assert_eq!(gate.calls(), CIRCUIT_FAILURES, "{case}");
                if shutdown {
                    assert_ne!(rig.state(), HealthState::Failed, "{case}");
                } else {
                    assert_eq!(rig.state(), HealthState::Failed, "{case}");
                }
            }
        }
    }

    /// The Windows read: a busy table repeats the last value, and before
    /// the first read every component is Starting (down), never healthy.
    #[test]
    fn cached_inputs_reuse_the_last_value_while_the_table_is_busy() {
        let busy = Arc::new(AtomicBool::new(true));
        let reattached = Arc::new(Mutex::new(vec![]));
        let healthy = Inputs::new().input_health();
        let inputs = CachedInputs::new(
            Box::new({
                let busy = Arc::clone(&busy);
                let healthy = healthy.clone();
                move || (!busy.load(Ordering::SeqCst)).then(|| healthy.clone())
            }),
            Box::new({
                let reattached = Arc::clone(&reattached);
                move |component| {
                    reattached.lock().push(component);
                    Ok(())
                }
            }),
            INPUT_COMPONENTS.to_vec(),
        );

        let unknown = inputs.input_health();
        assert_eq!(
            unknown.iter().map(|h| h.component).collect::<Vec<_>>(),
            INPUT_COMPONENTS
        );
        assert!(unknown
            .iter()
            .all(|h| matches!(Status::from_health(h), Status::Down(_))));

        busy.store(false, Ordering::SeqCst);
        assert_eq!(inputs.input_health(), healthy);
        busy.store(true, Ordering::SeqCst);
        assert_eq!(inputs.input_health(), healthy, "last value");

        inputs.reattach(Component::RawInput).unwrap();
        assert_eq!(*reattached.lock(), [Component::RawInput]);
    }

    /// macOS: the keyboard hook follows `native_ready`, so Health can
    /// reach Healthy; while it fails the hook is down with `hook_missing`.
    #[test]
    fn readiness_inputs_follow_the_probe() {
        let ready = Arc::new(AtomicBool::new(false));
        let inputs = Arc::new(ReadinessInputs::new(
            Box::new({
                let ready = Arc::clone(&ready);
                move || {
                    if ready.load(Ordering::SeqCst) {
                        Ok(())
                    } else {
                        Err("not ready".into())
                    }
                }
            }),
            ReasonCode::HookMissing,
        ));
        let rig = Rig::new();
        let supervisor = rig.build(Arc::new(NullExecutor), inputs.clone());
        supervisor.tick();
        assert_eq!(rig.state(), HealthState::Starting);
        assert_eq!(
            rig.component(Component::KeyboardHook).reason,
            Some(ReasonCode::HookMissing)
        );
        assert_eq!(
            inputs.reattach(Component::KeyboardHook),
            Err("not ready".into())
        );

        ready.store(true, Ordering::SeqCst);
        rig.clock.advance(TICK);
        supervisor.tick();
        assert_eq!(rig.state(), HealthState::Healthy);
        assert!(rig.state().accepts_resume());
        assert_eq!(inputs.reattach(Component::KeyboardHook), Ok(()));
    }
}
