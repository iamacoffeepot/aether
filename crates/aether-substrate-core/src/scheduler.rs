// Worker-pool scheduler. Shape borrowed from
// `spikes/aether-mail-spike-host/src/scheduler.rs` per ADR-0004:
// shared queue, per-component `Mutex`, frame-barrier counter, all
// under `std` primitives only. The spike crate is not a dependency.
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
//
// ADR-0022 layers freeze-drain semantics on top of the table:
// `replace_component` flips a per-entry `frozen` flag so workers park
// new mail on the entry instead of dispatching, then waits for the
// per-entry `pending` count of in-flight `deliver` calls to reach
// zero before swapping. The swap step (and the parked-mail flush) is
// driven by `ControlPlane::handle_replace`; this module just owns
// the per-entry state and the worker dispatch path that respects it.
//
// Issue 157 (2026-04-20): the original worker loop popped mail in
// FIFO order from the main queue and serialised delivery through
// `Mutex<Component>`. That preserved "one deliver at a time per
// component" but NOT "deliver in pop order": mutex acquisition is
// non-FIFO under contention, so two workers each popping sequential
// mails for the same mailbox could invert the deliver order (the
// `_row_/_col_` scramble the tic-tac-toe batch smoke caught). The fix
// is the per-mailbox strand: `ComponentEntry.strand_scheduled` is a
// claim flag the worker sets under the queue lock during the pop
// scan, and the strand owner drains every same-recipient mail still
// in the queue before releasing. Different mailboxes still dispatch
// in parallel; only same-mailbox mail serialises, and now it
// serialises in pop (== push) order.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};

use crate::component::Component;
use crate::mail::{Mail, MailboxId};
use crate::queue::MailQueue;
use crate::registry::{MailboxEntry, Registry};

/// Per-mailbox scheduler state. Beyond the wasmtime instance itself we
/// track ADR-0022's freeze-drain bookkeeping: `pending` counts mail
/// currently being delivered to this component, `frozen` halts dispatch
/// for replace's quiescence window, and `parked` holds mail popped
/// while frozen so it can be replayed against whichever component is
/// bound after the swap (new instance on success, old on timeout).
pub struct ComponentEntry {
    pub component: Mutex<Component>,
    pub pending: AtomicU32,
    pub frozen: AtomicBool,
    pub parked: Mutex<VecDeque<Mail>>,
    /// Per-mailbox strand lock. A worker that pops mail for this
    /// component flips this to `true` before leaving the queue's scan
    /// path; other workers whose scan lands on mail for the same
    /// recipient treat the strand as unavailable and keep scanning.
    /// The strand owner drains every same-recipient mail in the queue
    /// before flipping this back to `false`. Net effect: FIFO per
    /// mailbox even across multiple workers, without serialising the
    /// whole scheduler.
    ///
    /// Preserves the invariant the old `Mutex<Component>`-only design
    /// tried to rely on: that `pop_blocking` FIFO order equals deliver
    /// order for a given mailbox. The `Mutex<Component>` alone did not
    /// preserve it — mutex acquisition is not FIFO under contention, so
    /// two workers popping mail for the same mailbox could invert the
    /// deliver order. See issue 157.
    pub strand_scheduled: AtomicBool,
}

impl ComponentEntry {
    pub fn new(component: Component) -> Self {
        Self {
            component: Mutex::new(component),
            pending: AtomicU32::new(0),
            frozen: AtomicBool::new(false),
            parked: Mutex::new(VecDeque::new()),
            strand_scheduled: AtomicBool::new(false),
        }
    }
}

