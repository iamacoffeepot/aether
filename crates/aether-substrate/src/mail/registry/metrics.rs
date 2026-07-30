// Drain accounting for the two ADR-0165 serialized queues — the registry
// owner and the route relay. Both are single-drainer slots on the shared
// worker pool, so their throughput ceiling is a property of one drainer and
// is only knowable from measurement.
//
// ADR-0165 §Consequences names its own sharding trigger as "sustained churn
// exceeds approximately 5% of the measured single-owner ceiling". That is two
// numbers, and this meter is where both come from: the *ceiling* is `drained`
// over `busy_nanos` (items the drainer retires per nanosecond it is actually
// running), and the *churn* is `drained` over wall time. Their ratio —
// equivalently `busy_nanos` over elapsed wall nanos, which is
// [`RegistryQueueMetrics::duty_cycle`] — is what the 5% threshold reads
// against. `depth_max` and `drain_max` say whether the load arrives smoothly
// or in bursts, which decides whether the answer is a larger bound or a
// second shard.
//
// Counters are plain relaxed atomics. Each is a monotone tally read for
// trend, never for a decision, so no ordering between them is load-bearing;
// the admission decision itself reads the queue's own length under the queue
// lock and never consults this meter.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Slots a queue preallocates at attach. The configured capacity is a
/// saturation bound, not an expected depth — a healthy queue sits near empty
/// — so reserving the whole bound up front would trade megabytes of resident
/// memory for an allocation that only a saturating engine ever needs. The
/// `VecDeque` grows on its own past this.
pub(super) const INITIAL_QUEUE_RESERVE: usize = 64;

/// One serialized queue's admission and drain accounting, in items.
///
/// Read through
/// [`Registry::owner_queue_metrics`](super::Registry::owner_queue_metrics) or
/// [`Mailer::route_relay_metrics`](crate::mail::mailer::Mailer::route_relay_metrics).
/// Every field is a monotone tally since attach except `capacity` (the
/// configured bound) and `depth` (the queue length at the moment of the read).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RegistryQueueMetrics {
    /// The configured admission bound
    /// ([`RegistryQueueCapacities`](crate::config::RegistryQueueCapacities)).
    pub capacity: u64,
    /// Queue length at the moment of the read.
    pub depth: u64,
    /// Deepest the queue has ever been.
    pub depth_max: u64,
    /// Items accepted onto the queue.
    pub admitted: u64,
    /// Items the bound refused. Only the owner queue carries a sheddable
    /// class (an ordinary route-view miss), so the relay's `shed` stays zero
    /// by construction — every continuation it holds is work the owner
    /// already committed to delivering.
    pub shed: u64,
    /// Items accepted while the queue already stood at or past `capacity` —
    /// the reserved, correctness-bearing class the bound never refuses. A
    /// rising count means the bound is held open by traffic that cannot be
    /// shed, which is the signal that wants a shard rather than a larger
    /// number.
    pub over_capacity: u64,
    /// Items removed by a drain cycle.
    pub drained: u64,
    /// Drain cycles that moved at least one item.
    pub drains: u64,
    /// Largest single drain.
    pub drain_max: u64,
    /// Total time spent inside a drain, in nanoseconds. `drained` over this
    /// is the measured single-drainer ceiling; this over elapsed wall time is
    /// the duty cycle ADR-0165's 5% sharding trigger reads.
    pub busy_nanos: u64,
}

impl RegistryQueueMetrics {
    /// Fraction of `elapsed` the drainer spent inside a drain. ADR-0165's
    /// sharding trigger is this exceeding roughly `0.05` in sustained
    /// operation. `None` for a zero-length window.
    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        reason = "a duty cycle is a trend ratio; f64 is exact past any nanosecond count a run accumulates"
    )]
    pub fn duty_cycle(&self, elapsed: Duration) -> Option<f64> {
        let elapsed = u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX);
        (elapsed > 0).then(|| self.busy_nanos as f64 / elapsed as f64)
    }
}

/// The live counters behind [`RegistryQueueMetrics`]. Shared by `Arc` between
/// a queue's submission handle and its drain slot.
#[derive(Debug, Default)]
pub(super) struct QueueMeter {
    capacity: u64,
    depth: AtomicU64,
    depth_max: AtomicU64,
    admitted: AtomicU64,
    shed: AtomicU64,
    over_capacity: AtomicU64,
    drained: AtomicU64,
    drains: AtomicU64,
    drain_max: AtomicU64,
    busy_nanos: AtomicU64,
}

impl QueueMeter {
    pub(super) fn new(capacity: usize) -> Self {
        Self { capacity: u64::try_from(capacity).unwrap_or(u64::MAX), ..Self::default() }
    }

    /// Record an accepted item. `depth` is the queue length *after* the push,
    /// read under the queue lock by the caller.
    pub(super) fn admit(&self, depth: usize) {
        let depth = u64::try_from(depth).unwrap_or(u64::MAX);
        self.admitted.fetch_add(1, Ordering::Relaxed);
        self.depth.store(depth, Ordering::Relaxed);
        self.depth_max.fetch_max(depth, Ordering::Relaxed);
        if depth > self.capacity {
            self.over_capacity.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Record an item the bound refused.
    pub(super) fn shed(&self) {
        self.shed.fetch_add(1, Ordering::Relaxed);
    }

    /// Record that a drainer emptied its queue while holding the queue's
    /// admission lock. A producer admitted after that lock is released writes
    /// the new nonzero depth, and [`Self::drain`] deliberately does not erase
    /// it when the retired prefix finishes applying.
    pub(super) fn drained_to_empty(&self) {
        self.depth.store(0, Ordering::Relaxed);
    }

    /// Record one drain cycle: `count` items removed while the drainer held
    /// its serialization for `busy`.
    pub(super) fn drain(&self, count: usize, busy: Duration) {
        self.busy_nanos.fetch_add(u64::try_from(busy.as_nanos()).unwrap_or(u64::MAX), Ordering::Relaxed);
        if count == 0 {
            return;
        }
        let count = u64::try_from(count).unwrap_or(u64::MAX);
        self.drained.fetch_add(count, Ordering::Relaxed);
        self.drains.fetch_add(1, Ordering::Relaxed);
        self.drain_max.fetch_max(count, Ordering::Relaxed);
    }

    pub(super) fn snapshot(&self) -> RegistryQueueMetrics {
        let read = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        RegistryQueueMetrics {
            capacity: self.capacity,
            depth: read(&self.depth),
            depth_max: read(&self.depth_max),
            admitted: read(&self.admitted),
            shed: read(&self.shed),
            over_capacity: read(&self.over_capacity),
            drained: read(&self.drained),
            drains: read(&self.drains),
            drain_max: read(&self.drain_max),
            busy_nanos: read(&self.busy_nanos),
        }
    }
}
