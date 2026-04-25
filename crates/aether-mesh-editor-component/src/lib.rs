//! Mesh editor component (Spike C). Holds a triangulated mesh in
//! `&mut self`, applies DSL ops via mail handlers, and re-emits the
//! mesh as `DrawTriangle` to the `"render"` sink every tick.
//!
//! Stool tier (current): cube primitive + per-vertex translation.
//! Other primitives, scale/rotate, extrude, recoloring, and OBJ
//! export are scoped for follow-up once the iteration loop proves
//! itself end-to-end (load → set_primitive → capture_frame → mutate
//! → capture_frame).
//!
//! # Vertex ids for cube primitive
//!
//! `SetPrimitive { primitive: Cube, center, size }` produces 8
//! vertices in a deterministic layout. Index by sign on each axis,
//! `0` = negative half, `1` = positive half:
//!
//! - `0` = `(-, -, -)` `1` = `(+, -, -)` `2` = `(+, -, +)` `3` = `(-, -, +)`
//! - `4` = `(-, +, -)` `5` = `(+, +, -)` `6` = `(+, +, +)` `7` = `(-, +, +)`
//!
//! So vertices `4..=7` are the `+y` (top) face — translate them with
//! `delta: [0, dy, 0]` to extrude / push the top up.

use aether_component::{Component, Ctx, InitCtx, Sink, handlers};
use aether_kinds::{DrawTriangle, MeshPrimitive, SetPrimitive, Tick, TranslateVertices, Vertex};
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
/// `translate_vertices` against known vertex ids (see crate doc for
/// the cube-vertex layout). Use `capture_frame` between ops to verify.
///
/// - `SetPrimitive { primitive: Cube, center: [0, 0, 0], size: 1.0 }`
///   — replace the mesh with a unit cube
/// - `TranslateVertices { vertex_ids: [4, 5, 6, 7], delta: [0, 0.5, 0] }`
///   — push the top face up by 0.5 units
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
    /// `Cube` is the only family implemented today; others reply
    /// implicitly (no-op) and log a warning — future work.
    ///
    /// # Agent
    /// Use `Cube` with `size` in world units (1.0 fits the default
    /// orbit camera nicely). `center` translates the whole primitive.
    #[handler]
    fn on_set_primitive(&mut self, _ctx: &mut Ctx<'_>, msg: SetPrimitive) {
        match msg.primitive {
            MeshPrimitive::Cube => {
                let center = Vec3::new(msg.center[0], msg.center[1], msg.center[2]);
                build_cube(&mut self.vertices, &mut self.faces, center, msg.size);
                self.rebuild_render_cache();
            }
            MeshPrimitive::Sphere | MeshPrimitive::Cylinder | MeshPrimitive::Plane => {
                // Reserved primitive families; no-op until implemented.
            }
        }
    }

    /// Translate each named vertex by `delta`. Out-of-range ids are
    /// silently skipped so a partial-overlap selection still applies.
    ///
    /// # Agent
    /// See the crate doc for the cube vertex layout. Selection is by
    /// raw index in v1 — no `select_top_face` sugar yet.
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

aether_component::export!(MeshEditor);
