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
//!   - ADR-0013: Reply-to-sender. `Mail::sender()` returns `Some(Sender)`
//!     for mail that came from a Claude session; `Ctx::reply` takes a
//!     `Sender` and a typed `KindId<K>` to answer the originating
//!     session. The 4-param `receive(kind, ptr, count, sender)` ABI is
//!     absorbed by the `export!` macro so component authors don't see it.
//!   - ADR-0027: Component-declared kind dependencies via `type Kinds`
//!     associated typelist. The SDK walks the list at init, resolves
//!     each `K::NAME`, and stashes `(TypeId, raw_id)` in a per-component
//!     `KindTable`. Receive-time helpers `Mail::is::<K>()` and
//!     `Mail::decode_typed::<K>()` consult the table by `TypeId`, so
//!     the user never threads a `KindId<K>` field through `Self` for
//!     kinds they only need at the dispatch site. Tuple syntax up to
//!     `MAX_KINDS = 32`; `Cons<H, T>` / `Nil` is the unbounded escape
//!     hatch. Coexists with the ADR-0014 `KindId<K>` field pattern;
//!     `Sink<K>` is unchanged.

#![no_std]

extern crate alloc;

use core::any::TypeId;
use core::marker::PhantomData;

use aether_mail::{Kind, mailbox_id_from_name};

pub mod kinds;
pub mod raw;

pub use kinds::{Cons, KindList, KindTable, Nil};

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
    /// The kinds this component handles at receive time (ADR-0027).
    /// The SDK walks this typelist at init *before* user `init` runs
    /// and resolves each `K::NAME` into a per-component `KindTable`,
    /// which `Mail::is::<K>()` and `Mail::decode_typed::<K>()`
    /// consult. Tuple form (`type Kinds = (Tick, Key, MouseMove);`)
    /// covers up to 32 kinds; `Cons<H, T>` / `Nil` is the escape
    /// hatch beyond that.
    ///
    /// Components that don't use the type-driven helpers — including
    /// the older ADR-0014 shape with explicit `KindId<K>` fields —
    /// declare `type Kinds = ();`. `Sink<K>` resolution stays in
    /// user `init` and does not appear in `Kinds`.
    type Kinds: KindList;

    /// Runs once. Resolve kinds and sinks via `ctx` and return the
    /// initial component state. A failed `resolve` panics — see
    /// ADR-0012 §2 ("loud at init").
    fn init(ctx: &mut InitCtx<'_>) -> Self;

    /// Runs on every inbound mail. Component decides dispatch by
    /// matching `mail.kind()` against cached `KindId<K>` values, or by
    /// calling `mail.decode::<K>(kind_id)` directly — the `Option`
    /// return doubles as the match.
    fn receive(&mut self, ctx: &mut Ctx<'_>, mail: Mail<'_>);

    /// Called once on the old instance, immediately before a
    /// `replace_component` swap (ADR-0015 §3). Default is no-op;
    /// override to serialize state (via `DropCtx::save_state`) that
    /// the new instance can consume through `on_rehydrate`, or to
    /// emit farewell mail. ADR-0016 §2 governs the save-bundle shape.
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
    /// produced a state bundle via `DropCtx::save_state` (ADR-0016 §3).
    /// Default ignores the prior state; components that persist
    /// across replace override to rehydrate and typically branch on
    /// `prior.schema_version()`.
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
        use aether_kinds::{InputStream, Key, MouseButton, MouseMove, SubscribeInput, Tick};
        let tid = TypeId::of::<K>();
        let stream = if tid == TypeId::of::<Tick>() {
            InputStream::Tick
        } else if tid == TypeId::of::<Key>() {
            InputStream::Key
        } else if tid == TypeId::of::<MouseMove>() {
            InputStream::MouseMove
        } else if tid == TypeId::of::<MouseButton>() {
            InputStream::MouseButton
        } else {
            return;
        };
        let payload = SubscribeInput {
            stream,
            mailbox: self.mailbox,
        };
        let bytes = postcard::to_allocvec(&payload).expect("SubscribeInput encode infallible");
        let recipient = mailbox_id_from_name("aether.control");
        unsafe {
            raw::send_mail(
                recipient,
                <SubscribeInput as Kind>::ID,
                bytes.as_ptr().addr() as u32,
                bytes.len() as u32,
                1,
            );
        }
    }
}

