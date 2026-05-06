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

#[aether_actor::bridge]
mod native {
    use std::sync::Arc;

    use super::{HandlePin, HandlePublish, HandleRelease, HandleUnpin};
    use aether_actor::{MailCtx, actor};
    use aether_kinds::{
        HandleError, HandlePinResult, HandlePublishResult, HandleReleaseResult, HandleUnpinResult,
    };
    use aether_substrate::capability::BootError;
    use aether_substrate::handle_store::{HandleStore, PutError};
    use aether_substrate::native_actor::{NativeActor, NativeCtx, NativeInitCtx};

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

        /// Pull the shared [`HandleStore`] off the substrate's wired
        /// [`aether_substrate::Mailer`]. The store is wired by `SubstrateBoot::build`
        /// before the chassis builder runs, so a `None` here is a
        /// substrate-level boot ordering bug rather than user input —
        /// surface it as a `BootError`.
        fn init(_: (), ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            let store = ctx
                .mailer()
                .handle_store()
                .ok_or_else(|| {
                    BootError::Other(Box::new(std::io::Error::other(
                        "HandleCapability::init: substrate Mailer has no HandleStore wired \
                         (call wire_handle_store before chassis build)",
                    )))
                })?
                .clone();
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
        use aether_substrate::capability::ChassisBuilder;
        use aether_substrate::mail::{ReplyTarget, ReplyTo};
        use aether_substrate::mailer::Mailer;
        use aether_substrate::outbound::EgressEvent;
        use aether_substrate::registry::{MailboxEntry, Registry};

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
            let (outbound, rx) = aether_substrate::outbound::HubOutbound::attached_loopback();
            let mailer = Arc::new(Mailer::new());
            mailer.wire(Arc::clone(&registry));
            mailer.wire_outbound(outbound);
            mailer.wire_handle_store(Arc::clone(&store));
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

            let chassis = ChassisBuilder::new(Arc::clone(&registry), Arc::clone(&mailer))
                .with_actor::<HandleCapability>(())
                .build()
                .expect("capability boots");

            let id = registry
                .lookup(HandleCapability::NAMESPACE)
                .expect("mailbox registered");
            let MailboxEntry::Sink(handler) = registry.entry(id).expect("entry") else {
                panic!("expected sink entry");
            };

            let req = HandlePublish {
                kind_id: KindId(0xCAFE),
                bytes: vec![1, 2, 3, 4, 5],
            };
            let bytes = postcard::to_allocvec(&req).unwrap();
            handler(
                <HandlePublish as Kind>::ID,
                "aether.handle.publish",
                None,
                session_reply_to(),
                &bytes,
                1,
            );

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
                EgressEvent::ToSession { payload, .. } | EgressEvent::Broadcast { payload, .. } => {
                    payload
                }
                other => panic!("expected ToSession/Broadcast egress, got {other:?}"),
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

            chassis.shutdown();
        }

        /// Channel-drop shutdown: drop the chassis, the cap's dispatcher
        /// thread exits within a generous deadline.
        #[test]
        fn shutdown_joins_dispatcher_thread() {
            let (_store, mailer, registry, _rx) = fresh_substrate();

            let chassis = ChassisBuilder::new(Arc::clone(&registry), Arc::clone(&mailer))
                .with_actor::<HandleCapability>(())
                .build()
                .expect("capability boots");

            let start = Instant::now();
            chassis.shutdown();
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
            registry.register_sink(HandleCapability::NAMESPACE, Arc::new(|_, _, _, _, _, _| {}));

            let err = ChassisBuilder::new(Arc::clone(&registry), Arc::clone(&mailer))
                .with_actor::<HandleCapability>(())
                .build()
                .expect_err("collision must surface as BootError");
            assert!(matches!(
                err,
                BootError::MailboxAlreadyClaimed { ref name }
                    if name == HandleCapability::NAMESPACE
            ));
        }
    }
}
