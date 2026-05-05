//! aether-actor: actor SDK shared by WASM components and (eventually)
//! native capabilities. Issue 552 stage 0 folded the prior
//! `aether-component` crate's wasm-guest shim in here as `wasm` —
//! the SDK and its wasm-guest impl now share one home so the
//! proc-macro path emissions (`::aether_actor::WasmCtx<'_>`, etc.)
//! resolve through one crate name regardless of whether the consumer
//! is a wasm component or a native test fixture. The
//! `aether-data-derive` macro crate consolidated into `aether-actor-derive`
//! at the same time.
//!
//! ADR-0074 §Decision settled the actor model: components and
//! capabilities collapse into one actor primitive — one mpsc inbox,
//! one OS thread, one `MailboxId` — and share this SDK over two
//! transport implementations. Phase 1 lifted the SDK out of
//! `aether-component`; issue 552 stage 0 then collapsed the wasm
//! shim back in here so the SDK + transport impls share one crate.
//! `NativeTransport` arrives in stage 1 alongside the per-handler
//! `NativeCtx<'_>` ctx surface that mirrors [`wasm::WasmCtx<'_>`].
//!
//! Public surface:
//!   - [`MailTransport`] — the five-method trait every transport
//!     impl must provide; signatures mirror the wasm `_p32` FFI
//!     byte-for-byte.
//!   - [`Mail`], [`PriorState`], [`ReplyTo`], [`KindId`] —
//!     transport-free types: pure decode / phantom typing.
//!   - [`Mailbox`], [`Ctx`], [`InitCtx`], [`DropCtx`] — generic over
//!     `T: MailTransport`; method bodies dispatch through `T::*`.
//!     The wasm-flavoured 1-arg aliases ([`WasmCtx`], [`WasmInitCtx`],
//!     [`WasmDropCtx`]) live in [`wasm`] alongside [`WasmTransport`].
//!   - [`Slot`] — single-instance backing store the consumer's
//!     [`export!`] macro emits as a `static`.
//!   - [`WaitError`] + [`wait_reply`] + [`decode_wait_reply`] —
//!     ADR-0042 sync round-trip helper, generic over the reply kind,
//!     the error enum, and the transport.
//!   - [`wasm`] — wasm-guest impl: [`WasmTransport`] +
//!     [`WasmActor`] trait + [`Replaceable`] hook trait + the
//!     [`export!`] macro that pins `init` / `receive` / lifecycle FFI
//!     exports plus the `aether.kinds.inputs` / `aether.namespace`
//!     custom-section statics.
//!
//! No FFI imports are pulled in unconditionally — the wasm host-fn
//! externs in [`wasm::raw`] live behind a `#[cfg(target_arch =
//! "wasm32")]` block and the native-target stubs panic if invoked,
//! so the crate compiles for `cargo test --workspace` on the host
//! without dragging the FFI surface into the linker.

#![no_std]

extern crate alloc;

mod actor;
mod ctx;
mod mail;
mod sender;
mod sink;
mod slot;
mod sync;
mod transport;
pub mod wasm;

pub use actor::{Actor, Dispatch, HandlesKind, Singleton};
pub use ctx::{Ctx, DropCtx, InitCtx};
pub use mail::{Mail, NO_REPLY_HANDLE, PriorState, ReplyTo};
pub use sender::{MailCtx, Sender};
// Generic 2-arg `Mailbox<K, T>` stays accessible as
// `aether_actor::sink::Mailbox`. At the crate root we re-export the
// 1-arg wasm alias (defined in `wasm`) under the same `Mailbox` name
// so existing `aether_component::Mailbox<K>` consumers keep their
// call shape when migrating to `aether_actor::*`.
pub use sink::{ActorMailbox, KindId, resolve, resolve_mailbox};
pub use slot::Slot;
pub use sync::{WaitError, decode_wait_reply, wait_reply};
pub use transport::MailTransport;

// Wasm-guest surface promoted to the crate root so consumers see
// `aether_actor::WasmCtx<'_>` / `aether_actor::WasmActor` / etc.
// without an extra `wasm::` segment. Replaces the prior crate-root
// re-exports `aether-component` provided. `Mailbox<K>` here is the
// wasm 1-arg alias — the generic form is reachable through `sink`.
pub use wasm::{
    BootError, Component, Mailbox, Replaceable, WASM_TRANSPORT, WasmActor, WasmCtx, WasmDropCtx,
    WasmInitCtx, WasmTransport,
};
// Wasm helper modules (file I/O, HTTP egress, tracing → mail bridge)
// surface at the crate root so existing `aether_component::io::*`
// call sites migrate to `aether_actor::io::*` without growing a
// `wasm::` segment.
pub use wasm::{io, log, net};

// Issue 442 / ADR-0033: `MailTransport` doubles as a re-export name
// for the trait when consumers want to spell out the bound. Kept
// separate from the [`MailTransport`] re-export above for code that
// wrote `aether_component::MailTransportTrait` against the prior
// alias.
pub use transport::MailTransport as MailTransportTrait;

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
    pub use aether_data::{Kind, Schema};
}

/// ADR-0033 attribute macros and `Kind` / `Schema` derives, behind
/// the prior `aether-component`'s re-export of `aether-data`'s
/// `derive` feature. Issue 552 stage 0 routed the macro home to
/// `aether-actor-derive`; we forward through `aether-data` so the
/// derive paths the macro emits (`::aether_data::Kind`, etc.)
/// continue to resolve through the established re-export chain. The
/// stage-0 additions `#[capability]` (cfg-gates native cap fields)
/// and `#[derive(Singleton)]` (emits the `Singleton` marker) sit
/// alongside the existing actor derives so component / capability
/// authors only need `aether-actor` in their dep list.
pub use aether_data::{
    Kind, KindId as DataKindId, Schema, actor, bridge, capability, fallback, handler,
};
// `Singleton` lives in two namespaces:
//   - the type namespace for the marker trait (`Actor` super-trait),
//     re-exported above as part of `actor::*`.
//   - the macro namespace for the `#[derive(Singleton)]` proc-macro,
//     forwarded through `aether-data::Singleton` (which itself
//     re-exports `aether-actor-derive::Singleton` behind the `derive`
//     feature).
// Rust resolves derive names from the macro namespace and type names
// from the type namespace, so the same `Singleton` identifier covers
// both `impl Singleton for X` and `#[derive(Singleton)]` without
// ambiguity at the user's call site. The `#[doc(inline)]` flattens
// the rustdoc page so the derive shows up at `aether_actor::Singleton`
// rather than under a synthetic re-export shim.
#[doc(inline)]
pub use aether_data::Singleton;

/// Wrap one-or-more items in `#[cfg(not(target_arch = "wasm32"))]`.
/// Issue 552 stage 4's wasm-header-only build of `aether-capabilities`
/// gates per-cap native imports + helpers + impls behind that cfg;
/// this macro compresses what would otherwise be 5-7 sprinkled
/// attributes per cap file into one block per native-only chunk.
///
/// ```ignore
/// aether_actor::native_only! {
///     use aether_substrate::capability::BootError;
///     use aether_substrate::native_actor::{NativeActor, NativeCtx, NativeInitCtx};
///
///     fn put_error_to_handle_error(e: PutError) -> HandleError { ... }
/// }
/// ```
///
/// expands to each `$item` annotated with `#[cfg(not(target_arch = "wasm32"))]`.
/// Pure mechanical wrap — no clever cfg, no feature flags.
#[macro_export]
macro_rules! native_only {
    ($($item:item)*) => {
        $( #[cfg(not(target_arch = "wasm32"))] $item )*
    };
}
