// Substrate panic-hook plumbing.
//
// Two responsibilities, layered in order:
//
// 1. **Tracing event.** Routes panics through `tracing::error!` so
//    the panicking actor's own `ActorLogRing` (ADR-0081 §1) captures
//    the panic in-band — `engine_logs` callers see the panic the
//    same way they see any other event from that actor. Without
//    this, panics on dispatcher threads (`std::thread::Builder::spawn`
//    in `scheduler.rs`) print to stderr only.
//
// 2. **JSONL crash dump (ADR-0081 §4).** Synchronously reads the
//    panicking actor's `ActorLogRing` snapshot, writes a panic
//    header + ring contents to `<crash-dir>/<unix_ms>/<thread>.jsonl`
//    before the process tears down. Restores the post-mortem
//    retention property ADR-0023 §3 originally committed to (gone
//    silently after iamacoffeepot/aether#776 retired the hub-side
//    store). Closes iamacoffeepot/aether#825.
//
// Chains the previous hook (`take_hook` -> closure-wraps it ->
// `set_hook`) so the test harness's panic formatter, color_eyre /
// human_panic prettifiers, and any embedder hook keep working. The
// runtime only ever invokes one hook per panic; chaining is a manual
// pattern, not a runtime feature.
//
// Backtrace capture is gated on `AETHER_BACKTRACE` (mirrors the
// `RUST_BACKTRACE` ergonomic without flipping the global Rust knob).
// Falls through to `Backtrace::capture()` otherwise, which respects
// `RUST_BACKTRACE` if set — so existing toolchain conventions still
// work.

use std::any::Any;
use std::backtrace::{Backtrace, BacktraceStatus};
use std::env;
use std::fs;
use std::io;
use std::io::Write as _;
use std::panic::{self, PanicHookInfo};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use aether_actor::Local;
use aether_actor::log::ActorLogRing;
use aether_kinds::LogEntry;

/// Env var that forces backtrace capture without flipping
/// `RUST_BACKTRACE` for the whole process (which would change every
/// other crate's panic output).
pub const ENV_BACKTRACE: &str = "AETHER_BACKTRACE";

/// Disables the JSONL crash-dump path entirely. Anything truthy
/// (`"1"`, `"true"`, any non-empty value other than `"0"` / `"false"`)
/// skips the file write; the tracing event still fires.
pub const ENV_CRASH_LOG_DISABLE: &str = "AETHER_CRASH_LOG_DISABLE";

/// Override the crash-dump base directory. Each crash gets a
/// `<base>/<unix_ms>/` subdirectory; the panicking actor's ring
/// lands at `<base>/<unix_ms>/<thread>.jsonl`. Unset → default per
/// ADR-0081 §4 (`$XDG_DATA_HOME/aether/crash/`, falling back to
/// `$HOME/.local/share/aether/crash/`).
pub const ENV_CRASH_LOG_DIR: &str = "AETHER_CRASH_LOG_DIR";

static INIT: OnceLock<()> = OnceLock::new();

/// Install the substrate panic hook globally. Idempotent — only the
/// first call wires the hook; subsequent calls are no-ops, so chassis
/// boot, tests, and embedders can call freely without ordering
/// constraints.
///
/// Call early in process startup (substrate `boot::build` is the
/// canonical site). Installing later is fine — the hook only fires on
/// future panics — but earlier means more of the process is covered.
pub fn init_panic_hook() {
    INIT.get_or_init(|| {
        let prev = panic::take_hook();
        panic::set_hook(make_hook(prev));
    });
}

