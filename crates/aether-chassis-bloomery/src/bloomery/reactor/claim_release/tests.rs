//! The drain → release core of the claim-release reactor, over a real
//! `SqliteStore` and a fake-GitHub-backed `SourceShell` — the network side the
//! running capability drives, without the mail harness.
//!
//! The bug this reactor exists to close has two halves, and both are pinned
//! here: an orphaned ref must actually become releasable, and a ref a *live*
//! holder owns must not. A regression in either direction is silent at every
//! other layer — the first leaves the coordinator permanently wedged, the second
//! destroys another instance's work — so the pair is the point. A persisted
//! completion is the third pin: a recorded `Changed` or `Released` must keep
//! that variant even after the source holder has moved.

use std::path::{Path, PathBuf};
use std::slice::from_ref;
use std::sync::Arc;

use aether_bloomery::testing::{digest, workpiece};
use aether_bloomery::{
    Admit, BloomId, ClaimHolder, ClaimOutcome, ClaimRefKind, Digest, Event, Fact, OrphanClaimRelease,
    OrphanClaimReleaseCompletion, OrphanClaimReleasePayload, Topic,
};
use aether_bloomery_github::testing::FakeGithub;
use aether_bloomery_github::{GitSource, MainlineRef};
use aether_data::wire::{from_bytes, to_vec};

use super::runtime::drain_and_release;
use crate::bloomery::SourceShell;
use crate::bloomery::outbox::TopicOutbox;
use crate::store::{AppendOutcome, JournalWrite, SqliteStore, StoreBackend};

fn bloom(seed: u8) -> BloomId {
    BloomId(digest(seed))
}

fn shell() -> SourceShell {
    let fake = FakeGithub::new();
    SourceShell::new(Arc::new(GitSource::new(fake.clone(), Arc::new(fake), true, MainlineRef::default())))
}

// Enqueue one authorized release on the release topic — the bytes the reducer's
// `DispatchOrphanClaimRelease` projection enqueues once the signed request is
// admitted.
fn enqueue_release(store: &mut SqliteStore, target: &OrphanClaimRelease) -> u64 {
    let payload = OrphanClaimReleasePayload { request: target.request(), target: target.clone() };
    store
        .enqueue_topic(Topic::OrphanClaimRelease, &to_vec(&payload).expect("the payload encodes"), None)
        .expect("the enqueue lands")
}

// The completion an admit carries, so a test asserts the journaled terminal
// rather than the opaque bytes.
fn admitted_completion(admit: &Admit) -> (Digest, OrphanClaimReleaseCompletion) {
    let event: Event = from_bytes(&admit.event).expect("the admit carries an event");
    match event.fact {
        Fact::CompleteOrphanClaimRelease { request, completion } => (request, completion),
        other => panic!("expected a release completion, got {other:?}"),
    }
}

fn holder_of(source: &SourceShell, ref_kind: &ClaimRefKind) -> Option<ClaimHolder> {
    source
        .enumerate_claims()
        .expect("the enumeration succeeds")
        .into_iter()
        .find(|state| state.ref_kind == *ref_kind)
        .map(|state| state.holder)
}

fn admitted_event(admit: &Admit) -> Event {
    from_bytes(&admit.event).expect("the admit carries an event")
}

fn journal_event(store: &mut SqliteStore, event: &Event) -> AppendOutcome {
    let bytes = to_vec(event).expect("the event encodes");
    store
        .append_event(&JournalWrite {
            idempotency_key: &event.idempotency_key.0,
            event: &bytes,
            decisions: b"decided",
            decider: "test",
        })
        .expect("the journal append lands")
}

fn file_store() -> (tempfile::TempDir, PathBuf, SqliteStore) {
    let dir = tempfile::tempdir().expect("a temp dir binds");
    let path = dir.path().join("store.sqlite");
    let store = SqliteStore::open(path.to_str().expect("utf-8 path")).expect("a file store opens");
    (dir, path, store)
}

fn reopen_store(path: &Path) -> SqliteStore {
    SqliteStore::open(path.to_str().expect("utf-8 path")).expect("the same file reopens")
}

fn shell_parts() -> (SourceShell, FakeGithub) {
    let fake = FakeGithub::new();
    (
        SourceShell::new(Arc::new(GitSource::new(fake.clone(), Arc::new(fake.clone()), true, MainlineRef::default()))),
        fake,
    )
}

