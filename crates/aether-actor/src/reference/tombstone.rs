//! [`Tombstone`]: proof that a proven actor is dead.

use core::fmt;
use core::hash::{Hash, Hasher};
use core::marker::PhantomData;

use aether_data::MailboxId;

/// Proof that the actor of type `R` proven at an id is dead (ADR-0230).
/// Terminal under the registration order, so it stays true forever and keys
/// cleanup of held state.
///
/// The only way one comes into being is `ActorRef::entomb`, which exchanges
/// the reference it proves. Memory-only like every proven reference: no
/// codec, so a `Tombstone` can be held in actor state but never mailed,
/// configured, or persisted.
pub struct Tombstone<R> {
    id: MailboxId,
    _actor: PhantomData<fn() -> R>,
}

impl<R> Tombstone<R> {
    /// Record a proven id as dead. Called only by `ActorRef::entomb`.
    pub(super) const fn new(id: MailboxId) -> Self {
        Self { id, _actor: PhantomData }
    }

    /// The position whose actor is dead.
    #[must_use]
    pub const fn id(self) -> MailboxId {
        self.id
    }
}

impl<R> Clone for Tombstone<R> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<R> Copy for Tombstone<R> {}

impl<R> PartialEq for Tombstone<R> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl<R> Eq for Tombstone<R> {}

impl<R> Hash for Tombstone<R> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

impl<R> fmt::Debug for Tombstone<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tombstone").field("id", &self.id).finish()
    }
}
