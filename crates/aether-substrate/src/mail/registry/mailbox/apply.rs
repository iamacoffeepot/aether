//! Applying a batch of registry effects: the staged fold every writer
//! funnels through, and the direct pre-seal write path that names it.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

use rustc_hash::FxHashMap;

use aether_data::canonical::{canonical_kind_bytes, kind_id_from_parts};

use crate::mail::registry::authority::BootAuthority;
use crate::mail::registry::effect::{
    ActivationReservation, ActivationToken, EffectBatch, PreparedSpawnFailure, RegistryApplied, RegistryEffect,
    RegistryEffectError, StartingCancellation,
};
use crate::mail::registry::errors::{DropError, KindConflict, NameConflict};
use crate::mail::view::Update;
use crate::mail::{KindId, MailboxId};

use super::birth::{PendingBirth, RouteContinuation};
use super::kinds::KindSlot;
use super::publish::Publication;
use super::route::{RouteEndpoint, RouteLifecycle, RouteRecord};
use super::staged::{commit_staged, staged_kind, staged_pending_token, staged_route};
use super::{CapturedDisposition, Inner, Registry, SeizeCell};

impl Registry {
    /// The direct write path itself. Named only by [`Self::apply_one`],
    /// which every eager mutator funnels through, so the
    /// [`BootAuthority`] taken here is the single structural gate on the
    /// pre-owner writer (iamacoffeepot/aether#4161): a caller that cannot
    /// produce the token cannot reach this `inner.write()` at all.
    fn apply_batches(
        &self,
        _authority: &BootAuthority,
        batches: Vec<EffectBatch>,
    ) -> Vec<Result<Vec<RegistryApplied>, RegistryEffectError>> {
        let mut inner = self.inner.lock().expect("registry lock poisoned; fail-fast per ADR-0063");
        let mut publication = Publication::default();
        let results = batches
            .into_iter()
            .map(|batch| match Self::apply_batch_locked(&mut inner, batch) {
                Ok((applied, batch_publication, continuations, schedules)) => {
                    assert!(continuations.is_empty(), "direct legacy effects cannot cancel a pending birth");
                    assert!(schedules.is_empty(), "prepared births must run through the registry owner");
                    publication.append(batch_publication);
                    Ok(applied)
                }
                Err(error) => Err(error),
            })
            .collect();
        let inventory_changed = inner.publish(publication);
        drop(inner);
        if inventory_changed {
            self.notify_inventory_changed();
        }
        results
    }