/// Shared, runtime-mutable table of bound components. Cloned into the
/// scheduler's workers and into the ADR-0010 load handler so both read
/// and write through the same `RwLock`. Values are `Arc`-shared so the
/// freeze-drain path in `replace_component` can hold a long-lived
/// reference to one entry without keeping the table read lock open.
pub type ComponentTable = Arc<RwLock<HashMap<MailboxId, Arc<ComponentEntry>>>>;

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
                .map(|(id, c)| (id, Arc::new(ComponentEntry::new(c))))
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
    /// right, handled by `ControlPlane::handle_replace` (ADR-0022).
    pub fn add_component(&self, id: MailboxId, component: Component) {
        self.ctx
            .components
            .write()
            .unwrap()
            .insert(id, Arc::new(ComponentEntry::new(component)));
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
    loop {
        // Pick the next runnable mail. For Component recipients,
        // `pop_blocking_if`'s predicate tries to claim the per-mailbox
        // strand atomically; the pop only commits if the claim
        // succeeds. Sinks, dropped/unknown recipients, and dangling
        // component ids (registered but no `ComponentEntry`) pop
        // unconditionally.
        let claimed_entry = std::sync::Mutex::new(None::<Arc<ComponentEntry>>);
        let Some(mail) = ctx.queue.pop_blocking_if(|m| {
            match ctx.registry.entry(m.recipient) {
                Some(MailboxEntry::Component) => {
                    let entry = ctx
                        .components
                        .read()
                        .unwrap()
                        .get(&m.recipient)
                        .map(Arc::clone);
                    match entry {
                        Some(e) => {
                            // `swap(true)` returns the previous value.
                            // If it was `false` we just claimed the
                            // strand; otherwise another worker owns it
                            // and we keep scanning.
                            if !e.strand_scheduled.swap(true, Ordering::AcqRel) {
                                *claimed_entry.lock().unwrap() = Some(e);
                                true
                            } else {
                                false
                            }
                        }
                        // Dangling id: warn-drop path below handles it;
                        // nothing to serialise on.
                        None => true,
                    }
                }
                // Sinks are stateless and safe under concurrent calls;
                // dropped/unknown mail just warn-drops.
                _ => true,
            }
        }) else {
            return;
        };

        // Capture the recipient before `mail` is consumed — we need
        // it to drain same-recipient follow-ups below.
        let recipient = mail.recipient;
        let strand = claimed_entry.lock().unwrap().take();
        dispatch_mail(&ctx, mail, strand.as_ref());

        // If we owned a strand, drain any further mail for the same
        // recipient before releasing it. This is what preserves FIFO
        // per-mailbox: same-recipient mail that arrived between the
        // first pop and this drain goes through us, not through a
        // racing worker.
        if let Some(entry) = strand {
            while let Some(next) = ctx.queue.try_pop_for_recipient(recipient) {
                dispatch_mail(&ctx, next, Some(&entry));
            }
            entry.strand_scheduled.store(false, Ordering::Release);
            // Wake any worker that skipped this recipient during the
            // drain so it can re-scan the queue.
            ctx.queue.notify_waiters();
        }
    }
}

/// Deliver one mail to its recipient. Strand claim (if any) is the
/// caller's job — this function only dispatches. `strand` carries the
/// already-claimed `ComponentEntry` for Component recipients so we
/// don't hit the components table lookup twice.
fn dispatch_mail(ctx: &Arc<WorkerContext>, mail: Mail, strand: Option<&Arc<ComponentEntry>>) {
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
            let entry = match strand {
                Some(e) => Some(Arc::clone(e)),
                None => ctx
                    .components
                    .read()
                    .unwrap()
                    .get(&recipient)
                    .map(Arc::clone),
            };
            match entry {
                Some(entry) => {
                    // ADR-0022 freeze-drain: while frozen, mail is
                    // parked on the entry without entering the pending
                    // count. handle_replace flushes the parked queue
                    // under the write lock once the swap (or timeout)
                    // resolves. The strand claim we made above stays
                    // live — the freeze path coordinates with us via
                    // `pending` (which we never incremented for parked
                    // mail), not via `strand_scheduled`.
                    if entry.frozen.load(Ordering::Acquire) {
                        entry.parked.lock().unwrap().push_back(mail);
                        ctx.queue.mark_completed();
                        return;
                    }
                    entry.pending.fetch_add(1, Ordering::AcqRel);
                    let rc = {
                        let mut c = entry.component.lock().unwrap();
                        c.deliver(&mail).expect("component.deliver failed")
                    };
                    entry.pending.fetch_sub(1, Ordering::AcqRel);
                    if rc == crate::component::DISPATCH_UNKNOWN_KIND {
                        let kind_name = ctx
                            .registry
                            .kind_name(mail.kind)
                            .unwrap_or_else(|| format!("kind#{:#x}", mail.kind));
                        tracing::warn!(
                            target: "aether_substrate::scheduler",
                            mailbox = ?recipient,
                            kind = %kind_name,
                            "component has no handler for mail kind (ADR-0033 strict receiver); dropped",
                        );
                    }
                    ctx.queue.mark_completed();
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
