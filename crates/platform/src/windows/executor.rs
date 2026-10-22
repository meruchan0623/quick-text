//! Executor (B-01, S-01): one worker thread resolves matched candidates and
//! runs their plans, behind panic isolation, an epoch fence and a busy guard.
//!
//! * Every task runs inside `catch_unwind`. A panic is logged as
//!   `executor.panic` (stage and payload kind only, never the payload text),
//!   the run counts as failed and the worker keeps serving; the supervisor
//!   then sees the executor as down and swaps in a fresh worker.
//! * Epoch: a task records the executor epoch when it starts. Every input it
//!   sends goes through [`Fenced`], which re-checks the epoch first and
//!   refuses once it changed; the stamp guard (`actions::guarded_context`)
//!   checks it too, through [`checkpoint`]. The supervisor bumps the epoch
//!   when a task makes no progress for [`STALL_BUDGET`]
//!   ([`ExecutorCore::fence`]), so a hung thread that wakes up later sends
//!   nothing and quietly exits.
//! * Keys and mouse buttons pressed but not yet released are tracked in a
//!   256-bit atomic bitmap behind the small [`HeldKeys`] interface, so
//!   whoever ends the task (the busy guard on unwind, or the fence on a
//!   stall) sends the matching MARK key-ups. At merge the implementation is
//!   swapped for P1b's `keystate::OwnInput` (one source of truth).
//! * Clipboard: a paste task claims the clipboard for its lease
//!   ([`TaskScope::claim_clipboard`]). While a fenced (stalled) task still
//!   holds the claim, paste actions fail with `ClipboardBusy` and the
//!   clipboard component reports Degraded. They do NOT fall back to typing:
//!   plan B-01 (5) suggested that, but it contradicts no-regression #2
//!   ("paste never degrades to typing"), and the coordinator ruled for #2.
//!   Key output runs as usual.
use super::actions::{
    begin_trace, current_stage, execute_with_proof, set_stage, stage_marks, Ports, Stage,
    StageMarks,
};
use super::clipboard::WinClipboard;
use super::foreground::WinForeground;
use super::inject::{WinInjector, SENT_EFFECT};
use super::keystate::physical;
use super::*;
use crate::fault::{self, FaultPoint};
use crate::health::{Component, Outcome, ReasonCode, StageTimings, TriggerKind};
use crate::history::Record;
use crate::runtime::Runtime;
use crate::supervisor::{ExecutorControl, Status};
use crate::traits::{ClipboardBackend, Clock, Injector, SystemClock};
use arc_swap::ArcSwapOption;
use parking_lot::{Mutex as PlMutex, MutexGuard as PlMutexGuard};
use std::cell::{Cell, RefCell};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{PoisonError, Weak};
use std::thread::JoinHandle;

pub(super) struct Candidate {
    pub(super) rule: Rule,
    pub(super) matched: MatchResult,
    pub(super) proof: CandidateProof,
    pub(super) focus: Foreground,
    pub(super) generation: u64,
    pub(super) terminator: Option<String>,
}
#[derive(Clone)]
pub(super) enum CandidateProof {
    Espanso(rawinput::Proof),
}

/// A running task that makes no progress (no input sent, no guard check)
/// for this long is fenced by the supervisor. 待基线确定: long clipboard
/// retries and slow `Open` targets must stay below it on real machines.
pub(super) const STALL_BUDGET: Duration = Duration::from_secs(10);
/// How long a sender, the fence or the busy guard waits for a send already
/// in flight before going ahead without the send lock (the holder is then a
/// fenced thread stuck inside the OS call).
const SEND_LOCK_WAIT: Duration = Duration::from_millis(250);
const FENCED: &str = "Executor task was fenced after a stall; stopped without further input";
const CLIPBOARD_HELD: &str =
    "Clipboard is still held by a stalled action; paste refused (no typing fallback)";

/// What one task produced.
pub(super) struct RunReport {
    pub(super) result: Result<ExecutionResult, String>,
}

/// Runs one candidate on the worker thread. Production wires the Windows
/// ports ([`run_on_windows`]); pipeline tests wire `crate::mock`. The runner
/// must route its injector through [`TaskScope::fence`] and its clipboard
/// through [`TaskScope::claim_clipboard`].
pub(super) type Runner = Arc<dyn Fn(&Candidate, &TaskScope) -> RunReport + Send + Sync>;

struct Job {
    candidate: Candidate,
    task: Task,
}

/// What the history needs to know about a queued run: identifiers only
/// (rule id, trigger kind, executable name), never the rule body or the
/// abbreviation.
#[derive(Clone)]
struct Task {
    rule_id: String,
    trigger: TriggerKind,
    process: Option<String>,
    queued_at: Instant,
}

impl Task {
    fn of(candidate: &Candidate, queued_at: Instant) -> Self {
        Self {
            rule_id: candidate.rule.id.clone(),
            trigger: match candidate.rule.trigger_mode {
                TriggerMode::Immediate => TriggerKind::Immediate,
                TriggerMode::Terminator => TriggerKind::Terminator,
            },
            process: Some(candidate.focus.process.clone()).filter(|name| !name.is_empty()),
            queued_at,
        }
    }

