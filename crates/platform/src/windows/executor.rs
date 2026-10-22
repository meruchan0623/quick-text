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
//! * Keys and mouse buttons pressed but not yet released are tracked in
//!   P1b's `keystate::OwnInput`, the process's one record of the input it
//!   holds down (shared with `inject.rs` and the hooks), through the small
//!   [`HeldKeys`] interface, so whoever ends the task (the busy guard, or
//!   after a fence on a stall the payers of its debt, S3) sends the
//!   matching MARK key-ups. A key the hooks saw released elsewhere is
//!   cleared there and gets no key-up from here.
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
//!   input after it is ordered behind it: all other input and the re-sent
//!   key-ups of the fence's debt go through the send lock, so they run
//!   after it returns; the re-send reads the bitmap snapshot inside its
//!   own, later hold, and S2 marked this send's key-downs before the call,
//!   so the keys it pressed are released too. The bump only withholds
//!   permission from sends that have not passed the check yet; seen from
//!   outside, this window is indistinguishable from a `SendInput` that was
//!   already executing when the fence came. Making the fence take the send
//!   lock first would not shorten it, it would only make the fence hang
//!   whenever a `SendInput` hangs.
//!   S2: the held-key bitmap is settled in the same hold: every key-down of
//!   the batch (taps and chords alike) is marked before the call; once
//!   `SendInput` returns `k`, only the accepted prefix is replayed in order
//!   (a down marks, an up clears), so a key-down past the prefix stays
//!   marked (at worst one redundant MARK key-up later). Without a usable
//!   `k` (a panic inside the call, or `k` above the batch size) every
//!   key-down of the batch stays held.
//!   S3: the fence's key-ups go through the same lock, so they follow the
//!   old epoch's in-flight send. The fence itself never sends them: it only
//!   records a release debt (`release_owed`). The debt is paid inside a
//!   hold of the send lock, reading the bitmap in that hold, and only by
//!   (a) the old thread's busy guard once its send returns, (b) the
//!   executor's own worker thread, first thing after it starts and before
//!   every task, (c) `rebuild` (a supervisor attempt, which may block), and
//!   a manual run before its first send ([`Serialized`]). A payment settles
//!   the debt only when `SendInput` accepted every owed key-up and their
//!   bits are cleared; a refused key-up stays in the bitmap and stays owed.
//!   While anything is owed, no task or manual input is sent at all (the
//!   run fails at once, Failed/ExecutorTimeout), the probe reports
//!   Down(ExecutorTimeout) and `rebuild` returns Err, so the supervisor
//!   backs off and retries, ends in Failed when its retries run out, and
//!   repair tries again. The next payment that `SendInput` accepts clears
//!   the debt and everything resumes on its own. Residual risk: the keys a
//!   stalled task left pressed stay down from the fence until the next
//!   payment (the old thread's return, the replacement worker or the
//!   rebuild, normally the next supervisor tick plus backoff); the task
//!   already made no progress for [`STALL_BUDGET`] (10 s), so that is
//!   accepted.
//! * The supervisor's side never touches the injector: `enforce_budget`,
//!   `probe` and the fence only change state (bump the epoch, record the
//!   debt, free `busy`, take `running`, close the queue for a respawn).
//!   The fence's one look at the send lock is a `try_lock` to tell whether
//!   a send is in flight, released at once without sending.
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
//!   a newer run owns. The busy guard's drop does the same. A manual run
//!   (`src-tauri`'s `execute_action`) holds `busy` through [`ManualBusy`]:
//!   claimed by a compare-and-set under `life`, like `enqueue`'s, and given
//!   back under `life` only while that claim still owns `busy` (a fence
//!   frees `busy` and drops the claim with it). Its output still goes
//!   through [`Serialized`].
//! * While holding `life`: never call the injector or the clipboard, never
//!   call a callback, never join a thread, never make a channel send that
//!   can block, and never wait for `send_lock`. Only bounded leaf locks are
//!   taken under it (`worker`, `fault`, `shared.last`, and inside
//!   `Runtime::record` the history ring, a brief read of the listener list
//!   and the event queue's lock). Two bounded non-lock steps also run under
//!   it: the rebuild starts the worker thread, and `Runtime::record` offers
//!   its event to the event thread's bounded queue with `try_send` (a full
//!   queue drops and counts it, so the send never blocks), starting the
//!   event thread when there is none; it never runs a listener inline
//!   (listeners run on that thread, with no runtime lock held) and never
//!   calls back into the executor. The worker's job queue is only ever
//!   offered with `try_send`.
//! * The supervisor's calls never wait for an executor lock (A2):
//!   `enforce_budget` and `probe` only `try_lock` `life`, `worker` and
//!   `fault`. When one is held elsewhere, `probe` returns the status it
//!   last observed (kept in an atomic) and `enforce_budget` skips the round
//!   and looks again on the next tick, so a fence comes at most one tick
//!   (1 s) late, against a [`STALL_BUDGET`] of 10 s. Supervisor-path calls
//!   (`enforce_budget`, `probe`) never wait on executor locks (`try_lock`
//!   only); they may take the runtime's leaf locks, which are bounded by
//!   the runtime's contract (a fence's `retire` records the fenced run
//!   through `Runtime::record`). `rebuild` is a recovery attempt and may
//!   block, as said above.
//! * Stall check (A1): `enforce_budget` decides and fences in one hold of
//!   `life`. A first look names the suspect run; in the fencing hold, and
//!   before anything changes, it confirms that the epoch is the same, that
//!   `running` still holds the same run id, and that the heartbeat's
//!   deadline has really passed. A new run's heartbeat starts in the hold
//!   that registers it, so a check that sees the run sees its fresh
//!   deadline. A run that ended while the check waited is never fenced in
//!   the place of the next one.
//! * Lock order, only ever nested this way: `send_lock` → `life` →
//!   `worker` → leaves (`fault`, `shared.last`, the history). Nothing waits
//!   for `send_lock` while holding `life`.
use super::actions::{
    begin_trace, current_stage, run_plan, set_stage, stage_marks, Ports, Stage, StageMarks,
};
use super::clipboard::{try_clipboard_health, WinClipboard};
use super::foreground::WinForeground;
use super::inject::{WinInjector, SENT_EFFECT};
use super::keystate::{own_input, physical, OwnInput};
use super::*;
use crate::fault::{self, FaultPoint};
use crate::health::{Component, ComponentHealth, Outcome, ReasonCode, StageTimings, TriggerKind};
use crate::history::Record;
use crate::runtime::Runtime;
use crate::supervisor::{ExecutorControl, Status};
use crate::traits::{ClipboardBackend, ClipboardRead, Clock, Injector, SystemClock};
use arc_swap::ArcSwapOption;
use parking_lot::{Mutex as PlMutex, MutexGuard as PlMutexGuard};
use std::cell::{Cell, RefCell};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::AtomicU8;
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
/// How long a sender, a payer of owed key-ups or the rebuild waits for a
/// send already in flight (the fence and the supervisor's calls never wait). Past it the holder is taken to be stuck inside
/// the OS call and the waiter gives up without sending (fail closed).
const SEND_LOCK_WAIT: Duration = Duration::from_millis(250);
const FENCED: &str = "Executor task was fenced after a stall; stopped without further input";
const CLIPBOARD_HELD: &str =
    "Clipboard is still held by a stalled action; paste refused (no typing fallback)";
