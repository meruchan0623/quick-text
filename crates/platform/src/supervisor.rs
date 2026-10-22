//! Supervisor (B-03): drives the Health state machine of development plan
//! §7.2 from a 1 s tick.
//!
//! ```text
//! Starting   -> Healthy/Degraded  required components report healthy
//! Starting   -> Failed            not ready after STARTUP_GRACE and the
//!                                 recovery attempts are exhausted
//! Healthy   <-> Degraded          optional components fail / come back
//! Healthy/Degraded -> Recovering  a required component fails
//! Recovering -> Healthy/Degraded  rebuild succeeded and components report healthy
//! Recovering -> Recovering        attempt failed; retry after BACKOFF
//! Recovering -> Failed            5th failure within CIRCUIT_WINDOW
//! Failed     -> Recovering        only `repair()` (the user's "repair engine")
//! ```
//!
//! The module is platform-neutral: the executor and the input sources are
//! reached through [`ExecutorControl`] and [`InputSources`], and all time
//! decisions use the injected [`Clock`], so the machine is tested with a
//! fake clock. `repair()` and automatic recovery share one rebuild path
//! (executor worker rebuild plus input `reattach`). Adapter calls never run
//! under the machine lock; a recovery attempt runs unlocked and commits by
//! attempt number (see [`Supervisor::tick`]). Any panic or timeout inside an
//! attempt is an attempt failure.
use crate::health::{Component, ComponentHealth, HealthSnapshot, HealthState, ReasonCode};
use crate::runtime::Runtime;
use crate::traits::Clock;
use arc_swap::ArcSwapOption;
use parking_lot::{Condvar, Mutex};
use std::collections::VecDeque;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Delay before recovery attempt 1, 2, 3, 4 and 5 of one incident; later
/// attempts keep the last value.
pub const BACKOFF: [Duration; 5] = [
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(15),
];
/// Failures counted towards the circuit breaker.
pub const CIRCUIT_WINDOW: Duration = Duration::from_secs(10 * 60);
/// The failure (or incident) that opens the circuit (Health → Failed).
pub const CIRCUIT_FAILURES: usize = 5;
pub const TICK: Duration = Duration::from_secs(1);
/// After a rebuild reported success, how long the components may take to
/// report healthy before the attempt counts as failed. 待实测 (hook and
/// RawInput re-registration time on real machines).
pub const VERIFY_TIMEOUT: Duration = Duration::from_secs(3);
/// How long Starting waits for the input sources to come up before
/// recovery attempts begin. 待基线确定.
pub const STARTUP_GRACE: Duration = Duration::from_secs(10);
/// `check_supervisor` respawns the driver when the machine has made no
/// progress (no completed tick, no committed attempt) for longer than this,
/// unless a recovery attempt is still within [`ATTEMPT_TIMEOUT`]. Measured on
/// the injected clock (the system clock in production).
pub const SUPERVISOR_STALE: Duration = Duration::from_secs(5);
/// Longest a recovery attempt (executor rebuild plus the input reattaches,
/// which run one after the other) may take. Past it the attempt counts as
/// failed, its late result is discarded and the next attempt follows the
/// backoff; within it `check()` does not treat the busy driver as stale.
/// 待实测: real reattach upper bounds (hooks ≈2 s, RawInput ≈4 s, so ≈6 s
/// in sequence) plus margin.
pub const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(10);

/// Failure text for a recovery attempt that panicked (fixed text: panic
/// payloads are never logged).
const ATTEMPT_PANICKED: &str = "attempt panicked";
/// Failure text for a recovery attempt that ran past [`ATTEMPT_TIMEOUT`].
const ATTEMPT_TIMED_OUT: &str = "attempt did not return within ATTEMPT_TIMEOUT";

/// One component's condition as reported to the supervisor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Ok,
    Degraded(ReasonCode),
    Down(ReasonCode),
}

impl Status {
    /// Input-source health → status: anything not Healthy/Degraded is down.
    pub fn from_health(health: &ComponentHealth) -> Self {
        let reason = health.reason.unwrap_or(ReasonCode::Internal);
        match health.state {
            HealthState::Healthy => Self::Ok,
            HealthState::Degraded => Self::Degraded(reason),
            HealthState::Starting | HealthState::Recovering | HealthState::Failed => {
                Self::Down(reason)
            }
        }
    }
    fn severity(self) -> u8 {
        match self {
            Self::Ok => 0,
            Self::Degraded(_) => 1,
            Self::Down(_) => 2,
        }
    }
}

/// Input sources (keyboard/mouse hooks, RawInput, focus events). Windows
/// wires `input::input_health` / `input::reattach` (P1b).
pub trait InputSources: Send + Sync {
    fn input_health(&self) -> Vec<ComponentHealth>;
    /// Re-registers one component; should return within a few hundred ms.
    fn reattach(&self, component: Component) -> Result<(), String>;
}

/// The executor as the supervisor sees it.
pub trait ExecutorControl: Send + Sync {
    /// Fences a task that has run past its budget (called first each tick).
    fn enforce_budget(&self, now: Instant);
    /// Executor (and related components such as the clipboard) status.
    fn probe(&self, now: Instant) -> Vec<(Component, Status)>;
    /// Replaces the executor worker with a fresh one.
    fn rebuild(&self) -> Result<(), String>;
}

struct Observed {
    executor: Vec<(Component, Status)>,
    inputs: Vec<(Component, Status)>,
}

impl Observed {
    /// Worst status per component, in `Component::ALL` order. The
    /// supervisor itself is always present (it is the one observing).
    fn merged(&self) -> Vec<(Component, Status)> {
        let mut merged: Vec<(Component, Status)> = vec![(Component::Supervisor, Status::Ok)];
        for &(component, status) in self.executor.iter().chain(&self.inputs) {
            match merged.iter_mut().find(|(c, _)| *c == component) {
                Some((_, current)) if status.severity() > current.severity() => *current = status,
                Some(_) => {}
                None => merged.push((component, status)),
            }
        }
        merged.sort_by_key(|(component, _)| *component);
        merged
    }
    fn required_down(&self) -> Vec<(Component, ReasonCode)> {
        self.merged()
            .into_iter()
            .filter_map(|(component, status)| match status {
                Status::Down(reason) if component.required() => Some((component, reason)),
                _ => None,
            })
            .collect()
    }
    fn settled_state(&self) -> HealthState {
        if self
            .merged()
            .iter()
            .all(|(_, status)| *status == Status::Ok)
        {
            HealthState::Healthy
        } else {
            HealthState::Degraded
        }
    }
}

struct Machine {
    state: HealthState,
    started: Instant,
    /// Failed recovery attempts within the circuit window.
    failures: VecDeque<Instant>,
    /// Entries into Recovering within the circuit window (a component that
    /// keeps failing right after successful rebuilds, e.g. a panic loop).
    incidents: VecDeque<Instant>,
    /// Failed attempts in the current incident (backoff position).
    streak: usize,
    next_attempt: Option<Instant>,
    verify_until: Option<Instant>,
    /// Set by `repair()`: rebuild the executor even if it looks healthy.
    forced: bool,
    published: Vec<ComponentHealth>,
    /// Number of the last attempt started.
    attempts: u64,
    /// The attempt running outside the lock. At most one; only its result
    /// is committed, and the machine makes no transition until it arrives,
    /// times out or `repair()` abandons it.
    in_flight: Option<InFlight>,
}

#[derive(Clone, Copy)]
struct InFlight {
    id: u64,
    started: Instant,
}

/// An attempt the machine decided to run once its lock is released.
struct Ticket {
    id: u64,
    forced: bool,
}

/// What `check()` judges the driver by (injected-clock times).
struct Beat {
    /// Last progress: a tick that advanced the machine, or a committed
    /// attempt result. Skipped ticks and discarded results do not count.
    progress: Instant,
    /// The attempt a ticking thread is running (id, start).
    attempt: Option<(u64, Instant)>,
}

