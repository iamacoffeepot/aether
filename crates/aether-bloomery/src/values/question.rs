//! The parked-question artifact (ADR-0151, issue #3533).
//!
//! A stage attempt that reaches a genuine decision point only the owner can
//! settle completes **parked** — a third terminal beside succeeded / failed —
//! and returns one [`Question`]: the decision needed, the options considered,
//! and what the decision blocks. Like a [`StudyRecord`](super::StudyRecord) it
//! is a standalone content-addressed artifact, **not** an
//! [`Evidence`](super::Evidence) itself: an [`EvidenceKind::Question`] verdict
//! admitted through `Fact::AdmitEvidence` names this question by its `detail`
//! digest (ADR-0151), and the reducer folds a per-member pending-decision hold
//! from that evidence. Digest-addressed so the **answer** — an author-signed
//! statement adopting the question's exact digest (ADR-0149 §The boundary) —
//! can name the exact bytes it settles.
//!
//! [`EvidenceKind::Question`]: super::EvidenceKind::Question

use alloc::string::String;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use crate::digest::{ContentAddressed, Digest, digest_of};
use crate::ids::{StageId, WorkpieceId};

/// A parked attempt's decision request: the held stage, the member it belongs
/// to, the attempt subject the park is about, and the human-readable decision
/// the owner must settle. The answer that releases the hold is a native
/// [`Statement`](super::Statement) whose parents name this question's digest,
/// so the whole decision context is content-addressed and auditable.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Question {
    /// The stage the parked attempt ran — the stage held until the answer
    /// releases it and re-dispatches.
    pub stage: StageId,
    /// The attempt subject the park is about (the digest the parked attempt
    /// ran against). A question says nothing about any other subject.
    pub subject: Digest,
    /// The member whose stage is held awaiting the decision — the axis the
    /// pending-decision hold binds to when the reducer's view resolves this
    /// question back from its digest.
    pub workpiece: WorkpieceId,
    /// The decision to be made, in plain language (the load-bearing "why"
    /// first), rendered outward onto the held member's own object where a
    /// person already looks.
    pub prompt: String,
    /// The options considered, each with its consequence, for the owner to
    /// choose among.
    pub options: Vec<String>,
    /// What the pending decision blocks — the work that cannot proceed until
    /// the question is answered.
    pub blocked: String,
}

impl ContentAddressed for Question {
    const DOMAIN: &'static str = "aether.bloomery.question";
}

impl Question {
    /// The question's content-addressed identity — the digest an adopting
    /// answer statement names in its `parents`.
    #[must_use]
    pub fn id(&self) -> Digest {
        digest_of(self)
    }

    /// Does this question hold `workpiece`'s stage? True only for the exact
    /// member it names, so the view associates a resolved question with the
    /// one member it belongs to.
    #[must_use]
    pub fn holds(&self, workpiece: &WorkpieceId) -> bool {
        self.workpiece == *workpiece
    }
}

#[cfg(test)]
mod tests {
    use aether_data::wire::{from_bytes, to_vec};

    use super::Question;
    use crate::digest::Digest;
    use crate::ids::{StageId, WorkpieceId};

    fn question() -> Question {
        Question {
            stage: StageId::Construct,
            subject: Digest::from_bytes([2; 32]),
            workpiece: WorkpieceId("reactor-core".into()),
            prompt: "Tie between approach A and approach B; which wins?".into(),
            options: vec!["A — smaller diff".into(), "B — future-proof".into()],
            blocked: "the construct stage cannot proceed until the tie is settled".into(),
        }
    }

    #[test]
    fn a_question_round_trips_through_its_content_addressed_bytes() {
        let question = question();
        let bytes = to_vec(&question).expect("a question wire-encodes");
        let decoded: Question = from_bytes(&bytes).expect("its bytes decode back");
        assert_eq!(decoded, question);
        assert!(question.holds(&WorkpieceId("reactor-core".into())), "the question holds the member it names");
        assert!(!question.holds(&WorkpieceId("other".into())), "and no other");
    }

    #[test]
    fn the_question_digest_is_stable() {
        // Tripwire: the pinned digest is the sha256 over the question domain tag
        // + the question's canonical wire bytes. It drifts if the field set,
        // field order, or the `Question` DOMAIN changes — any of which silently
        // moves the content address of every persisted question, breaking the
        // answer→question adoption that binds to the exact digest.
        let expected = Digest::from_bytes([
            9, 36, 77, 209, 57, 31, 29, 1, 159, 208, 11, 169, 106, 91, 222, 223, 120, 79, 66, 27, 15, 225, 171, 251,
            124, 47, 108, 197, 181, 239, 89, 73,
        ]);
        assert_eq!(question().id(), expected);
    }
}
