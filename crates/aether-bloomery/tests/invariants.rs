//! The named invariants of ADR-0149, under a mix of property tests (where a
//! rule must hold over a family of inputs) and specific-sequence tripwires
//! (where one triggering sequence is the whole point).
//!
//! Each case names one owned rule of the value vocabulary or the reducer — the
//! canonical seal order, the all-or-nothing admission set, the seal-time
//! member validation, the one-active-bloom rule, the compare-and-swap land
//! against the *sealed* base, the second-door supersession scan, the
//! latest-wins claim map, and the scope-bound claim inheritance — and fails
//! exactly when that rule regresses.

#![allow(clippy::unwrap_used)]

mod common;

use aether_bloomery::{
    AdmitEvidenceError, Artifact, BloomStatus, Digest, Evidence, EvidenceKind, Fact, LandError, Outcome, ResolveError,
    SealError, Snapshot, SupersedeError, reduce,
};
use aether_data::wire::to_vec;
use common::{claim, digest, draft, event, membership, sealed_and_resolved, splice_bloom, step, workpiece};
use proptest::collection::btree_set;
use proptest::prelude::*;
use std::collections::BTreeSet;

/// A set of distinct memberships named by their (distinct) revision seeds, so
/// order-sensitivity is actually exercised.
fn distinct_members() -> impl Strategy<Value = Vec<aether_bloomery::Membership>> {
    btree_set(1u8..60u8, 1..6).prop_map(|revisions: BTreeSet<u8>| {
        revisions.into_iter().map(|rev| membership(&format!("wp-{rev}"), rev)).collect()
    })
}

proptest! {
    // Invariant 1 — a sealed spec never mutates: sealing canonicalizes member
    // order, so the same set in any order seals to byte-identical specs and the
    // same id. Tripwire: dropping the sort in `seal` breaks this.
    #[test]
    fn seal_is_canonical_over_member_order(members in distinct_members()) {
        let forward = draft(1, members.clone());
        let mut reversed_members = members;
        reversed_members.reverse();
        let reversed = draft(1, reversed_members);

        prop_assert_eq!(forward.seal(), reversed.seal());
        prop_assert_eq!(forward.seal().id(), reversed.seal().id());
    }

    // Invariant 1 (tie-break) — members sharing one scope_revision still seal
    // canonically: the sort key falls through past the revision to the workpiece
    // (then the approval evidence), so the same same-revision set in any order
    // seals byte-identically. Tripwire: sorting on the revision alone (the
    // pre-total-order key) left same-revision members order-undetermined, so a
    // stable sort leaked their input position into the id.
    #[test]
    fn seal_is_canonical_over_shared_revision_members(names in btree_set("wp-[a-z]{1,5}", 2..6)) {
        let names: Vec<String> = names.into_iter().collect();
        let forward: Vec<_> = names.iter().map(|name| membership(name, 7)).collect();
        let mut reversed = forward.clone();
        reversed.reverse();

        prop_assert_eq!(draft(1, forward.clone()).seal(), draft(1, reversed.clone()).seal());
        prop_assert_eq!(draft(1, forward).seal().id(), draft(1, reversed).seal().id());
    }

    // Invariant 4 — no evidence validates a digest it does not name.
    // Tripwire: `Evidence::validates` returning anything but exact-match.
    #[test]
    fn evidence_binds_exactly_one_digest(subject in 0u8..=255u8, other in 0u8..=255u8) {
        let evidence = Evidence { subject: digest(subject), kind: EvidenceKind::Approval, detail: digest(0) };
        prop_assert!(evidence.validates(&digest(subject)));
        prop_assert_eq!(evidence.validates(&digest(other)), subject == other);
    }

    // Invariant 5 — a resolved bloom carries a resolution claim for every
    // frozen member, and cannot resolve while a member is unintegrated.
    #[test]
    fn resolve_covers_every_member(members in distinct_members()) {
        let (snapshot, spec) = sealed_and_resolved(1, members.clone(), 40);
        let bloom = spec.id();
        let record = snapshot.blooms.get(&bloom).unwrap();
        prop_assert_eq!(record.status, BloomStatus::Resolved);

        // A resolve after seal-only (before any integrate) must be refused. The
        // seal-only bloom seals on its own fresh mainline, so the V1 rule does
        // not block it.
        let sealed_only = draft(2, members).seal();
        let bloom2 = sealed_only.id();
        let base = Snapshot::new(digest(2));
        let (after_seal, sealed) = step(&base, &event("s", Fact::Seal(sealed_only)));
        prop_assert!(matches!(sealed.outcome, Outcome::Sealed(_)));
        let early = reduce(&after_seal, &event("r", Fact::Resolve { bloom: bloom2, tree: digest(40), lineage: vec![] }));
        let member_not_integrated =
            matches!(early.outcome, Outcome::ResolveRejected(ResolveError::MemberNotIntegrated { .. }));
        prop_assert!(member_not_integrated);
    }

    // Invariant 6 (C1) — landing is a compare-and-swap against the bloom's own
    // *sealed* base, not a caller-supplied one. A mainline moved off that base
    // is refused with a BaseMismatch naming `spec.base()`, and only a mainline
    // still at the sealed base lands. Tripwire: comparing against anything but
    // `record.spec.base()`.
    #[test]
    fn landing_cas_is_against_the_sealed_base(members in distinct_members(), moved in 2u8..=255u8) {
        prop_assume!(moved != 1);
        let (mut snapshot, spec) = sealed_and_resolved(1, members, 40);
        let bloom = spec.id();

        // Move mainline off the sealed base (as if another bloom had landed).
        snapshot.mainline = digest(moved);
        let stale = reduce(&snapshot, &event("stale", Fact::Land { bloom, new_head: digest(50) }));
        match stale.outcome {
            Outcome::LandRejected(LandError::BaseMismatch(mismatch)) => {
                prop_assert_eq!(mismatch.expected, spec.base());
                prop_assert_eq!(mismatch.actual, digest(moved));
            }
            other => prop_assert!(false, "expected BaseMismatch against the sealed base, got {other:?}"),
        }

        // Back at the sealed base, the same land succeeds and advances mainline.
        snapshot.mainline = spec.base();
        let (landed, good) = step(&snapshot, &event("good", Fact::Land { bloom, new_head: digest(50) }));
        prop_assert!(matches!(good.outcome, Outcome::Landed(_)));
        prop_assert_eq!(landed.mainline, digest(50));
    }
}

