//! aether-component: guest-side SDK for WASM components that run on the
//! substrate. See ADR-0012 for motivation — this crate wraps the raw
//! `extern "C"` + `static mut u32` + sentinel pattern every component
//! previously wrote by hand, and presents typed handles instead.
//!
//! Shipped surfaces:
//!   - ADR-0012: `Sink<K>`, `KindId<K>`, `resolve::<K>()`, `resolve_sink::<K>(name)`,
//!     and the `raw` FFI module with host-target panicking stubs.
//!   - ADR-0014: `Component` trait (`init` / `receive`), typed `InitCtx`
//!     and `Ctx` with init-vs-receive capability fencing, `Mail<'_>` for
//!     inbound with typed `decode` / `decode_slice`, and the `export!`
//!     macro that owns the `#[no_mangle]` init/receive shims.
//!   - ADR-0015: Additive lifecycle hooks on the `Component` trait
//!     (`on_replace`, `on_drop`, `on_rehydrate`, all defaulted), plus
//!     the narrowed `DropCtx<'_>` capability type. `export!` emits the
//!     matching `#[no_mangle]` shims and the substrate invokes them at
//!     `drop_component` and `replace_component` call sites. Components
//!     that don't override stay green; hook traps are contained so a
//!     panicking hook doesn't stall teardown.
//!   - ADR-0016: Persistent state across hot reload. `DropCtx::save_state`
//!     lets `on_replace` deposit a version-tagged byte bundle; the
//!     substrate hands it to the new instance via `on_rehydrate` with
//!     a populated `PriorState<'_>`. Opt-in — components that don't
//!     override either hook migrate nothing.
//!   - ADR-0041: Guest helpers for the substrate's file I/O sink.
//!     `io::read` / `io::write` / `io::delete` / `io::list` build
//!     the typed request kinds, postcard-encode them, and send to
//!     the substrate's `"io"` mailbox. Replies arrive as the paired
//!     `*Result` kinds — declare `#[handler]` methods to consume
//!     them. See `io` module rustdoc for the typical save-loader
//!     shape.
//!   - ADR-0040: Kind-typed state on top of ADR-0016.
//!     `DropCtx::save_state_kind::<K>` prepends `K::ID` to the
//!     postcard encoding of `value` and writes the concatenation
//!     through the unchanged host fn; `PriorState::as_kind::<K>`
//!     reads the leading id and decodes on match, returning `None`
//!     on mismatch so schema evolution (different `K::ID`) reboots
//!     fresh. The raw `save_state` / `bytes()` API stays legal for
//!     non-kind blobs and explicit migration flows.
//!   - ADR-0013: Reply-to-sender. `Mail::sender()` returns `Some(ReplyTo)`
//!     for mail that came from a Claude session; `Ctx::reply` takes a
//!     `Sender` and a typed `KindId<K>` to answer the originating
//!     session. The 4-param `receive(kind, ptr, count, sender)` ABI is
//!     absorbed by the `export!` macro so component authors don't see it.
//!   - ADR-0033 (supersedes ADR-0027): `#[handlers]` on an
//!     `impl Component for T` block is the one and only receive path.
//!     The attribute macro walks `#[handler]` methods (kind inferred
//!     from the third param), an optional `#[fallback]`, and per-method
//!     rustdoc (with `# Agent` section filter), and emits: (a) an
//!     inherent `__aether_dispatch(&mut self, ctx, mail) -> u32` method
//!     on the component type that `export!`'s `receive_p32` shim calls,
//!     (b) input-subscribe calls prepended to the user's `init` so every
//!     `K::IS_INPUT` handler kind gets wired to the substrate's stream,
//!     and (c) `aether.kinds.inputs` section statics the hub reads at
//!     `load_component` for MCP capability surfacing. The dispatcher
//!     returns `DISPATCH_UNKNOWN_KIND` on a strict-receiver miss so the
//!     scheduler's warn path actually fires. ADR-0027's `type Kinds`
//!     typelist, `KindList`/`Cons`/`Nil`, `KindTable`, and the trait
//!     `Component::receive` method are all retired.

#![no_std]

extern crate alloc;

use core::any::TypeId;
use core::marker::PhantomData;

use aether_mail::{Kind, Ref, Schema, mailbox_id_from_name};

pub mod io;
pub mod net;
pub mod raw;

/// ADR-0033 attribute macros. Applied to `impl Component for C`
/// blocks: `#[handlers]` at the impl level; `#[handler]` on each
/// typed handler method; `#[fallback]` on an optional catchall.
/// Forwarded from `aether-mail-derive` so the full component
/// vocabulary sits behind one `use aether_component::*` line.
pub use aether_mail::{fallback, handler, handlers};

/// Return code the `#[handlers]`-synthesized dispatcher sends back up
/// through `receive_p32` when a `#[handler]` arm matched (or the
/// `#[fallback]` ran, which by definition handles anything). Propagated
/// verbatim by `export!`'s FFI shim.
pub const DISPATCH_HANDLED: u32 = 0;

/// Return code for "no `#[handler]` matched and there's no `#[fallback]`"
/// — the strict-receiver miss. Propagated through the FFI so the
/// substrate's scheduler can emit a `tracing::warn!` naming the
/// mailbox + kind (ADR-0033 §Strict receivers, issue #142). Matches
/// `aether_substrate::component::DISPATCH_UNKNOWN_KIND` by value.
pub const DISPATCH_UNKNOWN_KIND: u32 = 1;

/// Re-exports the `#[handlers]` macro relies on at expansion sites
/// that don't depend on `aether-mail` directly (e.g., component
/// crates that only pull in `aether-component`). Keeping the macro's
/// emitted paths rooted at `::aether_component::__macro_internals`
/// removes the "add aether-mail to your Cargo.toml" boilerplate that
/// `::aether_mail::...` paths would otherwise force on every consumer.
///
/// Not part of the public API; the macro is the only intended caller.
#[doc(hidden)]
pub mod __macro_internals {
    pub use aether_mail::__derive_runtime::{Cow, KindLabels, SchemaType, canonical};
    pub use aether_mail::{Kind, Schema};
}

/// Phantom-typed wrapper around a resolved kind id. A `KindId<Tick>`
/// cannot be passed where a `KindId<DrawTriangle>` is expected — the
/// mismatch is a compile error rather than a runtime bad-dispatch.
///
/// Constructed via `resolve::<K>()` during component init. The raw
/// id is retrievable via `.raw()` for comparison against incoming
/// `kind` parameters in a hand-rolled `receive` shim (ADR-0014's
/// `Mail::decode` will make the raw-int compare go away).
pub struct KindId<K: Kind> {
    raw: u64,
    _k: PhantomData<fn() -> K>,
}

impl<K: Kind> Copy for KindId<K> {}
impl<K: Kind> Clone for KindId<K> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<K: Kind> PartialEq for KindId<K> {
    fn eq(&self, other: &Self) -> bool {
        self.raw == other.raw
    }
}
impl<K: Kind> Eq for KindId<K> {}

impl<K: Kind> KindId<K> {
    /// The raw kind id the substrate assigned. Exposed for hand-rolled
    /// receive shims that `match` on the inbound `kind: u64` parameter.
    pub fn raw(self) -> u64 {
        self.raw
    }

    /// Returns `true` if `raw` is the id the substrate assigned to `K`.
    /// Convenience over `kind_id.raw() == raw`.
    pub fn matches(self, raw: u64) -> bool {
        self.raw == raw
    }
}