    fn record(&self, verdict: Verdict, timings: StageTimings) -> Record {
        Record {
            process: self.process.clone(),
            completed_steps: verdict.completed_steps,
            side_effects: verdict.side_effects,
            timings,
            ..Record::new(
                self.rule_id.clone(),
                self.trigger,
                verdict.outcome,
                verdict.reason,
            )
        }
    }
}

/// History verdict of one run (B-35).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Verdict {
    pub(super) outcome: Outcome,
    pub(super) reason: ReasonCode,
    pub(super) completed_steps: usize,
    pub(super) side_effects: bool,
}

impl Verdict {
    fn new(outcome: Outcome, reason: ReasonCode, side_effects: bool) -> Self {
        Self {
            outcome,
            reason,
            completed_steps: 0,
            side_effects,
        }
    }
}

/// Reason codes recognised in run messages. The action pipeline reports
/// failures as text; these fragments are the stable parts of those texts
/// (inject.rs, clipboard.rs, protection.rs and the guards in actions.rs).
const REASONS: &[(&str, ReasonCode)] = &[
    ("SendInput accepted", ReasonCode::InjectPartial),
    ("Executor task was fenced", ReasonCode::ExecutorTimeout),
    ("Executor task panicked", ReasonCode::ExecutorPanic),
    ("Execution timeout", ReasonCode::ExecutorTimeout),
    ("Clipboard is still held", ReasonCode::ClipboardBusy),
    ("Clipboard is busy", ReasonCode::ClipboardBusy),
    ("changed externally", ReasonCode::ClipboardChanged),
    ("cannot be safely", ReasonCode::ClipboardFormatSkipped),
    (
        "unsupported handle format",
        ReasonCode::ClipboardFormatSkipped,
    ),
    ("Cancelled", ReasonCode::Cancelled),
    ("Engine paused", ReasonCode::Paused),
    (
        "Foreground window/process changed",
        ReasonCode::FocusChanged,
    ),
    ("next target is not foreground", ReasonCode::FocusChanged),
    ("changed its declared target", ReasonCode::FocusChanged),
    ("User input changed", ReasonCode::StaleInput),
    (
        "Focus, caret, layout or configuration changed",
        ReasonCode::StaleInput,
    ),
    ("modifier is held", ReasonCode::ModifierHeld),
    ("physically held", ReasonCode::ModifierHeld),
    ("Physical mouse button", ReasonCode::ModifierHeld),
    ("blocked", ReasonCode::ProtectedControl),
];

/// Reason for a run that stopped with `message` while in `stage`.
fn reason_for(message: &str, stage: Stage) -> ReasonCode {
    match stage {
        Stage::Idle | Stage::Preflight => ReasonCode::PreflightFailed,
        Stage::Resolve => ReasonCode::ResolveFailed,
        _ => REASONS
            .iter()
            .find(|(fragment, _)| message.contains(fragment))
            .map_or(ReasonCode::Internal, |(_, reason)| *reason),
    }
}

/// Engine-side failures stay `Failed` even when nothing reached the screen;
/// refusals (guards, preflight, clipboard safety) are `Skipped` then.
fn failed_or_skipped(reason: ReasonCode, side_effects: bool) -> Outcome {
    let engine_failure = matches!(
        reason,
        ReasonCode::InjectPartial
            | ReasonCode::ClipboardBusy
            | ReasonCode::ExecutorPanic
            | ReasonCode::ExecutorTimeout
            | ReasonCode::Internal
    );
    if side_effects || engine_failure {
        Outcome::Failed
    } else {
        Outcome::Skipped
    }
}

/// The real outcome of a finished run (fixes "automatic status is always
/// unknown"). A completed plan is `Succeeded`: every event was accepted by
/// the system. The UI-facing `ExecutionResult.status` keeps saying
/// `unknown`, because consumption by the target is still not verified.
/// `stage` is where the run stopped; `sent` whether any input or clipboard
/// effect happened (used when the run returned an error).
pub(super) fn classify(
    result: &Result<ExecutionResult, String>,
    stage: Stage,
    sent: bool,
) -> Verdict {
    match result {
        Ok(run) => {
            let (outcome, reason) = match run.status.as_str() {
                "cancelled" => (Outcome::Cancelled, ReasonCode::Cancelled),
                "failed" => {
                    let reason = reason_for(&run.message, stage);
                    (failed_or_skipped(reason, run.side_effects), reason)
                }
                _ => (Outcome::Succeeded, ReasonCode::Ok),
            };
            Verdict {
                outcome,
                reason,
                completed_steps: run.completed_steps,
                side_effects: run.side_effects,
            }
        }
        Err(message) => {
            let reason = reason_for(message, stage);
            let outcome = match reason {
                ReasonCode::Cancelled => Outcome::Cancelled,
                _ => failed_or_skipped(reason, sent),
            };
            Verdict::new(outcome, reason, sent)
        }
    }
}

fn micros(from: Instant, to: Instant) -> Option<u64> {
    Some(u64::try_from(to.saturating_duration_since(from).as_micros()).unwrap_or(u64::MAX))
}

