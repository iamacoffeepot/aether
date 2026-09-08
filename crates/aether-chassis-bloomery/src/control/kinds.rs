//! The `aether.bloomery.control.*` kinds the control core sends itself.
//!
//! Always-on beside the identity ZST rather than inside the `runtime`-gated
//! runtime module (ADR-0122): the `#[actor]` dispatch table names every handled
//! kind, so a self-addressed wake has to resolve without the runtime feature —
//! the same reason the store / source caps keep their kind family here.

/// The self-addressed wake the control core's mainline observer fires each poll
/// interval; its handler asks the source for the repository's live head. Zero-
/// field — the timer carries only the schedule, as the outbox reactors' ticks do.
#[aether_data::kind(name = "aether.bloomery.control.observe_tick", default)]
pub struct ObserveTick {}
