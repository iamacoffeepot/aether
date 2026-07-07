// `#[handler]` methods take the decoded mail by value per the ADR-0033
// dispatch ABI; the macro-generated trampoline owns the payload.
#![allow(clippy::needless_pass_by_value)]

//! World-view runtime. Meshes the chunked plane stack
//! ([`crate::world`]) into ground geometry and replays the cached
//! per-chunk mesh to `"aether.render"` each frame on the `Render`
//! lifecycle stage.
//!
//! The mesher lives in [`super::mesher`] as a pure function
//! ([`mesh_chunk`]); this actor keeps the per-chunk mesh cache, the active
//! view mode, and the cache invalidation. Each chunk becomes keyed-quilt
//! underlay cells (flat world-anchored color with pooled rims and a wash
//! gradient) and corner-minimized overlay contours over the subcell masks,
//! in world-space meters (`1 cell = 1 m`) with the existing `aether.camera`
//! `view_proj` handling projection. A chunk's rims and contours read a
//! bounded apron into its neighbors, so a write invalidates its own mesh
//! and its eight cached neighbors.
//!
//! # Mail surface
//!
//! - `aether.kit.world.set_chunk` — write one chunk's planes and remesh
//!   that chunk plus its eight cached neighbors (their border rims and
//!   contours read the new planes through the apron).
//! - `aether.kit.world.set_region` — register a region so the underlay
//!   cascade has a default to resolve to; remeshes every cached chunk
//!   (a region default can change any chunk's cascade-resolved underlay).
//! - `aether.kit.world.set_view_mode` — switch between the painted gouache
//!   grammar and the raw grayscale calibration field; remeshes all.
//! - `aether.kit.world.load` — fetch a serialized world through
//!   `aether.fs`, decode, atomically swap, and remesh all. A decode or
//!   read failure keeps the prior world (errors go to logs).

use alloc::collections::BTreeMap;

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_capabilities::fs::{FsMailboxExt, ReadResult};
use aether_capabilities::lifecycle::LifecycleMailboxExt;
use aether_capabilities::render::DrawTriangle;
use aether_capabilities::{FsCapability, LifecycleCapability, RenderCapability};
use aether_kinds::Render;

use super::mesher::mesh_chunk;
use crate::world::{ChunkPos, SetChunk, SetRegion, SetViewMode, ViewMode, World, WorldLoad};

/// World-view component: holds the world plane stack and a per-chunk
/// mesh cache, and replays the cache to the render sink each frame.
///
/// # Agent
/// Load with the `aether_kit@aether.world` export. Paint the world by
/// sending `aether.kit.world.set_chunk` (one chunk's planes) and
/// `aether.kit.world.set_region` (a region default for the underlay
/// cascade); each send remeshes and the meadow renders every frame under
/// the active `aether.camera` view. `aether.kit.world.set_view_mode`
/// toggles the raw grayscale field for calibrating the material table;
/// `aether.kit.world.load` swaps a serialized world from `aether.fs`. Use
/// `capture_frame` to verify.
pub struct WorldView {
    world: World,
    meshes: BTreeMap<ChunkPos, Vec<DrawTriangle>>,
    mode: ViewMode,
}

impl WorldView {
    /// Rebuild every chunk's cached mesh from the current world — used
    /// after a whole-world change (region default, world load, view mode)
    /// that can alter any chunk's mesh.
    fn remesh_all(&mut self) {
        self.meshes.clear();
        let positions: Vec<ChunkPos> = self.world.chunks().map(|(pos, _)| pos).collect();
        for pos in positions {
            self.meshes
                .insert(pos, mesh_chunk(&self.world, pos, self.mode));
        }
    }
}

#[actor]
impl WasmActor for WorldView {
    const NAMESPACE: &'static str = "aether.world";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(WorldView {
            world: World::new(),
            meshes: BTreeMap::new(),
            mode: ViewMode::default(),
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
    #[handler]
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
    #[handler]
    fn on_set_chunk(&mut self, _ctx: &mut WasmCtx<'_>, msg: SetChunk) {
        let pos = msg.chunk_pos();
        self.world.insert_chunk(pos, msg.into_chunk());
        for dz in -1..=1 {
            for dx in -1..=1 {
                let neighbor = ChunkPos {
                    x: pos.x + dx,
                    z: pos.z + dz,
                };
                if (dx == 0 && dz == 0) || self.meshes.contains_key(&neighbor) {
                    self.meshes
                        .insert(neighbor, mesh_chunk(&self.world, neighbor, self.mode));
                }
            }
        }
    }

    /// Switch the view between the painted gouache grammar and the raw
    /// grayscale field, then remesh every cached chunk — the mode changes
    /// how every cell paints. A repeat of the current mode still remeshes.
    ///
    /// # Agent
    /// Send `aether.kit.world.set_view_mode` with `mode = 0` for the
    /// painted look or `mode = 1` for the raw hue-noise field, used to
    /// calibrate the material table by eye. `capture_frame` to compare.
    #[handler]
    fn on_set_view_mode(&mut self, _ctx: &mut WasmCtx<'_>, msg: SetViewMode) {
        self.mode = msg.view_mode();
        self.remesh_all();
    }

    /// Register a region in the world's table so the underlay cascade has
    /// a default to resolve to, then remesh every cached chunk — a region
    /// default can change the resolved underlay of any cell pointing at
    /// that region.
    #[handler]
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
    #[handler]
    fn on_load(&mut self, ctx: &mut WasmCtx<'_>, msg: WorldLoad) {
        tracing::info!(
            target: "aether_kit",
            namespace = %msg.namespace,
            path = %msg.path,
            "world load requested; issuing read",
        );
        ctx.actor::<FsCapability>().read(&msg.namespace, &msg.path);
    }

    /// Consume the `aether.fs` read reply. On `Ok`, decode the bytes with
    /// `World::from_bytes`; on success swap the world and remesh all. Any
    /// failure (read error or decode error) leaves the prior world intact
    /// with a warn log.
    ///
    /// # Agent
    /// Substrate-driven; do not send manually.
    #[handler]
    fn on_read_result(&mut self, _ctx: &mut WasmCtx<'_>, result: ReadResult) {
        match result {
            ReadResult::Ok {
                namespace,
                path,
                bytes,
            } => match World::from_bytes(&bytes) {
                Ok(world) => {
                    self.world = world;
                    self.remesh_all();
                    tracing::info!(
                        target: "aether_kit",
                        namespace = %namespace,
                        path = %path,
                        chunks = self.meshes.len(),
                        "world load complete; cache replaced",
                    );
                }
                Err(error) => tracing::warn!(
                    target: "aether_kit",
                    namespace = %namespace,
                    path = %path,
                    error = ?error,
                    "world decode failed; keeping prior world",
                ),
            },
            ReadResult::Err {
                namespace,
                path,
                error,
            } => tracing::warn!(
                target: "aether_kit",
                namespace = %namespace,
                path = %path,
                error = ?error,
                "world read failed; keeping prior world",
            ),
        }
    }
}
