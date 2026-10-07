//! Wasm guest SDK and transport-agnostic actor primitives, shared by wasm
//! components and native capabilities. Components and capabilities are one
//! actor primitive: one mpsc inbox, one OS thread, one `MailboxId` (ADR-0074).
//!
//! - [`Mail`], [`PriorState`], [`ReplyHandle`]: transport-free types that
//!   decode bytes, nothing more.
//! - [`model::ctx`]: the per-stage capability traits ([`MailSender`],
//!   [`OutboundReply`], [`Persistence`]), the one abstraction the wasm and
//!   native targets share. [`wasm::ctx`] and the substrate's `NativeCtx`
//!   family each implement the relevant subset.
//! - [`Slot`]: the single-instance backing store [`export!`] emits as a
//!   `static`.
//! - [`Blob`], [`BlobReader`], [`MAX_READ_BYTES`]: immutable bytes as a value
//!   and their streaming reader, re-exported from `aether-data` (ADR-0238).
//! - [`wasm`]: the guest binding layer. [`wasm::bridge`] holds the dispatch
//!   functions, [`WasmActor`] is the trait a component implements (including
//!   the `on_dehydrate` / `on_rehydrate` hot-swap hooks, ADR-0101),
//!   [`WasmCtx`] carries the flat send verbs, and [`export!`]
//!   pins the `init` / `receive` / lifecycle FFI exports plus the
//!   `aether.kinds.inputs` and `aether.namespace` custom-section statics.
//!
//! The FFI externs (the `wasm` module's private `raw` module) sit behind
//! `#[cfg(target_family = "wasm")]` and the native stubs panic if called, so a
//! host build links no FFI surface and `cargo test --workspace` builds this
//! crate like any other.

#![no_std]

extern crate alloc;

// Self-alias so proc-macros (today: `#[local]` in
// `aether-actor-derive`) that emit absolute paths like
// `::aether_actor::Local` resolve when used inside this crate
// itself — e.g., the `local` test module's probe newtypes.
// Outside callers don't need this; it's a no-op for them.
extern crate self as aether_actor;

pub mod asset;
mod blob;
mod held_reply;
mod instant;
pub mod local;
pub mod log;
pub mod mail;
pub mod model;
mod path;
pub mod reference;
mod refusal_answer;
pub mod request_context;
mod sender_refused;
pub mod trace;
pub mod wasm;

pub use asset::{AssetInfo, Assets};
#[cfg(target_arch = "wasm32")]
#[doc(hidden)]
pub use blob::guest::__mint_guest_blob;
pub use held_reply::HeldReply;
#[doc(hidden)]
pub use instant::__mint_instant;
pub use instant::Instant;
pub use local::Local;
pub use model::ctx::{Erased, MailSender, OutboundReply, Persistence, ReplyMode, Single, Unchecked};
pub use model::slot::Slot;
pub use model::{
    Actor, Addressable, AllHandle, Anyone, At, CallerAddressable, CallerScope, CallerScoped, CastTarget, ChildOf,
    Contract, Contracts, CoveredBy, CoversRows, Declared, DependencyLink, DependencyList, DependencyResolver,
    DependsOn, Gap, HandlesKind, Here, Instanced, Lifecycle, ListIndex, Many, NAMESPACE_SEGMENT_MAX_LEN,
    NamespaceError, One, Protocol, Publisher, Publishes, Replies, ReplyShape, Resolve, Root, Row, RowAt, RowIndex,
    RowReply, RowSet, SendableTo, SenderRequirement, SentBy, Silent, SilentRow, Singleton, Subname, Subscriber, There,
    Undeclared, WatchTarget, Watchable, Watches, declared_dependencies, root_mailbox, validate_namespace_segment,
};
pub use path::{ActorPath, PathRefusal, PathRefused, ProtocolPath, ResolveError, TypedPath};
#[doc(hidden)]
pub use reference::{__mint_actor_ref, __mint_erased_actor_ref, __mint_protocol_ref};
pub use reference::{ActorRef, Direct, ErasedActorRef, HandsOff, ProtocolRef, Target};
pub use request_context::{RequestContextTable, split_state_envelope};
// The `resolve_path_p32` answer (ADR-0230 §3), the `published_rows_p32` and
// `route_rows_p32` answer (ADR-0231 §4, §3), and the `live_route_p32` answer
// (ADR-0230 §3, #7205), and the `actor_path_p32` answer (ADR-0231 §11), the
// position-to-path read beside them: the substrate's host fns encode them,
// and `WasmCtx::resolve_path`,
// `WasmCtx::cast`, a guest's `ProtocolPath` decode, `WasmCtx::resolve`, and a
// dispatch arm's refusal of its sender decode them.
#[doc(hidden)]
pub use wasm::bridge::address::{__ActorPath, __LiveRoute, __PublishedRows, __ResolvedPath};
// Both transports send through flat verbs and hold no typed handle: wasm
// actors through [`WasmCtx`], native actors through
// `aether_substrate::actor::native::NativeCtx`.
pub use mail::{Mail, NO_REPLY_HANDLE, PriorState, RegistryChanged, ReplyHandle};

