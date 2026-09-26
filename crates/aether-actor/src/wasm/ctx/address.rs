//! Proving an [`ActorPath`] that arrived in config or mail —
//! [`WasmCtx::resolve_path`], the guest twin of the native
//! `NativeCtx::resolve_path` (ADR-0230 §3), and its refusal,
//! [`ResolvePathError`].

use core::error::Error;
use core::fmt;

use aether_data::{ActorPath, MailboxId};
use alloc::string::String;

use super::WasmCtx;
use crate::model::ctx::reply_mode::ReplyMode;
use crate::reference::ErasedActorRef;
use crate::wasm::bridge::address::{self, __ResolvedPath};

/// Why [`WasmCtx::resolve_path`] could not prove an [`ActorPath`] (ADR-0230
/// §3). Neither refusal names a position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvePathError {
    /// The registry refused the path: an unknown or instanced root, an illegal
    /// or ambiguous segment, an over-cap path, or no route at the canonical
    /// path, a dropped route included.
    Unresolved {
        /// The registry's refusal, rendered as text.
        detail: String,
    },
    /// The path names a route whose actor is not `Live`: its birth is still
    /// `Starting`, or it dropped between the two reads.
    NotLive {
        /// The canonical path the route was found at.
        canonical_path: String,
    },
}

impl fmt::Display for ResolvePathError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unresolved { detail } => formatter.write_str(detail),
            Self::NotLive { canonical_path } => write!(formatter, "{canonical_path} is not live"),
        }
    }
}

impl Error for ResolvePathError {}

impl<A, M: ReplyMode> WasmCtx<'_, A, M> {
    /// Prove an [`ActorPath`] that arrived in this component's config or in a
    /// payload, and hand back the proven reference (ADR-0230 §3). The guest
    /// twin of the native `NativeCtx::resolve_path`: the host expands and
    /// resolves the path — ADR-0166 short-path expansion and canonical
    /// validation are the registry's own — and proves the answered position
    /// through the same crate-private path the native verb takes, so the two
    /// answers cannot drift apart. The position never leaves the verb.
    ///
    /// It costs what the native verb costs, one address resolution plus one
    /// published-route read, reached through one host call. Run it once, at
    /// `wire` (a [`WireCtx`](super::WireCtx) derefs here) or at receipt, and
    /// keep the reference; never re-derive it at a send.
    ///
    /// The reference is an [`ErasedActorRef`], because a guest cannot name the
    /// type of a native actor it reaches by path, so a send through it is not
    /// checked by kind: a kind the actor does not handle is caught only at the
    /// recipient.
    ///
    /// Its consumer is the environment bootstrap script
    /// (`aether-bloomery-bootstrap`), whose `wire` proves the journal owner and
    /// the bundle driver from its config.
    ///
    /// # Errors
    ///
    /// [`ResolvePathError::Unresolved`] with the registry's refusal when the
    /// path resolves to no route, and [`ResolvePathError::NotLive`] naming the
    /// canonical path when its route is not `Live`.
    pub fn resolve_path(&self, address: &ActorPath) -> Result<ErasedActorRef, ResolvePathError> {
        match address::resolve_path(address) {
            __ResolvedPath::Live { position } => Ok(ErasedActorRef::new(MailboxId(position))),
            __ResolvedPath::Unresolved { detail } => Err(ResolvePathError::Unresolved { detail }),
            __ResolvedPath::NotLive { canonical_path } => Err(ResolvePathError::NotLive { canonical_path }),
        }
    }
}
