use alloc::vec;
use alloc::vec::Vec;

use aether_render::{DrawTriangle, Vertex};

use crate::world::{CellPos, ChunkPos, Material, World};

use super::constants::{
    CONTOUR_UPSAMPLE, COVERAGE_LIFT, EDGE, MAX_APRON_SUBCELLS, OCTIMETERS_PER_SUBCELL, SUB, SUBCELLS_PER_CHUNK_EDGE,
};
use super::contour::{SmoothParams, march_grid, minimize_corners};
use super::partition::chunk_placement;
use super::style::{StyleTable, flat_color};

/// Distinct non-void overlay materials present in the chunk, in stable
/// material-id order.
pub(super) fn overlay_materials(world: &World, at: ChunkPos) -> Vec<Material> {
    let Some(chunk) = world.chunk(at) else {
        return Vec::new();
    };
    let mut present = [false; 6];
    for material in &chunk.overlay {
        if *material != Material::Void {
            present[*material as usize] = true;
        }
    }
    present
        .iter()
        .enumerate()
        .filter(|(_, seen)| **seen)
        .map(|(id, _)| Material::from_u8_or_void(id.try_into().unwrap_or(0)))
        .collect()
}

/// Coverage of the subcell at chunk-local index `(six, siz)` for `material`.
/// Off-material samples read uncovered, while out-of-chunk indices resolve
/// through [`World`] so neighboring chunks supply the apron.
pub(super) fn subcell_coverage(world: &World, at: ChunkPos, six: i32, siz: i32, material: Material) -> u8 {
    let cell = CellPos { x: at.x * EDGE + six.div_euclid(SUB), z: at.z * EDGE + siz.div_euclid(SUB) };
    if world.overlay(cell) != material {
        return 0;
    }
    world.overlay_coverage(cell, six, siz)
}

/// Emit a flat scalar-coverage pass for authored overlays. Each distinct
/// overlay material gets its own scalar mask, marched through the contour
/// library so authored `1..254` samples keep interpolated crossings.
pub(super) fn mesh_coverage(world: &World, at: ChunkPos, styles: &StyleTable, tris: &mut Vec<DrawTriangle>) {
    let apron = MAX_APRON_SUBCELLS;
    let n = (SUBCELLS_PER_CHUNK_EDGE + 2 * apron) as usize;
    let params = vec![SmoothParams { iterations: 0, smoothing_degrees: 90 }; n * n];
    let upsample = CONTOUR_UPSAMPLE;
    let step_oct = OCTIMETERS_PER_SUBCELL / upsample as i32;
    let placement = chunk_placement(at, apron, step_oct);
    for material in overlay_materials(world, at) {
        let mut field = vec![0; n * n];
        for sj in -apron..SUBCELLS_PER_CHUNK_EDGE + apron {
            for si in -apron..SUBCELLS_PER_CHUNK_EDGE + apron {
                let idx = (sj + apron) as usize * n + (si + apron) as usize;
                field[idx] = subcell_coverage(world, at, si, sj, material);
            }
        }
        let (samples, width, height) = minimize_corners(&field, n, n, upsample, &params);
        let color = flat_color(styles.get(material));
        let vertex = |wx: f32, wz: f32| Vertex { x: wx, y: COVERAGE_LIFT, z: wz, color };
        march_grid(&samples, width, height, &placement, &vertex, tris);
    }
}