// Wasm surface promoted to the crate root so consumers see
// `aether_actor::WasmCtx<'_>` / `aether_actor::WasmActor` / etc. without
// an extra `wasm::` segment.
pub use wasm::{
    ActorInitError, ActorTypeTag, Departed, ErasedWasmActor, HasParent, Held, InlineChild, InlineParent, Pending,
    Rebuildable, Sends, SpawnError, Spawns, WasmActor, WasmCtx, WasmDispatch, WasmDropCtx, WasmInitCtx, WireCtx,
};

// Issue 665 retired `MailTransport` and its `MailTransportTrait`
// alias. Per-stage capability traits in `actor::ctx` are the
// cross-target abstraction; per-target dispatch lives in
// `wasm::bridge::*` (wasm) and `NativeBinding`'s inherent methods
// (native).

/// Return code the `#[actor]`-synthesized dispatcher sends back up
/// through `receive_p32` when a `#[handler::unchecked(..)]` arm matched or the
/// `#[fallback]` ran (which by definition handles anything). Either may
/// keep the dispatch's reply handle and answer it later, so the handle
/// stays live until answered. Propagated verbatim by the consumer's FFI
/// shim; a guest built before [`DISPATCH_HANDLED_RELEASE`] existed
/// returns this from every arm and so keeps every handle.
pub const DISPATCH_HANDLED: u32 = 0;

/// Return code for "a single-class `#[handler]` arm matched and returned
/// with nothing held". It replied through the macro's `-> R` auto-reply, or
/// not at all, and armed no held reply, so no reply can follow once it
/// returns (ADR-0112); the substrate frees the dispatch's reply handle. A
/// single arm that returns a `Pending<R>` answers later and returns
/// [`DISPATCH_HANDLED_HOLD`] instead (ADR-0243 §6). Value 2 is the
/// substrate's host-only `DISPATCH_DROPPED_OVERSIZE` and is never returned
/// by a guest.
pub const DISPATCH_HANDLED_RELEASE: u32 = 3;

/// Return code for "a single arm returned a `Pending<R>`; keep the handle
/// and hold the requester's settlement" (ADR-0243 §6). The substrate keeps
/// the dispatch's reply handle live and holds the inbound's root open in
/// the handle's reply-table slot until the guest answers it, so the held
/// reply is sent on the requester's own chain. A host that predates it
/// reads it as an unrecognized class and keeps the handle, as for
/// [`DISPATCH_HANDLED`].
pub const DISPATCH_HANDLED_HOLD: u32 = 4;

