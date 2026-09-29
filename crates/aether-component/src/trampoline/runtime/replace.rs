#![allow(clippy::needless_pass_by_value)]

use std::fmt::Display;

use aether_actor::Single;
use aether_data::ErasedActorPath;
use aether_kinds::{ComponentCapabilities, ReplaceComponent, ReplaceResult};
use aether_substrate::actor::native::{NativeCtx, RegistryBatch, RegistryBatchResult, TaskDone};
use aether_substrate::actor::wasm::kind_manifest::ActorInputs;
use aether_substrate::mail::registry::{PreparedAliasRetirement, PreparedAliasRoute};

use crate::component::{Prepared, replacement_refusal};
use crate::trampoline::WasmTrampoline;

use super::contract;
use super::republish::CandidateType;
use super::state::WasmTrampolineState;

impl WasmTrampolineState {
    /// Publish the logical inline-child routes a guest call staged. The
    /// owner batch is reserved admission; completion is a later no-reply
    /// actor turn so rejection cannot silently lose the originating chain.
    pub fn stage_inline_aliases<A>(ctx: &mut NativeCtx<'_, A, Single>, aliases: Vec<PreparedAliasRoute>) {
        for alias in aliases {
            let context = InlineAliasContext { alias: alias.rendered_name.to_string() };
            let _ = ctx.stage_registry_batch(RegistryBatch::publish_alias(alias), context);
        }
    }

    /// Retire the logical inline-child routes a guest call despawned (#4228),
    /// the teardown mirror of [`Self::stage_inline_aliases`]. Each alias closes
    /// here, from this actor's own turn: it tombstones (ADR-0241 §8), so its
    /// key is never spawned again, and fires its departure notices, so a cap
    /// keying rows on the child's stamped identity (ADR-0114 §4) reclaims them;
    /// the route retirement itself is staged through the owner alongside.
    pub fn stage_inline_alias_retirements<A>(
        ctx: &mut NativeCtx<'_, A, Single>,
        aliases: Vec<PreparedAliasRetirement>,
    ) {
        for alias in aliases {
            ctx.close_alias(&alias);
            let context = InlineAliasContext { alias: alias.rendered_name.to_string() };
            let _ = ctx.stage_registry_batch(RegistryBatch::retire_alias(alias), context);
        }
    }

