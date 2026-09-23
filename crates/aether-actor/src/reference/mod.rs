//! Proven actor references (ADR-0230): [`ActorRef`] and [`ErasedActorRef`].
//!
//! Each type proves its actor reached `Live` at an id, in this engine session.
//! The proof is memory-only — none of these types has a codec — and the only
//! constructors are the crate-private ones the guest SDK uses plus the gated
//! mint for the native registry.

mod actor_ref;
mod erased_actor_ref;
mod mint;

pub use actor_ref::ActorRef;
pub use erased_actor_ref::ErasedActorRef;

#[doc(hidden)]
pub use mint::{__mint_actor_ref, __mint_erased_actor_ref};
