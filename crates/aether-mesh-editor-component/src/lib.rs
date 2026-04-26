//! Mesh editor component (Spike C). Holds a triangulated mesh in
//! `&mut self`, applies DSL ops via mail handlers, and re-emits the
//! mesh as `DrawTriangle` to the `"render"` sink every tick.
//!
//! Current ops: `set_primitive` (Cube + Cylinder), `translate_vertices`,
//! `scale_vertices`. Extrude / face deletion / new-vertex / OBJ export
//! are scoped for follow-up — issue 241 tracks the v3 op set.
//!
//! # Vertex ids for cube primitive
//!
//! `Primitive::Cube { center, size }` produces 8 vertices in a
//! deterministic layout. Index by sign on each axis, `0` = negative
//! half, `1` = positive half:
//!
//! - `0` = `(-, -, -)` `1` = `(+, -, -)` `2` = `(+, -, +)` `3` = `(-, -, +)`
//! - `4` = `(-, +, -)` `5` = `(+, +, -)` `6` = `(+, +, +)` `7` = `(-, +, +)`
//!
//! Vertices `4..=7` are the `+y` (top) face — translate them with
//! `delta: [0, dy, 0]` to push the top up.
//!
//! # Vertex ids for cylinder primitive
//!
//! `Primitive::Cylinder { center, radius, height, segments: N }`
//! produces `2N + 2` vertices arranged as:
//!
//! - `0..N`     — bottom ring, CCW around the y axis. `i`-th vertex
//!   is at angle `2π·i/N` (so vertex 0 sits on `+x`).
//! - `N..2N`    — top ring, same angular positions as the bottom ring,
//!   `height` units higher.
//! - `2N`       — bottom center (cap fan apex).
//! - `2N + 1`   — top center.
//!
//! Faces are `4N` triangles total: `2N` for the side wall (one quad
//! per segment, triangulated), `N` for the top cap (fan from top
//! center), `N` for the bottom cap.
//!
//! Useful selections for a 24-segment cylinder (N = 24):
//! - top ring: `vertex_ids = (24..48).collect()`
//! - bottom ring: `vertex_ids = (0..24).collect()`
//! - flare the top: `scale_vertices` on top ring with
//!   `factor: [1.2, 1, 1.2]` and `pivot: [center.x, top_y, center.z]`.

use aether_component::{Component, Ctx, InitCtx, Sink, handlers};
use aether_kinds::{
    DrawTriangle, Primitive, ScaleVertices, SetPrimitive, Tick, TranslateVertices, Vertex,
};
use aether_math::Vec3;

/// One face of the editor's mesh. Stored as the three vertex indices
/// into `MeshEditor::vertices` plus an RGB color used for every vertex
/// of the emitted `DrawTriangle`. Triangulated only — quads/n-gons are
/// pre-split when a primitive is generated.
#[derive(Clone, Copy)]
struct Face {
    vertices: [u32; 3],
    color: [f32; 3],
}

/// Mesh editor component. Holds the current mesh in `vertices` +
/// `faces`; rebuilds a `DrawTriangle` cache on mutation and replays it
/// every tick.
///
/// # Agent
/// Workflow: `set_primitive` to seed the mesh, then iterate with
/// `translate_vertices` and `scale_vertices` against known vertex ids
/// (see crate doc for the per-primitive layouts). Use `capture_frame`
/// between ops to verify.
///
/// - `SetPrimitive { primitive: Cube { center, size } }` — replace
///   the mesh with a cube
/// - `SetPrimitive { primitive: Cylinder { center, radius, height,
///   segments } }` — replace with a capped cylinder around the y axis
/// - `TranslateVertices { vertex_ids, delta }` — shift listed vertices
/// - `ScaleVertices { vertex_ids, pivot, factor }` — scale listed
///   vertices around a pivot point, per axis
pub struct MeshEditor {
    render: Sink<DrawTriangle>,
    vertices: Vec<Vec3>,
    faces: Vec<Face>,
    rendered: Vec<DrawTriangle>,
}

#[handlers]
impl Component for MeshEditor {
    fn init(ctx: &mut InitCtx<'_>) -> Self {
        MeshEditor {
            render: ctx.resolve_sink::<DrawTriangle>("render"),
            vertices: Vec::new(),
            faces: Vec::new(),
            rendered: Vec::new(),
        }
    }

    /// Replay the current mesh as `DrawTriangle` mail every tick.
    /// Empty mesh emits nothing.
    ///
    /// # Agent
    /// Tick-driven; not useful to send manually.
    #[handler]
    fn on_tick(&mut self, ctx: &mut Ctx<'_>, _tick: Tick) {
        if !self.rendered.is_empty() {
            ctx.send_many(&self.render, &self.rendered);
        }
    }

