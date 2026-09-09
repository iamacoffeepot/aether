//! Idle admission and durable settlement of aggregate pre-checks.

use std::fmt::Display;
use std::slice::from_ref;

use aether_bloomery::{
    Admit, BloomId, Event, Evidence, EvidenceKind, Fact, IdempotencyKey, PrecheckCompletion, PrecheckPayload, StageId,
    Topic, WorkHandle, WorkpieceId,
};
use aether_data::wire::from_bytes;

use crate::artifacts::{ArtifactsCapabilityState, PutResult};
use crate::bloomery::executor::{ExecutorPort, Settled};
use crate::bloomery::intake::{
    AdmissionKey, DispatchRecord, dispatch_and_record, dispatch_nonce, dispatch_precheck_idle,
};
use crate::bloomery::outbox::{OutboxResultDelivery, TopicOutbox};
use crate::bloomery::precheck::PrecheckProjection;
use crate::store::{OrderLifecycle, OutboxEntry, StoreBackend};

fn record(entry: &OutboxEntry, payload: &PrecheckPayload) -> DispatchRecord {
    DispatchRecord {
        nonce: dispatch_nonce(entry.sequence),
        bloom: BloomId(payload.bloom),
        workpiece: WorkpieceId::composition(),
        scope_revision: payload.node.digest(),
        candidate: payload.node.tree,
        displayed_digest: payload.node.tree,
        stage: StageId::AggregateVerify,
        transformation: payload.transformation.clone(),
        profile: payload.profile.clone(),
        configs: payload.configs.clone(),
        instruction_bundle: None,
        prompt_manifest: None,
    }
}

fn retain_event(
    store: &mut dyn StoreBackend,
    topic: Topic,
    sequence: u64,
    event: &Event,
    admits: &mut Vec<Admit>,
) -> rusqlite::Result<()> {
    store.record_topic_results(topic, sequence, from_ref(event))?;
    if let OutboxResultDelivery::Pending(pending) = store.replay_topic_results(topic, sequence)? {
        admits.extend(pending);
    }
    Ok(())
}

/// The offer remains durable until its request is journaled. Requests use the
/// offer's sequence, so a later retry of the same node is a distinct request.
fn request_idle(
    store: &mut dyn StoreBackend,
    executor: &dyn ExecutorPort,
    projection: &PrecheckProjection,
    admits: &mut Vec<Admit>,
) -> rusqlite::Result<()> {
    for entry in store.drain_topic(Topic::OfferPrecheck)? {
        match store.replay_topic_results(Topic::OfferPrecheck, entry.sequence)? {
            OutboxResultDelivery::Journaled => {
                store.ack_topic(Topic::OfferPrecheck, entry.sequence)?;
                continue;
            }
            OutboxResultDelivery::Pending(pending) => {
                admits.extend(pending);
                break;
            }
            OutboxResultDelivery::Unrecorded => {}
        }
        let payload = from_bytes::<PrecheckPayload>(&entry.payload).map_err(decode_error)?;
        let Some(state) = projection.get(&BloomId(payload.bloom)) else {
            store.ack_topic(Topic::OfferPrecheck, entry.sequence)?;
            continue;
        };
        if !state.is_current_node(payload.node.digest()) || state.issued_is(payload.node.digest()) {
            store.ack_topic(Topic::OfferPrecheck, entry.sequence)?;
            continue;
        }
        if !state.can_request(payload.node.digest()) {
            if state.paused || state.issued.is_some() {
                break;
            }
            store.ack_topic(Topic::OfferPrecheck, entry.sequence)?;
            continue;
        }
        if !executor.has_idle_capacity(&record(&entry, &payload).to_order()) {
            break;
        }
        retain_event(
            store,
            Topic::OfferPrecheck,
            entry.sequence,
            &Event {
                idempotency_key: IdempotencyKey(format!("aether.bloomery.precheck-request:{}", entry.sequence)),
                fact: Fact::RequestPrecheck { bloom: BloomId(payload.bloom), node: payload.node.digest() },
            },
            admits,
        )?;
        break;
    }
    Ok(())
}

fn complete_unstarted(
    store: &mut dyn StoreBackend,
    entry: &OutboxEntry,
    record: &DispatchRecord,
    completion: PrecheckCompletion,
    admits: &mut Vec<Admit>,
) -> rusqlite::Result<()> {
    retain_event(
        store,
        Topic::DispatchPrecheck,
        entry.sequence,
        &Event {
            idempotency_key: AdmissionKey::PrecheckCompleted.of(&record.nonce.0),
            fact: Fact::PrecheckCompleted { bloom: record.bloom, node: record.scope_revision, completion },
        },
        admits,
    )
}

fn setup_fault(
    artifacts: Option<&mut ArtifactsCapabilityState>,
    record: &DispatchRecord,
    error: &impl Display,
) -> rusqlite::Result<PrecheckCompletion> {
    let bytes =
        format!("Aggregate pre-check setup failed for {}: {error}", record.scope_revision.to_hex()).into_bytes();
    let Some(artifacts) = artifacts else {
        return Err(decode_error("pre-check setup diagnostic needs an artifacts store"));
    };
    if let PutResult::Err { error } = artifacts.put(&bytes, &[]) {
        return Err(decode_error(format!("pre-check setup diagnostic was not retained: {error:?}")));
    }
    Ok(PrecheckCompletion::HostFault(Evidence {
        subject: record.candidate,
        kind: EvidenceKind::ExecutorFault,
        detail: aether_bloomery::Digest::of_wire_bytes(&bytes),
    }))
}

