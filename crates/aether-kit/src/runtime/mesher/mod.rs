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
// The bilinear-patch and shade arithmetic is written as explicit
// multiply-add chains for readability; a fused mul_add would need a libm
// symbol on the wasm target and does not change the result meaningfully.
#![allow(clippy::suboptimal_flops)]

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
//! # Underlay pass — one surface, repartitioned
//!
//! The ground is a single surface, world-space in meters (`1 cell =
//! 1 m`), tiled exactly by its material regions. The
//! cascade-resolved material grid runs through [`repartition`] — the
//! multi-label corner minimization, honoring the per-cell smoothing field
//! — so smoothing moves samples between materials rather than layering
//! shapes; each material then splits into a pooled rim band (within the
//! rim width of its contour) and a keyed body. Wherever a cell and its
//! one-sample surround are uniformly body, the cell emits the flat keyed
//! quilt cell — color resolved from its world center ([`resolve_cell`]),
//! a low-amplitude wash along the stroke flow field, and same-material
//! hue steps past the blob-merge threshold pooling a nine-slice rim.
//! Everywhere else the partition marches per window, each label's
//! polygon colored by its material keyed at the owning cell, rims
//! darkened, saddles resolved by label order so every window tiles.
//! Material boundaries smooth identically whether the materials differ by
//! explicit paint or by region default.
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
//! geometry lifts a hair above the underlay surface so the coplanar
//! passes never z-fight, and it carries its own vertices so the seam
//! stays hard.
//!
//! # Height pass — plates, slopes, and skirts
//!
//! Every vertex lifts onto the plate-resolved height surface
//! ([`World::surface_height_in`]): cell heights blend into continuous
//! slopes where neighbors sit within the step ceiling
//! ([`crate::world::STEP_MAX_OCTIMETERS`]) and break where they exceed
//! it — the corner plates split exactly on cliff edges, an interior
//! cell's owner-pinned patch lands the break on the cell line, and
//! the skirt pass closes that gap with a vertical face wearing the high
//! cell's region cliff material, darkened toward its base. Boundary
//! windows and overlay contours lift each vertex through its own
//! (floor) cell — continuous wherever no cliff intervenes — and water
//! rides the same surface for now (an authored per-body water level is
//! future work). A slope shade from the patch normal bakes into vertex
//! color so slopes read under the flat-color grammar; it multiplies by
//! exactly one on level ground, so an all-flat world meshes
//! byte-identically to a world with no height pass at all.

pub mod contour;
pub mod style;

use alloc::vec;
use alloc::vec::Vec;

use aether_capabilities::render::{DrawTriangle, Vertex};
use aether_math::Vec3;

use crate::world::{
    CELLS_PER_CHUNK, CellPos, ChunkPos, Material, STEP_MAX_OCTIMETERS, SUBCELLS_PER_CELL_EDGE,
    ViewMode, World,
};
use contour::{
    GridPlacement, SmoothParams, emit_label_window, erode, label_case, march_grid,
    minimize_corners, push_quad, repartition,
};
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

/// The directional light the slope shade reads, world-space (Y up). Only
/// the direction matters — the shade divides by the Y component, so flat
/// ground multiplies by exactly one and stays untouched by the height
/// pass.
const SLOPE_LIGHT: Vec3 = Vec3::new(0.4, 1.0, 0.6);

/// Slope-shade floor — the darkest a steep away-facing slope goes.
const SLOPE_SHADE_MIN: f32 = 0.55;

/// Slope-shade ceiling — the brightest a light-facing slope goes.
const SLOPE_SHADE_MAX: f32 = 1.25;

/// Cliff-face lightness multiplier at the top of a skirt.
const SKIRT_TOP_SHADE: f32 = 0.80;

/// Cliff-face lightness multiplier at the base of a skirt — darker than
/// the top, so the face reads as receding toward the ground shadow.
const SKIRT_BASE_SHADE: f32 = 0.55;

/// The raw calibration view's flat gray for cliff faces — a skirt has no
/// noise field to calibrate; it only keeps the terrain closed.
const RAW_SKIRT_GRAY: f32 = 0.35;

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
    emit_skirts(world, at, mode, &mut tris);
    tris
}

/// One cell's bilinear surface patch: the four plate-resolved corner
/// heights ([`World::cell_corner_heights`]) plus the height and shade
/// evaluations the mesher builds vertices from. Owner-pinned — a vertex
/// on a cliff edge evaluated through the high cell's patch reads the
/// high plate, which is exactly how the drawn break lands on the cliff
/// line.
struct CellLift {
    x0: f32,
    z0: f32,
    corners: [f32; 4],
}

impl CellLift {
    fn of(world: &World, cell: CellPos) -> Self {
        Self {
            x0: cell.x as f32,
            z0: cell.z as f32,
            corners: world.cell_corner_heights(cell),
        }
    }

    /// The patch height at `(wx, wz)` meters, coordinates clamped to the
    /// cell span — [`World::surface_height_in`] over the cached corners.
    fn y(&self, wx: f32, wz: f32) -> f32 {
        let fx = (wx - self.x0).clamp(0.0, 1.0);
        let fz = (wz - self.z0).clamp(0.0, 1.0);
        let bottom = self.corners[0] + (self.corners[1] - self.corners[0]) * fx;
        let top = self.corners[2] + (self.corners[3] - self.corners[2]) * fx;
        bottom + (top - bottom) * fz
    }

    /// The slope-shade multiplier at `(wx, wz)`: the patch normal against
    /// [`SLOPE_LIGHT`], scaled so level ground reads exactly one.
    fn shade(&self, wx: f32, wz: f32) -> f32 {
        let fx = (wx - self.x0).clamp(0.0, 1.0);
        let fz = (wz - self.z0).clamp(0.0, 1.0);
        let c = &self.corners;
        let grad_x = (c[1] - c[0]) * (1.0 - fz) + (c[3] - c[2]) * fz;
        let grad_z = (c[2] - c[0]) * (1.0 - fx) + (c[3] - c[1]) * fx;
        let normal = Vec3::new(-grad_x, 1.0, -grad_z).normalize();
        (normal.dot(SLOPE_LIGHT) / SLOPE_LIGHT.y).clamp(SLOPE_SHADE_MIN, SLOPE_SHADE_MAX)
    }
}

