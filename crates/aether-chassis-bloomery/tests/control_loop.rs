//! The ADR-0149 migration step 1 exit gate, cross-process: boot the `bloomery`
//! bin — the control core is a native capability mounted into the chassis at
//! boot, so it comes up already live — seal a synthetic single-workpiece bloom
//! by admitting a `Fact::Seal` event through the `aether.bloomery.admit`
//! ingress, `kill -9` the process, restart it against the same database (the
//! control core's `wire` replays the journal on boot), and prove convergence
//! **through the reducer**: `aether.bloomery.query` returns the rebuilt bloom,
//! and a second overlapping seal loses cleanly.
//!
//! Unlike `recovery.rs` (which drives the raw `aether.store.*` primitives), this
//! drives the admit/query mail and reply outcomes only — never the store
//! directly — so it exercises the whole control loop: admit → reduce → combined
//! commit → snapshot apply, and boot replay → rebuild.
//!
//! The one exception is `aether.store.record_config`, which the configuration
//! test below sends deliberately: it is standing in for the api cap, which
//! authors a configuration straight to the store without the control core
//! hearing about it (ADR-0174). Reaching the store is the *point* of that test —
//! it is how the core's resolved set is made stale, which is the condition the
//! deferred re-read exists to survive.

#![allow(clippy::unwrap_used)]
// The test addresses the native control cap by its lineage path
// (`mailbox_id_from_path`), disallowed-by-default outside the id/routing API and
// permitted here per the clippy.toml test carve-out.
#![allow(clippy::disallowed_methods)]

mod common;

use std::net::TcpStream;
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use aether_bloomery::{
    Admit, AdmitResult, BloomDraft, BloomId, CONTROL_CORE_NAMESPACE, CalibrationDocument, CandidateRef, ConfigKind,
    ConfigRegistry, Decisions, Digest, Event, Evidence, EvidenceKind, Fact, IdempotencyKey, Membership, ModelOverride,
    OperatorHold, OperatorRepair, OperatorRepairError, Outcome, Query, QueryResult, ResolvedConfigs, SealError,
    Snapshot, SpendWindow, StageCatalog, StageId, StudyCost, StudyRecord, Unproducible, ViewDocument, WorkpieceId,
    digest_of, reduce,
};
use aether_chassis_bloomery::artifacts::{ArtifactsCapabilityState, PutResult};
use aether_chassis_bloomery::store::{JournalWrite, RecordConfig, RecordConfigResult, SqliteStore, StoreBackend};
use aether_codec::frame::{read_frame, write_frame};
use aether_data::wire::{from_bytes, to_vec};
use aether_data::{Kind, MailboxId, mailbox_id_from_path};
use aether_rpc::WireFrame;
use common::client::{call, call_frame, connect_and_handshake, try_connect_and_handshake};
use common::{Coordinator, free_port};
use serde::Serialize;

/// Fork the `bloomery` bin against `db` on `port`, reaped when the returned
/// guard drops.
fn spawn(port: u16, db: &str) -> Coordinator {
    Coordinator::spawn(port, &[("AETHER_STORE_PATH", db)])
}

/// Fork the coordinator against `db` and handshake only with the child that
/// stayed alive. Same bind-race retry as [`spawn_with_artifacts`].
fn spawn_with_store(db: &str, client_name: &str) -> (Coordinator, TcpStream) {
    spawn_and_connect(client_name, Duration::from_secs(30), || {
        let port = free_port();
        (port, spawn(port, db))
    })
}

/// Pipeline two typed `Call`s to `mailbox` — write **both** frames before reading
/// either reply — so the second request sits in the actor's mailbox while the
/// first's store round-trip is still outstanding, forcing the in-flight
/// same-key admit interleaving. Returns each cid's reply of the expected kind.
fn call_pair<Req, Reply>(
    stream: &mut TcpStream,
    cids: (u64, u64),
    mailbox: MailboxId,
    requests: (&Req, &Req),
) -> (Reply, Reply)
where
    Req: Kind + Serialize,
    Reply: Kind,
{
    for (cid, request) in [(cids.0, requests.0), (cids.1, requests.1)] {
        write_frame(stream, &call_frame(cid, mailbox, request)).unwrap();
    }

    let mut first: Option<Reply> = None;
    let mut second: Option<Reply> = None;
    let mut ended = 0;
    while ended < 2 {
        match read_frame(stream).unwrap() {
            WireFrame::ReplyEvent { cid, envelope } => {
                if envelope.kind == Reply::ID
                    && let Some(reply) = Reply::decode_from_bytes(&envelope.payload)
                {
                    if cid == cids.0 {
                        first = Some(reply);
                    } else if cid == cids.1 {
                        second = Some(reply);
                    } else {
                        panic!("ReplyEvent for an unexpected cid {cid}");
                    }
                }
            }
            WireFrame::ReplyEnd { cid, result } => {
                result.unwrap();
                assert!(cid == cids.0 || cid == cids.1, "ReplyEnd for an unexpected cid {cid}");
                ended += 1;
            }
            other => panic!("unexpected frame during pipelined pair: {other:?}"),
        }
    }
    (
        first.expect("the first cid produced a reply of the expected kind"),
        second.expect("the second cid produced a reply of the expected kind"),
    )
}

