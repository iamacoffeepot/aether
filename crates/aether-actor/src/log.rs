//! ADR-0081 per-actor log storage. Every actor — native or wasm
//! trampoline — owns an [`ActorLogRing`] in its [`crate::local::ActorSlots`].
//! `tracing::*` events fired inside a dispatched handler land in the
//! ring directly; the host's `aether-substrate::runtime::log_install::ActorAwareLayer`
//! is the producer side (native), and the wasm trampoline's per-event
//! FFI host fn re-fires the event on the trampoline thread so the
//! same layer catches it (guest events ride the trampoline's
//! `ActorSlots` — see ADR-0081 §7).
//!
//! Out-of-actor `tracing::*` events (substrate boot, scheduler
//! thread, panic hook) keep hitting stderr via the substrate's
//! registered `tsfmt::Layer` — they do not enter any ring and do not
//! surface in `engine_logs` (matches today's post-#601 behaviour).
//!
//! ADR-0081 retires the pre-ADR flush-hop machinery alongside this
//! rewrite: `LogBuffer`, `drain_buffer`, the `IN_LOG_PIPELINE`
//! re-entry guard, the chassis-pushed `ConfigureLogDrain` mail, the
//! `set_native_log_shipper` hook, the wasm `MAIL_BRIDGE` route for
//! `LogBatch`, and `LogCapability` itself. The new query path
//! ([`aether_kinds::LogTail`] / [`aether_kinds::LogTailResult`]) is
//! served by a framework-built-in dispatch arm every actor inherits
//! — no `#[handler]` for it on user types.

extern crate alloc;

use alloc::collections::VecDeque;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt::Write as _;

use aether_kinds::{LogEntry, LogTail, LogTailResult};
use tracing::{
    Event, Level,
    field::{Field, Visit},
};
// Wasm-only `tracing` imports needed by [`WasmSubscriber`]'s
// `Subscriber` impl.
#[cfg(target_arch = "wasm32")]
use tracing::{Subscriber, span};

use crate::Local;
use core::fmt;

/// Default per-actor ring capacity. Overridable at substrate boot
/// via `AETHER_ACTOR_LOG_RING_SIZE` — the substrate parses the env
/// var and threads the value into [`ActorLogRing::with_capacity`]
/// when the chassis materialises each actor's slots. 1024 keeps the
/// worst-case memory at ~1024 × ~300B = ~300 KiB per actor on
/// typical traffic; ADR-0081 §Negative consequences for the N-actor
/// total-memory call.
pub const DEFAULT_RING_CAP: usize = 1024;

/// Substrate-default cap on entries returned per [`LogTail`] when
/// the caller passes `max == 0`. Same value ADR-0023 §4 used for the
/// pre-ADR-0081 centralized `LogRead`; the upper clamp is the ring
/// capacity itself (one ring at a time can never reply with more
/// than it holds).
pub const DEFAULT_TAIL_MAX: u32 = 100;

/// Absolute ceiling on entries returned per [`LogTail`]. Caller-
/// supplied `max` values above this clamp down. Same value
/// ADR-0023 §4 used.
pub const MAX_TAIL_MAX: u32 = 1_000;

/// Hard cap on the per-event message bytes. Trims oversize payloads
/// with a `" [truncated]"` suffix so a reader of `engine_logs` can
/// tell the source was longer.
const MAX_MESSAGE_BYTES: usize = 4096;
const TRUNCATED_SUFFIX: &str = " [truncated]";

/// Per-actor bounded ring of [`LogEntry`]. ADR-0081 §1. Single-
/// writer: the actor's dispatcher thread is the sole producer
/// (post-ADR-0038 — one OS thread per actor), so the `sequence`
/// counter and the underlying `VecDeque` need no lock or atomic
/// internally. Cross-thread reads run through the `Local`
/// [`crate::local::with_stamped`] path on the responding actor's
/// dispatcher (the framework-built-in `aether.log.tail` handler
/// invokes [`Self::tail`] from inside the dispatcher loop, holding
/// `&mut` exclusively for the duration of the read).
///
/// Eviction is FIFO drop-oldest once the ring is at cap; readers
/// detect the gap via the computed-at-read-time `truncated_before`
/// cursor.
///
/// Mirrors the shape `LogCapability::ring` had pre-ADR-0081 — same
/// `(ring, ring_cap, sequence)` triple and the same push/read
/// semantics — relocated to the actor's `ActorSlots` and reachable
/// only through the framework dispatch arm.
pub struct ActorLogRing {
    ring: VecDeque<LogEntry>,
    ring_cap: usize,
    /// Monotonic per-actor sequence; starts at 1. Persists across
    /// eviction — the next entry after the ring evicts gets
    /// `last_sequence + 1`, so a caller's `since` cursor stays
    /// meaningful even after eviction (it just resolves into a
    /// `truncated_before` gap signal).
    sequence: u64,
}

