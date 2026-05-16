//! Shared `TestChassis` fixture for unit tests inside `aether-mcp`.
//!
//! `tools.rs`'s `#[cfg(test)] mod tests` boots a hub-shaped passive
//! chassis through [`Builder`](aether_substrate::chassis::builder::Builder)
//! against a no-op chassis declaration. This module hosts the single
//! canonical declaration so test modules `use crate::test_chassis::TestChassis;`
//! instead of inlining the same 8-line `impl Chassis for TestChassis` block.
//!
//! Filed by issue 802. Local sibling to `aether-capabilities`'s
//! `test_chassis` module (issue 785); the cap-side copy is
//! `#[cfg(test)] pub(crate)` and not reachable cross-crate, so each
//! workspace consumer that needs the fixture keeps its own one-line
//! declaration close at hand.

use aether_substrate::chassis::Chassis;
use aether_substrate::chassis::builder::{BuiltChassis, NeverDriver};
use aether_substrate::chassis::error::BootError;

/// Canonical test chassis. `build()` is unreachable — every consumer
/// drives the chassis through `Builder::<TestChassis>::new(...)` directly
/// rather than going through `TestChassis::build(())`.
pub struct TestChassis;

impl Chassis for TestChassis {
    const PROFILE: &'static str = "test";
    type Driver = NeverDriver;
    type Env = ();
    fn build(_env: Self::Env) -> Result<BuiltChassis<Self>, BootError> {
        unreachable!("TestChassis is driven by Builder::new directly in unit tests")
    }
}
