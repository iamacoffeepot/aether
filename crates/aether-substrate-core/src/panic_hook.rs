// Substrate panic-hook plumbing (issue #321 Phase 1).
//
// Installs a process-global panic hook that routes panics through
// `tracing::error!` so they surface in `engine_logs` via the existing
// ADR-0023 capture path. Without this, panics on dispatcher threads
// (`std::thread::Builder::spawn` in `scheduler.rs`) print to stderr
// only — and stderr is not what the capture layer reads.
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

use std::backtrace::{Backtrace, BacktraceStatus};
use std::panic::{self, PanicHookInfo};
use std::sync::OnceLock;

/// Env var that forces backtrace capture without flipping
/// `RUST_BACKTRACE` for the whole process (which would change every
/// other crate's panic output).
pub const ENV_BACKTRACE: &str = "AETHER_BACKTRACE";

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
        emit_event(info);
        prev(info);
    })
}

fn emit_event(info: &PanicHookInfo<'_>) {
    let thread = std::thread::current();
    let thread_name = thread.name().unwrap_or("<unnamed>");
    let location = info
        .location()
        .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
        .unwrap_or_else(|| "<unknown>".to_string());
    let payload = payload_string(info.payload());
    let backtrace = capture_backtrace();

    // The current `tracing::Span` is auto-captured by the dispatcher
    // (subscriber-side concern), so component / mailbox / kind ride
    // along when the panic happens inside `dispatcher_loop` without
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

/// Stringify the panic payload. Std supports two common shapes —
/// `&'static str` (from `panic!("literal")`) and `String` (from
/// `panic!("{}", thing)`) — and falls back to a `TypeId` mention for
/// anything else. Mirrors the default hook's behaviour without
/// duplicating its full dance.
fn payload_string(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        format!("<non-string panic payload type_id={:?}>", payload.type_id())
    }
}

fn capture_backtrace() -> Option<Backtrace> {
    capture_backtrace_with(std::env::var_os(ENV_BACKTRACE).is_some())
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
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use tracing::Subscriber;
    use tracing::field::{Field, Visit};

    use super::*;

    /// `&'static str` payload (the common `panic!("literal")` case).
    #[test]
    fn payload_string_handles_static_str() {
        let payload: Box<dyn std::any::Any + Send> = Box::new("static reason");
        assert_eq!(payload_string(payload.as_ref()), "static reason");
    }

    /// `String` payload (the common `panic!("{}", x)` case).
    #[test]
    fn payload_string_handles_owned_string() {
        let payload: Box<dyn std::any::Any + Send> = Box::new(String::from("owned reason"));
        assert_eq!(payload_string(payload.as_ref()), "owned reason");
    }

    /// Anything else falls through to a TypeId mention so the message
    /// at least identifies the payload shape.
    #[test]
    fn payload_string_handles_unknown_type() {
        let payload: Box<dyn std::any::Any + Send> = Box::new(42i32);
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
    /// not double-install or panic. The OnceLock guard is the
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
        let test_thread = std::thread::current().id();
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_for_prev = Arc::clone(&counter);
        let prev: Box<dyn Fn(&PanicHookInfo<'_>) + Sync + Send + 'static> =
            Box::new(move |_info| {
                if std::thread::current().id() == test_thread {
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

        tracing::subscriber::with_default(subscriber, || {
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
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            let mut buf = String::new();
            let mut visitor = StringVisit(&mut buf);
            event.record(&mut visitor);
            self.events.lock().unwrap().push(CapturedEvent {
                target: event.metadata().target().to_string(),
                fields: buf,
            });
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    struct StringVisit<'a>(&'a mut String);

    impl Visit for StringVisit<'_> {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.0.push_str(&format!("{}={:?};", field.name(), value));
        }
        fn record_str(&mut self, field: &Field, value: &str) {
            self.0.push_str(&format!("{}={};", field.name(), value));
        }
    }
}
