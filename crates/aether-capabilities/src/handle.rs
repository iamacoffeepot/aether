//! `aether.handle` cap. Owns the substrate's `HandleStore` and routes
//! ADR-0045 publish/release/pin/unpin requests via ADR-0112 `-> R` handlers.
//! Decode failure on a malformed payload goes through the macro miss
//! path (warn-log, no reply, so the sender's correlated reply handler
//! never fires) — substrate-level invariant violation, not
//! user-recoverable input.
//!
//! The handle store flows in through `NativeInitCtx::mailer().handle_store()`
//! at boot; the chassis builder's `with_actor::<HandleCapability>(())`
//! is the boot site.

// `#[handler]` methods take their decoded payload by value per the
// ADR-0033 dispatch ABI; the macro-generated trampoline owns the
// decoded bytes so callers can't see references.
#![allow(clippy::needless_pass_by_value)]

// Handler-signature kinds must be importable at file root because
// `#[bridge]` emits `impl HandlesKind<K> for X {}` markers as siblings
// of the mod (always-on, outside the cfg gate).
use aether_kinds::{HandleDescribe, HandlePin, HandlePublish, HandleRelease, HandleUnpin};

#[aether_actor::bridge(singleton)]
mod native {
    use std::sync::Arc;

    use super::{HandleDescribe, HandlePin, HandlePublish, HandleRelease, HandleUnpin};
    use aether_actor::actor;
    use aether_kinds::{
        HandleDescribeResult, HandleError, HandlePinResult, HandlePublishResult,
        HandleReleaseResult, HandleSummary, HandleUnpinResult,
    };
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::chassis::error::BootError;
    use aether_substrate::handle_store::{
        HandleStore, HandleStoreSnapshot, HandleSummary as StoreSummary, PutError,
    };

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
        fn init((): (), ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            let store = Arc::clone(ctx.mailer().handle_store());
            Ok(Self { store })
        }

        /// Publish bytes under a fresh handle id.
        ///
        /// # Agent
        /// Reply: `HandlePublishResult`.
        #[handler]
        fn on_publish(&self, _ctx: &mut NativeCtx<'_>, mail: HandlePublish) -> HandlePublishResult {
            let id = self.store.next_ephemeral();
            match self.store.put(id, mail.kind_id, mail.bytes) {
                Ok(()) => {
                    // Hold a reference on behalf of the publishing
                    // component. Drop / explicit release decrements;
                    // on zero the entry stays in the store (subject
                    // to LRU eviction under pressure).
                    self.store.inc_ref(id);
                    HandlePublishResult::Ok {
                        kind_id: mail.kind_id,
                        id,
                    }
                }
                Err(e) => HandlePublishResult::Err {
                    kind_id: mail.kind_id,
                    error: put_error_to_handle_error(e),
                },
            }
        }

        /// Decrement a handle's refcount. SDK-side `Handle<K>::Drop`
        /// fires this; explicit `Ctx::release` paths also use it.
        ///
        /// # Agent
        /// Reply: `HandleReleaseResult`.
        #[handler]
        fn on_release(&self, _ctx: &mut NativeCtx<'_>, mail: HandleRelease) -> HandleReleaseResult {
            if self.store.dec_ref(mail.id) {
                HandleReleaseResult::Ok { id: mail.id }
            } else {
                HandleReleaseResult::Err {
                    id: mail.id,
                    error: HandleError::UnknownHandle,
                }
            }
        }

        /// Pin a handle so the LRU evictor skips it.
        ///
        /// # Agent
        /// Reply: `HandlePinResult`.
        #[handler]
        fn on_pin(&self, _ctx: &mut NativeCtx<'_>, mail: HandlePin) -> HandlePinResult {
            if self.store.pin(mail.id) {
                HandlePinResult::Ok { id: mail.id }
            } else {
                HandlePinResult::Err {
                    id: mail.id,
                    error: HandleError::UnknownHandle,
                }
            }
        }

        /// Clear the pinned flag on a handle.
        ///
        /// # Agent
        /// Reply: `HandleUnpinResult`.
        #[handler]
        fn on_unpin(&self, _ctx: &mut NativeCtx<'_>, mail: HandleUnpin) -> HandleUnpinResult {
            if self.store.unpin(mail.id) {
                HandleUnpinResult::Ok { id: mail.id }
            } else {
                HandleUnpinResult::Err {
                    id: mail.id,
                    error: HandleError::UnknownHandle,
                }
            }
        }