#[test]
fn an_orphaned_ref_is_released_and_its_completion_admitted() {
    // Tripwire on the whole point of ADR-0179: a ref whose holder no journal
    // knows must actually be deletable through this path. If the release stops
    // reaching the source — a mis-decoded payload, a swallowed outcome — the
    // orphan survives and every later seal against that mainline keeps answering
    // `ActiveBloomExists`, which is the wedge with no in-band exit.
    let source = shell();
    let orphan = bloom(7);
    let admission = ClaimRefKind::MainlineAdmission;
    assert_eq!(source.claim_seal(&orphan, &[]).expect("the acquire succeeds"), ClaimOutcome::Acquired);
    assert_eq!(holder_of(&source, &admission), Some(ClaimHolder::Held(orphan)), "the orphan holds the admission ref");

    let mut store = SqliteStore::open(":memory:").expect("an in-memory store opens");
    let target = OrphanClaimRelease { ref_kind: admission.clone(), expected_holder: orphan };
    enqueue_release(&mut store, &target);

    let (admits, ack_through) = drain_and_release(&mut store, &source).expect("the drain succeeds");

    assert_eq!(admits.len(), 1, "one authorized release admits one completion");
    assert_eq!(admitted_completion(&admits[0]), (target.request(), OrphanClaimReleaseCompletion::Released));
    assert_eq!(ack_through, None, "a completion holds the prefix until the journal confirms it");
    assert_eq!(holder_of(&source, &admission), None, "the orphaned admission ref is gone");

    journal_event(&mut store, &admitted_event(&admits[0]));
    let (again, ack_through) = drain_and_release(&mut store, &source).expect("the redrive succeeds");
    assert!(again.is_empty(), "a journaled completion does not fabricate another receipt");
    assert!(ack_through.is_some(), "journal confirmation acknowledges the release");
}

#[test]
fn a_ref_a_live_holder_owns_is_spared_and_completes_as_changed() {
    // Tripwire on the safety half: the expected-holder compare-and-swap is the
    // only thing standing between this operator surface and destroying another
    // instance's live bloom. A release that stopped comparing would delete
    // whatever ref it was pointed at, and the operator would learn nothing —
    // the completion is what tells them the ref moved.
    let source = shell();
    let (authorized, live) = (bloom(7), bloom(9));
    let held = workpiece("wp-live");
    let ref_kind = ClaimRefKind::Workpiece(held.clone());
    assert_eq!(source.claim_seal(&live, &[held]).expect("the acquire succeeds"), ClaimOutcome::Acquired);

    let mut store = SqliteStore::open(":memory:").expect("an in-memory store opens");
    let target = OrphanClaimRelease { ref_kind: ref_kind.clone(), expected_holder: authorized };
    enqueue_release(&mut store, &target);

    let (admits, ack_through) = drain_and_release(&mut store, &source).expect("the drain succeeds");

    assert_eq!(ack_through, None, "a completion holds the prefix until the journal confirms it");
    assert_eq!(
        admitted_completion(&admits[0]),
        (target.request(), OrphanClaimReleaseCompletion::Changed { observed_holder: live }),
        "the release reports the holder it found instead of clobbering it",
    );
    assert_eq!(holder_of(&source, &ref_kind), Some(ClaimHolder::Held(live)), "the live claim is untouched");
}

#[test]
fn a_release_whose_ref_is_already_gone_completes_idempotently() {
    // Tripwire on the crash window ADR-0179 calls out: a release whose source
    // deletion landed but whose completion was never admitted re-drains, and the
    // ref is genuinely absent the second time. Treating that as an error would
    // leave the same authorized request permanently uncompletable — the exact
    // shape of unrecoverable state this work exists to retire.
    let source = shell();
    let mut store = SqliteStore::open(":memory:").expect("an in-memory store opens");
    let target =
        OrphanClaimRelease { ref_kind: ClaimRefKind::Workpiece(workpiece("wp-gone")), expected_holder: bloom(7) };
    enqueue_release(&mut store, &target);

    let (admits, ack_through) = drain_and_release(&mut store, &source).expect("the drain succeeds");

    assert_eq!(admitted_completion(&admits[0]), (target.request(), OrphanClaimReleaseCompletion::AlreadyAbsent));
    assert_eq!(ack_through, None, "a completion holds the prefix until the journal confirms it");

    journal_event(&mut store, &admitted_event(&admits[0]));
    let (again, ack_through) = drain_and_release(&mut store, &source).expect("the redrive succeeds");
    assert!(again.is_empty(), "a journaled completion does not fabricate another receipt");
    assert!(ack_through.is_some(), "an absent ref is a terminal success, not a redrive");
}

