//! Engine runtime: state snapshot, pause, manual execution and recording.
use crate::{
    app_state::{AppState, Managed},
    error::CommandError,
    startup::StartupProblem,
};
use quicker_domain::*;
use quicker_platform::{
    health::{HealthState, PauseReasons, ReasonCode},
    runtime::Runtime,
    ExecutionResult, ManualFailure, Shared,
};
use std::{
    io,
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc::{self, RecvTimeoutError, TryRecvError},
        Arc, Mutex, PoisonError,
    },
    time::Duration,
};
use tauri::{Manager, State};

#[tauri::command]
pub fn load_state(store: State<Managed>) -> Result<AppState, CommandError> {
    Ok(store.lock().state())
}
/// Every problem found at startup (the `startup-problem` events in full).
#[tauri::command]
pub fn startup_problems(store: State<Managed>) -> Vec<StartupProblem> {
    store.lock().startup_problems().to_vec()
}
/// Sets or clears the manual pause reason only (contract §4). Other reasons
/// (safe mode, recording, self-test, the engine not up) keep the engine
/// paused, so `set_paused(false)` may return a paused state. A resume is
/// refused with `safe_mode` in safe mode and `engine_unavailable` while the
/// supervisor's health does not accept it; a resume during a recording
/// clears MANUAL and stays paused until the recording stops.
#[tauri::command]
pub fn set_paused(paused: bool, store: State<Managed>) -> Result<AppState, CommandError> {
    let s = store.lock();
    s.set_manual_pause(paused, quicker_platform::engine_ready)?;
    Ok(s.state())
}
#[tauri::command]
pub fn cancel_execution(store: State<Managed>) -> bool {
    let s = store.lock();
    s.engine.cancel.store(true, Ordering::SeqCst);
    true
}
#[tauri::command]
pub async fn execute_action(
    action: Action,
    app: tauri::AppHandle,
) -> Result<ExecutionResult, CommandError> {
    let (engine, runtime, busy, permit, compiled, action, speed) = {
        let state = app.state::<Managed>();
        let s = state.lock();
        // Safe mode: the data is for viewing; nothing runs (B-04).
        s.ensure_writable()?;
        let (busy, permit) = admit(&s.engine, &MANUAL_JOBS, || s.runtime.health().state)?;
        s.engine.cancel.store(false, Ordering::SeqCst);
        // The active rules as compiled at load or publish (B-43).
        let compiled = s.engine.compiled.lock().unwrap().clone();
        (
            s.engine.clone(),
            s.runtime.clone(),
            busy,
            permit,
            compiled,
            action.clone(),
            match &action {
                Action::Replay { speed, .. } => *speed,
                _ => 1.0,
            },
        )
    };
    // The runtime does not watch `busy`, so the claim and its release are
    // announced here (`engine-state`). The run's history entry is the
    // executor's: written when the run settles, or by the fence when the
    // run stalls (F5-1).
    runtime.refresh_state();
    let window = app.get_webview_window("main");
    if let Some(w) = &window {
        if let Err(e) = w.hide() {
            drop(busy);
            runtime.refresh_state();
            return Err(CommandError::internal(e));
        }
    }
    // The claim stays here and the job's thread acts with tickets that free
    // nothing (F5-1 C): a thread that never comes back cannot hold or free
    // `busy`; dropping the claim in `restore` is the only release.
    let ticket = busy.ticket();
    let watch = busy.ticket();
    let joined = run_job(
        spawn_thread,
        permit,
        move || {
            // Each checkpoint restarts the claim's stall budget, and stops
            // the run, sending nothing, once the claim was fenced (F5-1 B).
            let checkpoint = || ticket.progress().map_err(|_| RunError::Fenced);
            std::thread::sleep(Duration::from_secs(3));
            checkpoint()?;
            let target = quicker_platform::foreground();
            let generation = engine.generation.load(Ordering::SeqCst);
            let mut context = VariableContext {
                clipboard: arboard::Clipboard::new()
                    .ok()
                    .and_then(|mut cb| cb.get_text().ok()),
                ..Default::default()
            };
            checkpoint()?;
            let needs_selection = serde_json::to_string(&action)
                .unwrap_or_default()
                .contains("{selection}")
                || matches!(action, Action::Reusable { .. })
                    && serde_json::to_string(&compiled.config)
                        .unwrap_or_default()
                        .contains("{selection}");
            if needs_selection {
                context.selection = quicker_platform::capture_selection().ok();
            }
            checkpoint()?;
            let plan = compiled
                .expand_reusable(&action, &context)
                .map_err(RunError::Invalid)?;
            ticket
                .execute(plan, target, generation, 0, None, speed)
                .map_err(RunError::Failed)
        },
        move || watch.fenced(),
        FENCE_POLL,
        || {
            drop(busy);
            runtime.refresh_state();
            if let Some(w) = window {
                let _ = w.show();
            }
        },
    )
    .await;
    conclude(joined, runtime.health().state)
}

/// How often [`execute_action`] looks at its claim's fence while the run
/// goes on: the command returns this long after a fence at the latest.
const FENCE_POLL: Duration = Duration::from_millis(250);

/// At most this many manual jobs that have not really ended (F5-1 X2). Each
/// job runs on a thread of its own (X3), and a fenced job is left to itself:
/// its thread may never return (a system call that never does). Each job
/// keeps its slot until its thread returns, so stuck threads cannot pile up.
/// Four: the live run plus room for three stuck ones, so a stuck job does
/// not stop the next runs (each stuck job already cost a fence and a
/// rebuild); a desktop that strands more than that needs the user's
/// attention, not more threads.
const MAX_MANUAL_JOBS: usize = 4;

