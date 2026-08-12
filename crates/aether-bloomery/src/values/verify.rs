//! Typed verifier-failure identities and their canonical bounded set (ADR-0178).

use alloc::string::String;
use core::fmt;
use core::iter::FromIterator;

use serde::de::{Error as DeError, SeqAccess, Visitor};
use serde::ser::SerializeSeq;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// One member of the closed V1 verifier-failure vocabulary, in canonical order.
///
/// The order is append-only and independent of the umbrella's run order: each
/// identity's bit is its position, so a new identity goes on the end or every
/// deployed mask shifts. Every bit of the mask byte carries an identity, so a
/// ninth one needs a `u16` set and a four-hex-digit token (ADR-0181).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum VerifyFailure {
    /// The umbrella could not satisfy its tool/target prerequisites.
    Preflight,
    /// `verify.fmt` failed.
    Fmt,
    /// `verify.clippy` failed.
    Clippy,
    /// `verify.docs` failed.
    Docs,
    /// `verify.test` failed.
    Test,
    /// `verify.dup` failed.
    Dup,
    /// `verify.deps` failed.
    Deps,
    /// `verify.suppress` failed (ADR-0181).
    Suppress,
}

impl VerifyFailure {
    /// Every V1 identity, in canonical wire order.
    pub const ALL: [Self; 8] =
        [Self::Preflight, Self::Fmt, Self::Clippy, Self::Docs, Self::Test, Self::Dup, Self::Deps, Self::Suppress];

    /// The canonical identity string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Preflight => "verify.preflight",
            Self::Fmt => "verify.fmt",
            Self::Clippy => "verify.clippy",
            Self::Docs => "verify.docs",
            Self::Test => "verify.test",
            Self::Dup => "verify.dup",
            Self::Deps => "verify.deps",
            Self::Suppress => "verify.suppress",
        }
    }

    /// Decode one exact canonical identity string.
    #[must_use]
    pub const fn from_name(name: &str) -> Option<Self> {
        match name.as_bytes() {
            b"verify.preflight" => Some(Self::Preflight),
            b"verify.fmt" => Some(Self::Fmt),
            b"verify.clippy" => Some(Self::Clippy),
            b"verify.docs" => Some(Self::Docs),
            b"verify.test" => Some(Self::Test),
            b"verify.dup" => Some(Self::Dup),
            b"verify.deps" => Some(Self::Deps),
            b"verify.suppress" => Some(Self::Suppress),
            _ => None,
        }
    }

    const fn bit(self) -> u8 {
        1 << (self as u8)
    }
}

impl fmt::Display for VerifyFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl Serialize for VerifyFailure {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

struct VerifyFailureVisitor;

impl Visitor<'_> for VerifyFailureVisitor {
    type Value = VerifyFailure;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a canonical verify.* failure identity")
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: DeError,
    {
        VerifyFailure::from_name(value).ok_or_else(|| E::unknown_variant(value, &VERIFY_FAILURE_NAMES))
    }
}

impl<'de> Deserialize<'de> for VerifyFailure {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_str(VerifyFailureVisitor)
    }
}

const VERIFY_FAILURE_NAMES: [&str; 8] = [
    "verify.preflight",
    "verify.fmt",
    "verify.clippy",
    "verify.docs",
    "verify.test",
    "verify.dup",
    "verify.deps",
    "verify.suppress",
];

/// A deduplicated verifier-failure set with one canonical order and mask.
///
/// The empty set is a valid transport value for a passing or non-Verify result.
/// Whether a failed member Verify may be empty is an intake-boundary invariant,
/// not a property of this reusable value.
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash, Debug)]
pub struct VerifyFailureSet(u8);

impl VerifyFailureSet {
    /// The empty set.
    pub const EMPTY: Self = Self(0);

    /// A set containing exactly `failure`.
    #[must_use]
    pub const fn one(failure: VerifyFailure) -> Self {
        Self(failure.bit())
    }

    /// Whether no failure identity is present.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Whether `failure` belongs to the set.
    #[must_use]
    pub const fn contains(self, failure: VerifyFailure) -> bool {
        self.0 & failure.bit() != 0
    }

    /// The set-theoretic union.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// The set-theoretic intersection.
    #[must_use]
    pub const fn intersection(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }

    /// Iterate in the canonical identity order.
    pub fn iter(self) -> impl Iterator<Item = VerifyFailure> {
        VerifyFailure::ALL.into_iter().filter(move |failure| self.contains(*failure))
    }

    /// Encode the canonical artifact token: exactly two lowercase hex digits.
    #[must_use]
    pub fn to_mask(self) -> String {
        format!("{:02x}", self.0)
    }

    /// Decode an exact two-lowercase-hex-digit artifact token.
    ///
    /// Refuses wrong length, uppercase, and non-hex text. Every bit of the byte
    /// carries an identity, so any well-formed token names a valid set and the
    /// decode makes no unknown-bit refusal (ADR-0181); the workflow's own
    /// canonical-order and duplicate checks, plus the evidence digest, carry the
    /// semantic validation on the Actions path.
    #[must_use]
    pub fn from_mask(mask: &str) -> Option<Self> {
        let bytes = mask.as_bytes();
        if bytes.len() != 2 || !bytes.iter().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte)) {
            return None;
        }
        Some(Self((hex_nibble(bytes[0])? << 4) | hex_nibble(bytes[1])?))
    }
}

const fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

impl FromIterator<VerifyFailure> for VerifyFailureSet {
    fn from_iter<T: IntoIterator<Item = VerifyFailure>>(failures: T) -> Self {
        failures.into_iter().fold(Self::EMPTY, |set, failure| set.union(Self::one(failure)))
    }
}

