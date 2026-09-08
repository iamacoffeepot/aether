//! [`Sends`] — the receive ctx's outbound surface with the reply-class
//! marker dropped, so a helper that only sends mail needs no `M: ReplyMode`
//! parameter.
//!
//! [`WasmCtx<'_, M>`](WasmCtx) is generic over its reply class (ADR-0112,
//! ADR-0134) because the marker selects which reply surface the handler is
//! allowed to reach: `reply` / `reply_to` exist only on `Manual`, `emit` only
//! on `Multi<K>`, and neither on `Single`. That is load-bearing at the handler
//! boundary — a `#[handler::single]` whose body calls `ctx.reply` must not
//! compile. It is pure friction one call deeper: a helper factored out of a
//! handler to *send* something inherits a type parameter it never reads, and
//! the author discovers that through a mismatched-`Single`/`Manual` error.
//!
//! `ctx.sends()` hands out this view: the same addressing and outbound-mail
//! verbs, none of the reply channel. Helpers take `&mut Sends<'_>` and are
//! callable from every handler class:
//!
//! ```ignore
//! fn announce(sends: &mut Sends<'_>, frame: &Frame) {
//!     sends.actor::<RenderCapability>().send(frame);
//! }
//!
//! #[handler::single]
//! fn on_tick(&mut self, ctx: &mut WasmCtx<'_>, _t: Tick) {
//!     announce(&mut ctx.sends(), &self.frame);
//! }
//! ```
//!
//! **What stays behind.** `reply` / `reply_to` / `emit` are the reply channel
//! itself. `send_with_context` / `send_to_with_context` and `take_context` are
//! its correlation machinery — a stashed context is recovered on the *reply*,
//! so it belongs with the surface that owns replies. Child spawning and the
//! cluster-relative verbs are their own concerns and stay on the full ctx.

use aether_data::{Kind, MailboxId, mailbox_id_from_path};

use super::WasmCtx;
use crate::mail::mailbox::Mailbox;
use crate::model::ctx::mail_sender::MailSender;
use crate::model::ctx::reply_mode::ReplyMode;
use crate::model::{
    Addressable, CallerAddressable, CallerScope, CallerScoped, HandlesKind, Instanced, Resolve, Singleton,
};
use crate::wasm::bridge::mail;
use crate::wasm::inline::{ChainMode, Registry};
use crate::wasm::mailbox::WasmActorMailbox;

/// The reply-mode-free view of a receive ctx's outbound surface.
///
/// Carries exactly what a send needs — the sending actor's own folded
/// [`MailboxId`] raw value (the "from" half every send stamps, issue 1987) and
/// a borrow of the per-component inline registry sends route through — so it
/// resolves addresses and routes mail identically to the [`WasmCtx`] it came
/// from, without naming that ctx's reply class.
///
/// Obtained from [`WasmCtx::sends`]. The `&mut self` borrow of the ctx is held
/// for the view's life, so an actor never holds a send view and the reply
/// channel open at once.
pub struct Sends<'a> {
    mailbox: u64,
    inline: &'a Registry,
}

impl<M: ReplyMode> WasmCtx<'_, M> {
    /// The reply-class-free view of this ctx's outbound surface (see
    /// [`Sends`]). Hand it to a helper that only sends mail, so the helper
    /// stays callable from a `single`, `manual`, and `multi` handler alike
    /// without a `M: ReplyMode` parameter of its own.
    #[must_use]
    pub fn sends(&mut self) -> Sends<'_> {
        Sends { mailbox: self.mailbox, inline: self.inline }
    }
}

