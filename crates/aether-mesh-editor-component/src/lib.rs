//! DSL mesh editor component (ADR-0052). Accepts a `.dsl` source per
//! ADR-0026 + ADR-0051, parses + meshes it via `aether-dsl-mesh`, and
//! replays the cached triangle list to the `"render"` sink every tick.
//! Hot reload is by-replacement: each `SetText` (or successful
//! `SetPath`-driven I/O reply) drops the prior cache and installs the
//! new triangles atomically — partial parse or mesh failures keep the
//! previous mesh visible.
//!
//! Per ADR-0057, the canonical mesh form returned by `aether-dsl-mesh`
//! is now `Vec<Polygon>` (n-gons with holes); this component
//! tessellates each polygon to triangles at emit time via
//! `tessellate_polygon` so the upload path stays triangle-based but
//! the source-of-truth is the n-gon.
//!
//! Supersedes the Spike C vertex/face stateful editor. The
//! `aether.mesh.set_primitive` / `translate_vertices` / `scale_vertices`
//! / `rotate_vertices` / `extrude_face` / `delete_faces` / `describe`
//! mail kinds were removed in the same PR; agents now edit DSL text
//! and re-send it.
//!
//! # Workflow
//!
//! 1. `load_component` this binary.
//! 2. Send either:
//!    - `aether.dsl_mesh.set_text { dsl }` with the source inline, or
//!    - `aether.dsl_mesh.set_path { namespace, path }` to load from
//!      the substrate's I/O surface (ADR-0041).
//! 3. The editor parses, meshes, and caches the triangles. The next
//!    tick (and every tick after) re-emits them to `"render"`.
//! 4. To iterate, modify the DSL text and re-send `set_text` (or
//!    re-write the file and re-send `set_path`).

use aether_component::{Component, Ctx, InitCtx, Sink, handlers, io};
use aether_dsl_mesh::{Polygon, tessellate_polygon};
use aether_kinds::{DrawTriangle, ReadResult, SetPath, SetText, Tick, Vertex};
use aether_math::Vec3;

/// Outline edges are emitted as thin in-plane quads. Width is in world
/// units; matches the box/sphere scale we typically demo against
/// (~0.5 to 3 unit primitives).
const OUTLINE_WIDTH: f32 = 0.012;

/// Lift outlines slightly along the polygon's plane normal so they
/// don't z-fight with the filled triangles underneath.
const OUTLINE_LIFT: f32 = 0.002;

/// Outline color. Hardcoded slate (matches PALETTE[7]) for "DCC mode"
/// readability against any fill color. Not a DSL color — outlines are
/// a render decoration, not part of the source mesh.
const OUTLINE_RGB: (f32, f32, f32) = (0.12, 0.12, 0.16);

/// Built-in palette mapping DSL `:color N` indices to RGB. The DSL's
/// color is a `u32` palette reference; the substrate renderer needs
/// floating-point RGB. Indices wrap modulo `PALETTE.len()` so any
/// non-negative integer is a valid color.
const PALETTE: &[(f32, f32, f32)] = &[
    (0.55, 0.70, 0.92), // 0 — soft blue (default)
    (0.85, 0.40, 0.30), // 1 — terracotta
    (0.45, 0.75, 0.45), // 2 — sage green
    (0.95, 0.85, 0.40), // 3 — mustard
    (0.80, 0.55, 0.85), // 4 — lilac
    (0.65, 0.50, 0.35), // 5 — wood brown
    (0.95, 0.95, 0.95), // 6 — white
    (0.30, 0.30, 0.35), // 7 — slate
];

pub struct DslMeshEditor {
    triangles: Vec<DrawTriangle>,
    render: Sink<DrawTriangle>,
}

/// DSL mesh editor component.
///
/// # Agent
/// Send `aether.dsl_mesh.set_text { dsl: "(box 1 1 1 :color 0)" }` for
/// inline DSL, or `aether.dsl_mesh.set_path { namespace: "assets",
/// path: "teapot.dsl" }` to load from disk. Iterate by re-sending
/// `set_text` with the modified source — the editor swaps the mesh
/// atomically and the next frame reflects the change. Parse / mesh
/// failures silently retain the previous cache; check `engine_logs`
/// or capture a frame to confirm the new mesh rendered.
#[handlers]
impl Component for DslMeshEditor {
    fn init(ctx: &mut InitCtx<'_>) -> Self {
        DslMeshEditor {
            triangles: Vec::new(),
            render: ctx.resolve_sink::<DrawTriangle>("render"),
        }
    }

    /// Re-emits every cached triangle to the render sink.
    ///
    /// # Agent
    /// Substrate-driven; do not send manually. If no triangles render
    /// after a `set_text`, the source failed to parse or mesh — the
    /// cache stayed empty (or kept its previous contents).
    #[handler]
    fn on_tick(&mut self, ctx: &mut Ctx<'_>, _tick: Tick) {
        if !self.triangles.is_empty() {
            ctx.send_many(&self.render, &self.triangles);
        }
    }

    /// Parse + mesh the supplied DSL text inline; on success replace
    /// the cached triangles wholesale. On parse or mesh failure the
    /// previous cache stays intact (silent drop).
    ///
    /// # Agent
    /// The full DSL grammar is documented in ADR-0026 and ADR-0051;
    /// `crates/aether-dsl-mesh/examples/` has worked examples
    /// (box.dsl, lamp_post.dsl, teapot.dsl).
    #[handler]
    fn on_set_text(&mut self, _ctx: &mut Ctx<'_>, msg: SetText) {
        self.try_replace(&msg.dsl);
    }

