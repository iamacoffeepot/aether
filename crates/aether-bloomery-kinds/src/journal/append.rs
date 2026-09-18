//! The one caused write: append the native bundle driver's own records (ADR-0226 decision 10).

use alloc::string::String;
use alloc::vec::Vec;

use crate::{
    Activated, ActivationRejected, Digest, EncodedArtifact, Fault, ReactionFailed, RecordedHeadMove, Requested,
    Transition,
};

/// One driver-owned record, typed with the journal cause it carries (ADR-0226 decisions 3, 6, 8).
///
/// Typing the cause per variant, instead of one shared `Option<u64>`, makes
/// an uncaused `Transition` or similar impossible to represent: only
/// `Requested` may be uncaused, and only for a `Native` source. Every other
/// variant's cause is a plain `u64`.
///
/// Each stored value decodes through its own validating wire codec, so this
/// enum adds no validation of its own; the journal appends the concrete
/// value it matches out, never caller bytes. Dedup on `(cause, source)`,
/// checking that a `Transition` / `Fault` cause names a `Requested` entry,
/// and checking that a `Requested` source matches its cause are the
/// driver's invariants, not the journal's.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Schema)]
pub enum DriverRecord {
    /// A program request. Caused by the trigger seq for a `Reaction` source; uncaused for `Native`.
    Requested { cause: Option<u64>, record: Requested },
    /// One execution. Caused by its `Requested` seq.
    Transition { cause: u64, record: Transition },
    /// An attempt that produced no execution. Caused by its `Requested` seq.
    Fault { cause: u64, record: Fault },
    /// A reactor instance brought live for a head. Caused by the boundary seq.
    Activated { cause: u64, record: Activated },
    /// A driver activation attempt that failed. Caused by the boundary seq.
    ActivationRejected { cause: u64, record: ActivationRejected },
    /// A reactor reaction that failed. Caused by the trigger seq.
    ReactionFailed { cause: u64, record: ReactionFailed },
    /// A head move caused by a reactor's `SetHead`. Caused by the trigger seq.
    HeadMoved { cause: u64, record: RecordedHeadMove },
}

/// Stage encoded artifacts and append driver records under the whole-journal fence.
///
/// Every record's cause must lie in `1..=expected_seq`; a cause outside that
/// range is refused before anything is written. The fence is
/// `expected_seq`, exactly as [`crate::Publish`]'s. Artifacts stage first,
/// in request order; records append next, also in request order, at
/// consecutive seqs starting at `expected_seq + 1`, each under its own
/// cause. Staging and appending happen in one transaction: any refusal
/// rolls back every staged artifact and every record. A `Transition`'s
/// `input` and `result` must each be stored or staged in the same command,
/// or the whole append is refused — the journal checks only that they
/// exist, never their declared kind prefix. Dedup, and every other
/// driver-side invariant, is the driver's, not this command's.
#[aether_data::kind(name = "aether.bloomery.journal.append_records", eq, no_serde)]
pub struct AppendRecords {
    artifacts: Vec<EncodedArtifact>,
    records: Vec<DriverRecord>,
    expected_seq: u64,
}

impl AppendRecords {
    /// Stage `artifacts` and append `records` if the journal's last stored sequence is still `expected_seq`.
    #[must_use]
    pub fn new(artifacts: Vec<EncodedArtifact>, records: Vec<DriverRecord>, expected_seq: u64) -> Self {
        Self { artifacts, records, expected_seq }
    }

    /// Artifacts to stage, in request order.
    #[must_use]
    pub fn artifacts(&self) -> &[EncodedArtifact] {
        &self.artifacts
    }

    /// Records to append, in request order.
    #[must_use]
    pub fn records(&self) -> &[DriverRecord] {
        &self.records
    }

    /// Whole-journal fence: the last stored sequence the caller observed.
    #[must_use]
    pub const fn expected_seq(&self) -> u64 {
        self.expected_seq
    }

    /// Take the artifacts, records, and fence without copying payload bytes.
    #[must_use]
    pub fn into_parts(self) -> (Vec<EncodedArtifact>, Vec<DriverRecord>, u64) {
        (self.artifacts, self.records, self.expected_seq)
    }
}

/// Outcome of one fenced [`AppendRecords`]. Mirrors [`crate::PublishResult`]
/// so the driver has one reply shape to handle for every fenced write.
#[aether_data::kind(name = "aether.bloomery.journal.append_records_result", eq, no_serde)]
pub enum AppendRecordsResult {
    /// Everything was written. `head` is the journal head after the append;
    /// the records occupy `expected_seq + 1 ..= head`. `artifacts` are the
    /// staged digests in request order.
    Committed { head: u64, artifacts: Vec<Digest> },
    /// The supplied whole-journal fence was stale; nothing was written.
    Conflict { actual: u64 },
    /// A cause outside the fenced prefix, a `Transition` whose input or
    /// result is neither stored nor staged, citation or destination
    /// validation, or the journal backend refused the append.
    Err { message: String },
}
