//! ADR-0045 typed-handle sink dispatch logic. ADR-0070 Phase 2
//! moved the `aether.sink.handle` mailbox out of `SubstrateBoot`'s
//! inline registration and into [`crate::capabilities::handle`] —
//! this module retains the per-kind dispatch implementation, called
//! from the capability's dispatcher thread for each envelope it
//! receives.
//!
//! The four request kinds defined in `aether-kinds`:
//!
//! - `HandlePublish { kind_id, bytes }` → `HandlePublishResult { Ok { id } | Err }`
//! - `HandleRelease { id }` → `HandleReleaseResult { Ok | Err }`
//! - `HandlePin { id }` → `HandlePinResult { Ok | Err }`
//! - `HandleUnpin { id }` → `HandleUnpinResult { Ok | Err }`
//!
//! Each request is postcard-decoded, routed to the matching
//! `HandleStore` op, and replied with the paired `*Result` kind via
//! `Mailer::send_reply` — the same dispatch path the io and net
//! sinks use, so session / engine-mailbox / local-component replies
//! all funnel through one router. Components publish via the SDK's
//! `Ctx::publish` (encode + `Sink::send` + `wait_reply`); the
//! `Drop` impl on `Handle<K>` mails `HandleRelease` fire-and-forget
//! to keep teardown safe.

use crate::handle_store::{HandleStore, PutError};
use crate::mail::ReplyTo;
use crate::mailer::Mailer;
use aether_data::{HandleId, Kind, KindId};
use aether_kinds::{
    HandleError, HandlePin, HandlePinResult, HandlePublish, HandlePublishResult, HandleRelease,
    HandleReleaseResult, HandleUnpin, HandleUnpinResult,
};

/// Demultiplex one envelope's payload to the matching per-kind
/// handler. Called by [`crate::capabilities::handle::HandleCapability`]'s
/// dispatcher thread; tests call it directly to exercise the per-kind
/// logic without spinning up a capability.
pub(crate) fn dispatch(
    store: &HandleStore,
    mailer: &Mailer,
    kind: KindId,
    sender: ReplyTo,
    bytes: &[u8],
) {
    // Issue 466: `Kind::ID` is typed `KindId`, so the match arms read
    // each typed const directly.
    match kind {
        <HandlePublish as Kind>::ID => dispatch_publish(store, mailer, sender, bytes),
        <HandleRelease as Kind>::ID => dispatch_release(store, mailer, sender, bytes),
        <HandlePin as Kind>::ID => dispatch_pin(store, mailer, sender, bytes),
        <HandleUnpin as Kind>::ID => dispatch_unpin(store, mailer, sender, bytes),
        _ => {
            // Unknown kind on this sink — warn and drop. The sender's
            // wait_reply on a paired *Result kind will time out, which
            // is the right surface for "you mailed something the
            // handle sink doesn't know about."
            tracing::warn!(
                target: "aether_substrate::handle_sink",
                kind = %kind,
                "handle sink received unknown kind",
            );
        }
    }
}

fn dispatch_publish(store: &HandleStore, mailer: &Mailer, sender: ReplyTo, bytes: &[u8]) {
    let req: HandlePublish = match postcard::from_bytes(bytes) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                target: "aether_substrate::handle_sink",
                error = %e,
                "publish: decode failed, replying Err",
            );
            mailer.send_reply(
                sender,
                &HandlePublishResult::Err {
                    kind_id: KindId(0),
                    error: HandleError::AdapterError(format!("decode failed: {e}")),
                },
            );
            return;
        }
    };
    let id = store.next_ephemeral();
    match store.put(id, req.kind_id, req.bytes) {
        Ok(()) => {
            // Hold a reference on behalf of the publishing
            // component. Drop / explicit release decrements; on
            // zero the entry stays in the store (subject to LRU
            // eviction under pressure).
            store.inc_ref(id);
            mailer.send_reply(
                sender,
                &HandlePublishResult::Ok {
                    kind_id: req.kind_id,
                    id,
                },
            );
        }
        Err(e) => {
            mailer.send_reply(
                sender,
                &HandlePublishResult::Err {
                    kind_id: req.kind_id,
                    error: put_error_to_handle_error(e),
                },
            );
        }
    }
}

