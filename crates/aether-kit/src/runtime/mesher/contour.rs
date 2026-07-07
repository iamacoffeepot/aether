// Grid indices and octimeter coordinates are small integers cast between
// i32 / usize / f32 for lattice math and vertex output; the ranges here
// (chunk-bounded subcell grids, octimeter positions) make the pedantic
// precision / sign / truncation lints non-issues.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap
)]

//! The resolution-parameterized contour library: corner minimization, a
//! mask erode, and marching-squares emission over an arbitrary binary
//! grid.
//!
//! [`minimize_corners`] upsamples a binary mask and rounds its blocky
//! corners with one analytic 45-degree chamfer followed by angle-gated
//! cellular passes; the boundary only ever moves within the cells it
//! already occupied, so the stored mask stays the truth. [`march_grid`]
//! turns a binary grid into crisp contour polygons plus greedy-merged
//! interior quads on a caller-supplied octimeter lattice, and
//! [`erode`] peels a one-or-more-cell band off a grid so a second march
//! can lay a body layer inside a rim. All three are pure functions of the
//! grid, so a detail-prop mesher can drive the same machinery at its own
//! resolution.

use alloc::vec;
use alloc::vec::Vec;

use aether_capabilities::render::{DrawTriangle, Vertex};

/// Octimeters per meter (`1 cell = 1 m = 256 octimeters`), for the
/// octimeter-to-meter conversion at vertex emit.
const OCTIMETERS_PER_METER: f32 = 256.0;

/// Where a marched grid lands in the world: the octimeter position of
/// sample `(0, 0)`'s center, the octimeter step between adjacent samples,
/// and the `y` the layer lifts to.
#[derive(Clone, Copy)]
pub struct GridPlacement {
    /// Octimeter position of sample `(0, 0)`'s center.
    pub origin_oct: [i32; 2],
    /// Octimeter distance between adjacent samples.
    pub step_oct: i32,
    /// The `y` every emitted vertex lifts to.
    pub y_lift: f32,
}

/// Corner-smoothing parameters for [`minimize_corners`].
#[derive(Clone, Copy)]
pub struct SmoothParams {
    /// Iteration count: `0` leaves the mask blocky, `1` applies the
    /// analytic chamfer, `2+` add that many cellular passes.
    pub iterations: u32,
    /// Corner angle in degrees the cellular passes flatten down to: `90`
    /// disables the windowed rule (only true right-angle corners round),
    /// smaller angles round the gentler junctions too.
    pub smoothing_degrees: u32,
}

/// Read `mask` at `(x, z)`, returning `fallback` outside its bounds.
fn in_mask(mask: &[bool], width: usize, height: usize, x: i32, z: i32, fallback: bool) -> bool {
    if x < 0 || z < 0 || x >= width as i32 || z >= height as i32 {
        fallback
    } else {
        mask[z as usize * width + x as usize]
    }
}

/// The corner-flip test: the two orthogonal neighbors and the diagonal
/// must all agree on the opposite value for the corner to chamfer. Reads
/// outside the mask fall back to `m0`, so a mask edge never chamfers.
fn corner_flip(
    mask: &[bool],
    width: usize,
    height: usize,
    m0: bool,
    n1: (i32, i32),
    n2: (i32, i32),
    diag: (i32, i32),
) -> Option<bool> {
    let a = in_mask(mask, width, height, n1.0, n1.1, m0);
    if a != m0
        && a == in_mask(mask, width, height, n2.0, n2.1, m0)
        && a == in_mask(mask, width, height, diag.0, diag.1, m0)
    {
        Some(a)
    } else {
        None
    }
}

