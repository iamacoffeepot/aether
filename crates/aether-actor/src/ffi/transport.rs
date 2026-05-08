//! [`FfiTransport`] — the FFI guest path's [`MailTransport`] impl.
//!
//! ZST whose `MailTransport` methods forward to the matching
//! `extern "C"` host fns in [`super::raw`]. Any host that exposes the
//! `_p32`-suffixed import surface (today: the wasm runtime in
//! `aether-substrate::actor::wasm`; future: a C host, an OS-process
//! host, ...) can drive an actor through this transport.
//!
//! `aether_substrate::NativeTransport` is the in-process counterpart
//! native capabilities own; both impls share the same SDK and the
//! same trait surface.

use crate::ffi::raw;
use crate::mail::transport::MailTransport;

/// ZST `MailTransport` impl for the FFI guest path. Each method
/// forwards to the matching [`super::raw`]`::*` host-fn import. The
/// `&self` receiver is unused — `FfiTransport` carries no per-instance
/// state because the FFI imports are global to the loaded module — so
/// there's no overhead beyond the host-fn call itself.
pub struct FfiTransport;

/// Process-wide [`FfiTransport`] instance. The type is a ZST, so this
/// `static` occupies zero bytes; its only purpose is giving
/// `&FFI_TRANSPORT` callers (the [`crate::export!`]-emitted ctx
/// constructors) a stable address to borrow without each call site
/// having to write `&FfiTransport` inline.
pub static FFI_TRANSPORT: FfiTransport = FfiTransport;

impl MailTransport for FfiTransport {
    fn send_mail(&self, recipient: u64, kind: u64, bytes: &[u8], count: u32) -> u32 {
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

    fn reply_mail(&self, sender: u32, kind: u64, bytes: &[u8], count: u32) -> u32 {
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

    fn save_state(&self, version: u32, bytes: &[u8]) -> u32 {
        unsafe { raw::save_state(version, bytes.as_ptr().addr() as u32, bytes.len() as u32) }
    }

    fn wait_reply(
        &self,
        expected_kind: u64,
        out: &mut [u8],
        timeout_ms: u32,
        expected_correlation: u64,
    ) -> i32 {
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

    fn prev_correlation(&self) -> u64 {
        unsafe { raw::prev_correlation() }
    }
}
