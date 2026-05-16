//! `aether.handle` cap. Owns the substrate's `HandleStore` and routes
//! ADR-0045 publish/release/pin/unpin requests via `ctx.reply(&result)`.
//! Decode failure on a malformed payload goes through the macro miss
//! path (warn-log, no reply, sender's `wait_reply` times out) —
//! substrate-level invariant violation, not user-recoverable input.
//!
//! The handle store flows in through `NativeInitCtx::mailer().handle_store()`
//! at boot; the chassis builder's `with_actor::<HandleCapability>(())`
//! is the boot site.

// Handler-signature kinds must be importable at file root because
// `#[bridge]` emits `impl HandlesKind<K> for X {}` markers as siblings
// of the mod (always-on, outside the cfg gate).
use aether_kinds::{HandlePin, HandlePublish, HandleRelease, HandleUnpin};

#[aether_actor::bridge(singleton)]
mod native {
    use std::sync::Arc;

    use super::{HandlePin, HandlePublish, HandleRelease, HandleUnpin};
    use aether_actor::{MailCtx, actor};
    use aether_kinds::{
        HandleError, HandlePinResult, HandlePublishResult, HandleReleaseResult, HandleUnpinResult,
    };
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::chassis::error::BootError;
    use aether_substrate::handle_store::{HandleStore, PutError};

    /// `aether.handle` mailbox cap. Owns the substrate's `HandleStore`.
    pub struct HandleCapability {
        store: Arc<HandleStore>,
    }

    #[actor]
    impl NativeActor for HandleCapability {
        type Config = ();
        /// ADR-0045 + ADR-0074 Phase 5: chassis-owned mailbox under the
        /// `aether.<name>` namespace.
        const NAMESPACE: &'static str = "aether.handle";

        /// Pull the shared [`HandleStore`] off the substrate's
        /// [`aether_substrate::Mailer`]. The store is supplied at
        /// `Mailer` construction by `SubstrateBoot::build` (issue 657
        /// retired the post-construction `wire_handle_store` setter),
        /// so the cap can clone it directly without a `None`-arm
        /// bootstrap-ordering check.
        fn init(_: (), ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            let store = Arc::clone(ctx.mailer().handle_store());
            Ok(Self { store })
        }

        /// Publish bytes under a fresh handle id.
        ///
        /// # Agent
        /// Reply: `HandlePublishResult`.
        #[handler]
        fn on_publish(&self, ctx: &mut NativeCtx<'_>, mail: HandlePublish) {
            let id = self.store.next_ephemeral();
            match self.store.put(id, mail.kind_id, mail.bytes) {
                Ok(()) => {
                    // Hold a reference on behalf of the publishing
                    // component. Drop / explicit release decrements;
                    // on zero the entry stays in the store (subject
                    // to LRU eviction under pressure).
                    self.store.inc_ref(id);
                    ctx.reply(&HandlePublishResult::Ok {
                        kind_id: mail.kind_id,
                        id,
                    });
                }
                Err(e) => {
                    ctx.reply(&HandlePublishResult::Err {
                        kind_id: mail.kind_id,
                        error: put_error_to_handle_error(e),
                    });
                }
            }
        }

        /// Decrement a handle's refcount. SDK-side `Handle<K>::Drop`
        /// fires this; explicit `Ctx::release` paths also use it.
        ///
        /// # Agent
        /// Reply: `HandleReleaseResult`.
        #[handler]
        fn on_release(&self, ctx: &mut NativeCtx<'_>, mail: HandleRelease) {
            if self.store.dec_ref(mail.id) {
                ctx.reply(&HandleReleaseResult::Ok { id: mail.id });
            } else {
                ctx.reply(&HandleReleaseResult::Err {
                    id: mail.id,
                    error: HandleError::UnknownHandle,
                });
            }
        }

        /// Pin a handle so the LRU evictor skips it.
        ///
        /// # Agent
        /// Reply: `HandlePinResult`.
        #[handler]
        fn on_pin(&self, ctx: &mut NativeCtx<'_>, mail: HandlePin) {
            if self.store.pin(mail.id) {
                ctx.reply(&HandlePinResult::Ok { id: mail.id });
            } else {
                ctx.reply(&HandlePinResult::Err {
                    id: mail.id,
                    error: HandleError::UnknownHandle,
                });
            }
        }

        /// Clear the pinned flag on a handle.
        ///
        /// # Agent
        /// Reply: `HandleUnpinResult`.
        #[handler]
        fn on_unpin(&self, ctx: &mut NativeCtx<'_>, mail: HandleUnpin) {
            if self.store.unpin(mail.id) {
                ctx.reply(&HandleUnpinResult::Ok { id: mail.id });
            } else {
                ctx.reply(&HandleUnpinResult::Err {
                    id: mail.id,
                    error: HandleError::UnknownHandle,
                });
            }
        }
    }

    fn put_error_to_handle_error(e: PutError) -> HandleError {
        match e {
            PutError::EvictionFailed { .. } => HandleError::EvictionFailed,
            PutError::KindMismatch {
                existing_kind,
                requested_kind,
            } => HandleError::AdapterError(format!(
                "kind id mismatch: existing={existing_kind} requested={requested_kind}"
            )),
        }
    }

    #[cfg(test)]
    mod tests {
        use std::sync::mpsc;
        use std::thread;
        use std::time::{Duration, Instant};

        use aether_actor::Actor;
        use aether_data::{HandleId, Kind, KindId};
        use aether_data::{SessionToken, Uuid};

