//! ADR-0048 §3 dedicated transform compute pool
//! (iamacoffeepot/aether#1012).
//!
//! A native transform is a pure `fn` with no instance and no thread
//! affinity — it is `Send`. The executor does **not** run transforms
//! inline (a slow transform would stall the parking/reaping loop) and
//! does **not** run them on any actor thread. Instead they run on this
//! dedicated pool — a bounded set of OS threads, separate from every
//! actor thread, sized independently (default: available parallelism).
//!
//! This is emphatically **not** the actor worker-pool ADR-0038 retired
//! — that was actor dispatch (instances, lifecycles, strand-claims);
//! this is a pure-compute pool of stateless `fn` calls with none of
//! that machinery.
//!
//! Completion is signalled the same way the reaper timer wakes the
//! executor: the worker stashes the outcome in a shared map keyed by
//! `job_id`, then pushes a `DagTransformDone { job_id }` mail at the
//! `aether.dag` mailbox so the executor pulls the result and resolves /
//! fails the node on its own single-threaded actor thread.

use std::any::Any;
use std::collections::HashMap;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use aether_data::{InvokeFn, KindId, MailboxId, TransformError};
use aether_kinds::DagTransformDone;

use crate::mail::Mail;
use crate::mail::mailer::Mailer;

const TARGET: &str = "aether::dag::transform_pool";

/// The result of one off-thread transform invocation (ADR-0048 §3/§6).
/// A domain `Err` value the transform itself returns is **not** here —
/// that's a successful `Ok(bytes)` whose bytes encode an `Err` variant
/// (ADR-0048 §6 "domain Err is not a DAG failure"). This enum captures
/// only the runtime-abort classes the executor maps to node failure.
pub enum TransformOutcome {
    /// The transform returned; `bytes` is its encoded output.
    Ok { bytes: Vec<u8> },
    /// The thunk reported a decode / arity error (ADR-0048 §1).
    Err { error: TransformError },
    /// The transform `panic!`d; `message` is the unwind payload as a
    /// string (ADR-0048 §6 panic = failure).
    Panicked { message: String },
}

/// One queued invocation. The pool worker owns the inputs (copied off
/// the handle store before submission so the worker borrows no shared
/// state) and the type-erased `invoke` thunk.
struct Job {
    id: u64,
    invoke: InvokeFn,
    inputs: Vec<Vec<u8>>,
    /// Where to post the completion wake.
    wake_mailbox: MailboxId,
    wake_kind: KindId,
}

/// Dedicated transform compute pool. Owned by the executor.
pub struct TransformPool {
    tx: Option<Sender<Job>>,
    workers: Vec<JoinHandle<()>>,
    /// Completed outcomes keyed by `job_id`, drained by the executor on
    /// the `DagTransformDone` wake.
    outcomes: Arc<Mutex<HashMap<u64, TransformOutcome>>>,
    /// Count of actual `invoke` calls (cache hits never reach the pool,
    /// so this excludes them). Test instrumentation for the
    /// `transform_skips_invoke_on_cache_hit` fixture.
    invoke_count: Arc<AtomicUsize>,
    /// Monotonic job-id allocator.
    next_job: u64,
}

impl TransformPool {
    /// Spawn `threads` worker threads (clamped to at least 1) fed by a
    /// shared job channel. The workers hold a clone of the `Mailer` so
    /// they can post the completion wake; they do **not** touch the
    /// handle store or any `DagState` directly.
    #[must_use]
    pub fn new(threads: usize, mailer: &Arc<Mailer>) -> Self {
        let threads = threads.max(1);
        let (tx, rx) = mpsc::channel::<Job>();
        let rx = Arc::new(Mutex::new(rx));
        let outcomes: Arc<Mutex<HashMap<u64, TransformOutcome>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let invoke_count = Arc::new(AtomicUsize::new(0));

        let mut workers = Vec::with_capacity(threads);
        for i in 0..threads {
            let rx = Arc::clone(&rx);
            let outcomes = Arc::clone(&outcomes);
            let invoke_count = Arc::clone(&invoke_count);
            let mailer = Arc::clone(mailer);
            // DAG transform worker pool — execution floor below the actor model,
            // built without a ctx; not per-handler chain work.
            #[allow(clippy::disallowed_methods)]
            let spawned = thread::Builder::new()
                .name(format!("aether-transform-{i}"))
                .spawn(move || worker_loop(&rx, &outcomes, &invoke_count, &mailer));
            match spawned {
                Ok(handle) => workers.push(handle),
                Err(e) => {
                    tracing::warn!(
                        target: TARGET,
                        error = %e,
                        worker = i,
                        "failed to spawn transform compute worker",
                    );
                }
            }
        }

        Self {
            tx: Some(tx),
            workers,
            outcomes,
            invoke_count,
            next_job: 1,
        }
    }

