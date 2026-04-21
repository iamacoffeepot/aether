// Actor-per-component dispatch (ADR-0038 Phase 1).
//
// Each component owns a dedicated dispatcher thread that loops on an
// mpsc inbox. The shared `MailQueue` is still pushed to by the existing
// senders (host-fn `send_mail_p32`, platform input fan-out, hub-
// delivered mail); a single router thread pops from it and forwards
// each Mail to either the inline sink handler or the recipient
// component's inbox. Per-mailbox FIFO is the channel's natural shape,
// so the strand claim that the pre-ADR-0038 worker pool needed is gone
// — along with `pending` / `frozen` / `strand_scheduled` / `parked`.
//
// wait_idle semantics are preserved: the shared queue's `outstanding`
// counter increments on `push` and decrements inside the dispatcher
// thread after `deliver` returns (for Component recipients) or inside
// the router after a sink call (for Sink recipients). A drop-warn'd
// mail decrements immediately.
//
// Shutdown: `Scheduler::Drop` initiates shutdown on the queue, waking
// the router, and joins it. Per-component dispatcher threads exit when
// their `ComponentEntry` Arc drops (the `Sender` drops with it, the
// inbox closes, `recv()` returns `None`, the thread returns the
// `Component`). The owning layer (chassis / test) is responsible for
// dropping the `ComponentTable` if it wants dispatchers to exit.
//
// Phase 2 will retire the shared queue entirely: senders push directly
// to per-component inboxes, the router thread goes away, and
// `MailQueue::outstanding` / `wait_idle` are replaced by per-mailbox
// drain primitives.

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};

use crate::component::{Component, DISPATCH_UNKNOWN_KIND};
use crate::mail::{Mail, MailboxId};
use crate::queue::MailQueue;
use crate::registry::{MailboxEntry, Registry};

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
/// scheduler's router and into the ADR-0010 load handler so both read
/// and write through the same `RwLock`. Values are `Arc`-shared so
/// short-lived clones (e.g. in the router's forward path) can outlive
/// a concurrent `remove` without racing on `ComponentEntry`'s `Drop`.
pub type ComponentTable = Arc<RwLock<HashMap<MailboxId, Arc<ComponentEntry>>>>;

/// Owned by the scheduler, shared with the router. Separate from the
/// public `Scheduler` handle so the router can keep running while the
/// owner thread is asleep on a `wait_idle`.
struct WorkerContext {
    queue: Arc<MailQueue>,
    registry: Arc<Registry>,
    components: ComponentTable,
}

pub struct Scheduler {
    ctx: Arc<WorkerContext>,
    router: Option<JoinHandle<()>>,
}

impl Scheduler {
    /// Build a scheduler over an empty component table. Spawns a
    /// single router thread that pops from `queue` and forwards each
    /// mail to the appropriate per-component inbox or the inline sink
    /// handler.
    ///
    /// The `_k_workers` parameter is retained for boot-config
    /// compatibility (ADR-0004 sized the worker pool) but is ignored
    /// under ADR-0038: dispatch parallelism is one thread per
    /// component, not a shared pool.
    pub fn new(registry: Arc<Registry>, queue: Arc<MailQueue>, _k_workers: usize) -> Self {
        let components: ComponentTable = Arc::new(RwLock::new(HashMap::new()));
        let ctx = Arc::new(WorkerContext {
            queue,
            registry,
            components,
        });
        let ctx_r = Arc::clone(&ctx);
        let router = thread::Builder::new()
            .name("aether-mail-router".into())
            .spawn(move || router_loop(ctx_r))
            .expect("spawn router");
        Self {
            ctx,
            router: Some(router),
        }
    }

    pub fn queue(&self) -> &Arc<MailQueue> {
        &self.ctx.queue
    }

    pub fn registry(&self) -> &Arc<Registry> {
        &self.ctx.registry
    }

    /// Handle to the runtime-mutable component table. ADR-0010's load
    /// handler holds a clone and inserts newly instantiated components
    /// without needing an `Arc<Scheduler>` — which would create a
    /// cycle through any registry sink closures that referenced back.
    pub fn components(&self) -> &ComponentTable {
        &self.ctx.components
    }

