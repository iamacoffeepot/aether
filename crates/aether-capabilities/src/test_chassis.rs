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
//! Filed by issue 785.

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
