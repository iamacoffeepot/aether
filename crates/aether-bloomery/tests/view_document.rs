//! Property tests for the projection assembler `view_of` (ADR-0149 §The
//! boundary, as amended by #3471). The assembler is the pure
//! `Snapshot -> ViewDocument` the reconcile port pushes outward; these are
//! tripwires on the membership-fidelity rules it owns — a sealed bloom's
//! document names every member exactly once, and a resolved bloom's document
//! carries a resolution claim for every member.

#![allow(clippy::unwrap_used)]

mod common;

use aether_bloomery::{Evidence, EvidenceKind, Fact, Question, Snapshot, StageId, WorkpieceId, reduce, view_of};
use common::{digest, draft, event, membership, sealed_and_resolved};
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
    snapshot.apply(&seal, &reduce(&snapshot, &seal))
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
    snapshot = snapshot.apply(&seal, &reduce(&snapshot, &seal));

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
    snapshot = snapshot.apply(&admit, &reduce(&snapshot, &admit));

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
