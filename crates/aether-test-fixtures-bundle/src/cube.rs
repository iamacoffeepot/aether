//! Test-fixture component that renders a solid unit cube through a
//! fixed camera, so a `SubstrateHarness` capture scenario can assert the
//! full render pipeline end-to-end: view source + view and projection +
//! world-space geometry + depth test + GPU readback (issue 1454).
//!
//! The existing offscreen capture fixture (`probe`) paints a single
//! flat NDC triangle at the identity view, which touches none of
//! the camera path. This fixture instead emits a twelve-triangle
//! world-space cube centered at the origin (corners at ±0.5) and is the
//! reference [`ViewSource`](aether_render::ViewSource): it publishes a
//! hand-computed `ViewProjection` that frames the cube as a centered
//! silhouette. The camera sits in the all-positive octant looking back at
//! the origin, so three faces are visible and the view is
//! non-axis-aligned — the depth test actually orders the faces rather
//! than collapsing to a flat quad.
//!
//! Behaviour:
//!
//! - `init` computes the framing view once (a look-at and a perspective,
//!   built from `aether-math`) and stores it. The view is fixed, so
//!   every captured frame is deterministic.
//! - `wire` subscribes `Tick` on `aether.lifecycle` (ADR-0082),
//!   mirroring the reference camera and the probe.
//! - `ViewSubscribe` is answered with the stored view, sent through the
//!   sender cast to a `ViewProjection` subscriber. The renderer sends it
//!   when it is told to follow this fixture with `aether.render.view_from`;
//!   until then the renderer holds its identity view and the cube is
//!   unframed.
//! - On each tick the fixture emits the cube's twelve `DrawTriangle`s as
//!   one batch — six faces, each a distinct flat color so the silhouette
//!   reads as one solid blob. Vertices carry world `z`, so the
//!   `Depth32Float` / `LessEqual` test draws nearer faces over farther
//!   ones.

use core::f32::consts::FRAC_PI_4;