/// The native control-core capability's mailbox — mounted into the chassis at
/// boot under `aether.bloomery.control`, addressed by its lineage path.
fn control_mailbox() -> MailboxId {
    mailbox_id_from_path(CONTROL_CORE_NAMESPACE)
}

/// A valid `Fact::Seal` event for a single-workpiece bloom on `base`, its member
/// approved (the approval evidence binds the member's own scope revision, which
/// the reducer's admission requires). Distinct `base` seeds yield distinct bloom
/// ids over the same workpiece.
fn seal_event(key: &str, base: u8, workpiece: &str) -> Event {
    seal_event_configured(key, base, workpiece, ConfigRegistry::default())
}

/// [`seal_event`], with the member sealing `configs` (ADR-0174).
fn seal_event_configured(key: &str, base: u8, workpiece: &str, configs: ConfigRegistry) -> Event {
    let scope_revision = Digest::from_bytes([1; 32]);
    let mut member = Membership {
        workpiece: WorkpieceId(workpiece.to_owned()),
        scope_revision,
        configs,
        approval: Evidence {
            subject: Digest::default(),
            kind: EvidenceKind::Approval,
            detail: Digest::from_bytes([200; 32]),
        },
    };
    // The approval binds the member's subject (ADR-0174).
    member.approval.subject = member.subject();
    // An empty registry selects the compiled stage line. A configured seal uses
    // the catalog content the caller resolved before reducing.
    let spec =
        BloomDraft { proposals: vec![member], base: Digest::from_bytes([base; 32]), ..BloomDraft::default() }.seal();
    Event { idempotency_key: IdempotencyKey(key.to_owned()), fact: Fact::Seal(spec) }
}

/// Admit an event's wire bytes and decode the reducer outcome from the reply.
fn admit(stream: &mut TcpStream, cid: u64, control: MailboxId, event: &Event) -> Outcome {
    let admit = Admit { event: to_vec(event).unwrap() };
    match call::<_, AdmitResult>(stream, cid, control, &admit) {
        AdmitResult::Ok { outcome } => from_bytes(&outcome).expect("outcome decodes"),
        AdmitResult::Err { error } => panic!("admit failed: {error}"),
    }
}

