//! A base verify whose fan-out reached no verdict holds the day exactly as a
//! red one does, rather than being refused back out of its stage.
//!
//! Pre-fix: `verify.base` ran the suppression scan with no `--base`, the scan
//! refused on a lane worktree carrying no `origin/main`, the umbrella stamped
//! `environment`, and intake answered the resulting `ExecutorFault` with
//! `ExecutorFaultOutOfStage(BaseVerify)` — so every sealed member stayed
//! withheld forever and the view said nothing about why (#5384).

#![allow(clippy::unwrap_used)]

use aether_bloomery::{Outcome, StageId};
use aether_chassis_bloomery::store::OutstandingOrder;
use aether_data::wire::from_bytes;
use aether_harness_bloomery::{FixtureHarness, Oracle, digest, faulted};

fn stage_of(order: &OutstandingOrder) -> StageId {
    from_bytes(&order.stage).expect("a recorded order carries a StageId")
}

#[test]
fn a_faulted_base_holds_the_day_rather_than_being_refused() {
    let mut harness = FixtureHarness::start("faulted-base-holds-the-day");
    let (_bloom, outcome) = harness.try_seal(&[("wp", digest(0x52))]);
    assert!(matches!(outcome, Outcome::Sealed(_)), "got {outcome:?}");

    let order = harness.await_order();
    assert_eq!(stage_of(&order), StageId::BaseVerify);
    // `upload_admitted` panics on a refusal, so this is the assertion that the
    // fault is admitted at all rather than bounced out of the stage.
    harness.upload_admitted(&faulted(&order));

    for _ in 0..10 {
        harness.tick();
        assert!(
            harness.orders().iter().all(|order| stage_of(order) != StageId::Construct),
            "a base nobody could judge must not dispatch construct",
        );
    }

    assert!(
        harness.view().base_alert.is_some(),
        "a base the fan-out could not judge raises the same day-level alert a red one does",
    );

    Oracle::check(&harness.view(), harness.doctor().as_ref(), &harness.outstanding())
        .unwrap_or_else(|violation| panic!("{violation}"));
}
