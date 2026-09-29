//! Text clipboard capability.
//!
//! `aether.clipboard` is a request/reply peripheral, separate from the
//! publish/subscribe `aether.window` input streams. Desktop composes
//! [`ClipboardCapability`] with the system backend, `SubstrateHarness` selects its
//! deterministic in-memory backend by default, and a chassis with no clipboard
//! composes none, so a dependent of [`ClipboardCapability`] is refused there.

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

#[cfg(feature = "runtime")]
mod runtime;
