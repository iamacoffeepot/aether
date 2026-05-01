//! Native capabilities (ADR-0070). Each submodule extracts one of
//! the substrate's chassis-policy sinks into a [`Capability`]
//! implementation owning its mailbox(es), state, dispatcher thread,
//! and lifecycle.
//!
//! Phasing:
//! - Phase 2 (this PR): `handle` — least-state validator of the
//!   trait shape end-to-end.
//! - Phase 3: `log`, `io`, `net`, `audio` (gated by `audio` feature),
//!   `render` + `camera` (gated by `render` feature). One PR per
//!   sink.
//! - Phase 4–5: `HubClientCapability` and `HubServerCapability` land
//!   in the new `aether-hub` crate, not here, so the substrate ends with
//!   zero hub knowledge.
//!
//! [`Capability`]: crate::capability::Capability

pub mod handle;
pub use handle::{HANDLE_SINK_NAME, HandleCapability, HandleRunning};
