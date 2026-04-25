//! ADR-0045 typed-handle sink. The `"handle"` sink owns
//! `Arc<HandleStore>` and dispatches the four request kinds defined
//! in `aether-kinds`:
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
//! all funnel through one router.
//!
//! The dispatcher runs synchronously on the sink dispatch thread —
//! fine for handle ops, which are sub-microsecond `RwLock`
//! acquisitions on the store. Components publish via the SDK's
//! `Ctx::publish` (encode + `send_postcard` + `wait_reply`); the
//! `Drop` impl on `Handle<K>` mails `HandleRelease` fire-and-forget
//! to keep teardown safe.

use std::sync::Arc;

use aether_kinds::{
    HandleError, HandlePin, HandlePinResult, HandlePublish, HandlePublishResult, HandleRelease,
    HandleReleaseResult, HandleUnpin, HandleUnpinResult,
};
use aether_mail::Kind;

use crate::handle_store::{HandleStore, PutError};
use crate::mail::ReplyTo;
use crate::mailer::Mailer;
use crate::registry::SinkHandler;

/// Build the `"handle"` sink handler. Boot calls this after
/// constructing the `HandleStore` and `Mailer`, and registers the
/// returned closure under the `"handle"` mailbox name. The closure
/// demultiplexes incoming mail by kind id and replies via
/// `mailer.send_reply` — see module docs for the per-kind contract.
pub fn handle_sink_handler(store: Arc<HandleStore>, mailer: Arc<Mailer>) -> SinkHandler {
    Arc::new(
        move |kind_id: u64,
              _kind_name: &str,
              _origin: Option<&str>,
              sender: ReplyTo,
              bytes: &[u8],
              _count: u32| {
            dispatch(&store, &mailer, kind_id, sender, bytes);
        },
    )
}

