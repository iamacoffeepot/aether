//! The drain → fold → resolve core of the integrate reactor, over a real
//! `SqliteStore` and a fake-GitHub-backed `SourceShell` — the network side the
//! running capability drives, without the mail harness. `init` / the timer / the
//! ctx send are the thin glue the chassis-boot test and compilation cover; this
//! pins the loop that turns a completed claim set into the integration fold and
//! the admitted `Fact::Resolve`.

use std::sync::Arc;

use aether_bloomery::testing::digest;
use aether_bloomery::{
    BloomId, Digest, Event, Fact, IdempotencyKey, IntegratePayload, MemberCandidate, SplicePayload, Topic, WorkpieceId,
};
use aether_bloomery_github::testing::FakeGithub;
use aether_bloomery_github::{GitDataApi, GitSource, MainlineRef, MergeResult, short_hex};
use aether_data::wire::{from_bytes, to_vec};

use super::{drain_and_integrate, drain_and_splice};
use crate::artifacts::{ArtifactsCapabilityState, GetResult};
use crate::bloomery::SourceShell;
use crate::bloomery::outbox::TopicOutbox;
use crate::store::{JournalWrite, SqliteStore, StoreBackend};
use aether_bloomery_github::candidate_ref_name;

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
    store.enqueue_topic(Topic::Integrate, &to_vec(&payload).unwrap(), None).unwrap()
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

fn decoded_event(admit: &aether_bloomery::Admit) -> Event {
    from_bytes(&admit.event).unwrap()
}

fn journal_event(store: &mut SqliteStore, event: &Event) {
    let bytes = to_vec(event).unwrap();
    store
        .append_event(&JournalWrite {
            idempotency_key: &event.idempotency_key.0,
            event: &bytes,
            decisions: b"decided",
            decider: "test",
        })
        .unwrap();
}

fn assert_unacked(store: &mut SqliteStore, topic: Topic, sequence: u64, ack_through: Option<u64>) {
    assert_eq!(ack_through, None, "receipt persistence is not acknowledgement; the journal is");
    assert_eq!(
        store.drain_topic(topic).unwrap().first().map(|entry| entry.sequence),
        Some(sequence),
        "the entry stays undelivered until the journal holds every result key",
    );
}

fn confirm_journaled(
    store: &mut SqliteStore,
    source: &SourceShell,
    topic: Topic,
    admits: &[aether_bloomery::Admit],
) -> u64 {
    for admit in admits {
        journal_event(store, &decoded_event(admit));
    }
    let (replayed, ack_through) = match topic {
        Topic::Integrate => drain_and_integrate(store, source, None).unwrap(),
        Topic::Splice => drain_and_splice(store, source, None).unwrap(),
        other => panic!("unexpected topic {other:?}"),
    };
    assert!(replayed.is_empty(), "a journaled receipt does not re-admit");
    let sequence = ack_through.expect("the journaled prefix acknowledges");
    store.ack_topic(topic, sequence).unwrap();
    sequence
}

fn file_store_path() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bloomery.db").to_str().unwrap().to_owned();
    (dir, path)
}

fn integration_ref(bloom: &BloomId) -> String {
    format!("heads/bloom/{}/integration", short_hex(&bloom.0))
}

fn retarget_integration(fake: &FakeGithub, bloom: &BloomId) {
    let foreign = fake.create_commit("foreign", "tree-foreign", &[]).unwrap();
    fake.seed_ref(&integration_ref(bloom), &foreign.sha);
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

    let (admits, ack_through) = drain_and_integrate(&mut store, &source, None).unwrap();

    assert_eq!(admits.len(), 1, "a folded bloom admits one resolve");
    assert_unacked(&mut store, Topic::Integrate, sequence, ack_through);
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

    let (admits, ack_through) = drain_and_integrate(&mut store, &source, None).unwrap();

    assert_eq!(admits.len(), 1, "a multi-member fold resolves rather than failing closed");
    assert_unacked(&mut store, Topic::Integrate, sequence, ack_through);
    let (_, tree, head, lineage) = decoded_resolve(&admits[0]);
    assert_ne!(tree, second, "a tree-replace would have produced exactly the last member's candidate");
    assert_ne!(tree, first, "nor the first member's — the fold combined them");
    assert_ne!(head, tree, "the landable head stays a distinct commit digest");
    assert_eq!(lineage, vec![first, second], "the lineage records every member's candidate, in member order");
}

