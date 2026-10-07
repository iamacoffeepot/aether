//! The engine's actor clock (`now_nanos`), the transport under
//! `WasmCtx::now` and `WasmInitCtx::now`.

use crate::wasm::raw;

/// The engine's actor clock, in nanoseconds since its anchor. The host reads
/// the same clock a native actor's `NativeCtx::now` does.
#[must_use]
pub fn now_nanos() -> u64 {
    // SAFETY: `raw::now_nanos` takes no arguments and reads a host-side
    // scalar; no ABI invariants to uphold beyond "we are the FFI guest",
    // which the `#[cfg(target_family = "wasm")]` import gate enforces (the
    // host-target stub panics rather than returning garbage).
    unsafe { raw::now_nanos() }
}
