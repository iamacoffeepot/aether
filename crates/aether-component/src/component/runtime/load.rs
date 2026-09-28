//! Owner-staged component load, module-boot, and replacement stages, and the
//! deferred replies they carry from one stage to the next.

use std::sync::Arc;

use aether_actor::{ErasedActorRef, Manual, OutboundReply, ReplyMode, Single};
use aether_data::{BlobHash, ErasedActorPath, Kind, Source};
use aether_kinds::{ComponentCapabilities, LoadComponent, LoadComponentUnder, ReplaceComponent, ReplaceResult};

use aether_substrate::actor::native::{
    DeferredReply, IntoDeferredReply, NativeCtx, RegistryBatch, RegistryBatchResult, SpawnOutcome, TaskDone,
    spawn::Subname,
};
use aether_substrate::actor::wasm::kind_manifest::Dependency;
use aether_substrate::actor::wasm::module::Module;

use super::LoadResult;
use super::dependencies::{dependency_refusal, inline_dependency_refusal};
use super::placement::root_refusal;
use crate::component::runtime::{BootEntry, ComponentDrop, ComponentHostCapabilityState, PendingReplace};
use crate::component::{ComponentHostCapability, LoadDelivered};
use crate::kinds::BootTeardown;
use crate::trampoline::{WasmTrampoline, WasmTrampolineConfig};

pub(super) struct PreparedLoad {
    capabilities: ComponentCapabilities,
    dependencies: Vec<Dependency>,
    type_tag: Option<u64>,
    /// The checked-in module. Its hash is, for a module that declares a boot
    /// slot, the boot registry's key.
    module: Module,
    config: Vec<u8>,
    name: String,
    placement: LoadPlacement,
}

#[derive(Clone)]
enum LoadPlacement {
    ComponentHost,
    Under { parent: ErasedActorRef },
}

impl PreparedLoad {
    fn requested_config(&self, state: &ComponentHostCapabilityState) -> WasmTrampolineConfig {
        WasmTrampolineConfig {
            engine: Arc::clone(&state.engine),
            linker: Arc::clone(&state.linker),
            module: self.module.clone(),
            modules: state.modules.clone(),
            outbound: Arc::clone(&state.outbound),
            capabilities: self.capabilities.clone(),
            config: self.config.clone(),
            type_tag: self.type_tag,
        }
    }
}

#[derive(Clone)]
pub(super) struct PreparedBoot {
    namespace: String,
    capabilities: ComponentCapabilities,
    dependencies: Vec<Dependency>,
    module: Module,
}

impl PreparedBoot {
    /// The boot plan of `module`, or `None` when it declares no boot slot.
    /// Reads the boot namespace and its group from the parsed manifest.
    fn of(module: &Module) -> Option<Self> {
        let manifest = module.manifest();
        let namespace = manifest.boot()?;
        let group = manifest.actors().iter().find(|actor| actor.namespace.as_deref() == Some(namespace));
        Some(Self {
            namespace: namespace.to_owned(),
            capabilities: group.map(|actor| actor.capabilities.clone()).unwrap_or_default(),
            dependencies: group.map(|actor| actor.dependencies.clone()).unwrap_or_default(),
            module: module.clone(),
        })
    }

    /// The module's content hash: the boot registry's key.
    fn hash(&self) -> BlobHash {
        self.module.hash()
    }

    fn config(&self, state: &ComponentHostCapabilityState) -> WasmTrampolineConfig {
        WasmTrampolineConfig {
            engine: Arc::clone(&state.engine),
            linker: Arc::clone(&state.linker),
            module: self.module.clone(),
            modules: state.modules.clone(),
            outbound: Arc::clone(&state.outbound),
            capabilities: self.capabilities.clone(),
            config: Vec::new(),
            type_tag: Some(aether_data::ActorId::singleton(&self.namespace).0),
        }
    }
}

/// A module publish (ADR-0241 §3) staged through the registry owner, and what
/// continues once admission accepts the module and its kinds register. One
/// context for both, because task completions route by output type.
pub(super) enum ModulePublication {
    /// A load: the prepared load spawns once the publish commits.
    Load(Arc<PreparedLoad>),
    /// A replace: forwarded to its trampoline once the publish commits.
    Replace(ReplacePublication),
}

/// A replace whose replacement module publish is staged through the registry
/// owner. Nothing is forwarded to the trampoline until the publish commits;
/// a refusal answers the original `source` instead.
#[derive(Clone)]
pub(super) struct ReplacePublication {
    source: Source,
    actor: ErasedActorRef,
    module: Module,
    /// The encoded `ReplaceComponent` the trampoline is forwarded.
    bytes: Arc<[u8]>,
}

#[derive(Clone)]
pub(super) enum BootSuccessor {
    Load(Arc<PreparedLoad>),
    Replacement { pending: PendingReplace, result: ReplaceResult },
}

struct BootWaiter {
    owed: DeferredReply,
    successor: BootSuccessor,
}