const PANICKED: &str = "Executor task panicked; stopped";
const RELEASE_OWED: &str = "Key-ups of a stopped action are still owed; refused without input";

/// What one task produced.
pub(super) struct RunReport {
    pub(super) result: Result<ExecutionResult, String>,
    /// The clipboard lease's reason when the lease stopped the run (B-08),
    /// as it was raised: it wins over the message text ([`classify`]).
    pub(super) reason: Option<ReasonCode>,
    /// The formats the lease's backup skipped, or the ones behind its
    /// refusal, for `HistoryEntry::skipped_formats`.
    pub(super) skipped_formats: Vec<u32>,
}

impl RunReport {
    /// A report without a structured reason or skipped formats.
    pub(super) fn text(result: Result<ExecutionResult, String>) -> Self {
        Self {
            result,
            reason: None,
            skipped_formats: vec![],
        }
    }
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

    fn record(&self, verdict: Verdict, skipped_formats: Vec<u32>, timings: StageTimings) -> Record {
        Record {
            process: self.process.clone(),
            completed_steps: verdict.completed_steps,
            side_effects: verdict.side_effects,
            skipped_formats,
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
/// wins. Clipboard stops never come through here: the lease raises its
/// reason with the stop and the executor's clipboard claim flags its
/// refusal, both passed on as they are ([`RunReport::reason`]). The rest of
/// the action pipeline still reports failures as text: the executor's own
/// texts are matched whole (their constants), the others by a stable
/// fragment of the text produced by inject.rs, protection.rs, rawinput.rs
/// or the guards in actions.rs. Every text this table maps is quoted
/// verbatim, with its source file, in the `ORIGINALS` table of
/// `pipeline_tests_executor`: a reworded text fails that test instead of
/// silently falling back to `Internal`.
const REASONS: &[(&str, ReasonCode)] = &[
    ("SendInput accepted", ReasonCode::InjectPartial),
    (FENCED, ReasonCode::ExecutorTimeout),
    (RELEASE_OWED, ReasonCode::ExecutorTimeout),
    (PANICKED, ReasonCode::ExecutorPanic),
    ("Execution timeout", ReasonCode::ExecutorTimeout),
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
/// `reported` is the reason the run raised with its stop (the clipboard
/// lease's, [`RunReport::reason`]); it wins over the message text, which is
/// only mapped ([`reason_for`]) when the run reported none. `stage` is where
/// the run stopped; `sent` whether any input or clipboard effect happened
/// (used when the run returned an error; a result carries its own
/// `side_effects`, which the verdict keeps).
pub(super) fn classify(
    result: &Result<ExecutionResult, String>,
    reported: Option<ReasonCode>,
    stage: Stage,
    sent: bool,
) -> Verdict {
    let reason_for = |message: &str| reported.unwrap_or_else(|| reason_for(message, stage));
    match result {
        Ok(run) => {
            let stopped = match run.status.as_str() {
                "cancelled" => Some(ReasonCode::Cancelled),
                "failed" => Some(reason_for(&run.message)),
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
            let reason = reason_for(message);
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
pub(super) fn record_manual(process: &str, report: &RunReport, started: Instant) {
    let end = SystemClock.now();
    let sent = SENT_EFFECT.with(Cell::get);
    let verdict = if MANUAL_REFUSED.with(|refused| refused.replace(false)) {
        // Refused by the send exclusion (S1, S3), like a task's send: the
        // same verdict, not the "accepted 0/N" text it surfaces as.
        Verdict::new(Outcome::Failed, ReasonCode::ExecutorTimeout, sent)
    } else {
        classify(&report.result, report.reason, current_stage(), sent)
    };
    let task = Task {
        rule_id: String::new(),
        trigger: TriggerKind::Manual,
        process: Some(process.to_owned()).filter(|name| !name.is_empty()),
        queued_at: started,
    };
    crate::runtime::current().record(task.record(
        verdict,
        report.skipped_formats.clone(),
        stage_timings(None, started, end, &stage_marks()),
    ));
}

#[derive(Default)]
struct Worker {
    sender: Option<mpsc::SyncSender<Job>>,
    handle: Option<JoinHandle<()>>,
    /// Replaced workers, kept so tests can wait for a fenced thread.
    retired: Vec<JoinHandle<()>>,
}

impl Worker {
    /// The queue is open and its thread still runs.
    fn alive(&self) -> bool {
        self.sender.is_some() && self.handle.as_ref().is_some_and(|h| !h.is_finished())
    }
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
    /// The id of the manual run's claim holding `busy` ([`ManualBusy`]).
    manual: Option<u64>,
}

/// Points where a test can hold a thread (no-ops outside tests).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Pause {
    /// In [`Fenced`]: the epoch check passed, the send lock not yet taken
    /// (the in-lock re-check comes next).
    BeforeSend,
    /// In [`ExecutorCore::fence`]: epoch bumped, debt recorded and `running`
    /// taken; the send lock not yet looked at, the history entry not yet
    /// written.
    FenceDecided,
    /// In the busy guard: keys released, about to compare and clear.
    GuardBeforeClear,
    /// In the stall check: the first look named a suspect run; the fencing
    /// hold of `life`, which confirms it again, comes next.
    StallCheck,
}

/// Every executor status [`ExecutorCore::status`] reports, so the last one
/// observed fits in an atomic (an index) for `probe` to fall back on.
const EXECUTOR_STATUSES: [Status; 4] = [
    Status::Ok,
    Status::Down(ReasonCode::ExecutorTimeout),
    Status::Down(ReasonCode::ExecutorPanic),
    Status::Down(ReasonCode::Internal),
];

/// The index of `status` in [`EXECUTOR_STATUSES`]; a status outside it
/// (none is reported today) is kept as Down(Internal).
fn status_index(status: Status) -> u8 {
    EXECUTOR_STATUSES
        .iter()
        .position(|known| *known == status)
        .unwrap_or(EXECUTOR_STATUSES.len() - 1) as u8
}

/// Every clipboard status [`ExecutorCore::status`] caches, as for
/// [`EXECUTOR_STATUSES`]. A lease only ever reports Healthy or
/// Degraded(ClipboardBusy); any other status is cached as Degraded or Down
/// with `Internal`. Clipboard is optional: the supervisor publishes it as
/// Degraded at worst and never rebuilds for it.
const CLIPBOARD_STATUSES: [Status; 4] = [
    Status::Ok,
    Status::Degraded(ReasonCode::ClipboardBusy),
    Status::Degraded(ReasonCode::Internal),
    Status::Down(ReasonCode::Internal),
];

/// The index of `status` in [`CLIPBOARD_STATUSES`].
fn clipboard_index(status: Status) -> u8 {
    CLIPBOARD_STATUSES
        .iter()
        .position(|known| *known == status)
        .unwrap_or(match status {
            Status::Ok => 0,
            Status::Degraded(_) => 2,
            Status::Down(_) => 3,
        }) as u8
}

/// The worse of two statuses of one component; `a` on a tie.
fn worse(a: Status, b: Status) -> Status {
    let rank = |status: Status| match status {
        Status::Ok => 0,
        Status::Degraded(_) => 1,
        Status::Down(_) => 2,
    };
    if rank(b) > rank(a) {
        b
    } else {
        a
    }
}

/// Reads the clipboard component's own health without waiting; `None`
/// while it cannot be read now. Production reads the process-wide
/// `clipboard_health` ([`try_clipboard_health`]); a test rig its mock's.
struct ClipboardProbe(Box<dyn Fn() -> Option<ComponentHealth> + Send + Sync>);

#[cfg(test)]
type PauseHook = Arc<dyn Fn(Pause) + Send + Sync>;

/// The executor locks a test can hold from another thread.
#[cfg(test)]
#[derive(Clone, Copy, Debug)]
pub(super) enum CoreLock {
    Life,
    Worker,
    Fault,
}

/// A held send lock: whoever has one may call `SendInput` (S1).
pub(super) type SendTurn<'a> = PlMutexGuard<'a, ()>;

/// Whether the key-ups a fence (or a busy guard) owes are paid (S3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Debt {
    /// Nothing owed: every owed key-up was accepted and its bit cleared.
    Settled,
    /// At least one key-up was refused; it stays in the bitmap, owed.
    Owed,
}

/// The result of one payment attempt.
#[derive(Clone, Copy, Debug)]
struct Payment {
    /// Key-ups `SendInput` accepted in this attempt.
    released: usize,
    debt: Debt,
}

/// Why a sender got no permission to send.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Refusal {
    /// The send lock stayed held past [`SEND_LOCK_WAIT`]: a send is stuck
    /// inside `SendInput`.
    Busy,
    /// Owed key-ups are still refused by `SendInput` (S3).
    Owed,
}

impl Refusal {
    fn as_str(self) -> &'static str {
        match self {
            Self::Busy => "send_lock_busy",
            Self::Owed => "release_owed",
        }
    }
}

/// Keys (and mouse buttons, by their VK) that this program pressed and has
/// not released yet. The executor only reaches the bitmap through this
/// interface: [`Fenced`] marks, the busy guard and the fence read
/// `held_snapshot` to send the MARK key-ups. Implemented by P1b's
/// `keystate::OwnInput` (B-01's 256-bit atomic bitmap), the same record
/// `inject.rs` notes accepted input in, so there is a single source of
/// truth; see the adapter there.
pub(super) trait HeldKeys: Send + Sync {
    fn mark_down(&self, vk: u16);
    fn mark_up(&self, vk: u16);
    /// Currently held keys, ascending.
    fn held_snapshot(&self) -> Vec<u16>;
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
    /// The process-wide [`OwnInput`] (`keystate::own_input`); a test rig
    /// routes its thread to a private one before building the core, and the
    /// worker thread then injects through that one too.
    held: &'static OwnInput,
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
    /// Key-ups of the held keys are owed: a fence decided to release them,
    /// or `SendInput` refused some. Paid only by the payers listed in the
    /// module doc (S3), never by the supervisor's calls.
    release_owed: AtomicBool,
    /// `epoch + 1` of the task holding a clipboard lease; 0 when free.
    clipboard_holder: AtomicU64,
    /// Why the executor is down until the next rebuild.
    fault: PlMutex<Option<ReasonCode>>,
    /// The executor status last observed, as an index into
    /// [`EXECUTOR_STATUSES`]: what `probe` returns while a lock it needs is
    /// held elsewhere (A2).
    last_status: AtomicU8,
    /// Where `probe` reads the clipboard's own health; empty: the
    /// process-wide one ([`try_clipboard_health`]).
    clipboard_probe: ArcSwapOption<ClipboardProbe>,
    /// The clipboard status last read, as an index into
    /// [`CLIPBOARD_STATUSES`]: what `probe` returns while a lease is
    /// updating it (A2).
    last_clipboard: AtomicU8,
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
            held: own_input(),
            origin,
            beat_ns: AtomicU64::new(0),
            life: PlMutex::new(Life::default()),
            next_id: AtomicU64::new(1),
            worker: PlMutex::new(Worker::default()),
            send_lock: PlMutex::new(()),
            release_owed: AtomicBool::new(false),
            clipboard_holder: AtomicU64::new(0),
            fault: PlMutex::new(None),
            last_status: AtomicU8::new(0),
            clipboard_probe: ArcSwapOption::empty(),
            last_clipboard: AtomicU8::new(0),
            #[cfg(test)]
            pause_hook: PlMutex::new(None),
        })
    }

    /// Makes `probe` read the clipboard's own health from `probe` (tests:
    /// the rig's mock clipboard instead of the process-wide health).
    #[cfg(test)]
    pub(super) fn set_clipboard_probe(
        &self,
        probe: impl Fn() -> Option<ComponentHealth> + Send + Sync + 'static,
    ) {
        self.clipboard_probe
            .store(Some(Arc::new(ClipboardProbe(Box::new(probe)))));
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

    /// Holds one of the executor's own locks, as a thread stalled inside it
    /// would (tests).
    #[cfg(test)]
    pub(super) fn hold_lock(&self, lock: CoreLock) -> impl Sized + '_ {
        match lock {
            CoreLock::Life => (Some(self.life.lock()), None, None),
            CoreLock::Worker => (None, Some(self.worker.lock()), None),
            CoreLock::Fault => (None, None, Some(self.fault.lock())),
        }
    }

    /// Whether a fence's key-ups are still unpaid (tests).
    #[cfg(test)]
    pub(super) fn release_owed(&self) -> bool {
        self.release_owed.load(Ordering::SeqCst)
    }

    /// Takes the send lock for task or manual input, waiting at most
    /// `wait`, and pays an outstanding release debt first, inside the same
    /// hold (S3). Err: the caller must not send, because the lock is held
    /// elsewhere (`Busy`) or key-ups are still owed (`Owed`).
    fn lock_send(&self, wait: Duration) -> Result<SendTurn<'_>, Refusal> {
        let turn = self.send_lock.try_lock_for(wait).ok_or(Refusal::Busy)?;
        match self.pay_release(&turn).debt {
            Debt::Settled => Ok(turn),
            Debt::Owed => Err(Refusal::Owed),
        }
    }