/// Settle an old idle submission before cancelling or promoting it. A call
/// already on a worker may have started a process; a restart probe may not.
fn settle_old_submission(
    store: &mut dyn StoreBackend,
    executor: &dyn ExecutorPort,
    record: &DispatchRecord,
) -> rusqlite::Result<Settled<Option<WorkHandle>>> {
    match executor.settle_idle_submission(&record.to_order()) {
        Settled::InFlight => Ok(Settled::InFlight),
        Settled::Answered(Ok(handle)) => {
            if handle.is_some() {
                store.mark_order_submitted(&record.nonce.0)?;
            } else {
                store.consume_order(&record.nonce.0)?;
            }
            Ok(Settled::Answered(handle))
        }
        Settled::Answered(Err(error)) => Err(decode_error(error)),
    }
}

fn dispatch_one(
    store: &mut dyn StoreBackend,
    mut artifacts: Option<&mut ArtifactsCapabilityState>,
    executor: &dyn ExecutorPort,
    projection: &PrecheckProjection,
    entry: &OutboxEntry,
    admits: &mut Vec<Admit>,
    now_unix_millis: u64,
) -> rusqlite::Result<Settled<Option<WorkHandle>>> {
    let payload = from_bytes::<PrecheckPayload>(&entry.payload).map_err(decode_error)?;
    let record = record(entry, &payload);
    if store.journal_holds_any(from_ref(&AdmissionKey::PrecheckCompleted.of(&record.nonce.0).0))? {
        return Ok(Settled::Answered(None));
    }
    let state = projection.get(&record.bloom);
    let issued = state.is_some_and(|state| state.issued_is(record.scope_revision));
    let joined = issued && state.is_some_and(|state| state.joined_is(record.scope_revision) && !state.paused);
    let current = issued && state.is_some_and(|state| state.is_current_node(record.scope_revision) && !state.paused);
    if let Some(order) = store.lookup_order(&record.nonce.0)? {
        if order.lifecycle == OrderLifecycle::Submitted {
            return Ok(Settled::Answered(Some(WorkHandle::new(record.nonce))));
        }
        if joined || !current {
            match settle_old_submission(store, executor, &record)? {
                Settled::InFlight => return Ok(Settled::InFlight),
                Settled::Answered(Some(handle)) => return Ok(Settled::Answered(Some(handle))),
                Settled::Answered(None) => {}
            }
        }
    }
    if !current && !joined {
        complete_unstarted(store, entry, &record, PrecheckCompletion::SkippedBeforeStart, admits)?;
        return Ok(Settled::InFlight);
    }
    let submitted = if joined {
        dispatch_and_record(executor, store, artifacts.as_deref_mut(), &record, now_unix_millis).map(|answer| {
            match answer {
                Settled::InFlight => Settled::InFlight,
                Settled::Answered(handle) => Settled::Answered(Some(handle)),
            }
        })
    } else {
        dispatch_precheck_idle(executor, store, &record, now_unix_millis)
    };
    match submitted {
        Ok(Settled::Answered(None)) => Ok(Settled::InFlight),
        Ok(answer) => Ok(answer),
        Err(error) if error.is_permanent() => {
            let completion = setup_fault(artifacts, &record, &error)?;
            complete_unstarted(store, entry, &record, completion, admits)?;
            Ok(Settled::InFlight)
        }
        Err(error) => Err(decode_error(error)),
    }
}

/// Required dispatches have already been drained this turn. Running obsolete
/// orders still settle through the ordinary intake; only unstarted work retires.
pub fn drain_prechecks(
    store: &mut dyn StoreBackend,
    mut artifacts: Option<&mut ArtifactsCapabilityState>,
    executor: &dyn ExecutorPort,
    projection: &PrecheckProjection,
    now_unix_millis: u64,
) -> rusqlite::Result<(Vec<WorkHandle>, Vec<Admit>)> {
    let entries = store.drain_topic(Topic::DispatchPrecheck)?;
    let mut handles = Vec::new();
    let mut admits = Vec::new();
    let progress = (|| -> rusqlite::Result<()> {
        for entry in entries {
            match store.replay_topic_results(Topic::DispatchPrecheck, entry.sequence)? {
                OutboxResultDelivery::Journaled => {
                    store.ack_topic(Topic::DispatchPrecheck, entry.sequence)?;
                    continue;
                }
                OutboxResultDelivery::Pending(pending) => {
                    admits.extend(pending);
                    break;
                }
                OutboxResultDelivery::Unrecorded => {}
            }
            match dispatch_one(
                store,
                artifacts.as_deref_mut(),
                executor,
                projection,
                &entry,
                &mut admits,
                now_unix_millis,
            )? {
                Settled::Answered(handle) => {
                    handles.extend(handle);
                    store.ack_topic(Topic::DispatchPrecheck, entry.sequence)?;
                }
                Settled::InFlight => break,
            }
        }
        // These are wake signals. The journaled state above owns cancellation and
        // promotion, including when a signal is delivered again after a restart.
        for topic in [Topic::CancelPrecheck, Topic::PromotePrecheck] {
            if let Some(entry) = store.drain_topic(topic)?.last() {
                store.ack_topic(topic, entry.sequence)?;
            }
        }
        request_idle(store, executor, projection, &mut admits)?;
        Ok(())
    })();
    // An earlier entry may already have started and acknowledged its order.
    // Always hand that handle to the reactor, even if a later entry failed.
    if let Err(error) = progress {
        tracing::warn!(%error, "pre-check drain stopped; accepted work remains tracked and pending entries retry");
    }
    Ok((handles, admits))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;

fn decode_error(error: impl Display) -> rusqlite::Error {
    rusqlite::Error::InvalidParameterName(format!("aggregate pre-check: {error}"))
}