pub(super) struct PendingBoot {
    waiters: Vec<BootWaiter>,
}

impl PendingBoot {
    fn new() -> Self {
        Self { waiters: Vec::new() }
    }
}

impl Drop for PendingBoot {
    fn drop(&mut self) {
        for waiter in self.waiters.drain(..) {
            waiter.owed.abandon_for_actor_close();
        }
    }
}

/// The multi-step load plan a staged trampoline birth carries into its
/// authoritative completion. Unlike the caps whose completion context was only
/// an id, this names *which stage of the load pipeline* the birth belongs to —
/// the module-boot leg or the requested-actor leg — plus the prepared inputs
/// that leg still needs. The identity the birth reports rides
/// [`SpawnOutcome`] instead.
#[derive(Clone)]
pub(super) enum SpawnContext {
    ModuleBoot { plan: Box<PreparedBoot>, first: Box<BootSuccessor> },
    RequestedActor { load: Arc<PreparedLoad>, boot_hash: Option<BlobHash> },
}

impl ComponentHostCapabilityState {
    pub fn begin_load<A>(&mut self, ctx: &mut NativeCtx<'_, A, Manual>, payload: LoadComponent) {
        self.begin_load_at(ctx, payload, LoadPlacement::ComponentHost);
    }

    pub fn begin_load_under<A>(&mut self, ctx: &mut NativeCtx<'_, A, Manual>, payload: LoadComponentUnder) {
        // ADR-0230 §1: the parent must be `Live`. A `Starting` parent resolves
        // as an address but does not prove, so a child is never staged beneath
        // an unborn parent; the proof carries the parent's own canonical path.
        let resolved = ErasedActorPath::new(&payload.parent)
            .map_err(|error| error.to_string())
            .and_then(|parent| ctx.resolve_path(&parent).map_err(|error| error.to_string()));
        let parent = match resolved {
            Ok(parent) => parent,
            Err(error) => {
                ctx.reply(&LoadResult::Err {
                    error: format!("component parent {:?} did not resolve: {error}", payload.parent),
                });
                return;
            }
        };
        self.begin_load_at(ctx, payload.load, LoadPlacement::Under { parent });
    }

    fn begin_load_at<A>(
        &mut self,
        ctx: &mut NativeCtx<'_, A, Manual>,
        payload: LoadComponent,
        placement: LoadPlacement,
    ) {
        let load = match self.prepare_load(ctx, payload, placement) {
            Ok(load) => load,
            Err(result) => {
                ctx.reply(&result);
                return;
            }
        };
        // ADR-0241 §3/§4: publish the module before anything spawns. The
        // owner runs admission and registers the module's kinds in one batch.
        let _ = ctx.stage_registry_batch(RegistryBatch::publish_module(&load.module), ModulePublication::Load(load));
    }