/// Phantom-typed send target. Wraps a mailbox id plus the kind id that
/// the sink accepts. `Sink<DrawTriangle>` can only `send` a
/// `&DrawTriangle` or `&[DrawTriangle]` — the kind is fixed at
/// resolution time.
///
/// Built via `resolve_sink::<K>(name)` during init.
pub struct Sink<K: Kind> {
    mailbox: u64,
    kind: u64,
    _k: PhantomData<fn() -> K>,
}

impl<K: Kind> Copy for Sink<K> {}
impl<K: Kind> Clone for Sink<K> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<K: Kind> Sink<K> {
    /// Raw mailbox id. Exposed for components that need to pass the
    /// id to a host fn not yet wrapped by the SDK.
    pub fn mailbox(self) -> u64 {
        self.mailbox
    }

    /// Raw kind id. Exposed for the same reason as `mailbox`.
    pub fn kind(self) -> u64 {
        self.kind
    }
}

impl<K: Kind + bytemuck::NoUninit> Sink<K> {
    /// Send a single typed payload. The substrate's `count` field is 1.
    /// Bytemuck handles the `&K → &[u8]` cast.
    pub fn send(self, payload: &K) {
        let bytes = bytemuck::bytes_of(payload);
        unsafe {
            raw::send_mail(
                self.mailbox,
                self.kind,
                bytes.as_ptr().addr() as u32,
                bytes.len() as u32,
                1,
            );
        }
    }

    /// Send a slice of typed payloads as a contiguous buffer. The
    /// substrate's `count` field is `payloads.len()`.
    pub fn send_many(self, payloads: &[K]) {
        let bytes: &[u8] = bytemuck::cast_slice(payloads);
        unsafe {
            raw::send_mail(
                self.mailbox,
                self.kind,
                bytes.as_ptr().addr() as u32,
                bytes.len() as u32,
                payloads.len() as u32,
            );
        }
    }
}

impl<K: Kind + serde::Serialize> Sink<K> {
    /// Send a single postcard-encoded payload. Sibling of [`Sink::send`]
    /// for schema-shaped kinds (`#[derive(Schema, Serialize)]` — e.g.
    /// `Count`, `SubscribeInput`, the `io::*` request kinds) that
    /// aren't bytemuck-castable. The substrate's `count` field is 1.
    ///
    /// No `send_postcard_many` — postcard has no efficient contiguous
    /// batch shape, so batch sends stay bytemuck-only. A component
    /// that wants to fan out N postcard payloads calls this in a loop.
    pub fn send_postcard(self, payload: &K) {
        let bytes = postcard::to_allocvec(payload).expect("postcard encode to Vec is infallible");
        unsafe {
            raw::send_mail(
                self.mailbox,
                self.kind,
                bytes.as_ptr().addr() as u32,
                bytes.len() as u32,
                1,
            );
        }
    }
}

/// ADR-0045 typed-handle wrapper around a substrate-side handle id.
/// Created by [`Ctx::publish`] and friends; carries an RAII drop-
/// release to drop the publisher's refcount when the handle goes out
/// of scope. `K` is phantom — the id-as-bytes representation is
/// type-agnostic, but `as_ref` pulls `K::ID` to construct a
/// type-aligned `Ref::Handle`.
///
/// Not `Copy`. Cloning a refcounted handle without inc-ref'ing
/// would cause a double-release on drop; if a component genuinely
/// needs multiple references it pins the handle and reads the raw
/// id.
///
/// Sending: build the wire-shaped value with [`Handle::as_ref`] in
/// the `Ref<K>` field of an outgoing kind, then send the parent
/// kind through any existing `Sink<_>::send_postcard`. The handle
/// itself stays in the sender's hands until drop / explicit
/// release; the substrate's dispatch path resolves the wire ref
/// against the cached bytes before delivery.
pub struct Handle<K> {
    id: u64,
    _k: PhantomData<fn() -> K>,
}

impl<K> Handle<K> {
    /// Raw handle id. Exposed for hand-rolled callers that need to
    /// pass the id to a host fn the SDK doesn't yet wrap, or to
    /// detach it from the RAII guard via [`core::mem::forget`].
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Pin against LRU eviction. Useful when the publisher wants to
    /// release its local guard (drop the `Handle`) without losing
    /// the cached bytes — pin first, then drop.
    pub fn pin(&self) {
        unsafe {
            raw::handle_pin(self.id);
        }
    }

    /// Clear the pinned flag.
    pub fn unpin(&self) {
        unsafe {
            raw::handle_unpin(self.id);
        }
    }

    /// Drop the publisher's reference and consume the handle.
    /// Equivalent to `drop(handle)` but explicit. Returns the
    /// underlying id so callers that pinned first can keep using
    /// it after release.
    pub fn release(self) -> u64 {
        let id = self.id;
        // Capture the id, suppress the Drop impl, call release
        // once. The Drop impl would otherwise double-release.
        core::mem::forget(self);
        unsafe {
            raw::handle_release(id);
        }
        id
    }
}

impl<K: Kind> Handle<K> {
    /// Wire-shaped reference to this handle. Embed in a `Ref<K>`
    /// field on an outgoing kind so the substrate's dispatch path
    /// resolves the inline bytes before delivery. The handle keeps
    /// its refcount on the publisher side — `as_ref` is a borrow,
    /// not a transfer.
    pub fn as_ref(&self) -> Ref<K> {
        Ref::Handle {
            id: self.id,
            kind_id: K::ID,
        }
    }
}

impl<K> Drop for Handle<K> {
    fn drop(&mut self) {
        // dec_ref saturates at zero on the substrate side — calling
        // release on an already-released handle is a no-op success.
        // Failures (no store wired, unknown id) are silent because a
        // panicking Drop is poison for ADR-0015 trap containment.
        unsafe {
            raw::handle_release(self.id);
        }
    }
}

/// Postcard-encode `value` and call the `handle_publish` host fn.
/// Shared by `InitCtx::publish` / `Ctx::publish` / `DropCtx::publish`.
/// Returns `None` when the substrate signals failure via the `0`
/// sentinel (no store wired, OOB pointer, eviction-failed).
fn publish_value<K: Kind + serde::Serialize>(value: &K) -> Option<Handle<K>> {
    let bytes = postcard::to_allocvec(value).expect("postcard encode to Vec is infallible");
    let id =
        unsafe { raw::handle_publish(K::ID, bytes.as_ptr().addr() as u32, bytes.len() as u32) };
    if id == 0 {
        return None;
    }
    Some(Handle {
        id,
        _k: PhantomData,
    })
}

/// Resolve a kind, producing a typed id from the `const ID` the derive
/// emits on the `Kind` impl. ADR-0030 Phase 2 made kind ids a pure
/// function of `(name, schema)` at compile time — no host-fn round
/// trip, no "kind not registered" failure mode at the guest boundary.
/// The substrate and guest compute the same id independently; a
/// mismatch means one side was compiled against a different schema
/// revision, and that surfaces as "kind not found" on the first mail.
pub const fn resolve<K: Kind>() -> KindId<K> {
    KindId {
        raw: K::ID,
        _k: PhantomData,
    }
}

