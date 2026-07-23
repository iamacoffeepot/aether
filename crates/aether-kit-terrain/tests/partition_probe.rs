// Probe-point math casts small loop indices to f32; the ranges make the
// pedantic precision lints non-issues.
#![allow(clippy::cast_precision_loss, clippy::cast_sign_loss, clippy::suboptimal_flops)]

//! Partition tiling and frame-budget tripwires over a demo-shaped world
//! (a lake with a sand ring on a grass-default region, four chunks).

use aether_kit_terrain::{
    CELLS_PER_CHUNK_AREA, Chunk, ChunkPos, Material, Region, WaterPlane, World,
    world::{
        CellPos, SUBCELLS_PER_CELL, SUBCELLS_PER_CELL_EDGE,
        mesher::{mesh_chunk, style::StyleTable},
    },
};
use aether_render::DrawTriangle;

fn lake_world() -> World {
    let mut world = World::new();
    world.insert_region(
        1,
        Region { name: "meadow".into(), default_material: Material::Grass, cliff_material: Material::Stone },
    );
    // The lake surface plane every lake cell references.
    world.insert_water_plane(1, WaterPlane { level_octimeters: 0 });
    let in_lake = |x: f32, z: f32| {
        let (dx, dz) = (x - 16.5, z - 20.0);
        dx.hypot(dz) < 5.5
    };
    let near_lake = |x: f32, z: f32, pad: f32| {
        let (dx, dz) = (x - 16.5, z - 20.0);
        dx.hypot(dz) < 5.5 + pad
    };
    for cz in 0..2 {
        for cx in 0..2 {
            let mut chunk = Chunk::empty_boxed();
            chunk.region = [1; CELLS_PER_CHUNK_AREA];
            for lz in 0..16i32 {
                for lx in 0..16i32 {
                    let (wx, wz) = ((cx * 16 + lx) as f32, (cz * 16 + lz) as f32);
                    let i = (lz * 16 + lx) as usize;
                    if near_lake(wx + 0.5, wz + 0.5, 2.2) {
                        chunk.underlay[i] = Material::Sand;
                    }
                    // Water is underlay fabric now: paint the lake cells Water
                    // and point them at the water plane; the partition smooths
                    // the waterline at subcell expression from the cell paint.
                    if in_lake(wx + 0.5, wz + 0.5) {
                        chunk.underlay[i] = Material::Water;
                        chunk.water_plane[i] = 1;
                    }
                }
            }
            world.insert_chunk(ChunkPos { x: cx, z: cz }, chunk);
        }
    }
    world
}

#[test]
fn demo_world_ground_has_no_holes() {
    let world = lake_world();
    let styles = StyleTable::default();
    let meshes: Vec<_> = (0..4).map(|k| mesh_chunk(&world, ChunkPos { x: k % 2, z: k / 2 }, &styles)).collect();
    let covers = |t: &DrawTriangle, px: f32, pz: f32| {
        let sign = |ax: f32, az: f32, bx: f32, bz: f32| (px - bx) * (az - bz) - (ax - bx) * (pz - bz);
        let d1 = sign(t.verts[0].x, t.verts[0].z, t.verts[1].x, t.verts[1].z);
        let d2 = sign(t.verts[1].x, t.verts[1].z, t.verts[2].x, t.verts[2].z);
        let d3 = sign(t.verts[2].x, t.verts[2].z, t.verts[0].x, t.verts[0].z);
        let has_neg = d1 < -1e-6 || d2 < -1e-6 || d3 < -1e-6;
        let has_pos = d1 > 1e-6 || d2 > 1e-6 || d3 > 1e-6;
        !(has_neg && has_pos)
    };
    let mut holes = Vec::new();
    for j in 0..64 {
        for i in 0..64 {
            let px = (i as f32) * 0.5 + 0.29;
            let pz = (j as f32) * 0.5 + 0.31;
            let ground_covered =
                meshes.iter().flatten().any(|t| t.verts.iter().all(|v| v.y == 0.0) && covers(t, px, pz));
            if !ground_covered {
                holes.push((px, pz));
            }
        }
    }
    assert!(holes.is_empty(), "{} ground holes, first at {:?}", holes.len(), holes.first());
}

#[test]
fn demo_world_fits_the_frame_vertex_budget() {
    // The desktop render capability accepts 64 MiB of vertices per frame
    // by default (~932k triangles at 24-byte vertices) and warn-drops the
    // excess — the failure mode that motivated window ownership and strip
    // merging. At SUB=16, this four-chunk lake scene should still cost less
    // than one full-water chunk's per-subcell body mesh.
    let world = lake_world();
    let styles = StyleTable::default();
    let total: usize = (0..4).map(|k| mesh_chunk(&world, ChunkPos { x: k % 2, z: k / 2 }, &styles).len()).sum();
    let budget = CELLS_PER_CHUNK_AREA * SUBCELLS_PER_CELL * 2;
    assert!(total < budget, "frame budget headroom: {total} triangles, budget {budget}");
}

#[test]
fn dense_multilevel_cliffs_stay_inside_the_local_topology_ceiling() {
    // Four cells per axis carry eight repeating point levels. Every adjacent
    // authored sample differs by at least 80 octimeters (past the 64-octimeter
    // step ceiling), so the patch densely exercises pinned four-way windows
    // without making work depend on the number of unique heights.
    let mut world = World::new();
    for chunk_z in -1..=1 {
        for chunk_x in -1..=1 {
            let mut chunk = Chunk::empty_boxed();
            chunk.underlay = [Material::Grass; CELLS_PER_CHUNK_AREA];
            world.insert_chunk(ChunkPos { x: chunk_x, z: chunk_z }, chunk);
        }
    }
    let sub = SUBCELLS_PER_CELL_EDGE as usize;
    for cell_z in 6..10 {
        for cell_x in 6..10 {
            let mut deltas = [0i16; SUBCELLS_PER_CELL];
            for point_z in 0..sub {
                for point_x in 0..sub {
                    let global_x = cell_x as usize * sub + point_x;
                    let global_z = cell_z as usize * sub + point_z;
                    deltas[point_z * sub + point_x] =
                        i16::try_from(((global_x + global_z) % 8) * 80).expect("fixture levels fit i16");
                }
            }
            world.set_cell_heights(CellPos { x: cell_x, z: cell_z }, &deltas);
        }
    }

    let mesh = mesh_chunk(&world, ChunkPos { x: 0, z: 0 }, &StyleTable::default());
    // Independently derived ceiling: the 4*SUB authored patch can affect at
    // most one extra contour-window center on each positive boundary,
    // `(4*SUB + 1)^2` windows. A pinned uniform-material window emits at
    // most four cap quads (8 triangles) plus four wall quads (8 triangles).
    // The authored 4x4 cells may also retain their SUB² relief-cap quads in
    // regions the plan proves continuous; every remaining chunk cell
    // contributes at most its ordinary cap quad. Counting both paths is
    // deliberately conservative (their footprints do not actually overlap).
    let affected_edge = 4 * sub + 1;
    let local_case_ceiling = affected_edge * affected_edge * 16;
    let authored_cells = 4 * 4;
    let relief_cap_ceiling = authored_cells * SUBCELLS_PER_CELL * 2;
    let ordinary_cap_ceiling = (CELLS_PER_CHUNK_AREA - authored_cells) * 2;
    let triangle_ceiling = local_case_ceiling + relief_cap_ceiling + ordinary_cap_ceiling;
    assert!(
        mesh.len() <= triangle_ceiling,
        "dense bounded arrangement emitted {} triangles past independently derived ceiling {triangle_ceiling}",
        mesh.len(),
    );
}
