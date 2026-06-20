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

use aether_actor::{
    BootError, Manual, OutboundReply, ReplyHandle, WasmActor, WasmCtx, WasmInitCtx, actor,
};
use aether_capabilities::fs::FsMailboxExt;
use aether_capabilities::lifecycle::LifecycleMailboxExt;
use aether_capabilities::{FsCapability, LifecycleCapability, RenderCapability};
use aether_data::Kind;
use aether_kinds::{
    DrawTriangle, MeshLoadResult, ReadResult, Render, TrajectorySampleEntry, Vertex,
};
use aether_labyrinth::{CorridorGraph, EdgeKind, ScalarField, TrajectorySet};
use aether_math::Vec3;
use aether_mesh::{Point3, Polygon, tessellate_polygon};

use crate::{CorridorLoadResult, LoadCorridor, LoadMesh, Scrub};
use alloc::collections::BTreeMap;
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

/// Iso threshold for the `.field` arm (issue 1868). Fixed at `1`: cost-0
/// cells classify as outside and every positive value — the `u32::MAX`
/// unreachable sentinel included — as inside, with no special case, so a
/// reachability field becomes a solid whose empty regions read as
/// tunnels through it.
const FIELD_ISO_THRESHOLD: u32 = 1;

/// World cell size for the `.field` arm: one world unit per grid step on
/// each axis, with time → world-z (the camera's depth convention). The
/// camera frames the result through `view_proj`; the placement is the
/// fixed unit convention, matching the DSL/OBJ paths' world units.
const FIELD_CELL: Vec3 = Vec3::splat(1.0);

/// World origin for the `.field` arm. The field's `(0, 0, tick 0)` corner
/// maps to the world origin (a half-cell shell extends just outside it).
const FIELD_ORIGIN: Vec3 = Vec3::ZERO;

/// Half-extent of a corridor node dot at unit `cell_count`, in world
/// units. The dot scales up with `sqrt(cell_count)` so area tracks the
/// component's size (issue 1869 render).
const CORRIDOR_NODE_RADIUS: f32 = 0.18;

/// Half-width of a corridor flow-edge quad at unit `overlap_width`, in
/// world units. The quad widens with `sqrt(overlap_width)` so the rendered
/// thickness tracks the branch's pinch width.
const CORRIDOR_FLOW_HALF_WIDTH: f32 = 0.025;

/// World z-lift for a corridor flow edge so it sits just behind the node
/// dots (smaller world-z draws under larger; the camera's `LessEqual`
/// depth convention).
const CORRIDOR_EDGE_LIFT: f32 = -0.01;

/// Color of a corridor `Punch` edge — a contrasting slate-grey so an
/// intra-tick barrier merge reads distinctly from the lineage-colored
/// flow edges.
const CORRIDOR_PUNCH_RGB: (f32, f32, f32) = (0.30, 0.30, 0.35);

/// Per-tick lane spacing on the component axis (world-y) and the
/// world-z step per tick layer for the abstract corridor-graph layout.
const CORRIDOR_LANE_STEP: f32 = 0.6;
const CORRIDOR_TICK_STEP: f32 = 1.0;

/// Half-width of a path overlay ribbon-tube cross-section (issue 1870),
/// in world units. Each path segment becomes a `+`-cross of two
/// perpendicular ribbons this wide, so the polyline reads as a thin tube
/// through the field volume from any camera angle.
const PATH_TUBE_HALF_WIDTH: f32 = 0.06;

/// Shortest segment the overlay draws (issue 1870). A consecutive sample
/// pair whose world-space separation is below this degenerates to nothing
/// rather than emitting a zero-area ribbon (e.g. a "stay put" tick that
/// holds the same cell, where the perpendicular basis is undefined).
const PATH_SEGMENT_EPSILON: f32 = 1e-6;

/// Reference span the field-rate ramp normalizes against (issue 1870).
/// Reach-cost fields are small integers (per-tick accumulated cost); a
/// fixed span keeps the cool→warm read stable across loads rather than
/// re-colouring the whole solid when a per-field maximum shifts. Values
/// past the span saturate at the warm end.
const RATE_SPAN: f32 = 64.0;

pub struct MeshViewer {
    triangles: Vec<DrawTriangle>,
    /// The path overlay (issue 1870): ribbon-tube `DrawTriangle`s for a
    /// decoded `TrajectorySet`, coloured per segment by how many paths
    /// share that grid step (the traffic ramp). Built once on a `.paths`
    /// load and re-emitted alongside `triangles` every `Render` stage.
    /// Empty until the first successful `.paths` load; a decode failure
    /// leaves the prior overlay intact (atomic replace, mirroring the
    /// mesh path).
    overlay: Vec<DrawTriangle>,
    /// The most recently decoded `.field` `ScalarField` (issue 1870),
    /// retained so the rate ramp shading the solid is available and so a
    /// later overlay load shares the same volume. `None` until the first
    /// successful `.field` load.
    field: Option<ScalarField>,
    /// Reply target of the most recent `aether.mesh.load` /
    /// `aether.corridor.load` request, parked across the async
    /// `aether.fs.read` round-trip (issue 964). `on_load` /
    /// `on_load_corridor` runs in the requester's reply context; the
    /// actual parse + cache replace happens later in `on_read_result`,
    /// whose reply context points at `FsCapability`, not the original
    /// requester. Stashing the handle here lets the load-result reply
    /// route back to whoever sent the request (the parked-sender
    /// pattern; the handle stays valid for the instance lifetime per the
    /// SDK `ReplyHandle` contract). `None` when the load was
    /// fire-and-forget (no reply target) or when no load is in flight.
    pending_reply: Option<ReplyHandle>,
    /// Which reply kind to send when the parked read settles — the load
    /// path that issued it. Set alongside `pending_reply` in `on_load` /
    /// `on_load_corridor` so `on_read_result` answers with the right
    /// result kind. The `.field` / `.dsl` / `.obj` arms still dispatch
    /// on the path extension inside the mesh outcome; this distinguishes
    /// the corridor ingest, whose bytes are a `CorridorGraph` rather than
    /// a mesh.
    pending_load: PendingLoad,
    /// The scrubbable corridor datum (issue 1869), built once from a
    /// `CorridorGraph` on `aether.corridor.load`. `None` until the first
    /// successful corridor load; a decode failure leaves the prior datum
    /// intact (whole-graph atomic replace, mirroring the mesh path).
    corridor: Option<CorridorView>,
    /// The current scrub tick cursor (issue 1869), set by
    /// `aether.corridor.scrub` and clamped to `[0, ticks)`. The `Render`
    /// stage re-emits this tick's node slice. Defaults to `0`.
    current_tick: u32,
}

/// Which load path parked the in-flight `aether.fs.read`, so
/// `on_read_result` knows which result kind to reply with.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PendingLoad {
    /// No load in flight, or the last load was a mesh load (the default).
    Mesh,
    /// An `aether.corridor.load` is awaiting its read reply.
    Corridor,
}

/// A loaded corridor graph plus its derived scrub index (issue 1869).
/// The graph is retained so the render pass can read node summaries
/// (`tick`, `component`, `cell_count`) and edge endpoints; the index
/// carries the O(1) per-tick addressing and the lineage that holds a
/// region's color constant across the scrub.
struct CorridorView {
    graph: CorridorGraph,
    index: ScrubIndex,
}

