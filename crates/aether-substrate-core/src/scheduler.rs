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
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::thread::{self, JoinHandle};

use crate::component::{Component, DISPATCH_UNKNOWN_KIND};
use crate::mail::{Mail, MailboxId};
use crate::mailer::Mailer;
use crate::registry::Registry;
use crate::wait::FilterSlot;

/// Per-entry quiescence counter + condvar, shared with the dispatcher
/// thread. `send` increments `pending` before forwarding to the inbox;
/// the dispatcher decrements after each `deliver` and signals when the
/// counter reaches zero. `drain` waits on the condvar for that signal,
/// giving callers a per-mailbox barrier equivalent to the Phase-2
/// global `Mailer::wait_idle`.
#[derive(Default)]
struct PendingGate {
    pending: AtomicU32,
    lock: Mutex<()>,
    cv: Condvar,
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
    /// ADR-0042 sync-wait filter slot, shared by-Arc with the
    /// component's `SubstrateCtx`. `send` consults it on the
    /// pushing thread so matching mail lands directly on the
    /// waiter's oneshot instead of the mpsc — the dispatcher
    /// thread is already parked inside `deliver` while a wait is
    /// in flight, so a check at the dispatcher's `rx.recv()` could
    /// never run.
    filter_slot: Arc<FilterSlot>,
}

impl ComponentEntry {
    /// Spawn a dispatcher thread for `component`, wire it to a fresh
    /// mpsc inbox, and return the entry. The `Arc<Registry>` is used
    /// by the dispatcher to format warn-logs for unknown kinds.
    pub fn spawn(component: Component, registry: Arc<Registry>) -> Self {
        let (tx, rx) = mpsc::channel();
        let gate: Arc<PendingGate> = Arc::new(PendingGate::default());
        let gate_for_thread = Arc::clone(&gate);
        let filter_slot = component.filter_slot();
        let handle = thread::Builder::new()
            .name("aether-component-dispatch".into())
            .spawn(move || dispatcher_loop(component, rx, registry, gate_for_thread))
            .expect("spawn component dispatcher");
        Self {
            sender: Mutex::new(Some(tx)),
            handle: Mutex::new(Some(handle)),
            gate,
            filter_slot,
        }
    }