/// Wrap `prev` so the returned hook emits a structured tracing event
/// before invoking `prev`. Factored out of `init_panic_hook` so the
/// chaining and event-emit mechanics are testable without touching
/// the global hook slot.
fn make_hook(
    prev: Box<dyn Fn(&PanicHookInfo<'_>) + Sync + Send + 'static>,
) -> Box<dyn Fn(&PanicHookInfo<'_>) + Sync + Send + 'static> {
    Box::new(move |info| {
        // Order matters: snapshot the ring + write the JSONL dump
        // *before* the tracing event so the dump captures the
        // panicking actor's history without the panic event itself
        // appearing inside it (the event lands on the ring through
        // the `ActorAwareLayer` push). Both paths swallow their own
        // failures — a panic during the panic hook is the worst-
        // case-scenario; we never want to obscure the original
        // payload by panicking again here.
        let timestamp_unix_ms = now_unix_ms();
        let thread = thread::current();
        let thread_name = thread.name().unwrap_or("<unnamed>").to_owned();
        let location = info.location().map_or_else(
            || "<unknown>".to_string(),
            |l| format!("{}:{}:{}", l.file(), l.line(), l.column()),
        );
        let payload = payload_string(info.payload());
        let backtrace = capture_backtrace();

        write_crash_dump(
            timestamp_unix_ms,
            &thread_name,
            &location,
            &payload,
            backtrace.as_ref(),
        );
        emit_event(&thread_name, &location, &payload, backtrace.as_ref());
        prev(info);
    })
}

fn emit_event(thread_name: &str, location: &str, payload: &str, backtrace: Option<&Backtrace>) {
    // The current `tracing::Span` is auto-captured by the dispatcher
    // (subscriber-side concern), so component / mailbox / kind ride
    // along when the panic happens inside the dispatch loop without
    // having to thread them in here.
    if let Some(bt) = backtrace {
        tracing::error!(
            target: "aether_substrate::panic",
            thread = %thread_name,
            location = %location,
            payload = %payload,
            backtrace = %bt,
            "panic on substrate thread",
        );
    } else {
        tracing::error!(
            target: "aether_substrate::panic",
            thread = %thread_name,
            location = %location,
            payload = %payload,
            "panic on substrate thread",
        );
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| {
        #[allow(clippy::cast_possible_truncation)]
        let ms = d.as_millis() as u64;
        ms
    })
}

/// ADR-0081 §4 crash dump. Reads the panicking actor's
/// [`ActorLogRing`] snapshot via the thread-local `ActorSlots` stamp
/// the dispatch loop opened, writes a panic header line followed by
/// one JSONL record per ring entry to `<crash-dir>/<unix_ms>/<thread>.jsonl`.
///
/// Best-effort: any failure (env-var-unparseable, no ring stamped,
/// fs write failure) silently returns. The panic itself is still
/// surfaced via the tracing event that follows; the JSONL file is
/// the *forensic* surface, not the primary signal.
///
/// Skipped entirely when `AETHER_CRASH_LOG_DISABLE` is truthy.
fn write_crash_dump(
    timestamp_unix_ms: u64,
    thread_name: &str,
    location: &str,
    payload: &str,
    backtrace: Option<&Backtrace>,
) {
    if crash_log_disabled() {
        return;
    }
    let Some(dir) = resolve_crash_dir(timestamp_unix_ms) else {
        return;
    };
    if let Err(e) = fs::create_dir_all(&dir) {
        // Stderr only — the panic itself is the load-bearing
        // signal; failure to write the forensic dump should not
        // obscure it with extra noise on the normal logging path.
        let _ = writeln!(
            io::stderr(),
            "aether-substrate: failed to create crash dir {}: {e}",
            dir.display(),
        );
        return;
    }
    let path = dir.join(format!("{}.jsonl", sanitize_filename(thread_name)));
    // Snapshot the ring on the panicking thread — `try_with` reads
    // the actor's `ActorSlots` stamp, which is live for the
    // duration of the dispatch loop's `local::with_stamped`. Out-of-
    // actor panics (substrate boot, scheduler init) see `None` and
    // we write only the header.
    let ring = ActorLogRing::try_with(ActorLogRing::snapshot);
    let backtrace_text = backtrace
        .map(|bt| matches!(bt.status(), BacktraceStatus::Captured).then(|| format!("{bt}")))
        .and_then(|opt| opt);
    if let Err(e) = write_jsonl(
        &path,
        timestamp_unix_ms,
        thread_name,
        location,
        payload,
        backtrace_text.as_deref(),
        ring.as_deref(),
    ) {
        let _ = writeln!(
            io::stderr(),
            "aether-substrate: failed to write crash dump {}: {e}",
            path.display(),
        );
    }
}

fn crash_log_disabled() -> bool {
    env::var(ENV_CRASH_LOG_DISABLE).is_ok_and(|v| {
        let v = v.trim();
        !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false")
    })
}

/// `$AETHER_CRASH_LOG_DIR/<unix_ms>/` if set; else
/// `$XDG_DATA_HOME/aether/crash/<unix_ms>/`; else
/// `$HOME/.local/share/aether/crash/<unix_ms>/`. `None` only when
/// neither override nor `$HOME` is available (rare — typically
/// containerised env with neither set).
fn resolve_crash_dir(timestamp_unix_ms: u64) -> Option<PathBuf> {
    let base = if let Ok(dir) = env::var(ENV_CRASH_LOG_DIR) {
        PathBuf::from(dir)
    } else if let Ok(xdg) = env::var("XDG_DATA_HOME") {
        PathBuf::from(xdg).join("aether").join("crash")
    } else {
        let home = env::var("HOME").ok()?;
        PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("aether")
            .join("crash")
    };
    Some(base.join(timestamp_unix_ms.to_string()))
}

/// Replace path-unfriendly characters in the thread name so it
/// survives as a filename component. Thread names in aether tend to
/// look like `aether-worker-2` (pool) or `aether.audio` (per-actor
/// Thread scheduling); only `:` shows up via the trampoline format
/// `aether.component.trampoline:NAME` and the dispatcher trims it
/// before naming the thread anyway. Keep the routine defensive for
/// future scheduler shapes.
fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '\0' => '-',
            c => c,
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn write_jsonl(
    path: &Path,
    timestamp_unix_ms: u64,
    thread_name: &str,
    location: &str,
    payload: &str,
    backtrace: Option<&str>,
    ring: Option<&[LogEntry]>,
) -> io::Result<()> {
    let mut file = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)?;

    // Header line — one JSON object capturing the panic context.
    let header = serde_json::json!({
        "kind": "panic",
        "timestamp_unix_ms": timestamp_unix_ms,
        "thread": thread_name,
        "location": location,
        "payload": payload,
        "backtrace": backtrace,
        "ring_entries": ring.map_or(0, <[LogEntry]>::len),
    });
    writeln!(file, "{header}")?;

    // Ring entries — one JSON object per line, in push order.
    if let Some(entries) = ring {
        for entry in entries {
            let line = serde_json::to_string(entry).map_err(io::Error::other)?;
            writeln!(file, "{line}")?;
        }
    }
    Ok(())
}

/// Stringify the panic payload. Std supports two common shapes —
/// `&'static str` (from `panic!("literal")`) and `String` (from
/// `panic!("{}", thing)`) — and falls back to a `TypeId` mention for
/// anything else. Mirrors the default hook's behaviour without
/// duplicating its full dance.
// Chained if-let on disjoint downcasts reads cleaner than a deep
// `map_or_else` ladder over two Options.
#[allow(clippy::option_if_let_else)]
fn payload_string(payload: &(dyn Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        format!("<non-string panic payload type_id={:?}>", payload.type_id())
    }
}

fn capture_backtrace() -> Option<Backtrace> {
    capture_backtrace_with(env::var_os(ENV_BACKTRACE).is_some())
}

/// Pure-fn factor of `capture_backtrace` so tests can exercise both
/// branches without racing on env-var mutations.
fn capture_backtrace_with(forced: bool) -> Option<Backtrace> {
    if forced {
        return Some(Backtrace::force_capture());
    }
    let bt = Backtrace::capture();
    matches!(bt.status(), BacktraceStatus::Captured).then_some(bt)
}

#[cfg(test)]
// Tests hold the capture `Mutex` guard across the assertion block so
// the event snapshot reads atomically against the panic hook's push.
#[allow(clippy::significant_drop_tightening)]
#[allow(
    clippy::unwrap_used,
    reason = "test-setup unwraps: backtrace capture and Mutex lock panic on failure is the assertion"
)]
mod tests {
    use std::fmt::Write as _;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use tracing::Subscriber;
    use tracing::field::{Field, Visit};

