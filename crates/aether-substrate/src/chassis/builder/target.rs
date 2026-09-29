//! Typed targets for mail pushed by a chassis embedder.

use aether_actor::{ActorRef, Direct, ErasedActorRef, HandlesKind, Protocol, ProtocolRef, RowAt};
use aether_data::Kind;

mod sealed {
    use aether_actor::{ActorRef, ProtocolRef};

    pub trait Sealed {}

    impl<R> Sealed for ActorRef<R> {}
    impl<P> Sealed for ProtocolRef<P> {}
    impl<T: Sealed + ?Sized> Sealed for &T {}
}

/// A proven reference through which a chassis embedder may push mail of kind
/// `K`.
///
/// The target is checked entirely by the compiler. An actor reference is a
/// target only for kinds its actor handles, and a protocol reference is a
/// target only for kinds listed by the protocol. The trait is sealed and has
/// no implementation for [`ErasedActorRef`], so an embedder cannot pair an
/// erased proof with arbitrary bytes.
///
/// An erased reference is not a chassis target:
///
/// ```compile_fail,E0277
/// use aether_actor::{Direct, ErasedActorRef};
/// use aether_substrate::ChassisTarget;
///
/// fn require_target<T: ChassisTarget<(), Direct>>(_: T) {}
///
/// fn erased_is_not_a_target(reference: ErasedActorRef) {
///     require_target(reference);
/// }
/// ```
///
/// A typed reference is not a target for a kind its actor does not handle:
///
/// ```compile_fail,E0277
/// use aether_actor::{ActorRef, Addressable, Direct, One};
/// use aether_kinds::Ping;
/// use aether_substrate::ChassisTarget;
///
/// struct Peer;
///
/// impl Addressable for Peer {
///     const NAMESPACE: &'static str = "example.peer";
///     type Resolver = One;
/// }
///
/// fn require_target<T: ChassisTarget<Ping, Direct>>(_: T) {}
///
/// fn wrong_kind(reference: ActorRef<Peer>) {
///     require_target(reference);
/// }
/// ```
pub trait ChassisTarget<K: Kind, I = Direct>: sealed::Sealed {
    /// Forget the target's static actor or protocol type after its coverage
    /// check has compiled.
    fn erased(&self) -> ErasedActorRef;
}

impl<R: HandlesKind<K>, K: Kind> ChassisTarget<K> for ActorRef<R> {
    fn erased(&self) -> ErasedActorRef {
        self.erase()
    }
}

impl<P: Protocol, K: Kind, I> ChassisTarget<K, I> for ProtocolRef<P>
where
    P::Rows: RowAt<K, I>,
{
    fn erased(&self) -> ErasedActorRef {
        self.erase()
    }
}

impl<K: Kind, I, T: ChassisTarget<K, I> + ?Sized> ChassisTarget<K, I> for &T {
    fn erased(&self) -> ErasedActorRef {
        (**self).erased()
    }
}
