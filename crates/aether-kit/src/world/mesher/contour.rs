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
//! grayscale coverage erode, and marching-squares emission over an
//! arbitrary scalar coverage grid.
//!
//! [`minimize_corners`] upsamples a coverage plane and rounds only its
//! binary 0/255 zones with one analytic 45-degree chamfer followed by
//! angle-gated cellular passes; scalar zones are reconstructed with
//! bilinear coverage and frozen so soft authored edges remain the truth.
//! [`march_grid`] thresholds at half coverage and interpolates edge
//! crossings, so binary data keeps exact midpoint crossings while scalar
//! data can place crossings between samples. [`erode`] applies a grayscale
//! min-filter so a second march can lay a body layer inside a rim. All
//! three are pure functions of the grid, so a detail-prop mesher can drive
//! the same machinery at its own resolution.

use alloc::vec;
use alloc::vec::Vec;

use crate::world::SCALAR_COVERAGE_THRESHOLD;
use aether_render::{DrawTriangle, Vertex};
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
}

struct MarchWindow<'a> {
    grid: &'a [u8],
    gw: usize,
    place: &'a GridPlacement,
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

pub(crate) const COVERAGE_CROSSING: f32 = SCALAR_COVERAGE_THRESHOLD as f32 - 0.5;

fn covered(sample: u8) -> bool {
    scalar_coverage_is_inside(f32::from(sample))
}

const ORTHOGONAL_OFFSETS: [(i32, i32); 4] = [(-1, 0), (1, 0), (0, -1), (0, 1)];

fn orthogonal_match_count(grid: &[u8], width: usize, height: usize, x: i32, z: i32, expected: bool) -> usize {
    ORTHOGONAL_OFFSETS.iter().filter(|(dx, dz)| in_mask(grid, width, height, x + dx, z + dz, expected)).count()
}

fn binary_coverage(sample: u8) -> bool {
    sample == 0 || sample == u8::MAX
}

/// Read `coverage` at `(x, z)`, returning `fallback` outside its bounds.
fn sample_at(coverage: &[u8], width: usize, height: usize, x: i32, z: i32, fallback: u8) -> u8 {
    if x < 0 || z < 0 || x >= width as i32 || z >= height as i32 {
        fallback
    } else {
        coverage[z as usize * width + x as usize]
    }
}

fn in_mask(coverage: &[u8], width: usize, height: usize, x: i32, z: i32, fallback: bool) -> bool {
    if x < 0 || z < 0 || x >= width as i32 || z >= height as i32 {
        fallback
    } else {
        covered(coverage[z as usize * width + x as usize])
    }
}

fn scalar_zone(coverage: &[u8], width: usize, height: usize, x: i32, z: i32) -> bool {
    [(0, 0), (1, 0), (0, 1), (1, 1)]
        .iter()
        .any(|(dx, dz)| !binary_coverage(sample_at(coverage, width, height, x + dx, z + dz, 0)))
}

pub(crate) fn interpolated_coverage(
    bottom_left: u8,
    bottom_right: u8,
    top_left: u8,
    top_right: u8,
    fraction_x: f32,
    fraction_z: f32,
) -> f32 {
    let bl = f32::from(bottom_left);
    let br = f32::from(bottom_right);
    let tl = f32::from(top_left);
    let tr = f32::from(top_right);
    let fx = fraction_x.clamp(0.0, 1.0);
    let fz = fraction_z.clamp(0.0, 1.0);
    let bottom = bl + (br - bl) * fx;
    let top = tl + (tr - tl) * fx;
    bottom + (top - bottom) * fz
}

pub(crate) fn reconstructed_coverage(
    bottom_left: u8,
    bottom_right: u8,
    top_left: u8,
    top_right: u8,
    fraction_x: f32,
    fraction_z: f32,
) -> u8 {
    if [bottom_left, bottom_right, top_left, top_right].into_iter().all(binary_coverage) {
        bottom_left
    } else {
        interpolated_coverage(bottom_left, bottom_right, top_left, top_right, fraction_x, fraction_z)
            .round()
            .clamp(0.0, 255.0) as u8
    }
}

pub(crate) fn scalar_coverage_is_inside(coverage: f32) -> bool {
    coverage >= COVERAGE_CROSSING
}

/// The corner-flip test: the two orthogonal neighbors and the diagonal
/// must all agree on the opposite value for the corner to chamfer. Reads
/// outside the mask fall back to `m0`, so a mask edge never chamfers.
fn corner_flip(
    coverage: &[u8],
    width: usize,
    height: usize,
    m0: bool,
    n1: (i32, i32),
    n2: (i32, i32),
    diag: (i32, i32),
) -> Option<bool> {
    let a = in_mask(coverage, width, height, n1.0, n1.1, m0);
    if a != m0
        && a == in_mask(coverage, width, height, n2.0, n2.1, m0)
        && a == in_mask(coverage, width, height, diag.0, diag.1, m0)
    {
        Some(a)
    } else {
        None
    }
}

/// The per-sample threshold the windowed 5x5 rule fires at, from the
/// sample's angle setting; `99` (unreachable — the window holds 24
/// neighbors) disables the rule at 90 degrees.
fn t24_of(smoothing_degrees: u32) -> i32 {
    if smoothing_degrees >= 90 {
        99
    } else {
        13 + ((smoothing_degrees as i32 - 45) * 2 + 15) / 30
    }
}

