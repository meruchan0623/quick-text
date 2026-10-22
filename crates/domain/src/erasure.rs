use crate::{CompositionEvidence, ImeErasure, TextObservation};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErasureKind {
    /// Scalar count is descriptive. The adapter must prove its deletion units/range are equivalent.
    CommittedText {
        scalar_count: usize,
    },
    CancelComposition,
}

#[derive(Debug, Clone)]
pub struct ErasureProof {
    observation: TextObservation,
    generation: u64,
    kind: ErasureKind,
    expected_suffix: String,
    timeout_ms: u64,
}

impl ErasureProof {
    pub(crate) fn from_session(
        observation: &TextObservation,
        generation: u64,
        matched: &str,
        preference: ImeErasure,
        timeout_ms: u64,
        now_ms: u64,
    ) -> Result<Self, &'static str> {
        if !observation.valid(now_ms, timeout_ms) {
            return Err("observation_unavailable");
        }
        let (kind, expected_suffix) = match &observation.composition {
            CompositionEvidence::Inactive => (
                ErasureKind::CommittedText {
                    scalar_count: matched.chars().count(),
                },
                observation
                    .committed_suffix
                    .strip_suffix(matched)
                    .ok_or("suffix_changed")?
                    .to_owned(),
            ),
            CompositionEvidence::Active {
                text,
                body_unchanged: true,
            } if preference == ImeErasure::Escape && text == matched => (
                ErasureKind::CancelComposition,
                observation.committed_suffix.clone(),
            ),
            _ => return Err("unsafe_ime"),
        };
        Ok(Self {
            observation: observation.clone(),
            generation,
            kind,
            expected_suffix,
            timeout_ms,
        })
    }
    pub fn kind(&self) -> ErasureKind {
        self.kind
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn observation(&self) -> &TextObservation {
        &self.observation
    }
    pub fn expected_suffix(&self) -> &str {
        &self.expected_suffix
    }
    pub fn verify_before(
        &self,
        current: &TextObservation,
        generation: u64,
        now_ms: u64,
    ) -> Result<(), &'static str> {
        if generation != self.generation {
            return Err("stale_buffer");
        }
        self.verify_target(current, now_ms)?;
        if current.committed_suffix != self.observation.committed_suffix {
            return Err("suffix_changed");
        }
        if current.composition != self.observation.composition {
            return Err("unsafe_ime");
        }
        Ok(())
    }
    fn verify_target(&self, current: &TextObservation, now_ms: u64) -> Result<(), &'static str> {
        // Rendering may take longer than a provider polling interval. A fresh, unchanged reread
        // and the transaction's generation/deadline establish validity without expiring its seed.
        if !current.valid(now_ms, self.timeout_ms)
            || current.observed_at_ms < self.observation.observed_at_ms
        {
            return Err("observation_unavailable");
        }
        if current.revision != self.observation.revision {
            return Err("stale_revision");
        }
        if !self.observation.same_target(current) {
            return Err("focus_or_caret_changed");
        }
        Ok(())
    }
    /// A send call is not success: prove the exact body after deletion/cancellation and inactive IME.
    pub fn verify_after(&self, current: &TextObservation, now_ms: u64) -> Result<(), &'static str> {
        self.verify_target(current, now_ms)?;
        if current.observed_at_ms <= self.observation.observed_at_ms {
            return Err("observation_not_fresh");
        }
        if current.composition != CompositionEvidence::Inactive {
            return Err("unsafe_ime");
        }
        if current.committed_suffix != self.expected_suffix {
            return Err("erasure_not_observed");
        }
        Ok(())
    }
}
