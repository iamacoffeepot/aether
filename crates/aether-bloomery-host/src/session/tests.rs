//! Contract tests for the session-pool backend (the executor session-reuse
//! pool). Each drives the [`SessionBackend`] trait over a real
//! [`SqliteSessionStore`] with an injected `now`, so the age + lease + eligibility
//! logic is exercised deterministically without a wall-clock sleep.
//!
//! The load-bearing test is `tree_hash_change_still_acquires`: the #3341 non-gate
//! regression tripwire. Every other gate (head-freshness #3422, age #3264,
//! context cap, lease state) is a `KEEP`-worthy tripwire on the eligibility
//! predicate — flipping any of them the wrong way admits a stale-cache resume or
//! defeats reuse.

#![allow(clippy::unwrap_used)]

use super::kinds::{SessionKey, SessionManifest};
use super::runtime::{SessionBackend, SqliteSessionStore};

const HOUR_SECS: u64 = 3600;
const LEASE_SECS: u64 = 900;
const CAP: u64 = 150_000;

fn store() -> SqliteSessionStore {
    SqliteSessionStore::open(":memory:", HOUR_SECS, LEASE_SECS, CAP).unwrap()
}

fn key() -> SessionKey {
    SessionKey { model: "claude-opus-4-8".to_owned(), effort: "high".to_owned(), task: "implement".to_owned() }
}

/// A manifest with the head hash, context size, and deposit time under test; the
/// receipt/parent and tree hash carry sensible audit defaults callers override.
fn manifest(head_hash: &str, context_tokens: u64, deposited_at: u64) -> SessionManifest {
    SessionManifest {
        parent_receipt: None,
        receipt: "receipt-1".to_owned(),
        head_hash: head_hash.to_owned(),
        context_tokens,
        workspace_tree_hash: "tree-1".to_owned(),
        read_files: vec!["CLAUDE.md".to_owned()],
        deposited_at,
    }
}

#[test]
fn eligible_session_acquires_and_leases() {
    let mut store = store();
    store.release(&key(), "digest-1", &manifest("head-A", 1000, 1000)).unwrap();

    let leased = store.acquire(&key(), "head-A", 1000).unwrap().expect("an eligible session leases");
    assert_eq!(leased.session_bytes, "digest-1");
    // The acquired session's own receipt is handed back as the resume's parent.
    assert_eq!(leased.parent_receipt, "receipt-1");
}

#[test]
fn second_acquire_while_leased_misses() {
    // The exclusive-lease guarantee: while a session is leased (until now +
    // LEASE_SECS), a concurrent acquire of the same key finds nothing.
    let mut store = store();
    store.release(&key(), "digest-1", &manifest("head-A", 1000, 1000)).unwrap();

    assert!(store.acquire(&key(), "head-A", 1000).unwrap().is_some());
    assert!(store.acquire(&key(), "head-A", 1000).unwrap().is_none(), "a leased session is exclusive");
}

#[test]
fn lease_past_ttl_is_reacquirable() {
    // Lazy expiry: a lease older than LEASE_SECS is treated as free, so a crashed
    // holder never wedges the key. Tripwire: an expired lease must re-acquire.
    let mut store = store();
    store.release(&key(), "digest-1", &manifest("head-A", 1000, 1000)).unwrap();

    // Lease at t=1000 → held until t=1900.
    assert!(store.acquire(&key(), "head-A", 1000).unwrap().is_some());
    // Still held just before expiry...
    assert!(store.acquire(&key(), "head-A", 1899).unwrap().is_none());
    // ...free at expiry (the age bound still passes: 2000 - 1000 < 3600).
    assert!(store.acquire(&key(), "head-A", 2000).unwrap().is_some(), "an expired lease re-acquires");
}

#[test]
fn past_cutoff_is_ineligible() {
    // The age bound (#3264): a session deposited longer ago than the cutoff is
    // retired even though its head still matches. Tripwire: an aged session misses.
    let mut store = store();
    store.release(&key(), "digest-1", &manifest("head-A", 1000, 1000)).unwrap();

    // now - deposited_at == 3600 == cutoff → ineligible (the boundary is exclusive).
    assert!(store.acquire(&key(), "head-A", 1000 + HOUR_SECS).unwrap().is_none(), "a past-cutoff session misses");
    // A hair inside the cutoff still acquires.
    assert!(store.acquire(&key(), "head-A", 1000 + HOUR_SECS - 1).unwrap().is_some());
}

#[test]
fn head_hash_drift_misses() {
    // #3422 head-freshness: the static prefix (`CLAUDE.md` + skill text) a resume
    // reuses without re-deriving. A head that moved since deposit is a real cache
    // miss. Tripwire: a drifted head must not acquire.
    let mut store = store();
    store.release(&key(), "digest-1", &manifest("head-A", 1000, 1000)).unwrap();

    assert!(store.acquire(&key(), "head-MOVED", 1000).unwrap().is_none(), "a drifted head misses");
    // The same head still acquires (proving only the drift, not the key, moved).
    assert!(store.acquire(&key(), "head-A", 1000).unwrap().is_some());
}

