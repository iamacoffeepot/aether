//! The `aether.substrate_harness` capability (ADR-0067, issue 603 Phase 4).
//!
//! One mailbox, two chassis profiles, as ADR-0122 lays out for a cap whose
//! behaviour differs by host:
//!
//! - [`SubstrateHarnessCapability`] — the real drive. Only the substrate-harness
//!   chassis advances ticks by mail rather than from a frame loop, so the
//!   handler hands `advance { ticks }` to the embedder loop over [`events`]
//!   and the loop replies once the ticks complete.
//! - [`UnsupportedSubstrateHarnessCapability`] — the fail-fast stub desktop,
//!   headless and hub compose. It claims the same mailbox and replies
//!   `AdvanceResult::Err`, so an agent's `advance` fails immediately instead of
//!   warn-dropping into a reply that never arrives.
//!
//! The identities are wasm-safe ZSTs; everything `aether_substrate`-typed —
//! both runtime states and the embedder channel — rides the `runtime` feature.

#![forbid(unsafe_code)]

pub mod cap;
#[cfg(feature = "runtime")]
pub mod events;
pub mod unsupported_cap;

#[cfg(feature = "runtime")]
pub use cap::SubstrateHarnessCapParams;
pub use cap::SubstrateHarnessCapability;
pub use unsupported_cap::UnsupportedSubstrateHarnessCapability;
