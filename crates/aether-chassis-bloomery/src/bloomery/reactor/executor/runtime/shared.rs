//! Durable shared physical verification execution.

use std::{
    collections::{BTreeMap, BTreeSet},
    slice::from_ref,
};

use aether_bloomery::{
    Admit, BloomId, CompositionInput, ConfigScopes, ConstructionAdmission, ConstructionAdmissionPayload,
    ConstructionCheckpoint, ContextualDispatchPayload, CoordinationCancelPayload, Digest, DispatchPayload, Event,
    Evidence, EvidenceKind, ExecutionStatus, Fact, FailureScope, IdempotencyKey, MAX_VERIFIER_IDENTITIES, MemberPin,
    MemberVerifyLatency, MemberVerifyOutcome, ModelOverride, Nonce, PartialHeadRepairCompletion,
    PartialHeadRepairDispatch, PartialHeadRepairPayload, SharedRunCompletion, SharedRunDispatch,
    SharedRunDispatchPayload, SharedRunExecution, SharedRunMode, StageId, StageVerdict, StudyCall, StudyCost, Topic,
    VerificationObligation, VerifyFailure, VerifyFailureSet, VerifyProof, WorkHandle, WorkpieceId,
    construction_nonce_digest,
};
use aether_data::wire::{from_bytes, to_vec};

use crate::artifacts::{ArtifactsCapabilityState, GetResult, PutResult};
use crate::bloomery::executor::{ExecutorPort, Settled};
use crate::bloomery::intake::{
    DispatchError, DispatchRecord, EvidenceClaims, NameEvidenceClaims, UploadedEvidence, dispatch_and_record,
    dispatch_and_record_idle, dispatch_shared_and_record,
};
use crate::bloomery::outbox::{OutboxResultDelivery, TopicOutbox};
use crate::bloomery::reactor::shared_run::{
    PreparedSharedProbe, SharedProbePreparation, SharedProbePreparationRequest, SharedStepDescriptor, SharedStepReceipt,
};
use crate::bloomery::study::{
    StudyAdmitDecision, UploadedStudyRecord, admit_study, price_shared_run_step, record_shared_run_study,
    study_evidence_event,
};
use crate::bloomery::{
    BatchCheck, BatchFailure, BatchMember, BatchProbeReceipt, BatchProbeRequest, BatchProgress, BatchReport,
    ContextualProofReuse, HostClass, ProbeVerdict, ProofResult, contextual_bundle_reports, contextual_fact_key,
    dispatch_model, findings::verification_findings_key, next_batch_probe, observed_probe_verdict,
    record_contextual_facts, reuse_contextual_proof,
};
use crate::store::{
    CommissionBackend, ConstructionAdmissionRow, OrderLifecycle, PartialHeadRepairRow, SharedRunLifecycle,
    SharedRunMemberRow, SharedRunRow, SharedRunStepRow, StoreBackend, resolve_config,
};
use serde::de::DeserializeOwned;

use super::partial_repair::{derive_partial_repair_surface, materialize_partial_repair_task};
use super::scheduler::MemberVerificationScheduler;

const START_PREFIX: &str = "aether.bloomery.shared_run_started";
const COMPLETE_PREFIX: &str = "aether.bloomery.shared_run_completed";
const PARTIAL_REPAIR_PREFIX: &str = "aether.bloomery.partial_head_repair_completed";
const CONSTRUCTION_ADMISSION_PREFIX: &str = "aether.bloomery.request_construction_admission";

