#![allow(clippy::needless_pass_by_value)]

use std::collections::HashSet;
use std::fmt::Display;
use std::sync::Arc;

use aether_actor::Local as _;
use aether_actor::Single;
use aether_data::canonical::kind_id_from_parts;
use aether_kinds::{ComponentCapabilities, ReplaceComponent, ReplaceResult};
use aether_substrate::actor::native::spawn::Subname;
use aether_substrate::actor::native::{
    Dispatch, NativeCtx, RegistryBatch, RegistryBatchResult, SpawnOutcome, TaskDone,
};
use aether_substrate::actor::wasm::asset_manifest;
use aether_substrate::actor::wasm::component::{Component, ComponentCtx, PendingSpawn, StateBundle};
use aether_substrate::actor::wasm::kind_manifest;
use aether_substrate::actor::wasm::kind_manifest::ActorInputs;
use aether_substrate::mail::registry::PreparedAliasRoute;
use aether_substrate::mail::{CostCells, KindId, MailboxId};
use wasmtime::Module;

use crate::component::replacement_refusal;
use crate::trampoline::WasmTrampoline;

use super::config::WasmTrampolineConfig;
use super::contract;
use super::state::WasmTrampolineState;

impl WasmTrampolineState {
    /// Publish the logical inline-child routes a guest call staged. The
    /// owner batch is reserved admission; completion is a later no-reply
    /// actor turn so rejection cannot silently lose the originating chain.
    pub fn stage_inline_aliases<A>(&self, ctx: &mut NativeCtx<'_, A, Single>, aliases: Vec<PreparedAliasRoute>) {
        for alias in aliases {
            let alias_id = alias.alias;
            let _ = ctx.stage_registry_batch(
                RegistryBatch::publish_alias(alias),
                InlineAliasContext { parent: self.mailbox, alias: alias_id },
            );
        }
    }

    /// Retire the logical inline-child routes a guest call despawned (#4228),
    /// the teardown mirror of [`Self::stage_inline_aliases`]. Each alias fires
    /// its departure notices here, from this actor's own turn, so a cap keying
    /// rows on the child's stamped identity (ADR-0114 §4) reclaims them; the
    /// route retirement itself is staged through the owner alongside.
    pub fn stage_inline_alias_retirements<A>(&self, ctx: &mut NativeCtx<'_, A, Single>, aliases: Vec<MailboxId>) {
        for alias in aliases {
            ctx.vacate_alias(alias);
            let _ = ctx.stage_registry_batch(
                RegistryBatch::retire_alias(alias),
                InlineAliasContext { parent: self.mailbox, alias },
            );
        }
    }

    pub(super) fn finish_inline_aliases(done: TaskDone<RegistryBatchResult, InlineAliasContext>) {
        if let Err(error) = done.output() {
            tracing::warn!(
                target: "aether_component",
                parent = %done.context().parent,
                alias = %done.context().alias,
                "inline-child alias registry batch failed after owner staging: {error}",
            );
        }
        done.release_no_reply();
    }

