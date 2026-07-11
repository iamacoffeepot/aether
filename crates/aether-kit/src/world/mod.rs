// `#[handler]` methods take the decoded mail by value per the ADR-0033
// dispatch ABI; the macro-generated trampoline owns the payload.
#![allow(clippy::needless_pass_by_value)]

//! World-view runtime. Meshes the chunked plane stack
//! ([`crate::world`]) into a flat-color base render and replays the cached
//! meshes to `"aether.render"` each frame on the `Render` lifecycle stage.
//!
//! The mesher lives in [`mesher`] as a pure function ([`mesh_chunk`]); this
//! actor keeps the per-chunk mesh cache and the cache invalidation. Each
//! chunk becomes flat-color partition regions with closed cliff faces. A
//! chunk's contours read a bounded apron into its neighbors, so a write
//! invalidates its own cache and its eight cached neighbors.
//!
//! # Mail surface
//!
//! - `aether.kit.world.set_chunk` — write one chunk's planes and remesh
//!   that chunk plus its eight cached neighbors (their border contours read
//!   the new planes through the apron).
//! - `aether.kit.world.set_cell_points` — stamp one cell's underlay
//!   material points and remesh that cell's chunk plus its eight cached
//!   neighbors (the single-cell live-paint counterpart to `set_chunk`).
//! - `aether.kit.world.set_cell_heights` — stamp one cell's height deltas
//!   (subcell relief off the cell height) and remesh that cell's chunk plus
//!   its eight cached neighbors (the height sibling of `set_cell_points`).
//! - `aether.kit.world.stamp_{polygon,disc,hexagon}` — rasterize a compact
//!   world-octimeter shape into scalar subcell coverage and remesh every
//!   touched chunk plus its cached apron neighbors.
//! - `aether.kit.world.{apply_brush,run_automaton}` — execute a bounded,
//!   mark-attributed terrain operator, reply with exact partial statistics,
//!   and remesh its deduplicated touched-chunk apron even on exhaustion.
//! - `aether.kit.world.pick_terrain` — intersect a bounded named world ray
//!   with the first markable rendered top surface.
//! - `aether.kit.world.set_mark_overlay_{visibility,selection}` — control a
//!   read-only, revision-exact projection of the default-loaded `MarkBook`.
//! - `aether.kit.world.set_region` — register a region so the underlay
//!   cascade has a default to resolve to; remeshes every cached chunk
//!   (a region default can change any chunk's cascade-resolved underlay).
//! - `aether.kit.world.load` — fetch a serialized world through
//!   `aether.fs`, decode, atomically swap, and remesh all. A decode or
//!   read failure keeps the prior world (errors go to logs).

mod data;
pub use data::*;
mod kinds;
pub use kinds::*;
pub mod mesher;
mod operator;
mod overlay;
mod pick;
mod raster;
pub use overlay::{
    MARK_OVERLAY_COLOR, MARK_OVERLAY_LIFT_METERS, MARK_PATH_HALF_WIDTH_METERS, MARK_POINT_RADIUS_METERS,
    MARK_SELECTED_COLOR, MARK_SELECTED_HALF_WIDTH_METERS, MARK_SELECTED_HANDLE_RADIUS_METERS,
    MAX_MARK_OVERLAY_TRIANGLES, MAX_MARK_OVERLAY_VERTICES,
};
pub use pick::{
    MAX_TERRAIN_PICK_DISTANCE_METERS, TERRAIN_PICK_EPSILON_METERS, TERRAIN_PICK_REFINEMENT_STEPS,
    TERRAIN_PICK_STEP_METERS,
};

use alloc::collections::{BTreeMap, BTreeSet};

use aether_actor::{
    ActorInitError, Manual, OutboundReply, ReplyMode, RequestId, WasmActor, WasmCtx, WasmInitCtx, actor,
};
use aether_capabilities::component::ComponentHostWasmExt;
use aether_capabilities::fs::{Read, ReadResult};
use aether_capabilities::lifecycle::LifecycleMailboxExt;
use aether_capabilities::render::DrawTriangle;
use aether_capabilities::{ComponentHostCapability, FsCapability, LifecycleCapability, RenderCapability};
use aether_data::Source;
use aether_kinds::Render;
use serde::{Deserialize, Serialize};

