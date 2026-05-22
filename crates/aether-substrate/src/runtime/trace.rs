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

use std::mem;
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

/// Number of per-root trace shards (power of two; mask is `N - 1`).
/// Sized generously so concurrent roots spread across distinct queues,
/// removing the single-`SegQueue` tail contention that dominated
/// saturated multi-worker mail (~50% of worker CPU). Each empty
/// `SegQueue` is cheap, so over-provisioning shards costs little.
const TRACE_SHARD_COUNT: usize = 64;

/// Per-chassis trace-event queue, sharded by causal root: every event
/// for a given root lands in one FIFO `SegQueue`.
///
/// ADR-0080 settlement counts on per-root ordering — a root's `Sent`
/// must be folded before its matching `Finished`, and its `HoldOpen`
/// before `Release` — or `in_flight`/`held_open` can hit a false zero
/// and fire `Settled` early. A `SegQueue` is FIFO and the producer
/// hooks push a root's events in happens-before order, so keeping a
/// whole root in one shard preserves that ordering exactly. Distinct
/// roots spread across shards, so concurrent workers emitting for
/// different roots no longer contend on one queue's tail.
#[derive(Debug)]
pub struct ShardedTraceQueue {
    shards: Box<[SegQueue<TraceEvent>]>,
    mask: u64,
}

impl ShardedTraceQueue {
    /// Allocate `TRACE_SHARD_COUNT` empty shards.
    #[must_use]
    pub fn new() -> Self {
        let shards = (0..TRACE_SHARD_COUNT)
            .map(|_| SegQueue::new())
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            shards,
            #[allow(clippy::cast_possible_truncation)]
            mask: TRACE_SHARD_COUNT as u64 - 1,
        }
    }

    /// Shard index for a causal root: a cheap mix of the root's two
    /// 64-bit words. `correlation_id` is a per-root incrementing counter
    /// (its low bits already round-robin across shards); folding in a
    /// scrambled `sender` keeps roots minted by the same mailbox spread.
    #[inline]
    #[allow(clippy::cast_possible_truncation)] // masked to < TRACE_SHARD_COUNT, fits usize
    fn shard_index(&self, root: MailId) -> usize {
        let h = root.sender.0.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ root.correlation_id;
        (h & self.mask) as usize
    }

    /// Push an event onto its root's shard.
    #[inline]
    pub fn push(&self, root: MailId, event: TraceEvent) {
        self.shards[self.shard_index(root)].push(event);
    }

    /// Pop one event from the first non-empty shard. Order *across*
    /// shards is unspecified — per-root order lives *within* a shard —
    /// so this is for tests that count or filter, not ones that rely on
    /// a global event order.
    #[must_use]
    pub fn pop(&self) -> Option<TraceEvent> {
        self.shards.iter().find_map(SegQueue::pop)
    }

    /// Re-enqueue an event that a test popped and wants to put back.
    /// Sharded by the event's root where it carries one
    /// (`Sent`/`HoldOpen`/`Release`); `Received`/`Finished` don't, so
    /// they land on shard 0 — harmless because [`Self::pop`] scans every
    /// shard and the restore-pattern tests don't assert cross-shard
    /// order. Not used on the production drain path.
    pub fn restore(&self, event: TraceEvent) {
        let shard = match &event {
            TraceEvent::Sent { root, .. }
            | TraceEvent::HoldOpen { root, .. }
            | TraceEvent::Release { root, .. } => self.shard_index(*root),
            TraceEvent::Received { .. } | TraceEvent::Finished { .. } => 0,
        };
        self.shards[shard].push(event);
    }
}

impl Default for ShardedTraceQueue {
    fn default() -> Self {
        Self::new()
    }
}

/// Per-chassis trace-pipeline handle. Owned by the chassis [`Mailer`];
/// producer-side hooks reach it via `mailer.trace_handle()` or the
/// `mailer.record_*` shortcuts that wrap the methods on this type.
///
/// Cloning shares the underlying [`SegQueue`] (Arc) and copies the
/// boot-time anchor (Copy), so the chassis drainer and every producer
/// site can hold an independent `TraceHandle` cheaply.
#[derive(Clone, Debug)]
pub struct TraceHandle {
    queue: Arc<ShardedTraceQueue>,
    boot_time: Instant,
}

impl TraceHandle {
    /// Build a fresh handle: empty queue, `Instant::now()` boot
    /// anchor. The chassis builder calls this once per `build_passive`;
    /// the returned handle is installed on the chassis [`Mailer`] and
    /// its queue is handed to [`start_drainer`].
    #[must_use]
    pub fn new() -> Self {
        Self::with_queue(Arc::new(ShardedTraceQueue::new()))
    }

    /// Build a handle that wraps an existing queue Arc. Used by tests
    /// that boot multiple `Mailer`s pointing at the same queue so the
    /// test body can drain a single shared event stream and filter by
    /// `mail_id.sender`. Production chassis use [`Self::new`] instead;
    /// the queue is allocated fresh per chassis.
    #[must_use]
    pub fn with_queue(queue: Arc<ShardedTraceQueue>) -> Self {
        Self {
            queue,
            boot_time: Instant::now(),
        }
    }

