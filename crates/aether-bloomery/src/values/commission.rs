//! Signed commissions: versioned canonical values for intended work (ADR-0199).
//!
//! A **commission** is a signed statement of intended work. The name is distinct
//! from [`crate::WorkOrder`], which is the portable executor dispatch unit
//! `{ transformation, nonce }` and is deliberately blind to which bloom
//! dispatched it. Reusing that term here would make "work order" mean both
//! *what was dispatched* and *what was intended*.
//!
//! Digest identity is the hash of these typed values' canonical aether-wire
//! bytes, never of a SQL projection. Schema version sits in the bytes from
//! row one: a signature over version-1 bytes can never be reinterpreted by a
//! changed encoder — only superseded by a later schema.

use alloc::string::String;
use alloc::vec::Vec;

use aether_data::wire::{from_bytes, to_vec};
use serde::{Deserialize, Serialize};

use crate::digest::{ContentAddressed, Digest};
use crate::ids::WorkpieceId;

/// The schema number a version-1 [`ScopeRevision`] writes into its first field.
pub const SCOPE_REVISION_SCHEMA: u32 = 1;

/// Why canonical commission bytes could not become a typed value.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CommissionValueError {
    /// The bytes are not a well-formed aether-wire encoding of this type.
    Malformed,
    /// The bytes decoded, but the schema field is not one this binary writes.
    UnsupportedSchema(u32),
}

/// Size and model-routing lines sealed into a scope revision.
///
/// Structured rather than a blob of markdown so a later renderer can emit the
/// managed headings from the value. Changing the field layout is a new schema.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ScopeRouting {
    /// The scoped size line (for example `"M"`).
    pub size: String,
    /// The scoped model-routing line (for example `"construct: test"`).
    pub model: String,
}

/// An immutable, versioned scope: the structured work a commission currently
/// intends, addressed by the digest of these bytes.
///
/// Markdown is rendered *from* this value. The stored bytes are this encoding,
/// never a byte-preserved issue body.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ScopeRevision {
    /// Schema version. Version 1 writes [`SCOPE_REVISION_SCHEMA`]. A later
    /// schema is a new encoding, not a mutation of this field's meaning.
    pub schema: u32,
    /// The commission this revision belongs to.
    pub workpiece: WorkpieceId,
    /// The preceding revision's digest, or `None` for the first revision.
    pub predecessor: Option<Digest>,
    /// The problem statement.
    pub problem: String,
    /// The design notes.
    pub design: String,
    /// The implementation plan.
    pub plan: String,
    /// Declared-surface globs, in declaration order.
    pub declared_surface: Vec<String>,
    /// The dogfood brief. Empty when the scope asks for none.
    pub dogfood_brief: String,
    /// Size and model routing.
    pub routing: ScopeRouting,
    /// Workpieces this scope depends on, in declaration order.
    pub dependencies: Vec<WorkpieceId>,
    /// Advisory description. Never part of the signed subject on its own —
    /// it rides the revision so a later renderer can emit it from the same
    /// bytes an approval covers.
    pub description: String,
    /// ADR digests this revision implements. Empty when the commission does
    /// not bind itself to a stored decision. Trailing so the version-1 layout
    /// can name ADRs without a schema bump (#5124).
    pub implements: Vec<Digest>,
}

impl ScopeRevision {
    /// Decode canonical bytes as a version-1 revision.
    ///
    /// # Errors
    /// [`CommissionValueError::Malformed`] when the bytes are not this type;
    /// [`CommissionValueError::UnsupportedSchema`] when they decode as a
    /// later (or zero) schema this encoder does not write.
    pub fn from_canonical(bytes: &[u8]) -> Result<Self, CommissionValueError> {
        let value: Self = from_bytes(bytes).map_err(|_| CommissionValueError::Malformed)?;
        if value.schema != SCOPE_REVISION_SCHEMA {
            return Err(CommissionValueError::UnsupportedSchema(value.schema));
        }
        Ok(value)
    }

    /// Canonical aether-wire bytes of this revision.
    ///
    /// # Panics
    /// Panics if the value exceeds the ADR-0118 `u32` wire-length ceiling,
    /// which no commission value does.
    #[must_use]
    pub fn to_canonical(&self) -> Vec<u8> {
        to_vec(self).expect("commission values never exceed the ADR-0118 u32 wire-length ceiling")
    }
}

impl ContentAddressed for ScopeRevision {
    const DOMAIN: &'static str = "aether.bloomery.scope_revision";
}