    #[allow(
        clippy::result_large_err,
        reason = "cold synchronous preparation returns the exact public LoadResult error shape"
    )]
    fn prepare_load<A, M: ReplyMode>(
        &mut self,
        ctx: &NativeCtx<'_, A, M>,
        payload: LoadComponent,
        placement: LoadPlacement,
    ) -> Result<Arc<PreparedLoad>, LoadResult> {
        let LoadComponent { wasm, name, config, export } = payload;

        // ADR-0241 §2: check the bytes in and take the module from the
        // engine's one cache, which compiles and parses them once per content
        // hash. The code blob is let go once the module is built.
        let module = self
            .modules
            .check_in(&ctx.blob_check_in(), &ctx.check_in(wasm.into_boxed_slice()))
            .map_err(|error| LoadResult::Err { error })?;
        let manifest = module.manifest();
        let actors = manifest.actors();

        // ADR-0230 §3: an actor the module can spawn inline runs before the
        // host sees it, so its declared dependencies are checked here, before
        // the module publishes, the module boot actor, or the requested actor.
        if let Some(error) = inline_dependency_refusal(ctx, manifest) {
            return Err(LoadResult::Err { error });
        }

        // ADR-0241 §5: a module boot is always host-placed, whatever the
        // requested placement, so its type must declare `root`.
        if let Some(error) = manifest.boot().and_then(|boot_ns| root_refusal(manifest.lineage(), boot_ns)) {
            return Err(LoadResult::Err { error });
        }

        if let Some(boot_ns) = manifest.boot()
            && export.as_deref() == Some(boot_ns)
        {
            return Err(LoadResult::Err {
                error: format!("export {boot_ns:?} names this module's boot actor, which is not selectable (ADR-0147)"),
            });
        }

        // A single-actor module (`export!(public = [A])`) carries no actor boundaries, so
        // its one implicit group is unnamed; its sole export is still nameable
        // by the namespace its `aether.namespace` section declares, and it is
        // instantiated exactly as the unselected default load would be.
        let sole_export = actors.iter().all(|actor| actor.namespace.is_none())
            && export.is_some()
            && manifest.namespace() == export.as_deref();

        let (mut capabilities, dependencies, type_tag, selected_namespace) = if sole_export {
            let sole = actors.first();
            (
                sole.map(|actor| actor.capabilities.clone()).unwrap_or_default(),
                sole.map(|actor| actor.dependencies.clone()).unwrap_or_default(),
                None,
                export,
            )
        } else if let Some(requested) = &export {
            let Some(group) = actors.iter().find(|actor| actor.namespace.as_deref() == Some(requested.as_str())) else {
                let available: Vec<&str> = actors.iter().filter_map(|actor| actor.namespace.as_deref()).collect();
                return Err(LoadResult::Err {
                    error: format!("export {requested:?} not found in module; exported types: {available:?}"),
                });
            };
            let tag = aether_data::ActorId::singleton(requested).0;
            (group.capabilities.clone(), group.dependencies.clone(), Some(tag), Some(requested.clone()))
        } else if manifest.no_default() {
            let available: Vec<&str> = actors.iter().filter_map(|actor| actor.namespace.as_deref()).collect();
            return Err(LoadResult::Err {
                error: format!(
                    "module has no default (ADR-0138): load one of its exports by name via the export selector; exported types: {available:?}"
                ),
            });
        } else {
            let default_actor = manifest.boot().map_or_else(
                || actors.first(),
                |boot_ns| actors.iter().find(|actor| actor.namespace.as_deref() != Some(boot_ns)),
            );
            (
                default_actor.map(|actor| actor.capabilities.clone()).unwrap_or_default(),
                default_actor.map(|actor| actor.dependencies.clone()).unwrap_or_default(),
                None,
                default_actor.and_then(|actor| actor.namespace.clone()),
            )
        };

        // ADR-0241 §5: a host load places the selected type at the component
        // host, so it must declare `root`; a single-actor module's implicit
        // group is named by the module's namespace. `load_under` places
        // beneath a parent and is not checked here.
        if matches!(placement, LoadPlacement::ComponentHost) {
            let Some(namespace) = selected_namespace.as_deref().or_else(|| manifest.namespace()) else {
                return Err(LoadResult::Err {
                    error: "the load selects no actor namespace, so its host placement cannot be checked".to_owned(),
                });
            };
            if let Some(error) = root_refusal(manifest.lineage(), namespace) {
                return Err(LoadResult::Err { error });
            }
        }

        capabilities.assets = manifest.asset_catalog().to_vec();
        let name =
            name.or(selected_namespace).or_else(|| manifest.namespace().map(str::to_owned)).unwrap_or_else(|| {
                let counter = self.default_name_counter;
                self.default_name_counter += 1;
                format!("component_{counter}")
            });

        Ok(Arc::new(PreparedLoad { capabilities, dependencies, type_tag, module, config, name, placement }))
    }

    /// Continue a load or a replace once its module publish settles. A
    /// refusal (admission or a kind conflict) answers the caller: a load
    /// spawns nothing, and a replace is never forwarded.
    pub(super) fn finish_publish(
        &mut self,
        ctx: &mut NativeCtx<'_, ComponentHostCapability, Single>,
        done: TaskDone<RegistryBatchResult, ModulePublication>,
    ) {
        let refusal = done.output().as_ref().err().map(|error| format!("module publish refused: {error}"));
        match (done.context(), refusal) {
            (ModulePublication::Load(_), Some(error)) => done.resolve_with(ctx, move |_, _| LoadResult::Err { error }),
            (ModulePublication::Replace(_), Some(error)) => {
                done.resolve_with(ctx, move |_, _| ReplaceResult::Err { error });
            }
            (ModulePublication::Load(load), None) => {
                let load = Arc::clone(load);
                self.continue_load(ctx, done.into_deferred_reply(), load);
            }
            (ModulePublication::Replace(replace), None) => {
                let replace = replace.clone();
                self.forward_replace(ctx, done, replace);
            }
        }
    }

    fn continue_load(
        &mut self,
        ctx: &mut NativeCtx<'_, ComponentHostCapability, Single>,
        owed: DeferredReply,
        load: Arc<PreparedLoad>,
    ) {
        let Some(plan) = PreparedBoot::of(&load.module) else {
            self.stage_requested_actor(ctx, owed, load, None);
            return;
        };
        let hash = plan.hash();
        if self.boot_registry.contains_key(&hash) {
            self.stage_requested_actor(ctx, owed, load, Some(hash));
        } else if let Some(pending) = self.pending_boots.get_mut(&hash) {
            pending.waiters.push(BootWaiter { owed, successor: BootSuccessor::Load(load) });
        } else {
            self.stage_module_boot(ctx, owed, plan, BootSuccessor::Load(load));
        }
    }

    fn stage_module_boot<M: ReplyMode>(
        &mut self,
        ctx: &mut NativeCtx<'_, ComponentHostCapability, M>,
        owed: DeferredReply,
        plan: PreparedBoot,
        first: BootSuccessor,
    ) {
        if let Some(namespace) = ctx.missing_child_dependency(&plan.dependencies) {
            let error = dependency_refusal(&plan.namespace, namespace);
            match first {
                BootSuccessor::Load(_) => {
                    owed.reply(ctx, &LoadResult::Err { error });
                }
                BootSuccessor::Replacement { pending, result } => {
                    tracing::warn!(
                        target: "aether_component",
                        actor = %ctx.actor_path(pending.actor),
                        %error,
                        "replace succeeded but the replacement module boot failed",
                    );
                    owed.reply(ctx, &result);
                }
            }
            return;
        }
        let hash = plan.hash();
        let namespace = plan.namespace.clone();
        let config = plan.config(self);
        match ctx
            .spawn_child::<WasmTrampoline>(Subname::Named(&namespace), config, ())
            .continue_from(owed, SpawnContext::ModuleBoot { plan: Box::new(plan), first: Box::new(first.clone()) })
        {
            Ok(_) => {
                let previous = self.pending_boots.insert(hash, PendingBoot::new());
                debug_assert!(previous.is_none(), "one actor-local reservation owns a module boot hash");
            }
            Err((error, owed)) => {
                Self::reply_boot_failure(ctx, owed, first, format!("boot trampoline spawn failed: {error:?}"));
            }
        }
    }

    fn stage_requested_actor(
        &mut self,
        ctx: &mut NativeCtx<'_, ComponentHostCapability, Single>,
        owed: DeferredReply,
        load: Arc<PreparedLoad>,
        boot_hash: Option<BlobHash>,
    ) {
        let missing = match &load.placement {
            LoadPlacement::ComponentHost => ctx.missing_child_dependency(&load.dependencies),
            LoadPlacement::Under { parent } => ctx.missing_dependency(Some(*parent), &load.dependencies),
        };
        if let Some(namespace) = missing {
            owed.reply(ctx, &LoadResult::Err { error: dependency_refusal(&load.name, namespace) });
            return;
        }
        let config = load.requested_config(self);
        let context = SpawnContext::RequestedActor { load: Arc::clone(&load), boot_hash };
        let placement = load.placement.clone();
        let staged = match placement {
            LoadPlacement::ComponentHost => {
                ctx.spawn_child::<WasmTrampoline>(Subname::Named(&load.name), config, ()).continue_from(owed, context)
            }
            LoadPlacement::Under { parent } => ctx
                .spawn_child_scoped::<WasmTrampoline>(parent, Subname::Named(&load.name), config, ())
                .continue_from(owed, context),
        };
        match staged {
            Ok(_) => {
                if let Some(hash) = boot_hash {
                    let entry =
                        self.boot_registry.get_mut(&hash).expect("requested actor starts only after its boot is Live");
                    entry.pending_requests = entry
                        .pending_requests
                        .checked_add(1)
                        .expect("module boot pending-request count cannot overflow");
                }
            }
            Err((error, owed)) => {
                owed.reply(ctx, &LoadResult::Err { error: format!("trampoline spawn failed: {error:?}") });
            }
        }
    }

    pub(super) fn finish_spawn(
        &mut self,
        ctx: &mut NativeCtx<'_, ComponentHostCapability, Single>,
        done: TaskDone<SpawnOutcome<WasmTrampoline>, SpawnContext>,
    ) {
        match done.context().clone() {
            SpawnContext::ModuleBoot { plan, first } => self.finish_module_boot(ctx, done, *plan, *first),
            SpawnContext::RequestedActor { load, boot_hash } => {
                self.finish_requested_actor(ctx, done, load, boot_hash);
            }
        }
    }

    fn finish_module_boot(
        &mut self,
        ctx: &mut NativeCtx<'_, ComponentHostCapability, Single>,
        done: TaskDone<SpawnOutcome<WasmTrampoline>, SpawnContext>,
        plan: PreparedBoot,
        first: BootSuccessor,
    ) {
        let outcome = done.output();
        let booted = outcome.result.as_ref().copied().map_err(|error| format!("{error:?}"));
        let hash = plan.hash();
        let mut pending = self.pending_boots.remove(&hash).expect("module boot retains its actor-local reservation");
        match booted {
            Ok(boot) => {
                self.register_boot(hash, BootEntry { boot, refcount: 0, pending_requests: 0 });
                self.finish_boot_successor(ctx, done.into_deferred_reply(), first, hash);
                for waiter in pending.waiters.drain(..) {
                    self.finish_boot_successor(ctx, waiter.owed, waiter.successor, hash);
                }
                self.drop_orphan_boot(ctx, hash);
            }
            Err(error) => {
                Self::reply_boot_failure(ctx, done.into_deferred_reply(), first, error.clone());
                for waiter in pending.waiters.drain(..) {
                    Self::reply_boot_failure(ctx, waiter.owed, waiter.successor, error.clone());
                }
            }
        }
    }

    fn finish_boot_successor(
        &mut self,
        ctx: &mut NativeCtx<'_, ComponentHostCapability, Single>,
        owed: DeferredReply,
        successor: BootSuccessor,
        hash: BlobHash,
    ) {
        match successor {
            BootSuccessor::Load(load) => {
                self.stage_requested_actor(ctx, owed, load, Some(hash));
            }
            BootSuccessor::Replacement { pending, result } => {
                self.commit_replacement_boot(ctx, pending.actor, pending.boot_operation, Some(hash));
                owed.reply(ctx, &result);
            }
        }
    }

    fn reply_boot_failure<M: ReplyMode, A>(
        ctx: &mut NativeCtx<'_, A, M>,
        owed: DeferredReply,
        successor: BootSuccessor,
        error: String,
    ) {
        match successor {
            BootSuccessor::Load(_) => {
                owed.reply(
                    ctx,
                    &LoadResult::Err { error: format!("module boot failed before requested actor: {error}") },
                );
            }
            BootSuccessor::Replacement { pending, result } => {
                tracing::warn!(
                    target: "aether_component",
                    actor = %ctx.actor_path(pending.actor),
                    %error,
                    "replace succeeded but the replacement module boot failed",
                );
                owed.reply(ctx, &result);
            }
        }
    }

    fn finish_requested_actor(
        &mut self,
        ctx: &mut NativeCtx<'_, ComponentHostCapability, Single>,
        done: TaskDone<SpawnOutcome<WasmTrampoline>, SpawnContext>,
        load: Arc<PreparedLoad>,
        boot_hash: Option<BlobHash>,
    ) {
        let child = match &done.output().result {
            Ok(child) => *child,
            Err(error) => {
                let error = format!("trampoline spawn failed: {error:?}");
                if let Some(hash) = boot_hash {
                    self.settle_boot_request(ctx, hash, None);
                }
                done.resolve_with(ctx, move |_, _| LoadResult::Err { error });
                return;
            }
        };

        self.drop_targets.insert(child.erase(), child.narrow::<ComponentDrop>());
        if let Some(hash) = boot_hash {
            self.settle_boot_request(ctx, hash, Some(child.erase()));
        }
        // ADR-0230 §3: the loaded trampoline answers the requester itself, so
        // the reply's stamped sender is the reference the requester keeps; the
        // host hands it the owed reply rather than replying.
        let path = done.output().canonical_name.clone();
        let capabilities = load.capabilities.clone();
        done.hand_off(ctx, &child, &LoadDelivered { path, capabilities });
    }

    /// Record a module's Live boot under its content hash, indexing its
    /// reference for the drop guard.
    fn register_boot(&mut self, hash: BlobHash, entry: BootEntry) {
        self.boot_actors.insert(entry.boot.erase());
        self.boot_registry.insert(hash, entry);
    }

    /// Remove a module's boot and its reference index together, handing back
    /// the entry the teardown sends through.
    fn unregister_boot(&mut self, hash: BlobHash) -> Option<BootEntry> {
        let entry = self.boot_registry.remove(&hash)?;
        self.boot_actors.remove(&entry.boot.erase());
        Some(entry)
    }

    fn drop_orphan_boot<M: ReplyMode, A>(&mut self, ctx: &mut NativeCtx<'_, A, M>, hash: BlobHash) {
        let removable =
            self.boot_registry.get(&hash).is_some_and(|entry| entry.refcount == 0 && entry.pending_requests == 0);
        if removable {
            let entry = self.unregister_boot(hash).expect("orphan boot remains present");
            ctx.send_detached_to(entry.boot, &BootTeardown {});
        }
    }

    fn settle_boot_request<M: ReplyMode, A>(
        &mut self,
        ctx: &mut NativeCtx<'_, A, M>,
        hash: BlobHash,
        live_actor: Option<ErasedActorRef>,
    ) {
        let entry = self.boot_registry.get_mut(&hash).expect("requested actor's Live boot remains registered");
        entry.pending_requests = entry
            .pending_requests
            .checked_sub(1)
            .expect("each accepted requested actor settles its boot pending count exactly once");
        if let Some(actor) = live_actor {
            entry.refcount = entry.refcount.checked_add(1).expect("module boot reference count cannot overflow");
            self.boot_hash_by_actor.insert(actor, hash);
        }
        self.drop_orphan_boot(ctx, hash);
    }

    pub fn release_boot_ref<M: ReplyMode, A>(&mut self, ctx: &mut NativeCtx<'_, A, M>, actor: ErasedActorRef) {
        let Some(hash) = self.boot_hash_by_actor.remove(&actor) else {
            return;
        };
        let remove = if let Some(entry) = self.boot_registry.get_mut(&hash) {
            entry.refcount = entry
                .refcount
                .checked_sub(1)
                .expect("each boot-bearing actor releases its module boot reference exactly once");
            entry.refcount == 0 && entry.pending_requests == 0
        } else {
            false
        };
        if remove {
            let entry = self.unregister_boot(hash).expect("zero-ref boot remains present");
            ctx.send_detached_to(entry.boot, &BootTeardown {});
        }
    }

    pub fn begin_replace<A>(&mut self, ctx: &mut NativeCtx<'_, A>, payload: ReplaceComponent) {
        let source = ctx.reply_target();
        // ADR-0230: prove the target address at receipt. A dropped
        // trampoline keeps its `Live` route (vacate, not close), so a replace
        // that refills it still proves; an address with no live route answers
        // `Err` here instead of parking a forward nothing will answer.
        let actor = match ctx.resolve_path(&payload.target) {
            Ok(proven) => proven,
            Err(error) => {
                let error = format!("no component to replace at {}: {error}", payload.target);
                ctx.defer_reply_to(source).reply(ctx, &ReplaceResult::Err { error });
                return;
            }
        };
        let bytes = Arc::from(payload.encode_into_bytes());

        // ADR-0241 §2: the replacement module comes from the engine's one
        // cache, checked in before forwarding, so its sections parse once and
        // bytes that do not check in answer here, with the error the
        // trampoline would give. `ReplacePublication` and then
        // `PendingReplace` hold the module across the hops, so the
        // trampoline's own check-in of the forwarded bytes is a cache hit.
        let module = match self.modules.check_in(&ctx.blob_check_in(), &ctx.check_in(payload.wasm.into_boxed_slice())) {
            Ok(module) => module,
            Err(error) => {
                ctx.defer_reply_to(source).reply(ctx, &ReplaceResult::Err { error });
                return;
            }
        };
        // A replacement installs a module whose inline children are rebuilt
        // on rehydrate, so it is a module load for the ADR-0230 §3 check too.
        // The module-wide inline check runs here; the trampoline checks the
        // dependencies of the type the replacement will host.
        if let Some(error) = inline_dependency_refusal(ctx, module.manifest()) {
            ctx.defer_reply_to(source).reply(ctx, &ReplaceResult::Err { error });
            return;
        }
        // ADR-0241 §5: the replacement's module boot is host-placed, so a
        // boot type without `root` refuses the whole replace here, before
        // anything is staged, rather than failing the boot after the swap.
        let manifest = module.manifest();
        if let Some(error) = manifest.boot().and_then(|boot_ns| root_refusal(manifest.lineage(), boot_ns)) {
            ctx.defer_reply_to(source).reply(ctx, &ReplaceResult::Err { error });
            return;
        }
        // ADR-0241 §4: a replace republishes its module, so admission runs
        // and the replacement's kinds register before the trampoline sees it.
        let batch = RegistryBatch::publish_module(&module);
        let _ = ctx.stage_registry_batch(
            batch,
            ModulePublication::Replace(ReplacePublication { source, actor, module, bytes }),
        );
    }

    /// Forward a replace to its trampoline once the replacement module's
    /// publish commits, under the caller's chain. The forward's
    /// `ReplaceResult` comes back to [`Self::finish_replace`].
    fn forward_replace(
        &mut self,
        ctx: &mut NativeCtx<'_, ComponentHostCapability, Single>,
        done: TaskDone<RegistryBatchResult, ModulePublication>,
        replace: ReplacePublication,
    ) {
        let ReplacePublication { source, actor, module, bytes } = replace;
        let boot_operation = self.next_boot_operation(actor);
        match done.forward_tracked(ctx, actor, ReplaceComponent::ID, &bytes) {
            Ok(mail_id) => {
                self.pending_replace
                    .insert(mail_id.correlation_id, PendingReplace { source, actor, module, boot_operation });
            }
            Err(done) => {
                let error = "the replace request was refused as engine-only mail".to_owned();
                done.resolve_with(ctx, move |_, _| ReplaceResult::Err { error });
            }
        }
    }

    pub fn finish_replace(&mut self, ctx: &mut NativeCtx<'_, ComponentHostCapability, Manual>, result: ReplaceResult) {
        let Some(correlation) = ctx.in_reply_to().map(|request| request.0) else {
            return;
        };
        let Some(pending) = self.pending_replace.remove(&correlation) else {
            return;
        };
        if !matches!(result, ReplaceResult::Ok { .. }) {
            ctx.reply_to(pending.source, &result);
            return;
        }
        if !self.accept_successful_boot_operation(pending.actor, pending.boot_operation) {
            ctx.reply_to(pending.source, &result);
            return;
        }

        let plan = PreparedBoot::of(&pending.module);
        let new_hash = plan.as_ref().map(PreparedBoot::hash);
        if self.boot_hash_by_actor.get(&pending.actor) == new_hash.as_ref() {
            ctx.reply_to(pending.source, &result);
            return;
        }
        let Some(plan) = plan else {
            self.commit_replacement_boot(ctx, pending.actor, pending.boot_operation, None);
            ctx.reply_to(pending.source, &result);
            return;
        };
        if self.boot_registry.contains_key(&plan.hash()) {
            self.commit_replacement_boot(ctx, pending.actor, pending.boot_operation, Some(plan.hash()));
            ctx.reply_to(pending.source, &result);
            return;
        }

        let owed = ctx.defer_reply_to(pending.source);
        if let Some(inflight) = self.pending_boots.get_mut(&plan.hash()) {
            inflight.waiters.push(BootWaiter { owed, successor: BootSuccessor::Replacement { pending, result } });
        } else {
            self.stage_module_boot(ctx, owed, plan, BootSuccessor::Replacement { pending, result });
        }
    }

    fn commit_replacement_boot<M: ReplyMode, A>(
        &mut self,
        ctx: &mut NativeCtx<'_, A, M>,
        actor: ErasedActorRef,
        boot_operation: u64,
        new_hash: Option<BlobHash>,
    ) {
        if self.dominant_boot_operation_by_actor.get(&actor) != Some(&boot_operation) {
            return;
        }
        self.release_boot_ref(ctx, actor);
        if let Some(hash) = new_hash {
            let entry =
                self.boot_registry.get_mut(&hash).expect("replacement boot is Live before its reference commits");
            entry.refcount = entry.refcount.checked_add(1).expect("module boot reference count cannot overflow");
            self.boot_hash_by_actor.insert(actor, hash);
        }
    }

    fn next_boot_operation(&mut self, actor: ErasedActorRef) -> u64 {
        let sequence = self.boot_operation_sequence_by_actor.entry(actor).or_default();
        *sequence = sequence.checked_add(1).expect("an actor's boot-operation sequence cannot overflow");
        *sequence
    }

    fn accept_successful_boot_operation(&mut self, actor: ErasedActorRef, boot_operation: u64) -> bool {
        let dominant = self.dominant_boot_operation_by_actor.entry(actor).or_default();
        if boot_operation < *dominant {
            return false;
        }
        *dominant = boot_operation;
        true
    }

    pub(super) fn invalidate_replacement_boot_operation(&mut self, actor: ErasedActorRef) {
        let boot_operation = self.next_boot_operation(actor);
        self.dominant_boot_operation_by_actor.insert(actor, boot_operation);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use aether_data::Source;
    use aether_substrate::actor::native::NativeBinding;
    use aether_substrate::chassis::builder::PassiveChassis;
    use aether_substrate::mail::mailer::Mailer;
    use aether_substrate::mail::outbound::HubOutbound;
    use aether_substrate::mail::registry::{Registry, noop_handler};
    use aether_substrate::testing::{TestChassis, boot_test_chassis_with, registered_ref, unrouted_binding};
    use wasmtime::{Engine, Linker};

    use aether_substrate::actor::wasm::module::ModuleCache;

    use super::*;

    /// A host state beside the registry its tests register into and a binding
    /// over the mailer that routes through that registry.
    fn fixture() -> (ComponentHostCapabilityState, Arc<Registry>, Arc<NativeBinding>, PassiveChassis<TestChassis>) {
        let registry = Arc::new(Registry::new());
        let (outbound, _events) = HubOutbound::attached_loopback();
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry)).with_outbound(Arc::clone(&outbound)));
        let engine = Arc::new(Engine::default());
        let linker = Arc::new(Linker::new(&engine));
        let chassis = boot_test_chassis_with::<ComponentHostCapability>(
            &registry,
            &mailer,
            (),
            super::super::ComponentHostParams {
                engine: Arc::clone(&engine),
                linker: Arc::clone(&linker),
                hub_outbound: Arc::clone(&outbound),
            },
        );
        let state = ComponentHostCapabilityState {
            linker,
            modules: ModuleCache::new(Arc::clone(&engine)),
            engine,
            outbound,
            registry_subscription: None,
            last_egressed_inventory: None,
            default_name_counter: 0,
            boot_registry: HashMap::new(),
            boot_actors: HashSet::new(),
            pending_boots: HashMap::new(),
            boot_hash_by_actor: HashMap::new(),
            pending_replace: HashMap::new(),
            drop_targets: HashMap::new(),
            boot_operation_sequence_by_actor: HashMap::new(),
            dominant_boot_operation_by_actor: HashMap::new(),
        };
        let binding = unrouted_binding(&mailer);
        (state, registry, binding, chassis)
    }

    /// Register a test-local inbox under `name` and take its reference from
    /// the test-support `registered_ref`, proven by the same registry read a
    /// drop or replace receipt takes.
    fn proven_actor(registry: &Registry, name: &str) -> ErasedActorRef {
        registered_ref(registry, name, noop_handler())
    }

    /// A boot entry over a test-local inbox registered under `name`, proven
    /// like the boot spawn outcome's reference.
    fn boot_entry(
        chassis: &PassiveChassis<TestChassis>,
        registry: &Registry,
        name: &str,
        refcount: u32,
        pending_requests: u32,
    ) -> BootEntry {
        let route = proven_actor(registry, &format!("aether.embedded:{name}"));
        let boot = chassis.adopt_load::<WasmTrampoline>(route).expect("the live trampoline route is adopted");
        BootEntry { boot, refcount, pending_requests }
    }

    #[test]
    fn manual_interleaving_last_live_drop_then_pending_rejection_drops_boot() {
        let (mut state, registry, binding, chassis) = fixture();
        let hash = BlobHash::from_bytes([1; 32]);
        let mut ctx = NativeCtx::new(&binding, Source::NONE, None, None);
        let live_actor = proven_actor(&registry, "test.component.live-actor");
        let boot = boot_entry(&chassis, &registry, "boot-pending", 1, 1);
        state.register_boot(hash, boot);
        state.boot_hash_by_actor.insert(live_actor, hash);

        // Manual state-machine proof: the last Live actor drops while another
        // requested actor is still pending, then that pending birth rejects.
        // This does not assert that a scheduler will choose this ordering.
        state.release_boot_ref(&mut ctx, live_actor);
        assert_eq!(state.boot_registry.get(&hash).map(|entry| (entry.refcount, entry.pending_requests)), Some((0, 1)));
        state.settle_boot_request(&mut ctx, hash, None);

        assert!(!state.boot_registry.contains_key(&hash), "zero-ref/zero-pending boot must be removed after rejection");
    }

    #[test]
    fn manual_interleaving_reverse_replacement_boot_completion_keeps_newest_epoch() {
        let (mut state, registry, binding, chassis) = fixture();
        let mut ctx = NativeCtx::new(&binding, Source::NONE, None, None);
        let actor = proven_actor(&registry, "test.component.reverse-replacement");
        let old_hash = BlobHash::from_bytes([1; 32]);
        let new_hash = BlobHash::from_bytes([2; 32]);
        let old_operation = state.next_boot_operation(actor);
        assert!(state.accept_successful_boot_operation(actor, old_operation));
        let new_operation = state.next_boot_operation(actor);
        assert!(state.accept_successful_boot_operation(actor, new_operation));
        let old_boot = boot_entry(&chassis, &registry, "boot-n1", 0, 0);
        let new_boot = boot_entry(&chassis, &registry, "boot-n2", 0, 0);
        state.register_boot(old_hash, old_boot);
        state.register_boot(new_hash, new_boot);

        // Manual state-machine proof: N2's absent boot promotes first, then
        // N1's different boot promotes late. This is not a scheduler-order
        // proof; it directly drives the two completion orders that matter.
        state.commit_replacement_boot(&mut ctx, actor, new_operation, Some(new_hash));
        state.drop_orphan_boot(&mut ctx, new_hash);
        state.commit_replacement_boot(&mut ctx, actor, old_operation, Some(old_hash));
        state.drop_orphan_boot(&mut ctx, old_hash);

        assert_eq!(state.boot_hash_by_actor.get(&actor), Some(&new_hash));
        assert_eq!(state.boot_registry.get(&new_hash).map(|entry| entry.refcount), Some(1));
        assert!(!state.boot_registry.contains_key(&old_hash), "the boot created only for stale N1 is dropped");
    }

    #[test]
    fn later_failed_replacement_does_not_dominate_earlier_success() {
        let (mut state, registry, _binding, _chassis) = fixture();
        let actor = proven_actor(&registry, "test.component.later-failure");
        let earlier_success = state.next_boot_operation(actor);
        let later_failure = state.next_boot_operation(actor);

        // The later request reserves a sequence but its failed ReplaceResult
        // never enters the dominant table. The earlier successful request may
        // therefore still establish the actor's boot operation.
        assert!(state.accept_successful_boot_operation(actor, earlier_success));
        assert_eq!(state.dominant_boot_operation_by_actor.get(&actor), Some(&earlier_success));
        assert!(later_failure > earlier_success);
    }

    #[test]
    fn manual_interleaving_drop_before_replacement_boot_completion_cannot_resurrect_ref() {
        let (mut state, registry, binding, chassis) = fixture();
        let mut ctx = NativeCtx::new(&binding, Source::NONE, None, None);
        let actor = proven_actor(&registry, "test.component.drop-before-completion");
        let hash = BlobHash::from_bytes([1; 32]);
        let replacement_operation = state.next_boot_operation(actor);
        assert!(state.accept_successful_boot_operation(actor, replacement_operation));
        state.invalidate_replacement_boot_operation(actor);
        let boot = boot_entry(&chassis, &registry, "boot-after-drop", 0, 0);
        state.register_boot(hash, boot);

        // Manual state-machine proof: DropComponent invalidates the actor
        // before its boot completion arrives. This deliberately proves the
        // bookkeeping transition, not a particular scheduler ordering.
        state.commit_replacement_boot(&mut ctx, actor, replacement_operation, Some(hash));
        state.drop_orphan_boot(&mut ctx, hash);

        assert!(!state.boot_hash_by_actor.contains_key(&actor), "late completion cannot resurrect an actor boot ref");
        assert!(!state.boot_registry.contains_key(&hash), "a boot created solely for the stale completion is dropped");
    }
}
