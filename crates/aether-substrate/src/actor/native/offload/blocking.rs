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
//!   `TaskDone` without resolving releases the hold and `debug_assert`s
//!   (a silent lost reply).
//! - the in-flight ledger (`InflightTable`) — a per-actor map from
//!   `DispatchId` to its held `(hold, reply_to, context)` plus a
//!   completion output slot the worker fills. Lives behind a `&self`
//!   interior-mutability `Mutex` on [`NativeBinding`](crate::actor::native::binding),
//!   like `outbound` / `blob_producer`; the single logical writer is the
//!   actor's own dispatch thread.
//! - [`TaskCompletionWake`] — a substrate-internal framework kind the
//!   worker pushes (carrying just the `DispatchId`) to the actor's own
//!   mailbox, the same loopback-wake mechanism `InFlightDispatch`'s
//!   worker uses to wake the actor.
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
use std::sync::{Mutex, Weak};

use aether_actor::{ReplyMode, Single};
use aether_data::{Kind, KindId, MailId};

use crate::mail::Source;
use crate::runtime::trace::SettlementHold;

use crate::actor::native::binding::NativeBinding;
use crate::actor::native::ctx::NativeCtx;

/// A `Copy` correlation token minted monotonically per
/// [`dispatch_blocking`](NativeCtx::dispatch_blocking). Names one
/// in-flight dispatch in the `InflightTable`; rides the
/// [`TaskCompletionWake`] mail so the completion routes back to the
/// right ledger entry. Returned to the call site for *optional*
/// cancellation — the happy path ignores it.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DispatchId(pub u64);

/// A type-level "receipt" for a deferred reply (ADR-0109). Returned by
/// [`dispatch_blocking`](NativeCtx::dispatch_blocking) in place of a bare
/// [`DispatchId`] so a request handler can declare `-> Pending<R>`: the
/// reply is an `R`, sent later from the matching `#[handler(task)]`
/// completion rather than synchronously on this handler's return.
///
/// Phantom over `R` only — the actual hold and reply target live in the
/// in-flight ledger, not here, so a `Pending<R>` carries just the
/// [`DispatchId`] (reachable via [`Pending::dispatch_id`] for *optional*
/// cancellation) plus the reply-kind marker. Framework-constructed: only
/// `dispatch_blocking` mints one, so a signature that declares
/// `Pending<R>` implies an obligation for `R` was actually armed
/// (ADR-0109 §3) — it is not user-fabricable.
pub struct Pending<R: Kind> {
    dispatch_id: DispatchId,
    /// `fn() -> R` so `Pending<R>` is covariant in `R` and stays
    /// `Send`/`Sync` regardless of `R` — it owns no `R`, it only names
    /// the reply kind.
    _reply: PhantomData<fn() -> R>,
}

impl<R: Kind> Pending<R> {
    /// Wrap the armed dispatch's [`DispatchId`]. Crate-internal — only
    /// [`dispatch_blocking`](NativeCtx::dispatch_blocking) constructs a
    /// `Pending`, which is what makes the `-> Pending<R>` contract
    /// non-forgeable (ADR-0109 §3).
    pub(crate) fn new(dispatch_id: DispatchId) -> Self {
        Self { dispatch_id, _reply: PhantomData }
    }

