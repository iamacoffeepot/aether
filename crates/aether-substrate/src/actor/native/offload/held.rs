//! ADR-0243 typed held replies: a [`DeferredReply`] with its reply kind in
//! its type, and the per-actor table that parks one beside an outbound send.
//!
//! [`NativeCtx::hold`] arms the pair. The handler returns the
//! [`Pending<R>`](super::blocking::Pending) receipt, which sets its row, and
//! keeps the [`Held<R>`] debt in state, in the held table through
//! [`NativeCtx::send_holding`], or on a successor. The debt answers exactly
//! one `R`, from any handler on the same actor.

use std::collections::HashMap;
use std::marker::PhantomData;

use aether_actor::ReplyMode;
use aether_data::{ActorMail, KindId, RequestId};

use super::blocking::{DeferredReply, IntoDeferredReply};
use crate::actor::native::ctx::NativeCtx;

/// A reply of kind `R` this actor still owes its caller (ADR-0243 §1).
///
/// A typed wrapper over [`DeferredReply`]: it keeps the caller's settlement
/// hold and reply target, and its drop fails fast when it is dropped
/// unanswered, through the inner debt's own `Drop`. [`Self::answer`] sends
/// the one terminal `R` and releases the hold, and
/// [`Self::abandon_for_actor_close`] stays the one silent discharge. It
/// implements [`IntoDeferredReply`], so every staging surface that takes a
/// deferred reply takes it unchanged.
#[must_use = "answer the held reply or stage it on a successor; dropping it fails fast"]
pub struct Held<R: ActorMail> {
    reply: DeferredReply,
    /// `fn() -> R` so `Held<R>` is `Send` regardless of `R`: it owns no
    /// `R`, it only names the reply kind.
    _reply: PhantomData<fn() -> R>,
}

impl<R: ActorMail> Held<R> {
    pub(crate) fn new(reply: DeferredReply) -> Self {
        Self { reply, _reply: PhantomData }
    }

    /// The debt with the reply kind it answers, for the held table to key.
    pub(crate) fn into_keyed(self) -> (KindId, DeferredReply) {
        (R::ID, self.reply)
    }

    /// Send `reply` to the waiting caller as the terminal answer, then
    /// release the settlement hold. Consuming, so a held reply answers once.
    pub fn answer<M: ReplyMode, A>(self, ctx: &mut NativeCtx<'_, A, M>, reply: &R) {
        self.reply.reply(ctx, reply);
    }

    /// Release the obligation because the actor that owns it is closing,
    /// with no reply and no panic. The queues and tables that park a held
    /// reply call it from their own teardown.
    #[doc(hidden)]
    pub fn abandon_for_actor_close(self) {
        self.reply.abandon_for_actor_close();
    }
}

impl<R: ActorMail> IntoDeferredReply for Held<R> {
    fn into_deferred_reply(self) -> DeferredReply {
        self.reply
    }
}

/// Per-actor held replies keyed by the correlation of the outbound request
/// whose reply answers them (ADR-0243 §4).
///
/// Each entry records the reply kind its [`Held`] was typed with, so a take
/// of another kind leaves it stored, as the ADR-0139 request-context table
/// does. The table never evicts: an entry leaves by a matching take, or by
/// [`Self::abandon_all`] when the actor closes, and the `Drop` backstop
/// abandons whatever a close tail left.
pub(crate) struct HeldTable {
    entries: HashMap<RequestId, (KindId, DeferredReply)>,
}

impl HeldTable {
    pub(crate) fn new() -> Self {
        Self { entries: HashMap::new() }
    }

    /// Park `reply`, typed as `kind`, under `request`. A correlation is
    /// minted once per send, so the key is always new; a displaced entry
    /// would be a lost reply, and its drop fails fast.
    pub(crate) fn insert(&mut self, request: RequestId, kind: KindId, reply: DeferredReply) {
        let previous = self.entries.insert(request, (kind, reply));
        debug_assert!(previous.is_none(), "a held reply's correlation is minted fresh per send");
    }

    /// Remove the reply parked under `request` when it was typed as `kind`.
    /// A wrong kind leaves the entry stored.
    pub(crate) fn take(&mut self, request: RequestId, kind: KindId) -> Option<DeferredReply> {
        if self.entries.get(&request)?.0 != kind {
            return None;
        }
        self.entries.remove(&request).map(|(_, reply)| reply)
    }

    /// Abandon every parked reply for actor close: each hold releases with
    /// no reply and no panic.
    pub(crate) fn abandon_all(&mut self) {
        for (_, (_, reply)) in self.entries.drain() {
            reply.abandon_for_actor_close();
        }
    }
}

impl Drop for HeldTable {
    fn drop(&mut self) {
        self.abandon_all();
    }
}
