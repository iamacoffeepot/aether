//! Mesh viewer runtime. Loads a mesh file from the substrate's I/O
//! surface (ADR-0041), parses it into `DrawTriangle`s, and replays the
//! cached list to the `"aether.render"` sink each frame on the `Render`
//! lifecycle stage.
//!
//! Dispatches on the file extension echoed back on `aether.fs.read_result`:
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
//! `aether.fs.write` and call `aether.mesh.load` instead) and the
//! `aether-static-mesh-component` (its `aether.static_mesh.load` kind
//! was renamed to `aether.mesh.load`).
//!
//! # Lifecycle
//!
//! 1. Send `aether.mesh.load { namespace, path }` pointing at a `.dsl`
//!    or `.obj` file inside one of the substrate's I/O namespaces
//!    (`save`, `assets`, `config`).
//! 2. The component fires `aether.fs.read` and waits for the reply.
//! 3. On reply, the cached triangle list is replaced atomically. Any
//!    parse or mesh failure leaves the prior cache intact (silent
//!    drop; errors surface via `engine_logs`).
//! 4. Every `aether.lifecycle.render` stage re-emits the cached
//!    triangles to `"aether.render"`.

use aether_actor::{BootError, FfiActor, FfiCtx, OutboundReply, ReplyTo, Resolver, actor};
use aether_capabilities::fs::FsMailboxExt;
use aether_capabilities::lifecycle::LifecycleMailboxExt;
use aether_capabilities::{FsCapability, LifecycleCapability, RenderCapability};
use aether_data::{Kind, MailboxId};
use aether_kinds::{DrawTriangle, MeshLoadResult, ReadResult, Render, Vertex};
use aether_math::Vec3;
use aether_mesh::{Point3, Polygon, tessellate_polygon};

use crate::LoadMesh;
use core::str;

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
    /// Reply target of the most recent `aether.mesh.load` request,
    /// parked across the async `aether.fs.read` round-trip (issue 964).
    /// `on_load` runs in the requester's reply context; the actual
    /// parse + cache replace happens later in `on_read_result`, whose
    /// reply context points at `FsCapability`, not the original
    /// requester. Stashing the handle here lets the `MeshLoadResult`
    /// route back to whoever sent the `LoadMesh` (the
    /// parked-sender pattern; the handle stays valid for the instance
    /// lifetime per the SDK `ReplyTo` contract). `None` when the load
    /// was fire-and-forget (no reply target) or when no load is in
    /// flight.
    pending_reply: Option<ReplyTo>,
}

/// Mesh viewer component.
///
/// # Agent
/// Workflow: `load_component` this binary, then send
/// `aether.mesh.load { namespace, path }` pointing at a `.dsl` or
/// `.obj` file. After the substrate's read reply comes back the mesh
/// renders every frame; `capture_frame` verifies. Send another `load`
/// to swap the cached mesh. Iterate on a DSL by writing the new source
/// via `aether.fs.write` and re-sending `aether.mesh.load` against the
/// same path.
#[actor]
impl FfiActor for MeshViewer {
    const NAMESPACE: &'static str = "mesh_viewer";

    fn init<C>(_ctx: &mut C) -> Result<Self, BootError>
    where
        C: Resolver,
    {
        Ok(MeshViewer {
            triangles: Vec::new(),
            pending_reply: None,
        })
    }

    //noinspection DuplicatedCode
    /// Issue 640 / 1378: subscribe to the `Render` lifecycle stage so the
    /// cached triangles re-emit once per frame, after the `Tick` chain
    /// has settled (ADR-0082 §11). The viewer has no per-tick compute —
    /// it only re-emits — so it subscribes `Render` alone, not `Tick`.
    /// Lives in `wire` (post-init, mail-allowed); `init` is
    /// `Resolver`-only.
    ///
    /// On a chassis whose lifecycle graph omits `Render` (headless), the
    /// cap replies `Err(UnsupportedStage)` to this fire-and-forget
    /// subscribe; the reply warn-drops and the viewer simply never
    /// receives `Render` and never submits — a no-op there, where the
    /// render cap discards anyway (ADR-0082 §7 / §11).
    fn wire(&mut self, ctx: &mut FfiCtx<'_>) {
        ctx.actor::<LifecycleCapability>()
            .subscribe(Render::ID, MailboxId(ctx.mailbox_id()));
    }

