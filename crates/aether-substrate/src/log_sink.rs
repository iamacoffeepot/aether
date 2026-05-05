// ADR-0060 guest-side log dispatch. ADR-0070 Phase 3 moved the
// `aether.log` mailbox out of inline registration in chassis
// mains and into [`crate::capabilities::log`] — this module retains
// the per-payload decode + log-facade emit, called from the
// capability's dispatcher thread for each envelope it receives.
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
// The capability is intentionally chassis-conditional — desktop,
// headless, and the test bench wire it; hub doesn't (no shipped
// chassis hosts components on the hub today; the kinds bubble back
// to the child substrate via ADR-0037 if a guest-bound component
// were ever loaded there).

use aether_kinds::LogEvent;

/// Decode a single `aether.log` payload and emit through the
/// `log::log!` facade. Pre-ADR-0075 this was the dispatcher's whole
/// body; post-ADR-0075 the chassis dispatcher decodes via the cap's
/// macro-emitted `Dispatch::__dispatch` which calls
/// [`handle_log_mail_decoded`] directly. This raw-bytes form remains
/// for tests that want to exercise the decode + warn-on-garbage path.
#[cfg(test)]
pub(crate) fn handle_log_mail(bytes: &[u8]) {
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
    handle_log_mail_decoded(event);
}

/// Emit an already-decoded `LogEvent` through the `log::log!` facade.
/// Called from the substrate-side `LogTracingBackend::on_log_event`
/// after the macro-emitted `Dispatch::__dispatch` decoded the bytes.
pub fn handle_log_mail_decoded(event: LogEvent) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use aether_data::Kind;

    #[test]
    fn log_event_kind_name_matches() {
        // The component-side subscriber sends to "aether.log" with
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
