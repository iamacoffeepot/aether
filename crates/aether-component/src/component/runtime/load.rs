//! Owner-staged component load, module-boot, and replacement stages. A
//! load's held reply waits in host state keyed by its [`LoadId`] (ADR-0243
//! §9) until the guest's birth answers it or hands it off.

use std::mem;
use std::sync::Arc;

use aether_actor::{ErasedActorRef, Manual, OutboundReply, ReplyMode, Single};
use aether_data::{BlobHash, ErasedActorPath, Kind, Source};
use aether_kinds::{ComponentCapabilities, LoadComponent, LoadComponentUnder, ReplaceComponent, ReplaceResult};

use aether_substrate::actor::native::{
    GuestBirth, GuestOutcome, Held, NativeCtx, RegistryBatch, RegistryBatchResult, TaskDone, spawn::Subname,
};
use aether_substrate::actor::wasm::kind_manifest::Dependency;
use aether_substrate::actor::wasm::module::{Module, ModuleManifest};

use super::LoadResult;
use super::dependencies::{dependency_refusal, inline_dependency_refusal};
use super::placement::{child_refusal, root_refusal};
use crate::component::runtime::{ComponentHostCapabilityState, GuestControl, LoadedGuest, PendingReplace};
use crate::component::{ComponentHostCapability, LoadDelivered};
use crate::kinds::{GuestBorn, LoadPublished};
use crate::trampoline::{WasmTrampoline, WasmTrampolineConfig};

pub(super) struct PreparedLoad {
    capabilities: ComponentCapabilities,
    dependencies: Vec<Dependency>,
    type_tag: Option<u64>,
    /// The checked-in module. Its hash is, for a module that declares a boot
    /// slot, the key its boot spawns once under.
    module: Module,
    config: Vec<u8>,
    /// The selected type's declared namespace, which names it in a refusal.
    namespace: String,
    /// The name the selected type publishes under (ADR-0241 §3), which the
    /// guest is born at: its namespace, or `NS.<hash>` for a
    /// content-addressed module.
    published: String,
    key: LoadKey,
    placement: LoadPlacement,
}

/// The key a guest is born under (ADR-0241 §5): a singleton names none, an
/// instanced type takes the load's name or a counter.
enum LoadKey {
    Singleton,
    Named(String),
    Counter,
}

impl LoadKey {
    fn subname(&self) -> Option<Subname<'_>> {
        match self {
            Self::Singleton => None,
            Self::Named(name) => Some(Subname::Named(name)),
            Self::Counter => Some(Subname::Counter),
        }
    }
}

/// Where a load places its guest: at the root, or beneath a proven parent.
#[derive(Clone, Copy)]
enum LoadPlacement {
    Root,
    Under { parent: ErasedActorRef },
}

impl LoadPlacement {
    fn parent(self) -> Option<ErasedActorRef> {
        match self {
            Self::Root => None,
            Self::Under { parent } => Some(parent),
        }
    }
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

/// The name `namespace`, an exported type of `module`, publishes under
/// (ADR-0241 §3), or `None` when the module exports no such type.
fn published_name(module: &Module, namespace: &str) -> Option<String> {
    module
        .manifest()
        .exported_groups()
        .zip(module.published_groups())
        .find(|((declared, _), _)| *declared == namespace)
        .map(|(_, (published, _))| published.into_owned())
}

pub(super) struct PreparedBoot {
    namespace: String,
    /// The boot type's published name, which the boot is born at.
    published: String,
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
            published: published_name(module, namespace).unwrap_or_else(|| namespace.to_owned()),
            capabilities: group.map(|actor| actor.capabilities.clone()).unwrap_or_default(),
            dependencies: group.map(|actor| actor.dependencies.clone()).unwrap_or_default(),
            module: module.clone(),
        })
    }

    /// The module's content hash, which its boot spawns once under.
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

/// One load in flight, keyed by its [`LoadId`] from the moment its module
/// publish is staged until the guest's birth settles it (ADR-0243 §9). The
/// held reply leaves only through `answer` or `Held::hand_off`; at host
/// close the ledger answers it `LoadResult::unanswered()`.
pub(super) struct LoadInFlight {
    held: Held<LoadResult>,
    load: Arc<PreparedLoad>,
}

