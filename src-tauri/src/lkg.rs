//! Last-known-good tracking (B-04, plan §7.2 LKG): records the active
//! revision as last-known-good ([`crate::store::Store::mark_last_good`]) once
//! it has proven itself, so that a corrupt `active.json` recovers to a
//! configuration that worked (`recovered`, `from: last_good`) rather than
//! just the newest snapshot.
//!
//! # Condition
//!
//! The active revision qualifies once, since it took effect,
//!
//! - at least one expansion succeeded on the trigger path: a history entry
//!   with trigger `immediate` or `terminator` and outcome `succeeded`,
//!   recorded after this store state took effect (see "Which revision a
//!   run belongs to"), and
//! - overall Health has read Healthy (「正常」) for [`QUALIFY_AFTER`] without
//!   a break, and the runtime's own Health still reads Healthy when the mark
//!   is about to be written (see "Live Health before the write").
//!
//! The two may come in either order. Manual runs (`execute_action`, trigger
//! `manual`) do not count: they run an action the frontend hands over (often
//! from the unsaved draft) without the hook, the buffer or matching, and
//! with no rule id, so they say nothing about whether the active revision
//! works. Degraded is not Healthy and does not count either.
//!
//! # Which revision a run belongs to
//!
//! History entries carry no revision, and the bus gets them from the
//! runtime's event thread, which is not ordered with the store's reports: a
//! success of revision n can reach the tracker after the report of n+1
//! (review finding 1). So a store state counts only entries whose `seq` is
//! above the runtime's last `seq` ([`Runtime::history_seq`]: monotonic,
//! kept across a history clear) as read when the tracker takes that state
//! on the bus thread.
//!
//! - The store reports a new state only after it installed the new
//!   configuration, so every entry recorded before the install is at or
//!   below that mark and is ignored however late it arrives.
//! - An entry above the mark was recorded after the install, and its run
//!   executed the new configuration. A publish is refused (`busy`) while a
//!   run is queued or running (`busy` is set when a candidate is queued and
//!   cleared after its entry is recorded). Before that check the publish
//!   holds MANUAL (detection stops, an automatic run stops at its next
//!   step), sets cancel and moves the input generation (a candidate
//!   detected before stops at its first step). MANUAL is released only
//!   after the install, so a candidate detected after that was matched
//!   against the new configuration.
//! - The mark can only under-count: a success recorded between the install
//!   and the bus taking the report is ignored, and the revision waits for
//!   the next one. (If that success reaches the tracker before the report,
//!   it counts for the old state, whose mark is then refused as
//!   `revision_changed`: the revision changed under the store lock that
//!   [`mark_active`] takes.)
//!
//! Neither the history entry nor any serde shape changes. Timestamps are not
//! compared: an entry's `at_ms` is wall-clock, and both it and the ring's
//! monotonic instant are taken when the entry is recorded, at the end of
//! the run. The run's start could only be derived from them as an
//! over-estimate, which errs the unsafe way.
//!
//! # Live Health before the write
//!
//! The minute is measured on the Health the bus delivers, and the bus can
//! be behind the runtime. An observer ahead of the tracker (the tray, which
//! waits for the main thread) can hold the bus while a Failed waits in its
//! queue (review finding 2). So right before the mark the tracker reads the
//! runtime's current Health ([`Runtime::health`], the snapshot last
//! published). If it is not Healthy, nothing is written or logged and the
//! minute starts again from now. The queued Health stops the minute when it
//! arrives, until Healthy returns. The runtime offers no "Healthy since"
//! time or Health generation to compare with (its version counter is
//! private and shared with State), so only the current state is checked.
//!
//! # Resets
//!
//! - A new store state (another revision or safe mode: publish, rollback, a
//!   published import, a SafeMode(Locked) retry) clears the success count,
//!   moves the history mark (above) and restarts the minute (from that
//!   moment, if Health reads Healthy).
//! - Health leaving Healthy stops the minute; it starts again from zero
//!   when Health is Healthy again. Successes belong to the revision and are
//!   kept. The live check finding the runtime not Healthy restarts the
//!   minute too.
//! - In safe mode nothing is counted and nothing is marked.
//!
//! A revision is marked at most once per session, and a store state is
//! tried at most once: a refused mark (safe mode, the revision is no longer
//! active, the active configuration was never written, an I/O error) logs
//! one `store.last_good_skipped` at debug and is not retried until the
//! store state changes. No error is logged here and nothing loops; an I/O
//! error is still logged by the store itself (`command.io`, warn).
//!
//! # Thread
//!
//! The tracker is an EventBus [`Observer`] and runs on its thread
//! (`qt-event-bus`), where Health (before the bus's throttle), every history
//! entry and every store-state report arrive, in order, on one channel. The
//! minute is a deadline the bus sleeps until ([`Observer::due_in`]), not a
//! poll. The mark ([`mark_active`]) takes the store lock, which the hook and
//! executor never take, and writes `store-meta.json` (and
//! `revisions/<n>.json` when missing): never on the hook, executor or main
//! thread, never under a lock of the input path. Reading the runtime's
//! Health or history `seq` takes its health or history lock briefly, with
//! no other lock held (the store lock is taken only afterwards, by the
//! mark). The clock is the runtime's (monotonic, injectable).
//!
//! # Limits (待实测)
//!
//! - Health is latest-value, and the write checks only the current Health:
//!   a dip the runtime merges away before the bus sees it, or one that
//!   starts and ends entirely while the bus is held (by the tray, say),
//!   does not restart the minute.
//! - Health can still change in the short window between the live check
//!   and the write (the wait for the store lock).
//! - A success recorded between a publish's install and the bus taking its
//!   report is not counted (see above).
//! - Only platforms that record trigger-path history (Windows) ever mark.
//! - The real timing (60 s of Healthy under the production supervisor, real
//!   expansions reaching the bus) is verified on a real machine only.
use crate::{
    app_state::Managed,
    error::CommandError,
    events::{Observer, StoreState},
};
use quicker_platform::{
    health::{EngineState, HealthSnapshot, HealthState, HistoryEntry, Outcome, TriggerKind},
    runtime::Runtime,
    traits::Clock,
};
use std::{
    sync::{Arc, Weak},
    time::{Duration, Instant},
};

