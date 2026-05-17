//! [`MailBridge`] — outbound-mail FFI bridge.
//!
//! ZST whose inherent methods forward to the matching `extern "C"`
//! host fns in [`crate::ffi::raw`]. `send_mail` pushes a typed payload
//! at a recipient mailbox; `reply_mail` routes to the originator of
//! the mail currently being dispatched; `prev_correlation` reads the
//! correlation id the host minted for the most-recent `send_mail`.
//!
//! Correlation is universal — every send mints a correlation id
//! regardless of whether the caller sync-waits or async-handles the
//! reply. It's a property of the outbound mail, not of any waiting
//! strategy, so it lives here rather than on [`super::sync_wait::SyncWaitBridge`].

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
    #[must_use]
    pub fn send_mail(&self, recipient: u64, kind: u64, bytes: &[u8], count: u32) -> u32 {
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
    #[must_use]
    pub fn reply_mail(&self, sender: u32, kind: u64, bytes: &[u8], count: u32) -> u32 {
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
        unsafe { raw::prev_correlation() }
    }
}
