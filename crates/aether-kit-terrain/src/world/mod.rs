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
mod proposal;
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
use aether_component::ComponentHostCapability;
use aether_component::component::ComponentHostWasmExt;
use aether_data::Source;
use aether_fs::{FsCapability, Read, ReadResult};
use aether_kinds::Render;
use aether_lifecycle::{LifecycleCapability, LifecycleMailboxExt};
use aether_render::{DrawTriangle, RenderCapability};
use serde::{Deserialize, Serialize};

use crate::mark::{Mark, MarkBook, MarkId, MarkList, MarkListResult, MarkRef};

use self::mesher::mesh_chunk;
use self::mesher::style::StyleTable;

/// Default load name of the authoritative `MarkBook` peer.
const MARK_BOOK_COMPONENT: &str = "aether.kit.mark";
const MARK_OVERLAY_REFRESH_ATTEMPT_LIMIT: u8 = 3;
const MAX_STAGED_PROPOSALS: usize = 64;

/// World-view component: holds the world plane stack and a per-chunk
/// mesh cache, and replays the cache to the render sink each frame.
///
/// # Agent
/// Load with the `aether_kit_terrain@aether.kit.world` export. Paint the world by
/// sending `aether.kit.world.stamp_{polygon,disc,hexagon}` for compact smooth
/// shapes, bounded `apply_brush` / `run_automaton` requests for repeatable
/// mark-attributed generation, `aether.kit.world.set_chunk` for raw planes, and
/// `aether.kit.world.set_region` for an underlay cascade default; each send
/// remeshes and the world renders every frame under the active
/// `aether.view_projection` view. `aether.kit.world.load` swaps a serialized
/// world from `aether.fs`. Operator replies report an applied or exhausted
/// partial result; proposal/commit state is available through the typed
/// proposal lifecycle.
/// Use `capture_frame` to verify.
pub struct WorldView {
    world: World,
    meshes: BTreeMap<ChunkPos, Vec<DrawTriangle>>,
    styles: StyleTable,
    mark_overlay: MarkOverlayProjection,
    proposals: BTreeMap<ProposalId, proposal::StagedProposal>,
    active_preview: Option<ProposalId>,
    next_proposal_id: Option<u64>,
    committed_revision: u64,
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
    fn new() -> Self {
        Self {
            world: World::new(),
            meshes: BTreeMap::new(),
            styles: StyleTable::default(),
            mark_overlay: MarkOverlayProjection::default(),
            proposals: BTreeMap::new(),
            active_preview: None,
            next_proposal_id: Some(1),
            committed_revision: 0,
        }
    }

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
                target: "aether_kit_terrain",
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
        self.remesh_touched(&BTreeSet::from([pos]));
    }

    /// Rebuild every touched chunk and cached apron neighbor exactly once.
    /// Operators may touch adjacent chunks, whose 3×3 aprons overlap heavily;
    /// collecting first prevents repeated meshing while preserving the
    /// ordinary rule that an absent, untouched neighbor needs no cache entry.
    fn remesh_touched(&mut self, touched: &BTreeSet<ChunkPos>) {
        for pos in proposal::affected_cache_keys(touched, &self.meshes) {
            self.meshes.insert(pos, mesh_chunk(&self.world, pos, &self.styles));
        }
    }

    /// Finish one immediate mutation whose write path reports touched chunks.
    /// An empty set means the request wrote nothing, so it neither advances
    /// the committed revision nor clears a fresh proposal preview. Any
    /// non-empty set, including an operator's exhausted partial prefix,
    /// remeshes and advances exactly once.
    fn finish_touched_mutation(&mut self, touched: &BTreeSet<ChunkPos>) {
        if touched.is_empty() {
            return;
        }
        self.remesh_touched(touched);
        self.advance_committed_revision();
    }

    /// Rasterize `vertices` into the overlay coverage plane and rebuild every
    /// changed chunk through the ordinary apron-aware invalidation path.
    fn stamp_vertices(&mut self, vertices: &[WorldPoint], material: Material) {
        let touched = raster::stamp_polygon(&mut self.world, vertices, material);
        self.finish_touched_mutation(&touched);
    }

    fn advance_committed_revision(&mut self) {
        self.active_preview = None;
        if let Some(next) = self.committed_revision.checked_add(1) {
            self.committed_revision = next;
        } else {
            self.committed_revision = 0;
            self.proposals.clear();
        }
    }

    fn allocate_proposal_id(&mut self) -> Option<ProposalId> {
        let value = self.next_proposal_id?;
        self.next_proposal_id = value.checked_add(1);
        Some(ProposalId { value })
    }

    fn proposal_freshness_error(&self, proposal_id: ProposalId) -> Option<ProposalError> {
        let Some(proposal) = self.proposals.get(&proposal_id) else {
            return Some(ProposalError::UnknownProposal { proposal_id });
        };
        if proposal.proposed_at_revision != self.committed_revision {
            return Some(ProposalError::StaleProposal {
                proposal_id,
                proposed_at_revision: proposal.proposed_at_revision,
                committed_revision: self.committed_revision,
            });
        }
        None
    }

    fn propose(&mut self, operation: ProposalOperation) -> ProposalResult {
        let proposal = match proposal::StagedProposal::build(
            self.committed_revision,
            operation,
            &mut self.world,
            &self.meshes,
            &self.styles,
        ) {
            Ok(proposal) => proposal,
            Err(operation_result) => {
                return ProposalResult::Rejected { error: ProposalError::NoTouchedChunks { operation_result } };
            }
        };
        if self.proposals.len() >= MAX_STAGED_PROPOSALS {
            return ProposalResult::Rejected { error: ProposalError::StagedProposalLimitReached };
        }
        let Some(proposal_id) = self.allocate_proposal_id() else {
            return ProposalResult::Rejected { error: ProposalError::ProposalIdExhausted };
        };
        let operation_result = proposal.operation_result.clone();
        let digest = proposal.digest.clone();
        self.proposals.insert(proposal_id, proposal);
        ProposalResult::Staged { proposal_id, operation_result, digest }
    }

    fn commit_proposal(&mut self, proposal_id: ProposalId) -> ProposalResult {
        if let Some(error) = self.proposal_freshness_error(proposal_id) {
            return ProposalResult::Rejected { error };
        }
        let proposal = self.proposals.remove(&proposal_id).expect("freshness checked proposal presence");
        let touched = proposal.touched.clone();
        let digest = proposal.digest.clone();
        proposal.commit(&mut self.world);
        self.remesh_touched(&touched);
        self.advance_committed_revision();
        ProposalResult::Committed { proposal_id, digest }
    }

    fn discard_proposal(&mut self, proposal_id: ProposalId) -> ProposalResult {
        if self.proposals.remove(&proposal_id).is_none() {
            return ProposalResult::Rejected { error: ProposalError::UnknownProposal { proposal_id } };
        }
        if self.active_preview == Some(proposal_id) {
            self.active_preview = None;
        }
        ProposalResult::Discarded { proposal_id }
    }

    fn set_proposal_preview(&mut self, proposal_id: Option<ProposalId>) -> ProposalResult {
        let Some(proposal_id) = proposal_id else {
            self.active_preview = None;
            return ProposalResult::PreviewSet { active_proposal_id: None, digest: None };
        };
        if let Some(error) = self.proposal_freshness_error(proposal_id) {
            return ProposalResult::Rejected { error };
        }
        self.active_preview = Some(proposal_id);
        ProposalResult::PreviewSet {
            active_proposal_id: Some(proposal_id),
            digest: Some(self.proposals.get(&proposal_id).expect("freshness checked proposal presence").digest.clone()),
        }
    }

    fn rendered_meshes(&self) -> Vec<(ChunkPos, &[DrawTriangle])> {
        let Some(proposal) = self.active_preview.and_then(|proposal_id| self.proposals.get(&proposal_id)) else {
            return self.meshes.iter().map(|(at, mesh)| (*at, mesh.as_slice())).collect();
        };
        self.meshes
            .keys()
            .chain(proposal.affected.iter())
            .copied()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter_map(|at| {
                proposal.meshes.get(&at).or_else(|| self.meshes.get(&at)).map(|mesh| (at, mesh.as_slice()))
            })
            .collect()
    }
}

