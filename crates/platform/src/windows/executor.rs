//! Executor (B-01, S-01): one worker thread resolves matched candidates and
//! runs their plans, behind panic isolation, an epoch fence and a busy guard.
//!
//! * Every task runs inside `catch_unwind`. A panic is logged as
//!   `executor.panic` (stage and payload kind only, never the payload text),
//!   the run counts as failed and the worker keeps serving; the supervisor
//!   then sees the executor as down and swaps in a fresh worker.
//! * Epoch: a task records the executor epoch when it starts. Every input it
//!   sends goes through [`Fenced`], which re-checks the epoch and refuses
//!   once it changed; the stamp guard (`actions::guarded_context`) checks it
//!   too, through [`checkpoint`]. The supervisor bumps the epoch when a task
//!   makes no progress for [`STALL_BUDGET`] ([`ExecutorCore::fence`]), so a
//!   hung thread that wakes up later sends nothing and quietly exits.
//! * Keys and mouse buttons pressed but not yet released are tracked in a
//!   256-bit atomic bitmap behind the small [`HeldKeys`] interface, so
//!   whoever ends the task (the busy guard, or the fence on a stall) sends
//!   the matching MARK key-ups. At merge the implementation is swapped for
//!   P1b's `keystate::OwnInput` (one source of truth).
//! * Send exclusion (`send_lock`). S1: every `SendInput` the executor makes
//!   (task output, the plan's own cleanup, re-sent key-ups) runs while the
//!   send lock is held, and the epoch check that allows it is made in the
//!   same hold, right before the call. A sender that cannot get the lock
//!   within [`SEND_LOCK_WAIT`] sends nothing: the call reports 0 accepted
//!   events, the task fail-stops and the executor reports
//!   Down(ExecutorTimeout). Nothing is ever sent without the lock. Manual
//!   runs take the same lock ([`serialized`]).
//!   In flight: a send is *in flight* once it has passed its epoch check
//!   while holding the send lock. Such a send may overlap the epoch bump
//!   (the fence bumps under `life`, not under the send lock), but every
//!   input after it is ordered behind it: all other input and the fence's
//!   re-sent key-ups go through the send lock, so they run after it
//!   returns; the re-send reads the bitmap snapshot inside its own, later
//!   hold, and S2 marked this send's key-downs before the call, so the keys
//!   it pressed are released too. The bump only withholds permission from
//!   sends that have not passed the check yet; seen from outside, this
//!   window is indistinguishable from a `SendInput` that was already
//!   executing when the fence came. Making the fence take the send lock
//!   first would not shorten it, it would only make the fence hang
//!   whenever a `SendInput` hangs.
//!   S2: the held-key bitmap is settled in the same hold: every key-down of
//!   the batch (taps and chords alike) is marked before the call; once
//!   `SendInput` returns `k`, only the accepted prefix is replayed in order
//!   (a down marks, an up clears), so a key-down past the prefix stays
//!   marked (at worst one redundant MARK key-up later). Without a usable
//!   `k` (a panic inside the call, or `k` above the batch size) every
//!   key-down of the batch stays held.
//!   S3: the fence's key-ups go through the same lock, so they follow the
//!   old epoch's in-flight send. The fence first records a release debt
//!   (`release_owed`); whoever holds the lock next pays it before doing
//!   anything else, reading the bitmap inside that hold: the fence itself
//!   when the lock is free, else the old thread when its send returns, the
//!   next `enforce_budget`, the rebuild, or the new epoch's first send.
//! * Fail closed: when a fenced thread never returns from `SendInput`, the
//!   send lock stays held, so nothing can be sent again. The debt stays
//!   unpaid and the executor reports Down(ExecutorTimeout); `rebuild`
//!   refuses to start a new worker without the lock; a task of the current
//!   epoch fails fast (no input, Failed/ExecutorTimeout); the supervisor's
//!   retries run out and it ends in Failed.
//! * Clipboard: a paste task claims the clipboard for its lease
//!   ([`TaskScope::claim_clipboard`]). While a fenced (stalled) task still
//!   holds the claim, paste actions fail with `ClipboardBusy` and the
//!   clipboard component reports Degraded. They do NOT fall back to typing:
//!   plan B-01 (5) suggested that, but it contradicts no-regression #2
//!   ("paste never degrades to typing"), and the coordinator ruled for #2.
//!   Key output runs as usual.
//! * Task lifecycle: one lock (`ExecutorCore::life`) guards the triple
//!   {epoch, running task, busy}. `epoch` and `shared.busy` are atomics so
//!   senders, the stamp guard and the RawInput dispatcher read them without
//!   locking, but the executor writes them only while holding `life`, and
//!   always together with `running`. The fence bumps the epoch and takes
//!   `running` in one hold, which makes it the owner of the run's history
//!   entry. A finishing task compares first (epoch still its own AND
//!   `running` still holds its run id), and only then takes `running`,
//!   publishes `last`, writes its history entry and frees `busy`, all in
//!   one hold; if either comparison fails it leaves silently (no history,
//!   no `last`, `busy` untouched), so a fenced task never clears state that
//!   a newer run owns. The busy guard's drop does the same.
//!   (`src-tauri`'s manual `execute_action` still flips `busy` itself
//!   around a manual run, outside this lock; that code is not ours to
//!   change, and `enqueue` only ever compare-exchanges `busy`.)
//! * Lock order, only ever nested this way: `send_lock` → `life` →
//!   `worker` → leaves (`fault`, `shared.last`, the history). Nothing takes
//!   `send_lock` while holding `life`. `Runtime::record` never calls back
//!   into the executor (it delivers events on its own thread), so it may
//!   run under `life`.
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
/// How long a sender, the fence, the busy guard or the rebuild waits for a
/// send already in flight. Past it the holder is taken to be stuck inside
/// the OS call and the waiter gives up without sending (fail closed).
const SEND_LOCK_WAIT: Duration = Duration::from_millis(250);
const FENCED: &str = "Executor task was fenced after a stall; stopped without further input";
const CLIPBOARD_HELD: &str =
    "Clipboard is still held by a stalled action; paste refused (no typing fallback)";
