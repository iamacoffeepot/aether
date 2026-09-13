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
use std::slice::from_ref;
use std::thread;
use std::time::{Duration, Instant};

use aether_bloomery::testing::{compiled_resolved, with_compiled_manifest};
use aether_bloomery::{
    AggregateVerifyError, AttemptCompletedError, BloomDraft, BloomId, CandidateRef, ConfigRegistry, Decision,
    Decisions, Digest, Event, Evidence, EvidenceKind, Fact, IdempotencyKey, Membership, Outcome, PipelineManifest,
    PrecheckCompletion, PrecheckMember, PrecheckNode, PrecheckPayload, PrecheckPlan, PrecheckPolicy,
    PrecheckPreparation, PrecheckState, ResolvedConfigs, Snapshot, SpendWindow, StageCatalog, StageId, Topic,
    Transformation, VerifyGateSet, WorkpieceId, config_address, decode_recorded_decisions, digest_of, reduce,
};
use aether_chassis_bloomery::store::{
    AppendEvent, AppendEventResult, ClaimSeal, ClaimSealResult, DrainOutbox, DrainOutboxResult, EnqueueOutbox,
    EnqueueOutboxResult, JournalWrite, OrderLifecycle, OutstandingOrder, ReplayJournal, ReplayJournalResult,
    SealOutcome, SqliteStore, StoreBackend,
};
use aether_data::wire::{from_bytes, to_vec};
use aether_data::{Kind, mailbox_id_from_path};
use common::client::{call, spawn_and_connect};
use common::{Coordinator, Ingress};
use serde::Serialize;

/// How long a fork has to come up and answer a handshake, across however many
/// forks that takes.
const HANDSHAKE_BUDGET: Duration = Duration::from_mins(1);

/// Fork the `bloomery` bin against `db` and handshake the child that stayed up,
/// reaped when the returned guard drops.
///
/// RPC port `0`: the child holds its port from the moment it binds and reports
/// which one, so neither a sibling fixture nor this suite's own restart can be
/// handed a port someone else already took (#5000, #5116).
fn spawn_ready(db: &str) -> (Coordinator, TcpStream) {
    let (coordinator, mut stream) =
        spawn_and_connect("recovery-test", HANDSHAKE_BUDGET, || Coordinator::spawn(0, &[("AETHER_STORE_PATH", db)]));
    file_compiled_manifest(&mut stream);
    (coordinator, stream)
}

fn file_compiled_manifest(stream: &mut TcpStream) {
    use aether_bloomery::{PipelineManifest, config_address};
    use aether_chassis_bloomery::store::{RecordConfig, RecordConfigResult};
    use aether_data::Kind;
    use aether_data::mailbox_id_from_path;
    use aether_data::wire::to_vec;

    let bytes = to_vec(&PipelineManifest::compiled()).expect("the compiled manifest encodes");
    let address = config_address(PipelineManifest::NAME, &bytes);
    let record = RecordConfig { digest: address.as_bytes().to_vec(), kind: PipelineManifest::NAME.to_owned(), bytes };
    match call::<_, RecordConfigResult>(stream, 9000, mailbox_id_from_path("aether.store"), &record) {
        RecordConfigResult::Ok { .. } => {}
        RecordConfigResult::Err { error } => panic!("compiled manifest write failed: {error}"),
    }
}

fn store_call<Req, Reply>(stream: &mut TcpStream, cid: u64, request: &Req) -> Reply
where
    Req: Kind + Serialize,
    Reply: Kind,
{
    call(stream, cid, mailbox_id_from_path("aether.store"), request)
}