/// The surface height and slope shade at a world point, resolved through
/// the point's own (floor) cell — the position-pure form overlay vertices
/// lift through, continuous wherever no cliff intervenes.
fn point_lift(world: &World, wx: f32, wz: f32) -> (f32, f32) {
    let lift = CellLift::of(
        world,
        CellPos {
            x: floor_to_i32(wx),
            z: floor_to_i32(wz),
        },
    );
    (lift.y(wx, wz), lift.shade(wx, wz))
}

/// The lift for a vertex of label geometry owned by `owner`: position-pure
/// through the vertex's own (floor) cell, unless that cell stands a cliff
/// apart from the owner — then the owner's clamped patch wins, so a
/// boundary polygon at a cliff stays on its own side of the break instead
/// of stretching down the face (the skirt draws the face). On continuous
/// ground the two forms agree and the rule is invisible.
fn label_lift(world: &World, owner: CellPos, wx: f32, wz: f32) -> (f32, f32) {
    let cell = CellPos {
        x: floor_to_i32(wx),
        z: floor_to_i32(wz),
    };
    if cell != owner && (world.height(cell) - world.height(owner)).abs() > STEP_MAX_OCTIMETERS {
        let lift = CellLift::of(world, owner);
        return (lift.y(wx, wz), lift.shade(wx, wz));
    }
    point_lift(world, wx, wz)
}

/// Floor to `i32` — `as i32` truncates toward zero, which is wrong for
/// negative world coordinates, so step down when it rounded up.
fn floor_to_i32(v: f32) -> i32 {
    let t = v as i32;
    if (t as f32) > v { t - 1 } else { t }
}

/// The per-side pooled-rim strengths `[left, right, top, bottom]` for an
/// interior cell of `material`: a proportional rim where a same-material
/// neighbor's hue steps past the blob-merge threshold. Material-change
/// edges rim through the partition's marched band, never here. A pure
/// function of the cell and its four neighbors, so the two cells sharing
/// an edge agree on the rim there.
fn cell_rims(world: &World, cell: CellPos, material: Material) -> [f32; 4] {
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
        if neighbor != material {
            continue; // the partition's marched rim owns this edge
        }
        let hue_b = resolve_cell(neighbor, *nx as f32 + 0.5, *nz as f32 + 0.5, None).hue;
        rims[k] = rim_strength(true, true, hue_a, hue_b, threshold);
    }
    rims
}

/// The partition's inputs at subcell expression: the cascade-resolved
/// material id and the smoothing params (the cell's field override, else
/// the sample's own material's style row) for every sample of the chunk
/// plus its apron. `None` when the whole area is Void — nothing to mesh.
fn partition_inputs(
    world: &World,
    at: ChunkPos,
    apron: i32,
    n: usize,
) -> Option<(Vec<u8>, Vec<SmoothParams>)> {
    let mut ids = vec![0u8; n * n];
    let mut params = vec![
        SmoothParams {
            iterations: 0,
            smoothing_degrees: 90,
        };
        n * n
    ];
    let mut any = false;
    for sj in -apron..SUBCELLS_PER_CHUNK_EDGE + apron {
        for si in -apron..SUBCELLS_PER_CHUNK_EDGE + apron {
            let cell = CellPos {
                x: at.x * EDGE + si.div_euclid(SUB),
                z: at.z * EDGE + sj.div_euclid(SUB),
            };
            let material = world.underlay(cell);
            let idx = (sj + apron) as usize * n + (si + apron) as usize;
            ids[idx] = material.to_u8();
            any |= material != Material::Void;
            params[idx] = world.smoothing_override(cell).map_or_else(
                || {
                    let s = style(material);
                    SmoothParams {
                        iterations: s.smoothing_iterations,
                        smoothing_degrees: s.smoothing_degrees,
                    }
                },
                |profile| SmoothParams {
                    iterations: profile.iterations,
                    smoothing_degrees: profile.degrees,
                },
            );
        }
    }
    any.then_some((ids, params))
}

/// The display label a repartitioned material sample renders as: the
/// pooled rim band (odd, `2m - 1`) within the rim width of the material's
/// contour, or the keyed body (even, `2m`) inside it. Void stays `0`.
/// Saddle conflicts resolve by label order, so the split is also the
/// painter's priority.
fn display_label(material: u8, body: bool) -> u8 {
    if material == 0 {
        0
    } else if body {
        2 * material
    } else {
        2 * material - 1
    }
}

