use std::fmt;
#[cfg(debug_assertions)]
use std::{cell::Cell, thread};

use aether_kinds::trace::Nanos;

use crate::mail::{KindId, MailId, MailRef, MailboxId, Source};

/// Test-only helper that builds a [`MailDispatch`] with empty
/// `origin` / `Source::NONE` / `MailId::NONE` defaults from the
/// minimum positional args. Used by chassis and capability tests
/// that drive a registered handler synchronously without going
/// through the full `Mail` → `Mailer::push` path.
#[cfg(test)]
pub fn test_dispatch<'a>(kind: KindId, kind_name: &'a str, payload: &'a [u8], count: u32) -> MailDispatch<'a> {
    MailDispatch {
        kind,
        kind_name,
        origin: None,
        sender: Source::NONE,
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
/// (empty origin, `Source::NONE`, `MailId::NONE`).
///
/// Issue iamacoffeepot/aether#848 PR 2: added alongside the
/// [`OwnedDispatch`] migration so cap-side dispatcher tests stay
/// terse without each rebuilding the full struct literal.
#[cfg(test)]
pub fn test_owned_dispatch(kind: KindId, kind_name: &str, payload: &[u8], count: u32) -> OwnedDispatch {
    OwnedDispatch::disarmed(
        kind,
        kind_name.to_owned(),
        None,
        Source::NONE,
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

/// One mail's worth of dispatch metadata handed to an
/// [`InlineHandler`](crate::mail::registry::InlineHandler). Bundled into a
/// single struct (rather than a positional argument list) so the
/// producer-minted ADR-0080 §1 / §5
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
    pub sender: Source,
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
struct ObligationGuard {
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
        Self { mail_id, kind_name, mailbox, armed: Cell::new(mail_id != MailId::NONE) }
    }

    /// A guard that carries no obligation — test/helper mints and the
    /// disarmed result of a `Clone`.
    fn disarmed(mail_id: MailId, kind_name: String, mailbox: MailboxId) -> Self {
        Self { mail_id, kind_name, mailbox, armed: Cell::new(false) }
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

/// Owned mirror of [`MailDispatch`] handed to
/// [`InboxHandler::enqueue`](crate::mail::registry::InboxHandler::enqueue).
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
    pub sender: Source,
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
    /// The mailbox this dispatch was routed to (ADR-0114 decision #1).
    /// For a normally-addressed actor this is the actor's own mailbox
    /// id; once inline-child aliases exist (ADR-0114) it is the alias
    /// the producer addressed, which the guest membrane demuxes on.
    /// Set from the `recipient` parameter the two production mint sites
    /// already pass; survives release builds (where the debug-only
    /// `ObligationGuard` that previously held it is compiled out).
    pub recipient: MailboxId,
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
        sender: Source,
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
            recipient,
            #[cfg(debug_assertions)]
            obligation,
        }
    }

    /// Construct an `OwnedDispatch` whose ADR-0094 obligation is
    /// **disarmed** — dropping it without discharge/transfer does not
    /// panic. For test/helper mints, the `noop` handler, and seeds that
    /// carry no real settlement lineage. `recipient` is stored on the
    /// dispatch (and names the never-firing guard's mailbox in debug);
    /// pass `MailboxId(0)` when none is meaningful.
    ///
    /// `pub` (not `pub(crate)`) because integration tests and sibling
    /// crates' (the per-cap crates') tests mint dispatches directly to
    /// poke an `InboxHandler`; they have no settlement obligation, so
    /// they take the disarmed path. The armed constructor stays
    /// crate-internal — only the substrate's own mint sites arm.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn disarmed(
        kind: KindId,
        kind_name: String,
        origin: Option<String>,
        sender: Source,
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
            recipient,
            #[cfg(debug_assertions)]
            obligation,
        }
    }

    /// ADR-0094: "the obligation ends here." Records intent that the
    /// consumer is calling `Mailer::record_finished` for this
    /// `mail_id`; placed adjacent to every such call so the two cannot
    /// drift. No-op in release (no guard field). `pub` because the
    /// desktop window drain (`aether-chassis-desktop`) is a hand-rolled
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
            recipient: self.recipient,
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
            .field("recipient", &self.recipient)
            // ADR-0094: the debug-only `obligation` guard is deliberately
            // omitted so `Debug` output is identical across debug/release.
            .finish_non_exhaustive()
    }
}
