//! Schema, transition, and authority tests for the ADR store.

use std::collections::BTreeMap;

use aether_bloomery::{
    ADR_SCHEMA, ADR_TRANSITION_SCHEMA, Adr, AdrStatus, AdrTransition, AuthorityDoor, Digest, Ed25519KeyProvider, KeyId,
    Observation, Provenance, SignatureEnvelope, Statement, authorization_message, digest_of,
};
use ed25519_dalek::{Signer, SigningKey};

use super::{AdrBackend, AdrError};
use crate::store::runtime::SqliteStore;

fn memory() -> SqliteStore {
    SqliteStore::open(":memory:").expect("in-memory store opens")
}

fn fixture(number: u32, title: &str) -> Adr {
    Adr {
        schema: ADR_SCHEMA,
        number,
        title: title.to_owned(),
        date: "2026-08-18".to_owned(),
        context: "context".to_owned(),
        decision: "decision".to_owned(),
        consequences: "consequences".to_owned(),
        alternatives: "alternatives".to_owned(),
    }
}

fn signing_key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

fn provider(signer: &str, key: &SigningKey) -> Ed25519KeyProvider {
    Ed25519KeyProvider::new(BTreeMap::from([(KeyId(signer.to_owned()), key.verifying_key())]))
}

fn signed_accept(signer: &str, key: &SigningKey, adr: Digest) -> Statement {
    let message = authorization_message(AuthorityDoor::Accept, adr, adr.as_bytes());
    Statement {
        words: adr.as_bytes().to_vec(),
        provenance: Provenance::AuthorSignature(SignatureEnvelope {
            signer: KeyId(signer.to_owned()),
            signature: key.sign(message.as_bytes()).to_bytes().to_vec(),
        }),
        parents: Vec::new(),
    }
}

fn accepted_digest(adr: Digest, citations: &[Digest]) -> Digest {
    digest_of(&AdrTransition {
        schema: ADR_TRANSITION_SCHEMA,
        adr,
        status: AdrStatus::Accepted,
        citations: citations.to_vec(),
        successor: None,
    })
}

#[test]
fn a_v8_store_gains_empty_adr_tables() {
    // Tripwire: version 9 is the ADR store. Opening a schema-8 file must
    // create the tables empty rather than skip them because user_version
    // was already "current" at 8.
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("v8.db");
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
             PRAGMA user_version = 8;",
        )
        .expect("plant v8 header");

    let mut store = SqliteStore::open(path).expect("a v8 store migrates");
    assert!(store.list().expect("list").is_empty(), "migration invents no ADRs");
    let flags: i64 = store.conn.query_row("PRAGMA user_version", [], |row| row.get(0)).expect("user_version");
    assert_eq!(flags, 10, "the open stamps the current schema");
}

#[test]
fn adrs_have_no_status_column() {
    // An unsigned status column would be authoritative for acceptance. Status
    // is the last transition; this query failing closed is the guard.
    let store = memory();
    let mut stmt = store.conn.prepare("PRAGMA table_info(adrs)").expect("pragma");
    let names: Vec<String> =
        stmt.query_map([], |row| row.get::<_, String>(1)).expect("rows").map(|row| row.expect("name")).collect();
    assert!(!names.iter().any(|name| name == "status"), "adrs must not carry a status column: {names:?}");
}