/// Upsample `coverage` (`width × height`) by `upsample` and minimize its
/// binary-zone corners, each source sample governed by its own entry in
/// `params` (a slice parallel to `coverage` — index `z * width + x`). A
/// source sample whose 2x2 neighborhood is all `0` / `255` uses nearest
/// upsampling and can flip under the legacy corner rules, writing `0` or
/// `255`. Any neighborhood containing a scalar value uses bilinear
/// reconstruction and is frozen out of smoothing, preserving soft authored
/// edges. The pass loop runs to the slice's maximum iteration count; a
/// sample flips in pass `k` only where its own `iterations` exceeds `k`,
/// and the angle gate reads its own `smoothing_degrees` — so a spatially
/// varying field degrades into an authored crisp↔smooth seam with no
/// reconciliation pass.
/// Returns the smoothed grid and its dimensions (`width * upsample`,
/// `height * upsample`).
#[must_use]
pub fn minimize_corners(
    coverage: &[u8],
    width: usize,
    height: usize,
    upsample: usize,
    params: &[SmoothParams],
) -> (Vec<u8>, usize, usize) {
    debug_assert_eq!(params.len(), coverage.len(), "one SmoothParams per sample");
    let upsample = upsample.max(1);
    let gw = width * upsample;
    let gh = height * upsample;
    let mut grid = vec![0; gw * gh];
    let inv = 1.0 / upsample as f32;
    for gz in 0..gh {
        for gx in 0..gw {
            let cx = (gx / upsample) as i32;
            let cz = (gz / upsample) as i32;
            let fx = ((gx % upsample) as f32 + 0.5) * inv;
            let fz = ((gz % upsample) as f32 + 0.5) * inv;
            grid[gz * gw + gx] = reconstructed_coverage(
                sample_at(coverage, width, height, cx, cz, 0),
                sample_at(coverage, width, height, cx + 1, cz, 0),
                sample_at(coverage, width, height, cx, cz + 1, 0),
                sample_at(coverage, width, height, cx + 1, cz + 1, 0),
                fx,
                fz,
            );
        }
    }
    let max_iterations = params.iter().map(|p| p.iterations).max().unwrap_or(0);
    if max_iterations < 1 {
        return (grid, gw, gh);
    }

    // Pass one: rasterize the analytic cell-level 45-degree chamfer onto
    // the upsampled grid. A corner triangle flips only when its two
    // orthogonal neighbors and the diagonal all agree, so one rule covers
    // both a convex cut and a concave fill. A sample whose own cell sits
    // at zero iterations keeps its raw value.
    for gz in 0..gh {
        for gx in 0..gw {
            let cx = (gx / upsample) as i32;
            let cz = (gz / upsample) as i32;
            if params[cz as usize * width + cx as usize].iterations < 1 {
                continue;
            }
            if scalar_zone(coverage, width, height, cx, cz) {
                continue;
            }
            let fx = ((gx % upsample) as f32 + 0.5) * inv;
            let fz = ((gz % upsample) as f32 + 0.5) * inv;
            let m0 = in_mask(coverage, width, height, cx, cz, false);
            let mut m = m0;
            if fx - fz > 0.5
                && let Some(a) = corner_flip(coverage, width, height, m0, (cx, cz - 1), (cx + 1, cz), (cx + 1, cz - 1))
            {
                m = a;
            } else if fx + fz < 0.5
                && let Some(a) = corner_flip(coverage, width, height, m0, (cx, cz - 1), (cx - 1, cz), (cx - 1, cz - 1))
            {
                m = a;
            } else if fx + fz > 1.5
                && let Some(a) = corner_flip(coverage, width, height, m0, (cx, cz + 1), (cx + 1, cz), (cx + 1, cz + 1))
            {
                m = a;
            } else if fz - fx > 0.5
                && let Some(a) = corner_flip(coverage, width, height, m0, (cx, cz + 1), (cx - 1, cz), (cx - 1, cz + 1))
            {
                m = a;
            }
            grid[gz * gw + gx] = if m {
                u8::MAX
            } else {
                0
            };
        }
    }

    // Passes two and up: cellular corner flips at subgrid scale, each pass
    // eating the corners the previous one left. A true right-angle corner
    // reads pointwise (five-plus of the eight neighbors differ); the
    // parity shoulder (four differ with two adjacent orthogonal sides)
    // catches a chamfer apex that straddles two subcells; a windowed 5x5
    // count grades the gentler junctions when the angle setting is under
    // 90 degrees.
    for pass in 1..max_iterations {
        grid = cellular_pass(&grid, gw, coverage, params, width, upsample, pass);
    }

    grid = prune_one_wide_artifacts(grid, gw, gh, coverage, params, width, upsample);
    (grid, gw, gh)
}

/// Converge away one-sample-wide artifacts the final cellular pass had no
/// successor to eat — a fill made while its neighbors were cut reads as a
/// bump jutting off the boundary once marched. A covered sample attached
/// by at most one orthogonal side cuts; an uncovered sample enclosed on
/// three or more orthogonal sides fills; every staircase or corner sample
/// carries exactly two sides, so legitimate contours are untouchable and
/// the sweep is a no-op on them. Gated per sample like the passes — a
/// zero-iteration zone keeps its raw mask verbatim.
fn prune_one_wide_artifacts(
    mut grid: Vec<u8>,
    gw: usize,
    gh: usize,
    source: &[u8],
    params: &[SmoothParams],
    mask_width: usize,
    upsample: usize,
) -> Vec<u8> {
    for _sweep in 0..8 {
        let mut changed = false;
        let mut next = grid.clone();
        for gz in 0..gh as i32 {
            for gx in 0..gw as i32 {
                let own = params[(gz as usize / upsample) * mask_width + gx as usize / upsample];
                if own.iterations < 1 {
                    continue;
                }
                let source_x = gx / upsample as i32;
                let source_z = gz / upsample as i32;
                if scalar_zone(source, mask_width, params.len() / mask_width, source_x, source_z) {
                    continue;
                }
                let sample = grid[gz as usize * gw + gx as usize];
                let m = covered(sample);
                let orth_covered = orthogonal_match_count(&grid, gw, gh, gx, gz, m);
                if (m && orth_covered <= 1) || (!m && orth_covered >= 3) {
                    next[gz as usize * gw + gx as usize] = if m {
                        0
                    } else {
                        u8::MAX
                    };
                    changed = true;
                }
            }
        }
        grid = next;
        if !changed {
            break;
        }
    }
    grid
}

/// One cellular corner-flip pass over `grid`. A sample flips when a true
/// right-angle corner is detected pointwise (five-plus of the eight
/// neighbors differ), when the parity shoulder fires (four differ with two
/// adjacent orthogonal sides), or — when its own angle setting is under 90
/// degrees — when the 5x5 window count reaches its threshold. Neighborhood
/// counts read the whole shared grid; only the flip decision is gated by
/// the sample's own cell (`params` at mask resolution, still in pass
/// `pass` when its `iterations` exceeds it).
fn cellular_pass(
    grid: &[u8],
    gw: usize,
    source: &[u8],
    params: &[SmoothParams],
    mask_width: usize,
    upsample: usize,
    pass: u32,
) -> Vec<u8> {
    let gh = grid.len() / gw;
    let mut next = grid.to_vec();
    for gz in 0..gh as i32 {
        for gx in 0..gw as i32 {
            let own = params[(gz as usize / upsample) * mask_width + gx as usize / upsample];
            if own.iterations <= pass {
                continue;
            }
            let source_x = gx / upsample as i32;
            let source_z = gz / upsample as i32;
            if scalar_zone(source, mask_width, params.len() / mask_width, source_x, source_z) {
                continue;
            }
            let t24 = t24_of(own.smoothing_degrees);
            let m = covered(grid[gz as usize * gw + gx as usize]);
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
                next[gz as usize * gw + gx as usize] = if m {
                    0
                } else {
                    u8::MAX
                };
            }
        }
    }
    next
}

