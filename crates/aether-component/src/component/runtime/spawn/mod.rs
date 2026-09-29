//! A spawn asks for an instance of a published type to exist (ADR-0241 §9).
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
//! Only a guest type is spawned here: a namespace no module publishes,
//! native ones included, is refused.

use std::sync::Arc;

use aether_actor::ReplyMode;
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
        let module = ctx.published_module(&namespace).ok_or_else(|| {
            format!("no module publishes {namespace}: publish its code first (a native type is not spawned by mail)")
        })?;
        let declared = declared_name(&module, &namespace).expect("a module publishes only the types it exports");
        let placement = match parent {
            None => LoadPlacement::Root,
            Some(parent) => Self::placement_under(ctx, &parent)
                .map_err(|error| format!("spawn parent {parent} did not resolve: {error}"))?,
        };
        Self::prepare_load(ctx, Selection { module, export: Some(declared), name: key, config, placement })
    }
}
