//! Contract + restart tests for the eviction-free artifacts store (ADR-0149).
//!
//! Each test drives [`ArtifactsCapabilityState`]'s `put` / `get` — the exact
//! methods the `on_put` / `on_get` handlers delegate to — over a real
//! [`ContentStore`](aether_substrate::content_store::ContentStore) rooted at a
//! temp dir: the put/get round-trip, digest dedup, absent-digest `NotFound`,
//! and the load-bearing property that a stored artifact with no name and no
//! pin survives a capability restart (index restore from the atomic sidecars).
//!
//! One root can carry two live handles — the chassis mounts this capability
//! and the executor reactor opens its own handle over the same root — so the
//! last tests drive that arrangement directly.

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

/// Tripwire: the reported bug's own shape (issue #4834). `mounted` is the
/// capability the chassis mounts at boot; `reactor` is the second handle the
/// executor opens over the same root and files study records through. Both
/// are live at once, so `mounted`'s index — built at boot, before any of
/// `reactor`'s writes existed — is not the whole record. A `get` that
/// answered from it alone replies `NotFound` for bytes sitting on disk, which
/// is what `GET /artifacts/{digest}` relays to its caller.
#[test]
fn the_mounted_capability_serves_an_artifact_the_reactors_handle_filed_after_boot() {
    let dir = tempfile::tempdir().unwrap();
    let mut mounted = ArtifactsCapabilityState::open(dir.path()).unwrap();
    let mut reactor = ArtifactsCapabilityState::open(dir.path()).unwrap();

    let digest = put_digest(&mut reactor, b"study-record", &["graded-attempt"]);

    match mounted.get(digest.clone()) {
        GetResult::Ok { digest: got, bytes, parents: got_parents } => {
            assert_eq!(got, digest);
            assert_eq!(bytes, b"study-record");
            assert_eq!(got_parents, parents(&["graded-attempt"]), "the reactor's recorded parents come back");
        }
        GetResult::Err { error, .. } => panic!("the mounted capability cannot see the reactor's write: {error:?}"),
    }
}

/// Tripwire: the enumeration half of the same arrangement, kept separate so it
/// stands on `scan` alone — a prior `get` of the digest would have pulled the
/// entry into `mounted`'s index and hidden the failure. `scan` is what
/// `rebuild_study_index` reads, and it enumerates the index, so without a
/// refresh the rebuild silently projects over only the records this handle
/// wrote itself.
#[test]
fn the_mounted_capabilitys_scan_enumerates_what_the_reactors_handle_filed() {
    let dir = tempfile::tempdir().unwrap();
    let mut mounted = ArtifactsCapabilityState::open(dir.path()).unwrap();
    let mut reactor = ArtifactsCapabilityState::open(dir.path()).unwrap();

    let digest = put_digest(&mut reactor, b"study-record", &["graded-attempt"]);

    let scanned = mounted.scan();
    assert_eq!(scanned.len(), 1, "the projection rebuild enumerates the reactor's record: {scanned:?}");
    assert_eq!(scanned[0].digest, digest);
    assert_eq!(scanned[0].bytes, b"study-record");
    assert_eq!(scanned[0].parents, parents(&["graded-attempt"]));
}

/// Tripwire: a second handle's `put` of bytes the first already stored keeps
/// the first handle's recorded parents. Deduping against only the putting
/// handle's index rewrites the sidecar, so the peer's derivation edge is lost
/// — and `put`'s dropped-parents warning cannot catch it, because to that
/// handle the entry looks new. Provenance loss with nothing logged is the
/// failure this guards.
#[test]
fn a_second_handles_put_does_not_overwrite_the_first_handles_recorded_parents() {
    let dir = tempfile::tempdir().unwrap();
    let mut mounted = ArtifactsCapabilityState::open(dir.path()).unwrap();
    let mut reactor = ArtifactsCapabilityState::open(dir.path()).unwrap();

    let filed = put_digest(&mut reactor, b"same-artifact-bytes", &["the-reactors-parent"]);
    let re_put = put_digest(&mut mounted, b"same-artifact-bytes", &["a-later-parent"]);
    assert_eq!(re_put, filed, "identical bytes address the same artifact across handles");

    for (label, state) in [("the mounted capability", &mut mounted), ("the reactor", &mut reactor)] {
        match state.get(filed.clone()) {
            GetResult::Ok { parents: got_parents, .. } => {
                assert_eq!(got_parents, parents(&["the-reactors-parent"]), "{label} reads the original parents");
            }
            GetResult::Err { error, .. } => panic!("{label} lost the artifact: {error:?}"),
        }
    }
}
