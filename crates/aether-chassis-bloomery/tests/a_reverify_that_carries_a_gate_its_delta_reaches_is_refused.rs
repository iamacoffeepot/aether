//! A delta-confirm that claims to have carried a gate its own delta reaches is
//! refused as an incomplete receipt.
//!
//! This is the whole of the ADR-0200 2026-09-15 amendment's soundness. A receipt
//! carrying a gate the delta *can* affect is claiming a verdict nothing computed
//! over this tree, and admitting it would integrate a candidate on the strength
//! of a check that never ran. No unit test over the classifier reaches it — the
//! classifier decides what to run, and this decides what the ledger will take
//! from a lane whose classification disagrees with the shared table.
//!
//! The refusal has to land as a *host fault* rather than a candidate failure,
//! and both halves matter. The candidate did nothing wrong, so it must not spend
//! a repair roll and must not be routed to Refine with nothing to repair; it
//! owes another Verify, because the receipt it needs has not been produced yet.
//! A refusal that charged the member would be worse than not refusing at all.
//!
//! The sibling scenario carries soundly and integrates. Neither proves anything
//! alone: a door that refuses everything passes this one on its own.

mod common;

use aether_bloomery::{DeltaClass, StageId};
use aether_harness_bloomery::FixtureHarness;

use common::carry::{carrying, to_the_delta_confirm};

#[test]
fn a_reverify_that_carries_a_gate_its_delta_reaches_is_refused() {
    let mut harness = FixtureHarness::start_refining("verify-carry-unsound");
    let (bloom, first, _repaired, confirm) = to_the_delta_confirm(&mut harness);

    // The same claim the sibling makes soundly, over a `code` delta — which
    // reaches every gate, so the claim refutes itself against the table.
    harness.upload_admitted(&carrying(&confirm, first.tree, &[DeltaClass::Code]));

    let member = harness.bloom(bloom).members[0].clone();
    let fault =
        member.host_fault.expect("an unsound carry is refused as an incomplete receipt, never admitted as a pass");
    assert!(
        fault.findings.contains("verify.test"),
        "the refusal names the gate that was carried and should not have been: {}",
        fault.findings,
    );
    assert!(member.wedge.is_none(), "a refused receipt is the host's fault, so the member is not stopped");
    assert_eq!(
        member.resolution, None,
        "the refused pass must not resolve the member: the suite verdict it claims was never computed over this tree",
    );
    assert_eq!(
        member.cursor.as_ref().map(|cursor| cursor.stage),
        Some(StageId::Verify),
        "the member owes another Verify, not a Refine: its candidate has nothing to repair",
    );
}