    /// Forward `mail` to this component's inbox. Returns `false` if
    /// the inbox is closed (caller should warn-and-drop). On success,
    /// increments the per-entry quiescence counter before sending; the
    /// dispatcher decrements after `deliver` returns. On closed-inbox,
    /// the counter is left untouched (nothing to deliver, nothing to
    /// drain).
    pub fn send(&self, mail: Mail) -> bool {
        // ADR-0042: if a `wait_reply_p32` host fn is parked on the
        // dispatcher thread and has registered a matching kind, hand
        // the mail straight to its oneshot and skip the inbox. No
        // `deliver` call is owed, so the pending counter stays
        // untouched on this branch.
        let mail = match self.filter_slot.try_match(mail) {
            Ok(()) => return true,
            Err(mail) => mail,
        };
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

    /// Expose the filter slot so the `wait_reply_p32` host fn (landing
    /// in a follow-up PR) can install filters without reaching back
    /// through the scheduler. Cheap `Arc::clone` — callers keep the
    /// handle for the duration of a single wait.
    pub fn filter_slot(&self) -> Arc<FilterSlot> {
        Arc::clone(&self.filter_slot)
    }

    /// Block until every mail ever sent to this entry has been
    /// delivered (i.e. the per-entry counter reaches zero). New sends
    /// arriving during the wait re-raise the counter and extend the
    /// wait; callers that need a single-frame barrier should ensure
    /// no concurrent sends target this mailbox.
    pub fn drain(&self) {
        let mut guard = self.gate.lock.lock().unwrap();
        while self.gate.pending.load(Ordering::Acquire) > 0 {
            guard = self.gate.cv.wait(guard).unwrap();
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
    // ADR-0042 §5: any in-flight sync wait must unpark before we try
    // to join. Clearing the filter drops the oneshot sender; the
    // dispatcher's parked `recv_timeout` wakes with `Disconnected` and
    // the host fn returns the cancellation code, letting `deliver`
    // return so the dispatcher can observe the closed inbox.
    entry.filter_slot.clear();
    // Drop the Sender so the dispatcher sees recv() == None after it
    // drains any queued mail.
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
    // ADR-0042 §5: cancel any in-flight wait on the outgoing
    // instance. Same invariant as `close_and_join`: the old
    // dispatcher can't hit its closed-inbox exit condition while
    // parked inside `wait_reply_p32`, so the oneshot must disconnect
    // first. The successor instance starts with an empty slot.
    entry.filter_slot.clear();
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

/// Spawn a fresh dispatcher onto `entry`'s current inbox (`rx`) with
/// `component`, and record the new `JoinHandle`. Pairs with
/// `splice_inbox` to complete a replace (or, on replace failure, to
/// restore the old `Component` onto the post-splice inbox). The new
/// dispatcher shares the entry's existing `PendingGate` so drain
/// counts stay consistent across the splice.
pub fn spawn_dispatcher_on(
    entry: &Arc<ComponentEntry>,
    component: Component,
    rx: Receiver<Mail>,
    registry: Arc<Registry>,
) {
    let gate = Arc::clone(&entry.gate);
    let handle = thread::Builder::new()
        .name("aether-component-dispatch".into())
        .spawn(move || dispatcher_loop(component, rx, registry, gate))
        .expect("spawn component dispatcher");
    let prev = entry.handle.lock().unwrap().replace(handle);
    debug_assert!(prev.is_none(), "entry handle slot must be empty");
}

fn dispatcher_loop(
    mut component: Component,
    rx: Receiver<Mail>,
    registry: Arc<Registry>,
    gate: Arc<PendingGate>,
) -> Component {
    while let Ok(mail) = rx.recv() {
        let rc = component.deliver(&mail).expect("component.deliver failed");
        if rc == DISPATCH_UNKNOWN_KIND {
            let kind_name = registry
                .kind_name(mail.kind)
                .unwrap_or_else(|| format!("kind#{:#x}", mail.kind));
            tracing::warn!(
                target: "aether_substrate::scheduler",
                mailbox = ?mail.recipient,
                kind = %kind_name,
                "component has no handler for mail kind (ADR-0033 strict receiver); dropped",
            );
        }
        decrement_and_notify(&gate);
    }
    component
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
        let entry = ComponentEntry::spawn(component, Arc::clone(&self.registry));
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
// Per-component dispatcher threads exit when their `ComponentEntry`
// Arc drops (the `Sender` drops with it, the inbox closes, `recv()`
// returns `None`). The scheduler no longer owns a router thread, so
// its `Drop` impl is redundant — the owning layer (chassis / test)
// disposes of the `ComponentTable` when it wants dispatchers to exit.

#[cfg(test)]
mod tests {
    use std::sync::mpsc::RecvTimeoutError;
    use std::time::Duration;

    use wasmtime::{Engine, Linker, Module};

    use super::*;
    use crate::ctx::SubstrateCtx;
    use crate::hub_client::HubOutbound;
    use crate::input;

    /// Minimal guest: just exports `memory` and a no-op `receive_p32`.
    /// Enough to satisfy `Component::instantiate`; these tests only
    /// exercise the `ComponentEntry` send path, never `deliver`.
    const WAT_NOOP: &str = r#"
        (module
            (memory (export "memory") 1)
            (func (export "receive_p32") (param i64 i32 i32 i32) (result i32)
                i32.const 0))
    "#;

    fn minimal_component() -> Component {
        let engine = Engine::default();
        let mut linker: Linker<SubstrateCtx> = Linker::new(&engine);
        crate::host_fns::register(&mut linker).expect("register host fns");
        let wasm = wat::parse_str(WAT_NOOP).expect("compile WAT");
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

    fn spawn_entry() -> Arc<ComponentEntry> {
        Arc::new(ComponentEntry::spawn(
            minimal_component(),
            Arc::new(Registry::new()),
        ))
    }

    /// ADR-0042: matching mail on an installed filter bypasses the
    /// mpsc and lands directly on the waiter's oneshot. The pending
    /// counter is untouched — there's no `deliver` call owed.
    #[test]
    fn send_matching_mail_hands_off_to_filter_and_skips_pending() {
        let entry = spawn_entry();
        let slot = entry.filter_slot();
        let rx = slot.install(0xAA);

        assert!(entry.send(Mail::new(MailboxId(0), 0xAA, vec![7, 8, 9], 1)));

        let delivered = rx
            .recv_timeout(Duration::from_millis(100))
            .expect("matched mail delivered on oneshot");
        assert_eq!(delivered.kind, 0xAA);
        assert_eq!(delivered.payload, vec![7, 8, 9]);
        assert_eq!(
            entry.gate.pending.load(Ordering::Acquire),
            0,
            "filter-match path must not touch pending",
        );
        // drain returns immediately because pending is zero.
        entry.drain();
    }

    /// Non-matching mail falls through to the inbox even when a
    /// filter is installed. The pending counter ticks up the normal
    /// amount; the no-op dispatcher will drain it.
    #[test]
    fn send_non_matching_mail_falls_through_to_inbox() {
        let entry = spawn_entry();
        let slot = entry.filter_slot();
        let _rx = slot.install(0xAA);

        assert!(entry.send(Mail::new(MailboxId(0), 0xBB, vec![1], 1)));

        // Wait for the no-op dispatcher to consume the mail. drain
        // blocks on the condvar until pending hits zero.
        entry.drain();
        assert_eq!(entry.gate.pending.load(Ordering::Acquire), 0);
    }

    /// ADR-0042 §5: `close_and_join` must cancel any in-flight sync
    /// wait before joining. The parked waiter's `recv_timeout` wakes
    /// with `Disconnected`, which the host fn will surface to the
    /// guest as the `-3` cancellation code.
    #[test]
    fn close_and_join_cancels_in_flight_wait() {
        let entry = spawn_entry();
        let rx = entry.filter_slot().install(0xAA);

        let _component = close_and_join(entry);

        match rx.recv_timeout(Duration::from_millis(100)) {
            Err(RecvTimeoutError::Disconnected) => {}
            other => panic!("expected Disconnected after close_and_join, got {other:?}"),
        }
    }

    /// ADR-0042 §5 applied to replace: `splice_inbox` clears the
    /// filter before joining the old dispatcher. The successor
    /// instance starts with an empty slot.
    #[test]
    fn splice_inbox_cancels_in_flight_wait() {
        let entry = spawn_entry();
        let slot = entry.filter_slot();
        let rx = slot.install(0xAA);

        let (_old_component, _new_rx) = splice_inbox(&entry);

        match rx.recv_timeout(Duration::from_millis(100)) {
            Err(RecvTimeoutError::Disconnected) => {}
            other => panic!("expected Disconnected after splice_inbox, got {other:?}"),
        }
    }
}