fn dispatch_release(store: &HandleStore, mailer: &Mailer, sender: ReplyTo, bytes: &[u8]) {
    let req: HandleRelease = match postcard::from_bytes(bytes) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                target: "aether_substrate::handle_sink",
                error = %e,
                "release: decode failed, replying Err",
            );
            mailer.send_reply(
                sender,
                &HandleReleaseResult::Err {
                    id: HandleId(0),
                    error: HandleError::AdapterError(format!("decode failed: {e}")),
                },
            );
            return;
        }
    };
    if store.dec_ref(req.id) {
        mailer.send_reply(sender, &HandleReleaseResult::Ok { id: req.id });
    } else {
        mailer.send_reply(
            sender,
            &HandleReleaseResult::Err {
                id: req.id,
                error: HandleError::UnknownHandle,
            },
        );
    }
}

fn dispatch_pin(store: &HandleStore, mailer: &Mailer, sender: ReplyTo, bytes: &[u8]) {
    let req: HandlePin = match postcard::from_bytes(bytes) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                target: "aether_substrate::handle_sink",
                error = %e,
                "pin: decode failed, replying Err",
            );
            mailer.send_reply(
                sender,
                &HandlePinResult::Err {
                    id: HandleId(0),
                    error: HandleError::AdapterError(format!("decode failed: {e}")),
                },
            );
            return;
        }
    };
    if store.pin(req.id) {
        mailer.send_reply(sender, &HandlePinResult::Ok { id: req.id });
    } else {
        mailer.send_reply(
            sender,
            &HandlePinResult::Err {
                id: req.id,
                error: HandleError::UnknownHandle,
            },
        );
    }
}

