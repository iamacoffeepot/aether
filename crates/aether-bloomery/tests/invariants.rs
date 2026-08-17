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
    Adjudication, AdjudicationError, AdmitEvidenceError, AdoptAnswerError, AggregateReviewError, AggregateVerifyError,
    Artifact, AttemptCompletedError, BloomId, BloomStatus, CandidateRef, CatalogError, ClaimRefKind, ConfigRegistry,
    Decision, Digest, DispatchKey, Disposition, Event, Evidence, EvidenceKind, Fact, GrantAttemptsError,
    HostFaultError, KeyId, LandError, LandingRejectedError, Membership, ORPHAN_CLAIM_RELEASE_WORDS, Observation,
    OperatorHold, OperatorHoldError, OperatorRepair, OperatorRepairError, OrphanClaimRelease,
    OrphanClaimReleaseCompletion, OrphanClaimReleaseError, Outcome, Provenance, Question, ResolutionClaim,
    ResolveError, ResolvedConfigs, SealError, SignatureEnvelope, Snapshot, SpendWindow, StageCatalog, StageId,
    StageProgress, Statement, SupersedeError, Unproducible, VerifyFailedError, VerifyFailure, VerifyFailureSet, grade,
    reduce,
};
use aether_bloomery::{BloomRecord, WorkpieceId};
use aether_data::Kind;
use aether_data::wire::to_vec;
use common::{
    claim, digest, draft, draft_with_catalog, event, membership, observing, sealed_and_resolved, splice_bloom, step,
    workpiece,
};
use proptest::collection::btree_set;
use proptest::prelude::*;
use std::collections::{BTreeMap, BTreeSet};

/// A set of distinct memberships named by their (distinct) revision seeds, so
/// order-sensitivity is actually exercised.
fn distinct_members() -> impl Strategy<Value = Vec<Membership>> {
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
        let early = reduce(&after_seal, &event("r", Fact::Resolve { bloom: bloom2, tree: digest(40), head: digest(41), lineage: vec![] }), &ResolvedConfigs::default(), &SpendWindow::default());
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
        let stale = reduce(&snapshot, &event("stale", Fact::Land { bloom, new_head: digest(50) }), &ResolvedConfigs::default(), &SpendWindow::default());
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
    let rejected = reduce(
        &after_first,
        &event("second", Fact::Seal(second)),
        &ResolvedConfigs::default(),
        &SpendWindow::default(),
    );
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
    let decided =
        reduce(&base, &event("empty", Fact::Seal(empty)), &ResolvedConfigs::default(), &SpendWindow::default());
    assert!(matches!(decided.outcome, Outcome::SealRejected(SealError::EmptyMembership)));
}

#[test]
fn seal_rejects_duplicate_workpiece() {
    let base = Snapshot::new(digest(1));
    // Same workpiece at two distinct revisions — not an exact duplicate, so it
    // survives seal's dedup and reaches the reducer's duplicate check.
    let dup = draft(1, vec![membership("wp", 10), membership("wp", 11)]).seal();
    let decided = reduce(&base, &event("dup", Fact::Seal(dup)), &ResolvedConfigs::default(), &SpendWindow::default());
    match decided.outcome {
        Outcome::SealRejected(SealError::DuplicateWorkpiece(wp)) => assert_eq!(wp, workpiece("wp")),
        other => panic!("expected DuplicateWorkpiece, got {other:?}"),
    }
}

// Tripwire (ADR-0191): the composition workpiece shares the member maps — the
// stage cursor, the wedge set, the dispatch ledger — so a member holding the
// reserved id would silently share the composition's cursor and each would move
// the other's line position. The door refuses it while the operator is still
// holding the membership they authored.
#[test]
fn seal_rejects_a_member_claiming_the_reserved_composition_id() {
    let base = Snapshot::new(digest(1));
    let reserved = draft(1, vec![membership(WorkpieceId::COMPOSITION, 10)]).seal();
    let decided =
        reduce(&base, &event("reserved", Fact::Seal(reserved)), &ResolvedConfigs::default(), &SpendWindow::default());
    match decided.outcome {
        Outcome::SealRejected(SealError::ReservedWorkpieceId(wp)) => assert!(wp.is_composition()),
        other => panic!("expected ReservedWorkpieceId, got {other:?}"),
    }
}

#[test]
fn seal_rejects_unbound_or_wrong_kind_approval() {
    let base = Snapshot::new(digest(1));

    // Approval whose subject is not the member's scope revision.
    let mut wrong_subject = membership("wp", 10);
    wrong_subject.approval = Evidence { subject: digest(99), kind: EvidenceKind::Approval, detail: digest(0) };
    let decided = reduce(
        &base,
        &event("s1", Fact::Seal(draft(1, vec![wrong_subject]).seal())),
        &ResolvedConfigs::default(),
        &SpendWindow::default(),
    );
    assert!(matches!(decided.outcome, Outcome::SealRejected(SealError::UnapprovedMember(_))));

    // Right subject, but the evidence is not an Approval.
    let mut wrong_kind = membership("wp", 10);
    wrong_kind.approval = Evidence { subject: digest(10), kind: EvidenceKind::VerificationResult, detail: digest(0) };
    let decided = reduce(
        &base,
        &event("s2", Fact::Seal(draft(1, vec![wrong_kind]).seal())),
        &ResolvedConfigs::default(),
        &SpendWindow::default(),
    );
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

    let again =
        reduce(&after_seal, &event("again", Fact::Seal(spec)), &ResolvedConfigs::default(), &SpendWindow::default());
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
    let decided = reduce(
        &Snapshot::new(digest(1)),
        &event("unrunnable", Fact::Seal(unrunnable.seal())),
        &configs,
        &SpendWindow::default(),
    );
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
        &SpendWindow::default(),
    );
    assert!(matches!(admitted.outcome, Outcome::Sealed(_)), "sealing no catalog runs the line: {:?}", admitted.outcome);

    let (empty, configs) = draft_with_catalog(1, vec![membership("wp", 10)], &StageCatalog::default());
    let decided =
        reduce(&Snapshot::new(digest(1)), &event("empty", Fact::Seal(empty.seal())), &configs, &SpendWindow::default());
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

    let decided =
        reduce(&Snapshot::new(digest(1)), &event("stale", Fact::Seal(draft.seal())), &configs, &SpendWindow::default());
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