#[derive(serde::Serialize, serde::Deserialize)]
struct ReusedContextualProof {
    witness: ContextualProofReuse,
    evidence: Evidence,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct PartialRepairAccounting {
    subject: Digest,
    cost: StudyCost,
    calls: Option<Vec<StudyCall>>,
    session_reuse_arm: Option<String>,
    session_reuse_saved_micro_usd: Option<u64>,
    peak_resident_bytes: Option<u64>,
}

fn run_id(nonce: &Nonce) -> Digest {
    Digest::of_wire_bytes(format!("aether.bloomery.physical_run.v1:{}", nonce.0).as_bytes())
}

fn dispatch_bloom(dispatch: &SharedRunDispatch) -> Option<BloomId> {
    let bloom = dispatch.plan.requests.first()?.bloom;
    dispatch.plan.requests.iter().all(|request| request.bloom == bloom).then_some(bloom)
}

fn started_event(dispatch: &SharedRunDispatch, run: Digest) -> Option<Event> {
    Some(Event {
        idempotency_key: IdempotencyKey(format!("{START_PREFIX}:{}", run.to_hex())),
        fact: Fact::SharedRunStarted { bloom: dispatch_bloom(dispatch)?, plan: dispatch.plan.digest(), run },
    })
}

fn completion_event(bloom: BloomId, completion: SharedRunCompletion) -> Event {
    Event {
        idempotency_key: IdempotencyKey(format!("{COMPLETE_PREFIX}:{}", completion.run.to_hex())),
        fact: Fact::SharedRunCompleted { bloom, completion },
    }
}

fn partial_repair_event(dispatch: &PartialHeadRepairDispatch, completion: PartialHeadRepairCompletion) -> Event {
    Event {
        idempotency_key: IdempotencyKey(format!("{PARTIAL_REPAIR_PREFIX}:{}", dispatch.plan.digest().to_hex())),
        fact: Fact::PartialHeadRepairCompleted { bloom: dispatch.plan.bloom, plan: dispatch.plan.digest(), completion },
    }
}

fn construction_admission_event(admission: ConstructionAdmission) -> Event {
    Event {
        idempotency_key: IdempotencyKey(format!("{CONSTRUCTION_ADMISSION_PREFIX}:{}", admission.digest().to_hex())),
        fact: Fact::RequestConstructionAdmission { admission },
    }
}

fn admit(event: &Event) -> Option<Admit> {
    to_vec(event).ok().map(|event| Admit { event })
}

fn first_subject(transformation: &aether_bloomery::Transformation) -> Option<Digest> {
    transformation.inputs.first().copied()
}

fn logical_deadline(now_unix_millis: u64, request: &aether_bloomery::MemberVerifyRequest) -> u64 {
    now_unix_millis.saturating_add(request.transformation.limits.wall_clock_secs.saturating_mul(1_000))
}

fn contextual_record(dispatch: &aether_bloomery::ContextualAttemptDispatch, nonce: Nonce) -> DispatchRecord {
    let displayed = dispatch.candidate.unwrap_or(dispatch.scope_revision);
    DispatchRecord {
        nonce,
        bloom: dispatch.bloom,
        workpiece: dispatch.workpiece.clone(),
        scope_revision: dispatch.scope_revision,
        candidate: displayed,
        displayed_digest: displayed,
        stage: dispatch.stage,
        transformation: dispatch.transformation.clone(),
        configs: dispatch.configs.clone(),
        profile: dispatch.profile.clone(),
        instruction_bundle: None,
        prompt_manifest: None,
    }
}

fn submit_contextual_construct(
    store: &mut dyn StoreBackend,
    artifacts: Option<&mut ArtifactsCapabilityState>,
    executor: &dyn ExecutorPort,
    dispatch: &aether_bloomery::ContextualAttemptDispatch,
    admission: &ConstructionAdmissionRow,
    nonce: Nonce,
    sequence: u64,
) -> rusqlite::Result<super::DispatchSubmit> {
    let mut record = contextual_record(dispatch, nonce);
    if let Err(error) = super::overlay_member_advisory(store, &mut record, sequence) {
        tracing::warn!(%error, sequence, "contextual construction advisory could not resolve");
        return Ok(super::DispatchSubmit::Transient);
    }
    match dispatch_and_record_idle(executor, store, artifacts, &record, admission.deadline_unix_millis) {
        Ok(Settled::Answered(Some(handle))) => {
            store.mark_construction_admission_submitted(dispatch.digest().as_bytes())?;
            Ok(super::DispatchSubmit::Submitted(handle))
        }
        Ok(Settled::Answered(None) | Settled::InFlight) => Ok(super::DispatchSubmit::InFlight),
        Err(error) => {
            tracing::warn!(%error, sequence, "contextual idle submission will retry");
            Ok(super::DispatchSubmit::Transient)
        }
    }
}

fn scalar_contextual_dispatch(dispatch: aether_bloomery::ContextualAttemptDispatch) -> DispatchPayload {
    DispatchPayload {
        bloom: dispatch.bloom.0,
        workpiece: dispatch.workpiece,
        stage: dispatch.stage,
        transformation: dispatch.transformation,
        scope_revision: dispatch.scope_revision,
        candidate: dispatch.candidate,
        profile: dispatch.profile,
        configs: dispatch.configs,
    }
}

/// Persist new dispatches and replay their durable Started result until control
/// has journaled it. Only then acknowledge the outbox row and expose the run to
/// executor submission.
pub(super) fn drain_shared_dispatches(
    store: &mut dyn StoreBackend,
    now_unix_millis: u64,
) -> rusqlite::Result<Vec<Admit>> {
    let mut admits = Vec::new();
    let mut ack_through = None;
    for entry in store.drain_topic(Topic::SharedRun)? {
        let Ok(payload) = from_bytes::<SharedRunDispatchPayload>(&entry.payload) else {
            break;
        };
        let nonce = super::dispatch_nonce(entry.sequence);
        let run = run_id(&nonce);
        let cancelled = store.shared_run_cancelled(payload.dispatch.plan.digest().as_bytes())?;
        let Some(event) = started_event(&payload.dispatch, run) else {
            break;
        };
        let dispatch_bytes = to_vec(&payload.dispatch)
            .map_err(|error| rusqlite::Error::InvalidParameterName(format!("shared-run dispatch encode: {error}")))?;
        let members = payload
            .dispatch
            .plan
            .requests
            .iter()
            .enumerate()
            .map(|(ordinal, request)| {
                let queued = store.queued_member_verification(request.digest().as_bytes())?.map_or_else(
                    || (now_unix_millis, logical_deadline(now_unix_millis, request)),
                    |row| (row.queued_unix_millis, row.deadline_unix_millis),
                );
                Ok(SharedRunMemberRow {
                    run: run.as_bytes().to_vec(),
                    request: request.digest().as_bytes().to_vec(),
                    ordinal: u32::try_from(ordinal).unwrap_or(u32::MAX),
                    queued_unix_millis: queued.0,
                    deadline_unix_millis: queued.1,
                    cancelled: false,
                    outcome: None,
                    latency_millis: None,
                })
            })
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let deadline_unix_millis = if payload.dispatch.plan.mode == SharedRunMode::Contextual {
            members.iter().map(|member| member.deadline_unix_millis).min().unwrap_or(now_unix_millis)
        } else {
            members.iter().map(|member| member.deadline_unix_millis).max().unwrap_or(now_unix_millis)
        };
        store.record_shared_run(
            &SharedRunRow {
                run: run.as_bytes().to_vec(),
                nonce: nonce.0,
                dispatch: dispatch_bytes,
                lifecycle: SharedRunLifecycle::Preparing,
                next_ordinal: 0,
                deadline_unix_millis,
                charged: false,
                physical_cost: None,
            },
            &members,
        )?;
        if cancelled {
            for member in &members {
                store.cancel_shared_run_member(&member.request)?;
            }
        }

        match store.replay_topic_results(Topic::SharedRun, entry.sequence)? {
            OutboxResultDelivery::Unrecorded => {
                store.record_topic_results(Topic::SharedRun, entry.sequence, from_ref(&event))?;
                if let Some(admit) = admit(&event) {
                    admits.push(admit);
                }
                break;
            }
            OutboxResultDelivery::Pending(pending) => {
                admits.extend(pending);
                break;
            }
            OutboxResultDelivery::Journaled => {
                if store
                    .lookup_shared_run(run.as_bytes())?
                    .is_some_and(|row| row.lifecycle == SharedRunLifecycle::Preparing)
                {
                    store.update_shared_run(
                        run.as_bytes(),
                        if cancelled {
                            SharedRunLifecycle::Completing
                        } else {
                            SharedRunLifecycle::Ready
                        },
                        0,
                    )?;
                }
                ack_through = Some(entry.sequence);
            }
        }
    }
    if let Some(sequence) = ack_through {
        store.ack_topic(Topic::SharedRun, sequence)?;
    }
    Ok(admits)
}

/// Dispatch an opted-in member attempt through the legacy scalar executor
/// while retaining its exact starting head beside the nonce. The same payload
/// re-drives a submit-intent after restart; the context row is immutable.
pub(super) fn drain_contextual_dispatches(
    scheduler: &MemberVerificationScheduler,
    store: &mut dyn StoreBackend,
    artifacts: Option<&mut ArtifactsCapabilityState>,
    executor: &dyn ExecutorPort,
    now_unix_millis: u64,
) -> rusqlite::Result<Vec<WorkHandle>> {
    let mut handles = Vec::new();
    let mut ack_through = None;
    let mut artifacts = artifacts;
    for entry in store.drain_topic(Topic::ContextualDispatch)? {
        let Ok(payload) = from_bytes::<ContextualDispatchPayload>(&entry.payload) else {
            break;
        };
        let admitted_construct = payload.dispatch.stage == StageId::Construct;
        let admission = if admitted_construct {
            let Some(admission) = store.construction_admission(payload.dispatch.digest().as_bytes())? else {
                tracing::warn!(
                    sequence = entry.sequence,
                    "contextual construction dispatch has no journaled physical admission; leaving it pending"
                );
                break;
            };
            Some(admission)
        } else {
            None
        };
        let nonce = admission
            .as_ref()
            .map_or_else(|| super::dispatch_nonce(entry.sequence), |admission| Nonce(admission.nonce.clone()));
        let existing_order = store.lookup_order(&nonce.0)?;
        if admitted_construct
            && admission.as_ref().is_some_and(|admission| admission.retired)
            && existing_order.is_none()
        {
            ack_through = Some(entry.sequence);
            continue;
        }
        if admitted_construct
            && !scheduler.retains_construction_admission(&payload.dispatch, construction_nonce_digest(&nonce))
            && admission.as_ref().is_some_and(|admission| !admission.submitted)
            && existing_order.is_none()
        {
            store.retire_construction_admission(payload.dispatch.digest().as_bytes())?;
            ack_through = Some(entry.sequence);
            continue;
        }
        store.record_contextual_dispatch(
            &nonce.0,
            &to_vec(&payload.dispatch).map_err(|error| {
                rusqlite::Error::InvalidParameterName(format!("contextual dispatch encode: {error}"))
            })?,
        )?;
        if existing_order.is_some_and(|order| order.lifecycle == OrderLifecycle::Submitted) {
            if admitted_construct {
                store.mark_construction_admission_submitted(payload.dispatch.digest().as_bytes())?;
            }
            handles.push(WorkHandle::new(nonce));
            ack_through = Some(entry.sequence);
            continue;
        }
        let dispatch = payload.dispatch;
        let submission = if admitted_construct {
            submit_contextual_construct(
                store,
                artifacts.as_deref_mut(),
                executor,
                &dispatch,
                admission.as_ref().expect("construction dispatch has retained admission"),
                nonce,
                entry.sequence,
            )?
        } else {
            super::submit_dispatch_entry(
                store,
                artifacts.as_deref_mut(),
                executor,
                scalar_contextual_dispatch(dispatch),
                entry.sequence,
                now_unix_millis,
            )?
        };
        match submission {
            super::DispatchSubmit::Submitted(handle) => {
                handles.push(handle);
                ack_through = Some(entry.sequence);
            }
            super::DispatchSubmit::Parked | super::DispatchSubmit::Refused => {
                ack_through = Some(entry.sequence);
                break;
            }
            super::DispatchSubmit::InFlight | super::DispatchSubmit::Transient => break,
        }
    }
    if let Some(sequence) = ack_through {
        store.ack_topic(Topic::ContextualDispatch, sequence)?;
    }
    Ok(handles)
}

pub(super) fn drain_construction_admissions(
    scheduler: &MemberVerificationScheduler,
    store: &mut dyn StoreBackend,
    executor: &dyn ExecutorPort,
    now_unix_millis: u64,
) -> rusqlite::Result<Vec<Admit>> {
    let mut admits = Vec::new();
    let Some(entry) = store.drain_topic(Topic::ConstructionAdmission)?.into_iter().next() else {
        return Ok(admits);
    };
    let Ok(payload) = from_bytes::<ConstructionAdmissionPayload>(&entry.payload) else {
        return Ok(admits);
    };
    let nonce = super::dispatch_nonce(entry.sequence);
    let dispatch_digest = payload.dispatch.digest();
    if let Some(retained) = store.construction_admission(dispatch_digest.as_bytes())? {
        if retained.retired || retained.nonce != nonce.0 {
            // The admitted nonce is the one the journal already carries, so a
            // topic row naming a different one is superseded, not authoritative.
            // Acknowledging it retires the row; refusing the pass would leave
            // the entry un-acked and wedge the whole topic behind it (#5903).
            if retained.nonce != nonce.0 {
                tracing::warn!(
                    retained = %retained.nonce,
                    replayed = %nonce.0,
                    "construction admission replay names another physical nonce; the retained admission stands"
                );
            }
            store.ack_topic(Topic::ConstructionAdmission, entry.sequence)?;
            return Ok(admits);
        }
    } else {
        if store.has_pending_construction_admission()? {
            return Ok(admits);
        }
        let record = contextual_record(&payload.dispatch, nonce.clone());
        if !executor.has_idle_capacity(&record.to_order()) {
            return Ok(admits);
        }
        let wall_clock_millis = payload.dispatch.transformation.limits.wall_clock_secs.saturating_mul(1_000);
        let deadline_unix_millis = now_unix_millis.saturating_add(wall_clock_millis);
        store.record_construction_admission(
            dispatch_digest.as_bytes(),
            &nonce.0,
            now_unix_millis,
            deadline_unix_millis,
        )?;
    }
    let event = construction_admission_event(ConstructionAdmission {
        nonce: construction_nonce_digest(&nonce),
        dispatch: payload.dispatch.clone(),
    });
    match store.replay_topic_results(Topic::ConstructionAdmission, entry.sequence)? {
        OutboxResultDelivery::Unrecorded => {
            store.record_topic_results(Topic::ConstructionAdmission, entry.sequence, from_ref(&event))?;
            if let Some(admit) = admit(&event) {
                admits.push(admit);
            }
        }
        OutboxResultDelivery::Pending(pending) => {
            admits.extend(pending);
        }
        OutboxResultDelivery::Journaled => {
            if scheduler.retains_construction_admission(&payload.dispatch, construction_nonce_digest(&nonce)) {
                store.mark_construction_admission_journaled(dispatch_digest.as_bytes())?;
            } else {
                store.retire_construction_admission(dispatch_digest.as_bytes())?;
            }
            store.ack_topic(Topic::ConstructionAdmission, entry.sequence)?;
        }
    }
    Ok(admits)
}

fn partial_repair_record(dispatch: &PartialHeadRepairDispatch, nonce: Nonce) -> DispatchRecord {
    DispatchRecord {
        nonce,
        bloom: dispatch.plan.bloom,
        workpiece: WorkpieceId::composition(),
        scope_revision: dispatch.scope_revision,
        candidate: dispatch.plan.head.candidate.tree,
        displayed_digest: dispatch.plan.head.candidate.tree,
        stage: StageId::Refine,
        transformation: dispatch.transformation.clone(),
        configs: dispatch.configs.clone(),
        profile: dispatch.profile.clone(),
        instruction_bundle: None,
        prompt_manifest: None,
    }
}

fn refused_partial_repair(dispatch: &PartialHeadRepairDispatch, detail: Digest) -> PartialHeadRepairCompletion {
    PartialHeadRepairCompletion::HostFault {
        evidence: Evidence { subject: dispatch.plan.head.candidate.tree, kind: EvidenceKind::ExecutorFault, detail },
    }
}

fn observed_partial_repair(
    dispatch: &PartialHeadRepairDispatch,
    upload: &UploadedEvidence,
) -> PartialHeadRepairCompletion {
    let subject = dispatch.plan.head.candidate.tree;
    if upload.subject != subject || upload.verdict == StageVerdict::ExecutorFault {
        return refused_partial_repair(dispatch, upload.detail);
    }
    if upload.verdict == StageVerdict::VerificationPassed
        && let Some(candidate) = upload.observation.candidate
    {
        return PartialHeadRepairCompletion::Repaired {
            candidate,
            evidence: Evidence { subject, kind: EvidenceKind::VerificationResult, detail: upload.detail },
        };
    }
    PartialHeadRepairCompletion::Refused {
        evidence: Evidence {
            subject,
            kind: if upload.verdict == StageVerdict::ReviewFinding {
                EvidenceKind::ReviewFinding
            } else {
                EvidenceKind::RepairTriage
            },
            detail: upload.detail,
        },
    }
}

fn partial_repair_accounting(upload: &UploadedEvidence) -> Option<PartialRepairAccounting> {
    Some(PartialRepairAccounting {
        subject: upload.subject,
        cost: upload.observation.cost?,
        calls: upload.observation.calls.clone(),
        session_reuse_arm: upload.observation.session_reuse_arm.clone(),
        session_reuse_saved_micro_usd: upload.observation.session_reuse_saved_micro_usd,
        peak_resident_bytes: upload.observation.peak_resident_bytes,
    })
}

fn partial_repair_findings(
    store: &mut dyn StoreBackend,
    artifacts: Option<&mut ArtifactsCapabilityState>,
    bloom: BloomId,
    evidence: Digest,
) -> rusqlite::Result<Option<String>> {
    let key = verification_findings_key(evidence);
    if let Some(findings) = store.lookup_review_findings(bloom.0.as_bytes(), &key)? {
        return Ok(Some(findings));
    }
    let Some(artifacts) = artifacts else {
        return Ok(None);
    };
    Ok(match artifacts.get(evidence.to_hex()) {
        GetResult::Ok { bytes, .. } => String::from_utf8(bytes).ok().filter(|value| !value.trim().is_empty()),
        GetResult::Err { error, .. } => {
            tracing::warn!(?error, evidence = %evidence.to_hex(), "partial-head repair evidence is unavailable; retrying");
            None
        }
    })
}

fn resolve_partial_repair_model(store: &mut dyn StoreBackend, record: &mut DispatchRecord) -> rusqlite::Result<()> {
    let model_override = resolve_config::<ModelOverride>(store, ConfigScopes::bloom_wide(&record.configs))
        .map_err(|error| rusqlite::Error::InvalidParameterName(format!("partial-head repair model: {error}")))?
        .unwrap_or_default();
    record.transformation.model = Some(dispatch_model(record.stage, &record.profile, &model_override));
    Ok(())
}

fn materialize_partial_repair_result(
    store: &mut dyn StoreBackend,
    artifacts: Option<&mut ArtifactsCapabilityState>,
    dispatch: &PartialHeadRepairDispatch,
    nonce: &Nonce,
    row: &PartialHeadRepairRow,
) -> rusqlite::Result<bool> {
    if row.result.is_some() {
        return Ok(true);
    }
    let Some(completion) =
        row.completion.as_deref().map(from_bytes::<PartialHeadRepairCompletion>).transpose().map_err(|error| {
            rusqlite::Error::InvalidParameterName(format!("partial-head completion decode: {error}"))
        })?
    else {
        return Ok(false);
    };
    let mut events = Vec::new();
    if let Some(bytes) = row.accounting.as_deref() {
        let Some(artifacts) = artifacts else {
            return Ok(false);
        };
        let accounting = decode_host::<PartialRepairAccounting>(bytes)?;
        let study = UploadedStudyRecord {
            nonce: nonce.clone(),
            subject: accounting.subject,
            cost: accounting.cost,
            calls: accounting.calls,
            session_reuse_arm: accounting.session_reuse_arm,
            session_reuse_saved_micro_usd: accounting.session_reuse_saved_micro_usd,
            peak_resident_bytes: accounting.peak_resident_bytes,
        };
        match admit_study(store, artifacts, &study) {
            Ok(StudyAdmitDecision::Admitted(admission)) => {
                if let Some(event) = study_evidence_event(&admission, nonce) {
                    events.push(event);
                }
            }
            Ok(StudyAdmitDecision::Refused(refusal)) => {
                tracing::warn!(?refusal, nonce = %nonce.0, "partial-head repair study was refused; result remains retained");
                return Ok(false);
            }
            Err(error) => {
                tracing::warn!(%error, nonce = %nonce.0, "partial-head repair study could not be stored; retrying");
                return Ok(false);
            }
        }
    }
    events.push(partial_repair_event(dispatch, completion));
    store.record_partial_head_repair_result(
        &row.nonce,
        &to_vec(&events)
            .map_err(|error| rusqlite::Error::InvalidParameterName(format!("partial-head result encode: {error}")))?,
    )?;
    Ok(true)
}

fn replay_partial_repair_result(
    store: &mut dyn StoreBackend,
    row: &PartialHeadRepairRow,
) -> rusqlite::Result<Vec<Admit>> {
    let Some(bytes) = row.result.as_deref() else {
        return Ok(Vec::new());
    };
    let events = from_bytes::<Vec<Event>>(bytes)
        .map_err(|error| rusqlite::Error::InvalidParameterName(format!("partial-head result decode: {error}")))?;
    match store.replay_topic_results(Topic::PartialHeadRepair, row.sequence)? {
        OutboxResultDelivery::Unrecorded => {
            store.record_topic_results(Topic::PartialHeadRepair, row.sequence, &events)?;
            Ok(events.iter().filter_map(admit).collect())
        }
        OutboxResultDelivery::Pending(pending) => Ok(pending),
        OutboxResultDelivery::Journaled => {
            store.ack_topic(Topic::PartialHeadRepair, row.sequence)?;
            store.settle_partial_head_repair(&row.nonce)?;
            Ok(Vec::new())
        }
    }
}

fn resume_partial_repair_result(
    store: &mut dyn StoreBackend,
    artifacts: Option<&mut ArtifactsCapabilityState>,
    row: &PartialHeadRepairRow,
    dispatch: &PartialHeadRepairDispatch,
    nonce: &Nonce,
) -> rusqlite::Result<Option<Vec<Admit>>> {
    if row.result.is_some() {
        store.consume_order(&row.nonce)?;
        return replay_partial_repair_result(store, row).map(Some);
    }
    if row.completion.is_none() {
        return Ok(None);
    }
    if !materialize_partial_repair_result(store, artifacts, dispatch, nonce, row)? {
        return Ok(Some(Vec::new()));
    }
    store.consume_order(&row.nonce)?;
    let updated = store
        .list_partial_head_repairs()?
        .into_iter()
        .find(|candidate| candidate.nonce == row.nonce)
        .ok_or_else(|| rusqlite::Error::InvalidParameterName("partial-head repair disappeared".to_owned()))?;
    replay_partial_repair_result(store, &updated).map(Some)
}

fn retain_partial_repair_completion(
    store: &mut dyn StoreBackend,
    nonce: &str,
    completion: &PartialHeadRepairCompletion,
    accounting: Option<&[u8]>,
) -> rusqlite::Result<()> {
    let completion = to_vec(completion)
        .map_err(|error| rusqlite::Error::InvalidParameterName(format!("partial-head completion encode: {error}")))?;
    store.record_partial_head_repair_observation(nonce, &completion, accounting)?;
    Ok(())
}

fn submit_partial_repair<S>(
    store: &mut S,
    artifacts: Option<&mut ArtifactsCapabilityState>,
    executor: &dyn ExecutorPort,
    row: &PartialHeadRepairRow,
    dispatch: &PartialHeadRepairDispatch,
    nonce: &Nonce,
    now_unix_millis: u64,
) -> rusqlite::Result<bool>
where
    S: StoreBackend + CommissionBackend,
{
    let mut artifacts = artifacts;
    if let Some(order) = store.lookup_order(&row.nonce)? {
        return Ok(order.lifecycle == OrderLifecycle::Submitted);
    }
    let mut record = partial_repair_record(dispatch, nonce.clone());
    if let Err(error) = resolve_partial_repair_model(store, &mut record) {
        tracing::warn!(%error, nonce = %row.nonce, "partial-head repair model could not resolve");
        return Ok(false);
    }
    let Some(findings) =
        partial_repair_findings(store, artifacts.as_deref_mut(), dispatch.plan.bloom, dispatch.plan.evidence)?
    else {
        return Ok(false);
    };
    let evidence_task = format!("## Findings\n\n{findings}");
    let task = match materialize_partial_repair_task(store, dispatch, &evidence_task) {
        Ok(task) => task,
        Err(error) => {
            let completion = refused_partial_repair(dispatch, Digest::of_wire_bytes(error.to_string().as_bytes()));
            retain_partial_repair_completion(store, &row.nonce, &completion, None)?;
            return Ok(false);
        }
    };
    record.transformation.description = Some(task.description);
    match dispatch_and_record(executor, store, artifacts, &record, now_unix_millis) {
        Ok(_) => Ok(false),
        Err(DispatchError::Provenance(refusal)) => {
            let completion = refused_partial_repair(dispatch, Digest::of_wire_bytes(refusal.to_string().as_bytes()));
            retain_partial_repair_completion(store, &row.nonce, &completion, None)?;
            Ok(false)
        }
        Err(error) => {
            tracing::warn!(%error, nonce = %row.nonce, "partial-head repair submit will retry");
            Ok(false)
        }
    }
}

fn expire_partial_repair(
    store: &mut dyn StoreBackend,
    executor: &dyn ExecutorPort,
    row: &PartialHeadRepairRow,
    dispatch: &PartialHeadRepairDispatch,
    nonce: &Nonce,
    now_unix_millis: u64,
) -> rusqlite::Result<bool> {
    if store.lookup_order(&row.nonce)?.is_none_or(|order| order.deadline_unix_millis > now_unix_millis) {
        return Ok(false);
    }
    match executor.cancel(&WorkHandle::new(nonce.clone())) {
        Settled::InFlight => Ok(true),
        Settled::Answered(Err(error)) => {
            tracing::warn!(%error, nonce = %row.nonce, "expired partial-head repair cancellation will retry");
            Ok(true)
        }
        Settled::Answered(Ok(())) => {
            let detail =
                Digest::of_wire_bytes(format!("aether.bloomery.partial_head_repair.expired:{}", row.nonce).as_bytes());
            retain_partial_repair_completion(store, &row.nonce, &refused_partial_repair(dispatch, detail), None)?;
            Ok(true)
        }
    }
}

fn observe_partial_repair<S>(
    store: &mut S,
    artifacts: Option<&mut ArtifactsCapabilityState>,
    executor: &dyn ExecutorPort,
    claims: NameEvidenceClaims,
    row: &PartialHeadRepairRow,
    dispatch: &PartialHeadRepairDispatch,
    nonce: &Nonce,
) -> rusqlite::Result<Vec<Admit>>
where
    S: StoreBackend + CommissionBackend,
{
    let Settled::Answered(observed) = executor.observe(&WorkHandle::new(nonce.clone())) else {
        return Ok(Vec::new());
    };
    let observed = match observed {
        Ok(observed) => observed,
        Err(error) => {
            tracing::warn!(%error, nonce = %row.nonce, "partial-head repair observation will retry");
            return Ok(Vec::new());
        }
    };
    if !matches!(observed.status, ExecutionStatus::Completed { .. }) {
        return Ok(Vec::new());
    }
    let Some(references) = observed.evidence.and_then(Result::ok) else {
        return Ok(Vec::new());
    };
    let Some(upload) = references.iter().find_map(|reference| claims.claim_for(reference)) else {
        return Ok(Vec::new());
    };
    let mut completion = observed_partial_repair(dispatch, &upload);
    let repaired_candidate = match &completion {
        PartialHeadRepairCompletion::Repaired { candidate, .. } => Some(*candidate),
        PartialHeadRepairCompletion::Refused { .. } | PartialHeadRepairCompletion::HostFault { .. } => None,
    };
    if let Some(candidate) = repaired_candidate {
        let surface = match derive_partial_repair_surface(store, dispatch) {
            Ok(surface) => surface,
            Err(error) => {
                completion = refused_partial_repair(dispatch, Digest::of_wire_bytes(error.to_string().as_bytes()));
                Vec::new()
            }
        };
        if !surface.is_empty() {
            match executor.retain_partial_head_repair(&dispatch.plan.digest(), &candidate, &surface) {
                Settled::InFlight => return Ok(Vec::new()),
                Settled::Answered(Err(error)) => {
                    completion = refused_partial_repair(dispatch, Digest::of_wire_bytes(error.to_string().as_bytes()));
                }
                Settled::Answered(Ok(())) => {}
            }
        }
    }
    let accounting = partial_repair_accounting(&upload).map(|value| encode_host(&value)).transpose()?;
    retain_partial_repair_completion(store, &row.nonce, &completion, accounting.as_deref())?;
    let updated = store
        .list_partial_head_repairs()?
        .into_iter()
        .find(|candidate| candidate.nonce == row.nonce)
        .ok_or_else(|| rusqlite::Error::InvalidParameterName("partial-head repair disappeared".to_owned()))?;
    if !materialize_partial_repair_result(store, artifacts, dispatch, nonce, &updated)? {
        return Ok(Vec::new());
    }
    store.consume_order(&row.nonce)?;
    let updated = store
        .list_partial_head_repairs()?
        .into_iter()
        .find(|candidate| candidate.nonce == row.nonce)
        .ok_or_else(|| rusqlite::Error::InvalidParameterName("partial-head repair disappeared".to_owned()))?;
    replay_partial_repair_result(store, &updated)
}

pub(super) fn drive_partial_head_repairs<S>(
    store: &mut S,
    artifacts: Option<&mut ArtifactsCapabilityState>,
    executor: &dyn ExecutorPort,
    claims: NameEvidenceClaims,
    now_unix_millis: u64,
) -> rusqlite::Result<Vec<Admit>>
where
    S: StoreBackend + CommissionBackend,
{
    let mut artifacts = artifacts;
    for entry in store.drain_topic(Topic::PartialHeadRepair)? {
        let Ok(payload) = from_bytes::<PartialHeadRepairPayload>(&entry.payload) else {
            break;
        };
        let nonce = super::dispatch_nonce(entry.sequence);
        store.record_partial_head_repair(&PartialHeadRepairRow {
            sequence: entry.sequence,
            nonce: nonce.0,
            dispatch: to_vec(&payload.dispatch)
                .map_err(|error| rusqlite::Error::InvalidParameterName(format!("partial-head dispatch: {error}")))?,
            completion: None,
            accounting: None,
            result: None,
        })?;
    }

    let mut admits = Vec::new();
    for row in store.list_partial_head_repairs()? {
        let dispatch = from_bytes::<PartialHeadRepairDispatch>(&row.dispatch)
            .map_err(|error| rusqlite::Error::InvalidParameterName(format!("partial-head dispatch decode: {error}")))?;
        let nonce = Nonce(row.nonce.clone());
        if let Some(replayed) = resume_partial_repair_result(store, artifacts.as_deref_mut(), &row, &dispatch, &nonce)?
        {
            admits.extend(replayed);
            continue;
        }
        if !submit_partial_repair(store, artifacts.as_deref_mut(), executor, &row, &dispatch, &nonce, now_unix_millis)?
        {
            continue;
        }
        if expire_partial_repair(store, executor, &row, &dispatch, &nonce, now_unix_millis)? {
            continue;
        }
        admits.extend(observe_partial_repair(
            store,
            artifacts.as_deref_mut(),
            executor,
            claims,
            &row,
            &dispatch,
            &nonce,
        )?);
    }
    Ok(admits)
}

fn prepared_member(request: &aether_bloomery::MemberVerifyRequest) -> PreparedSharedProbe {
    PreparedSharedProbe {
        request: request.digest(),
        candidate: request.member.candidate,
        transformation: request.transformation.clone(),
        profile: request.profile.clone(),
        configs: request.configs.clone(),
    }
}

fn prepared_contextual(dispatch: &SharedRunDispatch) -> Option<PreparedSharedProbe> {
    let SharedRunExecution::Contextual { node, transformation, profile, configs } = &dispatch.execution else {
        return None;
    };
    Some(PreparedSharedProbe {
        request: dispatch.plan.digest(),
        candidate: node.candidate,
        transformation: (**transformation).clone(),
        profile: profile.clone(),
        configs: configs.clone(),
    })
}

fn encode_host<T: serde::Serialize>(value: &T) -> rusqlite::Result<Vec<u8>> {
    serde_json::to_vec(value)
        .map_err(|error| rusqlite::Error::InvalidParameterName(format!("shared-run host record encode: {error}")))
}

fn decode_host<T: DeserializeOwned>(bytes: &[u8]) -> rusqlite::Result<T> {
    serde_json::from_slice(bytes)
        .map_err(|error| rusqlite::Error::InvalidParameterName(format!("shared-run host record decode: {error}")))
}

fn next_initial_step(
    store: &mut dyn StoreBackend,
    row: &SharedRunRow,
    dispatch: &SharedRunDispatch,
    now_unix_millis: u64,
) -> rusqlite::Result<Option<SharedRunStepRow>> {
    if matches!(dispatch.execution, SharedRunExecution::Contextual { .. })
        && !store.shared_run_steps(&row.run)?.is_empty()
    {
        return Ok(None);
    }
    let (request, descriptor, prepared) = match &dispatch.execution {
        SharedRunExecution::Serial => {
            let members = store.shared_run_members(&row.run)?;
            let Some((request, _)) = dispatch.plan.requests.iter().zip(members).find(|(_, member)| {
                !member.cancelled && member.outcome.is_none() && member.deadline_unix_millis > now_unix_millis
            }) else {
                return Ok(None);
            };
            (
                Some(request.digest()),
                SharedStepDescriptor::Member { request: request.digest() },
                prepared_member(request),
            )
        }
        SharedRunExecution::Contextual { node, .. } => (
            None,
            SharedStepDescriptor::ContextualFull { node: node.digest() },
            prepared_contextual(dispatch).expect("matched contextual execution"),
        ),
    };
    let ordinal = row.next_ordinal;
    Ok(Some(SharedRunStepRow {
        run: row.run.clone(),
        ordinal,
        nonce: format!("{}-step-{ordinal}", row.nonce),
        request: request.map(|request| request.as_bytes().to_vec()),
        descriptor: encode_host(&descriptor)?,
        prepared: Some(encode_host(&SharedProbePreparation::Prepared(Box::new(prepared)))?),
        receipt: None,
        duration_millis: None,
        release_physical_run: false,
    }))
}

fn submit_step(
    store: &mut dyn StoreBackend,
    artifacts: Option<&mut ArtifactsCapabilityState>,
    executor: &dyn ExecutorPort,
    row: &SharedRunRow,
    dispatch: &SharedRunDispatch,
    step: &SharedRunStepRow,
    now_unix_millis: u64,
) -> rusqlite::Result<()> {
    if step.receipt.is_some() || step.prepared.is_none() {
        return Ok(());
    }
    let run = Digest::from_slice(&row.run)
        .ok_or_else(|| rusqlite::Error::InvalidParameterName("shared run identity is not a digest".to_owned()))?;
    let prepared = match decode_host::<SharedProbePreparation>(step.prepared.as_deref().unwrap_or_default())? {
        SharedProbePreparation::Prepared(prepared) => prepared,
        SharedProbePreparation::Refused { .. } => return Ok(()),
    };
    let request = dispatch.plan.requests.iter().find(|request| request.digest() == prepared.request);
    let (workpiece, scope_revision, stage) = request.map_or_else(
        || (WorkpieceId::composition(), dispatch.plan.digest(), StageId::AggregateVerify),
        |request| (request.member.workpiece.clone(), request.member.scope_revision, StageId::Verify),
    );
    let subject = first_subject(&prepared.transformation).unwrap_or(prepared.candidate.tree);
    let record = DispatchRecord {
        nonce: Nonce(step.nonce.clone()),
        bloom: dispatch.plan.requests.first().map_or_else(|| BloomId(Digest::default()), |request| request.bloom),
        workpiece,
        scope_revision,
        candidate: subject,
        displayed_digest: subject,
        stage,
        transformation: prepared.transformation,
        configs: prepared.configs,
        profile: prepared.profile,
        instruction_bundle: None,
        prompt_manifest: None,
    };
    let deadline = request
        .and_then(|request| {
            store
                .shared_run_members(&row.run)
                .ok()?
                .into_iter()
                .find(|member| member.request == request.digest().as_bytes())
                .map(|member| member.deadline_unix_millis)
        })
        .unwrap_or(row.deadline_unix_millis);
    if deadline <= now_unix_millis {
        store.consume_order(&step.nonce)?;
        let detail = Digest::of_wire_bytes(format!("expired:{}:{deadline}", step.nonce).as_bytes());
        let receipt = SharedStepReceipt {
            invocation: detail,
            evidence: Evidence { subject, kind: EvidenceKind::ExecutorFault, detail },
            verdict: StageVerdict::ExecutorFault,
            failed_verifiers: VerifyFailureSet::default(),
            failed_verifier_names: Vec::new(),
            findings: None,
            cost: None,
            calls: None,
            contextual_observations: None,
            probe_verdict: Some(ProbeVerdict::Infrastructure),
        };
        store.complete_shared_run_step(&step.nonce, &encode_host(&receipt)?, 0)?;
        return Ok(());
    }
    if store.lookup_order(&step.nonce)?.is_some_and(|order| order.lifecycle == OrderLifecycle::Submitted) {
        return Ok(());
    }
    match dispatch_shared_and_record(executor, store, artifacts, &record, run, false, deadline) {
        Ok(Settled::Answered(_)) => {
            store.update_shared_run(&row.run, SharedRunLifecycle::Running, step.ordinal)?;
        }
        Ok(Settled::InFlight) => {}
        Err(error) => {
            tracing::warn!(%error, nonce = %step.nonce, "shared-run step submit will retry");
        }
    }
    Ok(())
}

fn evidence_for(verdict: StageVerdict, subject: Digest, detail: Digest) -> Evidence {
    Evidence {
        subject,
        kind: if verdict == StageVerdict::ExecutorFault {
            EvidenceKind::ExecutorFault
        } else {
            EvidenceKind::VerificationResult
        },
        detail,
    }
}

fn project_shared_findings(
    store: &mut dyn StoreBackend,
    dispatch: &SharedRunDispatch,
    receipt: &SharedStepReceipt,
) -> rusqlite::Result<()> {
    let Some(bloom) = dispatch_bloom(dispatch) else {
        return Ok(());
    };
    project_shared_findings_for_bloom(store, bloom, receipt)
}

fn project_shared_findings_for_bloom(
    store: &mut dyn StoreBackend,
    bloom: BloomId,
    receipt: &SharedStepReceipt,
) -> rusqlite::Result<()> {
    let Some(findings) = receipt.findings.as_deref().filter(|findings| !findings.trim().is_empty()) else {
        return Ok(());
    };
    if receipt.verdict == StageVerdict::VerificationFailed {
        store.record_review_findings(
            bloom.0.as_bytes(),
            &verification_findings_key(receipt.evidence.detail),
            findings,
        )?;
    }
    Ok(())
}

const SHARED_MEMBER_FINDINGS_PREFIX: &str = "shared-member-findings";
const SHARED_MEMBER_FINDINGS_GUARD_PREFIX: &str = "shared-member-findings-guard";

pub(super) fn shared_member_findings_key(member: &MemberPin) -> String {
    format!(
        "{SHARED_MEMBER_FINDINGS_PREFIX}:{}:{}:{}:{}",
        member.workpiece.0,
        member.scope_revision.to_hex(),
        member.candidate.tree.to_hex(),
        member.candidate.checkout.to_hex(),
    )
}

pub(super) fn shared_member_findings_guard_key(workpiece: &WorkpieceId) -> String {
    format!("{SHARED_MEMBER_FINDINGS_GUARD_PREFIX}:{}", workpiece.0)
}

pub(super) fn guarded_shared_member_findings_key(guard: &str) -> Option<&str> {
    guard.split_once('\n').map(|(_, key)| key).filter(|key| !key.is_empty())
}

fn guarded_shared_member_findings_sequence(guard: &str) -> Option<u64> {
    guard.split_once('\n')?.0.parse().ok()
}

fn shared_run_dispatch_sequence(row: &SharedRunRow) -> Option<u64> {
    row.nonce.strip_prefix("dispatch-")?.parse().ok()
}

fn attributed_member_evidence(outcome: &MemberVerifyOutcome, request: Digest, member: &MemberPin) -> Option<Digest> {
    let MemberVerifyOutcome::Failed {
        request: outcome_request,
        scope: FailureScope::Attributed { members, evidence },
        evidence: receipt,
        ..
    } = outcome
    else {
        return None;
    };
    (*outcome_request == request
        && receipt.kind == EvidenceKind::VerificationResult
        && receipt.detail == *evidence
        && members.as_slice() == from_ref(member))
    .then_some(*evidence)
}

fn record_exact_member_findings(
    store: &mut dyn StoreBackend,
    bloom: BloomId,
    sequence: u64,
    member: &MemberPin,
    evidence: Digest,
) -> rusqlite::Result<()> {
    let Some(findings) = store.lookup_review_findings(bloom.0.as_bytes(), &verification_findings_key(evidence))? else {
        return Ok(());
    };
    if findings.trim().is_empty() {
        return Ok(());
    }

    let guard_key = shared_member_findings_guard_key(&member.workpiece);
    if store
        .lookup_review_findings(bloom.0.as_bytes(), &guard_key)?
        .and_then(|guard| guarded_shared_member_findings_sequence(&guard))
        .is_some_and(|retained| retained > sequence)
    {
        return Ok(());
    }

    let exact_key = shared_member_findings_key(member);
    store.record_review_findings(bloom.0.as_bytes(), &exact_key, &findings)?;
    store.record_review_findings(bloom.0.as_bytes(), &guard_key, &format!("{sequence}\n{exact_key}"))?;
    store.record_review_findings(bloom.0.as_bytes(), &member.workpiece.0, &findings)?;
    Ok(())
}

fn project_attributed_member_findings(
    store: &mut dyn StoreBackend,
    row: &SharedRunRow,
    dispatch: &SharedRunDispatch,
    outcome: &MemberVerifyOutcome,
) -> rusqlite::Result<()> {
    let Some(request) = dispatch.plan.requests.iter().find(|request| request.digest() == outcome.request()) else {
        return Ok(());
    };
    let Some(evidence) = attributed_member_evidence(outcome, request.digest(), &request.member) else {
        return Ok(());
    };
    let Some(bloom) = dispatch_bloom(dispatch) else {
        return Ok(());
    };
    let Some(sequence) = shared_run_dispatch_sequence(row) else {
        return Ok(());
    };
    record_exact_member_findings(store, bloom, sequence, &request.member, evidence)
}

fn complete_observed_step(
    store: &mut dyn StoreBackend,
    executor: &dyn ExecutorPort,
    claims: NameEvidenceClaims,
    host_class: &HostClass,
    dispatch: &SharedRunDispatch,
    step: &SharedRunStepRow,
) -> rusqlite::Result<bool> {
    let Some(order) = store.lookup_order(&step.nonce)? else {
        return Ok(false);
    };
    if order.lifecycle != OrderLifecycle::Submitted {
        return Ok(false);
    }
    let handle = WorkHandle::new(Nonce(step.nonce.clone()));
    let Settled::Answered(observed) = executor.observe(&handle) else {
        return Ok(false);
    };
    let observed = match observed {
        Ok(observed) => observed,
        Err(error) => {
            tracing::warn!(%error, nonce = %step.nonce, "shared-run observation faulted");
            return Ok(false);
        }
    };
    if !matches!(observed.status, ExecutionStatus::Completed { .. }) {
        return Ok(false);
    }
    let references = match observed.evidence {
        Some(Ok(references)) => references,
        Some(Err(error)) => {
            tracing::warn!(%error, nonce = %step.nonce, "shared-run evidence stream faulted");
            return Ok(false);
        }
        None => return Ok(false),
    };
    let Some(upload) = references.iter().find_map(|reference| claims.claim_for(reference)) else {
        return Ok(false);
    };
    let displayed = Digest::from_slice(&order.displayed_digest);
    if displayed != Some(upload.subject) {
        tracing::warn!(nonce = %step.nonce, "shared-run evidence named a different subject");
        return Ok(false);
    }
    let Ok(descriptor) = decode_host::<SharedStepDescriptor>(&step.descriptor) else {
        tracing::warn!(nonce = %step.nonce, "shared-run step descriptor does not decode; the step is left open");
        return Ok(false);
    };
    let contextual_observations = upload.observation.contextual_observations;
    let probe_verdict = match (&descriptor, contextual_observations.as_deref()) {
        (SharedStepDescriptor::Probe(request), Some(bytes)) => {
            Some(observed_probe_verdict(bytes, &step.nonce, &request.probe.check).unwrap_or(ProbeVerdict::Unknown))
        }
        (SharedStepDescriptor::Probe(_), None) => Some(ProbeVerdict::Unknown),
        _ => None,
    };
    let receipt = SharedStepReceipt {
        invocation: Digest::of_wire_bytes(format!("{}:{}", step.nonce, upload.detail.to_hex()).as_bytes()),
        evidence: evidence_for(upload.verdict, upload.subject, upload.detail),
        verdict: upload.verdict,
        failed_verifiers: upload.observation.failed_verifiers,
        failed_verifier_names: upload.observation.failed_verifier_names,
        findings: upload.observation.findings,
        cost: upload.observation.cost,
        calls: upload.observation.calls,
        contextual_observations,
        probe_verdict,
    };
    let bytes = encode_host(&receipt)?;
    if store.complete_shared_run_step(&step.nonce, &bytes, receipt.cost.map_or(0, |cost| cost.duration_millis))? {
        project_shared_findings(store, dispatch, &receipt)?;
        if matches!(descriptor, SharedStepDescriptor::ContextualFull { .. }) {
            record_independent_contextual_facts(store, dispatch, step, &receipt, host_class)?;
        }
        store.consume_order(&step.nonce)?;
    }
    Ok(true)
}

fn record_independent_contextual_facts(
    store: &mut dyn StoreBackend,
    dispatch: &SharedRunDispatch,
    step: &SharedRunStepRow,
    receipt: &SharedStepReceipt,
    host_class: &HostClass,
) -> rusqlite::Result<()> {
    let (SharedRunExecution::Contextual { node, .. }, Some(composition), Some(observations), Some(bloom)) = (
        &dispatch.execution,
        dispatch.plan.composition.as_ref(),
        receipt.contextual_observations.as_deref(),
        dispatch_bloom(dispatch),
    ) else {
        return Ok(());
    };
    if !contextual_contract_valid(dispatch)
        || composition.contract.host_class != host_class.digest()
        || !contextual_receipt_matches_node(receipt, node)
    {
        return Ok(());
    }
    let current = match contextual_bundle_reports(observations, &step.nonce, node, &composition.contract) {
        Ok(reports) => reports,
        Err(error) => {
            tracing::warn!(%error, nonce = %step.nonce, "contextual observation artifact refused");
            return Ok(());
        }
    };
    for prior_run in store.list_shared_runs()? {
        if prior_run.run == step.run {
            continue;
        }
        let Ok(prior_dispatch) = from_bytes::<SharedRunDispatch>(&prior_run.dispatch) else {
            continue;
        };
        let (SharedRunExecution::Contextual { node: prior_node, .. }, Some(prior_composition)) =
            (&prior_dispatch.execution, prior_dispatch.plan.composition.as_ref())
        else {
            continue;
        };
        if !same_contextual_fact_input(&prior_dispatch, dispatch) || !contextual_contract_valid(&prior_dispatch) {
            continue;
        }
        for prior_step in store.shared_run_steps(&prior_run.run)? {
            if !matches!(
                decode_host::<SharedStepDescriptor>(&prior_step.descriptor),
                Ok(SharedStepDescriptor::ContextualFull { .. })
            ) {
                continue;
            }
            let Some(prior_receipt) =
                prior_step.receipt.as_deref().and_then(|bytes| decode_host::<SharedStepReceipt>(bytes).ok())
            else {
                continue;
            };
            if !contextual_receipt_matches_node(&prior_receipt, prior_node) {
                continue;
            }
            let Some(prior_observations) = prior_receipt.contextual_observations.as_deref() else {
                continue;
            };
            let Ok(prior) = contextual_bundle_reports(
                prior_observations,
                &prior_step.nonce,
                prior_node,
                &prior_composition.contract,
            ) else {
                continue;
            };
            for report in &current {
                let Some(previous) = prior.iter().find(|candidate| candidate.gate == report.gate) else {
                    continue;
                };
                if let Err(error) = record_contextual_facts(
                    store,
                    node,
                    &composition.contract,
                    [previous, report],
                    host_class,
                    &step.nonce,
                    bloom.0.as_bytes(),
                ) {
                    tracing::warn!(%error, nonce = %step.nonce, "contextual proof facts were not recorded");
                }
            }
        }
    }
    Ok(())
}

fn same_contextual_fact_input(left: &SharedRunDispatch, right: &SharedRunDispatch) -> bool {
    let (
        SharedRunExecution::Contextual { node: left_node, .. },
        Some(left_composition),
        SharedRunExecution::Contextual { node: right_node, .. },
        Some(right_composition),
    ) = (&left.execution, left.plan.composition.as_ref(), &right.execution, right.plan.composition.as_ref())
    else {
        return false;
    };
    left_composition.contract == right_composition.contract
        && contextual_fact_key(left_node, &left_composition.contract)
            == contextual_fact_key(right_node, &right_composition.contract)
}

fn contextual_receipt_matches_node(receipt: &SharedStepReceipt, node: &aether_bloomery::SharedRunNode) -> bool {
    receipt.verdict != StageVerdict::ExecutorFault
        && receipt.evidence.kind == EvidenceKind::VerificationResult
        && receipt.evidence.subject == node.candidate.tree
}

fn settle_refused_preparation(store: &mut dyn StoreBackend, step: &SharedRunStepRow) -> rusqlite::Result<bool> {
    let Some(prepared) = step.prepared.as_deref() else {
        return Ok(false);
    };
    let SharedProbePreparation::Refused { request: _, detail } = decode_host::<SharedProbePreparation>(prepared)?
    else {
        return Ok(false);
    };
    let receipt = SharedStepReceipt {
        invocation: Digest::of_wire_bytes(format!("refused:{}", step.nonce).as_bytes()),
        evidence: Evidence { subject: Digest::default(), kind: EvidenceKind::ExecutorFault, detail },
        verdict: StageVerdict::ExecutorFault,
        failed_verifiers: VerifyFailureSet::default(),
        failed_verifier_names: Vec::new(),
        findings: None,
        cost: None,
        calls: None,
        contextual_observations: None,
        probe_verdict: Some(ProbeVerdict::Unknown),
    };
    store.complete_shared_run_step(&step.nonce, &encode_host(&receipt)?, 0)
}

fn member_outcome(request: &aether_bloomery::MemberVerifyRequest, receipt: &SharedStepReceipt) -> MemberVerifyOutcome {
    let request_id = request.digest();
    if receipt.verdict == StageVerdict::ExecutorFault {
        return MemberVerifyOutcome::HostFault { request: request_id, evidence: receipt.evidence.clone() };
    }
    if receipt.verdict == StageVerdict::VerificationPassed {
        return MemberVerifyOutcome::PassedStandalone {
            request: request_id,
            proof: VerifyProof {
                gate_set: request.contract.gate_set,
                stage: StageId::Verify,
                evidence: receipt.evidence.clone(),
            },
        };
    }
    MemberVerifyOutcome::Failed {
        request: request_id,
        scope: FailureScope::Unattributed { evidence: receipt.evidence.detail },
        failures: receipt.failed_verifiers,
        evidence: receipt.evidence.clone(),
    }
}

fn contextual_contract_valid(dispatch: &SharedRunDispatch) -> bool {
    let (Some(composition), SharedRunExecution::Contextual { node, transformation, profile, configs }) =
        (dispatch.plan.composition.as_ref(), &dispatch.execution)
    else {
        return true;
    };
    let template = &composition.contract.invocation;
    let mut inputs = Vec::with_capacity(template.extra_inputs.len().saturating_add(1));
    inputs.push(node.candidate.tree);
    inputs.extend_from_slice(&template.extra_inputs);
    let expected = aether_bloomery::Transformation {
        command: template.command.clone(),
        inputs,
        checkout: node.candidate.checkout,
        diff_base: template.diff_base,
        outputs: template.outputs.clone(),
        image: template.image.clone(),
        limits: template.limits,
        network: template.network,
        description: template.description.clone(),
        model: template.model.clone(),
    };
    composition.requests == dispatch.plan.requests
        && node.plan == dispatch.plan.digest()
        && **transformation == expected
        && profile == &template.profile
        && configs == &template.configs
        && composition.contract.members.len() == dispatch.plan.requests.len()
        && composition
            .contract
            .members
            .iter()
            .zip(&dispatch.plan.requests)
            .all(|(pin, request)| pin.request == request.digest() && pin.contract == request.contract.digest())
        && dispatch.plan.requests.iter().all(|request| {
            node.coverage.contains(&request.member)
                && request.contract.environment == composition.contract.environment
                && request.profile == template.profile
                && request.configs == template.configs
                && request.transformation.image == template.image
                && request.transformation.limits == template.limits
                && request.transformation.network == template.network
                && request.contract.obligations.iter().all(|obligation| match obligation {
                    VerificationObligation::Gate { identity } => {
                        composition.contract.gate_identities.contains(identity)
                    }
                    VerificationObligation::MemberDelta { .. } => true,
                })
                && request.contract.member_delta().is_some_and(|(scope_revision, candidate, diff_base)| {
                    *scope_revision == request.member.scope_revision
                        && *candidate == request.member.candidate
                        && *diff_base == request.contract.diff_base
                })
        })
}

fn refuse_invalid_shared_inputs(
    store: &mut dyn StoreBackend,
    row: &SharedRunRow,
    dispatch: &SharedRunDispatch,
    host_class: &HostClass,
    now_unix_millis: u64,
) -> rusqlite::Result<bool> {
    if contextual_contract_valid(dispatch)
        && !host_class.as_str().is_empty()
        && dispatch.plan.requests.iter().all(|request| request.contract.host_class == host_class.digest())
        && dispatch
            .plan
            .composition
            .as_ref()
            .is_none_or(|composition| composition.contract.host_class == host_class.digest())
    {
        return Ok(false);
    }
    let subject = match &dispatch.execution {
        SharedRunExecution::Contextual { node, .. } => node.candidate.tree,
        SharedRunExecution::Serial => {
            dispatch.plan.requests.first().map_or_else(Digest::default, |request| request.member.candidate.tree)
        }
    };
    let detail = Digest::of_wire_bytes(
        format!("aether.bloomery.shared_run.host_mismatch:{}:{}", host_class.as_str(), dispatch.plan.digest().to_hex())
            .as_bytes(),
    );
    for request in &dispatch.plan.requests {
        let outcome = MemberVerifyOutcome::HostFault {
            request: request.digest(),
            evidence: Evidence { subject, kind: EvidenceKind::ExecutorFault, detail },
        };
        let queued_unix_millis = store
            .shared_run_members(&row.run)?
            .into_iter()
            .find(|member| member.request == request.digest().as_bytes())
            .map_or(now_unix_millis, |member| member.queued_unix_millis);
        store.record_shared_run_member_outcome(
            &row.run,
            request.digest().as_bytes(),
            &to_vec(&outcome).map_err(|error| rusqlite::Error::InvalidParameterName(error.to_string()))?,
            now_unix_millis.saturating_sub(queued_unix_millis),
        )?;
    }
    store.update_shared_run(&row.run, SharedRunLifecycle::Completing, row.next_ordinal)?;
    Ok(true)
}

fn fold_serial_receipts(
    store: &mut dyn StoreBackend,
    row: &SharedRunRow,
    dispatch: &SharedRunDispatch,
    now_unix_millis: u64,
) -> rusqlite::Result<()> {
    for step in store.shared_run_steps(&row.run)? {
        let (Some(request_bytes), Some(receipt_bytes)) = (&step.request, &step.receipt) else {
            continue;
        };
        let Some(request) =
            dispatch.plan.requests.iter().find(|request| request_bytes.as_slice() == request.digest().as_bytes())
        else {
            continue;
        };
        if store.shared_run_members(&row.run)?.iter().any(|member| member.request == *request_bytes && member.cancelled)
        {
            continue;
        }
        let receipt = decode_host::<SharedStepReceipt>(receipt_bytes)?;
        let outcome = member_outcome(request, &receipt);
        if matches!(outcome, MemberVerifyOutcome::Failed { .. })
            && receipt.evidence.kind == EvidenceKind::VerificationResult
            && let Some(bloom) = dispatch_bloom(dispatch)
            && let Some(sequence) = shared_run_dispatch_sequence(row)
        {
            record_exact_member_findings(store, bloom, sequence, &request.member, receipt.evidence.detail)?;
        }
        let queued_unix_millis = store
            .shared_run_members(&row.run)?
            .into_iter()
            .find(|member| member.request == *request_bytes)
            .map_or(now_unix_millis, |member| member.queued_unix_millis);
        store.record_shared_run_member_outcome(
            &row.run,
            request_bytes,
            &to_vec(&outcome).map_err(|error| rusqlite::Error::InvalidParameterName(error.to_string()))?,
            now_unix_millis.saturating_sub(queued_unix_millis),
        )?;
    }
    Ok(())
}

fn derive_batch_members(requests: &[(&MemberPin, bool)], inputs: &[CompositionInput]) -> Vec<BatchMember> {
    let mut atomic = BTreeMap::<WorkpieceId, BTreeSet<WorkpieceId>>::new();
    for input in inputs {
        if requests.iter().any(|(request, _)| request.candidate == input.candidate) {
            continue;
        }
        let group = input
            .members
            .iter()
            .filter(|pin| requests.iter().any(|(request, _)| *request == *pin))
            .map(|pin| pin.workpiece.clone())
            .collect::<BTreeSet<_>>();
        for member in &group {
            atomic.entry(member.clone()).or_default().extend(group.iter().filter(|peer| *peer != member).cloned());
        }
    }
    requests
        .iter()
        .map(|(request, inherited)| {
            let exact =
                inputs.iter().find(|input| input.candidate == request.candidate && input.members.contains(*request));
            let carrying = exact.or_else(|| inputs.iter().find(|input| input.members.contains(*request)));
            let atomic_peers = atomic.get(&request.workpiece).cloned().unwrap_or_default();
            let dependencies = carrying.map_or_else(Vec::new, |input| {
                input
                    .members
                    .iter()
                    .filter(|pin| {
                        pin.workpiece != request.workpiece
                            && !atomic_peers.contains(&pin.workpiece)
                            && requests.iter().any(|(outstanding, _)| *outstanding == *pin)
                    })
                    .map(|pin| pin.workpiece.clone())
                    .collect()
            });
            BatchMember {
                workpiece: request.workpiece.clone(),
                dependencies,
                inherited: *inherited,
                atomic_peers: atomic_peers.into_iter().collect(),
            }
        })
        .collect()
}

fn selected_probe_inputs(
    inputs: &[CompositionInput],
    outstanding: &BTreeSet<WorkpieceId>,
    selected: &BTreeSet<WorkpieceId>,
) -> Vec<CompositionInput> {
    let mut seen = BTreeSet::new();
    inputs
        .iter()
        .filter(|input| {
            let pending = input
                .members
                .iter()
                .filter(|pin| outstanding.contains(&pin.workpiece))
                .map(|pin| &pin.workpiece)
                .collect::<Vec<_>>();
            !pending.is_empty()
                && pending.iter().all(|workpiece| selected.contains(*workpiece))
                && seen.insert(input.digest())
        })
        .cloned()
        .collect()
}

fn batch_members(dispatch: &SharedRunDispatch) -> Vec<BatchMember> {
    let inputs = dispatch.plan.composition.as_ref().map_or(&[][..], |composition| composition.inputs.as_slice());
    let requests =
        dispatch.plan.requests.iter().map(|request| (&request.member, request.context.is_some())).collect::<Vec<_>>();
    derive_batch_members(&requests, inputs)
}

fn declared_failed_checks(dispatch: &SharedRunDispatch, receipt: &SharedStepReceipt, nonce: &str) -> Vec<BatchCheck> {
    let Some(composition) = dispatch.plan.composition.as_ref() else {
        return Vec::new();
    };
    composition
        .contract
        .gate_identities
        .iter()
        .map(|identity| BatchCheck::Gate { id: identity.clone() })
        .filter(|check| {
            receipt.failed_verifier_names.iter().any(|failed| failed == check.gate())
                || receipt.contextual_observations.as_deref().is_some_and(|bytes| {
                    matches!(observed_probe_verdict(bytes, nonce, check), Ok(ProbeVerdict::Failed))
                })
        })
        .collect()
}

fn contextual_run_passed(verdict: StageVerdict, failed_checks: &[BatchCheck]) -> bool {
    verdict == StageVerdict::VerificationPassed && failed_checks.is_empty()
}

fn retained_probe_receipts(
    store: &mut dyn StoreBackend,
    row: &SharedRunRow,
    dispatch: &SharedRunDispatch,
    initial: &SharedRunStepRow,
    initial_receipt: &SharedStepReceipt,
    checks: &[BatchCheck],
) -> rusqlite::Result<Vec<BatchProbeReceipt>> {
    let mut receipts = Vec::new();
    for step in store.shared_run_steps(&row.run)? {
        let Ok(SharedStepDescriptor::Probe(preparation)) = decode_host(&step.descriptor) else {
            continue;
        };
        let Some(bytes) = step.receipt.as_deref() else {
            continue;
        };
        let Ok(receipt) = decode_host::<SharedStepReceipt>(bytes) else {
            tracing::warn!(nonce = %step.nonce, "probe receipt does not decode; it supplies no attribution evidence");
            continue;
        };
        receipts.push(BatchProbeReceipt {
            request: preparation.probe,
            invocation: receipt.invocation,
            verdict: receipt.probe_verdict.unwrap_or(ProbeVerdict::Unknown),
            evidence: receipt.evidence.detail,
        });
    }

    let (Some(bytes), SharedRunExecution::Contextual { node, .. }, Some(composition)) =
        (initial_receipt.contextual_observations.as_deref(), &dispatch.execution, dispatch.plan.composition.as_ref())
    else {
        return Ok(receipts);
    };
    let Ok(reports) = contextual_bundle_reports(bytes, &initial.nonce, node, &composition.contract) else {
        return Ok(receipts);
    };
    for check in checks {
        let key = check.observation_key();
        for (repetition, report) in reports
            .iter()
            .filter(|report| report.gate == check.gate())
            .filter_map(|report| {
                report.report.outcomes().find(|(observed, _)| *observed == key).map(|(_, verdict)| (report, verdict))
            })
            .take(2)
            .enumerate()
        {
            receipts.push(BatchProbeReceipt {
                request: BatchProbeRequest {
                    plan: dispatch.plan.digest(),
                    members: dispatch.plan.requests.iter().map(|request| request.member.workpiece.clone()).collect(),
                    baseline: None,
                    check: check.clone(),
                    repetition: u8::try_from(repetition).unwrap_or(1),
                },
                invocation: report.0.invocation,
                verdict: match report.1 {
                    ProofResult::Green => ProbeVerdict::Passed,
                    ProofResult::Red => ProbeVerdict::Failed,
                },
                evidence: initial_receipt.evidence.detail,
            });
        }
    }
    Ok(receipts)
}

fn probe_materialization(
    row: &SharedRunRow,
    dispatch: &SharedRunDispatch,
    probe: BatchProbeRequest,
) -> rusqlite::Result<SharedRunStepRow> {
    let Some(composition) = dispatch.plan.composition.as_ref() else {
        return Err(rusqlite::Error::InvalidParameterName("contextual plan has no composition".to_owned()));
    };
    let base = match probe.baseline.as_ref() {
        Some(member) => dispatch
            .plan
            .requests
            .iter()
            .find(|request| &request.member.workpiece == member)
            .and_then(|request| request.context.as_ref())
            .map(|context| context.starting_head.candidate)
            .ok_or_else(|| {
                rusqlite::Error::InvalidParameterName("inherited baseline has no pinned starting head".to_owned())
            })?,
        None => composition.base.candidate,
    };
    let selected = probe.members.iter().cloned().collect::<BTreeSet<_>>();
    let outstanding =
        dispatch.plan.requests.iter().map(|request| request.member.workpiece.clone()).collect::<BTreeSet<_>>();
    let inputs = selected_probe_inputs(&composition.inputs, &outstanding, &selected);
    let SharedRunExecution::Contextual { transformation, profile, configs, .. } = &dispatch.execution else {
        return Err(rusqlite::Error::InvalidParameterName("probe requested for serial run".to_owned()));
    };
    let ordinal = row.next_ordinal.saturating_add(1);
    let materialized = inputs
        .iter()
        .flat_map(|input| input.members.iter().map(|pin| pin.workpiece.clone()))
        .filter(|workpiece| outstanding.contains(workpiece))
        .collect::<BTreeSet<_>>();
    let request = SharedProbePreparationRequest {
        run: Digest::from_slice(&row.run)
            .ok_or_else(|| rusqlite::Error::InvalidParameterName("shared run identity is not a digest".to_owned()))?,
        ordinal,
        plan: dispatch.plan.digest(),
        probe,
        base,
        inputs,
        transformation: (**transformation).clone(),
        profile: profile.clone(),
        configs: configs.clone(),
    };
    let descriptor = encode_host(&SharedStepDescriptor::Probe(Box::new(request.clone())))?;
    let prepared = if materialized == selected {
        None
    } else {
        Some(encode_host(&SharedProbePreparation::Refused {
            request: request.probe.digest(),
            detail: Digest::of_wire_bytes(
                format!("aether.bloomery.unmaterializable_probe:{}", request.probe.digest().to_hex()).as_bytes(),
            ),
        })?)
    };
    Ok(SharedRunStepRow {
        run: row.run.clone(),
        ordinal,
        nonce: format!("{}-step-{ordinal}", row.nonce),
        request: None,
        descriptor,
        prepared,
        receipt: None,
        duration_millis: None,
        release_physical_run: false,
    })
}

fn scoped_evidence(template: &Evidence, detail: Digest) -> Evidence {
    Evidence { subject: template.subject, kind: template.kind, detail }
}

fn selected_failure<'a>(failures: &'a [BatchFailure], member: &WorkpieceId) -> Option<&'a BatchFailure> {
    failures
        .iter()
        .filter(|failure| match failure {
            BatchFailure::Attributed { member: attributed, .. } => attributed == member,
            BatchFailure::Interaction { members, .. } | BatchFailure::Unknown { members, .. } => {
                members.contains(member)
            }
            BatchFailure::Inherited { member: Some(inherited), .. } => inherited == member,
            BatchFailure::Inherited { member: None, .. } | BatchFailure::Infrastructure { .. } => true,
        })
        .min_by_key(|failure| match failure {
            BatchFailure::Attributed { .. } => 0,
            BatchFailure::Interaction { .. } => 1,
            BatchFailure::Inherited { member: Some(_), .. } => 2,
            BatchFailure::Inherited { member: None, .. } => 3,
            BatchFailure::Unknown { .. } => 4,
            BatchFailure::Infrastructure { .. } => 5,
        })
}

