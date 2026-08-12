//! Mesh viewer runtime. Loads a mesh file from the substrate's I/O
//! surface (ADR-0041), caches filled render triangles plus canonical DSL
//! outline loops, and submits them to the `"aether.render"` sink on the
//! `Render` lifecycle stage. Filled geometry is view-independent; DSL
//! outlines are rebuilt as eye-facing ribbons from the active camera eye.
//!
//! Dispatches on the file extension stashed in the fs request context:
//!
//! - `.dsl` → `aether-mesh`'s parser + mesher (ADR-0026 + ADR-0051).
//!   Filled triangles use the DSL's `:color N` palette indices; the
//!   polygon-edge loops are retained and rebuilt as lifted slate ribbons
//!   for the current eye on each render.
//! - `.obj` → `aether-mesh`'s indexed Wavefront importer (fan-style
//!   triangulation). OBJ doesn't carry per-face color, so triangles default
//!   to soft blue; no outline is emitted because the n-gon source is
//!   already tessellated by the time it arrives.
//!
//! This runtime supersedes the old `aether-mesh-editor-component`
//! (its inline `set_text` path is gone — write the DSL to a file via
//! `aether.fs.write` and call `aether.kit.mesh.load` instead) and the
//! `aether-static-mesh-component` (its `aether.static_mesh.load` kind
//! was renamed to `aether.kit.mesh.load`).
//!
//! # Lifecycle
//!
//! 1. Send `aether.kit.mesh.load { namespace, path }` pointing at a `.dsl`
//!    or `.obj` file inside one of the substrate's I/O namespaces
//!    (`save`, `assets`, `config`).
//! 2. The component fires `aether.fs.read` and waits for the reply.
//! 3. On reply, the filled-triangle and outline-loop cache is replaced
//!    atomically. Any parse or mesh failure leaves the prior cache intact.
//! 4. Every `aether.lifecycle.render` stage emits cached faces immediately.
//!    When DSL outline loops exist, the viewer asks the default-loaded
//!    `aether.kit.camera` component for its active eye and emits the solved
//!    outline triangles when the source-bound reply settles.

mod kinds;
pub use kinds::*;

