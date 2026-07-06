// `#[handler]` methods take the decoded mail by value per the ADR-0033
// dispatch ABI; the macro-generated trampoline owns the payload.
#![allow(clippy::needless_pass_by_value)]
// Chunk-local loop counters and world-cell coordinates are small
// integers cast between i32 (coordinate math) and f32 (vertex output) /
// usize (plane indexing); the ranges (0..16 locally, chunk-bounded
// world cells) make the precision / sign lints the pedantic set raises
// non-issues here.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap
)]

//! World-view runtime. Greedy-meshes the chunked plane stack
//! ([`crate::world`]) into flat ground quads and replays the cached
//! per-chunk mesh to `"aether.render"` each frame on the `Render`
//! lifecycle stage.
//!
//! The mesher ([`mesh_chunk`]) is a pure function over the
//! cascade-resolved **underlay** plane: per chunk it greedy-merges
//! maximal same-material rectangles and emits one flat quad (two
//! triangles) per rectangle at `y = 0`, in world-space meters
//! (`1 cell = 1 m`). The existing `aether.camera` `view_proj` handles
//! projection. `Void` cells emit nothing.
//!
//! # Scope (v1)
//!
//! Flat underlay only. The `overlay` / `overlay_mask` planes and the
//! `height` plane are ignored here — the settled mesher direction
//! (per-cell corner-blended triangles + marching squares over the
//! overlay subcell masks + per-cell hue jitter) is a mesher-only
//! follow-up; the wire layout the plane stack ships is already final
//! for it. Interest management is out of scope — every loaded chunk
//! renders.
//!
//! # Mail surface
//!
//! - `aether.kit.world.set_chunk` — write one chunk's planes and remesh
//!   just that chunk.
//! - `aether.kit.world.set_region` — register a region so the underlay
//!   cascade has a default to resolve to; remeshes every cached chunk
//!   (a region default can change any chunk's cascade-resolved underlay).
//! - `aether.kit.world.load` — fetch a serialized world through
//!   `aether.fs`, decode, atomically swap, and remesh all. A decode or
//!   read failure keeps the prior world (errors go to logs).

use alloc::collections::BTreeMap;

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_capabilities::fs::{FsMailboxExt, ReadResult};
use aether_capabilities::lifecycle::LifecycleMailboxExt;
use aether_capabilities::render::{DrawTriangle, Vertex};
use aether_capabilities::{FsCapability, LifecycleCapability, RenderCapability};
use aether_kinds::Render;

use crate::world::{
    CELLS_PER_CHUNK, CELLS_PER_CHUNK_AREA, CellPos, ChunkPos, Material, SetChunk, SetRegion, World,
    WorldLoad,
};

/// Per-material vertex colors, **pre-linearized** — vertex colors are
/// linear into the sRGB surface, so the sRGB design values are converted
/// to linear here (`c/12.92` below the knee, `((c+0.055)/1.055)^2.4`
/// above) once at authoring time rather than washing out at draw. Index
/// by `Material as usize`; index 0 (`Void`) is never emitted.
///
/// sRGB design values: Grass `(0.30, 0.55, 0.25)`, Dirt
/// `(0.45, 0.32, 0.18)`, Stone `(0.55, 0.55, 0.58)`, Sand
/// `(0.85, 0.78, 0.55)`, Water `(0.20, 0.40, 0.70)`.
const LINEAR_PALETTE: [[f32; 3]; 6] = [
    [0.0, 0.0, 0.0],          // Void — unused
    [0.0732, 0.2633, 0.0509], // Grass
    [0.1708, 0.0835, 0.0272], // Dirt
    [0.2633, 0.2633, 0.2957], // Stone
    [0.6919, 0.5705, 0.2633], // Sand
    [0.0331, 0.1329, 0.4479], // Water
];

/// The cells-per-chunk edge as a plain `i32`, for the mesher's local
/// loop bounds and coordinate math.
const EDGE: i32 = CELLS_PER_CHUNK;

