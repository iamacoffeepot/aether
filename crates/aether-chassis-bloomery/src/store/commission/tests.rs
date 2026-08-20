//! Schema, constraint, transaction, and restart tests for the commission store.

use std::collections::BTreeMap;

use aether_bloomery::{
    AuthorityDoor, CommissionProjection, CommissionStatus, Digest, Ed25519KeyProvider, FakeKeyProvider, KeyId,
    Observation, Provenance, SCOPE_REVISION_SCHEMA, ScopeRevision, ScopeRouting, SignatureEnvelope, Statement, Topic,
    WorkpieceId, authorization_message, digest_of,
};
use aether_data::wire::{from_bytes, to_vec};
use ed25519_dalek::{Signer, SigningKey};

use super::{CommissionBackend, CommissionError, RecordCommissionApproval, RecordCommissionApprovalResult};
use crate::bloomery::TopicOutbox;
use crate::store::runtime::{SqliteStore, StoreBackend, StoreCapabilityState};
use crate::store::{JournalWrite, now_unix_millis};

fn memory() -> SqliteStore {
    SqliteStore::open(":memory:").expect("in-memory store opens")
}

fn workpiece(id: &str) -> WorkpieceId {
    WorkpieceId(id.to_owned())
}

fn intent() -> Statement {
    Statement {
        words: b"ship the commission store".to_vec(),
        provenance: Provenance::ObservationAttestation(Observation { source: "test".to_owned() }),
        parents: Vec::new(),
    }
}

fn revision(id: &str, predecessor: Option<Digest>) -> ScopeRevision {
    ScopeRevision {
        schema: SCOPE_REVISION_SCHEMA,
        workpiece: workpiece(id),
        predecessor,
        problem: "problem".to_owned(),
        design: "design".to_owned(),
        plan: "plan".to_owned(),
        declared_surface: vec!["crates/aether-bloomery/**".to_owned()],
        dogfood_brief: "dogfood".to_owned(),
        routing: ScopeRouting { size: "M".to_owned(), model: "construct: test".to_owned() },
        dependencies: Vec::new(),
        description: "advisory".to_owned(),
        implements: Vec::new(),
    }
}

fn signing_key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

fn provider(signer: &str, key: &SigningKey) -> Ed25519KeyProvider {
    Ed25519KeyProvider::new(BTreeMap::from([(KeyId(signer.to_owned()), key.verifying_key())]))
}

fn signed_approval(signer: &str, key: &SigningKey, scope: Digest) -> Statement {
    let message = authorization_message(AuthorityDoor::Approve, scope, scope.as_bytes());
    Statement {
        words: scope.as_bytes().to_vec(),
        provenance: Provenance::AuthorSignature(SignatureEnvelope {
            signer: KeyId(signer.to_owned()),
            signature: key.sign(message.as_bytes()).to_bytes().to_vec(),
        }),
        parents: Vec::new(),
    }
}

fn auto_approval(scope: Digest) -> Statement {
    Statement {
        words: scope.as_bytes().to_vec(),
        provenance: Provenance::ObservationAttestation(Observation {
            source: "aether.bloomery.approve_gate:auto-tier".to_owned(),
        }),
        parents: vec![scope],
    }
}

fn cancel_of(intent: Digest) -> Statement {
    Statement {
        words: intent.as_bytes().to_vec(),
        provenance: Provenance::AuthorSignature(SignatureEnvelope {
            signer: KeyId("owner".to_owned()),
            signature: vec![1, 2, 3],
        }),
        parents: Vec::new(),
    }
}

fn seed(store: &mut SqliteStore, id: &str) -> Digest {
    store.create(&workpiece(id), &intent()).expect("create commission")
}

fn write(store: &mut SqliteStore, id: &str, predecessor: Option<Digest>) -> Digest {
    store.write_revision(&revision(id, predecessor)).expect("write revision")
}