/// One gate identity as a position in `failures`' vocabulary: the compiled
/// identity of that name, or the lowest declared position this set has not
/// already spent — the same arrival interning a decoded row takes.
fn interned_gate(failures: VerifyFailureSet, gate: &str) -> Option<VerifyFailure> {
    VerifyFailure::from_name(gate).or_else(|| {
        (0..u8::try_from(MAX_VERIFIER_IDENTITIES).unwrap_or(u8::MAX))
            .find_map(|position| VerifyFailure::declared(position, gate).filter(|failure| !failures.contains(*failure)))
    })
}

/// Every gate identity this run failed: the executor's own verifier set plus
/// the gates only the observation artifact declared red.
///
/// The two sources are separate — [`declared_failed_checks`] already accepts
/// either — so a member failure has to carry their union. The reducer refuses
/// a `Failed` outcome whose failure set is empty, so an artifact-only red would
/// otherwise settle as invalid evidence and stall the whole completion (#5903).
fn contextual_failures(receipt: &SharedStepReceipt, checks: &[BatchCheck]) -> VerifyFailureSet {
    checks.iter().fold(receipt.failed_verifiers, |failures, check| {
        interned_gate(failures, check.gate()).map_or(failures, |failure| failures.union(VerifyFailureSet::one(failure)))
    })
}

