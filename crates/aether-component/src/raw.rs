// Raw FFI boundary with the substrate. This is the only place in a
// guest component tree that should write `extern "C"` decls or host-stub
// panics; everything else goes through the typed wrappers in `lib.rs`.
//
// On wasm32 the three fns are imports from the `aether` module the
// substrate exposes (see `aether-substrate/src/host_fns.rs`). On any
// other target they're stubs that panic if called, which keeps the
// crate (and every component that depends on it) compilable for
// `cargo test --workspace` on the host — components can still be
// unit-tested for pure logic there, they just can't cross the FFI.

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "aether")]
unsafe extern "C" {
    pub fn send_mail(recipient: u32, kind: u32, ptr: u32, len: u32, count: u32) -> u32;
    pub fn resolve_kind(name_ptr: u32, name_len: u32) -> u32;
    pub fn resolve_mailbox(name_ptr: u32, name_len: u32) -> u32;
}

/// # Safety
/// Host-target stub for the wasm `aether::send_mail` import. Always
/// panics — callers on non-wasm targets are misusing the SDK.
#[cfg(not(target_arch = "wasm32"))]
pub unsafe fn send_mail(_recipient: u32, _kind: u32, _ptr: u32, _len: u32, _count: u32) -> u32 {
    panic!("aether-component: send_mail called on non-wasm target");
}

/// # Safety
/// Host-target stub for the wasm `aether::resolve_kind` import. Always
/// panics — callers on non-wasm targets are misusing the SDK.
#[cfg(not(target_arch = "wasm32"))]
pub unsafe fn resolve_kind(_name_ptr: u32, _name_len: u32) -> u32 {
    panic!("aether-component: resolve_kind called on non-wasm target");
}

/// # Safety
/// Host-target stub for the wasm `aether::resolve_mailbox` import.
/// Always panics — callers on non-wasm targets are misusing the SDK.
#[cfg(not(target_arch = "wasm32"))]
pub unsafe fn resolve_mailbox(_name_ptr: u32, _name_len: u32) -> u32 {
    panic!("aether-component: resolve_mailbox called on non-wasm target");
}
