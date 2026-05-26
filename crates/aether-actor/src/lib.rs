//! aether-actor: transport-agnostic actor SDK shared by FFI guests
//! (today: wasm components) and native capabilities. Issue 552 stage 0
//! folded the prior `aether-component` crate's guest shim in here so
//! the SDK and its FFI binding layer share one home; issue 663 then
//! renamed the binding layer from `wasm` to `ffi` to reflect that any
//! host satisfying the `_p32` import surface can drive an actor
//! through it (the wasm runtime in `aether_substrate::actor::wasm` is
//! one consumer; future C / OS-process hosts would be others).
//!
//! ADR-0074 §Decision settled the actor model: components and
//! capabilities collapse into one actor primitive — one mpsc inbox,
//! one OS thread, one `MailboxId`. Issue 665 retired the unifying
//! `MailTransport` trait that originally tied the FFI and native
//! halves together — the cross-target abstraction is now the
//! per-stage capability traits in [`actor::ctx`]; the per-target
//! dispatch surfaces are [`ffi::bridge`] (FFI: [`ffi::MAIL_BRIDGE`],
//! [`ffi::PERSIST_BRIDGE`], [`ffi::SYNC_WAIT_BRIDGE`]) and the inherent methods on
//! `aether_substrate::actor::native::binding::NativeBinding`.
//!
//! Public surface:
//!   - [`Mail`], [`PriorState`], [`ReplyTo`], [`KindId`] —
//!     transport-free types: pure decode / phantom typing.
//!   - [`Mailbox`] — pure addressing token (`mailbox_id`, `kind_id`)
//!     after issue 665 dropped the `T: MailTransport` parameter; sends
//!     route through each ctx's send methods, not through the mailbox
//!     itself.
//!   - [`actor::ctx`] — per-stage capability traits ([`MailSender`],
//!     [`OutboundReply`], [`Resolver`], [`Persistence`],
//!     [`LifecycleControl`], `SyncWaiter`). FFI ctxs in [`ffi::ctx`]
//!     and substrate's `NativeCtx` family impl the relevant subset.
//!   - [`Slot`] — single-instance backing store the consumer's
//!     [`export!`] macro emits as a `static`.
//!   - [`WaitError`] + [`decode_wait_reply`] — ADR-0042 sync
//!     round-trip helpers; the per-target wait primitive plugs into
//!     `actor::ctx::sync_waiter::wait_reply_via`.
//!   - [`ffi`] — FFI binding layer: [`ffi::bridge`] dispatch ZSTs +
//!     [`FfiActor`] trait + [`Replaceable`] hook trait +
//!     [`FfiActorMailbox`] for the actor-typed sender chain +
//!     the [`export!`] macro that pins `init` / `receive` /
//!     lifecycle FFI exports plus the `aether.kinds.inputs` /
//!     `aether.namespace` custom-section statics.
//!
//! No FFI imports are pulled in unconditionally — the host-fn externs
//! in [`ffi::raw`] live behind a `#[cfg(target_arch = "wasm32")]`
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
pub mod cost;
pub mod ffi;
pub mod local;
pub mod log;
pub mod mail;
pub mod trace_ring;

pub use actor::ctx::{LifecycleControl, MailSender, OutboundReply, Persistence, Resolver};
pub use actor::sender::{MailCtx, Sender};
pub use actor::slot::Slot;
pub use actor::{
    Actor, HandlesKind, Instanced, NAMESPACE_SEGMENT_MAX_LEN, NamespaceError, Singleton,
    validate_namespace_segment,
};
pub use local::Local;
// Issue 665: `Mailbox<K, T>` and `ActorMailbox<'_, R, T>` retired; the
// surviving [`mail::mailbox::Mailbox<K>`] is a transport-free
// addressing token. Per-side actor-typed mailboxes live next to their
// transport: [`ffi::FfiActorMailbox<R>`] for FFI guests and
// `aether_substrate::actor::native::NativeActorMailbox<'a, R>` for
// native actors.
pub use mail::mailbox::{KindId, Mailbox, resolve, resolve_mailbox};
pub use mail::sync::{WaitError, decode_wait_reply};
pub use mail::{Mail, NO_REPLY_HANDLE, PriorState, ReplyTo};

// FFI surface promoted to the crate root so consumers see
// `aether_actor::FfiCtx<'_>` / `aether_actor::FfiActor` / etc. without
// an extra `ffi::` segment.
pub use ffi::{BootError, FfiActor, FfiActorMailbox, FfiCtx, FfiDropCtx, FfiInitCtx, Replaceable};

// Issue 665 retired `MailTransport` and its `MailTransportTrait`
// alias. Per-stage capability traits in `actor::ctx` are the
// cross-target abstraction; per-target dispatch lives in
// `ffi::bridge::*` (FFI) and `NativeBinding`'s inherent methods
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
    Kind, KindId as DataKindId, Schema, actor, bridge, capability, fallback, handler, local,
};
// `Singleton` and `Instanced` each live in two namespaces:
//   - the type namespace for the marker trait (`Actor` super-trait),
//     re-exported above as part of `actor::*`.
//   - the macro namespace for the `#[derive(Singleton)]` /
//     `#[derive(Instanced)]` proc-macros, forwarded through
//     `aether-data` (which itself re-exports them from
//     `aether-actor-derive` behind the `derive` feature).
// Rust resolves derive names from the macro namespace and type names
// from the type namespace, so the same identifier covers both
// `impl Singleton for X` / `impl Instanced for X` and the
// `#[derive(...)]` form without ambiguity at the user's call site.
// The `#[doc(inline)]` flattens the rustdoc page so the derives show
// up at `aether_actor::{Singleton, Instanced}` rather than under
// synthetic re-export shims. Issue 625 (ADR-0079) made the cardinality
// derives the explicit author-side surface — `#[bridge]` no longer
// auto-emits either.
#[doc(inline)]
pub use aether_data::{Instanced, Singleton};

/// Wrap one-or-more items in `#[cfg(not(target_arch = "wasm32"))]`.
/// Issue 552 stage 4's wasm-header-only build of `aether-capabilities`
/// gates per-cap native imports + helpers + impls behind that cfg;
/// this macro compresses what would otherwise be 5-7 sprinkled
/// attributes per cap file into one block per native-only chunk.
///
/// ```ignore
/// aether_actor::native_only! {
///     use aether_substrate::chassis::error::BootError;
///     use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
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