    #[allow(clippy::too_many_lines, clippy::type_complexity)]
    pub(super) fn apply_batch_locked(
        inner: &mut Inner,
        batch: EffectBatch,
    ) -> Result<
        (Vec<RegistryApplied>, Publication, Vec<RouteContinuation>, Vec<Arc<dyn ActivationReservation>>),
        RegistryEffectError,
    > {
        let mut staged_routes = FxHashMap::<MailboxId, Option<RouteRecord>>::default();
        let mut staged_kinds = FxHashMap::<KindId, KindSlot>::default();
        let mut staged_pending = FxHashMap::<MailboxId, Option<ActivationToken>>::default();
        let mut next_activation_token = inner.next_activation_token;
        let mut publication = Publication::default();
        let mut applied = Vec::with_capacity(batch.effects.len());
        let mut prepared_births = FxHashMap::<MailboxId, PendingBirth>::default();
        let mut prepared_cancellations = HashSet::<(MailboxId, ActivationToken)>::new();
        let mut promotions = Vec::<(MailboxId, RouteEndpoint)>::new();

        for effect in batch.effects {
            match effect {
                RegistryEffect::PreparedSpawn(mut commit) => {
                    let id = commit.route.id;
                    if id == MailboxId::NONE || id == MailboxId::CHASSIS_MAILBOX_ID {
                        let name = commit.route.canonical_name.clone();
                        drop(commit.reject_at_home(PreparedSpawnFailure::SubnameInUse { full_name: name.clone() }));
                        return Err(RegistryEffectError::Name(NameConflict { name }));
                    }
                    match staged_route(&staged_routes, inner, id) {
                        // Same-name reuse of a `Dropped` route. Only
                        // `Registry::drop_mailbox` produces that lifecycle, and
                        // it is a public routing primitive no chassis or cap
                        // calls today (issue 4152 audited every caller: all are
                        // tests). Retiring an actor leaves its route in place
                        // and tombstones the id in the `ActorRegistry` instead,
                        // which is what the conflict arm below reads.
                        Some(existing)
                            if matches!(existing.lifecycle, RouteLifecycle::Dropped)
                                && existing.canonical_name == commit.route.canonical_name => {}
                        // A route already occupies this id. `reserve` — where
                        // the authoritative retired-name answer lives — is
                        // still two steps away and will never run for this
                        // birth, so classify the conflict here instead of
                        // reporting every one of them as a live occupant.
                        Some(_) => {
                            let name = commit.route.canonical_name.clone();
                            let failure = commit.route_conflict_failure();
                            drop(commit.reject_at_home(failure));
                            return Err(RegistryEffectError::Name(NameConflict { name }));
                        }
                        None => {}
                    }
                    let token = ActivationToken::next(&mut next_activation_token);
                    let activation = match commit.take_activation().reserve(token) {
                        Ok(activation) => activation,
                        Err((prepared, failure)) => {
                            drop(prepared.discard_at_home(failure));
                            return Err(RegistryEffectError::ActivationRejected);
                        }
                    };
                    if !commit.costs.prepare(id, token) {
                        activation.reject(PreparedSpawnFailure::ActivationRejected);
                        return Err(RegistryEffectError::ActivationRejected);
                    }
                    let route = commit.route;
                    let record = RouteRecord {
                        canonical_name: route.canonical_name,
                        lifecycle: RouteLifecycle::Starting { token },
                    };
                    staged_routes.insert(route.id, Some(record.clone()));
                    staged_pending.insert(route.id, Some(token));
                    publication.route_updates.push(Update::Insert(route.id, record));
                    prepared_births.insert(
                        route.id,
                        PendingBirth {
                            id: route.id,
                            token,
                            parked: VecDeque::new(),
                            activation: Some(Arc::clone(&activation)),
                            costs: Some(commit.costs),
                            after_init: commit.after_init,
                            armed: true,
                            cancel_requested: false,
                        },
                    );
                    applied.push(RegistryApplied::Starting { id: route.id, token });
                }
                RegistryEffect::PublishAlias(alias) => {
                    let name = alias.rendered_name.to_string();
                    if alias.alias == MailboxId::NONE || alias.alias == MailboxId::CHASSIS_MAILBOX_ID {
                        return Err(RegistryEffectError::Name(NameConflict { name }));
                    }
                    let target_live =
                        match staged_route(&staged_routes, inner, alias.target_parent).map(|route| &route.lifecycle) {
                            Some(RouteLifecycle::Starting { .. }) => false,
                            Some(RouteLifecycle::Live { endpoint: RouteEndpoint::Inbox { .. } }) => true,
                            _ => {
                                return Err(RegistryEffectError::AliasTargetUnavailable {
                                    alias: alias.alias,
                                    target_parent: alias.target_parent,
                                });
                            }
                        };
                    match staged_route(&staged_routes, inner, alias.alias) {
                        Some(RouteRecord { canonical_name, lifecycle: RouteLifecycle::Alias { target_parent } })
                            if canonical_name == alias.rendered_name.as_ref()
                                && *target_parent == alias.target_parent =>
                        {
                            applied.push(RegistryApplied::Mailbox(alias.alias));
                            continue;
                        }
                        Some(existing)
                            if matches!(existing.lifecycle, RouteLifecycle::Dropped)
                                && existing.canonical_name == alias.rendered_name.as_ref() => {}
                        Some(_) => return Err(RegistryEffectError::Name(NameConflict { name })),
                        None => {}
                    }
                    let record = RouteRecord {
                        canonical_name: name,
                        lifecycle: RouteLifecycle::Alias { target_parent: alias.target_parent },
                    };
                    staged_routes.insert(alias.alias, Some(record.clone()));
                    publication.route_updates.push(Update::Insert(alias.alias, record));
                    publication.inventory_dirty |= target_live;
                    applied.push(RegistryApplied::Mailbox(alias.alias));
                }
                RegistryEffect::RetireAlias(alias) => {
                    // Only an alias record is retirable here — see the effect's
                    // own doc for why that is structural rather than checked at
                    // the caller. Anything else (absent, already `Dropped`, or a
                    // real mailbox that happens to answer to this id) falls
                    // through as a clean `false`, which is what makes a
                    // re-despawn idempotent.
                    let Some(mut record) = staged_route(&staged_routes, inner, alias).cloned() else {
                        applied.push(RegistryApplied::AliasRetired(false));
                        continue;
                    };
                    let RouteLifecycle::Alias { target_parent } = record.lifecycle else {
                        applied.push(RegistryApplied::AliasRetired(false));
                        continue;
                    };
                    // The alias only occupied the live inventory while its
                    // target parent was live, so only then does retiring it
                    // change what the inventory publishes.
                    let inventory_live = staged_route(&staged_routes, inner, target_parent)
                        .is_some_and(|target| matches!(target.lifecycle, RouteLifecycle::Live { .. }));

                    record.lifecycle = RouteLifecycle::Dropped;
                    staged_routes.insert(alias, Some(record.clone()));
                    publication.route_updates.push(Update::Insert(alias, record));
                    publication.inventory_dirty |= inventory_live;
                    applied.push(RegistryApplied::AliasRetired(true));
                }
                RegistryEffect::ReserveStarting { route } => {
                    if route.id == MailboxId::NONE || route.id == MailboxId::CHASSIS_MAILBOX_ID {
                        return Err(RegistryEffectError::Name(NameConflict { name: route.canonical_name }));
                    }
                    match staged_route(&staged_routes, inner, route.id) {
                        Some(existing)
                            if matches!(existing.lifecycle, RouteLifecycle::Dropped)
                                && existing.canonical_name == route.canonical_name => {}
                        Some(_) => {
                            return Err(RegistryEffectError::Name(NameConflict { name: route.canonical_name }));
                        }
                        None => {}
                    }
                    let token = ActivationToken::next(&mut next_activation_token);
                    let record = RouteRecord {
                        canonical_name: route.canonical_name,
                        lifecycle: RouteLifecycle::Starting { token },
                    };
                    staged_routes.insert(route.id, Some(record.clone()));
                    staged_pending.insert(route.id, Some(token));
                    publication.route_updates.push(Update::Insert(route.id, record));
                    applied.push(RegistryApplied::Starting { id: route.id, token });
                }
                RegistryEffect::PromoteStarting { id, token, activation } => {
                    let reserved = matches!(
                        staged_route(&staged_routes, inner, id).map(|route| &route.lifecycle),
                        Some(RouteLifecycle::Starting { token: current }) if *current == token
                    ) && staged_pending_token(&staged_pending, inner, id) == Some(token);
                    let Some(canonical_name) = reserved
                        .then(|| staged_route(&staged_routes, inner, id).map(|route| route.canonical_name.clone()))
                        .flatten()
                    else {
                        return Err(RegistryEffectError::ActivationRejected);
                    };
                    let endpoint = RouteEndpoint::from_entry(activation.into_legacy());
                    let record =
                        RouteRecord { canonical_name, lifecycle: RouteLifecycle::Live { endpoint: endpoint.clone() } };
                    staged_routes.insert(id, Some(record.clone()));
                    publication.route_updates.push(Update::Insert(id, record));
                    publication.inventory_dirty = true;
                    promotions.push((id, endpoint));
                    applied.push(RegistryApplied::Mailbox(id));
                }
                RegistryEffect::CancelStarting { id, token } => {
                    let prepared_cancel = !staged_routes.contains_key(&id)
                        && inner.pending_births.get(&id).is_some_and(|birth| {
                            birth.token == token && birth.activation.is_some() && !birth.cancel_requested
                        });
                    let cancellation = if prepared_cancel {
                        prepared_cancellations.insert((id, token));
                        StartingCancellation::Cancelled(id)
                    } else {
                        match staged_route(&staged_routes, inner, id) {
                            Some(RouteRecord { lifecycle: RouteLifecycle::Starting { token: current }, .. })
                                if *current == token
                                    && staged_pending_token(&staged_pending, inner, id) == Some(token) =>
                            {
                                staged_routes.insert(id, None);
                                staged_pending.insert(id, None);
                                publication.route_updates.push(Update::Remove(id));
                                StartingCancellation::Cancelled(id)
                            }
                            Some(RouteRecord { lifecycle: RouteLifecycle::Starting { .. }, .. }) => {
                                StartingCancellation::TokenMismatch(id)
                            }
                            _ => StartingCancellation::NotStarting(id),
                        }
                    };
                    applied.push(RegistryApplied::StartingCancellation(cancellation));
                }
                RegistryEffect::PublishLive { route, activation } => {
                    if route.id == MailboxId::NONE || route.id == MailboxId::CHASSIS_MAILBOX_ID {
                        return Err(RegistryEffectError::Name(NameConflict { name: route.canonical_name }));
                    }
                    let record = RouteRecord {
                        canonical_name: route.canonical_name.clone(),
                        lifecycle: RouteLifecycle::Live {
                            endpoint: RouteEndpoint::from_entry(activation.into_legacy()),
                        },
                    };
                    match staged_route(&staged_routes, inner, route.id) {
                        Some(existing)
                            if matches!(existing.lifecycle, RouteLifecycle::Dropped)
                                && existing.canonical_name == route.canonical_name => {}
                        Some(_) => {
                            return Err(RegistryEffectError::Name(NameConflict { name: route.canonical_name }));
                        }
                        None => {}
                    }
                    staged_routes.insert(route.id, Some(record.clone()));
                    publication.route_updates.push(Update::Insert(route.id, record));
                    publication.inventory_dirty = true;
                    applied.push(RegistryApplied::Mailbox(route.id));
                }
                RegistryEffect::DropMailbox(id) => {
                    let Some(mut record) = staged_route(&staged_routes, inner, id).cloned() else {
                        return Err(RegistryEffectError::Drop(DropError::UnknownId(id)));
                    };
                    let inventory_live = match &record.lifecycle {
                        RouteLifecycle::Starting { .. } => {
                            return Err(RegistryEffectError::Drop(DropError::UnknownId(id)));
                        }
                        RouteLifecycle::Dropped => {
                            return Err(RegistryEffectError::Drop(DropError::AlreadyDropped(id)));
                        }
                        RouteLifecycle::Live { .. } => true,
                        RouteLifecycle::Alias { target_parent } => staged_route(&staged_routes, inner, *target_parent)
                            .is_some_and(|target| matches!(target.lifecycle, RouteLifecycle::Live { .. })),
                    };
                    record.lifecycle = RouteLifecycle::Dropped;
                    let name = record.canonical_name.clone();
                    staged_routes.insert(id, Some(record.clone()));
                    publication.route_updates.push(Update::Insert(id, record.clone()));
                    publication.inventory_dirty |= inventory_live;
                    applied.push(RegistryApplied::Dropped(name));
                }
                RegistryEffect::RemoveMailbox(id) => {
                    let (removable, inventory_live) =
                        staged_route(&staged_routes, inner, id).map_or((false, false), |record| {
                            match &record.lifecycle {
                                RouteLifecycle::Live { .. } => (true, true),
                                RouteLifecycle::Alias { target_parent } => (
                                    true,
                                    staged_route(&staged_routes, inner, *target_parent)
                                        .is_some_and(|target| matches!(target.lifecycle, RouteLifecycle::Live { .. })),
                                ),
                                RouteLifecycle::Starting { .. } | RouteLifecycle::Dropped => (false, false),
                            }
                        });
                    if removable {
                        staged_routes.insert(id, None);
                        publication.route_updates.push(Update::Remove(id));
                        publication.inventory_dirty |= inventory_live;
                    }
                    applied.push(RegistryApplied::Removed(removable));
                }
                RegistryEffect::InstallSeize { id, handle } => {
                    let Some(mut record) = staged_route(&staged_routes, inner, id).cloned() else {
                        applied.push(RegistryApplied::SeizeInstalled(false));
                        continue;
                    };
                    let RouteLifecycle::Live { endpoint: RouteEndpoint::Inbox { handler, seize } } = &record.lifecycle
                    else {
                        applied.push(RegistryApplied::SeizeInstalled(false));
                        continue;
                    };
                    if seize.get().is_some() {
                        applied.push(RegistryApplied::SeizeInstalled(false));
                        continue;
                    }
                    let replacement = SeizeCell::default();
                    assert!(replacement.set(handle).is_ok(), "fresh seize cell must accept its first handle");
                    record.lifecycle = RouteLifecycle::Live {
                        endpoint: RouteEndpoint::Inbox { handler: Arc::clone(handler), seize: replacement },
                    };
                    staged_routes.insert(id, Some(record.clone()));
                    publication.route_updates.push(Update::Insert(id, record.clone()));
                    applied.push(RegistryApplied::SeizeInstalled(true));
                }
                RegistryEffect::RegisterKind { descriptor, reject_conflict } => {
                    let id = KindId(kind_id_from_parts(&descriptor.name, &descriptor.schema));
                    if let Some(slot) = staged_kind(&staged_kinds, inner, id) {
                        if reject_conflict
                            && canonical_kind_bytes(&slot.descriptor.name, &slot.descriptor.schema)
                                != canonical_kind_bytes(&descriptor.name, &descriptor.schema)
                        {
                            return Err(RegistryEffectError::Kind(KindConflict {
                                name: descriptor.name,
                                existing: slot.descriptor.schema.clone(),
                                requested: descriptor.schema,
                            }));
                        }
                    } else {
                        let name = Arc::from(descriptor.name.as_str());
                        staged_kinds.insert(id, KindSlot { name, descriptor });
                        publication.kinds_dirty = true;
                    }
                    applied.push(RegistryApplied::Kind(id));
                }
            }
        }

        inner.next_activation_token = next_activation_token;
        let mut continuations = commit_staged(inner, staged_routes, staged_kinds, staged_pending);
        // The promoted route is Live now, so the mail parked behind its
        // `Starting` reservation continues to the endpoint the caller thread
        // just wired — in the order the owner observed it, ahead of anything
        // routed after this apply.
        for (id, endpoint) in promotions {
            let Some(mut birth) = inner.pending_births.remove(&id) else {
                continue;
            };
            continuations.extend(birth.parked.drain(..).map(|mail| RouteContinuation {
                disposition: CapturedDisposition::Live { endpoint: endpoint.clone() },
                mail,
            }));
            birth.disarm();
        }
        for (id, token) in prepared_cancellations {
            let birth = inner.pending_births.get_mut(&id).expect("validated prepared cancellation remains pending");
            assert_eq!(birth.token, token, "validated prepared cancellation retains its exact token");
            birth.cancel_requested = true;
            birth.activation.as_ref().expect("prepared birth retains activation").cancel();
        }
        let mut schedules = Vec::with_capacity(prepared_births.len());
        for (id, birth) in prepared_births {
            schedules.push(Arc::clone(birth.activation.as_ref().expect("prepared birth retains activation")));
            inner.pending_births.insert(id, birth);
        }
        Ok((applied, publication, continuations, schedules))
    }

    pub(super) fn apply_one(
        &self,
        authority: &BootAuthority,
        effect: RegistryEffect,
    ) -> Result<RegistryApplied, RegistryEffectError> {
        self.apply_batches(authority, vec![EffectBatch::new(vec![effect])])
            .pop()
            .expect("one submitted batch returns one result")?
            .pop()
            .ok_or_else(|| RegistryEffectError::Name(NameConflict { name: "empty registry effect".to_owned() }))
    }
}
