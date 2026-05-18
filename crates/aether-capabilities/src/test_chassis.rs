//! Shared `TestChassis` fixture for unit tests across `aether-capabilities`.
//!
//! Every cap's `#[cfg(test)] mod tests` exercises its `init` / handlers
//! by booting a real [`Builder`](aether_substrate::chassis::builder::Builder)
//! against a no-op chassis declaration. Pre-extraction every site copied
//! the same 8-line `impl Chassis for TestChassis` block; this module is
//! the single canonical declaration so test modules just
//! `use crate::test_chassis::TestChassis;` and address it by the typename
//! the builder expects.
//!
//! Filed by issue 785. The `fresh_substrate` helper extracted by issue
//! 786 lives here too — same six sites all wanted the same
//! `(Arc<Registry>, Arc<Mailer>)` seed for `Builder::new`.

use std::sync::Arc;

use aether_substrate::chassis::Chassis;
use aether_substrate::chassis::builder::{BuiltChassis, NeverDriver};
use aether_substrate::chassis::error::BootError;
use aether_substrate::handle_store::HandleStore;
use aether_substrate::mail::mailer::Mailer;
use aether_substrate::mail::outbound::HubOutbound;
use aether_substrate::mail::registry::Registry;

/// Canonical test chassis. `build()` is unreachable — every consumer
/// drives the chassis through `Builder::<TestChassis>::new(...)` directly
/// rather than going through `TestChassis::build(())`.
pub struct TestChassis;

//noinspection DuplicatedCode
impl Chassis for TestChassis {
    const PROFILE: &'static str = "test";
    type Driver = NeverDriver;
    type Env = ();
    fn build(_env: Self::Env) -> Result<BuiltChassis<Self>, BootError> {
        unreachable!("TestChassis is driven by Builder::new directly in unit tests")
    }
}

/// Build the `(Arc<Registry>, Arc<Mailer>)` seed every cap test feeds to
/// `Builder::<TestChassis>::new`. The registry is pre-populated with the
/// substrate kind descriptors so tests can address built-in kinds by id
/// without re-registering; the mailer carries a loopback `HubOutbound`
/// (rx dropped) so the unknown-mailbox bubble-up path (ADR-0037) is
/// wired but inert — tests that never hit it (audio, fs, http handler
/// paths) see no behavioural difference, and tests that do hit it
/// (rpc, engine proxy) get the connected backend they need.
pub fn fresh_substrate() -> (Arc<Registry>, Arc<Mailer>) {
    let registry = Arc::new(Registry::new());
    for d in aether_kinds::descriptors::all() {
        let _ = registry.register_kind_with_descriptor(d);
    }
    let (outbound, _rx) = HubOutbound::attached_loopback();
    let store = Arc::new(HandleStore::new(1024 * 1024));
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry), store).with_outbound(outbound));
    (registry, mailer)
}
