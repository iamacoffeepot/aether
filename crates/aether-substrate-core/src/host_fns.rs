// Host-function surface exposed to WASM components. Adding one is an
// explicit capability decision per ADR-0002 — every host function
// becomes reachable by every component that gets linked against this
// surface. Growth of this surface should be reviewed as deliberately
// as any other architectural change.

use std::sync::mpsc::{RecvTimeoutError, TryRecvError};
use std::time::{Duration, Instant};

use aether_hub_protocol::{ClaudeAddress, EngineMailFrame, EngineToHub};
use wasmtime::{Caller, Linker};

use crate::ctx::{StateBundle, SubstrateCtx};
use crate::mail::{MailboxId, ReplyTarget};

/// Status codes returned by the `reply_mail` host fn (ADR-0013 §3).
/// `0` is success; non-zero values distinguish call-site errors
/// (unknown handle, OOB guest memory, unregistered kind) from each
/// other so the SDK can surface a useful message. "Session gone" is
/// a named status but not yet populated — V0 cannot synchronously
/// detect that the hub has dropped a session; the outbound frame is
/// queued and if the session is gone the hub discards it silently.
pub const REPLY_OK: u32 = 0;
pub const REPLY_UNKNOWN_HANDLE: u32 = 1;
pub const REPLY_SESSION_GONE: u32 = 2;
pub const REPLY_OOB: u32 = 3;
pub const REPLY_KIND_NOT_FOUND: u32 = 4;

/// ADR-0016 §2: maximum size of a single state bundle. A `save_state`
/// call with `len > MAX_STATE_BUNDLE_BYTES` is rejected (status 3) and
/// the failure is recorded on the ctx so the substrate can abort the
/// replace. 1 MiB is conservative and matches ADR-0006's `MAX_FRAME_SIZE`
/// — revisitable once a real component actually hits the cap.
pub const MAX_STATE_BUNDLE_BYTES: usize = 1 << 20;

/// Status codes returned by the `save_state` host fn. 0 is success —
/// non-zero values let the SDK distinguish component bugs (OOB, no
/// memory) from policy rejection (over the size cap).
pub const SAVE_STATE_OK: u32 = 0;
pub const SAVE_STATE_NO_MEMORY: u32 = 1;
pub const SAVE_STATE_OOB: u32 = 2;
pub const SAVE_STATE_TOO_LARGE: u32 = 3;

/// Sentinel return values for `wait_reply_p32` (ADR-0042 §1). A
/// non-negative result is the number of payload bytes written to the
/// guest's out buffer; negatives are disjoint error codes.
pub const WAIT_TIMEOUT: i32 = -1;
pub const WAIT_BUFFER_TOO_SMALL: i32 = -2;
pub const WAIT_CANCELLED: i32 = -3;

/// Upper bound on the `timeout_ms` arg to `wait_reply_p32` (ADR-0042
/// §3). Matches `capture_frame`'s ceiling so any substrate-side bug
/// can't park a component thread indefinitely. Guests that want a
/// genuine "wait forever" pass this constant and accept the eventual
/// `WAIT_TIMEOUT`.
pub const MAX_WAIT_TIMEOUT_MS: u32 = 30_000;

/// Status codes returned by `handle_release_p32`, `handle_pin_p32`,
/// `handle_unpin_p32` (ADR-0045). `handle_publish_p32` returns the
/// minted handle id directly — `0` is the sentinel for failure (no
/// memory exported, OOB pointer, no store wired, eviction-failed).
pub const HANDLE_OK: u32 = 0;
pub const HANDLE_UNKNOWN: u32 = 1;
pub const HANDLE_NO_STORE: u32 = 2;

/// Sentinel return value from `handle_publish_p32` indicating the
/// publish failed. Matches `0` because the store's `next_ephemeral`
/// counter starts at `1`, so `0` cannot collide with a real handle
/// id. The SDK wraps this in `Option<Handle<K>>` so callers see the
/// failure as `None` rather than a magic-zero handle.
pub const HANDLE_PUBLISH_FAILED: u64 = 0;

