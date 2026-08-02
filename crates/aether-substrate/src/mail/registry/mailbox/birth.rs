//! The birth of a route: the owner-private reservation a `Starting`
//! route stands on, the mail parked behind it, and its promotion or
//! cancellation.

use std::collections::VecDeque;
use std::mem;
use std::sync::Arc;

use crate::mail::registry::effect::{
    ActivationReservation, ActivationToken, EffectBatch, PreparedActivation, PreparedCostCells, PreparedMail,
    PreparedSpawnFailure, RegistryApplied, RegistryEffect, RegistryEffectError,
};
use crate::mail::registry::handlers::InboxHandler;
use crate::mail::view::Update;
use crate::mail::{Mail, MailId, MailboxId};

use super::publish::Publication;
use super::resolve::{ResolvedRoute, resolve_route};
use super::route::{RouteEndpoint, RouteLifecycle, RouteRecord};
use super::{Inner, MailboxEntry, Registry, SeizeCell};

pub(super) struct PendingBirth {
    pub(super) id: MailboxId,
    pub(super) token: ActivationToken,
    pub(super) parked: VecDeque<Mail>,
    pub(super) activation: Option<Arc<dyn ActivationReservation>>,
    pub(super) costs: Option<PreparedCostCells>,
    pub(super) after_init: Vec<PreparedMail>,
    pub(super) armed: bool,
    pub(super) cancel_requested: bool,
}

impl PendingBirth {
    pub(super) fn placeholder(id: MailboxId, token: ActivationToken) -> Self {
        Self {
            id,
            token,
            parked: VecDeque::new(),
            activation: None,
            costs: None,
            after_init: Vec::new(),
            armed: false,
            cancel_requested: false,
        }
    }

    pub(super) fn disarm(&mut self) {
        self.armed = false;
        self.activation = None;
        self.costs = None;
    }
}

impl Drop for PendingBirth {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Some(activation) = self.activation.take() {
            activation.reject(PreparedSpawnFailure::ActivationRejected);
        }
        if let Some(costs) = self.costs.take() {
            costs.rollback(self.id, self.token);
        }
    }
}

pub enum CapturedDisposition {
    Live { endpoint: RouteEndpoint },
    Dropped,
    Unknown,
}

pub struct RouteContinuation {
    pub(crate) mail: Mail,
    pub(crate) disposition: CapturedDisposition,
}

impl Registry {
    /// First ack of the post-seal pumped activation handshake (ADR-0165):
    /// reserve `name` as a `Starting` route through the owner and block for
    /// the token it assigned.
    ///
    /// Mail addressed here from now on parks in the owner rather than
    /// warn-dropping, so the window between the reservation and the caller's
    /// `wire` loses nothing. The caller is an embedder thread, never a pool
    /// worker, so waiting on the owner cannot starve it.
    pub(crate) fn reserve_starting_through_owner(
        &self,
        name: &str,
    ) -> Result<(MailboxId, ActivationToken), RegistryEffectError> {
        let effect = RegistryEffect::reserve_named(name.to_owned());
        let Some(completion) = self.submit(EffectBatch::new(vec![effect])) else {
            return Err(RegistryEffectError::OwnerClosed);
        };
        match completion.wait()?.as_slice() {
            [RegistryApplied::Starting { id, token }] => Ok((*id, *token)),
            _ => Err(RegistryEffectError::ActivationRejected),
        }
    }

    /// Second ack of the same handshake: hand the owner the endpoint the
    /// caller thread finished wiring at its own execution home, and block
    /// until the route is `Live` and its parked mail has been released.
    pub(crate) fn promote_starting_through_owner(
        &self,
        id: MailboxId,
        token: ActivationToken,
        handler: Arc<dyn InboxHandler>,
    ) -> Result<(), RegistryEffectError> {
        let effect = RegistryEffect::PromoteStarting {
            id,
            token,
            activation: PreparedActivation::legacy(MailboxEntry::Inbox { handler, seize: SeizeCell::default() }),
        };
        let Some(completion) = self.submit(EffectBatch::new(vec![effect])) else {
            return Err(RegistryEffectError::OwnerClosed);
        };
        completion.wait().map(|_| ())
    }

    /// Release a reservation whose caller-thread `init` / `wire` failed. The
    /// route disappears and anything parked behind it continues under the
    /// unknown-recipient policy, exactly as a cancelled prepared birth does.
    pub(crate) fn cancel_starting_through_owner(&self, id: MailboxId, token: ActivationToken) {
        if let Some(completion) = self.submit(EffectBatch::new(vec![RegistryEffect::CancelStarting { id, token }])) {
            drop(completion.wait());
        }
    }

