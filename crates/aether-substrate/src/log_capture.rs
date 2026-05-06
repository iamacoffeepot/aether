// ADR-0023 substrate-side log capture. Installs a tracing-subscriber
// `Layer` that captures formatted events into a bounded ring and a
// background thread that drains the ring into `EngineToHub::LogBatch`
// frames via the existing `HubOutbound`. Capture is additive — the
// `init` function also installs a stderr `fmt` layer so the existing
// console output behaviour (what `eprintln!` used to give) is
// preserved for operators reading the substrate's terminal.
//
// Filter: `AETHER_LOG_FILTER` (standard `EnvFilter` syntax) overrides
// the INFO+ default. Unset = INFO+.
//
// Bounds: per-line cap 16 KiB (truncated with `...[truncated]`),
// ring cap 2_000 entries / 2 MiB total. Overflow drops oldest and
// surfaces a synthetic `Warn` entry at the next flush so the loss is
// observable.
//
// The flush task is a `std::thread` per the substrate's stay-sync
// stance (see `hub_client.rs`). Wakeup is a `Condvar` poked when the
// ring crosses the batch threshold; otherwise the timer fires every
// 250 ms.
//
// Issue #583: [`emit_decoded`] is a direct-emit path for already-
// decoded `aether.log` mail that bypasses `tracing::dispatcher`
// entirely — it writes to stderr and pushes the ring without going
// back through `log::log!()` → `tracing-log` → the global subscriber.
// `LogCapability` calls it instead of routing through the dispatcher
// so the actor-aware subscriber issue #581 will install can't catch
// the cap's own emissions and recurse back into the cap.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::outbound::{LogEntry, LogLevel};
use aether_kinds::LogEvent;
use tracing::level_filters::LevelFilter;
use tracing::{Event, Subscriber};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::field::Visit;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{Layer, fmt as tsfmt};

use crate::outbound::HubOutbound;

/// Per-event message cap before truncation.
const MESSAGE_CAP: usize = 16 * 1024;
const TRUNCATED_SUFFIX: &str = "...[truncated]";

/// Default ring caps. Mirror the values quoted in ADR-0023.
const DEFAULT_RING_ENTRIES: usize = 2_000;
const DEFAULT_RING_BYTES: usize = 2 * 1024 * 1024;

/// Flush triggers. Whichever fires first.
const BATCH_TRIGGER_ENTRIES: usize = 100;
const BATCH_TRIGGER_INTERVAL: Duration = Duration::from_millis(250);

/// Env var name for overriding the default filter.
const FILTER_ENV: &str = "AETHER_LOG_FILTER";

/// Process-lifetime handle to the capture ring, populated by `init`.
/// `flush_now` reads this so `lifecycle::fatal_abort` can drain the
/// ring synchronously before the substrate exits (ADR-0063); the
/// background flush thread on a 250 ms timer can't be relied on when
/// the process is about to call `std::process::exit`.
static CAPTURE: OnceLock<Arc<Inner>> = OnceLock::new();

/// Install the global tracing subscriber and spawn the flush thread.
///
/// `outbound` is the same handle the hub client populates; this lets
/// the capture layer drop its frames silently when no hub is attached
/// rather than buffering forever. The flush thread keeps running for
/// the process lifetime — there's no clean handoff because the global
/// subscriber installation is also process-lifetime.
///
/// Returns silently on the second call (a global subscriber was
/// already installed); intended to be called exactly once during
/// substrate boot.
pub fn init(outbound: Arc<HubOutbound>) {
    let filter = EnvFilter::try_from_env(FILTER_ENV).unwrap_or_else(|_| EnvFilter::new("info"));
    // Issue #583: derive the direct-emit threshold from the same env
    // before the filter is moved into the registry. `max_level_hint`
    // is the most-permissive level any directive in the env enables;
    // per-target narrowing (e.g. `wgpu=warn`) intentionally widens
    // rather than narrows the direct path — guests rarely match
    // operator tuning targets and a target-aware filter would couple
    // this path back to `tracing::Metadata`'s `&'static str` target
    // requirement.
    let direct_min_level = level_filter_to_log_level(filter.max_level_hint());

    let inner = Arc::new(Inner::new(outbound, direct_min_level));
    let capture = CaptureLayer {
        inner: Arc::clone(&inner),
    };
    let stderr_fmt = tsfmt::layer().with_writer(std::io::stderr);

    let registered = tracing_subscriber::registry()
        .with(filter)
        .with(stderr_fmt)
        .with(capture)
        .try_init()
        .is_ok();

    if registered {
        // Publish the ring handle for `flush_now`. Set on first init
        // only; subsequent inits (chassis re-entry / tests) are no-ops
        // because `try_init` already returned false above.
        let _ = CAPTURE.set(Arc::clone(&inner));

        let flusher = Arc::clone(&inner);
        thread::Builder::new()
            .name("aether-log-flush".into())
            .spawn(move || flush_loop(flusher))
            .expect("spawn flush thread");
    }
}