/// Per-stage durations from the stage entry times of one run. A stage ends
/// where the first of `until` was entered, or at `end` when the run stopped
/// inside it. Erase and restore are absent when the run had none.
pub(super) fn stage_timings(
    queued: Option<Instant>,
    started: Instant,
    end: Instant,
    marks: &StageMarks,
) -> StageTimings {
    let span = |stage: Stage, until: &[Stage]| {
        let from = marks[stage as usize]?;
        let to = until
            .iter()
            .find_map(|next| marks[*next as usize])
            .unwrap_or(end);
        micros(from, to)
    };
    StageTimings {
        queue_us: queued.and_then(|queued| micros(queued, started)),
        resolve_us: span(Stage::Resolve, &[Stage::Preflight]),
        erase_us: span(
            Stage::Erase,
            &[Stage::Output, Stage::Cleanup, Stage::Restore, Stage::Done],
        ),
        output_us: span(Stage::Output, &[Stage::Restore, Stage::Done]),
        restore_us: span(Stage::Restore, &[Stage::Done]),
        total_us: micros(queued.unwrap_or(started), end),
    }
}

/// Records one manual run (`execute`, no rule) in the process history.
pub(super) fn record_manual(
    process: &str,
    result: &Result<ExecutionResult, String>,
    started: Instant,
) {
    let end = SystemClock.now();
    let verdict = classify(result, current_stage(), SENT_EFFECT.with(Cell::get));
    let task = Task {
        rule_id: String::new(),
        trigger: TriggerKind::Manual,
        process: Some(process.to_owned()).filter(|name| !name.is_empty()),
        queued_at: started,
    };
    crate::runtime::current()
        .record(task.record(verdict, stage_timings(None, started, end, &stage_marks())));
}

#[derive(Default)]
struct Worker {
    sender: Option<mpsc::SyncSender<Job>>,
    handle: Option<JoinHandle<()>>,
    /// Replaced workers, kept so tests can wait for a fenced thread.
    retired: Vec<JoinHandle<()>>,
}

/// The task on the worker. Whoever takes it out of `ExecutorCore::running`
/// (the worker when the run ends, or the fence) writes its history entry,
/// so each run is recorded exactly once.
struct Running {
    epoch: u64,
    task: Task,
    started_at: Instant,
}

/// Keys (and mouse buttons, by their VK) that this program pressed and has
/// not released yet. The executor only reaches the bitmap through this
/// interface: [`Fenced`] marks, the busy guard and the fence read
/// `held_snapshot` to send the MARK key-ups. Merge note: P1b's
/// `keystate::OwnInput` tracks the same thing inside `inject.rs`; at merge
/// the coordinator replaces [`AtomicHeldKeys`] with an adapter over it so
/// there is a single source of truth.
pub(super) trait HeldKeys: Send + Sync {
    fn mark_down(&self, vk: u16);
    fn mark_up(&self, vk: u16);
    /// Currently held keys, ascending.
    fn held_snapshot(&self) -> Vec<u16>;
}

/// The B-01 256-bit atomic bitmap.
#[derive(Default)]
pub(super) struct AtomicHeldKeys([AtomicU64; 4]);

impl AtomicHeldKeys {
    fn bit(vk: u16) -> (usize, u64) {
        let vk = usize::from(vk & 0xff);
        (vk / 64, 1u64 << (vk % 64))
    }
}

impl HeldKeys for AtomicHeldKeys {
    fn mark_down(&self, vk: u16) {
        let (word, bit) = Self::bit(vk);
        self.0[word].fetch_or(bit, Ordering::SeqCst);
    }
    fn mark_up(&self, vk: u16) {
        let (word, bit) = Self::bit(vk);
        self.0[word].fetch_and(!bit, Ordering::SeqCst);
    }
    fn held_snapshot(&self) -> Vec<u16> {
        let words: [u64; 4] = std::array::from_fn(|i| self.0[i].load(Ordering::SeqCst));
        (0u16..256)
            .filter(|vk| words[usize::from(*vk) / 64] & (1u64 << (vk % 64)) != 0)
            .collect()
    }
}

/// Mouse flags → (button VK, pressed). `data` selects the X button.
fn mouse_button(flags: u32, data: u32) -> Option<(u16, bool)> {
    let x = if data == 2 { VK_XBUTTON2 } else { VK_XBUTTON1 };
    [
        (MOUSEEVENTF_LEFTDOWN, VK_LBUTTON, true),
        (MOUSEEVENTF_LEFTUP, VK_LBUTTON, false),
        (MOUSEEVENTF_RIGHTDOWN, VK_RBUTTON, true),
        (MOUSEEVENTF_RIGHTUP, VK_RBUTTON, false),
        (MOUSEEVENTF_MIDDLEDOWN, VK_MBUTTON, true),
        (MOUSEEVENTF_MIDDLEUP, VK_MBUTTON, false),
        (MOUSEEVENTF_XDOWN, x, true),
        (MOUSEEVENTF_XUP, x, false),
    ]
    .into_iter()
    .find(|(flag, _, _)| flags & flag != 0)
    .map(|(_, vk, down)| (vk, down))
}