/// How long Health must read Healthy, without a break, on the active
/// revision before it can be marked.
pub const QUALIFY_AFTER: Duration = Duration::from_secs(60);

/// Records a revision as last-known-good. Production: [`mark_in`].
pub type MarkFn = Box<dyn FnMut(u64) -> Result<(), CommandError> + Send>;

pub struct LkgTracker {
    /// Weak, as the bus's own reference: the runtime holds the bus's
    /// listener, and the bus owns this tracker.
    runtime: Weak<Runtime>,
    clock: Arc<dyn Clock>,
    mark: MarkFn,
    /// The store state the counting is for.
    store: StoreState,
    /// The runtime's last history `seq` when this store state was taken:
    /// only entries above it count for it.
    counted_after: u64,
    /// Since when Health has read Healthy for this store state; `None`
    /// while it does not.
    healthy_since: Option<Instant>,
    /// Trigger-path successes since this store state took effect.
    successes: u32,
    /// This store state was already tried (marked or refused).
    tried: bool,
    /// The revision this tracker last marked.
    marked: Option<u64>,
}

impl LkgTracker {
    /// `store` is the store state when the tracker starts (setup takes it
    /// under the same store lock as the bus's baseline, before the engine
    /// starts); the clock is `runtime`'s.
    pub fn new(runtime: &Arc<Runtime>, store: StoreState, mark: MarkFn) -> Self {
        Self {
            runtime: Arc::downgrade(runtime),
            clock: runtime.clock().clone(),
            mark,
            store,
            counted_after: runtime.history_seq(),
            healthy_since: None,
            successes: 0,
            tried: false,
            marked: None,
        }
    }

    /// The runtime's last history `seq`; once the runtime is gone, the
    /// highest value, so nothing more counts.
    fn history_seq(&self) -> u64 {
        self.runtime
            .upgrade()
            .map_or(u64::MAX, |runtime| runtime.history_seq())
    }

    /// Whether the runtime's current Health (not the bus's, which can be
    /// behind) reads Healthy; false once the runtime is gone.
    fn healthy_now(&self) -> bool {
        self.runtime
            .upgrade()
            .is_some_and(|runtime| runtime.health().state == HealthState::Healthy)
    }

    /// The time left until the current revision qualifies, when time is
    /// all that is missing; `None` when something else is (or it is done).
    fn remaining(&self) -> Option<Duration> {
        let waiting = self.store.safe_mode.is_none()
            && !self.tried
            && self.marked != Some(self.store.revision)
            && self.successes > 0;
        let since = self.healthy_since.filter(|_| waiting)?;
        let healthy_for = self.clock.now().saturating_duration_since(since);
        Some(QUALIFY_AFTER.saturating_sub(healthy_for))
    }

    /// Marks the current revision if it qualifies now.
    fn poll(&mut self) {
        if self.remaining() != Some(Duration::ZERO) {
            return;
        }
        // The minute ran on the Health the bus delivered; a Health that left
        // Healthy may still wait behind it. Not written, not tried: the
        // minute starts again (and the queued Health, once it arrives,
        // stops it).
        if !self.healthy_now() {
            self.healthy_since = Some(self.clock.now());
            return;
        }
        let revision = self.store.revision;
        // Set first: a mark that panics is not retried either.
        self.tried = true;
        match (self.mark)(revision) {
            Ok(()) => self.marked = Some(revision),
            Err(e) => tracing::debug!(
                event = "store.last_good_skipped",
                revision,
                reason = refusal(&e),
                "last-known-good not recorded"
            ),
        }
    }
}

