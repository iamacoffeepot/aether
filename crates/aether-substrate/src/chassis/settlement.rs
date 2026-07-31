//! ADR-0080 §6 settlement registry — chassis-side gate-notification
//! map for `Settled { root }` mail.
//!
//! Three subscriber shapes share one pending map (keyed on root
//! [`MailId`]):
//!
//! - [`SettlementRegistry::subscribe_settlement`] returns a
//!   `crossbeam_channel::Receiver<()>` for in-thread waiters
//!   (chassis-internal code, tests) that can block on `recv` directly.
//! - [`SettlementRegistry::subscribe_settlement_mail`] pushes a
//!   notification mail to a target mailbox when the root settles —
//!   for actors whose thread is committed to its mpsc inbox and
//!   can't block on a separate channel without per-cid helper threads.
//! - [`SettlementRegistry::subscribe_settlement_with`] runs a one-shot
//!   `Send + 'static` callback on the settling thread (ADR-0161
//!   §Decision 2) — the pumped render driver bridges a settlement into
//!   its unified [`PumpWake`] channel this way so the wait that pumps
//!   the render slot wakes when a gated chain settles.
//!
//! Both fire when the [`crate::actor::native`] dispatcher routes a
//! `Settled { root }` mail addressed to
//! [`MailboxId::CHASSIS_MAILBOX_ID`] through the
//! registry's [`SettlementRegistry::fire_settled`] hook.
//!
//! ADR-0080 §6 framing: settlement is eventually-consistent, not
//! transactional. Two races are handled here:
//!
//! - **Subscribe-after-fire.** A gate may subscribe to a root that
//!   already settled (the `Finished` event landed before the gate
//!   site got around to subscribing). The registry tracks
//!   already-fired roots in a small `HashSet`; subscribing to one
//!   pre-fires the receiver immediately so the gate doesn't hang.
//! - **Duplicate `fire_settled`.** Per ADR §6, settlement is a hint
//!   — a root may report settled multiple times under retries or
//!   late-arriving `Finished` events. The registry's `fire_settled`
//!   is idempotent: subsequent calls for the same root after the
//!   subscribers have drained are no-ops (the `HashSet` hit short-
//!   circuits).
//!
//! The registry is striped into independent mutex cells keyed by the
//! root's correlation id, and each cell's `settled` set is bounded
//! (issue 2618): the oldest remembered roots evict in insertion order
//! past the per-cell cap, so the registry holds a fixed-size
//! recent-settlement window rather than growing for the chassis
//! lifetime. A subscriber arriving after its root has been evicted from
//! the window misses the pre-fire — see `SETTLED_CAP_PER_CELL` for
//! why that gap sits far outside any legitimate subscribe timing.

use std::array;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::actor::native::NativeActor;
use crate::actor::native::pumped_slot::PumpedSlot;
use crate::chassis::ctx::MailboxWakeSlot;
use crate::mail::Mail;
use crate::mail::mailer::Mailer;
use crate::runtime::lifecycle::FatalAbortRecord;
use aether_data::{Kind, KindId, MailId, MailboxId};
use aether_kinds::trace::Settled;
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, bounded};

/// Cell count for the registry's striped lock (issue 2618). A power of
/// two so the root hash masks; 64 cells is far past the worker count on
/// any target machine, so two workers firing settlement for different
/// roots virtually never contend.
const CELL_COUNT: usize = 64;

/// Per-cell cap on remembered settled roots (issue 2618). The settled
/// set exists only to pre-fire a subscriber that arrives *after* its
/// root settled; every production subscriber registers within
/// microseconds of dispatching its root, so an eviction window
/// `CELL_COUNT * SETTLED_CAP_PER_CELL` (131,072) roots deep — a second-plus
/// of history even at stress-harness settle rates — is far beyond any
/// legitimate subscribe-after-settle gap, and it converts what was
/// unbounded growth over the chassis lifetime into a fixed ~6 MiB
/// ceiling (the long-lived-substrate leak the stress arc surfaced).
const SETTLED_CAP_PER_CELL: usize = 2048;

/// Chassis-owned settlement notification registry. Owned by the
/// chassis (one per substrate); cloned via `Arc` into the
/// [`Mailer`]'s chassis-router closure so the
/// dispatcher's `Settled` switch can fire.
///
/// Striped into `CELL_COUNT` independent mutex cells keyed by the
/// root's correlation id (issue 2618): every settled chain in the
/// substrate fires through here from whichever worker discharged it,
/// and every settlement-awaiting dispatch subscribes, so a single
/// registry-wide mutex was the measured throughput ceiling once the
/// HTTP dispatch path sharded (ADR-0135).
pub struct SettlementRegistry {
    cells: [Mutex<Cell>; CELL_COUNT],
}

impl Default for SettlementRegistry {
    fn default() -> Self {
        Self { cells: array::from_fn(|_| Mutex::new(Cell::default())) }
    }
}

#[derive(Default)]
struct Cell {
    /// Subscribers waiting on each root's settlement signal. Vec so
    /// multiple gate sites can wait on the same root concurrently
    /// (lifecycle gates + the per-frame drain barrier might both
    /// listen on the same Tick root). Channel and mail subscribers
    /// coexist in the same vec, distinguished by [`SettlementSubscriber`]
    /// variant — one map, one drain.
    pending: HashMap<MailId, Vec<SettlementSubscriber>>,
    /// Roots that have already settled at least once. Subscribing to
    /// one pre-fires the receiver. Bounded to [`SETTLED_CAP_PER_CELL`]
    /// recent roots, evicted in insertion order via `settled_order`
    /// (issue 2618) — see the cap's doc for the window semantics.
    settled: HashSet<MailId>,
    /// Insertion-order companion to `settled`, driving the bounded
    /// eviction: the oldest remembered root leaves both structures
    /// when the cap is exceeded.
    settled_order: VecDeque<MailId>,
}

