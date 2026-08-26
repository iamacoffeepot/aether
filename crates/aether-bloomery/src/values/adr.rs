//! Architecture decision records as versioned canonical values (ADR-0201).
//!
//! An ADR's identity is the digest of these typed bytes. Status is not a
//! field here: an unsigned column would be authoritative for acceptance, so
//! status lives in append-only [`AdrTransition`] rows. Markdown is rendered
//! from the value and never stored as the source of truth.

use alloc::string::String;
use alloc::vec::Vec;

use aether_data::wire::{from_bytes, to_vec};
use serde::{Deserialize, Serialize};

use crate::digest::{ContentAddressed, Digest};

/// The schema number a version-1 [`Adr`] writes into its first field.
pub const ADR_SCHEMA: u32 = 1;

/// The schema number a version-1 [`AdrTransition`] writes into its first field.
pub const ADR_TRANSITION_SCHEMA: u32 = 1;

/// Why canonical ADR bytes could not become a typed value.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AdrValueError {
    /// The bytes are not a well-formed aether-wire encoding of this type.
    Malformed,
    /// The bytes decoded, but the schema field is not one this binary writes.
    UnsupportedSchema(u32),
}

/// Lifecycle recorded by an append-only transition. The last transition is
/// the only status a reader may believe.
///
/// Variant order is the wire discriminant. This is the first encoding; do
/// not reorder once a production row exists.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum AdrStatus {
    /// The ADR is registered and not yet in force.
    Proposed,
    /// The machine has stated that work may proceed pending owner ratification.
    /// A provisional record must not carry a signature.
    Provisional,
    /// The owner accepted the ADR. The matching transition carries a signature.
    Accepted,
    /// A later ADR replaced this one. The transition names the successor.
    Superseded,
}

impl AdrStatus {
    /// The spelling stored in the `adr_transitions.status` column.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Proposed => "proposed",
            Self::Provisional => "provisional",
            Self::Accepted => "accepted",
            Self::Superseded => "superseded",
        }
    }

    /// Parse a stored status spelling.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "proposed" => Some(Self::Proposed),
            "provisional" => Some(Self::Provisional),
            "accepted" => Some(Self::Accepted),
            "superseded" => Some(Self::Superseded),
            _ => None,
        }
    }

    /// The status line a rendered mirror writes.
    #[must_use]
    pub fn render_line(self, successor_number: Option<u32>) -> String {
        match (self, successor_number) {
            (Self::Superseded, Some(number)) => alloc::format!("Superseded by ADR-{number:04}"),
            (Self::Superseded, None) => String::from("Superseded"),
            (Self::Proposed, _) => String::from("Proposed"),
            (Self::Provisional, _) => String::from("Provisional"),
            (Self::Accepted, _) => String::from("Accepted"),
        }
    }
}

/// An immutable, versioned architecture decision, addressed by the digest of
/// these bytes.
///
/// Status is not a field. Changing Proposed to Accepted must not move the
/// digest an Accept signature covers.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Adr {
    /// Schema version. Version 1 writes [`ADR_SCHEMA`].
    pub schema: u32,
    /// Sequential ADR number. The mirror filename pads it to four digits.
    pub number: u32,
    /// The decision's title, without the `ADR-NNNN:` prefix.
    pub title: String,
    /// The `YYYY-MM-DD` the record was written.
    pub date: String,
    /// What problem and forces the decision answers.
    pub context: String,
    /// What was decided, stated plainly.
    pub decision: String,
    /// What changes as a result.
    pub consequences: String,
    /// Rejected options. Empty omits the alternatives heading on render.
    pub alternatives: String,
}

impl Adr {
    /// Decode canonical bytes as a version-1 ADR.
    ///
    /// # Errors
    /// [`AdrValueError::Malformed`] when the bytes are not this type;
    /// [`AdrValueError::UnsupportedSchema`] when they decode as a later (or
    /// zero) schema this encoder does not write.
    pub fn from_canonical(bytes: &[u8]) -> Result<Self, AdrValueError> {
        let value: Self = from_bytes(bytes).map_err(|_| AdrValueError::Malformed)?;
        if value.schema != ADR_SCHEMA {
            return Err(AdrValueError::UnsupportedSchema(value.schema));
        }
        Ok(value)
    }