    /// Issue an `aether.io.read` to the substrate for `namespace://path`.
    /// The reply lands on `on_read_result` and triggers the same
    /// parse-mesh-replace path as `set_text`.
    ///
    /// # Agent
    /// `namespace` is the short prefix (no `://`) — `"save"`,
    /// `"assets"`, `"config"`. `path` is relative to the namespace
    /// root; `..` and absolute prefixes are rejected by the substrate.
    #[handler]
    fn on_set_path(&mut self, _ctx: &mut Ctx<'_>, msg: SetPath) {
        io::read(&msg.namespace, &msg.path);
    }

    /// Consume the substrate's I/O reply for a prior `set_path`. On
    /// success, decode the bytes as UTF-8 DSL text and run the same
    /// parse-mesh-replace path. On any failure (I/O error, non-utf8,
    /// parse error, mesh error) the previous cache is retained.
    ///
    /// # Agent
    /// Substrate-driven; do not send manually. If a `set_path` doesn't
    /// take effect, the I/O error surfaces in `engine_logs`.
    #[handler]
    fn on_read_result(&mut self, _ctx: &mut Ctx<'_>, r: ReadResult) {
        if let ReadResult::Ok { bytes, .. } = r
            && let Ok(text) = core::str::from_utf8(&bytes)
        {
            self.try_replace(text);
        }
    }
}

impl DslMeshEditor {
    /// Parse DSL text and (on success) replace the cached triangle
    /// list. Atomic: failures leave the prior cache untouched, so a
    /// bad reload doesn't blank the render.
    ///
    /// Per ADR-0057, the source mesh form is `Vec<Polygon>` (n-gons);
    /// we tessellate each polygon to triangles here at cache time so
    /// the per-tick render path stays cheap (one cached triangle list,
    /// no re-tessellation per frame).
    fn try_replace(&mut self, dsl: &str) {
        let Ok(ast) = aether_dsl_mesh::parse(dsl) else {
            return;
        };
        let Ok(polygons) = aether_dsl_mesh::mesh_polygons(&ast) else {
            return;
        };
        let mut out = Vec::new();
        for polygon in &polygons {
            // Filled face triangles.
            for tri in tessellate_polygon(polygon) {
                out.push(to_draw_triangle_palette(tri, polygon.color));
            }
            // Polygon-edge outlines (per ADR-0057's "polygon-edge wireframe"
            // — show the n-gon boundary, never the tessellator's diagonals).
            for tri in polygon_outline_triangles(polygon) {
                out.push(to_draw_triangle_rgb(tri, OUTLINE_RGB));
            }
        }
        self.triangles = out;
    }
}

/// Generate thin in-plane outline quads for every outer + hole edge of
/// a polygon. Each edge becomes a 2-triangle strip of width
/// [`OUTLINE_WIDTH`], lifted [`OUTLINE_LIFT`] units along the plane
/// normal so it sits cleanly above the filled face. Returns flat
/// triangles (no internal grouping) ready for `DrawTriangle` emission.
///
/// World-space thickness — outlines stay the same size in world units
/// regardless of camera distance. They look thinner edge-on, which is
/// the right behavior for face boundaries (a face viewed edge-on is
/// itself a line).
fn polygon_outline_triangles(polygon: &Polygon) -> Vec<[Vec3; 3]> {
    let mut tris = Vec::new();
    let n = polygon.plane_normal;
    outline_loop(&polygon.vertices, n, &mut tris);
    for hole in &polygon.holes {
        outline_loop(hole, n, &mut tris);
    }
    tris
}

fn outline_loop(loop_: &[Vec3], n: Vec3, out: &mut Vec<[Vec3; 3]>) {
    let count = loop_.len();
    if count < 2 {
        return;
    }
    let lift = n * OUTLINE_LIFT;
    for i in 0..count {
        let v0 = loop_[i];
        let v1 = loop_[(i + 1) % count];
        let edge = v1 - v0;
        let perp = n.cross(edge);
        let perp_len = perp.length();
        if perp_len < 1e-6 {
            continue;
        }
        let off = perp * (OUTLINE_WIDTH * 0.5 / perp_len);
        // CCW around the plane normal (matches face winding) so culling
        // and lighting future-friendly.
        out.push([v0 + lift - off, v1 + lift - off, v1 + lift + off]);
        out.push([v0 + lift - off, v1 + lift + off, v0 + lift + off]);
    }
}

fn to_draw_triangle_palette(tri: [Vec3; 3], color: u32) -> DrawTriangle {
    let rgb = PALETTE[(color as usize) % PALETTE.len()];
    to_draw_triangle_rgb(tri, rgb)
}

fn to_draw_triangle_rgb(tri: [Vec3; 3], rgb: (f32, f32, f32)) -> DrawTriangle {
    let (r, g, b) = rgb;
    DrawTriangle {
        verts: [
            Vertex {
                x: tri[0].x,
                y: tri[0].y,
                z: tri[0].z,
                r,
                g,
                b,
            },
            Vertex {
                x: tri[1].x,
                y: tri[1].y,
                z: tri[1].z,
                r,
                g,
                b,
            },
            Vertex {
                x: tri[2].x,
                y: tri[2].y,
                z: tri[2].z,
                r,
                g,
                b,
            },
        ],
    }
}

aether_component::export!(DslMeshEditor);