    /// Replace the mesh with a procedurally generated primitive.
    ///
    /// # Agent
    /// `Cube { center, size }` for axis-aligned boxes; `Cylinder
    /// { center, radius, height, segments }` for cylinders around
    /// the y axis. `segments` of 16–32 read as smooth at default
    /// camera distance; lower for visible facets.
    #[handler]
    fn on_set_primitive(&mut self, _ctx: &mut Ctx<'_>, msg: SetPrimitive) {
        match msg.primitive {
            Primitive::Cube { center, size } => {
                build_cube(
                    &mut self.vertices,
                    &mut self.faces,
                    Vec3::new(center[0], center[1], center[2]),
                    size,
                );
            }
            Primitive::Cylinder {
                center,
                radius,
                height,
                segments,
            } => {
                build_cylinder(
                    &mut self.vertices,
                    &mut self.faces,
                    Vec3::new(center[0], center[1], center[2]),
                    radius,
                    height,
                    segments.max(3),
                );
            }
        }
        self.rebuild_render_cache();
    }

    /// Translate each named vertex by `delta`. Out-of-range ids are
    /// silently skipped so a partial-overlap selection still applies.
    ///
    /// # Agent
    /// See the crate doc for per-primitive vertex layouts.
    #[handler]
    fn on_translate_vertices(&mut self, _ctx: &mut Ctx<'_>, msg: TranslateVertices) {
        let delta = Vec3::new(msg.delta[0], msg.delta[1], msg.delta[2]);
        let mut touched = false;
        for id in &msg.vertex_ids {
            if let Some(v) = self.vertices.get_mut(*id as usize) {
                *v += delta;
                touched = true;
            }
        }
        if touched {
            self.rebuild_render_cache();
        }
    }

    /// Scale each named vertex's offset from `pivot` by `factor`,
    /// per axis. Non-uniform factors flare or flatten without
    /// changing the unaffected axes.
    ///
    /// # Agent
    /// To flare the top of a cylinder centered at the origin with
    /// height 1.0, scale the top ring by `factor: [1.2, 1, 1.2]`
    /// with `pivot: [0, 1, 0]`. Out-of-range ids skipped.
    #[handler]
    fn on_scale_vertices(&mut self, _ctx: &mut Ctx<'_>, msg: ScaleVertices) {
        let pivot = Vec3::new(msg.pivot[0], msg.pivot[1], msg.pivot[2]);
        let factor = Vec3::new(msg.factor[0], msg.factor[1], msg.factor[2]);
        let mut touched = false;
        for id in &msg.vertex_ids {
            if let Some(v) = self.vertices.get_mut(*id as usize) {
                let offset = *v - pivot;
                let scaled = Vec3::new(
                    offset.x * factor.x,
                    offset.y * factor.y,
                    offset.z * factor.z,
                );
                *v = pivot + scaled;
                touched = true;
            }
        }
        if touched {
            self.rebuild_render_cache();
        }
    }
}

impl MeshEditor {
    fn rebuild_render_cache(&mut self) {
        self.rendered.clear();
        self.rendered.reserve(self.faces.len());
        for face in &self.faces {
            let [a, b, c] = face.vertices;
            let (Some(va), Some(vb), Some(vc)) = (
                self.vertices.get(a as usize),
                self.vertices.get(b as usize),
                self.vertices.get(c as usize),
            ) else {
                continue;
            };
            let [r, g, b_] = face.color;
            self.rendered.push(DrawTriangle {
                verts: [
                    Vertex {
                        x: va.x,
                        y: va.y,
                        z: va.z,
                        r,
                        g,
                        b: b_,
                    },
                    Vertex {
                        x: vb.x,
                        y: vb.y,
                        z: vb.z,
                        r,
                        g,
                        b: b_,
                    },
                    Vertex {
                        x: vc.x,
                        y: vc.y,
                        z: vc.z,
                        r,
                        g,
                        b: b_,
                    },
                ],
            });
        }
    }
}

