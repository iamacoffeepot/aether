//! A broken intra-doc link passes every member gate, fails the one pass that
//! judges the finished product, and is answered by a repair lap rather than by
//! ejecting anybody (ADR-0218 §Amendment: documentation is judged once, over
//! the product).
//!
//! Two rules meet here and they pull opposite ways. The member position does
//! not fan out to `verify.docs` at all — an intra-doc link resolves across the
//! whole workspace, so a member's closure can neither break it alone nor prove
//! it alone — which means a candidate carrying one goes green and folds. And
//! §Amendment: low tolerance parks a bloom on the *first* red fold, because a
//! fold that does not build is a statement about the combination and there is
//! no member to blame. Compose those two without the discrimination this
//! scenario pins and every documentation typo becomes a parked bloom waiting on
//! a person, which is the most expensive possible answer to a defect whose
//! diagnostic already names the file and the line.
//!
//! What must hold is therefore a conjunction, and each half fails silently on
//! its own: the red opens a lap (not a park, not an ejection), the lap is the
//! composition's rather than any member's, the gates run *again* over the
//! repaired weave so the documentation pass is actually re-asked, and the bloom
//! lands. A reducer that parked, that revoked a member's claim, or that resolved
//! straight to the landing without re-running the gates passes every unit test
//! in the aggregate-verify module and fails here.

#![allow(clippy::unwrap_used)]

use aether_bloomery::{
    BloomStatus, PipelineManifest, StageId, Transformation, VerifyFailure, VerifyFailureSet, WorkpieceId,
};
use aether_chassis_bloomery::bloomery::ScriptedUpload;
use aether_chassis_bloomery::store::OutstandingOrder;
use aether_data::wire::from_bytes;
use aether_harness_bloomery::{FixtureHarness, captured, digest, failed, passed, reviewed};

const MEMBER: &str = "wp-0";

/// What a real rustdoc red looks like coming back off the lane: a file, a line,
/// and the link it could not resolve. Scripted verbatim because the repair lap's
/// work order is this text — a lap handed an empty finding has nothing to aim at.
const DOCS_FINDINGS: &str = "## verify.docs\n\nerror: unresolved link to `Fact::SuppressionDisposition`\n  \
     --> crates/aether-bloomery/src/persisted/mod.rs:541:9\n";

#[test]
fn a_documentation_red_over_the_product_is_repaired_in_one_lap_and_lands() {
    let mut harness = FixtureHarness::start("documentation-red-repairs-in-a-lap");
    let bloom = harness.seal_members(&[(MEMBER, digest(0x51))]);

    let construct = harness.await_order();
    let candidate = harness.seed_capture(bloom, MEMBER, digest(0xC1), digest(0xD1));
    harness.upload_admitted(&captured(&construct, candidate));

    // The member's own gate never asks the question its candidate fails. That
    // is the premise the rest of the scenario rests on, so it is asserted
    // rather than assumed: a member position that regained `verify.docs` would
    // catch this defect here, the fold would never see it, and every assertion
    // below would pass for the wrong reason.
    let verify = harness.await_order();
    let member_gates = gates_of(&verify);
    assert!(
        !member_gates.contains(VerifyFailure::Docs),
        "the member position must not fan out to documentation: {member_gates:?}",
    );
    harness.upload_admitted(&passed(&verify));

    harness.integrate_tick();
    let gates = harness.await_orders(2);
    let (aggregate, review) = split_gates(&gates);
    assert!(
        gates_of(aggregate).contains(VerifyFailure::Docs),
        "the aggregate verify over the product is the position that owes the documentation question",
    );

    harness.upload_admitted(&reviewed(review));
    harness.upload_admitted(&with_findings(&failed(aggregate, VerifyFailureSet::one(VerifyFailure::Docs))));

    // The refusal must not have cost anyone their place. Under
    // §Amendment: low tolerance a red fold parks the bloom under an operator
    // hold and dispatches nothing further, and that is exactly the outcome a
    // documentation red must not take.
    let refused = harness.bloom(bloom);
    assert!(refused.operator_hold.is_none(), "a documentation red does not brake the bloom: {refused:?}");
    assert!(
        refused.members.iter().all(|member| member.withdrawn.is_none() && member.wedge.is_none()),
        "a documentation red ejects nobody: {:?}",
        refused.members,
    );

    let repair = harness.await_order();
    assert_eq!(
        repair.workpiece,
        WorkpieceId::COMPOSITION,
        "the lap belongs to the composition, not to a member that passed its own gate",
    );
    assert_eq!(from_bytes::<StageId>(&repair.stage).expect("an order names its stage"), StageId::Refine);

    let repaired = harness.seed_capture(bloom, WorkpieceId::COMPOSITION, digest(0xC2), digest(0xD2));
    harness.upload_admitted(&captured(&repair, repaired));

    // The pass is re-asked over the tree the lap produced. Without this the
    // repair would be taken on trust and a lap that fixed nothing would land.
    let rerun = harness.await_orders(2);
    let (reproved, rereviewed) = split_gates(&rerun);
    assert_eq!(
        from_bytes::<Transformation>(&reproved.transformation).expect("an order carries its work order").inputs[0],
        repaired.tree,
        "the second documentation pass judges the repaired weave, not the tree that was refused",
    );
    harness.upload_admitted(&reviewed(rereviewed));
    harness.upload_admitted(&passed(reproved));

    harness.land_tick();
    harness.await_landing(bloom, BloomStatus::Landed);
    assert_eq!(harness.bloom(bloom).status, BloomStatus::Landed, "one documentation lap later, the product lands");
}

/// The verifier identities the position this order dispatches actually fans out
/// to — the manifest's `[verifiers.runs]` for the order's own command.
///
/// Resolved through the command the work order names rather than through the
/// stage, because the stage is what makes both the composition pre-check and
/// the final pass `AggregateVerify` while the command is the only thing that
/// separates their fan-outs.
fn gates_of(order: &OutstandingOrder) -> VerifyFailureSet {
    let transformation = from_bytes::<Transformation>(&order.transformation).expect("an order carries its work order");
    let manifest = PipelineManifest::compiled();

    manifest
        .verifiers
        .runs
        .get(&transformation.command)
        .into_iter()
        .flatten()
        .filter_map(|name| manifest.intern(name))
        .collect()
}

/// The mechanical gate and the critic of one composite-gate pair, in that order.
fn split_gates(orders: &[OutstandingOrder]) -> (&OutstandingOrder, &OutstandingOrder) {
    let review = orders
        .iter()
        .find(|order| from_bytes::<StageId>(&order.stage).is_ok_and(|stage| stage == StageId::AggregateReview))
        .expect("the composite pair carries a critic");
    let verify = orders
        .iter()
        .find(|order| from_bytes::<StageId>(&order.stage).is_ok_and(|stage| stage == StageId::AggregateVerify))
        .expect("the composite pair carries a mechanical gate");
    (verify, review)
}

/// The failing upload carrying the diagnostic the repair lap reads.
fn with_findings(upload: &ScriptedUpload) -> ScriptedUpload {
    ScriptedUpload { findings: Some(DOCS_FINDINGS.to_owned()), ..upload.clone() }
}
