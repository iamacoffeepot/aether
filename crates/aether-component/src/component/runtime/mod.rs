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
mod module_cache;

use self::module_cache::ModuleCache;
use super::{ComponentHostCapability, LoadResult};
use crate::trampoline::WasmTrampoline;
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
// vacate/close `MonitorNotice` (each cap monitors its registrants and purges
// its own rows), so the host names no peer cap's type or kinds.
use aether_actor::{ErasedActorRef, OutboundReply, Single};
use aether_data::ActorPath;
use aether_data::{MailboxCategory, Source};

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use wasmtime::{Engine, Linker};

use aether_substrate::actor::native::{
    Erased, NativeActor, NativeCtx, NativeInitCtx, RegistryBatchResult, SpawnOutcome, TaskDone,
};
use aether_substrate::actor::wasm::component::ComponentCtx;
use aether_substrate::chassis::error::BootError;
use aether_substrate::mail::mailer::Mailer;
use aether_substrate::mail::outbound::HubOutbound;
use aether_substrate::mail::registry::{Registry, RegistrySubscription};

/// `aether.component` runtime state (ADR-0122 split). Holds the wasmtime
/// `engine` + `linker` every load instantiates against, the mail `registry`,
/// the `mailer` / `outbound` egress handles, and the monotonic
/// `default_name_counter` for `component_N` default names. Plain fields (no
/// `Arc<Inner>` wrapper) per ADR-0078 — the cap is single-threaded, every
/// handler runs on the cap's dispatcher thread. The host addresses no
/// sibling cap: drop-time registration cleanup rides the ADR-0079
/// vacate/close `MonitorNotice` fired from the trampoline, not host mail.
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
    pub registry: Arc<Registry>,
    pub mailer: Arc<Mailer>,
    pub outbound: Arc<HubOutbound>,
    /// Retained registry-inventory subscription. `wire` installs the weak sink
    /// before the registry issues its initial wake, then this handle keeps that
    /// sink live for the host's lifetime.
    pub registry_subscription: Option<RegistrySubscription>,
    /// The coherent inventory generations most recently egressed to the hub.
    /// Mailbox and kind generations advance independently, so both form the
    /// idempotence key.
    pub last_egressed_inventory: Option<(u64, u64)>,
    /// Monotonic counter for `component_N` default names when an agent passes
    /// `name: None` and the wasm doesn't declare an `aether.namespace`.
    pub default_name_counter: u64,
    /// The most recently compiled module, so a burst of loads of one artifact
    /// (`replicas: N`, a boot manifest naming several of its exports) pays
    /// cranelift once instead of once per load.
    pub module_cache: ModuleCache,
    /// ADR-0147 module-boot bookkeeping: content hash (sha256 hex of the wasm
    /// bytes) → the module's boot singleton. A module that declares a `boot =`
    /// slot instantiates exactly one boot actor per `(engine, content hash)`;
    /// this table is the per-engine half of that pairing (the state itself is
    /// the per-substrate-process singleton every load runs through). Refcounted
    /// against the module's non-boot actors and empty for every bootless module,
    /// so the common case costs nothing. Changed only through
    /// `register_boot` / `unregister_boot`, which keep [`Self::boot_actors`]
    /// in lockstep.
    boot_registry: HashMap<String, BootEntry>,
    /// ADR-0147: every live module boot's reference — the reverse index of
    /// [`Self::boot_registry`], so the drop guard refusing a drop addressed at
    /// a boot actor is one lookup rather than a scan over every module.
    boot_actors: HashSet<ErasedActorRef>,
    /// Actor-local reservations for module boots that have been staged but are
    /// not authoritative `Live` yet. Same-hash loads and replacements retain
    /// their own move-only deferred replies here and join the first boot result.
    pending_boots: HashMap<String, load::PendingBoot>,
    /// ADR-0147: a loaded non-boot actor → the content hash of the module it
    /// came from. The key is the actor's proof, taken from its spawn outcome or
    /// proven once at the receipt of a drop / replace (ADR-0230). Populated only
    /// for actors sourced from a module that declares a boot slot, so a drop /
    /// replace can find and decrement the right boot refcount. A bootless module
    /// inserts nothing.
    pub boot_hash_by_actor: HashMap<ErasedActorRef, String>,
    /// ADR-0147: in-flight `aether.component.replace` forwards awaiting their
    /// trampoline `ReplaceResult`, keyed by the forward's correlation id. The
    /// boot-refcount transfer for a replace is committed only after the swap
    /// succeeds (`finish_replace`), so the caller's reply target and the
    /// replacement wasm are parked here across the hop. Empty except while a
    /// replace is settling.
    pub pending_replace: HashMap<u64, PendingReplace>,
    /// Last replace/drop operation sequence allocated for each actor, keyed by
    /// the proof taken at the drop / replace receipt. A replace reserves its
    /// sequence when forwarded; a drop reserves the next sequence and
    /// immediately makes it dominant. A proof compares by the position it
    /// proves, so entries survive the deterministic mailbox id's drop/reload
    /// boundary and an older incarnation can never become current again.
    pub boot_operation_sequence_by_actor: HashMap<ErasedActorRef, u64>,
    /// Latest successful replacement or drop operation that is allowed to
    /// mutate each actor's boot mapping, keyed like the sequence table. Failed
    /// replacements never enter this table, so they cannot suppress an earlier
    /// successful replacement.
    pub dominant_boot_operation_by_actor: HashMap<ErasedActorRef, u64>,
}

