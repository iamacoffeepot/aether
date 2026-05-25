//! ADR-0086 Phase 3 per-actor trace storage. Every actor — native or
//! wasm trampoline — owns an [`ActorTraceRing`] in its
//! [`crate::local::ActorSlots`], the trace-side sibling of ADR-0081's
//! [`crate::log::ActorLogRing`]. The producer hooks
//! (`record_sent` / `record_received` / `record_finished`) push the
//! mail-graph events for the current actor into its ring; a coordinator
//! reconstructs a trace tree on demand by fanning out
//! [`aether_kinds::trace::TraceTail`] across live actors and stitching
//! the per-ring slices by lineage keys.
//!
//! The ring stores only the mail-graph events — `Sent` lands in the
//! sender's ring, `Received` / `Finished` in the recipient's. Each entry
//! is tagged with its causal `root` (the producer hook has it at push
//! time, even for `Received` / `Finished` whose wire variants don't
//! carry it), so a root-filtered [`TraceTail`] resolves a single tree's
//! events from a ring without the central observer's by-mail join.
//! Settlement holds (`HoldOpen` / `Release`) are *not* stored: they
//! aren't tree nodes and settlement no longer rides the trace stream
//! (ADR-0086 Phase 2 — the emit-time counter owns them).
//!
//! Single-writer: the actor's dispatcher thread is the sole producer
//! (one OS thread per actor at a time), so the `sequence` counter and
//! the `VecDeque` need no lock internally. Cross-thread reads run
//! through the `Local` path on the responding actor's dispatcher (the
//! framework-built-in `aether.trace.tail` arm invokes [`ActorTraceRing::tail`]
//! from inside the dispatch loop). Off-actor `Sent`s (chassis-root /
//! injected mail produced outside any actor's dispatch) have no
//! `ActorSlots` to land in; the chassis trace handle keeps a separate
//! locked chassis-host ring for those (ADR-0086 Phase 3 §B).

extern crate alloc;

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use aether_data::MailId;
use aether_kinds::trace::{TraceEvent, TraceRingEntry, TraceTail, TraceTailResult};

use crate::Local;

/// Default per-actor trace-ring capacity. Larger than the log ring's
/// 1024 because a busy actor emits up to three trace events per mail
/// (`Sent` on send, `Received` + `Finished` on dispatch). 4096 keeps
/// the worst-case memory near 4096 × ~64 B = ~256 KiB per actor.
pub const DEFAULT_TRACE_RING_CAP: usize = 4096;

/// Substrate-default cap on entries returned per [`TraceTail`] when the
/// caller passes `max == 0`. Mirrors the log ring's tail default.
pub const DEFAULT_TAIL_MAX: u32 = 256;

/// Absolute ceiling on entries returned per [`TraceTail`]; caller
/// `max` above this clamps down. The ring capacity is the natural
/// upper bound — one ring can never reply with more than it holds.
pub const MAX_TAIL_MAX: u32 = 4096;

/// Per-actor bounded ring of trace events (ADR-0086 Phase 3). The
/// trace-side sibling of [`crate::log::ActorLogRing`]: same
/// `(ring, ring_cap, sequence)` triple, same FIFO drop-oldest eviction,
/// same `truncated_before` gap cursor. Entries carry their causal
/// `root` so a root-filtered [`Self::tail`] resolves one tree's events
/// directly.
pub struct ActorTraceRing {
    ring: VecDeque<TraceRingEntry>,
    ring_cap: usize,
    /// Monotonic per-ring sequence; starts at 1. Persists across
    /// eviction so a caller's `since` cursor stays meaningful (it
    /// resolves into a `truncated_before` gap signal rather than
    /// silently re-pulling).
    sequence: u64,
}

impl Default for ActorTraceRing {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_TRACE_RING_CAP)
    }
}

impl Local for ActorTraceRing {}

impl ActorTraceRing {
    /// Build an empty ring with the supplied capacity, clamped to
    /// `max(1, _)` — a zero-cap ring would drop every entry.
    #[must_use]
    pub fn with_capacity(ring_cap: usize) -> Self {
        let ring_cap = ring_cap.max(1);
        Self {
            ring: VecDeque::with_capacity(ring_cap),
            ring_cap,
            sequence: 1,
        }
    }

    /// Push one trace event tagged with its causal `root`, stamping the
    /// next-available `sequence`. Evicts the oldest entry when the ring
    /// is at cap.
    pub fn push(&mut self, root: MailId, event: TraceEvent) {
        let sequence = self.sequence;
        self.sequence += 1;
        if self.ring.len() == self.ring_cap {
            self.ring.pop_front();
        }
        self.ring.push_back(TraceRingEntry {
            sequence,
            root,
            event,
        });
    }

    /// Pure read-side: filter on `since` (and `root`, when set), cap at
    /// `max` (with `0 → DEFAULT_TAIL_MAX` and `> MAX_TAIL_MAX → ceiling`
    /// clamping), compute the `truncated_before` cursor. Ordered
    /// oldest-to-newest.
    #[must_use]
    pub fn tail(&self, request: &TraceTail) -> TraceTailResult {
        let max = resolve_max(request.max) as usize;
        let since = request.since.unwrap_or(0);

        let earliest = self.ring.front().map(|e| e.sequence);
        let truncated_before = match earliest {
            Some(e) if e > since + 1 => Some(e),
            _ => None,
        };

        let entries: Vec<TraceRingEntry> = self
            .ring
            .iter()
            .filter(|e| e.sequence > since)
            .filter(|e| request.root.is_none_or(|r| e.root == r))
            .take(max)
            .cloned()
            .collect();

        let next_since = entries.last().map_or(since, |e| e.sequence);

        TraceTailResult::Ok {
            entries,
            next_since,
            truncated_before,
        }
    }

