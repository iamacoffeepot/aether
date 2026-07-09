// `#[handler]` methods take the decoded mail by value per the ADR-0033
// dispatch ABI; the macro-generated trampoline owns the payload.
#![allow(clippy::needless_pass_by_value)]

//! World-view runtime. Meshes the chunked plane stack
//! ([`crate::world`]) into ground geometry, uploads painted overlay
//! material mask planes as render textures, and replays both cached surfaces
//! to `"aether.render"` each frame on the `Render` lifecycle stage.
//!
//! The mesher lives in [`mesher`] as pure functions ([`mesh_chunk`] for
//! terrain triangles and [`mesher::prepare_material_mask_plane`] for overlay
//! sample fields); this actor keeps the per-chunk mesh cache, the
//! per-(chunk, material) R8 material-mask texture cache, the active view mode,
//! and the cache invalidation. Each chunk becomes keyed-quilt underlay
//! cells (flat world-anchored color with pooled rims and a wash gradient),
//! while painted overlays render as depth-tested coverage material rects
//! sampling the prepared plane. A chunk's rims and material mask planes read a
//! bounded apron into its neighbors, so a write invalidates its own cache
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

use aether_actor::{ActorInitError, RequestId, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_capabilities::fs::{Read, ReadResult};
use aether_capabilities::lifecycle::LifecycleMailboxExt;
use aether_capabilities::render::{
    CreateTexture, CreateTextureResult, DrawMaterialCoverage, DrawTriangle, MaterialCoverageRect,
    MaterialRect, TextureFormat, UpdateTexture,
};
use aether_capabilities::{FsCapability, LifecycleCapability, RenderCapability};
use aether_kinds::Render;
use serde::{Deserialize, Serialize};

use self::mesher::style::{StyleTable, hsl_to_linear_rgb};
use self::mesher::{MaterialMaskPlane, mesh_chunk, overlay_materials, prepare_material_mask_plane};

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
    material_mask_planes: BTreeMap<MaterialMaskPlaneKey, MaterialMaskPlaneEntry>,
    mode: ViewMode,
    styles: StyleTable,
}

#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.world.load_context")]
struct WorldLoadContext {
    namespace: String,
    path: String,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct MaterialMaskPlaneKey {
    chunk: ChunkPos,
    material: u8,
}

#[derive(Clone)]
struct MaterialMaskPlaneEntry {
    plane: MaterialMaskPlane,
    texture: MaterialMaskTextureState,
}

#[derive(Clone, Copy, Debug)]
enum MaterialMaskTextureState {
    Pending(RequestId),
    Ready(u32),
    Failed,
}

#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.world.material_mask_texture_context")]
struct MaterialMaskTextureContext {
    chunk_x: i32,
    chunk_z: i32,
    material: u8,
}

/// Convert the CPU rim inset into the coverage shader's normalized
/// coverage-distance band. The texture is an R8 normalized field; a
/// nominal one-sample coverage ramp spans 1.0 coverage fraction.
const MATERIAL_MASK_RIM_FRACTION_PER_SAMPLE: f32 = 1.0;

impl WorldView {
    /// Rebuild every chunk's cached mesh from the current world — used
    /// after a whole-world change (region default, world load, view mode,
    /// material style) that can alter any chunk's mesh.
    fn remesh_all(&mut self, ctx: &mut WasmCtx<'_>) {
        self.meshes.clear();
        let positions: Vec<ChunkPos> = self.world.chunks().map(|(pos, _)| pos).collect();
        for pos in positions {
            self.meshes
                .insert(pos, mesh_chunk(&self.world, pos, self.mode, &self.styles));
        }
        self.sync_material_mask_planes(ctx);
    }

    /// Remesh `pos` and its eight cached neighbors after a write inside
    /// `pos`. The mesher's rims and contours read a bounded apron into the
    /// neighbors, so a write changes the border geometry of any cached
    /// neighbor as well as `pos`'s own mesh. Neighbors with no cached mesh
    /// are not rendered, so they need no remesh; an empty neighbor's border
    /// geometry is already covered by this chunk's own apron windows.
    fn remesh_around(&mut self, ctx: &mut WasmCtx<'_>, pos: ChunkPos) {
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
        self.sync_material_mask_planes(ctx);
    }

    fn sync_material_mask_planes(&mut self, ctx: &mut WasmCtx<'_>) {
        if self.mode != ViewMode::Painted {
            self.material_mask_planes.clear();
            return;
        }

        let mut expected = Vec::new();
        let positions: Vec<ChunkPos> = self.meshes.keys().copied().collect();
        for chunk in positions {
            for material in overlay_materials(&self.world, chunk) {
                let key = MaterialMaskPlaneKey {
                    chunk,
                    material: material as u8,
                };
                expected.push(key);
                let plane = prepare_material_mask_plane(&self.world, chunk, material, &self.styles);
                self.sync_material_mask_plane(ctx, key, plane);
            }
        }
        self.material_mask_planes
            .retain(|key, _| expected.iter().any(|expected| expected == key));
    }

