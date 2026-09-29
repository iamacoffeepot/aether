//! ADR-0093 hold-until-resolve dispatch primitive (runtime half).
//!
//! The third spawn shape (alongside `spawn_inherit` and
//! `spawn_detached`, see [`super::thread`]): *work that replies in a
//! later handler turn*. A handler kicks off a slow blocking call, the
//! worker pushes its result and dies, and the real reply is sent from a
//! *subsequent* handler invocation when that result lands. The
//! settlement hold must outlive the worker — it spans accept → the later
//! re-reply — so neither `spawn_inherit` (hold dies with the worker) nor
//! `spawn_detached` (no hold) fits.
//!
//! This generalises the content-gen `InFlightDispatch` prototype into a
//! first-class ctx primitive. The pieces:
//!
//! - [`DispatchId`] — a `Copy` correlation token minted per dispatch.
//! - [`TaskDone`] — a move-only completion that carries the worker's
//!   output, the originating [`Source`], the held [`SettlementHold`],
//!   and an opt-in context `C`. Its consuming [`TaskDone::resolve`]
//!   re-replies through the carried reply target **first**, then drops
//!   the hold (`Sent` before `Release`, ADR-0080 §12). Dropping a
//!   `TaskDone` without resolving releases the hold and then panics outside
//!   an unwind, in every build, which the scheduler escalates through the
//!   chassis aborter (ADR-0063) — a lost reply is never silent.
//! - the in-flight ledger (`InflightTable`) — a per-actor map from
//!   `DispatchId` to its held `(hold, reply_to, context)` plus a
//!   completion output slot the worker fills. A staged task's entry
//!   (ADR-0243 §9) holds only its hold, its request id, and the output
//!   slot: it owes no reply. Lives in the `InflightLedger` on
//!   [`NativeBinding`](crate::actor::native::binding): a `Mutex`, because
//!   threads other than the actor's own write it — offload workers filling
//!   or abandoning output, child activations and the registry owner
//!   completing deferred work, a `Held` dropped wherever it is dropped, and
//!   close and teardown — beside a lock-free flag that says whether any
//!   entry is parked in a request context.
//! - [`TaskCompletionWake`] — a substrate-internal framework kind the
//!   worker pushes (carrying just the `DispatchId`) to the actor's own
//!   mailbox, the same loopback-wake mechanism `InFlightDispatch`'s
//!   worker uses to wake the actor. A staged task's wake is correlated to
//!   its request and carries the chain its hold keeps open.
//!
//! The request side and completion routing live on
//! [`NativeCtx`](crate::actor::native::ctx): `dispatch_blocking` /
//! `dispatch_blocking_with` spawn the worker, and `take_task_done`
//! reunites the worker's output with the held `(hold, reply_to,
//! context)` when the completion-wake mail lands. The
//! `#[handler(task)]` macro sugar that hand-wires the completion handler
//! is a separate later PR; for now a handler matches
//! [`TaskCompletionWake`] explicitly and calls `take_task_done` itself.

use std::any::Any;
use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, Weak};
use std::thread;

use aether_actor::{ActorRef, ErasedActorRef, HandlesKind, ReplyMode, Single};
use aether_data::name_inventory::EngineOnlyKind;
use aether_data::{ActorMail, Kind, KindId, MailId, RequestId, wire};

use crate::mail::Source;
use crate::runtime::trace::SettlementHold;

use super::held::AnswerUnanswered;
use crate::actor::native::binding::NativeBinding;
use crate::actor::native::ctx::NativeCtx;

/// A `Copy` correlation token minted monotonically per armed reply
/// obligation: one entry in the `InflightTable`, armed by an offload
/// dispatch such as [`dispatch_blocking`](NativeCtx::dispatch_blocking) or
/// by [`NativeCtx::hold`] (ADR-0243 §1). A worker entry's id rides the
/// [`TaskCompletionWake`] mail so the completion routes back to the right
/// ledger entry. Returned to the call site for *optional* cancellation —
/// the happy path ignores it.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DispatchId(pub u64);

/// A type-level "receipt" for a deferred reply (ADR-0109). A request handler
/// returns it to declare `-> Pending<R>`: the reply is an `R`, sent later
/// rather than synchronously on this handler's return — from the matching
/// `#[handler(task)]` completion, or through the [`Held<R>`] debt minted
/// beside it.
///
/// Phantom over `R` only — the actual hold and reply target live in the
/// in-flight ledger entry it names, not here, so a `Pending<R>` carries just
/// the [`DispatchId`] (reachable via [`Pending::dispatch_id`] for *optional*
/// cancellation) plus the reply-kind marker. Framework-constructed:
/// `Pending::new` is crate-internal, and the only mint sites are
/// [`NativeCtx::hold`] and the offload dispatch calls
/// [`NativeCtx::dispatch_blocking`] and
/// [`NativeCtx::dispatch_blocking_with_pending`], so every receipt names an
/// armed ledger entry (ADR-0109 §3, ADR-0243 §3). A bounded queue returns
/// the receipt its `hold` minted from its `submit`.
///
/// The receipt must be returned from the handler that minted it: the
/// `#[actor]` / `#[handler_set]` dispatch takes the returned receipt as the
/// handler's declaration that it replies `R` later. Dropping an armed receipt
/// anywhere else panics outside an unwind (ADR-0243 §7), so a handler cannot
/// mint a deferred reply, discard the receipt, and declare a `-> ()` row that
/// hides the reply it sends later.
///
/// [`Held<R>`]: crate::actor::native::offload::held::Held
pub struct Pending<R: ActorMail> {
    dispatch_id: DispatchId,
    /// Set at mint; cleared only by [`Pending::disarm`], the dispatch view's
    /// acknowledgement that the handler returned the receipt.
    armed: bool,
    /// `fn() -> R` so `Pending<R>` is covariant in `R` and stays
    /// `Send`/`Sync` regardless of `R` — it owns no `R`, it only names
    /// the reply kind.
    _reply: PhantomData<fn() -> R>,
}

impl<R: ActorMail> Pending<R> {
    /// Wrap the armed obligation's [`DispatchId`]. Crate-internal — called
    /// only from [`NativeCtx::hold`] and
    /// [`NativeCtx::dispatch_blocking_with_pending`] (ADR-0109 §3,
    /// ADR-0243 §3).
    pub(crate) fn new(dispatch_id: DispatchId) -> Self {
        Self { dispatch_id, armed: true, _reply: PhantomData }
    }

    /// The [`DispatchId`] of the armed dispatch, for *optional*
    /// cancellation. The happy path ignores it — the completion routes
    /// back through the in-flight ledger without it.
    #[must_use]
    pub fn dispatch_id(&self) -> DispatchId {
        self.dispatch_id
    }

    /// Accept the receipt as returned from its handler. Reachable only from
    /// `NativeCtx::<A, Manual>::__accept_pending`, which the `#[actor]` and
    /// `#[handler_set]` native dispatch arms call on the value a
    /// `-> Pending<R>` handler returns; a single handler never holds that
    /// view, so it cannot disarm its own receipt and declare a false row.
    pub(crate) fn disarm(mut self) {
        self.armed = false;
    }
}

impl<R: ActorMail> Drop for Pending<R> {
    /// An armed receipt dropped outside an unwind is a handler that minted a
    /// deferred reply without returning its receipt, so its declared row lies
    /// about the reply it sends. Fail fast (ADR-0243 §7); a panic already
    /// unwinding past the receipt stays the one reported.
    fn drop(&mut self) {
        assert!(
            !self.armed || thread::panicking(),
            "Pending<{}> dropped without being returned from its handler: a handler that mints a \
             deferred reply must return the receipt (ADR-0243 §7)",
            R::NAME
        );
    }
}

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

/// Move-only typed capability for filling one armed dispatch completion.
///
/// The capability deliberately retains only a weak reference to the parent
/// binding plus the ledger id. Completing after the parent has gone away is
/// therefore a no-op: dropping the binding already dropped the ledger entry
/// and its settlement hold, and no stale wake is emitted.
#[must_use = "complete the deferred output; dropping it abandons the ledger entry and releases its hold without waking"]
pub(crate) struct DeferredCompletion<O> {
    binding: Weak<NativeBinding>,
    dispatch_id: DispatchId,
    armed: bool,
    _output: PhantomData<fn(O)>,
}

impl<O> DeferredCompletion<O> {
    pub(crate) fn new(binding: Weak<NativeBinding>, dispatch_id: DispatchId) -> Self {
        Self { binding, dispatch_id, armed: true, _output: PhantomData }
    }

    pub(crate) fn dispatch_id(&self) -> DispatchId {
        self.dispatch_id
    }

    /// Consume this capability and offer its output to the parent ledger.
    /// Only the first fill wins and wakes the actor.
    pub(crate) fn complete(mut self, output: O)
    where
        O: Send + 'static,
    {
        self.armed = false;
        if let Some(binding) = self.binding.upgrade() {
            binding.dispatch_complete(self.dispatch_id, output);
        }
    }
}

impl<O> Drop for DeferredCompletion<O> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.armed = false;
        if let Some(binding) = self.binding.upgrade() {
            drop(binding.dispatch_abandon(self.dispatch_id));
        }
    }
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

// Engine-only mail (ADR-0233): `NativeBinding::wake_self` pushes it from host
// code, so it has no `ActorMail` impl, and its entry is submitted by hand
// because this impl bypasses the `Kind` derive.
aether_data::name_inventory::inventory::submit! {
    EngineOnlyKind {
        kind: <TaskCompletionWake as Kind>::ID,
        name: <TaskCompletionWake as Kind>::NAME,
    }
}

/// One armed reply obligation's held state, parked in the [`InflightTable`]
/// from the arming handler's return until the obligation is answered.
///
/// The actor thread writes the entry when it arms (the hold and reply
/// target, plus the [`EntryState`] naming what answers it). A worker entry
/// is filled by its worker under the table mutex and read back when its
/// [`TaskCompletionWake`] lands ([`NativeCtx::take_task_done`]); a held
/// entry is claimed by its [`Held`](crate::actor::native::offload::held::Held)
/// ticket.
struct InflightEntry {
    /// The [`SettlementHold`] acquired eagerly in the arming handler
    /// (before it returned), keeping the chain root open across the
    /// deferral. Released only after the re-reply, via
    /// [`TaskDone::resolve`] or `Held::answer`, or, for a staged task, when
    /// its completion's ctx drops. `None` when the arming context had no
    /// chain to hold, in which case the obligation is invisible to
    /// settlement (ADR-0168 §2).
    hold: Option<SettlementHold>,
    /// What answers the obligation, and where its reply goes when it owes
    /// one.
    state: EntryState,
}

