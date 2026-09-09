//! A minimal aether component, end to end. On each tick it emits a fixed
//! world-space triangle to the render capability, and it answers `aether.ping`
//! mail with a matching `aether.pong` back to whoever sent it, the smallest
//! round trip that proves reply-to-sender works.
//!
//! The triangle sits at `z = 0` in world space. With no camera loaded the
//! substrate's identity view-projection passes `(x, y)` straight through to
//! clip space, so the triangle stays put until a camera component starts
//! publishing `aether.view_projection`.
//!
//! `#[actor]` on the `impl WasmActor` block generates the dispatch table and
//! the `aether.kinds.inputs` custom section, so the per-handler rustdoc below
//! travels with the compiled component and shows up in tooling that
//! introspects it.

// `#[handler]` methods take `&mut self` to match the dispatch ABI; a handler
// that ignores `self` keeps the signature anyway.
#![allow(clippy::unused_self)]

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_kinds::{Ping, Pong, Tick};
use aether_lifecycle::LifecycleCapability;
use aether_lifecycle::LifecycleMailboxExt;
use aether_math::Rgb;
use aether_render::{DrawTriangle, RenderCapability, Vertex};

static TRIANGLE: DrawTriangle = DrawTriangle {
    verts: [
        Vertex { x: 0.0, y: 0.5, z: 0.0, color: Rgb::new(1.0, 0.0, 0.0) },
        Vertex { x: -0.5, y: -0.5, z: 0.0, color: Rgb::new(0.0, 1.0, 0.0) },
        Vertex { x: 0.5, y: -0.5, z: 0.0, color: Rgb::new(0.0, 0.0, 1.0) },
    ],
};

/// Per-instance state for the hello component.
pub struct Hello {}

/// Minimal end-to-end smoke component: draws a static triangle every tick and
/// echoes pings back to the sender.
///
/// Capture a frame to see the triangle. A frame that has gone a solid color
/// means the tick path stalled.
#[actor]
impl WasmActor for Hello {
    const NAMESPACE: &'static str = "example.hello";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Hello {})
    }

    //noinspection DuplicatedCode
    fn wire(&mut self, ctx: &mut aether_actor::WireCtx<'_, '_>) {
        ctx.actor::<LifecycleCapability>().subscribe::<Tick>();
    }

    /// Emits the configured triangle to the render capability every tick.
    /// Nothing sends this by hand: the substrate drives it from the frame
    /// lifecycle, and the effect shows up in a captured frame.
    #[handler::single]
    fn on_tick(&mut self, ctx: &mut WasmCtx<'_>, _tick: Tick) {
        ctx.actor::<RenderCapability>().send(&TRIANGLE);
    }

    /// Replies to a ping with a pong carrying the same sequence number, so a
    /// caller with several requests in flight can pair each reply with its
    /// request. A ping with no sender (component-origin or broadcast) is
    /// dropped: there is nothing to reply to.
    #[handler::single]
    fn on_ping(&mut self, _ctx: &mut WasmCtx<'_>, ping: Ping) -> Pong {
        Pong { seq: ping.seq }
    }
}

aether_actor::export!(Hello);
