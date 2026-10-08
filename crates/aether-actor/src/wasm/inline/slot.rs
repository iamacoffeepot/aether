//! The life of an inline child's slot: where its child is, and whether that
//! child owes an `unwire` (ADR-0249 §4, §6).
//!
//! A slot's one changing field is its [`Seat`]. The registry's verbs are the
//! moves between its cases: `insert_child` and `reinsert` seat a child,
//! `reserve` makes a slot that is born [`Seat::Out`], and `take` hands the
//! seated [`Child`] to its caller. The wiring fact travels with the box, so
//! while a child is out its holder has it as a local and the slot remembers
//! nothing about what to put back.

use alloc::boxed::Box;

use crate::wasm::ErasedWasmActor;

/// Where a slot's child is.
pub enum Seat {
    /// At rest in the registry.
    Seated(Child),
    /// On the stack of the one caller running its handler or hook. That
    /// caller holds the [`Child`].
    Out,
}

/// A built inline child and whether it owes an `unwire`.
pub enum Child {
    /// `wire` has not returned `Ok` on this box, or its `unwire` has run. It
    /// owes no `unwire`.
    Unwired(Box<dyn ErasedWasmActor>),
    /// `wire` returned `Ok` and `unwire` has not run. It owes exactly one
    /// `unwire`.
    Wired(Box<dyn ErasedWasmActor>),
}

impl Child {
    /// The actor of either case, for a caller that runs a handler or a hook
    /// on it.
    pub fn actor_mut(&mut self) -> &mut dyn ErasedWasmActor {
        match self {
            Self::Unwired(actor) | Self::Wired(actor) => actor.as_mut(),
        }
    }
}

/// What `Registry::reinsert` did with the child it was handed.
pub enum Reinserted {
    /// The child is back in its slot.
    Seated,
    /// Its slot was removed while it was out. The caller owns the child and
    /// closes it.
    Departed(Child),
}