fn prune(window: &mut VecDeque<Instant>, now: Instant) {
    while window
        .front()
        .is_some_and(|at| now.saturating_duration_since(*at) > CIRCUIT_WINDOW)
    {
        window.pop_front();
    }
}

pub struct Supervisor {
    clock: Arc<dyn Clock>,
    runtime: Arc<Runtime>,
    executor: Arc<dyn ExecutorControl>,
    inputs: Arc<dyn InputSources>,
    machine: Mutex<Machine>,
    /// Set by `repair()` when a running tick holds the machine; the next
    /// tick applies it.
    repair_requested: AtomicBool,
    /// Ticks are being skipped (logged once per stretch).
    skipping: AtomicBool,
    /// Progress heartbeat; taken after `machine` when both are held.
    beat: Mutex<Beat>,
    wake: Mutex<bool>,
    wake_signal: Condvar,
    driver: Mutex<Option<JoinHandle<()>>>,
    driver_generation: AtomicU64,
}

impl Supervisor {
    pub fn new(
        clock: Arc<dyn Clock>,
        runtime: Arc<Runtime>,
        executor: Arc<dyn ExecutorControl>,
        inputs: Arc<dyn InputSources>,
    ) -> Arc<Self> {
        let started = clock.now();
        Arc::new(Self {
            clock,
            runtime,
            executor,
            inputs,
            machine: Mutex::new(Machine {
                state: HealthState::Starting,
                started,
                failures: VecDeque::new(),
                incidents: VecDeque::new(),
                streak: 0,
                next_attempt: None,
                verify_until: None,
                forced: false,
                published: vec![],
                attempts: 0,
                in_flight: None,
            }),
            repair_requested: AtomicBool::new(false),
            skipping: AtomicBool::new(false),
            beat: Mutex::new(Beat {
                progress: started,
                attempt: None,
            }),
            wake: Mutex::new(false),
            wake_signal: Condvar::new(),
            driver: Mutex::new(None),
            driver_generation: AtomicU64::new(0),
        })
    }

    fn observe(&self, now: Instant) -> Observed {
        Observed {
            executor: self.executor.probe(now),
            inputs: self
                .inputs
                .input_health()
                .iter()
                .map(|health| (health.component, Status::from_health(health)))
                .collect(),
        }
    }

    /// One supervision step: enforce the executor budget, read every
    /// component, advance the machine, publish the snapshot and, when an
    /// attempt is due, run it and commit its result.
    ///
    /// No adapter call (`enforce_budget`, `probe`, `input_health`,
    /// `rebuild`, `reattach`) runs under the machine lock, so an adapter that
    /// blocks never stalls `repair()` or a replacement driver; the lock is
    /// held only to decide and to commit. An attempt is identified by its
    /// number: only the attempt in flight can commit, so a result that
    /// arrives after the attempt timed out or was abandoned by `repair()` is
    /// discarded. A driver blocked in an attempt is busy, not stale, until
    /// ATTEMPT_TIMEOUT (see [`Self::check`]).
    pub fn tick(&self) {
        self.executor.enforce_budget(self.clock.now());
        let observed = catch_unwind(AssertUnwindSafe(|| self.observe(self.clock.now())));
        let Some(mut machine) = self.machine.try_lock_for(TICK) else {
            if !self.skipping.swap(true, Ordering::SeqCst) {
                tracing::warn!(event = "supervisor.tick_skipped", "state machine busy");
            }
            return; // not progress: the heartbeat is left alone
        };
        self.skipping.store(false, Ordering::SeqCst);
        let now = self.clock.now();
        if self.repair_requested.swap(false, Ordering::SeqCst) {
            Self::arm_repair(&mut machine, now);
        }
        Self::expire_attempt(&mut machine, now);
        let observed = match observed {
            Ok(observed) => observed,
            Err(panic) => {
                // A probe that panics while a rebuild is being verified
                // fails that attempt; otherwise the tick is just lost.
                if machine.verify_until.take().is_some() {
                    Self::failure(&mut machine, now, ATTEMPT_PANICKED);
                }
                drop(machine);
                std::panic::resume_unwind(panic);
            }
        };
        let ticket = self.advance(&mut machine, &observed, now);
        self.publish(&mut machine, &observed);
        {
            let mut beat = self.beat.lock();
            beat.progress = now;
            if let Some(ticket) = &ticket {
                beat.attempt = Some((ticket.id, now));
            }
        }
        drop(machine);
        if let Some(ticket) = ticket {
            let outcome = self.attempt(&observed, ticket.forced);
            self.commit(ticket.id, outcome, &observed);
        }
    }

    /// Advances the machine on one observation (lock held, no adapter
    /// calls). Returns the attempt to run once the lock is released.
    fn advance(&self, machine: &mut Machine, observed: &Observed, now: Instant) -> Option<Ticket> {
        if machine.in_flight.is_some() {
            // Another thread is running an attempt; its result decides.
            return None;
        }
        match machine.state {
            HealthState::Starting => {
                if observed.required_down().is_empty() {
                    self.settle(machine, observed);
                } else if now.saturating_duration_since(machine.started) >= STARTUP_GRACE {
                    if machine.next_attempt.is_none() && machine.verify_until.is_none() {
                        machine.next_attempt = Some(now);
                    }
                    return self.recover(machine, observed, now);
                }
            }
            HealthState::Healthy | HealthState::Degraded => {
                let down = observed.required_down();
                if down.is_empty() {
                    machine.state = observed.settled_state();
                } else {
                    Self::incident(machine, &down, now);
                }
            }
            HealthState::Recovering => return self.recover(machine, observed, now),
            HealthState::Failed => {}
        }
        None
    }

    /// [`Self::tick`] behind `catch_unwind`; false when the tick panicked.
    pub fn tick_guarded(&self) -> bool {
        let ok = catch_unwind(AssertUnwindSafe(|| self.tick())).is_ok();
        if !ok {
            tracing::error!(
                event = "supervisor.panic",
                "supervisor tick panicked; next tick continues"
            );
        }
        ok
    }

    fn incident(machine: &mut Machine, down: &[(Component, ReasonCode)], now: Instant) {
        machine.incidents.push_back(now);
        prune(&mut machine.incidents, now);
        tracing::warn!(
            event = "supervisor.recovering",
            components = ?down,
            incidents = machine.incidents.len(),
            "required component down; recovering"
        );
        if machine.incidents.len() >= CIRCUIT_FAILURES {
            Self::open_circuit(machine, "incidents");
            return;
        }
        machine.state = HealthState::Recovering;
        machine.streak = 0;
        machine.verify_until = None;
        machine.next_attempt = Some(now + BACKOFF[0]);
    }

    fn recover(&self, machine: &mut Machine, observed: &Observed, now: Instant) -> Option<Ticket> {
        let healthy = observed.required_down().is_empty();
        if let Some(deadline) = machine.verify_until {
            if healthy {
                self.settle(machine, observed);
            } else if now >= deadline {
                machine.verify_until = None;
                Self::failure(
                    machine,
                    now,
                    "components did not report healthy after rebuild",
                );
            }
            return None;
        }
        if healthy && !machine.forced {
            // The fault cleared on its own (e.g. a hook came back).
            self.settle(machine, observed);
            return None;
        }
        if machine.next_attempt.is_none_or(|due| now < due) {
            return None;
        }
        machine.next_attempt = None;
        machine.attempts += 1;
        let id = machine.attempts;
        machine.in_flight = Some(InFlight { id, started: now });
        Some(Ticket {
            id,
            forced: std::mem::take(&mut machine.forced),
        })
    }