/// What answers one ledger entry (ADR-0243 §1). Every state but
/// [`Self::Task`] owes the caller whose reply target it carries.
enum EntryState {
    /// An offload worker produces the output a later completion replies
    /// with (ADR-0093).
    Worker {
        /// The originating caller's reply target, captured when armed. The
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
    },
    /// Armed by [`NativeCtx::hold`] with no worker: the `Held` ticket that
    /// names this entry answers it, from any handler on the actor.
    Held {
        /// The caller the ticket answers.
        reply_to: Source,
        /// Sends the reply kind's `unanswered` value to `reply_to` when the
        /// actor closes first.
        answer: AnswerUnanswered,
    },
    /// A held entry whose ticket sits in the encoded bytes of a stored
    /// request context (ADR-0243 §4). Only a decode of that context under
    /// the same `request` and `reply` claims it back to [`Self::Held`].
    Parked {
        /// The caller the ticket answers.
        reply_to: Source,
        /// Carried from [`Self::Held`] so a parked ticket is still answered
        /// at actor close.
        answer: AnswerUnanswered,
        /// The request whose context carries the ticket.
        request: RequestId,
        /// The reply kind the ticket answers.
        reply: KindId,
        /// The stored context's kind name, for the untaken-reply failure
        /// (ADR-0243 §7).
        context_name: &'static str,
    },
    /// Work staged by [`NativeCtx::stage_blocking`] (ADR-0243 §9). It owes
    /// no reply, so it has no reply target: its completion wakes the actor
    /// correlated to `request`, on the chain its hold keeps open, and takes
    /// the context stored under `request` from the ctx.
    Task {
        /// The request id staging minted, from the counter outbound requests
        /// use.
        request: RequestId,
        /// The worker's output, filled once; `None` until the worker
        /// finishes.
        output: Option<Box<dyn Any + Send>>,
    },
}

/// A held or parked entry an actor's close removed from its ledger: the
/// close sends `answer` to `reply_to` under the hold's root, then releases
/// the hold, so `Sent` precedes `Release` (ADR-0243 §1).
pub(crate) struct OwedAtClose {
    pub(crate) hold: Option<SettlementHold>,
    pub(crate) reply_to: Source,
    pub(crate) answer: AnswerUnanswered,
}

/// Per-actor in-flight ledger for hold-until-resolve dispatch (ADR-0093
/// §2). Maps a [`DispatchId`] to its held `(hold, reply_to, context)`
/// plus the worker's eventual output. Opaque framework plumbing — it
/// holds none of the cap's *business* state, only the primitive's own
/// bookkeeping, so centralising it here doesn't violate the
/// plain-actor-state rule (ADR-0038).
///
/// It lives behind the [`InflightLedger`]'s `Mutex` because it has writers
/// off the actor's dispatch thread: offload workers fill or abandon an
/// entry's output, child activations and the registry owner complete
/// deferred work, a dropped `Held` claims its entry from whichever thread
/// drops it, and close and teardown drain it. Only the actor's own turn
/// parks or unparks an entry, which is what lets the ledger keep its
/// lock-free `any_parked` flag exact.
pub(crate) struct InflightTable {
    next_id: u64,
    entries: HashMap<DispatchId, InflightEntry>,
    /// The parked entries of each request whose stored context carries
    /// tickets, so a reply's dispatch tail finds an untaken one in one probe.
    parked: HashMap<RequestId, Vec<DispatchId>>,
}

impl InflightTable {
    pub(crate) fn new() -> Self {
        Self { next_id: 0, entries: HashMap::new(), parked: HashMap::new() }
    }

    /// Mint the next monotonic [`DispatchId`]. Called on the actor
    /// thread under the table lock, so the bump is uncontended.
    fn mint_id(&mut self) -> DispatchId {
        self.next_id += 1;
        DispatchId(self.next_id)
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum FillOutcome {
    /// This fill won; the actor is woken as the entry's state says.
    Filled(CompletionWake),
    AlreadyFilled,
    Missing,
}

/// What a peek at a completion's entry found, before it is taken.
enum Probe {
    /// The worker has not filled its output yet.
    Unfilled,
    /// The output or context is not the `(O, C)` the taker asked for.
    Mismatched,
    /// The output is filled and both types match.
    Matched,
}

/// How a winning fill wakes the actor.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum CompletionWake {
    /// A worker entry's unchained, uncorrelated loopback wake: its completion
    /// replies through the target the entry captured.
    Unchained,
    /// A staged task's wake (ADR-0243 §9): correlated to `request`, and on
    /// the chain `root` the entry's hold keeps open.
    Task { request: RequestId, root: Option<MailId> },
}

/// A move-only dispatch completion (ADR-0093 §3-§4). Carries the
/// worker's `output`, the held [`SettlementHold`], an opt-in context `C`
/// (unit by default), and, for a completion that owes a reply, the
/// originating [`Source`].
///
/// Two kinds of work complete as one: a worker armed by a reply-arming
/// verb (`dispatch_blocking*`), which owes its caller the reply it
/// `resolve`s, and a task staged by [`NativeCtx::stage_blocking`], which
/// owes nothing (ADR-0243 §9). A staged task's completion is discharged by
/// [`Self::into_output`]; its context is taken from the ctx with
/// `take_context`, so its `C` is always `()`, and a `resolve*` call on it
/// panics.
///
/// Move-only by construction — no `Clone` / `Copy` — so the held state
/// can't be duplicated and the hold's release can't be issued twice. The
/// consuming `resolve` family re-replies **first**, then drops the hold,
/// making the `Sent`-before-`Release` ordering (ADR-0080 §12) structural
/// rather than a remembered drop order. Dropping a `TaskDone` that owes a
/// reply without resolving it releases the hold and then panics outside an
/// unwind, in every build, which the scheduler escalates through the
/// chassis aborter (ADR-0063) — catching the silent lost reply that
/// discipline misses.
#[must_use = "a TaskDone holds the chain open; resolve it (or resolve_err) to send the deferred reply and release the hold"]
pub struct TaskDone<O, C = ()> {
    /// The worker's output, `None` only once [`Self::into_output`] moved it
    /// out.
    output: Option<O>,
    context: C,
    /// The chain the dispatch keeps open, absent when the dispatching
    /// context had none to give (ADR-0168 §2). Also `take`n out by
    /// `resolve` so the release lands *after* the reply is sent, leaving
    /// `Drop` nothing to do — `resolved` rather than this field is what
    /// separates a resolved completion from a lost one.
    hold: Option<SettlementHold>,
    /// The caller a worker's completion owes, `None` for a staged task,
    /// which owes nothing (ADR-0243 §9).
    reply_to: Option<Source>,
    /// Set true by every `resolve*` path before it consumes `self`, so
    /// `Drop` can tell a resolved completion (clean) from a dropped-
    /// without-resolve one (the lost-reply bug).
    resolved: bool,
}

/// A move-only reply the actor still owes its caller, carried across
/// successive owner-staged operations without releasing its settlement hold.
///
/// Two fields of substance: who is waiting ([`Source`]) and the obligation to
/// answer them (the [`SettlementHold`] that keeps the caller's causal chain
/// open, absent when the capturing context had no chain — ADR-0168 §2).
/// Erlang spells the same value `From`; JavaScript spells it `resolve`.
/// It carries no code and reifies no rest-of-computation, so it is a deferred
/// reply rather than a continuation.
///
/// Successor staging consumes it only after all synchronous preparation
/// succeeds; a preparation error must return it to the caller so the terminal
/// error can still be replied exactly once. Dropping one without replying
/// strands the caller forever, which is why [`Drop`] releases the hold and then
/// panics outside an unwind, in every build, which the scheduler escalates
/// through the chassis aborter (ADR-0063) — the distinction from a plain
/// context value, whose drop means nothing. A `DeferredReply` names no reply
/// kind, so an actor close while the engine keeps running cannot answer it
/// the way it answers a `Held<R>` (ADR-0243 §1): its owner answers it with
/// the terminal it knows, in `unwire`. [`Self::abandon_for_actor_close`] is the one quiet discharge, and
/// it survives only for the component host's boot waiters until #7008 removes
/// it.
///
/// A debt may also forward any number of already-encoded non-terminal
/// replies first, through [`Self::reply_envelope`], which borrows it and
/// leaves it owed. [`Self::reply`] and [`Self::abandon_for_actor_close`] stay
/// the only discharges.
///
/// # Distinct from `InboundMail`
///
/// ADR-0080 keeps two counts per root, and this type and
/// [`InboundMail`](crate::chassis::inbox::InboundMail) are the handles for one
/// each (iamacoffeepot/aether#4163). This one is the debt and moves
/// `held_open`; `InboundMail` brackets a drained envelope and moves
/// `in_flight`. They are deliberately not one type: on every deferred reply
/// both are outstanding on the same chain, the inbound recording `Finished`
/// when the handler returns while this hold keeps the chain open until the
/// answer goes out. Merging the handles would merge the counts and settle the
/// chain inside the window the hold exists to cover.
#[must_use = "stage a successor or reply to the original caller before dropping the deferred reply"]
pub struct DeferredReply {
    hold: Option<SettlementHold>,
    reply_to: Source,
    consumed: bool,
}

impl DeferredReply {
    pub(crate) fn new(hold: Option<SettlementHold>, reply_to: Source) -> Self {
        Self { hold, reply_to, consumed: false }
    }

    pub(crate) fn into_parts(mut self) -> (Option<SettlementHold>, Source) {
        let hold = self.hold.take();
        self.consumed = true;
        (hold, self.reply_to)
    }

