// Actor-per-component dispatch (ADR-0038 Phases 1–2).
//
// Phase 1 gave each component a dedicated dispatcher thread that
// loops on an mpsc inbox. Phase 2 retires the router thread that
// Phase 1 left in place: `MailQueue::push` now resolves the
// recipient inline on the caller's thread and forwards directly into
// the per-component inbox (see `queue.rs`). The per-mailbox dispatcher
// is unchanged.
//
// `wait_idle` semantics are preserved: `MailQueue`'s `outstanding`
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
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};

use crate::component::{Component, DISPATCH_UNKNOWN_KIND};
use crate::mail::{Mail, MailboxId};
use crate::queue::MailQueue;
use crate::registry::Registry;

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
}

impl ComponentEntry {
    /// Spawn a dispatcher thread for `component`, wire it to a fresh
    /// mpsc inbox, and return the entry. `queue` is the shared
    /// `MailQueue`; the dispatcher calls `mark_completed` on it after
    /// each delivery so `wait_idle` still reflects end-to-end
    /// completion.
    pub fn spawn(component: Component, queue: Arc<MailQueue>, registry: Arc<Registry>) -> Self {
        let (tx, rx) = mpsc::channel();
        let handle = thread::Builder::new()
            .name("aether-component-dispatch".into())
            .spawn(move || dispatcher_loop(component, rx, queue, registry))
            .expect("spawn component dispatcher");
        Self {
            sender: Mutex::new(Some(tx)),
            handle: Mutex::new(Some(handle)),
        }
    }

    /// Forward `mail` to this component's inbox. Returns `false` if
    /// the inbox is closed (caller should warn-and-drop).
    pub fn send(&self, mail: Mail) -> bool {
        match self.sender.lock().unwrap().as_ref() {
            Some(tx) => tx.send(mail).is_ok(),
            None => false,
        }
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
/// restore the old `Component` onto the post-splice inbox).
pub fn spawn_dispatcher_on(
    entry: &Arc<ComponentEntry>,
    component: Component,
    rx: Receiver<Mail>,
    queue: Arc<MailQueue>,
    registry: Arc<Registry>,
) {
    let handle = thread::Builder::new()
        .name("aether-component-dispatch".into())
        .spawn(move || dispatcher_loop(component, rx, queue, registry))
        .expect("spawn component dispatcher");
    let prev = entry.handle.lock().unwrap().replace(handle);
    debug_assert!(prev.is_none(), "entry handle slot must be empty");
}

fn dispatcher_loop(
    mut component: Component,
    rx: Receiver<Mail>,
    queue: Arc<MailQueue>,
    registry: Arc<Registry>,
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
        queue.mark_completed();
    }
    component
}

/// Shared, runtime-mutable table of bound components. Cloned into the
/// `MailQueue` (for inline routing on push) and into the ADR-0010
/// load handler so both read and write through the same `RwLock`.
/// Values are `Arc`-shared so short-lived clones (e.g. the router's
/// forward path) can outlive a concurrent `remove` without racing on
/// `ComponentEntry`'s `Drop`.
pub type ComponentTable = Arc<RwLock<HashMap<MailboxId, Arc<ComponentEntry>>>>;

pub struct Scheduler {
    queue: Arc<MailQueue>,
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
    pub fn new(registry: Arc<Registry>, queue: Arc<MailQueue>, _k_workers: usize) -> Self {
        let components: ComponentTable = Arc::new(RwLock::new(HashMap::new()));
        queue.wire(Arc::clone(&registry), Arc::clone(&components));
        Self {
            queue,
            registry,
            components,
        }
    }

    pub fn queue(&self) -> &Arc<MailQueue> {
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
            Arc::clone(&self.queue),
            Arc::clone(&self.registry),
        );
        self.components.write().unwrap().insert(id, Arc::new(entry));
    }
}
// Per-component dispatcher threads exit when their `ComponentEntry`
// Arc drops (the `Sender` drops with it, the inbox closes, `recv()`
// returns `None`). The scheduler no longer owns a router thread, so
// its `Drop` impl is redundant — the owning layer (chassis / test)
// disposes of the `ComponentTable` when it wants dispatchers to exit.
