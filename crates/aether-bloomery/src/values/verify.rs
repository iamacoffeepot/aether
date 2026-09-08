//! Typed verifier-failure identities and their canonical bounded set (ADR-0178,
//! ADR-0215).
//!
//! An identity is a *declared* id: a position in the verifier vocabulary the
//! bloom's sealed [`PipelineManifest`](super::PipelineManifest) states, bounded
//! at [`MAX_VERIFIER_IDENTITIES`]. This binary compiles ten of them
//! ([`VerifyFailure::ALL`]) because this repository's `pipeline.toml` declares
//! exactly those ten; a caller holding no manifest — `xtask`, the Actions
//! wrapper's rendered bit table — reads the vocabulary out of that compiled
//! copy, and a caller holding one interns against it
//! ([`PipelineManifest::intern`](super::PipelineManifest::intern)).
//!
//! # Decode is tolerant, intake is strict
//!
//! [`WireDecode`] has no manifest in hand, so it admits any syntactically valid
//! identity and gives one this binary compiles no name for the next position
//! above the compiled vocabulary. A malformed name still fails at decode; a
//! well-formed one the bloom's manifest does not declare fails at admission,
//! with the bloom — and so the manifest — in hand. That is the trust boundary
//! ADR-0178 already named for the check, moved to where the vocabulary is
//! recorded rather than compiled.
//!
//! The persisted shape is unchanged by that move and deliberately so: an
//! identity travels as its canonical name string and a set as the ordered
//! sequence of those names, with the same [`Schema`] and the same labels as
//! before, so no journal or config schema digest moves and no
//! [`PersistedUpcast`](crate::persisted::PersistedUpcast) is owed.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;
use core::iter::FromIterator;

use aether_data::Schema;
use aether_data::schema::{LabelCell, LabelNode, SchemaCell, SchemaType};
use aether_data::wire::{Error as WireError, WireDecode, WireEncode};
use serde::de::{Error as DeError, SeqAccess, Visitor};
use serde::ser::SerializeSeq;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::MAX_VERIFIER_IDENTITIES;
use crate::hex_nibble;

/// The prefix every verifier identity carries.
const IDENTITY_PREFIX: &str = "verify.";

/// The most bytes one verifier identity's name may occupy.
///
/// A cap rather than an open string because a declared identity is stored
/// inline in a [`VerifyFailureSet`], which is `Copy` and travels through the
/// fold by value. The longest identity this repository declares is
/// `verify.containment` at eighteen bytes.
pub const MAX_VERIFIER_IDENTITY_BYTES: usize = 24;

/// How many identities a set can carry that this binary compiles no name for —
/// the declared vocabulary's width less the compiled one's.
const DECLARED_SLOTS: usize = MAX_VERIFIER_IDENTITIES - VerifyFailure::ALL.len();

/// The vocabulary bound as a position count, which is what a mask is indexed by.
const POSITION_COUNT: u8 = MAX_VERIFIER_IDENTITIES as u8;

/// One verifier identity's name, stored inline so an identity stays `Copy`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct IdentityName {
    bytes: [u8; MAX_VERIFIER_IDENTITY_BYTES],
    len: u8,
}

impl IdentityName {
    const EMPTY: Self = Self { bytes: [0; MAX_VERIFIER_IDENTITY_BYTES], len: 0 };

    /// The name, if it is a syntactically valid identity: the `verify.` prefix,
    /// a non-empty lowercase-ASCII tail, and at most
    /// [`MAX_VERIFIER_IDENTITY_BYTES`].
    fn new(name: &str) -> Option<Self> {
        if name.len() > MAX_VERIFIER_IDENTITY_BYTES || name.len() <= IDENTITY_PREFIX.len() {
            return None;
        }
        if !name.starts_with(IDENTITY_PREFIX) {
            return None;
        }
        if !name.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_')) {
            return None;
        }

        let mut stored = Self::EMPTY;
        stored.bytes[..name.len()].copy_from_slice(name.as_bytes());
        stored.len = u8::try_from(name.len()).ok()?;
        Some(stored)
    }

    fn is_empty(self) -> bool {
        self.len == 0
    }

    fn as_str(&self) -> &str {
        // Every stored name came through `new`, which admits ASCII only, so the
        // bytes are valid UTF-8 by construction; an empty name renders empty.
        core::str::from_utf8(&self.bytes[..usize::from(self.len)]).unwrap_or("")
    }
}

