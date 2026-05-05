//! Mesh viewer runtime. Loads a mesh file from the substrate's I/O
//! surface (ADR-0041), parses it into `DrawTriangle`s, and replays the
//! cached list to the `"aether.render"` sink every tick.
//!
//! Dispatches on the file extension echoed back on `aether.io.read_result`:
//!
//! - `.dsl` → `aether-mesh`'s parser + mesher (ADR-0026 + ADR-0051).
//!   Filled triangles use the DSL's `:color N` palette indices; the
//!   polygon-edge outlines (slate) come along for free since the n-gon
//!   source is in hand at load time.
//! - `.obj` → minimal Wavefront parser (fan-style triangulation). OBJ
//!   doesn't carry per-face color, so triangles default to soft blue;
//!   no wireframe is emitted because the n-gon source is already
//!   tessellated by the time it arrives.
//!
//! This runtime supersedes the old `aether-mesh-editor-component`
//! (its inline `set_text` path is gone — write the DSL to a file via
//! `aether.io.write` and call `aether.mesh.load` instead) and the
//! `aether-static-mesh-component` (its `aether.static_mesh.load` kind
//! was renamed to `aether.mesh.load`).
//!
//! # Lifecycle
//!
//! 1. Send `aether.mesh.load { namespace, path }` pointing at a `.dsl`
//!    or `.obj` file inside one of the substrate's I/O namespaces
//!    (`save`, `assets`, `config`).
//! 2. The component fires `aether.io.read` and waits for the reply.
//! 3. On reply, the cached triangle list is replaced atomically. Any
//!    parse or mesh failure leaves the prior cache intact (silent
//!    drop; errors surface via `engine_logs`).
//! 4. Every `aether.tick` re-emits the cached triangles to
//!    `"aether.render"`.

use aether_actor::{BootError, Mailbox, WasmActor, WasmCtx, WasmInitCtx, actor, io};
use aether_kinds::{DrawTriangle, ReadResult, Tick, Vertex};
use aether_math::Vec3;
use aether_mesh::{Point3, Polygon, tessellate_polygon};

use crate::LoadMesh;

const OUTLINE_WIDTH: f32 = 0.012;
const OUTLINE_LIFT: f32 = 0.002;
const OUTLINE_RGB: (f32, f32, f32) = (0.12, 0.12, 0.16);

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

const OBJ_DEFAULT_COLOR: (f32, f32, f32) = PALETTE[0];

pub struct MeshViewer {
    triangles: Vec<DrawTriangle>,
    render: Mailbox<DrawTriangle>,
}

/// Mesh viewer component.
///
/// # Agent
/// Workflow: `load_component` this binary, then send
/// `aether.mesh.load { namespace, path }` pointing at a `.dsl` or
/// `.obj` file. After the substrate's read reply comes back the mesh
/// renders every frame; `capture_frame` verifies. Send another `load`
/// to swap the cached mesh. Iterate on a DSL by writing the new source
/// via `aether.io.write` and re-sending `aether.mesh.load` against the
/// same path.
#[actor]
impl WasmActor for MeshViewer {
    const NAMESPACE: &'static str = "mesh_viewer";

    fn init(ctx: &mut WasmInitCtx<'_>) -> Result<Self, BootError> {
        Ok(MeshViewer {
            triangles: Vec::new(),
            render: ctx.resolve_mailbox::<DrawTriangle>("aether.render"),
        })
    }

    /// Re-emits every cached triangle to the render sink.
    ///
    /// # Agent
    /// Substrate-driven; do not send manually. If no triangles render
    /// after a `load`, the file failed to read / parse / mesh — check
    /// `engine_logs`.
    #[handler]
    fn on_tick(&mut self, ctx: &mut WasmCtx<'_>, _tick: Tick) {
        if !self.triangles.is_empty() {
            ctx.send_many(&self.render, &self.triangles);
        }
    }

    /// Triggers an asynchronous mesh load. Reply arrives as
    /// `aether.io.read_result`; the parser is picked from the file
    /// extension at that point.
    ///
    /// # Agent
    /// `namespace` is the short prefix with no `://` — `"save"`,
    /// `"assets"`, `"config"`. `path` is relative to the namespace
    /// root and must end in `.dsl` or `.obj`.
    #[handler]
    fn on_load(&mut self, _ctx: &mut WasmCtx<'_>, msg: LoadMesh) {
        tracing::info!(
            target: "aether_mesh_viewer",
            namespace = %msg.namespace,
            path = %msg.path,
            "load requested; issuing read",
        );
        io::read(&msg.namespace, &msg.path);
    }

