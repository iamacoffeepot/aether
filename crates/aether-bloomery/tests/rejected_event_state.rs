//! Rejected events leave surface holds and file leases standing.
//!
//! Snapshot application used to clear those tables from the fact alone, so a
//! `VerifyFailed` refused at Construct, a passing-looking completion the
//! reducer rejected, or a refused Integrate still dropped the hold and the
//! member's leases. A rejection is a no-op: only the idempotency key is
//! recorded.

#![allow(clippy::unwrap_used)]

mod common;

use aether_bloomery::{
    AttemptCompletedError, BloomId, CandidateRef, Digest, Evidence, EvidenceKind, Fact, IntegrateError, Outcome,
    ResolutionClaim, Snapshot, StageId, SurfaceRequest, VerifyFailedError, VerifyFailure, VerifyFailureSet,
    WorkpieceId,
};
use common::{claim, digest, draft, event, membership, step};

const LEASED_PATH: &str = "crates/a/src/lib.rs";
const OBSERVED_AT: u64 = 1_700_000_000_000;

fn sealed() -> (Snapshot, BloomId, WorkpieceId, Digest) {
    let spec = draft(0, vec![membership("wp", 1)]).seal();
    let bloom = spec.id();
    let workpiece = spec.members()[0].workpiece.clone();
    let scope_revision = spec.members()[0].scope_revision;
    let (snapshot, _) = step(&Snapshot::new(digest(0)).with_green_base(digest(0)), &event("seal", Fact::Seal(spec)));
    (snapshot, bloom, workpiece, scope_revision)
}

fn surface_request(scope_revision: Digest) -> SurfaceRequest {
    SurfaceRequest::normalize(
        scope_revision,
        &[],
        "the caller this construct must update lives outside the sealed surface",
        vec![("crates/example-b/src/lib.rs".to_string(), "the caller".to_string())],
    )
    .expect("a literal path normalizes")
}

fn request_surface(snapshot: &Snapshot, bloom: BloomId, workpiece: &WorkpieceId, scope_revision: Digest) -> Snapshot {
    let stage = snapshot.blooms[&bloom].progress[workpiece].stage;
    let (after, decided) = step(
        snapshot,
        &event(
            "surface",
            Fact::SurfaceRequested {
                bloom,
                workpiece: workpiece.clone(),
                stage,
                evidence: Evidence {
                    subject: scope_revision,
                    kind: EvidenceKind::ConstructDeclined,
                    detail: digest(9),
                },
                request: surface_request(scope_revision),
            },
        ),
    );
    assert!(matches!(decided.outcome, Outcome::SurfaceRequested { .. }), "the hold this case protects: {decided:?}");
    assert!(after.awaiting_surface(&bloom, workpiece).is_some());
    after
}

fn observe_write(snapshot: &Snapshot, bloom: BloomId, workpiece: &WorkpieceId) -> Snapshot {
    let stage = snapshot.blooms[&bloom].progress[workpiece].stage;
    let (after, decided) = step(
        snapshot,
        &event(
            "observe",
            Fact::LaneWritesObserved {
                bloom,
                workpiece: workpiece.clone(),
                stage,
                paths: vec![LEASED_PATH.to_string()],
                observed_at: OBSERVED_AT,
            },
        ),
    );
    assert!(matches!(decided.outcome, Outcome::LeasesObserved { .. }), "the lease this case protects: {decided:?}");
    assert_eq!(after.file_lease(&bloom, LEASED_PATH).unwrap().holder, *workpiece);
    after
}

fn unbound_claim(workpiece: &WorkpieceId, scope_revision: Digest) -> ResolutionClaim {
    ResolutionClaim {
        workpiece: workpiece.clone(),
        scope_revision,
        candidate: digest(51),
        evidence: Evidence { subject: digest(99), kind: EvidenceKind::ResolutionClaim, detail: digest(201) },
    }
}

// The plausible bug: `Snapshot::apply` cleared a surface request from
// `Fact::VerifyFailed` without asking whether the reducer admitted it, so a
// member still at Construct lost the hold a rejected verdict could not have
// overtaken.
#[test]
fn a_rejected_verify_failed_at_construct_leaves_the_surface_hold() {
    let (snapshot, bloom, workpiece, scope_revision) = sealed();
    let parked = request_surface(&snapshot, bloom, &workpiece, scope_revision);

    let (after, decided) = step(
        &parked,
        &event(
            "verify-failed",
            Fact::VerifyFailed {
                bloom,
                workpiece: workpiece.clone(),
                evidence: Evidence {
                    subject: scope_revision,
                    kind: EvidenceKind::VerificationResult,
                    detail: digest(82),
                },
                failed_verifiers: VerifyFailureSet::one(VerifyFailure::Fmt),
            },
        ),
    );

    assert!(
        matches!(
            decided.outcome,
            Outcome::VerifyFailedRejected(VerifyFailedError::StageMismatch { expected: StageId::Construct })
        ),
        "the member is still at Construct, so the verdict is refused: {decided:?}",
    );
    assert!(
        after.awaiting_surface(&bloom, &workpiece).is_some(),
        "a fact the reducer rejected changes nothing in the fold",
    );
}

