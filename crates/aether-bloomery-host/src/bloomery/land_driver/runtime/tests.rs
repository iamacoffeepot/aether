//! The drain → land core of the land driver, over a real `SqliteStore` and a
//! fake-GitHub-backed `SourceShell` — the network side the running capability
//! drives, without the mail harness. `init` / the timer / the ctx send are the
//! thin glue the chassis-boot test and compilation cover; this pins the loop that
//! turns a land decision into the source-port CAS and the admitted `Fact::Land`.

use std::sync::Arc;

use aether_bloomery::{BloomId, Digest, Event, Fact, LandPayload};
use aether_bloomery_github::GitSource;
use aether_bloomery_github::testing::FakeGithub;
use aether_data::wire::{from_bytes, to_vec};

use super::{LAND_TOPIC, drain_and_land};
use crate::bloomery::SourceShell;
use crate::store::{SqliteStore, StoreBackend};

fn digest(seed: u8) -> Digest {
    Digest::from_bytes([seed; 32])
}

// A fake-GitHub-backed source shell with the CAS-land gate set explicitly, so a
// test drives the same shell the running driver holds.
fn shell(fake: FakeGithub, cas_land_enabled: bool) -> SourceShell {
    SourceShell::new(Arc::new(GitSource::new(fake, cas_land_enabled)))
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
    store.enqueue_outbox(LAND_TOPIC, &to_vec(&payload).unwrap()).unwrap()
}

#[test]
fn landed_admits_a_fact_land_and_acks_the_entry() {
    let (fake, base) = seeded();
    let source = shell(fake, true);
    let mut store = SqliteStore::open(":memory:").unwrap();
    let bloom = BloomId(digest(1));
    let new_head = digest(90);
    let sequence = enqueue_land(&mut store, bloom, base, new_head);

    let (admits, ack_through) = drain_and_land(&mut store, &source).unwrap();

    // The CAS on the matching base landed, so one `Fact::Land` is admitted and the
    // entry is acked.
    assert_eq!(admits.len(), 1, "a landed bloom admits one fact");
    assert_eq!(ack_through, Some(sequence), "the landed entry is acked");
    let event = from_bytes::<Event>(&admits[0].event).unwrap();
    match event.fact {
        Fact::Land { bloom: landed, new_head: head } => {
            assert_eq!(landed, bloom, "the admitted land names the resolved bloom");
            assert_eq!(head, new_head, "the admitted land carries the new mainline head");
        }
        other => panic!("expected Fact::Land, got {other:?}"),
    }
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

    // No land: a moved mainline forces supersession, never a land onto the new
    // head. The bloom stays supersedable; the refusal is definitive, so it is
    // acked rather than re-driven forever.
    assert!(admits.is_empty(), "a moved base admits no land");
    assert_eq!(ack_through, Some(sequence), "the definitive base-moved refusal is acked");
}

#[test]
fn a_gated_off_land_is_a_transient_fault_that_re_drains() {
    let (fake, base) = seeded();
    // The kill switch: `cas_land_enabled` off makes `land` refuse with a transport
    // fault, so the entry stays unacked to re-drive when the gate is re-enabled.
    let source = shell(fake, false);
    let mut store = SqliteStore::open(":memory:").unwrap();
    let sequence = enqueue_land(&mut store, BloomId(digest(1)), base, digest(90));

    let (admits, ack_through) = drain_and_land(&mut store, &source).unwrap();

    assert!(admits.is_empty(), "a gated-off land admits nothing");
    assert_eq!(ack_through, None, "the gated entry is not acked; it re-drains");
    let _ = sequence;
}