/// Bind a mailbox name to kind `K`, producing a typed `Sink<K>`. The
/// mailbox id is derived from the name client-side (ADR-0029 stable
/// hash) and the kind id is `K::ID` (ADR-0030 Phase 2). No host-fn
/// round trip, no requirement that the target mailbox or kind already
/// exist on the substrate side at init time.
pub const fn resolve_sink<K: Kind>(mailbox_name: &str) -> Sink<K> {
    Sink {
        mailbox: mailbox_id_from_name(mailbox_name),
        kind: K::ID,
        _k: PhantomData,
    }
}

/// User-implemented component. ADR-0014 commits to `Self`-is-state —
/// cached kind ids, cached sinks, and any domain fields live on the
/// implementor. `init` runs once before any `receive`; `receive` is
/// called with the stored `&mut self` on every inbound mail.
///
/// The `#[no_mangle]` `init` / `receive` exports that actually cross
/// the WASM FFI are generated by `export!(MyComponent)`; implementors
/// do not write `extern "C"` by hand.
pub trait Component: Sized + 'static {
    /// Runs once. Resolve kinds and sinks via `ctx` and return the
    /// initial component state. A failed `resolve` panics — see
    /// ADR-0012 §2 ("loud at init"). ADR-0033: `#[handlers]` prepends
    /// `ctx.subscribe_input::<K>()` for every `K::IS_INPUT` handler
    /// kind so the user body never needs to do it by hand.
    fn init(ctx: &mut InitCtx<'_>) -> Self;

    /// Called once on the old instance, immediately before a
    /// `replace_component` swap (ADR-0015 §3). Default is no-op;
    /// override to serialize state that the new instance can consume
    /// through `on_rehydrate`, or to emit farewell mail. Prefer
    /// `DropCtx::save_state_kind::<K>` (ADR-0040) to let the kind
    /// system carry schema identity; reach for the raw
    /// `DropCtx::save_state` only when persisting a non-kind blob or
    /// driving an explicit migration off the leading id.
    fn on_replace(&mut self, ctx: &mut DropCtx<'_>) {
        let _ = ctx;
    }

    /// Called once on the instance being dropped — both for
    /// `drop_component` and for the old instance of
    /// `replace_component` — immediately before the substrate tears
    /// down linear memory. Default is no-op; override for cleanup
    /// (sending "goodbye" mail, flushing work to a sibling component,
    /// logging).
    fn on_drop(&mut self, ctx: &mut DropCtx<'_>) {
        let _ = ctx;
    }

    /// Called after `init` on a freshly-instantiated component that
    /// is replacing an older instance, if and only if the predecessor
    /// produced a state bundle via `DropCtx::save_state` (ADR-0016 §3)
    /// or `DropCtx::save_state_kind` (ADR-0040). Default ignores the
    /// prior state; components that persist across replace override to
    /// rehydrate — typically `prior.as_kind::<MyState>()` for kind-
    /// typed saves, or `prior.bytes()` + `prior.schema_version()` for
    /// the raw path.
    fn on_rehydrate(&mut self, ctx: &mut Ctx<'_>, prior: PriorState<'_>) {
        let _ = ctx;
        let _ = prior;
    }
}

/// Init-only capability handle. The type split between `InitCtx` and
/// `Ctx` fences "when can I resolve?" (init only) and "when can I
/// send?" (receive only) at compile time — calling `resolve` from a
/// `&mut Ctx` is a type error, not a convention.
///
/// The component's own mailbox id rides here — the substrate passes it
/// into `init` at instantiation (ADR-0030 Phase 2) and the SDK uses
/// it to self-address `aether.control.subscribe_input` mails for
/// every `K::IS_INPUT` kind in `Component::Kinds`.
pub struct InitCtx<'a> {
    mailbox: u64,
    _borrow: PhantomData<&'a ()>,
}

impl InitCtx<'_> {
    /// Not part of the public API; called only by `export!`.
    #[doc(hidden)]
    pub fn __new(mailbox: u64) -> Self {
        InitCtx {
            mailbox,
            _borrow: PhantomData,
        }
    }

    /// The component's own mailbox id — the value the substrate uses
    /// to address `receive` calls to this instance. Useful for
    /// hand-rolled subscribe / self-mailing at init time when the
    /// SDK's higher-level wrappers don't fit.
    pub fn mailbox_id(&self) -> u64 {
        self.mailbox
    }

    /// Resolve a kind by its `const ID`. Pure compile-time construction
    /// under ADR-0030 Phase 2 — no host-fn round trip, never fails.
    pub const fn resolve<K: Kind>(&self) -> KindId<K> {
        resolve::<K>()
    }

    /// Resolve a mailbox by name and bind it to kind `K`, producing a
    /// typed `Sink<K>`. Pure compile-time construction.
    pub const fn resolve_sink<K: Kind>(&self, name: &str) -> Sink<K> {
        resolve_sink::<K>(name)
    }

    /// Publish `value` into the substrate's handle store at init.
    /// See [`Ctx::publish`] for semantics.
    pub fn publish<K: Kind + serde::Serialize>(&self, value: &K) -> Option<Handle<K>> {
        publish_value::<K>(value)
    }

    /// Send `aether.control.subscribe_input` with this component's
    /// mailbox as the subscriber for `K`'s stream. Called by
    /// `KindList::resolve_all` for every `K::IS_INPUT` kind — ADR-0030
    /// Phase 2 moved the subscribe side effect out of `resolve_kind`
    /// and into the guest SDK. No-op if `K` isn't one of the four
    /// known substrate input kind types (input kinds defined downstream
    /// of aether-kinds get to pick their own subscribe path).
    ///
    /// Stream selection goes through `TypeId` rather than `K::NAME`
    /// so a future rename on either side of the pairing surfaces as a
    /// type error instead of silently skipping the subscribe.
    pub fn subscribe_input<K: Kind + 'static>(&self) {
        use aether_kinds::{
            InputStream, Key, KeyRelease, MouseButton, MouseMove, SubscribeInput, Tick, WindowSize,
        };
        let tid = TypeId::of::<K>();
        let stream = if tid == TypeId::of::<Tick>() {
            InputStream::Tick
        } else if tid == TypeId::of::<Key>() {
            InputStream::Key
        } else if tid == TypeId::of::<KeyRelease>() {
            InputStream::KeyRelease
        } else if tid == TypeId::of::<MouseMove>() {
            InputStream::MouseMove
        } else if tid == TypeId::of::<MouseButton>() {
            InputStream::MouseButton
        } else if tid == TypeId::of::<WindowSize>() {
            InputStream::WindowSize
        } else {
            return;
        };
        let payload = SubscribeInput {
            stream,
            mailbox: self.mailbox,
        };
        resolve_sink::<SubscribeInput>("aether.control").send_postcard(&payload);
    }
}

/// Per-receive capability handle. Exposes send primitives only.
/// Resolution is intentionally absent — runtime resolution after init
/// is not a supported shape.
///
/// ADR-0033: typed handlers receive `K` by value, so they no longer
/// hold a `Mail<'_>` to call `mail.reply_to()` on. The synthesized
/// dispatcher threads the inbound mail's sender onto `Ctx` via
/// `__set_reply_to` before every handler call, and `Ctx::sender()`
/// reads it back. `#[fallback]` methods still receive the raw
/// `Mail<'_>` and can call `mail.reply_to()` directly.
pub struct Ctx<'a> {
    sender: Option<u32>,
    _borrow: PhantomData<&'a ()>,
}

