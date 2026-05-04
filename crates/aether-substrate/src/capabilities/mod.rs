//! Native capabilities (ADR-0070). Each submodule extracts one of
//! the substrate's chassis-policy sinks into a [`Capability`]
//! implementation owning its mailbox(es), state, dispatcher thread,
//! and lifecycle.
//!
//! Phasing:
//! - Phase 2: `handle` — least-state validator of the trait shape
//!   end-to-end. Universal (shared by every chassis); booted by
//!   [`crate::SubstrateBoot::build`].
//! - Phase 3: `log` (this PR), `io`, `net`, `audio` (gated by `audio`
//!   feature), `render` + `camera` (gated by `render` feature). All
//!   chassis-conditional; chassis mains call
//!   [`crate::SubstrateBoot::add_capability`] after boot.
//! - Phase 4–5: `HubClientCapability` and `HubServerDriverCapability` land
//!   in the new `aether-hub` crate, not here, so the substrate ends
//!   with zero hub knowledge.
//!
//! [`Capability`]: crate::capability::Capability

#[cfg(feature = "audio")]
pub mod audio;
pub mod handle;
pub mod io;
pub mod log;
pub mod net;
#[cfg(feature = "render")]
pub mod render;
#[cfg(feature = "audio")]
pub use audio::{AudioConfig, CpalAudioBackend};
pub use handle::HandleStoreBackend;
pub use io::IoAdapterBackend;
pub use log::LogTracingBackend;
pub use net::NetAdapterBackend;
#[cfg(feature = "render")]
pub use render::{RenderCapability, RenderConfig, RenderGpu, RenderHandles};
