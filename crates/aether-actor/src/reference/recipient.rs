//! [`Recipient`]: proof that an actor handling a kind reached `Live`.

use core::fmt;
use core::hash::{Hash, Hasher};
use core::marker::PhantomData;

use aether_data::MailboxId;

/// Proof that an actor handling kind `K` reached `Live` at an id, in this
/// engine session (ADR-0230). What a capability holds for a subscriber or
/// consumer: the holder knows the actor handles `K` and nothing else about
/// it.
///
/// Memory-only like every proven reference: no codec, so a `Recipient` can be
/// held in actor state but never mailed, configured, or persisted.
pub struct Recipient<K> {
    id: MailboxId,
    _kind: PhantomData<fn(K)>,
}

impl<K> Recipient<K> {
    /// Mint a recipient for a confirmed-`Live` id. The guest SDK calls this
    /// on the host's answers; native code goes through the gated mint.
    pub(crate) const fn new(id: MailboxId) -> Self {
        Self { id, _kind: PhantomData }
    }
}

impl<K> Clone for Recipient<K> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<K> Copy for Recipient<K> {}

impl<K> PartialEq for Recipient<K> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl<K> Eq for Recipient<K> {}

impl<K> Hash for Recipient<K> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

impl<K> fmt::Debug for Recipient<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Recipient").field("id", &self.id).finish()
    }
}