/// Generate a unit-axis-aligned cube centered at `center` with edge
/// length `size`. Replaces `vertices` and `faces` wholesale. Vertex
/// layout matches the crate doc: bit `x = id & 1`, `y = (id >> 2) & 1`,
/// `z = (id >> 1) & 1` with `0` = negative half, `1` = positive half.
fn build_cube(vertices: &mut Vec<Vec3>, faces: &mut Vec<Face>, center: Vec3, size: f32) {
    let h = size * 0.5;
    vertices.clear();
    vertices.extend_from_slice(&[
        Vec3::new(center.x - h, center.y - h, center.z - h), // 0 (-, -, -)
        Vec3::new(center.x + h, center.y - h, center.z - h), // 1 (+, -, -)
        Vec3::new(center.x + h, center.y - h, center.z + h), // 2 (+, -, +)
        Vec3::new(center.x - h, center.y - h, center.z + h), // 3 (-, -, +)
        Vec3::new(center.x - h, center.y + h, center.z - h), // 4 (-, +, -)
        Vec3::new(center.x + h, center.y + h, center.z - h), // 5 (+, +, -)
        Vec3::new(center.x + h, center.y + h, center.z + h), // 6 (+, +, +)
        Vec3::new(center.x - h, center.y + h, center.z + h), // 7 (-, +, +)
    ]);

    // Distinct hue per face so the agent can read orientation directly
    // from a `capture_frame` without needing wireframe overlays.
    const BOTTOM: [f32; 3] = [0.40, 0.40, 0.40]; // grey
    const TOP: [f32; 3] = [0.95, 0.95, 0.95]; // white
    const FRONT: [f32; 3] = [0.85, 0.20, 0.20]; // red    (+z)
    const BACK: [f32; 3] = [0.20, 0.30, 0.85]; // blue   (-z)
    const LEFT: [f32; 3] = [0.20, 0.75, 0.30]; // green  (-x)
    const RIGHT: [f32; 3] = [0.95, 0.85, 0.20]; // yellow (+x)

    faces.clear();
    faces.extend_from_slice(&[
        Face {
            vertices: [0, 2, 1],
            color: BOTTOM,
        },
        Face {
            vertices: [0, 3, 2],
            color: BOTTOM,
        },
        Face {
            vertices: [4, 5, 6],
            color: TOP,
        },
        Face {
            vertices: [4, 6, 7],
            color: TOP,
        },
        Face {
            vertices: [3, 6, 2],
            color: FRONT,
        },
        Face {
            vertices: [3, 7, 6],
            color: FRONT,
        },
        Face {
            vertices: [0, 1, 5],
            color: BACK,
        },
        Face {
            vertices: [0, 5, 4],
            color: BACK,
        },
        Face {
            vertices: [0, 4, 7],
            color: LEFT,
        },
        Face {
            vertices: [0, 7, 3],
            color: LEFT,
        },
        Face {
            vertices: [1, 2, 6],
            color: RIGHT,
        },
        Face {
            vertices: [1, 6, 5],
            color: RIGHT,
        },
    ]);
}

/// Generate a capped cylinder around the y axis with `n` segments.
/// See the crate doc for the vertex layout. `n` should be at least 3
/// (caller clamps).
///
/// Side wall is two triangles per segment; the top cap is a fan
/// of `n` triangles from the top center vertex (id `2n+1`); the
/// bottom cap is the same fan from `2n`. Per-segment side hue
/// alternates between two shades so adjacent segments are
/// visually distinguishable in `capture_frame`.
fn build_cylinder(
    vertices: &mut Vec<Vec3>,
    faces: &mut Vec<Face>,
    center: Vec3,
    radius: f32,
    height: f32,
    n: u32,
) {
    use core::f32::consts::TAU;

    vertices.clear();
    let bottom_y = center.y;
    let top_y = center.y + height;

    // Bottom ring (0..n) and top ring (n..2n).
    for i in 0..n {
        let theta = TAU * (i as f32) / (n as f32);
        let x = center.x + radius * theta.cos();
        let z = center.z + radius * theta.sin();
        vertices.push(Vec3::new(x, bottom_y, z));
    }
    for i in 0..n {
        let theta = TAU * (i as f32) / (n as f32);
        let x = center.x + radius * theta.cos();
        let z = center.z + radius * theta.sin();
        vertices.push(Vec3::new(x, top_y, z));
    }
    // Bottom center (2n), top center (2n+1).
    vertices.push(Vec3::new(center.x, bottom_y, center.z));
    vertices.push(Vec3::new(center.x, top_y, center.z));

    const SIDE_A: [f32; 3] = [0.30, 0.55, 0.85]; // mid blue
    const SIDE_B: [f32; 3] = [0.40, 0.65, 0.92]; // light blue
    const TOP_CAP: [f32; 3] = [0.95, 0.95, 0.95]; // white
    const BOTTOM_CAP: [f32; 3] = [0.40, 0.40, 0.40]; // grey

    faces.clear();
    let bottom_center = 2 * n;
    let top_center = 2 * n + 1;

    // Side wall: for each segment i, vertices are
    //   bot_a = i, bot_b = (i+1) % n, top_a = n + i, top_b = n + (i+1) % n
    // Two triangles per segment, CCW when viewed from outside (+x at i=0).
    for i in 0..n {
        let next = (i + 1) % n;
        let bot_a = i;
        let bot_b = next;
        let top_a = n + i;
        let top_b = n + next;
        let color = if i % 2 == 0 { SIDE_A } else { SIDE_B };
        // Quad (bot_a, bot_b, top_b, top_a) split into two CCW tris.
        faces.push(Face {
            vertices: [bot_a, bot_b, top_b],
            color,
        });
        faces.push(Face {
            vertices: [bot_a, top_b, top_a],
            color,
        });
    }

    // Top cap: fan from top_center, CCW when viewed from +y looking down.
    // Ring goes CCW around y axis when viewed from +y, so the fan
    // triangles wind (top_center, top_a, top_b).
    for i in 0..n {
        let next = (i + 1) % n;
        faces.push(Face {
            vertices: [top_center, n + i, n + next],
            color: TOP_CAP,
        });
    }

    // Bottom cap: fan from bottom_center, CCW when viewed from -y.
    // Ring goes CCW from +y view, which is CW from -y view, so the
    // fan triangles wind (bottom_center, bot_b, bot_a) to face down.
    for i in 0..n {
        let next = (i + 1) % n;
        faces.push(Face {
            vertices: [bottom_center, next, i],
            color: BOTTOM_CAP,
        });
    }
}

aether_component::export!(MeshEditor);