/// Read `ids` at `(x, z)`, returning `fallback` outside its bounds.
fn id_at(ids: &[u8], width: usize, height: usize, x: i32, z: i32, fallback: u8) -> u8 {
    if x < 0 || z < 0 || x >= width as i32 || z >= height as i32 {
        fallback
    } else {
        ids[z as usize * width + x as usize]
    }
}

/// The dominant label among `neighbors` that differs from `own`: the most
/// frequent, ties resolved to the smallest id so both sides of a chunk
/// border pick identically. `None` when every neighbor matches `own`.
// `neighbors` is a 4-8 element ring, not a byte buffer — bytecount is noise.
#[allow(clippy::naive_bytecount)]
fn dominant_other(neighbors: &[u8], own: u8) -> Option<u8> {
    let mut best: Option<(u8, usize)> = None;
    for &candidate in neighbors {
        if candidate == own {
            continue;
        }
        let count = neighbors.iter().filter(|&&n| n == candidate).count();
        let better = match best {
            None => true,
            Some((b, c)) => count > c || (count == c && candidate < b),
        };
        if better {
            best = Some((candidate, count));
        }
    }
    best.map(|(id, _)| id)
}

/// Upsample a material-id grid (`width × height`) by `upsample` and
/// repartition it under the corner-minimization rules — the multi-label
/// generalization of [`minimize_corners`]. Every rule that cut or filled
/// a boolean mask becomes a reassignment: a sample flips to the agreeing
/// (chamfer) or dominant (cellular, prune) neighboring material, and only
/// when its own cell's entry in `params` allows — so a crisp zone neither
/// yields territory nor absorbs any, and every pass preserves the
/// partition by construction (samples change owner, never coverage).
/// `params` is per mask sample, resolved by the caller from the sample's
/// original material (or its cell's smoothing-field override) and held
/// fixed across passes. `frozen` (per mask sample, like `params`) pins a
/// sample to its seed label through every pass: the mesher freezes the
/// samples flanking a point-height break so the paint boundary can never
/// smooth across a physical cliff — a contour along a break stays on the
/// authored line (the accepted sample-resolution staircase), while
/// boundaries over continuous ground keep smoothing as ever. Neighborhood
/// counts still read frozen samples; only their own reassignment is
/// blocked, the same one-way gate a zero-iteration zone has.
#[must_use]
pub fn repartition(
    ids: &[u8],
    width: usize,
    height: usize,
    upsample: usize,
    params: &[SmoothParams],
    frozen: &[bool],
) -> (Vec<u8>, usize, usize) {
    debug_assert_eq!(params.len(), ids.len(), "one SmoothParams per sample");
    debug_assert_eq!(frozen.len(), ids.len(), "one frozen flag per sample");
    let upsample = upsample.max(1);
    let gw = width * upsample;
    let gh = height * upsample;
    let mut grid = vec![0u8; gw * gh];
    for gz in 0..gh {
        for gx in 0..gw {
            grid[gz * gw + gx] = id_at(ids, width, height, (gx / upsample) as i32, (gz / upsample) as i32, 0);
        }
    }
    let max_iterations = params.iter().map(|p| p.iterations).max().unwrap_or(0);
    if max_iterations < 1 {
        return (grid, gw, gh);
    }

    // Pass one: the analytic cell-level 45-degree chamfer. A corner
    // triangle reassigns to the neighbor material when the two orthogonal
    // neighbors and the diagonal all agree on it — one rule covers a cut
    // and a fill, exactly as in the boolean form.
    let inv = 1.0 / upsample as f32;
    let mut out = grid.clone();
    for gz in 0..gh {
        for gx in 0..gw {
            let cx = (gx / upsample) as i32;
            let cz = (gz / upsample) as i32;
            let sample = cz as usize * width + cx as usize;
            if params[sample].iterations < 1 || frozen[sample] {
                continue;
            }
            let fx = ((gx % upsample) as f32 + 0.5) * inv;
            let fz = ((gz % upsample) as f32 + 0.5) * inv;
            let m0 = id_at(ids, width, height, cx, cz, 0);
            let corner: Option<[(i32, i32); 3]> = if fx - fz > 0.5 {
                Some([(cx, cz - 1), (cx + 1, cz), (cx + 1, cz - 1)])
            } else if fx + fz < 0.5 {
                Some([(cx, cz - 1), (cx - 1, cz), (cx - 1, cz - 1)])
            } else if fx + fz > 1.5 {
                Some([(cx, cz + 1), (cx + 1, cz), (cx + 1, cz + 1)])
            } else if fz - fx > 0.5 {
                Some([(cx, cz + 1), (cx - 1, cz), (cx - 1, cz + 1)])
            } else {
                None
            };
            if let Some([n1, n2, diag]) = corner {
                let a = id_at(ids, width, height, n1.0, n1.1, m0);
                if a != m0
                    && a == id_at(ids, width, height, n2.0, n2.1, m0)
                    && a == id_at(ids, width, height, diag.0, diag.1, m0)
                {
                    out[gz * gw + gx] = a;
                }
            }
        }
    }
    grid = out;

    // Passes two and up: cellular reassignment at subgrid scale — the
    // same pointwise, parity-shoulder, and windowed rules, with the flip
    // target the dominant differing neighbor.
    for pass in 1..max_iterations {
        grid = cellular_repartition_pass(&grid, gw, gh, params, frozen, width, upsample, pass);
    }

    grid = prune_one_wide_labels(grid, gw, gh, params, frozen, width, upsample);
    (grid, gw, gh)
}