    /// Borrow the queue Arc so the chassis builder can pass it to
    /// [`start_drainer`]. Internal: producer-side call sites go
    /// through the `record_*` methods on this type.
    #[must_use]
    pub fn queue(&self) -> &Arc<ShardedTraceQueue> {
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
        self.queue.push(
            root,
            TraceEvent::Sent {
                mail_id,
                root,
                parent_mail,
                sender,
                recipient,
                kind,
                t: self.now_nanos(),
            },
        );
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
    pub fn record_received(&self, mail_id: MailId, root: MailId, thread_name: Option<String>) {
        if mail_id == MailId::NONE {
            return;
        }
        self.queue.push(
            root,
            TraceEvent::Received {
                mail_id,
                t: self.now_nanos(),
                thread_name,
            },
        );
    }

    /// ADR-0080 §2 producer hook for the `Finished` event. Pushed by
    /// the native dispatcher trampoline at handler exit (normal
    /// return). Symmetric `MailId::NONE` short-circuit (see
    /// [`Self::record_received`]).
    pub fn record_finished(&self, mail_id: MailId, root: MailId) {
        if mail_id == MailId::NONE {
            return;
        }
        self.queue.push(
            root,
            TraceEvent::Finished {
                mail_id,
                t: self.now_nanos(),
            },
        );
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
        self.queue.push(
            root,
            TraceEvent::HoldOpen {
                root,
                t: self.now_nanos(),
            },
        );
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
        self.handle.queue.push(
            self.root,
            TraceEvent::Release {
                root: self.root,
                t: self.handle.now_nanos(),
            },
        );
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
pub fn start_drainer(queue: Arc<ShardedTraceQueue>, mailer: Arc<Mailer>) -> TraceDrainerHandle {
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
fn drainer_loop(queue: Arc<ShardedTraceQueue>, mailer: Arc<Mailer>, shutdown_rx: Receiver<()>) {
    let recipient = aether_data::mailbox_id_from_name(TRACE_OBSERVER_MAILBOX_NAME);
    let kind = <BatchedTraceEvents as aether_data::Kind>::ID;

    loop {
        ship_all(&queue, &mailer, recipient, kind);

        // Park BATCH_INTERVAL or until shutdown.
        match shutdown_rx.recv_timeout(BATCH_INTERVAL) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => {
                // One last drain on shutdown so events queued just
                // before signal don't get lost.
                ship_all(&queue, &mailer, recipient, kind);
                break;
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
}

/// Drain every shard once and ship the events as one or more
/// [`BatchedTraceEvents`] mails (one per [`BATCH_MAX`] events). Each
/// shard is drained FIFO, so a root's events stay in their
/// happens-before order within the shipped stream — the ordering ADR-0080
/// settlement depends on. Events pushed to a shard after it is scanned
/// wait for the next cycle (still FIFO for their root).
///
/// Shipping is skipped silently when the [`TRACE_OBSERVER_MAILBOX_NAME`]
/// sink is not registered (unit tests that boot a chassis without
/// `TraceObserverCapability`), but the shards are still drained so the
/// queue can't grow without bound. Production chassis register the
/// observer so the skip branch never fires.
fn ship_all(
    queue: &Arc<ShardedTraceQueue>,
    mailer: &Arc<Mailer>,
    recipient: MailboxId,
    kind: KindId,
) {
    let registered = mailer
        .registry()
        .entry(recipient)
        .is_some_and(|e| matches!(e, MailboxEntry::Inbox(_)));

    let mut batch: Vec<TraceEvent> = Vec::with_capacity(BATCH_MAX);
    for shard in &queue.shards {
        while let Some(event) = shard.pop() {
            batch.push(event);
            if batch.len() >= BATCH_MAX {
                let full = mem::take(&mut batch);
                if registered {
                    ship_one(mailer, recipient, kind, full);
                }
            }
        }
    }
    if registered && !batch.is_empty() {
        ship_one(mailer, recipient, kind, batch);
    }
}

/// Encode one batch and push it to the observer mailbox.
///
/// Drainer-originated mail is chassis-root; no inheritance,
/// no `parent_mail`. We push directly through the `Mailer`
/// (no `Sender::send_detached` wrapper) — the recursion
/// break (ADR-0080 §7) is structural here: the producer
/// hook in `NativeBinding::send_mail_with_lineage` is what
/// would push a `TraceEvent::Sent` for an outbound, and we
/// bypass that path by going straight to `Mailer::push`.
fn ship_one(mailer: &Arc<Mailer>, recipient: MailboxId, kind: KindId, events: Vec<TraceEvent>) {
    let envelope = BatchedTraceEvents { events };
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
    mailer.push(Mail::new(recipient, kind, payload, 1));
}