// Invariant 1b (m3) — sealing is canonical over the *full* member content, not
// just `(scope_revision, workpiece)`: members sharing a revision, or even a
// revision and a workpiece, still order deterministically, so the id never
// depends on input order. Tripwire: a partial sort key leaves a shared-revision
// or shared-workpiece pair order-sensitive.
#[test]
fn seal_is_canonical_over_full_member_content() {
    // Two members sharing a scope revision, differing only in workpiece.
    let a = membership("wp-a", 10);
    let b = membership("wp-b", 10);
    assert_eq!(draft(1, vec![a.clone(), b.clone()]).seal().id(), draft(1, vec![b, a.clone()]).seal().id());

    // Two members sharing (scope_revision, workpiece), differing only in the
    // approval evidence — a degenerate set the reducer rejects at admission, but
    // seal() must still order it deterministically so its id is input-order
    // independent.
    let mut c = membership("wp-a", 10);
    c.approval = Evidence { subject: digest(10), kind: EvidenceKind::Approval, detail: digest(77) };
    assert_eq!(draft(1, vec![a.clone(), c.clone()]).seal().id(), draft(1, vec![c, a]).seal().id());
}

// Invariant 2 (M4) — the V1 one-sealed-unlanded-bloom-per-mainline rule: a
// second seal is refused while any bloom is still Sealed or Resolved, even when
// its membership is disjoint (so this is the V1 rule, not a membership
// conflict). Tripwire: sealing a second concurrent bloom succeeds.
#[test]
fn second_concurrent_seal_is_refused() {
    let base = Snapshot::new(digest(1));
    let first = draft(1, vec![membership("alpha", 10)]).seal();
    let (after_first, sealed) = step(&base, &event("first", Fact::Seal(first)));
    assert!(matches!(sealed.outcome, Outcome::Sealed(_)));

    // Disjoint membership — no workpiece overlap, so a conflict cannot fire.
    let second = draft(1, vec![membership("beta", 11)]).seal();
    let rejected = reduce(&after_first, &event("second", Fact::Seal(second)));
    assert!(matches!(rejected.outcome, Outcome::SealRejected(SealError::ActiveBloomExists(_))));
    assert!(rejected.effects.is_empty());
}