/// Emit the underlay as a partition of one flat surface: repartition the
/// cascade-resolved material grid under the smoothing rules, split each
/// material into rim band and body, then tile the ground exactly — flat
/// keyed quilt cells (with same-material hue-step rims) wherever a cell
/// and its one-sample surround are uniformly body, and per-window marching
/// polygons everywhere else, all at `y = 0` with no lifts. Every decision
/// is a pure function of world coordinates, so two chunks emit identical
/// geometry over their shared apron and the overlap is invisible.
fn mesh_underlay(world: &World, at: ChunkPos, tris: &mut Vec<DrawTriangle>) {
    let apron = MAX_APRON_SUBCELLS;
    let n = (SUBCELLS_PER_CHUNK_EDGE + 2 * apron) as usize;
    let Some((ids, params)) = partition_inputs(world, at, apron, n) else {
        return;
    };

    let upsample = CONTOUR_UPSAMPLE;
    let (grid, gw, _gh) = repartition(&ids, n, n, upsample, &params);
    let u = upsample as i32;
    let step_oct = OCTIMETERS_PER_SUBCELL / u;

    // Split every material into rim band and body labels.
    let mut display = vec![0u8; grid.len()];
    let mut present = [false; 6];
    for &m in &grid {
        present[m as usize] = true;
    }
    for m in 1..6u8 {
        if !present[m as usize] {
            continue;
        }
        let region: Vec<bool> = grid.iter().map(|&g| g == m).collect();
        let rim_width =
            (style(Material::from_u8_or_void(m)).rim_inset_octimeters / step_oct).max(1);
        let eroded = erode(&region, gw, gw, rim_width);
        for (idx, &g) in grid.iter().enumerate() {
            if g == m {
                display[idx] = display_label(m, eroded[idx]);
            }
        }
    }

    // Interior cells: the cell's sample block plus a one-sample surround
    // is uniformly its own material's body. Classified for every cell
    // whose surround fits the grid (local -1..EDGE), identically on both
    // sides of a chunk border, so the window skip below never disagrees
    // with a neighbor's cell quad.
    let lo = -1i32;
    let hi = EDGE;
    let cells_w = (hi - lo) as usize;
    let mut interior = vec![false; cells_w * cells_w];
    for lz in lo..hi {
        for lx in lo..hi {
            let cell = CellPos {
                x: at.x * EDGE + lx,
                z: at.z * EDGE + lz,
            };
            let m = world.underlay(cell).to_u8();
            if m == 0 {
                continue;
            }
            let body = display_label(m, true);
            let x0 = (lx * SUB + apron) * u;
            let z0 = (lz * SUB + apron) * u;
            let span = SUB * u;
            let uniform = ((z0 - 1)..=(z0 + span)).all(|gz| {
                ((x0 - 1)..=(x0 + span)).all(|gx| {
                    gx >= 0
                        && gz >= 0
                        && (gx as usize) < gw
                        && (gz as usize) < gw
                        && display[gz as usize * gw + gx as usize] == body
                })
            });
            interior[(lz - lo) as usize * cells_w + (lx - lo) as usize] = uniform;
        }
    }

    // Chunk-local interior cells emit the keyed quilt cell.
    for lz in 0..EDGE {
        for lx in 0..EDGE {
            if !interior[(lz - lo) as usize * cells_w + (lx - lo) as usize] {
                continue;
            }
            let cell = CellPos {
                x: at.x * EDGE + lx,
                z: at.z * EDGE + lz,
            };
            let material = world.underlay(cell);
            let resolved = resolve_cell(material, cell.x as f32 + 0.5, cell.z as f32 + 0.5, None);
            let rims = cell_rims(world, cell, material);
            let lift = CellLift::of(world, cell);
            emit_underlay_cell(material, &resolved, cell.x, cell.z, rims, &lift, tris);
        }
    }

    emit_partition_windows(
        world, at, &display, gw, apron, step_oct, &interior, lo, tris,
    );
}

/// Emit the marching-squares windows of the partition's boundary zone.
/// Each window is owned by exactly one chunk — the one holding the cell
/// under its center — so the boundary zone is emitted once fleet-wide,
/// with no cross-chunk duplicates against the fixed per-frame vertex
/// budget. Windows fully covered by interior cell quads are skipped;
/// uniform single-label windows coalesce into row strips (per owning
/// cell, so the keyed color stays flat per cell); mixed windows emit each
/// label's case polygon, rim labels darkened, saddles resolved by label
/// order so every window tiles exactly.
#[allow(clippy::too_many_arguments)] // one call site; the partition state travels together
fn emit_partition_windows(
    world: &World,
    at: ChunkPos,
    display: &[u8],
    gw: usize,
    apron: i32,
    step_oct: i32,
    interior: &[bool],
    lo: i32,
    tris: &mut Vec<DrawTriangle>,
) {
    let hi = EDGE;
    let cells_w = (hi - lo) as usize;
    let base_oct = [
        at.x * SUBCELLS_PER_CHUNK_EDGE * OCTIMETERS_PER_SUBCELL,
        at.z * SUBCELLS_PER_CHUNK_EDGE * OCTIMETERS_PER_SUBCELL,
    ];
    let origin_oct = [
        base_oct[0] - apron * OCTIMETERS_PER_SUBCELL + step_oct / 2,
        base_oct[1] - apron * OCTIMETERS_PER_SUBCELL + step_oct / 2,
    ];
    let place = GridPlacement {
        origin_oct,
        step_oct,
    };
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
        emit_label_quad(world, label, owner, rect, tris);
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
            // there).
            if corners[0] != 0 && corners.iter().all(|&c| c == corners[0]) {
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
            emit_mixed_window(world, display, gw, wi, wj, corners, owner, &place, tris);
        }
        flush(&mut run, windows, wj, tris);
    }
}

/// Emit one display label's quad over `rect`, colored through its owning
/// cell and lifted position-pure — the strip-run emit. Windows overhang
/// their owner by half a window, so each corner reads the surface through
/// its own cell rather than the owner's clamped patch.
fn emit_label_quad(
    world: &World,
    label: u8,
    owner: CellPos,
    rect: [i32; 4],
    tris: &mut Vec<DrawTriangle>,
) {
    let material = Material::from_u8_or_void(label.div_ceil(2));
    let resolved = resolve_cell(material, owner.x as f32 + 0.5, owner.z as f32 + 0.5, None);
    let rim = if label % 2 == 1 {
        style(material).rim_darken
    } else {
        0.0
    };
    let surface = |wx: f32, wz: f32| label_lift(world, owner, wx, wz);
    emit_quad_shaded(material, &resolved, rect, rim, &surface, tris);
}