fn dispatch(store: &HandleStore, mailer: &Mailer, kind_id: u64, sender: ReplyTo, bytes: &[u8]) {
    if kind_id == <HandlePublish as Kind>::ID {
        dispatch_publish(store, mailer, sender, bytes);
    } else if kind_id == <HandleRelease as Kind>::ID {
        dispatch_release(store, mailer, sender, bytes);
    } else if kind_id == <HandlePin as Kind>::ID {
        dispatch_pin(store, mailer, sender, bytes);
    } else if kind_id == <HandleUnpin as Kind>::ID {
        dispatch_unpin(store, mailer, sender, bytes);
    } else {
        // Unknown kind on this sink — warn and drop. The sender's
        // wait_reply on a paired *Result kind will time out, which
        // is the right surface for "you mailed something the
        // handle sink doesn't know about."
        tracing::warn!(
            target: "aether_substrate::handle_sink",
            kind_id = format_args!("{kind_id:#x}"),
            "handle sink received unknown kind",
        );
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
                    kind_id: 0,
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
                    id: 0,
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
                    id: 0,
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
                    id: 0,
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
            existing_kind_id,
            requested_kind_id,
        } => HandleError::AdapterError(format!(
            "kind id mismatch: existing={existing_kind_id:#x} requested={requested_kind_id:#x}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
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

    fn build_harness() -> (
        Arc<HandleStore>,
        Arc<Mailer>,
        Arc<Registry>,
        std::sync::mpsc::Receiver<EngineToHub>,
        SinkHandler,
    ) {
        let store = Arc::new(HandleStore::new(64 * 1024));
        let registry = Arc::new(Registry::new());
        // Register every handle kind so send_reply can resolve their
        // names (the hub-bound EngineMailFrame path uses kind_name).
        for d in aether_kinds::descriptors::all() {
            let _ = registry.register_kind_with_descriptor(d);
        }
        let (outbound, rx) = HubOutbound::test_channel();
        let mailer = Arc::new(Mailer::new());
        mailer.wire(Arc::clone(&registry), Arc::new(RwLock::new(HashMap::new())));
        mailer.wire_outbound(outbound);
        mailer.wire_handle_store(Arc::clone(&store));
        let handler = handle_sink_handler(Arc::clone(&store), Arc::clone(&mailer));
        (store, mailer, registry, rx, handler)
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
        let (store, _mailer, _registry, rx, handler) = build_harness();
        let req = HandlePublish {
            kind_id: 0xCAFE,
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

        let frame = rx.try_recv().expect("publish_result frame");
        let payload = extract_payload(frame);
        let result: HandlePublishResult = postcard::from_bytes(&payload).unwrap();
        let HandlePublishResult::Ok { kind_id, id } = result else {
            panic!("expected Ok, got {result:?}");
        };
        assert_eq!(kind_id, 0xCAFE);
        assert!(id > 0, "id must be a real handle, not the failure sentinel");
        // Bytes landed in the store with refcount=1 (so eviction
        // pressure can't drop them silently).
        let (stored_kind, stored_bytes) = store.get(id).unwrap();
        assert_eq!(stored_kind, 0xCAFE);
        assert_eq!(stored_bytes, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn release_unknown_id_replies_err_unknown_handle() {
        let (_store, _mailer, _registry, rx, handler) = build_harness();
        let req = HandleRelease { id: 0xBAD };
        let bytes = postcard::to_allocvec(&req).unwrap();
        handler(
            <HandleRelease as Kind>::ID,
            "aether.handle.release",
            None,
            session_reply_to(),
            &bytes,
            1,
        );
        let frame = rx.try_recv().expect("release_result frame");
        let payload = extract_payload(frame);
        let result: HandleReleaseResult = postcard::from_bytes(&payload).unwrap();
        match result {
            HandleReleaseResult::Err { id, error } => {
                assert_eq!(id, 0xBAD);
                assert_eq!(error, HandleError::UnknownHandle);
            }
            other => panic!("expected Err(UnknownHandle), got {other:?}"),
        }
    }

    #[test]
    fn pin_then_unpin_round_trips_via_sink() {
        let (store, _mailer, _registry, rx, handler) = build_harness();
        // Publish first to mint an id.
        let publish_req = HandlePublish {
            kind_id: 0xCAFE,
            bytes: vec![1, 2, 3],
        };
        handler(
            <HandlePublish as Kind>::ID,
            "aether.handle.publish",
            None,
            session_reply_to(),
            &postcard::to_allocvec(&publish_req).unwrap(),
            1,
        );
        let publish_frame = rx.try_recv().unwrap();
        let HandlePublishResult::Ok { id, .. } =
            postcard::from_bytes(&extract_payload(publish_frame)).unwrap()
        else {
            panic!("expected Ok");
        };
        // Pin.
        let pin_req = HandlePin { id };
        handler(
            <HandlePin as Kind>::ID,
            "aether.handle.pin",
            None,
            session_reply_to(),
            &postcard::to_allocvec(&pin_req).unwrap(),
            1,
        );
        let frame = rx.try_recv().unwrap();
        let result: HandlePinResult = postcard::from_bytes(&extract_payload(frame)).unwrap();
        assert!(matches!(result, HandlePinResult::Ok { id: r } if r == id));
        // Unpin.
        let unpin_req = HandleUnpin { id };
        handler(
            <HandleUnpin as Kind>::ID,
            "aether.handle.unpin",
            None,
            session_reply_to(),
            &postcard::to_allocvec(&unpin_req).unwrap(),
            1,
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
        let (_store, _mailer, _registry, rx, handler) = build_harness();
        // Truncated postcard bytes — a `HandlePublish` is at least
        // a u64 + a varint length, so 1 byte is malformed.
        handler(
            <HandlePublish as Kind>::ID,
            "aether.handle.publish",
            None,
            session_reply_to(),
            &[0u8],
            1,
        );
        let frame = rx.try_recv().expect("err frame");
        let result: HandlePublishResult = postcard::from_bytes(&extract_payload(frame)).unwrap();
        match result {
            HandlePublishResult::Err { kind_id, error } => {
                assert_eq!(kind_id, 0);
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
        let (_store, _mailer, _registry, rx, handler) = build_harness();
        handler(
            0xDEAD,
            "test.unknown",
            None,
            session_reply_to(),
            &[1, 2, 3],
            1,
        );
        assert!(rx.try_recv().is_err(), "no reply for unknown kind");
    }

    /// Component-targeted reply path: when sender is
    /// `ReplyTarget::Component`, the reply lands on the component's
    /// inbox via `Mailer::push`, NOT on the hub outbound. Pin via a
    /// captured-bytes sink masquerading as the target component.
    #[test]
    fn component_reply_path_pushes_into_component_inbox() {
        let (_store, _mailer, registry, rx, handler) = build_harness();
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
            kind_id: 0xCAFE,
            bytes: vec![9, 9, 9],
        };
        handler(
            <HandlePublish as Kind>::ID,
            "aether.handle.publish",
            None,
            ReplyTo::to(ReplyTarget::Component(component_mbox)),
            &postcard::to_allocvec(&publish_req).unwrap(),
            1,
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