/// Query the whole projection, retrying until the expected bloom count appears —
/// boot replay runs asynchronously after a component load, so a query issued
/// immediately can race the rebuild.
fn query_until_blooms(stream: &mut TcpStream, cid_base: u64, control: MailboxId, want: usize) -> ViewDocument {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut cid = cid_base;
    loop {
        let document = match call::<_, QueryResult>(
            stream,
            cid,
            control,
            &Query { bloom: None, release: None, calibration: false },
        ) {
            QueryResult::Document { document } => from_bytes::<ViewDocument>(&document).expect("document decodes"),
            other => panic!("expected a document reply, got {other:?}"),
        };
        if document.blooms.len() == want || Instant::now() >= deadline {
            return document;
        }
        cid += 1;
        thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn replay_folds_the_recorded_decision_not_the_current_reducer() {
    // The #4937 incident, both directions (ADR-0190). Row 1: a seal the reducer
    // of its day REJECTED — rejected events journal, the inbox dedup requires
    // it — that today's reducer would ADMIT (the resurrection direction: a rule
    // loosened after the row was decided). Row 2: a seal recorded as ADMITTED
    // that today's reducer would refuse in row 1's re-decided wake, because a
    // resurrected row 1 occupies the one active bloom (the cascade direction: a
    // rule tightened after the row was decided). A fold reproduces the recorded
    // history — bloom B alone; a re-decide inverts it — bloom A alone.
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("bloomery.db");
    let db = db.to_str().unwrap();

    let resurrectable = seal_event("seal-a", 0, "wp-a");
    let control_a = reduce(&Snapshot::default(), &resurrectable, &ResolvedConfigs::default(), &SpendWindow::default());
    assert!(
        matches!(control_a.outcome, Outcome::Sealed(_)),
        "fixture control: today's reducer would admit the rejected row (else the resurrection arm tests nothing)"
    );
    let refusal = Decisions { outcome: Outcome::SealRejected(SealError::EmptyMembership), effects: Vec::new() };

    let admitted = seal_event("seal-b", 0, "wp-b");
    let decided_b = reduce(&Snapshot::default(), &admitted, &ResolvedConfigs::default(), &SpendWindow::default());
    assert!(matches!(decided_b.outcome, Outcome::Sealed(_)), "fixture control: the admitted row's record seals");

    let mut store = SqliteStore::open(db).unwrap();
    store
        .append_event(&JournalWrite {
            idempotency_key: "seal-a",
            event: &to_vec(&resurrectable).unwrap(),
            decisions: &to_vec(&refusal).unwrap(),
            decider: "older-rules",
        })
        .unwrap();
    store
        .append_event(&JournalWrite {
            idempotency_key: "seal-b",
            event: &to_vec(&admitted).unwrap(),
            decisions: &to_vec(&decided_b).unwrap(),
            decider: "older-rules",
        })
        .unwrap();
    drop(store);

    let port = free_port();
    let _coordinator = spawn(port, db);
    let mut stream = connect_and_handshake(port, "control-loop-test");
    let control = control_mailbox();

    // The admitted row folded (so the fold demonstrably ran past both rows),
    // the rejected row stayed rejected: exactly bloom B, never bloom A.
    let document = query_until_blooms(&mut stream, 10, control, 1);
    assert_eq!(document.blooms.len(), 1, "one bloom folds back: {document:?}");
    assert_eq!(document.blooms[0].members[0].workpiece.0, "wp-b", "the recorded rejection did not resurrect");

    // The rejected row's key is still durably consumed: re-admitting it is a
    // duplicate, not a fresh decision under today's rules.
    let replayed = admit(&mut stream, 30, control, &resurrectable);
    assert!(matches!(replayed, Outcome::Duplicate), "the refused key stays consumed: {replayed:?}");
}

#[test]
fn control_loop_converges_across_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("bloomery.db");
    let db = db.to_str().unwrap();

    // First boot: load the control core and seal a synthetic bloom through the
    // admit ingress — the reducer decides, the combined commit persists.
    let (coordinator, mut stream) = spawn_with_store(db, "control-loop-test");

    let control = control_mailbox();
    let sealed = admit(&mut stream, 2, control, &seal_event("seal-1", 0, "wp"));
    assert!(matches!(sealed, Outcome::Sealed(_)), "the first seal admits and seals: {sealed:?}");

    // Crash: SIGKILL after the committed admit.
    drop(stream);
    coordinator.kill9();

    // Restart against the same database and reload the control core; its `wire`
    // replays the journal to rebuild the snapshot.
    let (_coordinator, mut stream) = spawn_with_store(db, "control-loop-test");
    let control = control_mailbox();

    // Convergence: the rebuilt snapshot names the bloom, folded from the
    // decisions the first boot's admission recorded (ADR-0190).
    let document = query_until_blooms(&mut stream, 10, control, 1);
    assert_eq!(document.blooms.len(), 1, "journal replay rebuilt exactly the sealed bloom");
    let bloom = &document.blooms[0];
    assert_eq!(bloom.members.len(), 1);
    assert_eq!(bloom.members[0].workpiece.0, "wp");

    // A second overlapping seal (a different bloom over the same workpiece) loses
    // cleanly against the rebuilt membership — proof the snapshot converged, not
    // just the raw store.
    let second = admit(&mut stream, 20, control, &seal_event("seal-2", 5, "wp"));
    assert!(matches!(second, Outcome::SealRejected(_)), "the overlapping seal is refused: {second:?}");
}

/// Read the calibration document, retrying until the ledger has measured
/// something — boot replay runs asynchronously after a component load, so a read
/// issued immediately races the rebuild exactly as `query_until_blooms` does.
fn calibration_until_measured(stream: &mut TcpStream, cid_base: u64, control: MailboxId) -> CalibrationDocument {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut cid = cid_base;
    loop {
        let read = Query { bloom: None, release: None, calibration: true };
        let document = match call::<_, QueryResult>(stream, cid, control, &read) {
            QueryResult::Calibration { document } => {
                from_bytes::<CalibrationDocument>(&document).expect("calibration document decodes")
            }
            other => panic!("expected a calibration reply, got {other:?}"),
        };
        if !document.ledger.cells.is_empty() || Instant::now() >= deadline {
            return document;
        }
        cid += 1;
        thread::sleep(Duration::from_millis(100));
    }
}

// Tripwire (ADR-0184): the capability ledger is folded at *both* of the control
// core's apply sites — the live commit and the boot replay — so a restart
// rebuilds exactly what the crashed process had measured.
//
// Folding at one site only is the failure that reads as correct right up until
// the coordinator restarts: the live path alone measures a running fleet and
// then loses every observation the journal still holds, and the replay path
// alone leaves a calibration read stale by however long the process has been up.
// Reading the same cell on both sides of a `kill -9` is what tells them apart.
#[test]
fn the_capability_ledger_is_measured_live_and_rebuilt_on_replay() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("bloomery.db");
    let db = db.to_str().unwrap();

    // The agent the compiled line runs Construct under, resolved the way the
    // ledger resolves it. Derived rather than spelled, because the line's
    // harness/model/effort are refinable without an ADR.
    let construct = ModelOverride::default().resolve(StageId::Construct, &StageCatalog::profile_of(StageId::Construct));

    let (coordinator, mut stream) = spawn_with_store(db, "calibration-test");
    let control = control_mailbox();

    // The seal dispatches its one member at the entry stage, which is the one
    // measurement this bloom has made.
    let sealed = admit(&mut stream, 2, control, &seal_event("seal-1", 0, "wp"));
    assert!(matches!(sealed, Outcome::Sealed(_)), "the seal admits: {sealed:?}");

    let live = calibration_until_measured(&mut stream, 10, control);
    assert_eq!(live.ledger.cells.len(), 1, "one model lane has dispatched, so the ledger measures one cell");
    assert_eq!(live.ledger.cells[0].stage, StageId::Construct);
    assert_eq!(live.ledger.cells[0].agent, construct, "the cell is keyed by the agent the sealed line resolves");
    assert_eq!(live.ledger.cells[0].attempts, 1);
    assert!(!live.ledger.caveat.is_empty(), "the read carries its honesty boundary");

    drop(stream);
    coordinator.kill9();

    let (_coordinator, mut stream) = spawn_with_store(db, "calibration-test");
    let control = control_mailbox();

    let replayed = calibration_until_measured(&mut stream, 30, control);
    assert_eq!(replayed.ledger, live.ledger, "boot replay rebuilds the ledger the live fold measured");
}

