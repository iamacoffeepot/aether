use alloc::vec;
use alloc::vec::Vec;

use aether_capabilities::render::{DrawTriangle, Vertex};
use aether_math::Rgb;

use crate::world::{CellPos, ChunkPos, Material, STEP_MAX_OCTIMETERS, World};

use super::constants::{EDGE, OCTIMETERS_PER_METER, OCTIMETERS_PER_SUBCELL, SUB};
use super::contour::{GridPlacement, label_case, label_window_polys};
use super::geometry::{emit_flat_quad, push_wall_quad};
use super::partition::chunk_placement;
use super::style::{StyleTable, flat_color};
use super::surface::{floor_to_i32, fragment_lift, label_lift, point_surface_level_at};

/// The break lines crossing a window's interior, per axis, in octimeters:
/// `Some(midline)` when the window's corner-sample point levels split past
/// the step ceiling across that axis. Levels differ across an axis only
/// when the two sample columns (rows) sit in different subcells, so a
/// returned midline is always a subcell lattice line — the line the break
/// stands on and the relief walls loft from. A flap polygon spanning a
/// returned line must be clipped there ([`emit_clipped_flap`]): a cap fan
/// must never connect vertices whose plates split.
///
/// Reads the point-surface levels directly rather than gating on
/// [`World::cell_has_height_relief`], so a **whole-cell** height cliff — a
/// plate break at a cell boundary with no subcell relief — also returns a
/// break line. That is what lets a chamfered relief-free material boundary
/// clip its overhang flap so the sliver closer ([`emit_clip_gap_walls`]) can
/// seal it; a flat cell splits nothing (equal levels) and stays break-free.
fn window_break_lines(
    world: &World,
    x_lo: i32,
    z_lo: i32,
    step_oct: i32,
) -> (Option<i32>, Option<i32>) {
    let x_hi = x_lo + step_oct;
    let z_hi = z_lo + step_oct;
    // Corner-sample point levels in [BL, BR, TR, TL] order.
    let level = [[x_lo, z_lo], [x_hi, z_lo], [x_hi, z_hi], [x_lo, z_hi]]
        .map(|[px, pz]| point_surface_level_at(world, px, pz));
    let x_break = (level[0] - level[1]).abs() > STEP_MAX_OCTIMETERS
        || (level[3] - level[2]).abs() > STEP_MAX_OCTIMETERS;
    let z_break = (level[0] - level[3]).abs() > STEP_MAX_OCTIMETERS
        || (level[1] - level[2]).abs() > STEP_MAX_OCTIMETERS;
    let half = step_oct / 2;
    (
        x_break.then_some(x_lo + half),
        z_break.then_some(z_lo + half),
    )
}

/// Split a convex polygon (octimeter coordinates) by the axis-aligned line
/// `coord[axis] = line`, returning the `(below, above)` sides in boundary
/// order. A vertex exactly on the line joins both sides, and a crossed
/// edge gains its intersection on both — so the two fragments share their
/// cut edge vertex-for-vertex.
fn split_poly_at(poly: &[[f32; 2]], axis: usize, line: f32) -> (Vec<[f32; 2]>, Vec<[f32; 2]>) {
    let mut below = Vec::new();
    let mut above = Vec::new();
    for (i, &a) in poly.iter().enumerate() {
        let b = poly[(i + 1) % poly.len()];
        let da = a[axis] - line;
        let db = b[axis] - line;
        if da <= 0.0 {
            below.push(a);
        }
        if da >= 0.0 {
            above.push(a);
        }
        if (da < 0.0 && db > 0.0) || (da > 0.0 && db < 0.0) {
            let t = da / (da - db);
            let mut p = [0.0f32; 2];
            p[axis] = line;
            p[1 - axis] = a[1 - axis] + (b[1 - axis] - a[1 - axis]) * t;
            below.push(p);
            above.push(p);
        }
    }
    (below, above)
}

/// Twice the area of a polygon (shoelace, absolute) — the degenerate-
/// fragment filter for [`emit_clipped_flap`].
fn poly_area_doubled(poly: &[[f32; 2]]) -> f32 {
    let mut sum = 0.0f32;
    for (i, &a) in poly.iter().enumerate() {
        let b = poly[(i + 1) % poly.len()];
        sum += a[0] * b[1] - b[0] * a[1];
    }
    sum.abs()
}

/// One clipped flap fragment: its polygon (octimeter coordinates) and its
/// side of each clipped break line per axis (`false` = below) — the datum
/// [`fragment_lift`] resolves an on-line vertex's subcell through.
type FlapFragment = (Vec<[f32; 2]>, [Option<(i32, bool)>; 2]);

