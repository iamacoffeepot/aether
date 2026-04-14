// Worker-pool scheduler. Shape borrowed from
// `aether-mail-spike-host/src/scheduler.rs` per ADR-0004: shared queue,
// per-component `Mutex`, frame-barrier counter, all under `std`
// primitives only. The spike crate is not a dependency.
//
// Design notes carried from ADR-0004:
//   - Single `Mutex<VecDeque<Mail>>` + `Condvar` as the shared queue.
//     Work-stealing per-worker deques are the identified next-lever
//     candidate but are not pulled here.
//   - Sinks are NOT dispatched here. They are handled inline by
//     `SubstrateCtx::send` when a component invokes the `send_mail`
//     host function; they never enter the queue under normal use.
//     If mail for a sink does end up in the queue (e.g. a future
//     caller chooses to enqueue one), the worker handles it in line
//     with the component path — lookup, call, decrement.
//
// ADR-0010 makes the component table runtime-mutable: load_component
// inserts, drop_component removes, replace_component rebinds. Reads
// take the shared lock (one per dispatch); writes are rare and held
// briefly (insert/remove). The per-component `Mutex` serialises
// deliver calls for a single component as before.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};

use crate::component::Component;
use crate::mail::MailboxId;
use crate::queue::MailQueue;
use crate::registry::{MailboxEntry, Registry};

/// Shared, runtime-mutable table of bound components. Cloned into the
/// scheduler's workers and into the ADR-0010 load handler so both read
/// and write through the same `RwLock`. The inner `Mutex<Component>`
/// still serialises deliver calls per-component.
pub type ComponentTable = Arc<RwLock<HashMap<MailboxId, Mutex<Component>>>>;

/// Owned by the scheduler, shared with every worker. Separate from the
/// public `Scheduler` handle so workers can keep running even while the
/// owner thread is asleep waiting on a frame drain.
struct WorkerContext {
    queue: Arc<MailQueue>,
    registry: Arc<Registry>,
    components: ComponentTable,
}

pub struct Scheduler {
    ctx: Arc<WorkerContext>,
    workers: Vec<JoinHandle<()>>,
}

impl Scheduler {
    /// Build a scheduler over `components` keyed by `MailboxId`. The
    /// registry is the same one every component's `SubstrateCtx` holds
    /// — it defines what mailbox names resolve to what entries.
    pub fn new(
        registry: Arc<Registry>,
        queue: Arc<MailQueue>,
        components: HashMap<MailboxId, Component>,
        k_workers: usize,
    ) -> Self {
        assert!(k_workers >= 1, "need at least one worker");

        let components: ComponentTable = Arc::new(RwLock::new(
            components
                .into_iter()
                .map(|(id, c)| (id, Mutex::new(c)))
                .collect(),
        ));
        let ctx = Arc::new(WorkerContext {
            queue,
            registry,
            components,
        });

        let mut workers = Vec::with_capacity(k_workers);
        for _ in 0..k_workers {
            let ctx = Arc::clone(&ctx);
            workers.push(thread::spawn(move || worker_loop(ctx)));
        }

        Self { ctx, workers }
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

    /// Insert a freshly instantiated component under `id`. Called by
    /// the load handler once instantiation succeeds and the mailbox
    /// id has been assigned. Silently replaces any existing component
    /// at `id` — replacement is an ADR-0010 primitive in its own
    /// right, wired in a later PR.
    pub fn add_component(&self, id: MailboxId, component: Component) {
        self.ctx
            .components
            .write()
            .unwrap()
            .insert(id, Mutex::new(component));
    }

    /// Remove and return the component bound to `id`, if any. The
    /// caller is responsible for ensuring no further mail is dispatched
    /// to this mailbox; workers that look up `id` after removal log a
    /// "no component bound" drop, consistent with the pre-ADR-0010
    /// unknown-mailbox path.
    pub fn remove_component(&self, id: MailboxId) -> Option<Component> {
        self.ctx
            .components
            .write()
            .unwrap()
            .remove(&id)
            .map(|m| m.into_inner().expect("component mutex poisoned"))
    }

    /// Atomically rebind `id` to a freshly instantiated component
    /// (ADR-0010). Returns the old component so the caller can drop
    /// it after the swap; wasmtime resources (linear memory, Store)
    /// are reclaimed when the returned value falls out of scope.
    /// The entry is replaced under the write lock so no worker can
    /// observe a gap between old and new — any mail that gets popped
    /// after the swap is delivered to the new instance.
    pub fn replace_component(&self, id: MailboxId, new_component: Component) -> Option<Component> {
        let mut table = self.ctx.components.write().unwrap();
        let old = table
            .remove(&id)
            .map(|m| m.into_inner().expect("component mutex poisoned"));
        table.insert(id, Mutex::new(new_component));
        old
    }
}

impl Drop for Scheduler {
    fn drop(&mut self) {
        self.ctx.queue.initiate_shutdown();
        for h in self.workers.drain(..) {
            let _ = h.join();
        }
    }
}

fn worker_loop(ctx: Arc<WorkerContext>) {
    while let Some(mail) = ctx.queue.pop_blocking() {
        let recipient = mail.recipient;
        match ctx.registry.entry(recipient) {
            Some(MailboxEntry::Sink(handler)) => {
                let kind_name = ctx.registry.kind_name(mail.kind).unwrap_or_default();
                // Mail reaching a sink through the scheduler queue
                // came from substrate core (e.g. the frame loop's
                // FrameStats push) and has no sending mailbox; per
                // ADR-0011 origin is `None`. Components reach sinks
                // inline via `SubstrateCtx::send`, not this path.
                handler(&kind_name, None, mail.sender, &mail.payload, mail.count);
            }
            Some(MailboxEntry::Component) => {
                let components = ctx.components.read().unwrap();
                match components.get(&recipient) {
                    Some(lock) => {
                        let mut c = lock.lock().unwrap();
                        c.deliver(&mail).expect("component.deliver failed");
                    }
                    None => {
                        eprintln!(
                            "substrate: mail to registered-component mailbox {:?} \
                             but no component bound to it — dropped",
                            recipient
                        );
                    }
                }
            }
            Some(MailboxEntry::Dropped) => {
                eprintln!(
                    "substrate: mail to dropped mailbox {:?} — discarded",
                    recipient
                );
            }
            None => {
                eprintln!(
                    "substrate: mail to unknown mailbox {:?} — dropped",
                    recipient
                );
            }
        }
        ctx.queue.mark_completed();
    }
}
