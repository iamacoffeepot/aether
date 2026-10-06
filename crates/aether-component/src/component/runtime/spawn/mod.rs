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
//! A spawn may bring the published module's bytes, and its guest's load
//! window then reads the module's assets from them (ADR-0163 §4). The host
//! keeps none after a publish, so the spawn is the door that brings them.
//! Brought bytes that are not the module the namespace is bound to are
//! refused, because the asset ranges the window reads are that module's. A
//! spawn that brings none opens a window that serves no payload.
//!
//! Only a published guest type is spawned here. A native namespace is
//! refused: native types are composed by their chassis or parent, and
//! spawning one by mail is not supported yet. A namespace no module
//! publishes is refused until its code is published.

use std::sync::Arc;

use aether_actor::ReplyMode;
use aether_data::Blob;
use aether_data::name_inventory::native_type_entries;
use aether_kinds::{Spawn, SpawnResult};
use aether_substrate::actor::native::Held;
use aether_substrate::actor::wasm::module::Module;

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
        match self.prepare_spawn(ctx, payload) {
            Ok(load) => self.spawn_prepared(ctx, Requester::Spawn(held), load),
            Err(error) => held.answer(ctx, &SpawnResult::Err { error }),
        }
    }

    /// Prepare the guest a `Spawn` asks for from the module that publishes
    /// its namespace, placed beneath its proven parent when it names one.
    fn prepare_spawn<M: ReplyMode>(&self, ctx: &HostCtx<'_, M>, payload: Spawn) -> Result<Arc<PreparedLoad>, String> {
        let Spawn { namespace, key, parent, config, code } = payload;
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
        // ADR-0163 §4: the guest's load window reads the module's assets
        // from the code its spawn brought. A spawn that brought none opens a
        // window that answers the catalog and refuses a catalogued asset.
        let code = self.brought_code(ctx, &namespace, &module, code)?;
        let selection = Selection { module, code, export: Some(declared), name: key, config, placement };
        Self::prepare_load(ctx, selection)
    }

    /// The code a spawn of `namespace` brought, once it is known to be
    /// `published`, the module the namespace is bound to. The bytes are
    /// checked in through the module cache, a hit for a published module, and
    /// refused when they are any other module: a load window reads the
    /// published module's asset ranges out of them.
    fn brought_code<M: ReplyMode>(
        &self,
        ctx: &HostCtx<'_, M>,
        namespace: &str,
        published: &Module,
        code: Option<Blob>,
    ) -> Result<Option<Blob>, String> {
        let Some(code) = code else {
            return Ok(None);
        };
        let brought = self
            .modules
            .check_in(&ctx.blob_check_in(), &code)
            .map_err(|error| format!("the code brought to spawn {namespace} is not a module: {error}"))?;
        if brought.hash() != published.hash() {
            return Err(format!(
                "the code brought to spawn {namespace} is not the module that publishes it: bring the published module's bytes, or none"
            ));
        }
        Ok(Some(code))
    }
}