/// Clip one label flap polygon (octimeter coordinates) along the window's
/// break midlines, so no fan triangle can connect vertices whose plates
/// split — the slanted bridge a whole-window fan draws across an internal
/// break. With no break lines the polygon passes through whole; degenerate
/// slivers of a clip are dropped.
fn clip_window_poly(
    poly: &[[f32; 2]],
    clip_x: Option<i32>,
    clip_z: Option<i32>,
) -> Vec<FlapFragment> {
    let mut fragments: Vec<FlapFragment> = vec![(poly.to_vec(), [None, None])];
    for (axis, clip) in [clip_x, clip_z].into_iter().enumerate() {
        let Some(line) = clip else {
            continue;
        };
        fragments = fragments
            .into_iter()
            .flat_map(|(frag, sides)| {
                let (below, above) = split_poly_at(&frag, axis, line as f32);
                let mut lo_sides = sides;
                lo_sides[axis] = Some((line, false));
                let mut hi_sides = sides;
                hi_sides[axis] = Some((line, true));
                [(below, lo_sides), (above, hi_sides)]
            })
            .collect();
    }
    fragments.retain(|(frag, _)| frag.len() >= 3 && poly_area_doubled(frag) >= 1.0);
    fragments
}

/// Close the vertical gaps a clipped flap opens **inside itself**: where
/// two fragments of one flap meet across a clipped break line at split
/// plates, no other wall class stands — the display is uniform there (one
/// flap, so no marched boundary), and where the flanking authored
/// materials differ the relief walk skips the edge as marched territory.
/// The regime is the marching chamfer at a silhouette corner: the cut
/// leaves a sliver of this label's display over the far side's raised (or
/// dropped) fabric, honestly lifted to that fabric's plate, and its
/// lattice-line edges would otherwise be open cracks. The shared chord of
/// each adjacent fragment pair is walled from the higher side's lift down to
/// the lower's committed edge — gated to authored-material-differing lines so
/// a same-material break stays the closure walk's (and only its) face.
#[allow(clippy::too_many_lines)] // one pass: pair fragments, gate, loft
fn emit_clip_gap_walls(
    world: &World,
    owner: CellPos,
    fragments: &[FlapFragment],
    styles: &StyleTable,
    tris: &mut Vec<DrawTriangle>,
) {
    let mut color: Option<Rgb> = None;
    for axis in 0..2 {
        for (fa, sa) in fragments {
            let Some((line, false)) = sa[axis] else {
                continue;
            };
            for (fb, sb) in fragments {
                if sb[axis] != Some((line, true)) || sb[1 - axis] != sa[1 - axis] {
                    continue;
                }
                // The shared chord: each side's on-line vertex extent along
                // the other axis, overlapped.
                let extent = |frag: &[[f32; 2]]| -> Option<(f32, f32)> {
                    let mut lo = f32::MAX;
                    let mut hi = f32::MIN;
                    for v in frag {
                        if (v[axis] - line as f32).abs() < 0.5 {
                            lo = lo.min(v[1 - axis]);
                            hi = hi.max(v[1 - axis]);
                        }
                    }
                    (hi > lo).then_some((lo, hi))
                };
                let (Some((a_lo, a_hi)), Some((b_lo, b_hi))) = (extent(fa), extent(fb)) else {
                    continue;
                };
                let lo = a_lo.max(b_lo);
                let hi = a_hi.min(b_hi);
                if hi - lo < 0.5 {
                    continue;
                }
                // Authored materials across the line at the chord: a
                // same-material break is the relief walk's face, not ours.
                let lattice = line / OCTIMETERS_PER_SUBCELL;
                let sub_other = floor_to_i32((lo + hi) * 0.5 / OCTIMETERS_PER_SUBCELL as f32);
                let material_of = |sub_axis: i32| {
                    let (sx, sz) = if axis == 0 {
                        (sub_axis, sub_other)
                    } else {
                        (sub_other, sub_axis)
                    };
                    world.underlay_point(
                        CellPos {
                            x: sx.div_euclid(SUB),
                            z: sz.div_euclid(SUB),
                        },
                        sx.rem_euclid(SUB),
                        sz.rem_euclid(SUB),
                    )
                };
                if material_of(lattice - 1) == material_of(lattice) {
                    continue;
                }
                // Lift the chord endpoints through both fragments' sides.
                let endpoint = |other: f32| -> [f32; 2] {
                    let mut p = [0.0f32; 2];
                    p[axis] = line as f32;
                    p[1 - axis] = other;
                    p
                };
                let lift = |p: [f32; 2], sides: [Option<(i32, bool)>; 2]| {
                    fragment_lift(
                        world,
                        owner,
                        sides,
                        p[0] / OCTIMETERS_PER_METER,
                        p[1] / OCTIMETERS_PER_METER,
                    )
                };
                let (c0, c1) = (endpoint(lo), endpoint(hi));
                let below = [lift(c0, *sa), lift(c1, *sa)];
                let above = [lift(c0, *sb), lift(c1, *sb)];
                if (below[0] - above[0]).abs() < f32::EPSILON
                    && (below[1] - above[1]).abs() < f32::EPSILON
                {
                    continue; // the plates agree — no gap to close
                }
                let (top, base) = if below[0] + below[1] > above[0] + above[1] {
                    (below, above)
                } else {
                    (above, below)
                };
                let face = *color
                    .get_or_insert_with(|| flat_color(styles.get(world.cliff_material(owner))));
                push_wall_quad(
                    tris,
                    [
                        c0[0] / OCTIMETERS_PER_METER,
                        c0[1] / OCTIMETERS_PER_METER,
                        top[0],
                    ],
                    [
                        c1[0] / OCTIMETERS_PER_METER,
                        c1[1] / OCTIMETERS_PER_METER,
                        top[1],
                    ],
                    base[0],
                    base[1],
                    face,
                );
            }
        }
    }
}

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
    let flush = |run: &mut Option<(u8, CellPos, usize)>,
                 end_wi: usize,
                 wj: usize,
                 tris: &mut Vec<DrawTriangle>| {
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
            let owner = CellPos {
                x: x_center_cell,
                z: z_center_cell,
            };
            // A uniform window joins (or starts) a strip run. A run's quad
            // takes position-pure corner heights, and its edges lie on
            // sample-lattice lines where the bilinear surface is linear —
            // so the merge stays crack-free on sloped ground too (the quad
            // interior deviates from the surface by at most the tiny
            // cross-term over one window row, and nothing else draws
            // there). A uniform window straddling a point-height break is
            // the exception: its quad would bridge the split plates, so it
            // routes through the mixed emitter, whose break clipping splits
            // it (the label's case is 15 — the full window polygon).
            if corners[0] != 0 && corners.iter().all(|&c| c == corners[0]) {
                let (bx, bz) = window_break_lines(world, x_lo, z_lo, step_oct);
                if bx.is_none() && bz.is_none() {
                    match run {
                        Some((label, cell, _)) if label == corners[0] && cell == owner => {}
                        _ => {
                            flush(&mut run, wi, wj, tris);
                            run = Some((corners[0], owner, wi));
                        }
                    }
                    continue;
                }
            }
            flush(&mut run, wi, wj, tris);
            emit_mixed_window(
                world, display, gw, wi, wj, corners, owner, &place, styles, tris,
            );
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

/// Emit every label's case polygon for one mixed boundary window, colored
/// by its material's flat color and lifted per vertex through the vertex's
/// own cell. A two-label saddle resolves by label order — the higher label
/// connects its diagonal, the lower splits — so the window always tiles
/// exactly.
#[allow(clippy::too_many_arguments)] // one call site; the window state travels together
fn emit_mixed_window(
    world: &World,
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
    // The break midlines a flap must be clipped along — a fan across a
    // split would draw a slanted bridge over the break instead of letting
    // the walls close it.
    let (clip_x, clip_z) = window_break_lines(world, x_lo, z_lo, step_oct);
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
    for k in 0..4 {
        let label = corners[k];
        if label == 0 || corners[..k].contains(&label) {
            continue;
        }
        let case = label_case(display, gw, wi, wj, label);
        let connected = if case == 5 || case == 10 {
            // The other diagonal pair: [BR, TL] for case 5, [BL, TR] for
            // case 10.
            let (o1, o2) = if case == 5 {
                (corners[1], corners[2])
            } else {
                (corners[0], corners[3])
            };
            o1 != o2 || label > o1
        } else {
            true
        };
        let material = Material::from_u8_or_void(label);
        let color = flat_color(styles.get(material));
        let vertex = |pos: [f32; 2], sides: [Option<(i32, bool)>; 2]| {
            let wx = pos[0] / OCTIMETERS_PER_METER;
            let wz = pos[1] / OCTIMETERS_PER_METER;
            let y = fragment_lift(world, owner, sides, wx, wz);
            Vertex {
                x: wx,
                y,
                z: wz,
                color,
            }
        };
        for poly in label_window_polys(case, connected) {
            if poly.is_empty() {
                continue;
            }
            let pts: Vec<[f32; 2]> = poly.iter().map(|&idx| points[idx as usize]).collect();
            let fragments = clip_window_poly(&pts, clip_x, clip_z);
            for (frag, sides) in &fragments {
                for k in 1..frag.len() - 1 {
                    tris.push(DrawTriangle {
                        verts: [
                            vertex(frag[0], *sides),
                            vertex(frag[k], *sides),
                            vertex(frag[k + 1], *sides),
                        ],
                    });
                }
            }
            if clip_x.is_some() || clip_z.is_some() {
                emit_clip_gap_walls(world, owner, &fragments, styles, tris);
            }
        }
    }
}