/// Fork the coordinator against a unique artifacts root and handshake only
/// with the child that stayed alive.
///
/// Port selection, spawn, and handshake are one bounded retryable
/// transaction. `free_port` binds `:0` and releases, so a sibling can claim
/// the port before this bin binds. The loser exits; a deadline handshake
/// then waits 30s for a process that can never become ready. Retrying the
/// whole fork after an early exit (or a handshake that landed on a
/// stranger) is what turns that flake into another attempt.
fn spawn_with_artifacts(db: &str, artifacts: &str, client_name: &str) -> (Coordinator, TcpStream) {
    spawn_and_connect(client_name, Duration::from_secs(30), || {
        let port = free_port();
        (port, Coordinator::spawn(port, &[("AETHER_STORE_PATH", db), ("AETHER_ARTIFACTS_ROOT", artifacts)]))
    })
}

/// Run `spawn` and handshake as one transaction: a fresh child per attempt,
/// handshake attempts only while that child is alive, the whole fork retried
/// after an early exit or a bind collision. Returns the live guard beside
/// the stream so the caller cannot keep a connection to a stranger.
fn spawn_and_connect(
    client_name: &str,
    budget: Duration,
    mut spawn: impl FnMut() -> (u16, Coordinator),
) -> (Coordinator, TcpStream) {
    let deadline = Instant::now() + budget;
    let mut last = String::from("no attempt");
    while Instant::now() < deadline {
        let (port, mut coordinator) = spawn();
        match handshake_while_alive(&mut coordinator, port, client_name, deadline) {
            Ok(stream) => return (coordinator, stream),
            Err(why) => last = why,
        }
    }
    panic!("no artifacts coordinator answered a handshake: {last}");
}

