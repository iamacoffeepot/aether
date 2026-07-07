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
//! width. Every overlay material contours identically — water is underlay
//! ground fabric (below), so an overlay-painted water body draws as a
//! generic marched surface with no special treatment. Overlay geometry
//! lifts a hair above the underlay surface so the coplanar passes never
//! z-fight, and it carries its own vertices so the seam stays hard.
//!
//! # Water — a flat underlay plane
//!
//! [`Material::Water`] is an underlay partition material, so the waterline
//! smooths, rims, and tiles through the underlay repartition like every
//! other boundary. What makes it read as water is the surface level: a
//! water cell resolves its corners at its authored water-plane level
//! ([`World::water_level`]) rather than its lakebed [`World::height`], and a
//! corner plate with any water member pins to that level — so the surface
//! is exactly flat, a connected shore blends down to the waterline (beach),
//! and a past-step bank splits and wears the wall down to the water. An
//! interior water cell tiles the flat patch with a depth-graded quad per
//! subcell, depth derived as the plane level minus the bilinear lakebed
//! [`World::height`] over `WATER_DEPTH_FULL_OCTIMETERS`.
//!
//! # Height pass — plates, slopes, and walls
//!
//! Every vertex lifts onto the plate-resolved height surface
//! ([`World::surface_height_in`]): cell heights blend into continuous
//! slopes where neighbors sit within the step ceiling
//! ([`crate::world::STEP_MAX_OCTIMETERS`]) and break where they exceed
//! it — the corner plates split exactly on cliff edges, an interior
//! cell's owner-pinned patch lands the break on the cell line, and the
//! wall pass closes that gap with a vertical face wearing the high cell's
//! region cliff material, darkened toward its base. The walls stitch from
//! the same repartitioned sample grid the caps march, as the union of two
//! segment classes over one pass: a material or Void boundary standing past
//! the step ceiling lofts its marched contour down as a curtain — the
//! wall's top vertices are the cap contour's own vertices lifted through
//! the same owner-clamped patch, so the seam is watertight by construction
//! rather than by stitching two independently derived lines — while a
//! same-material cliff, which the material partition leaves no boundary to
//! follow, lofts the cell-edge lattice line the owner-pinned patches
//! already break on. Where the low side is a Void hole with no ground the
//! curtain drops a fixed depth so the hole reads as thick ground rather
//! than a paper lip. Boundary
//! windows and overlay contours lift each vertex through its own
//! (floor) cell — continuous wherever no cliff intervenes — and a water
//! cell reads its flat authored plane level (the water section above)
//! rather than the lakebed, so a lake on sloped terrain lies flat and a
//! bank above it wears the wall down to the water. A slope shade from the
//! patch normal bakes into vertex
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
    ResolvedCell, StyleTable, fbm, hsl_to_linear_rgb, raw_field, resolve_cell, rim_strength,
    wash_lightness,
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

/// The raw calibration view's flat gray for cliff faces — a wall has no
/// noise field to calibrate; it only keeps the terrain closed.
const RAW_SKIRT_GRAY: f32 = 0.35;

/// How far in octimeters a wall's base tucks under the low side's surface,
/// so the low cap covers the seam with no crack.
const WALL_TUCK_OCTIMETERS: i32 = 8;

/// How far in octimeters a wall drops below its top edge where the low side
/// is a Void hole with no ground — a hole reads as thick ground rather than
/// a paper-thin lip.
const WALL_VOID_DROP_OCTIMETERS: i32 = 512;

/// The overlay rim layer lift — two octimeters over the underlay, one
/// above the encroachment flap so an overlay contour always draws over a
/// material margin that grew across the same seam.
const OVERLAY_RIM_LIFT: f32 = 2.0 / OCTIMETERS_PER_METER;

/// The overlay body layer lift — one octimeter over the rim, so the body
/// sits on top of the rim it insets from.
const OVERLAY_BODY_LIFT: f32 = 3.0 / OCTIMETERS_PER_METER;

/// The encroachment flap layer lift — one octimeter over the underlay.
/// The lower material's geometry is untouched at `y = 0`; the higher-rank
/// material's noise-ragged margin rides just above it, reading as growth
/// over the seam (grass over a dirt path, sand over a waterline) rather
/// than a hard partition line.
const ENCROACH_LIFT: f32 = 1.0 / OCTIMETERS_PER_METER;

/// Base seed for the encroachment margin's raggedness noise, distinct from
/// the color-field seeds so the ragged edge decorrelates from the quilt.
const SEED_ENCROACH: u32 = 130_077;

/// The raggedness noise wavelength in cells — a little over a cell, so the
/// margin waves in and out roughly once per cell of seam.
const ENCROACH_NOISE_WAVELENGTH: f32 = 1.3;

/// Octave count for the raggedness noise — two octaves give the margin a
/// coarse wave plus a finer fray without a heavy fractal.
const ENCROACH_NOISE_OCTAVES: u32 = 2;

/// Per-octave amplitude falloff for the raggedness noise.
const ENCROACH_NOISE_PERSISTENCE: f32 = 0.5;

/// Upsample factor for a non-water material's smoothed contour grid.
const CONTOUR_UPSAMPLE: usize = 2;

/// Apron cap in subcells (two cells) so a chunk's smoothing reads stay
/// within the eight-neighbor remesh the `R = 1` invalidation covers.
const MAX_APRON_SUBCELLS: i32 = 8;

/// Water depth in octimeters at which the depth grading reaches full
/// darkening — one meter of water below the surface. Shallower water grades
/// proportionally between the shore (depth `0`) and this floor; deeper
/// water clamps here.
const WATER_DEPTH_FULL_OCTIMETERS: f32 = 256.0;

/// Mesh one chunk into its triangle list. Pure — no wgpu, no ctx — so it
/// is unit-testable host-side. Reads neighbor cells through [`World`] (a
/// bounded apron); a missing neighbor reads as empty. `mode` selects the
/// painted gouache grammar or the raw grayscale calibration field.
/// `styles` resolves each material's live style row.
#[must_use]
pub fn mesh_chunk(
    world: &World,
    at: ChunkPos,
    mode: ViewMode,
    styles: &StyleTable,
) -> Vec<DrawTriangle> {
    let mut tris = Vec::new();
    match mode {
        ViewMode::Raw => {
            mesh_raw(world, at, styles, &mut tris);
            emit_walls(world, at, mode, styles, None, &mut tris);
        }
        ViewMode::Painted => {
            let partition = mesh_underlay(world, at, styles, &mut tris);
            mesh_overlay(world, at, styles, &mut tris);
            emit_walls(world, at, mode, styles, partition.as_ref(), &mut tris);
        }
    }
    tris
}