/// Upsample `mask` (`width × height`) by `upsample` and minimize its
/// corners per `params`. Returns the smoothed grid and its dimensions
/// (`width * upsample`, `height * upsample`).
#[must_use]
pub fn minimize_corners(
    mask: &[bool],
    width: usize,
    height: usize,
    upsample: usize,
    params: SmoothParams,
) -> (Vec<bool>, usize, usize) {
    let upsample = upsample.max(1);
    let gw = width * upsample;
    let gh = height * upsample;
    let mut grid = vec![false; gw * gh];
    for gz in 0..gh {
        for gx in 0..gw {
            grid[gz * gw + gx] = in_mask(
                mask,
                width,
                height,
                (gx / upsample) as i32,
                (gz / upsample) as i32,
                false,
            );
        }
    }
    if params.iterations < 1 {
        return (grid, gw, gh);
    }

    // Pass one: rasterize the analytic cell-level 45-degree chamfer onto
    // the upsampled grid. A corner triangle flips only when its two
    // orthogonal neighbors and the diagonal all agree, so one rule covers
    // both a convex cut and a concave fill.
    let inv = 1.0 / upsample as f32;
    for gz in 0..gh {
        for gx in 0..gw {
            let cx = (gx / upsample) as i32;
            let cz = (gz / upsample) as i32;
            let fx = ((gx % upsample) as f32 + 0.5) * inv;
            let fz = ((gz % upsample) as f32 + 0.5) * inv;
            let m0 = in_mask(mask, width, height, cx, cz, false);
            let mut m = m0;
            if fx - fz > 0.5
                && let Some(a) = corner_flip(
                    mask,
                    width,
                    height,
                    m0,
                    (cx, cz - 1),
                    (cx + 1, cz),
                    (cx + 1, cz - 1),
                )
            {
                m = a;
            } else if fx + fz < 0.5
                && let Some(a) = corner_flip(
                    mask,
                    width,
                    height,
                    m0,
                    (cx, cz - 1),
                    (cx - 1, cz),
                    (cx - 1, cz - 1),
                )
            {
                m = a;
            } else if fx + fz > 1.5
                && let Some(a) = corner_flip(
                    mask,
                    width,
                    height,
                    m0,
                    (cx, cz + 1),
                    (cx + 1, cz),
                    (cx + 1, cz + 1),
                )
            {
                m = a;
            } else if fz - fx > 0.5
                && let Some(a) = corner_flip(
                    mask,
                    width,
                    height,
                    m0,
                    (cx, cz + 1),
                    (cx - 1, cz),
                    (cx - 1, cz + 1),
                )
            {
                m = a;
            }
            grid[gz * gw + gx] = m;
        }
    }

    // Passes two and up: cellular corner flips at subgrid scale, each pass
    // eating the corners the previous one left. A true right-angle corner
    // reads pointwise (five-plus of the eight neighbors differ); the
    // parity shoulder (four differ with two adjacent orthogonal sides)
    // catches a chamfer apex that straddles two subcells; a windowed 5x5
    // count grades the gentler junctions when the angle setting is under
    // 90 degrees.
    let t24 = if params.smoothing_degrees >= 90 {
        99
    } else {
        13 + ((params.smoothing_degrees as i32 - 45) * 2 + 15) / 30
    };
    for _pass in 1..params.iterations {
        grid = cellular_pass(&grid, gw, gh, t24);
    }
    (grid, gw, gh)
}

/// One cellular corner-flip pass over `grid`. A cell flips when a true
/// right-angle corner is detected pointwise (five-plus of the eight
/// neighbors differ), when the parity shoulder fires (four differ with two
/// adjacent orthogonal sides), or — when `t24` is enabled (under 90
/// degrees) — when the 5x5 window count reaches `t24`.
fn cellular_pass(grid: &[bool], gw: usize, gh: usize, t24: i32) -> Vec<bool> {
    let mut next = grid.to_vec();
    for gz in 0..gh as i32 {
        for gx in 0..gw as i32 {
            let m = grid[gz as usize * gw + gx as usize];
            let mut c8 = 0;
            for dz in -1..=1 {
                for dx in -1..=1 {
                    if dx == 0 && dz == 0 {
                        continue;
                    }
                    if in_mask(grid, gw, gh, gx + dx, gz + dz, m) != m {
                        c8 += 1;
                    }
                }
            }
            let dn = in_mask(grid, gw, gh, gx, gz - 1, m) != m;
            let ds = in_mask(grid, gw, gh, gx, gz + 1, m) != m;
            let dw = in_mask(grid, gw, gh, gx - 1, gz, m) != m;
            let de = in_mask(grid, gw, gh, gx + 1, gz, m) != m;
            let adjacent_orthogonal = (dw || de) && (ds || dn);
            let mut flip = c8 >= 5 || (c8 >= 4 && adjacent_orthogonal);
            if !flip && t24 < 99 && c8 >= 2 {
                let mut c24 = c8;
                for dz in -2i32..=2 {
                    for dx in -2i32..=2 {
                        if dx.abs().max(dz.abs()) < 2 {
                            continue;
                        }
                        if in_mask(grid, gw, gh, gx + dx, gz + dz, m) != m {
                            c24 += 1;
                        }
                    }
                }
                flip = c24 >= t24;
            }
            if flip {
                next[gz as usize * gw + gx as usize] = !m;
            }
        }
    }
    next
}

