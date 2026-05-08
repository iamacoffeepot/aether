//! Concrete FFI ctx structs — [`FfiInitCtx`] / [`FfiCtx`] / [`FfiDropCtx`].
//!
//! Replaces the pre-issue-663 parametric `Ctx<'a, T>` / `InitCtx<'a, T>` /
//! `DropCtx<'a, T>` aliases. The ctx interface is now spelled by the
//! per-stage capability traits in [`crate::actor::ctx`]; these structs
//! are concrete impls that route outbound calls through the
//! per-concern bridge ZSTs in [`crate::ffi::bridge`] ([`MAIL_BRIDGE`] /
//! [`PERSIST_BRIDGE`] / [`SYNC_WAIT_BRIDGE`]).
//!
//! Issue 665 retired the `transport: &'a FfiTransport` field along
//! with the `FfiTransport` ZST and `MailTransport` trait — ctxs hold
//! per-mail state only (mailbox id at init; reply target at receive),
//! and dispatch goes through the bridge statics directly.

use core::marker::PhantomData;

use aether_data::{Kind, mailbox_id_from_name};

use crate::actor::ctx::mail_sender::MailSender;
use crate::actor::ctx::outbound_reply::OutboundReply;
use crate::actor::ctx::persistence::Persistence;
use crate::actor::ctx::resolver::Resolver;
use crate::actor::ctx::sync_waiter::SyncWaiter;
use crate::actor::sender::{MailCtx, Sender};
use crate::actor::{Actor, HandlesKind, Singleton};
use crate::ffi::bridge::{MAIL_BRIDGE, PERSIST_BRIDGE, SYNC_WAIT_BRIDGE};
use crate::ffi::mailbox::FfiActorMailbox;
use crate::mail::ReplyTo;
use crate::mail::mailbox::{KindId, Mailbox, resolve, resolve_mailbox};
use crate::mail::sync::WaitError;

/// Init-only capability handle for FFI guests. Resolved during
/// `FfiActor::init`; not available at runtime (the type split fences
/// "when can I resolve?" against "when can I send?" at compile time).
pub struct FfiInitCtx<'a> {
    mailbox: u64,
    _borrow: PhantomData<&'a ()>,
}

impl<'a> FfiInitCtx<'a> {
    /// Not part of the public API; called only by [`crate::export!`].
    #[doc(hidden)]
    pub fn __new(mailbox: u64) -> Self {
        Self {
            mailbox,
            _borrow: PhantomData,
        }
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
    pub const fn resolve_mailbox<K: Kind>(&self, name: &str) -> Mailbox<K> {
        resolve_mailbox::<K>(name)
    }

    /// Send `aether.input.subscribe` for kind `K`. Mirrors
    /// [`Resolver::subscribe_input`].
    pub fn subscribe_input<K: Kind + 'static>(&self) {
        <Self as Resolver>::subscribe_input::<K>(self)
    }

    /// Singleton sender shortcut. Returns a typed [`FfiActorMailbox`]
    /// addressing the unique instance of receiver actor `R`.
    pub fn actor<R: Singleton>(&self) -> FfiActorMailbox<R> {
        FfiActorMailbox::__new(mailbox_id_from_name(R::NAMESPACE).0)
    }

    /// Multi-instance sender. Resolve a typed [`FfiActorMailbox`]
    /// from a runtime instance name.
    pub fn resolve_actor<R: Actor>(&self, name: &str) -> FfiActorMailbox<R> {
        FfiActorMailbox::__new(mailbox_id_from_name(name).0)
    }
}

impl<'a> MailSender for FfiInitCtx<'a> {
    fn send<R, K>(&mut self, payload: &K)
    where
        R: Actor + HandlesKind<K>,
        K: Kind,
    {
        let bytes = payload.encode_into_bytes();
        MAIL_BRIDGE.send_mail(mailbox_id_from_name(R::NAMESPACE).0, K::ID.0, &bytes, 1);
    }

    fn send_many<R, K>(&mut self, payloads: &[K])
    where
        R: Actor + HandlesKind<K>,
        K: Kind + bytemuck::NoUninit,
    {
        let bytes: &[u8] = bytemuck::cast_slice(payloads);
        MAIL_BRIDGE.send_mail(
            mailbox_id_from_name(R::NAMESPACE).0,
            K::ID.0,
            bytes,
            payloads.len() as u32,
        );
    }

    fn send_to_named<K: Kind>(&mut self, name: &str, payload: &K) {
        let bytes = payload.encode_into_bytes();
        MAIL_BRIDGE.send_mail(mailbox_id_from_name(name).0, K::ID.0, &bytes, 1);
    }

