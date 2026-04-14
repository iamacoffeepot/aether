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

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use crate::component::Component;
use crate::mail::MailboxId;
use crate::queue::MailQueue;
use crate::registry::{MailboxEntry, Registry};

/// Owned by the scheduler, shared with every worker. Separate from the
/// public `Scheduler` handle so workers can keep running even while the
/// owner thread is asleep waiting on a frame drain.
struct WorkerContext {
    queue: Arc<MailQueue>,
    registry: Arc<Registry>,
    components: HashMap<MailboxId, Mutex<Component>>,
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

        let ctx = Arc::new(WorkerContext {
            queue,
            registry,
            components: components
                .into_iter()
                .map(|(id, c)| (id, Mutex::new(c)))
                .collect(),
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
                let kind_name = ctx.registry.kind_name(mail.kind).unwrap_or("");
                // Mail reaching a sink through the scheduler queue
                // came from substrate core (e.g. the frame loop's
                // FrameStats push) and has no sending mailbox; per
                // ADR-0011 origin is `None`. Components reach sinks
                // inline via `SubstrateCtx::send`, not this path.
                handler(kind_name, None, mail.sender, &mail.payload, mail.count);
            }
            Some(MailboxEntry::Component) => match ctx.components.get(&recipient) {
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
            },
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
