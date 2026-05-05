// Actor-per-component dispatch (ADR-0038 Phases 1–2).
//
// Phase 1 gave each component a dedicated dispatcher thread that
// loops on an mpsc inbox. Phase 2 retires the router thread that
// Phase 1 left in place: `Mailer::push` now resolves the
// recipient inline on the caller's thread and forwards directly into
// the per-component inbox (see `queue.rs`). The per-mailbox dispatcher
// is unchanged.
//
// `wait_idle` semantics are preserved: `Mailer`'s `outstanding`
// counter increments on `push` (before routing) and decrements inside
// the dispatcher thread after `deliver` returns, or inline for sinks /
// warn-drops. A `wait_idle` that returns still means every pushed
// mail has reached its terminal state (delivered, dropped, or
// discarded).
//
// Shutdown: per-component dispatcher threads exit when their
// `ComponentEntry` Arc drops (the `Sender` drops with it, the inbox
// closes, `recv()` returns `None`). The owning layer (chassis / test)
// disposes of the `ComponentTable`; the scheduler itself no longer
// owns a router thread to join.
//
// Phase 3 will retire the global `outstanding` barrier in favour of
// per-mailbox drains.

use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU8, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use aether_data::Kind;
use aether_kinds::ComponentDied;

use crate::component::{Component, DISPATCH_UNKNOWN_KIND};
use crate::mail::{Mail, MailboxId, ReplyTo};
use crate::mailer::Mailer;
use crate::registry::Registry;

/// MailboxState tag stored in `ComponentEntry::state` (issue 321
/// Phase 2). The dispatcher transitions Live → Dead before exiting on
/// a panic / trap; `send` and the mailer's routing path read this on
/// the fast path so mail to a dead mailbox warn-drops with the
/// distinct "actor died" reason instead of queuing on a Sender
/// nobody is reading.
const STATE_LIVE: u8 = 0;
const STATE_DEAD: u8 = 1;

/// Per-entry quiescence counter + condvar, shared with the dispatcher
/// thread. `send` increments `pending` before forwarding to the inbox;
/// the dispatcher decrements after each `deliver` and signals when the
/// counter reaches zero. `drain_with_budget` waits on the condvar for
/// that signal, giving callers a per-mailbox barrier with a deadline.
///
/// `death` is populated by `kill_actor` before it calls
/// `decrement_and_notify`, so a `drain_with_budget` waking on the same
/// notify reads a fully-formed `DrainDeath` and can report
/// `DrainOutcome::Died` instead of `Quiesced` (ADR-0063).
#[derive(Default)]
struct PendingGate {
    pending: AtomicU32,
    lock: Mutex<()>,
    cv: Condvar,
    death: Mutex<Option<DrainDeath>>,
}

/// Structured information about a dispatcher death — recorded by
/// `kill_actor` and surfaced through `drain_with_budget` /
/// `drain_all_with_budget` so the chassis can fail-fast (ADR-0063)
/// without scraping log text.
#[derive(Debug, Clone)]
pub struct DrainDeath {
    pub mailbox: MailboxId,
    pub mailbox_name: String,
    pub last_kind: String,
    pub reason: String,
}

/// Result of a single per-entry `drain_with_budget` call.
#[derive(Debug)]
pub enum DrainOutcome {
    /// Pending counter reached zero with the entry still live.
    Quiesced,
    /// The dispatcher transitioned to Dead during the wait. The
    /// `DrainDeath` carries the mailbox identity and trap / panic
    /// reason recorded by `kill_actor`.
    Died(DrainDeath),
    /// The budget expired with `pending > 0`. The dispatcher is
    /// either mid-trap or wedged in host code; the chassis treats
    /// this as a fatal substrate event.
    Wedged { waited: Duration },
}

/// Aggregate outcome of `drain_all_with_budget` across the whole
/// component table. The chassis matches on this each frame and routes
/// abnormal cases through `lifecycle::fatal_abort` (ADR-0063).
#[derive(Debug, Default)]
pub struct DrainSummary {
    pub deaths: Vec<DrainDeath>,
    /// First wedged entry encountered. Walking stops on the first
    /// wedge — the substrate is going down regardless, so collecting
    /// further state isn't useful.
    pub wedged: Option<(MailboxId, Duration)>,
}

/// Per-mailbox scheduler state. The `Component` (and its wasmtime
/// `Store`) lives on the dispatcher thread's stack; the host side only
/// sees the `Sender` (for forwarding mail) and the `JoinHandle` (for
/// recovering the `Component` on teardown).
///
/// Both are behind `Mutex<Option<_>>` so `handle_replace` can swap
/// them in-place without moving the `ComponentEntry` out of the
/// scheduler table: the entry stays alive through a replace, keeping
/// the `MailboxId` continuously addressable, and mail sent during the
/// swap buffers in the new inbox until the new dispatcher starts
/// consuming.
pub struct ComponentEntry {
    sender: Mutex<Option<Sender<Mail>>>,
    handle: Mutex<Option<JoinHandle<Component>>>,
    /// Shared with the dispatcher thread. Carried as an `Arc` so the
    /// dispatcher can decrement-and-notify without holding a reference
    /// to the whole entry (which would create a cycle through the
    /// `JoinHandle`).
    gate: Arc<PendingGate>,
    /// Stable identity for this entry (issue #321). Carried so
    /// `splice_inbox` can rewire a fresh dispatcher under the same
    /// thread name + dispatch span without the caller threading the id
    /// back through, and so panic-hook events have a structured field
    /// for the failing mailbox.
    mailbox: MailboxId,
    /// Issue 321 Phase 2: actor liveness flag. Shared with the
    /// dispatcher loop so a panic / trap detected during `deliver`
    /// transitions the entry to `STATE_DEAD` before the actor thread
    /// exits — `send` / mailer routing then warn-drop subsequent mail
    /// with the dead-actor reason instead of queuing on a Sender no
    /// one is reading. Reset to `STATE_LIVE` on a successful replace
    /// (`spawn_dispatcher_on`'s caller takes care of that path).
    state: Arc<AtomicU8>,
}