/// Register the substrate host functions on `linker`. Components that
/// want these capabilities must be instantiated via a linker that this
/// function has been called on.
pub fn register(linker: &mut Linker<SubstrateCtx>) -> wasmtime::Result<()> {
    linker.func_wrap(
        "aether",
        "send_mail_p32",
        |mut caller: Caller<'_, SubstrateCtx>,
         recipient: u64,
         kind: u64,
         ptr: u32,
         len: u32,
         count: u32|
         -> u32 {
            let memory = match caller.get_export("memory").and_then(|e| e.into_memory()) {
                Some(m) => m,
                None => return 1, // guest exports no memory
            };

            // Copy the bytes out of guest memory so the mail outlives
            // the current host-function call (queues, other threads).
            let data = memory.data(&caller);
            let start = ptr as usize;
            let end = match start.checked_add(len as usize) {
                Some(e) if e <= data.len() => e,
                _ => return 2, // out-of-bounds
            };
            let payload = data[start..end].to_vec();

            let ctx = caller.data();
            ctx.send(MailboxId(recipient), kind, payload, count);
            0
        },
    )?;

    // `resolve_kind_p32` was retired in ADR-0030 Phase 2: kind ids are
    // the `fnv1a_64(KIND_DOMAIN ++ canonical(name, schema))` hash,
    // computed on the
    // guest side via the `Kind` derive's `const ID`. The host fn and
    // its `KIND_NOT_FOUND` sentinel are gone. Input-stream auto-
    // subscribe (the side-effect that used to ride this host fn)
    // moved to the guest SDK — ADR-0033 phase 3 has `#[handlers]`
    // prepend `ctx.subscribe_input::<K>()` for every `K::IS_INPUT`
    // handler kind to the user's `init` body.

    // ADR-0016 §2: save_state buffers the component's migration payload
    // into a substrate-owned slot on the store ctx. The guest passes a
    // `version` (opaque to the substrate) and a `(ptr, len)` pair
    // pointing at its own linear memory. Bytes are copied out so the
    // old instance can drop its memory normally; the substrate later
    // hands them to the new instance via `on_rehydrate`.
    //
    // Size cap is enforced before the guest memory is read — an
    // oversized request records an error and aborts without touching
    // memory. A subsequent `save_state` in the same `on_replace` call
    // overwrites; this matches ADR-0016 §2's "zero or one times" clause
    // for the success path and doesn't change behavior on error.
    linker.func_wrap(
        "aether",
        "save_state_p32",
        |mut caller: Caller<'_, SubstrateCtx>, version: u32, ptr: u32, len: u32| -> u32 {
            if len as usize > MAX_STATE_BUNDLE_BYTES {
                caller.data_mut().save_state_error = Some(format!(
                    "save_state: bundle size {} exceeds {} byte cap",
                    len, MAX_STATE_BUNDLE_BYTES,
                ));
                return SAVE_STATE_TOO_LARGE;
            }
            let memory = match caller.get_export("memory").and_then(|e| e.into_memory()) {
                Some(m) => m,
                None => return SAVE_STATE_NO_MEMORY,
            };
            let data = memory.data(&caller);
            let start = ptr as usize;
            let end = match start.checked_add(len as usize) {
                Some(e) if e <= data.len() => e,
                _ => return SAVE_STATE_OOB,
            };
            let bytes = data[start..end].to_vec();
            let ctx = caller.data_mut();
            ctx.saved_state = Some(StateBundle { version, bytes });
            SAVE_STATE_OK
        },
    )?;

    // ADR-0013 + ADR-0017: `reply_mail` addresses the originator of
    // the inbound mail whose sender handle the guest received.
    // Branches on the `ReplyEntry` variant:
    //   - Session: ship as a `ClaudeAddress::Session` frame through
    //     `HubOutbound` (same route as ADR-0013's original design).
    //   - Component: enqueue on the local `Mailer` via
    //     `SubstrateCtx::send`. Dropped-mailbox discard is handled
    //     there already, so a component that vanished between the
    //     request and the reply silently drops — the same contract
    //     as any other send to a dropped mailbox.
    linker.func_wrap(
        "aether",
        "reply_mail_p32",
        |mut caller: Caller<'_, SubstrateCtx>,
         sender: u32,
         kind: u64,
         ptr: u32,
         len: u32,
         count: u32|
         -> u32 {
            let memory = match caller.get_export("memory").and_then(|e| e.into_memory()) {
                Some(m) => m,
                None => return REPLY_OOB,
            };
            let data = memory.data(&caller);
            let start = ptr as usize;
            let end = match start.checked_add(len as usize) {
                Some(e) if e <= data.len() => e,
                _ => return REPLY_OOB,
            };
            let payload = data[start..end].to_vec();

            let ctx = caller.data();
            let Some(entry) = ctx.reply_table.resolve(sender) else {
                return REPLY_UNKNOWN_HANDLE;
            };
            // ADR-0042: echo the inbound correlation on every reply
            // path so a parked `wait_reply_p32` on the originator
            // can filter its own reply out of a busy inbox.
            let correlation = entry.correlation_id;
            match entry.target {
                ReplyTarget::Session(token) => {
                    let Some(kind_name) = ctx.registry.kind_name(kind) else {
                        return REPLY_KIND_NOT_FOUND;
                    };
                    let origin = ctx.registry.mailbox_name(ctx.sender);
                    ctx.outbound.send(EngineToHub::Mail(EngineMailFrame {
                        address: ClaudeAddress::Session(token),
                        kind_name,
                        payload,
                        origin,
                        correlation_id: correlation,
                    }));
                }
                ReplyTarget::Component(mbox) => {
                    // Validate the kind id cheaply — the guest might
                    // have passed a bogus one and we'd rather return
                    // a meaningful status than silently enqueue mail
                    // that the receiver can't decode.
                    if ctx.registry.kind_name(kind).is_none() {
                        return REPLY_KIND_NOT_FOUND;
                    }
                    ctx.send(mbox, kind, payload, count);
                }
                ReplyTarget::EngineMailbox {
                    engine_id,
                    mailbox_id,
                } => {
                    // ADR-0037 Phase 2: reply to a component on
                    // another engine. Validate the kind exists
                    // locally so we surface a meaningful status
                    // rather than shipping a frame the receiver
                    // can't decode. The hub forwards the frame to
                    // the target engine's connection as
                    // `HubToEngine::MailById`.
                    if ctx.registry.kind_name(kind).is_none() {
                        return REPLY_KIND_NOT_FOUND;
                    }
                    ctx.outbound.send(EngineToHub::MailToEngineMailbox(
                        aether_hub_protocol::MailToEngineMailboxFrame {
                            target_engine_id: engine_id,
                            target_mailbox_id: mailbox_id,
                            kind_id: kind,
                            payload,
                            count,
                            correlation_id: correlation,
                        },
                    ));
                }
                ReplyTarget::None => {
                    // Shouldn't happen — `ReplyEntry`s only get
                    // allocated for mail with a real reply target.
                    // Treat as unknown-handle to avoid silent drops.
                    return REPLY_UNKNOWN_HANDLE;
                }
            }
            REPLY_OK
        },
    )?;

    // `resolve_mailbox_p32` was retired in ADR-0029: mailbox ids are
    // now a deterministic hash of the mailbox name, computed on the
    // guest side. The corresponding host fn is gone.

    // ADR-0042: synchronous mail wait via drain + buffer. The host
    // fn drains the component's own mpsc inbox in a loop (the same
    // `Receiver<Mail>` the dispatcher reads from — moved onto
    // `SubstrateCtx` at spawn so both sides can reach it). Matching
    // mail is copied into the guest's `out` buffer and returned as
    // a byte count; non-matching mail is pushed onto the component's
    // overflow buffer, where the dispatcher drains it FIFO-ahead of
    // the mpsc on its next pass. Timeout → `-1`. Reply too big →
    // `-2`. Inbox disconnected (teardown) → `-3`.
    //
    // The drain runs on the dispatcher thread because the guest's
    // host call IS the dispatcher (ADR-0038 actor-per-component).
    // Other senders keep pushing into the mpsc during the wait;
    // non-match accumulates in overflow until drain returns.
    linker.func_wrap(
        "aether",
        "wait_reply_p32",
        |mut caller: Caller<'_, SubstrateCtx>,
         expected_kind: u64,
         out_ptr: u32,
         out_cap: u32,
         timeout_ms: u32,
         expected_correlation: u64|
         -> i32 {
            let clamped = timeout_ms.min(MAX_WAIT_TIMEOUT_MS);
            let deadline = Instant::now() + Duration::from_millis(clamped as u64);

            // Drain loop. The inbox receiver lives on the ctx
            // (`inbox_rx`); we lock it for the duration of this
            // call. The same thread that owns the dispatcher is
            // the thread running this host fn, so nobody else is
            // contending for the lock — it's structural, not
            // performance-critical.
            let mail = {
                let ctx = caller.data();
                let rx_guard = ctx.inbox_rx.lock().unwrap();
                let Some(rx) = rx_guard.as_ref() else {
                    // No inbox installed — pathological. Treat
                    // as disconnect so the guest aborts rather
                    // than spins.
                    return WAIT_CANCELLED;
                };
                loop {
                    let recv = if clamped == 0 {
                        // `timeout_ms == 0`: try_recv only — drain
                        // whatever's already queued, stop without
                        // parking.
                        match rx.try_recv() {
                            Ok(m) => Ok(m),
                            Err(TryRecvError::Empty) => Err(RecvTimeoutError::Timeout),
                            Err(TryRecvError::Disconnected) => Err(RecvTimeoutError::Disconnected),
                        }
                    } else {
                        let remaining = deadline.saturating_duration_since(Instant::now());
                        if remaining.is_zero() {
                            Err(RecvTimeoutError::Timeout)
                        } else {
                            rx.recv_timeout(remaining)
                        }
                    };
                    match recv {
                        Ok(m)
                            if m.kind == expected_kind
                                && (expected_correlation == 0
                                    || m.reply_to.correlation_id == expected_correlation) =>
                        {
                            break m;
                        }
                        Ok(m) => {
                            // Non-match (wrong kind, or right kind but
                            // not our correlation): park on overflow
                            // so the dispatcher picks it up after we
                            // unwind. ADR-0042 correlation filter.
                            ctx.inbox_overflow.lock().unwrap().push_back(m);
                        }
                        Err(RecvTimeoutError::Timeout) => return WAIT_TIMEOUT,
                        Err(RecvTimeoutError::Disconnected) => return WAIT_CANCELLED,
                    }
                }
            };

            if mail.payload.len() > out_cap as usize {
                // ADR-0042: oversized payload preserves the caller's
                // right to retry with a larger buffer. Park the mail
                // back on overflow so a follow-up `wait_reply_p32`
                // with a bigger `out_cap` can pick it up via the
                // same drain (overflow is FIFO-first, so the retry
                // sees the saved mail before anything newer).
                caller
                    .data()
                    .inbox_overflow
                    .lock()
                    .unwrap()
                    .push_front(mail);
                return WAIT_BUFFER_TOO_SMALL;
            }

            let Some(memory) = caller.get_export("memory").and_then(|e| e.into_memory()) else {
                // Guest exports no memory; treat as buffer-unusable.
                // Put the mail back on overflow so it isn't lost.
                caller
                    .data()
                    .inbox_overflow
                    .lock()
                    .unwrap()
                    .push_front(mail);
                return WAIT_BUFFER_TOO_SMALL;
            };
            let start = out_ptr as usize;
            let Some(end) = start.checked_add(mail.payload.len()) else {
                caller
                    .data()
                    .inbox_overflow
                    .lock()
                    .unwrap()
                    .push_front(mail);
                return WAIT_BUFFER_TOO_SMALL;
            };
            let data = memory.data_mut(&mut caller);
            if end > data.len() {
                // Can't restore mail here — we've already moved past
                // the ctx borrow by grabbing `data_mut`. The mail is
                // dropped. Callers hitting this are misusing out_ptr;
                // the loss is on them.
                return WAIT_BUFFER_TOO_SMALL;
            }
            data[start..end].copy_from_slice(&mail.payload);

            mail.payload.len() as i32
        },
    )?;

    // ADR-0042: read back the correlation id the substrate minted
    // for this component's most recent `send_mail`. Sync SDK
    // wrappers call this right after a send to capture the id,
    // then pass it to `wait_reply_p32` so the drain loop picks out
    // the matching reply among any prior async-request replies
    // that share the same kind. Returns `0` (the
    // `NO_CORRELATION` sentinel) before any send has been made;
    // matches the "kind-only" fallback in `wait_reply_p32`.
    linker.func_wrap(
        "aether",
        "prev_correlation_p32",
        |caller: Caller<'_, SubstrateCtx>| -> u64 { caller.data().prev_correlation() },
    )?;

    // ADR-0045 typed-handle SDK: `publish` copies guest bytes into
    // the substrate-side handle store and returns a fresh ephemeral
    // id. The publisher's initial reference is recorded immediately
    // (refcount=1 after put), so a subsequent `release` from the
    // same component dec-refs it back to 0 and the entry becomes
    // LRU-eligible. `0` is the failure sentinel (matches the
    // store's `next_ephemeral` reservation of `0` as the no-handle
    // id).
    linker.func_wrap(
        "aether",
        "handle_publish_p32",
        |mut caller: Caller<'_, SubstrateCtx>, kind_id: u64, ptr: u32, len: u32| -> u64 {
            let memory = match caller.get_export("memory").and_then(|e| e.into_memory()) {
                Some(m) => m,
                None => return HANDLE_PUBLISH_FAILED,
            };
            let data = memory.data(&caller);
            let start = ptr as usize;
            let end = match start.checked_add(len as usize) {
                Some(e) if e <= data.len() => e,
                _ => return HANDLE_PUBLISH_FAILED,
            };
            let bytes = data[start..end].to_vec();

            let ctx = caller.data();
            let Some(store) = ctx.queue.handle_store() else {
                return HANDLE_PUBLISH_FAILED;
            };
            let id = store.next_ephemeral();
            if let Err(e) = store.put(id, kind_id, bytes) {
                tracing::warn!(
                    target: "aether_substrate::handle_store",
                    kind_id = format_args!("{kind_id:#x}"),
                    error = ?e,
                    "handle_publish failed",
                );
                return HANDLE_PUBLISH_FAILED;
            }
            // Hold a reference on behalf of the publishing
            // component. Drop / explicit `release` decrements; on
            // zero the entry stays in the store (subject to LRU
            // eviction under pressure).
            store.inc_ref(id);
            id
        },
    )?;

    // ADR-0045: drop the publisher's reference. `dec_ref` saturates
    // at zero so calling release on an already-released handle is
    // a no-op success rather than a panic.
    linker.func_wrap(
        "aether",
        "handle_release_p32",
        |caller: Caller<'_, SubstrateCtx>, id: u64| -> u32 {
            let ctx = caller.data();
            let Some(store) = ctx.queue.handle_store() else {
                return HANDLE_NO_STORE;
            };
            if store.dec_ref(id) {
                HANDLE_OK
            } else {
                HANDLE_UNKNOWN
            }
        },
    )?;

    // ADR-0045: pin / unpin let a component shield a handle from
    // LRU eviction even when its refcount drops to zero. Useful for
    // cross-frame caching where the publisher wants to release the
    // local guard without losing the entry.
    linker.func_wrap(
        "aether",
        "handle_pin_p32",
        |caller: Caller<'_, SubstrateCtx>, id: u64| -> u32 {
            let ctx = caller.data();
            let Some(store) = ctx.queue.handle_store() else {
                return HANDLE_NO_STORE;
            };
            if store.pin(id) {
                HANDLE_OK
            } else {
                HANDLE_UNKNOWN
            }
        },
    )?;

    linker.func_wrap(
        "aether",
        "handle_unpin_p32",
        |caller: Caller<'_, SubstrateCtx>, id: u64| -> u32 {
            let ctx = caller.data();
            let Some(store) = ctx.queue.handle_store() else {
                return HANDLE_NO_STORE;
            };
            if store.unpin(id) {
                HANDLE_OK
            } else {
                HANDLE_UNKNOWN
            }
        },
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use wasmtime::{Engine, Linker, Module, Store, TypedFunc};

    use super::*;
    use crate::handle_store::HandleStore;
    use crate::hub_client::HubOutbound;
    use crate::input::new_subscribers;
    use crate::mail::MailboxId;
    use crate::mailer::Mailer;
    use crate::registry::Registry;

    /// WAT module exposing thin wrappers over the four ADR-0045
    /// host fns. Each `(func (export ...))` matches the host-fn
    /// signature so the tests can call them through `TypedFunc`
    /// without indirection.
    const WAT_HANDLE_HOST_FNS: &str = r#"
        (module
          (import "aether" "handle_publish_p32"
            (func $publish (param i64 i32 i32) (result i64)))
          (import "aether" "handle_release_p32"
            (func $release (param i64) (result i32)))
          (import "aether" "handle_pin_p32"
            (func $pin (param i64) (result i32)))
          (import "aether" "handle_unpin_p32"
            (func $unpin (param i64) (result i32)))
          (memory (export "memory") 1)
          ;; 5 bytes at offset 100, used as the `publish` payload.
          (data (i32.const 100) "\01\02\03\04\05")

          (func (export "publish") (param i64) (result i64)
            (call $publish (local.get 0) (i32.const 100) (i32.const 5)))
          (func (export "publish_oob") (param i64) (result i64)
            (call $publish (local.get 0) (i32.const 99999999) (i32.const 5)))
          (func (export "release") (param i64) (result i32)
            (call $release (local.get 0)))
          (func (export "pin") (param i64) (result i32)
            (call $pin (local.get 0)))
          (func (export "unpin") (param i64) (result i32)
            (call $unpin (local.get 0)))
        )
    "#;

    struct Harness {
        store: Store<SubstrateCtx>,
        publish: TypedFunc<i64, i64>,
        publish_oob: TypedFunc<i64, i64>,
        release: TypedFunc<i64, i32>,
        pin_fn: TypedFunc<i64, i32>,
        unpin: TypedFunc<i64, i32>,
        handle_store: Arc<HandleStore>,
    }

    impl Harness {
        fn new() -> Self {
            Self::with_store(Arc::new(HandleStore::new(4096)))
        }

        fn with_store(handle_store: Arc<HandleStore>) -> Self {
            let registry = Arc::new(Registry::new());
            let queue = Arc::new(Mailer::new());
            queue.wire_handle_store(Arc::clone(&handle_store));
            // Wire registry+components so `Mailer::push` doesn't
            // panic; route_mail isn't called in these tests but
            // accessing `Mailer::handle_store` via the ctx still
            // wants the rest of the wiring sane.
            queue.wire(
                Arc::clone(&registry),
                Arc::new(std::sync::RwLock::new(Default::default())),
            );
            let outbound = HubOutbound::disconnected();

            let ctx = SubstrateCtx::new(
                MailboxId(0),
                registry,
                Arc::clone(&queue),
                outbound,
                new_subscribers(),
            );

            let engine = Engine::default();
            let mut linker: Linker<SubstrateCtx> = Linker::new(&engine);
            register(&mut linker).expect("register host fns");
            let wasm = wat::parse_str(WAT_HANDLE_HOST_FNS).expect("compile WAT");
            let module = Module::new(&engine, &wasm).expect("compile module");
            let mut store = Store::new(&engine, ctx);
            let instance = linker
                .instantiate(&mut store, &module)
                .expect("instantiate");

            let publish = instance
                .get_typed_func::<i64, i64>(&mut store, "publish")
                .unwrap();
            let publish_oob = instance
                .get_typed_func::<i64, i64>(&mut store, "publish_oob")
                .unwrap();
            let release = instance
                .get_typed_func::<i64, i32>(&mut store, "release")
                .unwrap();
            let pin_fn = instance
                .get_typed_func::<i64, i32>(&mut store, "pin")
                .unwrap();
            let unpin = instance
                .get_typed_func::<i64, i32>(&mut store, "unpin")
                .unwrap();

            Harness {
                store,
                publish,
                publish_oob,
                release,
                pin_fn,
                unpin,
                handle_store,
            }
        }
    }

    #[test]
    fn handle_publish_copies_guest_bytes_into_store_with_initial_refcount() {
        let mut h = Harness::new();
        let id = h
            .publish
            .call(&mut h.store, 0xCAFE_u64 as i64)
            .expect("publish call");
        assert_ne!(
            id, 0,
            "publish must mint a real id, not the failure sentinel"
        );
        let id = id as u64;
        // The handle store should now hold the bytes we baked into
        // the WAT data segment.
        let (kind, bytes) = h.handle_store.get(id).expect("entry present");
        assert_eq!(kind, 0xCAFE);
        assert_eq!(bytes, vec![1, 2, 3, 4, 5]);
        // Initial refcount is 1 — release once must drop it to 0
        // and clear the saturating floor without surfacing
        // UNKNOWN.
        let status = h
            .release
            .call(&mut h.store, id as i64)
            .expect("release call");
        assert_eq!(status, HANDLE_OK as i32);
    }

    #[test]
    fn handle_publish_oob_returns_failure_sentinel() {
        let mut h = Harness::new();
        let id = h
            .publish_oob
            .call(&mut h.store, 0xCAFE_u64 as i64)
            .expect("publish_oob call");
        assert_eq!(id, HANDLE_PUBLISH_FAILED as i64);
        // Nothing landed in the store.
        assert_eq!(h.handle_store.entry_count(), 0);
    }

    #[test]
    fn handle_release_unknown_id_returns_unknown_status() {
        let mut h = Harness::new();
        let status = h
            .release
            .call(&mut h.store, 0x1234_u64 as i64)
            .expect("release call");
        assert_eq!(status, HANDLE_UNKNOWN as i32);
    }

    #[test]
    fn handle_pin_then_unpin_round_trips() {
        let mut h = Harness::new();
        let id = h
            .publish
            .call(&mut h.store, 0xCAFE_u64 as i64)
            .expect("publish");

        let pin_status = h.pin_fn.call(&mut h.store, id).expect("pin");
        assert_eq!(pin_status, HANDLE_OK as i32);
        let unpin_status = h.unpin.call(&mut h.store, id).expect("unpin");
        assert_eq!(unpin_status, HANDLE_OK as i32);

        // Pinning an unknown id must surface as UNKNOWN — pin/unpin
        // shouldn't quietly succeed against a missing entry.
        let bad = h
            .pin_fn
            .call(&mut h.store, 0xBADBAD_u64 as i64)
            .expect("pin");
        assert_eq!(bad, HANDLE_UNKNOWN as i32);
    }

    #[test]
    fn handle_publish_with_unwired_store_returns_failure_sentinel() {
        // Set up a Mailer with no handle store wired. Every host
        // fn that needs the store should surface NO_STORE / the
        // failure sentinel and leave guest memory alone.
        let registry = Arc::new(Registry::new());
        let queue = Arc::new(Mailer::new());
        queue.wire(
            Arc::clone(&registry),
            Arc::new(std::sync::RwLock::new(Default::default())),
        );
        // No wire_handle_store call.
        let outbound = HubOutbound::disconnected();
        let ctx = SubstrateCtx::new(
            MailboxId(0),
            registry,
            Arc::clone(&queue),
            outbound,
            new_subscribers(),
        );

        let engine = Engine::default();
        let mut linker: Linker<SubstrateCtx> = Linker::new(&engine);
        register(&mut linker).expect("register");
        let wasm = wat::parse_str(WAT_HANDLE_HOST_FNS).unwrap();
        let module = Module::new(&engine, &wasm).unwrap();
        let mut store = Store::new(&engine, ctx);
        let instance = linker.instantiate(&mut store, &module).unwrap();

        let publish = instance
            .get_typed_func::<i64, i64>(&mut store, "publish")
            .unwrap();
        let release = instance
            .get_typed_func::<i64, i32>(&mut store, "release")
            .unwrap();
        let pin_fn = instance
            .get_typed_func::<i64, i32>(&mut store, "pin")
            .unwrap();

        assert_eq!(
            publish.call(&mut store, 0xCAFE_u64 as i64).unwrap(),
            HANDLE_PUBLISH_FAILED as i64,
        );
        assert_eq!(release.call(&mut store, 1).unwrap(), HANDLE_NO_STORE as i32,);
        assert_eq!(pin_fn.call(&mut store, 1).unwrap(), HANDLE_NO_STORE as i32);
    }
}
