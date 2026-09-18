#![allow(clippy::needless_pass_by_value)]

use std::sync::Arc;

use aether_actor::Local as _;
use aether_actor::Single;
use aether_kinds::{ComponentCapabilities, ReplaceComponent, ReplaceResult};
use aether_substrate::actor::native::spawn::Subname;
use aether_substrate::actor::native::{
    Dispatch, NativeCtx, RegistryBatch, RegistryBatchResult, SpawnOutcome, TaskDone,
};
use aether_substrate::actor::wasm::asset_manifest;
use aether_substrate::actor::wasm::component::{Component, ComponentCtx, PendingSpawn};
use aether_substrate::actor::wasm::kind_manifest;
use aether_substrate::actor::wasm::kind_manifest::ActorInputs;
use aether_substrate::mail::registry::PreparedAliasRoute;
use aether_substrate::mail::{CostCells, KindId, MailboxId};
use wasmtime::Module;

use crate::LifecycleFlags;
use crate::trampoline::WasmTrampoline;

use super::config::WasmTrampolineConfig;
use super::state::WasmTrampolineState;

/// A candidate whose fallible construction and restoration have completed.
/// Dropping this value aborts without changing the resident component or
/// publishing guest effects.
struct PreparedReplacement {
    component: Component,
    module: Module,
    actor_caps: Vec<ActorInputs>,
    wasm_bytes: Arc<[u8]>,
    type_tag: Option<u64>,
    capabilities: ComponentCapabilities,
}

