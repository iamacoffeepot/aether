use alloc::vec;
use alloc::vec::Vec;

use aether_capabilities::render::{DrawTriangle, Vertex};
use aether_math::Rgb;

use crate::world::{CellPos, ChunkPos, Material, World};

use super::constants::{
    MAX_APRON_SUBCELLS, OCTIMETERS_PER_METER, OCTIMETERS_PER_SUBCELL, SUBCELLS_PER_CHUNK_EDGE,
};
use super::contour::{SmoothParams, march_grid_with_contours, minimize_corners};
use super::partition::chunk_placement;
use super::style::{StyleTable, flat_color};
use super::surface::{
    CliffStep, cliff_steps, floor_to_i32, level_coverage_plane, point_surface_level_at,
    sample_level_plane,
};

/// The level contour gets one additional interpolation rung beyond the
/// already-dense point lattice. Material partitioning keeps its crisp
/// one-sample path; this separate scalar field is deliberately smoothed.
const LEVEL_CONTOUR_UPSAMPLE: usize = 2;

/// Emit every physical cliff as one cap-and-wall loft from the scalar level
/// contour. For each discovered high/low step, `march_grid_with_contours`
/// emits the high cap and returns the cap contour's exact vertices. The wall
/// copies those positions for its top ring and changes only `y` for the
/// bottom ring, so there is no independently reconstructed seam to reconcile.
pub(super) fn emit_lofts(
    world: &World,
    at: ChunkPos,
    styles: &StyleTable,
    tris: &mut Vec<DrawTriangle>,
) {
    let apron = MAX_APRON_SUBCELLS;
    let plane = sample_level_plane(world, at, apron);
    let params = vec![
        SmoothParams {
            iterations: 2,
            smoothing_degrees: 90,
        };
        plane.levels.len()
    ];
    let upsample = LEVEL_CONTOUR_UPSAMPLE;
    let step_oct = OCTIMETERS_PER_SUBCELL / upsample as i32;
    let placement = chunk_placement(at, apron, step_oct);
    let window_start = apron as usize * upsample - 1;
    let window_span = SUBCELLS_PER_CHUNK_EDGE as usize * upsample;
    let window_rect = [
        window_start,
        window_start,
        window_start + window_span,
        window_start + window_span,
    ];

    for step in cliff_steps(&plane) {
        let coverage = level_coverage_plane(&plane, step);
        let (grid, width, height) =
            minimize_corners(&coverage, plane.width, plane.width, upsample, &params);
        let cap_vertex = |wx: f32, wz: f32| {
            let (_, high_cell) = high_side_sample(world, step, wx, wz);
            let color = flat_color(styles.get(world.cliff_material(high_cell)));
            Vertex {
                x: wx,
                y: step.high as f32 / OCTIMETERS_PER_METER,
                z: wz,
                color: Rgb::new(color[0], color[1], color[2]),
            }
        };
        let polylines = march_grid_with_contours(
            &grid,
            width,
            height,
            &placement,
            window_rect,
            &cap_vertex,
            tris,
        );
        for polyline in polylines {
            for edge in polyline.windows(2) {
                let top_a = edge[0];
                let top_b = edge[1];
                push_loft_wall(tris, top_a, top_b, step.low);
            }
        }
    }
}

/// Resolve the solid high side nearest a contour position. The half-subcell
/// probes straddle the isoline; choosing the greatest step-local level gives
/// the cliff-material owner without inferring a side from whichever cell the
/// interpolated point happens to floor into.
fn high_side_sample(world: &World, step: CliffStep, wx: f32, wz: f32) -> (Material, CellPos) {
    let px = floor_to_i32(wx * OCTIMETERS_PER_METER);
    let pz = floor_to_i32(wz * OCTIMETERS_PER_METER);
    let half = OCTIMETERS_PER_SUBCELL / 2;
    let mut best: Option<(i32, Material, CellPos)> = None;
    for [dx, dz] in [[0, 0], [half, 0], [-half, 0], [0, half], [0, -half]] {
        let sx = px + dx;
        let sz = pz + dz;
        let cell = CellPos {
            x: sx.div_euclid(256),
            z: sz.div_euclid(256),
        };
        let sub_x = sx.rem_euclid(256) / OCTIMETERS_PER_SUBCELL;
        let sub_z = sz.rem_euclid(256) / OCTIMETERS_PER_SUBCELL;
        let material = world.underlay_point(cell, sub_x, sub_z);
        if material == Material::Void {
            continue;
        }
        let level = point_surface_level_at(world, sx, sz);
        if level < step.low || best.is_some_and(|(best_level, _, _)| best_level >= level) {
            continue;
        }
        best = Some((level, material, cell));
    }
    best.map_or_else(
        || {
            let cell = CellPos {
                x: px.div_euclid(256),
                z: pz.div_euclid(256),
            };
            let material = world.underlay(cell);
            (
                if material == Material::Void {
                    world.cliff_material(cell)
                } else {
                    material
                },
                cell,
            )
        },
        |(_, material, cell)| (material, cell),
    )
}

/// Emit one quad of the continuous wall ribbon. `top_a` and `top_b` are
/// copied from the marched cap contour without reconstruction or recoloring;
/// every field therefore remains bit-identical to the cap vertices. The low
/// ring changes only `y`, dropping the same `(x, z)` points to the step's low
/// level.
fn push_loft_wall(tris: &mut Vec<DrawTriangle>, top_a: Vertex, top_b: Vertex, low_level: i32) {
    let a = top_a;
    let b = top_b;
    let mut c = b;
    c.y = low_level as f32 / OCTIMETERS_PER_METER;
    let mut d = a;
    d.y = c.y;
    tris.push(DrawTriangle { verts: [a, b, c] });
    tris.push(DrawTriangle { verts: [a, c, d] });
}