/// One cellular reassignment pass over the id grid — the multi-label
/// [`cellular_pass`]. Counts read "differs from mine"; a firing sample
/// reassigns to its dominant differing eight-neighbor.
#[allow(clippy::too_many_arguments)] // one call site; the pass state travels together
fn cellular_repartition_pass(
    grid: &[u8],
    gw: usize,
    gh: usize,
    params: &[SmoothParams],
    frozen: &[bool],
    mask_width: usize,
    upsample: usize,
    pass: u32,
) -> Vec<u8> {
    let mut next = grid.to_vec();
    for gz in 0..gh as i32 {
        for gx in 0..gw as i32 {
            let sample = (gz as usize / upsample) * mask_width + gx as usize / upsample;
            let own = params[sample];
            if own.iterations <= pass || frozen[sample] {
                continue;
            }
            let t24 = t24_of(own.smoothing_degrees);
            let m = grid[gz as usize * gw + gx as usize];
            let mut ring = [m; 8];
            let mut k = 0;
            let mut c8 = 0;
            for dz in -1..=1 {
                for dx in -1..=1 {
                    if dx == 0 && dz == 0 {
                        continue;
                    }
                    let n = id_at(grid, gw, gh, gx + dx, gz + dz, m);
                    ring[k] = n;
                    k += 1;
                    if n != m {
                        c8 += 1;
                    }
                }
            }
            let dn = id_at(grid, gw, gh, gx, gz - 1, m) != m;
            let ds = id_at(grid, gw, gh, gx, gz + 1, m) != m;
            let dw = id_at(grid, gw, gh, gx - 1, gz, m) != m;
            let de = id_at(grid, gw, gh, gx + 1, gz, m) != m;
            let adjacent_orthogonal = (dw || de) && (ds || dn);
            let mut flip = c8 >= 5 || (c8 >= 4 && adjacent_orthogonal);
            if !flip && t24 < 99 && c8 >= 2 {
                let mut c24 = c8;
                for dz in -2i32..=2 {
                    for dx in -2i32..=2 {
                        if dx.abs().max(dz.abs()) < 2 {
                            continue;
                        }
                        if id_at(grid, gw, gh, gx + dx, gz + dz, m) != m {
                            c24 += 1;
                        }
                    }
                }
                flip = c24 >= t24;
            }
            if flip && let Some(target) = dominant_other(&ring, m) {
                next[gz as usize * gw + gx as usize] = target;
            }
        }
    }
    next
}

/// Converge away one-sample-wide artifacts in the id grid — the
/// multi-label [`prune_one_wide_artifacts`], where the boolean cut and
/// fill collapse into one rule: a sample attached to its own material by
/// at most one orthogonal side reassigns to its dominant differing
/// orthogonal neighbor. Staircase and corner samples carry exactly two
/// same-material sides, so legitimate boundaries are untouchable.
// `orth` is a 4-element ring, not a byte buffer — bytecount is noise.
#[allow(clippy::naive_bytecount)]
fn prune_one_wide_labels(
    mut grid: Vec<u8>,
    gw: usize,
    gh: usize,
    params: &[SmoothParams],
    frozen: &[bool],
    mask_width: usize,
    upsample: usize,
) -> Vec<u8> {
    for _sweep in 0..8 {
        let mut changed = false;
        let mut next = grid.clone();
        for gz in 0..gh as i32 {
            for gx in 0..gw as i32 {
                let sample = (gz as usize / upsample) * mask_width + gx as usize / upsample;
                let own = params[sample];
                if own.iterations < 1 || frozen[sample] {
                    continue;
                }
                let m = grid[gz as usize * gw + gx as usize];
                let orth = [
                    id_at(&grid, gw, gh, gx - 1, gz, m),
                    id_at(&grid, gw, gh, gx + 1, gz, m),
                    id_at(&grid, gw, gh, gx, gz - 1, m),
                    id_at(&grid, gw, gh, gx, gz + 1, m),
                ];
                let same = orth.iter().filter(|&&n| n == m).count();
                if same <= 1
                    && let Some(target) = dominant_other(&orth, m)
                {
                    next[gz as usize * gw + gx as usize] = target;
                    changed = true;
                }
            }
        }
        grid = next;
        if !changed {
            break;
        }
    }
    grid
}

/// The 4-bit marching case for `label` in window `(wi, wj)` of the id
/// grid — `BL | BR<<1 | TR<<2 | TL<<3`, `1` = the corner sample holds
/// `label`.
#[must_use]
pub fn label_case(ids: &[u8], gw: usize, wi: usize, wj: usize, label: u8) -> u8 {
    let bl = ids[wj * gw + wi] == label;
    let br = ids[wj * gw + wi + 1] == label;
    let tl = ids[(wj + 1) * gw + wi] == label;
    let tr = ids[(wj + 1) * gw + wi + 1] == label;
    u8::from(bl) | u8::from(br) << 1 | u8::from(tr) << 2 | u8::from(tl) << 3
}

/// The two disconnected saddle triangles, indexed by saddle case: the
/// id-priority loser of a two-label saddle window yields the center, so
/// the winner's connected hexagon plus these tiles the window exactly.
const SADDLE_SPLIT_POLYS: [(&[u8], &[u8]); 2] = [
    (&[0, 4, 7], &[2, 6, 5]), // case 5: BL and TR corner triangles
    (&[1, 5, 4], &[3, 7, 6]), // case 10: BR and TL corner triangles
];

/// Fan-triangulate one label's polygons for window `(wi, wj)` — the
/// public window emitter for partition marching. `connected` selects the
/// saddle resolution for cases 5 and 10: the id-priority winner connects
/// its diagonal through the center, the loser splits into two corner
/// triangles, and every other case has one fixed polygon. Case 0 emits
/// nothing; case 15 emits the full window. The vertex closure receives the
/// window point index alongside the position (corners `0 = BL, 1 = BR,
/// 2 = TR, 3 = TL`, edge midpoints `4 = bottom, 5 = right, 6 = top,
/// 7 = left`), so a caller lifting over height relief can resolve which
/// side of a crossed edge a boundary vertex belongs to as data — a
/// smoothing-displaced crossing sits off the subcell lattice, where the
/// side cannot be inferred from the position.
pub fn emit_label_window(
    wi: i32,
    wj: i32,
    place: &GridPlacement,
    case: u8,
    connected: bool,
    vertex: &impl Fn(f32, f32, u8) -> Vertex,
    tris: &mut Vec<DrawTriangle>,
) {
    for poly in label_window_polys(case, connected) {
        emit_window_poly_indexed(wi, wj, place, poly, vertex, tris);
    }
}

/// The one or two boundary polygons `label`'s marching case covers in a
/// window — the tables behind [`emit_label_window`], exposed so a caller
/// that must clip a flap before fanning it (the mesher's height-break
/// split) can walk the raw point-index polygons. `connected` resolves the
/// two saddles exactly as [`emit_label_window`] does; case 0 covers
/// nothing, every other case one polygon (a split saddle two). Point
/// indices are the window-point numbering (corners `0..4`, edge midpoints
/// `4..8`); every polygon is convex.
#[must_use]
pub fn label_window_polys(case: u8, connected: bool) -> [&'static [u8]; 2] {
    if case == 0 {
        return [&[], &[]];
    }
    if (case == 5 || case == 10) && !connected {
        return SADDLE_SPLIT_POLYS[usize::from(case == 10)].into();
    }
    [CASE_POLYS[case as usize], &[]]
}