// ADR-0196 G2 — a multi-tip join merges the named tips onto a scratch
// branch and admits SpliceAssembled, not the weave's Resolve. Catches
// routing a structural join through the bloom integration branch (which
// would steal the weave's resume position) or skipping assembly entirely.
#[test]
fn a_multi_tip_join_admits_splice_assembled_not_resolve() {
    let (first, second) = (digest(0xAB), digest(0xAC));
    let (fake, base) = seeded(&first);
    fake.seed_git_object(&second);
    let bloom = BloomId(digest(1));
    seed_candidate_branch(&fake, &bloom, "wp-a", "tree-a");
    seed_candidate_branch(&fake, &bloom, "wp-c", "tree-c");
    let source = shell(fake);
    let mut store = SqliteStore::open(":memory:").unwrap();
    let payload = SplicePayload {
        bloom: bloom.0,
        workpiece: WorkpieceId("wp-b".into()),
        base,
        members: vec![
            MemberCandidate { workpiece: WorkpieceId("wp-a".into()), candidate: first },
            MemberCandidate { workpiece: WorkpieceId("wp-c".into()), candidate: second },
        ],
        adopt_from: None,
    };
    let sequence = store.enqueue_topic(Topic::Splice, &to_vec(&payload).unwrap(), None).unwrap();

    let (admits, ack_through) = drain_and_splice(&mut store, &source, None).unwrap();

    assert_eq!(admits.len(), 1, "a clean join admits one assembled splice");
    assert_unacked(&mut store, Topic::Splice, sequence, ack_through);
    let event: Event = from_bytes(&admits[0].event).unwrap();
    match event.fact {
        Fact::SpliceAssembled { bloom: assembled_bloom, workpiece, tree, head } => {
            assert_eq!(assembled_bloom, bloom);
            assert_eq!(workpiece.0, "wp-b");
            assert_ne!(tree, first, "the assembled tree is not a single tip");
            assert_ne!(tree, second, "nor the other tip — the host merged them");
            assert_ne!(head, tree, "the checkout commit stays distinct from the tree");
        }
        other => panic!("expected Fact::SpliceAssembled, got {other:?}"),
    }
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

    let (admits, ack_through) = drain_and_integrate(&mut store, &source, None).unwrap();

    assert_eq!(admits.len(), 1, "the inherited work folds under the successor");
    assert_unacked(&mut store, Topic::Integrate, sequence, ack_through);
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

    let (admits, ack_through) = drain_and_integrate(&mut store, &source, None).unwrap();

    assert_eq!(admits.len(), 1, "a set missing a member's work admits the refusal rather than resolving");
    assert_unacked(&mut store, Topic::Integrate, sequence, ack_through);
    let event: Event = from_bytes(&admits[0].event).unwrap();
    match event.fact {
        Fact::FoldRefused { bloom: refused, refusal } => {
            assert_eq!(refused, successor);
            assert_eq!(refusal.gate, "fold");
            assert_eq!(refusal.guard, "candidate_ref_present");
            assert_eq!(
                refusal.reads.iter().find(|read| read.field == "member").map(|read| read.value.as_str()),
                Some("wp-0")
            );
        }
        other => panic!("expected Fact::FoldRefused, got {other:?}"),
    }
}

// A bloom superseded twice still has the inherited candidate under the
// grandparent. Looking only at the parent refuses a set that has the work.
// The claim transfers A→B→C are the provenance that names A as an ancestor;
// a fabricated grandparent candidate without that chain is not.
#[test]
fn a_twice_superseded_bloom_adopts_the_grandparent_candidate_ref() {
    let candidate = digest(0xAB);
    let (fake, base) = seeded(&candidate);
    let (grandparent, parent, successor) = (BloomId(digest(1)), BloomId(digest(2)), BloomId(digest(3)));
    let source = shell(fake.clone());
    let member = [WorkpieceId("wp-0".into())];
    source.claim_seal(&grandparent, &member).expect("grandparent acquires the workpiece claim");
    source.transfer_seal(&grandparent, &parent, &member, &[], &[]).expect("A→B transfer records the predecessor");
    source.transfer_seal(&parent, &successor, &member, &[], &[]).expect("B→C transfer records the grandparent lineage");
    seed_candidate_branch(&fake, &grandparent, "wp-0", "tree-a");
    let mut store = SqliteStore::open(":memory:").unwrap();
    let sequence = enqueue_integration_adopting(&mut store, successor, base, vec![candidate], Some(parent.0));

    let (admits, ack_through) = drain_and_integrate(&mut store, &source, None).unwrap();

    assert_eq!(admits.len(), 1, "the inherited work folds under the successor");
    assert_unacked(&mut store, Topic::Integrate, sequence, ack_through);
    let _ = decoded_resolve(&admits[0]);
    assert!(
        fake.ref_exists(candidate_ref_name(&successor, "wp-0").trim_start_matches("refs/")),
        "the grandparent's candidate ref now lives in the successor's namespace",
    );
}

// A fold that succeeds records no refusal — the resolve is the only admit.
#[test]
fn a_successful_fold_records_no_refusal() {
    let candidate = digest(0xAB);
    let (fake, base) = seeded(&candidate);
    let source = shell(fake);
    let mut store = SqliteStore::open(":memory:").unwrap();
    let bloom = BloomId(digest(1));
    let _ = enqueue_integration(&mut store, bloom, base, vec![candidate]);

    let (admits, _) = drain_and_integrate(&mut store, &source, None).unwrap();

    assert_eq!(admits.len(), 1);
    let event: Event = from_bytes(&admits[0].event).unwrap();
    assert!(matches!(event.fact, Fact::Resolve { .. }), "a successful fold admits a resolve, not a refusal");
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

    let (admits, ack_through) = drain_and_integrate(&mut store, &source, None).unwrap();

    assert_eq!(admits.len(), 1, "the mixed set folds instead of stalling on a ref addressed under another bloom");
    assert_unacked(&mut store, Topic::Integrate, sequence, ack_through);
    assert_eq!(
        fake.ref_target(candidate_ref_name(&successor, "wp-0").trim_start_matches("refs/")),
        Some(transferred),
        "the inherited member's ref is adopted into the successor's namespace",
    );

    let re_run_ref = fake.ref_target(candidate_ref_name(&successor, "wp-1").trim_start_matches("refs/"));
    assert_ne!(re_run_ref, Some(superseded), "a forced adoption would have written the superseded candidate here");
    assert_eq!(re_run_ref, Some(captured), "the re-run member keeps the capture it produced under the successor");
}