/// The repartitioned material grid the underlay pass marches its caps from,
/// handed to the wall pass so it lofts its curtains from the same samples —
/// the shared grid is what makes a wall top land exactly on a cap contour
/// vertex. Carries the display labels, the grid width, and the octimeter
/// placement (apron offset and sample step) the boundary walk reconstructs
/// window positions from.
struct DisplayPartition {
    display: Vec<u8>,
    gw: usize,
    apron: i32,
    step_oct: i32,
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
    if cell != owner && world.edge_is_cliff(cell, owner) {
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
fn cell_rims(world: &World, cell: CellPos, material: Material, styles: &StyleTable) -> [f32; 4] {
    let hue_a = resolve_cell(
        styles.get(material),
        cell.x as f32 + 0.5,
        cell.z as f32 + 0.5,
        None,
    )
    .hue;
    let threshold = styles.get(material).blob_merge_degrees;
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
        let hue_b = resolve_cell(
            styles.get(neighbor),
            *nx as f32 + 0.5,
            *nz as f32 + 0.5,
            None,
        )
        .hue;
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
    styles: &StyleTable,
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
            // Sample the cell's authored point, not the whole-cell material:
            // an all-inherit point folds back to `World::underlay(cell)`, so
            // an unshaped world is unchanged, while an authored point moves
            // the silhouette below cell scale.
            let material = world.underlay_point(cell, si.rem_euclid(SUB), sj.rem_euclid(SUB));
            let idx = (sj + apron) as usize * n + (si + apron) as usize;
            ids[idx] = material.to_u8();
            any |= material != Material::Void;
            params[idx] = world.smoothing_override(cell).map_or_else(
                || {
                    let s = styles.get(material);
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
/// geometry over their shared apron and the overlap is invisible. Returns
/// the split display grid so the wall pass lofts from the same samples;
/// `None` when the whole chunk plus apron is Void (nothing to mesh).
fn mesh_underlay(
    world: &World,
    at: ChunkPos,
    styles: &StyleTable,
    tris: &mut Vec<DrawTriangle>,
) -> Option<DisplayPartition> {
    let apron = MAX_APRON_SUBCELLS;
    let n = (SUBCELLS_PER_CHUNK_EDGE + 2 * apron) as usize;
    let (ids, params) = partition_inputs(world, at, apron, n, styles)?;

    let upsample = CONTOUR_UPSAMPLE;
    let (grid, gw, gh) = repartition(&ids, n, n, upsample, &params);
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
        let rim_width = (styles
            .get(Material::from_u8_or_void(m))
            .rim_inset_octimeters
            / step_oct)
            .max(1);
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
            if material == Material::Water {
                emit_water_cell(world, cell, styles, tris);
                continue;
            }
            let resolved = resolve_cell(
                styles.get(material),
                cell.x as f32 + 0.5,
                cell.z as f32 + 0.5,
                None,
            );
            let rims = cell_rims(world, cell, material, styles);
            let lift = CellLift::of(world, cell);
            emit_underlay_cell(
                material, &resolved, cell.x, cell.z, rims, &lift, styles, tris,
            );
        }
    }

    emit_partition_windows(
        world, at, &display, gw, apron, step_oct, &interior, lo, styles, tris,
    );

    let place = chunk_placement(at, apron, step_oct);
    emit_encroach_flaps(world, at, &grid, gw, gh, &place, styles, tris);

    Some(DisplayPartition {
        display,
        gw,
        apron,
        step_oct,
    })
}

/// Where a chunk's partition grid lands in the world: sample `(0, 0)`'s
/// octimeter center, offset back by the apron and forward half a step so
/// each sample sits at a subcell-lattice center, at `step_oct` spacing.
/// Both the partition-window and encroachment-flap passes place their
/// marches through this, so they agree on the lattice sample-for-sample.
fn chunk_placement(at: ChunkPos, apron: i32, step_oct: i32) -> GridPlacement {
    let base_oct = [
        at.x * SUBCELLS_PER_CHUNK_EDGE * OCTIMETERS_PER_SUBCELL,
        at.z * SUBCELLS_PER_CHUNK_EDGE * OCTIMETERS_PER_SUBCELL,
    ];
    GridPlacement {
        origin_oct: [
            base_oct[0] - apron * OCTIMETERS_PER_SUBCELL + step_oct / 2,
            base_oct[1] - apron * OCTIMETERS_PER_SUBCELL + step_oct / 2,
        ],
        step_oct,
    }
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
            emit_mixed_window(
                world, display, gw, wi, wj, corners, owner, &place, styles, tris,
            );
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
    styles: &StyleTable,
    tris: &mut Vec<DrawTriangle>,
) {
    let material = Material::from_u8_or_void(label.div_ceil(2));
    let depth = (material == Material::Water).then(|| owner_water_depth(world, owner));
    let resolved = resolve_cell(
        styles.get(material),
        owner.x as f32 + 0.5,
        owner.z as f32 + 0.5,
        depth,
    );
    let rim = if label % 2 == 1 {
        styles.get(material).rim_darken
    } else {
        0.0
    };
    let surface = |wx: f32, wz: f32| label_lift(world, owner, wx, wz);
    emit_quad_shaded(material, &resolved, rect, rim, &surface, styles, tris);
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
    styles: &StyleTable,
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
        let s = styles.get(material);
        let depth = (material == Material::Water).then(|| owner_water_depth(world, owner));
        let resolved = resolve_cell(s, owner.x as f32 + 0.5, owner.z as f32 + 0.5, depth);
        let mut light = wash_lightness(s, resolved.light, wash_x, wash_z, resolved.stroke);
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

/// The world cell under grid sample `(gx, gz)` of a partition grid placed
/// by `place`: the sample sits at `origin_oct + idx * step_oct` octimeters
/// and the cell is that position floored by the 256-octimeter cell size.
fn sample_cell(place: &GridPlacement, gx: i32, gz: i32) -> CellPos {
    CellPos {
        x: (place.origin_oct[0] + gx * place.step_oct).div_euclid(256),
        z: (place.origin_oct[1] + gz * place.step_oct).div_euclid(256),
    }
}

/// Barrier-respecting step distance from `encroacher`'s samples to every
/// sample of the repartitioned label grid, in orthogonal grid steps (each
/// `place.step_oct` octimeters). A bounded multi-source breadth-first flood
/// off every `encroacher` sample: because each step costs one, the first
/// time the flood reaches a sample is its exact geodesic distance. A step
/// between two samples is refused when the world cells under them stand a
/// cliff apart ([`World::edge_is_cliff`]), so the margin grows *around* a
/// cliff line rather than pouring down the face — the same break the skirt
/// owns. The flood stops at `max_steps` (the reach ceiling in steps), so it
/// stays inside the chunk's apron and cheap; a sample the flood never
/// reaches within the cap reads [`i32::MAX`].
fn encroach_distance(
    world: &World,
    grid: &[u8],
    gw: usize,
    place: &GridPlacement,
    encroacher: u8,
    max_steps: i32,
) -> Vec<i32> {
    let mut dist = vec![i32::MAX; grid.len()];
    let mut frontier: Vec<usize> = Vec::new();
    for (idx, &m) in grid.iter().enumerate() {
        if m == encroacher {
            dist[idx] = 0;
            frontier.push(idx);
        }
    }
    let gwi = gw as i32;
    let gh = (grid.len() / gw) as i32;
    let mut step = 0;
    while step < max_steps && !frontier.is_empty() {
        step += 1;
        let mut next: Vec<usize> = Vec::new();
        for &idx in &frontier {
            let gx = (idx % gw) as i32;
            let gz = (idx / gw) as i32;
            let here = sample_cell(place, gx, gz);
            for (dx, dz) in [(-1, 0), (1, 0), (0, -1), (0, 1)] {
                let nx = gx + dx;
                let nz = gz + dz;
                if nx < 0 || nz < 0 || nx >= gwi || nz >= gh {
                    continue;
                }
                let nidx = nz as usize * gw + nx as usize;
                if dist[nidx] <= step {
                    continue;
                }
                let there = sample_cell(place, nx, nz);
                if here != there && world.edge_is_cliff(here, there) {
                    continue;
                }
                dist[nidx] = step;
                next.push(nidx);
            }
        }
        frontier = next;
    }
    dist
}

/// The encroachment mask for one `encroacher` material over the
/// repartitioned label grid: `true` at every sample whose own material has
/// strictly lower [`encroach_rank`](style::MaterialStyle::encroach_rank)
/// and whose barrier-respecting distance from `encroacher` is within the
/// noise-modulated reach `reach_octimeters × (1 − raggedness × noise)`. The
/// noise is a world-anchored [`fbm`] sample (mapped to `[0, 1]` so the
/// raggedness only ever eats *into* the reach, never past it), so two
/// chunks resolve the same sample identically and the margin agrees over
/// their shared border. Void samples are never encroached — an empty cell
/// has no ground for a flap to lie on.
fn encroach_mask(
    world: &World,
    grid: &[u8],
    gw: usize,
    gh: usize,
    place: &GridPlacement,
    encroacher: Material,
    styles: &StyleTable,
) -> Vec<bool> {
    let s = styles.get(encroacher);
    let rank = s.encroach_rank;
    let reach_octimeters = s.encroach_reach_octimeters;
    let raggedness = s.encroach_raggedness;
    let max_steps = reach_octimeters / place.step_oct;
    let dist = encroach_distance(world, grid, gw, place, encroacher.to_u8(), max_steps);
    let mut mask = vec![false; grid.len()];
    for gz in 0..gh {
        for gx in 0..gw {
            let idx = gz * gw + gx;
            let d = dist[idx];
            if d == i32::MAX {
                continue;
            }
            let sample = grid[idx];
            if sample == 0 {
                continue;
            }
            if styles.get(Material::from_u8_or_void(sample)).encroach_rank >= rank {
                continue;
            }
            let wx =
                (place.origin_oct[0] + gx as i32 * place.step_oct) as f32 / OCTIMETERS_PER_METER;
            let wz =
                (place.origin_oct[1] + gz as i32 * place.step_oct) as f32 / OCTIMETERS_PER_METER;
            let noise = fbm(
                wx,
                wz,
                SEED_ENCROACH.wrapping_add(s.seed_offset),
                ENCROACH_NOISE_OCTAVES,
                ENCROACH_NOISE_WAVELENGTH,
                ENCROACH_NOISE_PERSISTENCE,
            );
            let noise01 = (noise * 0.5 + 0.5).clamp(0.0, 1.0);
            let reach = reach_octimeters as f32 * (1.0 - raggedness * noise01);
            if (d * place.step_oct) as f32 <= reach {
                mask[idx] = true;
            }
        }
    }
    mask
}

/// Emit the encroachment flap layer: for each material present in the grid
/// that can encroach (rank and reach both positive), build its mask and
/// march it as a marched surface one octimeter above the untouched
/// underlay. Materials draw lowest-rank first so a higher-rank flap wins
/// the depth test wherever two margins overlap the same lower sample. The
/// whole layer only runs in the painted underlay pass, so the raw
/// calibration view (a noise-field instrument) never carries it.
#[allow(clippy::too_many_arguments)] // one call site; the flap state travels together
fn emit_encroach_flaps(
    world: &World,
    at: ChunkPos,
    grid: &[u8],
    gw: usize,
    gh: usize,
    place: &GridPlacement,
    styles: &StyleTable,
    tris: &mut Vec<DrawTriangle>,
) {
    let mut present = [false; 6];
    for &m in grid {
        present[m as usize] = true;
    }
    let mut encroachers: Vec<Material> = (1..6u8)
        .filter(|&m| present[m as usize])
        .map(Material::from_u8_or_void)
        .filter(|&mat| {
            let s = styles.get(mat);
            s.encroach_rank > 0 && s.encroach_reach_octimeters > 0
        })
        .collect();
    encroachers.sort_by_key(|&mat| styles.get(mat).encroach_rank);
    for encroacher in encroachers {
        let mask = encroach_mask(world, grid, gw, gh, place, encroacher, styles);
        emit_encroach_windows(world, at, &mask, gw, gh, place, encroacher, styles, tris);
    }
}

/// March one encroacher's mask into flap polygons under the same
/// window-center ownership rule as [`emit_partition_windows`]: a window is
/// emitted only by the chunk holding the cell under its center, so the
/// layer is emitted once fleet-wide against the per-frame vertex budget.
/// Each window's polygon is colored as the encroacher keyed at its owner
/// cell (the [`emit_mixed_window`] convention) and every vertex lifts
/// through [`label_lift`] plus [`ENCROACH_LIFT`] — so a flap at a cliff top
/// clamps to the high side as a small overhanging lip, and a flap over a
/// water cell rides the flat authored plane level rather than the lakebed.
#[allow(clippy::too_many_arguments)] // one call site; the flap state travels together
fn emit_encroach_windows(
    world: &World,
    at: ChunkPos,
    mask: &[bool],
    gw: usize,
    gh: usize,
    place: &GridPlacement,
    encroacher: Material,
    styles: &StyleTable,
    tris: &mut Vec<DrawTriangle>,
) {
    let step_oct = place.step_oct;
    let s = styles.get(encroacher);
    for wj in 0..gh - 1 {
        for wi in 0..gw - 1 {
            let bl = mask[wj * gw + wi];
            let br = mask[wj * gw + wi + 1];
            let tl = mask[(wj + 1) * gw + wi];
            let tr = mask[(wj + 1) * gw + wi + 1];
            let case = u8::from(bl) | u8::from(br) << 1 | u8::from(tr) << 2 | u8::from(tl) << 3;
            if case == 0 {
                continue;
            }
            let x_lo = place.origin_oct[0] + wi as i32 * step_oct;
            let z_lo = place.origin_oct[1] + wj as i32 * step_oct;
            let x_center_cell = (x_lo + step_oct / 2).div_euclid(256);
            let z_center_cell = (z_lo + step_oct / 2).div_euclid(256);
            let x_owner_local = x_center_cell - at.x * EDGE;
            let z_owner_local = z_center_cell - at.z * EDGE;
            if !(0..EDGE).contains(&x_owner_local) || !(0..EDGE).contains(&z_owner_local) {
                continue;
            }
            let owner = CellPos {
                x: x_center_cell,
                z: z_center_cell,
            };
            let resolved = resolve_cell(s, owner.x as f32 + 0.5, owner.z as f32 + 0.5, None);
            let vertex = |wx: f32, wz: f32| {
                let (y, shade) = label_lift(world, owner, wx, wz);
                let light = wash_lightness(s, resolved.light, wx, wz, resolved.stroke);
                let rgb = hsl_to_linear_rgb(
                    resolved.hue,
                    resolved.sat,
                    (light * shade).clamp(0.0, 100.0),
                );
                Vertex {
                    x: wx,
                    y: y + ENCROACH_LIFT,
                    z: wz,
                    r: rgb[0],
                    g: rgb[1],
                    b: rgb[2],
                }
            };
            emit_label_window(wi as i32, wj as i32, place, case, true, &vertex, tris);
        }
    }
}

/// Emit one cell's geometry: a single flat quad when no side rims, else a
/// nine-slice whose edge strips and corners darken by the pooled-rim
/// factor. All geometry stays inside the cell — the rim only pools inward.
#[allow(clippy::too_many_arguments)] // one call site; the cell state travels together
#[allow(clippy::too_many_lines)] // a flat sequence of nine quad emits; splitting hides the slice
fn emit_underlay_cell(
    material: Material,
    resolved: &ResolvedCell,
    cx: i32,
    cz: i32,
    rims: [f32; 4],
    patch: &CellLift,
    styles: &StyleTable,
    tris: &mut Vec<DrawTriangle>,
) {
    let surface = |wx: f32, wz: f32| (patch.y(wx, wz), patch.shade(wx, wz));
    let surface = &surface;
    let s = styles.get(material);
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
        emit_quad_shaded(
            material,
            resolved,
            [x0, z0, x3, z3],
            0.0,
            surface,
            styles,
            tris,
        );
        return;
    }
    let darken = s.rim_darken;
    let (left, right, top, bottom) = (rims[0], rims[1], rims[2], rims[3]);
    // Interior.
    emit_quad_shaded(
        material,
        resolved,
        [x1, z1, x2, z2],
        0.0,
        surface,
        styles,
        tris,
    );
    // Edge strips.
    emit_quad_shaded(
        material,
        resolved,
        [x0, z1, x1, z2],
        darken * left,
        surface,
        styles,
        tris,
    );
    emit_quad_shaded(
        material,
        resolved,
        [x2, z1, x3, z2],
        darken * right,
        surface,
        styles,
        tris,
    );
    emit_quad_shaded(
        material,
        resolved,
        [x1, z0, x2, z1],
        darken * top,
        surface,
        styles,
        tris,
    );
    emit_quad_shaded(
        material,
        resolved,
        [x1, z2, x2, z3],
        darken * bottom,
        surface,
        styles,
        tris,
    );
    // Corners darken by the stronger of their two adjacent sides.
    emit_quad_shaded(
        material,
        resolved,
        [x0, z0, x1, z1],
        darken * left.max(top),
        surface,
        styles,
        tris,
    );
    emit_quad_shaded(
        material,
        resolved,
        [x2, z0, x3, z1],
        darken * right.max(top),
        surface,
        styles,
        tris,
    );
    emit_quad_shaded(
        material,
        resolved,
        [x0, z2, x1, z3],
        darken * left.max(bottom),
        surface,
        styles,
        tris,
    );
    emit_quad_shaded(
        material,
        resolved,
        [x2, z2, x3, z3],
        darken * right.max(bottom),
        surface,
        styles,
        tris,
    );
}

/// Push the two triangles of one underlay quad spanning `rect`
/// (`[x0, z0, x1, z1]` octimeters) on the given surface evaluator
/// (`(wx, wz)` meters to `(y, slope shade)`), each corner shaded by the
/// wash field and the slope shade at its own world position and darkened
/// by `rim_darken`.
// The resolved style row `s` plus the quad's four corners (`a`..`d`) read
// clearest under these conventional short names.
#[allow(clippy::many_single_char_names)]
#[allow(clippy::too_many_arguments)] // two call sites; the quad state travels together
fn emit_quad_shaded(
    material: Material,
    resolved: &ResolvedCell,
    rect: [i32; 4],
    rim_darken: f32,
    surface: &impl Fn(f32, f32) -> (f32, f32),
    styles: &StyleTable,
    tris: &mut Vec<DrawTriangle>,
) {
    let s = styles.get(material);
    let corner = |xo: i32, zo: i32| {
        let wx = xo as f32 / OCTIMETERS_PER_METER;
        let wz = zo as f32 / OCTIMETERS_PER_METER;
        let (y, shade) = surface(wx, wz);
        let light = wash_lightness(s, resolved.light, wx, wz, resolved.stroke);
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

/// The bilinear lakebed height in octimeters at world point `(wx, wz)`
/// (meters), interpolated over the raw per-cell [`World::height`] plane
/// anchored at cell centers — the ground the water depth grades against.
fn bilinear_lakebed(world: &World, wx: f32, wz: f32) -> f32 {
    let fx = wx - 0.5;
    let fz = wz - 0.5;
    let x0 = floor_to_i32(fx);
    let z0 = floor_to_i32(fz);
    let tx = fx - x0 as f32;
    let tz = fz - z0 as f32;
    let h = |cx: i32, cz: i32| world.height(CellPos { x: cx, z: cz }) as f32;
    let bottom = h(x0, z0) * (1.0 - tx) + h(x0 + 1, z0) * tx;
    let top = h(x0, z0 + 1) * (1.0 - tx) + h(x0 + 1, z0 + 1) * tx;
    bottom * (1.0 - tz) + top * tz
}

/// The water depth `[0, 1]` at world point `(wx, wz)` for a surface at
/// `level_octimeters`: the surface-to-lakebed drop over
/// [`WATER_DEPTH_FULL_OCTIMETERS`], clamped. `0` at the waterline, `1` in
/// water at least [`WATER_DEPTH_FULL_OCTIMETERS`] deep.
fn water_depth(world: &World, level_octimeters: i32, wx: f32, wz: f32) -> f32 {
    ((level_octimeters as f32 - bilinear_lakebed(world, wx, wz)) / WATER_DEPTH_FULL_OCTIMETERS)
        .clamp(0.0, 1.0)
}

/// The depth to grade a water label owned by `owner`: the depth at the
/// owner's center when it is a water cell, else `0` (a water polygon owned
/// by a land cell reads as the shallow rim, not deep water).
fn owner_water_depth(world: &World, owner: CellPos) -> f32 {
    world.water_level(owner).map_or(0.0, |level| {
        water_depth(world, level, owner.x as f32 + 0.5, owner.z as f32 + 0.5)
    })
}

/// Emit one interior water cell's body: the flat water patch at the plane
/// level, tiled as a depth-graded quad per subcell. Each subcell grades
/// toward the pooled deep-water hue by the lakebed drop below the surface
/// at its center, so a shelving lakebed reads as shallows deepening inward
/// while the surface stays exactly flat. No pooled-rim nine-slice — the
/// shoreline rim is the partition's marched band around the water body.
fn emit_water_cell(
    world: &World,
    cell: CellPos,
    styles: &StyleTable,
    tris: &mut Vec<DrawTriangle>,
) {
    let level = world.water_level(cell).unwrap_or(0);
    let s = styles.get(Material::Water);
    let lift = CellLift::of(world, cell);
    for sj in 0..SUB {
        for si in 0..SUB {
            let x0 = cell.x * 256 + si * OCTIMETERS_PER_SUBCELL;
            let z0 = cell.z * 256 + sj * OCTIMETERS_PER_SUBCELL;
            let center_x = (x0 + OCTIMETERS_PER_SUBCELL / 2) as f32 / OCTIMETERS_PER_METER;
            let center_z = (z0 + OCTIMETERS_PER_SUBCELL / 2) as f32 / OCTIMETERS_PER_METER;
            let depth = water_depth(world, level, center_x, center_z);
            let resolved = resolve_cell(s, center_x, center_z, Some(depth));
            let color = hsl_to_linear_rgb(resolved.hue, resolved.sat, resolved.light);
            let vertex = |wx: f32, wz: f32| Vertex {
                x: wx,
                y: lift.y(wx, wz),
                z: wz,
                r: color[0],
                g: color[1],
                b: color[2],
            };
            push_quad(
                tris,
                x0,
                z0,
                x0 + OCTIMETERS_PER_SUBCELL,
                z0 + OCTIMETERS_PER_SUBCELL,
                &vertex,
            );
        }
    }
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
fn mesh_overlay(world: &World, at: ChunkPos, styles: &StyleTable, tris: &mut Vec<DrawTriangle>) {
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
            mesh_overlay_material(world, at, Material::from_u8_or_void(id as u8), styles, tris);
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
    styles: &StyleTable,
) -> Vec<SmoothParams> {
    let s = styles.get(material);
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
/// layer. Every overlay material contours the same way — water is ground
/// fabric in the underlay, so an overlay-painted water body draws as a
/// generic marched surface with no special treatment. The apron is fixed at
/// the maximum a profile can demand ([`MAX_APRON_SUBCELLS`]), so field
/// content never changes a chunk's read reach.
fn mesh_overlay_material(
    world: &World,
    at: ChunkPos,
    material: Material,
    styles: &StyleTable,
    tris: &mut Vec<DrawTriangle>,
) {
    let s = styles.get(material);
    let apron = MAX_APRON_SUBCELLS;
    let (field, n) = sample_field(world, at, material, apron);
    let params = sample_params(world, at, material, apron, n, styles);
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

/// Emit the raw calibration view: one grayscale quad per non-Void
/// underlay cell on the cell's surface patch, its value the cell's own
/// hue-noise field (no slope shade — the gray is the calibration
/// instrument).
fn mesh_raw(world: &World, at: ChunkPos, styles: &StyleTable, tris: &mut Vec<DrawTriangle>) {
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
            let v = raw_field(
                styles.get(material),
                cell.x as f32 + 0.5,
                cell.z as f32 + 0.5,
            );
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

/// The marching-squares contour segments for one label's window case,
/// indexed by the 4-bit case `BL | BR<<1 | TR<<2 | TL<<3` (`1` = the corner
/// holds the label). Each entry lists the boundary segments as pairs of
/// edge-midpoint indices over the window's eight points (edge midpoints
/// `4 = bottom, 5 = right, 6 = top, 7 = left`) — the same crossings the cap
/// polygon draws, so a wall lofted from them shares the cap's vertices.
/// Cases `0` and `15` have no boundary; the two saddles (`5`, `10`) split
/// into their two corner crossings.
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

/// Emit the chunk's vertical cliff faces as the union of two segment
/// classes lofted from one shared partition: marched walls where a material
/// or Void boundary stands past the step ceiling
/// ([`emit_marched_walls`]), and cell-edge lattice walls where a
/// same-material cliff leaves no marched boundary ([`emit_lattice_walls`]).
/// The raw calibration view has no partition, so it closes every cliff at
/// cell resolution in flat gray.
fn emit_walls(
    world: &World,
    at: ChunkPos,
    mode: ViewMode,
    styles: &StyleTable,
    partition: Option<&DisplayPartition>,
    tris: &mut Vec<DrawTriangle>,
) {
    if let Some(part) = partition {
        emit_marched_walls(world, at, part, styles, tris);
        emit_lattice_walls(world, at, mode, styles, true, tris);
    } else {
        emit_lattice_walls(world, at, mode, styles, false, tris);
    }
}

/// Loft the marched cliff walls: over the boundary windows of the display
/// partition, wherever a segment separates two materials — or a material
/// from a Void hole — whose surface levels stand past the step ceiling,
/// drop the segment's marched contour from the high side down to the low
/// ground. Each window is owned by the cell under its center, the same
/// ownership the cap walk uses, so a wall's top vertices lift through the
/// identical owner-clamped patch ([`label_lift`]) the cap drew — landing
/// the top edge exactly on the cap's contour, watertight by construction.
/// Emission gates on the owner standing a cliff above the segment's low
/// side, so exactly the high-owned window lofts a given face and a
/// low-owned window (whose top would clamp low) stays silent. A grounded
/// low side tucks under its surface so the low cap covers the base; a Void
/// low side drops [`WALL_VOID_DROP_OCTIMETERS`] so the hole reads as thick
/// ground.
#[allow(clippy::too_many_lines)] // one boundary walk: classify sides, extract segments, loft
#[allow(clippy::similar_names)] // the segment's two endpoints read clearest as `_p` / `_q` pairs
fn emit_marched_walls(
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
            let owner_level = world.surface_level(owner);
            // The lowest edge neighbor of the owner: a Void hole floors to
            // the owner's own (high) cell, so its drop is gated on the owner
            // standing a real cliff above its surroundings rather than on the
            // hole's meaningless level.
            let min_neighbor = [(1, 0), (-1, 0), (0, 1), (0, -1)]
                .into_iter()
                .map(|(dx, dz)| {
                    world.surface_level(CellPos {
                        x: owner.x + dx,
                        z: owner.z + dz,
                    })
                })
                .min()
                .unwrap_or(owner_level);
            // Corner sample floor-cell surface levels, same order as `mats`.
            let level = [[x_lo, z_lo], [x_hi, z_lo], [x_hi, z_hi], [x_lo, z_hi]].map(|[cx, cz]| {
                world.surface_level(CellPos {
                    x: cx.div_euclid(256),
                    z: cz.div_euclid(256),
                })
            });
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
            // Wall color: the owning (high) cell's cliff material, top-to-base
            // shaded. The owner is the high side wherever a wall lofts.
            let cliff = world.cliff_material(owner);
            let resolved = resolve_cell(
                styles.get(cliff),
                owner.x as f32 + 0.5,
                owner.z as f32 + 0.5,
                None,
            );
            let top_rgb = hsl_to_linear_rgb(
                resolved.hue,
                resolved.sat,
                (resolved.light * SKIRT_TOP_SHADE).clamp(0.0, 100.0),
            );
            let base_rgb = hsl_to_linear_rgb(
                resolved.hue,
                resolved.sat,
                (resolved.light * SKIRT_BASE_SHADE).clamp(0.0, 100.0),
            );
            for a in 1..6u8 {
                if !mats.contains(&a) {
                    continue;
                }
                let case = u8::from(mats[0] == a)
                    | u8::from(mats[1] == a) << 1
                    | u8::from(mats[2] == a) << 2
                    | u8::from(mats[3] == a) << 3;
                for &(p, q) in WALL_SEGMENTS[case as usize] {
                    // The low side along the segment: each crossed edge's
                    // non-`a` corner. Void wins (a hole drops the fixed
                    // depth); otherwise the lower ground governs the base.
                    let low_of = |m: u8| {
                        let (i, j) = edge_corners(m);
                        let k = if mats[i] == a { j } else { i };
                        (mats[k], level[k])
                    };
                    let (mat_p, lvl_p) = low_of(p);
                    let (mat_q, lvl_q) = low_of(q);
                    let is_void = mat_p == 0 || mat_q == 0;
                    let low_level = lvl_p.min(lvl_q);
                    // Only a genuine cliff under the high owner lofts. A
                    // material boundary reads the drop to the low neighbor
                    // cell; a Void hole reads the owner's drop to its
                    // surroundings. Either way a low-owned window (its top
                    // would clamp low) and a flat boundary fall through here.
                    let base_level = if is_void { min_neighbor } else { low_level };
                    if owner_level - base_level <= STEP_MAX_OCTIMETERS {
                        continue;
                    }
                    let mp = mid(p);
                    let mq = mid(q);
                    let wx_p = mp[0] as f32 / OCTIMETERS_PER_METER;
                    let wz_p = mp[1] as f32 / OCTIMETERS_PER_METER;
                    let wx_q = mq[0] as f32 / OCTIMETERS_PER_METER;
                    let wz_q = mq[1] as f32 / OCTIMETERS_PER_METER;
                    let yt_p = label_lift(world, owner, wx_p, wz_p).0;
                    let yt_q = label_lift(world, owner, wx_q, wz_q).0;
                    let (yb_p, yb_q) = if is_void {
                        let drop = WALL_VOID_DROP_OCTIMETERS as f32 / OCTIMETERS_PER_METER;
                        (yt_p - drop, yt_q - drop)
                    } else {
                        let base = (low_level - WALL_TUCK_OCTIMETERS) as f32 / OCTIMETERS_PER_METER;
                        (base, base)
                    };
                    push_wall_quad(
                        tris,
                        [wx_p, wz_p, yt_p],
                        [wx_q, wz_q, yt_q],
                        yb_p,
                        yb_q,
                        top_rgb,
                        base_rgb,
                    );
                }
            }
        }
    }
}

/// Loft the cell-edge cliff walls: for every chunk-local cell standing a
/// cliff above an edge neighbor, a vertical face on the shared cell-edge
/// lattice line — the break the owner-pinned patches already draw. The high
/// cell owns the face, so a chunk-border cliff lofts exactly once
/// fleet-wide. In the painted view `same_material_only` restricts this to
/// same-material cliffs (a material boundary lofts as a marched wall
/// instead); the raw calibration view has no partition, so it closes every
/// cliff here in flat gray. Where the corner walk merged the two sides'
/// plates the gap tapers to nothing and the face is skipped.
#[allow(clippy::too_many_lines)] // one linear pass: enumerate, classify, color, emit
fn emit_lattice_walls(
    world: &World,
    at: ChunkPos,
    mode: ViewMode,
    styles: &StyleTable,
    same_material_only: bool,
    tris: &mut Vec<DrawTriangle>,
) {
    // Per direction: neighbor offset, the shared edge's two lattice corners
    // as indices into the high cell's corner order, and the same lattice
    // points in the neighbor's order (the low side's plates).
    struct WallEdge {
        offset: (i32, i32),
        top: [usize; 2],
        bottom: [usize; 2],
    }
    const DIRECTIONS: [WallEdge; 4] = [
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
    let tuck = WALL_TUCK_OCTIMETERS as f32 / OCTIMETERS_PER_METER;
    for lz in 0..EDGE {
        for lx in 0..EDGE {
            let cell = CellPos {
                x: at.x * EDGE + lx,
                z: at.z * EDGE + lz,
            };
            let material = world.underlay(cell);
            if material == Material::Void {
                continue;
            }
            let cell_level = world.surface_level(cell);
            let mut cached: Option<[f32; 4]> = None;
            for edge in &DIRECTIONS {
                let neighbor = CellPos {
                    x: cell.x + edge.offset.0,
                    z: cell.z + edge.offset.1,
                };
                if same_material_only && world.underlay(neighbor) != material {
                    continue; // a material boundary lofts as a marched wall
                }
                if cell_level <= world.surface_level(neighbor)
                    || !world.edge_is_cliff(cell, neighbor)
                {
                    continue;
                }
                let top = *cached.get_or_insert_with(|| world.cell_corner_heights(cell));
                let bottom = world.cell_corner_heights(neighbor);
                let y_top = [top[edge.top[0]], top[edge.top[1]]];
                let y_low = [bottom[edge.bottom[0]], bottom[edge.bottom[1]]];
                if (y_top[0] - y_low[0]).abs() < f32::EPSILON
                    && (y_top[1] - y_low[1]).abs() < f32::EPSILON
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
                let (top_rgb, base_rgb) = match mode {
                    ViewMode::Raw => ([RAW_SKIRT_GRAY; 3], [RAW_SKIRT_GRAY; 3]),
                    ViewMode::Painted => {
                        let cliff = world.cliff_material(cell);
                        let resolved = resolve_cell(
                            styles.get(cliff),
                            cell.x as f32 + 0.5,
                            cell.z as f32 + 0.5,
                            None,
                        );
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
                push_wall_quad(
                    tris,
                    [x0, z0, y_top[0]],
                    [x1, z1, y_top[1]],
                    y_low[0] - tuck,
                    y_low[1] - tuck,
                    top_rgb,
                    base_rgb,
                );
            }
        }
    }
}

/// Push the two triangles of one vertical wall quad: a top edge from
/// `top_a` to `top_b` (`[wx, wz, y]` meters) dropped to `y_bottom_a` /
/// `y_bottom_b` at the same footprint, top vertices in `top_rgb` and base
/// in `base_rgb` so the face darkens toward the ground shadow.
fn push_wall_quad(
    tris: &mut Vec<DrawTriangle>,
    top_a: [f32; 3],
    top_b: [f32; 3],
    y_bottom_a: f32,
    y_bottom_b: f32,
    top_rgb: [f32; 3],
    base_rgb: [f32; 3],
) {
    let vert = |x: f32, z: f32, y: f32, rgb: [f32; 3]| Vertex {
        x,
        y,
        z,
        r: rgb[0],
        g: rgb[1],
        b: rgb[2],
    };
    let a = vert(top_a[0], top_a[1], top_a[2], top_rgb);
    let b = vert(top_b[0], top_b[1], top_b[2], top_rgb);
    let c = vert(top_b[0], top_b[1], y_bottom_b, base_rgb);
    let d = vert(top_a[0], top_a[1], y_bottom_a, base_rgb);
    tris.push(DrawTriangle { verts: [a, b, c] });
    tris.push(DrawTriangle { verts: [a, c, d] });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::{
        CELLS_PER_CHUNK_AREA, Chunk, Region, STEP_MAX_OCTIMETERS, SUBCELLS_PER_CELL,
        SetMaterialStyle, SmoothingProfile, UNDERLAY_POINT_INHERIT,
    };
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
            &StyleTable::default(),
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
            &StyleTable::default(),
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

        let left_cell_right_rim = cell_rims(
            &world,
            CellPos { x: 15, z: 4 },
            Material::Grass,
            &StyleTable::default(),
        )[1];
        let right_cell_left_rim = cell_rims(
            &world,
            CellPos { x: 16, z: 4 },
            Material::Grass,
            &StyleTable::default(),
        )[0];
        assert_eq!(
            left_cell_right_rim, right_cell_left_rim,
            "both sides must agree on the shared-edge rim",
        );
    }

    #[test]
    fn water_depth_grades_with_the_lakebed_and_clamps() {
        // Depth is the surface-to-lakebed drop over
        // WATER_DEPTH_FULL_OCTIMETERS: zero at the waterline, rising as the
        // lakebed falls away, clamped to full a meter down. A lakebed
        // shelving eastward reads monotonically deeper — the level-minus-
        // lakebed grading that replaced the shore-distance scan.
        let mut chunk = Chunk::empty();
        for lz in 0..EDGE {
            for lx in 0..EDGE {
                // Lakebed drops 20 octimeters per cell to the east.
                chunk.height[(lz * EDGE + lx) as usize] = -20 * lx;
            }
        }
        let mut world = World::new();
        world.insert_chunk(ChunkPos { x: 0, z: 0 }, chunk);
        // Surface at the datum; depth rises with the eastward lakebed drop.
        let shallow = water_depth(&world, 0, 2.5, 5.5);
        let deep = water_depth(&world, 0, 8.5, 5.5);
        assert!(deep > shallow, "deeper east: {deep} > {shallow}");
        assert_eq!(
            water_depth(&world, 0, 0.5, 5.5),
            0.0,
            "at the waterline depth is zero",
        );
        // Cell 14's lakebed is -280 octimeters, past a meter — clamps to 1.
        assert_eq!(
            water_depth(&world, 0, 14.5, 5.5),
            1.0,
            "past full depth clamps"
        );
    }

    #[test]
    fn full_water_chunk_budget_is_pinned() {
        // Tripwire: water is underlay fabric, so a fully-water chunk is all
        // interior body — each of its 256 cells tiles the flat water patch
        // with one depth-graded quad per subcell (16 subcells x 2 tris), and
        // nothing else draws (no partition windows inside a uniform body, no
        // overlay, no walls on flat water). 256 * 32 = 8192. A change to the
        // per-subcell water resolution or the interior body path moves this
        // and must be deliberate.
        let mut world = World::new();
        for dz in -1..=1 {
            for dx in -1..=1 {
                let mut c = Chunk::empty();
                c.underlay = [Material::Water; CELLS_PER_CHUNK_AREA];
                world.insert_chunk(ChunkPos { x: dx, z: dz }, c);
            }
        }
        let tris = mesh_chunk(
            &world,
            ChunkPos { x: 0, z: 0 },
            ViewMode::Painted,
            &StyleTable::default(),
        );
        assert_eq!(tris.len(), 8192, "256 cells x 16 subcells x 2 tris");
    }

    #[test]
    fn water_shoreline_is_smoothed_and_rimmed() {
        // Water is underlay fabric: a water square painted into a grass
        // field smooths and rims its waterline through the same partition as
        // any material boundary (a crossing lands off the cell lattice), and
        // the body renders as blue water.
        let mut world = World::new();
        let mut c = Chunk::empty();
        c.underlay = [Material::Grass; CELLS_PER_CHUNK_AREA];
        // A 4x4-cell water square in the chunk interior.
        for lz in 6..10 {
            for lx in 6..10 {
                c.underlay[(lz * EDGE + lx) as usize] = Material::Water;
            }
        }
        world.insert_chunk(ChunkPos { x: 0, z: 0 }, c);
        let tris = mesh_chunk(
            &world,
            ChunkPos { x: 0, z: 0 },
            ViewMode::Painted,
            &StyleTable::default(),
        );
        assert!(
            has_smoothed_crossing(&tris),
            "the waterline is corner-minimized like any material boundary",
        );
        assert!(
            tris.iter()
                .flat_map(|t| t.verts.iter())
                .any(|v| v.b > v.r && v.b > v.g),
            "the water body renders as blue water",
        );
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
            let tris = mesh_chunk(
                &world,
                ChunkPos { x: cx, z: 0 },
                ViewMode::Painted,
                &StyleTable::default(),
            );
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
        let mesh0 = mesh_chunk(
            &world,
            ChunkPos { x: 0, z: 0 },
            ViewMode::Painted,
            &StyleTable::default(),
        );
        let mesh1 = mesh_chunk(
            &world,
            ChunkPos { x: 1, z: 0 },
            ViewMode::Painted,
            &StyleTable::default(),
        );
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
            &StyleTable::default(),
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
            &StyleTable::default(),
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
        let mesh = mesh_chunk(
            &world,
            ChunkPos { x: 0, z: 0 },
            ViewMode::Painted,
            &StyleTable::default(),
        );
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
        let mesh0 = mesh_chunk(
            &world,
            ChunkPos { x: 0, z: 0 },
            ViewMode::Painted,
            &StyleTable::default(),
        );
        let mesh1 = mesh_chunk(
            &world,
            ChunkPos { x: 1, z: 0 },
            ViewMode::Painted,
            &StyleTable::default(),
        );
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
        let mesh = mesh_chunk(
            &world,
            ChunkPos { x: 0, z: 0 },
            ViewMode::Painted,
            &StyleTable::default(),
        );
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
        let mesh = mesh_chunk(
            &world,
            ChunkPos { x: 0, z: 0 },
            ViewMode::Painted,
            &StyleTable::default(),
        );
        assert!(!mesh.is_empty());
        for v in mesh.iter().flat_map(|t| t.verts.iter()) {
            let surface = world.surface_height(v.x, v.z);
            // Encroachment flaps ride ENCROACH_LIFT above the ground on
            // purpose (the grass-over-sand margin lies on top of the seam);
            // the drawn-equals-stood-on invariant is about the ground a
            // mover stands on, so skip the flap layer and hold every other
            // vertex exactly on the surface.
            if (v.y - surface - ENCROACH_LIFT).abs() < 1e-4 {
                continue;
            }
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
        let mesh = mesh_chunk(
            &world,
            ChunkPos { x: 0, z: 0 },
            ViewMode::Painted,
            &StyleTable::default(),
        );
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
    fn a_same_material_cliff_stands_a_wall_on_the_cell_line() {
        // A grass plateau chunk one meter up beside a grass ground chunk:
        // same material, so the material partition leaves no marched
        // boundary and the wall stands on the shared cell-edge line — where
        // the old skirt stood. The face belongs to the high cell, so the
        // high chunk lofts it exactly once and the low chunk never does;
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

        let is_border_wall = |t: &DrawTriangle| {
            t.verts.iter().all(|v| (v.x - 16.0).abs() < 1e-6)
                && t.verts.iter().any(|v| v.y > 0.9)
                && t.verts.iter().any(|v| v.y < 0.1)
        };
        let low_mesh = mesh_chunk(
            &world,
            ChunkPos { x: 0, z: 0 },
            ViewMode::Painted,
            &StyleTable::default(),
        );
        let high_mesh = mesh_chunk(
            &world,
            ChunkPos { x: 1, z: 0 },
            ViewMode::Painted,
            &StyleTable::default(),
        );
        assert!(
            high_mesh.iter().any(is_border_wall),
            "the high chunk stands the wall on the cell line",
        );
        assert!(
            !low_mesh.iter().any(is_border_wall),
            "the low chunk does not double-draw the shared face",
        );
    }

    #[test]
    fn a_material_boundary_at_a_cliff_does_not_drape() {
        // Sand plateau (1 m up) against grass ground, material boundary on
        // the cliff line: every colored (non-wall) triangle must stay on
        // its own side of the break — the cliff face belongs to the gray
        // wall. Position-pure lifting would stretch the boundary windows
        // into meter-tall sand and grass caps down the face.
        let mut world = World::new();
        let mut low = Chunk::empty();
        low.underlay = [Material::Grass; CELLS_PER_CHUNK_AREA];
        let mut high = Chunk::empty();
        high.underlay = [Material::Sand; CELLS_PER_CHUNK_AREA];
        high.height = [256; CELLS_PER_CHUNK_AREA];
        world.insert_chunk(ChunkPos { x: 0, z: 0 }, low);
        world.insert_chunk(ChunkPos { x: 1, z: 0 }, high);

        for at in [ChunkPos { x: 0, z: 0 }, ChunkPos { x: 1, z: 0 }] {
            let mesh = mesh_chunk(&world, at, ViewMode::Painted, &StyleTable::default());
            for t in &mesh {
                let gray = t
                    .verts
                    .iter()
                    .all(|v| (v.r - v.b).abs() < 0.05 && (v.g - v.b).abs() < 0.05);
                if gray {
                    continue; // walls own the vertical face
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
        // A stone overlay on the ramp keeps its rim and body lifts measured
        // from the surface, not from `y = 0` — the drape rule for overlays
        // over sloped ground. The grass-over-sand encroachment flap rides
        // the same surface one octimeter up, so it is a third legal lift.
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
        let mesh = mesh_chunk(&world, at, ViewMode::Painted, &StyleTable::default());
        let mut overlay_verts = 0;
        for v in mesh.iter().flat_map(|t| t.verts.iter()) {
            let lift = v.y - world.surface_height(v.x, v.z);
            if lift > 1e-5 {
                overlay_verts += 1;
                assert!(
                    (lift - OVERLAY_RIM_LIFT).abs() < 1e-4
                        || (lift - OVERLAY_BODY_LIFT).abs() < 1e-4
                        || (lift - ENCROACH_LIFT).abs() < 1e-4,
                    "surface layer lift {lift} is neither rim, body, nor flap at ({}, {})",
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
        let raw = mesh_chunk(
            &world,
            ChunkPos { x: 0, z: 0 },
            ViewMode::Raw,
            &StyleTable::default(),
        );
        assert!(!raw.is_empty(), "raw mode emits geometry");
        for v in raw.iter().flat_map(|t| t.verts.iter()) {
            assert!(v.r == v.g && v.g == v.b, "raw vertex is grayscale");
        }
        let painted = mesh_chunk(
            &world,
            ChunkPos { x: 0, z: 0 },
            ViewMode::Painted,
            &StyleTable::default(),
        );
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
        let tris = mesh_chunk(
            &world,
            ChunkPos { x: 0, z: 0 },
            ViewMode::Painted,
            &StyleTable::default(),
        );
        assert!(tris.is_empty(), "an all-Void chunk emits no geometry");
    }

    #[test]
    fn a_live_style_write_repaints_the_mesh() {
        // Threading StyleTable through the mesher is mechanical (dozens of
        // call sites); this catches any path that kept reading a stale
        // baked-in row instead of the table argument — a grass world meshed
        // against the default table and one with grass's base lightness
        // moved far must paint different colors.
        let world = world_with_underlay(ChunkPos { x: 0, z: 0 }, |_, _| Material::Grass);
        let default_styles = StyleTable::default();
        let baseline = mesh_chunk(
            &world,
            ChunkPos { x: 0, z: 0 },
            ViewMode::Painted,
            &default_styles,
        );

        let grass = default_styles.get(Material::Grass);
        let mut tuned = StyleTable::default();
        tuned.apply(&SetMaterialStyle {
            material: Material::Grass.to_u8(),
            base_hue: grass.base_hue,
            base_sat: grass.base_sat,
            base_light: grass.base_light + 30.0,
            amp_hue: grass.amp_hue,
            amp_sat: grass.amp_sat,
            amp_light: grass.amp_light,
            wavelength: grass.wavelength,
            octaves: grass.octaves,
            persistence: grass.persistence,
            seed_offset: grass.seed_offset,
            flow_wavelength: grass.flow_wavelength,
            smoothing_degrees: grass.smoothing_degrees,
            smoothing_iterations: grass.smoothing_iterations,
            rim_inset_octimeters: grass.rim_inset_octimeters,
            rim_darken: grass.rim_darken,
            wash_grade: grass.wash_grade,
            water_depth_darken: grass.water_depth_darken,
            blob_merge_degrees: grass.blob_merge_degrees,
            encroach_rank: grass.encroach_rank,
            encroach_reach_octimeters: grass.encroach_reach_octimeters,
            encroach_raggedness: grass.encroach_raggedness,
        });
        let tuned_mesh = mesh_chunk(&world, ChunkPos { x: 0, z: 0 }, ViewMode::Painted, &tuned);

        assert_eq!(
            baseline.len(),
            tuned_mesh.len(),
            "the same world tiles identically regardless of style values"
        );
        let base_color = |t: &DrawTriangle| (t.verts[0].r, t.verts[0].g, t.verts[0].b);
        assert!(
            baseline
                .iter()
                .zip(tuned_mesh.iter())
                .any(|(a, b)| base_color(a) != base_color(b)),
            "a live style-table write must change the painted mesh's colors",
        );
    }

    #[test]
    fn all_inherit_meshes_identically_to_cell_expansion() {
        // Tripwire: the underlay-point inherit sentinel must fold back to the
        // per-cell material exactly, so an unshaped world meshes byte-for-byte
        // as it did before points existed. A varied world meshed with
        // all-inherit points must equal the same world with every point
        // explicitly pinned to its cell's cascade material — the literal
        // cell-expansion `partition_inputs` performed before. Any drift in
        // `underlay_point`'s indexing or sentinel handling breaks this.
        let at = ChunkPos { x: 0, z: 0 };
        let mut world = World::new();
        let mut chunk = Chunk::empty();
        for lz in 0..EDGE {
            for lx in 0..EDGE {
                let i = (lz * EDGE + lx) as usize;
                chunk.underlay[i] = match (lx + lz) % 4 {
                    0 => Material::Void, // falls through to the region default
                    1 => Material::Grass,
                    2 => Material::Sand,
                    _ => Material::Stone,
                };
                chunk.height[i] = 10 * lx;
                chunk.region[i] = 1;
            }
        }
        world.insert_chunk(at, chunk);
        world.insert_region(
            1,
            Region {
                name: "meadow".into(),
                default_material: Material::Dirt,
                cliff_material: Material::Stone,
            },
        );

        let inherit_mesh = mesh_chunk(&world, at, ViewMode::Painted, &StyleTable::default());
        assert!(!inherit_mesh.is_empty(), "the fixture must mesh something");

        // Pin every cell's points to its own cascade material — the explicit
        // cell-expansion. Apron cells stay off-chunk and read Void either way.
        let mut expanded = world.clone();
        for lz in 0..EDGE {
            for lx in 0..EDGE {
                let cell = CellPos {
                    x: at.x * EDGE + lx,
                    z: at.z * EDGE + lz,
                };
                let material = expanded.underlay(cell).to_u8();
                expanded.set_cell_points(cell, &[material; SUBCELLS_PER_CELL]);
            }
        }
        let expanded_mesh = mesh_chunk(&expanded, at, ViewMode::Painted, &StyleTable::default());

        assert_eq!(
            inherit_mesh, expanded_mesh,
            "all-inherit points fold to the explicit cell-expansion",
        );
    }

    #[test]
    fn a_point_shaped_cell_marches_inside_its_cell_span() {
        // A cell whose only authored material is a 2x2 block of interior
        // subcells (the rest inherit Void) marches a silhouette that departs
        // the cell-edge lines: the Grass/Void contour lands between subcell
        // centers, strictly inside the cell span, where a whole-cell material
        // would only bound at the integer cell edges.
        let at = ChunkPos { x: 0, z: 0 };
        let cell = CellPos { x: 8, z: 8 };
        let sub = SUB as usize;
        let mut points = [UNDERLAY_POINT_INHERIT; SUBCELLS_PER_CELL];
        for sub_z in 1..3 {
            for sub_x in 1..3 {
                points[sub_z * sub + sub_x] = Material::Grass.to_u8();
            }
        }
        let mut world = World::new();
        world.insert_chunk(at, Chunk::empty()); // all Void underlay
        world.set_cell_points(cell, &points);

        let mesh = mesh_chunk(&world, at, ViewMode::Painted, &StyleTable::default());
        assert!(!mesh.is_empty(), "the authored blob must mesh");

        let verts = mesh.iter().flat_map(|t| t.verts.iter());
        let inside_x = verts
            .clone()
            .any(|v| v.x > cell.x as f32 + 0.1 && v.x < cell.x as f32 + 0.9);
        let inside_z = verts
            .clone()
            .any(|v| v.z > cell.z as f32 + 0.1 && v.z < cell.z as f32 + 0.9);
        assert!(
            inside_x && inside_z,
            "the marched contour must depart the cell-edge lines",
        );
        // And it stays bounded inside the cell — no vertex escapes the span.
        assert!(
            verts.clone().all(|v| {
                v.x >= cell.x as f32 - 1e-4
                    && v.x <= cell.x as f32 + 1.0 + 1e-4
                    && v.z >= cell.z as f32 - 1e-4
                    && v.z <= cell.z as f32 + 1.0 + 1e-4
            }),
            "the shaped silhouette stays inside the cell",
        );
    }

    /// A full `SetMaterialStyle` write mirroring `row` for `material` — the
    /// verbose per-field copy a live tuning pass would send, so a test can
    /// nudge one field off the defaults and apply it.
    fn style_msg(material: Material, row: &style::MaterialStyle) -> SetMaterialStyle {
        SetMaterialStyle {
            material: material.to_u8(),
            base_hue: row.base_hue,
            base_sat: row.base_sat,
            base_light: row.base_light,
            amp_hue: row.amp_hue,
            amp_sat: row.amp_sat,
            amp_light: row.amp_light,
            wavelength: row.wavelength,
            octaves: row.octaves,
            persistence: row.persistence,
            seed_offset: row.seed_offset,
            flow_wavelength: row.flow_wavelength,
            smoothing_degrees: row.smoothing_degrees,
            smoothing_iterations: row.smoothing_iterations,
            rim_inset_octimeters: row.rim_inset_octimeters,
            rim_darken: row.rim_darken,
            wash_grade: row.wash_grade,
            water_depth_darken: row.water_depth_darken,
            blob_merge_degrees: row.blob_merge_degrees,
            encroach_rank: row.encroach_rank,
            encroach_reach_octimeters: row.encroach_reach_octimeters,
            encroach_raggedness: row.encroach_raggedness,
        }
    }

    /// Does the mesh carry an encroachment flap — a triangle whose vertices
    /// all sit exactly `ENCROACH_LIFT` above a flat (`y = 0`) ground?
    fn has_flap(mesh: &[DrawTriangle]) -> bool {
        mesh.iter()
            .any(|t| t.verts.iter().all(|v| (v.y - ENCROACH_LIFT).abs() < 1e-4))
    }

    #[test]
    fn encroach_distance_wraps_around_a_cliff_barrier() {
        // A wall of cliff-high cells at local x = 2 blocks the flood from
        // stepping straight across it; the one low cell at the wall's end
        // is the only gap. The flood must route down to that gap and back,
        // so a cell just past the wall reads a distance far longer than the
        // blocked straight line — the margin grows around a cliff instead
        // of pouring across it.
        let mut world = World::new();
        let mut chunk = Chunk::empty();
        for lz in 0..EDGE {
            for lx in 0..EDGE {
                let wall = lx == 2 && lz < EDGE - 1;
                chunk.height[(lz * EDGE + lx) as usize] = if wall { 10_000 } else { 0 };
            }
        }
        world.insert_chunk(ChunkPos { x: 0, z: 0 }, chunk);
        // One grid sample per cell: samples sit at cell centers (128 oct),
        // so adjacent samples map to edge-adjacent cells and the flood's
        // barrier test is exactly the cell cliff test.
        let place = GridPlacement {
            origin_oct: [128, 128],
            step_oct: 256,
        };
        let gw = EDGE as usize;
        let gh = EDGE as usize;
        // Every sample is material 2, except the source column x = 0.
        let mut grid = vec![2u8; gw * gh];
        for gz in 0..gh {
            grid[gz * gw] = 1;
        }
        let dist = encroach_distance(&world, &grid, gw, &place, 1, 10_000);
        let target = 3; // (x = 3, z = 0), directly past the wall on the source row
        assert_ne!(
            dist[target],
            i32::MAX,
            "the gap at the wall's end lets the flood through",
        );
        assert!(
            dist[target] > 3,
            "the flood routed around the wall, not across the cliff (got {})",
            dist[target],
        );
    }

    #[test]
    fn encroach_mask_excludes_equal_or_higher_rank() {
        // Sand encroaches only over strictly-lower-rank samples: a grass
        // sample one step away (higher rank) and a sand sample (equal rank)
        // stay out of the mask even though both are within reach, while a
        // dirt sample the same one step off is covered. The rank gate, not
        // distance, decides who a margin may grow over.
        let mut world = World::new();
        world.insert_chunk(ChunkPos { x: 0, z: 0 }, Chunk::empty()); // flat, no cliffs
        let styles = StyleTable::default();
        let place = GridPlacement {
            origin_oct: [16, 16],
            step_oct: 32,
        };
        // sand source, grass, sand source, dirt — all within one cell.
        let grid = vec![4u8, 1, 4, 2];
        let mask = encroach_mask(&world, &grid, 4, 1, &place, Material::Sand, &styles);
        assert!(!mask[0], "a sand source is not in its own mask");
        assert!(!mask[1], "grass (higher rank) is excluded though in range");
        assert!(!mask[2], "sand (equal rank) is excluded though in range");
        assert!(mask[3], "dirt (lower rank) within reach is covered");
    }

    #[test]
    fn equal_rank_seam_emits_no_flap() {
        // The default grass (rank 3) over sand (rank 2) grows a margin, but
        // lifting sand to grass's rank must silence it: equal ranks never
        // encroach on each other, so the seam falls back to the hard
        // partition line with no flap layer at all.
        let world = sand_band_world(None);
        let at = ChunkPos { x: 0, z: 0 };
        let default_mesh = mesh_chunk(&world, at, ViewMode::Painted, &StyleTable::default());
        assert!(
            has_flap(&default_mesh),
            "unequal ranks grow a margin over the seam",
        );

        let defaults = StyleTable::default();
        let mut sand = style_msg(Material::Sand, defaults.get(Material::Sand));
        sand.encroach_rank = defaults.get(Material::Grass).encroach_rank;
        let mut styles = StyleTable::default();
        styles.apply(&sand);
        let equal_mesh = mesh_chunk(&world, at, ViewMode::Painted, &styles);
        assert!(
            !has_flap(&equal_mesh),
            "an equal-rank seam emits no encroachment flap",
        );
    }

    #[test]
    fn encroach_flaps_tile_the_seam_across_a_chunk_border() {
        // Grass (rank 3) over a sand band (rank 2), the seam running
        // horizontally across the x = 16 chunk border. With a full-cell
        // reach and no raggedness the margin is a clean deterministic band,
        // emitted under the single-owner window rule: the two chunks must
        // together cover the seam band with no window emitted twice and no
        // hole at the border. An ownership slip doubles a flap; an
        // apron-read slip leaves a gap.
        let mut world = World::new();
        world.insert_region(
            1,
            Region {
                name: "meadow".into(),
                default_material: Material::Grass,
                cliff_material: Material::Stone,
            },
        );
        for cx in 0..2 {
            let mut chunk = Chunk::empty();
            chunk.region = [1; CELLS_PER_CHUNK_AREA];
            for lz in 8..EDGE {
                for lx in 0..EDGE {
                    chunk.underlay[(lz * EDGE + lx) as usize] = Material::Sand;
                }
            }
            world.insert_chunk(ChunkPos { x: cx, z: 0 }, chunk);
        }
        // A clean full-cell margin: reach one cell, raggedness off.
        let defaults = StyleTable::default();
        let mut grass = style_msg(Material::Grass, defaults.get(Material::Grass));
        grass.encroach_reach_octimeters = 256;
        grass.encroach_raggedness = 0.0;
        let mut styles = StyleTable::default();
        styles.apply(&grass);

        let mesh0 = mesh_chunk(&world, ChunkPos { x: 0, z: 0 }, ViewMode::Painted, &styles);
        let mesh1 = mesh_chunk(&world, ChunkPos { x: 1, z: 0 }, ViewMode::Painted, &styles);
        let is_flap = |t: &DrawTriangle| t.verts.iter().all(|v| (v.y - ENCROACH_LIFT).abs() < 1e-4);
        let flaps0: Vec<&DrawTriangle> = mesh0.iter().filter(|t| is_flap(t)).collect();
        let flaps1: Vec<&DrawTriangle> = mesh1.iter().filter(|t| is_flap(t)).collect();
        assert!(
            !flaps0.is_empty() && !flaps1.is_empty(),
            "each chunk emits its own side of the seam flap",
        );

        // No flap window is emitted by both chunks (single-owner rule).
        let key = |t: &DrawTriangle| {
            let mut k = [(0u32, 0u32, 0u32); 3];
            for (i, v) in t.verts.iter().enumerate() {
                k[i] = (v.x.to_bits(), v.y.to_bits(), v.z.to_bits());
            }
            k
        };
        for t0 in &flaps0 {
            assert!(
                !flaps1.iter().any(|t1| key(t0) == key(t1)),
                "a flap window was emitted by both chunks",
            );
        }

        // The union covers the seam band continuously across the border.
        let flap_covers =
            |flaps: &[&DrawTriangle], px: f32, pz: f32| flaps.iter().any(|t| covers(t, px, pz));
        for i in 0..=40 {
            let px = (i as f32).mul_add(0.05, 15.0); // 15.0 .. 17.0
            let pz = 8.5;
            assert!(
                flap_covers(&flaps0, px, pz) || flap_covers(&flaps1, px, pz),
                "flap seam hole at ({px}, {pz})",
            );
        }
    }

    #[test]
    fn encroach_flap_over_water_rides_the_plane() {
        // Grass grows over a waterline the same as any lower-rank seam, and
        // its flap must ride the flat authored water plane, not the lakebed
        // a meter below it — the on-top read over water for free. A 3x3
        // water pool (surface at the datum, lakebed a meter down) in a grass
        // field: some grass flap vertex lands inside the pool footprint and
        // sits at the plane level plus the flap lift.
        let mut world = World::new();
        let mut chunk = Chunk::empty();
        chunk.underlay = [Material::Grass; CELLS_PER_CHUNK_AREA];
        for lz in 7..10 {
            for lx in 7..10 {
                let idx = (lz * EDGE + lx) as usize;
                chunk.underlay[idx] = Material::Water;
                chunk.height[idx] = -256; // lakebed one meter below the datum plane
            }
        }
        world.insert_chunk(ChunkPos { x: 0, z: 0 }, chunk);
        let mesh = mesh_chunk(
            &world,
            ChunkPos { x: 0, z: 0 },
            ViewMode::Painted,
            &StyleTable::default(),
        );
        let mut found = false;
        for t in &mesh {
            // A grass flap is green (the encroacher's body color) and lifted
            // above the ground.
            let green = t.verts.iter().all(|v| v.g > v.r && v.g > v.b);
            if !green {
                continue;
            }
            for v in &t.verts {
                // Strictly inside the pool footprint (the margin only reaches
                // the outer ring, so the covered band sits near the edges).
                if v.y > 0.0005 && (7.05..=9.95).contains(&v.x) && (7.05..=9.95).contains(&v.z) {
                    found = true;
                    assert!(
                        (v.y - ENCROACH_LIFT).abs() < 1e-4,
                        "a grass flap over water rides the plane, got y = {} (lakebed would be ~-1)",
                        v.y,
                    );
                }
            }
        }
        assert!(found, "a grass flap grows over the water pool");
    }

    /// The vertical span of a triangle — a wall face stands ~1 m tall while
    /// a cap or ground quad is near-flat, so the split filters walls from
    /// the surfaces they join.
    fn y_span(t: &DrawTriangle) -> f32 {
        let max = t.verts.iter().map(|v| v.y).fold(f32::MIN, f32::max);
        let min = t.verts.iter().map(|v| v.y).fold(f32::MAX, f32::min);
        max - min
    }

    #[test]
    fn marched_wall_tops_land_on_the_cap_contour() {
        // The watertight pin: a material-boundary cliff lofts marched walls
        // whose top vertices are the cap contour's own vertices. Sand plateau
        // (1 m up, east) against grass ground: mesh the high chunk, split its
        // triangles into walls (a tall vertical span) and caps (near-flat),
        // and assert every wall-top vertex coincides exactly with a cap
        // vertex — no gap and no overlap between the cliff face and the
        // surfaces it joins. The wall lofts its top through the same
        // owner-clamped patch the cap drew, so the match is bit-exact.
        let mut world = World::new();
        let mut low = Chunk::empty();
        low.underlay = [Material::Grass; CELLS_PER_CHUNK_AREA];
        let mut high = Chunk::empty();
        high.underlay = [Material::Sand; CELLS_PER_CHUNK_AREA];
        high.height = [256; CELLS_PER_CHUNK_AREA];
        world.insert_chunk(ChunkPos { x: 0, z: 0 }, low);
        world.insert_chunk(ChunkPos { x: 1, z: 0 }, high);

        let mesh = mesh_chunk(
            &world,
            ChunkPos { x: 1, z: 0 },
            ViewMode::Painted,
            &StyleTable::default(),
        );
        let walls: Vec<&DrawTriangle> = mesh.iter().filter(|t| y_span(t) > 0.5).collect();
        assert!(
            !walls.is_empty(),
            "the material-boundary cliff lofts marched walls",
        );
        let cap_verts: Vec<&Vertex> = mesh
            .iter()
            .filter(|t| y_span(t) <= 0.5)
            .flat_map(|t| t.verts.iter())
            .collect();
        for wall in &walls {
            let top = wall.verts.iter().map(|v| v.y).fold(f32::MIN, f32::max);
            for v in wall.verts.iter().filter(|v| (v.y - top).abs() < 1e-6) {
                assert!(
                    cap_verts.iter().any(|c| (c.x - v.x).abs() < 1e-6
                        && (c.y - v.y).abs() < 1e-6
                        && (c.z - v.z).abs() < 1e-6),
                    "wall-top vertex ({}, {}, {}) has no matching cap vertex",
                    v.x,
                    v.y,
                    v.z,
                );
            }
        }
    }

    #[test]
    fn a_flat_world_emits_no_walls() {
        // Tripwire: a cliff-free world lofts no walls, so the wall pass adds
        // nothing — the full mesh equals the underlay-plus-overlay mesh built
        // without it, byte for byte. A wall pass that fired on flat ground
        // (or double-counted a lattice against a marched segment) diverges
        // here.
        let world = world_with_underlay(ChunkPos { x: 0, z: 0 }, |_, _| Material::Grass);
        let at = ChunkPos { x: 0, z: 0 };
        let styles = StyleTable::default();
        let mut expected = Vec::new();
        let _ = mesh_underlay(&world, at, &styles, &mut expected);
        mesh_overlay(&world, at, &styles, &mut expected);
        let full = mesh_chunk(&world, at, ViewMode::Painted, &styles);
        assert_eq!(full, expected, "the wall pass adds nothing to a flat world");
    }

    #[test]
    fn adjacent_chunks_agree_on_a_wall_across_their_seam() {
        // A grass cliff running in x across the vertical chunk seam at
        // x = 16: cells at z < 8 sit 1 m up, z >= 8 at the datum, same
        // material so the face is a cell-edge lattice wall. Each chunk owns
        // its half of the line; at the shared seam the two halves must meet
        // on identical vertices, or the wall would gap or double where the
        // chunks join.
        let mut world = World::new();
        for cx in 0..2 {
            let mut chunk = Chunk::empty();
            chunk.underlay = [Material::Grass; CELLS_PER_CHUNK_AREA];
            for lz in 0..EDGE {
                for lx in 0..EDGE {
                    chunk.height[(lz * EDGE + lx) as usize] = if lz < 8 { 256 } else { 0 };
                }
            }
            world.insert_chunk(ChunkPos { x: cx, z: 0 }, chunk);
        }
        let seam_verts = |at| {
            let mesh = mesh_chunk(&world, at, ViewMode::Painted, &StyleTable::default());
            let mut verts: Vec<(i64, i64, i64)> = mesh
                .iter()
                .filter(|t| y_span(t) > 0.5)
                .flat_map(|t| t.verts.iter())
                .filter(|v| (v.x - 16.0).abs() < 1e-4)
                .map(|v| {
                    (
                        (v.x * 256.0).round() as i64,
                        (v.y * 256.0).round() as i64,
                        (v.z * 256.0).round() as i64,
                    )
                })
                .collect();
            verts.sort_unstable();
            verts.dedup();
            verts
        };
        let west = seam_verts(ChunkPos { x: 0, z: 0 });
        let east = seam_verts(ChunkPos { x: 1, z: 0 });
        assert!(!west.is_empty(), "the seam carries a wall");
        assert_eq!(
            west, east,
            "both chunks place the seam wall on identical vertices",
        );
    }

    #[test]
    fn a_raised_point_cell_wears_marched_walls() {
        // A point-authored blob raised to a plateau wears walls that follow
        // its marched silhouette, not the cell edge: a 2x2 grass point block
        // in one grass cell (the rest Void points) lifted 2 m stands a
        // Void-drop wall whose top vertices sit strictly inside the cell
        // span, where its Grass/Void contour marches — the cap-detaches-
        // from-the-cell-edge case the cell-edge skirt could not close.
        let at = ChunkPos { x: 0, z: 0 };
        let cell = CellPos { x: 8, z: 8 };
        let sub = SUB as usize;
        // Explicit Void points around a 2x2 grass core, so the raised cell's
        // grass silhouette pulls inside its own edges.
        let mut points = [Material::Void.to_u8(); SUBCELLS_PER_CELL];
        for sub_z in 1..3 {
            for sub_x in 1..3 {
                points[sub_z * sub + sub_x] = Material::Grass.to_u8();
            }
        }
        // Surround the raised cell with lower grass ground so the blob is a
        // genuine plateau; the cell itself is grass at 512 octimeters.
        let mut chunk = Chunk::empty();
        chunk.underlay = [Material::Grass; CELLS_PER_CHUNK_AREA];
        chunk.height[(cell.z * EDGE + cell.x) as usize] = 512;
        let mut world = World::new();
        world.insert_chunk(at, chunk);
        world.set_cell_points(cell, &points);

        let mesh = mesh_chunk(&world, at, ViewMode::Painted, &StyleTable::default());
        let walls: Vec<&DrawTriangle> = mesh.iter().filter(|t| y_span(t) > 0.5).collect();
        assert!(!walls.is_empty(), "the raised blob wears walls");
        let inside = walls.iter().flat_map(|t| t.verts.iter()).any(|v| {
            v.x > cell.x as f32 + 0.1
                && v.x < cell.x as f32 + 0.9
                && v.z > cell.z as f32 + 0.1
                && v.z < cell.z as f32 + 0.9
        });
        assert!(
            inside,
            "the walls follow the marched silhouette inside the cell",
        );
    }
}
