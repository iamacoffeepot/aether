//! Concrete FFI ctx structs — [`FfiInitCtx`] / [`FfiCtx`] / [`FfiDropCtx`].
//!
//! Replaces the pre-issue-663 parametric `Ctx<'a, T>` / `InitCtx<'a, T>` /
//! `DropCtx<'a, T>` aliases that pinned `T = FfiTransport`. The ctx
//! interface is now spelled by the per-stage capability traits in
//! [`crate::actor::ctx`] — these structs are concrete impls bound to
//! [`FfiTransport`], plus inherent constructors (`__new`,
//! `__set_reply_to`) the [`crate::export!`] macro and the `#[actor]`
//! dispatcher use to thread per-mail state through the trampoline.

use core::marker::PhantomData;

use aether_data::{Kind, mailbox_id_from_name};

use crate::actor::ctx::mail_sender::MailSender;
use crate::actor::ctx::outbound_reply::OutboundReply;
use crate::actor::ctx::persistence::Persistence;
use crate::actor::ctx::resolver::Resolver;
use crate::actor::sender::{MailCtx, Sender};
use crate::actor::{Actor, HandlesKind, Singleton};
use crate::ffi::transport::FfiTransport;
use crate::mail::ReplyTo;
use crate::mail::mailbox::{ActorMailbox, KindId, Mailbox, resolve, resolve_mailbox};
use crate::mail::transport::MailTransport;

/// Init-only capability handle for FFI guests. Resolved during
/// `FfiActor::init`; not available at runtime (the type split fences
/// "when can I resolve?" against "when can I send?" at compile time).
pub struct FfiInitCtx<'a> {
    transport: &'a FfiTransport,
    mailbox: u64,
    _borrow: PhantomData<&'a ()>,
}

impl<'a> FfiInitCtx<'a> {
    /// Not part of the public API; called only by [`crate::export!`].
    #[doc(hidden)]
    pub fn __new(transport: &'a FfiTransport, mailbox: u64) -> Self {
        Self {
            transport,
            mailbox,
            _borrow: PhantomData,
        }
    }

    /// Borrow the actor's transport. Mirrors
    /// [`MailSender::transport`]; available without importing the
    /// trait at the call site.
    pub fn transport(&self) -> &FfiTransport {
        self.transport
    }

    /// The component's own mailbox id. Mirrors [`Resolver::mailbox_id`].
    pub fn mailbox_id(&self) -> u64 {
        self.mailbox
    }

    /// Resolve a kind by its `const ID`. Mirrors [`Resolver::resolve`].
    pub const fn resolve<K: Kind>(&self) -> KindId<K> {
        resolve::<K>()
    }

    /// Bind a mailbox name to kind `K`. Mirrors
    /// [`Resolver::resolve_mailbox`].
    pub const fn resolve_mailbox<K: Kind>(&self, name: &str) -> Mailbox<K, FfiTransport> {
        resolve_mailbox::<K, FfiTransport>(name)
    }

    /// Send `aether.input.subscribe` for kind `K`. Mirrors
    /// [`Resolver::subscribe_input`].
    pub fn subscribe_input<K: Kind + 'static>(&self) {
        <Self as Resolver>::subscribe_input::<K>(self)
    }

    /// Singleton sender shortcut. Mirrors [`MailSender::actor`].
    pub fn actor<R: Singleton>(&self) -> ActorMailbox<'_, R, FfiTransport> {
        ActorMailbox::__new(mailbox_id_from_name(R::NAMESPACE).0, self.transport)
    }

    /// Multi-instance sender. Mirrors [`MailSender::resolve_actor`].
    pub fn resolve_actor<R: Actor>(&self, name: &str) -> ActorMailbox<'_, R, FfiTransport> {
        ActorMailbox::__new(mailbox_id_from_name(name).0, self.transport)
    }
}

impl<'a> MailSender for FfiInitCtx<'a> {
    type Transport = FfiTransport;
    fn transport(&self) -> &FfiTransport {
        self.transport
    }
}

impl<'a> Resolver for FfiInitCtx<'a> {
    fn mailbox_id(&self) -> u64 {
        self.mailbox
    }

    fn resolve<K: Kind>(&self) -> KindId<K> {
        resolve::<K>()
    }