    /// Commits the result of attempt `id` (`before` is the observation it
    /// started from) if it is still the attempt in flight; otherwise it
    /// timed out or was abandoned by `repair()` and the result is dropped.
    ///
    /// The attempt's duration is checked here as well, under the lock: one
    /// that took longer than ATTEMPT_TIMEOUT is a timeout even if no tick
    /// noticed it while it ran, so it counts as one failure and whatever it
    /// returned (success included) is discarded.
    fn commit(&self, id: u64, outcome: Result<Observed, String>, before: &Observed) {
        let mut machine = self.machine.lock();
        // The attempt can block for seconds (sequential reattach): backoff
        // and the verification window start when it returned.
        let now = self.clock.now();
        let current = machine.in_flight.filter(|attempt| attempt.id == id);
        let mut beat = self.beat.lock();
        if beat.attempt.is_some_and(|(running, _)| running == id) {
            beat.attempt = None;
        }
        let Some(attempt) = current else {
            tracing::warn!(
                event = "supervisor.attempt_discarded",
                attempt = id,
                "result of an attempt that timed out or was superseded"
            );
            return;
        };
        beat.progress = now;
        drop(beat);
        machine.in_flight = None;
        if now.saturating_duration_since(attempt.started) > ATTEMPT_TIMEOUT {
            tracing::warn!(
                event = "supervisor.attempt_discarded",
                attempt = id,
                "attempt returned after ATTEMPT_TIMEOUT; counted as a timeout"
            );
            Self::failure(&mut machine, now, ATTEMPT_TIMED_OUT);
            self.publish(&mut machine, before);
            return;
        }
        let after;
        let observed = match outcome {
            Ok(observed) => {
                after = observed;
                if after.required_down().is_empty() {
                    self.settle(&mut machine, &after);
                } else {
                    machine.verify_until = Some(now + VERIFY_TIMEOUT);
                }
                &after
            }
            Err(error) => {
                Self::failure(&mut machine, now, &error);
                before
            }
        };
        self.publish(&mut machine, observed);
    }

    /// An attempt in flight for longer than ATTEMPT_TIMEOUT counts as one
    /// failure and is abandoned: its result is discarded when it arrives,
    /// and the next attempt follows the backoff.
    fn expire_attempt(machine: &mut Machine, now: Instant) {
        let Some(attempt) = machine.in_flight else {
            return;
        };
        if now.saturating_duration_since(attempt.started) > ATTEMPT_TIMEOUT {
            machine.in_flight = None;
            Self::failure(machine, now, ATTEMPT_TIMED_OUT);
        }
    }

    /// One recovery attempt: the rebuild plus the observation that judges
    /// it. A panic anywhere in it (executor rebuild, input reattach, probe)
    /// is an attempt failure like any other, so it is counted, backed off
    /// and can open the circuit instead of silently ending the incident.
    fn attempt(&self, observed: &Observed, forced: bool) -> Result<Observed, String> {
        catch_unwind(AssertUnwindSafe(|| {
            self.rebuild(observed, forced)?;
            Ok(self.observe(self.clock.now()))
        }))
        .unwrap_or_else(|_| Err(ATTEMPT_PANICKED.into()))
    }

