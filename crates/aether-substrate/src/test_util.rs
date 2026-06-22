//! Substrate-internal test helpers.
//!
//! The cross-crate canonical `(Arc<Registry>, Arc<Mailer>)` fixture lives
//! at `aether_capabilities::test_chassis::fresh_substrate`. Substrate
//! itself can't depend on `aether-capabilities` (wrong direction of the
//! dep graph), so the same minimal seed lives here for substrate's own
//! `#[cfg(test)] mod tests` modules. Intentionally narrower than the
//! capabilities version: no kind descriptors registered, no outbound
//! wired — substrate-internal tests don't exercise descriptor lookup or
//! the unknown-mailbox bubble-up path (ADR-0037).
//!
//! Folded out of three identical `fn fresh_substrate()` copies under
//! `actor/native/binding.rs`, `actor/native/spawn_thread.rs`, and
//! `chassis/builder.rs` (Qodana `DuplicatedCode` notice).

use std::sync::Arc;

use crate::mail::mailer::Mailer;
use crate::mail::registry::Registry;

/// Build the `(Arc<Registry>, Arc<Mailer>)` seed substrate-internal
/// tests feed to `Builder::<...>::new` and `NativeBinding::new_for_test`.
pub fn fresh_substrate() -> (Arc<Registry>, Arc<Mailer>) {
    let registry = Arc::new(Registry::new());
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
    (registry, mailer)
}
