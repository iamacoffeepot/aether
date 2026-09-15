//! The operator verbs for a stuck candidate, end to end over the real
//! coordinator: a mismatched upload is refused and shown on the member beside
//! its outstanding order, `bloom cancel-order` drops the order without
//! faulting the lane, the doctor re-dispatches the member it then finds with
//! no lane and no dispatch, a repair on that non-terminal member moves it to
//! Verify, and a retry naming Verify takes a reconciled capture straight to
//! its verify.
//!
//! The plausible breakage each phase pins: repair refusing `NotWedged` for a
//! member that holds a candidate, retry refusing `StageMismatch` for Verify
//! off a Construct cursor, cancel killing the lane instead of dropping the
//! order, refusals landing nowhere the operator looks, and the doctor alerting
//! on a lane-less member without re-dispatching it.

#![allow(clippy::unwrap_used)]

use std::thread::sleep;
use std::time::{Duration, Instant};

use aether_bloomery::{BloomId, Digest, Evidence, EvidenceKind, Fact, OperatorRepair, Outcome, StageId, WorkpieceId};
use aether_chassis_bloomery::bloomery::{ScriptedEvidenceResult, ScriptedUpload};
use aether_chassis_bloomery::store::OutstandingOrder;
use aether_data::wire::from_bytes;
use aether_harness_bloomery::{FixtureHarness, digest, passed};
use serde_json::Value;

const STUCK: &str = "wp-stuck";
const OTHER: &str = "wp-other";

/// How long the doctor's re-dispatch may take before the wait is a failure
/// rather than a poll interval that has not come around yet.
const REDISPATCH_BUDGET: Duration = Duration::from_secs(30);

#[test]
fn a_stuck_candidate_is_repaired_retried_cancelled_and_redispatched() {
    // A one-second observer cadence: the doctor only re-dispatches a member
    // idle for a full poll interval, and the test has to outwait that clock.
    let mut harness = FixtureHarness::start_with_poll("stuck-candidate-verbs", 1);
    let bloom = harness.seal_members(&[(STUCK, digest(0x51)), (OTHER, digest(0x52))]);

    let refused = a_refusal_stands_on_the_member(&mut harness);
    let redispatched = a_cancel_drops_the_order_and_the_doctor_redispatches(&mut harness, &refused);
    a_repair_on_a_running_member_moves_it_to_verify(&mut harness, bloom, &redispatched);
    a_retry_naming_verify_takes_the_held_capture(&mut harness, bloom);
}

/// A mismatched upload is refused without touching the reducer, leaves its
/// order outstanding, and stands on the member as a blocker the operator can
/// read off `/view` — with the order's nonce, displayed digest, and deadline
/// beside it.
fn a_refusal_stands_on_the_member(harness: &mut FixtureHarness) -> OutstandingOrder {
    let order = named(&harness.await_orders(2), STUCK);
    let mut mismatched: ScriptedUpload = passed(&order);
    mismatched.subject = digest(0x99);
    match harness.upload(&mismatched) {
        ScriptedEvidenceResult::Refused { refusal } => {
            assert!(refusal.contains("DigestMismatch"), "the refusal names its variant: {refusal}");
        }
        other => panic!("a mismatched upload must be refused, got {other:?}"),
    }
    assert!(
        harness.orders().iter().any(|live| live.nonce == order.nonce),
        "a refused upload leaves its order outstanding"
    );

    let view = view_json(harness);
    let listed = view["orders"]
        .as_array()
        .expect("the view lists live orders")
        .iter()
        .find(|entry| entry["nonce"] == order.nonce)
        .expect("the refused order is listed")
        .clone();
    assert!(
        listed["blocker"].as_str().is_some_and(|blocker| blocker.contains("DigestMismatch")),
        "the order carries its blocker: {listed}"
    );
    assert!(listed["displayed"].is_string(), "the order names its displayed digest: {listed}");
    assert!(listed["deadline_unix_millis"].as_u64().is_some_and(|deadline| deadline > 0), "and its deadline: {listed}");

    let stuck = member_json(&view, STUCK);
    assert!(
        stuck["intake_refusal"].as_str().is_some_and(|refusal| refusal.contains("DigestMismatch")),
        "the member is blocked by the refusal: {stuck}"
    );

    order
}