const PANICKED: &str = "Executor task panicked; stopped";

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

/// Reason codes recognised in run messages; the first matching fragment
/// wins. The action pipeline reports failures as text: the executor's own
/// texts are matched whole (their constants), the others by a stable
/// fragment of the text produced by inject.rs, clipboard.rs, protection.rs,
/// rawinput.rs or the guards in actions.rs. Every text this table maps is
/// quoted verbatim, with its source file, in the `ORIGINALS` table of
/// `pipeline_tests_executor`: a reworded text fails that test instead of
/// silently falling back to `Internal`.
const REASONS: &[(&str, ReasonCode)] = &[
    ("SendInput accepted", ReasonCode::InjectPartial),
    (FENCED, ReasonCode::ExecutorTimeout),
    (PANICKED, ReasonCode::ExecutorPanic),
    ("Execution timeout", ReasonCode::ExecutorTimeout),
    (CLIPBOARD_HELD, ReasonCode::ClipboardBusy),
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
    // inject::hotkey and inject::release_owned refusing a key the user holds.
    ("Physical key conflicts with", ReasonCode::ModifierHeld),
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

/// The outcome of a run that stopped for `reason`; the one place that
/// decides it, whichever way the run reported the stop (an error, a failed
/// result or a cancelled result). A cancel is `Cancelled` wherever it
/// landed: before the first backspace, between two of them or during the
/// output. Engine-side failures stay `Failed` even when nothing reached the
/// screen; refusals (guards, held keys, preflight, clipboard safety) are
/// `Skipped` then and `Failed` once something was sent.
fn outcome_for(reason: ReasonCode, side_effects: bool) -> Outcome {
    let engine_failure = matches!(
        reason,
        ReasonCode::InjectPartial
            | ReasonCode::ClipboardBusy
            | ReasonCode::ExecutorPanic
            | ReasonCode::ExecutorTimeout
            | ReasonCode::Internal
    );
    match reason {
        ReasonCode::Cancelled => Outcome::Cancelled,
        _ if side_effects || engine_failure => Outcome::Failed,
        _ => Outcome::Skipped,
    }
}

/// The real outcome of a finished run (fixes "automatic status is always
/// unknown"). A completed plan is `Succeeded`: every event was accepted by
/// the system. The UI-facing `ExecutionResult.status` keeps saying
/// `unknown`, because consumption by the target is still not verified.
/// `stage` is where the run stopped; `sent` whether any input or clipboard
/// effect happened (used when the run returned an error; a result carries
/// its own `side_effects`, which the verdict keeps).
pub(super) fn classify(
    result: &Result<ExecutionResult, String>,
    stage: Stage,
    sent: bool,
) -> Verdict {
    match result {
        Ok(run) => {
            let stopped = match run.status.as_str() {
                "cancelled" => Some(ReasonCode::Cancelled),
                "failed" => Some(reason_for(&run.message, stage)),
                _ => None,
            };
            let (outcome, reason) = stopped
                .map_or((Outcome::Succeeded, ReasonCode::Ok), |reason| {
                    (outcome_for(reason, run.side_effects), reason)
                });
            Verdict {
                outcome,
                reason,
                completed_steps: run.completed_steps,
                side_effects: run.side_effects,
            }
        }
        Err(message) => {
            let reason = reason_for(message, stage);
            Verdict::new(outcome_for(reason, sent), reason, sent)
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

/// The task on the worker. Whoever takes it out of `Life::running` (the
/// worker when the run ends, or the fence) writes its history entry, so
/// each run is recorded exactly once.
struct Running {
    /// Unique per run, so a stale owner never matches a newer run of the
    /// same epoch.
    id: u64,
    epoch: u64,
    task: Task,
    started_at: Instant,
    /// At least one event of the run was accepted (shared with its scope).
    sent: Arc<AtomicBool>,
}

/// Lifecycle state behind `ExecutorCore::life`. The lock also guards every
/// write of `ExecutorCore::epoch` and of `shared.busy` (see the module doc).
#[derive(Default)]
struct Life {
    running: Option<Running>,
}

/// Points where a test can hold a thread (no-ops outside tests).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Pause {
    /// In [`Fenced`]: the epoch check passed, the send lock not yet taken
    /// (the in-lock re-check comes next).
    BeforeSend,
    /// In [`ExecutorCore::fence`]: epoch bumped and `running` taken, the
    /// keys not yet released, the history entry not yet written.
    FenceDecided,
    /// In the busy guard: keys released, about to compare and clear.
    GuardBeforeClear,
}

#[cfg(test)]
type PauseHook = Arc<dyn Fn(Pause) + Send + Sync>;

/// A held send lock: whoever has one may call `SendInput` (S1).
pub(super) type SendTurn<'a> = PlMutexGuard<'a, ()>;

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
    /// The lifecycle lock over {epoch, running, busy}.
    life: PlMutex<Life>,
    next_id: AtomicU64,
    worker: PlMutex<Worker>,
    /// Every `SendInput` of the executor and of manual runs happens while
    /// this is held (S1); see the module doc.
    send_lock: PlMutex<()>,
    /// A fence decided to release the held keys but has not sent the
    /// key-ups yet; the next holder of `send_lock` pays it first (S3).
    release_owed: AtomicBool,
    /// `epoch + 1` of the task holding a clipboard lease; 0 when free.
    clipboard_holder: AtomicU64,
    /// Why the executor is down until the next rebuild.
    fault: PlMutex<Option<ReasonCode>>,
    /// Execution history (B-35); production uses `runtime::current()`.
    runtime: Arc<Runtime>,
    #[cfg(test)]
    pause_hook: PlMutex<Option<PauseHook>>,
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
            epoch: AtomicU64::new(0),
            held: Box::new(AtomicHeldKeys::default()),
            origin,
            beat_ns: AtomicU64::new(0),
            life: PlMutex::new(Life::default()),
            next_id: AtomicU64::new(1),
            worker: PlMutex::new(Worker::default()),
            send_lock: PlMutex::new(()),
            release_owed: AtomicBool::new(false),
            clipboard_holder: AtomicU64::new(0),
            fault: PlMutex::new(None),
            #[cfg(test)]
            pause_hook: PlMutex::new(None),
        })
    }

    pub(super) fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }

    /// Lets a test hold threads at the [`Pause`] points.
    #[cfg(test)]
    pub(super) fn on_pause(&self, hook: impl Fn(Pause) + Send + Sync + 'static) {
        *self.pause_hook.lock() = Some(Arc::new(hook));
    }

    #[cfg(test)]
    fn pause(&self, at: Pause) {
        let hook = self.pause_hook.lock().clone();
        if let Some(hook) = hook {
            hook(at);
        }
    }

    #[cfg(not(test))]
    fn pause(&self, _at: Pause) {}

    /// The running task as (run id, rule id) (tests).
    #[cfg(test)]
    pub(super) fn running_task(&self) -> Option<(u64, String)> {
        self.life
            .lock()
            .running
            .as_ref()
            .map(|running| (running.id, running.task.rule_id.clone()))
    }

    #[cfg(test)]
    pub(super) fn held_keys(&self) -> Vec<u16> {
        self.held.held_snapshot()
    }

    /// Holds the send lock, as a sender stuck inside `SendInput` would
    /// (tests).
    #[cfg(test)]
    pub(super) fn hold_send_lock(&self) -> SendTurn<'_> {
        self.send_lock.lock()
    }

    /// Whether a fence's key-ups are still unpaid (tests).
    #[cfg(test)]
    pub(super) fn release_owed(&self) -> bool {
        self.release_owed.load(Ordering::SeqCst)
    }

    /// Takes the send lock, waiting at most `wait`, and pays an outstanding
    /// release debt first, inside the same hold (S3). `None`: the lock is
    /// held elsewhere and the caller must not send.
    fn lock_send(&self, wait: Duration) -> Option<SendTurn<'_>> {
        let turn = self.send_lock.try_lock_for(wait)?;
        self.pay_release(&turn);
        Some(turn)
    }

    /// Sends the key-ups a fence left owed, if any; the count of key-ups
    /// sent. A key-up that fails stays in the bitmap for the next owner of
    /// the keys (the busy guard of a later task), the debt is not re-armed.
    fn pay_release(&self, turn: &SendTurn<'_>) -> usize {
        if !self.release_owed.swap(false, Ordering::SeqCst) {
            return 0;
        }
        let released = self.release_held(turn);
        tracing::info!(
            event = "executor.release_paid",
            released,
            epoch = self.epoch()
        );
        released
    }

    /// A sender of `epoch` could not get the send lock: the holder is stuck
    /// inside `SendInput`. While `epoch` is current the executor goes Down
    /// (ExecutorTimeout) until a rebuild gets the lock; `owe_release` also
    /// leaves the held keys' key-ups to the next holder of the lock. Checked
    /// under `life`, so a stale thread never marks a newer epoch down.
    fn fail_closed(&self, epoch: u64, owe_release: bool) {
        let life = self.life.lock();
        if self.epoch() == epoch {
            *self.fault.lock() = Some(ReasonCode::ExecutorTimeout);
            if owe_release {
                self.release_owed.store(true, Ordering::SeqCst);
            }
        }
        drop(life);
        tracing::warn!(
            event = "executor.send_refused",
            epoch,
            "send lock held by a stuck send; nothing sent"
        );
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
        let job = Job {
            candidate,
            task: task.clone(),
        };
        // `busy` is set and, when the queue is closed, reset in one hold of
        // the lifecycle lock (life → worker).
        let queued = {
            let _life = self.life.lock();
            if self
                .shared
                .busy
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
            {
                None
            } else {
                self.shared.cancel.store(false, Ordering::SeqCst);
                let queued = self
                    .worker
                    .lock()
                    .sender
                    .as_ref()
                    .is_some_and(|sender| sender.try_send(job).is_ok());
                if !queued {
                    self.shared.busy.store(false, Ordering::SeqCst);
                }
                Some(queued)
            }
        };
        let verdict = match queued {
            Some(true) => return true,
            None => Verdict::new(Outcome::DroppedBusy, ReasonCode::DroppedBusy, false),
            Some(false) => {
                let reason = self.fault.lock().unwrap_or(ReasonCode::EngineDown);
                Verdict::new(Outcome::Skipped, reason, false)
            }
        };
        self.runtime
            .record(task.record(verdict, StageTimings::default()));
        false
    }

    /// Makes the executor serve again. A live worker in the middle of a task
    /// is kept (a stall is the budget's call, not the rebuild's); otherwise a
    /// fresh worker thread takes over the current epoch and the previous one
    /// exits once its queue closes. Uses only the executor's own locks, so it
    /// is safe without the supervisor's machine lock and concurrently with
    /// `probe`/`enforce_budget`.
    ///
    /// Fail closed: it first takes the send lock (paying a fence's key-ups
    /// inside that hold). While a fenced thread is still inside `SendInput`
    /// it gets no lock and refuses, so the executor stays Down until that
    /// send returns, or for good, which ends the supervisor in Failed.
    pub(super) fn respawn(&self) -> Result<(), String> {
        let Some(me) = self.me.upgrade() else {
            return Err("executor is shutting down".into());
        };
        let alive = |worker: &Worker| {
            worker.sender.is_some() && worker.handle.as_ref().is_some_and(|h| !h.is_finished())
        };
        let orphan = {
            let life = self.life.lock();
            let worker = self.worker.lock();
            let live = alive(&worker);
            life.running
                .as_ref()
                .map(|running| running.epoch)
                .filter(|&epoch| !(live && epoch == self.epoch()))
        };
        if let Some(epoch) = orphan {
            // The worker died under a task: retire that task first.
            self.fence(epoch);
        }
        let Some(_turn) = self.lock_send(SEND_LOCK_WAIT) else {
            tracing::warn!(event = "executor.rebuild_refused", epoch = self.epoch());
            return Err(
                "a send of a fenced task is still in flight; the executor stays closed".into(),
            );
        };
        let life = self.life.lock();
        let mut worker = self.worker.lock();
        if let Some(epoch) = life.running.as_ref().map(|running| running.epoch) {
            if alive(&worker) && epoch == self.epoch() {
                drop(worker);
                *self.fault.lock() = None;
                drop(life);
                tracing::info!(event = "executor.rebuilt", epoch, reused = true);
                return Ok(());
            }
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
        drop(life);
        tracing::info!(event = "executor.rebuilt", epoch, reused = false);
        Ok(())
    }

    /// Fences the task running in `epoch`: bumps the epoch so its thread can
    /// send nothing more, releases the keys it left pressed (MARK key-ups),
    /// frees the busy flag and closes the queue until the next rebuild.
    /// False when the epoch had already moved on.
    pub(super) fn fence(&self, epoch: u64) -> bool {
        // One hold of the lifecycle lock decides everything: the epoch moves
        // on, `busy` is freed, the queue closes, and taking `running` makes
        // this fence the only writer of the run's history entry.
        let running = {
            let mut life = self.life.lock();
            if self.epoch() != epoch {
                return false;
            }
            self.epoch.store(epoch + 1, Ordering::SeqCst);
            self.shared.busy.store(false, Ordering::SeqCst);
            self.worker.lock().sender = None;
            *self.fault.lock() = Some(ReasonCode::ExecutorTimeout);
            self.release_owed.store(true, Ordering::SeqCst);
            life.running.take()
        };
        self.pause(Pause::FenceDecided);
        // S3: the key-ups go through the send lock, so they follow the old
        // epoch's send in flight. When that send does not return in time
        // the debt stays for the next holder of the lock.
        let (paid, released) = match self.send_lock.try_lock_for(SEND_LOCK_WAIT) {
            Some(turn) => (true, self.pay_release(&turn)),
            None => (false, 0),
        };
        let send_lock_busy = !paid;
        if let Some(running) = running {
            // The stalled thread will never record this run itself. A send
            // still in flight (send lock held) may have reached the target.
            let now = self.clock.now();
            let sent = running.sent.load(Ordering::SeqCst) || released > 0 || send_lock_busy;
            self.runtime.record(running.task.record(
                Verdict::new(Outcome::Failed, ReasonCode::ExecutorTimeout, sent),
                StageTimings {
                    queue_us: micros(running.task.queued_at, running.started_at),
                    total_us: micros(running.task.queued_at, now),
                    ..StageTimings::default()
                },
            ));
        }
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
        let running = self
            .life
            .lock()
            .running
            .as_ref()
            .map(|running| running.epoch);
        let Some(epoch) = running else {
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
    /// stays marked so the next release retries it. `_turn` proves the send
    /// lock is held (S1), so the snapshot is read inside the same hold that
    /// sends the key-ups, after any send of the old epoch returned (S3).
    fn release_held(&self, _turn: &SendTurn<'_>) -> usize {
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
            // Key-ups owed after a fence: a send is stuck holding the lock.
            None if self.release_owed.load(Ordering::SeqCst) => {
                Status::Down(ReasonCode::ExecutorTimeout)
            }
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

    /// Waits up to `timeout` for replaced worker threads, and a current one
    /// whose queue is closed, to exit (tests).
    #[cfg(test)]
    pub(super) fn join_retired(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            let done = {
                let worker = self.worker.lock();
                let closed = worker.handle.as_ref().filter(|_| worker.sender.is_none());
                worker
                    .retired
                    .iter()
                    .chain(closed)
                    .all(JoinHandle::is_finished)
            };
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
            self.run(job, epoch);
            if self.epoch() != epoch {
                // Fenced: the fence already cleaned up.
                tracing::info!(event = "executor.retired", epoch);
                break;
            }
        }
    }

    fn run(self: &Arc<Self>, job: Job, epoch: u64) {
        let Job { candidate, task } = job;
        let started = self.clock.now();
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let sent = Arc::new(AtomicBool::new(false));
        {
            let mut life = self.life.lock();
            if self.epoch() != epoch {
                // Queued just before a fence, which already freed `busy`.
                drop(life);
                let reason = self.fault.lock().unwrap_or(ReasonCode::ExecutorTimeout);
                self.runtime.record(task.record(
                    Verdict::new(Outcome::Skipped, reason, false),
                    StageTimings::default(),
                ));
                return;
            }
            life.running = Some(Running {
                id,
                epoch,
                task,
                started_at: started,
                sent: Arc::clone(&sent),
            });
        }
        self.beat();
        let mut guard = BusyGuard {
            core: self,
            epoch,
            id,
            done: false,
        };
        let scope = Arc::new(TaskScope {
            core: Arc::clone(self),
            epoch,
            sent,
            refused: AtomicBool::new(false),
        });
        let installed = ScopeLease::install(Arc::clone(&scope));
        begin_trace();
        set_stage(Stage::Resolve, started);
        let outcome = catch_unwind(AssertUnwindSafe(|| (self.runner)(&candidate, &scope)));
        drop(installed);
        let end = self.clock.now();
        let stage = current_stage();
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
                if self.epoch() == epoch {
                    *self.fault.lock() = Some(ReasonCode::ExecutorPanic);
                }
                Err(PANICKED.into())
            }
        };
        let released = guard.release();
        let sent = SENT_EFFECT.with(Cell::get) || scope.sent.load(Ordering::SeqCst) || released > 0;
        let verdict = if panicked {
            Verdict::new(Outcome::Failed, ReasonCode::ExecutorPanic, sent)
        } else if scope.refused.load(Ordering::SeqCst) {
            // A send found the lock held past SEND_LOCK_WAIT (S1).
            Verdict::new(Outcome::Failed, ReasonCode::ExecutorTimeout, sent)
        } else {
            classify(&result, stage, sent)
        };
        let id = format!("{}:{}", candidate.rule.id, candidate.generation);
        let last = match result {
            Ok(mut completed) => {
                completed.id = id;
                completed
            }
            // The published result carries the same side effects as the
            // verdict (a panic after a send was not effect-free).
            Err(message) => ExecutionResult {
                id,
                status: "failed".into(),
                message,
                completed_steps: 0,
                side_effects: sent,
            },
        };
        guard.settle(verdict, Some(last), end);
    }
}