    /// Spawn a dispatcher thread for `component` and register the
    /// entry under `id`. Silently replaces any existing component at
    /// `id` — replacement is an ADR-0010 primitive in its own right,
    /// handled by `ControlPlane::handle_replace`.
    pub fn add_component(&self, id: MailboxId, component: Component) {
        let entry = ComponentEntry::spawn(
            component,
            Arc::clone(&self.ctx.queue),
            Arc::clone(&self.ctx.registry),
        );
        self.ctx
            .components
            .write()
            .unwrap()
            .insert(id, Arc::new(entry));
    }
}

impl Drop for Scheduler {
    fn drop(&mut self) {
        self.ctx.queue.initiate_shutdown();
        if let Some(h) = self.router.take() {
            let _ = h.join();
        }
        // Per-component dispatcher threads exit when their
        // `ComponentEntry` Arc drops (the `Sender` drops with it, the
        // inbox closes, `recv()` returns `None`). Explicit join
        // happens in `handle_drop` / `handle_replace`; on scheduler
        // drop we let the owning layer (chassis / test) dispose of
        // the `ComponentTable`.
    }
}

fn router_loop(ctx: Arc<WorkerContext>) {
    while let Some(mail) = ctx.queue.pop_blocking() {
        dispatch_mail(&ctx, mail);
    }
}

/// Route one mail. Sinks run inline on the router thread (consistent
/// with the pre-ADR-0038 behaviour for mail that arrived at a sink via
/// the queue, e.g. FrameStats). Component-bound mail is forwarded to
/// the recipient's inbox; the per-component dispatcher calls
/// `mark_completed` after `deliver` returns. Dropped / unknown
/// recipients are discarded with a warn-log and immediate
/// `mark_completed`.
fn dispatch_mail(ctx: &Arc<WorkerContext>, mail: Mail) {
    let recipient = mail.recipient;
    match ctx.registry.entry(recipient) {
        Some(MailboxEntry::Sink(handler)) => {
            let kind_name = ctx.registry.kind_name(mail.kind).unwrap_or_default();
            // Mail reaching a sink through the scheduler queue came
            // from substrate core (e.g. the frame loop's FrameStats
            // push) and has no sending mailbox; per ADR-0011 origin is
            // `None`. Components reach sinks inline via
            // `SubstrateCtx::send`, not this path.
            handler(
                mail.kind,
                &kind_name,
                None,
                mail.sender,
                &mail.payload,
                mail.count,
            );
            ctx.queue.mark_completed();
        }
        Some(MailboxEntry::Component) => {
            let entry = ctx
                .components
                .read()
                .unwrap()
                .get(&recipient)
                .map(Arc::clone);
            match entry {
                Some(entry) => {
                    if !entry.send(mail) {
                        // Inbox closed — the component was dropped
                        // between our registry check and our send.
                        // Drop-warn and complete; matches the
                        // Dropped-mailbox branch below in observable
                        // behaviour.
                        tracing::warn!(
                            target: "aether_substrate::scheduler",
                            mailbox = ?recipient,
                            "component inbox closed; mail discarded",
                        );
                        ctx.queue.mark_completed();
                    }
                    // Happy path: dispatcher owns mark_completed.
                }
                None => {
                    tracing::warn!(
                        target: "aether_substrate::scheduler",
                        mailbox = ?recipient,
                        "mail to registered-component mailbox but no component bound — dropped",
                    );
                    ctx.queue.mark_completed();
                }
            }
        }
        Some(MailboxEntry::Dropped) => {
            tracing::warn!(
                target: "aether_substrate::scheduler",
                mailbox = ?recipient,
                "mail to dropped mailbox — discarded",
            );
            ctx.queue.mark_completed();
        }
        None => {
            tracing::warn!(
                target: "aether_substrate::scheduler",
                mailbox = ?recipient,
                "mail to unknown mailbox — dropped",
            );
            ctx.queue.mark_completed();
        }
    }
}
