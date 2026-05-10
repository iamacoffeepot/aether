//! ADR-0080 substrate-wide mail tracing — chassis-side runtime.
//!
//! Two pieces:
//!
//! - **Producer-side helpers** (`record_sent` / `record_received` /
//!   `record_finished`) push [`TraceEvent`]s onto a process-global
//!   [`crossbeam_queue::SegQueue`]. The hooks live in
//!   [`crate::actor::native::binding::NativeBinding::send_mail_with_lineage`]
//!   (Sent), the native dispatcher trampoline (Received / Finished),
//!   and the wasm trampoline forwarder (Received / Finished). Calls
//!   are no-ops until the chassis [`install_trace_queue`]s the
//!   queue at boot — silent drop is the right shape for tests that
//!   don't bring up the chassis.
//!
//! - **[`start_drainer`]** spawns the chassis-owned drainer thread.
//!   The thread loop-drains up to `BATCH_MAX` events and ships a
//!   [`BatchedTraceEvents`] mail to the [`TRACE_OBSERVER_MAILBOX_NAME`]
//!   sink via the [`Mailer`]; it parks for `BATCH_INTERVAL` between
//!   drains. Shutdown is via channel-drop on the returned
//!   [`TraceDrainerHandle`] — its `Drop` signals the thread and
//!   joins.
//!
//! Why global rather than chassis-owned: producer-side hooks are
//! reached from every actor's send path; threading an
//! `Arc<SegQueue<TraceEvent>>` through every binding constructor is
//! invasive churn for a feature that runs at most once per process.
//! The global is initialised exactly once at chassis boot and never
//! reset — multi-chassis test fixtures (TestBench) share the queue
//! across their substrates, which is fine because each TraceEvent
//! carries the producer's [`MailboxId`] so the observer can attribute
//! events even if the queue is shared.

use std::sync::OnceLock;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use aether_data::{KindId, MailId, MailboxId};
use aether_kinds::trace::{BatchedTraceEvents, Nanos, TRACE_OBSERVER_MAILBOX_NAME, TraceEvent};
use crossbeam_queue::SegQueue;

use crate::mail::Mail;
use crate::mail::mailer::Mailer;

/// Monotonic-since-boot reference. Set once at chassis boot via
/// [`init_substrate_start`]; producer-side hooks subtract from it to
/// produce a [`Nanos`] for each event.
static SUBSTRATE_START: OnceLock<Instant> = OnceLock::new();

/// Process-global trace queue. Set by [`install_trace_queue`] at
/// chassis boot; producer-side hooks push when present, no-op when
/// absent. Wrapped in `OnceLock<Arc<SegQueue>>` so the drainer thread
/// can hold a clone for `pop` without contending on the OnceLock
/// itself on every read.
static TRACE_QUEUE: OnceLock<Arc<SegQueue<TraceEvent>>> = OnceLock::new();

/// Initialise the [`SUBSTRATE_START`] reference. Called once during
/// chassis boot before any actor is wired so every subsequent
/// timestamp has a stable origin. Safe to call multiple times — the
/// `OnceLock` ignores subsequent calls.
pub fn init_substrate_start() {
    let _ = SUBSTRATE_START.set(Instant::now());
}

/// Install the process-global trace queue. The chassis builder
/// constructs the queue + drainer pair at boot via
/// [`start_drainer`]; this function exposes the queue handle so
/// producer-side hooks can find it. Safe to call multiple times —
/// subsequent calls return the existing queue.
pub fn install_trace_queue(queue: Arc<SegQueue<TraceEvent>>) {
    let _ = TRACE_QUEUE.set(queue);
}

/// Read the process-global trace queue, if installed. Returns `None`
/// before chassis boot or in tests that bypass the chassis.
pub fn trace_queue() -> Option<&'static Arc<SegQueue<TraceEvent>>> {
    TRACE_QUEUE.get()
}

/// Compute the current [`Nanos`] timestamp relative to
/// [`SUBSTRATE_START`]. `0` if `SUBSTRATE_START` was not initialised
/// (the producer hooks early-out before this in that case, but the
/// guard makes the function safe to call standalone).
pub fn now_nanos() -> Nanos {
    let start = match SUBSTRATE_START.get() {
        Some(s) => *s,
        None => return Nanos(0),
    };
    let elapsed = Instant::now().saturating_duration_since(start);
    Nanos(elapsed.as_nanos() as u64)
}

