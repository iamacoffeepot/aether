//! A lane cancelled mid-run still spent tokens, and the ledger prices them
//! (issue 6029).
//!
//! The ledger prices a dispatch only from the evidence a lane deposits, so a
//! lane cancelled at its sealed execution limit used to spend tokens the
//! ledger never saw: its `evidence.json` result record — the one object every
//! priced path reads — was never written, and the timeout synthesized a
//! cost-free verdict. Now the cancel recovers the run's harness usage from
//! beside its evidence and the timeout admits it as a study row like any
//! other attempt, so `/spend`'s unaccounted count names only dispatches with
//! no session log at all.

#![allow(clippy::unwrap_used)]

use aether_bloomery::{Digest, FakeKeyProvider, KeyId, StudyRecord, signed_approval};
use aether_chassis_bloomery::artifacts::GetResult;
use aether_chassis_bloomery::bloomery::mock_lane::{LaneMode, LaneScript};
use aether_chassis_bloomery::store::CommissionBackend;
use aether_data::wire::from_bytes;
use aether_harness_bloomery::{HarnessBuilder, Repo, ScenarioHarness};

/// The member whose construct lane is cancelled at its limit mid-turn.
const MEMBER: &str = "wp-a";

fn approve(harness: &ScenarioHarness, scope_revisions: &[Digest]) {
    let mut store = harness.commission_store();
    for scope_revision in scope_revisions {
        let approval =
            signed_approval(KeyId(String::from("cancelled-lane-pricing harness")), &[0x0A; 32], *scope_revision);
        store.insert_approval(&approval, &FakeKeyProvider).expect("the authored scope retains its signed approval");
    }
}

#[test]
fn a_lane_cancelled_mid_run_is_priced_from_the_usage_it_streamed() {
    let authority = Repo::with_formatted_example_project();
    // The mock parks mid-turn after streaming two billed turns, so only the
    // sealed deadline can end a run — the timeout path, not a completion.
    // Every occurrence parks, so no retry can complete and overwrite the
    // cancelled attempt's study row with its own: the row this asserts on is
    // the priced cancellation, deterministically.
    let script = LaneScript::all_passing().with_default(LaneMode::NeverExits);
    let mut harness =
        HarnessBuilder::local_authority(&authority).script(&script).wall_clock_secs(8).start("cancelled-lane-pricing");
    let scope = harness.author_sized_scope_revision(MEMBER, &["crates/example-a/**"], "M");
    approve(&harness, &[scope]);
    let bloom = harness.seal_member(MEMBER, scope);

    harness.pump_until("the cancelled construct holds an order", |harness| !harness.orders().is_empty());
    let attempt = harness
        .orders()
        .into_iter()
        .find(|order| order.workpiece == MEMBER)
        .map(|order| Digest::from_slice(&order.displayed_digest).expect("the order displays a digest"))
        .expect("the cancelled construct dispatched an order");

    harness.pump_until("the cancelled attempt's cost reaches the study index", |harness| {
        harness.study_index_row(bloom, attempt).is_some()
    });

    let artifact = harness.study_index_row(bloom, attempt).expect("the cancelled attempt filed a study row");
    let GetResult::Ok { bytes, .. } = harness.artifact(&artifact) else {
        panic!("the study index names bytes the artifact store holds: {artifact}");
    };
    let record: StudyRecord = from_bytes(&bytes).expect("the study artifact decodes");
    assert_eq!(record.bloom, bloom, "the cancelled cost grades this bloom");
    assert!(record.grades(&attempt), "the cancelled cost grades the cancelled attempt");
    assert_eq!(record.cost.input_tokens, 3_200, "both streamed turns price the cancelled dispatch");
    assert_eq!(record.cost.cache_read_tokens, 2_300);
    assert_eq!(record.cost.cache_write_tokens, 300);
    assert_eq!(record.cost.output_tokens, 450);
}
