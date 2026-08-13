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

use aether_bloomery::{BloomId, ClaimHolder, ClaimRefKind, ClaimRefState, Digest, WorkpieceId};
use aether_bloomery_github::testing::FakeGithub;
use aether_bloomery_github::{GitSource, MainlineRef};
use aether_data::wire::{from_bytes, to_vec};

use super::kinds::{ClaimResult, CompleteReleaseResult, EnumerateClaimsResult, LandResult, SnapshotResult};
use super::runtime::SourceCapabilityState;
use crate::bloomery::SourceShell;

fn digest(seed: u8) -> Digest {
    Digest::from_bytes([seed; 32])
}

fn workpiece(id: &str) -> WorkpieceId {
    WorkpieceId(id.to_owned())
}

/// State over a plain `cas_land_enabled = false` [`GitSource`] with no seeded
/// namespace — the claim ops act on their own claim refs, not the integration
/// branch, so they need no base commit.
fn claim_state() -> SourceCapabilityState {
    let fake = FakeGithub::new();
    let backend = GitSource::new(fake.clone(), Arc::new(fake), false, MainlineRef::default());
    SourceCapabilityState::new(SourceShell::new(Arc::new(backend)))
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
    let backend = GitSource::new(fake.clone(), Arc::new(fake.clone()), cas_land_enabled, MainlineRef::default());
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
    // concurrent land produces. Seed the moved head's git-object correspondence so
    // the port reverse-resolves the real mainline object to it (ADR-0150).
    let moved = digest(77);
    fake.seed_git_object(&moved);
    fake.seed_ref_at("heads/main", &moved);

    let reply = state.land(&to_vec(&bloom).unwrap(), &to_vec(&base).unwrap(), &to_vec(&digest(90)).unwrap());

    let LandResult::BaseMoved { expected, actual } = reply else {
        panic!("expected BaseMoved, got {reply:?}")
    };
    assert_eq!(from_bytes::<Digest>(&expected).unwrap(), base, "expected carries the caller's stale base");
    assert_eq!(from_bytes::<Digest>(&actual).unwrap(), moved, "actual carries mainline's real current head");
}

#[test]
fn claim_seal_decodes_the_members_and_encodes_acquired() {
    let state = claim_state();
    let claimant = BloomId(digest(1));
    let workpieces = [to_vec(&workpiece("wp-1")).unwrap(), to_vec(&workpiece("wp-2")).unwrap()];

    let reply = state.claim_seal(&to_vec(&claimant).unwrap(), &workpieces);

    assert_eq!(reply, ClaimResult::Acquired, "a fresh acquire of both members and the admission ref");
}

#[test]
fn claim_seal_conflict_encodes_held_with_the_wire_ref_kind_and_holder() {
    // The Held branch wire-encodes both `ref_kind` and `held_by` — this crate's
    // own outcome→reply mapping, mirroring `land`'s BaseMoved encoding.
    let state = claim_state();
    let holder = BloomId(digest(1));
    let contender = BloomId(digest(2));
    let w1 = [to_vec(&workpiece("wp-1")).unwrap()];
    assert_eq!(state.claim_seal(&to_vec(&holder).unwrap(), &w1), ClaimResult::Acquired);

    let reply = state.claim_seal(&to_vec(&contender).unwrap(), &w1);

    let ClaimResult::Held { ref_kind, held_by } = reply else {
        panic!("expected Held, got {reply:?}")
    };
    assert_eq!(
        from_bytes::<ClaimRefKind>(&ref_kind).unwrap(),
        ClaimRefKind::Workpiece(workpiece("wp-1")),
        "ref_kind carries the conflicting member ref",
    );
    assert_eq!(from_bytes::<BloomId>(&held_by).unwrap(), holder, "held_by carries the current holder");
}

