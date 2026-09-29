//! Issue 7154: a load is a publish then a spawn (ADR-0241 §9), so a load of a
//! module that succeeds the one publishing its namespaces republishes every
//! live instance of them as one group (§7) before it spawns its own.
//!
//! The fixtures are the #7109 group pair: v2's `test.republish.gate` adds a
//! `GateProbe` row. Skipped when the fixture wasm hasn't been built
//! (`require_wasm`); CI pre-builds it and sets `AETHER_REQUIRE_RUNTIME=1` so
//! the skip becomes a hard panic there.

use std::fs;

use aether_data::Kind;
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::LoadComponent;
use aether_test_fixtures_kinds::{GateProbe, GateQuery, GateQueryResult};

const GATE: &str = "test.republish.gate";

fn load_gate(wasm: &[u8], key: &str) -> LoadComponent {
    LoadComponent { wasm: wasm.to_vec(), name: Some(key.to_owned()), config: Vec::new(), export: Some(GATE.to_owned()) }
}

#[test]
fn a_successor_load_republishes_every_live_instance_before_it_spawns() {
    // Catches: a load whose module succeeds a published one repointing the
    // publication table at the successor while the live gate keeps running
    // the predecessor, which has no `GateProbe` row and drops the probe.
    let read = |stem: &str| require_wasm(stem).map(|path| fs::read(path).expect("read fixture wasm"));
    let (Some(v1), Some(v2)) = (read("republish_group_v1"), read("republish_group_v2")) else {
        return;
    };
    let mut harness = SubstrateHarness::builder().with_component_host().size(64, 48).build().expect("boot");
    let (first, _) = harness.load_any(&load_gate(&v1, "a")).unwrap_or_else(|error| panic!("load gate a: {error}"));

    let (second, _) = harness.load_any(&load_gate(&v2, "b")).unwrap_or_else(|error| panic!("load gate b: {error}"));

    assert!(harness.accepts(first, GateProbe::ID), "the live gate moved to the successor with the load");
    assert!(harness.accepts(second, GateProbe::ID), "the loaded gate runs the successor");
    harness.execute(vec![("probe", HarnessOp::send_and_settle(first, &GateProbe { seq: 3 }))]).expect("probe gate a");
    let report = harness
        .execute(vec![("query", HarnessOp::send_and_await_reply(first, &GateQuery))])
        .expect("query gate a")
        .reply::<GateQueryResult>("query")
        .expect("decode GateQueryResult");
    assert_eq!(report.seqs, vec![3], "the moved gate's successor handles the probe");
}
