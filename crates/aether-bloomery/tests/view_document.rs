//! Property tests for the projection assembler `view_of` (ADR-0149 §The
//! boundary, as amended by #3471). The assembler is the pure
//! `Snapshot -> ViewDocument` the reconcile port pushes outward; these are
//! tripwires on the membership-fidelity rules it owns — a sealed bloom's
//! document names every member exactly once, and a resolved bloom's document
//! carries a resolution claim for every member.

#![allow(clippy::unwrap_used)]

mod common;

use aether_bloomery::{
    Evidence, EvidenceKind, Fact, Question, ResolvedConfigs, Snapshot, SpendQuiesce, SpendWindow, StageId,
    VerifyFailure, VerifyFailureSet, WorkpieceId, reduce, view_of,
};
use common::{digest, draft, event, membership, observing, sealed_and_resolved};
use proptest::collection::btree_set;
use proptest::prelude::*;
use std::collections::{BTreeMap, BTreeSet};

/// A set of distinct memberships named by their (distinct) revision seeds, so
/// the canonical-order and one-entry-per-member rules are actually exercised.
fn distinct_members() -> impl Strategy<Value = Vec<aether_bloomery::Membership>> {
    btree_set(1u8..60u8, 1..6).prop_map(|revisions: BTreeSet<u8>| {
        revisions.into_iter().map(|rev| membership(&format!("wp-{rev}"), rev)).collect()
    })
}

/// Seal `members` into a bloom on a fresh mainline and return the evolved
/// snapshot — the sealed-but-unresolved setup the coverage property needs.
fn sealed(members: Vec<aether_bloomery::Membership>) -> Snapshot {
    let spec = draft(0, members).seal();
    let snapshot = Snapshot::new(digest(0));
    let seal = event("seal", Fact::Seal(spec));
    snapshot.apply(
        &seal,
        &reduce(&snapshot, &seal, &ResolvedConfigs::default(), &SpendWindow::default()),
        &ResolvedConfigs::default(),
    )
}

#[test]
fn view_carries_the_spend_quiesce_marker() {
    // The plausible bug: view_of drops the snapshot marker, so GET /view
    // cannot show "quiesced: spend ceiling" and the door looks idle.
    let mut snapshot = Snapshot::new(digest(0));
    let marker =
        SpendQuiesce::Window { window: "bloomery/daily/2026-08-14".into(), spent_micro_usd: 12, ceiling_micro_usd: 10 };
    snapshot.spend_quiesce = Some(marker.clone());
    let view = view_of(&snapshot, |_| None);
    assert_eq!(view.spend_quiesce.as_ref(), Some(&marker));

    let open = view_of(&Snapshot::new(digest(0)), |_| None);
    assert_eq!(open.spend_quiesce, None, "an open door carries no marker");
}

#[test]
fn view_carries_no_base_alert_when_the_receipt_is_absent_or_green() {
    let view = view_of(&Snapshot::new(digest(0)), |_| None);
    assert_eq!(view.base_alert, None);

    let view = view_of(&Snapshot::new(digest(0)).with_green_base(digest(0)), |_| None);
    assert_eq!(view.base_alert, None, "a green receipt is not a day-level stop");
}

#[test]
fn view_carries_a_base_alert_when_the_receipt_is_red() {
    use aether_bloomery::{BaseReceipt, BaseVerdict, Decision, Decisions, EvidenceKind, VerifyFailure, VerifyGateSet};

    let mut snapshot = Snapshot::new(digest(0));
    let evidence = Evidence { subject: digest(0), kind: EvidenceKind::VerificationResult, detail: digest(9) };
    let receipt = BaseReceipt {
        base: digest(0),
        tree: digest(0),
        gate_set: VerifyGateSet::base().digest(),
        verdict: BaseVerdict::Red { evidence, failed: VerifyFailureSet::one(VerifyFailure::Docs) },
    };
    let decided = Decisions {
        outcome: aether_bloomery::Outcome::BaseRefused {
            base: digest(0),
            tree: digest(0),
            failed: VerifyFailureSet::one(VerifyFailure::Docs),
        },
        effects: vec![Decision::RecordBaseReceipt { receipt }],
    };
    snapshot = snapshot.apply(
        &event(
            "base-red",
            Fact::BaseVerifyCompleted {
                base: digest(0),
                tree: digest(0),
                passed: false,
                evidence: Evidence { subject: digest(0), kind: EvidenceKind::VerificationResult, detail: digest(9) },
                failed: VerifyFailureSet::one(VerifyFailure::Docs),
            },
        ),
        &decided,
        &ResolvedConfigs::default(),
    );
    let view = view_of(&snapshot, |_| None);
    let alert = view.base_alert.expect("a red receipt populates the alert");
    assert!(alert.failed.iter().any(|name| name.contains("docs")), "got {:?}", alert.failed);
}

