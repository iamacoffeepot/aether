//! Synthetic UI widget for the issue-1793 widget-actor cost spike.
//!
//! A scriptable widget in the UI design is a wasm component: it
//! subscribes to the frame lifecycle and re-emits its draw each frame,
//! paying a wasm boundary crossing plus a mail encode/send per tick. This
//! fixture stands in for one such widget so the `widget_actor_cost`
//! measurement can read its per-frame execution cost from the per-handler
//! EWMA (ADR-0036) and answer how many actor-backed widgets are affordable
//! at 60fps.
//!
//! Its [`UiWidgetConfig`] selects the per-frame profile:
//!
//! - `redraw_each_tick = true` (naive) — rebuild and re-emit the full
//!   `DrawSolidQuads` batch across the boundary every tick. This is the
//!   cost host-cached draw replay exists to remove.
//! - `redraw_each_tick = false` (cached) — early-return on tick, emitting
//!   nothing. This is the stable-frame floor: the irreducible boundary
//!   crossing + dispatch the runtime pays to reach a still-tick-subscribed
//!   guest. A true host-cached-replay cap would not dispatch the guest at
//!   all on an unchanged frame (it replays the retained batch host-side),
//!   so this floor is the upper bound on what cached replay leaves behind.
//!
//! Not a demo — its only job is to expose a realistic per-frame widget
//! cost to the measurement.

use aether_actor::{BootError, FfiActor, FfiCtx, FfiInitCtx, actor};
use aether_capabilities::lifecycle::LifecycleMailboxExt;
use aether_capabilities::{LifecycleCapability, RenderCapability};
use aether_kinds::{DrawSolidQuads, QuadSpace, SolidQuad, Tick};
use aether_test_fixtures::UiWidgetConfig;

pub struct UiWidget {
    config: UiWidgetConfig,
}

#[actor]
impl FfiActor for UiWidget {
    type Config = UiWidgetConfig;
    const NAMESPACE: &'static str = "test_fixtures_ui_widget";

    fn init(config: UiWidgetConfig, _ctx: &mut FfiInitCtx<'_>) -> Result<Self, BootError> {
        Ok(UiWidget { config })
    }

    /// `Tick` is a frame-lifecycle stage, so it subscribes on
    /// `aether.lifecycle` (ADR-0082) — the same path a real per-frame
    /// widget uses to be driven each frame.
    fn wire(&mut self, ctx: &mut FfiCtx<'_>) {
        ctx.actor::<LifecycleCapability>().subscribe::<Tick>();
    }

    /// One widget's per-frame work. In the cached profile the draw is
    /// unchanged, so the widget re-emits nothing and returns immediately —
    /// the measured cost is then the bare boundary crossing + dispatch. In
    /// the naive profile it rebuilds the `DrawSolidQuads` batch and sends
    /// it across the boundary every frame — the measured cost adds the
    /// batch build + mail encode + send that host-cached replay removes.
    #[handler]
    fn on_tick(&mut self, ctx: &mut FfiCtx<'_>, _: Tick) {
        if !self.config.redraw_each_tick {
            return;
        }
        let mut quads = Vec::new();
        for _ in 0..self.config.quad_count {
            quads.push(SolidQuad {
                x: 0.0,
                y: 0.0,
                width: 4.0,
                height: 4.0,
                color: [0.2, 0.4, 0.8, 1.0],
            });
        }
        ctx.actor::<RenderCapability>().send(&DrawSolidQuads {
            space: QuadSpace::Screen,
            quads,
        });
    }
}

aether_actor::export!(UiWidget);