    /// Canonical aether-wire bytes of this ADR.
    ///
    /// # Panics
    /// Panics if the value exceeds the ADR-0118 `u32` wire-length ceiling,
    /// which no ADR value does.
    #[must_use]
    pub fn to_canonical(&self) -> Vec<u8> {
        to_vec(self).expect("ADR values never exceed the ADR-0118 u32 wire-length ceiling")
    }

    /// Repository-relative mirror path (`docs/adr/NNNN-slug.md`).
    #[must_use]
    pub fn mirror_path(&self) -> String {
        alloc::format!("docs/adr/{:04}-{}.md", self.number, slug(&self.title))
    }

    /// Byte-stable markdown for the `docs/adr/` mirror.
    ///
    /// The same `(self, status, successor_number)` always emits the same
    /// bytes. `successor_number` is used only when `status` is
    /// [`AdrStatus::Superseded`].
    #[must_use]
    pub fn render(&self, status: AdrStatus, successor_number: Option<u32>) -> String {
        let mut out = alloc::format!(
            "# ADR-{:04}: {}\n\n- **Status:** {}\n- **Date:** {}\n\n",
            self.number,
            self.title,
            status.render_line(successor_number),
            self.date,
        );
        push_section(&mut out, "Context", &self.context);
        push_section(&mut out, "Decision", &self.decision);
        push_section(&mut out, "Consequences", &self.consequences);
        if !self.alternatives.is_empty() {
            push_section(&mut out, "Alternatives considered", &self.alternatives);
        }
        out
    }
}

impl ContentAddressed for Adr {
    const DOMAIN: &'static str = "aether.bloomery.adr";
}

/// One append-only status record over an [`Adr`] digest.
///
/// An Accepted record may cite resolution or evidence digests. Empty
/// citations are legal — docs-only ADRs have nothing to name — and the
/// field exists from row one.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct AdrTransition {
    /// Schema version. Version 1 writes [`ADR_TRANSITION_SCHEMA`].
    pub schema: u32,
    /// The ADR digest this transition speaks about.
    pub adr: Digest,
    /// The status this record asserts.
    pub status: AdrStatus,
    /// Evidence digests an Accepted record cites. Empty on every other status
    /// and on docs-only acceptances.
    pub citations: Vec<Digest>,
    /// The successor ADR digest when status is [`AdrStatus::Superseded`].
    pub successor: Option<Digest>,
}

impl AdrTransition {
    /// Decode canonical bytes as a version-1 transition.
    ///
    /// # Errors
    /// [`AdrValueError::Malformed`] when the bytes are not this type;
    /// [`AdrValueError::UnsupportedSchema`] when they decode as a later (or
    /// zero) schema this encoder does not write.
    pub fn from_canonical(bytes: &[u8]) -> Result<Self, AdrValueError> {
        let value: Self = from_bytes(bytes).map_err(|_| AdrValueError::Malformed)?;
        if value.schema != ADR_TRANSITION_SCHEMA {
            return Err(AdrValueError::UnsupportedSchema(value.schema));
        }
        Ok(value)
    }

    /// Canonical aether-wire bytes of this transition.
    ///
    /// # Panics
    /// Panics if the value exceeds the ADR-0118 `u32` wire-length ceiling,
    /// which no ADR value does.
    #[must_use]
    pub fn to_canonical(&self) -> Vec<u8> {
        to_vec(self).expect("ADR values never exceed the ADR-0118 u32 wire-length ceiling")
    }
}

impl ContentAddressed for AdrTransition {
    const DOMAIN: &'static str = "aether.bloomery.adr_transition";
}

fn slug(title: &str) -> String {
    let mut out = String::new();
    let mut pending_dash = false;
    for ch in title.chars() {
        if ch.is_ascii_alphanumeric() {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            out.push(ch.to_ascii_lowercase());
            pending_dash = false;
        } else {
            pending_dash = true;
        }
    }
    out
}

fn push_section(out: &mut String, heading: &str, body: &str) {
    out.push_str("## ");
    out.push_str(heading);
    out.push('\n');
    out.push('\n');
    out.push_str(body);
    if !body.ends_with('\n') {
        out.push('\n');
    }
    out.push('\n');
}

#[cfg(test)]
mod tests {
    use alloc::collections::BTreeMap;
    use alloc::string::String;
    use alloc::vec::Vec;