/// ADR-0147: a parked `aether.component.replace` forward. `source` is the
/// original caller's reply target (the trampoline's `ReplaceResult` is routed
/// to the cap instead, then re-replied here); `actor` — the target proven at
/// the replace's receipt — and `new_wasm` are what `commit_replacement_boot`
/// needs to commit the boot-refcount transfer once the swap is confirmed
/// successful. `boot_operation` is
/// reserved when the request is forwarded; it becomes dominant only if that
/// request succeeds, so a later failed request cannot suppress this one.
#[derive(Clone)]
pub struct PendingReplace {
    pub source: Source,
    pub actor: ErasedActorRef,
    pub new_wasm: Arc<[u8]>,
    pub boot_operation: u64,
}

/// ADR-0147: one module's boot singleton. `boot` is the boot trampoline's
/// proven reference, taken from its spawn outcome (spawned through the same
/// `WasmTrampoline` path as any export), which the teardown sends
/// [`BootTeardown`](crate::kinds::BootTeardown) through;
/// `refcount` counts the module's live **non-boot** actors — boot never counts
/// itself, so its own drop could never be the one that zeroes the count. The
/// `pending_requests` counts requested actors whose trampoline birth has been
/// accepted but has not yet promoted or rejected. The boot is torn down only
/// when both counters are zero: a pending birth keeps a temporarily
/// zero-refcount boot alive, and its later rejection performs the final
/// orphan check.
pub struct BootEntry {
    pub boot: ErasedActorRef,
    pub refcount: u32,
    pub pending_requests: u32,
}

#[runtime]
impl NativeActor for ComponentHostCapability {
    /// The runtime state this identity boots into (ADR-0122 split): the
    /// wasmtime instances, mail registry, egress handles, and default-name
    /// counter every load instantiates against.
    type State = ComponentHostCapabilityState;

    type Config = ();
    type Params = ComponentHostParams;
    const NAMESPACE: &'static str = "aether.component";

    fn init(
        _config: (),
        params: ComponentHostParams,
        ctx: &mut NativeInitCtx<'_>,
    ) -> Result<ComponentHostCapabilityState, BootError> {
        let mailer = ctx.mailer();
        let registry = Arc::clone(mailer.registry());
        Ok(ComponentHostCapabilityState {
            engine: params.engine,
            linker: params.linker,
            registry,
            mailer,
            outbound: params.hub_outbound,
            registry_subscription: None,
            last_egressed_inventory: None,
            default_name_counter: 0,
            module_cache: ModuleCache::default(),
            boot_registry: HashMap::new(),
            boot_actors: HashSet::new(),
            pending_boots: HashMap::new(),
            boot_hash_by_actor: HashMap::new(),
            pending_replace: HashMap::new(),
            boot_operation_sequence_by_actor: HashMap::new(),
            dominant_boot_operation_by_actor: HashMap::new(),
        })
    }

