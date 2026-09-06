//! Lane-boundary liveness tripwires. The classifier lives with the harness;
//! these tests pin the stall vs accountable-stop distinction.

#![allow(clippy::unwrap_used)]

use aether_bloomery::testing::digest;
use aether_bloomery::{
    AwaitingSurfaceView, BloomId, BloomStatus, BloomView, Digest, Evidence, EvidenceKind, Excuse, ExecutorFaultView,
    HostFaultView, LeaseEvictionView, MemberPark, MemberView, OperatorHold, StageId, VerifyFailureSet, ViewDocument,
    Wedge, WithdrawnView, WorkpieceId,
};
use aether_harness_bloomery::{Oracle, Progress, Quiescence, classify, is_answerable};

fn member(resolved: bool, wedge: Option<Wedge>) -> MemberView {
    MemberView {
        workpiece: WorkpieceId("wp".to_owned()),
        scope_revision: digest(1),
        approval: Evidence { subject: Digest::default(), kind: EvidenceKind::Approval, detail: Digest::default() },
        resolution: resolved.then(|| aether_bloomery::ResolutionClaim {
            workpiece: WorkpieceId("wp".to_owned()),
            scope_revision: digest(1),
            candidate: digest(2),
            evidence: Evidence {
                subject: Digest::default(),
                kind: EvidenceKind::ResolutionClaim,
                detail: Digest::default(),
            },
        }),
        wedge,
        ..MemberView::default()
    }
}

fn document(status: BloomStatus, members: Vec<MemberView>) -> ViewDocument {
    ViewDocument {
        blooms: vec![BloomView { id: BloomId(digest(7)), status, members, ..BloomView::default() }],
        ..ViewDocument::default()
    }
}

#[test]
fn a_sealed_bloom_with_nothing_in_flight_is_a_failure() {
    // Tripwire: this is the exact state the live coordinator sat in for five
    // hours — sealed, no wedge, empty outbox, nothing outstanding. If it
    // ever classifies as anything but a stall, every scenario in this tier
    // goes quietly green on a dead coordinator.
    let stalled = classify(&document(BloomStatus::Sealed, vec![member(false, None)]), &[]);

    assert!(matches!(stalled, Quiescence::Stalled(_)), "quiescence with work owed must fail: {stalled:?}");
}

#[test]
fn a_still_world_with_a_lane_in_flight_is_not_answerable() {
    // Tripwire: a running lane sits in the outstanding set while nothing else
    // in the fingerprint changes. Three still polls of that world used to call
    // classify, which reports every non-empty outstanding set as an order that
    // never completed.
    let in_flight =
        Progress::observe(&document(BloomStatus::Sealed, vec![member(false, None)]), vec!["n-running".to_owned()], 0);
    let still = Progress::observe(&document(BloomStatus::Sealed, vec![member(false, None)]), Vec::new(), 0);

    assert!(!is_answerable(&in_flight), "a lane in flight is not a standstill: {in_flight:?}");
    assert!(is_answerable(&still), "nothing outstanding is the standstill classify answers");
}

#[test]
fn an_order_that_never_completed_is_a_failure_however_the_bloom_reads() {
    // Tripwire: the second invariant, and the reason it is checked before
    // the projection. An order left in `outstanding_orders` advances no
    // counter, so a bloom can read perfectly resolved while a lane it
    // forgot is owed forever.
    let stalled = classify(&document(BloomStatus::Resolved, vec![member(true, None)]), &["n-lost".to_owned()]);

    assert!(matches!(stalled, Quiescence::Stalled(_)), "an outstanding order outranks a clean projection");
}

#[test]
fn a_terminal_executor_fault_is_an_accountable_stop_not_a_finished_bloom() {
    // Tripwire (ADR-0176): a bloom at its executor-fault ceiling has every
    // member resolved and its fold still held, so the member-shaped tests
    // above all pass on it. Classifying that as `Terminal` would let a bloom
    // stopped dead on a broken host read as one that finished its work.
    let faulted = ViewDocument {
        blooms: vec![BloomView {
            id: BloomId(digest(7)),
            status: BloomStatus::Sealed,
            members: vec![member(true, None)],
            executor_fault: Some(ExecutorFaultView {
                subject: digest(3),
                rolls: 2,
                budget: 2,
                evidence: digest(9),
                terminal: true,
            }),
            ..BloomView::default()
        }],
        ..ViewDocument::default()
    };

    assert!(matches!(classify(&faulted, &[]), Quiescence::Wedged(_)));
}

#[test]
fn a_wedge_is_a_legitimate_stop_and_a_resolution_is_a_terminal_one() {
    let wedge = Wedge { stage: StageId::Verify, evidence: digest(9), repeated_verifiers: VerifyFailureSet::EMPTY };

    assert!(matches!(
        classify(&document(BloomStatus::Sealed, vec![member(false, Some(wedge))]), &[]),
        Quiescence::Wedged(_)
    ));
    assert!(matches!(
        classify(&document(BloomStatus::Resolved, vec![member(true, None)]), &[]),
        Quiescence::Terminal(_)
    ));
}

#[test]
fn a_construct_park_is_an_accountable_stop() {
    // Tripwire (#5332): a parked member used to look like a stall because
    // neither oracle read `Snapshot::member_parks`. A named park is a
    // stop with an operator exit — the declared surface — not work owed.
    let parked =
        MemberView { park: Some(MemberPark { stage: StageId::Construct, evidence: digest(9) }), ..member(false, None) };

    assert!(matches!(classify(&document(BloomStatus::Sealed, vec![parked]), &[]), Quiescence::Wedged(_)));
}

