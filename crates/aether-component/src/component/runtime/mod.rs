//! The `aether.component` runtime half (ADR-0122 identity/runtime split).
//! Compiled only under `feature = "runtime"` (the `mod runtime;` declaration
//! in the parent carries the gate), so a transport-only build of the
//! `ComponentHostCapability` identity never names these types nor pulls
//! `aether_substrate` / `wasmtime`. The substrate-typed imports are gated once
//! by this module rather than line-by-line; the `#[actor] impl` reaches the
//! state through the single `use runtime::*` glob in the parent, and the
//! `load` sibling reaches the state fields through their `pub` visibility.

// The moved `#[runtime] impl NativeActor for ComponentHostCapability` body
// names the `#[runtime]` attribute, the cap struct, the cap kinds (input +
// reply), and `ComponentHostConfig` (its `Config` type), which previously
// resolved at `mod.rs` root — now sourced here beside the body.
use aether_actor::{RegistryChanged, runtime};

// `load` (the `handle_load` sequence as a method on the state) and `config`
// (the `ComponentHostConfig` init bundle), now nested under this `runtime`
// directory so the one `mod runtime;` gate in the parent covers them (no
// per-sibling `#[cfg]`). The `load` impl reaches the state fields through their
// `pub` visibility, unchanged by the move.
mod config;
mod dependencies;
mod load;
mod placement;
mod publish;
mod republish;
mod spawn;
mod unpublish;

use super::{ComponentHostCapability, LoadResult};
use crate::component::{Abort, Aborted, Commit, Committed, LoadDelivered, Prepare, Prepared, SpawnDelivered};
// `ComponentHostParams` rides up to the cap root through this `pub use`: the
// cap-root `pub use runtime::ComponentHostParams;` re-export sources it here.
pub use self::config::ComponentHostParams;

use crate::kinds::Unpublished;
use aether_kinds::trace::Settled;
use aether_kinds::{
    DescribeComponent, DescribeComponentResult, DropComponent, DropResult, ListComponents, ListComponentsResult,
    LoadComponent, Publish, PublishResult, Spawn, SpawnResult, Unpublish, UnpublishResult,
};

// Crate-local wiring the `#[runtime] impl` handler bodies name (the
// `MailboxCategory` vocabulary) and the state struct — all used within this
// module. No sibling-cap imports: drop-time cleanup rides the ADR-0079
// close `MonitorNotice` (each cap monitors its registrants and purges its own
// rows), so the host names no peer cap's type or kinds.
use aether_actor::{Anyone, ErasedActorRef, ProtocolRef, Single};
use aether_data::ErasedActorPath;
use aether_data::{MailId, MailboxCategory};

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use aether_data::BlobHash;
use wasmtime::{Engine, Linker};

use aether_substrate::actor::native::{
    Erased, GuestOutcome, NativeActor, NativeCtx, NativeInitCtx, Pending, RegistryBatchResult, TaskDone,
};
use aether_substrate::actor::wasm::component::ComponentCtx;
use aether_substrate::actor::wasm::module::{Module, ModuleCache};
use aether_substrate::chassis::error::BootError;
use aether_substrate::mail::outbound::HubOutbound;
use aether_substrate::mail::registry::RegistrySubscription;