    /// The [`DispatchId`] of the armed dispatch, for *optional*
    /// cancellation. The happy path ignores it — the completion routes
    /// back through the in-flight ledger without it.
    #[must_use]
    pub fn dispatch_id(&self) -> DispatchId {
        self.dispatch_id
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
    /// [`TaskDone::resolve`]. `None` when the dispatching context had no
    /// chain to hold, in which case the dispatch is invisible to
    /// settlement (ADR-0168 §2).
    hold: Option<SettlementHold>,
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
/// [`NativeBinding`](crate::actor::native::binding)'s `outbound` / `blob_producer`: the
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
        Self { next_id: 0, entries: HashMap::new() }
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
    Filled,
    AlreadyFilled,
    Missing,
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
    /// The chain the dispatch keeps open, absent when the dispatching
    /// context had none to give (ADR-0168 §2). Also `take`n out by
    /// `resolve` so the release lands *after* the reply is sent, leaving
    /// `Drop` nothing to do — `resolved` rather than this field is what
    /// separates a resolved completion from a lost one.
    hold: Option<SettlementHold>,
    reply_to: Source,
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
/// `debug_assert`s — the distinction from a plain context value, whose drop
/// means nothing.
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

    /// Send the terminal reply through the original target and then release
    /// the continuously-held settlement root.
    pub fn reply<M, R, A>(mut self, ctx: &mut NativeCtx<'_, M, A>, reply: &R)
    where
        M: ReplyMode,
        R: Kind + serde::Serialize,
    {
        let root = self.hold.as_ref().map_or(MailId::NONE, SettlementHold::root);
        ctx.reply_to_target(self.reply_to, reply, root, None);
        drop(self.hold.take());
        self.consumed = true;
    }

    /// Release the obligation because the actor that owned its pending state
    /// is itself closing. This is the no-spurious-reply parent-disappearance
    /// path, not an ordinary business completion.
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
            debug_assert!(
                false,
                "DeferredReply dropped without successor staging or terminal reply (the hold was released, but the owed reply was lost)"
            );
        }
    }
}

