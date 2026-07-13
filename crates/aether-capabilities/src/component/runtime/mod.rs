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
use aether_actor::runtime;

// `load` (the `handle_load` sequence as a method on the state) and `config`
// (the `ComponentHostConfig` init bundle), now nested under this `runtime`
// directory so the one `mod runtime;` gate in the parent covers them (no
// per-sibling `#[cfg]`). The `load` impl reaches the state fields through their
// `pub` visibility, unchanged by the move.
mod config;
mod load;

use super::ComponentHostCapability;
// `ComponentHostConfig` rides up to the cap root through this `pub use`: the
// cap-root `pub use runtime::ComponentHostConfig;` re-export sources it here.
pub use self::config::ComponentHostConfig;

use aether_kinds::{
    DescribeComponent, DescribeComponentResult, DropComponent, DropResult, ListComponents, ListComponentsResult,
    LoadComponent, LoadResult, ReplaceComponent, ReplaceResult,
};

pub use aether_actor::Manual;

// Crate-local wiring the `#[runtime] impl` handler bodies name (sibling caps it
// mails, the unsubscribe kind, the `Kind` / `MailboxCategory` vocabulary), the
// state struct, and `forward_to_trampoline` — all used within this module.
use crate::http::HttpServerCapability;
use crate::http::kinds::UnregisterRoutesAll;
use crate::input::{InputCapability, UnsubscribeAll};
use crate::lifecycle::LifecycleCapability;
use aether_actor::{Addressable, OutboundReply, ReplyMode};
use aether_data::{Kind, MailboxCategory, Source};
use aether_kinds::LifecycleUnsubscribeAll;

use std::collections::HashMap;
use std::sync::Arc;

use wasmtime::{Engine, Linker};

use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::actor::wasm::component::ComponentCtx;
use aether_substrate::chassis::error::BootError;
use aether_substrate::mail::mailer::Mailer;
use aether_substrate::mail::outbound::HubOutbound;
use aether_substrate::mail::registry::Registry;
use aether_substrate::mail::{KindId, MailboxId};

/// `aether.component` runtime state (ADR-0122 split). Holds the wasmtime
/// `engine` + `linker` every load instantiates against, the mail `registry`,
/// the `mailer` / `outbound` egress handles, and the monotonic
/// `default_name_counter` for `component_N` default names. Plain fields (no
/// `Arc<Inner>` wrapper) per ADR-0078 — the cap is single-threaded, every
/// handler runs on the cap's dispatcher thread. Input subscribe / unsubscribe
/// go through `aether.input` via mail (post-issue-640): no `input_mailbox`
/// field; `ctx.actor::<InputCapability>().send(...)` resolves it inline.
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
}

/// ADR-0147: a parked `aether.component.replace` forward. `source` is the
/// original caller's reply target (the trampoline's `ReplaceResult` is routed
/// to the cap instead, then re-replied here); `actor_mailbox` and `new_wasm`
/// are what `rebind_boot_ref` needs to commit the boot-refcount transfer once
/// the swap is confirmed successful.
pub struct PendingReplace {
    pub source: Source,
    pub actor_mailbox: MailboxId,
    pub new_wasm: Vec<u8>,
}

/// ADR-0147: one module's boot singleton. `mailbox_id` addresses the boot
/// trampoline (spawned through the same `WasmTrampoline` path as any export);
/// `refcount` counts the module's live **non-boot** actors — boot never counts
/// itself, so its own drop could never be the one that zeroes the count. The
/// boot is torn down when `refcount` reaches zero (the last non-boot actor from
/// the module unloads or is rebound onto a different content hash).
pub struct BootEntry {
    pub mailbox_id: MailboxId,
    pub refcount: u32,
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

impl ComponentHostCapabilityState {
    /// Purge a trampoline mailbox from every cap's fan-out / routing table so no
    /// cap keeps firing at a torn-down mailbox: `aether.input`'s input-stream
    /// tables ([`UnsubscribeAll`]), `aether.lifecycle`'s per-stage tables
    /// ([`LifecycleUnsubscribeAll`]), and `aether.http.server`'s route table
    /// ([`UnregisterRoutesAll`], ADR-0130). Mail rather than direct mutation
    /// (post-issue-640 — each cap owns its own subscriber table).
    ///
    /// Shared by the external drop path ([`NativeActor::on_drop_component`]) and
    /// the ADR-0147 internal boot-teardown paths ([`Self::release_boot_ref`] at
    /// refcount zero, and the load-time freshly-inserted-boot rollback), so a
    /// boot torn down internally clears the same registrations an external drop
    /// would — without self-sending a `DropComponent` back through the guarded
    /// `on_drop_component` (which rejects boot mailboxes, ADR-0147).
    pub fn purge_trampoline_registrations<M: ReplyMode>(&self, ctx: &mut NativeCtx<'_, M>, mailbox: MailboxId) {
        ctx.actor::<InputCapability>().send(&UnsubscribeAll { mailbox });
        ctx.actor::<LifecycleCapability>().send(&LifecycleUnsubscribeAll { mailbox: mailbox.0 });
        // The http server registers only when a bind is configured (ADR-0108),
        // so gate its route purge on the cap being live — an unguarded typed
        // send would warn-drop on every drop in a chassis that serves no HTTP.
        if self.registry.lookup(<HttpServerCapability as Addressable>::NAMESPACE).is_some() {
            ctx.actor::<HttpServerCapability>().send(&UnregisterRoutesAll { mailbox });
        }
    }
}

#[runtime]
impl NativeActor for ComponentHostCapability {
    /// The runtime state this identity boots into (ADR-0122 split): the
    /// wasmtime instances, mail registry, egress handles, and default-name
    /// counter every load instantiates against.
    type State = ComponentHostCapabilityState;

