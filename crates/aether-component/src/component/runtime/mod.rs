//! The `aether.component` runtime half (ADR-0122 identity/runtime split).
//! Compiled only under `feature = "runtime"` (the `mod runtime;` declaration
//! in the parent carries the gate), so a transport-only build of the
//! `ComponentHostCapability` identity never names these types nor pulls
//! `aether_substrate` / `wasmtime`. The substrate-typed imports are gated once
//! by this module rather than line-by-line; the `#[actor] impl` reaches the
//! state and the `forward_to_trampoline` helper through the single
//! `use runtime::*` glob in the parent, and the `load` sibling reaches the
//! state fields through their `pub` visibility.

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
mod load;

use super::{ComponentHostCapability, LoadResult};
// `ComponentHostParams` rides up to the cap root through this `pub use`: the
// cap-root `pub use runtime::ComponentHostParams;` re-export sources it here.
pub use self::config::ComponentHostParams;
use crate::component::ParamProviderRegistry;

use aether_kinds::{
    DescribeComponent, DescribeComponentResult, DropComponent, DropResult, ListComponents, ListComponentsResult,
    LoadComponent, ReplaceComponent, ReplaceResult,
};

pub use aether_actor::Manual;

// Crate-local wiring the `#[runtime] impl` handler bodies name (the
// `Kind` / `MailboxCategory` vocabulary), the state struct, and
// `forward_to_trampoline` — all used within this module. No sibling-cap
// imports: drop-time cleanup rides the ADR-0079 vacate/close
// `MonitorNotice` (each cap monitors its registrants and purges its own
// rows), so the host names no peer cap's type or kinds.
use aether_actor::{OutboundReply, ReplyMode, Single};
use aether_data::{Kind, MailboxCategory, Source};

use std::collections::HashMap;
use std::sync::Arc;

use wasmtime::{Engine, Linker};

use aether_substrate::actor::native::{
    NativeActor, NativeCtx, NativeInitCtx, RegistryBatchResult, SpawnOutcome, TaskDone,
};
use aether_substrate::actor::wasm::component::ComponentCtx;
use aether_substrate::chassis::error::BootError;
use aether_substrate::mail::mailer::Mailer;
use aether_substrate::mail::outbound::HubOutbound;
use aether_substrate::mail::registry::{Registry, RegistrySubscription};
use aether_substrate::mail::{KindId, MailboxId};

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
    /// ADR-0170: the composer-supplied provider registry every load validates
    /// its param requests against, and every trampoline evaluates its bag
    /// from. Read-only after Compose.
    pub param_providers: Arc<ParamProviderRegistry>,
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
    /// ADR-0147 module-boot bookkeeping: content hash (sha256 hex of the wasm
    /// bytes) → the module's boot singleton. A module that declares a `boot =`
    /// slot instantiates exactly one boot actor per `(engine, content hash)`;
    /// this table is the per-engine half of that pairing (the state itself is
    /// the per-substrate-process singleton every load runs through). Refcounted
    /// against the module's non-boot actors and empty for every bootless module,
    /// so the common case costs nothing.
    pub boot_registry: HashMap<String, BootEntry>,
    /// Actor-local reservations for module boots that have been staged but are
    /// not authoritative `Live` yet. Same-hash loads and replacements retain
    /// their own move-only deferred replies here and join the first boot result.
    pending_boots: HashMap<String, load::PendingBoot>,
    /// ADR-0147: a loaded non-boot actor's own trampoline mailbox → the content
    /// hash of the module it came from. Populated only for actors sourced from a
    /// module that declares a boot slot, so a drop / replace can find and
    /// decrement the right boot refcount. A bootless module inserts nothing.
    pub boot_hash_by_actor: HashMap<MailboxId, String>,
    /// ADR-0147: in-flight `aether.component.replace` forwards awaiting their
    /// trampoline `ReplaceResult`, keyed by the forward's correlation id. The
    /// boot-refcount transfer for a replace is committed only after the swap
    /// succeeds (`finish_replace`), so the caller's reply target and the
    /// replacement wasm are parked here across the hop. Empty except while a
    /// replace is settling.
    pub pending_replace: HashMap<u64, PendingReplace>,
    /// Last replace/drop operation sequence allocated for each actor mailbox.
    /// A replace reserves its sequence when forwarded; a drop reserves the
    /// next sequence and immediately makes it dominant. Entries survive the
    /// deterministic mailbox id's drop/reload boundary so an older incarnation
    /// can never become current again.
    pub boot_operation_sequence_by_actor: HashMap<MailboxId, u64>,
    /// Latest successful replacement or drop operation that is allowed to
    /// mutate each actor's boot mapping. Failed replacements never enter this
    /// table, so they cannot suppress an earlier successful replacement.
    pub dominant_boot_operation_by_actor: HashMap<MailboxId, u64>,
}

