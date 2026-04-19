// Hand-rolled worker-pool scheduler for the issue #14 spike.
//
// Model:
//   - K worker threads, each owning nothing; all state is shared via `Arc`.
//   - Actors live in a shared `Vec<Mutex<Actor>>`. Each `wasmtime::Store` is
//     `Send` but not `Sync`, so only one worker touches an actor at a time.
//   - A single shared queue (`Mutex<VecDeque<Tick>>` + `Condvar`) feeds
//     workers. No per-worker deques, no work-stealing — simplest honest
//     baseline. If lock contention shows up at high K, that IS the finding.
//   - Frame barrier is an `outstanding` counter guarded by its own mutex +
//     condvar. Main submits N ticks, waits for outstanding to reach 0.
//
// Primitives are std-only (no rayon, no crossbeam) so the scheduler we are
// measuring is end-to-end ours.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};

use crate::{Actor, KIND_TICK, Mail};

/// One unit of work handed to the scheduler. For PR A every tick carries a
/// single u32 payload (`work_units`) that the guest interprets as
/// KIND_TICK. Future workloads will grow this into an enum.
pub struct Tick {
    pub actor_id: u32,
    pub work_units: u32,
}

struct Shared {
    queue: Mutex<VecDeque<Tick>>,
    queue_cv: Condvar,
    // Ticks submitted this frame that haven't yet been processed to
    // completion. Main blocks on `done_cv` until this reaches 0.
    outstanding: Mutex<usize>,
    done_cv: Condvar,
    shutdown: AtomicBool,
}

pub struct Scheduler {
    shared: Arc<Shared>,
    actors: Arc<Vec<Mutex<Actor>>>,
    workers: Vec<JoinHandle<()>>,
}

impl Scheduler {
    pub fn new(actors: Vec<Actor>, k_workers: usize) -> Self {
        assert!(k_workers >= 1, "need at least one worker");
        let shared = Arc::new(Shared {
            queue: Mutex::new(VecDeque::new()),
            queue_cv: Condvar::new(),
            outstanding: Mutex::new(0),
            done_cv: Condvar::new(),
            shutdown: AtomicBool::new(false),
        });
        let actors: Arc<Vec<Mutex<Actor>>> = Arc::new(actors.into_iter().map(Mutex::new).collect());

        let mut workers = Vec::with_capacity(k_workers);
        for _ in 0..k_workers {
            let shared = Arc::clone(&shared);
            let actors = Arc::clone(&actors);
            workers.push(thread::spawn(move || worker_loop(shared, actors)));
        }

        Self {
            shared,
            actors,
            workers,
        }
    }

    pub fn n_actors(&self) -> usize {
        self.actors.len()
    }

    /// Submit every tick in `ticks` for this frame and block until all of
    /// them have been processed. `ticks.len()` sets the frame's
    /// outstanding counter; callers must not call `run_frame` reentrantly.
    pub fn run_frame(&self, ticks: Vec<Tick>) {
        let n = ticks.len();
        if n == 0 {
            return;
        }

        // Set outstanding BEFORE pushing ticks so a worker can't pop a tick
        // and race the done-signal past main's wait. (Invariant:
        // outstanding >= queue.len() + in_flight at every observable point.)
        {
            let mut outstanding = self.shared.outstanding.lock().unwrap();
            debug_assert_eq!(*outstanding, 0, "previous frame did not drain");
            *outstanding = n;
        }
        {
            let mut q = self.shared.queue.lock().unwrap();
            q.extend(ticks);
        }
        self.shared.queue_cv.notify_all();

        let mut outstanding = self.shared.outstanding.lock().unwrap();
        while *outstanding > 0 {
            outstanding = self.shared.done_cv.wait(outstanding).unwrap();
        }
    }
}

impl Drop for Scheduler {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::SeqCst);
        self.shared.queue_cv.notify_all();
        for h in self.workers.drain(..) {
            let _ = h.join();
        }
    }
}

fn worker_loop(shared: Arc<Shared>, actors: Arc<Vec<Mutex<Actor>>>) {
    loop {
        // Pop a tick, or exit on shutdown.
        let tick = {
            let mut q = shared.queue.lock().unwrap();
            loop {
                if let Some(t) = q.pop_front() {
                    break t;
                }
                if shared.shutdown.load(Ordering::SeqCst) {
                    return;
                }
                q = shared.queue_cv.wait(q).unwrap();
            }
        };

        // Deliver. One worker at a time per actor (Mutex<Actor>).
        {
            let mut actor = actors[tick.actor_id as usize].lock().unwrap();
            let payload = tick.work_units.to_le_bytes();
            let mail = Mail {
                recipient: tick.actor_id,
                kind: KIND_TICK,
                batch_bytes: &payload,
                batch_count: 1,
            };
            actor.deliver(&mail).expect("actor.deliver failed");
        }

        // Signal completion. Grab the outstanding mutex, decrement, and
        // notify main only on the last-tick transition.
        let mut outstanding = shared.outstanding.lock().unwrap();
        *outstanding -= 1;
        if *outstanding == 0 {
            shared.done_cv.notify_all();
        }
    }
}