impl Cell {
    /// Record `root` as settled, evicting the oldest remembered root
    /// past the cap. A repeat settle of an already-remembered root is
    /// a no-op (no duplicate order entry).
    fn remember_settled(&mut self, root: MailId) {
        if self.settled.insert(root) {
            self.settled_order.push_back(root);
            while self.settled_order.len() > SETTLED_CAP_PER_CELL {
                if let Some(evicted) = self.settled_order.pop_front() {
                    self.settled.remove(&evicted);
                }
            }
        }
    }
}

/// One subscriber parked on a root pending settlement. Channel
/// subscribers are for in-thread waiters (chassis-internal code, tests)
/// that block on `Receiver<()>`; mail subscribers are for actors whose
/// thread is committed to its mpsc inbox and can't block on a separate
/// channel without per-cid helper threads.
enum SettlementSubscriber {
    /// Wake an in-thread waiter on a `bounded(1)` channel.
    Channel(Sender<()>),
    /// Push a notification mail to `target` via `mailer` carrying a
    /// [`Settled`] with the settled root as the payload.
    Mail { target: MailboxId, kind: KindId, mailer: Arc<Mailer> },
    /// Run a one-shot callback on the settling thread (ADR-0161
    /// §Decision 2). The pumped render driver installs one that sends
    /// [`PumpWake::Settled`] into its unified wake channel. Boxed
    /// `FnOnce` — fired exactly once (the subscriber is drained from
    /// `pending` on `fire_settled`, and a pre-fire runs it inline).
    Callback(Box<dyn FnOnce() + Send>),
}

impl SettlementSubscriber {
    /// Fire this subscriber for the settled `root`. Channel sends are
    /// non-blocking (`try_send`, so a closed receiver doesn't panic);
    /// mail sends go through the chassis [`Mailer`]
    /// which resolves the recipient inline on the firing thread.
    fn fire(self, root: MailId) {
        match self {
            Self::Channel(tx) => {
                let _ = tx.try_send(());
            }
            Self::Mail { target, kind, mailer } => {
                push_settlement_notice(&mailer, target, kind, root);
            }
            Self::Callback(callback) => callback(),
        }
    }
}

impl SettlementRegistry {
    /// Construct an empty registry. Production chassis builders wrap
    /// the result in `Arc<SettlementRegistry>` and clone into both
    /// the chassis context (subscribers reach for it) and the
    /// `Mailer` chassis-router closure (the `Settled` mail dispatch
    /// fires it).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The mutex cell owning `root` (issue 2618): the correlation id is
    /// a mailer-minted monotonic counter, so a Fibonacci-hash mix
    /// spreads consecutive ids across the stripe before masking.
    fn cell_for(&self, root: MailId) -> &Mutex<Cell> {
        let mixed = root.correlation_id.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        // Take the mask from the mixed value's high bits — the ones the
        // multiply actually scrambles.
        let index = (mixed >> 32) as usize & (CELL_COUNT - 1);
        &self.cells[index]
    }

    /// Subscribe a gate site to `root`'s settlement signal. Returns
    /// a [`Receiver<()>`] that wakes when [`Self::fire_settled`] is
    /// called for the same root. Pre-fires immediately if `root` has
    /// already settled at least once.
    ///
    /// The receiver carries a single `()` value; subsequent receive
    /// attempts return [`crossbeam_channel::TryRecvError::Empty`] /
    /// `Disconnected` per the bounded(1) channel contract. Gate
    /// sites typically `recv_timeout` once and discard the receiver.
    ///
    /// # Panics
    /// Panics if the inner `Mutex` is poisoned — fail-fast per ADR-0063:
    /// a poisoned mutex means a prior holder panicked under the guard.
    pub fn subscribe_settlement(&self, root: MailId) -> Receiver<()> {
        let (tx, rx) = bounded::<()>(1);
        let mut cell = self.cell_for(root).lock().expect("settlement registry mutex poisoned; fail-fast per ADR-0063");
        if cell.settled.contains(&root) {
            // Pre-fire — root already settled. `try_send` rather
            // than `send` so a closed receiver (caller dropped it
            // before reading) doesn't panic.
            let _ = tx.try_send(());
        } else {
            cell.pending.entry(root).or_default().push(SettlementSubscriber::Channel(tx));
        }
        rx
    }

    /// Subscribe a mailbox to receive a notification mail when `root`
    /// settles. The notification is a [`Mail`] with the
    /// given `kind`, a [`Settled`] carrying the settled root as payload,
    /// and `count = 1`. Pre-fires immediately (synchronously
    /// pushes the mail) if `root` has already settled at least once.
    ///
    /// Coexists with [`Self::subscribe_settlement`] — a root can have
    /// channel and mail subscribers; both fire on `fire_settled`.
    ///
    /// # Panics
    /// Panics if the inner `Mutex` is poisoned — fail-fast per ADR-0063:
    /// a poisoned mutex means a prior holder panicked under the guard.
    pub fn subscribe_settlement_mail(&self, root: MailId, target: MailboxId, kind: KindId, mailer: Arc<Mailer>) {
        let mut cell = self.cell_for(root).lock().expect("settlement registry mutex poisoned; fail-fast per ADR-0063");
        if cell.settled.contains(&root) {
            // Drop the mutex before pushing — `push` may run hot
            // (resolves the recipient inline on this thread).
            drop(cell);
            push_settlement_notice(&mailer, target, kind, root);
        } else {
            cell.pending.entry(root).or_default().push(SettlementSubscriber::Mail { target, kind, mailer });
        }
    }

