use alloc::vec::Vec;

use aether_render::{DrawTriangle, Vertex};

use crate::world::{CellPos, ChunkPos, Material, World};

use super::cliffs::{CliffPlan, MaterialCap, PlanarPoint, WindowCenter};
use super::constants::{EDGE, OCTIMETERS_PER_METER};
use super::contour::{GridPlacement, label_case, label_window_polys};
use super::geometry::emit_flat_quad;
use super::partition::chunk_placement;
use super::style::{StyleTable, flat_color};
use super::surface::{fragment_lift, label_lift};

/// Emit the marching-squares windows of the partition's boundary zone.
/// Each window is owned by exactly one chunk — the one holding the cell
/// under its center — so the boundary zone is emitted once fleet-wide,
/// with no cross-chunk duplicates against the fixed per-frame vertex
/// budget. Windows fully covered by interior cell quads are skipped;
/// uniform single-label windows coalesce into row strips (per owning
/// cell, so the keyed color stays flat per cell); mixed windows emit each
/// label's case polygon, saddles resolved by label order so every window
/// tiles exactly.
#[allow(clippy::too_many_arguments)] // one call site; the partition state travels together
pub(super) fn emit_partition_windows(
    world: &World,
    at: ChunkPos,
    cliffs: &CliffPlan,
    display: &[u8],
    gw: usize,
    apron: i32,
    step_oct: i32,
    interior: &[bool],
    lo: i32,
    styles: &StyleTable,
    tris: &mut Vec<DrawTriangle>,
) {
    let hi = EDGE;
    let cells_w = (hi - lo) as usize;
    let place = chunk_placement(at, apron, step_oct);
    let origin_oct = place.origin_oct;
    // A pending run of uniform windows: (label, owner cell, start wi).
    let mut run: Option<(u8, CellPos, usize)> = None;
    let flush = |run: &mut Option<(u8, CellPos, usize)>, end_wi: usize, wj: usize, tris: &mut Vec<DrawTriangle>| {
        let Some((label, owner, start_wi)) = run.take() else {
            return;
        };
        let rect = [
            origin_oct[0] + start_wi as i32 * step_oct,
            origin_oct[1] + wj as i32 * step_oct,
            origin_oct[0] + end_wi as i32 * step_oct,
            origin_oct[1] + (wj + 1) as i32 * step_oct,
        ];
        emit_label_quad(world, label, owner, rect, styles, tris);
    };
    let windows = gw - 1;
    for wj in 0..windows {
        for wi in 0..windows {
            let x_lo = origin_oct[0] + wi as i32 * step_oct;
            let z_lo = origin_oct[1] + wj as i32 * step_oct;
            // Ownership: the cell under the window's center. Emitting only
            // chunk-local owners covers every window exactly once across
            // the fleet, and a local owner keeps every overlapping cell
            // within the classifiable range.
            let x_center_cell = (x_lo + step_oct / 2).div_euclid(256);
            let z_center_cell = (z_lo + step_oct / 2).div_euclid(256);
            let x_owner_local = x_center_cell - at.x * EDGE;
            let z_owner_local = z_center_cell - at.z * EDGE;
            if !(0..EDGE).contains(&x_owner_local) || !(0..EDGE).contains(&z_owner_local) {
                flush(&mut run, wi, wj, tris);
                continue;
            }
            let center = WindowCenter { x_octimeters: x_lo + step_oct / 2, z_octimeters: z_lo + step_oct / 2 };
            let has_cliff = cliffs.has_window_at(center);
            // The world cells this window's square overlaps.
            let cx0 = x_lo.div_euclid(256);
            let cx1 = (x_lo + step_oct - 1).div_euclid(256);
            let cz0 = z_lo.div_euclid(256);
            let cz1 = (z_lo + step_oct - 1).div_euclid(256);
            let mut all_interior = true;
            for cz in cz0..=cz1 {
                for cx in cx0..=cx1 {
                    let llx = cx - at.x * EDGE;
                    let llz = cz - at.z * EDGE;
                    debug_assert!(llx >= lo && llx < hi && llz >= lo && llz < hi);
                    if !interior[(llz - lo) as usize * cells_w + (llx - lo) as usize] {
                        all_interior = false;
                    }
                }
            }
            if all_interior {
                // The cell quads already tile it.
                flush(&mut run, wi, wj, tris);
                continue;
            }
            let corners = [
                display[wj * gw + wi],
                display[wj * gw + wi + 1],
                display[(wj + 1) * gw + wi],
                display[(wj + 1) * gw + wi + 1],
            ];
            let owner = CellPos { x: x_center_cell, z: z_center_cell };
            // A uniform window joins (or starts) a strip run. A run's quad
            // takes position-pure corner heights, and its edges lie on
            // sample-lattice lines where the bilinear surface is linear —
            // so the merge stays crack-free on sloped ground too (the quad
            // interior deviates from the surface by at most the tiny
            // cross-term over one window row, and nothing else draws
            // there). A uniform window straddling a point-height break is
            // the exception: its quad would bridge the split plates, so it
            // routes through the mixed emitter, whose bounded height arrangement splits it (the label's case is 15 — the full window polygon).
            if corners[0] != 0 && corners.iter().all(|&c| c == corners[0]) && !has_cliff {
                match run {
                    Some((label, cell, _)) if label == corners[0] && cell == owner => {}
                    _ => {
                        flush(&mut run, wi, wj, tris);
                        run = Some((corners[0], owner, wi));
                    }
                }
                continue;
            }
            flush(&mut run, wi, wj, tris);
            emit_mixed_window(world, cliffs, display, gw, wi, wj, corners, owner, &place, styles, tris);
        }
        flush(&mut run, windows, wj, tris);
    }
}