    fn prev_correlation(&self) -> u64 {
        MAIL_BRIDGE.prev_correlation()
    }
}

impl<'a> Resolver for FfiInitCtx<'a> {
    fn mailbox_id(&self) -> u64 {
        self.mailbox
    }

    fn resolve<K: Kind>(&self) -> KindId<K> {
        resolve::<K>()
    }

    fn resolve_mailbox<K: Kind>(&self, name: &str) -> Mailbox<K> {
        resolve_mailbox::<K>(name)
    }

    fn subscribe_input<K: Kind + 'static>(&self) {
        use aether_kinds::SubscribeInput;
        let payload = SubscribeInput {
            kind: <K as Kind>::ID,
            mailbox: aether_data::MailboxId(self.mailbox),
        };
        let bytes = payload.encode_into_bytes();
        MAIL_BRIDGE.send_mail(
            mailbox_id_from_name("aether.input").0,
            SubscribeInput::ID.0,
            &bytes,
            1,
        );
    }
}

impl<'a> Sender for FfiInitCtx<'a> {
    fn send<R, K>(&mut self, payload: &K)
    where
        R: Actor + HandlesKind<K>,
        K: Kind,
    {
        <Self as MailSender>::send::<R, K>(self, payload);
    }

    fn send_many<R, K>(&mut self, payloads: &[K])
    where
        R: Actor + HandlesKind<K>,
        K: Kind + bytemuck::NoUninit,
    {
        <Self as MailSender>::send_many::<R, K>(self, payloads);
    }

    fn send_to_named<K: Kind>(&mut self, name: &str, payload: &K) {
        <Self as MailSender>::send_to_named::<K>(self, name, payload);
    }
}

/// Per-receive capability handle for FFI guests. Exposes send and
/// reply primitives; resolution is intentionally absent (resolution
/// belongs at init).
pub struct FfiCtx<'a> {
    sender: Option<u32>,
    _borrow: PhantomData<&'a ()>,
}

impl<'a> FfiCtx<'a> {
    /// Not part of the public API; called only by [`crate::export!`].
    #[doc(hidden)]
    pub fn __new() -> Self {
        Self {
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

    /// Reply with an explicit `sender` + cached `KindId<K>`. Sits
    /// alongside the trait-driven [`OutboundReply::reply`] which uses
    /// the dispatcher-stamped sender plus `K::ID`. Useful for FFI
    /// guests sending cast-shaped types that don't impl
    /// `serde::Serialize` (the trait method's bound covers native's
    /// postcard reply path; FFI's `reply_mail` only ships bytes via
    /// [`Kind::encode_into_bytes`], so the bound is over-strict for
    /// guest-side cast kinds).
    pub fn reply_kind<K: Kind>(&self, sender: ReplyTo, kind: KindId<K>, payload: &K) {
        let bytes = payload.encode_into_bytes();
        MAIL_BRIDGE.reply_mail(sender.raw(), kind.raw(), &bytes, 1);
    }

    /// Reply target for the mail currently being dispatched. Mirrors
    /// [`OutboundReply::reply_to`].
    pub fn reply_to(&self) -> Option<ReplyTo> {
        self.sender.map(ReplyTo::__from_raw)
    }

    /// Singleton sender shortcut. Returns a typed [`FfiActorMailbox`]
    /// addressing the unique instance of receiver actor `R`.
    pub fn actor<R: Singleton>(&self) -> FfiActorMailbox<R> {
        FfiActorMailbox::__new(mailbox_id_from_name(R::NAMESPACE).0)
    }

    /// Multi-instance sender. Resolve a typed [`FfiActorMailbox`]
    /// from a runtime instance name.
    pub fn resolve_actor<R: Actor>(&self, name: &str) -> FfiActorMailbox<R> {
        FfiActorMailbox::__new(mailbox_id_from_name(name).0)
    }
}

impl<'a> MailSender for FfiCtx<'a> {
    fn send<R, K>(&mut self, payload: &K)
    where
        R: Actor + HandlesKind<K>,
        K: Kind,
    {
        let bytes = payload.encode_into_bytes();
        MAIL_BRIDGE.send_mail(mailbox_id_from_name(R::NAMESPACE).0, K::ID.0, &bytes, 1);
    }

    fn send_many<R, K>(&mut self, payloads: &[K])
    where
        R: Actor + HandlesKind<K>,
        K: Kind + bytemuck::NoUninit,
    {
        let bytes: &[u8] = bytemuck::cast_slice(payloads);
        MAIL_BRIDGE.send_mail(
            mailbox_id_from_name(R::NAMESPACE).0,
            K::ID.0,
            bytes,
            payloads.len() as u32,
        );
    }

    fn send_to_named<K: Kind>(&mut self, name: &str, payload: &K) {
        let bytes = payload.encode_into_bytes();
        MAIL_BRIDGE.send_mail(mailbox_id_from_name(name).0, K::ID.0, &bytes, 1);
    }

    fn prev_correlation(&self) -> u64 {
        MAIL_BRIDGE.prev_correlation()
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
            MAIL_BRIDGE.reply_mail(raw, K::ID.0, &bytes, 1);
        }
    }
}

impl<'a> SyncWaiter for FfiCtx<'a> {
    fn wait_reply<K, E>(
        &self,
        timeout_ms: u32,
        capacity: usize,
        expected_correlation: u64,
    ) -> Result<K, E>
    where
        K: Kind + serde::de::DeserializeOwned,
        E: WaitError,
    {
        crate::actor::ctx::sync_waiter::wait_reply_via::<K, E>(
            |kind, out, timeout, corr| SYNC_WAIT_BRIDGE.wait_reply(kind, out, timeout, corr),
            timeout_ms,
            capacity,
            expected_correlation,
        )
    }
}

impl<'a> Sender for FfiCtx<'a> {
    fn send<R, K>(&mut self, payload: &K)
    where
        R: Actor + HandlesKind<K>,
        K: Kind,
    {
        <Self as MailSender>::send::<R, K>(self, payload);
    }

