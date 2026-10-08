//! A spawn asks for an instance of a published type to exist (ADR-0241 §9,
//! ADR-0250).
//!
//! The type's code is the module the publication table binds its namespace
//! to (§3), read from the registry rather than kept here. The spawn prepares
//! the guest the way a load does, placed and keyed by the same rules, and
//! the name it would take decides the answer through the load's spawn half:
//! a live name answers with the instance there, an absent one stands the
//! guest up, and a tombstoned one is refused by its birth, since the name is
//! spent (§8). A spawn of a namespace a republish holds waits until the
//! republish answers, then runs against the code that won (§7).
//!
//! A spawn brings no bytes: the guest reads its assets from the module that
//! publishes its namespace, in every hook (ADR-0250 §4).
//!
//! Only a published guest type is spawned here. A native namespace is
//! refused: native types are composed by their chassis or parent, and
//! spawning one by mail is not supported yet. A namespace no module
//! publishes is refused until its code is published.

use std::sync::Arc;

use aether_actor::ReplyMode;
use aether_data::name_inventory::native_type_entries;
use aether_kinds::{Spawn, SpawnResult};
use aether_substrate::actor::native::Held;

use super::load::{LoadPlacement, PreparedLoad, Requester, Selection, declared_name};
use crate::component::runtime::{ComponentHostCapabilityState, HostCtx};

impl ComponentHostCapabilityState {
    /// Spawn the instance a `Spawn` names, or park it while its namespace
    /// republishes.
    pub(super) fn begin_spawn<M: ReplyMode>(
        &mut self,
        ctx: &mut HostCtx<'_, M>,
        held: Held<SpawnResult>,
        payload: Spawn,
    ) {
        if let Some(republish) = self.holding_republish(ctx, &payload.namespace) {
            republish.park_spawn(held, payload);
            return;
        }
        match Self::prepare_spawn(ctx, payload) {
            Ok(load) => self.spawn_prepared(ctx, Requester::Spawn(held), load),
            Err(error) => held.answer(ctx, &SpawnResult::Err { error }),
        }
    }

    /// Prepare the guest a `Spawn` asks for from the module that publishes
    /// its namespace, placed beneath its proven parent when it names one.
    fn prepare_spawn<M: ReplyMode>(ctx: &HostCtx<'_, M>, payload: Spawn) -> Result<Arc<PreparedLoad>, String> {
        let Spawn { namespace, key, parent, config } = payload;
        if native_type_entries().any(|entry| entry.namespace == namespace) {
            return Err(format!(
                "{namespace} is a native type: native types are composed by their chassis or parent; spawning one by mail is not supported yet"
            ));
        }
        let module = ctx
            .published_module(&namespace)
            .ok_or_else(|| format!("no module publishes {namespace}: publish its code first"))?;
        let declared = declared_name(&module, &namespace).expect("a module publishes only the types it exports");
        let placement = match parent {
            None => LoadPlacement::Root,
            Some(parent) => Self::placement_under(ctx, &parent)
                .map_err(|error| format!("spawn parent {parent} did not resolve: {error}"))?,
        };
        let selection = Selection { module, export: Some(declared), name: key, config, placement };
        Self::prepare_load(ctx, selection)
    }
}
