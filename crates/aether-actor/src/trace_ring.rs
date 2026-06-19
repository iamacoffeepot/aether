//! ADR-0086 Phase 3 per-actor trace storage. Every actor — native or
//! wasm trampoline — owns an [`ActorTraceRing`] in its
//! [`crate::local::ActorSlots`], the trace-side sibling of ADR-0081's
//! [`crate::log::ActorLogRing`]. The producer hooks
//! (`record_sent` / `record_received` / `record_finished`) push the
//! mail-graph events for the current actor into its ring; a coordinator
//! reconstructs a trace tree on demand by fanning out
//! [`TraceTail`] across live actors and stitching
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

/// Default ceiling the per-actor trace ring may grow to before it falls
/// back to drop-oldest (the floor is [`DEFAULT_TRACE_RING_CAP`]). 16× the
/// floor — a busy actor that bursts can reach ~65536 × ~64 B = ~4 MiB,
/// while a quiet one stays at the 256 KiB floor (it never grows). Growth
/// is geometric (see [`ActorTraceRing::push`]), so the climb costs only
/// ~`log2(max/floor)` reallocations.
pub const DEFAULT_TRACE_RING_MAX_CAP: usize = 65536;

/// Substrate-default cap on entries returned per [`TraceTail`] when the
/// caller passes `max == 0`. Mirrors the log ring's tail default.
pub const DEFAULT_TAIL_MAX: u32 = 256;

/// Absolute ceiling on entries returned per [`TraceTail`]; caller
/// `max` above this clamps down. The ring capacity is the natural
/// upper bound — one ring can never reply with more than it holds.
pub const MAX_TAIL_MAX: u32 = 4096;

/// Per-actor bounded ring of trace events (ADR-0086 Phase 3). The
/// trace-side sibling of [`crate::log::ActorLogRing`], with one
/// difference: a saturating trace ring grows toward `max_cap` rather than
/// immediately dropping its oldest entry, because each evicted entry is a
/// node the trace coordinator needs to stitch a causal tree — a mid-chain
/// hole breaks the whole reconstruction, unlike the log ring's
/// independent recency lines. `cap` is the current (growing) threshold; it
/// starts at the floor and doubles toward `max_cap`, but only to protect a
/// still-in-flight oldest chain — a full ring whose oldest chain has
/// settled reclaims that entry instead of growing (the settlement-aware
/// choice in [`Self::push`]), so the ring tracks live in-flight pressure
/// rather than total volume. When `cap == max_cap` it resumes plain FIFO
/// drop-oldest. A fixed-capacity ring is `cap == max_cap` from the start
/// ([`Self::with_capacity`]). Entries carry their causal `root` so a
/// root-filtered [`Self::tail`] resolves one tree's events directly, and
/// the `truncated_before` gap cursor still fires for any evicted prefix.
pub struct ActorTraceRing {
    ring: VecDeque<TraceRingEntry>,
    /// Current eviction threshold — the length at which the next push
    /// either grows the ring (`cap < max_cap`) or evicts the oldest
    /// entry (`cap == max_cap`). Starts at the floor and doubles toward
    /// `max_cap`.
    cap: usize,
    /// Ceiling `cap` grows to; past it the ring drops-oldest. Equals the
    /// floor for a fixed-capacity ring.
    max_cap: usize,
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
    /// Build an empty fixed-capacity ring: it never grows, evicting the
    /// oldest entry once `ring_cap` entries are present. Clamped to
    /// `max(1, _)` — a zero-cap ring would drop every entry. Equivalent
    /// to [`Self::with_growth(ring_cap, ring_cap)`](Self::with_growth).
    #[must_use]
    pub fn with_capacity(ring_cap: usize) -> Self {
        Self::with_growth(ring_cap, ring_cap)
    }