fn contextual_outcomes(
    dispatch: &SharedRunDispatch,
    report: &BatchReport,
    receipt: &SharedStepReceipt,
    checks: &[BatchCheck],
) -> Vec<MemberVerifyOutcome> {
    let failures = contextual_failures(receipt, checks);
    let mut outcomes = Vec::new();
    let node = match &dispatch.execution {
        SharedRunExecution::Contextual { node, .. } => node.digest(),
        SharedRunExecution::Serial => Digest::default(),
    };
    for request in &dispatch.plan.requests {
        let request_id = request.digest();
        let failure = selected_failure(&report.failures, &request.member.workpiece);
        let outcome = match failure {
            Some(BatchFailure::Attributed { evidence, .. }) => MemberVerifyOutcome::Failed {
                request: request_id,
                scope: FailureScope::Attributed {
                    members: vec![request.member.clone()],
                    evidence: evidence.first().copied().unwrap_or(receipt.evidence.detail),
                },
                failures,
                evidence: scoped_evidence(
                    &receipt.evidence,
                    evidence.first().copied().unwrap_or(receipt.evidence.detail),
                ),
            },
            Some(BatchFailure::Interaction { members, evidence, .. }) => MemberVerifyOutcome::Failed {
                request: request_id,
                scope: FailureScope::Interaction {
                    members: dispatch
                        .plan
                        .requests
                        .iter()
                        .filter(|candidate| members.contains(&candidate.member.workpiece))
                        .map(|candidate| candidate.member.clone())
                        .collect(),
                    evidence: evidence.first().copied().unwrap_or(receipt.evidence.detail),
                },
                failures,
                evidence: scoped_evidence(
                    &receipt.evidence,
                    evidence.first().copied().unwrap_or(receipt.evidence.detail),
                ),
            },
            Some(BatchFailure::Inherited { member, evidence, .. }) => MemberVerifyOutcome::Failed {
                request: request_id,
                scope: FailureScope::Inherited {
                    head: member
                        .as_ref()
                        .and_then(|member| {
                            dispatch
                                .plan
                                .requests
                                .iter()
                                .find(|candidate| &candidate.member.workpiece == member)
                                .and_then(|candidate| candidate.context.as_ref())
                                .map(|context| context.starting_head.node)
                        })
                        .or_else(|| dispatch.plan.composition.as_ref().map(|composition| composition.base.node))
                        .unwrap_or_else(Digest::default),
                    evidence: evidence.first().copied().unwrap_or(receipt.evidence.detail),
                },
                failures,
                evidence: scoped_evidence(
                    &receipt.evidence,
                    evidence.first().copied().unwrap_or(receipt.evidence.detail),
                ),
            },
            Some(BatchFailure::Infrastructure { evidence, .. }) => MemberVerifyOutcome::HostFault {
                request: request_id,
                evidence: Evidence {
                    subject: receipt.evidence.subject,
                    kind: EvidenceKind::ExecutorFault,
                    detail: evidence.first().copied().unwrap_or(receipt.evidence.detail),
                },
            },
            Some(BatchFailure::Unknown { evidence, .. }) => MemberVerifyOutcome::Pending {
                request: request_id,
                observation: evidence.first().copied().unwrap_or(receipt.evidence.detail),
            },
            None if report.survivors.contains(&request.member.workpiece) => {
                MemberVerifyOutcome::Survived { request: request_id, node, observation: receipt.evidence.detail }
            }
            None => MemberVerifyOutcome::Pending { request: request_id, observation: receipt.evidence.detail },
        };
        outcomes.push(outcome);
    }
    outcomes
}

