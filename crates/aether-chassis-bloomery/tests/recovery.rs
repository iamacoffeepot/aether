//! The ADR-0149 Demo, cross-process: boot the `bloomery` bin against a temp
//! database, seal a synthetic single-workpiece bloom through `aether.store.*`
//! typed mail over RPC, `kill -9` the process, restart it against the same
//! database file, and prove journal replay + outbox republish converge to the
//! sealed state — and that a second overlapping seal loses cleanly.
//!
//! This dials the bin over raw `WireFrame::Call` frames, the `FleetHarness` pattern
//! (a real process, a real socket, a real SIGKILL) — the process-boundary
//! complement to the in-process reopen test in `src/store/tests.rs`.

#![allow(clippy::unwrap_used)]
#![allow(
    clippy::disallowed_methods,
    reason = "cross-process wire fixtures address root caps by their rendered runtime name — the RPC Call surface under test"
)]

mod common;

use std::fs;
use std::net::TcpStream;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use aether_bloomery::{
    AggregateVerifyError, AttemptCompletedError, BloomDraft, BloomId, ConfigRegistry, Decisions, Digest, Event,
    Evidence, EvidenceKind, Fact, IdempotencyKey, Membership, Outcome, ResolvedConfigs, Snapshot, SpendWindow,
    StageCatalog, StageId, Transformation, WorkpieceId, decode_recorded_decisions, reduce,
};
use aether_chassis_bloomery::store::{
    AppendEvent, AppendEventResult, ClaimSeal, ClaimSealResult, DrainOutbox, DrainOutboxResult, EnqueueOutbox,
    EnqueueOutboxResult, JournalWrite, OutstandingOrder, ReplayJournal, ReplayJournalResult, SqliteStore, StoreBackend,
};
use aether_data::wire::{from_bytes, to_vec};
use aether_data::{Kind, mailbox_id_from_path};
use common::client::{self, connect_and_handshake};
use common::{Coordinator, free_port};
use serde::Serialize;

/// Fork the `bloomery` bin against `db` on `port`, reaped when the returned
/// guard drops.
fn spawn(port: u16, db: &str) -> Coordinator {
    Coordinator::spawn(port, &[("AETHER_STORE_PATH", db)])
}

/// Issue one typed `Call` to the `aether.store` mailbox and decode its reply.
fn call<Req, Reply>(stream: &mut TcpStream, cid: u64, request: &Req) -> Reply
where
    Req: Kind + Serialize,
    Reply: Kind,
{
    client::call(stream, cid, mailbox_id_from_path("aether.store"), request)
}