/// Ends a task in two steps, each comparing before it clears, so the guard
/// of a fenced (or otherwise replaced) task never touches a newer run.
/// * [`release`](Self::release): under the send lock, a fence's owed
///   key-ups first, then, only while the epoch is still the task's, the
///   MARK key-ups for keys it left pressed; then its clipboard claim (its
///   own token only) goes back. Without the lock nothing is sent: the
///   key-ups are owed and the executor goes Down.
/// * [`settle`](Self::settle): under `life`, only if the epoch is still the
///   task's AND `running` still holds its run id: takes `running`,
///   publishes `last`, writes the history entry and frees `busy`, in one
///   hold. Otherwise it leaves silently: no history, no `last`, `busy`
///   untouched (the fence recorded the run and freed `busy`).
///
/// Dropped before `settle` (unwinding) it does both, recording the run as
/// failed (`Internal`).
struct BusyGuard<'a> {
    core: &'a ExecutorCore,
    epoch: u64,
    id: u64,
    done: bool,
}

impl BusyGuard<'_> {
    /// Releases what the task still holds; the number of key-ups sent.
    fn release(&self) -> usize {
        let core = self.core;
        let released = match core.send_lock.try_lock_for(SEND_LOCK_WAIT) {
            // A fence's debt first (S3), then this task's own keys while
            // its epoch is current; a fenced task leaves them to the fence.
            Some(turn) if core.epoch() == self.epoch => {
                core.pay_release(&turn) + core.release_held(&turn)
            }
            Some(turn) => core.pay_release(&turn),
            None => {
                // A send is stuck holding the lock: owe the key-ups, never
                // send them without it.
                core.fail_closed(self.epoch, true);
                0
            }
        };
        let token = self.epoch + 1;
        let _ =
            core.clipboard_holder
                .compare_exchange(token, 0, Ordering::SeqCst, Ordering::SeqCst);
        released
    }

    fn settle(&mut self, verdict: Verdict, last: Option<ExecutionResult>, end: Instant) {
        self.done = true;
        let core = self.core;
        core.pause(Pause::GuardBeforeClear);
        let mut life = core.life.lock();
        if core.epoch() != self.epoch {
            return;
        }
        let Some(running) = life.running.take_if(|running| running.id == self.id) else {
            return;
        };
        if let Some(last) = last {
            *core
                .shared
                .last
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(last);
        }
        let timings = stage_timings(
            Some(running.task.queued_at),
            running.started_at,
            end,
            &stage_marks(),
        );
        core.runtime.record(running.task.record(verdict, timings));
        core.shared.busy.store(false, Ordering::SeqCst);
    }
}

impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        let released = self.release();
        tracing::error!(event = "executor.unwound", epoch = self.epoch, released);
        let end = self.core.clock.now();
        self.settle(
            Verdict::new(Outcome::Failed, ReasonCode::Internal, true),
            None,
            end,
        );
    }
}

/// The running task as its ports see it.
pub(super) struct TaskScope {
    core: Arc<ExecutorCore>,
    epoch: u64,
    /// At least one event was accepted (shared with `Running::sent`).
    sent: Arc<AtomicBool>,
    /// A send found the send lock held past [`SEND_LOCK_WAIT`]; every later
    /// send of the task is refused at once (fail-stop).
    refused: AtomicBool,
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

/// True on a worker thread while it runs an executor task. Such a task
/// publishes its result through its busy guard, under the lifecycle lock.
pub(super) fn in_task() -> bool {
    SCOPE.with(|slot| slot.borrow().is_some())
}

/// One event of a batch as the held-key bitmap sees it: (vk, is key-up).
type KeyEvent = (u16, bool);

/// The events of `send_keys`: each key pressed and released in turn.
fn taps(keys: &[u16]) -> Vec<KeyEvent> {
    keys.iter()
        .flat_map(|&vk| [(vk, false), (vk, true)])
        .collect()
}

/// The events of `send_chord`: downs in order, then ups in reverse.
fn chord(keys: &[u16]) -> Vec<KeyEvent> {
    let downs = keys.iter().map(|&vk| (vk, false));
    downs
        .chain(keys.iter().rev().map(|&vk| (vk, true)))
        .collect()
}

/// Injector of one task: every send runs under the send lock with the epoch
/// re-checked in that hold (refusing with 0 accepted events once it changed
/// or when the lock stays held, which fail-stops the run), and the held-key
/// bitmap is settled in the same hold. The epoch fence lives in this thin
/// wrapper (and `checkpoint`); `inject.rs` is unchanged.
pub(super) struct Fenced<'a, I> {
    inner: &'a I,
    scope: &'a TaskScope,
}

