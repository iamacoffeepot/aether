//! Text clipboard capability.
//!
//! `aether.clipboard` is a request/reply peripheral, separate from the
//! publish/subscribe `aether.input` streams. Desktop composes
//! [`ClipboardCapability`] with the system backend, `SubstrateHarness` selects its
//! deterministic in-memory backend by default, and unavailable chassis compose
//! [`HeadlessClipboardCapability`] so both requests fail fast.

pub mod kinds;
pub use kinds::*;

#[cfg(feature = "runtime")]
mod config;
#[cfg(feature = "runtime")]
pub use config::ClipboardParams;

use aether_actor::{WasmActorMailbox, actor};
#[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
use aether_substrate::actor::native::NativeActorMailbox;

/// Addressing identity for the system or in-memory `aether.clipboard` actor.
#[actor(singleton)]
pub struct ClipboardCapability;

/// Fail-fast `aether.clipboard` companion for chassis without a clipboard.
#[actor(singleton, headless_runtime)]
pub struct HeadlessClipboardCapability;

/// Sender-side convenience methods for the text clipboard requests.
pub trait ClipboardMailboxExt {
    /// Request the current clipboard text.
    fn get_text(&self);

    /// Replace the current clipboard text.
    fn set_text(&self, text: &str);
}

impl ClipboardMailboxExt for WasmActorMailbox<'_, ClipboardCapability> {
    fn get_text(&self) {
        self.send(&GetClipboardText);
    }

    fn set_text(&self, text: &str) {
        self.send(&SetClipboardText { text: text.to_owned() });
    }
}

#[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
impl ClipboardMailboxExt for NativeActorMailbox<'_, ClipboardCapability> {
    fn get_text(&self) {
        self.send(&GetClipboardText);
    }

    fn set_text(&self, text: &str) {
        self.send(&SetClipboardText { text: text.to_owned() });
    }
}

#[cfg(feature = "runtime")]
mod headless_runtime;
#[cfg(feature = "runtime")]
mod runtime;