/// The tick-indexable scrub datum derived once from a `CorridorGraph`
/// (issue 1869). Everything here is recoverable from the flat
/// `Vec<CorridorNode>` + `Vec<CorridorEdge>`, so it lives viewer-side
/// rather than on the wire kind — building it once is O(N + E) and
/// amortizes across every scrub.
struct ScrubIndex {
    /// Number of distinct tick layers (`max node.tick + 1`, or `0` for an
    /// empty graph). The scrub cursor clamps to `[0, ticks)`.
    ticks: u32,
    /// Per-tick node buckets: `nodes_by_tick[t]` is the node indices (into
    /// `CorridorGraph::nodes`) whose `tick == t`. Addressing tick `t` is an
    /// O(1) slice lookup.
    nodes_by_tick: Vec<Vec<usize>>,
    /// Per-node out-degree over `Flow` edges — the branch count the issue
    /// asks for. Indexed by node index.
    flow_out_degree: Vec<u32>,
    /// Per-node lineage id, assigned in one forward pass over the flow-edge
    /// DAG: a component reached by exactly one flow edge from a
    /// non-splitting predecessor inherits that predecessor's lineage; a
    /// split (predecessor out-degree > 1) starts a fresh lineage on each
    /// branch; a merge keeps the lowest incoming lineage. The lineage id is
    /// what lets the viewer hold a region's color constant across the
    /// scrub. Indexed by node index.
    lineage: Vec<u32>,
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
impl WasmActor for MeshViewer {
    const NAMESPACE: &'static str = "aether.mesh_viewer";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, BootError> {
        Ok(MeshViewer {
            triangles: Vec::new(),
            overlay: Vec::new(),
            field: None,
            pending_reply: None,
            pending_load: PendingLoad::Mesh,
            corridor: None,
            current_tick: 0,
        })
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
    fn wire(&mut self, ctx: &mut WasmCtx<'_>) {
        ctx.actor::<LifecycleCapability>().subscribe::<Render>();
    }

