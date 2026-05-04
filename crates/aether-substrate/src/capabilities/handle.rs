//! Issue 545 PR E1: collapsed `aether.handle` cap. Pre-PR-E1 the cap
//! lived split across `aether-kinds::handle::HandleCapability<B>`
//! (facade generic) and this file (concrete `HandleStoreBackend`).
//! The facade pattern (ADR-0075) is retired — caps are now regular
//! `#[actor]` blocks, same shape as wasm components.
//!
//! Owns the shared [`HandleStore`] (the same instance the substrate's
//! `Mailer::wire_handle_store` references for `Ref<Handle>` resolution)
//! and an [`Arc<Mailer>`] for routing reply mail. The dispatcher
//! thread owns the cap through the macro-emitted `Dispatch` impl;
//! channel-drop on shutdown disconnects the inbox and the cap drops
//! on the dispatcher thread.

use std::sync::Arc;

use aether_data::{Actor, ReplyTo, Singleton};
use aether_kinds::{
    HandleError, HandlePin, HandlePinResult, HandlePublish, HandlePublishResult, HandleRelease,
    HandleReleaseResult, HandleUnpin, HandleUnpinResult,
};

use crate::handle_store::{HandleStore, PutError};
use crate::mailer::Mailer;

/// `aether.handle` mailbox cap. Owns the substrate's `HandleStore`
/// and routes ADR-0045 publish/release/pin/unpin requests, replying
/// via `Mailer::send_reply`. Decode failure on a malformed payload
/// goes through the macro miss path (warn-log, no reply, sender's
/// `wait_reply` times out) — substrate-level invariant violation, not
/// user-recoverable input.
pub struct HandleCapability {
    store: Arc<HandleStore>,
    mailer: Arc<Mailer>,
}

impl HandleCapability {
    /// Construct against the substrate's [`HandleStore`] and
    /// [`Mailer`]. The store is shared with `Mailer::wire_handle_store`
    /// so dispatch-time `Ref<Handle>` resolution and capability-handled
    /// publish/release/pin/unpin observe the same entries.
    pub fn new(store: Arc<HandleStore>, mailer: Arc<Mailer>) -> Self {
        Self { store, mailer }
    }
}

impl Actor for HandleCapability {
    /// ADR-0045 + ADR-0074 Phase 5: chassis-owned mailbox under the
    /// `aether.<name>` namespace.
    const NAMESPACE: &'static str = "aether.handle";
}

impl Singleton for HandleCapability {}

#[aether_data::actor]
impl HandleCapability {
    /// Publish bytes under a fresh handle id.
    ///
    /// # Agent
    /// Reply: `HandlePublishResult`.
    #[aether_data::handler]
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

    /// Decrement a handle's refcount. SDK-side `Handle<K>::Drop`
    /// fires this; explicit `Ctx::release` paths also use it.
    ///
    /// # Agent
    /// Reply: `HandleReleaseResult`.
    #[aether_data::handler]
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

    /// Pin a handle so the LRU evictor skips it.
    ///
    /// # Agent
    /// Reply: `HandlePinResult`.
    #[aether_data::handler]
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

    /// Clear the pinned flag on a handle.
    ///
    /// # Agent
    /// Reply: `HandleUnpinResult`.
    #[aether_data::handler]
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::RwLock;
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    use aether_data::{HandleId, Kind, KindId};
    use aether_data::{SessionToken, Uuid};

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

    /// End-to-end through the cap: boot it, push a `HandlePublish`
    /// mail at the registered mailbox, the dispatcher thread runs the
    /// macro-emitted `Dispatch::__dispatch` which calls `on_publish`,
    /// the reply lands on the hub-outbound channel.
    #[test]
    fn capability_routes_publish_through_dispatcher_thread() {
        let (store, mailer, registry, rx) = fresh_substrate();

        let chassis = ChassisBuilder::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with(HandleCapability::new(
                Arc::clone(&store),
                Arc::clone(&mailer),
            ))
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
        let (store, mailer, registry, _rx) = fresh_substrate();

        let chassis = ChassisBuilder::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with(HandleCapability::new(
                Arc::clone(&store),
                Arc::clone(&mailer),
            ))
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
        registry.register_sink(HandleCapability::NAMESPACE, Arc::new(|_, _, _, _, _, _| {}));

        let err = ChassisBuilder::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with(HandleCapability::new(
                Arc::clone(&store),
                Arc::clone(&mailer),
            ))
            .build()
            .expect_err("collision must surface as BootError");
        assert!(matches!(
            err,
            BootError::MailboxAlreadyClaimed { ref name }
                if name == HandleCapability::NAMESPACE
        ));
    }
}
