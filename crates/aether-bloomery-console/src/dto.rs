//! Version-tolerant mirrors of the coordinator `GET /view` JSON.
//!
//! Digests are 64 hex characters on the REST edge, but
//! `aether_bloomery::Digest`'s own deserializer does not accept hex.
//! These types serialize that spelling and accept either hex or the
//! canonical 32-byte array on the way in.
//!
//! Enum-shaped fields ([`BloomStatus`], [`WedgeCause`], [`StageId`],
//! [`SpendQuiesce`]) are local mirrors with a `#[serde(other)]` catch-all,
//! so an unknown variant degrades to a rendered `unknown` rather than
//! failing the whole poll. Every field is `#[serde(default)]` and unknown
//! fields are ignored, so the console survives running against a newer or
//! older coordinator.

use std::fmt;

use serde::de::Error;
use serde::{Deserialize, Deserializer};
use serde_json::Value;

/// A digest as the REST edge renders it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct DigestHex([u8; 32]);

impl DigestHex {
    #[must_use]
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    #[must_use]
    pub fn as_hex(&self) -> String {
        encode(&self.0)
    }

    /// The short id the board prints for a bloom.
    #[must_use]
    pub fn prefix(&self) -> String {
        let hex = encode(&self.0);
        hex.get(..8).unwrap_or(&hex).to_owned()
    }
}

impl fmt::Display for DigestHex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_hex())
    }
}

impl<'de> Deserialize<'de> for DigestHex {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        from_json(&value).map(Self).map_err(D::Error::custom)
    }
}

/// A bloom's position in the one-way lifecycle, as `/view` spells it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub enum BloomStatus {
    Sealed,
    Resolved,
    Landed,
    Superseded,
    #[default]
    #[serde(other)]
    Unknown,
}

impl BloomStatus {
    /// The word the board prints. An unrecognized variant is `unknown`, not a
    /// failed poll.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Sealed => "Sealed",
            Self::Resolved => "Resolved",
            Self::Landed => "Landed",
            Self::Superseded => "Superseded",
            Self::Unknown => "unknown",
        }
    }
}

impl fmt::Display for BloomStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Why a member stopped dispatching, as `/view` spells it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub enum WedgeCause {
    Work,
    Machinery,
    #[default]
    #[serde(other)]
    Unknown,
}

impl WedgeCause {
    /// The word the board prints. An unrecognized variant is `unknown`.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Work => "Work",
            Self::Machinery => "Machinery",
            Self::Unknown => "unknown",
        }
    }
}

impl fmt::Display for WedgeCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// A stage name as `/view` spells it. Unknown names degrade rather than
/// taking the poll down.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub enum StageId {
    Sketch,
    Scope,
    Approve,
    Construct,
    Verify,
    Refine,
    Review,
    Integrate,
    AggregateVerify,
    AggregateReview,
    Land,
    Study,
    Reconcile,
    #[default]
    #[serde(other)]
    Unknown,
}

/// Why the seal door closed (ADR-0192), as `/view` spells it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub enum SpendQuiesce {
    Window {
        #[serde(default)]
        window: String,
        #[serde(default)]
        spent_micro_usd: u64,
        #[serde(default)]
        ceiling_micro_usd: u64,
    },
    Bloom {
        #[serde(default)]
        window: String,
        #[serde(default)]
        bloom: DigestHex,
        #[serde(default)]
        spent_micro_usd: u64,
        #[serde(default)]
        ceiling_micro_usd: u64,
    },
    #[serde(other)]
    Unknown,
}

/// `GET /view` as the console consumes it.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ViewDocument {
    #[serde(default)]
    pub mainline: DigestHex,
    #[serde(default)]
    pub observed: DigestHex,
    #[serde(default)]
    pub spend_quiesce: Option<SpendQuiesce>,
    #[serde(default)]
    pub blooms: Vec<BloomView>,
}

/// One bloom in the live projection.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BloomView {
    #[serde(default)]
    pub id: DigestHex,
    #[serde(default)]
    pub status: Option<BloomStatus>,
    #[serde(default)]
    pub superseded_by: Option<DigestHex>,
    #[serde(default)]
    pub members: Vec<MemberView>,
    #[serde(default)]
    pub landing_blocked: Option<LandingBlock>,
    #[serde(default)]
    pub executor_fault: Option<ExecutorFaultView>,
    #[serde(default)]
    pub review_park: Option<ReviewParkView>,
    #[serde(default)]
    pub composition: Option<CompositionView>,
}