/// `aether.component` runtime state (ADR-0122 split). Holds the wasmtime
/// `engine` + `linker` every load instantiates against, the registry-inventory
/// subscription, and the `outbound` egress handle. Plain fields (no
/// `Arc<Inner>` wrapper) per ADR-0078 — the cap is single-threaded, every
/// handler runs on the cap's dispatcher thread. The host addresses no
/// sibling cap: drop-time registration cleanup rides the ADR-0079
/// close `MonitorNotice` fired from the trampoline, not host mail.
///
/// The dispatcher holds this as the cap's state and routes envelopes through
/// the macro-emitted `Dispatch` impl; the addressing identity is the distinct
/// ZST `ComponentHostCapability`. Living in this private module keeps it
/// `pub`-enough to satisfy the `NativeActor::State` interface without exposing
/// it as crate-public API. Fields carry `pub` so the
/// `load` submodule (which holds `handle_load`) can reach them as a sibling
/// within `crate::component`.
pub struct ComponentHostCapabilityState {
    pub engine: Arc<Engine>,
    pub linker: Arc<Linker<ComponentCtx>>,
    pub outbound: Arc<HubOutbound>,
    /// Retained registry-inventory subscription. `wire` installs the weak sink
    /// before the registry issues its initial wake, then this handle keeps that
    /// sink live for the host's lifetime.
    pub registry_subscription: Option<RegistrySubscription>,
    /// The coherent inventory generations most recently egressed to the hub.
    /// Mailbox and kind generations advance independently, so both form the
    /// idempotence key.
    pub last_egressed_inventory: Option<(u64, u64)>,
    /// The engine's one module cache (ADR-0240 D5, ADR-0241 §2), built here
    /// on the engine every load instantiates against and handed to every
    /// trampoline, so a load, a boot and a republish of the same bytes all
    /// share one compiled, parsed entry per content hash — a
    /// burst of loads of one artifact (`replicas: N`, a boot manifest naming
    /// several of its exports) pays cranelift once instead of once per load.
    /// Nothing is evicted by count or capacity — an entry lives only as long
    /// as some trampoline, in-flight load or republish, or staged boot plan
    /// still holds its `Module`.
    pub modules: ModuleCache,
    /// ADR-0147: the content hash (the ADR-0238 BLAKE3 hash of the wasm
    /// bytes, [`Module::hash`]) of every module whose boot has been born. A
    /// module that declares a `boot =` slot spawns its boot once, by its
    /// first load, and never again: the hash stays after the boot is dropped,
    /// so a later load of the module proceeds without one. Empty for every
    /// bootless module, so the common case costs nothing.
    booted_modules: HashSet<BlobHash>,
    /// Actor-local reservations for module boots that have been staged but are
    /// not authoritative `Live` yet. Same-hash loads wait here by their ids
    /// and join the first boot result.
    pending_boots: HashMap<BlobHash, load::PendingBoot>,
    /// Every load in flight (ADR-0243 §9): its held reply and prepared inputs,
    /// keyed by the id its staged work's contexts carry, from the staged
    /// module publish until the guest's birth answers it or hands it off. At
    /// host close the ledger answers each held reply `unanswered`.
    loads: HashMap<load::LoadId, load::LoadInFlight>,
    /// The next [`load::LoadId`] a load takes.
    next_load: u64,
    /// Every `Publish` whose first publish of a module is staged, keyed by
    /// the id its completion's context carries, until the owner answers it.
    publishes: HashMap<u64, publish::PublishInFlight>,
    /// The next id a staged `Publish` takes.
    next_publish: u64,
    /// Every `Unpublish` whose withdrawal batch is staged, keyed by the id
    /// its completion's context carries, until the owner answers it.
    unpublishes: HashMap<u64, unpublish::UnpublishInFlight>,
    /// The next id a staged `Unpublish` takes.
    next_unpublish: u64,
    /// Every published namespace of every module that declares a boot
    /// (ADR-0147), recorded when its publish commits. A republish of any of
    /// them is refused, whether or not an instance is live: a boot module is
    /// not replaceable.
    boot_namespaces: HashSet<String>,
    /// Every republish in flight (ADR-0241 §7), keyed by the id its prepare,
    /// commit and abort contexts carry: its held reply, its members and the
    /// loads and drops that wait for it to answer.
    republishes: HashMap<republish::RepublishId, republish::Republish>,
    /// The next [`republish::RepublishId`] a republish takes.
    next_republish: u64,
    /// Which republish in flight owns each namespace, so a second republish
    /// of it is refused and a load or spawn of it waits.
    republishing: HashMap<String, republish::RepublishId>,
    /// The root of each commit a republish sent on its own chain, keyed to
    /// the republish and member, until that chain's `Settled` arrives.
    commit_roots: HashMap<MailId, republish::CommitRoot>,
    /// Publishes whose namespaces had a spawn or a first publish in flight
    /// when they arrived, in arrival order. Each runs its pre-checks once
    /// none of its namespaces has.
    queued_publishes: Vec<republish::QueuedPublish>,
    /// Host-owned control proof of each successfully born guest, a module
    /// boot included, keyed by its erased reference: the [`GuestControl`]
    /// rows its trampoline serves. The guest's public receive surface
    /// deliberately replaces the trampoline's native surface, so an external
    /// path cannot recover this proof by casting after load. A drop removes
    /// its entry before handing off, since the handed-off drop closes the
    /// trampoline (ADR-0241 §8), so a second drop that arrives before its
    /// route reads `Dropped` finds no entry and is refused. The entries a
    /// republish's module exports are its members.
    drop_targets: HashMap<ErasedActorRef, LoadedGuest>,
}