#[test]
fn a_changed_receipt_survives_the_expected_holder_reacquiring_the_ref() {
    // Tripwire: a persisted Changed must not be recomputed. After the sidecar
    // lands, the expected holder can reacquire the ref; re-effecting the CAS
    // would delete it and turn Changed into Released — the operator authorized
    // one named holder, not whoever holds the ref later.
    let source = shell();
    let (authorized, live) = (bloom(7), bloom(9));
    let held = workpiece("wp-live");
    let ref_kind = ClaimRefKind::Workpiece(held.clone());
    assert_eq!(source.claim_seal(&live, from_ref(&held)).expect("the acquire succeeds"), ClaimOutcome::Acquired);

    let (_dir, path, mut store) = file_store();
    let target = OrphanClaimRelease { ref_kind: ref_kind.clone(), expected_holder: authorized };
    enqueue_release(&mut store, &target);

    let (admits, ack_through) = drain_and_release(&mut store, &source).expect("the drain succeeds");
    assert_eq!(ack_through, None, "a completion holds the prefix until the journal confirms it");
    assert_eq!(
        admitted_completion(&admits[0]),
        (target.request(), OrphanClaimReleaseCompletion::Changed { observed_holder: live }),
    );
    let first = admits[0].clone();
    drop(store);

    assert_eq!(
        source.transfer_seal(&live, &authorized, from_ref(&held), &[], &[]).expect("the expected holder reacquires"),
        ClaimOutcome::Acquired
    );
    assert_eq!(holder_of(&source, &ref_kind), Some(ClaimHolder::Held(authorized)));

    let mut store = reopen_store(&path);
    let (again, ack_through) = drain_and_release(&mut store, &source).expect("the redrive succeeds");
    assert_eq!(ack_through, None, "a pending receipt does not ack");
    assert_eq!(again, vec![first], "the exact Changed bytes are retained");
    assert_eq!(
        admitted_completion(&again[0]),
        (target.request(), OrphanClaimReleaseCompletion::Changed { observed_holder: live }),
        "the recorded Changed is not turned into Released",
    );
    assert_eq!(
        holder_of(&source, &ref_kind),
        Some(ClaimHolder::Held(authorized)),
        "the reacquired hold is not deleted"
    );
}

#[test]
fn a_released_receipt_stays_released_after_another_holder_appears() {
    // Tripwire: a persisted `Released` must survive a later hold of the same
    // workpiece. Re-effecting the expected-holder CAS would report `Changed`,
    // not delete the new holder; the recorded variant must still stand.
    let (source, fake) = shell_parts();
    let (orphan, other) = (bloom(7), bloom(9));
    let held = workpiece("wp-released");
    let ref_kind = ClaimRefKind::Workpiece(held.clone());
    assert_eq!(source.claim_seal(&orphan, from_ref(&held)).expect("the acquire succeeds"), ClaimOutcome::Acquired);

    let (_dir, path, mut store) = file_store();
    let target = OrphanClaimRelease { ref_kind: ref_kind.clone(), expected_holder: orphan };
    enqueue_release(&mut store, &target);

    let (admits, ack_through) = drain_and_release(&mut store, &source).expect("the drain succeeds");
    assert_eq!(ack_through, None);
    assert_eq!(admitted_completion(&admits[0]), (target.request(), OrphanClaimReleaseCompletion::Released));
    assert_eq!(holder_of(&source, &ref_kind), None, "the orphaned ref is gone");
    let first = admits[0].clone();
    drop(store);

    fake.seed_claim_hold(&format!("bloomery/claims/{}", held.0), &other);
    assert_eq!(holder_of(&source, &ref_kind), Some(ClaimHolder::Held(other)));

    let mut store = reopen_store(&path);
    let (again, ack_through) = drain_and_release(&mut store, &source).expect("the redrive succeeds");
    assert_eq!(ack_through, None, "a pending receipt does not ack");
    assert_eq!(again, vec![first], "the exact Released bytes are retained");
    assert_eq!(
        admitted_completion(&again[0]),
        (target.request(), OrphanClaimReleaseCompletion::Released),
        "the recorded Released is not turned into Changed",
    );
    assert_eq!(holder_of(&source, &ref_kind), Some(ClaimHolder::Held(other)), "the new hold is not deleted");
}

