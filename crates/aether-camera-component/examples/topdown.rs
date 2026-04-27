//! Orthographic top-down camera. Looks straight down `-Z` at a
//! configurable world-xy centerpoint, with the frustum half-height
//! set by `extent` (in world units). Visible width tracks the live
//! window aspect.
//!
//! Natural fit for grid games: set `center` to the grid's world
//! centroid and `extent` to at least half the larger grid dimension
//! + a margin, and the whole grid lands on screen.
//!
//! Controllable via `aether.camera.topdown.set_*` mail:
//!
//! - `TopdownSetCenter { x, y }` — pan.
//! - `TopdownSetExtent { extent }` — zoom.
//!
//! Publishes `aether.camera` every tick, same sink and wire shape as
//! the orbit camera's main `src/lib.rs`.

use aether_component::{Component, Ctx, InitCtx, Sink, handlers};
use aether_kinds::{Camera, Tick, TopdownSetCenter, TopdownSetExtent, WindowSize};
use aether_math::{Mat4, Vec2, Vec3};

const DEFAULT_EXTENT: f32 = 3.0;
const Z_NEAR: f32 = 0.1;
const Z_FAR: f32 = 100.0;
/// Fallback aspect before the first `WindowSize` arrives. The
/// substrate re-pulses `WindowSize` every tick so this is only
/// visible for one frame after load.
const DEFAULT_ASPECT: f32 = 16.0 / 9.0;
/// Camera eye height along `+Z`. Orthographic projection is
/// translation-invariant along the view direction; this just has to
/// be positive and inside the far plane.
const EYE_HEIGHT: f32 = 10.0;

pub struct Topdown {
    camera: Sink<Camera>,
    center: Vec2,
    extent: f32,
    aspect: f32,
}

/// Orthographic top-down camera. Publishes `view_proj` every tick;
/// frames the world `xy` plane from above.
///
/// # Agent
/// Load alongside a 2D-ish scene (e.g. a tile grid) to view it
/// unprojected from directly above. Controls:
///
/// - `TopdownSetCenter { x, y }` — pan across the plane.
/// - `TopdownSetExtent { extent }` — zoom; half-height of the view
///   in world units, so e.g. `extent=4` shows a vertical slice
///   `[-4, +4]` and a horizontal slice `[-4*aspect, +4*aspect]`.
///
/// Use `capture_frame` between sends to verify each change.
#[handlers]
impl Component for Topdown {
    fn init(ctx: &mut InitCtx<'_>) -> Self {
        Topdown {
            camera: ctx.resolve_sink::<Camera>("aether.sink.camera"),
            center: Vec2::ZERO,
            extent: DEFAULT_EXTENT,
            aspect: DEFAULT_ASPECT,
        }
    }

    /// Publish a fresh `view_proj` each tick.
    ///
    /// # Agent
    /// Tick-driven; not useful to send manually.
    #[handler]
    fn on_tick(&mut self, ctx: &mut Ctx<'_>, _tick: Tick) {
        let half_w = self.extent * self.aspect;
        let proj = Mat4::orthographic_rh(-half_w, half_w, -self.extent, self.extent, Z_NEAR, Z_FAR);
        let eye = Vec3::new(self.center.x, self.center.y, EYE_HEIGHT);
        let target = Vec3::new(self.center.x, self.center.y, 0.0);
        let view = Mat4::look_at_rh(eye, target, Vec3::Y);
        let view_proj = proj * view;
        ctx.send(
            &self.camera,
            &Camera {
                view_proj: view_proj.to_cols_array(),
            },
        );
    }

    /// Track the live window aspect so world geometry stays unsquashed
    /// on non-square windows.
    ///
    /// # Agent
    /// Publish-subscribe; the substrate drives this, you don't need
    /// to send it.
    #[handler]
    fn on_window_size(&mut self, _ctx: &mut Ctx<'_>, size: WindowSize) {
        if size.width > 0 && size.height > 0 {
            self.aspect = size.width as f32 / size.height as f32;
        }
    }

    /// Pan the camera across the xy plane.
    #[handler]
    fn on_set_center(&mut self, _ctx: &mut Ctx<'_>, msg: TopdownSetCenter) {
        self.center = Vec2::new(msg.x, msg.y);
    }

    /// Zoom the camera. Clamps to a tiny positive floor so a zero or
    /// negative extent can't degenerate the projection into NaN.
    ///
    /// # Agent
    /// Larger values show more of the world (zoom out). Must be
    /// positive — values `<= 0` are clamped to `0.001`.
    #[handler]
    fn on_set_extent(&mut self, _ctx: &mut Ctx<'_>, msg: TopdownSetExtent) {
        self.extent = msg.extent.max(0.001);
    }
}

aether_component::export!(Topdown);