impl<I: Injector> Fenced<'_, I> {
    /// Sends one batch whose key events are `events` (S1, S2).
    fn send(&self, events: &[KeyEvent], send: impl FnOnce() -> u32) -> u32 {
        let scope = self.scope;
        let core = &scope.core;
        scope.faults();
        // A fenced or refused task leaves at once, without waiting.
        if scope.refused.load(Ordering::SeqCst) || scope.check().is_err() {
            return 0;
        }
        core.pause(Pause::BeforeSend);
        let Some(_turn) = core.lock_send(SEND_LOCK_WAIT) else {
            scope.refused.store(true, Ordering::SeqCst);
            core.fail_closed(scope.epoch, false);
            return 0;
        };
        // S1: the check that allows this send, in the same hold as the call.
        if scope.check().is_err() {
            return 0;
        }
        // S2: every key-down of the batch is held before the call...
        for &(vk, up) in events {
            if !up {
                core.held.mark_down(vk);
            }
        }
        let accepted = send();
        if accepted > 0 {
            // Side effects of a run the fence may have to record.
            scope.sent.store(true, Ordering::SeqCst);
        }
        // ...and after it only the accepted prefix is replayed, in order. A
        // count above the batch size is unusable: every key-down stays held.
        if let Some(prefix) = events.get(..accepted as usize) {
            for &(vk, up) in prefix {
                if up {
                    core.held.mark_up(vk);
                } else {
                    core.held.mark_down(vk);
                }
            }
        }
        accepted
    }
}

