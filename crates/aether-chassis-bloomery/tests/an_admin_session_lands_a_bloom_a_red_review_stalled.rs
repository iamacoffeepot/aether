//! Bloom `0f16e207`, reproduced and then repaired (ADR-0219).
//!
//! The live state this scenario recreates: members sealed, one withdrawn by the
//! operator, the rest integrated, the aggregate verify green over the composed
//! tree — and the aggregate review red, with findings naming the member that
//! had already left. The reducer answered that verdict the only way it knew
//! how, by dispatching a Refine lap to re-author work nobody wanted, and the
//! operator had no move that could stop it without paying for the stop.
//!
//! The repair is the sequence the owner described: enter admin, cancel the
//! refine lane, put the composition back on the tree that passed, void the
//! findings the withdrawal made meaningless, exit, and let the bloom land.
//!
//! What only an end-to-end run can prove, and why each link is here rather than
//! in a reducer unit test:
//!
//! - The cancelled lane is cancelled *through the executor*, from a real
//!   outbox row under a real nonce, and the member's budget is untouched after
//!   it. A unit test can assert the decision; only this can assert that the
//!   order settled and nothing else moved.
//! - The composition, addressed by its reserved id, reaches every door. Two
//!   sibling verbs refuse it today despite their help text, which is exactly
//!   the shape a unit test over a hand-built record does not catch.
//! - The landing stands up from a join *no verdict completed*: the critic's
//!   half came from a person. Nothing but the whole chain shows that the land
//!   reactor takes it and that the record says whose it was.

use aether_bloomery::{
    AdminCandidate, AdminLaneCancel, AdminNote, AdminWaiver, BloomStatus, Fact, Outcome, StageId, Withdrawal,
    WithdrawalCause, WorkpieceId,
};
use aether_chassis_bloomery::bloomery::{ScriptedUpload, ScriptedVerdict};
use aether_chassis_bloomery::store::OutstandingOrder;
use aether_data::wire::from_bytes;
use aether_harness_bloomery::{FixtureHarness, captured, digest, passed, verdict};

/// The two members that finish, and the one an operator takes out of the line.
const FIRST: &str = "wp-a";
const SECOND: &str = "wp-b";
const WITHDRAWN: &str = "wp-c";

/// The operator behind every act in this session.
const OPERATOR: &str = "iamacoffeepot";

fn stage_of(order: &OutstandingOrder) -> StageId {
    from_bytes(&order.stage).expect("a recorded order carries a StageId")
}

fn named<'a>(orders: &'a [OutstandingOrder], workpiece: &str) -> &'a OutstandingOrder {
    orders
        .iter()
        .find(|order| order.workpiece == workpiece)
        .unwrap_or_else(|| panic!("no outstanding order for {workpiece}"))
}

fn aggregate(orders: &[OutstandingOrder], stage: StageId) -> &OutstandingOrder {
    orders
        .iter()
        .find(|order| order.workpiece.is_empty() && stage_of(order) == stage)
        .unwrap_or_else(|| panic!("no bloom-level order at {stage:?}"))
}

fn note(reason: &str) -> AdminNote {
    AdminNote { reason: reason.to_owned(), operator: OPERATOR.to_owned() }
}

/// The red aggregate review that started the incident: findings prose naming a
/// member that is no longer in the bloom, and an implication list that names
/// nobody.
///
/// The prose is where the withdrawn member appears, which is exactly how it
/// appeared live — the critic read a work order the composition still listed.
/// It cannot ride the *implication* list, because a verdict implicating a
/// withdrawn member is refused before it reaches the findings channel: there is
/// no cursor to re-open and no claim to revoke, so the finding could not be
/// routed anywhere.
fn red_review(order: &OutstandingOrder) -> ScriptedUpload {
    ScriptedUpload {
        findings: Some(format!("{WITHDRAWN} is missing from the composed tree")),
        ..verdict(order, ScriptedVerdict::ReviewFinding)
    }
}

