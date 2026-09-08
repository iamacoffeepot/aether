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

use aether_actor::{MailboxForward, actor};

/// Addressing identity for the system or in-memory `aether.clipboard` actor.
#[actor(singleton, root)]
pub struct ClipboardCapability;

/// Sender-side convenience methods for the text clipboard requests.
///
/// Blanket-impl'd over [`MailboxForward<ClipboardCapability>`], so it reaches
/// every handle `ctx.actor::<ClipboardCapability>()` can return — the wasm and
/// native mailboxes and their typed request-context adapters alike — from one
/// set of bodies.
pub trait ClipboardMailboxExt: MailboxForward<ClipboardCapability> {
    /// Request the current clipboard text.
    fn get_text(&self) {
        self.forward(&GetClipboardText);
    }

    /// Replace the current clipboard text.
    fn set_text(&self, text: &str) {
        self.forward(&SetClipboardText { text: text.to_owned() });
    }
}

impl<T: MailboxForward<ClipboardCapability>> ClipboardMailboxExt for T {}

// The headless companion's identity lives in `headless.rs` (always-on, like
// the [`ClipboardCapability`] ZST above); its runtime half is the nested
// `runtime::headless` module, covered by the `mod runtime;` gate.
mod headless;
pub use headless::HeadlessClipboardCapability;

#[cfg(feature = "runtime")]
mod runtime;
