//! Arm of [`super::reduce`]'s fact dispatch (`Fact::Land`); wiring lives in
//! `mod.rs`.
//!
//! The compare-and-swap land: mainline moves onto a resolved bloom, or the
//! bloom is refused and a successor seals on the new head (ADR-0149 §The bloom).

use alloc::format;

use super::boundary::EventBoundary;
use super::gate::LAND_GATE;
use super::{BaseMismatch, BloomStatus, Decision, Decisions, LandError, Outcome, Snapshot};
use crate::digest::Digest;
use crate::ids::{BloomId, WorkpieceId};
use crate::port::ProjectedReceipt;
use crate::reads;
use crate::values::{BaseReceipt, BaseVerdict, LandingReceipt, VerifyGateSet};

/// The `land` boundary (ADR-0206): mainline advances onto a resolved bloom's
/// head, or the record says which guard stopped it.
///
/// The unknown-bloom lookup stays an ordinary early return. It is an addressing
/// error rather than a state an operator interrogates — there is no record to
/// file a refusal against, and filing one would mint a snapshot entry for a
/// bloom that does not exist.
pub(super) fn reduce_land(snapshot: &Snapshot, bloom: &BloomId, new_head: &Digest) -> Decisions {
    let Some(record) = snapshot.blooms.get(bloom) else {
        return Decisions::rejected(Outcome::LandRejected(LandError::UnknownBloom(*bloom)));
    };
    // Compare-and-swap against the bloom's own sealed base — the only head a V1
    // bloom may land on. A moved mainline forces supersession, never a land onto
    // the new head (ADR-0149 §The bloom).
    let base = record.spec.base();
    EventBoundary::new(LAND_GATE, *bloom)
        .require(
            "bloom_resolved",
            || record.status == BloomStatus::Resolved,
            || reads![status: format!("{:?}", record.status), required: "Resolved"],
            || Outcome::LandRejected(LandError::NotResolved(*bloom)),
        )
        .require(
            "mainline_at_sealed_base",
            || snapshot.mainline == base,
            || reads![sealed_base: base.to_hex(), mainline: snapshot.mainline.to_hex()],
            || {
                Outcome::LandRejected(LandError::BaseMismatch(BaseMismatch {
                    expected: base,
                    actual: snapshot.mainline,
                }))
            },
        )
        .decide(|| landing(snapshot, record, *bloom, base, *new_head))
}

/// The effects a passing land produces, lifted out so the guard list above is
/// one readable surface.
fn landing(
    snapshot: &Snapshot,
    record: &super::BloomRecord,
    bloom: BloomId,
    base: Digest,
    new_head: Digest,
) -> Decisions {
    let receipt = LandingReceipt { bloom, previous_base: base, new_head };
    // Release the landed bloom's memberships from `active`, then advance
    // mainline and emit the receipt — one atomic decision set (m5: a land frees
    // its workpieces so the next bloom may seal them).
    let mut effects: Vec<Decision> = record
        .spec
        .members()
        .iter()
        .map(|member| Decision::ReleaseMembership { workpiece: member.workpiece.clone(), bloom })
        .collect();
    effects.push(Decision::AdvanceMainline { from: snapshot.mainline, to: new_head });
    // The head this land produces is a tree the line has already answered for,
    // so the next bloom to seal on it should not re-ask (#4891 follow-on).
    effects.extend(landed_base_receipt(snapshot, record, base, new_head));

    // The receipt travels with the membership it was minted from: the value
    // itself names no members, and the outward projection has no other route to
    // the objects a landing belongs on (ADR-0149 §The receipt carries its
    // members). Spec order, so the projection writes in the bloom's own order.
    let members: Vec<WorkpieceId> = record.spec.members().iter().map(|member| member.workpiece.clone()).collect();
    effects.push(Decision::EmitReceipt(ProjectedReceipt { receipt: receipt.clone(), members }));

    Decisions { outcome: Outcome::Landed(receipt), effects }
}

/// The green whole-workspace receipt a land files for the head it produced, or
/// `None` when this bloom's chain does not reach that far.
///
/// The rule, exactly: a receipt is filed only when all three links hold — the
/// head being landed is the one the bloom resolved onto
/// ([`BloomRecord::resolved_head`](super::BloomRecord::resolved_head) is
/// `new_head`), the tree that head carries wears this bloom's own green lane
/// [`VerifyProof`](crate::VerifyProof), and the bloom's sealed base already held
/// a green whole-workspace receipt. Together they say the landed content was
/// reached from a tree the whole-workspace question was answered for, by a fold
/// whose mechanical gates passed over exactly the tree being landed. Break any
/// link — a head nobody resolved onto, a tree whose gates never judged it, a
/// base that was never proven — and nothing is filed: the next seal dispatches a
/// real `verify.base` and pays the honest price.
///
/// This is deliberately *not* the memo's key-equality reuse, which
/// [`VerifyGateSet::base`] keeps separate on purpose — a closure-narrowed member
/// proof must never answer the whole-workspace question by itself, and no lookup
/// here reads a receipt out from under the lane gate set. The receipt is minted
/// under the base gate set on the strength of the chain above. What it inherits
/// is the narrowing's own standing assumption: the lane's closure covers
/// everything the members' diff can reach, so what the base proved at the
/// previous head still holds everywhere the fold did not touch. A land whose
/// base receipt was itself filed here extends the chain by induction, one link
/// per landing, on that same assumption.
fn landed_base_receipt(
    snapshot: &Snapshot,
    record: &super::BloomRecord,
    base: Digest,
    new_head: Digest,
) -> Option<Decision> {
    let tree = record.resolved_tree.filter(|_| record.resolved_head == Some(new_head))?;
    let proof = record.verify_proof_for(tree)?;
    if !snapshot.base_receipt_for(base).is_some_and(BaseReceipt::is_green) {
        return None;
    }

    // Keyed by the landed tree under the base gate set, so the next seal's
    // `base_receipt_for(new_head)` resolves commit → tree → this verdict. The
    // evidence is the lane proof's own, which already binds the tree it judged.
    Some(Decision::RecordBaseReceipt {
        receipt: BaseReceipt {
            base: new_head,
            tree,
            gate_set: VerifyGateSet::base().digest(),
            verdict: BaseVerdict::Green { evidence: proof.evidence.clone() },
        },
    })
}