/// Poll the one-attempt handshake while `coordinator` is still the process
/// on `port`. An exited child is abandoned immediately — it will never
/// become ready — so the caller can retry the whole spawn.
fn handshake_while_alive(
    coordinator: &mut Coordinator,
    port: u16,
    client_name: &str,
    deadline: Instant,
) -> Result<TcpStream, String> {
    let mut last = String::from("child exited before a handshake attempt");
    while coordinator.is_alive() && Instant::now() < deadline {
        match try_connect_and_handshake(port, client_name) {
            Ok(stream) if coordinator.is_alive() => return Ok(stream),
            Ok(_) => return Err(format!("child on port {port} exited after handshake")),
            Err(why) => {
                last = why;
                if !coordinator.is_alive() {
                    break;
                }
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
    if coordinator.is_alive() {
        Err(last)
    } else {
        Err(format!("child on port {port} exited: {last}"))
    }
}

// The plausible bug: a journal that names a study artifact still reports
// zero cost because the resolver is a stub, so every pricing decision
// reads as "this seat is free".
#[test]
fn a_calibration_read_fills_cost_columns_from_a_resolved_study_artifact() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("bloomery.db");
    let artifacts_root = dir.path().join("artifacts");
    let db = db.to_str().unwrap();
    let artifacts_root = artifacts_root.to_str().unwrap();

    let (_coordinator, mut stream) = spawn_with_artifacts(db, artifacts_root, "calibration-study");
    let control = control_mailbox();

    let sealed = admit(&mut stream, 2, control, &seal_event("seal-1", 0, "wp"));
    let Outcome::Sealed(bloom) = sealed else {
        panic!("the seal admits: {sealed:?}");
    };

    let subject = Digest::from_bytes([1; 32]);
    let study = StudyRecord {
        bloom,
        subject,
        cost: StudyCost { cost_micro_usd: 7_000, duration_millis: 4_500, ..StudyCost::default() },
    };
    let bytes = to_vec(&study).unwrap();
    let mut artifacts = ArtifactsCapabilityState::open(Path::new(artifacts_root)).unwrap();
    assert!(matches!(artifacts.put(&bytes, &[]), PutResult::Ok { .. }));

    let admitted = admit(
        &mut stream,
        3,
        control,
        &Event {
            idempotency_key: IdempotencyKey("study-1".to_owned()),
            fact: Fact::AdmitEvidence {
                bloom,
                evidence: Evidence { subject, kind: EvidenceKind::StudyRecord, detail: Digest::of_wire_bytes(&bytes) },
            },
        },
    );
    assert!(matches!(admitted, Outcome::EvidenceAdmitted { .. }), "the study evidence admits: {admitted:?}");

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut cid = 10;
    let document = loop {
        let document = match call::<_, QueryResult>(
            &mut stream,
            cid,
            control,
            &Query { bloom: None, release: None, calibration: true },
        ) {
            QueryResult::Calibration { document } => {
                from_bytes::<CalibrationDocument>(&document).expect("calibration document decodes")
            }
            other => panic!("expected a calibration reply, got {other:?}"),
        };
        if document.ledger.cells.iter().any(|cell| cell.samples > 0) || Instant::now() >= deadline {
            break document;
        }
        cid += 1;
        thread::sleep(Duration::from_millis(100));
    };

    let construct = document.ledger.cells.iter().find(|cell| cell.stage == StageId::Construct).expect("Construct ran");
    assert_eq!(construct.cost_micro_usd, 7_000, "the cost column is the record's priced figure");
    assert_eq!(construct.worker_secs, 4, "worker time is the record's duration, in whole seconds");
    assert_eq!(construct.samples, 1, "samples counts the resolved record only");
    assert_eq!(construct.attempts, 1);
}

// The plausible bug: an evidence link whose artifact is gone is billed as
// zero, so a missing file looks like a free attempt and samples still
// increments as if the measurement landed.
#[test]
fn an_unresolvable_study_artifact_stays_unaccounted() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("bloomery.db");
    let artifacts_root = dir.path().join("artifacts");
    let db = db.to_str().unwrap();
    let artifacts_root = artifacts_root.to_str().unwrap();

    let (_coordinator, mut stream) = spawn_with_artifacts(db, artifacts_root, "calibration-missing");
    let control = control_mailbox();

    let sealed = admit(&mut stream, 2, control, &seal_event("seal-1", 0, "wp"));
    let Outcome::Sealed(bloom) = sealed else {
        panic!("the seal admits: {sealed:?}");
    };

    let admitted = admit(
        &mut stream,
        3,
        control,
        &Event {
            idempotency_key: IdempotencyKey("study-missing".to_owned()),
            fact: Fact::AdmitEvidence {
                bloom,
                evidence: Evidence {
                    subject: Digest::from_bytes([1; 32]),
                    kind: EvidenceKind::StudyRecord,
                    detail: Digest::from_bytes([0xEE; 32]),
                },
            },
        },
    );
    assert!(
        matches!(admitted, Outcome::EvidenceAdmitted { .. }),
        "the dangling study evidence still admits: {admitted:?}"
    );

    let document = calibration_until_measured(&mut stream, 10, control);
    let construct = document.ledger.cells.iter().find(|cell| cell.stage == StageId::Construct).expect("Construct ran");
    assert_eq!(construct.attempts, 1, "the dispatch still counts");
    assert_eq!(construct.samples, 0, "an unresolvable artifact is not a sample");
    assert_eq!(construct.cost_micro_usd, 0, "and it must not become a zero-priced measurement");
    assert_eq!(construct.worker_secs, 0);
}

// The plausible bug: the first child loses the bind race and exits, but
// the fixture keeps calling the 30s handshake helper against that port,
// so a recoverable startup collision becomes a verifier timeout.
#[test]
fn an_early_dead_study_artifact_coordinator_is_retried_without_the_handshake_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("bloomery.db");
    let artifacts = dir.path().join("artifacts");
    let db = db.to_str().unwrap();
    let artifacts = artifacts.to_str().unwrap();

    let mut attempts = 0;
    let mut first_returned_at = None;
    let mut second_started_at = None;
    let (mut coordinator, _stream) = spawn_and_connect("calibration-retry", Duration::from_secs(30), || {
        attempts += 1;
        if attempts == 1 {
            let port = free_port();
            let mut coordinator =
                Coordinator::spawn(port, &[("AETHER_STORE_PATH", db), ("AETHER_ARTIFACTS_ROOT", artifacts)]);
            let _ = Command::new("kill").args(["-9", &coordinator.pid().to_string()]).status().unwrap();
            let give_up = Instant::now() + Duration::from_secs(2);
            while coordinator.is_alive() && Instant::now() < give_up {
                thread::sleep(Duration::from_millis(5));
            }
            assert!(!coordinator.is_alive(), "the scripted first child must already be dead");
            first_returned_at = Some(Instant::now());
            (port, coordinator)
        } else {
            if second_started_at.is_none() {
                second_started_at = Some(Instant::now());
            }
            let port = free_port();
            (port, Coordinator::spawn(port, &[("AETHER_STORE_PATH", db), ("AETHER_ARTIFACTS_ROOT", artifacts)]))
        }
    });

    assert!(attempts >= 2, "the dead first child must be retried, attempts={attempts}");
    let gap = second_started_at.expect("a second spawn ran") - first_returned_at.expect("the first spawn returned");
    assert!(gap < Duration::from_secs(3), "retrying a dead child must not burn the 30s handshake deadline: {gap:?}");
    assert!(coordinator.is_alive(), "the retried child stayed up through handshake");
}

