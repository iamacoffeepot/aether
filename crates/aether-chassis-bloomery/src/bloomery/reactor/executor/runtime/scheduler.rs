//! Durable oldest-ready grouping for opted-in member verification.

use std::{
    collections::{BTreeMap, BTreeSet},
    slice::from_ref,
};

use aether_bloomery::{
    Admit, BloomId, CompatibilityPreview, CompositionPlan, CoordinationState, Decision, Digest, Event, Fact,
    IdempotencyKey, MemberContractPin, MemberVerificationPayload, Nonce, SharedRunMode, SharedRunPlan, Topic,
    VerificationMode, WorkOrder, decode_recorded_decisions,
};
use aether_data::wire::{from_bytes, to_vec};

use crate::bloomery::executor::ExecutorPort;
use crate::bloomery::outbox::{OutboxResultDelivery, TopicOutbox};
use crate::store::{QueuedMemberVerificationRow, StoreBackend};

const JOURNAL_PAGE_ROWS: u32 = 256;
const MAX_PAGES_PER_TURN: usize = 8;
const MAX_STALE_RETIREMENTS_PER_TURN: usize = 256;

#[derive(Default)]
pub(super) struct MemberVerificationScheduler {
    cursor: u64,
    states: BTreeMap<BloomId, CoordinationState>,
    #[cfg(any(test, feature = "testing"))]
    proposals_held: bool,
}

impl MemberVerificationScheduler {
    #[cfg(any(test, feature = "testing"))]
    pub(super) fn set_proposals_held(&mut self, held: bool) {
        self.proposals_held = held;
    }

