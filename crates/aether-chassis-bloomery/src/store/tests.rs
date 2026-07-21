//! Contract + recovery tests for the `SQLite` store backend (ADR-0149).
//!
//! Each test drives the [`StoreBackend`] trait over a real [`SqliteStore`] —
//! an in-memory database for the pure-logic contracts, a temp-file database for
//! the reopen/recovery ones (so state survives dropping and reopening the
//! connection, the way a `kill -9` + restart does).

#![allow(clippy::unwrap_used)]

use super::runtime::{
    AppendOutcome, CommitOutcome, OutstandingOrder, RecordOutcome, SealOutcome, SqliteStore, StoreBackend,
};
use aether_bloomery::{MembershipMutation, OutboxPayload};

fn memory() -> SqliteStore {
    SqliteStore::open(":memory:").unwrap()
}

fn members(names: &[&str]) -> Vec<String> {
    names.iter().map(|s| (*s).to_owned()).collect()
}

fn claim(workpiece: &str, bloom: &[u8]) -> MembershipMutation {
    MembershipMutation { workpiece: workpiece.to_owned(), bloom: bloom.to_vec() }
}

#[test]
fn commit_journals_claims_and_outbox_in_one_transaction() {
    // The combined commit is the control-loop primitive: one transact-mail that
    // journals the event, claims membership, and enqueues the outbox atomically.
    // A successful commit leaves all three durable. Topics here are raw strings
    // on purpose: these tests exercise the store's open string surface (any
    // caller-defined topic), below the typed `Topic` edge.
    let mut store = memory();
    let outcome = store
        .commit(
            "seal-1",
            b"sealed-bloom-event",
            &[],
            &[claim("wp-1", b"bloom-a"), claim("wp-2", b"bloom-a")],
            &[OutboxPayload { topic: "landing_receipt".to_owned(), payload: b"receipt".to_vec() }],
        )
        .unwrap();
    assert_eq!(outcome, CommitOutcome::Applied(1));

    // Journal: the event is present and byte-identical.
    let journal = store.replay_journal().unwrap();
    assert_eq!(journal.len(), 1);
    assert_eq!(journal[0].event, b"sealed-bloom-event");
    // Membership: a foreign bloom claiming an overlapping workpiece loses.
    assert_eq!(store.claim_seal(b"other", &members(&["wp-1"])).unwrap(), SealOutcome::Conflict("wp-1".to_owned()));
    // Outbox: the enqueued receipt is drainable.
    let outbox = store.drain_outbox(None).unwrap();
    assert_eq!(outbox.len(), 1);
    assert_eq!(outbox[0].topic, "landing_receipt");
    assert_eq!(outbox[0].payload, b"receipt");
}

#[test]
fn commit_dedupes_a_replayed_idempotency_key_and_applies_nothing() {
    // A commit whose key was already journaled is a whole no-op: no second
    // journal row, and — the point of the durable backstop — no double-applied
    // membership claim. Tripwire: a replayed commit must not re-claim membership.
    let mut store = memory();
    assert_eq!(store.commit("dup", b"first", &[], &[claim("wp", b"bloom-a")], &[]).unwrap(), CommitOutcome::Applied(1),);
    // Re-deliver the same key with a *different* claim: it must not apply.
    assert_eq!(
        store.commit("dup", b"second", &[], &[claim("wp-2", b"bloom-a")], &[]).unwrap(),
        CommitOutcome::Duplicate,
    );
    let journal = store.replay_journal().unwrap();
    assert_eq!(journal.len(), 1);
    assert_eq!(journal[0].event, b"first");
    // The duplicate's `wp-2` claim never applied — a fresh bloom can seal it.
    assert_eq!(store.claim_seal(b"fresh", &members(&["wp-2"])).unwrap(), SealOutcome::Sealed);
}