impl Default for ActorLogRing {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_RING_CAP)
    }
}

impl Local for ActorLogRing {}

impl ActorLogRing {
    /// Build an empty ring with the supplied capacity. `ring_cap` is
    /// clamped to `max(1, _)` — a zero-cap ring would drop every
    /// entry and is never the intent. Used by the substrate's
    /// per-actor `ActorSlots` materialisation path so the boot-time
    /// `AETHER_ACTOR_LOG_RING_SIZE` reading lands in the right
    /// place.
    #[must_use]
    pub fn with_capacity(ring_cap: usize) -> Self {
        let ring_cap = ring_cap.max(1);
        Self {
            ring: VecDeque::with_capacity(ring_cap),
            ring_cap,
            sequence: 1,
        }
    }

    /// Push one event onto the ring, stamping it with the next-
    /// available `sequence` + caller-supplied `timestamp_unix_ms`.
    /// `origin` is left `None` here — the entry's *owner* is the
    /// actor; the aggregator stamps `origin = Some(responder)` at
    /// merge time (`EngineLogs` fan-out) so the wire reply carries
    /// attribution without each ring duplicating its own id.
    /// Evicts the oldest entry when the ring is at cap.
    pub fn push(&mut self, level: u8, target: String, message: String, timestamp_unix_ms: u64) {
        let sequence = self.sequence;
        self.sequence += 1;
        let entry = LogEntry {
            timestamp_unix_ms,
            level,
            target,
            message,
            sequence,
            origin: None,
        };
        if self.ring.len() == self.ring_cap {
            self.ring.pop_front();
        }
        self.ring.push_back(entry);
    }

    /// Pure read-side: filter on `min_level` + `since`, cap at `max`
    /// (with the documented `0 → DEFAULT_TAIL_MAX` and `> MAX_TAIL_MAX
    /// → MAX_TAIL_MAX` clamping), compute the `truncated_before`
    /// cursor. Mirrors the pre-ADR-0081 `LogCapability::read` shape
    /// so callers' stable cursor semantics survive intact.
    #[must_use]
    pub fn tail(&self, request: &LogTail) -> LogTailResult {
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

        LogTailResult::Ok {
            entries,
            next_since,
            truncated_before,
        }
    }

    /// Drain a snapshot of the current ring entries oldest-to-newest
    /// without filtering. Used by the panic hook to dump the
    /// panicking actor's recent history to disk (ADR-0081 §4); not
    /// reachable through the wire surface.
    #[must_use]
    pub fn snapshot(&self) -> Vec<LogEntry> {
        self.ring.iter().cloned().collect()
    }

    /// Number of entries currently in the ring. Test-facing
    /// observability.
    #[must_use]
    pub fn len(&self) -> usize {
        self.ring.len()
    }

    /// Is the ring empty? Mirrors `len()`'s test-facing role.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ring.is_empty()
    }
}

/// Clamp `max == 0` to [`DEFAULT_TAIL_MAX`] and any value over
/// [`MAX_TAIL_MAX`] down to the ceiling. Surfaced as a free
/// function so the framework dispatch arm reads cleanly and unit
/// tests can pin the boundaries without materialising a ring.
fn resolve_max(max: u32) -> u32 {
    if max == 0 {
        DEFAULT_TAIL_MAX
    } else {
        max.min(MAX_TAIL_MAX)
    }
}

pub(crate) fn level_to_u8(level: Level) -> u8 {
    match level {
        Level::TRACE => 0,
        Level::DEBUG => 1,
        Level::INFO => 2,
        Level::WARN => 3,
        Level::ERROR => 4,
    }
}