impl Observer for LkgTracker {
    fn status(&mut self, health: &HealthSnapshot, _state: &EngineState) {
        if health.state != HealthState::Healthy {
            self.healthy_since = None;
        } else if self.healthy_since.is_none() {
            self.healthy_since = Some(self.clock.now());
        }
        self.poll();
    }

    fn executed(&mut self, entry: &HistoryEntry) {
        // A run recorded before this store state was taken (a late
        // delivery of the previous revision's) does not count.
        let counts = entry.seq > self.counted_after
            && entry.trigger != TriggerKind::Manual
            && entry.outcome == Outcome::Succeeded;
        if counts && self.store.safe_mode.is_none() {
            self.successes = self.successes.saturating_add(1);
        }
        self.poll();
    }

    fn store(&mut self, store: StoreState) {
        if store != self.store {
            self.store = store;
            // Read after the store installed this state (it reports only
            // then): every entry of the previous one is at or below it.
            self.counted_after = self.history_seq();
            self.successes = 0;
            self.tried = false;
            if self.healthy_since.is_some() {
                self.healthy_since = Some(self.clock.now());
            }
        }
        self.poll();
    }

    fn due_in(&self) -> Option<Duration> {
        self.remaining()
    }

    fn tick(&mut self) {
        self.poll();
    }
}

/// The `reason` of `store.last_good_skipped`: a fixed label, never the
/// error's text.
fn refusal(e: &CommandError) -> &'static str {
    match e {
        CommandError::SafeMode { .. } => "safe_mode",
        CommandError::RevisionConflict { .. } => "revision_changed",
        CommandError::NotFound { .. } => "not_found",
        CommandError::Io { .. } => "io",
        _ => "other",
    }
}

/// Records `revision` as last-known-good if it is still the active one
/// (a publish may have overtaken the tick: `revision_conflict` with the
/// current revision). Safe mode is refused by the store
/// ([`crate::store::Store::mark_last_good`]).
pub fn mark_active(store: &Managed, revision: u64) -> Result<(), CommandError> {
    let store = store.lock();
    let current = store.store_state().revision;
    if current != revision {
        return Err(CommandError::RevisionConflict { current });
    }
    store.mark_last_good(revision)
}