/// The key of a [`LoadInFlight`], carried by its staged work's contexts.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct LoadId(u64);

/// A staged module boot and the loads waiting on it, the first included,
/// each found in the host's loads by its id.
pub(super) struct PendingBoot {
    waiters: Vec<LoadId>,
}

impl ComponentHostCapabilityState {
    pub fn begin_load<A, M: ReplyMode>(
        &mut self,
        ctx: &mut NativeCtx<'_, A, M>,
        held: Held<LoadResult>,
        payload: LoadComponent,
    ) {
        self.begin_load_at(ctx, held, payload, LoadPlacement::Root);
    }

    pub fn begin_load_under<A, M: ReplyMode>(
        &mut self,
        ctx: &mut NativeCtx<'_, A, M>,
        held: Held<LoadResult>,
        payload: LoadComponentUnder,
    ) {
        // ADR-0230 §1: the parent must be `Live`. A `Starting` parent resolves
        // as an address but does not prove, so a child is never staged beneath
        // an unborn parent; the proof carries the parent's own canonical path.
        let resolved = ErasedActorPath::new(&payload.parent)
            .map_err(|error| error.to_string())
            .and_then(|parent| ctx.resolve_path(&parent).map_err(|error| error.to_string()));
        let parent = match resolved {
            Ok(parent) => parent,
            Err(error) => {
                held.answer(
                    ctx,
                    &LoadResult::Err {
                        error: format!("component parent {:?} did not resolve: {error}", payload.parent),
                    },
                );
                return;
            }
        };
        self.begin_load_at(ctx, held, payload.load, LoadPlacement::Under { parent });
    }

    fn begin_load_at<A, M: ReplyMode>(
        &mut self,
        ctx: &mut NativeCtx<'_, A, M>,
        held: Held<LoadResult>,
        payload: LoadComponent,
        placement: LoadPlacement,
    ) {
        let load = match self.prepare_load(ctx, payload, placement) {
            Ok(load) => load,
            Err(result) => {
                held.answer(ctx, &result);
                return;
            }
        };
        // ADR-0241 §3/§4: publish the module before anything spawns. The
        // owner runs admission and registers the module's kinds in one batch;
        // the held reply waits here, keyed by the load, until its completion
        // (ADR-0243 §9).
        let id = self.next_load_id();
        let batch = RegistryBatch::publish_module(&load.module);
        self.loads.insert(id, LoadInFlight { held, load });
        ctx.stage_registry_batch(batch, LoadPublished { load: id.0 });
    }

    fn next_load_id(&mut self) -> LoadId {
        let id = LoadId(self.next_load);
        self.next_load = self.next_load.checked_add(1).expect("the component host's load ids cannot overflow");
        id
    }

    fn take_load(&mut self, id: LoadId) -> LoadInFlight {
        self.loads.remove(&id).expect("a load in flight waits in state until it is answered")
    }

