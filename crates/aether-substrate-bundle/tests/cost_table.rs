//! iamacoffeepot/aether#1128: the per-handler cost EWMA, exercised
//! through a real component-load lifecycle on a `TestBench`.
//!
//! Guards the redesign invariant that `WasmTrampoline::init` seeds the
//! per-handler cost cells from the guest's declared handler set, under
//! the spawn path's `with_stamped(&slots, …)` — so a loaded component's
//! handlers are measurable with no lazy first-dispatch pull. The unit
//! tests cover the EWMA + table mechanics in isolation
//! (`aether_actor::cost`, `aether_substrate::mail::cost`); this is the
//! load-path integration guard, and in particular the one that proves
//! the per-actor `CostCells` cache was actually stamped at construction:
//! a fold only records if the cache holds the cell, so a nonzero sample
//! count after dispatch is end-to-end evidence the stamp ran.
//!
//! Skipped when no wgpu adapter / no pre-built component wasm (same gates
//! as the other bench integration tests).

use std::fs;
use std::path::Path;

use aether_actor::Actor;
use aether_capabilities::ComponentHostCapability;
use aether_data::{Kind, MailboxId};
use aether_kinds::{CostTail, CostTailResult, LoadComponent, LoadResult, Tick};
use aether_substrate_bundle::test_bench::{BenchOp, TestBench, test_helpers::require_runtime};
use aether_test_fixtures::SetRender;

// Pin the fixture rlib so its descriptor `inventory::submit!` entries
// land in this test binary (mirrors `cap_registry.rs`).
#[allow(unused_imports)]
use aether_test_fixtures as _;

fn load_probe(bench: &mut TestBench, wasm_path: &Path) -> MailboxId {
    let wasm = fs::read(wasm_path).expect("read fixture wasm");
    let loaded = bench
        .execute(vec![(
            "load",
            BenchOp::send_and_await(
                ComponentHostCapability::NAMESPACE,
                &LoadComponent {
                    wasm,
                    name: Some("cost-probe".to_owned()),
                },
            ),
        )])
        .expect("load sequence");
    match loaded
        .reply::<LoadResult>("load")
        .expect("decode LoadResult")
    {
        LoadResult::Ok { mailbox_id, .. } => mailbox_id,
        LoadResult::Err { error } => panic!("load_component: {error}"),
    }
}

/// `WasmTrampoline::init` seeds a neutral cost cell for every kind the
/// guest declares a `#[handler]` for; advancing the platform dispatches
/// the probe's `Tick` handler (it subscribes in `wire`), and each fold
/// reaches the cell through the init-seeded per-actor cache. End-to-end
/// proof of the construction-time seed + the lock-free fold path, on a
/// real component — the path the in-crate unit tests can only stub.
#[test]
fn init_seeds_cells_and_dispatch_folds() {
    let Some(wasm_path) = require_runtime("probe") else {
        return;
    };
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    let mbox = load_probe(&mut bench, &wasm_path);

    // At construction, before any dispatch: both declared handlers
    // (`Tick`, `SetRender`) are seeded at the neutral seed (`samples =
    // 0`) — the known-but-unrun state. If `init`'s seed had not run, the
    // table would hold no rows for this mailbox.
    {
        let CostTailResult::Ok { rows } = bench.cost_table().tail(mbox, &CostTail { kind: None })
        else {
            panic!("expected Ok");
        };
        let tick = rows
            .iter()
            .find(|r| r.kind_id == Tick::ID)
            .expect("Tick handler cell seeded at init");
        assert_eq!(tick.samples, 0, "neutral seed before any dispatch");
        assert!(
            rows.iter().any(|r| r.kind_id == SetRender::ID),
            "SetRender handler cell seeded at init",
        );
    }

    // Advance 3 ticks → the probe's on_tick dispatches 3× → 3 folds into
    // the Tick cell. A nonzero count proves the per-actor cache was
    // stamped at construction (the redesign's load-bearing claim) and the
    // fold reached it. `SetRender` is never dispatched, so it stays at the
    // neutral seed.
    bench
        .execute(vec![("advance", BenchOp::advance(3))])
        .expect("advance 3");

    let CostTailResult::Ok { rows } = bench.cost_table().tail(mbox, &CostTail { kind: None })
    else {
        panic!("expected Ok");
    };
    let tick = rows
        .iter()
        .find(|r| r.kind_id == Tick::ID)
        .expect("Tick handler cell present");
    assert_eq!(
        tick.samples, 3,
        "three Tick dispatches folded into the init-seeded cell",
    );
    let set_render = rows
        .iter()
        .find(|r| r.kind_id == SetRender::ID)
        .expect("SetRender handler cell present");
    assert_eq!(
        set_render.samples, 0,
        "an un-dispatched handler stays at the neutral seed",
    );
}