/// Walks an `Event`'s fields and renders them in fields-first
/// order: `key1=val1 key2=val2 message_body`. Matches
/// `tracing-subscriber`'s default fmt layer so a reader of
/// `engine_logs` sees the same shape regardless of which side
/// emitted the event. Returns `(level, target, message)`; callers
/// stamp the timestamp + push into the ring themselves.
#[must_use]
pub fn render_event(event: &Event<'_>) -> (u8, String, String) {
    let metadata = event.metadata();
    let level = level_to_u8(*metadata.level());
    let target = metadata.target().to_string();

    let mut visitor = MessageBuilder::new();
    event.record(&mut visitor);
    let message = visitor.finish();

    (level, target, message)
}

struct MessageBuilder {
    fields: String,
    message: String,
}

impl MessageBuilder {
    fn new() -> Self {
        Self {
            fields: String::new(),
            message: String::new(),
        }
    }

    fn finish(mut self) -> String {
        if !self.fields.is_empty() && !self.message.is_empty() {
            self.fields.push(' ');
        }
        self.fields.push_str(&self.message);
        truncate(self.fields)
    }

    fn append_field(&mut self, name: &str, separator: &str, value: fmt::Arguments<'_>) {
        if !self.fields.is_empty() {
            self.fields.push(' ');
        }
        let _ = write!(&mut self.fields, "{name}{separator}{value}");
    }
}

impl Visit for MessageBuilder {
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.append_field(field.name(), "=", format_args!("{value}"));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.append_field(field.name(), "=", format_args!("{value}"));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.append_field(field.name(), "=", format_args!("{value}"));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message.push_str(value);
        } else {
            self.append_field(field.name(), "=", format_args!("{value}"));
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(&mut self.message, "{value:?}");
        } else {
            self.append_field(field.name(), "=", format_args!("{value:?}"));
        }
    }
}

fn truncate(mut s: String) -> String {
    if s.len() <= MAX_MESSAGE_BYTES {
        return s;
    }
    let mut cap = MAX_MESSAGE_BYTES.saturating_sub(TRUNCATED_SUFFIX.len());
    while cap > 0 && !s.is_char_boundary(cap) {
        cap -= 1;
    }
    s.truncate(cap);
    s.push_str(TRUNCATED_SUFFIX);
    s
}

/// Wasm-side tracing subscriber. Each `tracing::*` event the guest
/// fires walks through this subscriber's `event()` and rides the
/// trampoline's per-event FFI host fn back into the host process,
/// where the host-side `ActorAwareLayer` lands it in the
/// trampoline's [`ActorLogRing`]. ADR-0081 §7 — the previous
/// guest-side `LogBuffer` + `drain_buffer` + `LogBatch` flush hop
/// retired alongside this rewrite.
#[cfg(target_arch = "wasm32")]
pub struct WasmSubscriber {
    next_span: core::sync::atomic::AtomicU64,
}

#[cfg(target_arch = "wasm32")]
impl WasmSubscriber {
    pub const fn new() -> Self {
        Self {
            next_span: core::sync::atomic::AtomicU64::new(1),
        }
    }
}

#[cfg(target_arch = "wasm32")]
impl Default for WasmSubscriber {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(target_arch = "wasm32")]
impl Subscriber for WasmSubscriber {
    fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
        // Filtering happens on the substrate side; the wasm
        // subscriber forwards everything so the host's `EnvFilter`
        // sees the guest's reported target.
        true
    }

    fn new_span(&self, _attrs: &span::Attributes<'_>) -> span::Id {
        let id = self
            .next_span
            .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        span::Id::from_u64(id.max(1))
    }

    fn record(&self, _: &span::Id, _: &span::Record<'_>) {}
    fn record_follows_from(&self, _: &span::Id, _: &span::Id) {}
    fn enter(&self, _: &span::Id) {}
    fn exit(&self, _: &span::Id) {}

    fn event(&self, event: &Event<'_>) {
        let (level, target, message) = render_event(event);
        crate::ffi::bridge::MAIL_BRIDGE.emit_log_event(level, &target, &message);
    }
}

#[cfg(target_arch = "wasm32")]
static WASM_INSTALLED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Install the wasm-side actor-aware subscriber as `tracing`'s
/// global default. Called from the `export!` macro before the
/// guest's `Component::init` runs (so logging from `init` works).
/// Idempotent.
#[cfg(target_arch = "wasm32")]
pub fn install_wasm_subscriber() {
    use core::sync::atomic::Ordering;
    if WASM_INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    let _ = tracing::dispatcher::set_global_default(tracing::dispatcher::Dispatch::new(
        WasmSubscriber::new(),
    ));
}