    /// Pays the owed key-ups, if any (S3).
    fn pay_release(&self, turn: &SendTurn<'_>) -> Payment {
        if !self.release_owed.load(Ordering::SeqCst) {
            return Payment {
                released: 0,
                debt: Debt::Settled,
            };
        }
        let payment = self.release_all(turn);
        if payment.debt == Debt::Settled {
            tracing::info!(
                event = "executor.release_paid",
                released = payment.released,
                epoch = self.epoch()
            );
        }
        payment
    }

    /// Sends the key-ups of every key in the bitmap and settles the debt by
    /// the result: settled only when none was refused, otherwise the
    /// refused keys stay in the bitmap and stay owed. Clearing the flag here
    /// is safe against a concurrent fence: while `turn` is held no task can
    /// mark a key down (S2 marks only in the send lock), so every key a task
    /// held when the fence recorded its debt was in this snapshot. Outside
    /// the lock the shared record only gains notes `inject.rs` makes after
    /// a send returned: the accepted prefix of a task batch S2 already
    /// marked (if this payment released such a key first, the late note
    /// sets it again: at worst one redundant MARK key-up later), or a
    /// manual run's presses, which that run's own cleanup releases. The
    /// hooks only clear bits.
    fn release_all(&self, turn: &SendTurn<'_>) -> Payment {
        let (released, refused) = self.release_held(turn);
        let debt = if refused == 0 {
            Debt::Settled
        } else {
            Debt::Owed
        };
        self.release_owed
            .store(debt == Debt::Owed, Ordering::SeqCst);
        if debt == Debt::Owed {
            tracing::warn!(
                event = "executor.release_owed",
                released,
                refused,
                epoch = self.epoch(),
                "key-ups refused; they stay owed and input stays refused"
            );
        }
        Payment { released, debt }
    }

