//! The cast a dispatch arm runs before a handler that takes
//! `sender: ProtocolRef<P>` (ADR-0231 §11): the guest twin of the native
//! ctx's `__sender_or_refuse` and `__sender_or_answer`.
//!
//! Hidden macro plumbing, like `Mail::__decode_kind_or_refused`: no handler
//! calls these, and no ctx verb is added. The `#[actor]` arm calls one before
//! the handler and passes the proven reference as its fourth argument. A
//! sender the cast refuses never reaches the handler: the helper logs the
//! refusal in the actor's own log, answers a request, and hands the arm the
//! code it returns.

use aether_data::{ActorMail, Kind};

use super::WasmCtx;
use crate::model::CastTarget;
use crate::model::ctx::outbound_reply::OutboundReply;
use crate::model::ctx::reply_mode::{ReplyMode, Unchecked};
use crate::reference::ProtocolRef;
use crate::sender_refused::SenderRefused;
use crate::wasm::bridge::address;
use crate::{DISPATCH_HANDLED_RELEASE, DISPATCH_REFUSED_SENDER, PathRefused};

impl<A, M: ReplyMode> WasmCtx<'_, A, M> {
    /// The mail's sender as the protocol `P` its handler requires, by the
    /// guard cast [`Self::cast`] runs.
    fn prove_sender<P: CastTarget>(&self) -> Option<ProtocolRef<P>> {
        self.sender().and_then(|sender| self.cast::<P>(sender))
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

    /// Prove a tell's sender as `P`, or refuse the mail.
    ///
    /// # Errors
    ///
    /// [`DISPATCH_REFUSED_SENDER`], the code the arm returns, when the sender
    /// does not cast to `P`. The refusal is logged and the handler does not
    /// run.
    #[doc(hidden)]
    pub fn __sender_or_refuse<P: CastTarget, K: Kind>(&self) -> Result<ProtocolRef<P>, u32> {
        if let Some(proven) = self.prove_sender::<P>() {
            return Ok(proven);
        }

        let _ = self.refused_sender::<P, K>();
        Err(DISPATCH_REFUSED_SENDER)
    }
}

impl<A> WasmCtx<'_, A, Unchecked> {
    /// Prove a request's sender as `P`, or answer the request with its
    /// reply's `From<PathRefused>` naming the sender. A `Pending<O>` row is
    /// answered the same way, at once.
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
    ) -> Result<ProtocolRef<P>, u32> {
        if let Some(proven) = self.prove_sender::<P>() {
            return Ok(proven);
        }

        let Some(refused) = self.refused_sender::<P, K>().path_refused() else {
            return Err(DISPATCH_REFUSED_SENDER);
        };
        OutboundReply::reply(self, &O::from(refused));
        Err(DISPATCH_HANDLED_RELEASE)
    }
}