#[test]
fn view_carries_the_snapshots_observed_digest() {
    // A boot-fresh snapshot has never recorded an observation, so `observed`
    // is still Digest::default() — the same all-zero genesis sentinel
    // Snapshot::GENESIS_MAINLINE names. The field is a Digest, not an Option:
    // the projection copies that sentinel through rather than treating "no
    // observation yet" as absent, so GET /view always renders a hex digest
    // (never null) and never panics. An operator can name that genesis digest
    // as a successor base the same way they name mainline.
    let fresh = Snapshot::new(digest(1));
    let view = view_of(&fresh, |_| None);
    assert_eq!(view.mainline, digest(1));
    assert_eq!(view.observed, Snapshot::GENESIS_MAINLINE);

    // After a held observation, mainline and observed diverge; the document
    // carries the recorded head, not a second copy of mainline — the pair
    // reduce_supersede admits.
    let snapshot = observing(&sealed(vec![membership("wp", 1)]), 9);
    let view = view_of(&snapshot, |_| None);
    assert_eq!(view.mainline, digest(0));
    assert_eq!(view.observed, digest(9));
    assert_eq!(view.observed, snapshot.observed);
}

// A parked question on one member of a two-member bloom: `view_of` resolves the
// held question back to its bytes and surfaces the pending decision on that one
// member (matched by workpiece), leaving the sibling's `None`. An unresolvable
// hold surfaces nothing — the same graceful-degrade the study grade uses for a
// record whose bytes are unavailable.
#[test]
fn a_held_member_surfaces_its_pending_decision_only_when_resolvable() {
    let spec = draft(0, vec![membership("wp-held", 1), membership("wp-free", 2)]).seal();
    let bloom = spec.id();
    let mut snapshot = Snapshot::new(digest(0));
    let seal = event("seal", Fact::Seal(spec));
    snapshot = snapshot.apply(
        &seal,
        &reduce(&snapshot, &seal, &ResolvedConfigs::default(), &SpendWindow::default()),
        &ResolvedConfigs::default(),
    );

    let question = Question {
        stage: StageId::Construct,
        subject: digest(50),
        workpiece: WorkpieceId("wp-held".into()),
        prompt: "which approach?".into(),
        options: vec!["A".into(), "B".into()],
        blocked: "construct is held".into(),
    };
    let question_digest = question.id();
    let evidence = Evidence { subject: digest(50), kind: EvidenceKind::Question, detail: question_digest };
    let admit = event("park-1", Fact::AdmitEvidence { bloom, evidence });
    snapshot = snapshot.apply(
        &admit,
        &reduce(&snapshot, &admit, &ResolvedConfigs::default(), &SpendWindow::default()),
        &ResolvedConfigs::default(),
    );

    // With a resolver that returns the question bytes, the held member carries
    // its pending decision, its sibling does not.
    let resolver = |d: &_| (*d == question_digest).then(|| question.clone());
    let view = view_of(&snapshot, resolver);
    let members = &view.blooms[0].members;
    let held_view = members.iter().find(|m| m.workpiece.0 == "wp-held").unwrap();
    let free_view = members.iter().find(|m| m.workpiece.0 == "wp-free").unwrap();
    let pending = held_view.pending_decision.as_ref().expect("the held member surfaces its pending decision");
    assert_eq!(pending.question, question_digest, "the pending decision names the held question's exact digest");
    assert_eq!(pending.stage, StageId::Construct);
    assert_eq!(pending.prompt, "which approach?");
    assert!(free_view.pending_decision.is_none(), "the sibling member is not held");

    // With no resolver (the live-read path), the hold surfaces nothing.
    let unresolved = view_of(&snapshot, |_| None);
    assert!(
        unresolved.blooms[0].members.iter().all(|m| m.pending_decision.is_none()),
        "an unresolvable hold surfaces no pending decision, exactly as an unresolvable study record contributes nothing",
    );
}

