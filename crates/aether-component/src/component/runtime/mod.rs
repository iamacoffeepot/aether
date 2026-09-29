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

use super::{ComponentHostCapability, LoadResult};
use crate::component::LoadDelivered;
// `ComponentHostParams` rides up to the cap root through this `pub use`: the
// cap-root `pub use runtime::ComponentHostParams;` re-export sources it here.
pub use self::config::ComponentHostParams;
// The trampoline checks a replacement's hosted type through this re-export.
pub use self::dependencies::replacement_refusal;

use aether_kinds::{
    DescribeComponent, DescribeComponentResult, DropComponent, DropResult, ListComponents, ListComponentsResult,
    LoadComponent, LoadComponentUnder, ReplaceComponent, ReplaceResult,
};

pub use aether_actor::Manual;

// Crate-local wiring the `#[runtime] impl` handler bodies name (the
// `MailboxCategory` vocabulary) and the state struct — all used within this
// module. No sibling-cap imports: drop-time cleanup rides the ADR-0079
// close `MonitorNotice` (each cap monitors its registrants and purges its own
// rows), so the host names no peer cap's type or kinds.
use aether_actor::{ErasedActorRef, OutboundReply, ProtocolRef, Single};
use aether_data::ErasedActorPath;
use aether_data::{MailboxCategory, Source};

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
    /// trampoline, so a load, a boot and a replace of the same bytes all
    /// share one compiled, parsed entry per content hash — a
    /// burst of loads of one artifact (`replicas: N`, a boot manifest naming
    /// several of its exports) pays cranelift once instead of once per load.
    /// Nothing is evicted by count or capacity — an entry lives only as long
    /// as some trampoline, in-flight load or replace, or staged boot plan
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
    /// In-flight `aether.component.replace` forwards awaiting their
    /// trampoline `ReplaceResult`, keyed by the forward's correlation id: the
    /// caller's reply target and the replacement module are parked here
    /// across the hop. Empty except while a replace is settling.
    pub pending_replace: HashMap<u64, PendingReplace>,
    /// Host-owned control proof of each successfully born guest, a module
    /// boot included, keyed by its erased reference: the [`GuestControl`]
    /// rows its trampoline serves. The guest's public receive surface
    /// deliberately replaces the trampoline's native surface, so an external
    /// path cannot recover this proof by casting after load. A drop removes
    /// its entry before forwarding, since the forwarded drop closes the
    /// trampoline (ADR-0241 §8), so a replace or a second drop that arrives
    /// before its route reads `Dropped` finds no entry and is refused.
    drop_targets: HashMap<ErasedActorRef, LoadedGuest>,
}

/// A parked `aether.component.replace` forward. `source` is the original
/// caller's reply target (the trampoline's `ReplaceResult` is routed to the
/// cap instead, then re-replied here). Holding `module` across the hop keeps
/// its cache entry live, so the trampoline's own check-in of the forwarded
/// bytes is a hit.
pub struct PendingReplace {
    pub source: Source,
    pub module: Module,
}

/// A guest this host loaded or booted: the control proof it is dropped
/// through, and whether its module declares a boot. The flag is set at birth
/// from the module manifest and never changes, because a replace can neither
/// add nor remove a boot (ADR-0147): a guest from a boot module is not
/// replaceable.
pub struct LoadedGuest {
    control: ProtocolRef<GuestControl>,
    from_boot_module: bool,
}

