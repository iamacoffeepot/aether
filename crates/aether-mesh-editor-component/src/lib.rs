//! Mesh editor component (Spike C). Holds a triangulated mesh in
//! `&mut self`, applies DSL ops via mail handlers, and re-emits the
//! mesh as `DrawTriangle` to the `"render"` sink every tick.
//!
//! Current ops: `set_primitive` (Cube + Cylinder), `translate_vertices`,
//! `scale_vertices`, `rotate_vertices`, `extrude_face`, `delete_faces`,
//! `describe`. New-vertex / new-face authoring and OBJ export are
//! scoped for follow-up.
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

use std::collections::{BTreeMap, BTreeSet};

use aether_component::{Component, Ctx, InitCtx, Sink, handlers};
use aether_kinds::{
    DeleteFaces, Describe, DrawTriangle, ExtrudeFace, FaceInfo, MeshState, Primitive,
    RotateVertices, ScaleVertices, SetPrimitive, Tick, TranslateVertices, Vertex, VertexInfo,
};
use aether_math::{Quat, Vec3};

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
/// - `RotateVertices { vertex_ids, pivot, axis, angle }` — rotate
///   listed vertices around an axis through a pivot
/// - `ExtrudeFace { face_ids, distance }` — region-extrude faces
///   along their averaged normal; old faces tombstoned, new faces
///   appended at the offset position with side quads on boundary edges
/// - `DeleteFaces { face_ids }` — tombstone faces (vertices left
///   live; lazy invalidation in render and describe)
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

    /// Rotate each named vertex around the axis through `pivot` by
    /// `angle` radians. `axis` is normalized internally; a zero-length
    /// axis is a no-op. Out-of-range and tombstoned ids skipped.
    ///
    /// # Agent
    /// To bend an extruded ring, rotate its vertices around an axis
    /// perpendicular to both the ring's tangent and the bend
    /// direction. Common case: bending around world `z` to curve
    /// geometry along the y axis — `axis: [0, 0, 1]`, pivot at the
    /// hinge point.
    #[handler]
    fn on_rotate_vertices(&mut self, _ctx: &mut Ctx<'_>, msg: RotateVertices) {
        let axis = Vec3::new(msg.axis[0], msg.axis[1], msg.axis[2]).normalize();
        if axis.length_squared() == 0.0 {
            return;
        }
        let pivot = Vec3::new(msg.pivot[0], msg.pivot[1], msg.pivot[2]);
        let q = Quat::from_axis_angle(axis, msg.angle);
        let mut touched = false;
        for id in &msg.vertex_ids {
            if let Some(slot) = self.vertices.get_mut(*id as usize)
                && let Some(v) = slot.as_mut()
            {
                *v = pivot + q.rotate_vec3(*v - pivot);
                touched = true;
            }
        }
        if touched {
            self.rebuild_render_cache();
        }
    }

    /// Region-extrude the listed faces along their averaged normal
    /// by `distance`. The input faces become the bottom of the
    /// extrusion (tombstoned), each input face gets a corresponding
    /// new face at the offset position with the original color, and
    /// boundary edges (edges in exactly one input face) get side
    /// quads stitching old to new. Interior edges (shared between
    /// two input faces) get no side quad.
    ///
    /// # Agent
    /// Send `describe` afterward to learn the new vertex/face ids.
    /// New ids are appended monotonically: vertices first (one per
    /// unique input vertex, in ascending input id order), then top
    /// faces (one per input face, in input order), then side-quad
    /// pairs in input-face / edge-walk order.
    #[handler]
    fn on_extrude_face(&mut self, _ctx: &mut Ctx<'_>, msg: ExtrudeFace) {
        self.extrude_faces(&msg.face_ids, msg.distance);
    }

    /// Tombstone the listed face ids. Vertices stay live (lazy
    /// invalidation; the agent can clean up unused vertices later).
    /// Out-of-range and already-tombstoned ids skipped.
    ///
    /// # Agent
    /// Use to hollow a region — e.g., delete the top cap of a
    /// cylinder before extruding the rim inward.
    #[handler]
    fn on_delete_faces(&mut self, _ctx: &mut Ctx<'_>, msg: DeleteFaces) {
        let mut touched = false;
        for id in &msg.face_ids {
            if let Some(slot) = self.faces.get_mut(*id as usize)
                && slot.is_some()
            {
                *slot = None;
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
    /// Region-extrude implementation. See the `ExtrudeFace` kind doc
    /// for semantics; see `on_extrude_face` for the handler that
    /// dispatches into this.
    fn extrude_faces(&mut self, face_ids: &[u32], distance: f32) {
        // 1. Filter to live, in-range face ids; snapshot their data.
        struct InputFace {
            verts: [u32; 3],
            color: [f32; 3],
        }
        let mut inputs: Vec<InputFace> = Vec::new();
        for &fid in face_ids {
            if let Some(Some(face)) = self.faces.get(fid as usize) {
                inputs.push(InputFace {
                    verts: face.vertices,
                    color: face.color,
                });
            }
        }
        if inputs.is_empty() {
            return;
        }

        // 2. Compute averaged normal across input faces. Skip faces
        //    whose normal is degenerate (zero-area triangle).
        let mut normal_sum = Vec3::ZERO;
        for f in &inputs {
            let (Some(Some(va)), Some(Some(vb)), Some(Some(vc))) = (
                self.vertices.get(f.verts[0] as usize),
                self.vertices.get(f.verts[1] as usize),
                self.vertices.get(f.verts[2] as usize),
            ) else {
                continue;
            };
            let n = (*vb - *va).cross(*vc - *va).normalize();
            normal_sum += n;
        }
        let avg = normal_sum.normalize();
        if avg.length_squared() == 0.0 {
            return;
        }
        let offset = avg * distance;

        // 3. Collect unique input vertex ids; duplicate each at
        //    `original + offset`, recording the old->new id map.
        let mut input_vertex_ids: BTreeSet<u32> = BTreeSet::new();
        for f in &inputs {
            for &v in &f.verts {
                input_vertex_ids.insert(v);
            }
        }
        let mut old_to_new: BTreeMap<u32, u32> = BTreeMap::new();
        for &vid in &input_vertex_ids {
            let Some(Some(orig)) = self.vertices.get(vid as usize) else {
                continue;
            };
            let new_pos = *orig + offset;
            let new_id = self.vertices.len() as u32;
            self.vertices.push(Some(new_pos));
            old_to_new.insert(vid, new_id);
        }

        // 4. Count canonical edge appearances across input faces.
        //    Boundary == count == 1.
        let mut edge_count: BTreeMap<(u32, u32), u32> = BTreeMap::new();
        for f in &inputs {
            for &(a, b) in &[
                (f.verts[0], f.verts[1]),
                (f.verts[1], f.verts[2]),
                (f.verts[2], f.verts[0]),
            ] {
                let key = if a < b { (a, b) } else { (b, a) };
                *edge_count.entry(key).or_insert(0) += 1;
            }
        }

        // 5. Tombstone the input faces in-place (preserves their ids
        //    as permanently-gone) and emit one new top face per input
        //    using the new (offset) vertex ids.
        for &fid in face_ids {
            if let Some(slot) = self.faces.get_mut(fid as usize)
                && slot.is_some()
            {
                *slot = None;
            }
        }
        for f in &inputs {
            let Some(&a) = old_to_new.get(&f.verts[0]) else {
                continue;
            };
            let Some(&b) = old_to_new.get(&f.verts[1]) else {
                continue;
            };
            let Some(&c) = old_to_new.get(&f.verts[2]) else {
                continue;
            };
            self.faces.push(Some(Face {
                vertices: [a, b, c],
                color: f.color,
            }));
        }

        // 6. Side quads on boundary edges. Walk each input face's
        //    edges in directed order; for each edge in the boundary
        //    set, emit two triangles bridging the original edge to
        //    its new copy, CCW from outside (consistent with the
        //    host face's winding).
        for f in &inputs {
            for &(a, b) in &[
                (f.verts[0], f.verts[1]),
                (f.verts[1], f.verts[2]),
                (f.verts[2], f.verts[0]),
            ] {
                let key = if a < b { (a, b) } else { (b, a) };
                if edge_count.get(&key).copied() != Some(1) {
                    continue;
                }
                let (Some(&a_new), Some(&b_new)) = (old_to_new.get(&a), old_to_new.get(&b)) else {
                    continue;
                };
                self.faces.push(Some(Face {
                    vertices: [a, b, b_new],
                    color: f.color,
                }));
                self.faces.push(Some(Face {
                    vertices: [a, b_new, a_new],
                    color: f.color,
                }));
            }
        }

        self.rebuild_render_cache();
    }

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

    // Faces wound CCW from outside, so each triangle's
    // (b - a) × (c - a) cross product points outward. extrude_face
    // depends on this convention to push faces away from the body
    // when distance is positive. Render-side culling is off today
    // (PR-A merged with culling-off intact), but the math here is
    // what extrude / future culling work will rely on.
    faces.clear();
    faces.extend([
        Some(Face {
            vertices: [0, 1, 2],
            color: BOTTOM,
        }),
        Some(Face {
            vertices: [0, 2, 3],
            color: BOTTOM,
        }),
        Some(Face {
            vertices: [4, 6, 5],
            color: TOP,
        }),
        Some(Face {
            vertices: [4, 7, 6],
            color: TOP,
        }),
        Some(Face {
            vertices: [3, 2, 6],
            color: FRONT,
        }),
        Some(Face {
            vertices: [3, 6, 7],
            color: FRONT,
        }),
        Some(Face {
            vertices: [0, 4, 5],
            color: BACK,
        }),
        Some(Face {
            vertices: [0, 5, 1],
            color: BACK,
        }),
        Some(Face {
            vertices: [0, 3, 7],
            color: LEFT,
        }),
        Some(Face {
            vertices: [0, 7, 4],
            color: LEFT,
        }),
        Some(Face {
            vertices: [1, 5, 6],
            color: RIGHT,
        }),
        Some(Face {
            vertices: [1, 6, 2],
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

    // All faces wound CCW from outside (radially outward for the
    // side wall, +y for the top cap, -y for the bottom cap), so
    // each triangle's (b - a) × (c - a) cross product points outward.
    // Same convention as build_cube — extrude_face depends on it.

    // Side wall.
    for i in 0..n {
        let next = (i + 1) % n;
        let bot_a = i;
        let bot_b = next;
        let top_a = n + i;
        let top_b = n + next;
        let color = if i % 2 == 0 { SIDE_A } else { SIDE_B };
        faces.push(Some(Face {
            vertices: [bot_a, top_a, top_b],
            color,
        }));
        faces.push(Some(Face {
            vertices: [bot_a, top_b, bot_b],
            color,
        }));
    }

    // Top cap (fan from top_center, outward = +y).
    for i in 0..n {
        let next = (i + 1) % n;
        faces.push(Some(Face {
            vertices: [top_center, n + next, n + i],
            color: TOP_CAP,
        }));
    }

    // Bottom cap (fan from bottom_center, outward = -y).
    for i in 0..n {
        let next = (i + 1) % n;
        faces.push(Some(Face {
            vertices: [bottom_center, i, next],
            color: BOTTOM_CAP,
        }));
    }
}

aether_component::export!(MeshEditor);
