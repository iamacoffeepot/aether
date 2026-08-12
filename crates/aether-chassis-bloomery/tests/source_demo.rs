#![cfg(feature = "github")]

//! The ADR-0149 git source-port demo (#3465 step 3 coverage).
//!
//! Drives a synthetic bloom through the source cap shell against the adapter's
//! in-process `FakeGithub` — the "Done" the issue names, end to end with no
//! token and no network:
//!
//! - a base snapshots to a stable commit/tree digest;
//! - the bloom's integration namespace is created and an integrate advances it
//!   against a matching checkpoint;
//! - a checkpoint is recorded, enumerated, and reusable across a simulated
//!   successor (matched by digest);
//! - `land` is refused while `cas_land_enabled` is off, leaving mainline
//!   untouched, and performs the expected-base compare-and-swap only when a
//!   test enables it.
//!
//! `create_namespace` is a bootstrap step on the concrete backend (it is not a
//! `SourceBackend` trait op), so the demo creates the namespace on the
//! `GitSource` before mounting it behind the shell, then drives the port ops
//! through the shell — mirroring how `mirror_demo` builds its projection.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use aether_bloomery::{BloomId, Digest, IntegrateOutcome, LandOutcome, LandProposal};
use aether_bloomery_github::testing::FakeGithub;
use aether_bloomery_github::{GitSource, SourceError};
use aether_chassis_bloomery::bloomery::SourceShell;

fn digest(seed: u8) -> Digest {
    Digest::from_bytes([seed; 32])
}

// Seed a fake with a base commit and a mainline ref at it, create the bloom's
// integration namespace on a gated (`cas_land_enabled = false`) backend, and
// return the shell, the fake handle, the bloom, and the base commit digest.
fn demo() -> (SourceShell, FakeGithub, BloomId, Digest) {
    let fake = FakeGithub::new();
    let base_tree = digest(10);
    let base = fake.seed_base_commit(&base_tree);
    fake.seed_ref_at("heads/main", &base);

    let bloom = BloomId(digest(1));
    let backend = GitSource::new(fake.clone(), Arc::new(fake.clone()), false);
    backend.create_namespace(&bloom, &base).unwrap();
    (SourceShell::new(Arc::new(backend)), fake, bloom, base)
}

#[test]
fn a_synthetic_bloom_snapshots_integrates_and_observes_gated_land() {
    let (shell, fake, bloom, base) = demo();
    let base_tree = digest(10);

    // Snapshot is stable and carries the base tree.
    let snapshot = shell.snapshot(&base).unwrap();
    assert_eq!(shell.snapshot(&base).unwrap(), snapshot, "a base snapshots stably");
    assert_eq!(snapshot.tree, base_tree);

    // Record a checkpoint at the integration branch's current tree, then
    // integrate a candidate against it — the candidate tree's git-object
    // correspondence is seeded (materialized elsewhere) so integrate resolves it.
    let checkpoint = shell.checkpoint(&bloom, &snapshot.tree).unwrap();
    let candidate = digest(50);
    fake.seed_git_object(&candidate);
    match shell.integrate(&bloom, &candidate, &checkpoint).unwrap() {
        IntegrateOutcome::Integrated { tree, head } => {
            assert_eq!(tree, candidate);
            // The integrated head is a distinct landable digest from the
            // artifact tree (#3615), recorded to the produced commit.
            assert_ne!(head, tree, "the integrated head is distinct from the artifact tree");
        }
        other => panic!("expected Integrated, got {other:?}"),
    }

    // The checkpoint is enumerable and reusable across a simulated successor —
    // matched by digest, the property a same-call guard value could never have.
    let reusable = shell.checkpoints(&bloom).unwrap();
    assert!(reusable.iter().any(|c| c.tree == base_tree), "the successor reuses the checkpoint by digest");

    // Land is refused while gated, and mainline is untouched.
    match shell.land(&bloom, &base, &digest(90)) {
        Err(SourceError::LandingDisabled) => {}
        other => panic!("expected LandingDisabled, got {other:?}"),
    }
    assert_eq!(fake.ref_digest("heads/main"), Some(base), "mainline untouched while gated");
}

#[test]
fn land_proposes_the_new_head_and_observes_the_acceptance_when_enabled() {
    let (_gated, fake, bloom, base) = demo();

    // A second shell over the same fake with the gate enabled: the guard passes
    // against the expected base and a landing proposal is opened. The new head's
    // git-object correspondence is seeded so the landing branch resolves.
    let enabled = SourceShell::new(Arc::new(GitSource::new(fake.clone(), Arc::new(fake.clone()), true)));
    let new_head = digest(90);
    fake.seed_git_object(&new_head);
    let number = match enabled.land(&bloom, &base, &new_head).unwrap() {
        LandOutcome::Proposed { number } => number,
        other @ LandOutcome::BaseMoved { .. } => panic!("expected Proposed, got {other:?}"),
    };
    assert_eq!(fake.ref_digest("heads/main"), Some(base), "mainline is protected — proposing never writes it");
    assert_eq!(enabled.poll_land(&bloom, &base, number).unwrap(), LandProposal::Open, "the proposal stands open");

    // Accepting it is what lands the bloom, and the receipt attests the commit
    // mainline actually became rather than the head that was proposed.
    fake.merge_pull_request(number, &"5c".repeat(20));
    let landed = enabled.poll_land(&bloom, &base, number).unwrap();
    let LandProposal::Landed(receipt) = landed else {
        panic!("expected Landed, got {landed:?}")
    };
    assert_eq!(receipt.previous_base, base);
    assert_ne!(receipt.new_head, new_head, "the landed head is the merge commit");

    // Re-issuing this bloom's land adopts the proposal it already opened, even
    // now that mainline has moved off the sealed base — moving is what accepting
    // the proposal *did*, so refusing here would abandon the bloom at the moment
    // it succeeded.
    assert_eq!(
        enabled.land(&bloom, &base, &new_head).unwrap(),
        LandOutcome::Proposed { number },
        "an open proposal is adopted rather than re-judged against the base",
    );

    // For a bloom with no proposal, though, a stale expected base is the clean
    // BaseMoved refusal, not an error — and it proposes nothing.
    fake.seed_ref_at("heads/main", &new_head);
    let successor = BloomId(digest(77));
    match enabled.land(&successor, &base, &digest(91)).unwrap() {
        LandOutcome::BaseMoved { expected, actual } => {
            assert_eq!(expected, base);
            assert_eq!(actual, new_head);
        }
        other @ LandOutcome::Proposed { .. } => panic!("expected BaseMoved, got {other:?}"),
    }
}