/// One sealed member as the projection renders it.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MemberView {
    #[serde(default)]
    pub workpiece: String,
    #[serde(default)]
    pub resolution: Option<Present>,
    #[serde(default)]
    pub pending_decision: Option<PendingDecisionView>,
    #[serde(default)]
    pub wedge: Option<Present>,
    #[serde(default)]
    pub blocked_by: Option<String>,
    #[serde(default)]
    pub host_fault: Option<Present>,
    #[serde(default)]
    pub machinery_rolls: u32,
    #[serde(default)]
    pub machinery_budget: u32,
    #[serde(default)]
    pub wedge_cause: Option<WedgeCause>,
}

/// A bloom's landing-gate standing, once a landing has been refused.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct LandingBlock {
    #[serde(default)]
    pub rolls: u32,
    #[serde(default)]
    pub budget: u32,
}

/// A bloom's aggregate-review executor-fault standing.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ExecutorFaultView {
    #[serde(default)]
    pub rolls: u32,
    #[serde(default)]
    pub budget: u32,
    #[serde(default)]
    pub terminal: bool,
}

/// The bloom-scoped aggregate-review park, including the question prose
/// when the artifact resolved.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ReviewParkView {
    #[serde(default)]
    pub question: DigestHex,
    #[serde(default)]
    pub stage: Option<StageId>,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub options: Vec<String>,
    #[serde(default)]
    pub blocked: Option<String>,
}

/// A member's pending-decision hold: the question digest plus the prose
/// an operator reads.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PendingDecisionView {
    #[serde(default)]
    pub question: DigestHex,
    #[serde(default)]
    pub stage: Option<StageId>,
    #[serde(default)]
    pub prompt: String,
    #[serde(default)]
    pub options: Vec<String>,
    #[serde(default)]
    pub blocked: String,
}

/// The composition workpiece's own line: cursor, wedge, and open findings.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CompositionView {
    #[serde(default)]
    pub cursor: Option<CompositionCursorView>,
    #[serde(default)]
    pub wedge: Option<CompositionWedge>,
    #[serde(default)]
    pub findings: Vec<CompositionFinding>,
}

/// The composition's stage cursor.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CompositionCursorView {
    #[serde(default)]
    pub stage: Option<StageId>,
    #[serde(default)]
    pub attempts: u32,
    #[serde(default)]
    pub candidate: Option<CandidateRef>,
}

/// Why the composition stopped, once it has wedged.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CompositionWedge {
    #[serde(default)]
    pub stage: Option<StageId>,
    #[serde(default)]
    pub evidence: DigestHex,
}

/// One composition-review finding the operator (or a later slice) quotes.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CompositionFinding {
    #[serde(default)]
    pub subject: DigestHex,
    #[serde(default)]
    pub detail: DigestHex,
    #[serde(default)]
    pub implicated: Vec<String>,
}

/// A captured candidate tree plus the commit a worker checks out.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CandidateRef {
    #[serde(default)]
    pub tree: DigestHex,
    #[serde(default)]
    pub checkout: DigestHex,
}

/// Presence marker: any JSON object (or other value) deserializes, extra
/// fields ignored. Used for `/view` objects the board only tests for
/// presence — a wedge, a host fault, a resolution claim.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Present {}

fn encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    out
}

fn decode(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 {
        return None;
    }
    let mut bytes = [0u8; 32];
    for (index, slot) in bytes.iter_mut().enumerate() {
        *slot = u8::from_str_radix(hex.get(index * 2..index * 2 + 2)?, 16).ok()?;
    }
    Some(bytes)
}

