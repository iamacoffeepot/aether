//! One golden task, two profile cells, sample size two: four benchmark members
//! of one bloom, and a trial ledger whose cells are attributable to the
//! `ModelOverride` each one sealed (ADR-0184, issue #4871).
//!
//! The bug this catches is the whole mechanism failing quietly. Every cell of a
//! benchmark run replays the *same* order on the *same* base, so four members
//! that all resolved the compiled line — an override that never reached the
//! registry, an address nothing stored, a plan that reused one workpiece and
//! collapsed four members into fewer — produce a ledger that looks exactly like
//! a working comparison, minus the comparison. Only reading the cells back and
//! finding each agent where its own override put it distinguishes the two.
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
fn a_benchmark_run_seals_one_member_per_profile_cell() {
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
    // The reducer's own answer, not merely that it answered: a refused seal is
    // still `Admitted`, and a run that sealed nothing would leave every later
    // assertion measuring an empty table.
    assert!(report["admission"]["Admitted"].get("Sealed").is_some(), "the run's seal must seal: {body}");

    let members = report["members"].as_array().unwrap();
    assert_eq!(members.len(), 4, "two cells at sample size two are four members: {body}");
    assert_eq!(
        members.iter().map(|member| member["workpiece"].as_str().unwrap()).collect::<BTreeSet<_>>().len(),
        4,
        "each sample is its own member rather than a name collapsed onto a sibling: {body}"
    );
    assert!(!report["cost_caveat"].as_str().unwrap().is_empty(), "the run renders the under-reporting caveat");
    assert_eq!(report["tasks"].as_array().unwrap().len(), 1, "one landed pull request is one golden task");

    // Answer whatever mechanical base gate the base owes as it appears, and wait
    // for the four model lanes — only those enter the capability ledger, and how
    // many gates precede them is the coordinator's business rather than
    // something this scenario should pin.
    harness.pump_until("the four benchmark members dispatch their construct lanes", |harness| {
        for order in harness.orders() {
            if stage_of(&order) == StageId::BaseVerify {
                harness.upload_admitted(&passed(&order));
            }
        }
        harness.orders().iter().filter(|order| stage_of(order) == StageId::Construct).count() == 4
    });

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