impl ComponentEntry {
    /// Spawn a dispatcher thread for `component`, wire it to a fresh
    /// mpsc inbox, and return the entry. The mpsc `Receiver` is
    /// installed onto the component's `SubstrateCtx` (ADR-0042) so
    /// that both the dispatcher and the `wait_reply_p32` host fn —
    /// which runs on the same dispatcher thread, nested under
    /// `deliver` — can drain the same inbox. The `Arc<Registry>` is
    /// used by the dispatcher to format warn-logs for unknown kinds
    /// and to resolve `mailbox` to a human-readable name when naming
    /// the dispatcher thread (issue #321) — a panic on the dispatcher
    /// then surfaces in `engine_logs` with `thread="aether-component-
    /// {name}-{mailbox_short}"` instead of an opaque thread label.
    pub fn spawn(
        mut component: Component,
        registry: Arc<Registry>,
        mailer: Arc<Mailer>,
        mailbox: MailboxId,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        component.install_inbox_rx(rx);
        let gate: Arc<PendingGate> = Arc::new(PendingGate::default());
        let state: Arc<AtomicU8> = Arc::new(AtomicU8::new(STATE_LIVE));
        let gate_for_thread = Arc::clone(&gate);
        let state_for_thread = Arc::clone(&state);
        let thread_name = dispatcher_thread_name(&registry, mailbox);
        let handle = thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                dispatcher_loop(
                    component,
                    registry,
                    mailer,
                    gate_for_thread,
                    state_for_thread,
                    mailbox,
                )
            })
            .expect("spawn component dispatcher");
        Self {
            sender: Mutex::new(Some(tx)),
            handle: Mutex::new(Some(handle)),
            gate,
            mailbox,
            state,
        }
    }

    /// Mailbox id this entry was registered under. Stable across a
    /// `splice_inbox` (replace) — the dispatcher swaps but the entry
    /// stays put.
    pub fn mailbox(&self) -> MailboxId {
        self.mailbox
    }

    /// Issue 321 Phase 2: `true` if the dispatcher transitioned this
    /// mailbox to Dead after a panic or trap during deliver. Mail
    /// routed to a dead mailbox is warn-dropped at the mailer with a
    /// distinct reason instead of being queued on a Sender no one
    /// reads.
    pub fn is_dead(&self) -> bool {
        self.state.load(Ordering::Acquire) == STATE_DEAD
    }

    /// Test-only: bump `pending` without queueing a matching mail.
    /// Simulates a stuck dispatcher (mail in flight, dispatcher
    /// never wakes / decrements) so `drain_all_with_budget` exposes
    /// the wedge path. The `frame_loop` tests reach for this
    /// directly because it's in the same module.
    #[cfg(test)]
    pub fn bump_pending_for_test(&self) {
        self.gate.pending.fetch_add(1, Ordering::AcqRel);
    }

    /// Test-only complement to `bump_pending_for_test`: clear a
    /// previously-bumped counter so test teardown doesn't trip the
    /// dispatcher's drain assertions on shutdown.
    #[cfg(test)]
    pub fn clear_pending_for_test(&self) {
        self.gate.pending.store(0, Ordering::Release);
    }

    /// Forward `mail` to this component's inbox. Returns `false` if
    /// the inbox is closed OR the mailbox transitioned to Dead
    /// (issue 321 Phase 2; callers that want to differentiate use
    /// `is_dead()` first). On success, increments the per-entry
    /// quiescence counter before sending; the dispatcher decrements
    /// after `deliver` returns. On the dead / closed paths the
    /// counter is left untouched (nothing to deliver, nothing to
    /// drain).
    pub fn send(&self, mail: Mail) -> bool {
        if self.is_dead() {
            return false;
        }
        let guard = self.sender.lock().unwrap();
        let Some(tx) = guard.as_ref() else {
            return false;
        };
        self.gate.pending.fetch_add(1, Ordering::AcqRel);
        if tx.send(mail).is_ok() {
            true
        } else {
            // Racy: inbox got closed between the Option check and the
            // send. Undo the increment so drain semantics stay clean.
            decrement_and_notify(&self.gate);
            false
        }
    }

    /// Block until every mail ever sent to this entry has been
    /// delivered (i.e. the per-entry counter reaches zero). New sends
    /// arriving during the wait re-raise the counter and extend the
    /// wait; callers that need a single-frame barrier should ensure
    /// no concurrent sends target this mailbox.
    ///
    /// Has no upper bound on wait time — chassis callers that need a
    /// fail-fast policy on stuck dispatchers use `drain_with_budget`
    /// instead (ADR-0063).
    pub fn drain(&self) {
        let mut guard = self.gate.lock.lock().unwrap();
        while self.gate.pending.load(Ordering::Acquire) > 0 {
            guard = self.gate.cv.wait(guard).unwrap();
        }
    }

    /// Budget-aware drain (ADR-0063). Same wait semantics as `drain`
    /// but with a deadline; returns a structured outcome the chassis
    /// can match on:
    ///
    /// - `Quiesced` — the entry is live and its inbox drained cleanly.
    /// - `Died(...)` — the dispatcher transitioned to Dead during the
    ///   wait; the `DrainDeath` carries the trap / panic reason
    ///   recorded by `kill_actor`.
    /// - `Wedged { waited }` — the budget elapsed with `pending > 0`.
    ///
    /// The death-vs-quiesce distinction reads from `gate.death` (a
    /// slot `kill_actor` populates before notifying the condvar), not
    /// from the `STATE_DEAD` flag. The slot is the structured signal;
    /// the state flag is for the `send`-side fast path.
    pub fn drain_with_budget(&self, budget: Duration) -> DrainOutcome {
        let deadline = Instant::now() + budget;
        let mut guard = self.gate.lock.lock().unwrap();
        while self.gate.pending.load(Ordering::Acquire) > 0 {
            let now = Instant::now();
            if now >= deadline {
                return DrainOutcome::Wedged { waited: budget };
            }
            let remaining = deadline - now;
            let (next, timeout) = self.gate.cv.wait_timeout(guard, remaining).unwrap();
            guard = next;
            if timeout.timed_out() && self.gate.pending.load(Ordering::Acquire) > 0 {
                return DrainOutcome::Wedged { waited: budget };
            }
        }
        // pending == 0. The death slot is the source of truth: if
        // `kill_actor` ran, it populated the slot before
        // `decrement_and_notify`, and that notify is what woke us.
        if let Some(d) = self.gate.death.lock().unwrap().clone() {
            DrainOutcome::Died(d)
        } else {
            DrainOutcome::Quiesced
        }
    }
}

fn decrement_and_notify(gate: &PendingGate) {
    if gate.pending.fetch_sub(1, Ordering::AcqRel) == 1 {
        let _g = gate.lock.lock().unwrap();
        gate.cv.notify_all();
    }
}

