//! Arm of [`super::reduce`]'s fact dispatch (`Fact::ProposeChange`); wiring
//! lives in `mod.rs`.
//!
//! An operator-supplied change waits in the journal until the board is clear,
//! then the propose reactor seals it as a memberless bloom (ADR-0205).

use super::seal::active_unlanded_bloom;
use super::{Decision, Decisions, Outcome, ProposalError, Snapshot};
use crate::digest::{Digest, digest_of};
use crate::values::{Evidence, EvidenceKind, OperatorProposal};

/// Reduce an operator proposal ([`crate::Fact::ProposeChange`]).
///
/// A blank reason or operator is refused the way a repair is. An authorization
/// that is not an approval, or that binds a digest other than this proposal's,
/// is refused rather than warned about: journal write access is not
/// authorization to write the day's branch.
pub(super) fn reduce_propose(snapshot: &Snapshot, proposal: &OperatorProposal, authorization: &Evidence) -> Decisions {
    let rejected = |error: ProposalError| Decisions::rejected(Outcome::ProposalRejected(error));

    if !stated(&proposal.reason) {
        return rejected(ProposalError::BlankReason);
    }
    if !stated(&proposal.operator) {
        return rejected(ProposalError::BlankOperator);
    }
    let digest = digest_of(proposal);
    if authorization.kind != EvidenceKind::Approval {
        return rejected(ProposalError::NotAnApproval);
    }
    if !authorization.validates(&digest) {
        return rejected(ProposalError::SubjectMismatch);
    }

    let mut effects = alloc::vec![Decision::QueueProposal { proposal: proposal.clone() }];
    let offered = offer_queued_proposal(snapshot, Some(proposal), snapshot.mainline).is_some_and(|offer| {
        effects.push(offer);
        true
    });

    Decisions { outcome: Outcome::ProposalQueued { proposal: digest, offered }, effects }
}

/// The one construction of a [`Decision::DispatchProposal`]: name the queue
/// head and the base it should seal against. Both offer sites go through here
/// so they cannot enqueue different shapes.
pub(super) fn offer_proposal(proposal: &OperatorProposal, base: Digest) -> Decision {
    Decision::DispatchProposal { proposal: proposal.clone(), base }
}

/// Offer the queue head when the board is clear. `newly_queued` is the
/// proposal this reduction just accepted, which is not yet in
/// [`Snapshot::queued_proposals`].
pub(super) fn offer_queued_proposal(
    snapshot: &Snapshot,
    newly_queued: Option<&OperatorProposal>,
    base: Digest,
) -> Option<Decision> {
    if active_unlanded_bloom(snapshot).is_some() {
        return None;
    }
    let head = snapshot.queued_proposals.first().or(newly_queued)?;
    Some(offer_proposal(head, base))
}

/// Offer the queue head after a land has cleared the board. The land's own
/// bloom is still `Resolved` on the snapshot this reads, so this does not
/// consult [`active_unlanded_bloom`].
pub(super) fn offer_after_land(snapshot: &Snapshot, new_head: Digest) -> Option<Decision> {
    snapshot.queued_proposals.first().map(|head| offer_proposal(head, new_head))
}

fn stated(text: &str) -> bool {
    !text.trim().is_empty()
}

#[cfg(test)]
mod tests {
    use super::{offer_after_land, reduce_propose};
    use crate::digest::{Digest, digest_of};
    use crate::ids::{IdempotencyKey, WorkpieceId};
    use crate::reduce::{BloomStatus, Decision, Event, Fact, Outcome, ProposalError, Snapshot, reduce};
    use crate::values::{
        BloomDraft, CandidateRef, ConfigRegistry, Evidence, EvidenceKind, Membership, OperatorProposal,
        ResolvedConfigs, SpendWindow,
    };

    fn digest(seed: u8) -> Digest {
        Digest::from_bytes([seed; 32])
    }

    fn proposal() -> OperatorProposal {
        OperatorProposal {
            candidate: CandidateRef { tree: digest(7), checkout: digest(8) },
            reason: "flip an ADR status".into(),
            operator: "operator".into(),
        }
    }

    fn approval_of(proposal: &OperatorProposal) -> Evidence {
        Evidence { subject: digest_of(proposal), kind: EvidenceKind::Approval, detail: digest(1) }
    }