#[test]
fn kill_and_restart_converges_over_rpc() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("bloomery.db");
    let db = db.to_str().unwrap();

    // First boot: seal a synthetic single-workpiece bloom through typed mail.
    let port = free_port();
    let coordinator = spawn(port, db);
    let mut stream = connect_and_handshake(port, "recovery-test");

    // A real, wire-encoded bloom-protocol event — the shape the host journals.
    // The control core replays this journal at boot and decodes each record as an
    // `Event`; a non-`Event` record trips its fail-fast boot-replay abort
    // (ADR-0063), so the synthetic seed must be a valid encoded event, not opaque
    // bytes. `Fact::Land` on an orphan bloom reduces to a clean rejection, so the
    // replay rebuilds without incident.
    let event = Event {
        idempotency_key: IdempotencyKey("seal-1".to_owned()),
        fact: Fact::Land { bloom: BloomId(Digest::from_bytes([7; 32])), new_head: Digest::from_bytes([9; 32]) },
    };
    let event_bytes = to_vec(&event).unwrap();
    // The row journals what the reducer decided about it (ADR-0190) — here the
    // clean rejection a land on an orphan bloom reduces to, so the restarted
    // core's fold consumes the key and changes nothing.
    let decisions = reduce(&Snapshot::default(), &event, &ResolvedConfigs::default(), &SpendWindow::default());
    let decision_bytes = to_vec(&decisions).unwrap();

    let append: AppendEventResult = call(
        &mut stream,
        1,
        &AppendEvent {
            idempotency_key: "seal-1".into(),
            event: event_bytes.clone(),
            decisions: decision_bytes.clone(),
            decider: "recovery-test".into(),
        },
    );
    assert_eq!(append, AppendEventResult::Applied { sequence: 1 });

    let seal: ClaimSealResult = call(&mut stream, 2, &ClaimSeal { bloom: vec![1; 32], members: vec!["wp".into()] });
    assert_eq!(seal, ClaimSealResult::Sealed);

    let enqueued: EnqueueOutboxResult =
        call(&mut stream, 3, &EnqueueOutbox { topic: "landing_receipt".into(), payload: b"receipt".to_vec() });
    assert_eq!(enqueued, EnqueueOutboxResult::Ok { sequence: 1 });

    // Crash: SIGKILL mid-service, after the committed transactions.
    drop(stream);
    coordinator.kill9();

    // Restart against the same database file.
    let port = free_port();
    let _coordinator = spawn(port, db);
    let mut stream = connect_and_handshake(port, "recovery-test");

    // Journal replay: the sealed event survived the crash.
    let replay: ReplayJournalResult = call(&mut stream, 1, &ReplayJournal);
    match replay {
        ReplayJournalResult::Ok { records } => {
            assert_eq!(records.len(), 1);
            assert_eq!(records[0].idempotency_key, "seal-1");
            assert_eq!(records[0].event, event_bytes);
            assert_eq!(records[0].decisions, decision_bytes, "the recorded decision survived the crash");
            assert_eq!(records[0].decider, "recovery-test");
        }
        ReplayJournalResult::Err { error } => panic!("journal replay failed: {error}"),
    }

    // The membership survived: a second overlapping seal loses cleanly.
    let seal_again: ClaimSealResult =
        call(&mut stream, 2, &ClaimSeal { bloom: vec![2; 32], members: vec!["wp".into()] });
    assert_eq!(seal_again, ClaimSealResult::Conflict { workpiece: "wp".into() });

    // Outbox republish: the undelivered landing receipt is still drainable.
    let drained: DrainOutboxResult = call(&mut stream, 3, &DrainOutbox { topic: None });
    match drained {
        DrainOutboxResult::Ok { entries } => {
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].topic, "landing_receipt");
            assert_eq!(entries[0].payload, b"receipt");
        }
        DrainOutboxResult::Err { error } => panic!("outbox drain failed: {error}"),
    }
}

/// A sealed single-workpiece bloom, the same shape `control_loop` admits — the
/// journal row a previous coordinator would have left, so restart replay has
/// an active bloom to restore.
fn sealed_bloom(key: &str, workpiece: &str) -> (Event, Decisions, BloomId) {
    let scope_revision = Digest::from_bytes([1; 32]);
    let mut member = Membership {
        workpiece: WorkpieceId(workpiece.to_owned()),
        scope_revision,
        configs: ConfigRegistry::default(),
        approval: Evidence {
            subject: Digest::default(),
            kind: EvidenceKind::Approval,
            detail: Digest::from_bytes([200; 32]),
        },
    };
    member.approval.subject = member.subject();
    let spec =
        BloomDraft { proposals: vec![member], base: Digest::from_bytes([0; 32]), ..BloomDraft::default() }.seal();
    let event = Event { idempotency_key: IdempotencyKey(key.to_owned()), fact: Fact::Seal(spec) };
    let decisions = reduce(&Snapshot::default(), &event, &ResolvedConfigs::default(), &SpendWindow::default());
    let Outcome::Sealed(bloom) = decisions.outcome else {
        panic!("fixture control: today's reducer must seal this draft: {decisions:?}");
    };
    (event, decisions, bloom)
}

/// The completed-while-down footprint a local-lane restart re-adopts: an
/// outstanding construct order plus the `evidence.json` the child wrote.
fn plant_completed_order(store: &mut SqliteStore, worktrees: &Path, bloom: BloomId, workpiece: &str, nonce: &str) {
    let subject = Digest::from_bytes([1; 32]);
    let transformation = Transformation::for_member_stage(
        &StageCatalog::binding_of(StageId::Construct),
        subject,
        Digest::from_bytes([0xC0; 32]),
        Digest::from_bytes([0xB0; 32]),
    );
    store
        .record_order(&OutstandingOrder {
            nonce: nonce.to_owned(),
            bloom: bloom.0.as_bytes().to_vec(),
            workpiece: workpiece.to_owned(),
            scope_revision: subject.as_bytes().to_vec(),
            candidate: subject.as_bytes().to_vec(),
            displayed_digest: subject.as_bytes().to_vec(),
            stage: to_vec(&StageId::Construct).unwrap(),
            transformation: to_vec(&transformation).unwrap(),
            configs: to_vec(&ConfigRegistry::default()).unwrap(),
            profile: to_vec(&StageCatalog::profile_of(StageId::Construct)).unwrap(),
            deadline_unix_millis: u64::MAX / 2,
        })
        .unwrap();

    let evidence_dir = worktrees.join(format!("{nonce}-evidence"));
    fs::create_dir_all(&evidence_dir).unwrap();
    fs::write(
        evidence_dir.join("evidence.json"),
        format!(
            r#"{{"command":"construct.implement","nonce":"{nonce}","produced_candidate":true,"result_record":{{"schema":1,"is_error":false,"result":{{"num_turns":3}}}}}}"#
        ),
    )
    .unwrap();
}