/// Synchronously drain the capture ring and push it through the
/// configured `HubOutbound` on the calling thread. Used by
/// `lifecycle::fatal_abort` (ADR-0063) immediately before
/// `std::process::exit` so the abort log lands in `engine_logs`
/// instead of being lost with the process.
///
/// No-op if `init` was never called (e.g. unit tests that don't boot
/// the global subscriber). Safe to call from any thread.
pub fn flush_now() {
    if let Some(inner) = CAPTURE.get() {
        flush_into(inner);
    }
}

/// Direct-emit path for already-decoded `aether.log` mail (issue #583).
///
/// Bypasses [`tracing::dispatcher`] entirely. The event is filtered
/// against the same `AETHER_LOG_FILTER` env that gates the global
/// `EnvFilter` (see [`init`]), then written:
///
/// 1. to `stderr` in `[unix_ms] LEVEL target: message` form, and
/// 2. into the engine_logs ring via [`Inner::push`] — same rotation
///    and per-line cap as host-emitted tracing events.
///
/// This is the substrate-side hook
/// [`crate::capabilities::log::LogCapability`] calls so guest log
/// events stop round-tripping through `log::log!()` →
/// `tracing-log` → the global subscriber. Issue #581's actor-aware
/// subscriber would otherwise catch the cap's own emissions and
/// recurse back into the cap.
///
/// Filter handling: `AETHER_LOG_FILTER`'s most-permissive directive
/// level (computed at `init` via [`EnvFilter::max_level_hint`]) is
/// the threshold. Per-target narrowing rules in the env (e.g.
/// `wgpu=warn`) widen rather than narrow this path — see the comment
/// in [`init`] for the rationale.
///
/// No-op if [`init`] hasn't been called (no global subscriber, no
/// ring). Safe to call from any thread.
pub fn emit_decoded(event: LogEvent) {
    if let Some(inner) = CAPTURE.get() {
        emit_decoded_into(inner, event);
    }
}

/// Inner half of [`emit_decoded`] — does the level map + filter +
/// stderr write + ring push against an explicit `Inner`. Pulled out
/// the same way [`flush_into`] is, so tests can drive the path
/// against a non-global ring without racing on `CAPTURE`'s
/// once-per-process initialisation.
fn emit_decoded_into(inner: &Inner, event: LogEvent) {
    let level = log_event_level_to_log_level(event.level);
    let Some(min) = inner.direct_min_level else {
        return;
    };
    if level < min {
        return;
    }
    write_console_line(level, &event.target, &event.message);
    inner.push(level, event.target, event.message);
}

/// Map `LogEvent.level` (the wire u8 from `aether-kinds`) onto the
/// substrate's [`LogLevel`]. Out-of-range values fold to `Info` —
/// matches the historical `log_sink::handle_log_mail_decoded`
/// disposition before issue #583 retired that path.
fn log_event_level_to_log_level(level: u8) -> LogLevel {
    match level {
        0 => LogLevel::Trace,
        1 => LogLevel::Debug,
        2 => LogLevel::Info,
        3 => LogLevel::Warn,
        4 => LogLevel::Error,
        _ => LogLevel::Info,
    }
}

/// Project an `EnvFilter::max_level_hint` result onto our `LogLevel`.
/// `None` (no hint, treat as INFO+) and `Some(LevelFilter::OFF)`
/// (filter is off, drop everything) collapse to the natural
/// representations for [`emit_decoded`].
fn level_filter_to_log_level(hint: Option<LevelFilter>) -> Option<LogLevel> {
    match hint {
        // No directive at all -> match the `init` fallback `info`.
        None => Some(LogLevel::Info),
        Some(LevelFilter::OFF) => None,
        Some(LevelFilter::ERROR) => Some(LogLevel::Error),
        Some(LevelFilter::WARN) => Some(LogLevel::Warn),
        Some(LevelFilter::INFO) => Some(LogLevel::Info),
        Some(LevelFilter::DEBUG) => Some(LogLevel::Debug),
        Some(LevelFilter::TRACE) => Some(LogLevel::Trace),
    }
}