    fn wire(state: &mut Self::State, ctx: &mut NativeCtx<'_>) {
        state.registry_subscription = Some(
            state.registry.subscribe_inventory::<ComponentHostCapability>(ctx.self_id(), Arc::clone(&state.mailer)),
        );
    }

    /// Load a fresh wasm component into the substrate.
    ///
    /// # Agent
    /// Pass the wasm bytes plus an optional `name`. On Ok the cap
    /// registers the kinds the wasm declared in its `aether.kinds`
    /// section, picks a final name (caller value > wasm's
    /// `aether.namespace` > `component_N`), spawns a
    /// [`WasmTrampoline`] under
    /// `aether.embedded:NAME`, and hands the trampoline the owed reply: the
    /// loaded trampoline itself replies `LoadResult::Ok { path, capabilities }`,
    /// where `path` is its full lineage address — agents send subsequent mail
    /// to that address, and an actor requester keeps the reply's stamped
    /// sender as its reference.
    /// Errors (bad wire bytes, kind conflict, name conflict,
    /// invalid wasm, instantiation trap) come back from the host as
    /// `LoadResult::Err`.
    #[handler::manual]
    fn on_load_component(state: &mut Self::State, ctx: &mut NativeCtx<'_, Erased, Manual>, payload: LoadComponent) {
        state.begin_load(ctx, payload);
    }

    /// Load a component beneath a caller-selected live logical parent for a
    /// `SubstrateHarness` composition scenario. Ordinary `LoadComponent`
    /// continues to place the requested trampoline beneath this component
    /// host; this handler is the explicit test-harness seam for nested peers.
    #[handler::manual]
    fn on_load_component_under(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_, Erased, Manual>,
        payload: LoadComponentUnder,
    ) {
        state.begin_load_under(ctx, payload);
    }

    #[handler(task)]
    fn on_kind_registration_done(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_, Self, Single>,
        done: TaskDone<RegistryBatchResult, load::KindRegistration>,
    ) {
        state.finish_kind_registration(ctx, done);
    }

