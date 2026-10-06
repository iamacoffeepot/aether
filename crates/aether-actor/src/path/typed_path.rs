//! [`TypedPath`]: the typed paths a guest's `resolve` proves, each naming
//! the proof it mints (ADR-0230 §3).

use aether_data::{ErasedActorPath, MailboxId};

use super::{ActorPath, ProtocolPath};
use crate::reference::{ActorRef, ProtocolRef};
use crate::{Addressable, Protocol};

mod sealed {
    pub trait Sealed {}
}

/// What `TypedPath::__mint` takes, so only this crate mints through it: the
/// type is not exported, and its field is crate-private. The guest's
/// `resolve` builds one after the host confirmed a `Live` route.
pub struct ConfirmedLive(pub(crate) ());

/// A typed path: an [`ActorPath<R>`] or a [`ProtocolPath<P>`]. Its type
/// names the proof `WasmCtx::resolve` mints from it, an
/// [`ActorRef<R>`] or a [`ProtocolRef<P>`], so the guest has one `resolve`
/// for both (ADR-0230 §3). Sealed: the two typed paths are the only
/// implementors.
pub trait TypedPath: sealed::Sealed {
    /// The reference a live route at this path is proven as.
    type Proof;

    /// The path text `resolve` asks the host about.
    #[doc(hidden)]
    fn __erased(&self) -> &ErasedActorPath;

    /// The proof for the `Live` route the host answered at `position`.
    #[doc(hidden)]
    fn __mint(position: MailboxId, live: ConfirmedLive) -> Self::Proof;
}

impl<R: Addressable> sealed::Sealed for ActorPath<R> {}

impl<R: Addressable> TypedPath for ActorPath<R> {
    type Proof = ActorRef<R>;

    fn __erased(&self) -> &ErasedActorPath {
        self.as_erased()
    }

    fn __mint(position: MailboxId, _live: ConfirmedLive) -> ActorRef<R> {
        ActorRef::new(position)
    }
}

impl<P: Protocol> sealed::Sealed for ProtocolPath<P> {}

impl<P: Protocol> TypedPath for ProtocolPath<P> {
    type Proof = ProtocolRef<P>;

    fn __erased(&self) -> &ErasedActorPath {
        self.as_erased()
    }

    fn __mint(position: MailboxId, _live: ConfirmedLive) -> ProtocolRef<P> {
        ProtocolRef::new(position)
    }
}