    #[allow(
        clippy::result_large_err,
        reason = "cold synchronous preparation returns the exact public LoadResult error shape"
    )]
    fn prepare_load<A, M: ReplyMode>(
        &self,
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

        // ADR-0241 §5: a module boot is always a root singleton, whatever the
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

        let Some(namespace) = selected_namespace.or_else(|| manifest.namespace().map(str::to_owned)) else {
            return Err(LoadResult::Err {
                error: "the load selects no actor namespace, so it cannot be placed or keyed".to_owned(),
            });
        };

        let key = Self::placement_key(ctx, manifest, &namespace, name, placement)
            .map_err(|error| LoadResult::Err { error })?;
        let published = published_name(&module, &namespace).unwrap_or_else(|| namespace.clone());

        capabilities.assets = manifest.asset_catalog().to_vec();

        Ok(Arc::new(PreparedLoad {
            capabilities,
            dependencies,
            type_tag,
            module,
            config,
            namespace,
            published,
            key,
            placement,
        }))
    }

    /// The key the selected type is born under, or the refusal of its
    /// placement (ADR-0241 §5). A guest is placed by its `#[actor]`
    /// declaration: a root load needs `root`, and a `load_under` a `child_of`
    /// edge naming the proven parent's type. A singleton is named by its
    /// namespace alone, so a load names no key for it and places it at the
    /// root; an instanced load's name is its key, or the spawn allocates a
    /// counter. Both are refused before the module publishes, so a refused
    /// load registers no route (#6821). A single-actor module's implicit
    /// group is named by the module's namespace.
    fn placement_key<A, M: ReplyMode>(
        ctx: &NativeCtx<'_, A, M>,
        manifest: &ModuleManifest,
        namespace: &str,
        name: Option<String>,
        placement: LoadPlacement,
    ) -> Result<LoadKey, String> {
        let refusal = match placement {
            LoadPlacement::Root => root_refusal(manifest.lineage(), namespace),
            LoadPlacement::Under { parent } => {
                let path = ctx.actor_path(parent);
                let leaf = path.as_str().rsplit('/').next().unwrap_or(path.as_str());
                let parent_namespace = leaf.split_once(':').map_or(leaf, |(parent_namespace, _)| parent_namespace);
                child_refusal(manifest.lineage(), namespace, parent_namespace)
            }
        };
        if let Some(error) = refusal {
            return Err(error);
        }

        match (manifest.instanced(namespace), name) {
            (Some(false), Some(_)) => Err(format!("{namespace} is a singleton; a load names no key")),
            (Some(false), None) if matches!(placement, LoadPlacement::Under { .. }) => {
                Err(format!("{namespace} is a singleton; it is named at the root and has no parent"))
            }
            (Some(false), None) => Ok(LoadKey::Singleton),
            (Some(true), Some(name)) => Ok(LoadKey::Named(name)),
            (Some(true), None) => Ok(LoadKey::Counter),
            (None, _) => Err(format!("{namespace} is not an exported type of this module")),
        }
    }

    /// A load's module publish settled (ADR-0241 §3): a refusal (admission
    /// or a kind conflict) answers the caller and spawns nothing; a commit
    /// continues to the module boot and the requested guest.
    pub(super) fn finish_load_publish(
        &mut self,
        ctx: &mut NativeCtx<'_, ComponentHostCapability, Single>,
        done: TaskDone<RegistryBatchResult>,
    ) {
        let Some(LoadPublished { load }) = ctx.take_context() else {
            return;
        };
        let id = LoadId(load);
        match done.into_output() {
            Ok(()) => self.continue_load(ctx, id),
            Err(error) => {
                let error = format!("module publish refused: {error}");
                self.take_load(id).held.answer(ctx, &LoadResult::Err { error });
            }
        }
    }

    fn continue_load(&mut self, ctx: &mut NativeCtx<'_, ComponentHostCapability, Single>, id: LoadId) {
        let module = self.loads.get(&id).expect("a published load waits in state").load.module.clone();
        // ADR-0147, ADR-0241 §8: a module's boot spawns once, by its first
        // load. A later load, including one after the boot was dropped,
        // proceeds without one.
        let Some(plan) = PreparedBoot::of(&module).filter(|plan| !self.booted_modules.contains(&plan.hash())) else {
            self.stage_requested(ctx, id);
            return;
        };
        if let Some(pending) = self.pending_boots.get_mut(&plan.hash()) {
            pending.waiters.push(id);
        } else {
            self.stage_module_boot(ctx, &plan, id);
        }
    }

    /// Stage a module's boot guest, the root singleton at the boot type's
    /// published name (ADR-0241 §5), with `first` as its first waiter.
    fn stage_module_boot(
        &mut self,
        ctx: &mut NativeCtx<'_, ComponentHostCapability, Single>,
        plan: &PreparedBoot,
        first: LoadId,
    ) {
        if let Some(namespace) = ctx.missing_dependency(&plan.dependencies) {
            self.refuse_boot_waiter(ctx, first, dependency_refusal(&plan.namespace, namespace));
            return;
        }
        let hash = plan.hash();
        let birth = GuestBirth { namespace: &plan.published, module: hash, key: None, parent: None };
        match ctx
            .spawn_guest::<WasmTrampoline, GuestControl>(birth, plan.config(self), ())
            .stage_with(GuestBorn::Boot { hash: *hash.as_bytes() })
        {
            Ok(_) => {
                let previous = self.pending_boots.insert(hash, PendingBoot { waiters: vec![first] });
                debug_assert!(previous.is_none(), "one actor-local reservation owns a module boot hash");
            }
            Err((error, _)) => {
                let error = format!("module boot failed before requested actor: boot guest spawn failed: {error:?}");
                self.refuse_boot_waiter(ctx, first, error);
            }
        }
    }

    /// Stage the load's guest at its published name (ADR-0241 §5, §6):
    /// `NS`, `NS:key`, or `parent/NS:key`.
    fn stage_requested(&mut self, ctx: &mut NativeCtx<'_, ComponentHostCapability, Single>, id: LoadId) {
        let load = Arc::clone(&self.loads.get(&id).expect("a published load waits in state").load);
        if let Some(namespace) = ctx.missing_dependency(&load.dependencies) {
            let error = dependency_refusal(&load.namespace, namespace);
            self.take_load(id).held.answer(ctx, &LoadResult::Err { error });
            return;
        }
        let birth = GuestBirth {
            namespace: &load.published,
            module: load.module.hash(),
            key: load.key.subname(),
            parent: load.placement.parent(),
        };
        if let Err((error, _)) = ctx
            .spawn_guest::<WasmTrampoline, GuestControl>(birth, load.requested_config(self), ())
            .stage_with(GuestBorn::Requested { load: id.0 })
        {
            let error = format!("guest spawn failed: {error:?}");
            self.take_load(id).held.answer(ctx, &LoadResult::Err { error });
        }
    }

    /// A staged guest birth settled: a module boot releases its waiters, and
    /// a requested guest takes over its load's held reply.
    pub(super) fn finish_guest_birth(
        &mut self,
        ctx: &mut NativeCtx<'_, ComponentHostCapability, Single>,
        done: TaskDone<GuestOutcome<GuestControl>>,
    ) {
        match ctx.take_context::<GuestBorn>() {
            Some(GuestBorn::Boot { hash }) => {
                self.finish_module_boot(ctx, BlobHash::from_bytes(hash), done.into_output());
            }
            Some(GuestBorn::Requested { load }) => self.finish_requested(ctx, LoadId(load), done.into_output()),
            None => {}
        }
    }

    /// A module's boot birth settled. On success its hash joins the booted
    /// modules for good, so the boot is never spawned again, and its control
    /// reference joins the drop targets, so a drop reaches it and closes it
    /// (ADR-0241 §8); every waiting load continues to its requested guest.
    /// On failure every waiting load is refused.
    fn finish_module_boot(
        &mut self,
        ctx: &mut NativeCtx<'_, ComponentHostCapability, Single>,
        hash: BlobHash,
        outcome: GuestOutcome<GuestControl>,
    ) {
        let waiters = mem::take(
            &mut self.pending_boots.remove(&hash).expect("module boot retains its actor-local reservation").waiters,
        );
        match outcome.result {
            Ok(control) => {
                self.booted_modules.insert(hash);
                self.drop_targets.insert(control.erase(), LoadedGuest { control, from_boot_module: true });
                for id in waiters {
                    self.stage_requested(ctx, id);
                }
            }
            Err(error) => {
                for id in waiters {
                    self.refuse_boot_waiter(ctx, id, format!("module boot failed before requested actor: {error:?}"));
                }
            }
        }
    }

    /// Answer a load whose module boot never came up with `error`.
    fn refuse_boot_waiter(
        &mut self,
        ctx: &mut NativeCtx<'_, ComponentHostCapability, Single>,
        id: LoadId,
        error: String,
    ) {
        self.take_load(id).held.answer(ctx, &LoadResult::Err { error });
    }

    fn finish_requested(
        &mut self,
        ctx: &mut NativeCtx<'_, ComponentHostCapability, Single>,
        id: LoadId,
        outcome: GuestOutcome<GuestControl>,
    ) {
        let LoadInFlight { held, load } = self.take_load(id);
        let control = match outcome.result {
            Ok(control) => control,
            Err(error) => {
                held.answer(ctx, &LoadResult::Err { error: format!("guest spawn failed: {error:?}") });
                return;
            }
        };

        // A replace can neither add nor remove a boot, so whether the guest's
        // module declares one is fixed at its birth.
        let from_boot_module = load.module.manifest().boot().is_some();
        self.drop_targets.insert(control.erase(), LoadedGuest { control, from_boot_module });
        // ADR-0230 §3: the loaded guest answers the requester itself, so the
        // reply's stamped sender is the reference the requester keeps; the
        // host hands it the held reply rather than replying.
        let delivered = LoadDelivered { path: outcome.canonical_name, capabilities: load.capabilities.clone() };
        held.hand_off(ctx, control, &delivered);
    }

    pub fn begin_replace<A>(
        &mut self,
        ctx: &mut NativeCtx<'_, A>,
        held: Held<ReplaceResult>,
        payload: ReplaceComponent,
    ) {
        let source = ctx.reply_target();
        // ADR-0230: prove the target address at receipt. An address with no
        // live route answers `Err` here instead of parking a forward nothing
        // will answer.
        let actor = match ctx.resolve_path(&payload.target) {
            Ok(proven) => proven,
            Err(error) => {
                let error = format!("no component to replace at {}: {error}", payload.target);
                held.answer(ctx, &ReplaceResult::Err { error });
                return;
            }
        };
        // ADR-0241 §8: only a live guest this host loaded is replaced. A
        // dropped one left the drop targets when its drop was forwarded, so a
        // replace that proves its path before the owner applies its `Dropped`
        // route is refused here.
        let Some(guest) = self.drop_targets.get(&actor) else {
            let error = format!("no live component at {}", payload.target);
            held.answer(ctx, &ReplaceResult::Err { error });
            return;
        };
        // ADR-0147: a module that declares a boot is not replaceable, so a
        // guest from one, its boot included, is refused before the bytes
        // check in.
        if guest.from_boot_module {
            let error = format!(
                "{} comes from a module that declares a boot, which is not replaceable: \
                 a boot module upgrades by engine restart",
                payload.target
            );
            held.answer(ctx, &ReplaceResult::Err { error });
            return;
        }
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
                held.answer(ctx, &ReplaceResult::Err { error });
                return;
            }
        };
        // ADR-0147: a replace can never add a boot either, so a replacement
        // module that declares one is refused before anything publishes.
        if let Some(boot) = module.manifest().boot() {
            let error = format!(
                "the replacement module declares the boot {boot}, which is not replaceable: \
                 a boot module upgrades by engine restart"
            );
            held.answer(ctx, &ReplaceResult::Err { error });
            return;
        }
        // A replacement installs a module whose inline children are rebuilt
        // on rehydrate, so it is a module load for the ADR-0230 §3 check too.
        // The module-wide inline check runs here; the trampoline checks the
        // dependencies of the type the replacement will host.
        if let Some(error) = inline_dependency_refusal(ctx, module.manifest()) {
            held.answer(ctx, &ReplaceResult::Err { error });
            return;
        }
        // ADR-0241 §4: a replace republishes its module, so admission runs
        // and the replacement's kinds register before the trampoline sees it.
        let batch = RegistryBatch::publish_module(&module);
        let _ = ctx.stage_registry_batch_from(held, batch, ReplacePublication { source, actor, module, bytes });
    }

    /// A replace's module publish settled (ADR-0241 §4): a refusal answers
    /// the caller and the replace is never forwarded; a commit forwards it to
    /// its trampoline.
    pub(super) fn finish_replace_publish(
        &mut self,
        ctx: &mut NativeCtx<'_, ComponentHostCapability, Single>,
        done: TaskDone<RegistryBatchResult, ReplacePublication>,
    ) {
        if let Err(error) = done.output() {
            let error = format!("module publish refused: {error}");
            done.resolve_with(ctx, move |_, _| ReplaceResult::Err { error });
            return;
        }
        let replace = done.context().clone();
        self.forward_replace(ctx, done, replace);
    }

    /// Forward a replace to its trampoline once the replacement module's
    /// publish commits, under the caller's chain. The forward's
    /// `ReplaceResult` comes back to [`Self::finish_replace`].
    fn forward_replace(
        &mut self,
        ctx: &mut NativeCtx<'_, ComponentHostCapability, Single>,
        done: TaskDone<RegistryBatchResult, ReplacePublication>,
        replace: ReplacePublication,
    ) {
        let ReplacePublication { source, actor, module, bytes } = replace;
        match done.forward_tracked(ctx, actor, ReplaceComponent::ID, &bytes) {
            Ok(mail_id) => {
                self.pending_replace.insert(mail_id.correlation_id, PendingReplace { source, module });
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
        ctx.reply_to(pending.source, &result);
    }
}
