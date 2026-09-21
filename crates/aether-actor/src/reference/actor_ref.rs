//! [`ActorRef`]: proof that an actor reached `Live` at an id.

use core::fmt;
use core::hash::{Hash, Hasher};
use core::marker::PhantomData;

use aether_data::{Address, MailboxId};
use aether_kinds::MonitorNotice;

use super::{AnyActorRef, Tombstone};

/// Proof that an actor of type `R` reached `Live` at an id, in this engine
/// session (ADR-0230).
///
/// A decoder cannot know that claim, so the proof is memory-only: [`ActorRef`]
/// has no codec impl of any kind, and only resolving an [`Address`] against
/// the registry yields one. What crosses a boundary is
/// [`ActorRef::address`], and the receiver proves it again on its own side.
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
    pub const fn erase(self) -> AnyActorRef {
        AnyActorRef::new(self.id)
    }

    /// The exact address of the proven position: how a held reference is
    /// handed to a peer, which resolves it on its own side.
    #[must_use]
    pub const fn address(self) -> Address<R> {
        Address::exact(self.id)
    }

    /// Exchange this reference for a [`Tombstone`] when `notice` names its
    /// id, or get the reference back unchanged on a mismatch. Reads only the
    /// notice's target.
    pub fn entomb(self, notice: &MonitorNotice) -> Result<Tombstone<R>, Self> {
        if notice.target == self.id {
            Ok(Tombstone::new(self.id))
        } else {
            Err(self)
        }
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

impl<R> fmt::Debug for ActorRef<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ActorRef").field("id", &self.id).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entomb_trades_a_matching_reference_for_its_tombstone() {
        let id = MailboxId(7);

        let matching = ActorRef::<()>::new(id).entomb(&MonitorNotice { target: id });
        let Ok(tombstone) = matching else {
            panic!("a notice for the reference's id must exchange it");
        };
        assert_eq!(tombstone.id(), id);

        let mismatched = ActorRef::<()>::new(id).entomb(&MonitorNotice { target: MailboxId(8) });
        let Err(returned) = mismatched else {
            panic!("a notice for another id must return the reference");
        };
        assert_eq!(returned.id(), id);
    }
}