/// The executor service: queue, worker thread, epoch, held-key bitmap and
/// the state the supervisor probes. Production has exactly one instance
/// ([`EXECUTOR`]), so its epoch is the process-wide executor epoch.
pub(super) struct ExecutorCore {
    me: Weak<ExecutorCore>,
    shared: Arc<Shared>,
    clock: Arc<dyn Clock>,
    runner: Runner,
    /// Sends the MARK key-ups for keys a task left pressed.
    release: Arc<dyn Injector>,
    budget: Duration,
    epoch: AtomicU64,
    /// Held-key tracking; key-ups after a stall are taken only from here.
    held: Box<dyn HeldKeys>,
    /// Clock origin for `beat_ns`.
    origin: Instant,
    /// Last progress of the running task, nanoseconds after `origin`.
    beat_ns: AtomicU64,
    running: PlMutex<Option<Running>>,
    worker: PlMutex<Worker>,
    /// Serializes "check epoch, then send" against the fence.
    send_lock: PlMutex<()>,
    /// `epoch + 1` of the task holding a clipboard lease; 0 when free.
    clipboard_holder: AtomicU64,
    /// Why the executor is down until the next rebuild.
    fault: PlMutex<Option<ReasonCode>>,
    /// Execution history (B-35); production uses `runtime::current()`.
    runtime: Arc<Runtime>,
    /// The running task got at least one event accepted (for the history
    /// entry the fence writes on its behalf).
    task_sent: AtomicBool,
}

impl ExecutorCore {
    pub(super) fn new(
        shared: Arc<Shared>,
        clock: Arc<dyn Clock>,
        runner: Runner,
        release: Arc<dyn Injector>,
        budget: Duration,
        runtime: Arc<Runtime>,
    ) -> Arc<Self> {
        let origin = clock.now();
        Arc::new_cyclic(|me| Self {
            me: me.clone(),
            shared,
            clock,
            runner,
            release,
            budget,
            runtime,
            task_sent: AtomicBool::new(false),
            epoch: AtomicU64::new(0),
            held: Box::new(AtomicHeldKeys::default()),
            origin,
            beat_ns: AtomicU64::new(0),
            running: PlMutex::new(None),
            worker: PlMutex::new(Worker::default()),
            send_lock: PlMutex::new(()),
            clipboard_holder: AtomicU64::new(0),
            fault: PlMutex::new(None),
        })
    }

