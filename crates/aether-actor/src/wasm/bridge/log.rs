// Wire-encode: `usize → u32` narrowings forward `(ptr, len)` pairs
// to the wasm32 host-fn ABI. wasm32 already has 32-bit addresses;
// `_p32`-suffixed FFI per ADR-0024 documents the convention.
#![allow(clippy::cast_possible_truncation)]

//! Log FFI bridge — the host-fn-facing layer for log events.
//!
//! ADR-0081 §7: re-emit one `tracing::*` event on the host side.
//! Split from `bridge::mail` (which owns the outbound-mail op family)
//! to keep one op family per module.

#[cfg(target_family = "wasm")]
use crate::wasm::raw;

/// ADR-0081 §7: re-emit one `tracing::*` event on the host side.
/// Called by the wasm subscriber per event so the host's
/// `ActorAwareLayer` lands the entry in the trampoline's
/// `ActorLogRing`. `level` is the `0 = trace .. 4 = error`
/// mapping the rest of `aether.log.*` uses; `target` and
/// `message` are the pre-rendered tracing field text.
///
/// Fire-and-forget: no return code. The host copies before
/// returning; the guest's borrows are released as soon as the
/// FFI call completes.
///
/// Only callable from wasm32 — installed as the [`crate::log::LogSink`]
/// by the guest runtime (`export!`) and invoked from
/// [`crate::log::ForwardingSubscriber::event`].
#[cfg(target_family = "wasm")]
pub fn emit_log_event(level: u8, target: &str, message: &str) {
    let target_bytes = target.as_bytes();
    let message_bytes = message.as_bytes();
    // SAFETY: forwards to `raw::log_event`, whose ABI is documented
    // at the import site in `ffi/raw.rs`. The `(ptr, len)` pairs are
    // derived from `&str` references valid for `len` bytes for the
    // call's duration; the host copies before returning.
    unsafe {
        raw::log_event(
            u32::from(level),
            target_bytes.as_ptr().addr() as u32,
            target_bytes.len() as u32,
            message_bytes.as_ptr().addr() as u32,
            message_bytes.len() as u32,
        );
    }
}
