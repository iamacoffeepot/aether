// `#[handler]` methods take the decoded mail by value per the ADR-0033
// dispatch ABI; the macro-generated trampoline owns the payload.
#![allow(clippy::needless_pass_by_value)]

//! World-view runtime. Meshes the chunked plane stack
//! ([`crate::world`]) into ground geometry and replays the cached
//! per-chunk mesh to `"aether.render"` each frame on the `Render`
//! lifecycle stage.
//!
//! The mesher lives in [`mesher`] as a pure function
//! ([`mesh_chunk`]); this actor keeps the per-chunk mesh cache, the active
//! view mode, and the cache invalidation. Each chunk becomes keyed-quilt
//! underlay cells (flat world-anchored color with pooled rims and a wash
//! gradient) and corner-minimized overlay contours over the subcell masks,
//! in world-space meters (`1 cell = 1 m`) with the existing `aether.view_projection`
//! `view_proj` handling projection. A chunk's rims and contours read a
//! bounded apron into its neighbors, so a write invalidates its own mesh
//! and its eight cached neighbors.
//!
//! # Mail surface
//!
//! - `aether.kit.world.set_chunk` — write one chunk's planes and remesh
//!   that chunk plus its eight cached neighbors (their border rims and
//!   contours read the new planes through the apron).
//! - `aether.kit.world.set_cell_points` — stamp one cell's underlay
//!   material points and remesh that cell's chunk plus its eight cached
//!   neighbors (the single-cell live-paint counterpart to `set_chunk`).
//! - `aether.kit.world.set_cell_heights` — stamp one cell's height deltas
//!   (subcell relief off the cell height) and remesh that cell's chunk plus
//!   its eight cached neighbors (the height sibling of `set_cell_points`).
//! - `aether.kit.world.set_region` — register a region so the underlay
//!   cascade has a default to resolve to; remeshes every cached chunk
//!   (a region default can change any chunk's cascade-resolved underlay).
//! - `aether.kit.world.set_smoothing_profile` — register a contour-
//!   smoothing profile the per-cell smoothing plane points at; remeshes
//!   every cached chunk.
//! - `aether.kit.world.set_water_plane` — register a water plane so water
//!   cells pointing at it resolve their flat surface level; remeshes every
//!   cached chunk (retuning a level restyles every referencing water body).
//! - `aether.kit.world.set_material_style` — write a material's complete
//!   live style row (base HSL, noise shape, smoothing defaults, rim /
//!   wash / water tunables) and remesh every cached chunk, so a tuning
//!   pass needs no rebuild. An undecodable or `Void` material byte is
//!   rejected with a warn log and leaves the table untouched.
//! - `aether.kit.world.set_view_mode` — switch between the painted gouache
//!   grammar and the raw grayscale calibration field; remeshes all.
//! - `aether.kit.world.load` — fetch a serialized world through
//!   `aether.fs`, decode, atomically swap, and remesh all. A decode or
//!   read failure keeps the prior world (errors go to logs).

mod data;
pub use data::*;
mod kinds;
pub use kinds::*;
pub mod mesher;

use alloc::collections::BTreeMap;

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_capabilities::fs::{Read, ReadResult};
use aether_capabilities::lifecycle::LifecycleMailboxExt;
use aether_capabilities::render::DrawTriangle;
use aether_capabilities::{FsCapability, LifecycleCapability, RenderCapability};
use aether_kinds::Render;
use serde::{Deserialize, Serialize};

use self::mesher::mesh_chunk;
use self::mesher::style::StyleTable;

/// World-view component: holds the world plane stack and a per-chunk
/// mesh cache, and replays the cache to the render sink each frame.
///
/// # Agent
/// Load with the `aether_kit@aether.kit.world` export. Paint the world by
/// sending `aether.kit.world.set_chunk` (one chunk's planes) and
/// `aether.kit.world.set_region` (a region default for the underlay
/// cascade); each send remeshes and the meadow renders every frame under
/// the active `aether.view_projection` view. `aether.kit.world.set_view_mode`
/// toggles the raw grayscale field for calibrating the material table;
/// `aether.kit.world.load` swaps a serialized world from `aether.fs`. Use
/// `capture_frame` to verify.
pub struct WorldView {
    world: World,
    meshes: BTreeMap<ChunkPos, Vec<DrawTriangle>>,
    mode: ViewMode,
    styles: StyleTable,
}

#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.world.load_context")]
struct WorldLoadContext {
    namespace: String,
    path: String,
}

impl WorldView {
    /// Rebuild every chunk's cached mesh from the current world — used
    /// after a whole-world change (region default, world load, view mode,
    /// material style) that can alter any chunk's mesh.
    fn remesh_all(&mut self) {
        self.meshes.clear();
        let positions: Vec<ChunkPos> = self.world.chunks().map(|(pos, _)| pos).collect();
        for pos in positions {
            self.meshes
                .insert(pos, mesh_chunk(&self.world, pos, self.mode, &self.styles));
        }
    }

