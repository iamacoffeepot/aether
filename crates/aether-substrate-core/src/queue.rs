// Shared mail queue + frame barrier. Held by Arc so senders, the
// router, the per-component dispatchers, and the scheduler owner all
// share one instance.
//
// Invariants:
//   - `push` increments `outstanding` BEFORE pushing into the deque.
//     The router cannot pop the mail and race its completion-decrement
//     past a main-thread `wait_idle` observing zero.
//   - The end-of-pipeline owner decrements `outstanding` via
//     `mark_completed`. For component-bound mail that's the per-
//     component dispatcher after `deliver` returns; for sink-bound
//     mail it's the router after the inline sink call. Dropped /
//     unknown recipients decrement from the router's warn-and-
//     discard branch.
//   - `wait_idle` blocks until the counter reaches zero. Safe to call
//     multiple times in sequence (the next frame's pushes re-raise it).
//   - `shutdown` is checked by the router before sleeping on an empty
//     queue. A scheduler drop sets it and broadcasts `pending_cv` to
//     wake the router so it can notice and exit.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};

use crate::mail::Mail;

pub struct MailQueue {
    pending: Mutex<VecDeque<Mail>>,
    pending_cv: Condvar,
    outstanding: Mutex<usize>,
    done_cv: Condvar,
    shutdown: AtomicBool,
}

impl MailQueue {
    pub fn new() -> Self {
        Self {
            pending: Mutex::new(VecDeque::new()),
            pending_cv: Condvar::new(),
            outstanding: Mutex::new(0),
            done_cv: Condvar::new(),
            shutdown: AtomicBool::new(false),
        }
    }

    /// Enqueue mail. Caller-thread synchronous. Wakes exactly one worker.
    pub fn push(&self, mail: Mail) {
        {
            let mut n = self.outstanding.lock().unwrap();
            *n += 1;
        }
        {
            let mut q = self.pending.lock().unwrap();
            q.push_back(mail);
        }
        self.pending_cv.notify_one();
    }

    /// Block until every mail enqueued so far has been processed. Used
    /// by the frame loop to wait for a frame to drain.
    pub fn wait_idle(&self) {
        let mut n = self.outstanding.lock().unwrap();
        while *n > 0 {
            n = self.done_cv.wait(n).unwrap();
        }
    }

    /// Test-only: non-blocking pop. Returns `None` immediately if the
    /// queue is empty rather than parking on `pending_cv`. Used by
    /// substrate tests that need to assert on queue state without
    /// running a router.
    #[cfg(test)]
    pub(crate) fn try_pop(&self) -> Option<Mail> {
        self.pending.lock().unwrap().pop_front()
    }

    /// FIFO pop. Parks on `pending_cv` when the queue is empty;
    /// returns `None` when `initiate_shutdown` is called so the router
    /// can exit cleanly.
    pub(crate) fn pop_blocking(&self) -> Option<Mail> {
        let mut q = self.pending.lock().unwrap();
        loop {
            if let Some(m) = q.pop_front() {
                return Some(m);
            }
            if self.shutdown.load(Ordering::SeqCst) {
                return None;
            }
            q = self.pending_cv.wait(q).unwrap();
        }
    }

    pub(crate) fn mark_completed(&self) {
        let mut n = self.outstanding.lock().unwrap();
        *n -= 1;
        if *n == 0 {
            self.done_cv.notify_all();
        }
    }

    pub(crate) fn initiate_shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        self.pending_cv.notify_all();
    }
}

impl Default for MailQueue {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread;

    use super::*;
    use crate::mail::{Mail, MailboxId};

    #[test]
    fn push_pop_roundtrip_and_idle() {
        let q = Arc::new(MailQueue::new());
        let qc = Arc::clone(&q);
        let worker = thread::spawn(move || {
            let m = qc.pop_blocking().expect("got mail");
            assert_eq!(m.recipient, MailboxId(3));
            qc.mark_completed();
        });
        q.push(Mail::new(MailboxId(3), 1, vec![], 0));
        q.wait_idle();
        worker.join().unwrap();
    }

    #[test]
    fn shutdown_unblocks_parked_worker() {
        let q = Arc::new(MailQueue::new());
        let qc = Arc::clone(&q);
        let worker = thread::spawn(move || qc.pop_blocking());
        // Give the worker a moment to park on the empty queue.
        thread::sleep(std::time::Duration::from_millis(20));
        q.initiate_shutdown();
        assert!(worker.join().unwrap().is_none());
    }
}
