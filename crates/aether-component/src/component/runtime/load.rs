//! A load is a publish and then a spawn (ADR-0241 §9), and this module owns
//! the spawn half every door shares: the prepared guest, its module boot,
//! its birth, and the requester it answers.
//!
//! A load prepares its guest before anything publishes, then publishes its
//! module through the host's one publish path ([`super::publish`]): a
//! module already published is not published again, a first publish binds
//! it, and a successor republishes every live instance of its namespaces as
//! one group (§7) before the load's guest is spawned. A load of a namespace
//! a republish holds waits in that republish until the republish answers.
//!
//! The spawn half answers a name that is already live with the instance
//! there, which is not re-initialised, and otherwise stages the guest's
//! birth. The requester's held reply waits in host state keyed by its
//! [`LoadId`] (ADR-0243 §9) until the birth answers it or hands it off.

use std::sync::Arc;

use aether_actor::{ErasedActorRef, ProtocolRef, ReplyMode};
use aether_data::{Blob, BlobHash, ErasedActorPath};
use aether_kinds::{ComponentCapabilities, LoadComponent, SpawnResult};

use aether_substrate::actor::native::{
    GuestBirth, GuestOutcome, Held, RegistryBatch, RegistryBatchResult, TaskDone, spawn::Subname,
};
use aether_substrate::actor::wasm::kind_manifest::Dependency;
use aether_substrate::actor::wasm::module::{Module, ModuleManifest};
use aether_substrate::mail::registry::Admitted;

use super::LoadResult;
use super::dependencies::dependency_refusal;
use super::placement::{child_refusal, root_refusal};
use super::publish::Publisher;
use super::republish::QueuedPublish;
use crate::component::runtime::{ComponentHostCapabilityState, GuestControl, HostCtx, LoadedGuest};
use crate::component::{LoadDelivered, SpawnDelivered};
use crate::kinds::{GuestBorn, LoadPublished, ModulePublished, RepublishPublished};
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

/// What a load or a spawn asks for: the type `export` selects from `module`
/// (its default when `None`), keyed by `name`, placed at `placement`, and
/// built with `config`.
pub(super) struct Selection {
    pub(super) module: Module,
    pub(super) export: Option<String>,
    pub(super) name: Option<String>,
    pub(super) config: Vec<u8>,
    pub(super) placement: LoadPlacement,
}

/// Where a load places its guest: at the root, or beneath a proven parent.
#[derive(Clone, Copy)]
pub(super) enum LoadPlacement {
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

