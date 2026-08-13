//! The sealed configuration registry and the resolution that reads it back
//! (ADR-0174).
//!
//! A bloom's configuration is a table of kind name to [`Digest`](crate::digest::Digest)
//! resolved *by type* at the point of use, rather than a field per configuration
//! on the sealed value types. [`registry`] is the sealed half — addressing, the
//! per-kind table, and the scope chain a lookup walks. [`resolve`] is the read
//! half: the content a caller fetched, and the typed lookup over it.
//!
//! The two halves are deliberately separate. Sealing an address is something the
//! reducer does on its own; producing the content behind one needs a store, which
//! the reducer does not have. So resolution takes the content as an argument
//! rather than going to find it, and whoever calls the reducer is responsible for
//! having it in hand.

#[cfg(test)]
mod encoder_equivalence;
mod registry;
mod resolve;

pub use registry::{ConfigKind, ConfigRegistry, ConfigScopes, config_address};
pub use resolve::{ConfigResolveError, ResolvedConfigs, Unproducible, decode_config};