impl WasmTrampolineState {
    /// Publish the logical inline-child routes a guest call staged. The
    /// owner batch is reserved admission; completion is a later no-reply
    /// actor turn so rejection cannot silently lose the originating chain.
    pub fn stage_inline_aliases<A>(&self, ctx: &mut NativeCtx<'_, Single, A>, aliases: Vec<PreparedAliasRoute>) {
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
    pub fn stage_inline_alias_retirements<A>(&self, ctx: &mut NativeCtx<'_, Single, A>, aliases: Vec<MailboxId>) {
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
    pub fn spawn_sibling(&self, ctx: &mut NativeCtx<'_, Single, WasmTrampoline>, pending: PendingSpawn) {
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
            prohibit: LifecycleFlags::NONE,
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

    pub(super) fn finish_sibling_spawn(&self, done: TaskDone<SpawnOutcome, SiblingSpawnContext>) {
        match &done.output().result {
            Ok(()) => {
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

    /// ADR-0096: resolve the **effective tag** an export-targeted
    /// replace instantiates, paired with the capability group to
    /// advertise. `export = Some(ns)` names an exported actor type of
    /// the replacement module, hashed to its tag the same way the load
    /// path resolves `LoadComponent.export` (component.rs `handle_load`);
    /// an export the new module doesn't declare is a clean `Err`,
    /// mirroring the load "export not found" message. `export = None`
    /// reuses the type THIS trampoline currently hosts (`self.type_tag`)
    /// — the byte-for-byte legacy path: entry (first actor) when the tag
    /// is None, else the actor whose namespace hashes to the tag, with
    /// an `Err` if the new module doesn't export it. The returned tag
    /// drives both the reply capabilities and `Component::instantiate`,
    /// and on success the caller promotes it to the new `self.type_tag`
    /// so a later bare replace reuses the *current* hosted type rather
    /// than reverting to the original load's.
    pub fn resolve_replace_target(
        &self,
        export: Option<&str>,
        actors: &[ActorInputs],
    ) -> Result<(ComponentCapabilities, Option<u64>), String> {
        if let Some(requested) = export {
            let group = actors.iter().find(|a| a.namespace.as_deref() == Some(requested)).ok_or_else(|| {
                let available: Vec<&str> = actors.iter().filter_map(|a| a.namespace.as_deref()).collect();
                format!("export {requested:?} not found in module; exported types: {available:?}")
            })?;
            return Ok((
                group.capabilities.clone(),
                // Runtime-name routing: `requested` is the export
                // namespace from the wire replace request, resolved to
                // its actor-type tag exactly as the load path does.
                #[allow(clippy::disallowed_methods)]
                Some(aether_data::mailbox_id_from_name(requested).0),
            ));
        }
        // Bare replace (`export: None`): reuse the type this trampoline
        // currently hosts. With no tag yet (post-drop refill) that's the
        // entry actor — first in the export list — with a `None` tag.
        let Some(tag) = self.type_tag else {
            return Ok((actors.first().map(|a| a.capabilities.clone()).unwrap_or_default(), None));
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
            .map(|group| (group.capabilities.clone(), Some(tag)))
            .ok_or_else(|| {
                format!("replace: new module does not export the actor type (tag {tag:#x}) this trampoline loaded")
            })
    }

    fn prepare_replace(
        &mut self,
        ctx: &mut NativeCtx<'_>,
        payload: ReplaceComponent,
    ) -> Result<PreparedReplacement, String> {
        if self.prohibit.contains(LifecycleFlags::REPLACE) {
            return Err("component replacement prohibited by native bootstrap".to_owned());
        }

        // `payload.wasm` is the new module bytes; `mailbox_id` is
        // the trampoline's own id (the agent already addressed
        // this mail to us, so the field is informational).
        let _ = payload.mailbox_id;

        let module =
            Module::new(&self.engine, &payload.wasm).map_err(|error| format!("invalid wasm module: {error}"))?;

        // ADR-0033 / ADR-0096 / ADR-0097: parse every exported type's
        // capability group from the new wasm. The full `actors` set
        // refreshes `self.actor_caps` below so post-replace sibling
        // spawns see the new module's types.
        let actors = kind_manifest::read_actor_inputs_from_bytes(&payload.wasm)?;

        // ADR-0096: resolve the effective tag the replacement
        // instantiates plus the capability group to advertise —
        // export-named, or the trampoline's current hosted type for a
        // bare replace. See [`Self::resolve_replace_target`].
        let (mut capabilities, effective_tag) = self.resolve_replace_target(payload.export.as_deref(), &actors)?;

        // ADR-0163 §3 (#3984): re-index the replacement module's assets into
        // a load window. Its catalog feeds the post-swap
        // `describe_component` / `ReplaceResult`; the window itself is
        // installed on the new instance's ctx below so the replacement's
        // `init` can pull asset bytes (replace re-runs `init`, not `wire`,
        // so the window closes after instantiate). A malformed asset section
        // fails the replace loudly, before the swap runs.
        let new_wasm_bytes: Arc<[u8]> = Arc::from(payload.wasm.as_slice());
        let load_window = asset_manifest::LoadWindow::index(Arc::clone(&new_wasm_bytes))?;
        capabilities.assets = load_window.catalog();

        // The existing dehydration ABI is now a read-only, fallible prepare
        // step. Keep the predecessor resident and wired through all candidate
        // work; an error here or below leaves it usable at the same address.
        let saved = self
            .component
            .as_mut()
            .map(Component::on_dehydrate)
            .transpose()
            .map_err(|error| format!("on_dehydrate failed: {error}"))?
            .flatten();

        // Build a fresh `ComponentCtx` for the new instance — same
        // mailer + registry/outbound/input references, new
        // ReplyTable since wasm-side state resets. Mailbox id is
        // preserved across replace per ADR-0022 §4.
        let mut substrate_ctx = ComponentCtx::new(
            self.mailbox,
            Arc::clone(&self.registry),
            Arc::clone(&self.mailer),
            Arc::clone(&self.outbound),
        );
        substrate_ctx.install_binding(ctx.transport_arc());
        substrate_ctx.begin_replacement_preparation();
        // ADR-0163 §3 (#3984): install the load window before instantiate so
        // the replacement's `init` can pull assets; closed after instantiate
        // (replace re-runs `init`, not `wire`).
        substrate_ctx.install_load_window(load_window);

        // ADR-0090 (issue 1257): thread the replace mail's config
        // bytes into the new instance's typed `init`, the same way
        // the load path does. Empty means "no config"; a typed-config
        // guest decodes its `Self::Config` from these bytes.
        let mut new_component =
            Component::instantiate(&self.engine, &self.linker, &module, substrate_ctx, &payload.config, effective_tag)
                .map_err(|error| format!("wasm instantiation failed: {error}"))?;
        // ADR-0163 §3 (#3984): replace re-runs `init` but not `wire`, so the
        // load window's job ends once the replacement instantiated — close
        // it, retaining the catalog metadata for the instance's life.
        new_component.close_load_window();

        if let Some(bundle) = saved {
            new_component.call_on_rehydrate(&bundle).map_err(|error| format!("on_rehydrate failed: {error}"))?;
        }

        // Rehydrated inline children keep their existing published aliases.
        // New alias changes and detached births require fallible owner
        // admission that this handler cannot guarantee before its success
        // reply, so reject that topology before retiring the predecessor.
        if !new_component.drain_pending_spawns().is_empty() {
            return Err(
                "replacement candidate requested sibling births during preparation; topology admission is unsupported"
                    .to_owned(),
            );
        }
        if !new_component.drain_pending_aliases().is_empty()
            || !new_component.drain_pending_alias_retirements().is_empty()
        {
            return Err("replacement candidate changed inline-child aliases during preparation; topology admission is unsupported".to_owned());
        }

        Ok(PreparedReplacement {
            component: new_component,
            module,
            actor_caps: actors,
            wasm_bytes: new_wasm_bytes,
            type_tag: effective_tag,
            capabilities,
        })
    }

    fn commit_replace(&mut self, prepared: PreparedReplacement) -> ReplaceResult {
        let PreparedReplacement { component, module, actor_caps, wasm_bytes, type_tag, capabilities } = prepared;
        if let Some(mut old) = self.component.take() {
            // Acceptance is final. A contained teardown trap is diagnostic.
            old.unwire();
        }

        self.component = Some(component);
        self.module = module;
        self.actor_caps = actor_caps;
        self.wasm_bytes = wasm_bytes;
        self.type_tag = type_tag;

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

        self.component.as_mut().expect("replacement component installed").publish_prepared_effects();
        ReplaceResult::Ok { capabilities }
    }

    pub fn handle_replace(&mut self, ctx: &mut NativeCtx<'_>, payload: ReplaceComponent) -> ReplaceResult {
        match self.prepare_replace(ctx, payload) {
            Ok(prepared) => self.commit_replace(prepared),
            Err(error) => ReplaceResult::Err { error },
        }
    }
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