/// Peel a `radius`-cell band off `grid`: a cell survives only when every
/// cell within its Chebyshev radius is also inside. Erode a smoothed grid
/// then march the result to lay a body layer inset from a rim.
#[must_use]
pub fn erode(grid: &[bool], width: usize, height: usize, radius: i32) -> Vec<bool> {
    let mut out = vec![false; width * height];
    for z in 0..height as i32 {
        for x in 0..width as i32 {
            if !grid[z as usize * width + x as usize] {
                continue;
            }
            let mut keep = true;
            'scan: for dz in -radius..=radius {
                for dx in -radius..=radius {
                    if !in_mask(grid, width, height, x + dx, z + dz, false) {
                        keep = false;
                        break 'scan;
                    }
                }
            }
            out[z as usize * width + x as usize] = keep;
        }
    }
    out
}

/// Marching-squares inside-polygon table, indexed by the 4-bit window case
/// `BL | BR<<1 | TR<<2 | TL<<3` (`1` = the corner sample is inside). Each
/// entry lists the covered region in boundary order over the eight window
/// points: corners `0 = BL, 1 = BR, 2 = TR, 3 = TL` and edge midpoints
/// `4 = bottom, 5 = right, 6 = top, 7 = left`. Case 0 is empty and case 15
/// is emitted by the interior merge. The two saddles (5, 10) resolve by
/// one fixed rule: the inside diagonal is connected through the center.
const CASE_POLYS: [&[u8]; 16] = [
    &[],
    &[0, 4, 7],
    &[1, 5, 4],
    &[0, 1, 5, 7],
    &[2, 6, 5],
    &[0, 4, 5, 2, 6, 7],
    &[1, 2, 6, 4],
    &[0, 1, 2, 6, 7],
    &[3, 7, 6],
    &[0, 4, 6, 3],
    &[1, 5, 6, 3, 7, 4],
    &[0, 1, 5, 6, 3],
    &[3, 2, 5, 7],
    &[0, 4, 5, 2, 3],
    &[1, 2, 3, 7, 4],
    &[0, 1, 2, 3],
];

/// March a binary grid into contour polygons and greedy-merged interior
/// quads on `place`'s octimeter lattice.
pub fn march_grid(
    grid: &[bool],
    gw: usize,
    gh: usize,
    place: &GridPlacement,
    color: [f32; 3],
    tris: &mut Vec<DrawTriangle>,
) {
    if gw < 2 || gh < 2 {
        return;
    }
    let windows_x = gw - 1;
    let windows_z = gh - 1;
    let mut case_grid = vec![0u8; windows_x * windows_z];
    let mut full = vec![false; windows_x * windows_z];
    for wj in 0..windows_z {
        for wi in 0..windows_x {
            let bl = grid[wj * gw + wi];
            let br = grid[wj * gw + wi + 1];
            let tl = grid[(wj + 1) * gw + wi];
            let tr = grid[(wj + 1) * gw + wi + 1];
            let case = u8::from(bl) | u8::from(br) << 1 | u8::from(tr) << 2 | u8::from(tl) << 3;
            case_grid[wj * windows_x + wi] = case;
            full[wj * windows_x + wi] = case == 15;
        }
    }
    merge_interior(&full, windows_x, windows_z, place, color, tris);
    for wj in 0..windows_z {
        for wi in 0..windows_x {
            let case = case_grid[wj * windows_x + wi];
            if case == 0 || case == 15 {
                continue;
            }
            emit_window_contour(wi as i32, wj as i32, place, case, color, tris);
        }
    }
}

/// Greedy-merge fully-covered windows into maximal rectangles — the
/// interior is one flat color, so merging is safe — and emit one quad
/// each.
fn merge_interior(
    full: &[bool],
    windows_x: usize,
    windows_z: usize,
    place: &GridPlacement,
    color: [f32; 3],
    tris: &mut Vec<DrawTriangle>,
) {
    let mut consumed = vec![false; full.len()];
    for wj in 0..windows_z {
        for wi in 0..windows_x {
            let idx = wj * windows_x + wi;
            if consumed[idx] || !full[idx] {
                continue;
            }
            let mut w = 1;
            while wi + w < windows_x
                && full[wj * windows_x + wi + w]
                && !consumed[wj * windows_x + wi + w]
            {
                w += 1;
            }
            let mut h = 1;
            'rows: while wj + h < windows_z {
                for dx in 0..w {
                    let cell = (wj + h) * windows_x + wi + dx;
                    if !full[cell] || consumed[cell] {
                        break 'rows;
                    }
                }
                h += 1;
            }
            for dz in 0..h {
                for dx in 0..w {
                    consumed[(wj + dz) * windows_x + wi + dx] = true;
                }
            }
            let x0 = place.origin_oct[0] + wi as i32 * place.step_oct;
            let x1 = place.origin_oct[0] + (wi + w) as i32 * place.step_oct;
            let z0 = place.origin_oct[1] + wj as i32 * place.step_oct;
            let z1 = place.origin_oct[1] + (wj + h) as i32 * place.step_oct;
            push_quad(tris, x0, z0, x1, z1, place.y_lift, color);
        }
    }
}

