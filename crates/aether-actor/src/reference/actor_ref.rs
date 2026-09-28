//! [`ActorRef`]: proof that an actor reached `Live` at an id.

use core::fmt;
use core::hash::{Hash, Hasher};
use core::marker::PhantomData;

use aether_data::MailboxId;

use super::{ErasedActorRef, ProtocolRef};
use crate::Addressable;
use crate::model::CoveredBy;

/// Proof that an actor of type `R` reached `Live` at an id, in this engine
/// session (ADR-0230).
///
/// A decoder cannot know that claim, so the proof is memory-only: [`ActorRef`]
/// has no codec impl of any kind, and only ADR-0230 §3's doors yield one.
/// What crosses a boundary is the actor's path, and the receiver proves it
/// again on its own side.
///
/// ```compile_fail,E0277
/// # use aether_actor::ActorRef;
/// #
/// fn assert_serialize<T: serde::Serialize>() {}
///
/// assert_serialize::<ActorRef<()>>();
/// ```
///
/// ```compile_fail,E0277
/// # use aether_actor::ActorRef;
/// #
/// fn assert_schema<T: aether_data::Schema>() {}
///
/// assert_schema::<ActorRef<()>>();
/// ```
///
/// ```compile_fail,E0277
/// # use aether_actor::ActorRef;
/// #
/// fn assert_wire_encode<T: aether_data::wire::WireEncode>() {}
///
/// assert_wire_encode::<ActorRef<()>>();
/// ```
pub struct ActorRef<R> {
    id: MailboxId,
    _actor: PhantomData<fn() -> R>,
}

impl<R> ActorRef<R> {
    /// Mint a reference for a confirmed-`Live` id. The guest SDK calls this
    /// on the host's answers; native code goes through the gated mint.
    /// Unreachable from any other crate:
    ///
    /// ```compile_fail,E0624
    /// # use aether_actor::{ActorRef, MailboxId};
    /// #
    /// let _reference = ActorRef::<()>::new(MailboxId(7));
    /// ```
    pub(crate) const fn new(id: MailboxId) -> Self {
        Self { id, _actor: PhantomData }
    }

    /// The position this reference proves.
    #[must_use]
    pub const fn id(self) -> MailboxId {
        self.id
    }

    /// Forget the actor type, keeping the proof.
    #[must_use]
    pub const fn erase(self) -> ErasedActorRef {
        ErasedActorRef::new(self.id)
    }

    /// The same proof under a narrower claim: the actor here covers `P`
    /// (ADR-0231 §3). Compiles only for `P: CoveredBy<R>`, the sealed coverage
    /// check over `R`'s contract rows (ADR-0231 §2), so a send through the
    /// result compiles only for a kind `P` lists and `R` handles with the
    /// row's exact reply. A copy of the proof: no registry is read.
    #[must_use]
    pub const fn narrow<P: CoveredBy<R>>(self) -> ProtocolRef<P> {
        ProtocolRef::new(self.id)
    }
}

impl<R> Clone for ActorRef<R> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<R> Copy for ActorRef<R> {}

impl<R> PartialEq for ActorRef<R> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl<R> Eq for ActorRef<R> {}

impl<R> Hash for ActorRef<R> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

/// A reference prints its actor type, never its position.
impl<R: Addressable> fmt::Debug for ActorRef<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ActorRef<{}>", R::NAMESPACE)
    }
}

#[cfg(test)]
mod tests {
    use alloc::format;

    use aether_data::MailboxId;

    use super::ActorRef;

    struct Probe;

    impl crate::Addressable for Probe {
        const NAMESPACE: &'static str = "test.reference.probe";
        type Resolver = crate::One;
    }

    // Tripwire: a reference never renders its position, typed or erased.
    #[test]
    fn debug_names_the_actor_type_and_never_the_position() {
        let reference = ActorRef::<Probe>::new(MailboxId(7));

        assert_eq!(format!("{reference:?}"), "ActorRef<test.reference.probe>");
        assert_eq!(format!("{:?}", reference.erase()), "ErasedActorRef { .. }");
    }
}
