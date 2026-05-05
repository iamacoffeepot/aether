//! Lifecycle capability handles — `InitCtx`, `Ctx`, `DropCtx`. All
//! three are generic over `T: MailTransport` AND borrow a transport
//! reference for the lifetime of the receive call. `send` / `reply`
//! / `save_state` etc. dispatch through `self.transport` rather than
//! a static-method trait — that's what lets `NativeTransport` carry
//! per-actor state in regular fields instead of a thread-local.
//!
//! Existing wasm components see them via 1-arg type aliases in
//! `aether-component` (`pub type Ctx<'a> = aether_actor::Ctx<'a,
//! WasmTransport>`), so user code keeps writing `Ctx<'_>`. The
//! transport reference is supplied internally — for the wasm path
//! that's `&WASM_TRANSPORT` (a `pub static` ZST defined in
//! `aether-component`); for native that's the capability's owned
//! `NativeTransport`.

use alloc::vec::Vec;
use core::marker::PhantomData;

use aether_data::{Kind, Schema, mailbox_id_from_name};

use crate::actor::{Actor, HandlesKind, Singleton};
use crate::mail::ReplyTo;
use crate::sender::{MailCtx, Sender};
use crate::sink::{ActorMailbox, KindId, Mailbox, resolve, resolve_mailbox};
use crate::transport::MailTransport;

/// Init-only capability handle. The type split between `InitCtx` and
/// `Ctx` fences "when can I resolve?" (init only) and "when can I
/// send?" (receive only) at compile time — calling `resolve` from a
/// `&mut Ctx` is a type error, not a convention.
///
/// Carries a borrow of the actor's transport instance so `send` /
/// `subscribe_input` / `publish` can dispatch through it without
/// touching a thread-local. The component's own mailbox id rides
/// here too — the substrate passes it into `init` at instantiation
/// (ADR-0030 Phase 2) and the SDK uses it to self-address
/// `aether.control.subscribe_input` mails for every `K::IS_INPUT`
/// kind handled by the component.
pub struct InitCtx<'a, T: MailTransport> {
    transport: &'a T,
    mailbox: u64,
    _borrow: PhantomData<&'a ()>,
}

impl<'a, T: MailTransport> InitCtx<'a, T> {
    /// Not part of the public API; called only by `export!`.
    #[doc(hidden)]
    pub fn __new(transport: &'a T, mailbox: u64) -> Self {
        InitCtx {
            transport,
            mailbox,
            _borrow: PhantomData,
        }
    }

    /// Borrow the actor's transport. Exposed for advanced callers
    /// (lower-level helpers that resolve a `Sink` and want to call
    /// `Sink::send` directly with the transport ref); typical
    /// component code goes through the `publish` / `subscribe_input`
    /// methods on this handle instead.
    pub fn transport(&self) -> &T {
        self.transport
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
    /// typed `Mailbox<K, T>`. Pure compile-time construction.
    pub const fn resolve_mailbox<K: Kind>(&self, name: &str) -> Mailbox<K, T> {
        resolve_mailbox::<K, T>(name)
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
        resolve_mailbox::<SubscribeInput, T>("aether.control").send(self.transport, &payload);
    }

    /// Singleton sender shortcut: returns a typed [`ActorMailbox`] that
    /// addresses the unique instance of receiver actor `R`. The returned
    /// handle borrows this ctx's transport for the duration of the call,
    /// so subsequent `send` / `send_many` are `&self`-receiver and need
    /// no transport thread-through.
    ///
    /// `R: Singleton` gates this call to actors loaded under their
    /// `R::NAMESPACE` default name; multi-instance receivers go through
    /// [`Self::resolve_actor`] with an explicit runtime name.
    pub fn actor<R: Singleton>(&self) -> ActorMailbox<'_, R, T> {
        ActorMailbox::__new(mailbox_id_from_name(R::NAMESPACE).0, self.transport)
    }

    /// Multi-instance sender: resolve a typed [`ActorMailbox`] from a
    /// runtime instance name. The string surfaces ONCE per handle;
    /// subsequent sends are string-free and compile-checked against
    /// `R: HandlesKind<K>`.
    pub fn resolve_actor<R: Actor>(&self, name: &str) -> ActorMailbox<'_, R, T> {
        ActorMailbox::__new(mailbox_id_from_name(name).0, self.transport)
    }
}