        /// Summarize the persistent handle store (ADR-0049 §10). `max`
        /// caps the top-N lists; the handler clamps it to `[1, 256]`
        /// (a 0 / absent request lands on the v1 default of 16).
        ///
        /// # Agent
        /// Reply: `HandleDescribeResult`.
        #[handler]
        fn on_describe(
            &self,
            _ctx: &mut NativeCtx<'_>,
            mail: HandleDescribe,
        ) -> HandleDescribeResult {
            let max = clamp_describe_max(mail.max);
            let snap = self.store.inspect(max);
            snapshot_to_result(&snap)
        }
    }

    /// Default top-N when the request asks for 0 (ADR-0049 §10 follow-up
    /// text); clamp ceiling.
    const DESCRIBE_DEFAULT_MAX: u32 = 16;
    const DESCRIBE_MAX_CAP: u32 = 256;

    fn clamp_describe_max(requested: u32) -> usize {
        let n = if requested == 0 {
            DESCRIBE_DEFAULT_MAX
        } else {
            requested.min(DESCRIBE_MAX_CAP)
        };
        n as usize
    }

    fn summary_to_wire(s: &StoreSummary) -> HandleSummary {
        HandleSummary {
            handle_id: s.handle_id,
            kind_id: s.kind_id,
            bytes_len: s.bytes_len,
            pinned: s.pinned,
            refcount: s.refcount,
            created_at_ms: s.created_at_ms,
        }
    }

    fn snapshot_to_result(snap: &HandleStoreSnapshot) -> HandleDescribeResult {
        let cast = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
        HandleDescribeResult {
            total_entries: cast(snap.total_entries),
            in_memory_entries: cast(snap.in_memory_entries),
            on_disk_entries: cast(snap.on_disk_entries),
            pinned_entries: cast(snap.pinned_entries),
            in_memory_bytes: snap.in_memory_bytes,
            on_disk_bytes: snap.on_disk_bytes,
            on_disk_budget_bytes: snap.on_disk_budget_bytes,
            top_by_size: snap.top_by_size.iter().map(summary_to_wire).collect(),
            top_by_recency: snap.top_by_recency.iter().map(summary_to_wire).collect(),
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

        use aether_actor::Addressable;
        use aether_data::{HandleId, Kind, KindId};
        use aether_data::{SessionToken, Uuid};

        use super::{
            Arc, BootError, HandleCapability, HandlePublish, HandlePublishResult, HandleStore,
        };
        use crate::test_chassis::{TestChassis, boot_test_chassis_with};
        use aether_kinds::descriptors;
        use aether_kinds::trace::Nanos;
        use aether_substrate::chassis::builder::Builder;
        use aether_substrate::mail::MailId;
        use aether_substrate::mail::mailer::Mailer;
        use aether_substrate::mail::outbound::EgressEvent;
        use aether_substrate::mail::outbound::HubOutbound;
        use aether_substrate::mail::registry;
        use aether_substrate::mail::registry::OwnedDispatch;
        use aether_substrate::mail::registry::{MailboxEntry, Registry};
        use aether_substrate::mail::{MailRef, Source, SourceAddr};

        fn fresh_substrate() -> (
            Arc<HandleStore>,
            Arc<Mailer>,
            Arc<Registry>,
            mpsc::Receiver<EgressEvent>,
        ) {
            let store = Arc::new(HandleStore::new(64 * 1024));
            let registry = Arc::new(Registry::new());
            for d in descriptors::all() {
                let _ = registry.register_kind_with_descriptor(d);
            }
            let (outbound, rx) = HubOutbound::attached_loopback();
            let mailer = Arc::new(
                Mailer::new(Arc::clone(&registry), Arc::clone(&store)).with_outbound(outbound),
            );
            (store, mailer, registry, rx)
        }

        fn session_reply_to() -> Source {
            Source::to(SourceAddr::Session(SessionToken(Uuid::from_u128(0xfeed))))
        }

        /// End-to-end through the cap: boot it via `with_actor`, push a
        /// `HandlePublish` mail at the registered mailbox, the dispatcher
        /// thread runs the macro-emitted `NativeDispatch::__aether_dispatch_envelope`
        /// which calls `on_publish`, the reply lands on the hub-outbound
        /// channel via the ADR-0112 `-> R` reply path →
        /// `Mailer::send_reply` → `outbound.send_reply`.
        #[test]
        fn capability_routes_publish_through_dispatcher_thread() {
            let (store, mailer, registry, rx) = fresh_substrate();

            let chassis = boot_test_chassis_with::<HandleCapability>(&registry, &mailer, ());

            let id = registry
                .lookup(HandleCapability::NAMESPACE)
                .expect("mailbox registered");
            let MailboxEntry::Inbox { handler, .. } = registry.entry(id).expect("entry") else {
                panic!("expected mailbox entry");
            };

            let req = HandlePublish {
                kind_id: KindId(0xCAFE),
                bytes: vec![1, 2, 3, 4, 5],
            };
            let bytes = req.encode_into_bytes();
            handler.enqueue(OwnedDispatch::disarmed(
                <HandlePublish as Kind>::ID,
                "aether.handle.publish".to_owned(),
                None,
                session_reply_to(),
                MailRef::from(bytes),
                1,
                MailId::NONE,
                MailId::NONE,
                None,
                Nanos(0),
                0,
                aether_data::MailboxId(0),
            ));

            let deadline = Instant::now() + Duration::from_secs(2);
            let frame = loop {
                if let Ok(f) = rx.try_recv() {
                    break f;
                }
                assert!(
                    Instant::now() < deadline,
                    "publish reply did not arrive within deadline"
                );
                thread::sleep(Duration::from_millis(5));
            };
            let payload = match frame {
                EgressEvent::ToSession { payload, .. } => payload,
                other => panic!("expected ToSession egress, got {other:?}"),
            };
            let result = HandlePublishResult::decode_from_bytes(&payload)
                .expect("test setup: HandlePublishResult decodes");
            let HandlePublishResult::Ok {
                kind_id,
                id: handle_id,
            } = result
            else {
                panic!("expected Ok, got {result:?}");
            };
            assert_eq!(kind_id, KindId(0xCAFE));
            assert_ne!(handle_id, HandleId(0));
            let (stored_kind, stored_bytes) = store
                .get(handle_id)
                .expect("test setup: stored handle should be retrievable");
            assert_eq!(stored_kind, KindId(0xCAFE));
            assert_eq!(stored_bytes, vec![1, 2, 3, 4, 5]);

            drop(chassis);
        }

        /// `aether.handle.describe` flows through the cap and returns a
        /// `HandleDescribeResult` whose counts match the store contents
        /// (ADR-0049 §10).
        #[test]
        fn capability_describe_summarizes_store() {
            use aether_kinds::{HandleDescribe, HandleDescribeResult};

            let (store, mailer, registry, rx) = fresh_substrate();
            // Pre-populate the store: 3 entries, 1 pinned.
            store
                .put(HandleId(1), KindId(0xA), vec![0u8; 100])
                .expect("put 1");
            store
                .put(HandleId(2), KindId(0xB), vec![0u8; 200])
                .expect("put 2");
            store
                .put(HandleId(3), KindId(0xC), vec![0u8; 50])
                .expect("put 3");
            store.pin(HandleId(2));

            let chassis = boot_test_chassis_with::<HandleCapability>(&registry, &mailer, ());

            let id = registry
                .lookup(HandleCapability::NAMESPACE)
                .expect("mailbox registered");
            let MailboxEntry::Inbox { handler, .. } = registry.entry(id).expect("entry") else {
                panic!("expected mailbox entry");
            };

            let req = HandleDescribe { max: 16 };
            let bytes = req.encode_into_bytes();
            handler.enqueue(OwnedDispatch::disarmed(
                <HandleDescribe as Kind>::ID,
                "aether.handle.describe".to_owned(),
                None,
                session_reply_to(),
                MailRef::from(bytes),
                1,
                MailId::NONE,
                MailId::NONE,
                None,
                Nanos(0),
                0,
                aether_data::MailboxId(0),
            ));

            let deadline = Instant::now() + Duration::from_secs(2);
            let frame = loop {
                if let Ok(f) = rx.try_recv() {
                    break f;
                }
                assert!(Instant::now() < deadline, "describe reply did not arrive");
                thread::sleep(Duration::from_millis(5));
            };
            let payload = match frame {
                EgressEvent::ToSession { payload, .. } => payload,
                other => panic!("expected ToSession egress, got {other:?}"),
            };
            let result = HandleDescribeResult::decode_from_bytes(&payload)
                .expect("HandleDescribeResult decodes");
            assert_eq!(result.total_entries, 3);
            assert_eq!(result.in_memory_entries, 3);
            assert_eq!(result.pinned_entries, 1);
            assert_eq!(result.in_memory_bytes, 350);
            // top_by_size descending: the 200-byte entry leads.
            assert_eq!(result.top_by_size.first().map(|s| s.bytes_len), Some(200));

            drop(chassis);
        }

        /// Channel-drop shutdown: drop the chassis, the cap's dispatcher
        /// thread exits within a generous deadline.
        #[test]
        fn shutdown_joins_dispatcher_thread() {
            let (_store, mailer, registry, _rx) = fresh_substrate();

            //noinspection DuplicatedCode
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
            registry.register_inbox(HandleCapability::NAMESPACE, registry::noop_handler());

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
