//! JSON shapes the coordinator REST edge already speaks.
//!
//! Digests are 64 hex characters on the wire. These types serialize that
//! spelling and accept either hex or the canonical 32-byte array on the way
//! in, so the command never composes a raw body and never re-encodes a
//! digest the edge already rendered.

use std::collections::BTreeMap;
use std::fmt;

use aether_bloomery::{
    BloomStatus, Digest, EvidenceKind, Forecast, ScopeRevision, ScopeRouting, ScopeVerifyInput, Statement, WorkpieceId,
};
use serde::de::Error;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use super::hex;

/// A digest as the REST edge renders it.
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

/// `GET /view` / `GET /blooms`.
#[derive(Debug, Clone, Deserialize)]
pub struct ViewDocument {
    pub mainline: DigestHex,
    pub observed: DigestHex,
    pub blooms: Vec<BloomView>,
}

/// One bloom in the live projection.
#[derive(Debug, Clone, Deserialize)]
pub struct BloomView {
    pub id: DigestHex,
    pub status: BloomStatus,
    pub superseded_by: Option<DigestHex>,
    pub members: Vec<MemberView>,
}

/// One sealed member as the projection renders it.
#[derive(Debug, Clone, Deserialize)]
pub struct MemberView {
    pub workpiece: String,
    pub scope_revision: DigestHex,
    /// The surface amendment this member is waiting on (ADR-0207). Absent from
    /// a coordinator that predates the field, so `#[serde(default)]`.
    #[serde(default)]
    pub awaiting_surface: Option<AwaitingSurfaceView>,
    /// Why the member left the line, once it was withdrawn (#5327). Absent
    /// from a coordinator that predates the field, so `#[serde(default)]`.
    #[serde(default)]
    pub withdrawn: Option<WithdrawnView>,
    /// The member's stage cursor: where it is and what it is carrying. Absent
    /// for a member that has never entered the line, and from a coordinator
    /// that predates the field, so `#[serde(default)]`.
    #[serde(default)]
    pub cursor: Option<MemberCursorView>,
}

/// A member's stage cursor, as `/view` renders it — the two facts a retry has
/// to name: which stage to run again, and the subject the fault binds to.
#[derive(Debug, Clone, Deserialize)]
pub struct MemberCursorView {
    /// The stage token exactly as the coordinator spells it. Carried as text
    /// and handed straight back on the retry body rather than parsed into a
    /// local vocabulary, so the CLI holds no second copy of the stage list to
    /// drift from the sealed one.
    pub stage: String,
    /// The candidate the member is carrying, when it has captured one.
    #[serde(default)]
    pub candidate: Option<CandidateRefView>,
}

/// A candidate the member holds, as `/view` renders it.
///
/// Only the tree is mirrored. The capture commit is the reducer's to resolve,
/// and what a retry names is the subject its fault evidence binds to — which is
/// the tree.
#[derive(Debug, Clone, Deserialize)]
pub struct CandidateRefView {
    /// The produced tree — the candidate's identity, and the subject a member
    /// stage past Construct is judged against.
    pub tree: DigestHex,
}

/// A member the day withdrew, as `/view` renders it (#5327). The projection
/// names the cause, the stranded ancestor, the reason and the operator; every
/// one of them answers "this member never integrates" the same way, so the
/// mirror keeps the presence and reads nothing out of the body.
#[derive(Debug, Clone, Deserialize)]
pub struct WithdrawnView {}

/// A member's journaled surface request, as `/view` renders it (ADR-0207).
#[derive(Debug, Clone, Deserialize)]
pub struct AwaitingSurfaceView {
    pub scope_revision: DigestHex,
    pub paths: Vec<SurfacePathRequest>,
    #[serde(default)]
    pub requests: u32,
}

/// One path a declining lane asked for, and the line justifying it.
#[derive(Debug, Clone, Deserialize)]
pub struct SurfacePathRequest {
    pub path: String,
    #[serde(default)]
    pub reason: String,
}

/// `GET /commissions/{id}` — the tip, typed, plus the approvals stored against
/// it. Digests arrive as hex, which is why this mirrors rather than reuses
/// `aether_bloomery`'s own type.
#[derive(Debug, Clone, Deserialize)]
pub struct CommissionShowView {
    pub intent: DigestHex,
    pub status: String,
    pub current_revision: Option<DigestHex>,
    pub current: Option<ScopeRevisionView>,
    #[serde(default)]
    pub approvals: Vec<StatementView>,
}

/// A stored scope revision as the REST edge renders it.
#[derive(Debug, Clone, Deserialize)]
pub struct ScopeRevisionView {
    pub schema: u32,
    pub workpiece: String,
    pub predecessor: Option<DigestHex>,
    pub problem: String,
    pub design: String,
    pub plan: String,
    pub declared_surface: Vec<String>,
    #[serde(default)]
    pub dogfood_brief: String,
    pub routing: ScopeRoutingView,
    #[serde(default)]
    pub dependencies: Vec<String>,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub implements: Vec<DigestHex>,
    #[serde(default)]
    pub declared_crates: Vec<String>,
    #[serde(default)]
    pub declared_reads: Vec<String>,
}