/// Emit every label's case polygon for one mixed boundary window, colored
/// by its material keyed at the window's owning cell (rim labels
/// darkened) and lifted per vertex through the vertex's own cell. A
/// two-label saddle resolves by label order — the higher label connects
/// its diagonal, the lower splits — so the window always tiles exactly.
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
    tris: &mut Vec<DrawTriangle>,
) {
    let step_oct = place.step_oct;
    let x_lo = place.origin_oct[0] + wi as i32 * step_oct;
    let z_lo = place.origin_oct[1] + wj as i32 * step_oct;
    let wash_x = (x_lo + step_oct / 2) as f32 / OCTIMETERS_PER_METER;
    let wash_z = (z_lo + step_oct / 2) as f32 / OCTIMETERS_PER_METER;
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
        let material = Material::from_u8_or_void(label.div_ceil(2));
        let s = style(material);
        let resolved = resolve_cell(material, owner.x as f32 + 0.5, owner.z as f32 + 0.5, None);
        let mut light = wash_lightness(material, resolved.light, wash_x, wash_z, resolved.stroke);
        if label % 2 == 1 {
            light *= 1.0 - s.rim_darken;
        }
        let vertex = |wx: f32, wz: f32| {
            let (y, shade) = label_lift(world, owner, wx, wz);
            let rgb = hsl_to_linear_rgb(
                resolved.hue,
                resolved.sat,
                (light * shade).clamp(0.0, 100.0),
            );
            Vertex {
                x: wx,
                y,
                z: wz,
                r: rgb[0],
                g: rgb[1],
                b: rgb[2],
            }
        };
        emit_label_window(wi as i32, wj as i32, place, case, connected, &vertex, tris);
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
    patch: &CellLift,
    tris: &mut Vec<DrawTriangle>,
) {
    let surface = |wx: f32, wz: f32| (patch.y(wx, wz), patch.shade(wx, wz));
    let surface = &surface;
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
        emit_quad_shaded(material, resolved, [x0, z0, x3, z3], 0.0, surface, tris);
        return;
    }
    let darken = s.rim_darken;
    let (left, right, top, bottom) = (rims[0], rims[1], rims[2], rims[3]);
    // Interior.
    emit_quad_shaded(material, resolved, [x1, z1, x2, z2], 0.0, surface, tris);
    // Edge strips.
    emit_quad_shaded(
        material,
        resolved,
        [x0, z1, x1, z2],
        darken * left,
        surface,
        tris,
    );
    emit_quad_shaded(
        material,
        resolved,
        [x2, z1, x3, z2],
        darken * right,
        surface,
        tris,
    );
    emit_quad_shaded(
        material,
        resolved,
        [x1, z0, x2, z1],
        darken * top,
        surface,
        tris,
    );
    emit_quad_shaded(
        material,
        resolved,
        [x1, z2, x2, z3],
        darken * bottom,
        surface,
        tris,
    );
    // Corners darken by the stronger of their two adjacent sides.
    emit_quad_shaded(
        material,
        resolved,
        [x0, z0, x1, z1],
        darken * left.max(top),
        surface,
        tris,
    );
    emit_quad_shaded(
        material,
        resolved,
        [x2, z0, x3, z1],
        darken * right.max(top),
        surface,
        tris,
    );
    emit_quad_shaded(
        material,
        resolved,
        [x0, z2, x1, z3],
        darken * left.max(bottom),
        surface,
        tris,
    );
    emit_quad_shaded(
        material,
        resolved,
        [x2, z2, x3, z3],
        darken * right.max(bottom),
        surface,
        tris,
    );
}