    /// Forward an already-encoded reply to the waiting caller without ending the exchange;
    /// the typed `reply` stays the one terminal.
    ///
    /// The reply takes the same binding reply path as [`Self::reply`], under
    /// the root this debt's hold keeps open: its `Sent` counts against that
    /// chain, its id is minted in the replier's reply-lineage space, and the
    /// caller's correlation is echoed. An engine-only `kind` (ADR-0233) is
    /// refused with a warning and nothing is sent.
    ///
    /// Its consumer is the fleet proxy, which relays each reply event a
    /// remote engine streams for a forwarded call before the call's
    /// terminal settles the debt.
    pub fn reply_envelope<M: ReplyMode, A>(&self, ctx: &mut NativeCtx<'_, A, M>, kind: KindId, bytes: &[u8]) {
        ctx.reply_envelope_to_target(self.reply_to, kind, bytes, self.hold.as_ref().map(SettlementHold::root), None);
    }

    /// Send the terminal reply through the original target and then release
    /// the continuously-held settlement root.
    pub fn reply<M, R, A>(mut self, ctx: &mut NativeCtx<'_, A, M>, reply: &R)
    where
        M: ReplyMode,
        R: ActorMail,
    {
        let root = self.hold.as_ref().map(SettlementHold::root);
        ctx.reply_to_target(self.reply_to, reply, root, None);
        drop(self.hold.take());
        self.consumed = true;
    }

    /// Release the obligation with no reply because the actor that owned its
    /// pending state is itself closing. Every other close while the engine
    /// keeps running answers its debts (ADR-0243 §1): the ledger sends
    /// `R::unanswered()` for each `Held<R>`, and a manual owner replies to
    /// its `DeferredReply`s in `unwire`. This
    /// survives only for the component host's boot waiters (`PendingBoot`),
    /// which close through a slot drop that runs no `unwire`, until #7008
    /// turns them into `Held`s and removes it.
    #[doc(hidden)]
    pub fn abandon_for_actor_close(mut self) {
        drop(self.hold.take());
        self.consumed = true;
    }
}

impl Drop for DeferredReply {
    fn drop(&mut self) {
        if !self.consumed {
            drop(self.hold.take());
            // Fails fast outside an unwind. A panic already unwinding past this
            // debt is the one the aborter should see; a second panic here would
            // abort the process instead.
            assert!(
                thread::panicking(),
                "DeferredReply dropped without successor staging or terminal reply (the hold was released, but the owed reply was lost)"
            );
        }
    }
}

/// Surrender an owed reply as a bare [`DeferredReply`].
///
/// Implemented by [`DeferredReply`] itself (identity), by [`TaskDone`],
/// whose completion carries the same debt alongside a worker output and a
/// context, and by the typed
/// [`Held<R>`](crate::actor::native::offload::held::Held). Staging surfaces such as
/// [`HandlerSpawnBuilder::continue_from`](crate::actor::native::spawn::HandlerSpawnBuilder::continue_from)
/// take `impl IntoDeferredReply` so a handler can continue from either without
/// an intermediate noun at the call site, and can be handed the value back
/// unchanged when synchronous preparation fails.
pub trait IntoDeferredReply {
    /// Consume `self`, transferring its hold and original reply target into a
    /// bare [`DeferredReply`]. No `Release` is emitted: the same move-only hold
    /// stays continuously owned until the successor eventually replies or is
    /// abandoned with its actor binding.
    fn into_deferred_reply(self) -> DeferredReply;
}

impl IntoDeferredReply for DeferredReply {
    fn into_deferred_reply(self) -> DeferredReply {
        self
    }
}

impl<O, C> IntoDeferredReply for TaskDone<O, C> {
    /// # Panics
    /// Panics on a staged task's completion, which owes no reply to carry
    /// (ADR-0243 §9).
    fn into_deferred_reply(mut self) -> DeferredReply {
        let reply_to = self.owed_reply_to();
        self.resolved = true;
        DeferredReply { hold: self.hold.take(), reply_to, consumed: false }
    }
}

impl<O, C> TaskDone<O, C> {
    /// Borrow the worker's output. The common `resolve` re-replies this
    /// directly; `resolve_with` maps it.
    ///
    /// # Panics
    /// Never in practice: only [`Self::into_output`] moves the output out,
    /// and it consumes the completion.
    pub fn output(&self) -> &O {
        self.output.as_ref().expect("a TaskDone holds its output until into_output consumes it")
    }

    /// Take the worker's output, discharging a staged task's completion
    /// (ADR-0243 §9): a staged task owes no reply, so its completion
    /// answers whatever it serves from actor state, with this output.
    ///
    /// # Panics
    /// Panics on a completion that owes a reply, which must be resolved
    /// instead so the reply it owes is sent.
    pub fn into_output(mut self) -> O {
        assert!(
            self.reply_to.is_none(),
            "TaskDone::into_output on a completion that owes a reply: resolve it so the caller is answered"
        );
        self.resolved = true;
        drop(self.hold.take());
        self.output.take().expect("a TaskDone holds its output until into_output consumes it")
    }

    /// Hand a staged task's settlement hold to the ctx whose handler takes
    /// its completion, so the chain it keeps open stays open until that
    /// handler's sends are counted (ADR-0243 §9). `None` for a completion
    /// that owes a reply, which keeps its hold until it is resolved.
    pub(crate) fn take_task_hold(&mut self) -> Option<SettlementHold> {
        if self.reply_to.is_some() {
            return None;
        }
        self.hold.take()
    }

