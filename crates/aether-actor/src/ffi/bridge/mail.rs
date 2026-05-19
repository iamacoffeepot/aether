// Wire-encode: `usize → u32` narrowings forward `(ptr, len)` pairs
// to the wasm32 host-fn ABI. wasm32 already has 32-bit addresses;
// `_p32`-suffixed FFI per ADR-0024 documents the convention.
#![allow(clippy::cast_possible_truncation)]

//! [`MailBridge`] — outbound-mail FFI bridge.
//!
//! ZST whose inherent methods forward to the matching `extern "C"`
//! host fns in [`raw`]. `send_mail` pushes a typed payload
//! at a recipient mailbox; `reply_mail` routes to the originator of
//! the mail currently being dispatched; `prev_correlation` reads the
//! correlation id the host minted for the most-recent `send_mail`.
//!
//! Correlation is universal — every send mints a correlation id
//! regardless of whether the caller sync-waits or async-handles the
//! reply. It's a property of the outbound mail, not of any waiting
//! strategy, so it lives here rather than on [`SyncWaitBridge`](super::sync_wait::SyncWaitBridge).

use crate::ffi::raw;

/// ZST FFI bridge for outbound mail. Callers borrow [`MAIL_BRIDGE`] rather
/// than constructing instances — the type carries no per-instance
/// state because the FFI imports are global to the loaded module.
pub struct MailBridge;

/// Process-wide [`MailBridge`] instance. Borrow `&MAIL_BRIDGE` from any ctx impl
/// that needs to send / reply / read the correlation id.
pub static MAIL_BRIDGE: MailBridge = MailBridge;

impl MailBridge {
    /// Push a typed payload at `recipient`. `bytes` is the wire
    /// encoding of the payload (cast for `#[repr(C)]` kinds, postcard
    /// for schema-shaped kinds — `Kind::encode_into_bytes` already
    /// resolves which). `count` is `1` for a single send and N for a
    /// batch (cast-only — postcard has no efficient batched wire
    /// shape, see `MailBridgebox::send_many`).
    ///
    /// Returns `0` on success; `1` on substrate-side recipient
    /// lookup miss. Other non-zero values are reserved for future
    /// host-side failure surfaces.
    ///
    /// Not `#[must_use]`: the public ctx surfaces (`MailSender::send`,
    /// `MailSender::send_to_named`, `OutboundReply::reply`, etc.) are
    /// trait-defined as fire-and-forget and have no return channel for
    /// a lookup-miss status. The substrate warn-drops unknown
    /// recipients on its side, which is the diagnostic path; the guest
    /// can't surface the status anywhere meaningful.
    #[allow(
        clippy::must_use_candidate,
        reason = "fire-and-forget by contract — see doc-comment above; #[must_use] retired in issue 892"
    )]
    pub fn send_mail(&self, recipient: u64, kind: u64, bytes: &[u8], count: u32) -> u32 {
        // SAFETY: forwards to `raw::send_mail`, whose ABI is documented
        // at the import site in `ffi/raw.rs`. The `(ptr, len)` pair is
        // derived from the `&[u8]` slice we just received, which the
        // borrow checker proves is valid for `bytes.len()` bytes for
        // the duration of the call; the host copies before returning.
        unsafe {
            raw::send_mail(
                recipient,
                kind,
                bytes.as_ptr().addr() as u32,
                bytes.len() as u32,
                count,
            )
        }
    }

    /// Reply to the originator of the mail currently being dispatched
    /// (ADR-0013). `sender` is the per-instance handle the dispatcher
    /// threaded onto the ctx at receive time; the substrate routes it
    /// to the right Claude session, sibling component, or remote
    /// engine mailbox.
    ///
    /// Not `#[must_use]`: the trait surfaces (`OutboundReply::reply`,
    /// `MailCtx::reply`) are fire-and-forget by contract — see the
    /// matching rationale on `send_mail`.
    #[allow(
        clippy::must_use_candidate,
        reason = "fire-and-forget by contract — see doc-comment above; #[must_use] retired in issue 892"
    )]
    pub fn reply_mail(&self, sender: u32, kind: u64, bytes: &[u8], count: u32) -> u32 {
        // SAFETY: forwards to `raw::reply_mail`, whose ABI is documented
        // at the import site in `ffi/raw.rs`. The `(ptr, len)` pair is
        // derived from the `&[u8]` slice we just received, which the
        // borrow checker proves is valid for `bytes.len()` bytes for
        // the duration of the call; the host copies before returning.
        unsafe {
            raw::reply_mail(
                sender,
                kind,
                bytes.as_ptr().addr() as u32,
                bytes.len() as u32,
                count,
            )
        }
    }

    /// Correlation id the host minted for this actor's most recent
    /// `send_mail` call (ADR-0042). `0` before any send. Universal —
    /// every send mints a correlation; sync wrappers filter
    /// `wait_reply` against it, async handlers stash it and match on
    /// the inbound's reply correlation.
    #[must_use]
    pub fn prev_correlation(&self) -> u64 {
        // SAFETY: `raw::prev_correlation` takes no arguments and reads
        // a host-side scalar set on the most recent `send_mail`; no
        // ABI invariants to uphold beyond "we are the FFI guest", which
        // the `#[cfg(target_arch = "wasm32")]` import gate enforces
        // (the host-target stub panics rather than returning garbage).
        unsafe { raw::prev_correlation() }
    }

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
    pub fn emit_log_event(&self, level: u8, target: &str, message: &str) {
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
}
