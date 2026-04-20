// Raw FFI boundary with the substrate. This is the only place in a
// guest component tree that should write `extern "C"` decls or host-stub
// panics; everything else goes through the typed wrappers in `lib.rs`.
//
// On wasm32 the fns are imports from the `aether` module the substrate
// exposes (see `aether-substrate/src/host_fns.rs`). On any other target
// they're stubs that panic if called, which keeps the crate (and every
// component that depends on it) compilable for `cargo test --workspace`
// on the host — components can still be unit-tested for pure logic
// there, they just can't cross the FFI.
//
// ADR-0024 Phase 1: the wasm-visible import names carry a `_p32`
// suffix in anticipation of a future `_p64` sibling for wasm64
// components. The Rust-side identifiers stay un-suffixed (`send_mail`,
// not `send_mail_p32`) so callers in `lib.rs` don't have to thread the
// suffix through every callsite — `#[link_name]` does the remap.

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "aether")]
unsafe extern "C" {
    #[link_name = "send_mail_p32"]
    pub fn send_mail(recipient: u64, kind: u64, ptr: u32, len: u32, count: u32) -> u32;
    #[link_name = "reply_mail_p32"]
    pub fn reply_mail(sender: u32, kind: u64, ptr: u32, len: u32, count: u32) -> u32;
    #[link_name = "save_state_p32"]
    pub fn save_state(version: u32, ptr: u32, len: u32) -> u32;
}

/// # Safety
/// Host-target stub for the wasm `aether::send_mail` import. Always
/// panics — callers on non-wasm targets are misusing the SDK.
#[cfg(not(target_arch = "wasm32"))]
pub unsafe fn send_mail(_recipient: u64, _kind: u64, _ptr: u32, _len: u32, _count: u32) -> u32 {
    panic!("aether-component: send_mail called on non-wasm target");
}

/// # Safety
/// Host-target stub for the wasm `aether::reply_mail` import. Always
/// panics — callers on non-wasm targets are misusing the SDK.
#[cfg(not(target_arch = "wasm32"))]
pub unsafe fn reply_mail(_sender: u32, _kind: u64, _ptr: u32, _len: u32, _count: u32) -> u32 {
    panic!("aether-component: reply_mail called on non-wasm target");
}

/// # Safety
/// Host-target stub for the wasm `aether::save_state` import. Always
/// panics — callers on non-wasm targets are misusing the SDK.
#[cfg(not(target_arch = "wasm32"))]
pub unsafe fn save_state(_version: u32, _ptr: u32, _len: u32) -> u32 {
    panic!("aether-component: save_state called on non-wasm target");
}
