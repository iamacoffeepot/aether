//! The sealed half of the configuration registry: addressing, the per-kind
//! table, and the scope chain a lookup walks (ADR-0174).
//!
//! A bloom's configuration is a table of kind name to [`Digest`] resolved *by
//! type* at the point of use, rather than a field per configuration on the
//! sealed value types. One entry per kind is what makes the lookup typed:
//! [`ConfigRegistry::address`] takes no key argument because the kind's
//! [`Kind::NAME`](aether_data::Kind::NAME) is the key. A caller wanting two
//! configurations of the same shape declares a newtype kind for the second, so
//! the registry never needs a name-plus-type composite key.
//!
//! The key is the kind *name* rather than its [`KindId`](aether_data::KindId),
//! and the reason is durability. A `KindId` folds the kind's schema into its
//! hash, so adding a field to a configuration kind moves its id — which would
//! orphan that entry in every bloom already sealed, turning benign schema
//! evolution into a fleet of unresolvable historical records. A name survives
//! its type growing a field, which is what a key inside an immutable record
//! has to do. The name also makes sealed bytes legible to anyone reading them
//! without the binaries that produced them.
//!
//! Nothing here resolves content — an address is fetched through
//! [`ResolvedConfigs`](super::ResolvedConfigs), which takes the bytes as an
//! argument rather than going to find them. The split is about *fetching*, not
//! decoding: decoding a config kind is a plain
//! [`from_bytes`](aether_data::wire::from_bytes) against a type this crate
//! already defines, which `no_std` does perfectly well. What this crate cannot
//! do is reach a store.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::iter::once;

use aether_data::wire::to_vec;
use serde::{Deserialize, Serialize};

use crate::digest::Digest;

/// A kind that can be sealed into a [`ConfigRegistry`].
///
/// Blanket-implemented for every [`Kind`](aether_data::Kind) that serializes,
/// so declaring a configuration is declaring a kind and nothing further. The
/// blanket impl is also what keeps the typed and generic authoring paths in
/// agreement: [`address`](ConfigKind::address) domain-separates on
/// [`Kind::NAME`](aether_data::Kind::NAME), which is the one string the generic
/// `POST /configs` route has in hand, so a value addressed from Rust and the
/// same value addressed from JSON land on the same digest by construction
/// rather than by a convention someone has to hold.
pub trait ConfigKind: aether_data::Kind + Serialize {
    /// The address this value seals under.
    ///
    /// # Panics
    ///
    /// Panics if the value fails to wire-encode, i.e. some length exceeds the
    /// ADR-0118 `u32` ceiling. This matches [`digest_of`](crate::digest::digest_of)
    /// and cannot happen for a configuration value; an occurrence is a broken
    /// invariant, deliberately loud rather than a silently colliding address.
    #[must_use]
    fn address(&self) -> Digest {
        let bytes = to_vec(self).expect("configuration values never exceed the ADR-0118 u32 wire-length ceiling");
        config_address(Self::NAME, &bytes)
    }
}

impl<K: aether_data::Kind + Serialize> ConfigKind for K {}

/// The address an already-encoded configuration seals under, domain-separated
/// by its kind name.
///
/// The generic authoring route reaches for this: it holds a kind *name* and
/// canonical bytes, never the Rust type, so it cannot go through
/// [`ConfigKind::address`]. Both compute the same hash over the same inputs —
/// the length-prefixed domain followed by the value's canonical wire bytes,
/// matching [`digest_of`](crate::digest::digest_of)'s construction — so the two
/// paths address a given value identically.
///
/// # Panics
///
/// Panics if `kind` is longer than `u32::MAX`, which no kind name is.
#[must_use]
pub fn config_address(kind: &str, bytes: &[u8]) -> Digest {
    use sha2::{Digest as _, Sha256};

    let domain_len =
        u32::try_from(kind.len()).expect("a kind name is a short static string, well under the u32 ceiling");
    let mut hasher = Sha256::new();
    hasher.update(domain_len.to_le_bytes());
    hasher.update(kind.as_bytes());
    hasher.update(bytes);
    Digest::from_bytes(hasher.finalize().into())
}

/// A sealed set of configuration addresses, at most one per kind.
///
/// Ordered by construction — the `BTreeMap` gives the canonical order a sealed
/// value needs for free, so two registries built by different insertion orders
/// encode identically and seal to the same bloom id.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Default, Serialize, Deserialize)]
pub struct ConfigRegistry {
    entries: BTreeMap<String, Digest>,
}

