//! Content addressing (ADR-0149 §The value vocabulary).
//!
//! A [`Digest`] is a sha256 over a value's *canonical aether-wire bytes*
//! ([`aether_data::wire`], ADR-0118) with a per-type domain tag hashed ahead
//! of them. The workspace already owns one canonical encoding; digests reuse
//! it rather than inventing a parallel canonicalization that would drift.
//! sha256 — not `aether-data`'s FNV-1a id hashing — because content
//! addressing needs collision resistance, consistent with the hub binary
//! store (ADR-0115).
//!
//! # Typed content addressing
//!
//! The wire format is positional and untagged, so structurally-identical
//! values of different Rust types encode to identical bytes. To deliver the
//! ADR-0149 promise of *typed* content addressing, [`digest_of`] hashes a
//! stable per-type domain tag ([`ContentAddressed::DOMAIN`]) ahead of the
//! value bytes: distinct types produce distinct digests by construction, and
//! the [`ContentAddressed`] bound leaves no untagged path over a typed value
//! to reach for. The domain tag is part of every digest of its type — it must
//! never change once a digest is persisted, or the address of every value of
//! that type moves.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use aether_data::schema::SchemaType;
use aether_data::wire::to_vec;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::persisted::RenderError;
use crate::persisted::render_schema;

const HEX_DIGIT: &[u8; 16] = b"0123456789abcdef";

/// Domain tag hashed ahead of a canonical schema rendering so a schema digest
/// can never collide with a value digest over the same bytes (ADR-0187).
pub const SCHEMA_DIGEST_DOMAIN: &str = "aether.bloomery.schema";

/// Encode `bytes` as lowercase hex.
///
/// Encoders emit lowercase; [`decode_hex`] and [`Digest::from_hex`] refuse
/// uppercase. The case rule is the one the estate's strictest existing
/// decoder already enforced.
#[must_use]
pub fn encode_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(HEX_DIGIT[usize::from(byte >> 4)]));
        out.push(char::from(HEX_DIGIT[usize::from(byte & 0x0f)]));
    }
    out
}

/// One lowercase hex digit to its nibble.
///
/// Refuses uppercase and non-hex, including sign characters (`+`, `-`) that
/// `from_str_radix` would accept as part of a number.
#[must_use]
pub const fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

/// Decode even-length lowercase hex into bytes.
///
/// Refuses uppercase, odd length, and non-hex. There is no sign: a leading
/// `+` or `-` is a non-hex character, not a radix prefix.
#[must_use]
pub fn decode_hex(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    let raw = hex.as_bytes();
    let mut out = Vec::with_capacity(raw.len() / 2);
    for pair in raw.chunks_exact(2) {
        out.push((hex_nibble(pair[0])? << 4) | hex_nibble(pair[1])?);
    }
    Some(out)
}

/// A sha256 digest over a value's canonical aether-wire bytes.
///
/// The primitive of the derivation DAG: every artifact is addressed by its
/// digest and names its parents by theirs. `Ord` (via the raw 32 bytes)
/// gives blooms a canonical member order at seal time.
#[derive(
    aether_data::Schema, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default, Serialize, Deserialize,
)]
pub struct Digest([u8; 32]);

impl Digest {
    /// Wrap 32 raw digest bytes — for reconstructing a digest from storage
    /// or naming a fixed one in a test.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Wrap a slice of raw digest bytes, or `None` when it is not exactly 32
    /// long.
    ///
    /// The store and the wire both carry a digest as an opaque byte string, so
    /// every read back into the value vocabulary crosses this boundary. Fallible
    /// rather than panicking: the length is a property of data that came from
    /// outside, and a caller decides whether a bad one is a refusal or an abort.
    #[must_use]
    pub fn from_slice(bytes: &[u8]) -> Option<Self> {
        <[u8; 32]>::try_from(bytes).ok().map(Self::from_bytes)
    }

    /// The raw 32 digest bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Render as 64 lowercase hex characters.
    ///
    /// Encoders emit lowercase; [`Self::from_hex`] refuses uppercase. The
    /// case rule is the one the estate's strictest existing decoder already
    /// enforced.
    #[must_use]
    pub fn to_hex(&self) -> String {
        encode_hex(self.as_bytes())
    }

    /// Parse exactly 64 lowercase hex characters into a digest.
    ///
    /// Refuses uppercase, the wrong length, and non-hex characters — including
    /// a signed prefix such as `+a` that `from_str_radix` would accept. There
    /// is no second spelling.
    #[must_use]
    pub fn from_hex(hex: &str) -> Option<Self> {
        if hex.len() != 64 {
            return None;
        }
        let bytes = decode_hex(hex)?;
        Self::from_slice(&bytes)
    }

    /// sha256 of an already-encoded byte string.
    #[must_use]
    pub fn of_wire_bytes(bytes: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        Self(hasher.finalize().into())
    }

    /// sha256 of a domain tag, length-prefixed as little-endian `u32`, followed
    /// by already-encoded value bytes.
    ///
    /// The one recipe [`digest_of`] and
    /// [`config_address`](crate::values::config_address) both use, so the typed
    /// path and the sealed-registry path cannot drift.
    ///
    /// # Panics
    ///
    /// Panics if `domain` is longer than `u32::MAX`, which no domain tag or kind
    /// name is.
    #[must_use]
    pub fn of_domain_tagged(domain: &str, bytes: &[u8]) -> Self {
        let domain_len =
            u32::try_from(domain.len()).expect("a domain tag is a short static string, well under the u32 ceiling");
        let mut hasher = Sha256::new();
        hasher.update(domain_len.to_le_bytes());
        hasher.update(domain.as_bytes());
        hasher.update(bytes);
        Self(hasher.finalize().into())
    }
}

