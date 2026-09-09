//! Immutable preview folds, separate from the bloom's ordinary integration.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::slice::from_ref;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use aether_bloomery::{
    Admit, BloomId, Checkpoint, Digest, Event, Fact, IdempotencyKey, IntegrateOutcome, PrecheckNode, PrecheckPlan,
    PrecheckPreparation, QueuePrecheckPlanPayload, Topic,
};
use aether_data::wire::from_bytes;
use aether_substrate::actor::native::NativeCtx;

use crate::artifacts::{ArtifactsCapabilityState, PutResult};
use crate::bloomery::SourceShell;
use crate::bloomery::outbox::{OutboxResultDelivery, TopicOutbox};
use crate::bloomery::precheck::PrecheckProjection;
use crate::store::StoreBackend;

use super::persist_pending_results;

use crate::bloomery::precheck::namespace;

#[derive(Clone, Debug)]
pub enum Preview {
    Superseded,
    Prepared(PrecheckNode),
    Refused(String),
}

/// One preparation at a time; only the newest durable plan is offered. An
/// obsolete worker can finish without blocking ordinary integration handlers.
#[derive(Default)]
pub struct PreviewWork {
    in_flight: Option<Digest>,
    cancelled: Arc<AtomicBool>,
    answer: Option<(Digest, Result<Preview, String>)>,
}

impl PreviewWork {
    fn prepare(
        &mut self,
        ctx: &mut NativeCtx<'_>,
        source: &SourceShell,
        plan: &PrecheckPlan,
    ) -> Option<Result<Preview, String>> {
        if self.answer.as_ref().is_some_and(|(key, _)| *key == plan.digest()) {
            return self.answer.take().map(|(_, result)| result);
        }
        if self.in_flight.is_some() {
            return None;
        }
        self.answer = None;
        self.in_flight = Some(plan.digest());
        self.cancelled = Arc::new(AtomicBool::new(false));
        let cancelled = Arc::clone(&self.cancelled);
        let source = source.clone();
        let plan = plan.clone();
        ctx.dispatch_blocking_with(plan.digest(), move || {
            // Keep the durable request retryable even if an adapter panics.
            catch_unwind(AssertUnwindSafe(|| fold_preview(&source, &plan, &cancelled)))
                .map_err(|_| "pre-check source worker panicked".to_owned())
                .and_then(|result| result.map_err(|error| error.to_string()))
        });
        None
    }

    pub fn complete(&mut self, plan: Digest, answer: Result<Preview, String>) {
        if self.in_flight == Some(plan) {
            self.in_flight = None;
            self.answer = Some((plan, answer));
        }
    }
}

fn fold_preview(
    source: &SourceShell,
    plan: &PrecheckPlan,
    cancelled: &AtomicBool,
) -> Result<Preview, aether_bloomery_github::SourceError> {
    if cancelled.load(Ordering::Relaxed) {
        return Ok(Preview::Superseded);
    }
    let bloom = namespace(plan);
    let position = source.integration_checkpoint(&bloom, &plan.base)?;
    let mut expected = position.checkpoint;
    let mut head = position.head;
    for member in &plan.members {
        if cancelled.load(Ordering::Relaxed) {
            return Ok(Preview::Superseded);
        }
        let outcome = match source.integrate_pinned(&bloom, &member.candidate, &expected) {
            Ok(outcome) => outcome,
            Err(aether_bloomery_github::SourceError::Malformed(detail)) => return Ok(Preview::Refused(detail)),
            Err(error) => return Err(error),
        };
        match outcome {
            IntegrateOutcome::Integrated { tree, head: next } => {
                expected = Checkpoint { bloom, tree };
                head = Some(next);
            }
            IntegrateOutcome::Conflict { paths, diff, .. } => {
                return Ok(Preview::Refused(format!(
                    "Aggregate pre-check {} could not merge {}.\nPaths: {}\n\n{}",
                    plan.digest().to_hex(),
                    member.workpiece.0,
                    paths.join(", "),
                    diff,
                )));
            }
            other @ IntegrateOutcome::StaleCheckpoint { .. } => {
                return Ok(Preview::Refused(format!("immutable pre-check namespace changed: {other:?}")));
            }
        }
    }
    Ok(head.map_or_else(
        || Preview::Refused("aggregate pre-check produced no checkout head".to_owned()),
        |head| {
            Preview::Prepared(PrecheckNode { plan: plan.digest(), tree: expected.tree, head, gate_set: plan.gate_set })
        },
    ))
}

fn preparation_key(sequence: u64) -> IdempotencyKey {
    IdempotencyKey(format!("aether.bloomery.precheck-prepared:{sequence}"))
}

