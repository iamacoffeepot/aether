// First real aether component, now built on the ADR-0012 SDK.
// On each tick it emits a triangle as draw-triangle mail to the
// substrate's render sink. Positions are clip-space; the component
// has no camera/transform story yet.
//
// What the SDK (aether-component) replaces compared to the pre-SDK
// shape:
//   - raw `extern "C"` block → `aether_component::raw` owns it.
//   - three `static mut u32` sentinel slots → two typed `Option`s
//     holding `KindId<Tick>` and `Sink<DrawTriangle>`.
//   - `ptr as u32`, `size_of::<T>() as u32`, `count` math at the
//     send site → `Sink::send(&payload)`.
//   - `#[cfg(target_arch = "wasm32")]` guards per item → none;
//     the SDK's raw module has host-target stubs so this whole file
//     compiles for `cargo test --workspace` on any target.
//
// The `#[unsafe(no_mangle)] extern "C" fn init/receive` exports and
// the `static mut` backing store remain hand-written — ADR-0014's
// `Component` trait + `export!` macro will absorb those next.

use aether_component::{KindId, Sink, resolve, resolve_sink};
use aether_substrate_mail::{DrawTriangle, Tick, Vertex};

// Resolved at init; read on every receive. `Option` carries the
// "not yet resolved" state without a sentinel u32 — the SDK's
// `resolve` panics on failure, so a `Some` here is always a valid
// id.
static mut TICK: Option<KindId<Tick>> = None;
static mut RENDER: Option<Sink<DrawTriangle>> = None;

// A fixed clip-space triangle with per-vertex color. Typed as
// DrawTriangle so `Sink::send` can byte-cast it without the caller
// doing pointer math.
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

/// Runs once before the first `receive`. Resolves the kinds and the
/// render sink this component cares about and caches them in typed
/// statics.
///
/// # Safety
/// Called by the substrate exactly once, before any `receive` call,
/// on a thread that owns this component's linear memory. The body
/// writes to the resolution statics — safe because there is no
/// concurrent reader until `init` returns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn init() -> u32 {
    unsafe {
        TICK = Some(resolve::<Tick>());
        RENDER = Some(resolve_sink::<DrawTriangle>("render"));
    }
    0
}

/// # Safety
/// Host contract: `ptr` points to a `count`-item payload for `kind`
/// within guest linear memory. For this milestone we do not read the
/// inbound payload; the tick's empty body is unused.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn receive(kind: u32, _ptr: u32, _count: u32) -> u32 {
    unsafe {
        if let (Some(tick), Some(render)) = (TICK, RENDER)
            && tick.matches(kind)
        {
            render.send(&TRIANGLE);
        }
    }
    0
}