    /// The owed key-ups, paid by a payer that may wait for the send lock
    /// (the worker thread, before it serves). Settled when nothing is owed.
    fn pay_owed(&self) -> Debt {
        if !self.release_owed.load(Ordering::SeqCst) {
            return Debt::Settled;
        }
        match self.send_lock.try_lock_for(SEND_LOCK_WAIT) {
            Some(turn) => self.pay_release(&turn).debt,
            None => {
                tracing::warn!(event = "executor.release_deferred", epoch = self.epoch());
                Debt::Owed
            }
        }
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
            .record(task.record(verdict, vec![], StageTimings::default()));
        false
    }

    /// The busy shadow's report sink (S-19, `set_dropped_busy_sink`): each
    /// rule the user typed while an action ran becomes a `dropped_busy`
    /// history entry, like a candidate [`enqueue`](Self::enqueue) drops
    /// busy. It runs on the input consumer thread and must not block: it
    /// takes no executor lock, only the configuration lock (as matching
    /// does) and the runtime's history path.
    pub(super) fn dropped_busy_sink(&self) -> Box<dyn Fn(DroppedBusy) + Send + Sync> {
        let me = self.me.clone();
        Box::new(move |dropped| {
            if let Some(core) = me.upgrade() {
                core.record_dropped_busy(dropped);
            }
        })
    }