/// Coalesce undispatched plans before doing Git work. Persist each returned
/// fact on the existing outbox row and wait for journal admission before ack.
pub fn drain_precheck_plans(
    store: &mut dyn StoreBackend,
    source: &SourceShell,
    mut artifacts: Option<&mut ArtifactsCapabilityState>,
    projection: &mut PrecheckProjection,
    work: &mut PreviewWork,
    ctx: &mut NativeCtx<'_>,
) -> rusqlite::Result<(Vec<Admit>, Option<u64>)> {
    let entries = store.drain_topic(Topic::QueuePrecheckPlan)?;
    if entries.is_empty() || !projection.refresh(store)? {
        return Ok((Vec::new(), None));
    }
    let mut admits = Vec::new();
    let mut ack_through = None;
    for entry in entries {
        match store.replay_topic_results(Topic::QueuePrecheckPlan, entry.sequence)? {
            OutboxResultDelivery::Journaled => {
                ack_through = Some(entry.sequence);
                continue;
            }
            OutboxResultDelivery::Pending(pending) => {
                admits.extend(pending);
                break;
            }
            OutboxResultDelivery::Unrecorded => {}
        }
        let Ok(payload) = from_bytes::<QueuePrecheckPlanPayload>(&entry.payload) else {
            tracing::warn!(sequence = entry.sequence, "pre-check plan did not decode; leaving it pending");
            break;
        };
        if !projection.get(&BloomId(payload.bloom)).is_some_and(|state| state.is_current_plan(payload.plan.digest())) {
            if work.in_flight == Some(payload.plan.digest()) {
                work.cancelled.store(true, Ordering::Relaxed);
                // The terminal janitor may prune this namespace only after
                // its producer has finished and this request is acknowledged.
                break;
            }
            ack_through = Some(entry.sequence);
            continue;
        }
        let Some(answer) = work.prepare(ctx, source, &payload.plan) else {
            break;
        };
        let preview = match answer {
            Ok(preview) => preview,
            Err(error) => {
                tracing::warn!(sequence = entry.sequence, %error, "pre-check preparation failed; leaving it pending");
                break;
            }
        };
        let preparation = match preview {
            Preview::Superseded => break,
            Preview::Prepared(node) => PrecheckPreparation::Prepared(node),
            Preview::Refused(diagnostic) => {
                let Some(artifacts) = artifacts.as_deref_mut() else {
                    tracing::warn!(sequence = entry.sequence, "pre-check diagnostic needs an artifacts store");
                    break;
                };
                if let PutResult::Err { error } = artifacts.put(diagnostic.as_bytes(), &[]) {
                    tracing::warn!(sequence = entry.sequence, ?error, "pre-check diagnostic was not retained");
                    break;
                }
                PrecheckPreparation::Refused { detail: Digest::of_wire_bytes(diagnostic.as_bytes()) }
            }
        };
        let event = Event {
            idempotency_key: preparation_key(entry.sequence),
            fact: Fact::PrecheckPrepared { bloom: BloomId(payload.bloom), plan: payload.plan.digest(), preparation },
        };
        let Some(pending) = persist_pending_results(store, Topic::QueuePrecheckPlan, entry.sequence, from_ref(&event))
        else {
            break;
        };
        admits.extend(pending);
        break;
    }
    Ok((admits, ack_through))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use aether_bloomery::testing::digest;
    use aether_bloomery::{CandidateRef, PrecheckMember, WorkpieceId};
    use aether_bloomery_github::{GitDataApi, GitSource, MainlineRef, fixture::FakeGithub};

    fn fixture() -> (FakeGithub, SourceShell, PrecheckPlan) {
        let fake = FakeGithub::new();
        let base = fake.seed_base_commit(&digest(1));
        let members = (2..5)
            .map(|seed| PrecheckMember {
                workpiece: WorkpieceId(format!("member-{seed}")),
                scope_revision: digest(seed + 10),
                candidate: CandidateRef { tree: digest(seed), checkout: fake.seed_base_commit(&digest(seed)) },
            })
            .collect();
        let source = SourceShell::new(Arc::new(GitSource::new(
            fake.clone(),
            Arc::new(fake.clone()),
            false,
            MainlineRef::default(),
        )));
        (fake, source, PrecheckPlan { bloom: BloomId(digest(9)), base, members, gate_set: digest(20) })
    }

    #[test]
    fn a_returning_plan_has_a_new_delivery_key_but_replay_keeps_its_request_key() {
        use crate::store::{JournalWrite, SqliteStore};
        use aether_bloomery::{Decisions, Outcome};
        use aether_data::wire::to_vec;
        let (_, _, plan) = fixture();
        let event = |sequence| Event {
            idempotency_key: preparation_key(sequence),
            fact: Fact::PrecheckPrepared {
                bloom: plan.bloom,
                plan: plan.digest(),
                preparation: PrecheckPreparation::Refused { detail: digest(30) },
            },
        };
        let first = event(1);
        let mut store = SqliteStore::open(":memory:").unwrap();
        store
            .append_event(&JournalWrite {
                idempotency_key: &first.idempotency_key.0,
                event: &to_vec(&first).unwrap(),
                decisions: &to_vec(&Decisions { outcome: Outcome::Duplicate, effects: Vec::new() }).unwrap(),
                decider: "precheck-repeated-plan",
            })
            .unwrap();
        assert!(store.journal_holds_any(&[event(1).idempotency_key.0]).unwrap());
        assert_eq!(first.fact, event(3).fact, "returning AB still prepares the immutable AB plan");
        assert!(
            !store.journal_holds_any(&[event(3).idempotency_key.0]).unwrap(),
            "AB after ABC must be admissible again"
        );
    }

    #[test]
    fn superseded_preparation_stops_before_writing_a_namespace() {
        let (fake, source, plan) = fixture();
        assert!(matches!(fold_preview(&source, &plan, &AtomicBool::new(true)).unwrap(), Preview::Superseded));
        assert!(fake.list_matching_refs("heads/bloom/").unwrap().is_empty());
    }

    #[test]
    fn preview_replay_returns_the_same_node_and_spares_the_ordinary_namespace() {
        let (fake, source, plan) = fixture();
        let Preview::Prepared(first) = fold_preview(&source, &plan, &AtomicBool::new(false)).unwrap() else {
            panic!("expected prepared node")
        };
        let Preview::Prepared(replayed) = fold_preview(&source, &plan, &AtomicBool::new(false)).unwrap() else {
            panic!("expected replayed node")
        };
        assert_eq!(first, replayed);
        assert_eq!(first.plan, plan.digest());
        assert_eq!(first.gate_set, plan.gate_set);
        assert!(
            fake.list_matching_refs(&format!("heads/bloom/{}/", aether_bloomery_github::short_hex(&plan.bloom.0)))
                .unwrap()
                .is_empty()
        );
    }
}