    use super::*;
    use std::any::Any;
    use std::fmt;
    use std::thread;
    use tracing::span::Attributes;
    use tracing::span::Id;
    use tracing::span::Record;
    use tracing::subscriber;

    /// `&'static str` payload (the common `panic!("literal")` case).
    #[test]
    fn payload_string_handles_static_str() {
        let payload: Box<dyn Any + Send> = Box::new("static reason");
        assert_eq!(payload_string(payload.as_ref()), "static reason");
    }

    /// `String` payload (the common `panic!("{}", x)` case).
    #[test]
    fn payload_string_handles_owned_string() {
        let payload: Box<dyn Any + Send> = Box::new(String::from("owned reason"));
        assert_eq!(payload_string(payload.as_ref()), "owned reason");
    }

    /// Anything else falls through to a `TypeId` mention so the message
    /// at least identifies the payload shape.
    #[test]
    fn payload_string_handles_unknown_type() {
        let payload: Box<dyn Any + Send> = Box::new(42i32);
        let out = payload_string(payload.as_ref());
        assert!(
            out.starts_with("<non-string panic payload"),
            "unexpected: {out}",
        );
    }

    /// Forced branch returns a captured backtrace regardless of env.
    #[test]
    fn capture_backtrace_with_forced_returns_some() {
        let bt = capture_backtrace_with(true);
        assert!(bt.is_some(), "forced=true must always capture");
        assert!(matches!(
            bt.unwrap().status(),
            BacktraceStatus::Captured | BacktraceStatus::Unsupported
        ));
    }