    /// Subscribe a one-shot `callback` to `root`'s settlement (ADR-0161
    /// §Decision 2). The callback runs on whatever thread
    /// [`Self::fire_settled`] fires on — the settling worker — so it must
    /// be `Send + 'static`; it fires exactly once. Pre-fires immediately
    /// (runs `callback` inline on the caller's thread) if `root` has
    /// already settled at least once.
    ///
    /// The additive counterpart of [`Self::subscribe_settlement`]: rather
    /// than parking a waiter on a channel, it lets a call site bridge a
    /// settlement into machinery of its own — the pumped render driver
    /// forwards [`PumpWake::Settled`] into the same channel its slot's
    /// mail wake feeds, because std `mpsc` cannot select across two
    /// sources. Coexists with the channel and mail forms; a root can
    /// carry any mix, and all fire on `fire_settled`.
    ///
    /// # Panics
    /// Panics if the inner `Mutex` is poisoned — fail-fast per ADR-0063:
    /// a poisoned mutex means a prior holder panicked under the guard.
    pub fn subscribe_settlement_with<F>(&self, root: MailId, callback: F)
    where
        F: FnOnce() + Send + 'static,
    {
        let mut cell = self.cell_for(root).lock().expect("settlement registry mutex poisoned; fail-fast per ADR-0063");
        if cell.settled.contains(&root) {
            // Drop the mutex before running the callback — it may run hot
            // (the pump bridge sends into a channel; a future subscriber
            // could re-enter the registry).
            drop(cell);
            callback();
        } else {
            cell.pending.entry(root).or_default().push(SettlementSubscriber::Callback(Box::new(callback)));
        }
    }

    /// Fire the settlement signal for `root`. Wakes every subscriber
    /// currently registered for `root` and records the root in the
    /// `settled` set so subsequent [`Self::subscribe_settlement`]
    /// calls pre-fire. Idempotent: calling twice is the same as
    /// calling once for any waiter that already woke.
    ///
    /// # Panics
    /// Panics if the inner `Mutex` is poisoned — fail-fast per ADR-0063:
    /// a poisoned mutex means a prior holder panicked under the guard.
    pub fn fire_settled(&self, root: MailId) {
        // Drop the mutex before firing — mail subscribers resolve
        // the recipient inline on this thread, and channel sends are
        // cheap but uniformly drop-then-fire keeps the lock window
        // tight and removes a re-entrancy hazard if a future
        // subscriber type re-enters the registry.
        let subs = {
            let mut cell =
                self.cell_for(root).lock().expect("settlement registry mutex poisoned; fail-fast per ADR-0063");
            cell.remember_settled(root);
            cell.pending.remove(&root)
        };
        if let Some(subs) = subs {
            for sub in subs {
                sub.fire(root);
            }
        }
    }

    /// Test introspection: count of pending channel subscribers
    /// across all roots. Used by the unit tests in this module;
    /// production code queries via mail (subscribe + recv).
    #[cfg(test)]
    fn pending_count(&self) -> usize {
        self.cells
            .iter()
            .map(|cell| {
                cell.lock()
                    .expect("settlement registry mutex poisoned; fail-fast per ADR-0063")
                    .pending
                    .values()
                    .flat_map(|v| v.iter())
                    .filter(|s| matches!(s, SettlementSubscriber::Channel(_)))
                    .count()
            })
            .sum()
    }

    /// Test introspection: count of roots recorded as already
    /// settled.
    #[cfg(test)]
    fn settled_count(&self) -> usize {
        self.cells
            .iter()
            .map(|cell| cell.lock().expect("settlement registry mutex poisoned; fail-fast per ADR-0063").settled.len())
            .sum()
    }

    /// Test introspection: count of pending mail subscribers across all
    /// roots.
    #[cfg(test)]
    fn pending_mail_count(&self) -> usize {
        self.cells
            .iter()
            .map(|cell| {
                cell.lock()
                    .expect("settlement registry mutex poisoned; fail-fast per ADR-0063")
                    .pending
                    .values()
                    .flat_map(|v| v.iter())
                    .filter(|s| matches!(s, SettlementSubscriber::Mail { .. }))
                    .count()
            })
            .sum()
    }
}

/// Push a settlement-notice mail to `target` via `mailer`. The payload
/// is a [`Settled`] carrying the settled root, encoded through the kind
/// codec — the same shape the consumer's `on_settled` handler decodes.
fn push_settlement_notice(mailer: &Mailer, target: MailboxId, kind: KindId, root: MailId) {
    let payload = Settled { root }.encode_into_bytes();
    mailer.push(Mail::new(target, kind, payload, 1));
}

/// What a call site wants done when a wait on an internal completion
/// signal exhausts its cumulative patience budget without the signal
/// firing (issue #1305). The helper owns the *patience strategy*
/// (escalating re-arm + per-round log); the disposition names what a
/// genuine wedge means here so the same "wait for an internal signal"
/// gate doesn't re-roll five divergent terminal behaviors by hand.
///
/// The variants line up with the five behaviors already scattered
/// across the substrate + its bundle. The helper dispenses `Proceed`,
/// `ReplyErr`, and `Panic` directly via its return value / a `panic!`;
/// `Abort` is the one disposition that needs an aborter (a
/// [`crate::runtime::lifecycle::FatalAborter`]), which the caller holds
/// — the helper stays free of any `HubOutbound` coupling and hands the
/// caller a [`GateWedge`] to route through its own aborter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalDisposition {
    /// Best-effort gate: log loud and let the caller carry on. Never
    /// blocks process exit. The helper returns
    /// [`WaitOutcome::Wedged`] so the caller can branch, but takes no
    /// terminal action itself.
    Proceed,
    /// The wedge is recoverable from the caller's vantage — it surfaces
    /// a typed error to someone who can retry. The helper returns
    /// [`WaitOutcome::Wedged`]; the caller maps it to its error type.
    ReplyErr,
    /// The wedge is unrecoverable: the caller must route the returned
    /// [`GateWedge`] through its [`crate::runtime::lifecycle::FatalAborter`].
    /// The helper does *not* call `fatal_abort` itself — that would
    /// couple this module to the desktop chassis's `HubOutbound`.
    Abort,
    /// Test/debug attributable failure: the helper `panic!`s on a wedge
    /// so a stuck gate fails at the gate site rather than as a
    /// downstream `0 != 1`. (A `panic!` needs no aborter, so the helper
    /// can dispense this one without coupling to the outbound.)
    Panic,
}