fn pending_contextual_outcomes(dispatch: &SharedRunDispatch, observation: Digest) -> Vec<MemberVerifyOutcome> {
    dispatch
        .plan
        .requests
        .iter()
        .map(|request| MemberVerifyOutcome::Pending { request: request.digest(), observation })
        .collect()
}

fn contextual_terminal_outcomes(
    store: &mut dyn StoreBackend,
    row: &SharedRunRow,
    dispatch: &SharedRunDispatch,
    steps: &[SharedRunStepRow],
) -> rusqlite::Result<Option<Vec<MemberVerifyOutcome>>> {
    let SharedRunExecution::Contextual { node, .. } = &dispatch.execution else {
        return Ok(None);
    };
    if let Some(reuse) =
        store.shared_run_proof_reuse(&row.run)?.map(|bytes| decode_host::<ReusedContextualProof>(&bytes)).transpose()?
    {
        return Ok(Some(
            dispatch
                .plan
                .requests
                .iter()
                .map(|request| MemberVerifyOutcome::PassedIn {
                    request: request.digest(),
                    node: node.digest(),
                    receipt: reuse.evidence.clone(),
                })
                .collect(),
        ));
    }
    let Some(step) = steps.iter().find(|step| {
        matches!(decode_host::<SharedStepDescriptor>(&step.descriptor), Ok(SharedStepDescriptor::ContextualFull { .. }))
    }) else {
        return Ok(None);
    };
    let Ok(receipt) = decode_host::<SharedStepReceipt>(step.receipt.as_deref().unwrap_or_default()) else {
        tracing::warn!(nonce = %step.nonce, "contextual receipt does not decode; the run holds rather than settles");
        return Ok(None);
    };
    let checks = declared_failed_checks(dispatch, &receipt, &step.nonce);
    if contextual_run_passed(receipt.verdict, &checks) {
        return Ok(Some(
            dispatch
                .plan
                .requests
                .iter()
                .map(|request| MemberVerifyOutcome::PassedIn {
                    request: request.digest(),
                    node: node.digest(),
                    receipt: receipt.evidence.clone(),
                })
                .collect(),
        ));
    }
    if receipt.verdict == StageVerdict::ExecutorFault {
        return Ok(Some(
            dispatch
                .plan
                .requests
                .iter()
                .map(|request| MemberVerifyOutcome::HostFault {
                    request: request.digest(),
                    evidence: receipt.evidence.clone(),
                })
                .collect(),
        ));
    }
    if checks.is_empty() {
        return Ok(Some(pending_contextual_outcomes(dispatch, receipt.evidence.detail)));
    }
    let receipts = retained_probe_receipts(store, row, dispatch, step, &receipt, &checks)?;
    Ok(
        match next_batch_probe(
            dispatch.plan.digest(),
            &batch_members(dispatch),
            &checks,
            &receipts,
            dispatch.plan.probe_budget,
        ) {
            BatchProgress::Probe(probe) => {
                let next = probe_materialization(row, dispatch, probe)?;
                store.record_shared_run_step(&next)?;
                store.update_shared_run(&row.run, SharedRunLifecycle::Running, next.ordinal)?;
                None
            }
            BatchProgress::Complete(report) => Some(contextual_outcomes(dispatch, &report, &receipt, &checks)),
            BatchProgress::Invalid(reason) => {
                let observation = Digest::of_wire_bytes(reason.as_bytes());
                Some(pending_contextual_outcomes(dispatch, observation))
            }
        },
    )
}

fn record_contextual_member_outcomes(
    store: &mut dyn StoreBackend,
    row: &SharedRunRow,
    dispatch: &SharedRunDispatch,
    outcomes: &[MemberVerifyOutcome],
    now_unix_millis: u64,
) -> rusqlite::Result<()> {
    let members = store.shared_run_members(&row.run)?;
    for outcome in outcomes {
        if members.iter().any(|member| member.request == outcome.request().as_bytes() && member.cancelled) {
            continue;
        }
        let outcome = if members.iter().any(|member| {
            member.request == outcome.request().as_bytes() && member.deadline_unix_millis <= now_unix_millis
        }) {
            MemberVerifyOutcome::Pending {
                request: outcome.request(),
                observation: Digest::of_wire_bytes(b"aether.bloomery.shared_run.member_deadline_expired"),
            }
        } else {
            outcome.clone()
        };
        project_attributed_member_findings(store, row, dispatch, &outcome)?;
        let latency = members
            .iter()
            .find(|member| member.request == outcome.request().as_bytes())
            .map_or(0, |member| now_unix_millis.saturating_sub(member.queued_unix_millis));
        store.record_shared_run_member_outcome(
            &row.run,
            outcome.request().as_bytes(),
            &to_vec(&outcome).map_err(|error| rusqlite::Error::InvalidParameterName(error.to_string()))?,
            latency,
        )?;
    }
    Ok(())
}

fn finish_if_terminal(
    store: &mut dyn StoreBackend,
    executor: &dyn ExecutorPort,
    row: &SharedRunRow,
    dispatch: &SharedRunDispatch,
    now_unix_millis: u64,
) -> rusqlite::Result<Vec<Admit>> {
    if row.lifecycle == SharedRunLifecycle::Completing {
        if store.shared_run_steps(&row.run)?.iter().any(|step| step.receipt.is_none()) {
            return Ok(Vec::new());
        }
        return replay_completion(store, row, executor);
    }
    if dispatch.plan.mode == SharedRunMode::Contextual {
        let steps = store.shared_run_steps(&row.run)?;
        if steps.iter().any(|step| step.receipt.is_none()) {
            return Ok(Vec::new());
        }
        let Some(outcomes) = contextual_terminal_outcomes(store, row, dispatch, &steps)? else {
            return Ok(Vec::new());
        };
        record_contextual_member_outcomes(store, row, dispatch, &outcomes, now_unix_millis)?;
        store.update_shared_run(&row.run, SharedRunLifecycle::Completing, row.next_ordinal)?;
        return replay_completion(store, row, executor);
    }

    fold_serial_receipts(store, row, dispatch, now_unix_millis)?;
    let members = store.shared_run_members(&row.run)?;
    if members
        .iter()
        .any(|member| !member.cancelled && member.outcome.is_none() && member.deadline_unix_millis > now_unix_millis)
    {
        if store.shared_run_steps(&row.run)?.iter().all(|step| step.receipt.is_some()) {
            store.update_shared_run(&row.run, SharedRunLifecycle::Ready, row.next_ordinal.saturating_add(1))?;
        }
        return Ok(Vec::new());
    }
    store.update_shared_run(&row.run, SharedRunLifecycle::Completing, row.next_ordinal)?;
    replay_completion(store, row, executor)
}