    pub(super) fn refresh(&mut self, store: &mut dyn StoreBackend) -> rusqlite::Result<bool> {
        for _ in 0..MAX_PAGES_PER_TURN {
            let rows = store.replay_journal_after(self.cursor, JOURNAL_PAGE_ROWS)?;
            let caught_up = rows.len() < JOURNAL_PAGE_ROWS as usize;
            for row in rows {
                let decisions = decode_recorded_decisions(&row.decisions, row.decisions_schema_digest.as_deref())
                    .map_err(|error| rusqlite::Error::InvalidParameterName(format!("coordination replay: {error}")))?;
                for effect in decisions.effects {
                    if let Decision::RecordCoordinationState { bloom, state } = effect {
                        match state {
                            Some(state) => {
                                self.states.insert(bloom, *state);
                            }
                            None => {
                                self.states.remove(&bloom);
                            }
                        }
                    }
                }
                self.cursor = row.sequence;
            }
            if caught_up {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub(super) fn retains_construction_admission(
        &self,
        dispatch: &aether_bloomery::ContextualAttemptDispatch,
        nonce: Digest,
    ) -> bool {
        self.states
            .get(&dispatch.bloom)
            .and_then(|state| state.admitted_construction.get(&dispatch.workpiece.0))
            .is_some_and(|admission| admission.nonce == nonce && admission.dispatch == *dispatch)
    }
}

fn proposal_capacity_order(plan: &SharedRunPlan) -> Option<WorkOrder> {
    let transformation = match &plan.composition {
        Some(composition) => composition.contract.invocation.instantiate(composition.base.candidate),
        None => plan.requests.first()?.transformation.clone(),
    };
    Some(WorkOrder {
        transformation,
        nonce: Nonce(format!("shared-capacity:{}", plan.digest().to_hex())),
        instruction_bundle: None,
        prompt_manifest: None,
        physical_run: Some(plan.digest()),
        release_physical_run: false,
    })
}

fn build_plan(state: &CoordinationState, requests: Vec<aether_bloomery::MemberVerifyRequest>) -> SharedRunPlan {
    let mode = match state.policy.verification {
        VerificationMode::Contextual if requests.iter().all(|request| state.composition_contract.covers(request)) => {
            SharedRunMode::Contextual
        }
        VerificationMode::Standalone | VerificationMode::Contextual => SharedRunMode::Standalone,
        VerificationMode::WarmSerial => SharedRunMode::WarmSerial,
    };
    let composition = (mode == SharedRunMode::Contextual).then(|| {
        let mut seen = BTreeSet::new();
        let inputs = requests
            .iter()
            .filter_map(|request| seen.insert(request.input.digest()).then_some(request.input.clone()))
            .collect();
        let members = requests
            .iter()
            .map(|request| MemberContractPin { request: request.digest(), contract: request.contract.digest() })
            .collect();
        CompositionPlan {
            bloom: state.integration.generation.bloom,
            base: state.integration.head.clone(),
            inputs,
            requests: requests.clone(),
            contract: state.composition_contract.bind(members),
        }
    });
    let prototype = SharedRunPlan {
        mode,
        requests,
        composition,
        probe_budget: if mode == SharedRunMode::Contextual {
            state.policy.max_attribution_probes
        } else {
            0
        },
        execution_attempt: 0,
    };
    SharedRunPlan { execution_attempt: state.next_execution_attempt(&prototype), ..prototype }
}

fn cap(state: &CoordinationState) -> usize {
    match state.policy.verification {
        VerificationMode::Standalone => 1,
        VerificationMode::WarmSerial => state.policy.max_run_members.min(state.policy.max_serial_requests) as usize,
        VerificationMode::Contextual => state.policy.max_run_members as usize,
    }
}

trait SharedRunSelection {
    fn select(&self, state: &CoordinationState, queued: &[QueuedMemberVerificationRow]) -> Vec<usize>;
}

struct SealedPolicySelection;

fn current_request(
    state: &CoordinationState,
    row: &QueuedMemberVerificationRow,
) -> Option<aether_bloomery::MemberVerifyRequest> {
    let payload = from_bytes::<MemberVerificationPayload>(&row.payload).ok()?;
    state
        .request(payload.request.digest())
        .filter(|current| *current == &payload.request)
        .filter(|current| {
            !state.runs.iter().any(|run| {
                !run.is_terminal() && run.plan.requests.iter().any(|request| request.digest() == current.digest())
            })
        })
        .cloned()
}

fn fresh_conflict(state: &CoordinationState, queued: &[QueuedMemberVerificationRow], selected: &[usize]) -> bool {
    let requests = selected.iter().filter_map(|index| current_request(state, &queued[*index])).collect::<Vec<_>>();
    let expected = requests.iter().map(|request| request.member.workpiece.clone()).collect::<BTreeSet<_>>();
    state.preview_plans.iter().any(|plan| {
        plan.bloom == state.integration.generation.bloom
            && plan.generation == state.integration.generation.digest()
            && plan.base == state.integration.head.candidate
            && plan.checkpoints.len() == expected.len()
            && plan.checkpoints.iter().all(|checkpoint| {
                expected.contains(&checkpoint.workpiece)
                    && state.checkpoints.get(&checkpoint.workpiece.0) == Some(checkpoint)
                    && requests.iter().any(|request| {
                        request.member.workpiece == checkpoint.workpiece
                            && request.member.scope_revision == checkpoint.scope_revision
                            && request.member.candidate == checkpoint.candidate
                            && request.input.candidate == checkpoint.candidate
                            && request.input.members.contains(&request.member)
                            && request.context.as_ref().is_some_and(|context| {
                                context.starting_head.candidate.checkout == checkpoint.starting_checkout
                            })
                    })
            })
            && state.previews.iter().any(|preview| {
                preview.plan == plan.digest() && matches!(&preview.result, CompatibilityPreview::Conflict { .. })
            })
    })
}

fn retained_survivor_group(
    state: &CoordinationState,
    queued: &[QueuedMemberVerificationRow],
    request: &aether_bloomery::MemberVerifyRequest,
    limit: usize,
) -> Option<Vec<usize>> {
    let request_digest = request.digest();
    let group = state.survivor_groups.iter().find(|group| group.requests.contains(&request_digest))?;
    if group.requests.len() > limit {
        return Some(Vec::new());
    }
    let Some(indices) = group
        .requests
        .iter()
        .map(|digest| {
            queued.iter().enumerate().find_map(|(index, row)| {
                current_request(state, row)
                    .filter(|candidate| candidate.bloom == request.bloom && candidate.digest() == *digest)
                    .map(|_| index)
            })
        })
        .collect::<Option<Vec<_>>>()
    else {
        return Some(Vec::new());
    };
    let selected = indices.iter().copied().collect::<BTreeSet<_>>();
    let atomic_group_is_complete = indices.iter().all(|index| {
        let Some(candidate) = current_request(state, &queued[*index]) else {
            return false;
        };
        state.composition_contract.covers(&candidate)
            && candidate.input.members.iter().all(|pin| {
                state.integration.head.coverage.contains(pin)
                    || queued.iter().enumerate().any(|(peer_index, row)| {
                        selected.contains(&peer_index)
                            && current_request(state, row).is_some_and(|peer| {
                                peer.bloom == candidate.bloom
                                    && peer.member == *pin
                                    && peer.input.digest() == candidate.input.digest()
                            })
                    })
            })
    });
    Some(if atomic_group_is_complete {
        indices
    } else {
        Vec::new()
    })
}

impl SharedRunSelection for SealedPolicySelection {
    fn select(&self, state: &CoordinationState, queued: &[QueuedMemberVerificationRow]) -> Vec<usize> {
        let Some(first_request) = current_request(state, &queued[0]) else {
            return Vec::new();
        };
        let limit = cap(state);
        if state.policy.verification == VerificationMode::Contextual
            && let Some(group) = retained_survivor_group(state, queued, &first_request, limit)
        {
            return group;
        }
        if state.policy.verification == VerificationMode::Contextual
            && !state.composition_contract.covers(&first_request)
        {
            return vec![0];
        }
        if state.policy.verification != VerificationMode::Contextual {
            return queued
                .iter()
                .enumerate()
                .filter_map(|(index, row)| {
                    let request = current_request(state, row)?;
                    (request.bloom == first_request.bloom).then_some(index)
                })
                .take(limit)
                .collect();
        }
        let mut selected = Vec::new();
        let mut inputs = BTreeSet::new();
        for row in queued {
            let Some(request) = current_request(state, row).filter(|request| request.bloom == first_request.bloom)
            else {
                continue;
            };
            if !state.composition_contract.covers(&request) {
                continue;
            }
            if state.survivor_groups.iter().any(|group| group.requests.contains(&request.digest())) {
                break;
            }
            if inputs.contains(&request.input.digest()) {
                continue;
            }
            let peers = request
                .input
                .members
                .iter()
                .filter_map(|pin| {
                    queued.iter().enumerate().find_map(|(peer_index, peer)| {
                        current_request(state, peer)
                            .filter(|candidate| {
                                candidate.bloom == request.bloom
                                    && candidate.member == *pin
                                    && candidate.input.digest() == request.input.digest()
                            })
                            .map(|_| peer_index)
                    })
                })
                .collect::<Vec<_>>();
            let missing = request.input.members.iter().any(|pin| {
                !state.integration.head.coverage.contains(pin)
                    && !peers.iter().any(|peer| {
                        current_request(state, &queued[*peer]).is_some_and(|candidate| candidate.member == *pin)
                    })
            });
            if missing || selected.len().saturating_add(peers.len()) > limit {
                if selected.is_empty() {
                    return Vec::new();
                }
                break;
            }
            inputs.insert(request.input.digest());
            let previous_len = selected.len();
            for peer in peers {
                if !selected.contains(&peer) {
                    selected.push(peer);
                }
            }
            if previous_len > 0 && fresh_conflict(state, queued, &selected) {
                selected.truncate(previous_len);
                break;
            }
            if selected.len() == limit {
                break;
            }
        }
        selected.sort_unstable();
        selected
    }
}

fn proposal_event(bytes: &[u8]) -> rusqlite::Result<Event> {
    from_bytes(bytes).map_err(|error| rusqlite::Error::InvalidParameterName(error.to_string()))
}

fn replay_proposal(
    store: &mut dyn StoreBackend,
    queued: &[QueuedMemberVerificationRow],
    proposal: &[u8],
) -> rusqlite::Result<Vec<Admit>> {
    let event = proposal_event(proposal)?;
    let rows = queued.iter().filter(|row| row.proposal.as_deref() == Some(proposal)).collect::<Vec<_>>();
    let mut all_journaled = true;
    let mut admits = Vec::new();
    for row in &rows {
        match store.replay_topic_results(Topic::MemberVerification, row.sequence)? {
            OutboxResultDelivery::Unrecorded => {
                store.record_topic_results(Topic::MemberVerification, row.sequence, from_ref(&event))?;
                all_journaled = false;
            }
            OutboxResultDelivery::Pending(pending) => {
                if admits.is_empty() {
                    admits.extend(pending);
                }
                all_journaled = false;
            }
            OutboxResultDelivery::Journaled => {}
        }
    }
    if all_journaled {
        let requests = rows.iter().map(|row| row.request.clone()).collect::<Vec<_>>();
        store.mark_queued_member_verifications_scheduled(&requests)?;
        if let Some(sequence) = rows.iter().map(|row| row.sequence).max() {
            store.ack_topic(Topic::MemberVerification, sequence)?;
        }
    } else if admits.is_empty() {
        admits.push(Admit { event: proposal.to_vec() });
    }
    Ok(admits)
}

pub(super) fn drain_member_verifications(
    scheduler: &mut MemberVerificationScheduler,
    store: &mut dyn StoreBackend,
    executor: &dyn ExecutorPort,
    now_unix_millis: u64,
) -> rusqlite::Result<Vec<Admit>> {
    if !scheduler.refresh(store)? {
        return Ok(Vec::new());
    }
    for entry in store.drain_topic(Topic::MemberVerification)? {
        let Ok(payload) = from_bytes::<MemberVerificationPayload>(&entry.payload) else {
            break;
        };
        let request = payload.request.digest();
        let row = QueuedMemberVerificationRow {
            request: request.as_bytes().to_vec(),
            sequence: entry.sequence,
            payload: entry.payload,
            queued_unix_millis: now_unix_millis,
            deadline_unix_millis: now_unix_millis
                .saturating_add(payload.request.transformation.limits.wall_clock_secs.saturating_mul(1_000)),
            scheduled: false,
            proposal: None,
        };
        store.record_queued_member_verification(&row)?;
        if store.queued_member_verification(&row.request)?.is_some_and(|stored| stored.scheduled) {
            store.ack_topic(Topic::MemberVerification, entry.sequence)?;
        }
    }

    let mut queued = store.queued_member_verifications()?;
    let mut retired = 0usize;
    let first = loop {
        let Some(first) = queued.first() else {
            return Ok(Vec::new());
        };
        if first.proposal.is_some() {
            break first;
        }
        let first_payload = from_bytes::<MemberVerificationPayload>(&first.payload)
            .map_err(|error| rusqlite::Error::InvalidParameterName(error.to_string()))?;
        let current = scheduler
            .states
            .get(&first_payload.request.bloom)
            .is_some_and(|state| current_request(state, first).is_some());
        if current {
            break first;
        }
        store.mark_queued_member_verifications_scheduled(from_ref(&first.request))?;
        store.ack_topic(Topic::MemberVerification, first.sequence)?;
        queued.remove(0);
        retired = retired.saturating_add(1);
        if retired == MAX_STALE_RETIREMENTS_PER_TURN {
            return Ok(Vec::new());
        }
    };
    if let Some(proposal) = first.proposal.as_deref() {
        return replay_proposal(store, &queued, proposal);
    }
    let first_payload = from_bytes::<MemberVerificationPayload>(&first.payload)
        .map_err(|error| rusqlite::Error::InvalidParameterName(error.to_string()))?;
    let Some(state) = scheduler.states.get(&first_payload.request.bloom) else {
        return Err(rusqlite::Error::InvalidParameterName(
            "current member verification lost its coordination state".to_owned(),
        ));
    };
    let indices = SealedPolicySelection.select(state, &queued);
    if indices.is_empty() {
        return Ok(Vec::new());
    }
    let rows = indices.iter().map(|index| &queued[*index]).collect::<Vec<_>>();
    let selected = rows.iter().filter_map(|row| current_request(state, row)).collect::<Vec<_>>();
    let plan = build_plan(state, selected);
    let Some(capacity_order) = proposal_capacity_order(&plan) else {
        return Ok(Vec::new());
    };
    #[cfg(any(test, feature = "testing"))]
    if scheduler.proposals_held {
        return Ok(Vec::new());
    }
    if !executor.has_idle_capacity(&capacity_order) {
        return Ok(Vec::new());
    }
    let event = Event {
        idempotency_key: IdempotencyKey(format!("aether.bloomery.propose_shared_run:{}", plan.digest().to_hex())),
        fact: Fact::ProposeSharedRun { bloom: first_payload.request.bloom, plan },
    };
    let proposal = to_vec(&event).map_err(|error| rusqlite::Error::InvalidParameterName(error.to_string()))?;
    let requests = rows.iter().map(|row| row.request.clone()).collect::<Vec<_>>();
    store.record_member_verification_proposal(&requests, &proposal)?;
    let queued = store.queued_member_verifications()?;
    replay_proposal(store, &queued, &proposal)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use aether_bloomery::{
        AgentProfile, BackendId, CandidateRef, CompatibilityPreviewPlan, CompatibilityPreviewRecord,
        CompositionContractTemplate, CompositionInput, ConfigRegistry, ConstructContext, ConstructionAdmissionPayload,
        ConstructionCheckpoint, ContextualAttemptDispatch, ContextualDispatchPayload, ContextualInvocationTemplate,
        CoordinationPolicy, Digest, ExecutionLimits, GenerationMember, Harness, MemberPin, MemberVerifyRequest,
        NetworkProfile, ObservedLaneWrites, ReasoningEffort, StageId, ToolPolicy, Transformation, VerificationContract,
        VerificationMode, VerificationObligation, WorkHandle, WorkOrder, WorkpieceId,
    };

    use super::*;
    use crate::bloomery::executor::{ExecutorPort, ExecutorPortError, RunObservation, Settled};
    use crate::store::{RecordOutcome, SqliteStore};

    struct CapacityPort(Cell<bool>);

    impl ExecutorPort for CapacityPort {
        fn backend_for(&self, _: &WorkHandle) -> BackendId {
            BackendId::SOLE
        }

        fn submit(&self, _: &WorkOrder) -> Settled<Result<WorkHandle, ExecutorPortError>> {
            Settled::InFlight
        }

        fn has_idle_capacity(&self, _: &WorkOrder) -> bool {
            self.0.get()
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

    fn digest(value: u8) -> Digest {
        Digest::of_wire_bytes(&[value])
    }

    fn candidate(value: u8) -> CandidateRef {
        CandidateRef { tree: digest(value), checkout: digest(value.saturating_add(64)) }
    }

    fn profile() -> AgentProfile {
        AgentProfile {
            harness: Harness::Grok,
            model: "test".to_owned(),
            effort: ReasoningEffort::Low,
            tools: ToolPolicy::None,
        }
    }

    fn pin(name: &str, value: u8) -> MemberPin {
        MemberPin {
            workpiece: WorkpieceId(name.to_owned()),
            scope_revision: digest(value),
            candidate: candidate(value),
        }
    }

    fn request(bloom: BloomId, member: &MemberPin, input: CompositionInput, base: CandidateRef) -> MemberVerifyRequest {
        let transformation = Transformation {
            command: "verify.member".to_owned(),
            inputs: vec![member.candidate.tree],
            checkout: member.candidate.checkout,
            diff_base: Some(base.checkout),
            outputs: Vec::new(),
            image: "verify".to_owned(),
            limits: ExecutionLimits { wall_clock_secs: 60 },
            network: NetworkProfile::None,
            description: None,
            model: None,
        };
        MemberVerifyRequest {
            bloom,
            member: member.clone(),
            input,
            attempt: 0,
            context: None,
            contract: VerificationContract {
                gate_set: digest(40),
                obligations: vec![
                    VerificationObligation::Gate { identity: "verify.clippy".to_owned() },
                    VerificationObligation::MemberDelta {
                        scope_revision: member.scope_revision,
                        candidate: member.candidate,
                        diff_base: base,
                    },
                ],
                diff_base: base,
                invocation: digest(41),
                environment: digest(42),
                host_class: digest(43),
            },
            transformation,
            profile: profile(),
            configs: ConfigRegistry::default(),
        }
    }

    fn state(requests: Vec<MemberVerifyRequest>, limit: u32) -> CoordinationState {
        let bloom = requests[0].bloom;
        let base = candidate(1);
        let mut state = CoordinationState::new(
            CoordinationPolicy {
                verification: VerificationMode::Contextual,
                eager_integration: true,
                max_run_members: limit,
                max_serial_requests: limit,
                max_attribution_probes: 4,
                movement_budget: 2,
                reservation_millis: 1_000,
                host_class: "test-host".to_owned(),
            },
            CompositionContractTemplate {
                gate_set: digest(50),
                gate_identities: vec!["verify.clippy".to_owned()],
                invocation: ContextualInvocationTemplate {
                    command: "verify.check".to_owned(),
                    extra_inputs: Vec::new(),
                    diff_base: Some(base.checkout),
                    outputs: Vec::new(),
                    image: "verify".to_owned(),
                    limits: ExecutionLimits { wall_clock_secs: 60 },
                    network: NetworkProfile::None,
                    description: None,
                    model: None,
                    profile: profile(),
                    configs: ConfigRegistry::default(),
                },
                environment: digest(42),
                host_class: digest(43),
            },
            bloom,
            base,
            requests
                .iter()
                .map(|request| GenerationMember {
                    workpiece: request.member.workpiece.clone(),
                    scope_revision: request.member.scope_revision,
                })
                .collect(),
        );
        state.requests = requests;
        state
    }

    fn queued(request: &MemberVerifyRequest, sequence: u64) -> QueuedMemberVerificationRow {
        QueuedMemberVerificationRow {
            request: request.digest().as_bytes().to_vec(),
            sequence,
            payload: to_vec(&MemberVerificationPayload { request: request.clone() }).expect("queue payload"),
            queued_unix_millis: 1_000,
            deadline_unix_millis: 61_000,
            scheduled: false,
            proposal: None,
        }
    }

    fn fixture() -> (CoordinationState, Vec<QueuedMemberVerificationRow>) {
        let bloom = BloomId(digest(2));
        let base = candidate(1);
        let alpha = pin("alpha", 10);
        let beta = pin("beta", 11);
        let charlie = pin("charlie", 12);
        let repaired = CompositionInput {
            node: digest(20),
            candidate: candidate(20),
            members: vec![alpha.clone(), charlie.clone()],
        };
        let alpha = request(bloom, &alpha, repaired.clone(), base);
        let beta = request(
            bloom,
            &beta,
            CompositionInput { node: beta.candidate.tree, candidate: beta.candidate, members: vec![beta.clone()] },
            base,
        );
        let charlie = request(bloom, &charlie, repaired, base);
        let rows = vec![queued(&alpha, 1), queued(&beta, 2), queued(&charlie, 3)];
        (state(vec![alpha, beta, charlie], 2), rows)
    }

    #[test]
    fn repaired_atomic_input_is_selected_across_an_interleaved_request() {
        let (state, queued) = fixture();

        assert_eq!(SealedPolicySelection.select(&state, &queued), vec![0, 2]);
    }

    #[test]
    fn retained_survivors_are_selected_together_in_their_frozen_order() {
        let (mut state, queued) = fixture();
        state.policy.max_run_members = 3;
        state.survivor_groups.push(aether_bloomery::SurvivorGroup {
            source_plan: digest(90),
            source_node: digest(91),
            requests: vec![state.requests[2].digest(), state.requests[0].digest()],
        });

        assert_eq!(SealedPolicySelection.select(&state, &queued), vec![2, 0]);
    }

    #[test]
    fn busy_capacity_defers_construction_admission_until_the_advanced_head_is_current() {
        let (mut state, _) = fixture();
        let request = state.requests[0].clone();
        let advanced = candidate(100);
        state.integration.head.node = advanced.tree;
        state.integration.head.candidate = advanced;
        let dispatch = ContextualAttemptDispatch {
            bloom: request.bloom,
            workpiece: request.member.workpiece,
            stage: StageId::Construct,
            attempt: 0,
            transformation: Transformation { checkout: advanced.checkout, ..request.transformation },
            scope_revision: request.member.scope_revision,
            candidate: Some(request.member.candidate.tree),
            profile: request.profile,
            configs: request.configs,
            context: ConstructContext {
                bloom_base: state.integration.generation.base,
                starting_head: state.integration.head.clone(),
            },
        };
        let mut scheduler = MemberVerificationScheduler::default();
        scheduler.states.insert(dispatch.bloom, state);
        let payload = to_vec(&ConstructionAdmissionPayload { dispatch: dispatch.clone() }).expect("payload");
        let mut store = SqliteStore::open(":memory:").expect("store");
        store.enqueue_topic(Topic::ConstructionAdmission, &payload, None).expect("queue admission");
        let capacity = CapacityPort(Cell::new(false));

        assert!(
            super::super::shared::drain_construction_admissions(&scheduler, &mut store, &capacity, 1_000)
                .expect("busy admission")
                .is_empty()
        );
        assert!(store.construction_admission_nonce(dispatch.digest().as_bytes()).expect("lookup").is_none());

        capacity.0.set(true);
        let admits = super::super::shared::drain_construction_admissions(&scheduler, &mut store, &capacity, 1_000)
            .expect("released capacity");
        let event = from_bytes::<Event>(&admits[0].event).expect("admission event");
        let Fact::RequestConstructionAdmission { admission } = event.fact else {
            panic!("expected construction admission");
        };
        assert_eq!(admission.dispatch, dispatch);
        assert_eq!(admission.dispatch.context.starting_head.candidate, advanced);
        let retained = store
            .construction_admission(dispatch.digest().as_bytes())
            .expect("admission lookup")
            .expect("admission clocks");
        assert_eq!(retained.queued_unix_millis, 1_000);
        assert_eq!(retained.deadline_unix_millis, 61_000);
    }

    #[test]
    fn stale_unsubmitted_contextual_dispatch_retires_without_launching() {
        let (state, _) = fixture();
        let request = state.requests[0].clone();
        let dispatch = ContextualAttemptDispatch {
            bloom: request.bloom,
            workpiece: request.member.workpiece,
            stage: StageId::Construct,
            attempt: 0,
            transformation: request.transformation,
            scope_revision: request.member.scope_revision,
            candidate: Some(request.member.candidate.tree),
            profile: request.profile,
            configs: request.configs,
            context: ConstructContext {
                bloom_base: state.integration.generation.base,
                starting_head: state.integration.head,
            },
        };
        let scheduler = MemberVerificationScheduler::default();
        let mut store = SqliteStore::open(":memory:").expect("store");
        store
            .record_construction_admission(dispatch.digest().as_bytes(), "admission-nonce", 1_000, 61_000)
            .expect("retain admission");
        store
            .enqueue_topic(
                Topic::ContextualDispatch,
                &to_vec(&ContextualDispatchPayload { dispatch: dispatch.clone() }).expect("payload"),
                None,
            )
            .expect("queue contextual dispatch");
        let capacity = CapacityPort(Cell::new(true));

        assert!(
            super::super::shared::drain_contextual_dispatches(&scheduler, &mut store, None, &capacity, 2_000,)
                .expect("stale dispatch")
                .is_empty()
        );
        let retained =
            store.construction_admission(dispatch.digest().as_bytes()).expect("lookup").expect("admission tombstone");
        assert!(retained.retired);
        assert!(store.lookup_order("admission-nonce").expect("order lookup").is_none());
    }

    #[test]
    fn atomic_input_does_not_wait_for_an_exact_member_already_in_the_base_head() {
        let (mut state, queued) = fixture();
        let alpha = state.requests[0].member.clone();
        state.integration.head.coverage.push(alpha);
        state.requests.remove(0);

        assert_eq!(SealedPolicySelection.select(&state, &queued[2..]), vec![0]);
    }

    #[test]
    fn repaired_root_requests_are_reselected_even_when_their_pins_are_head_coverage() {
        let (mut state, queued) = fixture();
        state.integration.head.coverage = vec![state.requests[0].member.clone(), state.requests[2].member.clone()];

        assert_eq!(SealedPolicySelection.select(&state, &queued), vec![0, 2]);
    }

    #[test]
    fn stale_oldest_request_is_recognized_before_selection() {
        let (mut state, queued) = fixture();
        state.requests.remove(0);

        assert!(current_request(&state, &queued[0]).is_none());
        assert!(SealedPolicySelection.select(&state, &queued).is_empty());
    }

    #[test]
    fn incompatible_aggregate_contract_falls_back_to_one_standalone_request() {
        let (mut state, mut queued_rows) = fixture();
        state.requests[0].contract.environment = digest(90);
        queued_rows[0] = queued(&state.requests[0], queued_rows[0].sequence);

        let selected = SealedPolicySelection.select(&state, &queued_rows);
        let plan = build_plan(&state, vec![state.requests[0].clone()]);

        assert_eq!(selected, vec![0]);
        assert_eq!(plan.mode, SharedRunMode::Standalone);
        assert!(plan.composition.is_none());
    }

    #[test]
    fn warm_serial_keeps_mixed_member_invocations_in_one_bounded_lease() {
        let (mut state, mut queued_rows) = fixture();
        state.policy.verification = VerificationMode::WarmSerial;
        state.requests[0].contract.environment = digest(90);
        state.requests[1].profile.model = "different-member-profile".to_owned();
        queued_rows[0] = queued(&state.requests[0], queued_rows[0].sequence);
        queued_rows[1] = queued(&state.requests[1], queued_rows[1].sequence);

        let selected = SealedPolicySelection.select(&state, &queued_rows);
        let plan = build_plan(&state, vec![state.requests[0].clone(), state.requests[1].clone()]);

        assert_eq!(selected, vec![0, 1]);
        assert_eq!(plan.mode, SharedRunMode::WarmSerial);
        assert!(plan.composition.is_none());
    }

    #[test]
    fn busy_verification_capacity_retains_ready_rows_until_they_can_coalesce() {
        let (mut state, queued_rows) = fixture();
        state.policy.verification = VerificationMode::WarmSerial;
        let bloom = state.integration.generation.bloom;
        let expected = vec![state.requests[0].digest(), state.requests[1].digest()];
        let mut scheduler = MemberVerificationScheduler::default();
        scheduler.states.insert(bloom, state);
        let mut store = SqliteStore::open(":memory:").expect("store");
        store.enqueue_topic(Topic::MemberVerification, &queued_rows[0].payload, None).expect("enqueue oldest request");
        let capacity = CapacityPort(Cell::new(false));

        assert!(
            drain_member_verifications(&mut scheduler, &mut store, &capacity, 9_000)
                .expect("busy scheduler")
                .is_empty()
        );
        let oldest =
            store.queued_member_verification(&queued_rows[0].request).expect("oldest lookup").expect("oldest retained");
        assert_eq!(oldest.queued_unix_millis, 9_000);
        assert_eq!(oldest.deadline_unix_millis, 69_000);
        assert!(oldest.proposal.is_none());

        store.enqueue_topic(Topic::MemberVerification, &queued_rows[1].payload, None).expect("enqueue second request");
        capacity.0.set(true);
        let admits =
            drain_member_verifications(&mut scheduler, &mut store, &capacity, 10_000).expect("released scheduler");
        assert_eq!(admits.len(), 1);
        let event = from_bytes::<Event>(&admits[0].event).expect("proposal event");
        let Fact::ProposeSharedRun { plan, .. } = event.fact else {
            panic!("expected shared run proposal");
        };
        assert_eq!(plan.mode, SharedRunMode::WarmSerial);
        assert_eq!(plan.requests.iter().map(MemberVerifyRequest::digest).collect::<Vec<_>>(), expected);
        for (row, (queued_unix_millis, deadline_unix_millis)) in
            queued_rows[..2].iter().zip([(9_000, 69_000), (10_000, 70_000)])
        {
            let retained =
                store.queued_member_verification(&row.request).expect("request lookup").expect("request retained");
            assert_eq!(retained.queued_unix_millis, queued_unix_millis);
            assert_eq!(retained.deadline_unix_millis, deadline_unix_millis);
            assert!(retained.proposal.is_some());
        }
    }

    #[test]
    fn only_an_exact_retained_conflict_splits_the_next_contextual_batch() {
        let (mut state, _) = fixture();
        state.policy.max_run_members = 3;
        let head = state.integration.head.clone();
        let bloom_base = state.integration.generation.base;
        let mut checkpoints = Vec::new();
        for (index, request) in state.requests.iter_mut().enumerate() {
            request.input = CompositionInput {
                node: request.member.candidate.tree,
                candidate: request.member.candidate,
                members: vec![request.member.clone()],
            };
            request.context = Some(ConstructContext { bloom_base, starting_head: head.clone() });
            checkpoints.push(ConstructionCheckpoint {
                bloom: request.bloom,
                workpiece: request.member.workpiece.clone(),
                scope_revision: request.member.scope_revision,
                nonce: digest(100 + u8::try_from(index).unwrap_or_default()),
                observation: 1,
                starting_checkout: head.candidate.checkout,
                candidate: request.member.candidate,
            });
        }
        for checkpoint in &checkpoints {
            state.checkpoints.insert(checkpoint.workpiece.0.clone(), checkpoint.clone());
        }
        let queued = state
            .requests
            .iter()
            .enumerate()
            .map(|(index, request)| queued(request, u64::try_from(index).unwrap_or_default() + 1))
            .collect::<Vec<_>>();
        let preview = CompatibilityPreviewPlan {
            bloom: state.integration.generation.bloom,
            generation: state.integration.generation.digest(),
            base: state.integration.head.candidate,
            checkpoints,
        };
        state.preview_plans.push(preview.clone());
        state.previews.push(CompatibilityPreviewRecord {
            plan: preview.digest(),
            result: CompatibilityPreview::Conflict { evidence: digest(110) },
        });

        assert_eq!(SealedPolicySelection.select(&state, &queued), vec![0, 1]);

        state.checkpoints.get_mut(&state.requests[1].member.workpiece.0).expect("checkpoint").observation = 2;
        assert_eq!(SealedPolicySelection.select(&state, &queued), vec![0, 1, 2]);
    }

    #[test]
    fn retained_proposal_survives_later_arrival_and_head_change() {
        let (mut state, mut queued_rows) = fixture();
        let bloom = state.integration.generation.bloom;
        let selected = vec![state.requests[0].clone(), state.requests[2].clone()];
        let plan = build_plan(&state, selected);
        let event = Event {
            idempotency_key: IdempotencyKey("retained-proposal".to_owned()),
            fact: Fact::ProposeSharedRun { bloom, plan },
        };
        let proposal = to_vec(&event).expect("proposal bytes");
        let mut store = SqliteStore::open(":memory:").expect("store");
        for row in &mut queued_rows {
            row.sequence =
                store.enqueue_topic(Topic::MemberVerification, &row.payload, None).expect("enqueue retained request");
            assert_eq!(store.record_queued_member_verification(row).expect("queue request"), RecordOutcome::Recorded);
        }
        store
            .record_member_verification_proposal(
                &[queued_rows[0].request.clone(), queued_rows[2].request.clone()],
                &proposal,
            )
            .expect("retain complete selected mapping");

        state.integration.head.node = digest(99);
        let later_pin = pin("delta", 13);
        let later = request(
            bloom,
            &later_pin,
            CompositionInput {
                node: later_pin.candidate.tree,
                candidate: later_pin.candidate,
                members: vec![later_pin.clone()],
            },
            candidate(1),
        );
        let later_payload = to_vec(&MemberVerificationPayload { request: later.clone() }).expect("later payload");
        let later_sequence =
            store.enqueue_topic(Topic::MemberVerification, &later_payload, None).expect("enqueue later arrival");
        store.record_queued_member_verification(&queued(&later, later_sequence)).expect("later arrival");

        let mut scheduler = MemberVerificationScheduler::default();
        scheduler.states.insert(bloom, state);
        scheduler.set_proposals_held(true);
        let capacity = CapacityPort(Cell::new(false));
        let admits = drain_member_verifications(&mut scheduler, &mut store, &capacity, 20_000)
            .expect("retained proposal replay while gated and busy");
        assert_eq!(admits.len(), 1);
        assert_eq!(proposal_event(&admits[0].event).expect("replayed event"), event);

        let retained = store.queued_member_verifications().expect("retained queue");
        assert_eq!(retained[0].proposal.as_deref(), Some(proposal.as_slice()));
        assert_eq!(retained[2].proposal.as_deref(), Some(proposal.as_slice()));
        assert_eq!(proposal_event(retained[0].proposal.as_deref().expect("proposal")).expect("decode"), event);
        assert!(retained[1].proposal.is_none());
        assert!(retained[3].proposal.is_none());

        store
            .mark_queued_member_verifications_scheduled(&[
                queued_rows[0].request.clone(),
                queued_rows[2].request.clone(),
            ])
            .expect("journal selected rows");
        store.ack_topic(Topic::MemberVerification, queued_rows[2].sequence).expect("ack through selected atom");
        let interleaved = store.queued_member_verification(&queued_rows[1].request).expect("lookup").expect("row");
        assert!(!interleaved.scheduled, "interleaved request remains durably selectable after topic ack");
    }

    #[test]
    fn journaled_proposal_before_ack_does_not_reactivate_the_same_outbox_row() {
        let (_, queued) = fixture();
        let proposal = b"immutable proposal";
        let mut store = SqliteStore::open(":memory:").expect("store");
        store.record_queued_member_verification(&queued[0]).expect("queue request");
        store.record_member_verification_proposal(from_ref(&queued[0].request), proposal).expect("retain proposal");
        store.mark_queued_member_verifications_scheduled(from_ref(&queued[0].request)).expect("journal proposal");

        assert_eq!(
            store.record_queued_member_verification(&queued[0]).expect("outbox replay before ack"),
            RecordOutcome::Duplicate
        );
        let retained = store.queued_member_verification(&queued[0].request).expect("lookup").expect("row");
        assert!(retained.scheduled);
        assert_eq!(retained.proposal.as_deref(), Some(proposal.as_slice()));

        let mut retry = queued[0].clone();
        retry.sequence = 4;
        assert_eq!(
            store.record_queued_member_verification(&retry).expect("later logical retry"),
            RecordOutcome::Recorded
        );
        let retained = store.queued_member_verification(&queued[0].request).expect("lookup").expect("row");
        assert!(!retained.scheduled);
        assert!(retained.proposal.is_none());
    }
}
