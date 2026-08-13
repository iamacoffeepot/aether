//! The drain → fold → resolve core of the integrate reactor, over a real
//! `SqliteStore` and a fake-GitHub-backed `SourceShell` — the network side the
//! running capability drives, without the mail harness. `init` / the timer / the
//! ctx send are the thin glue the chassis-boot test and compilation cover; this
//! pins the loop that turns a completed claim set into the integration fold and
//! the admitted `Fact::Resolve`.

use std::sync::Arc;

use aether_bloomery::{
    BloomId, Digest, Event, Fact, IdempotencyKey, IntegratePayload, MemberCandidate, Topic, WorkpieceId,
};
use aether_bloomery_github::testing::FakeGithub;
use aether_bloomery_github::{GitDataApi, GitSource, MainlineRef, short_hex};
use aether_data::wire::{from_bytes, to_vec};

use super::drain_and_integrate;
use crate::bloomery::SourceShell;
use crate::bloomery::outbox::TopicOutbox;
use crate::store::SqliteStore;
use aether_bloomery_github::candidate_ref_name;

fn digest(seed: u8) -> Digest {
    Digest::from_bytes([seed; 32])
}

fn shell(fake: FakeGithub) -> SourceShell {
    SourceShell::new(Arc::new(GitSource::new(fake.clone(), Arc::new(fake), false, MainlineRef::default())))
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
    enqueue_integration_adopting(store, bloom, base, candidates, None)
}

// The same enqueue for a bloom that inherited its claim set: the fold adopts the
// predecessor's candidate refs into its own namespace before merging them.
fn enqueue_integration_adopting(
    store: &mut SqliteStore,
    bloom: BloomId,
    base: Digest,
    candidates: Vec<Digest>,
    adopt_from: Option<Digest>,
) -> u64 {
    let members = candidates
        .into_iter()
        .enumerate()
        .map(|(index, candidate)| MemberCandidate { workpiece: WorkpieceId(format!("wp-{index}")), candidate })
        .collect();
    let payload = IntegratePayload { bloom: bloom.0, base, members, adopt_from };
    store.enqueue_topic(Topic::Integrate, &to_vec(&payload).unwrap()).unwrap()
}

fn decoded_resolve(admit: &aether_bloomery::Admit) -> (BloomId, Digest, Digest, Vec<Digest>) {
    let event: Event = from_bytes(&admit.event).unwrap();
    match event.fact {
        Fact::Resolve { bloom, tree, head, lineage } => (bloom, tree, head, lineage),
        other => panic!("expected Fact::Resolve, got {other:?}"),
    }
}

