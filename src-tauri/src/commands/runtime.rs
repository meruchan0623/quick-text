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
    fmt::Display,
    future::Future,
    io,
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc::{self, RecvTimeoutError, TryRecvError},
        Arc, PoisonError,
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
    // The claim stays here and the blocking thread acts with tickets that
    // free nothing (F5-1 C): a thread that never comes back cannot hold or
    // free `busy`; dropping the claim below is the only release.
    let ticket = busy.ticket();
    let watch = busy.ticket();
    let (done, finished) = mpsc::channel::<()>();
    // The job holds the run's slot until its thread really returns (X2).
    let job = spawn_job(permit, move || {
        // Dropped when the job ends, returned or unwound, which ends the
        // command's wait.
        let _done = done;
        // Each checkpoint restarts the claim's stall budget, and stops the
        // run, sending nothing, once the claim was fenced (F5-1 B).
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
    });
    let joined = join_unless_fenced(
        job,
        finished,
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

/// At most this many manual jobs that have not really ended (F5-1 X2). A
/// fenced job is left to itself and may hold its blocking thread for good (a
/// system call that never returns), and Tauri's blocking pool is shared with
/// the rest of the app: each job keeps its slot until its thread returns, so
/// stuck jobs cannot pile up there. Four: the live run plus room for three
/// stuck ones, so a stuck job does not stop the next runs (each stuck job
/// already cost a fence and a rebuild); a desktop that strands more than
/// that needs the user's attention, not more threads.
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

/// Runs a manual run's job on Tauri's blocking pool, holding `permit` until
/// the job's thread returns or unwinds (F5-1 X2): not until the command
/// returns, which after a fence is long before.
fn spawn_job<T: Send + 'static>(
    permit: JobPermit,
    job: impl FnOnce() -> T + Send + 'static,
) -> tauri::async_runtime::JoinHandle<T> {
    tauri::async_runtime::spawn_blocking(move || {
        let _permit = permit;
        job()
    })
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

/// Why the blocking job of a manual run ended without a result.
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

/// Runs [`wait_for_run`] on a thread of its own and hands its answer to the
/// command (F5-1 X2). Not on Tauri's blocking pool: the jobs it watches run
/// there and may hold every thread of it, and a wait queued behind them
/// would never see the fence. Not a timer on the async runtime either: Tauri
/// builds its Tokio without `time`. The thread ends as soon as the job ended
/// or its claim was fenced, whether or not the command still listens; a job
/// that hangs is fenced within its stall budget (the executor supervises its
/// claim), so the thread does not live on with it.
async fn watch_run(
    finished: mpsc::Receiver<()>,
    fenced: impl Fn() -> bool + Send + 'static,
    poll: Duration,
) -> tauri::Result<RunEnd> {
    let (tell, mut told) = tauri::async_runtime::channel(1);
    std::thread::Builder::new()
        .name("qt-manual-watch".into())
        .spawn(move || {
            let _ = tell.blocking_send(wait_for_run(&finished, fenced, poll));
        })?;
    told.recv()
        .await
        .ok_or_else(|| io::Error::other("the manual run's watch ended without an answer").into())
}

/// The command's side of a manual run (F5-1 A): joins the job, but waits no
/// longer than its claim stays unfenced (`fenced`, looked at every `poll`),
/// then runs `restore` (the claim given back, the state announced, the
/// window shown) once, whichever came first. `None`: the claim was fenced
/// while the job went on. The job is left to itself, not waited for: its
/// thread may never return, and if it does it sends nothing, writes no
/// history, publishes nothing and touches no newer claim (the executor's
/// epoch and claim checks). The wait runs off the blocking pool
/// ([`watch_run`], X2), so it needs no pool thread to see the fence.
async fn join_unless_fenced<T>(
    job: impl Future<Output = tauri::Result<T>>,
    finished: mpsc::Receiver<()>,
    fenced: impl Fn() -> bool + Send + 'static,
    poll: Duration,
    restore: impl FnOnce(),
) -> Option<tauri::Result<T>> {
    let end = watch_run(finished, fenced, poll).await;
    let joined = match end {
        Ok(RunEnd::Finished) => Some(job.await),
        Ok(RunEnd::Fenced) => None,
        // The wait itself failed; the job is not waited for either.
        Err(e) => Some(Err(e)),
    };
    restore();
    joined
}

/// The command's answer for a manual run (F5-1 A, E, H), given what
/// [`join_unless_fenced`] got and the supervisor's health. Fenced or
/// refused by the send exclusion: `EngineUnavailable`. A failed run:
/// `Internal`, logged by its reason code and trace id only. Nothing the
/// run's text says reaches the log or the frontend.
fn conclude<E: Display>(
    joined: Option<Result<Result<ExecutionResult, RunError>, E>>,
    health: HealthState,
) -> Result<ExecutionResult, CommandError> {
    match joined {
        Some(Ok(Ok(result))) => Ok(result),
        None | Some(Ok(Err(RunError::Fenced))) => Err(stalled(health, ReasonCode::ExecutorTimeout)),
        Some(Ok(Err(RunError::Invalid(diagnostics)))) => Err(CommandError::validation(diagnostics)),
        Some(Ok(Err(RunError::Failed(failure))))
            if failure.reason == ReasonCode::ExecutorTimeout =>
        {
            Err(stalled(health, failure.reason))
        }
        Some(Ok(Err(RunError::Failed(failure)))) => Err(run_failed(failure)),
        // 集成时改用 CommandError::task_failed（F2-1）
        Some(Err(e)) => Err(CommandError::internal(e)),
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

    /// F5-1 A: a worker that hangs after its claim was fenced does not hold
    /// the command. It returns within a poll of the fence, restores the
    /// window once, and maps to `engine_unavailable`; the hung thread is
    /// left behind and its late result is never seen.
    #[test]
    fn a_fenced_run_returns_at_once_and_restores_the_window_while_its_thread_hangs() {
        let fenced = Arc::new(AtomicBool::new(false));
        let (wake, hang) = mpsc::channel::<()>();
        let (done, finished) = mpsc::channel::<()>();
        let job = tauri::async_runtime::spawn_blocking(move || {
            let _done = done;
            // Hangs well past the fence; the timeout only keeps a broken
            // race from hanging the test run.
            let _ = hang.recv_timeout(Duration::from_secs(5));
            Ok::<_, RunError>(succeeded())
        });
        let fence = {
            let fenced = Arc::clone(&fenced);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(200));
                fenced.store(true, Ordering::SeqCst);
                Instant::now()
            })
        };
        let restored = AtomicUsize::new(0);
        let joined = tauri::async_runtime::block_on(join_unless_fenced(
            job,
            finished,
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
        assert!(joined.is_none(), "the hung job is not waited for");
        assert!(
            returned.duration_since(fenced_at) < Duration::from_secs(1),
            "returned {:?} after the fence",
            returned.duration_since(fenced_at)
        );
        assert_eq!(restored.load(Ordering::SeqCst), 1, "the window is restored");
        // Typed: the frontend's `engine_unavailable`, with the health the
        // supervisor is heading to.
        assert!(matches!(
            conclude::<tauri::Error>(None, HealthState::Healthy),
            Err(CommandError::EngineUnavailable {
                health: HealthState::Recovering
            })
        ));
        assert!(matches!(
            conclude::<tauri::Error>(None, HealthState::Failed),
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
        let (done, finished) = mpsc::channel::<()>();
        let job = tauri::async_runtime::spawn_blocking(move || {
            let _done = done;
            Ok::<_, RunError>(succeeded())
        });
        let joined = tauri::async_runtime::block_on(join_unless_fenced(
            job,
            finished,
            || false,
            FENCE_POLL,
            || {
                restored.fetch_add(1, Ordering::SeqCst);
            },
        ));
        assert_eq!(restored.load(Ordering::SeqCst), 1);
        let result = conclude(joined, HealthState::Healthy).expect("the run succeeded");
        assert_eq!(result.id, "run");

        let (done, finished) = mpsc::channel::<()>();
        let job = tauri::async_runtime::spawn_blocking(move || {
            let _done = done;
            if true {
                panic!("the job unwinds");
            }
            Ok::<_, RunError>(succeeded())
        });
        let joined = tauri::async_runtime::block_on(join_unless_fenced(
            job,
            finished,
            || false,
            FENCE_POLL,
            || {
                restored.fetch_add(1, Ordering::SeqCst);
            },
        ));
        assert_eq!(restored.load(Ordering::SeqCst), 2);
        assert!(
            matches!(joined, Some(Err(_))),
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
                conclude::<tauri::Error>(Some(Ok(refused)), HealthState::Degraded),
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
            let joined = Some(Ok::<_, tauri::Error>(
                execute(plan).map_err(RunError::Failed),
            ));
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

    /// F5-1 X2: the wait for the fence needs no thread of Tauri's blocking
    /// pool, which the jobs it watches may hold in full. The job here never
    /// starts, as one queued behind a full pool would not (filling the real
    /// pool, shared by every test of this binary, would stall the others),
    /// and every look at the fence is made off Tauri's runtime: the command
    /// still returns within a poll of the fence, restores once, and the
    /// watch thread ends once it answered.
    #[test]
    fn the_fence_is_seen_off_the_blocking_pool_and_its_watch_thread_ends() {
        let fenced = Arc::new(AtomicBool::new(false));
        let on_runtime = Arc::new(AtomicBool::new(false));
        let ended = Arc::new(AtomicBool::new(false));
        // The job never started: its end of the channel stays open.
        let (_done, finished) = mpsc::channel::<()>();
        let fence = {
            let fenced = Arc::clone(&fenced);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(200));
                fenced.store(true, Ordering::SeqCst);
                Instant::now()
            })
        };
        let restored = AtomicUsize::new(0);
        let joined = tauri::async_runtime::block_on(join_unless_fenced(
            std::future::pending::<tauri::Result<()>>(),
            finished,
            {
                let fenced = Arc::clone(&fenced);
                let on_runtime = Arc::clone(&on_runtime);
                let ends = DropFlag(Arc::clone(&ended));
                move || {
                    let _ = &ends;
                    if tauri::async_runtime::TokioHandle::try_current().is_ok() {
                        on_runtime.store(true, Ordering::SeqCst);
                    }
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
        assert!(joined.is_none(), "the job is not waited for");
        assert!(
            returned.duration_since(fenced_at) < Duration::from_secs(1),
            "returned {:?} after the fence",
            returned.duration_since(fenced_at)
        );
        assert_eq!(restored.load(Ordering::SeqCst), 1, "restored once");
        assert!(
            !on_runtime.load(Ordering::SeqCst),
            "the fence was looked at on a thread of Tauri's runtime"
        );
        assert!(
            becomes_set(&ended, Duration::from_secs(1)),
            "the watch thread is still waiting after it answered"
        );

        // A job that ended is joined, and its watch ends too.
        let ended = Arc::new(AtomicBool::new(false));
        let (done, finished) = mpsc::channel::<()>();
        drop(done);
        let joined = tauri::async_runtime::block_on(join_unless_fenced(
            std::future::ready(Ok(7)),
            finished,
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
        assert!(matches!(joined, Some(Ok(7))));
        assert_eq!(restored.load(Ordering::SeqCst), 2);
        assert!(becomes_set(&ended, Duration::from_secs(1)));
    }

    /// F5-1 X2: manual jobs that have not really ended are capped. A job
    /// holds its slot until its thread returns, not until its command does:
    /// at the cap the next one is refused, a job its command gave up on (its
    /// handle dropped, as after a fence) still counts, and once one really
    /// ends a job can run again.
    #[test]
    fn manual_jobs_that_have_not_ended_are_capped_until_one_really_ends() {
        let slots: &'static JobSlots = Box::leak(Box::new(JobSlots::new(MAX_MANUAL_JOBS)));
        let mut gates = Vec::new();
        let mut jobs = Vec::new();
        for _ in 0..MAX_MANUAL_JOBS {
            let permit = slots.take().expect("under the cap");
            let (open, gate) = mpsc::channel::<()>();
            gates.push(open);
            jobs.push(spawn_job(permit, move || {
                // The timeout only keeps a broken test from hanging the run.
                let _ = gate.recv_timeout(Duration::from_secs(10));
            }));
        }
        assert!(slots.take().is_none(), "a job past the cap was admitted");
        // The command stops waiting for a stuck job: its slot stays taken.
        drop(jobs.pop());
        assert!(slots.take().is_none(), "a dropped handle freed its slot");
        // The first job really ends: a job can run again.
        gates[0].send(()).unwrap();
        tauri::async_runtime::block_on(jobs.remove(0)).unwrap();
        let permit = slots.take().expect("a slot is free once a job ended");
        let again = tauri::async_runtime::block_on(spawn_job(permit, || 7)).unwrap();
        assert_eq!(again, 7);
        for open in &gates {
            let _ = open.send(());
        }
        for job in jobs {
            tauri::async_runtime::block_on(job).unwrap();
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
}
