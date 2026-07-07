// Chunk-local loop counters and world-cell / octimeter coordinates are
// small integers cast between i32 (coordinate math), usize (plane and
// grid indexing), and f32 (vertex output). The ranges — chunk-bounded
// cells, octimeter positions within a chunk plus a bounded apron — make
// the pedantic precision / sign / truncation lints non-issues here.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap
)]

//! The world-view mesher: a pure function over the plane stack
//! ([`crate::world`]) that turns one chunk into a triangle list, read by
//! the [`world_view`](super::world_view) actor and replayed to
//! `"aether.render"` each frame.
//!
//! [`mesh_chunk`] paints the gouache grammar in two passes over the chunk,
//! both pure and host-testable (no wgpu, no ctx). Every color and rim
//! decision is a function of world coordinates and the neighbor cells'
//! planes alone, so two chunks agree on their shared border with no shared
//! state.
//!
//! # Underlay pass — the keyed quilt
//!
//! One flat cell per non-[`Material::Void`] underlay cell at `y = 0`,
//! world-space in meters (`1 cell = 1 m`). The cell's color resolves from
//! its world center: the material's base HSL plus world-anchored value
//! noise on hue and lightness, converted to linear RGB
//! ([`resolve_cell`]). A low-amplitude wash grades the interior
//! lightness along a stroke flow field. A pooled rim darkens a cell side
//! where the paint changes — the neighbor is Void or a different material,
//! or the same material whose hue steps past the blob-merge threshold —
//! rendered as a nine-slice (interior, four edge strips, four corners); a
//! cell with no rimmed side collapses to one flat quad.
//!
//! # Overlay pass — corner-minimized contours
//!
//! Per distinct overlay material present, the material's binary subcell
//! field runs through [`minimize_corners`] (upsample, one analytic
//! 45-degree chamfer, angle-gated cellular passes) and then
//! [`march_grid`] into crisp contour polygons plus greedy-merged
//! interior. Smoothing params are spatial: each subcell reads its owning
//! cell's smoothing-field override ([`World::smoothing_override`]) with
//! the material's style row as the fallback, so where an edge reads soft
//! versus crisp is authored on the map. The pooled rim is a second marched
//! layer over an eroded mask: the smoothed shape draws in a darkened rim
//! color, and the body draws one octimeter above it inset by the rim
//! width. Water's body swaps the flat marched interior for a graded quad
//! per subcell fully inside the eroded region — lightness driven down by a
//! bounded shore-distance scan, hue varied per subcell — while the marched
//! rim band beneath carries the smoothed shoreline silhouette. Overlay
//! geometry lifts above the underlay so the coplanar passes never z-fight,
//! and it carries its own flat vertices so the seam stays hard.

pub mod contour;
pub mod style;

use alloc::vec;
use alloc::vec::Vec;

use aether_capabilities::render::{DrawTriangle, Vertex};

use crate::world::{
    CELLS_PER_CHUNK, CellPos, ChunkPos, Material, SUBCELLS_PER_CELL_EDGE, ViewMode, World,
};
use contour::{GridPlacement, SmoothParams, erode, march_grid, minimize_corners, push_quad};
use style::{
    ResolvedCell, hsl_to_linear_rgb, raw_field, resolve_cell, rim_strength, style, wash_lightness,
};

/// Cells along one chunk edge, as a plain `i32` for loop bounds.
const EDGE: i32 = CELLS_PER_CHUNK;

/// Subcells along one cell edge, as `i32`.
const SUB: i32 = SUBCELLS_PER_CELL_EDGE as i32;

/// Subcells along one chunk edge (`16 * 4 = 64`).
const SUBCELLS_PER_CHUNK_EDGE: i32 = EDGE * SUB;

/// Octimeters per subcell (`256 / SUB = 64`).
const OCTIMETERS_PER_SUBCELL: i32 = 256 / SUB;

/// Octimeters per meter, for the octimeter-to-meter conversion at vertex
/// emit.
const OCTIMETERS_PER_METER: f32 = 256.0;

/// The underlay's ground plane.
const UNDERLAY_Y: f32 = 0.0;

/// The overlay rim layer lift — one octimeter over the underlay.
const OVERLAY_RIM_LIFT: f32 = 1.0 / OCTIMETERS_PER_METER;