/// Surrender an owed reply as a bare [`DeferredReply`].
///
/// Implemented by [`DeferredReply`] itself (identity) and by [`TaskDone`],
/// whose completion carries the same debt alongside a worker output and a
/// context. Staging surfaces such as
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
    fn into_deferred_reply(mut self) -> DeferredReply {
        self.resolved = true;
        DeferredReply { hold: self.hold.take(), reply_to: self.reply_to, consumed: false }
    }
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

    /// The root the carried [`SettlementHold`] gates (ADR-0080 §5 /
    /// #1695). The deferred reply stamps this so its `Sent` joins the
    /// chain the hold keeps open — replied to from a *later* handler turn
    /// whose own ctx has no relation to the originating chain.
    /// `MailId::NONE` once the hold is taken (post-`release`) or for a
    /// chainless dispatch that never held one.
    fn hold_root(&self) -> MailId {
        self.hold.as_ref().map_or(MailId::NONE, SettlementHold::root)
    }

    /// Re-reply the carried `output` through the carried `reply_to`,
    /// then release the hold (ADR-0093 §4). The worker already shaped
    /// `output` into the reply value, so this is the common one-liner.
    pub fn resolve<A>(mut self, ctx: &mut NativeCtx<'_, Single, A>)
    where
        O: Kind + serde::Serialize,
    {
        ctx.reply_to_target(self.reply_to, &self.output, self.hold_root(), None);
        self.release();
    }

    /// Map `(&output, &context)` to a reply value via `f`, send it
    /// through the carried `reply_to`, then release the hold. For
    /// completion handlers that shape a different reply from the carried
    /// output (and context, when present) than the raw `output`.
    pub fn resolve_with<R, F, A>(mut self, ctx: &mut NativeCtx<'_, Single, A>, f: F)
    where
        R: Kind + serde::Serialize,
        F: FnOnce(&O, &C) -> R,
    {
        let reply = f(&self.output, &self.context);
        ctx.reply_to_target(self.reply_to, &reply, self.hold_root(), None);
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
    pub fn resolve_value<R, A>(mut self, ctx: &mut NativeCtx<'_, Single, A>, reply: &R)
    where
        R: Kind + serde::Serialize,
    {
        ctx.reply_to_target(self.reply_to, reply, self.hold_root(), None);
        self.release();
    }

    /// Release the hold **without** sending any reply — the sanctioned
    /// no-reply completion (ADR-0109): a `#[handler(task)]` that borrows
    /// the `TaskDone` and returns `()` discharges the chain without
    /// replying. Unlike dropping an un-resolved `TaskDone` (a silent lost
    /// reply), this is a deliberate signature choice, so it releases
    /// cleanly and skips the lost-reply `debug_assert`.
    pub fn release_no_reply(mut self) {
        self.release();
    }

    /// Send an error reply (a provider-failure shape the cap builds)
    /// through the carried `reply_to`, then release the hold. The
    /// carried `output` is discarded — used when the completion is a
    /// failure rather than a result.
    pub fn resolve_err<E, A>(mut self, ctx: &mut NativeCtx<'_, Single, A>, err: &E)
    where
        E: Kind + serde::Serialize,
    {
        ctx.reply_to_target(self.reply_to, err, self.hold_root(), None);
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
        if !self.resolved {
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
    fn insert(&mut self, hold: Option<SettlementHold>, reply_to: Source, context: Box<dyn Any + Send>) -> DispatchId {
        let id = self.mint_id();
        self.entries.insert(id, InflightEntry { hold, reply_to, context, output: None });
        id
    }

    /// Fill the worker's `output` into the named entry's completion slot.
    /// Called once, on the worker thread, under the table lock. A no-op
    /// for an unknown id (the dispatch was cancelled out of the table
    /// before the worker finished).
    fn fill_output(&mut self, id: DispatchId, output: Box<dyn Any + Send>) -> FillOutcome {
        let Some(entry) = self.entries.get_mut(&id) else {
            return FillOutcome::Missing;
        };
        if entry.output.is_some() {
            return FillOutcome::AlreadyFilled;
        }
        entry.output = Some(output);
        FillOutcome::Filled
    }

    /// Remove the named entry and hand back its parked `(hold, reply_to)`
    /// **without** any `O` / `C` downcast — the worker never ran, so there
    /// is no output to type. The spawn-error branch calls this to release
    /// the eagerly-acquired hold when arming failed: the caller drops the
    /// returned hold, settling the chain the dispatch would otherwise wedge
    /// forever. A no-op (`None`) for an unknown id.
    fn abandon(&mut self, id: DispatchId) -> Option<(Option<SettlementHold>, Source)> {
        let entry = self.entries.remove(&id)?;
        Some((entry.hold, entry.reply_to))
    }

    /// Remove the named entry and downcast its boxed `context` + filled
    /// `output` into a typed [`TaskDone`]. Returns `None` for an unknown
    /// id (cancelled / double-landed) or if the worker hasn't filled the
    /// output yet (the completion-wake must land after the fill, so this
    /// is the unknown-id case in practice) — leaving the entry intact on
    /// either miss so a parked hold is never bare-dropped. A downcast
    /// *mismatch* against a filled output is a genuine `O` / `C` wiring bug:
    /// it `debug_assert`s loudly (distinct from the benign unfilled case)
    /// and returns `None` with the entry retained.
    fn take<O: 'static, C: 'static>(&mut self, id: DispatchId) -> Option<TaskDone<O, C>> {
        let entry = self.entries.get(&id)?;
        // Peek-then-remove, the same discipline `try_take` uses: probe the
        // boxed `output` + `context` without disturbing the entry. An
        // unfilled output slot returns `None` quietly (a later wake completes
        // the still-parked entry). A type mismatch against a *filled* output
        // is a wiring bug — loud in debug, `None` in release — and never
        // removes the entry, so the parked hold stays reclaimable.
        let output = entry.output.as_deref()?;
        if output.downcast_ref::<O>().is_none() || entry.context.downcast_ref::<C>().is_none() {
            debug_assert!(
                false,
                "dispatch completion type mismatch: the task handler's (O, C) do not match the \
                 dispatch's — a wiring bug (the entry is retained, not bare-dropped)"
            );
            return None;
        }
        // Both probes passed — safe to remove and rebuild.
        let entry = self.entries.remove(&id)?;
        let InflightEntry { hold, reply_to, context, output } = entry;
        let output = output?.downcast::<O>().ok()?;
        let context = context.downcast::<C>().ok()?;
        Some(TaskDone { output: *output, context: *context, hold, reply_to, resolved: false })
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

/// Crate-internal accessors the [`NativeBinding`](crate::actor::native::binding) wraps
/// in its `Mutex<InflightTable>` field expose to
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

    pub(crate) fn dispatch_abandon(&mut self, id: DispatchId) -> Option<(Option<SettlementHold>, Source)> {
        self.abandon(id)
    }

    pub(crate) fn dispatch_try_take<O: 'static, C: 'static>(&mut self, id: DispatchId) -> Option<TaskDone<O, C>> {
        self.try_take(id)
    }
}

/// Wrap [`InflightTable`] for the [`NativeBinding`](crate::actor::native::binding)
/// field: a `Mutex` for `&self` interior mutability matching the binding's
/// other single-writer buffers.
pub(crate) type InflightLedger = Mutex<InflightTable>;

#[cfg(test)]
// Test harness constructs its own actor/inbox mailbox ids by name so the
// worker's wake push routes to a registered inbox — fixture id derivation,
// not sibling-cap addressing.
#[allow(clippy::disallowed_methods)]
#[allow(clippy::unwrap_used, reason = "test-setup unwraps: fixture construction panic on failure is the assertion")]
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
    use crate::testing::{bare_substrate, boot_authority};

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

    /// End-to-end happy path: dispatch a blocking closure, drive the
    /// completion through `take_task_done` + `resolve`, and assert the
    /// reply reached the original caller AND the hold released only after
    /// the reply was sent (the chain settles).
    #[test]
    fn dispatch_blocking_replies_and_releases_after_reply() {
        let (registry, mailer) = bare_substrate();
        let counter = Arc::clone(mailer.trace_handle().settlement_counter());

        // The original caller: a registered inbox we observe the re-reply
        // landing on (the reply routes to SourceAddr::Component(caller)).
        let (reply_tx, reply_rx) = mpsc::channel::<OwnedDispatch>();
        let caller = registry.register_inbox(&boot_authority(), "test.dispatch_blocking.caller", forward_to(reply_tx));

        // The actor's own mailbox — name-derived so the worker's wake
        // push (recipient = self_mailbox) routes to a registered inbox we
        // observe, rather than warn-dropping.
        let actor_mailbox = mailbox_id_from_name("test.dispatch_blocking.actor");
        let binding = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), actor_mailbox));
        let (wake_tx, wake_rx) = mpsc::channel::<OwnedDispatch>();
        registry.register_inbox(&boot_authority(), "test.dispatch_blocking.actor", forward_to(wake_tx));

        let root = root_id(1);
        let caller_reply_to = Source::with_correlation(SourceAddr::Component(caller), 77);

        // The dispatching handler: eager-acquire the hold, spawn the
        // worker, return.
        {
            let mut ctx = NativeCtx::new(&binding, caller_reply_to, MailId::NONE, root);
            // The bare `dispatch_blocking` now returns a `Pending<R>`
            // (ADR-0109); `R` is the declared reply kind (here `Answer`).
            let _pending = ctx.dispatch_blocking::<Answer, Answer, _>(move || Answer { value: 42 });
        }

        // The handler returned but the chain is held: settlement is
        // gated until the reply lands.
        assert_eq!(counter.held_open(root), 1, "the chain stays held after the dispatching handler returns");

        // The worker ran, filled the ledger, and pushed the wake.
        let id = await_wake(&wake_rx);
        assert_eq!(counter.held_open(root), 1, "the worker finishing does not release the chain");

        // The completion handler runs: rebuild the TaskDone and resolve.
        {
            let mut ctx = NativeCtx::new(&binding, Source::NONE, MailId::NONE, MailId::NONE);
            let done = ctx.take_task_done::<Answer, ()>(id).expect("the dispatch is in the ledger");
            assert_eq!(*done.output(), Answer { value: 42 });
            done.resolve(&mut ctx);
        }

        // The reply reached the original caller.
        let reply = reply_rx.recv_timeout(Duration::from_secs(2)).expect("the re-reply lands on the caller's mailbox");
        assert_eq!(reply.kind, Answer::ID, "reply carries the worker's output kind");
        // A Component-targeted reply is encoded through the kind codec by
        // `Mailer::send_reply` (not cast), so decode it the same way.
        let answer = Answer::decode_from_bytes(reply.payload.bytes()).expect("reply decodes");
        assert_eq!(answer, Answer { value: 42 });
        assert_eq!(reply.sender.correlation_id, 77, "the caller's correlation is echoed onto the reply");

        // Hold released after the reply — chain may settle.
        assert_eq!(counter.held_open(root), 0, "resolve releases the hold after re-replying");
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
        let (registry, mailer) = bare_substrate();
        let counter = Arc::clone(mailer.trace_handle().settlement_counter());

        let (reply_tx, reply_rx) = mpsc::channel::<OwnedDispatch>();
        let caller = registry.register_inbox(&boot_authority(), "test.dispatch_resumed.caller", forward_to(reply_tx));

        let actor_mailbox = mailbox_id_from_name("test.dispatch_resumed.actor");
        let binding = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), actor_mailbox));
        let (wake_tx, wake_rx) = mpsc::channel::<OwnedDispatch>();
        registry.register_inbox(&boot_authority(), "test.dispatch_resumed.actor", forward_to(wake_tx));

        let accept_root = root_id(1);
        let caller_reply_to = Source::with_correlation(SourceAddr::Component(caller), 77);

        // "Accept": acquire the hold on the accept root + capture the
        // caller, as a TaskQueue does when buffering an over-limit request.
        let buffered_hold = {
            let ctx = NativeCtx::new(&binding, caller_reply_to, MailId::NONE, accept_root);
            ctx.acquire_settlement_hold()
        };
        assert_eq!(counter.held_open(accept_root), 1, "the accept-time hold keeps the chain open while buffered");

        // "Drain": dispatch the buffered work from a *different* handler
        // turn — a ctx with a different root and reply target — passing the
        // captured `(hold, reply_to)` explicitly.
        let other_root = root_id(2);
        let id = {
            let mut ctx =
                NativeCtx::new(&binding, Source::with_correlation(SourceAddr::None, 99), MailId::NONE, other_root);
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
            let mut ctx = NativeCtx::new(&binding, Source::NONE, MailId::NONE, MailId::NONE);
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

        let actor_mailbox = mailbox_id_from_name("test.dispatch_blocking.actor2");
        let binding = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), actor_mailbox));
        let (wake_tx, wake_rx) = mpsc::channel::<OwnedDispatch>();
        registry.register_inbox(&boot_authority(), "test.dispatch_blocking.actor2", forward_to(wake_tx));

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
    /// settlement isn't wedged) and `debug_assert`s. Gated `#[should_panic]`
    /// — the assertion only fires in debug builds, which is where tests run.
    #[test]
    #[should_panic(expected = "TaskDone dropped without resolve")]
    #[cfg(debug_assertions)]
    fn dropping_task_done_without_resolve_releases_and_asserts() {
        let (_registry, mailer) = bare_substrate();
        let counter = Arc::clone(mailer.trace_handle().settlement_counter());

        let root = root_id(3);
        // Acquire a hold the same way dispatch does and hand it to a
        // TaskDone we then drop unresolved.
        let hold = mailer.acquire_settlement_hold(root);
        assert_eq!(counter.held_open(root), 1, "hold acquired");

        let done: TaskDone<u64, ()> =
            TaskDone { output: 1, context: (), hold, reply_to: Source::NONE, resolved: false };
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
        let (_registry, mailer) = bare_substrate();
        let counter = Arc::clone(mailer.trace_handle().settlement_counter());
        let root = root_id(4);
        let hold = mailer.acquire_settlement_hold(root);
        assert_eq!(counter.held_open(root), 1);

        let result = catch_unwind(AssertUnwindSafe(|| {
            let done: TaskDone<u64, ()> =
                TaskDone { output: 1, context: (), hold, reply_to: Source::NONE, resolved: false };
            drop(done);
        }));
        // In debug the drop asserts (unwinds); in release it doesn't.
        // Either way the hold released.
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
        let id = table.dispatch_insert(hold, Source::NONE, Box::new(()));
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
        let id = table.dispatch_insert(hold, Source::NONE, Box::new(()));
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
        let id = table.dispatch_insert(hold, Source::NONE, Box::new(()));
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
            mailer.acquire_settlement_hold(root_id(13)),
            Source::NONE,
            Box::new(String::from("typed context")),
        );

        assert_eq!(table.dispatch_fill_output(id, Box::new(Answer { value: 1 })), FillOutcome::Filled);
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
        let id = table.dispatch_insert(mailer.acquire_settlement_hold(root_id(14)), Source::NONE, Box::new(23_u16));
        assert_eq!(table.dispatch_fill_output(id, Box::new(Answer { value: 55 })), FillOutcome::Filled);

        let done = table.dispatch_take::<Answer, u16>(id).expect("matching typed take succeeds");
        assert_eq!(*done.output(), Answer { value: 55 });
        assert_eq!(*done.context(), 23);
        done.release_no_reply();
    }

    #[test]
    fn duplicate_deferred_completion_keeps_first_output_and_emits_one_wake() {
        let (registry, mailer) = bare_substrate();
        let actor_mailbox = mailbox_id_from_name("test.deferred_completion.duplicate");
        let binding = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), actor_mailbox));
        let (wake_tx, wake_rx) = mpsc::channel::<OwnedDispatch>();
        registry.register_inbox(&boot_authority(), "test.deferred_completion.duplicate", forward_to(wake_tx));

        let completion =
            binding.dispatch_arm::<Answer, _>(mailer.acquire_settlement_hold(root_id(15)), Source::NONE, ());
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
        let actor_mailbox = mailbox_id_from_name("test.deferred_completion.parent_loss");
        let binding = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), actor_mailbox));
        let (wake_tx, wake_rx) = mpsc::channel::<OwnedDispatch>();
        registry.register_inbox(&boot_authority(), "test.deferred_completion.parent_loss", forward_to(wake_tx));

        let completion = binding.dispatch_arm::<Answer, _>(mailer.acquire_settlement_hold(root), Source::NONE, ());
        assert_eq!(counter.held_open(root), 1, "arming parks the hold in the parent ledger");

        drop(binding);
        assert_eq!(counter.held_open(root), 0, "dropping the parent drops its ledger and hold");
        completion.complete(Answer { value: 1 });
        assert!(wake_rx.recv_timeout(Duration::from_millis(50)).is_err(), "parent loss emits no stale wake");
    }

    #[test]
    fn ctx_armer_captures_current_hold_reply_target_and_context() {
        let (registry, mailer) = bare_substrate();
        let counter = Arc::clone(mailer.trace_handle().settlement_counter());
        let root = root_id(17);

        let (caller_sink_tx, caller_sink_rx) = mpsc::channel::<OwnedDispatch>();
        let caller = registry.register_inbox(
            &boot_authority(),
            "test.deferred_completion.ctx_caller",
            forward_to(caller_sink_tx),
        );
        let reply_to = Source::with_correlation(SourceAddr::Component(caller), 77);

        let actor_mailbox = mailbox_id_from_name("test.deferred_completion.ctx_actor");
        let binding = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), actor_mailbox));
        let (wake_tx, wake_rx) = mpsc::channel::<OwnedDispatch>();
        registry.register_inbox(&boot_authority(), "test.deferred_completion.ctx_actor", forward_to(wake_tx));

        let completion = {
            let ctx = NativeCtx::new(&binding, reply_to, MailId::NONE, root);
            ctx.arm_deferred_completion::<Answer, _>(22_u64)
        };
        let id = completion.dispatch_id();
        assert_eq!(counter.held_open(root), 1, "ctx arming holds the current root");

        completion.complete(Answer { value: 20 });
        assert_eq!(await_wake(&wake_rx), id);
        assert_eq!(counter.held_open(root), 1, "completion fill retains the hold through TaskDone routing");

        {
            let mut ctx = NativeCtx::new(&binding, Source::NONE, MailId::NONE, MailId::NONE);
            let done = ctx.take_task_done::<Answer, u64>(id).expect("typed output and context remain parked");
            assert_eq!(*done.context(), 22);
            done.resolve_with(&mut ctx, |output, context| Answer { value: output.value + context });
        }

        let reply = caller_sink_rx.recv_timeout(Duration::from_secs(2)).expect("reply reaches the ctx-captured target");
        assert_eq!(reply.sender.correlation_id, 77);
        assert_eq!(Answer::decode_from_bytes(reply.payload.bytes()).expect("reply decodes"), Answer { value: 42 });
        assert_eq!(counter.held_open(root), 0, "TaskDone release closes the ctx-captured hold");
    }

    /// Tripwire: an owed reply that is dropped without being replied to or
    /// staged onto a successor releases its hold (so settlement isn't wedged)
    /// and `debug_assert`s. The assert is what separates [`DeferredReply`] from
    /// a plain context value — dropping a context means nothing, dropping a
    /// debt strands the caller forever — so the reshape onto the new name must
    /// keep it. Gated `#[should_panic]`: the assertion only fires in debug
    /// builds, which is where tests run.
    #[test]
    #[should_panic(expected = "DeferredReply dropped without successor staging or terminal reply")]
    #[cfg(debug_assertions)]
    fn dropping_deferred_reply_without_replying_releases_and_asserts() {
        let (_registry, mailer) = bare_substrate();
        let counter = Arc::clone(mailer.trace_handle().settlement_counter());
        let root = root_id(19);

        let owed = DeferredReply::new(mailer.acquire_settlement_hold(root), Source::NONE);
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

        let _ = catch_unwind(AssertUnwindSafe(|| drop(DeferredReply::new(hold, Source::NONE))));
        assert_eq!(counter.held_open(root), 0, "an unreplied DeferredReply releases its hold on drop");
    }

    /// Abandoning for actor close is the sanctioned no-reply path: the hold
    /// releases and the lost-reply assertion stays quiet, so a parent that
    /// disappears with pending state doesn't panic every debug build.
    #[test]
    fn abandoning_a_deferred_reply_for_actor_close_releases_without_asserting() {
        let (_registry, mailer) = bare_substrate();
        let counter = Arc::clone(mailer.trace_handle().settlement_counter());
        let root = root_id(21);

        DeferredReply::new(mailer.acquire_settlement_hold(root), Source::NONE).abandon_for_actor_close();
        assert_eq!(counter.held_open(root), 0, "actor-close abandonment releases the chain");
    }

    #[test]
    fn task_done_into_deferred_reply_keeps_one_hold_across_successor_completion() {
        let (registry, mailer) = bare_substrate();
        let counter = Arc::clone(mailer.trace_handle().settlement_counter());
        let root = root_id(18);
        let actor_mailbox = mailbox_id_from_name("test.deferred_completion.handoff");
        let binding = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), actor_mailbox));
        let (wake_tx, wake_rx) = mpsc::channel::<OwnedDispatch>();
        registry.register_inbox(&boot_authority(), "test.deferred_completion.handoff", forward_to(wake_tx));

        let first = binding.dispatch_arm::<Answer, _>(
            mailer.acquire_settlement_hold(root),
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

    #[test]
    fn dropping_armed_deferred_completion_abandons_hold_without_wake() {
        let (registry, mailer) = bare_substrate();
        let counter = Arc::clone(mailer.trace_handle().settlement_counter());
        let root = root_id(18);
        let actor_mailbox = mailbox_id_from_name("test.deferred_completion.drop");
        let binding = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), actor_mailbox));
        let (wake_tx, wake_rx) = mpsc::channel::<OwnedDispatch>();
        registry.register_inbox(&boot_authority(), "test.deferred_completion.drop", forward_to(wake_tx));

        let completion = binding.dispatch_arm::<Answer, _>(mailer.acquire_settlement_hold(root), Source::NONE, ());
        let id = completion.dispatch_id();
        assert_eq!(counter.held_open(root), 1, "arming parks the hold");

        drop(completion);

        assert_eq!(counter.held_open(root), 0, "dropping the token abandons its ledger entry and hold");
        assert!(wake_rx.recv_timeout(Duration::from_millis(50)).is_err(), "abandonment emits no wake");
        assert!(binding.dispatch_take::<Answer, ()>(id).is_none(), "the abandoned entry was removed");
    }
}