fn admitted_key(admit: &aether_bloomery::Admit) -> IdempotencyKey {
    from_bytes::<Event>(&admit.event).unwrap().idempotency_key
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

// Seed the candidate branch a member's capture would have pushed, at its own
// commit — what a combining fold merges — and return the commit sha the branch
// now points at. Keyed off `candidate_ref_name` so the test addresses it exactly
// as the fold does; a hand-spelled name here would pass while the fold read an
// empty branch.
fn seed_candidate_branch(fake: &FakeGithub, bloom: &BloomId, workpiece: &str, tree: &str) -> String {
    let commit = fake.create_commit(workpiece, tree, &[]).unwrap();
    fake.seed_ref(candidate_ref_name(bloom, workpiece).trim_start_matches("refs/"), &commit.sha);

    commit.sha
}

// ADR-0152 / #3653 — a multi-member fold merges every member's candidate ref
// instead of refusing. The refusal this replaces existed because tree-replace
// would keep only the last member's work; the decisive assertion is that the
// folded tree is *not* the last candidate, which is exactly what a tree-replace
// would have produced.
#[test]
fn a_multi_member_fold_merges_every_members_candidate() {
    let (first, second) = (digest(0xAB), digest(0xAC));
    let (fake, base) = seeded(&first);
    fake.seed_git_object(&second);
    let bloom = BloomId(digest(1));
    seed_candidate_branch(&fake, &bloom, "wp-0", "tree-a");
    seed_candidate_branch(&fake, &bloom, "wp-1", "tree-b");
    let source = shell(fake);
    let mut store = SqliteStore::open(":memory:").unwrap();
    let sequence = enqueue_integration(&mut store, bloom, base, vec![first, second]);

    let (admits, ack_through) = drain_and_integrate(&mut store, &source).unwrap();

    assert_eq!(admits.len(), 1, "a multi-member fold resolves rather than failing closed");
    assert_eq!(ack_through, Some(sequence), "the folded entry is acked");
    let (_, tree, head, lineage) = decoded_resolve(&admits[0]);
    assert_ne!(tree, second, "a tree-replace would have produced exactly the last member's candidate");
    assert_ne!(tree, first, "nor the first member's — the fold combined them");
    assert_ne!(head, tree, "the landable head stays a distinct commit digest");
    assert_eq!(lineage, vec![first, second], "the lineage records every member's candidate, in member order");
}

// A successor that inherited its claims has no candidate refs of its own — a
// ref is addressed under the bloom that produced it, and a successor is a
// different bloom (its id content-addresses a spec that includes the base, so
// re-basing mints a new one). The fold adopts the predecessor's refs into its
// own namespace first; without that it would merge branches that are not there.
#[test]
fn an_inheriting_successor_adopts_the_predecessors_candidate_refs_before_folding() {
    let candidate = digest(0xAB);
    let (fake, base) = seeded(&candidate);
    let predecessor = BloomId(digest(1));
    let successor = BloomId(digest(2));
    seed_candidate_branch(&fake, &predecessor, "wp-0", "tree-a");
    let source = shell(fake.clone());
    let mut store = SqliteStore::open(":memory:").unwrap();
    let sequence = enqueue_integration_adopting(&mut store, successor, base, vec![candidate], Some(predecessor.0));

    let (admits, ack_through) = drain_and_integrate(&mut store, &source).unwrap();

    assert_eq!(admits.len(), 1, "the inherited work folds under the successor");
    assert_eq!(ack_through, Some(sequence));
    assert!(
        fake.ref_exists(candidate_ref_name(&successor, "wp-0").trim_start_matches("refs/")),
        "the candidate ref now lives in the successor's namespace, so its fold reads only its own refs",
    );
}

// A member whose predecessor ref is gone has no work to fold, and folding the
// rest would resolve an artifact that never carried that member's changes.
#[test]
fn an_inherited_member_with_no_predecessor_ref_refuses_rather_than_folding_a_partial_set() {
    let candidate = digest(0xAB);
    let (fake, base) = seeded(&candidate);
    let (predecessor, successor) = (BloomId(digest(1)), BloomId(digest(2)));
    // Deliberately seed no candidate branch for the member.
    let source = shell(fake);
    let mut store = SqliteStore::open(":memory:").unwrap();
    let sequence = enqueue_integration_adopting(&mut store, successor, base, vec![candidate], Some(predecessor.0));

    let (admits, ack_through) = drain_and_integrate(&mut store, &source).unwrap();

    assert!(admits.is_empty(), "a set missing a member's work resolves nothing");
    assert_eq!(ack_through, Some(sequence), "the refusal is definitive — acked, never re-driven");
}

// #4903 — the mixed supersession: one member arrived on an inherited claim and
// has no ref of its own, the other re-ran under the successor and captured one.
// Both halves are asserted together because either alone passes a wrong fix —
// adopting nothing leaves the inherited member's ref absent and the fold merging
// a branch that is not there, while adopting the whole member set with a forced
// write puts the predecessor's superseded candidate over the fresh capture and
// folds work the re-run replaced.
#[test]
fn a_mixed_supersession_adopts_the_inherited_ref_and_keeps_the_re_run_capture() {
    let (inherited, re_run) = (digest(0xAB), digest(0xAC));
    let (fake, base) = seeded(&inherited);
    fake.seed_git_object(&re_run);
    let (predecessor, successor) = (BloomId(digest(1)), BloomId(digest(2)));

    // wp-0 integrated under the predecessor and transferred with the claim; wp-1
    // re-ran, so both namespaces hold a ref for it and only the successor's is
    // the candidate this fold claims.
    let transferred = seed_candidate_branch(&fake, &predecessor, "wp-0", "tree-a");
    let superseded = seed_candidate_branch(&fake, &predecessor, "wp-1", "tree-stale");
    let captured = seed_candidate_branch(&fake, &successor, "wp-1", "tree-fresh");
    let source = shell(fake.clone());
    let mut store = SqliteStore::open(":memory:").unwrap();
    let sequence =
        enqueue_integration_adopting(&mut store, successor, base, vec![inherited, re_run], Some(predecessor.0));

    let (admits, ack_through) = drain_and_integrate(&mut store, &source).unwrap();

    assert_eq!(admits.len(), 1, "the mixed set folds instead of stalling on a ref addressed under another bloom");
    assert_eq!(ack_through, Some(sequence));
    assert_eq!(
        fake.ref_target(candidate_ref_name(&successor, "wp-0").trim_start_matches("refs/")),
        Some(transferred),
        "the inherited member's ref is adopted into the successor's namespace",
    );

    let re_run_ref = fake.ref_target(candidate_ref_name(&successor, "wp-1").trim_start_matches("refs/"));
    assert_ne!(re_run_ref, Some(superseded), "a forced adoption would have written the superseded candidate here");
    assert_eq!(re_run_ref, Some(captured), "the re-run member keeps the capture it produced under the successor");
}

// A cross-member collision is refused, not re-driven: the two candidates
// conflict until something changes one of them, which is a decision above this
// reactor. Re-driving on a timer would spin forever on an unchanging fact.
#[test]
fn a_conflicting_member_refuses_the_fold_rather_than_re_driving_it() {
    let (first, second) = (digest(0xAB), digest(0xAC));
    let (fake, base) = seeded(&first);
    fake.seed_git_object(&second);
    let bloom = BloomId(digest(1));
    seed_candidate_branch(&fake, &bloom, "wp-0", "tree-a");
    seed_candidate_branch(&fake, &bloom, "wp-1", "tree-b");
    let integration = format!("bloom/{}/integration", short_hex(&bloom.0));
    fake.seed_merge_conflict(&integration, &format!("bloom/{}/candidate/wp-1", short_hex(&bloom.0)));
    let source = shell(fake);
    let mut store = SqliteStore::open(":memory:").unwrap();
    let sequence = enqueue_integration(&mut store, bloom, base, vec![first, second]);

    let (admits, ack_through) = drain_and_integrate(&mut store, &source).unwrap();

    assert!(admits.is_empty(), "a collision admits no resolve");
    assert_eq!(ack_through, Some(sequence), "the refusal is definitive — acked, never re-driven");
}

// #4722 — an aggregate-review finding routes a member back through Refine →
// Verify, and the lap that follows folds a genuinely different tree under the
// same bloom. Keyed by the bloom alone, that second resolve reduced to a
// duplicate and the bloom stopped dead: no wedge, no evidence, no log line.
// The two halves are one invariant — the key separates laps, without weakening
// the crash-replay dedup it exists for, so both are asserted here rather than
// split across tests that could drift apart.
#[test]
fn a_second_integration_of_the_same_bloom_admits_under_its_own_key() {
    let (first, second) = (digest(0xAB), digest(0xAC));
    let (fake, base) = seeded(&first);
    fake.seed_git_object(&second);
    let source = shell(fake);
    let mut store = SqliteStore::open(":memory:").unwrap();
    let bloom = BloomId(digest(1));

    enqueue_integration(&mut store, bloom, base, vec![first]);
    let (first_lap, ack) = drain_and_integrate(&mut store, &source).unwrap();
    store.ack_topic(Topic::Integrate, ack.unwrap()).unwrap();

    // The finding sent the member back around; the repaired attempt captured a
    // different candidate, so this lap folds a different tree.
    enqueue_integration(&mut store, bloom, base, vec![second]);
    let (second_lap, ack) = drain_and_integrate(&mut store, &source).unwrap();
    store.ack_topic(Topic::Integrate, ack.unwrap()).unwrap();

    // The same second-lap decision replayed — a crash before its ack landed.
    enqueue_integration(&mut store, bloom, base, vec![second]);
    let (replayed, _) = drain_and_integrate(&mut store, &source).unwrap();

    assert_eq!(decoded_resolve(&second_lap[0]).1, second, "the second lap folded the repaired candidate");
    assert_ne!(
        admitted_key(&second_lap[0]),
        admitted_key(&first_lap[0]),
        "two laps assert two different integrations, so they admit under two keys",
    );
    assert_eq!(
        admitted_key(&replayed[0]),
        admitted_key(&second_lap[0]),
        "a replay of one lap still reduces to that lap's single key",
    );
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
    store.ack_topic(Topic::Integrate, first_ack.unwrap()).unwrap();

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