/// ADR-0147: a parked `aether.component.replace` forward. `source` is the
/// original caller's reply target (the trampoline's `ReplaceResult` is routed
/// to the cap instead, then re-replied here); `actor_mailbox` and `new_wasm`
/// are what `commit_replacement_boot` needs to commit the boot-refcount
/// transfer once the swap is confirmed successful. `boot_operation` is
/// reserved when the request is forwarded; it becomes dominant only if that
/// request succeeds, so a later failed request cannot suppress this one.
#[derive(Clone)]
pub struct PendingReplace {
    pub source: Source,
    pub actor_mailbox: MailboxId,
    pub new_wasm: Arc<[u8]>,
    pub boot_operation: u64,
}

/// ADR-0147: one module's boot singleton. `mailbox_id` addresses the boot
/// trampoline (spawned through the same `WasmTrampoline` path as any export);
/// `refcount` counts the module's live **non-boot** actors — boot never counts
/// itself, so its own drop could never be the one that zeroes the count. The
/// `pending_requests` counts requested actors whose trampoline birth has been
/// accepted but has not yet promoted or rejected. The boot is torn down only
/// when both counters are zero: a pending birth keeps a temporarily
/// zero-refcount boot alive, and its later rejection performs the final
/// orphan check.
pub struct BootEntry {
    pub mailbox_id: MailboxId,
    pub refcount: u32,
    pub pending_requests: u32,
}

