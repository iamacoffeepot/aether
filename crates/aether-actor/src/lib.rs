//! Wasm guest SDK and transport-agnostic actor primitives, shared by wasm
//! components and native capabilities. Components and capabilities are one
//! actor primitive: one mpsc inbox, one OS thread, one `MailboxId` (ADR-0074).
//!
//! - [`Mail`], [`PriorState`], [`ReplyHandle`], [`KindId`]: transport-free
//!   types that decode bytes and carry phantom typing, nothing more.
//! - [`Mailbox`]: an addressing token (`mailbox_id`, `kind_id`). Sends go
//!   through a ctx's send methods, never through the mailbox itself.
//! - [`model::ctx`]: the per-stage capability traits ([`MailSender`],
//!   [`OutboundReply`], [`Persistence`]), the one abstraction the wasm and
//!   native targets share. [`wasm::ctx`] and the substrate's `NativeCtx`
//!   family each implement the relevant subset.
//! - [`Slot`]: the single-instance backing store [`export!`] emits as a
//!   `static`.
//! - [`wasm`]: the guest binding layer. [`wasm::bridge`] holds the dispatch
//!   functions, [`WasmActor`] is the trait a component implements (including
//!   the `on_dehydrate` / `on_rehydrate` hot-swap hooks, ADR-0101),
//!   [`WasmActorMailbox`] is the actor-typed sender chain, and [`export!`]
//!   pins the `init` / `receive` / lifecycle FFI exports plus the
//!   `aether.kinds.inputs` and `aether.namespace` custom-section statics.
//!
//! The FFI externs in [`wasm::raw`] sit behind `#[cfg(target_family = "wasm")]`
//! and the native stubs panic if called, so a host build links no FFI surface
//! and `cargo test --workspace` builds this crate like any other.

#![no_std]

extern crate alloc;

// Self-alias so proc-macros (today: `#[local]` in
// `aether-actor-derive`) that emit absolute paths like
// `::aether_actor::Local` resolve when used inside this crate
// itself — e.g., the `local` test module's probe newtypes.
// Outside callers don't need this; it's a no-op for them.
extern crate self as aether_actor;

pub mod asset;
pub mod local;
pub mod log;
pub mod mail;
pub mod model;
pub mod request_context;
pub mod trace;
pub mod wasm;

pub use asset::{AssetCatalog, AssetInfo, AssetWindow};
pub use local::Local;
pub use model::ctx::{Emit, MailSender, Manual, Multi, OutboundReply, Persistence, ReplyMode, Single};
pub use model::slot::Slot;
pub use model::{
    Actor, Addressable, CallerAddressable, CallerScope, CallerScoped, ChildOf, EMBEDDED_SCOPE, Embedded, EmbeddedMany,
    HandlesKind, Instanced, Lifecycle, Many, NAMESPACE_SEGMENT_MAX_LEN, NamespaceError, One, Publishes, Resolve, Root,
    Singleton, Subname, root_mailbox, validate_namespace_segment,
};
pub use request_context::{
    REQUEST_CONTEXT_CAPACITY, RequestContextTable, compose_state_envelope, split_state_envelope,
};
// Issue 665: `Mailbox<K, T>` and `ActorMailbox<'_, R, T>` retired; the
// surviving [`mail::mailbox::Mailbox<K>`] is a transport-free
// addressing token. Per-side actor-typed mailboxes live next to their
// transport: [`wasm::WasmActorMailbox<R>`] for wasm guests and
// `aether_substrate::actor::native::NativeActorMailbox<'a, R>` for
// native actors.
pub use mail::facade::MailboxForward;
pub use mail::mailbox::{KindId, Mailbox, resolve, resolve_mailbox};
pub use mail::{Mail, NO_REPLY_HANDLE, PriorState, RegistryChanged, ReplyHandle};

// Wasm surface promoted to the crate root so consumers see
// `aether_actor::WasmCtx<'_>` / `aether_actor::WasmActor` / etc. without
// an extra `wasm::` segment.
pub use wasm::{
    ActorInitError, ActorTypeTag, ErasedWasmActor, InlineChild, ModuleChild, RelativeMailbox, Sends, SpawnError,
    WasmActor, WasmActorMailbox, WasmActorMailboxWithContext, WasmCtx, WasmDispatch, WasmDropCtx, WasmInitCtx, WireCtx,
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
/// `aether_substrate::actor::wasm::component::DISPATCH_UNKNOWN_KIND` by value.
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
    pub use crate::wasm::{ActorTypeTag, WasmPlacementFacts};
    pub use aether_data::__derive_runtime::{Cow, KindLabels, SchemaType, canonical};
    pub use aether_data::{ActorId, Kind, Schema, mailbox_id_from_name};
    // Section-version bytes the `#[actor]` / `export!` writers emit as
    // token references so the literals const-fold from one source of
    // truth in `aether-data`.
    pub use aether_data::{
        ACTOR_LINEAGE_SECTION_VERSION, INPUTS_SECTION_VERSION, KINDS_SECTION_VERSION, LABELS_SECTION_VERSION,
        actor_lineage_child_len, actor_lineage_module_child_len, actor_lineage_root_len, write_actor_lineage_child,
        write_actor_lineage_module_child, write_actor_lineage_root,
    };
    // ADR-0096: the multi-actor `export!` arm stores the instance as
    // `Box<dyn ErasedWasmActor>`; re-export `Box` so the emitted code
    // doesn't depend on the guest crate's prelude exposing `alloc`.
    pub use alloc::boxed::Box;
    // Issue 2692: the `@spawn_inline_child_by_tag` resolver arm hands the
    // resolved subname to `spawn_one_child` as an owned `String`, so re-export
    // `String` the same way — the emitted code stays free of an `alloc`
    // prelude assumption on the guest crate.
    pub use alloc::string::String;
    // ADR-0113: the `#[actor]`-generated `on_rehydrate` warns through
    // `::aether_actor::__macro_internals::tracing::warn!` on a non-empty
    // decode-miss, so the macro roots the warn here rather than forcing
    // `tracing` into every component's dependency list.
    pub use tracing;
}

/// ADR-0033 actor-SDK attribute macros plus the data-layer
/// `Kind` / `Schema` re-exports. `Kind` / `Schema` / `KindId` /
/// `MailboxId` forward through `aether-data` so the derive paths the
/// macro emits (`::aether_data::Kind`, etc.) continue to resolve through
/// the established re-export chain. The actor-SDK attribute macros
/// (`actor`, `capability`, `fallback`, `handler`, `local`) are sourced
/// directly from `aether-actor-derive`; the data-layer derive macros are
/// sourced by `aether-data` from `aether-data-derive`. Component and
/// capability authors need only `aether-actor` in their dep list; the
/// full macro surface is available from here.
pub use aether_actor_derive::{actor, capability, export_asset, fallback, handler, handler_set, local, runtime};
pub use aether_data::{Kind, KindId as DataKindId, MailboxId, RequestId, Schema};
// ADR-0119: the `#[derive(Singleton)]` / `#[derive(Instanced)]` /
// `#[derive(Embeddable)]` proc-macros are retired. Cardinality is the
// `Addressable::Resolver`, and the `Singleton` / `Instanced` marker traits
// derive from it by blanket impl — so the only `aether_actor::{Singleton,
// Instanced}` surface now is the trait (re-exported via `actor::*` above).
