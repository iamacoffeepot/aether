//! The registry owner's side: submitting a batch to it, and draining the
//! commands it hands back under one acquisition of the inner guard.

use std::sync::Arc;

use crate::actor::native::offload::blocking::DeferredCompletion;
use crate::mail::Mail;
use crate::mail::mailer::Mailer;
use crate::mail::registry::RegistryQueueMetrics;
use crate::mail::registry::effect::{
    ACTIVATION_BARRIER_KIND, ActivationReservation, EffectBatch, PreparedSpawnFailure, RegistryApplied,
    RegistryBatchCompletionSink, RegistryBatchError, RegistryBatchResult, RegistryCompletion, RegistryEffectError,
    barrier_token,
};
use crate::mail::registry::owner::{BatchEnvelope, OwnerCommand, ParkAdmission, RegistryOwnerHandle};
use crate::mail::view::Update;

use super::Registry;
use super::birth::{CapturedDisposition, RouteContinuation};
use super::publish::Publication;
use super::route::RouteLifecycle;

impl Registry {
    pub(in crate::mail::registry) fn install_owner(&self, owner: RegistryOwnerHandle) {
        assert!(self.owner.set(owner).is_ok(), "a registry can attach only one owner");
    }

    pub(crate) fn submit(&self, batch: EffectBatch) -> Option<RegistryCompletion<Vec<RegistryApplied>>> {
        let Some(owner) = self.owner.get() else {
            drop(batch.discard_prepared());
            return None;
        };
        owner.submit(batch)
    }

    pub(crate) fn submit_deferred(
        &self,
        batch: EffectBatch,
        completion: DeferredCompletion<RegistryBatchResult>,
    ) -> bool {
        let Some(owner) = self.owner.get() else {
            drop(batch.discard_prepared());
            completion.complete(Err(RegistryBatchError::OwnerClosed));
            return false;
        };
        owner.submit_deferred(batch, completion)
    }

    pub(crate) fn park_or_drop(&self, mail: Mail, observed_generation: u64) -> ParkAdmission {
        match self.owner.get() {
            Some(owner) => owner.park_or_drop(mail, observed_generation),
            None => ParkAdmission::Closed(mail),
        }
    }

    /// The registry owner queue's admission and drain accounting (issue
    /// 4122), or `None` before an owner is attached. ADR-0165 §Consequences
    /// makes owner throughput the input to its own sharding decision; this is
    /// where that measurement is read.
    #[must_use]
    pub fn owner_queue_metrics(&self) -> Option<RegistryQueueMetrics> {
        self.owner.get().map(RegistryOwnerHandle::metrics)
    }