    #[handler(task)]
    fn on_component_spawn_done(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_, Self, Single>,
        done: TaskDone<SpawnOutcome<WasmTrampoline>, load::SpawnContext>,
    ) {
        state.finish_spawn(ctx, done);
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
    /// replies `DropResult::Ok` and vacates its mailbox (ADR-0079 §8
    /// amended), which is what purges the mailbox from every sibling
    /// cap's fan-out / routing table — each cap monitors its
    /// registrants and drops its own rows on the `MonitorNotice`, so
    /// the host mails no cap anything at drop time.
    ///
    /// # Agent
    /// `DropComponent { target }`. The `target` is the component's actor
    /// path, canonical (`LoadResult.path`) or short (`aether.component/:NAME`).
    #[handler::manual]
    fn on_drop_component(state: &mut Self::State, ctx: &mut NativeCtx<'_, Erased, Manual>, payload: DropComponent) {
        // ADR-0230: parse the address with the host's boundary parser and
        // prove the answer at once. An address with no live route has no
        // trampoline to drop, so it answers `Err` now rather than forwarding
        // into nothing; the position is never kept.
        let proven = state
            .registry
            .resolve_address(&payload.target)
            .map_err(|error| error.to_string())
            .and_then(|resolved| ctx.resolve_live(resolved.mailbox_id).map_err(|error| error.to_string()));
        let actor = match proven {
            Ok(proven) => proven,
            Err(error) => {
                ctx.reply(&DropResult::Err { error: format!("no component to drop at {}: {error}", payload.target) });
                return;
            }
        };
        // ADR-0147 non-droppability guard: the boot actor is unconditional and
        // refcounted against its module's non-boot actors, so an external drop
        // addressed straight at a boot actor must be rejected — letting it
        // through would tear the boot down out from under the refcount and leave
        // a dangling `boot_registry` entry. The boot is torn down automatically
        // (internally, through `release_boot_ref`) when its last non-boot actor
        // unloads; that internal path is not routed through this handler, so the
        // guard never blocks it. The guard runs after the receipt proof because
        // a boot entry's actor is live for as long as the entry exists, so the
        // proof succeeds for it and the comparison is by reference.
        if state.boot_actors.contains(&actor) {
            ctx.reply(&DropResult::Err {
                error: format!(
                    "{} is a module boot actor (ADR-0147): the boot singleton is unconditional \
                     and refcounted against its module's non-boot actors, so it cannot be dropped directly — \
                     drop the module's non-boot actors and the boot is torn down when the last one unloads",
                    payload.target
                ),
            });
            return;
        }
        // ADR-0147: account this actor's departure against its module's boot
        // singleton before forwarding the drop — the last non-boot actor from a
        // boot-bearing module tears the boot down here (the boot trampoline's
        // `BootTeardown` handler unloads its guest and vacates its
        // registrations).
        state.invalidate_replacement_boot_operation(actor);
        state.release_boot_ref(ctx, actor);
        // The forward inherits this call's chain, so the call stays open until
        // the trampoline's deferred reply lands at the original caller.
        ctx.forward_to(&actor, &payload);
    }

    /// Replace the component at `target` with a fresh wasm
    /// binary. Forwards [`ReplaceComponent`] to the trampoline;
    /// the trampoline's `WasmTrampoline::on_replace_component`
    /// handler swaps `Component` internally and replies
    /// `ReplaceResult`. ADR-0022 + ADR-0038 splice invariants
    /// hold because the inbox channel is the trampoline's
    /// `NativeBinding`, which outlives the swap.
    ///
    /// # Agent
    /// `ReplaceComponent { target, wasm, drain_timeout_ms, config, export }`,
    /// where `target` is the component's canonical or short actor path.
    /// `drain_timeout_ms` is accepted for wire compatibility but
    /// ignored under the trampoline's binding-stable replace.
    /// `export` (ADR-0096) names which exported actor type of the
    /// replacement module to instantiate; `None` reuses the type the
    /// trampoline currently hosts.
    #[handler::single]
    fn on_replace_component(state: &mut Self::State, ctx: &mut NativeCtx<'_>, payload: ReplaceComponent) {
        // ADR-0147: forward the replace to the trampoline but intercept its
        // `ReplaceResult` at this cap (`begin_replace`), so the boot-refcount
        // transfer is committed only after the swap actually succeeds
        // (`finish_replace` / `on_replace_result`). Committing it here — before
        // the fire-and-forget replace resolves — would desync the refcount on a
        // failed replace, where the trampoline keeps hosting the old module.
        state.begin_replace(ctx, payload);
    }

    /// Settle a forwarded `aether.component.replace` (ADR-0147). The
    /// trampoline's `ReplaceResult` is routed here rather than straight to the
    /// caller so the boot-refcount transfer can be gated on the swap's success;
    /// `finish_replace` commits it on `Ok`, then re-replies the verdict to the
    /// original caller.
    #[handler::manual]
    fn on_replace_result(state: &mut Self::State, ctx: &mut NativeCtx<'_, Self, Manual>, payload: ReplaceResult) {
        state.finish_replace(ctx, payload);
    }

    /// Enumerate the components this engine has actually loaded and
    /// registered, by their ADR-0099 lineage names (issue 2020).
    ///
    /// Reads the registry's live mailbox snapshot — the same coherent
    /// inventory projected to the hub by `RegistryChanged` — and keeps only
    /// the [`MailboxCategory::Trampoline`] entries, the loaded-component set.
    /// Chassis caps are boot-present and static, so the trampolines are
    /// the only registry membership a readiness poll cares about. The
    /// reply is names only: the mailbox id is a deterministic hash-chain
    /// over the lineage the name renders (ADR-0099) and routing is the
    /// substrate's job, so the caller never needs the handle.
    ///
    /// # Agent
    /// Fieldless `ListComponents` to the `aether.component` mailbox —
    /// guaranteed present from boot, so the send always resolves and the
    /// reply is a definitive snapshot. Reply `ListComponentsResult {
    /// names }` lists every currently-loaded component's full lineage
    /// address (`aether.component/aether.embedded:NAME`). Poll it after a
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
            .registry
            .list_mailbox_descriptors()
            .into_iter()
            .filter(|d| d.category == Some(MailboxCategory::Trampoline))
            .map(|d| d.name)
            .collect();
        ListComponentsResult { names }
    }

