//! A spawn of a native type (ADR-0241 §9): a type linked into this binary is
//! published at boot, and a spawn stands one up from that publication.
//!
//! The type's `#[actor]` declaration decides everything a guest's module
//! decides for it. A type whose `Params` is not `()` takes construction
//! wiring from its composer or parent, so no spawn can build it; a type whose
//! `Config` is that wiring is refused the same way. A singleton is composed
//! at boot: a spawn of one the chassis composed answers with it, and one it
//! did not is refused. An instanced type is placed by its `root` and
//! `child_of` declarations and keyed like a guest: a live name answers with
//! the instance there, which is not re-initialised, an absent one stands the
//! instance up with its `Config` resolved from the engine's config sources,
//! and a tombstoned one is refused by its birth, since the name is spent
//! (§8).
//!
//! Every answer but a refusal comes from the instance itself, through the
//! framework row every native actor serves, so the requester keeps the
//! reply's stamped sender as its reference (ADR-0230 §3). A staged birth
//! carries the held reply into its completion as its context (ADR-0243 §9).

use aether_actor::{ErasedActorRef, ReplyMode};
use aether_data::ErasedActorPath;
use aether_data::name_inventory::native_type_entries;
use aether_kinds::{ActorSpawnDelivered, Spawn, SpawnResult};
use aether_substrate::actor::native::{Held, NativeSpawnEntry, NativeSpawnOutcome, Subname, TaskDone};

use crate::component::runtime::placement::leaf_namespace;
use crate::component::runtime::{ComponentHostCapabilityState, HostCtx};

/// `aether.component.native_spawned` — the context a native spawn's staged
/// birth carries into its completion (ADR-0243 §9): the spawn's held reply,
/// which the born instance answers.
#[aether_data::kind(name = "aether.component.native_spawned")]
struct NativeSpawned {
    held: Held<SpawnResult>,
}

/// Whether `namespace` is a native actor's: a type linked into this binary
/// declares it (ADR-0241 §3).
pub(super) fn is_native(namespace: &str) -> bool {
    native_type_entries().any(|entry| entry.namespace == namespace)
}

impl ComponentHostCapabilityState {
    /// Answer a `Spawn` of the native type `payload.namespace` names: hand
    /// the held reply to the live or newly staged instance, or refuse.
    pub(super) fn begin_native_spawn<M: ReplyMode>(ctx: &mut HostCtx<'_, M>, held: Held<SpawnResult>, payload: Spawn) {
        let Spawn { namespace, key, parent, config } = payload;
        if !config.is_empty() {
            let error = format!(
                "{namespace} is a native type, whose config resolves from the engine's config sources: \
                 a spawn of it carries no config bytes"
            );
            held.answer(ctx, &SpawnResult::Err { error });
            return;
        }
        let entries: Vec<&'static NativeSpawnEntry> = NativeSpawnEntry::declaring(&namespace).collect();
        let Some(entry) = entries.first().copied() else {
            let error = format!(
                "{namespace} is a native type whose Params is not (): it is built with wiring its composer or \
                 parent hands it, so it is not spawned by mail"
            );
            held.answer(ctx, &SpawnResult::Err { error });
            return;
        };
        let parent = match parent.map(|parent| Self::native_parent(ctx, &parent)).transpose() {
            Ok(parent) => parent,
            Err(error) => {
                held.answer(ctx, &SpawnResult::Err { error });
                return;
            }
        };

        if let Err(error) = Self::native_placement(ctx, &entries, key.as_deref(), parent) {
            held.answer(ctx, &SpawnResult::Err { error });
            return;
        }
        if let Some(live) = ctx.live_native(entry, key.as_deref(), parent) {
            held.hand_off(ctx, live, &ActorSpawnDelivered { live: true });
            return;
        }
        if !entry.instanced() {
            let error = format!(
                "{namespace} is a native singleton this chassis did not compose: a native singleton is composed \
                 at boot, never spawned by mail"
            );
            held.answer(ctx, &SpawnResult::Err { error });
            return;
        }

        let key = key.as_deref().map_or(Subname::Counter, Subname::Named);
        if let Err((error, NativeSpawned { held })) = ctx.spawn_native(entry, key, parent, NativeSpawned { held }) {
            held.answer(ctx, &SpawnResult::Err { error: format!("native spawn failed: {error:?}") });
        }
    }

    /// The live actor `parent` names, which a native instance is placed
    /// beneath (ADR-0230 §1: a `Starting` parent does not prove).
    fn native_parent<M: ReplyMode>(ctx: &HostCtx<'_, M>, parent: &ErasedActorPath) -> Result<ErasedActorRef, String> {
        ctx.resolve_path(parent).map_err(|error| format!("spawn parent {parent} did not resolve: {error}"))
    }

    /// The refusal of a native spawn's key and placement, or `Ok` when the
    /// type's declaration takes them (ADR-0241 §5): a singleton names no key
    /// and no parent, and an instanced type is stageable by mail, declared by
    /// one linked type, and placed at the root only when it declares `root`
    /// and beneath a parent only when it declares `child_of` the parent's
    /// type.
    fn native_placement<M: ReplyMode>(
        ctx: &HostCtx<'_, M>,
        entries: &[&'static NativeSpawnEntry],
        key: Option<&str>,
        parent: Option<ErasedActorRef>,
    ) -> Result<(), String> {
        let entry = entries[0];
        let namespace = entry.namespace();
        if !entry.instanced() {
            return match (key, parent) {
                (Some(_), _) => Err(format!("{namespace} is a singleton; a spawn names no key")),
                (None, Some(_)) => {
                    Err(format!("{namespace} is a singleton; it is named at the root and has no parent"))
                }
                (None, None) => Ok(()),
            };
        }
        if !entry.stageable() {
            return Err(format!(
                "{namespace} ({}) takes its Config as wiring from the parent that spawns it, not from the \
                 engine's config sources, so it is not spawned by mail",
                entry.type_name()
            ));
        }
        if entries.len() > 1 {
            let types: Vec<&str> = entries.iter().map(|entry| entry.type_name()).collect();
            return Err(format!("{namespace} is declared by several native types {types:?}; compose the one to use"));
        }
        match parent {
            None if !entry.declares_root() => {
                Err(format!("{namespace} cannot be placed at the root: its #[actor] declares no `root` (ADR-0241 §5)"))
            }
            Some(parent) => {
                let path = ctx.actor_path(parent);
                let parent_namespace = leaf_namespace(&path);
                if entry.declares_child_of(parent_namespace) {
                    Ok(())
                } else {
                    Err(format!(
                        "{namespace} cannot be placed beneath {parent_namespace}: its #[actor] declares no \
                         `child_of` naming the parent's type (ADR-0241 §5)"
                    ))
                }
            }
            None => Ok(()),
        }
    }

    /// A native spawn's birth settled: the born instance answers the
    /// requester in its own name, or the host refuses with the birth's
    /// error, a spent name's `SubnameRetired` included.
    pub(in crate::component::runtime) fn finish_native_spawn<M: ReplyMode>(
        ctx: &mut HostCtx<'_, M>,
        done: TaskDone<NativeSpawnOutcome>,
    ) {
        let Some(NativeSpawned { held }) = ctx.take_context() else {
            return;
        };
        match done.into_output().result {
            Ok(delivery) => held.hand_off(ctx, delivery, &ActorSpawnDelivered { live: false }),
            Err(error) => held.answer(ctx, &SpawnResult::Err { error: format!("native spawn failed: {error:?}") }),
        }
    }
}