#[test]
fn over_context_cap_is_ineligible() {
    // A session whose terminal context exceeds the cap is retired (lowering the
    // cap must retire an existing entry). Tripwire: an over-cap session misses.
    let mut store = store();
    store.release(&key(), "digest-1", &manifest("head-A", CAP + 1, 1000)).unwrap();

    assert!(store.acquire(&key(), "head-A", 1000).unwrap().is_none(), "an over-cap session misses");
}

#[test]
fn at_context_cap_is_eligible() {
    // The cap boundary is inclusive — a session exactly at the cap still acquires.
    let mut store = store();
    store.release(&key(), "digest-1", &manifest("head-A", CAP, 1000)).unwrap();

    assert!(store.acquire(&key(), "head-A", 1000).unwrap().is_some());
}

#[test]
fn key_mismatch_misses() {
    // The pool identity is the full `{model, effort, task}` key — an effort flip
    // (which breaks the prompt cache, #3264) is a different pool entry.
    let mut store = store();
    store.release(&key(), "digest-1", &manifest("head-A", 1000, 1000)).unwrap();

    let other_effort = SessionKey { effort: "low".to_owned(), ..key() };
    assert!(store.acquire(&other_effort, "head-A", 1000).unwrap().is_none(), "a mismatched effort misses");
    let other_model = SessionKey { model: "claude-sonnet-5".to_owned(), ..key() };
    assert!(store.acquire(&other_model, "head-A", 1000).unwrap().is_none(), "a mismatched model misses");
    let other_task = SessionKey { task: "scope".to_owned(), ..key() };
    assert!(store.acquire(&other_task, "head-A", 1000).unwrap().is_none(), "a mismatched task misses");
}

#[test]
fn tree_hash_change_still_acquires() {
    // The #3341 non-gate — the load-bearing regression tripwire. A deposited
    // session whose `workspace_tree_hash` differs from any reference tree still
    // acquires so long as head/age/cap/lease all pass, because the workpiece tree
    // changing between attempts is the whole point of the construct/verify/refine
    // loop this pool accelerates. `acquire` carries no tree-hash input at all;
    // this asserts the reuse a belief-truth subtree gate would have defeated.
    // Tripwire: adding any `workspace_tree_hash` eligibility gate breaks this.
    let mut store = store();
    let mut deposited = manifest("head-A", 1000, 1000);
    deposited.workspace_tree_hash = "tree-CHANGED-since-last-attempt".to_owned();
    store.release(&key(), "digest-1", &deposited).unwrap();

    assert!(
        store.acquire(&key(), "head-A", 1000).unwrap().is_some(),
        "a changed workpiece tree must not retire a pooled session (#3341)"
    );
}

#[test]
fn release_chains_parent_receipt() {
    // Manifest chaining: a resumed `release` names the acquired session's receipt
    // as its `parent_receipt`, and the pool hands the new receipt to the next
    // resume — so a chain of resumes is provenance-linked (ADR-0149's fail-closed
    // closure across the resume). A temp-file DB + a second read connection lets
    // the test assert the stored `parent_receipt` column directly.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sessions.db");
    let path = path.to_str().unwrap();

    let mut store = SqliteSessionStore::open(path, HOUR_SECS, LEASE_SECS, CAP).unwrap();

    // Cold deposit: receipt R1, no parent.
    let mut cold = manifest("head-A", 1000, 1000);
    cold.receipt = "R1".to_owned();
    cold.parent_receipt = None;
    store.release(&key(), "digest-1", &cold).unwrap();

    // Resume: acquire hands back R1 as the resumed attempt's parent.
    let leased = store.acquire(&key(), "head-A", 1000).unwrap().unwrap();
    assert_eq!(leased.parent_receipt, "R1");

    // The resumed attempt releases R2, naming R1 as its parent.
    let mut resumed = manifest("head-A", 1200, 1001);
    resumed.receipt = "R2".to_owned();
    resumed.parent_receipt = Some("R1".to_owned());
    store.release(&key(), "digest-2", &resumed).unwrap();

    // The next resume now inherits R2 as its parent (the chain advanced).
    let next = store.acquire(&key(), "head-A", 1002).unwrap().unwrap();
    assert_eq!(next.parent_receipt, "R2");

    // And the stored parent link R2 → R1 persisted (provenance is durable).
    let conn = rusqlite::Connection::open(path).unwrap();
    let stored_parent: Option<String> =
        conn.query_row("SELECT parent_receipt FROM sessions LIMIT 1", [], |row| row.get(0)).unwrap();
    assert_eq!(stored_parent.as_deref(), Some("R1"));
}
