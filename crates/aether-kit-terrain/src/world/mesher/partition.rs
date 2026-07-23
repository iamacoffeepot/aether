use alloc::vec;
use alloc::vec::Vec;

use crate::world::{CellPos, ChunkPos, Material, STEP_MAX_OCTIMETERS, World};

use super::constants::{EDGE, OCTIMETERS_PER_SUBCELL, SUB, SUBCELLS_PER_CHUNK_EDGE};
use super::contour::{GridPlacement, SmoothParams};
use super::surface::point_surface_level_at;

/// The partition's inputs at subcell expression: the cascade-resolved
/// material id for every sample of the chunk plus its apron, the
/// smoothing-disabled params (zero iterations — the base render is crisp,
/// no chamfer), and the frozen mask — a sample flanking a point-height
/// break freezes so any future smoothing pass could never move the paint
/// boundary across a physical cliff ([`repartition`]'s barrier). `None`
/// when the whole area is Void — nothing to mesh.
pub(super) fn partition_inputs(
    world: &World,
    at: ChunkPos,
    apron: i32,
    n: usize,
) -> Option<(Vec<u8>, Vec<SmoothParams>, Vec<bool>)> {
    let mut ids = vec![0u8; n * n];
    // The base render disables smoothing: every sample carries zero
    // iterations, so `repartition` runs its crisp path (no chamfer). The
    // per-cell smoothing plane and material smoothing defaults are ignored
    // here; the smoothing rung reintroduces them.
    let params = vec![SmoothParams { iterations: 0, smoothing_degrees: 90 }; n * n];
    let mut frozen = vec![false; n * n];
    let mut any = false;
    for sj in -apron..SUBCELLS_PER_CHUNK_EDGE + apron {
        for si in -apron..SUBCELLS_PER_CHUNK_EDGE + apron {
            let cell = CellPos { x: at.x * EDGE + si.div_euclid(SUB), z: at.z * EDGE + sj.div_euclid(SUB) };
            // Sample the cell's authored point, not the whole-cell material:
            // an all-inherit point folds back to `World::underlay(cell)`, so
            // an unshaped world is unchanged, while an authored point moves
            // the silhouette below cell scale.
            let material = world.underlay_point(cell, si.rem_euclid(SUB), sj.rem_euclid(SUB));
            let idx = (sj + apron) as usize * n + (si + apron) as usize;
            ids[idx] = material.to_u8();
            any |= material != Material::Void;
            // Freeze the sample when its subcell flanks a point-height
            // break: paint smoothing must never cross a physical cliff, or
            // the silhouette's flank would alternate between painted steep
            // caps and wall wedges. Pure world reads (position-based), so
            // neighboring chunks agree over the shared apron. Reads the point
            // levels directly rather than gating on relief, so a **whole-cell**
            // cliff freezes the contour too: without this the chamfer smooths
            // across a relief-free material cliff, flipping the corner to the
            // low neighbor's paint over the high plate (the floating sliver)
            // and leaving its flank unwalled. A flat neighborhood reads equal
            // levels and stays unfrozen, so an unshaped world is unchanged.
            let gx = at.x * EDGE * SUB + si;
            let gz = at.z * EDGE * SUB + sj;
            let own_level = point_surface_level_at(world, gx * OCTIMETERS_PER_SUBCELL, gz * OCTIMETERS_PER_SUBCELL);
            frozen[idx] = [(1, 0), (-1, 0), (0, 1), (0, -1)].into_iter().any(|(dx, dz): (i32, i32)| {
                let level = point_surface_level_at(
                    world,
                    (gx + dx) * OCTIMETERS_PER_SUBCELL,
                    (gz + dz) * OCTIMETERS_PER_SUBCELL,
                );
                (own_level - level).abs() > STEP_MAX_OCTIMETERS
            });
        }
    }
    any.then_some((ids, params, frozen))
}

/// Where a chunk's partition grid lands in the world: sample `(0, 0)`'s
/// octimeter center, offset back by the apron and forward half a step so
/// each sample sits at a subcell-lattice center, at `step_oct` spacing.
/// Both the partition-window and encroachment-flap passes place their
/// marches through this, so they agree on the lattice sample-for-sample.
pub(super) fn chunk_placement(at: ChunkPos, apron: i32, step_oct: i32) -> GridPlacement {
    let base_oct = [
        at.x * SUBCELLS_PER_CHUNK_EDGE * OCTIMETERS_PER_SUBCELL,
        at.z * SUBCELLS_PER_CHUNK_EDGE * OCTIMETERS_PER_SUBCELL,
    ];
    GridPlacement {
        origin_oct: [
            base_oct[0] - apron * OCTIMETERS_PER_SUBCELL + step_oct / 2,
            base_oct[1] - apron * OCTIMETERS_PER_SUBCELL + step_oct / 2,
        ],
        step_oct,
    }
}