/// Close the inbox on `entry` and block until the dispatcher thread
/// returns the `Component`. The caller must hold the last external
/// strong reference to `entry`; short-lived Arc clones (e.g. the
/// router mid-forward) will drop on their own. Dropping the last
/// external strong ref through this function drops the `Sender`
/// (pulled out via `.take()` below), which closes the channel so the
/// dispatcher's `recv()` returns `None` after draining any queued mail.
///
/// Panics if the handle / sender have already been taken (a prior
/// `close_and_join` or `splice_inbox` consumed them).
pub fn close_and_join(entry: Arc<ComponentEntry>) -> Component {
    // Drop the Sender so the dispatcher sees recv() == None after it
    // drains any queued mail. ADR-0042 §5: a parked `wait_reply_p32`
    // reading the same receiver wakes with `Disconnected` on that
    // close and returns the guest's cancellation code, so the
    // dispatcher unwinds naturally.
    let _ = entry
        .sender
        .lock()
        .unwrap()
        .take()
        .expect("component sender already taken");
    let handle = entry
        .handle
        .lock()
        .unwrap()
        .take()
        .expect("component dispatcher already joined");
    drop(entry);
    handle.join().expect("component dispatcher panicked")
}

/// Splice a new inbox onto `entry`: creates a fresh `(Sender,
/// Receiver)` pair, swaps the entry's `Sender` for the new one (so
/// future mail goes to the new inbox), drops the old `Sender` (closing
/// the old channel), joins the old dispatcher (returning the old
/// `Component` and the new `Receiver`), and leaves the caller to
/// decide what dispatcher to wire onto the new inbox.
///
/// Used by `handle_replace` (ADR-0022 drain invariant): mail sent
/// between the `splice_inbox` return and the new dispatcher's spawn
/// buffers in the new `Receiver`, preserving FIFO across the swap.
pub fn splice_inbox(entry: &Arc<ComponentEntry>) -> (Component, Receiver<Mail>) {
    // ADR-0042 §5: dropping the old Sender (below) is the cancel
    // signal — a `wait_reply_p32` parked on the old inbox sees
    // `Disconnected` and returns the guest's cancellation code, so
    // the dispatcher can unwind and the join call completes.
    let (new_tx, new_rx) = mpsc::channel();
    let old_tx = entry
        .sender
        .lock()
        .unwrap()
        .replace(new_tx)
        .expect("component sender already taken");
    drop(old_tx);
    let old_handle = entry
        .handle
        .lock()
        .unwrap()
        .take()
        .expect("component dispatcher already joined");
    let old_component = old_handle.join().expect("component dispatcher panicked");
    (old_component, new_rx)
}

/// Spawn a fresh dispatcher onto `entry`'s current inbox with
/// `component`, after installing `rx` as the component's inbox
/// receiver (ADR-0042). Pairs with `splice_inbox` to complete a
/// replace (or, on replace failure, to restore the old `Component`
/// onto the post-splice inbox). The new dispatcher shares the entry's
/// existing `PendingGate` so drain counts stay consistent across the
/// splice.
pub fn spawn_dispatcher_on(
    entry: &Arc<ComponentEntry>,
    mut component: Component,
    rx: Receiver<Mail>,
    registry: Arc<Registry>,
    mailer: Arc<Mailer>,
) {
    component.install_inbox_rx(rx);
    let gate = Arc::clone(&entry.gate);
    let state = Arc::clone(&entry.state);
    // Replace resets liveness — a successful swap brings a fresh
    // instance up under the same entry, so a prior panic on the old
    // dispatcher shouldn't leave the entry permanently Dead. ADR-0022
    // freeze-drain-swap takes the failure-path policy elsewhere
    // (replace returns Err and the old instance stays bound).
    state.store(STATE_LIVE, Ordering::Release);
    // ADR-0063: clear the prior death record so a post-replace
    // `drain_with_budget` doesn't see stale `Died` from the dispatcher
    // we just replaced.
    *gate.death.lock().unwrap() = None;
    let mailbox = entry.mailbox;
    let thread_name = dispatcher_thread_name(&registry, mailbox);
    let handle = thread::Builder::new()
        .name(thread_name)
        .spawn(move || dispatcher_loop(component, registry, mailer, gate, state, mailbox))
        .expect("spawn component dispatcher");
    let prev = entry.handle.lock().unwrap().replace(handle);
    debug_assert!(prev.is_none(), "entry handle slot must be empty");
}

/// Build the dispatcher-thread name. Issue #321: panic-hook events
/// carry `thread.name` as a structured field, and naming threads after
/// their mailbox makes "what crashed?" answerable from one log line.
/// Format: `aether-component-{name}-{mailbox_short}` where
/// `mailbox_short` is the low 16 hex digits of the 64-bit id (full id
/// is too long for `top` / `ps`). Falls back to `"?"` for unnamed
/// mailboxes — sinks-only lookups would already have surfaced a
/// different error before getting here, but the fallback keeps the
/// thread-name path infallible.
fn dispatcher_thread_name(registry: &Registry, mailbox: MailboxId) -> String {
    let name = registry
        .mailbox_name(mailbox)
        .unwrap_or_else(|| "?".to_string());
    format!("aether-component-{}-{:016x}", name, mailbox.0)
}

