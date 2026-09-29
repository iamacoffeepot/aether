//! Typed artifact resolver validation and cancellation.

use std::future::Future;
use std::mem::forget;
use std::pin::{Pin, pin};
use std::task::{Context, Poll, Waker};

use aether_bloomery_kinds::{ClosureArtifact, Digest, EncodedArtifact, ReadArtifactResult, Ref};
use aether_bloomery_view::{ArtifactResolver, ResolveError};
use aether_data::wire::{decode_from_slice, encode_to_vec};
use aether_data::{Cites, Kind, Storage};

#[derive(Clone, Debug, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.bloomery.view.resolver.note")]
struct Note {
    value: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.bloomery.view.resolver.other")]
struct Other {
    value: u64,
}

fn artifact<K: Storage + Clone + Cites>(value: &K) -> (Ref<K>, ClosureArtifact) {
    let encoded = EncodedArtifact::new(value).expect("storage encode");
    let artifact = ClosureArtifact::new(encoded.kind(), encoded.bytes().to_vec());
    (Ref::from_digest(artifact.claimed().unverified()), artifact)
}

fn poll<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    future.poll(&mut cx)
}

fn with_claim(artifact: &ClosureArtifact, claimed: Digest) -> ClosureArtifact {
    let mut bytes = encode_to_vec(artifact).expect("wire encode");
    bytes[..32].copy_from_slice(claimed.as_bytes());
    decode_from_slice(&bytes).expect("wire decode")
}

#[test]
fn read_waits_for_transport_and_decodes_verified_storage() {
    let (reference, stored) = artifact(&Note { value: 42 });
    let (mut resolver, driver) = ArtifactResolver::operation();
    let mut read = pin!(resolver.read(reference));

    assert!(poll(read.as_mut()).is_pending());
    let pending = driver.take_pending().expect("requested artifact");
    assert_eq!(pending.digest, reference.digest());
    assert_eq!(pending.expected, Note::ID);

    driver.fulfill(ReadArtifactResult::Found { artifact: stored });
    assert_eq!(poll(read.as_mut()), Poll::Ready(Ok(Note { value: 42 })));
    assert_eq!(driver.terminal(), None);
}

#[test]
fn mismatched_reply_is_terminal_even_when_read_is_ignored() {
    let (reference, _) = artifact(&Note { value: 7 });
    let actual = Digest::from_bytes([9; 32]);
    let (mut resolver, driver) = ArtifactResolver::operation();
    let mut read = pin!(resolver.read(reference));

    assert!(poll(read.as_mut()).is_pending());
    assert!(driver.take_pending().is_some());
    driver.fulfill(ReadArtifactResult::Missing { digest: actual });

    assert_eq!(driver.terminal(), Some(ResolveError::DigestMismatch { expected: reference.digest(), actual }));
    assert_eq!(resolver.terminal(), driver.terminal());
}

#[test]
fn cancellation_reaches_retained_clones_and_discards_ready_payload() {
    let (reference, stored) = artifact(&Note { value: 99 });
    let (mut resolver, driver) = ArtifactResolver::operation();
    let retained = resolver.clone();
    let mut read = pin!(resolver.read(reference));

    assert!(poll(read.as_mut()).is_pending());
    assert!(driver.take_pending().is_some());
    driver.fulfill(ReadArtifactResult::Found { artifact: stored });
    driver.cancel();

    let cancelled = ResolveError::Cancelled { digest: reference.digest() };
    assert_eq!(poll(read.as_mut()), Poll::Ready(Err(cancelled.clone())));
    assert_eq!(retained.terminal(), Some(cancelled));
}

#[test]
fn abandoning_a_started_read_is_terminal_but_an_unpolled_read_is_harmless() {
    let (reference, _) = artifact(&Note { value: 11 });
    let (mut resolver, driver) = ArtifactResolver::operation();
    drop(resolver.read(reference));
    assert_eq!(driver.terminal(), None);

    let mut read = Box::pin(resolver.read(reference));
    assert!(poll(read.as_mut()).is_pending());
    drop(read);

    assert_eq!(driver.terminal(), Some(ResolveError::Cancelled { digest: reference.digest() }));
}

