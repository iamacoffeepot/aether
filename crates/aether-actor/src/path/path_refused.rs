//! [`PathRefused`]: the answer a request gets when a typed path it carries
//! does not prove, at decode or at `resolve` (ADR-0231 §3).

use core::fmt::{self, Display, Formatter};

use aether_data::wire::Error as WireError;
use aether_data::{ErasedActorPath, KindId};
use serde::{Deserialize, Serialize};

use super::ResolveError;

/// A typed path a request carried did not prove, so the receiver answers the
/// request with it rather than dropping it (ADR-0231 §3).
///
/// A request whose kind carries a `ProtocolPath` replies with a kind that
/// implements `From<PathRefused>`, its `Err` arm naming the refusal; the
/// `#[actor]` dispatch answers a refused decode with that conversion, and a
/// handler answers a path that closed before its `resolve` the same way,
/// through `From<ResolveError>`. It names the path and why, never a
/// position, so it crosses the wire as a reply field.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct PathRefused {
    /// The path the request named.
    pub path: ErasedActorPath,
    /// Why it did not prove.
    pub reason: PathRefusal,
}

/// Why a typed path did not prove.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathRefusal {
    /// The receiver has no published routes to check the path against: a
    /// guest receiver today (ADR-0241).
    Unchecked,
    /// No route has stood at the path, or its route is still starting.
    Unpublished,
    /// The route at the path does not publish the protocol row `kind`.
    Uncovered {
        /// The first row of the protocol the route does not publish.
        kind: KindId,
    },
    /// The path proved at decode, but no live actor stands at it now.
    NotLive,
}

impl PathRefused {
    /// The refusal a decode reported, when it was a typed-path refusal: the
    /// three errors `DecodeCtx::prove_route_covers` produces. `None` for any
    /// other error, such as malformed bytes, which the dispatcher keeps
    /// dropping.
    #[must_use]
    pub fn from_wire(error: &WireError) -> Option<Self> {
        let (path, reason) = match error {
            WireError::ProtocolPathUnchecked { path } => (path, PathRefusal::Unchecked),
            WireError::ProtocolPathUnpublished { path } => (path, PathRefusal::Unpublished),
            WireError::UncoveredProtocolPath { path, kind } => (path, PathRefusal::Uncovered { kind: *kind }),
            _ => return None,
        };
        Some(Self { path: path.clone(), reason })
    }
}

impl From<ResolveError> for PathRefused {
    fn from(error: ResolveError) -> Self {
        match error {
            ResolveError::NotLive { path } => Self { path, reason: PathRefusal::NotLive },
        }
    }
}

impl Display for PathRefused {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let path = &self.path;
        match self.reason {
            PathRefusal::Unchecked => write!(f, "the receiver cannot check the path {path}"),
            PathRefusal::Unpublished => write!(f, "no route has stood at {path}"),
            PathRefusal::Uncovered { kind } => write!(f, "the route at {path} does not publish {kind}"),
            PathRefusal::NotLive => write!(f, "no live actor stands at {path}"),
        }
    }
}
