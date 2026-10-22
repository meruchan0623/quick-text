//! Execution history (B-35, F-25): an in-memory ring of the most recent runs,
//! bounded both by count ([`CAPACITY`]) and by age ([`MAX_AGE`]).
//!
//! Entries carry identifiers, codes and timings only. A [`Record`] has no
//! field for a rule body, an abbreviation, the detection buffer or clipboard
//! content, so none of them can reach the history, its events or the
//! diagnostics built from it.
use crate::health::{HistoryEntry, Outcome, ReasonCode, StageTimings, TriggerKind};
use parking_lot::Mutex;
use serde::Serialize;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Most entries kept.
pub const CAPACITY: usize = 200;
/// Oldest entry kept, measured on the monotonic clock.
pub const MAX_AGE: Duration = Duration::from_secs(30 * 60);

/// One run to record; [`History::push`] assigns `seq` and `at_ms`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    /// Rule id only (never the abbreviation or the expansion).
    pub rule_id: String,
    pub trigger: TriggerKind,
    pub outcome: Outcome,
    pub reason: ReasonCode,
    /// Executable name only (e.g. `notepad.exe`).
    pub process: Option<String>,
    pub completed_steps: usize,
    pub side_effects: bool,
    pub skipped_formats: Vec<u32>,
    pub timings: StageTimings,
}

impl Record {
    pub fn new(
        rule_id: impl Into<String>,
        trigger: TriggerKind,
        outcome: Outcome,
        reason: ReasonCode,
    ) -> Self {
        Self {
            rule_id: rule_id.into(),
            trigger,
            outcome,
            reason,
            process: None,
            completed_steps: 0,
            side_effects: false,
            skipped_formats: vec![],
            timings: StageTimings::default(),
        }
    }
}

/// Payload of `explain_last_miss` (IPC contract §5): the latest entry whose
/// outcome is not `succeeded`. 0.4 explains from the history only.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MissExplanation {
    pub seq: u64,
    pub at_ms: i64,
    pub rule_id: String,
    pub outcome: Outcome,
    pub reason: ReasonCode,
}

impl From<&HistoryEntry> for MissExplanation {
    fn from(entry: &HistoryEntry) -> Self {
        Self {
            seq: entry.seq,
            at_ms: entry.at_ms,
            rule_id: entry.rule_id.clone(),
            outcome: entry.outcome,
            reason: entry.reason,
        }
    }
}

#[derive(Default)]
struct Ring {
    /// Entries in ascending `seq`, each with the monotonic instant it was
    /// recorded at (age eviction never depends on wall-clock changes).
    entries: VecDeque<(Instant, HistoryEntry)>,
    last_seq: u64,
}

impl Ring {
    fn evict(&mut self, now: Instant) {
        while self.entries.len() > CAPACITY {
            self.entries.pop_front();
        }
        while self
            .entries
            .front()
            .is_some_and(|(at, _)| now.saturating_duration_since(*at) > MAX_AGE)
        {
            self.entries.pop_front();
        }
    }
}

/// The history ring. Time is passed in by the caller (the runtime's
/// [`crate::traits::Clock`]), so eviction is testable with a fake clock.
#[derive(Default)]
pub struct History {
    ring: Mutex<Ring>,
}

impl History {
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends `record` with the next sequence number and returns the entry.
    pub fn push(&self, record: Record, now: Instant, at_ms: i64) -> HistoryEntry {
        let mut ring = self.ring.lock();
        ring.last_seq += 1;
        let entry = HistoryEntry {
            seq: ring.last_seq,
            at_ms,
            rule_id: record.rule_id,
            trigger: record.trigger,
            outcome: record.outcome,
            reason: record.reason,
            process: record.process,
            completed_steps: record.completed_steps,
            side_effects: record.side_effects,
            skipped_formats: record.skipped_formats,
            timings: record.timings,
        };
        ring.entries.push_back((now, entry.clone()));
        ring.evict(now);
        entry
    }

    /// Entries with `seq > since_seq` (all when `None`), ascending.
    pub fn list(&self, since_seq: Option<u64>, now: Instant) -> Vec<HistoryEntry> {
        let mut ring = self.ring.lock();
        ring.evict(now);
        let since = since_seq.unwrap_or(0);
        ring.entries
            .iter()
            .filter(|(_, entry)| entry.seq > since)
            .map(|(_, entry)| entry.clone())
            .collect()
    }

    /// Drops every entry. Sequence numbers keep increasing, so a client that
    /// polls with `since_seq` never sees a reused number.
    pub fn clear(&self) {
        self.ring.lock().entries.clear();
    }

    /// The latest retained entry whose outcome is not `Succeeded`.
    pub fn last_miss(&self, now: Instant) -> Option<MissExplanation> {
        let mut ring = self.ring.lock();
        ring.evict(now);
        ring.entries
            .iter()
            .rev()
            .find(|(_, entry)| entry.outcome != Outcome::Succeeded)
            .map(|(_, entry)| MissExplanation::from(entry))
    }