/// Fan-triangulate one boundary window's inside polygon.
fn emit_window_contour(
    wi: i32,
    wj: i32,
    place: &GridPlacement,
    case: u8,
    color: [f32; 3],
    tris: &mut Vec<DrawTriangle>,
) {
    let step_oct = place.step_oct;
    let half = step_oct / 2;
    let x_lo = place.origin_oct[0] + wi * step_oct;
    let x_hi = place.origin_oct[0] + (wi + 1) * step_oct;
    let z_lo = place.origin_oct[1] + wj * step_oct;
    let z_hi = place.origin_oct[1] + (wj + 1) * step_oct;
    let x_mid = x_lo + half;
    let z_mid = z_lo + half;
    let points = [
        [x_lo, z_lo],
        [x_hi, z_lo],
        [x_hi, z_hi],
        [x_lo, z_hi],
        [x_mid, z_lo],
        [x_hi, z_mid],
        [x_mid, z_hi],
        [x_lo, z_mid],
    ];
    let poly = CASE_POLYS[case as usize];
    let vert = |p: [i32; 2]| Vertex {
        x: p[0] as f32 / OCTIMETERS_PER_METER,
        y: place.y_lift,
        z: p[1] as f32 / OCTIMETERS_PER_METER,
        r: color[0],
        g: color[1],
        b: color[2],
    };
    for k in 1..poly.len() - 1 {
        tris.push(DrawTriangle {
            verts: [
                vert(points[poly[0] as usize]),
                vert(points[poly[k] as usize]),
                vert(points[poly[k + 1] as usize]),
            ],
        });
    }
}