/// The slots of the manual jobs that have not ended (F5-1 X2).
static MANUAL_JOBS: JobSlots = JobSlots::new(MAX_MANUAL_JOBS);

/// A count of live jobs under a limit (F5-1 X2).
struct JobSlots {
    live: AtomicUsize,
    limit: usize,
}

impl JobSlots {
    const fn new(limit: usize) -> Self {
        Self {
            live: AtomicUsize::new(0),
            limit,
        }
    }

    /// A slot, or `None` while `limit` jobs hold theirs.
    fn take(&'static self) -> Option<JobPermit> {
        self.live
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |live| {
                (live < self.limit).then_some(live + 1)
            })
            .ok()
            .map(|_| JobPermit(self))
    }
}

/// One job's slot, given back when dropped.
struct JobPermit(&'static JobSlots);

impl Drop for JobPermit {
    fn drop(&mut self) {
        self.0.live.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Admits a manual run (F5-1 C, X2): its `busy` claim, then a job slot.
/// Refused, it holds neither and nothing has started (no window hidden, no
/// history entry). `health` is read only for a refusal at the cap.
fn admit(
    engine: &Arc<Shared>,
    slots: &'static JobSlots,
    health: impl FnOnce() -> HealthState,
) -> Result<(quicker_platform::ManualBusy, JobPermit), CommandError> {
    // `busy` is the executor's: claimed and given back under its lifecycle
    // lock, never over a task's (dropping the claim frees it).
    let Some(busy) = quicker_platform::ManualBusy::claim(engine) else {
        return Err(CommandError::Busy);
    };
    let Some(permit) = slots.take() else {
        drop(busy);
        return Err(jobs_exhausted(health()));
    };
    Ok((busy, permit))
}

/// A thread's body, as [`start_run`] hands it to its spawner.
type Task = Box<dyn FnOnce() + Send + 'static>;

/// The thread a manual run's job runs on (F5-1 X3).
const JOB_THREAD: &str = "qt-manual-run";

/// The thread that waits for the job's end or its claim's fence (F5-1 X2).
const WATCH_THREAD: &str = "qt-manual-watch";

/// Starts `task` on a thread of its own named `name`: the spawner of
/// [`start_run`] (F5-1 X2, X3). A thread that does not start is an error,
/// never a panic nor a task queued for later.
fn spawn_thread(name: &'static str, task: Task) -> io::Result<()> {
    std::thread::Builder::new()
        .name(name.into())
        .spawn(task)
        .map(drop)
}

/// The answer when [`MAX_MANUAL_JOBS`] jobs have not ended (F5-1 X2),
/// logged without content. `Busy` while the supervisor accepts a resume:
/// earlier runs really are still going, stuck past their fence outside an
/// executor that was rebuilt and serves, so "the previous action is still
/// running" is the truth, where `engine_unavailable` would name a healthy
/// engine unavailable. `EngineUnavailable` with the supervisor's state when
/// it does not accept a resume.
fn jobs_exhausted(health: HealthState) -> CommandError {
    tracing::warn!(
        event = "command.execute_refused",
        reason = "jobs_exhausted",
        limit = MAX_MANUAL_JOBS,
        ?health,
        "manual run refused: earlier runs have not ended"
    );
    if health.accepts_resume() {
        CommandError::Busy
    } else {
        CommandError::EngineUnavailable { health }
    }
}

/// Why the job of a manual run ended without a result.
enum RunError {
    /// The action does not expand (an invalid or unavailable rule).
    Invalid(Vec<Diagnostic>),
    /// A progress checkpoint found the claim fenced: nothing was sent.
    Fenced,
    /// The run failed: the reason code of its history entry, never the
    /// run's text, which may echo the rule's content (F5-1 H).
    Failed(ManualFailure),
}

/// How the wait for a manual run's job ended.
#[derive(Debug, PartialEq, Eq)]
enum RunEnd {
    /// The job is done (returned or unwound): its result can be joined.
    Finished,
    /// The claim was fenced while the job went on.
    Fenced,
}

/// Waits until the job is done (its end of `done` sent or dropped) or its
/// claim is fenced, looking at `fenced` every `poll`. A job that is done by
/// the time the fence is seen counts as finished.
fn wait_for_run(done: &mpsc::Receiver<()>, fenced: impl Fn() -> bool, poll: Duration) -> RunEnd {
    loop {
        match done.recv_timeout(poll) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => return RunEnd::Finished,
            Err(RecvTimeoutError::Timeout) if fenced() => {
                return match done.try_recv() {
                    Err(TryRecvError::Empty) => RunEnd::Fenced,
                    Ok(()) | Err(TryRecvError::Disconnected) => RunEnd::Finished,
                };
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
}

/// A manual run whose threads started: the answers of its watch and its job.
struct Started<T> {
    /// How the wait ended ([`wait_for_run`]); closed without an answer if
    /// the watch unwound.
    end: tauri::async_runtime::Receiver<RunEnd>,
    /// The job's result; closed without one if the job unwound.
    result: tauri::async_runtime::Receiver<T>,
}

/// Starts a manual run (F5-1 A, X2, X3) through `spawn`: first the watch,
/// which waits on [`WATCH_THREAD`] until the job ended or `fenced` (looked
/// at every `poll`), then the job on [`JOB_THREAD`]. Neither runs on Tauri's
/// blocking pool: the pool is shared with the rest of the app, a wait queued
/// there behind stuck jobs would never see the fence, and Tokio queues a
/// task before it makes the thread for it, so a thread it cannot make
/// leaves the task queued (still holding its slot) and panics. Nor is the
/// wait a timer on the async runtime: Tauri builds its Tokio without `time`.
///
/// The job holds `permit` until its thread really ends, returned or unwound,
/// not until the command returns, which after a fence is long before (X2);
/// its end of the watch's channel goes then too, which ends the wait. The
/// watch thread ends once the job ended or the claim was fenced, whether or
/// not the command still listens; a job that hangs is fenced within its
/// stall budget (the executor supervises its claim).
///
/// A thread that does not start is `Err` with its name, and leaves nothing
/// behind (X3): no job runs, no watch is left waiting, the slot is free. The
/// slot and the end of the watch's channel are handed to the job when its
/// thread starts, not with the task, so they come back even from a spawner
/// that keeps the task it failed to start; such a task runs nothing if it
/// ever runs.
fn start_run<T: Send + 'static>(
    spawn: impl Fn(&'static str, Task) -> io::Result<()>,
    permit: JobPermit,
    job: impl FnOnce() -> T + Send + 'static,
    fenced: impl Fn() -> bool + Send + 'static,
    poll: Duration,
) -> Result<Started<T>, (&'static str, io::Error)> {
    let (done, finished) = mpsc::channel::<()>();
    let (tell_end, end) = tauri::async_runtime::channel(1);
    spawn(
        WATCH_THREAD,
        Box::new(move || {
            let _ = tell_end.blocking_send(wait_for_run(&finished, fenced, poll));
        }),
    )
    .map_err(|e| (WATCH_THREAD, e))?;
    let handover = Arc::new(Mutex::new(Some((permit, done))));
    let (tell, result) = tauri::async_runtime::channel(1);
    let task: Task = {
        let handover = Arc::clone(&handover);
        Box::new(move || {
            let taken = handover
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
            let Some((permit, done)) = taken else {
                return;
            };
            let output = job();
            drop(permit);
            drop(done);
            let _ = tell.blocking_send(output);
        })
    };
    if let Err(e) = spawn(JOB_THREAD, task) {
        // The slot comes back, and the watch ends (`done` dropped).
        drop(
            handover
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take(),
        );
        return Err((JOB_THREAD, e));
    }
    Ok(Started { end, result })
}

/// What became of a manual run, for [`conclude`].
enum Joined<T> {
    /// The job ended: its result, or why there is none (the job unwound, or
    /// the wait for it failed).
    Ended(tauri::Result<T>),
    /// The claim was fenced while the job went on.
    Fenced,
    /// A thread of the run did not start (X3), its name and the OS error:
    /// nothing ran.
    NotStarted(&'static str, io::Error),
}

/// The command's side of a started run (F5-1 A): joins the job, but waits
/// no longer than its claim stays unfenced. Fenced, the job is left to
/// itself, not waited for: its thread may never return, and if it does it
/// sends nothing, writes no history, publishes nothing and touches no newer
/// claim (the executor's epoch and claim checks).
async fn join_unless_fenced<T>(started: Started<T>) -> Joined<T> {
    let Started {
        mut end,
        mut result,
    } = started;
    match end.recv().await {
        Some(RunEnd::Finished) => Joined::Ended(
            result
                .recv()
                .await
                .ok_or_else(|| io::Error::other("the manual run's job unwound").into()),
        ),
        Some(RunEnd::Fenced) => Joined::Fenced,
        // The wait itself failed; the job is not waited for either.
        None => Joined::Ended(Err(io::Error::other(
            "the manual run's watch ended without an answer",
        )
        .into())),
    }
}

/// A manual run from its start to its answer (F5-1 A, X2, X3): starts it
/// ([`start_run`]) and joins it ([`join_unless_fenced`]), then runs
/// `restore` (the claim given back, the state announced, the window shown)
/// exactly once, however that went: ended, fenced or not started.
async fn run_job<T: Send + 'static>(
    spawn: impl Fn(&'static str, Task) -> io::Result<()>,
    permit: JobPermit,
    job: impl FnOnce() -> T + Send + 'static,
    fenced: impl Fn() -> bool + Send + 'static,
    poll: Duration,
    restore: impl FnOnce(),
) -> Joined<T> {
    let joined = match start_run(spawn, permit, job, fenced, poll) {
        Ok(started) => join_unless_fenced(started).await,
        Err((thread, e)) => Joined::NotStarted(thread, e),
    };
    restore();
    joined
}

/// The command's answer for a manual run (F5-1 A, E, H, X3), given what
/// [`run_job`] got and the supervisor's health. Fenced or refused by the
/// send exclusion: `EngineUnavailable`. A failed run, or one whose thread
/// did not start: `Internal`, logged by its reason code or the OS error's
/// kind and code, and its trace id. A job that unwound, or a wait that ended
/// without an answer: `Internal` through [`CommandError::task_failed`]
/// (F2-1), which logs a fixed class and never the error's text. Nothing the
/// run's text says reaches the log or the frontend.
fn conclude(
    joined: Joined<Result<ExecutionResult, RunError>>,
    health: HealthState,
) -> Result<ExecutionResult, CommandError> {
    match joined {
        Joined::Ended(Ok(Ok(result))) => Ok(result),
        Joined::Fenced | Joined::Ended(Ok(Err(RunError::Fenced))) => {
            Err(stalled(health, ReasonCode::ExecutorTimeout))
        }
        Joined::Ended(Ok(Err(RunError::Invalid(diagnostics)))) => {
            Err(CommandError::validation(diagnostics))
        }
        Joined::Ended(Ok(Err(RunError::Failed(failure))))
            if failure.reason == ReasonCode::ExecutorTimeout =>
        {
            Err(stalled(health, failure.reason))
        }
        Joined::Ended(Ok(Err(RunError::Failed(failure)))) => Err(run_failed(failure)),
        Joined::NotStarted(thread, error) => Err(not_started(thread, &error)),
        Joined::Ended(Err(e)) => Err(CommandError::task_failed(e)),
    }
}

/// The run was fenced (it stalled past its budget) or refused by the send
/// exclusion: the executor is down until the supervisor rebuilds it. The
/// health is where the supervisor is heading: its state when that already
/// refuses a resume, `Recovering` while it still reads Healthy or Degraded
/// (its next tick publishes the executor down).
fn stalled(health: HealthState, reason: ReasonCode) -> CommandError {
    let health = if health.accepts_resume() {
        HealthState::Recovering
    } else {
        health
    };
    tracing::warn!(
        event = "command.execute_failed",
        category = "stalled",
        reason = ?reason,
        ?health,
        "manual run stopped: the executor is down"
    );
    CommandError::EngineUnavailable { health }
}

/// A manual run that failed otherwise: `Internal` with a fresh trace id,
/// logged with the run's reason code (H: never its text).
fn run_failed(failure: ManualFailure) -> CommandError {
    let trace_id = uuid::Uuid::new_v4().to_string();
    tracing::error!(
        event = "command.execute_failed",
        category = "run_failed",
        reason = ?failure.reason,
        trace_id = %trace_id,
        "manual run failed"
    );
    CommandError::Internal { trace_id }
}

/// A thread of the run did not start (X3): nothing ran, nothing was
/// recorded. `Internal` with a fresh trace id, logged with the thread's name
/// and the OS error's kind and code only.
fn not_started(thread: &'static str, error: &io::Error) -> CommandError {
    let trace_id = uuid::Uuid::new_v4().to_string();
    tracing::error!(
        event = "command.execute_failed",
        category = "spawn_failed",
        thread,
        kind = ?error.kind(),
        os_code = ?error.raw_os_error(),
        trace_id = %trace_id,
        "manual run not started: a thread did not start"
    );
    CommandError::Internal { trace_id }
}
/// Opens a recording session. Detection pauses through the `Recording`
/// reason before the session opens and for as long as it stays open.
#[tauri::command]
pub fn start_recording(store: State<Managed>) -> Result<(), CommandError> {
    let (engine, runtime) = {
        let s = store.lock();
        if s.engine.busy.load(Ordering::SeqCst) {
            return Err(CommandError::Busy);
        }
        (s.engine.clone(), s.runtime.clone())
    };
    runtime.set_pause(PauseReasons::RECORDING, true);
    let started = quicker_platform::start_recording(&engine).map_err(CommandError::internal);
    sync_recording_reason(&runtime, &engine);
    started
}
/// Closes the recording session and returns what it captured. The
/// `Recording` reason clears with it, so the engine goes back to what the
/// other reasons say (a manual pause from before the recording still holds).
#[tauri::command]
pub fn stop_recording(store: State<Managed>) -> Result<ActionPlan, CommandError> {
    let (engine, runtime) = {
        let s = store.lock();
        (s.engine.clone(), s.runtime.clone())
    };
    // Windows stops and takes the recording in one critical section, so no
    // hook event lands between the two (M2).
    #[cfg(windows)]
    let plan = quicker_platform::finish_and_take(&engine);
    #[cfg(not(windows))]
    let plan = quicker_platform::stop_recording(&engine);
    sync_recording_reason(&runtime, &engine);
    plan.map_err(CommandError::internal)
}
/// `Recording` is held exactly while a recording session is open. Read
/// after every start or stop, whatever its outcome, so a failed start does
/// not leave the reason behind and a failed stop does not drop it early.
/// The recording lock is released before the runtime is touched.
fn sync_recording_reason(runtime: &Runtime, engine: &Shared) {
    let active = engine
        .recording
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .is_some();
    runtime.set_pause(PauseReasons::RECORDING, active);
}

#[cfg(test)]
mod tests {
    use super::*;
    use quicker_platform::traits::SystemClock;
    use std::{
        sync::{
            atomic::{AtomicBool, AtomicUsize},
            Arc,
        },
        time::Instant,
    };

    #[test]
    fn the_recording_reason_follows_the_open_session() {
        let runtime = Runtime::new(Arc::new(SystemClock));
        let engine = Shared::new();
        runtime.attach(&engine);
        runtime.set_pause(PauseReasons::MANUAL, false);
        // A start that failed leaves no reason behind.
        runtime.set_pause(PauseReasons::RECORDING, true);
        sync_recording_reason(&runtime, &engine);
        assert!(!runtime.pause_reasons().contains(PauseReasons::RECORDING));
        // An open session holds it, and the engine stays paused even with
        // MANUAL clear.
        *engine.recording.lock().unwrap() = Some(vec![]);
        sync_recording_reason(&runtime, &engine);
        assert_eq!(runtime.pause_reasons(), PauseReasons::RECORDING);
        assert!(engine.paused.load(Ordering::SeqCst));
        // Closing it clears only RECORDING.
        runtime.set_pause(PauseReasons::MANUAL, true);
        engine.recording.lock().unwrap().take();
        sync_recording_reason(&runtime, &engine);
        assert_eq!(runtime.pause_reasons(), PauseReasons::MANUAL);
    }

    fn succeeded() -> ExecutionResult {
        ExecutionResult {
            id: "run".into(),
            status: "completed".into(),
            message: String::new(),
            completed_steps: 1,
            side_effects: true,
        }
    }

    /// A slot of its own, for a run whose test is not about the cap.
    fn slot() -> JobPermit {
        let slots: &'static JobSlots = Box::leak(Box::new(JobSlots::new(1)));
        slots.take().expect("a fresh slot")
    }

    /// F5-1 A: a job that hangs after its claim was fenced does not hold
    /// the command. It returns within a poll of the fence, restores the
    /// window once, and maps to `engine_unavailable`; the hung thread is
    /// left behind and its late result is never seen.
    #[test]
    fn a_fenced_run_returns_at_once_and_restores_the_window_while_its_thread_hangs() {
        let fenced = Arc::new(AtomicBool::new(false));
        let (wake, hang) = mpsc::channel::<()>();
        let fence = {
            let fenced = Arc::clone(&fenced);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(200));
                fenced.store(true, Ordering::SeqCst);
                Instant::now()
            })
        };
        let restored = AtomicUsize::new(0);
        let joined = tauri::async_runtime::block_on(run_job(
            spawn_thread,
            slot(),
            move || {
                // Hangs well past the fence; the timeout only keeps a broken
                // race from hanging the test run.
                let _ = hang.recv_timeout(Duration::from_secs(5));
                Ok::<_, RunError>(succeeded())
            },
            {
                let fenced = Arc::clone(&fenced);
                move || fenced.load(Ordering::SeqCst)
            },
            FENCE_POLL,
            || {
                restored.fetch_add(1, Ordering::SeqCst);
            },
        ));
        let returned = Instant::now();
        let fenced_at = fence.join().unwrap();
        assert!(
            matches!(joined, Joined::Fenced),
            "the hung job is not waited for"
        );
        assert!(
            returned.duration_since(fenced_at) < Duration::from_secs(1),
            "returned {:?} after the fence",
            returned.duration_since(fenced_at)
        );
        assert_eq!(restored.load(Ordering::SeqCst), 1, "the window is restored");
        // Typed: the frontend's `engine_unavailable`, with the health the
        // supervisor is heading to.
        assert!(matches!(
            conclude(Joined::Fenced, HealthState::Healthy),
            Err(CommandError::EngineUnavailable {
                health: HealthState::Recovering
            })
        ));
        assert!(matches!(
            conclude(Joined::Fenced, HealthState::Failed),
            Err(CommandError::EngineUnavailable {
                health: HealthState::Failed
            })
        ));
        let _ = wake.send(());
    }

    /// The other ends of the wait: a run that finishes is joined and its
    /// result returned; one that unwinds ends the wait too (its end of the
    /// channel drops) and is an internal error. Either way the window is
    /// restored once. A checkpoint that found the claim fenced, and a run
    /// refused as fenced or by the send exclusion, are `engine_unavailable`.
    #[test]
    fn a_finished_or_unwound_run_is_joined_and_restores_the_window() {
        let restored = AtomicUsize::new(0);
        let joined = tauri::async_runtime::block_on(run_job(
            spawn_thread,
            slot(),
            || Ok::<_, RunError>(succeeded()),
            || false,
            FENCE_POLL,
            || {
                restored.fetch_add(1, Ordering::SeqCst);
            },
        ));
        assert_eq!(restored.load(Ordering::SeqCst), 1);
        let result = conclude(joined, HealthState::Healthy).expect("the run succeeded");
        assert_eq!(result.id, "run");

        let joined = tauri::async_runtime::block_on(run_job(
            spawn_thread,
            slot(),
            || {
                if true {
                    panic!("the job unwinds");
                }
                Ok::<_, RunError>(succeeded())
            },
            || false,
            FENCE_POLL,
            || {
                restored.fetch_add(1, Ordering::SeqCst);
            },
        ));
        assert_eq!(restored.load(Ordering::SeqCst), 2);
        assert!(
            matches!(joined, Joined::Ended(Err(_))),
            "the join reports the unwind"
        );
        assert!(matches!(
            conclude(joined, HealthState::Healthy),
            Err(CommandError::Internal { .. })
        ));

        for refused in [
            Err(RunError::Fenced),
            Err(RunError::Failed(ManualFailure {
                reason: ReasonCode::ExecutorTimeout,
            })),
        ] {
            assert!(matches!(
                conclude(Joined::Ended(Ok(refused)), HealthState::Degraded),
                Err(CommandError::EngineUnavailable {
                    health: HealthState::Recovering
                })
            ));
        }
    }

    /// F5-1 H: a manual run whose rule carries a private value (an unknown
    /// common function, missing image and file paths, an unknown key name)
    /// fails preflight in the platform, which hands back only the reason
    /// code (`ManualFailure` has no text; the platform's
    /// `a_manual_runs_failure_never_echoes_the_rules_values` checks the
    /// run's own text and history). The mapping logs the category, the
    /// reason and the trace id, and the frontend gets the code and the
    /// trace id: the value is in neither.
    #[test]
    fn a_failed_run_reaches_the_log_and_the_frontend_by_its_reason_code_only() {
        use crate::diagnostics::logging::{test_support, DEFAULT_LEVEL};
        const SENTINEL: &str = "SENTINEL-private-7d41";
        let missing = std::env::temp_dir()
            .join(format!("qt-missing-{SENTINEL}"))
            .to_string_lossy()
            .into_owned();
        let plan = ActionPlan {
            steps: [
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
            ]
            .into_iter()
            .map(|operation| Step::new(operation, SourceSpan::default()))
            .collect(),
            ..Default::default()
        };
        // Stands in for `ManualTicket::execute` (no real input in tests):
        // what the platform returns for this plan.
        let execute = |plan: ActionPlan| -> Result<ExecutionResult, ManualFailure> {
            assert!(serde_json::to_string(&plan).unwrap().contains(SENTINEL));
            Err(ManualFailure {
                reason: ReasonCode::PreflightFailed,
            })
        };
        let dir = std::env::temp_dir().join(format!("quicker-exec-log-{}", uuid::Uuid::new_v4()));
        let built = test_support::scoped(&dir, DEFAULT_LEVEL);
        let error = tracing::subscriber::with_default(built.subscriber, || {
            let joined = Joined::Ended(Ok(execute(plan).map_err(RunError::Failed)));
            conclude(joined, HealthState::Healthy).expect_err("the run failed")
        });
        drop(built.guard);
        let CommandError::Internal { trace_id } = &error else {
            panic!("a failed run is an internal error, got {error:?}");
        };
        let shown = serde_json::to_string(&error).unwrap();
        assert!(!shown.contains(SENTINEL), "{shown}");
        assert_eq!(
            shown,
            format!(r#"{{"code":"internal","detail":{{"trace_id":"{trace_id}"}}}}"#)
        );
        let log = test_support::read_logs(&dir);
        assert!(!log.contains(SENTINEL), "{log}");
        let line = log
            .lines()
            .find(|line| line.contains(trace_id.as_str()))
            .unwrap_or_else(|| panic!("trace id {trace_id} missing from log:\n{log}"));
        assert!(line.contains("event=\"command.execute_failed\""), "{line}");
        assert!(line.contains("category=\"run_failed\""), "{line}");
        assert!(line.contains("reason=PreflightFailed"), "{line}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// F2-1 at the manual run's join: a job that unwound, or a wait that
    /// ended without an answer, goes through `CommandError::task_failed`,
    /// which logs `command.internal` with a fixed task class and the trace
    /// id, never the error's text.
    #[test]
    fn a_run_without_an_answer_is_logged_by_task_failed_without_its_text() {
        use crate::diagnostics::logging::{test_support, DEFAULT_LEVEL};
        const SENTINEL: &str = "SENTINEL-join-text-3b52";
        let dir = std::env::temp_dir().join(format!("quicker-exec-join-{}", uuid::Uuid::new_v4()));
        let built = test_support::scoped(&dir, DEFAULT_LEVEL);
        let error = tracing::subscriber::with_default(built.subscriber, || {
            let joined = Joined::Ended(Err(io::Error::other(SENTINEL).into()));
            conclude(joined, HealthState::Healthy).expect_err("the run gave no answer")
        });
        drop(built.guard);
        let CommandError::Internal { trace_id } = &error else {
            panic!("a run without an answer is an internal error, got {error:?}");
        };
        let shown = serde_json::to_string(&error).unwrap();
        assert!(!shown.contains(SENTINEL), "{shown}");
        let log = test_support::read_logs(&dir);
        assert!(!log.contains(SENTINEL), "{log}");
        let line = log
            .lines()
            .find(|line| line.contains(trace_id.as_str()))
            .unwrap_or_else(|| panic!("trace id {trace_id} missing from log:\n{log}"));
        assert!(line.contains("event=\"command.internal\""), "{line}");
        assert!(line.contains("task=\"failed\""), "{line}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Sets its flag when dropped: the watch thread drops its `fenced`
    /// closure, and this with it, when it is done waiting.
    struct DropFlag(Arc<AtomicBool>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    /// Waits up to `limit` for `flag`.
    fn becomes_set(flag: &AtomicBool, limit: Duration) -> bool {
        let start = Instant::now();
        while !flag.load(Ordering::SeqCst) {
            if start.elapsed() > limit {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        true
    }

    /// Notes in `misplaced` a look made on a thread of Tauri's runtime (its
    /// blocking pool), or on any thread but the one named `expected`.
    fn note_misplaced(misplaced: &Mutex<Vec<String>>, expected: &str) {
        let thread = std::thread::current();
        let on_runtime = tauri::async_runtime::TokioHandle::try_current().is_ok();
        if on_runtime || thread.name() != Some(expected) {
            misplaced.lock().unwrap().push(format!(
                "{expected} ran on {:?} (on Tauri's runtime: {on_runtime})",
                thread.name()
            ));
        }
    }

    /// F5-1 X2, X3: neither the job nor the wait for the fence takes a
    /// thread of Tauri's blocking pool, which stuck jobs could hold in full.
    /// Each runs on a thread of its own, named, off Tauri's runtime. (Before
    /// X3 the job ran on the pool, and this test stood in for one queued
    /// behind a full pool by a job that never started; now a job starts at
    /// once or not at all, so a real one hangs here.) The command still
    /// returns within a poll of the fence while the job hangs, restores
    /// once, and the watch thread ends once it answered.
    #[test]
    fn the_fence_is_seen_off_the_blocking_pool_and_its_watch_thread_ends() {
        let fenced = Arc::new(AtomicBool::new(false));
        let misplaced = Arc::new(Mutex::new(Vec::new()));
        let ended = Arc::new(AtomicBool::new(false));
        let (wake, hang) = mpsc::channel::<()>();
        let (tell_started, started) = mpsc::channel::<()>();
        let fence = {
            let fenced = Arc::clone(&fenced);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(200));
                fenced.store(true, Ordering::SeqCst);
                Instant::now()
            })
        };
        let restored = AtomicUsize::new(0);
        let joined = tauri::async_runtime::block_on(run_job(
            spawn_thread,
            slot(),
            {
                let misplaced = Arc::clone(&misplaced);
                move || {
                    note_misplaced(&misplaced, JOB_THREAD);
                    let _ = tell_started.send(());
                    // Hangs well past the fence; the timeout only keeps a
                    // broken race from hanging the test run.
                    let _ = hang.recv_timeout(Duration::from_secs(5));
                }
            },
            {
                let fenced = Arc::clone(&fenced);
                let misplaced = Arc::clone(&misplaced);
                let ends = DropFlag(Arc::clone(&ended));
                move || {
                    let _ = &ends;
                    note_misplaced(&misplaced, WATCH_THREAD);
                    fenced.load(Ordering::SeqCst)
                }
            },
            FENCE_POLL,
            || {
                restored.fetch_add(1, Ordering::SeqCst);
            },
        ));
        let returned = Instant::now();
        let fenced_at = fence.join().unwrap();
        started
            .recv_timeout(Duration::from_secs(1))
            .expect("the job started");
        assert!(
            matches!(joined, Joined::Fenced),
            "the hung job is not waited for"
        );
        assert!(
            returned.duration_since(fenced_at) < Duration::from_secs(1),
            "returned {:?} after the fence",
            returned.duration_since(fenced_at)
        );
        assert_eq!(restored.load(Ordering::SeqCst), 1, "restored once");
        assert!(
            becomes_set(&ended, Duration::from_secs(1)),
            "the watch thread is still waiting after it answered"
        );
        assert_eq!(*misplaced.lock().unwrap(), Vec::<String>::new());
        let _ = wake.send(());

        // A job that ended is joined, and its watch ends too.
        let ended = Arc::new(AtomicBool::new(false));
        let joined = tauri::async_runtime::block_on(run_job(
            spawn_thread,
            slot(),
            {
                let misplaced = Arc::clone(&misplaced);
                move || {
                    note_misplaced(&misplaced, JOB_THREAD);
                    7
                }
            },
            {
                let ends = DropFlag(Arc::clone(&ended));
                move || {
                    let _ = &ends;
                    false
                }
            },
            FENCE_POLL,
            || {
                restored.fetch_add(1, Ordering::SeqCst);
            },
        ));
        assert!(matches!(joined, Joined::Ended(Ok(7))));
        assert_eq!(restored.load(Ordering::SeqCst), 2);
        assert!(becomes_set(&ended, Duration::from_secs(1)));
        assert_eq!(*misplaced.lock().unwrap(), Vec::<String>::new());
    }

    /// F5-1 X2, X3: manual jobs that have not really ended are capped. A
    /// job holds its slot until its thread returns, not until its command
    /// does: at the cap the next one is refused, a job its command gave up on
    /// (no longer joined, as after a fence) still counts, and once one really
    /// ends a job can run again.
    #[test]
    fn manual_jobs_that_have_not_ended_are_capped_until_one_really_ends() {
        let slots: &'static JobSlots = Box::leak(Box::new(JobSlots::new(MAX_MANUAL_JOBS)));
        let mut gates = Vec::new();
        let mut runs = Vec::new();
        for _ in 0..MAX_MANUAL_JOBS {
            let permit = slots.take().expect("under the cap");
            let (open, gate) = mpsc::channel::<()>();
            gates.push(open);
            let started = start_run(
                spawn_thread,
                permit,
                move || {
                    // The timeout only keeps a broken test from hanging the run.
                    let _ = gate.recv_timeout(Duration::from_secs(10));
                },
                || false,
                FENCE_POLL,
            );
            runs.push(started.unwrap_or_else(|(thread, e)| panic!("{thread}: {e}")));
        }
        assert!(slots.take().is_none(), "a job past the cap was admitted");
        // The command stops waiting for a stuck job: its slot stays taken.
        drop(runs.pop());
        assert!(slots.take().is_none(), "a run given up on freed its slot");
        // The first job really ends: a job can run again.
        gates[0].send(()).unwrap();
        let first = tauri::async_runtime::block_on(join_unless_fenced(runs.remove(0)));
        assert!(matches!(first, Joined::Ended(Ok(()))));
        let permit = slots.take().expect("a slot is free once a job ended");
        let again = tauri::async_runtime::block_on(run_job(
            spawn_thread,
            permit,
            || 7,
            || false,
            FENCE_POLL,
            || {},
        ));
        assert!(matches!(again, Joined::Ended(Ok(7))));
        for open in &gates {
            let _ = open.send(());
        }
        for run in runs {
            let joined = tauri::async_runtime::block_on(join_unless_fenced(run));
            assert!(matches!(joined, Joined::Ended(Ok(()))));
        }
        // The job whose handle was dropped gives its slot back as it ends.
        let start = Instant::now();
        while slots.live.load(Ordering::SeqCst) != 0 {
            assert!(start.elapsed() < Duration::from_secs(5), "a slot leaked");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// F5-1 C, X2: a run is admitted with `busy` and a slot, or with
    /// neither. While earlier jobs hold slots short of the cap a run is
    /// still admitted (a stuck job does not stop the next runs); at the cap
    /// it is refused before anything starts, with `busy` given back, as
    /// `busy` while the supervisor accepts a resume and `engine_unavailable`
    /// otherwise, logged without content.
    #[test]
    fn a_run_is_admitted_under_the_cap_and_refused_at_it_without_holding_busy() {
        use crate::diagnostics::logging::{test_support, DEFAULT_LEVEL};
        let slots: &'static JobSlots = Box::leak(Box::new(JobSlots::new(MAX_MANUAL_JOBS)));
        let engine = Shared::new();
        let stuck: Vec<_> = (1..MAX_MANUAL_JOBS)
            .map(|_| slots.take().expect("under the cap"))
            .collect();
        let (busy, permit) =
            admit(&engine, slots, || unreachable!("health read under the cap")).expect("admitted");
        // `busy` held: refused as busy, and no slot is taken for it.
        assert!(matches!(
            admit(&engine, slots, || HealthState::Healthy),
            Err(CommandError::Busy)
        ));
        drop(permit);
        assert!(matches!(
            admit(&engine, slots, || HealthState::Healthy),
            Err(CommandError::Busy)
        ));
        drop(busy);

        let dir = std::env::temp_dir().join(format!("quicker-exec-cap-{}", uuid::Uuid::new_v4()));
        let built = test_support::scoped(&dir, DEFAULT_LEVEL);
        let extra = slots.take().expect("the run's slot was given back");
        let (healthy, failed) = tracing::subscriber::with_default(built.subscriber, || {
            (
                admit(&engine, slots, || HealthState::Healthy).map(|_| ()),
                admit(&engine, slots, || HealthState::Failed).map(|_| ()),
            )
        });
        drop(built.guard);
        assert!(matches!(healthy, Err(CommandError::Busy)), "{healthy:?}");
        assert!(
            matches!(
                failed,
                Err(CommandError::EngineUnavailable {
                    health: HealthState::Failed
                })
            ),
            "{failed:?}"
        );
        assert!(
            !engine.busy.load(Ordering::SeqCst),
            "a refused run kept busy"
        );
        let log = test_support::read_logs(&dir);
        let refused: Vec<_> = log
            .lines()
            .filter(|line| line.contains("event=\"command.execute_refused\""))
            .collect();
        assert_eq!(refused.len(), 2, "{log}");
        for line in refused {
            assert!(line.contains("reason=\"jobs_exhausted\""), "{line}");
            assert!(line.contains(&format!("limit={MAX_MANUAL_JOBS}")), "{line}");
        }
        std::fs::remove_dir_all(dir).unwrap();

        // A stuck job ends: a run is admitted again.
        drop(stuck);
        drop(extra);
        let admitted = admit(&engine, slots, || HealthState::Healthy);
        assert!(admitted.is_ok());
    }

    /// A spawner whose `fails` thread does not start, as when the OS has no
    /// thread to give: it keeps the task, as Tokio's blocking pool keeps one
    /// it queued before it failed to make its thread, and answers the OS's
    /// error. Other threads start for real.
    struct NoThread {
        fails: &'static str,
        kept: Mutex<Vec<Task>>,
    }

    impl NoThread {
        fn spawn(&self, name: &'static str, task: Task) -> io::Result<()> {
            if name != self.fails {
                return spawn_thread(name, task);
            }
            self.kept.lock().unwrap().push(task);
            // ERROR_NOT_ENOUGH_MEMORY, as `CreateThread` fails for want of a
            // stack.
            Err(io::Error::from_raw_os_error(8))
        }
    }

    /// F5-1 X3: a run whose job thread or watch thread does not start
    /// leaves nothing behind. The command restores exactly once, `busy` and
    /// the slot come back (the next run is admitted), the job never runs, so
    /// no history entry is written (only the job's run writes one), and no
    /// watch is left waiting. The answer is `Internal` with a trace id,
    /// logged as `spawn_failed` with the thread and the OS error's kind and
    /// code only. A task the spawner kept runs nothing when it runs later.
    #[test]
    fn a_run_whose_thread_does_not_start_restores_once_and_frees_busy_and_its_slot() {
        use crate::diagnostics::logging::{test_support, DEFAULT_LEVEL};
        for fails in [JOB_THREAD, WATCH_THREAD] {
            let slots: &'static JobSlots = Box::leak(Box::new(JobSlots::new(1)));
            let engine = Shared::new();
            let (busy, permit) =
                admit(&engine, slots, || unreachable!("health read under the cap"))
                    .expect("admitted");
            let spawner = NoThread {
                fails,
                kept: Mutex::new(Vec::new()),
            };
            let ran = Arc::new(AtomicBool::new(false));
            let ended = Arc::new(AtomicBool::new(false));
            let restored = AtomicUsize::new(0);
            let dir =
                std::env::temp_dir().join(format!("quicker-exec-spawn-{}", uuid::Uuid::new_v4()));
            let built = test_support::scoped(&dir, DEFAULT_LEVEL);
            let answer = tracing::subscriber::with_default(built.subscriber, || {
                let joined = tauri::async_runtime::block_on(run_job(
                    |name, task| spawner.spawn(name, task),
                    permit,
                    {
                        let ran = Arc::clone(&ran);
                        move || {
                            ran.store(true, Ordering::SeqCst);
                            Ok::<_, RunError>(succeeded())
                        }
                    },
                    {
                        let ends = DropFlag(Arc::clone(&ended));
                        move || {
                            let _ = &ends;
                            false
                        }
                    },
                    FENCE_POLL,
                    || {
                        drop(busy);
                        restored.fetch_add(1, Ordering::SeqCst);
                    },
                ));
                assert!(
                    matches!(joined, Joined::NotStarted(thread, _) if thread == fails),
                    "{fails}: the run did not end as not started"
                );
                conclude(joined, HealthState::Healthy)
            });
            drop(built.guard);
            assert_eq!(restored.load(Ordering::SeqCst), 1, "{fails}: restored once");
            assert!(!engine.busy.load(Ordering::SeqCst), "{fails}: busy kept");
            let trace_id = match answer {
                Err(CommandError::Internal { trace_id }) => trace_id,
                other => panic!("{fails}: {other:?}"),
            };
            let again = admit(&engine, slots, || HealthState::Healthy);
            assert!(again.is_ok(), "{fails}: the next run was refused");
            drop(again);
            // What the spawner kept runs nothing, and then no watch waits.
            for task in spawner.kept.into_inner().unwrap() {
                task();
            }
            assert!(!ran.load(Ordering::SeqCst), "{fails}: the job ran");
            assert!(
                becomes_set(&ended, Duration::from_secs(1)),
                "{fails}: a watch is still waiting"
            );
            let log = test_support::read_logs(&dir);
            let lines: Vec<_> = log
                .lines()
                .filter(|line| line.contains("category=\"spawn_failed\""))
                .collect();
            assert_eq!(lines.len(), 1, "{log}");
            let line = lines[0];
            assert!(line.contains(&trace_id), "{line}");
            assert!(line.contains("event=\"command.execute_failed\""), "{line}");
            assert!(line.contains(&format!("thread=\"{fails}\"")), "{line}");
            assert!(line.contains("kind="), "{line}");
            assert!(line.contains("os_code=Some(8)"), "{line}");
            std::fs::remove_dir_all(dir).unwrap();
        }
    }
}