    /// Highest sequence number assigned so far (0 before the first entry).
    pub fn last_seq(&self) -> u64 {
        self.ring.lock().last_seq
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::FakeClock;
    use crate::traits::Clock;

    fn record(rule: &str, outcome: Outcome) -> Record {
        let reason = match outcome {
            Outcome::Succeeded => ReasonCode::Ok,
            Outcome::DroppedBusy => ReasonCode::DroppedBusy,
            _ => ReasonCode::InjectPartial,
        };
        Record::new(rule, TriggerKind::Immediate, outcome, reason)
    }

    fn push(history: &History, clock: &FakeClock, rule: &str, outcome: Outcome) -> HistoryEntry {
        history.push(record(rule, outcome), clock.now(), clock.wall_ms())
    }

    #[test]
    fn keeps_only_the_newest_200_entries() {
        let (history, clock) = (History::new(), FakeClock::new());
        for i in 0..CAPACITY + 5 {
            push(&history, &clock, &format!("r{i}"), Outcome::Succeeded);
        }
        let entries = history.list(None, clock.now());
        assert_eq!(entries.len(), CAPACITY);
        assert_eq!(entries.first().unwrap().seq, 6);
        assert_eq!(entries.last().unwrap().seq, CAPACITY as u64 + 5);
        assert_eq!(history.last_seq(), CAPACITY as u64 + 5);
    }

    #[test]
    fn drops_entries_older_than_30_minutes() {
        let (history, clock) = (History::new(), FakeClock::new());
        push(&history, &clock, "old", Outcome::Failed);
        clock.advance(Duration::from_secs(20 * 60));
        let young = push(&history, &clock, "young", Outcome::Succeeded);
        clock.advance(Duration::from_secs(10 * 60));
        // Exactly 30 minutes is still retained.
        assert_eq!(history.list(None, clock.now()).len(), 2);
        clock.advance(Duration::from_millis(1));
        assert_eq!(history.list(None, clock.now()), vec![young.clone()]);
        assert_eq!(history.last_miss(clock.now()), None, "the old miss expired");
        clock.advance(Duration::from_secs(20 * 60));
        assert!(history.list(None, clock.now()).is_empty());
    }

    #[test]
    fn since_seq_filters_strictly_and_ascending() {
        let (history, clock) = (History::new(), FakeClock::new());
        for rule in ["a", "b", "c", "d"] {
            push(&history, &clock, rule, Outcome::Succeeded);
        }
        let seqs = |since| {
            history
                .list(since, clock.now())
                .iter()
                .map(|e| e.seq)
                .collect::<Vec<_>>()
        };
        assert_eq!(seqs(None), [1, 2, 3, 4]);
        assert_eq!(seqs(Some(0)), [1, 2, 3, 4]);
        assert_eq!(seqs(Some(2)), [3, 4]);
        assert_eq!(seqs(Some(4)), Vec::<u64>::new());
        assert_eq!(seqs(Some(99)), Vec::<u64>::new());
    }

    #[test]
    fn clear_keeps_sequence_monotonic() {
        let (history, clock) = (History::new(), FakeClock::new());
        push(&history, &clock, "a", Outcome::Failed);
        push(&history, &clock, "b", Outcome::Failed);
        history.clear();
        assert!(history.list(None, clock.now()).is_empty());
        assert_eq!(history.last_miss(clock.now()), None);
        assert_eq!(push(&history, &clock, "c", Outcome::Succeeded).seq, 3);
    }

    #[test]
    fn last_miss_is_the_latest_non_success() {
        let (history, clock) = (History::new(), FakeClock::new());
        assert_eq!(history.last_miss(clock.now()), None);
        push(&history, &clock, "a", Outcome::Failed);
        clock.advance(Duration::from_millis(5));
        let busy = push(&history, &clock, "b", Outcome::DroppedBusy);
        push(&history, &clock, "c", Outcome::Succeeded);
        assert_eq!(
            history.last_miss(clock.now()),
            Some(MissExplanation {
                seq: busy.seq,
                at_ms: FakeClock::WALL_START_MS + 5,
                rule_id: "b".into(),
                outcome: Outcome::DroppedBusy,
                reason: ReasonCode::DroppedBusy,
            })
        );
        let json = serde_json::to_value(history.last_miss(clock.now())).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"seq": 2, "at_ms": FakeClock::WALL_START_MS + 5,
                "rule_id": "b", "outcome": "dropped_busy", "reason": "dropped_busy"})
        );
    }

    #[test]
    fn entries_keep_every_record_field() {
        let (history, clock) = (History::new(), FakeClock::new());
        let mut full = record("r", Outcome::Failed);
        full.process = Some("notepad.exe".into());
        full.completed_steps = 2;
        full.side_effects = true;
        full.skipped_formats = vec![3];
        full.timings.total_us = Some(1500);
        let entry = history.push(full.clone(), clock.now(), 42);
        assert_eq!(entry.seq, 1);
        assert_eq!(entry.at_ms, 42);
        assert_eq!(entry.process, full.process);
        assert_eq!(entry.completed_steps, 2);
        assert!(entry.side_effects);
        assert_eq!(entry.skipped_formats, vec![3]);
        assert_eq!(entry.timings, full.timings);
    }
}