fn dispatcher_loop(
    mut component: Component,
    registry: Arc<Registry>,
    mailer: Arc<Mailer>,
    gate: Arc<PendingGate>,
    state: Arc<AtomicU8>,
    mailbox: MailboxId,
) -> Component {
    // ADR-0042: the receiver lives on `SubstrateCtx`; `next_mail`
    // drains the overflow buffer (any non-match mail set aside by a
    // completed `wait_reply_p32` call) ahead of the mpsc. `None`
    // means both are empty and the inbox is closed — our exit.
    while let Some(mail) = component.next_mail() {
        // Issue 321: pre-resolve `kind_name` once and reuse it both as
        // a span field and (if we hit the unknown / trap / panic
        // paths) the warn / error / broadcast payloads.
        let kind_name = registry
            .kind_name(mail.kind)
            .unwrap_or_else(|| format!("kind#{:#x}", mail.kind.0));

        // Issue 321 Phase 2: wrap `deliver` in `catch_unwind` so a
        // host-side Rust panic (a panicking host fn, a poisoned
        // mutex unwrap, etc.) doesn't kill the dispatcher silently.
        // Wasmtime traps from the guest — an `unreachable` opcode,
        // OOB memory access, a panicked Rust guest under the SDK's
        // default panic handler — already surface as `Err` from
        // `deliver`, so they don't need `catch_unwind`; they're
        // handled in the inner `Ok(Err(e))` arm below. The two paths
        // are folded into one `kill_actor` disposition (issue 321
        // question C: "same disposition for traps and panics") that
        // marks the entry Dead and emits a `component_died`
        // broadcast.
        //
        // `AssertUnwindSafe` is required because the closure captures
        // `&mut component`, and `Component`'s wasmtime `Store` is not
        // `RefUnwindSafe` by default. The contract we promise: after
        // a panic the component's Store may be in an inconsistent
        // state — that's fine, because we drop the actor entirely
        // (mark Dead, exit the loop). We never touch the mid-panic
        // Store again.
        let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let span = tracing::info_span!(
                "dispatch",
                mailbox = %mailbox,
                kind = %kind_name,
            );
            let _enter = span.enter();
            component.deliver(&mail)
        }));

        match outcome {
            Ok(Ok(rc)) => {
                if rc == DISPATCH_UNKNOWN_KIND {
                    tracing::warn!(
                        target: "aether_substrate::scheduler",
                        mailbox = %mail.recipient,
                        kind = %kind_name,
                        "component has no handler for mail kind (ADR-0033 strict receiver); dropped",
                    );
                }
                decrement_and_notify(&gate);
            }
            Ok(Err(trap)) => {
                tracing::error!(
                    target: "aether_substrate::scheduler",
                    mailbox = %mail.recipient,
                    kind = %kind_name,
                    error = %trap,
                    "component deliver returned Err (wasmtime trap); marking mailbox dead",
                );
                kill_actor(
                    &state,
                    &gate,
                    &mailer,
                    &registry,
                    mailbox,
                    &kind_name,
                    format!("wasmtime trap: {trap}"),
                );
                decrement_and_notify(&gate);
                return component;
            }
            Err(payload) => {
                let payload_msg = panic_payload_string(&payload);
                tracing::error!(
                    target: "aether_substrate::scheduler",
                    mailbox = %mail.recipient,
                    kind = %kind_name,
                    payload = %payload_msg,
                    "host-side panic during deliver; marking mailbox dead",
                );
                kill_actor(
                    &state,
                    &gate,
                    &mailer,
                    &registry,
                    mailbox,
                    &kind_name,
                    format!("host panic: {payload_msg}"),
                );
                decrement_and_notify(&gate);
                return component;
            }
        }
    }
    component
}

/// Issue 321 Phase 2: transition `state` to Dead, then emit a
/// `aether.observation.component_died` broadcast through the mailer
/// so external monitor components (or a Claude session in MCP) can
/// observe the death without polling `engine_logs`. Same disposition
/// for both wasmtime traps (`Ok(Err)` from `deliver`) and host-side
/// Rust panics caught by `catch_unwind` — recovery policy is out of
/// scope for the substrate (issue 321 question D).
///
/// The broadcast emission is itself wrapped in `catch_unwind` —
/// pushing through the mailer involves a registered sink handler,
/// and we don't want a panic in that handler to escape on top of an
/// already-failing dispatcher. Worst case: the broadcast is silently
/// dropped and the death is only visible via `engine_logs`.
fn kill_actor(
    state: &AtomicU8,
    gate: &PendingGate,
    mailer: &Mailer,
    registry: &Registry,
    mailbox: MailboxId,
    last_kind: &str,
    reason: String,
) {
    let mailbox_name = registry
        .mailbox_name(mailbox)
        .unwrap_or_else(|| "?".to_string());

    // ADR-0063: record the structured death into the gate's slot
    // *before* `decrement_and_notify` (called immediately after we
    // return). A `drain_with_budget` waking on that notify reads this
    // slot and reports `DrainOutcome::Died` instead of `Quiesced`,
    // which is what lets the chassis fail-fast without scraping logs.
    {
        let mut slot = gate.death.lock().unwrap();
        *slot = Some(DrainDeath {
            mailbox,
            mailbox_name: mailbox_name.clone(),
            last_kind: last_kind.to_string(),
            reason: reason.clone(),
        });
    }

    state.store(STATE_DEAD, Ordering::Release);

    let died = ComponentDied {
        mailbox_id: mailbox,
        mailbox_name,
        last_kind: last_kind.to_string(),
        reason,
    };
    let payload = match postcard::to_allocvec(&died) {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(
                target: "aether_substrate::scheduler",
                error = %e,
                "failed to encode component_died broadcast; death visible only in logs",
            );
            return;
        }
    };

    let mail = Mail {
        recipient: crate::HubBroadcast::MAILBOX_ID,
        kind: ComponentDied::ID,
        payload,
        count: 1,
        from_component: Some(mailbox),
        reply_to: ReplyTo::NONE,
    };
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| mailer.push(mail)));
    if result.is_err() {
        tracing::error!(
            target: "aether_substrate::scheduler",
            "panic while emitting component_died broadcast; death visible only in logs",
        );
    }
}

/// Best-effort stringify a panic payload caught by `catch_unwind`.
/// Std supports `&'static str` (from `panic!("literal")`) and
/// `String` (from `panic!("{}", x)`); falls back to a `TypeId`
/// mention. Mirrors `panic_hook::payload_string` but takes a `Box`
/// since `catch_unwind`'s Err is `Box<dyn Any + Send>`.
fn panic_payload_string(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        format!("<non-string panic payload type_id={:?}>", payload.type_id())
    }
}

/// Shared, runtime-mutable table of bound components. Cloned into the
/// `Mailer` (for inline routing on push) and into the ADR-0010
/// load handler so both read and write through the same `RwLock`.
/// Values are `Arc`-shared so short-lived clones (e.g. the router's
/// forward path) can outlive a concurrent `remove` without racing on
/// `ComponentEntry`'s `Drop`.
pub type ComponentTable = Arc<RwLock<HashMap<MailboxId, Arc<ComponentEntry>>>>;

pub struct Scheduler {
    queue: Arc<Mailer>,
    registry: Arc<Registry>,
    components: ComponentTable,
}

impl Scheduler {
    /// Build a scheduler over an empty component table and wire the
    /// queue's inline router to the registry + components. The
    /// `_k_workers` parameter is retained for boot-config
    /// compatibility (ADR-0004 sized the worker pool) but is ignored
    /// under ADR-0038: dispatch parallelism is one thread per
    /// component, and Phase 2 retired the shared router thread.
    pub fn new(registry: Arc<Registry>, queue: Arc<Mailer>, _k_workers: usize) -> Self {
        let components: ComponentTable = Arc::new(RwLock::new(HashMap::new()));
        queue.wire(Arc::clone(&registry), Arc::clone(&components));
        Self {
            queue,
            registry,
            components,
        }
    }

