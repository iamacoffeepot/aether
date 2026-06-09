//! ADR-0093 hold-until-resolve dispatch primitive (runtime half).
//!
//! The third spawn shape (alongside `spawn_inherit` and
//! `spawn_detached`, see [`super::spawn_thread`]): *work that replies in a
//! later handler turn*. A handler kicks off a slow blocking call, the
//! worker pushes its result and dies, and the real reply is sent from a
//! *subsequent* handler invocation when that result lands. The
//! settlement hold must outlive the worker — it spans accept → the later
//! re-reply — so neither `spawn_inherit` (hold dies with the worker) nor
//! `spawn_detached` (no hold) fits.
//!
//! This generalises the content-gen `InFlightDispatch` prototype
//! (`aether_capabilities::contentgen::dispatch`) into a first-class
//! ctx primitive. The pieces:
//!
//! - [`DispatchId`] — a `Copy` correlation token minted per dispatch.
//! - [`TaskDone`] — a move-only completion that carries the worker's
//!   output, the originating [`Source`], the held [`SettlementHold`],
//!   and an opt-in context `C`. Its consuming [`TaskDone::resolve`]
//!   re-replies through the carried reply target **first**, then drops
//!   the hold (`Sent` before `Release`, ADR-0080 §12). Dropping a
//!   `TaskDone` without resolving releases the hold and `debug_assert`s
//!   (a silent lost reply).
//! - the in-flight ledger (`InflightTable`) — a per-actor map from
//!   `DispatchId` to its held `(hold, reply_to, context)` plus a
//!   completion output slot the worker fills. Lives behind a `&self`
//!   interior-mutability `Mutex` on [`NativeBinding`](super::binding),
//!   like `outbound` / `blob_producer`; the single logical writer is the
//!   actor's own dispatch thread.
//! - [`TaskCompletionWake`] — a substrate-internal framework kind the
//!   worker pushes (carrying just the `DispatchId`) to the actor's own
//!   mailbox, the same loopback-wake mechanism `InFlightDispatch`'s
//!   worker uses to wake the actor.
//!
//! The request side and completion routing live on
//! [`NativeCtx`](super::ctx): `dispatch_blocking` /
//! `dispatch_blocking_with` spawn the worker, and `take_task_done`
//! reunites the worker's output with the held `(hold, reply_to,
//! context)` when the completion-wake mail lands. The
//! `#[handler(task)]` macro sugar that hand-wires the completion handler
//! is a separate later PR; for now a handler matches
//! [`TaskCompletionWake`] explicitly and calls `take_task_done` itself.

use std::any::Any;
use std::collections::HashMap;
use std::sync::Mutex;

use aether_data::{Kind, KindId};

use crate::mail::Source;
use crate::runtime::trace::SettlementHold;

use super::ctx::NativeCtx;

/// A `Copy` correlation token minted monotonically per
/// [`dispatch_blocking`](NativeCtx::dispatch_blocking). Names one
/// in-flight dispatch in the `InflightTable`; rides the
/// [`TaskCompletionWake`] mail so the completion routes back to the
/// right ledger entry. Returned to the call site for *optional*
/// cancellation — the happy path ignores it.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DispatchId(pub u64);

/// Substrate-internal framework kind the dispatch worker pushes to the
/// actor's own mailbox when its blocking closure finishes. Carries only
/// the [`DispatchId`] — the worker's output rides the ledger entry's
/// completion slot, not the wire — so a non-serializable `O` never has
/// to encode. The actor's completion handler decodes this, then calls
/// [`NativeCtx::take_task_done`] to reunite output + held state.
///
/// Hand-rolled `Kind` (the cast-shape path): a `#[repr(C)]` `u64` body
/// that casts to / from bytes. Substrate-internal, so it is not derived
/// (no inventory submission, no `describe_kinds` surface) — it never
/// crosses the wire to a guest or the hub.
#[repr(C)]
#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct TaskCompletionWake {
    /// The [`DispatchId`] of the dispatch whose worker just finished.
    pub dispatch_id: u64,
}

impl Kind for TaskCompletionWake {
    const NAME: &'static str = "aether.dispatch.task_completion_wake";
    // Minted the same way `#[derive(Kind)]` mints a tagged kind id, so
    // the id is stable and tag-checks like any other kind on the wire
    // path the worker pushes through.
    const ID: KindId = KindId(aether_data::with_tag(
        aether_data::Tag::Kind,
        aether_data::fnv1a_64_prefixed(aether_data::KIND_DOMAIN, Self::NAME.as_bytes()),
    ));

