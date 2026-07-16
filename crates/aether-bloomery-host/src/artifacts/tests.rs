//! Contract + restart tests for the eviction-free artifacts store (ADR-0149).
//!
//! Each test drives [`ArtifactsCapabilityState`]'s `put` / `get` — the exact
//! methods the `on_put` / `on_get` handlers delegate to — over a real
//! [`ContentStore`](aether_substrate::content_store::ContentStore) rooted at a
//! temp dir: the put/get round-trip, digest dedup, absent-digest `NotFound`,
//! and the load-bearing property that a stored artifact with no name and no
//! pin survives a capability restart (index restore from the atomic sidecars).

#![allow(clippy::unwrap_used)]

use super::kinds::{ArtifactsError, GetResult, PutResult};
use super::runtime::ArtifactsCapabilityState;

/// Open a state over a fresh temp root. Returns the dir guard so the caller
/// keeps the temp root alive for the test's lifetime.
fn temp_state() -> (tempfile::TempDir, ArtifactsCapabilityState) {
    let dir = tempfile::tempdir().unwrap();
    let state = ArtifactsCapabilityState::open(dir.path()).unwrap();
    (dir, state)
}

fn parents(names: &[&str]) -> Vec<String> {
    names.iter().map(|p| (*p).to_owned()).collect()
}

fn put_digest(state: &mut ArtifactsCapabilityState, bytes: &[u8], ps: &[&str]) -> String {
    match state.put(bytes, &parents(ps)) {
        PutResult::Ok { digest } => digest,
        PutResult::Err { error } => panic!("put failed: {error:?}"),
    }
}

#[test]
fn put_then_get_round_trips_bytes_and_parents() {
    let (_dir, mut state) = temp_state();
    let digest = put_digest(&mut state, b"artifact-bytes", &["parent-a", "parent-b"]);

    match state.get(digest.clone()) {
        GetResult::Ok { digest: got, bytes, parents: got_parents } => {
            assert_eq!(got, digest);
            assert_eq!(bytes, b"artifact-bytes");
            assert_eq!(got_parents, parents(&["parent-a", "parent-b"]));
        }
        GetResult::Err { error, .. } => panic!("get failed: {error:?}"),
    }
}

#[test]
fn identical_bytes_dedup_to_one_digest() {
    let (_dir, mut state) = temp_state();
    // Same bytes, different declared parents on the re-put: the digest is the
    // content address, so it is identical and the entry dedups to one.
    let first = put_digest(&mut state, b"same", &["p1"]);
    let second = put_digest(&mut state, b"same", &["p2"]);
    assert_eq!(first, second);
}

#[test]
fn get_of_an_absent_digest_replies_not_found() {
    let (_dir, mut state) = temp_state();
    // No put — a get of any digest is `NotFound`.
    match state.get("deadbeef".to_owned()) {
        GetResult::Err { digest, error } => {
            assert_eq!(digest, "deadbeef");
            assert_eq!(error, ArtifactsError::NotFound);
        }
        GetResult::Ok { .. } => panic!("absent digest resolved to Ok"),
    }
}

#[test]
fn stored_artifact_survives_a_capability_restart_with_no_name_and_no_pin() {
    // The load-bearing property. Put an artifact with no name and no pin, drop
    // the capability state (models the capability restarting), re-open over the
    // same root (index restore from the atomic sidecars), and assert the bytes
    // and parents come back. This proves the eviction-free instance sidesteps
    // both hub-store hazards — pin-not-persisted and unnamed-silent-evict — by
    // not evicting at all.
    // Tripwire: the digest resolves after restart and its parents round-trip.
    let dir = tempfile::tempdir().unwrap();
    let digest = {
        let mut state = ArtifactsCapabilityState::open(dir.path()).unwrap();
        put_digest(&mut state, b"canonical-record", &["parent-1"])
        // `state` drops here — the capability is gone, only the on-disk root remains.
    };

    let mut restored = ArtifactsCapabilityState::open(dir.path()).unwrap();
    match restored.get(digest.clone()) {
        GetResult::Ok { digest: got, bytes, parents: got_parents } => {
            assert_eq!(got, digest);
            assert_eq!(bytes, b"canonical-record");
            assert_eq!(got_parents, parents(&["parent-1"]));
        }
        GetResult::Err { error, .. } => panic!("unnamed, unpinned artifact lost across restart: {error:?}"),
    }
}