    /// ADR-0097: perform the sibling spawn the guest staged via the
    /// `spawn_sibling` host fn during `Component::deliver`. The
    /// trampoline runs the typed `spawn_child::<WasmTrampoline>` (the
    /// identity ZST) the substrate host fn couldn't (it can't name this
    /// type), reusing
    /// the resident `Module` and registering the spawned sibling's
    /// own capability group (looked up by actor-type tag). A
    /// spawn-time failure surfaces here, asynchronously to the guest
    /// (which already received the `MailboxId`): logged, not fatal.
    pub fn spawn_sibling(&self, ctx: &mut NativeCtx<'_, WasmTrampoline, Single>, pending: PendingSpawn) {
        let capabilities = self
            .actor_caps
            .iter()
            .find(|actor| {
                // Runtime-name match: hash each loaded actor's declared
                // namespace (from module metadata) to find the one whose
                // tag the spawn requested — not a hardcoded sibling.
                #[allow(clippy::disallowed_methods)]
                actor.namespace.as_deref().is_some_and(|ns| aether_data::mailbox_id_from_name(ns).0 == pending.tag)
            })
            .map(|actor| actor.capabilities.clone())
            .unwrap_or_default();
        let config = WasmTrampolineConfig {
            engine: Arc::clone(&self.engine),
            linker: Arc::clone(&self.linker),
            module: self.module.clone(),
            registry: Arc::clone(&self.registry),
            outbound: Arc::clone(&self.outbound),
            capabilities: capabilities.clone(),
            config: pending.config,
            type_tag: Some(pending.tag),
            actor_caps: self.actor_caps.clone(),
            // ADR-0163 §3 (#3984): the sibling shares this module's bytes, so
            // it indexes its own asset load window from the same content.
            wasm_bytes: Arc::clone(&self.wasm_bytes),
        };
        if let Err(e) = ctx
            .spawn_child_scoped::<WasmTrampoline>(
                pending.parent,
                Arc::from(pending.parent_name.as_str()),
                Subname::Named(&pending.subname),
                config,
                (),
            )
            .stage_with(SiblingSpawnContext { parent: pending.parent, subname: pending.subname.clone(), capabilities })
        {
            tracing::warn!(
                target: "aether_component",
                parent = %pending.parent,
                subname = %pending.subname,
                "sibling spawn failed: {e:?}",
            );
        }
    }

    pub(super) fn finish_sibling_spawn(&self, done: TaskDone<SpawnOutcome<WasmTrampoline>, SiblingSpawnContext>) {
        match &done.output().result {
            Ok(_) => {
                self.mailer.capability_registry().register(done.output().mailbox_id, &done.context().capabilities);
            }
            Err(error) => {
                tracing::warn!(
                    target: "aether_component",
                    parent = %done.context().parent,
                    subname = %done.context().subname,
                    "sibling spawn failed after owner staging: {error:?}",
                );
            }
        }
        done.release_no_reply();
    }

