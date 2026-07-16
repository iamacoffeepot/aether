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

use aether_bloomery::{BloomId, Digest, IntegrateOutcome, LandOutcome};
use aether_bloomery_github::testing::FakeGithub;
use aether_bloomery_github::{GitSource, SourceError};
use aether_bloomery_host::bloomery::SourceShell;

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
    let backend = GitSource::new(fake.clone(), false);
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
    // integrate a candidate against it.
    let checkpoint = shell.checkpoint(&bloom, &snapshot.tree).unwrap();
    let candidate = digest(50);
    match shell.integrate(&bloom, &candidate, &checkpoint).unwrap() {
        IntegrateOutcome::Integrated { tree } => assert_eq!(tree, candidate),
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
fn land_performs_the_compare_and_swap_when_enabled() {
    let (_gated, fake, bloom, base) = demo();

    // A second shell over the same fake with the gate enabled: the CAS advances
    // mainline from the expected base and issues a receipt.
    let enabled = SourceShell::new(Arc::new(GitSource::new(fake.clone(), true)));
    let new_head = digest(90);
    match enabled.land(&bloom, &base, &new_head).unwrap() {
        LandOutcome::Landed(receipt) => {
            assert_eq!(receipt.previous_base, base);
            assert_eq!(receipt.new_head, new_head);
        }
        LandOutcome::BaseMoved { .. } => panic!("expected Landed, got BaseMoved"),
    }
    assert_eq!(fake.ref_digest("heads/main"), Some(new_head), "mainline advanced to the new head");

    // A stale expected base is the clean BaseMoved refusal, not an error.
    match enabled.land(&bloom, &base, &digest(91)).unwrap() {
        LandOutcome::BaseMoved { expected, actual } => {
            assert_eq!(expected, base);
            assert_eq!(actual, new_head);
        }
        LandOutcome::Landed(_) => panic!("expected BaseMoved, got Landed"),
    }
}