/// Per-receive capability handle. Exposes send primitives only.
/// Resolution is intentionally absent — runtime resolution after init
/// is not a supported shape.
///
/// Holds the transport borrow so `ctx.actor::<R>().send(&payload)` is
/// the natural call shape inside a handler — no need for the user to
/// thread a transport reference through every send. ADR-0033: typed
/// handlers receive `K` by value, so they no longer hold a `Mail<'_>`
/// to call `mail.reply_to()` on. The synthesized dispatcher threads
/// the inbound mail's sender onto `Ctx` via `__set_reply_to` before
/// every handler call, and `Ctx::reply_to()` reads it back.
/// `#[fallback]` methods still receive the raw `Mail<'_>` and can
/// call `mail.reply_to()` directly.
pub struct Ctx<'a, T: MailTransport> {
    transport: &'a T,
    sender: Option<u32>,
    _borrow: PhantomData<&'a ()>,
}

impl<'a, T: MailTransport> Ctx<'a, T> {
    /// Not part of the public API; called only by `export!`.
    #[doc(hidden)]
    pub fn __new(transport: &'a T) -> Self {
        Ctx {
            transport,
            sender: None,
            _borrow: PhantomData,
        }
    }

    /// Not part of the public API; called only by the `#[actor]`
    /// dispatcher. Accepts `None` or `Some(ReplyTo)` — the dispatcher
    /// passes `mail.reply_to()` verbatim so component-origin and
    /// broadcast mail (which have no reply target) land as `None`.
    #[doc(hidden)]
    pub fn __set_reply_to(&mut self, sender: Option<ReplyTo>) {
        self.sender = sender.map(|s| s.raw());
    }

    /// Borrow the actor's transport. Exposed for the few SDK helper
    /// modules (`aether-actor::wasm::{io,net,log}`) that resolve a
    /// kind-typed `Mailbox<K, T>` internally and call its `send`
    /// directly with the transport ref.
    pub fn transport(&self) -> &T {
        self.transport
    }

    /// Reply handle for the mail currently being dispatched. `None`
    /// for component-origin and broadcast-origin mail; `Some(ReplyTo)`
    /// when the inbound came from a Claude session. Pass the returned
    /// `ReplyTo` back to `Ctx::reply` to answer the originating
    /// session (ADR-0013).
    pub fn reply_to(&self) -> Option<ReplyTo> {
        self.sender.map(ReplyTo::__from_raw)
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
        self.transport
            .reply_mail(sender.raw(), kind.raw(), &bytes, 1);
    }

    /// Singleton sender shortcut: returns a typed [`ActorMailbox`] that
    /// addresses the unique instance of receiver actor `R`. The
    /// returned handle borrows this ctx's transport, so subsequent
    /// `send` / `send_many` calls are `&self`-receiver and need no
    /// explicit transport argument.
    ///
    /// ```ignore
    /// ctx.actor::<LogCapability>().send(&LogEvent { ... });
    /// ```
    ///
    /// `R: Singleton` gates the shortcut to actors loaded under their
    /// `R::NAMESPACE` default name; multi-instance receivers go through
    /// [`Self::resolve_actor`] with an explicit runtime name. Wrong-
    /// kind sends are compile errors via `R: HandlesKind<K>` on
    /// [`ActorMailbox::send`].
    pub fn actor<R: Singleton>(&self) -> ActorMailbox<'_, R, T> {
        ActorMailbox::__new(mailbox_id_from_name(R::NAMESPACE).0, self.transport)
    }

    /// Multi-instance sender: resolve a typed [`ActorMailbox`] from a
    /// runtime instance name. The string surfaces ONCE per handle;
    /// subsequent sends through the returned handle are string-free
    /// and compile-checked against `R: HandlesKind<K>`.
    ///
    /// Use this when addressing one of several live instances of the
    /// same actor type (e.g. `"player_1"` vs `"player_2"`). For
    /// singletons (chassis caps, uniquely-loaded user components),
    /// [`Self::actor`] skips the explicit name.
    pub fn resolve_actor<R: Actor>(&self, name: &str) -> ActorMailbox<'_, R, T> {
        ActorMailbox::__new(mailbox_id_from_name(name).0, self.transport)
    }
}

/// Narrowed capability handle for shutdown hooks (`on_replace`,
/// `on_drop`). Like `Ctx`, but deliberately smaller:
///
/// - Outbound mail still works through the [`Sender`] trait
///   (`ctx.send::<R, _>(&kind)` / `ctx.send_to_named(name, &kind)`)
///   — outbound mail during shutdown is a valid pattern ("I'm going
///   away, here's the last thing I observed").
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
    transport: &'a T,
    _borrow: PhantomData<&'a ()>,
}

impl<'a, T: MailTransport> DropCtx<'a, T> {
    /// Not part of the public API; called only by `export!`.
    #[doc(hidden)]
    pub fn __new(transport: &'a T) -> Self {
        DropCtx {
            transport,
            _borrow: PhantomData,
        }
    }

