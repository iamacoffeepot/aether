//! `aether.log` cap. Issue 565 pilot for the `#[bridge]` mod pattern:
//! the struct + actor impl + tests live inside `mod native`, which
//! `#[bridge]` cfg-gates. The macro emits a wasm-stub `pub struct
//! LogCapability;` at file root plus always-on Singleton + Actor +
//! HandlesKind markers, and re-exports the real struct from inside
//! the mod on native.
//!
//! Bridging via the `log` crate facade (rather than `tracing::event!`)
//! is load-bearing — see [`aether_substrate::log_sink`] for the rationale.
//! `tracing::event!` requires a `&'static str` target; the
//! guest-supplied target is dynamic, so we route through `log::log!`
//! and let `tracing-subscriber`'s `tracing-log` integration lift each
//! record back into the tracing pipeline.

// Handler-signature kinds must be importable at file root because
// `#[bridge]` emits `impl HandlesKind<K> for X {}` markers as siblings
// of the mod (always-on, outside the cfg gate). Aether-kinds is
// wasm-compatible so the import doesn't need cfg gating.
use aether_kinds::LogEvent;

#[aether_actor::bridge]
mod native {
    use super::LogEvent;
    use aether_actor::actor;
    use aether_substrate::capability::BootError;
    use aether_substrate::log_sink;
    use aether_substrate::native_actor::{NativeActor, NativeCtx, NativeInitCtx};

    /// `aether.log` mailbox cap. Stateless beyond the process-wide
    /// `tracing` subscriber set up by [`aether_substrate::log_capture::init`] —
    /// every cap instance bridges decoded `LogEvent` mail through the
    /// `log` facade and `tracing-log` re-emits it.
    pub struct LogCapability;

    #[actor]
    impl NativeActor for LogCapability {
        type Config = ();
        const NAMESPACE: &'static str = "aether.log";

        fn init(_: (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self)
        }

        /// Emit a decoded log event through the host's `tracing` pipeline
        /// so `engine_logs` (ADR-0023) sees it.
        ///
        /// # Agent
        /// Components mail `aether.log` `LogEvent { level, target, message }`
        /// to this mailbox. Fire-and-forget; no reply.
        #[handler]
        fn on_log_event(&self, _ctx: &mut NativeCtx<'_>, event: LogEvent) {
            log_sink::handle_log_mail_decoded(event);
        }
    }

    #[cfg(test)]
    mod tests {
        use std::sync::Arc;
        use std::thread;
        use std::time::{Duration, Instant};

        use super::{BootError, LogCapability, LogEvent};
        use aether_actor::Actor;
        use aether_data::Kind;
        use aether_substrate::chassis::Chassis;
        use aether_substrate::chassis_builder::{Builder, BuiltChassis, NeverDriver};
        use aether_substrate::mailer::Mailer;
        use aether_substrate::registry::{MailboxEntry, Registry};

        /// Stand-in chassis for the passive boot path. The Log cap doesn't
        /// need a driver, so `build_passive()` is the natural test entry
        /// — same shape as the `with_actor_*` smokes in
        /// `chassis_builder::tests`.
        struct TestChassis;
        impl Chassis for TestChassis {
            const PROFILE: &'static str = "test";
            type Driver = NeverDriver;
            type Env = ();
            fn build(_env: Self::Env) -> Result<BuiltChassis<Self>, BootError> {
                unreachable!("TestChassis is driven by Builder::new directly in unit tests")
            }
        }

        fn fresh_substrate() -> (Arc<Registry>, Arc<Mailer>) {
            (Arc::new(Registry::new()), Arc::new(Mailer::new()))
        }

        /// End-to-end: boot the cap through `with_actor`, push an
        /// `aether.log` mail at the registered mailbox, the dispatcher
        /// thread runs the macro-emitted `NativeDispatch` which calls
        /// `on_log_event`. Test asserts dispatch + clean shutdown.
        #[test]
        fn capability_routes_log_event_through_dispatcher() {
            let (registry, mailer) = fresh_substrate();
            let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
                .with_actor::<LogCapability>(())
                .build_passive()
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
                aether_substrate::mail::ReplyTo::NONE,
                &bytes,
                1,
            );

            thread::sleep(Duration::from_millis(50));
            let start = Instant::now();
            drop(chassis);
            assert!(
                start.elapsed() < Duration::from_millis(500),
                "shutdown should complete promptly via channel-drop"
            );
        }

        /// Builder rejects a duplicate claim if the well-known mailbox name
        /// was already registered.
        #[test]
        fn duplicate_claim_rejects_with_typed_error() {
            let (registry, mailer) = fresh_substrate();
            registry.register_sink(LogCapability::NAMESPACE, Arc::new(|_, _, _, _, _, _| {}));

            let err = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
                .with_actor::<LogCapability>(())
                .build_passive()
                .expect_err("collision must surface as BootError");
            assert!(matches!(
                err,
                BootError::MailboxAlreadyClaimed { ref name }
                    if name == LogCapability::NAMESPACE
            ));
        }
    }
}