// ADR-0189 — a cross-member collision admits FoldConflict rather than
// refusing in prose. The later member is the one that reconciles; the
// folded checkpoint is the tree it collided with; the overlay names the
// paths. Re-driving the same trees cannot resolve it, so the entry is
// retained once the fact is persisted.
#[test]
fn a_conflicting_member_admits_fold_conflict_instead_of_refusing() {
    let (first, second) = (digest(0xAB), digest(0xAC));
    let (fake, base) = seeded(&first);
    fake.seed_git_object(&second);
    let bloom = BloomId(digest(1));
    seed_candidate_branch(&fake, &bloom, "wp-0", "tree-a");
    seed_candidate_branch(&fake, &bloom, "wp-1", "tree-b");
    let integration = format!("bloom/{}/integration", short_hex(&bloom.0));
    let candidate = format!("bloom/{}/candidate/wp-1", short_hex(&bloom.0));
    fake.seed_merge_conflict_paths(&integration, &candidate, vec!["crates/overlap.rs".into()]);
    let source = shell(fake);
    let mut store = SqliteStore::open(":memory:").unwrap();
    let sequence = enqueue_integration(&mut store, bloom, base, vec![first, second]);

    let (admits, ack_through) = drain_and_integrate(&mut store, &source, None).unwrap();

    assert_eq!(admits.len(), 1, "a collision admits FoldConflict, not a resolve");
    assert_unacked(&mut store, Topic::Integrate, sequence, ack_through);
    let event: Event = from_bytes(&admits[0].event).unwrap();
    match event.fact {
        Fact::FoldConflict { bloom: collided, workpiece, checkpoint, evidence, .. } => {
            assert_eq!(collided, bloom);
            assert_eq!(workpiece.0, "wp-1", "the later-folding member absorbs reconciliation");
            assert_ne!(checkpoint, first, "the checkpoint is the folded tree, not the colliding candidate");
            assert_eq!(evidence.kind, aether_bloomery::EvidenceKind::FoldConflict);
            assert_eq!(evidence.subject, checkpoint);
        }
        other => panic!("expected Fact::FoldConflict, got {other:?}"),
    }
    let overlay = store.lookup_fold_conflict(bloom.0.as_bytes(), "wp-1").unwrap().expect("the overlay was persisted");
    assert!(overlay.contains("## Fold conflict"), "the contract is in the overlay");
    assert!(overlay.contains("crates/overlap.rs"), "the conflicting path is in the overlay");
    assert!(
        overlay.contains("## Conflicted candidate") && overlay.contains("diff --git"),
        "the member's conflicted work is in the overlay, not a content-address hex",
    );
    assert!(
        !overlay.contains("ours") && !overlay.contains("theirs") && !overlay.contains("union"),
        "textual merge strategies are not a fold-path mechanism",
    );
}

// #4952 (acceptance 1) — a fold that hits a collision finishes folding every
// non-conflicting candidate first, and every conflicted member is then sent to
// reconcile against that one settled tree.
//
// The fixture interleaves so both halves are load-bearing: wp-1 collides ahead
// of a clean wp-2, and wp-3 collides behind it. Returning at the first collision
// admitted one fact, left wp-2 and wp-3 unfolded, and named a checkpoint the
// rest of the fold would then move — so wp-1 reconciled against a tree nobody
// would land on and paid a second round for a collision it never had, which is
// the `10a1228c` cascade.
#[test]
fn a_collision_settles_the_fold_before_any_member_reconciles() {
    let candidates: Vec<Digest> = (0xA0..0xA4).map(digest).collect();
    let (fake, base) = seeded(&candidates[0]);
    for candidate in &candidates[1..] {
        fake.seed_git_object(candidate);
    }
    let bloom = BloomId(digest(1));
    for index in 0..candidates.len() {
        seed_candidate_branch(&fake, &bloom, &format!("wp-{index}"), &format!("tree-{index}"));
    }
    let integration = format!("bloom/{}/integration", short_hex(&bloom.0));
    let candidate_ref = |workpiece: &str| format!("bloom/{}/candidate/{workpiece}", short_hex(&bloom.0));
    for conflicted in ["wp-1", "wp-3"] {
        fake.seed_merge_conflict_paths(&integration, &candidate_ref(conflicted), vec!["crates/overlap.rs".into()]);
    }
    let source = shell(fake.clone());
    let mut store = SqliteStore::open(":memory:").unwrap();
    let sequence = enqueue_integration(&mut store, bloom, base, candidates);

    let (admits, ack_through) = drain_and_integrate(&mut store, &source, None).unwrap();

    assert_unacked(&mut store, Topic::Integrate, sequence, ack_through);
    let conflicts: Vec<(WorkpieceId, Digest, Digest)> = admits
        .iter()
        .map(|admit| match from_bytes::<Event>(&admit.event).unwrap().fact {
            Fact::FoldConflict { workpiece, checkpoint, head, .. } => (workpiece, checkpoint, head),
            other => panic!("expected Fact::FoldConflict, got {other:?}"),
        })
        .collect();
    assert_eq!(
        conflicts.iter().map(|(workpiece, ..)| workpiece.0.as_str()).collect::<Vec<_>>(),
        vec!["wp-1", "wp-3"],
        "every conflicted member is journaled in one pass, in member order",
    );
    assert_eq!(conflicts[0].1, conflicts[1].1, "both reconcile against the same settled tree");
    assert_eq!(conflicts[0].2, conflicts[1].2, "and check out the same settled head");

    // The decisive half: the clean member sitting *behind* the first collision
    // is in the settled tree, so nothing the conflicted members reconcile onto
    // can move underneath them.
    assert!(
        matches!(fake.merge(&integration, &candidate_ref("wp-2"), "probe").unwrap(), MergeResult::AlreadyUpToDate),
        "the fold finished the non-conflicting candidates behind the collision",
    );
    for conflicted in ["wp-1", "wp-3"] {
        let overlay = store.lookup_fold_conflict(bloom.0.as_bytes(), conflicted).unwrap();
        assert!(overlay.is_some_and(|overlay| overlay.contains("crates/overlap.rs")), "{conflicted} has its overlay");
    }
}