/// Accept the REST edge's two spellings: 64 hex characters, or a 32-byte array.
fn from_json(value: &Value) -> Result<[u8; 32], String> {
    match value {
        Value::String(hex) => decode(hex).ok_or_else(|| "digest is not a 32-byte hex string".to_owned()),
        Value::Array(items) if items.len() == 32 => {
            let mut bytes = [0u8; 32];
            for (slot, item) in bytes.iter_mut().zip(items) {
                let number = item.as_u64().ok_or_else(|| "digest is not a 32-byte array".to_owned())?;
                *slot = u8::try_from(number).map_err(|_| "digest is not a 32-byte array".to_owned())?;
            }
            Ok(bytes)
        }
        _ => Err("digest is not a 32-byte hex string".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BloomStatus, BloomView, DigestHex, MemberView, SpendQuiesce, StageId, ViewDocument, WedgeCause, decode, encode,
    };
    use serde_json::json;

    fn hex(byte: u8) -> String {
        encode(&[byte; 32])
    }

    fn digest(byte: u8) -> DigestHex {
        DigestHex::from_bytes([byte; 32])
    }

    #[test]
    fn hex_accepts_the_rest_edge_spelling() {
        // The plausible bug: the console imports aether_bloomery::Digest,
        // whose deserializer rejects the 64-hex /view rendering, so every
        // poll fails to decode a live coordinator.
        let bytes = [0x5c; 32];
        let hex = encode(&bytes);
        let parsed: DigestHex = serde_json::from_value(json!(hex)).expect("hex digest");
        assert_eq!(parsed.as_bytes(), &bytes);
        let from_array: DigestHex = serde_json::from_value(json!(bytes.to_vec())).expect("byte-array digest");
        assert_eq!(from_array.as_bytes(), &bytes);
        assert_eq!(decode(&hex.to_ascii_uppercase()), Some(bytes));
        assert_eq!(parsed.prefix().len(), 8);
    }

    #[test]
    fn decode_rejects_a_malformed_digest() {
        // The plausible bug: a truncated or non-hex id is silently zeroed,
        // so two bad blooms collapse onto the same selection identity.
        assert!(serde_json::from_value::<DigestHex>(json!("abcd")).is_err());
        assert!(serde_json::from_value::<DigestHex>(json!("g".repeat(64))).is_err());
        assert!(serde_json::from_value::<DigestHex>(json!([1, 2, 3])).is_err());
    }

    #[test]
    fn an_unknown_status_does_not_kill_the_poll() {
        // The plausible bug: BloomStatus is imported from aether-bloomery, so
        // a coordinator that adds one variant fails the whole BloomView
        // deserialize and takes GET /view down — the failure the module's
        // version-tolerance comment claimed to prevent.
        let view: ViewDocument = serde_json::from_value(json!({
            "blooms": [{
                "id": hex(0xab),
                "status": "AwaitingQuorum",
                "members": [{"workpiece": "issue-1", "wedge_cause": "Host"}]
            }]
        }))
        .expect("an unknown status must not fail the poll");
        let bloom = &view.blooms[0];
        assert_eq!(bloom.status, Some(BloomStatus::Unknown));
        assert_eq!(bloom.status.expect("status decoded").to_string(), "unknown");
        assert_eq!(bloom.members[0].wedge_cause, Some(WedgeCause::Unknown));
        assert_eq!(bloom.members[0].wedge_cause.expect("cause decoded").to_string(), "unknown");
    }

    #[test]
    fn known_enum_spellings_match_the_coordinator() {
        // Tripwire: the local mirrors deserialize the coordinator's JSON
        // spelling. A rename on that side would paint every live bloom as
        // unknown while the poll still claims to succeed.
        let status: BloomStatus = serde_json::from_value(
            serde_json::to_value(aether_bloomery::BloomStatus::Sealed).expect("coordinator status serializes"),
        )
        .expect("local status decodes the coordinator spelling");
        assert_eq!(status, BloomStatus::Sealed);
        let cause: WedgeCause = serde_json::from_value(
            serde_json::to_value(aether_bloomery::WedgeCause::Machinery).expect("coordinator cause serializes"),
        )
        .expect("local cause decodes the coordinator spelling");
        assert_eq!(cause, WedgeCause::Machinery);
    }

    #[test]
    fn view_defaults_missing_fields_and_ignores_unknown_ones() {
        // The plausible bug: a coordinator that added a field, or an older
        // one that omitted review_park / host_fault / the widened axes, fails
        // the whole poll.
        let view: ViewDocument = serde_json::from_value(json!({
            "future_axis": {"extra": true},
            "blooms": [{
                "id": hex(0xbb),
                "status": "Sealed",
                "not_yet_a_field": 1,
                "members": [{
                    "workpiece": "issue-1",
                    "wedge": {"stage": "Construct", "evidence": hex(0xcc)},
                    "wedge_cause": "Machinery",
                    "blocked_by": "issue-0",
                    "machinery_rolls": 2,
                    "machinery_budget": 3,
                    "host_fault": {"findings": "no cargo"}
                }]
            }]
        }))
        .expect("tolerant view");
        assert_eq!(view.mainline, DigestHex::default());
        assert_eq!(view.observed, DigestHex::default());
        assert!(view.spend_quiesce.is_none());
        assert_eq!(view.blooms.len(), 1);
        let bloom = &view.blooms[0];
        assert_eq!(bloom.status, Some(BloomStatus::Sealed));
        assert!(bloom.superseded_by.is_none());
        assert!(bloom.review_park.is_none());
        assert!(bloom.composition.is_none());
        assert!(bloom.landing_blocked.is_none());
        let member = &bloom.members[0];
        assert_eq!(member.workpiece, "issue-1");
        assert!(member.wedge.is_some());
        assert_eq!(member.wedge_cause, Some(WedgeCause::Machinery));
        assert!(member.pending_decision.is_none());
        assert_eq!(member.blocked_by.as_deref(), Some("issue-0"));
        assert_eq!(member.machinery_rolls, 2);
        assert_eq!(member.machinery_budget, 3);
        assert!(member.host_fault.is_some());
    }

    #[test]
    fn widened_fields_decode_from_a_realistic_document() {
        // The plausible bug: the mirror still drops mainline / observed /
        // spend_quiesce / superseded_by / review-park prose / pending_decision
        // / composition, so the war-room chrome has nothing to paint.
        let view: ViewDocument = serde_json::from_value(json!({
            "mainline": hex(0x11),
            "observed": hex(0x22),
            "spend_quiesce": {
                "Window": {
                    "window": "bloomery/daily/2026-08-17",
                    "spent_micro_usd": 12,
                    "ceiling_micro_usd": 10
                }
            },
            "blooms": [{
                "id": hex(0x33),
                "status": "Superseded",
                "superseded_by": hex(0x44),
                "review_park": {
                    "question": hex(0x55),
                    "stage": "AggregateReview",
                    "prompt": "land or wait?",
                    "options": ["land", "wait"],
                    "blocked": "aggregate review"
                },
                "composition": {
                    "cursor": {
                        "stage": "Review",
                        "attempts": 2,
                        "candidate": {"tree": hex(0x66), "checkout": hex(0x77)}
                    },
                    "wedge": {"stage": "Review", "evidence": hex(0x88)},
                    "findings": [{
                        "subject": hex(0x66),
                        "detail": hex(0x99),
                        "implicated": ["issue-1"]
                    }]
                },
                "members": [{
                    "workpiece": "issue-1",
                    "pending_decision": {
                        "question": hex(0xaa),
                        "stage": "Construct",
                        "prompt": "which approach?",
                        "options": ["A", "B"],
                        "blocked": "construct is held"
                    }
                }]
            }]
        }))
        .expect("realistic view");
        assert_eq!(view.mainline, digest(0x11));
        assert_eq!(view.observed, digest(0x22));
        assert_eq!(
            view.spend_quiesce,
            Some(SpendQuiesce::Window {
                window: "bloomery/daily/2026-08-17".to_owned(),
                spent_micro_usd: 12,
                ceiling_micro_usd: 10,
            })
        );
        let bloom = &view.blooms[0];
        assert_eq!(bloom.status, Some(BloomStatus::Superseded));
        assert_eq!(bloom.superseded_by, Some(digest(0x44)));
        let park = bloom.review_park.as_ref().expect("park prose");
        assert_eq!(park.question, digest(0x55));
        assert_eq!(park.stage, Some(StageId::AggregateReview));
        assert_eq!(park.prompt.as_deref(), Some("land or wait?"));
        assert_eq!(park.options, ["land", "wait"]);
        assert_eq!(park.blocked.as_deref(), Some("aggregate review"));
        let composition = bloom.composition.as_ref().expect("composition block");
        let cursor = composition.cursor.as_ref().expect("cursor");
        assert_eq!(cursor.stage, Some(StageId::Review));
        assert_eq!(cursor.attempts, 2);
        let candidate = cursor.candidate.as_ref().expect("candidate");
        assert_eq!(candidate.tree, digest(0x66));
        assert_eq!(candidate.checkout, digest(0x77));
        let wedge = composition.wedge.as_ref().expect("composition wedge");
        assert_eq!(wedge.stage, Some(StageId::Review));
        assert_eq!(wedge.evidence, digest(0x88));
        assert_eq!(composition.findings.len(), 1);
        assert_eq!(composition.findings[0].detail, digest(0x99));
        assert_eq!(composition.findings[0].implicated, ["issue-1"]);
        let pending = bloom.members[0].pending_decision.as_ref().expect("pending decision");
        assert_eq!(pending.question, digest(0xaa));
        assert_eq!(pending.stage, Some(StageId::Construct));
        assert_eq!(pending.prompt, "which approach?");
        assert_eq!(pending.options, ["A", "B"]);
        assert_eq!(pending.blocked, "construct is held");
    }

    #[test]
    fn empty_object_is_still_a_present_park() {
        // The plausible bug: a reduced park (digest-only, or even `{}`)
        // fails to deserialize, so the alert band stays quiet on a parked bloom.
        let bloom: BloomView = serde_json::from_value(json!({
            "id": hex(0xdd),
            "review_park": {"question": hex(0xee)}
        }))
        .expect("parked bloom");
        assert!(bloom.review_park.is_some());
        assert_eq!(bloom.review_park.as_ref().expect("park").question, digest(0xee));

        let member: MemberView = serde_json::from_value(json!({})).expect("empty member");
        assert!(member.workpiece.is_empty());
        assert!(member.wedge.is_none());
    }
}
