//! Issue 545 PR E1: collapsed `aether.log` cap. Pre-PR-E1 the cap
//! lived split across `aether-kinds::log::LogCapability<B>` (facade
//! generic) and this file (concrete `LogTracingBackend`). The facade
//! pattern (ADR-0075) is retired — caps are now regular `#[actor]`
//! blocks, same shape as wasm components.
//!
//! Bridging via the `log` crate facade (rather than `tracing::event!`)
//! is load-bearing — see [`crate::log_sink`] for the rationale.
//! `tracing::event!` requires a `&'static str` target; the
//! guest-supplied target is dynamic, so we route through `log::log!`
//! and let `tracing-subscriber`'s `tracing-log` integration lift each
//! record back into the tracing pipeline.

use aether_actor::{Actor, Singleton};
use aether_kinds::LogEvent;

use crate::log_sink;

/// `aether.log` mailbox cap. Stateless beyond the process-wide
/// `tracing` subscriber set up by [`crate::log_capture::init`] —
/// every cap instance bridges decoded `LogEvent` mail through the
/// `log` facade and `tracing-log` re-emits it.
#[derive(Default)]
pub struct LogCapability;

impl LogCapability {
    pub fn new() -> Self {
        Self
    }
}

impl Actor for LogCapability {
    /// Components mail `aether.log` (kind id) to this mailbox; the
    /// `aether.<name>` form is the post-ADR-0074 Phase 5 convention
    /// for chassis-owned mailboxes.
    const NAMESPACE: &'static str = "aether.log";
}

impl Singleton for LogCapability {}

#[aether_data::actor]
impl LogCapability {
    /// Emit a decoded log event through the host's `tracing` pipeline
    /// so `engine_logs` (ADR-0023) sees it.
    ///
    /// # Agent
    /// Components mail `aether.log` `LogEvent { level, target, message }`
    /// to this mailbox. Fire-and-forget; no reply.
    #[aether_data::handler]
    fn on_log_event(&mut self, event: LogEvent) {
        log_sink::handle_log_mail_decoded(event);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::capability::ChassisBuilder;
    use crate::mailer::Mailer;
    use crate::registry::{MailboxEntry, Registry};
    use aether_data::Kind;

    fn fresh_substrate() -> (Arc<Registry>, Arc<Mailer>) {
        (Arc::new(Registry::new()), Arc::new(Mailer::new()))
    }

    /// End-to-end: boot the cap, push an `aether.log` mail at the
    /// registered mailbox, the dispatcher thread runs the
    /// macro-emitted `Dispatch::__dispatch` which calls
    /// `on_log_event`. Test asserts dispatch + clean shutdown.
    #[test]
    fn capability_routes_log_event_through_dispatcher() {
        let (registry, mailer) = fresh_substrate();
        let chassis = ChassisBuilder::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with(LogCapability::new())
            .build()
            .expect("capability boots");

        let id = registry
            .lookup(LogCapability::NAMESPACE)
            .expect("mailbox registered");
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

        thread::sleep(Duration::from_millis(50));
        let start = Instant::now();
        chassis.shutdown();
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "shutdown should complete promptly via channel-drop"
        );
    }

    /// Builder rejects a duplicate claim if the well-known mailbox name
    /// was already registered.
    #[test]
    fn duplicate_claim_rejects_with_typed_error() {
        use crate::capability::BootError;

        let (registry, mailer) = fresh_substrate();
        registry.register_sink(LogCapability::NAMESPACE, Arc::new(|_, _, _, _, _, _| {}));

        let err = ChassisBuilder::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with(LogCapability::new())
            .build()
            .expect_err("collision must surface as BootError");
        assert!(matches!(
            err,
            BootError::MailboxAlreadyClaimed { ref name }
                if name == LogCapability::NAMESPACE
        ));
    }
}
