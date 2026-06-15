//! Local `TestChassis` fixture for the `aether.trajectory` recorder cap's
//! unit tests.
//!
//! Sibling to `aether-capabilities`'s `test_chassis` module (issue 785)
//! and `aether-mcp`'s (issue 802): the cap-side copies are `#[cfg(test)]
//! pub(crate)` and not reachable cross-crate, so each workspace consumer
//! that needs the fixture keeps its own declaration close at hand. Only
//! the two items the recorder cap's tests reach for live here —
//! [`TestChassis`] and [`boot_test_chassis_with`].

use std::sync::Arc;

use aether_substrate::actor::native::{NativeActor, NativeDispatch};
use aether_substrate::chassis::Chassis;
use aether_substrate::chassis::builder::{Builder, BuiltChassis, NeverDriver, PassiveChassis};
use aether_substrate::chassis::error::BootError;
use aether_substrate::mail::mailer::Mailer;
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

/// Boot a `TestChassis` carrying exactly one cap `A` with `config`. The
/// minimal-boot path the recorder cap's dispatcher-thread test reaches for.
pub fn boot_test_chassis_with<A>(
    registry: &Arc<Registry>,
    mailer: &Arc<Mailer>,
    config: A::Config,
) -> PassiveChassis<TestChassis>
where
    A: NativeActor + NativeDispatch,
{
    Builder::<TestChassis>::new(Arc::clone(registry), Arc::clone(mailer))
        .with_actor::<A>(config)
        .build_passive()
        .expect("test chassis boots")
}
