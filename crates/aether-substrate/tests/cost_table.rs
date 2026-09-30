//! iamacoffeepot/aether#1128: the per-handler cost EWMA, exercised
//! through a real component-load lifecycle on a `SubstrateHarness`.
//!
//! Guards the redesign invariant that `WasmTrampoline::init` seeds the
//! per-handler cost cells from the guest's declared handler set, under
//! the spawn path's `with_stamped(&slots, …)` — so a loaded component's
//! handlers are measurable with no lazy first-dispatch pull. The unit
//! tests cover the EWMA + table mechanics in isolation
//! (`aether_substrate::mail::cost`); this is the
//! load-path integration guard, and in particular the one that proves
//! the per-actor `CostCells` cache was actually stamped at construction:
//! a fold only records if the cache holds the cell, so a nonzero sample
//! count after dispatch is end-to-end evidence the stamp ran.
//!
//! Skipped when the component wasm hasn't been pre-built (the harness
//! composes no render cap, so there is no wgpu gate).

use std::fs;
use std::path::Path;

use aether_actor::ErasedActorRef;
use aether_component::ComponentHostCapability;
use aether_component::component::Prepare;
use aether_data::{Kind, KindId};
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::{CostTailResult, DropComponent, DropResult, LoadComponent, Tick};
use aether_test_fixtures_kinds::{GateProbe, GateQuery, UnsubscribeKeys};

// Pin the fixture rlib so its descriptor `inventory::submit!` entries
// land in this test binary (mirrors `cap_registry.rs`).
#[allow(unused_imports)]
use aether_test_fixtures_kinds as _;

fn load_probe(harness: &mut SubstrateHarness, wasm_path: &Path) -> ErasedActorRef {
    let wasm = fs::read(wasm_path).expect("read fixture wasm");
    harness
        .load_any(&LoadComponent { wasm, name: None, config: Vec::new(), export: Some("test.probe".to_owned()) })
        .map_or_else(|error| panic!("load_component: {error}"), |(probe, _)| probe)
}

/// `WasmTrampoline::init` seeds a neutral cost cell for every kind the
/// guest declares a `#[handler]` for; advancing the platform dispatches
/// the probe's `Tick` handler (it subscribes in `wire`), and each fold
/// reaches the cell through the init-seeded per-actor cache. End-to-end
/// proof of the construction-time seed + the lock-free fold path, on a
/// real component — the path the in-crate unit tests can only stub.
#[test]
fn init_seeds_cells_and_dispatch_folds() {
    let Some(wasm_path) = require_wasm("aether_test_fixtures_bundle") else {
        return;
    };
    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    let probe = load_probe(&mut harness, &wasm_path);

    // At construction, before any dispatch: the declared handlers
    // (`Tick`, `UnsubscribeKeys`, …) are seeded at the neutral seed
    // (`samples = 0`) — the known-but-unrun state. If `init`'s seed had not run, the
    // table would hold no rows for this mailbox.
    {
        let CostTailResult::Ok { rows } = harness.actor_cost(probe) else {
            panic!("expected Ok");
        };
        let tick = rows.iter().find(|r| r.kind_id == Tick::ID).expect("Tick handler cell seeded at init");
        assert_eq!(tick.samples, 0, "neutral seed before any dispatch");
        assert!(rows.iter().any(|r| r.kind_id == UnsubscribeKeys::ID), "UnsubscribeKeys handler cell seeded at init");
    }

    // Advance 3 ticks → the probe's on_tick dispatches 3× → 3 folds into
    // the Tick cell. A nonzero count proves the per-actor cache was
    // stamped at construction (the redesign's load-bearing claim) and the
    // fold reached it. `UnsubscribeKeys` is never dispatched, so it stays at the
    // neutral seed.
    harness.execute(vec![("advance", HarnessOp::advance(3))]).expect("advance 3");

    let CostTailResult::Ok { rows } = harness.actor_cost(probe) else {
        panic!("expected Ok");
    };
    let tick = rows.iter().find(|r| r.kind_id == Tick::ID).expect("Tick handler cell present");
    assert_eq!(tick.samples, 3, "three Tick dispatches folded into the init-seeded cell");
    let unsubscribe =
        rows.iter().find(|r| r.kind_id == UnsubscribeKeys::ID).expect("UnsubscribeKeys handler cell present");
    assert_eq!(unsubscribe.samples, 0, "an un-dispatched handler stays at the neutral seed");
}

fn cost_kinds(harness: &SubstrateHarness, actor: ErasedActorRef) -> Vec<KindId> {
    let CostTailResult::Ok { rows } = harness.actor_cost(actor) else {
        panic!("expected Ok");
    };
    rows.iter().map(|row| row.kind_id).collect()
}

/// `NativeCtx::sync_guest` unions the trampoline's own measured framework arms
/// with the guest's handlers (iamacoffeepot/aether#4269), and releasing the
/// guest drops its rows. A sync that seeded only the guest's kinds would leave
/// the republish prepare arm unmeasured across a replace, and one that skipped
/// the release would leave a dropped guest's handlers measured.
#[test]
fn replace_keeps_framework_arms_measured_and_drop_releases_guest_rows() {
    let (Some(v1_path), Some(v2_path)) = (require_wasm("republish_group_v1"), require_wasm("republish_group_v2"))
    else {
        return;
    };
    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    let gate = LoadComponent {
        wasm: fs::read(&v1_path).expect("read fixture wasm"),
        name: Some("cost".to_owned()),
        config: Vec::new(),
        export: Some("test.republish.gate".to_owned()),
    };
    let (swappable, path) = harness.load_any(&gate).unwrap_or_else(|error| panic!("load_component(gate v1): {error}"));
    if let Err(error) = harness.publish(fs::read(&v2_path).expect("read fixture wasm")) {
        panic!("publish(gate v2): {error}");
    }
    let replaced = cost_kinds(&harness, swappable);
    assert!(replaced.contains(&Prepare::ID), "the republish prepare arm stays measured across a replace");
    assert!(replaced.contains(&GateProbe::ID), "the replacement's new handler is measured");

    let host = harness.actor_ref::<ComponentHostCapability>();
    let dropped = harness
        .execute(vec![("drop", HarnessOp::send_and_await_reply(&host, &DropComponent { target: path }))])
        .expect("drop sequence");
    if let DropResult::Err { error } = dropped.reply::<DropResult>("drop").expect("decode DropResult") {
        panic!("drop_component: {error}");
    }
    // The guest's rows leave in the drop handler, before its reply. The
    // closing trampoline's own rows leave with it later, so only the guest's
    // absence is ordered by the reply.
    let released = cost_kinds(&harness, swappable);
    assert!(!released.contains(&GateQuery::ID), "a dropped guest's handler leaves the table");
    assert!(!released.contains(&GateProbe::ID), "a dropped guest's handler leaves the table");
}
