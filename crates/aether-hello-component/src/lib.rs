// First real aether component. Each frame the substrate ticks this
// component, and it replies by sending a heartbeat mail to a sink
// mailbox. Proves bidirectional host↔component flow for milestone 1.
//
// Mailbox ids for milestone 1 are hardcoded. The substrate assigns
// them deterministically at boot (component=0, heartbeat sink=1);
// symbolic name resolution at component init is future work per
// issue #18's open sub-questions.
//
// `#[cfg(target_arch = "wasm32")]` guards the bodies so the crate
// still compiles for the host target in `cargo test --workspace`
// (where the `send_mail` import is not resolvable at link time).

#[cfg(target_arch = "wasm32")]
const HEARTBEAT_SINK: u32 = 1;
#[cfg(target_arch = "wasm32")]
const KIND_HEARTBEAT: u32 = 42;

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "aether")]
unsafe extern "C" {
    fn send_mail(recipient: u32, kind: u32, ptr: u32, len: u32, count: u32) -> u32;
}

/// # Safety
/// Host contract: `ptr` points to `count`-item payload for `kind` within
/// linear memory. Milestone 1's only inbound kind is the tick (the host
/// pushes a single-item payload we do not read), so no pointer use here.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn receive(_kind: u32, _ptr: u32, count: u32) -> u32 {
    #[cfg(target_arch = "wasm32")]
    unsafe {
        send_mail(HEARTBEAT_SINK, KIND_HEARTBEAT, 0, 0, count);
    }
    #[cfg(not(target_arch = "wasm32"))]
    let _ = count;
    0
}