    /// Consumes the substrate's I/O reply. Dispatches on the echoed
    /// `path`'s extension and replaces the cached triangle list on
    /// success. Any failure (read error, non-utf8, parse error,
    /// unknown extension) leaves the previous cache intact, with a
    /// warn log explaining the failure.
    ///
    /// # Agent
    /// Substrate-driven; do not send manually.
    #[handler]
    fn on_read_result(&mut self, _ctx: &mut WasmCtx<'_>, r: ReadResult) {
        let (path, bytes) = match r {
            ReadResult::Ok { path, bytes, .. } => (path, bytes),
            ReadResult::Err {
                namespace,
                path,
                error,
            } => {
                tracing::warn!(
                    target: "aether_mesh_viewer",
                    namespace = %namespace,
                    path = %path,
                    error = ?error,
                    "read failed; keeping prior mesh",
                );
                return;
            }
        };
        let Ok(text) = core::str::from_utf8(&bytes) else {
            tracing::warn!(
                target: "aether_mesh_viewer",
                path = %path,
                "mesh file is not valid UTF-8; keeping prior mesh",
            );
            return;
        };
        if path.ends_with(".dsl") {
            self.try_replace_dsl(text);
        } else if path.ends_with(".obj") {
            self.try_replace_obj(text);
        } else {
            tracing::warn!(
                target: "aether_mesh_viewer",
                path = %path,
                "unsupported file extension; expected .dsl or .obj",
            );
        }
    }
}

impl MeshViewer {
    fn try_replace_dsl(&mut self, dsl: &str) {
        let ast = match aether_mesh::parse(dsl) {
            Ok(ast) => ast,
            Err(error) => {
                tracing::warn!(
                    target: "aether_mesh_viewer",
                    error = %error,
                    "DSL parse failed; keeping prior mesh",
                );
                return;
            }
        };
        let polygons = match aether_mesh::mesh_polygons(&ast) {
            Ok(p) => p,
            Err(error) => {
                tracing::warn!(
                    target: "aether_mesh_viewer",
                    error = %error,
                    "DSL mesh build failed; keeping prior mesh",
                );
                return;
            }
        };
        let mut out = Vec::new();
        for polygon in &polygons {
            for tri in tessellate_polygon(polygon) {
                out.push(to_draw_triangle_palette(tri, polygon.color));
            }
            for tri in polygon_outline_triangles(polygon) {
                out.push(to_draw_triangle_rgb(tri, OUTLINE_RGB));
            }
        }
        tracing::info!(
            target: "aether_mesh_viewer",
            polygons = polygons.len(),
            triangles = out.len(),
            "DSL load complete; cache replaced",
        );
        self.triangles = out;
    }

    fn try_replace_obj(&mut self, obj: &str) {
        match parse_obj(obj) {
            Ok(tris) => {
                tracing::info!(
                    target: "aether_mesh_viewer",
                    triangles = tris.len(),
                    "OBJ load complete; cache replaced",
                );
                self.triangles = tris;
            }
            Err(error) => tracing::warn!(
                target: "aether_mesh_viewer",
                error = ?error,
                "OBJ parse failed; keeping prior mesh",
            ),
        }
    }
}

fn polygon_outline_triangles(polygon: &Polygon) -> Vec<[Vec3; 3]> {
    let mut tris = Vec::new();
    let n = polygon.plane_normal;
    let outer_f32: Vec<Vec3> = polygon.vertices.iter().map(|p| p.to_f32()).collect();
    outline_loop(&outer_f32, n, &mut tris);
    for hole in &polygon.holes {
        let hole_f32: Vec<Vec3> = hole.iter().map(|p| p.to_f32()).collect();
        outline_loop(&hole_f32, n, &mut tris);
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
        out.push([v0 + lift - off, v1 + lift - off, v1 + lift + off]);
        out.push([v0 + lift - off, v1 + lift + off, v0 + lift + off]);
    }
}

