//! Owner-staged component load, module-boot, and replacement stages. A
//! load's held reply waits in host state keyed by its [`LoadId`] (ADR-0243
//! §9) until the guest's birth answers it or hands it off; a replacement's
//! deferred reply rides its boot waiter until #6867 converts replace.

use std::mem;
use std::sync::Arc;

use aether_actor::{ErasedActorRef, Manual, OutboundReply, ReplyMode, Single};
use aether_data::{BlobHash, ErasedActorPath, Kind, Source};
use aether_kinds::{ComponentCapabilities, LoadComponent, LoadComponentUnder, ReplaceComponent, ReplaceResult};

use aether_substrate::actor::native::{
    DeferredReply, GuestBirth, GuestOutcome, Held, NativeCtx, RegistryBatch, RegistryBatchResult, TaskDone,
    spawn::Subname,
};
use aether_substrate::actor::wasm::kind_manifest::Dependency;
use aether_substrate::actor::wasm::module::{Module, ModuleManifest};

use super::LoadResult;
use super::dependencies::{dependency_refusal, inline_dependency_refusal};
use super::placement::{child_refusal, root_refusal};
use crate::component::runtime::{BootEntry, ComponentHostCapabilityState, GuestControl, PendingReplace};
use crate::component::{ComponentHostCapability, LoadDelivered};
use crate::kinds::{BootTeardown, GuestBorn, LoadPublished};
use crate::trampoline::{WasmTrampoline, WasmTrampolineConfig};

pub(super) struct PreparedLoad {
    capabilities: ComponentCapabilities,
    dependencies: Vec<Dependency>,
    type_tag: Option<u64>,
    /// The checked-in module. Its hash is, for a module that declares a boot
    /// slot, the boot registry's key.
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
    /// The module boot whose pending-request count this load's staged birth
    /// holds, once it is staged.
    boot: Option<BlobHash>,
}

/// The key of a [`LoadInFlight`], carried by its staged work's contexts.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct LoadId(u64);

/// A successor waiting on a module boot: a load, which the host finds in
/// its loads by id, or a committed replacement with the reply it defers
/// until #6867.
pub(super) enum BootWaiter {
    Load(LoadId),
    Replacement(Box<ReplacementWaiter>),
}

/// A committed replacement waiting on its new module boot, with the reply it
/// defers until #6867.
pub(super) struct ReplacementWaiter {
    owed: DeferredReply,
    pending: PendingReplace,
    result: ReplaceResult,
}

/// A staged module boot and every successor waiting on it, the first
/// included.
pub(super) struct PendingBoot {
    waiters: Vec<BootWaiter>,
}

