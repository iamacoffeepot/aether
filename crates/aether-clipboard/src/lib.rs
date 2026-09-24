//! Text clipboard capability.
//!
//! `aether.clipboard` is a request/reply peripheral, separate from the
//! publish/subscribe `aether.window` input streams. Desktop composes
//! [`ClipboardCapability`] with the system backend, `SubstrateHarness` selects its
//! deterministic in-memory backend by default, and unavailable chassis compose
//! [`HeadlessClipboardCapability`] so both requests fail fast.

#![forbid(unsafe_code)]

pub mod kinds;
pub use kinds::*;

#[cfg(feature = "runtime")]
mod config;
#[cfg(feature = "runtime")]
pub use config::ClipboardParams;

use aether_actor::actor;

/// Addressing identity for the system or in-memory `aether.clipboard` actor.
#[actor(singleton, root)]
pub struct ClipboardCapability;

// The headless companion's identity lives in `headless.rs` (always-on, like
// the [`ClipboardCapability`] ZST above); its runtime half is the nested
// `runtime::headless` module, covered by the `mod runtime;` gate.
mod headless;
pub use headless::HeadlessClipboardCapability;

#[cfg(feature = "runtime")]
mod runtime;