#[actor]
impl WasmActor for WorldView {
    const NAMESPACE: &'static str = "aether.kit.world";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self::new())
    }

    /// Subscribe the `Render` lifecycle stage so the cached meshes
    /// re-emit once per frame, after the `Tick` chain settles (ADR-0082
    /// §11) — the same render-replay placement as the camera / mesh
    /// viewer. The view has no per-tick compute; it only re-emits. On a
    /// chassis whose lifecycle graph omits `Render` (headless), the
    /// fire-and-forget subscribe warn-drops and the view simply never
    /// submits.
    fn wire(&mut self, ctx: &mut aether_actor::WireCtx<'_, '_>) {
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
        for (_, mesh) in self.rendered_meshes() {
            if !mesh.is_empty() {
                ctx.actor::<RenderCapability>().send_many(mesh);
            }
        }
        if self.mark_overlay.visible {
            let batch = if let Some(proposal) =
                self.active_preview.and_then(|proposal_id| self.proposals.get_mut(&proposal_id))
            {
                proposal.with_installed(&mut self.world, |world| {
                    overlay::mark_overlay_batch(world, &self.mark_overlay.marks, self.mark_overlay.selected)
                })
            } else {
                overlay::mark_overlay_batch(&self.world, &self.mark_overlay.marks, self.mark_overlay.selected)
            };
            if let Some(overflow) = batch.overflow
                && !self.mark_overlay.budget_overflowed
            {
                tracing::warn!(
                    target: "aether_kit_terrain",
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
        self.advance_committed_revision();
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
        self.advance_committed_revision();
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
        self.finish_touched_mutation(&execution.touched);
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
        self.finish_touched_mutation(&execution.touched);
        if ctx.reply_target().is_some() {
            ctx.reply(&execution.result);
        }
    }

    /// Stage one bounded terrain mutation against the current committed
    /// revision and return its exact operation result and geometry digest.
    #[handler::manual]
    fn on_propose(&mut self, ctx: &mut WasmCtx<'_, Manual>, msg: Propose) {
        let result = self.propose(msg.operation);
        if ctx.reply_target().is_some() {
            ctx.reply(&result);
        }
    }

    /// Install a fresh proposal atomically. Its peers remain stored but become
    /// stale against the newly advanced committed revision.
    #[handler::manual]
    fn on_commit_proposal(&mut self, ctx: &mut WasmCtx<'_, Manual>, msg: CommitProposal) {
        let result = self.commit_proposal(msg.proposal_id);
        if ctx.reply_target().is_some() {
            ctx.reply(&result);
        }
    }

    /// Drop a proposal regardless of whether its committed revision is still
    /// current. Unknown ids are rejected observably.
    #[handler::manual]
    fn on_discard_proposal(&mut self, ctx: &mut WasmCtx<'_, Manual>, msg: DiscardProposal) {
        let result = self.discard_proposal(msg.proposal_id);
        if ctx.reply_target().is_some() {
            ctx.reply(&result);
        }
    }

    /// Select one fresh proposal for rendering, replace the prior selection,
    /// or clear preview rendering with `None`.
    #[handler::manual]
    fn on_set_proposal_preview(&mut self, ctx: &mut WasmCtx<'_, Manual>, msg: SetProposalPreview) {
        let result = self.set_proposal_preview(msg.proposal_id);
        if ctx.reply_target().is_some() {
            ctx.reply(&result);
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
        self.advance_committed_revision();
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
        self.advance_committed_revision();
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
            target: "aether_kit_terrain",
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
                    self.advance_committed_revision();
                    tracing::info!(
                        target: "aether_kit_terrain",
                        namespace = %context.namespace,
                        path = %context.path,
                        chunks = self.meshes.len(),
                        "world load complete; cache replaced",
                    );
                }
                Err(error) => tracing::warn!(
                    target: "aether_kit_terrain",
                    namespace = %context.namespace,
                    path = %context.path,
                    error = ?error,
                    "world decode failed; keeping prior world",
                ),
            },
            ReadResult::Err { error, .. } => tracing::warn!(
                target: "aether_kit_terrain",
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

    fn test_view() -> WorldView {
        WorldView::new()
    }

    fn point_operation(cell_x: i32, material: Material) -> ProposalOperation {
        ProposalOperation::SetCellPoints {
            request: SetCellPoints { x: cell_x, z: 0, points: vec![material.to_u8(); SUBCELLS_PER_CELL] },
        }
    }

    fn staged_id(view: &mut WorldView, operation: ProposalOperation) -> ProposalId {
        match view.propose(operation) {
            ProposalResult::Staged { proposal_id, .. } => proposal_id,
            other => panic!("expected staged proposal, got {other:?}"),
        }
    }

    fn fill_proposal_capacity(view: &mut WorldView) -> Vec<ProposalId> {
        (0..MAX_STAGED_PROPOSALS)
            .map(|index| {
                staged_id(
                    view,
                    point_operation(i32::try_from(index).expect("proposal cap fits cell coordinates"), Material::Grass),
                )
            })
            .collect()
    }

    #[test]
    fn chunk_border_stamp_remeshes_both_touched_chunks() {
        let mut view = test_view();
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

    #[test]
    fn proposal_ids_start_at_one_use_max_and_then_exhaust_without_mutation() {
        let mut view = test_view();
        assert_eq!(staged_id(&mut view, point_operation(0, Material::Grass)), ProposalId { value: 1 });

        view.next_proposal_id = Some(u64::MAX);
        assert_eq!(staged_id(&mut view, point_operation(1, Material::Stone)), ProposalId { value: u64::MAX });
        let proposals_before = view.proposals.len();
        let world_before = view.world.clone();
        assert_eq!(
            view.propose(point_operation(2, Material::Sand)),
            ProposalResult::Rejected { error: ProposalError::ProposalIdExhausted }
        );
        assert_eq!(view.proposals.len(), proposals_before);
        assert_eq!(view.world, world_before);
        assert_eq!(view.active_preview, None);
    }

    #[test]
    fn staged_proposal_cap_rejects_without_allocating_or_mutating_session_state() {
        let mut view = test_view();
        let proposal_ids = fill_proposal_capacity(&mut view);
        let active = proposal_ids[0];
        view.set_proposal_preview(Some(active));
        let next_proposal_id = view.next_proposal_id;
        let world_before = view.world.clone();
        let meshes_before = view.meshes.clone();

        assert_eq!(
            view.propose(point_operation(1_000, Material::Stone)),
            ProposalResult::Rejected { error: ProposalError::StagedProposalLimitReached }
        );

        assert_eq!(view.proposals.len(), MAX_STAGED_PROPOSALS);
        assert_eq!(view.next_proposal_id, next_proposal_id);
        assert_eq!(view.world, world_before);
        assert_eq!(view.meshes, meshes_before);
        assert_eq!(view.active_preview, Some(active));
        assert_eq!(view.proposal_freshness_error(active), None);
    }

    #[test]
    fn staged_proposal_capacity_recovers_after_discard_and_commit() {
        let mut view = test_view();
        let proposal_ids = fill_proposal_capacity(&mut view);
        let next_after_fill = view.next_proposal_id;

        assert_eq!(view.discard_proposal(proposal_ids[0]), ProposalResult::Discarded { proposal_id: proposal_ids[0] });
        let after_discard = staged_id(&mut view, point_operation(1_000, Material::Stone));
        assert_eq!(after_discard.value.checked_add(1), view.next_proposal_id);
        assert_eq!(view.proposals.len(), MAX_STAGED_PROPOSALS);
        assert_eq!(Some(after_discard.value), next_after_fill);

        assert!(matches!(
            view.commit_proposal(proposal_ids[1]),
            ProposalResult::Committed { proposal_id, .. } if proposal_id == proposal_ids[1]
        ));
        assert_eq!(view.proposals.len(), MAX_STAGED_PROPOSALS - 1);
        let after_commit = staged_id(&mut view, point_operation(1_001, Material::Sand));
        assert_eq!(after_commit.value.checked_add(1), view.next_proposal_id);
        assert_eq!(view.proposals.len(), MAX_STAGED_PROPOSALS);
    }

    #[test]
    fn no_touch_rejection_does_not_consume_an_id() {
        let mut view = test_view();
        let result = view.propose(ProposalOperation::StampDisc {
            request: StampDisc {
                center: WorldPoint::new(0, 0),
                radius_octimeters: 0,
                material: Material::Stone.to_u8(),
            },
        });
        assert_eq!(
            result,
            ProposalResult::Rejected {
                error: ProposalError::NoTouchedChunks { operation_result: ProposalOperationResult::Mutation }
            }
        );
        assert_eq!(view.next_proposal_id, Some(1));
        assert!(view.proposals.is_empty());
    }

    #[test]
    fn no_touch_direct_stamp_preserves_revision_preview_and_proposal_freshness() {
        let mut view = test_view();
        let proposal_id = staged_id(&mut view, point_operation(0, Material::Grass));
        assert!(matches!(
            view.set_proposal_preview(Some(proposal_id)),
            ProposalResult::PreviewSet { active_proposal_id: Some(id), .. } if id == proposal_id
        ));
        let world_before = view.world.clone();
        let meshes_before = view.meshes.clone();

        let vertices = raster::disc_vertices(WorldPoint::new(0, 0), 0);
        view.stamp_vertices(&vertices, Material::Stone);

        assert_eq!(view.world, world_before);
        assert_eq!(view.meshes, meshes_before);
        assert_eq!(view.committed_revision, 0);
        assert_eq!(view.active_preview, Some(proposal_id));
        assert_eq!(view.proposal_freshness_error(proposal_id), None);
    }

    #[test]
    fn touched_partial_operator_write_advances_revision_exactly_once() {
        let mut view = test_view();
        let proposal_id = staged_id(&mut view, point_operation(8, Material::Grass));
        view.set_proposal_preview(Some(proposal_id));
        let source = MarkRef { id: MarkId::new(17), revision: 2 };
        let execution = operator::run_automaton(
            &mut view.world,
            source,
            OperatorCell { cell_x: 0, cell_z: 0 },
            AutomatonRule::Grow { material: Material::Sand.to_u8(), generations: 1 },
            OperatorBudget {
                max_steps: 2,
                max_subcells: 2 * u32::try_from(SUBCELLS_PER_CELL).expect("fixed cell plane fits u32"),
            },
        );
        assert!(matches!(
            execution.result,
            OperatorResult::Failed {
                error: OperatorError::StepBudgetExhausted,
                stats: OperatorStats { steps_run: 2, .. },
                ..
            }
        ));
        assert!(!execution.touched.is_empty());

        view.finish_touched_mutation(&execution.touched);

        assert_eq!(view.committed_revision, 1);
        assert_eq!(view.active_preview, None);
        assert!(matches!(
            view.proposal_freshness_error(proposal_id),
            Some(ProposalError::StaleProposal { proposed_at_revision: 0, committed_revision: 1, .. })
        ));
        assert_eq!(view.world.underlay_point(CellPos { x: 0, z: 0 }, 0, 0), Material::Sand);
        assert_eq!(view.world.underlay_point(CellPos { x: 1, z: 0 }, 0, 0), Material::Sand);
    }

    #[test]
    fn first_commit_matches_preview_meshes_and_makes_its_peer_stale() {
        let mut view = test_view();
        let committed_before = view.world.clone();
        let first = staged_id(&mut view, point_operation(0, Material::Grass));
        let peer = staged_id(&mut view, point_operation(1, Material::Stone));
        assert_eq!(view.world, committed_before, "staging peers leaves committed terrain unchanged");
        let preview_meshes = view.proposals.get(&first).expect("first proposal").meshes.clone();
        let first_digest = view.proposals.get(&first).expect("first proposal").digest.clone();
        assert!(matches!(
            view.set_proposal_preview(Some(first)),
            ProposalResult::PreviewSet { active_proposal_id: Some(id), .. } if id == first
        ));

        assert_eq!(view.commit_proposal(first), ProposalResult::Committed { proposal_id: first, digest: first_digest });
        for (at, preview) in preview_meshes {
            assert_eq!(view.meshes.get(&at), Some(&preview), "committed cache is byte-identical to preview mesh");
        }
        assert_eq!(view.active_preview, None);

        let world_after_first = view.world.clone();
        let meshes_after_first = view.meshes.clone();
        let stale = ProposalError::StaleProposal { proposal_id: peer, proposed_at_revision: 0, committed_revision: 1 };
        assert_eq!(view.commit_proposal(peer), ProposalResult::Rejected { error: stale.clone() });
        assert_eq!(view.set_proposal_preview(Some(peer)), ProposalResult::Rejected { error: stale });
        assert_eq!(view.world, world_after_first, "stale rejection does not mutate committed terrain");
        assert_eq!(view.meshes, meshes_after_first, "stale rejection does not mutate the cache");
        assert!(view.proposals.contains_key(&peer), "stale proposal remains available for discard");
        assert_eq!(view.discard_proposal(peer), ProposalResult::Discarded { proposal_id: peer });
    }

    #[test]
    fn direct_write_and_successful_load_advance_revision_and_clear_preview() {
        let mut view = test_view();
        let direct_peer = staged_id(&mut view, point_operation(0, Material::Grass));
        view.set_proposal_preview(Some(direct_peer));
        view.world.set_cell_points(CellPos { x: 4, z: 0 }, &[Material::Sand.to_u8()]);
        view.remesh_around(ChunkPos { x: 0, z: 0 });
        view.advance_committed_revision();
        assert_eq!(view.committed_revision, 1);
        assert_eq!(view.active_preview, None);
        assert!(matches!(
            view.commit_proposal(direct_peer),
            ProposalResult::Rejected { error: ProposalError::StaleProposal { committed_revision: 1, .. } }
        ));

        let load_peer = staged_id(&mut view, point_operation(5, Material::Stone));
        view.set_proposal_preview(Some(load_peer));
        view.world = World::new();
        view.remesh_all();
        view.advance_committed_revision();
        assert_eq!(view.committed_revision, 2);
        assert_eq!(view.active_preview, None);
        assert!(matches!(
            view.set_proposal_preview(Some(load_peer)),
            ProposalResult::Rejected { error: ProposalError::StaleProposal { committed_revision: 2, .. } }
        ));
    }

    #[test]
    fn preview_switch_clear_unknown_and_stale_discard_are_observable() {
        let mut view = test_view();
        let first = staged_id(&mut view, point_operation(0, Material::Grass));
        let second = staged_id(&mut view, point_operation(1, Material::Stone));
        view.set_proposal_preview(Some(first));
        assert_eq!(view.active_preview, Some(first));
        view.set_proposal_preview(Some(second));
        assert_eq!(view.active_preview, Some(second), "a new preview replaces the old one");
        assert_eq!(
            view.set_proposal_preview(None),
            ProposalResult::PreviewSet { active_proposal_id: None, digest: None }
        );
        assert_eq!(view.active_preview, None);

        let unknown = ProposalId { value: 999 };
        assert_eq!(
            view.discard_proposal(unknown),
            ProposalResult::Rejected { error: ProposalError::UnknownProposal { proposal_id: unknown } }
        );
        view.advance_committed_revision();
        assert_eq!(view.discard_proposal(first), ProposalResult::Discarded { proposal_id: first });
    }

    #[test]
    fn revision_rollover_clears_every_proposal_and_preview_without_reusing_ids() {
        let mut view = test_view();
        let proposal_id = staged_id(&mut view, point_operation(0, Material::Grass));
        view.set_proposal_preview(Some(proposal_id));
        let next_id = view.next_proposal_id;
        view.committed_revision = u64::MAX;
        view.advance_committed_revision();
        assert_eq!(view.committed_revision, 0);
        assert!(view.proposals.is_empty());
        assert_eq!(view.active_preview, None);
        assert_eq!(view.next_proposal_id, next_id, "revision rollover does not alias session proposal ids");
    }

    #[test]
    fn preview_render_merge_substitutes_affected_keys_in_global_sorted_order() {
        let mut view = test_view();
        view.meshes.insert(ChunkPos { x: -2, z: 0 }, Vec::new());
        view.meshes.insert(ChunkPos { x: 3, z: 0 }, Vec::new());
        let proposal_id = staged_id(&mut view, point_operation(0, Material::Grass));
        view.set_proposal_preview(Some(proposal_id));
        let keys: Vec<ChunkPos> = view.rendered_meshes().into_iter().map(|(at, _)| at).collect();
        let mut sorted = keys.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(keys, sorted);
        assert!(keys.contains(&ChunkPos { x: 0, z: 0 }));
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
    fn visible_mark_heights_resolve_through_the_active_proposal() {
        let mut view = test_view();
        let mut chunk = Chunk::empty();
        chunk.underlay.fill(Material::Grass);
        view.world.insert_chunk(ChunkPos { x: 0, z: 0 }, chunk);
        view.remesh_all();
        view.mark_overlay.visible = true;
        view.mark_overlay.marks.insert(MarkId::new(1), projected_mark(1, 1));
        let committed = overlay::mark_overlay_batch(&view.world, &view.mark_overlay.marks, None);

        let proposal_id = staged_id(
            &mut view,
            ProposalOperation::SetCellHeights {
                request: SetCellHeights { x: 0, z: 0, deltas: vec![128; SUBCELLS_PER_CELL] },
            },
        );
        let preview =
            view.proposals.get_mut(&proposal_id).expect("height proposal").with_installed(&mut view.world, |world| {
                overlay::mark_overlay_batch(world, &view.mark_overlay.marks, None)
            });
        let committed_y =
            committed.triangles.iter().flat_map(|triangle| triangle.verts).map(|vertex| vertex.y).sum::<f32>();
        let preview_y =
            preview.triangles.iter().flat_map(|triangle| triangle.verts).map(|vertex| vertex.y).sum::<f32>();
        assert!(preview_y > committed_y, "mark vertices sample the staged height surface");
        assert_eq!(view.world.surface_height(0.5, 0.5), 0.0, "temporary mark sampling restores committed terrain");
    }

    #[test]
    fn fresh_component_init_drops_proposals_preview_revision_and_allocator_state() {
        let mut old = test_view();
        let old_id = staged_id(&mut old, point_operation(0, Material::Grass));
        old.set_proposal_preview(Some(old_id));
        old.advance_committed_revision();

        let replacement = test_view();
        assert_eq!(replacement.committed_revision, 0);
        assert_eq!(replacement.next_proposal_id, Some(1));
        assert!(replacement.proposals.is_empty());
        assert_eq!(replacement.active_preview, None);
        assert_eq!(
            replacement.proposal_freshness_error(old_id),
            Some(ProposalError::UnknownProposal { proposal_id: old_id }),
            "an old session id is unknown before the replacement allocates any new id",
        );
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
