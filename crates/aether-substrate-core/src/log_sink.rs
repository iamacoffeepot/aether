// ADR-0060 guest-side logging via mail sink. Counterpart to the
// `MailSubscriber` in `aether-component`: decodes `aether.log` mail
// the guest sent to `"aether.sink.log"` and re-emits the event through
// the host's existing tracing pipeline so it shows up in `engine_logs`
// (ADR-0023) and on stderr alongside native-side logs.
//
// We bridge via the `log` crate facade rather than `tracing::event!`
// because `tracing::event!` requires a `&'static str` target — the
// guest-supplied target is dynamic. `log::log!(target: <dyn>, lvl, ...)`
// accepts an expression target, and `tracing-subscriber`'s default
// `tracing-log` feature lifts every log record into a tracing event
// with that target preserved. The chassis `EnvFilter` (built in
// `log_capture::init`) then sees the guest's target verbatim and
// applies the operator's `AETHER_LOG_FILTER` directives without us
// maintaining a leaked-target cache.
//
// The handler is intentionally chassis-agnostic — desktop and
// headless wire the same closure. Hub doesn't (no shipped chassis
// hosts components on the hub today; the kinds bubble back to the
// child substrate via ADR-0037 if a guest-bound component were ever
// loaded there).

use std::sync::Arc;

use aether_kinds::LogEvent;

use crate::mail::ReplyTo;
use crate::registry::{Registry, SinkHandler};

/// Build the `SinkHandler` closure that decodes `LogEvent` mail and
/// emits the event through the `log` facade. Caller registers it
/// against `"aether.sink.log"`.
pub fn log_sink_handler() -> SinkHandler {
    Arc::new(
        move |_kind_id: u64,
              _kind_name: &str,
              _origin: Option<&str>,
              _sender: ReplyTo,
              bytes: &[u8],
              _count: u32| {
            handle_log_mail(bytes);
        },
    )
}

fn handle_log_mail(bytes: &[u8]) {
    let event: LogEvent = match postcard::from_bytes(bytes) {
        Ok(e) => e,
        Err(err) => {
            tracing::warn!(
                target: "aether_substrate::log_sink",
                error = %err,
                bytes = bytes.len(),
                "log sink: failed to decode LogEvent, dropping",
            );
            return;
        }
    };
    let level = match event.level {
        0 => log::Level::Trace,
        1 => log::Level::Debug,
        2 => log::Level::Info,
        3 => log::Level::Warn,
        4 => log::Level::Error,
        other => {
            tracing::warn!(
                target: "aether_substrate::log_sink",
                level = other,
                "log sink: unknown level, treating as Info",
            );
            log::Level::Info
        }
    };
    log::log!(target: event.target.as_str(), level, "{}", event.message);
}

/// Convenience: register the log sink against the canonical mailbox
/// name `"aether.sink.log"`. Returned `MailboxId` is normally ignored;
/// callers keep it only to assert against in tests.
pub fn register_log_sink(registry: &Registry) {
    registry.register_sink("aether.sink.log", log_sink_handler());
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_mail::Kind;

    #[test]
    fn log_event_kind_name_matches() {
        // The component-side subscriber sends to "aether.sink.log" with
        // kind name "aether.log"; if either side renames in isolation
        // this test catches the divergence without needing a live wire.
        assert_eq!(LogEvent::NAME, "aether.log");
    }

    #[test]
    fn handler_decodes_and_dispatches_without_panic() {
        // No subscriber installed in tests => log records get dropped
        // silently by the log facade. This test is a smoke check that
        // the postcard decode + level dispatch path doesn't panic on a
        // valid payload.
        let event = LogEvent {
            level: 3,
            target: "aether_test_guest".into(),
            message: "parse failed: missing close paren".into(),
        };
        let bytes = postcard::to_allocvec(&event).expect("encode");
        handle_log_mail(&bytes);
    }

    #[test]
    fn handler_drops_garbage_bytes() {
        // Out-of-band bytes should warn-and-return rather than panic.
        handle_log_mail(&[0xff, 0xff, 0xff, 0xff, 0xff]);
    }
}