    pub(super) fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }

    #[cfg(all(test, feature = "fault-injection"))]
    pub(super) fn held_keys(&self) -> Vec<u16> {
        self.held.held_snapshot()
    }

    fn send_guard(&self) -> Option<PlMutexGuard<'_, ()>> {
        self.send_lock.try_lock_for(SEND_LOCK_WAIT)
    }

    fn beat(&self) {
        let at = self.clock.now().saturating_duration_since(self.origin);
        self.beat_ns.store(at.as_nanos() as u64, Ordering::SeqCst);
    }

    fn last_beat(&self) -> Instant {
        self.origin + Duration::from_nanos(self.beat_ns.load(Ordering::SeqCst))
    }

    /// Queues a candidate; false when it was dropped (busy or no worker).
    /// A dropped trigger is recorded too: `dropped_busy` while another
    /// action runs, `skipped` while the executor waits for its rebuild.
    pub(super) fn enqueue(&self, candidate: Candidate) -> bool {
        let task = Task::of(&candidate, self.clock.now());
        if self
            .shared
            .busy
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            self.runtime.record(task.record(
                Verdict::new(Outcome::DroppedBusy, ReasonCode::DroppedBusy, false),
                StageTimings::default(),
            ));
            return false;
        }
        self.shared.cancel.store(false, Ordering::SeqCst);
        let job = Job {
            candidate,
            task: task.clone(),
        };
        let worker = self.worker.lock();
        let queued = match worker.sender.as_ref() {
            Some(sender) => sender.try_send(job).is_ok(),
            None => false,
        };
        drop(worker);
        if !queued {
            self.shared.busy.store(false, Ordering::SeqCst);
            let reason = self.fault.lock().unwrap_or(ReasonCode::EngineDown);
            self.runtime.record(task.record(
                Verdict::new(Outcome::Skipped, reason, false),
                StageTimings::default(),
            ));
        }
        queued
    }

    /// Makes the executor serve again. A live worker in the middle of a task
    /// is kept (a stall is the budget's call, not the rebuild's); otherwise a
    /// fresh worker thread takes over the current epoch and the previous one
    /// exits once its queue closes. Uses only the executor's own locks, so it
    /// is safe without the supervisor's machine lock and concurrently with
    /// `probe`/`enforce_budget`.
    pub(super) fn respawn(&self) -> Result<(), String> {
        let Some(me) = self.me.upgrade() else {
            return Err("executor is shutting down".into());
        };
        let mut worker = self.worker.lock();
        let alive =
            worker.sender.is_some() && worker.handle.as_ref().is_some_and(|h| !h.is_finished());
        let running = self.running.lock().as_ref().map(|r| r.epoch);
        match running {
            Some(epoch) if alive && epoch == self.epoch() => {
                drop(worker);
                *self.fault.lock() = None;
                tracing::info!(event = "executor.rebuilt", epoch, reused = true);
                return Ok(());
            }
            Some(epoch) => {
                // The worker died under a task: retire that task first.
                drop(worker);
                self.fence(epoch);
                worker = self.worker.lock();
            }
            None => {}
        }
        let epoch = self.epoch();
        let (sender, jobs) = mpsc::sync_channel::<Job>(1);
        let handle = std::thread::Builder::new()
            .name(format!("qt-executor-{epoch}"))
            .spawn(move || me.work(jobs, epoch))
            .map_err(|error| format!("executor thread: {error}"))?;
        worker.retired.retain(|old| !old.is_finished());
        if let Some(old) = worker.handle.replace(handle) {
            worker.retired.push(old);
        }
        worker.sender = Some(sender);
        drop(worker);
        *self.fault.lock() = None;
        tracing::info!(event = "executor.rebuilt", epoch, reused = false);
        Ok(())
    }

    /// Fences the task running in `epoch`: bumps the epoch so its thread can
    /// send nothing more, releases the keys it left pressed (MARK key-ups),
    /// frees the busy flag and closes the queue until the next rebuild.
    /// False when the epoch had already moved on.
    pub(super) fn fence(&self, epoch: u64) -> bool {
        let send = self.send_guard();
        if self
            .epoch
            .compare_exchange(epoch, epoch + 1, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return false;
        }
        *self.fault.lock() = Some(ReasonCode::ExecutorTimeout);
        self.worker.lock().sender = None;
        let running = self.running.lock().take();
        let released = self.release_held();
        let send_lock_busy = send.is_none();
        drop(send);
        if let Some(running) = running {
            // The stalled thread will never record this run itself. A send
            // still in flight (send lock held) may have reached the target.
            let now = self.clock.now();
            let sent = self.task_sent.load(Ordering::SeqCst) || released > 0 || send_lock_busy;
            self.runtime.record(running.task.record(
                Verdict::new(Outcome::Failed, ReasonCode::ExecutorTimeout, sent),
                StageTimings {
                    queue_us: micros(running.task.queued_at, running.started_at),
                    total_us: micros(running.task.queued_at, now),
                    ..StageTimings::default()
                },
            ));
        }
        self.shared.busy.store(false, Ordering::SeqCst);
        tracing::warn!(
            event = "executor.fenced",
            epoch = epoch + 1,
            released,
            send_lock_busy,
            "stalled task fenced; its thread may send nothing more"
        );
        true
    }

    /// Fences the running task when it made no progress within the budget.
    fn fence_if_stalled(&self, now: Instant) {
        let Some(epoch) = self.running.lock().as_ref().map(|running| running.epoch) else {
            return;
        };
        if epoch != self.epoch() {
            return;
        }
        let stalled = now.saturating_duration_since(self.last_beat());
        if stalled > self.budget {
            tracing::warn!(
                event = "executor.stalled",
                stalled_ms = stalled.as_millis() as u64,
                budget_ms = self.budget.as_millis() as u64
            );
            self.fence(epoch);
        }
    }

    /// Sends key-ups (or button-ups) for every key in [`HeldKeys`] that is
    /// not physically held. The keys come only from `held_snapshot`, and the
    /// release injector is `WinInjector` in production, so every key-up
    /// carries MARK and the hooks ignore it. A key-up the system refuses
    /// stays marked so the next release retries it. Callers hold the send
    /// lock when they can.
    fn release_held(&self) -> usize {
        let mut released = 0;
        for vk in self.held.held_snapshot() {
            if physical(vk) {
                // The user holds it now: releasing would desync the key.
                // Their own key-up ends it, so stop tracking it.
                self.held.mark_up(vk);
                continue;
            }
            let accepted = match vk {
                VK_LBUTTON => self.release.send_mouse(MOUSEEVENTF_LEFTUP, 0, 0, 0),
                VK_RBUTTON => self.release.send_mouse(MOUSEEVENTF_RIGHTUP, 0, 0, 0),
                VK_MBUTTON => self.release.send_mouse(MOUSEEVENTF_MIDDLEUP, 0, 0, 0),
                VK_XBUTTON1 => self.release.send_mouse(MOUSEEVENTF_XUP, 1, 0, 0),
                VK_XBUTTON2 => self.release.send_mouse(MOUSEEVENTF_XUP, 2, 0, 0),
                _ => self.release.send_key(vk, true),
            };
            if accepted == 1 {
                self.held.mark_up(vk);
                released += 1;
            } else {
                tracing::warn!(event = "executor.release_failed", vk);
            }
        }
        released
    }

    fn status(&self) -> Vec<(Component, Status)> {
        let fault = *self.fault.lock();
        let alive = {
            let worker = self.worker.lock();
            worker.sender.is_some() && worker.handle.as_ref().is_some_and(|h| !h.is_finished())
        };
        let executor = match fault {
            Some(reason) => Status::Down(reason),
            None if !alive => Status::Down(ReasonCode::Internal),
            None => Status::Ok,
        };
        let holder = self.clipboard_holder.load(Ordering::SeqCst);
        let clipboard = if holder != 0 && holder - 1 < self.epoch() {
            Status::Degraded(ReasonCode::ClipboardBusy)
        } else {
            Status::Ok
        };
        vec![
            (Component::Executor, executor),
            (Component::Clipboard, clipboard),
        ]
    }

    /// Closes the queue so the worker exits (tests; production never stops).
    #[cfg(test)]
    pub(super) fn shutdown(&self) {
        let mut worker = self.worker.lock();
        worker.sender = None;
        if let Some(handle) = worker.handle.take() {
            worker.retired.push(handle);
        }
    }

    /// Waits up to `timeout` for replaced worker threads to exit (tests).
    #[cfg(test)]
    pub(super) fn join_retired(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            let done = self
                .worker
                .lock()
                .retired
                .iter()
                .all(JoinHandle::is_finished);
            if done {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn work(self: Arc<Self>, jobs: mpsc::Receiver<Job>, epoch: u64) {
        for job in jobs {
            if self.epoch() != epoch {
                break;
            }
            self.run(job, epoch);
            if self.epoch() != epoch {
                // Fenced while running: the fence already cleaned up.
                tracing::info!(event = "executor.retired", epoch);
                break;
            }
        }
    }

    fn run(self: &Arc<Self>, job: Job, epoch: u64) {
        let Job { candidate, task } = job;
        let started = self.clock.now();
        self.task_sent.store(false, Ordering::SeqCst);
        *self.running.lock() = Some(Running {
            epoch,
            task,
            started_at: started,
        });
        self.beat();
        let guard = BusyGuard { core: self, epoch };
        let scope = Arc::new(TaskScope {
            core: Arc::clone(self),
            epoch,
        });
        let installed = ScopeLease::install(Arc::clone(&scope));
        begin_trace();
        set_stage(Stage::Resolve, started);
        let outcome = catch_unwind(AssertUnwindSafe(|| (self.runner)(&candidate, &scope)));
        drop(installed);
        let end = self.clock.now();
        let current = self.epoch() == epoch;
        let stage = current_stage();
        let sent = SENT_EFFECT.with(Cell::get) || self.task_sent.load(Ordering::SeqCst);
        let mut panicked = false;
        let result = match outcome {
            Ok(report) => report.result,
            Err(payload) => {
                panicked = true;
                let kind = if payload.is::<&'static str>() {
                    "str"
                } else if payload.is::<String>() {
                    "string"
                } else {
                    "other"
                };
                tracing::error!(
                    event = "executor.panic",
                    stage = current_stage().as_str(),
                    kind,
                    epoch,
                    "executor task panicked; the worker keeps serving"
                );
                if current {
                    *self.fault.lock() = Some(ReasonCode::ExecutorPanic);
                }
                Err("Executor task panicked; stopped".into())
            }
        };
        // Record exactly once: whoever takes `running` owns the history entry.
        // A fenced task finds it gone (the fence recorded ExecutorTimeout).
        let running = {
            let mut slot = self.running.lock();
            if slot.as_ref().is_some_and(|r| r.epoch == epoch) {
                slot.take()
            } else {
                None
            }
        };
        let owned = running.is_some();
        if let Some(running) = running {
            let verdict = if panicked {
                Verdict::new(Outcome::Failed, ReasonCode::ExecutorPanic, sent)
            } else {
                classify(&result, stage, sent)
            };
            let timings = stage_timings(
                Some(running.task.queued_at),
                running.started_at,
                end,
                &stage_marks(),
            );
            self.runtime.record(running.task.record(verdict, timings));
        }
        if owned && self.epoch() == epoch {
            let id = format!("{}:{}", candidate.rule.id, candidate.generation);
            let last = match result {
                Ok(mut completed) => {
                    completed.id = id;
                    completed
                }
                Err(message) => ExecutionResult {
                    id,
                    status: "failed".into(),
                    message,
                    completed_steps: 0,
                    side_effects: false,
                },
            };
            *self
                .shared
                .last
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(last);
        }
        drop(guard);
    }
}