fn to_draw_triangle_palette(tri: [Point3; 3], color: u32) -> DrawTriangle {
    let rgb = PALETTE[(color as usize) % PALETTE.len()];
    to_draw_triangle_rgb([tri[0].to_f32(), tri[1].to_f32(), tri[2].to_f32()], rgb)
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

#[derive(Debug)]
pub enum ObjParseError {
    VertexIndexOutOfRange { index: usize, defined: usize },
    DegenerateFace,
}

/// Minimal OBJ parser. Supports `v X Y Z` and `f V1 V2 V3 [V4 ...]`
/// (n-gons triangulated fan-style). Ignores normals (`vn`), texcoords
/// (`vt`), groups (`g`), materials (`mtllib`/`usemtl`), smoothing
/// (`s`), and comments (`#`). Face refs may be `v`, `v/vt`, `v//vn`,
/// or `v/vt/vn` — only the position index is used.
pub fn parse_obj(text: &str) -> Result<Vec<DrawTriangle>, ObjParseError> {
    let mut vertices: Vec<[f32; 3]> = Vec::new();
    let mut triangles: Vec<DrawTriangle> = Vec::new();
    let (cr, cg, cb) = OBJ_DEFAULT_COLOR;

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let mut parts = trimmed.split_whitespace();
        let head = match parts.next() {
            Some(h) => h,
            None => continue,
        };
        match head {
            "v" => {
                let x: f32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0.0);
                let y: f32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0.0);
                let z: f32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0.0);
                vertices.push([x, y, z]);
            }
            "f" => {
                let indices: Vec<usize> = parts
                    .filter_map(|tok| tok.split('/').next())
                    .filter_map(|n| n.parse::<usize>().ok())
                    .collect();
                if indices.len() < 3 {
                    return Err(ObjParseError::DegenerateFace);
                }
                for i in 1..indices.len() - 1 {
                    let a = obj_idx(indices[0], vertices.len())?;
                    let b = obj_idx(indices[i], vertices.len())?;
                    let c = obj_idx(indices[i + 1], vertices.len())?;
                    let va = vertices[a];
                    let vb = vertices[b];
                    let vc = vertices[c];
                    triangles.push(DrawTriangle {
                        verts: [
                            Vertex {
                                x: va[0],
                                y: va[1],
                                z: va[2],
                                r: cr,
                                g: cg,
                                b: cb,
                            },
                            Vertex {
                                x: vb[0],
                                y: vb[1],
                                z: vb[2],
                                r: cr,
                                g: cg,
                                b: cb,
                            },
                            Vertex {
                                x: vc[0],
                                y: vc[1],
                                z: vc[2],
                                r: cr,
                                g: cg,
                                b: cb,
                            },
                        ],
                    });
                }
            }
            _ => {}
        }
    }
    Ok(triangles)
}

fn obj_idx(one_based: usize, count: usize) -> Result<usize, ObjParseError> {
    if one_based == 0 || one_based > count {
        Err(ObjParseError::VertexIndexOutOfRange {
            index: one_based,
            defined: count,
        })
    } else {
        Ok(one_based - 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_box_obj() {
        let obj = "\
            v 0 0 0\n\
            v 1 0 0\n\
            v 1 1 0\n\
            v 0 1 0\n\
            f 1 2 3\n\
            f 1 3 4\n";
        let tris = parse_obj(obj).unwrap();
        assert_eq!(tris.len(), 2);
    }

    #[test]
    fn triangulates_quad_fan_style() {
        let obj = "\
            v 0 0 0\n\
            v 1 0 0\n\
            v 1 1 0\n\
            v 0 1 0\n\
            f 1 2 3 4\n";
        let tris = parse_obj(obj).unwrap();
        assert_eq!(tris.len(), 2, "quad should triangulate to 2 triangles");
    }

    #[test]
    fn ignores_unknown_directives() {
        let obj = "\
            # comment\n\
            mtllib foo.mtl\n\
            v 0 0 0\n\
            v 1 0 0\n\
            v 1 1 0\n\
            vn 0 0 1\n\
            usemtl bar\n\
            s off\n\
            g group_name\n\
            f 1 2 3\n";
        let tris = parse_obj(obj).unwrap();
        assert_eq!(tris.len(), 1);
    }

    #[test]
    fn handles_face_refs_with_slashes() {
        let obj = "\
            v 0 0 0\n\
            v 1 0 0\n\
            v 1 1 0\n\
            f 1/1/1 2/2/1 3/3/1\n";
        let tris = parse_obj(obj).unwrap();
        assert_eq!(tris.len(), 1);
    }

    #[test]
    fn rejects_out_of_range_index() {
        let obj = "\
            v 0 0 0\n\
            v 1 0 0\n\
            f 1 2 99\n";
        assert!(parse_obj(obj).is_err());
    }
}

aether_actor::export!(MeshViewer);