/// Why a gate stopped waiting without its signal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateFailure {
    /// The signal stayed silent for the whole cumulative patience budget.
    Silent,
    /// Every sender dropped without firing the signal.
    Disconnected,
    /// The chassis fatally aborted, so the thread that would have fired
    /// the signal is gone. Carries the reason the
    /// [`FatalAbortRecord`] captured on the way through
    /// [`crate::runtime::lifecycle::FatalAborter::abort`] — the one piece
    /// of attribution a caller that only sees the stalled gate could
    /// otherwise never recover (iamacoffeepot/aether#4193).
    FatalAbort(String),
}

/// A wedge detected by [`await_internal_signal`]: the internal signal
/// did not fire, either because it stayed silent to the cumulative
/// patience budget or because the chassis went down under it. Carries
/// enough context for the caller to log / surface / abort attributably.
#[derive(Debug, Clone)]
pub struct GateWedge {
    /// The gate name passed to [`await_internal_signal`].
    pub gate: String,
    /// Total wall-clock time waited before giving up.
    pub waited: Duration,
    /// What ended the wait.
    pub failure: GateFailure,
}

impl GateWedge {
    /// Render the wedge as the `reason` string a
    /// [`crate::runtime::lifecycle::FatalAborter`] consumes.
    #[must_use]
    pub fn reason(&self) -> String {
        match &self.failure {
            GateFailure::Silent => format!(
                "gate {} wedged: internal signal never fired within patience budget, waited {:?}",
                self.gate, self.waited
            ),
            GateFailure::Disconnected => format!(
                "gate {} wedged: signal channel disconnected without firing, waited {:?}",
                self.gate, self.waited
            ),
            GateFailure::FatalAbort(abort) => format!(
                "gate {} abandoned after {:?}: the chassis fatally aborted, so nothing is left to fire this signal — {abort}",
                self.gate, self.waited
            ),
        }
    }
}

/// Outcome of an [`await_internal_signal`] wait.
#[derive(Debug)]
#[must_use]
pub enum WaitOutcome {
    /// The internal signal fired before the cumulative cap. Proceed
    /// normally.
    Settled,
    /// The signal never fired (silent to the cap, or the channel
    /// disconnected). The caller dispenses its [`TerminalDisposition`]
    /// against the carried [`GateWedge`]. Only returned for the
    /// non-`Panic` dispositions — `Panic` diverges inside the helper.
    Wedged(GateWedge),
}

/// Escalating-patience wait on an internal completion signal — a
/// settlement [`Receiver`] or a pooled-actor teardown close-done
/// channel (issue #1305). Replaces the hand-rolled wall-clock
/// `recv_timeout(N)` that can't tell *starved-but-healthy* (the causal
/// chain is merely slow under load) from *genuinely wedged*: under
/// `nextest --workspace` saturation a healthy-but-slow gate trips a
/// fixed deadline and false-fires (flake #1295).
///
/// Loops `rx.recv_timeout(round_budget)`:
///
/// - `Ok(())` → [`WaitOutcome::Settled`].
/// - `Timeout` → log `gate <name> slow: waited <cumulative>, extending`
///   at warn and re-arm, until the cumulative wait reaches
///   `cumulative_cap`. The signal is a one-shot the worker fires
///   whenever it is next scheduled, so re-arming `recv` is patient
///   waiting with logged checkpoints, not a re-poke — a healthy gate
///   resolves before the cap; a genuine wedge exhausts it.
/// - `Disconnected` → the sender dropped without firing; the same
///   terminal path as cap-exhaustion, with [`GateFailure::Disconnected`]
///   set so the wedge is attributable.
///
/// `abort_watch` is the chassis's [`FatalAbortRecord`], and it is what
/// keeps a fatal abort attributable across the gate
/// (iamacoffeepot/aether#4193). A [`crate::runtime::lifecycle::PanicAborter`]
/// abort unwinds the thread it fires on, so a gate waiting on a signal
/// that thread owed fires nothing and simply burns its budget — under a
/// test-runner ceiling shorter than the budget, the run reports a bare
/// timeout and the panic that caused it reads as a hang. Watched, the
/// gate refuses to wait for a chassis that already aborted, wakes on the
/// record's tripwire if one aborts mid-wait, and folds the recorded
/// reason into the wedge either way. Pass `None` from a gate with no
/// chassis behind it.
///
/// On a wedge the helper dispenses `disposition`:
///
/// - [`TerminalDisposition::Panic`] → diverges via `panic!` (no aborter
///   needed; keeps the wedge attributable in test/debug).
/// - [`TerminalDisposition::Proceed`] / [`TerminalDisposition::ReplyErr`]
///   / [`TerminalDisposition::Abort`] → returns [`WaitOutcome::Wedged`]
///   carrying the [`GateWedge`]; the caller acts on it (log-and-carry,
///   typed error, or route through its `FatalAborter`). `Abort` is
///   *not* performed here — that would couple this module to the
///   desktop chassis's `HubOutbound`.
///
/// `round_budget` is one re-arm interval (the log cadence);
/// `cumulative_cap` is the total patience before declaring a wedge. A
/// `round_budget` of zero is clamped up to a small floor so the loop
/// can't spin.
pub fn await_internal_signal(
    rx: &Receiver<()>,
    gate: &str,
    round_budget: Duration,
    cumulative_cap: Duration,
    disposition: TerminalDisposition,
    abort_watch: Option<&FatalAbortRecord>,
) -> WaitOutcome {
    // The chassis has already aborted, so whatever owed this signal is
    // gone and the budget can only expire. Report the abort now rather
    // than time out anonymously later.
    if let Some(abort) = abort_watch.and_then(FatalAbortRecord::reason) {
        return wedge(gate, Duration::ZERO, GateFailure::FatalAbort(abort), disposition);
    }

    // Clamp the per-round budget off zero so a misconfigured caller
    // can't turn the loop into a busy-spin.
    let round = round_budget.max(Duration::from_millis(1));
    let tripwire = abort_watch.map_or_else(crossbeam_channel::never, FatalAbortRecord::tripwire);
    let start = Instant::now();
    loop {
        crossbeam_channel::select! {
            recv(rx) -> received => return match received {
                Ok(()) => WaitOutcome::Settled,
                Err(_) => wedge(gate, start.elapsed(), failure(abort_watch, GateFailure::Disconnected), disposition),
            },
            // The tripwire never carries a value — a receive on it means
            // the record dropped its sender, which it does only after
            // writing the abort reason `failure` is about to read.
            recv(tripwire) -> _ => {
                return wedge(gate, start.elapsed(), failure(abort_watch, GateFailure::Silent), disposition);
            }
            default(round) => {
                let waited = start.elapsed();
                if waited >= cumulative_cap {
                    return wedge(gate, waited, failure(abort_watch, GateFailure::Silent), disposition);
                }
                tracing::warn!(
                    target: "aether_substrate::settlement",
                    gate,
                    waited_millis = waited.as_millis(),
                    cap_millis = cumulative_cap.as_millis(),
                    "gate {gate} slow: waited {waited:?}, extending",
                );
            }
        }
    }
}