/// The overlay body layer lift — one octimeter over the rim, so the body
/// sits on top of the rim it insets from.
const OVERLAY_BODY_LIFT: f32 = 2.0 / OCTIMETERS_PER_METER;

/// Upsample factor for a non-water material's smoothed contour grid.
const CONTOUR_UPSAMPLE: usize = 2;

/// Apron cap in subcells (two cells) so a chunk's smoothing reads stay
/// within the eight-neighbor remesh the `R = 1` invalidation covers.
const MAX_APRON_SUBCELLS: i32 = 8;

/// Bounded outward scan length in subcells for the water shore-distance
/// derivation.
const MAX_SHORE_SCAN: i32 = 8;

/// Mesh one chunk into its triangle list. Pure — no wgpu, no ctx — so it
/// is unit-testable host-side. Reads neighbor cells through [`World`] (a
/// bounded apron); a missing neighbor reads as empty. `mode` selects the
/// painted gouache grammar or the raw grayscale calibration field.
#[must_use]
pub fn mesh_chunk(world: &World, at: ChunkPos, mode: ViewMode) -> Vec<DrawTriangle> {
    let mut tris = Vec::new();
    match mode {
        ViewMode::Raw => mesh_raw(world, at, &mut tris),
        ViewMode::Painted => {
            mesh_underlay(world, at, &mut tris);
            mesh_overlay(world, at, &mut tris);
        }
    }
    tris
}

/// The per-side pooled-rim strengths `[left, right, top, bottom]` for a
/// cell: a full rim where the neighbor is Void or a different material,
/// and a proportional rim where a same-material neighbor's hue steps past
/// the blob-merge threshold. A pure function of the cell and its four
/// neighbors, so the two cells sharing an edge agree on the rim there.
fn cell_rims(world: &World, cell: CellPos) -> [f32; 4] {
    let material = world.underlay(cell);
    let hue_a = resolve_cell(material, cell.x as f32 + 0.5, cell.z as f32 + 0.5, None).hue;
    let threshold = style(material).blob_merge_degrees;
    let sides = [
        (cell.x - 1, cell.z),
        (cell.x + 1, cell.z),
        (cell.x, cell.z - 1),
        (cell.x, cell.z + 1),
    ];
    let mut rims = [0.0f32; 4];
    for (k, (nx, nz)) in sides.iter().enumerate() {
        let neighbor = world.underlay(CellPos { x: *nx, z: *nz });
        let present = neighbor != Material::Void;
        let same = neighbor == material;
        let hue_b = if same {
            resolve_cell(neighbor, *nx as f32 + 0.5, *nz as f32 + 0.5, None).hue
        } else {
            hue_a
        };
        rims[k] = rim_strength(present, same, hue_a, hue_b, threshold);
    }
    rims
}

/// Emit the keyed-quilt underlay: one flat keyed cell per non-Void cell,
/// with pooled rims where the paint changes.
fn mesh_underlay(world: &World, at: ChunkPos, tris: &mut Vec<DrawTriangle>) {
    let base_x = at.x * EDGE;
    let base_z = at.z * EDGE;
    for lz in 0..EDGE {
        for lx in 0..EDGE {
            let cell = CellPos {
                x: base_x + lx,
                z: base_z + lz,
            };
            let material = world.underlay(cell);
            if material == Material::Void {
                continue;
            }
            let resolved = resolve_cell(material, cell.x as f32 + 0.5, cell.z as f32 + 0.5, None);
            let rims = cell_rims(world, cell);
            emit_underlay_cell(material, &resolved, cell.x, cell.z, rims, tris);
        }
    }
}

