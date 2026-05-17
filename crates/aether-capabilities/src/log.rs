//! `aether.log` cap. Issue 565 pilot for the `#[bridge]` mod pattern:
//! the struct + actor impl + tests live inside `mod native`, which
//! `#[bridge]` cfg-gates. The macro emits a wasm-stub `pub struct
//! LogCapability;` at file root plus always-on Singleton + Actor +
//! `HandlesKind` markers, and re-exports the real struct from inside
//! the mod on native.
//!
//! Issue #581 retired `log_capture`'s ring/flush plumbing in favour
//! of this cap as the egress owner. Every `tracing::*` event flows
//! through `aether-actor::log`'s actor-aware subscriber:
//!
//! - In-actor → buffered in `LogBuffer` → drain at handler exit
//!   ships a single `LogBatch` mail to this mailbox.
//! - Host code → single-entry `LogBatch` mail through the
//!   registered host dispatch (also lands here).
//!
//! Issue 776 retired the hub-bound egress path and put a bounded
//! ring inside this cap; the cap now serves [`LogRead`] requests via
//! [`LogReadResult`]. ADR-0023 §4 contract restored under the
//! forward model (RPC pull instead of frame push).

use aether_kinds::{LogBatch, LogRead};

#[aether_actor::bridge(singleton)]
mod native {
    use super::{LogBatch, LogRead};
    use aether_actor::{MailCtx, actor};
    use aether_kinds::{LogEntry, LogEvent, LogReadResult};
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::chassis::error::BootError;
    use std::collections::VecDeque;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Default per-substrate cap on retained log entries. Per
    /// ADR-0023 §3, each entry is bounded by the subscriber-side
    /// 16 KiB per-message cap, so 2,000 × 16 KiB worst case is well
    /// under the original 2 MiB budget; typical messages are 100-300
    /// bytes. Eviction is FIFO once the ring is full.
    const DEFAULT_RING_CAP: usize = 2_000;

    /// Default cap on entries returned per [`LogRead`] when the
    /// caller passes `max == 0`. Same value ADR-0023 §4 documented.
    const DEFAULT_READ_MAX: u32 = 100;

    /// Absolute ceiling on entries returned per [`LogRead`]. Caller-
    /// supplied `max` values above this clamp down.
    const MAX_READ_MAX: u32 = 1_000;

    /// `aether.log` mailbox cap. Receives [`LogBatch`] mail and
    /// appends each entry to an internal bounded ring; serves
    /// [`LogRead`] pulls back via [`LogReadResult`] (issue 776,
    /// restoring ADR-0023 §4 under the forward model).
    ///
    /// Issue 629 / Phase B: `sequence` is a plain `u64` field. The
    /// dispatcher thread is the sole writer (one handler at a time),
    /// so the pre-Phase-A `AtomicU64` was a worker-pool-era artifact
    /// rather than a contention point.
    pub struct LogCapability {
        ring: VecDeque<LogEntry>,
        ring_cap: usize,
        /// Monotonic per-substrate sequence; starts at 1. Persists
        /// across eviction — the next entry after the ring evicts
        /// gets `last_sequence + 1`, so callers' cursors stay
        /// meaningful even after eviction.
        sequence: u64,
    }

    #[actor]
    impl NativeActor for LogCapability {
        type Config = ();
        const NAMESPACE: &'static str = "aether.log";

        fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self::with_ring_cap(DEFAULT_RING_CAP))
        }

        /// Append a drained log batch to the ring. Each batch entry
        /// gets a fresh `sequence` (monotonic per substrate boot) and
        /// the substrate-local `(timestamp, origin)` stamp; eviction
        /// drops the oldest when the ring is at cap.
        ///
        /// # Agent
        /// The actor-aware tracing subscriber buffers `tracing::*` events
        /// per-actor and ships a [`LogBatch`] here at handler exit (or
        /// immediately on `WARN`/`ERROR` priority flush). Host-emitted
        /// events land as single-entry batches. Sender attribution
        /// rides on the mail envelope; this cap reads `ctx.origin()`
        /// to populate `LogEntry::origin`.
        #[handler]
        fn on_log_batch(&mut self, ctx: &mut NativeCtx<'_>, batch: LogBatch) {
            let origin = ctx.origin();
            let now = now_unix_ms();
            for event in batch.entries {
                self.push_entry(event, now, origin);
            }
        }

        /// Serve a [`LogRead`] pull. Filters by `min_level` and the
        /// `since` cursor, caps the returned slice at `max` (clamped
        /// to [`MAX_READ_MAX`]; `0` resolves to [`DEFAULT_READ_MAX`]),
        /// and surfaces a `truncated_before` signal when the ring has
        /// evicted entries the caller hadn't seen yet. Always replies
        /// `Ok` on a healthy cap — filter mismatches return an empty
        /// `entries` slice rather than `Err`.
        ///
        /// # Agent
        /// Reply: [`LogReadResult`]. `next_since` is the highest
        /// sequence in the returned `entries`, or the caller's
        /// `since` echoed back when `entries` is empty — thread it
        /// into the next `LogRead::since` for a stable cursor.
        #[handler]
        fn on_log_read(&self, ctx: &mut NativeCtx<'_>, mail: LogRead) {
            ctx.reply(&self.read(mail));
        }
    }

    impl LogCapability {
        /// Hand-construct a cap with a specific ring cap. Used by the
        /// chassis builder via `init` (which calls
        /// `with_ring_cap(DEFAULT_RING_CAP)`) and by unit tests that
        /// want to exercise eviction without pushing 2,000 entries.
        ///
        /// `ring_cap` is clamped to `max(1, _)` — a zero-cap ring
        /// would drop every entry and is never the intent.
        pub(crate) fn with_ring_cap(ring_cap: usize) -> Self {
            Self {
                ring: VecDeque::with_capacity(ring_cap.max(1)),
                ring_cap: ring_cap.max(1),
                sequence: 1,
            }
        }

        /// Push one event onto the ring, stamping it with the
        /// next-available `sequence` + caller-supplied `(timestamp,
        /// origin)`. Pure data manipulation — handler-thread-safe
        /// because the dispatcher serialises calls. Evicts the
        /// oldest entry when the ring is at cap.
        fn push_entry(
            &mut self,
            event: LogEvent,
            timestamp_unix_ms: u64,
            origin: Option<aether_data::MailboxId>,
        ) {
            let sequence = self.sequence;
            self.sequence += 1;
            let entry = LogEntry {
                timestamp_unix_ms,
                level: event.level,
                target: event.target,
                message: event.message,
                sequence,
                origin,
            };
            if self.ring.len() == self.ring_cap {
                self.ring.pop_front();
            }
            self.ring.push_back(entry);
        }

        /// Pure read-side: filter, cap, paginate, compute
        /// `truncated_before`. Lifted out of `on_log_read` so unit
        /// tests cover boundary cases (max clamping, level filter,
        /// cursor edges, eviction signal) without booting a chassis.
        pub(crate) fn read(&self, request: LogRead) -> LogReadResult {
            let max = resolve_max(request.max) as usize;
            let min_level = request.min_level.unwrap_or(0);
            let since = request.since.unwrap_or(0);

            let earliest = self.ring.front().map(|e| e.sequence);
            let truncated_before = match earliest {
                Some(e) if e > since + 1 => Some(e),
                _ => None,
            };

            let entries: Vec<LogEntry> = self
                .ring
                .iter()
                .filter(|e| e.sequence > since && e.level >= min_level)
                .take(max)
                .cloned()
                .collect();

            let next_since = entries.last().map_or(since, |e| e.sequence);

            LogReadResult::Ok {
                entries,
                next_since,
                truncated_before,
            }
        }
    }

    /// Clamp `max == 0` to the default and any value over
    /// [`MAX_READ_MAX`] down to the ceiling. Surface as a free
    /// function so the cap-side handler reads cleanly and unit
    /// tests can pin the boundaries without booting a chassis.
    fn resolve_max(max: u32) -> u32 {
        if max == 0 {
            DEFAULT_READ_MAX
        } else {
            max.min(MAX_READ_MAX)
        }
    }

    fn now_unix_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            // Millisecond clock fits comfortably in u64 for the next ~584 million years.
            .map(|d| {
                #[allow(clippy::cast_possible_truncation)]
                let ms = d.as_millis() as u64;
                ms
            })
            .unwrap_or(0)
    }

    #[cfg(test)]
    mod tests {
        use std::sync::Arc;
        use std::thread;
        use std::time::Duration;

        use super::{BootError, LogBatch, LogCapability, LogRead, LogReadResult, resolve_max};
        use crate::test_chassis::TestChassis;
        use aether_actor::Actor;
        use aether_data::Kind;
        use aether_kinds::LogEvent;
        use aether_substrate::chassis::builder::Builder;
        use aether_substrate::mail::mailer::Mailer;
        use aether_substrate::mail::registry::{MailboxEntry, Registry};

        fn fresh_substrate() -> (Arc<Registry>, Arc<Mailer>) {
            {
                let registry = Arc::new(Registry::new());
                let store = ::std::sync::Arc::new(
                    ::aether_substrate::handle_store::HandleStore::new(1024 * 1024),
                );
                let mailer = Arc::new(Mailer::new(Arc::clone(&registry), store));
                (registry, mailer)
            }
        }

        fn event(level: u8, message: &str) -> LogEvent {
            LogEvent {
                level,
                target: "log_cap_test".to_owned(),
                message: message.to_owned(),
            }
        }

        /// Unwrap the `Ok` arm of a `LogReadResult`. `Err` only
        /// surfaces in pathological cases on the cap's healthy path,
        /// so tests panic-and-print on it rather than threading
        /// `Result` matching through every assert.
        fn ok(result: LogReadResult) -> (Vec<aether_kinds::LogEntry>, u64, Option<u64>) {
            match result {
                LogReadResult::Ok {
                    entries,
                    next_since,
                    truncated_before,
                } => (entries, next_since, truncated_before),
                LogReadResult::Err { error } => panic!("expected Ok, got Err: {error}"),
            }
        }

        /// End-to-end: boot the cap through `with_actor`, push a
        /// `LogBatch` mail at the registered mailbox, the dispatcher
        /// thread runs the macro-emitted `NativeDispatch` which calls
        /// `on_log_batch`. Test asserts dispatch + clean shutdown.
        #[test]
        fn capability_routes_log_batch_through_dispatcher() {
            let (registry, mailer) = fresh_substrate();
            let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
                .with_actor::<LogCapability>(())
                .build_passive()
                .expect("capability boots");

            let id = registry
                .lookup(LogCapability::NAMESPACE)
                .expect("mailbox registered");
            let MailboxEntry::Inbox(handler) = registry.entry(id).expect("entry") else {
                panic!("expected mailbox entry");
            };

            let batch = LogBatch {
                entries: vec![event(3, "parse failed: missing close paren")],
            };
            let bytes = postcard::to_allocvec(&batch).expect("encode");
            handler.enqueue(aether_substrate::mail::registry::OwnedDispatch {
                kind: <LogBatch as Kind>::ID,
                kind_name: "aether.log".to_owned(),
                origin: None,
                sender: aether_substrate::mail::ReplyTo::NONE,
                payload: bytes,
                count: 1,
                mail_id: aether_substrate::mail::MailId::NONE,
                root: aether_substrate::mail::MailId::NONE,
                parent_mail: None,
            });

            thread::sleep(Duration::from_millis(50));
            drop(chassis);
        }

        #[test]
        fn duplicate_claim_rejects_with_typed_error() {
            let (registry, mailer) = fresh_substrate();
            registry.register_inbox(
                LogCapability::NAMESPACE,
                aether_substrate::mail::registry::noop_handler(),
            );

            let err = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
                .with_actor::<LogCapability>(())
                .build_passive()
                .expect_err("collision must surface as BootError");
            assert!(matches!(
                err,
                BootError::MailboxAlreadyClaimed { ref name }
                    if name == LogCapability::NAMESPACE
            ));
        }

        /// `resolve_max(0)` returns the documented default (100);
        /// values above 1000 clamp to 1000; values in-band pass through.
        #[test]
        fn resolve_max_clamps_to_documented_bounds() {
            assert_eq!(resolve_max(0), 100, "max=0 should resolve to default 100");
            assert_eq!(resolve_max(50), 50, "in-band max should pass through");
            assert_eq!(
                resolve_max(1_000),
                1_000,
                "max at the ceiling should pass through"
            );
            assert_eq!(
                resolve_max(5_000),
                1_000,
                "max above the ceiling should clamp"
            );
        }

        /// Round-trip: push three entries, read with no filter,
        /// observe all three in order with monotonically-increasing
        /// `sequence` starting at 1.
        #[test]
        fn read_returns_entries_in_push_order_with_monotonic_sequence() {
            let mut cap = LogCapability::with_ring_cap(8);
            cap.push_entry(event(2, "first"), 100, None);
            cap.push_entry(event(2, "second"), 200, None);
            cap.push_entry(event(2, "third"), 300, None);

            let (entries, next_since, truncated_before) = ok(cap.read(LogRead {
                max: 0,
                min_level: None,
                since: None,
            }));

            assert_eq!(entries.len(), 3);
            assert_eq!(entries[0].message, "first");
            assert_eq!(entries[0].sequence, 1);
            assert_eq!(entries[1].sequence, 2);
            assert_eq!(entries[2].sequence, 3);
            assert_eq!(next_since, 3);
            assert_eq!(truncated_before, None);
        }

        /// `min_level = Some(3)` (warn+) drops trace/debug/info; the
        /// surviving entries still carry their original `sequence`
        /// so a follow-up call with `since = next_since` doesn't
        /// double-pull.
        #[test]
        fn read_filters_below_min_level() {
            let mut cap = LogCapability::with_ring_cap(8);
            cap.push_entry(event(0, "trace"), 0, None);
            cap.push_entry(event(2, "info"), 0, None);
            cap.push_entry(event(3, "warn"), 0, None);
            cap.push_entry(event(4, "error"), 0, None);

            let (entries, _, _) = ok(cap.read(LogRead {
                max: 0,
                min_level: Some(3),
                since: None,
            }));

            assert_eq!(entries.len(), 2);
            assert_eq!(entries[0].message, "warn");
            assert_eq!(entries[1].message, "error");
        }

        /// `since = N` returns only entries with `sequence > N`.
        /// `next_since` advances to the highest sequence in the
        /// returned slice (caller's cursor); a follow-up call with
        /// that cursor returns an empty slice and echoes the cursor.
        #[test]
        fn read_since_cursor_paginates_without_double_pulling() {
            let mut cap = LogCapability::with_ring_cap(8);
            for i in 1..=5 {
                cap.push_entry(event(2, &format!("msg-{i}")), 0, None);
            }

            let (entries, next_since, _) = ok(cap.read(LogRead {
                max: 0,
                min_level: None,
                since: Some(2),
            }));

            assert_eq!(entries.len(), 3, "should return seq 3, 4, 5");
            assert_eq!(entries[0].sequence, 3);
            assert_eq!(next_since, 5);

            let (entries, cursor, _) = ok(cap.read(LogRead {
                max: 0,
                min_level: None,
                since: Some(next_since),
            }));

            assert!(entries.is_empty(), "no entries past the cursor");
            assert_eq!(cursor, 5, "next_since echoes the caller's cursor");
        }

        /// `max` caps the returned slice size — the cap doesn't
        /// re-stamp `next_since` past the returned tail, so a
        /// follow-up call with that cursor picks up the remainder.
        #[test]
        fn read_max_caps_returned_slice_and_cursor_walks_remainder() {
            let mut cap = LogCapability::with_ring_cap(8);
            for i in 1..=5 {
                cap.push_entry(event(2, &format!("msg-{i}")), 0, None);
            }

            let (entries, next_since, _) = ok(cap.read(LogRead {
                max: 2,
                min_level: None,
                since: None,
            }));

            assert_eq!(entries.len(), 2);
            assert_eq!(next_since, 2);

            let (entries, next_since, _) = ok(cap.read(LogRead {
                max: 2,
                min_level: None,
                since: Some(next_since),
            }));

            assert_eq!(entries.len(), 2);
            assert_eq!(entries[0].sequence, 3);
            assert_eq!(next_since, 4);
        }

        /// Eviction signal: with a 3-entry ring, pushing 5 entries
        /// evicts seq 1 and 2. A reader with `since = 0` (expecting
        /// the prefix) gets `truncated_before = Some(3)` — the
        /// lowest sequence still in the ring.
        #[test]
        fn read_signals_truncated_before_when_ring_evicted_prefix() {
            let mut cap = LogCapability::with_ring_cap(3);
            for i in 1..=5 {
                cap.push_entry(event(2, &format!("msg-{i}")), 0, None);
            }

            let (entries, _, truncated_before) = ok(cap.read(LogRead {
                max: 0,
                min_level: None,
                since: None,
            }));

            assert_eq!(
                entries.len(),
                3,
                "ring should hold the last 3 entries (seq 3, 4, 5)"
            );
            assert_eq!(entries[0].sequence, 3);
            assert_eq!(
                truncated_before,
                Some(3),
                "seq 1+2 evicted; truncated_before flags the gap"
            );
        }

        /// `truncated_before` stays `None` when caller's `since`
        /// covers everything that was evicted — the gap is on the
        /// caller's side of the cursor, so they've already missed
        /// (or never wanted) those entries.
        #[test]
        fn read_does_not_signal_truncated_before_when_since_covers_eviction() {
            let mut cap = LogCapability::with_ring_cap(3);
            for i in 1..=5 {
                cap.push_entry(event(2, &format!("msg-{i}")), 0, None);
            }

            let (_, _, truncated_before) = ok(cap.read(LogRead {
                max: 0,
                min_level: None,
                since: Some(2),
            }));

            assert_eq!(
                truncated_before, None,
                "ring's earliest (3) is exactly since+1; no caller-visible gap"
            );
        }
    }
}

// Subscriber install + tracing-subscriber stack moved to
// `aether-substrate::log_install` so the substrate's boot path can
// install the actor-aware layer before any cap boots (early-boot
// `tracing::*` still surfaces via the fmt::Layer fallback). The cap
// keeps only its handler body — this file no longer carries any
// install-side machinery.
