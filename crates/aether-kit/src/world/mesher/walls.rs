use aether_capabilities::render::DrawTriangle;
use aether_math::Rgb;

use crate::world::{CellPos, ChunkPos, Material, STEP_MAX_OCTIMETERS, World};

use super::constants::{
    EDGE, OCTIMETERS_PER_METER, OCTIMETERS_PER_SUBCELL, SUB, SUBCELLS_PER_CHUNK_EDGE,
};
use super::geometry::push_wall_quad;
use super::partition::DisplayPartition;
use super::style::{StyleTable, flat_color};
use super::surface::{anchored_lift, point_surface_level_at};
use super::voids::{void_floor_level, void_low_base};

const WALL_SEGMENTS: [&[(u8, u8)]; 16] = [
    &[],               // 0
    &[(7, 4)],         // 1  BL
    &[(4, 5)],         // 2  BR
    &[(7, 5)],         // 3  BL BR
    &[(5, 6)],         // 4  TR
    &[(7, 4), (5, 6)], // 5  BL TR (saddle)
    &[(4, 6)],         // 6  BR TR
    &[(7, 6)],         // 7  BL BR TR
    &[(6, 7)],         // 8  TL
    &[(4, 6)],         // 9  BL TL
    &[(4, 5), (6, 7)], // 10 BR TL (saddle)
    &[(5, 6)],         // 11 BL BR TL
    &[(7, 5)],         // 12 TR TL
    &[(4, 5)],         // 13 BL TR TL
    &[(4, 7)],         // 14 BR TR TL
    &[],               // 15
];

/// Emit the chunk's vertical cliff faces by closure: a wall lofts wherever
/// the two sides of a shared boundary edge committed different cap heights,
/// so the split decision lives in one place — the plate lift the caps drew
/// from — and can never drift between a predicting wall pass and the cap it
/// should meet. The lattice/subcell closure ([`emit_lattice_closure`]) walls
/// every same-material cell-edge and subcell-edge break; the marched closure
/// ([`emit_contour_closure`]) walls every material or Void boundary along
/// the display partition's contours, dropping a fillable Void low side to
/// its groove floor. A face lofts iff the high side committed strictly above
/// the low side there — a legal step merges the plates (the committed edges
/// agree) and lofts nothing, a cliff splits them and lofts a face whose top
/// and bottom vertices are the two caps' shared-edge vertices, watertight by
/// construction.
pub(super) fn emit_walls(
    world: &World,
    at: ChunkPos,
    styles: &StyleTable,
    partition: Option<&DisplayPartition>,
    tris: &mut Vec<DrawTriangle>,
) {
    emit_lattice_closure(world, at, styles, tris);
    if let Some(part) = partition {
        emit_contour_closure(world, at, part, styles, tris);
    }
}

/// The flat wall color for a cliff owned by `cell`: the cell's cliff
/// material's flat color. A pure function of the cell, so the two sides of a
/// shared edge agree on the color.
fn wall_color(world: &World, cell: CellPos, styles: &StyleTable) -> Rgb {
    flat_color(styles.get(world.cliff_material(cell)))
}

fn same_committed_edge(top: [f32; 2], bottom: [f32; 2]) -> bool {
    (top[0] - bottom[0]).abs() < f32::EPSILON && (top[1] - bottom[1]).abs() < f32::EPSILON
}

fn lattice_corner_position(base_x: f32, base_z: f32, span: f32, corner: usize) -> (f32, f32) {
    (
        base_x + if corner % 2 == 1 { span } else { 0.0 },
        base_z + if corner >= 2 { span } else { 0.0 },
    )
}