    fn sync_material_mask_plane(
        &mut self,
        ctx: &mut WasmCtx<'_>,
        key: MaterialMaskPlaneKey,
        plane: MaterialMaskPlane,
    ) {
        let Some(entry) = self.material_mask_planes.get_mut(&key) else {
            let request = send_create_material_mask_texture(ctx, key, &plane);
            self.material_mask_planes.insert(
                key,
                MaterialMaskPlaneEntry {
                    plane,
                    texture: MaterialMaskTextureState::Pending(request),
                },
            );
            return;
        };

        let changed = entry.plane.samples != plane.samples
            || entry.plane.width != plane.width
            || entry.plane.height != plane.height;
        entry.plane = plane;
        if changed && let MaterialMaskTextureState::Ready(texture_id) = entry.texture {
            send_update_material_mask_texture(ctx, texture_id, &entry.plane);
        }
    }

    fn coverage_rect(
        &self,
        key: MaterialMaskPlaneKey,
        entry: &MaterialMaskPlaneEntry,
    ) -> Option<MaterialCoverageRect> {
        let MaterialMaskTextureState::Ready(_) = entry.texture else {
            return None;
        };
        let material = entry.plane.material;
        let style = self.styles.get(material);
        let body = hsl_to_linear_rgb(style.base_hue, style.base_sat, style.base_light);
        let rim = hsl_to_linear_rgb(
            style.base_hue,
            style.base_sat,
            style.base_light * (1.0 - style.rim_darken),
        );
        let origin_x = octimeters_to_meters(
            entry.plane.placement.origin_oct[0] - entry.plane.step_octimeters / 2,
        );
        let origin_z = octimeters_to_meters(
            entry.plane.placement.origin_oct[1] - entry.plane.step_octimeters / 2,
        );
        let width = material_mask_extent_meters(entry.plane.width, entry.plane.step_octimeters);
        let height = material_mask_extent_meters(entry.plane.height, entry.plane.step_octimeters);
        let layer = chunk_high_plane(&self.world, key.chunk) + 3.0 / 256.0;
        let rim_width =
            material_mask_rim_width(style.rim_inset_octimeters, entry.plane.step_octimeters);

        Some(MaterialCoverageRect {
            rect: MaterialRect {
                x: origin_x,
                y: origin_z,
                width,
                height,
                z: layer,
            },
            body_color: [body[0], body[1], body[2], 1.0],
            rim_color: [rim[0], rim[1], rim[2], 1.0],
            rim_width,
        })
    }
}

#[allow(clippy::cast_precision_loss)]
fn octimeters_to_meters(octimeters: i32) -> f32 {
    octimeters as f32 / 256.0
}

#[allow(clippy::cast_precision_loss)]
fn material_mask_extent_meters(samples: usize, step_octimeters: i32) -> f32 {
    samples as f32 * step_octimeters as f32 / 256.0
}

#[allow(clippy::cast_precision_loss)]
fn material_mask_rim_width(rim_inset_octimeters: i32, step_octimeters: i32) -> f32 {
    (rim_inset_octimeters as f32 / step_octimeters as f32) * MATERIAL_MASK_RIM_FRACTION_PER_SAMPLE
}

fn send_create_material_mask_texture(
    ctx: &mut WasmCtx<'_>,
    key: MaterialMaskPlaneKey,
    plane: &MaterialMaskPlane,
) -> RequestId {
    let create = CreateTexture {
        width: plane
            .width
            .try_into()
            .expect("material mask plane width fits u32"),
        height: plane
            .height
            .try_into()
            .expect("material mask plane height fits u32"),
        format: TextureFormat::R8,
        pixels: plane.samples.clone(),
    };
    let context = MaterialMaskTextureContext {
        chunk_x: key.chunk.x,
        chunk_z: key.chunk.z,
        material: key.material,
    };
    ctx.actor::<RenderCapability>()
        .send_with_context(&create, &context)
}

fn send_update_material_mask_texture(
    ctx: &mut WasmCtx<'_>,
    texture_id: u32,
    plane: &MaterialMaskPlane,
) {
    ctx.actor::<RenderCapability>().send(&UpdateTexture {
        texture_id,
        x: 0,
        y: 0,
        width: plane
            .width
            .try_into()
            .expect("material mask plane width fits u32"),
        height: plane
            .height
            .try_into()
            .expect("material mask plane height fits u32"),
        pixels: plane.samples.clone(),
    });
}

fn chunk_high_plane(world: &World, chunk: ChunkPos) -> f32 {
    let base_x = chunk.x * CELLS_PER_CHUNK;
    let base_z = chunk.z * CELLS_PER_CHUNK;
    let mut highest = 0.0_f32;
    for z in 0..CELLS_PER_CHUNK {
        for x in 0..CELLS_PER_CHUNK {
            let cell = CellPos {
                x: base_x + x,
                z: base_z + z,
            };
            for corner in world.cell_corner_heights(cell) {
                highest = highest.max(corner);
            }
        }
    }
    highest
}

#[actor]
impl WasmActor for WorldView {
    const NAMESPACE: &'static str = "aether.kit.world";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(WorldView {
            world: World::new(),
            meshes: BTreeMap::new(),
            material_mask_planes: BTreeMap::new(),
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
        if self.mode == ViewMode::Painted {
            for (key, entry) in &self.material_mask_planes {
                let MaterialMaskTextureState::Ready(texture_id) = entry.texture else {
                    continue;
                };
                let Some(rect) = self.coverage_rect(*key, entry) else {
                    continue;
                };
                ctx.actor::<RenderCapability>().send(&DrawMaterialCoverage {
                    texture_id,
                    rects: vec![rect],
                });
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
    fn on_set_chunk(&mut self, ctx: &mut WasmCtx<'_>, msg: SetChunk) {
        let pos = msg.chunk_pos();
        self.world.insert_chunk(pos, msg.into_chunk());
        self.remesh_around(ctx, pos);
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
    fn on_set_cell_points(&mut self, ctx: &mut WasmCtx<'_>, msg: SetCellPoints) {
        let cell = msg.cell();
        self.world.set_cell_points(cell, &msg.points);
        self.remesh_around(ctx, cell.chunk());
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
    fn on_set_cell_heights(&mut self, ctx: &mut WasmCtx<'_>, msg: SetCellHeights) {
        let cell = msg.cell();
        self.world.set_cell_heights(cell, &msg.deltas);
        self.remesh_around(ctx, cell.chunk());
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
    fn on_set_view_mode(&mut self, ctx: &mut WasmCtx<'_>, msg: SetViewMode) {
        self.mode = msg.view_mode();
        self.remesh_all(ctx);
    }

    /// Register a region in the world's table so the underlay cascade has
    /// a default to resolve to, then remesh every cached chunk — a region
    /// default can change the resolved underlay of any cell pointing at
    /// that region.
    #[handler::single]
    fn on_set_region(&mut self, ctx: &mut WasmCtx<'_>, msg: SetRegion) {
        let id = msg.region_id;
        self.world.insert_region(id, msg.into_region());
        self.remesh_all(ctx);
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
    fn on_set_smoothing_profile(&mut self, ctx: &mut WasmCtx<'_>, msg: SetSmoothingProfile) {
        self.world
            .insert_smoothing_profile(msg.profile_id, msg.profile());
        self.remesh_all(ctx);
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
    fn on_set_water_plane(&mut self, ctx: &mut WasmCtx<'_>, msg: SetWaterPlane) {
        self.world.insert_water_plane(msg.plane_id, msg.plane());
        self.remesh_all(ctx);
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
    fn on_set_material_style(&mut self, ctx: &mut WasmCtx<'_>, msg: SetMaterialStyle) {
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
                self.remesh_all(ctx);
            }
        }
    }

    /// Resolve an R8 material-mask texture create request. A successful reply
    /// makes the prepared plane drawable on the next `Render` stage; an
    /// error leaves the entry textureless so headless and failed texture
    /// creation never submit a coverage material draw.
    #[handler::single]
    fn on_create_texture_result(&mut self, ctx: &mut WasmCtx<'_>, result: CreateTextureResult) {
        let Some(context) = ctx.take_context::<MaterialMaskTextureContext>() else {
            return;
        };
        let key = MaterialMaskPlaneKey {
            chunk: ChunkPos {
                x: context.chunk_x,
                z: context.chunk_z,
            },
            material: context.material,
        };
        let Some(entry) = self.material_mask_planes.get_mut(&key) else {
            tracing::warn!(
                target: "aether_kit",
                chunk_x = context.chunk_x,
                chunk_z = context.chunk_z,
                material = context.material,
                "material mask texture reply had no pending plane entry",
            );
            return;
        };
        let pending = match entry.texture {
            MaterialMaskTextureState::Pending(request) => Some(request),
            MaterialMaskTextureState::Ready(_) | MaterialMaskTextureState::Failed => None,
        };
        match result {
            CreateTextureResult::Ok { texture_id } => {
                entry.texture = MaterialMaskTextureState::Ready(texture_id);
                if let Some(request) = pending {
                    tracing::debug!(
                        target: "aether_kit",
                        request_id = request.0,
                        texture_id,
                        "material mask texture ready",
                    );
                }
            }
            CreateTextureResult::Err { error } => {
                entry.texture = MaterialMaskTextureState::Failed;
                tracing::warn!(
                    target: "aether_kit",
                    chunk_x = context.chunk_x,
                    chunk_z = context.chunk_z,
                    material = context.material,
                    request_id = pending.map_or(0, |request| request.0),
                    error = %error,
                    "material mask texture creation failed; overlay plane left textureless",
                );
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
                    self.remesh_all(ctx);
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
