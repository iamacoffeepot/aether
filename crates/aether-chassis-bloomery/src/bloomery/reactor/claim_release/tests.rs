//! The drain → release core of the claim-release reactor, over a real
//! `SqliteStore` and a fake-GitHub-backed `SourceShell` — the network side the
//! running capability drives, without the mail harness.
//!
//! The bug this reactor exists to close has two halves, and both are pinned
//! here: an orphaned ref must actually become releasable, and a ref a *live*
//! holder owns must not. A regression in either direction is silent at every
//! other layer — the first leaves the coordinator permanently wedged, the second
//! destroys another instance's work — so the pair is the point.

use std::sync::Arc;

use aether_bloomery::{
    BloomId, ClaimHolder, ClaimOutcome, ClaimRefKind, Digest, Event, Fact, OrphanClaimRelease,
    OrphanClaimReleaseCompletion, OrphanClaimReleasePayload, Topic, WorkpieceId,
};
use aether_bloomery_github::GitSource;
use aether_bloomery_github::testing::FakeGithub;
use aether_data::wire::{from_bytes, to_vec};

use super::runtime::drain_and_release;
use crate::bloomery::SourceShell;
use crate::bloomery::outbox::TopicOutbox;
use crate::store::SqliteStore;

fn bloom(seed: u8) -> BloomId {
    BloomId(Digest::from_bytes([seed; 32]))
}

fn workpiece(name: &str) -> WorkpieceId {
    WorkpieceId(name.to_owned())
}

fn shell() -> SourceShell {
    let fake = FakeGithub::new();
    SourceShell::new(Arc::new(GitSource::new(fake.clone(), Arc::new(fake), true)))
}

// Enqueue one authorized release on the release topic — the bytes the reducer's
// `DispatchOrphanClaimRelease` projection enqueues once the signed request is
// admitted.
fn enqueue_release(store: &mut SqliteStore, target: &OrphanClaimRelease) -> u64 {
    let payload = OrphanClaimReleasePayload { request: target.request(), target: target.clone() };
    store.enqueue_topic(Topic::OrphanClaimRelease, &to_vec(&payload).unwrap()).unwrap()
}

// The completion an admit carries, so a test asserts the journaled terminal
// rather than the opaque bytes.
fn admitted_completion(admit: &aether_bloomery::Admit) -> (Digest, OrphanClaimReleaseCompletion) {
    let event: Event = from_bytes(&admit.event).unwrap();
    match event.fact {
        Fact::CompleteOrphanClaimRelease { request, completion } => (request, completion),
        other => panic!("expected a release completion, got {other:?}"),
    }
}

fn holder_of(source: &SourceShell, ref_kind: &ClaimRefKind) -> Option<ClaimHolder> {
    source.enumerate_claims().unwrap().into_iter().find(|state| state.ref_kind == *ref_kind).map(|state| state.holder)
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
    assert_eq!(source.claim_seal(&orphan, &[]).unwrap(), ClaimOutcome::Acquired);
    assert_eq!(holder_of(&source, &admission), Some(ClaimHolder::Held(orphan)), "the orphan holds the admission ref");

    let mut store = SqliteStore::open(":memory:").unwrap();
    let target = OrphanClaimRelease { ref_kind: admission.clone(), expected_holder: orphan };
    enqueue_release(&mut store, &target);

    let (admits, ack_through) = drain_and_release(&mut store, &source).unwrap();

    assert_eq!(admits.len(), 1, "one authorized release admits one completion");
    assert_eq!(admitted_completion(&admits[0]), (target.request(), OrphanClaimReleaseCompletion::Released));
    assert!(ack_through.is_some(), "a terminal release acks its entry");
    assert_eq!(holder_of(&source, &admission), None, "the orphaned admission ref is gone");
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
    assert_eq!(source.claim_seal(&live, &[held]).unwrap(), ClaimOutcome::Acquired);

    let mut store = SqliteStore::open(":memory:").unwrap();
    let target = OrphanClaimRelease { ref_kind: ref_kind.clone(), expected_holder: authorized };
    enqueue_release(&mut store, &target);

    let (admits, _) = drain_and_release(&mut store, &source).unwrap();

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
    let mut store = SqliteStore::open(":memory:").unwrap();
    let target =
        OrphanClaimRelease { ref_kind: ClaimRefKind::Workpiece(workpiece("wp-gone")), expected_holder: bloom(7) };
    enqueue_release(&mut store, &target);

    let (admits, ack_through) = drain_and_release(&mut store, &source).unwrap();

    assert_eq!(admitted_completion(&admits[0]), (target.request(), OrphanClaimReleaseCompletion::AlreadyAbsent));
    assert!(ack_through.is_some(), "an absent ref is a terminal success, not a redrive");
}
