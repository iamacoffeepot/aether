// Wire-encode: `usize → u32` narrowings forward `(ptr, len)` pairs
// to the wasm32 host-fn ABI (`_p32` convention, ADR-0024).
#![allow(clippy::cast_possible_truncation)]

//! [`SyncWaitBridge`] — blocking-reply FFI bridge.
//!
//! ZST whose only inherent method is `wait_reply`, the ADR-0042 sync
//! round-trip primitive. The correlation id this method filters on
//! is read off [`super::mail::MailBridge::prev_correlation`] — every send
//! mints a correlation regardless of whether the caller sync-waits,
//! so correlation lives on the mail bridge, not here.
//!
//! Per-stage [`crate::actor::ctx::SyncWaitBridgeer`] impls route through
//! [`SYNC_WAIT_BRIDGE`] for FFI guests; native ctxs route through
//! `NativeBinding::wait_reply` (ADR-0074 §Decision 5 cross-class
//! guard lives in the native binding's inherent body).

use crate::ffi::raw;

/// ZST FFI bridge for `wait_reply`. Borrow [`SYNC_WAIT_BRIDGE`] from a
/// [`crate::actor::ctx::SyncWaitBridgeer`] impl.
pub struct SyncWaitBridge;

/// Process-wide [`SyncWaitBridge`] instance.
pub static SYNC_WAIT_BRIDGE: SyncWaitBridge = SyncWaitBridge;

impl SyncWaitBridge {
    /// Block the actor's thread until a mail of `expected_kind` (and,
    /// when `expected_correlation != 0`, also that correlation id)
    /// arrives, then copy up to `out.len()` bytes of its payload into
    /// `out` (ADR-0042). `timeout_ms` is clamped substrate-side to
    /// 30s.
    ///
    /// Returns `>= 0` = bytes written, `-1` = timeout, `-2` = payload
    /// larger than `out` (mail re-parked for retry), `-3` = the host
    /// tore the actor down mid-wait. Any other negative is reserved
    /// for future sentinels and surfaces through `WaitError::decode`
    /// in the SDK wrapper so a reader sees the unknown rc by name.
    pub fn wait_reply(
        &self,
        expected_kind: u64,
        out: &mut [u8],
        timeout_ms: u32,
        expected_correlation: u64,
    ) -> i32 {
        // SAFETY: forwards to `raw::wait_reply`, whose ABI is documented
        // at the import site in `ffi/raw.rs`. The `(out_ptr, out_cap)`
        // pair is derived from the `&mut [u8]` we just received, which
        // the borrow checker proves is valid for `out.len()` bytes for
        // the duration of the call; the host writes at most `out_cap`
        // bytes through the pointer (`-2` re-parks rather than spilling).
        unsafe {
            raw::wait_reply(
                expected_kind,
                out.as_mut_ptr().addr() as u32,
                out.len() as u32,
                timeout_ms,
                expected_correlation,
            )
        }
    }
}