// A re-collision at the same folded checkpoint with a *new* candidate must
// admit under a distinct key. Keyed only on (bloom, workpiece, checkpoint)
// the second lap is a duplicate, no fact reduces, and the bloom stops.
#[test]
fn a_re_collision_at_the_same_checkpoint_admits_under_the_new_candidates_key() {
    let (first, second, retried) = (digest(0xAB), digest(0xAC), digest(0xAD));
    let (fake, base) = seeded(&first);
    fake.seed_git_object(&second);
    fake.seed_git_object(&retried);
    let bloom = BloomId(digest(1));
    seed_candidate_branch(&fake, &bloom, "wp-0", "tree-a");
    seed_candidate_branch(&fake, &bloom, "wp-1", "tree-b");
    let integration = format!("bloom/{}/integration", short_hex(&bloom.0));
    let candidate = format!("bloom/{}/candidate/wp-1", short_hex(&bloom.0));
    fake.seed_merge_conflict_paths(&integration, &candidate, vec!["crates/overlap.rs".into()]);
    let source = shell(fake.clone());
    let mut store = SqliteStore::open(":memory:").unwrap();

    let first_sequence = enqueue_integration(&mut store, bloom, base, vec![first, second]);
    let (first_lap, ack) = drain_and_integrate(&mut store, &source, None).unwrap();
    assert_unacked(&mut store, Topic::Integrate, first_sequence, ack);
    assert_eq!(confirm_journaled(&mut store, &source, Topic::Integrate, &first_lap), first_sequence);

    seed_candidate_branch(&fake, &bloom, "wp-1", "tree-retried");
    let second_sequence = enqueue_integration(&mut store, bloom, base, vec![first, retried]);
    let (second_lap, ack) = drain_and_integrate(&mut store, &source, None).unwrap();
    assert_unacked(&mut store, Topic::Integrate, second_sequence, ack);
    let (replayed, ack) = drain_and_integrate(&mut store, &source, None).unwrap();
    assert_unacked(&mut store, Topic::Integrate, second_sequence, ack);

    assert!(
        matches!(from_bytes::<Event>(&first_lap[0].event).unwrap().fact, Fact::FoldConflict { .. }),
        "the first collision journals FoldConflict",
    );
    assert!(
        matches!(from_bytes::<Event>(&second_lap[0].event).unwrap().fact, Fact::FoldConflict { .. }),
        "a new candidate against the same fold is a new collision",
    );
    assert_ne!(
        admitted_key(&second_lap[0]),
        admitted_key(&first_lap[0]),
        "the conflicted candidate belongs in the key, or the second lap is swallowed",
    );
    assert_eq!(
        admitted_key(&replayed[0]),
        admitted_key(&second_lap[0]),
        "a replay of one lap still reduces to that lap's single key",
    );
}

// #5083 — an ancestry repair can replace the candidate-ref commit while
// preserving the exact tree the first collision keyed on. Keyed only on
// (bloom, workpiece, checkpoint, tree) the repaired merge is a duplicate,
// the outbox advances, and no Resolve follows.
#[test]
fn an_ancestry_corrected_re_collision_admits_under_the_new_checkout_key() {
    let (first, second) = (digest(0xAB), digest(0xAC));
    let (fake, base) = seeded(&first);
    fake.seed_git_object(&second);
    let bloom = BloomId(digest(1));
    seed_candidate_branch(&fake, &bloom, "wp-0", "tree-a");
    let original = seed_candidate_branch(&fake, &bloom, "wp-1", "tree-b");
    let integration = format!("bloom/{}/integration", short_hex(&bloom.0));
    let candidate = format!("bloom/{}/candidate/wp-1", short_hex(&bloom.0));
    fake.seed_merge_conflict_paths(&integration, &candidate, vec!["crates/overlap.rs".into()]);
    let source = shell(fake.clone());
    let mut store = SqliteStore::open(":memory:").unwrap();

    let first_sequence = enqueue_integration(&mut store, bloom, base, vec![first, second]);
    let (first_lap, ack) = drain_and_integrate(&mut store, &source, None).unwrap();
    assert_unacked(&mut store, Topic::Integrate, first_sequence, ack);
    assert_eq!(confirm_journaled(&mut store, &source, Topic::Integrate, &first_lap), first_sequence);

    let parent = fake.create_commit("parent", "tree-parent", &[]).unwrap();
    let repaired = fake.create_commit("wp-1", "tree-b", &[parent.sha]).unwrap();
    assert_ne!(repaired.sha, original, "the repaired checkout is a different commit");
    assert_eq!(repaired.tree, "tree-b", "the tree identity is unchanged");
    fake.seed_ref(candidate_ref_name(&bloom, "wp-1").trim_start_matches("refs/"), &repaired.sha);

    let second_sequence = enqueue_integration(&mut store, bloom, base, vec![first, second]);
    let (second_lap, ack) = drain_and_integrate(&mut store, &source, None).unwrap();
    assert_unacked(&mut store, Topic::Integrate, second_sequence, ack);
    let (replayed, ack) = drain_and_integrate(&mut store, &source, None).unwrap();
    assert_unacked(&mut store, Topic::Integrate, second_sequence, ack);

    assert!(
        matches!(from_bytes::<Event>(&first_lap[0].event).unwrap().fact, Fact::FoldConflict { .. }),
        "the first collision journals FoldConflict",
    );
    assert!(
        matches!(from_bytes::<Event>(&second_lap[0].event).unwrap().fact, Fact::FoldConflict { .. }),
        "the ancestry-corrected checkout is a new collision",
    );
    assert_ne!(
        admitted_key(&second_lap[0]),
        admitted_key(&first_lap[0]),
        "the attempted checkout belongs in the key, or the repaired merge is swallowed",
    );
    assert_eq!(
        admitted_key(&replayed[0]),
        admitted_key(&second_lap[0]),
        "a replay of the corrected checkout still reduces to that attempt's single key",
    );
}

