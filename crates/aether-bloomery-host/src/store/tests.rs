//! Contract + recovery tests for the `SQLite` store backend (ADR-0149).
//!
//! Each test drives the [`StoreBackend`] trait over a real [`SqliteStore`] —
//! an in-memory database for the pure-logic contracts, a temp-file database for
//! the reopen/recovery ones (so state survives dropping and reopening the
//! connection, the way a `kill -9` + restart does).

#![allow(clippy::unwrap_used)]

use super::runtime::{AppendOutcome, SealOutcome, SqliteStore, StoreBackend};

fn memory() -> SqliteStore {
    SqliteStore::open(":memory:").unwrap()
}

fn members(names: &[&str]) -> Vec<String> {
    names.iter().map(|s| (*s).to_owned()).collect()
}

#[test]
fn journal_round_trips_a_real_bloom_event() {
    use aether_bloomery::{BloomId, Digest, Event, Fact, IdempotencyKey};
    use aether_data::wire::{from_bytes, to_vec};

    // The store journals a real, wire-encoded bloom-protocol event (the shape the
    // host persists), and replay hands the exact bytes back for the reducer to
    // decode — the recovery contract, byte-for-byte.
    let mut store = memory();
    let event = Event {
        idempotency_key: IdempotencyKey("land-1".to_owned()),
        fact: Fact::Land { bloom: BloomId(Digest::from_bytes([7; 32])), new_head: Digest::from_bytes([9; 32]) },
    };
    store.append_event("land-1", &to_vec(&event).unwrap()).unwrap();

    let journal = store.replay_journal().unwrap();
    assert_eq!(journal.len(), 1);
    let decoded: Event = from_bytes(&journal[0].event).unwrap();
    assert_eq!(decoded, event);
}

#[test]
fn append_then_replay_round_trips_in_order() {
    let mut store = memory();
    assert_eq!(store.append_event("k1", b"alpha").unwrap(), AppendOutcome::Applied(1));
    assert_eq!(store.append_event("k2", b"beta").unwrap(), AppendOutcome::Applied(2));

    let journal = store.replay_journal().unwrap();
    assert_eq!(journal.len(), 2);
    assert_eq!(journal[0].idempotency_key, "k1");
    assert_eq!(journal[0].event, b"alpha");
    assert_eq!(journal[1].idempotency_key, "k2");
    assert_eq!(journal[1].event, b"beta");
}

#[test]
fn inbox_dedup_drops_a_duplicate_idempotency_key() {
    let mut store = memory();
    assert_eq!(store.append_event("dup", b"first").unwrap(), AppendOutcome::Applied(1));
    // A replayed key is a no-op — the second delivery of the same event does not
    // re-append, and the original bytes are untouched.
    assert_eq!(store.append_event("dup", b"second").unwrap(), AppendOutcome::Duplicate);

    let journal = store.replay_journal().unwrap();
    assert_eq!(journal.len(), 1);
    assert_eq!(journal[0].event, b"first");
}

#[test]
fn seal_claims_membership_and_a_foreign_overlap_conflicts() {
    let mut store = memory();
    let bloom_a = b"bloom-a";
    let bloom_b = b"bloom-b";

    assert_eq!(store.claim_seal(bloom_a, &members(&["wp-1", "wp-2"])).unwrap(), SealOutcome::Sealed);
    // A different bloom claiming an overlapping workpiece loses cleanly.
    assert_eq!(
        store.claim_seal(bloom_b, &members(&["wp-2", "wp-3"])).unwrap(),
        SealOutcome::Conflict("wp-2".to_owned()),
    );
}

#[test]
fn failed_seal_is_all_or_nothing() {
    let mut store = memory();
    assert_eq!(store.claim_seal(b"held", &members(&["shared"])).unwrap(), SealOutcome::Sealed);

    // A batch whose first member conflicts must leave its free members
    // unclaimed (the whole transaction rolls back).
    assert_eq!(
        store.claim_seal(b"batch", &members(&["shared", "free-a", "free-b"])).unwrap(),
        SealOutcome::Conflict("shared".to_owned()),
    );
    // Proof the free members were never claimed: a fresh bloom seals them.
    assert_eq!(store.claim_seal(b"other", &members(&["free-a", "free-b"])).unwrap(), SealOutcome::Sealed);
}

#[test]
fn overlapping_seals_yield_one_winner_and_one_clean_loser() {
    let mut store = memory();
    assert_eq!(store.claim_seal(b"first", &members(&["w"])).unwrap(), SealOutcome::Sealed);
    assert_eq!(store.claim_seal(b"second", &members(&["w"])).unwrap(), SealOutcome::Conflict("w".to_owned()));
}

