use alloc::vec;
use alloc::vec::Vec;

use aether_render::DrawTriangle;

use crate::world::{CellPos, ChunkPos, Material, World};

use super::cliffs::CliffPlan;
use super::constants::{
    CONTOUR_UPSAMPLE, EDGE, MAX_APRON_SUBCELLS, OCTIMETERS_PER_SUBCELL, SUB, SUBCELLS_PER_CHUNK_EDGE,
};
use super::contour::repartition;
use super::geometry::emit_flat_quad;
use super::partition::partition_inputs;
use super::style::{StyleTable, flat_color};
use super::surface::{CellLift, SubPatch};
use super::voids::emit_void_floors;
use super::windows::emit_partition_windows;

/// Emit the underlay as a partition of one flat surface: repartition the
/// cascade-resolved material grid (smoothing off — crisp marching-squares
/// contours, no chamfer), then tile the ground exactly — flat keyed quilt
/// cells wherever a cell and its one-sample surround are uniformly one
/// material, and per-window marching polygons everywhere else, all at
/// `y = 0` with no lifts. Every decision is a pure function of world
/// coordinates, so two chunks emit identical geometry over their shared
/// apron and the overlap is invisible. Windows carrying a physical cliff
/// intersect these material polygons with the already-built [`CliffPlan`];
/// the all-Void case emits nothing.
#[allow(clippy::too_many_lines)] // one underlay pass: partition, tile interiors, march windows
pub(super) fn mesh_underlay(
    world: &World,
    at: ChunkPos,
    cliffs: &CliffPlan,
    styles: &StyleTable,
    tris: &mut Vec<DrawTriangle>,
) {
    let apron = MAX_APRON_SUBCELLS;
    let n = (SUBCELLS_PER_CHUNK_EDGE + 2 * apron) as usize;
    let Some((ids, params, frozen)) = partition_inputs(world, at, apron, n) else {
        return;
    };

    let upsample = CONTOUR_UPSAMPLE;
    let (grid, gw, _gh) = repartition(&ids, n, n, upsample, &params, &frozen);
    let u = upsample as i32;
    let step_oct = OCTIMETERS_PER_SUBCELL / u;

    // The base render has no rim/body split, so the display grid is the
    // repartitioned material grid directly — each material is one flat label.
    let display = grid;

    // Interior cells: the cell's sample block plus a one-sample surround
    // is uniformly its own material. Classified for every cell whose
    // surround fits the grid (local -1..EDGE), identically on both sides of
    // a chunk border, so the window skip below never disagrees with a
    // neighbor's cell quad.
    let lo = -1i32;
    let hi = EDGE;
    let cells_w = (hi - lo) as usize;
    let mut interior = vec![false; cells_w * cells_w];
    for lz in lo..hi {
        for lx in lo..hi {
            let cell = CellPos { x: at.x * EDGE + lx, z: at.z * EDGE + lz };
            let m = world.underlay(cell).to_u8();
            if m == 0 {
                continue;
            }
            let x0 = (lx * SUB + apron) * u;
            let z0 = (lz * SUB + apron) * u;
            let span = SUB * u;
            let uniform = ((z0 - 1)..=(z0 + span)).all(|gz| {
                ((x0 - 1)..=(x0 + span)).all(|gx| {
                    gx >= 0
                        && gz >= 0
                        && (gx as usize) < gw
                        && (gz as usize) < gw
                        && display[gz as usize * gw + gx as usize] == m
                })
            });
            interior[(lz - lo) as usize * cells_w + (lx - lo) as usize] = uniform && !cliffs.cell_has_cliff(cell);
        }
    }

    // Chunk-local interior cells emit the flat keyed quilt cell.
    for lz in 0..EDGE {
        for lx in 0..EDGE {
            if !interior[(lz - lo) as usize * cells_w + (lx - lo) as usize] {
                continue;
            }
            let cell = CellPos { x: at.x * EDGE + lx, z: at.z * EDGE + lz };
            let material = world.underlay(cell);
            // Authored relief tessellates the cap to subcell resolution so
            // its per-point heights and breaks show; a flat cell keeps the
            // whole-cell fast path (byte-identical to a world with no relief).
            if world.cell_has_height_relief(cell) {
                emit_underlay_cell_subdivided(world, material, cell, styles, tris);
                continue;
            }
            let lift = CellLift::of(world, cell);
            emit_underlay_cell(material, cell.x, cell.z, &lift, styles, tris);
        }
    }

    emit_partition_windows(world, at, cliffs, &display, gw, apron, step_oct, &interior, lo, styles, tris);

    // The fill-over floor caps for enclosed Void joints — the flat groove
    // bottoms the plan's bounded Void ribbons drop to.
    emit_void_floors(world, at, cliffs, styles, tris);
}

/// Emit one flat keyed cell: a single flat-colored quad spanning the cell
/// on its bilinear surface patch.
fn emit_underlay_cell(
    material: Material,
    cx: i32,
    cz: i32,
    patch: &CellLift,
    styles: &StyleTable,
    tris: &mut Vec<DrawTriangle>,
) {
    let color = flat_color(styles.get(material));
    let surface = |wx: f32, wz: f32| patch.y(wx, wz);
    let x0 = cx * 256;
    let z0 = cz * 256;
    emit_flat_quad([x0, z0, (cx + 1) * 256, (cz + 1) * 256], color, &surface, tris);
}

/// Emit a height-relief cell's cap as `SUB × SUB` subcell quads, each lifted
/// through its own point patch ([`SubPatch`]) so authored subcell relief —
/// and the breaks where adjacent points cliff — shows in the cap and the
/// drawn height matches [`World::surface_height`] at every sample. A flat
/// cell keeps the whole-cell fast path in [`emit_underlay_cell`].
fn emit_underlay_cell_subdivided(
    world: &World,
    material: Material,
    cell: CellPos,
    styles: &StyleTable,
    tris: &mut Vec<DrawTriangle>,
) {
    let color = flat_color(styles.get(material));
    for sj in 0..SUB {
        for si in 0..SUB {
            let patch = SubPatch::of(world, cell, si, sj);
            let surface = |wx: f32, wz: f32| patch.y(wx, wz);
            let x0 = cell.x * 256 + si * OCTIMETERS_PER_SUBCELL;
            let z0 = cell.z * 256 + sj * OCTIMETERS_PER_SUBCELL;
            emit_flat_quad([x0, z0, x0 + OCTIMETERS_PER_SUBCELL, z0 + OCTIMETERS_PER_SUBCELL], color, &surface, tris);
        }
    }
}
