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
use crate::kinds::BootTeardown;
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
    /// ADR-0147 module-boot bookkeeping: content hash (the ADR-0238 BLAKE3
    /// hash of the wasm bytes, [`Module::hash`]) → the module's boot
    /// singleton. A module that declares a `boot =` slot instantiates exactly one boot actor per `(engine, content hash)`;
    /// this table is the per-engine half of that pairing (the state itself is
    /// the per-substrate-process singleton every load runs through). Refcounted
    /// against the module's non-boot actors and empty for every bootless module,
    /// so the common case costs nothing. Changed only through
    /// `register_boot` / `unregister_boot`, which keep [`Self::boot_actors`]
    /// in lockstep.
    boot_registry: HashMap<BlobHash, BootEntry>,
    /// ADR-0147: every live module boot's reference — the reverse index of
    /// [`Self::boot_registry`], so the drop guard refusing a drop addressed at
    /// a boot actor is one lookup rather than a scan over every module.
    boot_actors: HashSet<ErasedActorRef>,
    /// Actor-local reservations for module boots that have been staged but are
    /// not authoritative `Live` yet. Same-hash loads and replacements wait
    /// here and join the first boot result: a load by its id, a replacement
    /// with the deferred reply it keeps until #6867.
    pending_boots: HashMap<BlobHash, load::PendingBoot>,
    /// Every load in flight (ADR-0243 §9): its held reply and prepared inputs,
    /// keyed by the id its staged work's contexts carry, from the staged
    /// module publish until the guest's birth answers it or hands it off. At
    /// host close the ledger answers each held reply `unanswered`.
    loads: HashMap<load::LoadId, load::LoadInFlight>,
    /// The next [`load::LoadId`] a load takes.
    next_load: u64,
    /// ADR-0147: a loaded non-boot actor → the content hash of the module it
    /// came from. The key is the actor's proof, taken from its spawn outcome or
    /// proven once at the receipt of a drop / replace (ADR-0230). Populated only
    /// for actors sourced from a module that declares a boot slot, so a drop /
    /// replace can find and decrement the right boot refcount. A bootless module
    /// inserts nothing.
    pub boot_hash_by_actor: HashMap<ErasedActorRef, BlobHash>,
    /// ADR-0147: in-flight `aether.component.replace` forwards awaiting their
    /// trampoline `ReplaceResult`, keyed by the forward's correlation id. The
    /// boot-refcount transfer for a replace is committed only after the swap
    /// succeeds (`finish_replace`), so the caller's reply target and the
    /// replacement module are parked here across the hop. Empty except while a
    /// replace is settling.
    pub pending_replace: HashMap<u64, PendingReplace>,
    /// Host-owned control proof of each successfully born guest, keyed by its
    /// erased reference: the [`GuestControl`] rows its trampoline serves. The
    /// guest's public receive surface deliberately replaces the trampoline's
    /// native surface, so an external path cannot recover this proof by
    /// casting after load. A drop removes its entry before forwarding, since
    /// the forwarded drop closes the trampoline (ADR-0241 §8), so a replace
    /// or a second drop that arrives before its route reads `Dropped` finds
    /// no entry and is refused.
    drop_targets: HashMap<ErasedActorRef, ProtocolRef<GuestControl>>,
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
/// the replace's receipt — and `module` are what `commit_replacement_boot`
/// needs to commit the boot-refcount transfer once the swap is confirmed
/// successful. Holding `module` across the hop also keeps its cache entry
/// live, so the trampoline's own check-in of the forwarded bytes is a hit.
/// `boot_operation` is reserved when the request is forwarded; it becomes
/// dominant only if that request succeeds, so a later failed request cannot
/// suppress this one.
#[derive(Clone)]
pub struct PendingReplace {
    pub source: Source,
    pub actor: ErasedActorRef,
    pub module: Module,
    pub boot_operation: u64,
}

/// ADR-0147: one module's boot singleton. `boot` is the boot guest's control
/// proof, taken from its birth outcome (born through the same
/// `WasmTrampoline` path as any export), which the teardown sends
/// [`BootTeardown`] through;
/// `refcount` counts the module's live **non-boot** actors — boot never counts
/// itself, so its own drop could never be the one that zeroes the count. The
/// `pending_requests` counts requested actors whose trampoline birth has been
/// accepted but has not yet promoted or rejected. The boot is torn down only
/// when both counters are zero: a pending birth keeps a temporarily
/// zero-refcount boot alive, and its later rejection performs the final
/// orphan check.
pub struct BootEntry {
    boot: ProtocolRef<GuestControl>,
    refcount: u32,
    pending_requests: u32,
}

