//! JSON shapes the coordinator REST edge already speaks.
//!
//! Digests are 64 hex characters on the wire. These types serialize that
//! spelling and accept either hex or the canonical 32-byte array on the way
//! in, so the command never composes a raw body and never re-encodes a
//! digest the edge already rendered.

use std::collections::BTreeMap;
use std::fmt;

use aether_bloomery::{BloomStatus, EvidenceKind, Forecast};
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
}

/// `GET /journal`.
#[derive(Debug, Deserialize)]
pub struct JournalView {
    pub records: Vec<JournalEntry>,
}

#[derive(Debug, Deserialize)]
pub struct JournalEntry {
    pub event: JournalEvent,
}

#[derive(Debug, Deserialize)]
pub struct JournalEvent {
    pub fact: Value,
}

/// Write-route reply. The outcome payload is left as JSON so this crate does
/// not have to hex-decode every `Outcome` variant just to print it.
#[derive(Debug, Deserialize)]
pub struct OutcomeView {
    pub outcome: Value,
}
