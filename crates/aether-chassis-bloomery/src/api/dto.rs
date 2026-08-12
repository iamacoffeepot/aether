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

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use aether_bloomery::{
    Budget, ConfigRegistry, Digest, Event, Forecast, Membership, StageId, Statement, Workpiece, WorkpieceId,
};

use crate::bloomery::{AdrTouch, Completeness};

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
    /// Replace the bloom configuration registry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configs: Option<ConfigRegistry>,
    /// Replace the base tree digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<Digest>,
    /// Replace the budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<Budget>,
    /// Replace the forecast.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forecast: Option<Forecast>,
}

/// The seal-time scope projection an operator supplies per draft membership so
/// the pre-seal approve gate (issue #3583, the enforcement half of #3571) can
/// decide the member's admission. It mirrors the gate's
/// [`AdmissionRequest`](crate::bloomery::AdmissionRequest) inputs, keyed
/// by `{workpiece, scope_revision}` so the host matches it to the exact draft
/// proposal. These are seal-time-only inputs the gate consumes; they are never
/// folded into the immutable sealed `BloomSpec` — the `SealRequest` is their only
/// home (ADR-0150; the authenticated operator harness attests them).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemberProjection {
    /// The workpiece this projection describes — matches a draft proposal's
    /// `workpiece`.
    pub workpiece: WorkpieceId,
    /// The exact scope-revision digest — matches the proposal's `scope_revision`
    /// and the digest the formed approval binds.
    pub scope_revision: Digest,
    /// The declared-surface globs the tier policy resolves over.
    pub declared_surface: Vec<String>,
    /// The nine completeness facts the gate fails closed on.
    pub completeness: Completeness,
    /// The ADR-maturity of the change, for the unconditional hard gate.
    pub adr_touch: AdrTouch,
    /// Whether an owner-actor-verified `approval:pre-approved` override is
    /// present (waives the tier to `auto`, never the gate checks).
    pub pre_approved: bool,
    /// The owner-signed statement for an above-`auto` member. Consumed by the
    /// deferred-verify enforcement (its live wiring is the follow-up child #3599);
    /// an above-`auto` member fails closed until then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signed_statement: Option<Statement>,
}

/// `POST /drafts/{id}/seal` body — optional. The idempotency key defaults to
/// the sealed bloom's own id, so re-POSTing the same seal is a no-op duplicate.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SealRequest {
    /// Override the admit idempotency key; defaults to the sealed bloom id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    /// The per-member scope projections the pre-seal approve gate decides over
    /// (issue #3583). Every draft proposal must resolve one, matched by
    /// `{workpiece, scope_revision}`; a proposal with no projection fails closed
    /// (the seal is refused), so an empty list refuses any non-empty draft.
    #[serde(default)]
    pub projections: Vec<MemberProjection>,
    /// The operator-supplied, per-member work-order descriptions (#3595), keyed
    /// by workpiece id. Advisory model context the coordinator persists at seal
    /// so the construct lane's prompt can name a `## Task` — it binds no evidence
    /// and never enters the content-addressed spec, so it rides the seal *request*
    /// rather than the sealed draft. A member absent from the map dispatches with
    /// no task (the subject-only prompt), never blocking the seal.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub descriptions: BTreeMap<String, String>,
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
    /// One projection per successor member, the same shape [`SealRequest`]
    /// carries. Supersession is a second door into `active` claiming a fresh
    /// membership set, so it runs the same approve gate a first seal does and
    /// takes the gate-authored approval from it (#4638) — a draft's own
    /// `approval` is a placeholder either way.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub projections: Vec<MemberProjection>,
    /// The successor's per-member work-order descriptions, keyed by workpiece id
    /// — the same advisory model context [`SealRequest::descriptions`] carries,
    /// and required here for the same reason (#4631).
    ///
    /// Descriptions are stored per `(bloom, workpiece)`, and a successor is a new
    /// bloom id by construction, so the predecessor's rows do not resolve for it —
    /// not even for the members the supersession carries unchanged. Omitting them
    /// dispatches every member of the successor on a subject-only prompt, which
    /// warns and continues rather than failing, so the run looks normal and only
    /// its quality collapses.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub descriptions: BTreeMap<String, String>,
}

/// `POST /blooms/{id}/grant` body — hand a wedged member more attempts on the
/// bloom it already belongs to (#4708), instead of superseding an unchanged one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrantRequest {
    /// The wedged member to resume.
    pub workpiece: WorkpieceId,
    /// The stage the grant believes the member is wedged at — the reducer
    /// refuses a grant naming any other, so a stale read cannot act.
    pub stage: StageId,
    /// How many more dispatched attempts the member may spend before it wedges
    /// again.
    pub attempts: u32,
    /// Override the admit idempotency key.
    ///
    /// The default is derived from the grant's own content, so re-POSTing the
    /// same request is a no-op duplicate rather than a second grant. That also
    /// means a *deliberate* second grant of the same shape — the member wedged
    /// again and the operator is buying another round — has to name its own key.
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
