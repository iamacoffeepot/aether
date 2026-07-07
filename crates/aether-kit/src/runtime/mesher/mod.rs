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
//! it. Where a cell carries authored per-point relief
//! (`World::cell_has_height_relief`) the pass resolves one stride down: the
//! interior cap tessellates to `SUB × SUB` subcell quads over point patches
//! (`SubPatch`) so a subcell break shows, the marched wall split reads
//! point (not cell) levels so a silhouette raised by deltas alone closes its
//! wall on the authored line, and same-material breaks — where no material
//! boundary exists for the marched pass to follow — close as subcell-lattice
//! walls (`emit_lattice_closure`) standing exactly on the authored
//! break lines, cell interiors included. Contour smoothing freezes at a
//! break (`partition_inputs` hands [`repartition`] a frozen mask over the
//! flanking samples): paint may not smooth across a physical cliff, so a
//! silhouette's contour follows the authored break line — the accepted
//! sample-resolution staircase — while boundaries over continuous ground
//! keep smoothing, and a boundary crossing lifts through the sample on its
//! own side of the break (threaded as data through `window_point_anchor` /
//! `anchored_lift`, never inferred from the vertex position). A relief-free
//! cell keeps the whole-cell fast path, byte-identical to a world with no
//! height points.
//! On the cell lattice — the corner plates split exactly on cliff edges, an interior
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
    GridPlacement, SmoothParams, emit_label_window, erode, label_case, label_window_polys,
    march_grid, minimize_corners, push_quad, repartition,
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

/// How far in octimeters an unbounded-void wall drops below its top edge —
/// the border-skirt fallback for the one void case with no far rim within
/// the fill-over march bound (a void that reaches the world border). A
/// bounded void joint closes instead as a real groove: wall down to the void
/// floor, floor across, wall back up (see [`emit_void_floors`]). The skirt
/// reads as thick ground rather than a paper-thin lip.
const WALL_VOID_SKIRT_OCTIMETERS: i32 = 512;

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

/// One subcell's bilinear surface patch — the point-lattice analogue of
/// [`CellLift`], spanning `1 / SUB` m. Built only where a cell carries
/// authored height relief; its four corners are the point-plate heights
/// ([`World::subcell_corner_heights`]) the mesher lifts per-point caps and
/// wall tops through. [`World::surface_height_in`]'s relief branch resolves
/// the identical patch, so drawn and stood-on agree over authored relief.
struct SubPatch {
    x0: f32,
    z0: f32,
    corners: [f32; 4],
}

impl SubPatch {
    fn of(world: &World, cell: CellPos, sub_x: i32, sub_z: i32) -> Self {
        Self {
            x0: cell.x as f32 + sub_x as f32 / SUB as f32,
            z0: cell.z as f32 + sub_z as f32 / SUB as f32,
            corners: world.subcell_corner_heights(cell, sub_x, sub_z),
        }
    }

    /// The subcell of `cell` containing `(wx, wz)`, coordinates clamped to
    /// the cell span so an off-cell caller reads the nearest edge subcell —
    /// the same selection [`World::surface_height_in`] makes.
    fn containing(world: &World, cell: CellPos, wx: f32, wz: f32) -> Self {
        let sub = SUB as f32;
        let local_x = ((wx - cell.x as f32) * sub).clamp(0.0, sub);
        let local_z = ((wz - cell.z as f32) * sub).clamp(0.0, sub);
        let sub_x = floor_to_i32(local_x).clamp(0, SUB - 1);
        let sub_z = floor_to_i32(local_z).clamp(0, SUB - 1);
        Self::of(world, cell, sub_x, sub_z)
    }

    fn y(&self, wx: f32, wz: f32) -> f32 {
        let sub = SUB as f32;
        let fx = ((wx - self.x0) * sub).clamp(0.0, 1.0);
        let fz = ((wz - self.z0) * sub).clamp(0.0, 1.0);
        let bottom = self.corners[0] + (self.corners[1] - self.corners[0]) * fx;
        let top = self.corners[2] + (self.corners[3] - self.corners[2]) * fx;
        bottom + (top - bottom) * fz
    }

    fn shade(&self, wx: f32, wz: f32) -> f32 {
        let sub = SUB as f32;
        let fx = ((wx - self.x0) * sub).clamp(0.0, 1.0);
        let fz = ((wz - self.z0) * sub).clamp(0.0, 1.0);
        let c = &self.corners;
        // The corner deltas span 1/SUB m, so scale the gradient by SUB to
        // read the slope per meter the whole-cell patch reports.
        let grad_x = ((c[1] - c[0]) * (1.0 - fz) + (c[3] - c[2]) * fz) * sub;
        let grad_z = ((c[2] - c[0]) * (1.0 - fx) + (c[3] - c[1]) * fx) * sub;
        let normal = Vec3::new(-grad_x, 1.0, -grad_z).normalize();
        (normal.dot(SLOPE_LIGHT) / SLOPE_LIGHT.y).clamp(SLOPE_SHADE_MIN, SLOPE_SHADE_MAX)
    }
}

/// The surface height and slope shade at a world point, resolved through
/// the point's own (floor) cell — the position-pure form overlay vertices
/// lift through, continuous wherever no cliff intervenes. Where the cell
/// carries authored relief the point patch resolves the subcell height;
/// otherwise the whole-cell patch is the fast path.
fn point_lift(world: &World, wx: f32, wz: f32) -> (f32, f32) {
    let cell = CellPos {
        x: floor_to_i32(wx),
        z: floor_to_i32(wz),
    };
    if world.cell_has_height_relief(cell) {
        let patch = SubPatch::containing(world, cell, wx, wz);
        return (patch.y(wx, wz), patch.shade(wx, wz));
    }
    let lift = CellLift::of(world, cell);
    (lift.y(wx, wz), lift.shade(wx, wz))
}

/// The lift for a vertex of label geometry owned by `owner`: position-pure
/// through the vertex's own (floor) cell, unless that cell stands a cliff
/// apart from the owner — then the owner's clamped patch wins, so a
/// boundary polygon at a cliff stays on its own side of the break instead
/// of stretching down the face (the skirt draws the face). On continuous
/// ground the two forms agree and the rule is invisible. A material-break
/// crossing does not come here — it carries its side as data and lifts
/// through [`anchored_lift`].
fn label_lift(world: &World, owner: CellPos, wx: f32, wz: f32) -> (f32, f32) {
    let cell = CellPos {
        x: floor_to_i32(wx),
        z: floor_to_i32(wz),
    };
    if cell != owner && world.edge_is_cliff(cell, owner) {
        if world.cell_has_height_relief(owner) {
            let patch = SubPatch::containing(world, owner, wx, wz);
            return (patch.y(wx, wz), patch.shade(wx, wz));
        }
        let lift = CellLift::of(world, owner);
        return (lift.y(wx, wz), lift.shade(wx, wz));
    }
    if world.cell_has_height_relief(cell) {
        let patch = SubPatch::containing(world, cell, wx, wz);
        return (patch.y(wx, wz), patch.shade(wx, wz));
    }
    let lift = CellLift::of(world, cell);
    (lift.y(wx, wz), lift.shade(wx, wz))
}

/// The lift for a material-break crossing whose side is known as **data**:
/// `anchor_oct` is the display-grid sample (a window corner) on the
/// vertex's own side of the crossed edge, and over authored relief the
/// vertex lifts through that sample's subcell patch — so the high flap
/// holds its plate, the low flap holds the ground, and the marched wall
/// closes the gap on the same anchors. The side is never inferred from the
/// vertex position: a smoothing-displaced crossing sits off the subcell
/// lattice, where positional inference reads whichever subcell the vertex
/// floors into and collapses both flaps onto one plate. A crossing lies
/// within half a sample of its anchor, so the anchored patch never
/// extrapolates. Off relief the anchor is unused — the owner-clamp and
/// whole-cell paths are exactly [`label_lift`]'s, so relief-free worlds are
/// byte-identical.
fn anchored_lift(
    world: &World,
    owner: CellPos,
    anchor_oct: [i32; 2],
    wx: f32,
    wz: f32,
) -> (f32, f32) {
    let cell = CellPos {
        x: floor_to_i32(wx),
        z: floor_to_i32(wz),
    };
    if cell != owner && world.edge_is_cliff(cell, owner) {
        if world.cell_has_height_relief(owner) {
            let patch = SubPatch::containing(world, owner, wx, wz);
            return (patch.y(wx, wz), patch.shade(wx, wz));
        }
        let lift = CellLift::of(world, owner);
        return (lift.y(wx, wz), lift.shade(wx, wz));
    }
    if world.cell_has_height_relief(cell) {
        let anchor_cell = CellPos {
            x: anchor_oct[0].div_euclid(256),
            z: anchor_oct[1].div_euclid(256),
        };
        let sub_x = anchor_oct[0].rem_euclid(256) / OCTIMETERS_PER_SUBCELL;
        let sub_z = anchor_oct[1].rem_euclid(256) / OCTIMETERS_PER_SUBCELL;
        let patch = SubPatch::of(world, anchor_cell, sub_x, sub_z);
        return (patch.y(wx, wz), patch.shade(wx, wz));
    }
    let lift = CellLift::of(world, cell);
    (lift.y(wx, wz), lift.shade(wx, wz))
}

