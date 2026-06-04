// Name registries. Two tables: mailboxes (MailboxId → name + entry,
// ids derived from name via ADR-0029's stable hash) and kinds (u64
// kind id → name + descriptor, ids derived from (name, schema) via
// ADR-0030 Phase 2's `kind_id_from_parts`). Both id spaces are a pure
// function of declaration-time data — no sequential allocation, no
// registration order dependence. The registry uses interior mutability
// (`RwLock`) so mailboxes and kinds can be added at runtime —
// ADR-0010's runtime component loading mutates both tables after an
// `Arc<Registry>` has already been shared with the scheduler and hub
// client. Reads take a shared lock and are cheap; writes are rare
// (boot + load/replace/drop).

// Registry RwLock guards are intentionally held across read-then-update
// sequences — releasing the guard mid-sequence would open a TOCTOU
// window where a concurrent writer could mutate the map between the
// `get` and the dependent action. Writes are rare, contention is low.
#![allow(clippy::significant_drop_tightening)]

#[cfg(debug_assertions)]
use std::cell::Cell;
use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, OnceLock, RwLock};
#[cfg(debug_assertions)]
use std::thread;

use rustc_hash::FxHashMap;

use aether_kinds::trace::Nanos;

use crate::handle_store::schema_contains_ref;
use crate::mail::{KindId, MailId, MailRef, MailboxId, ReplyTo};
use crate::scheduler::SeizeHandle;
use std::error;

/// Deferred cell holding a `Pooled` actor's
/// [`SeizeHandle`], carried on every
/// [`MailboxEntry::Inbox`] entry (ADR-0087 §4, iamacoffeepot/aether#1135).
///
/// Registration (`register_inbox` / `try_register_inbox`) happens *before*
/// the dispatcher slot exists — the actor isn't built into a
/// `DispatcherSlot` until after `init` / `wire` — so the cell is empty
/// (`None`) at register time and the `Pooled`-branch wiring in
/// `chassis/builder.rs` + `actor/native/spawn.rs` installs the handle
/// once the slot is constructed (mirroring the `MailboxWakeSlot`
/// deferred-population pattern). The same `Arc` is shared between the
/// registry entry and the wiring caller. Closure / `Inline` handlers
/// have no slot to seize, so their cell stays empty forever and the blob
/// demuxer deposits their mail as usual.
pub(crate) type SeizeCell = Arc<OnceLock<SeizeHandle>>;

/// Test-only helper that builds a [`MailDispatch`] with empty
/// `origin` / `ReplyTo::NONE` / `MailId::NONE` defaults from the
/// minimum positional args. Used by chassis and capability tests
/// that drive a registered handler synchronously without going
/// through the full `Mail` → `Mailer::push` path.
#[cfg(test)]
pub(crate) fn test_dispatch<'a>(
    kind: KindId,
    kind_name: &'a str,
    payload: &'a [u8],
    count: u32,
) -> MailDispatch<'a> {
    MailDispatch {
        kind,
        kind_name,
        origin: None,
        sender: ReplyTo::NONE,
        payload,
        count,
        mail_id: MailId::NONE,
        root: MailId::NONE,
        parent_mail: None,
    }
}

/// Test-only owned mirror of [`test_dispatch`]. Used by tests that
/// poke an `Inbox` handler directly through
/// [`InboxHandler::enqueue`] — the trait's owned-dispatch contract
/// makes the borrowed [`test_dispatch`] unsuitable. Same defaults
/// (empty origin, `ReplyTo::NONE`, `MailId::NONE`).
///
/// Issue iamacoffeepot/aether#848 PR 2: added alongside the
/// [`OwnedDispatch`] migration so cap-side dispatcher tests stay
/// terse without each rebuilding the full struct literal.
#[cfg(test)]
pub(crate) fn test_owned_dispatch(
    kind: KindId,
    kind_name: &str,
    payload: &[u8],
    count: u32,
) -> OwnedDispatch {
    OwnedDispatch::disarmed(
        kind,
        kind_name.to_owned(),
        None,
        ReplyTo::NONE,
        MailRef::from(payload.to_vec()),
        count,
        MailId::NONE,
        MailId::NONE,
        None,
        Nanos(0),
        0,
        MailboxId(0),
    )
}

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

use aether_data::canonical::{canonical_kind_bytes, kind_id_from_parts};
use aether_data::{KindDescriptor, MailboxCategory, MailboxDescriptor, SchemaType};

/// One mail's worth of dispatch metadata handed to an
/// [`InlineHandler`]. Bundled into a single struct (rather than a
/// positional argument list) so the producer-minted ADR-0080 §1 / §5
/// lineage fields (`mail_id` / `root` / `parent_mail`) ride alongside
/// the existing envelope-style fields without exploding the closure's
/// call shape. Inbox handlers receive the owned mirror
/// [`OwnedDispatch`] instead so they can move payload into a
/// downstream channel rather than cloning the borrowed slice.
///
/// Handlers that build an [`crate::actor::native::envelope::Envelope`]
/// for an mpsc downstream copy `mail_id` / `root` / `parent_mail`
/// onto it (the dispatcher reads them to populate the per-handler
/// `NativeCtx`'s `in_flight()` accessors). Chassis-bound sinks that
/// consume mail inline can ignore the lineage triple.
#[derive(Copy, Clone, Debug)]
pub struct MailDispatch<'a> {
    /// Kind id (`K::ID`, ADR-0030 schema hash) the producer stamped.
    pub kind: KindId,
    /// Kind's registered name. Resolved by the dispatcher for
    /// diagnostic logging; handlers that only match on `kind` ignore.
    pub kind_name: &'a str,
    /// Sending mailbox's registered name, if the mail came from a
    /// component. `None` for substrate-core pushes with no sending
    /// mailbox (ADR-0011).
    pub origin: Option<&'a str>,
    /// Remote reply target of the mail (ADR-0008 / ADR-0037 /
    /// ADR-0042). Carries the correlation id for reply-routing.
    pub sender: ReplyTo,
    /// Payload bytes (the kind's encoded representation per ADR-0019).
    pub payload: &'a [u8],
    /// Kind-implied item count.
    pub count: u32,
    /// ADR-0080 §1: the producer-minted identity of this mail.
    /// `MailId::NONE` for legacy paths that haven't migrated.
    pub mail_id: MailId,
    /// ADR-0080 §5: the root of this mail's causal chain.
    pub root: MailId,
    /// ADR-0080 §5: the in-flight mail at the sender, or `None` for
    /// chassis-root sends.
    pub parent_mail: Option<MailId>,
}

/// ADR-0094 debug-only settlement-obligation guard. Rides on every
/// [`OwnedDispatch`] under `#[cfg(debug_assertions)]` and panics on
/// `Drop` if the dispatch is dropped while still *armed* — i.e.
/// neither [`OwnedDispatch::discharge`] (the consumer recorded
/// `Finished`) nor [`OwnedDispatch::mark_transferred`] (the obligation
/// moved onto a downstream envelope / into the park store) was called.
/// It converts the silent `in_flight` leak of ADR-0080 §2 (the #846 /
/// #1325 class) into an immediate, located panic naming `mail_id` +
/// `kind_name` + recipient mailbox.
///
/// Decoupled from the per-`root` `SettlementCounter` (ADR-0086): this
/// is a pure per-`OwnedDispatch` liveness assertion on the owned
/// value's lifecycle — it never reads or mutates the counter, so it
/// adds no cross-thread coupling.
///
/// `armed` is a [`Cell`] so [`OwnedDispatch::discharge`] /
/// [`OwnedDispatch::mark_transferred`] can disarm through a shared
/// `&self` (consumers hold the envelope by value but not always by
/// `mut` binding). The whole type is compiled out in release —
/// `cfg(not(debug_assertions))` carries no field and no `Drop`, so
/// `OwnedDispatch` is byte-identical to its pre-ADR-0094 shape.
#[cfg(debug_assertions)]
#[derive(Debug)]
pub(crate) struct ObligationGuard {
    mail_id: MailId,
    kind_name: String,
    mailbox: MailboxId,
    armed: Cell<bool>,
}

#[cfg(debug_assertions)]
impl ObligationGuard {
    /// Arm a fresh obligation at a mint site — the consumer that
    /// eventually drains this `OwnedDispatch` must `discharge()` it
    /// (record `Finished`) or `mark_transferred()` it (hand it onward).
    ///
    /// A `MailId::NONE` dispatch carries **no** settlement obligation:
    /// `TraceHandle::record_finished` no-ops on `MailId::NONE` (the
    /// recursion-break sentinel that chassis-internal fire-and-forget
    /// pushes — RPC self-pokes like `aether.rpc.inbound_ready`, window
    /// pushes — stamp). Arming such a dispatch would mint a *false*
    /// obligation: nothing discharges it (correctly), so the guard would
    /// then panic on drop. Mint disarmed in that case so the guard's arm
    /// condition matches `record_finished`'s NONE no-op exactly — a
    /// dispatch carries a guard obligation iff it carries a real
    /// settlement obligation (ADR-0094, issue 1326).
    fn armed(mail_id: MailId, kind_name: String, mailbox: MailboxId) -> Self {
        Self {
            mail_id,
            kind_name,
            mailbox,
            armed: Cell::new(mail_id != MailId::NONE),
        }
    }

    /// A guard that carries no obligation — test/helper mints and the
    /// disarmed result of a `Clone`.
    fn disarmed(mail_id: MailId, kind_name: String, mailbox: MailboxId) -> Self {
        Self {
            mail_id,
            kind_name,
            mailbox,
            armed: Cell::new(false),
        }
    }

    fn disarm(&self) {
        self.armed.set(false);
    }
}

#[cfg(debug_assertions)]
impl Clone for ObligationGuard {
    /// A clone is for inspection, never a second live obligation, so it
    /// is always disarmed — an accidental future clone cannot
    /// manufacture a phantom obligation (ADR-0094 `Clone` note).
    fn clone(&self) -> Self {
        Self::disarmed(self.mail_id, self.kind_name.clone(), self.mailbox)
    }
}

#[cfg(debug_assertions)]
impl Drop for ObligationGuard {
    fn drop(&mut self) {
        // Never panic-on-panic: a leaked obligation surfaced while the
        // thread is already unwinding (e.g. a test that itself paniced
        // mid-dispatch) must not abort the process and mask the real
        // failure.
        if !self.armed.get() || thread::panicking() {
            return;
        }
        panic!(
            "ADR-0094 settlement-obligation leak: OwnedDispatch dropped without \
             discharge() or mark_transferred() — mail_id={:?} kind_name={:?} mailbox={:?}. \
             The consumer must record Finished (discharge) or hand the obligation onward \
             (mark_transferred); see the InboxHandler contract in mail/registry.rs.",
            self.mail_id, self.kind_name, self.mailbox,
        );
    }
}

