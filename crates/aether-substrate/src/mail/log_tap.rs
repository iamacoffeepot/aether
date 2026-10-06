//! Spike (log-stream-scale): the log tap, held on the [`Mailer`](super::Mailer)
//! beside the cost table. Closed, it costs the dispatch loop one relaxed load.
//! Open, it runs one of two gathering designs so they can be measured against
//! each other on the same dispatch path:
//!
//! - **mail**: each handler's new lines are mailed to the sink actor as one
//!   `LogTailResult` with no lineage, so the mail is not counted by the
//!   settlement table.
//! - **mail, rooted**: the same slice on a fresh causal chain, which mints one
//!   settlement root per slice.
//! - **buffer**: each handler's new lines are copied into a bounded buffer
//!   here, drop-oldest with a count, which the sink drains once per tick. One
//!   shard is the single buffer; several shards give each worker thread its
//!   own lock.

use std::collections::VecDeque;
use std::mem;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use aether_actor::log::TapOpening;
use aether_data::MailboxId;
use aether_kinds::LogEntry;

/// The tap is closed: nothing leaves a ring.
pub const TAP_CLOSED: u8 = 0;
/// Lines leave by mail to the sink.
pub const TAP_MAIL: u8 = 1;
/// Lines leave into the bounded buffer.
pub const TAP_BUFFER: u8 = 2;
/// Lines leave by mail to the sink, each slice on a fresh causal chain.
pub const TAP_MAIL_ROOTED: u8 = 3;

const SHARDS: usize = 64;

#[repr(align(128))]
#[derive(Default)]
struct Shard(AtomicU64);

/// One line in the bounded buffer, with the actor that logged it.
pub struct TapLine {
    pub actor: MailboxId,
    pub entry: LogEntry,
}

#[derive(Default)]
struct TapBuffer {
    lines: VecDeque<TapLine>,
    cap: usize,
    pushed: u64,
    dropped: u64,
    ring_lost: u64,
}

/// The buffer's running counters, read under its lock.
#[derive(Clone, Copy, Default, Debug)]
pub struct TapCounters {
    /// Lines currently held.
    pub occupancy: usize,
    /// Lines ever pushed.
    pub pushed: u64,
    /// Lines evicted because the buffer was full.
    pub dropped: u64,
    /// Lines a ring evicted before the tap could take them.
    pub ring_lost: u64,
}

pub struct LogTap {
    mode: AtomicU8,
    backfill: AtomicBool,
    epoch: AtomicU64,
    opened_unix_millis: AtomicU64,
    sink: AtomicU64,
    slices_sent: [Shard; SHARDS],
    buffers: [Mutex<TapBuffer>; SHARDS],
    shards: AtomicU64,
}

impl Default for LogTap {
    fn default() -> Self {
        Self {
            mode: AtomicU8::new(TAP_CLOSED),
            backfill: AtomicBool::new(false),
            epoch: AtomicU64::new(0),
            opened_unix_millis: AtomicU64::new(0),
            sink: AtomicU64::new(0),
            slices_sent: std::array::from_fn(|_| Shard::default()),
            buffers: std::array::from_fn(|_| Mutex::new(TapBuffer::default())),
            shards: AtomicU64::new(1),
        }
    }
}

fn now_unix_millis() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(0))
}

fn shard_index() -> usize {
    use std::cell::Cell;
    static NEXT: AtomicU64 = AtomicU64::new(0);
    thread_local! {
        static INDEX: Cell<usize> = const { Cell::new(usize::MAX) };
    }
    INDEX.with(|index| {
        if index.get() == usize::MAX {
            index.set(usize::try_from(NEXT.fetch_add(1, Ordering::Relaxed)).unwrap_or(0) % SHARDS);
        }
        index.get()
    })
}

impl LogTap {
    /// The tap's mode: the one load every dispatched mail pays.
    #[inline]
    pub fn mode(&self) -> u8 {
        self.mode.load(Ordering::Relaxed)
    }

    pub fn sink(&self) -> MailboxId {
        MailboxId(self.sink.load(Ordering::Relaxed))
    }