/// Push the two triangles of a flat quad spanning `[x0, x1] × [z0, z1]`
/// (octimeters) at `y_lift`, all corners one color.
pub fn push_quad(
    tris: &mut Vec<DrawTriangle>,
    x0: i32,
    z0: i32,
    x1: i32,
    z1: i32,
    y_lift: f32,
    color: [f32; 3],
) {
    let vert = |x: i32, z: i32| Vertex {
        x: x as f32 / OCTIMETERS_PER_METER,
        y: y_lift,
        z: z as f32 / OCTIMETERS_PER_METER,
        r: color[0],
        g: color[1],
        b: color[2],
    };
    let a = vert(x0, z0);
    let b = vert(x1, z0);
    let c = vert(x1, z1);
    let d = vert(x0, z1);
    tris.push(DrawTriangle { verts: [a, b, c] });
    tris.push(DrawTriangle { verts: [a, c, d] });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn count_true(grid: &[bool]) -> usize {
        grid.iter().filter(|&&b| b).count()
    }

    /// A raw upsample of `mask` by `factor` — the identity `minimize_corners`
    /// must reproduce at zero iterations.
    fn raw_upsample(mask: &[bool], w: usize, h: usize, factor: usize) -> Vec<bool> {
        let mut out = vec![false; w * factor * h * factor];
        let gw = w * factor;
        for gz in 0..h * factor {
            for gx in 0..gw {
                out[gz * gw + gx] = mask[(gz / factor) * w + gx / factor];
            }
        }
        out
    }

    #[test]
    fn saddle_resolves_to_the_connected_rule() {
        // Case 5 (BL + TR inside) must resolve by the fixed rule that
        // connects the inside diagonal through the center — a 6-vertex
        // hexagon fanned to 4 triangles, one joining BL and TR. A
        // disconnected rule would emit two separate corner triangles.
        let grid = [true, false, false, true]; // BL, BR, TL, TR
        let place = GridPlacement {
            origin_oct: [0, 0],
            step_oct: 64,
            y_lift: 0.0,
        };
        let mut tris = Vec::new();
        march_grid(&grid, 2, 2, &place, [0.0; 3], &mut tris);
        assert_eq!(tris.len(), 4, "the connected hexagon fans to 4 triangles");
        // BL corner = (0,0) m; TR corner = (64,64) oct = (0.25,0.25) m.
        let joins = tris.iter().any(|t| {
            let has = |x: f32, z: f32| {
                t.verts
                    .iter()
                    .any(|v| (v.x - x).abs() < 1e-6 && (v.z - z).abs() < 1e-6)
            };
            has(0.0, 0.0) && has(0.25, 0.25)
        });
        assert!(joins, "the connected rule joins the inside diagonal");
    }

    #[test]
    fn straight_edge_crossings_are_collinear() {
        // Columns 0..3 covered, 3..6 empty, over three rows: a straight
        // vertical overlay edge. Every contour crossing on that edge must
        // share one x; a kink would put a crossing short of it.
        let (gw, gh) = (6usize, 3usize);
        let mut grid = vec![false; gw * gh];
        for z in 0..gh {
            for x in 0..3 {
                grid[z * gw + x] = true;
            }
        }
        let place = GridPlacement {
            origin_oct: [0, 0],
            step_oct: 64,
            y_lift: 0.0,
        };
        let mut tris = Vec::new();
        march_grid(&grid, gw, gh, &place, [0.0; 3], &mut tris);
        let xs: Vec<f32> = tris
            .iter()
            .flat_map(|t| t.verts.iter())
            .map(|v| v.x)
            .collect();
        let max_x = xs.iter().copied().fold(f32::MIN, f32::max);
        // Window 2 (samples 2,3) is the boundary; its right/top mids sit at
        // 2*step + step/2 = 160 oct = 0.625 m — the rightmost geometry.
        assert!(
            (max_x - 0.625).abs() < 1e-6,
            "boundary at 0.625 m, got {max_x}"
        );
        let on_edge = xs.iter().filter(|&&x| x > 0.5).count();
        assert!(on_edge >= 2, "the edge is one straight, repeated line");
        assert!(
            xs.iter()
                .filter(|&&x| x > 0.5)
                .all(|&x| (x - 0.625).abs() < 1e-6),
            "no crossing kinks short of the straight edge",
        );
    }

    #[test]
    fn geometry_sits_at_the_placement_origin() {
        // The same window marched at a shifted origin must move by exactly
        // the shift — a chunk away from the world origin places its
        // geometry over its own cells, not stacked at zero.
        let grid = [true, false, false, false]; // BL only
        let mut base = Vec::new();
        march_grid(
            &grid,
            2,
            2,
            &GridPlacement {
                origin_oct: [0, 0],
                step_oct: 64,
                y_lift: 0.0,
            },
            [0.0; 3],
            &mut base,
        );
        let mut shifted = Vec::new();
        march_grid(
            &grid,
            2,
            2,
            &GridPlacement {
                origin_oct: [1024, 512],
                step_oct: 64,
                y_lift: 0.0,
            },
            [0.0; 3],
            &mut shifted,
        );
        assert_eq!(base.len(), shifted.len());
        for (b, s) in base.iter().zip(&shifted) {
            for (vb, vs) in b.verts.iter().zip(&s.verts) {
                assert!(
                    (vs.x - vb.x - 4.0).abs() < 1e-6,
                    "x shift is 1024 oct = 4 m"
                );
                assert!((vs.z - vb.z - 2.0).abs() < 1e-6, "z shift is 512 oct = 2 m");
            }
        }
    }

    #[test]
    fn zero_iterations_reproduces_the_raw_upsample() {
        // Iteration 0 must be the plain blocky mask upsampled — the toggle
        // that turns smoothing off entirely.
        let (w, h) = (4usize, 4usize);
        let mut mask = vec![false; w * h];
        mask[w + 1] = true;
        mask[w + 2] = true;
        mask[2 * w + 1] = true;
        let params = SmoothParams {
            iterations: 0,
            smoothing_degrees: 90,
        };
        let (grid, gw, gh) = minimize_corners(&mask, w, h, 3, params);
        assert_eq!((gw, gh), (12, 12));
        assert_eq!(grid, raw_upsample(&mask, w, h, 3));
    }

    #[test]
    fn single_tile_chamfers_and_keeps_flattening() {
        // One covered cell, upsampled, must lose corner area to the chamfer
        // (iteration 1) and lose more on each cellular pass — the parity
        // shoulder and windowed rule eating what the chamfer left.
        let (w, h) = (5usize, 5usize);
        let mut mask = vec![false; w * h];
        mask[2 * w + 2] = true;
        let raw = count_true(
            &minimize_corners(
                &mask,
                w,
                h,
                4,
                SmoothParams {
                    iterations: 0,
                    smoothing_degrees: 45,
                },
            )
            .0,
        );
        let one = count_true(
            &minimize_corners(
                &mask,
                w,
                h,
                4,
                SmoothParams {
                    iterations: 1,
                    smoothing_degrees: 45,
                },
            )
            .0,
        );
        let two = count_true(
            &minimize_corners(
                &mask,
                w,
                h,
                4,
                SmoothParams {
                    iterations: 3,
                    smoothing_degrees: 45,
                },
            )
            .0,
        );
        assert_eq!(raw, 16, "a lone cell upsamples to 4x4");
        assert!(one < raw, "the chamfer cuts the corners: {one} < {raw}");
        assert!(two < one, "cellular passes keep flattening: {two} < {one}");
    }

    #[test]
    fn straight_runs_never_move() {
        // A straight half-plane edge has no corners, so no iteration count
        // or angle may move it — the boundary only ever shifts within cells
        // already on it.
        let (w, h) = (8usize, 8usize);
        let mut mask = vec![false; w * h];
        for z in 0..h {
            for x in 0..4 {
                mask[z * w + x] = true;
            }
        }
        let params = SmoothParams {
            iterations: 3,
            smoothing_degrees: 90,
        };
        let (grid, _, _) = minimize_corners(&mask, w, h, 4, params);
        assert_eq!(
            grid,
            raw_upsample(&mask, w, h, 4),
            "a straight edge is untouched"
        );
    }

    #[test]
    fn ninety_degrees_leaves_a_diagonal_alone() {
        // At 90 degrees the windowed rule is disabled, so a clean 45-degree
        // diagonal — a gentle 45-75 corner, not a right angle — survives the
        // cellular passes untouched: the chamfered edge after one iteration
        // is identical after five. A windowed rule wrongly active at 90 would
        // keep eroding it.
        let (w, h) = (14usize, 14usize);
        let mut mask = vec![false; w * h];
        for z in 0..h {
            for x in 0..w {
                if x + z < 12 {
                    mask[z * w + x] = true;
                }
            }
        }
        let one = minimize_corners(
            &mask,
            w,
            h,
            2,
            SmoothParams {
                iterations: 1,
                smoothing_degrees: 90,
            },
        )
        .0;
        let five = minimize_corners(
            &mask,
            w,
            h,
            2,
            SmoothParams {
                iterations: 5,
                smoothing_degrees: 90,
            },
        )
        .0;
        assert_eq!(
            one, five,
            "the cellular passes leave a 45-degree diagonal alone at 90 degrees"
        );
    }

    #[test]
    fn lower_angle_flattens_more_than_90() {
        // The angle gate must be wired: an isolated square rounds its
        // corners under the cellular passes, and the windowed rule a
        // 45-degree setting enables shaves the rounded shoulders past what
        // the pointwise and parity rules do at 90. A gate stuck on or off
        // would erase the difference.
        let (w, h) = (14usize, 14usize);
        let mut mask = vec![false; w * h];
        for z in 4..8 {
            for x in 4..8 {
                mask[z * w + x] = true;
            }
        }
        let at_90 = count_true(
            &minimize_corners(
                &mask,
                w,
                h,
                2,
                SmoothParams {
                    iterations: 8,
                    smoothing_degrees: 90,
                },
            )
            .0,
        );
        let at_45 = count_true(
            &minimize_corners(
                &mask,
                w,
                h,
                2,
                SmoothParams {
                    iterations: 8,
                    smoothing_degrees: 45,
                },
            )
            .0,
        );
        assert!(
            at_45 < at_90,
            "the 45-degree gate flattens more: {at_45} < {at_90}"
        );
    }

    #[test]
    fn erode_peels_a_band() {
        // Eroding a solid block by radius 1 removes its border ring, leaving
        // an inset body for a second march to lay inside a rim.
        let (w, h) = (5usize, 5usize);
        let grid = vec![true; w * h];
        let out = erode(&grid, w, h, 1);
        // Only the 3x3 interior survives.
        assert_eq!(count_true(&out), 9);
        assert!(out[2 * w + 2], "the center survives");
        assert!(!out[0], "the corner is peeled");
    }
}
