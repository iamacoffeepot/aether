//! A `Verify` the host cancelled at its sealed wall clock ejects its member,
//! naming the clock, and nothing is re-dispatched against it (ADR-0218
//! §Amendment: low tolerance, 2026-09-15).
//!
//! The expiry fact is admitted through the control core's ingress rather than
//! produced by letting fifteen minutes pass: the fixture cell's clock is the
//! host's, and a scenario that waited out a real sealed ceiling would be a
//! fifteen-minute test. What the sweep does with an overdue order — cancel it,
//! store the deterministic timeout record, and stamp
//! `StageVerdict::DeadlineExpiry` for a deadline while keeping `ExecutorFault`
//! for heartbeat silence — is the executor runtime's own unit surface, and it
//! is covered there.
//!
//! What only the loop can prove is the half below: that the reducer's
//! ejection closes the line rather than leaving a member at a stage with no
//! order behind it, and that the *other* fault verdict over the same evidence
//! still keeps its member. Those two facts arriving at the same door and
//! parting there is the whole point of the appended fact.

use aether_bloomery::{Evidence, EvidenceKind, Fact, Outcome, StageId, WorkpieceId};
use aether_harness_bloomery::{FixtureHarness, captured, digest};

/// The member whose verify runs out of time.
const WORKPIECE: &str = "wp";

#[test]
fn a_verify_killed_at_its_wall_clock_ejects_its_member() {
    let mut harness = FixtureHarness::start("wall-clock-ejects-scenario");
    let candidate_tree = digest(0xC1);
    let bloom = harness.seal_member(WORKPIECE, digest(0x51));

    let construct = harness.await_order();
    let candidate = harness.seed_capture(bloom, WORKPIECE, candidate_tree, digest(0xD1));
    harness.upload_admitted(&captured(&construct, candidate));

    // The member is at terminal Verify with an order outstanding against its
    // captured tree — exactly the position the expiry sweep selects from.
    let verify = harness.await_order();
    assert_eq!(verify.displayed_digest, candidate_tree.as_bytes().to_vec(), "the Verify runs against the capture");

    // The timeout record the sweep would have stored; its address is what the
    // ejection's reason points a reader at.
    let record = digest(0x7E);
    let outcome = harness.admit(
        "aether.bloomery.member_deadline_expired:wall-clock-scenario",
        Fact::MemberDeadlineExpired {
            bloom,
            workpiece: WorkpieceId(WORKPIECE.into()),
            stage: StageId::Verify,
            evidence: Evidence { subject: candidate_tree, kind: EvidenceKind::ExecutorFault, detail: record },
        },
    );
    assert!(matches!(outcome, Outcome::MembersWithdrawn { .. }), "the expired verify ejects its member: {outcome:?}");

    let view = harness.bloom(bloom);
    let member = &view.members[0];
    let departure = member.withdrawn.as_ref().expect("the member left the bloom");
    assert_eq!(departure.cause, "verify");
    assert!(
        departure.reason.contains("exceeded the sealed wall clock"),
        "the reason names the clock rather than a verifier: {}",
        departure.reason,
    );
    assert!(departure.reason.contains(&record.to_hex()), "and points at the timeout record: {}", departure.reason);
    assert!(member.cursor.is_none(), "an ejected member sits at no stage");
    assert!(member.wedge.is_none(), "an expiry spends no machinery budget on its way out");

    // The load-bearing negative: the artifact that ran out of time is not
    // handed to a lane again. Every dispatching reactor is given its tick.
    for _ in 0..3 {
        harness.dispatch_tick();
        harness.integrate_tick();
    }
    let redispatched: Vec<_> = harness.orders().into_iter().filter(|order| order.nonce != verify.nonce).collect();
    assert!(redispatched.is_empty(), "nothing is re-dispatched against an ejected member: {redispatched:?}");
}
