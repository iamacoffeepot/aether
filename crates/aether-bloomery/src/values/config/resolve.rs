//! The read half of the configuration registry: producing the value behind a
//! sealed address (ADR-0174).
//!
//! [`ConfigRegistry`] seals addresses; this resolves one back to content. The
//! content arrives as an argument — a [`ResolvedConfigs`] the caller filled from
//! wherever it keeps configuration bytes — because reaching a store is the one
//! thing this crate cannot do. Decoding is not: a config kind is a type declared
//! here, so [`from_bytes`] is all a resolution needs once the bytes are in hand.
//!
//! The distinction the error carries is the load-bearing one, and it is the same
//! distinction on both sides of the crate boundary. A kind *no* scope sealed is
//! [`None`] — absence is a valid state and the caller takes its default. A kind
//! some scope *did* seal but whose content cannot be produced is an error, never
//! a fall-through: defaulting past a sealed entry would run one configuration
//! while the receipt attests another, which is the divergence the registry exists
//! to close.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::error::Error;
use core::fmt;

use aether_data::wire::from_bytes;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::{ConfigKind, ConfigRegistry, ConfigScopes};
use crate::digest::Digest;

/// Why a sealed configuration could not be produced.
///
/// Every variant means the same thing to a caller: the bloom attests a
/// configuration it cannot run. They differ only in where the trail goes cold,
/// which is what a diagnosing operator needs. A host that reaches a store adds
/// its own failure on top of these rather than folding into them, so "the store
/// broke" stays distinguishable from "the content is not there".
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ConfigResolveError {
    /// The registry sealed an address with no available content. Either the
    /// authoring write never landed, the row was lost, or the caller did not
    /// fetch it before resolving.
    Missing {
        /// The kind the registry key named.
        kind: &'static str,
    },
    /// The available content is a different kind than the registry key claims.
    /// The address is domain-separated by kind name, so reaching this means some
    /// path produced content without computing the address that way.
    KindMismatch {
        /// The kind the registry key named.
        expected: &'static str,
        /// The kind the stored content declares.
        stored: String,
    },
    /// The content does not decode as the kind it is filed under.
    Decode {
        /// The kind the registry key named.
        kind: &'static str,
    },
}

impl fmt::Display for ConfigResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing { kind } => write!(f, "sealed config `{kind}` has no available content"),
            Self::KindMismatch { expected, stored } => write!(f, "sealed config `{expected}` is stored as `{stored}`"),
            Self::Decode { kind } => write!(f, "sealed config `{kind}` does not decode as its kind"),
        }
    }
}

impl Error for ConfigResolveError {}

/// Check that stored content is filed under `K`'s kind and decode it.
///
/// The one place the kind-check and the decode happen, shared by every caller
/// that produces a configuration — the reducer resolving through
/// [`ResolvedConfigs`], and a host resolving straight out of its own store.
/// Sharing it is what keeps the two from drifting into different answers about
/// what counts as resolvable.
///
/// # Errors
///
/// [`ConfigResolveError::KindMismatch`] when `stored_kind` is not `K::NAME`, and
/// [`ConfigResolveError::Decode`] when the bytes do not decode as `K`.
pub fn decode_config<K: ConfigKind + DeserializeOwned>(
    stored_kind: &str,
    bytes: &[u8],
) -> Result<K, ConfigResolveError> {
    if stored_kind != K::NAME {
        return Err(ConfigResolveError::KindMismatch { expected: K::NAME, stored: String::from(stored_kind) });
    }
    from_bytes::<K>(bytes).map_err(|_| ConfigResolveError::Decode { kind: K::NAME })
}

/// Configuration content, by the address it seals under.
///
/// The reducer's window onto configuration it did not fetch. A caller fills this
/// from its own store before reducing, and the reducer resolves through it as if
/// the content had been in the sealed bytes all along — which is what lets a
/// value the reducer must read live in the registry rather than needing a field
/// of its own on every sealed type.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct ResolvedConfigs {
    entries: BTreeMap<Digest, StoredConfig>,
}

/// One address's content: the kind it was filed under and its canonical bytes.
#[derive(Clone, PartialEq, Eq, Debug)]
struct StoredConfig {
    kind: String,
    bytes: Vec<u8>,
}

/// Why a sealed registry entry's content cannot be produced from a
/// [`ResolvedConfigs`].
///
/// The distinction is whether fetching would help. Absent content is a caller
/// that has not looked yet; the other two are already wrong at rest and will be
/// just as wrong after another fetch.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Unproducible {
    /// No content is available at that address.
    Absent,
    /// Content is available but declares a different kind than the registry key.
    MisfiledAs(String),
    /// Content is available and correctly filed, but does not decode as its
    /// kind. Only a typed resolution can reach this: the name-keyed walk
    /// [`unproducible_in`](ResolvedConfigs::unproducible_in) holds no Rust type
    /// to decode against, so a caller that must read the value reports it and a
    /// caller merely filling the set never sees it.
    Undecodable,
}