/// Greedy-mesh one chunk's cascade-resolved underlay plane into flat
/// ground quads. Pure — no wgpu, no ctx — so it is unit-testable
/// host-side. Each maximal same-material rectangle becomes two triangles
/// at `y = 0`, world-space in meters. `Void` cells emit nothing.
#[must_use]
pub fn mesh_chunk(world: &World, at: ChunkPos) -> Vec<DrawTriangle> {
    let mut consumed = [false; CELLS_PER_CHUNK_AREA];
    let mut tris = Vec::new();

    let base_x = at.x * EDGE;
    let base_z = at.z * EDGE;

    for lz in 0..EDGE {
        for lx in 0..EDGE {
            let idx = (lz * EDGE + lx) as usize;
            if consumed[idx] {
                continue;
            }
            let material = world.underlay(CellPos {
                x: base_x + lx,
                z: base_z + lz,
            });
            if material == Material::Void {
                consumed[idx] = true;
                continue;
            }

            // Grow the rectangle width along +x while the material
            // matches and the cell is free.
            let mut width = 1;
            while lx + width < EDGE {
                let next = (lz * EDGE + lx + width) as usize;
                if consumed[next]
                    || world.underlay(CellPos {
                        x: base_x + lx + width,
                        z: base_z + lz,
                    }) != material
                {
                    break;
                }
                width += 1;
            }

            // Grow height along +z while every cell in the row-span
            // matches and is free.
            let mut height = 1;
            'rows: while lz + height < EDGE {
                for dx in 0..width {
                    let cell = ((lz + height) * EDGE + lx + dx) as usize;
                    if consumed[cell]
                        || world.underlay(CellPos {
                            x: base_x + lx + dx,
                            z: base_z + lz + height,
                        }) != material
                    {
                        break 'rows;
                    }
                }
                height += 1;
            }

            for dz in 0..height {
                for dx in 0..width {
                    consumed[((lz + dz) * EDGE + lx + dx) as usize] = true;
                }
            }

            let color = LINEAR_PALETTE[material as usize];
            push_quad(
                &mut tris,
                (base_x + lx) as f32,
                (base_z + lz) as f32,
                (base_x + lx + width) as f32,
                (base_z + lz + height) as f32,
                color,
            );
        }
    }

    tris
}

/// Push the two triangles of a flat ground quad spanning
/// `[x0, x1] × [z0, z1]` at `y = 0`, all four corners the same color.
fn push_quad(tris: &mut Vec<DrawTriangle>, x0: f32, z0: f32, x1: f32, z1: f32, color: [f32; 3]) {
    let vert = |x: f32, z: f32| Vertex {
        x,
        y: 0.0,
        z,
        r: color[0],
        g: color[1],
        b: color[2],
    };
    let a = vert(x0, z0);
    let b = vert(x1, z0);
    let c = vert(x1, z1);
    let d = vert(x0, z1);
    tris.push(DrawTriangle { verts: [a, b, c] });
    tris.push(DrawTriangle { verts: [a, c, d] });
}

/// World-view component: holds the world plane stack and a per-chunk
/// mesh cache, and replays the cache to the render sink each frame.
///
/// # Agent
/// Load with the `aether_kit@aether.world` export. Paint the world by
/// sending `aether.kit.world.set_chunk` (one chunk's planes) and
/// `aether.kit.world.set_region` (a region default for the underlay
/// cascade); each send remeshes and the meadow renders every frame under
/// the active `aether.camera` view. `aether.kit.world.load` swaps a
/// serialized world from `aether.fs`. Use `capture_frame` to verify.
pub struct WorldView {
    world: World,
    meshes: BTreeMap<ChunkPos, Vec<DrawTriangle>>,
}

