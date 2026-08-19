//! The executor session-reuse pool, cross-process: boot the `bloomery` bin and
//! drive the `aether.session.*` mail surface over RPC — cold `release` pools a
//! session, `acquire` leases it back with its parent receipt, a drifted head
//! misses (#3422), a changed workpiece tree does NOT miss (#3341), a resumed
//! `release` chains the receipt, and a mismatched key misses.
//!
//! This dials the bin over raw `WireFrame::Call` frames, the `FleetHarness`
//! pattern (a real process, a real socket) — the process-boundary complement to
//! the in-process backend contracts in `src/session/tests.rs`. The bin runs with
//! `AETHER_SESSION_LEASE_TTL_MINS=0` so a lease expires immediately: that makes
//! the re-acquire assertions deterministic over the wire without a wall-clock
//! sleep (the lazy-expiry reclaim path), while the exclusive-hold path is the
//! backend unit test's job (injectable `now`).

#![allow(clippy::unwrap_used)]
#![allow(
    clippy::disallowed_methods,
    reason = "cross-process wire fixtures address root caps by their rendered runtime name — the RPC Call surface under test"
)]

mod common;

use std::net::TcpStream;
use std::time::{SystemTime, UNIX_EPOCH};

use aether_chassis_bloomery::session::{Acquire, AcquireResult, Release, ReleaseResult, SessionKey, SessionManifest};
use aether_data::{Kind, MailboxId, mailbox_id_from_path};
use common::client::{call, connect_and_handshake};
use common::{Coordinator, free_port};
use serde::Serialize;

/// Fork the `bloomery` bin on `port` with an in-memory pool and a zero lease TTL
/// (immediate lazy-expiry reclaim, so re-acquire is deterministic), reaped when
/// the returned guard drops.
///
/// `AETHER_STORE_PATH` is pinned rather than left to its `":memory:"` default:
/// the default only holds when nothing in the ambient environment names a store,
/// and a run under a coordinator's environment inherits one — which is the live
/// journal, opened read-write by a test that assumes it owns an empty pool
/// (#4714).
fn spawn(port: u16) -> Coordinator {
    Coordinator::spawn(port, &[("AETHER_STORE_PATH", ":memory:"), ("AETHER_SESSION_LEASE_TTL_MINS", "0")])
}

fn session_mailbox() -> MailboxId {
    mailbox_id_from_path("aether.session")
}

fn call_session<Req, Reply>(stream: &mut TcpStream, cid: u64, request: &Req) -> Reply
where
    Req: Kind + Serialize,
    Reply: Kind,
{
    call(stream, cid, session_mailbox(), request)
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

fn key() -> SessionKey {
    SessionKey { model: "claude-opus-4-8".to_owned(), effort: "high".to_owned(), task: "implement".to_owned() }
}

#[test]
fn acquire_release_resume_over_rpc() {
    let port = free_port();
    let _coordinator = spawn(port);
    let mut stream = connect_and_handshake(port, "session-pool-test");

    let now = now_secs();

    // Cold release: pool a fresh session (no prior lease, no parent receipt).
    let cold = Release {
        key: key(),
        lease: None,
        session_bytes: "digest-1".to_owned(),
        manifest: SessionManifest {
            parent_receipt: None,
            receipt: "R1".to_owned(),
            head_hash: "HEAD-A".to_owned(),
            context_tokens: 1000,
            workspace_tree_hash: "tree-1".to_owned(),
            read_files: vec!["CLAUDE.md".to_owned()],
            deposited_at: now,
        },
    };
    assert_eq!(call_session::<_, ReleaseResult>(&mut stream, 1, &cold), ReleaseResult::Ok);

    // Acquire with the matching head → leased back with the transcript digest and
    // the acquired session's own receipt as the resume's parent.
    let leased: AcquireResult =
        call_session(&mut stream, 2, &Acquire { key: key(), current_head_hash: "HEAD-A".to_owned() });
    let AcquireResult::Leased { session_bytes, parent_receipt, lease, .. } = leased else {
        panic!("expected a leased session, got {leased:?}");
    };
    assert_eq!(session_bytes, "digest-1");
    assert_eq!(parent_receipt, "R1");

    // A drifted static-prefix head is a real cache miss (#3422) → None.
    let drifted: AcquireResult =
        call_session(&mut stream, 3, &Acquire { key: key(), current_head_hash: "HEAD-MOVED".to_owned() });
    assert_eq!(drifted, AcquireResult::None, "a drifted head must miss");

    // Resumed release: deposit the next attempt's session, naming R1 as parent
    // and carrying a CHANGED workpiece tree — the construct/verify/refine loop's
    // normal state, which must not retire the pooled session (#3341 non-gate).
    // The resume presents the lease it acquired — a warm release proves it still
    // holds the row before depositing over it (#3665).
    let resumed = Release {
        key: key(),
        lease: Some(lease),
        session_bytes: "digest-2".to_owned(),
        manifest: SessionManifest {
            parent_receipt: Some("R1".to_owned()),
            receipt: "R2".to_owned(),
            head_hash: "HEAD-A".to_owned(),
            context_tokens: 1200,
            workspace_tree_hash: "tree-2-CHANGED".to_owned(),
            read_files: vec!["CLAUDE.md".to_owned(), "src/session/runtime.rs".to_owned()],
            deposited_at: now,
        },
    };
    assert_eq!(call_session::<_, ReleaseResult>(&mut stream, 4, &resumed), ReleaseResult::Ok);

    // Re-acquire (the prior lease expired immediately, lease TTL 0): the changed
    // tree did NOT block reuse, and the chain advanced R1 → R2.
    let reacquired: AcquireResult =
        call_session(&mut stream, 5, &Acquire { key: key(), current_head_hash: "HEAD-A".to_owned() });
    match reacquired {
        AcquireResult::Leased { session_bytes, parent_receipt, .. } => {
            assert_eq!(session_bytes, "digest-2", "a changed workpiece tree must not retire the session (#3341)");
            assert_eq!(parent_receipt, "R2", "the resume chain advanced to the new receipt");
        }
        other => panic!("expected a re-leased session, got {other:?}"),
    }

    // A mismatched effort (a distinct prompt-cache identity, #3264) → None.
    let other_effort = SessionKey { effort: "low".to_owned(), ..key() };
    let mismatch: AcquireResult =
        call_session(&mut stream, 6, &Acquire { key: other_effort, current_head_hash: "HEAD-A".to_owned() });
    assert_eq!(mismatch, AcquireResult::None, "a mismatched effort must miss");
}