impl Ctx<'_> {
    /// Not part of the public API; called only by `export!`.
    #[doc(hidden)]
    pub fn __new() -> Self {
        Ctx {
            sender: None,
            _borrow: PhantomData,
        }
    }

    /// Not part of the public API; called only by the `#[handlers]`
    /// dispatcher. Accepts `None` or `Some(ReplyTo)` — the dispatcher
    /// passes `mail.reply_to()` verbatim so component-origin and
    /// broadcast mail (which have no reply target) land as `None`.
    #[doc(hidden)]
    pub fn __set_reply_to(&mut self, sender: Option<ReplyTo>) {
        self.sender = sender.map(|s| s.raw);
    }

    /// Reply handle for the mail currently being dispatched. `None`
    /// for component-origin and broadcast-origin mail; `Some(ReplyTo)`
    /// when the inbound came from a Claude session. Pass the returned
    /// `Sender` back to `Ctx::reply` to answer the originating
    /// session (ADR-0013).
    pub fn reply_to(&self) -> Option<ReplyTo> {
        self.sender.map(|raw| ReplyTo { raw })
    }

    /// Send a single payload to `sink`. Typed wrapper around
    /// `Sink::send` — having the same entry point through both
    /// `Ctx` and `Sink` is deliberate: `Ctx` is the receive-time
    /// vocabulary, `Sink::send` is the universal one.
    pub fn send<K: Kind + bytemuck::NoUninit>(&self, sink: &Sink<K>, payload: &K) {
        sink.send(payload);
    }

    /// Send a slice of payloads as a contiguous batch.
    pub fn send_many<K: Kind + bytemuck::NoUninit>(&self, sink: &Sink<K>, payloads: &[K]) {
        sink.send_many(payloads);
    }

    /// Send a postcard-encoded payload. Sibling of [`Ctx::send`] for
    /// schema-shaped kinds — same dispatch discipline (receive-time
    /// capability), different wire shape.
    pub fn send_postcard<K: Kind + serde::Serialize>(&self, sink: &Sink<K>, payload: &K) {
        sink.send_postcard(payload);
    }

    /// Publish `value` into the substrate's handle store and return
    /// a typed [`Handle<K>`]. The publisher holds an initial
    /// refcount; dropping the handle releases it. Returns `None` on
    /// substrate-side failure (no store wired, OOB pointer,
    /// eviction-failed).
    pub fn publish<K: Kind + serde::Serialize>(&self, value: &K) -> Option<Handle<K>> {
        publish_value::<K>(value)
    }

    /// Reply to the Claude session that originated the inbound mail
    /// (ADR-0013). `sender` came from `mail.reply_to()` on the current
    /// receive — pass it back as the routing handle. The kind is
    /// supplied as a typed `KindId<K>` so the same compile-time
    /// matching the rest of the SDK uses applies here too.
    ///
    /// Status of the underlying host call is dropped; reply is
    /// fire-and-forget on the guest side. If the session is gone the
    /// hub silently discards the frame.
    pub fn reply<K: Kind + bytemuck::NoUninit>(
        &self,
        sender: ReplyTo,
        kind: KindId<K>,
        payload: &K,
    ) {
        let bytes = bytemuck::bytes_of(payload);
        unsafe {
            raw::reply_mail(
                sender.raw,
                kind.raw,
                bytes.as_ptr().addr() as u32,
                bytes.len() as u32,
                1,
            );
        }
    }
}

/// Sentinel the substrate passes as the reply-handle parameter on
/// the `receive` shim when there is no reply target — for
/// component-originated mail (no Claude session involved) and for
/// broadcast-origin mail. `Mail::reply_to()` returns `None` in this
/// case; `ReplyTo` is only constructable via the `Mail` accessor.
pub const NO_REPLY_HANDLE: u32 = u32::MAX;

/// Opaque per-instance handle identifying the reply destination for
/// an inbound mail. Pass it back to `Ctx::reply` to answer — the
/// substrate routes it to the right target (a Claude MCP session,
/// another local component, or a remote engine's mailbox) depending
/// on where the inbound came from. Mail is pushed at a recipient
/// and has no real "from" concept; this handle is purely a
/// reply-to address.
///
/// `Copy` because the handle is a `u32` underneath; cloning is free.
/// Cloning is also fine for stashing across receives — the substrate
/// guarantees the handle stays valid for the lifetime of the
/// receiving component instance.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ReplyTo {
    raw: u32,
}

impl ReplyTo {
    /// Raw handle value. Exposed for components that need to call a
    /// host fn the SDK doesn't yet wrap.
    pub fn raw(self) -> u32 {
        self.raw
    }
}

/// Narrowed capability handle for shutdown hooks (`on_replace`,
/// `on_drop`). Like `Ctx`, but deliberately smaller:
///
/// - `send` / `send_many` still work — outbound mail during shutdown
///   is a valid and useful pattern ("I'm going away, here's the last
///   thing I observed").
/// - `save_state` is only meaningful in `on_replace` — it deposits a
///   version-tagged byte bundle the substrate hands to the new
///   instance via `on_rehydrate`. Calling it from `on_drop` is
///   technically accepted by the host fn, but the bytes are then
///   discarded (ADR-0016 §5 — plain drops have no successor).
/// - No `reply` — sender handles invalidate on teardown; a reply
///   attempt during `on_drop` cannot be honored.
/// - No `resolve` — resolution belongs at init. There is no use case
///   for resolving at teardown.
pub struct DropCtx<'a> {
    _borrow: PhantomData<&'a ()>,
}

impl DropCtx<'_> {
    /// Not part of the public API; called only by `export!`.
    #[doc(hidden)]
    pub fn __new() -> Self {
        DropCtx {
            _borrow: PhantomData,
        }
    }

    /// Send a single payload during a shutdown hook.
    pub fn send<K: Kind + bytemuck::NoUninit>(&self, sink: &Sink<K>, payload: &K) {
        sink.send(payload);
    }

    /// Send a slice of payloads during a shutdown hook.
    pub fn send_many<K: Kind + bytemuck::NoUninit>(&self, sink: &Sink<K>, payloads: &[K]) {
        sink.send_many(payloads);
    }

    /// Send a postcard-encoded payload during a shutdown hook. Sibling
    /// of [`DropCtx::send`] for schema-shaped kinds.
    pub fn send_postcard<K: Kind + serde::Serialize>(&self, sink: &Sink<K>, payload: &K) {
        sink.send_postcard(payload);
    }

    /// Publish `value` into the substrate's handle store during a
    /// shutdown hook. See [`Ctx::publish`] for semantics — the
    /// returned handle's RAII drop releases the publisher refcount,
    /// which on `on_replace` typically means "pin first, then drop"
    /// so the cached entry survives the hand-off to the next
    /// instance.
    pub fn publish<K: Kind + serde::Serialize>(&self, value: &K) -> Option<Handle<K>> {
        publish_value::<K>(value)
    }

    /// Deposit a migration bundle for the substrate to hand to the
    /// replacement instance via `on_rehydrate`. `version` is
    /// component-defined (the substrate doesn't interpret it); bytes
    /// are copied into a substrate-owned buffer immediately, so the
    /// caller is free to drop the slice on return.
    ///
    /// Panics if the substrate rejects the call — today that's only
    /// the 1 MiB cap being exceeded or an internal OOB, both of
    /// which are component bugs. ADR-0015's trap containment ensures
    /// the panic doesn't stall teardown on the substrate side.
    ///
    /// May be called zero or one times per `on_replace`; a second
    /// call overwrites. Calling from `on_drop` is legal but the
    /// bundle is discarded on plain drops — `drop_component` has no
    /// successor to hand it to (ADR-0016 §5).
    pub fn save_state(&mut self, version: u32, bytes: &[u8]) {
        let status =
            unsafe { raw::save_state(version, bytes.as_ptr().addr() as u32, bytes.len() as u32) };
        if status != 0 {
            panic!("aether-component: save_state failed (status {status})");
        }
    }

    /// Persist a typed kind value across `replace_component`
    /// (ADR-0040). The bundle is framed as `[0..8)` little-endian
    /// `K::ID` followed by the postcard encoding of `value`; the
    /// replacement instance recovers `K` via `PriorState::as_kind`.
    ///
    /// `K::ID` is the ADR-0030 schema hash — changing the shape of
    /// `K` changes the id, which is what makes `as_kind::<K>`
    /// automatically reject stale bytes after a schema evolution.
    /// `version` is passed through to the substrate unchanged;
    /// components typically leave it `0` since `K::ID` already
    /// identifies the schema, but a non-zero value is legal for
    /// components that want to stack a migration counter on top of
    /// kind identity.
    ///
    /// Use the raw `save_state` when persisting bytes that aren't a
    /// kind (external checkpoints, opaque buffers) or when driving an
    /// explicit migration path that inspects the leading id itself.
    pub fn save_state_kind<K>(&mut self, version: u32, value: &K)
    where
        K: Kind + Schema + serde::Serialize,
    {
        let mut out = alloc::vec::Vec::from(K::ID.to_le_bytes());
        let payload = postcard::to_allocvec(value).expect("postcard encode to Vec is infallible");
        out.extend_from_slice(&payload);
        self.save_state(version, &out);
    }
}