    pub fn opening(&self) -> TapOpening {
        TapOpening {
            epoch: self.epoch.load(Ordering::Relaxed),
            backfill: self.backfill.load(Ordering::Relaxed),
            opened_unix_millis: self.opened_unix_millis.load(Ordering::Relaxed),
        }
    }

    /// Open the tap. `buffer_cap` is the buffer design's total line bound,
    /// split evenly over `shards` locks.
    pub fn open(&self, mode: u8, sink: MailboxId, backfill: bool, buffer_cap: usize, shards: usize) {
        let shards = shards.clamp(1, SHARDS);
        for buffer in &self.buffers[..shards] {
            let mut buffer = buffer.lock().unwrap_or_else(PoisonError::into_inner);
            buffer.cap = (buffer_cap / shards).max(1);
            buffer.lines = VecDeque::with_capacity(buffer.cap);
        }
        self.shards.store(shards as u64, Ordering::Relaxed);
        self.sink.store(sink.0, Ordering::Relaxed);
        self.backfill.store(backfill, Ordering::Relaxed);
        self.opened_unix_millis.store(now_unix_millis(), Ordering::Relaxed);
        self.epoch.fetch_add(1, Ordering::Relaxed);
        self.mode.store(mode, Ordering::Release);
    }

    pub fn close(&self) {
        self.mode.store(TAP_CLOSED, Ordering::Release);
    }

    /// Count one slice mailed to the sink (mail design), on this thread's shard.
    pub fn count_slice(&self) {
        self.slices_sent[shard_index()].0.fetch_add(1, Ordering::Relaxed);
    }

    /// Slices mailed to the sink since boot.
    pub fn slices_sent(&self) -> u64 {
        self.slices_sent.iter().map(|shard| shard.0.load(Ordering::Relaxed)).sum()
    }

    fn shards(&self) -> usize {
        usize::try_from(self.shards.load(Ordering::Relaxed)).unwrap_or(1)
    }

    /// Copy one handler's lines into this thread's shard of the bounded
    /// buffer (buffer design). A full shard evicts its oldest line and counts
    /// it; the evicted lines are freed after the lock is released.
    pub fn push(&self, actor: MailboxId, entries: Vec<LogEntry>, ring_lost: u64) {
        let mut evicted = Vec::new();
        {
            let shard = &self.buffers[shard_index() % self.shards()];
            let mut buffer = shard.lock().unwrap_or_else(PoisonError::into_inner);
            buffer.ring_lost += ring_lost;
            for entry in entries {
                if buffer.lines.len() >= buffer.cap {
                    evicted.extend(buffer.lines.pop_front());
                    buffer.dropped += 1;
                }
                buffer.lines.push_back(TapLine { actor, entry });
                buffer.pushed += 1;
            }
        }
        drop(evicted);
    }

    /// Move every buffered line into `out`, which the caller emptied, and
    /// report the counters as they stood. One shard is swapped whole; several
    /// are appended in turn, so `out` is ordered within a shard only.
    pub fn drain(&self, out: &mut VecDeque<TapLine>) -> TapCounters {
        let shards = self.shards();
        let mut counters = TapCounters::default();
        for shard in &self.buffers[..shards] {
            let mut buffer = shard.lock().unwrap_or_else(PoisonError::into_inner);
            counters.occupancy += buffer.lines.len();
            counters.pushed += buffer.pushed;
            counters.dropped += buffer.dropped;
            counters.ring_lost += buffer.ring_lost;
            if shards == 1 {
                mem::swap(&mut buffer.lines, out);
            } else {
                out.append(&mut buffer.lines);
            }
        }
        counters
    }

    /// The buffer's counters without draining it.
    pub fn counters(&self) -> TapCounters {
        let mut counters = TapCounters::default();
        for shard in &self.buffers[..self.shards()] {
            let buffer = shard.lock().unwrap_or_else(PoisonError::into_inner);
            counters.occupancy += buffer.lines.len();
            counters.pushed += buffer.pushed;
            counters.dropped += buffer.dropped;
            counters.ring_lost += buffer.ring_lost;
        }
        counters
    }
}