    aether_data::pod_kind_codec!();
}

/// One in-flight dispatch's held state, parked in the [`InflightTable`]
/// from the dispatching handler's return until its completion lands.
///
/// The actor thread writes the entry at dispatch time (the hold, reply
/// target, and context, with the output empty); the worker fills
/// `output` under the table mutex when its closure returns and pushes the
/// [`TaskCompletionWake`]; the actor reads + removes the entry when that
/// wake lands ([`NativeCtx::take_task_done`]).
struct InflightEntry {
    /// The [`SettlementHold`] acquired eagerly in the dispatching
    /// handler (before it returned), keeping the chain root open across
    /// the async worker. Released only after the re-reply, via
    /// [`TaskDone::resolve`].
    hold: SettlementHold,
    /// The originating caller's reply target, captured at dispatch. The
    /// re-reply routes through this.
    reply_to: Source,
    /// The opt-in completion context (`()` for the bare
    /// [`dispatch_blocking`](NativeCtx::dispatch_blocking)). Boxed so
    /// heterogeneous `C`s share one table type; downcast in
    /// `take_task_done`.
    context: Box<dyn Any + Send>,
    /// The worker's output, filled under the table mutex when the
    /// closure returns and taken in `take_task_done`. Boxed for the same
    /// heterogeneity reason; `None` until the worker finishes.
    output: Option<Box<dyn Any + Send>>,
}

/// Per-actor in-flight ledger for hold-until-resolve dispatch (ADR-0093
/// §2). Maps a [`DispatchId`] to its held `(hold, reply_to, context)`
/// plus the worker's eventual output. Opaque framework plumbing — it
/// holds none of the cap's *business* state, only the primitive's own
/// bookkeeping, so centralising it here doesn't violate the
/// plain-actor-state rule (ADR-0038).
///
/// The `Mutex` is for `&self` interior mutability + `Sync` only, like
/// [`NativeBinding`](super::binding)'s `outbound` / `blob_producer`: the
/// actor's own dispatch thread is the single logical writer of the
/// `(hold, reply_to, context)` slot and the reader of the output, while
/// the worker thread fills the output slot once. Contention is the brief
/// worker-fill / actor-read overlap, not a steady-state hot path.
pub(crate) struct InflightTable {
    next_id: u64,
    entries: HashMap<DispatchId, InflightEntry>,
}

impl InflightTable {
    pub(crate) fn new() -> Self {
        Self {
            next_id: 0,
            entries: HashMap::new(),
        }
    }

    /// Mint the next monotonic [`DispatchId`]. Called on the actor
    /// thread under the table lock, so the bump is uncontended.
    fn mint_id(&mut self) -> DispatchId {
        self.next_id += 1;
        DispatchId(self.next_id)
    }
}

/// A move-only dispatch completion (ADR-0093 §3-§4). Carries the
/// worker's `output`, the originating [`Source`], the held
/// [`SettlementHold`], and an opt-in context `C` (unit by default).
///
/// Move-only by construction — no `Clone` / `Copy` — so the held state
/// can't be duplicated and the hold's release can't be issued twice. The
/// consuming `resolve` family re-replies **first**, then drops the hold,
/// making the `Sent`-before-`Release` ordering (ADR-0080 §12) structural
/// rather than a remembered drop order. Dropping a `TaskDone` without
/// resolving releases the hold and `debug_assert`s — catching the silent
/// lost reply that discipline misses today.
#[must_use = "a TaskDone holds the chain open; resolve it (or resolve_err) to send the deferred reply and release the hold"]
pub struct TaskDone<O, C = ()> {
    output: O,
    context: C,
    /// `Option` so `resolve` can `take` the hold out and drop it
    /// *after* the reply is sent, leaving `Drop` nothing to release. A
    /// resolved `TaskDone` carries `None` here; an un-resolved one
    /// carries `Some` and `Drop` trips the assertion.
    hold: Option<SettlementHold>,
    reply_to: Source,
    /// Set true by every `resolve*` path before it consumes `self`, so
    /// `Drop` can tell a resolved completion (clean) from a dropped-
    /// without-resolve one (the lost-reply bug).
    resolved: bool,
}

