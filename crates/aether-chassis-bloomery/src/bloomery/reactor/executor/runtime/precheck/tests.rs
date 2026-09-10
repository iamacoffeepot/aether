use crate::bloomery::executor::LocalExecutorError;
use std::cell::Cell;
use std::io;

use aether_bloomery::testing::digest;
use aether_bloomery::{
    BackendId, ConfigRegistry, Decision, Decisions, IdempotencyKey, LaneObservation, ObservedLaneWrites, Outcome,
    PrecheckNode, PrecheckPolicy, PrecheckResult, PrecheckState, StageCatalog, StageVerdict, Transformation, WorkOrder,
};
use aether_data::wire::to_vec;

use super::*;
use crate::bloomery::executor::{ExecutorPortError, RunObservation};
use crate::bloomery::intake::{AdmitDecision, UploadedEvidence, admit_uploaded, record_dispatch};
use crate::bloomery::precheck::findings_key;
use crate::store::{JournalWrite, SqliteStore};

#[derive(Clone, Copy)]
enum Probe {
    Pending,
    Absent,
    Running,
    Fault,
}

struct Port {
    idle: Cell<bool>,
    probe: Cell<Probe>,
    idle_calls: Cell<u32>,
    required_calls: Cell<u32>,
}

impl Port {
    fn new() -> Self {
        Self {
            idle: Cell::new(false),
            probe: Cell::new(Probe::Absent),
            idle_calls: Cell::new(0),
            required_calls: Cell::new(0),
        }
    }
}

impl ExecutorPort for Port {
    fn backend_for(&self, _: &WorkHandle) -> BackendId {
        BackendId::SOLE
    }
    fn submit(&self, order: &WorkOrder) -> Settled<Result<WorkHandle, ExecutorPortError>> {
        self.required_calls.set(self.required_calls.get() + 1);
        Settled::Answered(Ok(WorkHandle::new(order.nonce.clone())))
    }
    fn try_submit_idle(&self, order: &WorkOrder) -> Settled<Result<Option<WorkHandle>, ExecutorPortError>> {
        self.idle_calls.set(self.idle_calls.get() + 1);
        Settled::Answered(Ok(self.idle.get().then(|| WorkHandle::new(order.nonce.clone()))))
    }
    fn has_idle_capacity(&self, _: &WorkOrder) -> bool {
        self.idle.get()
    }
    fn settle_idle_submission(&self, order: &WorkOrder) -> Settled<Result<Option<WorkHandle>, ExecutorPortError>> {
        match self.probe.get() {
            Probe::Pending => Settled::InFlight,
            Probe::Absent => Settled::Answered(Ok(None)),
            Probe::Running => Settled::Answered(Ok(Some(WorkHandle::new(order.nonce.clone())))),
            Probe::Fault => Settled::Answered(Err(ExecutorPortError::Local(LocalExecutorError::Spawn(
                io::Error::from(io::ErrorKind::ArgumentListTooLong),
            )))),
        }
    }
    fn observe(&self, _: &WorkHandle) -> Settled<Result<RunObservation, ExecutorPortError>> {
        Settled::InFlight
    }
    fn cancel(&self, _: &WorkHandle) -> Settled<Result<(), ExecutorPortError>> {
        Settled::Answered(Ok(()))
    }
    fn observe_writes(&self) -> Settled<Vec<ObservedLaneWrites>> {
        Settled::Answered(Vec::new())
    }
}

fn fixture() -> (SqliteStore, PrecheckPayload, PrecheckState) {
    let payload = PrecheckPayload {
        bloom: digest(1),
        node: PrecheckNode { plan: digest(2), tree: digest(3), head: digest(4), gate_set: digest(5) },
        transformation: Transformation::for_aggregate_verify(
            &StageCatalog::binding_of(StageId::AggregateVerify),
            digest(3),
            digest(4),
            digest(6),
        ),
        profile: StageCatalog::profile_of(StageId::AggregateVerify),
        configs: ConfigRegistry::default(),
    };
    let mut state = PrecheckState::new(PrecheckPolicy { run_budget: 3 });
    state.prepared = Some(payload.node.clone());
    state.issued = Some(payload.node.clone());
    state.issued_runs = 1;
    (SqliteStore::open(":memory:").unwrap(), payload, state)
}

