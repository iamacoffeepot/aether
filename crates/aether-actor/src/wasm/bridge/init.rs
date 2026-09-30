//! Guest init-failure staging: the guest half of the `init_failed_p32`
//! host fn (ADR-0096).
//!
//! Its own op family, distinct from `persist` (migration bundles) and
//! `log` (event forwarding): the `export!` init shims stage one message
//! right before returning non-zero from the guest's `init` export, and no
//! other caller has a reason to reach it.

use super::abi32;
use crate::wasm::raw;

/// Stage `message` for the substrate to surface in `LoadResult::Err`.
pub fn init_failed(message: &str) {
    let bytes = message.as_bytes();
    // SAFETY: `init_failed` copies `len` bytes from `ptr` into the
    // substrate synchronously; the borrowed slice outlives the call.
    unsafe {
        raw::init_failed(abi32(bytes.as_ptr().addr()), abi32(bytes.len()));
    }
}
