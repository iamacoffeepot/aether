//! JSON shapes the coordinator REST edge already speaks.
//!
//! Envelope types live in [`aether_bloomery`]. This module keeps the hex
//! wrapper CLI flags still need, plus the few request/reply shapes that are
//! this client's rather than the coordinator's.

use std::fmt;

#[cfg(test)]
use aether_bloomery::{BloomId, BloomStatus};
use aether_bloomery::{Digest, Evidence, EvidenceKind, Membership, WorkpieceId};
use serde::de::Error;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use super::hex;

pub use aether_bloomery::{
    ArchiveFailureView, ArchiveListView, ArchivePassView, ArchiveRecordView, BloomSpec, BloomView,
    CancelCommissionRequest, CandidateRef, CommissionCancelledView, CommissionReopenedView, CommissionShowView,
    ConfigRegistry, DraftPatch, DraftView, JournalEntry, JournalView, MemberView, OutcomeView, ReopenCommissionRequest,
    RepairRequest, RetryRequest, ReverifyBaseRequest, RevisionEvidence, ScopeRevisionWrittenView, SealRequest,
    SupersedeRequest, SuppressionAnswerRequest, SuppressionVerdict, ViewDocument, WithdrawRequest,
    WriteRevisionRequest,
};

/// A digest as the REST edge renders it — CLI flags and operator-facing print.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DigestHex([u8; 32]);

impl DigestHex {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn as_hex(&self) -> String {
        hex::encode(&self.0)
    }

    pub fn digest(self) -> Digest {
        Digest::from_bytes(self.0)
    }
}

impl From<Digest> for DigestHex {
    fn from(digest: Digest) -> Self {
        Self(*digest.as_bytes())
    }
}

impl From<DigestHex> for Digest {
    fn from(digest: DigestHex) -> Self {
        Self::from_bytes(digest.0)
    }
}

impl fmt::Display for DigestHex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_hex())
    }
}

impl Serialize for DigestHex {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.as_hex())
    }
}

impl<'de> Deserialize<'de> for DigestHex {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        hex::from_json(&value, "digest").map(Self).map_err(D::Error::custom)
    }
}

/// `GET /configs/{digest}` — a stored configuration, decoded through its kind's
/// schema.
#[derive(Debug, Clone, Deserialize)]
pub struct ConfigValueView {
    pub kind: String,
    pub value: Value,
}

/// `POST /configs` body.
#[derive(Debug, Serialize)]
pub struct ConfigRequest<'a> {
    pub kind: &'a str,
    pub value: &'a Value,
}

/// `POST /configs` reply.
#[derive(Debug, Deserialize)]
pub struct ConfigView {
    pub digest: Digest,
    pub kind: String,
}

/// `POST /commissions/{id}/approvals` — the stored approval's address. The
/// address itself is the caller's own signed statement re-rendered, so nothing
/// here reads it back; the type is what makes a non-object reply a parse
/// failure rather than a silent success.
#[derive(Debug, Clone, Deserialize)]
pub struct ApprovalStoredView {}

/// A reviewer's answer to the suppression requests a candidate is carrying.
///
/// Spelled here rather than reused from `aether_bloomery` so the CLI can derive
/// `clap::ValueEnum` on it; the variant names are the wire's, so the serialized
/// value is the one the route decodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, clap::ValueEnum)]
pub enum SuppressionVerdictArg {
    /// The suppressions may stand; the candidate keeps them and continues.
    Granted,
    /// They may not. The member re-opens at `Refine` carrying the denial's
    /// reason, at its own repair budget's expense.
    Denied,
}

impl From<SuppressionVerdictArg> for SuppressionVerdict {
    fn from(verdict: SuppressionVerdictArg) -> Self {
        match verdict {
            SuppressionVerdictArg::Granted => Self::Granted,
            SuppressionVerdictArg::Denied => Self::Denied,
        }
    }
}

/// A member for tests and local construction, with dummy approval evidence.
#[cfg(test)]
pub fn test_member(workpiece: &str, revision: impl Into<Digest>) -> MemberView {
    let revision = revision.into();
    MemberView {
        workpiece: WorkpieceId(workpiece.to_owned()),
        scope_revision: revision,
        approval: Evidence { subject: revision, kind: EvidenceKind::Approval, detail: Digest::from_bytes([9; 32]) },
        resolution: None,
        pending_decision: None,
        wedge: None,
        blocked_by: None,
        host_fault: None,
        machinery_rolls: 0,
        machinery_budget: 0,
        wedge_cause: None,
        cursor: None,
        park: None,
        awaiting_surface: None,
        withdrawn: None,
        leases: Vec::new(),
        evicted_by: None,
    }
}

/// A bloom for tests, with the load-bearing optional fields empty.
#[cfg(test)]
pub fn test_bloom(id: impl Into<Digest>, status: BloomStatus, members: Vec<MemberView>) -> BloomView {
    BloomView {
        id: BloomId(id.into()),
        status,
        superseded_by: None,
        members,
        landing_blocked: None,
        executor_fault: None,
        review_park: None,
        composition: None,
        operator_hold: None,
        blocker: None,
        leases: Vec::new(),
        narrowed_compositions: Vec::new(),
    }
}

/// A view document for tests.
#[cfg(test)]
pub fn test_view(mainline: impl Into<Digest>, observed: impl Into<Digest>, blooms: Vec<BloomView>) -> ViewDocument {
    ViewDocument { mainline: mainline.into(), observed: observed.into(), blooms, ..ViewDocument::default() }
}

/// A draft membership with placeholder approval evidence.
pub fn placeholder_member(workpiece: &str, scope_revision: Digest) -> Membership {
    Membership {
        workpiece: WorkpieceId(workpiece.to_owned()),
        scope_revision,
        configs: ConfigRegistry::default(),
        approval: Evidence {
            subject: scope_revision,
            kind: EvidenceKind::Approval,
            detail: Digest::from_bytes([0; 32]),
        },
    }
}