/// `cancel-order` drops the order from the board without faulting the lane —
/// the lane's later upload is ignored rather than host-faulting the member —
/// and the doctor re-dispatches the member that cancel left with no lane and
/// no dispatch, at the stage it still stands on.
fn a_cancel_drops_the_order_and_the_doctor_redispatches(
    harness: &mut FixtureHarness,
    refused: &OutstandingOrder,
) -> OutstandingOrder {
    let (status, body) = harness.post(
        &format!("/orders/{}/cancel", refused.nonce),
        &serde_json::json!({"reason": "doomed lane", "operator": "eve"}).to_string(),
    );
    assert_eq!(status, 200, "cancel-order must land: {body}");
    let cancelled: Value = serde_json::from_str(&body).expect("cancel-order answers JSON");
    assert_eq!(cancelled["cancelled"], true, "the outstanding row was dropped: {cancelled}");
    assert!(harness.orders().iter().all(|live| live.nonce != refused.nonce), "the cancelled nonce leaves the board");

    match harness.upload(&passed(refused)) {
        ScriptedEvidenceResult::Refused { refusal } => {
            assert!(refusal.contains("CancelledByOperator"), "the late upload is ignored: {refusal}");
        }
        other => panic!("a cancelled nonce must be ignored, got {other:?}"),
    }

    let started = Instant::now();
    let redispatched = loop {
        harness.doctor_tick();
        harness.dispatch_tick();
        if let Some(fresh) = harness.orders().iter().find(|live| live.workpiece == STUCK && live.nonce != refused.nonce)
        {
            break fresh.clone();
        }
        assert!(started.elapsed() < REDISPATCH_BUDGET, "the doctor re-dispatches within a few poll intervals");
        sleep(Duration::from_millis(100));
    };
    assert_eq!(stage_of(&redispatched), StageId::Construct, "the member resumes where it stood");

    redispatched
}

/// A repair on a member that is running rather than wedged is judged now: the
/// operator's candidate re-enters at Verify without waiting for the roll budget
/// to wedge the member first.
fn a_repair_on_a_running_member_moves_it_to_verify(
    harness: &mut FixtureHarness,
    bloom: BloomId,
    redispatched: &OutstandingOrder,
) {
    // Minted the way a lane's capture would be, so the verifying lane can check
    // out the commit the repair names.
    let repaired = harness.seed_unpublished_capture(STUCK, digest(0x61), digest(0x62));
    let outcome = harness.admit(
        "repair-stuck",
        Fact::OperatorRepair {
            bloom,
            repair: OperatorRepair {
                workpiece: WorkpieceId(STUCK.into()),
                candidate: repaired,
                reason: "hand-built".into(),
                operator: "eve".into(),
            },
        },
    );
    assert!(
        matches!(outcome, Outcome::OperatorRepairAccepted { .. }),
        "repair on a non-terminal member is accepted: {outcome:?}"
    );

    let verify = pump_for_order(harness, STUCK, &redispatched.nonce);
    assert_eq!(stage_of(&verify), StageId::Verify, "the repair re-enters at Verify");
    assert_eq!(cursor_of(harness, bloom, STUCK), Some((StageId::Verify, repaired.tree)));
}