    /// Re-emits every cached triangle to the render sink on the `Render`
    /// stage.
    ///
    /// # Agent
    /// Substrate-driven; do not send manually. If no triangles render
    /// after a `load`, the file failed to read / parse / mesh — check
    /// `engine_logs`.
    #[handler]
    fn on_render(&mut self, ctx: &mut WasmCtx<'_>, _render: Render) {
        if !self.triangles.is_empty() {
            ctx.actor::<RenderCapability>().send_many(&self.triangles);
        }
        // The path overlay (issue 1870) replays in the same pass as the
        // rate-shaded solid, so traffic-shaded polylines sit inside the
        // same volume the camera frames.
        if !self.overlay.is_empty() {
            ctx.actor::<RenderCapability>().send_many(&self.overlay);
        }
        if let Some(corridor) = &self.corridor {
            let tris = corridor.render_tick(self.current_tick);
            if !tris.is_empty() {
                ctx.actor::<RenderCapability>().send_many(&tris);
            }
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
    fn on_load(&mut self, ctx: &mut WasmCtx<'_>, msg: LoadMesh) {
        // Park the requester's reply target across the async read.
        // `on_read_result` answers it with the structured outcome.
        // Overwriting any prior pending handle is intentional —
        // loads are serialized through one read round-trip, and a
        // fresh load supersedes an unanswered prior one.
        self.pending_reply = ctx.reply_target();
        self.pending_load = PendingLoad::Mesh;
        tracing::info!(
            target: "aether_mesh_viewer",
            namespace = %msg.namespace,
            path = %msg.path,
            "load requested; issuing read",
        );
        ctx.actor::<FsCapability>().read(&msg.namespace, &msg.path);
    }

    /// Triggers an asynchronous corridor-graph load (issue 1869). The
    /// reply arrives as `aether.fs.read_result`; the bytes are decoded as
    /// an `aether_labyrinth::CorridorGraph` and a `ScrubIndex` is built over
    /// them. The `aether.corridor.load_result` reply to the originator
    /// fires once the read settles and the decode + index build outcome
    /// is known — see `on_read_result`.
    ///
    /// # Agent
    /// `namespace` is the short prefix with no `://` — `"save"`,
    /// `"assets"`, `"config"`. `path` points at a postcard-encoded
    /// `CorridorGraph` (write it via `aether.fs.write`). Send-and-await
    /// the `aether.corridor.load_result` reply to learn whether the load
    /// succeeded. After a successful load, `aether.corridor.scrub` to
    /// re-address a tick.
    #[allow(clippy::needless_pass_by_value)]
    #[handler]
    fn on_load_corridor(&mut self, ctx: &mut WasmCtx<'_>, msg: LoadCorridor) {
        self.pending_reply = ctx.reply_target();
        self.pending_load = PendingLoad::Corridor;
        tracing::info!(
            target: "aether_mesh_viewer",
            namespace = %msg.namespace,
            path = %msg.path,
            "corridor load requested; issuing read",
        );
        ctx.actor::<FsCapability>().read(&msg.namespace, &msg.path);
    }

    /// Sets the scrub tick cursor (issue 1869), clamped to the loaded
    /// graph's `[0, ticks)` span. Fire-and-forget. The next `Render`
    /// stage re-emits this tick's node slice, so the scrub is observable
    /// as a re-addressed frame. A `Scrub` with no corridor loaded clamps
    /// to `0` and renders nothing.
    ///
    /// # Agent
    /// Send `aether.corridor.scrub { tick }` after a corridor load to
    /// time-travel to that tick. Out-of-range ticks clamp to the last
    /// valid tick.
    #[allow(clippy::needless_pass_by_value)]
    #[handler]
    fn on_scrub(&mut self, _ctx: &mut WasmCtx<'_>, msg: Scrub) {
        let ticks = self.corridor.as_ref().map_or(0, |c| c.index.ticks);
        // Clamp into `[0, ticks)`; an empty graph (ticks == 0) holds the
        // cursor at 0. `saturating_sub` keeps `ticks == 0` from underflowing.
        self.current_tick = if ticks == 0 {
            0
        } else {
            msg.tick.min(ticks - 1)
        };
        tracing::info!(
            target: "aether_mesh_viewer",
            requested = msg.tick,
            current = self.current_tick,
            ticks,
            "scrub cursor set",
        );
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
    #[handler::manual]
    fn on_read_result(&mut self, ctx: &mut WasmCtx<'_, Manual>, r: ReadResult) {
        let pending = self.pending_load;
        let (namespace, path, outcome) = match r {
            ReadResult::Ok {
                namespace,
                path,
                bytes,
            } => {
                let outcome = match pending {
                    PendingLoad::Mesh => self.load_bytes(&path, &bytes),
                    PendingLoad::Corridor => self.load_corridor_bytes(&bytes),
                };
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
                    "read failed; keeping prior datum",
                );
                let outcome = LoadOutcome::failed(format!("read failed: {error:?}"));
                (namespace, path, outcome)
            }
        };
        // Reset to the default load path so a stray later read can't
        // mis-route as a corridor result.
        self.pending_load = PendingLoad::Mesh;
        self.reply_load_result(ctx, pending, namespace, path, outcome);
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
        let lower = path.rsplit('.').next().map(str::to_ascii_lowercase);
        // `.field` carries a binary `ScalarField` (issue 1868), not UTF-8
        // text, so it dispatches on the raw bytes before the text decode
        // the `.dsl` / `.obj` arms need.
        if lower.as_deref() == Some("field") {
            return self.try_replace_field(bytes);
        }
        // `.paths` carries a binary `TrajectorySet` (issue 1870), also not
        // UTF-8 text, so it dispatches on the raw bytes alongside `.field`.
        if lower.as_deref() == Some("paths") {
            return self.try_replace_paths(bytes);
        }
        let Ok(text) = str::from_utf8(bytes) else {
            tracing::warn!(
                target: "aether_mesh_viewer",
                path = %path,
                "mesh file is not valid UTF-8; keeping prior mesh",
            );
            return LoadOutcome::failed("mesh file is not valid UTF-8".to_string());
        };
        if lower.as_deref() == Some("dsl") {
            self.try_replace_dsl(text)
        } else if lower.as_deref() == Some("obj") {
            self.try_replace_obj(text)
        } else {
            tracing::warn!(
                target: "aether_mesh_viewer",
                path = %path,
                "unsupported file extension; expected .dsl, .obj, .field, or .paths",
            );
            LoadOutcome::failed(
                "unsupported file extension; expected .dsl, .obj, .field, or .paths".to_string(),
            )
        }
    }

    /// Build and dispatch the `aether.mesh.load_result` reply to the
    /// parked requester. No-op when no reply target was parked (the
    /// load was fire-and-forget). Clears the parked handle either way
    /// so a stale target can't leak into a later load's reply.
    fn reply_load_result(
        &mut self,
        ctx: &mut WasmCtx<'_, Manual>,
        pending: PendingLoad,
        namespace: String,
        path: String,
        outcome: LoadOutcome,
    ) {
        let Some(sender) = self.pending_reply.take() else {
            return;
        };
        let ok = outcome.error.is_none();
        match pending {
            PendingLoad::Mesh => ctx.reply_to(
                sender,
                &MeshLoadResult {
                    ok,
                    namespace,
                    path,
                    error: outcome.error,
                    warnings: outcome.warnings,
                },
            ),
            PendingLoad::Corridor => ctx.reply_to(
                sender,
                &CorridorLoadResult {
                    ok,
                    namespace,
                    path,
                    error: outcome.error,
                    warnings: outcome.warnings,
                },
            ),
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

    /// Decode a binary `ScalarField` (issue 1857) from `.field` bytes,
    /// iso-surface it, and replace the cached triangle list (issue 1868).
    ///
    /// The field's dense row-major `values[t * H * W + y * W + x]` layout
    /// *is* the stacked space-time volume, so it meshes directly with
    /// `(x, y, tick)` mapped to `(x, y, z)` and `depth = ticks` — time
    /// becomes world-z, matching the camera's depth convention. The
    /// iso threshold is fixed at `1`, so cost-0 cells are outside and
    /// every positive value (the `u32::MAX` unreachable sentinel included)
    /// is inside, with no special case. World placement is the fixed
    /// unit-cell convention (`FIELD_CELL` / `FIELD_ORIGIN`); the camera
    /// frames the result through `view_proj`. A decode or mesh failure
    /// leaves the prior cache intact, mirroring the `.dsl` / `.obj` arms.
    fn try_replace_field(&mut self, bytes: &[u8]) -> LoadOutcome {
        let Some(field) = ScalarField::decode_from_bytes(bytes) else {
            tracing::warn!(
                target: "aether_mesh_viewer",
                "ScalarField decode failed; keeping prior mesh",
            );
            return LoadOutcome::failed("ScalarField decode failed".to_string());
        };
        let expected = (field.width as usize)
            .saturating_mul(field.height as usize)
            .saturating_mul(field.ticks as usize);
        if field.values.len() != expected {
            tracing::warn!(
                target: "aether_mesh_viewer",
                width = field.width,
                height = field.height,
                ticks = field.ticks,
                values = field.values.len(),
                expected,
                "ScalarField values length mismatch; keeping prior mesh",
            );
            return LoadOutcome::failed(format!(
                "ScalarField values length {} != width * height * ticks = {}",
                field.values.len(),
                expected,
            ));
        }
        let tris = aether_mesh::surface_net(
            field.width as usize,
            field.height as usize,
            field.ticks as usize,
            &field.values,
            FIELD_ISO_THRESHOLD,
            FIELD_CELL,
            FIELD_ORIGIN,
        );
        // Issue 1870: shade each iso-vertex by the field rate `V` at the
        // cell it lands in, through `rate_ramp`, instead of the mesher's
        // flat palette index. A surface-net vertex sits a half-cell
        // outside the inside region (the boundary shell), so the cell it
        // reads from is the one its world position rounds *into* — clamped
        // to the field extent so a shell vertex just outside the grid
        // samples the nearest in-range cell rather than reading nothing.
        let out: Vec<DrawTriangle> = tris
            .iter()
            .map(|t| {
                let rgb = triangle_rate_rgb(&field, t.vertices);
                to_draw_triangle_rgb(t.vertices, rgb)
            })
            .collect();
        tracing::info!(
            target: "aether_mesh_viewer",
            width = field.width,
            height = field.height,
            ticks = field.ticks,
            triangles = out.len(),
            "field load complete; cache replaced",
        );
        self.triangles = out;
        self.field = Some(field);
        LoadOutcome::ok()
    }

    /// Decode a binary `TrajectorySet` (issue 1865 bundle kind) from
    /// `.paths` bytes, build the path overlay, and replace the cached
    /// overlay triangles (issue 1870).
    ///
    /// Each `TrajectoryLog` replays one moving point's tick-ordered path;
    /// a consecutive sample pair maps `(x, y, tick)` → world `(x, y,
    /// tick)` (time → world-z, the same stacking the `.field` solid uses)
    /// and becomes a `+`-cross ribbon tube. The tube is coloured by the
    /// step's *traffic*: how many paths in the set traverse that same grid
    /// step `(cell_t → cell_{t+1})`, mapped through `traffic_ramp` so a
    /// shared step reads hotter than a lone one. Traffic is counted in one
    /// pass over every log's consecutive sample pairs (keyed by the integer
    /// cell endpoints, so order within the set doesn't matter), then the
    /// ribbons are built in a second pass. A decode failure leaves the
    /// prior overlay intact, mirroring the `.dsl` / `.obj` / `.field` arms;
    /// an empty set clears the overlay to nothing.
    fn try_replace_paths(&mut self, bytes: &[u8]) -> LoadOutcome {
        let Some(set) = TrajectorySet::decode_from_bytes(bytes) else {
            tracing::warn!(
                target: "aether_mesh_viewer",
                "TrajectorySet decode failed; keeping prior overlay",
            );
            return LoadOutcome::failed("TrajectorySet decode failed".to_string());
        };
        let traffic = count_step_traffic(&set);
        let max_traffic = traffic.values().copied().max().unwrap_or(0);
        let mut out = Vec::new();
        for log in &set.logs {
            for pair in log.samples.windows(2) {
                let (from, to) = (&pair[0], &pair[1]);
                let key = step_key(from, to);
                let count = traffic.get(&key).copied().unwrap_or(0);
                let rgb = traffic_ramp(count, max_traffic);
                segment_tube(
                    cell_to_world(from),
                    cell_to_world(to),
                    PATH_TUBE_HALF_WIDTH,
                    rgb,
                    &mut out,
                );
            }
        }
        tracing::info!(
            target: "aether_mesh_viewer",
            paths = set.logs.len(),
            steps = traffic.len(),
            triangles = out.len(),
            "paths load complete; overlay replaced",
        );
        self.overlay = out;
        LoadOutcome::ok()
    }

    /// Decode a postcard `CorridorGraph` (issue 1858) from corridor-load
    /// bytes, build a `ScrubIndex` over it, and replace the cached
    /// corridor datum (issue 1869). The scrub cursor clamps to the new
    /// graph's tick span. A decode failure leaves the prior datum intact,
    /// mirroring the `.dsl` / `.obj` / `.field` arms.
    fn load_corridor_bytes(&mut self, bytes: &[u8]) -> LoadOutcome {
        let Some(graph) = CorridorGraph::decode_from_bytes(bytes) else {
            tracing::warn!(
                target: "aether_mesh_viewer",
                "CorridorGraph decode failed; keeping prior corridor",
            );
            return LoadOutcome::failed("CorridorGraph decode failed".to_string());
        };
        let index = build_scrub_index(&graph);
        // Re-clamp the cursor into the new tick span (a smaller graph may
        // leave the old cursor out of range).
        self.current_tick = if index.ticks == 0 {
            0
        } else {
            self.current_tick.min(index.ticks - 1)
        };
        tracing::info!(
            target: "aether_mesh_viewer",
            nodes = graph.nodes.len(),
            edges = graph.edges.len(),
            ticks = index.ticks,
            "corridor load complete; scrub datum built",
        );
        self.corridor = Some(CorridorView { graph, index });
        LoadOutcome::ok()
    }
}

/// Build the tick-indexable scrub datum (issue 1869) from a flat
/// `CorridorGraph`. One pass buckets nodes by `tick`, one edge pass
/// builds flow out-degree and forward flow adjacency, and one
/// topologically-ordered forward pass assigns lineage ids. Iterative
/// throughout (no recursion — the load-bearing-code rule): the lineage
/// pass walks nodes in `(tick, component)` order, which is a valid
/// topological order for a time-layered DAG whose flow edges always point
/// from tick `t` to `t + 1`.
fn build_scrub_index(graph: &CorridorGraph) -> ScrubIndex {
    let node_count = graph.nodes.len();

    // Tick span: max node tick + 1. An empty graph has 0 ticks.
    let ticks = graph
        .nodes
        .iter()
        .map(|n| n.tick)
        .max()
        .map_or(0, |t| t + 1);

    // Per-tick buckets. Pre-size to `ticks`, then push node indices in
    // node order so each bucket stays in `(tick, component)` order (the
    // graph's documented node ordering).
    let mut nodes_by_tick: Vec<Vec<usize>> = (0..ticks as usize).map(|_| Vec::new()).collect();
    for (idx, node) in graph.nodes.iter().enumerate() {
        nodes_by_tick[node.tick as usize].push(idx);
    }

    // Flow out-degree and forward flow adjacency in one edge pass. Punch
    // edges are intra-tick barrier merges and don't carry lineage, so they
    // are skipped here.
    let mut flow_out_degree = vec![0u32; node_count];
    let mut flow_adjacency: Vec<Vec<usize>> = (0..node_count).map(|_| Vec::new()).collect();
    for (edge_idx, edge) in graph.edges.iter().enumerate() {
        if edge.kind != EdgeKind::Flow {
            continue;
        }
        let from = edge.from as usize;
        if from < node_count {
            flow_out_degree[from] = flow_out_degree[from].saturating_add(1);
            flow_adjacency[from].push(edge_idx);
        }
    }

    // Lineage pass. Walk nodes in node order (a valid topological order
    // for the time-layered DAG) and propagate lineage forward along flow
    // edges. A node keeps the lowest lineage proposed to it by any
    // predecessor; a split (predecessor out-degree > 1) proposes a fresh
    // lineage on each branch so divergent regions read as new corridors;
    // a persist (out-degree == 1) hands the predecessor's lineage straight
    // through; a merge reconciles to the lowest incoming lineage. A node
    // never reached by a flow edge seeds a fresh lineage of its own.
    let mut lineage = vec![u32::MAX; node_count];
    let mut next_lineage: u32 = 0;
    for from in 0..node_count {
        // Seed a lineage for any node not yet assigned by a predecessor —
        // a tick-0 component or an isolated component.
        if lineage[from] == u32::MAX {
            lineage[from] = next_lineage;
            next_lineage += 1;
        }
        let from_lineage = lineage[from];
        let splits = flow_out_degree[from] > 1;
        for &edge_idx in &flow_adjacency[from] {
            let to = graph.edges[edge_idx].to as usize;
            if to >= node_count {
                continue;
            }
            // A split branch starts its own lineage; a single forward edge
            // carries the predecessor's lineage forward.
            let proposed = if splits {
                let fresh = next_lineage;
                next_lineage += 1;
                fresh
            } else {
                from_lineage
            };
            // Merge: keep the lowest lineage proposed to the successor.
            lineage[to] = if lineage[to] == u32::MAX {
                proposed
            } else {
                lineage[to].min(proposed)
            };
        }
    }

    ScrubIndex {
        ticks,
        nodes_by_tick,
        flow_out_degree,
        lineage,
    }
}

impl ScrubIndex {
    /// Node indices at tick `t` (the per-tick component slice). Empty for
    /// an out-of-range tick. O(1).
    fn nodes_at(&self, tick: u32) -> &[usize] {
        self.nodes_by_tick
            .get(tick as usize)
            .map_or(&[][..], Vec::as_slice)
    }
}

impl CorridorView {
    /// Lay the current tick's component slice out abstractly and emit it as
    /// `DrawTriangle`s (issue 1869 render). Nodes are square dots scaled by
    /// `sqrt(cell_count)` and lineage-colored (so a persisting region holds
    /// its color across the scrub); flow edges out of this tick's nodes are
    /// thin quads weighted by `sqrt(overlap_width)`; punch edges within the
    /// tick are contrasting slate quads. The layout is an abstract graph
    /// view — nodes positioned by `(tick → world-z, component lane →
    /// world-y)` — not a spatial field overlay, since the skeleton carries
    /// no cell sets.
    fn render_tick(&self, tick: u32) -> Vec<DrawTriangle> {
        let mut out = Vec::new();
        for &node_idx in self.index.nodes_at(tick) {
            let node = &self.graph.nodes[node_idx];
            let center = node_position(node.tick, node.component);
            // A splitting node (out-degree > 1) draws a touch larger so a
            // branch point reads as a graph event rather than a recolor.
            let split_emphasis = if self.index.flow_out_degree[node_idx] > 1 {
                1.3
            } else {
                1.0
            };
            let radius = CORRIDOR_NODE_RADIUS * scale_factor(node.cell_count) * split_emphasis;
            let rgb = lineage_color(self.index.lineage[node_idx]);
            push_quad(&mut out, center, radius, radius, 0.0, rgb);
            // Flow edges leaving this node — draw forward to the successor
            // at the next tick, weighted by overlap width.
            for edge in self
                .graph
                .edges
                .iter()
                .filter(|e| e.kind == EdgeKind::Flow && e.from as usize == node_idx)
            {
                let to = &self.graph.nodes[edge.to as usize];
                let end = node_position(to.tick, to.component);
                let half = CORRIDOR_FLOW_HALF_WIDTH * scale_factor(edge.overlap_width.max(1));
                push_edge_quad(&mut out, center, end, half, rgb);
            }
            // Punch edges touching this node — intra-tick barrier merges,
            // contrasting color. Drawn once from the lower endpoint to
            // avoid a double draw of the same edge.
            for edge in self
                .graph
                .edges
                .iter()
                .filter(|e| e.kind == EdgeKind::Punch && e.from as usize == node_idx)
            {
                let other = &self.graph.nodes[edge.to as usize];
                let end = node_position(other.tick, other.component);
                push_edge_quad(
                    &mut out,
                    center,
                    end,
                    CORRIDOR_FLOW_HALF_WIDTH,
                    CORRIDOR_PUNCH_RGB,
                );
            }
        }
        out
    }
}

/// World position of the component `component` at tick `tick` in the
/// abstract layout: tick steps along world-z, component lane along
/// world-y, world-x fixed. The casts are exact for the small tick /
/// component counts a corridor graph carries.
#[allow(clippy::cast_precision_loss)]
fn node_position(tick: u32, component: u32) -> Vec3 {
    Vec3::new(
        0.0,
        component as f32 * CORRIDOR_LANE_STEP,
        tick as f32 * CORRIDOR_TICK_STEP,
    )
}

/// Monotone, bounded scale from a count: `sqrt(count)` so a node dot's
/// area (or an edge's width) tracks the count without unbounded growth.
/// The cast is exact for the modest cell / overlap counts in practice.
#[allow(clippy::cast_precision_loss)]
fn scale_factor(count: u32) -> f32 {
    (count.max(1) as f32).sqrt()
}

/// Stable lineage → RGB via the existing palette, so a region holds one
/// color across the scrub (the anti-flicker payoff). Skips palette slot 7
/// (slate), reserved for the punch-edge contrast color.
fn lineage_color(lineage: u32) -> (f32, f32, f32) {
    PALETTE[(lineage as usize) % (PALETTE.len() - 1)]
}

/// Push an axis-aligned quad (two triangles) centered at `center` in the
/// x/y plane, lifted on world-z by `lift`.
fn push_quad(
    out: &mut Vec<DrawTriangle>,
    center: Vec3,
    hx: f32,
    hy: f32,
    lift: f32,
    rgb: (f32, f32, f32),
) {
    let c = Vec3::new(center.x, center.y, center.z + lift);
    let v00 = Vec3::new(c.x - hx, c.y - hy, c.z);
    let v10 = Vec3::new(c.x + hx, c.y - hy, c.z);
    let v11 = Vec3::new(c.x + hx, c.y + hy, c.z);
    let v01 = Vec3::new(c.x - hx, c.y + hy, c.z);
    out.push(to_draw_triangle_rgb([v00, v10, v11], rgb));
    out.push(to_draw_triangle_rgb([v00, v11, v01], rgb));
}

/// Push a thin quad (two triangles) along the segment `a → b`, `half`
/// world units to each side of the segment in the x/y plane. A zero-length
/// segment is skipped.
fn push_edge_quad(out: &mut Vec<DrawTriangle>, a: Vec3, b: Vec3, half: f32, rgb: (f32, f32, f32)) {
    let a = Vec3::new(a.x, a.y, a.z + CORRIDOR_EDGE_LIFT);
    let b = Vec3::new(b.x, b.y, b.z + CORRIDOR_EDGE_LIFT);
    let dir = b - a;
    let len = dir.length();
    if len < 1e-6 {
        return;
    }
    // Perpendicular in the x/y plane (z held), normalized.
    let perp = Vec3::new(-dir.y, dir.x, 0.0);
    let perp_len = perp.length();
    if perp_len < 1e-6 {
        return;
    }
    let off = perp * (half / perp_len);
    let v0 = a - off;
    let v1 = b - off;
    let v2 = b + off;
    let v3 = a + off;
    out.push(to_draw_triangle_rgb([v0, v1, v2], rgb));
    out.push(to_draw_triangle_rgb([v0, v2, v3], rgb));
}

/// Map a trajectory sample cell `(x, y, tick)` to its world position
/// (issue 1870): `(x, y, tick)` → world `(x, y, tick)`, with time on the
/// world-z axis. The same unit-cell / origin convention the `.field`
/// solid stacks with (`FIELD_CELL` / `FIELD_ORIGIN`), so a path threads
/// through the same volume the iso-surface fills.
#[allow(clippy::cast_precision_loss)] // grid cell indices are small
fn cell_to_world(s: &TrajectorySampleEntry) -> Vec3 {
    FIELD_ORIGIN
        + Vec3::new(
            s.x as f32 * FIELD_CELL.x,
            s.y as f32 * FIELD_CELL.y,
            s.tick as f32 * FIELD_CELL.z,
        )
}

/// A grid cell `(x, y, tick)` — one endpoint of a trajectory step.
type Cell = (u32, u32, u32);

/// The integer grid step a consecutive sample pair traverses: its two
/// cell endpoints `(from, to)`. Two paths that traverse the same step
/// share this key, so the traffic count over the set is exact in cell
/// space with no world-space rounding.
type StepKey = (Cell, Cell);

/// Per-step traffic counts over a `TrajectorySet`, keyed by [`StepKey`].
type TrafficMap = BTreeMap<StepKey, u32>;

/// The integer grid step a consecutive sample pair traverses (issue
/// 1870), keyed by its two cell endpoints. Two paths that traverse the
/// same step share this key.
fn step_key(from: &TrajectorySampleEntry, to: &TrajectorySampleEntry) -> StepKey {
    ((from.x, from.y, from.tick), (to.x, to.y, to.tick))
}

/// Count, over every log's consecutive sample pairs, how many paths
/// traverse each grid step (issue 1870). The map is keyed by [`step_key`]
/// so a step shared by N paths reaches a count of N; the per-segment
/// traffic ramp reads off it. Counted in one pass before any ribbon is
/// built so the maximum (for ramp normalization) is known up front.
fn count_step_traffic(set: &TrajectorySet) -> TrafficMap {
    let mut traffic = BTreeMap::new();
    for log in &set.logs {
        for pair in log.samples.windows(2) {
            *traffic.entry(step_key(&pair[0], &pair[1])).or_insert(0) += 1;
        }
    }
    traffic
}

/// A camera-independent `+`-cross ribbon tube along the segment `a → b`
/// (issue 1870), generalising [`outline_loop`]'s perpendicular-ribbon
/// technique to a free 3D segment. The viewer never receives `view_proj`
/// (the camera is owned elsewhere), so a camera-facing ribbon is not
/// available; instead two ribbons are placed in perpendicular planes —
/// the cross-section reads as a tube from any angle and degrades to a
/// thin edge-on only at the rare angle that looks straight down one
/// ribbon, which the other ribbon covers. The two in-plane perpendiculars
/// come from `dir × world_axis` with a fallback axis when `dir` is
/// parallel to the primary axis. A segment shorter than
/// `PATH_SEGMENT_EPSILON` emits nothing (an undefined direction).
fn segment_tube(
    a: Vec3,
    b: Vec3,
    half_width: f32,
    rgb: (f32, f32, f32),
    out: &mut Vec<DrawTriangle>,
) {
    let dir = b - a;
    let len = dir.length();
    if len < PATH_SEGMENT_EPSILON {
        return;
    }
    let dir = dir / len;
    // A stable perpendicular: cross `dir` with a world axis it isn't
    // (near-)parallel to. Y-up is the primary; fall back to X when `dir`
    // runs along Y so the cross product never collapses.
    let reference = if dir.cross(Vec3::Y).length() > 1e-3 {
        Vec3::Y
    } else {
        Vec3::X
    };
    let u = dir.cross(reference).normalize();
    let v = dir.cross(u).normalize();
    push_ribbon(out, a, b, u * half_width, rgb);
    push_ribbon(out, a, b, v * half_width, rgb);
}

/// Push one ribbon (two triangles) along `a → b`, `offset` to each side.
/// The shared half of [`segment_tube`]'s `+`-cross.
fn push_ribbon(out: &mut Vec<DrawTriangle>, a: Vec3, b: Vec3, offset: Vec3, rgb: (f32, f32, f32)) {
    let v0 = a - offset;
    let v1 = b - offset;
    let v2 = b + offset;
    let v3 = a + offset;
    out.push(to_draw_triangle_rgb([v0, v1, v2], rgb));
    out.push(to_draw_triangle_rgb([v0, v2, v3], rgb));
}

/// Colour ramp for the field rate `V` shading the solid (issue 1870):
/// low rate → cool blue, high rate → warm red, the `u32::MAX` unreachable
/// sentinel → a distinct desaturated grey so it reads apart from the
/// in-range gradient. The in-range value is normalized against a fixed
/// reference span so the ramp is stable across loads (a per-field max
/// would re-colour the whole solid every load); values past the span
/// saturate at the warm end. Monotone non-decreasing in `v` over the
/// in-range domain.
#[allow(clippy::cast_precision_loss)] // reach-cost values are small integers
fn rate_ramp(v: u32) -> (f32, f32, f32) {
    if v == u32::MAX {
        return (0.45, 0.45, 0.5); // unreachable sentinel — distinct grey
    }
    let t = (v as f32 / RATE_SPAN).clamp(0.0, 1.0);
    lerp_rgb((0.20, 0.45, 0.85), (0.90, 0.30, 0.20), t)
}

/// Colour ramp for path traffic density (issue 1870): a lone step (count
/// `1`) reads cool, the busiest step reads hot, normalized against
/// `max_count` so the hottest path in *this* set anchors the warm end.
/// `max_count == 0` (the degenerate empty set) maps everything to the
/// cool end. Monotone non-decreasing in `count`.
#[allow(clippy::cast_precision_loss)] // path counts are small integers
fn traffic_ramp(count: u32, max_count: u32) -> (f32, f32, f32) {
    let t = if max_count <= 1 {
        0.0
    } else {
        (count.saturating_sub(1) as f32 / (max_count - 1) as f32).clamp(0.0, 1.0)
    };
    lerp_rgb((0.35, 0.65, 0.95), (0.95, 0.75, 0.15), t)
}

/// Linearly interpolate between two RGB triples at `t ∈ [0, 1]`.
fn lerp_rgb(lo: (f32, f32, f32), hi: (f32, f32, f32), t: f32) -> (f32, f32, f32) {
    (
        (hi.0 - lo.0).mul_add(t, lo.0),
        (hi.1 - lo.1).mul_add(t, lo.1),
        (hi.2 - lo.2).mul_add(t, lo.2),
    )
}

/// Rate-shade a surface-net iso-triangle (issue 1870): sample the field
/// `V` at the cell each of the triangle's three world vertices rounds
/// into and average their rate colours. A surface-net vertex sits a
/// half-cell outside the inside region, so its world position is rounded
/// to the nearest cell and clamped into the field extent (a shell vertex
/// just outside the grid reads the nearest in-range cell). Averaging the
/// three keeps a face that straddles a rate boundary from snapping to one
/// side.
fn triangle_rate_rgb(field: &ScalarField, verts: [Vec3; 3]) -> (f32, f32, f32) {
    let mut acc = (0.0, 0.0, 0.0);
    for v in verts {
        let value = sample_field_at_world(field, v);
        let rgb = rate_ramp(value);
        acc.0 += rgb.0;
        acc.1 += rgb.1;
        acc.2 += rgb.2;
    }
    (acc.0 / 3.0, acc.1 / 3.0, acc.2 / 3.0)
}

/// Read the field `V` at the cell a world position rounds into (issue
/// 1870). World `(x, y, z)` inverts the `.field` placement
/// (`FIELD_ORIGIN` / `FIELD_CELL`) back to a `(cell_x, cell_y, tick)`
/// index, rounded to the nearest cell and clamped into `[0, dim)` on each
/// axis so a boundary-shell vertex just outside the grid still reads the
/// nearest in-range cell. Returns the dense-grid value
/// `values[tick * H * W + y * W + x]`.
fn sample_field_at_world(field: &ScalarField, world: Vec3) -> u32 {
    let local = world - FIELD_ORIGIN;
    let cx = round_clamp(local.x / FIELD_CELL.x, field.width);
    let cy = round_clamp(local.y / FIELD_CELL.y, field.height);
    let ct = round_clamp(local.z / FIELD_CELL.z, field.ticks);
    let idx = (ct * field.height as usize + cy) * field.width as usize + cx;
    field.values.get(idx).copied().unwrap_or(u32::MAX)
}

/// Round a cell coordinate to the nearest integer index and clamp it into
/// `[0, dim)`. `dim == 0` clamps to `0` (a degenerate field the caller's
/// length check already rejects, guarded here so the index math can't
/// underflow).
fn round_clamp(value: f32, dim: u32) -> usize {
    if dim == 0 {
        return 0;
    }
    // Clamp into `[0.0, (dim - 1)]` in float space *before* the integer
    // cast so the cast is provably non-negative and in range — the lint
    // it would otherwise warn about (sign loss / truncation) can't occur.
    #[allow(clippy::cast_precision_loss)] // dim is a small grid extent
    let hi = (dim - 1) as f32;
    let clamped = value.round().clamp(0.0, hi);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    // clamped to [0, dim-1], so the cast is exact and non-negative
    let index = clamped as usize;
    index
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

    /// A bare viewer with an empty cache and no parked reply — enough to
    /// drive `load_bytes` directly (no scheduler / ctx needed).
    fn empty_viewer() -> MeshViewer {
        MeshViewer {
            triangles: Vec::new(),
            overlay: Vec::new(),
            field: None,
            pending_reply: None,
            pending_load: PendingLoad::Mesh,
            corridor: None,
            current_tick: 0,
        }
    }

    /// A single-inside-sample `ScalarField` postcard-encoded the way an
    /// agent writes it via `aether.fs.write`.
    fn single_voxel_field_bytes() -> Vec<u8> {
        let mut values = vec![0u32; 3 * 3 * 3];
        values[13] = 1; // center sample (x, y, z) = (1, 1, 1): 1*9 + 1*3 + 1
        let field = ScalarField {
            width: 3,
            height: 3,
            ticks: 3,
            values,
        };
        field.encode_into_bytes()
    }

    /// Issue 1868: a `.field` load decodes a postcard `ScalarField`,
    /// meshes it, and replaces the cache with a non-empty triangle list,
    /// reporting `ok: true`. The single-voxel field meshes to the
    /// 12-triangle closed cube the mesher's own test asserts.
    #[test]
    fn field_load_replaces_cache_and_reports_ok() {
        let mut viewer = empty_viewer();
        let bytes = single_voxel_field_bytes();
        let outcome = viewer.load_bytes("reach.field", &bytes);
        assert!(
            outcome.error.is_none(),
            "good field should load: {:?}",
            outcome.error,
        );
        assert_eq!(
            viewer.triangles.len(),
            12,
            "single-voxel field meshes to a 12-triangle closed cube",
        );
    }

    /// Issue 1868: a malformed `.field` buffer keeps the prior cache and
    /// reports `ok: false`. A non-postcard byte run fails to decode; the
    /// previously-loaded triangles survive.
    #[test]
    fn malformed_field_keeps_prior_cache() {
        let mut viewer = empty_viewer();
        // Seed a prior good mesh.
        let good = single_voxel_field_bytes();
        viewer.load_bytes("good.field", &good);
        let prior = viewer.triangles.len();
        assert_eq!(prior, 12, "prior good load populated the cache");

        // A truncated / garbage buffer fails to decode.
        let outcome = viewer.load_bytes("bad.field", &[0xff, 0xff, 0xff, 0x01]);
        assert!(outcome.error.is_some(), "malformed field reports a failure");
        assert_eq!(
            viewer.triangles.len(),
            prior,
            "malformed field leaves the prior cache intact",
        );
    }

    /// A `.field` whose declared dimensions disagree with `values.len()`
    /// is rejected before meshing, keeping the prior cache.
    #[test]
    fn field_length_mismatch_keeps_prior_cache() {
        let mut viewer = empty_viewer();
        let good = single_voxel_field_bytes();
        viewer.load_bytes("good.field", &good);
        let prior = viewer.triangles.len();

        let field = ScalarField {
            width: 4,
            height: 4,
            ticks: 4,
            values: vec![1u32; 10], // not 4*4*4
        };
        let bytes = field.encode_into_bytes();
        let outcome = viewer.load_bytes("mismatch.field", &bytes);
        assert!(outcome.error.is_some(), "length mismatch reports a failure");
        assert_eq!(
            viewer.triangles.len(),
            prior,
            "length mismatch leaves the prior cache intact",
        );
    }

    use aether_kinds::TrajectoryEndReason;
    use aether_labyrinth::{CorridorEdge, CorridorNode};
    use aether_math::Aabb;

    /// Build a `TrajectorySampleEntry` at cell `(x, y, tick)` (value is
    /// not read by the overlay).
    fn entry(tick: u32, x: u32, y: u32) -> TrajectorySampleEntry {
        TrajectorySampleEntry {
            tick,
            x,
            y,
            value: 0,
        }
    }

    /// Build a `TrajectoryLog` from a list of `(tick, x, y)` cells.
    fn log_from(seed: u64, cells: &[(u32, u32, u32)]) -> aether_kinds::TrajectoryLog {
        aether_kinds::TrajectoryLog {
            seed,
            samples: cells.iter().map(|&(t, x, y)| entry(t, x, y)).collect(),
            end_reason: TrajectoryEndReason::Completed,
        }
    }

    fn paths_bytes(logs: Vec<aether_kinds::TrajectoryLog>) -> Vec<u8> {
        let set = TrajectorySet { logs };
        set.encode_into_bytes()
    }

    /// Issue 1870: `segment_tube` emits a `+`-cross of two ribbons (4
    /// triangles) for a non-degenerate segment, and the union's bounding
    /// box spans both endpoints.
    #[test]
    fn segment_tube_emits_a_cross_spanning_its_endpoints() {
        let a = Vec3::new(0.0, 0.0, 0.0);
        let b = Vec3::new(0.0, 0.0, 3.0);
        let mut out = Vec::new();
        segment_tube(a, b, 0.1, (0.5, 0.5, 0.5), &mut out);
        assert_eq!(
            out.len(),
            4,
            "a `+`-cross of two ribbons is 2 triangles each",
        );
        let mut points = Vec::new();
        for tri in &out {
            for v in tri.verts {
                points.push(Vec3::new(v.x, v.y, v.z));
            }
        }
        let aabb = Aabb::from_points(&points);
        assert!(
            aabb.min.z <= a.z + 1e-4 && aabb.max.z >= b.z - 1e-4,
            "the tube spans both endpoints on the segment axis: {aabb:?}",
        );
    }

    /// Issue 1870: a zero-length (stay-put) segment emits no geometry —
    /// its direction is undefined, so there is no ribbon to draw.
    #[test]
    fn degenerate_segment_emits_nothing() {
        let p = Vec3::new(1.0, 2.0, 3.0);
        let mut out = Vec::new();
        segment_tube(p, p, 0.1, (0.5, 0.5, 0.5), &mut out);
        assert!(out.is_empty(), "a zero-length segment draws nothing");
    }

    /// Issue 1870: the rate ramp is monotone non-decreasing over in-range
    /// values (warming as the rate rises) and the `u32::MAX` sentinel maps
    /// to a tone distinct from both ends of the gradient.
    #[test]
    fn rate_ramp_is_monotone_with_a_distinct_sentinel() {
        let lo = rate_ramp(0);
        let mid = rate_ramp(16);
        let hi = rate_ramp(64);
        assert!(
            lo.0 <= mid.0 && mid.0 <= hi.0,
            "red channel warms monotonically with rate",
        );
        assert!(
            lo.2 >= mid.2 && mid.2 >= hi.2,
            "blue channel cools monotonically with rate",
        );
        let sentinel = rate_ramp(u32::MAX);
        assert_ne!(sentinel, lo, "the sentinel reads apart from the cool end");
        assert_ne!(sentinel, hi, "the sentinel reads apart from the warm end");
    }

    /// Issue 1870: the traffic ramp warms monotonically as a step's share
    /// count rises toward the set maximum.
    #[test]
    fn traffic_ramp_warms_with_shared_count() {
        let lone = traffic_ramp(1, 4);
        let some = traffic_ramp(2, 4);
        let busiest = traffic_ramp(4, 4);
        assert!(
            lone.0 <= some.0 && some.0 <= busiest.0,
            "a busier step reads hotter (red channel rises)",
        );
        assert!(
            busiest.0 > lone.0,
            "the busiest step is strictly hotter than a lone step",
        );
    }

    /// Issue 1870: a `.paths` load decodes a `TrajectorySet`, builds the
    /// overlay, and reports `ok`. Two paths sharing one step colour that
    /// shared step's ribbon hotter than an unshared step's ribbon.
    #[test]
    fn shared_step_colours_hotter_than_an_unshared_step() {
        // Path A: (0,0,t0) → (1,0,t1) → (2,0,t2). Path B shares the first
        // step then diverges: (0,0,t0) → (1,0,t1) → (1,1,t2).
        let a = log_from(1, &[(0, 0, 0), (1, 1, 0), (2, 2, 0)]);
        let b = log_from(2, &[(0, 0, 0), (1, 1, 0), (2, 1, 1)]);
        let bytes = paths_bytes(vec![a, b]);

        let mut viewer = empty_viewer();
        let outcome = viewer.load_bytes("herd.paths", &bytes);
        assert!(
            outcome.error.is_none(),
            "good path set loads: {:?}",
            outcome.error,
        );
        assert!(!viewer.overlay.is_empty(), "overlay is populated");

        // Re-derive the colours the overlay used: the shared first step
        // (count 2) vs an unshared later step (count 1) under the same set
        // maximum, so the assertion tracks `try_replace_paths`'s ramp.
        let set = TrajectorySet {
            logs: vec![
                log_from(1, &[(0, 0, 0), (1, 1, 0), (2, 2, 0)]),
                log_from(2, &[(0, 0, 0), (1, 1, 0), (2, 1, 1)]),
            ],
        };
        let traffic = count_step_traffic(&set);
        let max = traffic
            .values()
            .copied()
            .max()
            .expect("test setup: the path set has at least one step");
        assert_eq!(max, 2, "the shared step carries two paths");
        let shared = traffic_ramp(2, max);
        let unshared = traffic_ramp(1, max);
        assert!(
            shared.0 > unshared.0,
            "the shared step's ribbon reads hotter than an unshared step's",
        );
    }

    /// Issue 1870: a malformed `.paths` buffer keeps the prior overlay and
    /// reports a failure.
    #[test]
    fn malformed_paths_keeps_prior_overlay() {
        let mut viewer = empty_viewer();
        let good = paths_bytes(vec![log_from(1, &[(0, 0, 0), (1, 1, 0)])]);
        viewer.load_bytes("good.paths", &good);
        let prior = viewer.overlay.len();
        assert!(prior > 0, "prior good load populated the overlay");

        let outcome = viewer.load_bytes("bad.paths", &[0xff, 0xff, 0xff, 0xff, 0xff]);
        assert!(
            outcome.error.is_some(),
            "malformed path set reports failure"
        );
        assert_eq!(
            viewer.overlay.len(),
            prior,
            "malformed path set leaves the prior overlay intact",
        );
    }

    /// Issue 1870: an empty `TrajectorySet` clears the overlay to no
    /// triangles (a successful load of zero paths).
    #[test]
    fn empty_path_set_clears_the_overlay() {
        let mut viewer = empty_viewer();
        let good = paths_bytes(vec![log_from(1, &[(0, 0, 0), (1, 1, 0)])]);
        viewer.load_bytes("good.paths", &good);
        assert!(
            !viewer.overlay.is_empty(),
            "prior good load populated overlay"
        );

        let empty = paths_bytes(Vec::new());
        let outcome = viewer.load_bytes("empty.paths", &empty);
        assert!(outcome.error.is_none(), "an empty set loads cleanly");
        assert!(
            viewer.overlay.is_empty(),
            "an empty path set clears the overlay to nothing",
        );
    }

    /// Issue 1870: the `.field` arm rate-shades the iso-surface — a field
    /// with a low-rate region and a high-rate region yields iso-vertices
    /// coloured at opposite ends of the rate ramp — and retains the
    /// decoded field on the viewer.
    #[test]
    fn field_rate_shades_low_and_high_regions() {
        // A 4×2×2 field: the x<2 half is low-rate (1), the x>=2 half is
        // high-rate (60), every cell inside. The boundary between the two
        // halves produces iso-vertices that read cool on the low side and
        // warm on the high side.
        let (w, h, t) = (4u32, 2u32, 2u32);
        let mut values = vec![1u32; (w * h * t) as usize];
        for tick in 0..t {
            for y in 0..h {
                for x in 2..w {
                    values[((tick * h + y) * w + x) as usize] = 60;
                }
            }
        }
        let field = ScalarField {
            width: w,
            height: h,
            ticks: t,
            values,
        };
        let bytes = field.encode_into_bytes();

        let mut viewer = empty_viewer();
        let outcome = viewer.load_bytes("rate.field", &bytes);
        assert!(
            outcome.error.is_none(),
            "rate field loads: {:?}",
            outcome.error,
        );
        assert!(viewer.field.is_some(), "the decoded field is retained");
        assert!(!viewer.triangles.is_empty(), "the solid meshed");

        // The reddest and bluest triangles in the solid should sit at
        // opposite ends — the high-rate half warm, the low-rate half cool.
        let reddest = viewer
            .triangles
            .iter()
            .map(|t| t.verts[0].r)
            .fold(f32::MIN, f32::max);
        let bluest = viewer
            .triangles
            .iter()
            .map(|t| t.verts[0].b)
            .fold(f32::MIN, f32::max);
        let cool = rate_ramp(1);
        let warm = rate_ramp(60);
        assert!(
            reddest > cool.0 + 0.1,
            "the high-rate region shades warm (redder than the cool end)",
        );
        assert!(
            bluest > warm.2 + 0.1,
            "the low-rate region shades cool (bluer than the warm end)",
        );
    }

    fn node(tick: u32, component: u32, cell_count: u32) -> CorridorNode {
        CorridorNode {
            tick,
            component,
            cell_count,
            min_cost: 0,
        }
    }

    fn flow_edge(from: u32, to: u32, overlap_width: u32) -> CorridorEdge {
        CorridorEdge {
            from,
            to,
            kind: EdgeKind::Flow,
            price: 0,
            overlap_width,
        }
    }

    fn punch_edge(from: u32, to: u32, price: u32) -> CorridorEdge {
        CorridorEdge {
            from,
            to,
            kind: EdgeKind::Punch,
            price,
            overlap_width: 0,
        }
    }

    /// A persist chain: one component per tick, linked by single flow edges.
    /// Lineage is constant across all three ticks (the anti-flicker
    /// property) and out-degree is 1 on every non-terminal node.
    #[test]
    fn persist_chain_holds_one_lineage() {
        let graph = CorridorGraph {
            nodes: vec![node(0, 0, 5), node(1, 0, 5), node(2, 0, 5)],
            edges: vec![flow_edge(0, 1, 4), flow_edge(1, 2, 4)],
        };
        let index = build_scrub_index(&graph);
        assert_eq!(index.ticks, 3);
        assert_eq!(
            index.lineage[0], index.lineage[1],
            "a persisting region keeps its lineage across a tick step",
        );
        assert_eq!(
            index.lineage[1], index.lineage[2],
            "lineage stays constant down the whole persist chain",
        );
        assert_eq!(index.flow_out_degree[0], 1);
        assert_eq!(index.flow_out_degree[1], 1);
        assert_eq!(
            index.flow_out_degree[2], 0,
            "the terminal node has no outgoing flow edges",
        );
    }

    /// A split: one component at tick 0 branches into two at tick 1.
    /// Out-degree is the branch count (2); each branch gets a fresh lineage
    /// distinct from the parent and from each other.
    #[test]
    fn split_branches_into_fresh_lineages() {
        let graph = CorridorGraph {
            nodes: vec![node(0, 0, 8), node(1, 0, 4), node(1, 1, 4)],
            edges: vec![flow_edge(0, 1, 3), flow_edge(0, 2, 3)],
        };
        let index = build_scrub_index(&graph);
        assert_eq!(
            index.flow_out_degree[0], 2,
            "out-degree equals the flow-edge branch count",
        );
        assert_ne!(
            index.lineage[1], index.lineage[2],
            "the two split branches get distinct lineages",
        );
        assert_ne!(
            index.lineage[1], index.lineage[0],
            "a split branch starts a fresh lineage, not the parent's",
        );
        assert_ne!(index.lineage[2], index.lineage[0]);
    }

    /// A merge: two components at tick 0 flow into one at tick 1. The
    /// successor reconciles to the lowest incoming lineage, so the join is
    /// recorded as a single carried lineage rather than two colors.
    #[test]
    fn merge_reconciles_to_lowest_lineage() {
        let graph = CorridorGraph {
            nodes: vec![node(0, 0, 4), node(0, 1, 4), node(1, 0, 8)],
            edges: vec![flow_edge(0, 2, 3), flow_edge(1, 2, 3)],
        };
        let index = build_scrub_index(&graph);
        let lo = index.lineage[0].min(index.lineage[1]);
        assert_eq!(
            index.lineage[2], lo,
            "a merge keeps the lowest incoming lineage",
        );
    }

    /// `nodes_at(t)` returns exactly the tick's component node indices, in
    /// `(tick, component)` order, and an empty slice for an out-of-range tick.
    #[test]
    fn nodes_at_returns_the_tick_slice() {
        let graph = CorridorGraph {
            nodes: vec![node(0, 0, 1), node(1, 0, 1), node(1, 1, 1), node(2, 0, 1)],
            edges: vec![],
        };
        let index = build_scrub_index(&graph);
        assert_eq!(index.nodes_at(0), &[0]);
        assert_eq!(index.nodes_at(1), &[1, 2], "tick 1 has two components");
        assert_eq!(index.nodes_at(2), &[3]);
        assert!(
            index.nodes_at(99).is_empty(),
            "out-of-range tick is an empty slice",
        );
    }

    /// An empty graph yields a zero-tick index with empty derived tables —
    /// no panic, and a scrub clamps to tick 0.
    #[test]
    fn empty_graph_builds_zero_tick_index() {
        let graph = CorridorGraph {
            nodes: vec![],
            edges: vec![],
        };
        let index = build_scrub_index(&graph);
        assert_eq!(index.ticks, 0);
        assert!(index.nodes_by_tick.is_empty());
        assert!(index.nodes_at(0).is_empty());
    }

    /// Punch edges don't contribute to flow out-degree or lineage: an
    /// intra-tick punch between two tick-0 components leaves each on its
    /// own lineage and out-degree 0.
    #[test]
    fn punch_edges_do_not_carry_lineage() {
        let graph = CorridorGraph {
            nodes: vec![node(0, 0, 4), node(0, 1, 4)],
            edges: vec![punch_edge(0, 1, 7)],
        };
        let index = build_scrub_index(&graph);
        assert_eq!(index.flow_out_degree[0], 0, "a punch is not a flow branch");
        assert_ne!(
            index.lineage[0], index.lineage[1],
            "a punch does not merge lineages",
        );
    }

    /// A good corridor load decodes the postcard bytes, builds the index,
    /// caches it, and reports `ok`. The scrub cursor clamps into the graph's
    /// tick span.
    #[test]
    fn corridor_load_builds_and_clamps_cursor() {
        let graph = CorridorGraph {
            nodes: vec![node(0, 0, 5), node(1, 0, 5)],
            edges: vec![flow_edge(0, 1, 4)],
        };
        let bytes = graph.encode_into_bytes();
        let mut viewer = empty_viewer();
        viewer.current_tick = 9; // out of range for a 2-tick graph
        let outcome = viewer.load_corridor_bytes(&bytes);
        assert!(outcome.error.is_none(), "good corridor load succeeds");
        assert!(viewer.corridor.is_some(), "the datum is cached");
        assert_eq!(
            viewer.current_tick, 1,
            "the cursor clamps into the new graph's tick span",
        );
    }

    /// A malformed corridor buffer keeps the prior datum and reports a
    /// failure (whole-graph atomic replace, mirroring the mesh path).
    #[test]
    fn malformed_corridor_keeps_prior_datum() {
        let graph = CorridorGraph {
            nodes: vec![node(0, 0, 5), node(1, 0, 5)],
            edges: vec![flow_edge(0, 1, 4)],
        };
        let mut viewer = empty_viewer();
        viewer.load_corridor_bytes(&graph.encode_into_bytes());
        assert!(
            viewer.corridor.is_some(),
            "prior good load populated the datum"
        );

        let outcome = viewer.load_corridor_bytes(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff]);
        assert!(
            outcome.error.is_some(),
            "malformed corridor reports a failure"
        );
        assert!(
            viewer.corridor.is_some(),
            "malformed corridor leaves the prior datum intact",
        );
    }

    /// The render pass emits triangles for a non-empty tick slice and an
    /// empty list for an out-of-range tick.
    #[test]
    fn render_tick_emits_for_a_populated_tick() {
        let graph = CorridorGraph {
            nodes: vec![node(0, 0, 5), node(1, 0, 5)],
            edges: vec![flow_edge(0, 1, 4)],
        };
        let index = build_scrub_index(&graph);
        let view = CorridorView { graph, index };
        assert!(
            !view.render_tick(0).is_empty(),
            "tick 0 has a node and an outgoing flow edge to draw",
        );
        assert!(
            view.render_tick(99).is_empty(),
            "an out-of-range tick draws nothing",
        );
    }
}

aether_actor::export!(MeshViewer);