/// Per-receive capability handle. Exposes send primitives only.
/// Resolution is intentionally absent — runtime resolution after init
/// is not a supported shape.
pub struct Ctx<'a> {
    _borrow: PhantomData<&'a ()>,
}

impl Ctx<'_> {
    /// Not part of the public API; called only by `export!`.
    #[doc(hidden)]
    pub fn __new() -> Self {
        Ctx {
            _borrow: PhantomData,
        }
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

    /// Reply to the Claude session that originated the inbound mail
    /// (ADR-0013). `sender` came from `mail.sender()` on the current
    /// receive — pass it back as the routing handle. The kind is
    /// supplied as a typed `KindId<K>` so the same compile-time
    /// matching the rest of the SDK uses applies here too.
    ///
    /// Status of the underlying host call is dropped; reply is
    /// fire-and-forget on the guest side. If the session is gone the
    /// hub silently discards the frame.
    pub fn reply<K: Kind + bytemuck::NoUninit>(
        &self,
        sender: Sender,
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

/// Sentinel the substrate passes as the `sender` parameter on the
/// `receive` shim when there is no meaningful reply target — for
/// component-originated mail (no Claude session involved) and for
/// broadcast-origin mail. `Mail::sender()` returns `None` in this
/// case; `Sender` is only constructable via the `Mail` accessor.
pub const SENDER_NONE: u32 = u32::MAX;

/// Opaque per-instance handle identifying the originating Claude
/// session of an inbound mail. Hand it back to `Ctx::reply` to send
/// a session-targeted response.
///
/// `Copy` because the handle is a `u32` underneath; cloning is free.
/// Cloning is also fine for stashing across receives — the substrate
/// guarantees the handle stays valid until the originating session
/// disconnects (in which case the eventual reply silently drops on
/// the hub side; ADR-0013 §1's session-gone status is not yet
/// detected synchronously).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Sender {
    raw: u32,
}

impl Sender {
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
    // ADR-0027: the per-component `KindTable` the `export!` macro
    // installed in its init shim. `Mail::is::<K>` / `decode_typed::<K>`
    // consult it. `None` only on the host-test fabricated path; the
    // FFI path always carries a table.
    table: Option<&'a KindTable>,
    _borrow: PhantomData<&'a [u8]>,
}

impl<'a> Mail<'a> {
    /// Not part of the public API; called only by `export!`. The FFI
    /// delivers `ptr` as a wasm32 offset (`u32`); this widens it.
    #[doc(hidden)]
    pub unsafe fn __from_raw(
        kind: u64,
        ptr: u32,
        count: u32,
        sender: u32,
        table: &'a KindTable,
    ) -> Self {
        Mail {
            kind,
            ptr: ptr as usize,
            count,
            sender,
            table: Some(table),
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
            table: None,
            _borrow: PhantomData,
        }
    }

    /// Not part of the public API; unit tests that exercise the
    /// type-driven helpers wire a real `KindTable` through here.
    #[doc(hidden)]
    #[cfg(test)]
    unsafe fn __from_ptr_test_with_table(
        kind: u64,
        ptr: usize,
        count: u32,
        sender: u32,
        table: &'a KindTable,
    ) -> Self {
        Mail {
            kind,
            ptr,
            count,
            sender,
            table: Some(table),
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
    /// `Some(Sender)` when the inbound came from a Claude session and
    /// can be answered via `Ctx::reply`.
    pub fn sender(&self) -> Option<Sender> {
        if self.sender == SENDER_NONE {
            None
        } else {
            Some(Sender { raw: self.sender })
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

    /// True if the inbound mail is a `K`, looked up via the
    /// per-component `KindTable` populated by `Component::Kinds`
    /// (ADR-0027 §3). For signal-shaped kinds (no payload to read) or
    /// as the discriminator before a `decode_typed` of a different K.
    /// Returns `false` if `K` was not declared in `Component::Kinds`.
    pub fn is<K: Kind + 'static>(&self) -> bool {
        let Some(table) = self.table else {
            return false;
        };
        table.lookup(TypeId::of::<K>()) == Some(self.kind)
    }

    /// Type-driven sibling of `decode`: looks `K` up in the
    /// per-component `KindTable` (ADR-0027) instead of taking an
    /// explicit `KindId<K>`. Returns `None` if `K` was not declared in
    /// `Component::Kinds`, if the inbound kind doesn't match, or if
    /// `count != 1`. Copies rather than borrows so alignment of the
    /// underlying bytes doesn't matter — same semantics as `decode`.
    ///
    /// (Distinct name from `decode` because Rust does not allow two
    /// inherent methods with the same name; the ADR-0027 sketch
    /// elided that constraint. `_typed` signals "type-driven via the
    /// kind table" rather than "via the explicit `KindId<K>` arg".)
    pub fn decode_typed<K: Kind + bytemuck::AnyBitPattern + 'static>(&self) -> Option<K> {
        let table = self.table?;
        let raw = table.lookup(TypeId::of::<K>())?;
        if raw != self.kind || self.count != 1 {
            return None;
        }
        let byte_len = core::mem::size_of::<K>();
        let bytes = unsafe { core::slice::from_raw_parts(self.ptr as *const u8, byte_len) };
        Some(bytemuck::pod_read_unaligned(bytes))
    }

    /// Type-driven sibling of `decode_slice`. Borrowed, alignment
    /// required (returns `None` if misaligned). Same fallback rules as
    /// `decode_typed` for an undeclared `K`.
    pub fn decode_slice_typed<K: Kind + bytemuck::AnyBitPattern + 'static>(
        &self,
    ) -> Option<&'a [K]> {
        let table = self.table?;
        let raw = table.lookup(TypeId::of::<K>())?;
        if raw != self.kind {
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
        // ADR-0027: per-component kind cache. Populated by
        // `<C::Kinds as KindList>::resolve_all` before user `init`;
        // read-only afterwards via `Mail::is` / `Mail::decode_typed`.
        static __AETHER_KINDS: $crate::KindTable = $crate::KindTable::new();

        /// # Safety
        /// Called exactly once by the substrate before any `receive`.
        /// Receives the component's own mailbox id (ADR-0030 Phase 2)
        /// so the SDK's init walker can auto-subscribe input kinds
        /// without a host-fn round trip.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn init(mailbox_id: u64) -> u32 {
            let mut ctx = $crate::InitCtx::__new(mailbox_id);
            // ADR-0027: walk the Kinds typelist and populate
            // __AETHER_KINDS before user init runs. ADR-0030 Phase 2:
            // also mails `aether.control.subscribe_input` for each
            // `K::IS_INPUT` kind through the same walker.
            unsafe {
                <<$component as $crate::Component>::Kinds as $crate::KindList>::resolve_all(
                    &mut ctx,
                    &__AETHER_KINDS,
                );
            }
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
        /// instance reply-to handle (ADR-0013) or `SENDER_NONE` for
        /// mail with no meaningful reply target. Exported under the
        /// `_p32` suffix per ADR-0024 Phase 1.
        #[unsafe(export_name = "receive_p32")]
        pub unsafe extern "C" fn receive(kind: u64, ptr: u32, count: u32, sender: u32) -> u32 {
            let Some(instance) = (unsafe { __AETHER_COMPONENT.get_mut() }) else {
                return 1;
            };
            let mut ctx = $crate::Ctx::__new();
            let mail =
                unsafe { $crate::Mail::__from_raw(kind, ptr, count, sender, &__AETHER_KINDS) };
            <$component as $crate::Component>::receive(instance, &mut ctx, mail);
            0
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
        let mail = unsafe { Mail::__from_ptr_test(7, ptr_raw, 1, SENDER_NONE) };
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
        let mail = unsafe { Mail::__from_ptr_test(7, ptr_raw, 1, SENDER_NONE) };
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
        let mail = unsafe { Mail::__from_ptr_test(7, ptr_raw, 2, SENDER_NONE) };
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
        let mail = unsafe { Mail::__from_ptr_test(7, ptr_raw, 2, SENDER_NONE) };
        let kind: KindId<FakePod> = KindId {
            raw: 7,
            _k: PhantomData,
        };
        let out = mail.decode_slice(kind).unwrap();
        assert_eq!(out, &values);
    }

    #[test]
    fn mail_sender_none_for_sentinel_handle() {
        let mail = unsafe { Mail::__from_ptr_test(0, 0, 0, SENDER_NONE) };
        assert!(mail.sender().is_none());
    }

    #[test]
    fn mail_sender_some_for_real_handle() {
        let mail = unsafe { Mail::__from_ptr_test(0, 0, 0, 42) };
        let s = mail.sender().expect("non-sentinel handle yields Some");
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

    // ADR-0027 typed-Mail tests. The path under test is
    // `Mail::is::<K>` / `Mail::decode_typed::<K>` reading from a
    // `KindTable` populated as if `KindList::resolve_all` had run.

    #[test]
    fn mail_is_typed_matches_declared_kind() {
        let table = KindTable::new();
        unsafe {
            table.insert(TypeId::of::<FakeKind>(), 7);
            table.insert(TypeId::of::<FakePod>(), 11);
        }
        let mail = unsafe { Mail::__from_ptr_test_with_table(7, 0, 0, SENDER_NONE, &table) };
        assert!(mail.is::<FakeKind>());
        assert!(!mail.is::<FakePod>());
    }

    #[test]
    fn mail_is_typed_returns_false_for_undeclared_kind() {
        let table = KindTable::new();
        unsafe {
            table.insert(TypeId::of::<FakeKind>(), 7);
        }
        // FakePod was not inserted; `is::<FakePod>` falls back to false.
        let mail = unsafe { Mail::__from_ptr_test_with_table(7, 0, 0, SENDER_NONE, &table) };
        assert!(!mail.is::<FakePod>());
    }

    #[test]
    fn mail_is_typed_returns_false_when_no_table() {
        // Test-fabricated Mail without a table — i.e. the
        // host-test path. Type-driven helpers degrade to false/None
        // rather than panicking, which keeps unit tests of the
        // ADR-0014 path (decode/decode_slice with explicit KindId)
        // unaffected.
        let mail = unsafe { Mail::__from_ptr_test(0, 0, 0, SENDER_NONE) };
        assert!(!mail.is::<FakeKind>());
    }

    #[test]
    fn mail_decode_typed_roundtrip() {
        let value = FakePod { a: 5, b: 9 };
        let ptr_raw = (&value as *const FakePod).addr();
        let table = KindTable::new();
        unsafe {
            table.insert(TypeId::of::<FakePod>(), 7);
        }
        let mail = unsafe { Mail::__from_ptr_test_with_table(7, ptr_raw, 1, SENDER_NONE, &table) };
        let out = mail.decode_typed::<FakePod>().unwrap();
        assert_eq!(out, value);
    }

    #[test]
    fn mail_decode_typed_wrong_kind_returns_none() {
        let value = FakePod { a: 5, b: 9 };
        let ptr_raw = (&value as *const FakePod).addr();
        let table = KindTable::new();
        unsafe {
            // FakePod inserted under id 7, but the inbound Mail
            // claims kind 8 — type lookup hits but the raw mismatch
            // makes decode_typed return None.
            table.insert(TypeId::of::<FakePod>(), 7);
        }
        let mail = unsafe { Mail::__from_ptr_test_with_table(8, ptr_raw, 1, SENDER_NONE, &table) };
        assert!(mail.decode_typed::<FakePod>().is_none());
    }

    #[test]
    fn mail_decode_typed_undeclared_kind_returns_none() {
        let value = FakePod { a: 5, b: 9 };
        let ptr_raw = (&value as *const FakePod).addr();
        let table = KindTable::new();
        // FakePod not inserted — the type lookup misses and
        // decode_typed returns None (silent miss is the v1
        // behaviour; the deferred `ContainedIn` gate would make
        // this a compile error).
        let mail = unsafe { Mail::__from_ptr_test_with_table(7, ptr_raw, 1, SENDER_NONE, &table) };
        assert!(mail.decode_typed::<FakePod>().is_none());
    }

    #[test]
    fn mail_decode_typed_wrong_count_returns_none() {
        let values = [FakePod { a: 5, b: 9 }, FakePod { a: 1, b: 1 }];
        let ptr_raw = values.as_ptr().addr();
        let table = KindTable::new();
        unsafe {
            table.insert(TypeId::of::<FakePod>(), 7);
        }
        let mail = unsafe { Mail::__from_ptr_test_with_table(7, ptr_raw, 2, SENDER_NONE, &table) };
        // decode_typed requires count == 1; use decode_slice_typed for batches.
        assert!(mail.decode_typed::<FakePod>().is_none());
    }

    #[test]
    fn mail_decode_slice_typed_roundtrip() {
        let values = [FakePod { a: 1, b: 2 }, FakePod { a: 3, b: 4 }];
        let ptr_raw = values.as_ptr().addr();
        let table = KindTable::new();
        unsafe {
            table.insert(TypeId::of::<FakePod>(), 7);
        }
        let mail = unsafe { Mail::__from_ptr_test_with_table(7, ptr_raw, 2, SENDER_NONE, &table) };
        let out = mail.decode_slice_typed::<FakePod>().unwrap();
        assert_eq!(out, &values);
    }
}