    fn record_dropped_busy(&self, dropped: DroppedBusy) {
        // The shadow drops only automatic triggers: the rule's own trigger
        // mode. It just matched, so the rule is in the configuration unless
        // a reload removed it since; it then reads as immediate.
        let mode = self
            .shared
            .compiled
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .config
            .rules
            .iter()
            .find(|rule| rule.id == dropped.rule_id)
            .map(|rule| rule.trigger_mode);
        let task = Task {
            trigger: match mode {
                Some(TriggerMode::Terminator) => TriggerKind::Terminator,
                _ => TriggerKind::Immediate,
            },
            process: Some(dropped.process).filter(|name| !name.is_empty()),
            rule_id: dropped.rule_id,
            queued_at: self.clock.now(),
        };
        let verdict = Verdict::new(Outcome::DroppedBusy, ReasonCode::DroppedBusy, false);
        self.runtime
            .record(task.record(verdict, vec![], StageTimings::default()));
    }

    /// A manual run claims `busy` ([`ManualBusy`]): compare-and-set in one
    /// hold of the lifecycle lock, as [`enqueue`](Self::enqueue) does, and
    /// the claim's id is kept there so only this claim can give it back.
    /// `None` while a task or another manual run holds `busy`.
    fn claim_manual(&self) -> Option<u64> {
        let mut life = self.life.lock();
        self.shared
            .busy
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .ok()?;
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        life.manual = Some(id);
        Some(id)
    }

    /// Gives a manual claim back: in one hold of the lifecycle lock, and
    /// only while claim `id` still owns `busy`. A fence frees `busy` and
    /// drops the claim with it, so a stale claim never frees the `busy` of
    /// a run that started since.
    fn end_manual(&self, id: u64) {
        let mut life = self.life.lock();
        if life.manual.take_if(|manual| *manual == id).is_some() {
            let _ =
                self.shared
                    .busy
                    .compare_exchange(true, false, Ordering::SeqCst, Ordering::SeqCst);
        }
    }

    /// [`ExecutorControl::rebuild`]: makes the executor serve again. Uses
    /// only the executor's own locks, so it is safe without the
    /// supervisor's machine lock and concurrently with `probe` and
    /// `enforce_budget`. It may block (a supervisor attempt, bounded by the
    /// supervisor's attempt timeout): it waits up to [`SEND_LOCK_WAIT`] for
    /// the send lock and pays the owed key-ups inside that hold (S3).
    ///
    /// Fail closed: while a fenced thread is still inside `SendInput` it
    /// gets no lock and refuses, so the executor stays Down until that send
    /// returns, or for good, which ends the supervisor in Failed. When
    /// `SendInput` still refuses an owed key-up it returns Err (the worker
    /// is started all the same; it refuses every task until a payment
    /// succeeds, so the supervisor's retries, or a later task, recover it
    /// once the key-ups are accepted).
    ///
    /// Thread count: a worker thread is only started while this call holds
    /// the send lock, and a thread stuck inside `SendInput` (a task's send,
    /// a payment, or another rebuild's payment) holds that lock, so no
    /// rebuild can pile up threads behind a stuck send; it refuses instead.
    /// A new thread only replaces a worker that is idle or gone (see
    /// [`spawn_worker`](Self::spawn_worker)), and the replaced one exits
    /// once its queue closes.
    fn rebuild_worker(&self) -> Result<(), String> {
        let Some(me) = self.me.upgrade() else {
            return Err("executor is shutting down".into());
        };
        let orphan = {
            let life = self.life.lock();
            let worker = self.worker.lock();
            let live = worker.alive();
            life.running
                .as_ref()
                .map(|running| running.epoch)
                .filter(|&epoch| !(live && epoch == self.epoch()))
        };
        if let Some(epoch) = orphan {
            // The worker died under a task: retire that task first.
            self.fence(epoch);
        }
        let Some(turn) = self.send_lock.try_lock_for(SEND_LOCK_WAIT) else {
            tracing::warn!(event = "executor.rebuild_refused", epoch = self.epoch());
            return Err(
                "a send of a fenced task is still in flight; the executor stays closed".into(),
            );
        };
        let payment = self.pay_release(&turn);
        self.spawn_worker(me, &turn)?;
        drop(turn);
        match payment.debt {
            Debt::Settled => Ok(()),
            Debt::Owed => {
                Err("key-ups of a stopped action are still refused; input stays refused".into())
            }
        }
    }