    /// Borrow the actor's transport. Same shape as
    /// [`Ctx::transport`].
    pub fn transport(&self) -> &T {
        self.transport
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
        let status = self.transport.save_state(version, bytes);
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

/// Issue 552 stage 1: cross-transport [`Sender`] impl. Routes through
/// the actor-typed sink (`ActorMailbox::__new` bound to `self.transport`)
/// for typed sends and through the kind-typed sink (`resolve_mailbox::<K, T>`)
/// for the string-keyed escape hatch. The wasm path's `Ctx<'a, WasmTransport>`
/// inherits this impl through the [`crate::WasmCtx`] alias; the native
/// `NativeCtx<'a>` (in `aether-substrate`) writes its own `Sender`
/// impl since it doesn't go through `Ctx<'a, T>` (extra per-mail state
/// — origin mailbox + reply target — that the universal ctx doesn't
/// carry).
impl<'a, T: MailTransport> Sender for Ctx<'a, T> {
    fn send<R, K>(&mut self, payload: &K)
    where
        R: Actor + HandlesKind<K>,
        K: aether_data::Kind,
    {
        ActorMailbox::<R, T>::__new(mailbox_id_from_name(R::NAMESPACE).0, self.transport)
            .send(payload);
    }

    fn send_many<R, K>(&mut self, payloads: &[K])
    where
        R: Actor + HandlesKind<K>,
        K: aether_data::Kind + bytemuck::NoUninit,
    {
        ActorMailbox::<R, T>::__new(mailbox_id_from_name(R::NAMESPACE).0, self.transport)
            .send_many(payloads);
    }

    fn send_to_named<K: aether_data::Kind>(&mut self, name: &str, payload: &K) {
        resolve_mailbox::<K, T>(name).send(self.transport, payload);
    }
}

/// Issue 552 stage 1: per-handler reply surface on top of [`Sender`].
/// `reply::<K>(...)` re-encodes through `Kind::encode_into_bytes` and
/// dispatches via `T::reply_mail`. No-op when the inbound has no
/// reply target (component-origin / broadcast mail).
impl<'a, T: MailTransport> MailCtx for Ctx<'a, T> {
    fn reply<K: aether_data::Kind + serde::Serialize>(&mut self, payload: &K) {
        if let Some(raw) = self.sender {
            let bytes = payload.encode_into_bytes();
            self.transport.reply_mail(raw, K::ID.0, &bytes, 1);
        }
        // No-op on no-sender mail (broadcast or peer-component).
    }
}

/// Issue 552 stage 1: init-time [`Sender`] impl. Same routing as
/// per-handler `Sender`. Init contexts deliberately don't implement
/// [`MailCtx`] — there is no inbound mail at boot, so no sender or
/// reply target is defined.
impl<'a, T: MailTransport> Sender for InitCtx<'a, T> {
    fn send<R, K>(&mut self, payload: &K)
    where
        R: Actor + HandlesKind<K>,
        K: aether_data::Kind,
    {
        ActorMailbox::<R, T>::__new(mailbox_id_from_name(R::NAMESPACE).0, self.transport)
            .send(payload);
    }

    fn send_many<R, K>(&mut self, payloads: &[K])
    where
        R: Actor + HandlesKind<K>,
        K: aether_data::Kind + bytemuck::NoUninit,
    {
        ActorMailbox::<R, T>::__new(mailbox_id_from_name(R::NAMESPACE).0, self.transport)
            .send_many(payloads);
    }

    fn send_to_named<K: aether_data::Kind>(&mut self, name: &str, payload: &K) {
        resolve_mailbox::<K, T>(name).send(self.transport, payload);
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
        fn send_mail(&self, _: u64, _: u64, _: &[u8], _: u32) -> u32 {
            0
        }
        fn reply_mail(&self, _: u32, _: u64, _: &[u8], _: u32) -> u32 {
            0
        }
        fn save_state(&self, _: u32, _: &[u8]) -> u32 {
            0
        }
        fn wait_reply(&self, _: u64, _: &mut [u8], _: u32, _: u64) -> i32 {
            -1
        }
        fn prev_correlation(&self) -> u64 {
            0
        }
    }

    /// `DropCtx::__new()` must be callable without special setup so
    /// the `export!` macro can build one inside a `#[no_mangle]` shim.
    /// The accessor covered here just verifies the constructor type
    /// is well-formed; send/send_many require a real transport and
    /// are not unit-testable on host.
    #[test]
    fn drop_ctx_constructor_well_formed() {
        let transport = NoopTransport;
        let _ctx: DropCtx<'_, NoopTransport> = DropCtx::__new(&transport);
    }
}
