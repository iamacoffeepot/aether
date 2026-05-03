//! Lifecycle capability handles — `InitCtx`, `Ctx`, `DropCtx`. All
//! three are generic over `T: MailTransport` so their `send` / `reply`
//! / `save_state` bodies dispatch through the consumer crate's
//! transport impl. Existing wasm components see them via 1-arg type
//! aliases in `aether-component` (`pub type Ctx<'a> = aether_actor::
//! Ctx<'a, WasmTransport>`), so user code keeps writing `Ctx<'_>`.
//!
//! The split between the three handles is the same as today:
//! `InitCtx` is init-only (resolve is a compile-time op now, but the
//! handle threads the component's mailbox id and is the right place
//! for input-stream subscriptions); `Ctx` is the per-receive vocabulary
//! (send, reply, publish); `DropCtx` is the narrowed shutdown surface
//! (send + save_state, no reply).

use alloc::vec::Vec;
use core::marker::PhantomData;

use aether_data::{Kind, Schema};

use crate::handle::{self, Handle, SyncHandleError};
use crate::mail::ReplyTo;
use crate::sink::{KindId, Sink, resolve, resolve_sink};
use crate::transport::MailTransport;

/// Init-only capability handle. The type split between `InitCtx` and
/// `Ctx` fences "when can I resolve?" (init only) and "when can I
/// send?" (receive only) at compile time — calling `resolve` from a
/// `&mut Ctx` is a type error, not a convention.
///
/// The component's own mailbox id rides here — the substrate passes it
/// into `init` at instantiation (ADR-0030 Phase 2) and the SDK uses
/// it to self-address `aether.control.subscribe_input` mails for
/// every `K::IS_INPUT` kind handled by the component.
pub struct InitCtx<'a, T: MailTransport> {
    mailbox: u64,
    _borrow: PhantomData<&'a ()>,
    _t: PhantomData<fn() -> T>,
}

impl<T: MailTransport> InitCtx<'_, T> {
    /// Not part of the public API; called only by `export!`.
    #[doc(hidden)]
    pub fn __new(mailbox: u64) -> Self {
        InitCtx {
            mailbox,
            _borrow: PhantomData,
            _t: PhantomData,
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
    /// typed `Sink<K, T>`. Pure compile-time construction.
    pub const fn resolve_sink<K: Kind>(&self, name: &str) -> Sink<K, T> {
        resolve_sink::<K, T>(name)
    }

    /// Publish `value` into the substrate's handle store at init.
    /// See [`handle::publish`] for full semantics — this is the
    /// init-time twin of [`Ctx::publish`] / [`DropCtx::publish`].
    pub fn publish<K: Kind + serde::Serialize>(
        &self,
        value: &K,
    ) -> Result<Handle<K, T>, SyncHandleError> {
        handle::publish::<K, T>(value)
    }

    /// Send `aether.control.subscribe_input` with this component's
    /// mailbox as the subscriber for `K`. ADR-0068 keys subscriber
    /// sets by `KindId` directly, so this collapses to a one-line
    /// send: any `Kind` is sendable, the substrate's platform thread
    /// fans out only for kinds it actually publishes, and a subscribe
    /// for a non-stream kind is a harmless no-op.
    pub fn subscribe_input<K: Kind + 'static>(&self) {
        use aether_kinds::SubscribeInput;
        let payload = SubscribeInput {
            kind: <K as Kind>::ID,
            mailbox: ::aether_data::MailboxId(self.mailbox),
        };
        resolve_sink::<SubscribeInput, T>("aether.control").send(&payload);
    }
}

/// Per-receive capability handle. Exposes send primitives only.
/// Resolution is intentionally absent — runtime resolution after init
/// is not a supported shape.
///
/// ADR-0033: typed handlers receive `K` by value, so they no longer
/// hold a `Mail<'_>` to call `mail.reply_to()` on. The synthesized
/// dispatcher threads the inbound mail's sender onto `Ctx` via
/// `__set_reply_to` before every handler call, and `Ctx::reply_to()`
/// reads it back. `#[fallback]` methods still receive the raw
/// `Mail<'_>` and can call `mail.reply_to()` directly.
pub struct Ctx<'a, T: MailTransport> {
    sender: Option<u32>,
    _borrow: PhantomData<&'a ()>,
    _t: PhantomData<fn() -> T>,
}