// Invariant 3 — a member already held by a foreign active bloom aborts the whole
// seal, and the free members in the batch stay unclaimed. Built from a spliced
// snapshot so the conflicting hold is a foreign bloom's, isolating the
// membership-conflict guard from the V1 rule.
#[test]
fn foreign_hold_aborts_the_whole_seal() {
    let mut snapshot = Snapshot::new(digest(1));
    let held = draft(9, vec![membership("shared", 10)]).seal();
    splice_bloom(&mut snapshot, &held, BloomStatus::Landed);

    let batch = draft(2, vec![membership("shared", 11), membership("free-a", 20), membership("free-b", 21)]).seal();
    let (after_batch, decided) = step(&snapshot, &event("batch", Fact::Seal(batch)));
    match decided.outcome {
        Outcome::SealRejected(SealError::MembershipConflict(conflict)) => {
            assert_eq!(conflict.workpiece, workpiece("shared"));
        }
        other => panic!("expected a membership conflict, got {other:?}"),
    }
    assert!(!after_batch.active.contains_key(&workpiece("free-a")));
    assert!(!after_batch.active.contains_key(&workpiece("free-b")));
}

// M2 — seal validates its membership set: empty, duplicate-workpiece,
// wrong-evidence-kind, and unbound-approval seals are all seal-time rejections.
#[test]
fn seal_rejects_empty_membership() {
    let base = Snapshot::new(digest(1));
    let empty = draft(1, vec![]).seal();
    let decided = reduce(&base, &event("empty", Fact::Seal(empty)));
    assert!(matches!(decided.outcome, Outcome::SealRejected(SealError::EmptyMembership)));
}

#[test]
fn seal_rejects_duplicate_workpiece() {
    let base = Snapshot::new(digest(1));
    // Same workpiece at two distinct revisions — not an exact duplicate, so it
    // survives seal's dedup and reaches the reducer's duplicate check.
    let dup = draft(1, vec![membership("wp", 10), membership("wp", 11)]).seal();
    let decided = reduce(&base, &event("dup", Fact::Seal(dup)));
    match decided.outcome {
        Outcome::SealRejected(SealError::DuplicateWorkpiece(wp)) => assert_eq!(wp, workpiece("wp")),
        other => panic!("expected DuplicateWorkpiece, got {other:?}"),
    }
}

#[test]
fn seal_rejects_unbound_or_wrong_kind_approval() {
    let base = Snapshot::new(digest(1));

    // Approval whose subject is not the member's scope revision.
    let mut wrong_subject = membership("wp", 10);
    wrong_subject.approval = Evidence { subject: digest(99), kind: EvidenceKind::Approval, detail: digest(0) };
    let decided = reduce(&base, &event("s1", Fact::Seal(draft(1, vec![wrong_subject]).seal())));
    assert!(matches!(decided.outcome, Outcome::SealRejected(SealError::UnapprovedMember(_))));

    // Right subject, but the evidence is not an Approval.
    let mut wrong_kind = membership("wp", 10);
    wrong_kind.approval = Evidence { subject: digest(10), kind: EvidenceKind::VerificationResult, detail: digest(0) };
    let decided = reduce(&base, &event("s2", Fact::Seal(draft(1, vec![wrong_kind]).seal())));
    assert!(matches!(decided.outcome, Outcome::SealRejected(SealError::UnapprovedMember(_))));
}

// C4 — re-sealing a known BloomId is refused rather than resurrecting and
// overwriting the existing record.
#[test]
fn seal_rejects_a_known_bloom_id() {
    let base = Snapshot::new(digest(1));
    let spec = draft(1, vec![membership("wp", 10)]).seal();
    let bloom = spec.id();
    let (after_seal, sealed) = step(&base, &event("seal", Fact::Seal(spec.clone())));
    assert!(matches!(sealed.outcome, Outcome::Sealed(_)));

    let again = reduce(&after_seal, &event("again", Fact::Seal(spec)));
    match again.outcome {
        Outcome::SealRejected(SealError::KnownBloom(id)) => assert_eq!(id, bloom),
        other => panic!("expected KnownBloom, got {other:?}"),
    }
}