#[test]
fn a_v6_store_gains_empty_commission_tables() {
    // Tripwire: version 7 is the commission store. Opening a schema-6 file
    // must create the four tables empty rather than skip them because
    // user_version was already "current" at 6.
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("v6.db");
    let path = path.to_str().expect("utf-8 path");
    rusqlite::Connection::open(path)
        .expect("legacy file")
        .execute_batch(
            "CREATE TABLE journal (
                 sequence        INTEGER PRIMARY KEY AUTOINCREMENT,
                 idempotency_key TEXT NOT NULL UNIQUE,
                 event           BLOB NOT NULL,
                 decisions       BLOB,
                 decider         TEXT,
                 decisions_schema TEXT
             );
             PRAGMA user_version = 6;",
        )
        .expect("plant v6 header");

    let mut store = SqliteStore::open(path).expect("a v6 store migrates");
    assert!(store.list(None).expect("list").is_empty(), "migration invents no commissions");
    let flags: i64 = store.conn.query_row("PRAGMA user_version", [], |row| row.get(0)).expect("user_version");
    assert_eq!(flags, 11, "the open stamps the current schema");
    assert!(
        store.load_projection(&workpiece("wp-1")).expect("load").is_none(),
        "migration invents no replica-issue numbers"
    );
}

#[test]
fn foreign_keys_are_on_and_existing_tables_still_write() {
    // Enabling PRAGMA foreign_keys is per-connection and would change DML for
    // every REFERENCES clause. This tree had none; turning the pragma on is a
    // deliberate decision for the commission tables, not a free property of
    // sharing SqliteStore::open. Existing tables must keep writing.
    let mut store = memory();
    let enabled: i64 = store.conn.query_row("PRAGMA foreign_keys", [], |row| row.get(0)).expect("pragma");
    assert_eq!(enabled, 1, "open turns foreign_keys on");

    let write = JournalWrite { idempotency_key: "k", event: b"e", decisions: b"d", decider: "test" };
    store.append_event(&write).expect("journal insert still works with the pragma on");

    seed(&mut store, "wp-1");
    let error = store
        .conn
        .execute(
            "INSERT INTO commission_approvals (digest, commission, scope_digest, tier, statement, signature)
             VALUES (x'00', 'wp-1', x'11', 'auto', x'22', NULL)",
            [],
        )
        .expect_err("an approval for a missing revision must fail the FK");
    assert!(error.to_string().contains("FOREIGN KEY"), "commission REFERENCES are enforced, not documentary: {error}");
}

#[test]
fn signed_and_auto_tier_checks_refuse_the_cross_labelled_row() {
    // A shared table plus a convention is an authority escalation: a signed
    // row stored as auto (or the reverse) would smuggle the wrong shape past
    // readers. The CHECK is the split.
    let mut store = memory();
    seed(&mut store, "wp-1");
    let digest = write(&mut store, "wp-1", None);

    let signed_as_auto = store.conn.execute(
        "INSERT INTO commission_approvals (digest, commission, scope_digest, tier, statement, signature)
         VALUES (x'01', 'wp-1', ?1, 'auto', x'aa', x'bb')",
        [digest.as_bytes().as_slice()],
    );
    assert!(signed_as_auto.is_err(), "auto + signature must fail the CHECK");

    let auto_as_signed = store.conn.execute(
        "INSERT INTO commission_approvals (digest, commission, scope_digest, tier, statement, signature)
         VALUES (x'02', 'wp-1', ?1, 'signed', x'aa', NULL)",
        [digest.as_bytes().as_slice()],
    );
    assert!(auto_as_signed.is_err(), "signed + NULL signature must fail the CHECK");

    store
        .conn
        .execute(
            "INSERT INTO commission_approvals (digest, commission, scope_digest, tier, statement, signature)
             VALUES (x'03', 'wp-1', ?1, 'auto', x'aa', NULL)",
            [digest.as_bytes().as_slice()],
        )
        .expect("auto without signature satisfies the CHECK");
    store
        .conn
        .execute(
            "INSERT INTO commission_approvals (digest, commission, scope_digest, tier, statement, signature)
             VALUES (x'04', 'wp-1', ?1, 'signed', x'aa', x'bb')",
            [digest.as_bytes().as_slice()],
        )
        .expect("signed with a signature satisfies the CHECK");
}

#[test]
fn a_revision_round_trips_the_implemented_adr_list() {
    // The trailing implements field is inside the canonical bytes. Dropping
    // it on write would let a signed commission forget which ADRs it named.
    let mut store = memory();
    seed(&mut store, "wp-1");
    let mut revision = revision("wp-1", None);
    revision.implements = vec![Digest::from_bytes([9; 32])];
    let digest = store.write_revision(&revision).expect("write");
    let loaded = store.load_revision(digest).expect("load").expect("row");
    assert_eq!(loaded.implements, revision.implements);
    assert_eq!(digest_of(&loaded), digest);
}

