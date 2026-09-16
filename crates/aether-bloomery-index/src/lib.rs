//! Typed last-move index of named heads over a contiguous journal prefix.
//!
//! [`Heads`] consumes [`aether_bloomery_journal::Entry`] values in sequence
//! order and answers lookup by [`aether_bloomery_kinds::Symbol`]. It does not
//! read the store, the journal, a clock, or another index. Current bindings
//! are this fold, not another source of truth.

mod heads;

pub use heads::{HeadFoldError, Heads};