#[test]
fn commit_membership_conflict_rolls_back_journal_and_releases() {
    // A claimed workpiece already held by a *foreign* bloom (one this commit does
    // not release) aborts the whole commit: the journal append, the free claims,
    // and every release roll back too. Tripwire: a conflicted commit persists
    // nothing at all.
    let mut store = memory();
    assert_eq!(store.claim_seal(b"pred", &members(&["w-held"])).unwrap(), SealOutcome::Sealed);
    assert_eq!(store.claim_seal(b"third", &members(&["taken"])).unwrap(), SealOutcome::Sealed);
    // A supersede-shaped commit: release `pred`'s `w-held`, claim `free` (ok),
    // then claim `taken` — held by a *third* bloom it does not release → conflict,
    // whole rollback.
    assert_eq!(
        store
            .commit(
                "conflicted",
                b"event-bytes",
                &[claim("w-held", b"pred")],
                &[claim("free", b"succ"), claim("taken", b"succ")],
                &[OutboxPayload { topic: "t".to_owned(), payload: b"p".to_vec() }],
            )
            .unwrap(),
        CommitOutcome::Conflict("taken".to_owned()),
    );
    // Nothing persisted: no journal row, no outbox entry, `free` still claimable,
    // `taken` still held by the third bloom, and — the release rolled back too —
    // `pred` still holds `w-held`.
    assert!(store.replay_journal().unwrap().is_empty());
    assert!(store.drain_outbox(None).unwrap().is_empty());
    assert_eq!(store.claim_seal(b"probe", &members(&["free"])).unwrap(), SealOutcome::Sealed);
    assert_eq!(store.claim_seal(b"probe2", &members(&["taken"])).unwrap(), SealOutcome::Conflict("taken".to_owned()));
    assert_eq!(store.claim_seal(b"probe3", &members(&["w-held"])).unwrap(), SealOutcome::Conflict("w-held".to_owned()));
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

    let drained = store.drain_outbox(None).unwrap();
    assert_eq!(drained.len(), 2);
    assert_eq!(drained[0].sequence, 1);
    assert_eq!(drained[0].payload, b"r1");
    assert_eq!(drained[1].payload, b"r2");

    // Ack the first only — the second stays undelivered for republish.
    assert_eq!(store.ack_outbox(None, 1).unwrap(), 1);
    let remaining = store.drain_outbox(None).unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].sequence, 2);
}

#[test]
fn outbox_topic_scoped_drain_and_ack_are_independent() {
    // Two reactors share one outbox partitioned by topic (ADR-0149 §Outbox
    // consumption): draining and acking one topic must never touch the other's
    // rows, so disjoint reactors never race on the shared `delivered` flag.
    let mut store = memory();
    assert_eq!(store.enqueue_outbox("view_document", b"v1").unwrap(), 1);
    assert_eq!(store.enqueue_outbox("landing_receipt", b"r1").unwrap(), 2);
    assert_eq!(store.enqueue_outbox("view_document", b"v2").unwrap(), 3);

    // A topic-scoped drain sees only its own topic's entries, in sequence order.
    let views = store.drain_outbox(Some("view_document")).unwrap();
    assert_eq!(views.iter().map(|e| e.sequence).collect::<Vec<_>>(), vec![1, 3]);
    let receipts = store.drain_outbox(Some("landing_receipt")).unwrap();
    assert_eq!(receipts.iter().map(|e| e.sequence).collect::<Vec<_>>(), vec![2]);

    // Tripwire: acking `view_document` through its highest sequence (3) must not
    // mark the `landing_receipt` entry at sequence 2 delivered — a topic-blind
    // `sequence <= 3` ack would wrongly sweep it up.
    assert_eq!(store.ack_outbox(Some("view_document"), 3).unwrap(), 2);
    assert!(store.drain_outbox(Some("view_document")).unwrap().is_empty());
    let receipts_after = store.drain_outbox(Some("landing_receipt")).unwrap();
    assert_eq!(receipts_after.iter().map(|e| e.sequence).collect::<Vec<_>>(), vec![2], "the other topic is untouched");
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
    let outbox = store.drain_outbox(None).unwrap();
    assert_eq!(outbox.len(), 1);
    assert_eq!(outbox[0].topic, "landing_receipt");
    assert_eq!(outbox[0].payload, b"receipt-bytes");
    // A re-delivered seal event is deduped (inbox survived the restart).
    assert_eq!(store.append_event("seal-1", b"anything").unwrap(), AppendOutcome::Duplicate);
}

fn order(nonce: &str) -> OutstandingOrder {
    OutstandingOrder {
        nonce: nonce.to_owned(),
        bloom: vec![1; 32],
        workpiece: "wp-return".to_owned(),
        scope_revision: vec![2; 32],
        candidate: vec![5; 32],
        displayed_digest: vec![5; 32],
        stage: vec![9],
    }
}