#[test]
fn finishing_rejects_a_retained_outstanding_read() {
    let (reference, _) = artifact(&Note { value: 12 });
    let (mut resolver, driver) = ArtifactResolver::operation();
    let mut read = Box::pin(resolver.read(reference));
    assert!(poll(read.as_mut()).is_pending());
    forget(read);

    assert_eq!(driver.finish(), Some(ResolveError::Cancelled { digest: reference.digest() }));
    assert_eq!(resolver.terminal(), driver.terminal());
}

#[test]
fn concurrent_reads_poison_the_operation() {
    let (first, _) = artifact(&Note { value: 1 });
    let (second, _) = artifact(&Note { value: 2 });
    let (mut resolver, driver) = ArtifactResolver::operation();
    let mut first_read = pin!(resolver.read(first));

    assert!(poll(first_read.as_mut()).is_pending());
    let mut second_read = pin!(resolver.read(second));
    let expected = ResolveError::ConcurrentRead { active: first.digest(), requested: second.digest() };
    assert_eq!(poll(second_read.as_mut()), Poll::Ready(Err(expected.clone())));
    assert_eq!(driver.terminal(), Some(expected));
}

#[test]
fn wrong_kind_is_terminal_before_payload_decode() {
    let (reference, _) = artifact(&Note { value: 3 });
    let (_, other) = artifact(&Other { value: 3 });
    let supplied_kind = other.kind();
    let (mut resolver, driver) = ArtifactResolver::operation();
    let mut read = Box::pin(resolver.read(reference));
    assert!(poll(read.as_mut()).is_pending());
    assert!(driver.take_pending().is_some());

    driver.fulfill(ReadArtifactResult::Found { artifact: with_claim(&other, reference.digest()) });
    assert!(poll(read.as_mut()).is_ready());

    assert_eq!(
        driver.terminal(),
        Some(ResolveError::KindMismatch { digest: reference.digest(), expected: Note::ID, actual: supplied_kind })
    );
}

#[test]
fn altered_payload_fails_full_content_hash_validation() {
    let (reference, stored) = artifact(&Note { value: 5 });
    let mut bytes = encode_to_vec(&stored).expect("wire encode");
    *bytes.last_mut().expect("payload byte") ^= 0x01;
    let altered = decode_from_slice(&bytes).expect("wire decode");
    let (mut resolver, driver) = ArtifactResolver::operation();
    let mut read = Box::pin(resolver.read(reference));
    assert!(poll(read.as_mut()).is_pending());
    assert!(driver.take_pending().is_some());

    driver.fulfill(ReadArtifactResult::Found { artifact: altered });
    assert!(poll(read.as_mut()).is_ready());

    assert_eq!(driver.terminal(), Some(ResolveError::ContentMismatch { digest: reference.digest() }));
}

#[test]
fn verified_but_invalid_storage_payload_is_terminal() {
    let invalid = ClosureArtifact::new(Note::ID, vec![0xff, 0xff]);
    let reference: Ref<Note> = Ref::from_digest(invalid.claimed().unverified());
    let (mut resolver, driver) = ArtifactResolver::operation();
    let mut read = pin!(resolver.read(reference));

    assert!(poll(read.as_mut()).is_pending());
    assert!(driver.take_pending().is_some());
    driver.fulfill(ReadArtifactResult::Found { artifact: invalid });

    assert_eq!(
        poll(read.as_mut()),
        Poll::Ready(Err(ResolveError::Decode { digest: reference.digest(), kind: Note::ID }))
    );
    assert_eq!(driver.terminal(), Some(ResolveError::Decode { digest: reference.digest(), kind: Note::ID }));
}

#[test]
fn transport_failure_is_terminal() {
    let (reference, _) = artifact(&Note { value: 6 });
    let (mut resolver, driver) = ArtifactResolver::operation();
    let mut read = Box::pin(resolver.read(reference));
    assert!(poll(read.as_mut()).is_pending());
    assert!(driver.take_pending().is_some());

    driver.fulfill(ReadArtifactResult::Err { digest: reference.digest(), message: "store unavailable".into() });
    assert!(poll(read.as_mut()).is_ready());

    assert_eq!(
        driver.terminal(),
        Some(ResolveError::Transport { digest: reference.digest(), message: "store unavailable".into() })
    );
}