// Seal catalog admission — the frozen stage_catalog must be the line the
// pipeline runs (ADR-0149 §The line): a spec promising a foreign catalog is
// inadmissible, since a bloom is graded against the catalog it promised. The
// default `draft` stamps the line digest, so every other seal test covers the
// positive case; this pins the rejection and names the found digest.
#[test]
fn seal_rejects_an_unknown_stage_catalog() {
    let base = Snapshot::new(digest(1));
    let mut foreign = draft(1, vec![membership("wp", 10)]);
    foreign.stage_catalog = digest(99);
    let decided = reduce(&base, &event("foreign", Fact::Seal(foreign.seal())));
    match decided.outcome {
        Outcome::SealRejected(SealError::UnknownStageCatalog { found }) => assert_eq!(found, digest(99)),
        other => panic!("expected UnknownStageCatalog, got {other:?}"),
    }
}

// The zero-default catalog is the specific case the invariant closes: before
// this slice a draft could seal against `Digest::default()` and be graded
// against no catalog at all.
#[test]
fn seal_rejects_the_default_stage_catalog() {
    let base = Snapshot::new(digest(1));
    let mut zero = draft(1, vec![membership("wp", 10)]);
    zero.stage_catalog = Digest::default();
    let decided = reduce(&base, &event("zero", Fact::Seal(zero.seal())));
    assert!(matches!(decided.outcome, Outcome::SealRejected(SealError::UnknownStageCatalog { .. })));
}

// C3 — a Resolved bloom is supersedable: the ADR's primary supersession trigger
// is a failed land, which happens at Resolved, so this must not wedge.
#[test]
fn resolved_bloom_is_supersedable() {
    let (snapshot, predecessor_spec) = sealed_and_resolved(1, vec![membership("wp", 10)], 40);
    let predecessor = predecessor_spec.id();
    assert_eq!(snapshot.blooms.get(&predecessor).unwrap().status, BloomStatus::Resolved);

    // A successor differing in base — a distinct id, same member (exempt from
    // conflict via the predecessor's release).
    let successor_spec = draft(2, vec![membership("wp", 10)]).seal();
    let successor = successor_spec.id();
    let (after, decided) = step(&snapshot, &event("sup", Fact::Supersede { predecessor, successor: successor_spec }));
    assert!(matches!(decided.outcome, Outcome::Superseded { .. }));
    assert_eq!(after.blooms.get(&predecessor).unwrap().status, BloomStatus::Superseded);
    assert_eq!(after.blooms.get(&successor).unwrap().status, BloomStatus::Sealed);
}

// C4 — a bloom cannot supersede itself into a bloom superseded by itself: an
// identical successor spec (same id) is refused.
#[test]
fn self_supersession_is_refused() {
    let base = Snapshot::new(digest(1));
    let spec = draft(1, vec![membership("wp", 10)]).seal();
    let predecessor = spec.id();
    let (after_seal, _) = step(&base, &event("seal", Fact::Seal(spec.clone())));

    let decided = reduce(&after_seal, &event("self", Fact::Supersede { predecessor, successor: spec }));
    assert!(matches!(decided.outcome, Outcome::SupersedeRejected(SupersedeError::SelfSupersession)));
}

// C2 — supersede runs the same all-or-nothing active scan as seal: a successor
// member held by an *unrelated* active bloom aborts the supersession rather than
// silently double-claiming it. Built from a spliced snapshot with two active
// blooms, the state the guard defends against.
#[test]
fn supersede_rejects_a_foreign_double_claim() {
    let mut snapshot = Snapshot::new(digest(1));
    // A foreign active bloom holding "shared".
    let foreign = draft(7, vec![membership("shared", 30)]).seal();
    splice_bloom(&mut snapshot, &foreign, BloomStatus::Sealed);
    // The predecessor to supersede, holding its own disjoint member.
    let predecessor_spec = draft(1, vec![membership("own", 10)]).seal();
    let predecessor = predecessor_spec.id();
    splice_bloom(&mut snapshot, &predecessor_spec, BloomStatus::Sealed);

    // The successor tries to claim "shared", held by the foreign bloom.
    let successor_spec = draft(1, vec![membership("own", 10), membership("shared", 31)]).seal();
    let decided = reduce(&snapshot, &event("sup", Fact::Supersede { predecessor, successor: successor_spec }));
    match decided.outcome {
        Outcome::SupersedeRejected(SupersedeError::MembershipConflict(conflict)) => {
            assert_eq!(conflict.workpiece, workpiece("shared"));
            assert_eq!(conflict.held_by, foreign.id());
        }
        other => panic!("expected a foreign membership conflict, got {other:?}"),
    }
}

