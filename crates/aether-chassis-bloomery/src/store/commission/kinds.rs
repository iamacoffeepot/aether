//! Transact-mail for the commission repository. Clients never see SQL.

use serde::{Deserialize, Serialize};

/// Persist a new open commission and its intent statement.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.store.create_commission")]
pub struct CreateCommission {
    /// The workpiece this commission is.
    pub id: String,
    /// Wire-encoded [`aether_bloomery::Statement`] intent.
    #[serde(with = "aether_data::bytes")]
    pub intent: Vec<u8>,
}

/// Reply to [`CreateCommission`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.store.create_commission_result")]
pub enum CreateCommissionResult {
    /// The intent was stored; `digest` is its content address.
    Ok {
        /// The workpiece this commission is.
        id: String,
        /// Digest of the stored intent statement.
        #[serde(with = "aether_data::bytes")]
        digest: Vec<u8>,
    },
    /// A commission under this workpiece id already exists.
    Duplicate {
        /// The workpiece id that was taken.
        id: String,
    },
    /// The write failed.
    Err {
        /// A human-readable failure reason.
        error: String,
    },
}

/// Store an immutable scope revision and advance `current`.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.store.write_scope_revision")]
pub struct WriteScopeRevision {
    /// Canonical [`aether_bloomery::ScopeRevision`] bytes.
    #[serde(with = "aether_data::bytes")]
    pub canonical: Vec<u8>,
}

/// Reply to [`WriteScopeRevision`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.store.write_scope_revision_result")]
pub enum WriteScopeRevisionResult {
    /// The revision was stored (or was already the current tip).
    Ok {
        /// Digest of the stored revision.
        #[serde(with = "aether_data::bytes")]
        digest: Vec<u8>,
    },
    /// No commission exists under this workpiece id.
    Missing {
        /// The workpiece id that was missing.
        id: String,
    },
    /// The predecessor is not the commission's current tip.
    Stale,
    /// The revision is already stored under a different chain position.
    Duplicate,
    /// The revision is not the next ordinal.
    Ordinal {
        /// The ordinal the tip requires next.
        expected: u64,
    },
    /// The bytes decoded as a schema this binary does not write.
    UnsupportedSchema {
        /// The schema number in the bytes.
        schema: u32,
    },
    /// Canonical bytes did not decode, or failed an index check.
    Malformed,
    /// The write failed.
    Err {
        /// A human-readable failure reason.
        error: String,
    },
    /// The commission is not open.
    NotOpen,
}

/// Persist an approval whose signature the caller has already verified.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.store.record_commission_approval")]
pub struct RecordCommissionApproval {
    /// The workpiece the caller addressed. The revision named by the
    /// statement must belong to this commission.
    pub id: String,
    /// Wire-encoded [`aether_bloomery::Statement`].
    #[serde(with = "aether_data::bytes")]
    pub statement: Vec<u8>,
}

/// Reply to [`RecordCommissionApproval`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.store.record_commission_approval_result")]
pub enum RecordCommissionApprovalResult {
    /// The approval was stored (or was already present).
    Ok {
        /// Digest of the stored statement.
        #[serde(with = "aether_data::bytes")]
        digest: Vec<u8>,
        /// The exact statement bytes that were written.
        #[serde(with = "aether_data::bytes")]
        statement: Vec<u8>,
    },
    /// The named scope revision is not in the store.
    MissingRevision,
    /// The named scope revision is not the commission's current tip.
    Stale,
    /// The statement's words are not a scope digest, or provenance is wrong.
    Refused {
        /// A human-readable refusal reason.
        error: String,
    },
    /// The write failed.
    Err {
        /// A human-readable failure reason.
        error: String,
    },
    /// The commission is not open.
    NotOpen,
}

/// Load one commission and recompute its current revision from canonical bytes.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.store.load_commission")]
pub struct LoadCommission {
    /// The workpiece this commission is.
    pub id: String,
}

