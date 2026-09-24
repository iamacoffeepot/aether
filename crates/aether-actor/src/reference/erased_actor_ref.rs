//! [`ErasedActorRef`]: proof that some actor reached `Live` at an id.

use core::fmt;

use aether_data::MailboxId;

/// Proof that some actor reached `Live` at an id, in this engine session
/// (ADR-0230). The envelope sender arrives in this form: it can reply,
/// monitor, and narrow, and carries no actor type.
///
/// Memory-only like every proven reference: no codec, so an `ErasedActorRef`
/// can be held in actor state but never mailed, configured, or persisted.
///
/// The order is over the proven position, exactly as the `Hash` and `Eq`
/// beside it are, and exists so a cap can key an ordered set of subscribers
/// on proofs rather than on positions — `WindowSubscribers` in
/// `aether-window` is the consumer that asks for it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ErasedActorRef {
    id: MailboxId,
}

impl ErasedActorRef {
    /// Mint a reference for a confirmed-`Live` id. The guest SDK calls this
    /// on the host's answers; native code goes through the gated mint.
    pub(crate) const fn new(id: MailboxId) -> Self {
        Self { id }
    }

    /// The position this reference proves, the mirror of
    /// [`ActorRef::id`](super::ActorRef::id). Reading a position out of a
    /// proof is what ADR-0230 permits; turning a position into a proof is
    /// what it closes, and only the gated mint does that.
    ///
    /// Its consumers are the routing bodies that hand a raw `u64` to the
    /// dispatch surface: the `MailSender` impl for `NativeCtx` in
    /// `aether-substrate`, plus the wasm-side ctxs here.
    #[must_use]
    pub const fn id(self) -> MailboxId {
        self.id
    }
}

/// An erased reference has no actor type to name, and never prints its
/// position.
impl fmt::Debug for ErasedActorRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ErasedActorRef").finish_non_exhaustive()
    }
}