impl fmt::Debug for IdentityName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), formatter)
    }
}

/// A verifier identity a manifest declared past the vocabulary this binary
/// compiles a name for.
///
/// Carries both halves because both are load-bearing and neither is derivable
/// from the other without the manifest: the position is the identity's bit in
/// a recorded set, and the name is what the persisted row actually says.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeclaredIdentity {
    position: u8,
    name: IdentityName,
}

impl DeclaredIdentity {
    /// This identity's position in the vocabulary that declared it — its bit.
    #[must_use]
    pub const fn position(self) -> u8 {
        self.position
    }

    /// The canonical identity string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.name.as_str()
    }
}

impl fmt::Debug for DeclaredIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("DeclaredIdentity").field(&self.name).field(&self.position).finish()
    }
}

/// One verifier identity, in canonical vocabulary order.
///
/// The order is append-only and independent of the umbrella's run order: each
/// identity's bit is its position, so a new identity goes on the end or every
/// deployed mask shifts. The set is a `u16` and the artifact token is four
/// lowercase hex digits; a two-digit token still decodes as the same eight
/// identities it already named, zero-extended (ADR-0209).
///
/// The ten named variants are the vocabulary this binary compiles, which is the
/// vocabulary this repository's `pipeline.toml` declares. [`Self::Declared`]
/// carries one a manifest declared past them — admitted by the decoder, refused
/// at intake unless the bloom's own manifest declares it.
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
    /// The candidate edited a path no declared-surface glob covers (ADR-0209).
    Containment,
    /// `verify.lock` failed — a manifest edit landed without the matching
    /// `Cargo.lock` regeneration (#5309). Appended past
    /// [`Self::Containment`] so every earlier identity keeps its bit.
    Lock,
    /// An identity a sealed manifest declares past the compiled vocabulary.
    ///
    /// Ordered last, and correctly: a declared position is always at or above
    /// the compiled vocabulary's width, so the derived ordering over this enum
    /// is the ordering over positions.
    Declared(DeclaredIdentity),
}

impl VerifyFailure {
    /// Every identity this binary compiles, in canonical wire order.
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
    pub fn as_str(&self) -> &str {
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
            Self::Declared(identity) => identity.as_str(),
        }
    }

    /// Decode one exact identity string the *compiled* vocabulary names.
    ///
    /// Deliberately not a membership test against a sealed vocabulary: this is
    /// what a caller holding no manifest has, and the manifest-holding answer is
    /// [`PipelineManifest::intern`](super::PipelineManifest::intern).
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
            b"verify.containment" => Some(Self::Containment),
            b"verify.lock" => Some(Self::Lock),
            _ => None,
        }
    }

    /// One identity at `position`, for a `name` the compiled vocabulary does
    /// not carry.
    ///
    /// `None` for a name that is not a syntactically valid identity, for a name
    /// the compiled vocabulary *does* carry (which has exactly one
    /// representation, its own variant), and for a position outside the
    /// declared range — positions below the compiled width belong to the
    /// compiled identities, and positions at or above
    /// [`MAX_VERIFIER_IDENTITIES`] are outside the bound the manifest reader
    /// enforces.
    #[must_use]
    pub fn declared(position: u8, name: &str) -> Option<Self> {
        if Self::from_name(name).is_some() {
            return None;
        }
        let slot = usize::from(position).checked_sub(Self::ALL.len())?;
        if slot >= DECLARED_SLOTS {
            return None;
        }
        Some(Self::Declared(DeclaredIdentity { position, name: IdentityName::new(name)? }))
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
            Self::Declared(identity) => identity.position,
        }
    }

    /// This identity's index in [`Self::ALL`], or `None` for one the compiled
    /// vocabulary does not name.
    ///
    /// What a fixed-width table indexed by the compiled vocabulary needs, and
    /// the honest answer for an identity outside it: the ADR-0184 ledger's
    /// per-identity columns key on the string rather than the position in the
    /// slice that follows this one (#5817).
    #[must_use]
    pub fn compiled_position(self) -> Option<usize> {
        let position = usize::from(self.position());
        (position < Self::ALL.len()).then_some(position)
    }

    const fn bit(self) -> u16 {
        1 << self.position()
    }

    /// The first position a declared identity may occupy — the compiled
    /// vocabulary's width.
    const fn first_declared_position() -> u8 {
        POSITION_COUNT - DECLARED_SLOTS as u8
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

// The serde impls above and below are hand-written, so the schema has to be
// hand-written with them: an identity travels as its canonical name, not as an
// enum discriminant, and a set travels as the ordered sequence of those names
// rather than as the mask it is in memory. A derive here would describe a
// unit enum and a newtype over `u16` — a shape the wire never carries. The
// schema-versus-serde equivalence is asserted over the whole decisions graph in
// `tests/golden_decisions`, so a drift between these two descriptions fails
// there rather than silently mis-describing the column.
//
// Both are byte-identical to what they were before an identity became a
// declared id (ADR-0215): the shape a row carries did not move, so no journal
// digest moves and no upcast is owed.
impl Schema for VerifyFailure {
    const SCHEMA: SchemaType = SchemaType::String;
    const LABEL: Option<&'static str> = Some("aether.bloomery.verify_failure");
    const LABEL_NODE: LabelNode = LabelNode::Anonymous;
}

impl Schema for VerifyFailureSet {
    const SCHEMA: SchemaType = SchemaType::Vec(SchemaCell::Static(&VerifyFailure::SCHEMA));
    const LABEL: Option<&'static str> = Some("aether.bloomery.verify_failure_set");
    const LABEL_NODE: LabelNode = LabelNode::Vec(LabelCell::Static(&VerifyFailure::LABEL_NODE));
}

impl WireEncode for VerifyFailure {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), WireError> {
        self.as_str().encode(out)
    }
}