/// Ends a task: frees the busy flag, releases keys the task still holds
/// (MARK key-ups) and returns a clipboard claim it never gave back. Runs on
/// normal return and on unwind; does nothing once the task was fenced,
/// because the fence did all of it (and a newer task may own them now).
struct BusyGuard<'a> {
    core: &'a ExecutorCore,
    epoch: u64,
}

impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        let core = self.core;
        let send = core.send_guard();
        if core.epoch() != self.epoch {
            return;
        }
        core.release_held();
        drop(send);
        let token = self.epoch + 1;
        let _ =
            core.clipboard_holder
                .compare_exchange(token, 0, Ordering::SeqCst, Ordering::SeqCst);
        core.running.lock().take();
        core.shared.busy.store(false, Ordering::SeqCst);
    }
}

/// The running task as its ports see it.
pub(super) struct TaskScope {
    core: Arc<ExecutorCore>,
    epoch: u64,
}

impl TaskScope {
    pub(super) fn shared(&self) -> &Arc<Shared> {
        &self.core.shared
    }

    /// Ok while the task's epoch is current (and records progress).
    pub(super) fn check(&self) -> Result<(), String> {
        if self.core.epoch() != self.epoch {
            return Err(FENCED.into());
        }
        self.core.beat();
        Ok(())
    }

    /// Wraps the task's injector with the epoch fence and key tracking.
    pub(super) fn fence<'a, I: Injector>(&'a self, inner: &'a I) -> Fenced<'a, I> {
        Fenced { inner, scope: self }
    }