/// Console emitter for [`emit_decoded`]. Writes one line to `stderr`
/// in `[unix_ms] LEVEL target: message` form. Format is intentionally
/// distinct from the registered `tsfmt::Layer` (no ANSI, no full
/// RFC3339 timestamp); operators reading the substrate terminal still
/// see guest log events, and the structured wire shape over
/// `engine_logs` (`LogEntry.timestamp_unix_ms`) is unchanged.
fn write_console_line(level: LogLevel, target: &str, message: &str) {
    use std::io::Write as _;
    let level_str = match level {
        LogLevel::Trace => "TRACE",
        LogLevel::Debug => "DEBUG",
        LogLevel::Info => "INFO ",
        LogLevel::Warn => "WARN ",
        LogLevel::Error => "ERROR",
    };
    // `lock()` keeps the write atomic against concurrent emissions;
    // ignore `Err` (a closed stderr is not an event we report).
    let _ = writeln!(
        std::io::stderr().lock(),
        "[{}] {} {}: {}",
        now_unix_ms(),
        level_str,
        target,
        message,
    );
}

/// Inner half of `flush_now` — does the synchronous take + send
/// against an explicit `Inner`. Pulled out so tests can drive the
/// drain path against a test channel without touching the
/// process-global `CAPTURE` static (which `init` populates exactly
/// once for the substrate's lifetime).
fn flush_into(inner: &Inner) {
    let batch = {
        let mut s = inner.state.lock().unwrap();
        take_batch(&mut s)
    };
    if !batch.is_empty() {
        inner.outbound.egress_log_batch(batch);
    }
}

struct Inner {
    state: Mutex<RingState>,
    cv: Condvar,
    outbound: Arc<HubOutbound>,
    shutdown: AtomicBool,
    /// Issue #583: minimum level that survives [`emit_decoded`].
    /// `Some(level)` drops events below `level`; `None` (filter == OFF)
    /// drops every event. Computed once at [`init`] from the same
    /// `AETHER_LOG_FILTER` env that drives the global `EnvFilter`.
    direct_min_level: Option<LogLevel>,
}

struct RingState {
    entries: VecDeque<LogEntry>,
    next_sequence: u64,
    dropped_since_last_flush: u64,
    current_bytes: usize,
}

impl Inner {
    fn new(outbound: Arc<HubOutbound>, direct_min_level: Option<LogLevel>) -> Self {
        Self {
            state: Mutex::new(RingState {
                entries: VecDeque::new(),
                // Sequences start at 1 so a `since=0` (the natural
                // first-poll default at the MCP tool) returns the
                // very first captured entry instead of missing it.
                next_sequence: 1,
                dropped_since_last_flush: 0,
                current_bytes: 0,
            }),
            cv: Condvar::new(),
            outbound,
            shutdown: AtomicBool::new(false),
            direct_min_level,
        }
    }

    fn push(&self, level: LogLevel, target: String, message: String) {
        let mut s = self.state.lock().unwrap();
        let seq = s.next_sequence;
        s.next_sequence = s.next_sequence.wrapping_add(1);
        let entry = LogEntry {
            timestamp_unix_ms: now_unix_ms(),
            level,
            target,
            message,
            sequence: seq,
        };
        let entry_bytes = entry_size(&entry);
        s.current_bytes = s.current_bytes.saturating_add(entry_bytes);
        s.entries.push_back(entry);
        while s.entries.len() > DEFAULT_RING_ENTRIES || s.current_bytes > DEFAULT_RING_BYTES {
            let Some(dropped) = s.entries.pop_front() else {
                break;
            };
            s.current_bytes = s.current_bytes.saturating_sub(entry_size(&dropped));
            s.dropped_since_last_flush = s.dropped_since_last_flush.saturating_add(1);
        }
        if s.entries.len() >= BATCH_TRIGGER_ENTRIES || s.dropped_since_last_flush > 0 {
            self.cv.notify_one();
        }
    }
}

