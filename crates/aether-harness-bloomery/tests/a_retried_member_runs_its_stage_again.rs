//! An operator retries a member's current stage and it runs again on the
//! candidate it already holds — no new revision, no supersede, and the run
//! charged to the member's machinery budget rather than to its work.
//!
//! Pre-fix there was no such door (#5423). A stage that had gone wrong for a
//! reason no verdict describes — a lane the host killed and never reported, a
//! candidate an operator can see is fine — could only be re-run by superseding
//! the bloom, which mints a new identity to express an execution decision and
//! throws away the candidate the member had already built. The grant door
//! (`Fact::GrantAttempts`) is the opposite of what is wanted here: it hands
//! budget *back*, and only to a member that has already wedged.

#![allow(clippy::unwrap_used)]

use aether_bloomery::{Digest, Evidence, EvidenceKind, Fact, Outcome, StageId, WorkpieceId};
use aether_harness_bloomery::{FixtureHarness, captured, digest, passed};

const MEMBER: &str = "wp";

#[test]
fn a_retried_member_runs_its_stage_again_on_the_candidate_it_holds() {
    let mut harness = FixtureHarness::start("operator-retry");
    let bloom = harness.seal_member(MEMBER, digest(0x51));

    // Drive the member to Verify holding a candidate, so the retry has both a
    // stage to name and a subject to bind to.
    let construct = harness.await_order();
    let candidate = harness.seed_capture(bloom, MEMBER, digest(0xC1), digest(0xD1));
    harness.upload_admitted(&captured(&construct, candidate));
    let verify = harness.await_order();
    assert_eq!(verify.workpiece, MEMBER);

    let before = harness.bloom(bloom).members[0].clone();
    assert_eq!(before.machinery_rolls, 0, "nothing has been charged to the machinery axis yet");

    // What `xtask bloom retry <bloom> wp` posts: the stage the member is
    // sitting at, and a fault bound to the candidate it is carrying.
    let outcome = harness.admit(
        "retry-wp",
        Fact::MemberExecutorFault {
            bloom,
            workpiece: WorkpieceId(MEMBER.into()),
            stage: StageId::Verify,
            evidence: Evidence { subject: candidate.tree, kind: EvidenceKind::ExecutorFault, detail: digest(0xAA) },
        },
    );
    assert!(
        matches!(outcome, Outcome::MachineryRetried { stage: StageId::Verify, rolls: 1, .. }),
        "the retry runs the named stage again and is charged one machinery roll: {outcome:?}",
    );

    let after = harness.bloom(bloom).members[0].clone();
    assert_eq!(after.scope_revision, before.scope_revision, "no new revision");
    assert_eq!(
        after.cursor.as_ref().and_then(|cursor| cursor.candidate).map(|held| held.tree),
        Some(candidate.tree),
        "and the same candidate: a retry re-runs the stage, it does not discard the work",
    );
    assert_eq!(after.machinery_rolls, 1, "the roll is charged to machinery, not to the member's attempts");
    assert_eq!(
        after.cursor.as_ref().map(|cursor| cursor.attempts),
        before.cursor.as_ref().map(|cursor| cursor.attempts),
        "the work axis is untouched — nothing here judged the candidate",
    );

    // A fresh order for the same member, beside the one the retry did not
    // answer for: the operator retried a stage still nominally in flight, and
    // the reducer minting a second order is what re-runs it.
    harness.pump_until("the retry re-dispatches the stage", |harness| harness.orders().len() == 2);
    let orders = harness.orders();
    let fresh: Vec<_> = orders.iter().filter(|order| order.nonce != verify.nonce).collect();
    assert_eq!(fresh.len(), 1, "exactly one new order: {orders:?}");
    assert_eq!(fresh[0].workpiece, MEMBER);
    assert_eq!(
        Digest::from_slice(&fresh[0].displayed_digest),
        Some(candidate.tree),
        "the fresh order is aimed at the same tree",
    );

    // The retried stage answers and the member resolves on it.
    harness.upload_admitted(&passed(fresh[0]));
    assert!(
        harness.bloom(bloom).members[0].resolution.is_some(),
        "the stage the operator re-ran is the one that carries the member forward",
    );

    // No liveness oracle here, and the omission is the point rather than an
    // exemption: a retry deliberately leaves the order it overtook outstanding —
    // the operator ran the stage again *because* the first dispatch was not
    // going to answer — and the oracle reads any outstanding order as work that
    // never completed. That order resolves through the reactor's deadline sweep,
    // which is the same path every overtaken dispatch has always taken.
    assert!(
        harness.outstanding().contains(&verify.nonce),
        "the overtaken order is left to its deadline rather than silently retired",
    );
}