// M3 (supersede admission) — a superseding spec runs the same per-member
// admission a seal does: an empty, duplicate-workpiece, or unapproved successor
// is refused before it claims or inherits anything. Tripwire on reduce_supersede
// skipping the member-validity checks reduce_seal runs.
#[test]
fn supersede_rejects_an_invalid_successor_membership() {
    let mut snapshot = Snapshot::new(digest(1));
    let predecessor_spec = draft(1, vec![membership("own", 10)]).seal();
    let predecessor = predecessor_spec.id();
    splice_bloom(&mut snapshot, &predecessor_spec, BloomStatus::Sealed);

    // A successor repeating one workpiece — invalid the same way a seal's is.
    let dup = draft(2, vec![membership("dup", 20), membership("dup", 21)]).seal();
    let decided = reduce(&snapshot, &event("dup", Fact::Supersede { predecessor, successor: dup }));
    assert_eq!(
        decided.outcome,
        Outcome::SupersedeRejected(SupersedeError::InvalidMember(SealError::DuplicateWorkpiece(workpiece("dup")))),
    );
    assert!(decided.effects.is_empty());

    // An empty successor is refused the same way.
    let empty = draft(2, vec![]).seal();
    let decided = reduce(&snapshot, &event("empty", Fact::Supersede { predecessor, successor: empty }));
    assert_eq!(decided.outcome, Outcome::SupersedeRejected(SupersedeError::InvalidMember(SealError::EmptyMembership)));
    assert!(decided.effects.is_empty());
}

// Supersede catalog admission — the successor is held to seal's catalog
// admission too (ADR-0149 §The line): a superseding spec promising a foreign
// catalog is refused, wrapped as InvalidMember alongside the other seal-validity
// failures a successor is held to.
#[test]
fn supersede_rejects_an_unknown_stage_catalog() {
    let mut snapshot = Snapshot::new(digest(1));
    let predecessor_spec = draft(1, vec![membership("own", 10)]).seal();
    let predecessor = predecessor_spec.id();
    splice_bloom(&mut snapshot, &predecessor_spec, BloomStatus::Sealed);

    // A valid membership (so member admission passes) but a foreign catalog.
    let mut foreign = draft(2, vec![membership("own", 10)]);
    foreign.stage_catalog = digest(99);
    let decided = reduce(&snapshot, &event("foreign", Fact::Supersede { predecessor, successor: foreign.seal() }));
    match decided.outcome {
        Outcome::SupersedeRejected(SupersedeError::InvalidMember(SealError::UnknownStageCatalog { found })) => {
            assert_eq!(found, digest(99));
        }
        other => panic!("expected InvalidMember(UnknownStageCatalog), got {other:?}"),
    }
}

// Invariant 7 (M3) — a successor atomically inherits its predecessor's claims,
// but only those whose workpiece it re-admits at the same scope revision: an
// ejected workpiece and a scope-changed one both drop their stale claims.
#[test]
fn successor_inherits_only_still_valid_claims() {
    let base = Snapshot::new(digest(1));
    // Predecessor admits kept (rev 10), ejected (rev 11), and changed (rev 12).
    let members = vec![membership("kept", 10), membership("ejected", 11), membership("changed", 12)];
    let predecessor_spec = draft(1, members).seal();
    let predecessor = predecessor_spec.id();
    let (mut snapshot, _) = step(&base, &event("seal", Fact::Seal(predecessor_spec)));

    // Integrate all three, each claim carrying the revision it was integrated at.
    for (name, rev, cand) in [("kept", 10u8, 100u8), ("ejected", 11, 101), ("changed", 12, 102)] {
        let ev = event(&format!("i-{name}"), Fact::Integrate { bloom: predecessor, claim: claim(name, rev, cand) });
        let (next, decided) = step(&snapshot, &ev);
        assert!(matches!(decided.outcome, Outcome::Integrated { .. }));
        snapshot = next;
    }

    // Successor keeps "kept" at rev 10, drops "ejected", and re-admits "changed"
    // at a *new* revision 13 — so only "kept"'s claim is still valid to inherit.
    let successor_spec = draft(2, vec![membership("kept", 10), membership("changed", 13)]).seal();
    let successor = successor_spec.id();
    let (after, decided) = step(&snapshot, &event("sup", Fact::Supersede { predecessor, successor: successor_spec }));
    assert!(matches!(decided.outcome, Outcome::Superseded { .. }));

    let claims = &after.blooms.get(&successor).unwrap().claims;
    assert!(claims.contains_key(&workpiece("kept")), "the kept member's claim inherits");
    assert!(!claims.contains_key(&workpiece("ejected")), "the ejected member's claim is dropped");
    assert!(!claims.contains_key(&workpiece("changed")), "the scope-changed member's stale claim is dropped");
    assert_eq!(claims.len(), 1);
}

