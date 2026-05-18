//! [`SyncWaiter`] — synchronous reply-receive surface (ADR-0042).
//!
//! Per-stage capability trait under issue 665: per-handler ctxs that
//! support blocking on a specific reply impl this. `wait_reply` is the
//! whole surface — the correlation id this method filters on lives on
//! [`crate::actor::ctx::MailSender::prev_correlation`] (every send
//! mints a correlation; sync wait is one of the strategies for
//! consuming the reply).
//!
//! FFI ctxs route to [`crate::ffi::bridge::SyncWaitBridge::wait_reply`].
//! Native ctxs route to `NativeBinding::wait_reply` (which carries
//! the ADR-0074 §Decision 5 cross-class guard inline). Either side
//! ships the same return-code contract: `>= 0` = bytes written,
//! `-1` = timeout, `-2` = payload exceeds buffer, `-3` = host tore the
//! actor down mid-wait. The pure rc → `Result<K, E>` mapping lives in
//! [`crate::mail::sync::decode_wait_reply`] for callers that want to
//! reuse the sentinel decoding without going through the trait.

use alloc::vec;
use alloc::vec::Vec;

use aether_data::Kind;

use crate::mail::sync::{WaitError, decode_wait_reply};

/// Synchronous reply-receive surface every per-handler ctx that
/// participates in ADR-0042 sync round trips exposes. Today FFI ctxs
/// (wasm guests) and native ctxs both impl this; the implementations
/// route to their per-target `wait_reply` primitive.
pub trait SyncWaiter {
    /// Allocate a `capacity`-sized scratch buffer, block until a mail
    /// of kind `K` (and, when `expected_correlation != 0`, also
    /// matching that correlation id) arrives, then postcard-decode
    /// the written bytes into a `K`.
    ///
    /// `expected_correlation` is typically the value returned by
    /// `MailSender::prev_correlation` immediately after the request
    /// send — that is the ADR-0042 contract for filtering "the reply
    /// for the request I just sent" instead of "any reply of this
    /// kind."
    ///
    /// `timeout_ms` is clamped substrate-side to 30s. The four
    /// failure modes (timeout / buffer-too-small / cancelled /
    /// decode) come back through [`WaitError`].
    fn wait_reply<K, E>(
        &self,
        timeout_ms: u32,
        capacity: usize,
        expected_correlation: u64,
    ) -> Result<K, E>
    where
        K: Kind + serde::de::DeserializeOwned,
        E: WaitError;
}

/// Helper for impls that want to reuse the alloc + raw-rc + decode
/// path without inlining it. Takes the four-arg blocking primitive as
/// a callback so each transport plugs in its own (FFI calls
/// `SYNC_WAIT.wait_reply`; native calls `NativeBinding::wait_reply`).
pub fn wait_reply_via<K, E>(
    raw_wait: impl FnOnce(u64, &mut [u8], u32, u64) -> i32,
    timeout_ms: u32,
    capacity: usize,
    expected_correlation: u64,
) -> Result<K, E>
where
    K: Kind + serde::de::DeserializeOwned,
    E: WaitError,
{
    let mut buf: Vec<u8> = vec![0u8; capacity];
    let rc = raw_wait(K::ID.0, &mut buf, timeout_ms, expected_correlation);
    decode_wait_reply::<K, E>(rc, &buf)
}

// `WaitError` and `decode_wait_reply` are imported above; re-exported
// at this module path so impls don't need a separate
// `use crate::mail::sync::WaitError` line.
#[doc(no_inline)]
pub use crate::mail::sync::WaitError as Error;