/// The rows the component host controls a guest through: its trampoline's
/// own framework rows, never the guest's published surface. A guest birth
/// completes with this proof (ADR-0241 §6), and the host hands a load's
/// reply off and forwards a drop through it.
#[aether_actor::protocol]
trait GuestControl {
    fn load_delivered(mail: LoadDelivered) -> LoadResult;
    fn drop_component(mail: DropComponent) -> DropResult;
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
            pending_replace: HashMap::new(),
            drop_targets: HashMap::new(),
        })
    }

    fn wire(state: &mut Self::State, ctx: &mut NativeCtx<'_, Self>) {
        state.registry_subscription = Some(ctx.subscribe_inventory());
    }

    /// Load a fresh wasm component into the substrate.
    ///
    /// # Agent
    /// Pass the wasm bytes plus an optional `name`. The cap publishes the
    /// module (ADR-0241 §3): admission checks its exported namespaces and
    /// their contracts, and the kinds the wasm declared in its `aether.kinds`
    /// section register. On Ok it spawns the selected type as a guest under
    /// its own published name (ADR-0241 §5): a singleton at `NS`, where a
    /// load that names any key is refused before the publish; an instanced
    /// type at `NS:name`, or `NS:<counter>` when the load names none. The
    /// loaded guest itself replies `LoadResult::Ok { path, capabilities }`,
    /// where `path` is that name — agents send subsequent mail to that
    /// address, and an actor requester keeps the reply's stamped sender as
    /// its reference.
    /// Errors (bad wire bytes, a publish admission refuses, kind conflict,
    /// name conflict, invalid wasm, instantiation trap) come back from the
    /// host as `LoadResult::Err`.
    #[handler::single]
    fn on_load_component(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        payload: LoadComponent,
    ) -> Pending<LoadResult> {
        let (pending, held) = ctx.hold::<LoadResult>();
        state.begin_load(ctx, held, payload);
        pending
    }

    /// Load a component beneath a live parent, at `parent/NS:key`. The
    /// selected type must declare `child_of` the parent's type (ADR-0241
    /// §5); any other placement is refused before the module publishes.
    /// Its `LoadResult` is held and answered the same way `on_load_component`'s is.
    #[handler::single]
    fn on_load_component_under(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        payload: LoadComponentUnder,
    ) -> Pending<LoadResult> {
        let (pending, held) = ctx.hold::<LoadResult>();
        state.begin_load_under(ctx, held, payload);
        pending
    }

    /// A load's module publish settled (ADR-0241 §3): a commit continues to
    /// the module boot and the requested guest, and a refusal answers the
    /// caller.
    #[handler(task)]
    fn on_load_published(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_, Self, Single>,
        done: TaskDone<RegistryBatchResult>,
    ) {
        state.finish_load_publish(ctx, done);
    }

    /// A replace's module publish settled (ADR-0241 §4): a commit forwards
    /// the replace to its trampoline, and a refusal answers the caller.
    #[handler(task)]
    fn on_replace_published(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_, Self, Single>,
        done: TaskDone<RegistryBatchResult, load::ReplacePublication>,
    ) {
        state.finish_replace_publish(ctx, done);
    }

    /// A staged guest birth settled (ADR-0241 §6): a module boot releases
    /// the loads waiting on it, and a requested guest takes over its load's
    /// held reply.
    #[handler(task)]
    fn on_guest_born(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_, Self, Single>,
        done: TaskDone<GuestOutcome<GuestControl>>,
    ) {
        state.finish_guest_birth(ctx, done);
    }

    /// Refresh the hub's registry projection after a coalesced publication.
    /// The registry owns publication and wake coalescing; this consumer reads
    /// one coherent snapshot, egresses it at most once per generation pair,
    /// and always acknowledges so a publication racing the clear is re-armed.
    #[handler::manual]
    fn on_registry_changed(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_, Erased, Manual>,
        _payload: RegistryChanged,
    ) {
        state.refresh_registry_inventory();
    }

    /// Drop a component by its actor path. Forwards
    /// [`DropComponent`] mail to the addressed trampoline; the
    /// trampoline's `WasmTrampoline::on_drop_component` handler
    /// replies `DropResult::Ok` and closes (ADR-0241 §8): its name
    /// tombstones, and the close tail's `MonitorNotice` purges the mailbox
    /// from every sibling cap's fan-out / routing table — each cap monitors
    /// its registrants and drops its own rows on the notice, so the host
    /// mails no cap anything at drop time.
    ///
    /// # Agent
    /// `DropComponent { target }`. The `target` is the component's actor
    /// path, `LoadResult.path`: `NS`, `NS:key`, or `parent/NS:key`. A drop
    /// at a module boot closes it for good: the module's later loads spawn
    /// no new one (ADR-0147, ADR-0241 §8).
    #[handler::manual]
    fn on_drop_component(state: &mut Self::State, ctx: &mut NativeCtx<'_, Erased, Manual>, payload: DropComponent) {
        // ADR-0230: prove the address at receipt. An address with no live
        // route has no trampoline to drop, so it answers `Err` now rather
        // than forwarding into nothing; no position leaves the verb.
        let actor = match ctx.resolve_path(&payload.target) {
            Ok(proven) => proven,
            Err(error) => {
                ctx.reply(&DropResult::Err { error: format!("no component to drop at {}: {error}", payload.target) });
                return;
            }
        };
        // The drop closes the trampoline, so its entry leaves now: a second
        // drop or a replace that proves the path before the owner applies its
        // `Dropped` route finds no entry and is refused.
        let Some(guest) = state.drop_targets.remove(&actor) else {
            ctx.reply(&DropResult::Err { error: format!("no live component to drop at {}", payload.target) });
            return;
        };
        // The forward inherits this call's chain, so the call stays open until
        // the trampoline's deferred reply lands at the original caller.
        ctx.forward_to(guest.control, &payload);
    }

    /// Replace the component at `target` with a fresh wasm
    /// binary. Forwards [`ReplaceComponent`] to the trampoline;
    /// the trampoline's `WasmTrampoline::on_replace_component`
    /// handler swaps `Component` internally and replies
    /// `ReplaceResult` to this host, which answers the held reply from
    /// `on_replace_result`; an early refusal answers it here.
    /// ADR-0022 + ADR-0038 splice invariants hold because the inbox
    /// channel is the trampoline's `NativeBinding`, which outlives the
    /// swap.
    ///
    /// # Agent
    /// `ReplaceComponent { target, wasm, drain_timeout_ms, config, export }`,
    /// where `target` is the component's actor path, `LoadResult.path`.
    /// `drain_timeout_ms` is accepted for wire compatibility but
    /// ignored under the trampoline's binding-stable replace.
    /// `export` (ADR-0096) names which exported actor type of the
    /// replacement module to instantiate; `None` reuses the type the
    /// trampoline currently hosts. A module that declares a boot, in its
    /// live or its replacement version, is not replaceable: it upgrades by
    /// engine restart (ADR-0147).
    #[handler::single]
    fn on_replace_component(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        payload: ReplaceComponent,
    ) -> Pending<ReplaceResult> {
        // ADR-0241 §4: the replacement module publishes first, so admission
        // refuses a republish that drops a namespace or narrows a contract
        // before the trampoline is touched, and the replacement's kinds
        // register. ADR-0147: a module whose live or replacement version
        // declares a boot is refused before anything publishes. Once the
        // publish commits, the replace is forwarded to the trampoline, whose
        // `ReplaceResult` comes back to this cap (`on_replace_result`).
        let (pending, held) = ctx.hold::<ReplaceResult>();
        state.begin_replace(ctx, held, payload);
        pending
    }

    /// Settle a forwarded `aether.component.replace`: `finish_replace`
    /// re-replies the trampoline's `ReplaceResult` to the original caller.
    #[handler::manual]
    fn on_replace_result(state: &mut Self::State, ctx: &mut NativeCtx<'_, Self, Manual>, payload: ReplaceResult) {
        state.finish_replace(ctx, payload);
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
    #[handler::single]
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
    /// hand back (`NS`, `NS:key`, or `parent/NS:key`). Reply `DescribeComponentResult::Ok
    /// { capabilities }` carries the full handler kinds, docs, fallback, and
    /// config kind; `Err { error }` means nothing is registered at that name.
    /// Name-addressed so a boot-manifest-loaded component (ADR-0116), whose
    /// spawner never receives a mailbox id, stays introspectable.
    #[handler::single]
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
                return DescribeComponentResult::Err {
                    error: format!("no component registered at name {}: {error}", payload.name),
                };
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

    use aether_substrate::mail::mailer::Mailer;
    use aether_substrate::mail::outbound::EgressEvent;
    use aether_substrate::mail::registry::{Registry, noop_handler};
    use aether_substrate::testing::{boot_authority, registered_binding, registered_ref, try_registered_ref};

    use super::*;

    #[test]
    fn registry_inventory_refresh_is_complete_idempotent_and_generation_gated() {
        let registry = Arc::new(Registry::new());
        let (outbound, rx) = HubOutbound::attached_loopback();
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry)).with_outbound(Arc::clone(&outbound)));
        let engine = Arc::new(Engine::default());
        let (binding, _subscriber) =
            registered_binding(&registry, &mailer, "test.component.inventory-subscriber", noop_handler());
        let mut state = ComponentHostCapabilityState {
            linker: Arc::new(Linker::new(&engine)),
            modules: ModuleCache::new(Arc::clone(&engine)),
            engine,
            outbound,
            registry_subscription: Some(
                NativeCtx::<ComponentHostCapability>::new_for_actor(&binding, Source::NONE, None, None)
                    .subscribe_inventory(),
            ),
            last_egressed_inventory: None,
            booted_modules: HashSet::new(),
            pending_boots: HashMap::new(),
            loads: HashMap::new(),
            next_load: 0,
            pending_replace: HashMap::new(),
            drop_targets: HashMap::new(),
        };

        // Initial wake refreshes both complete inventories in the prescribed
        // kinds-then-mailboxes order.
        state.refresh_registry_inventory();
        assert!(matches!(rx.try_recv(), Ok(EgressEvent::KindsChanged { .. })));
        let initial_mailbox_count = match rx.try_recv() {
            Ok(EgressEvent::MailboxesChanged { descriptors }) => descriptors.len(),
            other => panic!("expected initial mailbox inventory, got {other:?}"),
        };
        assert!(rx.try_recv().is_err());

        // A kind-only publication and a mailbox-only publication each refresh
        // the entire coherent projection. The #4062 registry tests cover the
        // producer's coalescing and publish-vs-clear re-arm internals.
        registry.register_kind(&boot_authority(), "test.component.inventory.kind");
        state.refresh_registry_inventory();
        assert!(matches!(rx.try_recv(), Ok(EgressEvent::KindsChanged { descriptors }) if descriptors.len() == 1));
        assert!(
            matches!(rx.try_recv(), Ok(EgressEvent::MailboxesChanged { descriptors }) if descriptors.len() == initial_mailbox_count)
        );

        registered_ref(&registry, "test.component.inventory.mailbox", noop_handler());
        state.refresh_registry_inventory();
        assert!(matches!(rx.try_recv(), Ok(EgressEvent::KindsChanged { descriptors }) if descriptors.len() == 1));
        assert!(
            matches!(rx.try_recv(), Ok(EgressEvent::MailboxesChanged { descriptors }) if descriptors.len() == initial_mailbox_count + 1)
        );

        // Bursts collapse to their latest inventory. An unchanged wake and a
        // rejected mutation cannot cause another egress.
        registry.register_kind(&boot_authority(), "test.component.inventory.burst-first");
        registry.register_kind(&boot_authority(), "test.component.inventory.burst-latest");
        state.refresh_registry_inventory();
        assert!(matches!(rx.try_recv(), Ok(EgressEvent::KindsChanged { descriptors }) if descriptors.len() == 3));
        assert!(
            matches!(rx.try_recv(), Ok(EgressEvent::MailboxesChanged { descriptors }) if descriptors.len() == initial_mailbox_count + 1)
        );
        assert!(try_registered_ref(&registry, "test.component.inventory.mailbox", noop_handler()).is_err());
        state.refresh_registry_inventory();
        assert!(rx.try_recv().is_err());
        state.refresh_registry_inventory();
        assert!(rx.try_recv().is_err());
    }
}
