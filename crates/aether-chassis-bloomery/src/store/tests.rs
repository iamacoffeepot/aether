//! Contract + recovery tests for the `SQLite` store backend (ADR-0149).
//!
//! Each test drives the [`StoreBackend`] trait over a real [`SqliteStore`] —
//! an in-memory database for the pure-logic contracts, a temp-file database for
//! the reopen/recovery ones (so state survives dropping and reopening the
//! connection, the way a `kill -9` + restart does).

#![allow(clippy::unwrap_used)]

use super::runtime::{
    AppendOutcome, CommitOutcome, JournalWrite, OutstandingOrder, RecordOutcome, SealOutcome, SqliteStore, StoreBackend,
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

/// A decided journal row for the write-path calls — the ADR-0190 shape every
/// production write carries.
fn write<'a>(idempotency_key: &'a str, event: &'a [u8], decisions: &'a [u8]) -> JournalWrite<'a> {
    JournalWrite { idempotency_key, event, decisions, decider: "test-build" }
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
            &write("seal-1", b"sealed-bloom-event", b"decided"),
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
    assert_eq!(
        store.commit(&write("dup", b"first", b"decided"), &[], &[claim("wp", b"bloom-a")], &[]).unwrap(),
        CommitOutcome::Applied(1),
    );
    // Re-deliver the same key with a *different* claim: it must not apply.
    assert_eq!(
        store.commit(&write("dup", b"second", b"decided-2"), &[], &[claim("wp-2", b"bloom-a")], &[]).unwrap(),
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
                &write("conflicted", b"event-bytes", b"decided"),
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
    store.append_event(&write("land-1", &to_vec(&event).unwrap(), b"decided")).unwrap();

    let journal = store.replay_journal().unwrap();
    assert_eq!(journal.len(), 1);
    let decoded: Event = from_bytes(&journal[0].event).unwrap();
    assert_eq!(decoded, event);
}

#[test]
fn append_then_replay_round_trips_in_order() {
    let mut store = memory();
    assert_eq!(store.append_event(&write("k1", b"alpha", b"decided-1")).unwrap(), AppendOutcome::Applied(1));
    assert_eq!(store.append_event(&write("k2", b"beta", b"decided-2")).unwrap(), AppendOutcome::Applied(2));

    let journal = store.replay_journal().unwrap();
    assert_eq!(journal.len(), 2);
    assert_eq!(journal[0].idempotency_key, "k1");
    assert_eq!(journal[0].event, b"alpha");
    // The recorded decision rides the row back byte-identical (ADR-0190) — the
    // fold's input is exactly what admission stamped, decider included.
    assert_eq!(journal[0].decisions, b"decided-1");
    assert_eq!(journal[0].decider, "test-build");
    assert_eq!(journal[1].idempotency_key, "k2");
    assert_eq!(journal[1].event, b"beta");
    assert_eq!(journal[1].decisions, b"decided-2");
    assert_eq!(
        journal[0].decisions_schema.as_deref(),
        Some(aether_bloomery::DECISIONS_SCHEMA),
        "a write after the column exists stamps the writing schema"
    );
}

#[test]
fn an_unstamped_pre_adr_0190_row_refuses_replay_by_name() {
    // A journal row written before ADR-0190 carries no recorded decision.
    // Replaying it would re-decide history under the current reducer — the
    // exact rewrite #4937 documents — so the read refuses, naming the row and
    // the backfill obligation, instead of handing the fold a row it would have
    // to invent a decision for.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bloomery.db");
    let path = path.to_str().unwrap();
    let mut store = SqliteStore::open(path).unwrap();

    // Write the legacy shape through a side connection — the store's own write
    // paths always stamp, so the pre-migration row must be planted raw.
    rusqlite::Connection::open(path)
        .unwrap()
        .execute("INSERT INTO journal (idempotency_key, event) VALUES ('legacy', x'01')", [])
        .unwrap();

    let error = store.replay_journal().unwrap_err().to_string();
    assert!(error.contains("predates ADR-0190"), "the refusal names the missing record: {error}");
    assert!(error.contains("Backfill"), "the refusal states the obligation: {error}");
}

