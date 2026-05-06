//! `aether.log` cap. Issue 565 pilot for the `#[bridge]` mod pattern:
//! the struct + actor impl + tests live inside `mod native`, which
//! `#[bridge]` cfg-gates. The macro emits a wasm-stub `pub struct
//! LogCapability;` at file root plus always-on Singleton + Actor +
//! HandlesKind markers, and re-exports the real struct from inside
//! the mod on native.
//!
//! Issue #581 retired `log_capture`'s ring/flush plumbing in favour
//! of this cap as the egress owner. Every `tracing::*` event flows
//! through `aether-actor::log`'s actor-aware subscriber:
//!
//! - In-actor → buffered in `LogBuffer` → drain at handler exit
//!   ships a single `LogBatch` mail to this mailbox.
//! - Host code → single-entry `LogBatch` mail through the
//!   registered host dispatch (also lands here).
//!
//! The cap's body is a pure forwarder: each entry becomes a
//! substrate-side `LogEntry` handed to `HubOutbound::egress_log_batch`.

use aether_kinds::LogBatch;

#[aether_actor::bridge]
mod native {
    use super::LogBatch;
    use aether_actor::actor;
    use aether_substrate::capability::BootError;
    use aether_substrate::native_actor::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::outbound::{HubOutbound, LogEntry, LogLevel};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    /// `aether.log` mailbox cap. Receives [`LogBatch`] mail, converts
    /// each entry into a substrate-side [`LogEntry`] (timestamp +
    /// monotonic sequence stamped on receipt; `origin` plucked from
    /// the mail envelope's sender), and forwards via
    /// [`HubOutbound::egress_log_batch`].
    pub struct LogCapability {
        outbound: Option<Arc<HubOutbound>>,
        sequence: AtomicU64,
    }

    #[actor]
    impl NativeActor for LogCapability {
        type Config = ();
        const NAMESPACE: &'static str = "aether.log";

        fn init(_: (), ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            let outbound = ctx.mailer().outbound().cloned();
            Ok(Self {
                outbound,
                sequence: AtomicU64::new(1),
            })
        }

        /// Forward a drained log batch to the hub via `egress_log_batch`.
        ///
        /// # Agent
        /// The actor-aware tracing subscriber buffers `tracing::*` events
        /// per-actor and ships a [`LogBatch`] here at handler exit (or
        /// immediately on `WARN`/`ERROR` priority flush). Host-emitted
        /// events land as single-entry batches. Sender attribution
        /// rides on the mail envelope; this cap reads `ctx.sender()`
        /// to populate `LogEntry::origin`.
        #[handler]
        fn on_log_batch(&self, ctx: &mut NativeCtx<'_>, batch: LogBatch) {
            let Some(outbound) = self.outbound.as_ref() else {
                return;
            };
            let origin = ctx.origin();
            let now = now_unix_ms();
            let entries: Vec<LogEntry> = batch
                .entries
                .into_iter()
                .map(|e| LogEntry {
                    timestamp_unix_ms: now,
                    level: u8_to_level(e.level),
                    target: e.target,
                    message: e.message,
                    sequence: self.sequence.fetch_add(1, Ordering::Relaxed),
                    origin,
                })
                .collect();
            outbound.egress_log_batch(entries);
        }
    }

    fn u8_to_level(level: u8) -> LogLevel {
        match level {
            0 => LogLevel::Trace,
            1 => LogLevel::Debug,
            2 => LogLevel::Info,
            3 => LogLevel::Warn,
            4 => LogLevel::Error,
            _ => LogLevel::Info,
        }
    }

    fn now_unix_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    #[cfg(test)]
    mod tests {
        use std::sync::Arc;
        use std::thread;
        use std::time::Duration;

        use super::{BootError, LogBatch, LogCapability};
        use aether_actor::Actor;
        use aether_data::Kind;
        use aether_kinds::LogEvent;
        use aether_substrate::chassis::Chassis;
        use aether_substrate::chassis_builder::{Builder, BuiltChassis, NeverDriver};
        use aether_substrate::mailer::Mailer;
        use aether_substrate::registry::{MailboxEntry, Registry};

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

        /// End-to-end: boot the cap through `with_actor`, push a
        /// `LogBatch` mail at the registered mailbox, the dispatcher
        /// thread runs the macro-emitted `NativeDispatch` which calls
        /// `on_log_batch`. Test asserts dispatch + clean shutdown.
        #[test]
        fn capability_routes_log_batch_through_dispatcher() {
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

            let batch = LogBatch {
                entries: vec![LogEvent {
                    level: 3,
                    target: "aether_test_guest".into(),
                    message: "parse failed: missing close paren".into(),
                }],
            };
            let bytes = postcard::to_allocvec(&batch).expect("encode");
            handler(
                <LogBatch as Kind>::ID,
                "aether.log",
                None,
                aether_substrate::mail::ReplyTo::NONE,
                &bytes,
                1,
            );

            thread::sleep(Duration::from_millis(50));
            drop(chassis);
        }

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

// Subscriber install + tracing-subscriber stack moved to
// `aether-substrate::log_install` so the substrate's boot path can
// install the actor-aware layer before any cap boots (early-boot
// `tracing::*` still surfaces via the fmt::Layer fallback). The cap
// keeps only its handler body — this file no longer carries any
// install-side machinery.
