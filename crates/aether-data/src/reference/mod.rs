//! ADR-0230 proven actor references: the vocabulary that says "this actor was
//! registered" — [`Namespace`](crate::Namespace), [`LoadName`](crate::LoadName),
//! [`Address`](crate::Address), [`ActorRef`](crate::ActorRef),
//! [`Recipient`](crate::Recipient), [`AnyActorRef`](crate::AnyActorRef), and
//! [`Tombstone`](crate::Tombstone). One file per type, re-exported from the
//! crate root; the segment grammar and the shared id codec stay private.

mod actor_ref;
mod address;
mod any_actor_ref;
pub(crate) mod id_codec;
mod load_name;
mod namespace;
mod recipient;
pub(crate) mod segment;
mod tombstone;

pub use actor_ref::ActorRef;
pub use address::Address;
pub use any_actor_ref::AnyActorRef;
pub use load_name::{LoadName, LoadNameError};
pub use namespace::Namespace;
pub use recipient::Recipient;
pub use tombstone::Tombstone;