    pub fn queue(&self) -> &Arc<Mailer> {
        &self.queue
    }

    pub fn registry(&self) -> &Arc<Registry> {
        &self.registry
    }

    /// Handle to the runtime-mutable component table. ADR-0010's load
    /// handler holds a clone and inserts newly instantiated components
    /// without needing an `Arc<Scheduler>` — which would create a
    /// cycle through any registry sink closures that referenced back.
    pub fn components(&self) -> &ComponentTable {
        &self.components
    }

    /// Spawn a dispatcher thread for `component` and register the
    /// entry under `id`. Silently replaces any existing component at
    /// `id` — replacement is an ADR-0010 primitive in its own right,
    /// handled by `ControlPlane::handle_replace`.
    pub fn add_component(&self, id: MailboxId, component: Component) {
        let entry = ComponentEntry::spawn(
            component,
            Arc::clone(&self.registry),
            Arc::clone(&self.queue),
            id,
        );
        self.components.write().unwrap().insert(id, Arc::new(entry));
    }
}

/// Barrier equivalent to the Phase-2 `Mailer::wait_idle`: block
/// until every component in `components` has an empty inbox and no
/// in-flight `deliver`. Iterates + re-checks in case a component's
/// delivery pushes fresh mail to one we already drained (e.g. a
/// component responding to input by dispatching to another
/// component via `SubstrateCtx::send`).
///
/// Sink-bound mail runs inline on the pushing thread, so there is
/// nothing to drain for sinks — only components hold a queue of
/// in-flight mail.
///
/// Safety on concurrent pushes: if another thread is pushing to a
/// mailbox in `components` during the drain, the drain will extend
/// until that mailbox quiesces, potentially forever if the pusher
/// never stops. Frame-barrier callers ensure their pushes complete
/// before calling `drain_all` (e.g. desktop's `publish_*` returns
/// before `drain_all` is invoked).
pub fn drain_all(components: &ComponentTable) {
    loop {
        let entries: Vec<Arc<ComponentEntry>> =
            components.read().unwrap().values().cloned().collect();
        for entry in &entries {
            entry.drain();
        }
        let still_busy = entries
            .iter()
            .any(|e| e.gate.pending.load(Ordering::Acquire) > 0);
        if !still_busy {
            return;
        }
    }
}

/// Budget-aware variant of `drain_all` (ADR-0063). Walks the component
/// table, collecting structured outcomes from each per-entry drain.
/// Returns a `DrainSummary` the chassis matches on — non-empty
/// `deaths` or any `wedged` triggers `lifecycle::fatal_abort`.
///
/// Stops iteration on the first wedged entry: the substrate is going
/// down regardless, so collecting further state isn't useful and may
/// itself be slow if other entries are also stuck.
///
/// Re-iterates after a clean pass if any entry's pending counter has
/// risen again — a delivered mail can dispatch fresh mail to another
/// mailbox we already drained, same way the no-budget `drain_all`
/// handles that case. The budget applies per-entry-per-pass; a
/// single chassis call can wait up to `budget * passes` in pathological
/// cross-component send loops, but in practice quiesces in one pass.
pub fn drain_all_with_budget(components: &ComponentTable, budget: Duration) -> DrainSummary {
    let mut summary = DrainSummary::default();
    loop {
        let entries: Vec<Arc<ComponentEntry>> =
            components.read().unwrap().values().cloned().collect();
        for entry in &entries {
            match entry.drain_with_budget(budget) {
                DrainOutcome::Quiesced => {}
                DrainOutcome::Died(d) => summary.deaths.push(d),
                DrainOutcome::Wedged { waited } => {
                    summary.wedged = Some((entry.mailbox, waited));
                    return summary;
                }
            }
        }
        let still_busy = entries
            .iter()
            .any(|e| e.gate.pending.load(Ordering::Acquire) > 0);
        if !still_busy {
            return summary;
        }
    }
}

// Per-component dispatcher threads exit when their `ComponentEntry`
// Arc drops (the `Sender` drops with it, the inbox closes, `recv()`
// returns `None`). The scheduler no longer owns a router thread, so
// its `Drop` impl is redundant — the owning layer (chassis / test)
// disposes of the `ComponentTable` when it wants dispatchers to exit.

#[cfg(test)]
mod tests {
    use wasmtime::{Engine, Linker, Module};

    use super::*;
    use crate::ctx::SubstrateCtx;
    use crate::input;
    use crate::outbound::HubOutbound;
    use aether_actor::Actor;

    /// Minimal guest: just exports `memory` and a no-op `receive_p32`.
    /// Enough to satisfy `Component::instantiate`; these tests only
    /// exercise the `ComponentEntry` send path, never `deliver`.
    const WAT_NOOP: &str = r#"
        (module
            (memory (export "memory") 1)
            (func (export "receive_p32") (param i64 i32 i32 i32 i32) (result i32)
                i32.const 0))
    "#;

    /// Issue #321: guest unreachables on every `receive_p32` call.
    /// Wasmtime translates the WASM `unreachable` opcode into a trap
    /// that surfaces as `Err(wasmtime::Error)` from `Component::deliver`.
    /// Pre-issue-321 the dispatcher's `.expect("component.deliver
    /// failed")` turned that Err into a panic that killed the actor
    /// thread silently — `drain` then deadlocked because the pending
    /// counter never decremented. With the fix, `deliver` errs cleanly,
    /// the dispatcher logs a `tracing::error!` and continues to the
    /// next mail.
    const WAT_TRAPS_IN_RECEIVE: &str = r#"
        (module
            (memory (export "memory") 1)
            (func (export "receive_p32") (param i64 i32 i32 i32 i32) (result i32)
                unreachable))
    "#;

    fn component_from_wat(wat: &str) -> Component {
        let engine = Engine::default();
        let mut linker: Linker<SubstrateCtx> = Linker::new(&engine);
        crate::host_fns::register(&mut linker).expect("register host fns");
        let wasm = wat::parse_str(wat).expect("compile WAT");
        let module = Module::new(&engine, &wasm).expect("compile module");
        let ctx = SubstrateCtx::new(
            MailboxId(0),
            Arc::new(Registry::new()),
            Arc::new(Mailer::new()),
            HubOutbound::disconnected(),
            input::new_subscribers(),
        );
        Component::instantiate(&engine, &linker, &module, ctx).expect("instantiate")
    }

    fn minimal_component() -> Component {
        component_from_wat(WAT_NOOP)
    }

