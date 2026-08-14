//! The landing gate's rejection path (#4689): what a bloom does when the
//! proposal carrying its resolved artifact cannot merge.
//!
//! Every other gate judges content the bloom controls. A member verifies its
//! own candidate; the aggregate verify builds the fold. Neither sees the one
//! thing that changes underneath a bloom while it works — the mainline it
//! sealed against moving on. That only fails at the landing branch, downstream
//! of every gate inside the loop, which is why the loop needs a way back in
//! rather than another gate.

use super::composition::{Refusal, finding_of, reweave};
use super::{BloomStatus, Decision, Decisions, LandingRejectedError, Outcome, Snapshot};
use crate::ids::{BloomId, StageId};
use crate::values::Evidence;

/// Reduce a refused landing.
///
/// Below the `Land` binding's retry budget the bloom un-resolves and the
/// composition repairs its weave against the mainline that refused it. At the
/// budget it parks to the owner: a landing branch that stays red after a repair
/// is not something the machine can answer by trying again, and re-proposing
/// forever is exactly the behaviour this replaces.
///
/// The composition owns the repair (ADR-0191 §4): CI names no owners, and a
/// conflict with a moved mainline belongs to the composed tree rather than to a
/// member that passed on its own — and a member that passed its review is never
/// re-opened by a bloom-level outcome.
pub(super) fn reduce_landing_rejected(snapshot: &Snapshot, bloom: &BloomId, evidence: &Evidence) -> Decisions {
    let Some(record) = snapshot.blooms.get(bloom) else {
        return Decisions::rejected(Outcome::LandingRejectedRefused(LandingRejectedError::NotAwaitingLanding));
    };
    // Only a resolved bloom is awaiting a landing. A rejection against one that
    // already landed, was superseded, or is still working names a proposal that
    // is no longer the bloom's, so it changes nothing.
    if record.status != BloomStatus::Resolved {
        return Decisions::rejected(Outcome::LandingRejectedRefused(LandingRejectedError::NotAwaitingLanding));
    }
    let Some(head) = record.resolved_head else {
        return Decisions::rejected(Outcome::LandingRejectedRefused(LandingRejectedError::NotAwaitingLanding));
    };
    if !evidence.validates(&head) {
        return Decisions::rejected(Outcome::LandingRejectedRefused(LandingRejectedError::SubjectMismatch {
            expected: head,
            got: evidence.subject,
        }));
    }

    let rolls = record.landing_rolls + 1;
    let mut effects = alloc::vec![
        Decision::RecordEvidence { bloom: *bloom, evidence: evidence.clone() },
        Decision::RecordLandingRoll { bloom: *bloom, rolls },
    ];

    if rolls >= record.stage_catalog.retry_budget_of(StageId::Land).unwrap_or(1) {
        // The budget is spent. The bloom stays resolved — the owner's decision
        // context is the artifact that keeps being refused — and parks under
        // the rejection's record artifact, which an adopting answer must name
        // to re-arm the cycle. The refusal files its finding first (#4977), on
        // the same channel and against the same head the repair below binds: a
        // landing that stays red is a refusal of the composed tree, and the
        // operator reading a parked bloom is reading exactly that finding.
        effects.push(finding_of(*bloom, head, evidence, &[]));
        effects.push(Decision::RecordReviewPark { bloom: *bloom, question: Some(evidence.detail) });
        return Decisions {
            outcome: Outcome::LandingParked { bloom: *bloom, rolls, question: evidence.detail },
            effects,
        };
    }

    // Un-resolve before repairing: a resolved bloom is land-ready by definition,
    // and leaving it resolved while the weave is repaired would let the land
    // reactor re-propose the head that just failed.
    effects.push(Decision::SetUnresolved { bloom: *bloom });
    // The refused head *is* the composition's candidate, so the repair belongs
    // to the composition (ADR-0191 §4/§5): a conflict with a mainline that moved
    // is a property of the weave, not of a member that passed its own review.
    // The head stands in for both digests here — a landing rejection binds the
    // head it judged and the bloom no longer holds the fold that produced it.
    let repair =
        reweave(record, bloom, &Refusal { refused_at: StageId::Land, tree: head, head, evidence, implicated: &[] });
    effects.extend(repair.effects);

    Decisions { outcome: repair.outcome, effects }
}
