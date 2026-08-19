//! Lane-boundary liveness tripwires. The classifier lives with the harness;
//! the tests stay here so only this binary runs them.

pub use crate::harness::liveness::*;

#[cfg(test)]
mod tests {
    use aether_bloomery::testing::digest;
    use aether_bloomery::{
        BloomId, BloomStatus, BloomView, Digest, Evidence, EvidenceKind, ExecutorFaultView, MemberView, ViewDocument,
        Wedge, WorkpieceId,
    };

    use super::{Quiescence, classify};

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
        let wedge = Wedge {
            stage: aether_bloomery::StageId::Verify,
            evidence: digest(9),
            repeated_verifiers: aether_bloomery::VerifyFailureSet::EMPTY,
        };

        assert!(matches!(
            classify(&document(BloomStatus::Sealed, vec![member(false, Some(wedge))]), &[]),
            Quiescence::Wedged(_)
        ));
        assert!(matches!(
            classify(&document(BloomStatus::Resolved, vec![member(true, None)]), &[]),
            Quiescence::Terminal(_)
        ));
    }
}