    type Config = ComponentHostConfig;
    const NAMESPACE: &'static str = "aether.component";

    fn init(
        config: ComponentHostConfig,
        ctx: &mut NativeInitCtx<'_>,
    ) -> Result<ComponentHostCapabilityState, BootError> {
        let mailer = ctx.mailer();
        let registry = Arc::clone(mailer.registry());
        Ok(ComponentHostCapabilityState {
            engine: config.engine,
            linker: config.linker,
            registry,
            mailer,
            outbound: config.hub_outbound,
            default_name_counter: 0,
            boot_registry: HashMap::new(),
            boot_hash_by_actor: HashMap::new(),
            pending_replace: HashMap::new(),
        })
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
    #[handler::single]
    fn on_load_component(state: &mut Self::State, ctx: &mut NativeCtx<'_>, payload: LoadComponent) -> LoadResult {
        // ADR-0109: the return type is the reply contract — the
        // `#[actor]` macro routes this `LoadResult` back to the sender
        // through `OutboundReply::reply`, so no manual `ctx.reply`.
        state.handle_load(ctx, payload)
    }

    /// Drop a component by its mailbox id. Forwards
    /// [`DropComponent`] mail to the addressed trampoline; the
    /// trampoline's `WasmTrampoline::on_drop_component` handler
    /// replies `DropResult::Ok` and shuts itself down.
    ///
    /// Before forwarding, purges the dying trampoline's mailbox from
    /// every fan-out / routing table so no cap keeps firing at a
    /// dropped mailbox: `aether.input`'s input-stream tables (via
    /// [`UnsubscribeAll`]), `aether.lifecycle`'s per-stage tables
    /// (via [`LifecycleUnsubscribeAll`]), and `aether.http.server`'s
    /// route table (via [`UnregisterRoutesAll`], ADR-0130).
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
        // Cap-side cleanup: purge the dying trampoline from every cap's fan-out
        // sets before forwarding the drop.
        state.purge_trampoline_registrations(ctx, payload.mailbox_id);
        // ADR-0147: account this actor's departure against its module's boot
        // singleton before forwarding the drop — the last non-boot actor from a
        // boot-bearing module tears the boot down here (which purges the boot's
        // own registrations via `release_boot_ref`).
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
    fn on_replace_result(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, payload: ReplaceResult) {
        state.finish_replace(ctx, payload);
    }

    /// Enumerate the components this engine has actually loaded and
    /// registered, by their ADR-0099 lineage names (issue 2020).
    ///
    /// Reads the registry's live mailbox snapshot — the same list
    /// already egressed to the hub after each load — and keeps only the
    /// [`MailboxCategory::Trampoline`] entries, the loaded-component set.
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
    /// reads the full caps the [`CapabilityRegistry`] retains for that
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
        let Some(mailbox) = state.registry.lookup(&payload.name) else {
            return DescribeComponentResult::Err { error: format!("no component registered at name {}", payload.name) };
        };
        match state.mailer.capability_registry().describe(mailbox) {
            Some(capabilities) => DescribeComponentResult::Ok { capabilities },
            None => {
                DescribeComponentResult::Err { error: format!("no capabilities retained for name {}", payload.name) }
            }
        }
    }
}