use crate::mark::{Mark, MarkBook, MarkId, MarkList, MarkListResult, MarkRef};

use self::mesher::mesh_chunk;
use self::mesher::style::StyleTable;

/// Default load name of the authoritative `MarkBook` peer.
const MARK_BOOK_COMPONENT: &str = "aether.kit.mark";
const MARK_OVERLAY_REFRESH_ATTEMPT_LIMIT: u8 = 3;

/// World-view component: holds the world plane stack and a per-chunk
/// mesh cache, and replays the cache to the render sink each frame.
///
/// # Agent
/// Load with the `aether_kit@aether.kit.world` export. Paint the world by
/// sending `aether.kit.world.stamp_{polygon,disc,hexagon}` for compact smooth
/// shapes, bounded `apply_brush` / `run_automaton` requests for repeatable
/// mark-attributed generation, `aether.kit.world.set_chunk` for raw planes, and
/// `aether.kit.world.set_region` for an underlay cascade default; each send
/// remeshes and the world renders every frame under the active
/// `aether.view_projection` view. `aether.kit.world.load` swaps a serialized
/// world from `aether.fs`. Operator replies report an applied or exhausted
/// partial result; proposal/commit state remains ADR-0143's separate surface.
/// Use `capture_frame` to verify.
pub struct WorldView {
    world: World,
    meshes: BTreeMap<ChunkPos, Vec<DrawTriangle>>,
    styles: StyleTable,
    mark_overlay: MarkOverlayProjection,
}

#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.world.load_context")]
struct WorldLoadContext {
    namespace: String,
    path: String,
}

#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy)]
#[kind(name = "aether.kit.world.mark_overlay_refresh_context")]
struct MarkOverlayRefreshContext {
    generation: u64,
}

#[derive(Default)]
struct MarkOverlayProjection {
    visible: bool,
    synchronized: bool,
    marks: BTreeMap<MarkId, Mark>,
    selected: Option<MarkRef>,
    pending_request: Option<RequestId>,
    generation: u64,
    budget_overflowed: bool,
    refresh_attempts: u8,
}

impl MarkOverlayProjection {
    fn set_visibility(&mut self, visible: bool) -> SetMarkOverlayVisibilityResult {
        if visible != self.visible {
            self.generation = self.generation.wrapping_add(1);
        }
        self.visible = visible;
        if !visible {
            self.synchronized = false;
            self.marks.clear();
            self.selected = None;
            self.pending_request = None;
            self.budget_overflowed = false;
            self.refresh_attempts = 0;
        }
        SetMarkOverlayVisibilityResult { visible, synchronized: visible && self.synchronized }
    }

    fn set_selection(&mut self, requested: Option<MarkRef>) -> SetMarkOverlaySelectionResult {
        let Some(requested) = requested else {
            self.selected = None;
            return SetMarkOverlaySelectionResult::Cleared;
        };
        let cached = self.marks.get(&requested.id).map(Mark::reference);
        match cached {
            Some(current) if current == requested && self.visible && self.synchronized => {
                self.selected = Some(requested);
                SetMarkOverlaySelectionResult::Selected { reference: requested }
            }
            Some(current) if current.revision > requested.revision => {
                SetMarkOverlaySelectionResult::Stale { requested, current }
            }
            _ => SetMarkOverlaySelectionResult::Unsynchronized { requested, cached },
        }
    }

    fn replace_snapshot(&mut self, marks: Vec<Mark>) {
        let replacement: BTreeMap<MarkId, Mark> = marks.into_iter().map(|mark| (mark.id, mark)).collect();
        if let Some(selected) = self.selected
            && replacement.get(&selected.id).is_none_or(|mark| mark.reference() != selected)
        {
            self.selected = None;
        }
        self.marks = replacement;
        self.synchronized = true;
        self.refresh_attempts = 0;
    }

    fn should_request_refresh(&self) -> bool {
        self.visible && self.pending_request.is_none() && self.refresh_attempts < MARK_OVERLAY_REFRESH_ATTEMPT_LIMIT
    }