#[test]
fn concurrent_same_key_admits_each_get_a_coherent_ok() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("bloomery.db");
    let db = db.to_str().unwrap();

    let port = free_port();
    let _coordinator = spawn(port, db);
    let mut stream = connect_and_handshake(port, "control-loop-test");
    let control = control_mailbox();

    // Two admits sharing one idempotency key, pipelined before either reply is
    // read, so the second sits in the mailbox while the first's store round-trip
    // is still outstanding — the in-flight interleaving. Tripwire: the pre-fix
    // code displaced the first pending entry on the second admit and answered the
    // first admitter `Err "superseded by a concurrent admit…"` even though its
    // commit landed; the fan-out fix opens one commit per key and hands every
    // same-key admitter the one resolved outcome.
    let admit = Admit { event: to_vec(&seal_event("dup-key", 0, "wp")).unwrap() };
    let (first, second): (AdmitResult, AdmitResult) = call_pair(&mut stream, (2, 3), control, (&admit, &admit));
    for (label, result) in [("first", &first), ("second", &second)] {
        match result {
            AdmitResult::Ok { outcome } => {
                let outcome: Outcome = from_bytes(outcome).expect("outcome decodes");
                assert!(
                    matches!(outcome, Outcome::Sealed(_) | Outcome::Duplicate),
                    "the {label} same-key admit gets a coherent outcome, never a rejection or stray error: {outcome:?}"
                );
            }
            AdmitResult::Err { error } => panic!("the {label} same-key admit got a spurious error: {error}"),
        }
    }

    // Exactly one bloom: one journal row, one applied decision — the deduped
    // second admit opened no second commit and forced no double-apply.
    let document = query_until_blooms(&mut stream, 10, control, 1);
    assert_eq!(document.blooms.len(), 1, "one bloom from the deduped same-key admit pair: {:?}", document.blooms);
}