/// `aether::log_trace!("msg")` — equivalent to `tracing::trace!`.
#[macro_export]
macro_rules! log_trace {
    ($($t:tt)*) => { ::tracing::trace!($($t)*) };
}

/// `aether::log_debug!("msg")` — equivalent to `tracing::debug!`.
#[macro_export]
macro_rules! log_debug {
    ($($t:tt)*) => { ::tracing::debug!($($t)*) };
}

/// `aether::log_info!("msg")` — equivalent to `tracing::info!`.
#[macro_export]
macro_rules! log_info {
    ($($t:tt)*) => { ::tracing::info!($($t)*) };
}

/// `aether::log_warn!("msg")` — equivalent to `tracing::warn!`.
#[macro_export]
macro_rules! log_warn {
    ($($t:tt)*) => { ::tracing::warn!($($t)*) };
}

/// `aether::log_error!("msg")` — equivalent to `tracing::error!`.
#[macro_export]
macro_rules! log_error {
    ($($t:tt)*) => { ::tracing::error!($($t)*) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::borrow::ToOwned;
    use alloc::format;

    #[test]
    fn level_mapping() {
        assert_eq!(level_to_u8(Level::TRACE), 0);
        assert_eq!(level_to_u8(Level::DEBUG), 1);
        assert_eq!(level_to_u8(Level::INFO), 2);
        assert_eq!(level_to_u8(Level::WARN), 3);
        assert_eq!(level_to_u8(Level::ERROR), 4);
    }

    #[test]
    fn truncate_preserves_short_messages() {
        let s = String::from("short message");
        let out = truncate(s);
        assert_eq!(out, "short message");
    }

    #[test]
    fn truncate_appends_suffix_when_oversize() {
        let s = "a".repeat(MAX_MESSAGE_BYTES + 100);
        let out = truncate(s);
        assert!(out.ends_with(TRUNCATED_SUFFIX));
        assert!(out.len() <= MAX_MESSAGE_BYTES);
    }

    #[test]
    fn truncate_respects_char_boundary() {
        let mut s = String::with_capacity(MAX_MESSAGE_BYTES + 4);
        for _ in 0..(MAX_MESSAGE_BYTES / 4 + 5) {
            s.push('🦀');
        }
        let out = truncate(s);
        assert!(out.ends_with(TRUNCATED_SUFFIX));
    }

    #[test]
    fn resolve_max_clamps_to_documented_bounds() {
        assert_eq!(resolve_max(0), DEFAULT_TAIL_MAX);
        assert_eq!(resolve_max(50), 50);
        assert_eq!(resolve_max(MAX_TAIL_MAX), MAX_TAIL_MAX);
        assert_eq!(resolve_max(5_000), MAX_TAIL_MAX);
    }

    fn push_three(ring: &mut ActorLogRing) {
        ring.push(2, "test".to_owned(), "first".to_owned(), 100);
        ring.push(2, "test".to_owned(), "second".to_owned(), 200);
        ring.push(2, "test".to_owned(), "third".to_owned(), 300);
    }

    /// Round-trip push + tail with no filter returns the entries in
    /// push order with monotonically-increasing sequence from 1.
    #[test]
    fn tail_returns_entries_in_push_order_with_monotonic_sequence() {
        let mut ring = ActorLogRing::with_capacity(8);
        push_three(&mut ring);
        let LogTailResult::Ok {
            entries,
            next_since,
            truncated_before,
        } = ring.tail(&LogTail {
            max: 0,
            min_level: None,
            since: None,
        })
        else {
            panic!("expected Ok");
        };
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].message, "first");
        assert_eq!(entries[0].sequence, 1);
        assert_eq!(entries[2].sequence, 3);
        assert_eq!(next_since, 3);
        assert_eq!(truncated_before, None);
        // ADR-0081 contract: tail entries carry origin = None; the
        // aggregator stamps it at merge time.
        assert_eq!(entries[0].origin, None);
    }

    #[test]
    fn tail_filters_below_min_level() {
        let mut ring = ActorLogRing::with_capacity(8);
        ring.push(0, "t".to_owned(), "trace".to_owned(), 0);
        ring.push(2, "t".to_owned(), "info".to_owned(), 0);
        ring.push(3, "t".to_owned(), "warn".to_owned(), 0);
        ring.push(4, "t".to_owned(), "error".to_owned(), 0);
        let LogTailResult::Ok { entries, .. } = ring.tail(&LogTail {
            max: 0,
            min_level: Some(3),
            since: None,
        }) else {
            panic!("expected Ok");
        };
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].message, "warn");
        assert_eq!(entries[1].message, "error");
    }

    #[test]
    fn tail_since_cursor_paginates_without_double_pulling() {
        let mut ring = ActorLogRing::with_capacity(8);
        for i in 1..=5 {
            ring.push(2, "t".to_owned(), format!("msg-{i}"), 0);
        }
        let LogTailResult::Ok {
            entries,
            next_since,
            ..
        } = ring.tail(&LogTail {
            max: 0,
            min_level: None,
            since: Some(2),
        })
        else {
            panic!("expected Ok");
        };
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].sequence, 3);
        assert_eq!(next_since, 5);

        let LogTailResult::Ok {
            entries,
            next_since,
            ..
        } = ring.tail(&LogTail {
            max: 0,
            min_level: None,
            since: Some(next_since),
        })
        else {
            panic!("expected Ok");
        };
        assert!(entries.is_empty());
        assert_eq!(next_since, 5);
    }

    #[test]
    fn tail_max_caps_returned_slice_and_cursor_walks_remainder() {
        let mut ring = ActorLogRing::with_capacity(8);
        for i in 1..=5 {
            ring.push(2, "t".to_owned(), format!("msg-{i}"), 0);
        }
        let LogTailResult::Ok {
            entries,
            next_since,
            ..
        } = ring.tail(&LogTail {
            max: 2,
            min_level: None,
            since: None,
        })
        else {
            panic!("expected Ok");
        };
        assert_eq!(entries.len(), 2);
        assert_eq!(next_since, 2);

        let LogTailResult::Ok {
            entries,
            next_since,
            ..
        } = ring.tail(&LogTail {
            max: 2,
            min_level: None,
            since: Some(next_since),
        })
        else {
            panic!("expected Ok");
        };
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].sequence, 3);
        assert_eq!(next_since, 4);
    }

    /// FIFO eviction with a 3-entry ring drops seq 1 + 2 once the
    /// 5th entry pushes; a reader with `since = 0` (expecting the
    /// prefix) sees `truncated_before = Some(3)`.
    #[test]
    fn tail_signals_truncated_before_when_ring_evicted_prefix() {
        let mut ring = ActorLogRing::with_capacity(3);
        for i in 1..=5 {
            ring.push(2, "t".to_owned(), format!("msg-{i}"), 0);
        }
        let LogTailResult::Ok {
            entries,
            truncated_before,
            ..
        } = ring.tail(&LogTail {
            max: 0,
            min_level: None,
            since: None,
        })
        else {
            panic!("expected Ok");
        };
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].sequence, 3);
        assert_eq!(truncated_before, Some(3));
    }

    #[test]
    fn tail_does_not_signal_truncated_before_when_since_covers_eviction() {
        let mut ring = ActorLogRing::with_capacity(3);
        for i in 1..=5 {
            ring.push(2, "t".to_owned(), format!("msg-{i}"), 0);
        }
        let LogTailResult::Ok {
            truncated_before, ..
        } = ring.tail(&LogTail {
            max: 0,
            min_level: None,
            since: Some(2),
        })
        else {
            panic!("expected Ok");
        };
        assert_eq!(truncated_before, None);
    }

    /// `snapshot()` returns every entry currently in the ring with
    /// no filter, oldest-to-newest. Used by the panic hook (ADR-0081
    /// §4).
    #[test]
    fn snapshot_returns_current_entries_oldest_first() {
        let mut ring = ActorLogRing::with_capacity(8);
        push_three(&mut ring);
        let snap = ring.snapshot();
        assert_eq!(snap.len(), 3);
        assert_eq!(snap[0].sequence, 1);
        assert_eq!(snap[2].sequence, 3);
    }

    #[test]
    fn zero_cap_is_clamped_to_one() {
        let mut ring = ActorLogRing::with_capacity(0);
        ring.push(2, "t".to_owned(), "only".to_owned(), 0);
        assert_eq!(ring.len(), 1);
    }
}