/// A member sitting at Construct because its reconcile assembled a capture is
/// handed that capture's verify by `retry --stage Verify`, rather than being
/// refused a stage it demonstrably holds the artifact for.
fn a_retry_naming_verify_takes_the_held_capture(harness: &mut FixtureHarness, bloom: BloomId) {
    let collided = harness.admit(
        "fold-conflict-other",
        Fact::FoldConflict {
            bloom,
            workpiece: WorkpieceId(OTHER.into()),
            checkpoint: digest(0x30),
            head: digest(0x31),
            evidence: Evidence { subject: digest(0x30), kind: EvidenceKind::FoldConflict, detail: digest(0x90) },
        },
    );
    assert!(!matches!(collided, Outcome::FoldConflictRejected { .. }), "the fold collision must record: {collided:?}");

    let assembled = harness.seed_unpublished_capture(OTHER, digest(0x41), digest(0x42));
    let reconciled = harness.admit(
        "reconcile-pass-other",
        Fact::AttemptCompleted {
            bloom,
            workpiece: WorkpieceId(OTHER.into()),
            stage: StageId::Reconcile,
            passed: true,
            evidence: Evidence { subject: digest(0x01), kind: EvidenceKind::VerificationResult, detail: digest(0x70) },
            candidate: Some(assembled),
        },
    );
    assert!(
        matches!(reconciled, Outcome::AttemptAdvanced { from: StageId::Reconcile, to: StageId::Construct, .. }),
        "the assembling reconcile returns to Construct holding its capture: {reconciled:?}"
    );

    let retried = harness.admit(
        "retry-verify-other",
        Fact::MemberExecutorFault {
            bloom,
            workpiece: WorkpieceId(OTHER.into()),
            stage: StageId::Verify,
            evidence: Evidence { subject: assembled.tree, kind: EvidenceKind::ExecutorFault, detail: digest(0x71) },
        },
    );
    assert!(
        matches!(retried, Outcome::MachineryRetried { stage: StageId::Verify, .. }),
        "retry --stage Verify on the held capture is accepted: {retried:?}"
    );

    let order = (0..100)
        .find_map(|_| {
            harness.dispatch_tick();
            harness
                .orders()
                .iter()
                .find(|order| order.workpiece == OTHER && stage_of(order) == StageId::Verify)
                .cloned()
        })
        .expect("the held capture is re-dispatched at Verify");
    assert_eq!(stage_of(&order), StageId::Verify);
    assert_eq!(
        cursor_of(harness, bloom, OTHER),
        Some((StageId::Verify, assembled.tree)),
        "the capture moved to its verify rather than taking another construct lap",
    );
}

fn named(orders: &[OutstandingOrder], workpiece: &str) -> OutstandingOrder {
    orders
        .iter()
        .find(|order| order.workpiece == workpiece)
        .unwrap_or_else(|| panic!("no outstanding order for {workpiece}"))
        .clone()
}

fn stage_of(order: &OutstandingOrder) -> StageId {
    from_bytes(&order.stage).expect("a recorded order carries a StageId")
}

/// The stage one member sits at and the capture it holds there.
fn cursor_of(harness: &mut FixtureHarness, bloom: BloomId, workpiece: &str) -> Option<(StageId, Digest)> {
    let cursor = harness.bloom(bloom).members.into_iter().find(|member| member.workpiece.0 == workpiece)?.cursor?;
    Some((cursor.stage, cursor.candidate?.tree))
}

fn view_json(harness: &mut FixtureHarness) -> Value {
    let (status, body) = harness.get("/view");
    assert_eq!(status, 200, "GET /view must answer: {body}");
    serde_json::from_str(&body).expect("GET /view answers JSON")
}

fn member_json(view: &Value, workpiece: &str) -> Value {
    view["blooms"]
        .as_array()
        .expect("the view lists blooms")
        .iter()
        .flat_map(|bloom| bloom["members"].as_array().cloned().unwrap_or_default())
        .find(|member| member["workpiece"] == workpiece)
        .unwrap_or_else(|| panic!("{workpiece} is not listed in the view"))
}

fn pump_for_order(harness: &mut FixtureHarness, workpiece: &str, exclude: &str) -> OutstandingOrder {
    for _ in 0..100 {
        harness.dispatch_tick();
        if let Some(order) =
            harness.orders().iter().find(|order| order.workpiece == workpiece && order.nonce != exclude)
        {
            return order.clone();
        }
    }
    panic!("no fresh order dispatched for {workpiece}");
}
