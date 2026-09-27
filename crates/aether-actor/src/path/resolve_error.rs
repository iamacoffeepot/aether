//! [`ResolveError`]: why `resolve` could not prove a typed path.

use core::error::Error;
use core::fmt::{self, Display, Formatter};

use aether_data::{ErasedActorPath, KindId};

/// Why `resolve` could not prove a typed path (ADR-0231 §3).
///
/// Every refusal names the path, never a position: the holder asked with a
/// path, and the path is what it can act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// No `Live` route stands at the path: it was never registered, its birth
    /// is still `Starting`, it was `Dropped`, or the route at the path's fold
    /// carries a different canonical name.
    NotLive {
        /// The path that was resolved.
        path: ErasedActorPath,
    },
    /// The route at the path is `Live`, but its published rows do not cover
    /// the protocol: `kind` is the first of the protocol's kinds whose row is
    /// missing or replies differently.
    Uncovered {
        /// The path that was resolved.
        path: ErasedActorPath,
        /// The first protocol kind the route's rows do not cover.
        kind: KindId,
    },
}

impl Display for ResolveError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotLive { path } => write!(f, "no live actor stands at {path}"),
            Self::Uncovered { path, kind } => {
                write!(f, "the actor at {path} does not cover the protocol's row for kind {kind}")
            }
        }
    }
}

impl Error for ResolveError {}