/// A guest this host loaded or booted: the control proof it is dropped and
/// republished through, the published namespace it was born at, and the
/// module it runs. The module is written at birth and rewritten when a
/// republish of it commits.
pub struct LoadedGuest {
    control: ProtocolRef<GuestControl>,
    namespace: String,
    module: Module,
}

/// The host's own ctx, in either reply mode.
type HostCtx<'a, M> = NativeCtx<'a, ComponentHostCapability, Anyone, M>;

/// The rows the component host controls a guest through: its trampoline's
/// own framework rows, never the guest's published surface. A guest birth
/// completes with this proof (ADR-0241 §6), and the host hands a load's or
/// a spawn's reply off, forwards a drop, and drives a republish's prepare,
/// commit and abort (ADR-0241 §7) through it.
#[aether_actor::protocol]
trait GuestControl {
    fn load_delivered(mail: LoadDelivered) -> LoadResult;
    fn spawn_delivered(mail: SpawnDelivered) -> SpawnResult;
    fn drop_component(mail: DropComponent) -> DropResult;
    fn prepare(mail: Prepare) -> Prepared;
    fn commit(mail: Commit) -> Committed;
    fn abort(mail: Abort) -> Aborted;
}

#[runtime]
impl NativeActor for ComponentHostCapability {
    /// The runtime state this identity boots into (ADR-0122 split): the
    /// wasmtime instances, registry-inventory subscription, egress handle, and
    /// default-name counter every load instantiates against.
    type State = ComponentHostCapabilityState;

    type Config = ();
    type Params = ComponentHostParams;
    const NAMESPACE: &'static str = "aether.component";

    fn init(
        _config: (),
        params: ComponentHostParams,
        _ctx: &mut NativeInitCtx<'_>,
    ) -> Result<ComponentHostCapabilityState, BootError> {
        Ok(ComponentHostCapabilityState {
            modules: ModuleCache::new(Arc::clone(&params.engine)),
            engine: params.engine,
            linker: params.linker,
            outbound: params.hub_outbound,
            registry_subscription: None,
            last_egressed_inventory: None,
            booted_modules: HashSet::new(),
            pending_boots: HashMap::new(),
            loads: HashMap::new(),
            next_load: 0,
            publishes: HashMap::new(),
            next_publish: 0,
            unpublishes: HashMap::new(),
            next_unpublish: 0,
            boot_namespaces: HashSet::new(),
            republishes: HashMap::new(),
            next_republish: 0,
            republishing: HashMap::new(),
            commit_roots: HashMap::new(),
            queued_publishes: Vec::new(),
            drop_targets: HashMap::new(),
        })
    }

    fn wire(state: &mut Self::State, ctx: &mut NativeCtx<'_, Self>) -> Result<(), BootError> {
        state.registry_subscription = Some(ctx.subscribe_inventory());
        Ok(())
    }

