//! The drain → land core of the land reactor, over a real `SqliteStore` and a
//! fake-GitHub-backed `SourceShell` — the network side the running capability
//! drives, without the mail harness. `init` / the timer / the ctx send are the
//! thin glue the chassis-boot test and compilation cover; this pins the loop that
//! turns a land decision into a landing proposal, watches it, and admits the
//! `Fact::Land` it observes.

use std::sync::Arc;

use aether_bloomery::{BloomId, Correspondence, Digest, Event, Fact, LandPayload, Topic};
use aether_bloomery_github::testing::FakeGithub;
use aether_bloomery_github::{GitObjectId, GitSource, PullRequestApi, short_hex};
use aether_data::wire::{from_bytes, to_vec};

use super::drain_and_land;
use crate::bloomery::SourceShell;
use crate::bloomery::outbox::TopicOutbox;
use crate::store::SqliteStore;

fn digest(seed: u8) -> Digest {
    Digest::from_bytes([seed; 32])
}

// A fake-GitHub-backed source shell with the land gate set explicitly, so a
// test drives the same shell the running reactor holds.
fn shell(fake: FakeGithub, cas_land_enabled: bool) -> SourceShell {
    SourceShell::new(Arc::new(GitSource::new(fake.clone(), Arc::new(fake), cas_land_enabled)))
}

// Seed a fake with a base commit and a mainline ref at it, returning the fake and
// the base commit digest — the sealed base a resolved bloom lands on.
fn seeded() -> (FakeGithub, Digest) {
    let fake = FakeGithub::new();
    let base = fake.seed_base_commit(&digest(10));
    fake.seed_ref_at("heads/main", &base);
    (fake, base)
}

// Enqueue one land decision on the land topic (the bytes the reducer's
// `DispatchLand` projection would enqueue), returning its outbox sequence.
fn enqueue_land(store: &mut SqliteStore, bloom: BloomId, expected_base: Digest, new_head: Digest) -> u64 {
    let payload = LandPayload { bloom: bloom.0, expected_base, new_head };
    store.enqueue_topic(Topic::Land, &to_vec(&payload).unwrap()).unwrap()
}

#[test]
fn an_open_proposal_admits_nothing_and_leaves_the_entry_to_re_drain() {
    // Tripwire: the outbox *is* the watch. A proposal that has not been accepted
    // yet must leave its entry unacked, or the bloom's landing is forgotten the
    // moment it is proposed and `Fact::Land` is never admitted at all.
    let (fake, base) = seeded();
    let new_head = digest(90);
    fake.seed_git_object(&new_head);
    let source = shell(fake, true);
    let mut store = SqliteStore::open(":memory:").unwrap();
    let sequence = enqueue_land(&mut store, BloomId(digest(1)), base, new_head);

    let (admits, ack_through) = drain_and_land(&mut store, &source).unwrap();

    assert!(admits.is_empty(), "an open proposal admits no land");
    assert_eq!(ack_through, None, "the entry stays unacked so the watch re-drains");
    let _ = sequence;
}

