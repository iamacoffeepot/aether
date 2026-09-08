//! The bloom-level reader at the tail of the line (ADR-0216): the dispatch a
//! landing decides, and the result a landed bloom files.
//!
//! `Study` consumes `bloom.receipt` and produces `bloom.study`. The receipt is
//! [`super::land`]'s product, so this is the one position on the line whose
//! subject does not exist until the bloom is over — which is also why it is the
//! one position that cannot gate anything. Findings are a product, never a
//! gate: the bloom has already landed when the read is dispatched, and it is
//! still landed however the read ends.
//!
//! That shapes both halves below. The dispatch is decided in the same decision
//! set the landing receipt is emitted in, and it is emitted once — the binding
//! carries a retry budget of one attempt, and nothing here re-dispatches. The
//! completion files evidence and stops: no cursor moves, no wedge is recorded,
//! no lane is bought. A read that failed, faulted, or was cancelled at its
//! deadline reaches the same arm as one that succeeded, and leaves the bloom
//! exactly where the landing left it, with its study missing.

use super::attempt::stage_binding;
use super::{BloomStatus, Decision, Decisions, Outcome, Snapshot, StudyError};
use crate::ids::{BloomId, StageId};
use crate::values::{Evidence, LandingReceipt, Transformation};

/// The reader's work order for the bloom that just landed (ADR-0216 §1).
///
/// `subject` is the receipt's own content address — the `bloom.receipt` the
/// binding consumes, and the digest the returning evidence binds to. The
/// checkout is the head mainline just advanced to and the diff base is the
/// bloom's sealed base, so the lane reads `base..new_head`: the landed range,
/// not the working tree of a clean checkout.
///
/// The profile and the registry are the bloom's own sealed ones, read the way
/// every other bloom-level dispatch reads them (ADR-0174) — the host resolves
/// the sealed `ModelOverride` and the ADR-0214 instruction pin out of that
/// registry, because the reader is a model lane like the critic.
pub(super) fn dispatch_after_land(record: &super::BloomRecord, receipt: &LandingReceipt) -> Decision {
    let binding = stage_binding(&record.stage_catalog, StageId::Study);

    Decision::DispatchStudy {
        bloom: receipt.bloom,
        transformation: Transformation::for_study_read(
            &binding,
            receipt.digest(),
            receipt.new_head,
            receipt.previous_base,
        ),
        profile: binding.profile,
        configs: record.spec.configs().clone(),
    }
}

/// Reduce the reader's result ([`crate::Fact::StudyCompleted`]).
///
/// The whole decision is one row on the evidence log. A passing read files what
/// it produced; a failing or faulted one files that it produced nothing. Either
/// way the bloom keeps the status the landing gave it, because there is nothing
/// left for a verdict here to hold: every member has been released, mainline has
/// moved, and the receipt is written.
///
/// What this deliberately does not emit is the point of the ADR's
/// "findings are a product, never a gate": no [`Decision::RecordWedge`], no
/// re-dispatch of the lane inside a budget it does not have, and no hold. A
/// reader that cannot read is a missing study, not a stopped line.
pub(super) fn reduce_study_completed(
    snapshot: &Snapshot,
    bloom: &BloomId,
    passed: bool,
    evidence: &Evidence,
) -> Decisions {
    let Some(record) = snapshot.blooms.get(bloom) else {
        return Decisions::rejected(Outcome::StudyRejected(StudyError::UnknownBloom));
    };
    if record.status != BloomStatus::Landed {
        return Decisions::rejected(Outcome::StudyRejected(StudyError::NotLanded));
    }

    Decisions {
        outcome: Outcome::StudyRecorded { bloom: *bloom, passed },
        effects: alloc::vec![Decision::RecordEvidence { bloom: *bloom, evidence: evidence.clone() }],
    }
}

#[cfg(test)]
mod tests {
    use super::reduce_study_completed;
    use crate::digest::Digest;
    use crate::ids::{BloomId, IdempotencyKey, StageId, WorkpieceId};
    use crate::reduce::{BloomStatus, Decision, Decisions, Event, Fact, Outcome, Snapshot, reduce};
    use crate::values::{
        BloomDraft, ConfigRegistry, Evidence, EvidenceKind, Membership, ResolvedConfigs, SpendWindow, Transformation,
    };

    fn digest(seed: u8) -> Digest {
        Digest::from_bytes([seed; 32])
    }

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

    /// A bloom carried to the moment before its land, plus its id.
    fn resolved(base: Digest) -> (Snapshot, BloomId) {
        let spec = BloomDraft { proposals: vec![membership("issue-5807", 10)], base, ..BloomDraft::default() }.seal();
        let bloom = spec.id();
        let seal = Event { idempotency_key: IdempotencyKey("seal".into()), fact: Fact::Seal(spec) };
        let snapshot = Snapshot::new(base).with_green_base(base);
        let decided = reduce(&snapshot, &seal, &ResolvedConfigs::default(), &SpendWindow::default());
        let mut snapshot = snapshot.apply(&seal, &decided, &ResolvedConfigs::default());
        snapshot.blooms.get_mut(&bloom).expect("the seal recorded the bloom").status = BloomStatus::Resolved;

        (snapshot, bloom)
    }

    fn study_order(decisions: &Decisions) -> &Transformation {
        decisions
            .effects
            .iter()
            .find_map(|effect| match effect {
                Decision::DispatchStudy { transformation, .. } => Some(transformation),
                _ => None,
            })
            .expect("a land dispatches the reader (ADR-0216)")
    }

