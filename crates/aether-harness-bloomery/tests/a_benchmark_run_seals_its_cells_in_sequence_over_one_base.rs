//! One golden task, two profile cells, sample size two: four benchmark blooms
//! sealed **one at a time** over the same base, and a trial ledger whose cells
//! are attributable to the `ModelOverride` each one sealed (ADR-0184, #4871).
//!
//! Two bugs this catches, and both are silent.
//!
//! The sequence: the reducer permits one active bloom, so a run that tried to
//! seal its cells together would seal the first and get
//! `SealRejected(ActiveBloomExists)` for the rest — a ledger that looks like a
//! working comparison with three cells quietly missing. Only watching each bloom
//! reach a terminal status and then finding its successor sealed shows the
//! sequence actually walked.
//!
//! The reset: a cell that replays over the tree the previous cell left behind is
//! measuring a different task. Mainline is moved between cells here to stand in
//! for the landing a cell would have produced, and the run is required to put it
//! back — that reset is the whole reason "the same task under four profiles" is
//! runnable against the fixture and unrunnable against the live repository.

#![allow(clippy::unwrap_used)]

use std::collections::BTreeSet;

use aether_bloomery::{StageId, StoreClass, WorkpieceId};
use aether_chassis_bloomery::store::OutstandingOrder;
use aether_data::wire::from_bytes;
use aether_harness_bloomery::{FixtureHarness, OperatorMove, digest, passed};

/// How many `(cell, sample)` blooms the run seals.
const CELLS: usize = 4;

fn stage_of(order: &OutstandingOrder) -> StageId {
    from_bytes(&order.stage).expect("a recorded order carries a StageId")
}

/// Whether the run has recorded cell `index` as reaching a terminal status.
fn resolved(run: &serde_json::Value, index: usize) -> bool {
    run["cells"][index]["state"].get("Resolved").is_some()
}

/// The construct order the cell currently sealed is waiting on, answering any
/// mechanical base gate that shows up first.
///
/// How many gates precede a cell's model lane is the coordinator's business, so
/// this waits for the lane rather than pinning the shape of what comes before
/// it — the base receipt is per base, and every cell here shares one.
fn await_construct(harness: &mut FixtureHarness) -> OutstandingOrder {
    harness.pump_until("the cell dispatches its construct lane", |harness| {
        for order in harness.orders() {
            if stage_of(&order) == StageId::BaseVerify {
                harness.upload_admitted(&passed(&order));
            }
        }
        harness.orders().iter().any(|order| stage_of(order) == StageId::Construct)
    });
    harness
        .orders()
        .into_iter()
        .find(|order| stage_of(order) == StageId::Construct)
        .expect("the pump returned once a construct order was outstanding")
}

#[test]
fn a_benchmark_run_seals_its_cells_in_sequence_over_one_base() {
    let mut harness = FixtureHarness::start("benchmark-run");
    let landing = harness.seed_landed_pull_request(5820, "Build the benchmark run.");
    let cells = [harness.record_model_override("bench-cell-a"), harness.record_model_override("bench-cell-b")];
    let base = harness.view().mainline;

    let request = serde_json::json!({
        "set": "0908-calibration",
        "base": base.to_hex(),
        "pull_requests": [landing],
        "cells": [cells[0].to_hex(), cells[1].to_hex()],
        "samples": 2,
        "instructions": harness.instructions().to_hex(),
        "reason": "measure the construct lane on landed history",
        "operator": "benchmark harness",
    });
    let (status, body) = harness.post("/benchmark", &request.to_string());
    assert_eq!(status, 202, "the door accepts the run and hands back a handle rather than waiting: {body}");
    let run = serde_json::from_str::<serde_json::Value>(&body).unwrap()["run"].as_u64().unwrap();

    let started = harness.benchmark_run(run);
    assert_eq!(started["cells"].as_array().unwrap().len(), CELLS, "two cells at sample size two are four: {started}");
    assert!(!started["cost_caveat"].as_str().unwrap().is_empty(), "the run renders the under-reporting caveat");
    assert!(!started["volatility"].as_str().unwrap().is_empty(), "and states that a restart ends it");

    let mut sealed = Vec::new();
    for index in 0..CELLS {
        let construct = await_construct(&mut harness);
        assert_eq!(
            harness.orders().iter().filter(|order| stage_of(order) == StageId::Construct).count(),
            1,
            "one cell walks at a time: cell {index} is the only model lane outstanding"
        );

        let bloom = aether_bloomery::BloomId(
            aether_bloomery::Digest::from_slice(&construct.bloom).expect("an order names a whole bloom id"),
        );
        sealed.push(bloom);

        // Stand in for the landing this cell would have produced, so the reset
        // between cells has something to undo.
        harness.move_mainline(digest(0xA0 + u8::try_from(index).unwrap()));

        // Withdrawing the member resolves the bloom without driving the whole
        // line: what this scenario is about is the sequence and the reset, and
        // the cell's agent has already entered the ledger at dispatch.
        harness.apply_operator(
            bloom,
            &OperatorMove::Withdraw {
                at_tick: 0,
                workpiece: WorkpieceId(construct.workpiece.clone()),
                reason: "the cell's lane is measured at dispatch".to_owned(),
                operator: "benchmark harness".to_owned(),
                cascade: false,
            },
        );

        harness.pump_until("the benchmark run records the cell and advances", |harness| {
            harness.benchmark_tick();
            harness.observe_tick();
            resolved(&harness.benchmark_run(run), index)
        });

        assert_eq!(
            harness.fixture_mainline(),
            base,
            "the run resets the fixture mainline to the golden-task base after cell {index}, so the next one replays \
             the same tree"
        );
    }

    assert_eq!(sealed.iter().collect::<BTreeSet<_>>().len(), CELLS, "each cell sealed its own bloom: {sealed:?}");
    assert_eq!(harness.benchmark_run(run)["status"], serde_json::json!("Finished"), "the sequence ran to the end");

    let ledger = harness.calibration().ledger;
    assert_eq!(ledger.store, StoreClass::Trial, "a benchmark run's rows are trial rows: {ledger:?}");
    assert!(!ledger.cost_caveat.is_empty(), "the under-reporting caveat renders beside the ledger's own");

    let mut measured: Vec<(String, u64)> = ledger
        .cells
        .iter()
        .map(|cell| {
            assert_eq!(cell.stage, StageId::Construct, "only the construct lanes ran: {cell:?}");
            (cell.agent.model.clone(), cell.attempts)
        })
        .collect();
    measured.sort();

    assert_eq!(
        measured,
        vec![("bench-cell-a".to_owned(), 2), ("bench-cell-b".to_owned(), 2)],
        "each cell's two samples are attributable to the override that selected it: {ledger:?}"
    );
}
