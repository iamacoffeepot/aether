//! Crash-safe outbox drains for eager integration and compatibility work.

use std::slice::from_ref;

use aether_bloomery::{
    Admit, CandidatePreparation, CandidatePreparationPayload, CandidateRef, Checkpoint, CompatibilityPreviewPayload,
    Digest, Event, Fact, IdempotencyKey, IntegrateOutcome, IntegrationAppendPayload, SharedRunPlanPayload,
    SharedRunPreparation, Topic, Transformation, VERIFY_BASE_COMMAND, VERIFY_CHECK_COMMAND,
};
use aether_bloomery_github::SourceError;
use aether_data::wire::from_bytes;

use super::compatibility::{PreviewResult, SharedPreparationResult, prepare_shared_run, preview};
use super::eager::{AppendResult, CandidatePreparationResult, append_plan, prepare_candidate};
use super::projection::CoordinationProjection;
use super::{persist_pending_results, store_fold_conflict_overlay};
use crate::artifacts::ArtifactsCapabilityState;
use crate::bloomery::SourceShell;
use crate::bloomery::coordination::{compatibility_namespace, shared_probe_namespace};
use crate::bloomery::outbox::{OutboxResultDelivery, TopicOutbox};
use crate::bloomery::reactor::shared_run::{
    PreparedSharedProbe, SharedProbePreparation, SharedProbePreparationRequest, SharedStepDescriptor,
};
use crate::store::{CommissionBackend, StoreBackend, now_unix_millis};

pub(super) fn drain_integration_appends(
    store: &mut dyn StoreBackend,
    source: &SourceShell,
    mut artifacts: Option<&mut ArtifactsCapabilityState>,
    projection: &CoordinationProjection,
) -> rusqlite::Result<(Vec<Admit>, Option<u64>)> {
    drain_topic(
        store,
        Topic::IntegrationAppend,
        |_, _| true,
        |sequence, payload, store| {
            let payload = from_bytes::<IntegrationAppendPayload>(payload).ok()?;
            let plan_digest = payload.plan.digest();
            if !projection.is_current_append(&payload.plan) {
                let diagnostic = format!("Eager append {} is no longer current.\n", plan_digest.to_hex());
                return append_refused(&payload, plan_digest, &diagnostic, artifacts.as_deref_mut());
            }
            let event = match append_plan(source, &payload.plan) {
                AppendResult::Advanced(head) => Event {
                    idempotency_key: result_key("integration-advanced", plan_digest),
                    fact: Fact::IntegrationAdvanced { bloom: payload.plan.bloom, plan: plan_digest, head },
                },
                AppendResult::Conflicted { input, at, evidence, diagnostic } => {
                    if !retain_diagnostic(artifacts.as_deref_mut(), &diagnostic, &at.tree) {
                        return None;
                    }
                    if let Some(owner) = input.members.iter().find(|pin| pin.candidate == input.candidate)
                        && let Err(error) =
                            store.record_fold_conflict(payload.plan.bloom.0.as_bytes(), &owner.workpiece.0, &diagnostic)
                    {
                        tracing::warn!(sequence, %error, "eager append conflict overlay did not persist");
                        return None;
                    }
                    Event {
                        idempotency_key: result_key("integration-conflicted", plan_digest),
                        fact: Fact::IntegrationAppendConflicted {
                            bloom: payload.plan.bloom,
                            plan: plan_digest,
                            generation: payload.plan.generation,
                            expected_parent: payload.plan.expected_parent.node,
                            input,
                            at,
                            evidence,
                            observed_at_unix_millis: now_unix_millis(),
                        },
                    }
                }
                AppendResult::Stale(actual) => {
                    let diagnostic = format!(
                        "Eager append {} expected head {} but found tree {}.\n",
                        plan_digest.to_hex(),
                        payload.plan.expected_parent.node.to_hex(),
                        actual.to_hex(),
                    );
                    append_refused(&payload, plan_digest, &diagnostic, artifacts.as_deref_mut())?
                }
                AppendResult::Refused(diagnostic) => {
                    append_refused(&payload, plan_digest, &diagnostic, artifacts.as_deref_mut())?
                }
                AppendResult::Stopped(error) => {
                    tracing::warn!(sequence, %error, "eager append stopped; leaving it pending");
                    return None;
                }
            };
            Some(event)
        },
    )
}

