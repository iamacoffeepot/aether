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

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::outbound::{LogEntry, LogLevel};
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

    let inner = Arc::new(Inner::new(outbound));
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
}

struct RingState {
    entries: VecDeque<LogEntry>,
    next_sequence: u64,
    dropped_since_last_flush: u64,
    current_bytes: usize,
}

impl Inner {
    fn new(outbound: Arc<HubOutbound>) -> Self {
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
        let (outbound, rx) = crate::outbound::HubOutbound::attached_loopback();
        (Arc::new(Inner::new(outbound)), rx)
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
}