impl<O, C> TaskDone<O, C> {
    /// Borrow the worker's output. The common `resolve` re-replies this
    /// directly; `resolve_with` maps it.
    pub fn output(&self) -> &O {
        &self.output
    }

    /// Borrow the opt-in completion context (`()` for the bare
    /// [`dispatch_blocking`](NativeCtx::dispatch_blocking)).
    pub fn context(&self) -> &C {
        &self.context
    }

    /// Mark resolved and drop the hold **after** the caller has sent the
    /// reply. Shared tail of every `resolve*` path: take the hold out so
    /// `Drop` sees `None` (no double release, no assertion), then let it
    /// fall out of scope here — strictly after the reply the caller
    /// already pushed, so `Sent` precedes `Release`.
    fn release(&mut self) {
        self.resolved = true;
        drop(self.hold.take());
    }

    /// Re-reply the carried `output` through the carried `reply_to`,
    /// then release the hold (ADR-0093 §4). The worker already shaped
    /// `output` into the reply value, so this is the common one-liner.
    pub fn resolve(mut self, ctx: &mut NativeCtx<'_>)
    where
        O: Kind + serde::Serialize,
    {
        ctx.reply_to_target(self.reply_to, &self.output);
        self.release();
    }

    /// Map `(&output, &context)` to a reply value via `f`, send it
    /// through the carried `reply_to`, then release the hold. For
    /// completion handlers that shape a different reply from the carried
    /// output (and context, when present) than the raw `output`.
    pub fn resolve_with<R, F>(mut self, ctx: &mut NativeCtx<'_>, f: F)
    where
        R: Kind + serde::Serialize,
        F: FnOnce(&O, &C) -> R,
    {
        let reply = f(&self.output, &self.context);
        ctx.reply_to_target(self.reply_to, &reply);
        self.release();
    }

    /// Send an error reply (a provider-failure shape the cap builds)
    /// through the carried `reply_to`, then release the hold. The
    /// carried `output` is discarded — used when the completion is a
    /// failure rather than a result.
    pub fn resolve_err<E>(mut self, ctx: &mut NativeCtx<'_>, err: &E)
    where
        E: Kind + serde::Serialize,
    {
        ctx.reply_to_target(self.reply_to, err);
        self.release();
    }
}

impl<O, C> Drop for TaskDone<O, C> {
    /// A `TaskDone` dropped without a `resolve*` call is a silent lost
    /// reply: the caller was owed a deferred reply that never went out.
    /// Release the hold so the chain can still settle (a stuck hold
    /// would wedge settlement forever), then `debug_assert` so the bug
    /// is loud in debug builds — the failure surface discipline misses
    /// today (ADR-0093 §4 / Consequences).
    fn drop(&mut self) {
        if self.hold.is_some() {
            drop(self.hold.take());
            debug_assert!(
                false,
                "TaskDone dropped without resolve — the deferred reply was never sent (the \
                 carried hold has been released so settlement isn't wedged, but the caller is \
                 owed a reply that never went out)"
            );
        }
    }
}

impl InflightTable {
    /// Insert a freshly-minted in-flight entry at dispatch time and
    /// return its [`DispatchId`]. The actor thread calls this (under the
    /// table lock) right after acquiring the hold, before spawning the
    /// worker.
    fn insert(
        &mut self,
        hold: SettlementHold,
        reply_to: Source,
        context: Box<dyn Any + Send>,
    ) -> DispatchId {
        let id = self.mint_id();
        self.entries.insert(
            id,
            InflightEntry {
                hold,
                reply_to,
                context,
                output: None,
            },
        );
        id
    }

    /// Fill the worker's `output` into the named entry's completion slot.
    /// Called once, on the worker thread, under the table lock. A no-op
    /// for an unknown id (the dispatch was cancelled out of the table
    /// before the worker finished).
    fn fill_output(&mut self, id: DispatchId, output: Box<dyn Any + Send>) {
        if let Some(entry) = self.entries.get_mut(&id) {
            entry.output = Some(output);
        }
    }

    /// Remove the named entry and downcast its boxed `context` + filled
    /// `output` into a typed [`TaskDone`]. Returns `None` for an unknown
    /// id (cancelled / double-landed) or if the worker hasn't filled the
    /// output yet (the completion-wake must land after the fill, so this
    /// is the unknown-id case in practice). A type mismatch on either
    /// downcast also yields `None` — a wiring bug where the handler's
    /// `O` / `C` don't match the dispatch's.
    fn take<O: 'static, C: 'static>(&mut self, id: DispatchId) -> Option<TaskDone<O, C>> {
        let entry = self.entries.remove(&id)?;
        let InflightEntry {
            hold,
            reply_to,
            context,
            output,
        } = entry;
        let output = output?.downcast::<O>().ok()?;
        let context = context.downcast::<C>().ok()?;
        Some(TaskDone {
            output: *output,
            context: *context,
            hold: Some(hold),
            reply_to,
            resolved: false,
        })
    }

    /// Non-consuming peek-then-take (ADR-0093 §3, peek variant). Look the
    /// entry up by `id` and *probe* the boxed `output` + `context` against
    /// `O` / `C` via `downcast_ref` **without removing the entry**. Only
    /// when both probes succeed is the entry removed and rebuilt into a
    /// typed [`TaskDone`]; a probe miss leaves the entry intact and returns
    /// `None`.
    ///
    /// This is what the `#[handler(task)]` dispatch chain needs:
    /// completions all arrive as the single [`TaskCompletionWake`] kind and
    /// are routed to the right task handler by *output type*, so the
    /// generated arm tries each handler's `(O, C)` in turn. A wrong-type
    /// attempt must not consume the entry, or the first probed handler
    /// would swallow a completion meant for a later one. Returns `None` for
    /// an unknown id, an unfilled output (worker not finished — in practice
    /// the unknown-id case, since the wake lands after the fill), or a type
    /// mismatch on either downcast.
    fn try_take<O: 'static, C: 'static>(&mut self, id: DispatchId) -> Option<TaskDone<O, C>> {
        let entry = self.entries.get(&id)?;
        // Probe both boxes without disturbing the entry — an unfilled
        // output slot or a type mismatch on either box short-circuits to
        // `None` (the entry stays intact for a later handler to claim).
        entry.output.as_deref()?.downcast_ref::<O>()?;
        entry.context.downcast_ref::<C>()?;
        // Both match — now it's safe to remove and rebuild.
        self.take(id)
    }
}

/// Crate-internal accessors the [`NativeBinding`](super::binding) wraps
/// in its `Mutex<InflightTable>` field expose to
/// [`NativeCtx`](super::ctx). Kept here next to the table so the
/// ledger's invariants (mint-then-insert, fill-once, take-removes) stay
/// in one file.
impl InflightTable {
    pub(crate) fn dispatch_insert(
        &mut self,
        hold: SettlementHold,
        reply_to: Source,
        context: Box<dyn Any + Send>,
    ) -> DispatchId {
        self.insert(hold, reply_to, context)
    }

    pub(crate) fn dispatch_fill_output(&mut self, id: DispatchId, output: Box<dyn Any + Send>) {
        self.fill_output(id, output);
    }

    pub(crate) fn dispatch_take<O: 'static, C: 'static>(
        &mut self,
        id: DispatchId,
    ) -> Option<TaskDone<O, C>> {
        self.take(id)
    }

    pub(crate) fn dispatch_try_take<O: 'static, C: 'static>(
        &mut self,
        id: DispatchId,
    ) -> Option<TaskDone<O, C>> {
        self.try_take(id)
    }
}

/// Wrap [`InflightTable`] for the [`NativeBinding`](super::binding)
/// field: a `Mutex` for `&self` interior mutability matching the binding's
/// other single-writer buffers.
pub(crate) type InflightLedger = Mutex<InflightTable>;

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "test-setup unwraps: fixture construction panic on failure is the assertion"
)]
mod tests {
    use super::*;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::Arc;
    use std::sync::mpsc;
    use std::time::Duration;