    /// Claims the clipboard for this task's lease, then opens the backend.
    /// Refused (`ClipboardBusy`) while a fenced task still holds it.
    pub(super) fn claim_clipboard<C: ClipboardBackend>(
        &self,
        open: impl FnOnce() -> Result<C, String>,
    ) -> Result<Claimed<C>, String> {
        let token = self.epoch + 1;
        let holder = &self.core.clipboard_holder;
        if holder
            .compare_exchange(0, token, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            tracing::warn!(
                event = "executor.clipboard_busy",
                "clipboard held by a stalled task; paste refused"
            );
            return Err(CLIPBOARD_HELD.into());
        }
        match open() {
            Ok(inner) => Ok(Claimed {
                inner,
                core: Arc::clone(&self.core),
                token,
            }),
            Err(error) => {
                let _ = holder.compare_exchange(token, 0, Ordering::SeqCst, Ordering::SeqCst);
                Err(error)
            }
        }
    }

    /// Fault-injection points on the send path (no-ops without the feature).
    fn faults(&self) {
        if fault::take(FaultPoint::ExecutorPanic).is_some() {
            panic!("fault injection: executor panic");
        }
        if let Some(ms) = fault::take(FaultPoint::ExecutorHang) {
            // A real stall of this thread, not the injected clock.
            std::thread::sleep(Duration::from_millis(ms));
        }
    }
}

thread_local! {
    static SCOPE: RefCell<Option<Arc<TaskScope>>> = const { RefCell::new(None) };
}

/// Installs the task scope on the worker thread for one run.
struct ScopeLease;

impl ScopeLease {
    fn install(scope: Arc<TaskScope>) -> Self {
        SCOPE.with(|slot| *slot.borrow_mut() = Some(scope));
        ScopeLease
    }
}

impl Drop for ScopeLease {
    fn drop(&mut self) {
        SCOPE.with(|slot| slot.borrow_mut().take());
    }
}

/// Epoch check for the stamp guard: Err once the running task was fenced.
/// Always Ok outside an executor task (manual runs, direct pipeline tests).
pub(super) fn checkpoint() -> Result<(), String> {
    SCOPE.with(|slot| slot.borrow().as_ref().map_or(Ok(()), |scope| scope.check()))
}

/// Injector of one task: re-checks the epoch before every send (refusing
/// with 0 accepted events once it changed, which fail-stops the run) and
/// mirrors pressed keys and buttons into [`HeldKeys`]. The epoch fence lives
/// in this thin wrapper (and `checkpoint`); `inject.rs` is unchanged.
pub(super) struct Fenced<'a, I> {
    inner: &'a I,
    scope: &'a TaskScope,
}

impl<I: Injector> Fenced<'_, I> {
    fn send(&self, keys: &[u16], send: impl FnOnce() -> u32, settle: impl FnOnce(u32)) -> u32 {
        self.scope.faults();
        let core = &self.scope.core;
        let _send = core.send_guard();
        if self.scope.check().is_err() {
            return 0;
        }
        for key in keys {
            core.held.mark_down(*key);
        }
        let accepted = send();
        if accepted > 0 {
            // Side effects of a run the fence may have to record.
            core.task_sent.store(true, Ordering::SeqCst);
        }
        settle(accepted);
        accepted
    }
}

impl<I: Injector> Injector for Fenced<'_, I> {
    fn send_text(&self, text: &str) -> u32 {
        self.send(&[], || self.inner.send_text(text), |_| {})
    }
    fn send_keys(&self, keys: &[u16]) -> u32 {
        // Taps: each key is pressed and released by the same call.
        self.send(&[], || self.inner.send_keys(keys), |_| {})
    }
    fn send_chord(&self, keys: &[u16]) -> u32 {
        let held = &self.scope.core.held;
        let requested = keys.len() as u32 * 2;
        self.send(
            keys,
            || self.inner.send_chord(keys),
            |accepted| {
                // A partial chord may leave keys down: keep them for release.
                if accepted == requested {
                    keys.iter().for_each(|key| held.mark_up(*key));
                }
            },
        )
    }
    fn send_key(&self, key: u16, up: bool) -> u32 {
        let held = &self.scope.core.held;
        let pressing: &[u16] = if up { &[] } else { std::slice::from_ref(&key) };
        self.send(
            pressing,
            || self.inner.send_key(key, up),
            |accepted| {
                if up == (accepted == 1) {
                    held.mark_up(key);
                }
            },
        )
    }
    fn send_mouse(&self, flags: u32, data: u32, x: i32, y: i32) -> u32 {
        let held = &self.scope.core.held;
        let button = mouse_button(flags, data);
        let pressing = match button {
            Some((vk, true)) => vec![vk],
            _ => vec![],
        };
        self.send(
            &pressing,
            || self.inner.send_mouse(flags, data, x, y),
            |accepted| match button {
                Some((vk, down)) if down == (accepted != 1) => held.mark_up(vk),
                _ => {}
            },
        )
    }
}

/// A clipboard backend whose lease holds the task's clipboard claim; the
/// claim is returned when the lease drops it (after its restore attempt).
pub(super) struct Claimed<C> {
    inner: C,
    core: Arc<ExecutorCore>,
    token: u64,
}

