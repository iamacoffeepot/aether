//! ADR-0070 Phase 3: guest-log sink as a native capability.
//!
//! Counterpart to the `MailSubscriber` in `aether-component`: decodes
//! `aether.log` mail the guest sent to `aether.sink.log` and re-emits
//! the event through the host's existing tracing pipeline so it shows
//! up in `engine_logs` (ADR-0023) and on stderr alongside native-side
//! logs.
//!
//! Bridging via the `log` crate facade (rather than `tracing::event!`)
//! is load-bearing — see [`crate::log_sink`] for the rationale.
//! `tracing::event!` requires a `&'static str` target; the
//! guest-supplied target is dynamic, so we route through `log::log!`
//! and let `tracing-subscriber`'s `tracing-log` integration lift each
//! record back into the tracing pipeline.
//!
//! Chassis-conditional, NOT universal: desktop, headless, and the
//! test bench register the log capability; the hub chassis does
//! not (no shipped hub chassis hosts components today). Each chassis
//! main calls `boot.add_capability(LogCapability::new())?` after
//! [`crate::SubstrateBoot::build`] returns.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::capability::{BootError, Capability, ChassisCtx, RunningCapability};
use crate::log_sink;

/// Recipient name the log capability claims. Components mail
/// `aether.log` (kind id) to this mailbox; the SDK's
/// `MailSubscriber` resolves through here. ADR-0058 places
/// chassis-owned sinks under `aether.sink.*`.
pub const LOG_SINK_NAME: &str = "aether.sink.log";

/// Polling interval for the dispatcher's shutdown check. Same shape
/// as `HandleCapability`; see ADR-0070 §"Threading model".
const SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Native capability owning the ADR-0060 guest-log sink. Boots a
/// single dispatcher thread that pulls envelopes from the
/// `aether.sink.log` mailbox and routes the bytes through
/// [`log_sink::handle_log_mail`] for postcard-decode + log-facade
/// emit.
///
/// Stateless: the capability holds no per-instance config, and the
/// global tracing subscriber (set up by [`crate::log_capture::init`]
/// during the shared boot) is what actually receives the bridged
/// log records.
#[derive(Default)]
pub struct LogCapability {}

impl LogCapability {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Running handle returned by [`LogCapability::boot`]. Same shape
/// as `HandleRunning`: dispatcher thread + shutdown flag the thread
/// polls.
pub struct LogRunning {
    thread: Option<JoinHandle<()>>,
    shutdown_flag: Arc<AtomicBool>,
}

impl Capability for LogCapability {
    type Running = LogRunning;

    fn boot(self, ctx: &mut ChassisCtx<'_>) -> Result<Self::Running, BootError> {
        let claim = ctx.claim_mailbox(LOG_SINK_NAME)?;
        let shutdown_flag = Arc::new(AtomicBool::new(false));
        let thread_flag = Arc::clone(&shutdown_flag);
        let receiver = claim.receiver;

        let thread = thread::Builder::new()
            .name("aether-log-sink".into())
            .spawn(move || {
                while !thread_flag.load(Ordering::Relaxed) {
                    match receiver.recv_timeout(SHUTDOWN_POLL_INTERVAL) {
                        Ok(env) => log_sink::handle_log_mail(&env.payload),
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
            })
            .map_err(|e| BootError::Other(Box::new(e)))?;

        Ok(LogRunning {
            thread: Some(thread),
            shutdown_flag,
        })
    }
}

impl RunningCapability for LogRunning {
    fn shutdown(self: Box<Self>) {
        let LogRunning {
            mut thread,
            shutdown_flag,
        } = *self;
        shutdown_flag.store(true, Ordering::Relaxed);
        if let Some(t) = thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;
    use crate::capability::ChassisBuilder;
    use crate::mailer::Mailer;
    use crate::registry::{MailboxEntry, Registry};
    use aether_data::Kind;
    use aether_kinds::LogEvent;

    fn fresh_substrate() -> (Arc<Registry>, Arc<Mailer>) {
        (Arc::new(Registry::new()), Arc::new(Mailer::new()))
    }

    /// End-to-end: boot the capability, push an `aether.log` mail at
    /// the registered sink, the dispatcher thread runs
    /// `handle_log_mail` (which on a test runner with no global
    /// tracing subscriber drops the record silently — what we're
    /// asserting is that the dispatch path doesn't panic and that
    /// shutdown joins cleanly).
    #[test]
    fn capability_routes_log_event_through_dispatcher() {
        let (registry, mailer) = fresh_substrate();
        let chassis = ChassisBuilder::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with(LogCapability::new())
            .build()
            .expect("capability boots");

        let id = registry.lookup(LOG_SINK_NAME).expect("sink registered");
        let MailboxEntry::Sink(handler) = registry.entry(id).expect("entry") else {
            panic!("expected sink entry");
        };

        let event = LogEvent {
            level: 3,
            target: "aether_test_guest".into(),
            message: "parse failed: missing close paren".into(),
        };
        let bytes = postcard::to_allocvec(&event).expect("encode");
        handler(
            <LogEvent as Kind>::ID,
            "aether.log",
            None,
            crate::mail::ReplyTo::NONE,
            &bytes,
            1,
        );

        // Give the dispatcher a moment to drain. recv_timeout means
        // worst-case latency is one poll interval; test budget is
        // 200ms.
        thread::sleep(Duration::from_millis(50));
        let start = Instant::now();
        chassis.shutdown();
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "shutdown should complete within a poll interval"
        );
    }

    /// Builder rejects a duplicate claim if the well-known sink name
    /// was already registered. Same protection as `HandleCapability`'s
    /// duplicate-claim test.
    #[test]
    fn duplicate_claim_rejects_with_typed_error() {
        let (registry, mailer) = fresh_substrate();
        registry.register_sink(LOG_SINK_NAME, Arc::new(|_, _, _, _, _, _| {}));

        let err = ChassisBuilder::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with(LogCapability::new())
            .build()
            .expect_err("collision must surface as BootError");
        assert!(matches!(
            err,
            BootError::MailboxAlreadyClaimed { ref name } if name == LOG_SINK_NAME
        ));
    }
}