    /// Load a wasm component into the substrate: a publish of its module,
    /// then a spawn of the selected type (ADR-0241 §9).
    ///
    /// # Agent
    /// Pass the wasm bytes plus an optional `name`. The cap publishes the
    /// module (ADR-0241 §3): admission checks its exported namespaces and
    /// their contracts, and the kinds the wasm declared in its `aether.kinds`
    /// section register. A module that succeeds the one publishing its
    /// namespaces republishes every live instance of them as one group first
    /// (§7), as `Publish` does. Then it spawns the selected type as a guest
    /// under its own published name (ADR-0241 §5): a singleton at `NS`, where
    /// a load that names any key is refused before the publish; an instanced
    /// type at `NS:name`, or `NS:<counter>` when the load names none. A name
    /// already live answers with that instance, which is not
    /// re-initialised. The guest itself replies `LoadResult::Ok { path,
    /// capabilities }`, where `path` is that name — agents send subsequent
    /// mail to that address, and an actor requester keeps the reply's
    /// stamped sender as its reference.
    /// Errors (bad wire bytes, a publish admission refuses, kind conflict,
    /// name conflict, invalid wasm, instantiation trap) come back from the
    /// host as `LoadResult::Err`.
    #[handler::request]
    fn on_load_component(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        payload: LoadComponent,
    ) -> Pending<LoadResult> {
        let (pending, held) = ctx.hold::<LoadResult>();
        state.begin_load(ctx, held, payload);
        pending
    }

    /// A module publish settled (ADR-0241 §3): a load's commit continues to
    /// the module boot and the requested guest, a `Publish`'s spawns the
    /// module's boot and answers, and a republish's commit sends every
    /// member its commit (§7). An unpublish's withdrawal (ADR-0250 §5)
    /// answers its held reply instead. A refusal answers the load, the
    /// `Publish`, or the `Unpublish`, or aborts every member of the
    /// republish.
    #[handler(task)]
    fn on_module_published(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_, Self, Anyone, Single>,
        done: TaskDone<RegistryBatchResult>,
    ) {
        // A withdrawal completes through the same owner batch output, so its
        // context answers the held unpublish here; every other context takes
        // the publish path it staged. A wrong-kind take leaves the context
        // stored, so this probe disturbs no publish completion.
        if let Some(unpublished) = ctx.take_context::<Unpublished>() {
            state.finish_unpublish(ctx, unpublished.unpublish, done.into_output());
        } else {
            state.finish_publish(ctx, done);
        }
        state.release_queued_publishes(ctx);
    }

    /// A staged guest birth settled (ADR-0241 §6): a module boot releases
    /// the spawns waiting on it, and a requested guest takes over its load's
    /// or spawn's held reply.
    #[handler(task)]
    fn on_guest_born(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_, Self, Anyone, Single>,
        done: TaskDone<GuestOutcome<GuestControl>>,
    ) {
        state.finish_guest_birth(ctx, done);
        state.release_queued_publishes(ctx);
    }

    /// Refresh the hub's registry projection after a coalesced publication.
    /// The registry owns publication and wake coalescing; this consumer reads
    /// one coherent snapshot, egresses it at most once per generation pair,
    /// and always acknowledges so a publication racing the clear is re-armed.
    #[handler::event]
    fn on_registry_changed(state: &mut Self::State, _ctx: &mut NativeCtx<'_, Erased>, _payload: RegistryChanged) {
        state.refresh_registry_inventory();
    }

    /// Drop a component by its actor path. Hands the drop to the addressed
    /// trampoline, whose `WasmTrampoline::on_drop_component` handler replies
    /// `DropResult::Ok` to the caller and closes (ADR-0241 §8): its name
    /// tombstones, and the close tail's `MonitorNotice` purges the mailbox
    /// from every sibling cap's fan-out / routing table — each cap monitors
    /// its registrants and drops its own rows on the notice, so the host
    /// mails no cap anything at drop time. A drop of an instance whose
    /// module is republishing waits until the republish answers, then runs
    /// against the instance the republish left (§7).
    ///
    /// # Agent
    /// `DropComponent { target }`. The `target` is the component's actor
    /// path, `LoadResult.path`: `NS`, `NS:key`, or `parent/NS:key`. A drop
    /// at a module boot closes it for good: the module's later loads spawn
    /// no new one (ADR-0147, ADR-0241 §8).
    #[handler::request]
    fn on_drop_component(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        payload: DropComponent,
    ) -> Pending<DropResult> {
        let (pending, held) = ctx.hold::<DropResult>();
        state.begin_drop(ctx, held, payload);
        pending
    }

