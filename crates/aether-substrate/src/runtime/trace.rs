//! ADR-0080 substrate-wide mail tracing — chassis-side runtime.
//!
//! Two pieces:
//!
//! - **Producer-side helpers** ([`TraceHandle::record_sent`] /
//!   [`TraceHandle::record_received`] / [`TraceHandle::record_finished`])
//!   push [`TraceEvent`]s onto the per-chassis [`SegQueue`] held in
//!   the [`TraceHandle`]. The handle is owned by the chassis
//!   [`Mailer`] and reached via [`Mailer::trace_handle`]; producer-side
//!   call sites usually reach for the `mailer.record_sent(...)`
//!   shortcuts. Calls are no-ops when the chassis hasn't installed a
//!   handle — silent drop is the right shape for tests that don't
//!   bring up the chassis.
//!
//! - **[`start_drainer`]** spawns the chassis-owned drainer thread.
//!   The thread loop-drains up to `BATCH_MAX` events and ships a
//!   [`BatchedTraceEvents`] mail to the [`TRACE_OBSERVER_MAILBOX_NAME`]
//!   sink via the [`Mailer`]; it parks for `BATCH_INTERVAL` between
//!   drains. Shutdown is via channel-drop on the returned
//!   [`TraceDrainerHandle`] — its `Drop` signals the thread and
//!   joins.
//!
//! Why per-chassis rather than process-global: the trace pipeline is
//! per-chassis everywhere else (drainer is per-`build_passive`,
//! observer is a regular cap actor, queue is allocated fresh each
//! boot). A process-global `OnceLock` for the queue pinned the *first*
//! chassis's queue forever — subsequent chassis boots in the same
//! process silently wired their drainer to a fresh empty Arc while
//! producers kept pushing into the orphaned first Arc, so trace events
//! never reached the second chassis's observer. The handle now rides
//! on the Mailer (one per chassis), which makes the lifecycle
//! structurally correct and unlocks future in-process multi-substrate
//! workflows. Filed as iamacoffeepot/aether#953.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use aether_data::{KindId, MailId, MailboxId};
use aether_kinds::trace::{BatchedTraceEvents, Nanos, TRACE_OBSERVER_MAILBOX_NAME, TraceEvent};
use crossbeam_queue::SegQueue;

use crate::mail::Mail;
use crate::mail::mailer::Mailer;
use crate::mail::registry::MailboxEntry;

/// Per-chassis trace-pipeline handle. Owned by the chassis [`Mailer`];
/// producer-side hooks reach it via `mailer.trace_handle()` or the
/// `mailer.record_*` shortcuts that wrap the methods on this type.
///
/// Cloning shares the underlying [`SegQueue`] (Arc) and copies the
/// boot-time anchor (Copy), so the chassis drainer and every producer
/// site can hold an independent `TraceHandle` cheaply.
#[derive(Clone, Debug)]
pub struct TraceHandle {
    queue: Arc<SegQueue<TraceEvent>>,
    boot_time: Instant,
}

impl TraceHandle {
    /// Build a fresh handle: empty queue, `Instant::now()` boot
    /// anchor. The chassis builder calls this once per `build_passive`;
    /// the returned handle is installed on the chassis [`Mailer`] and
    /// its queue is handed to [`start_drainer`].
    #[must_use]
    pub fn new() -> Self {
        Self::with_queue(Arc::new(SegQueue::new()))
    }

    /// Build a handle that wraps an existing queue Arc. Used by tests
    /// that boot multiple `Mailer`s pointing at the same queue so the
    /// test body can drain a single shared event stream and filter by
    /// `mail_id.sender`. Production chassis use [`Self::new`] instead;
    /// the queue is allocated fresh per chassis.
    #[must_use]
    pub fn with_queue(queue: Arc<SegQueue<TraceEvent>>) -> Self {
        Self {
            queue,
            boot_time: Instant::now(),
        }
    }

    /// Borrow the queue Arc so the chassis builder can pass it to
    /// [`start_drainer`]. Internal: producer-side call sites go
    /// through the `record_*` methods on this type.
    #[must_use]
    pub fn queue(&self) -> &Arc<SegQueue<TraceEvent>> {
        &self.queue
    }

    /// Boot-time anchor for [`Self::now_nanos`]. Exposed for fixtures
    /// that want to reconstruct timestamps for asserting.
    #[must_use]
    pub fn boot_time(&self) -> Instant {
        self.boot_time
    }

    /// Compute the current [`Nanos`] timestamp relative to this
    /// handle's boot anchor. Sub-microsecond resolution; saturates to
    /// `u64::MAX` after ~584 years of substrate uptime.
    #[must_use]
    pub fn now_nanos(&self) -> Nanos {
        let elapsed = Instant::now().saturating_duration_since(self.boot_time);
        // u128 → u64: trace timestamps overflow after ~584 years; the
        // handle's boot anchor is set at chassis boot, so realistic
        // runtimes are well within u64 range.
        #[allow(clippy::cast_possible_truncation)]
        Nanos(elapsed.as_nanos() as u64)
    }