// The plausible bug: a completion that *looks* like a pass still named
// `passed: true`, and the fold cleared the hold from that field even when
// the reducer refused the stage as a mismatch.
#[test]
fn a_rejected_passing_attempt_leaves_the_surface_hold() {
    let (snapshot, bloom, workpiece, scope_revision) = sealed();
    let parked = request_surface(&snapshot, bloom, &workpiece, scope_revision);

    let (after, decided) = step(
        &parked,
        &event(
            "stale-pass",
            Fact::AttemptCompleted {
                bloom,
                workpiece: workpiece.clone(),
                stage: StageId::Refine,
                passed: true,
                evidence: Evidence { subject: digest(70), kind: EvidenceKind::VerificationResult, detail: digest(80) },
                candidate: Some(CandidateRef { tree: digest(40), checkout: digest(41) }),
            },
        ),
    );

    assert!(
        matches!(
            decided.outcome,
            Outcome::AttemptCompletedRejected(AttemptCompletedError::StageMismatch {
                expected: StageId::Construct,
                got: StageId::Refine,
            })
        ),
        "a pass for a stage the member has not reached is refused: {decided:?}",
    );
    assert!(
        after.awaiting_surface(&bloom, &workpiece).is_some(),
        "a fact the reducer rejected changes nothing in the fold",
    );
}

// The plausible bug: `Fact::Integrate` released leases and dropped the surface
// hold without gating on `Outcome::Integrated`, so a claim the reducer refused
// still emptied both tables.
#[test]
fn a_rejected_integrate_leaves_surface_holds_and_file_leases() {
    let (snapshot, bloom, workpiece, scope_revision) = sealed();
    let snapshot = observe_write(&snapshot, bloom, &workpiece);
    let parked = request_surface(&snapshot, bloom, &workpiece, scope_revision);

    let (after, decided) =
        step(&parked, &event("integrate", Fact::Integrate { bloom, claim: unbound_claim(&workpiece, scope_revision) }));

    assert!(
        matches!(decided.outcome, Outcome::IntegrateRejected(IntegrateError::EvidenceNotBound)),
        "an unbound claim is refused: {decided:?}",
    );
    assert!(
        after.awaiting_surface(&bloom, &workpiece).is_some(),
        "a fact the reducer rejected changes nothing in the fold",
    );
    assert_eq!(
        after.file_lease(&bloom, LEASED_PATH).unwrap().holder,
        workpiece,
        "a refused integrate must not release the member's leases",
    );
}

// The plausible bug: gating the hold clear on the accepted outcome never
// opens, so a request outlives the passing attempt that made it false.
#[test]
fn a_passing_attempt_still_clears_the_surface_hold() {
    let (snapshot, bloom, workpiece, scope_revision) = sealed();
    let parked = request_surface(&snapshot, bloom, &workpiece, scope_revision);

    let (after, decided) = step(
        &parked,
        &event(
            "pass",
            Fact::AttemptCompleted {
                bloom,
                workpiece: workpiece.clone(),
                stage: StageId::Construct,
                passed: true,
                evidence: Evidence { subject: digest(70), kind: EvidenceKind::VerificationResult, detail: digest(80) },
                candidate: Some(CandidateRef { tree: digest(40), checkout: digest(41) }),
            },
        ),
    );

    assert!(
        matches!(decided.outcome, Outcome::AttemptAdvanced { from: StageId::Construct, .. }),
        "the gate opens on the accepted pass: {decided:?}",
    );
    assert_eq!(after.awaiting_surface(&bloom, &workpiece), None, "a passing attempt redeems the hold");
}

// The plausible bug: gating the hold-and-lease clear on `Outcome::Integrated`
// never opens, so both tables outlive the integration that made them false.
#[test]
fn an_integration_still_clears_surface_holds_and_file_leases() {
    let (snapshot, bloom, workpiece, scope_revision) = sealed();
    let snapshot = observe_write(&snapshot, bloom, &workpiece);
    let parked = request_surface(&snapshot, bloom, &workpiece, scope_revision);

    let (after, decided) = step(&parked, &event("integrate", Fact::Integrate { bloom, claim: claim("wp", 1, 51) }));

    assert!(
        matches!(decided.outcome, Outcome::Integrated { .. }),
        "the gate opens on the accepted integration: {decided:?}",
    );
    assert_eq!(after.awaiting_surface(&bloom, &workpiece), None, "an integration redeems the hold");
    assert!(after.file_lease(&bloom, LEASED_PATH).is_none(), "an integration releases the member's leases");
}