impl Serialize for VerifyFailureSet {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut sequence = serializer.serialize_seq(Some(self.0.count_ones() as usize))?;
        for failure in self.iter() {
            sequence.serialize_element(&failure)?;
        }
        sequence.end()
    }
}

struct VerifyFailureSetVisitor;

impl<'de> Visitor<'de> for VerifyFailureSetVisitor {
    type Value = VerifyFailureSet;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a canonical ordered array of unique verifier failures")
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut set = VerifyFailureSet::EMPTY;
        let mut previous = None;
        while let Some(failure) = sequence.next_element::<VerifyFailure>()? {
            if set.contains(failure) {
                return Err(A::Error::custom(format!("duplicate verifier failure `{failure}`")));
            }
            if previous.is_some_and(|previous| previous >= failure) {
                return Err(A::Error::custom("verifier failures are not in canonical order"));
            }
            set = set.union(VerifyFailureSet::one(failure));
            previous = Some(failure);
        }
        Ok(set)
    }
}

impl<'de> Deserialize<'de> for VerifyFailureSet {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_seq(VerifyFailureSetVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::{VerifyFailure, VerifyFailureSet};
    use serde::Deserialize;
    use serde::de::value::{Error as ValueError, SeqDeserializer, StrDeserializer};

    use aether_data::wire::{from_bytes, to_vec};

    fn set(failures: &[VerifyFailure]) -> VerifyFailureSet {
        failures.iter().copied().collect()
    }

    fn decode(names: &[&'static str]) -> Result<VerifyFailureSet, ValueError> {
        let values = names.iter().copied().map(StrDeserializer::<ValueError>::new);
        VerifyFailureSet::deserialize(SeqDeserializer::new(values))
    }

    #[test]
    fn identities_and_sets_round_trip_in_canonical_order() {
        let failures = set(&[VerifyFailure::Deps, VerifyFailure::Fmt, VerifyFailure::Preflight]);
        let bytes = to_vec(&failures).expect("set serializes");

        assert_eq!(failures.iter().map(VerifyFailure::as_str).collect::<Vec<_>>(), MEMBERS_IN_CANONICAL_ORDER);
        assert_eq!(from_bytes::<VerifyFailureSet>(&bytes).expect("set decodes"), failures);
        assert_eq!(VerifyFailure::Clippy.as_str(), "verify.clippy");
    }

    #[test]
    fn set_helpers_are_set_theoretic() {
        let left = set(&[VerifyFailure::Fmt, VerifyFailure::Clippy]);
        let right = set(&[VerifyFailure::Clippy, VerifyFailure::Docs]);

        assert_eq!(left.union(right), set(&[VerifyFailure::Fmt, VerifyFailure::Clippy, VerifyFailure::Docs]));
        assert_eq!(left.intersection(right), VerifyFailureSet::one(VerifyFailure::Clippy));
        assert!(VerifyFailureSet::EMPTY.is_empty());
    }

    #[test]
    fn serde_refuses_unknown_duplicate_and_out_of_order_values() {
        assert!(decode(&["verify.unknown"]).is_err());
        assert!(decode(&["verify.fmt", "verify.fmt"]).is_err());
        assert!(decode(&["verify.docs", "verify.fmt"]).is_err());
    }

    #[test]
    fn empty_set_is_a_valid_cursor_and_transport_value() {
        let bytes = to_vec(&VerifyFailureSet::EMPTY).expect("empty serializes");
        assert_eq!(from_bytes::<VerifyFailureSet>(&bytes).expect("empty decodes"), VerifyFailureSet::EMPTY);
        assert_eq!(VerifyFailureSet::from_mask("00"), Some(VerifyFailureSet::EMPTY));
    }

    #[test]
    fn exact_lowercase_mask_round_trips_and_rejects_invalid_tokens() {
        let failures = set(&[VerifyFailure::Preflight, VerifyFailure::Clippy, VerifyFailure::Deps]);
        assert_eq!(failures.to_mask(), "45");
        assert_eq!(VerifyFailureSet::from_mask("45"), Some(failures));
        assert_eq!(VerifyFailureSet::from_mask("7f").map(VerifyFailureSet::to_mask).as_deref(), Some("7f"));

        // Tripwire: the whole vocabulary must still fit the two-hex-digit token
        // the attempt-artifact grammar reserves for it. A ninth identity shifts
        // `bit()` by 8, and with overflow checks on — the profile `cargo test`
        // and CI run — that panics here before the comparison is reached. The
        // panic is the signal, not the compared mask: with overflow checks off
        // the shift wraps to bit 0, the mask still reads `ff`, and this line
        // passes, so a release-profile run does not catch the ninth identity.
        assert_eq!(VerifyFailure::ALL.into_iter().collect::<VerifyFailureSet>().to_mask(), "ff");
        assert_eq!(VerifyFailureSet::one(VerifyFailure::Suppress).to_mask(), "80");
        assert_eq!(VerifyFailureSet::from_mask("80"), Some(VerifyFailureSet::one(VerifyFailure::Suppress)));
        assert_eq!(
            VerifyFailureSet::from_mask("ff"),
            Some(VerifyFailure::ALL.into_iter().collect::<VerifyFailureSet>())
        );

        for invalid in ["0", "000", "0A", "GG", "g0", "-1"] {
            assert!(VerifyFailureSet::from_mask(invalid).is_none(), "`{invalid}` must be refused");
        }
    }

    const MEMBERS_IN_CANONICAL_ORDER: [&str; 3] = ["verify.preflight", "verify.fmt", "verify.deps"];
}