impl ConfigRegistry {
    /// Seal `address` as this registry's entry for `K`, returning the address
    /// it displaced.
    pub fn insert<K: ConfigKind>(&mut self, address: Digest) -> Option<Digest> {
        self.entries.insert(String::from(K::NAME), address)
    }

    /// Seal `address` under a kind named at runtime — the generic authoring
    /// route's entry point, where the Rust type is not in hand.
    pub fn insert_named(&mut self, kind: &str, address: Digest) -> Option<Digest> {
        self.entries.insert(String::from(kind), address)
    }

    /// The address this registry seals for `K`, if any.
    ///
    /// No key argument: `K::NAME` is the key, which is the whole point of
    /// holding at most one entry per kind.
    #[must_use]
    pub fn address<K: ConfigKind>(&self) -> Option<Digest> {
        self.address_of(K::NAME)
    }

    /// The address sealed for a kind named at runtime — the host's path when it
    /// walks a registry it did not resolve by type.
    #[must_use]
    pub fn address_of(&self, kind: &str) -> Option<Digest> {
        self.entries.get(kind).copied()
    }

    /// Every sealed entry, in canonical key order.
    pub fn entries(&self) -> impl Iterator<Item = (&str, Digest)> + '_ {
        self.entries.iter().map(|(kind, address)| (kind.as_str(), *address))
    }

    /// The effective registry a member runs under: `self` layered over `outer`,
    /// with `self`'s entry winning per kind.
    ///
    /// The flattened form of the [`ConfigScopes`] walk, for the one place that
    /// cannot walk it — the dispatch payload the reducer hands the host names a
    /// single member, so carrying the chain would carry the whole bloom's
    /// registry to say what one layered lookup already says.
    #[must_use]
    pub fn layered_over(&self, outer: &Self) -> Self {
        let mut effective = outer.clone();
        effective.entries.extend(self.entries.iter().map(|(kind, address)| (kind.clone(), *address)));
        effective
    }

    /// Whether anything is sealed here.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// How many kinds are sealed.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

impl<K: Into<String>> FromIterator<(K, Digest)> for ConfigRegistry {
    fn from_iter<I: IntoIterator<Item = (K, Digest)>>(iter: I) -> Self {
        Self { entries: iter.into_iter().map(|(kind, address)| (kind.into(), address)).collect() }
    }
}

/// The scope chain a configuration lookup walks: the member's registry first,
/// then the bloom's.
///
/// Layered rather than nested (ADR-0174). Nesting a per-member table inside the
/// bloom's registry would express the same fall-through while making the sealed
/// value types know about the scope hierarchy, so adding a level would re-digest
/// every spec; holding the hierarchy here costs nothing at seal.
#[derive(Clone, Copy, Debug)]
pub struct ConfigScopes<'a> {
    /// The member being dispatched, when the lookup is on a member's behalf.
    /// `None` for a bloom-wide lookup with no member in scope.
    pub member: Option<&'a ConfigRegistry>,
    /// The bloom the member belongs to.
    pub bloom: &'a ConfigRegistry,
}

impl<'a> ConfigScopes<'a> {
    /// A lookup on one member's behalf.
    #[must_use]
    pub const fn member_of(member: &'a ConfigRegistry, bloom: &'a ConfigRegistry) -> Self {
        Self { member: Some(member), bloom }
    }

    /// A bloom-wide lookup, with no member in scope.
    #[must_use]
    pub const fn bloom_wide(bloom: &'a ConfigRegistry) -> Self {
        Self { member: None, bloom }
    }

    /// The address `K` resolves to, taking the innermost scope that seals one.
    ///
    /// A `None` here means no scope sealed a `K`, which resolves to the
    /// caller's default. It does not mean the configuration is missing — an
    /// address that *is* sealed and cannot be fetched is a loud failure at the
    /// host, never a fall-through, because defaulting past a sealed entry would
    /// attest a configuration that never applied.
    #[must_use]
    pub fn address<K: ConfigKind>(&self) -> Option<Digest> {
        self.member.and_then(ConfigRegistry::address::<K>).or_else(|| self.bloom.address::<K>())
    }

    /// The scope chain's registries, innermost first — the order a host walks
    /// when it resolves a kind named at runtime rather than by type.
    #[must_use]
    pub fn chain(&self) -> Vec<&'a ConfigRegistry> {
        self.member.into_iter().chain(once(self.bloom)).collect()
    }
}