/// The break lines crossing a window's interior, per axis, in octimeters:
/// `Some(midline)` when the window's corner-sample point levels split past
/// the step ceiling across that axis. Levels differ across an axis only
/// when the two sample columns (rows) sit in different subcells, so a
/// returned midline is always a subcell lattice line — the line the break
/// stands on and the relief walls loft from. A flap polygon spanning a
/// returned line must be clipped there ([`emit_clipped_flap`]): a cap fan
/// must never connect vertices whose plates split.
fn window_break_lines(
    world: &World,
    owner: CellPos,
    x_lo: i32,
    z_lo: i32,
    step_oct: i32,
) -> (Option<i32>, Option<i32>) {
    if !world.cell_has_height_relief(owner) {
        return (None, None);
    }
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
    let mut colors: Option<([f32; 3], [f32; 3])> = None;
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
                    .0
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
                let (top_rgb, base_rgb) = *colors.get_or_insert_with(|| {
                    let cliff = world.cliff_material(owner);
                    let resolved = resolve_cell(
                        styles.get(cliff),
                        owner.x as f32 + 0.5,
                        owner.z as f32 + 0.5,
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
                });
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
                    top_rgb,
                    base_rgb,
                );
            }
        }
    }
}

/// The lift for a vertex of a clipped flap fragment: position-pure through
/// the vertex's own (floor) subcell, except that a vertex lying exactly on
/// a clipped break line reads the subcell on the **fragment's** side of it
/// (`sides` per axis, from [`emit_clipped_flap`]) — the high fragment holds
/// its plate, the low fragment holds the ground, and the wall classes close
/// the vertical gap on the same subcells. Off the break lines the two forms
/// agree wherever the plates connect, so continuous relief stays seamless.
/// The owner-clamp and relief-free paths are exactly [`label_lift`]'s, so
/// cell-cliff and relief-free worlds are untouched.
fn fragment_lift(
    world: &World,
    owner: CellPos,
    sides: [Option<(i32, bool)>; 2],
    wx: f32,
    wz: f32,
) -> (f32, f32) {
    let cell = CellPos {
        x: floor_to_i32(wx),
        z: floor_to_i32(wz),
    };
    if cell != owner && world.edge_is_cliff(cell, owner) {
        if world.cell_has_height_relief(owner) {
            let patch = SubPatch::containing(world, owner, wx, wz);
            return (patch.y(wx, wz), patch.shade(wx, wz));
        }
        let lift = CellLift::of(world, owner);
        return (lift.y(wx, wz), lift.shade(wx, wz));
    }
    if world.cell_has_height_relief(cell) {
        // Subcell per axis: the fragment's side when the coordinate sits
        // exactly on that axis's clipped break line (always a subcell
        // lattice line), else the floor subcell.
        let sub_of = |w: f32, side: Option<(i32, bool)>| -> i32 {
            if let Some((line, above)) = side {
                let oct = w * OCTIMETERS_PER_METER;
                if (oct - line as f32).abs() < 0.5 {
                    let lattice = line / OCTIMETERS_PER_SUBCELL;
                    return if above { lattice } else { lattice - 1 };
                }
            }
            floor_to_i32(w * SUB as f32)
        };
        let sx = sub_of(wx, sides[0]);
        let sz = sub_of(wz, sides[1]);
        let patch = SubPatch::of(
            world,
            CellPos {
                x: sx.div_euclid(SUB),
                z: sz.div_euclid(SUB),
            },
            sx.rem_euclid(SUB),
            sz.rem_euclid(SUB),
        );
        return (patch.y(wx, wz), patch.shade(wx, wz));
    }
    let lift = CellLift::of(world, cell);
    (lift.y(wx, wz), lift.shade(wx, wz))
}

/// Floor to `i32` — `as i32` truncates toward zero, which is wrong for
/// negative world coordinates, so step down when it rounded up.
fn floor_to_i32(v: f32) -> i32 {
    let t = v as i32;
    if (t as f32) > v { t - 1 } else { t }
}

