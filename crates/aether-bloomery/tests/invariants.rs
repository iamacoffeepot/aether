//! The seven named invariants of ADR-0149, under property tests.
//!
//! Each `proptest!` case is a tripwire on one owned rule of the value
//! vocabulary or the reducer — the sort in `seal`, the all-or-nothing seal
//! transaction, the exact-digest evidence binding, the resolve coverage
//! check, the compare-and-swap land, and the atomic successor inheritance.

#![allow(clippy::unwrap_used)]

mod common;

use aether_bloomery::{
    BloomStatus, Evidence, EvidenceKind, Fact, Outcome, ResolveError, Snapshot, WorkpieceId, reduce,
};
use common::{claim, digest, draft, event, membership, sealed_and_resolved, step, workpiece};
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

    // Invariant 2 — at most one active bloom membership per workpiece.
    #[test]
    fn one_active_bloom_per_workpiece(name in "wp-[a-z]{1,6}") {
        let base = Snapshot::new(digest(1));
        let first = draft(1, vec![membership(&name, 10)]).seal();
        let (after_first, sealed) = step(&base, &event("first", Fact::Seal(first)));
        prop_assert!(matches!(sealed.outcome, Outcome::Sealed(_)));

        let second = draft(2, vec![membership(&name, 11)]).seal();
        let rejected = reduce(&after_first, &event("second", Fact::Seal(second)));
        match rejected.outcome {
            Outcome::SealRejected(conflict) => prop_assert_eq!(conflict.workpiece, workpiece(&name)),
            other => prop_assert!(false, "expected SealRejected, got {other:?}"),
        }
        prop_assert!(rejected.effects.is_empty());
    }

    // Invariant 3 — a failed batch admission leaves no claims: the whole seal
    // aborts, and the free members in the batch stay unclaimed.
    #[test]
    fn failed_batch_admission_leaves_no_claims(free in btree_set(20u8..60u8, 2..4)) {
        let free: Vec<u8> = free.into_iter().collect();
        let base = Snapshot::new(digest(1));
        let held = draft(1, vec![membership("shared", 10)]).seal();
        let (after_held, _) = step(&base, &event("held", Fact::Seal(held)));

        let mut batch = vec![membership("shared", 11)];
        for rev in &free {
            batch.push(membership(&format!("free-{rev}"), *rev));
        }
        let (after_batch, decided) = step(&after_held, &event("batch", Fact::Seal(draft(2, batch).seal())));
        let rejected = matches!(decided.outcome, Outcome::SealRejected(_));
        prop_assert!(rejected);
        for rev in &free {
            let key = workpiece(&format!("free-{rev}"));
            prop_assert!(!after_batch.active.contains_key(&key));
        }
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

        // A resolve after seal-only (before any integrate) must be refused.
        let sealed_only = draft(2, members).seal();
        let bloom2 = sealed_only.id();
        let base = Snapshot::new(digest(2));
        let (after_seal, _) = step(&base, &event("s", Fact::Seal(sealed_only)));
        let early = reduce(&after_seal, &event("r", Fact::Resolve { bloom: bloom2, tree: digest(40), lineage: vec![] }));
        let member_not_integrated =
            matches!(early.outcome, Outcome::ResolveRejected(ResolveError::MemberNotIntegrated { .. }));
        prop_assert!(member_not_integrated);
    }

    // Invariant 6 — landing requires the expected base (compare-and-swap).
    #[test]
    fn landing_requires_expected_base(members in distinct_members(), wrong in 2u8..=255u8) {
        prop_assume!(wrong != 1);
        let (snapshot, spec) = sealed_and_resolved(1, members, 40);
        let bloom = spec.id();

        let bad = reduce(&snapshot, &event("bad", Fact::Land {
            bloom, expected_base: digest(wrong), new_head: digest(50),
        }));
        prop_assert!(matches!(bad.outcome, Outcome::LandRejected(_)));

        let (landed_snapshot, good) = step(&snapshot, &event("good", Fact::Land {
            bloom, expected_base: digest(1), new_head: digest(50),
        }));
        prop_assert!(matches!(good.outcome, Outcome::Landed(_)));
        prop_assert_eq!(landed_snapshot.mainline, digest(50));
    }

    // Invariant 7 — a successor atomically inherits its predecessor's claims.
    #[test]
    fn successor_inherits_predecessor_claims(members in distinct_members()) {
        // Seal a predecessor and integrate one member so it carries a claim.
        let predecessor_spec = draft(1, members.clone()).seal();
        let predecessor = predecessor_spec.id();
        let first_member = predecessor_spec.members()[0].workpiece.clone();
        let base = Snapshot::new(digest(1));
        let (after_seal, _) = step(&base, &event("seal", Fact::Seal(predecessor_spec)));

        let integrated_claim = claim_for(&first_member, 100);
        let (after_integrate, _) = step(&after_seal, &event(
            "i",
            Fact::Integrate { bloom: predecessor, claim: integrated_claim },
        ));
        let predecessor_claims = after_integrate.blooms.get(&predecessor).unwrap().claims.clone();

        // Supersede with a spec differing in base — a distinct id.
        let successor_spec = draft(2, members).seal();
        let successor = successor_spec.id();
        let (after_supersede, decided) = step(&after_integrate, &event(
            "sup",
            Fact::Supersede { predecessor, successor: successor_spec },
        ));
        let superseded = matches!(decided.outcome, Outcome::Superseded { .. });
        prop_assert!(superseded);

        let predecessor_record = after_supersede.blooms.get(&predecessor).unwrap();
        prop_assert_eq!(predecessor_record.status, BloomStatus::Superseded);
        prop_assert_eq!(predecessor_record.superseded_by, Some(successor));

        let successor_record = after_supersede.blooms.get(&successor).unwrap();
        prop_assert_eq!(&successor_record.claims, &predecessor_claims);
    }
}

/// A resolution claim for an existing workpiece id (the fixture `claim` takes
/// a name; this variant takes the id a spec already produced).
fn claim_for(workpiece: &WorkpieceId, candidate: u8) -> aether_bloomery::ResolutionClaim {
    let mut base = claim("placeholder", candidate);
    base.workpiece = workpiece.clone();
    base
}
