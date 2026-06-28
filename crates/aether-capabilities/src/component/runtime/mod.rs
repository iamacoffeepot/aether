//! The `aether.component` runtime half (ADR-0122 identity/runtime split).
//! Compiled only under `feature = "runtime"` (the `mod runtime;` declaration
//! in the parent carries the gate), so a transport-only build of the
//! `ComponentHostCapability` identity never names these types nor pulls
//! `aether_substrate` / `wasmtime`. The substrate-typed imports are gated once
//! by this module rather than line-by-line; the `#[actor] impl` reaches the
//! state and the `forward_to_trampoline` helper through the single
//! `use runtime::*` glob in the parent, and the `load` sibling reaches the
//! state fields through their `pub(in crate::component)` visibility.

// The moved `#[runtime] impl NativeActor for ComponentHostCapability` body
// names the `#[runtime]` attribute, the cap struct, the cap kinds (input +
// reply), and `ComponentHostConfig` (its `Config` type), which previously
// resolved at `mod.rs` root — now sourced here beside the body.
use aether_actor::runtime;

// `load` (the `handle_load` sequence as a method on the state) and `config`
// (the `ComponentHostConfig` init bundle), now nested under this `runtime`
// directory so the one `mod runtime;` gate in the parent covers them (no
// per-sibling `#[cfg]`). The `load` impl reaches the state fields through their
// `pub(in crate::component)` visibility, unchanged by the move.
mod config;
mod load;

use super::ComponentHostCapability;
// `ComponentHostConfig` rides up to the cap root through this `pub use`: the
// cap-root `pub use runtime::ComponentHostConfig;` re-export sources it here.
pub use self::config::ComponentHostConfig;

use aether_kinds::{
    DescribeComponent, DescribeComponentResult, DropComponent, ListComponents,
    ListComponentsResult, LoadComponent, LoadResult, ReplaceComponent,
};

// Crate-local wiring the `#[actor] impl` handler bodies name (sibling caps it
// mails, the unsubscribe kind, the `Kind` / `MailboxCategory` vocabulary),
// re-exported so the parent reaches them through its `use runtime::*` glob.
pub use crate::input::{InputCapability, UnsubscribeAll};
pub use crate::lifecycle::LifecycleCapability;
pub use aether_data::{Kind, MailboxCategory};
pub use aether_kinds::LifecycleUnsubscribeAll;

pub use std::sync::Arc;
pub use std::sync::atomic::AtomicU64;

pub use wasmtime::{Engine, Linker};

pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
pub use aether_substrate::actor::wasm::component::ComponentCtx;
pub use aether_substrate::chassis::error::BootError;
pub use aether_substrate::mail::mailer::Mailer;
pub use aether_substrate::mail::outbound::HubOutbound;
pub use aether_substrate::mail::registry::Registry;
pub use aether_substrate::mail::{KindId, MailboxId};

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
/// it as crate-public API. Fields carry `pub(in crate::component)` so the
/// `load` submodule (which holds `handle_load`) can reach them as a sibling
/// within `crate::component`.
pub struct ComponentHostCapabilityState {
    pub(in crate::component) engine: Arc<Engine>,
    pub(in crate::component) linker: Arc<Linker<ComponentCtx>>,
    pub(in crate::component) registry: Arc<Registry>,
    pub(in crate::component) mailer: Arc<Mailer>,
    pub(in crate::component) outbound: Arc<HubOutbound>,
    /// Monotonic counter for `component_N` default names when an agent passes
    /// `name: None` and the wasm doesn't declare an `aether.namespace`.
    pub(in crate::component) default_name_counter: AtomicU64,
}

/// Forward an arbitrary kind to a trampoline's mailbox, preserving the
/// original `reply_to` so the trampoline's reply lands at the agent (not the
/// cap). Used for [`DropComponent`] and [`ReplaceComponent`].
///
/// The forward threads the child mail under the cap's current in-flight root
/// and bumps that root's `in_flight` count before the calling handler returns
/// (`send_envelope_traced_with_reply_to`), so the originating call stays open
/// across the boundary: the trampoline's deferred `ctx.reply` streams back
/// under a still-open root and settlement fires `ReplyEnd` only after it. A
/// bare enqueue would let the cap handler's return settle the call before the
/// trampoline replied, dropping the reply (the deferred-reply hold-open
/// contract).
///
/// A free fn (no `self`) under the ADR-0122 split: the state-bearing struct
/// holds no field this helper reads, so it stays stateless and the handlers
/// reach it through the parent's `use runtime::*` glob.
pub fn forward_to_trampoline<P>(
    ctx: &mut NativeCtx<'_>,
    recipient: MailboxId,
    kind: KindId,
    payload: &P,
) where
    P: Kind,
{
    let bytes = payload.encode_into_bytes();
    let _ = ctx.send_envelope_traced_with_reply_to(recipient, kind, &bytes, ctx.reply_target());
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
            default_name_counter: AtomicU64::new(0),
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
    #[handler]
    fn on_load_component(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        payload: LoadComponent,
    ) -> LoadResult {
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
    /// every fan-out subscriber table so no cap keeps firing at a
    /// dropped mailbox: `aether.input`'s input-stream tables (via
    /// [`UnsubscribeAll`]) and `aether.lifecycle`'s per-stage tables
    /// (via [`LifecycleUnsubscribeAll`]).
    ///
    /// # Agent
    /// `DropComponent { mailbox_id }`. The `mailbox_id` is the
    /// trampoline's id from the `LoadResult.mailbox_id` field.
    #[handler]
    fn on_drop_component(
        _state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        payload: DropComponent,
    ) {
        // Cap-side cleanup: ask each owning cap to drop the dying
        // trampoline from its fan-out sets. Mail rather than direct
        // mutation post-issue-640 — each cap is the sole owner of its
        // own subscriber table.
        ctx.actor::<InputCapability>().send(&UnsubscribeAll {
            mailbox: payload.mailbox_id,
        });
        ctx.actor::<LifecycleCapability>()
            .send(&LifecycleUnsubscribeAll {
                mailbox: payload.mailbox_id.0,
            });
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
    #[handler]
    fn on_replace_component(
        _state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        payload: ReplaceComponent,
    ) {
        forward_to_trampoline(ctx, payload.mailbox_id, ReplaceComponent::ID, &payload);
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
    #[handler]
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
    #[handler]
    fn on_describe_component(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        payload: DescribeComponent,
    ) -> DescribeComponentResult {
        let Some(mailbox) = state.registry.lookup(&payload.name) else {
            return DescribeComponentResult::Err {
                error: format!("no component registered at name {}", payload.name),
            };
        };
        match state.mailer.capability_registry().describe(mailbox) {
            Some(capabilities) => DescribeComponentResult::Ok { capabilities },
            None => DescribeComponentResult::Err {
                error: format!("no capabilities retained for name {}", payload.name),
            },
        }
    }
}