    /// Publish a module: bind every namespace it exports to it (ADR-0241
    /// §3, §9).
    ///
    /// # Agent
    /// `Publish { code, configs }`, where `code` is the module's wasm bytes.
    /// Identical bytes answer `Ok` with nothing changed. A module none of
    /// whose namespaces is published binds them, and spawns the module's
    /// boot once when it declares one. A module that succeeds the one
    /// publishing its namespaces must export each of them, keep each one's
    /// handler rows and fallback, and declare no boot; it republishes every
    /// live instance of them as one group, or none (§7). `configs` lists
    /// `{ path, config }` for an instance whose type's config kind changed,
    /// which needs one; every other instance keeps its stored config. The
    /// reply is `PublishResult::Ok { types }`: each namespace as published
    /// (`NS.<hash>` for a content-addressed module) with its capabilities,
    /// the `namespace` a `Spawn` names. A republish answers once every
    /// commit's chain has settled.
    #[handler::request]
    fn on_publish(state: &mut Self::State, ctx: &mut NativeCtx<'_>, payload: Publish) -> Pending<PublishResult> {
        let (pending, held) = ctx.hold::<PublishResult>();
        state.begin_publish(ctx, held, payload);
        pending
    }

    /// Spawn an instance of a published type (ADR-0241 §9).
    ///
    /// # Agent
    /// `Spawn { namespace, key, parent, config, code }`, where `namespace` is a
    /// published name `PublishResult` reported. The name the instance takes
    /// decides the answer: `NS` for a singleton (which names no `key`),
    /// `NS:key` for an instanced type (`NS:<counter>` when `key` is `None`),
    /// or `parent/NS:key` beneath the live `parent`, whose type the spawned
    /// type must declare `child_of`. A live name answers
    /// `SpawnResult::Live { path, capabilities }` and nothing is
    /// re-initialised; an absent name stands the instance up with `config`
    /// and answers `Spawned`; a dropped name is spent and refused. The
    /// instance itself answers, so an actor requester keeps the reply's
    /// stamped sender as its reference. A namespace no module publishes is
    /// refused, and a spawn of one whose module is republishing waits until
    /// the republish answers. `code` is the published module's bytes, brought
    /// so the new instance reads its assets in `init` and `wire`; bytes of
    /// any other module are refused, and `None` stands up an instance whose
    /// load window serves no asset payload.
    #[handler::request]
    fn on_spawn(state: &mut Self::State, ctx: &mut NativeCtx<'_>, payload: Spawn) -> Pending<SpawnResult> {
        let (pending, held) = ctx.hold::<SpawnResult>();
        state.begin_spawn(ctx, held, payload);
        pending
    }

    /// Withdraw one published namespace (ADR-0250 §5).
    ///
    /// # Agent
    /// `Unpublish { namespace }`, where `namespace` is a published name a
    /// `PublishResult` reported. The namespace must have no live instance:
    /// drop its instances first. A namespace no module publishes, one native
    /// code implements, one with a publish or load in flight, or one still
    /// running an instance is refused with the reason. Reply:
    /// `UnpublishResult`.
    #[handler::request]
    fn on_unpublish(state: &mut Self::State, ctx: &mut NativeCtx<'_>, payload: Unpublish) -> Pending<UnpublishResult> {
        let (pending, held) = ctx.hold::<UnpublishResult>();
        state.begin_unpublish(ctx, held, payload);
        pending
    }

    /// A member answered its republish prepare (ADR-0241 §7).
    #[handler::response]
    fn on_prepared(state: &mut Self::State, ctx: &mut NativeCtx<'_>, payload: Prepared) {
        state.finish_prepare(ctx, payload);
    }

