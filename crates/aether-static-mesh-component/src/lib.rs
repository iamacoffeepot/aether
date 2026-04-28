//! Static mesh viewer. Loads a Wavefront OBJ file from the substrate's
//! I/O surface (ADR-0041), parses it into `DrawTriangle`s, and replays
//! the cached list to the `"aether.sink.render"` sink every tick.
//!
//! Intended as a developer tool for inspecting `aether-mesh` output
//! (or any other OBJ-producing tool) end-to-end through the substrate's
//! render path. ADR-0026's no-import-for-production-content rule
//! targets *asset content*; this is a viewer, not an authoring path.
//!
//! # Lifecycle
//!
//! 1. Send `aether.static_mesh.load { namespace, path }` to the
//!    component. It fires an `aether.io.read` to the substrate's
//!    `"aether.sink.io"` sink.
//! 2. The substrate's I/O adapter resolves `namespace://path`, reads
//!    the bytes, and replies with `aether.io.read_result`.
//! 3. The component parses the OBJ text and caches the resulting
//!    triangle list. Any prior cache is dropped.
//! 4. On every `aether.tick` the cached triangles are re-emitted to
//!    `"aether.sink.render"` so the mesh stays visible.
//!
//! Errors (parse failures, missing files, adapter rejection) silently
//! drop the load. Visible symptom: no triangles render. Substrate-side
//! errors (NotFound, Forbidden) surface via `engine_logs`. The SDK now
//! exposes a `tracing`-based logging facility (ADR-0060) so per-
//! component warns can surface the parse-side too — left as a focused
//! follow-up rather than scoped into the ADR-0060 implementation PR.

use aether_component::{Component, Ctx, InitCtx, Sink, handlers, io};
use aether_kinds::{DrawTriangle, LoadStaticMesh, ReadResult, Tick, Vertex};

/// Default soft-blue color applied to every vertex of every loaded
/// triangle. OBJ doesn't carry per-face color in v1; per-group palette
/// support is parked.
const DEFAULT_COLOR: (f32, f32, f32) = (0.55, 0.7, 0.92);

pub struct StaticMesh {
    triangles: Vec<DrawTriangle>,
    render: Sink<DrawTriangle>,
}

/// Static-mesh viewer component.
///
/// # Agent
/// Workflow: `load_component` this binary, then send
/// `aether.static_mesh.load { namespace, path }` pointing at an OBJ
/// file inside one of the substrate's I/O namespaces (`save`, `assets`,
/// `config`). After the substrate's read reply comes back the mesh
/// renders every frame; capture_frame verifies. Send another `load`
/// to swap the cached mesh.
#[handlers]
impl Component for StaticMesh {
    fn init(ctx: &mut InitCtx<'_>) -> Self {
        StaticMesh {
            triangles: Vec::new(),
            render: ctx.resolve_sink::<DrawTriangle>("aether.sink.render"),
        }
    }

    /// Re-emits every cached triangle to the render sink.
    ///
    /// # Agent
    /// Substrate-driven; do not send manually. If the rendered mesh
    /// disappears the tick path stalled or the cache was cleared by
    /// a failing reload.
    #[handler]
    fn on_tick(&mut self, ctx: &mut Ctx<'_>, _tick: Tick) {
        for tri in &self.triangles {
            ctx.send(&self.render, tri);
        }
    }

    /// Triggers an asynchronous OBJ load. Reply arrives as
    /// `aether.io.read_result`.
    ///
    /// # Agent
    /// `namespace` is the short prefix with no `://` — `"save"`,
    /// `"assets"`, `"config"`. `path` is relative to the namespace
    /// root; `..` and absolute prefixes are forbidden by the adapter.
    #[handler]
    fn on_load(&mut self, _ctx: &mut Ctx<'_>, msg: LoadStaticMesh) {
        io::read(&msg.namespace, &msg.path);
    }

    /// Consumes the substrate's I/O reply. On success, parses the
    /// bytes as OBJ and replaces the cached triangle list. On failure
    /// or non-utf8 / parse-error bytes, silently leaves the previous
    /// cache intact.
    ///
    /// # Agent
    /// Not useful to send manually — the substrate emits this in
    /// response to the component's `aether.io.read` request.
    #[handler]
    fn on_read_result(&mut self, _ctx: &mut Ctx<'_>, r: ReadResult) {
        if let ReadResult::Ok { bytes, .. } = r
            && let Ok(text) = core::str::from_utf8(&bytes)
            && let Ok(tris) = parse_obj(text)
        {
            self.triangles = tris;
        }
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
    let (cr, cg, cb) = DEFAULT_COLOR;

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
            _ => {} // silently skip unknown directives
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

aether_component::export!(StaticMesh);
