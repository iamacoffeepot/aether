// Shared mail queue + frame barrier. Held by Arc so workers, host-function
// contexts, and the scheduler owner all share one instance.
//
// Invariants:
//   - `push` increments `outstanding` BEFORE pushing into the deque. A
//     worker cannot pop the mail and race its completion-decrement past
//     a main-thread `wait_idle` observing zero.
//   - Workers decrement after processing. When the counter hits zero they
//     signal `done_cv`.
//   - `wait_idle` blocks until the counter reaches zero. Safe to call
//     multiple times in sequence (the next frame's pushes re-raise it).
//   - `shutdown` is checked by workers before sleeping on an empty queue.
//     A scheduler drop sets it and broadcasts `pending_cv` to wake parked
//     workers so they can notice and exit.

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
    /// running a worker.
    #[cfg(test)]
    pub(crate) fn try_pop(&self) -> Option<Mail> {
        self.pending.lock().unwrap().pop_front()
    }

    /// Test helper — unconditional FIFO pop. Production workers use
    /// `pop_blocking_if` so the per-mailbox strand claim can veto a
    /// pop; tests that don't exercise strands keep the simple path.
    #[cfg(test)]
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

    /// Block until a mail for which `pred` returns `true` exists at
    /// some position in the queue, then pop it. The scheduler's
    /// per-mailbox strand uses this: `pred` atomically tries to claim
    /// the strand for the mail's recipient, returning `true` (pop me)
    /// iff it succeeded and `false` (skip me, try later mails)
    /// otherwise. `pred` is called left-to-right across the queue each
    /// scan and may short-circuit as soon as one mail returns `true`.
    ///
    /// `pred` is permitted to have atomic side effects (it must, to
    /// reserve the strand). The contract: if `pred` returns `true`,
    /// the caller guarantees to dispatch that mail; no "uncommit" path
    /// is provided, so `pred` must not reserve on anything other than
    /// the mail it's about to green-light.
    pub(crate) fn pop_blocking_if<F>(&self, mut pred: F) -> Option<Mail>
    where
        F: FnMut(&Mail) -> bool,
    {
        let mut q = self.pending.lock().unwrap();
        loop {
            if let Some(idx) = q.iter().position(&mut pred) {
                return q.remove(idx);
            }
            if self.shutdown.load(Ordering::SeqCst) {
                return None;
            }
            q = self.pending_cv.wait(q).unwrap();
        }
    }

    /// Non-blocking: pop the first mail whose recipient matches.
    /// Returns `None` immediately if no matching mail is queued. Used
    /// by the strand owner to drain every remaining mail for its
    /// recipient before releasing the strand.
    pub(crate) fn try_pop_for_recipient(&self, recipient: crate::mail::MailboxId) -> Option<Mail> {
        let mut q = self.pending.lock().unwrap();
        let idx = q.iter().position(|m| m.recipient == recipient)?;
        q.remove(idx)
    }

    /// Wake every worker parked on `pending_cv`. Called when a strand
    /// releases so workers that previously skipped mail for that
    /// recipient can re-scan. `notify_one` on push is not enough on
    /// its own: a release can happen without a push (the strand owner
    /// drains the last pending mail for its recipient and finds the
    /// queue contains only skipped mails that are now unblocked).
    pub(crate) fn notify_waiters(&self) {
        self.pending_cv.notify_all();
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
