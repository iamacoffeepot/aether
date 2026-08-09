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

use aether_bloomery::ConfigKind;
use aether_bloomery::Decisions;
use aether_bloomery::{
    AdmitEvidenceError, AdoptAnswerError, AggregateReviewError, AggregateVerifyError, Artifact, AttemptCompletedError,
    BloomId, BloomStatus, CandidateRef, CatalogError, Decision, Digest, Event, Evidence, EvidenceKind, Fact,
    GrantAttemptsError, KeyId, LandError, LandingRejectedError, Observation, Outcome, Provenance, Question,
    ResolveError, ResolvedConfigs, SealError, SignatureEnvelope, Snapshot, StageCatalog, StageId, StageProgress,
    Statement, SupersedeError, Unproducible, reduce,
};
use aether_data::Kind;
use aether_data::wire::to_vec;
use common::{
    claim, digest, draft, draft_with_catalog, event, membership, observing, sealed_and_resolved, splice_bloom, step,
    workpiece,
};
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
    // canonically: the sort key is the member's full content, so the same
    // same-revision set in any order seals byte-identically. Tripwire: sorting on
    // any single field leaves members agreeing on it order-undetermined, so a
    // stable sort leaks their input position into the id.
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
        let early = reduce(&after_seal, &event("r", Fact::Resolve { bloom: bloom2, tree: digest(40), head: digest(41), lineage: vec![] }), &ResolvedConfigs::default());
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
        let stale = reduce(&snapshot, &event("stale", Fact::Land { bloom, new_head: digest(50) }), &ResolvedConfigs::default());
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
    let rejected = reduce(&after_first, &event("second", Fact::Seal(second)), &ResolvedConfigs::default());
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
    let decided = reduce(&base, &event("empty", Fact::Seal(empty)), &ResolvedConfigs::default());
    assert!(matches!(decided.outcome, Outcome::SealRejected(SealError::EmptyMembership)));
}

#[test]
fn seal_rejects_duplicate_workpiece() {
    let base = Snapshot::new(digest(1));
    // Same workpiece at two distinct revisions — not an exact duplicate, so it
    // survives seal's dedup and reaches the reducer's duplicate check.
    let dup = draft(1, vec![membership("wp", 10), membership("wp", 11)]).seal();
    let decided = reduce(&base, &event("dup", Fact::Seal(dup)), &ResolvedConfigs::default());
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
    let decided =
        reduce(&base, &event("s1", Fact::Seal(draft(1, vec![wrong_subject]).seal())), &ResolvedConfigs::default());
    assert!(matches!(decided.outcome, Outcome::SealRejected(SealError::UnapprovedMember(_))));

    // Right subject, but the evidence is not an Approval.
    let mut wrong_kind = membership("wp", 10);
    wrong_kind.approval = Evidence { subject: digest(10), kind: EvidenceKind::VerificationResult, detail: digest(0) };
    let decided =
        reduce(&base, &event("s2", Fact::Seal(draft(1, vec![wrong_kind]).seal())), &ResolvedConfigs::default());
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

    let again = reduce(&after_seal, &event("again", Fact::Seal(spec)), &ResolvedConfigs::default());
    match again.outcome {
        Outcome::SealRejected(SealError::KnownBloom(id)) => assert_eq!(id, bloom),
        other => panic!("expected KnownBloom, got {other:?}"),
    }
}

// Seal catalog admission — an authored catalog is admitted on structural
// validity, not on matching the compiled line (ADR-0174): differing is the point.
// A stage left unbound is refused, because the member holding it would wedge with
// no attempt ever made, long after the operator who wrote the catalog moved on.
#[test]
fn seal_rejects_a_catalog_the_line_cannot_run() {
    let mut catalog = StageCatalog::line();
    catalog.bindings.retain(|binding| binding.stage != StageId::Verify);

    let (unrunnable, configs) = draft_with_catalog(1, vec![membership("wp", 10)], &catalog);
    let decided = reduce(&Snapshot::new(digest(1)), &event("unrunnable", Fact::Seal(unrunnable.seal())), &configs);
    match decided.outcome {
        Outcome::SealRejected(SealError::UnrunnableStageCatalog(error)) => {
            assert_eq!(error, CatalogError::UnboundStage(StageId::Verify));
        }
        other => panic!("expected UnrunnableStageCatalog, got {other:?}"),
    }
}

// Tripwire: sealing *no* catalog and sealing an *empty* one are different. Absent
// means "run the compiled line" and is the ordinary case every unconfigured bloom
// takes; empty binds nothing, so every member would wedge immediately. Collapsing
// the two either refuses every unconfigured bloom or admits a line that cannot
// run.
#[test]
fn an_absent_catalog_runs_the_line_but_an_empty_one_is_refused() {
    let unconfigured = draft(1, vec![membership("wp", 10)]);
    let admitted = reduce(
        &Snapshot::new(digest(1)),
        &event("absent", Fact::Seal(unconfigured.seal())),
        &ResolvedConfigs::default(),
    );
    assert!(matches!(admitted.outcome, Outcome::Sealed(_)), "sealing no catalog runs the line: {:?}", admitted.outcome);

    let (empty, configs) = draft_with_catalog(1, vec![membership("wp", 10)], &StageCatalog::default());
    let decided = reduce(&Snapshot::new(digest(1)), &event("empty", Fact::Seal(empty.seal())), &configs);
    assert!(
        matches!(decided.outcome, Outcome::SealRejected(SealError::UnrunnableStageCatalog(_))),
        "an empty catalog binds nothing and is refused: {:?}",
        decided.outcome,
    );
}

// Tripwire: content that is present and correctly filed but does not decode is a
// refusal, not a fall-through to the compiled line (ADR-0174). The name-keyed
// walk over the registry holds no Rust type, so it can only see that *something*
// is filed under the right kind; only a typed resolution reaches the decode. This
// is the shape of a configuration authored before a breaking change to its kind
// — the registry keys on the kind name precisely so it survives schema
// evolution, which is what leaves the decode as the sole place a stale value
// surfaces. Falling through here would seal a bloom running a line its receipt
// does not name.
#[test]
fn a_sealed_catalog_whose_bytes_do_not_decode_is_refused() {
    let catalog = StageCatalog::line();
    let (draft, _) = draft_with_catalog(1, vec![membership("wp", 10)], &catalog);

    // Correctly filed at the sealed address, so the name-keyed walk passes it —
    // the bytes are what will not produce a catalog.
    let mut configs = ResolvedConfigs::default();
    configs.insert(catalog.address(), StageCatalog::NAME, vec![0xff]);

    let decided = reduce(&Snapshot::new(digest(1)), &event("stale", Fact::Seal(draft.seal())), &configs);
    assert_eq!(
        decided.outcome,
        Outcome::SealRejected(SealError::UnproducibleConfig {
            kind: String::from(StageCatalog::NAME),
            address: catalog.address(),
            reason: Unproducible::Undecodable,
        }),
        "undecodable content refuses rather than running the compiled line"
    );
    assert!(decided.effects.is_empty(), "a refused seal claims nothing");
}

// C3 — a Resolved bloom is supersedable: the ADR's primary supersession trigger
// is a failed land, which happens at Resolved, so this must not wedge.
#[test]
fn resolved_bloom_is_supersedable() {
    let (snapshot, predecessor_spec) = sealed_and_resolved(1, vec![membership("wp", 10)], 40);
    let predecessor = predecessor_spec.id();
    assert_eq!(snapshot.blooms.get(&predecessor).unwrap().status, BloomStatus::Resolved);

    // A successor differing in base — a distinct id, same member (exempt from
    // conflict via the predecessor's release). The base it rebases onto has to
    // be one the source reported (#4709).
    let snapshot = observing(&snapshot, 2);
    let successor_spec = draft(2, vec![membership("wp", 10)]).seal();
    let successor = successor_spec.id();
    let (after, decided) = step(&snapshot, &event("sup", Fact::Supersede { predecessor, successor: successor_spec }));
    assert!(matches!(decided.outcome, Outcome::Superseded { .. }));
    assert_eq!(after.blooms.get(&predecessor).unwrap().status, BloomStatus::Superseded);
    assert_eq!(after.blooms.get(&successor).unwrap().status, BloomStatus::Sealed);
}

// A successor whose members all arrive already integrated has a complete claim
// set the instant it seals and no member left to run, so nothing downstream
// would ever dispatch its fold: `reduce_integrate` dispatches on the claim that
// *completes* the set, and here every claim was inherited in the supersession
// itself. Without an integration dispatch here the successor is claimed,
// complete, and permanently unresolvable — the predecessor's work carried over
// and then stranded. This is the re-base shape, so it is the path a bloom
// catching up to a moved base takes.
#[test]
fn a_successor_inheriting_every_claim_dispatches_its_own_fold() {
    let (snapshot, predecessor_spec) = sealed_and_resolved(1, vec![membership("wp", 10)], 40);
    let predecessor = predecessor_spec.id();

    // Same member at the same scope revision on a different base: every claim
    // inherits, and no member enters the line fresh.
    let snapshot = observing(&snapshot, 2);
    let successor_spec = draft(2, vec![membership("wp", 10)]).seal();
    let successor = successor_spec.id();
    let successor_base = successor_spec.base();
    let (_, decided) = step(&snapshot, &event("sup", Fact::Supersede { predecessor, successor: successor_spec }));

    assert!(
        !decided.effects.iter().any(|e| matches!(e, Decision::DispatchAttempt { .. })),
        "no member enters the line fresh — every one arrived integrated",
    );
    match decided.effects.iter().find(|e| matches!(e, Decision::DispatchIntegration { .. })) {
        Some(Decision::DispatchIntegration { bloom, base, members, adopt_from }) => {
            assert_eq!(*bloom, successor, "the fold is dispatched for the successor, not the predecessor");
            assert_eq!(*base, successor_base, "and folds onto the successor's base — the point of the re-base");
            assert_eq!(
                *adopt_from,
                Some(predecessor),
                "the candidates were produced under the predecessor's id, so the successor adopts its refs \
                 rather than folding refs it does not have",
            );
            let folded: Vec<(&str, Digest)> =
                members.iter().map(|member| (member.workpiece.0.as_str(), member.candidate)).collect();
            assert_eq!(folded, vec![("wp", digest(100))], "carrying the inherited claim's candidate");
        }
        other => panic!("expected a DispatchIntegration for the successor, got {other:?}"),
    }
}

// C4 — a bloom cannot supersede itself into a bloom superseded by itself: an
// identical successor spec (same id) is refused.
#[test]
fn self_supersession_is_refused() {
    let base = Snapshot::new(digest(1));
    let spec = draft(1, vec![membership("wp", 10)]).seal();
    let predecessor = spec.id();
    let (after_seal, _) = step(&base, &event("seal", Fact::Seal(spec.clone())));

    let decided = reduce(
        &after_seal,
        &event("self", Fact::Supersede { predecessor, successor: spec }),
        &ResolvedConfigs::default(),
    );
    assert!(matches!(decided.outcome, Outcome::SupersedeRejected(SupersedeError::SelfSupersession)));
}