/// Return code for "a single arm's handler requires something of its sender,
/// and this mail's sender does not cover it, so the handler did not run"
/// (ADR-0231 §11). The arm logged the refusal and sent no reply: it is a tell,
/// or a request whose mail has no sender to name in one. The substrate frees
/// the dispatch's reply handle, as for [`DISPATCH_HANDLED_RELEASE`], and
/// answers the refusal notice `aether.mail.decode_refused` to a reply target
/// that opted in to it, so a caller relayed through `aether.rpc.server` is
/// told. A request the arm answered itself returns
/// [`DISPATCH_HANDLED_RELEASE`] instead, so its caller gets one answer. A
/// host that predates it reads it as an unrecognized class and keeps the
/// handle, as for [`DISPATCH_HANDLED`].
pub const DISPATCH_REFUSED_SENDER: u32 = 5;

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
    // ADR-0231 §10: `export!` writes a type's `Dependency` records from its
    // `Declared::Depends` list with these.
    pub use crate::model::{dependency_records_len, write_dependency_records};
    // ADR-0231 §4: `#[protocol]` opts each protocol into the guard cast's
    // protocol arm with this marker.
    pub use crate::model::ProtocolCast;
    // ADR-0231 §3: a dispatch arm answers a refused typed path through the
    // row's reply with this selector, which refuses to compile when a
    // path-carrying request's reply cannot say so.
    pub use crate::refusal_answer::{Refusal, RefusalAnswer, refused_reply};
    // ADR-0231 §11: what both transports' dispatch arms build, log, and
    // answer from when a handler's sender requirement refuses the sender.
    pub use crate::sender_refused::SenderRefused;
    pub use crate::wasm::{ActorTypeTag, WasmPlacementFacts};
    pub use aether_data::__derive_runtime::{Cow, KindLabels, SchemaType, canonical};
    pub use aether_data::{ActorId, CrossesActors, Kind, KindId, ReplyContract, Schema};
    // ADR-0079 §8: the notice a guest's departure handlers share one row and
    // one dispatch arm for, which no author names.
    pub use aether_kinds::MonitorNotice;
    // Section-version bytes the `#[actor]` / `export!` writers emit as
    // token references so the literals const-fold from one source of
    // truth in `aether-data`.
    pub use aether_data::{
        ACTOR_LINEAGE_SECTION_VERSION, INPUTS_SECTION_VERSION, KINDS_SECTION_VERSION, LABELS_SECTION_VERSION,
        actor_lineage_child_len, actor_lineage_root_len, write_actor_lineage_child, write_actor_lineage_root,
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
    // The `#[actor]`-generated dispatch arms log through
    // `::aether_actor::__macro_internals::tracing`, so the macro roots the
    // log here rather than forcing `tracing` into every component's
    // dependency list.
    pub use tracing;
}

#[doc(hidden)]
pub use aether_actor_derive::__export_emit_classified;
/// ADR-0033 actor-SDK attribute macros plus the data-layer
/// `Kind` / `Schema` re-exports. `Kind` / `Schema` / `KindId` /
/// `MailboxId` forward through `aether-data` so the derive paths the
/// macro emits (`::aether_data::Kind`, etc.) continue to resolve through
/// the established re-export chain. The actor-SDK attribute macros
/// (`actor`, `capability`, `fallback`, `handler`, `local`, `protocol`) are
/// sourced directly from `aether-actor-derive`; the data-layer derive macros are
/// sourced by `aether-data` from `aether-data-derive`. Component and
/// capability authors need only `aether-actor` in their dep list; the
/// full macro surface is available from here.
pub use aether_actor_derive::{
    actor, capability, export_asset, fallback, handler, handler_set, local, protocol, runtime,
};
pub use aether_data::{Blob, BlobReader, MAX_READ_BYTES};
pub use aether_data::{Kind, KindId as DataKindId, MailboxId, RequestId, Schema, WatchId};
/// The engine's empty watch context: `ctx.watch(reference, NoContext)` for a
/// departure handler that takes no context parameter (ADR-0079 §8).
pub use aether_kinds::NoContext;
// ADR-0119: the `#[derive(Singleton)]` / `#[derive(Instanced)]` /
// `#[derive(Embeddable)]` proc-macros are retired. Cardinality is the
// `Addressable::Resolver`, and the `Singleton` / `Instanced` marker traits
// derive from it by blanket impl — so the only `aether_actor::{Singleton,
// Instanced}` surface now is the trait (re-exported via `actor::*` above).