// The overlay bytes are filed under the same sha256 the evidence details, so
// a wedge later resolves the address against the artifacts store.
#[test]
fn a_fold_conflict_files_its_overlay_under_the_evidence_detail() {
    let (first, second) = (digest(0xAB), digest(0xAC));
    let (fake, base) = seeded(&first);
    fake.seed_git_object(&second);
    let bloom = BloomId(digest(1));
    seed_candidate_branch(&fake, &bloom, "wp-0", "tree-a");
    seed_candidate_branch(&fake, &bloom, "wp-1", "tree-b");
    let integration = format!("bloom/{}/integration", short_hex(&bloom.0));
    let candidate = format!("bloom/{}/candidate/wp-1", short_hex(&bloom.0));
    fake.seed_merge_conflict_paths(&integration, &candidate, vec!["crates/overlap.rs".into()]);
    let source = shell(fake);
    let mut store = SqliteStore::open(":memory:").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut artifacts = ArtifactsCapabilityState::open(dir.path()).unwrap();
    enqueue_integration(&mut store, bloom, base, vec![first, second]);

    let (admits, _) = drain_and_integrate(&mut store, &source, Some(&mut artifacts)).unwrap();
    let event: Event = from_bytes(&admits[0].event).unwrap();
    let Fact::FoldConflict { evidence, .. } = event.fact else {
        panic!("expected FoldConflict, got {:?}", event.fact);
    };
    let hex = evidence.detail.to_hex();
    match artifacts.get(hex) {
        GetResult::Ok { bytes, .. } => {
            let overlay = String::from_utf8(bytes).unwrap();
            assert!(overlay.contains("crates/overlap.rs"), "the stored bytes are the overlay");
        }
        GetResult::Err { error, .. } => panic!("the evidence detail must resolve in the artifacts store: {error:?}"),
    }
}