use aether_actor::{ActorInitError, Manual, OutboundReply, ReplyHandle, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_component::ComponentHostCapability;
use aether_component::component::ComponentHostWasmExt;
use aether_fs::{FsCapability, FsMailboxExt, ReadResult};
use aether_kinds::{MeshLoadResult, Render};
use aether_lifecycle::LifecycleCapability;
use aether_lifecycle::LifecycleMailboxExt;
use aether_math::{Rgb, Vec3};
use aether_mesh::stroke::{self, StrokeParameters, StrokePoint};
use aether_mesh::{Point3, Polygon, tessellate_polygon};
use aether_render::{DrawTriangle, RenderCapability, Vertex};
use serde::{Deserialize, Serialize};

use crate::camera::{CameraComponent, CameraEyeRequest, CameraEyeResult};

use core::str;

const CAMERA_COMPONENT: &str = "aether.kit.camera";
const OUTLINE_ANGULAR_HALF_WIDTH_RADIANS: f32 = 0.002;
const OUTLINE_LIFT: f32 = 0.002;
const OUTLINE_SEED: u64 = 0x6d65_7368_2d6f_7574;
const OUTLINE_RGB: Rgb = Rgb::new(0.12, 0.12, 0.16);
const OUTLINE_PARAMETERS: StrokeParameters = StrokeParameters {
    angular_half_width: OUTLINE_ANGULAR_HALF_WIDTH_RADIANS,
    angular_wobble: 0.0,
    wobble_scale: 1.0,
    minimum_angular_length: 0.0,
};

const PALETTE: &[Rgb] = &[
    Rgb::new(0.55, 0.70, 0.92), // 0 — soft blue (default)
    Rgb::new(0.85, 0.40, 0.30), // 1 — terracotta
    Rgb::new(0.45, 0.75, 0.45), // 2 — sage green
    Rgb::new(0.95, 0.85, 0.40), // 3 — mustard
    Rgb::new(0.80, 0.55, 0.85), // 4 — lilac
    Rgb::new(0.65, 0.50, 0.35), // 5 — wood brown
    Rgb::new(0.95, 0.95, 0.95), // 6 — white
    Rgb::new(0.30, 0.30, 0.35), // 7 — slate
];

const OBJ_DEFAULT_COLOR: Rgb = PALETTE[0];

#[derive(Default)]
struct MeshCache {
    faces: Vec<DrawTriangle>,
    outlines: Vec<OutlineLoop>,
}

struct OutlineLoop {
    points: Vec<Vec3>,
    normal: Vec3,
}

pub struct MeshViewer {
    cache: MeshCache,
}

#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.mesh.load_context")]
struct MeshLoadContext {
    reply: Option<ReplyHandle>,
    namespace: String,
    path: String,
}

/// Mesh viewer component.
///
/// # Agent
/// Workflow: `load_component` this binary, then send
/// `aether.kit.mesh.load { namespace, path }` pointing at a `.dsl` or
/// `.obj` file. After the substrate's read reply comes back the mesh
/// renders every frame; `capture_frame` verifies. Send another `load`
/// to swap the cached mesh. Iterate on a DSL by writing the new source
/// via `aether.fs.write` and re-sending `aether.kit.mesh.load` against the
/// same path.
#[actor]
impl WasmActor for MeshViewer {
    const NAMESPACE: &'static str = "aether.kit.mesh";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(MeshViewer { cache: MeshCache::default() })
    }

    //noinspection DuplicatedCode
    /// Issue 640 / 1378: subscribe to the `Render` lifecycle stage so the
    /// cached triangles re-emit once per frame, after the `Tick` chain
    /// has settled (ADR-0082 §11). The viewer has no per-tick compute —
    /// it only re-emits — so it subscribes `Render` alone, not `Tick`.
    /// Lives in `wire` (post-init, mail-allowed); `init` has no send
    /// surface.
    ///
    /// On a chassis whose lifecycle graph omits `Render` (headless), the
    /// cap replies `Err(UnsupportedStage)` to this fire-and-forget
    /// subscribe; the reply warn-drops and the viewer simply never
    /// receives `Render` and never submits — a no-op there, where the
    /// render cap discards anyway (ADR-0082 §7 / §11).
    fn wire(&mut self, ctx: &mut aether_actor::WireCtx<'_, '_>) {
        ctx.actor::<LifecycleCapability>().subscribe::<Render>();
    }

    /// Emit cached faces and request the active eye when this DSL cache has
    /// outline loops. The reply stays in this settled Render cascade.
    ///
    /// # Agent
    /// Substrate-driven; do not send manually. If no triangles render
    /// after a `load`, the file failed to read / parse / mesh — check
    /// `engine_logs`.
    #[handler::single]
    fn on_render(&mut self, ctx: &mut WasmCtx<'_>, _render: Render) {
        if !self.cache.faces.is_empty() {
            ctx.actor::<RenderCapability>().send_many(&self.cache.faces);
        }
        if !self.cache.outlines.is_empty() {
            ctx.actor::<ComponentHostCapability>().loaded::<CameraComponent>(CAMERA_COMPONENT).send(&CameraEyeRequest);
        }
    }

    /// Rebuild and submit the cached DSL outline loops for the eye that
    /// answered this Render's request. No active camera means no outline.
    #[handler::single]
    fn on_camera_eye_result(&mut self, ctx: &mut WasmCtx<'_>, result: CameraEyeResult) {
        let Some(eye) = result.eye.map(Vec3::from_array) else {
            return;
        };
        let triangles = outline_triangles(&self.cache.outlines, eye);
        if !triangles.is_empty() {
            ctx.actor::<RenderCapability>().send_many(&triangles);
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
    #[allow(clippy::needless_pass_by_value, clippy::unused_self)]
    #[handler::single]
    fn on_load(&mut self, ctx: &mut WasmCtx<'_>, msg: LoadMesh) {
        let context = MeshLoadContext { reply: ctx.reply_target(), namespace: msg.namespace, path: msg.path };
        tracing::info!(
            target: "aether_kit_commons",
            namespace = %context.namespace,
            path = %context.path,
            "load requested; issuing read",
        );
        ctx.actor::<FsCapability>().with_context(&context).read(&context.namespace, &context.path);
    }

    /// Consumes the substrate's I/O reply. Dispatches on the request
    /// context path's extension and atomically replaces the split cache on
    /// success. Any failure (read error, invalid DSL UTF-8, malformed mesh,
    /// unknown extension) leaves the previous cache intact, with a warn log
    /// explaining the failure. Issue 964: after computing the
    /// outcome, replies `aether.mesh.load_result` to the originator of
    /// the `aether.kit.mesh.load` request (carried in the request context),
    /// echoing the request's `namespace` + `path` and carrying the structured
    /// `ok` / `error` verdict so a scenario harness or MCP `send_mail`
    /// caller has a wire signal instead of having to scrape
    /// `engine_logs`.
    ///
    /// # Agent
    /// Substrate-driven; do not send manually.
    #[handler::manual]
    fn on_read_result(&mut self, ctx: &mut WasmCtx<'_, Manual>, r: ReadResult) {
        let Some(context) = ctx.take_context::<MeshLoadContext>() else {
            return;
        };
        let outcome = match r {
            ReadResult::Ok { bytes, .. } => self.load_bytes(&context.path, &bytes),
            ReadResult::Err { error, .. } => {
                tracing::warn!(
                    target: "aether_kit_commons",
                    namespace = %context.namespace,
                    path = %context.path,
                    error = ?error,
                    "read failed; keeping prior datum",
                );
                LoadOutcome::failed(format!("read failed: {error:?}"))
            }
        };
        self.reply_load_result(ctx, context.reply, context.namespace, context.path, outcome);
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
        Self { error: None, warnings: Vec::new() }
    }

    fn failed(error: String) -> Self {
        Self { error: Some(error), warnings: Vec::new() }
    }
}

impl MeshViewer {
    /// Parse `bytes` for `path`, replacing the split mesh cache on success
    /// and leaving it intact on any failure. Returns the
    /// structured outcome for the `MeshLoadResult` reply.
    fn load_bytes(&mut self, path: &str, bytes: &[u8]) -> LoadOutcome {
        let lower = path.rsplit('.').next().map(str::to_ascii_lowercase);
        if lower.as_deref() == Some("dsl") {
            str::from_utf8(bytes).map_or_else(
                |_| {
                    tracing::warn!(
                        target: "aether_kit_commons",
                        path = %path,
                        "mesh file is not valid UTF-8; keeping prior mesh",
                    );
                    LoadOutcome::failed("mesh file is not valid UTF-8".to_string())
                },
                |text| self.try_replace_dsl(text),
            )
        } else if lower.as_deref() == Some("obj") {
            self.try_replace_obj(bytes)
        } else {
            tracing::warn!(
                target: "aether_kit_commons",
                path = %path,
                "unsupported file extension; expected .dsl or .obj",
            );
            LoadOutcome::failed("unsupported file extension; expected .dsl or .obj".to_string())
        }
    }

    /// Build and dispatch the `aether.mesh.load_result` reply to the
    /// requester carried in the fs request context. No-op when no reply
    /// target was carried (the load was fire-and-forget).
    #[allow(clippy::unused_self)]
    fn reply_load_result(
        &self,
        ctx: &mut WasmCtx<'_, Manual>,
        sender: Option<ReplyHandle>,
        namespace: String,
        path: String,
        outcome: LoadOutcome,
    ) {
        let Some(sender) = sender else {
            return;
        };
        let ok = outcome.error.is_none();
        ctx.reply_to(sender, &MeshLoadResult { ok, namespace, path, error: outcome.error, warnings: outcome.warnings });
    }

    fn try_replace_dsl(&mut self, dsl: &str) -> LoadOutcome {
        let ast = match aether_mesh::parse(dsl) {
            Ok(ast) => ast,
            Err(error) => {
                tracing::warn!(
                    target: "aether_kit_commons",
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
                    target: "aether_kit_commons",
                    error = %error,
                    "DSL mesh build failed; keeping prior mesh",
                );
                return LoadOutcome::failed(format!("DSL mesh build failed: {error}"));
            }
        };
        let mut cache = MeshCache::default();
        for polygon in &polygons {
            for tri in tessellate_polygon(polygon) {
                cache.faces.push(to_draw_triangle_palette(tri, polygon.color));
            }
            cache_outline_loops(polygon, &mut cache.outlines);
        }
        let outline_segments = cache.outlines.iter().map(|outline| outline.points.len()).sum::<usize>();
        tracing::info!(
            target: "aether_kit_commons",
            polygons = polygons.len(),
            face_triangles = cache.faces.len(),
            outline_segments,
            "DSL load complete; cache replaced",
        );
        self.cache = cache;
        LoadOutcome::ok()
    }

    fn try_replace_obj(&mut self, obj: &[u8]) -> LoadOutcome {
        match draw_obj(obj) {
            Ok(tris) => {
                tracing::info!(
                    target: "aether_kit_commons",
                    triangles = tris.len(),
                    "OBJ load complete; cache replaced",
                );
                self.cache = MeshCache { faces: tris, outlines: Vec::new() };
                LoadOutcome::ok()
            }
            Err(error) => {
                tracing::warn!(
                    target: "aether_kit_commons",
                    error = %error,
                    "OBJ parse failed; keeping prior mesh",
                );
                LoadOutcome::failed(format!("OBJ parse failed: {error}"))
            }
        }
    }
}

fn cache_outline_loops(polygon: &Polygon, outlines: &mut Vec<OutlineLoop>) {
    outlines.push(OutlineLoop {
        points: polygon.vertices.iter().map(|point| point.to_f32()).collect(),
        normal: polygon.plane_normal,
    });
    for hole in &polygon.holes {
        outlines.push(OutlineLoop {
            points: hole.iter().map(|point| point.to_f32()).collect(),
            normal: polygon.plane_normal,
        });
    }
}

fn outline_triangles(outlines: &[OutlineLoop], eye: Vec3) -> Vec<DrawTriangle> {
    outlines
        .iter()
        .flat_map(|outline| closed_outline_ribbon(outline, eye))
        .map(|triangle| to_draw_triangle_rgb(triangle, OUTLINE_RGB))
        .collect()
}

fn closed_outline_ribbon(outline: &OutlineLoop, eye: Vec3) -> Vec<[Vec3; 3]> {
    if outline.points.len() < 2 {
        return Vec::new();
    }

    let lift = outline.normal * OUTLINE_LIFT;
    let point = |position: Vec3| StrokePoint { pos: position + lift, weight: 1.0 };
    let mut points = Vec::with_capacity(outline.points.len() + 3);
    points.push(point(*outline.points.last().expect("non-empty outline")));
    points.extend(outline.points.iter().copied().map(point));
    points.push(point(outline.points[0]));
    points.push(point(outline.points[1]));

    let mut ribbon = stroke::ribbon(points, OUTLINE_PARAMETERS, eye, OUTLINE_SEED, 0);
    if ribbon.len() < 4 {
        return Vec::new();
    }
    ribbon.drain(..2);
    ribbon.truncate(ribbon.len() - 2);
    ribbon
}

fn to_draw_triangle_palette(tri: [Point3; 3], color: u32) -> DrawTriangle {
    let rgb = PALETTE[(color as usize) % PALETTE.len()];
    to_draw_triangle_rgb([tri[0].to_f32(), tri[1].to_f32(), tri[2].to_f32()], rgb)
}

fn to_draw_triangle_rgb(tri: [Vec3; 3], color: Rgb) -> DrawTriangle {
    DrawTriangle {
        verts: [
            Vertex { x: tri[0].x, y: tri[0].y, z: tri[0].z, color },
            Vertex { x: tri[1].x, y: tri[1].y, z: tri[1].z, color },
            Vertex { x: tri[2].x, y: tri[2].y, z: tri[2].z, color },
        ],
    }
}

/// Convert the renderer-independent indexed import at the upload boundary.
fn draw_obj(bytes: &[u8]) -> Result<Vec<DrawTriangle>, aether_mesh::ObjImportError> {
    let aether_mesh::IndexedMesh { positions, faces } = aether_mesh::parse_obj(bytes)?;

    Ok(faces
        .into_iter()
        .map(|face| to_draw_triangle_rgb(face.map(|index| positions[index as usize]), OBJ_DEFAULT_COLOR))
        .collect())
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
        let tris = draw_obj(obj.as_bytes()).expect("test setup: well-formed OBJ parses");
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
        let tris = draw_obj(obj.as_bytes()).expect("test setup: quad OBJ parses");
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
        let tris = draw_obj(obj.as_bytes()).expect("test setup: OBJ with unknown directives still parses faces");
        assert_eq!(tris.len(), 1);
    }

    #[test]
    fn handles_face_refs_with_slashes() {
        let obj = "\
            v 0 0 0\n\
            v 1 0 0\n\
            v 1 1 0\n\
            f 1/1/1 2/2/1 3/3/1\n";
        let tris = draw_obj(obj.as_bytes()).expect("test setup: OBJ with v/vt/vn refs parses");
        assert_eq!(tris.len(), 1);
    }

    #[test]
    fn rejects_out_of_range_index() {
        let obj = "\
            v 0 0 0\n\
            v 1 0 0\n\
            f 1 2 99\n";
        assert!(draw_obj(obj.as_bytes()).is_err());
    }

    fn outline(points: &[[f32; 3]], normal: [f32; 3]) -> OutlineLoop {
        OutlineLoop { points: points.iter().copied().map(Vec3::from_array).collect(), normal: Vec3::from_array(normal) }
    }

    #[test]
    fn closed_outline_emits_two_triangles_for_every_segment() {
        let square =
            outline(&[[-1.0, -1.0, 0.0], [1.0, -1.0, 0.0], [1.0, 1.0, 0.0], [-1.0, 1.0, 0.0]], [0.0, 0.0, 1.0]);

        let triangles = closed_outline_ribbon(&square, Vec3::new(0.0, 0.0, 4.0));

        assert_eq!(triangles.len(), square.points.len() * 2);
    }

    #[test]
    fn outer_and_hole_loops_are_both_rendered() {
        let outlines = [
            outline(&[[-2.0, -2.0, 0.0], [2.0, -2.0, 0.0], [2.0, 2.0, 0.0], [-2.0, 2.0, 0.0]], [0.0, 0.0, 1.0]),
            outline(&[[-0.5, -0.5, 0.0], [-0.5, 0.5, 0.0], [0.5, 0.5, 0.0], [0.5, -0.5, 0.0]], [0.0, 0.0, 1.0]),
        ];

        let triangles = outline_triangles(&outlines, Vec3::new(0.0, 0.0, 4.0));

        assert_eq!(triangles.len(), 16);
    }

    #[test]
    fn repeated_outline_solves_are_bit_identical() {
        let square =
            outline(&[[-1.0, -1.0, 0.0], [1.0, -1.0, 0.0], [1.0, 1.0, 0.0], [-1.0, 1.0, 0.0]], [0.0, 0.0, 1.0]);
        let eye = Vec3::new(1.5, -0.7, 5.0);

        let bits = |triangles: Vec<[Vec3; 3]>| {
            triangles
                .into_iter()
                .map(|triangle| triangle.map(|vertex| [vertex.x.to_bits(), vertex.y.to_bits(), vertex.z.to_bits()]))
                .collect::<Vec<_>>()
        };
        let first = bits(closed_outline_ribbon(&square, eye));
        let second = bits(closed_outline_ribbon(&square, eye));

        assert_eq!(first, second);
    }
}
