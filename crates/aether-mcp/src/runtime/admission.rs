//! Admission: the global rate bucket, the in-flight permit pool, and the
//! deadline timer that expires a dispatched call.
//!
//! All three are *global* rather than per-caller, because the stateless profile
//! has no caller to key on — no session, no retained client identity. That is
//! the honest bound available here; a per-caller limit would need a coordinator
//! this design deliberately does not have.
//!
//! A refusal is a real answer, not a dropped request: a rate-refused call gets
//! `-32000` with `retryAfterMillis`, so a client backs off by a number the
//! server chose rather than by guessing.

use std::cmp::{Ordering as SortOrder, Reverse};
use std::collections::BinaryHeap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use aether_data::{Kind, KindId, MailboxId};
use aether_substrate::Mail;
use aether_substrate::mail::mailer::Mailer;

use crate::kinds::RequestDeadlineElapsed;

/// Milliseconds in one minute — the rate's declared period.
const MILLIS_PER_MINUTE: u64 = 60_000;

/// A global token bucket over accepted messages.
///
/// Refill is continuous rather than per-tick, so a burst that arrives one
/// millisecond after the bucket empties waits one millisecond's worth of
/// tokens instead of a whole period.
#[derive(Debug)]
pub struct RateLimiter {
    tokens: f64,
    burst: f64,
    tokens_per_milli: f64,
    last_millis: u64,
}

/// The declared period as a rate divisor.
fn millis_per_minute() -> f64 {
    // Exact in binary floating point, so the conversion introduces no rounding
    // the refill arithmetic would then carry.
    #[allow(clippy::cast_precision_loss)] // aether-suppression-request: exact-in-binary constant conversion
    let millis = MILLIS_PER_MINUTE as f64;
    millis
}

impl RateLimiter {
    #[must_use]
    pub fn new(requests_per_minute: u32, burst: u32, now_millis: u64) -> Self {
        Self {
            tokens: f64::from(burst),
            burst: f64::from(burst),
            tokens_per_milli: f64::from(requests_per_minute) / millis_per_minute(),
            last_millis: now_millis,
        }
    }

    /// Spend one token, or report how long until the next one exists.
    pub fn admit(&mut self, now_millis: u64) -> Result<(), u64> {
        let elapsed = now_millis.saturating_sub(self.last_millis);
        self.last_millis = now_millis;
        #[allow(clippy::cast_precision_loss)] // aether-suppression-request: refill; loss starts past 2^53 millis
        let refill = elapsed as f64 * self.tokens_per_milli;
        self.tokens = (self.tokens + refill).min(self.burst);

        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            return Ok(());
        }

        // A configured rate of zero can never produce a token, so the honest
        // hint is the whole period rather than an infinite wait rendered as a
        // nonsensical number.
        if self.tokens_per_milli <= 0.0 {
            return Err(MILLIS_PER_MINUTE);
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // aether-suppression-request: bounded ceil
        Err((((1.0 - self.tokens) / self.tokens_per_milli).ceil() as u64).max(1))
    }
}

/// The concurrent-dispatch pool.
///
/// Immediate lifecycle and list responses never take a permit — they hold no
/// downstream work — so the pool bounds exactly what it is named for.
#[derive(Debug)]
pub struct InFlightPool {
    held: usize,
    maximum: usize,
}

impl InFlightPool {
    #[must_use]
    pub fn new(maximum: usize) -> Self {
        Self { held: 0, maximum }
    }

    /// Take a permit, or refuse. A refusal takes nothing, which is what stops a
    /// rejected request from consuming a permit forever.
    pub fn acquire(&mut self) -> bool {
        if self.held >= self.maximum {
            return false;
        }
        self.held += 1;
        true
    }

    pub fn release(&mut self) {
        self.held = self.held.saturating_sub(1);
    }

    #[must_use]
    pub fn held(&self) -> usize {
        self.held
    }
}

/// One armed deadline.
#[derive(Debug, PartialEq, Eq)]
struct Deadline {
    at_millis: u64,
    correlation_id: u64,
    generation: u64,
}

impl Ord for Deadline {
    fn cmp(&self, other: &Self) -> SortOrder {
        self.at_millis.cmp(&other.at_millis).then(self.correlation_id.cmp(&other.correlation_id))
    }
}

impl PartialOrd for Deadline {
    fn partial_cmp(&self, other: &Self) -> Option<SortOrder> {
        Some(self.cmp(other))
    }
}

/// The heap the timer thread sleeps on.
#[derive(Default)]
struct DeadlineQueue {
    heap: BinaryHeap<Reverse<Deadline>>,
}

