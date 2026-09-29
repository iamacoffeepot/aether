//! The `aether.bloomery.workspace` actor's boot, composed on a
//! [`SubstrateHarness`] as a chassis composes it. Its imports and runs are
//! driven on the shipped Bloomery composition, where a unit's journal is the
//! source they read and stage through (`aether-chassis-bloomery`'s workspace
//! scenarios).

use aether_bloomery_workspace::{WorkspaceCapability, WorkspaceConfig};
use aether_harness_substrate::SubstrateHarness;

#[test]
fn a_zero_max_deadline_refuses_boot_naming_it() {
    // Catches the ceiling left unvalidated: a zero ceiling would clamp every
    // run's deadline to nothing, so every run would answer Exhausted(Time).
    let config = WorkspaceConfig { max_deadline_millis: 0, ..WorkspaceConfig::default() };
    let message = SubstrateHarness::builder()
        .with_actor_configured::<WorkspaceCapability>((), config)
        .build()
        .err()
        .expect("boot with a zero max deadline must fail")
        .to_string();

    assert!(message.contains("AETHER_WORKSPACE_MAX_DEADLINE_MILLIS"), "the refusal names the knob: {message}");
}
