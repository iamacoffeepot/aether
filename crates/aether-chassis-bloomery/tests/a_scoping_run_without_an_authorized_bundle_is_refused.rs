#![cfg(all(unix, feature = "github"))]

//! A pre-bloom scoping run under a host that authorized no instruction bundle
//! is refused at dispatch with a journaled machinery fault (ADR-0214).
//!
//! `gated()` used to exclude `scope.fill` by name because a scoping run carries
//! no bloom registry a pin could live in. That left the one model lane that
//! runs before a bloom exists ungated. The run record now holds the pin, and
//! this scenario is what fails on a head that still bypasses the gate: the
//! drain submits, an order exists, and nothing is journaled about the missing
//! process.

use std::thread;
use std::time::{Duration, Instant};

use aether_bloomery::{Event, Fact, Observation, Provenance, Statement, WorkpieceId, decode_recorded_event};
use aether_chassis_bloomery::bloomery::open_scope_run;
use aether_chassis_bloomery::store::{CommissionBackend, StoreBackend};
use aether_harness_bloomery::{HarnessBuilder, HarnessRoots};

#[test]
fn a_scoping_run_without_an_authorized_bundle_is_refused_rather_than_run() {
    let roots = HarnessRoots::create();
    let mut harness =
        HarnessBuilder::fixture().without_authorized_instructions().roots(&roots).start("unpinned-scope-run");

    let commission = WorkpieceId("wp-scope".to_owned());
    let intent = Statement {
        words: b"scope this workpiece".to_vec(),
        provenance: Provenance::ObservationAttestation(Observation { source: "scenario".to_owned() }),
        parents: Vec::new(),
    };
    let mut store = harness.commission_store();
    let intent_digest = store.create(&commission, &intent).expect("the commission is created");
    let base = harness.view().mainline;
    open_scope_run(&mut store, &commission, intent_digest, base, "scope sketch").expect("the run opens unpinned");
    drop(store);

    pump_until(&mut harness, "the refused scoping run is journaled as a host fault", |harness| {
        journal_has_executor_fault(harness)
    });

    assert!(
        harness.orders().is_empty(),
        "a refused scoping run must not reach a worker: {:?}",
        harness.orders().iter().map(|order| order.nonce.clone()).collect::<Vec<_>>(),
    );
    let rows = harness.commission_store().list_scope_runs(&commission.0).expect("the run ledger reads");
    assert!(
        rows.iter().all(|row| row.kind != "dispatched"),
        "a refused run never records a dispatched nonce: {rows:?}",
    );
}

fn journal_has_executor_fault(harness: &mut aether_harness_bloomery::ScenarioHarness) -> bool {
    harness
        .commission_store()
        .replay_journal()
        .expect("the journal replays")
        .iter()
        .filter_map(|record| decode_recorded_event(&record.event, record.event_schema.as_deref()).ok())
        .any(|event: Event| matches!(event.fact, Fact::MemberExecutorFault { .. }))
}

fn pump_until(
    harness: &mut aether_harness_bloomery::ScenarioHarness,
    what: &str,
    ready: impl Fn(&mut aether_harness_bloomery::ScenarioHarness) -> bool,
) {
    let deadline = Instant::now() + Duration::from_mins(2);
    while !ready(harness) {
        assert!(Instant::now() < deadline, "{what} did not happen inside the scenario's budget");
        harness.tick();
        thread::sleep(Duration::from_millis(25));
    }
}