/// Lifecycle of a commission row. Mutable store state, not part of a signed
/// revision — a land or cancel advances it without rewriting immutable bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CommissionStatus {
    /// Accepting revisions and approvals.
    Open,
    /// Closed by a signed cancel. Not this slice's write path.
    Cancelled,
    /// Marked landed after a successful land. Not this slice's write path.
    Landed,
}

impl CommissionStatus {
    /// The spelling stored in the `commissions.status` column.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Cancelled => "cancelled",
            Self::Landed => "landed",
        }
    }

    /// Parse a stored status spelling.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "open" => Some(Self::Open),
            "cancelled" => Some(Self::Cancelled),
            "landed" => Some(Self::Landed),
            _ => None,
        }
    }
}

/// Why an approval row is in the shared table: a signed author envelope, or
/// an unsigned policy attestation.
///
/// Distinct from [`crate::Tier`], which is the *policy* that decides whether
/// a surface may advance unattended. This is the row shape that keeps a
/// signed row from being stored as auto (and the reverse) — a CHECK
/// constraint on the store side, not a convention.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CommissionApprovalTier {
    /// An author signature is present and required.
    Signed,
    /// Policy provenance, no signature.
    Auto,
}

impl CommissionApprovalTier {
    /// The spelling stored in the `commission_approvals.tier` column.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Signed => "signed",
            Self::Auto => "auto",
        }
    }

    /// Parse a stored tier spelling.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "signed" => Some(Self::Signed),
            "auto" => Some(Self::Auto),
            _ => None,
        }
    }
}

/// The role a stored [`crate::Statement`] plays on a commission.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CommissionStatementRole {
    /// The intent statement named by `commissions.intent`.
    Intent,
    /// A signed cancel. Not this slice's write path.
    Cancel,
}