    /// The single rebuild path for automatic recovery and `repair()`:
    /// rebuilds the executor worker when it is not healthy (always when
    /// forced) and reattaches every input component that is not healthy.
    fn rebuild(&self, observed: &Observed, forced: bool) -> Result<(), String> {
        let mut errors = vec![];
        let executor_ok = observed
            .executor
            .iter()
            .all(|(component, status)| *component != Component::Executor || *status == Status::Ok);
        if forced || !executor_ok {
            if let Err(error) = self.executor.rebuild() {
                errors.push(format!("executor: {error}"));
            }
        }
        for &(component, status) in &observed.inputs {
            if status != Status::Ok {
                if let Err(error) = self.inputs.reattach(component) {
                    errors.push(format!("{component:?}: {error}"));
                }
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }

    fn failure(machine: &mut Machine, now: Instant, error: &str) {
        machine.failures.push_back(now);
        prune(&mut machine.failures, now);
        if machine.failures.len() >= CIRCUIT_FAILURES {
            tracing::warn!(
                event = "supervisor.attempt_failed",
                error,
                failures = machine.failures.len()
            );
            Self::open_circuit(machine, "attempts");
            return;
        }
        machine.streak += 1;
        let delay = BACKOFF[machine.streak.min(BACKOFF.len() - 1)];
        machine.next_attempt = Some(now + delay);
        tracing::warn!(
            event = "supervisor.attempt_failed",
            error,
            failures = machine.failures.len(),
            retry_in_ms = delay.as_millis() as u64,
        );
    }

    fn open_circuit(machine: &mut Machine, cause: &'static str) {
        machine.state = HealthState::Failed;
        machine.next_attempt = None;
        machine.verify_until = None;
        machine.forced = false;
        tracing::error!(
            event = "supervisor.circuit_open",
            cause,
            "engine failed; waiting for repair"
        );
    }

    fn settle(&self, machine: &mut Machine, observed: &Observed) {
        let from = machine.state;
        machine.state = observed.settled_state();
        machine.streak = 0;
        machine.next_attempt = None;
        machine.verify_until = None;
        machine.forced = false;
        if from == HealthState::Recovering {
            tracing::info!(event = "supervisor.recovered", state = ?machine.state);
        }
    }

    fn publish(&self, machine: &mut Machine, observed: &Observed) {
        let wall = self.clock.wall_ms();
        let components: Vec<ComponentHealth> = observed
            .merged()
            .into_iter()
            .map(|(component, status)| {
                let (state, reason) = match status {
                    Status::Ok => (HealthState::Healthy, None),
                    Status::Degraded(reason) => (HealthState::Degraded, Some(reason)),
                    Status::Down(reason) if !component.required() => {
                        (HealthState::Degraded, Some(reason))
                    }
                    Status::Down(reason) => (
                        match machine.state {
                            HealthState::Starting => HealthState::Starting,
                            HealthState::Failed => HealthState::Failed,
                            _ => HealthState::Recovering,
                        },
                        Some(reason),
                    ),
                };
                let since_ms = machine
                    .published
                    .iter()
                    .find(|p| p.component == component && p.state == state && p.reason == reason)
                    .map_or(wall, |p| p.since_ms);
                ComponentHealth {
                    component,
                    state,
                    reason,
                    since_ms,
                }
            })
            .collect();
        machine.published = components.clone();
        self.runtime.publish_health(HealthSnapshot {
            state: machine.state,
            components,
        });
    }

    /// The user's "repair engine": clears the circuit breaker, enters
    /// Recovering and schedules a forced rebuild on the driver's next tick
    /// (woken immediately). Returns the Recovering snapshot; the outcome is
    /// published through the runtime's `Health` events.
    ///
    /// An attempt still in flight is abandoned (its result will be
    /// discarded) so the forced rebuild does not wait for it. Never blocks
    /// the caller for longer than one TICK: if the machine stays locked, the
    /// request is left for the next tick and the current snapshot is
    /// returned.
    pub fn repair(&self) -> HealthSnapshot {
        tracing::info!(event = "supervisor.repair");
        // Probed before locking: the probe is an adapter call.
        let observed = catch_unwind(AssertUnwindSafe(|| self.observe(self.clock.now())));
        match self.machine.try_lock_for(TICK) {
            Some(mut machine) => {
                Self::arm_repair(&mut machine, self.clock.now());
                if let Ok(observed) = &observed {
                    self.publish(&mut machine, observed);
                }
            }
            None => {
                self.repair_requested.store(true, Ordering::SeqCst);
                tracing::warn!(
                    event = "supervisor.repair_deferred",
                    "state machine busy; repair applies on the next tick"
                );
            }
        }
        self.wake();
        self.runtime.health()
    }

    fn arm_repair(machine: &mut Machine, now: Instant) {
        if let Some(attempt) = machine.in_flight.take() {
            tracing::info!(
                event = "supervisor.attempt_superseded",
                attempt = attempt.id
            );
        }
        machine.failures.clear();
        machine.incidents.clear();
        machine.streak = 0;
        machine.verify_until = None;
        machine.state = HealthState::Recovering;
        machine.forced = true;
        machine.next_attempt = Some(now);
    }

    fn wake(&self) {
        *self.wake.lock() = true;
        self.wake_signal.notify_all();
    }

    /// Time until the next tick: one TICK, or sooner for a due attempt or
    /// verification deadline (converted from the injected clock). Never
    /// waits for the machine lock: a stuck tick must not stall the driver.
    fn next_wait(&self) -> Duration {
        let now = self.clock.now();
        let Some(machine) = self.machine.try_lock() else {
            return TICK;
        };
        [machine.next_attempt, machine.verify_until]
            .into_iter()
            .flatten()
            .map(|due| due.saturating_duration_since(now))
            .fold(TICK, Duration::min)
    }

    /// Starts (or replaces) the driver thread that ticks every second.
    pub fn spawn_driver(self: &Arc<Self>) -> std::io::Result<()> {
        let generation = self.driver_generation.fetch_add(1, Ordering::SeqCst) + 1;
        let me = Arc::clone(self);
        let handle = std::thread::Builder::new()
            .name("qt-supervisor".into())
            .spawn(move || me.drive(generation))?;
        // A new driver gets SUPERVISOR_STALE to make its first progress.
        self.beat.lock().progress = self.clock.now();
        *self.driver.lock() = Some(handle);
        Ok(())
    }

    fn drive(&self, generation: u64) {
        while self.driver_generation.load(Ordering::SeqCst) == generation
            && !self.runtime.shutting_down()
        {
            self.tick_guarded();
            let wait = self.next_wait();
            let mut woken = self.wake.lock();
            if !*woken {
                self.wake_signal.wait_for(&mut woken, wait);
            }
            *woken = false;
        }
    }

    /// Main-thread watchdog (call every 5 s): respawns the driver if its
    /// thread ended or the machine is stale: no progress for longer than
    /// SUPERVISOR_STALE and no attempt that is still within ATTEMPT_TIMEOUT
    /// (a driver blocked in a slow reattach is busy, not dead; one blocked
    /// past the limit is replaced, and the new driver's tick counts the
    /// attempt as failed). Returns false when it had to respawn.
    pub fn check(self: &Arc<Self>) -> bool {
        if self.runtime.shutting_down() {
            return true;
        }
        let finished = self
            .driver
            .lock()
            .as_ref()
            .is_none_or(JoinHandle::is_finished);
        let now = self.clock.now();
        let stale = {
            let beat = self.beat.lock();
            let busy = beat.attempt.is_some_and(|(_, started)| {
                now.saturating_duration_since(started) <= ATTEMPT_TIMEOUT
            });
            !busy && now.saturating_duration_since(beat.progress) > SUPERVISOR_STALE
        };
        if !finished && !stale {
            return true;
        }
        tracing::warn!(event = "supervisor.respawn", finished, stale);
        if let Err(error) = self.spawn_driver() {
            tracing::error!(event = "supervisor.respawn_failed", %error);
        }
        false
    }
}

static SUPERVISOR: ArcSwapOption<Supervisor> = ArcSwapOption::const_empty();

/// Makes `supervisor` the process-wide instance used by [`repair`] and
/// [`check_supervisor`].
pub fn install(supervisor: Arc<Supervisor>) {
    SUPERVISOR.store(Some(supervisor));
}

/// `repair_engine`: forced rebuild; returns the (usually Recovering)
/// snapshot immediately. Without a supervisor, the current snapshot.
pub fn repair() -> HealthSnapshot {
    match SUPERVISOR.load_full() {
        Some(supervisor) => supervisor.repair(),
        None => crate::runtime::current_health(),
    }
}

/// Main-thread watchdog; call every 5 s. True when the supervisor driver is
/// alive (or none is installed), false when it was respawned.
pub fn check_supervisor() -> bool {
    SUPERVISOR
        .load_full()
        .is_none_or(|supervisor| supervisor.check())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::PauseReasons;
    use crate::mock::FakeClock;
    use crate::Shared;
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc;

    /// Test hook run inside an adapter call (may panic, block or move the
    /// fake clock).
    type Hook = Arc<dyn Fn() + Send + Sync>;

    fn run_hook(hook: &Mutex<Option<Hook>>) {
        let hook = hook.lock().clone();
        if let Some(hook) = hook {
            hook();
        }
    }

    struct FakeExecutor {
        clock: Arc<FakeClock>,
        status: Mutex<Status>,
        /// How many of the next rebuilds fail.
        refuse: Mutex<usize>,
        /// Whether a successful rebuild heals the executor.
        heals: Mutex<bool>,
        /// Fake-clock time of every rebuild call.
        rebuilds: Mutex<Vec<Duration>>,
        budget_checks: AtomicUsize,
        /// Runs at the start of every rebuild, after it is recorded.
        on_rebuild: Mutex<Option<Hook>>,
    }

    impl FakeExecutor {
        fn new(clock: &Arc<FakeClock>) -> Arc<Self> {
            Arc::new(Self {
                clock: Arc::clone(clock),
                status: Mutex::new(Status::Ok),
                refuse: Mutex::new(0),
                heals: Mutex::new(true),
                rebuilds: Mutex::new(vec![]),
                budget_checks: AtomicUsize::new(0),
                on_rebuild: Mutex::new(None),
            })
        }
        fn rebuild_count(&self) -> usize {
            self.rebuilds.lock().len()
        }
    }

    impl ExecutorControl for FakeExecutor {
        fn enforce_budget(&self, _now: Instant) {
            self.budget_checks.fetch_add(1, Ordering::SeqCst);
        }
        fn probe(&self, _now: Instant) -> Vec<(Component, Status)> {
            vec![(Component::Executor, *self.status.lock())]
        }
        fn rebuild(&self) -> Result<(), String> {
            self.rebuilds.lock().push(self.clock.elapsed());
            run_hook(&self.on_rebuild);
            let mut refuse = self.refuse.lock();
            if *refuse > 0 {
                *refuse -= 1;
                return Err("worker refused".into());
            }
            if *self.heals.lock() {
                *self.status.lock() = Status::Ok;
            }
            Ok(())
        }
    }

    struct FakeInputs {
        clock: Arc<FakeClock>,
        down: Mutex<Vec<(Component, HealthState, ReasonCode)>>,
        reattached: Mutex<Vec<Component>>,
        heal_on_reattach: Mutex<bool>,
        panic_once: Mutex<bool>,
        reads: AtomicUsize,
        /// Runs at the start of every reattach, after it is recorded.
        on_reattach: Mutex<Option<Hook>>,
    }

    impl FakeInputs {
        fn new(clock: &Arc<FakeClock>) -> Arc<Self> {
            Arc::new(Self {
                clock: Arc::clone(clock),
                down: Mutex::new(vec![]),
                reattached: Mutex::new(vec![]),
                heal_on_reattach: Mutex::new(true),
                panic_once: Mutex::new(false),
                reads: AtomicUsize::new(0),
                on_reattach: Mutex::new(None),
            })
        }
        fn set(&self, component: Component, state: HealthState, reason: ReasonCode) {
            let mut down = self.down.lock();
            down.retain(|(c, _, _)| *c != component);
            if state != HealthState::Healthy {
                down.push((component, state, reason));
            }
        }
    }

    impl InputSources for FakeInputs {
        fn input_health(&self) -> Vec<ComponentHealth> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            if std::mem::take(&mut *self.panic_once.lock()) {
                panic!("input probe bug");
            }
            let down = self.down.lock();
            [
                Component::KeyboardHook,
                Component::MouseHook,
                Component::RawInput,
                Component::FocusEvents,
            ]
            .into_iter()
            .map(|component| {
                let (state, reason) = down
                    .iter()
                    .find(|(c, _, _)| *c == component)
                    .map_or((HealthState::Healthy, None), |(_, s, r)| (*s, Some(*r)));
                ComponentHealth {
                    component,
                    state,
                    reason,
                    since_ms: self.clock.wall_ms(),
                }
            })
            .collect()
        }
        fn reattach(&self, component: Component) -> Result<(), String> {
            self.reattached.lock().push(component);
            run_hook(&self.on_reattach);
            if *self.heal_on_reattach.lock() {
                self.set(component, HealthState::Healthy, ReasonCode::Ok);
                Ok(())
            } else {
                Err("hook install refused".into())
            }
        }
    }

    struct Rig {
        clock: Arc<FakeClock>,
        runtime: Arc<Runtime>,
        executor: Arc<FakeExecutor>,
        inputs: Arc<FakeInputs>,
        supervisor: Arc<Supervisor>,
    }

    impl Rig {
        fn new() -> Self {
            let clock = Arc::new(FakeClock::new());
            let runtime = Arc::new(Runtime::new(clock.clone()));
            let executor = FakeExecutor::new(&clock);
            let inputs = FakeInputs::new(&clock);
            let supervisor = Supervisor::new(
                clock.clone(),
                runtime.clone(),
                executor.clone(),
                inputs.clone(),
            );
            Self {
                clock,
                runtime,
                executor,
                inputs,
                supervisor,
            }
        }
        /// A rig that already passed Starting.
        fn healthy() -> Self {
            let rig = Self::new();
            rig.supervisor.tick();
            assert_eq!(rig.state(), HealthState::Healthy);
            rig
        }
        fn state(&self) -> HealthState {
            self.runtime.health().state
        }
        fn step(&self, by: Duration) {
            self.clock.advance(by);
            self.supervisor.tick();
        }
        fn component(&self, component: Component) -> ComponentHealth {
            self.runtime
                .health()
                .components
                .into_iter()
                .find(|c| c.component == component)
                .unwrap()
        }
        fn failures(&self) -> usize {
            self.supervisor.machine.lock().failures.len()
        }
    }

    const MS: Duration = Duration::from_millis(1);

    /// A hook that counts its calls, then panics.
    fn panicking(calls: &Arc<AtomicUsize>) -> Hook {
        let calls = Arc::clone(calls);
        Arc::new(move || {
            calls.fetch_add(1, Ordering::SeqCst);
            panic!("adapter bug");
        })
    }

    /// Drives an incident in which every attempt panics (`attempts` counts
    /// them): each panic is one failure, the next attempt follows BACKOFF
    /// exactly, and the 5th failure opens the circuit.
    fn assert_attempt_panics_are_failures(rig: &Rig, attempts: &AtomicUsize) {
        rig.step(MS);
        assert_eq!(rig.state(), HealthState::Recovering);
        for (n, delay) in BACKOFF.into_iter().enumerate() {
            rig.step(delay - MS);
            assert_eq!(attempts.load(Ordering::SeqCst), n, "early at {n}");
            rig.step(MS);
            assert_eq!(attempts.load(Ordering::SeqCst), n + 1, "late at {n}");
            assert_eq!(rig.failures(), n + 1, "one failure per panic");
            let expected = if n + 1 < CIRCUIT_FAILURES {
                HealthState::Recovering
            } else {
                HealthState::Failed
            };
            assert_eq!(rig.state(), expected, "after failure {}", n + 1);
        }
        rig.step(Duration::from_secs(3600));
        assert_eq!(attempts.load(Ordering::SeqCst), CIRCUIT_FAILURES);
    }

    #[test]
    fn starting_becomes_healthy_and_lists_every_component() {
        let rig = Rig::new();
        assert_eq!(rig.state(), HealthState::Starting);
        rig.supervisor.tick();
        let health = rig.runtime.health();
        assert_eq!(health.state, HealthState::Healthy);
        let listed: Vec<Component> = health.components.iter().map(|c| c.component).collect();
        assert_eq!(
            listed,
            [
                Component::Executor,
                Component::KeyboardHook,
                Component::MouseHook,
                Component::RawInput,
                Component::Supervisor,
                Component::FocusEvents,
            ]
        );
        assert!(health
            .components
            .iter()
            .all(|c| c.state == HealthState::Healthy && c.reason.is_none()));
        assert!(rig.runtime.pause_reasons().is_empty());
    }

    #[test]
    fn starting_waits_for_inputs_then_fails_after_exhausted_attempts() {
        let rig = Rig::new();
        rig.inputs.set(
            Component::RawInput,
            HealthState::Starting,
            ReasonCode::RawInputMissing,
        );
        *rig.inputs.heal_on_reattach.lock() = false;
        rig.supervisor.tick();
        assert_eq!(rig.state(), HealthState::Starting);
        assert_eq!(
            rig.component(Component::RawInput).state,
            HealthState::Starting
        );
        assert!(
            rig.runtime.pause_reasons().is_empty(),
            "Starting carries no ENGINE_DOWN"
        );
        rig.step(STARTUP_GRACE - MS);
        assert!(rig.inputs.reattached.lock().is_empty(), "grace not over");
        rig.step(MS);
        assert_eq!(
            rig.inputs.reattached.lock().len(),
            1,
            "first attempt at grace"
        );
        for delay in &BACKOFF[1..4] {
            rig.step(*delay);
            assert_eq!(rig.state(), HealthState::Starting);
        }
        assert_eq!(rig.inputs.reattached.lock().len(), 4);
        rig.step(BACKOFF[4]);
        assert_eq!(rig.inputs.reattached.lock().len(), 5);
        assert_eq!(rig.state(), HealthState::Failed);
        assert_eq!(
            rig.component(Component::RawInput).state,
            HealthState::Failed
        );
        assert!(rig
            .runtime
            .pause_reasons()
            .contains(PauseReasons::ENGINE_DOWN));
    }

    #[test]
    fn backoff_is_strict_and_the_fifth_failure_opens_the_circuit() {
        let rig = Rig::healthy();
        *rig.executor.refuse.lock() = usize::MAX;
        *rig.executor.status.lock() = Status::Down(ReasonCode::ExecutorPanic);
        rig.step(MS);
        assert_eq!(rig.state(), HealthState::Recovering);
        let entered = rig.clock.elapsed();
        let mut expected = vec![];
        let mut at = entered;
        for delay in BACKOFF {
            rig.step(delay - MS);
            assert_eq!(
                rig.executor.rebuild_count(),
                expected.len(),
                "no attempt before {delay:?}"
            );
            rig.step(MS);
            at += delay;
            expected.push(at);
            assert_eq!(*rig.executor.rebuilds.lock(), expected);
        }
        assert_eq!(rig.state(), HealthState::Failed);
        let executor = rig.component(Component::Executor);
        assert_eq!(executor.state, HealthState::Failed);
        assert_eq!(executor.reason, Some(ReasonCode::ExecutorPanic));
        // Failed waits for the user: no further attempts, however long.
        rig.step(Duration::from_secs(3600));
        assert_eq!(rig.executor.rebuild_count(), 5);
        assert_eq!(rig.state(), HealthState::Failed);
    }

    #[test]
    fn failures_older_than_the_window_do_not_open_the_circuit() {
        let rig = Rig::healthy();
        *rig.executor.refuse.lock() = usize::MAX;
        *rig.executor.status.lock() = Status::Down(ReasonCode::ExecutorTimeout);
        rig.step(MS);
        for delay in &BACKOFF[..4] {
            rig.step(*delay);
        }
        assert_eq!(rig.executor.rebuild_count(), 4);
        // The next attempt comes 15 s later; age the first failures out.
        rig.step(CIRCUIT_WINDOW);
        assert_eq!(rig.executor.rebuild_count(), 5);
        assert_eq!(
            rig.state(),
            HealthState::Recovering,
            "only 1 failure in window"
        );
        rig.step(BACKOFF[4]);
        assert_eq!(rig.executor.rebuild_count(), 6, "keeps the 15 s cadence");
    }

    #[test]
    fn repair_reenters_recovering_and_rebuilds_through_the_same_path() {
        let rig = Rig::healthy();
        let shared = Shared::new();
        rig.runtime.attach(&shared); // starts paused: adopted as MANUAL
        *rig.executor.refuse.lock() = 5;
        *rig.executor.status.lock() = Status::Down(ReasonCode::ExecutorPanic);
        rig.inputs.set(
            Component::MouseHook,
            HealthState::Failed,
            ReasonCode::HookMissing,
        );
        *rig.inputs.heal_on_reattach.lock() = false;
        rig.step(MS);
        for delay in BACKOFF {
            rig.step(delay);
        }
        assert_eq!(rig.state(), HealthState::Failed);
        assert_eq!(
            rig.runtime.pause_reasons(),
            PauseReasons::MANUAL.with(PauseReasons::ENGINE_DOWN)
        );
        *rig.inputs.heal_on_reattach.lock() = true;
        let reattached_before = rig.inputs.reattached.lock().len();

        let snapshot = rig.supervisor.repair();
        assert_eq!(snapshot.state, HealthState::Recovering);
        assert_eq!(rig.state(), HealthState::Recovering);
        assert_eq!(
            rig.component(Component::Executor).state,
            HealthState::Recovering
        );
        rig.supervisor.tick();
        assert_eq!(rig.executor.rebuild_count(), 6, "one forced rebuild");
        assert_eq!(rig.inputs.reattached.lock().len(), reattached_before + 1);
        assert_eq!(rig.state(), HealthState::Healthy);
        assert_eq!(
            rig.runtime.pause_reasons(),
            PauseReasons::MANUAL,
            "ENGINE_DOWN cleared, MANUAL kept"
        );
        assert!(
            shared.paused.load(Ordering::SeqCst),
            "still manually paused"
        );
    }

    #[test]
    fn repair_forces_an_executor_rebuild_even_when_healthy() {
        let rig = Rig::healthy();
        rig.supervisor.repair();
        rig.supervisor.tick();
        assert_eq!(rig.executor.rebuild_count(), 1);
        assert!(
            rig.inputs.reattached.lock().is_empty(),
            "healthy hooks untouched"
        );
        assert_eq!(rig.state(), HealthState::Healthy);
    }

    #[test]
    fn repair_during_a_stuck_tick_is_applied_by_the_next_tick() {
        let rig = Rig::healthy();
        let held = rig.supervisor.machine.lock();
        let supervisor = Arc::clone(&rig.supervisor);
        let caller = std::thread::spawn(move || supervisor.repair());
        let snapshot = caller.join().unwrap();
        assert_eq!(
            snapshot.state,
            HealthState::Healthy,
            "returned without waiting"
        );
        // A tick that cannot get the machine still fences the executor, but
        // it is not progress: the heartbeat does not move.
        let budget_checks = rig.executor.budget_checks.load(Ordering::SeqCst);
        let progress = rig.supervisor.beat.lock().progress;
        rig.clock.advance(MS);
        let supervisor = Arc::clone(&rig.supervisor);
        std::thread::spawn(move || supervisor.tick())
            .join()
            .unwrap();
        assert_eq!(
            rig.executor.budget_checks.load(Ordering::SeqCst),
            budget_checks + 1
        );
        assert_eq!(rig.supervisor.beat.lock().progress, progress);
        drop(held);
        assert_eq!(rig.executor.rebuild_count(), 0);
        rig.supervisor.tick();
        assert_eq!(rig.executor.rebuild_count(), 1, "deferred forced rebuild");
        assert_eq!(rig.state(), HealthState::Healthy);
    }

    #[test]
    fn engine_down_clears_on_recovery_and_manual_stays() {
        let rig = Rig::healthy();
        let shared = Shared::new();
        shared.paused.store(false, Ordering::SeqCst);
        rig.runtime.attach(&shared);
        rig.inputs.set(
            Component::KeyboardHook,
            HealthState::Failed,
            ReasonCode::HookMissing,
        );
        rig.step(MS);
        assert_eq!(rig.state(), HealthState::Recovering);
        assert_eq!(rig.runtime.pause_reasons(), PauseReasons::ENGINE_DOWN);
        assert!(shared.paused.load(Ordering::SeqCst), "detection stops");
        rig.runtime.set_pause(PauseReasons::MANUAL, true);
        rig.step(BACKOFF[0]);
        assert_eq!(*rig.inputs.reattached.lock(), [Component::KeyboardHook]);
        assert_eq!(rig.state(), HealthState::Healthy);
        assert_eq!(rig.runtime.pause_reasons(), PauseReasons::MANUAL);
        rig.runtime.set_pause(PauseReasons::MANUAL, false);
        assert!(!shared.paused.load(Ordering::SeqCst));

        // Without a manual pause, recovery resumes detection on its own.
        rig.inputs.set(
            Component::RawInput,
            HealthState::Failed,
            ReasonCode::RawInputMissing,
        );
        rig.step(MS);
        assert!(shared.paused.load(Ordering::SeqCst));
        rig.step(BACKOFF[0]);
        assert_eq!(rig.state(), HealthState::Healthy);
        assert!(rig.runtime.pause_reasons().is_empty());
        assert!(!shared.paused.load(Ordering::SeqCst));
    }

    #[test]
    fn self_healing_fault_recovers_without_a_rebuild() {
        let rig = Rig::healthy();
        rig.inputs.set(
            Component::RawInput,
            HealthState::Failed,
            ReasonCode::RawInputMissing,
        );
        rig.step(MS);
        assert_eq!(rig.state(), HealthState::Recovering);
        rig.inputs
            .set(Component::RawInput, HealthState::Healthy, ReasonCode::Ok);
        rig.step(100 * MS);
        assert_eq!(rig.state(), HealthState::Healthy);
        assert!(rig.inputs.reattached.lock().is_empty());
        assert_eq!(rig.executor.rebuild_count(), 0);
    }

    #[test]
    fn rebuild_that_does_not_heal_fails_after_the_verify_timeout() {
        let rig = Rig::healthy();
        *rig.executor.heals.lock() = false;
        *rig.executor.status.lock() = Status::Down(ReasonCode::ExecutorTimeout);
        rig.step(MS);
        rig.step(BACKOFF[0]);
        assert_eq!(rig.executor.rebuild_count(), 1);
        rig.step(VERIFY_TIMEOUT - MS);
        assert_eq!(rig.supervisor.machine.lock().failures.len(), 0);
        *rig.executor.status.lock() = Status::Down(ReasonCode::ExecutorTimeout);
        rig.step(MS);
        assert_eq!(rig.supervisor.machine.lock().failures.len(), 1);
        assert_eq!(rig.state(), HealthState::Recovering);
        // The next attempt follows the backoff from the failure.
        rig.step(BACKOFF[1]);
        assert_eq!(rig.executor.rebuild_count(), 2);
    }

    #[test]
    fn repeated_incidents_open_the_circuit() {
        let rig = Rig::healthy();
        for round in 1..=CIRCUIT_FAILURES {
            *rig.executor.status.lock() = Status::Down(ReasonCode::ExecutorPanic);
            rig.step(MS);
            if round == CIRCUIT_FAILURES {
                break;
            }
            assert_eq!(rig.state(), HealthState::Recovering, "round {round}");
            rig.step(BACKOFF[0]);
            assert_eq!(rig.state(), HealthState::Healthy, "round {round}");
        }
        assert_eq!(rig.state(), HealthState::Failed, "panic loop is cut off");
        assert_eq!(rig.executor.rebuild_count(), CIRCUIT_FAILURES - 1);
    }

    #[test]
    fn optional_components_only_degrade() {
        let rig = Rig::healthy();
        rig.inputs.set(
            Component::FocusEvents,
            HealthState::Failed,
            ReasonCode::HookMissing,
        );
        rig.step(MS);
        assert_eq!(rig.state(), HealthState::Degraded);
        let focus = rig.component(Component::FocusEvents);
        assert_eq!(focus.state, HealthState::Degraded);
        assert_eq!(focus.reason, Some(ReasonCode::HookMissing));
        assert_eq!(focus.since_ms, rig.clock.wall_ms());
        assert!(rig.runtime.pause_reasons().is_empty());
        rig.step(Duration::from_secs(2));
        assert_eq!(
            rig.component(Component::FocusEvents).since_ms,
            focus.since_ms,
            "since_ms only moves on change"
        );
        rig.inputs
            .set(Component::FocusEvents, HealthState::Healthy, ReasonCode::Ok);
        rig.step(MS);
        assert_eq!(rig.state(), HealthState::Healthy);
    }

    #[test]
    fn executor_reports_merge_with_the_worst_status_winning() {
        let observed = Observed {
            executor: vec![
                (Component::Executor, Status::Ok),
                (
                    Component::Clipboard,
                    Status::Degraded(ReasonCode::ClipboardBusy),
                ),
            ],
            inputs: vec![
                (Component::Clipboard, Status::Ok),
                (
                    Component::RawInput,
                    Status::Down(ReasonCode::RawInputMissing),
                ),
            ],
        };
        assert_eq!(
            observed.merged(),
            [
                (Component::Executor, Status::Ok),
                (
                    Component::RawInput,
                    Status::Down(ReasonCode::RawInputMissing)
                ),
                (Component::Supervisor, Status::Ok),
                (
                    Component::Clipboard,
                    Status::Degraded(ReasonCode::ClipboardBusy)
                ),
            ]
        );
        assert_eq!(
            observed.required_down(),
            [(Component::RawInput, ReasonCode::RawInputMissing)]
        );
    }

    #[test]
    fn a_panicking_tick_is_contained() {
        let rig = Rig::healthy();
        *rig.inputs.panic_once.lock() = true;
        assert!(!rig.supervisor.tick_guarded());
        assert!(rig.supervisor.tick_guarded());
        assert_eq!(rig.state(), HealthState::Healthy);
    }

    #[test]
    fn a_panicking_rebuild_counts_as_a_failure() {
        let rig = Rig::healthy();
        let attempts = Arc::new(AtomicUsize::new(0));
        *rig.executor.on_rebuild.lock() = Some(panicking(&attempts));
        *rig.executor.status.lock() = Status::Down(ReasonCode::ExecutorPanic);
        assert_attempt_panics_are_failures(&rig, &attempts);
    }

    #[test]
    fn a_panicking_reattach_counts_as_a_failure() {
        let rig = Rig::healthy();
        let attempts = Arc::new(AtomicUsize::new(0));
        *rig.inputs.on_reattach.lock() = Some(panicking(&attempts));
        rig.inputs.set(
            Component::KeyboardHook,
            HealthState::Failed,
            ReasonCode::HookMissing,
        );
        assert_attempt_panics_are_failures(&rig, &attempts);
        assert_eq!(rig.executor.rebuild_count(), 0, "executor was healthy");
    }

    #[test]
    fn a_probe_panic_right_after_rebuild_counts_as_a_failure() {
        let rig = Rig::healthy();
        let attempts = Arc::new(AtomicUsize::new(0));
        let (calls, inputs) = (Arc::clone(&attempts), Arc::clone(&rig.inputs));
        *rig.executor.on_rebuild.lock() = Some(Arc::new(move || {
            calls.fetch_add(1, Ordering::SeqCst);
            *inputs.panic_once.lock() = true; // the post-rebuild observe
        }));
        *rig.executor.heals.lock() = false;
        *rig.executor.status.lock() = Status::Down(ReasonCode::ExecutorTimeout);
        assert_attempt_panics_are_failures(&rig, &attempts);
    }

    #[test]
    fn a_probe_panic_while_verifying_counts_as_a_failure() {
        let rig = Rig::healthy();
        *rig.executor.heals.lock() = false;
        *rig.executor.status.lock() = Status::Down(ReasonCode::ExecutorTimeout);
        rig.step(MS);
        rig.step(BACKOFF[0]);
        assert_eq!(rig.executor.rebuild_count(), 1);
        assert!(rig.supervisor.machine.lock().verify_until.is_some());
        *rig.inputs.panic_once.lock() = true;
        rig.clock.advance(MS);
        assert!(!rig.supervisor.tick_guarded(), "the tick itself is lost");
        assert_eq!(rig.failures(), 1);
        rig.step(BACKOFF[1] - MS);
        assert_eq!(rig.executor.rebuild_count(), 1);
        rig.step(MS);
        assert_eq!(rig.executor.rebuild_count(), 2, "retried after BACKOFF[1]");
        assert_eq!(rig.state(), HealthState::Recovering);
    }

    /// A rebuild that takes longer than VERIFY_TIMEOUT and BACKOFF[1].
    const SLOW_REBUILD: Duration = Duration::from_secs(4);

    fn slow_rebuilds(rig: &Rig) {
        let clock = Arc::clone(&rig.clock);
        *rig.executor.on_rebuild.lock() = Some(Arc::new(move || clock.advance(SLOW_REBUILD)));
    }

    #[test]
    fn backoff_counts_from_the_end_of_a_slow_rebuild() {
        let rig = Rig::healthy();
        slow_rebuilds(&rig);
        *rig.executor.refuse.lock() = usize::MAX;
        *rig.executor.status.lock() = Status::Down(ReasonCode::ExecutorTimeout);
        rig.step(MS);
        rig.step(BACKOFF[0]);
        let mut expected = vec![rig.clock.elapsed() - SLOW_REBUILD];
        for delay in &BACKOFF[1..] {
            let ended = rig.clock.elapsed();
            rig.step(*delay - MS);
            assert_eq!(rig.executor.rebuild_count(), expected.len(), "early");
            rig.step(MS);
            expected.push(ended + *delay);
            assert_eq!(*rig.executor.rebuilds.lock(), expected);
        }
        assert_eq!(rig.state(), HealthState::Failed);
    }

    #[test]
    fn verification_window_starts_when_the_rebuild_returns() {
        let rig = Rig::healthy();
        slow_rebuilds(&rig);
        *rig.executor.heals.lock() = false;
        *rig.executor.status.lock() = Status::Down(ReasonCode::ExecutorTimeout);
        rig.step(MS);
        rig.step(BACKOFF[0]);
        assert_eq!(rig.executor.rebuild_count(), 1);
        rig.step(VERIFY_TIMEOUT - MS);
        assert_eq!(rig.failures(), 0, "window not over");
        rig.step(MS);
        assert_eq!(rig.failures(), 1, "window over");
        assert_eq!(rig.state(), HealthState::Recovering);
        rig.step(BACKOFF[1]);
        assert_eq!(rig.executor.rebuild_count(), 2);
    }

    /// The rebuild overruns ATTEMPT_TIMEOUT and then reports success, with
    /// no tick or check in between to notice: the commit itself treats it
    /// as a timeout, so it is one failure and the success is discarded.
    #[test]
    fn an_attempt_that_overran_the_timeout_fails_at_commit() {
        let rig = Rig::healthy();
        let clock = Arc::clone(&rig.clock);
        *rig.executor.on_rebuild.lock() = Some(Arc::new(move || {
            clock.advance(ATTEMPT_TIMEOUT + MS);
        }));
        *rig.executor.status.lock() = Status::Down(ReasonCode::ExecutorTimeout);
        rig.step(MS);
        rig.step(BACKOFF[0]);
        assert_eq!(rig.executor.rebuild_count(), 1);
        assert_eq!(
            *rig.executor.status.lock(),
            Status::Ok,
            "the rebuild healed"
        );
        assert_eq!(rig.failures(), 1);
        assert_eq!(rig.state(), HealthState::Recovering, "success discarded");
        let machine = rig.supervisor.machine.lock();
        assert!(machine.in_flight.is_none());
        assert_eq!(machine.streak, 1);
        assert_eq!(machine.next_attempt, Some(rig.clock.now() + BACKOFF[1]));
    }

    /// Puts a parked thread in the driver slot: `check()` sees a live
    /// driver that never ticks. Dropping the sender ends the thread.
    fn parked_driver(rig: &Rig) -> mpsc::Sender<()> {
        let (tx, rx) = mpsc::channel::<()>();
        let handle = std::thread::spawn(move || {
            let _ = rx.recv();
        });
        *rig.supervisor.driver.lock() = Some(handle);
        tx
    }

    /// Stops the driver `check()` spawned and waits for its thread.
    fn retire_driver(rig: &Rig) {
        rig.supervisor
            .driver_generation
            .fetch_add(1, Ordering::SeqCst);
        rig.supervisor.wake();
        let handle = rig.supervisor.driver.lock().take();
        handle.unwrap().join().unwrap();
    }

    struct Gate {
        entered: mpsc::Receiver<()>,
        release: mpsc::Sender<()>,
    }

    /// The first rebuild signals `entered` and blocks until `release`;
    /// later rebuilds run straight through.
    fn block_first_rebuild(rig: &Rig) -> Gate {
        let (entered_tx, entered) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        let (entered_tx, release_rx) = (Mutex::new(entered_tx), Mutex::new(release_rx));
        let first = AtomicBool::new(true);
        *rig.executor.on_rebuild.lock() = Some(Arc::new(move || {
            if first.swap(false, Ordering::SeqCst) {
                entered_tx.lock().send(()).unwrap();
                let _ = release_rx.lock().recv();
            }
        }));
        Gate { entered, release }
    }

    /// Runs one tick on another thread until it blocks in the first
    /// rebuild; returns that thread and the fake time the attempt started.
    fn tick_into_blocked_rebuild(rig: &Rig, gate: &Gate) -> (JoinHandle<()>, Duration) {
        let supervisor = Arc::clone(&rig.supervisor);
        let stuck = std::thread::spawn(move || supervisor.tick());
        gate.entered
            .recv_timeout(Duration::from_secs(5))
            .expect("attempt started");
        (stuck, rig.clock.elapsed())
    }

    #[test]
    fn a_stuck_attempt_times_out_into_a_failure_and_its_late_result_is_discarded() {
        let rig = Rig::healthy();
        let driver = parked_driver(&rig);
        let gate = block_first_rebuild(&rig);
        *rig.executor.status.lock() = Status::Down(ReasonCode::ExecutorTimeout);
        rig.step(MS);
        rig.clock.advance(BACKOFF[0]);
        let (stuck, started) = tick_into_blocked_rebuild(&rig, &gate);
        assert!(
            rig.supervisor.machine.try_lock().is_some(),
            "the machine is not locked across the rebuild"
        );

        // Busy in an attempt within ATTEMPT_TIMEOUT: not stale, even with
        // no progress for longer than SUPERVISOR_STALE.
        let busy = SUPERVISOR_STALE + Duration::from_secs(1);
        rig.clock.advance(busy);
        assert!(rig.supervisor.check());
        // Stuck past the limit: reported (and the driver replaced).
        rig.clock.advance(ATTEMPT_TIMEOUT - busy + MS);
        assert!(
            !rig.supervisor.check(),
            "a stuck attempt must not look alive"
        );
        retire_driver(&rig);
        drop(driver);

        // The next tick (whichever driver runs it) counts one failure.
        rig.supervisor.tick();
        let expired = rig.clock.elapsed();
        assert_eq!(expired, started + ATTEMPT_TIMEOUT + MS);
        assert_eq!(rig.failures(), 1);
        assert_eq!(rig.state(), HealthState::Recovering);
        assert!(rig.supervisor.machine.lock().in_flight.is_none());

        // The late result (a failure, had it counted) is discarded.
        *rig.executor.refuse.lock() = 1;
        gate.release.send(()).unwrap();
        stuck.join().unwrap();
        assert_eq!(*rig.executor.refuse.lock(), 0, "the late rebuild returned");
        assert_eq!(rig.failures(), 1, "late result discarded");
        assert!(rig.supervisor.beat.lock().attempt.is_none());

        // The retry follows the backoff from the timeout.
        rig.step(BACKOFF[1] - MS);
        assert_eq!(rig.executor.rebuild_count(), 1);
        rig.step(MS);
        assert_eq!(
            *rig.executor.rebuilds.lock(),
            [started, expired + BACKOFF[1]]
        );
        assert_eq!(rig.state(), HealthState::Healthy);
    }

    #[test]
    fn one_attempt_at_a_time_and_repair_supersedes_a_stuck_one() {
        let rig = Rig::healthy();
        let gate = block_first_rebuild(&rig);
        *rig.executor.status.lock() = Status::Down(ReasonCode::ExecutorPanic);
        rig.step(MS);
        rig.clock.advance(BACKOFF[0]);
        let (stuck, _) = tick_into_blocked_rebuild(&rig, &gate);

        // Another ticking thread (a second driver) neither starts a second
        // attempt nor makes a transition while the first one runs.
        for _ in 0..3 {
            rig.step(Duration::from_secs(2));
        }
        assert_eq!(rig.executor.rebuild_count(), 1);
        assert_eq!(rig.failures(), 0);
        assert_eq!(rig.state(), HealthState::Recovering);

        // repair() abandons it; the forced rebuild runs on the next tick.
        assert_eq!(rig.supervisor.repair().state, HealthState::Recovering);
        rig.supervisor.tick();
        assert_eq!(rig.executor.rebuild_count(), 2);
        assert_eq!(rig.state(), HealthState::Healthy);

        // The abandoned attempt's late result (a failure) is discarded.
        *rig.executor.refuse.lock() = 1;
        gate.release.send(()).unwrap();
        stuck.join().unwrap();
        assert_eq!(*rig.executor.refuse.lock(), 0, "the late rebuild returned");
        assert_eq!(rig.failures(), 0);
        assert_eq!(rig.state(), HealthState::Healthy);
    }

    #[test]
    fn a_six_second_sequential_reattach_does_not_replace_the_driver() {
        let rig = Rig::healthy();
        let _driver = parked_driver(&rig);
        let calls = AtomicUsize::new(0);
        let clock = Arc::clone(&rig.clock);
        let supervisor = Arc::clone(&rig.supervisor);
        let verdicts = Arc::new(Mutex::new(vec![]));
        let seen = Arc::clone(&verdicts);
        // Real upper bounds: a hook ≈2 s, then RawInput ≈4 s.
        *rig.inputs.on_reattach.lock() = Some(Arc::new(move || {
            let took = if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                2
            } else {
                4
            };
            clock.advance(Duration::from_secs(took));
            seen.lock().push(supervisor.check());
        }));
        rig.inputs.set(
            Component::KeyboardHook,
            HealthState::Failed,
            ReasonCode::HookMissing,
        );
        rig.inputs.set(
            Component::RawInput,
            HealthState::Failed,
            ReasonCode::RawInputMissing,
        );
        let generation = rig.supervisor.driver_generation.load(Ordering::SeqCst);
        rig.step(MS);
        rig.step(BACKOFF[0]);
        assert_eq!(*verdicts.lock(), [true, true], "checked at +2 s and +6 s");
        assert_eq!(
            rig.supervisor.driver_generation.load(Ordering::SeqCst),
            generation,
            "driver not replaced"
        );
        assert_eq!(rig.state(), HealthState::Healthy);
        assert!(rig.supervisor.check(), "the commit is progress");
        *rig.inputs.on_reattach.lock() = None; // breaks the Arc cycle
    }

    #[test]
    fn driver_ticks_and_the_watchdog_respawns_it() {
        let rig = Rig::new();
        rig.supervisor.spawn_driver().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while rig.state() != HealthState::Healthy {
            assert!(Instant::now() < deadline, "driver never ticked");
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(rig.supervisor.check(), "fresh driver is alive");
        // Retire the driver (as if its thread had ended) and wake it.
        rig.supervisor
            .driver_generation
            .fetch_add(1, Ordering::SeqCst);
        rig.supervisor.wake();
        while !rig
            .supervisor
            .driver
            .lock()
            .as_ref()
            .is_some_and(JoinHandle::is_finished)
        {
            assert!(Instant::now() < deadline, "driver did not stop");
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(!rig.supervisor.check(), "dead driver is respawned");
        let reads = rig.inputs.reads.load(Ordering::SeqCst);
        rig.supervisor.wake();
        while rig.inputs.reads.load(Ordering::SeqCst) == reads {
            assert!(Instant::now() < deadline, "respawned driver never ticked");
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(rig.supervisor.check());
        rig.supervisor
            .driver_generation
            .fetch_add(1, Ordering::SeqCst);
        rig.supervisor.wake();
    }
}