/// Emit one cell's geometry: a single flat quad when no side rims, else a
/// nine-slice whose edge strips and corners darken by the pooled-rim
/// factor. All geometry stays inside the cell — the rim only pools inward.
fn emit_underlay_cell(
    material: Material,
    resolved: &ResolvedCell,
    cx: i32,
    cz: i32,
    rims: [f32; 4],
    tris: &mut Vec<DrawTriangle>,
) {
    let s = style(material);
    let inset = s.rim_inset_octimeters;
    let x0 = cx * 256;
    let x3 = (cx + 1) * 256;
    let x1 = x0 + inset;
    let x2 = x3 - inset;
    let z0 = cz * 256;
    let z3 = (cz + 1) * 256;
    let z1 = z0 + inset;
    let z2 = z3 - inset;

    if rims.iter().all(|&r| r == 0.0) {
        emit_quad_shaded(material, resolved, [x0, z0, x3, z3], 0.0, tris);
        return;
    }
    let darken = s.rim_darken;
    let (left, right, top, bottom) = (rims[0], rims[1], rims[2], rims[3]);
    // Interior.
    emit_quad_shaded(material, resolved, [x1, z1, x2, z2], 0.0, tris);
    // Edge strips.
    emit_quad_shaded(material, resolved, [x0, z1, x1, z2], darken * left, tris);
    emit_quad_shaded(material, resolved, [x2, z1, x3, z2], darken * right, tris);
    emit_quad_shaded(material, resolved, [x1, z0, x2, z1], darken * top, tris);
    emit_quad_shaded(material, resolved, [x1, z2, x2, z3], darken * bottom, tris);
    // Corners darken by the stronger of their two adjacent sides.
    emit_quad_shaded(
        material,
        resolved,
        [x0, z0, x1, z1],
        darken * left.max(top),
        tris,
    );
    emit_quad_shaded(
        material,
        resolved,
        [x2, z0, x3, z1],
        darken * right.max(top),
        tris,
    );
    emit_quad_shaded(
        material,
        resolved,
        [x0, z2, x1, z3],
        darken * left.max(bottom),
        tris,
    );
    emit_quad_shaded(
        material,
        resolved,
        [x2, z2, x3, z3],
        darken * right.max(bottom),
        tris,
    );
}

/// Push the two triangles of one underlay quad spanning `rect`
/// (`[x0, z0, x1, z1]` octimeters), each corner shaded by the wash field
/// at its own world position and darkened by `rim_darken`.
fn emit_quad_shaded(
    material: Material,
    resolved: &ResolvedCell,
    rect: [i32; 4],
    rim_darken: f32,
    tris: &mut Vec<DrawTriangle>,
) {
    let corner = |xo: i32, zo: i32| {
        let wx = xo as f32 / OCTIMETERS_PER_METER;
        let wz = zo as f32 / OCTIMETERS_PER_METER;
        let light = wash_lightness(material, resolved.light, wx, wz, resolved.stroke);
        let light = (light * (1.0 - rim_darken)).clamp(0.0, 100.0);
        let c = hsl_to_linear_rgb(resolved.hue, resolved.sat, light);
        Vertex {
            x: wx,
            y: UNDERLAY_Y,
            z: wz,
            r: c[0],
            g: c[1],
            b: c[2],
        }
    };
    let a = corner(rect[0], rect[1]);
    let b = corner(rect[2], rect[1]);
    let c = corner(rect[2], rect[3]);
    let d = corner(rect[0], rect[3]);
    tris.push(DrawTriangle { verts: [a, b, c] });
    tris.push(DrawTriangle { verts: [a, c, d] });
}

/// Is the subcell at chunk-local index `(six, siz)` covered by `material`?
/// Indices range over the field plus its apron; an out-of-chunk index
/// resolves to a neighbor cell through [`World`], reading empty for a
/// missing chunk.
fn subcell_covered(world: &World, at: ChunkPos, six: i32, siz: i32, material: Material) -> bool {
    let cell = CellPos {
        x: at.x * EDGE + six.div_euclid(SUB),
        z: at.z * EDGE + siz.div_euclid(SUB),
    };
    if world.overlay(cell) != material {
        return false;
    }
    let bit = (siz.rem_euclid(SUB) * SUB + six.rem_euclid(SUB)) as u32;
    (world.overlay_mask(cell) >> bit) & 1 == 1
}

/// Emit the overlay pass: for each distinct overlay material present in
/// the chunk, contour its subcell field.
fn mesh_overlay(world: &World, at: ChunkPos, tris: &mut Vec<DrawTriangle>) {
    let Some(chunk) = world.chunk(at) else {
        return;
    };
    let mut present = [false; 6];
    for material in &chunk.overlay {
        if *material != Material::Void {
            present[*material as usize] = true;
        }
    }
    for (id, seen) in present.iter().enumerate() {
        if *seen {
            mesh_overlay_material(world, at, Material::from_u8_or_void(id as u8), tris);
        }
    }
}