fn entry_size(entry: &LogEntry) -> usize {
    // Approximate; tracks only the variable-length parts. Constant
    // per-entry overhead (timestamps, level, sequence) is not counted
    // since the ring cap is a guard against runaway message volume,
    // not a precise wire-size accounting.
    entry.target.len() + entry.message.len()
}

fn flush_loop(inner: Arc<Inner>) {
    while !inner.shutdown.load(Ordering::Acquire) {
        let batch = {
            let mut s = inner.state.lock().unwrap();
            while s.entries.is_empty() && s.dropped_since_last_flush == 0 {
                let (next, timeout) = inner
                    .cv
                    .wait_timeout(s, BATCH_TRIGGER_INTERVAL)
                    .expect("flush condvar poisoned");
                s = next;
                if timeout.timed_out() {
                    break;
                }
                if inner.shutdown.load(Ordering::Acquire) {
                    return;
                }
            }
            take_batch(&mut s)
        };
        if !batch.is_empty() {
            inner.outbound.egress_log_batch(batch);
        }
    }
}

fn take_batch(s: &mut RingState) -> Vec<LogEntry> {
    let mut out: Vec<LogEntry> = Vec::with_capacity(s.entries.len() + 1);
    if s.dropped_since_last_flush > 0 {
        let dropped = std::mem::take(&mut s.dropped_since_last_flush);
        let seq = s.next_sequence;
        s.next_sequence = s.next_sequence.wrapping_add(1);
        out.push(LogEntry {
            timestamp_unix_ms: now_unix_ms(),
            level: LogLevel::Warn,
            target: "aether_substrate::log_capture".into(),
            message: format!("dropped {dropped} entries (capture ring full)"),
            sequence: seq,
        });
    }
    let drained: Vec<LogEntry> = s.entries.drain(..).collect();
    s.current_bytes = 0;
    out.extend(drained);
    out
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

struct CaptureLayer {
    inner: Arc<Inner>,
}

impl<S> Layer<S> for CaptureLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let metadata = event.metadata();
        let level = match *metadata.level() {
            tracing::Level::TRACE => LogLevel::Trace,
            tracing::Level::DEBUG => LogLevel::Debug,
            tracing::Level::INFO => LogLevel::Info,
            tracing::Level::WARN => LogLevel::Warn,
            tracing::Level::ERROR => LogLevel::Error,
        };
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        let message = visitor.finish();
        self.inner
            .push(level, metadata.target().to_owned(), message);
    }
}

#[derive(Default)]
struct MessageVisitor {
    /// Primary `message` field (tracing's implicit format-string slot).
    message: String,
    /// Other structured fields, flattened as `name=value` pairs.
    rest: String,
}

impl MessageVisitor {
    fn finish(self) -> String {
        let mut out = self.message;
        if !self.rest.is_empty() {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(&self.rest);
        }
        truncate_in_place(out)
    }
}

