//! ADR-0075 §Decision 3 backend for the [`aether_kinds::HandleCapability`]
//! facade (issue 533 PR D2). The cap and its `HandleBackend` trait
//! live in `aether-kinds` so wasm senders can address it without
//! pulling in `HandleStore` / `Mailer` types; this module supplies
//! the concrete backend the chassis installs at boot.
//!
//! Pre-PR-D2 the cap and dispatcher both lived here, with
//! `handle_sink::dispatch_*` doing per-kind kind-id matching. The
//! macro-emitted `Dispatch` impl on `HandleCapability` now does the
//! kind matching; the per-kind logic moved into the matching
//! [`HandleBackend`] methods on this struct.

use std::sync::Arc;

use aether_data::ReplyTo;
use aether_kinds::{
    HandleBackend, HandleError, HandlePin, HandlePinResult, HandlePublish, HandlePublishResult,
    HandleRelease, HandleReleaseResult, HandleUnpin, HandleUnpinResult,
};

use crate::handle_store::{HandleStore, PutError};
use crate::mailer::Mailer;

/// Substrate-side state for the handle cap. Holds the shared
/// [`HandleStore`] (the same instance the substrate's
/// `Mailer::wire_handle_store` references for `Ref<Handle>` resolution)
/// and an [`Arc<Mailer>`] for routing reply mail.
///
/// The dispatcher thread owns this through the macro-emitted
/// `Dispatch` impl on `aether_kinds::HandleCapability<Self>`; the
/// chassis stores only the `FacadeHandle` wrapper. On shutdown the
/// channel disconnects, the thread exits, and this struct drops on
/// the dispatcher thread — no cross-thread state to worry about.
pub struct HandleStoreBackend {
    store: Arc<HandleStore>,
    mailer: Arc<Mailer>,
}

impl HandleStoreBackend {
    /// Construct against the substrate's [`HandleStore`] and
    /// [`Mailer`]. The store is shared with `Mailer::wire_handle_store`
    /// so dispatch-time `Ref<Handle>` resolution and capability-handled
    /// publish/release/pin/unpin observe the same entries.
    pub fn new(store: Arc<HandleStore>, mailer: Arc<Mailer>) -> Self {
        Self { store, mailer }
    }
}

impl HandleBackend for HandleStoreBackend {
    fn on_publish(&mut self, sender: ReplyTo, mail: HandlePublish) {
        let id = self.store.next_ephemeral();
        match self.store.put(id, mail.kind_id, mail.bytes) {
            Ok(()) => {
                // Hold a reference on behalf of the publishing
                // component. Drop / explicit release decrements; on
                // zero the entry stays in the store (subject to LRU
                // eviction under pressure).
                self.store.inc_ref(id);
                self.mailer.send_reply(
                    sender,
                    &HandlePublishResult::Ok {
                        kind_id: mail.kind_id,
                        id,
                    },
                );
            }
            Err(e) => {
                self.mailer.send_reply(
                    sender,
                    &HandlePublishResult::Err {
                        kind_id: mail.kind_id,
                        error: put_error_to_handle_error(e),
                    },
                );
            }
        }
    }

    fn on_release(&mut self, sender: ReplyTo, mail: HandleRelease) {
        if self.store.dec_ref(mail.id) {
            self.mailer
                .send_reply(sender, &HandleReleaseResult::Ok { id: mail.id });
        } else {
            self.mailer.send_reply(
                sender,
                &HandleReleaseResult::Err {
                    id: mail.id,
                    error: HandleError::UnknownHandle,
                },
            );
        }
    }

    fn on_pin(&mut self, sender: ReplyTo, mail: HandlePin) {
        if self.store.pin(mail.id) {
            self.mailer
                .send_reply(sender, &HandlePinResult::Ok { id: mail.id });
        } else {
            self.mailer.send_reply(
                sender,
                &HandlePinResult::Err {
                    id: mail.id,
                    error: HandleError::UnknownHandle,
                },
            );
        }
    }

