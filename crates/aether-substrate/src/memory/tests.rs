use std::sync::Arc;

use super::*;

/// A charge that grows, shrinks, and drops returns its gauge to what the
/// other charges hold. The bug this catches is a resized or retired entry's
/// bytes staying in the report.
#[test]
fn a_resized_then_dropped_charge_returns_its_bytes() {
    let ledger = Arc::new(MemoryLedger::default());
    let gauge = ledger.gauge(MailboxId(7), "geometry");
    let kept = gauge.charge(10);

    let mut charge = gauge.charge(100);
    charge.resize(260);
    assert_eq!(gauge.bytes(), 270);
    charge.resize(40);
    assert_eq!(gauge.bytes(), 50);

    drop(charge);
    assert_eq!(gauge.bytes(), kept.bytes());
    assert_eq!(ledger.rows()[0].bytes, 10, "the ledger reads the gauge's own count");
}

/// A dropped gauge leaves no row, and takes only its own: a second gauge the
/// same owner holds stays listed. The bug this catches is a dead actor's row
/// staying in the report, or a drop removing a sibling's row.
#[test]
fn a_dropped_gauge_leaves_no_row() {
    let ledger = Arc::new(MemoryLedger::default());
    let textures = ledger.gauge(MailboxId(7), "textures");
    let geometry = ledger.gauge(MailboxId(7), "geometry");
    let outliving = textures.charge(64);

    drop(textures);
    let labels: Vec<_> = ledger.rows().iter().map(|row| row.label).collect();
    assert_eq!(labels, ["geometry"]);

    // A charge that outlives its gauge still drops without touching a row.
    drop(outliving);
    assert_eq!(geometry.bytes(), 0);
}