/// Peel a `radius`-cell band off `grid` with a grayscale min-filter:
/// each sample becomes the minimum coverage in its Chebyshev-radius
/// neighborhood. Binary grids therefore keep the old all-neighbors-inside
/// erosion, while scalar soft edges inset continuously.
#[must_use]
pub fn erode(grid: &[u8], width: usize, height: usize, radius: i32) -> Vec<u8> {
    if radius <= 0 {
        return grid.to_vec();
    }
    let mut out = vec![0; width * height];
    for z in 0..height as i32 {
        for x in 0..width as i32 {
            let mut min = u8::MAX;
            for dz in -radius..=radius {
                for dx in -radius..=radius {
                    min = min.min(sample_at(grid, width, height, x + dx, z + dz, 0));
                }
            }
            out[z as usize * width + x as usize] = min;
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

/// March a scalar coverage grid into contour polygons and greedy-merged
/// interior quads on `place`'s octimeter lattice. Samples `>= 128` are
/// inside; boundary vertices interpolate along crossed edges at the
/// half-coverage threshold (`127.5`).
pub fn march_grid(
    grid: &[u8],
    gw: usize,
    gh: usize,
    place: &GridPlacement,
    vertex: &impl Fn(f32, f32) -> Vertex,
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
            let bl = covered(grid[wj * gw + wi]);
            let br = covered(grid[wj * gw + wi + 1]);
            let tl = covered(grid[(wj + 1) * gw + wi]);
            let tr = covered(grid[(wj + 1) * gw + wi + 1]);
            let case = u8::from(bl) | u8::from(br) << 1 | u8::from(tr) << 2 | u8::from(tl) << 3;
            case_grid[wj * windows_x + wi] = case;
            full[wj * windows_x + wi] = case == 15;
        }
    }
    merge_interior(&full, windows_x, windows_z, place, vertex, tris);
    for wj in 0..windows_z {
        for wi in 0..windows_x {
            let case = case_grid[wj * windows_x + wi];
            if case == 0 || case == 15 {
                continue;
            }
            let window = MarchWindow { grid, gw, place };
            emit_window_contour(wi as i32, wj as i32, &window, case, vertex, tris);
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
    vertex: &impl Fn(f32, f32) -> Vertex,
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
            while wi + w < windows_x && full[wj * windows_x + wi + w] && !consumed[wj * windows_x + wi + w] {
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
            push_quad(tris, x0, z0, x1, z1, vertex);
        }
    }
}

/// Fan-triangulate one boundary window's inside polygon.
fn emit_window_contour(
    wi: i32,
    wj: i32,
    window: &MarchWindow<'_>,
    case: u8,
    vertex: &impl Fn(f32, f32) -> Vertex,
    tris: &mut Vec<DrawTriangle>,
) {
    emit_window_poly(wi, wj, window, CASE_POLYS[case as usize], vertex, tris);
}

/// Fan-triangulate one window polygon given by its boundary-point indices
/// (corners `0..4`, edge midpoints `4..8`), handing each vertex its point
/// index alongside the position — the label-marching form
/// [`emit_label_window`] emits through.
fn emit_window_poly_indexed(
    wi: i32,
    wj: i32,
    place: &GridPlacement,
    poly: &[u8],
    vertex: &impl Fn(f32, f32, u8) -> Vertex,
    tris: &mut Vec<DrawTriangle>,
) {
    if poly.len() < 3 {
        return; // the empty slot of a single-polygon case
    }
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
    let vert = |idx: u8| {
        let p = points[idx as usize];
        vertex(p[0] as f32 / OCTIMETERS_PER_METER, p[1] as f32 / OCTIMETERS_PER_METER, idx)
    };
    for k in 1..poly.len() - 1 {
        tris.push(DrawTriangle { verts: [vert(poly[0]), vert(poly[k]), vert(poly[k + 1])] });
    }
}

/// Fan-triangulate one window polygon given by its boundary-point indices
/// (corners `0..4`, edge midpoints `4..8`).
fn emit_window_poly(
    wi: i32,
    wj: i32,
    window: &MarchWindow<'_>,
    poly: &[u8],
    vertex: &impl Fn(f32, f32) -> Vertex,
    tris: &mut Vec<DrawTriangle>,
) {
    let step_oct = window.place.step_oct;
    let edge_t = |a: u8, b: u8| -> f32 {
        if a == b {
            0.5
        } else {
            ((COVERAGE_CROSSING - f32::from(a)) / (f32::from(b) - f32::from(a))).clamp(0.0, 1.0)
        }
    };
    let x_lo = window.place.origin_oct[0] + wi * step_oct;
    let x_hi = window.place.origin_oct[0] + (wi + 1) * step_oct;
    let z_lo = window.place.origin_oct[1] + wj * step_oct;
    let z_hi = window.place.origin_oct[1] + (wj + 1) * step_oct;
    let base = wj as usize * window.gw + wi as usize;
    let bl = window.grid[base];
    let br = window.grid[base + 1];
    let tl = window.grid[base + window.gw];
    let tr = window.grid[base + window.gw + 1];
    let bottom = edge_t(bl, br);
    let right = edge_t(br, tr);
    let top = edge_t(tl, tr);
    let left = edge_t(bl, tl);
    let interp = |lo: i32, hi: i32, t: f32| lo as f32 + (hi - lo) as f32 * t;
    let points = [
        [x_lo as f32, z_lo as f32],
        [x_hi as f32, z_lo as f32],
        [x_hi as f32, z_hi as f32],
        [x_lo as f32, z_hi as f32],
        [interp(x_lo, x_hi, bottom), z_lo as f32],
        [x_hi as f32, interp(z_lo, z_hi, right)],
        [interp(x_lo, x_hi, top), z_hi as f32],
        [x_lo as f32, interp(z_lo, z_hi, left)],
    ];
    let vert = |p: [f32; 2]| vertex(p[0] / OCTIMETERS_PER_METER, p[1] / OCTIMETERS_PER_METER);
    for k in 1..poly.len() - 1 {
        tris.push(DrawTriangle {
            verts: [vert(points[poly[0] as usize]), vert(points[poly[k] as usize]), vert(points[poly[k + 1] as usize])],
        });
    }
}

/// Push the two triangles of a quad spanning `[x0, x1] × [z0, z1]`
/// (octimeters), each corner built by `vertex` from its world position in
/// meters — the caller owns height and color.
pub fn push_quad(
    tris: &mut Vec<DrawTriangle>,
    x0: i32,
    z0: i32,
    x1: i32,
    z1: i32,
    vertex: &impl Fn(f32, f32) -> Vertex,
) {
    let vert = |x: i32, z: i32| vertex(x as f32 / OCTIMETERS_PER_METER, z as f32 / OCTIMETERS_PER_METER);
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

    fn count_true(grid: &[u8]) -> usize {
        grid.iter().filter(|&&sample| covered(sample)).count()
    }

    fn coverage(mask: &[bool]) -> Vec<u8> {
        mask.iter()
            .map(|&sample| {
                if sample {
                    u8::MAX
                } else {
                    0
                }
            })
            .collect()
    }

    fn minimize_bool(
        mask: &[bool],
        width: usize,
        height: usize,
        upsample: usize,
        params: &[SmoothParams],
    ) -> (Vec<u8>, usize, usize) {
        minimize_corners(&coverage(mask), width, height, upsample, params)
    }

    /// A flat colorless vertex builder — the `y = 0`, one-color case every
    /// geometry test wants.
    fn flat_vertex(wx: f32, wz: f32) -> Vertex {
        Vertex { x: wx, y: 0.0, z: wz, color: aether_math::Rgb::new(0.0, 0.0, 0.0) }
    }

    /// A uniform per-sample params slice — the single-setting case every
    /// caller had before the field made params spatial.
    fn uniform(iterations: u32, smoothing_degrees: u32, len: usize) -> Vec<SmoothParams> {
        vec![SmoothParams { iterations, smoothing_degrees }; len]
    }

    /// A raw upsample of `mask` by `factor` — the identity `minimize_corners`
    /// must reproduce at zero iterations.
    fn raw_upsample(mask: &[bool], w: usize, h: usize, factor: usize) -> Vec<u8> {
        let mut out = vec![0; w * factor * h * factor];
        let gw = w * factor;
        for gz in 0..h * factor {
            for gx in 0..gw {
                out[gz * gw + gx] = if mask[(gz / factor) * w + gx / factor] {
                    u8::MAX
                } else {
                    0
                };
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
        let grid = coverage(&[true, false, false, true]); // BL, BR, TL, TR
        let place = GridPlacement { origin_oct: [0, 0], step_oct: 64 };
        let mut tris = Vec::new();
        march_grid(&grid, 2, 2, &place, &flat_vertex, &mut tris);
        assert_eq!(tris.len(), 4, "the connected hexagon fans to 4 triangles");
        // BL corner = (0,0) m; TR corner = (64,64) oct = (0.25,0.25) m.
        let joins = tris.iter().any(|t| {
            let has = |x: f32, z: f32| t.verts.iter().any(|v| (v.x - x).abs() < 1e-6 && (v.z - z).abs() < 1e-6);
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
        let mut mask = vec![false; gw * gh];
        for z in 0..gh {
            for x in 0..3 {
                mask[z * gw + x] = true;
            }
        }
        let grid = coverage(&mask);
        let place = GridPlacement { origin_oct: [0, 0], step_oct: 64 };
        let mut tris = Vec::new();
        march_grid(&grid, gw, gh, &place, &flat_vertex, &mut tris);
        let xs: Vec<f32> = tris.iter().flat_map(|t| t.verts.iter()).map(|v| v.x).collect();
        let max_x = xs.iter().copied().fold(f32::MIN, f32::max);
        // Window 2 (samples 2,3) is the boundary; its right/top mids sit at
        // 2*step + step/2 = 160 oct = 0.625 m — the rightmost geometry.
        assert!((max_x - 0.625).abs() < 1e-6, "boundary at 0.625 m, got {max_x}");
        let on_edge = xs.iter().filter(|&&x| x > 0.5).count();
        assert!(on_edge >= 2, "the edge is one straight, repeated line");
        assert!(
            xs.iter().filter(|&&x| x > 0.5).all(|&x| (x - 0.625).abs() < 1e-6),
            "no crossing kinks short of the straight edge",
        );
    }

    #[test]
    fn geometry_sits_at_the_placement_origin() {
        // The same window marched at a shifted origin must move by exactly
        // the shift — a chunk away from the world origin places its
        // geometry over its own cells, not stacked at zero.
        let grid = coverage(&[true, false, false, false]); // BL only
        let mut base = Vec::new();
        march_grid(&grid, 2, 2, &GridPlacement { origin_oct: [0, 0], step_oct: 64 }, &flat_vertex, &mut base);
        let mut shifted = Vec::new();
        march_grid(&grid, 2, 2, &GridPlacement { origin_oct: [1024, 512], step_oct: 64 }, &flat_vertex, &mut shifted);
        assert_eq!(base.len(), shifted.len());
        for (b, s) in base.iter().zip(&shifted) {
            for (vb, vs) in b.verts.iter().zip(&s.verts) {
                assert!((vs.x - vb.x - 4.0).abs() < 1e-6, "x shift is 1024 oct = 4 m");
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
        let (grid, gw, gh) = minimize_bool(&mask, w, h, 3, &uniform(0, 90, w * h));
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
        let raw = count_true(&minimize_bool(&mask, w, h, 4, &uniform(0, 45, w * h)).0);
        let one = count_true(&minimize_bool(&mask, w, h, 4, &uniform(1, 45, w * h)).0);
        let two = count_true(&minimize_bool(&mask, w, h, 4, &uniform(3, 45, w * h)).0);
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
        let (grid, _, _) = minimize_bool(&mask, w, h, 4, &uniform(3, 90, w * h));
        assert_eq!(grid, raw_upsample(&mask, w, h, 4), "a straight edge is untouched");
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
        let one = minimize_bool(&mask, w, h, 2, &uniform(1, 90, w * h)).0;
        let five = minimize_bool(&mask, w, h, 2, &uniform(5, 90, w * h)).0;
        assert_eq!(one, five, "the cellular passes leave a 45-degree diagonal alone at 90 degrees");
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
        let at_90 = count_true(&minimize_bool(&mask, w, h, 2, &uniform(8, 90, w * h)).0);
        let at_45 = count_true(&minimize_bool(&mask, w, h, 2, &uniform(8, 45, w * h)).0);
        assert!(at_45 < at_90, "the 45-degree gate flattens more: {at_45} < {at_90}");
    }

    #[test]
    fn binary_minimization_stays_binary_and_matches_raw_when_crisp() {
        // Binary coverage keeps the legacy byte shape: no scalar values are
        // introduced, and a zero-iteration field is byte-identical to a raw
        // 0/255 upsample.
        let (w, h) = (5usize, 4usize);
        let rows = ["#..#.", "###..", ".##..", "....."];
        let mask: Vec<bool> = rows.iter().flat_map(|r| r.bytes().map(|b| b == b'#')).collect();
        let (grid, _, _) = minimize_bool(&mask, w, h, 3, &uniform(0, 90, w * h));
        assert_eq!(grid, raw_upsample(&mask, w, h, 3));
        assert!(grid.iter().all(|sample| binary_coverage(*sample)), "binary coverage remains binary after upsample");
    }

    #[test]
    fn scalar_edge_crossing_uses_authored_fraction() {
        // A vertical soft edge from 255 to 64 crosses half coverage at
        // t=(127.5-255)/(64-255), not at the binary midpoint. The emitted
        // boundary should therefore sit farther right than 0.5 of the
        // window width.
        let grid = [255, 64, 255, 64]; // BL, BR, TL, TR
        let place = GridPlacement { origin_oct: [0, 0], step_oct: 64 };
        let mut tris = Vec::new();
        march_grid(&grid, 2, 2, &place, &flat_vertex, &mut tris);
        let max_x = tris.iter().flat_map(|t| t.verts.iter()).map(|v| v.x).fold(f32::MIN, f32::max);
        let expected = ((127.5 - 255.0) / (64.0 - 255.0)) * 64.0 / 256.0;
        assert!((max_x - expected).abs() < 1e-6, "scalar crossing at {expected}, got {max_x}");
    }

    #[test]
    fn scalar_zone_is_frozen_out_of_corner_flips() {
        // A lone scalar sample would be rounded away by the binary corner
        // rules if it were treated as covered. Instead the scalar 2x2
        // neighborhoods are bilinear and frozen, so extra smoothing passes
        // cannot change the reconstructed coverage field.
        let (w, h) = (3usize, 3usize);
        let mut field = vec![0; w * h];
        field[w + 1] = 200;
        let raw = minimize_corners(&field, w, h, 3, &uniform(0, 45, w * h)).0;
        let smoothed = minimize_corners(&field, w, h, 3, &uniform(4, 45, w * h)).0;
        assert_eq!(smoothed, raw, "scalar neighborhoods do not corner-flip");
        assert!(smoothed.iter().any(|sample| *sample != 0 && *sample != u8::MAX), "the scalar zone remains scalar");
    }

    #[test]
    fn a_split_field_smooths_only_its_own_side() {
        // Two identical squares, one field: the left square's cells sit at
        // zero iterations, the right square's at three. The left square
        // must come out exactly as the raw upsample (a crisp authored
        // zone), the right must lose corner area — and the right side must
        // match a uniform run over the same square, so a zone boundary
        // never bleeds smoothing into (or steals it from) the other side.
        let (w, h) = (16usize, 8usize);
        let mut mask = vec![false; w * h];
        for z in 2..6 {
            for x in 2..6 {
                mask[z * w + x] = true; // left square
            }
            for x in 10..14 {
                mask[z * w + x] = true; // right square
            }
        }
        let mut params = uniform(0, 45, w * h);
        for z in 0..h {
            for x in w / 2..w {
                params[z * w + x].iterations = 3;
            }
        }
        let (grid, gw, _) = minimize_bool(&mask, w, h, 2, &params);

        // Left half: identical to the raw upsample.
        let raw = raw_upsample(&mask, w, h, 2);
        for gz in 0..h * 2 {
            for gx in 0..w {
                assert_eq!(grid[gz * gw + gx], raw[gz * gw + gx], "the zero-iteration side stays raw at ({gx}, {gz})");
            }
        }

        // Right half: smoothed — and exactly as a uniform run smooths it.
        let right_zone: usize = (0..h * 2)
            .flat_map(|gz| (w..w * 2).map(move |gx| (gx, gz)))
            .filter(|&(gx, gz)| covered(grid[gz * gw + gx]))
            .count();
        let raw_zone = 8 * 8;
        assert!(right_zone < raw_zone, "the smoothing side loses corner area: {right_zone} < {raw_zone}");
        let (uniform_grid, ugw, _) = minimize_bool(&mask, w, h, 2, &uniform(3, 45, w * h));
        for gz in 0..h * 2 {
            for gx in w + 4..w * 2 {
                assert_eq!(
                    grid[gz * gw + gx],
                    uniform_grid[gz * ugw + gx],
                    "away from the seam the field side matches a uniform run at ({gx}, {gz})",
                );
            }
        }
    }

    #[test]
    fn smoothed_boundaries_carry_no_one_wide_artifacts() {
        // An organic shoreline-shaped mask (a section of the demo lake)
        // drives the cellular passes into their known failure mode: the
        // final pass fills samples whose neighbors it simultaneously cut,
        // leaving one-sample-wide bumps that march into triangles jutting
        // off the boundary. The prune sweep must converge them away: every
        // covered sample in the output has at least two covered orthogonal
        // neighbors, and every uncovered sample at most two covered ones.
        let rows = [
            "..........................",
            "..........................",
            "..........##..............",
            ".......#########..........",
            "......############........",
            ".....###############......",
            ".....##################...",
            ".....#####################",
            ".....#####################",
            ".....#####################",
            ".....#####################",
            ".....#####################",
            ".....#####################",
            ".....#####################",
        ];
        let (w, h) = (rows[0].len(), rows.len());
        let mask: Vec<bool> = rows.iter().flat_map(|r| r.bytes().map(|b| b == b'#')).collect();
        let (grid, gw, gh) = minimize_bool(&mask, w, h, 2, &uniform(3, 90, w * h));
        for gz in 0..gh as i32 {
            for gx in 0..gw as i32 {
                let m = covered(grid[gz as usize * gw + gx as usize]);
                let orth_covered = orthogonal_match_count(&grid, gw, gh, gx, gz, m);
                if m {
                    assert!(orth_covered >= 2, "one-wide bump survived at ({gx}, {gz})");
                } else {
                    assert!(orth_covered <= 2, "one-wide notch survived at ({gx}, {gz})");
                }
            }
        }
    }

    #[test]
    fn repartition_matches_the_boolean_path_on_two_labels() {
        // Tripwire: with exactly two labels the repartition must reproduce
        // minimize_corners sample-for-sample — the chamfer target is the
        // one other label, the dominant differing neighbor is too, and the
        // unified prune rule covers the boolean cut and fill. The fixture
        // is the shoreline mask that exercises the prune's failure mode.
        let rows = [
            "..........................",
            "..........................",
            "..........##..............",
            ".......#########..........",
            "......############........",
            ".....###############......",
            ".....##################...",
            ".....#####################",
            ".....#####################",
            ".....#####################",
            ".....#####################",
            ".....#####################",
            ".....#####################",
            ".....#####################",
        ];
        let (w, h) = (rows[0].len(), rows.len());
        let mask: Vec<bool> = rows.iter().flat_map(|r| r.bytes().map(|b| b == b'#')).collect();
        let ids: Vec<u8> = mask.iter().map(|&b| u8::from(b)).collect();
        let params = uniform(3, 90, w * h);
        let (bool_grid, gw, gh) = minimize_bool(&mask, w, h, 2, &params);
        let (id_grid, igw, igh) = repartition(&ids, w, h, 2, &params, &vec![false; w * h]);
        assert_eq!((gw, gh), (igw, igh));
        for (i, (&b, &id)) in bool_grid.iter().zip(&id_grid).enumerate() {
            assert_eq!(u8::from(covered(b)), id, "two-label repartition diverges at {i}");
        }
    }

    #[test]
    fn repartition_never_moves_a_crisp_zone() {
        // A sample flips only when its own cell's params allow, so a
        // zero-iteration zone neither yields territory nor absorbs any —
        // its samples come out exactly as the raw upsample.
        let (w, h) = (12usize, 8usize);
        let mut ids = vec![1u8; w * h];
        for z in 0..h {
            for x in 6..12 {
                ids[z * w + x] = 2; // material 2 on the right half
            }
        }
        // A notch in the boundary so smoothing has corners to eat.
        ids[3 * w + 5] = 2;
        let mut params = uniform(3, 45, w * h);
        for z in 0..h {
            for x in 0..6 {
                params[z * w + x].iterations = 0; // left zone is crisp
            }
        }
        let (grid, gw, _) = repartition(&ids, w, h, 2, &params, &vec![false; w * h]);
        for gz in 0..h * 2 {
            for gx in 0..12 {
                assert_eq!(grid[gz * gw + gx], ids[(gz / 2) * w + gx / 2], "crisp-zone sample moved at ({gx}, {gz})");
            }
        }
    }

    fn poly_area(tris: &[DrawTriangle]) -> f32 {
        tris.iter()
            .map(|t| {
                let (ax, az) = (t.verts[0].x, t.verts[0].z);
                let (bx, bz) = (t.verts[1].x, t.verts[1].z);
                let (cx, cz) = (t.verts[2].x, t.verts[2].z);
                (cx - ax).mul_add(-(bz - az), (bx - ax) * (cz - az)).abs() * 0.5
            })
            .sum()
    }

    #[test]
    fn saddle_windows_tile_exactly() {
        // A two-label saddle window: the higher label connects its
        // diagonal, the lower splits into two corner triangles, and the
        // union covers the window exactly once.
        let place = GridPlacement { origin_oct: [0, 0], step_oct: 64 };
        // Samples [BL, BR, TL, TR] = [1, 2, 2, 1]: label 1 case 5, label 2
        // case 10. Label 2 wins the diagonal, label 1 splits.
        let ids = [1u8, 2, 2, 1];
        let mut tris = Vec::new();
        let case1 = label_case(&ids, 2, 0, 0, 1);
        let case2 = label_case(&ids, 2, 0, 0, 2);
        assert_eq!((case1, case2), (5, 10));
        let indexed = |wx: f32, wz: f32, _point: u8| flat_vertex(wx, wz);
        emit_label_window(0, 0, &place, case1, false, &indexed, &mut tris);
        let split_area = poly_area(&tris);
        emit_label_window(0, 0, &place, case2, true, &indexed, &mut tris);
        let window_area = (64.0f32 / 256.0) * (64.0 / 256.0);
        assert!((poly_area(&tris) - window_area).abs() < 1e-6, "the saddle window tiles exactly");
        assert!((split_area - window_area / 4.0).abs() < 1e-6, "the split loser holds its two corner triangles");
    }

    #[test]
    fn three_label_windows_tile_exactly() {
        // Three labels in one window: the diagonal-pair label connects,
        // the two single-corner labels take their triangles, and the
        // union tiles the window.
        let place = GridPlacement { origin_oct: [0, 0], step_oct: 64 };
        // Samples [BL, BR, TL, TR] = [1, 2, 3, 1]: label 1 case 5 (both
        // diagonal corners), labels 2 and 3 one corner each.
        let ids = [1u8, 2, 3, 1];
        let mut tris = Vec::new();
        let indexed = |wx: f32, wz: f32, _point: u8| flat_vertex(wx, wz);
        for label in 1..=3u8 {
            let case = label_case(&ids, 2, 0, 0, label);
            // Label 1's saddle faces two different labels (2 != 3), so it
            // connects; the single-corner labels have no saddle at all.
            emit_label_window(0, 0, &place, case, true, &indexed, &mut tris);
        }
        let window_area = (64.0f32 / 256.0) * (64.0 / 256.0);
        assert!((poly_area(&tris) - window_area).abs() < 1e-6, "three labels tile the window exactly");
    }

    #[test]
    fn erode_peels_a_band() {
        // Eroding a solid block by radius 1 removes its border ring, leaving
        // an inset body for a second march to lay inside a rim.
        let (w, h) = (5usize, 5usize);
        let grid = vec![u8::MAX; w * h];
        let out = erode(&grid, w, h, 1);
        // Only the 3x3 interior survives.
        assert_eq!(count_true(&out), 9);
        assert!(covered(out[2 * w + 2]), "the center survives");
        assert!(!covered(out[0]), "the corner is peeled");
    }

    #[test]
    fn erode_uses_grayscale_min_filter() {
        let (w, h) = (3usize, 3usize);
        let mut grid = vec![200; w * h];
        grid[0] = 77;
        let out = erode(&grid, w, h, 1);
        assert_eq!(out[w + 1], 77, "the center sees the corner minimum");
        assert_eq!(out[2 * w + 2], 0, "out-of-bounds samples are uncovered");
    }
}