    /// A member installed its prepared candidate (ADR-0241 §7).
    #[handler::response]
    fn on_committed(state: &mut Self::State, ctx: &mut NativeCtx<'_>, _payload: Committed) {
        state.finish_commit(ctx);
    }

    /// A member reinstated its old guest (ADR-0241 §7).
    #[handler::response]
    fn on_aborted(state: &mut Self::State, ctx: &mut NativeCtx<'_>, _payload: Aborted) {
        state.finish_abort(ctx);
    }

    /// The chain a member's commit started has settled: the mail its
    /// candidate held and every chain that mail caused are done (ADR-0241
    /// §7). `Settled` notices for other roots are ignored.
    #[handler::event]
    fn on_commit_settled(state: &mut Self::State, ctx: &mut NativeCtx<'_>, payload: Settled) {
        state.settle_commit(ctx, payload.root);
    }

    /// Enumerate the components this engine has actually loaded and
    /// registered, by their ADR-0099 lineage names (issue 2020).
    ///
    /// Reads the registry's live mailbox snapshot — the same coherent
    /// inventory projected to the hub by `RegistryChanged` — and keeps only
    /// the [`MailboxCategory::Trampoline`] entries, the guests: routes whose
    /// namespace a published module implements, read from the publication
    /// table (ADR-0241 §3), and their inline children. Chassis caps are
    /// boot-present and static, so the guests are the only registry
    /// membership a readiness poll cares about. The
    /// reply is names only: the mailbox id is a deterministic hash-chain
    /// over the lineage the name renders (ADR-0099) and routing is the
    /// substrate's job, so the caller never needs the handle.
    ///
    /// # Agent
    /// Fieldless `ListComponents` to the `aether.component` mailbox —
    /// guaranteed present from boot, so the send always resolves and the
    /// reply is a definitive snapshot. Reply `ListComponentsResult {
    /// names }` lists every currently-loaded component's full lineage
    /// address (`NS`, `NS:key`, or `parent/NS:key`). Poll it after a
    /// boot-manifest spawn (ADR-0116) to learn deterministically when a
    /// requested component is loaded, instead of inferring liveness by
    /// proxy.
    #[handler::request]
    fn on_list_components(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        _payload: ListComponents,
    ) -> ListComponentsResult {
        let names = state
            .subscription()
            .inventory()
            .mailboxes
            .into_iter()
            .filter(|d| d.category == Some(MailboxCategory::Trampoline))
            .map(|d| d.name)
            .collect();
        ListComponentsResult { names }
    }

    /// Introspect one loaded component's ADR-0033 receive-side
    /// `ComponentCapabilities` by lineage `name` (iamacoffeepot/aether#2421).
    /// Proves `name` through the host's address resolution, then reads the
    /// full receive surface the substrate retains for the proven actor.
    ///
    /// # Agent
    /// `DescribeComponent { name }` to the `aether.component` mailbox, where
    /// `name` is the lineage address `ListComponents` / `LoadResult.path`
    /// hand back (`NS`, `NS:key`, or `parent/NS:key`), or a published
    /// namespace no live actor is named by, such as an instanced type's,
    /// which is answered from its published module. Reply `DescribeComponentResult::Ok
    /// { capabilities }` carries the full handler kinds, docs, fallback, and
    /// config kind; `Err { error }` means nothing is registered or published
    /// at that name.
    /// Name-addressed so a boot-manifest-loaded component (ADR-0116), whose
    /// spawner never receives a mailbox id, stays introspectable.
    #[handler::request]
    fn on_describe_component(
        _state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        payload: DescribeComponent,
    ) -> DescribeComponentResult {
        // The host's address resolution, not an exact-name lookup: a short
        // path that is ambiguous rather than absent reports its candidate
        // spellings instead of collapsing to "nothing registered" (ADR-0166
        // §5, issue 4125).
        let proven = ErasedActorPath::new(&payload.name)
            .map_err(|error| error.to_string())
            .and_then(|name| ctx.resolve_path(&name).map_err(|error| error.to_string()));
        let actor = match proven {
            Ok(actor) => actor,
            Err(error) => {
                // ADR-0241 §3: a published namespace no live actor is named
                // by answers from the module the table binds it to.
                let published = ctx.published_module(&payload.name).and_then(|module| {
                    publish::published_surfaces(&module).find(|(namespace, _)| *namespace == payload.name)
                });
                return published.map_or_else(
                    || DescribeComponentResult::Err {
                        error: format!("no component registered at name {}: {error}", payload.name),
                    },
                    |(_, capabilities)| DescribeComponentResult::Ok { capabilities },
                );
            }
        };
        match ctx.receive_surface(actor) {
            Some(capabilities) => DescribeComponentResult::Ok { capabilities },
            None => {
                DescribeComponentResult::Err { error: format!("no capabilities retained for name {}", payload.name) }
            }
        }
    }
}