/// Owned mirror of [`MailDispatch`] handed to [`InboxHandler::enqueue`].
/// Built by the mailer at the `Inbox` arm by moving `mail.payload`
/// and `kind_name` out of the inbound `Mail`, so the receiving
/// closure can forward the bytes onto a downstream mpsc without an
/// intervening `payload.to_vec()` clone. The `MailDispatch<'_>`
/// borrow shape is wrong for actor-enqueue handlers — the borrow
/// can't outlive the synchronous push call, so any handler that
/// wants to enqueue must first clone. `OwnedDispatch` owns its
/// payload + `kind_name` so it can be moved cross-thread directly.
///
/// ADR-0094: under `#[cfg(debug_assertions)]` the struct carries a
/// debug-only `ObligationGuard` that panics if the dispatch is
/// dropped without [`Self::discharge`] or [`Self::mark_transferred`].
/// Construct through `OwnedDispatch::armed` (the two production mint
/// sites) or [`Self::disarmed`] (tests / helpers / lineage-free seeds)
/// rather than a struct literal so the `cfg`-gated field stays out of
/// call sites.
/// `Clone` is hand-rolled so a clone is **disarmed** (a clone is for
/// inspection, never a second obligation); release builds carry no
/// guard field and no `Drop`, so the type is byte-identical to its
/// pre-ADR-0094 shape.
//noinspection DuplicatedCode
pub struct OwnedDispatch {
    /// Kind id (`K::ID`, ADR-0030 schema hash) the producer stamped.
    pub kind: KindId,
    /// Kind's registered name. Owned `String` so the handler can move
    /// it into a downstream envelope without cloning.
    pub kind_name: String,
    /// Sending mailbox's registered name, if the mail came from a
    /// component. `None` for substrate-core pushes with no sending
    /// mailbox (ADR-0011).
    pub origin: Option<String>,
    /// Remote reply target of the mail (ADR-0008 / ADR-0037 /
    /// ADR-0042). Carries the correlation id for reply-routing.
    pub sender: ReplyTo,
    /// Payload bytes (the kind's encoded representation per ADR-0019),
    /// held as a [`MailRef`] (ADR-0087, iamacoffeepot/aether#1104).
    /// Phase 1 only ever carries `MailRef::Owned` — handlers move it
    /// into the downstream envelope rather than cloning every dispatch
    /// (the perf win called out in iamacoffeepot/aether#848); Phase 2
    /// adds the zero-copy `InRing` ref. Read via [`MailRef::bytes`].
    pub payload: MailRef,
    /// Kind-implied item count.
    pub count: u32,
    /// ADR-0080 §1: the producer-minted identity of this mail.
    /// `MailId::NONE` for legacy paths that haven't migrated.
    pub mail_id: MailId,
    /// ADR-0080 §5: the root of this mail's causal chain.
    pub root: MailId,
    /// ADR-0080 §5: the in-flight mail at the sender, or `None` for
    /// chassis-root sends.
    pub parent_mail: Option<MailId>,
    /// iamacoffeepot/aether#1134, re-anchored by
    /// iamacoffeepot/aether#1150: when the consumer side took this
    /// envelope. On the `route_mail` Inbox arm it is the **deposit**
    /// instant (placed into the recipient's inbox); on the #1135 in-place
    /// blob path the `BlobWork` demuxer stamps it with the **blob-pickup**
    /// instant instead (when the draining worker entered `run_cycle`),
    /// shared by every mail that worker dispatches. The recipient's
    /// dispatcher reads it at its `Received` hook and folds it into
    /// [`TraceEvent::Received`]'s `t_enqueue`, so the hop splits into
    /// **queued** (`t_enqueue − t_sent`) and **drain**
    /// (`t_received − t_enqueue`). `Nanos(0)` on construction sites that
    /// don't stamp it (chassis-internal / test envelopes that never enter
    /// the traced relay path).
    ///
    /// [`TraceEvent::Received`]: aether_kinds::trace::TraceEvent
    pub t_enqueue: Nanos,
    /// iamacoffeepot/aether#1134: scheduler ready-queue depth at deposit
    /// (`worker_deque::pending_depth`) — folded into
    /// [`TraceEvent::Received`]'s `enqueue_depth`. `0` off any pool worker.
    ///
    /// [`TraceEvent::Received`]: aether_kinds::trace::TraceEvent
    pub enqueue_depth: u32,
    /// ADR-0094 debug-only settlement-obligation guard. Present only
    /// under `#[cfg(debug_assertions)]`; release builds carry no field
    /// (byte-identical to the pre-ADR-0094 layout). Disarmed via
    /// [`Self::discharge`] / [`Self::mark_transferred`].
    #[cfg(debug_assertions)]
    obligation: ObligationGuard,
}

impl OwnedDispatch {
    /// Construct an `OwnedDispatch` whose ADR-0094 obligation is
    /// **armed** (debug builds): the consumer that drains it must
    /// [`Self::discharge`] or [`Self::mark_transferred`] before it
    /// drops, or the debug guard panics. The two production mint sites —
    /// `route_mail`'s `Inbox` arm and `ComponentCtx::send`'s inline
    /// `Inbox` arm — plus the #1135 in-place demux seed use this. The
    /// guard field is compiled out in release, so this is identical to
    /// a struct literal there.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn armed(
        kind: KindId,
        kind_name: String,
        origin: Option<String>,
        sender: ReplyTo,
        payload: MailRef,
        count: u32,
        mail_id: MailId,
        root: MailId,
        parent_mail: Option<MailId>,
        t_enqueue: Nanos,
        enqueue_depth: u32,
        recipient: MailboxId,
    ) -> Self {
        #[cfg(debug_assertions)]
        let obligation = ObligationGuard::armed(mail_id, kind_name.clone(), recipient);
        #[cfg(not(debug_assertions))]
        let _ = recipient;
        Self {
            kind,
            kind_name,
            origin,
            sender,
            payload,
            count,
            mail_id,
            root,
            parent_mail,
            t_enqueue,
            enqueue_depth,
            #[cfg(debug_assertions)]
            obligation,
        }
    }

    /// Construct an `OwnedDispatch` whose ADR-0094 obligation is
    /// **disarmed** — dropping it without discharge/transfer does not
    /// panic. For test/helper mints, the `noop` handler, and seeds that
    /// carry no real settlement lineage. `recipient` is recorded only so
    /// the (never-firing) guard names a mailbox if it is later armed by
    /// other means; pass `MailboxId(0)` when none is meaningful.
    ///
    /// `pub` (not `pub(crate)`) because integration tests and sibling
    /// crates' (`aether-capabilities`) tests mint dispatches directly to
    /// poke an `InboxHandler`; they have no settlement obligation, so
    /// they take the disarmed path. The armed constructor stays
    /// crate-internal — only the substrate's own mint sites arm.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn disarmed(
        kind: KindId,
        kind_name: String,
        origin: Option<String>,
        sender: ReplyTo,
        payload: MailRef,
        count: u32,
        mail_id: MailId,
        root: MailId,
        parent_mail: Option<MailId>,
        t_enqueue: Nanos,
        enqueue_depth: u32,
        recipient: MailboxId,
    ) -> Self {
        #[cfg(debug_assertions)]
        let obligation = ObligationGuard::disarmed(mail_id, kind_name.clone(), recipient);
        #[cfg(not(debug_assertions))]
        let _ = recipient;
        Self {
            kind,
            kind_name,
            origin,
            sender,
            payload,
            count,
            mail_id,
            root,
            parent_mail,
            t_enqueue,
            enqueue_depth,
            #[cfg(debug_assertions)]
            obligation,
        }
    }

    /// ADR-0094: "the obligation ends here." Records intent that the
    /// consumer is calling `Mailer::record_finished` for this
    /// `mail_id`; placed adjacent to every such call so the two cannot
    /// drift. No-op in release (no guard field). `pub` because the
    /// desktop window drain (`aether-substrate-bundle`) is a hand-rolled
    /// out-of-crate consumer that must discharge its envelopes.
    #[inline]
    pub fn discharge(&self) {
        #[cfg(debug_assertions)]
        self.obligation.disarm();
    }

    /// ADR-0094: "the obligation moves onward." The payload was relayed
    /// onto a downstream envelope (which arms its own guard) or into the
    /// park store; the downstream owner will discharge it. Also covers
    /// the lost-mail relay branches (receiver/sender dropped) where the
    /// envelope is discarded at the seam rather than held. No-op in
    /// release. `pub` for symmetry with [`Self::discharge`] — out-of-crate
    /// hand-rolled relays may need it too.
    #[inline]
    pub fn mark_transferred(&self) {
        #[cfg(debug_assertions)]
        self.obligation.disarm();
    }
}

impl Clone for OwnedDispatch {
    fn clone(&self) -> Self {
        Self {
            kind: self.kind,
            kind_name: self.kind_name.clone(),
            origin: self.origin.clone(),
            sender: self.sender,
            payload: self.payload.clone(),
            count: self.count,
            mail_id: self.mail_id,
            root: self.root,
            parent_mail: self.parent_mail,
            t_enqueue: self.t_enqueue,
            enqueue_depth: self.enqueue_depth,
            // ADR-0094: a clone is for inspection, never a second live
            // obligation — `ObligationGuard::clone` is disarmed.
            #[cfg(debug_assertions)]
            obligation: self.obligation.clone(),
        }
    }
}

impl fmt::Debug for OwnedDispatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // ADR-0094: skip the debug-only `obligation` field so `Debug`
        // is identical across debug/release builds.
        f.debug_struct("OwnedDispatch")
            .field("kind", &self.kind)
            .field("kind_name", &self.kind_name)
            .field("origin", &self.origin)
            .field("sender", &self.sender)
            .field("payload", &self.payload)
            .field("count", &self.count)
            .field("mail_id", &self.mail_id)
            .field("root", &self.root)
            .field("parent_mail", &self.parent_mail)
            .field("t_enqueue", &self.t_enqueue)
            .field("enqueue_depth", &self.enqueue_depth)
            // ADR-0094: the debug-only `obligation` guard is deliberately
            // omitted so `Debug` output is identical across debug/release.
            .finish_non_exhaustive()
    }
}

/// Synchronous handler installed under [`MailboxEntry::Inline`]. Runs
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

/// Actor-enqueue handler installed under [`MailboxEntry::Inbox`]. The
/// handler is expected to move `dispatch` onto a downstream channel
/// (typically a cap-local mpsc); the downstream consumer — an actor
/// dispatcher or chassis-side recv loop — records
/// `Received`/`Finished` per envelope. **Contract:** every
/// [`OwnedDispatch`] you receive must eventually have `Finished`
/// recorded for its `mail_id` — otherwise the chain's `in_flight`
/// leaks and any settlement subscriber hangs. iamacoffeepot/aether#846
/// is the canonical incident: a synchronous closure that captured
/// fields off the dispatch but had no downstream owner of the
/// bracket caused [`TestBench::send_bytes`] to time out at 5s once
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
/// [`TestBench::send_bytes`]: ../../../aether_substrate_bundle/test_bench/struct.TestBench.html#method.send_bytes
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