#[test]
fn a_proposed_record_registers_without_a_signature() {
    // Registration is observation, not authority. Writing a signature on
    // Proposed would let an unsigned path look ratified.
    let mut store = memory();
    let adr = fixture(201, "store objects");
    let digest = store.propose(&adr).expect("propose");
    let view = store.load(digest).expect("load").expect("exists");
    assert_eq!(view.status, AdrStatus::Proposed);
    assert_eq!(view.adr, adr);
    assert_eq!(digest_of(&view.adr), digest);
    assert!(view.citations.is_empty());
    let (status, signature): (String, Option<Vec<u8>>) = store
        .conn
        .query_row(
            "SELECT status, signature FROM adr_transitions WHERE adr = ?1",
            [digest.as_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("row");
    assert_eq!(status, "proposed");
    assert_eq!(signature, None);
}

#[test]
fn a_signed_provisional_row_fails_the_tier_check() {
    // The structural split: a provisional record must not carry a signature.
    // A convention would let a ratification hide as provisional.
    let mut store = memory();
    let digest = store.propose(&fixture(201, "t")).expect("propose");
    let planted = store.conn.execute(
        "INSERT INTO adr_transitions (digest, adr, status, canonical, statement, signature, successor)
         VALUES (x'01', ?1, 'provisional', x'aa', NULL, x'bb', NULL)",
        [digest.as_bytes().as_slice()],
    );
    assert!(planted.is_err(), "provisional + signature must fail the CHECK");
}

#[test]
fn an_accepted_row_without_a_signature_fails_the_tier_check() {
    let mut store = memory();
    let digest = store.propose(&fixture(201, "t")).expect("propose");
    let planted = store.conn.execute(
        "INSERT INTO adr_transitions (digest, adr, status, canonical, statement, signature, successor)
         VALUES (x'02', ?1, 'accepted', x'aa', x'cc', NULL, NULL)",
        [digest.as_bytes().as_slice()],
    );
    assert!(planted.is_err(), "accepted + NULL signature must fail the CHECK");
}

#[test]
fn acceptance_requires_a_provisional_tip_and_an_accept_signature() {
    // Skipping Provisional, or presenting an Approve envelope, would let
    // work proceed as ratified without the unsigned machine record or
    // the Accept door.
    let mut store = memory();
    let adr = fixture(201, "t");
    let digest = store.propose(&adr).expect("propose");
    let key = signing_key(7);
    let keys = provider("owner", &key);
    let statement = signed_accept("owner", &key, digest);
    assert_eq!(
        store.accept(digest, &statement, &[], &keys),
        Err(AdrError::WrongStatus { current: AdrStatus::Proposed }),
        "Accepted must not skip Provisional"
    );

    store.mark_provisional(digest).expect("provisional");
    let approve = {
        let message = authorization_message(AuthorityDoor::Approve, digest, digest.as_bytes());
        Statement {
            words: digest.as_bytes().to_vec(),
            provenance: Provenance::AuthorSignature(SignatureEnvelope {
                signer: KeyId("owner".to_owned()),
                signature: key.sign(message.as_bytes()).to_bytes().to_vec(),
            }),
            parents: Vec::new(),
        }
    };
    assert_eq!(store.accept(digest, &approve, &[], &keys), Err(AdrError::Unverified));

    let unsigned = Statement {
        words: digest.as_bytes().to_vec(),
        provenance: Provenance::ObservationAttestation(Observation { source: "test".to_owned() }),
        parents: Vec::new(),
    };
    assert_eq!(store.accept(digest, &unsigned, &[], &keys), Err(AdrError::WrongProvenance));

    assert_eq!(store.accept(digest, &statement, &[], &keys).expect("accept"), accepted_digest(digest, &[]));
    let view = store.load(digest).expect("load").expect("exists");
    assert_eq!(view.status, AdrStatus::Accepted);
    assert!(view.citations.is_empty(), "docs-only citations stay empty");
}

#[test]
fn empty_citations_are_stored_on_acceptance() {
    // The field exists from row one. Dropping empty citations on write would
    // make a docs-only ADR indistinguishable from a row that never grew the
    // field.
    let mut store = memory();
    let digest = store.propose(&fixture(201, "t")).expect("propose");
    store.mark_provisional(digest).expect("provisional");
    let key = signing_key(7);
    store.accept(digest, &signed_accept("owner", &key, digest), &[], &provider("owner", &key)).expect("accept");
    let canonical: Vec<u8> = store
        .conn
        .query_row(
            "SELECT canonical FROM adr_transitions WHERE adr = ?1 ORDER BY rowid DESC LIMIT 1",
            [digest.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .expect("row");
    let decoded = AdrTransition::from_canonical(&canonical).expect("decode");
    assert_eq!(decoded.status, AdrStatus::Accepted);
    assert!(decoded.citations.is_empty());
}

#[test]
fn cited_evidence_survives_reload() {
    // The accepted-after-implementation rule is queryable only if citations
    // round-trip. Dropping them on load would make the checklist uncheckable.
    let mut store = memory();
    let digest = store.propose(&fixture(201, "t")).expect("propose");
    store.mark_provisional(digest).expect("provisional");
    let key = signing_key(7);
    let evidence = Digest::from_bytes([9; 32]);
    store.accept(digest, &signed_accept("owner", &key, digest), &[evidence], &provider("owner", &key)).expect("accept");
    let view = store.load(digest).expect("load").expect("exists");
    assert_eq!(view.citations, [evidence]);
}

#[test]
fn supersession_names_the_successor_and_consumes_provisional() {
    // A successor-less supersede, or one that skips Provisional, would
    // leave the chain with an unsigned "this is done" and no replacement.
    let mut store = memory();
    let first = fixture(201, "old");
    let second = fixture(202, "new");
    let old = store.propose(&first).expect("propose old");
    let next = store.propose(&second).expect("propose new");
    assert_eq!(
        store.supersede(old, next),
        Err(AdrError::WrongStatus { current: AdrStatus::Proposed }),
        "supersede must consume Provisional"
    );
    store.mark_provisional(old).expect("provisional");
    assert_eq!(store.supersede(old, old), Err(AdrError::BadSuccessor));
    assert_eq!(store.supersede(old, Digest::from_bytes([1; 32])), Err(AdrError::BadSuccessor));
    store.supersede(old, next).expect("supersede");
    let view = store.load(old).expect("load").expect("exists");
    assert_eq!(view.status, AdrStatus::Superseded);
    assert_eq!(view.successor, Some(next));
    let rendered = view.adr.render(view.status, Some(second.number));
    assert!(rendered.contains("Superseded by ADR-0202"), "{rendered}");
}

#[test]
fn mutating_an_immutable_adr_row_is_refused() {
    // An unsigned UPDATE is not a supersession.
    let mut store = memory();
    let digest = store.propose(&fixture(201, "t")).expect("propose");
    let error = store
        .conn
        .execute("UPDATE adrs SET title = 'rewritten' WHERE digest = ?1", [digest.as_bytes().as_slice()])
        .expect_err("update must abort");
    assert!(error.to_string().contains("immutable"), "the trigger names the refusal: {error}");
}

#[test]
fn a_file_store_survives_reopen() {
    // Dropping the connection and opening the same file must recompute the
    // same digest from the same bytes, including an empty citation list.
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("adrs.db");
    let path = path.to_str().expect("utf-8 path");
    let adr = fixture(201, "t");
    let key = signing_key(7);
    let digest;
    {
        let mut store = SqliteStore::open(path).expect("create file store");
        digest = store.propose(&adr).expect("propose");
        store.mark_provisional(digest).expect("provisional");
        store.accept(digest, &signed_accept("owner", &key, digest), &[], &provider("owner", &key)).expect("accept");
    }
    let mut store = SqliteStore::open(path).expect("reopen");
    let view = store.load(digest).expect("load").expect("persisted");
    assert_eq!(view.status, AdrStatus::Accepted);
    assert_eq!(digest_of(&view.adr), digest);
    assert!(view.citations.is_empty());
    assert_eq!(store.load_by_number(201).expect("by number").expect("exists").digest, digest);
}

#[test]
fn a_duplicate_number_with_different_bytes_is_refused() {
    let mut store = memory();
    store.propose(&fixture(201, "one")).expect("first");
    assert_eq!(store.propose(&fixture(201, "two")), Err(AdrError::NumberTaken(201)));
    let again = fixture(201, "one");
    assert_eq!(store.propose(&again).expect("idempotent"), digest_of(&again));
}