    /// Submit a transform invocation to the pool. Returns the allocated
    /// `job_id`; the worker posts a `DagTransformDone { job_id }` wake
    /// once the call returns (or panics), and the executor reads the
    /// outcome via [`Self::take_outcome`].
    pub fn submit(
        &mut self,
        invoke: InvokeFn,
        inputs: Vec<Vec<u8>>,
        wake_mailbox: MailboxId,
        wake_kind: KindId,
    ) -> u64 {
        let job_id = self.next_job;
        self.next_job = self.next_job.wrapping_add(1);
        if self.next_job == 0 {
            self.next_job = 1;
        }
        let job = Job {
            id: job_id,
            invoke,
            inputs,
            wake_mailbox,
            wake_kind,
        };
        if let Some(tx) = &self.tx
            && tx.send(job).is_err()
        {
            tracing::warn!(
                target: TARGET,
                job_id,
                "transform pool channel closed; invocation dropped",
            );
        }
        job_id
    }

    /// Drain the outcome for a completed `job_id`, or `None` if the
    /// worker hasn't posted it yet (or it was already taken). Called by
    /// the executor on the `DagTransformDone` wake.
    ///
    /// # Panics
    /// Panics if the outcome mutex is poisoned — fail-fast per ADR-0063.
    #[must_use]
    pub fn take_outcome(&self, job_id: u64) -> Option<TransformOutcome> {
        self.outcomes
            .lock()
            .expect("transform pool outcome mutex poisoned; fail-fast per ADR-0063")
            .remove(&job_id)
    }

    /// Discard a stashed outcome for a job the executor already failed
    /// (e.g. a timeout fired before the worker finished). Keeps the
    /// completion map from leaking entries for orphaned threads.
    ///
    /// # Panics
    /// Panics if the outcome mutex is poisoned — fail-fast per ADR-0063.
    pub fn forget(&self, job_id: u64) {
        self.outcomes
            .lock()
            .expect("transform pool outcome mutex poisoned; fail-fast per ADR-0063")
            .remove(&job_id);
    }

    /// Total `invoke` calls the pool has started (excludes cache hits,
    /// which never reach the pool). Test instrumentation.
    #[must_use]
    pub fn invoke_count(&self) -> usize {
        self.invoke_count.load(Ordering::Acquire)
    }

    /// Shut the pool down: close the job channel + join every worker.
    /// Called from the cap's `unwire`. Idempotent.
    pub fn shutdown(&mut self) {
        // Dropping the only `Sender` closes the channel; workers see
        // `Err(RecvError)` and exit their loop.
        self.tx = None;
        for handle in self.workers.drain(..) {
            let _ = handle.join();
        }
    }
}

impl Drop for TransformPool {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// One worker thread's loop: pull a job, run its thunk under
/// `catch_unwind`, stash the outcome, post the wake.
fn worker_loop(
    rx: &Arc<Mutex<mpsc::Receiver<Job>>>,
    outcomes: &Arc<Mutex<HashMap<u64, TransformOutcome>>>,
    invoke_count: &Arc<AtomicUsize>,
    mailer: &Arc<Mailer>,
) {
    loop {
        // Hold the receiver lock only across `recv`, so siblings can
        // pick up the next job while this one computes.
        let job = {
            let guard = rx
                .lock()
                .expect("transform pool receiver mutex poisoned; fail-fast per ADR-0063");
            guard.recv()
        };
        let Ok(job) = job else {
            // Channel closed (pool dropped) — exit.
            return;
        };

        invoke_count.fetch_add(1, Ordering::AcqRel);
        let outcome = run_job(&job);

        outcomes
            .lock()
            .expect("transform pool outcome mutex poisoned; fail-fast per ADR-0063")
            .insert(job.id, outcome);

        // Wake the executor on its own thread. The payload carries the
        // job id so the executor can pull the stashed outcome.
        let payload = aether_data::Kind::encode_into_bytes(&DagTransformDone { job_id: job.id });
        mailer.push(Mail::new(job.wake_mailbox, job.wake_kind, payload, 1));
    }
}

/// Run one job's thunk under `catch_unwind` (ADR-0048 §6 panic =
/// failure). A panic inside the transform is contained here — the pure
/// `fn` shares no mutable state, so a panic can't poison the executor or
/// any actor.
fn run_job(job: &Job) -> TransformOutcome {
    let slices: Vec<&[u8]> = job.inputs.iter().map(Vec::as_slice).collect();
    let invoke = job.invoke;
    let result = panic::catch_unwind(AssertUnwindSafe(|| invoke(&slices)));
    match result {
        Ok(Ok(bytes)) => TransformOutcome::Ok { bytes },
        Ok(Err(error)) => TransformOutcome::Err { error },
        Err(payload) => TransformOutcome::Panicked {
            message: panic_message(payload.as_ref()),
        },
    }
}

/// Best-effort extraction of a panic payload's message.
fn panic_message(payload: &(dyn Any + Send)) -> String {
    payload
        .downcast_ref::<&'static str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "<non-string panic payload>".to_owned())
}