/// Sample one overlay material's subcell field (plus its smoothing apron)
/// into a bounded bool grid.
fn sample_field(world: &World, at: ChunkPos, material: Material, apron: i32) -> (Vec<bool>, usize) {
    let n = (SUBCELLS_PER_CHUNK_EDGE + 2 * apron) as usize;
    let mut field = vec![false; n * n];
    for sj in -apron..SUBCELLS_PER_CHUNK_EDGE + apron {
        for si in -apron..SUBCELLS_PER_CHUNK_EDGE + apron {
            let idx = (sj + apron) as usize * n + (si + apron) as usize;
            field[idx] = subcell_covered(world, at, si, sj, material);
        }
    }
    (field, n)
}

/// Build the per-subcell smoothing params parallel to [`sample_field`]'s
/// grid: each subcell reads its owning cell's smoothing-field override
/// ([`World::smoothing_override`]), falling back to the material's style
/// row. Pure over world coordinates like every other paint decision, so
/// both sides of a chunk border resolve the same params through the apron.
fn sample_params(
    world: &World,
    at: ChunkPos,
    material: Material,
    apron: i32,
    n: usize,
) -> Vec<SmoothParams> {
    let s = style(material);
    let default = SmoothParams {
        iterations: s.smoothing_iterations,
        smoothing_degrees: s.smoothing_degrees,
    };
    let mut params = vec![default; n * n];
    for sj in -apron..SUBCELLS_PER_CHUNK_EDGE + apron {
        for si in -apron..SUBCELLS_PER_CHUNK_EDGE + apron {
            let cell = CellPos {
                x: at.x * EDGE + si.div_euclid(SUB),
                z: at.z * EDGE + sj.div_euclid(SUB),
            };
            if let Some(profile) = world.smoothing_override(cell) {
                let idx = (sj + apron) as usize * n + (si + apron) as usize;
                params[idx] = SmoothParams {
                    iterations: profile.iterations,
                    smoothing_degrees: profile.degrees,
                };
            }
        }
    }
    params
}

/// Contour one overlay material's subcell field: smooth its corners per
/// the spatial smoothing field, then march a rim layer and an inset body
/// layer. Water's body swaps the flat marched interior for per-subcell
/// depth-graded quads over the eroded region, keeping the smoothed rim
/// band as its shoreline silhouette. The apron is fixed at the maximum a
/// profile can demand ([`MAX_APRON_SUBCELLS`]), so field content never
/// changes a chunk's read reach.
fn mesh_overlay_material(
    world: &World,
    at: ChunkPos,
    material: Material,
    tris: &mut Vec<DrawTriangle>,
) {
    let s = style(material);
    let apron = MAX_APRON_SUBCELLS;
    let (field, n) = sample_field(world, at, material, apron);
    let params = sample_params(world, at, material, apron, n);
    let base_oct = [
        at.x * SUBCELLS_PER_CHUNK_EDGE * OCTIMETERS_PER_SUBCELL,
        at.z * SUBCELLS_PER_CHUNK_EDGE * OCTIMETERS_PER_SUBCELL,
    ];

    let upsample = CONTOUR_UPSAMPLE;
    let (smoothed, gw, gh) = minimize_corners(&field, n, n, upsample, &params);
    let step_oct = OCTIMETERS_PER_SUBCELL / upsample as i32;
    let origin_oct = [
        base_oct[0] - apron * OCTIMETERS_PER_SUBCELL + step_oct / 2,
        base_oct[1] - apron * OCTIMETERS_PER_SUBCELL + step_oct / 2,
    ];
    let rim_color = hsl_to_linear_rgb(s.base_hue, s.base_sat, s.base_light * (1.0 - s.rim_darken));
    let rim_place = GridPlacement {
        origin_oct,
        step_oct,
        y_lift: OVERLAY_RIM_LIFT,
    };
    march_grid(&smoothed, gw, gh, &rim_place, rim_color, tris);
    let rim_width = (s.rim_inset_octimeters / step_oct).max(1);
    let eroded = erode(&smoothed, gw, gh, rim_width);

    if material == Material::Water {
        emit_water(&eroded, gw, apron, upsample, base_oct, tris);
        return;
    }

    let body_color = hsl_to_linear_rgb(s.base_hue, s.base_sat, s.base_light);
    let body_place = GridPlacement {
        origin_oct,
        step_oct,
        y_lift: OVERLAY_BODY_LIFT,
    };
    march_grid(&eroded, gw, gh, &body_place, body_color, tris);
}

