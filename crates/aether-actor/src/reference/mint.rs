//! The gated mint: the native registry's door to the proven types.
//!
//! Contract: the caller has confirmed a `Live` route at `id`. These functions
//! are `#[doc(hidden)]` and guarded by `scripts/check-reference-mint.py`: the
//! only permitted callers are the paths in that scanner's allowlist, and
//! widening it means editing the gate in a reviewed diff. The guest SDK mints
//! through the crate-private constructors instead, so it needs no door here.

use aether_data::MailboxId;

use super::{ActorRef, AnyActorRef};

/// Mint an [`ActorRef`] for a confirmed-`Live` id. See the module contract.
#[doc(hidden)]
#[must_use]
pub const fn __mint_actor_ref<R>(id: MailboxId) -> ActorRef<R> {
    ActorRef::new(id)
}

/// Mint an [`AnyActorRef`] for a confirmed-`Live` id. See the module contract.
#[doc(hidden)]
#[must_use]
pub const fn __mint_any_actor_ref(id: MailboxId) -> AnyActorRef {
    AnyActorRef::new(id)
}
