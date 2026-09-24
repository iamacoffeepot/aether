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
//! `ctx.sends()` hands out this view: the ctx's by-proof outbound-mail verbs,
//! none of the reply channel. The view keeps the actor of the ctx it
//! came from, so helpers take `&mut Sends<'_, A>` and are callable from every
//! handler class. A helper sends through a proof it is handed:
//!
//! ```ignore
//! fn announce<A>(sends: &mut Sends<'_, A>, focused: ErasedActorRef) {
//!     sends.send_to(focused, &FocusGained { keyboard: true });
//! }
//!
//! #[handler::single]
//! fn on_click(&mut self, ctx: &mut WasmCtx<'_>, _c: Click) {
//!     announce(&mut ctx.sends(), self.focused);
//! }
//! ```
//!
//! **What stays behind.** `reply` / `reply_to` / `emit` are the reply channel
//! itself. `send_with_context` and `take_context` are its correlation
//! machinery — a stashed context is recovered on the *reply*, so it belongs
//! with the surface that owns replies. The flat typed sends to a declared
//! dependency, child spawning, and the cluster-relative verbs stay on the full
//! ctx too.

use core::marker::PhantomData;

use aether_data::ActorMail;

use super::WasmCtx;
use crate::model::ctx::Erased;
use crate::model::ctx::mail_sender::MailSender;
use crate::model::ctx::reply_mode::ReplyMode;
use crate::reference::{ErasedActorRef, Target};
use crate::wasm::bridge::mail;
use crate::wasm::inline::{ChainMode, Registry};

/// The reply-mode-free view of a receive ctx's outbound surface.
///
/// Carries exactly what a send needs — the sending actor's own folded id
/// (the "from" half every send stamps, issue 1987) and a borrow of the per-component inline registry sends route through — so it
/// routes mail identically to the [`WasmCtx`] it came from, without naming
/// that ctx's reply class.
///
/// Obtained from [`WasmCtx::sends`]. The `&mut self` borrow of the ctx is held
/// for the view's life, so an actor never holds a send view and the reply
/// channel open at once.
///
/// The view keeps the actor `A` of the ctx it came from; it sends only through
/// a proof it is handed. `Sends<'_>` alone names the erased view.
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
    /// Send `payload` through a held reference, inheriting the handler's
    /// causal chain. Identical to [`WasmCtx::send_to`]: an [`ActorRef<R>`](crate::ActorRef) is
    /// kind-checked against `K` and an [`ErasedActorRef`] is not.
    pub fn send_to<K: ActorMail>(&mut self, target: impl Target<K>, payload: &K) {
        self.route::<K>(target.erased().id().0, &payload.encode_into_bytes(), 1, ChainMode::Inherit);
    }

    /// The one routing call every [`Sends`] verb funnels through: hand the
    /// recipient, kind, and chain mode to the inline registry, stamping this
    /// actor as the sender. A cluster-member recipient dispatches in place;
    /// any other hands off to the host (ADR-0114 addressing amendment).
    fn route<K: ActorMail>(&self, recipient: u64, bytes: &[u8], count: u32, chain: ChainMode) {
        self.inline.route_or_enqueue(recipient, K::ID.0, bytes, count, chain, self.mailbox);
    }
}

// The same routing contract `MailSender for WasmCtx<'_, A, M>` implements — the
// view routes through the same registry, so a helper handed a `Sends` sends
// exactly what its caller would have sent.
impl<A> MailSender for Sends<'_, A> {
    fn prev_correlation(&self) -> u64 {
        mail::prev_correlation()
    }

    fn send_detached_to<K: ActorMail>(&mut self, target: ErasedActorRef, payload: &K) {
        self.route::<K>(target.id().0, &payload.encode_into_bytes(), 1, ChainMode::Detached);
    }
}