    /// The name the guest takes (ADR-0241 §5), or `None` when a counter
    /// draws its key, which names no instance yet.
    fn name<M: ReplyMode>(&self, ctx: &HostCtx<'_, M>) -> Option<String> {
        let leaf = match &self.key {
            LoadKey::Singleton => self.published.clone(),
            LoadKey::Named(key) => format!("{}:{key}", self.published),
            LoadKey::Counter => return None,
        };
        Some(match self.placement {
            LoadPlacement::Root => leaf,
            LoadPlacement::Under { parent } => format!("{}/{leaf}", ctx.actor_path(parent)),
        })
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

/// The declared namespace of the exported type `module` publishes as
/// `published` (ADR-0241 §3): the inverse of [`published_name`], which the
/// export selector and the type tag read.
pub(super) fn declared_name(module: &Module, published: &str) -> Option<String> {
    module
        .manifest()
        .exported_groups()
        .zip(module.published_groups())
        .find(|(_, (name, _))| name == published)
        .map(|((declared, _), _)| declared.to_owned())
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

/// Who a guest's spawn answers: a load, with a [`LoadResult`], or a spawn,
/// with a [`SpawnResult`]. The guest answers either in its own name, so the
/// requester keeps the reply's stamped sender as its reference (ADR-0230
/// §3); only a refusal is answered by the host.
pub(super) enum Requester {
    Load(Held<LoadResult>),
    Spawn(Held<SpawnResult>),
}

impl Requester {
    fn refuse<M: ReplyMode>(self, ctx: &mut HostCtx<'_, M>, error: String) {
        match self {
            Self::Load(held) => held.answer(ctx, &LoadResult::Err { error }),
            Self::Spawn(held) => held.answer(ctx, &SpawnResult::Err { error }),
        }
    }

    /// Hand the owed reply to the guest `control` reaches, which answers
    /// with `path` and `capabilities`: a spawn hears whether it was `live`
    /// before the request arrived.
    fn deliver<M: ReplyMode>(
        self,
        ctx: &mut HostCtx<'_, M>,
        control: ProtocolRef<GuestControl>,
        path: ErasedActorPath,
        capabilities: ComponentCapabilities,
        live: bool,
    ) {
        match self {
            Self::Load(held) => held.hand_off(ctx, control, &LoadDelivered { path, capabilities }),
            Self::Spawn(held) => held.hand_off(ctx, control, &SpawnDelivered { path, capabilities, live }),
        }
    }
}

/// One spawn in flight, a load's or a spawn's, keyed by its [`LoadId`] from
/// the moment it is staged until the guest's birth settles it (ADR-0243 §9).
/// The held reply leaves only through an answer or `Held::hand_off`; at host
/// close the ledger answers it unanswered.
pub(super) struct LoadInFlight {
    requester: Requester,
    load: Arc<PreparedLoad>,
}

impl LoadInFlight {
    /// The name the guest is born at, which a publish of its module waits
    /// on until the birth settles (ADR-0241 §7).
    pub(super) fn published(&self) -> &str {
        &self.load.published
    }
}

/// The key of a [`LoadInFlight`], carried by its staged work's contexts.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct LoadId(u64);

/// A staged module boot and the spawns waiting on it, each found in the
/// host's loads by its id, with the name and module the boot guest is
/// recorded under once it is born. A boot a `Publish` staged starts with no
/// waiter.
pub(super) struct PendingBoot {
    waiters: Vec<LoadId>,
    namespace: String,
    module: Module,
}

impl ComponentHostCapabilityState {
    pub fn begin_load<M: ReplyMode>(
        &mut self,
        ctx: &mut HostCtx<'_, M>,
        held: Held<LoadResult>,
        payload: LoadComponent,
    ) {
        self.begin_load_at(ctx, held, payload, LoadPlacement::Root);
    }

    /// The placement beneath the live actor `parent` names. ADR-0230 §1: the
    /// parent must be `Live`. A `Starting` parent resolves as an address but
    /// does not prove, so a child is never staged beneath an unborn parent;
    /// the proof carries the parent's own canonical path.
    pub(super) fn placement_under<M: ReplyMode>(
        ctx: &HostCtx<'_, M>,
        parent: &ErasedActorPath,
    ) -> Result<LoadPlacement, String> {
        ctx.resolve_path(parent).map(|parent| LoadPlacement::Under { parent }).map_err(|error| error.to_string())
    }

    fn begin_load_at<M: ReplyMode>(
        &mut self,
        ctx: &mut HostCtx<'_, M>,
        held: Held<LoadResult>,
        payload: LoadComponent,
        placement: LoadPlacement,
    ) {
        let LoadComponent { wasm, name, config, export } = payload;

        // ADR-0241 §2: check the bytes in and take the module from the
        // engine's one cache, which compiles and parses them once per content
        // hash. The code blob is kept for a republish's prepare.
        let code = ctx.check_in(wasm.into_boxed_slice());
        let prepared = self
            .modules
            .check_in(&ctx.blob_check_in(), &code)
            .and_then(|module| Self::prepare_load(ctx, Selection { module, export, name, config, placement }));
        let load = match prepared {
            Ok(load) => load,
            Err(error) => {
                held.answer(ctx, &LoadResult::Err { error });
                return;
            }
        };

        if let Some(republish) = self.holding_republish(ctx, &load.published) {
            republish.park_load(held, load, code);
            return;
        }
        self.publish_then_spawn(ctx, held, load, code);
    }

    /// Publish a prepared load's module, then spawn its guest (ADR-0241 §9).
    /// A module that already publishes every namespace it exports spawns at
    /// once; any other goes through the host's one publish path, which
    /// spawns the guest once the module is bound or its republish commits.
    pub(super) fn publish_then_spawn<M: ReplyMode>(
        &mut self,
        ctx: &mut HostCtx<'_, M>,
        held: Held<LoadResult>,
        load: Arc<PreparedLoad>,
        code: Blob,
    ) {
        if ctx.admission_preview(&load.module) == Ok(Admitted::Unchanged) {
            self.spawn_prepared(ctx, Requester::Load(held), load);
            return;
        }
        let module = load.module.clone();
        self.publish_or_queue(ctx, QueuedPublish::new(Publisher::Load { held, load }, code, module, Vec::new()));
    }

    /// Publish a load's module for the first time before anything spawns
    /// (ADR-0241 §3, §4). The owner runs admission and registers the
    /// module's kinds in one batch; the held reply waits here, keyed by the
    /// load, until its completion (ADR-0243 §9).
    pub(super) fn publish_load<M: ReplyMode>(
        &mut self,
        ctx: &mut HostCtx<'_, M>,
        held: Held<LoadResult>,
        load: Arc<PreparedLoad>,
    ) {
        let batch = RegistryBatch::publish_module(&load.module);
        let id = self.track_load(Requester::Load(held), load);
        ctx.stage_registry_batch(batch, LoadPublished { load: id.0 });
    }

    /// Spawn a prepared guest of a published module: its module boot first,
    /// once, then the guest at its name (ADR-0241 §6, §9).
    pub(super) fn spawn_prepared<M: ReplyMode>(
        &mut self,
        ctx: &mut HostCtx<'_, M>,
        requester: Requester,
        load: Arc<PreparedLoad>,
    ) {
        let id = self.track_load(requester, load);
        self.continue_load(ctx, id);
    }

    fn track_load(&mut self, requester: Requester, load: Arc<PreparedLoad>) -> LoadId {
        let id = LoadId(self.next_load);
        self.next_load = self.next_load.checked_add(1).expect("the component host's load ids cannot overflow");
        self.loads.insert(id, LoadInFlight { requester, load });
        id
    }

    fn take_load(&mut self, id: LoadId) -> LoadInFlight {
        self.loads.remove(&id).expect("a load in flight waits in state until it is answered")
    }

    /// Select the type a [`Selection`] names from its module and place it,
    /// or refuse (ADR-0138, ADR-0147, ADR-0241 §5). A load and a spawn
    /// prepare their guest here, before anything publishes or stands up.
    pub(super) fn prepare_load<M: ReplyMode>(
        ctx: &HostCtx<'_, M>,
        selection: Selection,
    ) -> Result<Arc<PreparedLoad>, String> {
        let Selection { module, export, name, config, placement } = selection;
        let manifest = module.manifest();
        let actors = manifest.actors();

        // ADR-0241 §5: a module boot is always a root singleton, whatever the
        // requested placement, so its type must declare `root`.
        if let Some(error) = manifest.boot().and_then(|boot_ns| root_refusal(manifest.lineage(), boot_ns)) {
            return Err(error);
        }

        if let Some(boot_ns) = manifest.boot()
            && export.as_deref() == Some(boot_ns)
        {
            return Err(format!(
                "export {boot_ns:?} names this module's boot actor, which is not selectable (ADR-0147)"
            ));
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
                return Err(format!("export {requested:?} not found in module; exported types: {available:?}"));
            };
            let tag = aether_data::ActorId::singleton(requested).0;
            (group.capabilities.clone(), group.dependencies.clone(), Some(tag), Some(requested.clone()))
        } else {
            // ADR-0138 (issue #7163): no fallback to a module's `export!(default
            // = …)` any more — an unselected load succeeds only when the module
            // exports exactly one non-boot type, and is refused, naming every
            // export, otherwise.
            let boot_ns = manifest.boot();
            let selectable: Vec<_> = actors
                .iter()
                .filter(|actor| boot_ns.is_none_or(|boot_ns| actor.namespace.as_deref() != Some(boot_ns)))
                .collect();
            let [sole] = selectable.as_slice() else {
                let available: Vec<&str> = actors.iter().filter_map(|actor| actor.namespace.as_deref()).collect();
                return Err(format!(
                    "load selects no export (ADR-0138): load one of the module's exports by name via the export \
                     selector; exported types: {available:?}"
                ));
            };
            (sole.capabilities.clone(), sole.dependencies.clone(), None, sole.namespace.clone())
        };

        let Some(namespace) = selected_namespace.or_else(|| manifest.namespace().map(str::to_owned)) else {
            return Err("the load selects no actor namespace, so it cannot be placed or keyed".to_owned());
        };

        let key = Self::placement_key(ctx, manifest, &namespace, name, placement)?;
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
    fn placement_key<M: ReplyMode>(
        ctx: &HostCtx<'_, M>,
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

    /// A module publish settled, a load's, a `Publish`'s, or a republish's,
    /// as its context names it.
    pub(super) fn finish_publish<M: ReplyMode>(
        &mut self,
        ctx: &mut HostCtx<'_, M>,
        done: TaskDone<RegistryBatchResult>,
    ) {
        if let Some(LoadPublished { load }) = ctx.take_context() {
            self.finish_load_publish(ctx, LoadId(load), done.into_output());
        } else if let Some(ModulePublished { publish }) = ctx.take_context() {
            self.finish_module_publish(ctx, publish, done.into_output());
        } else if let Some(RepublishPublished { republish }) = ctx.take_context() {
            self.finish_republish_publish(ctx, republish, done.into_output());
        }
    }

    /// A load's first module publish settled (ADR-0241 §3): a refusal
    /// (admission or a kind conflict) answers the caller and spawns nothing;
    /// a commit continues to the module boot and the requested guest.
    fn finish_load_publish<M: ReplyMode>(
        &mut self,
        ctx: &mut HostCtx<'_, M>,
        id: LoadId,
        published: RegistryBatchResult,
    ) {
        match published {
            Ok(()) => {
                let module = self.loads.get(&id).expect("a published load waits in state").load.module.clone();
                self.record_published(&module);
                self.continue_load(ctx, id);
            }
            Err(error) => {
                let error = format!("module publish refused: {error}");
                self.take_load(id).requester.refuse(ctx, error);
            }
        }
    }

    /// Record a module whose first publish committed: a module that declares
    /// a boot has its namespaces recorded as not replaceable (ADR-0147).
    pub(super) fn record_published(&mut self, module: &Module) {
        if module.manifest().boot().is_some() {
            self.boot_namespaces.extend(module.published_groups().map(|(published, _)| published.into_owned()));
        }
    }

    /// Stage `module`'s boot, which no spawn waits on, when it declares one
    /// that is neither born nor staged: a `Publish` spawns it once, when the
    /// module is first published (ADR-0147, ADR-0241 §8).
    pub(super) fn boot_published<M: ReplyMode>(&mut self, ctx: &mut HostCtx<'_, M>, module: &Module) {
        if let Some(plan) = PreparedBoot::of(module).filter(|plan| {
            !self.booted_modules.contains(&plan.hash()) && !self.pending_boots.contains_key(&plan.hash())
        }) {
            self.stage_module_boot(ctx, &plan, Vec::new());
        }
    }

    fn continue_load<M: ReplyMode>(&mut self, ctx: &mut HostCtx<'_, M>, id: LoadId) {
        let module = self.loads.get(&id).expect("a published load waits in state").load.module.clone();
        // ADR-0147, ADR-0241 §8: a module's boot spawns once. A later spawn,
        // including one after the boot was dropped, proceeds without one,
        // and one that arrives while it is staged waits for it.
        let Some(plan) = PreparedBoot::of(&module).filter(|plan| !self.booted_modules.contains(&plan.hash())) else {
            self.stage_requested(ctx, id);
            return;
        };
        if let Some(pending) = self.pending_boots.get_mut(&plan.hash()) {
            pending.waiters.push(id);
        } else {
            self.stage_module_boot(ctx, &plan, vec![id]);
        }
    }

    /// Stage a module's boot guest, the root singleton at the boot type's
    /// published name (ADR-0241 §5), with `waiters` waiting on it.
    fn stage_module_boot<M: ReplyMode>(&mut self, ctx: &mut HostCtx<'_, M>, plan: &PreparedBoot, waiters: Vec<LoadId>) {
        if let Some(namespace) = ctx.missing_dependency(&plan.dependencies) {
            self.refuse_boot_waiters(ctx, waiters, &dependency_refusal(&plan.namespace, namespace));
            return;
        }
        let hash = plan.hash();
        let birth = GuestBirth { namespace: &plan.published, module: hash, key: None, parent: None };
        match ctx
            .spawn_guest::<WasmTrampoline, GuestControl>(birth, plan.config(self), ())
            .stage_with(GuestBorn::Boot { hash: *hash.as_bytes() })
        {
            Ok(_) => {
                let pending = PendingBoot { waiters, namespace: plan.published.clone(), module: plan.module.clone() };
                let previous = self.pending_boots.insert(hash, pending);
                debug_assert!(previous.is_none(), "one actor-local reservation owns a module boot hash");
            }
            Err((error, _)) => {
                let error = format!("module boot failed before requested actor: boot guest spawn failed: {error:?}");
                self.refuse_boot_waiters(ctx, waiters, &error);
            }
        }
    }

    /// Stage the guest at its published name (ADR-0241 §5, §6): `NS`,
    /// `NS:key`, or `parent/NS:key`. A name already live answers with the
    /// instance there, which is not re-initialised (§9).
    fn stage_requested<M: ReplyMode>(&mut self, ctx: &mut HostCtx<'_, M>, id: LoadId) {
        let load = Arc::clone(&self.loads.get(&id).expect("a published load waits in state").load);
        if let Some((actor, control)) = self.live_instance(ctx, &load) {
            let path = ctx.actor_path(actor);
            self.take_load(id).requester.deliver(ctx, control, path, load.capabilities.clone(), true);
            return;
        }
        if let Some(namespace) = ctx.missing_dependency(&load.dependencies) {
            let error = dependency_refusal(&load.namespace, namespace);
            self.take_load(id).requester.refuse(ctx, error);
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
            self.take_load(id).requester.refuse(ctx, error);
        }
    }

    /// The live guest of the load's type at the name the load would take,
    /// with its control proof. A name nothing is live at, a tombstoned one
    /// included, answers `None`, and the birth there decides the answer.
    fn live_instance<M: ReplyMode>(
        &self,
        ctx: &HostCtx<'_, M>,
        load: &PreparedLoad,
    ) -> Option<(ErasedActorRef, ProtocolRef<GuestControl>)> {
        let actor = ErasedActorPath::new(&load.name(ctx)?).ok().and_then(|name| ctx.resolve_path(&name).ok())?;
        self.drop_targets
            .get(&actor)
            .filter(|guest| guest.namespace == load.published)
            .map(|guest| (actor, guest.control))
    }

    /// A staged guest birth settled: a module boot releases its waiters, and
    /// a requested guest takes over its requester's held reply.
    pub(super) fn finish_guest_birth<M: ReplyMode>(
        &mut self,
        ctx: &mut HostCtx<'_, M>,
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
    /// (ADR-0241 §8); every waiting spawn continues to its requested guest.
    /// On failure every waiting spawn is refused.
    fn finish_module_boot<M: ReplyMode>(
        &mut self,
        ctx: &mut HostCtx<'_, M>,
        hash: BlobHash,
        outcome: GuestOutcome<GuestControl>,
    ) {
        let PendingBoot { waiters, namespace, module } =
            self.pending_boots.remove(&hash).expect("module boot retains its actor-local reservation");
        match outcome.result {
            Ok(control) => {
                self.booted_modules.insert(hash);
                self.drop_targets.insert(control.erase(), LoadedGuest { control, namespace, module });
                for id in waiters {
                    self.stage_requested(ctx, id);
                }
            }
            Err(error) => {
                let error = format!("module boot failed before requested actor: {error:?}");
                self.refuse_boot_waiters(ctx, waiters, &error);
            }
        }
    }

    /// Answer each spawn whose module boot never came up with `error`. A
    /// boot no spawn waits on, one a `Publish` staged, is logged instead,
    /// and the module's next spawn stages it again.
    fn refuse_boot_waiters<M: ReplyMode>(&mut self, ctx: &mut HostCtx<'_, M>, waiters: Vec<LoadId>, error: &str) {
        if waiters.is_empty() {
            tracing::warn!(target: "aether_component", error, "module boot failed with no spawn waiting on it");
        }
        for id in waiters {
            self.take_load(id).requester.refuse(ctx, error.to_owned());
        }
    }

    fn finish_requested<M: ReplyMode>(
        &mut self,
        ctx: &mut HostCtx<'_, M>,
        id: LoadId,
        outcome: GuestOutcome<GuestControl>,
    ) {
        let LoadInFlight { requester, load } = self.take_load(id);
        let control = match outcome.result {
            Ok(control) => control,
            Err(error) => {
                requester.refuse(ctx, format!("guest spawn failed: {error:?}"));
                return;
            }
        };

        let guest = LoadedGuest { control, namespace: load.published.clone(), module: load.module.clone() };
        self.drop_targets.insert(control.erase(), guest);
        // ADR-0230 §3: the guest answers the requester itself, so the reply's
        // stamped sender is the reference the requester keeps; the host hands
        // it the held reply rather than replying.
        requester.deliver(ctx, control, outcome.canonical_name, load.capabilities.clone(), false);
    }
}