/// Reply to [`LoadCommission`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.store.load_commission_result")]
pub enum LoadCommissionResult {
    /// The commission exists; index columns were recomputed from stored bytes.
    Ok {
        /// The workpiece this commission is.
        id: String,
        /// Digest of the stored intent statement.
        #[serde(with = "aether_data::bytes")]
        intent: Vec<u8>,
        /// Digest of the current scope revision, when one has been written.
        current_revision: Option<Vec<u8>>,
        /// Store-side chain position of the current revision.
        current_ordinal: Option<u64>,
        /// Lifecycle flag. Not signed.
        status: String,
        /// Canonical bytes of the current revision, when present.
        current: Option<Vec<u8>>,
        /// Wire-encoded approval statements for the current revision, in insert order.
        approvals: Vec<Vec<u8>>,
    },
    /// No commission exists under this workpiece id.
    Missing {
        /// The workpiece id that was missing.
        id: String,
    },
    /// The read failed.
    Err {
        /// A human-readable failure reason.
        error: String,
    },
}

/// List commissions, optionally filtered by status.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.store.list_commissions")]
pub struct ListCommissions {
    /// Lifecycle filter, or `None` for every commission.
    pub status: Option<String>,
}

/// One listed commission head.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ListedCommission {
    /// The workpiece this commission is.
    pub id: String,
    /// Digest of the stored intent statement.
    #[serde(with = "aether_data::bytes")]
    pub intent: Vec<u8>,
    /// Digest of the current scope revision, when one has been written.
    pub current_revision: Option<Vec<u8>>,
    /// Store-side chain position of the current revision.
    pub current_ordinal: Option<u64>,
    /// Lifecycle flag. Not signed.
    pub status: String,
}

/// Reply to [`ListCommissions`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.store.list_commissions_result")]
pub enum ListCommissionsResult {
    /// Matching commissions, in workpiece-id order.
    Ok {
        /// The listed heads.
        commissions: Vec<ListedCommission>,
    },
    /// The read failed.
    Err {
        /// A human-readable failure reason.
        error: String,
    },
}

/// Persist the GitHub issue number a commission projector created.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.store.record_commission_projection")]
pub struct RecordCommissionProjection {
    /// The workpiece this commission is.
    pub id: String,
    /// The issue number recorded from this projector's own create.
    pub issue_number: u64,
}

/// Reply to [`RecordCommissionProjection`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.store.record_commission_projection_result")]
pub enum RecordCommissionProjectionResult {
    /// The number is now recorded.
    Ok {
        /// The workpiece this commission is.
        id: String,
        /// The recorded issue number.
        issue_number: u64,
    },
    /// No commission exists under this workpiece id.
    Missing {
        /// The workpiece id that was missing.
        id: String,
    },
    /// The write failed.
    Err {
        /// A human-readable failure reason.
        error: String,
    },
}

/// Store a signed cancel and close the commission.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.store.cancel_commission")]
pub struct CancelCommission {
    /// The workpiece this commission is.
    pub id: String,
    /// Wire-encoded cancel [`aether_bloomery::Statement`].
    #[serde(with = "aether_data::bytes")]
    pub statement: Vec<u8>,
}

/// Reply to [`CancelCommission`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.store.cancel_commission_result")]
pub enum CancelCommissionResult {
    /// The commission is now cancelled.
    Ok {
        /// The workpiece this commission is.
        id: String,
        /// Digest of the stored cancel statement.
        #[serde(with = "aether_data::bytes")]
        digest: Vec<u8>,
    },
    /// No commission exists under this workpiece id.
    Missing {
        /// The workpiece id that was missing.
        id: String,
    },
    /// The commission is not open.
    NotOpen,
    /// The statement's words are not the stored intent digest.
    WrongSubject,
    /// The write failed.
    Err {
        /// A human-readable failure reason.
        error: String,
    },
}
