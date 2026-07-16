//! Handler tests for [`SourceCapabilityState`] (ADR-0149 §The boundary).
//!
//! Each test drives the state's inherent methods — the exact methods the
//! `on_*` handlers delegate to — over a real [`SourceShell`] mounted on the
//! `aether-bloomery-github` adapter's in-process `FakeGithub` double (no
//! token, no network), mirroring `tests/source_demo.rs`'s setup. Tripwire: the
//! outcome→reply mapping (decode request → shell call → encode outcome) is
//! this crate's own logic, not a derive or a passthrough.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use aether_bloomery::{BloomId, ClaimOutcome, ClaimRefKind, ClaimSealResult, Digest, ReleaseSealResult, WorkpieceId};
use aether_bloomery_github::GitSource;
use aether_bloomery_github::testing::FakeGithub;
use aether_data::wire::{from_bytes, to_vec};

use super::kinds::{LandResult, SnapshotResult};
use super::runtime::SourceCapabilityState;
use crate::bloomery::SourceShell;

fn digest(seed: u8) -> Digest {
    Digest::from_bytes([seed; 32])
}

fn workpiece(name: &str) -> WorkpieceId {
    WorkpieceId(name.to_owned())
}

fn encoded_workpieces(names: &[&str]) -> Vec<Vec<u8>> {
    names.iter().map(|name| to_vec(&workpiece(name)).unwrap()).collect()
}

/// Seed a fake with a base commit and a mainline ref at it, create the bloom's
/// integration namespace on a gated (`cas_land_enabled = false`) backend, and
/// mount the shell behind fresh state — mirroring `source_demo.rs`'s `demo()`.
fn state_over_fake(cas_land_enabled: bool) -> (SourceCapabilityState, FakeGithub, BloomId, Digest) {
    let fake = FakeGithub::new();
    let base_tree = digest(10);
    let base = fake.seed_base_commit(&base_tree);
    fake.seed_ref_at("heads/main", &base);

    let bloom = BloomId(digest(1));
    let backend = GitSource::new(fake.clone(), cas_land_enabled);
    backend.create_namespace(&bloom, &base).unwrap();
    (SourceCapabilityState::new(SourceShell::new(Arc::new(backend))), fake, bloom, base)
}

#[test]
fn snapshot_decodes_the_request_calls_the_shell_and_encodes_the_reply() {
    let (state, _fake, _bloom, base) = state_over_fake(false);

    let reply = state.snapshot(&to_vec(&base).unwrap());

    let SnapshotResult::Ok { snapshot } = reply else {
        panic!("expected Ok, got {reply:?}")
    };
    let decoded: aether_bloomery::SourceSnapshot = from_bytes(&snapshot).unwrap();
    assert_eq!(decoded.tree, digest(10), "the encoded reply carries the fake's seeded base tree");
}

#[test]
fn land_maps_a_moved_base_to_base_moved() {
    // CAS-land enabled so the port attempts the swap rather than refusing
    // with `LandingDisabled`; the moved-base branch is the mapping under test.
    let (state, fake, bloom, base) = state_over_fake(true);
    // Advance mainline past `base` behind the capability's back — the shape a
    // concurrent land produces.
    let moved = digest(77);
    fake.seed_ref_at("heads/main", &moved);

    let reply = state.land(&to_vec(&bloom).unwrap(), &to_vec(&base).unwrap(), &to_vec(&digest(90)).unwrap());

    let LandResult::BaseMoved { expected, actual } = reply else {
        panic!("expected BaseMoved, got {reply:?}")
    };
    assert_eq!(from_bytes::<Digest>(&expected).unwrap(), base, "expected carries the caller's stale base");
    assert_eq!(from_bytes::<Digest>(&actual).unwrap(), moved, "actual carries mainline's real current head");
}

#[test]
fn claim_seal_decodes_calls_the_shell_and_encodes_an_acquired_outcome() {
    let (state, _fake, bloom, _base) = state_over_fake(false);

    let reply =
        state.claim_seal("seal-key".to_owned(), &to_vec(&bloom).unwrap(), &encoded_workpieces(&["reactor-core"]));

    let ClaimSealResult::Ok { outcome, .. } = reply else {
        panic!("expected Ok, got {reply:?}")
    };
    assert_eq!(from_bytes::<ClaimOutcome>(&outcome).unwrap(), ClaimOutcome::Acquired);
}

#[test]
fn claim_seal_encodes_a_held_conflict_outcome() {
    let (state, _fake, first, _base) = state_over_fake(false);
    // `first` takes the workpiece; a second bloom's claim must encode a `Held`
    // naming the ref kind and holder.
    let _ = state.claim_seal("seal-key".to_owned(), &to_vec(&first).unwrap(), &encoded_workpieces(&["reactor-core"]));
    let second = BloomId(digest(2));

    let reply =
        state.claim_seal("seal-key-2".to_owned(), &to_vec(&second).unwrap(), &encoded_workpieces(&["reactor-core"]));

    let ClaimSealResult::Ok { outcome, .. } = reply else {
        panic!("expected Ok, got {reply:?}")
    };
    assert_eq!(
        from_bytes::<ClaimOutcome>(&outcome).unwrap(),
        ClaimOutcome::Held { ref_kind: ClaimRefKind::Workpiece(workpiece("reactor-core")), held_by: first }
    );
}

#[test]
fn release_seal_decodes_and_releases() {
    let (state, fake, bloom, _base) = state_over_fake(false);
    let _ = state.claim_seal("seal-key".to_owned(), &to_vec(&bloom).unwrap(), &encoded_workpieces(&["reactor-core"]));
    assert!(fake.ref_exists("bloomery/claims/reactor-core"), "claim taken");

    let reply = state.release_seal(&to_vec(&bloom).unwrap(), &encoded_workpieces(&["reactor-core"]));

    assert_eq!(reply, ReleaseSealResult::Ok);
    assert!(!fake.ref_exists("bloomery/claims/reactor-core"), "claim released");
}

#[test]
fn an_unconfigured_cap_acquires_locally_without_touching_github() {
    // Local-backstop-only mode (ADR-0150): an unconfigured instance's seal path
    // never reaches GitHub — claim_seal is a no-op `Acquired` and no ref is
    // written, so exclusivity rests on the local SQLite constraint alone.
    let fake = FakeGithub::new();
    let backend = GitSource::new(fake.clone(), false);
    let state = SourceCapabilityState::new_unconfigured(SourceShell::new(Arc::new(backend)));
    let bloom = BloomId(digest(1));

    let reply =
        state.claim_seal("seal-key".to_owned(), &to_vec(&bloom).unwrap(), &encoded_workpieces(&["reactor-core"]));

    let ClaimSealResult::Ok { outcome, .. } = reply else {
        panic!("expected Ok, got {reply:?}")
    };
    assert_eq!(from_bytes::<ClaimOutcome>(&outcome).unwrap(), ClaimOutcome::Acquired);
    assert!(!fake.ref_exists("bloomery/claims/reactor-core"), "unconfigured mode writes no claim ref");
}
