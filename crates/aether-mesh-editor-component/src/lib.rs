//! Mesh editor component (Spike C). Holds a triangulated mesh in
//! `&mut self`, applies DSL ops via mail handlers, and re-emits the
//! mesh as `DrawTriangle` to the `"render"` sink every tick.
//!
//! Current ops: `set_primitive` (Cube + Cylinder), `translate_vertices`,
//! `scale_vertices`, `describe`. Extrude / face deletion / new-vertex
//! / OBJ export are scoped for follow-up — issue 241 tracks the v3
//! op set.
//!
//! # Identity rules — read first
//!
//! Vertex and face ids are **monotonic and never reused**. A
//! `set_primitive` resets the mesh wholesale and starts ids at zero;
//! after that, every new vertex or face gets the next unused id (=
//! `len()` of the underlying sparse vec). When delete arrives in a
//! later PR, deleted slots become tombstones — the id is permanently
//! gone, future allocations skip past it.
//!
//! Why: agents (and humans) iterating against this editor over
//! multiple captures can't tolerate id renumbering. "Vertex 17 is
//! gone" is a stable fact; "vertex 17 was renumbered to 12 because
//! 5 was deleted" is a context disaster.
//!
//! # Inspecting the current mesh
//!
//! Mail `aether.mesh.describe` to the editor; it publishes
//! `aether.mesh.state` to `hub.claude.broadcast`. The MCP harness
//! reads it via `receive_mail`. Tombstoned ids are excluded from
//! the snapshot — a missing id from a previous snapshot means it
//! was deleted (or never existed).
//!
//! # Vertex ids for cube primitive (at primitive creation)
//!
//! `Primitive::Cube { center, size }` produces 8 vertices in a
//! deterministic layout. Index by sign on each axis, `0` = negative
//! half, `1` = positive half:
//!
//! - `0` = `(-, -, -)` `1` = `(+, -, -)` `2` = `(+, -, +)` `3` = `(-, -, +)`
//! - `4` = `(-, +, -)` `5` = `(+, +, -)` `6` = `(+, +, +)` `7` = `(-, +, +)`
//!
//! Vertices `4..=7` are the `+y` (top) face — translate them with
//! `delta: [0, dy, 0]` to push the top up. Layout is stable AT primitive
//! creation; after any mutation, prefer `describe` over assumed indices.
//!
//! # Vertex ids for cylinder primitive (at primitive creation)
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
//! center), `N` for the bottom cap. Same caveat as cube: layout
//! stable AT creation, prefer `describe` after edits.