fn truncate_in_place(mut s: String) -> String {
    if s.len() > MESSAGE_CAP {
        let cut = MESSAGE_CAP.saturating_sub(TRUNCATED_SUFFIX.len());
        // Step back to a char boundary so the resulting `String` stays
        // valid UTF-8 even if the cap fell mid-codepoint.
        let mut boundary = cut.min(s.len());
        while boundary > 0 && !s.is_char_boundary(boundary) {
            boundary -= 1;
        }
        s.truncate(boundary);
        s.push_str(TRUNCATED_SUFFIX);
    }
    s
}

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(&mut self.message, "{value:?}");
        } else {
            if !self.rest.is_empty() {
                self.rest.push(' ');
            }
            let _ = write!(&mut self.rest, "{}={value:?}", field.name());
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.message.push_str(value);
        } else {
            if !self.rest.is_empty() {
                self.rest.push(' ');
            }
            let _ = write!(&mut self.rest, "{}={value:?}", field.name());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outbound::EgressEvent;

    fn make_inner() -> (Arc<Inner>, std::sync::mpsc::Receiver<EgressEvent>) {
        // Tests that don't need the direct-emit threshold tuned default to
        // INFO+ — same as `init`'s fallback when `AETHER_LOG_FILTER` is unset.
        make_inner_with_min(Some(LogLevel::Info))
    }

    fn make_inner_with_min(
        min: Option<LogLevel>,
    ) -> (Arc<Inner>, std::sync::mpsc::Receiver<EgressEvent>) {
        let (outbound, rx) = crate::outbound::HubOutbound::attached_loopback();
        (Arc::new(Inner::new(outbound, min)), rx)
    }

    #[test]
    fn push_assigns_monotonic_sequence() {
        let (inner, _rx) = make_inner();
        inner.push(LogLevel::Info, "t".into(), "first".into());
        inner.push(LogLevel::Info, "t".into(), "second".into());
        let s = inner.state.lock().unwrap();
        assert_eq!(s.entries[0].sequence, 1);
        assert_eq!(s.entries[1].sequence, 2);
        assert_eq!(s.next_sequence, 3);
    }

    #[test]
    fn ring_evicts_oldest_at_entry_cap() {
        let (inner, _rx) = make_inner();
        for i in 0..(DEFAULT_RING_ENTRIES + 5) {
            inner.push(LogLevel::Info, "t".into(), format!("msg-{i}"));
        }
        let s = inner.state.lock().unwrap();
        assert_eq!(s.entries.len(), DEFAULT_RING_ENTRIES);
        assert_eq!(s.dropped_since_last_flush, 5);
        // Sequences start at 1; first 5 (seq 1..=5) evicted, so the
        // oldest surviving entry has sequence 6.
        assert_eq!(s.entries.front().unwrap().sequence, 6);
    }

    #[test]
    fn take_batch_prepends_drop_warning() {
        let (inner, _rx) = make_inner();
        // Force a drop without churning thousands of pushes.
        {
            let mut s = inner.state.lock().unwrap();
            s.dropped_since_last_flush = 7;
        }
        inner.push(LogLevel::Info, "t".into(), "kept".into());
        let mut s = inner.state.lock().unwrap();
        let batch = take_batch(&mut s);
        assert_eq!(batch.len(), 2);
        assert_eq!(batch[0].level, LogLevel::Warn);
        assert!(batch[0].message.contains("dropped 7 entries"));
        assert_eq!(batch[1].message, "kept");
        // Counter resets after flush.
        assert_eq!(s.dropped_since_last_flush, 0);
    }

    /// ADR-0063: `flush_into` (the inner half of `flush_now`) drains
    /// the ring on the calling thread — no background flusher
    /// involvement, so a `fatal_abort` flushing immediately before
    /// `process::exit` actually lands the abort log on the engine
    /// TCP. Subsequent flushes are no-ops because the ring is empty.
    #[test]
    fn flush_into_drains_synchronously_on_calling_thread() {
        let (inner, rx) = make_inner();
        inner.push(LogLevel::Error, "lifecycle".into(), "abort msg".into());
        inner.push(LogLevel::Warn, "lifecycle".into(), "secondary".into());

        flush_into(&inner);

        let event = rx.try_recv().expect("flush_into should send a batch");
        let EgressEvent::LogBatch { entries: batch } = event else {
            panic!("unexpected egress variant: {event:?}");
        };
        assert_eq!(batch.len(), 2);
        assert_eq!(batch[0].message, "abort msg");
        assert_eq!(batch[1].message, "secondary");

        // Re-flushing an empty ring is a no-op — no extra frame on
        // the channel, ring stays empty.
        flush_into(&inner);
        assert!(rx.try_recv().is_err());
        assert_eq!(inner.state.lock().unwrap().entries.len(), 0);
    }

    #[test]
    fn message_truncation_preserves_utf8_and_marker() {
        let huge = "ä".repeat(MESSAGE_CAP);
        let truncated = truncate_in_place(huge);
        assert!(truncated.len() <= MESSAGE_CAP);
        assert!(truncated.ends_with(TRUNCATED_SUFFIX));
        // String is still valid UTF-8 (no multibyte char half-cut).
        assert!(std::str::from_utf8(truncated.as_bytes()).is_ok());
    }

    /// Issue #583: the direct-emit path lands a ring entry whose
    /// target / level / message mirror the input `LogEvent` exactly.
    /// This is the equivalence we care about with the retired
    /// `log_sink::handle_log_mail_decoded` → `log::log!()` →
    /// `tracing-log` round-trip — that path also produced an entry
    /// where target / message survived verbatim and `level` mapped
    /// 0..=4 onto Trace..=Error.
    #[test]
    fn emit_decoded_writes_decoded_fields_into_ring() {
        let (inner, _rx) = make_inner_with_min(Some(LogLevel::Trace));
        let event = LogEvent {
            level: 3,
            target: "aether_test_guest".into(),
            message: "parse failed: missing close paren".into(),
        };
        emit_decoded_into(&inner, event);
        let s = inner.state.lock().unwrap();
        assert_eq!(s.entries.len(), 1);
        let entry = &s.entries[0];
        assert_eq!(entry.level, LogLevel::Warn);
        assert_eq!(entry.target, "aether_test_guest");
        assert_eq!(entry.message, "parse failed: missing close paren");
    }

    /// Each `LogEvent.level` 0..=4 maps onto the matching `LogLevel`.
    /// Out-of-range bytes fall back to `Info` — preserves the parity
    /// the old `log_sink::handle_log_mail_decoded` warn-and-treat-as-Info
    /// branch enforced.
    #[test]
    fn emit_decoded_level_mapping_matches_old_log_sink() {
        for (in_byte, want) in [
            (0u8, LogLevel::Trace),
            (1, LogLevel::Debug),
            (2, LogLevel::Info),
            (3, LogLevel::Warn),
            (4, LogLevel::Error),
            (255, LogLevel::Info),
        ] {
            let (inner, _rx) = make_inner_with_min(Some(LogLevel::Trace));
            emit_decoded_into(
                &inner,
                LogEvent {
                    level: in_byte,
                    target: "t".into(),
                    message: "m".into(),
                },
            );
            let s = inner.state.lock().unwrap();
            assert_eq!(s.entries.len(), 1, "level {in_byte}");
            assert_eq!(s.entries[0].level, want, "level {in_byte}");
        }
    }

    /// Filter floor is enforced: with the threshold at WARN, an Info
    /// event leaves the ring untouched. Mirrors the pre-#583 EnvFilter
    /// behaviour where `AETHER_LOG_FILTER=warn` would have dropped the
    /// event before it reached `CaptureLayer`.
    #[test]
    fn emit_decoded_drops_below_min_level() {
        let (inner, _rx) = make_inner_with_min(Some(LogLevel::Warn));
        emit_decoded_into(
            &inner,
            LogEvent {
                level: 2,
                target: "t".into(),
                message: "info-msg".into(),
            },
        );
        emit_decoded_into(
            &inner,
            LogEvent {
                level: 4,
                target: "t".into(),
                message: "error-msg".into(),
            },
        );
        let s = inner.state.lock().unwrap();
        assert_eq!(s.entries.len(), 1);
        assert_eq!(s.entries[0].message, "error-msg");
    }

    /// `direct_min_level == None` represents `LevelFilter::OFF` — the
    /// direct path drops every event so `engine_logs` matches the
    /// EnvFilter-off behaviour.
    #[test]
    fn emit_decoded_drops_everything_when_filter_off() {
        let (inner, _rx) = make_inner_with_min(None);
        emit_decoded_into(
            &inner,
            LogEvent {
                level: 4,
                target: "t".into(),
                message: "e".into(),
            },
        );
        assert!(inner.state.lock().unwrap().entries.is_empty());
    }

    /// `EnvFilter::max_level_hint`'s shape projects onto our
    /// `Option<LogLevel>` 1:1. Unset env (no hint) defaults to INFO+
    /// — matches `init`'s `unwrap_or_else(|_| EnvFilter::new("info"))`
    /// fallback.
    #[test]
    fn level_filter_to_log_level_projects_envfilter_shape() {
        assert_eq!(level_filter_to_log_level(None), Some(LogLevel::Info));
        assert_eq!(level_filter_to_log_level(Some(LevelFilter::OFF)), None);
        assert_eq!(
            level_filter_to_log_level(Some(LevelFilter::ERROR)),
            Some(LogLevel::Error),
        );
        assert_eq!(
            level_filter_to_log_level(Some(LevelFilter::WARN)),
            Some(LogLevel::Warn),
        );
        assert_eq!(
            level_filter_to_log_level(Some(LevelFilter::INFO)),
            Some(LogLevel::Info),
        );
        assert_eq!(
            level_filter_to_log_level(Some(LevelFilter::DEBUG)),
            Some(LogLevel::Debug),
        );
        assert_eq!(
            level_filter_to_log_level(Some(LevelFilter::TRACE)),
            Some(LogLevel::Trace),
        );
    }
}
