//! The owned-bytes envelope shape native dispatchers receive on their
//! mpsc inbox.
//!
//! Pre-refactor `Envelope` was a separate struct that mirrored
//! [`OwnedDispatch`](crate::mail::registry::OwnedDispatch)
//! field-for-field, with a `From<OwnedDispatch>` impl moving every
//! field across. Both the struct definition and
//! the move were Qodana DC findings — and rightly so, since the two
//! types literally carried the same data with the same ownership
//! semantics under two names. `Envelope` is now a type alias for
//! `OwnedDispatch`: same fields, same construction sites
//! (`Envelope { kind, ... }` still type-checks), every `From` call
//! site collapses to a no-op.
//!
//! The alias preserves the actor-layer naming (production code that
//! says "envelope into the actor's inbox" reads naturally) without
//! the duplicated type definition.

pub use crate::mail::registry::OwnedDispatch as Envelope;