#[test]
fn a_mismatched_decisions_schema_refuses_replay_by_name() {
    // ADR-0187: a row stamped with a writing schema this binary has no upcast
    // for must refuse at fold, naming both identities — never decode as
    // current and never invent an empty decision.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mismatch.db");
    let path = path.to_str().unwrap();
    let mut store = SqliteStore::open(path).unwrap();
    store.append_event(&write("shaped", b"event", b"decided")).unwrap();
    rusqlite::Connection::open(path)
        .unwrap()
        .execute(
            "UPDATE journal SET decisions_schema = 'aether.bloomery.decisions.v0' WHERE idempotency_key = 'shaped'",
            [],
        )
        .unwrap();

    let mut store = SqliteStore::open(path).unwrap();
    let record = store.replay_journal().unwrap().pop().expect("the row is still readable");
    assert_eq!(record.decisions_schema.as_deref(), Some("aether.bloomery.decisions.v0"));
    let error = aether_bloomery::decode_recorded_decisions(&record.decisions, record.decisions_schema.as_deref())
        .unwrap_err()
        .to_string();
    assert!(error.contains("no migration from schema `aether.bloomery.decisions.v0`"), "{error}");
    assert!(error.contains(aether_bloomery::DECISIONS_SCHEMA), "{error}");
}

#[test]
fn a_v2_decided_row_is_stamped_with_the_current_schema_on_open() {
    // ADR-0187 bootstrap: rows written before the column carry an implicit
    // current-at-migration identity and are stamped as such, so a later
    // reshape can still name what wrote them.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v2.db");
    let path = path.to_str().unwrap();
    rusqlite::Connection::open(path)
        .unwrap()
        .execute_batch(
            "CREATE TABLE journal (
                 sequence        INTEGER PRIMARY KEY AUTOINCREMENT,
                 idempotency_key TEXT NOT NULL UNIQUE,
                 event           BLOB NOT NULL,
                 decisions       BLOB,
                 decider         TEXT
             );
             INSERT INTO journal (idempotency_key, event, decisions, decider)
             VALUES ('legacy', x'01', x'02', 'old-build');
             PRAGMA user_version = 2;",
        )
        .unwrap();

    let mut store = SqliteStore::open(path).expect("a v2 store migrates");
    let journal = store.replay_journal().unwrap();
    assert_eq!(journal.len(), 1);
    assert_eq!(journal[0].decisions, b"\x02");
    assert_eq!(
        journal[0].decisions_schema.as_deref(),
        Some(aether_bloomery::DECISIONS_SCHEMA),
        "the migration stamps the implicit current identity"
    );
}

#[test]
fn inbox_dedup_drops_a_duplicate_idempotency_key() {
    let mut store = memory();
    assert_eq!(store.append_event(&write("dup", b"first", b"decided")).unwrap(), AppendOutcome::Applied(1));
    // A replayed key is a no-op — the second delivery of the same event does not
    // re-append, and the original bytes are untouched.
    assert_eq!(store.append_event(&write("dup", b"second", b"decided-2")).unwrap(), AppendOutcome::Duplicate);

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
        store.append_event(&write("seal-1", b"sealed-bloom-event", b"decided")).unwrap();
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
    assert_eq!(store.append_event(&write("seal-1", b"anything", b"decided-2")).unwrap(), AppendOutcome::Duplicate);
}

/// A fixed Unix-millisecond reading the deadline fixtures sit around, so an
/// expiry assertion never depends on when the suite ran.
const NOW_UNIX_MILLIS: u64 = 1_700_000_000_000;

fn order(nonce: &str) -> OutstandingOrder {
    order_due_at(nonce, NOW_UNIX_MILLIS + 60_000)
}