    fn send_many<R, K>(&mut self, payloads: &[K])
    where
        R: Actor + HandlesKind<K>,
        K: Kind + bytemuck::NoUninit,
    {
        <Self as MailSender>::send_many::<R, K>(self, payloads);
    }

    fn send_to_named<K: Kind>(&mut self, name: &str, payload: &K) {
        <Self as MailSender>::send_to_named::<K>(self, name, payload);
    }
}

impl<'a> MailCtx for FfiCtx<'a> {
    fn reply<K: Kind + serde::Serialize>(&mut self, payload: &K) {
        if let Some(raw) = self.sender {
            let bytes = payload.encode_into_bytes();
            MAIL_BRIDGE.reply_mail(raw, K::ID.0, &bytes, 1);
        }
    }
}

/// Narrowed capability handle for shutdown hooks (`on_replace`,
/// `on_drop`). Outbound mail still works through [`Sender`]; the
/// reply / resolve surfaces are intentionally absent.
pub struct FfiDropCtx<'a> {
    _borrow: PhantomData<&'a ()>,
}

impl<'a> FfiDropCtx<'a> {
    /// Not part of the public API; called only by [`crate::export!`].
    #[doc(hidden)]
    pub fn __new() -> Self {
        Self {
            _borrow: PhantomData,
        }
    }

    /// Deposit a migration bundle. Mirrors [`Persistence::save_state`].
    pub fn save_state(&mut self, version: u32, bytes: &[u8]) {
        let status = PERSIST_BRIDGE.save_state(version, bytes);
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
    fn send<R, K>(&mut self, payload: &K)
    where
        R: Actor + HandlesKind<K>,
        K: Kind,
    {
        let bytes = payload.encode_into_bytes();
        MAIL_BRIDGE.send_mail(mailbox_id_from_name(R::NAMESPACE).0, K::ID.0, &bytes, 1);
    }

    fn send_many<R, K>(&mut self, payloads: &[K])
    where
        R: Actor + HandlesKind<K>,
        K: Kind + bytemuck::NoUninit,
    {
        let bytes: &[u8] = bytemuck::cast_slice(payloads);
        MAIL_BRIDGE.send_mail(
            mailbox_id_from_name(R::NAMESPACE).0,
            K::ID.0,
            bytes,
            payloads.len() as u32,
        );
    }

    fn send_to_named<K: Kind>(&mut self, name: &str, payload: &K) {
        let bytes = payload.encode_into_bytes();
        MAIL_BRIDGE.send_mail(mailbox_id_from_name(name).0, K::ID.0, &bytes, 1);
    }

    fn prev_correlation(&self) -> u64 {
        MAIL_BRIDGE.prev_correlation()
    }
}

impl<'a> Persistence for FfiDropCtx<'a> {
    fn save_state(&mut self, version: u32, bytes: &[u8]) {
        let status = PERSIST_BRIDGE.save_state(version, bytes);
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
        <Self as MailSender>::send::<R, K>(self, payload);
    }

    fn send_many<R, K>(&mut self, payloads: &[K])
    where
        R: Actor + HandlesKind<K>,
        K: Kind + bytemuck::NoUninit,
    {
        <Self as MailSender>::send_many::<R, K>(self, payloads);
    }

    fn send_to_named<K: Kind>(&mut self, name: &str, payload: &K) {
        <Self as MailSender>::send_to_named::<K>(self, name, payload);
    }
}