// M1 — claims are keyed by workpiece, latest-wins: a refined re-integration of a
// member overwrites its stale predecessor claim, so resolve reads the refined
// candidate, not the first one integrated.
#[test]
fn re_integration_overwrites_the_stale_claim() {
    let base = Snapshot::new(digest(1));
    let spec = draft(1, vec![membership("wp", 10)]).seal();
    let bloom = spec.id();
    let (mut snapshot, _) = step(&base, &event("seal", Fact::Seal(spec)));

    // Integrate a stale candidate, then re-integrate a refined one.
    let (next, _) = step(&snapshot, &event("i1", Fact::Integrate { bloom, claim: claim("wp", 10, 100) }));
    snapshot = next;
    let (next, _) = step(&snapshot, &event("i2", Fact::Integrate { bloom, claim: claim("wp", 10, 200) }));
    snapshot = next;

    // Exactly one claim survives, and it is the refined candidate.
    let record = snapshot.blooms.get(&bloom).unwrap();
    assert_eq!(record.claims.len(), 1);
    assert_eq!(record.claims.get(&workpiece("wp")).unwrap().candidate, digest(200));

    // Resolve reads the refined claim.
    let resolved = reduce(&snapshot, &event("r", Fact::Resolve { bloom, tree: digest(40), lineage: vec![] }));
    match resolved.outcome {
        Outcome::Resolved(bloom) => {
            assert_eq!(bloom.resolution_claims.len(), 1);
            assert_eq!(bloom.resolution_claims[0].candidate, digest(200));
        }
        other => panic!("expected Resolved, got {other:?}"),
    }
}

// ADR-0151 — a study-record evidence admitted against a sealed bloom is recorded
// in the per-bloom evidence log, in admission order, and the reducer reports the
// bloom + subject it recorded against. A freshly sealed bloom starts with an
// empty log. Tripwire: the `AdmitEvidence` arm or the `RecordEvidence` apply
// effect dropping the append.
#[test]
fn admit_evidence_records_it_in_the_bloom_log() {
    let base = Snapshot::new(digest(1));
    let spec = draft(1, vec![membership("wp", 10)]).seal();
    let bloom = spec.id();
    let (snapshot, _) = step(&base, &event("seal", Fact::Seal(spec)));
    assert!(snapshot.blooms.get(&bloom).unwrap().evidence.is_empty(), "a freshly sealed bloom logs no evidence");

    let study = Evidence { subject: digest(70), kind: EvidenceKind::StudyRecord, detail: digest(80) };
    let (after, decided) = step(&snapshot, &event("admit", Fact::AdmitEvidence { bloom, evidence: study.clone() }));
    match decided.outcome {
        Outcome::EvidenceAdmitted { bloom: b, subject } => {
            assert_eq!(b, bloom);
            assert_eq!(subject, digest(70));
        }
        other => panic!("expected EvidenceAdmitted, got {other:?}"),
    }
    assert_eq!(after.blooms.get(&bloom).unwrap().evidence, vec![study]);
}

// ADR-0151 — admission binds to the evidence-log door: an unknown or inactive
// bloom is `UnknownOrInactiveBloom`, and an integrating class (a `ResolutionClaim`
// integrates, an `Approval` seals a member) is `EvidenceNotBound` — a resolution
// claim never enters through `AdmitEvidence`.
#[test]
fn admit_evidence_refuses_unknown_bloom_and_the_wrong_door() {
    let base = Snapshot::new(digest(1));
    let spec = draft(1, vec![membership("wp", 10)]).seal();
    let bloom = spec.id();

    // Unknown bloom — nothing sealed yet.
    let study = Evidence { subject: digest(70), kind: EvidenceKind::StudyRecord, detail: digest(80) };
    let unknown = reduce(&base, &event("u", Fact::AdmitEvidence { bloom, evidence: study }));
    assert!(matches!(unknown.outcome, Outcome::AdmitEvidenceRejected(AdmitEvidenceError::UnknownOrInactiveBloom)));
    assert!(unknown.effects.is_empty());

    let (snapshot, _) = step(&base, &event("seal", Fact::Seal(spec)));

    // A resolution claim is bound to the integrate door, not the evidence log.
    let claim_ev = Evidence { subject: digest(70), kind: EvidenceKind::ResolutionClaim, detail: digest(80) };
    let mis_routed = reduce(&snapshot, &event("c", Fact::AdmitEvidence { bloom, evidence: claim_ev }));
    assert!(matches!(mis_routed.outcome, Outcome::AdmitEvidenceRejected(AdmitEvidenceError::EvidenceNotBound)));

    // An approval seals a member; it is not free-log evidence either.
    let approval = Evidence { subject: digest(70), kind: EvidenceKind::Approval, detail: digest(80) };
    let also_mis = reduce(&snapshot, &event("a", Fact::AdmitEvidence { bloom, evidence: approval }));
    assert!(matches!(also_mis.outcome, Outcome::AdmitEvidenceRejected(AdmitEvidenceError::EvidenceNotBound)));
}