#[test]
fn kill_and_restart_converges_over_rpc() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("bloomery.db");
    let db = db.to_str().unwrap();

    // First boot: seal a synthetic single-workpiece bloom through typed mail.
    let (coordinator, mut stream) = spawn_ready(db);

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

    let append: AppendEventResult = store_call(
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

    let seal: ClaimSealResult =
        store_call(&mut stream, 2, &ClaimSeal { bloom: vec![1; 32], members: vec!["wp".into()] });
    assert_eq!(seal, ClaimSealResult::Sealed);

    let enqueued: EnqueueOutboxResult =
        store_call(&mut stream, 3, &EnqueueOutbox { topic: "landing_receipt".into(), payload: b"receipt".to_vec() });
    assert_eq!(enqueued, EnqueueOutboxResult::Ok { sequence: 1 });

    // Crash: SIGKILL mid-service, after the committed transactions.
    drop(stream);
    coordinator.kill9();

    // Restart against the same database file.
    let (_coordinator, mut stream) = spawn_ready(db);

    // Journal replay: the sealed event survived the crash.
    let replay: ReplayJournalResult = store_call(&mut stream, 1, &ReplayJournal);
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
        store_call(&mut stream, 2, &ClaimSeal { bloom: vec![2; 32], members: vec!["wp".into()] });
    assert_eq!(seal_again, ClaimSealResult::Conflict { workpiece: "wp".into() });

    // Outbox republish: the undelivered landing receipt is still drainable.
    let drained: DrainOutboxResult = store_call(&mut stream, 3, &DrainOutbox { topic: None });
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
    let spec = with_compiled_manifest(BloomDraft {
        proposals: vec![member],
        base: Digest::from_bytes([0; 32]),
        ..BloomDraft::default()
    })
    .seal();
    let event = Event { idempotency_key: IdempotencyKey(key.to_owned()), fact: Fact::Seal(spec) };
    let decisions = reduce(&Snapshot::default(), &event, &compiled_resolved(), &SpendWindow::default());
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
            lifecycle: OrderLifecycle::Submitted,
            prompt_manifest: None,
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

struct PrecheckFixture {
    bloom: BloomId,
    node: PrecheckNode,
    payload: PrecheckPayload,
    state: PrecheckState,
}

fn append_journal(store: &mut SqliteStore, event: &Event, decisions: &Decisions) {
    store
        .append_event(&JournalWrite {
            idempotency_key: &event.idempotency_key.0,
            event: &to_vec(event).unwrap(),
            decisions: &to_vec(decisions).unwrap(),
            decider: "recovery-test",
        })
        .unwrap();
}

fn precheck_member(name: &str, revision: u8, approval: u8) -> Membership {
    let mut member = Membership {
        workpiece: WorkpieceId(name.to_owned()),
        scope_revision: Digest::from_bytes([revision; 32]),
        configs: ConfigRegistry::default(),
        approval: Evidence {
            subject: Digest::default(),
            kind: EvidenceKind::Approval,
            detail: Digest::from_bytes([approval; 32]),
        },
    };
    member.approval.subject = member.subject();
    member
}

fn precheck_plan(bloom: BloomId, first: &Membership, second: &Membership, manifest: &PipelineManifest) -> PrecheckPlan {
    PrecheckPlan {
        bloom,
        base: Digest::default(),
        members: vec![
            PrecheckMember {
                workpiece: first.workpiece.clone(),
                scope_revision: first.scope_revision,
                candidate: CandidateRef { tree: Digest::from_bytes([21; 32]), checkout: Digest::from_bytes([31; 32]) },
            },
            PrecheckMember {
                workpiece: second.workpiece.clone(),
                scope_revision: second.scope_revision,
                candidate: CandidateRef { tree: Digest::from_bytes([22; 32]), checkout: Digest::from_bytes([32; 32]) },
            },
        ],
        gate_set: digest_of(&VerifyGateSet::fold_of(manifest)),
    }
}

/// Plant a real opt-in seal and the reducer decision that issued one prepared
/// pre-check. The host rows are planted separately by each crash boundary.
fn plant_issued_precheck(store: &mut SqliteStore, key: &str) -> PrecheckFixture {
    let policy = PrecheckPolicy { run_budget: 2 };
    let policy_bytes = to_vec(&policy).unwrap();
    let policy_address = config_address(PrecheckPolicy::NAME, &policy_bytes);
    let manifest = PipelineManifest::compiled();
    let manifest_bytes = to_vec(&manifest).unwrap();
    let manifest_address = config_address(PipelineManifest::NAME, &manifest_bytes);

    let first = precheck_member("precheck-one", 11, 201);
    let second = precheck_member("precheck-two", 12, 202);

    let mut draft = with_compiled_manifest(BloomDraft {
        proposals: vec![first.clone(), second.clone()],
        base: Digest::default(),
        ..BloomDraft::default()
    });
    draft.configs.insert::<PrecheckPolicy>(policy_address);
    let spec = draft.seal();
    let bloom = spec.id();
    let seal = Event { idempotency_key: IdempotencyKey(format!("{key}-seal")), fact: Fact::Seal(spec) };
    let mut resolved = compiled_resolved();
    resolved.insert(policy_address, PrecheckPolicy::NAME, policy_bytes.clone(), None);
    let sealed = reduce(&Snapshot::default(), &seal, &resolved, &SpendWindow::default());
    assert!(matches!(sealed.outcome, Outcome::Sealed(got) if got == bloom));
    append_journal(store, &seal, &sealed);
    assert!(matches!(
        store.claim_seal(bloom.0.as_bytes(), &[first.workpiece.0.clone(), second.workpiece.0.clone()]).unwrap(),
        SealOutcome::Sealed
    ));
    store.record_config(manifest_address.as_bytes(), PipelineManifest::NAME, &manifest_bytes).unwrap();
    store.record_config(policy_address.as_bytes(), PrecheckPolicy::NAME, &policy_bytes).unwrap();

    let plan = precheck_plan(bloom, &first, &second, &manifest);
    let node = PrecheckNode {
        plan: plan.digest(),
        tree: Digest::from_bytes([40; 32]),
        head: Digest::from_bytes([41; 32]),
        gate_set: plan.gate_set,
    };
    let mut before_request = Snapshot::default().apply(&seal, &sealed, &resolved);
    let mut state = PrecheckState::new(policy);
    state.latest_plan = Some(plan);
    state.prepared = Some(node.clone());
    before_request.blooms.get_mut(&bloom).unwrap().precheck = Some(state);
    let request = Event {
        idempotency_key: IdempotencyKey(format!("{key}-request")),
        fact: Fact::RequestPrecheck { bloom, node: node.digest() },
    };
    let requested = reduce(&before_request, &request, &resolved, &SpendWindow::default());
    assert!(
        matches!(requested.outcome, Outcome::PrecheckRequested { bloom: got, node: got_node, run: 1 } if got == bloom && got_node == node.digest())
    );
    let state = requested
        .effects
        .iter()
        .find_map(|effect| match effect {
            Decision::RecordPrecheckState { bloom: owner, state } if *owner == bloom => state.as_deref().cloned(),
            _ => None,
        })
        .expect("the request records its issued state");
    let payload = requested
        .effects
        .iter()
        .find_map(|effect| match effect {
            Decision::DispatchPrecheck { bloom: owner, node, transformation, profile, configs } if *owner == bloom => {
                Some(PrecheckPayload {
                    bloom: bloom.0,
                    node: node.clone(),
                    transformation: transformation.clone(),
                    profile: profile.clone(),
                    configs: configs.clone(),
                })
            }
            _ => None,
        })
        .expect("the request dispatches the prepared pre-check");
    append_journal(store, &request, &requested);

    PrecheckFixture { bloom, node, payload, state }
}

fn precheck_order(
    fixture: &PrecheckFixture,
    nonce: &str,
    lifecycle: OrderLifecycle,
    deadline_unix_millis: u64,
) -> OutstandingOrder {
    OutstandingOrder {
        nonce: nonce.to_owned(),
        bloom: fixture.bloom.0.as_bytes().to_vec(),
        workpiece: WorkpieceId::COMPOSITION.to_owned(),
        scope_revision: fixture.node.digest().as_bytes().to_vec(),
        candidate: fixture.node.tree.as_bytes().to_vec(),
        displayed_digest: fixture.node.tree.as_bytes().to_vec(),
        stage: to_vec(&StageId::AggregateVerify).unwrap(),
        transformation: to_vec(&fixture.payload.transformation).unwrap(),
        configs: to_vec(&fixture.payload.configs).unwrap(),
        profile: to_vec(&fixture.payload.profile).unwrap(),
        deadline_unix_millis,
        lifecycle,
        prompt_manifest: None,
    }
}

fn wait_for_journal_key(stream: &mut TcpStream, key: &str, cid_base: u64) -> (Event, Decisions) {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut cid = cid_base;
    loop {
        let replay: ReplayJournalResult = store_call(stream, cid, &ReplayJournal);
        let records = match replay {
            ReplayJournalResult::Ok { records } => records,
            ReplayJournalResult::Err { error } => panic!("journal replay failed: {error}"),
        };
        let matching: Vec<_> = records.iter().filter(|record| record.idempotency_key == key).collect();
        assert!(matching.len() <= 1, "the recovered fact was journaled more than once: {matching:?}");
        if let Some(record) = matching.first() {
            return (
                from_bytes(&record.event).unwrap(),
                decode_recorded_decisions(&record.decisions, record.decisions_schema_digest.as_deref()).unwrap(),
            );
        }
        assert!(Instant::now() < deadline, "recovered fact `{key}` never reached the journal");
        cid += 1;
        thread::sleep(Duration::from_millis(100));
    }
}

fn plant_obsolete_idle_boundary(db: &str, deadline_unix_millis: u64) -> (String, String, PrecheckNode) {
    let mut store = SqliteStore::open(db).unwrap();
    let mut fixture = plant_issued_precheck(&mut store, "obsolete-idle");
    let plan = fixture.state.latest_plan.as_mut().unwrap();
    plan.members[0].candidate.tree = Digest::from_bytes([23; 32]);
    fixture.state.prepared = None;
    let changed = Event {
        idempotency_key: IdempotencyKey("obsolete-idle-newer-plan".to_owned()),
        fact: Fact::PrecheckPrepared {
            bloom: fixture.bloom,
            plan: plan.digest(),
            preparation: PrecheckPreparation::Refused { detail: Digest::from_bytes([99; 32]) },
        },
    };
    append_journal(
        &mut store,
        &changed,
        &Decisions {
            outcome: Outcome::PrecheckPreparationRefused {
                bloom: fixture.bloom,
                plan: plan.digest(),
                diagnostic: Digest::from_bytes([99; 32]),
            },
            effects: vec![Decision::RecordPrecheckState {
                bloom: fixture.bloom,
                state: Some(Box::new(fixture.state.clone())),
            }],
        },
    );

    let sequence =
        store.enqueue_outbox(Topic::DispatchPrecheck.as_str(), &to_vec(&fixture.payload).unwrap(), None).unwrap();
    let nonce = format!("dispatch-{sequence}");
    store.record_order(&precheck_order(&fixture, &nonce, OrderLifecycle::Submitting, deadline_unix_millis)).unwrap();
    let planted = store.lookup_order(&nonce).unwrap().unwrap();
    assert_eq!(planted.lifecycle, OrderLifecycle::Submitting);
    assert_eq!(planted.deadline_unix_millis, deadline_unix_millis);

    (nonce.clone(), format!("aether.bloomery.precheck_completed:{nonce}"), fixture.node)
}

/// Poll the store journal until a re-adopted attempt has been committed, or
/// the deadline expires.
fn wait_for_attempt(stream: &mut TcpStream, cid_base: u64) -> (Event, Decisions) {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut cid = cid_base;
    loop {
        let replay: ReplayJournalResult = store_call(stream, cid, &ReplayJournal);
        let records = match replay {
            ReplayJournalResult::Ok { records } => records,
            ReplayJournalResult::Err { error } => panic!("journal replay failed: {error}"),
        };
        for record in &records {
            let decisions = decode_recorded_decisions(&record.decisions, record.decisions_schema_digest.as_deref())
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
    let (_coordinator, mut stream) = spawn_and_connect("recovery-test", HANDSHAKE_BUDGET, || {
        Coordinator::spawn(
            0,
            &[
                ("AETHER_STORE_PATH", db),
                ("AETHER_GITHUB_LOCAL_WORKTREE_BASE", worktrees_path),
                ("AETHER_GITHUB_POLL_INTERVAL_SECS", "3600"),
            ],
        )
    });

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

// The plausible bug: boot sees an idle pre-check's `Submitting` reservation,
// treats it like an ordinary dispatch, and starts the obsolete order again.
// The restart must probe the missing worker, retain the original deadline
// while that probe settles, then journal a skip without materializing a lane.
#[test]
fn an_obsolete_idle_submission_is_probed_not_started_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("bloomery.db");
    let worktrees = dir.path().join("worktrees");
    fs::create_dir_all(&worktrees).unwrap();
    let db = db_path.to_str().unwrap();
    let worktrees_path = worktrees.to_str().unwrap();
    let deadline_unix_millis = 4_000_000_000_123;
    let (nonce, completion_key, node) = plant_obsolete_idle_boundary(db, deadline_unix_millis);

    let (coordinator, mut stream) = spawn_and_connect("recovery-test", HANDSHAKE_BUDGET, || {
        Coordinator::spawn(
            0,
            &[
                ("AETHER_STORE_PATH", db),
                ("AETHER_GITHUB_LOCAL_LANE_COMMANDS", "verify."),
                ("AETHER_GITHUB_LOCAL_WORKTREE_BASE", worktrees_path),
                ("AETHER_BLOOMERY_LANE_PROGRAM", "bloomery-control-loop-test"),
            ],
        )
    });

    let (event, decided) = wait_for_journal_key(&mut stream, &completion_key, 10_000);
    assert!(matches!(
        event.fact,
        Fact::PrecheckCompleted {
            node: got,
            completion: PrecheckCompletion::SkippedBeforeStart,
            ..
        } if got == node.digest()
    ));
    assert!(matches!(decided.outcome, Outcome::PrecheckCompleted { node: got, .. } if got == node.digest()));
    assert!(
        !worktrees.join(format!("{nonce}-evidence")).exists(),
        "a restart probe must not materialize the absent obsolete lane"
    );

    drop(stream);
    coordinator.kill9();
    let mut recovered = SqliteStore::open(db).unwrap();
    assert!(recovered.lookup_order(&nonce).unwrap().is_none(), "the skipped reservation is consumed");
    assert_eq!(
        recovered.replay_journal().unwrap().iter().filter(|record| record.idempotency_key == completion_key).count(),
        1,
        "the skip is accounted exactly once"
    );
}

// The plausible bug: restart recovery omits the pre-check dispatch topic, or
// the drain recreates its already-consumed order instead of replaying the
// retained completion. The original fact must cross the process boundary once.
#[test]
fn a_retained_precheck_completion_replays_once_without_recreating_its_order() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("bloomery.db");
    let db = db_path.to_str().unwrap();
    let deadline_unix_millis = 4_000_000_000_456;

    let (nonce, expected) = {
        let mut store = SqliteStore::open(db).unwrap();
        let fixture = plant_issued_precheck(&mut store, "retained-completion");
        let sequence =
            store.enqueue_outbox(Topic::DispatchPrecheck.as_str(), &to_vec(&fixture.payload).unwrap(), None).unwrap();
        let nonce = format!("dispatch-{sequence}");
        store.record_order(&precheck_order(&fixture, &nonce, OrderLifecycle::Submitted, deadline_unix_millis)).unwrap();
        let expected = Event {
            idempotency_key: IdempotencyKey(format!("aether.bloomery.precheck_completed:{nonce}")),
            fact: Fact::PrecheckCompleted {
                bloom: fixture.bloom,
                node: fixture.node.digest(),
                completion: PrecheckCompletion::HostFault(Evidence {
                    subject: fixture.node.tree,
                    kind: EvidenceKind::ExecutorFault,
                    detail: Digest::from_bytes([77; 32]),
                }),
            },
        };
        store.record_outbox_results(Topic::DispatchPrecheck.as_str(), sequence, from_ref(&expected)).unwrap();
        assert!(store.consume_order(&nonce).unwrap());
        assert_eq!(store.ack_outbox(Some(Topic::DispatchPrecheck.as_str()), sequence).unwrap(), 1);
        assert_eq!(
            store.outbox_results(Topic::DispatchPrecheck.as_str(), sequence).unwrap(),
            Some(vec![expected.clone()]),
            "the completion survives beside the acknowledged dispatch row"
        );
        assert!(
            !store.journal_holds_any(from_ref(&expected.idempotency_key.0)).unwrap(),
            "the crash boundary precedes journal admission"
        );
        (nonce, expected)
    };

    let (coordinator, mut stream) =
        spawn_and_connect("recovery-test", HANDSHAKE_BUDGET, || Coordinator::spawn(0, &[("AETHER_STORE_PATH", db)]));
    let (replayed, decided) = wait_for_journal_key(&mut stream, &expected.idempotency_key.0, 20_000);
    assert_eq!(replayed, expected, "recovery admits the retained fact byte-for-byte");
    let Fact::PrecheckCompleted { bloom: expected_bloom, node: expected_node, .. } = &expected.fact else {
        panic!("fixture control: expected a pre-check completion")
    };
    assert!(matches!(
        decided.outcome,
        Outcome::PrecheckCompleted { bloom, node }
            if bloom == *expected_bloom && node == *expected_node
    ));

    drop(stream);
    coordinator.kill9();
    let mut recovered = SqliteStore::open(db).unwrap();
    assert!(recovered.lookup_order(&nonce).unwrap().is_none(), "receipt replay does not recreate the consumed order");
    let matching: Vec<_> = recovered
        .replay_journal()
        .unwrap()
        .into_iter()
        .filter(|record| record.idempotency_key == expected.idempotency_key.0)
        .collect();
    assert_eq!(matching.len(), 1, "the retained completion is admitted exactly once");
    assert_eq!(from_bytes::<Event>(&matching[0].event).unwrap(), expected);
}

// The overlap the journal holder guard exists to prevent, across two real
// processes. The plausible bug: the store's open path ran the migrations
// unconditionally and asked nothing about who else was already here, so a
// supervisor restart that overlapped the outgoing process — or an operator
// starting a second coordinator by hand — silently folded one journal from two
// generations. WAL lets both of them write; only this guard says no.
#[test]
fn a_second_coordinator_refuses_a_journal_the_first_still_holds() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("bloomery.db");
    let db = db.to_str().unwrap();

    let (holder, holder_stream) = spawn_ready(db);

    // Deliberately not `spawn_and_connect`: this fork is supposed to die, and
    // the retrying helper would keep re-forking it until its budget ran out.
    let overlapping = Coordinator::spawn(0, &[("AETHER_STORE_PATH", db)]);
    let refusal = overlapping
        .await_port(Ingress::Rpc, Instant::now() + HANDSHAKE_BUDGET)
        .expect_err("a second coordinator must not come up against a held journal");
    assert!(refusal.contains(&holder.pid().to_string()), "the refusal names the holding pid: {refusal}");
    assert!(refusal.contains(db), "the refusal names the journal it was kept out of: {refusal}");
    drop(overlapping);

    // The holder is SIGKILLed, so it releases nothing. Its claim is stale, not
    // a lock: the next generation takes the journal over rather than being
    // shut out of it by its own predecessor's crash.
    drop(holder_stream);
    holder.kill9();
    let (_restarted, _stream) = spawn_ready(db);
}
