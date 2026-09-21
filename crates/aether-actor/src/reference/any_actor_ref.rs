//! [`AnyActorRef`]: proof that some actor reached `Live` at an id.

use aether_data::MailboxId;

/// Proof that some actor reached `Live` at an id, in this engine session
/// (ADR-0230). The envelope sender arrives in this form: it can reply,
/// monitor, and narrow, and carries no actor type.
///
/// Memory-only like every proven reference: no codec, so an `AnyActorRef`
/// can be held in actor state but never mailed, configured, or persisted.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct AnyActorRef {
    id: MailboxId,
}

impl AnyActorRef {
    /// Mint a reference for a confirmed-`Live` id. The guest SDK calls this
    /// on the host's answers; native code goes through the gated mint.
    pub(crate) const fn new(id: MailboxId) -> Self {
        Self { id }
    }
}