fn retain_contextual_proof_reuse(
    store: &mut dyn StoreBackend,
    artifacts: Option<&mut ArtifactsCapabilityState>,
    row: &SharedRunRow,
    dispatch: &SharedRunDispatch,
    host_class: &HostClass,
) -> rusqlite::Result<bool> {
    if dispatch.plan.mode != SharedRunMode::Contextual
        || store.shared_run_proof_reuse(&row.run)?.is_some()
        || !store.shared_run_steps(&row.run)?.is_empty()
    {
        return Ok(false);
    }
    let (SharedRunExecution::Contextual { node, .. }, Some(composition), Some(artifacts)) =
        (&dispatch.execution, dispatch.plan.composition.as_ref(), artifacts)
    else {
        return Ok(false);
    };
    let Some(witness) = reuse_contextual_proof(store, node, &composition.contract, host_class)? else {
        return Ok(false);
    };
    let witness_bytes = encode_host(&witness)?;
    let parents = vec![node.digest().to_hex(), composition.contract.digest().to_hex()];
    let PutResult::Ok { digest } = artifacts.put(&witness_bytes, &parents) else {
        return Ok(false);
    };
    let Some(detail) = Digest::from_hex(&digest) else {
        return Ok(false);
    };
    let reuse = ReusedContextualProof {
        witness,
        evidence: Evidence { subject: node.candidate.tree, kind: EvidenceKind::VerificationResult, detail },
    };
    store.record_shared_run_proof_reuse(&row.run, &encode_host(&reuse)?)?;
    Ok(true)
}

fn replay_completion(
    store: &mut dyn StoreBackend,
    row: &SharedRunRow,
    executor: &dyn ExecutorPort,
) -> rusqlite::Result<Vec<Admit>> {
    let current = store.lookup_shared_run(&row.run)?.ok_or_else(|| {
        rusqlite::Error::InvalidParameterName("shared run disappeared before completion replay".to_owned())
    })?;
    if !current.charged {
        return Ok(Vec::new());
    }
    let run = Digest::from_slice(&row.run)
        .ok_or_else(|| rusqlite::Error::InvalidParameterName("shared run identity is not a digest".to_owned()))?;
    let members = store.shared_run_members(&row.run)?;
    let (outcomes, unfinished) = retained_member_completion(&members)?;
    let dispatch = from_bytes::<SharedRunDispatch>(&row.dispatch)
        .map_err(|error| rusqlite::Error::InvalidParameterName(error.to_string()))?;
    let latencies = members
        .iter()
        .filter_map(|member| {
            let request = dispatch
                .plan
                .requests
                .iter()
                .find(|request| member.request.as_slice() == request.digest().as_bytes())?;
            Some(MemberVerifyLatency {
                request: request.digest(),
                member: request.member.clone(),
                latency_millis: member.latency_millis?,
            })
        })
        .collect();
    let completion = SharedRunCompletion { plan: dispatch.plan.digest(), run, outcomes, unfinished, latencies };
    let bloom = dispatch_bloom(&dispatch)
        .ok_or_else(|| rusqlite::Error::InvalidParameterName("shared run requests disagree on bloom".to_owned()))?;
    let event = completion_event(bloom, completion);
    if store.journal_holds_any(from_ref(&event.idempotency_key.0))? {
        if matches!(executor.release_physical_run(&run), Settled::Answered(Ok(()))) {
            store.update_shared_run(&row.run, SharedRunLifecycle::Completed, current.next_ordinal)?;
        }
        return Ok(Vec::new());
    }
    Ok(admit(&event).into_iter().collect())
}

fn retained_member_completion(
    members: &[SharedRunMemberRow],
) -> rusqlite::Result<(Vec<MemberVerifyOutcome>, Vec<Digest>)> {
    let mut outcomes = Vec::new();
    let mut unfinished = Vec::new();
    for member in members {
        if let Some(bytes) = member.outcome.as_deref() {
            outcomes.push(
                from_bytes(bytes).map_err(|error| {
                    rusqlite::Error::InvalidParameterName(format!("shared member outcome: {error}"))
                })?,
            );
        } else {
            unfinished.push(Digest::from_slice(&member.request).ok_or_else(|| {
                rusqlite::Error::InvalidParameterName("shared member request identity is not a digest".to_owned())
            })?);
        }
    }
    Ok((outcomes, unfinished))
}

fn add_cost(total: &mut StudyCost, value: StudyCost) {
    total.cost_micro_usd = total.cost_micro_usd.saturating_add(value.cost_micro_usd);
    total.turns = total.turns.saturating_add(value.turns);
    total.duration_millis = total.duration_millis.saturating_add(value.duration_millis);
    total.input_tokens = total.input_tokens.saturating_add(value.input_tokens);
    total.cache_write_tokens = total.cache_write_tokens.saturating_add(value.cache_write_tokens);
    total.cache_write_1h_tokens = total.cache_write_1h_tokens.saturating_add(value.cache_write_1h_tokens);
    total.cache_write_5m_tokens = total.cache_write_5m_tokens.saturating_add(value.cache_write_5m_tokens);
    total.cache_read_tokens = total.cache_read_tokens.saturating_add(value.cache_read_tokens);
    total.output_tokens = total.output_tokens.saturating_add(value.output_tokens);
}

fn materialize_physical_charge(
    store: &mut dyn StoreBackend,
    artifacts: Option<&mut ArtifactsCapabilityState>,
    row: &SharedRunRow,
    bloom: BloomId,
    run: Digest,
    cost: StudyCost,
) -> rusqlite::Result<bool> {
    let Some(artifacts) = artifacts else {
        return Ok(false);
    };
    record_shared_run_study(store, artifacts, bloom, run, cost)
        .map_err(|error| rusqlite::Error::InvalidParameterName(error.to_string()))?;
    store.claim_shared_run_charge(&row.run)?;
    Ok(true)
}

fn release_terminal_physical_run(executor: &dyn ExecutorPort, row: &SharedRunRow) -> rusqlite::Result<bool> {
    let run = Digest::from_slice(&row.run)
        .ok_or_else(|| rusqlite::Error::InvalidParameterName("shared run identity is not a digest".to_owned()))?;
    match executor.release_physical_run(&run) {
        Settled::Answered(Ok(())) => Ok(true),
        Settled::InFlight => Ok(false),
        Settled::Answered(Err(error)) => {
            tracing::warn!(%error, run = %run.to_hex(), "terminal shared-run lease release will retry");
            Ok(false)
        }
    }
}

fn charge_physical_run(
    store: &mut dyn StoreBackend,
    artifacts: Option<&mut ArtifactsCapabilityState>,
    row: &SharedRunRow,
    dispatch: &SharedRunDispatch,
) -> rusqlite::Result<bool> {
    if row.charged {
        return Ok(true);
    }
    let bloom = dispatch.plan.requests.first().map_or_else(|| BloomId(Digest::default()), |request| request.bloom);
    let run = Digest::from_slice(&row.run)
        .ok_or_else(|| rusqlite::Error::InvalidParameterName("shared run identity is not a digest".to_owned()))?;
    let cost = if let Some(bytes) = row.physical_cost.as_deref() {
        decode_host::<StudyCost>(bytes)?
    } else {
        let mut total = StudyCost::default();
        for step in store.shared_run_steps(&row.run)? {
            let (Some(prepared), Some(receipt)) = (step.prepared.as_deref(), step.receipt.as_deref()) else {
                continue;
            };
            let SharedProbePreparation::Prepared(prepared) = decode_host::<SharedProbePreparation>(prepared)? else {
                continue;
            };
            let receipt = decode_host::<SharedStepReceipt>(receipt)?;
            let Some(measured) = receipt.cost else {
                continue;
            };
            let subject = first_subject(&prepared.transformation).unwrap_or(prepared.candidate.tree);
            let record = DispatchRecord {
                nonce: Nonce(step.nonce),
                bloom,
                workpiece: WorkpieceId::composition(),
                scope_revision: dispatch.plan.digest(),
                candidate: subject,
                displayed_digest: subject,
                stage: StageId::AggregateVerify,
                transformation: prepared.transformation,
                configs: prepared.configs,
                profile: prepared.profile,
                instruction_bundle: None,
                prompt_manifest: None,
            };
            add_cost(&mut total, price_shared_run_step(store, &record, measured, receipt.calls.as_deref()));
        }
        let bytes = encode_host(&total)?;
        store.record_shared_run_cost(&row.run, &bytes)?;
        total
    };
    materialize_physical_charge(store, artifacts, row, bloom, run, cost)
}

fn pending_prepared_step(steps: &[SharedRunStepRow]) -> Option<&SharedRunStepRow> {
    steps.iter().find(|step| step.receipt.is_none() && step.prepared.is_some())
}

/// Drive every durable run from stored history. A process restart loses no
/// logical association or completed invocation: rows, not callbacks, select
/// the next step.
pub(super) fn drive_shared_runs(
    store: &mut dyn StoreBackend,
    artifacts: Option<&mut ArtifactsCapabilityState>,
    executor: &dyn ExecutorPort,
    claims: NameEvidenceClaims,
    host_class: &HostClass,
    now_unix_millis: u64,
) -> rusqlite::Result<Vec<Admit>> {
    let mut admits = Vec::new();
    let mut artifacts = artifacts;
    for row in store.list_open_shared_runs()? {
        if row.lifecycle == SharedRunLifecycle::Preparing {
            continue;
        }
        let Ok(dispatch) = from_bytes::<SharedRunDispatch>(&row.dispatch) else {
            tracing::warn!(nonce = %row.nonce, "shared-run dispatch does not decode; its siblings still drive");
            continue;
        };
        // A receipted step whose order is still outstanding is one whose
        // completion pass did not finish: the receipt is durable, its
        // projections are not. Replaying them is restart recovery, not routine
        // work — consuming the order closes the step, so a later tick neither
        // re-projects findings nor re-derives facts against the whole retained
        // run history, where a re-appended older green would supersede the red
        // that replaced it (#5903).
        for step in store.shared_run_steps(&row.run)?.into_iter().filter(|step| step.receipt.is_some()) {
            if store.lookup_order(&step.nonce)?.is_none() {
                continue;
            }
            if let Some(receipt) =
                step.receipt.as_deref().and_then(|bytes| decode_host::<SharedStepReceipt>(bytes).ok())
            {
                project_shared_findings(store, &dispatch, &receipt)?;
                if matches!(
                    decode_host::<SharedStepDescriptor>(&step.descriptor),
                    Ok(SharedStepDescriptor::ContextualFull { .. })
                ) {
                    record_independent_contextual_facts(store, &dispatch, &step, &receipt, host_class)?;
                }
            }
            store.consume_order(&step.nonce)?;
        }
        if matches!(row.lifecycle, SharedRunLifecycle::Ready | SharedRunLifecycle::Running)
            && refuse_invalid_shared_inputs(store, &row, &dispatch, host_class, now_unix_millis)?
        {
            let current = store.lookup_shared_run(&row.run)?.unwrap_or(row);
            if !current.charged {
                release_terminal_physical_run(executor, &current)?;
            }
            charge_physical_run(store, artifacts.as_deref_mut(), &current, &dispatch)?;
            admits.extend(replay_completion(store, &current, executor)?);
            continue;
        }
        if row.lifecycle == SharedRunLifecycle::Completing
            && store.shared_run_steps(&row.run)?.iter().all(|step| step.receipt.is_some())
        {
            if !row.charged {
                release_terminal_physical_run(executor, &row)?;
            }
            charge_physical_run(store, artifacts.as_deref_mut(), &row, &dispatch)?;
        }
        for step in store.shared_run_steps(&row.run)?.into_iter().filter(|step| step.receipt.is_none()) {
            if settle_refused_preparation(store, &step)? {
                continue;
            }
            complete_observed_step(store, executor, claims, host_class, &dispatch, &step)?;
        }
        admits.extend(finish_if_terminal(store, executor, &row, &dispatch, now_unix_millis)?);
        let Some(current) = store.lookup_shared_run(&row.run)? else {
            continue;
        };
        if current.lifecycle == SharedRunLifecycle::Completing
            && store.shared_run_steps(&current.run)?.iter().all(|step| step.receipt.is_some())
        {
            if !current.charged {
                release_terminal_physical_run(executor, &current)?;
            }
            charge_physical_run(store, artifacts.as_deref_mut(), &current, &dispatch)?;
        }
        if !matches!(current.lifecycle, SharedRunLifecycle::Ready | SharedRunLifecycle::Running) {
            continue;
        }
        if retain_contextual_proof_reuse(store, artifacts.as_deref_mut(), &current, &dispatch, host_class)? {
            admits.extend(finish_if_terminal(store, executor, &current, &dispatch, now_unix_millis)?);
            continue;
        }
        let steps = store.shared_run_steps(&row.run)?;
        if let Some(step) = pending_prepared_step(&steps) {
            submit_step(store, artifacts.as_deref_mut(), executor, &current, &dispatch, step, now_unix_millis)?;
            continue;
        }
        if steps.iter().any(|step| step.receipt.is_none()) {
            continue;
        }
        if let Some(step) = next_initial_step(store, &current, &dispatch, now_unix_millis)? {
            store.record_shared_run_step(&step)?;
            submit_step(store, artifacts.as_deref_mut(), executor, &current, &dispatch, &step, now_unix_millis)?;
        }
    }
    Ok(admits)
}

fn cancel_live_steps(
    store: &mut dyn StoreBackend,
    executor: &dyn ExecutorPort,
    row: &SharedRunRow,
) -> rusqlite::Result<bool> {
    for step in store.shared_run_steps(&row.run)?.into_iter().filter(|step| step.receipt.is_none()) {
        if store.lookup_order(&step.nonce)?.is_none() {
            store.complete_shared_run_step(&step.nonce, &encode_host(&cancelled_receipt(&step))?, 0)?;
            continue;
        }
        match executor.cancel(&WorkHandle::new(Nonce(step.nonce.clone()))) {
            Settled::InFlight => return Ok(false),
            Settled::Answered(Err(error)) => {
                tracing::warn!(%error, nonce = %step.nonce, "shared-run cancellation will retry");
                return Ok(false);
            }
            Settled::Answered(Ok(())) => {
                store.consume_order(&step.nonce)?;
                let cancelled = cancelled_receipt(&step);
                store.complete_shared_run_step(&step.nonce, &encode_host(&cancelled)?, 0)?;
            }
        }
    }
    Ok(true)
}

fn cancelled_receipt(step: &SharedRunStepRow) -> SharedStepReceipt {
    let detail = Digest::of_wire_bytes(format!("cancel:{}", step.nonce).as_bytes());
    SharedStepReceipt {
        invocation: detail,
        evidence: Evidence { subject: Digest::default(), kind: EvidenceKind::ExecutorFault, detail },
        verdict: StageVerdict::ExecutorFault,
        failed_verifiers: VerifyFailureSet::default(),
        failed_verifier_names: Vec::new(),
        findings: None,
        cost: None,
        calls: None,
        contextual_observations: None,
        probe_verdict: Some(ProbeVerdict::Unknown),
    }
}

fn pending_member_step(steps: Vec<SharedRunStepRow>, request: Digest) -> Option<SharedRunStepRow> {
    steps
        .into_iter()
        .find(|step| step.receipt.is_none() && step.request.as_deref() == Some(request.as_bytes().as_slice()))
}

fn cancel_member_step(
    store: &mut dyn StoreBackend,
    executor: &dyn ExecutorPort,
    row: &SharedRunRow,
    request: Digest,
) -> rusqlite::Result<bool> {
    let Some(step) = pending_member_step(store.shared_run_steps(&row.run)?, request) else {
        return Ok(true);
    };
    if store.lookup_order(&step.nonce)?.is_none() {
        store.complete_shared_run_step(&step.nonce, &encode_host(&cancelled_receipt(&step))?, 0)?;
        return Ok(true);
    }
    match executor.cancel(&WorkHandle::new(Nonce(step.nonce.clone()))) {
        Settled::InFlight => Ok(false),
        Settled::Answered(Err(error)) => {
            tracing::warn!(%error, nonce = %step.nonce, "shared member cancellation will retry");
            Ok(false)
        }
        Settled::Answered(Ok(())) => {
            store.consume_order(&step.nonce)?;
            let cancelled = cancelled_receipt(&step);
            store.complete_shared_run_step(&step.nonce, &encode_host(&cancelled)?, 0)?;
            Ok(true)
        }
    }
}