/// The rows the component host controls a guest through: its trampoline's
/// own framework rows, never the guest's published surface. A guest birth
/// completes with this proof (ADR-0241 §6), and the host hands a load's
/// reply off, forwards a drop, and tears a module boot down through it.
#[aether_actor::protocol]
trait GuestControl {
    fn load_delivered(mail: LoadDelivered) -> LoadResult;
    fn drop_component(mail: DropComponent) -> DropResult;
    fn boot_teardown(mail: BootTeardown);
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
    /// the loads and replacements waiting on it, and a requested guest takes
    /// over its load's held reply.
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
    /// path, `LoadResult.path`: `NS`, `NS:key`, or `parent/NS:key`.
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
        // The drop closes the trampoline, so its entry leaves now: a second
        // drop or a replace that proves the path before the owner applies its
        // `Dropped` route finds no entry and is refused.
        let Some(target) = state.drop_targets.remove(&actor) else {
            ctx.reply(&DropResult::Err { error: format!("no live component to drop at {}", payload.target) });
            return;
        };
        // ADR-0147: account this actor's departure against its module's boot
        // singleton before forwarding the drop — the last non-boot actor from a
        // boot-bearing module tears the boot down here (the boot trampoline's
        // `BootTeardown` handler releases its guest and vacates its
        // registrations).
        state.invalidate_replacement_boot_operation(actor);
        state.release_boot_ref(ctx, actor);
        // The forward inherits this call's chain, so the call stays open until
        // the trampoline's deferred reply lands at the original caller.
        ctx.forward_to(target, &payload);
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
    /// trampoline currently hosts.
    #[handler::single]
    fn on_replace_component(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        payload: ReplaceComponent,
    ) -> Pending<ReplaceResult> {
        // ADR-0241 §4: the replacement module publishes first, so admission
        // refuses a republish that drops a namespace or narrows a contract
        // before the trampoline is touched, and the replacement's kinds
        // register. ADR-0147: once the publish commits, forward the replace
        // to the trampoline but intercept its `ReplaceResult` at this cap
        // (`forward_replace`), so the boot-refcount
        // transfer is committed only after the swap actually succeeds
        // (`finish_replace` / `on_replace_result`). Committing it here — before
        // the fire-and-forget replace resolves — would desync the refcount on a
        // failed replace, where the trampoline keeps hosting the old module.
        let (pending, held) = ctx.hold::<ReplaceResult>();
        state.begin_replace(ctx, held, payload);
        pending
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
    use aether_substrate::testing::{
        boot_authority, decode_session_reply, registered_binding, registered_ref, session_sender, try_registered_ref,
        unrouted_binding,
    };

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

    /// A live route without a retained component-host proof is refused at the
    /// external-address boundary. The guard must run before either boot
    /// accounting or replacement ordering changes, or a wrong actor kind can
    /// corrupt component-host state even though no drop was forwarded.
    #[test]
    fn unowned_drop_refuses_before_state_mutation() {
        let registry = Arc::new(Registry::new());
        let (outbound, rx) = HubOutbound::attached_loopback();
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry)).with_outbound(Arc::clone(&outbound)));
        let binding = unrouted_binding(&mailer);
        let engine = Arc::new(Engine::default());
        let mut state = ComponentHostCapabilityState {
            linker: Arc::new(Linker::new(&engine)),
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
        let actor = registered_ref(&registry, "test.component.not-a-trampoline", noop_handler());
        let target =
            NativeCtx::<ComponentHostCapability>::new_for_actor(&binding, Source::NONE, None, None).actor_path(actor);
        let hash = BlobHash::from_bytes([7; 32]);
        state.boot_hash_by_actor.insert(actor, hash);
        state.boot_operation_sequence_by_actor.insert(actor, 11);
        state.dominant_boot_operation_by_actor.insert(actor, 10);

        {
            let mut ctx = NativeCtx::new_dispatching(&binding, session_sender(), None, None);
            ComponentHostCapability::on_drop_component(&mut state, &mut ctx, DropComponent { target });
        }

        assert!(matches!(
            decode_session_reply::<DropResult>(&rx),
            DropResult::Err { error } if error.contains("no live component to drop at")
        ));
        assert_eq!(state.boot_hash_by_actor.get(&actor), Some(&hash));
        assert_eq!(state.boot_operation_sequence_by_actor.get(&actor), Some(&11));
        assert_eq!(state.dominant_boot_operation_by_actor.get(&actor), Some(&10));
    }
}
