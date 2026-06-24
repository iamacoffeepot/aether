//! The `aether.component` runtime half (ADR-0122 identity/runtime split).
//! Compiled only under `feature = "runtime"` (the `mod runtime;` declaration
//! in the parent carries the gate), so a transport-only build of the
//! `ComponentHostCapability` identity never names these types nor pulls
//! `aether_substrate` / `wasmtime`. The substrate-typed imports are gated once
//! by this module rather than line-by-line; the `#[actor] impl` reaches the
//! state and the `forward_to_trampoline` helper through the single
//! `use runtime::*` glob in the parent, and the `load` sibling reaches the
//! state fields through their `pub(in crate::component)` visibility.

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
/// cap). Used for [`DropComponent`](aether_kinds::DropComponent) and
/// [`ReplaceComponent`](aether_kinds::ReplaceComponent).
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