/// Digest of a kind's schema: sha256 over a bloomery-owned canonical rendering
/// of the shape, domain-tagged so it cannot collide with a value digest over
/// the same bytes (ADR-0187).
///
/// The rendering is a function of the *shape* alone. A change to
/// [`SchemaType`] itself in the data crate therefore moves no existing
/// pinned digest.
///
/// # Errors
///
/// [`RenderError`] when the schema tree exceeds the walker's node or depth
/// budget. Compiled-in kinds are well under both.
pub fn schema_digest(kind: &str, schema: &SchemaType) -> Result<Digest, RenderError> {
    Ok(Digest::of_domain_tagged(SCHEMA_DIGEST_DOMAIN, &render_schema(kind, schema)?))
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// A value that is content-addressed by digest.
///
/// The `const DOMAIN` is a stable per-type domain-separation tag [`digest_of`]
/// hashes length-prefixed ahead of the value's wire bytes, so two
/// structurally-identical values of different vocabulary types never share a
/// digest. Making the tag mandatory through this trait bound is what delivers
/// "typed content addressed" (ADR-0149 §The value vocabulary) by construction:
/// there is no untagged `digest_of` path over a typed value.
///
/// `DOMAIN` is part of every persisted digest of the implementing type, so it
/// must be an explicit, stable string — never `core::any::type_name`, whose
/// output is not stable across compiler versions.
pub trait ContentAddressed: Serialize {
    /// The stable domain-separation tag for this type.
    const DOMAIN: &'static str;
}

/// The digest of a content-addressed value: sha256 over its type's domain tag
/// (length-prefixed) followed by the value's canonical aether-wire encoding.
///
/// The domain tag ([`ContentAddressed::DOMAIN`]) is hashed through
/// [`Digest::of_domain_tagged`] ahead of the value bytes so the domain/value
/// boundary is unambiguous, distinct types never collide, and the sealed
/// registry path cannot drift from this one.
///
/// Infallible by invariant: every bloom value encodes well under the ADR-0118
/// `u32` wire-length ceiling — no control-plane value approaches 4 GiB — so an
/// encode failure is a broken invariant, not a recoverable runtime condition.
/// It panics rather than degrade to a wrong address (the prior
/// `unwrap_or_default` aliased every encode failure, and any genuinely-empty
/// encoding, to a single colliding digest — the wrong degradation for a
/// content-addressing primitive).
///
/// # Panics
///
/// Panics if `value` fails to wire-encode — i.e. some length exceeds the
/// ADR-0118 `u32` ceiling. This cannot happen for any bloom value (none
/// approaches 4 GiB); an occurrence is a broken invariant, deliberately loud
/// rather than a silently colliding address.
#[must_use]
pub fn digest_of<T: ContentAddressed + ?Sized>(value: &T) -> Digest {
    let bytes = to_vec(value).expect("bloom values never exceed the ADR-0118 u32 wire-length ceiling");
    Digest::of_domain_tagged(T::DOMAIN, &bytes)
}

#[cfg(test)]
mod tests {
    use aether_data::schema::SchemaType;

    use crate::persisted::render_schema;

    use super::{ContentAddressed, Digest, SCHEMA_DIGEST_DOMAIN, decode_hex, digest_of, encode_hex, schema_digest};

    // Two ContentAddressed impls sharing one byte payload but differing in
    // DOMAIN, so the domain tag is the only thing that can distinguish them.
    struct Alpha(u32);
    struct Beta(u32);

    impl serde::Serialize for Alpha {
        fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            self.0.serialize(s)
        }
    }
    impl serde::Serialize for Beta {
        fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            self.0.serialize(s)
        }
    }

    impl ContentAddressed for Alpha {
        const DOMAIN: &'static str = "test.alpha";
    }
    impl ContentAddressed for Beta {
        const DOMAIN: &'static str = "test.beta";
    }

    #[test]
    fn distinct_domain_over_identical_bytes_yields_distinct_digests() {
        // Alpha and Beta encode to the same wire bytes; only the domain tag
        // differs, so a collision here means domain separation is not applied.
        assert_ne!(digest_of(&Alpha(7)), digest_of(&Beta(7)));
    }

    #[test]
    fn same_value_yields_stable_digest() {
        assert_eq!(digest_of(&Alpha(7)), digest_of(&Alpha(7)));
    }

    #[test]
    fn schema_digest_is_domain_separated_from_a_value_hash_of_the_same_bytes() {
        // A schema digest and a value digest over identical bytes must not
        // collide, or a stored schema identity could be mistaken for a value
        // address.
        let rendered = render_schema("k", &SchemaType::Unit).expect("a unit schema renders");
        assert_ne!(
            schema_digest("k", &SchemaType::Unit).expect("unit schema digests"),
            Digest::of_wire_bytes(&rendered)
        );
        assert_ne!(Digest::of_domain_tagged(SCHEMA_DIGEST_DOMAIN, &rendered), Digest::of_domain_tagged("k", &rendered));
    }

    #[test]
    fn hex_round_trips_lowercase_and_refuses_uppercase_and_signed_prefix() {
        let digest = Digest::from_bytes([0x5c; 32]);
        let hex = digest.to_hex();
        assert_eq!(hex, "5c".repeat(32));
        assert_eq!(Digest::from_hex(&hex), Some(digest));
        assert_eq!(Digest::from_hex(&hex.to_ascii_uppercase()), None);
        assert_eq!(Digest::from_hex(&"a".repeat(63)), None);

        let mut signed = String::from("+a");
        signed.push_str(&"0".repeat(62));
        assert_eq!(Digest::from_hex(&signed), None);
        assert_eq!(decode_hex("+a"), None);
        assert_eq!(encode_hex(&[0x00, 0x0f, 0xa0, 0xff]), "000fa0ff");
    }
}