    /// Unforced branch defers to `Backtrace::capture()` which respects
    /// `RUST_BACKTRACE`. We don't assert on the result (the test env
    /// might or might not have it set) — we just verify the function
    /// doesn't panic and returns a Backtrace value of some shape.
    #[test]
    fn capture_backtrace_with_unforced_does_not_panic() {
        let _ = capture_backtrace_with(false);
    }

    /// `init_panic_hook` is idempotent — calling it many times must
    /// not double-install or panic. The `OnceLock` guard is the
    /// load-bearing piece; this test makes regressions loud.
    #[test]
    fn init_is_idempotent() {
        init_panic_hook();
        init_panic_hook();
        init_panic_hook();
    }

    /// The wrapped hook calls the previous hook exactly once per
    /// panic. Tests the chaining mechanic directly. `set_hook` is
    /// process-global, so other tests panicking concurrently could
    /// fire our sentinel too — we filter on `thread::current().id()`
    /// to count only panics from this test's thread, keeping the
    /// assertion stable under parallel test execution.
    #[test]
    fn make_hook_chains_to_previous() {
        let test_thread = thread::current().id();
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_for_prev = Arc::clone(&counter);
        let prev: Box<dyn Fn(&PanicHookInfo<'_>) + Sync + Send + 'static> =
            Box::new(move |_info| {
                if thread::current().id() == test_thread {
                    counter_for_prev.fetch_add(1, Ordering::SeqCst);
                }
            });
        let hook = make_hook(prev);

        // Drive the hook with a real PanicHookInfo by triggering a
        // panic inside `catch_unwind` while our hook is the global
        // hook. We have to install it (briefly) to get a real info.
        let saved = panic::take_hook();
        panic::set_hook(hook);
        let _ = panic::catch_unwind(|| {
            panic!("chain probe");
        });
        let _ = panic::take_hook();
        panic::set_hook(saved);

        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "previous hook must run exactly once per panic on this thread",
        );
    }