    /// Remesh `pos` and its eight cached neighbors after a write inside
    /// `pos`. The mesher's rims and contours read a bounded apron into the
    /// neighbors, so a write changes the border geometry of any cached
    /// neighbor as well as `pos`'s own mesh. Neighbors with no cached mesh
    /// are not rendered, so they need no remesh; an empty neighbor's border
    /// geometry is already covered by this chunk's own apron windows.
    fn remesh_around(&mut self, pos: ChunkPos) {
        for dz in -1..=1 {
            for dx in -1..=1 {
                let neighbor = ChunkPos {
                    x: pos.x + dx,
                    z: pos.z + dz,
                };
                if (dx == 0 && dz == 0) || self.meshes.contains_key(&neighbor) {
                    self.meshes.insert(
                        neighbor,
                        mesh_chunk(&self.world, neighbor, self.mode, &self.styles),
                    );
                }
            }
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
            mode: ViewMode::default(),
            styles: StyleTable::default(),
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
        for mesh in self.meshes.values() {
            if !mesh.is_empty() {
                ctx.actor::<RenderCapability>().send_many(mesh);
            }
        }
    }

    /// Write one chunk's planes into the world, then remesh that chunk and
    /// its eight cached neighbors. The mesher's rims and overlay contours
    /// read a bounded apron into the neighbors, so a write changes the
    /// border rims and edge contours of any cached neighbor as well as the
    /// written chunk's own mesh. Neighbors with no cached mesh are not
    /// rendered, so they need no remesh; an empty neighbor's border
    /// geometry is already covered by this chunk's own apron windows.
    #[handler::single]
    fn on_set_chunk(&mut self, _ctx: &mut WasmCtx<'_>, msg: SetChunk) {
        let pos = msg.chunk_pos();
        self.world.insert_chunk(pos, msg.into_chunk());
        self.remesh_around(pos);
    }

    /// Stamp one cell's underlay material points into the world, then remesh
    /// that cell's chunk and its eight cached neighbors — a border cell's
    /// points feed the neighbor's rims and contours through the same apron
    /// as a chunk write.
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

    /// Switch the view between the painted gouache grammar and the raw
    /// grayscale field, then remesh every cached chunk — the mode changes
    /// how every cell paints. A repeat of the current mode still remeshes.
    ///
    /// # Agent
    /// Send `aether.kit.world.set_view_mode` with `mode = 0` for the
    /// painted look or `mode = 1` for the raw hue-noise field, used to
    /// calibrate the material table by eye. `capture_frame` to compare.
    #[handler::single]
    fn on_set_view_mode(&mut self, _ctx: &mut WasmCtx<'_>, msg: SetViewMode) {
        self.mode = msg.view_mode();
        self.remesh_all();
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

    /// Register a contour-smoothing profile in the world's table, then
    /// remesh every cached chunk — a profile change alters the smoothed
    /// contour of every cell whose smoothing plane points at it.
    ///
    /// # Agent
    /// Send `aether.kit.world.set_smoothing_profile` with a 1-based
    /// `profile_id`, an iteration count (`0` = crisp raw contours, up to
    /// `4`), and the corner angle in degrees (`45`–`90`). Cells opt in
    /// through the `smoothing` plane of `aether.kit.world.set_chunk`
    /// (byte = profile id, `0` = the material default).
    #[handler::single]
    fn on_set_smoothing_profile(&mut self, _ctx: &mut WasmCtx<'_>, msg: SetSmoothingProfile) {
        self.world
            .insert_smoothing_profile(msg.profile_id, msg.profile());
        self.remesh_all();
    }

    /// Register a water plane in the world's table, then remesh every cached
    /// chunk — the level sets the flat surface of every water cell pointing
    /// at this plane, so retuning it repaints all of them.
    ///
    /// # Agent
    /// Send `aether.kit.world.set_water_plane` with a 1-based `plane_id` and
    /// a `level_octimeters`. Point water cells at it through the
    /// `water_plane` plane of `aether.kit.world.set_chunk` (id = plane,
    /// `0` = the datum-0 level). One write retunes a whole lake live; use
    /// `capture_frame` to verify.
    #[handler::single]
    fn on_set_water_plane(&mut self, _ctx: &mut WasmCtx<'_>, msg: SetWaterPlane) {
        self.world.insert_water_plane(msg.plane_id, msg.plane());
        self.remesh_all();
    }

    /// Write a material's complete live style row, then remesh every
    /// cached chunk — a style change alters the resolved color, noise
    /// field, and smoothing defaults of every cell of that material. An
    /// undecodable or `Void` material byte is rejected with a warn log and
    /// leaves the table untouched.
    ///
    /// # Agent
    /// Send `aether.kit.world.set_material_style` with a raw `material`
    /// byte (`1` Grass … `5` Water) and every `MaterialStyle` field — it is
    /// a full-row write, not a delta. Tune against `capture_frame`, and
    /// switch to `aether.kit.world.set_view_mode`'s raw mode to read the
    /// noise field directly. Tuned values are session-scoped; commit the
    /// judged row back into `mesher::style`'s const table as the new
    /// default once satisfied.
    #[handler::single]
    fn on_set_material_style(&mut self, _ctx: &mut WasmCtx<'_>, msg: SetMaterialStyle) {
        match Material::try_from(msg.material) {
            Ok(Material::Void) | Err(_) => {
                tracing::warn!(
                    target: "aether_kit",
                    material = msg.material,
                    "set_material_style: undecodable or Void material byte; table unchanged",
                );
            }
            Ok(_) => {
                self.styles.apply(&msg);
                self.remesh_all();
            }
        }
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
        let read = Read {
            namespace: msg.namespace.clone(),
            path: msg.path.clone(),
        };
        let context = WorldLoadContext {
            namespace: msg.namespace,
            path: msg.path,
        };
        tracing::info!(
            target: "aether_kit",
            namespace = %read.namespace,
            path = %read.path,
            "world load requested; issuing read",
        );
        let _ = ctx
            .actor::<FsCapability>()
            .send_with_context(&read, &context);
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
