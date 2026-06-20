//! First real aether component. On each tick it emits a fixed
//! world-space triangle to the substrate's render sink. It also
//! answers ADR-0013 `aether.ping` mail with a matching `aether.pong`
//! back to the originating Claude session — a minimal round-trip
//! smoke test proving reply-to-sender works end-to-end over the hub.
//!
//! The triangle sits at `z = 0` in world space. With no camera loaded
//! the substrate's identity uniform passes `(x, y)` straight through
//! to clip space, so visually this behaves exactly like the old
//! clip-space-only version until a camera component starts driving
//! `aether.camera`.
//!
//! ADR-0033 shape: `#[actor]` on the `impl Component` block emits
//! both the dispatcher and the `aether.kinds.inputs` section entries.
//! Per-handler rustdoc (with an optional `# Agent` section) feeds
//! MCP via the same section so the harness sees typed capabilities
//! plus author-written intent for each inbox.

// `#[handler]` methods take `&mut self` to match the dispatch ABI
// (ADR-0033 / ADR-0038); a stateless handler that ignores `self` is
// fine but must keep the signature.
#![allow(clippy::unused_self)]

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_capabilities::lifecycle::LifecycleMailboxExt;
use aether_capabilities::{LifecycleCapability, RenderCapability};
use aether_kinds::{DrawTriangle, Ping, Pong, Tick, Vertex};

static TRIANGLE: DrawTriangle = DrawTriangle {
    verts: [
        Vertex {
            x: 0.0,
            y: 0.5,
            z: 0.0,
            r: 1.0,
            g: 0.0,
            b: 0.0,
        },
        Vertex {
            x: -0.5,
            y: -0.5,
            z: 0.0,
            r: 0.0,
            g: 1.0,
            b: 0.0,
        },
        Vertex {
            x: 0.5,
            y: -0.5,
            z: 0.0,
            r: 0.0,
            g: 0.0,
            b: 1.0,
        },
    ],
};

/// Per-instance state for the hello component.
pub struct Hello {}

/// Minimal end-to-end smoke component: draws a static triangle every
/// tick and echoes pings back to the sender.
///
/// # Agent
/// Watch the render output (via `capture_frame`) to see the triangle —
/// if the frame goes solid color the tick path stalled. Send
/// `aether.ping` with an incrementing `seq` to exercise reply-to-
/// sender; the matching `aether.pong` lands back at your session.
#[actor]
impl WasmActor for Hello {
    const NAMESPACE: &'static str = "hello";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Hello {})
    }

    //noinspection DuplicatedCode
    fn wire(&mut self, ctx: &mut WasmCtx<'_>) {
        ctx.actor::<LifecycleCapability>().subscribe::<Tick>();
    }

    /// Emits the configured triangle to the render sink every tick.
    ///
    /// # Agent
    /// Not useful to send manually — the substrate drives this from
    /// its own tick loop. The effect is visible in `capture_frame`
    /// output.
    #[handler]
    fn on_tick(&mut self, ctx: &mut WasmCtx<'_>, _tick: Tick) {
        ctx.actor::<RenderCapability>().send(&TRIANGLE);
    }

    /// Replies to a ping with a pong carrying the same sequence
    /// number. Silently drops pings that have no sender (component-
    /// origin or broadcast) since there's nothing to reply to.
    ///
    /// # Agent
    /// Send `{ seq: N }` and expect a matching pong at your session.
    /// The seq echo lets you pair requests and replies when multiple
    /// are in flight.
    #[handler]
    fn on_ping(&mut self, _ctx: &mut WasmCtx<'_>, ping: Ping) -> Pong {
        Pong { seq: ping.seq }
    }
}

aether_actor::export!(Hello);