/// Opaque view of a prior state bundle handed to `on_rehydrate` by
/// the substrate. Populated when the predecessor called
/// `DropCtx::save_state` during its own `on_replace`; empty otherwise
/// (and in that case `on_rehydrate` is not called at all — ADR-0016
/// §3).
///
/// The lifetime `'a` ties `bytes()` back to the call; holding a
/// reference past return is a compile error.
pub struct PriorState<'a> {
    version: u32,
    ptr: usize,
    len: usize,
    _borrow: PhantomData<&'a [u8]>,
}

impl<'a> PriorState<'a> {
    /// Not part of the public API; called only by `export!`. The FFI
    /// delivers the buffer as wasm32 `(u32, u32)`; this widens.
    #[doc(hidden)]
    pub unsafe fn __from_raw(version: u32, ptr: u32, len: u32) -> Self {
        PriorState {
            version,
            ptr: ptr as usize,
            len: len as usize,
            _borrow: PhantomData,
        }
    }

    /// Component-defined schema version. The substrate does not
    /// interpret it — see ADR-0016.
    pub fn schema_version(&self) -> u32 {
        self.version
    }

    /// Bytes the previous instance saved via `DropCtx::save_state`.
    pub fn bytes(&self) -> &'a [u8] {
        if self.len == 0 {
            &[]
        } else {
            unsafe { core::slice::from_raw_parts(self.ptr as *const u8, self.len) }
        }
    }

    /// Decode the prior-state bundle as kind `K` (ADR-0040). Returns
    /// `Some(K)` when the leading 8 bytes match `K::ID` (little-
    /// endian) and the trailing bytes decode cleanly via postcard;
    /// `None` on id mismatch, short buffer (fewer than 8 bytes), or
    /// decode failure.
    ///
    /// Id mismatch is how schema evolution manifests: changing the
    /// shape of `K` changes `K::ID`, so a replacement instance
    /// compiled against the new schema sees `None` from the old
    /// instance's save and boots fresh. Components that want to
    /// migrate across a schema change can reach for `bytes()` +
    /// `schema_version()` directly, or try `as_kind::<OldShape>()`
    /// first and fall back if it returns `None`.
    pub fn as_kind<K>(&self) -> Option<K>
    where
        K: Kind + Schema + serde::de::DeserializeOwned,
    {
        let bytes = self.bytes();
        if bytes.len() < 8 {
            return None;
        }
        let (id_bytes, payload) = bytes.split_at(8);
        let mut id_arr = [0u8; 8];
        id_arr.copy_from_slice(id_bytes);
        let id = u64::from_le_bytes(id_arr);
        if id != K::ID {
            return None;
        }
        postcard::from_bytes(payload).ok()
    }
}

/// Inbound mail, as received by `Component::receive`. Wraps the raw
/// `(kind, ptr, count, sender)` FFI parameters with typed decode helpers.
///
/// The lifetime `'a` ties the returned references back to the receive
/// call; holding a decoded `&K` past the return of `receive` is a
/// compile error. The underlying bytes live in the component's own
/// linear memory (the substrate placed them there before the FFI
/// call), so zero-copy is possible when alignment permits.
pub struct Mail<'a> {
    kind: u64,
    // Stored as `usize` so `Mail::decode` can reconstruct a full host
    // pointer for tests, while the FFI path (`__from_raw`) widens the
    // incoming `u32` address. On wasm32 `usize == u32` so this is a
    // no-op; on 64-bit hosts it lets us unit-test with real pointers.
    ptr: usize,
    count: u32,
    sender: u32,
    _borrow: PhantomData<&'a [u8]>,
}

impl<'a> Mail<'a> {
    /// Not part of the public API; called only by `export!`. The FFI
    /// delivers `ptr` as a wasm32 offset (`u32`); this widens it.
    #[doc(hidden)]
    pub unsafe fn __from_raw(kind: u64, ptr: u32, count: u32, sender: u32) -> Self {
        Mail {
            kind,
            ptr: ptr as usize,
            count,
            sender,
            _borrow: PhantomData,
        }
    }

    /// Not part of the public API; unit tests that fabricate `Mail`
    /// from a host pointer go through here so 64-bit addresses survive.
    #[doc(hidden)]
    #[cfg(test)]
    unsafe fn __from_ptr_test(kind: u64, ptr: usize, count: u32, sender: u32) -> Self {
        Mail {
            kind,
            ptr,
            count,
            sender,
            _borrow: PhantomData,
        }
    }

    /// Raw kind id the substrate routed this mail under. Match against
    /// a cached `KindId<K>` via `kind_id.matches(mail.kind())`, or use
    /// `decode::<K>(kind_id)` and let it be the discriminator.
    pub fn kind(&self) -> u64 {
        self.kind
    }

    /// Number of items carried on the mail frame — 1 for a single
    /// payload send, N for a batch send of N elements.
    pub fn count(&self) -> u32 {
        self.count
    }

    /// Reply handle for the session that originated this mail. `None`
    /// for component-to-component mail and broadcast-origin mail;
    /// `Some(ReplyTo)` when the inbound came from a Claude session and
    /// can be answered via `Ctx::reply`.
    pub fn reply_to(&self) -> Option<ReplyTo> {
        if self.sender == NO_REPLY_HANDLE {
            None
        } else {
            Some(ReplyTo { raw: self.sender })
        }
    }

