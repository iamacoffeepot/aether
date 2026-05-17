// Host-function surface exposed to WASM components. Adding one is an
// explicit capability decision per ADR-0002 — every host function
// becomes reachable by every component that gets linked against this
// surface. Growth of this surface should be reviewed as deliberately
// as any other architectural change.

use wasmtime::{Caller, Linker};

use crate::actor::wasm::component::{ComponentCtx, StateBundle};
use crate::mail::{KindId, MailboxId, ReplyTarget};

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

/// Register the substrate host functions on `linker`. Components that
/// want these capabilities must be instantiated via a linker that this
/// function has been called on.
pub fn register(linker: &mut Linker<ComponentCtx>) -> wasmtime::Result<()> {
    linker.func_wrap(
        "aether",
        "send_mail_p32",
        |mut caller: Caller<'_, ComponentCtx>,
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
            ctx.send(MailboxId(recipient), KindId(kind), payload, count);
            0
        },
    )?;

    // `resolve_kind_p32` was retired in ADR-0030 Phase 2: kind ids are
    // the `fnv1a_64(KIND_DOMAIN ++ canonical(name, schema))` hash,
    // computed on the
    // guest side via the `Kind` derive's `const ID`. The host fn and
    // its `KIND_NOT_FOUND` sentinel are gone. Input-stream auto-
    // subscribe (the side-effect that used to ride this host fn)
    // moved to the guest SDK — ADR-0033 phase 3 has `#[actor]`
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
        |mut caller: Caller<'_, ComponentCtx>, version: u32, ptr: u32, len: u32| -> u32 {
            if len as usize > MAX_STATE_BUNDLE_BYTES {
                caller.data_mut().save_state_error = Some(format!(
                    "save_state: bundle size {len} exceeds {MAX_STATE_BUNDLE_BYTES} byte cap"
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
    //     `ComponentCtx::send`. Dropped-mailbox discard is handled
    //     there already, so a component that vanished between the
    //     request and the reply silently drops — the same contract
    //     as any other send to a dropped mailbox.
    linker.func_wrap(
        "aether",
        "reply_mail_p32",
        |mut caller: Caller<'_, ComponentCtx>,
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
            let kind = KindId(kind);
            match entry.target {
                ReplyTarget::Session(token) => {
                    let Some(kind_name) = ctx.registry.kind_name(kind) else {
                        return REPLY_KIND_NOT_FOUND;
                    };
                    let origin = ctx.registry.mailbox_name(ctx.sender);
                    ctx.outbound
                        .egress_to_session(token, &kind_name, payload, origin, correlation);
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
                    ctx.outbound.egress_to_engine_mailbox(
                        engine_id,
                        mailbox_id,
                        kind,
                        payload,
                        count,
                        correlation,
                    );
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

    // ADR-0042: synchronous mail wait, delegating to the trampoline's
    // `NativeBinding::wait_reply` (issue 634 Phase 4 PR 3). The
    // transport already owns inbox + overflow + correlation-filter;
    // the host fn just bridges between wasm linear memory and the
    // transport's `&mut [u8]` buffer.
    //
    // The drain runs on the dispatcher thread because the guest's
    // host call IS the dispatcher (ADR-0038 actor-per-component).
    // Other senders keep pushing into the transport's mpsc during
    // the wait; non-match accumulates in the transport's overflow
    // until drain returns.
    //
    // Sentinel codes: timeout → `-1`, reply too big for `out_cap` →
    // `-2` (envelope is parked back on overflow so a retry with a
    // larger buffer can pick it up), inbox disconnected → `-3`,
    // ctx without a wired transport → `-3` (test-path
    // pathological — production trampolines always wire one).
    linker.func_wrap(
        "aether",
        "wait_reply_p32",
        |mut caller: Caller<'_, ComponentCtx>,
         expected_kind: u64,
         out_ptr: u32,
         out_cap: u32,
         timeout_ms: u32,
         expected_correlation: u64|
         -> i32 {
            let clamped = timeout_ms.min(MAX_WAIT_TIMEOUT_MS);
            let Some(transport) = caller.data().binding.clone() else {
                // No trampoline transport wired — the ctx was built by
                // a test path that doesn't exercise wait_reply.
                // Surface as cancelled so the guest doesn't spin.
                tracing::error!(
                    target: "aether_substrate::host_fns",
                    "wait_reply_p32 called on a ComponentCtx with no transport wired",
                );
                return WAIT_CANCELLED;
            };

            // Resolve wasm linear memory and bounds-check `out_ptr +
            // out_cap` *before* calling into the transport so a bad
            // pointer doesn't burn a real envelope. After this point,
            // a `-2` from the transport means the payload exceeded
            // `out_cap` and the transport already parked the envelope
            // on its overflow for a retry.
            let Some(memory) = caller.get_export("memory").and_then(|e| e.into_memory()) else {
                return WAIT_BUFFER_TOO_SMALL;
            };
            let start = out_ptr as usize;
            let Some(end) = start.checked_add(out_cap as usize) else {
                return WAIT_BUFFER_TOO_SMALL;
            };
            let data = memory.data_mut(&mut caller);
            if end > data.len() {
                return WAIT_BUFFER_TOO_SMALL;
            }
            transport.wait_reply(
                expected_kind,
                &mut data[start..end],
                clamped,
                expected_correlation,
            )
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
        |caller: Caller<'_, ComponentCtx>| -> u64 { caller.data().prev_correlation() },
    )?;

    // HOST_FN_OK: ADR-0002 / issue 531. The BootError plumbing
    // can't ride a mail sink because mail is not dispatched until
    // the component finishes booting — the `init` FFI call itself
    // is the entry point, and a `Result::Err` returned from it
    // needs a side channel to ship the error string back to the
    // substrate before the FFI call returns. A host fn is the
    // only mechanism that's available pre-`init`-completion.
    //
    // Issue 525 Phase 4b / issue 531: stage a `BootError` message
    // for `Component::instantiate` to surface in `LoadResult::Err`
    // after the guest's `init` returns non-zero. The bytes are
    // copied out of guest memory before the call returns; OOB or
    // missing-memory drops silently — the guest's non-zero return
    // still triggers the failure path, just without a message
    // (`Component::instantiate` falls back to a generic "init
    // returned <rc> without staging an error" diagnostic).
    linker.func_wrap(
        "aether",
        "init_failed_p32",
        |mut caller: Caller<'_, ComponentCtx>, ptr: u32, len: u32| {
            let Some(memory) = caller.get_export("memory").and_then(|e| e.into_memory()) else {
                return;
            };
            let data = memory.data(&caller);
            let start = ptr as usize;
            let end = match start.checked_add(len as usize) {
                Some(e) if e <= data.len() => e,
                _ => return,
            };
            let msg = String::from_utf8_lossy(&data[start..end]).into_owned();
            caller.data_mut().init_failure = Some(msg);
        },
    )?;

    Ok(())
}