    /// The caller this completion owes.
    ///
    /// # Panics
    /// Panics on a staged task's completion, which owes nothing.
    fn owed_reply_to(&self) -> Source {
        self.reply_to.expect("a staged task owes no reply (ADR-0243 §9)")
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

    /// The root the carried [`SettlementHold`] gates (ADR-0080 §5 /
    /// #1695). The deferred reply stamps this so its `Sent` joins the
    /// chain the hold keeps open — replied to from a *later* handler turn
    /// whose own ctx has no relation to the originating chain.
    /// `None` once the hold is taken (post-`release`) or for a
    /// chainless dispatch that never held one.
    fn hold_root(&self) -> Option<MailId> {
        self.hold.as_ref().map(SettlementHold::root)
    }

    /// Re-reply the carried `output` through the carried `reply_to`,
    /// then release the hold (ADR-0093 §4). The worker already shaped
    /// `output` into the reply value, so this is the common one-liner.
    ///
    /// # Panics
    /// Panics on a staged task's completion, which owes no reply (ADR-0243
    /// §9).
    pub fn resolve<A>(mut self, ctx: &mut NativeCtx<'_, A, Single>)
    where
        O: ActorMail,
    {
        ctx.reply_to_target(self.owed_reply_to(), self.output(), self.hold_root(), None);
        self.release();
    }

    /// Map `(&output, &context)` to a reply value via `f`, send it
    /// through the carried `reply_to`, then release the hold. For
    /// completion handlers that shape a different reply from the carried
    /// output (and context, when present) than the raw `output`.
    ///
    /// # Panics
    /// Panics on a staged task's completion, which owes no reply (ADR-0243
    /// §9).
    pub fn resolve_with<R, F, A>(mut self, ctx: &mut NativeCtx<'_, A, Single>, f: F)
    where
        R: ActorMail,
        F: FnOnce(&O, &C) -> R,
    {
        let reply_to = self.owed_reply_to();
        let reply = f(self.output(), &self.context);
        ctx.reply_to_target(reply_to, &reply, self.hold_root(), None);
        self.release();
    }

    /// Send a precomputed `reply` value through the carried `reply_to`,
    /// then release the hold (ADR-0109). The deferred-contract form: the
    /// `#[handler(task)]` completion handler *borrows* the `TaskDone` and
    /// **returns** the reply, and the `#[actor]` macro hands that value
    /// here — [`resolve_with`](Self::resolve_with) with the value already
    /// computed by the handler rather than built in a ctx-less closure.
    /// Re-replies **first**, then releases the hold (`Sent` before
    /// `Release`, ADR-0080 §12), like the rest of the `resolve*` family.
    ///
    /// # Panics
    /// Panics on a staged task's completion, which owes no reply (ADR-0243
    /// §9).
    pub fn resolve_value<R, A>(mut self, ctx: &mut NativeCtx<'_, A, Single>, reply: &R)
    where
        R: ActorMail,
    {
        ctx.reply_to_target(self.owed_reply_to(), reply, self.hold_root(), None);
        self.release();
    }

    /// Hand the owed reply to `target`, a proven actor that then replies in
    /// its own name: push `payload` to it with the carried reply target
    /// pinned and the held root as its lineage, then release the hold. The
    /// push takes its settlement count before the release, so the chain the
    /// hold kept open stays open until `target` answers.
    ///
    /// The owed reply leaves this actor, so the waiting caller hears from
    /// `target` — stamped as the reply's sender — rather than from the actor
    /// that took the request. No verb lets one actor reply *as* another; this
    /// one moves the obligation to an actor that genuinely sends.
    ///
    /// Its consumer is the component host, which hands a successful load to
    /// the trampoline it just spawned, so the requester takes its reference
    /// to the loaded actor from the reply's stamped sender (ADR-0230 §3).
    ///
    /// # Panics
    /// Panics on a staged task's completion, which owes no reply to hand
    /// off (ADR-0243 §9).
    pub fn hand_off<R, K, A, M>(mut self, ctx: &mut NativeCtx<'_, A, M>, target: &ActorRef<R>, payload: &K)
    where
        R: HandlesKind<K>,
        K: ActorMail,
        M: ReplyMode,
    {
        ctx.push_handed_off(target.erase(), payload, self.hold_root(), self.owed_reply_to());
        self.release();
    }

    /// Forward already-encoded `bytes` of `kind` to `target` as a tracked
    /// request whose reply comes back to this actor, under the root the
    /// carried hold keeps open, then release the hold. The forward takes its
    /// settlement count before the release, so the caller's chain stays open
    /// until `target` answers and this actor answers the caller from that
    /// reply's turn, which inherits the same root.
    ///
    /// Returns the forward's [`MailId`], whose correlation keys the reply, or
    /// hands the completion back unresolved when the send is refused (an
    /// engine-only `kind`, ADR-0233, or bytes whose tag-1 fields do not
    /// resolve), so the caller can still be answered.
    ///
    /// Its consumer is the component host, which forwards a replace to the
    /// trampoline once the replacement module's publish commits (ADR-0241 §4).
    pub fn forward_tracked<A, M: ReplyMode>(
        mut self,
        ctx: &NativeCtx<'_, A, M>,
        target: ErasedActorRef,
        kind: KindId,
        bytes: &[u8],
    ) -> Result<MailId, Self> {
        let Some(mail_id) = ctx.send_envelope_tracked_under(target, kind, bytes, self.hold_root()) else {
            return Err(self);
        };
        self.release();
        Ok(mail_id)
    }

    /// Release the hold **without** sending any reply — the sanctioned
    /// no-reply completion (ADR-0109): a `#[handler(task)]` that borrows
    /// the `TaskDone` and returns `()` discharges the chain without
    /// replying. Unlike dropping an un-resolved `TaskDone` (a lost reply),
    /// this is a deliberate signature choice, so it releases cleanly and
    /// skips the lost-reply panic. A staged task's completion, which owes
    /// nothing, discharges the same way.
    pub fn release_no_reply(mut self) {
        self.release();
    }

    /// Send an error reply (a provider-failure shape the cap builds)
    /// through the carried `reply_to`, then release the hold. The
    /// carried `output` is discarded — used when the completion is a
    /// failure rather than a result.
    ///
    /// # Panics
    /// Panics on a staged task's completion, which owes no reply (ADR-0243
    /// §9).
    pub fn resolve_err<E, A>(mut self, ctx: &mut NativeCtx<'_, A, Single>, err: &E)
    where
        E: ActorMail,
    {
        ctx.reply_to_target(self.owed_reply_to(), err, self.hold_root(), None);
        self.release();
    }
}

impl<O, C> Drop for TaskDone<O, C> {
    /// A `TaskDone` that owes a reply, dropped without a `resolve*` call, is
    /// a lost reply: the caller was owed a deferred reply that never went
    /// out. Release the hold so the chain can still settle (a stuck hold
    /// would wedge settlement forever), then panic outside an unwind, in
    /// every build, so the scheduler escalates the bug through the chassis
    /// aborter (ADR-0063; ADR-0093 §4 / Consequences). A panic already
    /// unwinding past the completion stays the one reported. A staged task's
    /// completion owes nothing, so its drop only releases its hold.
    fn drop(&mut self) {
        if !self.resolved {
            drop(self.hold.take());
            assert!(
                self.reply_to.is_none() || thread::panicking(),
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
    fn insert(&mut self, hold: Option<SettlementHold>, reply_to: Source, context: Box<dyn Any + Send>) -> DispatchId {
        let id = self.mint_id();
        self.entries.insert(id, InflightEntry { hold, state: EntryState::Worker { reply_to, context, output: None } });
        id
    }

    /// Insert an entry armed with no worker (ADR-0243 §1) and return its
    /// [`DispatchId`]. Only its `Held` ticket claims it back, and `answer`
    /// replies for it when the actor closes first.
    fn insert_held(&mut self, hold: Option<SettlementHold>, reply_to: Source, answer: AnswerUnanswered) -> DispatchId {
        let id = self.mint_id();
        self.entries.insert(id, InflightEntry { hold, state: EntryState::Held { reply_to, answer } });
        id
    }

    /// Insert a staged task's entry (ADR-0243 §9) and return its
    /// [`DispatchId`]: it keeps the staging turn's hold, owes no reply, and
    /// waits for its worker's output under `request`.
    fn insert_task(&mut self, hold: Option<SettlementHold>, request: RequestId) -> DispatchId {
        let id = self.mint_id();
        self.entries.insert(id, InflightEntry { hold, state: EntryState::Task { request, output: None } });
        id
    }

    /// Remove the named entry and hand back its parked `(hold, reply_to)`,
    /// only when it is a held entry. `None` for an unknown id or a worker
    /// entry, which is left in place: a held ticket never discharges a
    /// worker's obligation.
    fn claim_held(&mut self, id: DispatchId) -> Option<(Option<SettlementHold>, Source)> {
        let EntryState::Held { reply_to, .. } = self.entries.get(&id)?.state else {
            return None;
        };
        let entry = self.entries.remove(&id)?;
        Some((entry.hold, reply_to))
    }

    /// Hand the held entry `id` to a worker (ADR-0243 §3): it keeps its hold
    /// and reply target, and its state becomes a worker entry carrying
    /// `context` with no output yet, so the worker's completion answers the
    /// obligation the entry's `Held` ticket named. The entry's close answer
    /// drops: a worker entry names no reply kind.
    ///
    /// # Panics
    /// Panics when `id` names no held entry: an unknown id, an entry a
    /// worker already answers, or one parked in a stored context.
    fn attach_worker(&mut self, id: DispatchId, context: Box<dyn Any + Send>) {
        let entry = self.entries.get_mut(&id).expect("a worker attached to a ledger entry that is not held");
        let EntryState::Held { reply_to, .. } = entry.state else {
            panic!("a worker attached to a ledger entry that is not held");
        };
        entry.state = EntryState::Worker { reply_to, context, output: None };
    }

    /// Remove every entry no worker answers, parked ones and staged tasks
    /// included, for the actor-close tail (ADR-0243 §1, §9). A held or parked
    /// entry comes back owed, with the answer a close while the engine keeps
    /// running sends before its hold releases; an engine teardown drops it
    /// unanswered. A staged task owes nothing and a closing actor handles no
    /// completion, so its hold comes back to release with no reply. Worker
    /// entries stay: their workers' fills and wakes still find them, and the
    /// binding's drop settles them as before.
    fn close_for_actor(&mut self) -> (Vec<OwedAtClose>, Vec<Option<SettlementHold>>) {
        self.parked.clear();
        let (mut owed, mut released) = (Vec::new(), Vec::new());
        for (_, InflightEntry { hold, state }) in
            self.entries.extract_if(|_, entry| !matches!(entry.state, EntryState::Worker { .. }))
        {
            match state {
                EntryState::Held { reply_to, answer } | EntryState::Parked { reply_to, answer, .. } => {
                    owed.push(OwedAtClose { hold, reply_to, answer });
                }
                EntryState::Task { .. } | EntryState::Worker { .. } => released.push(hold),
            }
        }
        (owed, released)
    }

    /// Name the state of the named entry, for tests that pin which entries
    /// a ledger operation leaves behind.
    #[cfg(test)]
    fn state_of(&self, id: DispatchId) -> Option<&'static str> {
        self.entries.get(&id).map(|entry| match entry.state {
            EntryState::Worker { .. } => "worker",
            EntryState::Held { .. } => "held",
            EntryState::Parked { .. } => "parked",
            EntryState::Task { .. } => "task",
        })
    }

    /// Park the held entry `id` in the context stored under `request`
    /// (ADR-0243 §4): it answers `reply` and waits for that context's take.
    ///
    /// # Errors
    /// [`wire::Error::HeldUnclaimed`] when `id` names no held entry.
    fn park(
        &mut self,
        id: DispatchId,
        request: RequestId,
        reply: KindId,
        context_name: &'static str,
    ) -> Result<(), wire::Error> {
        let unclaimed = || wire::Error::HeldUnclaimed { ticket: id.0, reply };
        let entry = self.entries.get_mut(&id).ok_or_else(unclaimed)?;
        let EntryState::Held { reply_to, answer } = entry.state else {
            return Err(unclaimed());
        };
        entry.state = EntryState::Parked { reply_to, answer, request, reply, context_name };
        self.parked.entry(request).or_default().push(id);
        Ok(())
    }

    /// Claim the parked entry `id` back to held for a decode of the context
    /// stored under `request`.
    ///
    /// # Errors
    /// [`wire::Error::HeldUnclaimed`] when `id` is not parked under both
    /// `request` and `reply`.
    fn unpark(&mut self, id: DispatchId, request: RequestId, reply: KindId) -> Result<(), wire::Error> {
        let unclaimed = || wire::Error::HeldUnclaimed { ticket: id.0, reply };
        let entry = self.entries.get_mut(&id).ok_or_else(unclaimed)?;
        let EntryState::Parked { reply_to, answer, request: parked_request, reply: parked_reply, .. } = entry.state
        else {
            return Err(unclaimed());
        };
        if parked_request != request || parked_reply != reply {
            return Err(unclaimed());
        }
        entry.state = EntryState::Held { reply_to, answer };
        if let Some(ids) = self.parked.get_mut(&request) {
            ids.retain(|parked| *parked != id);
            if ids.is_empty() {
                self.parked.remove(&request);
            }
        }
        Ok(())
    }

    /// The kind name of the context stored under `request` while it still
    /// carries a parked ticket.
    fn parked_context(&self, request: RequestId) -> Option<&'static str> {
        self.parked.get(&request)?.iter().find_map(|id| match self.entries.get(id)?.state {
            EntryState::Parked { context_name, .. } => Some(context_name),
            _ => None,
        })
    }

    /// Fill the worker's `output` into the named entry's completion slot and
    /// say how the winning fill wakes the actor: a worker entry's unchained
    /// wake, or a staged task's wake correlated to its request on the chain
    /// its hold keeps open. Called once, on the worker thread, under the
    /// table lock. A no-op for an unknown id (the dispatch was cancelled out
    /// of the table before the worker finished) or an entry no worker
    /// answers.
    fn fill_output(&mut self, id: DispatchId, output: Box<dyn Any + Send>) -> FillOutcome {
        let Some(InflightEntry { hold, state }) = self.entries.get_mut(&id) else {
            return FillOutcome::Missing;
        };
        let (slot, wake) = match state {
            EntryState::Worker { output: slot, .. } => (slot, CompletionWake::Unchained),
            EntryState::Task { request, output: slot } => {
                (slot, CompletionWake::Task { request: *request, root: hold.as_ref().map(SettlementHold::root) })
            }
            EntryState::Held { .. } | EntryState::Parked { .. } => return FillOutcome::Missing,
        };
        if slot.is_some() {
            return FillOutcome::AlreadyFilled;
        }
        *slot = Some(output);
        FillOutcome::Filled(wake)
    }

    /// Remove the named entry and hand back its hold, if it holds one,
    /// **without** any `O` / `C` downcast — the worker never ran, so there
    /// is no output to type. The spawn-error branch calls this to release
    /// the eagerly-acquired hold when arming failed, and an unstarted staged
    /// task's drop calls it to give its chain back: the caller drops the
    /// returned hold, settling the chain the entry would otherwise wedge
    /// forever. A no-op for an unknown id, and for an entry no worker
    /// answers, which is left in place for its `Held` ticket.
    fn abandon(&mut self, id: DispatchId) -> Option<SettlementHold> {
        if !matches!(self.entries.get(&id)?.state, EntryState::Worker { .. } | EntryState::Task { .. }) {
            return None;
        }
        self.entries.remove(&id)?.hold
    }

    /// Remove the named entry and downcast its boxed `context` + filled
    /// `output` into a typed [`TaskDone`]. Returns `None` for an unknown
    /// id (cancelled / double-landed) or if the worker hasn't filled the
    /// output yet (the completion-wake must land after the fill, so this
    /// is the unknown-id case in practice) — leaving the entry intact on
    /// either miss so a parked hold is never bare-dropped. A downcast
    /// *mismatch* against a filled output is a genuine `O` / `C` wiring bug:
    /// it `debug_assert`s loudly (distinct from the benign unfilled case)
    /// and returns `None` with the entry retained. An entry no worker
    /// answers is never taken.
    ///
    /// A worker entry rebuilds a completion that owes its captured caller.
    /// A staged task's rebuilds one that owes nothing, with a `()` context:
    /// its context is a kind stored under its request, taken from the ctx
    /// (ADR-0243 §9).
    fn take<O: 'static, C: 'static>(&mut self, id: DispatchId) -> Option<TaskDone<O, C>> {
        // Peek-then-remove, the same discipline `try_take` uses: probe the
        // boxed `output` + `context` without disturbing the entry. An
        // unfilled output slot returns `None` quietly (a later wake completes
        // the still-parked entry). A type mismatch against a *filled* output
        // is a wiring bug — loud in debug, `None` in release — and never
        // removes the entry, so the parked hold stays reclaimable.
        match self.probe::<O, C>(id)? {
            Probe::Unfilled => return None,
            Probe::Mismatched => {
                debug_assert!(
                    false,
                    "dispatch completion type mismatch: the task handler's (O, C) do not match the \
                     dispatch's — a wiring bug (the entry is retained, not bare-dropped)"
                );
                return None;
            }
            Probe::Matched => {}
        }
        // Both probes passed — safe to remove and rebuild.
        let InflightEntry { hold, state } = self.entries.remove(&id)?;
        let (output, context, reply_to) = match state {
            EntryState::Worker { reply_to, context, output } => (output?, context, Some(reply_to)),
            EntryState::Task { output, .. } => (output?, Box::new(()) as Box<dyn Any + Send>, None),
            EntryState::Held { .. } | EntryState::Parked { .. } => return None,
        };
        let output = output.downcast::<O>().ok()?;
        let context = context.downcast::<C>().ok()?;
        Some(TaskDone { output: Some(*output), context: *context, hold, reply_to, resolved: false })
    }

    /// Probe the named entry's output and context against `O` / `C` without
    /// disturbing it. `None` for an unknown id or an entry no worker
    /// answers. A staged task's context is always `()`.
    fn probe<O: 'static, C: 'static>(&self, id: DispatchId) -> Option<Probe> {
        let (output, context): (_, &dyn Any) = match &self.entries.get(&id)?.state {
            EntryState::Worker { context, output, .. } => (output.as_deref(), &**context),
            EntryState::Task { output, .. } => (output.as_deref(), &()),
            EntryState::Held { .. } | EntryState::Parked { .. } => return None,
        };
        Some(match output {
            None => Probe::Unfilled,
            Some(output) if output.is::<O>() && context.is::<C>() => Probe::Matched,
            Some(_) => Probe::Mismatched,
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
        // Probe both boxes without disturbing the entry — an unfilled
        // output slot or a type mismatch on either box short-circuits to
        // `None` (the entry stays intact for a later handler to claim).
        if !matches!(self.probe::<O, C>(id)?, Probe::Matched) {
            return None;
        }
        // Both match — now it's safe to remove and rebuild.
        self.take(id)
    }
}

/// Crate-internal accessors the [`NativeBinding`](crate::actor::native::binding)'s
/// [`InflightLedger`] lock exposes to
/// [`NativeCtx`](crate::actor::native::ctx). Kept here next to the table so the
/// ledger's invariants (mint-then-insert, fill-once, take-removes) stay
/// in one file.
impl InflightTable {
    pub(crate) fn dispatch_insert(
        &mut self,
        hold: Option<SettlementHold>,
        reply_to: Source,
        context: Box<dyn Any + Send>,
    ) -> DispatchId {
        self.insert(hold, reply_to, context)
    }

    pub(crate) fn dispatch_fill_output(&mut self, id: DispatchId, output: Box<dyn Any + Send>) -> FillOutcome {
        self.fill_output(id, output)
    }

    pub(crate) fn dispatch_take<O: 'static, C: 'static>(&mut self, id: DispatchId) -> Option<TaskDone<O, C>> {
        self.take(id)
    }

    pub(crate) fn dispatch_abandon(&mut self, id: DispatchId) -> Option<SettlementHold> {
        self.abandon(id)
    }

    pub(crate) fn dispatch_insert_task(&mut self, hold: Option<SettlementHold>, request: RequestId) -> DispatchId {
        self.insert_task(hold, request)
    }

    pub(crate) fn dispatch_try_take<O: 'static, C: 'static>(&mut self, id: DispatchId) -> Option<TaskDone<O, C>> {
        self.try_take(id)
    }

    pub(crate) fn dispatch_insert_held(
        &mut self,
        hold: Option<SettlementHold>,
        reply_to: Source,
        answer: AnswerUnanswered,
    ) -> DispatchId {
        self.insert_held(hold, reply_to, answer)
    }

    pub(crate) fn dispatch_claim_held(&mut self, id: DispatchId) -> Option<(Option<SettlementHold>, Source)> {
        self.claim_held(id)
    }

    pub(crate) fn dispatch_attach_worker(&mut self, id: DispatchId, context: Box<dyn Any + Send>) {
        self.attach_worker(id, context);
    }

    #[cfg(test)]
    pub(crate) fn dispatch_state_of(&self, id: DispatchId) -> Option<&'static str> {
        self.state_of(id)
    }
}

/// The [`NativeBinding`](crate::actor::native::binding)'s in-flight ledger:
/// the [`InflightTable`] behind a `Mutex`, for its off-thread writers, and
/// `any_parked`, which lets a reply's dispatch tail skip the lock when no
/// entry is parked in a request context (ADR-0243 §7).
///
/// `any_parked` is `!table.parked.is_empty()`, stored under `table`'s lock
/// by [`Self::park`], [`Self::unpark`] and [`Self::close_for_actor`], the
/// only operations that change `parked`; the table's own versions are
/// private to this module, so nothing else can. Every one of them runs on
/// the actor's turn or in its close, and a park for a request happens in a
/// turn before that request's reply is dispatched, so the scheduler's slot
/// handoff orders the `Release` store before the reply's `Acquire` load. A
/// `true` read is only conservative: the locked lookup still decides.
pub(crate) struct InflightLedger {
    table: Mutex<InflightTable>,
    any_parked: AtomicBool,
}

impl InflightLedger {
    pub(crate) fn new() -> Self {
        Self { table: Mutex::new(InflightTable::new()), any_parked: AtomicBool::new(false) }
    }

    /// Lock the table for one ledger operation.
    ///
    /// # Panics
    /// Panics if the ledger mutex is poisoned — fail-fast per ADR-0063.
    pub(crate) fn lock(&self) -> MutexGuard<'_, InflightTable> {
        self.table.lock().expect("in-flight ledger poisoned; fail-fast per ADR-0063")
    }

    /// Park the held entry `id` in the context stored under `request`
    /// (ADR-0243 §4) and set `any_parked`.
    ///
    /// # Errors
    /// [`wire::Error::HeldUnclaimed`] when `id` names no held entry.
    pub(crate) fn park(
        &self,
        id: DispatchId,
        request: RequestId,
        reply: KindId,
        context_name: &'static str,
    ) -> Result<(), wire::Error> {
        let mut table = self.lock();
        let parked = table.park(id, request, reply, context_name);
        self.any_parked.store(!table.parked.is_empty(), Ordering::Release);
        parked
    }

    /// Claim the parked entry `id` back to held for a decode of the context
    /// stored under `request`, clearing `any_parked` when it was the last.
    ///
    /// # Errors
    /// [`wire::Error::HeldUnclaimed`] when `id` is not parked under both
    /// `request` and `reply`.
    pub(crate) fn unpark(&self, id: DispatchId, request: RequestId, reply: KindId) -> Result<(), wire::Error> {
        let mut table = self.lock();
        let unparked = table.unpark(id, request, reply);
        self.any_parked.store(!table.parked.is_empty(), Ordering::Release);
        unparked
    }

    /// Remove every entry no worker answers for the actor-close tail and
    /// clear `any_parked`, as the table's `close_for_actor` describes.
    pub(crate) fn close_for_actor(&self) -> (Vec<OwedAtClose>, Vec<Option<SettlementHold>>) {
        let mut table = self.lock();
        let closed = table.close_for_actor();
        self.any_parked.store(!table.parked.is_empty(), Ordering::Release);
        closed
    }

    /// The kind name of the context stored under `request` while it still
    /// carries a parked ticket, without taking the lock when no entry is
    /// parked anywhere.
    pub(crate) fn parked_context(&self, request: RequestId) -> Option<&'static str> {
        if !self.any_parked.load(Ordering::Acquire) {
            return None;
        }
        self.lock().parked_context(request)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test-setup unwraps: fixture construction panic on failure is the assertion")]
mod tests {
    use super::*;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::Arc;
    use std::sync::mpsc;
    use std::time::Duration;

    use aether_data::{MailId, MailboxId, Source, SourceAddr};

    use crate::NativeInitCtx;
    use crate::actor::native::NativeActor;
    use crate::actor::native::NativeBinding;
    use crate::actor::native::ctx::NativeCtx;
    use crate::chassis::builder::ReplyTarget;
    use crate::chassis::error::BootError;
    use crate::mail::registry::{InboxHandler, OwnedDispatch};
    use crate::testing::{
        PumpedDriver, bare_substrate, boot_authority, boot_bare_test_chassis, fresh_substrate, registered_ref,
    };

    /// A `#[repr(C)]` `Pod` reply kind the worker produces and `resolve`
    /// re-replies. Carries a `u64` so a test can assert the routed reply
    /// payload is exactly the worker's output.
    #[repr(C)]
    #[derive(
        Copy, Clone, Debug, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable, serde::Serialize, serde::Deserialize,
    )]
    struct Answer {
        value: u64,
    }

    impl Kind for Answer {
        const NAME: &'static str = "test.dispatch_blocking.answer";
        const ID: KindId = KindId(0xD15B_0CC1_0000_0001);
        aether_data::pod_kind_codec!();
    }

    impl ActorMail for Answer {}
    impl aether_data::CrossesActors for Answer {}

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
        MailId { sender: MailboxId(0xAB), correlation_id: cid }
    }

    /// Block until the worker's [`TaskCompletionWake`] lands on the
    /// registered actor inbox channel, returning its decoded
    /// [`DispatchId`]. The worker fills the ledger output slot before
    /// pushing the wake, so by the time the wake is observable
    /// `take_task_done` will find the output.
    fn await_wake(wake_rx: &mpsc::Receiver<OwnedDispatch>) -> DispatchId {
        let env = wake_rx.recv_timeout(Duration::from_secs(2)).expect("completion wake never landed");
        assert_eq!(env.kind, TaskCompletionWake::ID, "only the wake is expected");
        let wake = TaskCompletionWake::decode_from_bytes(env.payload.bytes()).expect("wake decodes");
        DispatchId(wake.dispatch_id)
    }

    /// The reply [`GatedAsk`]'s worker produces and its completion resolves
    /// by value.
    #[aether_data::kind(name = "test.dispatch_blocking.gated_answer", copy, partial_eq)]
    struct GatedAnswer {
        value: u64,
    }

    #[aether_data::kind(name = "test.dispatch_blocking.ask")]
    struct Ask;

    /// A pumped root whose request handler dispatches a blocking worker
    /// that waits on the test's gate, and whose completion re-replies the
    /// worker's output by value.
    struct GatedAsk {
        /// The gate the one worker waits on, handed in as the boot params.
        gate: Option<mpsc::Receiver<()>>,
        /// Set by `on_ask` once the worker is dispatched.
        asked: bool,
    }

    #[aether_actor::actor(singleton, root)]
    impl NativeActor for GatedAsk {
        const NAMESPACE: &'static str = "test.dispatch_blocking.gated";
        type Config = ();
        type Params = mpsc::Receiver<()>;

        fn init((): (), gate: mpsc::Receiver<()>, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self { gate: Some(gate), asked: false })
        }

        #[handler::single]
        fn on_ask(&mut self, ctx: &mut NativeCtx<'_>, _ask: Ask) -> Pending<GatedAnswer> {
            let gate = self.gate.take().expect("one ask per probe");
            self.asked = true;
            ctx.dispatch_blocking::<GatedAnswer, GatedAnswer, _>(move || {
                gate.recv().expect("the test opens the gate");
                GatedAnswer { value: 42 }
            })
        }

        #[handler(task)]
        fn on_answered(&mut self, ctx: &mut NativeCtx<'_>, done: TaskDone<GatedAnswer>) {
            assert!(self.asked, "a completion follows its ask");
            done.resolve(ctx);
        }
    }

    /// End-to-end happy path through a real handler turn: the request
    /// handler dispatches a blocking worker and returns, the completion
    /// resolves the worker's output by value, and the reply reaches the
    /// original caller. Catches a hold released when the dispatching handler
    /// returns, or before the reply is sent, and a reply that drops the
    /// caller's correlation.
    #[test]
    fn dispatch_blocking_replies_and_releases_after_reply() {
        let (registry, mailer) = fresh_substrate();
        let counter = Arc::clone(mailer.trace_handle().settlement_counter());
        let (reply_tx, reply_rx) = mpsc::channel::<OwnedDispatch>();
        let sink_mailer = Arc::clone(&mailer);
        let caller = registered_ref(
            &registry,
            "test.dispatch_blocking.caller",
            Arc::new(move |dispatch: OwnedDispatch| {
                // The caller forwards the reply, then finishes it so the
                // chain it joined settles only once the test can read it.
                let (mail_id, root) = (dispatch.mail_id, dispatch.root);
                dispatch.discharge();
                let _ = reply_tx.send(dispatch);
                sink_mailer.record_finished(mail_id, root);
            }),
        );
        let (open_gate, gate) = mpsc::channel::<()>();
        let mut driver = PumpedDriver::<GatedAsk>::boot(boot_bare_test_chassis(&registry, &mailer), (), gate);

        let root = driver.send_tracked(
            driver.chassis().actor_ref::<GatedAsk>(),
            &Ask,
            Some(ReplyTarget::Actor { to: caller, correlation: 77 }),
        );
        driver.pump_until("the ask dispatches its worker", |ask| ask.asked);
        assert_eq!(counter.held_open(root), 1, "the chain stays held after the dispatching handler returns");

        open_gate.send(()).expect("the worker waits on the gate");
        driver.settle(&[root]);

        let reply = reply_rx.try_recv().expect("the re-reply lands on the caller's mailbox");
        assert_eq!(reply.kind, GatedAnswer::ID, "the reply carries the worker's output kind");
        assert_eq!(GatedAnswer::decode_from_bytes(reply.payload.bytes()), Some(GatedAnswer { value: 42 }));
        assert_eq!(reply.sender.correlation_id, 77, "the caller's correlation is echoed onto the reply");
        assert_eq!(counter.held_open(root), 0, "resolve releases the hold after re-replying");
    }

    /// The resumed entry uses the *supplied* `(hold, reply_to)`, not the
    /// dispatching ctx's — the property a caller that captured them at
    /// accept relies on when it dispatches from a *different* handler's turn.
    /// Accept on one root/caller, dispatch via `dispatch_blocking_resumed`
    /// from a ctx with a different root and reply target, then assert the
    /// *accept* chain is the one held and the *original* caller is replied
    /// to.
    #[test]
    fn dispatch_blocking_resumed_uses_supplied_hold_and_reply_to() {
        let (registry, mailer) = bare_substrate();
        let counter = Arc::clone(mailer.trace_handle().settlement_counter());

        let (reply_tx, reply_rx) = mpsc::channel::<OwnedDispatch>();
        let caller = registry.register_inbox(&boot_authority(), "test.dispatch_resumed.caller", forward_to(reply_tx));

        let (wake_tx, wake_rx) = mpsc::channel::<OwnedDispatch>();
        let actor_mailbox =
            registry.register_inbox(&boot_authority(), "test.dispatch_resumed.actor", forward_to(wake_tx));
        let binding = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), actor_mailbox));

        let accept_root = root_id(1);
        let caller_reply_to = Source::with_correlation(SourceAddr::Component(caller), 77);

        // "Accept": acquire the hold on the accept root + capture the
        // caller, as a caller deferring a request's dispatch does.
        let buffered_hold = {
            let ctx = NativeCtx::new(&binding, caller_reply_to, None, Some(accept_root));
            ctx.acquire_settlement_hold()
        };
        assert_eq!(counter.held_open(accept_root), 1, "the accept-time hold keeps the chain open while buffered");

        // "Drain": dispatch the buffered work from a *different* handler
        // turn — a ctx with a different root and reply target — passing the
        // captured `(hold, reply_to)` explicitly.
        let other_root = root_id(2);
        let id = {
            let mut ctx =
                NativeCtx::new(&binding, Source::with_correlation(SourceAddr::None, 99), None, Some(other_root));
            ctx.dispatch_blocking_resumed(buffered_hold, caller_reply_to, move || Answer { value: 7 })
        };

        // The held chain is the accept root, not the drain ctx's root.
        assert_eq!(
            counter.held_open(accept_root),
            1,
            "the supplied hold keeps the accept chain open across the resumed dispatch"
        );
        assert_eq!(counter.held_open(other_root), 0, "the drain ctx's own chain is never held");

        let landed = await_wake(&wake_rx);
        assert_eq!(landed, id);

        {
            let mut ctx = NativeCtx::new(&binding, Source::NONE, None, None);
            let done = ctx.take_task_done::<Answer, ()>(id).expect("the resumed dispatch is in the ledger");
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
        assert_eq!(counter.held_open(accept_root), 0, "resolve releases the captured hold");
    }

    /// `dispatch_blocking_with` carries an opt-in context the completion
    /// handler reads via `TaskDone::context`, and `resolve_with` maps
    /// `(output, context)` to the reply.
    #[test]
    fn dispatch_blocking_with_context_resolve_with() {
        let (registry, mailer) = bare_substrate();

        let (reply_tx, reply_rx) = mpsc::channel::<OwnedDispatch>();
        let caller = registry.register_inbox(&boot_authority(), "test.dispatch_blocking.caller2", forward_to(reply_tx));

        let (wake_tx, wake_rx) = mpsc::channel::<OwnedDispatch>();
        let actor_mailbox =
            registry.register_inbox(&boot_authority(), "test.dispatch_blocking.actor2", forward_to(wake_tx));
        let binding = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), actor_mailbox));