    fn resolve_mailbox<K: Kind>(&self, name: &str) -> Mailbox<K, FfiTransport> {
        resolve_mailbox::<K, FfiTransport>(name)
    }

    fn subscribe_input<K: Kind + 'static>(&self) {
        use aether_kinds::SubscribeInput;
        let payload = SubscribeInput {
            kind: <K as Kind>::ID,
            mailbox: aether_data::MailboxId(self.mailbox),
        };
        resolve_mailbox::<SubscribeInput, FfiTransport>("aether.input")
            .send(self.transport, &payload);
    }
}

impl<'a> Sender for FfiInitCtx<'a> {
    fn send<R, K>(&mut self, payload: &K)
    where
        R: Actor + HandlesKind<K>,
        K: Kind,
    {
        ActorMailbox::<R, FfiTransport>::__new(
            mailbox_id_from_name(R::NAMESPACE).0,
            self.transport,
        )
        .send(payload);
    }

    fn send_many<R, K>(&mut self, payloads: &[K])
    where
        R: Actor + HandlesKind<K>,
        K: Kind + bytemuck::NoUninit,
    {
        ActorMailbox::<R, FfiTransport>::__new(
            mailbox_id_from_name(R::NAMESPACE).0,
            self.transport,
        )
        .send_many(payloads);
    }

    fn send_to_named<K: Kind>(&mut self, name: &str, payload: &K) {
        resolve_mailbox::<K, FfiTransport>(name).send(self.transport, payload);
    }
}

/// Per-receive capability handle for FFI guests. Exposes send and
/// reply primitives; resolution is intentionally absent (resolution
/// belongs at init).
pub struct FfiCtx<'a> {
    transport: &'a FfiTransport,
    sender: Option<u32>,
    _borrow: PhantomData<&'a ()>,
}

impl<'a> FfiCtx<'a> {
    /// Not part of the public API; called only by [`crate::export!`].
    #[doc(hidden)]
    pub fn __new(transport: &'a FfiTransport) -> Self {
        Self {
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

    /// 3-arg back-compat reply: explicit `sender` + `kind`. Pre-trait
    /// callers (older examples and demos) thread `mail.reply_to()` and
    /// a cached `KindId<K>` through this method. The [`OutboundReply::reply`]
    /// trait method is the new shape — it pulls the sender from
    /// internal state and infers the kind from `K`.
    pub fn reply<K: Kind>(&self, sender: ReplyTo, kind: KindId<K>, payload: &K) {
        let bytes = payload.encode_into_bytes();
        self.transport
            .reply_mail(sender.raw(), kind.raw(), &bytes, 1);
    }

    /// Borrow the actor's transport. Mirrors
    /// [`MailSender::transport`]; available without importing the
    /// trait at the call site.
    pub fn transport(&self) -> &FfiTransport {
        self.transport
    }

    /// Reply target for the mail currently being dispatched. Mirrors
    /// [`OutboundReply::reply_to`].
    pub fn reply_to(&self) -> Option<ReplyTo> {
        self.sender.map(ReplyTo::__from_raw)
    }

    /// Singleton sender shortcut. Mirrors [`MailSender::actor`].
    pub fn actor<R: Singleton>(&self) -> ActorMailbox<'_, R, FfiTransport> {
        ActorMailbox::__new(mailbox_id_from_name(R::NAMESPACE).0, self.transport)
    }

    /// Multi-instance sender. Mirrors [`MailSender::resolve_actor`].
    pub fn resolve_actor<R: Actor>(&self, name: &str) -> ActorMailbox<'_, R, FfiTransport> {
        ActorMailbox::__new(mailbox_id_from_name(name).0, self.transport)
    }
}

impl<'a> MailSender for FfiCtx<'a> {
    type Transport = FfiTransport;
    fn transport(&self) -> &FfiTransport {
        self.transport
    }
}

impl<'a> OutboundReply for FfiCtx<'a> {
    type ReplyHandle = ReplyTo;

    fn reply_to(&self) -> Option<ReplyTo> {
        self.sender.map(ReplyTo::__from_raw)
    }

    fn origin(&self) -> Option<aether_data::MailboxId> {
        None
    }