fn append_refused(
    payload: &IntegrationAppendPayload,
    plan_digest: Digest,
    diagnostic: &str,
    artifacts: Option<&mut ArtifactsCapabilityState>,
) -> Option<Event> {
    let detail = Digest::of_wire_bytes(diagnostic.as_bytes());
    retain_diagnostic(artifacts, diagnostic, &payload.plan.expected_parent.candidate.tree).then(|| Event {
        idempotency_key: result_key("integration-refused", plan_digest),
        fact: Fact::IntegrationAppendRefused {
            bloom: payload.plan.bloom,
            plan: plan_digest,
            generation: payload.plan.generation,
            expected_parent: payload.plan.expected_parent.node,
            detail,
        },
    })
}

pub(super) fn drain_candidate_preparations(
    store: &mut dyn StoreBackend,
    source: &SourceShell,
    mut artifacts: Option<&mut ArtifactsCapabilityState>,
    projection: &CoordinationProjection,
) -> rusqlite::Result<(Vec<Admit>, Option<u64>)> {
    drain_topic(
        store,
        Topic::CandidatePreparation,
        |_, _| true,
        |sequence, payload, store| {
            let payload = from_bytes::<CandidatePreparationPayload>(payload).ok()?;
            let plan_digest = payload.plan.digest();
            if !projection.is_current_preparation(&payload.plan) {
                let diagnostic = format!("Candidate preparation {} is no longer current.\n", plan_digest.to_hex());
                let detail = Digest::of_wire_bytes(diagnostic.as_bytes());
                if !retain_diagnostic(
                    artifacts.as_deref_mut(),
                    &diagnostic,
                    &payload.plan.context.starting_head.candidate.tree,
                ) {
                    return None;
                }
                return Some(Event {
                    idempotency_key: result_key("candidate-prepared", plan_digest),
                    fact: Fact::CandidatePrepared {
                        bloom: payload.plan.bloom,
                        plan: plan_digest,
                        preparation: CandidatePreparation::Refused { detail },
                    },
                });
            }
            let preparation = match prepare_candidate(source, &payload.plan) {
                CandidatePreparationResult::Completed(preparation) => preparation,
                CandidatePreparationResult::Conflicted { preparation, diagnostic, parent } => {
                    if !retain_diagnostic(artifacts.as_deref_mut(), &diagnostic, &parent)
                        || store
                            .record_fold_conflict(
                                payload.plan.bloom.0.as_bytes(),
                                &payload.plan.workpiece.0,
                                &diagnostic,
                            )
                            .is_err()
                    {
                        tracing::warn!(sequence, "candidate preparation diagnostic did not persist");
                        return None;
                    }
                    preparation
                }
                CandidatePreparationResult::Refused(diagnostic) => {
                    let detail = Digest::of_wire_bytes(diagnostic.as_bytes());
                    if !retain_diagnostic(
                        artifacts.as_deref_mut(),
                        &diagnostic,
                        &payload.plan.context.starting_head.candidate.tree,
                    ) {
                        return None;
                    }
                    CandidatePreparation::Refused { detail }
                }
                CandidatePreparationResult::Stopped(error) => {
                    tracing::warn!(sequence, %error, "candidate preparation stopped; leaving it pending");
                    return None;
                }
            };
            Some(Event {
                idempotency_key: result_key("candidate-prepared", plan_digest),
                fact: Fact::CandidatePrepared { bloom: payload.plan.bloom, plan: plan_digest, preparation },
            })
        },
    )
}