/// Author a configuration straight to the store, exactly as the api cap's
/// `POST /configs` does — behind the control core's back.
fn author_config<K: ConfigKind>(stream: &mut TcpStream, cid: u64, value: &K) -> Digest {
    let address = value.address();
    let record =
        RecordConfig { digest: address.as_bytes().to_vec(), kind: K::NAME.to_owned(), bytes: to_vec(value).unwrap() };
    let store = mailbox_id_from_path("aether.store");
    match call::<_, RecordConfigResult>(stream, cid, store, &record) {
        // Tripwire: `POST /configs` renders its response *and* fills the api
        // cap's resolved-config cache from this echo rather than from a held
        // correlation entry (#3694, #4616), so an echo that does not reproduce
        // the request hands a caller a digest that resolves to nothing — the
        // ADR-0174 divergence the write exists to close.
        RecordConfigResult::Ok { digest, kind, bytes } => {
            assert_eq!(digest, address.as_bytes(), "the write echoes the address it stored under");
            assert_eq!(kind, K::NAME, "the write echoes the kind it stored under");
            assert_eq!(bytes, to_vec(value).unwrap(), "the write echoes the bytes it stored");
            address
        }
        RecordConfigResult::Err { error } => panic!("config write failed: {error}"),
    }
}

// Tripwire: a seal naming a configuration authored *after* the control core
// booted still admits (ADR-0174). The api cap writes an authored config straight
// to the store, so the core's resolved set is stale by construction the first
// time any new configuration is sealed — without the deferred re-read the core
// would refuse a perfectly good seal, and every operator would have to restart
// the coordinator between authoring a config and using it.
//
// The second half is the other side of the same gate: an address nothing ever
// authored is refused, and named. A re-read cannot conjure it, so the refusal has
// to arrive rather than the core re-reading forever.
#[test]
fn a_seal_naming_a_configuration_authored_after_boot_still_admits() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("bloomery.db");
    let db = db.to_str().unwrap();

    let port = free_port();
    let _coordinator = spawn(port, db);
    let mut stream = connect_and_handshake(port, "control-loop-test");
    let control = control_mailbox();

    let override_config = ModelOverride::default();
    let address = author_config(&mut stream, 2, &override_config);
    let mut configs = ConfigRegistry::default();
    configs.insert::<ModelOverride>(address);

    let sealed = admit(&mut stream, 3, control, &seal_event_configured("cfg-seal", 0, "wp-a", configs));
    assert!(
        matches!(sealed, Outcome::Sealed(_)),
        "a seal naming a config authored after boot admits on the re-read: {sealed:?}",
    );

    // An address nothing authored: the re-read runs, finds nothing, and the
    // reducer's own refusal answers — naming the kind that went missing.
    let mut dangling = ConfigRegistry::default();
    dangling.insert::<ModelOverride>(Digest::from_bytes([0xAB; 32]));
    let refused = admit(&mut stream, 4, control, &seal_event_configured("cfg-dangling", 1, "wp-b", dangling));
    assert!(
        matches!(
            refused,
            Outcome::SealRejected(SealError::UnproducibleConfig { ref kind, reason: Unproducible::Absent, .. })
                if kind == ModelOverride::NAME
        ),
        "an address nothing authored is refused, naming its kind: {refused:?}",
    );
}

/// The same content-derived key the repair route mints: bloom id plus the
/// repair's own digest. A resent body without `--idempotency-key` is this
/// string twice.
fn repair_event(bloom: BloomId) -> Event {
    let repair = OperatorRepair {
        workpiece: WorkpieceId("wp".into()),
        candidate: CandidateRef { tree: Digest::from_bytes([0x60; 32]), checkout: Digest::from_bytes([0x61; 32]) },
        reason: "one-line fix, cheaper than a lap".into(),
        operator: "eve".into(),
    };
    let key = format!(
        "aether.bloomery.repair:{}:{}",
        hex_bytes(bloom.0.as_bytes()),
        hex_bytes(digest_of(&repair).as_bytes()),
    );
    Event { idempotency_key: IdempotencyKey(key), fact: Fact::OperatorRepair { bloom, repair } }
}