#[test]
fn write_revision_advances_current_in_one_transaction() {
    // The tip pointer is not a second source of truth: it moves in the same
    // transaction as the immutable row. A write that only inserted, or only
    // updated current, would let an approval bind a digest the commission
    // does not currently name.
    let mut store = memory();
    seed(&mut store, "wp-1");
    let first = write(&mut store, "wp-1", None);
    let view = store.load(&workpiece("wp-1")).expect("load").expect("created");
    assert_eq!(view.head.current_revision, Some(first));
    assert_eq!(view.head.current_ordinal, Some(1));
    assert_eq!(view.current.as_ref().map(|item| item.schema), Some(SCOPE_REVISION_SCHEMA));
    assert_eq!(digest_of(view.current.as_ref().expect("current")), first);

    let second = write(&mut store, "wp-1", Some(first));
    let view = store.load(&workpiece("wp-1")).expect("load").expect("created");
    assert_eq!(view.head.current_revision, Some(second));
    assert_eq!(view.head.current_ordinal, Some(2));
    assert_eq!(view.current.as_ref().and_then(|item| item.predecessor), Some(first));
}

#[test]
fn insert_approval_requires_the_referenced_revision_to_be_current() {
    // Approvals are bound to one revision. Inserting against a missing or
    // superseded digest would let a seal read a signature that no longer
    // names the tip.
    let mut store = memory();
    seed(&mut store, "wp-1");
    let first = write(&mut store, "wp-1", None);
    let key = signing_key(7);
    let keys = provider("owner", &key);
    let statement = signed_approval("owner", &key, first);
    assert_eq!(store.insert_approval(&statement, &keys).expect("approve current"), digest_of(&statement));
    assert_eq!(store.load_approvals(first).expect("approvals").len(), 1);

    let second = write(&mut store, "wp-1", Some(first));
    assert!(
        store.load_approvals(second).expect("new revision").is_empty(),
        "a new revision must not inherit the prior approval"
    );
    assert_eq!(
        store.insert_approval(&statement, &keys),
        Err(CommissionError::StaleRevision),
        "the first revision is no longer current"
    );

    let missing = signed_approval("owner", &key, Digest::from_bytes([9; 32]));
    assert_eq!(
        store.insert_approval(&missing, &keys),
        Err(CommissionError::MissingRevision),
        "words naming an unknown digest must refuse before insert"
    );
}

#[test]
fn malformed_canonical_bytes_are_refused_on_read() {
    // Index columns are not the truth. Garbage planted under a digest key
    // must not come back as a ScopeRevision.
    let mut store = memory();
    seed(&mut store, "wp-1");
    let digest = write(&mut store, "wp-1", None);
    store
        .conn
        .execute("UPDATE scope_revisions SET canonical = x'ffff' WHERE digest = ?1", [digest.as_bytes().as_slice()])
        .expect_err("the immutability trigger must refuse the UPDATE");

    store
        .conn
        .execute("DELETE FROM scope_revisions WHERE digest = ?1", [digest.as_bytes().as_slice()])
        .expect_err("the immutability trigger must refuse the DELETE");

    store.conn.pragma_update(None, "foreign_keys", "OFF").expect("disable to plant a raw row");
    store
        .conn
        .execute(
            "INSERT INTO scope_revisions (digest, commission, predecessor, ordinal, canonical)
             VALUES (x'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', 'wp-1', NULL, 9, x'ffff')",
            [],
        )
        .expect("raw plant");
    store.conn.pragma_update(None, "foreign_keys", "ON").expect("restore");
    let planted = Digest::from_bytes([0xaa; 32]);
    assert_eq!(
        store.load_revision(planted),
        Err(CommissionError::MalformedCanonical),
        "garbage canonical bytes must not decode as a revision"
    );
}

#[test]
fn a_first_revision_after_one_exists_is_an_ordinal_violation() {
    // Predecessor None means ordinal 1. Writing it against a commission that
    // already has a tip would skip or rewrite the chain.
    let mut store = memory();
    seed(&mut store, "wp-1");
    write(&mut store, "wp-1", None);
    let mut skipped = revision("wp-1", None);
    skipped.problem = "a different first revision".to_owned();
    assert_eq!(
        store.write_revision(&skipped),
        Err(CommissionError::OrdinalViolation { expected: 2 }),
        "a second first-revision must not land"
    );
    let view = store.load(&workpiece("wp-1")).expect("load").expect("created");
    assert_eq!(view.head.current_ordinal, Some(1), "the refused write must not advance current");
}