/// What a given mailbox actually is. The registry records this so the
/// scheduler can dispatch appropriately without a per-mail type check.
/// `Clone` so readers can pull the entry out from under the `RwLock`
/// guard without holding it for the duration of the handler call.
///
/// Issue 634 Phase 4 retired the dedicated `Component` variant —
/// every loaded wasm component is now a `WasmTrampoline` registered
/// here as an `Inbox` like every other actor.
///
/// Issue 838 / iamacoffeepot/aether#841: `Inbox` and `Inline` are
/// intentionally distinct variants — they *name where the handler
/// runs*. `Inbox` defers the work to an actor's dispatch thread,
/// `Inline` runs the work on the pushing thread. That decides who
/// owns the `Received`/`Finished` lifecycle bracket: the downstream
/// dispatch loop for `Inbox`, the mailer itself for `Inline`. See
/// each variant's docs and `Mailer::push`'s `route_mail` for the
/// bracket semantics.
///
/// Issue iamacoffeepot/aether#848 PR 2 + 3: the variants wrap
/// distinct trait objects ([`InboxHandler`] vs [`InlineHandler`])
/// whose dispatch types (`OwnedDispatch` vs `MailDispatch<'_>`) make
/// the wrong-shape body uneconomical to write at compile time. Not
/// a hard proof of correctness, but the affordance gap is wide
/// enough that the wrong shape genuinely doesn't fit.
#[derive(Clone)]
pub enum MailboxEntry {
    /// The handler body forwards the envelope into an actor's mpsc
    /// inbox; the actor's dispatch loop on another thread runs the
    /// work and records the `Received`/`Finished` lifecycle hooks.
    /// `Mailer::push` does NOT bracket this arm — the downstream
    /// dispatch loop owns the bracket. Installed by
    /// `claim_mailbox` / `Spawner::register_inbox` (instanced +
    /// singleton actors, including the wasm trampoline) and by the
    /// public [`Registry::register_inbox`] /
    /// [`Registry::try_register_inbox`] for callers that own a
    /// separate dispatcher loop. Handler receives [`OwnedDispatch`]
    /// so payload + `kind_name` move into the downstream envelope —
    /// see [`InboxHandler`] for the full contract.
    ///
    /// iamacoffeepot/aether#1135: `seize` is the deferred
    /// `SeizeCell` — populated once the recipient's dispatcher slot
    /// exists so the blob demuxer can resolve recipient → slot and
    /// dispatch in place (ADR-0087 §4). Empty for closure-backed inboxes
    /// (no pool slot behind them).
    Inbox {
        handler: Arc<dyn InboxHandler>,
        seize: SeizeCell,
    },
    /// The handler body does its work inline on the pushing thread;
    /// there is no actor dispatch loop behind it. `Mailer::push`
    /// brackets this arm with `Received` and `Finished` so the
    /// chain's `in_flight` balances and settlement subscribers
    /// (`SettlementRegistry`) wake (ADR-0080 §2, issue 838).
    /// Installed by [`Registry::register_inline`] /
    /// [`Registry::try_register_inline`]. Distinct from `Inbox` so
    /// the bracket isn't double-counted when the closure was an
    /// actor-enqueue (which would fire settlement prematurely).
    /// Handler receives borrowed [`MailDispatch<'_>`] — zero-copy
    /// reads; see [`InlineHandler`] for the full contract.
    Inline(Arc<dyn InlineHandler>),
    /// Mailbox has been explicitly dropped (ADR-0010). Mail addressed
    /// to a `Dropped` slot is discarded by the scheduler / ctx dispatch
    /// until the same name is re-registered, at which point the slot
    /// transitions back to `Inbox` under the same id (ADR-0029 ids
    /// are a function of name, so they're stable across drop/reload).
    Dropped,
}

pub struct Registry {
    inner: RwLock<Inner>,
    /// Issue iamacoffeepot/aether#742: notification hook fired after
    /// every successful mailbox registration. The chassis (or any
    /// hub-aware boot path) installs a closure that pushes the full
    /// inventory snapshot to the hub via `HubOutbound::egress_mailboxes_changed`,
    /// keeping the hub's per-engine mailbox cache in sync without
    /// requiring callers (chassis caps, the component-load cap) to
    /// remember to publish manually after each registration. Default
    /// `None` — registry stays decoupled from the hub layer.
    on_mailbox_change: RwLock<Option<MailboxChangeHook>>,
}

/// Issue iamacoffeepot/aether#742: hook signature. Receives the full
/// post-registration mailbox inventory so the chassis-installed
/// implementation can hand it straight to `HubOutbound::egress_mailboxes_changed`,
/// matching the existing `MailboxesChanged` wire shape (full snapshot
/// per replace, not deltas).
pub type MailboxChangeHook = Arc<dyn Fn(Vec<MailboxDescriptor>) + Send + Sync>;

/// One mailbox's bookkeeping. Grouped so a single lookup hits name,
/// entry, and any future per-mailbox fields together.
struct Mailbox {
    name: String,
    entry: MailboxEntry,
}

/// Everything [`Registry::route_lookup`] hands `route_mail` for one
/// mail, resolved under a single read guard. `ref_schema` is `Some`
/// only when the kind embeds a `Ref` (its cached `has_ref`); see the
/// method doc.
pub(crate) struct RouteLookup {
    pub(crate) entry: Option<MailboxEntry>,
    pub(crate) kind_name: String,
    pub(crate) ref_schema: Option<SchemaType>,
    /// iamacoffeepot/aether#1135: the recipient's
    /// [`SeizeHandle`], resolved under the
    /// same read guard. `Some` only when the recipient is an `Inbox`
    /// entry whose deferred [`SeizeCell`] was populated (a `Pooled`
    /// actor's slot). `None` for closure / `Inline` / `Dropped` / unknown
    /// recipients — the blob demuxer deposits their mail through
    /// `route_mail` instead of dispatching in place.
    pub(crate) seize: Option<SeizeHandle>,
}

/// One kind's bookkeeping, keyed in the registry on the hashed id.
struct KindSlot {
    name: String,
    descriptor: KindDescriptor,
    /// `schema_contains_ref(&descriptor.schema)`, computed once at
    /// registration. The route path reads this bool instead of cloning
    /// the descriptor and re-walking the schema tree per mail just to
    /// decide whether ADR-0045 handle resolution is needed.
    has_ref: bool,
}

#[derive(Default)]
struct Inner {
    /// Sparse, keyed on the deterministic `MailboxId` (ADR-0029).
    /// Registration inserts; `drop_mailbox` transitions the entry to
    /// `Dropped` so the id stays addressable until re-registered.
    mailboxes: FxHashMap<MailboxId, Mailbox>,
    /// Sparse, keyed on the `kind_id_from_parts(name, schema)` hash
    /// (ADR-0030 Phase 2). Every descriptor registered with a given
    /// (name, schema) maps to the same id everywhere it's ever
    /// computed — derive-emitted `K::ID`, hub re-derived from
    /// `KindDescriptor`, substrate boot from `descriptors::all()`.
    kinds: FxHashMap<KindId, KindSlot>,
    /// O(1) name → id reverse lookup. Kept as a parallel map rather
    /// than scanning `kinds` because the dispatch path (`reply_mail` kind
    /// validation, `hub_client` inbound-mail name→id) runs on every mail.
    /// Every insert into `kinds` mirrors into `name_index`; every slot
    /// has exactly one entry here.
    name_index: HashMap<String, KindId>,
}

/// Rejected-load error returned when a runtime kind registration
/// names an existing kind but supplies a different descriptor than the
/// one first seen. Per ADR-0010, the load fails rather than silently
/// reinterpreting; agents rename, evolve the existing descriptor, or
/// restart the substrate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KindConflict {
    pub name: String,
    pub existing: SchemaType,
    pub requested: SchemaType,
}

impl fmt::Display for KindConflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "kind {:?} already registered with a different encoding (existing={:?}, requested={:?})",
            self.name, self.existing, self.requested
        )
    }
}

impl error::Error for KindConflict {}

/// A runtime mailbox registration lost to name collision. Returned
/// from `try_register_inbox` (ADR-0010) so a runtime caller can
/// reply with an error instead of panicking. The boot path that
/// registers hard-coded mailbox names still uses `register_inbox` /
/// `register_inline` and panics — collisions there are bugs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameConflict {
    pub name: String,
}

impl fmt::Display for NameConflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "mailbox name {:?} already registered", self.name)
    }
}

impl error::Error for NameConflict {}

/// Reasons `Registry::drop_mailbox` can refuse. Distinct from the
/// post-drop dispatch log, which the scheduler handles independently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DropError {
    UnknownId(MailboxId),
    AlreadyDropped(MailboxId),
}

impl fmt::Display for DropError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownId(id) => write!(f, "unknown mailbox id {id:?}"),
            Self::AlreadyDropped(id) => write!(f, "mailbox {id:?} already dropped"),
        }
    }
}

impl error::Error for DropError {}