#[test]
fn release_frees_membership_for_reclaim() {
    let mut store = memory();
    store.claim_seal(b"a", &members(&["w1", "w2"])).unwrap();
    assert_eq!(store.release_membership(b"a").unwrap(), 2);
    // Freed workpieces are claimable again.
    assert_eq!(store.claim_seal(b"b", &members(&["w1", "w2"])).unwrap(), SealOutcome::Sealed);
}

#[test]
fn supersession_atomically_releases_and_reclaims() {
    let mut store = memory();
    // Predecessor holds w1, w2.
    store.claim_seal(b"pred", &members(&["w1", "w2"])).unwrap();
    // Successor re-admits w1 (dropping w2) and adds w3 — one atomic transaction
    // releases the predecessor and claims the successor's set.
    assert_eq!(store.supersede(b"pred", b"succ", &members(&["w1", "w3"])).unwrap(), SealOutcome::Sealed,);
    // w2 was released (a fresh bloom can claim it); w1 and w3 are the successor's.
    assert_eq!(store.claim_seal(b"other", &members(&["w2"])).unwrap(), SealOutcome::Sealed);
    assert_eq!(store.claim_seal(b"clash", &members(&["w1"])).unwrap(), SealOutcome::Conflict("w1".to_owned()));
}

#[test]
fn supersession_conflicting_with_a_third_bloom_rolls_back() {
    let mut store = memory();
    store.claim_seal(b"pred", &members(&["w1"])).unwrap();
    store.claim_seal(b"third", &members(&["w9"])).unwrap();
    // The successor wants w9, held by a third bloom → conflict, whole rollback.
    assert_eq!(
        store.supersede(b"pred", b"succ", &members(&["w1", "w9"])).unwrap(),
        SealOutcome::Conflict("w9".to_owned()),
    );
    // The predecessor kept its claim (the DELETE rolled back too).
    assert_eq!(store.claim_seal(b"probe", &members(&["w1"])).unwrap(), SealOutcome::Conflict("w1".to_owned()));
}

#[test]
fn outbox_enqueue_drain_ack_cycle() {
    let mut store = memory();
    assert_eq!(store.enqueue_outbox("receipt", b"r1").unwrap(), 1);
    assert_eq!(store.enqueue_outbox("receipt", b"r2").unwrap(), 2);

    let drained = store.drain_outbox().unwrap();
    assert_eq!(drained.len(), 2);
    assert_eq!(drained[0].sequence, 1);
    assert_eq!(drained[0].payload, b"r1");
    assert_eq!(drained[1].payload, b"r2");

    // Ack the first only — the second stays undelivered for republish.
    assert_eq!(store.ack_outbox(1).unwrap(), 1);
    let remaining = store.drain_outbox().unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].sequence, 2);
}

#[test]
fn reopen_converges_via_journal_replay_and_outbox_republish() {
    // A temp-file DB so state survives dropping and reopening the connection —
    // the reopen models a `kill -9` after a committed transaction + a restart.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bloomery.db");
    let path = path.to_str().unwrap();

    {
        let mut store = SqliteStore::open(path).unwrap();
        store.append_event("seal-1", b"sealed-bloom-event").unwrap();
        store.claim_seal(b"bloom-1", &members(&["wp"])).unwrap();
        store.enqueue_outbox("landing_receipt", b"receipt-bytes").unwrap();
        // Drop without any explicit close — a committed WAL transaction is durable.
    }

    // Restart against the same file.
    let mut store = SqliteStore::open(path).unwrap();
    // Journal replay: the sealed event is present.
    let journal = store.replay_journal().unwrap();
    assert_eq!(journal.len(), 1);
    assert_eq!(journal[0].event, b"sealed-bloom-event");
    // Membership survived: a second seal of the same workpiece still loses.
    assert_eq!(store.claim_seal(b"bloom-2", &members(&["wp"])).unwrap(), SealOutcome::Conflict("wp".to_owned()));
    // Outbox republish: the undelivered receipt is drainable.
    let outbox = store.drain_outbox().unwrap();
    assert_eq!(outbox.len(), 1);
    assert_eq!(outbox[0].topic, "landing_receipt");
    assert_eq!(outbox[0].payload, b"receipt-bytes");
    // A re-delivered seal event is deduped (inbox survived the restart).
    assert_eq!(store.append_event("seal-1", b"anything").unwrap(), AppendOutcome::Duplicate);
}