fn journal(store: &mut SqliteStore, event: &Event, effects: Vec<Decision>) {
    store
        .append_event(&JournalWrite {
            idempotency_key: &event.idempotency_key.0,
            event: &to_vec(event).unwrap(),
            decisions: &to_vec(&Decisions {
                outcome: Outcome::PrecheckPrepared { bloom: BloomId(digest(1)), node: digest(2) },
                effects,
            })
            .unwrap(),
            decider: "precheck-host-test",
        })
        .unwrap();
}

fn project(store: &mut SqliteStore, state: Option<PrecheckState>, key: &str) -> PrecheckProjection {
    journal(
        store,
        &Event {
            idempotency_key: IdempotencyKey(key.to_owned()),
            fact: Fact::RequestPrecheck { bloom: BloomId(digest(1)), node: digest(2) },
        },
        vec![Decision::RecordPrecheckState { bloom: BloomId(digest(1)), state }],
    );
    let mut projection = PrecheckProjection::default();
    assert!(projection.refresh(store).unwrap());
    projection
}

fn enqueue(store: &mut SqliteStore, payload: &PrecheckPayload) -> OutboxEntry {
    store.enqueue_topic(Topic::DispatchPrecheck, &to_vec(payload).unwrap(), None).unwrap();
    store.drain_topic(Topic::DispatchPrecheck).unwrap().remove(0)
}

#[test]
fn a_later_malformed_entry_does_not_lose_an_already_started_handle() {
    let (mut store, payload, state) = fixture();
    let projection = project(&mut store, Some(state), "initial");
    let entry = enqueue(&mut store, &payload);
    store.enqueue_topic(Topic::OfferPrecheck, &[255], None).unwrap();
    let port = Port::new();
    port.idle.set(true);
    let (handles, admits) = drain_prechecks(&mut store, None, &port, &projection, 1).unwrap();
    assert_eq!(handles.len(), 1);
    assert_eq!(handles[0].nonce, dispatch_nonce(entry.sequence));
    assert!(admits.is_empty());
    assert!(store.drain_topic(Topic::DispatchPrecheck).unwrap().is_empty());
    assert_eq!(store.drain_topic(Topic::OfferPrecheck).unwrap().len(), 1);
    assert_eq!(store.lookup_order(&handles[0].nonce.0).unwrap().unwrap().lifecycle, OrderLifecycle::Submitted);
}

#[test]
fn a_busy_precheck_remains_pending_without_an_order_or_required_submission() {
    let (mut store, payload, state) = fixture();
    let projection = project(&mut store, Some(state), "initial");
    let entry = enqueue(&mut store, &payload);
    let port = Port::new();
    let (handles, admits) = drain_prechecks(&mut store, None, &port, &projection, 1).unwrap();
    assert!(handles.is_empty() && admits.is_empty());
    assert!(store.lookup_order(&dispatch_nonce(entry.sequence).0).unwrap().is_none());
    assert_eq!(store.drain_topic(Topic::DispatchPrecheck).unwrap().len(), 1);
    assert_eq!(port.required_calls.get(), 0);
    port.idle.set(true);
    let (handles, _) = drain_prechecks(&mut store, None, &port, &projection, 2).unwrap();
    assert_eq!(handles.len(), 1);
    assert_eq!(store.lookup_order(&handles[0].nonce.0).unwrap().unwrap().lifecycle, OrderLifecycle::Submitted);
}