    use ed25519_dalek::{Signer, SigningKey};

    use super::{ADR_SCHEMA, Adr, AdrStatus, AdrTransition, AdrValueError};
    use crate::digest::digest_of;
    use crate::ids::KeyId;
    use crate::sign::{AuthorityDoor, AuthorizedSigner, Ed25519KeyProvider, SignatureEnvelope, authorization_message};
    use crate::values::Tier;
    use crate::values::{Provenance, Statement};

    fn fixture() -> Adr {
        Adr {
            schema: ADR_SCHEMA,
            number: 201,
            title: String::from("t"),
            date: String::from("2026-08-18"),
            context: String::from("c"),
            decision: String::from("d"),
            consequences: String::from("q"),
            alternatives: String::from("a"),
        }
    }

    // Tripwire: version-1 wire bytes. A signature covers these exact bytes; the
    // encoder may be superseded, never silently changed. Recompute-and-repin
    // only when introducing a new schema.
    const GOLDEN_ADR_V1: &[u8] = &[
        1, 0, 0, 0, 201, 0, 0, 0, 1, 0, 0, 0, 116, 10, 0, 0, 0, 50, 48, 50, 54, 45, 48, 56, 45, 49, 56, 1, 0, 0, 0, 99,
        1, 0, 0, 0, 100, 1, 0, 0, 0, 113, 1, 0, 0, 0, 97,
    ];

    // Tripwire: content address of the version-1 fixture under
    // `aether.bloomery.adr`. Drifts if the domain tag, the hash, or the
    // fixture bytes move.
    const GOLDEN_ADR_DIGEST: [u8; 32] = [
        125, 37, 177, 13, 235, 41, 46, 42, 118, 53, 159, 189, 102, 30, 55, 11, 220, 86, 184, 19, 37, 233, 100, 223, 22,
        129, 126, 83, 224, 149, 21, 65,
    ];

    // Tripwire: ADR-0182 authorization message for Accept over the fixture
    // digest, with words equal to that digest's raw bytes. Drifts if the door
    // discriminant, domain tag, or binding layout changes.
    const GOLDEN_ACCEPT_AUTHORIZATION: [u8; 32] = [
        74, 205, 228, 204, 133, 21, 45, 181, 92, 61, 197, 187, 202, 135, 86, 159, 72, 167, 23, 173, 179, 8, 180, 156,
        249, 58, 10, 41, 205, 220, 8, 18,
    ];

    // Tripwire: ed25519 signature by seed-7 over the golden authorization
    // message. Drifts if the signed subject or the seed-7 key meaning changes.
    const GOLDEN_ACCEPT_SIGNATURE: &[u8] = &[
        80, 255, 38, 59, 163, 143, 5, 112, 13, 152, 132, 199, 89, 154, 108, 17, 64, 175, 127, 52, 19, 87, 123, 63, 149,
        65, 160, 82, 32, 39, 97, 125, 142, 62, 164, 134, 45, 72, 90, 149, 140, 199, 91, 182, 107, 141, 114, 222, 15,
        226, 54, 131, 113, 43, 68, 218, 17, 136, 46, 72, 96, 202, 207, 12,
    ];

    // Tripwire: rendered mirror of the fixture at Proposed. Drifts if the
    // heading, status line, or section layout changes.
    const GOLDEN_RENDER: &str = "\
# ADR-0201: t

- **Status:** Proposed
- **Date:** 2026-08-18

## Context

c

## Decision

d

## Consequences

q

## Alternatives considered

a

";

    #[test]
    fn version_one_encoding_matches_the_pinned_golden() {
        let adr = fixture();
        assert_eq!(adr.schema, ADR_SCHEMA, "v1 writes schema 1 into the bytes");
        let encoded = adr.to_canonical();
        assert_eq!(encoded.as_slice(), GOLDEN_ADR_V1, "Adr v1 wire drifted; encoded={encoded:?}");
        let decoded =
            Adr::from_canonical(GOLDEN_ADR_V1).expect("pinned version-1 bytes must decode against HEAD types");
        assert_eq!(decoded, adr);
    }

    #[test]
    fn version_one_digest_matches_the_pinned_golden() {
        assert_eq!(
            *digest_of(&fixture()).as_bytes(),
            GOLDEN_ADR_DIGEST,
            "Adr content addressing drifted from the pinned golden digest; digest={:?}",
            digest_of(&fixture())
        );
    }