/// The wedge cause, preferring a recorded fatal abort over the raw
/// channel outcome: an abort that fired while the gate waited is the
/// attribution a reader wants, not "the signal stayed silent".
fn failure(abort_watch: Option<&FatalAbortRecord>, channel_outcome: GateFailure) -> GateFailure {
    abort_watch.and_then(FatalAbortRecord::reason).map_or(channel_outcome, GateFailure::FatalAbort)
}

/// Build the wedge verdict and dispense the one disposition the helper
/// owns (`Panic`); the rest ride back to the caller in
/// [`WaitOutcome::Wedged`].
fn wedge(gate: &str, waited: Duration, failure: GateFailure, disposition: TerminalDisposition) -> WaitOutcome {
    let wedge = GateWedge { gate: gate.to_owned(), waited, failure };
    match disposition {
        TerminalDisposition::Panic => panic!("{}", wedge.reason()),
        TerminalDisposition::Proceed | TerminalDisposition::ReplyErr | TerminalDisposition::Abort => {
            WaitOutcome::Wedged(wedge)
        }
    }
}

/// A wake on the pumped settlement driver's unified channel (ADR-0161
/// §Decision 2). std `mpsc` cannot select across two sources, so the two
/// producers — the pumped slot's mailbox wake and the settlement
/// subscription — share one channel and distinguish their reason here.
///
/// - [`PumpWake::Mail`] — an envelope was accepted onto the pumped
///   mailbox; [`await_settlement_pumped`] drains the slot and keeps
///   waiting. Installed on the slot's [`MailboxWakeSlot`] by
///   [`install_pump_wake`].
/// - [`PumpWake::Settled`] — the awaited root settled; the wait returns.
///   Fed by a [`SettlementRegistry::subscribe_settlement_with`] callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PumpWake {
    /// The awaited root settled — the wait may return.
    Settled,
    /// Mail landed on the pumped mailbox — drain the slot and keep waiting.
    Mail,
}

/// Install the [`PumpWake::Mail`] wake on a pumped mailbox's
/// [`MailboxWakeSlot`] (ADR-0161 §Decision 2). After each accepted
/// inbound send the slot fires this hook, nudging
/// [`await_settlement_pumped`] to drain even while the driver thread is
/// parked in the wait. The `tx` half is a `crossbeam_channel::Sender`
/// (its `Send + Sync` is what lets the hook satisfy the
/// [`crate::chassis::ctx::MailboxWakeFn`] bound; std `mpsc::Sender` is
/// `!Sync`), so both wake producers share one channel the wait can
/// consume.
pub fn install_pump_wake(slot: &MailboxWakeSlot, tx: Sender<PumpWake>) {
    slot.set(Arc::new(move || {
        let _ = tx.send(PumpWake::Mail);
    }));
}

/// The pumped counterpart of [`await_internal_signal`] (ADR-0161
/// §Decision 2). A chassis driver that owns a [`PumpedSlot`] blocks here
/// waiting for a chain to settle while remaining able to pump its own
/// slot — the deadlock the ADR's Context names: draw / capture mail
/// addressed to the pumped mailbox is on the very chain being awaited, so
/// a slot pumped only at its normal pump point can never settle it.
///
/// Reads the unified [`PumpWake`] channel both producers feed:
///
/// - [`PumpWake::Mail`] → [`PumpedSlot::drain_available`] and keep
///   waiting. This wake is the *mechanism*: the drain is driven by mail
///   arrival, not by the round timer.
/// - [`PumpWake::Settled`] → [`WaitOutcome::Settled`].
///
/// The escalating-patience bookkeeping is identical to
/// [`await_internal_signal`]: `round_budget` is the warn-checkpoint
/// cadence (clamped off zero so the loop can't spin), `cumulative_cap`
/// the total patience before a wedge, a `Disconnected` channel is an
/// attributable wedge with [`GateFailure::Disconnected`] set, and the
/// terminal `disposition` is dispensed the same way (`Panic` diverges via
/// `panic!`; the rest ride back in [`WaitOutcome::Wedged`]).
///
/// Per ADR-0161 the round budget is strictly a *log cadence*, not a pump
/// cadence — per-round pumping is rejected because settlement is gated on
/// the very mail that would sit waiting out the round, so the timeout arm
/// only logs and re-arms; it never drains. The wake is the sole drain
/// mechanism, and the parked-driver hole the ADR names is closed at the
/// driver (a `ControlFlow::WaitUntil(capture_deadline)` read), not here.
pub fn await_settlement_pumped<A>(
    wake_rx: &Receiver<PumpWake>,
    slot: &mut PumpedSlot<A>,
    gate: &str,
    round_budget: Duration,
    cumulative_cap: Duration,
    disposition: TerminalDisposition,
) -> WaitOutcome
where
    A: NativeActor,
{
    let round = round_budget.max(Duration::from_millis(1));
    let start = Instant::now();
    loop {
        match wake_rx.recv_timeout(round) {
            Ok(PumpWake::Settled) => return WaitOutcome::Settled,
            Ok(PumpWake::Mail) => {
                // The load-bearing arm: mail arrived on the pumped mailbox
                // while the driver blocked here, so drain it — a chain gated
                // on one of this slot's handlers advances only because this
                // pump runs.
                slot.drain_available();
            }
            Err(RecvTimeoutError::Timeout) => {
                let waited = start.elapsed();
                if waited >= cumulative_cap {
                    return wedge(gate, waited, GateFailure::Silent, disposition);
                }
                tracing::warn!(
                    target: "aether_substrate::settlement",
                    gate,
                    waited_millis = waited.as_millis(),
                    cap_millis = cumulative_cap.as_millis(),
                    "gate {gate} slow: waited {waited:?}, extending",
                );
            }
            Err(RecvTimeoutError::Disconnected) => {
                return wedge(gate, start.elapsed(), GateFailure::Disconnected, disposition);
            }
        }
    }
}