use aether_component::{Component, Ctx, InitCtx, Sink, handlers};
use aether_kinds::{
    Describe, DrawTriangle, FaceInfo, MeshState, Primitive, ScaleVertices, SetPrimitive, Tick,
    TranslateVertices, Vertex, VertexInfo,
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

/// Mesh editor component. Holds the current mesh in sparse vertex
/// and face vectors (`Vec<Option<T>>` indexed by id; tombstones for
/// deleted entries). Rebuilds a `DrawTriangle` cache on mutation and
/// replays it every tick.
///
/// # Agent
/// Workflow: `set_primitive` to seed the mesh, then iterate with
/// `translate_vertices`, `scale_vertices`. Send `describe` to get a
/// `MeshState` snapshot back via the broadcast channel. Use
/// `capture_frame` between ops to verify visually.
///
/// - `SetPrimitive { primitive: Cube { center, size } }` — replace
///   the mesh with a cube
/// - `SetPrimitive { primitive: Cylinder { center, radius, height,
///   segments } }` — replace with a capped cylinder around the y axis
/// - `TranslateVertices { vertex_ids, delta }` — shift listed vertices
/// - `ScaleVertices { vertex_ids, pivot, factor }` — scale listed
///   vertices around a pivot point, per axis
/// - `Describe { }` — request a mesh-state snapshot. Reply lands on
///   the broadcast channel as `MeshState`; consume via `receive_mail`.
pub struct MeshEditor {
    render: Sink<DrawTriangle>,
    broadcast: Sink<MeshState>,
    vertices: Vec<Option<Vec3>>,
    faces: Vec<Option<Face>>,
    rendered: Vec<DrawTriangle>,
}

#[handlers]
impl Component for MeshEditor {
    fn init(ctx: &mut InitCtx<'_>) -> Self {
        MeshEditor {
            render: ctx.resolve_sink::<DrawTriangle>("render"),
            broadcast: ctx.resolve_sink::<MeshState>("hub.claude.broadcast"),
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
    /// Vertex and face ids reset to zero (fresh mesh, no inherited
    /// tombstones from the previous one).
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

    /// Translate each named vertex by `delta`. Out-of-range ids and
    /// tombstoned ids are silently skipped so a partial-overlap
    /// selection still applies cleanly to the live ids.
    ///
    /// # Agent
    /// See the crate doc for per-primitive vertex layouts. After any
    /// edit, prefer a fresh `describe` over assumed indices.
    #[handler]
    fn on_translate_vertices(&mut self, _ctx: &mut Ctx<'_>, msg: TranslateVertices) {
        let delta = Vec3::new(msg.delta[0], msg.delta[1], msg.delta[2]);
        let mut touched = false;
        for id in &msg.vertex_ids {
            if let Some(slot) = self.vertices.get_mut(*id as usize)
                && let Some(v) = slot.as_mut()
            {
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
    /// changing the unaffected axes. Out-of-range and tombstoned
    /// ids skipped.
    ///
    /// # Agent
    /// To flare the top of a cylinder centered at the origin with
    /// height 1.0, scale the top ring by `factor: [1.2, 1, 1.2]`
    /// with `pivot: [0, 1, 0]`.
    #[handler]
    fn on_scale_vertices(&mut self, _ctx: &mut Ctx<'_>, msg: ScaleVertices) {
        let pivot = Vec3::new(msg.pivot[0], msg.pivot[1], msg.pivot[2]);
        let factor = Vec3::new(msg.factor[0], msg.factor[1], msg.factor[2]);
        let mut touched = false;
        for id in &msg.vertex_ids {
            if let Some(slot) = self.vertices.get_mut(*id as usize)
                && let Some(v) = slot.as_mut()
            {
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

    /// Publish the current mesh as `MeshState` to the broadcast sink.
    /// Tombstoned (deleted) ids are excluded; the snapshot only
    /// includes live vertices and faces. The MCP harness reads the
    /// reply via `receive_mail`.
    ///
    /// # Agent
    /// Empty payload: `Describe { }`. Watch for an `aether.mesh.state`
    /// item in the next `receive_mail` drain.
    #[handler]
    fn on_describe(&mut self, ctx: &mut Ctx<'_>, _msg: Describe) {
        let mut vertices = Vec::with_capacity(self.vertices.len());
        for (id, slot) in self.vertices.iter().enumerate() {
            if let Some(v) = slot {
                vertices.push(VertexInfo {
                    id: id as u32,
                    x: v.x,
                    y: v.y,
                    z: v.z,
                });
            }
        }
        let mut faces = Vec::with_capacity(self.faces.len());
        for (id, slot) in self.faces.iter().enumerate() {
            if let Some(f) = slot {
                faces.push(FaceInfo {
                    id: id as u32,
                    vertex_ids: f.vertices,
                    color: f.color,
                });
            }
        }
        ctx.send_postcard(&self.broadcast, &MeshState { vertices, faces });
    }
}

impl MeshEditor {
    fn rebuild_render_cache(&mut self) {
        self.rendered.clear();
        self.rendered.reserve(self.faces.len());
        for face_slot in &self.faces {
            let Some(face) = face_slot else { continue };
            let [a, b, c] = face.vertices;
            let (Some(Some(va)), Some(Some(vb)), Some(Some(vc))) = (
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
/// length `size`. Replaces `vertices` and `faces` wholesale (clears
/// the sparse vecs before populating; ids start at 0). Vertex layout
/// matches the crate doc.
fn build_cube(
    vertices: &mut Vec<Option<Vec3>>,
    faces: &mut Vec<Option<Face>>,
    center: Vec3,
    size: f32,
) {
    let h = size * 0.5;
    vertices.clear();
    vertices.extend([
        Some(Vec3::new(center.x - h, center.y - h, center.z - h)), // 0
        Some(Vec3::new(center.x + h, center.y - h, center.z - h)), // 1
        Some(Vec3::new(center.x + h, center.y - h, center.z + h)), // 2
        Some(Vec3::new(center.x - h, center.y - h, center.z + h)), // 3
        Some(Vec3::new(center.x - h, center.y + h, center.z - h)), // 4
        Some(Vec3::new(center.x + h, center.y + h, center.z - h)), // 5
        Some(Vec3::new(center.x + h, center.y + h, center.z + h)), // 6
        Some(Vec3::new(center.x - h, center.y + h, center.z + h)), // 7
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
    faces.extend([
        Some(Face {
            vertices: [0, 2, 1],
            color: BOTTOM,
        }),
        Some(Face {
            vertices: [0, 3, 2],
            color: BOTTOM,
        }),
        Some(Face {
            vertices: [4, 5, 6],
            color: TOP,
        }),
        Some(Face {
            vertices: [4, 6, 7],
            color: TOP,
        }),
        Some(Face {
            vertices: [3, 6, 2],
            color: FRONT,
        }),
        Some(Face {
            vertices: [3, 7, 6],
            color: FRONT,
        }),
        Some(Face {
            vertices: [0, 1, 5],
            color: BACK,
        }),
        Some(Face {
            vertices: [0, 5, 4],
            color: BACK,
        }),
        Some(Face {
            vertices: [0, 4, 7],
            color: LEFT,
        }),
        Some(Face {
            vertices: [0, 7, 3],
            color: LEFT,
        }),
        Some(Face {
            vertices: [1, 2, 6],
            color: RIGHT,
        }),
        Some(Face {
            vertices: [1, 6, 5],
            color: RIGHT,
        }),
    ]);
}

/// Generate a capped cylinder around the y axis with `n` segments.
/// See the crate doc for the vertex layout. `n` should be at least 3
/// (caller clamps).
///
/// Side wall is two triangles per segment; the top cap is a fan of
/// `n` triangles from the top center vertex (id `2n+1`); the bottom
/// cap is the same fan from `2n`. Per-segment side hue alternates
/// between two shades so adjacent segments are visually
/// distinguishable in `capture_frame`.
fn build_cylinder(
    vertices: &mut Vec<Option<Vec3>>,
    faces: &mut Vec<Option<Face>>,
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
        vertices.push(Some(Vec3::new(x, bottom_y, z)));
    }
    for i in 0..n {
        let theta = TAU * (i as f32) / (n as f32);
        let x = center.x + radius * theta.cos();
        let z = center.z + radius * theta.sin();
        vertices.push(Some(Vec3::new(x, top_y, z)));
    }
    // Bottom center (2n), top center (2n+1).
    vertices.push(Some(Vec3::new(center.x, bottom_y, center.z)));
    vertices.push(Some(Vec3::new(center.x, top_y, center.z)));

    const SIDE_A: [f32; 3] = [0.30, 0.55, 0.85]; // mid blue
    const SIDE_B: [f32; 3] = [0.40, 0.65, 0.92]; // light blue
    const TOP_CAP: [f32; 3] = [0.95, 0.95, 0.95]; // white
    const BOTTOM_CAP: [f32; 3] = [0.40, 0.40, 0.40]; // grey

    faces.clear();
    let bottom_center = 2 * n;
    let top_center = 2 * n + 1;

    // Side wall.
    for i in 0..n {
        let next = (i + 1) % n;
        let bot_a = i;
        let bot_b = next;
        let top_a = n + i;
        let top_b = n + next;
        let color = if i % 2 == 0 { SIDE_A } else { SIDE_B };
        faces.push(Some(Face {
            vertices: [bot_a, bot_b, top_b],
            color,
        }));
        faces.push(Some(Face {
            vertices: [bot_a, top_b, top_a],
            color,
        }));
    }

    // Top cap.
    for i in 0..n {
        let next = (i + 1) % n;
        faces.push(Some(Face {
            vertices: [top_center, n + i, n + next],
            color: TOP_CAP,
        }));
    }

    // Bottom cap.
    for i in 0..n {
        let next = (i + 1) % n;
        faces.push(Some(Face {
            vertices: [bottom_center, next, i],
            color: BOTTOM_CAP,
        }));
    }
}

aether_component::export!(MeshEditor);