    /// Introspect one loaded component's ADR-0033 receive-side
    /// `ComponentCapabilities` by lineage `name` (iamacoffeepot/aether#2421).
    /// Resolves `name` to its mailbox id through the routing registry, then
    /// reads the full caps the [`aether_substrate::mail::CapabilityRegistry`]
    /// retains for that
    /// mailbox.
    ///
    /// # Agent
    /// `DescribeComponent { name }` to the `aether.component` mailbox, where
    /// `name` is the lineage address `ListComponents` / `LoadResult.path`
    /// hand back (`aether.embedded:NAME`). Reply `DescribeComponentResult::Ok
    /// { capabilities }` carries the full handler kinds, docs, fallback, and
    /// config kind; `Err { error }` means nothing is registered at that name.
    /// Name-addressed so a boot-manifest-loaded component (ADR-0116), whose
    /// spawner never receives a mailbox id, stays introspectable.
    #[handler::single]
    fn on_describe_component(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        payload: DescribeComponent,
    ) -> DescribeComponentResult {
        // `resolve_address`, not `lookup`: a short path that is
        // ambiguous rather than absent reports its candidate spellings instead
        // of collapsing to "nothing registered" (ADR-0166 §5, issue 4125).
        let resolved = ActorPath::new(&payload.name)
            .map_err(|error| error.to_string())
            .and_then(|name| state.registry.resolve_address(&name).map_err(|error| error.to_string()));
        let mailbox = match resolved {
            Ok(resolved) => resolved.mailbox_id,
            Err(error) => {
                return DescribeComponentResult::Err {
                    error: format!("no component registered at name {}: {error}", payload.name),
                };
            }
        };
        match state.mailer.capability_registry().describe(mailbox) {
            Some(capabilities) => DescribeComponentResult::Ok { capabilities },
            None => {
                DescribeComponentResult::Err { error: format!("no capabilities retained for name {}", payload.name) }
            }
        }
    }
}

impl ComponentHostCapabilityState {
    fn refresh_registry_inventory(&mut self) {
        let inventory = self.registry.inventory();
        let generations = (inventory.mailbox_generation, inventory.kind_generation);

        if self.last_egressed_inventory != Some(generations) {
            self.outbound.egress_kinds_changed(inventory.kinds);
            self.outbound.egress_mailboxes_changed(inventory.mailboxes);
            self.last_egressed_inventory = Some(generations);
        }

        self.registry_subscription
            .as_ref()
            .expect("component host registry subscription installed during wire")
            .acknowledge(generations.0, generations.1);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use aether_substrate::mail::outbound::EgressEvent;
    use aether_substrate::mail::registry::noop_handler;
    use aether_substrate::testing::boot_authority;

    use super::*;

    #[test]
    fn registry_inventory_refresh_is_complete_idempotent_and_generation_gated() {
        let registry = Arc::new(Registry::new());
        let (outbound, rx) = HubOutbound::attached_loopback();
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry)).with_outbound(Arc::clone(&outbound)));
        let engine = Arc::new(Engine::default());
        let subscriber =
            registry.register_inbox(&boot_authority(), "test.component.inventory-subscriber", noop_handler());
        let mut state = ComponentHostCapabilityState {
            linker: Arc::new(Linker::new(&engine)),
            engine,
            registry: Arc::clone(&registry),
            mailer: Arc::clone(&mailer),
            outbound,
            registry_subscription: Some(registry.subscribe_inventory::<ComponentHostCapability>(subscriber, mailer)),
            last_egressed_inventory: None,
            default_name_counter: 0,
            module_cache: ModuleCache::default(),
            boot_registry: HashMap::new(),
            boot_actors: HashSet::new(),
            pending_boots: HashMap::new(),
            boot_hash_by_actor: HashMap::new(),
            pending_replace: HashMap::new(),
            boot_operation_sequence_by_actor: HashMap::new(),
            dominant_boot_operation_by_actor: HashMap::new(),
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

        registry.register_inbox(&boot_authority(), "test.component.inventory.mailbox", noop_handler());
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
        assert!(
            registry.try_register_inbox(&boot_authority(), "test.component.inventory.mailbox", noop_handler()).is_err()
        );
        state.refresh_registry_inventory();
        assert!(rx.try_recv().is_err());
        state.refresh_registry_inventory();
        assert!(rx.try_recv().is_err());
    }
}