#[cfg(test)]
// Settlement tests hold per-test `Mutex` guards across the assertion
// sequence so the captured state stays consistent against the
// concurrent firing thread.
#[allow(clippy::significant_drop_tightening)]
#[allow(
    clippy::unwrap_used,
    reason = "test-setup unwraps: fixture construction and decode panic on failure is the assertion"
)]
#[allow(clippy::disallowed_methods)] // test scaffolding — threads here hold no settlement contract
mod tests {
    use super::*;
    use crate::mail::mailer::Mailer;
    use crate::mail::registry::OwnedDispatch;
    use crate::mail::registry::Registry;
    use crate::testing::boot_authority;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;

    fn root(sender: u64, cid: u64) -> MailId {
        MailId { sender: MailboxId(sender), correlation_id: cid }
    }

    /// One captured dispatch — what the test asserts against.
    #[derive(Clone, Debug, PartialEq, Eq)]
    struct CapturedDispatch {
        kind: KindId,
        payload: Vec<u8>,
        count: u32,
    }

    /// Build a fresh `Mailer` backed by a registry + handle store
    /// pair. Registers a closure-bound sink under `sink_name` that
    /// captures the dispatched mails into a shared buffer the test
    /// asserts against. Returns the mailer, the registered sink's
    /// mailbox id, and the buffer.
    fn fresh_mailer_with_sink(sink_name: &str) -> (Arc<Mailer>, MailboxId, Arc<StdMutex<Vec<CapturedDispatch>>>) {
        let registry = Arc::new(Registry::new());
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
        let captured: Arc<StdMutex<Vec<CapturedDispatch>>> = Arc::new(StdMutex::new(Vec::new()));
        let captured_clone = Arc::clone(&captured);
        let target = registry.register_inbox(
            &boot_authority(),
            sink_name,
            // iamacoffeepot/aether#848 PR 3: take `OwnedDispatch`
            // directly and move payload into the captured row
            // (was: `payload.to_vec()` clone via the legacy
            // borrowed-dispatch shape).
            Arc::new(move |dispatch: OwnedDispatch| {
                // ADR-0094: terminal test consumer — discharge before the
                // partial-move of `payload` below.
                dispatch.discharge();
                captured_clone.lock().unwrap().push(CapturedDispatch {
                    kind: dispatch.kind,
                    payload: dispatch.payload.into_vec(),
                    count: dispatch.count,
                });
            }),
        );
        (mailer, target, captured)
    }

    #[test]
    fn subscribe_then_fire_wakes_receiver() {
        let reg = SettlementRegistry::new();
        let r = root(1, 1);
        let rx = reg.subscribe_settlement(r);
        assert_eq!(reg.pending_count(), 1);
        reg.fire_settled(r);
        assert_eq!(reg.pending_count(), 0);
        assert_eq!(reg.settled_count(), 1);
        rx.recv().expect("settlement signal");
    }

    #[test]
    fn fire_then_subscribe_pre_fires_receiver() {
        let reg = SettlementRegistry::new();
        let r = root(1, 1);
        reg.fire_settled(r);
        assert_eq!(reg.settled_count(), 1);
        let rx = reg.subscribe_settlement(r);
        // Subscriber landed in the settled-set fast path — no
        // pending entry was added.
        assert_eq!(reg.pending_count(), 0);
        rx.recv().expect("pre-fired signal");
    }

    #[test]
    fn multiple_subscribers_all_wake() {
        let reg = SettlementRegistry::new();
        let r = root(1, 1);
        let rx1 = reg.subscribe_settlement(r);
        let rx2 = reg.subscribe_settlement(r);
        let rx3 = reg.subscribe_settlement(r);
        assert_eq!(reg.pending_count(), 3);
        reg.fire_settled(r);
        rx1.recv().expect("subscriber 1 wakes");
        rx2.recv().expect("subscriber 2 wakes");
        rx3.recv().expect("subscriber 3 wakes");
    }

    #[test]
    fn fire_twice_is_idempotent() {
        let reg = SettlementRegistry::new();
        let r = root(1, 1);
        let rx = reg.subscribe_settlement(r);
        reg.fire_settled(r);
        reg.fire_settled(r);
        // First fire wakes the subscriber; second is a no-op for the
        // already-drained pending entry.
        rx.recv().expect("first fire wakes");
        assert_eq!(reg.settled_count(), 1);
    }

    #[test]
    fn distinct_roots_are_independent() {
        let reg = SettlementRegistry::new();
        let r1 = root(1, 1);
        let r2 = root(1, 2);
        let rx1 = reg.subscribe_settlement(r1);
        let rx2 = reg.subscribe_settlement(r2);
        reg.fire_settled(r1);
        rx1.recv().expect("r1 wakes");
        // r2's subscriber stays parked.
        assert!(rx2.try_recv().is_err());
        reg.fire_settled(r2);
        rx2.recv().expect("r2 wakes");
    }

    /// `subscribe_settlement_mail` then `fire_settled`: one mail is
    /// pushed to the subscribed target with the expected `(kind,
    /// payload-decodes-to-root)`.
    #[test]
    fn subscribe_mail_then_fire_pushes_notification() {
        let reg = SettlementRegistry::new();
        let (mailer, target, captured) = fresh_mailer_with_sink("test.settlement.subscribe_fire");
        let r = root(1, 1);
        let kind = KindId(0xABCD);

        reg.subscribe_settlement_mail(r, target, kind, Arc::clone(&mailer));
        assert_eq!(reg.pending_mail_count(), 1);
        reg.fire_settled(r);
        assert_eq!(reg.pending_mail_count(), 0);

        let captured = captured.lock().unwrap();
        assert_eq!(captured.len(), 1);
        let mail = &captured[0];
        assert_eq!(mail.kind, kind);
        assert_eq!(mail.count, 1);
        let decoded = Settled::decode_from_bytes(&mail.payload).expect("decode Settled").root;
        assert_eq!(decoded, r);
    }

