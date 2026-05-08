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
//! one OS thread, one `MailboxId` — and share this SDK over two
//! transport implementations. The FFI side rides
//! [`ffi::FfiTransport`]; the native side rides
//! `aether_substrate::actor::native::transport::NativeTransport`.
//!
//! Public surface:
//!   - [`MailTransport`] — the five-method trait every transport
//!     impl must provide; signatures mirror the wasm `_p32` FFI
//!     byte-for-byte.
//!   - [`Mail`], [`PriorState`], [`ReplyTo`], [`KindId`] —
//!     transport-free types: pure decode / phantom typing.
//!   - [`Mailbox`] — generic over `T: MailTransport`; method bodies
//!     dispatch through `T::*`. The 1-arg FFI alias
//!     ([`ffi::Mailbox<K>`]) lives in [`ffi`] alongside
//!     [`ffi::FfiTransport`].
//!   - [`actor::ctx`] — per-stage capability traits ([`MailSender`],
//!     [`OutboundReply`], [`Resolver`], [`Persistence`],
//!     [`LifecycleControl`]). FFI ctxs in [`ffi::ctx`] and substrate's
//!     `NativeCtx` family impl the relevant subset.
//!   - [`Slot`] — single-instance backing store the consumer's
//!     [`export!`] macro emits as a `static`.
//!   - [`WaitError`] + [`wait_reply`] + [`decode_wait_reply`] —
//!     ADR-0042 sync round-trip helper, generic over the reply kind,
//!     the error enum, and the transport.
//!   - [`ffi`] — FFI binding layer: [`ffi::FfiTransport`] +
//!     [`ffi::FfiActor`] trait + [`ffi::Replaceable`] hook trait + the
//!     [`export!`] macro that pins `init` / `receive` / lifecycle FFI
//!     exports plus the `aether.kinds.inputs` / `aether.namespace`
//!     custom-section statics.
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
pub mod ffi;
pub mod local;
pub mod log;
pub mod mail;

pub use actor::ctx::{LifecycleControl, MailSender, OutboundReply, Persistence, Resolver};
pub use actor::sender::{MailCtx, Sender};
pub use actor::slot::Slot;
pub use actor::{
    Actor, Dispatch, HandlesKind, Instanced, NAMESPACE_SEGMENT_MAX_LEN, NamespaceError, Singleton,
    validate_namespace_segment,
};
pub use local::Local;
// Generic 2-arg `Mailbox<K, T>` stays accessible as
// `aether_actor::mail::mailbox::Mailbox`. At the crate root we
// re-export the 1-arg FFI alias (defined in `ffi`) under the same
// `Mailbox` name so existing `aether_component::Mailbox<K>` consumers
// keep their call shape when migrating to `aether_actor::*`.
pub use mail::mailbox::{ActorMailbox, KindId, resolve, resolve_mailbox};
pub use mail::sync::{WaitError, decode_wait_reply, wait_reply};
pub use mail::transport::MailTransport;
pub use mail::{Mail, NO_REPLY_HANDLE, PriorState, ReplyTo};

// FFI surface promoted to the crate root so consumers see
// `aether_actor::FfiCtx<'_>` / `aether_actor::FfiActor` / etc. without
// an extra `ffi::` segment. `Mailbox<K>` here is the FFI 1-arg alias —
// the generic form is reachable through `mail::mailbox::Mailbox<K, T>`.
pub use ffi::{
    BootError, FFI_TRANSPORT, FfiActor, FfiCtx, FfiDropCtx, FfiInitCtx, FfiTransport, Mailbox,
    Replaceable,
};

// Issue 442 / ADR-0033: `MailTransport` doubles as a re-export name
// for the trait when consumers want to spell out the bound. Kept
// separate from the [`MailTransport`] re-export above for code that
// wrote `aether_component::MailTransportTrait` against the prior
// alias.
pub use mail::transport::MailTransport as MailTransportTrait;

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
    /// Issue #601: `#[actor]` / `#[handlers]` auto-emit a synthetic
    /// dispatch arm for `ConfigureLogDrain` so every actor accepts the
    /// chassis's log-drain push without user code declaring a handler.
    /// Re-exported here so the macro emits a path through `aether-actor`
    /// and consumer crates aren't forced to depend on `aether-kinds`
    /// directly.
    pub use aether_kinds::ConfigureLogDrain;
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
