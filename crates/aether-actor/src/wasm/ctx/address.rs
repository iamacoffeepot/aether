//! Proving a path that arrived in config or mail: [`WasmCtx::resolve_path`],
//! the guest twin of the native `NativeCtx::resolve_path` (ADR-0230 §3), over
//! an untyped [`ErasedActorPath`], and its refusal, [`ResolvePathError`]; and
//! [`WasmCtx::resolve`], the guest's typed-path door (ADR-0230 §3, #7205),
//! over an `ActorPath<R>` or a `ProtocolPath<P>`, and its refusal,
//! [`ResolveError`].

use core::error::Error;
use core::fmt;

use aether_data::{ErasedActorPath, MailboxId};
use alloc::string::String;

use super::WasmCtx;
use crate::model::ctx::reply_mode::ReplyMode;
use crate::path::ConfirmedLive;
use crate::reference::ErasedActorRef;
use crate::wasm::bridge::address::{self, __ResolvedPath};
use crate::{ResolveError, TypedPath};

/// Why [`WasmCtx::resolve_path`] could not prove an [`ErasedActorPath`] (ADR-0230
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
    /// Prove an [`ErasedActorPath`] that arrived in this component's config or in a
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
    /// The reference is an [`ErasedActorRef`], because its answer names no
    /// actor type: it is for a guest handed an [`ErasedActorPath`] with no
    /// compile-time claim on what stands there — text that arrived over the
    /// wire, or a short path that only expands against the live registry. A
    /// holder that already knows the actor type resolves an
    /// [`ActorPath<R>`](crate::ActorPath) instead, through [`Self::resolve`],
    /// and gets a kind-checked [`ActorRef<R>`](crate::ActorRef). After #6932, this verb serves identity
    /// (naming what a path resolves to) and the guard cast
    /// ([`Self::cast`](super::WasmCtx::cast), over a reference this proves),
    /// not a checked send.
    ///
    /// # Errors
    ///
    /// [`ResolvePathError::Unresolved`] with the registry's refusal when the
    /// path resolves to no route, and [`ResolvePathError::NotLive`] naming the
    /// canonical path when its route is not `Live`.
    pub fn resolve_path(&self, address: &ErasedActorPath) -> Result<ErasedActorRef, ResolvePathError> {
        match address::resolve_path(address) {
            __ResolvedPath::Live { position } => Ok(ErasedActorRef::new(MailboxId(position))),
            __ResolvedPath::Unresolved { detail } => Err(ResolvePathError::Unresolved { detail }),
            __ResolvedPath::NotLive { canonical_path } => Err(ResolvePathError::NotLive { canonical_path }),
        }
    }

    /// Prove a typed path that arrived in this component's config or in a
    /// payload, and hand back the proven, kind-checked reference its type
    /// names (ADR-0230 §3, #7205, #7501): an [`ActorRef<R>`](crate::ActorRef)
    /// for an [`ActorPath<R>`](crate::ActorPath), a
    /// [`ProtocolRef<P>`](crate::ProtocolRef) for a
    /// [`ProtocolPath<P>`](crate::ProtocolPath). The guest's typed-path door,
    /// beside [`Self::resolve_path`]'s untyped one: it folds the path as
    /// written — a typed path is canonical by construction, so no short-path
    /// expansion runs — and finds the `Live` route standing under exactly
    /// that canonical name through `Registry::live_route`, the read the
    /// native `NativeCtx::resolve` takes, so the two answers cannot drift
    /// apart.
    ///
    /// It proves liveness and nothing else. The path's type was proven where
    /// the path was made: an `ActorPath<R>`'s leaf namespace is
    /// `R::NAMESPACE` by construction (ADR-0230 §2), so a route standing
    /// under it is an `R` and no actor-type tag is compared; a
    /// `ProtocolPath<P>` proved at its decode that the route published every
    /// row of `P`, so no rows are read or compared here. A name is never
    /// reused and a route's rows only grow within one engine (ADR-0231 §5),
    /// so the route found now is the one the decode read.
    ///
    /// It costs one host call reading the published route view. Run it
    /// once, at `wire` (a [`WireCtx`](super::WireCtx) derefs here) or at
    /// receipt, keep the reference, and send through it with
    /// [`Self::send_to`](super::WasmCtx::send_to); never re-derive it at a
    /// send.
    ///
    /// Its consumers are the environment bootstrap script
    /// (`aether-bloomery-bootstrap`), whose `wire` proves the journal owner
    /// and the bundle driver from its config, and a guest whose config names
    /// a peer by protocol, such as a scene naming whatever publishes a view.
    ///
    /// # Errors
    ///
    /// [`ResolveError::NotLive`] naming the path when no `Live` route stands
    /// under its canonical name: the actor closed, or is still starting. It
    /// never names a position.
    pub fn resolve<T: TypedPath>(&self, path: &T) -> Result<T::Proof, ResolveError> {
        address::live_route(path.__erased())
            .position
            .map(|position| T::__mint(MailboxId(position), ConfirmedLive(())))
            .ok_or_else(|| ResolveError::NotLive { path: path.__erased().clone() })
    }
}