/// ADR-0080 §2 producer hook for the `Sent` event. No-op if the
/// global queue isn't installed yet (early-test paths, tests that
/// bypass chassis boot). Allocates and pushes only when the queue
/// is live.
pub fn record_sent(
    mail_id: MailId,
    root: MailId,
    parent_mail: Option<MailId>,
    sender: MailboxId,
    recipient: MailboxId,
    kind: KindId,
) {
    let Some(queue) = TRACE_QUEUE.get() else {
        return;
    };
    queue.push(TraceEvent::Sent {
        mail_id,
        root,
        parent_mail,
        sender,
        recipient,
        kind,
        t: now_nanos(),
    });
}

/// ADR-0080 §2 producer hook for the `Received` event. Pushed by the
/// native dispatcher trampoline at handler entry. No-op when
/// `mail_id == MailId::NONE` — that's the structural recursion break
/// per ADR-0080 §7: the drainer's own outbound `BatchedTraceEvents`
/// mail is pushed bare through the `Mailer` (skipping
/// `NativeBinding::send_mail_with_lineage`), so it carries the
/// default `MailId::NONE`. Suppressing `Received`/`Finished` for
/// `NONE`-stamped mail prevents the observer's own dispatch from
/// generating events that would re-feed the drainer.
pub fn record_received(mail_id: MailId) {
    if mail_id == MailId::NONE {
        return;
    }
    let Some(queue) = TRACE_QUEUE.get() else {
        return;
    };
    queue.push(TraceEvent::Received {
        mail_id,
        t: now_nanos(),
    });
}

/// ADR-0080 §2 producer hook for the `Finished` event. Pushed by the
/// native dispatcher trampoline at handler exit (normal return).
/// Symmetric `MailId::NONE` short-circuit (see [`record_received`]).
pub fn record_finished(mail_id: MailId) {
    if mail_id == MailId::NONE {
        return;
    }
    let Some(queue) = TRACE_QUEUE.get() else {
        return;
    };
    queue.push(TraceEvent::Finished {
        mail_id,
        t: now_nanos(),
    });
}

/// ADR-0080 chassis-root push helper. Combines `MailId` minting,
/// the `Sent` trace event emission, and the `Mailer::push` into one
/// call so chassis-side mail (Tick fanout from the frame loop, hub-
/// bridged inbound, MCP-bridged) gets observable lineage without
/// duplicating the producer-side hook in `NativeBinding::send_mail_with_lineage`.
///
/// Returns the freshly minted `MailId` so the caller can subscribe
/// to its settlement via the chassis [`crate::chassis::settlement::SettlementRegistry`]
/// before waiting on the chain.
///
/// `correlation_id` is allocated by the caller (the chassis owns its
/// own `AtomicU64` counter, symmetric with each per-actor `NativeBinding`'s
/// counter). Pre-PR 4 the test bench / hub minters were the only
/// chassis-side counters; this helper shapes them as ADR-0080 §1
/// chassis-root MailIds.
pub fn push_chassis_root_mail(
    mailer: &Mailer,
    correlation_id: u64,
    recipient: MailboxId,
    kind: KindId,
    payload: Vec<u8>,
    count: u32,
) -> MailId {
    let mail_id = MailId::new(MailboxId::CHASSIS_MAILBOX_ID, correlation_id);
    record_sent(
        mail_id,
        mail_id,
        None,
        MailboxId::CHASSIS_MAILBOX_ID,
        recipient,
        kind,
    );
    mailer.push(Mail::new(recipient, kind, payload, count).with_lineage(mail_id, mail_id, None));
    mail_id
}

/// ADR-0080 §3 batch parameters: drain at most this many events per
/// drainer cycle. Picked to amortise observer dispatch (~1k observer
/// mails/sec at the busy-scene baseline of ~33k events/sec).
const BATCH_MAX: usize = 256;

/// ADR-0080 §3 drainer cadence: park between drains.
const BATCH_INTERVAL: Duration = Duration::from_millis(1);

/// Lifetime guard for the trace drainer thread. Held by the chassis;
/// dropping joins the thread.
pub struct TraceDrainerHandle {
    /// Signalled by `Drop` (channel disconnect → drainer exits its
    /// `recv_timeout` loop). Wrapped in `Option` so `Drop` can take
    /// it without consuming `self`.
    shutdown_tx: Option<Sender<()>>,
    /// Wrapped in `Mutex<Option>` so `Drop` can take ownership of the
    /// `JoinHandle` without consuming `self`. The mutex is for the
    /// `&mut self` borrow shape — only `Drop` ever reaches in.
    join_handle: Mutex<Option<JoinHandle<()>>>,
}