pub(super) fn drain_shared_cancellations(
    store: &mut dyn StoreBackend,
    executor: &dyn ExecutorPort,
) -> rusqlite::Result<()> {
    let mut ack = None;
    for entry in store.drain_topic(Topic::CancelMemberVerification)? {
        let Ok(payload) = from_bytes::<CoordinationCancelPayload>(&entry.payload) else {
            break;
        };
        store.cancel_shared_run_member(payload.subject.as_bytes())?;
        for row in store.list_open_shared_runs()? {
            if store.shared_run_members(&row.run)?.iter().any(|member| member.request == payload.subject.as_bytes())
                && !cancel_member_step(store, executor, &row, payload.subject)?
            {
                return Ok(());
            }
        }
        ack = Some(entry.sequence);
    }
    if let Some(sequence) = ack {
        store.ack_topic(Topic::CancelMemberVerification, sequence)?;
    }

    let mut run_ack = None;
    for entry in store.drain_topic(Topic::CancelSharedRun)? {
        let Ok(payload) = from_bytes::<CoordinationCancelPayload>(&entry.payload) else {
            break;
        };
        store.record_shared_run_cancellation(payload.subject.as_bytes())?;
        let mut waiting = false;
        for row in store.list_open_shared_runs()? {
            let dispatch = from_bytes::<SharedRunDispatch>(&row.dispatch)
                .map_err(|error| rusqlite::Error::InvalidParameterName(error.to_string()))?;
            if dispatch.plan.digest() != payload.subject {
                continue;
            }
            for member in store.shared_run_members(&row.run)? {
                store.cancel_shared_run_member(&member.request)?;
            }
            store.update_shared_run(&row.run, SharedRunLifecycle::Completing, row.next_ordinal)?;
            waiting |= !cancel_live_steps(store, executor, &row)?;
        }
        if waiting {
            break;
        }
        run_ack = Some(entry.sequence);
    }
    if let Some(sequence) = run_ack {
        store.ack_topic(Topic::CancelSharedRun, sequence)?;
    }
    Ok(())
}