/// Close the same-material cliff faces of the chunk on the cell lattice — or,
/// where a cell carries authored relief, on its subcell lattice, the stride
/// its cap tessellated at. For every chunk-local cell and each of its four
/// outgoing shared edges the two sides' committed cap edges are read through
/// the same corner plates the caps drew from
/// ([`World::cell_corner_heights`] / [`World::subcell_corner_heights`]); a
/// face lofts iff this (the higher) side committed above the neighbor and the
/// committed edges differ. A merged plate — flat ground or a legal step —
/// leaves the edges equal and lofts nothing, so the step size is never gated
/// directly; the plate merge is the one split decision. The high side owns
/// the face and cells iterate chunk-local, so a chunk-border cliff lofts
/// exactly once fleet-wide. A material or Void boundary edge is skipped here
/// — the marched closure owns those.
#[allow(clippy::too_many_lines)] // one closure walk over the cell and subcell strides
fn emit_lattice_closure(
    world: &World,
    at: ChunkPos,
    styles: &StyleTable,
    tris: &mut Vec<DrawTriangle>,
) {
    let sub_span = 1.0 / SUB as f32;
    for lz in 0..EDGE {
        for lx in 0..EDGE {
            let cell = CellPos {
                x: at.x * EDGE + lx,
                z: at.z * EDGE + lz,
            };
            // Relief cells close on the subcell lattice (the stride their
            // caps tessellated at); every other cell closes on the cell
            // lattice.
            let relief = world.cell_has_height_relief(cell);
            let face_rgb = wall_color(world, cell, styles);
            if relief {
                for sj in 0..SUB {
                    for si in 0..SUB {
                        // The point's own material, not the cell's cascade — a
                        // stone point on a grass cell closes its own
                        // same-material breaks, and a Void point's rim lofts as
                        // a marched wall.
                        let material = world.underlay_point(cell, si, sj);
                        if material == Material::Void {
                            continue; // a hole's rim lofts as a marched wall
                        }
                        let level = world.point_surface_level(cell, si, sj);
                        let mut cached: Option<[f32; 4]> = None;
                        for edge in &WALL_DIRECTIONS {
                            let gx = cell.x * SUB + si + edge.offset.0;
                            let gz = cell.z * SUB + sj + edge.offset.1;
                            let neighbor = CellPos {
                                x: gx.div_euclid(SUB),
                                z: gz.div_euclid(SUB),
                            };
                            let nsx = gx.rem_euclid(SUB);
                            let nsz = gz.rem_euclid(SUB);
                            if world.underlay_point(neighbor, nsx, nsz) != material {
                                continue; // material / Void boundaries loft as marched walls
                            }
                            // Ownership: only the strictly-higher point lofts,
                            // so each face is emitted once. The step size is
                            // not gated — a legal step merges the point plates
                            // and the committed-edge test below leaves nothing.
                            if level <= world.point_surface_level(neighbor, nsx, nsz) {
                                continue;
                            }
                            let top = *cached
                                .get_or_insert_with(|| world.subcell_corner_heights(cell, si, sj));
                            let bottom = world.subcell_corner_heights(neighbor, nsx, nsz);
                            let y_top = [top[edge.top[0]], top[edge.top[1]]];
                            let y_low = [bottom[edge.bottom[0]], bottom[edge.bottom[1]]];
                            if same_committed_edge(y_top, y_low) {
                                continue; // the point plates merged — no break to close
                            }
                            let base_x = cell.x as f32 + si as f32 * sub_span;
                            let base_z = cell.z as f32 + sj as f32 * sub_span;
                            let (x0, z0) =
                                lattice_corner_position(base_x, base_z, sub_span, edge.top[0]);
                            let (x1, z1) =
                                lattice_corner_position(base_x, base_z, sub_span, edge.top[1]);
                            push_wall_quad(
                                tris,
                                [x0, z0, y_top[0]],
                                [x1, z1, y_top[1]],
                                y_low[0],
                                y_low[1],
                                face_rgb,
                            );
                        }
                    }
                }
            } else {
                let material = world.underlay(cell);
                if material == Material::Void {
                    continue; // a Void cell's rim is the marched closure's face
                }
                let cell_level = world.surface_level(cell);
                let mut cached: Option<[f32; 4]> = None;
                for edge in &WALL_DIRECTIONS {
                    let neighbor = CellPos {
                        x: cell.x + edge.offset.0,
                        z: cell.z + edge.offset.1,
                    };
                    if world.underlay(neighbor) != material {
                        continue; // a material boundary lofts as a marched wall
                    }
                    // Ownership: only the strictly-higher cell lofts. The step
                    // size is not gated — a legal step merges the plates and
                    // the committed-edge test below leaves nothing to close.
                    if cell_level <= world.surface_level(neighbor) {
                        continue;
                    }
                    let top = *cached.get_or_insert_with(|| world.cell_corner_heights(cell));
                    let bottom = world.cell_corner_heights(neighbor);
                    let y_top = [top[edge.top[0]], top[edge.top[1]]];
                    let y_low = [bottom[edge.bottom[0]], bottom[edge.bottom[1]]];
                    if same_committed_edge(y_top, y_low) {
                        continue; // the plates merged — no break to close
                    }
                    let (x0, z0) =
                        lattice_corner_position(cell.x as f32, cell.z as f32, 1.0, edge.top[0]);
                    let (x1, z1) =
                        lattice_corner_position(cell.x as f32, cell.z as f32, 1.0, edge.top[1]);
                    push_wall_quad(
                        tris,
                        [x0, z0, y_top[0]],
                        [x1, z1, y_top[1]],
                        y_low[0],
                        y_low[1],
                        face_rgb,
                    );
                }
            }
        }
    }
}