impl<C> Drop for Claimed<C> {
    fn drop(&mut self) {
        let _ = self.core.clipboard_holder.compare_exchange(
            self.token,
            0,
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
    }
}

impl<C: ClipboardBackend> ClipboardBackend for Claimed<C> {
    type Staged = C::Staged;
    fn identity(&self) -> u64 {
        self.inner.identity()
    }
    fn owner(&self) -> u64 {
        self.inner.owner()
    }
    fn sequence(&self) -> u32 {
        self.inner.sequence()
    }
    fn locked<R>(&self, f: impl FnOnce() -> R) -> Option<R> {
        self.inner.locked(f)
    }
    fn formats(&self) -> Vec<u32> {
        self.inner.formats()
    }
    fn read(&self, format: u32, limit: usize) -> Result<Option<Vec<u8>>, String> {
        self.inner.read(format, limit)
    }
    fn clear(&self) -> bool {
        self.inner.clear()
    }
    fn stage(&self, bytes: &[u8]) -> Result<(Self::Staged, Vec<u8>), String> {
        self.inner.stage(bytes)
    }
    fn set(&self, format: u32, staged: Self::Staged) -> bool {
        self.inner.set(format, staged)
    }
}

impl ExecutorControl for ExecutorCore {
    fn enforce_budget(&self, now: Instant) {
        self.fence_if_stalled(now);
    }
    fn probe(&self, _now: Instant) -> Vec<(Component, Status)> {
        self.status()
    }
    fn rebuild(&self) -> Result<(), String> {
        self.respawn()
    }
}

/// Resolves the candidate's plan and runs it on `ports` (the injector and
/// clipboard already wrapped by the task scope).
pub(super) fn run_candidate<I, F, K, C, B>(
    ports: &Ports<'_, I, F, K, B>,
    shared: &Shared,
    candidate: &Candidate,
    mut context: VariableContext,
) -> RunReport
where
    I: Injector,
    F: crate::traits::ForegroundProbe,
    K: Clock,
    C: ClipboardBackend,
    B: Fn() -> Result<C, String>,
{
    context.parameters.insert(
        "action_parameter".into(),
        candidate
            .matched
            .parameter
            .clone()
            .map(serde_json::Value::String)
            .unwrap_or(serde_json::Value::Null),
    );
    let config = shared
        .config
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    match expand_reusable(&candidate.rule.action, &config, &context) {
        Ok(plan) => RunReport {
            result: execute_with_proof(
                ports,
                shared,
                plan,
                candidate.focus.clone(),
                candidate.generation,
                Some(candidate.proof.clone()),
                if candidate.matched.consume_terminator {
                    None
                } else {
                    candidate.terminator.clone()
                },
                match candidate.rule.action {
                    Action::Replay { speed, .. } => speed,
                    _ => 1.0,
                },
            ),
        },
        Err(errors) => RunReport {
            result: Err(errors
                .iter()
                .map(|e| e.message.clone())
                .collect::<Vec<_>>()
                .join("; ")),
        },
    }
}

/// Production runner: Windows ports behind the task's fence and claim.
fn run_on_windows(candidate: &Candidate, scope: &TaskScope) -> RunReport {
    let shared = scope.shared();
    let mut context = VariableContext {
        clipboard: arboard::Clipboard::new()
            .ok()
            .and_then(|mut cb| cb.get_text().ok()),
        ..Default::default()
    };
    let selection_needed = serde_json::to_string(&candidate.rule.action)
        .unwrap_or_default()
        .contains("{selection}")
        || matches!(candidate.rule.action, Action::Reusable { .. })
            && serde_json::to_string(
                &*shared.config.lock().unwrap_or_else(PoisonError::into_inner),
            )
            .unwrap_or_default()
            .contains("{selection}");
    if selection_needed {
        context.selection = capture_selection().ok();
    }
    let injector = scope.fence(&WinInjector);
    run_candidate(
        &Ports {
            injector: &injector,
            foreground: &WinForeground,
            clock: &SystemClock,
            clipboard: || scope.claim_clipboard(WinClipboard::new),
        },
        shared,
        candidate,
        context,
    )
}

/// The process-wide executor (set by [`start_executor`]).
static EXECUTOR: ArcSwapOption<ExecutorCore> = ArcSwapOption::const_empty();

/// Called by the RawInput dispatcher; false when the candidate was dropped.
pub(super) fn enqueue(_shared: &Shared, candidate: Candidate) -> bool {
    EXECUTOR
        .load_full()
        .is_some_and(|core| core.enqueue(candidate))
}

pub(crate) fn start_executor(shared: Arc<Shared>) {
    let core = ExecutorCore::new(
        shared,
        Arc::new(SystemClock),
        Arc::new(run_on_windows),
        Arc::new(WinInjector),
        STALL_BUDGET,
        crate::runtime::current(),
    );
    if let Err(error) = core.respawn() {
        tracing::error!(event = "executor.start_failed", %error);
    }
    EXECUTOR.store(Some(core));
}

/// The executor as the supervisor drives it, for `Supervisor::new` (P5).
/// `None` before `start_engine`.
pub fn executor_control() -> Option<Arc<dyn ExecutorControl>> {
    EXECUTOR
        .load_full()
        .map(|core| core as Arc<dyn ExecutorControl>)
}