    /// Starts the worker thread for the current epoch, unless a live worker
    /// is in the middle of one of its tasks (a stall is the budget's call,
    /// not the rebuild's), which is kept. Only creates the thread: it sends
    /// nothing, and the new thread pays any debt itself (S3). `_turn`
    /// proves the send lock is held, which bounds the thread count (see
    /// [`rebuild_worker`](Self::rebuild_worker)). Clears the fault.
    fn spawn_worker(&self, me: Arc<Self>, _turn: &SendTurn<'_>) -> Result<(), String> {
        let life = self.life.lock();
        let mut worker = self.worker.lock();
        if let Some(epoch) = life.running.as_ref().map(|running| running.epoch) {
            if worker.alive() && epoch == self.epoch() {
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
            .spawn(move || {
                // Tests: the tasks' injections (`inject.rs`) are noted in
                // this core's record, as production's are in the one
                // process-wide record.
                #[cfg(test)]
                let _track = super::keystate::track_own_input(me.held);
                me.work(jobs, epoch)
            })
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
    /// send nothing more, owes the key-ups of the keys it left pressed (S3),
    /// frees the busy flag and closes the queue until the next rebuild.
    /// False when the epoch had already moved on. Changes state only: it
    /// never calls the injector and never waits for the send lock.
    pub(super) fn fence(&self, epoch: u64) -> bool {
        let running = {
            let mut life = self.life.lock();
            if self.epoch() != epoch {
                return false;
            }
            self.bump(&mut life, &mut self.worker.lock(), &mut self.fault.lock())
        };
        self.retire(epoch, running);
        true
    }

    /// The fence's decision, in the caller's one hold of the lifecycle lock
    /// (with the epoch checked there): the epoch moves on, the debt is
    /// recorded, `busy` is freed (a manual claim on it, if any, with it),
    /// the queue closes, and taking `running` makes the fence the only
    /// writer of the run's history entry.
    fn bump(
        &self,
        life: &mut Life,
        worker: &mut Worker,
        fault: &mut Option<ReasonCode>,
    ) -> Option<Running> {
        self.epoch.fetch_add(1, Ordering::SeqCst);
        self.shared.busy.store(false, Ordering::SeqCst);
        life.manual = None;
        worker.sender = None;
        *fault = Some(ReasonCode::ExecutorTimeout);
        self.release_owed.store(true, Ordering::SeqCst);
        life.running.take()
    }

    /// After the bump, outside `life`: records the fenced run on behalf of
    /// its thread, which never will. The send lock is only looked at with
    /// `try_lock` and released at once: held means a send of the old epoch
    /// is in flight and may have reached the target.
    fn retire(&self, epoch: u64, running: Option<Running>) {
        self.pause(Pause::FenceDecided);
        let in_flight = self.send_lock.try_lock().is_none();
        if let Some(running) = running {
            let now = self.clock.now();
            let sent = running.sent.load(Ordering::SeqCst) || in_flight;
            self.runtime.record(running.task.record(
                Verdict::new(Outcome::Failed, ReasonCode::ExecutorTimeout, sent),
                vec![],
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
            in_flight,
            "stalled task fenced; its thread may send nothing more, key-ups owed"
        );
    }

    /// Fences the running task when it made no progress within the budget
    /// (`enforce_budget`). Never waits for a lock (A2): each one is only
    /// try_locked, and when one is held elsewhere the round is skipped and
    /// the next tick looks again. The first look only names a suspect; the
    /// decision is taken again in the one hold of `life` that fences (A1):
    /// same epoch, same run in `running`, and its heartbeat's deadline
    /// passed. Only state changes in that hold.
    fn fence_if_stalled(&self, now: Instant) {
        let Some(life) = self.life.try_lock() else {
            return Self::skip_round("life");
        };
        let suspect = life
            .running
            .as_ref()
            .filter(|running| running.epoch == self.epoch())
            .map(|running| (running.epoch, running.id));
        drop(life);
        let Some((epoch, id)) = suspect else {
            return;
        };
        if now.saturating_duration_since(self.last_beat()) <= self.budget {
            return;
        }
        self.pause(Pause::StallCheck);
        let Some(mut life) = self.life.try_lock() else {
            return Self::skip_round("life");
        };
        let same_run = life
            .running
            .as_ref()
            .is_some_and(|running| running.id == id);
        let stalled = now.saturating_duration_since(self.last_beat());
        if self.epoch() != epoch || !same_run || stalled <= self.budget {
            return;
        }
        let (Some(mut worker), Some(mut fault)) = (self.worker.try_lock(), self.fault.try_lock())
        else {
            return Self::skip_round("worker_or_fault");
        };
        let running = self.bump(&mut life, &mut worker, &mut fault);
        drop((fault, worker, life));
        tracing::warn!(
            event = "executor.stalled",
            stalled_ms = stalled.as_millis() as u64,
            budget_ms = self.budget.as_millis() as u64
        );
        self.retire(epoch, running);
    }

    /// A lock the stall check needs is held elsewhere: this round is
    /// skipped, so the fence comes at most one tick (1 s) late, against a
    /// [`STALL_BUDGET`] of 10 s.
    fn skip_round(held: &'static str) {
        tracing::debug!(event = "executor.budget_skipped", held);
    }

    /// Sends key-ups (or button-ups) for every key in [`HeldKeys`] that is
    /// not physically held. The keys come only from `held_snapshot`, and the
    /// release injector is `WinInjector` in production, so every key-up
    /// carries MARK and the hooks ignore it. A key-up the system refuses
    /// stays marked so the next payment retries it. `_turn` proves the send
    /// lock is held (S1), so the snapshot is read inside the same hold that
    /// sends the key-ups, after any send of the old epoch returned (S3).
    /// The key-ups bypass `inject.rs`, so they are bracketed here like every
    /// other injection (P1b H1): a resync sampling the asynchronous key state
    /// meanwhile, or before it caught up, is discarded and retried.
    /// Returns (key-ups accepted, key-ups refused).
    fn release_held(&self, _turn: &SendTurn<'_>) -> (usize, usize) {
        let held = self.held.held_snapshot();
        let _injecting = (!held.is_empty()).then(|| self.held.begin());
        let (mut released, mut refused) = (0, 0);
        for vk in held {
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
                refused += 1;
                tracing::warn!(event = "executor.release_failed", vk);
            }
        }
        (released, refused)
    }

    /// The components' status (`probe`). Never waits for a lock (A2): the
    /// fault and the worker are only try_locked, and while either is held
    /// elsewhere the executor status last observed is returned. The
    /// clipboard is the worse of a lease held by a fenced task and the
    /// clipboard's own health ([`Self::clipboard_reported`]).
    fn status(&self) -> Vec<(Component, Status)> {
        let fault = self.fault.try_lock().map(|fault| *fault);
        let alive = self.worker.try_lock().map(|worker| worker.alive());
        let executor = match (fault, alive) {
            (Some(fault), Some(alive)) => {
                let status = match fault {
                    Some(reason) => Status::Down(reason),
                    // Key-ups owed (S3): a send is stuck holding the lock, or
                    // `SendInput` refuses them; no input is sent until they
                    // are paid.
                    None if self.release_owed.load(Ordering::SeqCst) => {
                        Status::Down(ReasonCode::ExecutorTimeout)
                    }
                    None if !alive => Status::Down(ReasonCode::Internal),
                    None => Status::Ok,
                };
                self.last_status
                    .store(status_index(status), Ordering::SeqCst);
                status
            }
            _ => {
                tracing::debug!(event = "executor.probe_cached");
                EXECUTOR_STATUSES[usize::from(self.last_status.load(Ordering::SeqCst))]
            }
        };
        let holder = self.clipboard_holder.load(Ordering::SeqCst);
        let held = if holder != 0 && holder - 1 < self.epoch() {
            Status::Degraded(ReasonCode::ClipboardBusy)
        } else {
            Status::Ok
        };
        vec![
            (Component::Executor, executor),
            (Component::Clipboard, worse(held, self.clipboard_reported())),
        ]
    }

    /// The clipboard's own health (`clipboard_health`, B-08: Degraded after
    /// a lease found it busy, held it too long or met a hung owner; Healthy
    /// again after the next lease that kept every section in time) as a
    /// status. Never waits (A2): while a lease is updating it, the status
    /// last read is returned.
    fn clipboard_reported(&self) -> Status {
        let probe = self.clipboard_probe.load();
        let health = match probe.as_deref() {
            Some(probe) => (probe.0)(),
            None => try_clipboard_health(),
        };
        match health {
            Some(health) => {
                let status = Status::from_health(&health);
                self.last_clipboard
                    .store(clipboard_index(status), Ordering::SeqCst);
                status
            }
            None => {
                tracing::debug!(event = "executor.clipboard_probe_cached");
                CLIPBOARD_STATUSES[usize::from(self.last_clipboard.load(Ordering::SeqCst))]
            }
        }
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
        // S3 (b): a new worker pays owed key-ups before it serves anything.
        // Unpaid (refused, or the lock stuck), they stay owed and each task
        // tries again before it may send.
        self.pay_owed();
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
                    vec![],
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
            // The run's heartbeat starts in the hold that registers it: a
            // stall check that sees this run sees its fresh deadline (A1).
            self.beat();
        }
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
            clipboard_refused: AtomicBool::new(false),
        });
        // S3 (b): owed key-ups are paid before the task may do anything.
        // While they stay owed the task sends nothing, touches nothing and
        // fails at once. Paid before the trace starts, so the payment's
        // key-ups are not counted as this run's side effects.
        let owed = self.pay_owed() == Debt::Owed;
        if owed {
            scope.refused.store(true, Ordering::SeqCst);
        }
        let installed = ScopeLease::install(Arc::clone(&scope));
        begin_trace();
        set_stage(Stage::Resolve, started);
        let outcome = if owed {
            Ok(RunReport::text(Err(RELEASE_OWED.into())))
        } else {
            catch_unwind(AssertUnwindSafe(|| (self.runner)(&candidate, &scope)))
        };
        drop(installed);
        let end = self.clock.now();
        let stage = current_stage();
        let mut panicked = false;
        let (result, reported, skipped_formats) = match outcome {
            Ok(report) => (report.result, report.reason, report.skipped_formats),
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
                (Err(PANICKED.into()), None, vec![])
            }
        };
        // The clipboard port's refusal is text to the run (the port is
        // `Fn() -> Result<C, String>`); the scope remembers it as a reason.
        let reported = reported.or_else(|| {
            scope
                .clipboard_refused
                .load(Ordering::SeqCst)
                .then_some(ReasonCode::ClipboardBusy)
        });
        // Read before the guard sends its key-ups (`released` counts those).
        let effect = SENT_EFFECT.with(Cell::get);
        let released = guard.release();
        // A refused run pressed nothing: key-ups its guard paid belong to the
        // fenced task that owed them.
        let sent = effect || scope.sent.load(Ordering::SeqCst) || (released > 0 && !owed);
        let verdict = if panicked {
            Verdict::new(Outcome::Failed, ReasonCode::ExecutorPanic, sent)
        } else if scope.refused.load(Ordering::SeqCst) {
            // A send found the lock held past SEND_LOCK_WAIT (S1), or the
            // run was refused for owed key-ups (S3).
            Verdict::new(Outcome::Failed, ReasonCode::ExecutorTimeout, sent)
        } else {
            classify(&result, reported, stage, sent)
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
        guard.settle(verdict, skipped_formats, Some(last), end);
    }
}

/// Ends a task in two steps, each comparing before it clears, so the guard
/// of a fenced (or otherwise replaced) task never touches a newer run.
/// * [`release`](Self::release): under the send lock, while the epoch is
///   still the task's, the MARK key-ups for every key in the bitmap (the
///   keys it left pressed, and any owed ones); a key-up `SendInput` refuses
///   stays owed (S3). A fenced task only pays the debt, if any (payer (a)).
///   Then its clipboard claim (its own token only) goes back. Without the
///   lock nothing is sent: the key-ups are owed and the executor goes Down.
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
            // Its epoch current: every held key, owed or its own. Fenced:
            // the fence owes its keys; pay that debt, nothing more.
            Some(turn) if core.epoch() == self.epoch => core.release_all(&turn).released,
            Some(turn) => core.pay_release(&turn).released,
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

