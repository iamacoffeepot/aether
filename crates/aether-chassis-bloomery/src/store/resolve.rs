//! Resolving a sealed configuration back to its content out of the store
//! (ADR-0174).
//!
//! The scope-chain walk and the decode belong to `aether-bloomery` — the reducer
//! resolves the same way against content handed to it, and one semantic with two
//! callers is what stops the two sides drifting on what counts as resolvable.
//! What lives here is the half that crate cannot do: reaching a store for the
//! bytes an address names.
//!
//! So the error is `aether-bloomery`'s, wrapped with the one failure a store
//! adds. "The store broke" is a transient the caller retries; every
//! [`ConfigResolveError`] under it is a permanent statement about the content,
//! and conflating them would turn a lost connection into a bloom refused for
//! attesting a configuration that is, in fact, right where it should be.

use std::error::Error;
use std::fmt;

use aether_bloomery::{ConfigKind, ConfigResolveError, ConfigScopes, decode_config};
use aether_data::Schema;
use serde::de::DeserializeOwned;

use super::StoreBackend;

/// Why a sealed configuration could not be produced from the store.
#[derive(Debug)]
pub enum StoreConfigError {
    /// The content itself is unproducible — absent, mis-filed, or undecodable.
    /// Permanent: the address is immutable, so a retry resolves identically.
    Content(ConfigResolveError),
    /// The store failed. Transient, and the only variant a caller should retry.
    Store(rusqlite::Error),
}

impl fmt::Display for StoreConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Content(error) => error.fmt(f),
            Self::Store(error) => write!(f, "config store failed: {error}"),
        }
    }
}

impl Error for StoreConfigError {}

impl From<rusqlite::Error> for StoreConfigError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Store(error)
    }
}

impl From<ConfigResolveError> for StoreConfigError {
    fn from(error: ConfigResolveError) -> Self {
        Self::Content(error)
    }
}

/// The configuration of kind `K` the innermost scope seals, or `None` when no
/// scope seals one.
///
/// # Errors
///
/// [`StoreConfigError::Content`] when a scope *did* seal a `K` whose content
/// cannot be produced — a missing row, a row filed under another kind, or bytes
/// that do not decode. Each is a refusal rather than a fall-through to the
/// caller's default. [`StoreConfigError::Store`] when the store itself failed,
/// which says nothing about the content.
pub fn resolve_config<K: ConfigKind + DeserializeOwned + Schema>(
    store: &mut dyn StoreBackend,
    scopes: ConfigScopes<'_>,
) -> Result<Option<K>, StoreConfigError> {
    let Some(address) = scopes.address::<K>() else {
        return Ok(None);
    };
    let Some((stored_kind, bytes, schema_digest)) = store.lookup_config(address.as_bytes())? else {
        return Err(ConfigResolveError::Missing { kind: K::NAME }.into());
    };

    Ok(Some(decode_config::<K>(&stored_kind, &bytes, schema_digest.as_deref())?))
}