/// Join backend checkpoint observations to the original trusted dispatch. The
/// backend supplies no bloom/member identity, so a fabricated nonce cannot
/// become a coordination fact and a stale checkout version is rejected.
pub(super) fn observe_construction_checkpoints(
    store: &mut dyn StoreBackend,
    executor: &dyn ExecutorPort,
) -> rusqlite::Result<Vec<Admit>> {
    let Settled::Answered(observations) = executor.observe_construction_checkpoints() else {
        return Ok(Vec::new());
    };
    let mut admits = Vec::new();
    for observation in observations {
        let Some(stored) = store.lookup_order(&observation.nonce.0)? else {
            continue;
        };
        if stored.lifecycle != OrderLifecycle::Submitted {
            continue;
        }
        let Some(record) = DispatchRecord::from_stored(&stored) else {
            continue;
        };
        if record.stage != StageId::Construct || record.transformation.checkout != observation.starting_checkout {
            continue;
        }
        let checkpoint = ConstructionCheckpoint {
            bloom: record.bloom,
            workpiece: record.workpiece,
            scope_revision: record.scope_revision,
            nonce: construction_nonce_digest(&observation.nonce),
            observation: observation.observation,
            starting_checkout: observation.starting_checkout,
            candidate: observation.candidate,
        };
        let event = Event {
            idempotency_key: IdempotencyKey(format!(
                "aether.bloomery.construction_checkpoint:{}:{}:{}",
                observation.nonce.0,
                checkpoint.observation,
                checkpoint.digest().to_hex(),
            )),
            fact: Fact::ConstructionCheckpointObserved { checkpoint },
        };
        if !store.journal_holds_any(from_ref(&event.idempotency_key.0))?
            && let Some(admission) = admit(&event)
        {
            admits.push(admission);
        }
    }
    Ok(admits)
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, iter::once};

    use super::*;
    use crate::bloomery::executor::{ExecutorPortError, RunObservation};
    use crate::store::{RecordOutcome, SqliteStore};
    use aether_bloomery::{
        AgentProfile, BackendId, CandidateRef, CompositionContract, CompositionInput, CompositionPlan, ConfigRegistry,
        ContextualInvocationTemplate, ExecutionLimits, Harness, IntegrationHead, MemberContractPin, MemberPin,
        MemberVerifyRequest, NetworkProfile, ObservedLaneWrites, ReasoningEffort, SharedRunNode, SharedRunPlan,
        ToolPolicy, Transformation, VerificationContract, VerificationObligation, WorkOrder,
    };

    struct ReleasePort {
        releases: Cell<u32>,
    }

    impl ExecutorPort for ReleasePort {
        fn backend_for(&self, _: &WorkHandle) -> BackendId {
            BackendId::SOLE
        }

        fn submit(&self, _: &WorkOrder) -> Settled<Result<WorkHandle, ExecutorPortError>> {
            Settled::InFlight
        }

        fn observe(&self, _: &WorkHandle) -> Settled<Result<RunObservation, ExecutorPortError>> {
            Settled::InFlight
        }

        fn cancel(&self, _: &WorkHandle) -> Settled<Result<(), ExecutorPortError>> {
            Settled::Answered(Ok(()))
        }

        fn release_physical_run(&self, _: &Digest) -> Settled<Result<(), ExecutorPortError>> {
            self.releases.set(self.releases.get().saturating_add(1));
            Settled::Answered(Ok(()))
        }

        fn observe_writes(&self) -> Settled<Vec<ObservedLaneWrites>> {
            Settled::Answered(Vec::new())
        }
    }

    fn pin(name: &str, candidate: u8) -> MemberPin {
        MemberPin {
            workpiece: WorkpieceId(name.to_owned()),
            scope_revision: Digest::of_wire_bytes(format!("scope:{name}").as_bytes()),
            candidate: CandidateRef {
                tree: Digest::of_wire_bytes(&[candidate, 0]),
                checkout: Digest::of_wire_bytes(&[candidate, 1]),
            },
        }
    }

    fn contextual_member_request(
        bloom: BloomId,
        base: CandidateRef,
        member: &MemberPin,
        limits: ExecutionLimits,
        profile: &AgentProfile,
        configs: &ConfigRegistry,
    ) -> MemberVerifyRequest {
        MemberVerifyRequest {
            bloom,
            member: member.clone(),
            input: CompositionInput {
                node: member.candidate.tree,
                candidate: member.candidate,
                members: vec![member.clone()],
            },
            attempt: 0,
            context: None,
            contract: VerificationContract {
                gate_set: Digest::of_wire_bytes(b"gate-set"),
                obligations: vec![
                    VerificationObligation::Gate { identity: "verify.test".to_owned() },
                    VerificationObligation::MemberDelta {
                        scope_revision: member.scope_revision,
                        candidate: member.candidate,
                        diff_base: base,
                    },
                ],
                diff_base: base,
                invocation: Digest::of_wire_bytes(b"member-invocation"),
                environment: Digest::of_wire_bytes(b"environment"),
                host_class: Digest::of_wire_bytes(b"host-class"),
            },
            transformation: Transformation {
                command: "verify.member".to_owned(),
                inputs: vec![member.candidate.tree],
                checkout: member.candidate.checkout,
                diff_base: Some(base.checkout),
                outputs: Vec::new(),
                image: "verify".to_owned(),
                limits,
                network: NetworkProfile::None,
                description: None,
                model: None,
            },
            profile: profile.clone(),
            configs: configs.clone(),
        }
    }

    fn reducer_shaped_contextual_dispatch() -> SharedRunDispatch {
        let bloom = BloomId(Digest::of_wire_bytes(b"bloom"));
        let base = CandidateRef {
            tree: Digest::of_wire_bytes(b"base-tree"),
            checkout: Digest::of_wire_bytes(b"base-checkout"),
        };
        let member = pin("member", 17);
        let limits = ExecutionLimits { wall_clock_secs: 60 };
        let profile = AgentProfile {
            harness: Harness::Grok,
            model: "test".to_owned(),
            effort: ReasoningEffort::Low,
            tools: ToolPolicy::None,
        };
        let configs = ConfigRegistry::default();
        let request = contextual_member_request(bloom, base, &member, limits, &profile, &configs);
        let template = ContextualInvocationTemplate {
            command: "verify.check".to_owned(),
            extra_inputs: Vec::new(),
            diff_base: Some(base.checkout),
            outputs: Vec::new(),
            image: "verify".to_owned(),
            limits,
            network: NetworkProfile::None,
            description: None,
            model: None,
            profile: profile.clone(),
            configs: configs.clone(),
        };
        let composition = CompositionPlan {
            bloom,
            base: IntegrationHead {
                generation: Digest::of_wire_bytes(b"generation"),
                node: base.tree,
                candidate: base,
                plan: Digest::of_wire_bytes(b"base-plan"),
                coverage: Vec::new(),
            },
            inputs: vec![request.input.clone()],
            requests: vec![request.clone()],
            contract: CompositionContract {
                gate_set: request.contract.gate_set,
                gate_identities: vec!["verify.test".to_owned()],
                members: vec![MemberContractPin { request: request.digest(), contract: request.contract.digest() }],
                invocation: template.clone(),
                environment: request.contract.environment,
                host_class: request.contract.host_class,
            },
        };
        let plan = SharedRunPlan {
            mode: SharedRunMode::Contextual,
            requests: vec![request],
            composition: Some(composition),
            probe_budget: 4,
            execution_attempt: 0,
        };
        let node = SharedRunNode { plan: plan.digest(), candidate: member.candidate, coverage: vec![member] };
        SharedRunDispatch {
            execution: SharedRunExecution::Contextual {
                transformation: Box::new(template.instantiate(node.candidate)),
                node: Box::new(node),
                profile,
                configs,
            },
            plan,
        }
    }

    fn step(request: Digest, ordinal: u32, completed: bool) -> SharedRunStepRow {
        SharedRunStepRow {
            run: Digest::default().as_bytes().to_vec(),
            ordinal,
            nonce: format!("step-{ordinal}"),
            request: Some(request.as_bytes().to_vec()),
            descriptor: Vec::new(),
            prepared: Some(Vec::new()),
            receipt: completed.then(Vec::new),
            duration_millis: completed.then_some(1),
            release_physical_run: false,
        }
    }

    #[test]
    fn repaired_composition_input_is_atomic_without_a_dependency_cycle() {
        let a = pin("A", 1);
        let b = pin("B", 2);
        let repaired = CompositionInput {
            node: Digest::of_wire_bytes(b"repaired-ab"),
            candidate: CandidateRef {
                tree: Digest::of_wire_bytes(b"repaired-tree"),
                checkout: Digest::of_wire_bytes(b"repaired-checkout"),
            },
            members: vec![a.clone(), b.clone()],
        };

        let members = derive_batch_members(&[(&a, false), (&b, false)], &[repaired]);

        assert_eq!(members[0].dependencies, Vec::<WorkpieceId>::new());
        assert_eq!(members[1].dependencies, Vec::<WorkpieceId>::new());
        assert_eq!(members[0].atomic_peers, vec![b.workpiece]);
        assert_eq!(members[1].atomic_peers, vec![a.workpiece]);
    }

    #[test]
    fn base_covered_parent_is_not_an_absent_dependency_and_its_input_stays_atomic() {
        let a = pin("A", 1);
        let b = pin("B", 2);
        let repaired = CompositionInput {
            node: Digest::of_wire_bytes(b"repaired-b-on-a"),
            candidate: CandidateRef {
                tree: Digest::of_wire_bytes(b"repaired-tree"),
                checkout: Digest::of_wire_bytes(b"repaired-checkout"),
            },
            members: vec![a, b.clone()],
        };

        let members = derive_batch_members(&[(&b, true)], from_ref(&repaired));
        assert!(members[0].dependencies.is_empty());
        assert!(members[0].atomic_peers.is_empty());

        let outstanding = once(b.workpiece.clone()).collect();
        let selected = once(b.workpiece).collect();
        assert_eq!(selected_probe_inputs(from_ref(&repaired), &outstanding, &selected), vec![repaired]);
    }

    #[test]
    fn nominal_umbrella_green_with_a_declared_raw_red_requires_diagnosis() {
        let raw_red = vec![BatchCheck::Gate { id: "verify.test".to_owned() }];

        assert!(!contextual_run_passed(StageVerdict::VerificationPassed, &raw_red));
        assert!(contextual_run_passed(StageVerdict::VerificationPassed, &[]));
    }

    #[test]
    fn exact_attribution_is_not_masked_by_earlier_generic_failures() {
        let member = WorkpieceId("C".to_owned());
        let check = BatchCheck::Gate { id: "verify.test".to_owned() };
        let failures = vec![
            BatchFailure::Infrastructure { check: check.clone(), evidence: vec![Digest::of_wire_bytes(b"host")] },
            BatchFailure::Inherited {
                member: None,
                check: check.clone(),
                evidence: vec![Digest::of_wire_bytes(b"base")],
            },
            BatchFailure::Unknown {
                members: vec![member.clone()],
                check: check.clone(),
                evidence: vec![Digest::of_wire_bytes(b"unknown")],
            },
            BatchFailure::Attributed {
                member: member.clone(),
                check,
                evidence: vec![Digest::of_wire_bytes(b"attributed")],
            },
        ];

        assert!(matches!(selected_failure(&failures, &member), Some(BatchFailure::Attributed { .. })));
    }

    #[test]
    fn one_member_cancellation_selects_only_its_pending_sibling_step() {
        let a = Digest::of_wire_bytes(b"request-a");
        let b = Digest::of_wire_bytes(b"request-b");
        let selected = pending_member_step(vec![step(a, 1, false), step(b, 2, false)], a).expect("A is pending");

        assert_eq!(selected.request.as_deref(), Some(a.as_bytes().as_slice()));
        assert_ne!(selected.request.as_deref(), Some(b.as_bytes().as_slice()));
    }

    #[test]
    fn physical_cost_recovery_accumulates_every_completed_step() {
        let mut total = StudyCost::default();
        add_cost(&mut total, StudyCost { cost_micro_usd: 10, duration_millis: 20, turns: 1, ..StudyCost::default() });
        add_cost(&mut total, StudyCost { cost_micro_usd: 30, duration_millis: 40, turns: 2, ..StudyCost::default() });

        assert_eq!(total.cost_micro_usd, 40);
        assert_eq!(total.duration_millis, 60);
        assert_eq!(total.turns, 3);
    }

    #[test]
    fn prepared_step_without_a_receipt_is_resubmitted_after_recovery() {
        let completed = step(Digest::of_wire_bytes(b"completed"), 1, true);
        let pending = step(Digest::of_wire_bytes(b"pending"), 2, false);

        assert_eq!(pending_prepared_step(&[completed, pending]).map(|step| step.ordinal), Some(2));
    }

    #[test]
    fn completion_replay_decodes_wire_outcomes_and_accounts_cancelled_members() {
        let completed = Digest::of_wire_bytes(b"completed-request");
        let cancelled = Digest::of_wire_bytes(b"cancelled-request");
        let outcome = MemberVerifyOutcome::Pending {
            request: completed,
            observation: Digest::of_wire_bytes(b"retained-observation"),
        };
        let members = vec![
            SharedRunMemberRow {
                run: Digest::default().as_bytes().to_vec(),
                request: completed.as_bytes().to_vec(),
                ordinal: 0,
                queued_unix_millis: 1,
                deadline_unix_millis: 2,
                cancelled: false,
                outcome: Some(to_vec(&outcome).expect("member outcome encodes")),
                latency_millis: Some(1),
            },
            SharedRunMemberRow {
                run: Digest::default().as_bytes().to_vec(),
                request: cancelled.as_bytes().to_vec(),
                ordinal: 1,
                queued_unix_millis: 1,
                deadline_unix_millis: 2,
                cancelled: true,
                outcome: None,
                latency_millis: None,
            },
        ];

        let (outcomes, unfinished) = retained_member_completion(&members).expect("completion state replays");

        assert_eq!(outcomes, vec![outcome]);
        assert_eq!(unfinished, vec![cancelled], "withdrawal still accounts for the original run request");
    }

    #[test]
    fn artifact_outage_retains_cost_and_retries_the_single_physical_charge() {
        let mut store = SqliteStore::open(":memory:").expect("store");
        let bloom = BloomId(Digest::of_wire_bytes(b"bloom"));
        let run = Digest::of_wire_bytes(b"physical-run");
        let row = SharedRunRow {
            run: run.as_bytes().to_vec(),
            nonce: "shared-run".to_owned(),
            dispatch: b"immutable-dispatch".to_vec(),
            lifecycle: SharedRunLifecycle::Completing,
            next_ordinal: 2,
            deadline_unix_millis: 10,
            charged: false,
            physical_cost: None,
        };
        store.record_shared_run(&row, &[]).expect("run");
        let cost = StudyCost { cost_micro_usd: 40, duration_millis: 60, ..StudyCost::default() };
        store.record_shared_run_cost(&row.run, &encode_host(&cost).expect("cost wire")).expect("durable cost");

        assert!(!materialize_physical_charge(&mut store, None, &row, bloom, run, cost).expect("outage is retryable"));
        let waiting = store.lookup_shared_run(&row.run).expect("lookup").expect("run remains");
        assert!(!waiting.charged);
        assert!(waiting.physical_cost.is_some());
        assert_eq!(waiting.lifecycle, SharedRunLifecycle::Completing);
        let executor = ReleasePort { releases: Cell::new(0) };
        assert!(release_terminal_physical_run(&executor, &waiting).expect("terminal lease release"));
        assert_eq!(executor.releases.get(), 1);
        let released = store.lookup_shared_run(&row.run).expect("lookup").expect("run remains");
        assert!(!released.charged, "lease release cannot stand in for study materialization");
        assert_eq!(released.lifecycle, SharedRunLifecycle::Completing, "logical completion still waits");

        let directory = tempfile::tempdir().expect("artifact root");
        let mut artifacts = ArtifactsCapabilityState::open(directory.path()).expect("artifacts");
        assert!(
            materialize_physical_charge(&mut store, Some(&mut artifacts), &waiting, bloom, run, cost)
                .expect("recovery writes study")
        );
        let charged = store.lookup_shared_run(&row.run).expect("lookup").expect("run remains");
        assert!(charged.charged);
        let first = store.lookup_study(bloom.0.as_bytes(), run.as_bytes()).expect("study lookup").expect("study");

        assert!(
            materialize_physical_charge(&mut store, Some(&mut artifacts), &waiting, bloom, run, cost)
                .expect("replayed materialization is idempotent")
        );
        assert_eq!(store.lookup_study(bloom.0.as_bytes(), run.as_bytes()).expect("study lookup"), Some(first));
    }

    #[test]
    fn retained_proof_reuse_atomically_marks_zero_physical_work_charged() {
        let mut store = SqliteStore::open(":memory:").expect("store");
        let run = Digest::of_wire_bytes(b"reused-proof-run");
        let row = SharedRunRow {
            run: run.as_bytes().to_vec(),
            nonce: "reused-proof".to_owned(),
            dispatch: b"dispatch".to_vec(),
            lifecycle: SharedRunLifecycle::Ready,
            next_ordinal: 0,
            deadline_unix_millis: 10,
            charged: false,
            physical_cost: None,
        };
        store.record_shared_run(&row, &[]).expect("run");
        store.record_shared_run_proof_reuse(&row.run, b"witness").expect("atomic witness retention");

        let retained = store.lookup_shared_run(&row.run).expect("lookup").expect("run");
        assert!(retained.charged, "a crash after witness retention cannot enter physical zero-cost charging");
        assert!(retained.physical_cost.is_none(), "reuse creates no physical cost record");
        assert!(store.shared_run_steps(&row.run).expect("steps").is_empty(), "reuse creates no invocation receipt");
        assert_eq!(store.shared_run_proof_reuse(&row.run).expect("reuse"), Some(b"witness".to_vec()));
    }

    #[test]
    fn shared_receipt_projects_exact_findings_without_an_artifact_store_object() {
        let mut store = SqliteStore::open(":memory:").expect("store");
        let bloom = BloomId(Digest::of_wire_bytes(b"bloom"));
        let detail = Digest::of_wire_bytes(b"local evidence bytes");
        let receipt = SharedStepReceipt {
            invocation: Digest::of_wire_bytes(b"invocation"),
            evidence: Evidence {
                subject: Digest::of_wire_bytes(b"composed tree"),
                kind: EvidenceKind::VerificationResult,
                detail,
            },
            verdict: StageVerdict::VerificationFailed,
            failed_verifiers: VerifyFailureSet::default(),
            failed_verifier_names: Vec::new(),
            findings: Some("the exact composed failure".to_owned()),
            cost: None,
            calls: None,
            contextual_observations: None,
            probe_verdict: None,
        };

        project_shared_findings_for_bloom(&mut store, bloom, &receipt).expect("retain exact findings");

        assert_eq!(
            partial_repair_findings(&mut store, None, bloom, detail).expect("exact findings").as_deref(),
            Some("the exact composed failure")
        );
        assert!(
            partial_repair_findings(&mut store, None, bloom, Digest::of_wire_bytes(b"newer unrelated evidence"))
                .expect("unrelated lookup")
                .is_none()
        );
    }

    #[test]
    fn only_exact_attribution_selects_a_member_repair_diagnostic() {
        let member = pin("member", 7);
        let request = Digest::of_wire_bytes(b"member request");
        let detail = Digest::of_wire_bytes(b"attributed evidence");
        let evidence =
            Evidence { subject: Digest::of_wire_bytes(b"group node"), kind: EvidenceKind::VerificationResult, detail };
        let attributed = MemberVerifyOutcome::Failed {
            request,
            scope: FailureScope::Attributed { members: vec![member.clone()], evidence: detail },
            failures: VerifyFailureSet::default(),
            evidence: evidence.clone(),
        };
        assert_eq!(attributed_member_evidence(&attributed, request, &member), Some(detail));

        for scope in [
            FailureScope::Interaction { members: vec![member.clone()], evidence: detail },
            FailureScope::Inherited { head: Digest::of_wire_bytes(b"head"), evidence: detail },
            FailureScope::Unattributed { evidence: detail },
        ] {
            let outcome = MemberVerifyOutcome::Failed {
                request,
                scope,
                failures: VerifyFailureSet::default(),
                evidence: evidence.clone(),
            };
            assert_eq!(attributed_member_evidence(&outcome, request, &member), None);
        }
    }

    #[test]
    fn attributed_member_findings_replay_without_stale_run_overwrite() {
        let mut store = SqliteStore::open(":memory:").expect("store");
        let bloom = BloomId(Digest::of_wire_bytes(b"bloom"));
        let older = pin("member", 8);
        let newer = pin("member", 9);
        let older_evidence = Digest::of_wire_bytes(b"older evidence");
        let newer_evidence = Digest::of_wire_bytes(b"newer evidence");
        store
            .record_review_findings(bloom.0.as_bytes(), &verification_findings_key(older_evidence), "older finding")
            .expect("older exact finding");
        store
            .record_review_findings(bloom.0.as_bytes(), &verification_findings_key(newer_evidence), "newer finding")
            .expect("newer exact finding");

        record_exact_member_findings(&mut store, bloom, 20, &newer, newer_evidence).expect("newer projection");
        record_exact_member_findings(&mut store, bloom, 20, &newer, newer_evidence).expect("newer replay");
        record_exact_member_findings(&mut store, bloom, 10, &older, older_evidence).expect("stale replay");

        assert_eq!(
            store.lookup_review_findings(bloom.0.as_bytes(), &newer.workpiece.0).expect("member advisory").as_deref(),
            Some("newer finding")
        );
        let guard = store
            .lookup_review_findings(bloom.0.as_bytes(), &shared_member_findings_guard_key(&newer.workpiece))
            .expect("guard lookup")
            .expect("guard");
        assert_eq!(guarded_shared_member_findings_key(&guard), Some(shared_member_findings_key(&newer).as_str()));
        assert!(
            store
                .lookup_review_findings(bloom.0.as_bytes(), &shared_member_findings_key(&older))
                .expect("stale exact lookup")
                .is_none()
        );
        assert_eq!(
            super::super::shared_member_advisory(&mut store, bloom.0.as_bytes(), &newer)
                .expect("newer prompt lookup")
                .as_deref(),
            Some("newer finding")
        );
        assert!(
            super::super::shared_member_advisory(&mut store, bloom.0.as_bytes(), &older)
                .expect("stale prompt lookup")
                .is_none()
        );
    }

    #[test]
    fn cancellation_tombstone_survives_before_the_physical_dispatch() {
        let mut store = SqliteStore::open(":memory:").expect("store");
        let plan = Digest::of_wire_bytes(b"cancelled-before-dispatch");

        assert_eq!(
            store.record_shared_run_cancellation(plan.as_bytes()).expect("retain cancellation"),
            RecordOutcome::Recorded
        );
        assert!(store.shared_run_cancelled(plan.as_bytes()).expect("read cancellation"));
        assert_eq!(
            store.record_shared_run_cancellation(plan.as_bytes()).expect("replay cancellation"),
            RecordOutcome::Duplicate
        );
    }

    #[test]
    fn executor_fault_cannot_reuse_leftover_contextual_observations() {
        let node = SharedRunNode {
            plan: Digest::of_wire_bytes(b"plan"),
            candidate: CandidateRef {
                tree: Digest::of_wire_bytes(b"node-tree"),
                checkout: Digest::of_wire_bytes(b"node-checkout"),
            },
            coverage: Vec::new(),
        };
        let mut receipt = SharedStepReceipt {
            invocation: Digest::of_wire_bytes(b"invocation"),
            evidence: Evidence {
                subject: node.candidate.tree,
                kind: EvidenceKind::ExecutorFault,
                detail: Digest::of_wire_bytes(b"fault"),
            },
            verdict: StageVerdict::ExecutorFault,
            failed_verifiers: VerifyFailureSet::default(),
            failed_verifier_names: Vec::new(),
            findings: None,
            cost: None,
            calls: None,
            contextual_observations: Some(b"stale but parseable observations".to_vec()),
            probe_verdict: None,
        };

        assert!(!contextual_receipt_matches_node(&receipt, &node));
        receipt.verdict = StageVerdict::VerificationPassed;
        receipt.evidence.kind = EvidenceKind::VerificationResult;
        assert!(contextual_receipt_matches_node(&receipt, &node));
        receipt.evidence.subject = Digest::of_wire_bytes(b"another-tree");
        assert!(!contextual_receipt_matches_node(&receipt, &node));
    }

    #[test]
    fn an_artifact_only_gate_failure_still_names_a_failing_verifier() {
        // The executor's own verifier set and the observation artifact are two
        // sources for the same question. A run whose gate only failed in the
        // artifact would otherwise produce `Failed { failures: empty }`, which
        // the reducer refuses as invalid evidence for the whole completion —
        // the run then never settles at all.
        let dispatch = reducer_shaped_contextual_dispatch();
        let member = dispatch.plan.requests[0].member.workpiece.clone();
        let detail = Digest::of_wire_bytes(b"artifact evidence");
        let receipt = SharedStepReceipt {
            invocation: Digest::of_wire_bytes(b"invocation"),
            evidence: Evidence {
                subject: Digest::of_wire_bytes(b"composed tree"),
                kind: EvidenceKind::VerificationResult,
                detail,
            },
            verdict: StageVerdict::VerificationFailed,
            failed_verifiers: VerifyFailureSet::default(),
            failed_verifier_names: Vec::new(),
            findings: None,
            cost: None,
            calls: None,
            contextual_observations: None,
            probe_verdict: None,
        };
        let failed = |gate: &str| {
            let check = BatchCheck::Gate { id: gate.to_owned() };
            let report = BatchReport {
                failures: vec![BatchFailure::Attributed {
                    member: member.clone(),
                    check: check.clone(),
                    evidence: vec![detail],
                }],
                ejected: Vec::new(),
                survivors: Vec::new(),
            };
            match contextual_outcomes(&dispatch, &report, &receipt, from_ref(&check)).remove(0) {
                MemberVerifyOutcome::Failed { failures, .. } => failures,
                other => panic!("an attributed batch failure is a member failure, not {other:?}"),
            }
        };

        assert!(failed("verify.test").contains(VerifyFailure::Test));
        assert!(!failed("verify.novel").is_empty(), "a declared gate takes a position past the compiled vocabulary");
    }

    #[test]
    fn an_undecodable_dispatch_blob_does_not_stall_its_siblings() {
        // Every open run is driven in one pass, so refusing the pass over one
        // unreadable row stops every other run on every tick — permanently,
        // since nothing deletes the row.
        let mut store = SqliteStore::open(":memory:").expect("store");
        let unreadable = SharedRunRow {
            run: Digest::of_wire_bytes(b"unreadable-run").as_bytes().to_vec(),
            nonce: "unreadable".to_owned(),
            dispatch: b"not a shared-run dispatch".to_vec(),
            lifecycle: SharedRunLifecycle::Ready,
            next_ordinal: 0,
            deadline_unix_millis: 10_000,
            charged: false,
            physical_cost: None,
        };
        store.record_shared_run(&unreadable, &[]).expect("unreadable run");

        let dispatch = reducer_shaped_contextual_dispatch();
        let sibling = SharedRunRow {
            run: Digest::of_wire_bytes(b"sibling-run").as_bytes().to_vec(),
            nonce: "dispatch-1".to_owned(),
            dispatch: to_vec(&dispatch).expect("dispatch wire"),
            lifecycle: SharedRunLifecycle::Ready,
            next_ordinal: 0,
            deadline_unix_millis: 10_000,
            charged: false,
            physical_cost: None,
        };
        let participant = SharedRunMemberRow {
            run: sibling.run.clone(),
            request: dispatch.plan.requests[0].digest().as_bytes().to_vec(),
            ordinal: 0,
            queued_unix_millis: 0,
            deadline_unix_millis: 10_000,
            cancelled: false,
            outcome: None,
            latency_millis: None,
        };
        store.record_shared_run(&sibling, from_ref(&participant)).expect("sibling run");

        let executor = ReleasePort { releases: Cell::new(0) };
        drive_shared_runs(&mut store, None, &executor, NameEvidenceClaims, &HostClass::new("fleet"), 1_000)
            .expect("one unreadable row cannot fail the pass");

        let members = store.shared_run_members(&sibling.run).expect("sibling membership");
        assert!(members[0].outcome.is_some(), "the sibling run still drove to a member outcome");
    }

    #[test]
    fn contextual_contract_binds_the_full_physical_plan_identity() {
        let dispatch = reducer_shaped_contextual_dispatch();
        assert!(contextual_contract_valid(&dispatch));

        let mut composition_only = dispatch.clone();
        let composition = composition_only.plan.composition.as_ref().expect("composition");
        let SharedRunExecution::Contextual { node, .. } = &mut composition_only.execution else {
            panic!("contextual execution");
        };
        node.plan = composition.digest();
        assert_ne!(node.plan, composition_only.plan.digest());
        assert!(!contextual_contract_valid(&composition_only));

        let mut stale = dispatch;
        let SharedRunExecution::Contextual { node, .. } = &mut stale.execution else {
            panic!("contextual execution");
        };
        node.plan = Digest::of_wire_bytes(b"stale physical plan");
        assert!(!contextual_contract_valid(&stale));
    }

    #[test]
    fn contextual_fact_input_spans_only_exact_candidate_retries() {
        let first = reducer_shaped_contextual_dispatch();
        let mut retry = first.clone();
        retry.plan.execution_attempt = 1;
        let SharedRunExecution::Contextual { node: retry_node, .. } = &mut retry.execution else {
            panic!("contextual execution");
        };
        retry_node.plan = retry.plan.digest();

        let SharedRunExecution::Contextual { node: first_node, .. } = &first.execution else {
            panic!("contextual execution");
        };
        assert_ne!(first_node.digest(), retry_node.digest());
        assert!(contextual_contract_valid(&retry));
        assert!(same_contextual_fact_input(&first, &retry));

        let mut different_checkout = retry.clone();
        let SharedRunExecution::Contextual { node, transformation, .. } = &mut different_checkout.execution else {
            panic!("contextual execution");
        };
        node.candidate.checkout = Digest::of_wire_bytes(b"another checkout");
        transformation.checkout = node.candidate.checkout;
        assert!(contextual_contract_valid(&different_checkout));
        assert!(!same_contextual_fact_input(&first, &different_checkout));

        let mut different_contract = retry;
        different_contract.plan.composition.as_mut().expect("composition").contract.gate_set =
            Digest::of_wire_bytes(b"another gate set");
        let SharedRunExecution::Contextual { node, .. } = &mut different_contract.execution else {
            panic!("contextual execution");
        };
        node.plan = different_contract.plan.digest();
        assert!(!same_contextual_fact_input(&first, &different_contract));
    }
}