use aether_actor::{ActorInitError, Subscriber, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_kinds::Tick;
use aether_lifecycle::LifecycleCapability;
use aether_math::{Mat4, Rgb, Vec3};
use aether_render::{
    DrawTriangle, RenderCapability, Vertex, ViewProjection, ViewSubscribe, ViewUnsubscribe, ViewportExtent,
};

/// Half-extent of the unit cube: corners sit at ±`HALF` on every axis,
/// so the cube spans one world unit and is centered at the origin.
const HALF: f32 = 0.5;

/// Viewport the projection is built for, in physical pixels. The cube
/// scenario boots the harness at 128×96, so this 4:3 viewport keeps the
/// projected silhouette undistorted. A small mismatch with the real frame
/// only scales the silhouette slightly; the capture asserts leave margin
/// for it.
const VIEWPORT_WIDTH: u16 = 128;
const VIEWPORT_HEIGHT: u16 = 96;

/// Vertical field of view in radians (45°). Combined with the eye
/// distance below it sizes the cube to a healthy fraction of the frame
/// without bleeding to the edges.
const FIELD_OF_VIEW_Y_RADIANS: f32 = FRAC_PI_4;

/// Near / far planes bracketing the cube comfortably; the cube's world
/// `z` lives in roughly [-0.87, 0.87] after projection, well inside.
const Z_NEAR: f32 = 0.1;
const Z_FAR: f32 = 100.0;

pub struct Cube {
    view: ViewProjection,
}

impl Cube {
    /// The view that frames the cube. The eye sits in the all-positive
    /// octant and looks back at the origin, so the +X, +Y, and +Z faces
    /// are all visible and no cube edge is parallel to a frame axis. The
    /// renderer applies `projection * view`.
    fn framing_view() -> ViewProjection {
        let eye = Vec3::new(1.8, 1.5, 2.2);
        let aspect = f32::from(VIEWPORT_WIDTH) / f32::from(VIEWPORT_HEIGHT);

        ViewProjection {
            view: Mat4::look_at_rh(eye, Vec3::ZERO, Vec3::Y),
            projection: Mat4::perspective_rh(FIELD_OF_VIEW_Y_RADIANS, aspect, Z_NEAR, Z_FAR),
            eye,
            near: Z_NEAR,
            far: Z_FAR,
            extent: ViewportExtent { width: u32::from(VIEWPORT_WIDTH), height: u32::from(VIEWPORT_HEIGHT) },
        }
    }

    /// The cube's twelve world-space triangles. Each of the six faces
    /// is two triangles sharing a flat color, wound so the solid
    /// silhouette is gap-free regardless of cull state. Colors are
    /// distinct per face purely so the faces are visually separable in
    /// a captured frame; the silhouette asserts only care that the
    /// union is a solid centered blob.
    fn triangles() -> [DrawTriangle; 12] {
        // Eight corners of the cube, named by their sign on each axis.
        let corner = |sx: f32, sy: f32, sz: f32| (sx * HALF, sy * HALF, sz * HALF);
        let nnn = corner(-1.0, -1.0, -1.0);
        let pnn = corner(1.0, -1.0, -1.0);
        let npn = corner(-1.0, 1.0, -1.0);
        let ppn = corner(1.0, 1.0, -1.0);
        let nnp = corner(-1.0, -1.0, 1.0);
        let pnp = corner(1.0, -1.0, 1.0);
        let npp = corner(-1.0, 1.0, 1.0);
        let ppp = corner(1.0, 1.0, 1.0);

        // One vertex with a face color baked in.
        let vert =
            |position: (f32, f32, f32), color: Rgb| Vertex { x: position.0, y: position.1, z: position.2, color };
        // A quad as two triangles, all six vertices sharing `color`.
        let quad = |a, b, c, d, color: Rgb| {
            [
                DrawTriangle { verts: [vert(a, color), vert(b, color), vert(c, color)] },
                DrawTriangle { verts: [vert(a, color), vert(c, color), vert(d, color)] },
            ]
        };

        let [front_0, front_1] = quad(nnp, pnp, ppp, npp, Rgb::new(0.85, 0.20, 0.20)); // +Z
        let [back_0, back_1] = quad(pnn, nnn, npn, ppn, Rgb::new(0.20, 0.30, 0.85)); // -Z
        let [right_0, right_1] = quad(pnp, pnn, ppn, ppp, Rgb::new(0.20, 0.75, 0.30)); // +X
        let [left_0, left_1] = quad(nnn, nnp, npp, npn, Rgb::new(0.85, 0.75, 0.20)); // -X
        let [top_0, top_1] = quad(npp, ppp, ppn, npn, Rgb::new(0.80, 0.45, 0.85)); // +Y
        let [bottom_0, bottom_1] = quad(nnn, pnn, pnp, nnp, Rgb::new(0.30, 0.80, 0.80)); // -Y

        [front_0, front_1, back_0, back_1, right_0, right_1, left_0, left_1, top_0, top_1, bottom_0, bottom_1]
    }
}

#[actor(root, depends(RenderCapability, LifecycleCapability))]
impl WasmActor for Cube {
    const NAMESPACE: &'static str = "test.cube";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Cube { view: Cube::framing_view() })
    }

    /// Subscribe `Tick` so the chassis tick fanout drives `on_tick`.
    /// `init` can't mail (its ctx has no send surface), so the subscribe
    /// lands here in `wire` (mirrors the probe and the reference camera).
    fn wire(&mut self, ctx: &mut aether_actor::WireCtx<'_, '_>) -> Result<(), ActorInitError> {
        ctx.subscribe::<LifecycleCapability, Tick>();
        Ok(())
    }

    /// Emit the cube.
    ///
    /// # Agent
    /// Not sent manually; the substrate's tick fanout fires it once per
    /// advance for every `aether.lifecycle`-subscribed mailbox. A
    /// `capture_frame` taken after one tick shows the centered cube
    /// silhouette once the renderer follows this fixture's view.
    #[handler::event]
    fn on_tick(&mut self, ctx: &mut WasmCtx<'_, Self>, _: Tick) {
        ctx.send_many::<RenderCapability>(&Cube::triangles());
    }

    /// Send the framing view to the subscribing sender. The view never
    /// changes, so this one send is the whole publication and the fixture
    /// keeps no subscriber: a source whose view moves keeps the cast
    /// reference and sends each later view through it.
    ///
    /// # Agent
    /// Sent by a viewer that wants the view; the renderer sends it
    /// when `aether.render.view_from` names this fixture. A sender that
    /// does not take `aether.view_projection` silently is sent nothing.
    #[handler::tell]
    fn on_view_subscribe(&mut self, ctx: &mut WasmCtx<'_, Self>, _: ViewSubscribe) {
        let Some(sender) = ctx.sender() else {
            tracing::warn!("view subscribe arrived with no sender; ignoring");
            return;
        };
        let Some(viewer) = ctx.cast::<Subscriber<ViewProjection>>(sender) else {
            tracing::warn!("view subscribe sender does not take a view projection; ignoring");
            return;
        };

        ctx.send_to(viewer, &self.view);
    }

    /// Nothing to release: the fixed view is sent once, at subscribe, and
    /// no subscriber is kept.
    ///
    /// # Agent
    /// Sent by a viewer that no longer wants the view; the renderer sends
    /// it when it is told to follow another source.
    #[handler::tell]
    fn on_view_unsubscribe(&mut self, _ctx: &mut WasmCtx<'_, Self>, _: ViewUnsubscribe) {
        let _ = self;
    }
}