fn hex_bytes(bytes: &[u8]) -> String {
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut text, byte| {
        use std::fmt::Write;
        write!(&mut text, "{byte:02x}").expect("writing to String cannot fail");
        text
    })
}

fn fail_construct(key: &str, bloom: BloomId) -> Event {
    Event {
        idempotency_key: IdempotencyKey(key.into()),
        fact: Fact::AttemptCompleted {
            bloom,
            workpiece: WorkpieceId("wp".into()),
            stage: StageId::Construct,
            passed: false,
            evidence: Evidence {
                subject: Digest::from_bytes([70; 32]),
                kind: EvidenceKind::VerificationResult,
                detail: Digest::from_bytes([80; 32]),
            },
            candidate: None,
        },
    }
}

fn brake(key: &str, bloom: BloomId, fact: impl FnOnce(BloomId, OperatorHold) -> Fact) -> Event {
    Event {
        idempotency_key: IdempotencyKey(key.into()),
        fact: fact(bloom, OperatorHold { reason: "the run looks wrong".into(), operator: "eve".into() }),
    }
}

// Tripwire (#5107): a repair refused while the bloom is held must not occupy
// the content-derived key. The incident was a `Held` row answering the
// identical resubmit after release as `Duplicate`, so the operator had to
// guess at `--idempotency-key`. Replay is in the loop because `seen` is
// rebuilt from the event bytes: retagging only the store column would look
// correct until the next boot.
#[test]
fn a_rejected_repair_does_not_shadow_the_same_content_after_release() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("bloomery.db");
    let db = db.to_str().unwrap();

    let (coordinator, mut stream) = spawn_with_store(db, "control-loop-test");
    let control = control_mailbox();

    let Outcome::Sealed(bloom) = admit(&mut stream, 2, control, &seal_event("seal-1", 0, "wp")) else {
        panic!("the seal admits");
    };
    let retried = admit(&mut stream, 3, control, &fail_construct("fail-1", bloom));
    assert!(matches!(retried, Outcome::AttemptRetried { .. }), "first construct failure retries: {retried:?}");
    let wedged = admit(&mut stream, 4, control, &fail_construct("fail-2", bloom));
    assert!(matches!(wedged, Outcome::AttemptWedged { .. }), "the construct budget wedges: {wedged:?}");
    let held =
        admit(&mut stream, 5, control, &brake("hold-1", bloom, |bloom, hold| Fact::OperatorHold { bloom, hold }));
    assert!(matches!(held, Outcome::BloomHeld { .. }), "the bloom is on the brake: {held:?}");

    let repair = repair_event(bloom);
    let refused = admit(&mut stream, 6, control, &repair);
    assert!(
        matches!(refused, Outcome::OperatorRepairRejected(OperatorRepairError::Held)),
        "a repair while held is refused: {refused:?}",
    );
    let still_held = admit(&mut stream, 7, control, &repair);
    assert!(
        matches!(still_held, Outcome::OperatorRepairRejected(OperatorRepairError::Held)),
        "resubmitting while still held is still Held, not Duplicate: {still_held:?}",
    );

    drop(stream);
    coordinator.kill9();

    let (_coordinator, mut stream) = spawn_with_store(db, "control-loop-test");
    let control = control_mailbox();

    let after_replay = admit(&mut stream, 8, control, &repair);
    assert!(
        matches!(after_replay, Outcome::OperatorRepairRejected(OperatorRepairError::Held)),
        "replay must not put the content key in `seen`: {after_replay:?}",
    );

    let let_go = admit(
        &mut stream,
        9,
        control,
        &brake("release-1", bloom, |bloom, release| Fact::OperatorRelease { bloom, release }),
    );
    assert!(matches!(let_go, Outcome::BloomReleased { .. }), "the bloom comes off the brake: {let_go:?}");

    let accepted = admit(&mut stream, 10, control, &repair);
    assert!(
        matches!(accepted, Outcome::OperatorRepairAccepted { .. }),
        "the identical repair is accepted without a key override: {accepted:?}",
    );
    let replayed = admit(&mut stream, 11, control, &repair);
    assert!(matches!(replayed, Outcome::Duplicate), "an accepted repair still consumes the content key: {replayed:?}");
}