#[test]
fn transfer_seal_decodes_every_partition_and_encodes_acquired() {
    let state = claim_state();
    let predecessor = BloomId(digest(1));
    let successor = BloomId(digest(2));
    let w1 = [to_vec(&workpiece("wp-1")).unwrap()];
    assert_eq!(state.claim_seal(&to_vec(&predecessor).unwrap(), &w1), ClaimResult::Acquired);

    // Carry the sole member (and the admission ref) from predecessor to successor.
    let reply = state.transfer_seal(&to_vec(&predecessor).unwrap(), &to_vec(&successor).unwrap(), &w1, &[], &[]);

    assert_eq!(reply, ClaimResult::Acquired, "the carried member and admission ref moved to the successor");
}

#[test]
fn release_seal_decodes_the_members_and_encodes_acquired() {
    let state = claim_state();
    let holder = BloomId(digest(1));
    let w1 = [to_vec(&workpiece("wp-1")).unwrap()];
    assert_eq!(state.claim_seal(&to_vec(&holder).unwrap(), &w1), ClaimResult::Acquired);

    let reply = state.release_seal(&to_vec(&holder).unwrap(), &w1);

    assert_eq!(reply, ClaimResult::Acquired, "the held member and admission ref were released");
}

#[test]
fn enumerate_claims_encodes_the_live_ref_states() {
    // The reply wire-encodes one `ClaimRefState` per live ref — this crate's own
    // outcome→reply mapping over the shell enumeration, mirroring `land`'s encode.
    let state = claim_state();
    let holder = BloomId(digest(1));
    assert_eq!(
        state.claim_seal(&to_vec(&holder).unwrap(), &[to_vec(&workpiece("wp-1")).unwrap()]),
        ClaimResult::Acquired
    );

    let EnumerateClaimsResult::Ok { states } = state.enumerate_claims() else {
        panic!("expected Ok enumeration")
    };
    let mut decoded: Vec<ClaimRefState> = states.iter().map(|bytes| from_bytes(bytes).unwrap()).collect();
    decoded.sort_by(|a, b| format!("{:?}", a.ref_kind).cmp(&format!("{:?}", b.ref_kind)));
    assert_eq!(
        decoded,
        vec![
            ClaimRefState { ref_kind: ClaimRefKind::MainlineAdmission, holder: ClaimHolder::Held(holder) },
            ClaimRefState { ref_kind: ClaimRefKind::Workpiece(workpiece("wp-1")), holder: ClaimHolder::Held(holder) },
        ],
    );
}

#[test]
fn complete_transfer_decodes_the_operands_and_encodes_acquired() {
    let state = claim_state();
    let (predecessor, successor) = (BloomId(digest(1)), BloomId(digest(2)));
    assert_eq!(
        state.claim_seal(&to_vec(&predecessor).unwrap(), &[to_vec(&workpiece("wp-1")).unwrap()]),
        ClaimResult::Acquired
    );

    let ref_kind = to_vec(&ClaimRefKind::Workpiece(workpiece("wp-1"))).unwrap();
    let reply = state.complete_transfer(&to_vec(&predecessor).unwrap(), &to_vec(&successor).unwrap(), &ref_kind);

    assert_eq!(reply, ClaimResult::Acquired, "the per-ref transfer moved the carried ref to the successor");
}

#[test]
fn complete_release_decodes_the_operands_and_reports_the_terminal_it_reached() {
    let state = claim_state();
    let holder = BloomId(digest(1));
    assert_eq!(
        state.claim_seal(&to_vec(&holder).unwrap(), &[to_vec(&workpiece("wp-1")).unwrap()]),
        ClaimResult::Acquired
    );

    let ref_kind = to_vec(&ClaimRefKind::Workpiece(workpiece("wp-1"))).unwrap();
    let reply = state.complete_release(&to_vec(&holder).unwrap(), &ref_kind);

    assert_eq!(reply, CompleteReleaseResult::Released, "naming the holder releases exactly its ref");
    assert_eq!(
        state.complete_release(&to_vec(&holder).unwrap(), &ref_kind),
        CompleteReleaseResult::AlreadyAbsent,
        "re-releasing the same ref is the idempotent terminal a crash-window redrive relies on",
    );
}