#[test]
fn an_obsolete_submission_waits_for_its_worker_then_replays_its_skip_until_journaled() {
    let (mut store, payload, mut state) = fixture();
    let entry = enqueue(&mut store, &payload);
    record_dispatch(&mut store, &record(&entry, &payload)).unwrap();
    state.prepared = None;
    let projection = project(&mut store, Some(state), "obsolete");
    let port = Port::new();
    port.probe.set(Probe::Pending);
    let (handles, admits) = drain_prechecks(&mut store, None, &port, &projection, 1).unwrap();
    assert!(handles.is_empty() && admits.is_empty());
    assert!(store.lookup_order(&dispatch_nonce(entry.sequence).0).unwrap().is_some());
    port.probe.set(Probe::Absent);
    let (_, first) = drain_prechecks(&mut store, None, &port, &projection, 2).unwrap();
    let (_, replay) = drain_prechecks(&mut store, None, &port, &projection, 3).unwrap();
    assert_eq!(first[0].event, replay[0].event);
    let event: Event = from_bytes(&first[0].event).unwrap();
    assert!(matches!(event.fact, Fact::PrecheckCompleted { completion: PrecheckCompletion::SkippedBeforeStart, .. }));
    assert_eq!(port.idle_calls.get(), 0);
    assert_eq!(port.required_calls.get(), 0);
    assert_eq!(store.drain_topic(Topic::DispatchPrecheck).unwrap().len(), 1);
    journal(&mut store, &event, Vec::new());
    drain_prechecks(&mut store, None, &port, &projection, 4).unwrap();
    assert!(store.drain_topic(Topic::DispatchPrecheck).unwrap().is_empty());
}

#[test]
fn an_obsolete_worker_that_started_keeps_its_handle_and_never_reports_a_skip() {
    let (mut store, payload, mut state) = fixture();
    let entry = enqueue(&mut store, &payload);
    record_dispatch(&mut store, &record(&entry, &payload)).unwrap();
    state.prepared = None;
    let projection = project(&mut store, Some(state), "obsolete");
    let port = Port::new();
    port.probe.set(Probe::Running);
    let (handles, admits) = drain_prechecks(&mut store, None, &port, &projection, 1).unwrap();
    assert_eq!(handles.len(), 1);
    assert!(admits.is_empty());
    assert_eq!(store.lookup_order(&handles[0].nonce.0).unwrap().unwrap().lifecycle, OrderLifecycle::Submitted);
    assert_eq!(port.idle_calls.get() + port.required_calls.get(), 0);
}

#[test]
fn a_final_join_settles_idle_submission_before_becoming_required() {
    for (probe, expected_required) in [(Probe::Absent, 1), (Probe::Running, 0)] {
        let (mut store, payload, mut state) = fixture();
        let entry = enqueue(&mut store, &payload);
        record_dispatch(&mut store, &record(&entry, &payload)).unwrap();
        let nonce = dispatch_nonce(entry.sequence);
        let deadline = store.lookup_order(&nonce.0).unwrap().unwrap().deadline_unix_millis;
        state.final_join = Some(payload.node.clone());
        state.promoted = true;
        let projection = project(&mut store, Some(state), "joined");
        let port = Port::new();
        port.probe.set(Probe::Pending);
        assert!(drain_prechecks(&mut store, None, &port, &projection, 1).unwrap().0.is_empty());
        assert_eq!(port.required_calls.get(), 0);
        port.probe.set(probe);
        assert_eq!(drain_prechecks(&mut store, None, &port, &projection, 2).unwrap().0.len(), 1);
        assert_eq!(port.required_calls.get(), expected_required);
        assert_eq!(port.idle_calls.get(), 0);
        assert_eq!(
            store.lookup_order(&nonce.0).unwrap().unwrap().deadline_unix_millis,
            deadline,
            "joining preserves the original execution limit"
        );
    }
}