    /// Build an empty ring that starts at `floor` and grows toward `max`
    /// before it begins dropping oldest entries (see [`Self::push`]).
    /// `floor` is clamped to `max(1, _)` (a zero-floor ring would drop
    /// every entry); `max` is clamped up to `floor`, so a misconfigured
    /// `max < floor` degrades to a fixed ring at the floor rather than
    /// one asked to hold fewer than it started with. The floor is
    /// preallocated; the growth steps reallocate geometrically.
    #[must_use]
    pub fn with_growth(floor: usize, max: usize) -> Self {
        let floor = floor.max(1);
        let max_cap = max.max(floor);
        Self {
            ring: VecDeque::with_capacity(floor),
            cap: floor,
            max_cap,
            sequence: 1,
        }
    }

    /// Push one trace event tagged with its causal `root`, stamping the
    /// next-available `sequence`.
    ///
    /// When the ring is full, it either grows or evicts its oldest entry,
    /// and the choice is settlement-aware (issue 2076). The ring grows —
    /// doubling its threshold toward `max_cap` — **only to protect a
    /// still-in-flight oldest chain**: `front_still_live` is called with
    /// the oldest entry's `root`, and a `true` answer (the chain hasn't
    /// settled, so dropping its oldest event would punch a hole in a tree
    /// still being built) is what triggers growth. A `false` answer (the
    /// oldest chain has settled — its tree is complete, the least-valuable
    /// entry to keep) reclaims that entry instead, so a ring full of
    /// settled events stays at its current size rather than growing to
    /// hoard them. At `max_cap`, or with no growth headroom, the ring
    /// always evicts (FIFO drop-oldest) without consulting the predicate.
    ///
    /// `VecDeque::push_back` reallocates the backing buffer geometrically
    /// in lockstep with the threshold, so a full climb from floor to
    /// ceiling costs only ~`log2(max_cap/floor)` reallocations. The
    /// predicate is invoked at most once per push, and only on a
    /// growable full ring.
    pub fn push(
        &mut self,
        root: MailId,
        event: TraceEvent,
        front_still_live: impl FnOnce(MailId) -> bool,
    ) {
        let sequence = self.sequence;
        self.sequence += 1;
        if self.ring.len() == self.cap {
            let grow = self.cap < self.max_cap
                && self.ring.front().is_some_and(|e| front_still_live(e.root));
            if grow {
                self.cap = self.cap.saturating_mul(2).min(self.max_cap);
            } else {
                self.ring.pop_front();
            }
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
        ring.push(r, sent(r), |_| false);
        ring.push(r, finished(r), |_| false);
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
        ring.push(a, sent(a), |_| false);
        ring.push(b, sent(b), |_| false);
        ring.push(a, finished(a), |_| false);
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
            ring.push(r, sent(r), |_| false);
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
            ring.push(r, sent(r), |_| false);
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
            ring.push(r, sent(r), |_| false);
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
        ring.push(r, sent(r), |_| false);
        assert_eq!(ring.len(), 1);
    }

    #[test]
    fn fixed_capacity_drops_oldest_at_cap_without_growing() {
        // with_capacity is floor == max: it never grows, so it evicts at
        // the cap exactly as before this feature.
        let mut ring = ActorTraceRing::with_capacity(3);
        let r = mid(1, 1);
        // The predicate says "still in flight" for everything, yet a fixed
        // ring (floor == max) never grows — it has no headroom.
        for _ in 0..10 {
            ring.push(r, sent(r), |_| true);
        }
        assert_eq!(ring.len(), 3, "fixed ring stays at its cap");
    }

    #[test]
    fn growing_ring_retains_in_flight_entries_a_fixed_ring_would_have_dropped() {
        // Floor 2, max 8, oldest chain always in flight (`|_| true`):
        // pushing 8 entries grows the ring instead of dropping, so all 8
        // are retained and no gap is signalled.
        let mut ring = ActorTraceRing::with_growth(2, 8);
        let r = mid(1, 1);
        for _ in 0..8 {
            ring.push(r, sent(r), |_| true);
        }
        assert_eq!(ring.len(), 8, "grew floor→max instead of dropping");
        let (entries, _, truncated_before) = ok(ring.tail(&unfiltered()));
        assert_eq!(entries.len(), 8);
        assert_eq!(entries[0].sequence, 1, "oldest entry still present");
        assert_eq!(truncated_before, None, "nothing evicted, so no gap");
    }

    #[test]
    fn growing_ring_drops_oldest_only_once_at_max() {
        // Floor 2, max 8, oldest always in flight: the 9th push has nowhere
        // left to grow, so it resumes drop-oldest and the gap cursor fires.
        let mut ring = ActorTraceRing::with_growth(2, 8);
        let r = mid(1, 1);
        for _ in 0..9 {
            ring.push(r, sent(r), |_| true);
        }
        assert_eq!(ring.len(), 8, "capped at max");
        let (entries, _, truncated_before) = ok(ring.tail(&unfiltered()));
        assert_eq!(entries[0].sequence, 2, "sequence 1 was evicted");
        assert_eq!(truncated_before, Some(2));
    }

    #[test]
    fn growth_threshold_doubles_and_clamps_to_a_non_power_of_two_max() {
        // Floor 2, max 5, oldest always in flight: the threshold climbs
        // 2 → 4 → 5 (the last step clamps instead of overshooting to 8), so
        // the ring holds exactly 5 before it starts dropping.
        let mut ring = ActorTraceRing::with_growth(2, 5);
        let r = mid(1, 1);
        for _ in 0..5 {
            ring.push(r, sent(r), |_| true);
        }
        assert_eq!(ring.len(), 5, "grew up to the clamped max of 5");
        let (.., truncated_before) = ok(ring.tail(&unfiltered()));
        assert_eq!(truncated_before, None, "nothing dropped yet at the max");
        ring.push(r, sent(r), |_| true);
        assert_eq!(ring.len(), 5, "stays at max, drops oldest");
        let (entries, ..) = ok(ring.tail(&unfiltered()));
        assert_eq!(entries[0].sequence, 2, "oldest evicted past the max");
    }

    #[test]
    fn settled_oldest_is_reclaimed_instead_of_growing() {
        // The settlement-aware property (issue 2076): with ample growth
        // headroom (floor 2, max 64) but the oldest chain always settled
        // (`|_| false`), the ring reclaims rather than grows — it stays at
        // the floor and signals the evicted prefix, so a stream of settled
        // events never inflates the ring.
        let mut ring = ActorTraceRing::with_growth(2, 64);
        let r = mid(1, 1);
        for _ in 0..20 {
            ring.push(r, sent(r), |_| false);
        }
        assert_eq!(ring.len(), 2, "settled oldest reclaimed, no growth");
        let (entries, _, truncated_before) = ok(ring.tail(&unfiltered()));
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].sequence, 19, "only the two newest survive");
        assert_eq!(truncated_before, Some(19));
    }

    #[test]
    fn growth_stops_once_the_oldest_chain_settles() {
        // Grow while the oldest chain is in flight, then — once it settles —
        // reclaim instead of growing further. Floor 2, max 64.
        let mut ring = ActorTraceRing::with_growth(2, 64);
        let r = mid(1, 1);
        // In-flight oldest: grows 2 → 4 → 8 across these pushes.
        for _ in 0..8 {
            ring.push(r, sent(r), |_| true);
        }
        let grown = ring.len();
        assert_eq!(grown, 8, "grew to hold the in-flight burst");
        // Oldest now settled: further pushes reclaim, holding the size.
        for _ in 0..8 {
            ring.push(r, sent(r), |_| false);
        }
        assert_eq!(ring.len(), grown, "settled oldest reclaimed, size held");
    }

    #[test]
    fn max_below_floor_clamps_up_to_floor() {
        // A misconfigured max < floor degrades to a fixed ring at the
        // floor — never one asked to hold fewer than it started with.
        let mut ring = ActorTraceRing::with_growth(4, 1);
        let r = mid(1, 1);
        for _ in 0..10 {
            ring.push(r, sent(r), |_| true);
        }
        assert_eq!(ring.len(), 4, "clamped to the floor, fixed thereafter");
    }
}