impl ComponentHostCapabilityState {
    /// The registry-inventory subscription `wire` installs, which every
    /// handler runs after.
    fn subscription(&self) -> &RegistrySubscription {
        self.registry_subscription.as_ref().expect("component host registry subscription installed during wire")
    }

    fn refresh_registry_inventory(&mut self) {
        let inventory = self.subscription().inventory();
        let generations = (inventory.mailbox_generation, inventory.kind_generation);

        if self.last_egressed_inventory != Some(generations) {
            self.outbound.egress_kinds_changed(inventory.kinds);
            self.outbound.egress_mailboxes_changed(inventory.mailboxes);
            self.last_egressed_inventory = Some(generations);
        }

        self.subscription().acknowledge(generations.0, generations.1);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::mpsc;

    use aether_data::{KindDescriptor, MailboxDescriptor};
    use aether_substrate::chassis::ctx::MailboxWakeFn;
    use aether_substrate::mail::mailer::Mailer;
    use aether_substrate::mail::outbound::EgressEvent;
    use aether_substrate::mail::registry::{Registry, noop_handler};
    use aether_substrate::testing::{
        await_signal, boot_authority, boot_test_chassis_with, registered_ref, try_registered_ref,
    };

    use super::*;

    /// Await the next egress pair the host's `on_registry_changed` handler
    /// produces: `refresh_registry_inventory` always egresses kinds before
    /// mailboxes, each event firing its own wake right after it lands on
    /// `egress_rx` (`HubOutbound::attach_recording`'s contract), so one
    /// [`await_signal`] per event reads the pair in the prescribed order.
    fn next_pair(
        signal_rx: &crossbeam_channel::Receiver<()>,
        egress_rx: &mpsc::Receiver<EgressEvent>,
    ) -> (Vec<KindDescriptor>, Vec<MailboxDescriptor>) {
        await_signal(signal_rx, "component host egress: kinds");
        let kinds = match egress_rx.try_recv() {
            Ok(EgressEvent::KindsChanged { descriptors }) => descriptors,
            other => panic!("expected KindsChanged first, got {other:?}"),
        };

        await_signal(signal_rx, "component host egress: mailboxes");
        let mailboxes = match egress_rx.try_recv() {
            Ok(EgressEvent::MailboxesChanged { descriptors }) => descriptors,
            other => panic!("expected MailboxesChanged second, got {other:?}"),
        };

        (kinds, mailboxes)
    }

    /// [`next_pair`] repeated until the pair carries `name` (in `kinds` when
    /// `in_kinds`, else in `mailboxes`): the pool can service a registry
    /// publication that lands between the handler's inventory read and its
    /// `acknowledge` re-arm, so the mutation a step just made may take more
    /// than one pair to surface.
    fn next_pair_carrying(
        signal_rx: &crossbeam_channel::Receiver<()>,
        egress_rx: &mpsc::Receiver<EgressEvent>,
        name: &str,
        in_kinds: bool,
    ) -> (Vec<KindDescriptor>, Vec<MailboxDescriptor>) {
        loop {
            let pair = next_pair(signal_rx, egress_rx);
            let carries = if in_kinds {
                pair.0.iter().any(|d| d.name == name)
            } else {
                pair.1.iter().any(|d| d.name == name)
            };
            if carries {
                return pair;
            }
        }
    }

    /// The host's registry-inventory egress (`wire`'s `subscribe_inventory`
    /// through `on_registry_changed`'s `refresh_registry_inventory`) is
    /// change-driven and always a complete kinds-then-mailboxes pair: booting
    /// the real host on a pooled chassis, rather than hand-building its
    /// `NativeCtx` and calling `refresh_registry_inventory` as a plain
    /// function, exercises `wire`, the `RegistryChanged` wake, the
    /// macro-generated dispatch arm, and the handler-end flush together.
    #[test]
    fn registry_inventory_egress_is_complete_and_change_driven() {
        let registry = Arc::new(Registry::new());
        let outbound = HubOutbound::disconnected();
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry)).with_outbound(Arc::clone(&outbound)));

        let (signal_tx, signal_rx) = crossbeam_channel::unbounded();
        let wake: MailboxWakeFn = Arc::new(move || {
            let _ = signal_tx.send(());
        });
        let egress_rx = outbound.attach_recording(Some(wake));

        let engine = Arc::new(Engine::default());
        let params = ComponentHostParams {
            engine: Arc::clone(&engine),
            linker: Arc::new(Linker::new(&engine)),
            hub_outbound: Arc::clone(&outbound),
        };
        let _chassis = boot_test_chassis_with::<ComponentHostCapability>(&registry, &mailer, (), params);

        // The initial wake `wire`'s `subscribe_inventory` arms egresses a
        // complete pair in the prescribed order, with no kind registered yet
        // and the boot-time mailboxes only.
        let (initial_kinds, initial_mailboxes) = next_pair(&signal_rx, &egress_rx);
        assert!(initial_kinds.is_empty(), "no kind is registered before the test's own registrations");
        assert!(
            initial_mailboxes.iter().any(|d| d.name == "aether.component"),
            "the booted host's own mailbox is in the initial inventory: {initial_mailboxes:?}"
        );

        // A kind-only publication refreshes both complete inventories.
        registry.register_kind(&boot_authority(), "test.component.inventory.kind");
        let (kinds, mailboxes) = next_pair_carrying(&signal_rx, &egress_rx, "test.component.inventory.kind", true);
        assert!(kinds.iter().any(|d| d.name == "test.component.inventory.kind"));
        assert!(mailboxes.iter().any(|d| d.name == "aether.component"));

        // A mailbox-only publication refreshes both complete inventories
        // too, and keeps the earlier kind.
        registered_ref(&registry, "test.component.inventory.mailbox", noop_handler());
        let (kinds, mailboxes) = next_pair_carrying(&signal_rx, &egress_rx, "test.component.inventory.mailbox", false);
        assert!(kinds.iter().any(|d| d.name == "test.component.inventory.kind"));
        assert!(mailboxes.iter().any(|d| d.name == "test.component.inventory.mailbox"));

        // A rejected duplicate mailbox claim causes no egress of its own;
        // fenced by the next real kind registration, whose pair — the first
        // to carry the new kind — lists the claimed mailbox name exactly
        // once, never doubled by the rejected claim.
        assert!(try_registered_ref(&registry, "test.component.inventory.mailbox", noop_handler()).is_err());
        registry.register_kind(&boot_authority(), "test.component.inventory.fence");
        let (kinds, mailboxes) = next_pair_carrying(&signal_rx, &egress_rx, "test.component.inventory.fence", true);
        assert!(kinds.iter().any(|d| d.name == "test.component.inventory.kind"));
        assert!(kinds.iter().any(|d| d.name == "test.component.inventory.fence"));
        assert!(mailboxes.iter().any(|d| d.name == "aether.component"));
        assert_eq!(
            mailboxes.iter().filter(|d| d.name == "test.component.inventory.mailbox").count(),
            1,
            "the rejected duplicate claim must not double the mailbox's inventory entry: {mailboxes:?}"
        );
    }
}
