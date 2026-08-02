//! Options staged on the builder: a config override with no composed member
//! aborts the boot, and the worker-count knob clamps and survives the
//! type-state transition into a driven builder.

use super::support::{DrivenTestChassis, RanDriver, StubLog};
use crate::chassis::builder::Builder;
use crate::testing::{TestChassis, bare_substrate};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

/// ADR-0156 §5: an override staged for a type no composed member declares as
/// its `Config` is a hard boot error naming the type — the
/// staged-but-never-composed coherence guard (defense-in-depth on the
/// `ConfigSources` bulk path). The paired [`Builder::with_actor_configured`]
/// makes an orphan *unconstructable* through the public builder API — an
/// override always composes its actor — so the orphan case is reachable only by
/// staging directly into a `ConfigSources` and handing it over via
/// `with_config_sources` (the chassis's argv/file bulk path), which is what
/// this test does. A composed cap with no orphan override boots.
#[test]
fn build_passive_rejects_staged_but_never_composed_override() {
    use crate::config::ConfigSources;

    // A distinctive marker type that is never any cap's `Config`.
    #[derive(Debug)]
    struct OrphanKnob;

    let mut orphan_sources = ConfigSources::hermetic();
    orphan_sources.set_override(OrphanKnob);

    let (registry, mailer) = bare_substrate();
    let err = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_config_sources(orphan_sources)
        .with_actor::<StubLog>(())
        .build_passive()
        .expect_err("a staged-but-never-composed override must abort boot");
    assert!(
        format!("{err:?}").contains("OrphanKnob"),
        "the orphan-override error must name the offending type, got {err:?}",
    );

    // The paired form composes + stages coherently, so it boots.
    let (registry, mailer) = bare_substrate();
    Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<StubLog>(())
        .build_passive()
        .expect("a composed cap with no orphan override boots");
}

/// Issue 745: `Some(0)` clamps to 1 since the pool requires at
/// least one worker.
#[test]
fn with_workers_some_zero_clamps_to_one() {
    let (registry, mailer) = bare_substrate();
    let builder = Builder::<TestChassis>::new(registry, mailer).with_workers(Some(0));
    assert_eq!(builder.workers, Some(1));
}

/// Issue 745: the override survives the type-state transition into
/// [`HasDriver`](crate::chassis::builder::HasDriver) so chassis mains can call `.with_workers(...)`
/// either before or after `.driver(_)`.
#[test]
fn with_workers_survives_driver_transition() {
    let (registry, mailer) = bare_substrate();
    let ran = Arc::new(AtomicBool::new(false));
    let builder = Builder::<DrivenTestChassis<RanDriver>>::new(registry, mailer)
        .with_workers(Some(3))
        .driver(RanDriver { ran: Arc::clone(&ran) });
    assert_eq!(builder.workers, Some(3));
}