        let root = root_id(2);
        let caller_reply_to = Source::with_correlation(SourceAddr::Component(caller), 5);

        {
            let mut ctx = NativeCtx::new(&binding, caller_reply_to, None, Some(root));
            // Worker produces a raw count; context carries an offset the
            // completion handler folds in.
            let _id = ctx.dispatch_blocking_with(100u64, move || 7u64);
        }

        let id = await_wake(&wake_rx);
        {
            let mut ctx = NativeCtx::new(&binding, Source::NONE, None, None);
            let done = ctx.take_task_done::<u64, u64>(id).expect("the dispatch is in the ledger");
            assert_eq!(*done.output(), 7);
            assert_eq!(*done.context(), 100);
            done.resolve_with(&mut ctx, |output, cx| Answer { value: output + cx });
        }

        let reply = reply_rx.recv_timeout(Duration::from_secs(2)).expect("the mapped re-reply lands");
        // A Component-targeted reply is encoded through the kind codec by
        // `Mailer::send_reply` (not cast), so decode it the same way.
        let answer = Answer::decode_from_bytes(reply.payload.bytes()).expect("reply decodes");
        assert_eq!(answer, Answer { value: 107 }, "resolve_with folds output + context");
    }

    /// Dropping a `TaskDone` without resolving releases the hold (so
    /// settlement isn't wedged) and then panics, in every build profile.
    #[test]
    #[should_panic(expected = "TaskDone dropped without resolve")]
    fn dropping_task_done_without_resolve_releases_and_asserts() {
        let (_registry, mailer) = bare_substrate();
        let counter = Arc::clone(mailer.trace_handle().settlement_counter());

        let root = root_id(3);
        // Acquire a hold the same way dispatch does and hand it to a
        // TaskDone we then drop unresolved.
        let hold = mailer.acquire_settlement_hold(root);
        assert_eq!(counter.held_open(root), 1, "hold acquired");

        let done: TaskDone<u64, ()> =
            TaskDone { output: Some(1), context: (), hold: Some(hold), reply_to: Some(Source::NONE), resolved: false };
        // The drop releases the hold (verified indirectly: the chain
        // returns to 0 even as the panic unwinds) then panics.
        drop(done);
    }