#[test]
fn a_promoted_submission_failure_reaches_the_precheck_fault_path() {
    let (mut store, payload, mut state) = fixture();
    state.final_join = Some(payload.node.clone());
    state.promoted = true;
    let projection = project(&mut store, Some(state), "joined-fault");
    let entry = enqueue(&mut store, &payload);
    let order = record(&entry, &payload);
    record_dispatch(&mut store, &order).unwrap();
    let port = Port::new();
    port.probe.set(Probe::Fault);
    let dir = tempfile::tempdir().unwrap();
    let mut artifacts = ArtifactsCapabilityState::open(dir.path()).unwrap();
    let (handles, admits) = drain_prechecks(&mut store, Some(&mut artifacts), &port, &projection, 2).unwrap();
    assert!(handles.is_empty());
    assert_eq!(admits.len(), 1);
    let event: Event = from_bytes(&admits[0].event).unwrap();
    assert!(
        matches!(event.fact, Fact::PrecheckCompleted { node, completion: PrecheckCompletion::HostFault(_), .. } if node == payload.node.digest())
    );
    assert!(store.lookup_order(&order.nonce.0).unwrap().is_none());
    assert_eq!(port.required_calls.get(), 0, "settled failure cannot dispatch a replacement before journal admission");
}

#[test]
fn a_precheck_red_routes_to_its_node_and_only_a_final_join_restores_its_findings() {
    let (mut store, payload, mut state) = fixture();
    let entry = enqueue(&mut store, &payload);
    let order = record(&entry, &payload);
    record_dispatch(&mut store, &order).unwrap();
    store.mark_order_submitted(&order.nonce.0).unwrap();
    let upload = UploadedEvidence {
        nonce: order.nonce.clone(),
        subject: payload.node.tree,
        detail: digest(9),
        verdict: StageVerdict::VerificationFailed,
        observation: LaneObservation {
            findings: Some("interaction compiler error".to_owned()),
            ..LaneObservation::default()
        },
    };
    let AdmitDecision::Admitted(admission) = admit_uploaded(&mut store, &upload).unwrap() else {
        panic!("expected admission")
    };
    assert!(
        matches!(admission.event.fact, Fact::PrecheckCompleted { node, completion: PrecheckCompletion::Failed(_) , .. } if node == payload.node.digest())
    );
    assert!(store.lookup_review_findings(payload.bloom.as_bytes(), WorkpieceId::COMPOSITION).unwrap().is_none());
    assert_eq!(
        store
            .lookup_review_findings(payload.bloom.as_bytes(), &findings_key(payload.node.digest()))
            .unwrap()
            .as_deref(),
        Some("interaction compiler error")
    );
    state.issued = None;
    state.result = Some(PrecheckResult::Failed { node: payload.node.digest(), evidence: digest(9) });
    let mut projection = project(&mut store, Some(state.clone()), "red");
    projection.prepare_dispatch(&mut store).unwrap();
    assert!(store.lookup_review_findings(payload.bloom.as_bytes(), WorkpieceId::COMPOSITION).unwrap().is_none());
    state.final_join = Some(payload.node);
    let mut projection = project(&mut store, Some(state), "final-red");
    projection.prepare_dispatch(&mut store).unwrap();
    projection.prepare_dispatch(&mut store).unwrap();
    let findings = store.lookup_review_findings(payload.bloom.as_bytes(), WorkpieceId::COMPOSITION).unwrap().unwrap();
    assert_eq!(findings.matches("interaction compiler error").count(), 1);
}

#[test]
fn a_joined_unstarted_order_cannot_dispatch_through_an_operator_hold() {
    let (mut store, payload, mut state) = fixture();
    let entry = enqueue(&mut store, &payload);
    state.final_join = Some(payload.node.clone());
    state.promoted = true;
    state.paused = true;
    let projection = project(&mut store, Some(state), "held-join");
    let port = Port::new();
    let (handles, admits) = drain_prechecks(&mut store, None, &port, &projection, 1).unwrap();
    assert!(handles.is_empty());
    assert_eq!(port.required_calls.get() + port.idle_calls.get(), 0);
    assert!(store.lookup_order(&dispatch_nonce(entry.sequence).0).unwrap().is_none());
    assert!(matches!(
        from_bytes::<Event>(&admits[0].event).unwrap().fact,
        Fact::PrecheckCompleted { completion: PrecheckCompletion::SkippedBeforeStart, .. }
    ));
}

