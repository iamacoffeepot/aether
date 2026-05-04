//! ADR-0075 §Decision 3: substrate-side backend for the
//! [`aether_kinds::LogCapability`] facade. The cap's facade lives in
//! `aether-kinds` (so wasm senders can address it without pulling in
//! substrate-only types); this module provides the concrete backend
//! the chassis installs at boot.
//!
//! Pre-ADR-0075 the cap and the dispatcher lifecycle both lived here.
//! ADR-0075 split them: the cap is now a thin generic in
//! `aether-kinds`, the dispatcher loop moved into the chassis-side
//! [`crate::capability::ChassisCtx::spawn_actor_dispatcher`] helper,
//! and this file is just the substrate-side `LogBackend` impl.
//!
//! Bridging via the `log` crate facade (rather than `tracing::event!`)
//! is load-bearing — see [`crate::log_sink`] for the rationale.
//! `tracing::event!` requires a `&'static str` target; the
//! guest-supplied target is dynamic, so we route through `log::log!`
//! and let `tracing-subscriber`'s `tracing-log` integration lift each
//! record back into the tracing pipeline.

use aether_kinds::{LogBackend, LogEvent};

use crate::log_sink;

/// `LogBackend` impl that bridges decoded `LogEvent` mail through the
/// `log` facade so the chassis's existing `tracing-log` subscriber
/// re-emits it. Stateless beyond what the global subscriber owns —
/// every cap instance shares the process-wide `tracing` plumbing set
/// up by [`crate::log_capture::init`] during chassis boot.
#[derive(Default)]
pub struct LogTracingBackend;

impl LogTracingBackend {
    pub fn new() -> Self {
        Self
    }
}

impl LogBackend for LogTracingBackend {
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
    use aether_data::{Actor, Kind};
    use aether_kinds::LogCapability;

    fn fresh_substrate() -> (Arc<Registry>, Arc<Mailer>) {
        (Arc::new(Registry::new()), Arc::new(Mailer::new()))
    }

    /// End-to-end: boot the facade cap, push an `aether.log` mail at
    /// the registered mailbox, the dispatcher thread runs the
    /// macro-emitted `Dispatch::__dispatch` which delegates to the
    /// backend's `on_log_event`. Test asserts dispatch + clean
    /// shutdown without hanging.
    #[test]
    fn capability_routes_log_event_through_dispatcher() {
        let (registry, mailer) = fresh_substrate();
        let chassis = ChassisBuilder::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with_facade(LogCapability::new(LogTracingBackend::new()))
            .build()
            .expect("capability boots");

        let id = registry
            .lookup(<LogCapability<LogTracingBackend> as Actor>::NAMESPACE)
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
        registry.register_sink(
            <LogCapability<LogTracingBackend> as Actor>::NAMESPACE,
            Arc::new(|_, _, _, _, _, _| {}),
        );

        let err = ChassisBuilder::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with_facade(LogCapability::new(LogTracingBackend::new()))
            .build()
            .expect_err("collision must surface as BootError");
        assert!(matches!(
            err,
            BootError::MailboxAlreadyClaimed { ref name }
                if name == <LogCapability<LogTracingBackend> as Actor>::NAMESPACE
        ));
    }
}
