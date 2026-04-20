// Host-function surface exposed to WASM components. Adding one is an
// explicit capability decision per ADR-0002 — every host function
// becomes reachable by every component that gets linked against this
// surface. Growth of this surface should be reviewed as deliberately
// as any other architectural change.

use aether_hub_protocol::{ClaudeAddress, EngineMailFrame, EngineToHub};
use aether_kinds::InputStream;
use wasmtime::{Caller, Linker};

use crate::ctx::{StateBundle, SubstrateCtx};
use crate::mail::MailboxId;
use crate::sender_table::SenderEntry;

/// Returned by `resolve_kind` / `resolve_mailbox` when the requested
/// name has not been registered. Guests use this as a "lookup failed"
/// sentinel.
pub const KIND_NOT_FOUND: u32 = u32::MAX;
pub const MAILBOX_NOT_FOUND: u32 = u32::MAX;

/// Map a resolved kind name to the substrate input stream it belongs
/// to. The four built-in input kinds (`aether.tick`, `aether.key`,
/// `aether.mouse_button`, `aether.mouse_move`) are the only inputs
/// today; a new input kind here would also need a publisher change
/// in `main.rs`. Keeping the mapping in a small match — rather than a
/// boot-time map on `Registry` — lets the guest's `resolve_kind`
/// auto-subscribe without any new shared state: the Kind trait's
/// `IS_INPUT` const on the kind definition is what the derive flags,
/// and this mapping is the substrate-side counterpart.
fn input_stream_for_name(name: &str) -> Option<InputStream> {
    match name {
        "aether.tick" => Some(InputStream::Tick),
        "aether.key" => Some(InputStream::Key),
        "aether.mouse_move" => Some(InputStream::MouseMove),
        "aether.mouse_button" => Some(InputStream::MouseButton),
        _ => None,
    }
}

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

/// Register the substrate host functions on `linker`. Components that
/// want these capabilities must be instantiated via a linker that this
/// function has been called on.
pub fn register(linker: &mut Linker<SubstrateCtx>) -> wasmtime::Result<()> {
    linker.func_wrap(
        "aether",
        "send_mail_p32",
        |mut caller: Caller<'_, SubstrateCtx>,
         recipient: u32,
         kind: u32,
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

    linker.func_wrap(
        "aether",
        "resolve_kind_p32",
        |mut caller: Caller<'_, SubstrateCtx>, name_ptr: u32, name_len: u32| -> u32 {
            let memory = match caller.get_export("memory").and_then(|e| e.into_memory()) {
                Some(m) => m,
                None => return KIND_NOT_FOUND,
            };
            let data = memory.data(&caller);
            let start = name_ptr as usize;
            let end = match start.checked_add(name_len as usize) {
                Some(e) if e <= data.len() => e,
                _ => return KIND_NOT_FOUND,
            };
            let name = match std::str::from_utf8(&data[start..end]) {
                Ok(s) => s,
                Err(_) => return KIND_NOT_FOUND,
            };
            let ctx = caller.data();
            let id = ctx.registry.kind_id(name).unwrap_or(KIND_NOT_FOUND);
            // ADR-0021 auto-subscribe: if the resolved kind is one of
            // the substrate's input streams, fold the caller's mailbox
            // into the subscriber set. A component declaring
            // `type Kinds = (Tick, ...)` gets Tick delivery without
            // ever sending `aether.control.subscribe_input`. Idempotent:
            // repeated resolves (e.g. replace_component) stay correct.
            if id != KIND_NOT_FOUND
                && let Some(stream) = input_stream_for_name(name)
            {
                ctx.input_subscribers
                    .write()
                    .unwrap()
                    .entry(stream)
                    .or_default()
                    .insert(ctx.sender);
            }
            id
        },
    )?;

    // Symmetric to `resolve_kind`: lookup a mailbox by its registered
    // name and return the `MailboxId`. Runtime-loaded components rely
    // on this to reach substrate-owned sinks (`render`,
    // `hub.claude.broadcast`, `aether.control`) without hardcoding
    // numeric ids — ADR-0010's empty boot removed the fixed boot
    // order that such hardcoding used to depend on.
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
    // Branches on the `SenderEntry` variant:
    //   - Session: ship as a `ClaudeAddress::Session` frame through
    //     `HubOutbound` (same route as ADR-0013's original design).
    //   - Component: enqueue on the local `MailQueue` via
    //     `SubstrateCtx::send`. Dropped-mailbox discard is handled
    //     there already, so a component that vanished between the
    //     request and the reply silently drops — the same contract
    //     as any other send to a dropped mailbox.
    linker.func_wrap(
        "aether",
        "reply_mail_p32",
        |mut caller: Caller<'_, SubstrateCtx>,
         sender: u32,
         kind: u32,
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
            let Some(entry) = ctx.sender_table.resolve(sender) else {
                return REPLY_UNKNOWN_HANDLE;
            };
            match entry {
                SenderEntry::Session(token) => {
                    let Some(kind_name) = ctx.registry.kind_name(kind) else {
                        return REPLY_KIND_NOT_FOUND;
                    };
                    let origin = ctx.registry.mailbox_name(ctx.sender);
                    ctx.outbound.send(EngineToHub::Mail(EngineMailFrame {
                        address: ClaudeAddress::Session(token),
                        kind_name,
                        payload,
                        origin,
                    }));
                }
                SenderEntry::Component(mbox) => {
                    // Validate the kind id cheaply — the guest might
                    // have passed a bogus one and we'd rather return
                    // a meaningful status than silently enqueue mail
                    // that the receiver can't decode.
                    if ctx.registry.kind_name(kind).is_none() {
                        return REPLY_KIND_NOT_FOUND;
                    }
                    ctx.send(mbox, kind, payload, count);
                }
            }
            REPLY_OK
        },
    )?;

    linker.func_wrap(
        "aether",
        "resolve_mailbox_p32",
        |mut caller: Caller<'_, SubstrateCtx>, name_ptr: u32, name_len: u32| -> u32 {
            let memory = match caller.get_export("memory").and_then(|e| e.into_memory()) {
                Some(m) => m,
                None => return MAILBOX_NOT_FOUND,
            };
            let data = memory.data(&caller);
            let start = name_ptr as usize;
            let end = match start.checked_add(name_len as usize) {
                Some(e) if e <= data.len() => e,
                _ => return MAILBOX_NOT_FOUND,
            };
            let name = match std::str::from_utf8(&data[start..end]) {
                Ok(s) => s,
                Err(_) => return MAILBOX_NOT_FOUND,
            };
            caller
                .data()
                .registry
                .lookup(name)
                .map(|id| id.0)
                .unwrap_or(MAILBOX_NOT_FOUND)
        },
    )?;

    Ok(())
}