/// Emit one material label's quad over `rect`, colored by its material's
/// flat color and lifted position-pure — the strip-run emit. Windows
/// overhang their owner by half a window, so each corner reads the surface
/// through its own cell rather than the owner's clamped patch.
fn emit_label_quad(
    world: &World,
    label: u8,
    owner: CellPos,
    rect: [i32; 4],
    styles: &StyleTable,
    tris: &mut Vec<DrawTriangle>,
) {
    let material = Material::from_u8_or_void(label);
    let color = flat_color(styles.get(material));
    let surface = |wx: f32, wz: f32| label_lift(world, owner, wx, wz);
    emit_flat_quad(rect, color, &surface, tris);
}

pub(super) fn label_case_is_connected(corners: [u8; 4], label: u8, case: u8) -> bool {
    if case != 5 && case != 10 {
        return true;
    }
    // The other diagonal pair: [BR, TL] for case 5, [BL, TR] for case 10.
    let (other_a, other_b) = if case == 5 {
        (corners[1], corners[2])
    } else {
        (corners[0], corners[3])
    };
    other_a != other_b || label > other_a
}

/// Emit every label's case polygon for one mixed boundary window, colored
/// by its material's flat color and lifted per vertex through the vertex's
/// own cell. A two-label saddle resolves by label order — the higher label
/// connects its diagonal, the lower splits — so the window always tiles
/// exactly.
#[allow(clippy::too_many_arguments)] // one call site; the window state travels together
fn emit_mixed_window(
    world: &World,
    cliffs: &CliffPlan,
    display: &[u8],
    gw: usize,
    wi: usize,
    wj: usize,
    corners: [u8; 4],
    owner: CellPos,
    place: &GridPlacement,
    styles: &StyleTable,
    tris: &mut Vec<DrawTriangle>,
) {
    let step_oct = place.step_oct;
    let half = step_oct / 2;
    let x_lo = place.origin_oct[0] + wi as i32 * step_oct;
    let z_lo = place.origin_oct[1] + wj as i32 * step_oct;
    let x_hi = x_lo + step_oct;
    let z_hi = z_lo + step_oct;
    // Window point positions in octimeters, contour point numbering
    // (corners 0..4, edge midpoints 4..8).
    let points = [
        [x_lo, z_lo],
        [x_hi, z_lo],
        [x_hi, z_hi],
        [x_lo, z_hi],
        [x_lo + half, z_lo],
        [x_hi, z_lo + half],
        [x_lo + half, z_hi],
        [x_lo, z_lo + half],
    ]
    .map(|[px, pz]| [px as f32, pz as f32]);
    let center = WindowCenter { x_octimeters: x_lo + half, z_octimeters: z_lo + half };
    let has_cliff = cliffs.has_window_at(center);
    let mut cap_fragment_count = 0;
    for k in 0..4 {
        let label = corners[k];
        if corners[..k].contains(&label) || (label == 0 && !has_cliff) {
            continue;
        }
        let case = label_case(display, gw, wi, wj, label);
        let connected = label_case_is_connected(corners, label, case);
        let material = Material::from_u8_or_void(label);
        let color = flat_color(styles.get(material));
        let vertex = |pos: [f32; 2], sides: [Option<(i32, bool)>; 2]| {
            let wx = pos[0] / OCTIMETERS_PER_METER;
            let wz = pos[1] / OCTIMETERS_PER_METER;
            let y = fragment_lift(world, owner, sides, wx, wz);
            Vertex { x: wx, y, z: wz, color }
        };
        for poly in label_window_polys(case, connected) {
            if poly.is_empty() {
                continue;
            }
            let pts: Vec<[f32; 2]> = poly.iter().map(|&idx| points[idx as usize]).collect();
            if has_cliff {
                let material_points: Vec<PlanarPoint> =
                    pts.iter().map(|point| PlanarPoint { x_oct: point[0], z_oct: point[1] }).collect();
                cliffs.emit_cap_polygon(
                    world,
                    center,
                    MaterialCap { polygon: &material_points, material },
                    &mut cap_fragment_count,
                    styles,
                    tris,
                );
                continue;
            }
            for index in 1..pts.len() - 1 {
                tris.push(DrawTriangle {
                    verts: [
                        vertex(pts[0], [None, None]),
                        vertex(pts[index], [None, None]),
                        vertex(pts[index + 1], [None, None]),
                    ],
                });
            }
        }
    }
}