    /// End-to-end: installing the global hook, driving a panic,
    /// observing a structured tracing event with the expected fields.
    /// Uses a thread-local subscriber so the assertion only sees the
    /// panic this test triggered (other parallel tests' panics route
    /// to whatever subscriber is active on their threads).
    #[test]
    fn global_hook_emits_panic_event() {
        init_panic_hook();

        let captured: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let subscriber = CapturingSubscriber {
            events: Arc::clone(&captured),
        };

        subscriber::with_default(subscriber, || {
            let _ = panic::catch_unwind(|| {
                panic!("e2e probe 8e3a");
            });
        });

        let events = captured.lock().unwrap();
        let panic_events: Vec<&CapturedEvent> = events
            .iter()
            .filter(|e| e.target == "aether_substrate::panic")
            .collect();
        assert!(
            !panic_events.is_empty(),
            "no panic event captured (got {} non-panic events)",
            events.len(),
        );
        let merged: String = panic_events
            .iter()
            .map(|e| e.fields.as_str())
            .collect::<Vec<_>>()
            .join(" | ");
        assert!(
            merged.contains("e2e probe 8e3a"),
            "panic payload missing from event: {merged}",
        );
        assert!(
            merged.contains("location"),
            "location field missing from event: {merged}",
        );
        assert!(
            merged.contains("thread"),
            "thread field missing from event: {merged}",
        );
    }

    struct CapturedEvent {
        target: String,
        fields: String,
    }

    /// Minimal in-tree tracing subscriber that captures event target +
    /// formatted fields into a Vec for test assertions. Avoids pulling
    /// `tracing-test` as a dev-dep just to do this one thing.
    struct CapturingSubscriber {
        events: Arc<Mutex<Vec<CapturedEvent>>>,
    }

    impl Subscriber for CapturingSubscriber {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &Attributes<'_>) -> Id {
            Id::from_u64(1)
        }
        fn record(&self, _: &Id, _: &Record<'_>) {}
        fn record_follows_from(&self, _: &Id, _: &Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            let mut buf = String::new();
            let mut visitor = StringVisit(&mut buf);
            event.record(&mut visitor);
            self.events.lock().unwrap().push(CapturedEvent {
                target: event.metadata().target().to_string(),
                fields: buf,
            });
        }
        fn enter(&self, _: &Id) {}
        fn exit(&self, _: &Id) {}
    }

