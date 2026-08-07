//! Resolving a sealed configuration back to its content (ADR-0174).
//!
//! The reducer seals a [`ConfigRegistry`] of addresses and resolves nothing; the
//! host walks the scope chain, fetches the address the innermost scope sealed,
//! and decodes it. This is that walk.
//!
//! The distinction the error type carries is the load-bearing one. A kind *no*
//! scope sealed is [`None`] — absence is a valid state and the caller takes its
//! default. A kind some scope *did* seal but whose content cannot be produced is
//! an error, never a fall-through: defaulting past a sealed entry would run one
//! configuration while the receipt attests another, which is the divergence the
//! registry exists to close.

use std::error::Error;
use std::fmt;

use aether_bloomery::{ConfigKind, ConfigScopes};
use aether_data::wire::from_bytes;
use serde::de::DeserializeOwned;

use super::StoreBackend;

/// Why a sealed configuration could not be produced.
#[derive(Debug)]
pub enum ConfigResolveError {
    /// The registry sealed an address with no stored content. Either the
    /// authoring write never landed or the row was lost — both mean the bloom
    /// cannot run the configuration it attests.
    Missing {
        /// The kind the registry key named.
        kind: &'static str,
    },
    /// The stored row is a different kind than the registry key claims. The
    /// address is domain-separated by kind name, so reaching this means some
    /// path wrote a row without computing the address that way.
    KindMismatch {
        /// The kind the registry key named.
        expected: &'static str,
        /// The kind the stored row declares.
        stored: String,
    },
    /// The stored bytes do not decode as the kind they are filed under.
    Decode {
        /// The kind the registry key named.
        kind: &'static str,
    },
    /// The store itself failed.
    Store(rusqlite::Error),
}

impl fmt::Display for ConfigResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing { kind } => write!(f, "sealed config `{kind}` has no stored content"),
            Self::KindMismatch { expected, stored } => {
                write!(f, "sealed config `{expected}` is stored as `{stored}`")
            }
            Self::Decode { kind } => write!(f, "sealed config `{kind}` does not decode as its kind"),
            Self::Store(error) => write!(f, "config store failed: {error}"),
        }
    }
}

impl Error for ConfigResolveError {}

impl From<rusqlite::Error> for ConfigResolveError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Store(error)
    }
}

/// The configuration of kind `K` the innermost scope seals, or `None` when no
/// scope seals one.
///
/// # Errors
///
/// Returns [`ConfigResolveError`] when a scope *did* seal a `K` whose content
/// cannot be produced — a missing row, a row filed under another kind, or bytes
/// that do not decode. Each is a refusal rather than a fall-through to the
/// caller's default.
pub fn resolve_config<K: ConfigKind + DeserializeOwned>(
    store: &mut dyn StoreBackend,
    scopes: ConfigScopes<'_>,
) -> Result<Option<K>, ConfigResolveError> {
    let Some(address) = scopes.address::<K>() else {
        return Ok(None);
    };

    let Some((stored_kind, bytes)) = store.lookup_config(address.as_bytes())? else {
        return Err(ConfigResolveError::Missing { kind: K::NAME });
    };
    if stored_kind != K::NAME {
        return Err(ConfigResolveError::KindMismatch { expected: K::NAME, stored: stored_kind });
    }

    from_bytes::<K>(&bytes).map(Some).map_err(|_| ConfigResolveError::Decode { kind: K::NAME })
}