    pub(crate) fn activation_cancelled(&self, id: MailboxId, token: ActivationToken) {
        if let Some(owner) = self.owner.get() {
            let _ = owner.activation_cancelled(id, token);
        }
    }

    pub(super) fn cancel_completed_locked(
        inner: &mut Inner,
        id: MailboxId,
        token: ActivationToken,
        publication: &mut Publication,
    ) -> Vec<RouteContinuation> {
        let valid = inner.pending_births.get(&id).is_some_and(|birth| birth.token == token && birth.cancel_requested);
        if !valid {
            return Vec::new();
        }
        let mut birth = inner.pending_births.remove(&id).expect("validated pending cancellation exists");
        let continuations = birth
            .parked
            .drain(..)
            .map(|mail| RouteContinuation { mail, disposition: CapturedDisposition::Unknown })
            .collect();
        birth.costs.as_ref().expect("prepared cancellation retains cost reservation").rollback(id, token);
        birth.disarm();
        if matches!(
            inner.mailboxes.get(&id).map(|route| &route.lifecycle),
            Some(RouteLifecycle::Starting { token: current }) if *current == token
        ) {
            inner.mailboxes.remove(&id);
            publication.route_updates.push(Update::Remove(id));
        }
        continuations
    }

    pub(super) fn promote_locked(
        inner: &mut Inner,
        id: MailboxId,
        token: ActivationToken,
        barrier_mail_id: Option<MailId>,
        publication: &mut Publication,
    ) -> Option<Box<dyn FnOnce() + Send>> {
        if !matches!(
            inner.mailboxes.get(&id).map(|route| &route.lifecycle),
            Some(RouteLifecycle::Starting { token: current }) if *current == token
        ) {
            return None;
        }
        let mut birth = inner.pending_births.remove(&id).expect("Starting route retains its pending birth");
        if birth.token != token {
            inner.pending_births.insert(id, birth);
            return None;
        }
        if birth.cancel_requested {
            inner.pending_births.insert(id, birth);
            return None;
        }
        let activation = birth.activation.as_ref().expect("prepared Starting birth retains activation");
        if barrier_mail_id.is_some_and(|mail_id| !activation.barrier_matches(mail_id)) {
            inner.pending_births.insert(id, birth);
            return None;
        }
        let Some(live) = activation.take_live() else {
            inner.pending_births.insert(id, birth);
            return None;
        };
        let bootstrap = mem::take(&mut birth.after_init);
        let parked = birth.parked.drain(..).map(PreparedMail::parked).collect();
        let installed = live.install(bootstrap, parked);
        birth.costs.as_ref().expect("prepared Starting birth retains cost reservation").promote(id, token);

        let endpoint = match installed.entry {
            MailboxEntry::Inbox { handler, seize } => RouteEndpoint::Inbox { handler, seize },
            MailboxEntry::Inline(_) | MailboxEntry::Dropped => {
                panic!("prepared actor activation must install an inbox endpoint")
            }
        };
        let canonical_name =
            inner.mailboxes.get(&id).expect("Starting route exists while promoting").canonical_name.clone();
        let record = RouteRecord { canonical_name, lifecycle: RouteLifecycle::Live { endpoint } };
        inner.mailboxes.insert(id, record.clone());
        publication.route_updates.push(Update::Insert(id, record));
        publication.inventory_dirty = true;
        birth.disarm();
        Some(installed.catch_up)
    }

    pub(super) fn capture_mail_locked(inner: &mut Inner, mail: Mail) -> Option<RouteContinuation> {
        match resolve_route(mail.recipient, |id| inner.mailboxes.get(&id)) {
            ResolvedRoute::Starting { target } => {
                let token = match inner.mailboxes.get(&target).map(|route| &route.lifecycle) {
                    Some(RouteLifecycle::Starting { token }) => *token,
                    _ => unreachable!("resolved Starting target remains Starting under the owner lock"),
                };
                let pending = inner
                    .pending_births
                    .get_mut(&target)
                    .unwrap_or_else(|| panic!("published Starting route missing its owner-private pending birth"));
                assert_eq!(pending.token, token, "published Starting token disagrees with pending birth");
                pending.parked.push_back(mail);
                None
            }
            ResolvedRoute::Live { endpoint } => {
                Some(RouteContinuation { disposition: CapturedDisposition::Live { endpoint: endpoint.clone() }, mail })
            }
            ResolvedRoute::Dropped => Some(RouteContinuation { mail, disposition: CapturedDisposition::Dropped }),
            ResolvedRoute::Unknown => Some(RouteContinuation { mail, disposition: CapturedDisposition::Unknown }),
        }
    }
}
