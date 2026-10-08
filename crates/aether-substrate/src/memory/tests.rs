use std::sync::Arc;

use super::*;

/// A charged resource that grows, shrinks, and drops returns its gauge to
/// what the other resources count. The bug this catches is a resized or
/// retired entry's bytes staying in the report.
#[test]
fn a_resized_then_dropped_resource_returns_its_bytes() {
    let ledger = Arc::new(MemoryLedger::default());
    let gauge = ledger.gauge(MailboxId(7), "geometry");
    let _kept = gauge.charged(10, ());

    let mut resource = gauge.charged(100, ());
    Charged::resize(&mut resource, 260);
    assert_eq!(gauge.bytes(), 270);
    Charged::resize(&mut resource, 40);
    assert_eq!(gauge.bytes(), 50);

    drop(resource);
    assert_eq!(gauge.bytes(), 10);
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
    let outliving = textures.charged(64, ());

    drop(textures);
    let labels: Vec<_> = ledger.rows().iter().map(|row| row.label).collect();
    assert_eq!(labels, ["geometry"]);

    // A resource that outlives its gauge still drops without touching a row.
    drop(outliving);
    assert_eq!(geometry.bytes(), 0);
}
