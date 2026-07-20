use std::sync::Arc;

use crate::mail::registry::dispatch::{MailDispatch, OwnedDispatch};

/// No-op [`InboxHandler`] for tests that just need a registered
/// mailbox to route to *somewhere* without observing the mail. The
/// explicit named helper documents intent at the call site.
///
/// Defaults to the Inbox variant because every current caller pairs
/// it with `register_inbox` / `try_register_inbox`. Tests that need
/// the Inline variant (e.g. asserting bracket recording paths)
/// build their own `Arc::new(|_d: MailDispatch<'_>| {}) as
/// Arc<dyn InlineHandler>`.
#[must_use]
pub fn noop_handler() -> Arc<dyn InboxHandler> {
    Arc::new(|dispatch: OwnedDispatch| {
        // ADR-0094: this handler intentionally discards the dispatch
        // without a downstream consumer, so mark the obligation
        // transferred (discarded-at-the-seam) rather than letting the
        // debug guard fire when a test routes a real mail here.
        dispatch.mark_transferred();
    })
}

/// Synchronous handler installed under
/// [`MailboxEntry::Inline`](crate::mail::registry::MailboxEntry::Inline). Runs
/// on the mailer thread inside `Mailer::push`; the mailer brackets
/// the call with `record_received` / `record_finished` so the
/// chain's `in_flight` balances (ADR-0080 §2). The borrowed
/// [`MailDispatch<'_>`] argument is zero-copy — the handler may read
/// `payload` directly without owning it, which is the right shape
/// for "do the work right here and return" bodies. Bodies that need
/// to enqueue the payload across a channel should pick
/// [`InboxHandler`] instead so the bytes move rather than copy.
///
/// **Wrong-variant symptom.** An actor-enqueue closure (one that
/// forwards `dispatch` into an mpsc the dispatcher thread drains)
/// installed here double-counts `Finished`: the mailer brackets the
/// enqueue, then the dispatcher records its own bracket when the
/// envelope is picked up. Settlement subscribers wake on the first
/// `Finished` — before the actual work runs — and the chain reports
/// settled prematurely (the inverse of the iamacoffeepot/aether#846
/// failure). Pick [`InboxHandler`] for those bodies; the dispatch
/// type asymmetry (`MailDispatch<'_>` vs `OwnedDispatch`) is a
/// structural nudge but not a hard guarantee.
///
/// Blanket impl below covers any `Fn(MailDispatch<'_>)` closure;
/// hand-rolled `impl InlineHandler for MyType` is also supported
/// for handlers that hold state.
pub trait InlineHandler: Send + Sync + 'static {
    fn dispatch(&self, dispatch: MailDispatch<'_>);
}

/// Actor-enqueue handler installed under
/// [`MailboxEntry::Inbox`](crate::mail::registry::MailboxEntry::Inbox). The
/// handler is expected to move `dispatch` onto a downstream channel
/// (typically a cap-local mpsc); the downstream consumer — an actor
/// dispatcher or chassis-side recv loop — records
/// `Received`/`Finished` per envelope. **Contract:** every
/// [`OwnedDispatch`] you receive must eventually have `Finished`
/// recorded for its `mail_id` — otherwise the chain's `in_flight`
/// leaks and any settlement subscriber hangs. iamacoffeepot/aether#846
/// is the canonical incident: a synchronous closure that captured
/// fields off the dispatch but had no downstream owner of the
/// bracket caused [`SubstrateHarness::send_bytes`] to time out at 5s once
/// strict settlement propagation landed.
///
/// **ADR-0094 obligation guard.** The type-shape split above is the
/// first line of defence (a "structural nudge but not a hard
/// guarantee"); ADR-0094 adds a *debug-build* runtime check that names
/// the leaking seam instead of hanging anonymously. Every
/// [`OwnedDispatch`] is minted *armed* (debug builds) and its `Drop`
/// panics — reporting `mail_id` + `kind_name` + mailbox — unless the
/// consumer explicitly disarms it via exactly one of:
/// - [`OwnedDispatch::discharge`] — "the obligation ends here": call it
///   adjacent to every `Mailer::record_finished(mail_id, root)` for a
///   consumed envelope (e.g. `dispatcher_slot::dispatch_one`, the wasm
///   trampoline drain via that same dispatcher, the desktop window
///   drain). The two must sit together so they cannot drift.
/// - [`OwnedDispatch::mark_transferred`] — "the obligation moves
///   onward": call it on relay / park / fan-out / discard-at-the-seam
///   paths where the obligation rides onto a freshly-built downstream
///   envelope (which arms its own guard) or is intentionally discarded.
///
/// Release builds compile the guard out entirely (no field, no `Drop`),
/// so it is zero-cost. Test/helper mints use the disarmed constructor.
///
/// **ADR-0106: prefer the framework drain.** A capability that claims a
/// mailbox via
/// [`ChassisCtx::claim_mailbox`](crate::chassis::ctx::ChassisCtx::claim_mailbox)
/// no longer hand-rolls this bracket: the claim carries a
/// [`SettlingInbox`](crate::chassis::inbox::SettlingInbox) whose drain
/// methods yield each mail as an
/// [`InboundMail`](crate::chassis::inbox::InboundMail) guard that records
/// `Finished` + disarms on scope exit, so every arm settles by
/// construction. Hand-rolling an `impl InboxHandler` and pairing
/// `record_finished` with `discharge` per arm is the move-onward relay
/// shape (the three production closures route through `relay_or_transfer`)
/// — reach for it only when forwarding the dispatch onward, not when
/// consuming it.
///
/// The owned dispatch type is the structural hint: payload arrives
/// as `Vec<u8>`, so moving it into an mpsc `Sender` is a single
/// move, not a clone. A handler that does immediate synchronous
/// work against the dispatch wastes the move and skips the
/// bracket entirely — those bodies belong on [`InlineHandler`]
/// instead.
///
/// Blanket impl below covers any `Fn(OwnedDispatch)` closure;
/// hand-rolled `impl InboxHandler for MyType` is supported for caps
/// that want to bundle the channel sender with handler state.
///
/// [`SubstrateHarness::send_bytes`]: ../../../aether_substrate_bundle/substrate_harness/struct.SubstrateHarness.html#method.send_bytes
pub trait InboxHandler: Send + Sync + 'static {
    fn enqueue(&self, dispatch: OwnedDispatch);
}

impl<F> InlineHandler for F
where
    F: for<'a> Fn(MailDispatch<'a>) + Send + Sync + 'static,
{
    #[inline]
    fn dispatch(&self, dispatch: MailDispatch<'_>) {
        self(dispatch);
    }
}

impl<F> InboxHandler for F
where
    F: Fn(OwnedDispatch) + Send + Sync + 'static,
{
    #[inline]
    fn enqueue(&self, dispatch: OwnedDispatch) {
        self(dispatch);
    }
}