#[test]
fn mutating_an_immutable_row_is_refused() {
    // Scope revisions, statements, and approvals are append-only. An unsigned
    // UPDATE is not a supersession.
    let mut store = memory();
    seed(&mut store, "wp-1");
    let digest = write(&mut store, "wp-1", None);
    let error = store
        .conn
        .execute("UPDATE scope_revisions SET ordinal = 4 WHERE digest = ?1", [digest.as_bytes().as_slice()])
        .expect_err("update must abort");
    assert!(error.to_string().contains("immutable"), "the trigger names the refusal: {error}");
}

#[test]
fn a_file_store_survives_reopen() {
    // The repository is the durable home: dropping the connection and opening
    // the same file must recompute the same digests from the same bytes.
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("commissions.db");
    let path = path.to_str().expect("utf-8 path");
    let key = signing_key(7);
    let keys = provider("owner", &key);
    let first;
    {
        let mut store = SqliteStore::open(path).expect("create file store");
        seed(&mut store, "wp-1");
        first = write(&mut store, "wp-1", None);
        let statement = signed_approval("owner", &key, first);
        store.insert_approval(&statement, &keys).expect("approve");
    }
    let mut store = SqliteStore::open(path).expect("reopen");
    let view = store.load(&workpiece("wp-1")).expect("load").expect("persisted");
    assert_eq!(view.head.current_revision, Some(first));
    assert_eq!(digest_of(view.current.as_ref().expect("current")), first);
    assert_eq!(store.load_approvals(first).expect("approvals").len(), 1);
    assert_eq!(store.list(Some(CommissionStatus::Open)).expect("list").len(), 1);
}

