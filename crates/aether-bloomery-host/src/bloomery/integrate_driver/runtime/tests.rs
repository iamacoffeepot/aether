//! The drain → fold → resolve core of the integrate driver, over a real
//! `SqliteStore` and a fake-GitHub-backed `SourceShell` — the network side the
//! running capability drives, without the mail harness. `init` / the timer / the
//! ctx send are the thin glue the chassis-boot test and compilation cover; this
//! pins the loop that turns a completed claim set into the integration fold and
//! the admitted `Fact::Resolve`.

use std::sync::Arc;

use aether_bloomery::{BloomId, Digest, Event, Fact, IntegratePayload, Topic};
use aether_bloomery_github::GitSource;
use aether_bloomery_github::testing::FakeGithub;
use aether_data::wire::{from_bytes, to_vec};

use super::drain_and_integrate;
use crate::bloomery::SourceShell;
use crate::bloomery::outbox::TopicOutbox;
use crate::store::SqliteStore;

fn digest(seed: u8) -> Digest {
    Digest::from_bytes([seed; 32])
}

fn shell(fake: FakeGithub) -> SourceShell {
    SourceShell::new(Arc::new(GitSource::new(fake.clone(), Arc::new(fake), false)))
}

// A fake seeded with a base commit (head + tree correspondences recorded) and a
// mainline ref at it, plus one candidate tree object the fold resolves.
fn seeded(candidate: &Digest) -> (FakeGithub, Digest) {
    let fake = FakeGithub::new();
    let base = fake.seed_base_commit(&digest(10));
    fake.seed_ref_at("heads/main", &base);
    fake.seed_git_object(candidate);
    (fake, base)
}

fn enqueue_integration(store: &mut SqliteStore, bloom: BloomId, base: Digest, candidates: Vec<Digest>) -> u64 {
    let payload = IntegratePayload { bloom: bloom.0, base, candidates };
    store.enqueue_topic(Topic::INTEGRATE, &to_vec(&payload).unwrap()).unwrap()
}

fn decoded_resolve(admit: &aether_bloomery::Admit) -> (BloomId, Digest, Digest, Vec<Digest>) {
    let event: Event = from_bytes(&admit.event).unwrap();
    match event.fact {
        Fact::Resolve { bloom, tree, head, lineage } => (bloom, tree, head, lineage),
        other => panic!("expected Fact::Resolve, got {other:?}"),
    }
}

// ADR-0152 — a completed claim set folds its candidate onto the integration
// branch (bootstrapping the namespace itself) and admits a `Fact::Resolve`
// carrying the integrated tree, a landable head distinct from it, and the
// candidate lineage. Catches the gap this arc closes: resolutions never
// reaching the git side, a bloom "landing" a head identical to its base.
#[test]
fn a_completed_claim_set_folds_and_admits_a_resolve() {
    let candidate = digest(0xAB);
    let (fake, base) = seeded(&candidate);
    let source = shell(fake);
    let mut store = SqliteStore::open(":memory:").unwrap();
    let bloom = BloomId(digest(1));
    let sequence = enqueue_integration(&mut store, bloom, base, vec![candidate]);

    let (admits, ack_through) = drain_and_integrate(&mut store, &source).unwrap();

    assert_eq!(admits.len(), 1, "a folded bloom admits one resolve");
    assert_eq!(ack_through, Some(sequence), "the folded entry is acked");
    let (resolved_bloom, tree, head, lineage) = decoded_resolve(&admits[0]);
    assert_eq!(resolved_bloom, bloom);
    assert_eq!(tree, candidate, "the integrated tree is the folded candidate");
    assert_ne!(head, tree, "the landable head is a distinct commit digest, never the artifact tree");
    assert_eq!(lineage, vec![candidate], "the lineage is the candidate fold sequence");
}

// ADR-0152 / #3653 — a multi-member fold refuses (acked, no admit): the port's
// tree-replace `integrate` would keep only the last member's work, so failing
// closed beats a resolve whose head silently dropped members' changes.
#[test]
fn a_multi_member_fold_refuses_and_acks_instead_of_dropping_work() {
    let candidate = digest(0xAB);
    let (fake, base) = seeded(&candidate);
    fake.seed_git_object(&digest(0xAC));
    let source = shell(fake);
    let mut store = SqliteStore::open(":memory:").unwrap();
    let sequence = enqueue_integration(&mut store, BloomId(digest(1)), base, vec![candidate, digest(0xAC)]);

    let (admits, ack_through) = drain_and_integrate(&mut store, &source).unwrap();

    assert!(admits.is_empty(), "no resolve is admitted for a fold that would drop work");
    assert_eq!(ack_through, Some(sequence), "the refusal is definitive — acked, never re-driven");
}

// ADR-0152 — a drain interrupted between the final integrate and the resolve
// admit recovers on re-drain: the branch already sits at the candidate, nothing
// re-folds, and the head comes back from the branch position's recorded
// `head ↔ commit` correspondence. Catches the wedge where a crash in that
// window strands a fully-folded bloom un-resolvable.
#[test]
fn a_re_drain_after_the_fold_recovers_the_head_without_re_integrating() {
    let candidate = digest(0xAB);
    let (fake, base) = seeded(&candidate);
    let source = shell(fake);
    let mut store = SqliteStore::open(":memory:").unwrap();
    let bloom = BloomId(digest(1));

    enqueue_integration(&mut store, bloom, base, vec![candidate]);
    let (first, first_ack) = drain_and_integrate(&mut store, &source).unwrap();
    store.ack_topic(Topic::INTEGRATE, first_ack.unwrap()).unwrap();

    // The same decision re-enqueued (modeling a crash before the first ack /
    // a replayed outbox): the branch is already at the candidate.
    let sequence = enqueue_integration(&mut store, bloom, base, vec![candidate]);
    let (second, second_ack) = drain_and_integrate(&mut store, &source).unwrap();

    assert_eq!(second_ack, Some(sequence));
    assert_eq!(second.len(), 1, "the re-drain still admits the resolve");
    let (_, first_tree, first_head, _) = decoded_resolve(&first[0]);
    let (_, second_tree, second_head, _) = decoded_resolve(&second[0]);
    assert_eq!(second_tree, first_tree);
    assert_eq!(second_head, first_head, "the recovered head is the one the fold produced, not a re-mint");
}
