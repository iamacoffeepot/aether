//! One golden task, two profile cells, sample size two: four benchmark blooms,
//! and a trial ledger whose cells are attributable to the `ModelOverride` each
//! one sealed (ADR-0184, issue #4871).
//!
//! The bug this catches is the whole mechanism failing quietly. Every cell of a
//! benchmark run replays the *same* order on the *same* base, so four blooms
//! that all resolved the compiled line — an override that never reached the
//! registry, an address nothing stored, a plan that reused one workpiece and
//! deduplicated three of its four seals — produce a ledger that looks exactly
//! like a working comparison, minus the comparison. Only reading the cells back
//! and finding each agent where its own override put it distinguishes the two.
//!
//! The fixture cell *is* the trial cell (issue #5794), so this boots precisely
//! the coordinator a calibration host runs and drives the door an operator
//! drives.

#![allow(clippy::unwrap_used)]

use std::collections::BTreeSet;

use aether_bloomery::{StageId, StoreClass};
use aether_chassis_bloomery::store::OutstandingOrder;
use aether_data::wire::from_bytes;
use aether_harness_bloomery::{FixtureHarness, passed};

fn stage_of(order: &OutstandingOrder) -> StageId {
    from_bytes(&order.stage).expect("a recorded order carries a StageId")
}

#[test]
fn a_benchmark_run_seals_one_bloom_per_profile_cell() {
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
    assert_eq!(status, 200, "the benchmark door must seal the run: {body}");

    let report: serde_json::Value = serde_json::from_str(&body).unwrap();
    let blooms = report["blooms"].as_array().unwrap();
    assert_eq!(blooms.len(), 4, "two cells at sample size two are four blooms: {body}");
    assert_eq!(
        blooms.iter().map(|bloom| bloom["bloom"].as_str().unwrap()).collect::<BTreeSet<_>>().len(),
        4,
        "each sample is its own bloom, not a duplicate admit of a sibling: {body}"
    );
    for bloom in blooms {
        assert!(bloom["admission"].get("Admitted").is_some(), "every cell's seal must be admitted: {bloom}");
    }
    assert!(!report["cost_caveat"].as_str().unwrap().is_empty(), "the run renders the under-reporting caveat");
    assert_eq!(report["tasks"].as_array().unwrap().len(), 1, "one landed pull request is one golden task");

    // The four blooms share one base, so one `verify.base` gates all of them;
    // passing it is what lets each bloom's Construct dispatch, and only a model
    // lane enters the capability ledger.
    let base_verify = harness.await_order();
    assert_eq!(stage_of(&base_verify), StageId::BaseVerify);
    harness.upload_admitted(&passed(&base_verify));

    let constructs = harness.await_orders(4);
    for order in &constructs {
        assert_eq!(stage_of(order), StageId::Construct);
    }

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