/// Push the two triangles of one underlay quad spanning `rect`
/// (`[x0, z0, x1, z1]` octimeters) on the given surface evaluator
/// (`(wx, wz)` meters to `(y, slope shade)`), each corner shaded by the
/// wash field and the slope shade at its own world position and darkened
/// by `rim_darken`.
fn emit_quad_shaded(
    material: Material,
    resolved: &ResolvedCell,
    rect: [i32; 4],
    rim_darken: f32,
    surface: &impl Fn(f32, f32) -> (f32, f32),
    tris: &mut Vec<DrawTriangle>,
) {
    let corner = |xo: i32, zo: i32| {
        let wx = xo as f32 / OCTIMETERS_PER_METER;
        let wz = zo as f32 / OCTIMETERS_PER_METER;
        let (y, shade) = surface(wx, wz);
        let light = wash_lightness(material, resolved.light, wx, wz, resolved.stroke);
        let light = (light * (1.0 - rim_darken) * shade).clamp(0.0, 100.0);
        let rgb = hsl_to_linear_rgb(resolved.hue, resolved.sat, light);
        Vertex {
            x: wx,
            y,
            z: wz,
            r: rgb[0],
            g: rgb[1],
            b: rgb[2],
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
    let place = GridPlacement {
        origin_oct,
        step_oct,
    };
    let rim_vertex = surface_vertex(world, OVERLAY_RIM_LIFT, rim_color);
    march_grid(&smoothed, gw, gh, &place, &rim_vertex, tris);
    let rim_width = (s.rim_inset_octimeters / step_oct).max(1);
    let eroded = erode(&smoothed, gw, gh, rim_width);

    if material == Material::Water {
        emit_water(world, &eroded, gw, apron, upsample, base_oct, tris);
        return;
    }

    let body_color = hsl_to_linear_rgb(s.base_hue, s.base_sat, s.base_light);
    let body_vertex = surface_vertex(world, OVERLAY_BODY_LIFT, body_color);
    march_grid(&eroded, gw, gh, &place, &body_vertex, tris);
}

/// A vertex builder for overlay geometry: one flat color, lifted `lift`
/// above the surface at the vertex's own (floor) cell — overlay contours
/// drape the terrain; a contour crossing a cliff line stretches down the
/// face rather than floating.
fn surface_vertex(world: &World, lift: f32, color: [f32; 3]) -> impl Fn(f32, f32) -> Vertex + '_ {
    move |wx: f32, wz: f32| Vertex {
        x: wx,
        y: point_lift(world, wx, wz).0 + lift,
        z: wz,
        r: color[0],
        g: color[1],
        b: color[2],
    }
}

/// Emit water's body: a graded quad per subcell whose sample block sits
/// fully inside the eroded smoothed grid, lightness falling with derived
/// shore depth and hue varying per subcell. A shoreline subcell only
/// partially inside refines to per-sample quads over its covered samples,
/// so the body hugs the smoothed contour at the smoothing grid's own
/// resolution and the marched rim band beneath shows as an even pooled
/// edge instead of subcell-scale teeth.
fn emit_water(
    world: &World,
    eroded: &[bool],
    gw: usize,
    apron: i32,
    upsample: usize,
    base_oct: [i32; 2],
    tris: &mut Vec<DrawTriangle>,
) {
    let u = upsample as i32;
    let sample = |gx: i32, gz: i32| {
        // The grid is square (n × n upsampled), so gw bounds both.
        gx >= 0
            && gz >= 0
            && (gx as usize) < gw
            && (gz as usize) < gw
            && eroded[gz as usize * gw + gx as usize]
    };
    let inside = |six: i32, siz: i32| {
        let gx0 = (six + apron) * u;
        let gz0 = (siz + apron) * u;
        (0..u).all(|dz| (0..u).all(|dx| sample(gx0 + dx, gz0 + dz)))
    };
    let step_oct = OCTIMETERS_PER_SUBCELL / u;
    for sj in 0..SUBCELLS_PER_CHUNK_EDGE {
        for si in 0..SUBCELLS_PER_CHUNK_EDGE {
            let x_oct = base_oct[0] + si * OCTIMETERS_PER_SUBCELL;
            let z_oct = base_oct[1] + sj * OCTIMETERS_PER_SUBCELL;
            if inside(si, sj) {
                let depth = subcell_shore_depth(&inside, si, sj);
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
                    &surface_vertex(world, OVERLAY_BODY_LIFT, color),
                );
                continue;
            }
            // Shoreline refinement: per-sample quads over the covered
            // samples of a partially-inside subcell, at the shore depth.
            let gx0 = (si + apron) * u;
            let gz0 = (sj + apron) * u;
            let mut any = false;
            for dz in 0..u {
                for dx in 0..u {
                    if sample(gx0 + dx, gz0 + dz) {
                        any = true;
                    }
                }
            }
            if !any {
                continue;
            }
            let center_x = (x_oct + OCTIMETERS_PER_SUBCELL / 2) as f32 / OCTIMETERS_PER_METER;
            let center_z = (z_oct + OCTIMETERS_PER_SUBCELL / 2) as f32 / OCTIMETERS_PER_METER;
            let resolved = resolve_cell(Material::Water, center_x, center_z, Some(0.0));
            let color = hsl_to_linear_rgb(resolved.hue, resolved.sat, resolved.light);
            let vertex = surface_vertex(world, OVERLAY_BODY_LIFT, color);
            for dz in 0..u {
                for dx in 0..u {
                    if sample(gx0 + dx, gz0 + dz) {
                        let sx = x_oct + dx * step_oct;
                        let sz = z_oct + dz * step_oct;
                        push_quad(tris, sx, sz, sx + step_oct, sz + step_oct, &vertex);
                    }
                }
            }
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

/// Emit the raw calibration view: one grayscale quad per non-Void
/// underlay cell on the cell's surface patch, its value the cell's own
/// hue-noise field (no slope shade — the gray is the calibration
/// instrument).
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
            let lift = CellLift::of(world, cell);
            let vertex = |wx: f32, wz: f32| Vertex {
                x: wx,
                y: lift.y(wx, wz),
                z: wz,
                r: v,
                g: v,
                b: v,
            };
            push_quad(tris, x0, z0, x0 + 256, z0 + 256, &vertex);
        }
    }
}