fn operator_hold() -> OperatorHold {
    OperatorHold { reason: "the run looks wrong".into(), operator: "eve".into() }
}

fn held_document(status: BloomStatus, members: Vec<MemberView>) -> ViewDocument {
    let mut document = document(status, members);
    document.blooms[0].operator_hold = Some(operator_hold());
    document
}

#[test]
fn a_bloom_hold_is_an_accountable_stop_for_that_blooms_unresolved_members() {
    // Tripwire (#4976): after the last in-flight lane finishes under a
    // bloom-wide operator hold, no outstanding order or member-level excuse
    // remains. That pause is the hold, not a stall. Clearing the hold must
    // re-expose the same unresolved members as work owed.
    let held = held_document(BloomStatus::Sealed, vec![member(false, None)]);
    let quiescence = classify(&held, &[]);

    assert!(
        matches!(quiescence, Quiescence::Wedged(_)),
        "a hold with no outstanding work is an accountable stop: {quiescence:?}"
    );
    Oracle::check(&held, None, &[])
        .unwrap_or_else(|violation| panic!("termination must agree the hold is a named stop: {violation}"));

    let mut released = held;
    released.blooms[0].operator_hold = None;
    let stalled = classify(&released, &[]);
    assert!(
        matches!(stalled, Quiescence::Stalled(_)),
        "releasing the hold re-exposes the unresolved stall: {stalled:?}"
    );
}

#[test]
fn a_bloom_hold_does_not_excuse_an_outstanding_order() {
    // Tripwire: outstanding still outranks the projection. A hold that froze
    // dispatch does not explain a lane that never completed.
    let stalled = classify(&held_document(BloomStatus::Sealed, vec![member(false, None)]), &["n-lost".to_owned()]);

    assert!(matches!(stalled, Quiescence::Stalled(_)), "an outstanding order outranks a bloom-wide hold: {stalled:?}");
}

#[test]
fn a_held_bloom_does_not_excuse_another_blooms_unresolved_members() {
    // Tripwire: the hold is per bloom. A sibling with no hold and no member
    // excuse is still work owed.
    let mixed = ViewDocument {
        blooms: vec![
            BloomView {
                id: BloomId(digest(7)),
                status: BloomStatus::Sealed,
                members: vec![member(false, None)],
                operator_hold: Some(operator_hold()),
                ..BloomView::default()
            },
            BloomView {
                id: BloomId(digest(8)),
                status: BloomStatus::Sealed,
                members: vec![MemberView { workpiece: WorkpieceId("other".to_owned()), ..member(false, None) }],
                ..BloomView::default()
            },
        ],
        ..ViewDocument::default()
    };

    match classify(&mixed, &[]) {
        Quiescence::Stalled(why) => {
            assert!(why.contains("other"), "the unheld sibling is the work owed: {why}");
        }
        other => panic!("a hold on one bloom must not quiet another's unexplained members: {other:?}"),
    }
}

fn member_carrying(excuse: Excuse) -> MemberView {
    let mut member = member(false, None);
    match excuse {
        Excuse::Wedge => {
            member.wedge = Some(Wedge {
                stage: StageId::Verify,
                evidence: digest(9),
                repeated_verifiers: VerifyFailureSet::EMPTY,
            });
        }
        Excuse::Claim => {
            member.resolution = Some(aether_bloomery::ResolutionClaim {
                workpiece: WorkpieceId("wp".to_owned()),
                scope_revision: digest(1),
                candidate: digest(2),
                evidence: Evidence {
                    subject: Digest::default(),
                    kind: EvidenceKind::ResolutionClaim,
                    detail: Digest::default(),
                },
            });
        }
        Excuse::HostFault => member.host_fault = Some(HostFaultView { findings: "missing tool".into() }),
        Excuse::Park => {
            member.park = Some(MemberPark { stage: StageId::Construct, evidence: digest(9) });
        }
        Excuse::AwaitingSurface => {
            member.awaiting_surface = Some(AwaitingSurfaceView {
                stage: StageId::Construct,
                scope_revision: digest(1),
                evidence: digest(9),
                paths: Vec::new(),
                summary: "need a path".into(),
                requests: 1,
            });
        }
        Excuse::LeaseEviction => {
            member.evicted_by =
                Some(LeaseEvictionView { by: WorkpieceId("sibling".into()), path: "src/lib.rs".into(), evicted_at: 0 });
        }
        Excuse::Withdrawal => {
            member.withdrawn = Some(WithdrawnView {
                cause: "operator".into(),
                depends_on: None,
                reason: "stop".into(),
                operator: "op".into(),
            });
        }
    }
    member
}

#[test]
fn every_excuse_kind_is_read_by_every_reader() {
    // Tripwire: an excuse kind added to `Excuse` but not to a reader's match
    // is the drift this enumeration exists to close — the member would look
    // like a stall to one oracle and an accountable stop to another.
    for excuse in Excuse::ALL {
        let doc = document(BloomStatus::Sealed, vec![member_carrying(*excuse)]);
        let quiescence = classify(&doc, &[]);
        assert!(
            !matches!(quiescence, Quiescence::Stalled(_)),
            "{excuse:?} must be an accountable stop to liveness, got {quiescence:?}"
        );
        Oracle::check(&doc, None, &[])
            .unwrap_or_else(|violation| panic!("{excuse:?} must be an accountable stop to termination: {violation}"));
    }
}