impl<I: Injector> Injector for Fenced<'_, I> {
    fn send_text(&self, text: &str) -> u32 {
        // Unicode events: no virtual keys to track.
        self.send(&[], || self.inner.send_text(text))
    }
    fn send_keys(&self, keys: &[u16]) -> u32 {
        self.send(&taps(keys), || self.inner.send_keys(keys))
    }
    fn send_chord(&self, keys: &[u16]) -> u32 {
        self.send(&chord(keys), || self.inner.send_chord(keys))
    }
    fn send_key(&self, key: u16, up: bool) -> u32 {
        self.send(&[(key, up)], || self.inner.send_key(key, up))
    }
    fn send_mouse(&self, flags: u32, data: u32, x: i32, y: i32) -> u32 {
        let events: Vec<KeyEvent> = mouse_button(flags, data)
            .map(|(vk, down)| (vk, !down))
            .into_iter()
            .collect();
        self.send(&events, || self.inner.send_mouse(flags, data, x, y))
    }
}

/// Injector of a manual run (`actions::execute_inner`): each send takes the
/// executor's send lock (paying a fence's key-ups first), so manual output
/// never interleaves with a task's or a fence's (S1). It refuses (0
/// accepted, which fail-stops the run) while the lock stays held. Without
/// an executor (`core` is `None`: not started, or direct pipeline tests)
/// there is nothing to exclude and it sends directly. Manual runs carry no
/// epoch and keep their own key cleanup, so the bitmap is not touched.
pub(super) struct Serialized<'a, I> {
    inner: &'a I,
    core: Option<Arc<ExecutorCore>>,
}

