use crate::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImeState {
    NotComposing,
    Uncommitted,
    FullyCommitted,
    PartiallyCommitted,
    Unknown,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentStamp {
    pub revision: u64,
    pub buffer_generation: u64,
    pub focus_id: String,
    pub process: String,
    pub ime_state: ImeState,
    pub injection_safe: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransactionState {
    Preflight,
    Ready,
    Erased,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    Unknown,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionReport {
    pub rule_id: String,
    pub state: TransactionState,
    pub completed_steps: usize,
    pub trigger_erased: bool,
    pub side_effects: Vec<String>,
    pub error_code: Option<String>,
}
#[derive(Debug, Clone)]
pub struct Transaction {
    pub stamp: EnvironmentStamp,
    pub report: ExecutionReport,
    pub cancelled: bool,
    pub deadline_ms: u64,
    owned_keys: BTreeSet<Key>,
    owned_buttons: BTreeSet<String>,
}
impl Transaction {
    pub fn new(rule_id: impl Into<String>, stamp: EnvironmentStamp, deadline_ms: u64) -> Self {
        Self {
            stamp,
            report: ExecutionReport {
                rule_id: rule_id.into(),
                state: TransactionState::Preflight,
                completed_steps: 0,
                trigger_erased: false,
                side_effects: vec![],
                error_code: None,
            },
            cancelled: false,
            deadline_ms,
            owned_keys: BTreeSet::new(),
            owned_buttons: BTreeSet::new(),
        }
    }
    pub fn mark_ready(&mut self) -> Result<(), &'static str> {
        if self.report.state != TransactionState::Preflight {
            return Err("invalid_phase");
        }
        self.report.state = TransactionState::Ready;
        Ok(())
    }
    pub fn cancel(&mut self) {
        self.cancelled = true;
        self.report.state = TransactionState::Cancelled;
        self.report.error_code = Some("cancelled".into());
    }
    pub fn check_before_erase(
        &self,
        current: &EnvironmentStamp,
        erasure: ImeErasure,
        now_ms: u64,
    ) -> Result<(), &'static str> {
        if self.report.state != TransactionState::Ready || self.report.trigger_erased {
            return Err("invalid_phase");
        }
        self.check_common(current, now_ms)?;
        if self.stamp.buffer_generation != current.buffer_generation {
            return Err("stale_buffer");
        }
        if self.stamp.focus_id != current.focus_id {
            return Err("focus_changed");
        }
        match current.ime_state {
            ImeState::PartiallyCommitted | ImeState::Unknown => return Err("unsafe_ime"),
            ImeState::Uncommitted if erasure != ImeErasure::Escape => return Err("unsafe_ime"),
            ImeState::NotComposing | ImeState::FullyCommitted if erasure == ImeErasure::Escape => {}
            _ => {}
        }
        Ok(())
    }
    /// Native observation path. The legacy ImeState flag alone is not deletion evidence.
    pub fn check_before_erase_with_proof(
        &self,
        current: &EnvironmentStamp,
        proof: &ErasureProof,
        observation: &TextObservation,
        now_ms: u64,
    ) -> Result<(), &'static str> {
        self.check_proof_stamp(current, proof, now_ms)?;
        proof.verify_before(observation, current.buffer_generation, now_ms)
    }
    fn check_proof_stamp(
        &self,
        current: &EnvironmentStamp,
        proof: &ErasureProof,
        now_ms: u64,
    ) -> Result<(), &'static str> {
        if self.report.state != TransactionState::Ready || self.report.trigger_erased {
            return Err("invalid_phase");
        }
        self.check_common(current, now_ms)?;
        if self.stamp.buffer_generation != current.buffer_generation
            || proof.generation() != current.buffer_generation
        {
            return Err("stale_buffer");
        }
        if self.stamp.focus_id != current.focus_id
            || proof.observation().focus_id != current.focus_id
            || self.stamp.process != current.process
            || proof.observation().process != current.process
        {
            return Err("focus_changed");
        }
        if proof.observation().revision != current.revision {
            return Err("stale_revision");
        }
        Ok(())
    }
    /// Mark deletion only after a new target observation proves its result.
    pub fn mark_erased_with_proof(
        &mut self,
        current: &EnvironmentStamp,
        proof: &ErasureProof,
        after: &TextObservation,
        now_ms: u64,
    ) -> Result<(), &'static str> {
        self.check_proof_stamp(current, proof, now_ms)?;
        proof.verify_after(after, now_ms)?;
        self.report.trigger_erased = true;
        self.report.state = TransactionState::Erased;
        self.report
            .side_effects
            .push("trigger_erasure_observed".into());
        Ok(())
    }
    fn check_common(&self, current: &EnvironmentStamp, now_ms: u64) -> Result<(), &'static str> {
        if self.cancelled {
            return Err("cancelled");
        }
        if now_ms >= self.deadline_ms {
            return Err("timeout");
        }
        if !current.injection_safe {
            return Err("injection_unavailable");
        }
        if self.stamp.revision != current.revision {
            return Err("stale_revision");
        }
        Ok(())
    }
    pub fn mark_erased(
        &mut self,
        current: &EnvironmentStamp,
        erasure: ImeErasure,
        now_ms: u64,
    ) -> Result<(), &'static str> {
        self.check_before_erase(current, erasure, now_ms)?;
        self.report.trigger_erased = true;
        self.report.state = TransactionState::Erased;
        self.report.side_effects.push("trigger_erased".into());
        Ok(())
    }
    pub fn check_step(
        &self,
        current: &EnvironmentStamp,
        target: &TargetPolicy,
        now_ms: u64,
    ) -> Result<(), &'static str> {
        if ![TransactionState::Erased, TransactionState::Running].contains(&self.report.state) {
            return Err("invalid_phase");
        }
        self.check_common(current, now_ms)?;
        // Unexpected physical input invalidates every remaining step, not just deletion.
        if self.stamp.buffer_generation != current.buffer_generation {
            return Err("stale_buffer");
        }
        match target {
            TargetPolicy::Locked if self.stamp.focus_id != current.focus_id => Err("focus_changed"),
            TargetPolicy::Declared { process }
                if process.is_empty() || !process.eq_ignore_ascii_case(&current.process) =>
            {
                Err("undeclared_target")
            }
            _ => Ok(()),
        }
    }
    pub fn step_sent(&mut self, operation: &Operation, consumed: bool) {
        self.report.state = TransactionState::Running;
        self.report.completed_steps += 1;
        match operation {
            Operation::KeyDown { key } => {
                self.owned_keys.insert(key.clone());
            }
            Operation::KeyUp { key } => {
                self.owned_keys.remove(key);
            }
            Operation::MouseButton { button, state } => {
                let button = format!("{button:?}");
                match state {
                    ButtonState::Down => {
                        self.owned_buttons.insert(button);
                    }
                    ButtonState::Up => {
                        self.owned_buttons.remove(&button);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        if !matches!(operation, Operation::Delay { .. }) {
            self.report.side_effects.push(
                if consumed {
                    "operation_observed"
                } else {
                    "operation_sent_consumption_unknown"
                }
                .into(),
            );
        }
    }
    pub fn finish(&mut self, consumption_observed: bool) -> Result<(), &'static str> {
        if ![TransactionState::Running, TransactionState::Erased].contains(&self.report.state) {
            return Err("invalid_phase");
        }
        self.report.state = if consumption_observed {
            TransactionState::Succeeded
        } else {
            TransactionState::Unknown
        };
        Ok(())
    }
    pub fn fail(&mut self, code: impl Into<String>) {
        self.report.state = TransactionState::Failed;
        self.report.error_code = Some(code.into());
    }
    /// Physical state must be known false for each held key/button. Unknown or down is never released.
    pub fn cleanup(
        &self,
        physical_keys: &BTreeMap<Key, Option<bool>>,
        physical_buttons: &BTreeMap<String, Option<bool>>,
    ) -> CleanupPlan {
        let keys = self
            .owned_keys
            .iter()
            .filter(|k| physical_keys.get(*k) == Some(&Some(false)))
            .cloned()
            .collect();
        let buttons = self
            .owned_buttons
            .iter()
            .filter(|b| physical_buttons.get(*b) == Some(&Some(false)))
            .cloned()
            .collect();
        let blocked = self
            .owned_keys
            .iter()
            .any(|k| physical_keys.get(k) != Some(&Some(false)))
            || self
                .owned_buttons
                .iter()
                .any(|b| physical_buttons.get(b) != Some(&Some(false)));
        CleanupPlan {
            keys,
            buttons,
            blocked_by_physical_state: blocked,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CleanupPlan {
    pub keys: Vec<Key>,
    pub buttons: Vec<String>,
    pub blocked_by_physical_state: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClipboardLease {
    pub written_generation: u64,
}
impl ClipboardLease {
    pub fn can_restore(&self, current_generation: u64) -> bool {
        self.written_generation == current_generation
    }
}
/// Configuration transaction in memory; the host performs durable pointer publishing through `publish`.
#[derive(Clone)]
pub struct DraftStore {
    pub active: Config,
    pub draft: Config,
    pub base_serialized: String,
}
/// The serialized base configuration holds every abbreviation and phrase, so it is
/// redacted (B-09); the two configurations print through their own redacting `Debug`.
impl std::fmt::Debug for DraftStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DraftStore")
            .field("active", &self.active)
            .field("draft", &self.draft)
            .field("base_serialized", &Redacted(&self.base_serialized))
            .finish()
    }
}
impl DraftStore {
    pub fn new(active: Config) -> Self {
        let base_serialized = serde_json::to_string(&active).expect("config serialization");
        Self {
            draft: active.clone(),
            active,
            base_serialized,
        }
    }
    pub fn is_dirty(&self) -> bool {
        self.active != self.draft
    }
    pub fn discard(&mut self) {
        self.draft = self.active.clone();
    }
    pub fn save_draft(&mut self, draft: Config) {
        self.draft = draft;
    }
    pub fn apply<F>(
        &mut self,
        current_serialized: &str,
        mut publish: F,
    ) -> Result<u64, Vec<Diagnostic>>
    where
        F: FnMut(&CompiledConfig) -> Result<u64, String>,
    {
        if current_serialized != self.base_serialized {
            return Err(vec![Diagnostic::error(
                "external_conflict",
                "Active configuration changed outside this draft",
                SourceSpan::default(),
            )]);
        }
        let mut next = self.draft.clone();
        next.revision = self.active.revision.checked_add(1).ok_or_else(|| {
            vec![Diagnostic::error(
                "revision_overflow",
                "Configuration revision is exhausted",
                SourceSpan::default(),
            )]
        })?;
        let compiled = compile_config(&next)?;
        let ack = publish(&compiled).map_err(|_| {
            vec![Diagnostic::error(
                "publish_failed",
                "Engine publish failed; old active revision is retained",
                SourceSpan::default(),
            )]
        })?;
        if ack != next.revision {
            return Err(vec![Diagnostic::error(
                "revision_ack",
                "Engine acknowledged a different revision; host must roll back its publication",
                SourceSpan::default(),
            )]);
        }
        self.active = next.clone();
        self.draft = next;
        self.base_serialized = serde_json::to_string(&self.active).expect("config serialization");
        Ok(ack)
    }
}