#[test]
fn a_verify_wedge_projects_only_terminal_repeated_identities() {
    let spec = draft(0, vec![membership("wp", 1)]).seal();
    let bloom = spec.id();
    let mut snapshot = Snapshot::new(digest(0));
    let seal = event("seal", Fact::Seal(spec));
    snapshot = snapshot.apply(
        &seal,
        &reduce(&snapshot, &seal, &ResolvedConfigs::default(), &SpendWindow::default()),
        &ResolvedConfigs::default(),
    );

    let construct = event(
        "construct",
        Fact::AttemptCompleted {
            bloom,
            workpiece: WorkpieceId("wp".into()),
            stage: StageId::Construct,
            passed: true,
            evidence: Evidence { subject: digest(1), kind: EvidenceKind::VerificationResult, detail: digest(70) },
            candidate: None,
        },
    );
    snapshot = snapshot.apply(
        &construct,
        &reduce(&snapshot, &construct, &ResolvedConfigs::default(), &SpendWindow::default()),
        &ResolvedConfigs::default(),
    );

    // First clippy is novel. Three repeats spend the compiled Verify budget;
    // docs appears only in the terminal verdict and therefore is not responsible
    // for that roll.
    for index in 0u8..4 {
        let failed_verifiers = if index == 3 {
            [VerifyFailure::Clippy, VerifyFailure::Docs].into_iter().collect()
        } else {
            VerifyFailureSet::one(VerifyFailure::Clippy)
        };
        let failed = event(
            &format!("verify-{index}"),
            Fact::VerifyFailed {
                bloom,
                workpiece: WorkpieceId("wp".into()),
                evidence: Evidence {
                    subject: digest(1),
                    kind: EvidenceKind::VerificationResult,
                    detail: digest(80 + index),
                },
                failed_verifiers,
            },
        );
        snapshot = snapshot.apply(
            &failed,
            &reduce(&snapshot, &failed, &ResolvedConfigs::default(), &SpendWindow::default()),
            &ResolvedConfigs::default(),
        );
        if index < 3 {
            let refine = event(
                &format!("refine-{index}"),
                Fact::AttemptCompleted {
                    bloom,
                    workpiece: WorkpieceId("wp".into()),
                    stage: StageId::Refine,
                    passed: true,
                    evidence: Evidence {
                        subject: digest(1),
                        kind: EvidenceKind::VerificationResult,
                        detail: digest(90 + index),
                    },
                    candidate: None,
                },
            );
            snapshot = snapshot.apply(
                &refine,
                &reduce(&snapshot, &refine, &ResolvedConfigs::default(), &SpendWindow::default()),
                &ResolvedConfigs::default(),
            );
        }
    }

    let view = view_of(&snapshot, |_| None);
    let wedge = view.blooms[0].members[0].wedge.as_ref().expect("the repeated failures wedge the member");
    assert_eq!(wedge.evidence, digest(83));
    assert_eq!(wedge.repeated_verifiers, VerifyFailureSet::one(VerifyFailure::Clippy));
}

proptest! {
    // Property (a) — the document for a sealed bloom names every member
    // exactly once: no duplicate, no drop. The assembler must fold the spec's
    // canonical membership into `MemberView`s one-to-one.
    #[test]
    fn sealed_bloom_document_names_every_member_once(members in distinct_members()) {
        let snapshot = sealed(members.clone());
        let expected: Vec<WorkpieceId> = members.iter().map(|m| m.workpiece.clone()).collect();

        let view = view_of(&snapshot, |_| None);
        prop_assert_eq!(view.blooms.len(), 1);
        let bloom = &view.blooms[0];

        // Same count as the spec — no drop, no duplicate.
        prop_assert_eq!(bloom.members.len(), members.len());
        // Every member appears exactly once (count each workpiece).
        let mut counts: BTreeMap<WorkpieceId, usize> = BTreeMap::new();
        for member in &bloom.members {
            *counts.entry(member.workpiece.clone()).or_default() += 1;
        }
        for workpiece in &expected {
            prop_assert_eq!(counts.get(workpiece), Some(&1));
        }
        prop_assert_eq!(counts.len(), expected.len());
        // A merely-sealed member has no resolution claim yet.
        prop_assert!(bloom.members.iter().all(|m| m.resolution.is_none()));
    }

    // Property (b) — the document for a resolved bloom carries a resolution
    // claim for every member: `resolution` is `Some` for each, matched to the
    // member's own workpiece.
    #[test]
    fn resolved_bloom_document_carries_a_claim_per_member(members in distinct_members()) {
        let (snapshot, spec) = sealed_and_resolved(0, members, 7);

        let view = view_of(&snapshot, |_| None);
        prop_assert_eq!(view.blooms.len(), 1);
        let bloom = &view.blooms[0];

        prop_assert_eq!(bloom.members.len(), spec.members().len());
        for member in &bloom.members {
            let claim = member.resolution.as_ref();
            prop_assert!(claim.is_some());
            // The claim resolves this member's own workpiece.
            prop_assert_eq!(&claim.unwrap().workpiece, &member.workpiece);
        }
    }
}
