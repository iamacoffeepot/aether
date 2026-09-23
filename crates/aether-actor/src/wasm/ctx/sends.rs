//! [`Sends`] — the receive ctx's outbound surface with the reply-class
//! marker dropped, so a helper that only sends mail needs no `M: ReplyMode`
//! parameter.
//!
//! [`WasmCtx<'_, A, M>`](WasmCtx) is generic over its reply class (ADR-0112,
//! ADR-0134) because the marker selects which reply surface the handler is
//! allowed to reach: `reply` / `reply_to` exist only on `Manual`, not on
//! `Single`. That is load-bearing at the handler
//! boundary — a `#[handler::single]` whose body calls `ctx.reply` must not
//! compile. It is pure friction one call deeper: a helper factored out of a
//! handler to *send* something inherits a type parameter it never reads, and
//! the author discovers that through a mismatched-`Single`/`Manual` error.
//!
//! `ctx.sends()` hands out this view: the same addressing and outbound-mail
//! verbs, none of the reply channel. The view keeps the actor of the ctx it
//! came from, so helpers take `&mut Sends<'_, A>` and are callable from every
//! handler class. A helper that reaches an actor through the view takes
//! `A: Reaches<R>`, the bound `ctx.actor::<R>()` carries:
//!
//! ```ignore
//! fn announce<A: Reaches<RenderCapability>>(sends: &mut Sends<'_, A>, frame: &Frame) {
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
//! itself. `send_with_context` and `take_context` are its correlation
//! machinery — a stashed context is recovered on the *reply*, so it belongs
//! with the surface that owns replies. Child spawning and the
//! cluster-relative verbs are their own concerns and stay on the full ctx.

use core::marker::PhantomData;

use aether_data::{Kind, MailboxId};

use super::WasmCtx;
use crate::model::ctx::Erased;
use crate::model::ctx::mail_sender::MailSender;
use crate::model::ctx::reply_mode::ReplyMode;
use crate::model::{Addressable, CallerAddressable, CallerScope, CallerScoped, HandlesKind, Reaches, Singleton};
use crate::reference::{ActorRef, ErasedActorRef, Target};
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
///
/// The view keeps the actor `A` of the ctx it came from, so it reaches exactly
/// what that ctx reaches. `Sends<'_>` alone names the erased view.
pub struct Sends<'a, A = Erased> {
    mailbox: u64,
    inline: &'a Registry,
    /// `fn() -> A`, the same marker [`WasmCtx`] carries: the view owns no
    /// actor state, so it inherits none of `A`'s auto-traits or drop glue.
    _actor: PhantomData<fn() -> A>,
}

impl<A, M: ReplyMode> WasmCtx<'_, A, M> {
    /// The reply-class-free view of this ctx's outbound surface (see
    /// [`Sends`]). Hand it to a helper that only sends mail, so the helper
    /// stays callable from a `single` and a `manual` handler alike
    /// without a `M: ReplyMode` parameter of its own.
    #[must_use]
    pub fn sends(&mut self) -> Sends<'_, A> {
        Sends { mailbox: self.mailbox, inline: self.inline, _actor: PhantomData }
    }
}

impl<A> Sends<'_, A> {
    /// Singleton sender shortcut, identical to [`WasmCtx::actor`]: a
    /// ctx-bound [`WasmActorMailbox`] addressing the unique instance of
    /// receiver actor `R`, carrying this actor's id as the send's `from`.
    ///
    /// Bounded `A: Reaches<R>` exactly like [`WasmCtx::actor`]: the erased
    /// view reaches every singleton, and a view typed by its actor reaches
    /// only that actor's declared dependencies. A typed ctx's view calling
    /// `actor::<R>()` for an `R` its actor has not declared does not
    /// type-check:
    ///
    /// ```compile_fail,E0277
    /// use aether_actor::{Addressable, One, WasmCtx};
    ///
    /// struct Undeclared;
    ///
    /// impl Addressable for Undeclared {
    ///     const NAMESPACE: &'static str = "example.undeclared";
    ///     type Resolver = One;
    /// }
    ///
    /// struct Lonely;
    ///
    /// impl Addressable for Lonely {
    ///     const NAMESPACE: &'static str = "example.lonely";
    ///     type Resolver = One;
    /// }
    ///
    /// fn missing_dependency(ctx: &mut WasmCtx<'_, Lonely>) {
    ///     let _ = ctx.sends().actor::<Undeclared>();
    /// }
    /// ```
    #[must_use]
    pub fn actor<R: Singleton + CallerAddressable>(&self) -> WasmActorMailbox<'_, R>
    where
        A: Reaches<R>,
    {
        WasmActorMailbox::new(self.resolve_singleton::<R>(), self.mailbox, self.inline)
    }

    /// Send through a proven [`ActorRef`], identical to [`WasmCtx::to`]: a
    /// helper handed a `Sends` sends through the same reference its caller
    /// would have sent through.
    #[must_use]
    pub fn to<R: Addressable>(&self, target: &ActorRef<R>) -> WasmActorMailbox<'_, R> {
        WasmActorMailbox::new(target.id().0, self.mailbox, self.inline)
    }

    /// Send `payload` through a held reference, inheriting the handler's
    /// causal chain. Identical to [`WasmCtx::send_to`]: an [`ActorRef<R>`] is
    /// kind-checked against `K` and an [`ErasedActorRef`] is not.
    pub fn send_to<K: Kind>(&mut self, target: impl Target<K>, payload: &K) {
        self.route::<K>(target.erased().id().0, &payload.encode_into_bytes(), 1, ChainMode::Inherit);
    }

    /// The routing seed for `scope`, mirroring `WasmCtx::scope_mailbox`.
    fn scope_mailbox(&self, scope: CallerScope) -> u64 {
        self.inline.scope_mailbox(MailboxId(self.mailbox), scope)
    }

    /// The one routing call every verb above funnels through: hand the
    /// recipient, kind, and chain mode to the inline registry, stamping this
    /// actor as the sender. A cluster-member recipient dispatches in place;
    /// any other hands off to the host (ADR-0114 addressing amendment).
    fn route<K: Kind>(&self, recipient: u64, bytes: &[u8], count: u32, chain: ChainMode) {
        self.inline.route_or_enqueue(recipient, K::ID.0, bytes, count, chain, self.mailbox);
    }

    /// The typed-recipient routing seed shared by [`Self::actor`] and the
    /// [`MailSender`] verbs.
    fn resolve_singleton<R: Singleton + CallerAddressable>(&self) -> u64 {
        R::resolve(self.scope_mailbox(<<R as Addressable>::Resolver as CallerScoped>::SCOPE), ()).0
    }
}

// The same routing contract `MailSender for WasmCtx<'_, A, M>` implements — the
// view resolves recipients through the same resolver scopes and routes through
// the same registry, so a helper handed a `Sends` sends exactly what its caller
// would have sent.
impl<A> MailSender for Sends<'_, A> {
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

    fn send_detached_to<K: Kind>(&mut self, target: ErasedActorRef, payload: &K) {
        self.route::<K>(target.id().0, &payload.encode_into_bytes(), 1, ChainMode::Detached);
    }
}