impl<'a, I: Injector> Serialized<'a, I> {
    pub(super) fn new(inner: &'a I, core: Option<Arc<ExecutorCore>>) -> Self {
        Self { inner, core }
    }

    fn send(&self, send: impl FnOnce() -> u32) -> u32 {
        let Some(core) = &self.core else {
            return send();
        };
        let Some(_turn) = core.lock_send(SEND_LOCK_WAIT) else {
            tracing::warn!(
                event = "executor.manual_send_refused",
                "send lock held by a stuck send; manual input not sent"
            );
            return 0;
        };
        send()
    }
}

impl<I: Injector> Injector for Serialized<'_, I> {
    fn send_text(&self, text: &str) -> u32 {
        self.send(|| self.inner.send_text(text))
    }
    fn send_keys(&self, keys: &[u16]) -> u32 {
        self.send(|| self.inner.send_keys(keys))
    }
    fn send_chord(&self, keys: &[u16]) -> u32 {
        self.send(|| self.inner.send_chord(keys))
    }
    fn send_key(&self, key: u16, up: bool) -> u32 {
        self.send(|| self.inner.send_key(key, up))
    }
    fn send_mouse(&self, flags: u32, data: u32, x: i32, y: i32) -> u32 {
        self.send(|| self.inner.send_mouse(flags, data, x, y))
    }
}

/// Wraps a manual run's injector with the executor's send lock.
pub(super) fn serialized<I: Injector>(inner: &I) -> Serialized<'_, I> {
    Serialized::new(inner, EXECUTOR.load_full())
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
        if self.release_owed.load(Ordering::SeqCst) {
            // Retry a fence's key-ups (S3); taking the lock pays them. Never
            // waits: a send still stuck keeps the debt for the next try.
            drop(self.lock_send(Duration::ZERO));
        }
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