    /// `fire_settled` first, then `subscribe_settlement_mail`: the
    /// notification pre-fires synchronously.
    #[test]
    fn fire_then_subscribe_mail_pre_fires() {
        let reg = SettlementRegistry::new();
        let (mailer, target, captured) = fresh_mailer_with_sink("test.settlement.fire_subscribe");
        let r = root(2, 4);
        let kind = KindId(0x1234);

        reg.fire_settled(r);
        assert!(captured.lock().unwrap().is_empty());

        reg.subscribe_settlement_mail(r, target, kind, Arc::clone(&mailer));
        // Pre-fire path: no parked entry should remain.
        assert_eq!(reg.pending_mail_count(), 0);

        let captured = captured.lock().unwrap();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].kind, kind);
        let decoded = Settled::decode_from_bytes(&captured[0].payload).expect("decode Settled").root;
        assert_eq!(decoded, r);
    }

    /// Three mail subscribers on the same root all receive a
    /// notification when `fire_settled` runs.
    #[test]
    fn multiple_mail_subscribers_all_receive() {
        let reg = SettlementRegistry::new();
        let (mailer, target, captured) = fresh_mailer_with_sink("test.settlement.multi");
        let r = root(3, 9);
        let kind = KindId(0x5555);

        reg.subscribe_settlement_mail(r, target, kind, Arc::clone(&mailer));
        reg.subscribe_settlement_mail(r, target, kind, Arc::clone(&mailer));
        reg.subscribe_settlement_mail(r, target, kind, Arc::clone(&mailer));
        assert_eq!(reg.pending_mail_count(), 3);

        reg.fire_settled(r);
        assert_eq!(reg.pending_mail_count(), 0);

        let captured = captured.lock().unwrap();
        assert_eq!(captured.len(), 3);
        for entry in captured.iter() {
            assert_eq!(entry.kind, kind);
            let decoded = Settled::decode_from_bytes(&entry.payload).expect("decode Settled").root;
            assert_eq!(decoded, r);
        }
    }

    /// A channel subscriber and a mail subscriber on the same root
    /// both fire when `fire_settled` runs.
    #[test]
    fn channel_and_mail_subscribers_coexist() {
        let reg = SettlementRegistry::new();
        let (mailer, target, captured) = fresh_mailer_with_sink("test.settlement.coexist");
        let r = root(4, 16);
        let kind = KindId(0x7777);

        let rx = reg.subscribe_settlement(r);
        reg.subscribe_settlement_mail(r, target, kind, Arc::clone(&mailer));
        assert_eq!(reg.pending_count(), 1);
        assert_eq!(reg.pending_mail_count(), 1);

        reg.fire_settled(r);
        assert_eq!(reg.pending_count(), 0);
        assert_eq!(reg.pending_mail_count(), 0);

        rx.recv().expect("channel subscriber wakes");
        let captured = captured.lock().unwrap();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].kind, kind);
    }

    /// Mail subscribers on distinct roots fire independently — settling
    /// r1 does not fire r2's mail subscription.
    #[test]
    fn distinct_roots_independent_for_mail() {
        let reg = SettlementRegistry::new();
        let (mailer, target, captured) = fresh_mailer_with_sink("test.settlement.distinct");
        let r1 = root(5, 25);
        let r2 = root(5, 36);
        let kind = KindId(0x9999);

        reg.subscribe_settlement_mail(r1, target, kind, Arc::clone(&mailer));
        reg.subscribe_settlement_mail(r2, target, kind, Arc::clone(&mailer));
        assert_eq!(reg.pending_mail_count(), 2);

        reg.fire_settled(r1);
        assert_eq!(reg.pending_mail_count(), 1);

        let after_r1 = captured.lock().unwrap().clone();
        assert_eq!(after_r1.len(), 1);
        let decoded = Settled::decode_from_bytes(&after_r1[0].payload).expect("decode Settled").root;
        assert_eq!(decoded, r1);

        reg.fire_settled(r2);
        assert_eq!(reg.pending_mail_count(), 0);

        let after_r2 = captured.lock().unwrap().clone();
        assert_eq!(after_r2.len(), 2);
        let decoded = Settled::decode_from_bytes(&after_r2[1].payload).expect("decode Settled").root;
        assert_eq!(decoded, r2);
    }

    /// Resolves-within-cap: the signal fires after one round elapses
    /// (exercising the re-arm path), and the helper returns `Settled`.
    #[test]
    fn await_internal_signal_resolves_after_rearm() {
        let (tx, rx) = bounded::<()>(1);
        // Fire from a sibling thread after roughly one round budget so
        // the first `recv_timeout` times out (logging + re-arm) and the
        // second resolves.
        let handle = thread::spawn(move || {
            thread::sleep(Duration::from_millis(30));
            let _ = tx.try_send(());
        });
        let outcome = await_internal_signal(
            &rx,
            "test.rearm",
            Duration::from_millis(10),
            Duration::from_secs(5),
            TerminalDisposition::Proceed,
            None,
        );
        handle.join().expect("firing thread joins");
        assert!(matches!(outcome, WaitOutcome::Settled));
    }

    /// Cap-exhaustion: the signal never fires, so the helper exhausts
    /// the cumulative cap and returns a `Silent` `Wedged` for a
    /// non-`Panic` disposition.
    #[test]
    fn await_internal_signal_cap_exhaustion_wedges() {
        // Hold the sender alive so the channel doesn't disconnect —
        // this is the silent-to-cap path, distinct from `Disconnected`.
        let (_tx, rx) = bounded::<()>(1);
        let outcome = await_internal_signal(
            &rx,
            "test.cap",
            Duration::from_millis(5),
            Duration::from_millis(20),
            TerminalDisposition::ReplyErr,
            None,
        );
        match outcome {
            WaitOutcome::Wedged(w) => {
                assert_eq!(w.failure, GateFailure::Silent);
                assert_eq!(w.gate, "test.cap");
                assert!(w.waited >= Duration::from_millis(20));
            }
            WaitOutcome::Settled => panic!("expected a wedge, got Settled"),
        }
    }

    /// `Disconnected`: dropping the sender takes the same terminal path
    /// as cap-exhaustion, with `GateFailure::Disconnected` set.
    #[test]
    fn await_internal_signal_disconnect_wedges() {
        let (tx, rx) = bounded::<()>(1);
        drop(tx);
        let outcome = await_internal_signal(
            &rx,
            "test.disconnect",
            Duration::from_millis(50),
            Duration::from_secs(5),
            TerminalDisposition::Proceed,
            None,
        );
        match outcome {
            WaitOutcome::Wedged(w) => {
                assert_eq!(w.failure, GateFailure::Disconnected);
                assert_eq!(w.gate, "test.disconnect");
            }
            WaitOutcome::Settled => panic!("expected a wedge, got Settled"),
        }
    }

    /// `Panic` disposition diverges inside the helper on a wedge —
    /// asserts the gate fails attributably at the gate site.
    #[test]
    #[should_panic(expected = "gate test.panic wedged")]
    fn await_internal_signal_panic_disposition_diverges() {
        let (tx, rx) = bounded::<()>(1);
        drop(tx);
        let _ = await_internal_signal(
            &rx,
            "test.panic",
            Duration::from_millis(5),
            Duration::from_millis(20),
            TerminalDisposition::Panic,
            None,
        );
    }

    /// The settlement-notice payload decodes back to the
    /// subscribed root — direct check of the wire contract.
    #[test]
    fn mail_payload_decodes_to_root() {
        let reg = SettlementRegistry::new();
        let (mailer, target, captured) = fresh_mailer_with_sink("test.settlement.payload");
        let r = root(7, 49);
        let kind = KindId(0x4321);

        reg.subscribe_settlement_mail(r, target, kind, Arc::clone(&mailer));
        reg.fire_settled(r);

        let captured = captured.lock().unwrap();
        assert_eq!(captured.len(), 1);
        let decoded = Settled::decode_from_bytes(&captured[0].payload).expect("decode Settled").root;
        assert_eq!(decoded, r);
    }

    /// The settled window is bounded and evicts oldest-first (issue
    /// 2618): overfilling the registry never grows the remembered set
    /// past the cap, and the most recent root still pre-fires a late
    /// subscriber.
    ///
    /// Tripwire: without the per-cell eviction, `settled_count` tracks
    /// the total fired count (the pre-2618 unbounded growth); with
    /// eviction ordered wrongly (evict-newest), the recent-root
    /// pre-fire fails.
    #[test]
    fn settled_window_is_bounded_and_recent_roots_prefire() {
        let reg = SettlementRegistry::new();
        let cap = CELL_COUNT * SETTLED_CAP_PER_CELL;
        let total = cap + cap / 4;
        for cid in 0..total {
            reg.fire_settled(root(7, cid as u64));
        }
        assert!(reg.settled_count() <= cap, "settled window exceeded its bound: {} > {cap}", reg.settled_count());
        // The most recently settled root is inside every cell's window,
        // so a late subscriber still pre-fires.
        let rx = reg.subscribe_settlement(root(7, (total - 1) as u64));
        assert!(rx.try_recv().is_ok(), "recent root should pre-fire a late subscriber");
    }

    /// ADR-0161 §Decision 2: `subscribe_settlement_with` runs its callback
    /// exactly once per settlement. `fire_settled` drains the subscriber, so a
    /// second fire — the idempotent duplicate-settle case — does not re-run it.
    #[test]
    fn subscribe_with_callback_fires_exactly_once() {
        let reg = SettlementRegistry::new();
        let r = root(8, 64);
        let count = Arc::new(AtomicUsize::new(0));
        let count_for_cb = Arc::clone(&count);
        reg.subscribe_settlement_with(r, move || {
            count_for_cb.fetch_add(1, Ordering::SeqCst);
        });
        reg.fire_settled(r);
        // A duplicate settle (ADR-0080 §6 hint semantics) must not re-fire.
        reg.fire_settled(r);
        assert_eq!(count.load(Ordering::SeqCst), 1, "the callback fired exactly once");
    }

    /// ADR-0161 §Decision 2: subscribing to an already-settled root pre-fires
    /// the callback synchronously (once), mirroring the channel form's
    /// pre-fire.
    #[test]
    fn subscribe_with_callback_pre_fires_when_already_settled() {
        let reg = SettlementRegistry::new();
        let r = root(9, 81);
        reg.fire_settled(r);
        let count = Arc::new(AtomicUsize::new(0));
        let count_for_cb = Arc::clone(&count);
        reg.subscribe_settlement_with(r, move || {
            count_for_cb.fetch_add(1, Ordering::SeqCst);
        });
        assert_eq!(count.load(Ordering::SeqCst), 1, "the pre-fire ran the callback once");
    }

    /// ADR-0161 §Decision 2: `install_pump_wake` installs a hook on the
    /// [`MailboxWakeSlot`] that sends [`PumpWake::Mail`] each time it fires —
    /// the plumbing a pumped slot's mailbox uses to nudge the pumped wait.
    #[test]
    fn install_pump_wake_sends_mail_on_each_fire() {
        let slot = MailboxWakeSlot::default();
        let (tx, rx) = crossbeam_channel::unbounded::<PumpWake>();
        install_pump_wake(&slot, tx);
        let hook = slot.get().expect("the pump wake hook is installed");
        hook();
        hook();
        assert_eq!(rx.recv().expect("first wake"), PumpWake::Mail);
        assert_eq!(rx.recv().expect("second wake"), PumpWake::Mail);
    }
}