    fn spawn_entry() -> Arc<ComponentEntry> {
        Arc::new(ComponentEntry::spawn(
            minimal_component(),
            Arc::new(Registry::new()),
            Arc::new(Mailer::new()),
            MailboxId(0),
        ))
    }

    /// Basic inbox: non-overflow mail dispatches through the
    /// no-op WAT component and the pending counter balances.
    #[test]
    fn send_delivers_through_dispatcher_and_drains_pending() {
        let entry = spawn_entry();
        assert!(entry.send(Mail::new(
            MailboxId(0),
            aether_data::KindId(0xBB),
            vec![1],
            1
        )));
        entry.drain();
        assert_eq!(entry.gate.pending.load(Ordering::Acquire), 0);
    }

    /// ADR-0042: overflow-buffered mail (simulated by pushing
    /// directly onto the ctx's overflow via the dispatcher
    /// component's accessor) is drained by the dispatcher ahead of
    /// any mpsc mail. Tests `Component::next_mail`'s FIFO overlay.
    #[test]
    fn overflow_buffer_drains_before_mpsc() {
        // Build a component directly so we can seed its overflow
        // before handing it to a dispatcher.
        let mut component = minimal_component();
        component.push_overflow_for_test(Mail::new(
            MailboxId(0),
            aether_data::KindId(0xCC),
            vec![9],
            1,
        ));
        // next_mail should pop from overflow without touching mpsc.
        let mail = component.next_mail().expect("overflow pops first");
        assert_eq!(mail.kind, aether_data::KindId(0xCC));
    }

    /// ADR-0042 §5: `close_and_join` drops the mpsc Sender; a
    /// dispatcher parked on the inbox unwinds because `next_mail`
    /// sees the disconnected receiver and returns `None`.
    #[test]
    fn close_and_join_returns_component_cleanly() {
        let entry = spawn_entry();
        let _component = close_and_join(entry);
    }

    /// ADR-0042 §5 applied to replace: `splice_inbox` drops the old
    /// Sender so the old dispatcher's inbox disconnects and the
    /// thread joins cleanly. Retained as a smoke test that the
    /// refactor didn't break the post-splice flow.
    #[test]
    fn splice_inbox_joins_old_dispatcher() {
        let entry = spawn_entry();
        let (_old_component, _new_rx) = splice_inbox(&entry);
    }

    /// Issue #321: thread name carries the resolved mailbox name plus a
    /// 16-hex-char suffix derived from the 64-bit mailbox id. The panic
    /// hook's `thread.name` field is what makes a dispatcher panic
    /// answerable from one log line, so the format is load-bearing.
    #[test]
    fn dispatcher_thread_name_includes_mailbox_name_and_id() {
        let registry = Registry::new();
        let mbox = registry.register_component("test-comp");
        let name = dispatcher_thread_name(&registry, mbox);
        assert!(
            name.starts_with("aether-component-test-comp-"),
            "unexpected prefix: {name}",
        );
        // 16-hex-char id suffix.
        let suffix = &name[name.len() - 16..];
        assert_eq!(suffix.len(), 16);
        assert!(
            suffix.chars().all(|c| c.is_ascii_hexdigit()),
            "id suffix not hex: {suffix}",
        );
    }

    /// Falls back to `?` when the mailbox isn't registered. Real
    /// substrate paths register the mailbox before spawning, but the
    /// fallback keeps the thread-name path infallible — a missing name
    /// must never crash a dispatcher spawn.
    #[test]
    fn dispatcher_thread_name_falls_back_when_unnamed() {
        let registry = Registry::new();
        let name = dispatcher_thread_name(&registry, MailboxId(0xABCDEF1234567890));
        assert_eq!(name, "aether-component-?-abcdef1234567890");
    }

    /// Issue 321 Phase 2 (replaces Phase 1's
    /// `trap_in_receive_does_not_kill_dispatcher`): a guest trap during
    /// `deliver` now marks the mailbox Dead and exits the dispatcher.
    /// Pre-Phase-1 this test would deadlock in `drain` because the
    /// panicked dispatcher never decremented the pending counter.
    /// Post-Phase-1 the dispatcher logged the Err and continued. Phase 2
    /// changes the disposition again: same as a host panic, the actor
    /// is killed (issue 321 question C: "same disposition for traps and
    /// panics"). Recovery (replace_component) is policy that lives
    /// outside the substrate.
    #[test]
    fn trap_in_receive_marks_mailbox_dead() {
        let registry = Arc::new(Registry::new());
        let mailer = Arc::new(Mailer::new());
        // Install the broadcast sink so the dispatcher's death push
        // doesn't bubble up as "unknown mailbox" — a side effect of
        // having no broadcast in test setup. Counter is plumbed
        // through to verify the broadcast actually fired.
        let broadcast_count = Arc::new(AtomicU32::new(0));
        let bc = Arc::clone(&broadcast_count);
        registry.register_sink(
            crate::HubBroadcast::NAMESPACE,
            Arc::new(move |_, _, _, _, _, _| {
                bc.fetch_add(1, Ordering::SeqCst);
            }),
        );
        // Wire the mailer with a fresh ComponentTable so push routing
        // hits the registered sink.
        let components: ComponentTable = Arc::new(RwLock::new(HashMap::new()));
        mailer.wire(Arc::clone(&registry), Arc::clone(&components));

        let entry = Arc::new(ComponentEntry::spawn(
            component_from_wat(WAT_TRAPS_IN_RECEIVE),
            Arc::clone(&registry),
            Arc::clone(&mailer),
            MailboxId(0),
        ));

        // Send mail; guest traps on receive_p32. Phase 2 dispatcher
        // marks state Dead, emits ComponentDied broadcast, exits.
        assert!(entry.send(Mail::new(
            MailboxId(0),
            aether_data::KindId(0xDD),
            vec![1],
            1
        )));
        entry.drain();
        assert_eq!(
            entry.gate.pending.load(Ordering::Acquire),
            0,
            "dispatcher must decrement pending before exiting",
        );
        assert!(
            entry.is_dead(),
            "trap during deliver must transition mailbox to Dead",
        );
        assert_eq!(
            broadcast_count.load(Ordering::SeqCst),
            1,
            "exactly one component_died broadcast must be emitted",
        );

        // Send to dead mailbox: returns false, doesn't queue, doesn't
        // increment pending.
        assert!(
            !entry.send(Mail::new(
                MailboxId(0),
                aether_data::KindId(0xDD),
                vec![2],
                1
            )),
            "send to dead mailbox must fail-fast",
        );
        assert_eq!(entry.gate.pending.load(Ordering::Acquire), 0);

        // close_and_join still recovers cleanly — dispatcher already
        // exited via the kill_actor path, so the JoinHandle is ready.
        let _component = close_and_join(entry);
    }