#[test]
fn a_precheck_host_fault_is_admitted_without_a_member_repair_or_red_finding() {
    let (mut store, payload, _) = fixture();
    let entry = enqueue(&mut store, &payload);
    let order = record(&entry, &payload);
    record_dispatch(&mut store, &order).unwrap();
    store.mark_order_submitted(&order.nonce.0).unwrap();
    let upload = UploadedEvidence {
        nonce: order.nonce,
        subject: payload.node.tree,
        detail: digest(9),
        verdict: StageVerdict::ExecutorFault,
        observation: LaneObservation { findings: Some("host fault".to_owned()), ..LaneObservation::default() },
    };
    let AdmitDecision::Admitted(admission) = admit_uploaded(&mut store, &upload).unwrap() else {
        panic!("expected host fault admission")
    };
    assert!(
        matches!(admission.event.fact, Fact::PrecheckCompleted { node, completion: PrecheckCompletion::HostFault(_), .. } if node == payload.node.digest())
    );
    assert!(store.lookup_review_findings(payload.bloom.as_bytes(), WorkpieceId::COMPOSITION).unwrap().is_none());
    assert!(
        store.lookup_review_findings(payload.bloom.as_bytes(), &findings_key(payload.node.digest())).unwrap().is_none()
    );
}

#[test]
fn a_lost_completion_replays_without_recreating_a_consumed_precheck_order() {
    for submitted in [false, true] {
        let (mut store, payload, state) = fixture();
        let projection = project(&mut store, Some(state), "issued");
        let entry = enqueue(&mut store, &payload);
        let order = record(&entry, &payload);
        record_dispatch(&mut store, &order).unwrap();
        if submitted {
            store.mark_order_submitted(&order.nonce.0).unwrap();
            store.ack_topic(Topic::DispatchPrecheck, entry.sequence).unwrap();
        }
        let upload = UploadedEvidence {
            nonce: order.nonce.clone(),
            subject: payload.node.tree,
            detail: digest(9),
            verdict: StageVerdict::ExecutorFault,
            observation: LaneObservation::default(),
        };
        let AdmitDecision::Admitted(admission) = admit_uploaded(&mut store, &upload).unwrap() else {
            panic!("expected completion")
        };
        assert!(store.lookup_order(&order.nonce.0).unwrap().is_none());
        let port = Port::new();
        port.idle.set(true);
        let (handles, replayed) = drain_prechecks(&mut store, None, &port, &projection, 2).unwrap();
        assert!(handles.is_empty());
        assert_eq!(replayed[0].event, admission.admit.event);
        assert_eq!(port.idle_calls.get() + port.required_calls.get(), 0);
        journal(&mut store, &admission.event, Vec::new());
        drain_prechecks(&mut store, None, &port, &projection, 3).unwrap();
        assert!(store.drain_topic(Topic::DispatchPrecheck).unwrap().is_empty());
    }
}

#[test]
fn restart_discovers_prechecks_even_after_every_dispatch_row_was_acknowledged() {
    let (mut store, payload, state) = fixture();
    project(&mut store, Some(state.clone()), "issued");
    let entry = enqueue(&mut store, &payload);
    store.ack_topic(Topic::DispatchPrecheck, entry.sequence).unwrap();
    assert!(store.drain_topic(Topic::DispatchPrecheck).unwrap().is_empty());
    let mut restarted = PrecheckProjection::default();
    assert!(restarted.prepare_dispatch(&mut store).unwrap());
    assert_eq!(restarted.get(&BloomId(payload.bloom)), Some(&state));
}

#[test]
fn the_projection_replays_recorded_state_incrementally_and_clears_terminal_blooms() {
    let (mut store, _, state) = fixture();
    let mut projection = project(&mut store, Some(state.clone()), "initial");
    assert_eq!(projection.get(&BloomId(digest(1))), Some(&state));
    assert!(projection.refresh(&mut store).unwrap());
    assert_eq!(projection.states().count(), 1);
    project(&mut store, None, "terminal");
    assert!(projection.refresh(&mut store).unwrap());
    assert!(projection.is_empty());
    let mut restarted = PrecheckProjection::default();
    assert!(restarted.refresh(&mut store).unwrap());
    assert!(restarted.is_empty());
}