    /// ADR-0080 §2 producer hook for the `Sent` event.
    pub fn record_sent(
        &self,
        mail_id: MailId,
        root: MailId,
        parent_mail: Option<MailId>,
        sender: MailboxId,
        recipient: MailboxId,
        kind: KindId,
    ) {
        self.queue.push(TraceEvent::Sent {
            mail_id,
            root,
            parent_mail,
            sender,
            recipient,
            kind,
            t: self.now_nanos(),
        });
    }

    /// ADR-0080 §2 producer hook for the `Received` event. Pushed by
    /// the native dispatcher trampoline at handler entry. No-op when
    /// `mail_id == MailId::NONE` — that's the structural recursion
    /// break per ADR-0080 §7: the drainer's own outbound
    /// `BatchedTraceEvents` mail is pushed bare through the `Mailer`
    /// (skipping `NativeBinding::send_mail_with_lineage`), so it
    /// carries the default `MailId::NONE`. Suppressing
    /// `Received`/`Finished` for `NONE`-stamped mail prevents the
    /// observer's own dispatch from generating events that would
    /// re-feed the drainer.
    ///
    /// Issue 734: dispatchers pass `std::thread::current().name()
    /// .map(str::to_owned)` as `thread_name` so the trace renderer
    /// (`hub::mcp::trace`) can stamp each event with a per-thread row.
    pub fn record_received(&self, mail_id: MailId, thread_name: Option<String>) {
        if mail_id == MailId::NONE {
            return;
        }
        self.queue.push(TraceEvent::Received {
            mail_id,
            t: self.now_nanos(),
            thread_name,
        });
    }

    /// ADR-0080 §2 producer hook for the `Finished` event. Pushed by
    /// the native dispatcher trampoline at handler exit (normal
    /// return). Symmetric `MailId::NONE` short-circuit (see
    /// [`Self::record_received`]).
    pub fn record_finished(&self, mail_id: MailId) {
        if mail_id == MailId::NONE {
            return;
        }
        self.queue.push(TraceEvent::Finished {
            mail_id,
            t: self.now_nanos(),
        });
    }

    /// ADR-0080 §12 / iamacoffeepot/aether#716: acquire a
    /// [`SettlementHold`] against `root`. The returned guard increments
    /// the root's `held_open` counter (via a [`TraceEvent::HoldOpen`]
    /// pushed inline) and decrements it again on `Drop` (via
    /// [`TraceEvent::Release`]). Settlement for `root` gates on
    /// `(in_flight == 0 && held_open == 0)`, so any thread that
    /// outlives its spawning handler (`InheritCtx<A>` from
    /// [`crate::actor::native::NativeCtx::spawn_inherit`]) keeps the
    /// chain open until it drops.
    ///
    /// Acquired on the parent thread before the worker is spawned so
    /// the `HoldOpen` event is visible in the trace queue before the
    /// parent handler's `Finished` lands. Moving the guard into the
    /// worker thread (via the `InheritCtx<A>` ctor) ties release to
    /// the worker's lifetime.
    #[must_use = "SettlementHold gates root settlement; storing _ silently fires Release"]
    pub fn acquire_settlement_hold(&self, root: MailId) -> SettlementHold {
        self.queue.push(TraceEvent::HoldOpen {
            root,
            t: self.now_nanos(),
        });
        SettlementHold {
            handle: self.clone(),
            root,
        }
    }
}

impl Default for TraceHandle {
    fn default() -> Self {
        Self::new()
    }
}

/// RAII guard for an ADR-0080 §12 settlement hold. Construct via
/// [`TraceHandle::acquire_settlement_hold`] (or
/// [`Mailer::acquire_settlement_hold`]); drop fires
/// [`TraceEvent::Release`] for the same root. The only public surface
/// is the guard — `Release` is never a free function, so a paired
/// `hold`/`release` mismatch is structurally impossible.
#[derive(Debug)]
pub struct SettlementHold {
    handle: TraceHandle,
    root: MailId,
}

impl SettlementHold {
    /// The root this hold gates. Exposed for diagnostics / tests; the
    /// hold itself releases via `Drop`.
    #[must_use]
    pub fn root(&self) -> MailId {
        self.root
    }
}

impl Drop for SettlementHold {
    fn drop(&mut self) {
        self.handle.queue.push(TraceEvent::Release {
            root: self.root,
            t: self.handle.now_nanos(),
        });
    }
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
/// to `BATCH_MAX` events (crate-internal const) from the trace queue
/// every `BATCH_INTERVAL` (also crate-internal) and ships a
/// [`BatchedTraceEvents`] mail to the [`TRACE_OBSERVER_MAILBOX_NAME`]
/// sink. Returns a handle whose `Drop` joins the thread.
///
/// # Panics
/// Panics if the OS refuses to spawn the drainer thread — fail-fast
/// per ADR-0063: thread spawn is a substrate-boot prerequisite and a
/// failure means the process is in an unrecoverable state.
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

// All arguments are taken by value so the spawned drainer thread
// owns them for its lifetime.
#[allow(clippy::needless_pass_by_value)]
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
    kind: KindId,
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
        .is_some_and(|e| matches!(e, MailboxEntry::Inbox(_)))
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
