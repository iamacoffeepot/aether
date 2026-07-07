// Probe-point math casts small loop indices to f32; the ranges make the
// pedantic precision lints non-issues.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::suboptimal_flops
)]

//! Partition tiling and frame-budget tripwires over a demo-shaped world
//! (a lake with a sand ring on a grass-default region, four chunks).

use aether_capabilities::render::DrawTriangle;
use aether_kit::{
    CELLS_PER_CHUNK_AREA, Chunk, ChunkPos, Material, Region, ViewMode, World,
    runtime::mesher::mesh_chunk,
};

fn lake_world() -> World {
    let mut world = World::new();
    world.insert_region(
        1,
        Region {
            name: "meadow".into(),
            default_material: Material::Grass,
        },
    );
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
            let mut chunk = Chunk::empty();
            chunk.region = [1; CELLS_PER_CHUNK_AREA];
            for lz in 0..16i32 {
                for lx in 0..16i32 {
                    let (wx, wz) = ((cx * 16 + lx) as f32, (cz * 16 + lz) as f32);
                    let i = (lz * 16 + lx) as usize;
                    if near_lake(wx + 0.5, wz + 0.5, 2.2) {
                        chunk.underlay[i] = Material::Sand;
                    }
                    let mut mask = 0u16;
                    for sz in 0..4 {
                        for sx in 0..4 {
                            let scx = wx + (sx as f32 + 0.5) / 4.0;
                            let scz = wz + (sz as f32 + 0.5) / 4.0;
                            if in_lake(scx, scz) {
                                mask |= 1 << (sz * 4 + sx);
                            }
                        }
                    }
                    if mask != 0 {
                        chunk.overlay[i] = Material::Water;
                        chunk.overlay_mask[i] = mask;
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
    let meshes: Vec<_> = (0..4)
        .map(|k| mesh_chunk(&world, ChunkPos { x: k % 2, z: k / 2 }, ViewMode::Painted))
        .collect();
    let covers = |t: &DrawTriangle, px: f32, pz: f32| {
        let sign =
            |ax: f32, az: f32, bx: f32, bz: f32| (px - bx) * (az - bz) - (ax - bx) * (pz - bz);
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
            let ground_covered = meshes
                .iter()
                .flatten()
                .any(|t| t.verts.iter().all(|v| v.y == 0.0) && covers(t, px, pz));
            if !ground_covered {
                holes.push((px, pz));
            }
        }
    }
    assert!(
        holes.is_empty(),
        "{} ground holes, first at {:?}",
        holes.len(),
        holes.first()
    );
}

#[test]
fn demo_world_fits_the_frame_vertex_budget() {
    // The desktop render capability accepts 4 MiB of vertices per frame
    // (~58k triangles at 24-byte vertices) and warn-drops the excess —
    // the failure mode that motivated window ownership and strip merging.
    // The four-chunk lake scene must sit comfortably inside it.
    let world = lake_world();
    let total: usize = (0..4)
        .map(|k| mesh_chunk(&world, ChunkPos { x: k % 2, z: k / 2 }, ViewMode::Painted).len())
        .sum();
    assert!(total < 45_000, "frame budget headroom: {total} triangles");
}
