//! Test-fixture component that renders a solid unit cube through a
//! fixed camera, so a `TestBench` capture scenario can assert the
//! full render pipeline end-to-end: camera + `view_proj` + world-space
//! geometry + depth test + GPU readback (issue 1454).
//!
//! The existing offscreen capture fixture (`probe`) paints a single
//! flat NDC triangle at identity `view_proj`, which touches none of
//! the camera path. This fixture instead emits a twelve-triangle
//! world-space cube centered at the origin (corners at ±0.5) and
//! publishes a hand-computed `Camera { view_proj }` that frames the
//! cube as a centered silhouette. The camera sits in the all-positive
//! octant looking back at the origin, so three faces are visible and
//! the view is non-axis-aligned — the depth test actually orders the
//! faces rather than collapsing to a flat quad.
//!
//! Behaviour:
//!
//! - `init` computes the `view_proj` once (perspective × look-at,
//!   built from `aether-math`) and stores it. The matrix is fixed, so
//!   every captured frame is deterministic.
//! - `wire` subscribes `Tick` on `aether.lifecycle` (ADR-0082),
//!   mirroring the reference camera and the probe.
//! - On each tick the fixture publishes the stored `Camera` to the
//!   chassis render mailbox, then emits the cube's twelve
//!   `DrawTriangle`s — six faces, each a distinct flat color so the
//!   silhouette reads as one solid blob. Vertices carry world `z`, so
//!   the `Depth32Float` / `LessEqual` test draws nearer faces over
//!   farther ones.

use core::f32::consts::FRAC_PI_4;

use aether_actor::{BootError, FfiActor, FfiCtx, FfiInitCtx, actor};
use aether_capabilities::lifecycle::LifecycleMailboxExt;
use aether_capabilities::{LifecycleCapability, RenderCapability};
use aether_kinds::{Camera, DrawTriangle, Tick, Vertex};
use aether_math::{Mat4, Vec3};

/// Half-extent of the unit cube: corners sit at ±`HALF` on every axis,
/// so the cube spans one world unit and is centered at the origin.
const HALF: f32 = 0.5;

/// Aspect ratio the `view_proj` is built for. The cube scenario boots
/// the bench at 128×96, so a 4:3 aspect keeps the projected silhouette
/// undistorted. A small mismatch with the real frame only scales the
/// silhouette slightly; the capture asserts leave margin for it.
const ASPECT: f32 = 128.0 / 96.0;

/// Vertical field of view in radians (45°). Combined with the eye
/// distance below it sizes the cube to a healthy fraction of the frame
/// without bleeding to the edges.
const FIELD_OF_VIEW_Y_RADIANS: f32 = FRAC_PI_4;

/// Near / far planes bracketing the cube comfortably; the cube's world
/// `z` lives in roughly [-0.87, 0.87] after projection, well inside.
const Z_NEAR: f32 = 0.1;
const Z_FAR: f32 = 100.0;

pub struct Cube {
    view_proj: [f32; 16],
}

impl Cube {
    /// World-to-clip matrix that frames the cube. The eye sits in the
    /// all-positive octant and looks back at the origin, so the +X,
    /// +Y, and +Z faces are all visible and no cube edge is parallel
    /// to a frame axis. `proj * view` is the column-major product the
    /// chassis uploads verbatim as the `view_proj` uniform.
    fn framing_view_proj() -> [f32; 16] {
        let eye = Vec3::new(1.8, 1.5, 2.2);
        let view = Mat4::look_at_rh(eye, Vec3::ZERO, Vec3::Y);
        let proj = Mat4::perspective_rh(FIELD_OF_VIEW_Y_RADIANS, ASPECT, Z_NEAR, Z_FAR);
        (proj * view).to_cols_array()
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
        let vert = |position: (f32, f32, f32), color: [f32; 3]| Vertex {
            x: position.0,
            y: position.1,
            z: position.2,
            r: color[0],
            g: color[1],
            b: color[2],
        };
        // A quad as two triangles, all six vertices sharing `color`.
        let quad = |a, b, c, d, color: [f32; 3]| {
            [
                DrawTriangle {
                    verts: [vert(a, color), vert(b, color), vert(c, color)],
                },
                DrawTriangle {
                    verts: [vert(a, color), vert(c, color), vert(d, color)],
                },
            ]
        };

        let [front_0, front_1] = quad(nnp, pnp, ppp, npp, [0.85, 0.20, 0.20]); // +Z
        let [back_0, back_1] = quad(pnn, nnn, npn, ppn, [0.20, 0.30, 0.85]); // -Z
        let [right_0, right_1] = quad(pnp, pnn, ppn, ppp, [0.20, 0.75, 0.30]); // +X
        let [left_0, left_1] = quad(nnn, nnp, npp, npn, [0.85, 0.75, 0.20]); // -X
        let [top_0, top_1] = quad(npp, ppp, ppn, npn, [0.80, 0.45, 0.85]); // +Y
        let [bottom_0, bottom_1] = quad(nnn, pnn, pnp, nnp, [0.30, 0.80, 0.80]); // -Y

        [
            front_0, front_1, back_0, back_1, right_0, right_1, left_0, left_1, top_0, top_1,
            bottom_0, bottom_1,
        ]
    }
}

#[actor]
impl FfiActor for Cube {
    const NAMESPACE: &'static str = "cube";

    fn init(_ctx: &mut FfiInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Cube {
            view_proj: Cube::framing_view_proj(),
        })
    }

    /// Subscribe `Tick` so the chassis tick fanout drives `on_tick`.
    /// `init` can't mail (its ctx has no send surface), so the subscribe
    /// lands here in `wire` (mirrors the probe and the reference camera).
    fn wire(&mut self, ctx: &mut FfiCtx<'_>) {
        ctx.actor::<LifecycleCapability>().subscribe::<Tick>();
    }

    /// Publish the fixed camera, then emit the cube. The camera goes
    /// first so the chassis's `view_proj` uniform holds the framing
    /// matrix before the triangles are rasterized in the same frame.
    ///
    /// # Agent
    /// Not sent manually; the substrate's tick fanout fires it once per
    /// advance for every `aether.lifecycle`-subscribed mailbox. A
    /// `capture_frame` taken after one tick shows the centered cube
    /// silhouette.
    #[handler]
    fn on_tick(&mut self, ctx: &mut FfiCtx<'_>, _: Tick) {
        ctx.actor::<RenderCapability>().send(&Camera {
            view_proj: self.view_proj,
        });
        for triangle in Cube::triangles() {
            ctx.actor::<RenderCapability>().send(&triangle);
        }
    }
}