    #[test]
    fn a_landing_dispatches_one_reader_over_the_range_it_landed() {
        // Tripwire: the reader's whole subject is the landed range. A dispatch
        // that checked out the base, or named a diff base other than the one
        // the bloom sealed against, would hand the model an empty diff and file
        // findings about nothing (#4723) — and an order carrying anything but
        // the receipt's own address would not bind the evidence the read
        // returns to the artifact the binding consumes.
        let base = digest(0);
        let (snapshot, bloom) = resolved(base);
        let landed = digest(40);

        let decisions = reduce(
            &snapshot,
            &Event { idempotency_key: IdempotencyKey("land".into()), fact: Fact::Land { bloom, new_head: landed } },
            &ResolvedConfigs::default(),
            &SpendWindow::default(),
        );

        let Outcome::Landed(receipt) = &decisions.outcome else {
            panic!("a land on the sealed base lands: {:?}", decisions.outcome);
        };
        let order = study_order(&decisions);
        assert_eq!(order.command, crate::RETROSPECT_READ_COMMAND);
        assert_eq!(order.inputs, [receipt.digest()], "the order pins the receipt the land produced");
        assert_eq!(order.checkout, landed, "the reader checks out the head mainline moved to");
        assert_eq!(order.diff_base, Some(base), "and reads it against the base the bloom sealed on");
        assert_eq!(
            decisions.effects.iter().filter(|effect| matches!(effect, Decision::DispatchStudy { .. })).count(),
            1,
            "one read per landing"
        );
    }

    #[test]
    fn a_faulted_read_files_its_evidence_and_wedges_nothing() {
        // Tripwire: the reader sits past every gate, so a failure here has
        // nothing left to refuse — but the arms it shares its shape with
        // (the aggregate gates) all wedge or re-dispatch on a bad verdict.
        // Reaching for one of those would hold a bloom open on a read whose
        // whole output is advisory, which ADR-0216 forbids in as many words.
        let base = digest(0);
        let (snapshot, bloom) = resolved(base);
        let land =
            Event { idempotency_key: IdempotencyKey("land".into()), fact: Fact::Land { bloom, new_head: digest(40) } };
        let decided = reduce(&snapshot, &land, &ResolvedConfigs::default(), &SpendWindow::default());
        let snapshot = snapshot.apply(&land, &decided, &ResolvedConfigs::default());

        let fault = Evidence { subject: digest(77), kind: EvidenceKind::ExecutorFault, detail: digest(78) };
        let decisions = reduce_study_completed(&snapshot, &bloom, false, &fault);

        assert!(matches!(decisions.outcome, Outcome::StudyRecorded { passed: false, .. }), "{decisions:?}");
        assert_eq!(decisions.effects.len(), 1, "a study result decides one row and nothing else: {decisions:?}");
        assert!(matches!(&decisions.effects[0], Decision::RecordEvidence { .. }));

        let after = snapshot.apply(
            &Event {
                idempotency_key: IdempotencyKey("study".into()),
                fact: Fact::StudyCompleted { bloom, passed: false, evidence: fault },
            },
            &decisions,
            &ResolvedConfigs::default(),
        );
        let record = after.blooms.get(&bloom).expect("the landed bloom is still recorded");
        assert_eq!(record.status, BloomStatus::Landed, "a faulted read leaves the bloom landed");
        assert!(record.wedged.is_empty(), "and wedges nothing: {:?}", record.wedged);
        assert!(record.holds.is_empty(), "and raises no pending-decision hold");
    }

    #[test]
    fn a_study_result_for_an_unlanded_bloom_is_refused() {
        // The reader is dispatched at the landing and only there, so a result
        // against a bloom still walking names an order this reducer never
        // decided. Filing it would put a retrospective verdict on the evidence
        // log of a bloom whose gates are still running.
        let (snapshot, bloom) = resolved(digest(0));
        let evidence = Evidence { subject: digest(77), kind: EvidenceKind::StudyRecord, detail: digest(78) };

        let decisions = reduce_study_completed(&snapshot, &bloom, true, &evidence);

        assert!(matches!(decisions.outcome, Outcome::StudyRejected(super::StudyError::NotLanded)), "{decisions:?}");
        assert!(decisions.effects.is_empty());
    }

    #[test]
    fn the_reader_is_the_blooms_own_calibration_not_the_compiled_one() {
        // Tripwire: `stage_binding` falls back to the compiled line when the
        // bloom sealed no catalog, and reaching for `StageCatalog::line()`
        // directly would run the compiled seat for a bloom that sealed a
        // different one — the #4324 divergence, at the reader's position. The
        // limit is the visible half of that binding.
        let base = digest(0);
        let (snapshot, bloom) = resolved(base);

        let decisions = reduce(
            &snapshot,
            &Event { idempotency_key: IdempotencyKey("land".into()), fact: Fact::Land { bloom, new_head: digest(40) } },
            &ResolvedConfigs::default(),
            &SpendWindow::default(),
        );

        let sealed = super::stage_binding(
            &snapshot.blooms.get(&bloom).expect("the seal recorded the bloom").stage_catalog,
            StageId::Study,
        );
        assert_eq!(study_order(&decisions).limits.wall_clock_secs, sealed.wall_clock_secs);
    }
}
