//! A coordinator in trial mode writes its rows into a trial store, and the
//! capability ledger reads them back as trial (ADR-0184).
//!
//! The bug this catches: a benchmark run's cells are byte-identical in shape to
//! live ones — same reducer, same dispatch decisions, same study columns — so a
//! ledger that does not carry the class of the journal it folded renders
//! measurements of a replayed world as measurements of the estate's own work,
//! and a calibration edit argued from that table cannot tell which half it is
//! reading.
//!
//! The fixture cell *is* the trial cell: `github_backend = fixture` is exactly
//! what a calibration host runs, so this scenario boots the same coordinator a
//! benchmark run does and reads the same `/calibration` answer an operator
//! would.

#![allow(clippy::unwrap_used)]

use aether_bloomery::{StageId, StoreClass};
use aether_chassis_bloomery::store::OutstandingOrder;
use aether_data::wire::from_bytes;
use aether_harness_bloomery::{FixtureHarness, digest, passed};

fn stage_of(order: &OutstandingOrder) -> StageId {
    from_bytes(&order.stage).expect("a recorded order carries a StageId")
}

#[test]
fn a_trial_coordinator_ledgers_its_rows_as_trial() {
    let mut harness = FixtureHarness::start("trial-store-ledger");
    harness.seal_member("wp", digest(0x51));

    let base = harness.await_order();
    assert_eq!(stage_of(&base), StageId::BaseVerify);
    harness.upload_admitted(&passed(&base));

    // Construct is a model lane, so the fold has a cell to report: only those
    // enter the capability ledger, and a scenario that stopped at the
    // mechanical base verify would assert the class of an empty table.
    let construct = harness.await_order();
    assert_eq!(stage_of(&construct), StageId::Construct);

    let document = harness.calibration();
    assert_eq!(
        document.ledger.store,
        StoreClass::Trial,
        "a fixture-backend coordinator's ledger must declare itself trial: {:?}",
        document.ledger,
    );
    assert!(!document.ledger.cells.is_empty(), "the construct dispatch must have entered a cell");
    assert_eq!(harness.journal_class(), StoreClass::Trial, "the journal on disk carries the same class");
}