pub(super) fn drain_compatibility_previews(
    store: &mut dyn StoreBackend,
    source: &SourceShell,
    mut artifacts: Option<&mut ArtifactsCapabilityState>,
) -> rusqlite::Result<(Vec<Admit>, Option<u64>)> {
    let mut pruned = false;
    drain_topic(
        store,
        Topic::CompatibilityPreview,
        |sequence, payload| {
            if pruned {
                return false;
            }
            let Ok(payload) = from_bytes::<CompatibilityPreviewPayload>(payload) else {
                tracing::warn!(sequence, "journaled compatibility preview did not decode for cleanup");
                return false;
            };
            match source.prune_working_refs(&compatibility_namespace(&payload.plan)) {
                Ok(_) => {
                    pruned = true;
                    true
                }
                Err(error) => {
                    tracing::warn!(sequence, %error, "compatibility preview namespace cleanup failed");
                    false
                }
            }
        },
        |sequence, payload, _store| {
            let payload = from_bytes::<CompatibilityPreviewPayload>(payload).ok()?;
            let plan_digest = payload.plan.digest();
            let result = match preview(source, &payload.plan) {
                PreviewResult::Completed(result) => result,
                PreviewResult::Diagnosed { result, diagnostic, parent } => {
                    if !retain_diagnostic(artifacts.as_deref_mut(), &diagnostic, &parent) {
                        return None;
                    }
                    result
                }
                PreviewResult::Stopped(error) => {
                    tracing::warn!(sequence, %error, "compatibility preview stopped; leaving it pending");
                    return None;
                }
            };
            Some(Event {
                idempotency_key: result_key("compatibility-previewed", plan_digest),
                fact: Fact::CompatibilityPreviewed { bloom: payload.plan.bloom, plan: plan_digest, result },
            })
        },
    )
}

pub(super) fn drain_shared_run_preparations<S>(
    store: &mut S,
    source: &SourceShell,
    mut artifacts: Option<&mut ArtifactsCapabilityState>,
    projection: &CoordinationProjection,
) -> rusqlite::Result<(Vec<Admit>, Option<u64>)>
where
    S: StoreBackend + CommissionBackend,
{
    drain_topic(
        store,
        Topic::SharedRunPreparation,
        |_, _| true,
        |sequence, payload, store| {
            let payload = from_bytes::<SharedRunPlanPayload>(payload).ok()?;
            let plan_digest = payload.plan.digest();
            let bloom = payload
                .plan
                .composition
                .as_ref()
                .map(|composition| composition.bloom)
                .or_else(|| payload.plan.requests.first().map(|request| request.bloom))?;
            if !projection.is_current_shared_run(&payload.plan) {
                let diagnostic = format!("Shared run {} is no longer current.\n", plan_digest.to_hex());
                let parent = payload
                    .plan
                    .composition
                    .as_ref()
                    .map_or_else(Digest::default, |composition| composition.base.candidate.tree);
                if !retain_diagnostic(artifacts.as_deref_mut(), &diagnostic, &parent) {
                    return None;
                }
                return Some(Event {
                    idempotency_key: result_key("shared-run-prepared", plan_digest),
                    fact: Fact::SharedRunPrepared {
                        bloom,
                        plan: plan_digest,
                        preparation: SharedRunPreparation::Refused {
                            detail: Digest::of_wire_bytes(diagnostic.as_bytes()),
                        },
                    },
                });
            }
            let preparation = match prepare_shared_run(source, store, &payload.plan) {
                SharedPreparationResult::Completed(preparation) => preparation,
                SharedPreparationResult::Diagnosed { preparation, diagnostic, parent } => {
                    if !retain_diagnostic(artifacts.as_deref_mut(), &diagnostic, &parent) {
                        return None;
                    }
                    preparation
                }
                SharedPreparationResult::Stopped(error) => {
                    tracing::warn!(sequence, %error, "shared-run preparation stopped; leaving it pending");
                    return None;
                }
            };
            Some(Event {
                idempotency_key: result_key("shared-run-prepared", plan_digest),
                fact: Fact::SharedRunPrepared { bloom, plan: plan_digest, preparation },
            })
        },
    )
}