    /// Snapshot every entry currently in the ring, oldest-to-newest, no
    /// filter. For the panic-hook dump path and tests.
    #[must_use]
    pub fn snapshot(&self) -> Vec<TraceRingEntry> {
        self.ring.iter().cloned().collect()
    }

    /// Number of entries currently in the ring.
    #[must_use]
    pub fn len(&self) -> usize {
        self.ring.len()
    }

    /// Is the ring empty?
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ring.is_empty()
    }
}

/// Clamp `max == 0` to [`DEFAULT_TAIL_MAX`] and any value over
/// [`MAX_TAIL_MAX`] down to the ceiling.
fn resolve_max(max: u32) -> u32 {
    if max == 0 {
        DEFAULT_TAIL_MAX
    } else {
        max.min(MAX_TAIL_MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_data::MailboxId;
    use aether_kinds::trace::Nanos;

    fn mid(sender: u64, cid: u64) -> MailId {
        MailId {
            sender: MailboxId(sender),
            correlation_id: cid,
        }
    }

    fn sent(root: MailId) -> TraceEvent {
        TraceEvent::Sent {
            mail_id: root,
            root,
            parent_mail: None,
            sender: MailboxId(1),
            recipient: MailboxId(2),
            kind: aether_data::KindId(3),
            t_construct_start: Nanos(0),
            t: Nanos(0),
        }
    }

    fn finished(mail_id: MailId) -> TraceEvent {
        TraceEvent::Finished {
            mail_id,
            t: Nanos(0),
        }
    }

    fn ok(result: TraceTailResult) -> (Vec<TraceRingEntry>, u64, Option<u64>) {
        match result {
            TraceTailResult::Ok {
                entries,
                next_since,
                truncated_before,
            } => (entries, next_since, truncated_before),
            TraceTailResult::Err { error } => panic!("expected Ok, got Err: {error}"),
        }
    }

    fn unfiltered() -> TraceTail {
        TraceTail {
            max: 0,
            since: None,
            root: None,
        }
    }

    #[test]
    fn tail_returns_entries_in_push_order_with_monotonic_sequence() {
        let mut ring = ActorTraceRing::with_capacity(8);
        let r = mid(1, 1);
        ring.push(r, sent(r));
        ring.push(r, finished(r));
        let (entries, next_since, truncated_before) = ok(ring.tail(&unfiltered()));
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].sequence, 1);
        assert_eq!(entries[1].sequence, 2);
        assert_eq!(next_since, 2);
        assert_eq!(truncated_before, None);
    }

    #[test]
    fn tail_filters_by_root() {
        let mut ring = ActorTraceRing::with_capacity(8);
        let a = mid(1, 1);
        let b = mid(1, 2);
        ring.push(a, sent(a));
        ring.push(b, sent(b));
        ring.push(a, finished(a));
        let (entries, ..) = ok(ring.tail(&TraceTail {
            max: 0,
            since: None,
            root: Some(a),
        }));
        assert_eq!(entries.len(), 2, "only root a's two events");
        assert!(entries.iter().all(|e| e.root == a));
    }

    #[test]
    fn tail_since_cursor_paginates_without_double_pulling() {
        let mut ring = ActorTraceRing::with_capacity(8);
        let r = mid(1, 1);
        for _ in 0..5 {
            ring.push(r, sent(r));
        }
        let (entries, next_since, _) = ok(ring.tail(&TraceTail {
            max: 0,
            since: Some(2),
            root: None,
        }));
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].sequence, 3);
        assert_eq!(next_since, 5);

        let (entries, next_since, _) = ok(ring.tail(&TraceTail {
            max: 0,
            since: Some(next_since),
            root: None,
        }));
        assert!(entries.is_empty());
        assert_eq!(next_since, 5);
    }

    #[test]
    fn tail_max_caps_returned_slice() {
        let mut ring = ActorTraceRing::with_capacity(8);
        let r = mid(1, 1);
        for _ in 0..5 {
            ring.push(r, sent(r));
        }
        let (entries, next_since, _) = ok(ring.tail(&TraceTail {
            max: 2,
            since: None,
            root: None,
        }));
        assert_eq!(entries.len(), 2);
        assert_eq!(next_since, 2);
    }

    #[test]
    fn tail_signals_truncated_before_when_ring_evicted_prefix() {
        let mut ring = ActorTraceRing::with_capacity(3);
        let r = mid(1, 1);
        for _ in 0..5 {
            ring.push(r, sent(r));
        }
        let (entries, _, truncated_before) = ok(ring.tail(&unfiltered()));
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].sequence, 3);
        assert_eq!(truncated_before, Some(3));
    }

    #[test]
    fn resolve_max_clamps_to_documented_bounds() {
        assert_eq!(resolve_max(0), DEFAULT_TAIL_MAX);
        assert_eq!(resolve_max(50), 50);
        assert_eq!(resolve_max(MAX_TAIL_MAX), MAX_TAIL_MAX);
        assert_eq!(resolve_max(MAX_TAIL_MAX + 5_000), MAX_TAIL_MAX);
    }

    #[test]
    fn zero_cap_is_clamped_to_one() {
        let mut ring = ActorTraceRing::with_capacity(0);
        let r = mid(1, 1);
        ring.push(r, sent(r));
        assert_eq!(ring.len(), 1);
    }
}