#[test]
fn auto_approval_inserts_without_a_signature() {
    let mut store = memory();
    seed(&mut store, "wp-1");
    let scope = write(&mut store, "wp-1", None);
    let statement = auto_approval(scope);
    assert_eq!(store.insert_approval(&statement, &FakeKeyProvider).expect("auto"), digest_of(&statement));
    let (tier, signature): (String, Option<Vec<u8>>) = store
        .conn
        .query_row(
            "SELECT tier, signature FROM commission_approvals WHERE scope_digest = ?1",
            [scope.as_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("row");
    assert_eq!(tier, "auto");
    assert_eq!(signature, None);
}

#[test]
fn a_tampered_signed_approval_is_unverified() {
    let mut store = memory();
    seed(&mut store, "wp-1");
    let scope = write(&mut store, "wp-1", None);
    let key = signing_key(7);
    let keys = provider("owner", &key);
    let mut statement = signed_approval("owner", &key, scope);
    if let Provenance::AuthorSignature(envelope) = &mut statement.provenance {
        envelope.signature[0] ^= 0xff;
    }
    assert_eq!(store.insert_approval(&statement, &keys), Err(CommissionError::Unverified));
}

#[test]
fn write_revision_for_an_unknown_commission_is_refused() {
    let mut store = memory();
    assert_eq!(
        store.write_revision(&revision("missing", None)),
        Err(CommissionError::MissingCommission("missing".to_owned()))
    );
}

#[test]
fn journal_writes_still_stamp_the_envelope_after_the_commission_migration() {
    // The pragma and the new tables must not change the journal write path
    // the rest of the store tests rely on.
    let mut store = memory();
    let before = now_unix_millis();
    store
        .append_event(&JournalWrite { idempotency_key: "k", event: b"e", decisions: b"d", decider: "test" })
        .expect("append");
    let after = now_unix_millis();
    let stamp = store.journal_recorded_unix_millis().expect("stamps")[0].expect("stamped");
    assert!((before..=after).contains(&stamp), "journal stamp still writes");
}

#[test]
fn canonical_index_columns_match_the_decoded_revision() {
    // Duplicated SQL columns are indexes. After a write they must agree with
    // the decoded bytes, or a later reader could trust the wrong workpiece.
    let mut store = memory();
    seed(&mut store, "wp-1");
    let digest = write(&mut store, "wp-1", None);
    let (commission, predecessor, ordinal, canonical): (String, Option<Vec<u8>>, i64, Vec<u8>) = store
        .conn
        .query_row(
            "SELECT commission, predecessor, ordinal, canonical FROM scope_revisions WHERE digest = ?1",
            [digest.as_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("row");
    let decoded = ScopeRevision::from_canonical(&canonical).expect("decode");
    assert_eq!(digest_of(&decoded), digest);
    assert_eq!(decoded.workpiece.0, commission);
    assert_eq!(decoded.predecessor.map(|item| item.as_bytes().to_vec()), predecessor);
    assert_eq!(ordinal, 1);
    assert_eq!(canonical, to_vec(&decoded).expect("encode"));
}

#[test]
fn write_revision_on_a_cancelled_commission_is_not_open() {
    // Pre-fix, a cancelled commission still accepted a new revision (201,
    // tip advanced). Seal then misreported the closed door as a stale scope.
    // The write must refuse and leave the tip unmoved.
    let mut store = memory();
    let intent = seed(&mut store, "wp-1");
    let first = write(&mut store, "wp-1", None);
    store.cancel(&workpiece("wp-1"), &cancel_of(intent)).expect("cancel");
    let mut next = revision("wp-1", Some(first));
    next.problem = "after cancel".to_owned();
    assert_eq!(store.write_revision(&next), Err(CommissionError::NotOpen));
    let view = store.load(&workpiece("wp-1")).expect("load").expect("exists");
    assert_eq!(view.head.current_revision, Some(first), "the refused write must not advance current");
    assert_eq!(view.head.status, CommissionStatus::Cancelled);
}

#[test]
fn persist_approval_on_a_cancelled_commission_is_not_open() {
    // Approvals on a closed commission used to land because persist checked
    // only that the named scope was the tip.
    let mut store = memory();
    let intent = seed(&mut store, "wp-1");
    let scope = write(&mut store, "wp-1", None);
    store.cancel(&workpiece("wp-1"), &cancel_of(intent)).expect("cancel");
    assert_eq!(store.insert_approval(&auto_approval(scope), &FakeKeyProvider), Err(CommissionError::NotOpen));
    assert!(store.load_approvals(scope).expect("approvals").is_empty(), "the refused approval must not land");
}

#[test]
fn cancel_closes_an_open_commission_and_refuses_a_second_close() {
    // Cancel is a write, not a status flip: the statement is stored and the
    // row is closed in one transaction. A second cancel must not rewrite it.
    let mut store = memory();
    let intent = seed(&mut store, "wp-1");
    let statement = cancel_of(intent);
    assert!(store.cancel(&workpiece("wp-1"), &statement).is_ok());
    let view = store.load(&workpiece("wp-1")).expect("load").expect("exists");
    assert_eq!(view.head.status, CommissionStatus::Cancelled);
    assert_eq!(store.cancel(&workpiece("wp-1"), &statement), Err(CommissionError::NotOpen));
}

#[test]
fn create_enqueues_a_commission_projection_and_record_round_trips() {
    // The projector writes title/body only to a number stored here. A create
    // that skipped the outbox would leave the replica never opened; a persist
    // that adopted an unstored number would write a human-authored issue.
    let mut store = memory();
    seed(&mut store, "wp-1");
    let entries = store.drain_topic(Topic::Commission).expect("drain");
    assert_eq!(entries.len(), 1, "create enqueues one replica projection");
    let payload: CommissionProjection = from_bytes(&entries[0].payload).expect("payload");
    assert_eq!(payload.workpiece.0, "wp-1");
    assert_eq!(payload.status, "open");
    assert!(payload.recorded_issue.is_none(), "nothing has been created on GitHub yet");

    store.record_projection(&workpiece("wp-1"), 9).expect("persist");
    assert_eq!(store.load_projection(&workpiece("wp-1")).expect("load"), Some(9));
    write(&mut store, "wp-1", None);
    let after = store.drain_topic(Topic::Commission).expect("drain after scope");
    let latest: CommissionProjection = from_bytes(&after.last().expect("row").payload).expect("decode");
    assert_eq!(latest.recorded_issue, Some(9), "later writes snapshot the recorded number");
}

#[test]
fn drain_overlays_the_recorded_number_onto_a_frozen_payload() {
    // Two events enqueue before the first create persists. The later row's
    // payload still says recorded_issue=None. Drain must consult the store
    // row, or the projector treats search as authority and opens a sibling.
    let mut store = memory();
    seed(&mut store, "wp-1");
    write(&mut store, "wp-1", None);
    store.record_projection(&workpiece("wp-1"), 9).expect("persist");

    let entries = store.drain_topic(Topic::Commission).expect("drain");
    assert_eq!(entries.len(), 2, "create and scope each enqueued one row");
    for entry in &entries {
        let payload: CommissionProjection = from_bytes(&entry.payload).expect("payload");
        assert_eq!(payload.recorded_issue, Some(9), "drain overlays the store row onto a frozen snapshot");
    }
}

#[test]
fn cancel_refuses_words_that_are_not_the_intent() {
    let mut store = memory();
    seed(&mut store, "wp-1");
    let statement = Statement {
        words: vec![9; 32],
        provenance: Provenance::AuthorSignature(SignatureEnvelope {
            signer: KeyId("owner".to_owned()),
            signature: vec![1, 2, 3],
        }),
        parents: Vec::new(),
    };
    assert_eq!(store.cancel(&workpiece("wp-1"), &statement), Err(CommissionError::WrongSubject));
    assert_eq!(store.load(&workpiece("wp-1")).expect("load").expect("exists").head.status, CommissionStatus::Open);
}

#[test]
fn unresolved_dependencies_name_uncommissioned_ids_once() {
    // Approve is the freeze point: a dependency may be created after the
    // revision is written, but an id with no commission at all is a scope
    // defect. The store reports those ids so the live approve door can refuse
    // before a signature is spent.
    let mut store = memory();
    seed(&mut store, "wp-1");
    let mut missing = revision("wp-1", None);
    missing.dependencies = vec![workpiece("ghost"), workpiece("ghost"), workpiece("wp-2")];
    let digest = store.write_revision(&missing).expect("write");
    assert_eq!(
        store.unresolved_dependencies(digest).expect("probe"),
        vec![workpiece("ghost"), workpiece("wp-2")],
        "uncommissioned ids are reported once in declaration order"
    );

    store.create(&workpiece("wp-2"), &Statement { words: b"ship wp-2".to_vec(), ..intent() }).expect("create wp-2");
    let mut present = revision("wp-1", Some(digest));
    present.dependencies = vec![workpiece("wp-2")];
    let next = store.write_revision(&present).expect("write successor");
    assert!(
        store.unresolved_dependencies(next).expect("probe").is_empty(),
        "a created commission is not an unresolved dependency"
    );
}

#[test]
fn the_live_approve_door_refuses_uncommissioned_dependencies() {
    // persist_approval is shared with sealed-bloom reconstruction, so the
    // refusal lives on the mail handler. An open but existing dependency
    // must still record; closedness is a seal-time check.
    let mut store = memory();
    seed(&mut store, "wp-1");
    let mut missing = revision("wp-1", None);
    missing.dependencies = vec![workpiece("ghost")];
    let scope = store.write_revision(&missing).expect("write");
    let statement = auto_approval(scope);
    let mut state = StoreCapabilityState::new(store);
    match state.record_commission_approval(RecordCommissionApproval {
        id: "wp-1".to_owned(),
        statement: to_vec(&statement).expect("encode"),
    }) {
        RecordCommissionApprovalResult::Refused { error } => {
            assert!(error.contains("ghost"), "refusal names the missing workpiece: {error}");
        }
        other => panic!("uncommissioned dependency must be Refused, got {other:?}"),
    }

    let mut store = memory();
    store
        .create(&workpiece("wp-dep"), &Statement { words: b"ship wp-dep".to_vec(), ..intent() })
        .expect("create wp-dep");
    seed(&mut store, "wp-1");
    let mut open = revision("wp-1", None);
    open.dependencies = vec![workpiece("wp-dep")];
    let scope = store.write_revision(&open).expect("write");
    let statement = auto_approval(scope);
    let mut state = StoreCapabilityState::new(store);
    match state.record_commission_approval(RecordCommissionApproval {
        id: "wp-1".to_owned(),
        statement: to_vec(&statement).expect("encode"),
    }) {
        RecordCommissionApprovalResult::Ok { .. } => {}
        other => panic!("an existing open dependency must still record, got {other:?}"),
    }
}
