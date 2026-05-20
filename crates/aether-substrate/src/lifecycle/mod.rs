//! ADR-0082 application-declared lifecycle sequence.
//!
//! Each chassis declares its lifecycle as an ordered directed graph of
//! `(factory, next_kind, optional quit_kind)` states. The
//! [`LifecycleDriverCapability`] is a `NativeActor` registered at
//! `aether.lifecycle` that holds the compiled graph, broadcasts each
//! state's kind on receipt of an [`Advance`](aether_kinds::Advance) mail,
//! awaits settlement (ADR-0080), and replies with the next state's id —
//! enabling chassis main loops to drive the lifecycle cadence by mail
//! without per-stage hand-rolled glue.
//!
//! See ADR-0082 for the full design. PR 2 of the migration shipped the
//! core types and synthetic-chassis tests; PR 3 wired the driver into
//! production chassis main loops; PR 4 renamed `aether.tick` into the
//! `aether.lifecycle.*` family.

mod driver;
mod graph;

pub use driver::{LifecycleDriverCapability, LifecycleDriverConfig};
pub use graph::{
    BuildError, LifecycleGraph, LifecycleGraphBuilder, NoOpen, OpenNoNext, OpenWithNext,
};