    /// Log an alias batch the owner refused after staging. The batch owes no
    /// reply; its context names the alias it staged (ADR-0243 §9).
    pub(super) fn finish_inline_aliases<A>(ctx: &mut NativeCtx<'_, A, Single>, done: TaskDone<RegistryBatchResult>) {
        let Some(InlineAliasContext { alias }) = ctx.take_context() else {
            return;
        };
        if let Err(error) = done.into_output() {
            tracing::warn!(
                target: "aether_component",
                alias = %alias,
                "inline-child alias registry batch failed after owner staging: {error}",
            );
        }
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
                Some(aether_data::ActorId::singleton(requested).0),
            ));
        }
        // Bare replace (`export: None`): reuse the type this trampoline
        // currently hosts. With no tag (a load by the module's default) that's
        // the module's first actor, with a `None` tag. A module that declares
        // a boot never reaches here: the host refuses its replace before
        // forwarding (ADR-0147).
        let Some(tag) = self.type_tag else {
            return Ok((actors.first(), None));
        };
        actors
            .iter()
            .find(|a| {
                // Runtime-name match: compute each replacement actor's
                // declared namespace's actor-type identity to find the one
                // whose tag was loaded — not a hardcoded sibling.
                a.namespace.as_deref().is_some_and(|ns| aether_data::ActorId::singleton(ns).0 == tag)
            })
            .map(|group| (Some(group), Some(tag)))
            .ok_or_else(|| {
                format!("replace: new module does not export the actor type (tag {tag:#x}) this trampoline loaded")
            })
    }

    /// ADR-0230: the replacement's hosted type must find every declared
    /// dependency live, checked before anything is touched. `own` is this
    /// trampoline's canonical path, which names it in the refusal. A module
    /// that declares no group has nothing to refuse and passes.
    fn check_dependencies(
        ctx: &NativeCtx<'_, WasmTrampoline>,
        own: &ErasedActorPath,
        group: Option<&ActorInputs>,
    ) -> Result<(), String> {
        let Some(group) = group else {
            return Ok(());
        };
        replacement_refusal(ctx, own.as_str(), &group.dependencies).map_or(Ok(()), Err)
    }

    /// ADR-0231 §5: the replacement's hosted type must keep every handler
    /// row, and the `#[fallback]` if it has one, of the type this slot hosts
    /// now, and may add rows. The predecessor resolves from the resident module's
    /// manifest the way the replacement does, and a failure to resolve it
    /// refuses the replace.
    fn check_contract(
        &self,
        ctx: &NativeCtx<'_, WasmTrampoline>,
        target: &impl Display,
        replacement: &ComponentCapabilities,
    ) -> Result<(), String> {
        let resident = self.module.manifest();
        let (predecessor, _) = self.resolve_replace_target(None, resident.actors())?;
        let predecessor = predecessor.map(|group| group.capabilities.clone()).unwrap_or_default();
        contract::contract_break(&predecessor, replacement).map_or(Ok(()), |contract_break| {
            Err(contract::contract_refusal(target, contract_break, |kind| ctx.kind_label(kind)))
        })
    }

    pub fn handle_replace(
        &mut self,
        ctx: &mut NativeCtx<'_, WasmTrampoline>,
        payload: ReplaceComponent,
    ) -> ReplaceResult {
        // `wasm` is the new module bytes; `target` named this trampoline and
        // the host proved it before forwarding, so the field only names the
        // actor in a contract or carried-context refusal.
        let ReplaceComponent { target, wasm, config, export, .. } = payload;

        // ADR-0241 §2: the candidate comes from the engine's one module
        // cache, so its code compiles and its sections parse once per content
        // hash. The host checked the same bytes in before forwarding and
        // holds that module until this replace settles, so this is a cache
        // hit rather than a second compile. The code blob is let go once the
        // module is built.
        let module = match self.modules.check_in(&ctx.blob_check_in(), &ctx.check_in(wasm.into_boxed_slice())) {
            Ok(module) => module,
            Err(error) => return ReplaceResult::Err { error },
        };
        let manifest = module.manifest();

        // ADR-0096: resolve the effective tag the replacement
        // instantiates plus the capability group to advertise —
        // export-named, or the trampoline's current hosted type for a
        // bare replace. See [`Self::resolve_replace_target`].
        let (group, type_tag) = match self.resolve_replace_target(export.as_deref(), manifest.actors()) {
            Ok(resolved) => resolved,
            Err(error) => return ReplaceResult::Err { error },
        };

        // ADR-0230 and ADR-0231 §5: checked before anything is touched, so a
        // refusal leaves the old guest as it was.
        if let Err(error) = Self::check_dependencies(ctx, &ctx.path(), group) {
            return ReplaceResult::Err { error };
        }
        let mut capabilities = group.map(|group| group.capabilities.clone()).unwrap_or_default();
        if let Err(error) = self.check_contract(ctx, &target, &capabilities) {
            return ReplaceResult::Err { error };
        }

        // ADR-0163 §3 (#3984): the replacement's asset catalog feeds the
        // post-swap `describe_component` / `ReplaceResult`.
        capabilities.assets = manifest.asset_catalog().to_vec();

        // ADR-0241 §7: a single-instance replace prepares and, on `Ready`,
        // commits in the same turn, so the candidate's held mail leaves on
        // this replace's chain. An empty config means the stored one.
        let candidate = CandidateType { module, type_tag, capabilities: capabilities.clone() };
        let config = (!config.is_empty()).then_some(config);
        match self.prepare(ctx, &target, candidate, config) {
            Prepared::Ready => {
                self.commit(ctx);
                ReplaceResult::Ok { capabilities }
            }
            Prepared::Refused { error } => ReplaceResult::Err { error },
        }
    }
}

/// The context an inline alias batch carries into its task completion: the
/// alias's rendered name, for the refusal's log line.
#[aether_data::kind(name = "aether.component.trampoline.inline_alias", no_serde)]
pub(super) struct InlineAliasContext {
    alias: String,
}