    /// Issue 321 Phase 2: the ComponentDied broadcast carries the
    /// expected structured fields (mailbox_id, mailbox_name, last_kind,
    /// reason) so external monitors can act on the failure without
    /// polling logs.
    #[test]
    fn component_died_broadcast_carries_expected_fields() {
        let registry = Arc::new(Registry::new());
        // Register the dispatcher's mailbox under a known name so the
        // broadcast's `mailbox_name` field has something to resolve.
        let mailbox = registry.register_component("crashy");
        let mailer = Arc::new(Mailer::new());

        let captured: Arc<Mutex<Vec<ComponentDied>>> = Arc::new(Mutex::new(Vec::new()));
        let cap = Arc::clone(&captured);
        registry.register_sink(
            crate::HubBroadcast::NAMESPACE,
            Arc::new(move |kind, _, _, _, bytes, _| {
                if kind == ComponentDied::ID
                    && let Ok(d) = postcard::from_bytes::<ComponentDied>(bytes)
                {
                    cap.lock().unwrap().push(d);
                }
            }),
        );
        let components: ComponentTable = Arc::new(RwLock::new(HashMap::new()));
        mailer.wire(Arc::clone(&registry), Arc::clone(&components));

        let entry = Arc::new(ComponentEntry::spawn(
            component_from_wat(WAT_TRAPS_IN_RECEIVE),
            Arc::clone(&registry),
            Arc::clone(&mailer),
            mailbox,
        ));

        assert!(entry.send(Mail::new(mailbox, aether_data::KindId(0xDD), vec![], 1)));
        entry.drain();

        let died = captured.lock().unwrap();
        assert_eq!(died.len(), 1, "exactly one death broadcast expected");
        let d = &died[0];
        assert_eq!(d.mailbox_id, mailbox);
        assert_eq!(d.mailbox_name, "crashy");
        assert!(
            d.reason.starts_with("wasmtime trap:"),
            "expected wasmtime trap reason, got: {}",
            d.reason,
        );
        // last_kind is the registry-resolved or hex-fallback name; this
        // mailbox's send used kind 0xDD which has no registered name.
        assert!(
            d.last_kind.contains("0xdd") || d.last_kind == "kind#0xdd",
            "expected hex fallback for unregistered kind, got: {}",
            d.last_kind,
        );

        let _component = close_and_join(entry);
    }

    /// Issue 321 Phase 2: `is_dead` defaults to false on a freshly
    /// spawned entry, transitions to true only after a kill_actor
    /// path. Tests the AtomicU8 visibility guarantee — Acquire load
    /// must see the dispatcher's Release store.
    #[test]
    fn is_dead_starts_false_and_transitions_after_trap() {
        let registry = Arc::new(Registry::new());
        let mailer = Arc::new(Mailer::new());
        registry.register_sink(
            crate::HubBroadcast::NAMESPACE,
            Arc::new(|_, _, _, _, _, _| {}),
        );
        let components: ComponentTable = Arc::new(RwLock::new(HashMap::new()));
        mailer.wire(Arc::clone(&registry), Arc::clone(&components));

        let entry = Arc::new(ComponentEntry::spawn(
            component_from_wat(WAT_TRAPS_IN_RECEIVE),
            Arc::clone(&registry),
            Arc::clone(&mailer),
            MailboxId(0),
        ));
        assert!(!entry.is_dead(), "freshly spawned entry must be Live");

        assert!(entry.send(Mail::new(
            MailboxId(0),
            aether_data::KindId(0xDD),
            vec![],
            1
        )));
        entry.drain();
        assert!(entry.is_dead(), "post-trap entry must be Dead");

        let _component = close_and_join(entry);
    }

    /// Issue 321 Phase 2: a healthy component (no trap) stays Live
    /// across many mails. Negative regression — kill_actor must not
    /// fire on Ok deliveries.
    #[test]
    fn healthy_component_stays_live_across_many_mails() {
        let entry = Arc::new(ComponentEntry::spawn(
            minimal_component(),
            Arc::new(Registry::new()),
            Arc::new(Mailer::new()),
            MailboxId(0),
        ));
        for i in 0..16u32 {
            assert!(entry.send(Mail::new(
                MailboxId(0),
                aether_data::KindId(0xCC),
                vec![i as u8],
                1
            )));
        }
        entry.drain();
        assert!(!entry.is_dead());
        assert_eq!(entry.gate.pending.load(Ordering::Acquire), 0);
        let _component = close_and_join(entry);
    }

    /// Issue 321 Phase 2: `panic_payload_string` mirrors the
    /// `panic_hook::payload_string` shapes for `&'static str`,
    /// `String`, and unknown payload types. Pure-fn coverage so a
    /// future regression in formatting shows up at the unit level
    /// rather than only via the integration trap path.
    #[test]
    fn panic_payload_string_handles_common_shapes() {
        let s_static: Box<dyn std::any::Any + Send> = Box::new("literal");
        assert_eq!(panic_payload_string(&s_static), "literal");

        let s_owned: Box<dyn std::any::Any + Send> = Box::new(String::from("formatted"));
        assert_eq!(panic_payload_string(&s_owned), "formatted");

        let other: Box<dyn std::any::Any + Send> = Box::new(42i32);
        let out = panic_payload_string(&other);
        assert!(
            out.starts_with("<non-string panic payload"),
            "unexpected: {out}",
        );
    }

    /// ADR-0063: a clean delivery cycle ends with the entry still
    /// live, so `drain_with_budget` returns `Quiesced`.
    #[test]
    fn drain_with_budget_returns_quiesced_on_clean_delivery() {
        let entry = spawn_entry();
        assert!(entry.send(Mail::new(
            MailboxId(0),
            aether_data::KindId(0xBB),
            vec![1],
            1
        )));
        match entry.drain_with_budget(Duration::from_secs(1)) {
            DrainOutcome::Quiesced => {}
            other => panic!("expected Quiesced, got {other:?}"),
        }
        let _component = close_and_join(entry);
    }