    pub(in crate::mail::registry) fn apply_owner_commands(&self, commands: Vec<OwnerCommand>, mailer: &Mailer) -> u64 {
        enum AfterLock {
            Batch(RegistryBatchCompletionSink, Result<Vec<RegistryApplied>, RegistryEffectError>),
            Route(RouteContinuation),
            Schedule(Arc<dyn ActivationReservation>),
            CatchUp(Box<dyn FnOnce() + Send>),
        }

        let mut inner = self.inner.lock().expect("registry lock poisoned; fail-fast per ADR-0063");
        let mut publication = Publication::default();
        let mut after_lock = Vec::new();
        let mut readiness = Vec::new();
        for command in commands {
            match command {
                OwnerCommand::Batch(BatchEnvelope { batch, completion }) => {
                    let result = match Self::apply_batch_locked(&mut inner, batch) {
                        Ok((applied, batch_publication, continuations, schedules)) => {
                            publication.append(batch_publication);
                            after_lock.extend(continuations.into_iter().map(AfterLock::Route));
                            after_lock.extend(schedules.into_iter().map(AfterLock::Schedule));
                            Ok(applied)
                        }
                        Err(error) => Err(error),
                    };
                    after_lock.push(AfterLock::Batch(completion, result));
                }
                OwnerCommand::ParkOrDrop { mail, observed_generation: _ } => {
                    if mail.kind == ACTIVATION_BARRIER_KIND {
                        let token = barrier_token(&mail);
                        mailer.record_finished(mail.mail_id, mail.root);
                        if let Some(token) = token {
                            readiness.push((mail.recipient, token, Some(mail.mail_id)));
                        }
                    } else if let Some(continuation) = Self::capture_mail_locked(&mut inner, mail) {
                        after_lock.push(AfterLock::Route(continuation));
                    }
                }
                OwnerCommand::ActivationCancelled { id, token } => {
                    after_lock.extend(
                        Self::cancel_completed_locked(&mut inner, id, token, &mut publication)
                            .into_iter()
                            .map(AfterLock::Route),
                    );
                }
            }
        }
        for (id, token, barrier_mail_id) in readiness {
            if let Some(catch_up) = Self::promote_locked(&mut inner, id, token, barrier_mail_id, &mut publication) {
                after_lock.push(AfterLock::CatchUp(catch_up));
            }
        }
        let inventory_changed = inner.publish(publication);
        drop(inner);
        if inventory_changed {
            self.relay_inventory_changed();
        }
        for continuation in after_lock {
            match continuation {
                AfterLock::Batch(completion, result) => {
                    completion.complete(result);
                }
                AfterLock::Route(continuation) => mailer.relay_captured(continuation),
                AfterLock::Schedule(activation) => activation.schedule(),
                AfterLock::CatchUp(catch_up) => catch_up(),
            }
        }
        self.routes.load().generation()
    }

    pub(in crate::mail::registry) fn close_owner_commands(&self, commands: Vec<OwnerCommand>, mailer: &Mailer) -> u64 {
        let mut inner = self.inner.lock().expect("registry lock poisoned; fail-fast per ADR-0063");
        let mut completions = Vec::new();
        let mut continuations = Vec::new();
        let mut discarded = Vec::new();
        for command in commands {
            match command {
                OwnerCommand::Batch(envelope) => {
                    discarded.extend(envelope.batch.discard_prepared());
                    completions.push(envelope.completion);
                }
                OwnerCommand::ParkOrDrop { mail, observed_generation: _ } => {
                    if mail.kind == ACTIVATION_BARRIER_KIND {
                        mailer.record_finished(mail.mail_id, mail.root);
                    } else if let Some(continuation) = Self::capture_mail_locked(&mut inner, mail) {
                        continuations.push(continuation);
                    }
                }
                OwnerCommand::ActivationCancelled { .. } => {}
            }
        }
        let mut pending_births = inner.pending_births.drain().collect::<Vec<_>>();
        for (_, birth) in &mut pending_births {
            continuations.extend(
                birth
                    .parked
                    .drain(..)
                    .map(|mail| RouteContinuation { mail, disposition: CapturedDisposition::Unknown }),
            );
        }
        drop(inner);

        for (_, birth) in &mut pending_births {
            if let Some(activation) = &birth.activation {
                activation.reject(PreparedSpawnFailure::OwnerClosed);
                activation.join();
            }
            if let Some(costs) = &birth.costs {
                costs.rollback(birth.id, birth.token);
            }
            birth.disarm();
        }
        for done in discarded {
            let _ = done.recv();
        }

        let mut inner = self.inner.lock().expect("registry lock poisoned; fail-fast per ADR-0063");
        let mut publication = Publication::default();
        for (id, birth) in &pending_births {
            if matches!(
                inner.mailboxes.get(id).map(|route| &route.lifecycle),
                Some(RouteLifecycle::Starting { token }) if *token == birth.token
            ) {
                inner.mailboxes.remove(id);
                publication.route_updates.push(Update::Remove(*id));
            }
        }
        let inventory_changed = inner.publish(publication);
        drop(inner);
        if inventory_changed {
            self.relay_inventory_changed();
        }
        for completion in completions {
            completion.complete(Err(RegistryEffectError::OwnerClosed));
        }
        for continuation in continuations {
            mailer.relay_captured(continuation);
        }
        self.routes.load().generation()
    }
}