    /// Companion to the panic test: a [`TaskDone`] dropped unresolved still
    /// releases its hold (so settlement isn't permanently wedged). Verifies
    /// the release half in isolation by catching the unwind.
    #[test]
    fn dropping_task_done_releases_hold_even_when_unresolved() {
        let (_registry, mailer) = bare_substrate();
        let counter = Arc::clone(mailer.trace_handle().settlement_counter());
        let root = root_id(4);
        let hold = mailer.acquire_settlement_hold(root);
        assert_eq!(counter.held_open(root), 1);

        let result = catch_unwind(AssertUnwindSafe(|| {
            let done: TaskDone<u64, ()> = TaskDone {
                output: Some(1),
                context: (),
                hold: Some(hold),
                reply_to: Some(Source::NONE),
                resolved: false,
            };
            drop(done);
        }));
        // The drop panics after releasing, so the hold is already gone.
        let _ = result;
        assert_eq!(counter.held_open(root), 0, "an unresolved TaskDone releases its hold on drop");
    }

    /// The Site-1 release mechanism: `abandon` removes the entry and hands
    /// back its parked hold + `reply_to` so the spawn-error branch can drop
    /// the hold and settle the chain, rather than orphaning it in the
    /// ledger.
    #[test]
    fn abandon_removes_entry_and_returns_hold() {
        let (_registry, mailer) = bare_substrate();
        let counter = Arc::clone(mailer.trace_handle().settlement_counter());
        let root = root_id(10);
        let hold = mailer.acquire_settlement_hold(root);
        assert_eq!(counter.held_open(root), 1, "hold acquired");

        let mut table = InflightTable::new();
        let id = table.dispatch_insert(Some(hold), Source::NONE, Box::new(()));
        assert!(table.entries.contains_key(&id), "entry parked");

        let abandoned = table.dispatch_abandon(id);
        assert!(abandoned.is_some(), "abandon hands back the parked hold");
        drop(abandoned);
        assert!(!table.entries.contains_key(&id), "abandon removes the entry");
        assert_eq!(counter.held_open(root), 0, "dropping the abandoned hold releases the chain");
    }