#[test]
fn an_accepted_proposal_admits_a_fact_land_carrying_the_merge_commit() {
    let (fake, base) = seeded();
    let new_head = digest(90);
    // Seed the proposed head's git-object correspondence so the landing branch
    // resolves its target.
    fake.seed_git_object(&new_head);
    let source = shell(fake.clone(), true);
    let mut store = SqliteStore::open(":memory:").unwrap();
    let bloom = BloomId(digest(1));
    let sequence = enqueue_land(&mut store, bloom, base, new_head);

    // First pass proposes and finds it open.
    assert_eq!(drain_and_land(&mut store, &source).unwrap().1, None, "still open");

    // The operator squash-merges it. Mainline becomes a commit that is neither
    // the proposed head nor anything Bloomery created.
    let squashed = "5c".repeat(20);
    let number = fake
        .find_pull_request_for_head(&format!("bloom/{}/landing", short_hex(&bloom.0)))
        .unwrap()
        .expect("the first pass proposed one")
        .number;
    fake.merge_pull_request(number, &squashed);

    // The re-drain observes the acceptance and admits the landing.
    let (admits, ack_through) = drain_and_land(&mut store, &source).unwrap();
    assert_eq!(admits.len(), 1, "an accepted proposal admits one fact");
    assert_eq!(ack_through, Some(sequence), "the completed entry is acked");
    let event = from_bytes::<Event>(&admits[0].event).unwrap();
    match event.fact {
        Fact::Land { bloom: landed, new_head: head } => {
            assert_eq!(landed, bloom, "the admitted land names the resolved bloom");
            // Tripwire: the admitted head is what mainline *became*. Carrying the
            // proposed head instead would record a mainline commit that a squash
            // accept never produced, and the next bloom would seal on it.
            assert_ne!(head, new_head, "the admitted head is the merge commit, not the proposed head");
            assert_eq!(
                fake.resolve_backend_object(&head)
                    .unwrap()
                    .map(GitObjectId::try_from)
                    .transpose()
                    .unwrap()
                    .map(|object| object.to_hex()),
                Some(squashed),
                "the admitted head resolves to the commit mainline actually became",
            );
        }
        other => panic!("expected Fact::Land, got {other:?}"),
    }
}

#[test]
fn a_declined_proposal_admits_nothing_and_acks_the_definitive_refusal() {
    let (fake, base) = seeded();
    let new_head = digest(90);
    fake.seed_git_object(&new_head);
    let source = shell(fake.clone(), true);
    let mut store = SqliteStore::open(":memory:").unwrap();
    let bloom = BloomId(digest(1));
    let sequence = enqueue_land(&mut store, bloom, base, new_head);

    drain_and_land(&mut store, &source).unwrap();
    let number = fake
        .find_pull_request_for_head(&format!("bloom/{}/landing", short_hex(&bloom.0)))
        .unwrap()
        .expect("proposed")
        .number;
    fake.close_pull_request(number);

    let (admits, ack_through) = drain_and_land(&mut store, &source).unwrap();
    assert!(admits.is_empty(), "a declined proposal admits no land");
    assert_eq!(ack_through, Some(sequence), "the definitive refusal is acked rather than re-driven forever");
}

#[test]
fn base_moved_declines_to_land_but_acks_the_definitive_refusal() {
    let (fake, _base) = seeded();
    let source = shell(fake, true);
    let mut store = SqliteStore::open(":memory:").unwrap();
    let bloom = BloomId(digest(1));
    // A sealed base that no longer matches the mainline — a moved head.
    let stale_base = digest(99);
    let sequence = enqueue_land(&mut store, bloom, stale_base, digest(90));

    let (admits, ack_through) = drain_and_land(&mut store, &source).unwrap();

    // No proposal: a moved mainline forces supersession, never a land onto the
    // new head. The bloom stays supersedable; the refusal is definitive, so it is
    // acked rather than re-driven forever.
    assert!(admits.is_empty(), "a moved base admits no land");
    assert_eq!(ack_through, Some(sequence), "the definitive base-moved refusal is acked");
}

#[test]
fn a_gated_off_land_is_a_transient_fault_that_re_drains() {
    let (fake, base) = seeded();
    // The kill switch: the land gate off makes `land` refuse with a transport
    // fault, so the entry stays unacked to re-drive when the gate is re-enabled.
    let source = shell(fake, false);
    let mut store = SqliteStore::open(":memory:").unwrap();
    let sequence = enqueue_land(&mut store, BloomId(digest(1)), base, digest(90));

    let (admits, ack_through) = drain_and_land(&mut store, &source).unwrap();

    assert!(admits.is_empty(), "a gated-off land admits nothing");
    assert_eq!(ack_through, None, "the gated entry is not acked; it re-drains");
    let _ = sequence;
}