/// Emit water's body: a graded quad per subcell whose sample block sits
/// fully inside the eroded smoothed grid, lightness falling with derived
/// shore depth and hue varying per subcell. Partially-covered shoreline
/// subcells are left to the marched rim layer beneath — that band carries
/// the smoothed silhouette, so depth grading keeps its subcell resolution
/// without quadrupling the water budget.
fn emit_water(
    eroded: &[bool],
    gw: usize,
    apron: i32,
    upsample: usize,
    base_oct: [i32; 2],
    tris: &mut Vec<DrawTriangle>,
) {
    let u = upsample as i32;
    let inside = |six: i32, siz: i32| {
        let gx0 = (six + apron) * u;
        let gz0 = (siz + apron) * u;
        (0..u).all(|dz| {
            (0..u).all(|dx| {
                let gx = gx0 + dx;
                let gz = gz0 + dz;
                // The grid is square (n × n upsampled), so gw bounds both.
                gx >= 0
                    && gz >= 0
                    && (gx as usize) < gw
                    && (gz as usize) < gw
                    && eroded[gz as usize * gw + gx as usize]
            })
        })
    };
    for sj in 0..SUBCELLS_PER_CHUNK_EDGE {
        for si in 0..SUBCELLS_PER_CHUNK_EDGE {
            if !inside(si, sj) {
                continue;
            }
            let depth = subcell_shore_depth(&inside, si, sj);
            let x_oct = base_oct[0] + si * OCTIMETERS_PER_SUBCELL;
            let z_oct = base_oct[1] + sj * OCTIMETERS_PER_SUBCELL;
            let center_x = (x_oct + OCTIMETERS_PER_SUBCELL / 2) as f32 / OCTIMETERS_PER_METER;
            let center_z = (z_oct + OCTIMETERS_PER_SUBCELL / 2) as f32 / OCTIMETERS_PER_METER;
            let resolved = resolve_cell(Material::Water, center_x, center_z, Some(depth));
            let color = hsl_to_linear_rgb(resolved.hue, resolved.sat, resolved.light);
            push_quad(
                tris,
                x_oct,
                z_oct,
                x_oct + OCTIMETERS_PER_SUBCELL,
                z_oct + OCTIMETERS_PER_SUBCELL,
                OVERLAY_BODY_LIFT,
                color,
            );
        }
    }
}

/// Derived shore depth `[0, 1]` for a water subcell: a bounded four-way
/// outward scan (in subcells) for the nearest subcell not fully inside
/// the water body. `0` at the shore edge, rising monotonically inward to
/// `1` past the scan reach.
fn subcell_shore_depth(inside: &impl Fn(i32, i32) -> bool, x: i32, z: i32) -> f32 {
    let mut nearest = MAX_SHORE_SCAN;
    for (dx, dz) in [(-1, 0), (1, 0), (0, -1), (0, 1)] {
        for step in 1..=MAX_SHORE_SCAN {
            if !inside(x + dx * step, z + dz * step) {
                nearest = nearest.min(step);
                break;
            }
        }
    }
    ((nearest - 1) as f32 / (MAX_SHORE_SCAN - 1) as f32).clamp(0.0, 1.0)
}