    struct StringVisit<'a>(&'a mut String);

    impl Visit for StringVisit<'_> {
        fn record_str(&mut self, field: &Field, value: &str) {
            let _ = write!(self.0, "{}={};", field.name(), value);
        }
        fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
            let _ = write!(self.0, "{}={:?};", field.name(), value);
        }
    }

    // ADR-0081 §4 crash-dump unit tests. The env-var-reading paths
    // (`crash_log_disabled`, `resolve_crash_dir`) are deliberately
    // not unit-tested in this module: cargo's parallel test execution
    // shares the process env, and setting `AETHER_CRASH_LOG_DIR` /
    // `AETHER_CRASH_LOG_DISABLE` here would race other tests in the
    // same binary. The pure-fn write path (`write_jsonl`) and the
    // filename-sanitisation routine cover the load-bearing logic
    // without touching env state.

    use aether_kinds::LogEntry;
    use std::fs;
    use std::path::PathBuf;
    use std::process;

    fn tempdir(suffix: &str) -> PathBuf {
        let mut dir = env::temp_dir();
        dir.push(format!(
            "aether-panic-hook-test-{}-{}",
            suffix,
            process::id(),
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("tempdir creates");
        dir
    }

    fn entry(level: u8, sequence: u64, message: &str) -> LogEntry {
        LogEntry {
            timestamp_unix_ms: 1_700_000_000_000 + sequence,
            level,
            target: "panic_hook_test".to_owned(),
            message: message.to_owned(),
            sequence,
            origin: None,
        }
    }

    /// `sanitize_filename` rewrites `/`, `\`, `:`, and NUL to `-`,
    /// leaves other characters alone. The trampoline-style
    /// `aether.component.trampoline:NAME` produces a `:`-containing
    /// thread name today; the sanitiser keeps the file shape sane.
    #[test]
    fn sanitize_filename_rewrites_path_chars() {
        assert_eq!(
            sanitize_filename("aether.component.trampoline:camera"),
            "aether.component.trampoline-camera",
        );
        assert_eq!(sanitize_filename("a/b\\c"), "a-b-c");
        assert_eq!(sanitize_filename("aether.audio"), "aether.audio");
    }

    /// `write_jsonl` produces a header object on line 1 + one JSON
    /// object per ring entry on subsequent lines. Verifies the
    /// load-bearing shape — every consumer of the crash dump reads
    /// this byte format.
    #[test]
    fn write_jsonl_emits_header_then_entries() {
        let dir = tempdir("write_jsonl");
        let path = dir.join("aether.audio.jsonl");
        let ring = vec![entry(2, 1, "before crash a"), entry(3, 2, "before crash b")];
        write_jsonl(
            &path,
            1_700_000_001_234,
            "aether.audio",
            "src/audio.rs:42:8",
            "panic payload",
            Some("backtrace text"),
            Some(&ring),
        )
        .expect("write succeeds");

        let contents = fs::read_to_string(&path).expect("readback");
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 3, "header + 2 entries");

        let header: serde_json::Value = serde_json::from_str(lines[0]).expect("header is json");
        assert_eq!(header["kind"], "panic");
        assert_eq!(header["timestamp_unix_ms"], 1_700_000_001_234u64);
        assert_eq!(header["thread"], "aether.audio");
        assert_eq!(header["location"], "src/audio.rs:42:8");
        assert_eq!(header["payload"], "panic payload");
        assert_eq!(header["backtrace"], "backtrace text");
        assert_eq!(header["ring_entries"], 2);

        let e0: LogEntry = serde_json::from_str(lines[1]).expect("entry 0 round-trips");
        let e1: LogEntry = serde_json::from_str(lines[2]).expect("entry 1 round-trips");
        assert_eq!(e0.message, "before crash a");
        assert_eq!(e0.sequence, 1);
        assert_eq!(e1.message, "before crash b");
        assert_eq!(e1.sequence, 2);
    }

    /// With no ring stamped (out-of-actor panic), the header is
    /// still written and `ring_entries` is 0 — the dump file
    /// exists, just with no per-actor history.
    #[test]
    fn write_jsonl_with_no_ring_emits_header_only() {
        let dir = tempdir("write_jsonl_no_ring");
        let path = dir.join("scheduler-thread.jsonl");
        write_jsonl(
            &path,
            1_700_000_002_000,
            "scheduler-thread",
            "src/scheduler.rs:1:1",
            "host-thread panic",
            None,
            None,
        )
        .expect("write succeeds");
        let contents = fs::read_to_string(&path).expect("readback");
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 1, "header only");
        let header: serde_json::Value = serde_json::from_str(lines[0]).expect("header is json");
        assert_eq!(header["ring_entries"], 0);
        assert!(
            header["backtrace"].is_null(),
            "no backtrace → null in the JSON",
        );
    }

    /// Empty ring (actor stamped but no events emitted) round-trips
    /// the same way as a missing ring — header only, no entries.
    /// Confirms the empty-slice branch in `write_jsonl`.
    #[test]
    fn write_jsonl_with_empty_ring_emits_header_only() {
        let dir = tempdir("write_jsonl_empty_ring");
        let path = dir.join("aether.idle.jsonl");
        write_jsonl(
            &path,
            0,
            "aether.idle",
            "<unknown>",
            "boom",
            None,
            Some(&[]),
        )
        .expect("write succeeds");
        let contents = fs::read_to_string(&path).expect("readback");
        assert_eq!(contents.lines().count(), 1);
    }
}