/// Emit the vertical cliff faces this chunk owns: for every local cell
/// standing strictly higher than an edge neighbor past the step ceiling,
/// a quad from the high plate corners down to the low ones. The high cell
/// owns the face, so a chunk-border cliff is emitted exactly once
/// fleet-wide. Faces wear the high cell's region cliff material
/// ([`World::cliff_material`]), darkened toward the base; the raw view
/// keeps them a flat gray so the terrain stays closed without polluting
/// the calibration field. Where the corner walk merged the two sides'
/// plates the gap tapers to nothing and the face is skipped.
fn emit_skirts(world: &World, at: ChunkPos, mode: ViewMode, tris: &mut Vec<DrawTriangle>) {
    // Per direction: neighbor offset, the shared edge's two lattice
    // corners as indices into the high cell's corner order, and the same
    // lattice points in the neighbor's order (the low side's plates).
    struct SkirtEdge {
        offset: (i32, i32),
        top: [usize; 2],
        bottom: [usize; 2],
    }
    const DIRECTIONS: [SkirtEdge; 4] = [
        SkirtEdge {
            offset: (1, 0),
            top: [1, 3],
            bottom: [0, 2],
        },
        SkirtEdge {
            offset: (-1, 0),
            top: [0, 2],
            bottom: [1, 3],
        },
        SkirtEdge {
            offset: (0, 1),
            top: [2, 3],
            bottom: [0, 1],
        },
        SkirtEdge {
            offset: (0, -1),
            top: [0, 1],
            bottom: [2, 3],
        },
    ];
    for lz in 0..EDGE {
        for lx in 0..EDGE {
            let cell = CellPos {
                x: at.x * EDGE + lx,
                z: at.z * EDGE + lz,
            };
            let cell_height = world.height(cell);
            let mut cached: Option<[f32; 4]> = None;
            for edge in &DIRECTIONS {
                let neighbor = CellPos {
                    x: cell.x + edge.offset.0,
                    z: cell.z + edge.offset.1,
                };
                if cell_height <= world.height(neighbor) || !world.edge_is_cliff(cell, neighbor) {
                    continue;
                }
                let top = *cached.get_or_insert_with(|| world.cell_corner_heights(cell));
                let bottom = world.cell_corner_heights(neighbor);
                let y_top = [top[edge.top[0]], top[edge.top[1]]];
                let y_bottom = [bottom[edge.bottom[0]], bottom[edge.bottom[1]]];
                if (y_top[0] - y_bottom[0]).abs() < f32::EPSILON
                    && (y_top[1] - y_bottom[1]).abs() < f32::EPSILON
                {
                    continue;
                }
                // Lattice-corner positions in meters from the index order
                // [(x, z), (x+1, z), (x, z+1), (x+1, z+1)].
                let corner_pos = |k: usize| {
                    (
                        cell.x as f32 + if k % 2 == 1 { 1.0 } else { 0.0 },
                        cell.z as f32 + if k >= 2 { 1.0 } else { 0.0 },
                    )
                };
                let (x0, z0) = corner_pos(edge.top[0]);
                let (x1, z1) = corner_pos(edge.top[1]);
                let (top_rgb, bottom_rgb) = match mode {
                    ViewMode::Raw => ([RAW_SKIRT_GRAY; 3], [RAW_SKIRT_GRAY; 3]),
                    ViewMode::Painted => {
                        let material = world.cliff_material(cell);
                        let resolved =
                            resolve_cell(material, cell.x as f32 + 0.5, cell.z as f32 + 0.5, None);
                        (
                            hsl_to_linear_rgb(
                                resolved.hue,
                                resolved.sat,
                                (resolved.light * SKIRT_TOP_SHADE).clamp(0.0, 100.0),
                            ),
                            hsl_to_linear_rgb(
                                resolved.hue,
                                resolved.sat,
                                (resolved.light * SKIRT_BASE_SHADE).clamp(0.0, 100.0),
                            ),
                        )
                    }
                };
                let vert = |x: f32, z: f32, y: f32, rgb: [f32; 3]| Vertex {
                    x,
                    y,
                    z,
                    r: rgb[0],
                    g: rgb[1],
                    b: rgb[2],
                };
                let a = vert(x0, z0, y_top[0], top_rgb);
                let b = vert(x1, z1, y_top[1], top_rgb);
                let c = vert(x1, z1, y_bottom[1], bottom_rgb);
                let d = vert(x0, z0, y_bottom[0], bottom_rgb);
                tris.push(DrawTriangle { verts: [a, b, c] });
                tris.push(DrawTriangle { verts: [a, c, d] });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::{CELLS_PER_CHUNK_AREA, Chunk, Region, SmoothingProfile};
    use style::ResolvedCell;

    fn grass_cell() -> ResolvedCell {
        ResolvedCell {
            hue: 110.0,
            sat: 37.5,
            light: 40.0,
            stroke: (1.0, 0.0),
        }
    }

    /// A level surface patch for cell `(cx, cz)` — the flat-world case.
    fn flat_lift(cx: i32, cz: i32) -> CellLift {
        CellLift {
            x0: cx as f32,
            z0: cz as f32,
            corners: [0.0; 4],
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
        emit_underlay_cell(
            Material::Grass,
            &grass_cell(),
            3,
            5,
            [0.0; 4],
            &flat_lift(3, 5),
            &mut tris,
        );
        assert_eq!(tris.len(), 2, "no rims is one flat quad");
    }

    #[test]
    fn rimmed_cell_is_a_nine_slice_inside_its_bounds() {
        // A fully-rimmed cell emits the nine-slice (eighteen triangles), and
        // every vertex stays inside the cell — the rim only pools inward, it
        // never invents geometry outside the cell.
        let mut tris = Vec::new();
        emit_underlay_cell(
            Material::Grass,
            &grass_cell(),
            3,
            5,
            [1.0; 4],
            &flat_lift(3, 5),
            &mut tris,
        );
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

        let left_cell_right_rim = cell_rims(&world, CellPos { x: 15, z: 4 }, Material::Grass)[1];
        let right_cell_left_rim = cell_rims(&world, CellPos { x: 16, z: 4 }, Material::Grass)[0];
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

    /// Two chunks of grass-default region with an explicit sand band over
    /// cells `10..22 × 4..8` (crossing the chunk border at `x = 16`).
    /// `profile` paints every cell's smoothing plane with the given
    /// profile-1 settings; `None` leaves the material defaults governing.
    fn sand_band_world(profile: Option<SmoothingProfile>) -> World {
        let mut world = World::new();
        world.insert_region(
            1,
            Region {
                name: "meadow".into(),
                default_material: Material::Grass,
                cliff_material: Material::Stone,
            },
        );
        if let Some(p) = profile {
            world.insert_smoothing_profile(1, p);
        }
        for cx in 0..2 {
            let mut chunk = Chunk::empty();
            chunk.region = [1; CELLS_PER_CHUNK_AREA];
            if profile.is_some() {
                chunk.smoothing = [1; CELLS_PER_CHUNK_AREA];
            }
            for lz in 4..8 {
                for lx in 0..EDGE {
                    if (10..22).contains(&(cx * EDGE + lx)) {
                        chunk.underlay[(lz * EDGE + lx) as usize] = Material::Sand;
                    }
                }
            }
            world.insert_chunk(ChunkPos { x: cx, z: 0 }, chunk);
        }
        world
    }

    /// Does the underlay geometry carry a smoothed-crossing vertex? A
    /// marched crossing sits at a window midpoint (x on the 32-oct
    /// lattice); crisp cell-mask crossings land only on cell lines (0 mod
    /// 256) and the nine-slice insets only at ±32 mod 256, so an x-residue
    /// in {64, 96, 128, 160, 192} exists exactly when smoothing moved a
    /// boundary off the cell lattice.
    fn has_smoothed_crossing(mesh: &[DrawTriangle]) -> bool {
        mesh.iter()
            .filter(|t| t.verts.iter().all(|v| v.y == 0.0))
            .flat_map(|t| t.verts.iter())
            .any(|v| {
                let oct = v.x * 256.0;
                let rounded = oct.round();
                if (oct - rounded).abs() > 0.01 {
                    return false;
                }
                let oct = rounded as i64;
                oct % 32 == 0 && matches!(oct.rem_euclid(256), 64 | 96 | 128 | 160 | 192)
            })
    }

    /// Does `t` (projected to the ground plane) cover point `(px, pz)`?
    fn covers(t: &DrawTriangle, px: f32, pz: f32) -> bool {
        let sign = |ax: f32, az: f32, bx: f32, bz: f32| {
            (ax - bx).mul_add(-(pz - bz), (px - bx) * (az - bz))
        };
        let d1 = sign(t.verts[0].x, t.verts[0].z, t.verts[1].x, t.verts[1].z);
        let d2 = sign(t.verts[1].x, t.verts[1].z, t.verts[2].x, t.verts[2].z);
        let d3 = sign(t.verts[2].x, t.verts[2].z, t.verts[0].x, t.verts[0].z);
        let has_neg = d1 < -1e-6 || d2 < -1e-6 || d3 < -1e-6;
        let has_pos = d1 > 1e-6 || d2 > 1e-6 || d3 > 1e-6;
        !(has_neg && has_pos)
    }

    #[test]
    fn partition_boundary_smooths_and_the_field_governs_it() {
        // The sand-grass boundary repartitions under the smoothing rules:
        // with a strong field profile the marched partition must put
        // crossings off the cell lattice, and a zero-iteration profile on
        // the same world must put none anywhere — the field governs
        // underlay boundaries exactly as it governs overlay contours.
        let smoothed = mesh_chunk(
            &sand_band_world(Some(SmoothingProfile {
                iterations: 4,
                degrees: 45,
            })),
            ChunkPos { x: 0, z: 0 },
            ViewMode::Painted,
        );
        assert!(
            has_smoothed_crossing(&smoothed),
            "a strong field profile moves the boundary off the cell lattice",
        );
        let crisp = mesh_chunk(
            &sand_band_world(Some(SmoothingProfile {
                iterations: 0,
                degrees: 90,
            })),
            ChunkPos { x: 0, z: 0 },
            ViewMode::Painted,
        );
        assert!(
            !has_smoothed_crossing(&crisp),
            "a zero-iteration field zone keeps the raw cell staircase",
        );
    }

    #[test]
    fn partition_ground_has_no_gaps() {
        // The partition must tile the painted ground exactly — every probe
        // point strictly inside the chunk is covered by at least one
        // ground-plane triangle. A saddle-rule or window-skip bug shows up
        // as a hole here.
        let world = sand_band_world(None);
        let mesh = mesh_chunk(&world, ChunkPos { x: 0, z: 0 }, ViewMode::Painted);
        let ground: Vec<&DrawTriangle> = mesh
            .iter()
            .filter(|t| t.verts.iter().all(|v| v.y == 0.0))
            .collect();
        for j in 0..32 {
            for i in 0..32 {
                let px = (i as f32 + 0.37) * 0.5;
                let pz = (j as f32 + 0.53) * 0.5;
                assert!(
                    ground.iter().any(|t| covers(t, px, pz)),
                    "ground hole at ({px}, {pz})",
                );
            }
        }
    }

    #[test]
    fn partition_chunks_tile_the_seam_together() {
        // Every boundary window is owned by exactly one chunk (the one
        // holding its center cell), so neither mesh alone covers the whole
        // seam strip — but their union must, with no gap. An ownership or
        // classification disagreement between the two chunks shows up as a
        // hole here.
        let world = sand_band_world(None);
        let mesh0 = mesh_chunk(&world, ChunkPos { x: 0, z: 0 }, ViewMode::Painted);
        let mesh1 = mesh_chunk(&world, ChunkPos { x: 1, z: 0 }, ViewMode::Painted);
        let ground_covers = |mesh: &[DrawTriangle], px: f32, pz: f32| {
            mesh.iter()
                .filter(|t| t.verts.iter().all(|v| v.y == 0.0))
                .any(|t| covers(t, px, pz))
        };
        for j in 0..64 {
            let pz = (j as f32).mul_add(0.25, 0.13);
            for i in 0..16 {
                let px = (i as f32).mul_add(0.125, 15.06);
                assert!(
                    ground_covers(&mesh0, px, pz) || ground_covers(&mesh1, px, pz),
                    "seam hole at ({px}, {pz})",
                );
            }
        }
    }

    #[test]
    fn region_default_boundary_smooths_too() {
        // Two regions with different default materials and no explicit
        // paint anywhere: the partition smooths the boundary all the same —
        // it never mattered why the materials differ.
        let mut world = World::new();
        world.insert_region(
            1,
            Region {
                name: "meadow".into(),
                default_material: Material::Grass,
                cliff_material: Material::Stone,
            },
        );
        world.insert_region(
            2,
            Region {
                name: "shore".into(),
                default_material: Material::Sand,
                cliff_material: Material::Stone,
            },
        );
        let mut chunk = Chunk::empty();
        for lz in 0..EDGE {
            for lx in 0..EDGE {
                // A diagonal-ish region boundary so corners exist to smooth.
                chunk.region[(lz * EDGE + lx) as usize] = if lx + lz / 2 < 10 { 1 } else { 2 };
            }
        }
        world.insert_chunk(ChunkPos { x: 0, z: 0 }, chunk);
        let mesh = mesh_chunk(&world, ChunkPos { x: 0, z: 0 }, ViewMode::Painted);
        assert!(
            has_smoothed_crossing(&mesh),
            "a region-default boundary smooths like any material boundary",
        );
    }

    /// The sand-band world over a gentle eastward ramp — heights step 8
    /// octimeters per cell, capped at the step ceiling so nothing cliffs
    /// anywhere (a 64-octimeter drop against the unloaded zero-height
    /// surroundings is a legal step, not a cliff).
    fn ramp_world() -> World {
        let mut world = sand_band_world(None);
        for cx in 0..2 {
            let at = ChunkPos { x: cx, z: 0 };
            let mut chunk = world.chunk(at).expect("chunk").clone();
            for lz in 0..EDGE {
                for lx in 0..EDGE {
                    let global_x = cx * EDGE + lx;
                    chunk.height[(lz * EDGE + lx) as usize] =
                        (8 * global_x).min(STEP_MAX_OCTIMETERS);
                }
            }
            world.insert_chunk(at, chunk);
        }
        world
    }

    #[test]
    fn drawn_ground_equals_stood_on_height() {
        // The drawn-equals-stood-on tripwire: on a cliff-free ramp every
        // vertex sits exactly on `World::surface_height` — the same patch
        // a mover will stand on. Any lift path that misses a vertex
        // (nine-slice, strip, window poly) diverges here.
        let world = ramp_world();
        let mesh = mesh_chunk(&world, ChunkPos { x: 0, z: 0 }, ViewMode::Painted);
        assert!(!mesh.is_empty());
        for v in mesh.iter().flat_map(|t| t.verts.iter()) {
            let surface = world.surface_height(v.x, v.z);
            assert!(
                (v.y - surface).abs() < 1e-4,
                "vertex ({}, {}) drawn at {} but stood on {surface}",
                v.x,
                v.z,
                v.y,
            );
        }
    }

    #[test]
    fn ramp_ground_still_has_no_gaps() {
        // The no-gap probe on sloped ground: the strip fallback and the
        // per-window bilinear quads must tile exactly like the flat path.
        let world = ramp_world();
        let mesh = mesh_chunk(&world, ChunkPos { x: 0, z: 0 }, ViewMode::Painted);
        for j in 0..32 {
            for i in 0..32 {
                let px = (i as f32 + 0.37) * 0.5;
                let pz = (j as f32 + 0.53) * 0.5;
                assert!(
                    mesh.iter().any(|t| covers(t, px, pz)),
                    "ground hole at ({px}, {pz})",
                );
            }
        }
    }

    #[test]
    fn a_cliff_emits_its_skirt_from_the_high_chunk() {
        // A plateau chunk one meter up beside a ground chunk: the cliff
        // face on the shared border belongs to the high cell, so the high
        // chunk emits it exactly once and the low chunk never does —
        // double emission would z-fight, omission would leave a window
        // through the terrain.
        let mut world = World::new();
        let mut low = Chunk::empty();
        low.underlay = [Material::Grass; CELLS_PER_CHUNK_AREA];
        world.insert_chunk(ChunkPos { x: 0, z: 0 }, low);
        let mut high = Chunk::empty();
        high.underlay = [Material::Grass; CELLS_PER_CHUNK_AREA];
        high.height = [256; CELLS_PER_CHUNK_AREA];
        world.insert_chunk(ChunkPos { x: 1, z: 0 }, high);

        let is_border_skirt = |t: &DrawTriangle| {
            t.verts.iter().all(|v| (v.x - 16.0).abs() < 1e-6)
                && t.verts.iter().any(|v| v.y > 0.9)
                && t.verts.iter().any(|v| v.y < 0.1)
        };
        let low_mesh = mesh_chunk(&world, ChunkPos { x: 0, z: 0 }, ViewMode::Painted);
        let high_mesh = mesh_chunk(&world, ChunkPos { x: 1, z: 0 }, ViewMode::Painted);
        assert!(
            high_mesh.iter().any(is_border_skirt),
            "the high chunk closes its cliff face",
        );
        assert!(
            !low_mesh.iter().any(is_border_skirt),
            "the low chunk does not double-draw the shared face",
        );
    }

    #[test]
    fn a_material_boundary_at_a_cliff_does_not_drape() {
        // Sand plateau (1 m up) against grass ground, material boundary on
        // the cliff line: every colored (non-skirt) triangle must stay on
        // its own side of the break — the cliff face belongs to the gray
        // skirt. Position-pure lifting would stretch the boundary windows
        // into meter-tall sand and grass walls down the face.
        let mut world = World::new();
        let mut low = Chunk::empty();
        low.underlay = [Material::Grass; CELLS_PER_CHUNK_AREA];
        let mut high = Chunk::empty();
        high.underlay = [Material::Sand; CELLS_PER_CHUNK_AREA];
        high.height = [256; CELLS_PER_CHUNK_AREA];
        world.insert_chunk(ChunkPos { x: 0, z: 0 }, low);
        world.insert_chunk(ChunkPos { x: 1, z: 0 }, high);

        for at in [ChunkPos { x: 0, z: 0 }, ChunkPos { x: 1, z: 0 }] {
            let mesh = mesh_chunk(&world, at, ViewMode::Painted);
            for t in &mesh {
                let gray = t
                    .verts
                    .iter()
                    .all(|v| (v.r - v.b).abs() < 0.05 && (v.g - v.b).abs() < 0.05);
                if gray {
                    continue; // skirts own the vertical face
                }
                let max_y = t.verts.iter().map(|v| v.y).fold(f32::MIN, f32::max);
                let min_y = t.verts.iter().map(|v| v.y).fold(f32::MAX, f32::min);
                assert!(
                    max_y - min_y < 0.5,
                    "chunk {at:?}: a colored triangle drapes the cliff (span {} at x {})",
                    max_y - min_y,
                    t.verts[0].x,
                );
            }
        }
    }

    #[test]
    fn overlay_rides_the_surface_at_its_lift() {
        // A stone overlay on the ramp keeps its one- and two-octimeter
        // lifts measured from the surface, not from `y = 0` — the drape
        // rule for overlays over sloped ground.
        let mut world = ramp_world();
        let at = ChunkPos { x: 0, z: 0 };
        let mut chunk = world.chunk(at).expect("chunk").clone();
        for lz in 5..9 {
            for lx in 2..6 {
                chunk.overlay[(lz * EDGE + lx) as usize] = Material::Stone;
                chunk.overlay_mask[(lz * EDGE + lx) as usize] = 0xFFFF;
            }
        }
        world.insert_chunk(at, chunk);
        let mesh = mesh_chunk(&world, at, ViewMode::Painted);
        let mut overlay_verts = 0;
        for v in mesh.iter().flat_map(|t| t.verts.iter()) {
            let lift = v.y - world.surface_height(v.x, v.z);
            if lift > 1e-5 {
                overlay_verts += 1;
                assert!(
                    (lift - OVERLAY_RIM_LIFT).abs() < 1e-4
                        || (lift - OVERLAY_BODY_LIFT).abs() < 1e-4,
                    "overlay lift {lift} is neither rim nor body at ({}, {})",
                    v.x,
                    v.z,
                );
            }
        }
        assert!(overlay_verts > 0, "the overlay emitted");
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