/// The size and model-routing lines a revision seals.
#[derive(Debug, Clone, Deserialize)]
pub struct ScopeRoutingView {
    pub size: String,
    pub model: String,
}

impl ScopeRevisionView {
    /// The typed revision these rendered fields describe.
    ///
    /// The command widens and re-posts the *typed* value rather than the
    /// rendering, so the successor's bytes are the tip's bytes with one field
    /// changed — anything this conversion dropped would silently rewrite a
    /// field the existing approval was read against.
    pub fn to_revision(&self) -> ScopeRevision {
        ScopeRevision {
            schema: self.schema,
            workpiece: WorkpieceId(self.workpiece.clone()),
            predecessor: self.predecessor.map(|digest| Digest::from_bytes(*digest.as_bytes())),
            problem: self.problem.clone(),
            design: self.design.clone(),
            plan: self.plan.clone(),
            declared_surface: self.declared_surface.clone(),
            dogfood_brief: self.dogfood_brief.clone(),
            routing: ScopeRouting { size: self.routing.size.clone(), model: self.routing.model.clone() },
            dependencies: self.dependencies.iter().map(|id| WorkpieceId(id.clone())).collect(),
            description: self.description.clone(),
            implements: self.implements.iter().map(|digest| Digest::from_bytes(*digest.as_bytes())).collect(),
            declared_crates: self.declared_crates.clone(),
            declared_reads: self.declared_reads.clone(),
        }
    }
}

/// A stored statement, as much of it as the command reads: the words are the
/// scope digest an approval binds.
#[derive(Debug, Clone, Deserialize)]
pub struct StatementView {
    #[serde(default)]
    pub words: Vec<u8>,
}

/// `POST /commissions/{id}/revisions` body: the signed revision and sidecar
/// evidence about it. The revision's bytes are the signed subject; nothing in
/// [`evidence`](Self::evidence) is hashed into them.
#[derive(Debug, Serialize)]
pub struct WriteRevisionRequest<'a> {
    pub revision: &'a ScopeRevision,
    pub evidence: &'a RevisionEvidence,
}

/// What is known about a revision without being part of it.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RevisionEvidence {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope_verify: Option<ScopeVerifyInput>,
}

/// `POST /commissions/{id}/revisions` — the written revision's address.
#[derive(Debug, Clone, Deserialize)]
pub struct ScopeRevisionWrittenView {
    pub digest: DigestHex,
}

/// `POST /commissions/{id}/approvals` — the stored approval's address. The
/// address itself is the caller's own signed statement re-rendered, so nothing
/// here reads it back; the type is what makes a non-object reply a parse
/// failure rather than a silent success.
#[derive(Debug, Clone, Deserialize)]
pub struct ApprovalStoredView {}

/// `POST /commissions/{id}/cancel` body: the signature is the authority; the
/// reason is operator context recorded in the coordinator log.
#[derive(Debug, Serialize)]
pub struct CancelCommissionRequest {
    pub statement: Statement,
    pub reason: String,
}

/// `POST /commissions/{id}/cancel` reply.
#[derive(Debug, Clone, Deserialize)]
pub struct CommissionCancelledView {
    pub digest: DigestHex,
    pub status: String,
}

/// `POST /commissions/{id}/reopen` body: the cancel's shape at the Reopen door.
#[derive(Debug, Serialize)]
pub struct ReopenCommissionRequest {
    pub statement: Statement,
    pub reason: String,
}

/// `POST /commissions/{id}/reopen` reply.
#[derive(Debug, Clone, Deserialize)]
pub struct CommissionReopenedView {
    pub digest: DigestHex,
    pub status: String,
}

/// `GET /configs/{digest}` — a stored configuration, decoded through its kind's
/// schema.
#[derive(Debug, Clone, Deserialize)]
pub struct ConfigValueView {
    pub kind: String,
    pub value: Value,
}

/// `POST /drafts` / `GET /drafts/{id}` envelope.
#[derive(Debug, Clone, Deserialize)]
pub struct DraftView {
    pub draft_id: String,
}

/// `PATCH /drafts/{id}` — only present fields replace.
#[derive(Debug, Clone, Default, Serialize)]
pub struct DraftPatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proposals: Option<Vec<Membership>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub configs: Option<ConfigRegistry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<DigestHex>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forecast: Option<Forecast>,
}

/// A draft membership. `approval` is a placeholder the pre-seal gate overwrites.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Membership {
    pub workpiece: String,
    pub scope_revision: DigestHex,
    pub configs: ConfigRegistry,
    pub approval: Approval,
}

/// Placeholder approval evidence the gate replaces at seal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Approval {
    pub subject: DigestHex,
    pub kind: EvidenceKind,
    pub detail: DigestHex,
}

/// Kind-keyed configuration addresses.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigRegistry {
    pub entries: BTreeMap<String, DigestHex>,
}

