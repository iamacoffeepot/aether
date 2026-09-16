//! Typed verifier-failure identities and their canonical bounded set.
//!
//! An identity is a declared id: a position in the verifier vocabulary
//! [`super::vocabulary`] states, bounded at [`MAX_VERIFIER_IDENTITIES`]. A
//! position is the identity's bit in a failure set, so the order is append-only
//! — inserting one would shift every mask a recorded evidence envelope carries.
//!
//! The set is a `u16` and renders as four lowercase hex digits, which is the
//! failure token the umbrella stamps onto its evidence.

use std::fmt;

use serde::ser::SerializeSeq;
use serde::{Serialize, Serializer};

/// The most verifier identities the vocabulary may declare.
///
/// Sixteen keeps a failure set a `u16` and its token four hex digits.
pub(super) const MAX_VERIFIER_IDENTITIES: usize = 16;

/// The bound is the set's width, not a comment about it: a vocabulary that
/// outgrew the mask would silently drop the identities past bit fifteen.
const _: () = assert!(VerifyFailure::ALL.len() <= MAX_VERIFIER_IDENTITIES);

/// One verifier identity, in canonical vocabulary order.
///
/// The order is append-only and independent of the umbrella's run order: each
/// identity's bit is its position, so a new identity goes on the end or every
/// recorded mask shifts.
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
    /// `verify.suppress` failed.
    Suppress,
    /// The change edited a path no declared-surface glob covers. A legal
    /// identity no lane in this repository runs: containment is judged outside
    /// the umbrella, and stating that as a declared-but-unrun identity is what
    /// keeps the omission from being something to remember.
    Containment,
    /// `verify.lock` failed — a manifest edit landed without the matching
    /// `Cargo.lock` regeneration. Appended past [`Self::Containment`] so every
    /// earlier identity keeps its bit.
    Lock,
}

impl VerifyFailure {
    /// Every identity, in canonical order.
    pub const ALL: [Self; 10] = [
        Self::Preflight,
        Self::Fmt,
        Self::Clippy,
        Self::Docs,
        Self::Test,
        Self::Dup,
        Self::Deps,
        Self::Suppress,
        Self::Containment,
        Self::Lock,
    ];

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
            Self::Containment => "verify.containment",
            Self::Lock => "verify.lock",
        }
    }

    /// Decode one exact identity string.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|failure| failure.as_str() == name)
    }

    /// This identity's position in the vocabulary — its bit in a set.
    #[must_use]
    pub const fn position(self) -> u8 {
        match self {
            Self::Preflight => 0,
            Self::Fmt => 1,
            Self::Clippy => 2,
            Self::Docs => 3,
            Self::Test => 4,
            Self::Dup => 5,
            Self::Deps => 6,
            Self::Suppress => 7,
            Self::Containment => 8,
            Self::Lock => 9,
        }
    }

    const fn bit(self) -> u16 {
        1 << self.position()
    }
}

impl fmt::Display for VerifyFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl Serialize for VerifyFailure {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// The failed verifier identities of one run, as a bounded set.
///
/// A `u16` mask in memory and the ordered sequence of canonical names on the
/// wire, so the evidence envelope reads as the identity list it is rather than
/// as an opaque number.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct VerifyFailureSet {
    mask: u16,
}

impl VerifyFailureSet {
    /// The empty set.
    pub const EMPTY: Self = Self { mask: 0 };

    /// A set containing exactly `failure`.
    #[must_use]
    pub const fn one(failure: VerifyFailure) -> Self {
        Self { mask: failure.bit() }
    }

    /// Whether no failure identity is present.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.mask == 0
    }

    /// Whether `failure` belongs to the set.
    #[must_use]
    pub const fn contains(self, failure: VerifyFailure) -> bool {
        self.mask & failure.bit() != 0
    }

    /// The set-theoretic union.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self { mask: self.mask | other.mask }
    }

    /// Iterate the identities, in canonical order.
    pub fn iter(self) -> impl Iterator<Item = VerifyFailure> {
        VerifyFailure::ALL.into_iter().filter(move |failure| self.contains(*failure))
    }

    /// Encode the canonical failure token: exactly four lowercase hex digits.
    #[must_use]
    pub fn to_mask(self) -> String {
        format!("{:04x}", self.mask)
    }
}

impl fmt::Debug for VerifyFailureSet {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_set().entries(self.iter()).finish()
    }
}

impl FromIterator<VerifyFailure> for VerifyFailureSet {
    fn from_iter<I: IntoIterator<Item = VerifyFailure>>(failures: I) -> Self {
        failures.into_iter().fold(Self::EMPTY, |set, failure| set.union(Self::one(failure)))
    }
}

impl Serialize for VerifyFailureSet {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(None)?;
        for failure in self.iter() {
            sequence.serialize_element(&failure)?;
        }
        sequence.end()
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_VERIFIER_IDENTITIES, VerifyFailure, VerifyFailureSet};

    /// Tripwire: the token is the computed mask of the identities' positions,
    /// so an identity inserted rather than appended moves every token a
    /// recorded envelope already carries.
    #[test]
    fn the_token_is_the_bit_or_of_the_identity_positions() {
        assert_eq!(VerifyFailureSet::EMPTY.to_mask(), "0000");
        assert_eq!(VerifyFailureSet::one(VerifyFailure::Preflight).to_mask(), "0001");
        assert_eq!(VerifyFailureSet::one(VerifyFailure::Lock).to_mask(), "0200");
        assert_eq!(
            [VerifyFailure::Fmt, VerifyFailure::Clippy].into_iter().collect::<VerifyFailureSet>().to_mask(),
            "0006",
        );
    }

    #[test]
    fn a_set_serializes_as_its_canonical_name_sequence() {
        let set: VerifyFailureSet = [VerifyFailure::Lock, VerifyFailure::Fmt].into_iter().collect();

        assert_eq!(
            serde_json::to_value(set).expect("a failure set serializes"),
            serde_json::json!(["verify.fmt", "verify.lock"]),
        );
    }

    #[test]
    fn set_algebra_is_over_the_positions() {
        let all: VerifyFailureSet = VerifyFailure::ALL.into_iter().collect();
        let fmt = VerifyFailureSet::one(VerifyFailure::Fmt);

        assert!(all.contains(VerifyFailure::Fmt));
        assert!(!all.difference(fmt).contains(VerifyFailure::Fmt));
        assert!(all.difference(fmt).contains(VerifyFailure::Clippy));
        assert!(fmt.union(VerifyFailureSet::one(VerifyFailure::Docs)).contains(VerifyFailure::Docs));
        assert!(VerifyFailureSet::EMPTY.is_empty());
    }

    #[test]
    fn every_identity_round_trips_its_name_inside_the_bound() {
        for failure in VerifyFailure::ALL {
            assert_eq!(VerifyFailure::from_name(failure.as_str()), Some(failure));
            assert!(usize::from(failure.position()) < MAX_VERIFIER_IDENTITIES);
        }
        assert_eq!(VerifyFailure::from_name("verify.nothing"), None);
    }
}