    /// Decode as a single owned `K`. Returns `None` if the kind does
    /// not match or if `count` is not 1. Copies rather than borrows so
    /// alignment of the underlying bytes doesn't matter.
    pub fn decode<K: Kind + bytemuck::AnyBitPattern>(&self, kind_id: KindId<K>) -> Option<K> {
        if !kind_id.matches(self.kind) || self.count != 1 {
            return None;
        }
        let byte_len = core::mem::size_of::<K>();
        let bytes = unsafe { core::slice::from_raw_parts(self.ptr as *const u8, byte_len) };
        Some(bytemuck::pod_read_unaligned(bytes))
    }

    /// Decode as a zero-copy slice of `K`. Returns `None` if the kind
    /// does not match or the bytes are not aligned for `K`. The
    /// returned slice borrows from component linear memory for the
    /// lifetime of this `Mail`.
    pub fn decode_slice<K: Kind + bytemuck::AnyBitPattern>(
        &self,
        kind_id: KindId<K>,
    ) -> Option<&'a [K]> {
        if !kind_id.matches(self.kind) {
            return None;
        }
        let byte_len = core::mem::size_of::<K>() * self.count as usize;
        let bytes = unsafe { core::slice::from_raw_parts(self.ptr as *const u8, byte_len) };
        bytemuck::try_cast_slice(bytes).ok()
    }

    /// True if the inbound mail's kind id matches `<K as Kind>::ID`
    /// (ADR-0030 compile-time hash). Zero-cost — just a `u64` compare
    /// against a const. Useful as the discriminator before deciding
    /// how to handle a kind, or as a signal check when `K` is a
    /// zero-sized input marker like `Tick` / `MouseButton`.
    pub fn is<K: Kind>(&self) -> bool {
        self.kind == K::ID
    }

    /// Type-driven sibling of `decode`: takes `K` as a type parameter
    /// and uses `<K as Kind>::ID` directly (ADR-0030 compile-time hash),
    /// so no `KindId<K>` thread-through is needed. Returns `None` if
    /// the inbound kind doesn't match `K::ID` or if `count != 1`.
    /// Copies rather than borrows so alignment of the underlying bytes
    /// doesn't matter — same semantics as `decode`.
    pub fn decode_typed<K: Kind + bytemuck::AnyBitPattern>(&self) -> Option<K> {
        if self.kind != K::ID || self.count != 1 {
            return None;
        }
        let byte_len = core::mem::size_of::<K>();
        let bytes = unsafe { core::slice::from_raw_parts(self.ptr as *const u8, byte_len) };
        Some(bytemuck::pod_read_unaligned(bytes))
    }

    /// Type-driven sibling of `decode_slice`. Borrowed, alignment
    /// required (returns `None` if misaligned).
    pub fn decode_slice_typed<K: Kind + bytemuck::AnyBitPattern>(&self) -> Option<&'a [K]> {
        if self.kind != K::ID {
            return None;
        }
        let byte_len = core::mem::size_of::<K>() * self.count as usize;
        let bytes = unsafe { core::slice::from_raw_parts(self.ptr as *const u8, byte_len) };
        bytemuck::try_cast_slice(bytes).ok()
    }
}

/// Macro-use backing store for the one `Component` instance per
/// guest. WASM components are single-threaded per instance (ADR-0010
/// §5 — the substrate holds a read lock across `deliver`), so an
/// `UnsafeCell` with a blanket `Sync` impl is sound *provided the
/// macro is the only caller*. The `export!` macro orchestrates
/// `set` / `get_mut` from within `init` / `receive` shims that the
/// substrate serializes.
pub struct Slot<T> {
    inner: core::cell::UnsafeCell<Option<T>>,
}

impl<T> Slot<T> {
    /// Build an empty slot. `const` so it can live in a `static`.
    pub const fn new() -> Self {
        Slot {
            inner: core::cell::UnsafeCell::new(None),
        }
    }

    /// # Safety
    /// Caller must guarantee no aliasing access. Intended to be called
    /// exactly once, from within the `init` shim, before any other
    /// access.
    pub unsafe fn set(&self, value: T) {
        unsafe {
            *self.inner.get() = Some(value);
        }
    }

    /// # Safety
    /// Caller must guarantee no aliasing access. Intended to be called
    /// from within the `receive` shim, after `init` has completed.
    // Returning `&mut T` from `&self` is the load-bearing pattern
    // here — the `UnsafeCell` makes this sound under the substrate's
    // serialized-dispatch guarantee. Clippy's `mut_from_ref` lint
    // catches this as a footgun in general; we're the exception the
    // lint is designed around.
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn get_mut(&self) -> Option<&mut T> {
        unsafe { (*self.inner.get()).as_mut() }
    }
}

impl<T> Default for Slot<T> {
    fn default() -> Self {
        Slot::new()
    }
}

// Single-threaded WASM + serialized FFI entry points mean the
// `UnsafeCell` is only ever touched from one thread at a time. The
// `Sync` impl unlocks `static SLOT: Slot<MyComponent>` without
// needing `std::sync` types the `no_std` surface can't provide.
unsafe impl<T> Sync for Slot<T> {}