impl Drop for PendingBoot {
    /// A load waiter's reply waits in the host's loads, which the ledger
    /// answers at close; only a replacement waiter carries its own deferred
    /// reply, which is abandoned here until #6867.
    fn drop(&mut self) {
        for waiter in self.waiters.drain(..) {
            if let BootWaiter::Replacement(waiter) = waiter {
                waiter.owed.abandon_for_actor_close();
            }
        }
    }
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
        self.loads.insert(id, LoadInFlight { held, load, boot: None });
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
        let Some(plan) = PreparedBoot::of(&module) else {
            self.stage_requested(ctx, id, None);
            return;
        };
        let hash = plan.hash();
        if self.boot_registry.contains_key(&hash) {
            self.stage_requested(ctx, id, Some(hash));
        } else if let Some(pending) = self.pending_boots.get_mut(&hash) {
            pending.waiters.push(BootWaiter::Load(id));
        } else {
            self.stage_module_boot(ctx, &plan, BootWaiter::Load(id));
        }
    }

    /// Stage a module's boot guest, the root singleton at the boot type's
    /// published name (ADR-0241 §5), with `first` as its first waiter.
    fn stage_module_boot<M: ReplyMode>(
        &mut self,
        ctx: &mut NativeCtx<'_, ComponentHostCapability, M>,
        plan: &PreparedBoot,
        first: BootWaiter,
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
    fn stage_requested(
        &mut self,
        ctx: &mut NativeCtx<'_, ComponentHostCapability, Single>,
        id: LoadId,
        boot: Option<BlobHash>,
    ) {
        let in_flight = self.loads.get_mut(&id).expect("a published load waits in state");
        let load = Arc::clone(&in_flight.load);
        if let Some(namespace) = ctx.missing_dependency(&load.dependencies) {
            let error = dependency_refusal(&load.namespace, namespace);
            self.take_load(id).held.answer(ctx, &LoadResult::Err { error });
            return;
        }
        in_flight.boot = boot;
        let birth = GuestBirth {
            namespace: &load.published,
            module: load.module.hash(),
            key: load.key.subname(),
            parent: load.placement.parent(),
        };
        match ctx
            .spawn_guest::<WasmTrampoline, GuestControl>(birth, load.requested_config(self), ())
            .stage_with(GuestBorn::Requested { load: id.0 })
        {
            Ok(_) => {
                if let Some(hash) = boot {
                    let entry =
                        self.boot_registry.get_mut(&hash).expect("requested actor starts only after its boot is Live");
                    entry.pending_requests = entry
                        .pending_requests
                        .checked_add(1)
                        .expect("module boot pending-request count cannot overflow");
                }
            }
            Err((error, _)) => {
                let error = format!("guest spawn failed: {error:?}");
                self.take_load(id).held.answer(ctx, &LoadResult::Err { error });
            }
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
            Ok(boot) => {
                self.register_boot(hash, BootEntry { boot, refcount: 0, pending_requests: 0 });
                for waiter in waiters {
                    self.finish_boot_waiter(ctx, waiter, hash);
                }
                self.drop_orphan_boot(ctx, hash);
            }
            Err(error) => {
                for waiter in waiters {
                    self.refuse_boot_waiter(
                        ctx,
                        waiter,
                        format!("module boot failed before requested actor: {error:?}"),
                    );
                }
            }
        }
    }

    fn finish_boot_waiter(
        &mut self,
        ctx: &mut NativeCtx<'_, ComponentHostCapability, Single>,
        waiter: BootWaiter,
        hash: BlobHash,
    ) {
        match waiter {
            BootWaiter::Load(id) => self.stage_requested(ctx, id, Some(hash)),
            BootWaiter::Replacement(waiter) => {
                let ReplacementWaiter { owed, pending, result } = *waiter;
                self.commit_replacement_boot(ctx, pending.actor, pending.boot_operation, Some(hash));
                owed.reply(ctx, &result);
            }
        }
    }

    /// Answer a boot waiter whose boot never came up: a load's caller hears
    /// `error`, and a replacement, whose swap already succeeded, answers its
    /// own result and logs the boot failure.
    fn refuse_boot_waiter<M: ReplyMode, A>(
        &mut self,
        ctx: &mut NativeCtx<'_, A, M>,
        waiter: BootWaiter,
        error: String,
    ) {
        match waiter {
            BootWaiter::Load(id) => self.take_load(id).held.answer(ctx, &LoadResult::Err { error }),
            BootWaiter::Replacement(waiter) => {
                let ReplacementWaiter { owed, pending, result } = *waiter;
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

    fn finish_requested(
        &mut self,
        ctx: &mut NativeCtx<'_, ComponentHostCapability, Single>,
        id: LoadId,
        outcome: GuestOutcome<GuestControl>,
    ) {
        let LoadInFlight { held, load, boot } = self.take_load(id);
        let control = match outcome.result {
            Ok(control) => control,
            Err(error) => {
                if let Some(hash) = boot {
                    self.settle_boot_request(ctx, hash, None);
                }
                held.answer(ctx, &LoadResult::Err { error: format!("guest spawn failed: {error:?}") });
                return;
            }
        };

        self.drop_targets.insert(control.erase(), control);
        if let Some(hash) = boot {
            self.settle_boot_request(ctx, hash, Some(control.erase()));
        }
        // ADR-0230 §3: the loaded guest answers the requester itself, so the
        // reply's stamped sender is the reference the requester keeps; the
        // host hands it the held reply rather than replying.
        let delivered = LoadDelivered { path: outcome.canonical_name, capabilities: load.capabilities.clone() };
        held.hand_off(ctx, control, &delivered);
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

    pub fn begin_replace<A>(
        &mut self,
        ctx: &mut NativeCtx<'_, A>,
        held: Held<ReplaceResult>,
        payload: ReplaceComponent,
    ) {
        let source = ctx.reply_target();
        // ADR-0230: prove the target address at receipt. A dropped
        // trampoline keeps its `Live` route (vacate, not close), so a replace
        // that refills it still proves; an address with no live route answers
        // `Err` here instead of parking a forward nothing will answer.
        let actor = match ctx.resolve_path(&payload.target) {
            Ok(proven) => proven,
            Err(error) => {
                let error = format!("no component to replace at {}: {error}", payload.target);
                held.answer(ctx, &ReplaceResult::Err { error });
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
                held.answer(ctx, &ReplaceResult::Err { error });
                return;
            }
        };
        // A replacement installs a module whose inline children are rebuilt
        // on rehydrate, so it is a module load for the ADR-0230 §3 check too.
        // The module-wide inline check runs here; the trampoline checks the
        // dependencies of the type the replacement will host.
        if let Some(error) = inline_dependency_refusal(ctx, module.manifest()) {
            held.answer(ctx, &ReplaceResult::Err { error });
            return;
        }
        // ADR-0241 §5: the replacement's module boot is host-placed, so a
        // boot type without `root` refuses the whole replace here, before
        // anything is staged, rather than failing the boot after the swap.
        let manifest = module.manifest();
        if let Some(error) = manifest.boot().and_then(|boot_ns| root_refusal(manifest.lineage(), boot_ns)) {
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
        let waiter = BootWaiter::Replacement(Box::new(ReplacementWaiter { owed, pending, result }));
        if let Some(inflight) = self.pending_boots.get_mut(&plan.hash()) {
            inflight.waiters.push(waiter);
        } else {
            self.stage_module_boot(ctx, &plan, waiter);
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

    use aether_actor::Addressable;
    use aether_data::{LoadName, Source};
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
            boot_registry: HashMap::new(),
            boot_actors: HashSet::new(),
            pending_boots: HashMap::new(),
            loads: HashMap::new(),
            next_load: 0,
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

    /// A boot entry over a test-local inbox registered beneath the host under
    /// `name`, proven as the host's trampoline child and narrowed to its
    /// control rows like a boot birth outcome's reference.
    fn boot_entry(
        chassis: &PassiveChassis<TestChassis>,
        registry: &Registry,
        name: &str,
        refcount: u32,
        pending_requests: u32,
    ) -> BootEntry {
        proven_actor(registry, &format!("{}/{}:{name}", ComponentHostCapability::NAMESPACE, WasmTrampoline::NAMESPACE));
        let host = chassis.actor_ref::<ComponentHostCapability>();
        let key = LoadName::new(name).expect("the fixture key is a valid load name");
        let boot = chassis
            .child::<ComponentHostCapability, WasmTrampoline>(host, key)
            .expect("the live trampoline route is proven")
            .narrow::<GuestControl>();
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