/// Forward an arbitrary kind to a trampoline's mailbox, preserving the
/// original `reply_to` so the trampoline's reply lands at the agent (not the
/// cap). Used for [`DropComponent`] and [`ReplaceComponent`].
///
/// The forward threads the child mail under the cap's current in-flight root
/// and bumps that root's `in_flight` count before the calling handler returns
/// (`send_envelope_tracked_with_reply_to`), so the originating call stays open
/// across the boundary: the trampoline's deferred `ctx.reply` streams back
/// under a still-open root and settlement fires `ReplyEnd` only after it. A
/// bare enqueue would let the cap handler's return settle the call before the
/// trampoline replied, dropping the reply (the deferred-reply hold-open
/// contract).
///
/// A free fn (no `self`) under the ADR-0122 split: the state-bearing struct
/// holds no field this helper reads, so it stays stateless and the handlers
/// reach it through the parent's `use runtime::*` glob.
fn forward_to_trampoline<M: ReplyMode, P>(ctx: &mut NativeCtx<'_, M>, recipient: MailboxId, kind: KindId, payload: &P)
where
    P: Kind,
{
    let bytes = payload.encode_into_bytes();
    let _ = ctx.send_envelope_tracked_with_reply_to(recipient, kind, &bytes, ctx.reply_target());
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
            param_providers: params.param_providers,
            registry_subscription: None,
            last_egressed_inventory: None,
            default_name_counter: 0,
            boot_registry: HashMap::new(),
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
    /// [`WasmTrampoline`](crate::trampoline::WasmTrampoline) under
    /// `aether.embedded:NAME`, and replies `LoadResult::Ok { mailbox_id,
    /// name, capabilities }` where `name` is the full trampoline
    /// address — agents send subsequent mail to that name.
    /// Errors (bad wire bytes, kind conflict, name conflict,
    /// invalid wasm, instantiation trap) come back as
    /// `LoadResult::Err`.
    #[handler::manual]
    fn on_load_component(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, payload: LoadComponent) {
        state.begin_load(ctx, payload);
    }

    #[handler(task)]
    fn on_kind_registration_done(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_, Single, Self>,
        done: TaskDone<RegistryBatchResult, load::KindRegistration>,
    ) {
        state.finish_kind_registration(ctx, done);
    }

    #[handler(task)]
    fn on_component_spawn_done(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_, Single, Self>,
        done: TaskDone<SpawnOutcome, load::SpawnContext>,
    ) {
        state.finish_spawn(ctx, done);
    }

    /// Refresh the hub's registry projection after a coalesced publication.
    /// The registry owns publication and wake coalescing; this consumer reads
    /// one coherent snapshot, egresses it at most once per generation pair,
    /// and always acknowledges so a publication racing the clear is re-armed.
    #[handler::manual]
    fn on_registry_changed(state: &mut Self::State, _ctx: &mut NativeCtx<'_, Manual>, _payload: RegistryChanged) {
        state.refresh_registry_inventory();
    }

    /// Drop a component by its mailbox id. Forwards
    /// [`DropComponent`] mail to the addressed trampoline; the
    /// trampoline's `WasmTrampoline::on_drop_component` handler
    /// replies `DropResult::Ok` and vacates its mailbox (ADR-0079 §8
    /// amended), which is what purges the mailbox from every sibling
    /// cap's fan-out / routing table — each cap monitors its
    /// registrants and drops its own rows on the `MonitorNotice`, so
    /// the host mails no cap anything at drop time.
    ///
    /// # Agent
    /// `DropComponent { mailbox_id }`. The `mailbox_id` is the
    /// trampoline's id from the `LoadResult.mailbox_id` field.
    #[handler::manual]
    fn on_drop_component(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, payload: DropComponent) {
        // ADR-0147 non-droppability guard: the boot actor is unconditional and
        // refcounted against its module's non-boot actors, so an external drop
        // addressed straight at a boot mailbox must be rejected — letting it
        // through would tear the boot down out from under the refcount and leave
        // a dangling `boot_registry` entry. The boot is torn down automatically
        // (internally, through `release_boot_ref`) when its last non-boot actor
        // unloads; that internal path is not routed through this handler, so the
        // guard never blocks it.
        if state.boot_registry.values().any(|entry| entry.mailbox_id == payload.mailbox_id) {
            ctx.reply(&DropResult::Err {
                error: format!(
                    "mailbox {} is a module boot actor (ADR-0147): the boot singleton is unconditional \
                     and refcounted against its module's non-boot actors, so it cannot be dropped directly — \
                     drop the module's non-boot actors and the boot is torn down when the last one unloads",
                    payload.mailbox_id
                ),
            });
            return;
        }
        // ADR-0147: account this actor's departure against its module's boot
        // singleton before forwarding the drop — the last non-boot actor from a
        // boot-bearing module tears the boot down here (the boot trampoline's
        // own `DropComponent` handler vacates its registrations).
        state.invalidate_replacement_boot_operation(payload.mailbox_id);
        state.release_boot_ref(ctx, payload.mailbox_id);
        forward_to_trampoline(ctx, payload.mailbox_id, DropComponent::ID, &payload);
    }

    /// Replace the component at `mailbox_id` with a fresh wasm
    /// binary. Forwards [`ReplaceComponent`] to the trampoline;
    /// the trampoline's `WasmTrampoline::on_replace_component`
    /// handler swaps `Component` internally and replies
    /// `ReplaceResult`. ADR-0022 + ADR-0038 splice invariants
    /// hold because the inbox channel is the trampoline's
    /// `NativeBinding`, which outlives the swap.
    ///
    /// # Agent
    /// `ReplaceComponent { mailbox_id, wasm, drain_timeout_ms, config, export }`.
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
    fn on_replace_result(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual, Self>, payload: ReplaceResult) {
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
    /// `name` is the lineage address `ListComponents` / `LoadResult.name`
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
        // `resolve_address`, not `lookup`: an abbreviated name that is
        // ambiguous rather than absent reports its candidate spellings instead
        // of collapsing to "nothing registered" (ADR-0166 §5, issue 4125).
        let mailbox = match state.registry.resolve_address(&payload.name) {
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
            param_providers: Arc::new(ParamProviderRegistry::with_substrate_facts()),
            registry_subscription: Some(registry.subscribe_inventory::<ComponentHostCapability>(subscriber, mailer)),
            last_egressed_inventory: None,
            default_name_counter: 0,
            boot_registry: HashMap::new(),
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
