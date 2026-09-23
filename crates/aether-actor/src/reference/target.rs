//! [`Target`]: a held reference a flat `send_to` verb sends through.

use aether_data::Kind;

use super::{ActorRef, ErasedActorRef};
use crate::model::HandlesKind;

mod sealed {
    use crate::reference::{ActorRef, ErasedActorRef};

    /// The seal: only the proven references in this crate are targets.
    pub trait Sealed {}

    impl<R> Sealed for ActorRef<R> {}

    impl Sealed for ErasedActorRef {}

    impl<T: Sealed + ?Sized> Sealed for &T {}
}

/// A held reference that mail of kind `K` may be sent through (ADR-0232 §1).
///
/// The flat held-reference verbs (`ctx.send_to(reference, &kind)` and its
/// context-carrying siblings) take `impl Target<K>`, so the kind is inferred
/// from the payload and no turbofish is written. An [`ActorRef<R>`] is a
/// target only for the kinds `R` handles, which keeps the compile-time check
/// the `ctx.to(&reference)` handle had. An [`ErasedActorRef`] is a target for
/// every kind, unchecked, as ADR-0230 §2 allows for a proof whose actor type
/// the holder cannot name. A borrow of either is a target too, so a reference
/// reached through a borrow, such as a map lookup, sends without a copy-out.
/// A held reference is `Copy`, so a call site passes it by value.
///
/// The trait is sealed: no crate outside `aether-actor` adds a target, so a
/// foreign impl cannot forward an erased proof as any kind it likes.
///
/// ```
/// use aether_actor::{ActorRef, Addressable, Embedded, HandlesKind, WasmCtx};
///
/// struct Peer;
///
/// impl Addressable for Peer {
///     const NAMESPACE: &'static str = "example.peer";
///     type Resolver = Embedded;
/// }
///
/// impl HandlesKind<()> for Peer {}
///
/// fn ping(ctx: &mut WasmCtx<'_>, peer: ActorRef<Peer>) {
///     ctx.send_to(peer, &());
/// }
/// ```
///
/// A typed reference to an actor with no handler for the kind does not
/// compile:
///
/// ```compile_fail,E0277
/// use aether_actor::{ActorRef, Addressable, Embedded, WasmCtx};
///
/// struct Peer;
///
/// impl Addressable for Peer {
///     const NAMESPACE: &'static str = "example.peer";
///     type Resolver = Embedded;
/// }
///
/// fn ping(ctx: &mut WasmCtx<'_>, peer: ActorRef<Peer>) {
///     ctx.send_to(peer, &());
/// }
/// ```
pub trait Target<K: Kind>: sealed::Sealed {
    /// The proof this target sends through, with its actor type forgotten.
    fn erased(&self) -> ErasedActorRef;
}

impl<R: HandlesKind<K>, K: Kind> Target<K> for ActorRef<R> {
    fn erased(&self) -> ErasedActorRef {
        self.erase()
    }
}

impl<K: Kind> Target<K> for ErasedActorRef {
    fn erased(&self) -> ErasedActorRef {
        *self
    }
}

impl<K: Kind, T: Target<K> + ?Sized> Target<K> for &T {
    fn erased(&self) -> ErasedActorRef {
        (**self).erased()
    }
}