/// Emit the raw calibration view: one flat grayscale quad per non-Void
/// underlay cell, its value the cell's own hue-noise field.
fn mesh_raw(world: &World, at: ChunkPos, tris: &mut Vec<DrawTriangle>) {
    let base_x = at.x * EDGE;
    let base_z = at.z * EDGE;
    for lz in 0..EDGE {
        for lx in 0..EDGE {
            let cell = CellPos {
                x: base_x + lx,
                z: base_z + lz,
            };
            let material = world.underlay(cell);
            if material == Material::Void {
                continue;
            }
            let v = raw_field(material, cell.x as f32 + 0.5, cell.z as f32 + 0.5);
            let x0 = cell.x * 256;
            let z0 = cell.z * 256;
            push_quad(tris, x0, z0, x0 + 256, z0 + 256, UNDERLAY_Y, [v, v, v]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::{CELLS_PER_CHUNK_AREA, Chunk, SmoothingProfile};
    use style::ResolvedCell;

    fn grass_cell() -> ResolvedCell {
        ResolvedCell {
            hue: 110.0,
            sat: 37.5,
            light: 40.0,
            stroke: (1.0, 0.0),
        }
    }

    fn overlay_tris(tris: &[DrawTriangle]) -> usize {
        tris.iter()
            .filter(|t| t.verts.iter().all(|v| v.y > 0.0))
            .count()
    }

    #[test]
    fn no_rims_collapse_to_one_flat_quad() {
        // A cell with no pooled edge on any side must emit a single quad
        // (two triangles), not the nine-slice — the common interior case
        // must not pay for rim geometry it does not have.
        let mut tris = Vec::new();
        emit_underlay_cell(Material::Grass, &grass_cell(), 3, 5, [0.0; 4], &mut tris);
        assert_eq!(tris.len(), 2, "no rims is one flat quad");
    }

    #[test]
    fn rimmed_cell_is_a_nine_slice_inside_its_bounds() {
        // A fully-rimmed cell emits the nine-slice (eighteen triangles), and
        // every vertex stays inside the cell — the rim only pools inward, it
        // never invents geometry outside the cell.
        let mut tris = Vec::new();
        emit_underlay_cell(Material::Grass, &grass_cell(), 3, 5, [1.0; 4], &mut tris);
        assert_eq!(tris.len(), 18, "a fully-rimmed cell is a nine-slice");
        for v in tris.iter().flat_map(|t| t.verts.iter()) {
            assert!(
                (3.0..=4.0).contains(&v.x) && (5.0..=6.0).contains(&v.z),
                "vertex ({}, {}) escaped the cell",
                v.x,
                v.z,
            );
        }
    }

    /// A world of one chunk whose underlay is filled per a closure.
    fn world_with_underlay(pos: ChunkPos, fill: impl Fn(i32, i32) -> Material) -> World {
        let mut chunk = Chunk::empty();
        for lz in 0..EDGE {
            for lx in 0..EDGE {
                chunk.underlay[(lz * EDGE + lx) as usize] = fill(lx, lz);
            }
        }
        let mut world = World::new();
        world.insert_chunk(pos, chunk);
        world
    }

    #[test]
    fn border_cells_agree_on_the_shared_rim() {
        // The two cells sharing a chunk-border edge must compute the same
        // rim for it. Cell (15, z) of chunk (0,0) and cell (16, z) of chunk
        // (1,0) are both grass with different resolved hues; the rim each
        // reads for the shared edge must match, or the seam would show a
        // one-sided rim.
        let mut world = world_with_underlay(ChunkPos { x: 0, z: 0 }, |_, _| Material::Grass);
        let mut right = Chunk::empty();
        right.underlay = [Material::Grass; CELLS_PER_CHUNK_AREA];
        world.insert_chunk(ChunkPos { x: 1, z: 0 }, right);

        let left_cell_right_rim = cell_rims(&world, CellPos { x: 15, z: 4 })[1];
        let right_cell_left_rim = cell_rims(&world, CellPos { x: 16, z: 4 })[0];
        assert_eq!(
            left_cell_right_rim, right_cell_left_rim,
            "both sides must agree on the shared-edge rim",
        );
    }

    #[test]
    fn water_depth_is_monotone_inward_and_absent_outside() {
        // Shore depth must be zero at the shoreline and rise monotonically
        // toward the interior; a subcell one step from open water is
        // shallower than one buried deep. Depth outside the water body is
        // never sampled (the caller skips subcells not fully inside).
        let n = 24i32;
        let mut field = vec![false; (n * n) as usize];
        for z in 0..n {
            for x in 0..n {
                if (x - 12) * (x - 12) + (z - 12) * (z - 12) <= 81 {
                    field[(z * n + x) as usize] = true;
                }
            }
        }
        let inside =
            |x: i32, z: i32| x >= 0 && z >= 0 && x < n && z < n && field[(z * n + x) as usize];
        let center = subcell_shore_depth(&inside, 12, 12);
        let shore = subcell_shore_depth(&inside, 21, 12);
        assert!(center > shore, "deeper inside: {center} > {shore}");
        assert_eq!(shore, 0.0, "a subcell touching open water is at depth 0");
        assert_eq!(center, 1.0, "the middle of a wide lake is fully deep");
    }

    #[test]
    fn full_water_chunk_budget_is_pinned() {
        // Tripwire: the water budget is body + rim, all derivable. Body: a
        // graded quad per subcell fully inside the eroded grid — with water
        // on every side, all 64*64 subcells qualify = 8192 tris. Rim: the
        // smoothed grid is all-true, so its march greedy-merges to one quad
        // (2 tris); water never marches the eroded grid (the body quads
        // replace that layer). Total 8194. A change to the water
        // resolution, rim width, or grading fan-out moves this and must be
        // deliberate.
        let mut world = World::new();
        for dz in -1..=1 {
            for dx in -1..=1 {
                let mut c = Chunk::empty();
                c.overlay = [Material::Water; CELLS_PER_CHUNK_AREA];
                c.overlay_mask = [0xFFFF; CELLS_PER_CHUNK_AREA];
                world.insert_chunk(ChunkPos { x: dx, z: dz }, c);
            }
        }
        let tris = mesh_chunk(&world, ChunkPos { x: 0, z: 0 }, ViewMode::Painted);
        assert_eq!(overlay_tris(&tris), 8194, "8192 body + one merged rim quad");
    }

    #[test]
    fn water_shoreline_is_smoothed_and_rimmed() {
        // The waterline flows through the same corner minimization and rim
        // march as land contours: a lone water square must emit strictly
        // more than its raw per-subcell quads (the marched rim band is
        // present) and its smoothed silhouette must cut the square's
        // corners (some rim geometry sits off the subcell lattice).
        let mut world = World::new();
        let mut c = Chunk::empty();
        // A 4x4-cell water square in the chunk interior.
        for lz in 6..10 {
            for lx in 6..10 {
                c.overlay[(lz * EDGE + lx) as usize] = Material::Water;
                c.overlay_mask[(lz * EDGE + lx) as usize] = 0xFFFF;
            }
        }
        world.insert_chunk(ChunkPos { x: 0, z: 0 }, c);
        let tris = mesh_chunk(&world, ChunkPos { x: 0, z: 0 }, ViewMode::Painted);

        let rim_tris = tris
            .iter()
            .filter(|t| t.verts.iter().all(|v| v.y > 0.0 && v.y < OVERLAY_BODY_LIFT))
            .count();
        assert!(rim_tris > 0, "water marches a shoreline rim layer");
        // The smoothed rim contour cuts corners between subcell lattice
        // points: some rim vertex sits on the half-subcell grid (odd
        // multiple of 1/8 m), which raw per-subcell quads never produce.
        let off_lattice = tris
            .iter()
            .filter(|t| t.verts.iter().all(|v| v.y > 0.0 && v.y < OVERLAY_BODY_LIFT))
            .flat_map(|t| t.verts.iter())
            .any(|v| {
                let scaled = v.x * 8.0;
                let eighths = scaled.round();
                (scaled - eighths).abs() < 1e-4 && (eighths as i64) % 2 != 0
            });
        assert!(off_lattice, "the shoreline silhouette is corner-minimized");
    }

    #[test]
    fn smoothing_field_overrides_agree_across_a_chunk_border() {
        // A field-painted crisp zone must beat the material default on both
        // sides of a chunk border identically. Stone (which smooths by
        // default) covers a band across the border; every cell in both
        // chunks points at a zero-iteration profile, so both chunks must
        // emit exactly the raw blocky contour — and agree with each other.
        // If either side dropped the override (or read it only inside its
        // own chunk, not through the apron), its smoothed rim contour would
        // put vertices on the half-subcell grid. Only the rim layer is
        // checked: the body layer's eroded boundary legitimately sits
        // mid-window (half-subcell positions) even when raw.
        let mut world = World::new();
        world.insert_smoothing_profile(
            1,
            SmoothingProfile {
                iterations: 0,
                degrees: 90,
            },
        );
        for cx in 0..2 {
            let mut chunk = Chunk::empty();
            chunk.smoothing = [1; CELLS_PER_CHUNK_AREA];
            for lz in 6..10 {
                for lx in 0..EDGE {
                    let global = cx * EDGE + lx;
                    if (12..20).contains(&global) {
                        chunk.overlay[(lz * EDGE + lx) as usize] = Material::Stone;
                        chunk.overlay_mask[(lz * EDGE + lx) as usize] = 0xFFFF;
                    }
                }
            }
            world.insert_chunk(ChunkPos { x: cx, z: 0 }, chunk);
        }
        for cx in 0..2 {
            let tris = mesh_chunk(&world, ChunkPos { x: cx, z: 0 }, ViewMode::Painted);
            for v in tris
                .iter()
                .filter(|t| t.verts.iter().all(|v| v.y > 0.0 && v.y < OVERLAY_BODY_LIFT))
                .flat_map(|t| t.verts.iter())
            {
                let eighths = v.x * 8.0;
                let is_half_subcell =
                    (eighths - eighths.round()).abs() < 1e-4 && (eighths.round() as i64) % 2 != 0;
                assert!(
                    !is_half_subcell,
                    "chunk {cx}: a zero-iteration field zone stays raw, vertex x = {}",
                    v.x,
                );
            }
        }
    }

    #[test]
    fn overlay_contour_is_continuous_across_a_chunk_border() {
        // A straight overlay edge that crosses a chunk boundary must land at
        // one x whether meshed from either chunk — the apron read makes the
        // two independently-meshed chunks agree on the border. Stone covers
        // every subcell with global subcell-x < 70 (chunk 0 fully, chunk 1's
        // first six subcells), a straight vertical edge past the border.
        let mut world = World::new();
        for cx in 0..2 {
            let mut chunk = Chunk::empty();
            for lz in 0..EDGE {
                for lx in 0..EDGE {
                    let mut mask = 0u16;
                    for sz in 0..SUB {
                        for sx in 0..SUB {
                            let global_sub_x = (cx * EDGE + lx) * SUB + sx;
                            if global_sub_x < 70 {
                                mask |= 1 << (sz * SUB + sx);
                            }
                        }
                    }
                    if mask != 0 {
                        chunk.overlay[(lz * EDGE + lx) as usize] = Material::Stone;
                        chunk.overlay_mask[(lz * EDGE + lx) as usize] = mask;
                    }
                }
            }
            world.insert_chunk(ChunkPos { x: cx, z: 0 }, chunk);
        }
        let mesh0 = mesh_chunk(&world, ChunkPos { x: 0, z: 0 }, ViewMode::Painted);
        let mesh1 = mesh_chunk(&world, ChunkPos { x: 1, z: 0 }, ViewMode::Painted);
        let max_x = |mesh: &[DrawTriangle]| {
            mesh.iter()
                .flat_map(|t| t.verts.iter())
                .filter(|v| v.y > 0.0)
                .map(|v| v.x)
                .fold(f32::MIN, f32::max)
        };
        // The straight edge sits at global subcell 70 = 70/4 = 17.5 m; the
        // chunk that carries it (chunk 1) marches its crossings there.
        let edge1 = max_x(&mesh1);
        assert!((edge1 - 17.5).abs() < 1e-4, "edge at 17.5 m, got {edge1}");
        // Chunk 0 is fully covered and its apron reads into chunk 1, so its
        // overlay reaches across its own border (x = 16 m) with no gap — the
        // two chunks overlap at the seam rather than leaving a hole.
        let edge0 = max_x(&mesh0);
        assert!(
            edge0 >= 16.0,
            "chunk 0 overlay reaches its border, got {edge0}"
        );
    }

    #[test]
    fn raw_view_is_grayscale_and_painted_is_not() {
        // Raw mode paints each cell its own noise field as gray (r == g == b)
        // for calibration; switching back to painted must repaint in color
        // (some vertex has r != g). A stuck mode would fail one side.
        let world = world_with_underlay(ChunkPos { x: 0, z: 0 }, |_, _| Material::Grass);
        let raw = mesh_chunk(&world, ChunkPos { x: 0, z: 0 }, ViewMode::Raw);
        assert!(!raw.is_empty(), "raw mode emits geometry");
        for v in raw.iter().flat_map(|t| t.verts.iter()) {
            assert!(v.r == v.g && v.g == v.b, "raw vertex is grayscale");
        }
        let painted = mesh_chunk(&world, ChunkPos { x: 0, z: 0 }, ViewMode::Painted);
        assert!(
            painted
                .iter()
                .flat_map(|t| t.verts.iter())
                .any(|v| v.r != v.g),
            "painted mode is in color",
        );
    }

    #[test]
    fn void_chunk_meshes_to_nothing() {
        let mut world = World::new();
        world.insert_chunk(ChunkPos { x: 0, z: 0 }, Chunk::empty());
        let tris = mesh_chunk(&world, ChunkPos { x: 0, z: 0 }, ViewMode::Painted);
        assert!(tris.is_empty(), "an all-Void chunk emits no geometry");
    }
}
