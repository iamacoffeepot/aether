//! Operator-facing kinds the janitor reactor answers, distinct from the poll tick.

use serde::{Deserialize, Serialize};

use aether_data::Kind;

/// `POST /archive` — run the between-blooms archive pass.
#[derive(Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default)]
#[kind(name = "aether.bloomery.janitor.archive_records")]
pub struct ArchiveRecords {}

/// One archived record as the janitor reports it.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ArchivedRecordView {
    /// `evidence` or `session`.
    pub class: String,
    /// The name the record was addressed by.
    pub name: String,
    /// Path on the tier.
    pub path: String,
    /// Total file bytes under the tree.
    pub bytes: u64,
}

/// Why one record in an otherwise successful archive pass did not move.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ArchiveFailureView {
    /// `evidence` or `session`.
    pub class: String,
    /// The name the record was addressed by.
    pub name: String,
    /// Why the move did not complete.
    pub error: String,
}

/// Reply to [`ArchiveRecords`].
#[derive(Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.bloomery.janitor.archive_records_result")]
pub enum ArchiveRecordsResult {
    /// Eligible records moved, plus per-record failures that left their source
    /// in place.
    Archived {
        /// Records that now live on the tier.
        records: Vec<ArchivedRecordView>,
        /// Records that could not move.
        failures: Vec<ArchiveFailureView>,
    },
    /// The coordinator is not between blooms. Nothing moved.
    Refused {
        /// The walking bloom or outstanding nonce.
        reason: String,
    },
    /// The store or the tier could not be read.
    Errored {
        /// A human-readable failure reason.
        error: String,
    },
}

/// `GET /archive` — list the tier.
#[derive(Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default)]
#[kind(name = "aether.bloomery.janitor.list_archive")]
pub struct ListArchive {}

/// Reply to [`ListArchive`].
#[derive(Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.bloomery.janitor.list_archive_result")]
pub enum ListArchiveResult {
    /// The records currently on the tier.
    Ok {
        /// Evidence then session trees, each class sorted by name.
        records: Vec<ArchivedRecordView>,
    },
    /// The tier could not be read.
    Err {
        /// A human-readable failure reason.
        error: String,
    },
}
