//! aether-actor: wasm guest SDK and transport-agnostic actor primitives.
//! Shared by wasm components and native capabilities. Issue 552 stage 0
//! folded the prior `aether-component` crate's guest shim in here so
//! the SDK and its wasm binding layer share one home.
//!
//! ADR-0074 §Decision settled the actor model: components and
//! capabilities collapse into one actor primitive — one mpsc inbox,
//! one OS thread, one `MailboxId`. Issue 665 retired the unifying
//! `MailTransport` trait that originally tied the wasm and native
//! halves together — the cross-target abstraction is now the
//! per-stage capability traits in [`actor::ctx`]; the per-target
//! dispatch surfaces are [`wasm::bridge`] (wasm: `wasm::bridge::mail`,
//! `wasm::bridge::persist`) and the inherent methods on
//! `aether_substrate::actor::native::binding::NativeBinding`.
//!
//! Public surface:
//!   - [`Mail`], [`PriorState`], [`ReplyHandle`], [`KindId`] —
//!     transport-free types: pure decode / phantom typing.
//!   - [`Mailbox`] — pure addressing token (`mailbox_id`, `kind_id`)
//!     after issue 665 dropped the `T: MailTransport` parameter; sends
//!     route through each ctx's send methods, not through the mailbox
//!     itself.
//!   - [`actor::ctx`] — per-stage capability traits ([`MailSender`],
//!     [`OutboundReply`], [`Persistence`]). Wasm ctxs in [`wasm::ctx`]
//!     and substrate's `NativeCtx` family impl the relevant subset.
//!   - [`Slot`] — single-instance backing store the consumer's
//!     [`export!`] macro emits as a `static`.
//!   - [`wasm`] — wasm guest binding layer: [`wasm::bridge`] dispatch
//!     functions + [`WasmActor`] trait (with the `on_dehydrate` /
//!     `on_rehydrate` hot-swap hooks, ADR-0101) +
//!     [`WasmActorMailbox`] for the actor-typed sender chain +
//!     the [`export!`] macro that pins `init` / `receive` /
//!     lifecycle FFI exports plus the `aether.kinds.inputs` /
//!     `aether.namespace` custom-section statics.
//!
//! No FFI imports are pulled in unconditionally — the host-fn externs
//! in [`wasm::raw`] live behind a `#[cfg(target_family = "wasm")]`
//! block and the native-target stubs panic if invoked, so the crate
//! compiles for `cargo test --workspace` on the host without dragging
//! the FFI surface into the linker.

#![no_std]

extern crate alloc;

// Self-alias so proc-macros (today: `#[local]` in
// `aether-actor-derive`) that emit absolute paths like
// `::aether_actor::Local` resolve when used inside this crate
// itself — e.g., the `local` test module's probe newtypes.
// Outside callers don't need this; it's a no-op for them.
extern crate self as aether_actor;

pub mod actor;
pub mod local;
pub mod log;
pub mod mail;
pub mod trace_ring;
pub mod wasm;

pub use actor::ctx::{MailSender, Manual, OutboundReply, Persistence, ReplyMode, Single, Stream};
pub use actor::slot::Slot;
pub use actor::{
    Actor, Addressable, EMBEDDED_SCOPE, Embedded, EmbeddedMany, HandlesKind, Instanced, Lifecycle,
    Many, NAMESPACE_SEGMENT_MAX_LEN, NamespaceError, One, Resolve, Singleton, Subname,
    validate_namespace_segment,
};
pub use local::Local;
// Issue 665: `Mailbox<K, T>` and `ActorMailbox<'_, R, T>` retired; the
// surviving [`mail::mailbox::Mailbox<K>`] is a transport-free
// addressing token. Per-side actor-typed mailboxes live next to their
// transport: [`wasm::WasmActorMailbox<R>`] for wasm guests and
// `aether_substrate::actor::native::NativeActorMailbox<'a, R>` for
// native actors.
pub use mail::mailbox::{KindId, Mailbox, resolve, resolve_mailbox};
pub use mail::{Mail, NO_REPLY_HANDLE, PriorState, ReplyHandle};