// #4903 — the mixed successor: one member arrives on an inherited claim, the
// other re-runs under the successor and its claim completes the set. That
// completing claim takes the ordinary integrate path, which named no predecessor
// at all — so the fold went looking for a candidate ref that exists only under
// the predecessor's bloom, the source answered "head does not exist" every tick,
// and the bloom read healthy throughout. The predecessor's own record is where
// the inheritance is written: the candidate this fold carries for that member is
// the candidate the predecessor claimed.
#[test]
fn a_mixed_successors_completing_claim_dispatches_a_fold_that_adopts_the_predecessor() {
    let (snapshot, predecessor_spec) =
        sealed_and_resolved(1, vec![membership("kept", 10), membership("rerun", 11)], 40);
    let predecessor = predecessor_spec.id();
    let inherited = snapshot.blooms.get(&predecessor).unwrap().claims.get(&workpiece("kept")).unwrap().candidate;

    // "kept" is re-admitted at its own scope revision and inherits; "rerun" comes
    // back at a new one, so it drops its stale claim and enters the line fresh.
    let snapshot = observing(&snapshot, 2);
    let successor_spec = draft(2, vec![membership("kept", 10), membership("rerun", 12)]).seal();
    let successor = successor_spec.id();
    let (snapshot, _) = step(&snapshot, &event("sup", Fact::Supersede { predecessor, successor: successor_spec }));

    // The re-run member captured under the successor, and its claim completes a
    // set whose other candidate is still the predecessor's.
    let (_, decided) =
        step(&snapshot, &event("rerun-claim", Fact::Integrate { bloom: successor, claim: claim("rerun", 12, 150) }));

    match decided.effects.iter().find(|e| matches!(e, Decision::DispatchIntegration { .. })) {
        Some(Decision::DispatchIntegration { bloom, members, adopt_from, .. }) => {
            assert_eq!(*bloom, successor, "the fold is dispatched for the successor");
            assert_eq!(
                *adopt_from,
                Some(predecessor),
                "one folded candidate was produced under the predecessor's id, so the fold has to adopt its ref \
                 before it can merge one",
            );
            let folded: Vec<(&str, Digest)> =
                members.iter().map(|member| (member.workpiece.0.as_str(), member.candidate)).collect();
            assert_eq!(
                folded,
                vec![("kept", inherited), ("rerun", digest(150))],
                "carrying the inherited candidate beside the one the successor just produced",
            );
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
        &SpendWindow::default(),
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
        &SpendWindow::default(),
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
    let decided = reduce(
        &snapshot,
        &event("dup", Fact::Supersede { predecessor, successor: dup }),
        &ResolvedConfigs::default(),
        &SpendWindow::default(),
    );
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
        &SpendWindow::default(),
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
        &SpendWindow::default(),
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
        &SpendWindow::default(),
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
    match reduce(&after, &verdict, &ResolvedConfigs::default(), &SpendWindow::default()).outcome {
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

/// A passing composition review over `tree` — the hop that resolves a bloom
/// once its composite gate run is green.
fn review_passed(bloom: BloomId, key: &str, tree: u8) -> Event {
    event(
        key,
        Fact::AggregateReviewCompleted {
            bloom,
            passed: true,
            evidence: Evidence { subject: digest(tree), kind: EvidenceKind::ReviewFinding, detail: digest(58) },
            implicated: vec![],
        },
    )
}

/// A returning weave repair (ADR-0191 §5): the composition workpiece's `Refine`
/// completing with the re-woven candidate it produced.
fn weave_repaired(bloom: BloomId, key: &str, from: u8, tree: u8, head: u8) -> Event {
    event(
        key,
        Fact::AttemptCompleted {
            bloom,
            workpiece: composition(),
            stage: StageId::Refine,
            passed: true,
            evidence: Evidence { subject: digest(from), kind: EvidenceKind::VerificationResult, detail: digest(57) },
            candidate: Some(CandidateRef { tree: digest(tree), checkout: digest(head) }),
        },
    )
}

/// The composition workpiece's id — the synthetic subject ADR-0191 gives the
/// weave, keyed into the same member maps.
fn composition() -> WorkpieceId {
    WorkpieceId::composition()
}

/// The composition's stage cursor, which every repair path must write.
fn composition_cursor(record: &BloomRecord) -> StageProgress {
    *record.progress.get(&composition()).expect("a repairing composition carries a cursor")
}

/// Tripwire helper for ADR-0191 §4: no decision in `decided` dispatches against
/// a member, and none revokes a member's resolution.
///
/// This is the exact incident shape of bloom `05b1f598` — an aggregate refusal
/// re-entering finished members at `Construct` with fresh work orders, throwing
/// away four reviewed candidates. Under ADR-0191 that transition does not exist,
/// and this is what says so.
fn assert_no_member_dispatch(decided: &Decisions, why: &str) {
    for effect in &decided.effects {
        if let Decision::DispatchAttempt { workpiece, stage, .. } = effect {
            assert!(workpiece.is_composition(), "{why}: dispatched {stage:?} against member {workpiece:?}");
        }
        assert!(
            !matches!(effect, Decision::RevokeResolution { .. }),
            "{why}: revoked a member resolution ({effect:?})",
        );
    }
}

// #4689 / ADR-0191 — the landing gate is the last one, and the only one that
// judges the bloom against a mainline that moved while it worked. A refused
// landing un-resolves the bloom and repairs the *weave*; the second refusal
// spends the `Land` budget and parks it. Neither leaves the bloom polling a
// proposal nothing will accept, which is the behaviour this replaces.
//
// Two tripwires. The un-resolve: a bloom left `Resolved` while it repairs would
// let the land reactor re-propose the exact head the gate just refused, which is
// an infinite loop rather than a repair. And the immutability rule (ADR-0191
// §4): a landing rejection is a fact about the composed tree, so no member's
// claim may be revoked and no member may be dispatched by it.
//
// #5106 is the admission half of the second refusal: the two facts below share
// a cause string and differ by the head they judged, which is the shape the
// land reactor now admits under. Under a bloom+cause key the second reduce
// was `Outcome::Duplicate` and the bloom sat Resolved with its land acked.
#[test]
fn a_refused_landing_repairs_the_weave_then_parks_at_the_budget() {
    let base = Snapshot::new(digest(1));
    let spec = draft(1, vec![membership("alpha", 10), membership("beta", 11)]).seal();
    let bloom = spec.id();
    let (snapshot, _) = step(&base, &event("seal", Fact::Seal(spec)));
    let (snapshot, _) = step(&snapshot, &event("i-a", Fact::Integrate { bloom, claim: claim("alpha", 10, 100) }));
    let (snapshot, _) = step(&snapshot, &event("i-b", Fact::Integrate { bloom, claim: claim("beta", 11, 101) }));
    let (snapshot, _) =
        step(&snapshot, &event("r1", Fact::Resolve { bloom, tree: digest(40), head: digest(41), lineage: vec![] }));
    let (snapshot, _) = step(&snapshot, &verify_passed(bloom, "v1", 40));
    let (snapshot, _) = step(&snapshot, &review_passed(bloom, "review", 40));
    assert_eq!(snapshot.blooms.get(&bloom).unwrap().status, BloomStatus::Resolved, "the bloom is awaiting its land");

    let refused = |head: u8, detail: u8| {
        event(
            &format!("aether.bloomery.landing_rejected:{head}:CI pass,Clippy"),
            Fact::LandingRejected {
                bloom,
                evidence: Evidence {
                    subject: digest(head),
                    kind: EvidenceKind::VerificationResult,
                    detail: digest(detail),
                },
            },
        )
    };

    // A rejection naming a head other than the one being landed is stale.
    assert!(matches!(
        reduce(&snapshot, &refused(99, 60), &ResolvedConfigs::default(), &SpendWindow::default()).outcome,
        Outcome::LandingRejectedRefused(LandingRejectedError::SubjectMismatch { .. }),
    ));

    let (after1, d1) = step(&snapshot, &refused(41, 60));
    assert!(matches!(d1.outcome, Outcome::CompositionRewoven { refused_at: StageId::Land, attempt: 1, .. }));
    assert_no_member_dispatch(&d1, "a refused landing repairs the weave, never a member");
    let record = after1.blooms.get(&bloom).unwrap();
    assert_eq!(record.status, BloomStatus::Sealed, "the bloom is no longer land-ready");
    assert_eq!(record.resolved_head, None, "and no longer names a head to propose");
    assert_eq!(record.claims.len(), 2, "every member's resolution stands — members are immutable after review");
    assert_eq!(record.landing_rolls, 1);
    assert_eq!(composition_cursor(record).stage, StageId::Refine, "the composition is the thing that repairs");

    // The weave repair returns a re-woven candidate, which re-enters the
    // composition's Verify and then its Review, and the bloom proposes again.
    let (s2, _) = step(&after1, &weave_repaired(bloom, "weave-1", 41, 44, 45));
    let (s2, _) = step(&s2, &verify_passed(bloom, "v2", 44));
    let (s2, _) = step(&s2, &review_passed(bloom, "review-2", 44));
    assert_eq!(s2.blooms.get(&bloom).unwrap().status, BloomStatus::Resolved);

    // The second refusal spends the budget: parked, nothing dispatches — and it
    // files its finding on the composition's channel like the refusal it is
    // (#4977). Spending the budget changes where the bloom goes next, not what
    // the gate said about the composed tree.
    let (after2, d2) = step(&s2, &refused(45, 61));
    assert!(
        !matches!(d2.outcome, Outcome::Duplicate),
        "a second landing refused for the same cause is a new fact, not a replay (#5106)",
    );
    assert!(matches!(d2.outcome, Outcome::LandingParked { rolls: 2, question, .. } if question == digest(61)));
    assert!(
        !d2.effects.iter().any(|e| matches!(e, Decision::DispatchAttempt { .. } | Decision::SetUnresolved { .. })),
        "a parked bloom dispatches nothing",
    );
    let record = after2.blooms.get(&bloom).unwrap();
    assert_eq!(record.review_park, Some(digest(61)));
    let parked = record.composition_findings.last().expect("the ceiling refusal files a finding");
    assert_eq!(parked.detail, digest(61), "carrying the rejection that spent the budget");
    assert_eq!(parked.subject, digest(45), "raised against the head the landing gate refused");
    assert!(parked.implicated.is_empty(), "a mainline that moved implicates no member");
    assert!(
        record.open_composition_findings().any(|open| open.detail == digest(61)),
        "so the parked bloom is adjudicable through the findings channel, not only through its park marker",
    );
}

// #4696 / ADR-0191 §2 — a fold that does not build is a defect of the
// *composition*, not of a member that passed on its own. The refusal files a
// finding on the composition's channel and dispatches the weave repair against
// the tree that failed to build; the held fold stays held, because it is the
// candidate under repair. The stage's own budget bounds the retries — the second
// failure parks the bloom rather than re-weaving a combination that has not
// built yet.
//
// Tripwire for ADR-0191 §4 above all: the old behaviour re-opened every member
// at `Refine` and revoked every claim, which is how a compile failure in the
// combination came to cost five members their finished work.
#[test]
fn a_failing_aggregate_verify_repairs_the_weave_then_parks_at_the_ceiling() {
    let base = Snapshot::new(digest(1));
    let spec = draft(1, vec![membership("alpha", 10), membership("beta", 11)]).seal();
    let bloom = spec.id();
    let (snapshot, _) = step(&base, &event("seal", Fact::Seal(spec)));
    let (snapshot, _) = step(&snapshot, &event("i-a", Fact::Integrate { bloom, claim: claim("alpha", 10, 100) }));
    let (snapshot, _) = step(&snapshot, &event("i-b", Fact::Integrate { bloom, claim: claim("beta", 11, 101) }));
    let (snapshot, _) =
        step(&snapshot, &event("r1", Fact::Resolve { bloom, tree: digest(40), head: digest(41), lineage: vec![] }));

    let failed = |key: &str, subject: u8, detail: u8| {
        event(
            key,
            Fact::AggregateVerifyCompleted {
                bloom,
                passed: false,
                evidence: Evidence {
                    subject: digest(subject),
                    kind: EvidenceKind::VerificationResult,
                    detail: digest(detail),
                },
            },
        )
    };

    // A verdict bound to a tree other than the held fold's is stale — refused,
    // so a superseded fold's failure cannot act on a newer one.
    assert!(matches!(
        reduce(&snapshot, &failed("stale", 99, 52), &ResolvedConfigs::default(), &SpendWindow::default()).outcome,
        Outcome::AggregateVerifyRejected(AggregateVerifyError::SubjectMismatch { .. }),
    ));

    let (after1, d1) = step(&snapshot, &failed("fail-1", 40, 52));
    assert!(matches!(d1.outcome, Outcome::CompositionRewoven { refused_at: StageId::AggregateVerify, attempt: 1, .. }));
    assert_no_member_dispatch(&d1, "a fold that does not build repairs at the seam");
    let record = after1.blooms.get(&bloom).unwrap();
    assert_eq!(record.claims.len(), 2, "every member's resolution stands");
    assert!(record.integration.is_some(), "the fold stays held — it is the composition's candidate under repair");
    assert_eq!(record.aggregate_verify_rolls, 1);
    assert_eq!(record.aggregate_rolls, 0, "a spent verify roll does not spend the critic's budget");
    let finding = record.composition_findings.last().expect("the refusal files a finding");
    assert_eq!(finding.subject, digest(40), "the finding names the weave it was raised against");
    assert_eq!(finding.detail, digest(52));
    assert!(finding.implicated.is_empty(), "a compile failure over the fold implicates no member in particular");
    let cursor = composition_cursor(record);
    assert_eq!(cursor.stage, StageId::Refine);
    assert_eq!(cursor.candidate.unwrap().tree, digest(40), "the repair targets the tree that failed to build");
    for member in ["alpha", "beta"] {
        assert_ne!(record.progress.get(&workpiece(member)).unwrap().stage, StageId::Refine, "{member} is untouched");
    }

    // The weave repair returns; the composition re-enters its Verify over the
    // re-woven tree, on the stage's second roll.
    let (after2, d2) = step(&after1, &weave_repaired(bloom, "weave-1", 40, 44, 45));
    assert!(matches!(d2.outcome, Outcome::CompositionRepaired { tree, .. } if tree == digest(44)));
    assert!(
        d2.effects.iter().any(|e| matches!(e, Decision::DispatchAggregateVerify { roll: 2, .. })),
        "the repaired weave goes straight back to the composite gate run",
    );
    assert_eq!(after2.blooms.get(&bloom).unwrap().integration.as_ref().unwrap().tree, digest(44));

    // The second failure spends the budget: the bloom parks and dispatches
    // nothing further — and files its finding on the way, because a fold that
    // still does not build is a refusal of the composed tree whether the budget
    // buys another repair or not (#4977).
    let (after3, d3) = step(&after2, &failed("fail-2", 44, 53));
    assert!(matches!(d3.outcome, Outcome::AggregateVerifyParked { rolls: 2, question, .. } if question == digest(53)));
    assert!(
        !d3.effects.iter().any(|e| matches!(e, Decision::DispatchAttempt { .. } | Decision::RevokeResolution { .. })),
        "a parked bloom dispatches nothing",
    );
    let record = after3.blooms.get(&bloom).unwrap();
    assert_eq!(record.review_park, Some(digest(53)));
    let parked = record.composition_findings.last().expect("the ceiling refusal files a finding");
    assert_eq!(parked.detail, digest(53), "carrying the verdict that spent the budget");
    assert_eq!(parked.subject, digest(44), "raised against the weave that still does not build");
    assert!(parked.implicated.is_empty());
    assert!(
        record.open_composition_findings().any(|open| open.detail == digest(53)),
        "so the parked bloom is adjudicable through the findings channel, not only through its park marker",
    );
}

// ADR-0191 §3/§4/§5 — a failing composition review repairs the weave and never
// re-opens a member. The implicated set is a *label on the finding* (it files
// follow-up work and points a reader at the right code), not a routing table:
// no claim is revoked, no member cursor moves, and no member work order is
// dispatched. The second failing verdict parks the bloom at the two-pass
// ceiling — never a third roll.
//
// This is the direct tripwire for the bloom `05b1f598` incident: an aggregate
// refusal that dispatched member `construct.implement` orders and discarded four
// finished, reviewed candidates. Under this model that transition does not
// exist, so the assertion is that no member dispatch appears at all.
#[test]
fn a_failing_composition_review_repairs_the_weave_and_never_reopens_a_member() {
    let base = Snapshot::new(digest(1));
    let spec = draft(1, vec![membership("alpha", 10), membership("beta", 11)]).seal();
    let bloom = spec.id();
    let (snapshot, _) = step(&base, &event("seal", Fact::Seal(spec)));
    let (snapshot, _) = step(&snapshot, &event("i-a", Fact::Integrate { bloom, claim: claim("alpha", 10, 100) }));
    let (snapshot, _) = step(&snapshot, &event("i-b", Fact::Integrate { bloom, claim: claim("beta", 11, 101) }));
    let (snapshot, _) =
        step(&snapshot, &event("r1", Fact::Resolve { bloom, tree: digest(40), head: digest(41), lineage: vec![] }));
    let (snapshot, _) = step(&snapshot, &verify_passed(bloom, "v1", 40));
    let member_cursors = snapshot.blooms.get(&bloom).unwrap().progress.clone();

    let verdict = |key: &str, subject: u8, detail: u8, implicated: Vec<&str>| {
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
                implicated: implicated.into_iter().map(workpiece).collect(),
            },
        )
    };

    // A verdict bound to a tree other than the held fold's is stale — refused.
    assert!(matches!(
        reduce(
            &snapshot,
            &verdict("stale", 99, 50, vec!["alpha"]),
            &ResolvedConfigs::default(),
            &SpendWindow::default()
        )
        .outcome,
        Outcome::AggregateReviewRejected(AggregateReviewError::SubjectMismatch { .. }),
    ));
    // A verdict naming a non-member is malformed — the label still has to name
    // real code for the follow-up it files to be findable.
    assert!(matches!(
        reduce(
            &snapshot,
            &verdict("ghost", 40, 50, vec!["ghost"]),
            &ResolvedConfigs::default(),
            &SpendWindow::default()
        )
        .outcome,
        Outcome::AggregateReviewRejected(AggregateReviewError::NotAMember(_)),
    ));
    // An empty implication is a finding about the weave as a whole. It is no
    // longer expanded to every member, because there is nothing to route to.
    assert!(matches!(
        reduce(&snapshot, &verdict("empty", 40, 50, vec![]), &ResolvedConfigs::default(), &SpendWindow::default())
            .outcome,
        Outcome::CompositionRewoven { refused_at: StageId::AggregateReview, attempt: 1, .. },
    ));

    let (after1, d1) = step(&snapshot, &verdict("fail-1", 40, 50, vec!["alpha"]));
    assert!(matches!(d1.outcome, Outcome::CompositionRewoven { refused_at: StageId::AggregateReview, attempt: 1, .. }));
    assert_no_member_dispatch(&d1, "the 05b1f598 re-entry is abolished");
    match d1.effects.iter().find(|effect| matches!(effect, Decision::DispatchAttempt { .. })) {
        Some(Decision::DispatchAttempt { workpiece: wp, stage, transformation, candidate, .. }) => {
            assert!(wp.is_composition(), "the one dispatch is the composition's");
            assert_eq!(*stage, StageId::Refine, "and it is the weave repair");
            assert_eq!(*candidate, Some(digest(40)), "aimed at the composed tree");
            assert_eq!(transformation.checkout, digest(41), "checked out at the composed head");
        }
        other => panic!("expected the composition's weave repair, got {other:?}"),
    }
    let record = after1.blooms.get(&bloom).unwrap();
    assert_eq!(record.claims.len(), 2, "a member that passed its review is done and is never touched again");
    assert_eq!(record.progress, member_cursors_with_composition(&member_cursors, record), "no member cursor moved");
    assert!(record.wedged.is_empty());
    assert!(record.integration.is_some(), "the fold stays held as the composition's candidate");
    assert_eq!(record.aggregate_rolls, 1);
    let finding = record.composition_findings.last().expect("the refusal files a finding");
    assert_eq!(finding.implicated, vec![workpiece("alpha")], "the member-scope observation is recorded, not routed");
    assert_eq!(finding.detail, digest(50));

    // The bloom still cannot resolve out from under the repair: the fold it
    // holds is the one that was refused.
    let (after2, d2) = step(&after1, &weave_repaired(bloom, "weave-1", 40, 44, 45));
    assert!(matches!(d2.outcome, Outcome::CompositionRepaired { .. }));
    let (after3, d3) = step(&after2, &verify_passed(bloom, "v2", 44));
    assert!(matches!(d3.outcome, Outcome::AggregateVerifyPassed { .. }));
    assert!(
        d3.effects.iter().any(|e| matches!(e, Decision::DispatchAggregateReview { roll: 2, .. })),
        "the passing re-verify dispatches the delta-confirm",
    );

    // The failing delta-confirm hits the ceiling: the bloom parks to the owner,
    // and files the refusal's finding beside the park (#4977) — the two-pass
    // ceiling is where the composition's refusals *end*, so a channel missing
    // them would undercount exactly the ones that escalated.
    let (after4, d4) = step(&after3, &verdict("fail-2", 44, 51, vec!["alpha"]));
    assert!(matches!(d4.outcome, Outcome::AggregateReviewParked { rolls: 2, question, .. } if question == digest(51)));
    assert_no_member_dispatch(&d4, "a parked bloom re-opens nothing");
    let record = after4.blooms.get(&bloom).unwrap();
    assert_eq!(record.review_park, Some(digest(51)), "the park marker names the failing review's record artifact");
    assert!(record.holds.contains(&digest(51)), "the park raises the pending-decision hold");
    let parked = record.composition_findings.last().expect("the ceiling refusal files a finding");
    assert_eq!(parked.detail, digest(51), "carrying the delta-confirm that spent the budget");
    assert_eq!(parked.subject, digest(44), "raised against the re-woven tree it judged");
    assert_eq!(parked.implicated, vec![workpiece("alpha")], "with the members the verdict named, recorded not routed");
    assert!(
        record.open_composition_findings().any(|open| open.detail == digest(51)),
        "so the parked bloom is adjudicable through the findings channel, not only through its park marker",
    );
    assert!(record.integration.is_some(), "the fold stays held as the owner's decision context");
    // A re-fold while parked is refused by the pending decision — the named
    // reason is the owner's open question, not a bare ceiling count.
    assert!(matches!(
        reduce(&after4, &event("r3", Fact::Resolve { bloom, tree: digest(46), head: digest(47), lineage: vec![] }), &ResolvedConfigs::default(), &SpendWindow::default())
            .outcome,
        Outcome::ResolveRejected(ResolveError::PendingDecision { question }) if question == digest(51),
    ));
}

/// The member cursors as they stood, plus whatever cursor the composition now
/// carries — so a "no member cursor moved" assertion can compare whole maps
/// without the composition's own (expected) entry masking a member move.
fn member_cursors_with_composition(
    before: &BTreeMap<WorkpieceId, StageProgress>,
    record: &BloomRecord,
) -> BTreeMap<WorkpieceId, StageProgress> {
    let mut expected = before.clone();
    if let Some(progress) = record.progress.get(&composition()) {
        expected.insert(composition(), *progress);
    }
    expected
}

// ADR-0176 — an aggregate review whose executor could not run is not a verdict
// about the candidate, so it must not move one. The fold stays held, every claim
// stands, no cursor moves, and the retry is a re-dispatch of the *same* review
// against the *same* tree, bounded by the sealed AggregateReview budget on a
// ledger of its own.
//
// Tripwire for the ledger separation above all: charging `aggregate_rolls` would
// spend the critic's two passes on judgments it never gave, and re-opening
// members would spend a candidate's repair budget on a host outage — the exact
// behaviour that reached the reducer while the fault was flattened to a failing
// review.
#[test]
fn an_aggregate_review_executor_fault_retries_the_review_without_charging_any_other_ledger() {
    let base = Snapshot::new(digest(1));
    let spec = draft(1, vec![membership("alpha", 10), membership("beta", 11)]).seal();
    let bloom = spec.id();
    let (snapshot, _) = step(&base, &event("seal", Fact::Seal(spec)));
    let (snapshot, _) = step(&snapshot, &event("i-a", Fact::Integrate { bloom, claim: claim("alpha", 10, 100) }));
    let (snapshot, _) = step(&snapshot, &event("i-b", Fact::Integrate { bloom, claim: claim("beta", 11, 101) }));
    let (snapshot, _) =
        step(&snapshot, &event("r1", Fact::Resolve { bloom, tree: digest(40), head: digest(41), lineage: vec![] }));
    let (snapshot, _) = step(&snapshot, &verify_passed(bloom, "v1", 40));

    let fault = |key: &str, subject: u8, detail: u8| {
        event(
            key,
            Fact::AggregateReviewExecutorFault {
                bloom,
                evidence: Evidence {
                    subject: digest(subject),
                    kind: EvidenceKind::ExecutorFault,
                    detail: digest(detail),
                },
            },
        )
    };

    // A fault bound to a tree other than the held fold's is stale — refused on
    // the same axis a stale verdict is, so a report from a superseded fold
    // cannot spend a newer fold's retries.
    assert!(matches!(
        reduce(&snapshot, &fault("stale", 99, 60), &ResolvedConfigs::default(), &SpendWindow::default()).outcome,
        Outcome::AggregateReviewRejected(AggregateReviewError::SubjectMismatch { .. }),
    ));

    // The first fault: one redispatch of the same held tree, and nothing else.
    let (after1, d1) = step(&snapshot, &fault("fault-1", 40, 60));
    assert!(matches!(
        d1.outcome,
        Outcome::AggregateReviewExecutorFaulted { fault, budget: 2, .. } if fault.rolls == 1 && fault.subject == digest(40),
    ));
    match d1.effects.iter().find(|effect| matches!(effect, Decision::DispatchAggregateReview { .. })) {
        Some(Decision::DispatchAggregateReview { transformation, roll, .. }) => {
            assert_eq!(*roll, 1, "a fault spends no review roll, so the critic's cursor has not moved");
            assert_eq!(transformation.inputs[0], digest(40), "the retry judges the same held fold");
            assert_eq!(transformation.checkout, digest(41), "and checks out the same head");
        }
        other => panic!("expected a redispatch of the held fold, got {other:?}"),
    }
    assert!(
        !d1.effects.iter().any(|effect| matches!(
            effect,
            Decision::DispatchAttempt { .. }
                | Decision::RevokeResolution { .. }
                | Decision::RecordAggregateRoll { .. }
                | Decision::RecordIntegration { .. }
                | Decision::RecordReviewPark { .. }
        )),
        "a fault re-opens no member, revokes no claim, spends no review roll, and clears no fold",
    );
    let record = after1.blooms.get(&bloom).unwrap();
    assert_eq!(record.aggregate_fault.unwrap().rolls, 1);
    assert_eq!(record.aggregate_rolls, 0, "the critic's ledger is untouched");
    assert_eq!(record.claims.len(), 2, "both claims stand");
    assert!(record.integration.is_some(), "the fold stays held for the retry");
    assert!(record.holds.is_empty(), "a fault raises no pending decision — there is nothing to adopt");
    for member in ["alpha", "beta"] {
        assert!(record.progress.get(&workpiece(member)).is_none_or(|cursor| cursor.repair_rolls == 0));
    }

    // Replaying the admitted fault is a no-op: the series is folded from the
    // evidence log, so an idempotency-keyed replay must not buy a second roll.
    let (replayed, d_replay) = step(&after1, &fault("fault-1", 40, 60));
    assert!(matches!(d_replay.outcome, Outcome::Duplicate));
    assert_eq!(replayed.blooms.get(&bloom).unwrap().aggregate_fault.unwrap().rolls, 1);

    // The second fault on the same fold reaches the sealed budget: terminal, and
    // it dispatches nothing at all rather than looping the review forever.
    let (after2, d2) = step(&after1, &fault("fault-2", 40, 61));
    assert!(matches!(
        d2.outcome,
        Outcome::AggregateReviewExecutorWedged { fault, budget: 2, .. } if fault.rolls == 2 && fault.evidence == digest(61),
    ));
    assert_eq!(d2.effects.len(), 1, "the terminal fault records its evidence and decides nothing else");
    assert!(matches!(d2.effects[0], Decision::RecordEvidence { .. }));
    let record = after2.blooms.get(&bloom).unwrap();
    assert_eq!(record.aggregate_rolls, 0, "even the terminal fault charges the critic nothing");
    assert_eq!(record.claims.len(), 2, "and re-opens no member");
    assert!(record.review_park.is_none(), "an executor outage is not an ADR-0151 pending decision");
}

// ADR-0176 — the fault series is keyed to the fold it is against, so a bloom
// that re-integrated after an outage starts over rather than inheriting spent
// retries. Tripwire: a bloom-keyed counter would wedge the very next fold on its
// first fault, permanently, with no route back other than supersession.
#[test]
fn a_fault_series_resets_when_a_different_fold_arrives() {
    let base = Snapshot::new(digest(1));
    let spec = draft(1, vec![membership("wp", 10)]).seal();
    let bloom = spec.id();
    let (snapshot, _) = step(&base, &event("seal", Fact::Seal(spec)));
    let (snapshot, _) = step(&snapshot, &event("i1", Fact::Integrate { bloom, claim: claim("wp", 10, 100) }));
    let (snapshot, _) =
        step(&snapshot, &event("r1", Fact::Resolve { bloom, tree: digest(40), head: digest(41), lineage: vec![] }));
    let (snapshot, _) = step(&snapshot, &verify_passed(bloom, "v1", 40));

    let fault = |key: &str, subject: u8| {
        event(
            key,
            Fact::AggregateReviewExecutorFault {
                bloom,
                evidence: Evidence { subject: digest(subject), kind: EvidenceKind::ExecutorFault, detail: digest(60) },
            },
        )
    };
    let (snapshot, _) = step(&snapshot, &fault("f1", 40));
    assert_eq!(snapshot.blooms.get(&bloom).unwrap().aggregate_fault.unwrap().rolls, 1);

    // A failing review clears the fold and re-opens the member; the repaired
    // member re-integrates onto a fresh tree.
    let (snapshot, _) = step(
        &snapshot,
        &event(
            "review-fail",
            Fact::AggregateReviewCompleted {
                bloom,
                passed: false,
                evidence: Evidence { subject: digest(40), kind: EvidenceKind::ReviewFinding, detail: digest(50) },
                implicated: vec![],
            },
        ),
    );
    let (snapshot, _) = step(&snapshot, &event("i2", Fact::Integrate { bloom, claim: claim("wp", 10, 101) }));
    let (snapshot, _) =
        step(&snapshot, &event("r2", Fact::Resolve { bloom, tree: digest(44), head: digest(45), lineage: vec![] }));
    let (snapshot, _) = step(&snapshot, &verify_passed(bloom, "v2", 44));

    let (after, decisions) = step(&snapshot, &fault("f2", 44));
    let series = after.blooms.get(&bloom).unwrap().aggregate_fault.unwrap();
    assert_eq!(series.subject, digest(44), "the series re-keys onto the fold it is against");
    assert_eq!(series.rolls, 1, "a new fold begins its own series rather than inheriting the last one's");
    assert!(
        decisions.effects.iter().any(|effect| matches!(effect, Decision::DispatchAggregateReview { .. })),
        "so the first fault on the new fold still buys its bounded retry",
    );
}

// ADR-0195 — a member-stage executor fault is not a verdict about the candidate,
// so it must not move one. The cursor stays put, attempts and repair_rolls stay
// put, and the retry is a re-dispatch of the *same* stage against the *same*
// artifact, bounded by the sealed stage budget on a ledger of its own.
//
// Tripwire for the ledger separation above all: charging `attempts` would spend
// the model's laps on judgments it never gave, and routing to Refine would spend
// a candidate's repair budget on a host outage.
#[test]
fn a_member_executor_fault_retries_the_same_stage_without_charging_work_or_repair() {
    let base = Snapshot::new(digest(1));
    let spec = draft(1, vec![membership("wp", 10)]).seal();
    let bloom = spec.id();
    let (snapshot, _) = step(&base, &event("seal", Fact::Seal(spec)));

    let captured = CandidateRef { tree: digest(21), checkout: digest(22) };
    let (snapshot, _) = step(
        &snapshot,
        &event(
            "c-pass",
            Fact::AttemptCompleted {
                bloom,
                workpiece: workpiece("wp"),
                stage: StageId::Construct,
                passed: true,
                evidence: Evidence { subject: digest(21), kind: EvidenceKind::VerificationResult, detail: digest(80) },
                candidate: Some(captured),
            },
        ),
    );

    let before = snapshot.blooms.get(&bloom).unwrap().progress.get(&workpiece("wp")).copied().unwrap();
    assert_eq!(before.stage, StageId::Verify);
    assert_eq!(before.attempts, 1);
    assert_eq!(before.repair_rolls, 0);
    assert_eq!(before.candidate, Some(captured));

    let fault = |key: &str, detail: u8| {
        event(
            key,
            Fact::MemberExecutorFault {
                bloom,
                workpiece: workpiece("wp"),
                stage: StageId::Verify,
                evidence: Evidence { subject: digest(21), kind: EvidenceKind::ExecutorFault, detail: digest(detail) },
            },
        )
    };

    // A fault bound to a tree other than the member's current candidate is stale.
    let stale = event(
        "stale",
        Fact::MemberExecutorFault {
            bloom,
            workpiece: workpiece("wp"),
            stage: StageId::Verify,
            evidence: Evidence { subject: digest(99), kind: EvidenceKind::ExecutorFault, detail: digest(60) },
        },
    );
    assert!(matches!(
        reduce(&snapshot, &stale, &ResolvedConfigs::default(), &SpendWindow::default()).outcome,
        Outcome::MemberExecutorFaultRejected(aether_bloomery::MemberExecutorFaultError::EvidenceNotBound { .. }),
    ));

    let (after1, d1) = step(&snapshot, &fault("fault-1", 60));
    assert!(matches!(d1.outcome, Outcome::MachineryRetried { stage: StageId::Verify, rolls: 1, budget: 3, .. },));
    match d1.effects.iter().find(|effect| matches!(effect, Decision::DispatchAttempt { .. })) {
        Some(Decision::DispatchAttempt { stage, candidate, transformation, .. }) => {
            assert_eq!(*stage, StageId::Verify, "the retry is the same stage");
            assert_eq!(*candidate, Some(captured.tree), "and aims at the same candidate");
            assert_eq!(transformation.checkout, captured.checkout);
        }
        other => panic!("expected a same-stage redispatch, got {other:?}"),
    }
    assert!(
        !d1.effects
            .iter()
            .any(|effect| matches!(effect, Decision::RecordWedge { .. } | Decision::RevokeResolution { .. })),
        "a fault below the ceiling wedges nothing and revokes no claim",
    );
    let cursor = after1.blooms.get(&bloom).unwrap().progress.get(&workpiece("wp")).copied().unwrap();
    assert_eq!(cursor.attempts, 1, "ordinary attempts do not move");
    assert_eq!(cursor.repair_rolls, 0, "repair rolls do not move");
    assert_eq!(cursor.candidate, Some(captured), "the candidate does not move");
    assert_eq!(cursor.seen_verify_failures, VerifyFailureSet::EMPTY, "no verifier history is written");
    assert_eq!(after1.member_machinery(&bloom, &workpiece("wp")).unwrap().rolls, 1);

    let (replayed, d_replay) = step(&after1, &fault("fault-1", 60));
    assert!(matches!(d_replay.outcome, Outcome::Duplicate));
    assert_eq!(replayed.member_machinery(&bloom, &workpiece("wp")).unwrap().rolls, 1);

    let (after2, _) = step(&after1, &fault("fault-2", 61));
    assert_eq!(after2.member_machinery(&bloom, &workpiece("wp")).unwrap().rolls, 2);
    let cursor = after2.blooms.get(&bloom).unwrap().progress.get(&workpiece("wp")).copied().unwrap();
    assert_eq!(cursor.attempts, 1);
    assert_eq!(cursor.repair_rolls, 0);

    let (after3, d3) = step(&after2, &fault("fault-3", 62));
    assert!(matches!(d3.outcome, Outcome::MachineryWedged { stage: StageId::Verify, rolls: 3, budget: 3, .. },));
    assert!(
        !d3.effects.iter().any(|effect| matches!(effect, Decision::DispatchAttempt { .. })),
        "the terminal fault dispatches nothing further",
    );
    let record = after3.blooms.get(&bloom).unwrap();
    let wedge = record.wedged.get(&workpiece("wp")).expect("the machinery ceiling records a wedge");
    assert_eq!(wedge.stage, StageId::Verify);
    assert_eq!(wedge.evidence, digest(62));
    let cursor = record.progress.get(&workpiece("wp")).copied().unwrap();
    assert_eq!(cursor.attempts, 1, "even the terminal fault charges the work budget nothing");
    assert_eq!(cursor.repair_rolls, 0);
    assert_eq!(cursor.candidate, Some(captured));
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
    let (snapshot, _) = step(&snapshot, &weave_repaired(bloom, "weave-1", 40, 42, 43));
    let (snapshot, _) = step(&snapshot, &verify_passed(bloom, "v2", 42));
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

    // The re-armed cycle runs whole: a failing verdict repairs the weave again
    // instead of tripping the spent ceiling.
    let repaired = reduce(&rearmed, &fail("f3", 42, 52), &ResolvedConfigs::default(), &SpendWindow::default());
    assert!(matches!(repaired.outcome, Outcome::CompositionRewoven { attempt: 1, .. }));
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
        &SpendWindow::default(),
    );
    assert!(
        adopted.effects.iter().any(|e| matches!(e, Decision::DispatchAggregateReview { roll: 1, .. })),
        "adopting the contested park re-arms the review, not a member redispatch",
    );
    // #4977 — this park files nothing on the composition's findings channel: no
    // gate refused a tree here, a reviewer raised a question about one. So it is
    // the live park the adjudication door's marker arm still covers, now that
    // the three gate ceilings file their findings and adjudicate through the
    // channel like every other refusal.
    assert!(record.composition_findings.is_empty(), "a contested question is not a gate's refusal");
    assert!(matches!(
        reduce(&held, &adjudicated(bloom, "adj", vec![60]), &ResolvedConfigs::default(), &SpendWindow::default())
            .outcome,
        Outcome::FindingsAdjudicated { .. },
    ));
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
    let unknown = reduce(
        &base,
        &event("u", Fact::AdmitEvidence { bloom, evidence: study }),
        &ResolvedConfigs::default(),
        &SpendWindow::default(),
    );
    assert!(matches!(unknown.outcome, Outcome::AdmitEvidenceRejected(AdmitEvidenceError::UnknownOrInactiveBloom)));
    assert!(unknown.effects.is_empty());

    let (snapshot, _) = step(&base, &event("seal", Fact::Seal(spec)));

    // A resolution claim is bound to the integrate door, not the evidence log.
    let claim_ev = Evidence { subject: digest(70), kind: EvidenceKind::ResolutionClaim, detail: digest(80) };
    let mis_routed = reduce(
        &snapshot,
        &event("c", Fact::AdmitEvidence { bloom, evidence: claim_ev }),
        &ResolvedConfigs::default(),
        &SpendWindow::default(),
    );
    assert!(matches!(mis_routed.outcome, Outcome::AdmitEvidenceRejected(AdmitEvidenceError::EvidenceNotBound)));

    // An approval seals a member; it is not free-log evidence either.
    let approval = Evidence { subject: digest(70), kind: EvidenceKind::Approval, detail: digest(80) };
    let also_mis = reduce(
        &snapshot,
        &event("a", Fact::AdmitEvidence { bloom, evidence: approval }),
        &ResolvedConfigs::default(),
        &SpendWindow::default(),
    );
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
        workpiece: WorkpieceId(workpiece.into()),
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
        &SpendWindow::default(),
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
        &SpendWindow::default(),
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
        &SpendWindow::default(),
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
    let refused = reduce(
        &held,
        &event("obs", Fact::AdoptAnswer { bloom, answer: observed }),
        &ResolvedConfigs::default(),
        &SpendWindow::default(),
    );
    assert!(matches!(refused.outcome, Outcome::AdoptAnswerRejected(AdoptAnswerError::NotInstructionCapable)));

    // An author signature that adopts an unheld digest releases nothing.
    let wrong = answer_adopting(digest(222));
    let no_match = reduce(
        &held,
        &event("wrong", Fact::AdoptAnswer { bloom, answer: wrong }),
        &ResolvedConfigs::default(),
        &SpendWindow::default(),
    );
    assert!(matches!(no_match.outcome, Outcome::AdoptAnswerRejected(AdoptAnswerError::NoMatchingHold)));

    assert!(held.blooms.get(&bloom).unwrap().holds.contains(&question_digest), "a refused answer leaves the hold");
}

// ADR-0182 — the reducer takes its released question from the submitter's
// `parents`, in the submitter's order. `Fact::AdoptAnswer` carries no question
// field, so on a multi-hold bloom `parents` alone decides which hold falls, and
// a later parent that is also an open hold never gets a look in.
//
// Tripwire: this is the property that forces the host answer route to require
// `parents` to equal exactly the question its signature is bound to. As long as
// the reducer picks this way, a route that merely verified the signature — or
// that checked `parents.contains(&question)` — would let a submitter aim a
// genuine envelope at a hold the signature never named.
#[test]
fn the_released_hold_is_the_first_parent_in_submitter_order() {
    let base = Snapshot::new(digest(1));
    let spec = draft(1, vec![membership("first", 10), membership("second", 11)]).seal();
    let bloom = spec.id();
    let (mut snapshot, _) = step(&base, &event("seal", Fact::Seal(spec)));

    // A parked question raises one hold per member, so a two-hold bloom is the
    // ordinary shape rather than a contrived one.
    let first = parked_question("first").id();
    let second = parked_question("second").id();
    let park = |subject, detail| Evidence { subject, kind: EvidenceKind::Question, detail };
    (snapshot, _) = step(&snapshot, &event("p1", Fact::AdmitEvidence { bloom, evidence: park(digest(70), first) }));
    (snapshot, _) = step(&snapshot, &event("p2", Fact::AdmitEvidence { bloom, evidence: park(digest(71), second) }));

    let holds = &snapshot.blooms.get(&bloom).unwrap().holds;
    assert!(holds.contains(&first) && holds.contains(&second), "both questions are held");
    // The two orders have to disagree for the second case below to distinguish
    // them, so pin that rather than leaving it to the fixture's digests.
    assert!(first < second, "the fixture needs digest order to prefer `first`");

    // A sole parent names the hold that falls, and only that one.
    let (released, adopted) =
        step(&snapshot, &event("a1", Fact::AdoptAnswer { bloom, answer: answer_adopting(second) }));
    assert!(
        matches!(adopted.outcome, Outcome::AnswerAdopted { question: q, .. } if q == second),
        "the sole parent is the released question",
    );
    let holds = &released.blooms.get(&bloom).unwrap().holds;
    assert!(!holds.contains(&second) && holds.contains(&first), "only the named hold falls");

    // `[second, first]` *contains* `first` and still releases `second`: the scan
    // stops at the first parent that is an open hold, in the order the submitter
    // wrote them — not in digest order, which would have preferred `first`.
    let mut both = answer_adopting(second);
    both.parents = vec![second, first];
    let (_, adopted) = step(&snapshot, &event("a2", Fact::AdoptAnswer { bloom, answer: both }));
    assert!(
        matches!(adopted.outcome, Outcome::AnswerAdopted { question: q, .. } if q == second),
        "submitter order picks the winner, so membership alone cannot pin which hold falls",
    );
}

// m4 — an unknown bloom and a not-yet-resolved bloom each land-refuse with their
// own reason, never a misreported BaseMismatch.
#[test]
fn land_refusals_name_their_own_reason() {
    let base = Snapshot::new(digest(1));
    let spec = draft(1, vec![membership("wp", 10)]).seal();
    let bloom = spec.id();

    // Unknown bloom.
    let unknown = reduce(
        &base,
        &event("u", Fact::Land { bloom, new_head: digest(40) }),
        &ResolvedConfigs::default(),
        &SpendWindow::default(),
    );
    assert!(matches!(unknown.outcome, Outcome::LandRejected(LandError::UnknownBloom(_))));

    // Sealed but not resolved.
    let (after_seal, _) = step(&base, &event("seal", Fact::Seal(spec)));
    let not_resolved = reduce(
        &after_seal,
        &event("nr", Fact::Land { bloom, new_head: digest(40) }),
        &ResolvedConfigs::default(),
        &SpendWindow::default(),
    );
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

// A land frees the claim, not the work: resealing the same (workpiece,
// scope_revision) a landed bloom resolved is refused at the door, naming both.
// Tripwire: the claim release is the only gate, so an operator reseals already-
// done work and construct lanes burn retry refusing to fabricate it.
#[test]
fn seal_refuses_a_workpiece_a_landed_bloom_already_resolved() {
    let (snapshot, spec) = sealed_and_resolved(1, vec![membership("issue-4866", 10)], 40);
    let bloom = spec.id();
    let (landed, _) = step(&snapshot, &event("land", Fact::Land { bloom, new_head: digest(50) }));

    let again = draft(50, vec![membership("issue-4866", 10)]).seal();
    let refused =
        reduce(&landed, &event("reseal", Fact::Seal(again)), &ResolvedConfigs::default(), &SpendWindow::default());
    match &refused.outcome {
        Outcome::SealRejected(SealError::WorkpieceAlreadyLanded { workpiece: wp, bloom: landed_by }) => {
            assert_eq!(wp, &workpiece("issue-4866"));
            assert_eq!(*landed_by, bloom);
        }
        other => panic!("expected WorkpieceAlreadyLanded naming the workpiece and landing bloom, got {other:?}"),
    }
    assert!(refused.effects.is_empty(), "a refused seal claims nothing");
}

// The re-run escape is a fresh scope revision for the same workpiece id — that
// pair is not in the landed set, so a deliberate rework is a new approved plan,
// not a bypass flag on the request. Tripwire: the door keys on workpiece id
// alone and locks the workpiece out of every later bloom.
#[test]
fn a_fresh_scope_revision_is_the_rerun_escape() {
    let (snapshot, spec) = sealed_and_resolved(1, vec![membership("issue-4866", 10)], 40);
    let (landed, _) = step(&snapshot, &event("land", Fact::Land { bloom: spec.id(), new_head: digest(50) }));

    let rerun = draft(50, vec![membership("issue-4866", 11)]).seal();
    let decided =
        reduce(&landed, &event("rerun", Fact::Seal(rerun)), &ResolvedConfigs::default(), &SpendWindow::default());
    assert!(matches!(decided.outcome, Outcome::Sealed(_)), "a fresh scope revision reseals: {:?}", decided.outcome);
}

// Supersession carry stays admissible: a successor re-proposing the predecessor's
// own unlanded members is the salvage path and must not trip the landed set —
// that set contains only members of *landed* blooms, so this falls out without a
// special case. A fresh member a landed bloom already resolved is refused.
#[test]
fn supersede_refuses_a_fresh_landed_member_and_admits_the_predecessors_own() {
    let (snapshot, landed_spec) = sealed_and_resolved(1, vec![membership("done", 10)], 40);
    let landed_bloom = landed_spec.id();
    let (snapshot, _) = step(&snapshot, &event("land", Fact::Land { bloom: landed_bloom, new_head: digest(50) }));

    let predecessor_spec = draft(50, vec![membership("carried", 20)]).seal();
    let predecessor = predecessor_spec.id();
    let (snapshot, sealed) = step(&snapshot, &event("seal-pred", Fact::Seal(predecessor_spec)));
    assert!(matches!(sealed.outcome, Outcome::Sealed(_)));

    // Rebase so the carry-only successor is a distinct spec; same members at the
    // same revision on the current base would be a self-supersession.
    let snapshot = observing(&snapshot, 51);
    let carry_only = draft(51, vec![membership("carried", 20)]).seal();
    let carried = reduce(
        &snapshot,
        &event("carry", Fact::Supersede { predecessor, successor: carry_only }),
        &ResolvedConfigs::default(),
        &SpendWindow::default(),
    );
    assert!(
        matches!(carried.outcome, Outcome::Superseded { .. }),
        "the predecessor's own unlanded members stay admissible: {:?}",
        carried.outcome,
    );

    let adding_done = draft(50, vec![membership("carried", 20), membership("done", 10)]).seal();
    let refused = reduce(
        &snapshot,
        &event("add-done", Fact::Supersede { predecessor, successor: adding_done }),
        &ResolvedConfigs::default(),
        &SpendWindow::default(),
    );
    match &refused.outcome {
        Outcome::SupersedeRejected(SupersedeError::InvalidMember(SealError::WorkpieceAlreadyLanded {
            workpiece: wp,
            bloom,
        })) => {
            assert_eq!(wp, &workpiece("done"));
            assert_eq!(*bloom, landed_bloom);
        }
        other => panic!("expected InvalidMember(WorkpieceAlreadyLanded) for the fresh landed member, got {other:?}"),
    }
    assert!(refused.effects.is_empty(), "a refused supersession claims nothing");
}

// The landed set is read out of folded bloom records, not a side channel: the
// same journal folded twice from an empty snapshot refuses the same reseal for
// the same reason. Tripwire: a cache written outside `Snapshot::apply` makes
// replay miss the refusal the live fold made.
#[test]
fn a_replayed_journal_reproduces_the_landed_workpiece_refusal() {
    let spec = draft(1, vec![membership("issue-4866", 10)]).seal();
    let bloom = spec.id();
    let journal = [
        event("seal", Fact::Seal(spec)),
        event("integrate", Fact::Integrate { bloom, claim: claim("issue-4866", 10, 100) }),
        event("resolve", Fact::Resolve { bloom, tree: digest(40), head: digest(41), lineage: vec![] }),
        event(
            "review",
            Fact::AggregateReviewCompleted {
                bloom,
                passed: true,
                evidence: Evidence { subject: digest(40), kind: EvidenceKind::ReviewFinding, detail: digest(203) },
                implicated: vec![],
            },
        ),
        event("land", Fact::Land { bloom, new_head: digest(50) }),
    ];
    let fold = |events: &[Event]| events.iter().fold(Snapshot::new(digest(1)), |snapshot, ev| step(&snapshot, ev).0);
    let reseal = event("reseal", Fact::Seal(draft(50, vec![membership("issue-4866", 10)]).seal()));

    let live_refusal = reduce(&fold(&journal), &reseal, &ResolvedConfigs::default(), &SpendWindow::default());
    let replayed_refusal = reduce(&fold(&journal), &reseal, &ResolvedConfigs::default(), &SpendWindow::default());
    assert_eq!(
        replayed_refusal.outcome, live_refusal.outcome,
        "replay of the landed journal refuses the same reseal the live fold did",
    );
    match live_refusal.outcome {
        Outcome::SealRejected(SealError::WorkpieceAlreadyLanded { workpiece: wp, bloom: landed_by }) => {
            assert_eq!(wp, workpiece("issue-4866"));
            assert_eq!(landed_by, bloom);
        }
        other => panic!("expected WorkpieceAlreadyLanded from the folded journal, got {other:?}"),
    }
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
            assert_eq!(
                transformation.diff_base,
                Some(spec.base()),
                "the compiler narrows by the woven tree against the sealed base",
            );
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

fn verifier_set(failures: &[VerifyFailure]) -> VerifyFailureSet {
    failures.iter().copied().collect()
}

fn verify_failed(
    key: &str,
    bloom: BloomId,
    member: &str,
    subject: Digest,
    detail: u8,
    failed_verifiers: VerifyFailureSet,
) -> Event {
    event(
        key,
        Fact::VerifyFailed {
            bloom,
            workpiece: workpiece(member),
            evidence: Evidence { subject, kind: EvidenceKind::VerificationResult, detail: digest(detail) },
            failed_verifiers,
        },
    )
}

fn at_verify(member: &str) -> (Snapshot, BloomId) {
    let base = Snapshot::new(digest(1));
    let spec = draft(1, vec![membership(member, 10)]).seal();
    let bloom = spec.id();
    let (snapshot, _) = step(&base, &event("seal", Fact::Seal(spec)));
    let (snapshot, _) = step(
        &snapshot,
        &event(
            "c-pass",
            Fact::AttemptCompleted {
                bloom,
                workpiece: workpiece(member),
                stage: StageId::Construct,
                passed: true,
                evidence: attempt_evidence(),
                candidate: None,
            },
        ),
    );
    (snapshot, bloom)
}

// The plausible bug: a missing gate tool is a VerifyFailed that re-enters
// Refine and spends the member's budget — a host-provisioning gap buying a
// paid model lap (#5020).
#[test]
fn a_preflight_only_verify_holds_without_refine_or_budget() {
    let (snapshot, bloom) = at_verify("wp");
    let findings = "Verification did not run.\n\n- `jscpd` — npm install -g jscpd";
    let (after, decided) = step(
        &snapshot,
        &event(
            "preflight",
            Fact::VerifyHostFault {
                bloom,
                workpiece: workpiece("wp"),
                evidence: Evidence { subject: digest(10), kind: EvidenceKind::VerificationResult, detail: digest(71) },
                findings: findings.to_owned(),
            },
        ),
    );

    assert!(
        matches!(decided.outcome, Outcome::VerifyHostFaultHeld { .. }),
        "the host fault is its own outcome, not RefineReentered: {:?}",
        decided.outcome,
    );
    assert!(
        decided
            .effects
            .iter()
            .all(|effect| !matches!(effect, Decision::DispatchAttempt { stage: StageId::Refine, .. })),
        "a missing tool must not dispatch Refine: {:?}",
        decided.effects,
    );
    assert!(
        decided.effects.iter().any(|effect| matches!(effect, Decision::DeferDispatch { .. })),
        "the hold uses the operator-hold deferral shape",
    );

    let record = after.blooms.get(&bloom).unwrap();
    let cursor = record.progress.get(&workpiece("wp")).unwrap();
    assert_eq!(cursor.stage, StageId::Verify, "the member stays at Verify");
    assert_eq!(cursor.repair_rolls, 0, "a host fault spends no repair roll");
    assert_eq!(cursor.attempts, 1, "and it does not consume a Verify attempt");
    assert_eq!(
        record.host_faults.get(&workpiece("wp")).map(|hold| hold.findings.as_str()),
        Some(findings),
        "the missing tools are what the operator reads",
    );
}

// The plausible bug: fixing the host still needs an operator grant, so a
// provisioned executor sits idle until someone notices (#5020).
#[test]
fn resuming_a_host_fault_re_dispatches_verify_without_a_grant() {
    let (snapshot, bloom) = at_verify("wp");
    let (held, _) = step(
        &snapshot,
        &event(
            "preflight",
            Fact::VerifyHostFault {
                bloom,
                workpiece: workpiece("wp"),
                evidence: Evidence { subject: digest(10), kind: EvidenceKind::VerificationResult, detail: digest(71) },
                findings: "missing `jscpd`".into(),
            },
        ),
    );
    let (after, decided) = step(&held, &event("resume", Fact::ResumeHostFault { bloom, workpiece: workpiece("wp") }));

    assert!(
        matches!(decided.outcome, Outcome::HostFaultResumed { .. }),
        "the cadence resume is its own outcome: {:?}",
        decided.outcome,
    );
    match decided.effects.iter().find(|effect| matches!(effect, Decision::DispatchAttempt { .. })) {
        Some(Decision::DispatchAttempt { stage, .. }) => {
            assert_eq!(*stage, StageId::Verify, "the resume re-probes Verify, not Refine");
        }
        other => panic!("expected a Verify dispatch, got {other:?}"),
    }
    let record = after.blooms.get(&bloom).unwrap();
    assert!(record.host_faults.is_empty(), "the hold lifts");
    let cursor = record.progress.get(&workpiece("wp")).unwrap();
    assert_eq!(cursor.attempts, 1, "the resume spends no Verify attempt");
    assert_eq!(cursor.repair_rolls, 0, "and no repair roll");
}

#[test]
fn resuming_a_member_that_is_not_held_is_refused() {
    let (snapshot, bloom) = at_verify("wp");
    let (_, decided) = step(&snapshot, &event("resume", Fact::ResumeHostFault { bloom, workpiece: workpiece("wp") }));
    assert!(
        matches!(decided.outcome, Outcome::HostFaultRejected(HostFaultError::NotHeld(_))),
        "a resume without a hold is a refusal, not a free extra Verify: {:?}",
        decided.outcome,
    );
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
            progress: StageProgress {
                stage: StageId::Construct,
                attempts: 1,
                candidate: None,
                repair_rolls: 0,
                seen_verify_failures: VerifyFailureSet::EMPTY,
                fold_checkpoint: None,
                fold_conflict_evidence: None,
            },
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
            Fact::VerifyFailed {
                bloom,
                workpiece: workpiece("wp"),
                evidence: Evidence { subject: first.tree, kind: EvidenceKind::VerificationResult, detail: digest(80) },
                failed_verifiers: VerifyFailureSet::one(VerifyFailure::Clippy),
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

// ADR-0178 — first failures are forgiven per member; a verdict containing any
// repeated identity spends one roll for the whole set, and the terminal wedge
// reports only the identities that were repeated in that verdict.
#[test]
fn verifier_failure_accounting_uses_per_member_union_and_intersection() {
    let base = Snapshot::new(digest(1));
    let spec = draft(1, vec![membership("alpha", 10), membership("beta", 11)]).seal();
    let bloom = spec.id();
    let (snapshot, _) = step(&base, &event("seal", Fact::Seal(spec)));
    let pass = |key: &str, member: &str, stage: StageId, subject: Digest| {
        event(
            key,
            Fact::AttemptCompleted {
                bloom,
                workpiece: workpiece(member),
                stage,
                passed: true,
                evidence: Evidence { subject, kind: EvidenceKind::VerificationResult, detail: digest(90) },
                candidate: None,
            },
        )
    };

    let (mut snapshot, _) = step(&snapshot, &pass("alpha-construct", "alpha", StageId::Construct, digest(10)));
    let (next, _) = step(&snapshot, &pass("beta-construct", "beta", StageId::Construct, digest(11)));
    snapshot = next;

    // Three distinct first failures grow alpha's set without spending a roll.
    for (index, failure) in [VerifyFailure::Fmt, VerifyFailure::Clippy, VerifyFailure::Docs].into_iter().enumerate() {
        let (next, decided) = step(
            &snapshot,
            &verify_failed(
                &format!("alpha-new-{index}"),
                bloom,
                "alpha",
                digest(10),
                u8::try_from(80 + index).expect("three fixture failures fit in u8"),
                VerifyFailureSet::one(failure),
            ),
        );
        assert!(matches!(decided.outcome, Outcome::RefineReentered { rolls: 0, .. }));
        snapshot = step(&next, &pass(&format!("alpha-refine-{index}"), "alpha", StageId::Refine, digest(10))).0;
    }
    let alpha = snapshot.blooms.get(&bloom).unwrap().progress.get(&workpiece("alpha")).unwrap();
    assert_eq!(alpha.repair_rolls, 0);
    assert_eq!(
        alpha.seen_verify_failures,
        verifier_set(&[VerifyFailure::Fmt, VerifyFailure::Clippy, VerifyFailure::Docs]),
    );

    // Beta's first clippy failure is independently novel even though alpha has
    // already seen it.
    let (beta_failed, beta_decided) = step(
        &snapshot,
        &verify_failed("beta-clippy", bloom, "beta", digest(11), 84, VerifyFailureSet::one(VerifyFailure::Clippy)),
    );
    assert!(matches!(beta_decided.outcome, Outcome::RefineReentered { rolls: 0, .. }));
    assert_eq!(beta_failed.blooms.get(&bloom).unwrap().progress.get(&workpiece("beta")).unwrap().repair_rolls, 0,);

    // A mixed verdict spends exactly one roll because clippy is repeated while
    // test is new. The union remembers both.
    let mixed = verifier_set(&[VerifyFailure::Clippy, VerifyFailure::Test]);
    let (snapshot, mixed_decided) =
        step(&snapshot, &verify_failed("alpha-mixed", bloom, "alpha", digest(10), 85, mixed));
    assert!(matches!(mixed_decided.outcome, Outcome::RefineReentered { rolls: 1, .. }));
    let alpha = snapshot.blooms.get(&bloom).unwrap().progress.get(&workpiece("alpha")).unwrap();
    assert_eq!(alpha.repair_rolls, 1, "one umbrella verdict spends at most one roll");
    assert_eq!(
        alpha.seen_verify_failures,
        verifier_set(&[VerifyFailure::Fmt, VerifyFailure::Clippy, VerifyFailure::Docs, VerifyFailure::Test]),
    );
}

// A failing Verify that names no verifier is a gate that never rendered a
// verdict, not a candidate that failed one — the lane was killed mid-build, or
// cancelled on its execution limit, and the candidate it was dispatched against
// is untouched.
//
// Tripwire: an empty verifier set re-dispatches `Verify` against the same
// targets and spends a Verify attempt, while a set naming even one verifier
// still re-enters `Refine`. The two branches are asserted together because the
// bug is a routing choice, and a fix that sends *everything* back to Verify
// would pass either assertion alone. In production the unjudged branch bought
// three Refine laps, each asking a model to repair failures nobody observed
// against a worktree already checked out at the candidate.
#[test]
fn an_unjudged_verify_redispatches_verify_while_a_named_failure_still_refines() {
    let base = Snapshot::new(digest(1));
    let spec = draft(1, vec![membership("wp", 10)]).seal();
    let bloom = spec.id();
    let (snapshot, _) = step(&base, &event("seal", Fact::Seal(spec)));
    let candidate = CandidateRef { tree: digest(30), checkout: digest(31) };
    let (snapshot, _) = step(
        &snapshot,
        &event(
            "construct",
            Fact::AttemptCompleted {
                bloom,
                workpiece: workpiece("wp"),
                stage: StageId::Construct,
                passed: true,
                evidence: attempt_evidence(),
                candidate: Some(candidate),
            },
        ),
    );

    let (unjudged, decided) =
        step(&snapshot, &verify_failed("unjudged", bloom, "wp", candidate.tree, 81, VerifyFailureSet::EMPTY));
    assert!(
        matches!(decided.outcome, Outcome::AttemptRetried { stage: StageId::Verify, attempt: 2, .. }),
        "an unjudged verdict spends a Verify attempt, got {:?}",
        decided.outcome,
    );
    match decided.effects.iter().find(|effect| matches!(effect, Decision::DispatchAttempt { .. })) {
        Some(Decision::DispatchAttempt { stage, transformation, candidate: dispatched, .. }) => {
            assert_eq!(*stage, StageId::Verify, "the gate re-runs; the candidate is not sent for repair");
            assert_eq!(transformation.checkout, candidate.checkout);
            assert_eq!(*dispatched, Some(candidate.tree));
        }
        other => panic!("expected a DispatchAttempt, got {other:?}"),
    }
    let progress = unjudged.blooms.get(&bloom).unwrap().progress.get(&workpiece("wp")).unwrap();
    assert_eq!(progress.stage, StageId::Verify);
    assert_eq!(progress.attempts, 2);
    assert_eq!(progress.candidate, Some(candidate), "the unjudged candidate is untouched");
    assert_eq!(progress.repair_rolls, 0, "no repair roll is spent on a gate that never ran");
    assert_eq!(
        progress.seen_verify_failures,
        VerifyFailureSet::EMPTY,
        "an attempt that named no verifier adds none to the seen set",
    );

    let (refined, named) = step(
        &unjudged,
        &verify_failed("named", bloom, "wp", candidate.tree, 82, VerifyFailureSet::one(VerifyFailure::Clippy)),
    );
    assert!(
        matches!(named.outcome, Outcome::RefineReentered { .. }),
        "a verdict naming a verifier still repairs, got {:?}",
        named.outcome,
    );
    let progress = refined.blooms.get(&bloom).unwrap().progress.get(&workpiece("wp")).unwrap();
    assert_eq!(progress.stage, StageId::Refine);
    assert_eq!(progress.seen_verify_failures, VerifyFailureSet::one(VerifyFailure::Clippy));
}

// Tripwire: an unjudged candidate that exhausts the Verify budget wedges there
// instead of falling through to a repair lap. Without this, the terminal
// unjudged verdict would be the one that finally reached `Refine` — the same
// wrong routing, arrived at three attempts later — and the operator would read a
// repair failure rather than "the gate never answered".
#[test]
fn an_unjudged_verify_wedges_once_the_verify_budget_is_spent() {
    let base = Snapshot::new(digest(1));
    let spec = draft(1, vec![membership("wp", 10)]).seal();
    let bloom = spec.id();
    let budget = StageCatalog::binding_of(StageId::Verify).retry_budget;
    let (snapshot, _) = step(&base, &event("seal", Fact::Seal(spec)));
    let candidate = CandidateRef { tree: digest(30), checkout: digest(31) };
    let (mut snapshot, _) = step(
        &snapshot,
        &event(
            "construct",
            Fact::AttemptCompleted {
                bloom,
                workpiece: workpiece("wp"),
                stage: StageId::Construct,
                passed: true,
                evidence: attempt_evidence(),
                candidate: Some(candidate),
            },
        ),
    );

    for attempt in 1..budget {
        let (next, decided) = step(
            &snapshot,
            &verify_failed(&format!("unjudged-{attempt}"), bloom, "wp", candidate.tree, 81, VerifyFailureSet::EMPTY),
        );
        assert!(matches!(decided.outcome, Outcome::AttemptRetried { stage: StageId::Verify, .. }));
        snapshot = next;
    }

    let (wedged, decided) =
        step(&snapshot, &verify_failed("unjudged-last", bloom, "wp", candidate.tree, 82, VerifyFailureSet::EMPTY));
    assert!(
        matches!(
            decided.outcome,
            Outcome::AttemptWedged { stage: StageId::Verify, repeated_verifiers, .. }
                if repeated_verifiers == VerifyFailureSet::EMPTY
        ),
        "the terminal unjudged verdict wedges at Verify, got {:?}",
        decided.outcome,
    );
    assert!(
        !decided.effects.iter().any(|effect| matches!(effect, Decision::DispatchAttempt { .. })),
        "a wedged member is dispatched nothing — least of all a Refine lap",
    );
    let record = wedged.blooms.get(&bloom).unwrap();
    let wedge = record.wedged.get(&workpiece("wp")).expect("the member is recorded as wedged");
    assert_eq!(wedge.stage, StageId::Verify);
    assert_eq!(wedge.evidence, digest(82), "the wedge carries the unjudged attempt's own evidence");
    assert_eq!(
        wedge.repeated_verifiers,
        VerifyFailureSet::EMPTY,
        "an empty set on a Verify wedge is what says the gate never answered",
    );
}

#[test]
fn verify_failed_refuses_invalid_state_set_and_binding_without_effects() {
    let base = Snapshot::new(digest(1));
    let spec = draft(1, vec![membership("wp", 10)]).seal();
    let bloom = spec.id();
    let (snapshot, _) = step(&base, &event("seal", Fact::Seal(spec)));
    let (mut snapshot, _) = step(
        &snapshot,
        &event(
            "construct",
            Fact::AttemptCompleted {
                bloom,
                workpiece: workpiece("wp"),
                stage: StageId::Construct,
                passed: true,
                evidence: attempt_evidence(),
                candidate: None,
            },
        ),
    );

    // Tripwire: an empty verifier set re-dispatches Verify, so the binding check
    // has to come first — otherwise anyone able to fabricate a `VerifyFailed`
    // naming no verifier and no real subject could re-dispatch a member's lane at
    // will. This is the one refusal ordering the unjudged path can break.
    let unbound_and_empty = reduce(
        &snapshot,
        &verify_failed("unbound-empty", bloom, "wp", digest(99), 81, VerifyFailureSet::EMPTY),
        &ResolvedConfigs::default(),
        &SpendWindow::default(),
    );
    assert!(matches!(
        unbound_and_empty.outcome,
        Outcome::VerifyFailedRejected(VerifyFailedError::EvidenceNotBound { .. })
    ));
    assert!(unbound_and_empty.effects.is_empty());

    let unbound = reduce(
        &snapshot,
        &verify_failed("unbound", bloom, "wp", digest(99), 82, VerifyFailureSet::one(VerifyFailure::Fmt)),
        &ResolvedConfigs::default(),
        &SpendWindow::default(),
    );
    assert!(matches!(
        unbound.outcome,
        Outcome::VerifyFailedRejected(VerifyFailedError::EvidenceNotBound {
            expected,
            got,
        }) if expected == digest(10) && got == digest(99)
    ));
    assert!(unbound.effects.is_empty());

    let stranger = reduce(
        &snapshot,
        &verify_failed("stranger", bloom, "ghost", digest(10), 83, VerifyFailureSet::one(VerifyFailure::Fmt)),
        &ResolvedConfigs::default(),
        &SpendWindow::default(),
    );
    assert!(matches!(stranger.outcome, Outcome::VerifyFailedRejected(VerifyFailedError::NotAMember(_))));

    snapshot.blooms.get_mut(&bloom).unwrap().progress.remove(&workpiece("wp"));
    let no_cursor = reduce(
        &snapshot,
        &verify_failed("no-cursor", bloom, "wp", digest(10), 84, VerifyFailureSet::one(VerifyFailure::Fmt)),
        &ResolvedConfigs::default(),
        &SpendWindow::default(),
    );
    assert!(matches!(no_cursor.outcome, Outcome::VerifyFailedRejected(VerifyFailedError::NotDispatched(_))));

    let unknown = reduce(
        &base,
        &verify_failed("unknown", bloom, "wp", digest(10), 84, VerifyFailureSet::one(VerifyFailure::Fmt)),
        &ResolvedConfigs::default(),
        &SpendWindow::default(),
    );
    assert!(matches!(unknown.outcome, Outcome::VerifyFailedRejected(VerifyFailedError::UnknownOrInactiveBloom)));
}

#[test]
fn repeated_verify_failure_wedges_at_budget_with_exact_terminal_set() {
    let base = Snapshot::new(digest(1));
    let spec = draft(1, vec![membership("wp", 10)]).seal();
    let bloom = spec.id();
    let (snapshot, _) = step(&base, &event("seal", Fact::Seal(spec)));
    let pass = |key: &str, stage: StageId| {
        event(
            key,
            Fact::AttemptCompleted {
                bloom,
                workpiece: workpiece("wp"),
                stage,
                passed: true,
                evidence: Evidence { subject: digest(10), kind: EvidenceKind::VerificationResult, detail: digest(90) },
                candidate: None,
            },
        )
    };
    let (mut snapshot, _) = step(&snapshot, &pass("construct", StageId::Construct));

    // First clippy is novel; three later verdicts containing clippy spend the
    // compiled Verify budget. The terminal verdict also contains new docs, which
    // is remembered but is not named as responsible for the terminal roll.
    for index in 0u8..3 {
        let (next, decided) = step(
            &snapshot,
            &verify_failed(
                &format!("clippy-{index}"),
                bloom,
                "wp",
                digest(10),
                80 + index,
                VerifyFailureSet::one(VerifyFailure::Clippy),
            ),
        );
        assert!(matches!(decided.outcome, Outcome::RefineReentered { rolls, .. } if rolls == u32::from(index)));
        snapshot = step(&next, &pass(&format!("refine-{index}"), StageId::Refine)).0;
    }

    let terminal_set = verifier_set(&[VerifyFailure::Clippy, VerifyFailure::Docs]);
    let (wedged, decided) = step(&snapshot, &verify_failed("terminal", bloom, "wp", digest(10), 89, terminal_set));
    assert!(matches!(
        decided.outcome,
        Outcome::AttemptWedged {
            stage: StageId::Verify,
            repeated_verifiers,
            ..
        } if repeated_verifiers == VerifyFailureSet::one(VerifyFailure::Clippy)
    ));
    assert!(!decided.effects.iter().any(|effect| matches!(effect, Decision::DispatchAttempt { .. })));
    let record = wedged.blooms.get(&bloom).unwrap();
    let wedge = record.wedged.get(&workpiece("wp")).unwrap();
    assert_eq!(wedge.evidence, digest(89));
    assert_eq!(wedge.repeated_verifiers, VerifyFailureSet::one(VerifyFailure::Clippy));
    let progress = record.progress.get(&workpiece("wp")).unwrap();
    assert_eq!(progress.repair_rolls, 3);
    assert!(progress.seen_verify_failures.contains(VerifyFailure::Docs), "the terminal union is durable");
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

    let completion = |key: &str, stage: StageId, candidate: Option<CandidateRef>| {
        event(
            key,
            Fact::AttemptCompleted {
                bloom,
                workpiece: workpiece("wp"),
                stage,
                passed: true,
                evidence: attempt_evidence(),
                candidate,
            },
        )
    };

    // Construct passes with a capture. The first clippy failure is forgiven;
    // three repeats spend the whole repair ceiling and wedge holding that
    // candidate and seen-history.
    let captured = CandidateRef { tree: digest(21), checkout: digest(22) };
    let (mut snapshot, _) = step(&snapshot, &completion("c-pass", StageId::Construct, Some(captured)));
    for index in 0..3 {
        snapshot = step(
            &snapshot,
            &verify_failed(
                &format!("v-fail-{index}"),
                bloom,
                "wp",
                captured.tree,
                81 + index,
                VerifyFailureSet::one(VerifyFailure::Clippy),
            ),
        )
        .0;
        snapshot = step(&snapshot, &completion(&format!("refine-pass-{index}"), StageId::Refine, None)).0;
    }
    let (wedged, d) = step(
        &snapshot,
        &verify_failed("v-fail-terminal", bloom, "wp", captured.tree, 89, VerifyFailureSet::one(VerifyFailure::Clippy)),
    );
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
    assert!(progress.seen_verify_failures.contains(VerifyFailure::Clippy), "the grant preserves seen history");
    let (snapshot, _) = step(&granted, &completion("refine-pass-granted", StageId::Refine, None));
    let (_, d) = step(
        &snapshot,
        &verify_failed(
            "v-fail-after-grant",
            bloom,
            "wp",
            captured.tree,
            90,
            VerifyFailureSet::one(VerifyFailure::Clippy),
        ),
    );
    assert!(matches!(d.outcome, Outcome::AttemptWedged { .. }), "one granted roll, then wedged again");
}

#[test]
fn a_successor_starts_with_fresh_verifier_history() {
    let base = Snapshot::new(digest(1));
    let predecessor_spec = draft(1, vec![membership("wp", 10)]).seal();
    let predecessor = predecessor_spec.id();
    let (snapshot, _) = step(&base, &event("seal", Fact::Seal(predecessor_spec)));
    let construct = event(
        "construct",
        Fact::AttemptCompleted {
            bloom: predecessor,
            workpiece: workpiece("wp"),
            stage: StageId::Construct,
            passed: true,
            evidence: attempt_evidence(),
            candidate: None,
        },
    );
    let (snapshot, _) = step(&snapshot, &construct);
    let (snapshot, _) = step(
        &snapshot,
        &verify_failed(
            "predecessor-clippy",
            predecessor,
            "wp",
            digest(10),
            81,
            VerifyFailureSet::one(VerifyFailure::Clippy),
        ),
    );
    assert!(
        snapshot
            .blooms
            .get(&predecessor)
            .unwrap()
            .progress
            .get(&workpiece("wp"))
            .unwrap()
            .seen_verify_failures
            .contains(VerifyFailure::Clippy),
    );

    let successor_spec = draft(1, vec![membership("wp", 11)]).seal();
    let successor = successor_spec.id();
    let (snapshot, decided) =
        step(&snapshot, &event("supersede", Fact::Supersede { predecessor, successor: successor_spec }));
    assert!(matches!(decided.outcome, Outcome::Superseded { successor: id, .. } if id == successor));
    let cursor = snapshot.blooms.get(&successor).unwrap().progress.get(&workpiece("wp")).unwrap();
    assert_eq!(cursor.stage, StageId::Construct);
    assert!(cursor.seen_verify_failures.is_empty(), "successor seal owns a fresh per-member set");
}

/// The graded retry actual for one bloom (ADR-0180). Read through the public
/// `grade` rather than off the record, so these cases pin the number an operator
/// sees rather than the field behind it. No study artifact resolves — the retry
/// axis does not read them.
fn graded_retries(snapshot: &Snapshot, bloom: &BloomId) -> u32 {
    grade(snapshot, |_: &Digest| None)
        .blooms
        .iter()
        .find(|graded| graded.bloom == *bloom)
        .expect("the snapshot grades every bloom it holds")
        .actual_retries
}

/// One member's `Construct` slot — the ledger key the entry dispatch and every
/// in-budget `Construct` retry land on.
fn construct_slot(member: &str) -> DispatchKey {
    DispatchKey::Member { workpiece: workpiece(member), stage: StageId::Construct }
}

// ADR-0180 — a granted attempt is a retry, and the grant does not lower the
// ledger. `Fact::GrantAttempts` deliberately rewrites the member's headroom
// cursors downward to leave exactly the allowance the operator bought, so a
// retry axis derived from `StageProgress::attempts` would report *fewer* retries
// after paying for more execution. Tripwire for reading the grade out of a
// budget cursor rather than the dispatch ledger.
#[test]
fn a_granted_attempt_counts_a_retry_the_grant_cannot_lower() {
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
    let (snapshot, _) = step(&snapshot, &fail("c-fail-1"));
    let (wedged, _) = step(&snapshot, &fail("c-fail-2"));
    let wedged_record = wedged.blooms.get(&bloom).unwrap();
    assert_eq!(wedged_record.dispatches.get(&construct_slot("wp")), Some(&2), "the entry dispatch plus one retry");
    assert_eq!(graded_retries(&wedged, &bloom), 1);

    let grant = event(
        "grant",
        Fact::GrantAttempts { bloom, workpiece: workpiece("wp"), stage: StageId::Construct, attempts: 2 },
    );
    let (granted, _) = step(&wedged, &grant);

    let record = granted.blooms.get(&bloom).unwrap();
    assert_eq!(
        record.progress.get(&workpiece("wp")).unwrap().attempts,
        1,
        "the grant rewrites the headroom cursor downward to leave the allowance it bought",
    );
    assert_eq!(record.dispatches.get(&construct_slot("wp")), Some(&3), "while the ledger records the third dispatch");
    assert_eq!(graded_retries(&granted, &bloom), 2, "so the grade rises with the spend rather than falling with it");
}

// ADR-0180 — only a successor resets the ledger. A successor is a distinct id
// with its own record and its own sealed forecast, so it is graded against what
// it dispatched, while the predecessor keeps its own history rather than having
// it carried forward or erased.
#[test]
fn a_successor_starts_a_fresh_ledger_and_its_predecessor_keeps_one() {
    let base = Snapshot::new(digest(1));
    let predecessor_spec = draft(1, vec![membership("wp", 10)]).seal();
    let predecessor = predecessor_spec.id();
    let (snapshot, _) = step(&base, &event("seal", Fact::Seal(predecessor_spec)));
    let (snapshot, _) = step(
        &snapshot,
        &event(
            "c-fail",
            Fact::AttemptCompleted {
                bloom: predecessor,
                workpiece: workpiece("wp"),
                stage: StageId::Construct,
                passed: false,
                evidence: attempt_evidence(),
                candidate: None,
            },
        ),
    );
    assert_eq!(graded_retries(&snapshot, &predecessor), 1, "the predecessor re-dispatched its member once");

    let successor_spec = draft(1, vec![membership("wp", 11)]).seal();
    let successor = successor_spec.id();
    let (superseded, _) =
        step(&snapshot, &event("supersede", Fact::Supersede { predecessor, successor: successor_spec }));

    assert_eq!(
        superseded.blooms.get(&successor).unwrap().dispatches.get(&construct_slot("wp")),
        Some(&1),
        "the successor holds only the entry dispatch its own supersession decided",
    );
    assert_eq!(graded_retries(&superseded, &successor), 0, "so it is graded against what it dispatched");
    assert_eq!(graded_retries(&superseded, &predecessor), 1, "and the predecessor's own spend still stands");
}

// ADR-0180 — an owner's answer that re-arms a bloom-scope review park buys a
// real review execution, so it counts, even though the answer resets
// `aggregate_rolls` to zero to re-arm the budget. A retry axis derived from that
// cursor would report the re-armed cycle as *fewer* retries than the spent cycle
// it replaced, which is the second shape of the headroom-versus-history
// confusion the ledger exists to keep apart.
#[test]
fn a_rearmed_review_cycle_counts_a_retry_though_the_roll_cursor_resets() {
    let base = Snapshot::new(digest(1));
    let spec = draft(1, vec![membership("wp", 10)]).seal();
    let bloom = spec.id();
    let (snapshot, _) = step(&base, &event("seal", Fact::Seal(spec)));

    let verdict = |key: &str, subject: u8, detail: u8| {
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

    // Two whole review cycles: each folds, passes the compiler, reaches the
    // critic, and comes back failing. The second failure parks the bloom at the
    // two-pass ceiling with the review slot dispatched twice.
    let (snapshot, _) = step(&snapshot, &event("i1", Fact::Integrate { bloom, claim: claim("wp", 10, 100) }));
    let (snapshot, _) =
        step(&snapshot, &event("r1", Fact::Resolve { bloom, tree: digest(40), head: digest(41), lineage: vec![] }));
    let (snapshot, _) = step(&snapshot, &verify_passed(bloom, "v1", 40));
    let (snapshot, _) = step(&snapshot, &verdict("f1", 40, 50));
    let (snapshot, _) = step(&snapshot, &event("i2", Fact::Integrate { bloom, claim: claim("wp", 10, 101) }));
    let (snapshot, _) =
        step(&snapshot, &event("r2", Fact::Resolve { bloom, tree: digest(42), head: digest(43), lineage: vec![] }));
    let (snapshot, _) = step(&snapshot, &verify_passed(bloom, "v2", 42));
    let (parked, decided) = step(&snapshot, &verdict("f2", 42, 51));
    assert!(matches!(decided.outcome, Outcome::AggregateReviewParked { rolls: 2, .. }));

    let review_slot = DispatchKey::Bloom { stage: StageId::AggregateReview };
    let parked_record = parked.blooms.get(&bloom).unwrap();
    assert_eq!(parked_record.dispatches.get(&review_slot), Some(&2), "two spent review passes");
    assert_eq!(parked_record.aggregate_rolls, 2, "and a roll cursor at the ceiling");
    let parked_retries = graded_retries(&parked, &bloom);

    let (rearmed, _) = step(&parked, &event("ans", Fact::AdoptAnswer { bloom, answer: answer_adopting(digest(51)) }));

    let record = rearmed.blooms.get(&bloom).unwrap();
    assert_eq!(record.aggregate_rolls, 0, "the answer resets the roll cursor to re-arm the budget");
    assert_eq!(record.dispatches.get(&review_slot), Some(&3), "while the ledger records the review it dispatched");
    assert_eq!(
        graded_retries(&rearmed, &bloom),
        parked_retries + 1,
        "so the owner-bought cycle adds exactly one retry rather than erasing two",
    );
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

    // The sealed catalog is the whole retry authority (ADR-0177), so
    // `Construct`'s retry budget of 2 is the ceiling and nothing narrows it
    // further. Zero is refused on the same door: it would dispatch an attempt
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

fn verify_executor_fault(bloom: BloomId, key: &str, detail: u8) -> Event {
    event(
        key,
        Fact::MemberExecutorFault {
            bloom,
            workpiece: workpiece("wp"),
            stage: StageId::Verify,
            evidence: Evidence { subject: digest(21), kind: EvidenceKind::ExecutorFault, detail: digest(detail) },
        },
    )
}

/// Seal a two-member bloom, capture a Construct candidate on `wp`, then spend
/// the Verify machinery ceiling so that member is wedged on the host-fault axis.
fn machinery_wedged_at_verify() -> (Snapshot, BloomId, CandidateRef) {
    let spec = draft(1, vec![membership("wp", 10), membership("other", 11)]).seal();
    let bloom = spec.id();
    let captured = CandidateRef { tree: digest(21), checkout: digest(22) };
    let (snapshot, _) = step(&Snapshot::new(digest(1)), &event("seal", Fact::Seal(spec)));
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
                candidate: Some(captured),
            },
        ),
    );
    let (snapshot, _) = step(&snapshot, &verify_executor_fault(bloom, "fault-1", 60));
    let (snapshot, _) = step(&snapshot, &verify_executor_fault(bloom, "fault-2", 61));
    let (wedged, _) = step(&snapshot, &verify_executor_fault(bloom, "fault-3", 62));
    (wedged, bloom, captured)
}

// #5090 — an operator-selected machinery grant is a bounded same-stage batch
// on the host-fault axis. The plausible bugs: treating a machinery Verify
// wedge as a work wedge (resume at Refine, spend repair rolls), charging
// ordinary attempts, moving the candidate, or dispatching more than N times.
#[test]
fn a_machinery_grant_reruns_the_wedged_stage_for_exactly_n_dispatches() {
    let (wedged, bloom, captured) = machinery_wedged_at_verify();
    assert!(matches!(
        wedged.blooms.get(&bloom).unwrap().wedged.get(&workpiece("wp")),
        Some(wedge) if wedge.stage == StageId::Verify
    ));
    let record = wedged.blooms.get(&bloom).unwrap();
    let other_before = record.progress.get(&workpiece("other")).copied();
    let claims_before = record.claims.clone();
    let before = record.progress.get(&workpiece("wp")).copied().unwrap();
    assert_eq!(before.attempts, 1);
    assert_eq!(before.repair_rolls, 0);
    assert_eq!(before.candidate, Some(captured));

    let grant =
        event("grant", Fact::GrantAttempts { bloom, workpiece: workpiece("wp"), stage: StageId::Verify, attempts: 2 });
    let (granted, decisions) = step(&wedged, &grant);
    assert!(
        matches!(&decisions.outcome, Outcome::AttemptsGranted { resumes_at: StageId::Verify, attempts: 2, .. }),
        "a machinery Verify grant resumes in place, got {:?}",
        decisions.outcome,
    );
    match decisions.effects.iter().find(|effect| matches!(effect, Decision::DispatchAttempt { .. })) {
        Some(Decision::DispatchAttempt { stage, candidate, transformation, .. }) => {
            assert_eq!(*stage, StageId::Verify, "the grant re-runs the wedged stage");
            assert_eq!(*candidate, Some(captured.tree), "against the same candidate");
            assert_eq!(transformation.checkout, captured.checkout);
        }
        other => panic!("expected a Verify DispatchAttempt, got {other:?}"),
    }
    let record = granted.blooms.get(&bloom).unwrap();
    assert!(!record.wedged.contains_key(&workpiece("wp")), "a cursor that moves clears the wedge");
    let cursor = record.progress.get(&workpiece("wp")).copied().unwrap();
    assert_eq!(cursor.stage, StageId::Verify);
    assert_eq!(cursor.attempts, 1, "ordinary attempts do not move");
    assert_eq!(cursor.repair_rolls, 0, "repair rolls do not move");
    assert_eq!(cursor.candidate, Some(captured), "the candidate does not move");
    assert_eq!(cursor.seen_verify_failures, before.seen_verify_failures);
    assert_eq!(record.claims, claims_before, "resolved work does not move");
    assert_eq!(record.progress.get(&workpiece("other")).copied(), other_before, "a sibling's cursor does not move");
    assert_eq!(granted.member_machinery(&bloom, &workpiece("wp")).unwrap().rolls, 1);

    let (after1, d1) = step(&granted, &verify_executor_fault(bloom, "fault-g1", 63));
    assert!(
        matches!(d1.outcome, Outcome::MachineryRetried { stage: StageId::Verify, rolls: 2, budget: 3, .. }),
        "got {:?}",
        d1.outcome,
    );
    assert_eq!(
        d1.effects.iter().filter(|effect| matches!(effect, Decision::DispatchAttempt { .. })).count(),
        1,
        "the first spent fault of a 2-batch still redispatches",
    );
    let cursor = after1.blooms.get(&bloom).unwrap().progress.get(&workpiece("wp")).copied().unwrap();
    assert_eq!(cursor.attempts, 1);
    assert_eq!(cursor.repair_rolls, 0);
    assert_eq!(cursor.candidate, Some(captured));

    let (rewedged, d2) = step(&after1, &verify_executor_fault(bloom, "fault-g2", 64));
    assert!(
        matches!(d2.outcome, Outcome::MachineryWedged { stage: StageId::Verify, rolls: 3, budget: 3, .. }),
        "got {:?}",
        d2.outcome,
    );
    assert!(
        !d2.effects.iter().any(|effect| matches!(effect, Decision::DispatchAttempt { .. })),
        "the Nth fault of an N-batch wedges instead of dispatching",
    );
    assert!(rewedged.blooms.get(&bloom).unwrap().wedged.contains_key(&workpiece("wp")));
}

// #5090 — a repeated identical grant is a no-op unless the operator mints a
// fresh idempotency key, and a grant never admits a second worker on a
// running member or a resolved bloom.
#[test]
fn a_repeated_machinery_grant_needs_a_fresh_key_and_refuses_live_work() {
    let (wedged, bloom, _) = machinery_wedged_at_verify();
    let grant =
        event("grant", Fact::GrantAttempts { bloom, workpiece: workpiece("wp"), stage: StageId::Verify, attempts: 2 });
    let (granted, _) = step(&wedged, &grant);
    let (after1, _) = step(&granted, &verify_executor_fault(bloom, "fault-g1", 63));
    let (rewedged, _) = step(&after1, &verify_executor_fault(bloom, "fault-g2", 64));

    let (dup, d_dup) = step(&rewedged, &grant);
    assert!(matches!(d_dup.outcome, Outcome::Duplicate), "the same key cannot buy a second batch");
    assert!(dup.blooms.get(&bloom).unwrap().wedged.contains_key(&workpiece("wp")));

    let grant_again = event(
        "grant-2",
        Fact::GrantAttempts { bloom, workpiece: workpiece("wp"), stage: StageId::Verify, attempts: 2 },
    );
    let (granted_again, d_again) = step(&rewedged, &grant_again);
    assert!(
        matches!(&d_again.outcome, Outcome::AttemptsGranted { resumes_at: StageId::Verify, attempts: 2, .. }),
        "a fresh key authorizes another batch, got {:?}",
        d_again.outcome,
    );
    assert_eq!(d_again.effects.iter().filter(|effect| matches!(effect, Decision::DispatchAttempt { .. })).count(), 1);

    let (_, d_running) = step(
        &granted_again,
        &event(
            "grant-while-running",
            Fact::GrantAttempts { bloom, workpiece: workpiece("wp"), stage: StageId::Verify, attempts: 1 },
        ),
    );
    assert!(
        matches!(
            &d_running.outcome,
            Outcome::GrantAttemptsRejected(GrantAttemptsError::NotWedged(wp)) if *wp == workpiece("wp")
        ),
        "a running member is never redispatched, got {:?}",
        d_running.outcome,
    );

    let (resolved, spec) = sealed_and_resolved(2, vec![membership("done", 12)], 40);
    let resolved_bloom = spec.id();
    let (_, d_resolved) = step(
        &resolved,
        &event(
            "grant-resolved",
            Fact::GrantAttempts {
                bloom: resolved_bloom,
                workpiece: workpiece("done"),
                stage: StageId::Verify,
                attempts: 1,
            },
        ),
    );
    assert!(
        matches!(d_resolved.outcome, Outcome::GrantAttemptsRejected(GrantAttemptsError::UnknownOrInactiveBloom)),
        "a resolved bloom is not grantable, got {:?}",
        d_resolved.outcome,
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

    // The cursor is at Construct; a typed Verify failure is a stage mismatch.
    let mismatch = reduce(
        &snapshot,
        &verify_failed("m", bloom, "wp", digest(10), 80, VerifyFailureSet::one(VerifyFailure::Clippy)),
        &ResolvedConfigs::default(),
        &SpendWindow::default(),
    );
    assert!(matches!(
        mismatch.outcome,
        Outcome::VerifyFailedRejected(VerifyFailedError::StageMismatch { expected: StageId::Construct }),
    ));

    // A passing terminal Verify never completes here — it integrates through
    // Fact::Integrate.
    let terminal = reduce(
        &snapshot,
        &completion("t", "wp", StageId::Verify),
        &ResolvedConfigs::default(),
        &SpendWindow::default(),
    );
    assert!(matches!(
        terminal.outcome,
        Outcome::AttemptCompletedRejected(AttemptCompletedError::TerminalStage(StageId::Verify)),
    ));

    // A passing Review is off the dispatched line entirely (ADR-0153) and reads
    // as the same terminal mis-route.
    let off_line = reduce(
        &snapshot,
        &completion("r", "wp", StageId::Review),
        &ResolvedConfigs::default(),
        &SpendWindow::default(),
    );
    assert!(matches!(
        off_line.outcome,
        Outcome::AttemptCompletedRejected(AttemptCompletedError::TerminalStage(StageId::Review)),
    ));

    // A non-member workpiece.
    let stranger = reduce(
        &snapshot,
        &completion("n", "ghost", StageId::Construct),
        &ResolvedConfigs::default(),
        &SpendWindow::default(),
    );
    assert!(matches!(stranger.outcome, Outcome::AttemptCompletedRejected(AttemptCompletedError::NotAMember(_))));

    // An unknown bloom (nothing sealed on `base`).
    let unknown =
        reduce(&base, &completion("u", "wp", StageId::Construct), &ResolvedConfigs::default(), &SpendWindow::default());
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
            let ev = if cursor.stage == StageId::Verify && !passed {
                verify_failed(
                    &format!("a-{i}"),
                    bloom,
                    "wp",
                    digest(10),
                    80,
                    VerifyFailureSet::one(VerifyFailure::Clippy),
                )
            } else {
                event(
                    &format!("a-{i}"),
                    Fact::AttemptCompleted {
                        bloom,
                        workpiece: workpiece("wp"),
                        stage: cursor.stage,
                        passed,
                        evidence: attempt_evidence(),
                        candidate: None,
                    },
                )
            };
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
                Outcome::AttemptRetried { .. } => {
                    prop_assert_eq!(new.stage, cursor.stage, "a retry holds the cursor in place");
                }
                Outcome::AttemptWedged { .. } => {
                    prop_assert_eq!(new.stage, cursor.stage, "a wedge holds the cursor in place");
                    break;
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
        SealError, Snapshot, SpendWindow, Unproducible, reduce,
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
            &SpendWindow::default(),
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
        Decision, Evidence, EvidenceKind, Fact, Harness, Outcome, ReasoningEffort, SpendWindow, StageCatalog, StageId,
        ToolPolicy, VerifyFailure, VerifyFailureSet, reduce,
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
        let decided = reduce(
            &Snapshot::new(digest(1)),
            &event("seal", Fact::Seal(draft.seal())),
            &configs,
            &SpendWindow::default(),
        );
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
        let decided = reduce(
            &Snapshot::new(digest(1)),
            &event("seal", Fact::Seal(draft.seal())),
            &configs,
            &SpendWindow::default(),
        );
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

        let spec = draft.seal();
        let bloom = spec.id();
        let seal = event("seal", Fact::Seal(spec));
        let decided = reduce(&Snapshot::new(digest(1)), &seal, &configs, &SpendWindow::default());
        assert!(matches!(decided.outcome, Outcome::Sealed(_)), "the authored catalog seals: {:?}", decided.outcome);

        let (construct_command, construct_profile) = decided
            .effects
            .iter()
            .find_map(|effect| match effect {
                Decision::DispatchAttempt { profile, stage: StageId::Construct, transformation, .. } => {
                    Some((transformation.command.as_str(), profile))
                }
                _ => None,
            })
            .expect("sealing dispatches the entry stage");

        let mut snapshot = Snapshot::new(digest(1)).apply(&seal, &decided, &configs);
        let construct_pass = event(
            "construct-pass",
            Fact::AttemptCompleted {
                bloom,
                workpiece: workpiece("wp"),
                stage: StageId::Construct,
                passed: true,
                evidence: Evidence { subject: digest(10), kind: EvidenceKind::VerificationResult, detail: digest(70) },
                candidate: None,
            },
        );
        let construct_decided = reduce(&snapshot, &construct_pass, &configs, &SpendWindow::default());
        snapshot = snapshot.apply(&construct_pass, &construct_decided, &configs);

        let verify_failed = event(
            "verify-failed",
            Fact::VerifyFailed {
                bloom,
                workpiece: workpiece("wp"),
                evidence: Evidence { subject: digest(10), kind: EvidenceKind::VerificationResult, detail: digest(71) },
                failed_verifiers: VerifyFailureSet::one(VerifyFailure::Clippy),
            },
        );
        let decided = reduce(&snapshot, &verify_failed, &configs, &SpendWindow::default());
        let (refine_command, refine_profile) = decided
            .effects
            .iter()
            .find_map(|effect| match effect {
                Decision::DispatchAttempt { profile, stage: StageId::Refine, transformation, .. } => {
                    Some((transformation.command.as_str(), profile))
                }
                _ => None,
            })
            .expect("a failing Verify dispatches Refine");

        assert_eq!(construct_command, refine_command, "Construct and Refine share the construct command");
        assert_eq!(
            construct_profile,
            catalog.profile_for(StageId::Construct).expect("the catalog binds Construct"),
            "Construct runs the profile its sealed catalog names",
        );
        assert_eq!(
            refine_profile,
            catalog.profile_for(StageId::Refine).expect("the catalog binds Refine"),
            "Refine runs the profile its sealed catalog names",
        );
        assert_ne!(construct_profile, refine_profile, "the shared command must not collapse stage-specific profiles");
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
        let snapshot = Snapshot::new(digest(1)).apply(
            &seal,
            &reduce(&Snapshot::new(digest(1)), &seal, &configs, &SpendWindow::default()),
            &configs,
        );

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
        let decided = reduce(&snapshot, &failed, &configs, &SpendWindow::default());
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

    // Tripwire: a stale (strict-ancestor) observation cannot move mainline
    // (#4938). The host classifies the head as an ancestor and admits
    // `ObserveMainlineDiverged`; the reducer must name the refusal and leave
    // both pointers alone — recording the stale head as `observed` would
    // poison the only base a supersession may rebase onto. A rewritten
    // live ref is followable at the host and does not arrive here.
    #[test]
    fn a_stale_observation_cannot_regress_mainline() {
        let snapshot = Snapshot::new(digest(9));
        let stale = event("stale", Fact::ObserveMainlineDiverged { head: digest(1) });

        let (next, decided) = step(&snapshot, &stale);

        assert!(
            matches!(
                decided.outcome,
                Outcome::MainlineDiverged { head, mainline } if head == digest(1) && mainline == digest(9)
            ),
            "a backward observation is a named refusal: {:?}",
            decided.outcome,
        );
        assert!(decided.effects.is_empty(), "the refusal records nothing: {:?}", decided.effects);
        assert_eq!(next.mainline, digest(9), "mainline stays on the true head");
        assert_eq!(next.observed, Snapshot::GENESIS_MAINLINE, "and observed is not poisoned by the stale head");
    }

    // Tripwire: after a regression-shaped state, observing the true head
    // recovers mainline (#4938). The old `observe-mainline-<digest>` key made
    // this `Duplicate` forever; the admit key is now (head, current mainline),
    // so a new key for the same head against the regressed pointer reaches
    // the reducer and advances.
    #[test]
    fn observing_the_true_head_recovers_a_regressed_mainline() {
        let snapshot = Snapshot::new(digest(1));
        let first = event("observe-mainline-9-at-1", Fact::ObserveMainline { head: digest(9) });
        let (mut snapshot, first_decided) = step(&snapshot, &first);
        assert!(
            matches!(first_decided.outcome, Outcome::MainlineAdvanced { to, .. } if to == digest(9)),
            "the first observation advanced: {:?}",
            first_decided.outcome,
        );

        // The historical bug: a later observation (or a hand fold) walked
        // mainline back. `observed` and `seen` keep what that lap recorded.
        snapshot.mainline = digest(1);

        let recover = event("observe-mainline-9-at-1-regressed", Fact::ObserveMainline { head: digest(9) });
        let (next, decided) = step(&snapshot, &recover);

        assert!(
            !matches!(decided.outcome, Outcome::Duplicate),
            "the same head against a different mainline is not a duplicate: {:?}",
            decided.outcome,
        );
        assert!(
            matches!(
                decided.outcome,
                Outcome::MainlineAdvanced { from, to } if from == digest(1) && to == digest(9)
            ),
            "observing the true head recovers mainline: {:?}",
            decided.outcome,
        );
        assert_eq!(next.mainline, digest(9), "and the fold lands on the recovered head");
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

/// An author-signed statement asserting `words` over `parents` — the shape the
/// orphan-release door requires. The reducer only checks provenance *class* and
/// binding, so the envelope's bytes need not verify here; the cryptographic gate
/// is the host route's, upstream of admission.
fn signed(words: &str, parents: Vec<Digest>) -> Statement {
    Statement {
        words: words.as_bytes().to_vec(),
        provenance: Provenance::AuthorSignature(SignatureEnvelope {
            signer: KeyId("operator".into()),
            signature: vec![0; 64],
        }),
        parents,
    }
}

fn release_target(holder: u8) -> OrphanClaimRelease {
    OrphanClaimRelease { ref_kind: ClaimRefKind::MainlineAdmission, expected_holder: BloomId(digest(holder)) }
}

fn release_event(key: &str, target: &OrphanClaimRelease, authorization: Statement) -> Event {
    event(key, Fact::RequestOrphanClaimRelease { request: target.clone(), authorization })
}

#[test]
fn an_orphaned_holder_releases_once_and_a_repeat_enqueues_nothing() {
    // Tripwire on ADR-0179's whole reason to exist plus its idempotency clause: a
    // holder no bloom record knows must be releasable, and re-submitting the same
    // request must return the handle without enqueuing a second release. A lost
    // repeat guard means an operator's double-click deletes a ref twice — the
    // second time against whatever holder has since taken it.
    let target = release_target(200);
    let authorization = signed(ORPHAN_CLAIM_RELEASE_WORDS, vec![target.request()]);
    let snapshot = Snapshot::default();

    let (snapshot, decisions) = step(&snapshot, &release_event("release-1", &target, authorization.clone()));

    assert_eq!(decisions.outcome, Outcome::OrphanClaimReleaseRequested { request: target.request() });
    assert_eq!(
        decisions.effects.iter().filter(|e| matches!(e, Decision::DispatchOrphanClaimRelease { .. })).count(),
        1,
        "an admitted request enqueues exactly one source effect",
    );
    assert_eq!(
        snapshot.orphan_releases.get(&target.request()).map(|record| record.completion),
        Some(None),
        "the request is recorded pending",
    );

    let (_, repeat) = step(&snapshot, &release_event("release-2", &target, authorization));

    assert_eq!(repeat.outcome, Outcome::OrphanClaimReleaseRequested { request: target.request() });
    assert!(repeat.effects.is_empty(), "a repeat of a recorded request enqueues no second release");
}

#[test]
fn a_locally_known_holder_is_refused_however_it_is_signed() {
    // Tripwire on the safety boundary: this door exists only for a holder no
    // journal knows. A bloom the snapshot holds belongs to reconcile, supersede,
    // or the land-time release, and letting a signature reach it would make this
    // a second, unaudited route around all three — able to free the claim refs of
    // a bloom that is still working.
    let (snapshot, spec) = sealed_and_resolved(1, vec![membership("wp-1", 1)], 40);
    let target = OrphanClaimRelease { ref_kind: ClaimRefKind::MainlineAdmission, expected_holder: spec.id() };

    let (after, decisions) = step(
        &snapshot,
        &release_event("release-known", &target, signed(ORPHAN_CLAIM_RELEASE_WORDS, vec![target.request()])),
    );

    assert_eq!(decisions.outcome, Outcome::OrphanClaimReleaseRejected(OrphanClaimReleaseError::HolderKnown(spec.id())),);
    assert!(after.orphan_releases.is_empty(), "a refused request records nothing");
}

#[test]
fn an_authorization_that_does_not_bind_this_request_is_refused() {
    // Tripwire on the parent binding. The words alone are a reusable token: a
    // signature harvested from one release would authorize every other if the
    // parents were not checked, which is the difference between "release this ref"
    // and "release anything". The wrong-words case is the same gate from the other
    // side — a signature over unrelated instruction text must not be replayable
    // here.
    let target = release_target(200);
    let snapshot = Snapshot::default();

    let wrong_parent = signed(ORPHAN_CLAIM_RELEASE_WORDS, vec![digest(99)]);
    let (_, decisions) = step(&snapshot, &release_event("release-a", &target, wrong_parent));
    assert_eq!(decisions.outcome, Outcome::OrphanClaimReleaseRejected(OrphanClaimReleaseError::AuthorizationNotBound),);

    let wrong_words = signed("release everything", vec![target.request()]);
    let (_, decisions) = step(&snapshot, &release_event("release-b", &target, wrong_words));
    assert_eq!(decisions.outcome, Outcome::OrphanClaimReleaseRejected(OrphanClaimReleaseError::AuthorizationNotBound),);

    let observed = Statement {
        words: ORPHAN_CLAIM_RELEASE_WORDS.as_bytes().to_vec(),
        provenance: Provenance::ObservationAttestation(Observation { source: "a mirror".into() }),
        parents: vec![target.request()],
    };
    let (_, decisions) = step(&snapshot, &release_event("release-c", &target, observed));
    assert_eq!(decisions.outcome, Outcome::OrphanClaimReleaseRejected(OrphanClaimReleaseError::NotInstructionCapable),);
}

#[test]
fn the_first_completion_wins_and_a_redrive_changes_nothing() {
    // Tripwire on the terminal: the first result is what the source actually did.
    // A redrive arriving after it re-reads an absent ref and would otherwise
    // overwrite `Released` with `AlreadyAbsent`, rewriting the audit trail of a
    // destructive act into something weaker than what happened.
    let target = release_target(200);
    let request = target.request();
    let (snapshot, _) = step(
        &Snapshot::default(),
        &release_event("release-1", &target, signed(ORPHAN_CLAIM_RELEASE_WORDS, vec![request])),
    );

    let completed = event(
        "complete-1",
        Fact::CompleteOrphanClaimRelease { request, completion: OrphanClaimReleaseCompletion::Released },
    );
    let (snapshot, decisions) = step(&snapshot, &completed);
    assert_eq!(
        decisions.outcome,
        Outcome::OrphanClaimReleaseCompleted { request, completion: OrphanClaimReleaseCompletion::Released },
    );
    assert_eq!(
        snapshot.orphan_releases.get(&request).and_then(|record| record.completion),
        Some(OrphanClaimReleaseCompletion::Released),
    );

    let redrive = event(
        "complete-2",
        Fact::CompleteOrphanClaimRelease { request, completion: OrphanClaimReleaseCompletion::AlreadyAbsent },
    );
    let (after, decisions) = step(&snapshot, &redrive);
    assert_eq!(
        decisions.outcome,
        Outcome::OrphanClaimReleaseRejected(OrphanClaimReleaseError::AlreadyCompleted(request)),
    );
    assert_eq!(
        after.orphan_releases.get(&request).and_then(|record| record.completion),
        Some(OrphanClaimReleaseCompletion::Released),
        "the first terminal stands",
    );
}

#[test]
fn a_completion_for_an_unadmitted_request_opens_no_record() {
    // Tripwire: the completion door must not be an admission door. If it opened a
    // record, a fabricated or mis-routed completion would manufacture the audit
    // trail of a release nobody ever authorized.
    let request = digest(77);
    let (after, decisions) = step(
        &Snapshot::default(),
        &event(
            "complete-orphan",
            Fact::CompleteOrphanClaimRelease { request, completion: OrphanClaimReleaseCompletion::Released },
        ),
    );

    assert_eq!(
        decisions.outcome,
        Outcome::OrphanClaimReleaseRejected(OrphanClaimReleaseError::UnknownRequest(request)),
    );
    assert!(after.orphan_releases.is_empty());
}

/// One member's resolution claim as production forms it (#4891): the claim a
/// *passing terminal Verify* produces, whose evidence is the verification
/// verdict itself, bound to the candidate tree it judged. The shared
/// [`common::claim`] fixture carries `ResolutionClaim` evidence instead, which
/// is not a verify verdict and so deliberately files no proof.
fn verified_claim(name: &str, revision: u8, candidate: u8, verdict: u8) -> ResolutionClaim {
    ResolutionClaim {
        workpiece: workpiece(name),
        scope_revision: digest(revision),
        candidate: digest(candidate),
        evidence: Evidence {
            subject: digest(candidate),
            kind: EvidenceKind::VerificationResult,
            detail: digest(verdict),
        },
    }
}

// #4891 — a single-member fold is byte-identical to the candidate its member
// already verified, so the aggregate verify passes on the recorded verdict
// instead of dispatching a full mechanical run to re-derive it. The fold still
// reaches the critic, held and unchanged.
//
// Tripwire on the receipt above all: a pass that dispatched nothing and recorded
// nothing would be indistinguishable from a gate quietly skipped, which is the
// one way this optimization could become a lie.
#[test]
fn a_fold_on_an_already_verified_tree_passes_the_aggregate_verify_by_identity() {
    let spec = draft(1, vec![membership("wp", 10)]).seal();
    let bloom = spec.id();
    let (snapshot, _) = step(&Snapshot::new(digest(1)), &event("seal", Fact::Seal(spec)));
    let (snapshot, _) =
        step(&snapshot, &event("integrate", Fact::Integrate { bloom, claim: verified_claim("wp", 10, 100, 60) }));

    let (after, resolved) = step(
        &snapshot,
        &event("resolve", Fact::Resolve { bloom, tree: digest(100), head: digest(101), lineage: vec![] }),
    );

    assert_eq!(resolved.outcome, Outcome::AggregateVerifyReused { bloom, rolls: 1, proof: digest(60) });
    assert!(
        !resolved.effects.iter().any(|effect| matches!(effect, Decision::DispatchAggregateVerify { .. })),
        "the gates do not run again over a tree this bloom already proved",
    );
    match resolved.effects.iter().find(|effect| matches!(effect, Decision::DispatchAggregateReview { .. })) {
        Some(Decision::DispatchAggregateReview { transformation, .. }) => {
            assert_eq!(transformation.inputs[0], digest(100), "the critic judges the fold the memo passed");
            assert_eq!(transformation.checkout, digest(101));
        }
        other => panic!("expected the critic to be dispatched, got {other:?}"),
    }

    let record = after.blooms.get(&bloom).unwrap();
    let reuse = record.verify_reuses.first().expect("a memo hit leaves a receipt naming what it reused");
    assert_eq!(reuse.stage, StageId::AggregateVerify);
    assert_eq!(reuse.proof.stage, StageId::Verify, "the reused verdict is the member's own");
    assert_eq!(reuse.proof.evidence.detail, digest(60));
    assert_eq!(record.aggregate_verify_rolls, 1, "a pass by identity consumes its verdict like a dispatched one");
    assert_eq!(record.integration.as_ref().unwrap().tree, digest(100), "the fold stays held for the critic");
}

// #4891 — the memo fires on tree *identity* and nothing else. A multi-member
// fold combines candidates into a tree that never existed before, so it misses
// and the full mechanical pass runs — even though both members' own trees are
// proven and sitting in the memo, which is what makes this a test of the key
// rather than of an empty map.
#[test]
fn a_multi_member_fold_still_runs_the_full_aggregate_verify() {
    let spec = draft(1, vec![membership("alpha", 10), membership("beta", 11)]).seal();
    let bloom = spec.id();
    let (snapshot, _) = step(&Snapshot::new(digest(1)), &event("seal", Fact::Seal(spec)));
    let (snapshot, _) =
        step(&snapshot, &event("i-a", Fact::Integrate { bloom, claim: verified_claim("alpha", 10, 100, 60) }));
    let (snapshot, _) =
        step(&snapshot, &event("i-b", Fact::Integrate { bloom, claim: verified_claim("beta", 11, 101, 61) }));
    assert_eq!(snapshot.blooms.get(&bloom).unwrap().verify_proofs.len(), 2, "both members' trees are proven");

    let (after, resolved) = step(
        &snapshot,
        &event("resolve", Fact::Resolve { bloom, tree: digest(40), head: digest(41), lineage: vec![] }),
    );

    assert!(matches!(resolved.outcome, Outcome::AggregateVerifyDispatched { roll: 1, .. }));
    assert!(
        resolved.effects.iter().any(|effect| matches!(effect, Decision::DispatchAggregateVerify { .. })),
        "a combined tree has never been built, so the compiler has to build it",
    );
    assert!(
        !resolved.effects.iter().any(|effect| matches!(effect, Decision::DispatchAggregateReview { .. })),
        "the critic still waits for the mechanical verdict",
    );
    assert!(after.blooms.get(&bloom).unwrap().verify_reuses.is_empty());
}

// #4891 — the gate set is half the memo key, so a proof collected under a
// different verify vocabulary or lane answers for nothing. The rewritten proof
// below is what a journal written by a binary with different gates replays as:
// the same tree, proven, under an identity this binary no longer runs.
//
// Tripwire: without the gate-set half, a verifier added to the umbrella would be
// satisfied by every verdict recorded before it existed — the gates would
// silently stop running on exactly the trees they were added for.
#[test]
fn a_proof_from_another_gate_set_refuses_the_memo_and_re_proves() {
    let spec = draft(1, vec![membership("wp", 10)]).seal();
    let bloom = spec.id();
    let (snapshot, _) = step(&Snapshot::new(digest(1)), &event("seal", Fact::Seal(spec)));
    let (mut snapshot, _) =
        step(&snapshot, &event("integrate", Fact::Integrate { bloom, claim: verified_claim("wp", 10, 100, 60) }));

    let record = snapshot.blooms.get_mut(&bloom).unwrap();
    let mut proof = record.verify_proofs.values().next().unwrap().clone();
    proof.gate_set = digest(200);
    record.verify_proofs.clear();
    record.verify_proofs.insert(proof.verified(), proof);

    let (after, resolved) = step(
        &snapshot,
        &event("resolve", Fact::Resolve { bloom, tree: digest(100), head: digest(101), lineage: vec![] }),
    );

    assert!(matches!(resolved.outcome, Outcome::AggregateVerifyDispatched { roll: 1, .. }));
    assert!(
        resolved.effects.iter().any(|effect| matches!(effect, Decision::DispatchAggregateVerify { .. })),
        "gates this binary runs have never judged that tree, whatever else has",
    );
    assert!(after.blooms.get(&bloom).unwrap().verify_reuses.is_empty());
}

// #4891 — the memo is keyed by content, not by position, so it answers any
// verify aimed at a proven tree. The live case: an aggregate review sends a
// member back into Refine, the repair lap changes nothing the tree records (an
// amended commit message leaves the same tree), and the member's terminal Verify
// would otherwise re-pay the whole mechanical run for the verdict it already
// holds. It integrates on that verdict instead.
#[test]
fn a_repair_lap_that_leaves_the_tree_unchanged_reuses_its_verify_verdict() {
    let spec = draft(1, vec![membership("wp", 10)]).seal();
    let bloom = spec.id();
    let candidate = CandidateRef { tree: digest(100), checkout: digest(102) };
    let (snapshot, _) = step(&Snapshot::new(digest(1)), &event("seal", Fact::Seal(spec)));
    let (snapshot, constructed) = step(
        &snapshot,
        &event(
            "construct",
            Fact::AttemptCompleted {
                bloom,
                workpiece: workpiece("wp"),
                stage: StageId::Construct,
                passed: true,
                evidence: Evidence { subject: digest(10), kind: EvidenceKind::VerificationResult, detail: digest(59) },
                candidate: Some(candidate),
            },
        ),
    );
    assert!(
        constructed.effects.iter().any(|effect| matches!(effect, Decision::DispatchAttempt { .. })),
        "an unproven tree dispatches its verify, which is what makes the reuse below a memo hit",
    );

    // The member verifies and integrates, filing the proof; a later failing
    // terminal Verify sends it back into its own repair lap.
    let (snapshot, _) =
        step(&snapshot, &event("integrate", Fact::Integrate { bloom, claim: verified_claim("wp", 10, 100, 60) }));
    let (snapshot, _) = step(
        &snapshot,
        &event(
            "verify-failed",
            Fact::VerifyFailed {
                bloom,
                workpiece: workpiece("wp"),
                evidence: Evidence { subject: digest(100), kind: EvidenceKind::VerificationResult, detail: digest(70) },
                failed_verifiers: VerifyFailureSet::one(VerifyFailure::Clippy),
            },
        ),
    );
    assert_eq!(snapshot.blooms.get(&bloom).unwrap().progress.get(&workpiece("wp")).unwrap().stage, StageId::Refine);

    let (after, repaired) = step(
        &snapshot,
        &event(
            "refine",
            Fact::AttemptCompleted {
                bloom,
                workpiece: workpiece("wp"),
                stage: StageId::Refine,
                passed: true,
                evidence: Evidence { subject: digest(100), kind: EvidenceKind::VerificationResult, detail: digest(71) },
                candidate: None,
            },
        ),
    );

    assert_eq!(repaired.outcome, Outcome::VerifyReused { bloom, workpiece: workpiece("wp"), proof: digest(60) });
    assert!(
        !repaired.effects.iter().any(|effect| matches!(effect, Decision::DispatchAttempt { .. })),
        "the mechanical lane does not re-run over a tree it has already passed",
    );
    let record = after.blooms.get(&bloom).unwrap();
    let claim = record.claims.get(&workpiece("wp")).expect("passing by identity integrates the member");
    assert_eq!(claim.candidate, digest(100));
    assert_eq!(claim.evidence.detail, digest(60), "the claim carries the verdict it stood on, not a fresh one");
    assert_eq!(record.progress.get(&workpiece("wp")).unwrap().stage, StageId::Verify, "the cursor still lands there");
    assert_eq!(record.verify_reuses.last().unwrap().stage, StageId::Verify);
}

/// Three members carrying verified claims — the shape a fold collision arrives
/// at, with enough members that one can fold clean while two collide.
fn three_members_with_claims() -> (Snapshot, BloomId) {
    let spec = draft(1, vec![membership("alpha", 10), membership("beta", 11), membership("gamma", 12)]).seal();
    let bloom = spec.id();
    let (snapshot, _) = step(&Snapshot::new(digest(1)), &event("seal", Fact::Seal(spec)));
    let mut snapshot = snapshot;
    for (name, revision, candidate) in [("alpha", 10, 20), ("beta", 11, 21), ("gamma", 12, 22)] {
        snapshot = step(
            &snapshot,
            &event(&format!("i-{name}"), Fact::Integrate { bloom, claim: claim(name, revision, candidate) }),
        )
        .0;
    }
    (snapshot, bloom)
}

/// A collision for `workpiece` against the settled tree `checkpoint`, whose
/// landable head the Reconcile lane checks out.
fn fold_conflict(bloom: BloomId, key: &str, workpiece: &str, checkpoint: u8, head: u8, detail: u8) -> Event {
    event(
        key,
        Fact::FoldConflict {
            bloom,
            workpiece: crate::workpiece(workpiece),
            checkpoint: digest(checkpoint),
            head: digest(head),
            evidence: Evidence {
                subject: digest(checkpoint),
                kind: EvidenceKind::FoldConflict,
                detail: digest(detail),
            },
        },
    )
}

/// A Reconcile lane that passed, capturing `tree` at `checkout`.
fn reconciled(bloom: BloomId, key: &str, workpiece: &str, tree: u8, checkout: u8) -> Event {
    event(
        key,
        Fact::AttemptCompleted {
            bloom,
            workpiece: crate::workpiece(workpiece),
            stage: StageId::Reconcile,
            passed: true,
            evidence: Evidence { subject: digest(70), kind: EvidenceKind::VerificationResult, detail: digest(80) },
            candidate: Some(CandidateRef { tree: digest(tree), checkout: digest(checkout) }),
        },
    )
}

/// How many lanes the bloom has bought for one member's stage — the ADR-0180
/// spend ledger, which no reset inside a bloom's life hands back.
fn lanes_bought(snapshot: &Snapshot, bloom: BloomId, workpiece: &str, stage: StageId) -> u32 {
    snapshot
        .blooms
        .get(&bloom)
        .unwrap()
        .dispatches
        .get(&DispatchKey::Member { workpiece: crate::workpiece(workpiece), stage })
        .copied()
        .unwrap_or(0)
}

// #4952 (acceptance 1) — the settled fold sends every conflicted member to
// reconcile against one checkpoint, and buys each of them exactly one lane for
// it. The member that folded clean is not a subject of the collision at all.
//
// This is the reducer half of the `10a1228c` cascade: with the fold settled
// first, both collisions name the same tree, so neither member's reconcile can
// be correct against a checkpoint the other one then moves.
#[test]
fn a_settled_fold_buys_each_conflicted_member_one_lane_against_one_checkpoint() {
    let (snapshot, bloom) = three_members_with_claims();
    let alpha_cursor = *snapshot.blooms.get(&bloom).unwrap().progress.get(&workpiece("alpha")).unwrap();

    let (snapshot, beta) = step(&snapshot, &fold_conflict(bloom, "fc-beta", "beta", 30, 31, 90));
    let (after, gamma) = step(&snapshot, &fold_conflict(bloom, "fc-gamma", "gamma", 30, 31, 91));

    for (name, decided) in [("beta", &beta), ("gamma", &gamma)] {
        assert!(
            matches!(&decided.outcome, Outcome::FoldConflictDispatched { workpiece: wp, .. } if wp.0 == name),
            "{name} is dispatched, not refused: {:?}",
            decided.outcome,
        );
        let dispatches: Vec<(StageId, Digest)> = decided
            .effects
            .iter()
            .filter_map(|effect| match effect {
                Decision::DispatchAttempt { stage, transformation, .. } => Some((*stage, transformation.checkout)),
                _ => None,
            })
            .collect();
        assert_eq!(
            dispatches,
            vec![(StageId::Reconcile, digest(31))],
            "{name} reconciles once, checking out the settled head",
        );
    }

    let record = after.blooms.get(&bloom).unwrap();
    for name in ["beta", "gamma"] {
        let progress = record.progress.get(&workpiece(name)).unwrap();
        assert_eq!(progress.stage, StageId::Reconcile);
        assert_eq!(progress.fold_checkpoint, Some(digest(31)), "{name} is in the settled round");
        assert_eq!(lanes_bought(&after, bloom, name, StageId::Reconcile), 1, "{name} paid for one lane, not two");
        assert!(!record.claims.contains_key(&workpiece(name)), "{name}'s unfoldable claim is revoked");
    }
    assert!(record.wedged.is_empty(), "a first collision wedges nobody");
    assert!(record.claims.contains_key(&workpiece("alpha")), "the member that folded clean keeps its claim");
    assert_eq!(
        record.progress.get(&workpiece("alpha")).unwrap(),
        &alpha_cursor,
        "and is not a subject of somebody else's collision",
    );
}

// #4952 (acceptance 2) — the Reconcile budget guards a member's own inability to
// reproduce its intent, and nothing else. A collision whose checkpoint moved is
// a sibling's reconciled candidate folding underneath this one: the member's
// diff never changed and it was never asked about that tree, so it opens a fresh
// round however many siblings ripple through. A collision with the very head the
// member was already handed is the fold standing still while what came back
// still does not land on it — that wedges, with the collision evidence attached.
//
// Both halves are one invariant and are asserted together, because either alone
// passes a wrong rule: resetting unconditionally (the shape this replaces) buys
// unbounded lanes against a tree two of them already failed, and charging
// unconditionally wedges a member whose own work was never in question.
#[test]
fn sibling_moved_folds_reopen_a_reconcile_round_while_a_standing_one_wedges() {
    let (snapshot, bloom) = three_members_with_claims();

    // Three rounds, each on a head a sibling's fold moved to. The member's own
    // work is never in question, so no round may spend the last.
    let mut live = snapshot;
    for (round, head) in [(1u8, 31u8), (2, 33), (3, 35)] {
        let (after, decided) =
            step(&live, &fold_conflict(bloom, &format!("fc-{round}"), "beta", head - 1, head, 90 + round));
        assert!(
            matches!(decided.outcome, Outcome::FoldConflictDispatched { .. }),
            "round {round} is a fresh round, not a wedge: {:?}",
            decided.outcome,
        );
        assert_eq!(
            after.blooms.get(&bloom).unwrap().progress.get(&workpiece("beta")).unwrap().attempts,
            1,
            "round {round} starts beta's Reconcile attempts over",
        );
        assert!(after.blooms.get(&bloom).unwrap().wedged.is_empty(), "no sibling ripple wedges beta");

        let (after, _) = step(&after, &reconciled(bloom, &format!("rec-{round}"), "beta", 40 + round, 50 + round));
        live = step(
            &after,
            &event(&format!("re-i-{round}"), Fact::Integrate { bloom, claim: claim("beta", 11, 40 + round) }),
        )
        .0;
    }
    assert_eq!(lanes_bought(&live, bloom, "beta", StageId::Reconcile), 3, "one lane per round, never two");

    // The fold has not moved since beta was handed head 35, and what came back
    // still does not land on it. That is beta's own miss.
    let (after, wedged) = step(&live, &fold_conflict(bloom, "fc-standing", "beta", 34, 35, 99));

    assert!(
        matches!(wedged.outcome, Outcome::AttemptWedged { stage: StageId::Reconcile, workpiece: ref wp, .. } if wp.0 == "beta"),
        "a standing checkpoint is the member's own miss: {:?}",
        wedged.outcome,
    );
    assert!(
        !wedged.effects.iter().any(|effect| matches!(effect, Decision::DispatchAttempt { .. })),
        "a wedged member buys no further lane against the tree it already failed",
    );
    assert_eq!(lanes_bought(&after, bloom, "beta", StageId::Reconcile), 3, "and the ledger records no fourth");
    let record = after.blooms.get(&bloom).unwrap();
    let wedge = record.wedged.get(&workpiece("beta")).expect("the wedge is recorded");
    assert_eq!(wedge.stage, StageId::Reconcile);
    assert_eq!(wedge.evidence, digest(99), "the wedge attaches the collision evidence a reader needs");
    assert!(!record.claims.contains_key(&workpiece("beta")), "the unfoldable claim stays revoked");
}

/// A failing composition review over `tree` whose verdict artifact is `detail`
/// — the finding an operator later adjudicates, named apart from its siblings.
fn review_refused(bloom: BloomId, key: &str, tree: u8, detail: u8) -> Event {
    event(
        key,
        Fact::AggregateReviewCompleted {
            bloom,
            passed: false,
            evidence: Evidence { subject: digest(tree), kind: EvidenceKind::ReviewFinding, detail: digest(detail) },
            implicated: vec![workpiece("alpha")],
        },
    )
}

/// An operator adjudication of `findings`, accepted as they stand.
fn adjudicated(bloom: BloomId, key: &str, findings: Vec<u8>) -> Event {
    event(
        key,
        Fact::OperatorAdjudication {
            bloom,
            adjudication: Adjudication {
                findings: findings.into_iter().map(digest).collect(),
                disposition: Disposition::Accepted,
                reason: "read the finding; it is a fixture nit and the tree is landable".into(),
                operator: "iamacoffeepot".into(),
            },
        },
    )
}

/// A bloom that has spent its first composition-review roll, repaired the weave,
/// and is awaiting the delta-confirm over the re-woven tree — the position one
/// refusal short of the two-pass ceiling.
fn awaiting_the_delta_confirm() -> (Snapshot, BloomId) {
    let base = Snapshot::new(digest(1));
    let spec = draft(1, vec![membership("alpha", 10), membership("beta", 11)]).seal();
    let bloom = spec.id();
    let (snapshot, _) = step(&base, &event("seal", Fact::Seal(spec)));
    let (snapshot, _) = step(&snapshot, &event("i-a", Fact::Integrate { bloom, claim: claim("alpha", 10, 100) }));
    let (snapshot, _) = step(&snapshot, &event("i-b", Fact::Integrate { bloom, claim: claim("beta", 11, 101) }));
    let (snapshot, _) =
        step(&snapshot, &event("r1", Fact::Resolve { bloom, tree: digest(40), head: digest(41), lineage: vec![] }));
    let (snapshot, _) = step(&snapshot, &verify_passed(bloom, "v1", 40));
    let (snapshot, _) = step(&snapshot, &review_refused(bloom, "refuse-1", 40, 70));
    let (snapshot, _) = step(&snapshot, &weave_repaired(bloom, "weave-1", 40, 44, 45));
    let (snapshot, _) = step(&snapshot, &verify_passed(bloom, "v2", 44));
    (snapshot, bloom)
}

/// A bloom parked at its composition review's two-pass ceiling, with two
/// distinct findings on the composition's channel: the first refusal's, and the
/// delta-confirm's, which is also the question the park raises.
fn parked_at_the_review_ceiling() -> (Snapshot, BloomId) {
    let (snapshot, bloom) = awaiting_the_delta_confirm();
    let (snapshot, _) = step(&snapshot, &review_refused(bloom, "refuse-2", 44, 71));
    (snapshot, bloom)
}

// #4957 / ADR-0191 §4 — the manager override's first move. A bloom parked at
// its composition review's ceiling has one finding left that no re-roll will
// repair; an operator who has read it closes it with a stated reason and the
// composition proceeds to its landing from the weave it already holds.
//
// Two tripwires ride together. The override must reach the *composition* and
// nothing else: closing a finding is not a licence to re-open the members that
// finding pointed at, so `assert_no_member_dispatch` is the same assertion the
// ADR-0191 refusal paths carry — this door must not become the re-entry those
// paths abolished. And closure must be derived rather than destructive: the
// finding stays on the record, and what changes is that an adjudication names
// it, so the journal still carries both the verdict and the decision to waive
// it.
#[test]
fn an_adjudication_closes_the_finding_unparks_the_bloom_and_touches_no_member() {
    let (parked, bloom) = parked_at_the_review_ceiling();
    let before = parked.blooms.get(&bloom).unwrap();
    assert_eq!(before.review_park, Some(digest(71)), "the delta-confirm's artifact is the parked question");
    assert!(before.holds.contains(&digest(71)));
    // Both refusals filed a finding (#4977) — the first re-wove, the second
    // spent the budget and parked — so the channel carries the escalated
    // refusal as well as the repaired one, and both are the operator's to
    // adjudicate through it.
    assert_eq!(before.open_composition_findings().count(), 2, "every refusal of the composed tree filed one");
    let member_cursors = before.progress.clone();

    // An adjudication of a finding this bloom never raised closes nothing: the
    // override adjudicates findings, so it cannot invent one to waive.
    assert!(matches!(
        reduce(&parked, &adjudicated(bloom, "ghost", vec![99]), &ResolvedConfigs::default(), &SpendWindow::default()).outcome,
        Outcome::AdjudicationRejected(AdjudicationError::UnknownFinding(finding)) if finding == digest(99),
    ));

    let (after, decided) = step(&parked, &adjudicated(bloom, "adj", vec![70, 71]));

    assert!(
        matches!(decided.outcome, Outcome::FindingsAdjudicated { proceeds_to_landing: true, ref closed, .. }
            if *closed == vec![digest(70), digest(71)]),
        "got {:?}",
        decided.outcome,
    );
    assert_no_member_dispatch(&decided, "an override closes a composition finding, it never re-opens a member");

    let record = after.blooms.get(&bloom).unwrap();
    assert_eq!(record.open_composition_findings().count(), 0, "both filed findings are closed");
    assert_eq!(record.composition_findings.len(), 2, "and the verdicts that raised them still stand on the record");
    assert_eq!(record.adjudications.len(), 1, "the closure is a record of its own");
    assert_eq!(record.adjudications[0].operator, "iamacoffeepot", "naming who decided");
    assert_eq!(record.review_park, None, "the park is released");
    assert!(!record.holds.contains(&digest(71)), "and so is the hold it raised");
    assert_eq!(record.status, BloomStatus::Resolved, "the composition proceeds to its landing");
    assert_eq!(record.claims.len(), 2, "every member's resolution stands untouched");
    assert_eq!(record.progress.get(&workpiece("alpha")), member_cursors.get(&workpiece("alpha")), "alpha is untouched");
    assert_eq!(record.progress.get(&workpiece("beta")), member_cursors.get(&workpiece("beta")), "beta is untouched");
    assert_eq!(composition_cursor(record).stage, StageId::Land, "the composition is what moved");
    assert!(record.integration.is_none(), "the weave is consumed by the resolve, as a passing review consumes it");
    match decided.effects.iter().find(|effect| matches!(effect, Decision::DispatchLand { .. })) {
        Some(Decision::DispatchLand { new_head, .. }) => assert_eq!(*new_head, digest(45), "landing the held weave"),
        other => panic!("expected the land dispatch, got {other:?}"),
    }
}

// #4977 — the park's own finding is what an operator adjudicates. The ceiling
// refusal files it on the composition's channel like every other refusal, so
// closing it is the general path rather than a special case over the park
// marker: the bloom un-parks and proceeds to its landing from the weave it
// holds, and the finding the *first* refusal filed is untouched, because an
// adjudication closes what it names and nothing else.
//
// Rides with `assert_no_member_dispatch` for the same reason the sibling above
// does: closing the finding a ceiling raised must not become a way back into
// the members that finding pointed at (ADR-0191 §4).
#[test]
fn adjudicating_the_parks_own_finding_unparks_the_bloom() {
    let (parked, bloom) = parked_at_the_review_ceiling();
    let before = parked.blooms.get(&bloom).unwrap();
    let parked_finding =
        before.open_composition_findings().find(|open| open.detail == digest(71)).expect("the park filed its finding");
    assert_eq!(parked_finding.subject, digest(44), "raised against the weave the delta-confirm judged");
    assert_eq!(before.review_park, Some(digest(71)));

    let (after, decided) = step(&parked, &adjudicated(bloom, "adj", vec![71]));

    assert!(
        matches!(decided.outcome, Outcome::FindingsAdjudicated { proceeds_to_landing: true, .. }),
        "got {:?}",
        decided.outcome,
    );
    assert_no_member_dispatch(&decided, "closing a ceiling refusal's finding re-opens no member");
    let record = after.blooms.get(&bloom).unwrap();
    assert_eq!(record.review_park, None, "the park the finding raised is released with it");
    assert!(!record.holds.contains(&digest(71)));
    assert_eq!(record.status, BloomStatus::Resolved, "and the composition proceeds to its landing");
    assert_eq!(
        record.open_composition_findings().map(|open| open.detail).collect::<Vec<_>>(),
        vec![digest(70)],
        "the re-weaving refusal's finding is not closed by an adjudication that never named it",
    );
}

// #4977 / ADR-0190 — a journal written before the park filed its finding still
// replays and stays adjudicable. Boot replay folds the decisions as they were
// recorded, so such a record carries the park marker and nothing on the
// composition's channel for it; the marker arm of the adjudication door is what
// keeps that operator's one move reachable, and it is kept for exactly this and
// for the admitted-question park.
//
// The pre-fix decision set is written out rather than reduced, because that is
// the whole point: what the reducer emits today cannot reproduce the rows a
// prior binary wrote, and replay never re-reduces them.
#[test]
fn a_pre_fix_park_still_replays_and_stays_adjudicable() {
    let (awaiting, bloom) = awaiting_the_delta_confirm();
    let refusal = review_refused(bloom, "refuse-2", 44, 71);
    let evidence = Evidence { subject: digest(44), kind: EvidenceKind::ReviewFinding, detail: digest(71) };
    let pre_fix = Decisions {
        outcome: Outcome::AggregateReviewParked { bloom, rolls: 2, question: digest(71) },
        effects: vec![
            Decision::RecordEvidence { bloom, evidence },
            Decision::RecordAggregateRoll { bloom, rolls: 2 },
            Decision::RecordReviewPark { bloom, question: Some(digest(71)) },
        ],
    };

    let replayed = awaiting.apply(&refusal, &pre_fix, &ResolvedConfigs::default());

    let record = replayed.blooms.get(&bloom).unwrap();
    assert_eq!(record.review_park, Some(digest(71)), "the pre-fix park projects as it always did");
    assert!(record.holds.contains(&digest(71)));
    assert!(
        !record.composition_findings.iter().any(|finding| finding.detail == digest(71)),
        "and carries no finding for it — the row the fix adds was never written",
    );

    let (after, decided) = step(&replayed, &adjudicated(bloom, "adj", vec![71]));

    assert!(
        matches!(decided.outcome, Outcome::FindingsAdjudicated { proceeds_to_landing: true, .. }),
        "an old record's park is still the operator's to close: {:?}",
        decided.outcome,
    );
    assert_no_member_dispatch(&decided, "the legacy arm is a way to close a finding, not a way into a member");
    assert_eq!(after.blooms.get(&bloom).unwrap().review_park, None, "the park is released");
}

/// The first review has refused a verify-green weave and the composition's
/// refine lap has captured a newer candidate that has not yet been proven —
/// the #5104 incident shape, stopped before the newer head's aggregate verify
/// returns. The first refusal's finding stays open, so the same adjudication
/// can be issued against either fork.
fn newer_weave_awaiting_aggregate_verify() -> (Snapshot, BloomId) {
    let base = Snapshot::new(digest(1));
    let spec = draft(1, vec![membership("alpha", 10), membership("beta", 11)]).seal();
    let bloom = spec.id();
    let (snapshot, _) = step(&base, &event("seal", Fact::Seal(spec)));
    let (snapshot, _) = step(&snapshot, &event("i-a", Fact::Integrate { bloom, claim: claim("alpha", 10, 100) }));
    let (snapshot, _) = step(&snapshot, &event("i-b", Fact::Integrate { bloom, claim: claim("beta", 11, 101) }));
    let (snapshot, _) =
        step(&snapshot, &event("r1", Fact::Resolve { bloom, tree: digest(40), head: digest(41), lineage: vec![] }));
    let (snapshot, _) = step(&snapshot, &verify_passed(bloom, "v1", 40));
    let (snapshot, _) = step(&snapshot, &review_refused(bloom, "refuse-1", 40, 70));
    let (snapshot, _) = step(&snapshot, &weave_repaired(bloom, "weave-1", 40, 44, 45));
    (snapshot, bloom)
}

// #5104 — closing findings and re-arming the landing are separable. A refine
// lap can replace the held fold while an earlier finding is still open;
// adjudicating that finding must not propose the newer head whose aggregate
// verify is red. The same override, issued after that head goes green, is what
// dispatches the land. The plausible bug is the one the incident was:
// `proceed_to_landing` reading the current fold and emitting `DispatchLand`
// without consulting the proof bound to it.
#[test]
fn an_adjudication_does_not_land_a_head_whose_aggregate_verify_is_red() {
    let (pending, bloom) = newer_weave_awaiting_aggregate_verify();
    let before = pending.blooms.get(&bloom).unwrap();
    assert_eq!(before.integration.as_ref().unwrap().head, digest(45), "the refine lap already replaced the fold");
    assert!(before.verify_proof_for(digest(44)).is_none(), "the newer weave has no green proof yet");
    assert!(
        before.open_composition_findings().any(|open| open.detail == digest(70)),
        "the first refusal is still open"
    );

    let failed = event(
        "v-red",
        Fact::AggregateVerifyCompleted {
            bloom,
            passed: false,
            evidence: Evidence { subject: digest(44), kind: EvidenceKind::VerificationResult, detail: digest(52) },
        },
    );
    let (red, _) = step(&pending, &failed);
    let (after_red, decided_red) = step(&red, &adjudicated(bloom, "adj-red", vec![70]));

    assert!(
        matches!(decided_red.outcome, Outcome::FindingsAdjudicated { proceeds_to_landing: false, ref closed, .. }
            if *closed == vec![digest(70)]),
        "a red head is not a landing: {:?}",
        decided_red.outcome,
    );
    assert!(
        !decided_red.effects.iter().any(|effect| matches!(effect, Decision::DispatchLand { .. })),
        "the override must not propose the unproven weave: {:?}",
        decided_red.effects,
    );
    assert_no_member_dispatch(&decided_red, "closing a finding is still not a way back into a member");
    let record = after_red.blooms.get(&bloom).unwrap();
    assert_eq!(record.adjudications.len(), 1, "the findings still close");
    assert_eq!(record.status, BloomStatus::Sealed, "landing waits on a green proof, it does not resolve early");

    let (green, _) = step(&pending, &verify_passed(bloom, "v-green", 44));
    let (after_green, decided_green) = step(&green, &adjudicated(bloom, "adj-green", vec![70]));

    assert!(
        matches!(decided_green.outcome, Outcome::FindingsAdjudicated { proceeds_to_landing: true, .. }),
        "the same override lands once the head is proven: {:?}",
        decided_green.outcome,
    );
    match decided_green.effects.iter().find(|effect| matches!(effect, Decision::DispatchLand { .. })) {
        Some(Decision::DispatchLand { new_head, .. }) => {
            assert_eq!(*new_head, digest(45), "landing the weave the refine captured, now proven");
        }
        other => panic!("expected the land dispatch, got {other:?}"),
    }
    assert_eq!(after_green.blooms.get(&bloom).unwrap().status, BloomStatus::Resolved);
}

// #4957 — a deferral that names no filed issue is refused. Deferring a finding
// to nothing filed is exactly how a waived defect silently vanishes, which is
// the failure mode the disposition exists to prevent, so the reducer refuses it
// rather than recording a waiver that points nowhere.
#[test]
fn a_deferral_naming_no_issue_is_refused() {
    let (parked, bloom) = parked_at_the_review_ceiling();
    let deferred = |issue: u64| {
        event(
            "defer",
            Fact::OperatorAdjudication {
                bloom,
                adjudication: Adjudication {
                    findings: vec![digest(71)],
                    disposition: Disposition::Deferred { issue },
                    reason: "filed forward".into(),
                    operator: "iamacoffeepot".into(),
                },
            },
        )
    };

    assert!(matches!(
        reduce(&parked, &deferred(0), &ResolvedConfigs::default(), &SpendWindow::default()).outcome,
        Outcome::AdjudicationRejected(AdjudicationError::DeferredWithoutIssue),
    ));
    assert!(matches!(
        reduce(&parked, &deferred(4957), &ResolvedConfigs::default(), &SpendWindow::default()).outcome,
        Outcome::FindingsAdjudicated { .. },
    ));
}

// #4957 / ADR-0181 — Tripwire: an override adjudicates findings and retry
// budgets, never approval tiers. Both doors refuse a bloom whose membership is
// not fully approved rather than carrying it toward a landing, so no reason
// string can stand in for the signed statement an above-`auto` member needs.
//
// The seal door already refuses such a membership, which is why this is worth
// pinning: an override is the one act whose authority is an unsigned operator
// identity, so it is the one place a record that reached this state by any
// other route would convert "I read the findings" into "the approval was not
// needed".
#[test]
fn an_override_refuses_a_bloom_whose_membership_is_not_approved() {
    let unapproved = Membership {
        workpiece: workpiece("alpha"),
        scope_revision: digest(10),
        configs: ConfigRegistry::default(),
        // Bound to a digest that is not this member's subject — evidence for one
        // digest says nothing about any other.
        approval: Evidence { subject: digest(0), kind: EvidenceKind::Approval, detail: digest(200) },
    };
    let spec = draft(1, vec![unapproved]).seal();
    let bloom = spec.id();
    let mut snapshot = Snapshot::new(digest(1));
    splice_bloom(&mut snapshot, &spec, BloomStatus::Sealed);

    assert!(matches!(
        reduce(&snapshot, &adjudicated(bloom, "adj", vec![70]), &ResolvedConfigs::default(), &SpendWindow::default()).outcome,
        Outcome::AdjudicationRejected(AdjudicationError::UnapprovedMember(ref wp)) if *wp == workpiece("alpha"),
    ));

    let repair = event(
        "repair",
        Fact::OperatorRepair {
            bloom,
            repair: OperatorRepair {
                workpiece: workpiece("alpha"),
                candidate: CandidateRef { tree: digest(60), checkout: digest(61) },
                reason: "wrote the fix myself".into(),
                operator: "iamacoffeepot".into(),
            },
        },
    );
    assert!(matches!(
        reduce(&snapshot, &repair, &ResolvedConfigs::default(), &SpendWindow::default()).outcome,
        Outcome::OperatorRepairRejected(OperatorRepairError::UnapprovedMember(ref wp)) if *wp == workpiece("alpha"),
    ));

    // The refusal is about the approval, not about the shape of the request: an
    // approved membership admits the very same adjudication shape (its own
    // refusal is the absent finding, which is a different door).
    let approved_spec = draft(1, vec![membership("alpha", 10)]).seal();
    let mut approved_snapshot = Snapshot::new(digest(1));
    splice_bloom(&mut approved_snapshot, &approved_spec, BloomStatus::Sealed);
    assert!(matches!(
        reduce(
            &approved_snapshot,
            &adjudicated(approved_spec.id(), "adj-2", vec![70]),
            &ResolvedConfigs::default(),
            &SpendWindow::default()
        )
        .outcome,
        Outcome::AdjudicationRejected(AdjudicationError::UnknownFinding(_)),
    ));
}

// #4957 — the manager override's second move. A wedged member re-enters at
// `Verify` on the candidate the operator pushed, and the gates still run over
// it: the effect is a `Verify` dispatch, never a resolution claim, so the
// mechanical suite judges the operator's tree exactly as it judges a lane's and
// a bad operator fix bounces where a bad lane's does.
//
// Tripwires. The dispatch must be a `Verify` one and the member must gain no
// claim — a repair that integrated directly would be a waiver wearing a
// candidate's clothes. The operator must be recorded, since the dispatch it
// produces is otherwise indistinguishable from a lane's. And the member's spent
// counters must ride across unchanged: an operator writing the candidate buys a
// lap, never a fresh budget.
#[test]
fn an_operator_repair_re_enters_a_wedged_member_at_verify_with_the_gates_intact() {
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
    let (snapshot, _) = step(&snapshot, &fail("c-1"));
    let (wedged, decided) = step(&snapshot, &fail("c-2"));
    assert!(matches!(decided.outcome, Outcome::AttemptWedged { stage: StageId::Construct, .. }));

    let repair = |key: &str, workpiece: WorkpieceId, reason: &str| {
        event(
            key,
            Fact::OperatorRepair {
                bloom,
                repair: OperatorRepair {
                    workpiece,
                    candidate: CandidateRef { tree: digest(60), checkout: digest(61) },
                    reason: reason.into(),
                    operator: "iamacoffeepot".into(),
                },
            },
        )
    };

    // A blank reason is refused rather than defaulted: the record is the whole
    // product of an override, and one that says nothing records that a person
    // intervened and nothing about why.
    let refuse = |snapshot: &Snapshot, event: &Event| {
        reduce(snapshot, event, &ResolvedConfigs::default(), &SpendWindow::default()).outcome
    };
    assert!(matches!(
        refuse(&wedged, &repair("blank", workpiece("wp"), "   ")),
        Outcome::OperatorRepairRejected(OperatorRepairError::BlankReason),
    ));
    // A workpiece that is not stopped has nothing to restart, and a stranger to
    // the membership is work the seal never admitted.
    assert!(matches!(
        refuse(&snapshot, &repair("running", workpiece("wp"), "mid-flight")),
        Outcome::OperatorRepairRejected(OperatorRepairError::NotWedged(_)),
    ));
    assert!(matches!(
        refuse(&wedged, &repair("stranger", workpiece("ghost"), "not a member")),
        Outcome::OperatorRepairRejected(OperatorRepairError::NotWedged(_)),
    ));

    let (after, decided) = step(&wedged, &repair("repair", workpiece("wp"), "one-line fix, cheaper than a lap"));

    assert!(
        matches!(decided.outcome, Outcome::OperatorRepairAccepted { ref workpiece, candidate, .. }
            if workpiece.0 == "wp" && candidate == digest(60)),
        "got {:?}",
        decided.outcome,
    );
    match decided.effects.iter().find(|effect| matches!(effect, Decision::DispatchAttempt { .. })) {
        Some(Decision::DispatchAttempt { stage, candidate, transformation, .. }) => {
            assert_eq!(*stage, StageId::Verify, "the operator skips the model lap, never the gate");
            assert_eq!(*candidate, Some(digest(60)), "the returned evidence binds the operator's tree");
            assert_eq!(transformation.checkout, digest(61), "and the lane checks out the commit they pushed");
        }
        other => panic!("expected the member's Verify dispatch, got {other:?}"),
    }
    assert!(
        !decided.effects.iter().any(|effect| matches!(effect, Decision::RecordResolution { .. })),
        "a repair supplies a candidate for the gates to judge; it never integrates one",
    );

    let record = after.blooms.get(&bloom).unwrap();
    assert!(record.claims.is_empty(), "the member is not resolved by the operator writing its code");
    assert!(!record.wedged.contains_key(&workpiece("wp")), "a cursor that moves is a member dispatching again");
    let cursor = record.progress.get(&workpiece("wp")).unwrap();
    assert_eq!((cursor.stage, cursor.attempts), (StageId::Verify, 1));
    assert_eq!(cursor.candidate, Some(CandidateRef { tree: digest(60), checkout: digest(61) }));
    assert_eq!(record.operator_repairs.len(), 1, "the decider is recorded beside the dispatch");
    assert_eq!(record.operator_repairs[0].operator, "iamacoffeepot");
    assert_eq!(record.operator_repairs[0].reason, "one-line fix, cheaper than a lap");

    // The verify gate then runs for real: a failing verdict over the operator's
    // tree charges the repair roll and routes the member back into `Refine`,
    // exactly as it would over a lane's.
    let (_, judged) = step(
        &after,
        &event(
            "verify-fail",
            Fact::VerifyFailed {
                bloom,
                workpiece: workpiece("wp"),
                evidence: Evidence { subject: digest(60), kind: EvidenceKind::VerificationResult, detail: digest(62) },
                failed_verifiers: VerifyFailureSet::one(VerifyFailure::Clippy),
            },
        ),
    );
    assert!(
        matches!(judged.outcome, Outcome::RefineReentered { .. } | Outcome::AttemptWedged { .. }),
        "the operator's candidate faces the ordinary verdict: {:?}",
        judged.outcome,
    );
}

/// A composition review that passed while recording judgment advisories
/// (#4961) — the reviewer classified findings and marked none of them blocking,
/// so the lane reported a pass and the evidence arrives kinded as the advisory
/// it is.
fn review_passed_with_advisories(bloom: BloomId, key: &str, tree: u8, detail: u8) -> Event {
    event(
        key,
        Fact::AggregateReviewCompleted {
            bloom,
            passed: true,
            evidence: Evidence { subject: digest(tree), kind: EvidenceKind::ReviewAdvisory, detail: digest(detail) },
            implicated: vec![],
        },
    )
}

/// A bloom whose composite gate run is green and whose review is the next fact.
fn awaiting_composition_review() -> (Snapshot, BloomId) {
    let base = Snapshot::new(digest(1));
    let spec = draft(1, vec![membership("alpha", 10), membership("beta", 11)]).seal();
    let bloom = spec.id();
    let (snapshot, _) = step(&base, &event("seal", Fact::Seal(spec)));
    let (snapshot, _) = step(&snapshot, &event("i-a", Fact::Integrate { bloom, claim: claim("alpha", 10, 100) }));
    let (snapshot, _) = step(&snapshot, &event("i-b", Fact::Integrate { bloom, claim: claim("beta", 11, 101) }));
    let (snapshot, _) =
        step(&snapshot, &event("r1", Fact::Resolve { bloom, tree: digest(40), head: digest(41), lineage: vec![] }));
    let (snapshot, _) = step(&snapshot, &verify_passed(bloom, "v1", 40));
    (snapshot, bloom)
}

// #4961 — the owner's requirement, at the reducer: subjective findings must not
// break blooms. A review whose findings are all unmarked judgment advisories
// reports as a pass, and the pass resolves the bloom exactly as a findings-free
// one does — while the advisories still land on the composition's own channel,
// where an operator can adjudicate them and a study can file them forward.
//
// Three tripwires in one. **No repair**: a re-weave dispatch or a spent weave
// budget here is a taste call priced at a model lap, which is the behaviour the
// split exists to end. **No member**: ADR-0191 §4's immutability, the same
// assertion every other composition path carries. And **not silence**: an
// advisory that resolves the bloom without being recorded is a finding the
// pipeline threw away, which is the failure mode on the other side of the same
// change.
#[test]
fn an_advisory_only_review_records_its_findings_and_costs_the_composition_nothing() {
    let (green, bloom) = awaiting_composition_review();
    let (after, decided) = step(&green, &review_passed_with_advisories(bloom, "advisory", 40, 72));

    assert!(matches!(decided.outcome, Outcome::Resolved(_)), "an advisory pass resolves: {:?}", decided.outcome);
    assert_no_member_dispatch(&decided, "an advisory finding is recorded, it never re-opens a member");
    assert!(
        !decided.effects.iter().any(|effect| matches!(
            effect,
            Decision::DispatchAttempt { workpiece, stage: StageId::Refine, .. } if workpiece.is_composition()
        )),
        "an advisory finding buys no weave repair: {:?}",
        decided.effects,
    );
    assert!(
        decided.effects.iter().any(|effect| matches!(effect, Decision::DispatchLand { .. })),
        "and it does not delay the landing either",
    );

    let record = after.blooms.get(&bloom).unwrap();
    assert!(!record.progress.contains_key(&composition()), "no weave repair means no composition cursor moved");
    assert!(record.wedged.is_empty(), "and nothing wedged");
    assert_eq!(record.composition_findings.len(), 1, "the advisory is recorded, not discarded");
    assert_eq!(record.composition_findings[0].detail, digest(72), "naming the review record that raised it");
    assert_eq!(record.composition_findings[0].subject, digest(40), "against the weave it was raised on");
    assert_eq!(record.open_composition_findings().count(), 1, "and it is open for an operator to answer");

    // Adjudicable through #4957's ordinary door, which is what "recorded" has to
    // mean for a finding nothing else will ever act on.
    let (adjudged, closed) = step(&after, &adjudicated(bloom, "adj", vec![72]));
    assert!(matches!(closed.outcome, Outcome::FindingsAdjudicated { .. }), "got {:?}", closed.outcome);
    assert_eq!(adjudged.blooms.get(&bloom).unwrap().open_composition_findings().count(), 0);
}

// The other half of the same rule: a pass that recorded nothing files nothing.
// Tripwire — a reducer that filed a finding on every passing review would fill
// the channel with rows no verdict raised, and `open_composition_findings` is
// what an operator reads to decide whether a bloom needs them at all.
#[test]
fn an_ordinary_passing_review_files_no_finding() {
    let (green, bloom) = awaiting_composition_review();
    let (after, decided) = step(&green, &review_passed(bloom, "clean", 40));

    assert!(matches!(decided.outcome, Outcome::Resolved(_)), "got {:?}", decided.outcome);
    assert!(after.blooms.get(&bloom).unwrap().composition_findings.is_empty());
}

/// An operator hold or release edge, in the words a real one carries.
fn brake_words(reason: &str) -> OperatorHold {
    OperatorHold { reason: reason.into(), operator: "iamacoffeepot".into() }
}

/// A hold on `bloom`.
fn held(bloom: BloomId, key: &str, reason: &str) -> Event {
    event(key, Fact::OperatorHold { bloom, hold: brake_words(reason) })
}

/// A release of `bloom`.
fn released(bloom: BloomId, key: &str, reason: &str) -> Event {
    event(key, Fact::OperatorRelease { bloom, release: brake_words(reason) })
}

/// A completed `Construct` attempt for one member, passing or failing.
fn construct_completed(bloom: BloomId, key: &str, name: &str, passed: bool, capture: Option<u8>) -> Event {
    event(
        key,
        Fact::AttemptCompleted {
            bloom,
            workpiece: workpiece(name),
            stage: StageId::Construct,
            passed,
            evidence: attempt_evidence(),
            candidate: capture.map(|tree| CandidateRef { tree: digest(tree), checkout: digest(tree + 1) }),
        },
    )
}

/// Every member dispatch one reduction decided, as (workpiece, stage) pairs.
fn member_dispatches(decided: &Decisions) -> Vec<(WorkpieceId, StageId)> {
    decided
        .effects
        .iter()
        .filter_map(|effect| match effect {
            Decision::DispatchAttempt { workpiece, stage, .. } => Some((workpiece.clone(), *stage)),
            _ => None,
        })
        .collect()
}

/// A three-member bloom on the operator brake, with `alpha` and `beta`'s
/// `Construct` laps already returned and `gamma`'s still running.
///
/// The shape the hold exists for: two members whose next dispatch the hold
/// swallowed, and one whose worker is still out — which is the pair a release
/// has to be able to tell apart.
fn held_with_two_laps_returned() -> (Snapshot, BloomId) {
    let base = Snapshot::new(digest(1));
    let spec = draft(1, vec![membership("alpha", 10), membership("beta", 11), membership("gamma", 12)]).seal();
    let bloom = spec.id();
    let (snapshot, _) = step(&base, &event("seal", Fact::Seal(spec)));
    let (snapshot, _) =
        step(&snapshot, &held(bloom, "hold", "wave-1 is spending against a refusal that will not clear"));
    let (snapshot, _) = step(&snapshot, &construct_completed(bloom, "alpha-pass", "alpha", true, Some(90)));
    let (snapshot, _) = step(&snapshot, &construct_completed(bloom, "beta-fail", "beta", false, None));
    (snapshot, bloom)
}

// #4976 acceptance 1 — a held bloom dispatches nothing, and everything else
// about it keeps working.
//
// This is the whole difference between the brake and the only move that existed
// before it, which was killing the coordinator: the laps already running finish,
// their evidence lands in the journal, their cursors move, and a member that
// spends its last attempt still wedges. What the hold withholds is the *next*
// work order and nothing else.
//
// Tripwire on both halves. A hold that let a dispatch through spends the money
// the operator pulled the brake to stop; a hold that swallowed the fact instead
// of the dispatch would lose the returning lap's evidence, which is exactly the
// stranding the kill caused.
#[test]
fn a_held_bloom_dispatches_nothing_while_its_running_laps_still_journal() {
    let base = Snapshot::new(digest(1));
    let spec = draft(1, vec![membership("alpha", 10), membership("beta", 11)]).seal();
    let bloom = spec.id();
    let (sealed, seal_decided) = step(&base, &event("seal", Fact::Seal(spec)));
    assert_eq!(member_dispatches(&seal_decided).len(), 2, "the seal door dispatched both members' entry stage");

    let (snapshot, decided) = step(&sealed, &held(bloom, "hold", "the fixture stall is not going to clear"));
    assert!(matches!(decided.outcome, Outcome::BloomHeld { .. }), "got {:?}", decided.outcome);
    let record = snapshot.blooms.get(&bloom).unwrap();
    assert_eq!(record.operator_hold.as_ref().map(|hold| hold.operator.as_str()), Some("iamacoffeepot"));
    assert_eq!(record.operator_hold.as_ref().unwrap().reason, "the fixture stall is not going to clear");

    // alpha's lap comes back green: its cursor advances and its evidence lands,
    // but the Verify it earned does not go out.
    let (snapshot, advanced) = step(&snapshot, &construct_completed(bloom, "alpha-pass", "alpha", true, Some(90)));
    assert!(member_dispatches(&advanced).is_empty(), "a held bloom dispatches nothing: {:?}", advanced.effects);
    assert!(
        advanced.effects.iter().any(|effect| matches!(effect, Decision::RecordEvidence { .. })),
        "the returning lap still journals its evidence",
    );

    // beta's comes back red inside its budget: the retry is counted and withheld
    // the same way.
    let (snapshot, retried) = step(&snapshot, &construct_completed(bloom, "beta-fail", "beta", false, None));
    assert!(member_dispatches(&retried).is_empty(), "including a retry: {:?}", retried.effects);

    let record = snapshot.blooms.get(&bloom).unwrap();
    assert_eq!(record.progress.get(&workpiece("alpha")).unwrap().stage, StageId::Verify, "alpha's cursor moved");
    assert_eq!(record.progress.get(&workpiece("beta")).unwrap().attempts, 2, "beta's attempt was counted");
    assert_eq!(record.evidence.len(), 2, "both laps are in the evidence log");
    assert_eq!(
        record.deferred_dispatches,
        BTreeSet::from([workpiece("alpha"), workpiece("beta")]),
        "and both are owed the dispatch the hold swallowed",
    );
    // The ledger counts what was *spent* (ADR-0180), and a swallowed dispatch
    // spent nothing — a hold that inflated it would corrupt every retry grade
    // computed from it.
    assert_eq!(
        record.dispatches.get(&DispatchKey::Member { workpiece: workpiece("alpha"), stage: StageId::Verify }),
        None,
        "a deferral is not a dispatch",
    );
}

// #4976 acceptance 2 — the release re-derives exactly what is due: none lost,
// none doubled.
//
// The two failure modes are opposite and both fatal. Dispatching from every
// cursor would put a second worker on `gamma`, whose lap never came back and
// whose cursor therefore looks identical to a member the hold caught — that is
// the "doubled" side, and it is why the deferrals are recorded rather than
// inferred. Dispatching nothing, or dispatching from a position captured when
// the brake went on, is the "lost" side: `alpha` moved *while held*, so the
// dispatch it is owed is the `Verify` it is sitting at now, not the `Construct`
// that was in flight when the operator pulled the brake.
#[test]
fn releasing_dispatches_exactly_what_the_hold_owed_and_nothing_else() {
    let (snapshot, bloom) = held_with_two_laps_returned();

    let (after, decided) = step(&snapshot, &released(bloom, "release", "the fixture is fixed; let it run"));

    assert!(
        matches!(decided.outcome, Outcome::BloomReleased { ref dispatched, .. }
            if *dispatched == vec![workpiece("alpha"), workpiece("beta")]),
        "got {:?}",
        decided.outcome,
    );
    let mut dispatched = member_dispatches(&decided);
    dispatched.sort_by(|left, right| left.0.0.cmp(&right.0.0));
    assert_eq!(
        dispatched,
        vec![(workpiece("alpha"), StageId::Verify), (workpiece("beta"), StageId::Construct)],
        "alpha resumes where the hold left it, beta re-runs the attempt it is owed, and gamma is untouched",
    );
    // The dispatch is re-derived, not recalled: alpha's returned candidate is
    // what its Verify runs against, which is a fact the record only holds
    // because the completion folded *during* the hold.
    match decided
        .effects
        .iter()
        .find(|effect| matches!(effect, Decision::DispatchAttempt { stage: StageId::Verify, .. }))
    {
        Some(Decision::DispatchAttempt { candidate, transformation, .. }) => {
            assert_eq!(*candidate, Some(digest(90)), "the Verify binds the tree the held lap captured");
            assert_eq!(transformation.checkout, digest(91), "and checks out the commit it captured");
        }
        other => panic!("expected alpha's re-derived Verify dispatch, got {other:?}"),
    }

    let record = after.blooms.get(&bloom).unwrap();
    assert!(record.operator_hold.is_none(), "the brake is off");
    assert!(record.deferred_dispatches.is_empty(), "and the dispatches that went out are no longer owed");

    // Nothing is owed twice: releasing again is refused, and a fresh hold on the
    // released bloom starts from an empty set rather than replaying the old one.
    assert!(matches!(
        reduce(&after, &released(bloom, "again", "second try"), &ResolvedConfigs::default(), &SpendWindow::default())
            .outcome,
        Outcome::OperatorHoldRejected(OperatorHoldError::NotHeld),
    ));
    let (rehold, _) = step(&after, &held(bloom, "hold-2", "on second thoughts"));
    let (_, rereleased) = step(&rehold, &released(bloom, "release-2", "no, it was fine"));
    assert!(member_dispatches(&rereleased).is_empty(), "a hold that swallowed nothing owes nothing");
}

// #4976 — the deferral ledger is journal-derived like everything else on the
// record (ADR-0190): folding the recorded decisions alone, with no reducer in
// the loop, rebuilds the same bloom.
//
// Tripwire: a set the reducer kept as host state, or one the release cleared
// itself instead of letting each dispatch clear its own entry, would replay to a
// different bloom — and a replayed coordinator would then either re-dispatch
// work that already went out or sit on work it thinks it still owes.
#[test]
fn a_hold_and_its_release_replay_from_the_recorded_decisions_alone() {
    let base = Snapshot::new(digest(1));
    let spec = draft(1, vec![membership("alpha", 10), membership("beta", 11), membership("gamma", 12)]).seal();
    let bloom = spec.id();
    let script = vec![
        event("seal", Fact::Seal(spec)),
        held(bloom, "hold", "freeze it while I read the run"),
        construct_completed(bloom, "alpha-pass", "alpha", true, Some(90)),
        construct_completed(bloom, "beta-fail", "beta", false, None),
        released(bloom, "release", "read it; it is fine"),
    ];

    let mut live = base.clone();
    let mut recorded = Vec::new();
    for step_event in &script {
        let decided = reduce(&live, step_event, &ResolvedConfigs::default(), &SpendWindow::default());
        live = live.apply(step_event, &decided, &ResolvedConfigs::default());
        recorded.push(decided);
    }

    let mut replayed = base;
    for (step_event, decided) in script.iter().zip(recorded.iter()) {
        replayed = replayed.apply(step_event, decided, &ResolvedConfigs::default());
    }

    assert_eq!(replayed, live, "replay over the recorded decisions rebuilds the held-and-released bloom exactly");
}

// #4976 acceptance 3 — both edges state a reason and an operator, and both are
// refused when they do not.
//
// A brake pulled on a running bloom is an act no verdict produced, so the record
// of who pulled it and why is its whole product — the same discipline #4957's
// doors carry, for the same reason. The idempotence answers ride here too: a
// second hold and a release of an unheld bloom are both refused rather than
// absorbed, so the journal never carries an edge that changed nothing and a
// second hold cannot overwrite the reason the first recorded.
#[test]
fn both_brake_edges_state_a_reason_and_who_or_are_refused() {
    let base = Snapshot::new(digest(1));
    let spec = draft(1, vec![membership("alpha", 10)]).seal();
    let bloom = spec.id();
    let (sealed, _) = step(&base, &event("seal", Fact::Seal(spec)));

    let blank = |key: &str, fact: Fact| {
        reduce(&sealed, &event(key, fact), &ResolvedConfigs::default(), &SpendWindow::default()).outcome
    };
    let empty_operator = OperatorHold { reason: "stated".into(), operator: "  ".into() };
    for (label, outcome, expected) in [
        (
            "a hold with a whitespace reason",
            blank("h1", Fact::OperatorHold { bloom, hold: brake_words("   ") }),
            OperatorHoldError::BlankReason,
        ),
        (
            "a hold naming nobody",
            blank("h2", Fact::OperatorHold { bloom, hold: empty_operator.clone() }),
            OperatorHoldError::BlankOperator,
        ),
        (
            "a release with a whitespace reason",
            blank("r1", Fact::OperatorRelease { bloom, release: brake_words("") }),
            OperatorHoldError::BlankReason,
        ),
        (
            "a release naming nobody",
            blank("r2", Fact::OperatorRelease { bloom, release: empty_operator }),
            OperatorHoldError::BlankOperator,
        ),
        (
            "a release of a bloom that is not held",
            blank("r3", Fact::OperatorRelease { bloom, release: brake_words("let it go") }),
            OperatorHoldError::NotHeld,
        ),
    ] {
        assert!(
            matches!(outcome, Outcome::OperatorHoldRejected(ref error) if *error == expected),
            "{label} must be refused as {expected:?}, got {outcome:?}",
        );
    }

    let (snapshot, decided) = step(&sealed, &held(bloom, "hold", "the run looks wrong; stopping the spend"));
    assert_eq!(
        decided.effects,
        vec![Decision::RecordOperatorHold { bloom, hold: brake_words("the run looks wrong; stopping the spend") }],
        "the hold's whole product is the record of it",
    );
    assert!(matches!(
        reduce(&snapshot, &held(bloom, "hold-again", "again"), &ResolvedConfigs::default(), &SpendWindow::default())
            .outcome,
        Outcome::OperatorHoldRejected(OperatorHoldError::AlreadyHeld),
    ));
    assert_eq!(
        snapshot.blooms.get(&bloom).unwrap().operator_hold.as_ref().unwrap().reason,
        "the run looks wrong; stopping the spend",
        "and the first hold's reason is what a reader of the frozen bloom finds",
    );

    let (_, released_decided) = step(&snapshot, &released(bloom, "release", "read the run; it was fine"));
    match released_decided.effects.first() {
        Some(Decision::RecordOperatorRelease { release, .. }) => {
            assert_eq!(release.reason, "read the run; it was fine", "the release carries its own words");
            assert_eq!(release.operator, "iamacoffeepot");
        }
        other => panic!("expected the release record, got {other:?}"),
    }
}

// #4976 — the hold composes with the two ways a bloom already stops rather than
// replacing either.
//
// A park and a wedge are the machine saying "I cannot go on"; a hold is an
// operator saying "do not go on yet". They are independent facts about the same
// bloom, so holding must not answer a park, releasing must not answer one, and a
// release must not become a way to un-wedge a member that spent its budget —
// which would make the brake a retry grant wearing a different name.
#[test]
fn a_hold_leaves_the_park_and_the_wedge_exactly_where_they_were() {
    let (parked, bloom) = parked_at_the_review_ceiling();
    let question = parked.blooms.get(&bloom).unwrap().review_park.unwrap();

    let (held_parked, _) = step(&parked, &held(bloom, "hold", "freeze it while I read the review"));
    let record = held_parked.blooms.get(&bloom).unwrap();
    assert_eq!(record.review_park, Some(question), "holding a parked bloom is a no-op on the park");
    assert!(record.holds.contains(&question), "and on the hold it raised");

    let (let_go, decided) = step(&held_parked, &released(bloom, "release", "still thinking, but let it run"));
    let record = let_go.blooms.get(&bloom).unwrap();
    assert_eq!(record.review_park, Some(question), "releasing does not answer the park either");
    assert!(record.holds.contains(&question));
    assert!(member_dispatches(&decided).is_empty(), "the bloom comes off the brake and stops on its own question");

    // The wedge half: a member that spends its last attempt while held wedges,
    // and the release dispatches it no more than an unheld wedge would.
    let base = Snapshot::new(digest(1));
    let spec = draft(1, vec![membership("wp", 10)]).seal();
    let wedging = spec.id();
    let (snapshot, _) = step(&base, &event("seal", Fact::Seal(spec)));
    let (snapshot, _) = step(&snapshot, &held(wedging, "hold-w", "stop it before it burns the budget"));
    let (snapshot, _) = step(&snapshot, &construct_completed(wedging, "f1", "wp", false, None));
    let (snapshot, wedged_decided) = step(&snapshot, &construct_completed(wedging, "f2", "wp", false, None));
    assert!(matches!(wedged_decided.outcome, Outcome::AttemptWedged { .. }), "got {:?}", wedged_decided.outcome);

    assert!(
        snapshot.blooms.get(&wedging).unwrap().deferred_dispatches.is_empty(),
        "the wedge cancels the deferral its earlier retry recorded",
    );

    let (after, decided) = step(&snapshot, &released(wedging, "release-w", "leave it stopped"));
    assert!(member_dispatches(&decided).is_empty(), "a release is not a grant: a wedged member stays wedged");
    assert!(after.blooms.get(&wedging).unwrap().wedged.contains_key(&workpiece("wp")));
}

// #4976 — every reduce path that can dispatch a member is gated, enumerated.
//
// The guard itself is structural: `Decision::DispatchAttempt` is built in
// exactly two places, and the post-seal one takes its hold flag off the
// `SealedLine` that is the only way to reach it — so a dispatch path added later
// inherits the gate whether or not its author thought about it. (The other is
// the seal door's entry dispatch, which no hold can reach: a hold names an
// existing bloom and a seal is what creates one.)
//
// This is the behavioural half, and the bug it catches is the one the structural
// argument cannot: a future path that builds its `SealedLine` by hand, or reaches
// for `SealedLine::released` because a test was failing. Every fact family below
// dispatches a member on an unheld bloom; none of them may on a held one.
#[test]
fn no_fact_family_dispatches_a_member_of_a_held_bloom() {
    let base = Snapshot::new(digest(1));
    let spec = draft(1, vec![membership("alpha", 10), membership("beta", 11)]).seal();
    let bloom = spec.id();
    let (sealed, _) = step(&base, &event("seal", Fact::Seal(spec)));
    // A member at terminal Verify holding a candidate and a sibling still on its
    // first `Construct` attempt — between them, the position every dispatching
    // member fact below acts from.
    let (running, _) = step(&sealed, &construct_completed(bloom, "alpha-pass", "alpha", true, Some(90)));
    let (frozen, _) = step(&running, &held(bloom, "hold", "stop the spend while I read it"));

    let families: Vec<(&str, Event)> = vec![
        ("a passing attempt advancing the line", construct_completed(bloom, "f1", "beta", true, Some(92))),
        ("a failing attempt retrying inside its budget", construct_completed(bloom, "f2", "beta", false, None)),
        (
            "a failing terminal Verify re-entering Refine",
            verify_failed("f3", bloom, "alpha", digest(90), 91, verifier_set(&[VerifyFailure::Clippy])),
        ),
    ];
    for (label, fact) in families {
        let unheld = reduce(&running, &fact, &ResolvedConfigs::default(), &SpendWindow::default());
        assert!(!member_dispatches(&unheld).is_empty(), "fixture bug: {label} must dispatch on an unheld bloom");
        let gated = reduce(&frozen, &fact, &ResolvedConfigs::default(), &SpendWindow::default());
        assert!(member_dispatches(&gated).is_empty(), "{label} dispatched on a held bloom: {:?}", gated.effects);
    }

    // A fold collision opens a reconcile round, which only an integrated member
    // can be in — so it is enumerated from the fixture that has claims.
    let (folding, folding_bloom) = three_members_with_claims();
    let collision = fold_conflict(folding_bloom, "collide", "alpha", 93, 94, 95);
    assert!(
        !member_dispatches(&reduce(&folding, &collision, &ResolvedConfigs::default(), &SpendWindow::default()))
            .is_empty(),
        "fixture bug: a fold conflict must dispatch a reconcile on an unheld bloom",
    );
    let (folding_frozen, _) = step(&folding, &held(folding_bloom, "hold-fold", "stop the reconcile spend"));
    let gated = reduce(&folding_frozen, &collision, &ResolvedConfigs::default(), &SpendWindow::default());
    assert!(member_dispatches(&gated).is_empty(), "a reconcile dispatched on a held bloom: {:?}", gated.effects);

    // A grant is a budget move, which a hold does not gate — so it is admitted
    // and its lap is deferred by the same choke, rather than refused.
    let (wedged, _) = step(&frozen, &construct_completed(bloom, "beta-1", "beta", false, None));
    let (wedged, _) = step(&wedged, &construct_completed(bloom, "beta-2", "beta", false, None));
    let grant = event(
        "grant",
        Fact::GrantAttempts { bloom, workpiece: workpiece("beta"), stage: StageId::Construct, attempts: 1 },
    );
    let (granted, decided) = step(&wedged, &grant);
    assert!(matches!(decided.outcome, Outcome::AttemptsGranted { .. }), "got {:?}", decided.outcome);
    assert!(member_dispatches(&decided).is_empty(), "and the lap it bought waits for the release");
    assert!(granted.blooms.get(&bloom).unwrap().deferred_dispatches.contains(&workpiece("beta")));

    // An operator repair is refused outright: its entire product is the `Verify`
    // dispatch, so admitting it would record a candidate, dispatch nothing, and
    // answer the operator as though their fix were being judged.
    let repair = event(
        "repair",
        Fact::OperatorRepair {
            bloom,
            repair: OperatorRepair {
                workpiece: workpiece("beta"),
                candidate: CandidateRef { tree: digest(96), checkout: digest(97) },
                reason: "one-line fix".into(),
                operator: "iamacoffeepot".into(),
            },
        },
    );
    assert!(matches!(
        reduce(&granted, &repair, &ResolvedConfigs::default(), &SpendWindow::default()).outcome,
        Outcome::OperatorRepairRejected(OperatorRepairError::Held),
    ));
    let (let_go, _) = step(&granted, &released(bloom, "release", "fixed"));
    assert!(
        matches!(
            reduce(&let_go, &repair, &ResolvedConfigs::default(), &SpendWindow::default()).outcome,
            Outcome::OperatorRepairRejected(_)
        ),
        "and past the release it is judged on its own terms again",
    );
}

// #4976 / ADR-0191 §5 — the composition is a workpiece, so its weave repair is
// held like any other member's lap and re-derived like one.
//
// Tripwire on the composition's own line: the release has to rebuild a
// bloom-wide dispatch aimed at the weave, not a member-layered one aimed at a
// scope revision, and the composition is the one workpiece the membership list
// cannot answer for.
#[test]
fn a_held_composition_defers_its_weave_repair_and_resumes_it_on_release() {
    let (green, bloom) = awaiting_composition_review();
    let (frozen, _) = step(&green, &held(bloom, "hold", "the critic keeps refusing; stop before the next lap"));

    let (refused, decided) = step(&frozen, &review_refused(bloom, "refuse", 40, 70));
    assert!(matches!(decided.outcome, Outcome::CompositionRewoven { .. }), "got {:?}", decided.outcome);
    assert!(member_dispatches(&decided).is_empty(), "the weave repair is withheld like any other lap");
    let record = refused.blooms.get(&bloom).unwrap();
    assert_eq!(record.composition_findings.len(), 1, "the finding is still filed — a hold gates dispatch, not facts");
    assert_eq!(composition_cursor(record).stage, StageId::Refine, "and the composition's cursor still moved");

    let (after, decided) = step(&refused, &released(bloom, "release", "read it; re-weave"));

    match decided.effects.iter().find(|effect| matches!(effect, Decision::DispatchAttempt { .. })) {
        Some(Decision::DispatchAttempt { workpiece, stage, candidate, transformation, .. }) => {
            assert!(workpiece.is_composition(), "the owed dispatch is the composition's");
            assert_eq!(*stage, StageId::Refine, "at its weave repair");
            assert_eq!(*candidate, Some(digest(40)), "aimed at the refused weave");
            assert_eq!(transformation.checkout, digest(41), "checking out the commit that carries it");
        }
        other => panic!("expected the composition's re-derived weave repair, got {other:?}"),
    }
    assert!(after.blooms.get(&bloom).unwrap().deferred_dispatches.is_empty());
}

/// A two-member bloom whose claim set is complete and whose fold has not run.
///
/// The position `Fact::Resolve` acts from: every member has a claim, the
/// integration reactor has not yet handed back a tree.
fn ready_to_fold() -> (Snapshot, BloomId) {
    let base = Snapshot::new(digest(1));
    let spec = draft(1, vec![membership("alpha", 10), membership("beta", 11)]).seal();
    let bloom = spec.id();
    let (snapshot, _) = step(&base, &event("seal", Fact::Seal(spec)));
    let (snapshot, _) = step(&snapshot, &event("i-a", Fact::Integrate { bloom, claim: claim("alpha", 10, 100) }));
    let (snapshot, _) = step(&snapshot, &event("i-b", Fact::Integrate { bloom, claim: claim("beta", 11, 101) }));
    (snapshot, bloom)
}

/// A completed fold over the two-member fixture, as `Fact::Resolve` states it.
fn folded(bloom: BloomId, key: &str, tree: u8) -> Event {
    event(key, Fact::Resolve { bloom, tree: digest(tree), head: digest(tree.wrapping_add(1)), lineage: vec![] })
}

// #5100 — a held bloom must not launch aggregate verify or review.
//
// The hold already swallowed `DispatchAttempt`. The two aggregate gates ride
// their own decision paths, so a fold that completed while the brake was on
// still launched the compiler and then the paid critic — the spend the hold
// on bloom `a4e40021` named and failed to stop. Tripwire: a hold that still
// lets either aggregate work order out, or one that swallowed the fold
// itself instead of only the dispatch.
#[test]
fn a_held_bloom_defers_aggregate_verify_from_a_completed_fold() {
    let (ready, bloom) = ready_to_fold();
    let (frozen, _) = step(&ready, &held(bloom, "hold", "prevent repeated paid subject-only dispatches"));

    let (after, decided) = step(&frozen, &folded(bloom, "resolve", 40));
    assert!(
        matches!(decided.outcome, Outcome::AggregateVerifyDispatched { roll: 1, .. }),
        "the fold still reduces: {:?}",
        decided.outcome,
    );
    assert!(
        !decided.effects.iter().any(|effect| matches!(effect, Decision::DispatchAggregateVerify { .. })),
        "a held bloom must not launch aggregate verify: {:?}",
        decided.effects,
    );
    assert!(
        !decided.effects.iter().any(|effect| matches!(effect, Decision::DispatchAggregateReview { .. })),
        "and must not skip ahead to the critic either: {:?}",
        decided.effects,
    );
    assert!(
        decided
            .effects
            .iter()
            .any(|effect| { matches!(effect, Decision::DeferAggregate { stage: StageId::AggregateVerify, .. }) }),
        "the withheld gate is recorded so release can rebuild it: {:?}",
        decided.effects,
    );

    let record = after.blooms.get(&bloom).unwrap();
    assert!(record.integration.is_some(), "the fold is still held — a hold gates dispatch, not facts");
    assert_eq!(record.deferred_aggregates, BTreeSet::from([StageId::AggregateVerify]));
    assert_eq!(
        record.dispatches.get(&DispatchKey::Bloom { stage: StageId::AggregateVerify }),
        None,
        "a deferral is not a dispatch",
    );
}

// #5100 — the critic is the paid half of the same hole.
//
// An in-flight verify that returns green while the bloom is held is work
// already running: it journals and the fold stays. What it must not do is
// launch the review that spend is trying to stop. Tripwire: a passing
// aggregate verify that still emits `DispatchAggregateReview` under a hold.
#[test]
fn a_held_bloom_defers_aggregate_review_from_a_passing_verify() {
    let (ready, bloom) = ready_to_fold();
    let (folding, _) = step(&ready, &folded(bloom, "resolve", 40));
    let (frozen, _) = step(&folding, &held(bloom, "hold", "stop the critic before it spends"));

    let (after, decided) = step(&frozen, &verify_passed(bloom, "verify", 40));
    assert!(matches!(decided.outcome, Outcome::AggregateVerifyPassed { rolls: 1, .. }), "got {:?}", decided.outcome);
    assert!(
        !decided.effects.iter().any(|effect| matches!(effect, Decision::DispatchAggregateReview { .. })),
        "a held bloom must not launch the critic: {:?}",
        decided.effects,
    );
    assert!(
        decided
            .effects
            .iter()
            .any(|effect| { matches!(effect, Decision::DeferAggregate { stage: StageId::AggregateReview, .. }) }),
        "the withheld critic is recorded: {:?}",
        decided.effects,
    );
    assert_eq!(after.blooms.get(&bloom).unwrap().deferred_aggregates, BTreeSet::from([StageId::AggregateReview]),);
}

// #5100 — release re-derives the swallowed aggregate from the fold as it
// stands, the same way it re-derives a member lap from the cursor.
//
// Two failure modes. Dispatching nothing strands the compiler the fold
// already earned. Dispatching a gate that was already in flight puts a
// second worker on a running verify. The recorded deferral is what tells
// those two folds apart.
#[test]
fn releasing_rederives_the_aggregate_verify_the_hold_owed() {
    let (ready, bloom) = ready_to_fold();
    let (frozen, _) = step(&ready, &held(bloom, "hold", "stop the fold's compiler"));
    let (owed, _) = step(&frozen, &folded(bloom, "resolve", 40));

    let (after, decided) = step(&owed, &released(bloom, "release", "read it; let the compiler run"));
    match decided.effects.iter().find(|effect| matches!(effect, Decision::DispatchAggregateVerify { .. })) {
        Some(Decision::DispatchAggregateVerify { transformation, roll, .. }) => {
            assert_eq!(transformation.inputs[0], digest(40), "the compiler binds the fold the hold left standing");
            assert_eq!(transformation.checkout, digest(41), "and checks out the head that carries it");
            assert_eq!(*roll, 1);
        }
        other => panic!("expected the re-derived aggregate verify, got {other:?}"),
    }
    assert!(
        !decided.effects.iter().any(|effect| matches!(effect, Decision::DispatchAggregateReview { .. })),
        "release does not skip the compiler and launch the critic: {:?}",
        decided.effects,
    );

    let record = after.blooms.get(&bloom).unwrap();
    assert!(record.operator_hold.is_none(), "the brake is off");
    assert!(record.deferred_aggregates.is_empty(), "and the gate that went out is no longer owed");
}

// #5100 — the critic half of the same release.
#[test]
fn releasing_rederives_the_aggregate_review_the_hold_owed() {
    let (ready, bloom) = ready_to_fold();
    let (folding, _) = step(&ready, &folded(bloom, "resolve", 40));
    let (frozen, _) = step(&folding, &held(bloom, "hold", "stop the critic"));
    let (owed, _) = step(&frozen, &verify_passed(bloom, "verify", 40));

    let (after, decided) = step(&owed, &released(bloom, "release", "let the critic judge it"));
    match decided.effects.iter().find(|effect| matches!(effect, Decision::DispatchAggregateReview { .. })) {
        Some(Decision::DispatchAggregateReview { transformation, roll, .. }) => {
            assert_eq!(transformation.inputs[0], digest(40), "the critic judges the fold the verify already passed");
            assert_eq!(transformation.checkout, digest(41), "and checks out the same head");
            assert_eq!(*roll, 1);
        }
        other => panic!("expected the re-derived aggregate review, got {other:?}"),
    }

    assert!(after.blooms.get(&bloom).unwrap().deferred_aggregates.is_empty());
}

// #5100 — an aggregate already in flight is not a deferral.
//
// Holding after the fold has dispatched its compiler, then releasing before
// that compiler returns, must not launch a second verify. The fold looks
// identical to one whose verify was withheld; only the recorded set tells
// them apart. Tripwire: a release that inferred owed aggregates from the
// held fold alone.
#[test]
fn an_in_flight_aggregate_is_not_redispatched_on_release() {
    let (ready, bloom) = ready_to_fold();
    let (folding, dispatched) = step(&ready, &folded(bloom, "resolve", 40));
    assert!(
        dispatched.effects.iter().any(|effect| matches!(effect, Decision::DispatchAggregateVerify { .. })),
        "fixture bug: the unheld fold must have launched verify",
    );

    let (frozen, _) = step(&folding, &held(bloom, "hold", "the compiler is already out; freeze the rest"));
    let (_, decided) = step(&frozen, &released(bloom, "release", "let the in-flight compiler finish"));
    assert!(
        !decided.effects.iter().any(|effect| {
            matches!(effect, Decision::DispatchAggregateVerify { .. } | Decision::DispatchAggregateReview { .. })
        }),
        "release must not put a second worker on a running aggregate: {:?}",
        decided.effects,
    );
}

// #5100 — the owner's re-arm is a new critic dispatch, so a hold taken
// before they answer still withholds it.
//
// Tripwire: adopt inlined its own `DispatchAggregateReview` and so skipped
// the helper the other critic paths share. A hold that stopped every other
// review and still launched this one would spend the lane the operator just
// braked.
#[test]
fn a_held_bloom_defers_the_review_a_park_adopt_would_rearm() {
    let (parked, bloom) = parked_at_the_review_ceiling();
    let question = parked.blooms.get(&bloom).unwrap().review_park.unwrap();
    let (frozen, _) = step(&parked, &held(bloom, "hold", "do not buy another critic yet"));

    let (after, decided) = step(&frozen, &event("ans", Fact::AdoptAnswer { bloom, answer: answer_adopting(question) }));
    assert!(matches!(decided.outcome, Outcome::AnswerAdopted { .. }), "got {:?}", decided.outcome);
    assert!(
        !decided.effects.iter().any(|effect| matches!(effect, Decision::DispatchAggregateReview { .. })),
        "adopting under a hold must not launch the critic: {:?}",
        decided.effects,
    );
    assert!(
        decided
            .effects
            .iter()
            .any(|effect| { matches!(effect, Decision::DeferAggregate { stage: StageId::AggregateReview, .. }) }),
        "the re-arm is recorded as owed: {:?}",
        decided.effects,
    );
    assert_eq!(after.blooms.get(&bloom).unwrap().review_park, None, "the park still clears — a hold gates dispatch");
}

// #5100 — the aggregate deferral is journal-derived (ADR-0190): folding the
// recorded decisions alone rebuilds the same bloom.
//
// Tripwire: a set the reducer kept as host state, or one the release cleared
// itself instead of letting the dispatch clear its own entry, would replay to
// a different bloom.
#[test]
fn a_held_aggregate_and_its_release_replay_from_the_recorded_decisions_alone() {
    let (ready, bloom) = ready_to_fold();
    let script = vec![
        held(bloom, "hold", "freeze the fold"),
        folded(bloom, "resolve", 40),
        released(bloom, "release", "let the compiler run"),
    ];

    let mut live = ready.clone();
    let mut recorded = Vec::new();
    for step_event in &script {
        let decided = reduce(&live, step_event, &ResolvedConfigs::default(), &SpendWindow::default());
        live = live.apply(step_event, &decided, &ResolvedConfigs::default());
        recorded.push(decided);
    }

    let mut replayed = ready;
    for (step_event, decided) in script.iter().zip(recorded.iter()) {
        replayed = replayed.apply(step_event, decided, &ResolvedConfigs::default());
    }

    assert_eq!(replayed, live, "replay over the recorded decisions rebuilds the held-and-released fold exactly");
}