#[cfg(test)]
mod tests {
    use super::reduce_land;
    use crate::digest::Digest;
    use crate::ids::{BloomId, IdempotencyKey, WorkpieceId};
    use crate::reduce::{BloomStatus, Decision, Decisions, Event, Fact, Outcome, RecordedRefusal, Snapshot, reduce};
    use crate::values::{BloomDraft, ConfigRegistry, Evidence, EvidenceKind, Membership, ResolvedConfigs, SpendWindow};

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
        let mut snapshot = Snapshot::new(base).with_green_base(base);
        snapshot = snapshot.apply(
            &seal,
            &reduce(&snapshot, &seal, &ResolvedConfigs::default(), &SpendWindow::default()),
            &ResolvedConfigs::default(),
        );
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

    /// A sealed-but-unresolved bloom, plus the id it was recorded under.
    fn sealed(base: Digest) -> (Snapshot, BloomId) {
        let spec = BloomDraft { proposals: vec![membership("issue-4628", 10)], base, ..BloomDraft::default() }.seal();
        let bloom = spec.id();
        let seal = Event { idempotency_key: IdempotencyKey("seal".into()), fact: Fact::Seal(spec) };
        let snapshot = Snapshot::new(base).with_green_base(base);
        let decided = reduce(&snapshot, &seal, &ResolvedConfigs::default(), &SpendWindow::default());

        (snapshot.apply(&seal, &decided, &ResolvedConfigs::default()), bloom)
    }

    fn refusal(decisions: &Decisions) -> &RecordedRefusal {
        decisions
            .effects
            .iter()
            .find_map(|effect| match effect {
                Decision::RecordRefusal { refusal, .. } => Some(refusal),
                _ => None,
            })
            .expect("a refused land records why (ADR-0206)")
    }

    #[test]
    fn a_land_of_an_unresolved_bloom_records_the_guard_that_stopped_it() {
        // Pre-fix: the reducer answered `LandRejected(NotResolved)` and the
        // guard that produced it — with the status it actually read — was
        // destroyed at the `return`, so `/why` had nothing to show.
        let base = digest(0);
        let (snapshot, bloom) = sealed(base);

        let decisions = reduce_land(&snapshot, &bloom, &digest(40));

        assert!(matches!(decisions.outcome, Outcome::LandRejected(_)), "the typed rejection is unchanged");
        let recorded = refusal(&decisions);
        assert_eq!(recorded.gate, "land");
        assert_eq!(recorded.guard, "bloom_resolved");
        assert!(recorded.reads.iter().any(|read| read.value.contains("Sealed")), "{:?}", recorded.reads);
    }

    #[test]
    fn a_land_onto_a_moved_mainline_records_both_heads_it_compared() {
        // The second guard, reached only once the first holds — and the one
        // whose reads carry information no status field does: which two heads
        // the compare-and-swap found unequal.
        let base = digest(0);
        let (mut snapshot, bloom) = sealed(base);
        snapshot.blooms.get_mut(&bloom).expect("the seal recorded the bloom").status = BloomStatus::Resolved;
        snapshot.mainline = digest(7);

        let recorded = reduce_land(&snapshot, &bloom, &digest(40));
        let recorded = refusal(&recorded);

        assert_eq!(recorded.guard, "mainline_at_sealed_base");
        let read = |field: &str| {
            recorded.reads.iter().find(|read| read.field == field).map(|read| read.value.clone()).unwrap_or_default()
        };
        assert_eq!(read("sealed_base"), base.to_hex());
        assert_eq!(read("mainline"), digest(7).to_hex());
    }

    #[test]
    fn a_land_that_goes_through_records_no_refusal() {
        // The other half of ADR-0206: a boundary that decided must leave the
        // record clean, or every healthy bloom accumulates a blocker nobody
        // put there.
        let base = digest(0);
        let (mut snapshot, bloom) = sealed(base);
        snapshot.blooms.get_mut(&bloom).expect("the seal recorded the bloom").status = BloomStatus::Resolved;

        let decisions = reduce_land(&snapshot, &bloom, &digest(40));

        assert!(matches!(decisions.outcome, Outcome::Landed(_)));
        assert!(
            !decisions.effects.iter().any(|effect| matches!(effect, Decision::RecordRefusal { .. })),
            "a passing land records nothing"
        );
    }
}