fn order_due_at(nonce: &str, deadline_unix_millis: u64) -> OutstandingOrder {
    OutstandingOrder {
        profile: Vec::new(),
        nonce: nonce.to_owned(),
        bloom: vec![1; 32],
        workpiece: "wp-return".to_owned(),
        scope_revision: vec![2; 32],
        candidate: vec![5; 32],
        displayed_digest: vec![5; 32],
        stage: vec![9],
        transformation: vec![7, 7],
        configs: vec![3, 3],
        deadline_unix_millis,
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
fn a_consumed_order_still_names_the_bloom_that_dispatched_it() {
    // Tripwire: the janitor's evidence retention is per bloom, and consume
    // deletes the outstanding row so a replayed nonce refuses. If that delete
    // also dropped the owner, every consumed evidence directory would look
    // ownerless and the retention window would not be enforceable.
    let mut store = memory();
    let recorded = order("n-1");
    store.record_order(&recorded).unwrap();
    store.consume_order("n-1").unwrap();

    assert_eq!(
        store.lookup_dispatch_owner("n-1").unwrap().as_deref(),
        Some(recorded.bloom.as_slice()),
        "consume must not drop the dispatch's owning bloom",
    );
    assert_eq!(store.lookup_dispatch_owner("never-dispatched").unwrap(), None);
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
fn a_re_recorded_nonce_keeps_the_deadline_its_first_record_computed() {
    // Tripwire: the ADR-0177 deadline is absolute and set once. A redrive that
    // re-recorded the same nonce with a fresh now-plus-limit would renew the
    // allowance of an order already in flight, which is the property that let a
    // hung order outlive every restart in the first place.
    let mut store = memory();
    let first = order_due_at("dup", NOW_UNIX_MILLIS + 1_000);
    store.record_order(&first).unwrap();

    store.record_order(&order_due_at("dup", NOW_UNIX_MILLIS + 999_000)).unwrap();

    assert_eq!(store.lookup_order("dup").unwrap().unwrap().deadline_unix_millis, NOW_UNIX_MILLIS + 1_000);
}

#[test]
fn expired_orders_are_the_ones_whose_persisted_deadline_has_arrived() {
    // The selection the executor reactor terminates on (ADR-0177). At-or-before
    // `now`, so an order due exactly on the boundary is expired — the completion
    // pull that runs before the sweep is what gives evidence arriving at that
    // same instant the right of way.
    let mut store = memory();
    store.record_order(&order_due_at("future", NOW_UNIX_MILLIS + 1)).unwrap();
    store.record_order(&order_due_at("boundary", NOW_UNIX_MILLIS)).unwrap();
    store.record_order(&order_due_at("past", NOW_UNIX_MILLIS - 60_000)).unwrap();

    let expired = store.list_expired_orders(NOW_UNIX_MILLIS).unwrap();

    let nonces: Vec<&str> = expired.iter().map(|order| order.nonce.as_str()).collect();
    assert_eq!(nonces, vec!["boundary", "past"], "the future order is not selected, and the boundary one is");
    assert!(store.list_expired_orders(NOW_UNIX_MILLIS - 60_001).unwrap().is_empty(), "nothing is due before them all");
}

#[test]
fn an_empty_legacy_store_migrates_and_one_holding_orders_is_refused() {
    // ADR-0177's coordinated pre-1.0 break, from both sides. A legacy row has
    // neither a dispatch deadline nor decodable `transformation` bytes (#4697
    // re-typed `Transformation.limits`), and inventing either would attest a
    // limit no bloom sealed — so the store refuses rather than migrating a lie.
    // An empty legacy store has nothing to lie about and migrates mechanically.
    let dir = tempfile::tempdir().unwrap();
    let empty = temp_db(&dir);
    write_legacy_schema(&empty, false);
    let mut migrated = SqliteStore::open(&empty).expect("an empty legacy store migrates");
    assert_eq!(migrated.record_order(&order("n-1")).unwrap(), RecordOutcome::Recorded);

    let held = dir.path().join("legacy-with-orders.db").to_str().unwrap().to_owned();
    write_legacy_schema(&held, true);
    let refusal = match SqliteStore::open(&held) {
        Err(error) => error.to_string(),
        Ok(_) => panic!("a legacy store still holding orders must be refused"),
    };
    assert!(refusal.contains("1 outstanding order(s)"), "the refusal names the rows that block it: {refusal}");
    assert!(refusal.contains("ADR-0177"), "and the decision that requires it: {refusal}");
}

#[test]
fn a_half_migrated_legacy_store_finishes_the_job_rather_than_being_stamped_current() {
    // The shape a non-atomic migration leaves behind. Two `ALTER`s under separate
    // autocommits, and a fault or a crash between them migrates
    // `outstanding_orders` while `parked_question` stays legacy — with the
    // version unstamped, because the fault propagated. The next open then reads
    // the first table as proof the whole step was done, skips it, and stamps the
    // version anyway: permanently "current" with the ADR-0151 park column
    // missing, and nothing left that will ever repair it.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("half-migrated.db").to_str().unwrap().to_owned();
    write_legacy_schema(&path, false);
    let legacy = rusqlite::Connection::open(&path).unwrap();
    legacy
        .execute_batch("ALTER TABLE outstanding_orders ADD COLUMN deadline_unix_millis INTEGER NOT NULL DEFAULT 0;")
        .unwrap();
    drop(legacy);

    let mut store = SqliteStore::open(&path).expect("a half-migrated store opens and completes its migration");

    // Exercise the park path itself rather than re-reading `PRAGMA table_info` —
    // that path is what a missing column actually breaks, and its deadline is
    // what a replayed lane runs to.
    let parked = order_due_at("n-1", NOW_UNIX_MILLIS + 5_000);
    store.record_parked_question(b"question-bytes", &parked).unwrap();
    let replayed = store
        .lookup_parked_question(&parked.bloom, b"question-bytes")
        .unwrap()
        .expect("a parked order resolves by its question");
    assert_eq!(replayed.deadline_unix_millis, NOW_UNIX_MILLIS + 5_000, "the parked order kept its sealed deadline");
}

/// Write a pre-ADR-0177 store at `path`: the order-bearing tables without their
/// `deadline_unix_millis` column, at schema version zero, optionally holding one
/// outstanding order.
fn write_legacy_schema(path: &str, with_order: bool) {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch(
        "CREATE TABLE outstanding_orders (
             nonce            TEXT PRIMARY KEY,
             bloom            BLOB NOT NULL,
             workpiece        TEXT NOT NULL,
             scope_revision   BLOB NOT NULL,
             candidate        BLOB NOT NULL,
             displayed_digest BLOB NOT NULL,
             stage            BLOB NOT NULL,
             transformation   BLOB NOT NULL,
             configs          BLOB NOT NULL,
             profile          BLOB NOT NULL
         );
         CREATE TABLE parked_question (
             bloom            BLOB NOT NULL,
             question         BLOB NOT NULL,
             nonce            TEXT NOT NULL,
             workpiece        TEXT NOT NULL,
             scope_revision   BLOB NOT NULL,
             candidate        BLOB NOT NULL,
             displayed_digest BLOB NOT NULL,
             stage            BLOB NOT NULL,
             transformation   BLOB NOT NULL,
             configs          BLOB NOT NULL,
             profile          BLOB NOT NULL,
             PRIMARY KEY (bloom, question)
         );",
    )
    .unwrap();
    if with_order {
        conn.execute(
            "INSERT INTO outstanding_orders VALUES ('dispatch-11', x'01', 'issue-4626', x'02', x'03', x'03', x'04', \
             x'05', x'06', x'07')",
            [],
        )
        .unwrap();
    }
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

#[test]
fn a_members_candidate_commit_message_is_superseded_by_its_next_capture() {
    // The keying claim the landing assembly rests on. A member's only writer is
    // the lane that captures a candidate for it, so last-writer-wins on
    // (bloom, workpiece) is what makes the row *per candidate*: a Refine's fresh
    // capture supersedes the message of the candidate it replaces, and the row
    // the land path reads at the end is the resolving candidate's. A second row
    // per member instead would leave the land path choosing between a stale
    // message and a fresh one with nothing to tell them apart.
    let mut store = memory();
    let bloom = [0xB1; 32];
    store.record_candidate_commit_message(&bloom, "issue-11", "fix(crate:aether-fs): reject a traversal").unwrap();

    assert_eq!(
        store.lookup_candidate_commit_message(&bloom, "issue-11").unwrap().as_deref(),
        Some("fix(crate:aether-fs): reject a traversal"),
    );
    assert_eq!(store.lookup_candidate_commit_message(&bloom, "issue-12").unwrap(), None, "a sibling member is its own");
    assert_eq!(
        store.lookup_candidate_commit_message(&[0xB2; 32], "issue-11").unwrap(),
        None,
        "the key is scoped to the bloom, not the workpiece alone",
    );

    store.record_candidate_commit_message(&bloom, "issue-11", "fix(crate:aether-fs): reject every traversal").unwrap();
    assert_eq!(
        store.lookup_candidate_commit_message(&bloom, "issue-11").unwrap().as_deref(),
        Some("fix(crate:aether-fs): reject every traversal"),
        "the refined candidate's message supersedes the one it replaced",
    );
}

// The sealed-configuration resolution path (ADR-0174). A test kind stands in
// for a real configuration: the resolver is generic over `ConfigKind`, so what
// it resolves is the contract and which kind is incidental.
mod sealed_config {
    use aether_bloomery::{ConfigKind, ConfigRegistry, ConfigScopes, config_address};
    use aether_data::Kind;
    use aether_data::wire::to_vec;
    use serde::{Deserialize, Serialize};

    use super::memory;
    use aether_bloomery::ConfigResolveError;

    use crate::store::{StoreBackend, StoreConfigError, resolve_config};

    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[kind(name = "aether.bloomery.test_lane_config")]
    struct LaneConfig {
        lane: String,
    }

    /// Author `config` the way `POST /configs` does — encode, address, store —
    /// and hand back the address a registry would seal.
    fn author(store: &mut dyn StoreBackend, config: &LaneConfig) -> aether_bloomery::Digest {
        let bytes = to_vec(config).unwrap();
        let address = config.address();
        store.record_config(address.as_bytes(), LaneConfig::NAME, &bytes).unwrap();
        address
    }

    fn sealing(store: &mut dyn StoreBackend, lane: &str) -> ConfigRegistry {
        let address = author(store, &LaneConfig { lane: lane.to_owned() });
        let mut registry = ConfigRegistry::default();
        registry.insert::<LaneConfig>(address);
        registry
    }

    // The case a flat field on the sealed types cannot express: two members
    // configuring the *same kind* differently, each resolving its own. This is
    // the whole reason the registry is per-scope rather than per-bloom.
    #[test]
    fn two_members_resolve_their_own_config_of_one_kind() {
        let mut store = memory();
        let (first, second) = (sealing(&mut store, "cheap"), sealing(&mut store, "expensive"));
        let bloom = ConfigRegistry::default();

        let resolved = |store: &mut dyn StoreBackend, member: &ConfigRegistry| {
            resolve_config::<LaneConfig>(store, ConfigScopes::member_of(member, &bloom)).unwrap()
        };

        assert_eq!(resolved(&mut store, &first).unwrap().lane, "cheap");
        assert_eq!(resolved(&mut store, &second).unwrap().lane, "expensive");
    }

    // The scope chain resolves outward: a member without an entry takes the
    // bloom's, and a member with one takes its own over the bloom's.
    #[test]
    fn the_member_scope_shadows_the_bloom_scope() {
        let mut store = memory();
        let bloom = sealing(&mut store, "bloom-wide");
        let member = sealing(&mut store, "member-only");
        let bare = ConfigRegistry::default();

        let shadowed =
            resolve_config::<LaneConfig>(&mut store, ConfigScopes::member_of(&member, &bloom)).unwrap().unwrap();
        assert_eq!(shadowed.lane, "member-only");

        let inherited =
            resolve_config::<LaneConfig>(&mut store, ConfigScopes::member_of(&bare, &bloom)).unwrap().unwrap();
        assert_eq!(inherited.lane, "bloom-wide", "a member sealing nothing takes the bloom's");
    }

    // Nothing sealed anywhere is `None`, which the caller reads as "take the
    // calibrated default". Distinct from the two refusals below.
    #[test]
    fn an_unsealed_kind_resolves_to_nothing() {
        let mut store = memory();
        let empty = ConfigRegistry::default();
        assert!(resolve_config::<LaneConfig>(&mut store, ConfigScopes::bloom_wide(&empty)).unwrap().is_none());
    }

    // Tripwire: a sealed address with no stored row refuses. Falling through to
    // the default here would run one configuration while the receipt attests
    // another — the exact divergence the registry exists to close, so the
    // distinction between "unsealed" and "sealed but unresolvable" is the
    // load-bearing behaviour of this resolver.
    #[test]
    fn a_sealed_address_with_no_content_refuses_rather_than_defaulting() {
        let mut store = memory();
        let mut registry = ConfigRegistry::default();
        registry.insert::<LaneConfig>(LaneConfig { lane: "never authored".to_owned() }.address());

        let error = resolve_config::<LaneConfig>(&mut store, ConfigScopes::bloom_wide(&registry)).unwrap_err();
        assert!(matches!(error, StoreConfigError::Content(ConfigResolveError::Missing { .. })), "got {error:?}");
    }

    // Tripwire: the stored row's kind is checked against the registry key. The
    // address is domain-separated by kind name, so a mismatch means some path
    // wrote a row without addressing it that way — decoding whatever is at the
    // address would then hand the caller another kind's bytes.
    #[test]
    fn a_row_filed_under_another_kind_refuses() {
        let mut store = memory();
        let config = LaneConfig { lane: "cheap".to_owned() };
        let address = config.address();
        store.record_config(address.as_bytes(), "aether.bloomery.some_other_kind", &to_vec(&config).unwrap()).unwrap();

        let mut registry = ConfigRegistry::default();
        registry.insert::<LaneConfig>(address);

        let error = resolve_config::<LaneConfig>(&mut store, ConfigScopes::bloom_wide(&registry)).unwrap_err();
        assert!(matches!(error, StoreConfigError::Content(ConfigResolveError::KindMismatch { .. })), "got {error:?}");
    }

    // Tripwire: the address the typed path computes is the address the generic
    // authoring route computes from a kind name plus canonical bytes. If these
    // diverged, a configuration authored over `POST /configs` would seal at an
    // address no typed resolution could reach — and the failure would surface as
    // a missing row at dispatch, far from its cause.
    #[test]
    fn the_route_and_the_typed_path_address_a_config_identically() {
        let config = LaneConfig { lane: "cheap".to_owned() };
        assert_eq!(config.address(), config_address(LaneConfig::NAME, &to_vec(&config).unwrap()));
    }
}

// The 2026-08-14 boot-brick class: the live store holds vec-shape price-table
// bytes under their digests. Folding a journal that sealed one of those
// addresses must survive, and the decode is the named pre-migration posture
// — never a silent empty table, never a fatal abort (#4923).
mod pre_migration_price_table {
    use aether_bloomery::{
        BloomDraft, ConfigRegistry, ConfigScopes, Decisions, Digest, Event, Evidence, EvidenceKind, Fact, Forecast,
        IdempotencyKey, Membership, Outcome, PriceTable, ResolvedConfigs, SealedPriceTable, Snapshot, WorkpieceId,
        config_address,
    };
    use aether_data::Kind;
    use aether_data::wire::{from_bytes, to_vec};
    use serde::Serialize;

    use super::{memory, write};
    use crate::store::StoreBackend;

    #[derive(Serialize)]
    struct VecShapeRow {
        model: String,
        input: u64,
        cache_read: u64,
        cache_write_5m: u64,
        cache_write_1h: u64,
        cache_write: u64,
        output: u64,
    }

    #[derive(Serialize)]
    struct VecShapeTable {
        rows: Vec<VecShapeRow>,
    }

    #[test]
    fn a_journal_fold_over_a_pre_migration_price_table_survives_named() {
        // Tripwire: the shape change is visible to replay. A store holding
        // vec-shape price-table bytes, and a journal row whose seal names
        // that digest, must fold without aborting and must name the table
        // pre-migration rather than treating it as unsealed/empty.
        let mut store = memory();
        let vec_bytes = to_vec(&VecShapeTable {
            rows: vec![VecShapeRow {
                model: "claude-opus-5".to_owned(),
                input: 5_000_000,
                cache_read: 500_000,
                cache_write_5m: 6_250_000,
                cache_write_1h: 10_000_000,
                cache_write: 6_250_000,
                output: 25_000_000,
            }],
        })
        .expect("a pre-migration table wire-encodes");
        let address = config_address(PriceTable::NAME, &vec_bytes);
        store.record_config(address.as_bytes(), PriceTable::NAME, &vec_bytes).unwrap();

        let mut configs = ConfigRegistry::default();
        configs.insert::<PriceTable>(address);
        let mut member = Membership {
            workpiece: WorkpieceId("issue-4923".to_owned()),
            scope_revision: Digest::from_bytes([2; 32]),
            configs: ConfigRegistry::default(),
            approval: Evidence {
                subject: Digest::default(),
                kind: EvidenceKind::Approval,
                detail: Digest::from_bytes([3; 32]),
            },
        };
        member.approval.subject = member.subject();
        let spec =
            BloomDraft { proposals: vec![member], base: Digest::default(), configs, forecast: Forecast::default() }
                .seal();
        let bloom = spec.id();
        let event = Event { idempotency_key: IdempotencyKey("seal-pre-migration".to_owned()), fact: Fact::Seal(spec) };
        let decisions = Decisions { outcome: Outcome::Sealed(bloom), effects: Vec::new() };
        let event_bytes = to_vec(&event).expect("seal event encodes");
        let decision_bytes = to_vec(&decisions).expect("seal decisions encode");
        store.commit(&write("seal-pre-migration", &event_bytes, &decision_bytes), &[], &[], &[]).unwrap();

        let mut resolved = ResolvedConfigs::default();
        for record in store.load_configs().unwrap() {
            let digest = Digest::from_slice(&record.digest).expect("stored config digest is 32 bytes");
            resolved.insert(digest, record.kind, record.bytes);
        }
        let mut snapshot = Snapshot::default();
        for record in store.replay_journal().unwrap() {
            let event: Event = from_bytes(&record.event).expect("journaled event decodes");
            let decisions: Decisions = from_bytes(&record.decisions).expect("journaled decisions decode");
            snapshot = snapshot.apply(&event, &decisions, &resolved);
        }

        assert!(snapshot.blooms.contains_key(&bloom), "the fold rebuilt the sealed bloom");
        let spec = &snapshot.blooms.get(&bloom).expect("the bloom folded").spec;
        assert_eq!(
            PriceTable::sealed_in(ConfigScopes::bloom_wide(spec.configs()), &resolved),
            SealedPriceTable::PreMigration,
            "the fold names the sealed vec-shape table; it does not silently empty it",
        );
        assert_ne!(
            PriceTable::sealed_in(ConfigScopes::bloom_wide(spec.configs()), &resolved),
            SealedPriceTable::Current(PriceTable::default()),
        );
    }
}