// ADR-0189 — after the later member produces a reconciled candidate, the
// collision is gone: the fold re-drains, merges, and admits Resolve. The
// stub source is the seam; no operator action sits between the two drains.
#[test]
fn a_reconciled_candidate_re_folds_to_a_resolve() {
    let (first, second, reconciled) = (digest(0xAB), digest(0xAC), digest(0xAD));
    let (fake, base) = seeded(&first);
    fake.seed_git_object(&second);
    fake.seed_git_object(&reconciled);
    let bloom = BloomId(digest(1));
    seed_candidate_branch(&fake, &bloom, "wp-0", "tree-a");
    seed_candidate_branch(&fake, &bloom, "wp-1", "tree-b");
    let integration = format!("bloom/{}/integration", short_hex(&bloom.0));
    let candidate = format!("bloom/{}/candidate/wp-1", short_hex(&bloom.0));
    fake.seed_merge_conflict_paths(&integration, &candidate, vec!["crates/overlap.rs".into()]);
    let source = shell(fake.clone());
    let mut store = SqliteStore::open(":memory:").unwrap();

    let conflict_sequence = enqueue_integration(&mut store, bloom, base, vec![first, second]);
    let (conflicted, ack) = drain_and_integrate(&mut store, &source, None).unwrap();
    assert_unacked(&mut store, Topic::Integrate, conflict_sequence, ack);
    assert_eq!(confirm_journaled(&mut store, &source, Topic::Integrate, &conflicted), conflict_sequence);
    assert!(
        matches!(from_bytes::<Event>(&conflicted[0].event).unwrap().fact, Fact::FoldConflict { .. }),
        "the first drain journals the collision",
    );

    fake.clear_merge_conflict(&integration, &candidate);
    seed_candidate_branch(&fake, &bloom, "wp-1", "tree-reconciled");
    let sequence = enqueue_integration(&mut store, bloom, base, vec![first, reconciled]);
    let (admits, ack_through) = drain_and_integrate(&mut store, &source, None).unwrap();

    assert_unacked(&mut store, Topic::Integrate, sequence, ack_through);
    assert_eq!(admits.len(), 1, "the reconciled candidate folds instead of colliding again");
    let (_, tree, head, lineage) = decoded_resolve(&admits[0]);
    assert_ne!(tree, first, "the fold combined both members");
    assert_ne!(tree, reconciled, "and is not a tree-replace of the later member");
    assert_ne!(head, tree);
    assert_eq!(lineage, vec![first, reconciled]);
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

    let first_sequence = enqueue_integration(&mut store, bloom, base, vec![first]);
    let (first_lap, ack) = drain_and_integrate(&mut store, &source, None).unwrap();
    assert_unacked(&mut store, Topic::Integrate, first_sequence, ack);
    assert_eq!(confirm_journaled(&mut store, &source, Topic::Integrate, &first_lap), first_sequence);

    // The finding sent the member back around; the repaired attempt captured a
    // different candidate, so this lap folds a different tree.
    let second_sequence = enqueue_integration(&mut store, bloom, base, vec![second]);
    let (second_lap, ack) = drain_and_integrate(&mut store, &source, None).unwrap();
    assert_unacked(&mut store, Topic::Integrate, second_sequence, ack);
    let (replayed, ack) = drain_and_integrate(&mut store, &source, None).unwrap();
    assert_unacked(&mut store, Topic::Integrate, second_sequence, ack);

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

// ADR-0152 — two distinct replay shapes. An original undelivered row resends
// the retained resolve without re-folding. A newly enqueued same payload after
// journal+ack is Unrecorded and recovers the head from the branch position.
#[test]
fn a_re_drain_after_the_fold_recovers_the_head_without_re_integrating() {
    let candidate = digest(0xAB);
    let (fake, base) = seeded(&candidate);
    let source = shell(fake);
    let mut store = SqliteStore::open(":memory:").unwrap();
    let bloom = BloomId(digest(1));

    let first_sequence = enqueue_integration(&mut store, bloom, base, vec![candidate]);
    let (first, first_ack) = drain_and_integrate(&mut store, &source, None).unwrap();
    assert_unacked(&mut store, Topic::Integrate, first_sequence, first_ack);
    let (retained, retained_ack) = drain_and_integrate(&mut store, &source, None).unwrap();
    assert_unacked(&mut store, Topic::Integrate, first_sequence, retained_ack);
    assert_eq!(retained[0].event, first[0].event, "the original undelivered row resends the retained resolve");
    assert_eq!(confirm_journaled(&mut store, &source, Topic::Integrate, &first), first_sequence);

    // A newly enqueued same payload: the branch is already at the candidate.
    let sequence = enqueue_integration(&mut store, bloom, base, vec![candidate]);
    let (second, second_ack) = drain_and_integrate(&mut store, &source, None).unwrap();

    assert_unacked(&mut store, Topic::Integrate, sequence, second_ack);
    assert_eq!(second.len(), 1, "the newly enqueued row still admits the resolve");
    let (_, first_tree, first_head, _) = decoded_resolve(&first[0]);
    let (_, second_tree, second_head, _) = decoded_resolve(&second[0]);
    assert_eq!(second_tree, first_tree);
    assert_eq!(second_head, first_head, "the recovered head is the one the fold produced, not a re-mint");
}

// Tripwire: a crash after the fold and before control journals the admit must
// not re-effect. The receipt row is the retained Resolve; a later source
// mutation that would refuse on re-fold still resends the original bytes.
#[test]
fn a_persisted_resolve_survives_source_mutation_without_re_effect() {
    let candidate = digest(0xAB);
    let (fake, base) = seeded(&candidate);
    let source = shell(fake.clone());
    let (_dir, path) = file_store_path();
    let mut store = SqliteStore::open(&path).unwrap();
    let bloom = BloomId(digest(1));
    let sequence = enqueue_integration(&mut store, bloom, base, vec![candidate]);

    let (admits, ack_through) = drain_and_integrate(&mut store, &source, None).unwrap();
    assert_unacked(&mut store, Topic::Integrate, sequence, ack_through);
    let (resolved_bloom, tree, head, _) = decoded_resolve(&admits[0]);
    let retained = admits[0].event.clone();
    let key = admitted_key(&admits[0]);
    retarget_integration(&fake, &bloom);
    let commits = fake.create_commit_count();
    drop(store);

    let mut store = SqliteStore::open(&path).unwrap();
    let (again, ack_through) = drain_and_integrate(&mut store, &source, None).unwrap();
    assert_unacked(&mut store, Topic::Integrate, sequence, ack_through);
    assert_eq!(again[0].event, retained, "the retained resolve bytes do not change");
    let (again_bloom, again_tree, again_head, _) = decoded_resolve(&again[0]);
    assert_eq!(again_bloom, resolved_bloom);
    assert_eq!(again_tree, tree);
    assert_eq!(again_head, head);
    assert_eq!(admitted_key(&again[0]), key);
    assert_eq!(fake.create_commit_count(), commits, "replay must not re-fold");
}

// Tripwire: a spliced join is the same receipt shape as Resolve. Mutating the
// scratch branch after persistence must not assemble a different tree.
#[test]
fn a_persisted_splice_survives_source_mutation_without_re_effect() {
    let (first, second) = (digest(0xAB), digest(0xAC));
    let (fake, base) = seeded(&first);
    fake.seed_git_object(&second);
    let bloom = BloomId(digest(1));
    seed_candidate_branch(&fake, &bloom, "wp-a", "tree-a");
    seed_candidate_branch(&fake, &bloom, "wp-c", "tree-c");
    let source = shell(fake.clone());
    let (_dir, path) = file_store_path();
    let mut store = SqliteStore::open(&path).unwrap();
    let payload = SplicePayload {
        bloom: bloom.0,
        workpiece: WorkpieceId("wp-b".into()),
        base,
        members: vec![
            MemberCandidate { workpiece: WorkpieceId("wp-a".into()), candidate: first },
            MemberCandidate { workpiece: WorkpieceId("wp-c".into()), candidate: second },
        ],
        adopt_from: None,
    };
    let sequence = store.enqueue_topic(Topic::Splice, &to_vec(&payload).unwrap(), None).unwrap();

    let (admits, ack_through) = drain_and_splice(&mut store, &source, None).unwrap();
    assert_unacked(&mut store, Topic::Splice, sequence, ack_through);
    let retained = admits[0].event.clone();
    let key = admitted_key(&admits[0]);
    let namespace = super::splice_namespace(&bloom, &WorkpieceId("wp-b".into()));
    retarget_integration(&fake, &namespace);
    let commits = fake.create_commit_count();
    drop(store);

    let mut store = SqliteStore::open(&path).unwrap();
    let (again, ack_through) = drain_and_splice(&mut store, &source, None).unwrap();
    assert_unacked(&mut store, Topic::Splice, sequence, ack_through);
    assert_eq!(again[0].event, retained);
    assert_eq!(admitted_key(&again[0]), key);
    assert!(matches!(decoded_event(&again[0]).fact, Fact::SpliceAssembled { .. }));
    assert_eq!(fake.create_commit_count(), commits, "replay must not re-splice");
}

// Tripwire: a refusal is now a retained result, not an immediate ack. Seeding
// the missing ref after persistence must not fold a resolve.
#[test]
fn a_persisted_refusal_survives_a_now_valid_source() {
    let candidate = digest(0xAB);
    let (fake, base) = seeded(&candidate);
    let (predecessor, successor) = (BloomId(digest(1)), BloomId(digest(2)));
    let source = shell(fake.clone());
    let (_dir, path) = file_store_path();
    let mut store = SqliteStore::open(&path).unwrap();
    let sequence = enqueue_integration_adopting(&mut store, successor, base, vec![candidate], Some(predecessor.0));

    let (admits, ack_through) = drain_and_integrate(&mut store, &source, None).unwrap();
    assert_unacked(&mut store, Topic::Integrate, sequence, ack_through);
    assert!(matches!(decoded_event(&admits[0]).fact, Fact::FoldRefused { .. }));
    let retained = admits[0].event.clone();
    seed_candidate_branch(&fake, &predecessor, "wp-0", "tree-a");
    let commits = fake.create_commit_count();
    drop(store);

    let mut store = SqliteStore::open(&path).unwrap();
    let (again, ack_through) = drain_and_integrate(&mut store, &source, None).unwrap();
    assert_unacked(&mut store, Topic::Integrate, sequence, ack_through);
    assert_eq!(again[0].event, retained);
    assert!(matches!(decoded_event(&again[0]).fact, Fact::FoldRefused { .. }));
    assert_eq!(fake.create_commit_count(), commits, "replay must not re-fold the now-valid set");
}

// Tripwire: a FoldConflict receipt must not re-merge after the collision is
// cleared. Re-effect would admit Resolve and drop the reconcile.
#[test]
fn a_persisted_conflict_survives_a_cleared_merge() {
    let (first, second) = (digest(0xAB), digest(0xAC));
    let (fake, base) = seeded(&first);
    fake.seed_git_object(&second);
    let bloom = BloomId(digest(1));
    seed_candidate_branch(&fake, &bloom, "wp-0", "tree-a");
    seed_candidate_branch(&fake, &bloom, "wp-1", "tree-b");
    let integration = format!("bloom/{}/integration", short_hex(&bloom.0));
    let candidate = format!("bloom/{}/candidate/wp-1", short_hex(&bloom.0));
    fake.seed_merge_conflict_paths(&integration, &candidate, vec!["crates/overlap.rs".into()]);
    let source = shell(fake.clone());
    let (_dir, path) = file_store_path();
    let mut store = SqliteStore::open(&path).unwrap();
    let sequence = enqueue_integration(&mut store, bloom, base, vec![first, second]);

    let (admits, ack_through) = drain_and_integrate(&mut store, &source, None).unwrap();
    assert_unacked(&mut store, Topic::Integrate, sequence, ack_through);
    assert!(matches!(decoded_event(&admits[0]).fact, Fact::FoldConflict { .. }));
    let retained = admits[0].event.clone();
    let key = admitted_key(&admits[0]);
    fake.clear_merge_conflict(&integration, &candidate);
    let commits = fake.create_commit_count();
    drop(store);

    let mut store = SqliteStore::open(&path).unwrap();
    let (again, ack_through) = drain_and_integrate(&mut store, &source, None).unwrap();
    assert_unacked(&mut store, Topic::Integrate, sequence, ack_through);
    assert_eq!(again[0].event, retained);
    assert_eq!(admitted_key(&again[0]), key);
    assert!(matches!(decoded_event(&again[0]).fact, Fact::FoldConflict { .. }));
    assert_eq!(fake.create_commit_count(), commits, "replay must not re-merge a cleared collision");
}

// Tripwire: a pending earlier receipt blocks the prefix. A later valid row
// must not fold while the earlier result is still waiting on the journal.
#[test]
fn an_earlier_pending_result_holds_later_rows() {
    let (first_candidate, second_candidate) = (digest(0xAB), digest(0xAC));
    let (fake, base) = seeded(&first_candidate);
    fake.seed_git_object(&second_candidate);
    let source = shell(fake.clone());
    let (_dir, path) = file_store_path();
    let mut store = SqliteStore::open(&path).unwrap();
    let first = BloomId(digest(1));
    let second = BloomId(digest(2));
    let first_sequence = enqueue_integration(&mut store, first, base, vec![first_candidate]);
    let second_sequence = enqueue_integration(&mut store, second, base, vec![second_candidate]);

    let (admits, ack_through) = drain_and_integrate(&mut store, &source, None).unwrap();
    assert_eq!(ack_through, None, "the earlier receipt is not acknowledgement");
    assert_eq!(decoded_resolve(&admits[0]).0, first);
    assert_eq!(
        store.drain_topic(Topic::Integrate).unwrap().iter().map(|entry| entry.sequence).collect::<Vec<_>>(),
        vec![first_sequence, second_sequence],
    );
    assert!(fake.ref_exists(&integration_ref(&first)));
    assert!(
        !fake.ref_exists(&integration_ref(&second)),
        "the later row must not fold while the earlier receipt is pending",
    );

    retarget_integration(&fake, &first);
    drop(store);
    let mut store = SqliteStore::open(&path).unwrap();
    let (again, ack_through) = drain_and_integrate(&mut store, &source, None).unwrap();
    assert_eq!(ack_through, None);
    assert_eq!(again[0].event, admits[0].event);
    assert_eq!(
        store.drain_topic(Topic::Integrate).unwrap().iter().map(|entry| entry.sequence).collect::<Vec<_>>(),
        vec![first_sequence, second_sequence],
    );
    assert!(!fake.ref_exists(&integration_ref(&second)), "a reopened pending prefix still holds the later row");
}

// Tripwire: two FoldConflict keys are AND, not OR. Journaling the first key
// still leaves the batch pending; the second key releases only that prefix.
#[test]
fn two_fold_conflict_results_require_both_journal_keys() {
    let candidates: Vec<Digest> = (0xA0..0xA4).map(digest).collect();
    let (fake, base) = seeded(&candidates[0]);
    for candidate in &candidates[1..] {
        fake.seed_git_object(candidate);
    }
    let bloom = BloomId(digest(1));
    for index in 0..candidates.len() {
        seed_candidate_branch(&fake, &bloom, &format!("wp-{index}"), &format!("tree-{index}"));
    }
    let integration = format!("bloom/{}/integration", short_hex(&bloom.0));
    let candidate_ref = |workpiece: &str| format!("bloom/{}/candidate/{workpiece}", short_hex(&bloom.0));
    for conflicted in ["wp-1", "wp-3"] {
        fake.seed_merge_conflict_paths(&integration, &candidate_ref(conflicted), vec!["crates/overlap.rs".into()]);
    }
    let source = shell(fake.clone());
    let (_dir, path) = file_store_path();
    let mut store = SqliteStore::open(&path).unwrap();
    let sequence = enqueue_integration(&mut store, bloom, base, candidates);

    let (admits, ack_through) = drain_and_integrate(&mut store, &source, None).unwrap();
    assert_unacked(&mut store, Topic::Integrate, sequence, ack_through);
    assert_eq!(admits.len(), 2, "both conflicted members are one retained batch");
    let first = decoded_event(&admits[0]);
    let second = decoded_event(&admits[1]);
    journal_event(&mut store, &first);
    for conflicted in ["wp-1", "wp-3"] {
        fake.clear_merge_conflict(&integration, &candidate_ref(conflicted));
    }
    drop(store);

    let mut store = SqliteStore::open(&path).unwrap();
    let (pending, ack_through) = drain_and_integrate(&mut store, &source, None).unwrap();
    assert_unacked(&mut store, Topic::Integrate, sequence, ack_through);
    assert_eq!(pending.len(), 2, "one journaled key is not the whole batch");
    assert_eq!(pending[0].event, admits[0].event);
    assert_eq!(pending[1].event, admits[1].event);

    journal_event(&mut store, &second);
    let (done, ack_through) = drain_and_integrate(&mut store, &source, None).unwrap();
    assert!(done.is_empty(), "both keys journaled means no re-admit");
    assert_eq!(ack_through, Some(sequence), "the second key releases only this prefix");
}

// Tripwire: control journaled the admit, then the process died before ack.
// Restart observes the keys and acknowledges without replaying the effect.
#[test]
fn a_journaled_result_acks_without_replaying_effect() {
    let candidate = digest(0xAB);
    let (fake, base) = seeded(&candidate);
    let source = shell(fake.clone());
    let (_dir, path) = file_store_path();
    let mut store = SqliteStore::open(&path).unwrap();
    let bloom = BloomId(digest(1));
    let sequence = enqueue_integration(&mut store, bloom, base, vec![candidate]);

    let (admits, ack_through) = drain_and_integrate(&mut store, &source, None).unwrap();
    assert_unacked(&mut store, Topic::Integrate, sequence, ack_through);
    journal_event(&mut store, &decoded_event(&admits[0]));
    retarget_integration(&fake, &bloom);
    let commits = fake.create_commit_count();
    drop(store);

    let mut store = SqliteStore::open(&path).unwrap();
    let (again, ack_through) = drain_and_integrate(&mut store, &source, None).unwrap();
    assert!(again.is_empty(), "a journaled receipt does not re-admit");
    assert_eq!(ack_through, Some(sequence), "the restart observes the key and acknowledges");
    assert_eq!(fake.create_commit_count(), commits, "journal-before-ack must not re-fold");
}