/// Bind a `Component` implementor to the guest's `#[no_mangle]`
/// `init` / `receive` exports. Expands to:
///
/// - A `static` `Slot<T>` that backs the component instance.
/// - `extern "C" fn init() -> u32` — builds an `InitCtx`, calls
///   `T::init`, stashes the result in the slot.
/// - `extern "C" fn receive(kind, ptr, count) -> u32` — builds
///   `Ctx` and `Mail`, calls `T::receive` on the stashed instance.
///
/// Only one component per guest crate. A second `export!` call in
/// the same crate is a duplicate-symbol compile error on the shared
/// `init` / `receive` names — ADR-0014 §4 parks multi-component
/// crates as out of scope.
///
/// ```ignore
/// pub struct Hello { /* fields */ }
/// impl aether_component::Component for Hello { /* init + receive */ }
/// aether_component::export!(Hello);
/// ```
#[macro_export]
macro_rules! export {
    ($component:ty) => {
        static __AETHER_COMPONENT: $crate::Slot<$component> = $crate::Slot::new();

        /// # Safety
        /// Called exactly once by the substrate before any `receive`.
        /// Receives the component's own mailbox id (ADR-0030 Phase 2)
        /// so `#[handlers]`'s synthesized `init` prologue can self-
        /// address `subscribe_input` for every `K::IS_INPUT` handler
        /// kind.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn init(mailbox_id: u64) -> u32 {
            let mut ctx = $crate::InitCtx::__new(mailbox_id);
            let instance = <$component as $crate::Component>::init(&mut ctx);
            unsafe {
                __AETHER_COMPONENT.set(instance);
            }
            0
        }

        /// # Safety
        /// Called by the substrate with `(kind, ptr, count, sender)`
        /// matching the FFI contract in
        /// `aether-substrate/src/host_fns.rs`. `sender` is the per-
        /// instance reply-to handle (ADR-0013) or `NO_REPLY_HANDLE` for
        /// mail with no meaningful reply target. Exported under the
        /// `_p32` suffix per ADR-0024 Phase 1. Returns the `u32` the
        /// `#[handlers]`-synthesized `__aether_dispatch` produces —
        /// `DISPATCH_HANDLED` on match, `DISPATCH_UNKNOWN_KIND` on a
        /// strict-receiver miss (ADR-0033 §Strict receivers).
        #[unsafe(export_name = "receive_p32")]
        pub unsafe extern "C" fn receive(kind: u64, ptr: u32, count: u32, sender: u32) -> u32 {
            let Some(instance) = (unsafe { __AETHER_COMPONENT.get_mut() }) else {
                return 1;
            };
            let mut ctx = $crate::Ctx::__new();
            let mail = unsafe { $crate::Mail::__from_raw(kind, ptr, count, sender) };
            instance.__aether_dispatch(&mut ctx, mail)
        }

        /// # Safety
        /// Called by the substrate exactly once, on the old instance,
        /// immediately before a `replace_component` swap.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn on_replace() -> u32 {
            let Some(instance) = (unsafe { __AETHER_COMPONENT.get_mut() }) else {
                return 1;
            };
            let mut ctx = $crate::DropCtx::__new();
            <$component as $crate::Component>::on_replace(instance, &mut ctx);
            0
        }

        /// # Safety
        /// Called by the substrate exactly once on the instance being
        /// dropped, immediately before linear memory is torn down.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn on_drop() -> u32 {
            let Some(instance) = (unsafe { __AETHER_COMPONENT.get_mut() }) else {
                return 1;
            };
            let mut ctx = $crate::DropCtx::__new();
            <$component as $crate::Component>::on_drop(instance, &mut ctx);
            0
        }

        /// # Safety
        /// Called by the substrate after `init` on a freshly
        /// instantiated replacement, with `(version, ptr, len)`
        /// describing the prior-state bundle the old instance
        /// produced via `DropCtx::save_state`. Exported under the
        /// `_p32` suffix per ADR-0024 Phase 1.
        #[unsafe(export_name = "on_rehydrate_p32")]
        pub unsafe extern "C" fn on_rehydrate(version: u32, ptr: u32, len: u32) -> u32 {
            let Some(instance) = (unsafe { __AETHER_COMPONENT.get_mut() }) else {
                return 1;
            };
            let mut ctx = $crate::Ctx::__new();
            let prior = unsafe { $crate::PriorState::__from_raw(version, ptr, len) };
            <$component as $crate::Component>::on_rehydrate(instance, &mut ctx, prior);
            0
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeKind;
    impl Kind for FakeKind {
        const NAME: &'static str = "test.fake";
        const ID: u64 = aether_mail::mailbox_id_from_name(Self::NAME);
    }

    #[test]
    fn kind_id_equality_and_matches() {
        let a: KindId<FakeKind> = KindId {
            raw: 7,
            _k: PhantomData,
        };
        let b: KindId<FakeKind> = KindId {
            raw: 7,
            _k: PhantomData,
        };
        let c: KindId<FakeKind> = KindId {
            raw: 8,
            _k: PhantomData,
        };
        assert!(a == b);
        assert!(a != c);
        assert!(a.matches(7));
        assert!(!a.matches(8));
        assert_eq!(a.raw(), 7);
    }

    #[test]
    fn sink_accessors() {
        let s: Sink<FakeKind> = Sink {
            mailbox: 3u64,
            kind: 11,
            _k: PhantomData,
        };
        assert_eq!(s.mailbox(), 3u64);
        assert_eq!(s.kind(), 11);
    }

    #[test]
    fn slot_set_then_get_mut_returns_value() {
        let slot: Slot<u32> = Slot::new();
        unsafe {
            slot.set(42);
        }
        let got = unsafe { slot.get_mut() };
        assert_eq!(got.copied(), Some(42));
    }

    #[test]
    fn slot_get_mut_before_set_is_none() {
        let slot: Slot<u32> = Slot::new();
        let got = unsafe { slot.get_mut() };
        assert!(got.is_none());
    }

    #[repr(C)]
    #[derive(Copy, Clone, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
    struct FakePod {
        a: u32,
        b: u32,
    }
    impl Kind for FakePod {
        const NAME: &'static str = "test.fake_pod";
        const ID: u64 = aether_mail::mailbox_id_from_name(Self::NAME);
    }

    #[test]
    fn mail_decode_single_roundtrip() {
        // Build a `Mail` by hand that points at a local `FakePod`.
        // This is the only place in the crate that fabricates a Mail
        // outside the FFI path; the unsafe is load-bearing because
        // decode treats `ptr` as a guest-memory address and the test
        // has to arrange for that address to point at valid bytes.
        let value = FakePod { a: 5, b: 9 };
        let ptr_raw = (&value as *const FakePod).addr();
        let mail = unsafe { Mail::__from_ptr_test(7, ptr_raw, 1, NO_REPLY_HANDLE) };
        let kind: KindId<FakePod> = KindId {
            raw: 7,
            _k: PhantomData,
        };
        let out = mail.decode(kind).unwrap();
        assert_eq!(out, value);
    }

    #[test]
    fn mail_decode_wrong_kind_returns_none() {
        let value = FakePod { a: 5, b: 9 };
        let ptr_raw = (&value as *const FakePod).addr();
        let mail = unsafe { Mail::__from_ptr_test(7, ptr_raw, 1, NO_REPLY_HANDLE) };
        let wrong: KindId<FakePod> = KindId {
            raw: 8,
            _k: PhantomData,
        };
        assert!(mail.decode(wrong).is_none());
    }

    #[test]
    fn mail_decode_wrong_count_returns_none() {
        let values = [FakePod { a: 5, b: 9 }, FakePod { a: 1, b: 1 }];
        let ptr_raw = values.as_ptr().addr();
        let mail = unsafe { Mail::__from_ptr_test(7, ptr_raw, 2, NO_REPLY_HANDLE) };
        let kind: KindId<FakePod> = KindId {
            raw: 7,
            _k: PhantomData,
        };
        // `decode` requires count == 1; use `decode_slice` for batches.
        assert!(mail.decode(kind).is_none());
    }

    #[test]
    fn mail_decode_slice_roundtrip() {
        let values = [FakePod { a: 1, b: 2 }, FakePod { a: 3, b: 4 }];
        let ptr_raw = values.as_ptr().addr();
        let mail = unsafe { Mail::__from_ptr_test(7, ptr_raw, 2, NO_REPLY_HANDLE) };
        let kind: KindId<FakePod> = KindId {
            raw: 7,
            _k: PhantomData,
        };
        let out = mail.decode_slice(kind).unwrap();
        assert_eq!(out, &values);
    }

    #[test]
    fn mail_sender_none_for_sentinel_handle() {
        let mail = unsafe { Mail::__from_ptr_test(0, 0, 0, NO_REPLY_HANDLE) };
        assert!(mail.reply_to().is_none());
    }

    #[test]
    fn mail_sender_some_for_real_handle() {
        let mail = unsafe { Mail::__from_ptr_test(0, 0, 0, 42) };
        let s = mail.reply_to().expect("non-sentinel handle yields Some");
        assert_eq!(s.raw(), 42);
    }

    #[test]
    fn prior_state_empty_bytes_does_not_deref() {
        // With len=0, `bytes()` must not materialise a pointer — a
        // raw 0 ptr with len>0 would be UB if anyone called
        // `from_raw_parts` on it. The `if self.len == 0` branch in
        // `bytes()` is what guarantees this.
        let prior = unsafe { PriorState::__from_raw(7, 0, 0) };
        assert_eq!(prior.schema_version(), 7);
        assert_eq!(prior.bytes(), &[] as &[u8]);
    }

    #[test]
    fn prior_state_nonempty_bytes_roundtrip() {
        let buf: [u8; 4] = [1, 2, 3, 4];
        let prior = PriorState {
            version: 3,
            ptr: buf.as_ptr().addr(),
            len: buf.len(),
            _borrow: PhantomData,
        };
        assert_eq!(prior.schema_version(), 3);
        assert_eq!(prior.bytes(), &buf);
    }

    /// `DropCtx::__new()` must be callable without special setup so
    /// the `export!` macro can build one inside a `#[no_mangle]` shim.
    /// The accessor covered here just verifies the constructor type
    /// is well-formed; send/send_many require a real FFI and are not
    /// unit-testable on host.
    #[test]
    fn drop_ctx_constructor_well_formed() {
        let _ctx: DropCtx<'_> = DropCtx::__new();
    }

    // ADR-0033 phase 3: type-driven Mail tests now use `K::ID`
    // directly (no `KindTable`). `is::<K>` is a `u64` compare against
    // the const; `decode_typed::<K>` reads `K::ID` and the payload
    // size without any per-component cache.

    // Host-test fabrication lets us pick the `kind` id at will. These
    // types' `Kind::ID` is the name-hash under ADR-0030 — stable but
    // opaque. We assert against `Kind::ID` directly rather than
    // hard-coding the integer.

    #[test]
    fn mail_is_typed_matches_kind_id() {
        let mail = unsafe { Mail::__from_ptr_test(FakeKind::ID, 0, 0, NO_REPLY_HANDLE) };
        assert!(mail.is::<FakeKind>());
        assert!(!mail.is::<FakePod>());
    }

    #[test]
    fn mail_decode_typed_roundtrip() {
        let value = FakePod { a: 5, b: 9 };
        let ptr_raw = (&value as *const FakePod).addr();
        let mail = unsafe { Mail::__from_ptr_test(FakePod::ID, ptr_raw, 1, NO_REPLY_HANDLE) };
        let out = mail.decode_typed::<FakePod>().unwrap();
        assert_eq!(out, value);
    }

    #[test]
    fn mail_decode_typed_wrong_kind_returns_none() {
        let value = FakePod { a: 5, b: 9 };
        let ptr_raw = (&value as *const FakePod).addr();
        // Kind id deliberately mismatched (FakeKind instead of FakePod).
        let mail = unsafe { Mail::__from_ptr_test(FakeKind::ID, ptr_raw, 1, NO_REPLY_HANDLE) };
        assert!(mail.decode_typed::<FakePod>().is_none());
    }

    #[test]
    fn mail_decode_typed_wrong_count_returns_none() {
        let values = [FakePod { a: 5, b: 9 }, FakePod { a: 1, b: 1 }];
        let ptr_raw = values.as_ptr().addr();
        let mail = unsafe { Mail::__from_ptr_test(FakePod::ID, ptr_raw, 2, NO_REPLY_HANDLE) };
        assert!(mail.decode_typed::<FakePod>().is_none());
    }

    #[test]
    fn mail_decode_slice_typed_roundtrip() {
        let values = [FakePod { a: 1, b: 2 }, FakePod { a: 3, b: 4 }];
        let ptr_raw = values.as_ptr().addr();
        let mail = unsafe { Mail::__from_ptr_test(FakePod::ID, ptr_raw, 2, NO_REPLY_HANDLE) };
        let out = mail.decode_slice_typed::<FakePod>().unwrap();
        assert_eq!(out, &values);
    }

    // ADR-0040 typed-state framing. `DropCtx::save_state_kind` can't be
    // exercised end-to-end on host (the underlying `raw::save_state`
    // panics off-wasm, ADR-0015 §stub policy), so the tests below pair
    // a hand-built bundle matching the documented framing
    // (`[0..8) = K::ID LE`, `[8..) = postcard(value)`) against
    // `PriorState::as_kind` — the one we *can* unit-test on host. A
    // mismatch between framing and decode surfaces here before either
    // diverges from the ADR's wire shape.
    use alloc::vec::Vec;
    use serde::{Deserialize, Serialize};

    #[derive(
        aether_mail::Kind, aether_mail::Schema, Serialize, Deserialize, Debug, Clone, PartialEq,
    )]
    #[kind(name = "test.state.struct")]
    struct StateStruct {
        tag: u32,
        label: alloc::string::String,
        items: Vec<u32>,
    }

    #[derive(
        aether_mail::Kind, aether_mail::Schema, Serialize, Deserialize, Debug, Clone, PartialEq,
    )]
    #[kind(name = "test.state.other")]
    struct OtherState {
        flag: bool,
    }

    fn frame_bundle<K: Kind + Schema + Serialize>(value: &K) -> Vec<u8> {
        let mut out = Vec::from(K::ID.to_le_bytes());
        let payload = postcard::to_allocvec(value).unwrap();
        out.extend_from_slice(&payload);
        out
    }

    fn prior_from(buf: &[u8], version: u32) -> PriorState<'_> {
        PriorState {
            version,
            ptr: buf.as_ptr().addr(),
            len: buf.len(),
            _borrow: PhantomData,
        }
    }

    #[test]
    fn as_kind_roundtrip() {
        let value = StateStruct {
            tag: 11,
            label: alloc::string::String::from("phase-2"),
            items: alloc::vec![1, 2, 3],
        };
        let buf = frame_bundle(&value);
        let prior = prior_from(&buf, 0);
        let decoded = prior.as_kind::<StateStruct>().unwrap();
        assert_eq!(decoded, value);
    }

    #[test]
    fn as_kind_id_mismatch_returns_none() {
        // Frame under one kind, decode as a different one — the
        // leading `K::ID` compare rejects before postcard runs.
        let value = OtherState { flag: true };
        let buf = frame_bundle(&value);
        let prior = prior_from(&buf, 0);
        assert!(prior.as_kind::<StateStruct>().is_none());
    }

    #[test]
    fn as_kind_short_buffer_returns_none() {
        // Buffer shorter than the 8-byte leading id — not a kind-
        // typed save (or corrupt). Must not panic.
        let buf: [u8; 3] = [1, 2, 3];
        let prior = prior_from(&buf, 0);
        assert!(prior.as_kind::<StateStruct>().is_none());
    }

    #[test]
    fn as_kind_empty_buffer_returns_none() {
        // `on_rehydrate` only fires when the predecessor saved
        // something, but a hypothetical zero-length buffer must
        // still fall through cleanly.
        let prior = unsafe { PriorState::__from_raw(0, 0, 0) };
        assert!(prior.as_kind::<StateStruct>().is_none());
    }

    #[test]
    fn as_kind_correct_id_garbage_payload_returns_none() {
        // Leading id matches but the postcard tail is truncated.
        // Decode error must surface as None, not a panic.
        let mut buf = Vec::from(StateStruct::ID.to_le_bytes());
        buf.push(0xff);
        let prior = prior_from(&buf, 0);
        assert!(prior.as_kind::<StateStruct>().is_none());
    }

    #[test]
    fn as_kind_preserves_raw_access_for_migration() {
        // ADR-0040 keeps the raw bytes + version reachable so a
        // component that sees `as_kind::<New>() = None` can pivot to
        // an explicit migration path.
        let value = OtherState { flag: false };
        let buf = frame_bundle(&value);
        let prior = prior_from(&buf, 7);
        assert!(prior.as_kind::<StateStruct>().is_none());
        assert_eq!(prior.schema_version(), 7);
        assert_eq!(prior.bytes(), buf.as_slice());
    }
}