impl CommissionStatementRole {
    /// The spelling stored in the `commission_statements.role` column.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Intent => "intent",
            Self::Cancel => "cancel",
        }
    }

    /// Parse a stored role spelling.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "intent" => Some(Self::Intent),
            "cancel" => Some(Self::Cancel),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::collections::BTreeMap;
    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;

    use ed25519_dalek::{Signer, SigningKey};

    use super::{SCOPE_REVISION_SCHEMA, ScopeRevision, ScopeRouting};
    use crate::digest::digest_of;
    use crate::ids::{KeyId, WorkpieceId};
    use crate::sign::{AuthorityDoor, Ed25519KeyProvider, SignatureEnvelope, authorization_message};
    use crate::values::{Provenance, Statement};

    /// The fixture a signature may one day cover. Field values are short and
    /// fixed so a layout or encoder change is a visible golden drift, not a
    /// rewrite of production prose.
    fn fixture() -> ScopeRevision {
        ScopeRevision {
            schema: SCOPE_REVISION_SCHEMA,
            workpiece: WorkpieceId(String::from("issue-5045")),
            predecessor: None,
            problem: String::from("p"),
            design: String::from("d"),
            plan: String::from("n"),
            declared_surface: vec![String::from("crates/aether-bloomery/**")],
            dogfood_brief: String::from("df"),
            routing: ScopeRouting { size: String::from("M"), model: String::from("construct: test") },
            dependencies: Vec::new(),
            description: String::from("desc"),
            implements: Vec::new(),
        }
    }

    // Tripwire: version-1 wire bytes. A signature covers these exact bytes; the
    // encoder may be superseded, never silently changed. Recompute-and-repin
    // only when introducing a new schema.
    const GOLDEN_SCOPE_REVISION_V1: &[u8] = &[
        1, 0, 0, 0, 10, 0, 0, 0, 105, 115, 115, 117, 101, 45, 53, 48, 52, 53, 0, 1, 0, 0, 0, 112, 1, 0, 0, 0, 100, 1,
        0, 0, 0, 110, 1, 0, 0, 0, 25, 0, 0, 0, 99, 114, 97, 116, 101, 115, 47, 97, 101, 116, 104, 101, 114, 45, 98,
        108, 111, 111, 109, 101, 114, 121, 47, 42, 42, 2, 0, 0, 0, 100, 102, 1, 0, 0, 0, 77, 15, 0, 0, 0, 99, 111, 110,
        115, 116, 114, 117, 99, 116, 58, 32, 116, 101, 115, 116, 0, 0, 0, 0, 4, 0, 0, 0, 100, 101, 115, 99, 0, 0, 0, 0,
    ];

    // Tripwire: content address of the version-1 fixture under
    // `aether.bloomery.scope_revision`. Drifts if the domain tag, the hash, or
    // the fixture bytes move.
    const GOLDEN_SCOPE_REVISION_DIGEST: [u8; 32] = [
        251, 11, 252, 204, 212, 102, 22, 62, 168, 153, 97, 215, 99, 5, 239, 228, 36, 133, 53, 158, 99, 207, 253, 23,
        13, 58, 186, 139, 246, 97, 105, 120,
    ];

    // Tripwire: ADR-0182 authorization message for Approve over the fixture
    // digest, with words equal to that digest's raw bytes. Drifts if the door
    // discriminant, domain tag, or binding layout changes.
    const GOLDEN_APPROVE_AUTHORIZATION: [u8; 32] = [
        166, 129, 38, 225, 125, 70, 148, 35, 82, 146, 188, 122, 199, 210, 169, 61, 113, 120, 74, 9, 202, 80, 5, 254,
        161, 69, 91, 49, 103, 59, 159, 30,
    ];

    // Tripwire: ed25519 signature by seed-7 over the golden authorization
    // message. Drifts if the signed subject or the seed-7 key meaning changes.
    const GOLDEN_APPROVE_SIGNATURE: &[u8] = &[
        72, 10, 248, 173, 204, 193, 11, 124, 137, 171, 170, 32, 246, 207, 136, 171, 68, 136, 168, 228, 136, 110, 242,
        126, 118, 55, 49, 14, 85, 254, 6, 195, 151, 213, 135, 54, 117, 219, 214, 220, 137, 74, 132, 86, 180, 252, 29,
        35, 21, 56, 152, 50, 170, 134, 58, 130, 244, 244, 218, 79, 27, 93, 24, 3,
    ];

    #[test]
    fn version_one_encoding_matches_the_pinned_golden() {
        let revision = fixture();
        assert_eq!(revision.schema, SCOPE_REVISION_SCHEMA, "v1 writes schema 1 into the bytes");
        assert_eq!(
            revision.to_canonical().as_slice(),
            GOLDEN_SCOPE_REVISION_V1,
            "ScopeRevision v1 wire drifted; encoded={:?}",
            revision.to_canonical()
        );
        let decoded = ScopeRevision::from_canonical(GOLDEN_SCOPE_REVISION_V1)
            .expect("pinned version-1 bytes must decode against HEAD types");
        assert_eq!(decoded, revision);
    }

    #[test]
    fn version_one_digest_matches_the_pinned_golden() {
        assert_eq!(
            *digest_of(&fixture()).as_bytes(),
            GOLDEN_SCOPE_REVISION_DIGEST,
            "ScopeRevision content addressing drifted from the pinned golden digest"
        );
    }

    #[test]
    fn approve_authorization_over_the_fixture_matches_the_pinned_golden() {
        let digest = digest_of(&fixture());
        let message = authorization_message(AuthorityDoor::Approve, digest, digest.as_bytes());
        assert_eq!(
            *message.as_bytes(),
            GOLDEN_APPROVE_AUTHORIZATION,
            "Approve authorization message drifted; digest={digest:?} message={message:?}"
        );
    }

    #[test]
    fn a_seed_seven_signature_over_the_fixture_matches_the_pinned_golden() {
        let digest = digest_of(&fixture());
        let message = authorization_message(AuthorityDoor::Approve, digest, digest.as_bytes());
        let key = SigningKey::from_bytes(&[7; 32]);
        let signature = key.sign(message.as_bytes()).to_bytes();
        assert_eq!(
            signature.as_slice(),
            GOLDEN_APPROVE_SIGNATURE,
            "Approve signature drifted; signature={signature:?}"
        );

        let keys = Ed25519KeyProvider::new(BTreeMap::from([(KeyId(String::from("owner")), key.verifying_key())]));
        let statement = Statement {
            words: digest.as_bytes().to_vec(),
            provenance: Provenance::AuthorSignature(SignatureEnvelope {
                signer: KeyId(String::from("owner")),
                signature: signature.to_vec(),
            }),
            parents: Vec::new(),
        };
        assert!(
            statement.verify_authority(&keys, AuthorityDoor::Approve, digest),
            "the golden signature must verify over the fixture digest"
        );
    }

    #[test]
    fn unsupported_schema_is_refused_rather_than_decoded_as_v1() {
        let mut bytes = fixture().to_canonical();
        bytes[0] = 2;
        assert_eq!(
            ScopeRevision::from_canonical(&bytes),
            Err(super::CommissionValueError::UnsupportedSchema(2)),
            "a later schema must not be silently read as version 1"
        );
    }

    #[test]
    fn garbage_bytes_are_malformed_not_a_panic() {
        assert_eq!(
            ScopeRevision::from_canonical(&[0xff, 0x00]),
            Err(super::CommissionValueError::Malformed),
            "garbage must refuse rather than panic in the wire decoder"
        );
    }
}
