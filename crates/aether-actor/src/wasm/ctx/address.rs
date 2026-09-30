//! Proving a path that arrived in config or mail: [`WasmCtx::resolve_path`],
//! the guest twin of the native `NativeCtx::resolve_path` (ADR-0230 §3), over
//! an untyped [`ErasedActorPath`], and its refusal, [`ResolvePathError`]; and
//! [`WasmCtx::resolve`], the guest's typed-path door (ADR-0230 §3, #7205),
//! over an [`ActorPath<R>`], and its refusal, [`ResolveError`].

use core::error::Error;
use core::fmt;

use aether_data::{ErasedActorPath, MailboxId};
use alloc::string::String;

use super::WasmCtx;
use crate::model::ctx::reply_mode::ReplyMode;
use crate::reference::{ActorRef, ErasedActorRef};
use crate::wasm::bridge::address::{self, __ResolvedPath};
use crate::{ActorPath, Addressable, ResolveError};

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
    /// [`ActorPath<R>`] instead, through [`Self::resolve`], and gets a
    /// kind-checked [`ActorRef<R>`]. An erased reference has no send verb
    /// (ADR-0231 §4), so this verb serves identity (naming what a path
    /// resolves to) and the guard cast
    /// ([`Self::cast`](super::WasmCtx::cast), over a reference this proves).
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

    /// Prove an [`ActorPath<R>`] that arrived in this component's config or
    /// in a payload, and hand back a proven, kind-checked [`ActorRef<R>`]
    /// (ADR-0230 §3, #7205). The guest's typed-path door, beside
    /// [`Self::resolve_path`]'s untyped one: both fold the path as written —
    /// a typed path is canonical by construction, so no short-path expansion
    /// runs — and find the `Live` route standing under exactly that
    /// canonical name through `Registry::live_route`, the same read the
    /// native `NativeCtx::resolve` (over a `ProtocolPath<P>`) takes, so the
    /// two answers cannot drift apart. No actor-type tag is compared: the
    /// path's leaf namespace is `R::NAMESPACE` by construction (ADR-0230
    /// §2), so a route standing under it is an `R`.
    ///
    /// It costs one host call reading the published route view. Run it
    /// once, at `wire` (a [`WireCtx`](super::WireCtx) derefs here) or at
    /// receipt, and keep the reference; never re-derive it at a send.
    ///
    /// Its consumer is the environment bootstrap script
    /// (`aether-bloomery-bootstrap`), whose `wire` proves the journal owner
    /// and the bundle driver from its config.
    ///
    /// # Errors
    ///
    /// [`ResolveError::NotLive`] naming the path when no `Live` route stands
    /// under its canonical name. It never names a position.
    pub fn resolve<R: Addressable>(&self, path: &ActorPath<R>) -> Result<ActorRef<R>, ResolveError> {
        address::live_route(path.as_erased())
            .position
            .map(|position| ActorRef::new(MailboxId(position)))
            .ok_or_else(|| ResolveError::NotLive { path: path.as_erased().clone() })
    }
}