/// The production [`MarkFn`]: [`mark_active`] on the app's store, which is
/// managed before the EventBus starts.
pub fn mark_in<R: tauri::Runtime>(app: tauri::AppHandle<R>) -> MarkFn {
    Box::new(move |revision| {
        use tauri::Manager;
        mark_active(&app.state::<Managed>(), revision)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        diagnostics::logging::test_support::{read_logs, scoped},
        error::{EntityKind, SafeModeReason},
        events::{EventBus, Observers, Sink, ENGINE_HEALTH},
        startup::ProblemKind,
        store::{RecoverySource, Store},
    };
    use parking_lot::Mutex;
    use quicker_domain::*;
    use quicker_platform::{
        health::{EngineEvent, PauseReasons, ReasonCode},
        history::Record,
    };
    use serde_json::Value;
    use std::{
        fs,
        path::PathBuf,
        sync::{
            atomic::{AtomicBool, Ordering},
            mpsc,
        },
    };
    use tracing::level_filters::LevelFilter;

    /// A monotonic clock that only moves when told to.
    struct FakeClock {
        base: Instant,
        offset: Mutex<Duration>,
    }
    impl FakeClock {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                base: Instant::now(),
                offset: Mutex::new(Duration::ZERO),
            })
        }
        fn advance(&self, by: Duration) {
            *self.offset.lock() += by;
        }
    }
    impl Clock for FakeClock {
        fn now(&self) -> Instant {
            self.base + *self.offset.lock()
        }
        fn wall_ms(&self) -> i64 {
            1_700_000_000_000 + i64::try_from(self.offset.lock().as_millis()).unwrap()
        }
        fn sleep(&self, duration: Duration) {
            self.advance(duration);
        }
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn health(state: HealthState) -> HealthSnapshot {
        HealthSnapshot {
            state,
            components: vec![],
        }
    }

    fn store(revision: u64, safe_mode: Option<SafeModeReason>) -> StoreState {
        StoreState {
            revision,
            safe_mode,
        }
    }

    /// A run recorded in `runtime`'s history, which gives it its `seq`.
    fn record(runtime: &Runtime, trigger: TriggerKind, outcome: Outcome) -> HistoryEntry {
        let rule = if trigger == TriggerKind::Manual {
            ""
        } else {
            "rule"
        };
        runtime.record(Record::new(rule, trigger, outcome, ReasonCode::Ok))
    }

    /// A tracker on its own runtime (on a fake clock), driven by hand the
    /// way the bus drives it.
    struct Rig {
        clock: Arc<FakeClock>,
        runtime: Arc<Runtime>,
        tracker: LkgTracker,
        marks: Arc<Mutex<Vec<u64>>>,
    }

    impl Rig {
        /// Marks are accepted and listed in [`Rig::marks`].
        fn new(at: StoreState) -> Self {
            let marks = Arc::new(Mutex::new(vec![]));
            let seen = Arc::clone(&marks);
            let mut rig = Self::with(
                at,
                Box::new(move |revision| {
                    seen.lock().push(revision);
                    Ok(())
                }),
            );
            rig.marks = marks;
            rig
        }

        fn with(at: StoreState, mark: MarkFn) -> Self {
            let clock = FakeClock::new();
            let runtime = Arc::new(Runtime::new(clock.clone()));
            let tracker = LkgTracker::new(&runtime, at, mark);
            Self {
                clock,
                runtime,
                tracker,
                marks: Arc::default(),
            }
        }

        fn marks(&self) -> Vec<u64> {
            self.marks.lock().clone()
        }

        /// Health published by the supervisor and delivered by the bus at
        /// once: the runtime and the tracker agree.
        fn health(&mut self, state: HealthState) {
            self.runtime.publish_health(health(state));
            self.tracker.status(&health(state), &EngineState::default());
        }

        fn healthy(&mut self) {
            self.health(HealthState::Healthy);
        }

        /// A run recorded and delivered at once.
        fn run(&mut self, trigger: TriggerKind, outcome: Outcome) {
            let entry = record(&self.runtime, trigger, outcome);
            self.tracker.executed(&entry);
        }

        fn success(&mut self) {
            self.run(TriggerKind::Terminator, Outcome::Succeeded);
        }

        fn store(&mut self, at: StoreState) {
            self.tracker.store(at);
        }

        /// Moves the clock and gives the tracker the tick the bus would.
        fn wait(&mut self, by: Duration) {
            self.clock.advance(by);
            self.tracker.tick();
        }

        fn due_in(&self) -> Option<Duration> {
            self.tracker.due_in()
        }
    }

    #[test]
    fn sixty_healthy_seconds_and_one_success_mark_the_revision_once() {
        let mut rig = Rig::new(store(1, None));
        rig.healthy();
        assert_eq!(rig.due_in(), None, "no success yet: nothing to wait for");
        rig.wait(secs(10));
        rig.success();
        assert_eq!(rig.due_in(), Some(secs(50)), "the bus sleeps until then");
        rig.wait(secs(50) - Duration::from_millis(1));
        assert!(rig.marks().is_empty(), "not before 60 s");
        assert_eq!(rig.due_in(), Some(Duration::from_millis(1)));
        rig.wait(Duration::from_millis(1));
        assert_eq!(rig.marks(), [1]);
        // Exactly once: more successes, time and Health repeats change
        // nothing, and nothing is left to wait for.
        assert_eq!(rig.due_in(), None);
        rig.success();
        rig.healthy();
        rig.store(store(1, None));
        rig.wait(secs(600));
        assert_eq!(rig.marks(), [1]);
    }

    #[test]
    fn health_leaving_healthy_at_59s_restarts_the_full_minute() {
        let mut rig = Rig::new(store(1, None));
        rig.healthy();
        rig.success();
        rig.wait(secs(59));
        rig.health(HealthState::Degraded);
        assert_eq!(rig.due_in(), None, "no minute runs while not Healthy");
        rig.wait(secs(30));
        // Recovering is not Healthy either.
        rig.health(HealthState::Recovering);
        rig.wait(secs(30));
        assert!(rig.marks().is_empty());
        // Back to Healthy: a fresh 60 s, the success still counts.
        rig.healthy();
        assert_eq!(rig.due_in(), Some(secs(60)));
        rig.wait(secs(59));
        assert!(
            rig.marks().is_empty(),
            "the 59 s before the dip do not add up"
        );
        rig.wait(secs(1));
        assert_eq!(rig.marks(), [1]);
    }

    #[test]
    fn without_a_trigger_path_success_nothing_is_marked() {
        let mut rig = Rig::new(store(1, None));
        rig.healthy();
        for outcome in [
            Outcome::Unknown,
            Outcome::Failed,
            Outcome::Skipped,
            Outcome::Cancelled,
            Outcome::DroppedBusy,
        ] {
            rig.run(TriggerKind::Immediate, outcome);
        }
        // Manual runs do not exercise the active revision.
        rig.run(TriggerKind::Manual, Outcome::Succeeded);
        rig.wait(secs(600));
        assert_eq!(rig.due_in(), None);
        assert!(rig.marks().is_empty());
        // The success may also come after the minute: it marks at once.
        rig.run(TriggerKind::Immediate, Outcome::Succeeded);
        assert_eq!(rig.marks(), [1]);
    }

    #[test]
    fn a_publish_restarts_the_minute_and_the_new_revision_is_marked_once_it_qualifies() {
        let mut rig = Rig::new(store(1, None));
        rig.healthy();
        rig.success();
        rig.wait(secs(40));
        // Published at 40 s: revision 1 never qualified; its success and
        // its 40 s do not carry over.
        rig.store(store(2, None));
        rig.wait(secs(30));
        assert_eq!(rig.due_in(), None, "no success on revision 2 yet");
        rig.success();
        assert_eq!(rig.due_in(), Some(secs(30)), "60 s from the publish");
        rig.wait(secs(29));
        assert!(rig.marks().is_empty());
        rig.wait(secs(1));
        assert_eq!(rig.marks(), [2]);
        // The next publish (or a rollback, which is a new revision too)
        // starts over and is marked once it qualifies in turn.
        rig.store(store(3, None));
        rig.success();
        rig.wait(secs(59));
        assert_eq!(rig.marks(), [2]);
        rig.wait(secs(1));
        assert_eq!(rig.marks(), [2, 3]);
    }

    /// Review finding 1: entries reach the bus from the runtime's event
    /// thread, unordered with the store's reports. A success of revision 1,
    /// recorded before the publish of 2 but delivered after its report, is
    /// not revision 2's.
    #[test]
    fn a_success_recorded_before_a_publish_never_counts_for_the_new_revision() {
        let mut rig = Rig::new(store(1, None));
        rig.healthy();
        let late = record(&rig.runtime, TriggerKind::Terminator, Outcome::Succeeded);
        rig.store(store(2, None));
        rig.tracker.executed(&late);
        assert_eq!(rig.due_in(), None, "revision 2 has no success of its own");
        rig.wait(secs(600));
        assert!(rig.marks().is_empty());
        // A run recorded after the report is revision 2's.
        rig.success();
        assert_eq!(rig.marks(), [2]);
        // The same holds when safe mode ends at the same revision: what
        // was recorded before does not count after.
        rig.store(store(3, Some(SafeModeReason::Locked)));
        let late = record(&rig.runtime, TriggerKind::Terminator, Outcome::Succeeded);
        rig.store(store(3, None));
        rig.tracker.executed(&late);
        rig.wait(secs(600));
        assert_eq!(rig.marks(), [2]);
    }

    /// Review finding 2: the bus can be behind the runtime (an observer
    /// ahead of the tracker blocks while a Failed waits in the queue). The
    /// write reads the runtime's current Health: not Healthy, nothing is
    /// written and the minute starts again.
    #[test]
    fn a_health_the_bus_has_not_delivered_yet_stops_the_write_and_restarts_the_minute() {
        let mut rig = Rig::new(store(1, None));
        rig.healthy();
        rig.success();
        rig.wait(secs(59));
        // The runtime reads Failed; the bus has not delivered it.
        rig.runtime.publish_health(health(HealthState::Failed));
        rig.wait(secs(2));
        assert!(rig.marks().is_empty(), "the runtime no longer read Healthy");
        assert_eq!(
            rig.due_in(),
            Some(secs(60)),
            "a fresh minute from the check"
        );
        // Healthy again before the bus ever showed the dip (merged away):
        // still a full minute from the check.
        rig.runtime.publish_health(health(HealthState::Healthy));
        rig.wait(secs(59));
        assert!(rig.marks().is_empty());
        rig.wait(secs(1));
        assert_eq!(rig.marks(), [1]);
        // Revision 2: this time the Failed does arrive, later; it stops
        // the minute until Healthy returns.
        rig.store(store(2, None));
        rig.success();
        rig.wait(secs(59));
        rig.runtime.publish_health(health(HealthState::Failed));
        rig.wait(secs(1));
        assert_eq!(rig.due_in(), Some(secs(60)));
        rig.tracker
            .status(&health(HealthState::Failed), &EngineState::default());
        assert_eq!(rig.due_in(), None);
        rig.wait(secs(600));
        assert_eq!(rig.marks(), [1]);
        rig.healthy();
        rig.wait(secs(60));
        assert_eq!(rig.marks(), [1, 2]);
    }

    #[test]
    fn nothing_is_counted_or_marked_in_safe_mode() {
        let crash_loop = Some(SafeModeReason::CrashLoop);
        let mut rig = Rig::new(store(1, crash_loop));
        rig.healthy();
        rig.success();
        rig.wait(secs(600));
        assert_eq!(rig.due_in(), None);
        assert!(rig.marks().is_empty());
        // Safe mode ends at the same revision: what happened in it does
        // not count, the minute starts now.
        rig.store(store(1, None));
        rig.wait(secs(600));
        assert!(rig.marks().is_empty(), "the success was in safe mode");
        rig.success();
        assert_eq!(rig.marks(), [1], "healthy for 600 s since safe mode ended");
        assert_eq!(rig.due_in(), None);
        // Entering safe mode just before a revision would qualify stops it.
        rig.store(store(2, None));
        rig.success();
        rig.wait(secs(59));
        rig.store(store(2, Some(SafeModeReason::Locked)));
        rig.wait(secs(600));
        assert_eq!(rig.marks(), [1]);
    }

    fn temp_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("quicker-lkg-{name}-{}", uuid::Uuid::new_v4()))
    }

    /// Refusals are expected (safe mode, the revision overtaken by a
    /// publish, an active configuration never written): one debug line
    /// each, nothing at warn or error, and no retry while the store state
    /// stays the same.
    #[test]
    fn a_refused_mark_logs_one_debug_line_and_is_not_retried() {
        let dir = temp_dir("refused");
        let built = scoped(&dir, LevelFilter::DEBUG);
        let attempts = Arc::new(Mutex::new(vec![]));
        tracing::subscriber::with_default(built.subscriber, || {
            let seen = Arc::clone(&attempts);
            let mark: MarkFn = Box::new(move |revision| {
                seen.lock().push(revision);
                Err(match revision {
                    1 => CommandError::NotFound {
                        kind: EntityKind::Revision,
                        id: "active".into(),
                    },
                    2 => CommandError::SafeMode {
                        reason: SafeModeReason::Locked,
                    },
                    _ => CommandError::RevisionConflict { current: 9 },
                })
            });
            let mut rig = Rig::with(store(1, None), mark);
            rig.healthy();
            for revision in 1..=3 {
                rig.store(store(revision, None));
                for _ in 0..3 {
                    rig.success();
                    rig.wait(secs(60));
                    assert_eq!(rig.due_in(), None, "nothing left to wake up for");
                }
            }
        });
        drop(built.guard);
        assert_eq!(*attempts.lock(), [1, 2, 3], "one attempt per store state");
        let log = read_logs(&dir);
        let skipped: Vec<&str> = log
            .lines()
            .filter(|line| line.contains("event=\"store.last_good_skipped\""))
            .collect();
        assert_eq!(skipped.len(), 3, "{log}");
        for (line, reason) in skipped
            .iter()
            .zip(["not_found", "safe_mode", "revision_changed"])
        {
            assert!(line.contains("DEBUG"), "{line}");
            assert!(line.contains(&format!("reason=\"{reason}\"")), "{line}");
        }
        assert!(!log.contains("ERROR") && !log.contains("WARN"), "{log}");
        fs::remove_dir_all(dir).unwrap();
    }

    fn config(id: &str) -> Config {
        Config {
            rules: vec![Rule {
                id: id.into(),
                match_spec: MatchSpec {
                    kind: MatchKind::Literal,
                    value: format!("trigger-{id}"),
                },
                ..Rule::default()
            }],
            ..Config::default()
        }
    }

    fn last_good_on_disk(dir: &std::path::Path) -> Option<u64> {
        let meta: Value =
            serde_json::from_slice(&fs::read(dir.join("store-meta.json")).unwrap()).unwrap();
        meta["last_good_revision"].as_u64()
    }

    #[test]
    fn the_production_mark_refuses_a_revision_that_is_no_longer_active() {
        let dir = temp_dir("active");
        let managed = Managed::new(Store::load(dir.clone()));
        managed.lock().publish(config("one")).unwrap();
        managed.lock().publish(config("two")).unwrap();
        assert!(matches!(
            mark_active(&managed, 1),
            Err(CommandError::RevisionConflict { current: 2 })
        ));
        assert_eq!(last_good_on_disk(&dir), None);
        mark_active(&managed, 2).unwrap();
        assert_eq!(last_good_on_disk(&dir), Some(2));
        drop(managed);
        fs::remove_dir_all(dir).unwrap();
    }

    /// B-04 end to end at the store level: the tracker marks the revision
    /// that qualified; two newer ones never do; `active.json` is then
    /// corrupted, and the next load recovers the marked revision from
    /// `last_good` (without the mark it would be the newest snapshot, 3).
    #[test]
    fn a_revision_the_tracker_marked_is_what_a_corrupt_active_json_recovers_to() {
        let dir = temp_dir("e2e");
        let managed = Arc::new(Managed::new(Store::load(dir.clone())));
        managed.lock().publish(config("one")).unwrap();
        managed.lock().publish(config("two")).unwrap();
        let two = managed.lock().active.clone();
        assert_eq!(two.revision, 2);

        let target = Arc::clone(&managed);
        let mut rig = Rig::with(
            managed.lock().store_state(),
            Box::new(move |revision| mark_active(&target, revision)),
        );
        rig.healthy();
        rig.success();
        rig.wait(QUALIFY_AFTER - Duration::from_millis(1));
        assert_eq!(last_good_on_disk(&dir), None);
        rig.wait(Duration::from_millis(1));
        assert_eq!(last_good_on_disk(&dir), Some(2));

        for id in ["three", "four"] {
            managed.lock().publish(config(id)).unwrap();
            let state = managed.lock().store_state();
            rig.store(state);
            rig.wait(secs(600));
        }
        assert_eq!(managed.lock().active.revision, 4);
        assert_eq!(last_good_on_disk(&dir), Some(2), "3 and 4 had no success");
        drop(rig);
        drop(managed);

        fs::write(dir.join("active.json"), b"{\"revision\": 4, \"rul").unwrap();
        let reloaded = Store::load(dir.clone());
        assert_eq!(reloaded.safe_mode(), None);
        assert!(
            reloaded.startup_problems().iter().any(|problem| matches!(
                problem.kind,
                ProblemKind::Recovered {
                    from: RecoverySource::LastGood,
                    revision: Some(2)
                }
            )),
            "{:?}",
            reloaded.startup_problems()
        );
        assert_eq!(reloaded.active.revision, 2);
        assert_eq!(reloaded.active.rules[0].id, "two");
        assert_eq!(reloaded.active, two);
        drop(reloaded);
        fs::remove_dir_all(dir).unwrap();
    }

    struct Channel(mpsc::Sender<&'static str>);
    impl Sink for Channel {
        fn emit(&self, name: &'static str, _payload: Value) {
            let _ = self.0.send(name);
        }
    }

    /// A mark that only reports the revision.
    fn reported(marked: mpsc::Sender<u64>) -> MarkFn {
        Box::new(move |revision| {
            let _ = marked.send(revision);
            Ok(())
        })
    }

    /// Wired as in the app: the bus hands the tracker Health and history
    /// entries from the runtime and the store's reports, on its own thread.
    #[test]
    fn on_the_bus_runtime_health_and_history_reach_the_tracker() {
        let clock = FakeClock::new();
        let runtime = Arc::new(Runtime::new(clock.clone()));
        let (mark_tx, marked) = mpsc::channel();
        let tracker = LkgTracker::new(&runtime, store(1, None), reported(mark_tx));
        let (tx, emitted) = mpsc::channel();
        let bus = EventBus::start(&runtime, store(1, None), Channel(tx), Box::new(tracker));
        // A publish reported before anything else: revision 2 is counted.
        bus.store_sink().report(store(2, None));
        runtime.publish_health(health(HealthState::Healthy));
        // Once the bus sent the Health, the tracker has it.
        while emitted.recv_timeout(secs(10)).unwrap() != ENGINE_HEALTH {}
        clock.advance(QUALIFY_AFTER);
        record(&runtime, TriggerKind::Manual, Outcome::Succeeded);
        record(&runtime, TriggerKind::Immediate, Outcome::Succeeded);
        assert_eq!(marked.recv_timeout(secs(10)), Ok(2));
        assert!(marked.try_recv().is_err(), "marked once");
    }

    /// What the probe saw.
    #[derive(Debug, PartialEq)]
    enum Seen {
        Status(HealthState),
        Executed(u64),
        Store(u64),
    }

    /// Reports every callback. Listed after the tracker: once the probe saw
    /// something, the tracker has handled it.
    struct Probe(mpsc::Sender<Seen>);
    impl Observer for Probe {
        fn status(&mut self, health: &HealthSnapshot, _state: &EngineState) {
            let _ = self.0.send(Seen::Status(health.state));
        }
        fn executed(&mut self, entry: &HistoryEntry) {
            let _ = self.0.send(Seen::Executed(entry.seq));
        }
        fn store(&mut self, store: StoreState) {
            let _ = self.0.send(Seen::Store(store.revision));
        }
    }

    /// Waits until the probe sees `what`, skipping what comes before it.
    fn until(seen: &mpsc::Receiver<Seen>, what: Seen) {
        while seen.recv_timeout(secs(10)).unwrap() != what {}
    }

    /// Review finding 1 on the bus: a listener ahead of the bus's holds the
    /// runtime's event thread while it delivers a success of revision 1, so
    /// the store's report of revision 2 reaches the bus first and the
    /// success after it. The success does not count for revision 2.
    #[test]
    fn on_the_bus_a_success_of_the_old_revision_delivered_after_a_publish_does_not_count() {
        let clock = FakeClock::new();
        let runtime = Arc::new(Runtime::new(clock.clone()));
        let (entered_tx, entered) = mpsc::channel();
        let (release, release_rx) = mpsc::channel::<()>();
        let gate = Mutex::new(Some((entered_tx, release_rx)));
        runtime.subscribe(Box::new(move |event| {
            if matches!(event, EngineEvent::Execution(_)) {
                let held = gate.lock().take();
                if let Some((entered, release)) = held {
                    let _ = entered.send(());
                    let _ = release.recv();
                }
            }
        }));
        let (mark_tx, marked) = mpsc::channel();
        let tracker = LkgTracker::new(&runtime, store(1, None), reported(mark_tx));
        let (probe, seen) = mpsc::channel();
        let (tx, _emitted) = mpsc::channel();
        let bus = EventBus::start(
            &runtime,
            store(1, None),
            Channel(tx),
            Box::new(Observers(vec![Box::new(tracker), Box::new(Probe(probe))])),
        );
        let reports = bus.store_sink();
        runtime.publish_health(health(HealthState::Healthy));
        until(&seen, Seen::Status(HealthState::Healthy));
        // Revision 1 succeeds; its entry is held on the way to the bus.
        let old = record(&runtime, TriggerKind::Terminator, Outcome::Succeeded);
        entered.recv_timeout(secs(10)).unwrap();
        // Revision 2 is installed and reported; the bus takes the report
        // first, then the held entry.
        reports.report(store(2, None));
        until(&seen, Seen::Store(2));
        release.send(()).unwrap();
        until(&seen, Seen::Executed(old.seq));
        // Healthy for well over a minute since; a repeated report wakes
        // the tracker.
        clock.advance(QUALIFY_AFTER * 10);
        reports.report(store(2, None));
        until(&seen, Seen::Store(2));
        assert!(
            marked.try_recv().is_err(),
            "revision 1's success counted for revision 2"
        );
        // A success recorded after the report is revision 2's.
        record(&runtime, TriggerKind::Terminator, Outcome::Succeeded);
        assert_eq!(marked.recv_timeout(secs(10)), Ok(2));
        assert!(marked.try_recv().is_err(), "marked once");
    }

    /// Stands in for the tray: once armed, its next status call signals and
    /// then blocks until released (the main thread busy).
    struct Tray {
        armed: Arc<AtomicBool>,
        entered: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
    }
    impl Observer for Tray {
        fn status(&mut self, _health: &HealthSnapshot, _state: &EngineState) {
            if self.armed.swap(false, Ordering::SeqCst) {
                let _ = self.entered.send(());
                let _ = self.release.recv();
            }
        }
        fn executed(&mut self, _entry: &HistoryEntry) {}
    }

    /// Review finding 2 on the bus: the tray, ahead of the tracker, blocks
    /// across the end of the minute while a Failed waits behind it. When it
    /// returns, the tracker sees the minute up on the Health it last had,
    /// Healthy; the runtime's current Health, Failed, stops the write.
    #[test]
    fn on_the_bus_a_failed_queued_behind_a_blocked_observer_stops_the_write() {
        let clock = FakeClock::new();
        let runtime = Arc::new(Runtime::new(clock.clone()));
        let (mark_tx, marked) = mpsc::channel();
        let tracker = LkgTracker::new(&runtime, store(1, None), reported(mark_tx));
        let armed = Arc::new(AtomicBool::new(false));
        let (entered_tx, entered) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        let tray = Tray {
            armed: Arc::clone(&armed),
            entered: entered_tx,
            release: release_rx,
        };
        let (probe, seen) = mpsc::channel();
        let (tx, _emitted) = mpsc::channel();
        let bus = EventBus::start(
            &runtime,
            store(1, None),
            Channel(tx),
            Box::new(Observers(vec![
                Box::new(tray),
                Box::new(tracker),
                Box::new(Probe(probe)),
            ])),
        );
        runtime.publish_health(health(HealthState::Healthy));
        until(&seen, Seen::Status(HealthState::Healthy));
        let run = record(&runtime, TriggerKind::Terminator, Outcome::Succeeded);
        until(&seen, Seen::Executed(run.seq));
        clock.advance(secs(59));
        // The tray blocks on the next status (a pause change)...
        armed.store(true, Ordering::SeqCst);
        runtime.set_pause(PauseReasons::TIMED, true);
        entered.recv_timeout(secs(10)).unwrap();
        // ...while the engine fails and the minute ends.
        runtime.publish_health(health(HealthState::Failed));
        clock.advance(secs(2));
        release.send(()).unwrap();
        until(&seen, Seen::Status(HealthState::Failed));
        assert!(
            marked.try_recv().is_err(),
            "marked while the runtime read Failed"
        );
        // Healthy again: marked once it has been for a full minute.
        runtime.publish_health(health(HealthState::Healthy));
        until(&seen, Seen::Status(HealthState::Healthy));
        clock.advance(QUALIFY_AFTER);
        bus.store_sink().report(store(1, None));
        assert_eq!(marked.recv_timeout(secs(10)), Ok(1));
        assert!(marked.try_recv().is_err(), "marked once");
    }
}