fn dispatch_unpin(store: &HandleStore, mailer: &Mailer, sender: ReplyTo, bytes: &[u8]) {
    let req: HandleUnpin = match postcard::from_bytes(bytes) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                target: "aether_substrate::handle_sink",
                error = %e,
                "unpin: decode failed, replying Err",
            );
            mailer.send_reply(
                sender,
                &HandleUnpinResult::Err {
                    id: HandleId(0),
                    error: HandleError::AdapterError(format!("decode failed: {e}")),
                },
            );
            return;
        }
    };
    if store.unpin(req.id) {
        mailer.send_reply(sender, &HandleUnpinResult::Ok { id: req.id });
    } else {
        mailer.send_reply(
            sender,
            &HandleUnpinResult::Err {
                id: req.id,
                error: HandleError::UnknownHandle,
            },
        );
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
    use std::sync::Arc;
    use std::sync::RwLock;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use aether_hub_protocol::EngineToHub;
    use aether_kinds::{
        HandleError, HandlePin, HandlePinResult, HandlePublish, HandlePublishResult, HandleRelease,
        HandleReleaseResult, HandleUnpin, HandleUnpinResult,
    };

    use super::*;
    use crate::hub_client::HubOutbound;
    use crate::mail::{Mail, MailboxId, ReplyTarget, ReplyTo};
    use crate::registry::Registry;

    /// Wires the substrate state `dispatch` reads from. Returns owned
    /// handles so each test can call [`dispatch`] directly with the
    /// store + mailer it constructs.
    fn build_harness() -> (
        Arc<HandleStore>,
        Arc<Mailer>,
        Arc<Registry>,
        std::sync::mpsc::Receiver<EngineToHub>,
    ) {
        let store = Arc::new(HandleStore::new(64 * 1024));
        let registry = Arc::new(Registry::new());
        // Register every handle kind so send_reply can resolve their
        // names (the hub-bound EngineMailFrame path uses kind_name).
        for d in aether_kinds::descriptors::all() {
            let _ = registry.register_kind_with_descriptor(d);
        }
        let (outbound, rx) = HubOutbound::attached_loopback();
        let mailer = Arc::new(Mailer::new());
        mailer.wire(Arc::clone(&registry), Arc::new(RwLock::new(HashMap::new())));
        mailer.wire_outbound(outbound);
        mailer.wire_handle_store(Arc::clone(&store));
        (store, mailer, registry, rx)
    }

    /// Build a fake session-targeted ReplyTo so `send_reply` runs the
    /// hub-outbound path, which lands a typed `EngineMailFrame` we
    /// can decode in the test.
    fn session_reply_to() -> ReplyTo {
        use aether_hub_protocol::{SessionToken, Uuid};
        ReplyTo::to(ReplyTarget::Session(SessionToken(Uuid::from_u128(0xfeed))))
    }

    fn extract_payload(frame: EngineToHub) -> Vec<u8> {
        match frame {
            EngineToHub::Mail(m) => m.payload,
            other => panic!("expected Mail frame, got {other:?}"),
        }
    }

    #[test]
    fn publish_replies_with_fresh_id_and_initial_refcount() {
        let (store, mailer, _registry, rx) = build_harness();
        let req = HandlePublish {
            kind_id: KindId(0xCAFE),
            bytes: vec![1, 2, 3, 4, 5],
        };
        let bytes = postcard::to_allocvec(&req).unwrap();
        dispatch(
            &store,
            &mailer,
            <HandlePublish as Kind>::ID,
            session_reply_to(),
            &bytes,
        );

        let frame = rx.try_recv().expect("publish_result frame");
        let payload = extract_payload(frame);
        let result: HandlePublishResult = postcard::from_bytes(&payload).unwrap();
        let HandlePublishResult::Ok { kind_id, id } = result else {
            panic!("expected Ok, got {result:?}");
        };
        assert_eq!(kind_id, KindId(0xCAFE));
        assert!(
            id.0 > 0,
            "id must be a real handle, not the failure sentinel"
        );
        // Bytes landed in the store with refcount=1 (so eviction
        // pressure can't drop them silently).
        let (stored_kind, stored_bytes) = store.get(id).unwrap();
        assert_eq!(stored_kind, KindId(0xCAFE));
        assert_eq!(stored_bytes, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn release_unknown_id_replies_err_unknown_handle() {
        let (store, mailer, _registry, rx) = build_harness();
        let req = HandleRelease {
            id: HandleId(0xBAD),
        };
        let bytes = postcard::to_allocvec(&req).unwrap();
        dispatch(
            &store,
            &mailer,
            <HandleRelease as Kind>::ID,
            session_reply_to(),
            &bytes,
        );
        let frame = rx.try_recv().expect("release_result frame");
        let payload = extract_payload(frame);
        let result: HandleReleaseResult = postcard::from_bytes(&payload).unwrap();
        match result {
            HandleReleaseResult::Err { id, error } => {
                assert_eq!(id, HandleId(0xBAD));
                assert_eq!(error, HandleError::UnknownHandle);
            }
            other => panic!("expected Err(UnknownHandle), got {other:?}"),
        }
    }

    #[test]
    fn pin_then_unpin_round_trips_via_sink() {
        let (store, mailer, _registry, rx) = build_harness();
        // Publish first to mint an id.
        let publish_req = HandlePublish {
            kind_id: KindId(0xCAFE),
            bytes: vec![1, 2, 3],
        };
        dispatch(
            &store,
            &mailer,
            <HandlePublish as Kind>::ID,
            session_reply_to(),
            &postcard::to_allocvec(&publish_req).unwrap(),
        );
        let publish_frame = rx.try_recv().unwrap();
        let HandlePublishResult::Ok { id, .. } =
            postcard::from_bytes(&extract_payload(publish_frame)).unwrap()
        else {
            panic!("expected Ok");
        };
        // Pin.
        let pin_req = HandlePin { id };
        dispatch(
            &store,
            &mailer,
            <HandlePin as Kind>::ID,
            session_reply_to(),
            &postcard::to_allocvec(&pin_req).unwrap(),
        );
        let frame = rx.try_recv().unwrap();
        let result: HandlePinResult = postcard::from_bytes(&extract_payload(frame)).unwrap();
        assert!(matches!(result, HandlePinResult::Ok { id: r } if r == id));
        // Unpin.
        let unpin_req = HandleUnpin { id };
        dispatch(
            &store,
            &mailer,
            <HandleUnpin as Kind>::ID,
            session_reply_to(),
            &postcard::to_allocvec(&unpin_req).unwrap(),
        );
        let frame = rx.try_recv().unwrap();
        let result: HandleUnpinResult = postcard::from_bytes(&extract_payload(frame)).unwrap();
        assert!(matches!(result, HandleUnpinResult::Ok { id: r } if r == id));
        // Sanity-check the store actually has the entry.
        assert!(store.contains(id));
    }

    /// Decode failure: send malformed bytes claiming to be a publish
    /// request. The sink replies Err with empty kind_id echo +
    /// `AdapterError` carrying the diagnostic.
    #[test]
    fn publish_decode_failure_replies_adapter_error_with_zero_kind_id() {
        let (store, mailer, _registry, rx) = build_harness();
        // Truncated postcard bytes — a `HandlePublish` is at least
        // a u64 + a varint length, so 1 byte is malformed.
        dispatch(
            &store,
            &mailer,
            <HandlePublish as Kind>::ID,
            session_reply_to(),
            &[0u8],
        );
        let frame = rx.try_recv().expect("err frame");
        let result: HandlePublishResult = postcard::from_bytes(&extract_payload(frame)).unwrap();
        match result {
            HandlePublishResult::Err { kind_id, error } => {
                assert_eq!(kind_id, KindId(0));
                assert!(matches!(error, HandleError::AdapterError(_)));
            }
            other => panic!("expected Err, got {other:?}"),
        }
    }

    /// Unknown kinds on the sink are dropped silently (warn-log
    /// only). Pin the contract: no reply lands, the receiver chan
    /// stays empty.
    #[test]
    fn unknown_kind_on_handle_sink_drops_without_reply() {
        let (store, mailer, _registry, rx) = build_harness();
        dispatch(
            &store,
            &mailer,
            KindId(0xDEAD),
            session_reply_to(),
            &[1, 2, 3],
        );
        assert!(rx.try_recv().is_err(), "no reply for unknown kind");
    }

    /// Component-targeted reply path: when sender is
    /// `ReplyTarget::Component`, the reply lands on the component's
    /// inbox via `Mailer::push`, NOT on the hub outbound. Pin via a
    /// captured-bytes sink masquerading as the target component.
    #[test]
    fn component_reply_path_pushes_into_component_inbox() {
        let (store, mailer, registry, rx) = build_harness();
        let captured = Arc::new(RwLock::new(Vec::new()));
        let counter = Arc::new(AtomicUsize::new(0));
        let captured_inner = Arc::clone(&captured);
        let counter_inner = Arc::clone(&counter);
        let component_mbox = registry.register_sink(
            "test.component_target",
            Arc::new(
                move |_kind_id, _kind_name, _origin, _sender, bytes: &[u8], _count| {
                    captured_inner.write().unwrap().push(bytes.to_vec());
                    counter_inner.fetch_add(1, Ordering::SeqCst);
                },
            ),
        );

        let publish_req = HandlePublish {
            kind_id: KindId(0xCAFE),
            bytes: vec![9, 9, 9],
        };
        dispatch(
            &store,
            &mailer,
            <HandlePublish as Kind>::ID,
            ReplyTo::to(ReplyTarget::Component(component_mbox)),
            &postcard::to_allocvec(&publish_req).unwrap(),
        );
        // Hub outbound stayed empty…
        assert!(rx.try_recv().is_err(), "component reply must not bubble");
        // …and the component's inbox got one frame.
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        let bytes = captured.read().unwrap();
        assert_eq!(bytes.len(), 1);
        let result: HandlePublishResult = postcard::from_bytes(&bytes[0]).unwrap();
        assert!(matches!(result, HandlePublishResult::Ok { .. }));
    }

    /// Keep the unused-import suppression for `Mail` — pulled in by
    /// rust-analyzer's import resolution but the tests use it via
    /// type inference only.
    #[allow(dead_code)]
    fn _silence_mail(_m: Mail, _id: MailboxId) {}
}