    /// An unfilled entry is left intact by `take` — no bare drop of the
    /// parked hold, no premature remove that an early/spurious wake could
    /// otherwise destroy.
    #[test]
    fn take_leaves_entry_on_unfilled() {
        let (_registry, mailer) = bare_substrate();
        let root = root_id(11);
        let hold = mailer.acquire_settlement_hold(root);

        let mut table = InflightTable::new();
        let id = table.dispatch_insert(Some(hold), Source::NONE, Box::new(()));
        // Output was never filled: take returns None and retains the entry.
        assert!(table.dispatch_take::<Answer, ()>(id).is_none());
        assert!(table.entries.contains_key(&id), "an unfilled entry stays parked for a later wake");
    }

    /// Tripwire: a downcast *mismatch* against a filled output is a genuine
    /// `O` / `C` wiring bug — loud (`debug_assert`) in debug, `None` in
    /// release — and is distinct from the benign unfilled case. Either way
    /// the entry is retained rather than bare-dropped.
    #[test]
    fn take_debug_asserts_on_type_mismatch() {
        let (_registry, mailer) = bare_substrate();
        let root = root_id(12);
        let hold = mailer.acquire_settlement_hold(root);

        let mut table = InflightTable::new();
        let id = table.dispatch_insert(Some(hold), Source::NONE, Box::new(()));
        // Fill with a wrong-typed output (u32) where take asks for Answer.
        table.dispatch_fill_output(id, Box::new(7u32));

        let outcome = catch_unwind(AssertUnwindSafe(|| table.dispatch_take::<Answer, ()>(id)));
        #[cfg(debug_assertions)]
        assert!(outcome.is_err(), "a type mismatch debug_asserts, distinct from the benign unfilled None");
        #[cfg(not(debug_assertions))]
        assert!(matches!(outcome, Ok(None)), "a type mismatch returns None in release");
        assert!(table.entries.contains_key(&id), "a mismatched entry is retained, never bare-dropped");
    }

    #[test]
    fn fill_output_retains_the_first_value() {
        let (_registry, mailer) = bare_substrate();
        let mut table = InflightTable::new();
        let id = table.dispatch_insert(
            Some(mailer.acquire_settlement_hold(root_id(13))),
            Source::NONE,
            Box::new(String::from("typed context")),
        );

        assert_eq!(
            table.dispatch_fill_output(id, Box::new(Answer { value: 1 })),
            FillOutcome::Filled(CompletionWake::Unchained)
        );
        assert_eq!(table.dispatch_fill_output(id, Box::new(Answer { value: 2 })), FillOutcome::AlreadyFilled);

        let done =
            table.dispatch_take::<Answer, String>(id).expect("the first typed output and context remain takeable");
        assert_eq!(*done.output(), Answer { value: 1 });
        assert_eq!(done.context(), "typed context");
        done.release_no_reply();
    }

    #[test]
    fn fill_output_reports_a_missing_entry() {
        let mut table = InflightTable::new();
        let id = DispatchId(404);

        assert_eq!(table.dispatch_fill_output(id, Box::new(Answer { value: 1 })), FillOutcome::Missing);
        assert!(table.dispatch_take::<Answer, ()>(id).is_none());
    }

    #[test]
    fn typed_take_rebuilds_the_original_output_and_context() {
        let (_registry, mailer) = bare_substrate();
        let mut table = InflightTable::new();
        let id =
            table.dispatch_insert(Some(mailer.acquire_settlement_hold(root_id(14))), Source::NONE, Box::new(23_u16));
        assert_eq!(
            table.dispatch_fill_output(id, Box::new(Answer { value: 55 })),
            FillOutcome::Filled(CompletionWake::Unchained)
        );

        let done = table.dispatch_take::<Answer, u16>(id).expect("matching typed take succeeds");
        assert_eq!(*done.output(), Answer { value: 55 });
        assert_eq!(*done.context(), 23);
        done.release_no_reply();
    }