    /// Re-emits every cached triangle to the render sink on the `Render`
    /// stage.
    ///
    /// # Agent
    /// Substrate-driven; do not send manually. If no triangles render
    /// after a `load`, the file failed to read / parse / mesh — check
    /// `engine_logs`.
    #[handler]
    fn on_render(&mut self, ctx: &mut FfiCtx<'_>, _render: Render) {
        if !self.triangles.is_empty() {
            ctx.actor::<RenderCapability>().send_many(&self.triangles);
        }
    }

    /// Triggers an asynchronous mesh load. Reply arrives as
    /// `aether.fs.read_result`; the parser is picked from the file
    /// extension at that point. The `aether.mesh.load_result` reply to
    /// the originator (issue 964) fires once the read settles and the
    /// parse / mesh outcome is known — see `on_read_result`.
    ///
    /// # Agent
    /// `namespace` is the short prefix with no `://` — `"save"`,
    /// `"assets"`, `"config"`. `path` is relative to the namespace
    /// root and must end in `.dsl` or `.obj`. Send-and-await the
    /// `aether.mesh.load_result` reply to learn whether the load
    /// succeeded (`ok`) and why it didn't (`error`).
    // `msg: LoadMesh` matches the dispatch ABI (ADR-0033 / ADR-0038);
    // the load body delegates straight to `FsCapability` via `ctx`.
    #[allow(clippy::needless_pass_by_value)]
    #[handler]
    fn on_load(&mut self, ctx: &mut FfiCtx<'_>, msg: LoadMesh) {
        // Park the requester's reply target across the async read.
        // `on_read_result` answers it with the structured outcome.
        // Overwriting any prior pending handle is intentional —
        // loads are serialized through one read round-trip, and a
        // fresh load supersedes an unanswered prior one.
        self.pending_reply = ctx.reply_target();
        tracing::info!(
            target: "aether_mesh_viewer",
            namespace = %msg.namespace,
            path = %msg.path,
            "load requested; issuing read",
        );
        ctx.actor::<FsCapability>().read(&msg.namespace, &msg.path);
    }

    /// Consumes the substrate's I/O reply. Dispatches on the echoed
    /// `path`'s extension and replaces the cached triangle list on
    /// success. Any failure (read error, non-utf8, parse error,
    /// unknown extension) leaves the previous cache intact, with a
    /// warn log explaining the failure. Issue 964: after computing the
    /// outcome, replies `aether.mesh.load_result` to the originator of
    /// the `aether.mesh.load` request (parked in `on_load`), echoing
    /// the request's `namespace` + `path` and carrying the structured
    /// `ok` / `error` verdict so a scenario harness or MCP `send_mail`
    /// caller has a wire signal instead of having to scrape
    /// `engine_logs`.
    ///
    /// # Agent
    /// Substrate-driven; do not send manually.
    #[handler]
    fn on_read_result(&mut self, ctx: &mut FfiCtx<'_>, r: ReadResult) {
        let (namespace, path, outcome) = match r {
            ReadResult::Ok {
                namespace,
                path,
                bytes,
            } => {
                let outcome = self.load_bytes(&path, &bytes);
                (namespace, path, outcome)
            }
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
                let outcome = LoadOutcome::failed(format!("read failed: {error:?}"));
                (namespace, path, outcome)
            }
        };
        self.reply_load_result(ctx, namespace, path, outcome);
    }
}

/// The result of a single load attempt, decoupled from where the bytes
/// came from. `on_read_result` builds one of these, then turns it into
/// the wire `MeshLoadResult` reply (issue 964). A failed load reports
/// `error: Some(_)` and leaves the cache untouched; a succeeded load
/// reports `error: None` and may carry non-fatal `warnings` (none are
/// produced today — diagnostic content is a sibling issue — but the
/// shape is plumbed so it rides along once the content lands).
struct LoadOutcome {
    error: Option<String>,
    warnings: Vec<String>,
}