impl WorldView {
    /// Rebuild every chunk's cached mesh from the current world — used
    /// after a whole-world change (region default, world load) that can
    /// alter any chunk's cascade-resolved underlay.
    fn remesh_all(&mut self) {
        self.meshes.clear();
        let positions: Vec<ChunkPos> = self.world.chunks().map(|(pos, _)| pos).collect();
        for pos in positions {
            self.meshes.insert(pos, mesh_chunk(&self.world, pos));
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

    /// Write one chunk's planes into the world and remesh just that
    /// chunk — flat underlay meshing has no cross-chunk dependency, so a
    /// single-chunk write only invalidates its own mesh.
    #[handler]
    fn on_set_chunk(&mut self, _ctx: &mut WasmCtx<'_>, msg: SetChunk) {
        let pos = msg.chunk_pos();
        self.world.insert_chunk(pos, msg.into_chunk());
        self.meshes.insert(pos, mesh_chunk(&self.world, pos));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::{Chunk, Region};

    /// Fill a chunk's whole underlay plane with one material.
    fn uniform_chunk(material: Material) -> Chunk {
        let mut chunk = Chunk::empty();
        chunk.underlay = [material; CELLS_PER_CHUNK_AREA];
        chunk
    }

    #[test]
    fn uniform_chunk_meshes_to_one_quad() {
        let mut world = World::new();
        world.insert_chunk(ChunkPos { x: 0, z: 0 }, uniform_chunk(Material::Grass));
        let tris = mesh_chunk(&world, ChunkPos { x: 0, z: 0 });
        assert_eq!(tris.len(), 2, "one 16x16 rectangle = two triangles");
    }

    #[test]
    fn void_chunk_meshes_to_nothing() {
        let mut world = World::new();
        world.insert_chunk(ChunkPos { x: 0, z: 0 }, Chunk::empty());
        let tris = mesh_chunk(&world, ChunkPos { x: 0, z: 0 });
        assert!(tris.is_empty(), "all-Void underlay emits no geometry");
    }

    #[test]
    fn two_material_halves_merge_into_two_quads() {
        // Left 8 columns Grass, right 8 columns Dirt → two rectangles.
        let mut chunk = Chunk::empty();
        for z in 0..16 {
            for x in 0..16 {
                chunk.underlay[z * 16 + x] = if x < 8 {
                    Material::Grass
                } else {
                    Material::Dirt
                };
            }
        }
        let mut world = World::new();
        world.insert_chunk(ChunkPos { x: 0, z: 0 }, chunk);
        let tris = mesh_chunk(&world, ChunkPos { x: 0, z: 0 });
        assert_eq!(tris.len(), 4, "two rectangles = four triangles");
    }

    #[test]
    fn checkerboard_is_the_worst_case() {
        // Alternating materials → no two neighbors merge → 256 single-cell
        // rectangles → 512 triangles (the cited worst case).
        let mut chunk = Chunk::empty();
        for z in 0..16 {
            for x in 0..16 {
                chunk.underlay[z * 16 + x] = if (x + z) % 2 == 0 {
                    Material::Grass
                } else {
                    Material::Stone
                };
            }
        }
        let mut world = World::new();
        world.insert_chunk(ChunkPos { x: 0, z: 0 }, chunk);
        let tris = mesh_chunk(&world, ChunkPos { x: 0, z: 0 });
        assert_eq!(tris.len(), 512, "16x16 checkerboard = 512 triangles");
    }

    #[test]
    fn mesher_reads_the_underlay_cascade_not_the_raw_plane() {
        // A chunk with an all-Void underlay plane, but every cell in
        // region 1 whose default is Grass → the mesher sees Grass and
        // emits one merged quad.
        let mut chunk = Chunk::empty();
        chunk.region = [1u16; CELLS_PER_CHUNK_AREA];
        let mut world = World::new();
        world.insert_chunk(ChunkPos { x: 0, z: 0 }, chunk);
        world.insert_region(
            1,
            Region {
                name: "meadow".into(),
                default_material: Material::Grass,
            },
        );
        let tris = mesh_chunk(&world, ChunkPos { x: 0, z: 0 });
        assert_eq!(tris.len(), 2, "cascade-resolved Grass meshes as one quad");
        // And the color is Grass's linear palette entry.
        assert_eq!(
            tris[0].verts[0].r,
            LINEAR_PALETTE[Material::Grass as usize][0]
        );
    }

    #[test]
    fn quad_spans_world_cells_in_meters() {
        // A single Stone cell at chunk (1, -1), local (0,0) → world cell
        // (16, -16); the quad spans [16,17] × [-16,-15] at y=0.
        let mut chunk = Chunk::empty();
        chunk.underlay[0] = Material::Stone;
        let mut world = World::new();
        world.insert_chunk(ChunkPos { x: 1, z: -1 }, chunk);
        let tris = mesh_chunk(&world, ChunkPos { x: 1, z: -1 });
        assert_eq!(tris.len(), 2);
        let xs: Vec<f32> = tris
            .iter()
            .flat_map(|t| t.verts.iter().map(|v| v.x))
            .collect();
        let zs: Vec<f32> = tris
            .iter()
            .flat_map(|t| t.verts.iter().map(|v| v.z))
            .collect();
        assert!(xs.iter().all(|&x| (16.0..=17.0).contains(&x)));
        assert!(zs.iter().all(|&z| (-16.0..=-15.0).contains(&z)));
        assert!(tris.iter().all(|t| t.verts.iter().all(|v| v.y == 0.0)));
    }
}