    #[test]
    fn duplicate_deferred_completion_keeps_first_output_and_emits_one_wake() {
        let (registry, mailer) = bare_substrate();
        let (wake_tx, wake_rx) = mpsc::channel::<OwnedDispatch>();
        let actor_mailbox =
            registry.register_inbox(&boot_authority(), "test.deferred_completion.duplicate", forward_to(wake_tx));
        let binding = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), actor_mailbox));

        let completion =
            binding.dispatch_arm::<Answer, _>(Some(mailer.acquire_settlement_hold(root_id(15))), Source::NONE, ());
        let id = completion.dispatch_id();
        let duplicate = DeferredCompletion::new(Arc::downgrade(&binding), id);

        completion.complete(Answer { value: 1 });
        duplicate.complete(Answer { value: 2 });

        assert_eq!(await_wake(&wake_rx), id);
        assert!(wake_rx.recv_timeout(Duration::from_millis(50)).is_err(), "a duplicate fill emits no second wake");
        let done = binding.dispatch_take::<Answer, ()>(id).expect("the first completion remains parked");
        assert_eq!(*done.output(), Answer { value: 1 });
        done.release_no_reply();
    }

    #[test]
    fn deferred_completion_after_parent_loss_emits_no_wake() {
        let (registry, mailer) = bare_substrate();
        let counter = Arc::clone(mailer.trace_handle().settlement_counter());
        let root = root_id(16);
        let (wake_tx, wake_rx) = mpsc::channel::<OwnedDispatch>();
        let actor_mailbox =
            registry.register_inbox(&boot_authority(), "test.deferred_completion.parent_loss", forward_to(wake_tx));
        let binding = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), actor_mailbox));

        let completion =
            binding.dispatch_arm::<Answer, _>(Some(mailer.acquire_settlement_hold(root)), Source::NONE, ());
        assert_eq!(counter.held_open(root), 1, "arming parks the hold in the parent ledger");

        drop(binding);
        assert_eq!(counter.held_open(root), 0, "dropping the parent drops its ledger and hold");
        completion.complete(Answer { value: 1 });
        assert!(wake_rx.recv_timeout(Duration::from_millis(50)).is_err(), "parent loss emits no stale wake");
    }

    /// Tripwire: an owed reply that is dropped without being replied to or
    /// staged onto a successor releases its hold (so settlement isn't wedged)
    /// and then panics, in every build profile. The panic is what separates
    /// [`DeferredReply`] from a plain context value — dropping a context means
    /// nothing, dropping a debt strands the caller forever.
    #[test]
    #[should_panic(expected = "DeferredReply dropped without successor staging or terminal reply")]
    fn dropping_deferred_reply_without_replying_releases_and_asserts() {
        let (_registry, mailer) = bare_substrate();
        let counter = Arc::clone(mailer.trace_handle().settlement_counter());
        let root = root_id(19);

        let owed = DeferredReply::new(Some(mailer.acquire_settlement_hold(root)), Source::NONE);
        assert_eq!(counter.held_open(root), 1, "the debt holds the caller's chain open");
        drop(owed);
    }

    /// Companion to the panic test: the dropped debt still releases its hold,
    /// so a lost reply never wedges settlement. Catches the unwind so the
    /// release half is observable on its own.
    #[test]
    fn dropping_deferred_reply_releases_its_hold() {
        let (_registry, mailer) = bare_substrate();
        let counter = Arc::clone(mailer.trace_handle().settlement_counter());
        let root = root_id(20);
        let hold = mailer.acquire_settlement_hold(root);
        assert_eq!(counter.held_open(root), 1);

        let _ = catch_unwind(AssertUnwindSafe(|| drop(DeferredReply::new(Some(hold), Source::NONE))));
        assert_eq!(counter.held_open(root), 0, "an unreplied DeferredReply releases its hold on drop");
    }

    /// Abandoning for actor close is the sanctioned no-reply path: the hold
    /// releases and the lost-reply panic stays quiet, so a parent that
    /// disappears with pending state doesn't take the engine down.
    #[test]
    fn abandoning_a_deferred_reply_for_actor_close_releases_without_asserting() {
        let (_registry, mailer) = bare_substrate();
        let counter = Arc::clone(mailer.trace_handle().settlement_counter());
        let root = root_id(21);

        DeferredReply::new(Some(mailer.acquire_settlement_hold(root)), Source::NONE).abandon_for_actor_close();
        assert_eq!(counter.held_open(root), 0, "actor-close abandonment releases the chain");
    }

    /// A handler that panics while holding a debt surfaces its own panic: the
    /// debt's drop runs during the unwind, releases the hold, and stays quiet
    /// rather than panicking inside cleanup (which would abort the process
    /// and skip the chassis aborter).
    #[test]
    fn dropping_deferred_reply_while_unwinding_keeps_the_original_panic() {
        let (_registry, mailer) = bare_substrate();
        let counter = Arc::clone(mailer.trace_handle().settlement_counter());
        let root = root_id(22);
        let hold = mailer.acquire_settlement_hold(root);

        let payload = catch_unwind(AssertUnwindSafe(|| {
            let _owed = DeferredReply::new(Some(hold), Source::NONE);
            panic!("handler probe");
        }))
        .expect_err("the handler panic propagates");

        assert_eq!(payload.downcast_ref::<&str>(), Some(&"handler probe"), "the original panic payload survives");
        assert_eq!(counter.held_open(root), 0, "the unwinding drop still releases the hold");
    }

    #[test]
    fn task_done_into_deferred_reply_keeps_one_hold_across_successor_completion() {
        let (registry, mailer) = bare_substrate();
        let counter = Arc::clone(mailer.trace_handle().settlement_counter());
        let root = root_id(18);
        let (wake_tx, wake_rx) = mpsc::channel::<OwnedDispatch>();
        let actor_mailbox =
            registry.register_inbox(&boot_authority(), "test.deferred_completion.handoff", forward_to(wake_tx));
        let binding = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), actor_mailbox));

        let first = binding.dispatch_arm::<Answer, _>(
            Some(mailer.acquire_settlement_hold(root)),
            Source::NONE,
            String::from("first"),
        );
        let first_id = first.dispatch_id();
        first.complete(Answer { value: 1 });
        assert_eq!(await_wake(&wake_rx), first_id);

        let done = binding.dispatch_take::<Answer, String>(first_id).expect("first completion remains takeable");
        let (hold, reply_to) = done.into_deferred_reply().into_parts();
        assert_eq!(counter.held_open(root), 1, "the transfer moves the original hold without a release gap");

        let second = binding.dispatch_arm::<Answer, _>(hold, reply_to, String::from("second"));
        let second_id = second.dispatch_id();
        second.complete(Answer { value: 2 });
        assert_eq!(await_wake(&wake_rx), second_id);
        let done = binding
            .dispatch_take::<Answer, String>(second_id)
            .expect("successor completion retains the transferred hold");
        assert_eq!(done.context(), "second");
        done.release_no_reply();
        assert_eq!(counter.held_open(root), 0, "terminal successor release closes the one continuous hold");
    }

    /// Catches a forward pushed on the completion turn's own unchained
    /// lineage instead of the held root: the caller's chain would settle at
    /// the release, before the forwarded request is answered.
    #[test]
    fn forward_tracked_sends_under_the_held_root_and_releases_the_hold() {
        use crate::testing::registered_ref;

        let (registry, mailer) = bare_substrate();
        let counter = Arc::clone(mailer.trace_handle().settlement_counter());
        let root = root_id(19);
        let (wake_tx, wake_rx) = mpsc::channel::<OwnedDispatch>();
        let actor_mailbox =
            registry.register_inbox(&boot_authority(), "test.deferred_completion.forward", forward_to(wake_tx));
        let (target_tx, target_rx) = mpsc::channel::<OwnedDispatch>();
        let target = registered_ref(&registry, "test.deferred_completion.forward_target", forward_to(target_tx));
        let binding = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), actor_mailbox));

        let completion =
            binding.dispatch_arm::<Answer, _>(Some(mailer.acquire_settlement_hold(root)), Source::NONE, ());
        let id = completion.dispatch_id();
        completion.complete(Answer { value: 1 });
        assert_eq!(await_wake(&wake_rx), id);
        let done = binding.dispatch_take::<Answer, ()>(id).expect("the completion remains takeable");

        let ctx = NativeCtx::new(&binding, Source::NONE, None, None);
        let forwarded = done
            .forward_tracked(&ctx, target, Answer::ID, &Answer { value: 2 }.encode_into_bytes())
            .unwrap_or_else(|_| panic!("an actor kind forwards"));
        assert_eq!(counter.held_open(root), 0, "the forward discharges the hold");
        binding.flush_outbound();

        let delivered = target_rx.recv_timeout(Duration::from_secs(2)).expect("the forward is delivered");
        assert_eq!(delivered.root, Some(root), "the forward joins the chain the hold kept open");
        assert_eq!(delivered.mail_id, Some(forwarded));
    }

    #[test]
    fn dropping_armed_deferred_completion_abandons_hold_without_wake() {
        let (registry, mailer) = bare_substrate();
        let counter = Arc::clone(mailer.trace_handle().settlement_counter());
        let root = root_id(18);
        let (wake_tx, wake_rx) = mpsc::channel::<OwnedDispatch>();
        let actor_mailbox =
            registry.register_inbox(&boot_authority(), "test.deferred_completion.drop", forward_to(wake_tx));
        let binding = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), actor_mailbox));

        let completion =
            binding.dispatch_arm::<Answer, _>(Some(mailer.acquire_settlement_hold(root)), Source::NONE, ());
        let id = completion.dispatch_id();
        assert_eq!(counter.held_open(root), 1, "arming parks the hold");

        drop(completion);

        assert_eq!(counter.held_open(root), 0, "dropping the token abandons its ledger entry and hold");
        assert!(wake_rx.recv_timeout(Duration::from_millis(50)).is_err(), "abandonment emits no wake");
        assert!(binding.dispatch_take::<Answer, ()>(id).is_none(), "the abandoned entry was removed");
    }

    /// `reply_envelope` forwards each already-encoded reply to the debt's
    /// caller as it is called, under the name the registry gives its kind,
    /// and leaves the debt owed: the hold stays until the typed terminal
    /// `reply` goes out behind them.
    #[test]
    fn reply_envelope_forwards_in_order_and_keeps_the_debt_owed() {
        use crate::mail::outbound::EgressEvent;
        use crate::testing::{fresh_substrate_and_rx, session_sender, token_root, unrouted_binding};
        use aether_kinds::Tick;

        let (_registry, mailer, egress) = fresh_substrate_and_rx();
        let counter = Arc::clone(mailer.trace_handle().settlement_counter());
        let binding = unrouted_binding(&mailer);
        let root = token_root(23);

        let mut ctx = NativeCtx::new(&binding, session_sender(), None, Some(root));
        let owed = ctx.defer_reply_to(ctx.reply_target());
        let first = Tick { delta_micros: 1 }.encode_into_bytes();
        let second = Tick { delta_micros: 2 }.encode_into_bytes();
        owed.reply_envelope(&mut ctx, Tick::ID, &first);
        owed.reply_envelope(&mut ctx, Tick::ID, &second);
        assert_eq!(counter.held_open(root), 1, "forwarded replies leave the debt owed");

        owed.reply(&mut ctx, &Tick { delta_micros: 3 });
        assert_eq!(counter.held_open(root), 0, "the typed reply discharges the debt");

        let delivered: Vec<(String, Vec<u8>)> = egress
            .try_iter()
            .map(|event| {
                let EgressEvent::ToSession { kind_name, payload, .. } = event else {
                    panic!("only session replies are expected, got {event:?}");
                };
                (kind_name, payload)
            })
            .collect();
        let third = Tick { delta_micros: 3 }.encode_into_bytes();
        let expected: Vec<(String, Vec<u8>)> =
            [first, second, third].into_iter().map(|payload| (Tick::NAME.to_owned(), payload)).collect();
        assert_eq!(delivered, expected, "both forwarded replies arrive in order, ahead of the terminal");
    }

    /// An engine-only kind (ADR-0233) never leaves through `reply_envelope`:
    /// nothing is sent and the debt stays owed.
    #[test]
    fn reply_envelope_refuses_an_engine_only_kind() {
        use crate::testing::{fresh_substrate_and_rx, session_sender, token_root, unrouted_binding};
        use aether_kinds::MonitorNotice;

        let (_registry, mailer, egress) = fresh_substrate_and_rx();
        let counter = Arc::clone(mailer.trace_handle().settlement_counter());
        let binding = unrouted_binding(&mailer);
        let root = token_root(24);

        let mut ctx = NativeCtx::new(&binding, session_sender(), None, Some(root));
        let owed = ctx.defer_reply_to(ctx.reply_target());
        owed.reply_envelope(&mut ctx, MonitorNotice::ID, &MonitorNotice.encode_into_bytes());

        assert!(egress.try_recv().is_err(), "an engine-only reply is refused, not sent");
        assert_eq!(counter.held_open(root), 1, "a refused reply leaves the debt owed");
        owed.abandon_for_actor_close();
    }

    /// A discarded receipt is a handler hiding the reply it owes behind a
    /// false row (ADR-0243 §7), so `Pending`'s `Drop` fails fast when it is
    /// still armed.
    #[test]
    fn dropping_an_armed_pending_panics() {
        let payload = catch_unwind(|| drop(Pending::<Answer>::new(DispatchId(1))))
            .expect_err("an armed receipt dropped outside an unwind fails fast");
        let message =
            payload.downcast_ref::<String>().map(String::as_str).or_else(|| payload.downcast_ref::<&str>().copied());
        assert!(
            message.is_some_and(|message| message.starts_with("Pending<test.dispatch_blocking.answer> dropped")),
            "the panic names the receipt's reply kind"
        );
    }
}