impl<'de> WireDecode<'de> for VerifyFailure {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, WireError> {
        let name = String::decode(cursor)?;
        // A lone identity carries no sequence to intern against, so one the
        // compiled vocabulary does not name takes the first declared position.
        // The real interning is the set decoder's, below — a lone identity is
        // not a shape any durable row carries, since every recorded verifier
        // verdict is a set.
        Self::from_name(&name)
            .or_else(|| Self::declared(Self::first_declared_position(), &name))
            .ok_or(WireError::Message(name))
    }
}

impl WireEncode for VerifyFailureSet {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), WireError> {
        self.iter().collect::<Vec<_>>().encode(out)
    }
}

impl<'de> WireDecode<'de> for VerifyFailureSet {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, WireError> {
        let mut set = Self::EMPTY;
        for name in Vec::<String>::decode(cursor)? {
            let Some(failure) = set.intern(&name) else {
                return Err(WireError::Message(name));
            };
            set = set.union(Self::one(failure));
        }
        Ok(set)
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
        VerifyFailure::from_name(value)
            .or_else(|| VerifyFailure::declared(VerifyFailure::first_declared_position(), value))
            .ok_or_else(|| E::unknown_variant(value, &VERIFY_FAILURE_NAMES))
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

const VERIFY_FAILURE_NAMES: [&str; 10] = [
    "verify.preflight",
    "verify.fmt",
    "verify.clippy",
    "verify.docs",
    "verify.test",
    "verify.dup",
    "verify.deps",
    "verify.suppress",
    "verify.containment",
    "verify.lock",
];

/// A deduplicated verifier-failure set with one canonical order and mask.
///
/// The empty set is a valid transport value for a passing or non-Verify result.
/// Whether a failed member Verify may be empty is an intake-boundary invariant,
/// not a property of this reusable value.
///
/// The mask is over *interned positions*, and interning is relative to a
/// vocabulary — the compiled one for a reader holding no manifest, which is the
/// same relativity the mask always had against a binary. The names of
/// identities the compiled vocabulary does not carry ride beside the mask so a
/// decoded row re-encodes as the row it was, rather than losing the very
/// identity the tolerance exists to admit.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct VerifyFailureSet {
    mask: u16,
    declared: [IdentityName; DECLARED_SLOTS],
}

impl VerifyFailureSet {
    /// The empty set.
    pub const EMPTY: Self = Self { mask: 0, declared: [IdentityName::EMPTY; DECLARED_SLOTS] };

    /// A set containing exactly `failure`.
    #[must_use]
    pub fn one(failure: VerifyFailure) -> Self {
        let mut set = Self { mask: failure.bit(), ..Self::EMPTY };
        if let VerifyFailure::Declared(identity) = failure
            && let Some(slot) = Self::slot_of(identity.position)
        {
            set.declared[slot] = identity.name;
        }
        set
    }