#[test]
fn outstanding_order_records_looks_up_and_consumes() {
    // The evidence-intake registry contract (#3502): a recorded order resolves
    // by nonce, and consuming it makes a re-lookup miss — the consume-once
    // semantics a replayed upload's nonce is refused by.
    let mut store = memory();
    assert_eq!(store.record_order(&order("n-1")).unwrap(), RecordOutcome::Recorded);

    let found = store.lookup_order("n-1").unwrap().expect("a recorded order resolves by nonce");
    assert_eq!(found, order("n-1"));

    // Consume it: the row is removed, and a re-lookup misses.
    assert!(store.consume_order("n-1").unwrap(), "consuming a live order removes it");
    assert_eq!(store.lookup_order("n-1").unwrap(), None, "a consumed order no longer resolves");
    // Tripwire: consuming an already-consumed nonce reports no removal — the
    // replay-refused signal the broker gates on.
    assert!(!store.consume_order("n-1").unwrap(), "an already-consumed nonce removes nothing");
}

#[test]
fn recording_a_replayed_nonce_is_an_idempotent_no_op() {
    // A second record of the same nonce must not overwrite or duplicate the row.
    let mut store = memory();
    assert_eq!(store.record_order(&order("dup")).unwrap(), RecordOutcome::Recorded);
    let mut second = order("dup");
    second.workpiece = "different".to_owned();
    assert_eq!(store.record_order(&second).unwrap(), RecordOutcome::Duplicate);
    // The original row is untouched (the duplicate's `workpiece` never applied).
    assert_eq!(store.lookup_order("dup").unwrap().unwrap().workpiece, "wp-return");
}

#[test]
fn list_outstanding_nonces_reflects_recorded_and_consumed_orders() {
    // The restart recovery set (#3641): every nonce still outstanding, so the
    // executor reactor's init can re-track a dispatched-but-unresolved order
    // after a crash instead of starting with an empty `tracked` vec.
    let mut store = memory();
    assert_eq!(store.list_outstanding_nonces().unwrap(), Vec::<String>::new());

    assert_eq!(store.record_order(&order("n-1")).unwrap(), RecordOutcome::Recorded);
    assert_eq!(store.record_order(&order("n-2")).unwrap(), RecordOutcome::Recorded);
    let mut nonces = store.list_outstanding_nonces().unwrap();
    nonces.sort();
    assert_eq!(nonces, vec!["n-1".to_owned(), "n-2".to_owned()]);

    // Consuming one drops it from the enumeration; the other stays outstanding.
    assert!(store.consume_order("n-1").unwrap());
    assert_eq!(store.list_outstanding_nonces().unwrap(), vec!["n-2".to_owned()]);
}

#[test]
fn outstanding_order_survives_a_restart() {
    // Evidence returns after an arbitrary delay, so an order must survive a
    // `kill -9` + restart to stay matchable — the reason the registry is the
    // persisted store rather than an in-memory map. Model the restart with a
    // temp-file DB dropped and reopened.
    let dir = tempfile::tempdir().unwrap();
    let path = temp_db(&dir);
    {
        let mut store = SqliteStore::open(&path).unwrap();
        assert_eq!(store.record_order(&order("survivor")).unwrap(), RecordOutcome::Recorded);
    }
    let mut store = SqliteStore::open(&path).unwrap();
    assert_eq!(store.lookup_order("survivor").unwrap().expect("a recorded order survives restart"), order("survivor"));
}

fn temp_db(dir: &tempfile::TempDir) -> String {
    dir.path().join("bloomery.db").to_str().unwrap().to_owned()
}

#[test]
fn crash_mid_seal_batch_leaves_no_torn_membership() {
    // The claim_seal boundary. A multi-member seal whose *last* member conflicts
    // must persist nothing — and that atomicity has to survive a crash+restart,
    // not just an in-memory rollback. Model the crash by dropping the connection
    // right after the failed seal and reopening the file: if claim_seal ever
    // regressed to committing per member instead of once, `free-a` / `free-b`
    // would be torn-committed and survive the reopen.
    // Tripwire: after restart the free members are still claimable.
    let dir = tempfile::tempdir().unwrap();
    let path = temp_db(&dir);
    {
        let mut store = SqliteStore::open(&path).unwrap();
        store.claim_seal(b"held", &members(&["wp-last"])).unwrap();
        // `free-a` and `free-b` insert cleanly; `wp-last` conflicts → the whole
        // transaction rolls back before commit.
        assert_eq!(
            store.claim_seal(b"batch", &members(&["free-a", "free-b", "wp-last"])).unwrap(),
            SealOutcome::Conflict("wp-last".to_owned()),
        );
    }
    // Restart against the same file: the partial inserts must not have survived.
    let mut store = SqliteStore::open(&path).unwrap();
    assert_eq!(store.claim_seal(b"fresh", &members(&["free-a", "free-b"])).unwrap(), SealOutcome::Sealed);
}