    fn record_refresh_attempt(&mut self, request: RequestId) -> bool {
        self.refresh_attempts = self.refresh_attempts.saturating_add(1);
        if request.0 == Source::NO_CORRELATION {
            return false;
        }
        self.pending_request = Some(request);
        true
    }

    fn settle_refresh_reply(
        &mut self,
        request: RequestId,
        context: Option<MarkOverlayRefreshContext>,
        marks: Vec<Mark>,
    ) -> bool {
        if self.pending_request != Some(request) {
            return false;
        }
        self.pending_request = None;
        let Some(context) = context else {
            return false;
        };
        if !self.visible || context.generation != self.generation {
            return false;
        }
        self.replace_snapshot(marks);
        true
    }
}

impl WorldView {
    fn request_mark_overlay_refresh<M: ReplyMode>(&mut self, ctx: &WasmCtx<'_, M>) {
        if !self.mark_overlay.should_request_refresh() {
            return;
        }
        let context = MarkOverlayRefreshContext { generation: self.mark_overlay.generation };
        let request = ctx
            .actor::<ComponentHostCapability>()
            .loaded::<MarkBook>(MARK_BOOK_COMPONENT)
            .send_with_context(&MarkList, &context);
        if !self.mark_overlay.record_refresh_attempt(request)
            && self.mark_overlay.refresh_attempts == MARK_OVERLAY_REFRESH_ATTEMPT_LIMIT
        {
            tracing::warn!(
                target: "aether_kit",
                attempts = self.mark_overlay.refresh_attempts,
                "mark overlay refresh send was dropped; retry budget exhausted",
            );
        }
    }

    /// Rebuild every chunk's cached mesh from the current world — used
    /// after a whole-world change (region default, world load) that can
    /// alter any chunk's mesh.
    fn remesh_all(&mut self) {
        self.meshes.clear();
        let positions: Vec<ChunkPos> = self.world.chunks().map(|(pos, _)| pos).collect();
        for pos in positions {
            self.meshes.insert(pos, mesh_chunk(&self.world, pos, &self.styles));
        }
    }

    /// Remesh `pos` and its eight cached neighbors after a write inside
    /// `pos`. The mesher's contours read a bounded apron into the
    /// neighbors, so a write changes the border geometry of any cached
    /// neighbor as well as `pos`'s own mesh. Neighbors with no cached mesh
    /// are not rendered, so they need no remesh; an empty neighbor's border
    /// geometry is already covered by this chunk's own apron windows.
    fn remesh_around(&mut self, pos: ChunkPos) {
        for dz in -1..=1 {
            for dx in -1..=1 {
                let neighbor = ChunkPos { x: pos.x + dx, z: pos.z + dz };
                if (dx == 0 && dz == 0) || self.meshes.contains_key(&neighbor) {
                    self.meshes.insert(neighbor, mesh_chunk(&self.world, neighbor, &self.styles));
                }
            }
        }
    }

    /// Rebuild every touched chunk and cached apron neighbor exactly once.
    /// Operators may touch adjacent chunks, whose 3×3 aprons overlap heavily;
    /// collecting first prevents repeated meshing while preserving the
    /// ordinary rule that an absent, untouched neighbor needs no cache entry.
    fn remesh_touched(&mut self, touched: &BTreeSet<ChunkPos>) {
        let mut remesh = touched.clone();
        for pos in touched {
            for dz in -1..=1 {
                for dx in -1..=1 {
                    let Some(x) = pos.x.checked_add(dx) else {
                        continue;
                    };
                    let Some(z) = pos.z.checked_add(dz) else {
                        continue;
                    };
                    let neighbor = ChunkPos { x, z };
                    if self.meshes.contains_key(&neighbor) {
                        remesh.insert(neighbor);
                    }
                }
            }
        }
        for pos in remesh {
            self.meshes.insert(pos, mesh_chunk(&self.world, pos, &self.styles));
        }
    }

