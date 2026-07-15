//! Property tests for the projection assembler `view_of` (ADR-0149 §The
//! boundary, as amended by #3471). The assembler is the pure
//! `Snapshot -> ViewDocument` the reconcile port pushes outward; these are
//! tripwires on the membership-fidelity rules it owns — a sealed bloom's
//! document names every member exactly once, and a resolved bloom's document
//! carries a resolution claim for every member.

#![allow(clippy::unwrap_used)]

mod common;

use aether_bloomery::{Fact, Snapshot, WorkpieceId, reduce, view_of};
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

proptest! {
    // Property (a) — the document for a sealed bloom names every member
    // exactly once: no duplicate, no drop. The assembler must fold the spec's
    // canonical membership into `MemberView`s one-to-one.
    #[test]
    fn sealed_bloom_document_names_every_member_once(members in distinct_members()) {
        let snapshot = sealed(members.clone());
        let expected: Vec<WorkpieceId> = members.iter().map(|m| m.workpiece.clone()).collect();

        let view = view_of(&snapshot);
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

        let view = view_of(&snapshot);
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
