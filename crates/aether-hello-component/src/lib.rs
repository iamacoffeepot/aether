// First real aether component. On each tick the component emits a
// triangle as a draw-triangle mail to the substrate's render sink.
// Positions are clip-space; the component has no camera/transform
// story yet.
//
// Per ADR-0005, kind ids are resolved by name at component init — the
// substrate assigns them; the guest caches them in statics. Payload
// types are imported from `aether-substrate-mail` so wire layout stays
// in one place.
//
// Mailbox ids remain hardcoded (component=0, render sink=1); symbolic
// mailbox resolution is still future work.
//
// `#[cfg(target_arch = "wasm32")]` guards bodies so the crate still
// compiles for the host target in `cargo test --workspace` (where the
// `aether` imports aren't linkable and `ptr as u32` would truncate a
// 64-bit host pointer).

#[cfg(target_arch = "wasm32")]
use aether_mail::Kind;
#[cfg(target_arch = "wasm32")]
use aether_substrate_mail::{DrawTriangle, Tick, Vertex};

#[cfg(target_arch = "wasm32")]
const RENDER_SINK: u32 = 1;

// `u32::MAX` sentinel matches the host's KIND_NOT_FOUND — if `init`
// didn't run or a name is missing, the dispatch below simply no-ops.
#[cfg(target_arch = "wasm32")]
static mut KIND_TICK: u32 = u32::MAX;
#[cfg(target_arch = "wasm32")]
static mut KIND_DRAW_TRIANGLE: u32 = u32::MAX;

// A fixed clip-space triangle with per-vertex color. Typed as
// DrawTriangle so the vertex layout matches the substrate's decode
// type; bytemuck handles the &T-to-bytes cast at send time.
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
    fn resolve_kind(name_ptr: u32, name_len: u32) -> u32;
}

#[cfg(target_arch = "wasm32")]
unsafe fn resolve(name: &str) -> u32 {
    unsafe { resolve_kind(name.as_ptr() as u32, name.len() as u32) }
}

/// Runs once before the first `receive`. Resolves each kind name this
/// component cares about and caches the assigned id. Return value is
/// currently informational.
///
/// # Safety
/// Called by the substrate exactly once, before any `receive` call, on
/// a thread that owns this component's linear memory. The body mutates
/// the `KIND_*` statics — safe because there is no concurrent reader
/// until `init` returns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn init() -> u32 {
    #[cfg(target_arch = "wasm32")]
    unsafe {
        KIND_TICK = resolve(Tick::NAME);
        KIND_DRAW_TRIANGLE = resolve(DrawTriangle::NAME);
    }
    0
}

/// # Safety
/// Host contract: `ptr` points to a `count`-item payload for `kind`
/// within guest linear memory. For this milestone we do not read the
/// inbound payload; the tick's empty body is unused.
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
