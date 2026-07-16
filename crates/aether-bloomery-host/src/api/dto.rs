//! JSON request/response shapes for the REST control API (ADR-0149 §Packaging,
//! issue #3498).
//!
//! These are the wire contract an operator's `curl` speaks — plain serde
//! structs over the `aether-bloomery` value types (`Workpiece`, `BloomDraft`,
//! `Membership`, `Budget`, `Forecast`, `Digest`, and the reducer `Event` /
//! `Outcome` / projection `ViewDocument` / `BloomView`). The value types
//! already derive serde, so the API layer serializes them directly; these
//! structs are the request bodies and the small response envelopes that bundle
//! a minted draft handle alongside the value.
//!
//! They carry no `aether_data::Kind` — they are HTTP-JSON bodies, not mailbox
//! mail, and never cross the wire codec.

use serde::{Deserialize, Serialize};

use aether_bloomery::{Budget, Digest, Event, Forecast, Membership, Workpiece};

/// A draft plus its server-minted handle. The handle keys the in-memory
/// shaping state, so a subsequent `PATCH` / `seal` names the draft by it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DraftView {
    /// The draft's handle (a monotonic per-process id, rendered as a string).
    pub draft_id: String,
    /// The draft's current shape.
    pub draft: aether_bloomery::BloomDraft,
}

/// `GET /workpieces` — every staged workpiece.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkpiecesView {
    /// The staged workpieces, in id order.
    pub workpieces: Vec<Workpiece>,
}

/// `GET /drafts` — every open draft with its handle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DraftsView {
    /// The open drafts, in handle order.
    pub drafts: Vec<DraftView>,
}

/// `PATCH /drafts/{id}` body — every field optional; a present field replaces
/// that part of the draft, an absent one leaves it unchanged.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DraftPatch {
    /// Replace the proposed memberships.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposals: Option<Vec<Membership>>,
    /// Replace the base tree digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<Digest>,
    /// Replace the stage-catalog digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage_catalog: Option<Digest>,
    /// Replace the toolchain digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub toolchain: Option<Digest>,
    /// Replace the policy digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<Digest>,
    /// Replace the budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<Budget>,
    /// Replace the forecast.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forecast: Option<Forecast>,
}

/// `POST /drafts/{id}/seal` body — optional. The idempotency key defaults to
/// the sealed bloom's own id, so re-POSTing the same seal is a no-op duplicate.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SealRequest {
    /// Override the admit idempotency key; defaults to the sealed bloom id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
}

/// `POST /blooms/{id}/supersede` body — names the open draft to seal as the
/// successor. The predecessor bloom id is the `{id}` path segment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SupersedeRequest {
    /// The open draft handle to seal into the successor bloom.
    pub successor_draft: String,
    /// Override the admit idempotency key; defaults to the successor bloom id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
}

/// The reply to a write route: the reducer outcome the admitted event resolved
/// to (decoded from the control core's wire bytes).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutcomeView {
    /// The reducer outcome (sealed / superseded / rejected, and why).
    pub outcome: aether_bloomery::Outcome,
}

/// One decoded journal record for `GET /journal`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEntry {
    /// The record's journal sequence.
    pub sequence: u64,
    /// The record's idempotency key.
    pub idempotency_key: String,
    /// The decoded event the record journaled.
    pub event: Event,
}

/// `GET /journal` — the whole journal, oldest first, decoded.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalView {
    /// Every journaled event, in sequence order.
    pub records: Vec<JournalEntry>,
}

/// A structured error body for a `4xx` / `5xx` reply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorView {
    /// A human-readable failure reason.
    pub error: String,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{DraftPatch, ErrorView};

    #[test]
    fn error_body_is_a_single_error_field() {
        // Tripwire: the error body shape curl reads. A field rename or an added
        // field drifts the contract and breaks this pinned string.
        assert_eq!(serde_json::to_string(&ErrorView { error: "nope".to_owned() }).unwrap(), r#"{"error":"nope"}"#);
    }

    #[test]
    fn empty_patch_serializes_to_empty_object() {
        // Tripwire: `skip_serializing_if = "Option::is_none"` on every field is
        // what makes a partial `PATCH` partial — an absent field must not emit a
        // `null` that would clobber that part of the draft on a round-trip.
        assert_eq!(serde_json::to_string(&DraftPatch::default()).unwrap(), "{}");
    }
}
