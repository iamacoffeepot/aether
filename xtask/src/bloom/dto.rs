//! JSON shapes the coordinator REST edge already speaks.
//!
//! Digests are 64 hex characters on the wire. These types serialize that
//! spelling and accept either hex or the canonical 32-byte array on the way
//! in, so the command never composes a raw body and never re-encodes a
//! digest the edge already rendered.

use std::collections::BTreeMap;
use std::fmt;

use aether_bloomery::{
    BloomStatus, Digest, EvidenceKind, Forecast, ScopeRevision, ScopeRouting, Statement, WorkpieceId,
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
}

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

/// The nine completeness facts the pre-seal gate fails closed on.
///
/// Flattened groups keep the wire object one level (the gate's field names)
/// without packing eight independent bools onto a single Rust struct.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Completeness {
    #[serde(flatten)]
    statements: CompletenessStatements,
    pub referenced_adr_prs_merged: bool,
    pub model_routing_count: usize,
    pub blocked: bool,
    #[serde(flatten)]
    freshness: CompletenessFreshness,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct CompletenessStatements {
    has_problem_statement: bool,
    has_design_notes: bool,
    has_implementation_plan: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct CompletenessFreshness {
    declared_surface_fresh: bool,
    dependencies_all_closed: bool,
    umbrella_integrity: bool,
}

impl Completeness {
    /// The checklist a first-class direct-drive seal satisfies.
    pub fn direct_drive() -> Self {
        Self {
            statements: CompletenessStatements {
                has_problem_statement: true,
                has_design_notes: true,
                has_implementation_plan: true,
            },
            referenced_adr_prs_merged: true,
            model_routing_count: 1,
            blocked: false,
            freshness: CompletenessFreshness {
                declared_surface_fresh: true,
                dependencies_all_closed: true,
                umbrella_integrity: true,
            },
        }
    }
}

/// ADR-maturity the hard gate routes on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
pub enum AdrTouch {
    #[default]
    None,
    NewOrEstablished,
    ProposedOnly,
}

/// One member's seal-time projection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemberProjection {
    pub workpiece: String,
    pub scope_revision: DigestHex,
    pub declared_surface: Vec<String>,
    pub completeness: Completeness,
    pub adr_touch: AdrTouch,
    pub pre_approved: bool,
}

/// One declared member-dependency edge (`member` depends on `depends_on`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DependencyEdge {
    pub member: String,
    pub depends_on: String,
}

/// `POST /drafts/{id}/seal` body.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SealRequest {
    pub projections: Vec<MemberProjection>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub descriptions: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub edges: Vec<DependencyEdge>,
}

/// `POST /blooms/{id}/supersede` body.
#[derive(Debug, Clone, Serialize)]
pub struct SupersedeRequest {
    pub successor_draft: String,
    pub projections: Vec<MemberProjection>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub descriptions: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub edges: Vec<DependencyEdge>,
}

/// `POST /blooms/{id}/members/{workpiece}/withdraw` body (#5327).
#[derive(Debug, Serialize)]
pub struct WithdrawRequest {
    pub reason: String,
    pub operator: String,
    pub cascade: bool,
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