// Wasm surface promoted to the crate root so consumers see
// `aether_actor::WasmCtx<'_>` / `aether_actor::WasmActor` / etc. without
// an extra `wasm::` segment.
pub use wasm::{
    ActorInitError, ErasedWasmActor, RelativeMailbox, SpawnError, WasmActor, WasmActorMailbox,
    WasmCtx, WasmDispatch, WasmDropCtx, WasmInitCtx,
};

// Issue 665 retired `MailTransport` and its `MailTransportTrait`
// alias. Per-stage capability traits in `actor::ctx` are the
// cross-target abstraction; per-target dispatch lives in
// `wasm::bridge::*` (wasm) and `NativeBinding`'s inherent methods
// (native).

/// Return code the `#[actor]`-synthesized dispatcher sends back up
/// through `receive_p32` when a `#[handler]` arm matched (or the
/// `#[fallback]` ran, which by definition handles anything). Propagated
/// verbatim by the consumer's FFI shim.
pub const DISPATCH_HANDLED: u32 = 0;

/// Return code for "no `#[handler]` matched and there's no `#[fallback]`"
/// — the strict-receiver miss. Propagated through the FFI so the
/// substrate's scheduler can emit a `tracing::warn!` naming the
/// mailbox + kind (ADR-0033 §Strict receivers, issue #142). Matches
/// `aether_substrate_bundle::component::DISPATCH_UNKNOWN_KIND` by value.
pub const DISPATCH_UNKNOWN_KIND: u32 = 1;

/// Re-exports the `#[actor]` macro relies on at expansion sites
/// that don't depend on `aether-data` directly. Keeping the macro's
/// emitted paths rooted at `::aether_actor::__macro_internals` removes
/// the "add aether-data to your Cargo.toml" boilerplate that
/// `::aether_data::...` paths would otherwise force on every
/// consumer.
///
/// Not part of the public API; the macro is the only intended caller.
#[doc(hidden)]
pub mod __macro_internals {
    pub use aether_data::__derive_runtime::{Cow, KindLabels, SchemaType, canonical};
    pub use aether_data::{Kind, Schema, mailbox_id_from_name};
    // ADR-0096: the multi-actor `export!` arm stores the instance as
    // `Box<dyn ErasedWasmActor>`; re-export `Box` so the emitted code
    // doesn't depend on the guest crate's prelude exposing `alloc`.
    pub use alloc::boxed::Box;
    // ADR-0113: the `#[actor]`-generated `on_rehydrate` warns through
    // `::aether_actor::__macro_internals::tracing::warn!` on a non-empty
    // decode-miss, so the macro roots the warn here rather than forcing
    // `tracing` into every component's dependency list.
    pub use tracing;
}

/// ADR-0033 attribute macros and `Kind` / `Schema` derives. Issue 552
/// stage 0 consolidated the proc-macros into `aether-actor-derive`.
/// `Kind` / `Schema` / `KindId` / `MailboxId` forward through
/// `aether-data` so the derive paths the macro emits
/// (`::aether_data::Kind`, etc.) continue to resolve through the
/// established re-export chain. The actor-SDK attribute macros
/// (`actor`, `capability`, `fallback`, `handler`, `local`)
/// are sourced directly from `aether-actor-derive` — `aether-data` is
/// the foundational data-layer crate and no longer exports actor-SDK
/// surface. Component and capability authors need only `aether-actor`
/// in their dep list; the full macro surface is available from here.
pub use aether_actor_derive::{actor, capability, fallback, handler, local, runtime};
pub use aether_data::{Kind, KindId as DataKindId, MailboxId, Schema};
// ADR-0119: the `#[derive(Singleton)]` / `#[derive(Instanced)]` /
// `#[derive(Embeddable)]` proc-macros are retired. Cardinality is the
// `Addressable::Resolver`, and the `Singleton` / `Instanced` marker traits
// derive from it by blanket impl — so the only `aether_actor::{Singleton,
// Instanced}` surface now is the trait (re-exported via `actor::*` above).
