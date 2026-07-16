//! The study-stage evidence record (ADR-0149 §The bloom, issue #3523).
//!
//! A `construct.implement` attempt uploads one runner **result record** — the
//! cost / tokens / turns / duration object `scripts/agent-usage-record.mjs`
//! derives. Normalized, that record becomes a [`StudyRecord`]: the typed,
//! content-addressed **study evidence** ADR-0149's study stage grades against a
//! sealed bloom's [`Forecast`](super::Forecast).
//!
//! A study record is a standalone artifact, **not** an
//! [`Evidence`](super::Evidence): it never enters the reducer through the closed
//! [`EvidenceKind`](super::EvidenceKind) / `Fact` vocabulary (that reducer-side
//! admission is deferred to #3525). Its canonical bytes are the truth — `put`
//! into `aether.artifacts` with the graded attempt digest as a derivation
//! parent — so the per-bloom study index the host projects over them is always
//! rebuildable and never a second source of truth.

use serde::{Deserialize, Serialize};

use crate::digest::{ContentAddressed, Digest, digest_of};
use crate::ids::BloomId;

/// The gradeable cost columns a runner result record carries — the subset of
/// `scripts/agent-usage-record.mjs`'s object the study stage grades. Token
/// counts and durations are integral by nature; the dollar cost is carried in
/// **micro-USD** (`total_cost_usd` × `1_000_000`) so the whole record is `Eq`
/// and content-addressable — a float dollar amount is not a stable address.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct StudyCost {
    /// The attempt's total cost in micro-USD (`total_cost_usd` × `1_000_000`).
    pub cost_micro_usd: u64,
    /// The number of agent turns the attempt took (`num_turns`).
    pub turns: u64,
    /// The attempt's wall-clock duration in milliseconds (`duration_ms`).
    pub duration_millis: u64,
    /// Uncached input tokens (`input`).
    pub input_tokens: u64,
    /// Cache-write tokens (`cache_write`).
    pub cache_write_tokens: u64,
    /// The 1-hour-TTL cache-write split (`cache_write_1h`).
    pub cache_write_1h_tokens: u64,
    /// The 5-minute-TTL cache-write split (`cache_write_5m`).
    pub cache_write_5m_tokens: u64,
    /// Cache-read tokens (`cache_read`).
    pub cache_read_tokens: u64,
    /// Output tokens (`output`).
    pub output_tokens: u64,
}

/// A normalized runner result record: the typed study evidence for one
/// `construct.implement` attempt, bound to the exact attempt digest it grades.
///
/// `subject` is the displayed attempt digest the upload bound to — a study
/// record says nothing about any other attempt, the value-vocabulary invariant
/// [`Evidence`](super::Evidence) holds for verdicts. `bloom` names the sealed
/// bloom whose [`Forecast`](super::Forecast) the study report grades this
/// record against; it lives in the record's own bytes so the per-bloom study
/// index projected over the artifact store is rebuildable from the bytes alone,
/// with no dependence on a live outstanding-order row.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct StudyRecord {
    /// The sealed bloom this attempt's cost is graded against.
    pub bloom: BloomId,
    /// The exact attempt digest this record grades (the displayed digest).
    pub subject: Digest,
    /// The graded cost columns.
    pub cost: StudyCost,
}

impl ContentAddressed for StudyRecord {
    const DOMAIN: &'static str = "aether.bloomery.study_record";
}

impl StudyRecord {
    /// The record's content-addressed identity.
    #[must_use]
    pub fn id(&self) -> Digest {
        digest_of(self)
    }

    /// Does this record grade `attempt`? True only for the exact digest it
    /// names — the "no evidence validates a digest it does not name" invariant,
    /// at the type level.
    #[must_use]
    pub fn grades(&self, attempt: &Digest) -> bool {
        self.subject == *attempt
    }
}

#[cfg(test)]
mod tests {
    use aether_data::wire::{from_bytes, to_vec};

    use super::{StudyCost, StudyRecord};
    use crate::digest::Digest;
    use crate::ids::BloomId;

    fn record() -> StudyRecord {
        StudyRecord {
            bloom: BloomId(Digest::from_bytes([1; 32])),
            subject: Digest::from_bytes([2; 32]),
            cost: StudyCost {
                cost_micro_usd: 420_000,
                turns: 7,
                duration_millis: 123_456,
                input_tokens: 1_000,
                cache_write_tokens: 200,
                cache_write_1h_tokens: 150,
                cache_write_5m_tokens: 50,
                cache_read_tokens: 8_000,
                output_tokens: 900,
            },
        }
    }

    #[test]
    fn a_study_record_round_trips_through_its_content_addressed_bytes() {
        let record = record();
        let bytes = to_vec(&record).expect("a study record wire-encodes");
        let decoded: StudyRecord = from_bytes(&bytes).expect("its bytes decode back");
        assert_eq!(decoded, record);
        assert!(record.grades(&Digest::from_bytes([2; 32])), "the record grades the digest it names");
        assert!(!record.grades(&Digest::from_bytes([9; 32])), "and no other");
    }

    #[test]
    fn the_study_record_digest_is_stable() {
        // Tripwire: the pinned digest is the sha256 over the study-record domain
        // tag + the record's canonical wire bytes. It drifts if the field set,
        // field order, or the `StudyRecord` DOMAIN changes — any of which
        // silently moves the content address of every persisted study record.
        let expected = Digest::from_bytes([
            67, 178, 85, 6, 66, 27, 57, 4, 90, 83, 148, 183, 137, 160, 73, 73, 110, 65, 107, 163, 184, 159, 173, 36,
            177, 135, 181, 39, 67, 247, 93, 204,
        ]);
        assert_eq!(record().id(), expected);
    }
}