/// Prepare at most one durable adaptive-attribution step per tick. The source
/// effect precedes the store CAS; a crash in that window re-enters the same
/// plan-private namespace and recovers its exact candidate before retrying the
/// write. Executor submission is gated on the prepared column.
pub(super) fn prepare_shared_probe_step(
    store: &mut dyn StoreBackend,
    source: &SourceShell,
    artifacts: Option<&mut ArtifactsCapabilityState>,
) -> rusqlite::Result<bool> {
    let Some(step) = store.list_unprepared_shared_run_steps(1)?.into_iter().next() else {
        return Ok(false);
    };
    let Ok(SharedStepDescriptor::Probe(request)) = serde_json::from_slice(&step.descriptor) else {
        tracing::warn!(nonce = %step.nonce, "unprepared shared step is not a probe descriptor");
        return Ok(false);
    };
    if (request.run.as_bytes().as_slice(), request.ordinal) != (step.run.as_slice(), step.ordinal) {
        tracing::warn!(nonce = %step.nonce, "shared probe descriptor does not match its durable step identity");
        return Ok(false);
    }
    let request = *request;
    let request_digest = Digest::of_wire_bytes(&step.descriptor);
    let prepared = match prepare_probe_candidate(source, &request) {
        ProbeCandidateResult::Prepared(candidate) => match probe_transformation(&request, candidate) {
            Ok((candidate, transformation)) => SharedProbePreparation::Prepared(Box::new(PreparedSharedProbe {
                request: request_digest,
                candidate,
                transformation,
                profile: request.profile,
                configs: request.configs,
            })),
            Err(diagnostic) => {
                if !retain_diagnostic(artifacts, &diagnostic, &request.base.tree) {
                    return Ok(false);
                }
                SharedProbePreparation::Refused {
                    request: request_digest,
                    detail: Digest::of_wire_bytes(diagnostic.as_bytes()),
                }
            }
        },
        ProbeCandidateResult::Refused { diagnostic, parent } => {
            if !retain_diagnostic(artifacts, &diagnostic, &parent) {
                return Ok(false);
            }
            SharedProbePreparation::Refused {
                request: request_digest,
                detail: Digest::of_wire_bytes(diagnostic.as_bytes()),
            }
        }
        ProbeCandidateResult::Stopped(error) => {
            tracing::warn!(nonce = %step.nonce, %error, "shared probe preparation stopped; leaving it pending");
            return Ok(false);
        }
    };
    let Ok(bytes) = serde_json::to_vec(&prepared) else {
        tracing::warn!(nonce = %step.nonce, "prepared shared probe did not encode");
        return Ok(false);
    };
    store.prepare_shared_run_step(&step.nonce, &bytes)
}

fn probe_transformation(
    request: &SharedProbePreparationRequest,
    candidate: CandidateRef,
) -> Result<(CandidateRef, Transformation), String> {
    let mut transformation = request.transformation.clone();
    let Some(subject) = transformation.inputs.first_mut() else {
        return Err("Shared probe transformation has no bound subject input.\n".to_owned());
    };
    *subject = candidate.tree;
    transformation.checkout = candidate.checkout;
    if request.inputs.is_empty() {
        if transformation.command != VERIFY_CHECK_COMMAND || request.probe.check.gate().is_empty() {
            return Err("Shared baseline probe cannot derive the equivalent whole-workspace gate.\n".to_owned());
        }
        // `verify.base` and `verify.check` use the same manifest-declared gate
        // fan-out. The base command is the existing whole-workspace mode and
        // deliberately carries no diff base; base..base would select an empty
        // closure and could manufacture a green receipt without running tests.
        VERIFY_BASE_COMMAND.clone_into(&mut transformation.command);
        transformation.diff_base = None;
    } else {
        transformation.diff_base = Some(request.base.checkout);
    }
    Ok((candidate, transformation))
}

enum ProbeCandidateResult {
    Prepared(CandidateRef),
    Refused { diagnostic: String, parent: Digest },
    Stopped(String),
}