impl ResolvedConfigs {
    /// Make `address`'s content available, returning whether it displaced an
    /// entry.
    ///
    /// An address is a hash of the kind name and the bytes, so re-inserting one
    /// writes content equal to what was there — the return value reports a
    /// redundant fetch, never a conflict.
    pub fn insert(&mut self, address: Digest, kind: impl Into<String>, bytes: Vec<u8>) -> bool {
        self.entries.insert(address, StoredConfig { kind: kind.into(), bytes }).is_some()
    }

    /// Whether `address`'s content is available.
    #[must_use]
    pub fn contains(&self, address: Digest) -> bool {
        self.entries.contains_key(&address)
    }

    /// Every entry of `registry` whose content this set cannot produce, and why.
    ///
    /// Two consumers, one question. A caller filling the set fetches the
    /// [`Unproducible::Absent`] entries; a caller checking a registry is
    /// resolvable refuses on the first entry of any kind. Keeping the reason as
    /// data rather than splitting the walk is what stops a fetcher from looping
    /// on a mis-filed entry it can never fix by fetching again.
    pub fn unproducible_in<'a>(
        &'a self,
        registry: &'a ConfigRegistry,
    ) -> impl Iterator<Item = (&'a str, Digest, Unproducible)> + 'a {
        registry.entries().filter_map(move |(kind, address)| match self.entries.get(&address) {
            None => Some((kind, address, Unproducible::Absent)),
            Some(stored) if stored.kind != kind => Some((kind, address, Unproducible::MisfiledAs(stored.kind.clone()))),
            Some(_) => None,
        })
    }

    /// The configuration of kind `K` the innermost scope seals, or `None` when no
    /// scope seals one.
    ///
    /// # Errors
    ///
    /// [`ConfigResolveError`] when a scope *did* seal a `K` whose content cannot
    /// be produced — unavailable here, filed under another kind, or bytes that do
    /// not decode. Each is a refusal rather than a fall-through to the caller's
    /// default.
    pub fn resolve<K: ConfigKind + DeserializeOwned>(
        &self,
        scopes: ConfigScopes<'_>,
    ) -> Result<Option<K>, ConfigResolveError> {
        let Some(address) = scopes.address::<K>() else {
            return Ok(None);
        };
        let Some(stored) = self.entries.get(&address) else {
            return Err(ConfigResolveError::Missing { kind: K::NAME });
        };
        decode_config::<K>(&stored.kind, &stored.bytes).map(Some)
    }

    /// How many addresses have content available.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether any content is available.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use aether_data::Kind;
    use aether_data::wire::to_vec;
    use serde::{Deserialize, Serialize};

    use super::*;

    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[kind(name = "aether.bloomery.test_resolve_alpha")]
    struct Alpha {
        setting: u32,
    }

    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[kind(name = "aether.bloomery.test_resolve_beta")]
    struct Beta {
        other: String,
    }

    /// A `ResolvedConfigs` holding `value` at its own address, filed correctly.
    fn available<K: ConfigKind>(value: &K) -> ResolvedConfigs {
        let mut configs = ResolvedConfigs::default();
        configs.insert(value.address(), K::NAME, to_vec(value).expect("test value encodes"));
        configs
    }

    /// A bloom-wide registry sealing `value`'s address under its own kind.
    fn sealing<K: ConfigKind>(value: &K) -> ConfigRegistry {
        let mut registry = ConfigRegistry::default();
        registry.insert::<K>(value.address());
        registry
    }

    // A sealed kind whose content is available resolves to that content, through
    // the same scope chain a host walks.
    #[test]
    fn a_sealed_kind_with_available_content_resolves() {
        let alpha = Alpha { setting: 7 };
        let (registry, configs) = (sealing(&alpha), available(&alpha));

        assert_eq!(configs.resolve::<Alpha>(ConfigScopes::bloom_wide(&registry)), Ok(Some(alpha)));
    }

    // Tripwire: unsealed and sealed-but-unproducible are different answers
    // (ADR-0174). Collapsing them would let a bloom whose configuration cannot be
    // produced run the caller's default while its receipt attests the override —
    // the exact divergence the registry closes. The `Ok(None)` side is what makes
    // a bloom that configures nothing keep working, so neither can absorb the
    // other.
    #[test]
    fn unsealed_resolves_to_nothing_but_unavailable_content_refuses() {
        let empty = ConfigRegistry::default();
        assert_eq!(
            ResolvedConfigs::default().resolve::<Alpha>(ConfigScopes::bloom_wide(&empty)),
            Ok(None),
            "nothing sealed is the caller's default, not a failure"
        );

        let sealed = sealing(&Alpha { setting: 7 });
        assert_eq!(
            ResolvedConfigs::default().resolve::<Alpha>(ConfigScopes::bloom_wide(&sealed)),
            Err(ConfigResolveError::Missing { kind: Alpha::NAME }),
            "a sealed address with no content refuses rather than defaulting"
        );
    }

    // Tripwire: content filed under the wrong kind is refused, not decoded. Alpha
    // and Beta have incompatible shapes here, but a same-shaped pair would decode
    // clean and silently resolve one kind's value as another's — so the kind check
    // has to run before the decode rather than relying on it to fail.
    #[test]
    fn content_filed_under_another_kind_is_refused() {
        let alpha = Alpha { setting: 7 };
        let mut configs = ResolvedConfigs::default();
        configs.insert(alpha.address(), Beta::NAME, to_vec(&alpha).expect("test value encodes"));

        assert_eq!(
            configs.resolve::<Alpha>(ConfigScopes::bloom_wide(&sealing(&alpha))),
            Err(ConfigResolveError::KindMismatch { expected: Alpha::NAME, stored: String::from(Beta::NAME) })
        );
    }

    // Tripwire: bytes that do not decode as their kind are refused. Reached when
    // stored content predates a breaking change to the kind's shape — the registry
    // keys on the kind *name*, which deliberately survives schema evolution, so
    // the decode is the only place a stale value is caught.
    #[test]
    fn content_that_does_not_decode_is_refused() {
        let alpha = Alpha { setting: 7 };
        let mut configs = ResolvedConfigs::default();
        configs.insert(alpha.address(), Alpha::NAME, alloc::vec![0xff]);

        assert_eq!(
            configs.resolve::<Alpha>(ConfigScopes::bloom_wide(&sealing(&alpha))),
            Err(ConfigResolveError::Decode { kind: Alpha::NAME })
        );
    }

    // What a caller fetches before reducing: the sealed addresses it does not
    // already hold, and nothing more. A caller that re-fetched everything would
    // turn every admit into a store round trip; one that reported nothing
    // unproducible would resolve against content it never had.
    #[test]
    fn unproducible_names_only_the_entries_that_cannot_resolve() {
        let (alpha, beta) = (Alpha { setting: 7 }, Beta { other: String::from("x") });
        let mut registry = sealing(&alpha);
        registry.insert::<Beta>(beta.address());

        assert_eq!(
            available(&alpha).unproducible_in(&registry).collect::<Vec<_>>(),
            alloc::vec![(Beta::NAME, beta.address(), Unproducible::Absent)],
            "the held address is not re-fetched and the unheld one is named"
        );
        assert_eq!(available(&beta).unproducible_in(&ConfigRegistry::default()).count(), 0);
    }

    // Tripwire: mis-filed content is unproducible for a reason a fetch cannot
    // fix, and says so. A walk that reported it as merely absent would send a
    // filling caller back to the store for an address whose content is already
    // in hand and already wrong — the same fetch, the same result, forever.
    #[test]
    fn misfiled_content_is_distinguished_from_absent() {
        let alpha = Alpha { setting: 7 };
        let mut configs = ResolvedConfigs::default();
        configs.insert(alpha.address(), Beta::NAME, to_vec(&alpha).expect("test value encodes"));

        assert_eq!(
            configs.unproducible_in(&sealing(&alpha)).collect::<Vec<_>>(),
            alloc::vec![(Alpha::NAME, alpha.address(), Unproducible::MisfiledAs(String::from(Beta::NAME)))]
        );
    }

    // The member scope wins per kind here exactly as it does for addressing, so
    // resolution cannot disagree with `ConfigScopes::address` about which entry a
    // lookup takes.
    #[test]
    fn resolution_takes_the_innermost_scope() {
        let (outer, inner) = (Alpha { setting: 1 }, Alpha { setting: 2 });
        let (bloom, member) = (sealing(&outer), sealing(&inner));

        let mut configs = available(&outer);
        configs.insert(inner.address(), Alpha::NAME, to_vec(&inner).expect("test value encodes"));

        assert_eq!(configs.resolve::<Alpha>(ConfigScopes::member_of(&member, &bloom)), Ok(Some(inner)));
        assert_eq!(configs.resolve::<Alpha>(ConfigScopes::bloom_wide(&bloom)), Ok(Some(outer)));
    }
}