        use super::{
            Arc, BootError, HandleCapability, HandlePublish, HandlePublishResult, HandleStore,
        };
        use aether_substrate::chassis::Chassis;
        use aether_substrate::chassis::builder::{Builder, BuiltChassis, NeverDriver};
        use aether_substrate::mail::mailer::Mailer;
        use aether_substrate::mail::outbound::EgressEvent;
        use aether_substrate::mail::registry::{MailboxEntry, Registry};
        use aether_substrate::mail::{ReplyTarget, ReplyTo};

        struct TestChassis;
        impl Chassis for TestChassis {
            const PROFILE: &'static str = "test";
            type Driver = NeverDriver;
            type Env = ();
            fn build(_env: Self::Env) -> Result<BuiltChassis<Self>, BootError> {
                unreachable!("TestChassis is driven by Builder::new directly in unit tests")
            }
        }

        fn fresh_substrate() -> (
            Arc<HandleStore>,
            Arc<Mailer>,
            Arc<Registry>,
            mpsc::Receiver<EgressEvent>,
        ) {
            let store = Arc::new(HandleStore::new(64 * 1024));
            let registry = Arc::new(Registry::new());
            for d in aether_kinds::descriptors::all() {
                let _ = registry.register_kind_with_descriptor(d);
            }
            let (outbound, rx) = aether_substrate::mail::outbound::HubOutbound::attached_loopback();
            let mailer = Arc::new(
                Mailer::new(Arc::clone(&registry), Arc::clone(&store)).with_outbound(outbound),
            );
            (store, mailer, registry, rx)
        }

        fn session_reply_to() -> ReplyTo {
            ReplyTo::to(ReplyTarget::Session(SessionToken(Uuid::from_u128(0xfeed))))
        }

        /// End-to-end through the cap: boot it via `with_actor`, push a
        /// `HandlePublish` mail at the registered mailbox, the dispatcher
        /// thread runs the macro-emitted `NativeDispatch::__aether_dispatch_envelope`
        /// which calls `on_publish`, the reply lands on the hub-outbound
        /// channel via `ctx.reply(&HandlePublishResult::Ok)` →
        /// `Mailer::send_reply` → `outbound.send_reply`.
        #[test]
        fn capability_routes_publish_through_dispatcher_thread() {
            let (store, mailer, registry, rx) = fresh_substrate();

            let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
                .with_actor::<HandleCapability>(())
                .build_passive()
                .expect("capability boots");

            let id = registry
                .lookup(HandleCapability::NAMESPACE)
                .expect("mailbox registered");
            let MailboxEntry::Closure(handler) = registry.entry(id).expect("entry") else {
                panic!("expected mailbox entry");
            };

            let req = HandlePublish {
                kind_id: KindId(0xCAFE),
                bytes: vec![1, 2, 3, 4, 5],
            };
            let bytes = postcard::to_allocvec(&req).unwrap();
            handler(aether_substrate::mail::registry::MailDispatch {
                kind: <HandlePublish as Kind>::ID,
                kind_name: "aether.handle.publish",
                origin: None,
                sender: session_reply_to(),
                payload: &bytes,
                count: 1,
                mail_id: aether_substrate::mail::MailId::NONE,
                root: aether_substrate::mail::MailId::NONE,
                parent_mail: None,
            });

            let deadline = Instant::now() + Duration::from_secs(2);
            let frame = loop {
                if let Ok(f) = rx.try_recv() {
                    break f;
                }
                if Instant::now() >= deadline {
                    panic!("publish reply did not arrive within deadline");
                }
                thread::sleep(Duration::from_millis(5));
            };
            let payload = match frame {
                EgressEvent::ToSession { payload, .. } => payload,
                other => panic!("expected ToSession egress, got {other:?}"),
            };
            let result: HandlePublishResult = postcard::from_bytes(&payload).unwrap();
            let HandlePublishResult::Ok {
                kind_id,
                id: handle_id,
            } = result
            else {
                panic!("expected Ok, got {result:?}");
            };
            assert_eq!(kind_id, KindId(0xCAFE));
            assert_ne!(handle_id, HandleId(0));
            let (stored_kind, stored_bytes) = store.get(handle_id).unwrap();
            assert_eq!(stored_kind, KindId(0xCAFE));
            assert_eq!(stored_bytes, vec![1, 2, 3, 4, 5]);

            drop(chassis);
        }

        /// Channel-drop shutdown: drop the chassis, the cap's dispatcher
        /// thread exits within a generous deadline.
        #[test]
        fn shutdown_joins_dispatcher_thread() {
            let (_store, mailer, registry, _rx) = fresh_substrate();

            let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
                .with_actor::<HandleCapability>(())
                .build_passive()
                .expect("capability boots");

            let start = Instant::now();
            drop(chassis);
            let elapsed = start.elapsed();
            assert!(
                elapsed < Duration::from_millis(500),
                "shutdown should complete promptly via channel-drop (took {elapsed:?})"
            );
        }

        /// Builder rejects a duplicate claim if the well-known mailbox
        /// name was already registered.
        #[test]
        fn duplicate_claim_rejects_with_typed_error() {
            let (_store, mailer, registry, _rx) = fresh_substrate();
            registry.register_closure(
                HandleCapability::NAMESPACE,
                aether_substrate::mail::registry::noop_handler(),
            );

            let err = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
                .with_actor::<HandleCapability>(())
                .build_passive()
                .expect_err("collision must surface as BootError");
            assert!(matches!(
                err,
                BootError::MailboxAlreadyClaimed { ref name }
                    if name == HandleCapability::NAMESPACE
            ));
        }
    }
}