fn prepare_probe_candidate(source: &SourceShell, request: &SharedProbePreparationRequest) -> ProbeCandidateResult {
    let namespace = shared_probe_namespace(request.run, request.ordinal, request.plan);
    if request.inputs.is_empty() {
        return match source.prepare_pinned(&namespace, &request.base, &request.base) {
            Ok(IntegrateOutcome::Integrated { tree, head })
                if tree == request.base.tree && head == request.base.checkout =>
            {
                ProbeCandidateResult::Prepared(request.base)
            }
            Ok(_) | Err(SourceError::Malformed(_)) => ProbeCandidateResult::Refused {
                diagnostic: "Shared baseline probe does not resolve its exact pinned base.\n".to_owned(),
                parent: request.base.tree,
            },
            Err(error) => ProbeCandidateResult::Stopped(format!("shared baseline probe validation failed: {error}")),
        };
    }
    let position = match source.integration_checkpoint(&namespace, &request.base.checkout) {
        Ok(position) => position,
        Err(error) => {
            return ProbeCandidateResult::Stopped(format!("shared probe bootstrap failed: {error}"));
        }
    };
    if position.checkpoint.tree != request.base.tree {
        let Some(head) = position.head else {
            return ProbeCandidateResult::Refused {
                diagnostic: "Shared probe base checkout does not carry its named tree.\n".to_owned(),
                parent: position.checkpoint.tree,
            };
        };
        match source.is_fast_forward(&request.base.checkout, &head) {
            Ok(true) => {}
            Ok(false) => {
                return ProbeCandidateResult::Refused {
                    diagnostic: "Shared probe namespace advanced outside its immutable request.\n".to_owned(),
                    parent: position.checkpoint.tree,
                };
            }
            Err(error) => return ProbeCandidateResult::Stopped(format!("shared probe ancestry check failed: {error}")),
        }
    }
    let mut expected = Checkpoint { bloom: namespace, tree: position.checkpoint.tree };
    let mut head = position.head;
    for input in &request.inputs {
        match source.integrate_pinned(&namespace, &input.candidate, &expected) {
            Ok(IntegrateOutcome::Integrated { tree, head: next }) => {
                expected.tree = tree;
                head = Some(next);
            }
            Ok(IntegrateOutcome::Conflict { at, paths, diff, .. }) => {
                let diagnostic = format!(
                    "Shared probe {} found a source conflict at {}.\nPaths: {}\n\n{}",
                    request_digest(request).to_hex(),
                    at.to_hex(),
                    paths.join(", "),
                    diff,
                );
                return ProbeCandidateResult::Refused { diagnostic, parent: at };
            }
            Ok(IntegrateOutcome::StaleCheckpoint { actual }) => {
                return ProbeCandidateResult::Refused {
                    diagnostic: format!("Shared probe namespace held unexpected tree {}.\n", actual.to_hex()),
                    parent: actual,
                };
            }
            Err(SourceError::Malformed(reason)) => {
                return ProbeCandidateResult::Refused {
                    diagnostic: format!("Shared probe source input was refused: {reason}\n"),
                    parent: expected.tree,
                };
            }
            Err(error) => {
                return ProbeCandidateResult::Stopped(format!("shared probe source preparation failed: {error}"));
            }
        }
    }
    head.map_or_else(
        || ProbeCandidateResult::Refused {
            diagnostic: "Shared probe produced no checkout head.\n".to_owned(),
            parent: expected.tree,
        },
        |checkout| ProbeCandidateResult::Prepared(CandidateRef { tree: expected.tree, checkout }),
    )
}

fn request_digest(request: &SharedProbePreparationRequest) -> Digest {
    serde_json::to_vec(request).map_or_else(|_| Digest::default(), |bytes| Digest::of_wire_bytes(&bytes))
}

fn drain_topic<S: StoreBackend + ?Sized>(
    store: &mut S,
    topic: Topic,
    mut cleanup: impl FnMut(u64, &[u8]) -> bool,
    mut effect: impl FnMut(u64, &[u8], &mut S) -> Option<Event>,
) -> rusqlite::Result<(Vec<Admit>, Option<u64>)> {
    let entries = store.drain_topic(topic)?;
    let mut admits = Vec::new();
    let mut ack_through = None;
    for entry in entries {
        match store.replay_topic_results(topic, entry.sequence)? {
            OutboxResultDelivery::Journaled => {
                if !cleanup(entry.sequence, &entry.payload) {
                    break;
                }
                ack_through = Some(entry.sequence);
                continue;
            }
            OutboxResultDelivery::Pending(pending) => {
                admits.extend(pending);
                break;
            }
            OutboxResultDelivery::Unrecorded => {}
        }
        let Some(event) = effect(entry.sequence, &entry.payload, store) else {
            break;
        };
        let Some(pending) = persist_pending_results(store, topic, entry.sequence, from_ref(&event)) else {
            break;
        };
        admits.extend(pending);
        break;
    }
    Ok((admits, ack_through))
}