    use aether_data::{MailId, MailboxId, Source, SourceAddr, mailbox_id_from_name};

    use crate::actor::native::NativeBinding;
    use crate::actor::native::ctx::NativeCtx;
    use crate::mail::registry::{InboxHandler, OwnedDispatch};
    use crate::test_util::fresh_substrate;

    /// A `#[repr(C)]` `Pod` reply kind the worker produces and `resolve`
    /// re-replies. Carries a `u64` so a test can assert the routed reply
    /// payload is exactly the worker's output.
    #[repr(C)]
    #[derive(
        Copy,
        Clone,
        Debug,
        PartialEq,
        Eq,
        bytemuck::Pod,
        bytemuck::Zeroable,
        serde::Serialize,
        serde::Deserialize,
    )]
    struct Answer {
        value: u64,
    }

    impl Kind for Answer {
        const NAME: &'static str = "test.dispatch_blocking.answer";
        const ID: KindId = KindId(0xD15B_0CC1_0000_0001);
        aether_data::pod_kind_codec!();
    }

    /// Forward every dispatched envelope onto `tx` so a test can observe
    /// the routed reply. The reply lands at the caller's
    /// `SourceAddr::Component(sink)` mailbox.
    fn forward_to(tx: mpsc::Sender<OwnedDispatch>) -> Arc<dyn InboxHandler> {
        Arc::new(move |dispatch: OwnedDispatch| {
            // ADR-0094: terminal test consumer — discharge before the
            // value is forwarded for the test to observe and drop.
            dispatch.discharge();
            let _ = tx.send(dispatch);
        })
    }

    /// A synthetic chain root the dispatching handler reads from
    /// `ctx.in_flight_root()` — distinct so the hold accounting is
    /// isolated.
    fn root_id(cid: u64) -> MailId {
        MailId {
            sender: MailboxId(0xAB),
            correlation_id: cid,
        }
    }

    /// Block until the worker's [`TaskCompletionWake`] lands on the
    /// registered actor inbox channel, returning its decoded
    /// [`DispatchId`]. The worker fills the ledger output slot before
    /// pushing the wake, so by the time the wake is observable
    /// `take_task_done` will find the output.
    fn await_wake(wake_rx: &mpsc::Receiver<OwnedDispatch>) -> DispatchId {
        let env = wake_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("completion wake never landed");
        assert_eq!(
            env.kind,
            TaskCompletionWake::ID,
            "only the wake is expected"
        );
        let wake =
            TaskCompletionWake::decode_from_bytes(env.payload.bytes()).expect("wake decodes");
        DispatchId(wake.dispatch_id)
    }

    /// End-to-end happy path: dispatch a blocking closure, drive the
    /// completion through `take_task_done` + `resolve`, and assert the
    /// reply reached the original caller AND the hold released only after
    /// the reply was sent (the chain settles).
    #[test]
    fn dispatch_blocking_replies_and_releases_after_reply() {
        let (registry, mailer) = fresh_substrate();
        let counter = Arc::clone(mailer.trace_handle().settlement_counter());

        // The original caller: a registered inbox we observe the re-reply
        // landing on (the reply routes to SourceAddr::Component(caller)).
        let (reply_tx, reply_rx) = mpsc::channel::<OwnedDispatch>();
        let caller = registry.register_inbox("test.dispatch_blocking.caller", forward_to(reply_tx));

        // The actor's own mailbox — name-derived so the worker's wake
        // push (recipient = self_mailbox) routes to a registered inbox we
        // observe, rather than warn-dropping.
        let actor_mailbox = mailbox_id_from_name("test.dispatch_blocking.actor");
        let binding = Arc::new(NativeBinding::new_for_test(
            Arc::clone(&mailer),
            actor_mailbox,
        ));
        let (wake_tx, wake_rx) = mpsc::channel::<OwnedDispatch>();
        registry.register_inbox("test.dispatch_blocking.actor", forward_to(wake_tx));

        let root = root_id(1);
        let caller_reply_to = Source::with_correlation(SourceAddr::Component(caller), 77);

        // The dispatching handler: eager-acquire the hold, spawn the
        // worker, return.
        {
            let mut ctx = NativeCtx::new(&binding, caller_reply_to, MailId::NONE, root);
            let _id = ctx.dispatch_blocking(move || Answer { value: 42 });
        }

        // The handler returned but the chain is held: settlement is
        // gated until the reply lands.
        assert_eq!(
            counter.held_open(root),
            1,
            "the chain stays held after the dispatching handler returns"
        );

        // The worker ran, filled the ledger, and pushed the wake.
        let id = await_wake(&wake_rx);
        assert_eq!(
            counter.held_open(root),
            1,
            "the worker finishing does not release the chain"
        );

        // The completion handler runs: rebuild the TaskDone and resolve.
        {
            let mut ctx = NativeCtx::new(&binding, Source::NONE, MailId::NONE, MailId::NONE);
            let done = ctx
                .take_task_done::<Answer, ()>(id)
                .expect("the dispatch is in the ledger");
            assert_eq!(*done.output(), Answer { value: 42 });
            done.resolve(&mut ctx);
        }

        // The reply reached the original caller.
        let reply = reply_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("the re-reply lands on the caller's mailbox");
        assert_eq!(
            reply.kind,
            Answer::ID,
            "reply carries the worker's output kind"
        );
        // A Component-targeted reply is postcard-encoded by
        // `Mailer::send_reply` (not cast), so decode it the same way.
        let answer: Answer = postcard::from_bytes(reply.payload.bytes()).expect("reply decodes");
        assert_eq!(answer, Answer { value: 42 });
        assert_eq!(
            reply.sender.correlation_id, 77,
            "the caller's correlation is echoed onto the reply"
        );

        // Hold released after the reply — chain may settle.
        assert_eq!(
            counter.held_open(root),
            0,
            "resolve releases the hold after re-replying"
        );
    }

    /// The resumed entry uses the *supplied* `(hold, reply_to)`, not the
    /// dispatching ctx's — the property a bounded `TaskQueue` relies on
    /// when it drains a buffered request from a *different* handler's turn.
    /// Accept on one root/caller, dispatch via `dispatch_blocking_resumed`
    /// from a ctx with a different root and reply target, then assert the
    /// *accept* chain is the one held and the *original* caller is replied
    /// to.
    #[test]
    fn dispatch_blocking_resumed_uses_supplied_hold_and_reply_to() {
        let (registry, mailer) = fresh_substrate();
        let counter = Arc::clone(mailer.trace_handle().settlement_counter());

        let (reply_tx, reply_rx) = mpsc::channel::<OwnedDispatch>();
        let caller = registry.register_inbox("test.dispatch_resumed.caller", forward_to(reply_tx));

        let actor_mailbox = mailbox_id_from_name("test.dispatch_resumed.actor");
        let binding = Arc::new(NativeBinding::new_for_test(
            Arc::clone(&mailer),
            actor_mailbox,
        ));
        let (wake_tx, wake_rx) = mpsc::channel::<OwnedDispatch>();
        registry.register_inbox("test.dispatch_resumed.actor", forward_to(wake_tx));

        let accept_root = root_id(1);
        let caller_reply_to = Source::with_correlation(SourceAddr::Component(caller), 77);

        // "Accept": acquire the hold on the accept root + capture the
        // caller, as a TaskQueue does when buffering an over-limit request.
        let buffered_hold = {
            let ctx = NativeCtx::new(&binding, caller_reply_to, MailId::NONE, accept_root);
            ctx.acquire_settlement_hold()
        };
        assert_eq!(
            counter.held_open(accept_root),
            1,
            "the accept-time hold keeps the chain open while buffered"
        );

        // "Drain": dispatch the buffered work from a *different* handler
        // turn — a ctx with a different root and reply target — passing the
        // captured `(hold, reply_to)` explicitly.
        let other_root = root_id(2);
        let id = {
            let mut ctx = NativeCtx::new(
                &binding,
                Source::with_correlation(SourceAddr::None, 99),
                MailId::NONE,
                other_root,
            );
            ctx.dispatch_blocking_resumed(buffered_hold, caller_reply_to, move || Answer {
                value: 7,
            })
        };

        // The held chain is the accept root, not the drain ctx's root.
        assert_eq!(
            counter.held_open(accept_root),
            1,
            "the supplied hold keeps the accept chain open across the resumed dispatch"
        );
        assert_eq!(
            counter.held_open(other_root),
            0,
            "the drain ctx's own chain is never held"
        );

        let landed = await_wake(&wake_rx);
        assert_eq!(landed, id);

        {
            let mut ctx = NativeCtx::new(&binding, Source::NONE, MailId::NONE, MailId::NONE);
            let done = ctx
                .take_task_done::<Answer, ()>(id)
                .expect("the resumed dispatch is in the ledger");
            assert_eq!(*done.output(), Answer { value: 7 });
            done.resolve(&mut ctx);
        }

        // Reply went to the *original* caller (corr 77), not the drain ctx.
        let reply = reply_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("the re-reply lands on the captured caller, not the drain ctx");
        assert_eq!(
            reply.sender.correlation_id, 77,
            "the resumed dispatch replies to the captured caller, not the drain ctx"
        );
        assert_eq!(
            counter.held_open(accept_root),
            0,
            "resolve releases the captured hold"
        );
    }

    /// `dispatch_blocking_with` carries an opt-in context the completion
    /// handler reads via `TaskDone::context`, and `resolve_with` maps
    /// `(output, context)` to the reply.
    #[test]
    fn dispatch_blocking_with_context_resolve_with() {
        let (registry, mailer) = fresh_substrate();

        let (reply_tx, reply_rx) = mpsc::channel::<OwnedDispatch>();
        let caller =
            registry.register_inbox("test.dispatch_blocking.caller2", forward_to(reply_tx));

        let actor_mailbox = mailbox_id_from_name("test.dispatch_blocking.actor2");
        let binding = Arc::new(NativeBinding::new_for_test(
            Arc::clone(&mailer),
            actor_mailbox,
        ));
        let (wake_tx, wake_rx) = mpsc::channel::<OwnedDispatch>();
        registry.register_inbox("test.dispatch_blocking.actor2", forward_to(wake_tx));

        let root = root_id(2);
        let caller_reply_to = Source::with_correlation(SourceAddr::Component(caller), 5);

        {
            let mut ctx = NativeCtx::new(&binding, caller_reply_to, MailId::NONE, root);
            // Worker produces a raw count; context carries an offset the
            // completion handler folds in.
            let _id = ctx.dispatch_blocking_with(100u64, move || 7u64);
        }

        let id = await_wake(&wake_rx);
        {
            let mut ctx = NativeCtx::new(&binding, Source::NONE, MailId::NONE, MailId::NONE);
            let done = ctx
                .take_task_done::<u64, u64>(id)
                .expect("the dispatch is in the ledger");
            assert_eq!(*done.output(), 7);
            assert_eq!(*done.context(), 100);
            done.resolve_with(&mut ctx, |output, cx| Answer { value: output + cx });
        }

        let reply = reply_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("the mapped re-reply lands");
        // A Component-targeted reply is postcard-encoded by
        // `Mailer::send_reply` (not cast), so decode it the same way.
        let answer: Answer = postcard::from_bytes(reply.payload.bytes()).expect("reply decodes");
        assert_eq!(
            answer,
            Answer { value: 107 },
            "resolve_with folds output + context"
        );
    }

    /// Dropping a `TaskDone` without resolving releases the hold (so
    /// settlement isn't wedged) and `debug_assert`s. Gated `#[should_panic]`
    /// — the assertion only fires in debug builds, which is where tests run.
    #[test]
    #[should_panic(expected = "TaskDone dropped without resolve")]
    #[cfg(debug_assertions)]
    fn dropping_task_done_without_resolve_releases_and_asserts() {
        let (_registry, mailer) = fresh_substrate();
        let counter = Arc::clone(mailer.trace_handle().settlement_counter());

        let root = root_id(3);
        // Acquire a hold the same way dispatch does and hand it to a
        // TaskDone we then drop unresolved.
        let hold = mailer.acquire_settlement_hold(root);
        assert_eq!(counter.held_open(root), 1, "hold acquired");

        let done: TaskDone<u64, ()> = TaskDone {
            output: 1,
            context: (),
            hold: Some(hold),
            reply_to: Source::NONE,
            resolved: false,
        };
        // The drop releases the hold (verified indirectly: the chain
        // returns to 0 even as the assertion unwinds) then debug_asserts.
        drop(done);
    }

    /// Companion to the panic test: a [`TaskDone`] dropped unresolved still
    /// releases its hold (so settlement isn't permanently wedged). Built
    /// with the assertion compiled out — verifies the release half in
    /// isolation by catching the unwind.
    #[test]
    fn dropping_task_done_releases_hold_even_when_unresolved() {
        let (_registry, mailer) = fresh_substrate();
        let counter = Arc::clone(mailer.trace_handle().settlement_counter());
        let root = root_id(4);
        let hold = mailer.acquire_settlement_hold(root);
        assert_eq!(counter.held_open(root), 1);

        let result = catch_unwind(AssertUnwindSafe(|| {
            let done: TaskDone<u64, ()> = TaskDone {
                output: 1,
                context: (),
                hold: Some(hold),
                reply_to: Source::NONE,
                resolved: false,
            };
            drop(done);
        }));
        // In debug the drop asserts (unwinds); in release it doesn't.
        // Either way the hold released.
        let _ = result;
        assert_eq!(
            counter.held_open(root),
            0,
            "an unresolved TaskDone releases its hold on drop"
        );
    }
}
