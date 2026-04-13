// First real aether component. Each frame the substrate ticks this
// component over mail, and it replies with a heartbeat to the sink.
// Milestone 2 adds input kinds: the key-press payload (4-byte LE u32
// key code) is forwarded to the sink so the payload path travels end
// to end; other kinds produce an empty-bodied heartbeat.
//
// Mailbox ids for milestone 1/2 are hardcoded. The substrate assigns
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
const KIND_KEY: u32 = 10;

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "aether")]
unsafe extern "C" {
    fn send_mail(recipient: u32, kind: u32, ptr: u32, len: u32, count: u32) -> u32;
}

/// # Safety
/// Host contract: `ptr` points to a `count`-item payload for `kind` within
/// guest linear memory. For `KIND_KEY` the payload is 4 bytes (LE u32 key
/// code); for other kinds the body is unused here.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn receive(kind: u32, ptr: u32, count: u32) -> u32 {
    #[cfg(target_arch = "wasm32")]
    unsafe {
        let (forward_ptr, forward_len) = if kind == KIND_KEY { (ptr, 4) } else { (0, 0) };
        send_mail(
            HEARTBEAT_SINK,
            KIND_HEARTBEAT,
            forward_ptr,
            forward_len,
            count,
        );
    }
    #[cfg(not(target_arch = "wasm32"))]
    let _ = (kind, ptr, count);
    0
}
