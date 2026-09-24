//! Render probe fixture (issue #6507).
//!
//! `PaintProbe` mails render only while its `SetRender` state is visible,
//! so it is its own actor that declares render (ADR-0232 §6, no optional
//! peers) and loads only where render is composed. `capture_frame`
//! scenarios flip its state with `aether.test_fixture.set_render` and
//! observe the colored triangle in the captured PNG.

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_kinds::Tick;
use aether_lifecycle::LifecycleCapability;
use aether_math::Rgb;
use aether_render::{DrawTriangle, RenderCapability, Vertex};
use aether_test_fixtures_kinds::SetRender;

pub struct PaintProbe {
    render: SetRender,
}

#[actor(depends(LifecycleCapability), depends(RenderCapability))]
impl WasmActor for PaintProbe {
    const NAMESPACE: &'static str = "test.paint_probe";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(PaintProbe { render: SetRender::default() })
    }

    /// Subscribe `Tick` on `aether.lifecycle` (ADR-0082) so the tick
    /// fanout drives `on_tick`.
    fn wire(&mut self, ctx: &mut aether_actor::WireCtx<'_, '_, Self>) {
        ctx.subscribe::<LifecycleCapability, Tick>();
    }

    /// When the stored render state is `visible`, emits a colored
    /// `DrawTriangle` covering most of the frame so `capture_frame`
    /// scenarios can see the pre-mail effect in the PNG.
    ///
    /// # Agent
    /// Not sent manually; the substrate's tick fanout fires it once per
    /// advance for every lifecycle-subscribed mailbox.
    #[handler::single]
    fn on_tick(&mut self, ctx: &mut WasmCtx<'_, Self>, _: Tick) {
        if self.render.visible != 0 {
            let r = f32::from(self.render.r) / 255.0;
            let g = f32::from(self.render.g) / 255.0;
            let b = f32::from(self.render.b) / 255.0;
            let v = |x: f32, y: f32| Vertex { x, y, z: 0.5, color: Rgb::new(r, g, b) };
            ctx.send::<RenderCapability>(&DrawTriangle { verts: [v(-0.9, -0.9), v(0.9, -0.9), v(0.0, 0.9)] });
        }
    }

    /// Updates the stored render state. Subsequent ticks paint the
    /// new color (or stop painting when `visible == 0`).
    ///
    /// # Agent
    /// Send via `send_mail` with `kind_name = "aether.test_fixture.set_render"`
    /// and params `{ r, g, b, visible }`. Used by `capture_frame`
    /// scenarios to flip the fixture's render output between frames.
    #[handler::single]
    fn on_set_render(&mut self, _ctx: &mut WasmCtx<'_>, mail: SetRender) {
        self.render = mail;
    }
}