#[test]
fn an_admin_session_lands_a_bloom_a_red_review_stalled() {
    let mut harness = FixtureHarness::start("admin-mode-scenario");
    let sealed_on = harness.view().mainline;
    let bloom = harness.seal_members(&[(FIRST, digest(0x51)), (SECOND, digest(0x52)), (WITHDRAWN, digest(0x53))]);

    // Three members enter the line; one leaves it. The withdrawal cancels its
    // lane and frees its claim ref, and the composition's work order is
    // assembled from what is left — but the *review* still reads the member
    // list the order was written from, which is the seam this bloom fell
    // through.
    let constructs = harness.await_orders(3);
    harness.admit(
        "withdraw-c",
        Fact::Withdraw {
            bloom,
            withdrawals: vec![Withdrawal {
                workpiece: WorkpieceId(WITHDRAWN.to_owned()),
                cause: WithdrawalCause::Operator,
                reason: "the scope was wrong and the work is filed forward".to_owned(),
                operator: OPERATOR.to_owned(),
            }],
            cascade: false,
        },
    );

    for member in [FIRST, SECOND] {
        let candidate = harness.seed_capture(bloom, member, digest(0xC0), digest(0xD0));
        harness.upload_admitted(&captured(named(&constructs, member), candidate));
    }
    let verifies = harness.await_orders(2);
    for member in [FIRST, SECOND] {
        harness.upload_admitted(&passed(named(&verifies, member)));
    }

    // The remaining claim set is complete, so the fold goes out and both
    // composite gates judge it. The mechanical gate passes over the composed
    // tree — the tree this whole repair is about putting back.
    harness.integrate_tick();
    let gates = harness.await_orders(2);
    harness.upload_admitted(&passed(aggregate(&gates, StageId::AggregateVerify)));
    harness.upload_admitted(&red_review(aggregate(&gates, StageId::AggregateReview)));

    // The red verdict bought a repair lap over the withdrawn members' work.
    let refine = harness.await_order();
    assert_eq!(refine.workpiece, WorkpieceId::COMPOSITION, "the weave repair runs against the composition");
    assert_eq!(stage_of(&refine), StageId::Refine, "a red review re-weaves rather than re-verifying");

    let composition = harness.bloom(bloom).composition.expect("the refused weave gave the composition a line");
    let cursor = composition.cursor.expect("the repair lap wrote a cursor");
    let integrated = cursor.candidate.expect("the lap is repairing the composed tree");
    let finding = composition.findings.first().expect("the red verdict filed its finding").detail;

    // Enter. Nothing new dispatches from here, an executor fault costs the
    // bloom nothing, and the five acts below become admissible.
    harness.admit("admin-enter", Fact::AdminEnter { bloom, note: note("the review named withdrawn members") });
    assert!(harness.bloom(bloom).admin.is_some(), "the board says a person is inside this bloom");

    // Cancel the lap. The order settles through the executor and the
    // composition's budget does not move — the difference between this and
    // killing the model process, which the deadline reaper eventually charges
    // as a failed attempt.
    let rolls_before = harness.bloom(bloom).composition.and_then(|view| view.cursor).map(|cursor| cursor.attempts);
    harness.admit(
        "admin-cancel",
        Fact::AdminCancelLane {
            bloom,
            cancel: AdminLaneCancel {
                workpiece: WorkpieceId::composition(),
                nonce: refine.nonce.clone(),
                note: note("the lap is re-authoring members that already left"),
            },
        },
    );
    harness.pump_until("the cancelled lane leaves the outstanding set", |harness| {
        !harness.orders().iter().any(|order| order.nonce == refine.nonce)
    });
    assert_eq!(
        harness.bloom(bloom).composition.and_then(|view| view.cursor).map(|cursor| cursor.attempts),
        rolls_before,
        "a cancelled lane is a lane that never ran: no attempt is spent",
    );

    // Put the composition back on the tree the mechanical gate passed. The
    // weave is the one the record already holds, so the fold does not move and
    // the gate that passed it has not stopped having passed it.
    harness.admit(
        "admin-set-candidate",
        Fact::AdminSetCandidate {
            bloom,
            set: AdminCandidate {
                workpiece: WorkpieceId::composition(),
                candidate: integrated,
                note: note("this is the tree the aggregate verify passed"),
            },
        },
    );

    // Void the findings. What the ledger gets is an operator adjudication over
    // the named evidence and the critic's half of the join — never a green
    // verdict nobody produced.
    harness.admit(
        "admin-waive",
        Fact::AdminWaive {
            bloom,
            waiver: AdminWaiver {
                gate: StageId::AggregateReview,
                findings: vec![finding],
                acknowledged_unverified: false,
                note: note("every finding names a member that was withdrawn before the review ran"),
            },
        },
    );
    assert!(
        harness.bloom(bloom).composition.is_none_or(|view| view.findings.is_empty()),
        "the finding is closed by the adjudication the waiver recorded",
    );

    // Exit. The join is complete and its second half came from a person, so
    // nothing else was ever going to resolve this fold.
    let exit = harness.admit("admin-exit", Fact::AdminExit { bloom, note: note("the repair is done") });
    assert!(
        matches!(exit, Outcome::AdminExited { landing: true, .. }),
        "the close is what completed the join, not a verdict still to arrive: {exit:?}",
    );
    harness.await_landing(bloom, BloomStatus::Landed);

    let landed = harness.bloom(bloom);
    assert!(landed.admin.is_none(), "the session closed");
    assert_ne!(harness.view().mainline, sealed_on, "mainline advanced off the base the bloom sealed on");
    assert_eq!(landed.waivers, vec![finding], "the landed bloom still names the red verdict a person stood in for");
    assert!(
        landed.members.iter().any(|member| member.workpiece.0 == WITHDRAWN && member.withdrawn.is_some()),
        "and the member whose absence the review complained about is still recorded as withdrawn",
    );
}