    fn reply<K: Kind + serde::Serialize>(&mut self, payload: &K) {
        if let Some(raw) = self.sender {
            let bytes = payload.encode_into_bytes();
            self.transport.reply_mail(raw, K::ID.0, &bytes, 1);
        }
    }
}

impl<'a> Sender for FfiCtx<'a> {
    fn send<R, K>(&mut self, payload: &K)
    where
        R: Actor + HandlesKind<K>,
        K: Kind,
    {
        ActorMailbox::<R, FfiTransport>::__new(
            mailbox_id_from_name(R::NAMESPACE).0,
            self.transport,
        )
        .send(payload);
    }

    fn send_many<R, K>(&mut self, payloads: &[K])
    where
        R: Actor + HandlesKind<K>,
        K: Kind + bytemuck::NoUninit,
    {
        ActorMailbox::<R, FfiTransport>::__new(
            mailbox_id_from_name(R::NAMESPACE).0,
            self.transport,
        )
        .send_many(payloads);
    }

    fn send_to_named<K: Kind>(&mut self, name: &str, payload: &K) {
        resolve_mailbox::<K, FfiTransport>(name).send(self.transport, payload);
    }
}

impl<'a> MailCtx for FfiCtx<'a> {
    fn reply<K: Kind + serde::Serialize>(&mut self, payload: &K) {
        if let Some(raw) = self.sender {
            let bytes = payload.encode_into_bytes();
            self.transport.reply_mail(raw, K::ID.0, &bytes, 1);
        }
    }
}

/// Narrowed capability handle for shutdown hooks (`on_replace`,
/// `on_drop`). Outbound mail still works through [`Sender`]; the
/// reply / resolve surfaces are intentionally absent.
pub struct FfiDropCtx<'a> {
    transport: &'a FfiTransport,
    _borrow: PhantomData<&'a ()>,
}

impl<'a> FfiDropCtx<'a> {
    /// Not part of the public API; called only by [`crate::export!`].
    #[doc(hidden)]
    pub fn __new(transport: &'a FfiTransport) -> Self {
        Self {
            transport,
            _borrow: PhantomData,
        }
    }

    /// Borrow the actor's transport. Mirrors
    /// [`MailSender::transport`]; available without importing the
    /// trait at the call site.
    pub fn transport(&self) -> &FfiTransport {
        self.transport
    }

    /// Deposit a migration bundle. Mirrors [`Persistence::save_state`].
    pub fn save_state(&mut self, version: u32, bytes: &[u8]) {
        let status = self.transport.save_state(version, bytes);
        if status != 0 {
            panic!("aether-actor: save_state failed (status {status})");
        }
    }

    /// Persist a typed kind value. Mirrors
    /// [`Persistence::save_state_kind`].
    pub fn save_state_kind<K>(&mut self, version: u32, value: &K)
    where
        K: Kind + aether_data::Schema + serde::Serialize,
    {
        <Self as Persistence>::save_state_kind::<K>(self, version, value);
    }
}

impl<'a> MailSender for FfiDropCtx<'a> {
    type Transport = FfiTransport;
    fn transport(&self) -> &FfiTransport {
        self.transport
    }
}

impl<'a> Persistence for FfiDropCtx<'a> {
    fn save_state(&mut self, version: u32, bytes: &[u8]) {
        let status = self.transport.save_state(version, bytes);
        if status != 0 {
            panic!("aether-actor: save_state failed (status {status})");
        }
    }
}

impl<'a> Sender for FfiDropCtx<'a> {
    fn send<R, K>(&mut self, payload: &K)
    where
        R: Actor + HandlesKind<K>,
        K: Kind,
    {
        ActorMailbox::<R, FfiTransport>::__new(
            mailbox_id_from_name(R::NAMESPACE).0,
            self.transport,
        )
        .send(payload);
    }

    fn send_many<R, K>(&mut self, payloads: &[K])
    where
        R: Actor + HandlesKind<K>,
        K: Kind + bytemuck::NoUninit,
    {
        ActorMailbox::<R, FfiTransport>::__new(
            mailbox_id_from_name(R::NAMESPACE).0,
            self.transport,
        )
        .send_many(payloads);
    }

    fn send_to_named<K: Kind>(&mut self, name: &str, payload: &K) {
        resolve_mailbox::<K, FfiTransport>(name).send(self.transport, payload);
    }
}