#[test]
fn a_pending_prior_release_blocks_a_later_entry() {
    // Tripwire: a persisted-but-unjournaled prefix row must not let a later
    // entry run its source mutation. The later ref would otherwise be released
    // while the earlier receipt is still awaiting control.
    let (source, fake) = shell_parts();
    let (first_holder, second_holder) = (bloom(7), bloom(9));
    let (first_wp, second_wp) = (workpiece("wp-a"), workpiece("wp-b"));
    fake.seed_claim_hold(&format!("bloomery/claims/{}", first_wp.0), &first_holder);
    fake.seed_claim_hold(&format!("bloomery/claims/{}", second_wp.0), &second_holder);
    let first_kind = ClaimRefKind::Workpiece(first_wp);
    let second_kind = ClaimRefKind::Workpiece(second_wp);

    let (_dir, path, mut store) = file_store();
    let first_target = OrphanClaimRelease { ref_kind: first_kind, expected_holder: first_holder };
    let second_target = OrphanClaimRelease { ref_kind: second_kind.clone(), expected_holder: second_holder };
    enqueue_release(&mut store, &first_target);
    enqueue_release(&mut store, &second_target);

    let (admits, ack_through) = drain_and_release(&mut store, &source).expect("the drain succeeds");
    assert_eq!(ack_through, None, "the pending prefix is not acked");
    assert_eq!(admitted_completion(&admits[0]), (first_target.request(), OrphanClaimReleaseCompletion::Released));
    assert_eq!(holder_of(&source, &second_kind), Some(ClaimHolder::Held(second_holder)), "the later ref is spared");
    drop(store);

    let mut store = reopen_store(&path);
    let (again, ack_through) = drain_and_release(&mut store, &source).expect("the redrive succeeds");
    assert_eq!(ack_through, None);
    assert_eq!(again.len(), 1, "the pending prefix still holds later rows");
    assert_eq!(holder_of(&source, &second_kind), Some(ClaimHolder::Held(second_holder)));
}

#[test]
fn a_journaled_release_acks_without_calling_the_source() {
    // Tripwire: once control has journaled the completion, restart recovers the
    // ack from the sidecar and must not re-run the compare-and-swap. Re-seeding
    // the original expected holder on the workpiece would be deleted by a
    // repeated CAS; the journaled path must spare it.
    let (source, fake) = shell_parts();
    let orphan = bloom(7);
    let held = workpiece("wp-acked");
    let ref_kind = ClaimRefKind::Workpiece(held.clone());
    assert_eq!(source.claim_seal(&orphan, from_ref(&held)).expect("the acquire succeeds"), ClaimOutcome::Acquired);

    let (_dir, path, mut store) = file_store();
    let target = OrphanClaimRelease { ref_kind: ref_kind.clone(), expected_holder: orphan };
    let sequence = enqueue_release(&mut store, &target);

    let (admits, ack_through) = drain_and_release(&mut store, &source).expect("the drain succeeds");
    assert_eq!(ack_through, None);
    assert_eq!(admitted_completion(&admits[0]), (target.request(), OrphanClaimReleaseCompletion::Released));
    journal_event(&mut store, &admitted_event(&admits[0]));
    drop(store);

    fake.seed_claim_hold(&format!("bloomery/claims/{}", held.0), &orphan);

    let mut store = reopen_store(&path);
    let (again, ack_through) = drain_and_release(&mut store, &source).expect("the redrive succeeds");
    assert!(again.is_empty(), "a journaled receipt does not resend");
    assert_eq!(ack_through, Some(sequence), "journal confirmation acknowledges without a source call");
    assert_eq!(
        holder_of(&source, &ref_kind),
        Some(ClaimHolder::Held(orphan)),
        "the reacquired expected holder is not deleted"
    );
}
