//! Real target text evidence. Keyboard virtual keys are deliberately absent from this contract.
use crate::ImeErasure;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompositionEvidence {
    Inactive,
    /// The provider must prove this whole range is uncommitted and does not overlap body text.
    Active {
        text: String,
        body_unchanged: bool,
    },
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextObservation {
    pub revision: u64,
    pub focus_id: String,
    pub process: String,
    pub control_id: String,
    /// Provider anchor/epoch changes for navigation, mouse movement or any unproven caret change.
    pub caret_anchor: String,
    pub selection_collapsed: bool,
    /// Exact observed committed body before the caret, possibly bounded. Never inferred from VKs.
    pub committed_suffix: String,
    pub composition: CompositionEvidence,
    pub observed_at_ms: u64,
    pub self_injected: bool,
}

impl TextObservation {
    pub(crate) fn same_target(&self, other: &Self) -> bool {
        self.revision == other.revision
            && self.focus_id == other.focus_id
            && self.process == other.process
            && self.control_id == other.control_id
            && self.caret_anchor == other.caret_anchor
    }
    pub(crate) fn valid(&self, now_ms: u64, timeout_ms: u64) -> bool {
        !self.focus_id.is_empty()
            && !self.control_id.is_empty()
            && !self.caret_anchor.is_empty()
            && self.selection_collapsed
            && !self.self_injected
            && now_ms
                .checked_sub(self.observed_at_ms)
                .is_some_and(|age| age < timeout_ms)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObservationDelta {
    Reset,
    Unchanged,
    CommittedAppend(String),
    CompositionUpdated(String),
}

#[derive(Debug, Clone)]
pub struct InputSession {
    generation: u64,
    last: Option<TextObservation>,
    buffer: String,
    composition_body: Option<String>,
    max_chars: usize,
    observation_timeout_ms: u64,
}

impl InputSession {
    pub fn new(max_chars: usize, observation_timeout_ms: u64) -> Self {
        Self {
            generation: 0,
            last: None,
            buffer: String::new(),
            composition_body: None,
            max_chars,
            observation_timeout_ms,
        }
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn last_observation(&self) -> Option<&TextObservation> {
        self.last.as_ref()
    }
    pub fn invalidate(&mut self) {
        self.generation = self.generation.saturating_add(1);
        self.last = None;
        self.buffer.clear();
        self.composition_body = None;
    }
    fn reset_to(&mut self, observation: TextObservation) -> ObservationDelta {
        self.invalidate();
        if observation.valid(observation.observed_at_ms, self.observation_timeout_ms)
            && !matches!(observation.composition, CompositionEvidence::Unknown)
        {
            self.last = Some(observation);
        }
        ObservationDelta::Reset
    }
    pub fn observe(&mut self, observation: TextObservation, now_ms: u64) -> ObservationDelta {
        if !observation.valid(now_ms, self.observation_timeout_ms)
            || matches!(observation.composition, CompositionEvidence::Unknown)
        {
            self.invalidate();
            return ObservationDelta::Reset;
        }
        let Some(previous) = self.last.as_ref() else {
            return self.reset_to(observation);
        };
        if !previous.valid(now_ms, self.observation_timeout_ms)
            || observation.observed_at_ms < previous.observed_at_ms
            || !previous.same_target(&observation)
        {
            return self.reset_to(observation);
        }
        if previous.committed_suffix == observation.committed_suffix
            && previous.composition == observation.composition
        {
            self.last = Some(observation);
            return ObservationDelta::Unchanged;
        }
        // Every real text/composition change invalidates a pending deletion proof.
        self.generation = self.generation.saturating_add(1);
        match &observation.composition {
            CompositionEvidence::Inactive => {
                let Some(append) = observation
                    .committed_suffix
                    .strip_prefix(&previous.committed_suffix)
                else {
                    return self.reset_to(observation);
                };
                if append.is_empty() {
                    // Cancellation is not committed input; discard the candidate buffer.
                    self.buffer.clear();
                    self.composition_body = None;
                    self.last = Some(observation);
                    return ObservationDelta::Reset;
                }
                let append = append.to_owned();
                if matches!(previous.composition, CompositionEvidence::Inactive) {
                    self.buffer.push_str(&append);
                } else {
                    // Only the committed text, never the phonetic candidate, enters matching.
                    self.buffer = append.clone();
                }
                if self.buffer.chars().count() > self.max_chars {
                    // No slicing of Unicode or guessing at a shifted observation anchor.
                    return self.reset_to(observation);
                }
                self.composition_body = None;
                self.last = Some(observation);
                ObservationDelta::CommittedAppend(append)
            }
            CompositionEvidence::Active {
                text,
                body_unchanged,
            } => {
                if !body_unchanged || previous.committed_suffix != observation.committed_suffix {
                    return self.reset_to(observation);
                }
                if matches!(previous.composition, CompositionEvidence::Inactive) {
                    self.composition_body = Some(previous.committed_suffix.clone());
                }
                if self.composition_body.as_deref() != Some(observation.committed_suffix.as_str())
                    || text.chars().count() > self.max_chars
                {
                    return self.reset_to(observation);
                }
                self.buffer = text.clone();
                let delta = ObservationDelta::CompositionUpdated(text.clone());
                self.last = Some(observation);
                delta
            }
            CompositionEvidence::Unknown => unreachable!(),
        }
    }
    pub fn matching_input(&self) -> Option<&str> {
        let observation = self.last.as_ref()?;
        if self.buffer.is_empty() || !observation.selection_collapsed || observation.self_injected {
            return None;
        }
        match observation.composition {
            CompositionEvidence::Inactive => Some(&self.buffer),
            CompositionEvidence::Active {
                body_unchanged: true,
                ..
            } if self.composition_body.as_deref()
                == Some(observation.committed_suffix.as_str()) =>
            {
                Some(&self.buffer)
            }
            _ => None,
        }
    }
    pub fn erasure_proof(
        &self,
        matched: &str,
        preference: ImeErasure,
        now_ms: u64,
    ) -> Result<crate::ErasureProof, &'static str> {
        if matched.is_empty()
            || !self
                .matching_input()
                .is_some_and(|input| input.ends_with(matched))
        {
            return Err("unobserved_match");
        }
        crate::ErasureProof::from_session(
            self.last.as_ref().ok_or("observation_unavailable")?,
            self.generation,
            matched,
            preference,
            self.observation_timeout_ms,
            now_ms,
        )
    }
}