    /// ADR-0063: a guest trap during deliver records a `DrainDeath`
    /// in the gate, and `drain_with_budget` reads it on wake to
    /// return `Died`. Mirrors the existing `trap_in_receive_marks_
    /// mailbox_dead` test but exercises the new budget-aware path.
    #[test]
    fn drain_with_budget_returns_died_after_trap() {
        let registry = Arc::new(Registry::new());
        let mailbox = registry.register_component("trappy");
        let mailer = Arc::new(Mailer::new());

        // Broadcast sink: the dispatcher's death push goes through
        // it; without a registered sink the broadcast warn-drops as
        // an unknown mailbox, harmless but noisy.
        registry.register_sink(
            crate::HubBroadcast::NAMESPACE,
            Arc::new(|_, _, _, _, _, _| {}),
        );
        let components: ComponentTable = Arc::new(RwLock::new(HashMap::new()));
        mailer.wire(Arc::clone(&registry), Arc::clone(&components));

        let entry = Arc::new(ComponentEntry::spawn(
            component_from_wat(WAT_TRAPS_IN_RECEIVE),
            Arc::clone(&registry),
            Arc::clone(&mailer),
            mailbox,
        ));

        assert!(entry.send(Mail::new(mailbox, aether_data::KindId(0xDD), vec![], 1)));
        let outcome = entry.drain_with_budget(Duration::from_secs(2));
        match outcome {
            DrainOutcome::Died(d) => {
                assert_eq!(d.mailbox, mailbox);
                assert_eq!(d.mailbox_name, "trappy");
                assert!(
                    d.reason.starts_with("wasmtime trap:"),
                    "expected wasmtime trap reason, got: {}",
                    d.reason,
                );
            }
            other => panic!("expected Died, got {other:?}"),
        }

        let _component = close_and_join(entry);
    }

    /// ADR-0063: a dispatcher that doesn't decrement pending within
    /// the budget triggers `Wedged`. Simulated by bumping the
    /// pending counter directly so the dispatcher never sees the
    /// "mail" — the wait_timeout path is what we're testing, and
    /// driving it via an actual stuck guest would leak a wasmtime
    /// thread for the rest of the test run.
    #[test]
    fn drain_with_budget_returns_wedged_when_pending_does_not_drop() {
        let entry = spawn_entry();
        // Bump pending without going through `send` — no mpsc message
        // is queued, so the dispatcher never wakes to decrement.
        entry.gate.pending.fetch_add(1, Ordering::AcqRel);

        let outcome = entry.drain_with_budget(Duration::from_millis(100));
        match outcome {
            DrainOutcome::Wedged { waited } => {
                assert_eq!(waited, Duration::from_millis(100));
            }
            other => panic!("expected Wedged, got {other:?}"),
        }

        // Restore pending so close_and_join's drop doesn't leak the
        // gate state for any other tests.
        entry.gate.pending.fetch_sub(1, Ordering::AcqRel);
        let _component = close_and_join(entry);
    }

    /// ADR-0063: `drain_all_with_budget` aggregates per-entry
    /// outcomes. Quiesced entries leave `summary.deaths` empty;
    /// died entries land there.
    #[test]
    fn drain_all_with_budget_collects_deaths() {
        let registry = Arc::new(Registry::new());
        let mailbox_ok = registry.register_component("alive");
        let mailbox_dies = registry.register_component("dies");
        let mailer = Arc::new(Mailer::new());
        registry.register_sink(
            crate::HubBroadcast::NAMESPACE,
            Arc::new(|_, _, _, _, _, _| {}),
        );
        let components: ComponentTable = Arc::new(RwLock::new(HashMap::new()));
        mailer.wire(Arc::clone(&registry), Arc::clone(&components));

        let entry_ok = Arc::new(ComponentEntry::spawn(
            minimal_component(),
            Arc::clone(&registry),
            Arc::clone(&mailer),
            mailbox_ok,
        ));
        let entry_dies = Arc::new(ComponentEntry::spawn(
            component_from_wat(WAT_TRAPS_IN_RECEIVE),
            Arc::clone(&registry),
            Arc::clone(&mailer),
            mailbox_dies,
        ));
        components
            .write()
            .unwrap()
            .insert(mailbox_ok, Arc::clone(&entry_ok));
        components
            .write()
            .unwrap()
            .insert(mailbox_dies, Arc::clone(&entry_dies));

        // Send to both: one quiesces cleanly, the other traps.
        assert!(entry_ok.send(Mail::new(mailbox_ok, aether_data::KindId(0xBB), vec![], 1)));
        assert!(entry_dies.send(Mail::new(
            mailbox_dies,
            aether_data::KindId(0xDD),
            vec![],
            1
        )));

        let summary = drain_all_with_budget(&components, Duration::from_secs(2));
        assert!(summary.wedged.is_none(), "no entry should be wedged");
        assert_eq!(summary.deaths.len(), 1, "exactly one entry died");
        assert_eq!(summary.deaths[0].mailbox, mailbox_dies);
        assert_eq!(summary.deaths[0].mailbox_name, "dies");
    }

    /// ADR-0063: a successful replace clears the prior death record
    /// so a post-replace drain sees `Quiesced` rather than the
    /// stale `Died` from the dispatcher we just retired.
    #[test]
    fn replace_clears_prior_death() {
        let registry = Arc::new(Registry::new());
        let mailbox = registry.register_component("rep");
        let mailer = Arc::new(Mailer::new());
        registry.register_sink(
            crate::HubBroadcast::NAMESPACE,
            Arc::new(|_, _, _, _, _, _| {}),
        );
        let components: ComponentTable = Arc::new(RwLock::new(HashMap::new()));
        mailer.wire(Arc::clone(&registry), Arc::clone(&components));

        let entry = Arc::new(ComponentEntry::spawn(
            component_from_wat(WAT_TRAPS_IN_RECEIVE),
            Arc::clone(&registry),
            Arc::clone(&mailer),
            mailbox,
        ));

        // Trigger a death.
        assert!(entry.send(Mail::new(mailbox, aether_data::KindId(0xDD), vec![], 1)));
        match entry.drain_with_budget(Duration::from_secs(2)) {
            DrainOutcome::Died(_) => {}
            other => panic!("expected Died after trap, got {other:?}"),
        }

        // Splice + spawn fresh dispatcher onto the same entry.
        let (_old_component, new_rx) = splice_inbox(&entry);
        spawn_dispatcher_on(
            &entry,
            minimal_component(),
            new_rx,
            Arc::clone(&registry),
            Arc::clone(&mailer),
        );

        // Death slot should be cleared, state back to LIVE, drain
        // returns Quiesced.
        match entry.drain_with_budget(Duration::from_secs(1)) {
            DrainOutcome::Quiesced => {}
            other => panic!("expected Quiesced after replace, got {other:?}"),
        }

        let _component = close_and_join(entry);
    }
}