#[cfg(test)]
mod tests {
    use aether_data::Kind;
    use serde::Deserialize;

    use super::*;

    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[kind(name = "aether.bloomery.test_alpha")]
    struct Alpha {
        setting: u32,
    }

    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[kind(name = "aether.bloomery.test_beta")]
    struct Beta {
        setting: u32,
    }

    // Tripwire: the address is domain-separated by kind name. Alpha and Beta
    // encode to identical wire bytes, so a collision here means the generic
    // route could store one kind's content at another kind's address — the
    // registry key would no longer determine what the digest names.
    #[test]
    fn identical_bytes_under_different_kinds_address_differently() {
        assert_ne!(Alpha { setting: 7 }.address(), Beta { setting: 7 }.address());
    }

    // Tripwire: the typed path and the byte path agree. `POST /configs` holds a
    // kind name and canonical bytes, never the Rust type, so if these two ever
    // diverge a config authored over REST would seal at an address no typed
    // lookup can reach.
    #[test]
    fn the_typed_and_encoded_addresses_agree() {
        let alpha = Alpha { setting: 7 };
        let bytes = to_vec(&alpha).expect("test value encodes");
        assert_eq!(alpha.address(), config_address(Alpha::NAME, &bytes));
    }

    // A registry is keyed by kind, so one kind's entry replaces its own and
    // leaves a different kind's standing.
    #[test]
    fn one_entry_per_kind() {
        let mut registry = ConfigRegistry::default();
        let first = Alpha { setting: 1 }.address();
        let second = Alpha { setting: 2 }.address();

        assert_eq!(registry.insert::<Alpha>(first), None);
        assert_eq!(registry.insert::<Beta>(Beta { setting: 1 }.address()), None);
        assert_eq!(registry.insert::<Alpha>(second), Some(first), "the same kind replaces its own entry");

        assert_eq!(registry.address::<Alpha>(), Some(second));
        assert_eq!(registry.len(), 2, "a second kind is its own entry, not a replacement");
    }

    // Insertion order cannot reach the sealed bytes: the registry is ordered by
    // key, so two registries built in opposite orders encode identically. This
    // is what lets a bloom id stay a function of what was sealed rather than of
    // how it was assembled.
    #[test]
    fn insertion_order_does_not_reach_the_sealed_bytes() {
        let (alpha, beta) = (Alpha { setting: 1 }.address(), Beta { setting: 2 }.address());

        let mut forward = ConfigRegistry::default();
        forward.insert::<Alpha>(alpha);
        forward.insert::<Beta>(beta);

        let mut backward = ConfigRegistry::default();
        backward.insert::<Beta>(beta);
        backward.insert::<Alpha>(alpha);

        assert_eq!(to_vec(&forward).expect("registry encodes"), to_vec(&backward).expect("registry encodes"));
    }

    // The scope chain takes the innermost registry that seals the kind, and
    // falls through per kind rather than per registry — a member sealing one
    // kind does not shadow the bloom's entries for every other kind.
    #[test]
    fn the_member_scope_wins_per_kind_not_wholesale() {
        let (member_alpha, bloom_alpha) = (Alpha { setting: 1 }.address(), Alpha { setting: 2 }.address());
        let bloom_beta = Beta { setting: 3 }.address();

        let mut member = ConfigRegistry::default();
        member.insert::<Alpha>(member_alpha);

        let mut bloom = ConfigRegistry::default();
        bloom.insert::<Alpha>(bloom_alpha);
        bloom.insert::<Beta>(bloom_beta);

        let scopes = ConfigScopes::member_of(&member, &bloom);
        assert_eq!(scopes.address::<Alpha>(), Some(member_alpha), "the member's entry wins");
        assert_eq!(scopes.address::<Beta>(), Some(bloom_beta), "the bloom's entry still resolves");

        let wide = ConfigScopes::bloom_wide(&bloom);
        assert_eq!(wide.address::<Alpha>(), Some(bloom_alpha), "with no member in scope the bloom's entry stands");
    }

    // Nothing sealed anywhere resolves to `None`, which the caller reads as
    // "take the default". This is distinct from a sealed address that cannot be
    // fetched, which the host must refuse rather than default past.
    #[test]
    fn an_unsealed_kind_resolves_to_nothing() {
        let bloom = ConfigRegistry::default();
        assert_eq!(ConfigScopes::bloom_wide(&bloom).address::<Alpha>(), None);
    }
}