impl LoadOutcome {
    fn ok() -> Self {
        Self {
            error: None,
            warnings: Vec::new(),
        }
    }

    fn failed(error: String) -> Self {
        Self {
            error: Some(error),
            warnings: Vec::new(),
        }
    }
}

impl MeshViewer {
    /// Parse `bytes` for `path`, replacing the cached triangle list on
    /// success and leaving it intact on any failure. Returns the
    /// structured outcome for the `MeshLoadResult` reply.
    fn load_bytes(&mut self, path: &str, bytes: &[u8]) -> LoadOutcome {
        let Ok(text) = str::from_utf8(bytes) else {
            tracing::warn!(
                target: "aether_mesh_viewer",
                path = %path,
                "mesh file is not valid UTF-8; keeping prior mesh",
            );
            return LoadOutcome::failed("mesh file is not valid UTF-8".to_string());
        };
        let lower = path.rsplit('.').next().map(str::to_ascii_lowercase);
        if lower.as_deref() == Some("dsl") {
            self.try_replace_dsl(text)
        } else if lower.as_deref() == Some("obj") {
            self.try_replace_obj(text)
        } else {
            tracing::warn!(
                target: "aether_mesh_viewer",
                path = %path,
                "unsupported file extension; expected .dsl or .obj",
            );
            LoadOutcome::failed("unsupported file extension; expected .dsl or .obj".to_string())
        }
    }

    /// Build and dispatch the `aether.mesh.load_result` reply to the
    /// parked requester. No-op when no reply target was parked (the
    /// load was fire-and-forget). Clears the parked handle either way
    /// so a stale target can't leak into a later load's reply.
    fn reply_load_result(
        &mut self,
        ctx: &mut FfiCtx<'_>,
        namespace: String,
        path: String,
        outcome: LoadOutcome,
    ) {
        if let Some(sender) = self.pending_reply.take() {
            ctx.reply_to(
                sender,
                &MeshLoadResult {
                    ok: outcome.error.is_none(),
                    namespace,
                    path,
                    error: outcome.error,
                    warnings: outcome.warnings,
                },
            );
        }
    }

    fn try_replace_dsl(&mut self, dsl: &str) -> LoadOutcome {
        let ast = match aether_mesh::parse(dsl) {
            Ok(ast) => ast,
            Err(error) => {
                tracing::warn!(
                    target: "aether_mesh_viewer",
                    error = %error,
                    "DSL parse failed; keeping prior mesh",
                );
                return LoadOutcome::failed(format!("DSL parse failed: {error}"));
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
                return LoadOutcome::failed(format!("DSL mesh build failed: {error}"));
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
        LoadOutcome::ok()
    }

    fn try_replace_obj(&mut self, obj: &str) -> LoadOutcome {
        match parse_obj(obj) {
            Ok(tris) => {
                tracing::info!(
                    target: "aether_mesh_viewer",
                    triangles = tris.len(),
                    "OBJ load complete; cache replaced",
                );
                self.triangles = tris;
                LoadOutcome::ok()
            }
            Err(error) => {
                tracing::warn!(
                    target: "aether_mesh_viewer",
                    error = ?error,
                    "OBJ parse failed; keeping prior mesh",
                );
                LoadOutcome::failed(format!("OBJ parse failed: {error:?}"))
            }
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
        let Some(head) = parts.next() else {
            continue;
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
        let tris = parse_obj(obj).expect("test setup: well-formed OBJ parses");
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
        let tris = parse_obj(obj).expect("test setup: quad OBJ parses");
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
        let tris =
            parse_obj(obj).expect("test setup: OBJ with unknown directives still parses faces");
        assert_eq!(tris.len(), 1);
    }

    #[test]
    fn handles_face_refs_with_slashes() {
        let obj = "\
            v 0 0 0\n\
            v 1 0 0\n\
            v 1 1 0\n\
            f 1/1/1 2/2/1 3/3/1\n";
        let tris = parse_obj(obj).expect("test setup: OBJ with v/vt/vn refs parses");
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