// ADR-0151 — a replayed admission is a no-op: the existing `seen`-set guard
// covers `AdmitEvidence` for free, so a resent idempotency key reduces to
// `Duplicate` and the log does not grow a second copy.
#[test]
fn a_replayed_admission_is_a_duplicate() {
    let base = Snapshot::new(digest(1));
    let spec = draft(1, vec![membership("wp", 10)]).seal();
    let bloom = spec.id();
    let (snapshot, _) = step(&base, &event("seal", Fact::Seal(spec)));

    let study = Evidence { subject: digest(70), kind: EvidenceKind::StudyRecord, detail: digest(80) };
    let admit = event("admit", Fact::AdmitEvidence { bloom, evidence: study });
    let (once, _) = step(&snapshot, &admit);
    assert_eq!(once.blooms.get(&bloom).unwrap().evidence.len(), 1);

    let (twice, replay) = step(&once, &admit);
    assert!(matches!(replay.outcome, Outcome::Duplicate));
    assert_eq!(twice.blooms.get(&bloom).unwrap().evidence.len(), 1, "a replayed key does not re-append");
}

// m4 — an unknown bloom and a not-yet-resolved bloom each land-refuse with their
// own reason, never a misreported BaseMismatch.
#[test]
fn land_refusals_name_their_own_reason() {
    let base = Snapshot::new(digest(1));
    let spec = draft(1, vec![membership("wp", 10)]).seal();
    let bloom = spec.id();

    // Unknown bloom.
    let unknown = reduce(&base, &event("u", Fact::Land { bloom, new_head: digest(40) }));
    assert!(matches!(unknown.outcome, Outcome::LandRejected(LandError::UnknownBloom(_))));

    // Sealed but not resolved.
    let (after_seal, _) = step(&base, &event("seal", Fact::Seal(spec)));
    let not_resolved = reduce(&after_seal, &event("nr", Fact::Land { bloom, new_head: digest(40) }));
    assert!(matches!(not_resolved.outcome, Outcome::LandRejected(LandError::NotResolved(_))));
}

// m5 — a land releases the bloom's memberships from `active`, so the workpieces
// are free for the next bloom to seal.
#[test]
fn landing_releases_memberships() {
    let (snapshot, spec) = sealed_and_resolved(1, vec![membership("wp", 10)], 40);
    let bloom = spec.id();
    assert_eq!(snapshot.active.get(&workpiece("wp")), Some(&bloom));

    let (after, decided) = step(&snapshot, &event("land", Fact::Land { bloom, new_head: digest(50) }));
    assert!(matches!(decided.outcome, Outcome::Landed(_)));
    assert!(!after.active.contains_key(&workpiece("wp")), "landing frees the workpiece");
}

proptest! {
    // Invariant 8 — typed content addressing (ADR-0149 §The value vocabulary).
    // A content-addressed value's digest hashes its type's DOMAIN tag ahead of
    // the wire bytes, so it is never the bare sha256 of those bytes — the
    // untagged scheme under which structurally-identical values of different
    // types collide. Tripwire: dropping the domain-tag hashing in `digest_of`.
    #[test]
    fn digest_incorporates_the_domain_tag(bytes in prop::collection::vec(any::<u8>(), 0..64)) {
        let artifact = Artifact { media_type: String::new(), bytes, parents: vec![digest(1)] };
        let untagged = Digest::of_wire_bytes(&to_vec(&artifact).unwrap());
        prop_assert_ne!(artifact.id(), untagged);
    }
}