/// The effective point surface level in octimeters at octimeter position
/// `(px, pz)` — the cell and subcell it floors into, resolved through
/// [`World::point_surface_level`]. The marched wall gate samples this so its
/// break reads the same authored relief the cap drew.
fn point_surface_level_at(world: &World, px: i32, pz: i32) -> i32 {
    let cell = CellPos {
        x: px.div_euclid(256),
        z: pz.div_euclid(256),
    };
    let sub_x = px.rem_euclid(256) / OCTIMETERS_PER_SUBCELL;
    let sub_z = pz.rem_euclid(256) / OCTIMETERS_PER_SUBCELL;
    world.point_surface_level(cell, sub_x, sub_z)
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
/// plus its apron, plus the frozen mask — a sample flanking a point-height
/// break freezes so the smoothing passes can never move the paint boundary
/// across a physical cliff ([`repartition`]'s barrier; the contour along a
/// break stays the accepted sample-resolution staircase). `None` when the
/// whole area is Void — nothing to mesh.
fn partition_inputs(
    world: &World,
    at: ChunkPos,
    apron: i32,
    n: usize,
    styles: &StyleTable,
) -> Option<(Vec<u8>, Vec<SmoothParams>, Vec<bool>)> {
    let mut ids = vec![0u8; n * n];
    let mut params = vec![
        SmoothParams {
            iterations: 0,
            smoothing_degrees: 90,
        };
        n * n
    ];
    let mut frozen = vec![false; n * n];
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
            // Freeze the sample when its subcell flanks a point-height
            // break: paint smoothing must never cross a physical cliff, or
            // the silhouette's flank would alternate between painted steep
            // caps and wall wedges. Pure world reads (position-based), so
            // neighboring chunks agree over the shared apron. The relief
            // shortcut keeps flat neighborhoods off this path entirely.
            if world.cell_has_height_relief(cell) {
                let gx = at.x * EDGE * SUB + si;
                let gz = at.z * EDGE * SUB + sj;
                let own_level = point_surface_level_at(
                    world,
                    gx * OCTIMETERS_PER_SUBCELL,
                    gz * OCTIMETERS_PER_SUBCELL,
                );
                frozen[idx] =
                    [(1, 0), (-1, 0), (0, 1), (0, -1)]
                        .into_iter()
                        .any(|(dx, dz): (i32, i32)| {
                            let level = point_surface_level_at(
                                world,
                                (gx + dx) * OCTIMETERS_PER_SUBCELL,
                                (gz + dz) * OCTIMETERS_PER_SUBCELL,
                            );
                            (own_level - level).abs() > STEP_MAX_OCTIMETERS
                        });
            }
        }
    }
    any.then_some((ids, params, frozen))
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
#[allow(clippy::too_many_lines)] // one underlay pass: partition, split, tile interiors, march windows, flap margins
fn mesh_underlay(
    world: &World,
    at: ChunkPos,
    styles: &StyleTable,
    tris: &mut Vec<DrawTriangle>,
) -> Option<DisplayPartition> {
    let apron = MAX_APRON_SUBCELLS;
    let n = (SUBCELLS_PER_CHUNK_EDGE + 2 * apron) as usize;
    let (ids, params, frozen) = partition_inputs(world, at, apron, n, styles)?;

    let upsample = CONTOUR_UPSAMPLE;
    let (grid, gw, gh) = repartition(&ids, n, n, upsample, &params, &frozen);
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
            // Authored relief tessellates the cap to subcell resolution so
            // its per-point heights and breaks show; a flat cell keeps the
            // nine-slice fast path (byte-identical to a world with no relief).
            if world.cell_has_height_relief(cell) {
                emit_underlay_cell_subdivided(world, material, &resolved, cell, styles, tris);
                continue;
            }
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

    // The fill-over floor caps for enclosed Void joints — the flat groove
    // bottoms the marched closure's Void walls drop to.
    emit_void_floors(world, at, styles, tris);

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
            // there). A uniform window straddling a point-height break is
            // the exception: its quad would bridge the split plates, so it
            // routes through the mixed emitter, whose break clipping splits
            // it (the label's case is 15 — the full window polygon).
            if corners[0] != 0 && corners.iter().all(|&c| c == corners[0]) {
                let (bx, bz) = window_break_lines(world, owner, x_lo, z_lo, step_oct);
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
    let half = step_oct / 2;
    let x_lo = place.origin_oct[0] + wi as i32 * step_oct;
    let z_lo = place.origin_oct[1] + wj as i32 * step_oct;
    let x_hi = x_lo + step_oct;
    let z_hi = z_lo + step_oct;
    let wash_x = (x_lo + half) as f32 / OCTIMETERS_PER_METER;
    let wash_z = (z_lo + half) as f32 / OCTIMETERS_PER_METER;
    // The break midlines a flap must be clipped along — a fan across a
    // split would draw a slanted bridge over the break instead of letting
    // the walls close it.
    let (clip_x, clip_z) = window_break_lines(world, owner, x_lo, z_lo, step_oct);
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
        let material = Material::from_u8_or_void(label.div_ceil(2));
        let s = styles.get(material);
        let depth = (material == Material::Water).then(|| owner_water_depth(world, owner));
        let resolved = resolve_cell(s, owner.x as f32 + 0.5, owner.z as f32 + 0.5, depth);
        let mut light = wash_lightness(s, resolved.light, wash_x, wash_z, resolved.stroke);
        if label % 2 == 1 {
            light *= 1.0 - s.rim_darken;
        }
        let vertex = |pos: [f32; 2], sides: [Option<(i32, bool)>; 2]| {
            let wx = pos[0] / OCTIMETERS_PER_METER;
            let wz = pos[1] / OCTIMETERS_PER_METER;
            let (y, shade) = fragment_lift(world, owner, sides, wx, wz);
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

/// The effective surface level in octimeters at grid sample `(gx, gz)`,
/// resolved at subcell — not cell — granularity, so an authored point-height
/// break inside a cell reads as a step just as a cell-scale cliff does. The
/// encroachment flood tests this between adjacent samples so a margin wraps
/// around a subcell break rather than climbing or draping it. Water resolves
/// at its flat plane here.
fn sample_surface_level(world: &World, place: &GridPlacement, gx: i32, gz: i32) -> i32 {
    let sx = (place.origin_oct[0] + gx * place.step_oct).div_euclid(OCTIMETERS_PER_SUBCELL);
    let sz = (place.origin_oct[1] + gz * place.step_oct).div_euclid(OCTIMETERS_PER_SUBCELL);
    world.point_surface_level(
        CellPos {
            x: sx.div_euclid(SUB),
            z: sz.div_euclid(SUB),
        },
        sx.rem_euclid(SUB),
        sz.rem_euclid(SUB),
    )
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
            let here = sample_surface_level(world, place, gx, gz);
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
                // A cliff blocks the flood — cell-scale or an authored
                // subcell break — so a margin wraps around a step rather than
                // climbing or draping it.
                let there = sample_surface_level(world, place, nx, nz);
                if (here - there).abs() > STEP_MAX_OCTIMETERS {
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
            // A flap must not drape a height break. Where the window's
            // footprint straddles a subcell cliff — the effective surface
            // level a step apart across it — the margin stops at the break
            // edge rather than lofting a tall face down the wall the relief
            // pass already seals. Water resolves at its flat plane here, so a
            // shoreline flap still rides it; only an authored point-height
            // step trips the gate.
            let corner_level = |ox: i32, oz: i32| {
                let sx = ox.div_euclid(OCTIMETERS_PER_SUBCELL);
                let sz = oz.div_euclid(OCTIMETERS_PER_SUBCELL);
                world.point_surface_level(
                    CellPos {
                        x: sx.div_euclid(SUB),
                        z: sz.div_euclid(SUB),
                    },
                    sx.rem_euclid(SUB),
                    sz.rem_euclid(SUB),
                )
            };
            let x_hi = x_lo + step_oct;
            let z_hi = z_lo + step_oct;
            let (lo_level, hi_level) = [
                corner_level(x_lo, z_lo),
                corner_level(x_hi, z_lo),
                corner_level(x_lo, z_hi),
                corner_level(x_hi, z_hi),
            ]
            .into_iter()
            .fold((i32::MAX, i32::MIN), |(lo, hi), l| (lo.min(l), hi.max(l)));
            if hi_level - lo_level > STEP_MAX_OCTIMETERS {
                continue;
            }
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
            let vertex = |wx: f32, wz: f32, _point: u8| {
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

/// Emit a height-relief cell's cap as `SUB × SUB` subcell quads, each lifted
/// through its own point patch ([`SubPatch`]) so authored subcell relief —
/// and the breaks where adjacent points cliff — shows in the cap and the
/// drawn height matches [`World::surface_height`] at every sample. Pooled
/// rims are dropped here (relief shaping owns the cell); a flat cell keeps
/// the nine-slice fast path in [`emit_underlay_cell`].
fn emit_underlay_cell_subdivided(
    world: &World,
    material: Material,
    resolved: &ResolvedCell,
    cell: CellPos,
    styles: &StyleTable,
    tris: &mut Vec<DrawTriangle>,
) {
    for sj in 0..SUB {
        for si in 0..SUB {
            let patch = SubPatch::of(world, cell, si, sj);
            let surface = |wx: f32, wz: f32| (patch.y(wx, wz), patch.shade(wx, wz));
            let x0 = cell.x * 256 + si * OCTIMETERS_PER_SUBCELL;
            let z0 = cell.z * 256 + sj * OCTIMETERS_PER_SUBCELL;
            emit_quad_shaded(
                material,
                resolved,
                [
                    x0,
                    z0,
                    x0 + OCTIMETERS_PER_SUBCELL,
                    z0 + OCTIMETERS_PER_SUBCELL,
                ],
                0.0,
                &surface,
                styles,
                tris,
            );
        }
    }
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

/// Emit the chunk's vertical cliff faces by closure: a wall lofts wherever
/// the two sides of a shared boundary edge committed different cap heights,
/// so the split decision lives in one place — the plate lift the caps drew
/// from — and can never drift between a predicting wall pass and the cap it
/// should meet. The lattice/subcell closure ([`emit_lattice_closure`]) walls
/// every same-material cell-edge and subcell-edge break; the marched closure
/// ([`emit_contour_closure`], painted only) walls every material or Void
/// boundary along the display partition's contours, dropping a fillable Void
/// low side to its groove floor. A face lofts iff the high side committed
/// strictly above the low side there — a legal step merges the plates (the
/// committed edges agree) and lofts nothing, a cliff splits them and lofts a
/// face whose top and bottom vertices are the two caps' shared-edge vertices,
/// watertight by construction. The raw calibration view has no partition, so
/// it closes every cliff at cell resolution in flat gray and runs neither the
/// subcell nor the marched closure.
fn emit_walls(
    world: &World,
    at: ChunkPos,
    mode: ViewMode,
    styles: &StyleTable,
    partition: Option<&DisplayPartition>,
    tris: &mut Vec<DrawTriangle>,
) {
    let painted = partition.is_some();
    emit_lattice_closure(world, at, mode, painted, styles, tris);
    if let Some(part) = partition {
        emit_contour_closure(world, at, part, styles, tris);
    }
}

/// The top / base wall shades for a cliff owned by `cell`: the raw view's
/// flat calibration gray, or the painted view's cliff material keyed at the
/// cell center and graded top-to-base toward the ground shadow. A pure
/// function of the cell, so the two sides of a shared edge agree on the color.
fn wall_shades(
    world: &World,
    cell: CellPos,
    mode: ViewMode,
    styles: &StyleTable,
) -> ([f32; 3], [f32; 3]) {
    match mode {
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
    }
}

/// Close the same-material cliff faces of the chunk on the cell lattice — or,
/// where a cell carries authored relief in the painted view, on its subcell
/// lattice, the stride its cap tessellated at. For every chunk-local cell and
/// each of its four outgoing shared edges the two sides' committed cap edges
/// are read through the same corner plates the caps drew from
/// ([`World::cell_corner_heights`] / [`World::subcell_corner_heights`]); a
/// face lofts iff this (the higher) side committed above the neighbor and the
/// committed edges differ. A merged plate — flat ground or a legal step —
/// leaves the edges equal and lofts nothing, so the step size is never gated
/// directly; the plate merge is the one split decision. The high side owns
/// the face and cells iterate chunk-local, so a chunk-border cliff lofts
/// exactly once fleet-wide. In the painted view a material or Void boundary
/// edge is skipped here — the marched closure owns those; the raw view has no
/// partition and closes every cell-edge cliff in flat gray.
#[allow(clippy::too_many_lines)] // one closure walk over the cell and subcell strides
fn emit_lattice_closure(
    world: &World,
    at: ChunkPos,
    mode: ViewMode,
    painted: bool,
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
            let material = world.underlay(cell);
            if material == Material::Void {
                continue; // a Void cell's rim is the marched closure's face
            }
            // Painted relief cells close on the subcell lattice (the stride
            // their caps tessellated at); every other cell — and the whole
            // raw view — closes on the cell lattice.
            let relief = painted && world.cell_has_height_relief(cell);
            let (top_rgb, base_rgb) = wall_shades(world, cell, mode, styles);
            if relief {
                for sj in 0..SUB {
                    for si in 0..SUB {
                        if world.underlay_point(cell, si, sj) != material {
                            continue; // a hole / material rim lofts as a marched wall
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
                            if (y_top[0] - y_low[0]).abs() < f32::EPSILON
                                && (y_top[1] - y_low[1]).abs() < f32::EPSILON
                            {
                                continue; // the point plates merged — no break to close
                            }
                            let base_x = cell.x as f32 + si as f32 * sub_span;
                            let base_z = cell.z as f32 + sj as f32 * sub_span;
                            let corner_pos = |k: usize| {
                                (
                                    base_x + if k % 2 == 1 { sub_span } else { 0.0 },
                                    base_z + if k >= 2 { sub_span } else { 0.0 },
                                )
                            };
                            let (x0, z0) = corner_pos(edge.top[0]);
                            let (x1, z1) = corner_pos(edge.top[1]);
                            push_wall_quad(
                                tris,
                                [x0, z0, y_top[0]],
                                [x1, z1, y_top[1]],
                                y_low[0],
                                y_low[1],
                                top_rgb,
                                base_rgb,
                            );
                        }
                    }
                }
            } else {
                let cell_level = world.surface_level(cell);
                let mut cached: Option<[f32; 4]> = None;
                for edge in &WALL_DIRECTIONS {
                    let neighbor = CellPos {
                        x: cell.x + edge.offset.0,
                        z: cell.z + edge.offset.1,
                    };
                    if painted && world.underlay(neighbor) != material {
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
                    if (y_top[0] - y_low[0]).abs() < f32::EPSILON
                        && (y_top[1] - y_low[1]).abs() < f32::EPSILON
                    {
                        continue; // the plates merged — no break to close
                    }
                    let corner_pos = |k: usize| {
                        (
                            cell.x as f32 + if k % 2 == 1 { 1.0 } else { 0.0 },
                            cell.z as f32 + if k >= 2 { 1.0 } else { 0.0 },
                        )
                    };
                    let (x0, z0) = corner_pos(edge.top[0]);
                    let (x1, z1) = corner_pos(edge.top[1]);
                    push_wall_quad(
                        tris,
                        [x0, z0, y_top[0]],
                        [x1, z1, y_top[1]],
                        y_low[0],
                        y_low[1],
                        top_rgb,
                        base_rgb,
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
            // Wall color: the owning cell's cliff material, top-to-base
            // shaded — the owner is the color source even when the segment's
            // high side lies in a neighboring cell.
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
                    let yt_p = anchored_lift(world, owner, hi_anchor_p, wx_p, wz_p).0;
                    let yt_q = anchored_lift(world, owner, hi_anchor_q, wx_q, wz_q).0;
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
                            anchored_lift(world, owner, lo_anchor_p, wx_p, wz_p).0,
                            anchored_lift(world, owner, lo_anchor_q, wx_q, wz_q).0,
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
                        let step_m = STEP_MAX_OCTIMETERS as f32 / OCTIMETERS_PER_METER;
                        if (yt_p - yb_p).max(yt_q - yb_q) <= step_m {
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
                        top_rgb,
                        base_rgb,
                    );
                }
            }
        }
    }
}

/// The base a Void low side closes to at a marched segment endpoint: the
/// void point's stored floor height ([`World::point_height`] — the height
/// plane is total, so a void point still carries a height, void being only a
/// material flag) when the joint is enclosed by solid within the fill-over
/// reach ([`void_fill_border`]), so wall / floor / wall reads as a real
/// flat-bottomed groove; else `yt` dropped by the unbounded-void skirt, the
/// one case with no far rim within the bound.
fn void_low_base(world: &World, anchor_oct: [i32; 2], yt: f32) -> f32 {
    let cell = CellPos {
        x: anchor_oct[0].div_euclid(256),
        z: anchor_oct[1].div_euclid(256),
    };
    let sub_x = anchor_oct[0].rem_euclid(256) / OCTIMETERS_PER_SUBCELL;
    let sub_z = anchor_oct[1].rem_euclid(256) / OCTIMETERS_PER_SUBCELL;
    let gx = cell.x * SUB + sub_x;
    let gz = cell.z * SUB + sub_z;
    if world.underlay_point(cell, sub_x, sub_z) == Material::Void
        && void_fill_border(world, gx, gz).is_some()
    {
        return world.point_height(cell, sub_x, sub_z) as f32 / OCTIMETERS_PER_METER;
    }
    yt - WALL_VOID_SKIRT_OCTIMETERS as f32 / OCTIMETERS_PER_METER
}

/// The color source for a Void point's fill-over floor, or `None` when the
/// point is not enclosed. The void point at global subcell `(gx0, gz0)` is
/// enclosed when every one of the four axis directions reaches a solid point
/// within [`MAX_APRON_SUBCELLS`] subcells — a bounded march past void; the
/// returned cell is the closest such solid point's, whose `cliff_material`
/// colors the groove. A direction that runs to the bound still in void means
/// the void reaches the world border within reach: an open skirt, no floor.
/// The bound is the `R = 1` remesh read reach, so the march never reads past
/// what invalidation covers.
fn void_fill_border(world: &World, gx0: i32, gz0: i32) -> Option<CellPos> {
    let mut nearest: Option<(i32, CellPos)> = None;
    for (dx, dz) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
        let mut hit = false;
        for step in 1..=MAX_APRON_SUBCELLS {
            let gx = gx0 + dx * step;
            let gz = gz0 + dz * step;
            let cell = CellPos {
                x: gx.div_euclid(SUB),
                z: gz.div_euclid(SUB),
            };
            if world.underlay_point(cell, gx.rem_euclid(SUB), gz.rem_euclid(SUB)) != Material::Void
            {
                if nearest.is_none_or(|(d, _)| step < d) {
                    nearest = Some((step, cell));
                }
                hit = true;
                break;
            }
        }
        if !hit {
            return None; // open toward this side within reach — keep the skirt
        }
    }
    nearest.map(|(_, cell)| cell)
}

/// Emit the fill-over floor caps: every chunk-local Void point that
/// [`void_fill_border`] proves enclosed floors over at its stored point
/// height in the bordering cell's cliff material at base shade — the
/// bottom-of-groove shadow. The rim walls closing the groove's sides are the
/// marched closure's Void faces, dropped to this same stored height
/// ([`void_low_base`]), so the floor and its walls meet watertight. Painted
/// only; the raw calibration view has no partition and no fill-over. A cell
/// with no Void point emits nothing, so a solid world stays byte-identical.
fn emit_void_floors(
    world: &World,
    at: ChunkPos,
    styles: &StyleTable,
    tris: &mut Vec<DrawTriangle>,
) {
    for lz in 0..EDGE {
        for lx in 0..EDGE {
            let cell = CellPos {
                x: at.x * EDGE + lx,
                z: at.z * EDGE + lz,
            };
            for sj in 0..SUB {
                for si in 0..SUB {
                    if world.underlay_point(cell, si, sj) != Material::Void {
                        continue;
                    }
                    let gx = cell.x * SUB + si;
                    let gz = cell.z * SUB + sj;
                    let Some(border) = void_fill_border(world, gx, gz) else {
                        continue; // open skirt — no floor
                    };
                    let y = world.point_height(cell, si, sj) as f32 / OCTIMETERS_PER_METER;
                    let cliff = world.cliff_material(border);
                    let resolved = resolve_cell(
                        styles.get(cliff),
                        border.x as f32 + 0.5,
                        border.z as f32 + 0.5,
                        None,
                    );
                    let rgb = hsl_to_linear_rgb(
                        resolved.hue,
                        resolved.sat,
                        (resolved.light * SKIRT_BASE_SHADE).clamp(0.0, 100.0),
                    );
                    let x0 = cell.x * 256 + si * OCTIMETERS_PER_SUBCELL;
                    let z0 = cell.z * 256 + sj * OCTIMETERS_PER_SUBCELL;
                    let vertex = |wx: f32, wz: f32| Vertex {
                        x: wx,
                        y,
                        z: wz,
                        r: rgb[0],
                        g: rgb[1],
                        b: rgb[2],
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
    use alloc::collections::BTreeMap;

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

    #[test]
    fn drawn_relief_equals_stood_on() {
        // drawn≡stood-on over authored subcell relief: a continuous per-point
        // hill (deltas within the step ceiling, returning to zero at the chunk
        // edges) lofts no walls, and every cap vertex sits exactly on
        // `World::surface_height` — the same subcell patch a mover stands on.
        let at = ChunkPos { x: 0, z: 0 };
        let mut world = World::new();
        let mut chunk = Chunk::empty();
        chunk.underlay = [Material::Grass; CELLS_PER_CHUNK_AREA];
        world.insert_chunk(at, chunk);
        let sub = SUB as usize;
        // A pyramid: 12 octimeters per subcell up to the middle and back down
        // on both axes, flat (zero) by every chunk edge so the grass never
        // stands above the Void surround and nothing cliffs anywhere.
        let ramp = |g: i32| (12 * (g.min(60 - g)).max(0)) as i16;
        for lz in 0..EDGE {
            for lx in 0..EDGE {
                let mut deltas = [0i16; SUBCELLS_PER_CELL];
                for sz in 0..sub {
                    for sx in 0..sub {
                        let dx = ramp(lx * SUB + sx as i32);
                        let dz = ramp(lz * SUB + sz as i32);
                        deltas[sz * sub + sx] = dx.min(dz);
                    }
                }
                world.set_cell_heights(CellPos { x: lx, z: lz }, &deltas);
            }
        }
        let mesh = mesh_chunk(&world, at, ViewMode::Painted, &StyleTable::default());
        assert!(!mesh.is_empty(), "the relief cell meshes caps");
        assert!(
            mesh.iter().all(|t| y_span(t) < 0.5),
            "a continuous hill lofts no walls",
        );
        let mut raised = false;
        for v in mesh.iter().flat_map(|t| t.verts.iter()) {
            let surface = world.surface_height(v.x, v.z);
            assert!(
                (v.y - surface).abs() < 2e-3,
                "cap vertex ({}, {}) drawn at {} but stood on {surface}",
                v.x,
                v.z,
                v.y,
            );
            raised |= v.y > 1.0; // the ridge peaks near 360/256 ≈ 1.4 m
        }
        assert!(raised, "the authored relief actually lifts the cap");
    }

    /// Author a stone diamond (rotated square) at point resolution centered on
    /// subcell `(34, 34)` with subcell radius `r`, raised `lift` octimeters on
    /// a Void surround; the diamond's edges cross cell interiors diagonally.
    fn diamond_world(r: i32, lift: i16) -> World {
        let at = ChunkPos { x: 0, z: 0 };
        let mut world = World::new();
        world.insert_chunk(at, Chunk::empty()); // all-Void underlay
        let sub = SUB as usize;
        for lz in 0..EDGE {
            for lx in 0..EDGE {
                let mut points = [Material::Void.to_u8(); SUBCELLS_PER_CELL];
                let mut deltas = [0i16; SUBCELLS_PER_CELL];
                for sz in 0..sub {
                    for sx in 0..sub {
                        let gx = lx * SUB + sx as i32;
                        let gz = lz * SUB + sz as i32;
                        if (gx - 34).abs() + (gz - 34).abs() <= r {
                            points[sz * sub + sx] = Material::Stone.to_u8();
                            deltas[sz * sub + sx] = lift;
                        }
                    }
                }
                let cell = CellPos { x: lx, z: lz };
                world.set_cell_points(cell, &points);
                world.set_cell_heights(cell, &deltas);
            }
        }
        world
    }

    #[test]
    fn an_authored_diamond_wears_walls_on_its_silhouette() {
        // A point-authored stone diamond raised 300 octimeters wears walls on
        // its marched silhouette (which crosses cell interiors diagonally),
        // with no wall on an interior cell edge between two fully-raised cells.
        let at = ChunkPos { x: 0, z: 0 };
        let world = diamond_world(12, 300);
        let mesh = mesh_chunk(&world, at, ViewMode::Painted, &StyleTable::default());
        let walls: Vec<&DrawTriangle> = mesh.iter().filter(|t| y_span(t) > 0.5).collect();
        assert!(!walls.is_empty(), "the raised diamond wears walls");

        // The silhouette departs the cell grid: some wall vertex sits strictly
        // inside a cell, off every integer lattice line.
        let departs = walls.iter().flat_map(|t| t.verts.iter()).any(|v| {
            let fx = v.x - v.x.floor();
            let fz = v.z - v.z.floor();
            (0.1..0.9).contains(&fx) && (0.1..0.9).contains(&fz)
        });
        assert!(
            departs,
            "walls follow the diagonal silhouette, not cell edges"
        );

        // No wall on the interior cell edge x = 8 between cells (7,8) and
        // (8,8) — both fully inside the diamond (the boundary crosses x = 8
        // only at z = 6 and z = 11), so a fused interior stands no wall.
        let interior_edge = walls
            .iter()
            .flat_map(|t| t.verts.iter())
            .any(|v| (v.x - 8.0).abs() < 1e-4 && (8.05..8.95).contains(&v.z));
        assert!(
            !interior_edge,
            "no wall vertex sits on an interior cell edge inside the diamond",
        );

        // And the cap actually rides the raised plane.
        let peak = mesh
            .iter()
            .filter(|t| y_span(t) < 0.5)
            .flat_map(|t| t.verts.iter())
            .map(|v| v.y)
            .fold(f32::MIN, f32::max);
        assert!(
            (peak - 300.0 / 256.0).abs() < 1e-3,
            "the diamond cap stands at the raised level, peak {peak}",
        );
    }

    #[test]
    fn fused_equal_height_points_stand_no_internal_wall() {
        // Two stone cells raised to the same per-point level fuse: the wall
        // pass closes their outer perimeter against the Void surround but
        // stands no wall on their shared edge, where the heights match.
        let at = ChunkPos { x: 0, z: 0 };
        let mut world = World::new();
        let mut chunk = Chunk::empty();
        chunk.underlay[8 * EDGE as usize + 8] = Material::Stone; // cell (8,8)
        chunk.underlay[8 * EDGE as usize + 9] = Material::Stone; // cell (9,8)
        world.insert_chunk(at, chunk);
        world.set_cell_heights(CellPos { x: 8, z: 8 }, &[300; SUBCELLS_PER_CELL]);
        world.set_cell_heights(CellPos { x: 9, z: 8 }, &[300; SUBCELLS_PER_CELL]);

        let mesh = mesh_chunk(&world, at, ViewMode::Painted, &StyleTable::default());
        let walls: Vec<&DrawTriangle> = mesh.iter().filter(|t| y_span(t) > 0.5).collect();
        assert!(
            !walls.is_empty(),
            "the fused pair wears an outer perimeter wall"
        );
        let shared_edge = walls
            .iter()
            .flat_map(|t| t.verts.iter())
            .any(|v| (v.x - 9.0).abs() < 1e-4 && (8.05..8.95).contains(&v.z));
        assert!(
            !shared_edge,
            "no wall stands on the fused pair's equal-height shared edge",
        );
    }

    #[test]
    fn adjacent_chunks_agree_on_an_authored_break() {
        // A stone shelf (raised 300, rows z < 8) meets grass ground (rows
        // z >= 8) along a break running in x across the vertical chunk seam at
        // x = 16. The marched wall pass reads point levels — pure functions of
        // world position — so each chunk lofts its half and the two meet on
        // identical vertices at the shared seam column.
        let mut world = World::new();
        for cx in 0..2 {
            let mut chunk = Chunk::empty();
            for lz in 0..EDGE {
                for lx in 0..EDGE {
                    let i = (lz * EDGE + lx) as usize;
                    chunk.underlay[i] = if lz < 8 {
                        Material::Stone
                    } else {
                        Material::Grass
                    };
                }
            }
            world.insert_chunk(ChunkPos { x: cx, z: 0 }, chunk);
        }
        for cx in 0..2 {
            for lz in 0..8 {
                for lx in 0..EDGE {
                    world.set_cell_heights(
                        CellPos {
                            x: cx * EDGE + lx,
                            z: lz,
                        },
                        &[300; SUBCELLS_PER_CELL],
                    );
                }
            }
        }
        // Marched wall vertices land on subcell sample lines, not the exact
        // chunk edge, so agreement is that the two chunks emit an identical
        // vertex where their seam-straddling windows share an edge — a
        // non-empty intersection of their wall-vertex sets, near the seam.
        // The z band pins the collection to the authored z = 8 break line
        // (the shelf's outer Void-drop walls are a different feature).
        let wall_verts = |at| {
            let mesh = mesh_chunk(&world, at, ViewMode::Painted, &StyleTable::default());
            let mut verts: Vec<(i64, i64, i64)> = mesh
                .iter()
                .filter(|t| y_span(t) > 0.5)
                .flat_map(|t| t.verts.iter())
                .filter(|v| (7.5..8.5).contains(&v.z))
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
        let west = wall_verts(ChunkPos { x: 0, z: 0 });
        let east = wall_verts(ChunkPos { x: 1, z: 0 });
        assert!(
            !west.is_empty() && !east.is_empty(),
            "each chunk lofts the break"
        );
        let shared: Vec<_> = west.iter().filter(|v| east.contains(v)).collect();
        assert!(
            !shared.is_empty(),
            "the two chunks meet on identical seam vertices",
        );
        // The shared vertices sit at the seam column and the raised top level.
        assert!(
            shared
                .iter()
                .any(|v| (v.0 - 16 * 256).abs() <= 32 && v.1 == 300),
            "the shared seam wall stands at the authored break level",
        );
    }

    #[test]
    fn a_pure_delta_plateau_stands_walls_on_its_break_lines() {
        // The same-material height-break class: a stone field at uniform cell
        // height wears a plateau authored purely with point deltas — no
        // material boundary anywhere (nothing for the marched pass) and no
        // cell-height cliff (nothing for the cell-edge walk) — so every wall
        // must come from the subcell relief walk, standing exactly on the
        // plateau's break lines. Those lines run through cell interiors
        // (x and z = 7.5 / 9.5 m), where a cell-edge wall cannot stand.
        let at = ChunkPos { x: 0, z: 0 };
        let mut world = World::new();
        for dz in -1..=1 {
            for dx in -1..=1 {
                let mut c = Chunk::empty();
                c.underlay = [Material::Stone; CELLS_PER_CHUNK_AREA];
                world.insert_chunk(ChunkPos { x: dx, z: dz }, c);
            }
        }
        // Raise global subcells [30, 38) x [30, 38) by 300 octimeters.
        let sub = SUB as usize;
        for lz in 6..10 {
            for lx in 6..10 {
                let mut deltas = [0i16; SUBCELLS_PER_CELL];
                for sz in 0..sub {
                    for sx in 0..sub {
                        let gx = lx * SUB + sx as i32;
                        let gz = lz * SUB + sz as i32;
                        if (30..38).contains(&gx) && (30..38).contains(&gz) {
                            deltas[sz * sub + sx] = 300;
                        }
                    }
                }
                world.set_cell_heights(CellPos { x: lx, z: lz }, &deltas);
            }
        }
        let mesh = mesh_chunk(&world, at, ViewMode::Painted, &StyleTable::default());
        let walls: Vec<&DrawTriangle> = mesh.iter().filter(|t| y_span(t) > 0.5).collect();
        assert!(!walls.is_empty(), "the delta plateau wears walls");
        // Every wall vertex lies on the block's perimeter lines — the
        // authored break, nowhere else (no fused-interior or cell-edge wall).
        let lo = 7.5f32;
        let hi = 9.5f32;
        for v in walls.iter().flat_map(|t| t.verts.iter()) {
            let on_x = ((v.x - lo).abs() < 1e-4 || (v.x - hi).abs() < 1e-4)
                && (lo - 1e-4..=hi + 1e-4).contains(&v.z);
            let on_z = ((v.z - lo).abs() < 1e-4 || (v.z - hi).abs() < 1e-4)
                && (lo - 1e-4..=hi + 1e-4).contains(&v.x);
            assert!(
                on_x || on_z,
                "wall vertex ({}, {}) stands off the authored break lines",
                v.x,
                v.z,
            );
        }
        // And all four perimeter sides are closed.
        for (side, pick) in [
            (
                "west",
                &(|v: &Vertex| (v.x - lo).abs() < 1e-4) as &dyn Fn(&Vertex) -> bool,
            ),
            ("east", &|v: &Vertex| (v.x - hi).abs() < 1e-4),
            ("north", &|v: &Vertex| (v.z - lo).abs() < 1e-4),
            ("south", &|v: &Vertex| (v.z - hi).abs() < 1e-4),
        ] {
            assert!(
                walls.iter().flat_map(|t| t.verts.iter()).any(pick),
                "the plateau's {side} side is open",
            );
        }
    }

    #[test]
    fn a_relief_cell_on_a_cell_cliff_lofts_its_wall_exactly_once() {
        // A relief cell standing a plain cell-height cliff above a
        // same-material neighbor: the cell-edge walk skips relief cells and
        // the subcell relief walk owns the face, so the shared edge must be
        // covered exactly once — the wall segments' top edges sum to the one
        // cell-edge meter. Double emission (both walks firing) would sum to
        // two; a routing hole would sum to zero.
        let at = ChunkPos { x: 0, z: 0 };
        let mut world = World::new();
        let mut chunk = Chunk::empty();
        chunk.underlay = [Material::Grass; CELLS_PER_CHUNK_AREA];
        chunk.height[(8 * EDGE + 8) as usize] = 256; // cell (8,8) one meter up
        world.insert_chunk(at, chunk);
        // A small legal-step delta inside the raised cell flips it (and its
        // neighbors) onto the point path without adding any further break.
        let mut deltas = [0i16; SUBCELLS_PER_CELL];
        deltas[SUBCELLS_PER_CELL - 1] = 16;
        world.set_cell_heights(CellPos { x: 8, z: 8 }, &deltas);

        let mesh = mesh_chunk(&world, at, ViewMode::Painted, &StyleTable::default());
        // Wall triangles on the west edge plane x = 8 over cell (8,8)'s span.
        let west: Vec<&DrawTriangle> = mesh
            .iter()
            .filter(|t| {
                y_span(t) > 0.5
                    && t.verts.iter().all(|v| {
                        (v.x - 8.0).abs() < 1e-4 && (8.0 - 1e-4..=9.0 + 1e-4).contains(&v.z)
                    })
            })
            .collect();
        assert!(!west.is_empty(), "the cliff edge carries a wall");
        // Each wall quad's top edge appears on exactly one of its two
        // triangles (the one holding both top vertices); summing those top
        // spans measures the edge coverage.
        let top_length: f32 = west
            .iter()
            .map(|t| {
                let tops: Vec<&Vertex> = t.verts.iter().filter(|v| v.y > 0.5).collect();
                if tops.len() == 2 {
                    (tops[0].z - tops[1].z).abs()
                } else {
                    0.0
                }
            })
            .sum();
        assert!(
            (top_length - 1.0).abs() < 1e-3,
            "the shared edge is covered exactly once, got {top_length} m of top edge",
        );
    }

    /// The authored-silhouette fixture from the causeway demo failure: a
    /// stone square raised purely by point deltas over flat grass ground —
    /// materials and heights break on the same subcell lines (global
    /// subcells `[30, 38)²`, i.e. `[7.5, 9.5)` m), every cell height zero.
    /// Smoothing is pinned off through the per-cell override plane so the
    /// marched contour is exactly the authored break line.
    fn delta_column_world() -> World {
        let mut world = World::new();
        world.insert_smoothing_profile(
            1,
            SmoothingProfile {
                iterations: 0,
                degrees: 90,
            },
        );
        for dz in -1..=1 {
            for dx in -1..=1 {
                let mut c = Chunk::empty();
                c.underlay = [Material::Grass; CELLS_PER_CHUNK_AREA];
                c.smoothing = [1; CELLS_PER_CHUNK_AREA];
                world.insert_chunk(ChunkPos { x: dx, z: dz }, c);
            }
        }
        let sub = SUB as usize;
        for lz in 6..10 {
            for lx in 6..10 {
                let mut points = [UNDERLAY_POINT_INHERIT; SUBCELLS_PER_CELL];
                let mut deltas = [0i16; SUBCELLS_PER_CELL];
                for sz in 0..sub {
                    for sx in 0..sub {
                        let gx = lx * SUB + sx as i32;
                        let gz = lz * SUB + sz as i32;
                        if (30..38).contains(&gx) && (30..38).contains(&gz) {
                            points[sz * sub + sx] = Material::Stone.to_u8();
                            deltas[sz * sub + sx] = 300;
                        }
                    }
                }
                let cell = CellPos { x: lx, z: lz };
                world.set_cell_points(cell, &points);
                world.set_cell_heights(cell, &deltas);
            }
        }
        world
    }

    /// Twice the xz-projected area of a triangle — a wall face is exactly
    /// vertical, so it projects to a line (zero area) while every cap
    /// triangle keeps a footprint; the split separates the two without
    /// guessing from heights.
    fn xz_area_doubled(t: &DrawTriangle) -> f32 {
        let [a, b, c] = &t.verts;
        ((b.x - a.x) * (c.z - a.z) - (c.x - a.x) * (b.z - a.z)).abs()
    }

    #[test]
    fn a_delta_silhouette_wall_coverage_is_complete() {
        // The picket-fence regression from the causeway demo: gating marched
        // segments on the window center's point level skips every window
        // whose center lands on the low-material side — along a contour the
        // center alternates sides subcell to subcell, halving the wall into
        // isolated wedges. The side-aware per-segment gate must close the
        // full perimeter: the emitted wall top-edge length covers the
        // square's 8 m perimeter — the marched chamfers trade the corner
        // right angles for diagonals and the clip-gap walls close the
        // chamfer slivers' lattice edges, so the total sits just above 8 —
        // far above the alternating half.
        let world = delta_column_world();
        let mesh = mesh_chunk(
            &world,
            ChunkPos { x: 0, z: 0 },
            ViewMode::Painted,
            &StyleTable::default(),
        );
        let total: f32 = mesh
            .iter()
            .filter(|t| y_span(t) > 0.5)
            .filter_map(|t| {
                // Each wall quad's top edge rides exactly one of its two
                // triangles — the one holding both top vertices.
                let tops: Vec<&Vertex> = t.verts.iter().filter(|v| v.y > 1.0).collect();
                (tops.len() == 2).then(|| (tops[1].x - tops[0].x).hypot(tops[1].z - tops[0].z))
            })
            .sum();
        assert!(
            (7.5..8.6).contains(&total),
            "wall top-edge coverage {total} m over an 8 m perimeter — alternating gaps halve it",
        );
    }

    #[test]
    fn caps_split_at_a_delta_break_without_draping() {
        // The zigzag regression from the causeway demo: a contour vertex on
        // the break line lifted through whichever subcell it floors into
        // drapes the grass cap up the column flank and sags the stone cap
        // down it with subcell parity. Split caps read their own side: every
        // cap triangle (nonzero footprint — walls project to a line) lies
        // wholly on its side's plate, the grass ground at zero, the stone
        // column at its raised level, nothing in between.
        let world = delta_column_world();
        let mesh = mesh_chunk(
            &world,
            ChunkPos { x: 0, z: 0 },
            ViewMode::Painted,
            &StyleTable::default(),
        );
        let high = 300.0 / 256.0;
        let (mut on_ground, mut on_column) = (false, false);
        for t in &mesh {
            if xz_area_doubled(t) < 1e-6 {
                continue; // vertical wall faces own the gap
            }
            // Every cap triangle lies wholly on one plate: a draped triangle
            // would mix the two levels (or hit an interpolated height between
            // them) within one footprint.
            let ground = t.verts.iter().all(|v| v.y.abs() < 1e-3);
            let column = t.verts.iter().all(|v| (v.y - high).abs() < 1e-3);
            assert!(
                ground || column,
                "cap triangle at ({}, {}) spans heights {:?} — a drape across the break",
                t.verts[0].x,
                t.verts[0].z,
                t.verts.map(|v| v.y),
            );
            on_ground |= ground;
            on_column |= column;
        }
        assert!(
            on_ground && on_column,
            "both caps are present (ground {on_ground}, column {on_column})",
        );
    }

    #[test]
    fn delta_break_walls_seal_both_caps() {
        // Watertightness across the delta break: every wall top vertex
        // coincides with a high-cap vertex, and every wall bottom vertex
        // coincides with a low-cap vertex at the same position — under
        // closure the base is the low cap's committed edge exactly, so the
        // wall spans precisely the vertical gap the split caps open, no
        // pinholes, no overlap.
        let world = delta_column_world();
        let mesh = mesh_chunk(
            &world,
            ChunkPos { x: 0, z: 0 },
            ViewMode::Painted,
            &StyleTable::default(),
        );
        let cap_verts: Vec<&Vertex> = mesh
            .iter()
            .filter(|t| xz_area_doubled(t) > 1e-6)
            .flat_map(|t| t.verts.iter())
            .collect();
        let seals = |x: f32, z: f32, y: f32| {
            cap_verts
                .iter()
                .any(|c| (c.x - x).abs() < 1e-5 && (c.z - z).abs() < 1e-5 && (c.y - y).abs() < 1e-4)
        };
        let walls: Vec<&DrawTriangle> = mesh.iter().filter(|t| y_span(t) > 0.5).collect();
        assert!(!walls.is_empty(), "the break carries walls");
        for v in walls.iter().flat_map(|t| t.verts.iter()) {
            if v.y > 1.0 {
                assert!(
                    seals(v.x, v.z, v.y),
                    "wall top ({}, {}, {}) misses the high cap",
                    v.x,
                    v.z,
                    v.y,
                );
            } else {
                assert!(
                    seals(v.x, v.z, v.y),
                    "wall base ({}, {}, {}) misses the low cap",
                    v.x,
                    v.z,
                    v.y,
                );
            }
        }
    }

    /// The smoothed-relief fixture from the second causeway demo failure: a
    /// stone diamond (`|gx - 34| + |gz - 34| <= 10` on the global subcell
    /// lattice) raised purely by 300-octimeter point deltas over flat grass
    /// cell heights, with contour smoothing enabled at the demo's settings
    /// (iterations 2, degrees 45) through the per-cell override plane. The
    /// staircase silhouette plus smoothing displaces the marched crossings
    /// off the subcell lattice — the regime where a positional side
    /// inference collapses to the floor subcell and reads the wrong plate.
    fn smoothed_diamond_world() -> World {
        let mut world = World::new();
        world.insert_smoothing_profile(
            1,
            SmoothingProfile {
                iterations: 2,
                degrees: 45,
            },
        );
        for dz in -1..=1 {
            for dx in -1..=1 {
                let mut c = Chunk::empty();
                c.underlay = [Material::Grass; CELLS_PER_CHUNK_AREA];
                c.smoothing = [1; CELLS_PER_CHUNK_AREA];
                world.insert_chunk(ChunkPos { x: dx, z: dz }, c);
            }
        }
        let sub = SUB as usize;
        for lz in 0..EDGE {
            for lx in 0..EDGE {
                let mut points = [UNDERLAY_POINT_INHERIT; SUBCELLS_PER_CELL];
                let mut deltas = [0i16; SUBCELLS_PER_CELL];
                let mut any = false;
                for sz in 0..sub {
                    for sx in 0..sub {
                        let gx = lx * SUB + sx as i32;
                        let gz = lz * SUB + sz as i32;
                        if (gx - 34).abs() + (gz - 34).abs() <= 10 {
                            points[sz * sub + sx] = Material::Stone.to_u8();
                            deltas[sz * sub + sx] = 300;
                            any = true;
                        }
                    }
                }
                if any {
                    let cell = CellPos { x: lx, z: lz };
                    world.set_cell_points(cell, &points);
                    world.set_cell_heights(cell, &deltas);
                }
            }
        }
        world
    }

    /// The cap-split locations of a mesh: quantized xz positions where cap
    /// vertices (nonzero-footprint triangles) sit on two plates more than
    /// half a meter apart — the places the caps genuinely split over a
    /// break and a wall must span.
    fn cap_splits(mesh: &[DrawTriangle]) -> Vec<(f32, f32, f32, f32)> {
        let mut heights: BTreeMap<(i64, i64), (f32, f32)> = BTreeMap::new();
        for v in mesh
            .iter()
            .filter(|t| xz_area_doubled(t) > 1e-6)
            .flat_map(|t| t.verts.iter())
        {
            let key = ((v.x * 256.0).round() as i64, (v.z * 256.0).round() as i64);
            let entry = heights.entry(key).or_insert((v.y, v.y));
            entry.0 = entry.0.min(v.y);
            entry.1 = entry.1.max(v.y);
        }
        heights
            .into_iter()
            .filter(|&(_, (lo, hi))| hi - lo > 0.5)
            .map(|((qx, qz), (lo, hi))| (qx as f32 / 256.0, qz as f32 / 256.0, lo, hi))
            .collect()
    }

    /// Shared assertion for authored-plateau fixtures — the live causeway
    /// probe's invariant, in-repo: every cap triangle (nonzero footprint)
    /// lies level on one plateau (a triangle bridging two plateaus is the
    /// slanted "plane not sitting flat" the demo showed — a flap fan
    /// chording across a break), the authored breaks genuinely split the
    /// caps, and every split is spanned by a wall at exactly its two
    /// plates.
    fn assert_plateaus_flat_and_sealed(world: &World, at: ChunkPos) {
        let mesh = mesh_chunk(world, at, ViewMode::Painted, &StyleTable::default());
        for t in &mesh {
            if xz_area_doubled(t) < 1e-6 {
                continue; // vertical wall faces
            }
            let ys = t.verts.map(|v| v.y);
            assert!(
                (ys[0] - ys[1]).abs() < 1e-3 && (ys[0] - ys[2]).abs() < 1e-3,
                "cap triangle at ({}, {}) bridges plateaus: {ys:?}",
                t.verts[0].x,
                t.verts[0].z,
            );
        }
        let splits = cap_splits(&mesh);
        assert!(!splits.is_empty(), "the authored breaks split the caps");
        let walls: Vec<&DrawTriangle> = mesh
            .iter()
            .filter(|t| xz_area_doubled(t) < 1e-6 && y_span(t) > 0.25)
            .collect();
        // A wall covers `(x, z)` at height `y` when the point lies on some
        // wall triangle's edge — walls span whole subcell edges and marched
        // segments, so a split can sit mid-edge (a T-junction on the same
        // straight line), not only on a wall vertex.
        let covered = |x: f32, z: f32, y: f32| {
            walls.iter().any(|t| {
                (0..3).any(|k| {
                    let a = &t.verts[k];
                    let b = &t.verts[(k + 1) % 3];
                    let (ex, ez) = (b.x - a.x, b.z - a.z);
                    let len_sq = ex * ex + ez * ez;
                    if len_sq < 1e-8 {
                        return false;
                    }
                    let (px, pz) = (x - a.x, z - a.z);
                    if (ex * pz - ez * px).abs() > 1e-3 {
                        return false; // off the edge's xz line
                    }
                    let t_on = (px * ex + pz * ez) / len_sq;
                    if !(-1e-4..=1.0 + 1e-4).contains(&t_on) {
                        return false;
                    }
                    (a.y + (b.y - a.y) * t_on - y).abs() < 1e-3
                })
            })
        };
        for (x, z, lo, hi) in splits {
            assert!(
                covered(x, z, hi),
                "split at ({x}, {z}) has no wall top at {hi}",
            );
            assert!(
                covered(x, z, lo),
                "split at ({x}, {z}) has no wall base at {lo}",
            );
        }
    }

    #[test]
    fn a_three_plateau_junction_stays_flat_and_sealed() {
        // Three same-material plateaus (deltas 0 / 128 / 256) meeting at a
        // point inside a stone square on grass, smoothing on — the junction
        // regime the causeway probe caught: mixed windows near the zone
        // lines hold same-label corner samples on different plateaus, and a
        // whole-window flap fan chords across the break (pairs like
        // 0.5 ↔ 1.0 in one footprint triangle). The clipped flaps must keep
        // every cap triangle on one plateau and wall every split.
        let mut world = World::new();
        world.insert_smoothing_profile(
            1,
            SmoothingProfile {
                iterations: 2,
                degrees: 45,
            },
        );
        for dz in -1..=1 {
            for dx in -1..=1 {
                let mut c = Chunk::empty();
                c.underlay = [Material::Grass; CELLS_PER_CHUNK_AREA];
                c.smoothing = [1; CELLS_PER_CHUNK_AREA];
                world.insert_chunk(ChunkPos { x: dx, z: dz }, c);
            }
        }
        let sub = SUB as usize;
        for lz in 0..EDGE {
            for lx in 0..EDGE {
                let mut points = [UNDERLAY_POINT_INHERIT; SUBCELLS_PER_CELL];
                let mut deltas = [0i16; SUBCELLS_PER_CELL];
                let mut any = false;
                for sz in 0..sub {
                    for sx in 0..sub {
                        let gx = lx * SUB + sx as i32;
                        let gz = lz * SUB + sz as i32;
                        if (28..44).contains(&gx) && (28..44).contains(&gz) {
                            points[sz * sub + sx] = Material::Stone.to_u8();
                            // Zone lines x = 9 m and z = 9 m meet at the
                            // junction and run out through the perimeter.
                            deltas[sz * sub + sx] = if gx < 36 {
                                0
                            } else if gz < 36 {
                                128
                            } else {
                                256
                            };
                            any = true;
                        }
                    }
                }
                if any {
                    let cell = CellPos { x: lx, z: lz };
                    world.set_cell_points(cell, &points);
                    world.set_cell_heights(cell, &deltas);
                }
            }
        }
        assert_plateaus_flat_and_sealed(&world, ChunkPos { x: 0, z: 0 });
    }

    #[test]
    fn a_two_height_silhouette_against_grass_stays_flat_and_sealed() {
        // An L-shaped stone region on grass whose two arms stand at
        // different heights (deltas 128 / 256), smoothing on — the internal
        // same-material break line runs out through the stone/grass
        // perimeter, the second junction regime from the causeway probe.
        // Clipped flaps keep every cap triangle level; the walls close both
        // the perimeter and the internal break.
        let mut world = World::new();
        world.insert_smoothing_profile(
            1,
            SmoothingProfile {
                iterations: 2,
                degrees: 45,
            },
        );
        for dz in -1..=1 {
            for dx in -1..=1 {
                let mut c = Chunk::empty();
                c.underlay = [Material::Grass; CELLS_PER_CHUNK_AREA];
                c.smoothing = [1; CELLS_PER_CHUNK_AREA];
                world.insert_chunk(ChunkPos { x: dx, z: dz }, c);
            }
        }
        let sub = SUB as usize;
        for lz in 0..EDGE {
            for lx in 0..EDGE {
                let mut points = [UNDERLAY_POINT_INHERIT; SUBCELLS_PER_CELL];
                let mut deltas = [0i16; SUBCELLS_PER_CELL];
                let mut any = false;
                for sz in 0..sub {
                    for sx in 0..sub {
                        let gx = lx * SUB + sx as i32;
                        let gz = lz * SUB + sz as i32;
                        // Vertical arm at +128; the foot at +256.
                        let arm = (30..36).contains(&gx) && (26..42).contains(&gz);
                        let foot = (36..42).contains(&gx) && (34..42).contains(&gz);
                        if arm || foot {
                            points[sz * sub + sx] = Material::Stone.to_u8();
                            deltas[sz * sub + sx] = if arm { 128 } else { 256 };
                            any = true;
                        }
                    }
                }
                if any {
                    let cell = CellPos { x: lx, z: lz };
                    world.set_cell_points(cell, &points);
                    world.set_cell_heights(cell, &deltas);
                }
            }
        }
        assert_plateaus_flat_and_sealed(&world, ChunkPos { x: 0, z: 0 });
    }

    #[test]
    fn smoothed_break_splits_are_walled() {
        // Regression from the live causeway scene: smoothing displaced the
        // paint boundary off the physical break, where the retired
        // positional side inference collapsed both flaps onto the floor
        // subcell's plate — the caps fused (the grass drape) and the walls
        // thinned to picket striping. The break barrier freezes the samples
        // flanking the break, so the contour stays on the authored line;
        // the anchor threading then splits the caps there. The caps must
        // split at many crossings (the count floor is the anti-collapse
        // tripwire: observed 168, the collapse drives it toward zero), no
        // cap triangle may bridge the two plates (the drape), and every
        // split must be spanned by a wall at exactly its two plates.
        let world = smoothed_diamond_world();
        let mesh = mesh_chunk(
            &world,
            ChunkPos { x: 0, z: 0 },
            ViewMode::Painted,
            &StyleTable::default(),
        );
        let high = 300.0 / 256.0;
        for t in &mesh {
            if xz_area_doubled(t) < 1e-6 {
                continue; // vertical wall faces own the gap
            }
            let ground = t.verts.iter().all(|v| v.y.abs() < 1e-3);
            let column = t.verts.iter().all(|v| (v.y - high).abs() < 1e-3);
            assert!(
                ground || column,
                "cap triangle at ({}, {}) spans heights {:?} — a drape across the break",
                t.verts[0].x,
                t.verts[0].z,
                t.verts.map(|v| v.y),
            );
        }
        let splits = cap_splits(&mesh);
        assert!(
            splits.len() >= 80,
            "only {} cap splits — the flaps collapsed onto one plate",
            splits.len(),
        );
        let walls: Vec<&Vertex> = mesh
            .iter()
            .filter(|t| xz_area_doubled(t) < 1e-6 && y_span(t) > 0.5)
            .flat_map(|t| t.verts.iter())
            .collect();
        for (x, z, lo, hi) in splits {
            let at = |y: f32| {
                walls.iter().any(|v| {
                    (v.x - x).abs() < 1e-4 && (v.z - z).abs() < 1e-4 && (v.y - y).abs() < 1e-3
                })
            };
            assert!(
                at(hi),
                "split at ({x}, {z}) has no wall top at {hi} — a picket gap",
            );
            assert!(
                at(lo),
                "split at ({x}, {z}) has no wall base at {lo} — an unsealed foot",
            );
        }
    }

    #[test]
    fn smoothed_break_walls_seal_both_caps() {
        // Watertightness in the smoothing-displaced regime: every wall top
        // vertex coincides with a cap vertex and every wall base coincides
        // with one — the anchored lifts hand the wall the same values the
        // flaps drew, for crossings anywhere off the lattice.
        let world = smoothed_diamond_world();
        let mesh = mesh_chunk(
            &world,
            ChunkPos { x: 0, z: 0 },
            ViewMode::Painted,
            &StyleTable::default(),
        );
        let cap_verts: Vec<&Vertex> = mesh
            .iter()
            .filter(|t| xz_area_doubled(t) > 1e-6)
            .flat_map(|t| t.verts.iter())
            .collect();
        let seals = |x: f32, z: f32, y: f32| {
            cap_verts
                .iter()
                .any(|c| (c.x - x).abs() < 1e-5 && (c.z - z).abs() < 1e-5 && (c.y - y).abs() < 1e-4)
        };
        let walls: Vec<&DrawTriangle> = mesh
            .iter()
            .filter(|t| xz_area_doubled(t) < 1e-6 && y_span(t) > 0.5)
            .collect();
        assert!(!walls.is_empty(), "the smoothed break carries walls");
        for v in walls.iter().flat_map(|t| t.verts.iter()) {
            if v.y > 1.0 {
                assert!(
                    seals(v.x, v.z, v.y),
                    "wall top ({}, {}, {}) misses the high cap",
                    v.x,
                    v.z,
                    v.y,
                );
            } else {
                assert!(
                    seals(v.x, v.z, v.y),
                    "wall base ({}, {}, {}) misses the low cap",
                    v.x,
                    v.z,
                    v.y,
                );
            }
        }
    }

    #[test]
    fn zeroed_deltas_mesh_identically() {
        // Tripwire: a world whose height deltas are all zero meshes byte-for-
        // byte as one that never carried the plane. Author then clear a cell's
        // deltas over a varied world; the net-zero relief must collapse back
        // to the cell-stride caps and walls, adding and moving nothing.
        let at = ChunkPos { x: 0, z: 0 };
        let mut world = World::new();
        let mut chunk = Chunk::empty();
        for lz in 0..EDGE {
            for lx in 0..EDGE {
                let i = (lz * EDGE + lx) as usize;
                chunk.underlay[i] = if lx < 8 {
                    Material::Grass
                } else {
                    Material::Sand
                };
                chunk.height[i] = 8 * lx; // gentle ramp, no cliffs
            }
        }
        world.insert_chunk(at, chunk);
        let baseline = mesh_chunk(&world, at, ViewMode::Painted, &StyleTable::default());

        let mut authored = world.clone();
        authored.set_cell_heights(CellPos { x: 4, z: 4 }, &[50; SUBCELLS_PER_CELL]);
        authored.set_cell_heights(CellPos { x: 4, z: 4 }, &[]); // clears to zero
        let after = mesh_chunk(&authored, at, ViewMode::Painted, &StyleTable::default());

        assert_eq!(
            baseline, after,
            "a net-zero delta plane meshes identically to no plane",
        );
    }

    #[test]
    fn a_fillable_void_joint_floors_and_walls() {
        // Tripwire: an enclosed Void well one meter deep in a grass field
        // closes as a real flat-bottomed cut — a floor cap at the void
        // points' stored height and rim walls from the grass down to it, no
        // open bottom. A prediction pass blind to the void depth would leave
        // the groove open (a wall to a fixed drop, or no wall at all); the
        // floor and its walls both read the one total height plane.
        let mut world = World::new();
        let mut chunk = Chunk::empty();
        chunk.underlay = [Material::Grass; CELLS_PER_CHUNK_AREA];
        world.insert_chunk(ChunkPos { x: 0, z: 0 }, chunk);
        // Cell (7, 7) is a full Void well, floored one meter down and enclosed
        // by grass on every side.
        let cell = CellPos { x: 7, z: 7 };
        world.set_cell_points(cell, &[Material::Void.to_u8(); SUBCELLS_PER_CELL]);
        world.set_cell_heights(cell, &[-256; SUBCELLS_PER_CELL]);

        let mesh = mesh_chunk(
            &world,
            ChunkPos { x: 0, z: 0 },
            ViewMode::Painted,
            &StyleTable::default(),
        );
        let floor = -256.0 / OCTIMETERS_PER_METER;
        let has_floor = mesh.iter().any(|t| {
            xz_area_doubled(t) > 1e-6 && t.verts.iter().all(|v| (v.y - floor).abs() < 1e-3)
        });
        assert!(
            has_floor,
            "the enclosed void floors over at its stored depth"
        );
        // A rim wall spans the grass edge (y ~ 0) down to the floor (y ~ -1 m)
        // — the groove is closed, not open-bottomed.
        let has_rim_wall = mesh.iter().any(|t| {
            xz_area_doubled(t) < 1e-6
                && t.verts.iter().any(|v| v.y > -0.1)
                && t.verts.iter().any(|v| (v.y - floor).abs() < 1e-3)
        });
        assert!(
            has_rim_wall,
            "the void rim walls down to the floor, no open bottom",
        );
    }
}