/// One thread, one heap, for every deferred operation.
///
/// A thread or task per request would make the concurrency bound the operating
/// system's rather than the configured one, and each would hold its own copy of
/// the correlation it is waiting on. One heap keeps the cost proportional to
/// armed deadlines and keeps expiry in the same order the deadlines were set.
///
/// The thread never touches actor state. It posts [`RequestDeadlineElapsed`] as
/// ordinary mail, so expiry runs inside the actor's own serialized dispatch —
/// the only place the pending table may be mutated.
pub struct DeadlineTimer {
    queue: Arc<(Mutex<DeadlineQueue>, Condvar)>,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl DeadlineTimer {
    /// Start the timer for a capability at `self_id`.
    ///
    /// `epoch` is the monotonic base every deadline in this capability is
    /// measured from, shared with the actor so the two sides cannot disagree
    /// about what "now" means.
    pub fn start(mailer: Arc<Mailer>, self_id: MailboxId, epoch: Instant) -> Self {
        let queue = Arc::new((Mutex::new(DeadlineQueue::default()), Condvar::new()));
        let shutdown = Arc::new(AtomicBool::new(false));

        let queue_for_thread = Arc::clone(&queue);
        let shutdown_for_thread = Arc::clone(&shutdown);

        // A timer below the mail layer: it carries wakes *in*, so it has no
        // inbound chain to inherit and no settlement umbrella to join.
        #[allow(clippy::disallowed_methods)] // aether-suppression-request: deadline timer below the mail layer
        let thread = thread::Builder::new()
            .name("aether-mcp-deadlines".to_string())
            .spawn(move || run_timer(&queue_for_thread, &shutdown_for_thread, &mailer, self_id, epoch))
            .ok();

        Self { queue, shutdown, thread }
    }

    /// Arm one deadline.
    ///
    /// # Panics
    /// Panics if the deadline heap's lock is poisoned — fail-fast per ADR-0063,
    /// since a poisoned heap means the timer thread already panicked and no
    /// deadline after this one would ever fire.
    pub fn arm(&self, correlation_id: u64, generation: u64, at_millis: u64) {
        let (mutex, condvar) = &*self.queue;
        mutex.lock().expect("deadline heap lock poisoned").heap.push(Reverse(Deadline {
            at_millis,
            correlation_id,
            generation,
        }));
        condvar.notify_one();
    }
}

impl Drop for DeadlineTimer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        self.queue.1.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The timer thread body.
///
/// A fired deadline is *not* removed from the pending table here — the thread
/// posts mail and forgets. A deadline whose operation already completed simply
/// finds no matching generation when the actor handles it, which is why the
/// heap needs no cancellation path and no lock is ever held across a send.
fn run_timer(
    queue: &Arc<(Mutex<DeadlineQueue>, Condvar)>,
    shutdown: &AtomicBool,
    mailer: &Arc<Mailer>,
    self_id: MailboxId,
    epoch: Instant,
) {
    let (mutex, condvar) = &**queue;

    while !shutdown.load(Ordering::Acquire) {
        let due = {
            let mut pending = mutex.lock().expect("deadline heap lock poisoned");
            let now = elapsed_millis(epoch);
            match pending.heap.peek() {
                None => {
                    let _unused = condvar.wait(pending).expect("deadline heap lock poisoned");
                    continue;
                }
                Some(Reverse(earliest)) if earliest.at_millis > now => {
                    let wait = Duration::from_millis(earliest.at_millis - now);
                    let _unused = condvar.wait_timeout(pending, wait).expect("deadline heap lock poisoned");
                    continue;
                }
                Some(_) => pending.heap.pop(),
            }
        };

        // The lock is released before the send: a deadline firing must never
        // hold the heap against the actor arming the next one.
        if let Some(Reverse(due)) = due {
            mailer.push(Mail::new(
                self_id,
                KindId(<RequestDeadlineElapsed as Kind>::ID.0),
                RequestDeadlineElapsed { correlation_id: due.correlation_id, generation: due.generation }
                    .encode_into_bytes(),
                1,
            ));
        }
    }
}

/// Milliseconds since the capability's monotonic epoch.
///
/// Monotonic rather than wall clock: a lifetime or a deadline that a clock
/// adjustment could shorten would expire a live tool call or a live address for
/// a reason no log would explain.
#[must_use]
pub fn elapsed_millis(epoch: Instant) -> u64 {
    u64::try_from(epoch.elapsed().as_millis()).unwrap_or(u64::MAX)
}
