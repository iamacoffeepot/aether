// First real aether component. Milestone 3b: on each tick the
// component emits a triangle as KIND_DRAW_TRIANGLE mail to the
// substrate's render sink. Positions are already clip-space; the
// component has no camera/transform story yet.
//
// Mailbox ids remain hardcoded (component=0, render sink=1); symbolic
// name resolution at component init is future work per issue #18.
//
// `#[cfg(target_arch = "wasm32")]` guards the bodies so the crate
// still compiles for the host target in `cargo test --workspace`
// (where the `send_mail` import is not resolvable at link time, and
// where `ptr as u32` would truncate a 64-bit host pointer).

#[cfg(target_arch = "wasm32")]
const RENDER_SINK: u32 = 1;
#[cfg(target_arch = "wasm32")]
const KIND_TICK: u32 = 1;
#[cfg(target_arch = "wasm32")]
const KIND_DRAW_TRIANGLE: u32 = 20;

// A fixed clip-space triangle with per-vertex color. Laid out to match
// the substrate's VertexBufferLayout: (pos: vec2<f32>, color: vec3<f32>),
// 20 bytes per vertex, 60 bytes total.
#[cfg(target_arch = "wasm32")]
static TRIANGLE: [f32; 15] = [
    // pos x, pos y,   r,   g,   b
    0.0, 0.5, 1.0, 0.0, 0.0, //
    -0.5, -0.5, 0.0, 1.0, 0.0, //
    0.5, -0.5, 0.0, 0.0, 1.0, //
];

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "aether")]
unsafe extern "C" {
    fn send_mail(recipient: u32, kind: u32, ptr: u32, len: u32, count: u32) -> u32;
}

/// # Safety
/// Host contract: `ptr` points to a `count`-item payload for `kind` within
/// guest linear memory. For milestone 3b we do not read the inbound
/// payload; the tick's single-item body is unused.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn receive(kind: u32, _ptr: u32, _count: u32) -> u32 {
    #[cfg(target_arch = "wasm32")]
    unsafe {
        if kind == KIND_TICK {
            let ptr = TRIANGLE.as_ptr() as u32;
            let len = size_of_val(&TRIANGLE) as u32;
            // count = number of triangles in this payload (one).
            send_mail(RENDER_SINK, KIND_DRAW_TRIANGLE, ptr, len, 1);
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    let _ = kind;
    0
}
