//! The `aether.substrate_harness` capability (ADR-0067, issue 603 Phase 4).
//!
//! [`SubstrateHarnessCapability`] is the one drive. Only the substrate-harness
//! chassis advances ticks by mail rather than from a frame loop, so the
//! handler hands `advance { ticks }` to the embedder loop over [`events`] and
//! the loop replies once the ticks complete. Every other chassis runs its own
//! frame loop and composes no `aether.substrate_harness` actor, so a dependent
//! is refused there.
//!
//! The identity is a wasm-safe ZST; everything `aether_substrate`-typed — the
//! runtime state and the embedder channel — rides the `runtime` feature.

#![forbid(unsafe_code)]

pub mod cap;
#[cfg(feature = "runtime")]
pub mod events;

#[cfg(feature = "runtime")]
pub use cap::SubstrateHarnessCapParams;
pub use cap::SubstrateHarnessCapability;