    /// Whether no failure identity is present.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.mask == 0
    }

    /// Whether `failure` belongs to the set.
    #[must_use]
    pub fn contains(self, failure: VerifyFailure) -> bool {
        self.identity_at(failure.position()) == Some(failure)
    }

    /// The set-theoretic union.
    #[must_use]
    pub fn union(self, other: Self) -> Self {
        let mut out = Self { mask: self.mask | other.mask, declared: self.declared };
        for (slot, name) in out.declared.iter_mut().enumerate() {
            if name.is_empty() {
                *name = other.declared[slot];
            }
        }
        out
    }

    /// The set-theoretic intersection.
    #[must_use]
    pub fn intersection(self, other: Self) -> Self {
        self.retaining(self.mask & other.mask)
    }

    /// The set-theoretic difference: every identity in `self` that `other` does
    /// not name.
    ///
    /// What one gate set states by *subtraction* from another — the member
    /// verify position's vocabulary is the fold's less the one gate it does not
    /// run ([`VerifyGateSet::member`](crate::VerifyGateSet::member)) — so the two
    /// stay one hand-written list and one stated difference rather than two
    /// lists free to drift.
    #[must_use]
    pub fn difference(self, other: Self) -> Self {
        self.retaining(self.mask & !other.mask)
    }

    /// Iterate in the canonical identity order.
    ///
    /// A mask bit at a declared position whose name this set does not carry —
    /// what a four-hex artifact token alone can produce — names no identity and
    /// is skipped, exactly as an unknown bit was invisible to iteration before
    /// declared identities existed.
    pub fn iter(self) -> impl Iterator<Item = VerifyFailure> {
        (0..POSITION_COUNT).filter_map(move |position| self.identity_at(position))
    }

    /// Encode the canonical artifact token: exactly four lowercase hex digits.
    #[must_use]
    pub fn to_mask(self) -> String {
        format!("{:04x}", self.mask)
    }

    /// Decode a two- or four-lowercase-hex-digit artifact token.
    ///
    /// A two-digit token zero-extends to the same eight identities it already
    /// named. Refuses every other length, uppercase, and non-hex text. The
    /// decode makes no unknown-bit refusal (ADR-0181); the workflow's own
    /// canonical-order and duplicate checks, plus the evidence digest, carry the
    /// semantic validation on the Actions path.
    #[must_use]
    pub fn from_mask(mask: &str) -> Option<Self> {
        let value = match *mask.as_bytes() {
            [hi, lo] => u16::from((hex_nibble(hi)? << 4) | hex_nibble(lo)?),
            [a, b, c, d] => {
                (u16::from(hex_nibble(a)?) << 12)
                    | (u16::from(hex_nibble(b)?) << 8)
                    | (u16::from(hex_nibble(c)?) << 4)
                    | u16::from(hex_nibble(d)?)
            }
            _ => return None,
        };
        Some(Self { mask: value, ..Self::EMPTY })
    }

    /// One identity name interned against this set's vocabulary: the compiled
    /// identity of that name, or the next declared position this set has not
    /// already spent.
    ///
    /// Positions are handed out in the order names arrive, which is the order a
    /// canonical row carries them — so a manifest that appends identities past
    /// the compiled vocabulary interns to the same positions it declared.
    fn intern(self, name: &str) -> Option<VerifyFailure> {
        if let Some(compiled) = VerifyFailure::from_name(name) {
            return Some(compiled);
        }
        let free = self.declared.iter().position(|declared| declared.is_empty())?;
        VerifyFailure::declared(VerifyFailure::first_declared_position() + u8::try_from(free).ok()?, name)
    }

    /// The identity this set carries at `position`, or `None` when the bit is
    /// clear or names nothing this set can spell.
    fn identity_at(self, position: u8) -> Option<VerifyFailure> {
        if position >= POSITION_COUNT || self.mask & (1u16 << position) == 0 {
            return None;
        }
        match VerifyFailure::ALL.get(usize::from(position)) {
            Some(compiled) => Some(*compiled),
            None => {
                let name = self.declared[Self::slot_of(position)?];
                (!name.is_empty()).then_some(VerifyFailure::Declared(DeclaredIdentity { position, name }))
            }
        }
    }