    fn on_unpin(&mut self, sender: ReplyTo, mail: HandleUnpin) {
        if self.store.unpin(mail.id) {
            self.mailer
                .send_reply(sender, &HandleUnpinResult::Ok { id: mail.id });
        } else {
            self.mailer.send_reply(
                sender,
                &HandleUnpinResult::Err {
                    id: mail.id,
                    error: HandleError::UnknownHandle,
                },
            );
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

// Decode failure for one of the four request kinds bypasses these
// methods entirely — the macro-emitted `Dispatch::__dispatch` body
// returns `None` on decode error, and the chassis-side dispatcher
// surfaces the miss as a `tracing::warn!` (see
// `ChassisCtx::spawn_actor_dispatcher`). The pre-PR-D2 sink replied
// with `HandleError::AdapterError("decode failed: …")`; the new
// behaviour is wait_reply-timeout instead, which is consistent with
// every other facade cap and acceptable because schema mismatches at
// this layer indicate a substrate-level invariant violation rather
// than user-recoverable input.
#[allow(dead_code)]
fn _decode_failure_documentation() {}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::RwLock;
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    use aether_data::{Actor, HandleId, Kind, KindId};
    use aether_data::{SessionToken, Uuid};
    use aether_kinds::{HandleCapability, HandlePublish, HandlePublishResult};

    use super::*;
    use crate::capability::{BootError, ChassisBuilder};
    use crate::mail::{ReplyTarget, ReplyTo};
    use crate::mailer::Mailer;
    use crate::outbound::EgressEvent;
    use crate::registry::{MailboxEntry, Registry};

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
        let (outbound, rx) = crate::outbound::HubOutbound::attached_loopback();
        let mailer = Arc::new(Mailer::new());
        mailer.wire(Arc::clone(&registry), Arc::new(RwLock::new(HashMap::new())));
        mailer.wire_outbound(outbound);
        mailer.wire_handle_store(Arc::clone(&store));
        (store, mailer, registry, rx)
    }

    fn session_reply_to() -> ReplyTo {
        ReplyTo::to(ReplyTarget::Session(SessionToken(Uuid::from_u128(0xfeed))))
    }

    /// End-to-end through the facade: boot the cap, push a
    /// `HandlePublish` mail at the registered mailbox, the dispatcher
    /// thread runs the macro-emitted `Dispatch::__dispatch` which
    /// delegates to the backend's `on_publish`, the reply lands on
    /// the hub-outbound channel.
    #[test]
    fn capability_routes_publish_through_dispatcher_thread() {
        let (store, mailer, registry, rx) = fresh_substrate();

        let chassis = ChassisBuilder::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with_facade(HandleCapability::new(HandleStoreBackend::new(
                Arc::clone(&store),
                Arc::clone(&mailer),
            )))
            .build()
            .expect("capability boots");

        let id = registry
            .lookup(<HandleCapability<HandleStoreBackend> as Actor>::NAMESPACE)
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
        let (store, mailer, registry, _rx) = fresh_substrate();

        let chassis = ChassisBuilder::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with_facade(HandleCapability::new(HandleStoreBackend::new(
                Arc::clone(&store),
                Arc::clone(&mailer),
            )))
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
        let (store, mailer, registry, _rx) = fresh_substrate();
        registry.register_sink(
            <HandleCapability<HandleStoreBackend> as Actor>::NAMESPACE,
            Arc::new(|_, _, _, _, _, _| {}),
        );

        let err = ChassisBuilder::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with_facade(HandleCapability::new(HandleStoreBackend::new(
                Arc::clone(&store),
                Arc::clone(&mailer),
            )))
            .build()
            .expect_err("collision must surface as BootError");
        assert!(matches!(
            err,
            BootError::MailboxAlreadyClaimed { ref name }
                if name == <HandleCapability<HandleStoreBackend> as Actor>::NAMESPACE
        ));
    }
}
