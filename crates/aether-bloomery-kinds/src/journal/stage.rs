//! Unfenced, content-addressed staging (ADR-0240 D7): artifacts land in the
//! journal's store with no event and no head move.

use alloc::string::String;
use alloc::vec::Vec;

use aether_actor::HeldReply;

use crate::EncodedArtifact;

/// Store `artifacts` content-addressed, with no event and no head move.
///
/// Carries no fence: staging names bytes by their digest, so staging the
/// same bytes twice stores them once and never conflicts. The artifacts
/// commit together or not at all, and a citation may name an artifact staged
/// by the same mail or one already stored; a citation that names neither
/// refuses the whole stage. The answer comes only after the rows commit, so
/// a read sent after it finds every staged artifact.
#[aether_data::kind(name = "aether.bloomery.journal.stage", eq, no_serde)]
pub struct Stage {
    artifacts: Vec<EncodedArtifact>,
}

impl Stage {
    /// Stage `artifacts`, in request order.
    #[must_use]
    pub fn new(artifacts: Vec<EncodedArtifact>) -> Self {
        Self { artifacts }
    }

    /// Artifacts to stage, in request order.
    #[must_use]
    pub fn artifacts(&self) -> &[EncodedArtifact] {
        &self.artifacts
    }

    /// Take the artifacts without copying payload bytes.
    #[must_use]
    pub fn into_artifacts(self) -> Vec<EncodedArtifact> {
        self.artifacts
    }
}

/// Outcome of one [`Stage`].
#[aether_data::kind(name = "aether.bloomery.journal.stage_result", eq, no_serde)]
pub enum StageResult {
    /// Every artifact's row committed.
    Staged,
    /// A dangling or mistyped citation, or a journal backend failure; nothing
    /// was stored.
    Err {
        /// Human-readable failure.
        message: String,
    },
}

impl HeldReply for StageResult {
    fn unanswered() -> Self {
        Self::Err { message: String::from("bloomery journal closed before answering") }
    }
}