    #[test]
    fn accept_authorization_over_the_fixture_matches_the_pinned_golden() {
        let digest = digest_of(&fixture());
        let message = authorization_message(AuthorityDoor::Accept, digest, digest.as_bytes());
        assert_eq!(
            *message.as_bytes(),
            GOLDEN_ACCEPT_AUTHORIZATION,
            "Accept authorization message drifted; digest={digest:?} message={message:?}"
        );
    }

    #[test]
    fn a_seed_seven_signature_over_the_fixture_matches_the_pinned_golden() {
        let digest = digest_of(&fixture());
        let message = authorization_message(AuthorityDoor::Accept, digest, digest.as_bytes());
        let key = SigningKey::from_bytes(&[7; 32]);
        let signature = key.sign(message.as_bytes()).to_bytes();
        assert_eq!(signature.as_slice(), GOLDEN_ACCEPT_SIGNATURE, "Accept signature drifted; signature={signature:?}");

        let keys = Ed25519KeyProvider::new(BTreeMap::from([(
            KeyId(String::from("owner")),
            AuthorizedSigner { key: key.verifying_key(), ceiling: Tier::Human },
        )]));
        let statement = Statement {
            words: digest.as_bytes().to_vec(),
            provenance: Provenance::AuthorSignature(SignatureEnvelope {
                signer: KeyId(String::from("owner")),
                signature: signature.to_vec(),
            }),
            parents: Vec::new(),
        };
        assert!(
            statement.verify_authority(&keys, AuthorityDoor::Accept, digest),
            "the golden signature must verify over the fixture digest"
        );
        assert!(
            !statement.verify_authority(&keys, AuthorityDoor::Approve, digest),
            "an Accept envelope must not verify at Approve"
        );
    }

    #[test]
    fn unsupported_schema_is_refused_rather_than_decoded_as_v1() {
        let mut bytes = fixture().to_canonical();
        bytes[0] = 2;
        assert_eq!(
            Adr::from_canonical(&bytes),
            Err(AdrValueError::UnsupportedSchema(2)),
            "a later schema must not be silently read as version 1"
        );
    }

    #[test]
    fn garbage_bytes_are_malformed_not_a_panic() {
        assert_eq!(
            Adr::from_canonical(&[0xff, 0x00]),
            Err(AdrValueError::Malformed),
            "garbage must refuse rather than panic in the wire decoder"
        );
    }

    #[test]
    fn render_is_byte_stable_for_unchanged_input() {
        // A mirror that inserts a clock or reflows prose cannot be checked
        // against the store. The same value must emit the same bytes twice.
        let adr = fixture();
        let first = adr.render(AdrStatus::Proposed, None);
        let second = adr.render(AdrStatus::Proposed, None);
        assert_eq!(first, second);
        assert_eq!(first, GOLDEN_RENDER);
        assert_eq!(adr.mirror_path(), "docs/adr/0201-t.md");
    }

    #[test]
    fn empty_alternatives_omit_the_optional_heading() {
        // The template marks alternatives optional. Emitting an empty heading
        // would make a docs-only ADR look like it considered none on purpose.
        let mut adr = fixture();
        adr.alternatives.clear();
        let rendered = adr.render(AdrStatus::Accepted, None);
        assert!(!rendered.contains("Alternatives considered"), "{rendered}");
        assert!(rendered.contains("**Status:** Accepted"), "{rendered}");
    }

    #[test]
    fn superseded_render_names_the_successor_number() {
        let rendered = fixture().render(AdrStatus::Superseded, Some(202));
        assert!(rendered.contains("**Status:** Superseded by ADR-0202"), "{rendered}");
    }

    #[test]
    fn a_transition_round_trips_empty_citations() {
        // Empty citations must survive encode: a docs-only acceptance is
        // legal, and dropping the field would make "no evidence" unrepresentable.
        let transition = AdrTransition {
            schema: super::ADR_TRANSITION_SCHEMA,
            adr: digest_of(&fixture()),
            status: AdrStatus::Accepted,
            citations: Vec::new(),
            successor: None,
        };
        let decoded = AdrTransition::from_canonical(&transition.to_canonical()).expect("decode");
        assert_eq!(decoded, transition);
        assert!(decoded.citations.is_empty());
    }
}