    fn settle(
        &mut self,
        verdict: Verdict,
        skipped_formats: Vec<u32>,
        last: Option<ExecutionResult>,
        end: Instant,
    ) {
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
        core.runtime
            .record(running.task.record(verdict, skipped_formats, timings));
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
            vec![],
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
    /// [`claim_clipboard`](Self::claim_clipboard) refused the clipboard: the
    /// run's reason is `ClipboardBusy`, whatever text the refusal became.
    clipboard_refused: AtomicBool,
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
            self.clipboard_refused.store(true, Ordering::SeqCst);
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
        let _turn = match core.lock_send(SEND_LOCK_WAIT) {
            Ok(turn) => turn,
            Err(refusal) => {
                scope.refused.store(true, Ordering::SeqCst);
                if refusal == Refusal::Busy {
                    core.fail_closed(scope.epoch, false);
                }
                return 0;
            }
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
/// executor's send lock and pays owed key-ups first (S3), so manual output
/// never interleaves with a task's or a payment's (S1). It refuses (0
/// accepted, which fail-stops the run) while the lock stays held or
/// key-ups stay owed; the run is then recorded as Failed/ExecutorTimeout,
/// like a refused task ([`record_manual`]). Without an executor (`core` is
/// `None`: not started, or direct pipeline tests) there is nothing to
/// exclude and it sends directly. Manual runs carry no epoch and keep their
/// own key cleanup, so this wrapper does not touch the bitmap; `inject.rs`
/// still notes their accepted input in the shared record, so a key a manual
/// run failed to release is also released when the next task ends or a
/// debt is paid.
pub(super) struct Serialized<'a, I> {
    inner: &'a I,
    core: Option<Arc<ExecutorCore>>,
}

thread_local! {
    /// A send of the current manual run was refused by the send exclusion.
    /// Reset when its [`Serialized`] is made, read by [`record_manual`].
    static MANUAL_REFUSED: Cell<bool> = const { Cell::new(false) };
}

impl<'a, I: Injector> Serialized<'a, I> {
    pub(super) fn new(inner: &'a I, core: Option<Arc<ExecutorCore>>) -> Self {
        MANUAL_REFUSED.with(|refused| refused.set(false));
        Self { inner, core }
    }

    fn send(&self, send: impl FnOnce() -> u32) -> u32 {
        let Some(core) = &self.core else {
            return send();
        };
        let _turn = match core.lock_send(SEND_LOCK_WAIT) {
            Ok(turn) => turn,
            Err(refusal) => {
                MANUAL_REFUSED.with(|refused| refused.set(true));
                tracing::warn!(
                    event = "executor.manual_send_refused",
                    reason = refusal.as_str(),
                    "manual input not sent"
                );
                return 0;
            }
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

// Pure forwarding, defaulted methods included: falling back to a trait
// default here would silently drop the backend's own behavior (format
// registration, hung-owner check, health, metafile staging, mock time).
impl<C: ClipboardRead> ClipboardRead for Claimed<C> {
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
    fn read_format(&self, format: u32, limit: usize) -> crate::traits::FormatRead {
        self.inner.read_format(format, limit)
    }
    fn now(&self) -> Instant {
        self.inner.now()
    }
}

impl<C: ClipboardBackend> ClipboardBackend for Claimed<C> {
    type Staged = C::Staged;
    type Reader = C::Reader;
    fn identity(&self) -> u64 {
        self.inner.identity()
    }
    fn reader(&self) -> Self::Reader {
        self.inner.reader()
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
    fn pause(&self, duration: Duration) {
        self.inner.pause(duration)
    }
    fn wait_for<T>(
        &self,
        rx: &std::sync::mpsc::Receiver<T>,
        timeout: Duration,
    ) -> Result<T, std::sync::mpsc::RecvTimeoutError> {
        self.inner.wait_for(rx, timeout)
    }
    fn report_health(
        &self,
        state: crate::health::HealthState,
        reason: Option<crate::health::ReasonCode>,
    ) {
        self.inner.report_health(state, reason)
    }
    fn stage_format(&self, format: u32, bytes: &[u8]) -> Result<(Self::Staged, Vec<u8>), String> {
        self.inner.stage_format(format, bytes)
    }
    fn owner_hung(&self, owner: u64) -> bool {
        self.inner.owner_hung(owner)
    }
    fn register_format(&self, name: &str) -> u32 {
        self.inner.register_format(name)
    }
}

impl ExecutorControl for ExecutorCore {
    /// State only (S3): never calls the injector, never waits for the send
    /// lock. Owed key-ups are left to their payers. Never waits for an
    /// executor lock either (A2): a round that finds one held is skipped.
    fn enforce_budget(&self, now: Instant) {
        self.fence_if_stalled(now);
    }
    /// Never waits for a lock (A2); while one is held elsewhere it returns
    /// the executor status last observed.
    fn probe(&self, _now: Instant) -> Vec<(Component, Status)> {
        self.status()
    }
    fn rebuild(&self) -> Result<(), String> {
        self.rebuild_worker()
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
        Ok(plan) => run_plan(
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
        Err(errors) => RunReport::text(Err(errors
            .iter()
            .map(|e| e.message.clone())
            .collect::<Vec<_>>()
            .join("; "))),
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
    if let Err(error) = core.rebuild() {
        tracing::error!(event = "executor.start_failed", %error);
    }
    // S-19: rules typed while an action runs reach the history from now on.
    set_dropped_busy_sink(core.dropped_busy_sink());
    EXECUTOR.store(Some(core));
}

/// The executor as the supervisor drives it, for `Supervisor::new` (P5).
/// `None` before `start_engine`.
pub fn executor_control() -> Option<Arc<dyn ExecutorControl>> {
    EXECUTOR
        .load_full()
        .map(|core| core as Arc<dyn ExecutorControl>)
}

/// A manual run's claim on `shared.busy` (`src-tauri`'s `execute_action`).
/// [`claim`](Self::claim) compares and sets `busy` under the executor's
/// lifecycle lock, like a task's [`enqueue`](ExecutorCore::enqueue), and
/// dropping the claim compares and clears it under the same lock, only
/// while this claim still owns `busy`, so it never frees a newer run's.
/// The run's output still goes through [`Serialized`] (the send lock, owed
/// key-ups paid first). Without the executor of this `shared` (before
/// `start_engine`) there is no lock to take: the claim is a bare
/// compare-and-set on `busy`, and nothing else writes `busy` meanwhile.
#[must_use = "dropping the claim frees `busy` at once"]
pub struct ManualBusy {
    shared: Arc<Shared>,
    /// The executor that granted the claim, and the claim's id.
    grant: Option<(Arc<ExecutorCore>, u64)>,
}

impl ManualBusy {
    /// Claims `busy` for a manual run; `None` while a task or another
    /// manual run holds it.
    pub fn claim(shared: &Arc<Shared>) -> Option<Self> {
        let core = EXECUTOR
            .load_full()
            .filter(|core| Arc::ptr_eq(&core.shared, shared));
        Self::claim_on(core, shared)
    }

    /// [`claim`](Self::claim) on a given executor (tests pass their rig's).
    pub(super) fn claim_on(core: Option<Arc<ExecutorCore>>, shared: &Arc<Shared>) -> Option<Self> {
        let grant = match core {
            Some(core) => {
                let id = core.claim_manual()?;
                Some((core, id))
            }
            None => {
                shared
                    .busy
                    .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                    .ok()?;
                None
            }
        };
        Some(Self {
            shared: Arc::clone(shared),
            grant,
        })
    }
}

impl Drop for ManualBusy {
    fn drop(&mut self) {
        match &self.grant {
            Some((core, id)) => core.end_manual(*id),
            None => {
                let _ = self.shared.busy.compare_exchange(
                    true,
                    false,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                );
            }
        }
    }
}