impl Registry {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(Inner::default()),
            on_mailbox_change: RwLock::new(None),
        }
    }

    /// Issue iamacoffeepot/aether#742: install the post-registration
    /// hook. The chassis calls this once during boot — typically
    /// inside `connect_hub_client` — to wire up automatic
    /// `MailboxesChanged` republishing for any subsequent registration
    /// (chassis-builder `.with_actor::<...>` chain, runtime
    /// `load_component`, etc.). Subsequent calls overwrite the
    /// previous hook.
    ///
    /// # Panics
    /// Panics if the `on_mailbox_change` `RwLock` is poisoned —
    /// fail-fast per ADR-0063: a poisoned lock means a prior holder
    /// panicked under the guard.
    pub fn set_on_mailbox_change(&self, hook: MailboxChangeHook) {
        *self
            .on_mailbox_change
            .write()
            .expect("on_mailbox_change lock poisoned; fail-fast per ADR-0063") = Some(hook);
    }

    /// Snapshot the inventory and invoke the hook (if installed).
    /// Called from every successful `register_inbox` /
    /// `try_register_inbox`. Snapshot is taken with the inner read
    /// lock — separate from the write lock the registration just
    /// released — so a concurrent registration sees a consistent
    /// (post-this-insert) view rather than a torn one.
    fn notify_mailbox_change(&self) {
        let hook = self
            .on_mailbox_change
            .read()
            .expect("on_mailbox_change lock poisoned; fail-fast per ADR-0063")
            .clone();
        if let Some(hook) = hook {
            hook(self.list_mailbox_descriptors());
        }
    }

    /// Insert a mailbox, allocating its id from the name hash (ADR-0029).
    /// On a `Dropped` entry at the same id (same name re-registered
    /// after a drop), the entry transitions back to live. Any other
    /// occupied entry is a collision.
    fn insert(&self, name: String, entry: MailboxEntry) -> Result<MailboxId, NameConflict> {
        let id = MailboxId::from_name(&name);
        if id == MailboxId::NONE || id == MailboxId::CHASSIS_MAILBOX_ID {
            // Sentinel collisions are reserved: NONE shadows the
            // "absent/uninit" id (Option<MailboxId> semantics break if
            // a real mailbox claims it), and CHASSIS_MAILBOX_ID is the
            // chassis-router short-circuit target — registering a real
            // handler at that name would silently shadow chassis routing
            // (issue iamacoffeepot/aether#725). Hash collision against
            // either is practically impossible at 64 bits, but the
            // CHASSIS check also blocks the obvious footgun: a caller
            // literally registering "aether.chassis".
            return Err(NameConflict { name });
        }
        let mut inner = self
            .inner
            .write()
            .expect("registry lock poisoned; fail-fast per ADR-0063");
        match inner.mailboxes.get_mut(&id) {
            Some(slot) if matches!(slot.entry, MailboxEntry::Dropped) && slot.name == name => {
                slot.entry = entry;
                Ok(id)
            }
            Some(_) => Err(NameConflict { name }),
            None => {
                inner.mailboxes.insert(id, Mailbox { name, entry });
                Ok(id)
            }
        }
    }

    /// Invalidate a live mailbox (ADR-0010). Transitions the entry
    /// to `Dropped` so dispatch-path readers can distinguish an
    /// intentional drop from an unknown id; the id itself (a function
    /// of the name per ADR-0029) stays addressable and a subsequent
    /// `try_register_inbox` / `try_register_inline` with the same
    /// name reuses it. Returns the released name on success.
    ///
    /// Issue 634 Phase 4 retired the dedicated `Component` variant,
    /// so this now drops any live `Inbox` or `Inline` mailbox.
    /// Production has exactly one caller — `WasmTrampoline`'s
    /// shutdown path transitioning its own slot — chassis-cap
    /// mailboxes never route here.
    ///
    /// # Panics
    /// Panics if the inner `RwLock` is poisoned — fail-fast per
    /// ADR-0063: a poisoned lock means a prior holder panicked under
    /// the guard.
    pub fn drop_mailbox(&self, id: MailboxId) -> Result<String, DropError> {
        let mut inner = self
            .inner
            .write()
            .expect("registry lock poisoned; fail-fast per ADR-0063");
        let Some(slot) = inner.mailboxes.get_mut(&id) else {
            return Err(DropError::UnknownId(id));
        };
        match slot.entry {
            MailboxEntry::Inbox { .. } | MailboxEntry::Inline(_) => {}
            MailboxEntry::Dropped => return Err(DropError::AlreadyDropped(id)),
        }
        slot.entry = MailboxEntry::Dropped;
        Ok(slot.name.clone())
    }

    /// Register a mailbox whose handler body forwards the envelope
    /// into an actor's mpsc inbox. The actor's dispatch loop on its
    /// own thread runs the work and records the lifecycle
    /// `Received`/`Finished` bracket — `Mailer::push` does NOT
    /// bracket this arm. Use this for any registration where a
    /// dispatch loop downstream owns the per-handler invocation
    /// (chassis caps via `claim_mailbox*`, instanced + singleton
    /// actors via the spawner).
    ///
    /// **Wrong-variant symptom.** Picking [`Self::register_inbox`]
    /// for a synchronous handler — one that does immediate work on
    /// the pushing thread rather than enqueueing onto a downstream
    /// mpsc — leaks `in_flight` forever, because nothing downstream
    /// ever fires the `Finished` half of the bracket. Settlement
    /// subscribers on the parent chain hang. iamacoffeepot/aether#846
    /// is the canonical incident: `tick_fanout_propagates_chassis_root_lineage`
    /// used `register_inbox` for a `captured.push(...)` closure
    /// (synchronous Vec append, no downstream dispatcher), and once
    /// strict settlement propagation landed in
    /// `TestBench::run_frame` the test surfaced as a 5s
    /// `SettlementTimeout`. Fix: switch to [`Self::register_inline`].
    ///
    /// The dispatch-type asymmetry helps catch this — Inbox
    /// handlers receive [`OwnedDispatch`] so moving `payload` into a
    /// channel is natural; a synchronous body that doesn't move
    /// payload reads as "I should be Inline."
    ///
    /// # Panics
    /// Panics on a name collision (or if the inner `RwLock` is
    /// poisoned) — fail-fast per ADR-0063: substrate-internal
    /// registrations should never collide; use
    /// [`Self::try_register_inbox`] when a collision is a recoverable
    /// outcome rather than a bug.
    pub fn register_inbox(
        &self,
        name: impl Into<String>,
        handler: Arc<dyn InboxHandler>,
    ) -> MailboxId {
        match self.insert(
            name.into(),
            MailboxEntry::Inbox {
                handler,
                seize: SeizeCell::default(),
            },
        ) {
            Ok(id) => {
                self.notify_mailbox_change();
                id
            }
            Err(NameConflict { name }) => {
                panic!("mailbox name already registered: {name}")
            }
        }
    }

    /// Non-panicking variant of [`Self::register_inbox`]. Returns
    /// `NameConflict` on a collision so callers that legitimately
    /// race (ADR-0070 capability boots, where the side-by-side
    /// extraction period puts legacy registrations and a new
    /// capability claim against the same mailbox during the
    /// transition diff) can surface the collision as a typed error
    /// rather than aborting the chassis.
    ///
    /// # Panics
    /// Panics if the inner `RwLock` is poisoned — fail-fast per
    /// ADR-0063: a poisoned lock means a prior holder panicked under
    /// the guard.
    pub fn try_register_inbox(
        &self,
        name: impl Into<String>,
        handler: Arc<dyn InboxHandler>,
    ) -> Result<MailboxId, NameConflict> {
        let result = self.insert(
            name.into(),
            MailboxEntry::Inbox {
                handler,
                seize: SeizeCell::default(),
            },
        );
        if result.is_ok() {
            self.notify_mailbox_change();
        }
        result
    }

    /// Issue 838: register a mailbox whose handler runs inline on
    /// the pushing thread. `Mailer::push` brackets the call with
    /// `Received`/`Finished` so the chain's `in_flight` balances
    /// and settlement subscribers
    /// ([`crate::chassis::settlement::SettlementRegistry`]) wake
    /// (ADR-0080 §2).
    ///
    /// **Wrong-variant symptom.** Picking [`Self::register_inline`]
    /// for an actor-enqueue handler — one whose body forwards onto
    /// a downstream mpsc that another thread drains — double-counts
    /// `Finished`. The mailer fires the bracket around the enqueue,
    /// then the downstream dispatcher fires its own bracket when
    /// the envelope is picked up. Settlement subscribers wake on
    /// the first `Finished` — before the actual work runs — so
    /// callers proceed past the gate while the dispatcher is still
    /// processing the mail. Fix: switch to [`Self::register_inbox`].
    ///
    /// The dispatch-type asymmetry helps catch this — Inline
    /// handlers receive borrowed [`MailDispatch<'_>`] whose
    /// `payload: &[u8]` can't be moved into an mpsc without a
    /// `to_vec()` clone; that clone is the visible "I should be
    /// Inbox" smell.
    ///
    /// # Panics
    /// Panics on a name collision (or if the inner `RwLock` is
    /// poisoned) — fail-fast per ADR-0063: substrate-internal
    /// registrations should never collide; use
    /// [`Self::try_register_inline`] when a collision is a recoverable
    /// outcome rather than a bug.
    pub fn register_inline(
        &self,
        name: impl Into<String>,
        handler: Arc<dyn InlineHandler>,
    ) -> MailboxId {
        match self.insert(name.into(), MailboxEntry::Inline(handler)) {
            Ok(id) => {
                self.notify_mailbox_change();
                id
            }
            Err(NameConflict { name }) => {
                panic!("mailbox name already registered: {name}")
            }
        }
    }

    /// Non-panicking variant of [`Self::register_inline`], symmetric
    /// with [`Self::try_register_inbox`].
    ///
    /// # Panics
    /// Panics if the inner `RwLock` is poisoned — fail-fast per
    /// ADR-0063: a poisoned lock means a prior holder panicked under
    /// the guard.
    pub fn try_register_inline(
        &self,
        name: impl Into<String>,
        handler: Arc<dyn InlineHandler>,
    ) -> Result<MailboxId, NameConflict> {
        let result = self.insert(name.into(), MailboxEntry::Inline(handler));
        if result.is_ok() {
            self.notify_mailbox_change();
        }
        result
    }

    /// Issue 607 Phase 7: fully remove a registered mailbox. Used in
    /// the chassis-boot unwind path when a singleton's `init` fails
    /// after `try_register_inbox` claimed the slot — the partial-
    /// boot state must not leak into a later cap's namespace lookup.
    /// Returns `true` if the entry existed and was a live (`Inbox`
    /// or `Inline`) variant and was removed; `false` if the id is
    /// unknown or already in `Dropped` state. Component entries go
    /// through [`Self::drop_mailbox`] (which transitions to
    /// `Dropped` rather than removing) — the lifecycle difference
    /// is intentional: components can re-register the same id after
    /// a drop, chassis-bound mailboxes are torn down on cap
    /// teardown and the id can be freshly recreated.
    pub(crate) fn remove_closure(&self, id: MailboxId) -> bool {
        let mut inner = self
            .inner
            .write()
            .expect("registry lock poisoned; fail-fast per ADR-0063");
        match inner.mailboxes.get(&id) {
            Some(slot)
                if matches!(
                    slot.entry,
                    MailboxEntry::Inbox { .. } | MailboxEntry::Inline(_)
                ) =>
            {
                inner.mailboxes.remove(&id);
                true
            }
            _ => false,
        }
    }

    /// Does a live (non-`Dropped`) mailbox exist under `name`? Returns
    /// its id if so. The id itself is deterministic (ADR-0029) —
    /// callers that just want the id without a liveness check can use
    /// `MailboxId::from_name` directly.
    ///
    /// # Panics
    /// Panics if the inner `RwLock` is poisoned — fail-fast per
    /// ADR-0063: a poisoned lock means a prior holder panicked under
    /// the guard.
    pub fn lookup(&self, name: &str) -> Option<MailboxId> {
        let id = MailboxId::from_name(name);
        let inner = self
            .inner
            .read()
            .expect("registry lock poisoned; fail-fast per ADR-0063");
        match inner.mailboxes.get(&id) {
            Some(slot) if slot.name == name && !matches!(slot.entry, MailboxEntry::Dropped) => {
                Some(id)
            }
            _ => None,
        }
    }

    /// Fetch the entry for a mailbox id. Returns an owned clone so the
    /// caller can drop the internal lock before invoking the handler
    /// (whether `Inbox` or `Inline`) — avoids holding the registry
    /// lock across arbitrary user code.
    ///
    /// # Panics
    /// Panics if the inner `RwLock` is poisoned — fail-fast per
    /// ADR-0063: a poisoned lock means a prior holder panicked under
    /// the guard.
    pub fn entry(&self, id: MailboxId) -> Option<MailboxEntry> {
        self.inner
            .read()
            .expect("registry lock poisoned; fail-fast per ADR-0063")
            .mailboxes
            .get(&id)
            .map(|m| m.entry.clone())
    }

    /// Install a `Pooled` actor's [`SeizeHandle`]
    /// onto its `Inbox` entry's deferred [`SeizeCell`] so the blob
    /// demuxer can resolve recipient → slot and dispatch in place
    /// (ADR-0087 §4, iamacoffeepot/aether#1135). Called by the
    /// `Pooled`-branch wiring in `chassis/builder.rs` +
    /// `actor/native/spawn.rs` once the dispatcher slot exists. Returns
    /// `true` on a successful install; `false` if the id isn't a live
    /// `Inbox` entry or the cell was already populated (idempotent — one
    /// install per slot in production).
    ///
    /// # Panics
    /// Panics if the inner `RwLock` is poisoned — fail-fast per
    /// ADR-0063.
    pub(crate) fn install_seize_handle(&self, id: MailboxId, handle: SeizeHandle) -> bool {
        let inner = self
            .inner
            .read()
            .expect("registry lock poisoned; fail-fast per ADR-0063");
        let Some(MailboxEntry::Inbox { seize, .. }) = inner.mailboxes.get(&id).map(|m| &m.entry)
        else {
            return false;
        };
        seize.set(handle).is_ok()
    }

    /// Hot-path combined lookup for the mailer's route step: resolves
    /// the recipient's [`MailboxEntry`], the kind's name, and whether
    /// the kind needs ADR-0045 handle resolution — all under a single
    /// read guard, where `route_mail` previously took three separate
    /// reads (`kind_descriptor` + `entry` + `kind_name`).
    ///
    /// Like [`entry`](Self::entry), everything is cloned out so the
    /// caller drops the lock before touching a handler. The common case
    /// clones only the (cheap) kind name + entry: `ref_schema` is `Some`
    /// **iff** the kind's schema embeds a `Ref` (the cached `has_ref`),
    /// so the schema is cloned only on the rare ref-carrying path rather
    /// than every mail.
    ///
    /// # Panics
    /// Panics if the inner `RwLock` is poisoned — fail-fast per
    /// ADR-0063.
    pub(crate) fn route_lookup(&self, kind: KindId, recipient: MailboxId) -> RouteLookup {
        let inner = self
            .inner
            .read()
            .expect("registry lock poisoned; fail-fast per ADR-0063");
        let kind_slot = inner.kinds.get(&kind);
        let kind_name = kind_slot.map(|s| s.name.clone()).unwrap_or_default();
        let ref_schema = kind_slot
            .filter(|s| s.has_ref)
            .map(|s| s.descriptor.schema.clone());
        let entry = inner.mailboxes.get(&recipient).map(|m| m.entry.clone());
        // iamacoffeepot/aether#1135: hand the demuxer the recipient's
        // seize handle under the same guard. Cloned out of the deferred
        // cell — `Some` only when the recipient is a `Pooled` actor whose
        // slot was wired in.
        let seize = entry.as_ref().and_then(|e| match e {
            MailboxEntry::Inbox { seize, .. } => seize.get().cloned(),
            MailboxEntry::Inline(_) | MailboxEntry::Dropped => None,
        });
        RouteLookup {
            entry,
            kind_name,
            ref_schema,
            seize,
        }
    }

    /// Reverse of `lookup`: name for a given mailbox id, or `None` if
    /// the id is unknown. Used by the closure dispatch path to stamp
    /// `origin` on observation mail (ADR-0011).
    ///
    /// # Panics
    /// Panics if the inner `RwLock` is poisoned — fail-fast per
    /// ADR-0063: a poisoned lock means a prior holder panicked under
    /// the guard.
    pub fn mailbox_name(&self, id: MailboxId) -> Option<String> {
        self.inner
            .read()
            .expect("registry lock poisoned; fail-fast per ADR-0063")
            .mailboxes
            .get(&id)
            .map(|m| m.name.clone())
    }

    /// Register a mail kind by name, defaulting the schema to `Bytes`
    /// (raw byte payload, no agent-encodable structure). The id is
    /// derived from `(name, SchemaType::Bytes)` — so the name-only path
    /// only collides with a `register_kind_with_descriptor` call that
    /// also uses the `Bytes` schema. Mostly a convenience for tests and
    /// substrate-internal registrations that don't need the hub to
    /// encode params; production init should prefer
    /// `register_kind_with_descriptor` so the descriptor stored here
    /// matches the type definition and the derived id agrees with
    /// `<K as Kind>::ID` on the guest side.
    ///
    /// # Panics
    /// Panics if the inner `RwLock` is poisoned — fail-fast per
    /// ADR-0063: a poisoned lock means a prior holder panicked under
    /// the guard. The internal `expect("Bytes default cannot produce a
    /// conflict")` is unreachable by construction.
    pub fn register_kind(&self, name: impl Into<String>) -> KindId {
        let descriptor = KindDescriptor {
            name: name.into(),
            schema: SchemaType::Bytes,
        };
        // A fresh `Bytes` descriptor can only conflict with a prior
        // `Bytes` registration under the same name — in which case the
        // schemas match and the call is idempotent. Not reachable.
        self.register_kind_internal(descriptor, /*reject_conflict=*/ false)
            .expect("Bytes default cannot produce a conflict")
    }

    /// Register a mail kind along with the descriptor the hub will
    /// use to encode agent-supplied params (ADR-0007). Per ADR-0030
    /// Phase 2:
    ///
    /// - Fresh `(name, schema)` hash → insert, return the id.
    /// - Existing id with identical descriptor → return the id
    ///   (idempotent — same kind registered twice, e.g. boot + load).
    /// - Existing id with a different descriptor → `KindConflict`. At
    ///   64-bit hash width this is only reachable via a genuine hash
    ///   collision between two distinct kinds; loud failure rather
    ///   than silent data corruption.
    ///
    /// Used by substrate boot (`descriptors::all()`) and `load_component`.
    ///
    /// # Panics
    /// Panics if the inner `RwLock` is poisoned — fail-fast per
    /// ADR-0063: a poisoned lock means a prior holder panicked under
    /// the guard.
    pub fn register_kind_with_descriptor(
        &self,
        descriptor: KindDescriptor,
    ) -> Result<KindId, KindConflict> {
        self.register_kind_internal(descriptor, /*reject_conflict=*/ true)
    }

    fn register_kind_internal(
        &self,
        descriptor: KindDescriptor,
        reject_conflict: bool,
    ) -> Result<KindId, KindConflict> {
        let id = KindId(kind_id_from_parts(&descriptor.name, &descriptor.schema));
        let mut inner = self
            .inner
            .write()
            .expect("registry lock poisoned; fail-fast per ADR-0063");
        if let Some(slot) = inner.kinds.get(&id) {
            if reject_conflict
                && canonical_kind_bytes(&slot.descriptor.name, &slot.descriptor.schema)
                    != canonical_kind_bytes(&descriptor.name, &descriptor.schema)
            {
                // Same 64-bit id but distinct canonical bytes — a real
                // hash collision, keep the loud failure. Comparing
                // canonical bytes (not `SchemaType` PartialEq) means
                // nominal-only differences — named fields vs stripped
                // names from a manifest round-trip — are treated as
                // identical, since the canonical form is exactly the
                // structure the id hashes over.
                return Err(KindConflict {
                    name: descriptor.name,
                    existing: slot.descriptor.schema.clone(),
                    requested: descriptor.schema,
                });
            }
            return Ok(id);
        }
        inner.name_index.insert(descriptor.name.clone(), id);
        let has_ref = schema_contains_ref(&descriptor.schema);
        inner.kinds.insert(
            id,
            KindSlot {
                name: descriptor.name.clone(),
                descriptor,
                has_ref,
            },
        );
        Ok(id)
    }

    /// Look up a kind's id by its canonical name. Under hashed ids the
    /// id is a function of `(name, schema)` — so this only finds a
    /// match if `register_kind_with_descriptor` was called with the
    /// exact descriptor the caller is thinking of. Primarily used by
    /// the hub-inbound dispatch path, which needs to convert an
    /// incoming `kind_name` back to the registered id.
    ///
    /// # Panics
    /// Panics if the inner `RwLock` is poisoned — fail-fast per
    /// ADR-0063: a poisoned lock means a prior holder panicked under
    /// the guard.
    pub fn kind_id(&self, name: &str) -> Option<KindId> {
        self.inner
            .read()
            .expect("registry lock poisoned; fail-fast per ADR-0063")
            .name_index
            .get(name)
            .copied()
    }

    /// Reverse of `kind_id`: name for a given id, or `None` if the id
    /// isn't registered. Used by the dispatch path to hand mailbox
    /// closure handlers a kind name without them keeping their own
    /// map.
    ///
    /// # Panics
    /// Panics if the inner `RwLock` is poisoned — fail-fast per
    /// ADR-0063: a poisoned lock means a prior holder panicked under
    /// the guard.
    pub fn kind_name(&self, kind: KindId) -> Option<String> {
        self.inner
            .read()
            .expect("registry lock poisoned; fail-fast per ADR-0063")
            .kinds
            .get(&kind)
            .map(|s| s.name.clone())
    }

    /// The descriptor stored for a given kind id, or `None` if the id
    /// isn't registered. Returned as an owned clone so callers don't
    /// hold the read lock while inspecting the encoding.
    ///
    /// # Panics
    /// Panics if the inner `RwLock` is poisoned — fail-fast per
    /// ADR-0063: a poisoned lock means a prior holder panicked under
    /// the guard.
    pub fn kind_descriptor(&self, kind: KindId) -> Option<KindDescriptor> {
        self.inner
            .read()
            .expect("registry lock poisoned; fail-fast per ADR-0063")
            .kinds
            .get(&kind)
            .map(|s| s.descriptor.clone())
    }

    /// Snapshot of every kind descriptor currently registered. Sorted
    /// by name so the hub sees a deterministic ordering (ids are a
    /// hash of declaration-time data, so sorting on id would scramble
    /// unrelated kinds; name order preserves a human-readable grouping).
    /// Used by the control plane to ship an authoritative view to the
    /// hub after a runtime load or replace (ADR-0010 §4).
    ///
    /// # Panics
    /// Panics if the inner `RwLock` is poisoned — fail-fast per
    /// ADR-0063: a poisoned lock means a prior holder panicked under
    /// the guard.
    pub fn list_kind_descriptors(&self) -> Vec<KindDescriptor> {
        let mut out: Vec<KindDescriptor> = self
            .inner
            .read()
            .expect("registry lock poisoned; fail-fast per ADR-0063")
            .kinds
            .values()
            .map(|s| s.descriptor.clone())
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Snapshot of every mailbox descriptor currently registered, plus
    /// a synthetic entry for the chassis-router sentinel
    /// (`aether.chassis` / [`MailboxId::CHASSIS_MAILBOX_ID`]). Sorted
    /// by name. Used by the hub-client handshake to ship the
    /// authoritative inventory in `Hello.mailboxes`, and by the
    /// component cap to re-ship via `MailboxesChanged` after a load
    /// registers a new trampoline mailbox (issue iamacoffeepot/aether#730).
    ///
    /// `Dropped` entries are included with their last-known name so a
    /// trace tool can still resolve a mailbox that died after the
    /// trace was captured. Categorisation is a pure function of the
    /// mailbox name (`categorise_name`); the registry stores no
    /// per-mailbox category state.
    ///
    /// # Panics
    /// Panics if the inner `RwLock` is poisoned — fail-fast per
    /// ADR-0063: a poisoned lock means a prior holder panicked under
    /// the guard.
    pub fn list_mailbox_descriptors(&self) -> Vec<MailboxDescriptor> {
        let mut out: Vec<MailboxDescriptor> = self
            .inner
            .read()
            .expect("registry lock poisoned; fail-fast per ADR-0063")
            .mailboxes
            .iter()
            .map(|(id, m)| MailboxDescriptor {
                id: *id,
                name: m.name.clone(),
                category: categorise_mailbox_name(&m.name),
            })
            .collect();
        out.push(MailboxDescriptor {
            id: MailboxId::CHASSIS_MAILBOX_ID,
            name: "aether.chassis".to_owned(),
            category: Some(MailboxCategory::ChassisSentinel),
        });
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Number of registered mailbox entries (live + `Dropped`).
    ///
    /// # Panics
    /// Panics if the inner `RwLock` is poisoned — fail-fast per
    /// ADR-0063: a poisoned lock means a prior holder panicked under
    /// the guard.
    pub fn len(&self) -> usize {
        self.inner
            .read()
            .expect("registry lock poisoned; fail-fast per ADR-0063")
            .mailboxes
            .len()
    }

    /// `true` when no mailbox has ever been registered.
    ///
    /// # Panics
    /// Panics if the inner `RwLock` is poisoned — fail-fast per
    /// ADR-0063: a poisoned lock means a prior holder panicked under
    /// the guard.
    pub fn is_empty(&self) -> bool {
        self.inner
            .read()
            .expect("registry lock poisoned; fail-fast per ADR-0063")
            .mailboxes
            .is_empty()
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

/// Categorise a mailbox name for the inventory snapshot (issue 730).
/// Pure function of the name string. The hub uses this categorisation
/// (round-tripped through `MailboxDescriptor.category`) to render
/// type-prefixed labels in trace tool output.
fn categorise_mailbox_name(name: &str) -> Option<MailboxCategory> {
    if name == "aether.chassis" {
        // Reachable via [`MailboxId::CHASSIS_MAILBOX_ID`] short-circuit;
        // never registered with a real handler. The synthetic entry in
        // [`Registry::list_mailbox_descriptors`] uses the same
        // categorisation so re-registration would be redundant.
        Some(MailboxCategory::ChassisSentinel)
    // Literal kept in sync with `aether_capabilities::trampoline::WasmTrampoline::NAMESPACE`
    // (issue 654 made that the single source of truth). Substrate can't
    // import from capabilities (wrong dep direction), so this routing
    // categorisation duplicates the prefix; if it drifts, every
    // loaded-component test fails immediately because the mailbox
    // categorisation no longer matches.
    } else if name.starts_with("aether.component.trampoline:") {
        Some(MailboxCategory::Trampoline)
    } else if name.starts_with("aether.") {
        // Chassis caps and substrate-owned actors live under the
        // `aether.` namespace (post-ADR-0074). Anything else is
        // user-space and falls through to `None`.
        Some(MailboxCategory::Actor)
    } else {
        None
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "test-setup unwraps: fixture construction and decode panic on failure is the assertion"
)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;
    use std::sync::Mutex;

    /// ADR-0094: a fresh armed [`OwnedDispatch`] panics on drop if it was
    /// neither discharged nor transferred — the headline regression gate
    /// for the #846 / #1325 dropped-bracket class. Debug-only (the guard
    /// is compiled out in release).
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "settlement-obligation leak")]
    fn armed_dispatch_panics_if_dropped_without_discharge() {
        let env = OwnedDispatch::armed(
            KindId(7),
            "aether.window.set_mode".to_owned(),
            None,
            ReplyTo::NONE,
            MailRef::from(vec![1u8, 2, 3]),
            1,
            MailId::new(MailboxId(42), 9),
            MailId::new(MailboxId(42), 9),
            None,
            Nanos(0),
            0,
            MailboxId(42),
        );
        // Drop without discharge/transfer — the InboxHandler contract
        // violation. The panic message names the offending seam.
        drop(env);
    }

    /// ADR-0094: the panic message names `mail_id` + `kind_name` so the
    /// leaking seam is locatable, not anonymous.
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "aether.window.set_mode")]
    fn armed_dispatch_panic_names_the_kind() {
        let env = OwnedDispatch::armed(
            KindId(7),
            "aether.window.set_mode".to_owned(),
            None,
            ReplyTo::NONE,
            MailRef::from(Vec::new()),
            1,
            MailId::new(MailboxId(1), 1),
            MailId::new(MailboxId(1), 1),
            None,
            Nanos(0),
            0,
            MailboxId(1),
        );
        drop(env);
    }

    /// ADR-0094: an armed dispatch that is `discharge()`d before drop
    /// does NOT panic — the consumer recorded `Finished`.
    #[test]
    fn discharged_dispatch_does_not_panic() {
        let env = OwnedDispatch::armed(
            KindId(7),
            "aether.fs.read".to_owned(),
            None,
            ReplyTo::NONE,
            MailRef::from(Vec::new()),
            1,
            MailId::new(MailboxId(2), 2),
            MailId::new(MailboxId(2), 2),
            None,
            Nanos(0),
            0,
            MailboxId(2),
        );
        env.discharge();
        drop(env);
    }

    /// ADR-0094: an armed dispatch that is `mark_transferred()` before
    /// drop does NOT panic — the obligation moved onward.
    #[test]
    fn transferred_dispatch_does_not_panic() {
        let env = OwnedDispatch::armed(
            KindId(7),
            "aether.fs.write".to_owned(),
            None,
            ReplyTo::NONE,
            MailRef::from(Vec::new()),
            1,
            MailId::new(MailboxId(3), 3),
            MailId::new(MailboxId(3), 3),
            None,
            Nanos(0),
            0,
            MailboxId(3),
        );
        env.mark_transferred();
        drop(env);
    }

    /// ADR-0094: a disarmed mint (the test/helper path) never panics on
    /// drop even without discharge.
    #[test]
    fn disarmed_dispatch_does_not_panic() {
        let env = OwnedDispatch::disarmed(
            KindId(7),
            "aether.tick".to_owned(),
            None,
            ReplyTo::NONE,
            MailRef::from(Vec::new()),
            1,
            MailId::NONE,
            MailId::NONE,
            None,
            Nanos(0),
            0,
            MailboxId(0),
        );
        drop(env);
    }

    /// ADR-0094 `Clone` note: cloning an armed dispatch produces a
    /// **disarmed** clone (a clone is for inspection, never a second
    /// obligation), so dropping the clone does not panic. The original is
    /// discharged to keep the test itself clean.
    #[cfg(debug_assertions)]
    #[test]
    fn clone_of_armed_dispatch_is_disarmed() {
        let env = OwnedDispatch::armed(
            KindId(7),
            "aether.tick".to_owned(),
            None,
            ReplyTo::NONE,
            MailRef::from(vec![9u8]),
            1,
            MailId::new(MailboxId(4), 4),
            MailId::new(MailboxId(4), 4),
            None,
            Nanos(0),
            0,
            MailboxId(4),
        );
        let clone = env.clone();
        // The clone carries no obligation — dropping it must not panic.
        drop(clone);
        // Original still armed: discharge so the test exits cleanly.
        env.discharge();
    }

    /// ADR-0094 issue 1326: arming a `MailId::NONE` dispatch mints **no**
    /// obligation — `record_finished` no-ops on `MailId::NONE`, so the
    /// chassis-internal fire-and-forget pushes that stamp it (RPC
    /// self-pokes like `aether.rpc.inbound_ready`, window pushes) route
    /// through the armed `Inbox` arm but never discharge. The arm site is
    /// unconditional; `ObligationGuard::armed` disarms on NONE so the
    /// guard's arm condition matches `record_finished` exactly. Dropping
    /// such a dispatch without discharge must NOT panic.
    #[cfg(debug_assertions)]
    #[test]
    fn armed_none_mail_id_dispatch_does_not_panic() {
        let env = OwnedDispatch::armed(
            KindId(7),
            "aether.rpc.inbound_ready".to_owned(),
            None,
            ReplyTo::NONE,
            MailRef::from(Vec::new()),
            1,
            MailId::NONE,
            MailId::NONE,
            None,
            Nanos(0),
            0,
            MailboxId(63),
        );
        // No discharge / transfer — a NONE dispatch carries no obligation,
        // so the guard must be disarmed and the drop must be silent.
        drop(env);
    }

    /// ADR-0094 no-leak side of the headline coverage: routing a real mail
    /// through the standard actor dispatcher (`DispatcherSlot::dispatch_one`
    /// via `register_inbox` + a seized run) discharges the obligation, so
    /// no guard panic fires on the production drain path.
    #[test]
    fn standard_inbox_handler_relay_does_not_panic() {
        // The `register_inbox` relay closure moves the armed dispatch onto
        // a channel (a transfer); the channel's receiver here drains and
        // discharges it explicitly, mirroring `dispatch_one`. A panic here
        // would mean the relay/transfer path false-positives.
        use std::sync::mpsc;

        let (tx, rx) = mpsc::channel::<OwnedDispatch>();
        let handler: Arc<dyn InboxHandler> = Arc::new(move |dispatch: OwnedDispatch| {
            // Relay: the value moves onto the channel, carrying its
            // obligation. No discharge here — the drainer below owns it.
            let _ = tx.send(dispatch);
        });
        // Mint armed exactly as `route_mail`'s Inbox arm does.
        handler.enqueue(OwnedDispatch::armed(
            KindId(11),
            "aether.audio.note_on".to_owned(),
            None,
            ReplyTo::NONE,
            MailRef::from(vec![0u8]),
            1,
            MailId::new(MailboxId(5), 5),
            MailId::new(MailboxId(5), 5),
            None,
            Nanos(0),
            0,
            MailboxId(5),
        ));
        let env = rx.recv().expect("relay forwarded the dispatch");
        // Downstream dispatcher discharges (the `dispatch_one` template).
        env.discharge();
        drop(env);
    }

    #[test]
    fn register_and_lookup_closure_mailbox() {
        let r = Registry::new();
        let id = r.register_inbox("physics", noop_handler());
        assert_eq!(id, MailboxId::from_name("physics"));
        assert_eq!(r.lookup("physics"), Some(id));
        assert!(matches!(r.entry(id), Some(MailboxEntry::Inbox { .. })));
    }

    /// iamacoffeepot/aether#1135: a `Pooled` actor's `Inbox` entry exposes
    /// a live seize handle once the slot is wired in; a closure-backed
    /// inbox (no slot) exposes none.
    #[test]
    fn pooled_inbox_exposes_seize_handle_closure_does_not() {
        use crate::scheduler::{BatchBudget, CycleResult, Drainable, SlotState};
        use std::any::Any;

        // Minimal `Drainable` carrying a real `SlotState` so the installed
        // seize handle can drive the `Idle → Running` CAS.
        struct StatefulSlot {
            state: Arc<SlotState>,
        }
        impl Drainable for StatefulSlot {
            fn run_cycle(&self, _budget: BatchBudget) -> CycleResult {
                CycleResult::Idle
            }
            fn as_any(&self) -> &dyn Any {
                self
            }
        }

        let r = Registry::new();
        let kind = r.register_kind("test.seize.kind");

        // Closure-backed inbox: no slot, so no seize handle ever resolves.
        let closure_id = r.register_inbox("closure", noop_handler());
        assert!(
            r.route_lookup(kind, closure_id).seize.is_none(),
            "a closure-backed inbox exposes no seize handle"
        );

        // A `Pooled`-shaped inbox: empty before the slot is wired, then a
        // live handle after `install_seize_handle`.
        let pooled_id = r.register_inbox("pooled", noop_handler());
        assert!(
            r.route_lookup(kind, pooled_id).seize.is_none(),
            "the seize cell is empty until the Pooled slot is wired"
        );

        let slot = Arc::new(StatefulSlot {
            state: Arc::new(SlotState::new()),
        });
        let slot_dyn: Arc<dyn Drainable> = slot.clone();
        let installed = r.install_seize_handle(
            pooled_id,
            SeizeHandle::new(Arc::clone(&slot.state), Arc::downgrade(&slot_dyn)),
        );
        assert!(installed, "install lands on a live Inbox entry");

        let resolved = r
            .route_lookup(kind, pooled_id)
            .seize
            .expect("Pooled inbox now exposes a seize handle");
        // The handle is live: it wins the `Idle → Running` seize CAS and
        // upgrades to the same slot.
        assert!(
            resolved.try_seize().is_some(),
            "the resolved handle seizes a live slot"
        );
        let _ = slot_dyn;
    }

    #[test]
    fn closure_handler_runs_on_call() {
        let r = Registry::new();
        let counter = Arc::new(AtomicU32::new(0));
        let c2 = Arc::clone(&counter);
        let id = r.register_inbox(
            "heartbeat",
            Arc::new(move |dispatch: OwnedDispatch| {
                c2.fetch_add(dispatch.count, Ordering::SeqCst);
            }),
        );
        let Some(MailboxEntry::Inbox { handler: h, .. }) = r.entry(id) else {
            panic!("expected closure entry")
        };
        // Test-side id is irrelevant — the handler ignores it.
        h.enqueue(test_owned_dispatch(KindId(0), "aether.tick", &[], 7));
        h.enqueue(OwnedDispatch::disarmed(
            KindId(0),
            "aether.tick".to_owned(),
            Some("physics".to_owned()),
            ReplyTo::NONE,
            MailRef::from(Vec::new()),
            3,
            MailId::NONE,
            MailId::NONE,
            None,
            Nanos(0),
            0,
            MailboxId(0),
        ));
        assert_eq!(counter.load(Ordering::SeqCst), 10);
    }

    #[test]
    fn mailbox_ids_are_name_derived() {
        let r = Registry::new();
        let a = r.register_inbox("a", noop_handler());
        let b = r.register_inbox("b", noop_handler());
        let c = r.register_inbox("c", noop_handler());
        assert_eq!(a, MailboxId::from_name("a"));
        assert_eq!(b, MailboxId::from_name("b"));
        assert_eq!(c, MailboxId::from_name("c"));
        // All three distinct names produce distinct ids.
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_ne!(a, c);
        assert_eq!(r.len(), 3);
    }

    #[test]
    #[should_panic(expected = "mailbox name already registered")]
    fn duplicate_name_panics() {
        let r = Registry::new();
        r.register_inbox("x", noop_handler());
        r.register_inbox("x", noop_handler());
    }

    #[test]
    fn lookup_missing_returns_none() {
        let r = Registry::new();
        assert!(r.lookup("nope").is_none());
        assert!(r.entry(MailboxId(42)).is_none());
    }

    #[test]
    fn mailbox_name_reverse_lookup() {
        let r = Registry::new();
        let a = r.register_inbox("physics", noop_handler());
        let b = r.register_inbox("graphics", noop_handler());
        assert_eq!(r.mailbox_name(a).as_deref(), Some("physics"));
        assert_eq!(r.mailbox_name(b).as_deref(), Some("graphics"));
        assert!(r.mailbox_name(MailboxId(999)).is_none());
    }

    #[test]
    fn kind_ids_are_derived_from_name_and_schema() {
        let r = Registry::new();
        let a = r.register_kind("aether.tick");
        let b = r.register_kind("aether.key");
        let c = r.register_kind("hello.npc_health");
        // Ids are the fnv1a hash of canonical (name, schema) bytes —
        // distinct names under the same default schema must produce
        // distinct ids, and matching the expected const derivation
        // pins the hash contract with the derive.
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_ne!(a, c);
        assert_eq!(
            a,
            KindId(kind_id_from_parts("aether.tick", &SchemaType::Bytes))
        );
    }

    #[test]
    fn kind_registration_is_idempotent() {
        let r = Registry::new();
        let first = r.register_kind("aether.tick");
        let second = r.register_kind("aether.tick");
        assert_eq!(first, second);
        // Different name produces a different id — the id is a pure
        // function of the input, not an allocation order.
        assert_ne!(r.register_kind("aether.key"), first);
    }

    #[test]
    fn kind_id_lookup() {
        let r = Registry::new();
        let id = r.register_kind("aether.tick");
        assert_eq!(r.kind_id("aether.tick"), Some(id));
        assert!(r.kind_id("absent").is_none());
    }

    #[test]
    fn kind_name_reverse_lookup() {
        let r = Registry::new();
        let a = r.register_kind("aether.tick");
        let b = r.register_kind("aether.key");
        assert_eq!(r.kind_name(a).as_deref(), Some("aether.tick"));
        assert_eq!(r.kind_name(b).as_deref(), Some("aether.key"));
        assert!(r.kind_name(KindId(999)).is_none());
    }

    fn unit_desc(name: &str) -> KindDescriptor {
        KindDescriptor {
            name: name.to_string(),
            schema: SchemaType::Unit,
        }
    }

    fn cast_struct_desc(name: &str) -> KindDescriptor {
        use aether_data::{NamedField, Primitive};
        KindDescriptor {
            name: name.to_string(),
            schema: SchemaType::Struct {
                repr_c: true,
                fields: vec![NamedField {
                    name: "x".into(),
                    ty: SchemaType::Scalar(Primitive::U32),
                }]
                .into(),
            },
        }
    }

    #[test]
    fn register_kind_with_descriptor_stores_schema() {
        let r = Registry::new();
        let id = r
            .register_kind_with_descriptor(cast_struct_desc("aether.foo"))
            .expect("fresh name");
        let stored = r.kind_descriptor(id).expect("descriptor present");
        assert_eq!(stored.schema, cast_struct_desc("aether.foo").schema);
    }

    #[test]
    fn register_kind_with_descriptor_is_idempotent_on_match() {
        let r = Registry::new();
        let first = r
            .register_kind_with_descriptor(cast_struct_desc("aether.foo"))
            .expect("first");
        let second = r
            .register_kind_with_descriptor(cast_struct_desc("aether.foo"))
            .expect("same schema should succeed");
        assert_eq!(first, second);
    }

    /// The first registration stores the schema with named fields
    /// (e.g. substrate boot via `aether_kinds::descriptors::all()`); a
    /// second registration of the same structural kind with stripped
    /// names (e.g. reconstructed from a component's `aether.kinds`
    /// canonical bytes) must be accepted as idempotent because both
    /// produce the same kind id. This is the path `#[actor]`
    /// consumer-crate retention relies on for cross-crate kinds that
    /// duplicate boot-registered ones.
    #[test]
    fn register_kind_with_descriptor_accepts_nominal_only_differences() {
        use aether_data::{NamedField, Primitive};
        let r = Registry::new();
        let named_id = r
            .register_kind_with_descriptor(cast_struct_desc("aether.foo"))
            .expect("first");

        let unnamed = KindDescriptor {
            name: "aether.foo".into(),
            schema: SchemaType::Struct {
                repr_c: true,
                fields: vec![NamedField {
                    name: "".into(),
                    ty: SchemaType::Scalar(Primitive::U32),
                }]
                .into(),
            },
        };
        let unnamed_id = r
            .register_kind_with_descriptor(unnamed)
            .expect("same canonical bytes = same id = idempotent");
        assert_eq!(named_id, unnamed_id);

        // Named version stays in the stored slot — first writer wins.
        let stored = r.kind_descriptor(named_id).expect("still there");
        if let SchemaType::Struct { fields, .. } = &stored.schema {
            assert_eq!(fields[0].name, "x");
        } else {
            panic!("expected struct schema");
        }
    }

    #[test]
    fn register_kind_with_descriptor_distinct_schemas_take_distinct_ids() {
        // Pre-ADR-0030-Phase-2 behavior was: same name + different
        // schema = `KindConflict`. Under hashed ids the id IS the
        // `(name, schema)` pair, so two schemas under the same name
        // land in two separate slots — conflict is only reachable via
        // a genuine hash collision. Document the post-Phase-2 shape
        // and let the conflict path stay exercised via the
        // `_is_idempotent_on_match` test (same-id reentry).
        let r = Registry::new();
        let unit_id = r
            .register_kind_with_descriptor(unit_desc("aether.foo"))
            .expect("first");
        let struct_id = r
            .register_kind_with_descriptor(cast_struct_desc("aether.foo"))
            .expect("second — different schema, no conflict under hashed ids");
        assert_ne!(unit_id, struct_id);
        assert_eq!(r.kind_descriptor(unit_id).unwrap().schema, SchemaType::Unit);
        assert!(matches!(
            r.kind_descriptor(struct_id).unwrap().schema,
            SchemaType::Struct { .. }
        ));
    }

    #[test]
    fn register_kind_defaults_to_bytes() {
        let r = Registry::new();
        let id = r.register_kind("aether.bar");
        let stored = r.kind_descriptor(id).expect("descriptor present");
        assert_eq!(stored.schema, SchemaType::Bytes);
    }

    #[test]
    fn name_only_and_with_descriptor_resolve_to_distinct_ids() {
        // Under hashed ids the id is a function of (name, schema).
        // The same name registered with two different schemas —
        // `Bytes` (via `register_kind`) and a real struct (via
        // `register_kind_with_descriptor`) — produces two *different*
        // ids, each stored under its own slot. `kind_id(name)` returns
        // whichever id was written to `name_index` most recently; this
        // is a test-only hazard and production callers go through
        // `register_kind_with_descriptor` exclusively.
        let r = Registry::new();
        let real = r
            .register_kind_with_descriptor(cast_struct_desc("aether.foo"))
            .expect("real schema");
        let bytes = r.register_kind("aether.foo");
        assert_ne!(real, bytes);
        assert!(matches!(
            r.kind_descriptor(real).unwrap().schema,
            SchemaType::Struct { .. }
        ));
        assert!(matches!(
            r.kind_descriptor(bytes).unwrap().schema,
            SchemaType::Bytes,
        ));
    }

    #[test]
    fn try_register_inbox_is_non_panicking_on_collision() {
        let r = Registry::new();
        let first = r
            .try_register_inbox("loaded", noop_handler())
            .expect("fresh name");
        let err = r
            .try_register_inbox("loaded", noop_handler())
            .expect_err("collision must not panic");
        assert_eq!(err.name, "loaded");
        assert_eq!(r.lookup("loaded"), Some(first));
        // Entries count unchanged after the failed second attempt.
        assert_eq!(r.len(), 1);
    }

    /// Issue iamacoffeepot/aether#725: registering a real handler at the
    /// reserved `"aether.chassis"` name would silently shadow the
    /// chassis-router short-circuit in `Mailer::route_mail` (mail to
    /// `CHASSIS_MAILBOX_ID` never reaches the registry). Reject at the
    /// registration boundary so the routing path stays unambiguous.
    #[test]
    fn try_register_inbox_rejects_reserved_chassis_name() {
        let r = Registry::new();
        let err = r
            .try_register_inbox("aether.chassis", noop_handler())
            .expect_err("reserved name must reject");
        assert_eq!(err.name, "aether.chassis");
        assert_eq!(r.len(), 0);
    }

    #[test]
    fn drop_mailbox_frees_name_and_marks_entry_dropped() {
        let r = Registry::new();
        let id = r.try_register_inbox("loaded", noop_handler()).unwrap();
        let name = r.drop_mailbox(id).expect("drop");
        assert_eq!(name, "loaded");
        assert!(r.lookup("loaded").is_none(), "name should be reusable");
        assert!(
            matches!(r.entry(id), Some(MailboxEntry::Dropped)),
            "entry must mark id as dropped"
        );
        // Under ADR-0029 the id is a function of the name, so a
        // re-register produces the *same* id and flips the entry back
        // to `Component`.
        let reloaded = r.try_register_inbox("loaded", noop_handler()).unwrap();
        assert_eq!(reloaded, id);
        assert_eq!(r.lookup("loaded"), Some(reloaded));
        assert!(matches!(
            r.entry(reloaded),
            Some(MailboxEntry::Inbox { .. })
        ));
    }

    #[test]
    fn drop_mailbox_rejects_unknown_and_repeat() {
        let r = Registry::new();
        assert!(matches!(
            r.drop_mailbox(MailboxId(999)),
            Err(DropError::UnknownId(_))
        ));
        let c = r.try_register_inbox("x", noop_handler()).unwrap();
        r.drop_mailbox(c).unwrap();
        assert!(matches!(
            r.drop_mailbox(c),
            Err(DropError::AlreadyDropped(_))
        ));
    }

    /// Issue iamacoffeepot/aether#730: `list_mailbox_descriptors`
    /// snapshots the table sorted by name, categorises each entry by
    /// its name prefix, and inserts a synthetic `ChassisSentinel`
    /// entry under `aether.chassis` (which is never a real registry
    /// row — `insert` rejects the reserved name).
    #[test]
    fn list_mailbox_descriptors_snapshots_sorted_with_categories() {
        let r = Registry::new();
        r.register_inbox("aether.input", noop_handler());
        r.register_inbox("aether.component.trampoline:cam", noop_handler());
        r.register_inbox("user_thing", noop_handler());

        let snap = r.list_mailbox_descriptors();
        // Four entries: 3 registered + 1 synthetic chassis sentinel.
        assert_eq!(snap.len(), 4, "got: {snap:#?}");

        // Sorted by name.
        let names: Vec<&str> = snap.iter().map(|d| d.name.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted, "snapshot must be sorted by name");

        // Each name maps to the expected category.
        let cat = |n: &str| {
            snap.iter()
                .find(|d| d.name == n)
                .and_then(|d| d.category)
                .unwrap_or_else(|| panic!("missing entry for {n}"))
        };
        assert_eq!(cat("aether.chassis"), MailboxCategory::ChassisSentinel);
        assert_eq!(cat("aether.input"), MailboxCategory::Actor);
        assert_eq!(
            cat("aether.component.trampoline:cam"),
            MailboxCategory::Trampoline
        );
        // User-space names fall outside any of the recognised
        // categories; the hub's downstream renderer treats them as
        // raw tagged ids without a type prefix.
        assert!(
            snap.iter()
                .find(|d| d.name == "user_thing")
                .unwrap()
                .category
                .is_none(),
            "non-aether names categorise as None",
        );

        // The synthetic chassis sentinel uses the canonical id —
        // hub-side resolution of trace senders against this id finds
        // the right name without re-hashing.
        let chassis = snap.iter().find(|d| d.name == "aether.chassis").unwrap();
        assert_eq!(chassis.id, MailboxId::CHASSIS_MAILBOX_ID);
    }

    /// Each registered descriptor's id matches the deterministic hash
    /// of its name (ADR-0029) — same id space the hub already knows.
    #[test]
    fn list_mailbox_descriptors_ids_match_name_hashes() {
        let r = Registry::new();
        let id = r.register_inbox("aether.audio", noop_handler());
        let entry = r
            .list_mailbox_descriptors()
            .into_iter()
            .find(|d| d.name == "aether.audio")
            .expect("audio entry");
        assert_eq!(entry.id, id);
        assert_eq!(entry.id, MailboxId::from_name("aether.audio"));
    }

    /// Issue iamacoffeepot/aether#742: every successful
    /// `register_inbox` fires the installed change hook with the
    /// post-registration inventory snapshot. The chassis wires this
    /// hook to push to the hub via `egress_mailboxes_changed` so any
    /// chassis-builder cap that registers post-Hello shows up in the
    /// hub's inventory cache without an explicit publish.
    #[test]
    fn mailbox_change_hook_fires_on_register_inbox() {
        use std::sync::Mutex;

        let r = Arc::new(Registry::new());
        let snapshots: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(Vec::new()));
        let snapshots_for_hook = Arc::clone(&snapshots);
        r.set_on_mailbox_change(Arc::new(move |descriptors| {
            let names: Vec<String> = descriptors.into_iter().map(|d| d.name).collect();
            snapshots_for_hook.lock().unwrap().push(names);
        }));

        r.register_inbox("aether.input", noop_handler());
        r.register_inbox("aether.render", noop_handler());

        let captured = snapshots.lock().unwrap();
        assert_eq!(
            captured.len(),
            2,
            "hook should fire once per successful register_inbox"
        );
        // Each snapshot is the FULL inventory at that moment (matches
        // the wire `MailboxesChanged` semantics — full replace, not
        // delta), so the second snapshot strictly contains the first.
        assert!(captured[0].contains(&"aether.input".to_owned()));
        assert!(captured[1].contains(&"aether.input".to_owned()));
        assert!(captured[1].contains(&"aether.render".to_owned()));
    }

    /// Issue 742: `try_register_inbox` fires the hook on the Ok
    /// branch and stays silent on `NameConflict`.
    #[test]
    fn mailbox_change_hook_fires_on_try_register_inbox_ok_only() {
        use std::sync::Mutex;

        let r = Arc::new(Registry::new());
        let count: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
        let count_for_hook = Arc::clone(&count);
        r.set_on_mailbox_change(Arc::new(move |_| {
            *count_for_hook.lock().unwrap() += 1;
        }));

        let _ = r
            .try_register_inbox("aether.input", noop_handler())
            .expect("first register OK");
        // Second registration with the same name conflicts.
        let _ = r
            .try_register_inbox("aether.input", noop_handler())
            .expect_err("second register should NameConflict");

        assert_eq!(*count.lock().unwrap(), 1, "hook fires once on Ok only");
    }

    #[test]
    fn registration_through_shared_arc() {
        // Interior mutability means Arc<Registry> can register after
        // it's already been shared — the dispatch path today never
        // exercises this, but PR 2+ will when `load_component` adds
        // mailboxes and kinds from a handler that holds an Arc.
        let r = Arc::new(Registry::new());
        let r2 = Arc::clone(&r);
        let id = r2.register_inbox("late", noop_handler());
        assert_eq!(r.lookup("late"), Some(id));
        let kind_id = r.register_kind("aether.late");
        assert_eq!(
            r.kind_id("aether.late"),
            Some(kind_id),
            "shared Arc registrations are visible through the original handle"
        );
    }

    /// Issue iamacoffeepot/aether#848 Phase 1: a bare
    /// `Fn(MailDispatch<'_>)` closure satisfies `InlineHandler` via
    /// the blanket impl, and dispatching through
    /// `<dyn InlineHandler>::dispatch` invokes the body once per
    /// call. No mailer / registry plumbing is wired through yet —
    /// that lands in PR 2.
    #[test]
    fn inline_handler_blanket_impl_dispatches_closure_body() {
        let counter = Arc::new(AtomicU32::new(0));
        let c2 = Arc::clone(&counter);
        let handler: Arc<dyn InlineHandler> = Arc::new(move |dispatch: MailDispatch<'_>| {
            c2.fetch_add(dispatch.count, Ordering::SeqCst);
        });
        handler.dispatch(test_dispatch(KindId(0), "aether.tick", &[], 5));
        handler.dispatch(test_dispatch(KindId(0), "aether.tick", &[], 7));
        assert_eq!(
            counter.load(Ordering::SeqCst),
            12,
            "blanket InlineHandler impl should forward each dispatch to the closure body once",
        );
    }

    /// Issue iamacoffeepot/aether#848 Phase 1: a bare
    /// `Fn(OwnedDispatch)` closure satisfies `InboxHandler` via the
    /// blanket impl. The closure body moves the payload into a
    /// captured Vec, demonstrating the ownership transfer the trait
    /// exists to enable — the hot-path "no `to_vec()` clone" win
    /// called out in iamacoffeepot/aether#848.
    #[test]
    fn inbox_handler_blanket_impl_moves_owned_payload() {
        let collected = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let collected_for_handler = Arc::clone(&collected);
        let handler: Arc<dyn InboxHandler> = Arc::new(move |dispatch: OwnedDispatch| {
            // Payload moves straight into the captured Vec — no clone
            // or `to_vec()` on a borrowed slice.
            collected_for_handler
                .lock()
                .unwrap()
                .push(dispatch.payload.into_vec());
        });

        handler.enqueue(OwnedDispatch::disarmed(
            KindId(0),
            "aether.audio.note_on".to_owned(),
            None,
            ReplyTo::NONE,
            MailRef::from(vec![1, 2, 3]),
            1,
            MailId::NONE,
            MailId::NONE,
            None,
            Nanos(0),
            0,
            MailboxId(0),
        ));
        handler.enqueue(OwnedDispatch::disarmed(
            KindId(0),
            "aether.audio.note_on".to_owned(),
            None,
            ReplyTo::NONE,
            MailRef::from(vec![4, 5, 6, 7]),
            1,
            MailId::NONE,
            MailId::NONE,
            None,
            Nanos(0),
            0,
            MailboxId(0),
        ));

        let collected = collected.lock().unwrap();
        assert_eq!(collected.len(), 2);
        assert_eq!(collected[0], vec![1, 2, 3]);
        assert_eq!(collected[1], vec![4, 5, 6, 7]);
    }

    /// Issue iamacoffeepot/aether#848 Phase 1: hand-rolled
    /// `impl InboxHandler for MyStruct` compiles and dispatches
    /// alongside the blanket-impl path. This is the cap-authoring
    /// shape PR 3 will reach for (a struct holding the mpsc Sender);
    /// a regression here means caps can't migrate.
    #[test]
    fn inbox_handler_hand_rolled_impl_dispatches_per_call() {
        use std::sync::mpsc;

        struct ChannelForwarder {
            tx: mpsc::Sender<OwnedDispatch>,
        }
        impl InboxHandler for ChannelForwarder {
            fn enqueue(&self, dispatch: OwnedDispatch) {
                let _ = self.tx.send(dispatch);
            }
        }

        let (tx, rx) = mpsc::channel();
        let handler: Arc<dyn InboxHandler> = Arc::new(ChannelForwarder { tx });
        handler.enqueue(OwnedDispatch::disarmed(
            KindId(42),
            "aether.fs.write".to_owned(),
            Some("aether.fs".to_owned()),
            ReplyTo::NONE,
            MailRef::from(vec![0xAB, 0xCD]),
            1,
            MailId::NONE,
            MailId::NONE,
            None,
            Nanos(0),
            0,
            MailboxId(0),
        ));

        let received = rx.try_recv().expect("hand-rolled enqueue should send");
        assert_eq!(received.kind, KindId(42));
        assert_eq!(received.kind_name, "aether.fs.write");
        assert_eq!(received.payload.into_vec(), vec![0xAB, 0xCD]);
        assert!(
            rx.try_recv().is_err(),
            "exactly one enqueue should send exactly one envelope",
        );
    }
}