// C4 — a successor id that collides with some *other* already-known bloom is
// refused rather than resurrecting and overwriting that bloom's record,
// mirroring `reduce_seal`'s `KnownBloom` guard on the seal door.
#[test]
fn supersede_rejects_a_known_successor_id() {
    let mut snapshot = Snapshot::new(digest(1));
    // A landed bloom whose id the successor will collide with.
    let known = draft(9, vec![membership("landed", 10)]).seal();
    splice_bloom(&mut snapshot, &known, BloomStatus::Landed);
    // A fresh, unrelated predecessor to supersede.
    let predecessor_spec = draft(1, vec![membership("own", 20)]).seal();
    let predecessor = predecessor_spec.id();
    splice_bloom(&mut snapshot, &predecessor_spec, BloomStatus::Sealed);

    let (after, decided) = step(&snapshot, &event("sup", Fact::Supersede { predecessor, successor: known.clone() }));
    match decided.outcome {
        Outcome::SupersedeRejected(SupersedeError::KnownSuccessor(id)) => assert_eq!(id, known.id()),
        other => panic!("expected KnownSuccessor, got {other:?}"),
    }
    // The colliding bloom's record is untouched by the rejected supersession.
    assert_eq!(after.blooms.get(&known.id()).unwrap().status, BloomStatus::Landed);
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
    let decided = reduce(
        &snapshot,
        &event("sup", Fact::Supersede { predecessor, successor: successor_spec }),
        &ResolvedConfigs::default(),
    );
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
    let decided =
        reduce(&snapshot, &event("dup", Fact::Supersede { predecessor, successor: dup }), &ResolvedConfigs::default());
    assert_eq!(
        decided.outcome,
        Outcome::SupersedeRejected(SupersedeError::InvalidMember(SealError::DuplicateWorkpiece(workpiece("dup")))),
    );
    assert!(decided.effects.is_empty());

    // An empty successor is refused the same way.
    let empty = draft(2, vec![]).seal();
    let decided = reduce(
        &snapshot,
        &event("empty", Fact::Supersede { predecessor, successor: empty }),
        &ResolvedConfigs::default(),
    );
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

    // A valid membership (so member admission passes) but a catalog that cannot
    // run — a successor is held to the same catalog admission a fresh seal is, or
    // supersession becomes the way around the door.
    let mut catalog = StageCatalog::line();
    catalog.bindings.retain(|binding| binding.stage != StageId::Verify);
    let (unrunnable, configs) = draft_with_catalog(2, vec![membership("own", 10)], &catalog);
    let decided = reduce(
        &snapshot,
        &event("unrunnable", Fact::Supersede { predecessor, successor: unrunnable.seal() }),
        &configs,
    );
    match decided.outcome {
        Outcome::SupersedeRejected(SupersedeError::InvalidMember(SealError::UnrunnableStageCatalog(error))) => {
            assert_eq!(error, CatalogError::UnboundStage(StageId::Verify));
        }
        other => panic!("expected InvalidMember(UnrunnableStageCatalog), got {other:?}"),
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
    let snapshot = observing(&snapshot, 2);
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

// #3663 — a successor member that does not arrive already integrated (an
// inherited claim) is dispatched at supersede exactly as a seal dispatches a
// fresh member: cursor seeded at the entry stage, first attempt dispatched
// against the successor's sealed base. Catches the claimed-but-never-executed
// strand — the wedged-member escape hatch (ADR-0151) re-admits a member whose
// predecessor never integrated, and without the entry dispatch that member
// never runs and the successor never resolves.
#[test]
fn supersession_dispatches_every_non_inherited_successor_member() {
    let base = Snapshot::new(digest(1));
    // Predecessor admits "kept" (integrates at rev 10) and "wedged" (never
    // integrates — the escape-hatch case).
    let members = vec![membership("kept", 10), membership("wedged", 11)];
    let predecessor_spec = draft(1, members).seal();
    let predecessor = predecessor_spec.id();
    let (snapshot, _) = step(&base, &event("seal", Fact::Seal(predecessor_spec)));
    let (snapshot, _) =
        step(&snapshot, &event("i-kept", Fact::Integrate { bloom: predecessor, claim: claim("kept", 10, 100) }));

    // The successor re-admits "kept" at the same revision (inherits), "wedged"
    // at a fresh revision, and a net-new "grown".
    let snapshot = observing(&snapshot, 2);
    let successor_spec =
        draft(2, vec![membership("kept", 10), membership("wedged", 12), membership("grown", 13)]).seal();
    let successor = successor_spec.id();
    let successor_base = successor_spec.base();
    let (after, decided) = step(&snapshot, &event("sup", Fact::Supersede { predecessor, successor: successor_spec }));
    assert!(matches!(decided.outcome, Outcome::Superseded { .. }));

    // The non-inherited members are dispatched at the entry stage against the
    // successor's sealed base; the inherited member is not re-run.
    let dispatched: Vec<_> = decided
        .effects
        .iter()
        .filter_map(|effect| match effect {
            Decision::DispatchAttempt { workpiece, stage, transformation, .. } => {
                Some((workpiece.clone(), *stage, transformation.checkout))
            }
            _ => None,
        })
        .collect();
    // Canonical member order leads on the workpiece (ADR-0174 re-keyed the sort
    // when `configs` joined the member), so "grown" dispatches before "wedged".
    assert_eq!(
        dispatched,
        vec![
            (workpiece("grown"), StageId::Construct, successor_base),
            (workpiece("wedged"), StageId::Construct, successor_base),
        ],
        "exactly the non-inherited members dispatch, at Construct, against the successor's base",
    );

    // The applied record seeds cursors for exactly the dispatched members.
    let progress = &after.blooms.get(&successor).unwrap().progress;
    assert!(progress.get(&workpiece("kept")).is_none(), "an inherited member never enters the line");
    for name in ["wedged", "grown"] {
        let cursor = progress.get(&workpiece(name)).unwrap();
        assert_eq!((cursor.stage, cursor.attempts), (StageId::Construct, 1), "{name} is seeded at the entry stage");
    }

    // A completion for the inherited, cursor-less member refuses as
    // NotDispatched — not a fabricated entry-stage StageMismatch.
    let stray = reduce(
        &after,
        &event(
            "stray",
            Fact::AttemptCompleted {
                bloom: successor,
                workpiece: workpiece("kept"),
                stage: StageId::Construct,
                passed: false,
                evidence: attempt_evidence(),
                candidate: None,
            },
        ),
        &ResolvedConfigs::default(),
    );
    assert!(matches!(
        stray.outcome,
        Outcome::AttemptCompletedRejected(AttemptCompletedError::NotDispatched(ref wp)) if *wp == workpiece("kept"),
    ));
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

    // The resolved bloom — through the fold's aggregate review pass
    // (ADR-0153) — reads the refined claim.
    let (after, _) =
        step(&snapshot, &event("r", Fact::Resolve { bloom, tree: digest(40), head: digest(41), lineage: vec![] }));
    let verdict = event(
        "v",
        Fact::AggregateReviewCompleted {
            bloom,
            passed: true,
            evidence: Evidence { subject: digest(40), kind: EvidenceKind::ReviewFinding, detail: digest(50) },
            implicated: vec![],
        },
    );
    match reduce(&after, &verdict, &ResolvedConfigs::default()).outcome {
        Outcome::Resolved(bloom) => {
            assert_eq!(bloom.resolution_claims.len(), 1);
            assert_eq!(bloom.resolution_claims[0].candidate, digest(200));
        }
        other => panic!("expected Resolved, got {other:?}"),
    }
}

/// The passing aggregate-verify verdict a held fold takes before its critic
/// dispatches (#4696) — the mechanical gate now sits between the fold and the
/// review, so a test that drives the review path threads this hop first.
fn verify_passed(bloom: BloomId, key: &str, tree: u8) -> Event {
    event(
        key,
        Fact::AggregateVerifyCompleted {
            bloom,
            passed: true,
            evidence: Evidence { subject: digest(tree), kind: EvidenceKind::VerificationResult, detail: digest(51) },
        },
    )
}

// #4689 — the landing gate is the last one, and the only one that judges the
// bloom against a mainline that moved while it worked. A refused landing
// un-resolves the bloom and re-opens every member for repair; the second
// refusal spends the `Land` budget and parks it. Neither leaves the bloom
// polling a proposal nothing will accept, which is the behaviour this replaces.
//
// Tripwire on the un-resolve above all: a bloom left `Resolved` while its
// members repair would let the land reactor re-propose the exact head the gate
// just refused, which is an infinite loop rather than a repair.
#[test]
fn a_refused_landing_reopens_the_line_then_parks_at_the_budget() {
    let base = Snapshot::new(digest(1));
    let spec = draft(1, vec![membership("alpha", 10), membership("beta", 11)]).seal();
    let bloom = spec.id();
    let (snapshot, _) = step(&base, &event("seal", Fact::Seal(spec)));
    let (snapshot, _) = step(&snapshot, &event("i-a", Fact::Integrate { bloom, claim: claim("alpha", 10, 100) }));
    let (snapshot, _) = step(&snapshot, &event("i-b", Fact::Integrate { bloom, claim: claim("beta", 11, 101) }));
    let (snapshot, _) =
        step(&snapshot, &event("r1", Fact::Resolve { bloom, tree: digest(40), head: digest(41), lineage: vec![] }));
    let (snapshot, _) = step(&snapshot, &verify_passed(bloom, "v1", 40));
    let (snapshot, _) = step(
        &snapshot,
        &event(
            "review",
            Fact::AggregateReviewCompleted {
                bloom,
                passed: true,
                evidence: Evidence { subject: digest(40), kind: EvidenceKind::ReviewFinding, detail: digest(50) },
                implicated: vec![],
            },
        ),
    );
    assert_eq!(snapshot.blooms.get(&bloom).unwrap().status, BloomStatus::Resolved, "the bloom is awaiting its land");

    let refused = |key: &str, head: u8| {
        event(
            key,
            Fact::LandingRejected {
                bloom,
                evidence: Evidence {
                    subject: digest(head),
                    kind: EvidenceKind::VerificationResult,
                    detail: digest(60),
                },
            },
        )
    };

    // A rejection naming a head other than the one being landed is stale.
    assert!(matches!(
        reduce(&snapshot, &refused("stale", 99), &ResolvedConfigs::default()).outcome,
        Outcome::LandingRejectedRefused(LandingRejectedError::SubjectMismatch { .. }),
    ));

    let (after1, d1) = step(&snapshot, &refused("red-1", 41));
    assert!(matches!(&d1.outcome, Outcome::LandingReentered { members, rolls: 1, .. }
        if *members == vec![workpiece("alpha"), workpiece("beta")]));
    let record = after1.blooms.get(&bloom).unwrap();
    assert_eq!(record.status, BloomStatus::Sealed, "the bloom is no longer land-ready");
    assert_eq!(record.resolved_head, None, "and no longer names a head to propose");
    assert!(record.claims.is_empty(), "every member's claim is revoked");
    assert_eq!(record.landing_rolls, 1);
    for member in ["alpha", "beta"] {
        assert_eq!(record.progress.get(&workpiece(member)).unwrap().stage, StageId::Refine);
    }

    // Repair, re-fold, re-verify, re-review, and land again — the whole cycle,
    // because a landing rejection re-opens the line rather than short-cutting
    // back to a fresh proposal on the same artifact.
    let (s2, _) = step(&after1, &event("i-a2", Fact::Integrate { bloom, claim: claim("alpha", 10, 102) }));
    let (s2, _) = step(&s2, &event("i-b2", Fact::Integrate { bloom, claim: claim("beta", 11, 103) }));
    let (s2, _) = step(&s2, &event("r2", Fact::Resolve { bloom, tree: digest(44), head: digest(45), lineage: vec![] }));
    let (s2, _) = step(&s2, &verify_passed(bloom, "v2", 44));
    let (s2, _) = step(
        &s2,
        &event(
            "review-2",
            Fact::AggregateReviewCompleted {
                bloom,
                passed: true,
                evidence: Evidence { subject: digest(44), kind: EvidenceKind::ReviewFinding, detail: digest(51) },
                implicated: vec![],
            },
        ),
    );
    assert_eq!(s2.blooms.get(&bloom).unwrap().status, BloomStatus::Resolved);

    // The second refusal spends the budget: parked, nothing re-opens.
    let (after2, d2) = step(&s2, &refused("red-2", 45));
    assert!(matches!(d2.outcome, Outcome::LandingParked { rolls: 2, question, .. } if question == digest(60)));
    assert!(
        !d2.effects.iter().any(|e| matches!(e, Decision::DispatchAttempt { .. } | Decision::SetUnresolved { .. })),
        "a parked bloom re-opens nothing",
    );
    assert_eq!(after2.blooms.get(&bloom).unwrap().review_park, Some(digest(60)));
}

// #4696 — a fold that does not build re-opens every member, not just an
// implicated one: each member's own Verify passed on its own candidate, so a
// failure that appears only in the fold belongs to the combination, and a
// compiler names no owners to narrow it to. The stale fold clears so the
// re-integration produces a fresh one, and the stage's own budget bounds the
// retries — the second failure parks the bloom rather than re-folding a
// combination that has not built yet.
//
// Tripwire for the over-routing above all: narrowing this to a subset would
// strand a fold whose failure belongs to a member the narrowing left closed.
#[test]
fn a_failing_aggregate_verify_reopens_every_member_then_parks_at_the_ceiling() {
    let base = Snapshot::new(digest(1));
    let spec = draft(1, vec![membership("alpha", 10), membership("beta", 11)]).seal();
    let bloom = spec.id();
    let (snapshot, _) = step(&base, &event("seal", Fact::Seal(spec)));
    let (snapshot, _) = step(&snapshot, &event("i-a", Fact::Integrate { bloom, claim: claim("alpha", 10, 100) }));
    let (snapshot, _) = step(&snapshot, &event("i-b", Fact::Integrate { bloom, claim: claim("beta", 11, 101) }));
    let (snapshot, _) =
        step(&snapshot, &event("r1", Fact::Resolve { bloom, tree: digest(40), head: digest(41), lineage: vec![] }));

    let failed = |key: &str, subject: u8| {
        event(
            key,
            Fact::AggregateVerifyCompleted {
                bloom,
                passed: false,
                evidence: Evidence {
                    subject: digest(subject),
                    kind: EvidenceKind::VerificationResult,
                    detail: digest(52),
                },
            },
        )
    };

    // A verdict bound to a tree other than the held fold's is stale — refused,
    // so a superseded fold's failure cannot re-open members under a newer one.
    assert!(matches!(
        reduce(&snapshot, &failed("stale", 99), &ResolvedConfigs::default()).outcome,
        Outcome::AggregateVerifyRejected(AggregateVerifyError::SubjectMismatch { .. }),
    ));

    let (after1, d1) = step(&snapshot, &failed("fail-1", 40));
    assert!(matches!(&d1.outcome, Outcome::AggregateVerifyReentered { members, rolls: 1, .. }
        if *members == vec![workpiece("alpha"), workpiece("beta")]));
    let record = after1.blooms.get(&bloom).unwrap();
    assert!(record.claims.is_empty(), "every member's claim is revoked, not just one");
    assert!(record.integration.is_none(), "the stale fold is cleared");
    assert_eq!(record.aggregate_verify_rolls, 1);
    assert_eq!(record.aggregate_rolls, 0, "a spent verify roll does not spend the critic's budget");
    for member in ["alpha", "beta"] {
        assert_eq!(record.progress.get(&workpiece(member)).unwrap().stage, StageId::Refine);
    }

    // Both members repair and re-integrate; the re-fold re-dispatches the verify.
    let (after2, _) = step(&after1, &event("i-a2", Fact::Integrate { bloom, claim: claim("alpha", 10, 102) }));
    let (after3, _) = step(&after2, &event("i-b2", Fact::Integrate { bloom, claim: claim("beta", 11, 103) }));
    let (after4, d2) =
        step(&after3, &event("r2", Fact::Resolve { bloom, tree: digest(44), head: digest(45), lineage: vec![] }));
    assert!(matches!(d2.outcome, Outcome::AggregateVerifyDispatched { roll: 2, .. }));

    // The second failure spends the budget: the bloom parks, re-opens nothing,
    // and dispatches nothing further.
    let (after5, d3) = step(&after4, &failed("fail-2", 44));
    assert!(matches!(d3.outcome, Outcome::AggregateVerifyParked { rolls: 2, question, .. } if question == digest(52)));
    assert!(
        !d3.effects.iter().any(|e| matches!(e, Decision::DispatchAttempt { .. } | Decision::RevokeResolution { .. })),
        "a parked bloom re-opens nothing and dispatches nothing",
    );
    assert_eq!(after5.blooms.get(&bloom).unwrap().review_park, Some(digest(52)));
}

// ADR-0153 — a failing aggregate review freezes into member routing: every
// implicated member's claim is revoked and its cursor re-enters the
// repair-only Refine (the bloom cannot resolve while any member is re-open),
// the stale fold clears, the re-fold dispatches the delta-confirm, and the
// second failing verdict parks the bloom at the two-pass ceiling — never a
// third roll. Tripwire for every arm of the fail path.
#[test]
fn a_failing_aggregate_review_reopens_members_then_parks_at_the_ceiling() {
    let base = Snapshot::new(digest(1));
    let spec = draft(1, vec![membership("alpha", 10), membership("beta", 11)]).seal();
    let bloom = spec.id();
    let (snapshot, _) = step(&base, &event("seal", Fact::Seal(spec)));
    let (snapshot, _) = step(&snapshot, &event("i-a", Fact::Integrate { bloom, claim: claim("alpha", 10, 100) }));
    let (snapshot, _) = step(&snapshot, &event("i-b", Fact::Integrate { bloom, claim: claim("beta", 11, 101) }));
    let (snapshot, _) =
        step(&snapshot, &event("r1", Fact::Resolve { bloom, tree: digest(40), head: digest(41), lineage: vec![] }));
    let (snapshot, _) = step(&snapshot, &verify_passed(bloom, "v1", 40));

    let verdict = |key: &str, passed: bool, subject: u8, implicated: Vec<&str>| {
        event(
            key,
            Fact::AggregateReviewCompleted {
                bloom,
                passed,
                evidence: Evidence { subject: digest(subject), kind: EvidenceKind::ReviewFinding, detail: digest(50) },
                implicated: implicated.into_iter().map(workpiece).collect(),
            },
        )
    };

    // A verdict bound to a tree other than the held fold's is stale — refused.
    assert!(matches!(
        reduce(&snapshot, &verdict("stale", false, 99, vec!["alpha"]), &ResolvedConfigs::default()).outcome,
        Outcome::AggregateReviewRejected(AggregateReviewError::SubjectMismatch { .. }),
    ));
    // A failing verdict with an empty implication routes to every member —
    // the host admits verdicts without membership knowledge, so the reducer
    // expands the empty set rather than stranding the verdict.
    assert!(matches!(
        &reduce(&snapshot, &verdict("empty", false, 40, vec![]), &ResolvedConfigs::default()).outcome,
        Outcome::AggregateReviewReentered { members, rolls: 1, .. }
            if *members == vec![workpiece("alpha"), workpiece("beta")],
    ));
    // A failing verdict naming a non-member is malformed.
    assert!(matches!(
        reduce(&snapshot, &verdict("ghost", false, 40, vec!["ghost"]), &ResolvedConfigs::default()).outcome,
        Outcome::AggregateReviewRejected(AggregateReviewError::NotAMember(_)),
    ));

    // The first failing verdict re-opens exactly the implicated member: claim
    // revoked, cursor into Refine, fold cleared.
    let (after1, d1) = step(&snapshot, &verdict("fail-1", false, 40, vec!["alpha"]));
    assert!(matches!(&d1.outcome, Outcome::AggregateReviewReentered { members, rolls: 1, .. }
        if members == &vec![workpiece("alpha")]));
    assert!(
        d1.effects.iter().any(|e| matches!(e, Decision::DispatchAttempt { stage: StageId::Refine, workpiece: wp, .. }
            if *wp == workpiece("alpha"))),
        "the re-opened member dispatches into Refine",
    );
    let record = after1.blooms.get(&bloom).unwrap();
    assert!(!record.claims.contains_key(&workpiece("alpha")), "the implicated member's claim is revoked");
    assert!(record.claims.contains_key(&workpiece("beta")), "an unimplicated member's claim survives");
    assert!(record.integration.is_none(), "the stale fold is cleared");
    assert_eq!(record.aggregate_rolls, 1);
    assert_eq!(record.progress.get(&workpiece("alpha")).unwrap().stage, StageId::Refine);

    // The bloom cannot resolve while a member is re-open.
    assert!(matches!(
        reduce(
            &after1,
            &event("r-open", Fact::Resolve { bloom, tree: digest(42), head: digest(43), lineage: vec![] }),
            &ResolvedConfigs::default()
        )
        .outcome,
        Outcome::ResolveRejected(ResolveError::MemberNotIntegrated { .. }),
    ));

    // The repaired member re-integrates; the re-fold dispatches the
    // delta-confirm (roll 2).
    let (after2, _) = step(&after1, &event("i-a2", Fact::Integrate { bloom, claim: claim("alpha", 10, 102) }));
    let (after3, _) =
        step(&after2, &event("r2", Fact::Resolve { bloom, tree: digest(44), head: digest(45), lineage: vec![] }));
    let (after3, d2) = step(&after3, &verify_passed(bloom, "v2", 44));
    assert!(matches!(d2.outcome, Outcome::AggregateVerifyPassed { .. }));
    assert!(
        d2.effects.iter().any(|e| matches!(e, Decision::DispatchAggregateReview { roll: 2, .. })),
        "the passing re-verify dispatches the delta-confirm",
    );

    // The failing delta-confirm hits the ceiling: the bloom parks to the owner
    // (ADR-0151's hold vocabulary at bloom scope) — no member re-opens,
    // nothing further dispatches, and the failing review's record artifact
    // becomes the parked question holding the bloom.
    let (after4, d3) = step(&after3, &verdict("fail-2", false, 44, vec!["alpha"]));
    assert!(matches!(d3.outcome, Outcome::AggregateReviewParked { rolls: 2, question, .. } if question == digest(50)));
    assert!(
        !d3.effects.iter().any(|e| matches!(e, Decision::DispatchAttempt { .. } | Decision::RevokeResolution { .. })),
        "a parked bloom re-opens nothing and dispatches nothing",
    );
    let record = after4.blooms.get(&bloom).unwrap();
    assert_eq!(record.review_park, Some(digest(50)), "the park marker names the failing review's record artifact");
    assert!(record.holds.contains(&digest(50)), "the park raises the pending-decision hold");
    assert!(record.integration.is_some(), "the fold stays held as the owner's decision context");
    // A re-fold while parked is refused by the pending decision — the named
    // reason is the owner's open question, not a bare ceiling count.
    assert!(matches!(
        reduce(&after4, &event("r3", Fact::Resolve { bloom, tree: digest(46), head: digest(47), lineage: vec![] }), &ResolvedConfigs::default())
            .outcome,
        Outcome::ResolveRejected(ResolveError::PendingDecision { question }) if question == digest(50),
    ));
}

// ADR-0153 — the owner's answer to a parked bloom re-arms the review cycle:
// adopting the park question releases the hold, clears the marker, resets the
// roll cursor, and dispatches a fresh full review from the still-held fold.
// The owner bought the new cycle explicitly; the machine never buys its own
// third roll. Tripwire: the bloom-scope adoption falling through to the
// member-stage redispatch path, whose payload no consumer could resolve.
#[test]
fn adopting_the_park_question_rearms_the_review_cycle() {
    let base = Snapshot::new(digest(1));
    let spec = draft(1, vec![membership("wp", 10)]).seal();
    let bloom = spec.id();
    let (snapshot, _) = step(&base, &event("seal", Fact::Seal(spec)));
    let (snapshot, _) = step(&snapshot, &event("i1", Fact::Integrate { bloom, claim: claim("wp", 10, 100) }));
    let (snapshot, _) =
        step(&snapshot, &event("r1", Fact::Resolve { bloom, tree: digest(40), head: digest(41), lineage: vec![] }));

    let fail = |key: &str, subject: u8, detail: u8| {
        event(
            key,
            Fact::AggregateReviewCompleted {
                bloom,
                passed: false,
                evidence: Evidence {
                    subject: digest(subject),
                    kind: EvidenceKind::ReviewFinding,
                    detail: digest(detail),
                },
                implicated: vec![],
            },
        )
    };
    let (snapshot, _) = step(&snapshot, &fail("f1", 40, 50));
    let (snapshot, _) = step(&snapshot, &event("i2", Fact::Integrate { bloom, claim: claim("wp", 10, 101) }));
    let (snapshot, _) =
        step(&snapshot, &event("r2", Fact::Resolve { bloom, tree: digest(42), head: digest(43), lineage: vec![] }));
    let (parked, decided) = step(&snapshot, &fail("f2", 42, 51));
    assert!(matches!(decided.outcome, Outcome::AggregateReviewParked { question, .. } if question == digest(51)));

    let (rearmed, adopted) =
        step(&parked, &event("ans", Fact::AdoptAnswer { bloom, answer: answer_adopting(digest(51)) }));
    assert!(matches!(adopted.outcome, Outcome::AnswerAdopted { question, .. } if question == digest(51)));
    assert!(
        adopted.effects.iter().any(|e| matches!(e, Decision::DispatchAggregateReview { roll: 1, .. })),
        "the re-armed cycle dispatches a fresh full review from the held fold",
    );
    assert!(
        !adopted.effects.iter().any(|e| matches!(e, Decision::RedispatchStage { .. })),
        "a bloom-scope adoption never routes down the member-stage redispatch path",
    );
    let record = rearmed.blooms.get(&bloom).unwrap();
    assert!(record.holds.is_empty(), "the park hold is released");
    assert_eq!(record.review_park, None, "the park marker clears");
    assert_eq!(record.aggregate_rolls, 0, "the roll cursor resets — a whole owner-bought cycle");

    // The re-armed cycle runs whole: a failing verdict re-enters the member
    // again instead of tripping the spent ceiling.
    let reentered = reduce(&rearmed, &fail("f3", 42, 52), &ResolvedConfigs::default());
    assert!(matches!(reentered.outcome, Outcome::AggregateReviewReentered { rolls: 1, .. }));
}

// ADR-0153 — the aggregate review itself parking ("the findings are
// contested") raises the bloom-scope park: a Question bound to the held
// fold's tree sets the park marker alongside the ordinary ADR-0151 hold, so
// its adoption re-arms the review cycle rather than re-dispatching a member
// stage.
#[test]
fn a_question_bound_to_the_held_fold_marks_the_review_park() {
    let base = Snapshot::new(digest(1));
    let spec = draft(1, vec![membership("wp", 10)]).seal();
    let bloom = spec.id();
    let (snapshot, _) = step(&base, &event("seal", Fact::Seal(spec)));
    let (snapshot, _) = step(&snapshot, &event("i1", Fact::Integrate { bloom, claim: claim("wp", 10, 100) }));
    let (snapshot, _) =
        step(&snapshot, &event("r1", Fact::Resolve { bloom, tree: digest(40), head: digest(41), lineage: vec![] }));

    let contested = Evidence { subject: digest(40), kind: EvidenceKind::Question, detail: digest(60) };
    let (held, _) = step(&snapshot, &event("park", Fact::AdmitEvidence { bloom, evidence: contested }));
    let record = held.blooms.get(&bloom).unwrap();
    assert_eq!(record.review_park, Some(digest(60)), "the fold-bound question is the review park");
    assert!(record.holds.contains(&digest(60)), "the ordinary hold rises with it");

    let adopted = reduce(
        &held,
        &event("ans", Fact::AdoptAnswer { bloom, answer: answer_adopting(digest(60)) }),
        &ResolvedConfigs::default(),
    );
    assert!(
        adopted.effects.iter().any(|e| matches!(e, Decision::DispatchAggregateReview { roll: 1, .. })),
        "adopting the contested park re-arms the review, not a member redispatch",
    );
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
    let unknown =
        reduce(&base, &event("u", Fact::AdmitEvidence { bloom, evidence: study }), &ResolvedConfigs::default());
    assert!(matches!(unknown.outcome, Outcome::AdmitEvidenceRejected(AdmitEvidenceError::UnknownOrInactiveBloom)));
    assert!(unknown.effects.is_empty());

    let (snapshot, _) = step(&base, &event("seal", Fact::Seal(spec)));

    // A resolution claim is bound to the integrate door, not the evidence log.
    let claim_ev = Evidence { subject: digest(70), kind: EvidenceKind::ResolutionClaim, detail: digest(80) };
    let mis_routed =
        reduce(&snapshot, &event("c", Fact::AdmitEvidence { bloom, evidence: claim_ev }), &ResolvedConfigs::default());
    assert!(matches!(mis_routed.outcome, Outcome::AdmitEvidenceRejected(AdmitEvidenceError::EvidenceNotBound)));

    // An approval seals a member; it is not free-log evidence either.
    let approval = Evidence { subject: digest(70), kind: EvidenceKind::Approval, detail: digest(80) };
    let also_mis =
        reduce(&snapshot, &event("a", Fact::AdmitEvidence { bloom, evidence: approval }), &ResolvedConfigs::default());
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

// A parked question named on `wp` for the bloom `bloom`, its evidence bound to
// the attempt subject. Returns the built question so a test can name its digest.
fn parked_question(workpiece: &str) -> Question {
    Question {
        stage: StageId::Construct,
        subject: digest(70),
        workpiece: aether_bloomery::WorkpieceId(workpiece.into()),
        prompt: "tie between A and B".into(),
        options: vec!["A".into(), "B".into()],
        blocked: "construct is held".into(),
    }
}

// An author-signed statement adopting `question` — the answer shape the reducer
// admits as intent.
fn answer_adopting(question: Digest) -> Statement {
    Statement {
        words: b"answer: choose A".to_vec(),
        provenance: Provenance::AuthorSignature(SignatureEnvelope { signer: KeyId("owner".into()), signature: vec![] }),
        parents: vec![question],
    }
}

// ADR-0151 — admitting a Question derives a pending-decision hold that blocks
// resolve. The hold is folded from the evidence log (the question digest is the
// evidence detail), and `reduce_resolve` refuses a bloom with any open hold by
// name (PendingDecision), not a misreported MemberNotIntegrated.
#[test]
fn a_question_admission_holds_the_bloom_and_blocks_resolve() {
    let base = Snapshot::new(digest(1));
    let spec = draft(1, vec![membership("wp", 10)]).seal();
    let bloom = spec.id();
    let (snapshot, _) = step(&base, &event("seal", Fact::Seal(spec)));

    let question = parked_question("wp");
    let question_digest = question.id();
    let evidence = Evidence { subject: digest(70), kind: EvidenceKind::Question, detail: question_digest };
    let (held, decided) = step(&snapshot, &event("park", Fact::AdmitEvidence { bloom, evidence }));
    assert!(matches!(decided.outcome, Outcome::EvidenceAdmitted { .. }), "a question admits as evidence");
    assert!(held.blooms.get(&bloom).unwrap().holds.contains(&question_digest), "the fold derives the hold");

    // Even with the (single) member integrated, the open hold blocks resolve.
    let claim = claim("wp", 10, 100);
    let (integrated, _) = step(&held, &event("int", Fact::Integrate { bloom, claim }));
    let resolve = reduce(
        &integrated,
        &event("r", Fact::Resolve { bloom, tree: digest(40), head: digest(41), lineage: vec![] }),
        &ResolvedConfigs::default(),
    );
    assert!(
        matches!(resolve.outcome, Outcome::ResolveRejected(ResolveError::PendingDecision { question: q }) if q == question_digest),
        "an open hold blocks resolve, named by the held question",
    );
}

// ADR-0151 — an adopting answer releases the hold and re-dispatches: the answer
// names the held question digest, so the reducer clears the hold and emits a
// RedispatchStage outbox effect carrying the answer digest; the bloom can then
// resolve. A sibling member is unaffected throughout.
#[test]
fn an_adopted_answer_releases_the_hold_and_redispatches() {
    let base = Snapshot::new(digest(1));
    let spec = draft(1, vec![membership("held", 10), membership("free", 11)]).seal();
    let bloom = spec.id();
    let (mut snapshot, _) = step(&base, &event("seal", Fact::Seal(spec)));

    // The free sibling integrates while the other member is parked.
    let question = parked_question("held");
    let question_digest = question.id();
    let park_ev = Evidence { subject: digest(70), kind: EvidenceKind::Question, detail: question_digest };
    (snapshot, _) = step(&snapshot, &event("park", Fact::AdmitEvidence { bloom, evidence: park_ev }));
    (snapshot, _) = step(&snapshot, &event("int-free", Fact::Integrate { bloom, claim: claim("free", 11, 101) }));
    (snapshot, _) = step(&snapshot, &event("int-held", Fact::Integrate { bloom, claim: claim("held", 10, 100) }));

    // Both members integrated, but the hold still blocks resolve.
    let blocked = reduce(
        &snapshot,
        &event("r1", Fact::Resolve { bloom, tree: digest(40), head: digest(41), lineage: vec![] }),
        &ResolvedConfigs::default(),
    );
    assert!(matches!(blocked.outcome, Outcome::ResolveRejected(ResolveError::PendingDecision { .. })));

    // The answer adopts the held question: hold released, re-dispatch emitted.
    let answer = answer_adopting(question_digest);
    let answer_digest = aether_bloomery::digest_of(&answer);
    let (released, adopted) = step(&snapshot, &event("ans", Fact::AdoptAnswer { bloom, answer }));
    assert!(
        matches!(adopted.outcome, Outcome::AnswerAdopted { bloom: b, question: q } if b == bloom && q == question_digest),
        "the answer adopts the exact question",
    );
    assert!(
        adopted.effects.iter().any(|effect| matches!(
            effect,
            Decision::RedispatchStage { question: q, answer: a, .. } if *q == question_digest && *a == answer_digest,
        )),
        "the re-dispatch carries both the question and the answer digests",
    );
    assert!(!released.blooms.get(&bloom).unwrap().holds.contains(&question_digest), "the hold is released");

    // With the hold gone and every member integrated, the fold proceeds to its
    // aggregate review (ADR-0153) — the resolve is no longer refused.
    let resolved = reduce(
        &released,
        &event("r2", Fact::Resolve { bloom, tree: digest(40), head: digest(41), lineage: vec![] }),
        &ResolvedConfigs::default(),
    );
    assert!(
        matches!(resolved.outcome, Outcome::AggregateVerifyDispatched { .. }),
        "resolve proceeds once the hold clears",
    );
}

// ADR-0151 — the reducer's structural adoption gate: a non-author statement can
// never become intent (NotInstructionCapable), and an author-signed statement
// whose parents name no open hold releases nothing (NoMatchingHold). The hold
// persists in both refusals.
#[test]
fn an_answer_that_does_not_adopt_a_held_question_is_refused() {
    let base = Snapshot::new(digest(1));
    let spec = draft(1, vec![membership("wp", 10)]).seal();
    let bloom = spec.id();
    let (snapshot, _) = step(&base, &event("seal", Fact::Seal(spec)));
    let question = parked_question("wp");
    let question_digest = question.id();
    let park_ev = Evidence { subject: digest(70), kind: EvidenceKind::Question, detail: question_digest };
    let (held, _) = step(&snapshot, &event("park", Fact::AdmitEvidence { bloom, evidence: park_ev }));

    // A non-author statement (an observation) is never instruction-capable.
    let observed = Statement {
        words: b"i saw a reply".to_vec(),
        provenance: Provenance::ObservationAttestation(Observation { source: "github".into() }),
        parents: vec![question_digest],
    };
    let refused =
        reduce(&held, &event("obs", Fact::AdoptAnswer { bloom, answer: observed }), &ResolvedConfigs::default());
    assert!(matches!(refused.outcome, Outcome::AdoptAnswerRejected(AdoptAnswerError::NotInstructionCapable)));

    // An author signature that adopts an unheld digest releases nothing.
    let wrong = answer_adopting(digest(222));
    let no_match =
        reduce(&held, &event("wrong", Fact::AdoptAnswer { bloom, answer: wrong }), &ResolvedConfigs::default());
    assert!(matches!(no_match.outcome, Outcome::AdoptAnswerRejected(AdoptAnswerError::NoMatchingHold)));

    assert!(held.blooms.get(&bloom).unwrap().holds.contains(&question_digest), "a refused answer leaves the hold");
}

// m4 — an unknown bloom and a not-yet-resolved bloom each land-refuse with their
// own reason, never a misreported BaseMismatch.
#[test]
fn land_refusals_name_their_own_reason() {
    let base = Snapshot::new(digest(1));
    let spec = draft(1, vec![membership("wp", 10)]).seal();
    let bloom = spec.id();

    // Unknown bloom.
    let unknown = reduce(&base, &event("u", Fact::Land { bloom, new_head: digest(40) }), &ResolvedConfigs::default());
    assert!(matches!(unknown.outcome, Outcome::LandRejected(LandError::UnknownBloom(_))));

    // Sealed but not resolved.
    let (after_seal, _) = step(&base, &event("seal", Fact::Seal(spec)));
    let not_resolved =
        reduce(&after_seal, &event("nr", Fact::Land { bloom, new_head: digest(40) }), &ResolvedConfigs::default());
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

// ADR-0153 + #4696 — the fold passes two gates before the bloom resolves, in
// order, and resolution is land-readiness. A verified `Fact::Resolve` holds the
// fold and dispatches the whole-bloom aggregate *verify* — the compiler, not the
// critic; only its passing verdict dispatches the aggregate review, and only the
// review's passing verdict emits SetResolved plus the DispatchLand naming the
// sealed base and the resolved integrated *head* commit (distinct from the
// artifact tree, #3615). Both lane transformations bind the integrated tree and
// check out the integrated head.
//
// Tripwire for the ordering above all: a fold reaching the model critic before
// the compiler has passed is the failure this whole change exists to prevent —
// it spends a model lane on a tree that may not build, and lets a broken fold
// reach the landing CI, where #4689 says there is no route back. Also catches a
// resolve that lands without either pass, a dispatch mis-binding tree/head, and
// the land regressing to the artifact tree.
#[test]
fn the_fold_passes_verify_then_review_before_the_bloom_resolves() {
    let spec = draft(1, vec![membership("wp", 10)]).seal();
    let bloom = spec.id();
    let mut snapshot = Snapshot::new(digest(1));
    let (next, _) = step(&snapshot, &event("seal", Fact::Seal(spec.clone())));
    snapshot = next;
    let (next, _) = step(&snapshot, &event("integrate", Fact::Integrate { bloom, claim: claim("wp", 10, 100) }));
    snapshot = next;

    let tree = digest(40);
    let head = digest(41);
    let (after, dispatched) = step(&snapshot, &event("resolve", Fact::Resolve { bloom, tree, head, lineage: vec![] }));
    assert!(matches!(dispatched.outcome, Outcome::AggregateVerifyDispatched { roll: 1, .. }));
    assert!(
        !dispatched.effects.iter().any(|effect| matches!(effect, Decision::DispatchLand { .. })),
        "no land dispatches before either verdict",
    );
    assert!(
        !dispatched.effects.iter().any(|effect| matches!(effect, Decision::DispatchAggregateReview { .. })),
        "the fold reaches the compiler before the critic, never the other way round",
    );
    match dispatched.effects.iter().find(|e| matches!(e, Decision::DispatchAggregateVerify { .. })) {
        Some(Decision::DispatchAggregateVerify { transformation, roll, .. }) => {
            assert_eq!(transformation.inputs[0], tree, "the verify evidence binds the integrated tree");
            assert_eq!(transformation.checkout, head, "the compiler builds the integrated head");
            assert_eq!(*roll, 1);
        }
        other => panic!("expected a DispatchAggregateVerify, got {other:?}"),
    }
    let folded = after.blooms.get(&bloom).unwrap().integration.as_ref().expect("the fold is held on the record");
    assert_eq!((folded.tree, folded.head), (tree, head));

    // The passing verify hands the same fold to the critic, still holding it.
    let (after, verified) = step(&after, &verify_passed(bloom, "verify", 40));
    assert!(matches!(verified.outcome, Outcome::AggregateVerifyPassed { rolls: 1, .. }));
    match verified.effects.iter().find(|e| matches!(e, Decision::DispatchAggregateReview { .. })) {
        Some(Decision::DispatchAggregateReview { transformation, roll, .. }) => {
            assert_eq!(transformation.inputs[0], tree, "the review judges the tree the verify built");
            assert_eq!(transformation.checkout, head, "the critic checks out the integrated head");
            assert_eq!(*roll, 1);
        }
        other => panic!("expected a DispatchAggregateReview, got {other:?}"),
    }
    assert!(
        !verified.effects.iter().any(|effect| matches!(effect, Decision::DispatchLand { .. })),
        "a passing verify is not land-readiness; the critic has not judged it yet",
    );

    let verdict = event(
        "verdict",
        Fact::AggregateReviewCompleted {
            bloom,
            passed: true,
            evidence: Evidence { subject: tree, kind: EvidenceKind::ReviewFinding, detail: digest(50) },
            implicated: vec![],
        },
    );
    let (final_snapshot, resolved) = step(&after, &verdict);
    assert!(matches!(resolved.outcome, Outcome::Resolved(_)), "the passing verdict resolves the bloom");
    let land = resolved.effects.iter().find_map(|effect| match effect {
        Decision::DispatchLand { bloom: landed, expected_base, new_head } if *landed == bloom => {
            Some((*expected_base, *new_head))
        }
        _ => None,
    });
    let (expected_base, new_head) = land.expect("the passing verdict emits a DispatchLand for the resolved bloom");
    assert_eq!(expected_base, spec.base(), "the land compares against the sealed base");
    assert_eq!(new_head, head, "the land advances mainline to the resolved integrated head, not the artifact tree");
    assert_ne!(new_head, tree, "the integrated head is distinct from the artifact tree (#3615)");
    let record = final_snapshot.blooms.get(&bloom).unwrap();
    assert_eq!(record.status, BloomStatus::Resolved);
    assert!(record.integration.is_none(), "the consumed fold is cleared");
    assert_eq!(record.aggregate_rolls, 1, "the verdict consumed one review pass");
}

// Tripwire: the genesis reconcile (aether-chassis-bloomery) seeds
// `Snapshot::GENESIS_MAINLINE`, and the control core starts every fresh snapshot
// at `Snapshot::default().mainline` — the two must name the same genesis base or
// the seeded correspondence and the reducer's first land address different
// digests (#3615). This drifts the moment either the `Default` seed or the const
// changes, so it guards the reconcile-vs-core agreement the split relies on.
#[test]
fn genesis_mainline_const_matches_the_default_snapshot_base() {
    assert_eq!(Snapshot::default().mainline, Snapshot::GENESIS_MAINLINE);
}

// The evidence a completed attempt carries. The reducer does not re-check its
// binding in `AttemptCompleted` (the intake trust boundary enforces it before
// admission), so any well-formed evidence stands in.
fn attempt_evidence() -> Evidence {
    Evidence { subject: digest(70), kind: EvidenceKind::VerificationResult, detail: digest(80) }
}

// ADR-0149 §The line — a seal seeds each member's cursor at the entry stage
// (`Construct`, attempt 1) and dispatches its first attempt. Tripwire: the seal
// decision carries exactly one `DispatchAttempt` per member at the entry stage,
// and the folded snapshot reflects the seeded cursor.
#[test]
fn seal_dispatches_each_member_at_the_entry_stage() {
    let base = Snapshot::new(digest(1));
    let spec = draft(1, vec![membership("alpha", 10), membership("beta", 11)]).seal();
    let bloom = spec.id();
    let (after, decided) = step(&base, &event("seal", Fact::Seal(spec)));
    assert!(matches!(decided.outcome, Outcome::Sealed(_)));

    let dispatched: Vec<(_, _)> = decided
        .effects
        .iter()
        .filter_map(|effect| match effect {
            Decision::DispatchAttempt { workpiece, stage, .. } => Some((workpiece.clone(), *stage)),
            _ => None,
        })
        .collect();
    assert_eq!(dispatched.len(), 2, "one dispatch per member");
    assert!(dispatched.iter().all(|(_, stage)| *stage == StageCatalog::entry_stage()));

    let record = after.blooms.get(&bloom).unwrap();
    for name in ["alpha", "beta"] {
        let progress = record.progress.get(&workpiece(name)).expect("each member's cursor is seeded");
        assert_eq!(progress.stage, StageId::Construct);
        assert_eq!(progress.attempts, 1);
    }
}

// ADR-0149 §The line — Tripwire: a passing gate advances exactly one stage and
// dispatches the next. A passing `Construct` advances the cursor to `Verify` and
// dispatches one `Verify` attempt.
#[test]
fn a_passing_attempt_advances_one_stage_and_dispatches_the_next() {
    let base = Snapshot::new(digest(1));
    let spec = draft(1, vec![membership("wp", 10)]).seal();
    let bloom = spec.id();
    let (snapshot, _) = step(&base, &event("seal", Fact::Seal(spec)));

    let pass = event(
        "c-pass",
        Fact::AttemptCompleted {
            bloom,
            workpiece: workpiece("wp"),
            stage: StageId::Construct,
            passed: true,
            evidence: attempt_evidence(),
            candidate: None,
        },
    );
    let (after, decided) = step(&snapshot, &pass);
    match decided.outcome {
        Outcome::AttemptAdvanced { from, to, .. } => {
            assert_eq!(from, StageId::Construct);
            assert_eq!(to, StageId::Verify);
        }
        other => panic!("expected AttemptAdvanced, got {other:?}"),
    }
    let dispatched: Vec<StageId> = decided
        .effects
        .iter()
        .filter_map(|effect| match effect {
            Decision::DispatchAttempt { stage, .. } => Some(*stage),
            _ => None,
        })
        .collect();
    assert_eq!(dispatched, vec![StageId::Verify], "exactly one dispatch, at the next stage");
    let progress = after.blooms.get(&bloom).unwrap().progress.get(&workpiece("wp")).unwrap();
    assert_eq!(progress.stage, StageId::Verify);
    assert_eq!(progress.attempts, 1);
}

// ADR-0149 §The line — Tripwire: a failing gate re-dispatches within the stage's
// retry budget, and an exhausted budget stops dispatching (wedges) rather than
// looping. `Construct`'s budget is 2, so the first fail retries in place to
// attempt 2 and the second wedges.
#[test]
fn a_failing_attempt_retries_within_budget_then_wedges() {
    let base = Snapshot::new(digest(1));
    let spec = draft(1, vec![membership("wp", 10)]).seal();
    let bloom = spec.id();
    let (snapshot, _) = step(&base, &event("seal", Fact::Seal(spec)));

    let fail = |key: &str| {
        event(
            key,
            Fact::AttemptCompleted {
                bloom,
                workpiece: workpiece("wp"),
                stage: StageId::Construct,
                passed: false,
                evidence: attempt_evidence(),
                candidate: None,
            },
        )
    };

    let (after1, d1) = step(&snapshot, &fail("c-fail-1"));
    match d1.outcome {
        Outcome::AttemptRetried { stage, attempt, .. } => {
            assert_eq!(stage, StageId::Construct);
            assert_eq!(attempt, 2);
        }
        other => panic!("expected AttemptRetried, got {other:?}"),
    }
    assert_eq!(d1.effects.iter().filter(|e| matches!(e, Decision::DispatchAttempt { .. })).count(), 1);
    assert_eq!(after1.blooms.get(&bloom).unwrap().progress.get(&workpiece("wp")).unwrap().attempts, 2);

    // Attempt 2 fails: the budget is exhausted, so the member wedges — no dispatch.
    let (after2, d2) = step(&after1, &fail("c-fail-2"));
    assert!(matches!(d2.outcome, Outcome::AttemptWedged { stage: StageId::Construct, .. }));
    assert!(
        !d2.effects.iter().any(|e| matches!(e, Decision::DispatchAttempt { .. })),
        "a wedged member stops dispatching",
    );

    // Tripwire: the wedge has to survive into the record, not just the outcome
    // this one call returns. Wedging is terminal — the member never dispatches
    // again — so a reader arriving later has nothing else to go on, and the
    // stage cursor cannot supply it: an exhausted member and one mid-flight on
    // its last roll carry the same cursor.
    let wedge = after2.blooms.get(&bloom).unwrap().wedged.get(&workpiece("wp")).expect("the wedge is recorded");
    assert_eq!(wedge.stage, StageId::Construct, "the recorded wedge names the stage that exhausted");
    assert_eq!(wedge.evidence, attempt_evidence().detail, "and the failure that spent the last of the budget");

    // A member that dispatches again is no longer wedged, and `AdvanceStage` is
    // the only route back into the line — so it is the only thing that clears
    // the set. Without this a superseded-then-revived member would carry a
    // stale wedge forever.
    let advance = Decisions {
        outcome: Outcome::AttemptRetried { bloom, workpiece: workpiece("wp"), stage: StageId::Construct, attempt: 1 },
        effects: vec![Decision::AdvanceStage {
            bloom,
            workpiece: workpiece("wp"),
            progress: StageProgress { stage: StageId::Construct, attempts: 1, candidate: None, repair_rolls: 0 },
        }],
    };
    let revived = after2.apply(&event("revive", fail("ignored").fact), &advance, &ResolvedConfigs::default());
    assert!(
        !revived.blooms.get(&bloom).unwrap().wedged.contains_key(&workpiece("wp")),
        "a cursor that moves clears the wedge",
    );
}

// ADR-0152 — a passing completion adopts the candidate it captured onto the
// member's cursor, and the next dispatch re-targets from it: the returned
// evidence binds the candidate tree (`inputs[0]`), the worker checks out the
// capture commit, and the payload displays the tree while still carrying the
// true scope revision. Catches the placeholder regression this arc removes —
// every stage dispatching against the bare sealed base.
#[test]
fn a_passing_capture_retargets_the_next_dispatch_at_the_candidate() {
    let base = Snapshot::new(digest(1));
    let spec = draft(1, vec![membership("wp", 10)]).seal();
    let bloom = spec.id();
    let (snapshot, _) = step(&base, &event("seal", Fact::Seal(spec)));

    let captured = CandidateRef { tree: digest(21), checkout: digest(22) };
    let pass = event(
        "c-pass",
        Fact::AttemptCompleted {
            bloom,
            workpiece: workpiece("wp"),
            stage: StageId::Construct,
            passed: true,
            evidence: attempt_evidence(),
            candidate: Some(captured),
        },
    );
    let (after, decided) = step(&snapshot, &pass);

    let progress = after.blooms.get(&bloom).unwrap().progress.get(&workpiece("wp")).unwrap();
    assert_eq!(progress.candidate, Some(captured), "the cursor adopts the passing capture");
    match decided.effects.iter().find(|e| matches!(e, Decision::DispatchAttempt { .. })) {
        Some(Decision::DispatchAttempt { transformation, scope_revision, candidate, .. }) => {
            assert_eq!(transformation.inputs[0], captured.tree, "evidence binds the candidate tree");
            assert_eq!(transformation.checkout, captured.checkout, "the worker checks out the capture commit");
            assert_eq!(*scope_revision, digest(10), "the payload still names the true scope revision");
            assert_eq!(*candidate, Some(captured.tree), "the payload displays the candidate tree");
        }
        other => panic!("expected a DispatchAttempt, got {other:?}"),
    }
}

// ADR-0152 — a failing attempt's capture is never adopted: the retry re-targets
// the candidate the member's last *pass* produced, and the cursor keeps it.
// Catches the inverted adoption rule (adopting whatever the fact carries), which
// would re-verify a tree that failed its own gate.
#[test]
fn a_failing_capture_is_discarded_and_the_retry_targets_the_prior_candidate() {
    let base = Snapshot::new(digest(1));
    let spec = draft(1, vec![membership("wp", 10)]).seal();
    let bloom = spec.id();
    let (snapshot, _) = step(&base, &event("seal", Fact::Seal(spec)));

    // Construct passes with a capture, then Verify fails with none (mechanical
    // lanes carry no capture) — the candidate rides the cursor into the Refine
    // repair re-entry (ADR-0153).
    let first = CandidateRef { tree: digest(21), checkout: digest(22) };
    let (snapshot, _) = step(
        &snapshot,
        &event(
            "c-pass",
            Fact::AttemptCompleted {
                bloom,
                workpiece: workpiece("wp"),
                stage: StageId::Construct,
                passed: true,
                evidence: attempt_evidence(),
                candidate: Some(first),
            },
        ),
    );
    let (snapshot, verify_fail) = step(
        &snapshot,
        &event(
            "v-fail",
            Fact::AttemptCompleted {
                bloom,
                workpiece: workpiece("wp"),
                stage: StageId::Verify,
                passed: false,
                evidence: attempt_evidence(),
                candidate: None,
            },
        ),
    );
    match verify_fail.effects.iter().find(|e| matches!(e, Decision::DispatchAttempt { .. })) {
        Some(Decision::DispatchAttempt { transformation, .. }) => {
            assert_eq!(transformation.checkout, first.checkout, "the re-entry carries the candidate forward");
        }
        other => panic!("expected a DispatchAttempt, got {other:?}"),
    }

    // Refine fails carrying a fresh capture: the capture is discarded — the
    // retry targets the candidate the last pass produced, and the cursor holds it.
    let rejected = CandidateRef { tree: digest(31), checkout: digest(32) };
    let (after, retried) = step(
        &snapshot,
        &event(
            "r-fail",
            Fact::AttemptCompleted {
                bloom,
                workpiece: workpiece("wp"),
                stage: StageId::Refine,
                passed: false,
                evidence: attempt_evidence(),
                candidate: Some(rejected),
            },
        ),
    );
    assert!(matches!(retried.outcome, Outcome::AttemptRetried { stage: StageId::Refine, .. }));
    match retried.effects.iter().find(|e| matches!(e, Decision::DispatchAttempt { .. })) {
        Some(Decision::DispatchAttempt { transformation, candidate, .. }) => {
            assert_eq!(transformation.inputs[0], first.tree, "the retry binds the prior candidate, not the failure's");
            assert_eq!(transformation.checkout, first.checkout);
            assert_eq!(*candidate, Some(first.tree));
        }
        other => panic!("expected a DispatchAttempt, got {other:?}"),
    }
    let progress = after.blooms.get(&bloom).unwrap().progress.get(&workpiece("wp")).unwrap();
    assert_eq!(progress.candidate, Some(first), "the cursor keeps the last passing candidate");
}

// ADR-0153 — Tripwire: the completion gate applies across the whole member
// line, the terminal `Verify` included. A *failing* `Verify` re-enters the
// repair-only `Refine` within Verify's budget (3) and wedges on exhaustion —
// it is never silently integrated; only a *passing* `Verify` leaves this path
// (through `Fact::Integrate`).
#[test]
fn a_failing_verify_reenters_refine_then_wedges_at_the_ceiling() {
    let base = Snapshot::new(digest(1));
    let spec = draft(1, vec![membership("wp", 10)]).seal();
    let bloom = spec.id();
    let (snapshot, _) = step(&base, &event("seal", Fact::Seal(spec)));

    let completion = |key: &str, stage: StageId, passed: bool| {
        event(
            key,
            Fact::AttemptCompleted {
                bloom,
                workpiece: workpiece("wp"),
                stage,
                passed,
                evidence: attempt_evidence(),
                candidate: None,
            },
        )
    };

    let (snapshot, _) = step(&snapshot, &completion("c-pass", StageId::Construct, true));
    assert_eq!(
        snapshot.blooms.get(&bloom).unwrap().progress.get(&workpiece("wp")).unwrap().stage,
        StageId::Verify,
        "the member advanced to the terminal Verify stage",
    );

    // The first failing Verify re-enters Refine (ADR-0153) — never a same-stage
    // Verify retry (re-running the mechanical gate on an unchanged candidate),
    // never an Integrate of the failing verdict.
    let (after1, d1) = step(&snapshot, &completion("v-fail-1", StageId::Verify, false));
    match d1.outcome {
        Outcome::RefineReentered { rolls, .. } => assert_eq!(rolls, 1),
        other => panic!("expected RefineReentered, got {other:?}"),
    }
    assert!(
        d1.effects.iter().any(|e| matches!(e, Decision::DispatchAttempt { stage: StageId::Refine, .. })),
        "a failing terminal Verify dispatches the Refine re-entry",
    );
    let progress = after1.blooms.get(&bloom).unwrap().progress.get(&workpiece("wp")).unwrap();
    assert_eq!(progress.stage, StageId::Refine);
    assert_eq!(progress.repair_rolls, 1, "the roll count survives the stage move");

    // The re-entered Refine passes — back to Verify for the delta-confirm, with
    // the roll count intact (the per-stage attempts reset must not clear it).
    let (after2, _) = step(&after1, &completion("refine-pass-1", StageId::Refine, true));
    let progress = after2.blooms.get(&bloom).unwrap().progress.get(&workpiece("wp")).unwrap();
    assert_eq!(progress.stage, StageId::Verify);
    assert_eq!(progress.repair_rolls, 1, "the delta-confirm carries the ceiling cursor");

    // A second failing verdict still re-enters (Verify's budget is 3), and the
    // repaired member returns for one more delta-confirm.
    let (after3, d2) = step(&after2, &completion("v-fail-2", StageId::Verify, false));
    assert!(matches!(d2.outcome, Outcome::RefineReentered { rolls: 2, .. }));
    let (after4, _) = step(&after3, &completion("refine-pass-2", StageId::Refine, true));

    // The third failing Verify verdict hits the ceiling: the member wedges — no
    // further roll, no re-entry, and no ResolutionClaim from the failing verdict.
    let (_after5, d3) = step(&after4, &completion("v-fail-3", StageId::Verify, false));
    assert!(matches!(d3.outcome, Outcome::AttemptWedged { stage: StageId::Verify, .. }));
    assert!(
        !d3.effects.iter().any(|e| matches!(e, Decision::DispatchAttempt { .. })),
        "a wedged terminal Verify stops dispatching",
    );
}

// #4708 — Tripwire: a grant resumes a wedged member on the bloom it already
// belongs to, and hands back exactly the attempts it names. The escape from a
// wedge used to be supersession alone, which mints a new bloom id and re-enters
// every non-inherited member at the entry stage — so an execution decision cost
// a fabricated content difference *and* the work the member had already done.
// `Construct`'s budget is 2, so a grant of 2 buys one retry and a second failure
// wedges again; anything that mis-derives the counter shows up as a member that
// re-wedges immediately or one that loops past its budget.
#[test]
fn a_grant_resumes_a_wedged_member_on_its_own_bloom() {
    let base = Snapshot::new(digest(1));
    let spec = draft(1, vec![membership("wp", 10)]).seal();
    let bloom = spec.id();
    let (snapshot, _) = step(&base, &event("seal", Fact::Seal(spec.clone())));

    let fail = |key: &str| {
        event(
            key,
            Fact::AttemptCompleted {
                bloom,
                workpiece: workpiece("wp"),
                stage: StageId::Construct,
                passed: false,
                evidence: attempt_evidence(),
                candidate: None,
            },
        )
    };
    let (snapshot, _) = step(&snapshot, &fail("c-fail-1"));
    let (wedged, _) = step(&snapshot, &fail("c-fail-2"));
    assert!(wedged.blooms.get(&bloom).unwrap().wedged.contains_key(&workpiece("wp")), "the member is wedged");

    let grant = event(
        "grant",
        Fact::GrantAttempts { bloom, workpiece: workpiece("wp"), stage: StageId::Construct, attempts: 2 },
    );
    let (granted, decisions) = step(&wedged, &grant);
    match &decisions.outcome {
        Outcome::AttemptsGranted { resumes_at, attempts, .. } => {
            assert_eq!(*resumes_at, StageId::Construct, "a non-Verify wedge resumes in place");
            assert_eq!(*attempts, 2);
        }
        other => panic!("expected AttemptsGranted, got {other:?}"),
    }
    assert_eq!(
        decisions.effects.iter().filter(|e| matches!(e, Decision::DispatchAttempt { .. })).count(),
        1,
        "the grant dispatches the resumed attempt itself",
    );
    let record = granted.blooms.get(&bloom).unwrap();
    assert!(!record.wedged.contains_key(&workpiece("wp")), "a cursor that moves clears the wedge");
    assert_eq!(record.spec, spec, "the grant alters no field of the sealed spec");
    assert_eq!(granted.blooms.len(), 1, "and seals no successor");

    // Tripwire: the grant's arithmetic. Two attempts means one retry and then a
    // wedge — not a member that re-wedges on its first failure (headroom
    // mis-derived) and not one that outlives its stage budget.
    let (retried, d1) = step(&granted, &fail("c-fail-3"));
    assert!(matches!(d1.outcome, Outcome::AttemptRetried { attempt: 2, .. }), "the first granted failure retries");
    let (_, d2) = step(&retried, &fail("c-fail-4"));
    assert!(matches!(d2.outcome, Outcome::AttemptWedged { .. }), "the second spends the grant and wedges again");
}

// #4708 — Tripwire: a `Verify` wedge is spent *repair rolls*, not spent
// attempts, so a grant there resumes the member at `Refine` — the re-entry the
// wedge denied — carrying the candidate it had already built. A grant that only
// reset `attempts` would leave a Verify-wedged member exactly as stuck (the
// cursor-carried `repair_rolls` is what wedged it), and one that resumed at
// `Verify` would re-run the mechanical gate on an unchanged candidate, whose
// verdict cannot change (ADR-0153). That is the most common wedge there is.
#[test]
fn a_grant_on_a_verify_wedge_resumes_at_refine_with_its_candidate() {
    let base = Snapshot::new(digest(1));
    let spec = draft(1, vec![membership("wp", 10)]).seal();
    let bloom = spec.id();
    let (snapshot, _) = step(&base, &event("seal", Fact::Seal(spec)));

    let completion = |key: &str, stage: StageId, passed: bool, candidate: Option<CandidateRef>| {
        event(
            key,
            Fact::AttemptCompleted {
                bloom,
                workpiece: workpiece("wp"),
                stage,
                passed,
                evidence: attempt_evidence(),
                candidate,
            },
        )
    };

    // Construct passes with a capture, then Verify spends its whole repair
    // ceiling (budget 3) and the member wedges holding that candidate.
    let captured = CandidateRef { tree: digest(21), checkout: digest(22) };
    let (snapshot, _) = step(&snapshot, &completion("c-pass", StageId::Construct, true, Some(captured)));
    let (snapshot, _) = step(&snapshot, &completion("v-fail-1", StageId::Verify, false, None));
    let (snapshot, _) = step(&snapshot, &completion("refine-pass-1", StageId::Refine, true, None));
    let (snapshot, _) = step(&snapshot, &completion("v-fail-2", StageId::Verify, false, None));
    let (snapshot, _) = step(&snapshot, &completion("refine-pass-2", StageId::Refine, true, None));
    let (wedged, d) = step(&snapshot, &completion("v-fail-3", StageId::Verify, false, None));
    assert!(matches!(d.outcome, Outcome::AttemptWedged { stage: StageId::Verify, .. }));

    let grant =
        event("grant", Fact::GrantAttempts { bloom, workpiece: workpiece("wp"), stage: StageId::Verify, attempts: 1 });
    let (granted, decisions) = step(&wedged, &grant);
    assert!(
        matches!(&decisions.outcome, Outcome::AttemptsGranted { resumes_at: StageId::Refine, attempts: 1, .. }),
        "a Verify wedge resumes at the Refine re-entry, got {:?}",
        decisions.outcome,
    );
    match decisions.effects.iter().find(|e| matches!(e, Decision::DispatchAttempt { .. })) {
        Some(Decision::DispatchAttempt { stage, transformation, candidate, .. }) => {
            assert_eq!(*stage, StageId::Refine);
            assert_eq!(*candidate, Some(captured.tree), "the resumed attempt keeps the candidate it had built");
            assert_eq!(
                transformation.inputs.first().copied(),
                Some(captured.tree),
                "and binds its evidence to that candidate, not the sealed base",
            );
        }
        other => panic!("expected a Refine DispatchAttempt, got {other:?}"),
    }

    // A grant of 1 buys exactly one repair cycle: the re-entered Refine passes,
    // the delta-confirm fails, and the member wedges again rather than looping.
    let progress = granted.blooms.get(&bloom).unwrap().progress.get(&workpiece("wp")).unwrap();
    assert_eq!(progress.stage, StageId::Refine);
    let (snapshot, _) = step(&granted, &completion("refine-pass-3", StageId::Refine, true, None));
    let (_, d) = step(&snapshot, &completion("v-fail-4", StageId::Verify, false, None));
    assert!(matches!(d.outcome, Outcome::AttemptWedged { .. }), "one granted roll, then wedged again");
}

// #4708 — the grant's refusals. A running member is not grantable (two workers
// on one workpiece), a stale stage name is not silently applied to whatever the
// record says now, and a request the member could never spend is refused naming
// the ceiling rather than quietly clamped — a clamp would report a grant of five
// while handing back two.
#[test]
fn grant_refuses_a_running_member_a_stale_stage_and_an_unspendable_request() {
    let base = Snapshot::new(digest(1));
    let spec = draft(1, vec![membership("wp", 10)]).seal();
    let bloom = spec.id();
    let (running, _) = step(&base, &event("seal", Fact::Seal(spec)));

    let grant = |key: &str, wp: &str, stage: StageId, attempts: u32| {
        event(key, Fact::GrantAttempts { bloom, workpiece: workpiece(wp), stage, attempts })
    };

    let (_, d) = step(&running, &grant("g-running", "wp", StageId::Construct, 1));
    assert!(
        matches!(&d.outcome, Outcome::GrantAttemptsRejected(GrantAttemptsError::NotWedged(wp)) if *wp == workpiece("wp")),
        "a member mid-flight has attempts already, got {:?}",
        d.outcome,
    );

    let fail = |key: &str| {
        event(
            key,
            Fact::AttemptCompleted {
                bloom,
                workpiece: workpiece("wp"),
                stage: StageId::Construct,
                passed: false,
                evidence: attempt_evidence(),
                candidate: None,
            },
        )
    };
    let (snapshot, _) = step(&running, &fail("c-fail-1"));
    let (wedged, _) = step(&snapshot, &fail("c-fail-2"));

    let (_, d) = step(&wedged, &grant("g-stale", "wp", StageId::Verify, 1));
    assert!(
        matches!(
            d.outcome,
            Outcome::GrantAttemptsRejected(GrantAttemptsError::StageMismatch {
                wedged_at: StageId::Construct,
                got: StageId::Verify,
            })
        ),
        "a grant naming the wrong stage is refused, got {:?}",
        d.outcome,
    );

    // `Construct`'s retry budget is 2, and no `retry_cap` is sealed, so 2 is the
    // ceiling. Zero is refused on the same door: it would dispatch an attempt
    // while granting nothing to spend on it.
    for (key, attempts) in [("g-over", 3), ("g-zero", 0)] {
        let (_, d) = step(&wedged, &grant(key, "wp", StageId::Construct, attempts));
        assert!(
            matches!(
                d.outcome,
                Outcome::GrantAttemptsRejected(GrantAttemptsError::BeyondCap { requested, cap: 2 })
                    if requested == attempts
            ),
            "a grant of {attempts} is refused naming the ceiling, got {:?}",
            d.outcome,
        );
    }

    let (_, d) = step(&wedged, &grant("g-stranger", "nobody", StageId::Construct, 1));
    assert!(
        matches!(&d.outcome, Outcome::GrantAttemptsRejected(GrantAttemptsError::NotAMember(wp)) if *wp == workpiece("nobody")),
        "a grant for a non-member is refused, got {:?}",
        d.outcome,
    );
}

// ADR-0149 §The line — an attempt completion is refused when it does not name the
// member's current cursor stage, when it names the terminal `Verify` with a
// *passing* verdict (which integrates through `Fact::Integrate`, never completes
// here), for a non-member, and for an unknown bloom.
#[test]
fn attempt_completion_refuses_mismatch_terminal_non_member_and_unknown() {
    let base = Snapshot::new(digest(1));
    let spec = draft(1, vec![membership("wp", 10)]).seal();
    let bloom = spec.id();
    let (snapshot, _) = step(&base, &event("seal", Fact::Seal(spec)));

    let completion = |key: &str, wp: &str, stage: StageId| {
        event(
            key,
            Fact::AttemptCompleted {
                bloom,
                workpiece: workpiece(wp),
                stage,
                passed: true,
                evidence: attempt_evidence(),
                candidate: None,
            },
        )
    };

    // The cursor is at Construct; a *failing* completion naming Verify is a
    // stage mismatch (the passing case is the terminal mis-route below, which
    // is caught first).
    let mismatch = reduce(
        &snapshot,
        &event(
            "m",
            Fact::AttemptCompleted {
                bloom,
                workpiece: workpiece("wp"),
                stage: StageId::Verify,
                passed: false,
                evidence: attempt_evidence(),
                candidate: None,
            },
        ),
        &ResolvedConfigs::default(),
    );
    assert!(matches!(
        mismatch.outcome,
        Outcome::AttemptCompletedRejected(AttemptCompletedError::StageMismatch {
            expected: StageId::Construct,
            got: StageId::Verify,
        }),
    ));

    // A passing terminal Verify never completes here — it integrates through
    // Fact::Integrate.
    let terminal = reduce(&snapshot, &completion("t", "wp", StageId::Verify), &ResolvedConfigs::default());
    assert!(matches!(
        terminal.outcome,
        Outcome::AttemptCompletedRejected(AttemptCompletedError::TerminalStage(StageId::Verify)),
    ));

    // A passing Review is off the dispatched line entirely (ADR-0153) and reads
    // as the same terminal mis-route.
    let off_line = reduce(&snapshot, &completion("r", "wp", StageId::Review), &ResolvedConfigs::default());
    assert!(matches!(
        off_line.outcome,
        Outcome::AttemptCompletedRejected(AttemptCompletedError::TerminalStage(StageId::Review)),
    ));

    // A non-member workpiece.
    let stranger = reduce(&snapshot, &completion("n", "ghost", StageId::Construct), &ResolvedConfigs::default());
    assert!(matches!(stranger.outcome, Outcome::AttemptCompletedRejected(AttemptCompletedError::NotAMember(_))));

    // An unknown bloom (nothing sealed on `base`).
    let unknown = reduce(&base, &completion("u", "wp", StageId::Construct), &ResolvedConfigs::default());
    assert!(matches!(
        unknown.outcome,
        Outcome::AttemptCompletedRejected(AttemptCompletedError::UnknownOrInactiveBloom),
    ));
}

proptest! {
    // ADR-0153 — the per-member cursor only ever moves along the allowed repair
    // graph: Construct advances to Verify on a pass and holds on a fail; a
    // failing Verify re-enters Refine (or holds, wedged, at the ceiling); a
    // passing Refine returns to Verify and a failing one holds; a passing
    // Verify never completes here (the terminal mis-route is rejected with the
    // cursor untouched). Driving a random pass/fail sequence through
    // reduce+apply is journal replay, so this also pins that replay
    // reconstructs in-flight line position.
    #[test]
    fn cursor_moves_only_along_the_repair_graph(passes in prop::collection::vec(any::<bool>(), 0..8)) {
        let base = Snapshot::new(digest(1));
        let spec = draft(1, vec![membership("wp", 10)]).seal();
        let bloom = spec.id();
        let (mut snapshot, _) = step(&base, &event("seal", Fact::Seal(spec)));

        for (i, passed) in passes.into_iter().enumerate() {
            let cursor = snapshot.blooms.get(&bloom).unwrap().progress.get(&workpiece("wp")).copied().unwrap();
            let ev = event(
                &format!("a-{i}"),
                Fact::AttemptCompleted {
                    bloom,
                    workpiece: workpiece("wp"),
                    stage: cursor.stage,
                    passed,
                    evidence: attempt_evidence(),
                    candidate: None,
                },
            );
            let (next, decided) = step(&snapshot, &ev);
            snapshot = next;

            let new = snapshot.blooms.get(&bloom).unwrap().progress.get(&workpiece("wp")).copied().unwrap();
            match decided.outcome {
                Outcome::AttemptAdvanced { from, to, .. } => {
                    prop_assert_eq!(from, cursor.stage);
                    prop_assert_eq!(to, new.stage);
                    // The only passing advances are Construct → Verify and the
                    // repair Refine → Verify delta-confirm.
                    prop_assert!(matches!((from, to), (StageId::Construct | StageId::Refine, StageId::Verify)));
                }
                Outcome::RefineReentered { .. } => {
                    prop_assert_eq!(cursor.stage, StageId::Verify, "only a failing Verify re-enters");
                    prop_assert_eq!(new.stage, StageId::Refine);
                }
                Outcome::AttemptRetried { .. } | Outcome::AttemptWedged { .. } => {
                    prop_assert_eq!(new.stage, cursor.stage, "a retry or wedge holds the cursor in place");
                }
                Outcome::AttemptCompletedRejected(AttemptCompletedError::TerminalStage(_)) => {
                    prop_assert!(passed && cursor.stage == StageId::Verify);
                    prop_assert_eq!(new, cursor, "a rejected terminal mis-route leaves the cursor untouched");
                }
                other => return Err(TestCaseError::fail(format!("unexpected outcome {other:?}"))),
            }
        }
    }

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

// ADR-0152 — only the claim that completes the set dispatches integration, and
// its candidate list is every member's claimed candidate in member order.
// Catches both failure shapes: dispatching on every integrate (N redundant
// folds), and never dispatching (resolutions that never reach the git side).
#[test]
fn the_completing_integrate_dispatches_the_integration_fold_in_member_order() {
    let base = Snapshot::new(digest(1));
    let spec = draft(1, vec![membership("alpha", 10), membership("beta", 11)]).seal();
    let bloom = spec.id();
    let spec_base = spec.base();
    let (snapshot, _) = step(&base, &event("seal", Fact::Seal(spec)));

    let (snapshot, first) = step(&snapshot, &event("i-a", Fact::Integrate { bloom, claim: claim("alpha", 10, 21) }));
    assert!(matches!(first.outcome, Outcome::Integrated { .. }));
    assert!(
        !first.effects.iter().any(|e| matches!(e, Decision::DispatchIntegration { .. })),
        "a partial claim set dispatches no integration",
    );

    let (_, second) = step(&snapshot, &event("i-b", Fact::Integrate { bloom, claim: claim("beta", 11, 22) }));
    match second.effects.iter().find(|e| matches!(e, Decision::DispatchIntegration { .. })) {
        Some(Decision::DispatchIntegration { base, members, .. }) => {
            assert_eq!(*base, spec_base, "the fold bootstraps at the sealed base");
            let folded: Vec<(&str, Digest)> =
                members.iter().map(|member| (member.workpiece.0.as_str(), member.candidate)).collect();
            assert_eq!(
                folded,
                vec![("alpha", digest(21)), ("beta", digest(22))],
                "every member's workpiece and candidate, in member order — the workpiece addresses the \
                 candidate ref a combining fold merges",
            );
        }
        other => panic!("expected a DispatchIntegration, got {other:?}"),
    }
}

// The sealed configuration registry (ADR-0174). A test kind stands in for a
// real configuration; what matters is that the seal covers the registry at both
// scopes, which is what makes a receipt's configuration claim mean anything.
mod sealed_config {
    use aether_bloomery::{
        BloomDraft, ConfigKind, ConfigRegistry, Event, Fact, IdempotencyKey, Membership, Outcome, ResolvedConfigs,
        SealError, Snapshot, Unproducible, reduce,
    };
    use aether_data::Kind;
    use aether_data::wire::to_vec;
    use serde::{Deserialize, Serialize};

    use crate::common::{approved, digest, membership};

    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[kind(name = "aether.bloomery.test_seal_config")]
    struct LaneConfig {
        lane: String,
    }

    fn sealing(lane: &str) -> ConfigRegistry {
        let mut registry = ConfigRegistry::default();
        registry.insert::<LaneConfig>(LaneConfig { lane: lane.to_owned() }.address());
        registry
    }

    /// Content for every lane this module seals, so a seal under test refuses on
    /// the property it is about rather than on content nobody supplied.
    fn lane_content() -> ResolvedConfigs {
        let mut configs = ResolvedConfigs::default();
        for lane in ["cheap", "expensive"] {
            let value = LaneConfig { lane: lane.to_owned() };
            configs.insert(value.address(), LaneConfig::NAME, to_vec(&value).expect("test value encodes"));
        }
        configs
    }

    fn sealed(member: Membership) -> Outcome {
        sealed_given(member, &lane_content())
    }

    fn sealed_given(member: Membership, configs: &ResolvedConfigs) -> Outcome {
        reduce(
            &Snapshot::new(digest(0)),
            &Event {
                idempotency_key: IdempotencyKey("seal".to_owned()),
                fact: Fact::Seal(
                    BloomDraft { proposals: vec![member], base: digest(1), ..BloomDraft::default() }.seal(),
                ),
            },
            configs,
        )
        .outcome
    }

    // Tripwire: a member's approval binds its configuration, not just its scope
    // revision (ADR-0174). Moving the model override out of the scope revision
    // into the registry would otherwise drop model choice out from under the
    // approval — an operator could swap the model on an approved member and the
    // receipt would still read "approved". Re-configuring after approval must
    // refuse the seal until the member is re-approved.
    #[test]
    fn re_configuring_an_approved_member_invalidates_its_approval() {
        let member = membership("wp-a", 1);
        assert!(matches!(sealed(member.clone()), Outcome::Sealed(_)), "the approved member seals");

        let mut reconfigured = member;
        reconfigured.configs = sealing("cheap");
        assert!(
            matches!(sealed(reconfigured.clone()), Outcome::SealRejected(SealError::UnapprovedMember(_))),
            "configuring after approval refuses until re-approved",
        );

        assert!(matches!(sealed(approved(reconfigured)), Outcome::Sealed(_)), "re-approval admits it");
    }

    // Tripwire: a seal whose registry names content the reducer was not given is
    // refused at the door, not admitted to fail later. A sealed address is
    // immutable, so content that cannot be produced now never appears — admitting
    // would claim the members and block the mainline on a bloom whose every
    // dispatch parks. The refusal names the kind, so an operator sees which
    // configuration went missing rather than a bare rejection.
    #[test]
    fn a_seal_naming_configuration_the_reducer_lacks_is_refused() {
        let mut member = membership("wp-a", 1);
        member.configs = sealing("cheap");
        let member = approved(member);

        let refused = sealed_given(member.clone(), &ResolvedConfigs::default());
        assert!(
            matches!(
                refused,
                Outcome::SealRejected(SealError::UnproducibleConfig { ref kind, reason: Unproducible::Absent, .. })
                    if kind == LaneConfig::NAME
            ),
            "a sealed address with no content refuses the seal, naming its kind: {refused:?}",
        );

        assert!(
            matches!(sealed_given(member, &lane_content()), Outcome::Sealed(_)),
            "the same seal is admitted once the content is available",
        );
    }

    fn draft_with(bloom: ConfigRegistry, member: ConfigRegistry) -> BloomDraft {
        let mut proposal = membership("wp-a", 1);
        proposal.configs = member;
        BloomDraft { proposals: vec![proposal], base: digest(1), configs: bloom, ..BloomDraft::default() }
    }

    // Tripwire: the bloom id covers the configuration sealed at both scopes. A
    // registry the id did not cover would let two blooms with different
    // configurations share an identity, so a receipt naming one could not
    // distinguish which actually ran — the attestation the registry replaces
    // bespoke fields to provide.
    #[test]
    fn the_seal_covers_the_configuration_at_both_scopes() {
        let bare = draft_with(ConfigRegistry::default(), ConfigRegistry::default()).seal().id();

        let bloom_scoped = draft_with(sealing("cheap"), ConfigRegistry::default()).seal().id();
        assert_ne!(bare, bloom_scoped, "a bloom-scoped entry moves the id");

        let member_scoped = draft_with(ConfigRegistry::default(), sealing("cheap")).seal().id();
        assert_ne!(bare, member_scoped, "a member-scoped entry moves the id");
        assert_ne!(bloom_scoped, member_scoped, "the same entry at a different scope is a different bloom");

        let other_value = draft_with(sealing("expensive"), ConfigRegistry::default()).seal().id();
        assert_ne!(bloom_scoped, other_value, "changing the sealed content moves the id");
    }
}

/// The sealed stage catalog is what the bloom runs — the point of #4587.
mod sealed_catalog {
    use aether_bloomery::{
        Decision, Evidence, EvidenceKind, Fact, Harness, Outcome, ReasoningEffort, StageCatalog, StageId, ToolPolicy,
        reduce,
    };

    use crate::common::{
        approved, digest, draft_with_catalog, draft_with_member_override, event, membership, workpiece,
    };
    use aether_bloomery::{ModelOverride, OverrideError, SealError, Snapshot, StageOverride};

    use std::collections::BTreeMap;

    /// The compiled line with `Construct` recalibrated onto a distinct agent — the
    /// "cheap harness for construct, expensive for review" the issue names.
    fn recalibrated_construct() -> StageCatalog {
        let mut catalog = StageCatalog::line();
        for binding in &mut catalog.bindings {
            if binding.stage == StageId::Construct {
                binding.profile.harness = Harness::Claude;
                binding.profile.model = String::from("claude-opus-5");
                binding.profile.effort = ReasoningEffort::Low;
                binding.profile.tools = ToolPolicy::ReadOnly;
            }
        }
        catalog
    }

    // Tripwire: a member's per-stage override is refused at the door when the
    // sealed catalog runs no model at that stage (#4601). The operator authored a
    // sentence about which model runs where; an entry nothing resolves would let
    // them seal it, watch the calibrated default run, and read a receipt that
    // mentions neither. Verify is the case that matters — it is a real stage with
    // a real binding, so only reading the binding's *process* catches it.
    #[test]
    fn an_override_keyed_to_a_stage_running_no_model_is_refused_at_seal() {
        let escalate = |stage| ModelOverride {
            per_stage: BTreeMap::from([(
                stage,
                StageOverride { agent: None, reasoning_effort: Some(ReasoningEffort::Max) },
            )]),
            ..ModelOverride::default()
        };

        let (draft, configs) = draft_with_member_override(1, membership("wp", 10), &escalate(StageId::Verify));
        let decided = reduce(&Snapshot::new(digest(1)), &event("seal", Fact::Seal(draft.seal())), &configs);
        assert_eq!(
            decided.outcome,
            Outcome::SealRejected(SealError::UnusableModelOverride {
                workpiece: workpiece("wp"),
                error: OverrideError::StageRunsNoModel(StageId::Verify),
            }),
            "a stage the line runs mechanically cannot carry a model choice"
        );
        assert!(decided.effects.is_empty(), "a refused seal claims nothing");

        // The control: the same override keyed to a model lane seals. Without it
        // the test above would pass on a door that refused every override.
        let (draft, configs) = draft_with_member_override(1, membership("wp", 10), &escalate(StageId::Refine));
        let decided = reduce(&Snapshot::new(digest(1)), &event("seal", Fact::Seal(draft.seal())), &configs);
        assert!(matches!(decided.outcome, Outcome::Sealed(_)), "a model lane admits one: {:?}", decided.outcome);
    }

    // Tripwire: the entry dispatch carries the profile the *sealed* catalog names,
    // not the compiled line's. Without this the operator authors a catalog, the
    // receipt attests it, and the lane runs whatever the fleet was calibrated to —
    // the divergence #4324 and #4327 closed for the model and the harness, one
    // layer down. Asserted against the authored values directly, because pinning
    // it against `profile_of` would pass even if the sealed catalog were ignored.
    #[test]
    fn the_dispatched_profile_comes_from_the_sealed_catalog() {
        let catalog = recalibrated_construct();
        let (draft, configs) = draft_with_catalog(1, vec![approved(membership("wp", 10))], &catalog);

        let decided = reduce(&Snapshot::new(digest(1)), &event("seal", Fact::Seal(draft.seal())), &configs);
        assert!(matches!(decided.outcome, Outcome::Sealed(_)), "the authored catalog seals: {:?}", decided.outcome);

        let dispatched = decided
            .effects
            .iter()
            .find_map(|effect| match effect {
                Decision::DispatchAttempt { profile, stage: StageId::Construct, .. } => Some(profile),
                _ => None,
            })
            .expect("sealing dispatches the entry stage");

        let authored = catalog.profile_for(StageId::Construct).expect("the catalog binds Construct");
        assert_eq!(dispatched, authored, "the dispatch runs the agent the sealed catalog names");
        assert_ne!(
            dispatched,
            &StageCatalog::profile_of(StageId::Construct),
            "and that is not the compiled line's calibration, or the assertion above proves nothing",
        );
    }

    // Tripwire: the reducer counts to the *sealed* catalog's retry budget. This is
    // the read that cannot live behind a host-side resolve — it decides re-dispatch
    // versus wedge, inside `reduce`. A bloom sealing a budget of 1 must wedge on
    // its first failure even though the compiled line allows 2, or the receipt
    // attests a retry policy the reducer never applied.
    #[test]
    fn the_reducer_counts_to_the_sealed_catalogs_retry_budget() {
        let mut catalog = StageCatalog::line();
        for binding in &mut catalog.bindings {
            if binding.stage == StageId::Construct {
                binding.retry_budget = 1;
            }
        }
        assert_eq!(
            StageCatalog::line().retry_budget_of(StageId::Construct),
            Some(2),
            "the compiled line allows a second attempt, so wedging on the first is the sealed budget's doing",
        );

        let (draft, configs) = draft_with_catalog(1, vec![approved(membership("wp", 10))], &catalog);
        let spec = draft.seal();
        let bloom = spec.id();
        let seal = event("seal", Fact::Seal(spec));
        let snapshot =
            Snapshot::new(digest(1)).apply(&seal, &reduce(&Snapshot::new(digest(1)), &seal, &configs), &configs);

        let failed = event(
            "fail-1",
            Fact::AttemptCompleted {
                bloom,
                workpiece: workpiece("wp"),
                stage: StageId::Construct,
                passed: false,
                evidence: Evidence { subject: digest(70), kind: EvidenceKind::VerificationResult, detail: digest(80) },
                candidate: None,
            },
        );
        let decided = reduce(&snapshot, &failed, &configs);
        assert!(
            matches!(decided.outcome, Outcome::AttemptWedged { stage: StageId::Construct, .. }),
            "the sealed budget of 1 wedges on the first failure: {:?}",
            decided.outcome,
        );
    }
}

/// The mainline-observation rule (#4667): the coordinator's mainline pointer
/// follows the repository, and is held while a bloom is in flight.
mod observed_mainline {
    use super::*;

    // Tripwire: mainline follows the repository, not only the coordinator's own
    // lands (#4667). An observation of a head mainline has not reached must move
    // it, or every bloom sealed afterwards bases on a head the repository left —
    // the drift this fact exists to close.
    #[test]
    fn an_observed_head_advances_mainline() {
        let snapshot = Snapshot::new(digest(1));
        let observed = event("observe", Fact::ObserveMainline { head: digest(9) });

        let (next, decided) = step(&snapshot, &observed);

        assert!(
            matches!(decided.outcome, Outcome::MainlineAdvanced { from, to } if from == digest(1) && to == digest(9)),
            "an observation past mainline advances it: {:?}",
            decided.outcome,
        );
        assert_eq!(next.mainline, digest(9), "and the fold moves the pointer the decision named");
    }

    // Tripwire: re-observing the head mainline already sits at moves nothing. A
    // host that observes on a cadence submits this fact constantly, so the
    // steady state must decide no mainline move — an `AdvanceMainline` here
    // would churn the outbox on every poll.
    #[test]
    fn re_observing_the_current_head_moves_nothing() {
        let snapshot = Snapshot::new(digest(1));
        let observed = event("observe", Fact::ObserveMainline { head: digest(1) });

        let (next, decided) = step(&snapshot, &observed);

        assert!(
            matches!(decided.outcome, Outcome::MainlineUnchanged(head) if head == digest(1)),
            "observing the current head is a no-op: {:?}",
            decided.outcome,
        );
        assert!(
            !decided.effects.iter().any(|e| matches!(e, Decision::AdvanceMainline { .. })),
            "and moves mainline nowhere: {:?}",
            decided.effects,
        );
        assert_eq!(next.mainline, digest(1), "mainline stays put");
    }

    // Tripwire: an observation the advance cannot follow is still recorded
    // (#4709). The recorded head is the only base a supersession may rebase
    // onto, so dropping it is what leaves a wedged bloom pinning mainline with
    // no way back — a wedge never leaves flight on its own, so "the next
    // observation will advance it" is false for exactly the case that needs it.
    #[test]
    fn an_observation_held_by_a_bloom_in_flight_is_still_recorded() {
        let (snapshot, spec) = sealed_and_resolved(1, vec![membership("wp", 10)], 30);
        let observed = event("observe", Fact::ObserveMainline { head: digest(9) });

        let (next, decided) = step(&snapshot, &observed);

        assert!(
            matches!(decided.outcome, Outcome::MainlineHeld { head, by } if head == digest(9) && by == spec.id()),
            "the in-flight bloom holds the advance: {:?}",
            decided.outcome,
        );
        assert_eq!(next.mainline, digest(1), "mainline stays where the in-flight bloom sealed against");
        assert_eq!(next.observed, digest(9), "but the repository's head is recorded for a supersession to rebase onto");
    }

    // Tripwire: a land leaves the observed head no staler than mainline. A land
    // authors a head the source has not reported yet, so recording only
    // observations would leave `observed` pointing behind mainline — and a
    // supersession rebasing onto it would walk the compare-and-swap anchor
    // backwards onto a head the repository has already left.
    #[test]
    fn a_land_carries_the_observed_head_forward_with_mainline() {
        let (snapshot, spec) = sealed_and_resolved(1, vec![membership("wp", 10)], 30);
        let landed = event("land", Fact::Land { bloom: spec.id(), new_head: digest(9) });

        let (next, _) = step(&snapshot, &landed);

        assert_eq!(next.mainline, digest(9), "the land advanced mainline onto the head it authored");
        assert_eq!(next.observed, digest(9), "and the observed head followed it rather than trailing behind");
    }
}

/// The mainline-resync rule (#4709): a supersession that rebases onto the
/// observed head takes mainline with it, which is the only way a wedged bloom
/// ever stops pinning it.
mod mainline_resync {
    use super::*;

    // Tripwire: superseding onto the observed head advances mainline. Mainline
    // may not move while a bloom is in flight and a wedged bloom never leaves
    // flight, so without this the coordinator can never catch up to a repository
    // that moved — every successor inherits a base whose land is refused by both
    // the reducer's `BaseMismatch` and the git compare-and-swap.
    #[test]
    fn superseding_onto_the_observed_head_advances_mainline() {
        let (snapshot, predecessor_spec) = sealed_and_resolved(1, vec![membership("wp", 10)], 30);
        let (snapshot, _) = step(&snapshot, &event("observe", Fact::ObserveMainline { head: digest(2) }));

        let successor_spec = draft(2, vec![membership("wp", 10)]).seal();
        let superseded =
            event("sup", Fact::Supersede { predecessor: predecessor_spec.id(), successor: successor_spec.clone() });
        let (next, decided) = step(&snapshot, &superseded);

        assert!(
            matches!(decided.outcome, Outcome::Superseded { successor, .. } if successor == successor_spec.id()),
            "the supersession is admitted: {:?}",
            decided.outcome,
        );
        assert_eq!(next.mainline, digest(2), "and mainline followed the successor onto the observed head");
    }

    // Tripwire: a successor may not name a base nobody observed. A supersession
    // moves mainline, and mainline is the compare-and-swap anchor a land is
    // judged against — so accepting an arbitrary caller-supplied base would let
    // the route write that anchor directly and land a bloom onto a head that was
    // never in the repository.
    #[test]
    fn superseding_onto_an_unobserved_base_is_refused() {
        let (snapshot, predecessor_spec) = sealed_and_resolved(1, vec![membership("wp", 10)], 30);
        let (snapshot, _) = step(&snapshot, &event("observe", Fact::ObserveMainline { head: digest(2) }));

        // Base 7 is neither current mainline (1) nor the observed head (2).
        let successor_spec = draft(7, vec![membership("wp", 10)]).seal();
        let superseded =
            event("sup", Fact::Supersede { predecessor: predecessor_spec.id(), successor: successor_spec });
        let (next, decided) = step(&snapshot, &superseded);

        assert!(
            matches!(
                decided.outcome,
                Outcome::SupersedeRejected(SupersedeError::UnobservedBase { base, observed })
                    if base == digest(7) && observed == digest(2)
            ),
            "an unobserved base is refused, naming both: {:?}",
            decided.outcome,
        );
        assert_eq!(next.mainline, digest(1), "and mainline is untouched by the refusal");
    }

    // Tripwire: a supersession that keeps the base leaves mainline alone. The
    // ordinary re-run — same base, fresh attempt — must not be a mainline event,
    // or every wedge repair would churn the pointer a land compare-and-swaps
    // against.
    #[test]
    fn superseding_on_the_same_base_leaves_mainline_alone() {
        let (snapshot, predecessor_spec) = sealed_and_resolved(1, vec![membership("wp", 10)], 30);
        let (snapshot, _) = step(&snapshot, &event("observe", Fact::ObserveMainline { head: digest(2) }));

        let successor_spec = draft(1, vec![membership("wp", 11)]).seal();
        let superseded =
            event("sup", Fact::Supersede { predecessor: predecessor_spec.id(), successor: successor_spec });
        let (next, decided) = step(&snapshot, &superseded);

        assert!(
            !decided.effects.iter().any(|e| matches!(e, Decision::AdvanceMainline { .. })),
            "a same-base supersession decides no mainline move: {:?}",
            decided.effects,
        );
        assert_eq!(next.mainline, digest(1), "and mainline stays where it was");
    }
}
