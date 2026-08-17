//! Version-tolerant mirrors of the coordinator `GET /view` JSON.
//!
//! Digests are 64 hex characters on the REST edge, but
//! [`aether_bloomery::Digest`]'s own deserializer does not accept hex.
//! These types serialize that spelling and accept either hex or the
//! canonical 32-byte array on the way in. Enum-shaped fields with no
//! digest inside ([`BloomStatus`], [`WedgeCause`]) come from
//! `aether-bloomery` directly.
//!
//! Every field is `#[serde(default)]` and unknown fields are ignored, so
//! the console survives running against a newer or older coordinator.

use std::fmt;

use aether_bloomery::{BloomStatus, WedgeCause};
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

/// `GET /view` as the console consumes it.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ViewDocument {
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
    pub members: Vec<MemberView>,
    #[serde(default)]
    pub landing_blocked: Option<LandingBlock>,
    #[serde(default)]
    pub executor_fault: Option<ExecutorFaultView>,
    #[serde(default)]
    pub review_park: Option<Present>,
}

/// One sealed member as the projection renders it.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MemberView {
    #[serde(default)]
    pub workpiece: String,
    #[serde(default)]
    pub resolution: Option<Present>,
    #[serde(default)]
    pub pending_decision: Option<Present>,
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

/// Presence marker: any JSON object (or other value) deserializes, extra
/// fields ignored. Used for `/view` objects the board only tests for
/// presence — a wedge, a park, a host fault.
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
    use super::{BloomView, DigestHex, MemberView, ViewDocument, decode, encode};
    use aether_bloomery::{BloomStatus, WedgeCause};
    use serde_json::json;

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
    fn view_defaults_missing_fields_and_ignores_unknown_ones() {
        // The plausible bug: a coordinator that added a field, or an older
        // one that omitted review_park / host_fault, fails the whole poll.
        let view: ViewDocument = serde_json::from_value(json!({
            "mainline": "aa".repeat(32),
            "future_axis": {"extra": true},
            "blooms": [{
                "id": "bb".repeat(32),
                "status": "Sealed",
                "not_yet_a_field": 1,
                "members": [{
                    "workpiece": "issue-1",
                    "wedge": {"stage": "Construct", "evidence": "cc".repeat(32)},
                    "wedge_cause": "Machinery",
                    "blocked_by": "issue-0",
                    "machinery_rolls": 2,
                    "machinery_budget": 3,
                    "host_fault": {"findings": "no cargo"}
                }]
            }]
        }))
        .expect("tolerant view");
        assert_eq!(view.blooms.len(), 1);
        let bloom = &view.blooms[0];
        assert_eq!(bloom.status, Some(BloomStatus::Sealed));
        assert!(bloom.review_park.is_none());
        assert!(bloom.landing_blocked.is_none());
        let member = &bloom.members[0];
        assert_eq!(member.workpiece, "issue-1");
        assert!(member.wedge.is_some());
        assert_eq!(member.wedge_cause, Some(WedgeCause::Machinery));
        assert_eq!(member.blocked_by.as_deref(), Some("issue-0"));
        assert_eq!(member.machinery_rolls, 2);
        assert_eq!(member.machinery_budget, 3);
        assert!(member.host_fault.is_some());
    }

    #[test]
    fn empty_object_is_still_a_present_park() {
        // The plausible bug: a reduced park (digest-only, or even `{}`)
        // fails to deserialize, so the alert band stays quiet on a parked bloom.
        let bloom: BloomView = serde_json::from_value(json!({
            "id": "dd".repeat(32),
            "review_park": {"question": "ee".repeat(32)}
        }))
        .expect("parked bloom");
        assert!(bloom.review_park.is_some());

        let member: MemberView = serde_json::from_value(json!({})).expect("empty member");
        assert!(member.workpiece.is_empty());
        assert!(member.wedge.is_none());
    }
}