impl Drop for TraceDrainerHandle {
    fn drop(&mut self) {
        // Disconnect the shutdown channel — the drainer's
        // `recv_timeout` returns `Err(Disconnected)` and the loop
        // exits.
        drop(self.shutdown_tx.take());
        if let Ok(mut guard) = self.join_handle.lock()
            && let Some(handle) = guard.take()
        {
            let _ = handle.join();
        }
    }
}

/// Spawn the chassis-owned drainer thread. The thread loop-drains up
/// to [`BATCH_MAX`] events from the trace queue every
/// [`BATCH_INTERVAL`] and ships a [`BatchedTraceEvents`] mail to the
/// [`TRACE_OBSERVER_MAILBOX_NAME`] sink. Returns a handle whose
/// `Drop` joins the thread.
///
/// Idempotent in the sense that calling [`install_trace_queue`]
/// twice with the same `queue` Arc is a no-op; the chassis builder
/// installs the queue exactly once and pairs it with one drainer.
pub fn start_drainer(queue: Arc<SegQueue<TraceEvent>>, mailer: Arc<Mailer>) -> TraceDrainerHandle {
    let (shutdown_tx, shutdown_rx) = channel::<()>();
    let handle = thread::Builder::new()
        .name("aether-trace-drainer".to_owned())
        .spawn(move || drainer_loop(queue, mailer, shutdown_rx))
        .expect("spawn aether-trace-drainer thread");
    TraceDrainerHandle {
        shutdown_tx: Some(shutdown_tx),
        join_handle: Mutex::new(Some(handle)),
    }
}

fn drainer_loop(queue: Arc<SegQueue<TraceEvent>>, mailer: Arc<Mailer>, shutdown_rx: Receiver<()>) {
    let recipient = aether_data::mailbox_id_from_name(TRACE_OBSERVER_MAILBOX_NAME);
    let kind = <BatchedTraceEvents as aether_data::Kind>::ID;

    loop {
        ship_batch(&queue, &mailer, recipient, kind);

        // Park BATCH_INTERVAL or until shutdown.
        match shutdown_rx.recv_timeout(BATCH_INTERVAL) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => {
                // One last drain on shutdown so events queued just
                // before signal don't get lost.
                ship_batch(&queue, &mailer, recipient, kind);
                break;
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
}

/// Drain up to [`BATCH_MAX`] events and ship one
/// [`BatchedTraceEvents`] mail. Skipped silently if the
/// [`TRACE_OBSERVER_MAILBOX_NAME`] sink is not registered on this
/// substrate's [`crate::mail::registry::Registry`] — that's the
/// case in unit tests that boot a chassis without
/// `TraceObserverCapability`, and the bubble-up that would otherwise
/// fire for the unknown mailbox would interfere with the test's own
/// outbound assertions. Production chassis register the observer so
/// the drop branch never fires.
fn ship_batch(
    queue: &Arc<SegQueue<TraceEvent>>,
    mailer: &Arc<Mailer>,
    recipient: MailboxId,
    kind: aether_data::KindId,
) {
    let mut batch = Vec::with_capacity(BATCH_MAX);
    for _ in 0..BATCH_MAX {
        match queue.pop() {
            Some(event) => batch.push(event),
            None => break,
        }
    }
    if batch.is_empty() {
        return;
    }
    if !mailer
        .registry()
        .entry(recipient)
        .map(|e| matches!(e, crate::mail::registry::MailboxEntry::Closure(_)))
        .unwrap_or(false)
    {
        // Observer not registered (or dropped); silently discard the
        // batch. Test isolation: a chassis without
        // `TraceObserverCapability` should not surface trace mail as
        // an `UnresolvedMail` egress event on its outbound.
        return;
    }
    let envelope = BatchedTraceEvents { events: batch };
    let payload = match postcard::to_allocvec(&envelope) {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(
                target: "aether_substrate::trace",
                error = %e,
                "trace drainer: postcard encode failed; events dropped"
            );
            return;
        }
    };
    // Drainer-originated mail is chassis-root; no inheritance,
    // no `parent_mail`. We push directly through the Mailer
    // (no `Sender::send_detached` wrapper) — the recursion
    // break (ADR-0080 §7) is structural here: the producer
    // hook in `NativeBinding::send_mail_with_lineage` is what
    // would push a `TraceEvent::Sent` for an outbound, and we
    // bypass that path by going straight to `Mailer::push`.
    mailer.push(Mail::new(recipient, kind, payload, 1));
}