/// Close the marched cliff walls: over the boundary windows of the display
/// partition, wherever a segment separates two materials — or a material
/// from a Void hole — whose point surface levels split the caps, drop the
/// segment's marched contour from the high cap's committed edge to the low
/// side's. Each window is owned by the cell under its center, the same
/// ownership the cap walk uses; the owner is the fleet-wide dedup key (a
/// window emits only from the chunk holding its owner) and the color source,
/// never the gate — the split is per-segment and side-aware, reading the
/// `a`-side corner's point level against the crossed side's, because along a
/// contour the window center alternates sides subcell to subcell and an
/// owner-level test would picket-fence the wall. A wall's top vertices lift
/// through the identical anchored patch ([`anchored_lift`] on the `a`-side
/// corner sample) the high cap's crossing vertices took — landing the top
/// edge exactly on the cap's contour, watertight by construction under any
/// smoothing displacement. A grounded low side takes per-endpoint bases on
/// the low side's own committed lift over relief (a flat min base where no
/// relief engages), coincident with the low cap. A Void low side closes to
/// its fill-over floor ([`World::point_height`], the total height plane) when
/// the joint is enclosed ([`void_fill_border`]) — wall down, floor across
/// ([`emit_void_floors`]), wall up at the far rim — else drops the
/// unbounded-void skirt [`WALL_VOID_SKIRT_OCTIMETERS`] so an open hole reads
/// as thick ground.
#[allow(clippy::too_many_lines)] // one boundary walk: classify sides, extract segments, close
#[allow(clippy::similar_names)] // the segment's two endpoints read clearest as `_p` / `_q` pairs
fn emit_contour_closure(
    world: &World,
    at: ChunkPos,
    part: &DisplayPartition,
    styles: &StyleTable,
    tris: &mut Vec<DrawTriangle>,
) {
    let display = &part.display;
    let gw = part.gw;
    let apron = part.apron;
    let step_oct = part.step_oct;
    let base_oct = [
        at.x * SUBCELLS_PER_CHUNK_EDGE * OCTIMETERS_PER_SUBCELL,
        at.z * SUBCELLS_PER_CHUNK_EDGE * OCTIMETERS_PER_SUBCELL,
    ];
    let origin_oct = [
        base_oct[0] - apron * OCTIMETERS_PER_SUBCELL + step_oct / 2,
        base_oct[1] - apron * OCTIMETERS_PER_SUBCELL + step_oct / 2,
    ];
    let half = step_oct / 2;
    let windows = gw - 1;
    for wj in 0..windows {
        for wi in 0..windows {
            let x_lo = origin_oct[0] + wi as i32 * step_oct;
            let z_lo = origin_oct[1] + wj as i32 * step_oct;
            let x_hi = x_lo + step_oct;
            let z_hi = z_lo + step_oct;
            // Ownership: the cell under the window center, chunk-local only —
            // covers every window exactly once across the fleet and matches
            // the cap walk's owner, so the lofted top reuses the cap's patch.
            let owner = CellPos {
                x: (x_lo + half).div_euclid(256),
                z: (z_lo + half).div_euclid(256),
            };
            if !(0..EDGE).contains(&(owner.x - at.x * EDGE))
                || !(0..EDGE).contains(&(owner.z - at.z * EDGE))
            {
                continue;
            }
            // Corner materials in order [BL, BR, TR, TL] (the case-bit
            // order), folding the rim/body display split back to the plain
            // material — a wall follows a material boundary, not a rim edge.
            let mats = [
                display[wj * gw + wi].div_ceil(2),
                display[wj * gw + wi + 1].div_ceil(2),
                display[(wj + 1) * gw + wi + 1].div_ceil(2),
                display[(wj + 1) * gw + wi].div_ceil(2),
            ];
            if mats.iter().all(|&m| m == mats[0]) {
                continue; // uniform window — no material boundary to loft
            }
            // Does authored relief engage anywhere this window can read? The
            // per-endpoint wall bases below switch on this, so relief-free
            // worlds keep their flat base byte-identically.
            let relief = world.cell_has_height_relief(owner);
            // Corner sample positions and point surface levels, same order
            // as `mats`. Point levels (not the cell level) so an authored
            // break inside a cell — a silhouette raised by deltas alone,
            // the cell height flat — lofts its wall, and the gate reads the
            // sample the cap actually drew. The positions double as the
            // wall's side anchors ([`anchored_lift`]).
            let corner_oct = [[x_lo, z_lo], [x_hi, z_lo], [x_hi, z_hi], [x_lo, z_hi]];
            let point_level = corner_oct.map(|[cx, cz]| point_surface_level_at(world, cx, cz));
            let mid = |m: u8| -> [i32; 2] {
                match m {
                    4 => [x_lo + half, z_lo],
                    5 => [x_hi, z_lo + half],
                    6 => [x_lo + half, z_hi],
                    _ => [x_lo, z_lo + half],
                }
            };
            let edge_corners = |m: u8| -> (usize, usize) {
                match m {
                    4 => (0, 1),
                    5 => (1, 2),
                    6 => (2, 3),
                    _ => (3, 0),
                }
            };
            // Wall color: the owning cell's cliff material as a flat color —
            // the owner is the color source even when the segment's high side
            // lies in a neighboring cell.
            let face_rgb = flat_color(styles.get(world.cliff_material(owner)));
            for a in 1..6u8 {
                if !mats.contains(&a) {
                    continue;
                }
                let case = u8::from(mats[0] == a)
                    | u8::from(mats[1] == a) << 1
                    | u8::from(mats[2] == a) << 2
                    | u8::from(mats[3] == a) << 3;
                for &(p, q) in WALL_SEGMENTS[case as usize] {
                    // The two sides of each crossed edge: the `a` (high
                    // candidate) corner and the non-`a` (low) corner, each
                    // read at point level, each carrying its corner sample
                    // as the side anchor. The gate is side-aware and
                    // per-segment — never the window center, whose side
                    // alternates along a contour and would picket-fence the
                    // wall with skipped windows.
                    let sides_of = |m: u8| {
                        let (i, j) = edge_corners(m);
                        let (hi, lo) = if mats[i] == a { (i, j) } else { (j, i) };
                        (
                            mats[lo],
                            point_level[lo],
                            point_level[hi],
                            corner_oct[lo],
                            corner_oct[hi],
                        )
                    };
                    let (mat_p, lvl_p, hi_p, lo_anchor_p, hi_anchor_p) = sides_of(p);
                    let (mat_q, lvl_q, hi_q, lo_anchor_q, hi_anchor_q) = sides_of(q);
                    let is_void = mat_p == 0 || mat_q == 0;
                    let low_level = lvl_p.min(lvl_q);
                    let high_level = hi_p.max(hi_q);
                    let mp = mid(p);
                    let mq = mid(q);
                    let wx_p = mp[0] as f32 / OCTIMETERS_PER_METER;
                    let wz_p = mp[1] as f32 / OCTIMETERS_PER_METER;
                    let wx_q = mq[0] as f32 / OCTIMETERS_PER_METER;
                    let wz_q = mq[1] as f32 / OCTIMETERS_PER_METER;
                    // Tops anchored to each endpoint's `a`-side corner — the
                    // identical anchor the high flap's crossing vertex took
                    // ([`window_point_anchor`]), so the seam is shared
                    // vertices under any smoothing displacement.
                    let yt_p = anchored_lift(world, owner, hi_anchor_p, wx_p, wz_p);
                    let yt_q = anchored_lift(world, owner, hi_anchor_q, wx_q, wz_q);
                    // The low side's committed base, per endpoint. A material
                    // boundary reads the low cap's own committed lift (over
                    // relief the low cap splits to its own plates, so a flat
                    // min base would gap where the low ground varies along the
                    // segment; a relief-free owner keeps the flat min base
                    // byte-identically). A Void low side closes to its
                    // fill-over floor when the joint is enclosed — wall down,
                    // floor across ([`emit_void_floors`]), wall up — coincident
                    // with the groove floor cap; an unbounded void keeps the
                    // open skirt.
                    let (yb_p, yb_q) = if is_void {
                        (
                            void_low_base(world, lo_anchor_p, yt_p),
                            void_low_base(world, lo_anchor_q, yt_q),
                        )
                    } else if relief {
                        (
                            anchored_lift(world, owner, lo_anchor_p, wx_p, wz_p),
                            anchored_lift(world, owner, lo_anchor_q, wx_q, wz_q),
                        )
                    } else {
                        let base = low_level as f32 / OCTIMETERS_PER_METER;
                        (base, base)
                    };
                    // The split test: a face closes only where the high side
                    // committed above the low side past the step ceiling — a
                    // merged plate (flat ground or a legal step) closes
                    // nothing. The solid side tests the point-level split (the
                    // plate break); the Void side tests the committed drop to
                    // its stored floor. The owner is only the dedup key and
                    // color source, never the split — a low-owned window still
                    // closes its segment.
                    if is_void {
                        // A Void low side lofts only where the high side stands
                        // past the step ceiling above the void's floor: its
                        // enclosed groove floor where authored below the
                        // surrounding ground ([`void_floor_level`]), else the
                        // lowest neighbor ground the skirt drops toward. A flat
                        // void edge — the high side level with its surroundings
                        // — lofts nothing, so a cliff-free world stays
                        // wall-free even where grass meets an open border.
                        let floor = void_floor_level(world, lo_anchor_p, owner)
                            .min(void_floor_level(world, lo_anchor_q, owner));
                        if high_level - floor <= STEP_MAX_OCTIMETERS {
                            continue;
                        }
                    } else if high_level - low_level <= STEP_MAX_OCTIMETERS {
                        continue;
                    }
                    push_wall_quad(
                        tris,
                        [wx_p, wz_p, yt_p],
                        [wx_q, wz_q, yt_q],
                        yb_p,
                        yb_q,
                        face_rgb,
                    );
                }
            }
        }
    }
}

/// Per wall direction: neighbor offset, the shared edge's two lattice
/// corners as indices into the high side's corner order, and the same
/// lattice points in the neighbor's order (the low side's plates). Used by
/// the closure walk ([`emit_lattice_closure`]) at both the cell and subcell
/// strides — the corner index order is the same at both.
struct WallEdge {
    offset: (i32, i32),
    top: [usize; 2],
    bottom: [usize; 2],
}

const WALL_DIRECTIONS: [WallEdge; 4] = [
    WallEdge {
        offset: (1, 0),
        top: [1, 3],
        bottom: [0, 2],
    },
    WallEdge {
        offset: (-1, 0),
        top: [0, 2],
        bottom: [1, 3],
    },
    WallEdge {
        offset: (0, 1),
        top: [2, 3],
        bottom: [0, 1],
    },
    WallEdge {
        offset: (0, -1),
        top: [0, 1],
        bottom: [2, 3],
    },
];