/// Poll the store journal until a re-adopted attempt has been committed, or
/// the deadline expires.
fn wait_for_attempt(stream: &mut TcpStream, cid_base: u64) -> (Event, Decisions) {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut cid = cid_base;
    loop {
        let replay: ReplayJournalResult = call(stream, cid, &ReplayJournal);
        let records = match replay {
            ReplayJournalResult::Ok { records } => records,
            ReplayJournalResult::Err { error } => panic!("journal replay failed: {error}"),
        };
        for record in &records {
            let decisions = decode_recorded_decisions(&record.decisions, record.decisions_schema.as_deref())
                .unwrap_or_else(|error| panic!("record {} did not decode: {error}", record.idempotency_key));
            assert!(
                !matches!(
                    decisions.outcome,
                    Outcome::AttemptCompletedRejected(AttemptCompletedError::UnknownOrInactiveBloom)
                        | Outcome::AggregateVerifyRejected(AggregateVerifyError::UnknownOrInactiveBloom)
                ),
                "re-adopted evidence was consumed against the empty boot snapshot: {decisions:?}"
            );
            let event: Event = from_bytes(&record.event)
                .unwrap_or_else(|error| panic!("record {} event did not decode: {error}", record.idempotency_key));
            if matches!(event.fact, Fact::AttemptCompleted { .. }) {
                return (event, decisions);
            }
        }
        assert!(Instant::now() < deadline, "re-adopted attempt never reached the journal");
        cid += 1;
        thread::sleep(Duration::from_millis(100));
    }
}

// The plausible bug: the executor re-adopts a completed order at boot and
// admits its evidence while journal replay is still folding, so the reducer
// decides UnknownOrInactiveBloom against the empty snapshot and ADR-0190
// consumes the key. A later replay cannot resurrect it — the bloom stays
// stranded. Holding the admit until replay finishes is what keeps the
// completion decidable against the bloom it belongs to (#5066).
#[test]
fn a_completed_order_is_admitted_after_replay_not_as_unknown_bloom() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("bloomery.db");
    let worktrees = dir.path().join("worktrees");
    fs::create_dir_all(&worktrees).unwrap();
    let db = db.to_str().unwrap();
    let worktrees_path = worktrees.to_str().unwrap();

    let workpiece = "wp-readopt";
    let nonce = "dispatch-426";
    let (event, decisions, bloom) = sealed_bloom("seal-readopt", workpiece);
    {
        let mut store = SqliteStore::open(db).unwrap();
        store
            .append_event(&JournalWrite {
                idempotency_key: "seal-readopt",
                event: &to_vec(&event).unwrap(),
                decisions: &to_vec(&decisions).unwrap(),
                decider: "recovery-test",
            })
            .unwrap();
        plant_completed_order(&mut store, &worktrees, bloom, workpiece, nonce);
    }

    // The restart: same store, same scratch root the previous process left.
    // A long poll keeps the boot tick as the only drain so a retry the
    // admission may enqueue does not spawn a live lane while we inspect.
    let port = free_port();
    let _coordinator = Coordinator::spawn(
        port,
        &[
            ("AETHER_STORE_PATH", db),
            ("AETHER_GITHUB_LOCAL_WORKTREE_BASE", worktrees_path),
            ("AETHER_GITHUB_POLL_INTERVAL_SECS", "3600"),
        ],
    );
    let mut stream = connect_and_handshake(port, "recovery-test");

    let (admitted, decided) = wait_for_attempt(&mut stream, 1);
    match admitted.fact {
        Fact::AttemptCompleted { bloom: got, workpiece: got_wp, .. } => {
            assert_eq!(got, bloom, "the completion names the replayed bloom");
            assert_eq!(got_wp.0, workpiece);
        }
        other => panic!("expected AttemptCompleted, got {other:?}"),
    }
    assert!(
        !matches!(decided.outcome, Outcome::AttemptCompletedRejected(_)),
        "the completion was decided against the restored bloom, not refused: {decided:?}"
    );
}
