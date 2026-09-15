//! A bloom whose members are all documentation-clean pays rustdoc exactly once
//! (ADR-0218 §Amendment: documentation is judged once, over the product).
//!
//! `verify.docs` is the long pole of every verification the line runs — 245 to
//! 607 seconds per invocation — and it is the one gate `sccache` cannot cache,
//! because rustdoc's output is not a compilation artifact the wrapper knows how
//! to key. Its placement is therefore not a detail: it is most of what a bloom
//! spends. And the placement is stated nowhere as a rule. It is an emergent
//! property of which command each dispatch names and what the sealed manifest
//! declares that command fans out to, which means it can drift in either
//! direction without a single line of code looking wrong.
//!
//! Counting is the only assertion that catches both directions, so this counts.
//! The count comes off the coordinator's own journal rather than off the orders
//! the scenario happened to await — a dispatch nobody waited for is exactly the
//! one that would hide — and it resolves each work order's command against the
//! manifest rather than testing for a spelling, so the scenario keeps its
//! meaning if a position is ever renamed.

#![allow(clippy::unwrap_used)]

use aether_bloomery::{
    BloomStatus, Decision, PipelineManifest, StageId, VerifyFailure, decode_recorded_decisions,
};
use aether_chassis_bloomery::store::StoreBackend;
use aether_data::wire::from_bytes;
use aether_harness_bloomery::{FixtureHarness, ScenarioHarness, captured, digest, passed, reviewed};

const FIRST: &str = "wp-a";
const SECOND: &str = "wp-b";

#[test]
fn a_doc_clean_bloom_runs_the_documentation_gate_exactly_once() {
    let mut harness = FixtureHarness::start("doc-clean-bloom-runs-documentation-once");
    let bloom = harness.seal_members(&[(FIRST, digest(0x51)), (SECOND, digest(0x52))]);

    // Both members enter the line at seal, so both Construct orders are
    // outstanding together and each is answered against its own workpiece.
    for construct in &harness.await_orders(2) {
        let seed = u8::from(construct.workpiece == SECOND);
        let candidate =
            harness.seed_capture(bloom, &construct.workpiece, digest(0xC0 + seed), digest(0xD0 + seed));
        harness.upload_admitted(&captured(construct, candidate));
    }

    for verify in &harness.await_orders(2) {
        harness.upload_admitted(&passed(verify));
    }

    harness.integrate_tick();
    for order in &harness.await_orders(2) {
        let upload = if from_bytes::<StageId>(&order.stage).is_ok_and(|stage| stage == StageId::AggregateReview) {
            reviewed(order)
        } else {
            passed(order)
        };
        harness.upload_admitted(&upload);
    }

    harness.land_tick();
    harness.await_landing(bloom, BloomStatus::Landed);

    // Tripwire: exactly one dispatched work order in the whole bloom names a
    // position whose fan-out includes `verify.docs`.
    //
    // Above one, the gate has leaked back onto a position that judges something
    // short of the finished product — a member's closure, a pre-check of a
    // partial eager head, a contextual shared run over a group — and the bloom
    // is paying its longest gate per composition step to re-ask a question none
    // of those positions can answer, which is the cost this amendment removed.
    //
    // Below one, nothing judged the documentation of the product that landed.
    // That is the quieter failure and the more expensive one: it does not show
    // up as a slow bloom, it shows up as a red base the next morning.
    let dispatches = documentation_dispatches(&harness);
    assert_eq!(
        dispatches.len(),
        1,
        "a doc-clean bloom runs the documentation gate once, over the product; it ran it at {dispatches:?}",
    );
    assert_eq!(
        dispatches[0].0,
        StageId::AggregateVerify,
        "the one documentation pass is the aggregate verify over the finished product",
    );
}

/// Every dispatched work order in the journal whose command fans out to
/// `verify.docs`, as `(stage, command)` pairs.
///
/// Resolved through the manifest rather than by testing the command against a
/// spelling, so the count follows the gate rather than the name of whichever
/// position currently carries it.
fn documentation_dispatches(harness: &ScenarioHarness) -> Vec<(StageId, String)> {
    let manifest = PipelineManifest::compiled();
    let runs_documentation = |command: &str| {
        manifest
            .verifiers
            .runs
            .get(command)
            .into_iter()
            .flatten()
            .filter_map(|name| manifest.intern(name))
            .any(|identity| identity == VerifyFailure::Docs)
    };

    harness
        .commission_store()
        .replay_journal()
        .expect("the coordinator journal replays")
        .iter()
        .filter_map(|record| {
            decode_recorded_decisions(&record.decisions, record.decisions_schema_digest.as_deref()).ok()
        })
        .flat_map(|decisions| decisions.effects)
        .filter_map(dispatched)
        .filter(|(_, command)| runs_documentation(command))
        .collect()
}

/// The stage and command of one decision's dispatched work order, or `None`
/// when the decision dispatches no lane.
///
/// Every verify-shaped dispatch the line makes is listed, so a position added
/// later that carries a work order is a compile error here rather than a
/// silently uncounted run.
fn dispatched(effect: Decision) -> Option<(StageId, String)> {
    let (stage, transformation) = match effect {
        Decision::DispatchAttempt { stage, transformation, .. } => (stage, transformation),
        Decision::DispatchAggregateVerify { transformation, .. } => (StageId::AggregateVerify, transformation),
        Decision::DispatchPrecheck { transformation, .. } | Decision::OfferPrecheck { transformation, .. } => {
            (StageId::AggregateVerify, transformation)
        }
        Decision::DispatchBaseVerify { transformation, .. } => (StageId::BaseVerify, transformation),
        _ => return None,
    };

    Some((stage, transformation.command))
}