    /// ADR-0096: resolve the **effective tag** an export-targeted replace
    /// instantiates, paired with the group it will host: the group whose
    /// capabilities the reply advertises and whose dependencies the replace
    /// checks. `export = Some(ns)` names an exported actor type of the
    /// replacement module, hashed to its tag the same way the load path
    /// resolves `LoadComponent.export` (component.rs `handle_load`); an
    /// export the new module doesn't declare is a clean `Err`, mirroring the
    /// load "export not found" message. `export = None` reuses the type THIS
    /// trampoline currently hosts (`self.type_tag`): with no tag, the
    /// module's default — the first actor that is not `boot` (ADR-0147), the
    /// same selection `begin_load` makes — else the actor whose namespace
    /// hashes to the tag, with an `Err` if the new module doesn't export it.
    /// `boot` is the boot namespace of the module `actors` came from; the
    /// group is `None` only for a module that declares no group. The
    /// returned tag drives both the reply capabilities and
    /// `Component::instantiate`, and on success the caller promotes it to the
    /// new `self.type_tag` so a later bare replace reuses the *current*
    /// hosted type rather than reverting to the original load's.
    pub fn resolve_replace_target<'a>(
        &self,
        export: Option<&str>,
        actors: &'a [ActorInputs],
        boot: Option<&str>,
    ) -> Result<(Option<&'a ActorInputs>, Option<u64>), String> {
        if let Some(requested) = export {
            let group = actors.iter().find(|a| a.namespace.as_deref() == Some(requested)).ok_or_else(|| {
                let available: Vec<&str> = actors.iter().filter_map(|a| a.namespace.as_deref()).collect();
                format!("export {requested:?} not found in module; exported types: {available:?}")
            })?;
            return Ok((
                Some(group),
                // Runtime-name routing: `requested` is the export
                // namespace from the wire replace request, resolved to
                // its actor-type tag exactly as the load path does.
                #[allow(clippy::disallowed_methods)]
                Some(aether_data::mailbox_id_from_name(requested).0),
            ));
        }
        // Bare replace (`export: None`): reuse the type this trampoline
        // currently hosts. With no tag (a load by the module's default) that's
        // the first non-boot actor, as `begin_load` picks it, with a `None`
        // tag — `export!(boot = B, default = A, …)` lists `B` first.
        let Some(tag) = self.type_tag else {
            let hosted = actors.iter().find(|a| boot.is_none_or(|boot| a.namespace.as_deref() != Some(boot)));
            return Ok((hosted, None));
        };
        actors
            .iter()
            .find(|a| {
                // Runtime-name match: hash each replacement actor's
                // declared namespace to find the one whose tag was
                // loaded — not a hardcoded sibling.
                #[allow(clippy::disallowed_methods)]
                a.namespace.as_deref().is_some_and(|ns| aether_data::mailbox_id_from_name(ns).0 == tag)
            })
            .map(|group| (Some(group), Some(tag)))
            .ok_or_else(|| {
                format!("replace: new module does not export the actor type (tag {tag:#x}) this trampoline loaded")
            })
    }

    /// ADR-0230: the replacement's hosted type must find every declared
    /// dependency live, checked before anything is touched. An actor the
    /// registry does not name has nothing to refuse and passes.
    fn check_dependencies(&self, group: Option<&ActorInputs>) -> Result<(), String> {
        let (Some(group), Some(canonical)) = (group, self.registry.mailbox_name(self.mailbox)) else {
            return Ok(());
        };
        replacement_refusal(&self.registry, &canonical, &group.dependencies).map_or(Ok(()), Err)
    }

    /// ADR-0231 §5: the replacement's hosted type must keep every handler
    /// row of the type this slot hosts now — the live guest, or the dropped
    /// one a refill takes over from — and may add rows. The predecessor
    /// resolves from the retained module the way the replacement does, and a
    /// failure to resolve it refuses the replace.
    fn check_contract(&self, target: &impl Display, replacement: &ComponentCapabilities) -> Result<(), String> {
        let old_boot = kind_manifest::read_boot_namespace_from_bytes(&self.wasm_bytes)?;
        let (predecessor, _) = self.resolve_replace_target(None, &self.actor_caps, old_boot.as_deref())?;
        let predecessor = predecessor.map(|group| group.capabilities.clone()).unwrap_or_default();
        contract::contract_break(&predecessor, replacement).map_or(Ok(()), |kind| {
            let name = self.registry.kind_name(kind).unwrap_or_else(|| kind.to_string());
            Err(contract::contract_refusal(target, &name))
        })
    }

    /// ADR-0139 §4 (#6429): every request context the old instance carries
    /// in its saved bundle must have a kind the replacement module declares.
    /// Only kinds the predecessor module declares are judged: a context kind
    /// defined in a shared kinds crate may be missing from both sections, and
    /// the host has no record to judge it by.
    fn check_carried_contexts(
        &self,
        target: &impl Display,
        saved: Option<&StateBundle>,
        replacement: &HashSet<KindId>,
    ) -> Result<(), String> {
        let Some(bundle) = saved else {
            return Ok(());
        };
        let (table, _, _) = aether_actor::split_state_envelope(bundle.version, &bundle.bytes);
        if table.is_empty() {
            return Ok(());
        }

        let predecessor = declared_kinds(&self.wasm_bytes)?;
        contract::undeclared_context(table.kinds(), &predecessor, replacement).map_or(Ok(()), |kind| {
            let name = self.registry.kind_name(kind).unwrap_or_else(|| kind.to_string());
            Err(contract::context_refusal(target, &name))
        })
    }

    /// Run `unwire` then `on_dehydrate` on the old instance and lift any
    /// saved-state bundle, handing the retired guest back with it so a
    /// replacement that fails to rehydrate can reinstall it (#6134). If the
    /// trampoline is currently empty (a refill after `DropComponent`), there's
    /// no prior wasm to drain and this returns `None`; the new instance starts
    /// from scratch. Issue 584 Phase 2b: `unwire` fires first so the old
    /// instance can announce its retirement before the swap.
    fn retire_guest(
        &mut self,
        target: &impl Display,
        replacement: &HashSet<KindId>,
    ) -> Result<Option<(Component, Option<StateBundle>)>, String> {
        let Some(mut old) = self.component.take() else {
            return Ok(None);
        };
        old.unwire();
        old.on_dehydrate();
        if let Some(err) = old.take_save_error() {
            // Restore the old component so the trampoline isn't
            // accidentally emptied by a save-state failure.
            self.component = Some(old);
            return Err(err);
        }
        let saved = old.take_saved_state();
        // #6429: refuse a replacement that cannot take a carried request
        // context, restoring the old component as the save-error arm does.
        // It runs before the cursor and reply table move out, so the old
        // guest keeps both, and its context table was only borrowed to
        // compose the bundle. Whatever `unwire` and `on_dehydrate` tore down
        // stays gone, as ADR-0016 §4 accepts for a rollback after the hooks.
        if let Err(error) = self.check_carried_contexts(target, saved.as_ref(), replacement) {
            self.component = Some(old);
            return Err(error);
        }
        // #6400: record the leaving guest's cursor after `unwire` and
        // `on_dehydrate`, which may still send or reply, so the
        // replacement — or a later refill — resumes past its request ids
        // and, #6422, its reply ids.
        self.retired_correlations = Some(old.correlation_cursor());
        // #6409: likewise move out its reply table, after both hooks
        // (which may still answer handles), so the replacement answers
        // the rest to their own requesters.
        self.retired_replies = Some(old.take_pending_replies());
        Ok(Some((old, saved)))
    }

    pub fn handle_replace<A>(&mut self, ctx: &mut NativeCtx<'_, A>, payload: ReplaceComponent) -> ReplaceResult {
        // `payload.wasm` is the new module bytes; `target` named this
        // trampoline and the host proved it before forwarding, so the field
        // only names the actor in a contract or carried-context refusal.
        let module = match Module::new(&self.engine, &payload.wasm) {
            Ok(m) => m,
            Err(e) => {
                return ReplaceResult::Err { error: format!("invalid wasm module: {e}") };
            }
        };

        // ADR-0033 / ADR-0096 / ADR-0097: parse every exported type's
        // capability group from the new wasm. The full `actors` set
        // refreshes `self.actor_caps` below so post-replace sibling
        // spawns see the new module's types.
        let actors = match kind_manifest::read_actor_inputs_from_bytes(&payload.wasm) {
            Ok(a) => a,
            Err(error) => return ReplaceResult::Err { error },
        };

        // ADR-0096: resolve the effective tag the replacement
        // instantiates plus the capability group to advertise —
        // export-named, or the trampoline's current hosted type for a
        // bare replace. See [`Self::resolve_replace_target`].
        let new_boot = match kind_manifest::read_boot_namespace_from_bytes(&payload.wasm) {
            Ok(boot) => boot,
            Err(error) => return ReplaceResult::Err { error },
        };
        let (group, effective_tag) =
            match self.resolve_replace_target(payload.export.as_deref(), &actors, new_boot.as_deref()) {
                Ok(resolved) => resolved,
                Err(error) => return ReplaceResult::Err { error },
            };

        // ADR-0230 and ADR-0231 §5: checked before anything is touched, so a
        // refusal leaves the old module (or the empty post-drop slot) as it
        // was.
        if let Err(error) = self.check_dependencies(group) {
            return ReplaceResult::Err { error };
        }
        let mut capabilities = group.map(|group| group.capabilities.clone()).unwrap_or_default();
        if let Err(error) = self.check_contract(&payload.target, &capabilities) {
            return ReplaceResult::Err { error };
        }

        // #6429: the replacement's kind vocabulary, parsed before the old
        // guest is touched so a malformed manifest refuses cleanly. The
        // carried contexts it is checked against surface only after the old
        // guest's `on_dehydrate`, below.
        let replacement_kinds = match declared_kinds(&payload.wasm) {
            Ok(kinds) => kinds,
            Err(error) => return ReplaceResult::Err { error },
        };

        // ADR-0163 §3 (#3984): re-index the replacement module's assets into
        // a load window. Its catalog feeds the post-swap
        // `describe_component` / `ReplaceResult`; the window itself is
        // installed on the new instance's ctx below so the replacement's
        // `init` can pull asset bytes (replace re-runs `init`, not `wire`,
        // so the window closes after instantiate). A malformed asset section
        // fails the replace loudly, before the swap runs.
        let new_wasm_bytes: Arc<[u8]> = Arc::from(payload.wasm.as_slice());
        let load_window = match asset_manifest::LoadWindow::index(Arc::clone(&new_wasm_bytes)) {
            Ok(window) => window,
            Err(error) => return ReplaceResult::Err { error },
        };
        capabilities.assets = load_window.catalog();

        // Build a fresh `ComponentCtx` for the new instance — same
        // mailer + registry/outbound/input references. Mailbox id is
        // preserved across replace per ADR-0022 §4. The correlation cursor
        // and reply table it inherits are known only once the old guest's
        // hooks have run, so both are resumed after instantiate, below.
        let mut substrate_ctx = ComponentCtx::new(
            self.mailbox,
            Arc::clone(&self.registry),
            Arc::clone(&self.mailer),
            Arc::clone(&self.outbound),
        );
        substrate_ctx.install_binding(ctx.transport_arc());
        // ADR-0163 §3 (#3984): install the load window before instantiate so
        // the replacement's `init` can pull assets; closed after instantiate
        // (replace re-runs `init`, not `wire`).
        substrate_ctx.install_load_window(load_window);

        // #6134: instantiate the candidate while the old guest is still
        // installed and wired. `init` cannot send mail, so starting it early
        // makes nothing visible outside its own store, and a failed `init`
        // drops the candidate before the old guest runs any hook.
        //
        // ADR-0090 (issue 1257): thread the replace mail's config
        // bytes into the new instance's typed `init`, the same way
        // the load path does. Empty means "no config"; a typed-config
        // guest decodes its `Self::Config` from these bytes.
        let mut new_component = match Component::instantiate(
            &self.engine,
            &self.linker,
            &module,
            substrate_ctx,
            &payload.config,
            effective_tag,
        ) {
            Ok(c) => c,
            Err(e) => {
                return ReplaceResult::Err { error: format!("wasm instantiation failed: {e}") };
            }
        };

        let predecessor = match self.retire_guest(&payload.target, &replacement_kinds) {
            Ok(predecessor) => predecessor,
            Err(error) => return ReplaceResult::Err { error },
        };

        // ADR-0139 §3 (#6400, #6422): continue the mailbox's correlation and
        // reply-lineage sequences from the guest that last left the slot, so
        // the new instance reuses neither a request id nor a reply `MailId`.
        // #6409: resume the carried reply table the same way. `init` sent
        // nothing, and both precede `on_rehydrate` and every delivery.
        if let Some(cursor) = self.retired_correlations {
            new_component.resume_correlations(cursor);
        }
        if let Some(replies) = self.retired_replies.take() {
            new_component.resume_replies(replies);
        }
        // ADR-0163 §3 (#3984): replace re-runs `init` but not `wire`, so the
        // load window's job ends once the replacement instantiated — close
        // it, retaining the catalog metadata for the instance's life.
        new_component.close_load_window();

        // ADR-0016 §4: rehydrate the new instance if the old one produced a
        // bundle. A failed rehydrate aborts the replace: the candidate drops
        // without publishing its aliases, and the old guest is reinstalled
        // with its reply table and past every id the candidate minted. The
        // old guest's `unwire` / `on_dehydrate` effects and any mail the
        // candidate sent from `on_rehydrate` stay, as ADR-0016 §4 accepts.
        // The module, hosted type, capability registration and cost cells
        // are only written below, so they stay the old guest's. On success
        // the retired guest drops here — the `Component`'s own `Drop`
        // releases its wasm store.
        if let Some((mut old, Some(bundle))) = predecessor
            && let Err(e) = new_component.call_on_rehydrate(&bundle)
        {
            old.resume_replies(new_component.take_pending_replies());
            old.resume_correlations(new_component.correlation_cursor());
            self.component = Some(old);
            return ReplaceResult::Err { error: format!("on_rehydrate failed: {e}") };
        }

        // ADR-0097: the new module is now resident — retain it (and
        // the refreshed per-type cap map) so sibling spawns after this
        // replace re-instantiate the new code, not the old.
        self.module = module;
        self.actor_caps = actors;
        // ADR-0163 §3 (#3984): future sibling spawns index the new module's
        // assets, not the replaced module's.
        self.wasm_bytes = new_wasm_bytes;
        // ADR-0096: track the actor type this trampoline now hosts, so
        // a later bare (`export: None`) replace reuses the *current*
        // type rather than reverting to the original load's. A bare
        // replace leaves this unchanged (`effective_tag == self.type_tag`).
        self.type_tag = effective_tag;

        let (aliases, retired) =
            (new_component.drain_pending_aliases(), new_component.drain_pending_alias_retirements());
        self.component = Some(new_component);
        self.stage_inline_aliases(ctx, aliases);
        self.stage_inline_alias_retirements(ctx, retired);

        // iamacoffeepot/aether#1037: re-register the trampoline's
        // capabilities against the post-replace handler set. The
        // mailbox id is stable across replace (ADR-0022 §4), so
        // `register` overwrites the prior entry — the validator
        // sees the new accept-set immediately.
        self.mailer.capability_registry().register(self.mailbox, &capabilities);

        // iamacoffeepot/aether#1128: re-seed the per-handler cost
        // cells against the post-replace handler set, into BOTH
        // indexes. The mailbox id is stable across replace
        // (ADR-0022 §4), so the global `seed` reuses the prior cell
        // for an unchanged kind (keeping its accumulated EWMA) and
        // adds a neutral cell for a new one. `on_replace_component`
        // runs on the trampoline's own dispatch thread inside
        // `with_stamped`, so we can re-stamp the per-actor `CostCells`
        // cache directly with the freshly-returned `Arc`s — keeping
        // the cache exact across replace (a new kind's cell would
        // otherwise miss until the cache happened to re-pull).
        //
        // The trampoline's own framework arms ride along
        // (iamacoffeepot/aether#4269): `capabilities` is the *guest's* handler
        // set, and seeding from it alone dropped the cells for the kinds this
        // native actor dispatches itself — `ReplaceComponent` among them, so
        // this very handler's cost went unmeasured on every replace.
        let mut handler_kinds: Vec<KindId> = <WasmTrampoline as Dispatch<Self>>::measured_kinds();
        let guest_kinds: Vec<KindId> =
            capabilities.handlers.iter().map(|h| h.id).filter(|id| !handler_kinds.contains(id)).collect();
        handler_kinds.extend(guest_kinds);
        let seeded = self.mailer.cost_table().seed(self.mailbox, &handler_kinds);
        CostCells::try_with_mut(|cells| cells.seed(seeded));

        ReplaceResult::Ok { capabilities }
    }
}

/// Every kind a module's `aether.kinds` section declares, by the id the
/// registry derives from its name and schema, so a reshaped kind reads as a
/// different kind.
fn declared_kinds(wasm: &[u8]) -> Result<HashSet<KindId>, String> {
    Ok(kind_manifest::read_from_bytes(wasm)?
        .iter()
        .map(|descriptor| KindId(kind_id_from_parts(&descriptor.name, &descriptor.schema)))
        .collect())
}

#[derive(Clone)]
pub(super) struct SiblingSpawnContext {
    parent: MailboxId,
    subname: String,
    capabilities: ComponentCapabilities,
}

#[derive(Clone)]
pub(super) struct InlineAliasContext {
    parent: MailboxId,
    alias: MailboxId,
}
