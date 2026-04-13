// First real aether component. On each tick the component emits a
// triangle as KIND_DRAW_TRIANGLE mail to the substrate's render sink.
// Positions are clip-space; the component has no camera/transform story
// yet.
//
// Kind ids and payload types are imported from `aether-substrate-mail`
// per ADR-0005 — the substrate's mail vocabulary is a crate components
// depend on, not a duplicated wire constant.
//
// Mailbox ids remain hardcoded (component=0, render sink=1); symbolic
// name resolution at component init is future work per ADR-0005's
// kind-registry-at-init follow-up.
//
// `#[cfg(target_arch = "wasm32")]` guards the bodies so the crate still
// compiles for the host target in `cargo test --workspace` (where the
// `send_mail` import is not resolvable at link time, and where
// `ptr as u32` would truncate a 64-bit host pointer).

#[cfg(target_arch = "wasm32")]
use aether_substrate_mail::{DrawTriangle, KIND_DRAW_TRIANGLE, KIND_TICK, Vertex};

#[cfg(target_arch = "wasm32")]
const RENDER_SINK: u32 = 1;

// A fixed clip-space triangle with per-vertex color. Typed as
// DrawTriangle so the vertex layout is the same type the substrate
// decodes against; bytemuck handles the &T-to-bytes cast at send time.
#[cfg(target_arch = "wasm32")]
static TRIANGLE: DrawTriangle = DrawTriangle {
    verts: [
        Vertex {
            x: 0.0,
            y: 0.5,
            r: 1.0,
            g: 0.0,
            b: 0.0,
        },
        Vertex {
            x: -0.5,
            y: -0.5,
            r: 0.0,
            g: 1.0,
            b: 0.0,
        },
        Vertex {
            x: 0.5,
            y: -0.5,
            r: 0.0,
            g: 0.0,
            b: 1.0,
        },
    ],
};

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "aether")]
unsafe extern "C" {
    fn send_mail(recipient: u32, kind: u32, ptr: u32, len: u32, count: u32) -> u32;
}

/// # Safety
/// Host contract: `ptr` points to a `count`-item payload for `kind` within
/// guest linear memory. For this milestone we do not read the inbound
/// payload; the tick's empty body is unused.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn receive(kind: u32, _ptr: u32, _count: u32) -> u32 {
    #[cfg(target_arch = "wasm32")]
    unsafe {
        if kind == KIND_TICK {
            let ptr = &TRIANGLE as *const DrawTriangle as u32;
            let len = core::mem::size_of::<DrawTriangle>() as u32;
            // count = number of triangles in this payload (one).
            send_mail(RENDER_SINK, KIND_DRAW_TRIANGLE, ptr, len, 1);
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    let _ = kind;
    0
}
