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
use aether_data::{Kind, KindId};
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::{CostTailResult, DropComponent, DropResult, LoadComponent, ReplaceComponent, ReplaceResult, Tick};
use aether_test_fixtures_kinds::{Bump, InlineProbe, UnsubscribeKeys};

// Pin the fixture rlib so its descriptor `inventory::submit!` entries
// land in this test binary (mirrors `cap_registry.rs`).
#[allow(unused_imports)]
use aether_test_fixtures_kinds as _;

fn load_probe(harness: &mut SubstrateHarness, wasm_path: &Path) -> ErasedActorRef {
    let wasm = fs::read(wasm_path).expect("read fixture wasm");
    harness
        .load_any(&LoadComponent { wasm, name: Some("cost-probe".to_owned()), config: Vec::new(), export: None })
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
/// guest drops its rows before re-seeding those arms. A sync that seeded only
/// the guest's kinds would leave the arms that outlive a drop unmeasured, and
/// one that skipped the drop would leave a dropped guest's handlers measured.
#[test]
fn replace_and_drop_keep_framework_arms_measured() {
    let Some(bundle_path) = require_wasm("aether_test_fixtures_bundle") else {
        return;
    };
    let wasm = fs::read(&bundle_path).expect("read fixture wasm");
    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    let base = LoadComponent {
        wasm: wasm.clone(),
        name: Some("swappable".to_owned()),
        config: Vec::new(),
        export: Some("test.contract.base".to_owned()),
    };
    let (swappable, path) =
        harness.load_any(&base).unwrap_or_else(|error| panic!("load_component(swappable): {error}"));
    let host = harness.actor_ref::<ComponentHostCapability>();

    let replace = ReplaceComponent {
        target: path.clone(),
        wasm,
        drain_timeout_ms: None,
        config: Vec::new(),
        export: Some("test.contract.extended".to_owned()),
    };
    let swapped =
        harness.execute(vec![("swap", HarnessOp::send_and_await_reply(&host, &replace))]).expect("replace sequence");
    if let ReplaceResult::Err { error } = swapped.reply::<ReplaceResult>("swap").expect("decode ReplaceResult") {
        panic!("replace_component: {error}");
    }
    let replaced = cost_kinds(&harness, swappable);
    assert!(replaced.contains(&ReplaceComponent::ID), "the replace arm stays measured across a replace");
    assert!(replaced.contains(&InlineProbe::ID), "the replacement's new handler is measured");

    let dropped = harness
        .execute(vec![("drop", HarnessOp::send_and_await_reply(&host, &DropComponent { target: path }))])
        .expect("drop sequence");
    if let DropResult::Err { error } = dropped.reply::<DropResult>("drop").expect("decode DropResult") {
        panic!("drop_component: {error}");
    }
    let released = cost_kinds(&harness, swappable);
    assert!(released.contains(&ReplaceComponent::ID), "an empty slot still measures the replace arm that refills it");
    assert!(released.contains(&DropComponent::ID), "an empty slot still measures the drop arm");
    assert!(!released.contains(&Bump::ID), "a dropped guest's handler leaves the table");
    assert!(!released.contains(&InlineProbe::ID), "a dropped guest's handler leaves the table");
}