    /// This set's declared-name slot for `position`, or `None` when `position`
    /// belongs to the compiled vocabulary or lies past the bound.
    fn slot_of(position: u8) -> Option<usize> {
        let slot = usize::from(position).checked_sub(VerifyFailure::ALL.len())?;
        (slot < DECLARED_SLOTS).then_some(slot)
    }

    /// This set narrowed to `mask`, dropping the names of every bit the mask
    /// clears so an absent identity leaves nothing behind to compare.
    fn retaining(self, mask: u16) -> Self {
        let mut out = Self { mask, declared: self.declared };
        for (slot, name) in out.declared.iter_mut().enumerate() {
            if mask & (1u16 << (VerifyFailure::ALL.len() + slot)) == 0 {
                *name = IdentityName::EMPTY;
            }
        }
        out
    }
}

impl Default for VerifyFailureSet {
    fn default() -> Self {
        Self::EMPTY
    }
}

impl fmt::Debug for VerifyFailureSet {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_set().entries(self.iter()).finish()
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
        let identities = self.iter().collect::<Vec<_>>();
        let mut sequence = serializer.serialize_seq(Some(identities.len()))?;
        for failure in identities {
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
        while let Some(name) = sequence.next_element::<String>()? {
            let Some(failure) = set.intern(&name) else {
                return Err(A::Error::unknown_variant(&name, &VERIFY_FAILURE_NAMES));
            };
            if set.iter().any(|existing| existing.as_str() == name) {
                return Err(A::Error::custom(format!("duplicate verifier failure `{name}`")));
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
    use std::fs;
    use std::path::Path;

    use super::{MAX_VERIFIER_IDENTITY_BYTES, VerifyFailure, VerifyFailureSet};
    use crate::values::PipelineManifest;
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

        assert_eq!(
            failures.iter().map(|failure| failure.as_str().to_owned()).collect::<Vec<_>>(),
            MEMBERS_IN_CANONICAL_ORDER
        );
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
    fn serde_refuses_malformed_duplicate_and_out_of_order_values() {
        assert!(decode(&["VERIFY.FMT"]).is_err(), "an identity outside the charset is malformed");
        assert!(decode(&["clippy"]).is_err(), "an identity without the family prefix is malformed");
        assert!(decode(&["verify.fmt", "verify.fmt"]).is_err());
        assert!(decode(&["verify.docs", "verify.fmt"]).is_err());
    }

    #[test]
    fn a_declared_identity_past_the_compiled_vocabulary_decodes_and_re_encodes() {
        // Tripwire: decode is tolerant where intake is strict (ADR-0215). A row
        // written by a coordinator whose manifest declares an eleventh identity
        // must fold on a binary that compiles ten, or replay aborts at boot on
        // the journal the record exists to keep readable — and it must re-encode
        // as the row it was, since a decode that silently dropped the identity
        // would rewrite history rather than read it.
        let decoded = decode(&["verify.fmt", "verify.novel"]).expect("a declared identity decodes");
        let novel = VerifyFailure::declared(10, "verify.novel").expect("the eleventh position is declarable");

        assert!(decoded.contains(novel), "the identity survives the decode as its declared position");
        assert_eq!(novel.position(), 10, "an identity past the compiled ten takes the next position");
        assert_eq!(novel.compiled_position(), None, "and no position in the compiled table");
        assert_eq!(
            decoded.iter().map(|failure| failure.as_str().to_owned()).collect::<Vec<_>>(),
            ["verify.fmt", "verify.novel"],
            "the decoded set re-encodes as the names it was given",
        );
        assert_eq!(
            from_bytes::<VerifyFailureSet>(&to_vec(&decoded).expect("the set encodes")).expect("and decodes"),
            decoded,
        );

        // The bound is the manifest reader's, so a seventeenth position is not
        // declarable however well-formed its name, and neither is a name the
        // compiled vocabulary already spells at a position of its own.
        assert_eq!(VerifyFailure::declared(16, "verify.novel"), None);
        assert_eq!(VerifyFailure::declared(9, "verify.novel"), None);
        assert_eq!(VerifyFailure::declared(10, "verify.fmt"), None);
        assert_eq!(VerifyFailure::declared(10, "verify.a_name_far_past_the_cap"), None);
    }

    #[test]
    fn the_compiled_vocabulary_is_the_one_the_compiled_manifest_declares() {
        // Tripwire: ADR-0215's byte-neutrality claim rests on the compiled
        // identities keeping both their labels and their positions, because a
        // position is a bit in every stored mask and a label is what every
        // stored row literally says. A rename or a reorder here re-keys the
        // verification ledger and moves the persisted shape, which is the one
        // outcome the migration is not allowed to have.
        assert_eq!(
            VerifyFailure::ALL.iter().map(|failure| failure.as_str().to_owned()).collect::<Vec<_>>(),
            PipelineManifest::compiled().verifiers.identities,
        );
        for (position, identity) in VerifyFailure::ALL.into_iter().enumerate() {
            assert_eq!(usize::from(identity.position()), position, "{identity} moved off its bit");
            assert_eq!(identity.compiled_position(), Some(position));
            assert!(identity.as_str().len() <= MAX_VERIFIER_IDENTITY_BYTES, "{identity} outgrew the inline cap");
        }
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
        assert_eq!(failures.to_mask(), "0045");
        assert_eq!(VerifyFailureSet::from_mask("0045"), Some(failures));
        assert_eq!(VerifyFailureSet::from_mask("7f").map(VerifyFailureSet::to_mask).as_deref(), Some("007f"));

        // Tripwire: the whole vocabulary must still fit the four-hex-digit token
        // the attempt-artifact grammar reserves for it, at the exact value it
        // rendered before an identity became a declared id — the mask is the
        // artifact-name token and a moved bit renames every stored attempt.
        assert_eq!(VerifyFailure::ALL.into_iter().collect::<VerifyFailureSet>().to_mask(), "03ff");
        assert_eq!(VerifyFailureSet::one(VerifyFailure::Suppress).to_mask(), "0080");
        assert_eq!(VerifyFailureSet::from_mask("80"), Some(VerifyFailureSet::one(VerifyFailure::Suppress)));
        assert_eq!(VerifyFailureSet::from_mask("0080"), Some(VerifyFailureSet::one(VerifyFailure::Suppress)));
        assert_eq!(
            VerifyFailureSet::from_mask("03ff"),
            Some(VerifyFailure::ALL.into_iter().collect::<VerifyFailureSet>())
        );

        // Tripwire: a legacy two-digit token zero-extends to the same set as
        // its four-digit form, so already-journaled and already-named masks
        // keep their meaning after the token widens (ADR-0209).
        let eight: VerifyFailureSet = [
            VerifyFailure::Preflight,
            VerifyFailure::Fmt,
            VerifyFailure::Clippy,
            VerifyFailure::Docs,
            VerifyFailure::Test,
            VerifyFailure::Dup,
            VerifyFailure::Deps,
            VerifyFailure::Suppress,
        ]
        .into_iter()
        .collect();
        assert_eq!(VerifyFailureSet::from_mask("ff"), VerifyFailureSet::from_mask("00ff"));
        assert_eq!(VerifyFailureSet::from_mask("00ff"), Some(eight));
        assert_eq!(VerifyFailureSet::from_mask("45"), Some(failures));

        for invalid in ["0", "000", "00000", "0A", "GG", "g0", "-1"] {
            assert!(VerifyFailureSet::from_mask(invalid).is_none(), "`{invalid}` must be refused");
        }
    }

    #[test]
    fn the_wrapper_renders_a_mask_token_width_the_decoder_accepts() {
        let workflow =
            fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.github/workflows/transform.yml"))
                .expect("the wrapper workflow is checked in");
        let width: usize = workflow
            .split_once("printf '%")
            .and_then(|(_, rest)| rest.split_once("x'"))
            .and_then(|(digits, _)| digits.parse().ok())
            .expect("the wrapper renders the mask through one zero-padded hex printf");
        let all: VerifyFailureSet = VerifyFailure::ALL.into_iter().collect();

        // Tripwire: the wrapper's rendered token must be a width `from_mask`
        // accepts for every set the lane can emit, and the full vocabulary is
        // the value that drifts first. Zero-padding is a minimum, so a
        // too-narrow format renders `0x3ff` as the three characters the
        // decoder refuses — the artifact name then buys no upload and the
        // failing lane's evidence is dropped silently (#5798).
        assert_eq!(VerifyFailureSet::from_mask(&format!("{:0width$x}", all.mask)), Some(all));
    }

    const MEMBERS_IN_CANONICAL_ORDER: [&str; 3] = ["verify.preflight", "verify.fmt", "verify.deps"];
}
