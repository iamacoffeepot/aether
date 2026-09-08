//! The other half of the trial store (ADR-0184): a coordinator that nobody put
//! in trial mode keeps writing live rows, and its ledger keeps saying so.
//!
//! The bug this catches is the mirror of its sibling scenario's — a class that
//! defaulted to `trial`, or leaked from the fixture that every test binary
//! links, would relabel real operation as benchmark data and drop it out of the
//! very table calibration is read from. The local-authority cell runs the
//! production GitHub backend against a real repository, which is what makes it
//! the live side of the pair.

#![allow(clippy::unwrap_used)]

use aether_bloomery::StoreClass;
use aether_bloomery::testing::digest;
use aether_harness_bloomery::BloomeryHarness;

#[test]
fn a_live_coordinator_ledgers_its_rows_as_live() {
    let mut harness = BloomeryHarness::start();
    let bloom = harness.seal_member("wp", digest(0x51));
    harness.run_until(|harness| !harness.bloom(bloom).members.is_empty(), 10);

    assert_eq!(
        harness.calibration().ledger.store,
        StoreClass::Live,
        "a coordinator on the production backend measures live operation",
    );
    assert_eq!(harness.journal_class(), StoreClass::Live, "the journal on disk carries the same class");
}