impl ConfigRegistry {
    pub fn overlay(&mut self, other: Self) {
        self.entries.extend(other.entries);
    }
}

/// A sealed spec as the journal stores it.
#[derive(Debug, Clone, Deserialize)]
pub struct BloomSpec {
    pub members: Vec<Membership>,
    pub base: DigestHex,
    pub configs: ConfigRegistry,
    #[serde(default)]
    pub forecast: Forecast,
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
    pub digest: DigestHex,
    pub kind: String,
}

/// One declared member-dependency edge (`member` depends on `depends_on`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DependencyEdge {
    pub member: String,
    pub depends_on: String,
}

/// `POST /drafts/{id}/seal` body.
///
/// Scope, approval, description, and completeness are not fields here: the
/// door loads them from the commission store. A body that still carried
/// `projections` or `descriptions` would be accepted and ignored.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SealRequest {
    /// Override the admit idempotency key; defaults to the sealed bloom id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub edges: Vec<DependencyEdge>,
}

/// `POST /blooms/{id}/supersede` body.
#[derive(Debug, Clone, Serialize)]
pub struct SupersedeRequest {
    pub successor_draft: String,
    /// Override the admit idempotency key; defaults to the successor bloom id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub edges: Vec<DependencyEdge>,
}

/// `POST /blooms/{id}/members/{workpiece}/retry` body (#5423).
#[derive(Debug, Serialize)]
pub struct RetryRequest {
    pub stage: String,
    pub subject: DigestHex,
    pub reason: String,
    pub operator: String,
}

/// `POST /blooms/{id}/members/{workpiece}/withdraw` body (#5327).
#[derive(Debug, Serialize)]
pub struct WithdrawRequest {
    pub reason: String,
    pub operator: String,
    pub cascade: bool,
}

/// `POST /blooms/{id}/members/{workpiece}/repair` body (#4957, #5032).
///
/// Exactly one source is set. The `skip_serializing_if` keeps the other two
/// slots off the wire entirely rather than sending them as `null`: the route
/// counts the sources it was given, and a `null` that decodes to `None` is the
/// same as absent only for as long as nobody adds a third spelling.
#[derive(Debug, Serialize)]
pub struct RepairRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidate: Option<CandidateRefRequest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_worktree: Option<String>,
    pub reason: String,
    pub operator: String,
}

/// The `(tree, checkout)` pair a repair names when the operator has already
/// pushed the candidate ref themselves (ADR-0152).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CandidateRefRequest {
    pub tree: DigestHex,
    pub checkout: DigestHex,
}

/// `POST /blooms/{id}/members/{workpiece}/suppression` body (ADR-0193 §5).
#[derive(Debug, Serialize)]
pub struct SuppressionAnswerRequest {
    pub requests: Vec<DigestHex>,
    pub verdict: SuppressionVerdict,
    pub reason: String,
    pub operator: String,
}

/// A reviewer's answer to the suppression requests a candidate is carrying.
///
/// Spelled here rather than reused from `aether_bloomery` so the CLI can derive
/// `clap::ValueEnum` on it; the variant names are the wire's, so the serialized
/// value is the one the route decodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, clap::ValueEnum)]
pub enum SuppressionVerdict {
    /// The suppressions may stand; the candidate keeps them and continues.
    Granted,
    /// They may not. The member re-opens at `Refine` carrying the denial's
    /// reason, at its own repair budget's expense.
    Denied,
}

/// `GET /journal`.
#[derive(Debug, Deserialize)]
pub struct JournalView {
    pub records: Vec<JournalEntry>,
    #[serde(default)]
    pub total_matched: u64,
    #[serde(default)]
    pub shown: u64,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default)]
    pub next_from_sequence: Option<u64>,
}

impl JournalView {
    /// Inclusive sequence span of records that carried a sequence, if any.
    pub fn journal_span(&self) -> Option<(u64, u64)> {
        let mut sequences = self.records.iter().filter_map(|record| record.sequence);
        let first = sequences.next()?;
        Some(sequences.fold((first, first), |(lo, hi), sequence| (lo.min(sequence), hi.max(sequence))))
    }
}

#[derive(Debug, Deserialize)]
pub struct JournalEntry {
    #[serde(default)]
    pub sequence: Option<u64>,
    pub event: JournalEvent,
}

#[derive(Debug, Deserialize)]
pub struct JournalEvent {
    pub fact: Value,
}

/// A journal `Integrate` claim, as coverage reads it: workpiece plus evidence kind.
#[derive(Debug, Deserialize)]
pub struct IntegrateClaimView {
    pub workpiece: String,
    pub evidence: IntegrateEvidenceView,
}

/// Evidence bound to an integrate claim. Coverage keys on `kind`.
#[derive(Debug, Deserialize)]
pub struct IntegrateEvidenceView {
    pub kind: EvidenceKind,
}

/// Write-route reply. The outcome payload is left as JSON so this crate does
/// not have to hex-decode every `Outcome` variant just to print it.
#[derive(Debug, Deserialize)]
pub struct OutcomeView {
    pub outcome: Value,
}