    fn membership(name: &str) -> Membership {
        let mut member = Membership {
            workpiece: WorkpieceId(name.into()),
            scope_revision: digest(10),
            configs: ConfigRegistry::default(),
            approval: Evidence { subject: digest(0), kind: EvidenceKind::Approval, detail: digest(200) },
        };
        member.approval.subject = member.subject();
        member
    }

    fn sealed_snapshot() -> Snapshot {
        let spec = BloomDraft { proposals: vec![membership("wp")], base: digest(0), ..BloomDraft::default() }.seal();
        let seal = Event { idempotency_key: IdempotencyKey("seal".into()), fact: Fact::Seal(spec) };
        let snapshot = Snapshot::new(digest(0)).with_green_base(digest(0));
        snapshot.apply(
            &seal,
            &reduce(&snapshot, &seal, &ResolvedConfigs::default(), &SpendWindow::default()),
            &ResolvedConfigs::default(),
        )
    }

    #[test]
    fn an_unsigned_proposal_is_refused() {
        // A door that admitted an authorization whose kind is not Approval
        // would make journal write access sufficient to write the day's
        // branch, which is the authorization ADR-0205 replaces.
        let proposal = proposal();
        let unsigned =
            Evidence { subject: digest_of(&proposal), kind: EvidenceKind::VerificationResult, detail: digest(1) };
        let decided = reduce_propose(&Snapshot::new(digest(0)), &proposal, &unsigned);
        assert!(matches!(decided.outcome, Outcome::ProposalRejected(ProposalError::NotAnApproval)));
        assert!(decided.effects.is_empty(), "an unsigned proposal journals no queue entry");
    }

    #[test]
    fn a_proposal_signed_over_another_proposal_is_refused() {
        // A door that admitted evidence binding a different digest would let
        // one captured approval write any later change.
        let proposal = proposal();
        let other = Evidence { subject: digest(9), kind: EvidenceKind::Approval, detail: digest(1) };
        let decided = reduce_propose(&Snapshot::new(digest(0)), &proposal, &other);
        assert!(matches!(decided.outcome, Outcome::ProposalRejected(ProposalError::SubjectMismatch)));
        assert!(decided.effects.is_empty(), "a wrongly-signed proposal journals no queue entry");
    }

    #[test]
    fn a_proposal_with_a_blank_reason_is_refused() {
        let mut proposal = proposal();
        proposal.reason = "   ".into();
        let decided = reduce_propose(&Snapshot::new(digest(0)), &proposal, &approval_of(&proposal));
        assert!(matches!(decided.outcome, Outcome::ProposalRejected(ProposalError::BlankReason)));
    }

    #[test]
    fn a_signed_proposal_queues_and_offers_when_the_board_is_clear() {
        let proposal = proposal();
        let snapshot = Snapshot::new(digest(0));
        let decided = reduce_propose(&snapshot, &proposal, &approval_of(&proposal));
        assert!(matches!(decided.outcome, Outcome::ProposalQueued { offered: true, .. }));
        assert!(
            decided.effects.iter().any(|effect| matches!(effect, Decision::QueueProposal { .. })),
            "the proposal is queued: {decided:?}"
        );
        assert!(
            decided.effects.iter().any(|effect| matches!(
                effect,
                Decision::DispatchProposal { base, .. } if *base == snapshot.mainline
            )),
            "a clear board offers the head: {decided:?}"
        );
    }

    #[test]
    fn a_signed_proposal_queues_without_offering_while_a_bloom_walks() {
        let proposal = proposal();
        let snapshot = sealed_snapshot();
        assert!(snapshot.blooms.values().any(|record| record.status == BloomStatus::Sealed));
        let decided = reduce_propose(&snapshot, &proposal, &approval_of(&proposal));
        assert!(matches!(decided.outcome, Outcome::ProposalQueued { offered: false, .. }));
        assert!(
            !decided.effects.iter().any(|effect| matches!(effect, Decision::DispatchProposal { .. })),
            "a walking bloom must not be interleaved with a proposal: {decided:?}"
        );
    }

    #[test]
    fn a_land_offers_the_queued_head_on_the_new_mainline() {
        let proposal = proposal();
        let mut snapshot = Snapshot::new(digest(0));
        snapshot.queued_proposals.push(proposal.clone());
        let offer = offer_after_land(&snapshot, digest(40)).expect("a non-empty queue offers after land");
        assert!(matches!(
            offer,
            Decision::DispatchProposal { base, proposal: offered } if base == digest(40) && offered == proposal
        ));
    }
}
