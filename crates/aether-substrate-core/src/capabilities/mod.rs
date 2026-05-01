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
pub use audio::{AUDIO_SINK_NAME, AudioCapability, AudioConfig, AudioRunning};
pub use handle::{HANDLE_SINK_NAME, HandleCapability, HandleRunning};
pub use io::{IO_SINK_NAME, IoCapability, IoRunning};
pub use log::{LOG_SINK_NAME, LogCapability, LogRunning};
pub use net::{NET_SINK_NAME, NetCapability, NetRunning};
#[cfg(feature = "render")]
pub use render::{
    CAMERA_SINK_NAME, RENDER_SINK_NAME, RenderCapability, RenderConfig, RenderHandles,
    RenderRunning,
};