impl Sends<'_> {
    /// The sending actor's own mailbox id — the value the substrate uses to
    /// address `receive` calls to it. Mirrors [`WasmCtx::mailbox_id`].
    #[must_use]
    pub fn mailbox_id(&self) -> MailboxId {
        MailboxId(self.mailbox)
    }

    /// Singleton sender shortcut, identical to [`WasmCtx::actor`]: a
    /// ctx-bound [`WasmActorMailbox`] addressing the unique instance of
    /// receiver actor `R`, carrying this actor's id as the send's `from`.
    #[must_use]
    pub fn actor<R: Singleton + CallerAddressable>(&self) -> WasmActorMailbox<'_, R> {
        self.__actor_with_namespace::<R>(R::NAMESPACE)
    }

    /// Namespace-aware typed actor construction, identical to
    /// [`WasmCtx::__actor_with_namespace`]. Not part of the public API — the
    /// cap-owned facades that address by a runtime namespace (the
    /// `aether-component` peer verbs) build on it.
    #[doc(hidden)]
    #[must_use]
    pub fn __actor_with_namespace<R: Singleton + CallerAddressable>(&self, namespace: &str) -> WasmActorMailbox<'_, R> {
        WasmActorMailbox::__new(
            <<R as Addressable>::Resolver as Resolve>::resolve(
                self.scope_mailbox(<<R as Addressable>::Resolver as CallerScoped>::SCOPE),
                namespace,
                (),
            )
            .0,
            self.mailbox,
            self.inline,
        )
    }

    /// Multi-instance sender, identical to [`WasmCtx::resolve_actor`]: resolve
    /// a ctx-bound [`WasmActorMailbox`] from a runtime instance key through
    /// `R`'s caller-scoped resolver.
    #[must_use]
    pub fn resolve_actor<R: Instanced + CallerAddressable>(&self, name: &str) -> WasmActorMailbox<'_, R> {
        WasmActorMailbox::__new(
            R::resolve(self.scope_mailbox(<<R as Addressable>::Resolver as CallerScoped>::SCOPE), name).0,
            self.mailbox,
            self.inline,
        )
    }

    /// Send `payload` through a stored [`Mailbox<K>`] addressing token,
    /// inheriting the handler's causal chain. Identical to [`WasmCtx::send`].
    pub fn send<K: Kind>(&mut self, mailbox: Mailbox<K>, payload: &K) {
        self.route::<K>(mailbox.mailbox(), &payload.encode_into_bytes(), 1, ChainMode::Inherit);
    }

    /// Send `payload` to a raw [`MailboxId`], inheriting the handler's causal
    /// chain. Identical to [`WasmCtx::send_to`].
    pub fn send_to<K: Kind>(&mut self, id: MailboxId, payload: &K) {
        self.route::<K>(id.0, &payload.encode_into_bytes(), 1, ChainMode::Inherit);
    }

    /// The routing seed for `scope`, mirroring `WasmCtx::scope_mailbox`.
    fn scope_mailbox(&self, scope: CallerScope) -> u64 {
        self.inline.scope_mailbox(MailboxId(self.mailbox), scope).0
    }

    /// The one routing call every verb above funnels through: hand the
    /// recipient, kind, and chain mode to the inline registry, stamping this
    /// actor as the sender. A cluster-member recipient dispatches in place;
    /// any other hands off to the host (ADR-0114 addressing amendment).
    fn route<K: Kind>(&self, recipient: u64, bytes: &[u8], count: u32, chain: ChainMode) {
        self.inline.route_or_enqueue(recipient, K::ID.0, bytes, count, chain, self.mailbox);
    }

    /// The typed-recipient routing seed shared by the [`MailSender`] verbs.
    fn resolve_singleton<R: Singleton + CallerAddressable>(&self) -> u64 {
        R::resolve(self.scope_mailbox(<<R as Addressable>::Resolver as CallerScoped>::SCOPE), ()).0
    }
}

// The same routing contract `MailSender for WasmCtx<'_, M>` implements — the
// view resolves recipients through the same resolver scopes and routes through
// the same registry, so a helper handed a `Sends` sends exactly what its caller
// would have sent.
impl MailSender for Sends<'_> {
    fn send<R, K>(&mut self, payload: &K)
    where
        R: Singleton + CallerAddressable + HandlesKind<K>,
        K: Kind,
    {
        self.route::<K>(self.resolve_singleton::<R>(), &payload.encode_into_bytes(), 1, ChainMode::Inherit);
    }

    fn send_many<R, K>(&mut self, payloads: &[K])
    where
        R: Singleton + CallerAddressable + HandlesKind<K>,
        K: Kind + bytemuck::NoUninit,
    {
        let count = payloads.len() as u32;
        self.route::<K>(self.resolve_singleton::<R>(), bytemuck::cast_slice(payloads), count, ChainMode::Inherit);
    }

    // Runtime-name send escape hatch (the `MailSender::send_to_named` contract):
    // the recipient name is supplied at runtime, no compile-time `R` to resolve.
    #[allow(clippy::disallowed_methods)] // aether-suppression-request: ADR-0099 §4 path fold; a lineage address routes
    fn send_to_named<K: Kind>(&mut self, name: &str, payload: &K) {
        self.route::<K>(mailbox_id_from_path(name).0, &payload.encode_into_bytes(), 1, ChainMode::Inherit);
    }

    fn prev_correlation(&self) -> u64 {
        mail::prev_correlation()
    }

    fn send_detached<R, K>(&mut self, payload: &K)
    where
        R: Singleton + CallerAddressable + HandlesKind<K>,
        K: Kind,
    {
        self.route::<K>(self.resolve_singleton::<R>(), &payload.encode_into_bytes(), 1, ChainMode::Detached);
    }

    // Runtime-name detached escape hatch — the `send_to_named` counterpart.
    #[allow(clippy::disallowed_methods)] // aether-suppression-request: ADR-0099 §4 path fold, as send_to_named
    fn send_detached_to_named<K: Kind>(&mut self, name: &str, payload: &K) {
        self.route::<K>(mailbox_id_from_path(name).0, &payload.encode_into_bytes(), 1, ChainMode::Detached);
    }

    fn send_detached_to<K: Kind>(&mut self, id: MailboxId, payload: &K) {
        self.route::<K>(id.0, &payload.encode_into_bytes(), 1, ChainMode::Detached);
    }
}
