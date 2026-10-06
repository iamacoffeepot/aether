//! [`Target`]: a held reference a flat `send_to` verb sends through.

use aether_data::ActorMail;

use super::{ActorRef, ErasedActorRef, ProtocolRef};
use crate::model::{Anyone, HandlesKind, Protocol, RowAt};

mod sealed {
    use crate::reference::{ActorRef, ProtocolRef};

    /// The seal: only the proven references in this crate are targets.
    pub trait Sealed {}

    impl<R> Sealed for ActorRef<R> {}

    impl<P> Sealed for ProtocolRef<P> {}

    impl<T: Sealed + ?Sized> Sealed for &T {}
}

/// The index of a target that needs no row lookup, an [`ActorRef<R>`]: the default of [`Target`]'s index parameter, so
/// `Target<K>` is `Target<K, Direct>`. Inferred, never written.
pub struct Direct;

/// A held reference that mail of kind `K` may be sent through (ADR-0232 §1).
///
/// The flat held-reference verbs (`ctx.send_to(reference, &kind)` and its
/// context-carrying siblings) take `impl Target<K, I>`, so the kind is inferred
/// from the payload and no turbofish is written. An [`ActorRef<R>`] is a
/// target only for the kinds `R` handles, which keeps the compile-time check
/// a typed send to `R` carries. The kind must be [`ActorMail`], so no target
/// carries engine-only mail (ADR-0233). A borrow of a target is a target too,
/// so a reference reached through a borrow, such as a map lookup, sends
/// without a copy-out. A held reference is `Copy`, so a call site passes it by
/// value.
///
/// An [`ErasedActorRef`] is not a target: an erased reference has no send
/// verb (ADR-0231 §4). It names, identifies, monitors, and replies; a holder
/// that must send through one casts it once, where it arrives, to a protocol
/// it handles.
///
/// A [`ProtocolRef<P>`] is a target only for the kinds `P` lists (ADR-0231
/// §3). A protocol implements no [`Contract<K>`](crate::Contract), so its
/// impl finds `K`'s row through [`RowAt<K, I>`]: `I` is the row's position,
/// which the compiler infers at the call site. The held-reference verbs,
/// native and guest (`WasmCtx::send_to`, `Sends::send_to`), take
/// `impl Target<K, I>` with `I` inferred; the other targets are
/// `Target<K, Direct>`, which `Target<K>` names.
///
/// The trait is sealed: no crate outside `aether-actor` adds a target, so a
/// foreign impl cannot forward an erased proof as any kind it likes.
///
/// An erased reference does not compile as a target, for any kind:
///
/// ```compile_fail,E0277
/// use aether_actor::{ErasedActorRef, WasmCtx};
///
/// fn ping(ctx: &mut WasmCtx<'_>, peer: ErasedActorRef) {
///     ctx.send_to(peer, &());
/// }
/// ```
///
/// ```
/// use aether_actor::{ActorRef, Addressable, HandlesKind, One, WasmCtx};
///
/// struct Peer;
///
/// impl Addressable for Peer {
///     const NAMESPACE: &'static str = "example.peer";
///     type Resolver = One;
/// }
///
/// impl HandlesKind<()> for Peer { type Sender = aether_actor::Anyone; }
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
/// use aether_actor::{ActorRef, Addressable, One, WasmCtx};
///
/// struct Peer;
///
/// impl Addressable for Peer {
///     const NAMESPACE: &'static str = "example.peer";
///     type Resolver = One;
/// }
///
/// fn ping(ctx: &mut WasmCtx<'_>, peer: ActorRef<Peer>) {
///     ctx.send_to(peer, &());
/// }
/// ```
///
/// A protocol reference is a target for each kind its protocol lists, by
/// value or by borrow, with the row's index inferred:
///
/// ```
/// use aether_actor::{Protocol, ProtocolRef, Row, Silent, Target};
/// use aether_data::ActorMail;
/// use aether_kinds::{Ping, Pong};
///
/// struct Pinging;
///
/// impl Protocol for Pinging {
///     type Rows = (Row<Ping, Pong>, Row<(), Silent>);
/// }
///
/// fn sendable<K: ActorMail, I>(_: impl Target<K, I>, _: &K) {}
///
/// fn ping(pinging: ProtocolRef<Pinging>) {
///     sendable(pinging, &Ping::default());
///     sendable(&pinging, &());
/// }
/// ```
///
/// and for no other kind, even one the target may handle:
///
/// ```compile_fail,E0277
/// use aether_actor::{Protocol, ProtocolRef, Row, Silent, Target};
/// use aether_data::ActorMail;
/// use aether_kinds::{Ping, Pong};
///
/// struct Pinging;
///
/// impl Protocol for Pinging {
///     type Rows = (Row<Ping, Pong>, Row<(), Silent>);
/// }
///
/// fn sendable<K: ActorMail, I>(_: impl Target<K, I>, _: &K) {}
///
/// fn ping(pinging: ProtocolRef<Pinging>) {
///     sendable(pinging, &Pong::default());
/// }
/// ```
pub trait Target<K: ActorMail, I = Direct>: sealed::Sealed {
    /// What the target's handler for `K` requires of the sending actor.
    type Sender: Protocol;

    /// The proof this target sends through, with its actor type forgotten.
    fn erased(&self) -> ErasedActorRef;
}

impl<R: HandlesKind<K>, K: ActorMail> Target<K> for ActorRef<R> {
    type Sender = R::Sender;

    fn erased(&self) -> ErasedActorRef {
        self.erase()
    }
}

impl<P: Protocol, K: ActorMail, I> Target<K, I> for ProtocolRef<P>
where
    P::Rows: RowAt<K, I>,
{
    type Sender = Anyone;

    fn erased(&self) -> ErasedActorRef {
        self.target()
    }
}

impl<K: ActorMail, I, T: Target<K, I> + ?Sized> Target<K, I> for &T {
    type Sender = T::Sender;

    fn erased(&self) -> ErasedActorRef {
        (**self).erased()
    }
}