fn retain_diagnostic(artifacts: Option<&mut ArtifactsCapabilityState>, diagnostic: &str, parent: &Digest) -> bool {
    store_fold_conflict_overlay(artifacts, diagnostic, parent)
}

fn result_key(kind: &str, plan: Digest) -> IdempotencyKey {
    IdempotencyKey(format!("aether.bloomery.{kind}:{}", plan.to_hex()))
}

#[cfg(test)]
mod tests {
    use aether_bloomery::testing::digest;
    use aether_bloomery::{
        AgentProfile, ConfigRegistry, ExecutionLimits, Harness, NetworkProfile, ReasoningEffort, ResolvedModel,
        ToolPolicy, Transformation,
    };

    use super::*;
    use crate::bloomery::{BatchCheck, BatchProbeRequest};

    fn probe_request() -> SharedProbePreparationRequest {
        SharedProbePreparationRequest {
            run: digest(1),
            ordinal: 2,
            plan: digest(3),
            probe: BatchProbeRequest {
                plan: digest(3),
                members: Vec::new(),
                baseline: None,
                check: BatchCheck::Gate { id: "verify.clippy".to_owned() },
                repetition: 0,
            },
            base: CandidateRef { tree: digest(4), checkout: digest(5) },
            inputs: Vec::new(),
            transformation: Transformation {
                command: VERIFY_CHECK_COMMAND.to_owned(),
                inputs: vec![digest(6), digest(7)],
                checkout: digest(8),
                diff_base: Some(digest(9)),
                outputs: vec!["result-record".to_owned()],
                image: "verify-image".to_owned(),
                limits: ExecutionLimits { wall_clock_secs: 900 },
                network: NetworkProfile::Restricted,
                description: Some("probe".to_owned()),
                model: Some(ResolvedModel {
                    harness: Harness::Claude,
                    model: "mechanical".to_owned(),
                    effort: ReasoningEffort::High,
                }),
            },
            profile: AgentProfile {
                harness: Harness::Codex,
                model: "host".to_owned(),
                effort: ReasoningEffort::Max,
                tools: ToolPolicy::Allow(vec!["read".to_owned()]),
            },
            configs: ConfigRegistry::default(),
        }
    }

    #[test]
    fn an_empty_baseline_uses_the_real_whole_workspace_invocation() {
        let request = probe_request();
        let candidate = request.base;
        let retained = request.transformation.clone();

        let (prepared_candidate, prepared) =
            probe_transformation(&request, candidate).expect("verify.check has an equivalent base mode");

        assert_eq!(prepared_candidate, candidate);
        assert_eq!(prepared.command, VERIFY_BASE_COMMAND);
        assert_eq!(prepared.inputs, vec![candidate.tree, digest(7)]);
        assert_eq!(prepared.checkout, candidate.checkout);
        assert_eq!(prepared.diff_base, None, "base..base would select the empty closure");
        assert_eq!(prepared.outputs, retained.outputs);
        assert_eq!(prepared.image, retained.image);
        assert_eq!(prepared.limits, retained.limits);
        assert_eq!(prepared.network, retained.network);
        assert_eq!(prepared.description, retained.description);
        assert_eq!(prepared.model, retained.model);
    }

    #[test]
    fn a_baseline_without_an_equivalent_gate_is_refused() {
        let mut request = probe_request();
        request.transformation.command = "verify.member".to_owned();

        assert!(probe_transformation(&request, request.base).is_err());

        request.transformation.command = VERIFY_CHECK_COMMAND.to_owned();
        request.probe.check = BatchCheck::Gate { id: String::new() };
        assert!(probe_transformation(&request, request.base).is_err());
    }
}