#[test]
fn crash_mid_supersede_leaves_predecessor_and_third_intact() {
    // The supersede boundary. supersede is DELETE(predecessor) + INSERT(successor
    // members) in one transaction; a successor member colliding with a third
    // bloom must roll the DELETE back too, durably. Model the crash by dropping
    // after the failed supersede and reopening: a partially-applied supersede
    // (DELETE committed, INSERTs not) would strand the predecessor's workpiece
    // unclaimed after restart.
    // Tripwire: after restart the predecessor still holds w1 and the third holds w9.
    let dir = tempfile::tempdir().unwrap();
    let path = temp_db(&dir);
    {
        let mut store = SqliteStore::open(&path).unwrap();
        store.claim_seal(b"pred", &members(&["w1"])).unwrap();
        store.claim_seal(b"third", &members(&["w9"])).unwrap();
        // succ re-admits w1 (freed by the DELETE) then wants w9 → conflict, whole
        // rollback restoring pred's claim on w1.
        assert_eq!(
            store.supersede(b"pred", b"succ", &members(&["w1", "w9"])).unwrap(),
            SealOutcome::Conflict("w9".to_owned()),
        );
    }
    let mut store = SqliteStore::open(&path).unwrap();
    // Predecessor's claim survived (the DELETE rolled back durably)...
    assert_eq!(store.claim_seal(b"probe1", &members(&["w1"])).unwrap(), SealOutcome::Conflict("w1".to_owned()));
    // ...and the third bloom still holds w9.
    assert_eq!(store.claim_seal(b"probe2", &members(&["w9"])).unwrap(), SealOutcome::Conflict("w9".to_owned()));
}

#[test]
fn crash_between_enqueue_and_ack_preserves_the_entry_for_republish() {
    // The outbox boundary. A crash after enqueue but before ack must leave the
    // entry drainable so restart republishes it; a crash after ack must leave it
    // gone. Model each point with a drop+reopen — this is what proves an outbox
    // insert commits durably on its own and an ack is not lost.
    // Tripwire: an un-acked entry survives the restart, an acked one does not.
    let dir = tempfile::tempdir().unwrap();
    let path = temp_db(&dir);

    // Crash after enqueue, before ack.
    {
        let mut store = SqliteStore::open(&path).unwrap();
        store.enqueue_outbox("receipt", b"r1").unwrap();
    }
    let mut store = SqliteStore::open(&path).unwrap();
    let drained = store.drain_outbox(None).unwrap();
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].payload, b"r1");

    // Ack, then crash: the acked entry must not come back on restart.
    store.ack_outbox(None, drained[0].sequence).unwrap();
    drop(store);
    let mut store_after_ack = SqliteStore::open(&path).unwrap();
    assert!(store_after_ack.drain_outbox(None).unwrap().is_empty());
}

#[test]
fn dispatch_description_records_looks_up_and_is_key_scoped() {
    // The #3595 dispatch-description projection: the operator's work-order text
    // the coordinator persists at seal must read back verbatim for its
    // (bloom, workpiece) key, an absent key misses (so the reactor leaves the
    // transformation `None` rather than dispatching blind), and last-writer-wins
    // overwrites in place.
    let mut store = memory();
    let bloom = [0xB1; 32];
    store.record_dispatch_description(&bloom, "wp-a", "thread the work order into the prompt").unwrap();

    assert_eq!(
        store.lookup_dispatch_description(&bloom, "wp-a").unwrap().as_deref(),
        Some("thread the work order into the prompt"),
        "a persisted description reads back verbatim for its member key",
    );
    // A different workpiece under the same bloom, and a different bloom, both miss.
    assert_eq!(store.lookup_dispatch_description(&bloom, "wp-b").unwrap(), None, "an absent member has no description");
    assert_eq!(
        store.lookup_dispatch_description(&[0xB2; 32], "wp-a").unwrap(),
        None,
        "the key is scoped to the bloom, not the workpiece alone",
    );

    // Last-writer-wins on the key — a re-seal of the same member overwrites.
    store.record_dispatch_description(&bloom, "wp-a", "revised work order").unwrap();
    assert_eq!(store.lookup_dispatch_description(&bloom, "wp-a").unwrap().as_deref(), Some("revised work order"));
}
