//! JSON request/response shapes for the REST control API (ADR-0149 §Packaging,
//! issue #3498).
//!
//! The envelope types live in [`aether_bloomery`] so the coordinator and its
//! clients share one declaration. This module re-exports them so the API's
//! own call sites keep reading `crate::api::dto::…`, and keeps the one
//! envelope that names `serde_json::Value` — that crate is not a
//! `aether-bloomery` dependency.

pub use aether_bloomery::{
    AdjudicateRequest, ArchiveFailureView, ArchiveListView, ArchivePassView, ArchiveRecordView, BloomDispatchView,
    BloomDispatchesView, CancelCommissionRequest, ClaimRefView, ClaimsView, CommissionApprovalView,
    CommissionCancelledView, CommissionCreatedView, CommissionHeadView, CommissionReopenedView, CommissionShowView,
    CommissionsView, CoordinatorLogEntry, CoordinatorLogsView, CreateCommissionRequest, DispatchEvidenceView,
    DispatchFilePage, DispatchProcessView, DraftPatch, DraftView, DraftsView, ErrorView, GrantRequest, HoldRequest,
    JournalEntry, JournalView, MemberProjection, OutcomeView, ReleaseAcceptedView, ReleaseRequest,
    ReopenCommissionRequest, RepairRequest, RetryRequest, ReverifyBaseRequest, ScopeRevisionWrittenView,
    ScopeRunOpenedView, ScopeRunRequest, SealRequest, SupersedeRequest, SuppressionAnswerRequest, WithdrawRequest,
    WorkpiecesView, WriteRevisionRequest,
};

use serde::{Deserialize, Serialize};

/// `GET /artifacts/{digest}/decoded` — a known kind, or a raw range.
///
/// Kept here rather than with the shared envelopes: the decoded-JSON slot is
/// `serde_json::Value`, and `aether-bloomery` does not take that dependency.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecodedArtifactView {
    /// The content-address domain of the resolved type, or `null`.
    pub kind: Option<String>,
    /// The decoded value, present only when [`kind`](Self::kind) is set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<serde_json::Value>,
    /// A raw byte range, present only when no known type matched.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<Vec<u8>>,
    /// Offset of [`bytes`](Self::bytes) when this is a raw fallback.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<u64>,
    /// Full artifact length when this is a raw fallback.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
    /// Whether the raw fallback was truncated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
    /// Set when the caller named a `limit` above the clamp.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notice: Option<String>,
}