    /// Rasterize `vertices` into the overlay coverage plane and rebuild every
    /// changed chunk through the ordinary apron-aware invalidation path.
    fn stamp_vertices(&mut self, vertices: &[WorldPoint], material: Material) {
        let touched = raster::stamp_polygon(&mut self.world, vertices, material);
        for pos in touched {
            self.remesh_around(pos);
        }
    }
}

#[actor]
impl WasmActor for WorldView {
    const NAMESPACE: &'static str = "aether.kit.world";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(WorldView {
            world: World::new(),
            meshes: BTreeMap::new(),
            styles: StyleTable::default(),
            mark_overlay: MarkOverlayProjection::default(),
        })
    }

    /// Subscribe the `Render` lifecycle stage so the cached meshes
    /// re-emit once per frame, after the `Tick` chain settles (ADR-0082
    /// §11) — the same render-replay placement as the camera / mesh
    /// viewer. The view has no per-tick compute; it only re-emits. On a
    /// chassis whose lifecycle graph omits `Render` (headless), the
    /// fire-and-forget subscribe warn-drops and the view simply never
    /// submits.
    fn wire(&mut self, ctx: &mut WasmCtx<'_>) {
        ctx.actor::<LifecycleCapability>().subscribe::<Render>();
    }

    /// Re-emit every cached chunk mesh to the render sink on the `Render`
    /// stage.
    ///
    /// # Agent
    /// Substrate-driven; do not send manually. If nothing renders after a
    /// `set_chunk`, the underlay resolved to `Void` (nothing to draw) or
    /// no camera is active.
    #[handler::single]
    fn on_render(&mut self, ctx: &mut WasmCtx<'_>, _render: Render) {
        self.request_mark_overlay_refresh(ctx);
        for mesh in self.meshes.values() {
            if !mesh.is_empty() {
                ctx.actor::<RenderCapability>().send_many(mesh);
            }
        }
        if self.mark_overlay.visible {
            let batch = overlay::mark_overlay_batch(&self.world, &self.mark_overlay.marks, self.mark_overlay.selected);
            if let Some(overflow) = batch.overflow
                && !self.mark_overlay.budget_overflowed
            {
                tracing::warn!(
                    target: "aether_kit",
                    first_omitted_mark = overflow.first_omitted_mark.get(),
                    emitted_triangles = overflow.emitted_triangles,
                    emitted_vertices = overflow.emitted_vertices,
                    triangle_limit = MAX_MARK_OVERLAY_TRIANGLES,
                    vertex_limit = MAX_MARK_OVERLAY_VERTICES,
                    "mark overlay frame budget exhausted; later geometry was omitted",
                );
            }
            self.mark_overlay.budget_overflowed = batch.overflow.is_some();
            if !batch.triangles.is_empty() {
                ctx.actor::<RenderCapability>().send_many(&batch.triangles);
            }
        }
    }

    /// Intersect a bounded world-space ray with the first markable terrain
    /// top surface.
    #[handler::manual]
    fn on_pick_terrain(&mut self, ctx: &mut WasmCtx<'_, Manual>, msg: PickTerrain) {
        if ctx.reply_target().is_some() {
            ctx.reply(&pick::pick_terrain(&self.world, msg.ray));
        }
    }

    /// Show or hide the read-only `MarkBook` projection. Enabling starts one
    /// correlated refresh; the immediate result reports whether a snapshot
    /// was already synchronized.
    #[handler::manual]
    fn on_set_mark_overlay_visibility(&mut self, ctx: &mut WasmCtx<'_, Manual>, msg: SetMarkOverlayVisibility) {
        let result = self.mark_overlay.set_visibility(msg.visible);
        if msg.visible {
            self.request_mark_overlay_refresh(ctx);
        }
        if ctx.reply_target().is_some() {
            ctx.reply(&result);
        }
    }

    /// Select an exact cached mark revision for highlighted rendering.
    /// Missing or ahead revisions request a refresh and remain unchanged.
    #[handler::manual]
    fn on_set_mark_overlay_selection(&mut self, ctx: &mut WasmCtx<'_, Manual>, msg: SetMarkOverlaySelection) {
        let result = self.mark_overlay.set_selection(msg.selected);
        if matches!(result, SetMarkOverlaySelectionResult::Unsynchronized { .. }) {
            self.request_mark_overlay_refresh(ctx);
        }
        if ctx.reply_target().is_some() {
            ctx.reply(&result);
        }
    }

    /// Atomically replace the cached render projection from the correlated
    /// `MarkBook` list reply. Ordinary present-source sends, stale requests,
    /// and late replies after hiding are ignored.
    #[handler::single]
    fn on_mark_list_result(&mut self, ctx: &mut WasmCtx<'_>, result: MarkListResult) {
        if ctx.source_mailbox().is_some() {
            return;
        }
        let Some(request) = ctx.in_reply_to() else {
            return;
        };
        if self.mark_overlay.pending_request != Some(request) {
            return;
        }
        let context = ctx.take_context::<MarkOverlayRefreshContext>();
        self.mark_overlay.settle_refresh_reply(request, context, result.marks);
    }

    /// Write one chunk's planes into the world, then remesh that chunk and
    /// its eight cached neighbors. The mesher's contours read a bounded
    /// apron into the neighbors, so a write changes the edge contours of any
    /// cached neighbor as well as the written chunk's own mesh. Neighbors
    /// with no cached mesh are not rendered, so they need no remesh; an
    /// empty neighbor's border geometry is already covered by this chunk's
    /// own apron windows.
    #[handler::single]
    fn on_set_chunk(&mut self, _ctx: &mut WasmCtx<'_>, msg: SetChunk) {
        let pos = msg.chunk_pos();
        self.world.insert_chunk(pos, msg.into_chunk());
        self.remesh_around(pos);
    }

    /// Stamp one cell's underlay material points into the world, then remesh
    /// that cell's chunk and its eight cached neighbors — a border cell's
    /// points feed the neighbor's contours through the same apron as a chunk
    /// write.
    ///
    /// # Agent
    /// Send `aether.kit.world.set_cell_points` with a cell address (`x`,
    /// `z`) and up to SUB² point bytes in `z*SUB + x` subcell order (a
    /// `Material` byte, `255` = inherit the cell's cascade, or `0` = an
    /// authored `Void` that cuts a hole). A short vector leaves the cell's
    /// remaining points inheriting. `capture_frame` to verify the marched
    /// silhouette.
    #[handler::single]
    fn on_set_cell_points(&mut self, _ctx: &mut WasmCtx<'_>, msg: SetCellPoints) {
        let cell = msg.cell();
        self.world.set_cell_points(cell, &msg.points);
        self.remesh_around(cell.chunk());
    }

    /// Rasterize an arbitrary polygon in world-octimeter coordinates into
    /// anti-aliased scalar overlay coverage, then remesh every touched chunk
    /// and its cached apron neighbors.
    ///
    /// # Agent
    /// Send `aether.kit.world.stamp_polygon` with a vertex ring in
    /// `points` (named `x_octimeters` / `z_octimeters` fields) and a raw
    /// `Material` byte. This is the compact shape-authoring counterpart to
    /// hand-building `set_cell_points` / `set_chunk` arrays.
    #[handler::single]
    fn on_stamp_polygon(&mut self, _ctx: &mut WasmCtx<'_>, msg: StampPolygon) {
        self.stamp_vertices(&msg.points, Material::from_u8_or_void(msg.material));
    }

    /// Generate and rasterize a disc vertex ring in world-octimeter
    /// coordinates, then remesh every touched chunk and apron.
    ///
    /// # Agent
    /// Send `aether.kit.world.stamp_disc` with a `center` world point,
    /// `radius_octimeters`, and a raw `Material` byte.
    #[handler::single]
    fn on_stamp_disc(&mut self, _ctx: &mut WasmCtx<'_>, msg: StampDisc) {
        let vertices = raster::disc_vertices(msg.center, msg.radius_octimeters);
        self.stamp_vertices(&vertices, Material::from_u8_or_void(msg.material));
    }

    /// Generate and rasterize a flat-top regular hexagon in world-octimeter
    /// coordinates, then remesh every touched chunk and apron.
    ///
    /// # Agent
    /// Send `aether.kit.world.stamp_hexagon` with
    /// a `center` world point, center-to-vertex `radius_octimeters`, and a raw
    /// `Material` byte.
    #[handler::single]
    fn on_stamp_hexagon(&mut self, _ctx: &mut WasmCtx<'_>, msg: StampHexagon) {
        let vertices = raster::regular_hexagon_vertices(msg.center, msg.radius_octimeters);
        self.stamp_vertices(&vertices, Material::from_u8_or_void(msg.material));
    }

    /// Apply a bounded disc brush along a world-space path and reply with the
    /// exact accepted prefix. Exhaustion is a typed result, not a trap: the
    /// consistent partial world is remeshed before the reply is emitted.
    ///
    /// # Agent
    /// Send `aether.kit.world.apply_brush` with a revisioned `source`, named
    /// `WorldPoint` path, non-zero radius/spacing, raw material byte, and both
    /// execution limits. Inspect `aether.kit.world.operator_result`; on
    /// failure its stats describe the committed partial result exactly.
    #[handler::manual]
    fn on_apply_brush(&mut self, ctx: &mut WasmCtx<'_, Manual>, msg: ApplyBrush) {
        let execution = operator::apply_brush(&mut self.world, msg.source, &msg.path, msg.brush, msg.budget);
        self.remesh_touched(&execution.touched);
        if ctx.reply_target().is_some() {
            ctx.reply(&execution.result);
        }
    }

    /// Run a bounded iterative cell automaton and reply with exact accounting.
    /// Every accepted cell charges one step and `SUBCELLS_PER_CELL` subcells;
    /// the rejected over-cap cell is never mutated.
    ///
    /// # Agent
    /// Send `aether.kit.world.run_automaton` with a revisioned `source`, named
    /// seed cell, rule, and budget. The shared operator result deliberately
    /// says nothing about proposal/commit state; ADR-0143 owns that boundary.
    #[handler::manual]
    fn on_run_automaton(&mut self, ctx: &mut WasmCtx<'_, Manual>, msg: RunAutomaton) {
        let execution = operator::run_automaton(&mut self.world, msg.source, msg.seed, msg.rule, msg.budget);
        self.remesh_touched(&execution.touched);
        if ctx.reply_target().is_some() {
            ctx.reply(&execution.result);
        }
    }

    /// Stamp one cell's height deltas into the world, then remesh that cell's
    /// chunk and its eight cached neighbors — a border cell's relief feeds the
    /// neighbor's corner plates and walls through the same apron as a chunk
    /// write.
    ///
    /// # Agent
    /// Send `aether.kit.world.set_cell_heights` with a cell address (`x`,
    /// `z`) and up to SUB² `i16` octimeter deltas in `z*SUB + x` subcell
    /// order — each lifts (or drops) its subcell off the cell's `height`
    /// (`0` = no relief). A short vector leaves the cell's remaining points
    /// flat. `capture_frame` to verify the authored relief.
    #[handler::single]
    fn on_set_cell_heights(&mut self, _ctx: &mut WasmCtx<'_>, msg: SetCellHeights) {
        let cell = msg.cell();
        self.world.set_cell_heights(cell, &msg.deltas);
        self.remesh_around(cell.chunk());
    }

    /// Register a region in the world's table so the underlay cascade has
    /// a default to resolve to, then remesh every cached chunk — a region
    /// default can change the resolved underlay of any cell pointing at
    /// that region.
    #[handler::single]
    fn on_set_region(&mut self, _ctx: &mut WasmCtx<'_>, msg: SetRegion) {
        let id = msg.region_id;
        self.world.insert_region(id, msg.into_region());
        self.remesh_all();
    }

    /// Trigger an asynchronous world load. The reply arrives as
    /// `aether.fs.read_result`; the decode + swap happens there.
    /// Fire-and-forget — errors surface in logs, not a reply.
    ///
    /// # Agent
    /// `namespace` is the short prefix with no `://` (`"save"`,
    /// `"assets"`, `"config"`); `path` is the serialized world produced
    /// by the plane stack's world encoding.
    // The `&mut self` receiver is required by the `#[handler]` dispatch
    // ABI; this handler only issues a read and touches no state.
    #[allow(clippy::unused_self)]
    #[handler::single]
    fn on_load(&mut self, ctx: &mut WasmCtx<'_>, msg: WorldLoad) {
        let read = Read { namespace: msg.namespace.clone(), path: msg.path.clone() };
        let context = WorldLoadContext { namespace: msg.namespace, path: msg.path };
        tracing::info!(
            target: "aether_kit",
            namespace = %read.namespace,
            path = %read.path,
            "world load requested; issuing read",
        );
        let _ = ctx.actor::<FsCapability>().send_with_context(&read, &context);
    }

    /// Consume the `aether.fs` read reply. On `Ok`, decode the bytes with
    /// `World::from_bytes`; on success swap the world and remesh all. Any
    /// failure (read error or decode error) leaves the prior world intact
    /// with a warn log.
    ///
    /// # Agent
    /// Substrate-driven; do not send manually.
    #[handler::single]
    fn on_read_result(&mut self, ctx: &mut WasmCtx<'_>, result: ReadResult) {
        let Some(context) = ctx.take_context::<WorldLoadContext>() else {
            return;
        };
        match result {
            ReadResult::Ok { bytes, .. } => match World::from_bytes(&bytes) {
                Ok(world) => {
                    self.world = world;
                    self.remesh_all();
                    tracing::info!(
                        target: "aether_kit",
                        namespace = %context.namespace,
                        path = %context.path,
                        chunks = self.meshes.len(),
                        "world load complete; cache replaced",
                    );
                }
                Err(error) => tracing::warn!(
                    target: "aether_kit",
                    namespace = %context.namespace,
                    path = %context.path,
                    error = ?error,
                    "world decode failed; keeping prior world",
                ),
            },
            ReadResult::Err { error, .. } => tracing::warn!(
                target: "aether_kit",
                namespace = %context.namespace,
                path = %context.path,
                error = ?error,
                "world read failed; keeping prior world",
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mark::MarkGeometry;

    #[test]
    fn chunk_border_stamp_remeshes_both_touched_chunks() {
        let mut view = WorldView {
            world: World::new(),
            meshes: BTreeMap::new(),
            styles: StyleTable::default(),
            mark_overlay: MarkOverlayProjection::default(),
        };
        view.stamp_vertices(
            &[
                WorldPoint::new(4080, 256),
                WorldPoint::new(4112, 256),
                WorldPoint::new(4112, 512),
                WorldPoint::new(4080, 512),
            ],
            Material::Stone,
        );

        let west = ChunkPos { x: 0, z: 0 };
        let east = ChunkPos { x: 1, z: 0 };
        assert!(view.meshes.contains_key(&west), "west touched chunk remeshed");
        assert!(view.meshes.contains_key(&east), "east touched chunk remeshed");
        assert!(
            view.meshes.get(&west).is_some_and(|mesh| !mesh.is_empty()),
            "west mesh includes the border stamp and its east apron",
        );
        assert!(
            view.meshes.get(&east).is_some_and(|mesh| !mesh.is_empty()),
            "east mesh includes the border stamp and its west apron",
        );
    }

    fn projected_mark(id: u32, revision: u32) -> Mark {
        Mark {
            id: MarkId::new(id),
            revision,
            geometry: MarkGeometry::Point(WorldPoint::new(128, 128)),
            label: format!("mark-{id}"),
        }
    }

    #[test]
    fn overlay_projection_selection_is_revision_exact() {
        let mut projection = MarkOverlayProjection::default();
        assert_eq!(
            projection.set_visibility(true),
            SetMarkOverlayVisibilityResult { visible: true, synchronized: false }
        );
        projection.replace_snapshot(vec![projected_mark(1, 3), projected_mark(2, 1)]);
        let selected = MarkRef { id: MarkId::new(1), revision: 3 };
        assert_eq!(
            projection.set_selection(Some(selected)),
            SetMarkOverlaySelectionResult::Selected { reference: selected }
        );
        assert_eq!(projection.selected, Some(selected));

        let stale = MarkRef { id: MarkId::new(1), revision: 2 };
        assert_eq!(
            projection.set_selection(Some(stale)),
            SetMarkOverlaySelectionResult::Stale { requested: stale, current: selected }
        );
        assert_eq!(projection.selected, Some(selected));

        let ahead = MarkRef { id: MarkId::new(1), revision: 4 };
        assert_eq!(
            projection.set_selection(Some(ahead)),
            SetMarkOverlaySelectionResult::Unsynchronized { requested: ahead, cached: Some(selected) }
        );
        assert_eq!(projection.selected, Some(selected));

        projection.replace_snapshot(vec![projected_mark(1, 4), projected_mark(2, 1)]);
        assert_eq!(projection.selected, None, "a newer atomic snapshot clears the old highlighted revision");
    }

    #[test]
    fn overlay_projection_snapshot_removes_deleted_marks_and_hiding_clears_state() {
        let mut projection = MarkOverlayProjection::default();
        projection.set_visibility(true);
        projection.replace_snapshot(vec![projected_mark(1, 1), projected_mark(2, 1)]);
        assert_eq!(projection.marks.len(), 2);

        projection.replace_snapshot(vec![projected_mark(2, 1)]);
        assert!(!projection.marks.contains_key(&MarkId::new(1)));
        assert!(projection.synchronized);

        assert_eq!(
            projection.set_visibility(false),
            SetMarkOverlayVisibilityResult { visible: false, synchronized: false }
        );
        assert!(projection.marks.is_empty());
        assert_eq!(projection.selected, None);
        assert_eq!(
            projection.set_selection(Some(MarkRef { id: MarkId::new(2), revision: 1 })),
            SetMarkOverlaySelectionResult::Unsynchronized {
                requested: MarkRef { id: MarkId::new(2), revision: 1 },
                cached: None,
            }
        );
    }

    #[test]
    fn overlay_projection_bounds_retries_when_refresh_sends_are_dropped() {
        let mut projection = MarkOverlayProjection::default();
        projection.set_visibility(true);
        for expected_attempts in 1..=MARK_OVERLAY_REFRESH_ATTEMPT_LIMIT {
            assert!(projection.should_request_refresh());
            assert!(!projection.record_refresh_attempt(RequestId(Source::NO_CORRELATION)));
            assert_eq!(projection.refresh_attempts, expected_attempts);
            assert_eq!(projection.pending_request, None);
        }
        assert!(
            !projection.should_request_refresh(),
            "a missing MarkBook cannot produce an unbounded request every render"
        );

        projection.set_visibility(false);
        projection.set_visibility(true);
        assert_eq!(projection.refresh_attempts, 0);
        assert!(projection.should_request_refresh());
    }

    #[test]
    fn overlay_projection_ignores_late_reply_after_disable_and_clears_missing_context() {
        let mut projection = MarkOverlayProjection::default();
        projection.set_visibility(true);
        let first_generation = projection.generation;
        assert!(projection.record_refresh_attempt(RequestId(12)));

        projection.set_visibility(false);
        assert!(!projection.settle_refresh_reply(
            RequestId(12),
            Some(MarkOverlayRefreshContext { generation: first_generation }),
            vec![projected_mark(1, 1)],
        ));
        assert!(projection.marks.is_empty());

        projection.set_visibility(true);
        assert!(projection.record_refresh_attempt(RequestId(13)));
        assert!(
            !projection.settle_refresh_reply(
                RequestId(12),
                Some(MarkOverlayRefreshContext { generation: first_generation }),
                vec![projected_mark(1, 1)],
            ),
            "the disabled generation's late reply cannot replace the projection"
        );
        assert_eq!(projection.pending_request, Some(RequestId(13)));
        assert!(projection.marks.is_empty());

        assert!(
            !projection.settle_refresh_reply(RequestId(13), None, vec![projected_mark(2, 1)]),
            "a matching reply with missing typed context is rejected"
        );
        assert_eq!(
            projection.pending_request, None,
            "a matching reply with missing context cannot wedge future refreshes"
        );
        assert!(projection.should_request_refresh());
    }
}