impl<T: MailTransport> Ctx<'_, T> {
    /// Not part of the public API; called only by `export!`.
    #[doc(hidden)]
    pub fn __new() -> Self {
        Ctx {
            sender: None,
            _borrow: PhantomData,
            _t: PhantomData,
        }
    }

    /// Not part of the public API; called only by the `#[handlers]`
    /// dispatcher. Accepts `None` or `Some(ReplyTo)` — the dispatcher
    /// passes `mail.reply_to()` verbatim so component-origin and
    /// broadcast mail (which have no reply target) land as `None`.
    #[doc(hidden)]
    pub fn __set_reply_to(&mut self, sender: Option<ReplyTo>) {
        self.sender = sender.map(|s| s.raw());
    }

    /// Reply handle for the mail currently being dispatched. `None`
    /// for component-origin and broadcast-origin mail; `Some(ReplyTo)`
    /// when the inbound came from a Claude session. Pass the returned
    /// `ReplyTo` back to `Ctx::reply` to answer the originating
    /// session (ADR-0013).
    pub fn reply_to(&self) -> Option<ReplyTo> {
        self.sender.map(ReplyTo::__from_raw)
    }

    /// Send a single payload to `sink`. Typed wrapper around
    /// `Sink::send` — having the same entry point through both
    /// `Ctx` and `Sink` is deliberate: `Ctx` is the receive-time
    /// vocabulary, `Sink::send` is the universal one. Wire shape
    /// (cast or postcard) is the kind's, not the call's (issue #240).
    pub fn send<K: Kind>(&self, sink: &Sink<K, T>, payload: &K) {
        sink.send(payload);
    }

    /// Send a slice of payloads as a contiguous batch. Cast-only —
    /// see [`Sink::send_many`] for the wire-shape rationale.
    pub fn send_many<K: Kind + bytemuck::NoUninit>(&self, sink: &Sink<K, T>, payloads: &[K]) {
        sink.send_many(payloads);
    }

    /// Publish `value` into the substrate's handle store. Returns
    /// the typed [`Handle<K>`] — its RAII drop fires
    /// `HandleRelease` so the publisher's refcount goes back to
    /// zero when the handle leaves scope. See [`handle::publish`]
    /// for the synchronous round-trip semantics.
    pub fn publish<K: Kind + serde::Serialize>(
        &self,
        value: &K,
    ) -> Result<Handle<K, T>, SyncHandleError> {
        handle::publish::<K, T>(value)
    }

    /// Reply to the Claude session that originated the inbound mail
    /// (ADR-0013). `sender` came from `mail.reply_to()` on the current
    /// receive — pass it back as the routing handle. The kind is
    /// supplied as a typed `KindId<K>` so the same compile-time
    /// matching the rest of the SDK uses applies here too.
    ///
    /// Status of the underlying host call is dropped; reply is
    /// fire-and-forget on the guest side. If the session is gone the
    /// hub silently discards the frame. Issue #240: wire shape (cast
    /// or postcard) follows `Kind::encode_into_bytes` — the same
    /// derive-time autodetect as `Ctx::send`.
    pub fn reply<K: Kind>(&self, sender: ReplyTo, kind: KindId<K>, payload: &K) {
        let bytes = payload.encode_into_bytes();
        T::reply_mail(sender.raw(), kind.raw(), &bytes, 1);
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
pub struct DropCtx<'a, T: MailTransport> {
    _borrow: PhantomData<&'a ()>,
    _t: PhantomData<fn() -> T>,
}

impl<T: MailTransport> DropCtx<'_, T> {
    /// Not part of the public API; called only by `export!`.
    #[doc(hidden)]
    pub fn __new() -> Self {
        DropCtx {
            _borrow: PhantomData,
            _t: PhantomData,
        }
    }

    /// Send a single payload during a shutdown hook. Wire shape (cast
    /// or postcard) follows `Kind::encode_into_bytes`.
    pub fn send<K: Kind>(&self, sink: &Sink<K, T>, payload: &K) {
        sink.send(payload);
    }

    /// Send a slice of payloads during a shutdown hook. Cast-only —
    /// see [`Sink::send_many`] for the wire-shape rationale.
    pub fn send_many<K: Kind + bytemuck::NoUninit>(&self, sink: &Sink<K, T>, payloads: &[K]) {
        sink.send_many(payloads);
    }

    /// Publish `value` into the substrate's handle store during a
    /// shutdown hook. Common pattern at `on_replace`: pin the
    /// returned handle so the cached bytes survive the hand-off
    /// to the next instance, then drop it (`Handle::pin` followed
    /// by drop releases the local guard but keeps the entry).
    pub fn publish<K: Kind + serde::Serialize>(
        &self,
        value: &K,
    ) -> Result<Handle<K, T>, SyncHandleError> {
        handle::publish::<K, T>(value)
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
        let status = T::save_state(version, bytes);
        if status != 0 {
            panic!("aether-actor: save_state failed (status {status})");
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
        let mut out = Vec::from(K::ID.0.to_le_bytes());
        let payload = postcard::to_allocvec(value).expect("postcard encode to Vec is infallible");
        out.extend_from_slice(&payload);
        self.save_state(version, &out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::MailTransport;

    /// Stub transport — `DropCtx::__new()` doesn't dispatch through
    /// `T`, but the type bound forces something concrete. Lives next
    /// to the test; not a public surface.
    struct NoopTransport;
    impl MailTransport for NoopTransport {
        fn send_mail(_: u64, _: u64, _: &[u8], _: u32) -> u32 {
            0
        }
        fn reply_mail(_: u32, _: u64, _: &[u8], _: u32) -> u32 {
            0
        }
        fn save_state(_: u32, _: &[u8]) -> u32 {
            0
        }
        fn wait_reply(_: u64, _: &mut [u8], _: u32, _: u64) -> i32 {
            -1
        }
        fn prev_correlation() -> u64 {
            0
        }
    }

    /// `DropCtx::__new()` must be callable without special setup so
    /// the `export!` macro can build one inside a `#[no_mangle]` shim.
    /// The accessor covered here just verifies the constructor type
    /// is well-formed; send/send_many require a real FFI and are not
    /// unit-testable on host.
    #[test]
    fn drop_ctx_constructor_well_formed() {
        let _ctx: DropCtx<'_, NoopTransport> = DropCtx::__new();
    }
}
