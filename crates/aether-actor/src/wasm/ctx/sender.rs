//! The cast a dispatch arm runs before a handler whose ctx names a protocol
//! as its sender (ADR-0231 §11): the guest twin of the native ctx's
//! `__sender_or_refuse` and `__sender_or_answer`.
//!
//! Hidden macro plumbing, like `Mail::__decode_kind_or_refused`: no handler
//! calls these, and no ctx verb is added. The `#[actor]` arm calls one before
//! the handler and calls the handler with the ctx it returns, typed by the
//! protocol, whose `sender()` is the proven reference. A sender the cast
//! refuses never reaches the handler: the helper logs the refusal in the
//! actor's own log, answers a request, and hands the arm the code it returns.

use core::ptr;

use aether_data::{ActorMail, Kind};

use super::WasmCtx;
use crate::model::ctx::outbound_reply::OutboundReply;
use crate::model::ctx::reply_mode::{ReplyMode, Unchecked};
use crate::model::{Anyone, CastTarget};
use crate::sender_refused::SenderRefused;
use crate::wasm::bridge::address;
use crate::{DISPATCH_HANDLED_RELEASE, DISPATCH_REFUSED_SENDER, PathRefused};

impl<'a, A, M: ReplyMode> WasmCtx<'a, A, Anyone, M> {
    /// This ctx typed by the protocol `P` its handler requires of its
    /// sender, once the guard cast [`Self::cast`] runs proves the dispatch's
    /// sender as `P`; the ctx itself, unchanged, when the cast refuses or the
    /// dispatch has no sender.
    ///
    /// It is the only code that produces a ctx typed by a protocol, which is
    /// what lets `sender()` on that ctx mint from the stamp with no second
    /// cast: the stamp is fixed at construction, so the reference minted
    /// there names the position this cast proved.
    fn prove_sender<P: CastTarget>(&mut self) -> Result<&mut WasmCtx<'a, A, P, M>, &mut Self> {
        let proven = self.sender().and_then(|sender| self.cast::<P>(sender));
        if proven.is_none() {
            return Err(self);
        }

        // SAFETY: `S` appears only in `PhantomData`, so `WasmCtx<'a, A, Anyone, M>`
        // and `WasmCtx<'a, A, P, M>` are layout-identical for every `P` (see
        // `ffi_ctx_layout_identical_across_modes`). The reborrow swaps the
        // marker without touching any real field.
        Ok(unsafe { &mut *ptr::from_mut(self).cast::<WasmCtx<'a, A, P, M>>() })
    }

    /// Why the sender of a `K` did not cast to `P`, logged once as an error
    /// in this actor's log. Two host calls read the sender's path and the
    /// rows it published; they are paid only here, after the cast refused.
    #[cold]
    fn refused_sender<P: CastTarget, K: Kind>(&self) -> SenderRefused {
        let position = self.sender().map(|sender| sender.id().0);
        let sender = position.and_then(address::actor_path);
        let rows = position.and_then(|position| address::published_rows(position).rows);
        let refused = SenderRefused::of::<P>(sender, rows.as_deref());

        tracing::error!(kind = K::NAME, refusal = %refused, "handler did not run: its sender is refused");
        refused
    }

    /// Prove a tell's sender as `P` and type the ctx by it, or refuse the
    /// mail.
    ///
    /// # Errors
    ///
    /// [`DISPATCH_REFUSED_SENDER`], the code the arm returns, when the sender
    /// does not cast to `P`. The refusal is logged and the handler does not
    /// run.
    #[doc(hidden)]
    pub fn __sender_or_refuse<P: CastTarget, K: Kind>(&mut self) -> Result<&mut WasmCtx<'a, A, P, M>, u32> {
        let unproven = match self.prove_sender::<P>() {
            Ok(proven) => return Ok(proven),
            Err(unproven) => unproven,
        };

        let _ = unproven.refused_sender::<P, K>();
        Err(DISPATCH_REFUSED_SENDER)
    }
}

impl<'a, A> WasmCtx<'a, A, Anyone, Unchecked> {
    /// Prove a request's sender as `P` and type the ctx by it, or answer the
    /// request with its reply's `From<PathRefused>` naming the sender. A
    /// `Pending<O>` row is answered the same way, at once.
    ///
    /// # Errors
    ///
    /// The code the arm returns when the sender does not cast to `P`:
    /// [`DISPATCH_HANDLED_RELEASE`] once the reply is sent, or
    /// [`DISPATCH_REFUSED_SENDER`] for mail with no sender, which has no path
    /// to name in a reply. The refusal is logged and the handler does not
    /// run.
    #[doc(hidden)]
    pub fn __sender_or_answer<P: CastTarget, K: Kind, O: ActorMail + From<PathRefused>>(
        &mut self,
    ) -> Result<&mut WasmCtx<'a, A, P, Unchecked>, u32> {
        let unproven = match self.prove_sender::<P>() {
            Ok(proven) => return Ok(proven),
            Err(unproven) => unproven,
        };

        let Some(refused) = unproven.refused_sender::<P, K>().path_refused() else {
            return Err(DISPATCH_REFUSED_SENDER);
        };
        OutboundReply::reply(unproven, &O::from(refused));
        Err(DISPATCH_HANDLED_RELEASE)
    }
}
