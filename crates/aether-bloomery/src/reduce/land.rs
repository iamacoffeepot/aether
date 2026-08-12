//! The compare-and-swap land: mainline moves onto a resolved bloom, or the
//! bloom is refused and a successor seals on the new head (ADR-0149 §The bloom).

use super::{BaseMismatch, BloomStatus, Decision, Decisions, LandError, Outcome, Snapshot};
use crate::digest::Digest;
use crate::ids::{BloomId, WorkpieceId};
use crate::port::ProjectedReceipt;
use crate::values::LandingReceipt;

pub(super) fn reduce_land(snapshot: &Snapshot, bloom: &BloomId, new_head: &Digest) -> Decisions {
    let Some(record) = snapshot.blooms.get(bloom) else {
        return Decisions::rejected(Outcome::LandRejected(LandError::UnknownBloom(*bloom)));
    };
    if record.status != BloomStatus::Resolved {
        return Decisions::rejected(Outcome::LandRejected(LandError::NotResolved(*bloom)));
    }
    // Compare-and-swap against the bloom's own sealed base — the only head a V1
    // bloom may land on. A moved mainline forces supersession, never a land onto
    // the new head (ADR-0149 §The bloom).
    let base = record.spec.base();
    if snapshot.mainline != base {
        return Decisions::rejected(Outcome::LandRejected(LandError::BaseMismatch(BaseMismatch {
            expected: base,
            actual: snapshot.mainline,
        })));
    }
    let receipt = LandingReceipt { bloom: *bloom, previous_base: base, new_head: *new_head };
    // Release the landed bloom's memberships from `active`, then advance
    // mainline and emit the receipt — one atomic decision set (m5: a land frees
    // its workpieces so the next bloom may seal them).
    let mut effects: Vec<Decision> = record
        .spec
        .members()
        .iter()
        .map(|member| Decision::ReleaseMembership { workpiece: member.workpiece.clone(), bloom: *bloom })
        .collect();
    effects.push(Decision::AdvanceMainline { from: snapshot.mainline, to: *new_head });

    // The receipt travels with the membership it was minted from: the value
    // itself names no members, and the outward projection has no other route to
    // the objects a landing belongs on (ADR-0149 §The receipt carries its
    // members). Spec order, so the projection writes in the bloom's own order.
    let members: Vec<WorkpieceId> = record.spec.members().iter().map(|member| member.workpiece.clone()).collect();
    effects.push(Decision::EmitReceipt(ProjectedReceipt { receipt: receipt.clone(), members }));

    Decisions { outcome: Outcome::Landed(receipt), effects }
}

#[cfg(test)]
mod tests {
    use super::reduce_land;
    use crate::digest::Digest;
    use crate::ids::{IdempotencyKey, WorkpieceId};
    use crate::reduce::{BloomStatus, Decision, Event, Fact, Outcome, Snapshot, reduce};
    use crate::values::{BloomDraft, ConfigRegistry, Evidence, EvidenceKind, Membership, ResolvedConfigs};

    fn digest(seed: u8) -> Digest {
        Digest::from_bytes([seed; 32])
    }

    /// A membership whose approval binds its own subject (ADR-0174) — the
    /// two-step build the seal door admits.
    fn membership(name: &str, revision: u8) -> Membership {
        let mut member = Membership {
            workpiece: WorkpieceId(name.into()),
            scope_revision: digest(revision),
            configs: ConfigRegistry::default(),
            approval: Evidence { subject: digest(0), kind: EvidenceKind::Approval, detail: digest(200) },
        };
        member.approval.subject = member.subject();
        member
    }

    #[test]
    fn a_land_emits_a_receipt_carrying_every_member_in_spec_order() {
        // Tripwire: the emitted envelope is the outward projection's only route
        // to a landed bloom's membership — `LandingReceipt` names none — so a
        // receipt that loses a member silently stops reaching that member's
        // object, while `Outcome::Landed` must still carry the bare receipt the
        // land fact and the source port are written against.
        let base = digest(0);
        let spec = BloomDraft {
            proposals: vec![membership("issue-4628", 10), membership("issue-4629", 20)],
            base,
            ..BloomDraft::default()
        }
        .seal();
        let bloom = spec.id();
        let sealed_order: Vec<WorkpieceId> = spec.members().iter().map(|member| member.workpiece.clone()).collect();

        let seal = Event { idempotency_key: IdempotencyKey("seal".into()), fact: Fact::Seal(spec) };
        let mut snapshot = Snapshot::new(base);
        snapshot =
            snapshot.apply(&seal, &reduce(&snapshot, &seal, &ResolvedConfigs::default()), &ResolvedConfigs::default());
        snapshot.blooms.get_mut(&bloom).expect("the seal recorded the bloom under its own spec id").status =
            BloomStatus::Resolved;

        let decisions = reduce_land(&snapshot, &bloom, &digest(40));

        let Outcome::Landed(receipt) = &decisions.outcome else {
            panic!("a land on the sealed base lands");
        };
        let projected = decisions
            .effects
            .iter()
            .find_map(|effect| match effect {
                Decision::EmitReceipt(projected) => Some(projected),
                _ => None,
            })
            .expect("a land emits its receipt");

        assert_eq!(&projected.receipt, receipt, "the envelope carries the same receipt the outcome does");
        assert_eq!(projected.members, sealed_order, "every member, in the sealed spec's canonical order");
    }
}
