// Chunk-local loop counters and world-cell / octimeter coordinates are
// small integers cast between i32 (coordinate + color math), u32 (color
// accumulation), usize (plane indexing), and f32 (vertex output). The
// ranges — chunk-bounded cells, `[0, 65535]` color channels, octimeter
// positions within a chunk plus a one-subcell apron — make the
// precision / sign / truncation lints the pedantic set raises non-issues
// here.
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
//! [`mesh_chunk`] composes two passes over the chunk, both bit-exact
//! deterministic and unit-testable host-side (no wgpu, no ctx):
//!
//! # Underlay pass — corner-blended ground fabric
//!
//! One quad (two triangles) per non-[`Material::Void`] cell over the
//! chunk's `17 × 17` corner lattice at `y = 0`, world-space in meters
//! (`1 cell = 1 m`). Each lattice corner is shared by up to four cells
//! and takes the integer average, over its **non-Void incident cells
//! only**, of their cascade-resolved palette colors — so a world-edge
//! corner blends among what exists instead of darkening toward Void. The
//! palette is integer linear-space channels (`LINEAR_PALETTE`); before
//! averaging, each cell's color is scaled per channel by a deterministic
//! per-cell jitter (a hash of the cell's world coordinates) that mottles
//! the ground so the corner blend reads as fabric, not a gradient ramp.
//! All color math is integer with one f32 conversion at vertex emit, and
//! every position is an integer meter, so two chunks meshed independently
//! agree exactly on their shared border corners — the `R = 1` apron read
//! (corners on a chunk edge sample the neighbor chunk through [`World`]).
//!
//! # Overlay pass — marching-squares contours
//!
//! Per distinct overlay material in the chunk, march `2 × 2` windows over
//! the material's binary subcell field ("overlay is this material and the
//! coverage bit is set") to emit crisp contours: axis-aligned and
//! 45-degree segments with crossings at window-edge midpoints, all on the
//! half-subcell (32-octimeter) lattice. The field samples subcell centers
//! plus a one-subcell apron ring read from neighbor cells
//! ([`World::overlay_mask`]), so a contour continues correctly across a
//! cell or chunk boundary. Fully-covered windows greedy-merge into larger
//! flat quads (uniform overlay color, so merging is safe); boundary
//! windows emit their case's contour polygon from a 16-case table with
//! one fixed connected saddle rule. Overlay geometry is lifted one
//! octimeter (`1/256 m`) over the underlay so the coplanar passes never
//! z-fight, and it lives in its own vertices with its own flat color, so
//! the overlay/underlay seam stays hard — coincident positions carry two
//! colors with no blending.

use alloc::vec;
use alloc::vec::Vec;

use aether_capabilities::render::{DrawTriangle, Vertex};

use crate::world::{CELLS_PER_CHUNK, CellPos, ChunkPos, Material, SUBCELLS_PER_CELL_EDGE, World};

/// Per-material color channels in **linear** space as integers in
/// `[0, 65535]`, converted once from the sRGB design values (the same
/// values the v1 mesher pre-linearized as f32). Keeping the palette
/// integer makes the corner average a bit-exact sum-and-divide with no
/// float-rounding dependence; the single f32 conversion happens at vertex
/// emit. Index by `Material as usize`; index 0 (`Void`) is never emitted.
///
/// sRGB design values: Grass `(0.30, 0.55, 0.25)`, Dirt
/// `(0.45, 0.32, 0.18)`, Stone `(0.55, 0.55, 0.58)`, Sand
/// `(0.85, 0.78, 0.55)`, Water `(0.20, 0.40, 0.70)`.
const LINEAR_PALETTE: [[u16; 3]; 6] = [
    [0, 0, 0],             // Void — unused
    [4797, 17255, 3336],   // Grass
    [11193, 5472, 1783],   // Dirt
    [17255, 17255, 19379], // Stone
    [45344, 37388, 17255], // Sand
    [2169, 8710, 29353],   // Water
];

/// Fixed-point one for the jitter channel scale — a factor of
/// [`JITTER_ONE`] leaves a channel unchanged.
const JITTER_ONE: i32 = 256;

/// Peak jitter deviation in [`JITTER_ONE`] units — a channel is scaled
/// within `±JITTER_RANGE / JITTER_ONE` (~±5%).
const JITTER_RANGE: i32 = 13;

/// Mesher-constant seed folded into every per-cell jitter hash. A world
/// generator seed does not exist to thread yet; when it does it replaces
/// this constant so different worlds mottle differently.
const JITTER_SEED: u32 = 0x00A1_7E30;

/// Cells along one chunk edge, as a plain `i32` for the mesher's loop
/// bounds and coordinate math.
const EDGE: i32 = CELLS_PER_CHUNK;

/// Subcells along one chunk edge: `EDGE * SUB`.
const SUB: i32 = SUBCELLS_PER_CELL_EDGE as i32;

/// Subcells along one chunk edge (`16 * 4 = 64` at `SUB = 4`).
const SUBCELLS_PER_CHUNK_EDGE: i32 = EDGE * SUB;

/// Octimeters per subcell: `256 / SUB` (`64` at `SUB = 4`). Subcell
/// centers sit at `index * OCTIMETERS_PER_SUBCELL + OCTIMETERS_PER_SUBCELL / 2`.
const OCTIMETERS_PER_SUBCELL: i32 = 256 / SUB;

/// Octimeters per meter (`1 cell = 1 m = 256 octimeters`), for the
/// octimeter → meter conversion at overlay vertex emit.
const OCTIMETERS_PER_METER: f32 = 256.0;

/// The y-lift applied to overlay geometry: one octimeter above the
/// underlay's `y = 0`, so the two coplanar passes never z-fight.
const OVERLAY_Y_LIFT: f32 = 1.0 / OCTIMETERS_PER_METER;

/// Marching-squares inside-polygon table, indexed by the 4-bit window
/// case `BL | BR<<1 | TR<<2 | TL<<3` (`1` = the corner sample is inside
/// the material). Each entry lists the polygon of the covered region in
/// boundary order, over the eight window points: corners
/// `0 = BL, 1 = BR, 2 = TR, 3 = TL` and edge midpoints
/// `4 = bottom, 5 = right, 6 = top, 7 = left`. Case 0 is empty and case
/// 15 (fully inside) is emitted by the greedy interior merge, not here.
/// The two saddle cases (5, 10) resolve by one fixed rule: the inside
/// pair is always connected through the center (the background is
/// pinched), so a saddle reads as one region rather than two touching
/// corners.
const CASE_POLYS: [&[u8]; 16] = [
    &[],                 // 0  — empty
    &[0, 4, 7],          // 1  BL
    &[1, 5, 4],          // 2  BR
    &[0, 1, 5, 7],       // 3  BL BR — bottom half
    &[2, 6, 5],          // 4  TR
    &[0, 4, 5, 2, 6, 7], // 5  BL TR — saddle, connected
    &[1, 2, 6, 4],       // 6  BR TR — right half
    &[0, 1, 2, 6, 7],    // 7  BL BR TR
    &[3, 7, 6],          // 8  TL
    &[0, 4, 6, 3],       // 9  BL TL — left half
    &[1, 5, 6, 3, 7, 4], // 10 BR TL — saddle, connected
    &[0, 1, 5, 6, 3],    // 11 BL BR TL
    &[3, 2, 5, 7],       // 12 TR TL — top half
    &[0, 4, 5, 2, 3],    // 13 BL TR TL
    &[1, 2, 3, 7, 4],    // 14 BR TR TL
    &[0, 1, 2, 3],       // 15 — merged interior, unused
];

/// Mesh one chunk into its triangle list: the corner-blended underlay
/// pass then the marching-squares overlay pass. Pure — no wgpu, no ctx —
/// so it is unit-testable host-side. Reads neighbor cells through
/// [`World`] (the `R = 1` apron); a missing neighbor reads as empty.
#[must_use]
pub fn mesh_chunk(world: &World, at: ChunkPos) -> Vec<DrawTriangle> {
    let mut tris = Vec::new();
    mesh_underlay(world, at, &mut tris);
    mesh_overlay(world, at, &mut tris);
    tris
}

/// A deterministic integer hash of a cell's world coordinates, folding in
/// [`JITTER_SEED`]. Drives the per-cell color jitter; identical inputs
/// always produce identical output, so a cell mottles the same way no
/// matter which chunk's mesh reads it.
fn cell_hash(x: i32, z: i32) -> u32 {
    let mut h = JITTER_SEED;
    h = (h ^ x as u32).wrapping_mul(0x9E37_79B1);
    h ^= h >> 15;
    h = (h ^ z as u32).wrapping_mul(0x85EB_CA77);
    h ^= h >> 13;
    h = h.wrapping_mul(0xC2B2_AE3D);
    h ^ (h >> 16)
}

/// The jitter scale for one channel: a fixed-point factor in
/// `[JITTER_ONE - JITTER_RANGE, JITTER_ONE + JITTER_RANGE]` derived from a
/// distinct byte of the cell hash, so the three channels jitter
/// independently.
fn channel_factor(hash: u32, channel: usize) -> i32 {
    let byte = ((hash >> (channel * 8)) & 0xFF) as i32; // 0..=255
    let delta = (byte - 128) * JITTER_RANGE / 128; // ~ -JITTER_RANGE..=JITTER_RANGE
    JITTER_ONE + delta
}

/// A cell's jittered palette color as integer linear channels. The base
/// palette entry is scaled per channel by [`channel_factor`] and clamped
/// back into `[0, 65535]`; the result feeds the corner average as `u32`
/// so four of them sum without overflow.
fn jittered_cell_color(material: Material, x: i32, z: i32) -> [u32; 3] {
    let base = LINEAR_PALETTE[material as usize];
    let hash = cell_hash(x, z);
    let scale = |channel: usize| -> u32 {
        let factor = channel_factor(hash, channel);
        let scaled = i32::from(base[channel]) * factor / JITTER_ONE;
        scaled.clamp(0, i32::from(u16::MAX)) as u32
    };
    [scale(0), scale(1), scale(2)]
}

/// Resolve the blended color at the lattice corner `(x, z)` (world cell
/// coordinates): the integer average, over the corner's non-Void incident
/// cells, of their jittered palette colors, converted to f32 in `[0, 1]`.
/// The four incident cells are the four with a corner at `(x, z)`. A
/// corner of an emitted quad always has at least its own cell incident,
/// so the count is at least one.
fn corner_color(world: &World, x: i32, z: i32) -> [f32; 3] {
    let mut sum = [0u32; 3];
    let mut count = 0u32;
    for (cx, cz) in [(x - 1, z - 1), (x, z - 1), (x - 1, z), (x, z)] {
        let material = world.underlay(CellPos { x: cx, z: cz });
        if material == Material::Void {
            continue;
        }
        let color = jittered_cell_color(material, cx, cz);
        sum[0] += color[0];
        sum[1] += color[1];
        sum[2] += color[2];
        count += 1;
    }
    let count = count.max(1);
    [
        (sum[0] / count) as f32 / 65535.0,
        (sum[1] / count) as f32 / 65535.0,
        (sum[2] / count) as f32 / 65535.0,
    ]
}

/// Emit the underlay pass: one corner-blended quad per non-Void cell.
fn mesh_underlay(world: &World, at: ChunkPos, tris: &mut Vec<DrawTriangle>) {
    let base_x = at.x * EDGE;
    let base_z = at.z * EDGE;
    for lz in 0..EDGE {
        for lx in 0..EDGE {
            let cx = base_x + lx;
            let cz = base_z + lz;
            if world.underlay(CellPos { x: cx, z: cz }) == Material::Void {
                continue;
            }
            let vert = |x: i32, z: i32, color: [f32; 3]| Vertex {
                x: x as f32,
                y: 0.0,
                z: z as f32,
                r: color[0],
                g: color[1],
                b: color[2],
            };
            let a = vert(cx, cz, corner_color(world, cx, cz));
            let b = vert(cx + 1, cz, corner_color(world, cx + 1, cz));
            let c = vert(cx + 1, cz + 1, corner_color(world, cx + 1, cz + 1));
            let d = vert(cx, cz + 1, corner_color(world, cx, cz + 1));
            tris.push(DrawTriangle { verts: [a, b, c] });
            tris.push(DrawTriangle { verts: [a, c, d] });
        }
    }
}

/// Convert a `Material`'s linear palette entry to a flat f32 color for the
/// overlay pass — no jitter, no blending, so an overlay region reads as
/// one crisp color.
fn overlay_color(material: Material) -> [f32; 3] {
    let base = LINEAR_PALETTE[material as usize];
    [
        f32::from(base[0]) / 65535.0,
        f32::from(base[1]) / 65535.0,
        f32::from(base[2]) / 65535.0,
    ]
}

/// Is the subcell at chunk-local index `(six, siz)` covered by `material`?
/// Indices range over `-1..=SUBCELLS_PER_CHUNK_EDGE` (the field plus its
/// one-subcell apron); an out-of-chunk index resolves to a neighbor cell
/// through [`World`], reading empty for a missing chunk. Covered means the
/// cell's raw overlay is `material` *and* the subcell's coverage bit is
/// set.
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
/// the chunk, march its binary subcell field.
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

/// March one overlay material's subcell field into contour + merged
/// interior geometry.
fn mesh_overlay_material(
    world: &World,
    at: ChunkPos,
    material: Material,
    tris: &mut Vec<DrawTriangle>,
) {
    let color = overlay_color(material);

    // Sample the subcell field over `-1..=SUBCELLS_PER_CHUNK_EDGE` on each
    // axis — the chunk's own subcell centers plus the one-subcell apron
    // ring. Row stride is `SAMPLES` (index shifted by +1 so the apron sits
    // at 0). `SAMPLES = SUBCELLS_PER_CHUNK_EDGE + 2`.
    let samples = (SUBCELLS_PER_CHUNK_EDGE + 2) as usize;
    let mut covered = vec![false; samples * samples];
    for sj in -1..=SUBCELLS_PER_CHUNK_EDGE {
        for si in -1..=SUBCELLS_PER_CHUNK_EDGE {
            let idx = (sj + 1) as usize * samples + (si + 1) as usize;
            covered[idx] = subcell_covered(world, at, si, sj, material);
        }
    }

    // Classify each `2 × 2` window (lower-left sample `-1..=..-1`) into its
    // marching case, and flag the fully-inside ones for interior merging.
    // Window index `(wi, wj) = (six + 1, siz + 1)`; stride `WINDOWS`.
    let windows = (SUBCELLS_PER_CHUNK_EDGE + 1) as usize;
    let mut case_grid = vec![0u8; windows * windows];
    let mut full = vec![false; windows * windows];
    for siz in -1..SUBCELLS_PER_CHUNK_EDGE {
        for six in -1..SUBCELLS_PER_CHUNK_EDGE {
            let bl = covered[(siz + 1) as usize * samples + (six + 1) as usize];
            let br = covered[(siz + 1) as usize * samples + (six + 2) as usize];
            let tl = covered[(siz + 2) as usize * samples + (six + 1) as usize];
            let tr = covered[(siz + 2) as usize * samples + (six + 2) as usize];
            let case = u8::from(bl) | u8::from(br) << 1 | u8::from(tr) << 2 | u8::from(tl) << 3;
            let idx = (siz + 1) as usize * windows + (six + 1) as usize;
            case_grid[idx] = case;
            full[idx] = case == 15;
        }
    }

    // The window/subcell octimeter coordinates above are chunk-local; the
    // overlay geometry lives in world space like the underlay, so both
    // passes shift by the chunk's base octimeter offset `[x, z]` before
    // emit.
    let base_oct = [
        at.x * SUBCELLS_PER_CHUNK_EDGE * OCTIMETERS_PER_SUBCELL,
        at.z * SUBCELLS_PER_CHUNK_EDGE * OCTIMETERS_PER_SUBCELL,
    ];
    merge_interior(&full, windows, base_oct, color, tris);
    emit_contours(&case_grid, windows, base_oct, color, tris);
}

/// Greedy-merge the fully-covered (`case == 15`) windows into maximal
/// rectangles — the interior is one uniform overlay color, so merging is
/// safe — and emit one flat quad per rectangle.
fn merge_interior(
    full: &[bool],
    windows: usize,
    base_oct: [i32; 2],
    color: [f32; 3],
    tris: &mut Vec<DrawTriangle>,
) {
    let mut consumed = vec![false; full.len()];
    for wj in 0..windows {
        for wi in 0..windows {
            let idx = wj * windows + wi;
            if consumed[idx] || !full[idx] {
                continue;
            }
            let mut width = 1;
            while wi + width < windows
                && full[wj * windows + wi + width]
                && !consumed[wj * windows + wi + width]
            {
                width += 1;
            }
            let mut height = 1;
            'rows: while wj + height < windows {
                for dx in 0..width {
                    let cell = (wj + height) * windows + wi + dx;
                    if !full[cell] || consumed[cell] {
                        break 'rows;
                    }
                }
                height += 1;
            }
            for dz in 0..height {
                for dx in 0..width {
                    consumed[(wj + dz) * windows + wi + dx] = true;
                }
            }
            // A window `wi` spans `x ∈ [(wi-1)*sub + half, wi*sub + half]`
            // in octimeters (`six = wi - 1`, centers offset by half a
            // subcell); a `width`-wide run extends the far edge.
            let half = OCTIMETERS_PER_SUBCELL / 2;
            let x0 = base_oct[0] + (wi as i32 - 1) * OCTIMETERS_PER_SUBCELL + half;
            let x1 = base_oct[0] + (wi as i32 - 1 + width as i32) * OCTIMETERS_PER_SUBCELL + half;
            let z0 = base_oct[1] + (wj as i32 - 1) * OCTIMETERS_PER_SUBCELL + half;
            let z1 = base_oct[1] + (wj as i32 - 1 + height as i32) * OCTIMETERS_PER_SUBCELL + half;
            push_overlay_quad(tris, x0, z0, x1, z1, color);
        }
    }
}

/// Emit the contour geometry for every boundary window (case `1..=14`).
fn emit_contours(
    case_grid: &[u8],
    windows: usize,
    base_oct: [i32; 2],
    color: [f32; 3],
    tris: &mut Vec<DrawTriangle>,
) {
    for wj in 0..windows {
        for wi in 0..windows {
            let case = case_grid[wj * windows + wi];
            if case == 0 || case == 15 {
                continue;
            }
            let six = wi as i32 - 1;
            let siz = wj as i32 - 1;
            emit_window_contour(six, siz, base_oct, case, color, tris);
        }
    }
}

/// Fan-triangulate one window's marching-squares inside polygon at the
/// overlay y-lift.
fn emit_window_contour(
    six: i32,
    siz: i32,
    base_oct: [i32; 2],
    case: u8,
    color: [f32; 3],
    tris: &mut Vec<DrawTriangle>,
) {
    let half = OCTIMETERS_PER_SUBCELL / 2;
    // Sample centers sit half a subcell in from the window's octimeter
    // origin, so the window corners are at `..±half` and the edge
    // midpoints land on the subcell boundary lattice. The `base_oct`
    // shift lifts these chunk-local coordinates into world space.
    let x_lo = base_oct[0] + six * OCTIMETERS_PER_SUBCELL + half;
    let x_hi = base_oct[0] + (six + 1) * OCTIMETERS_PER_SUBCELL + half;
    let z_lo = base_oct[1] + siz * OCTIMETERS_PER_SUBCELL + half;
    let z_hi = base_oct[1] + (siz + 1) * OCTIMETERS_PER_SUBCELL + half;
    let x_mid = base_oct[0] + (six + 1) * OCTIMETERS_PER_SUBCELL;
    let z_mid = base_oct[1] + (siz + 1) * OCTIMETERS_PER_SUBCELL;
    let points = [
        [x_lo, z_lo],  // 0 BL corner
        [x_hi, z_lo],  // 1 BR corner
        [x_hi, z_hi],  // 2 TR corner
        [x_lo, z_hi],  // 3 TL corner
        [x_mid, z_lo], // 4 bottom mid
        [x_hi, z_mid], // 5 right mid
        [x_mid, z_hi], // 6 top mid
        [x_lo, z_mid], // 7 left mid
    ];
    let poly = CASE_POLYS[case as usize];
    let vert = |p: [i32; 2]| Vertex {
        x: p[0] as f32 / OCTIMETERS_PER_METER,
        y: OVERLAY_Y_LIFT,
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

/// Push the two triangles of a flat overlay quad spanning
/// `[x0, x1] × [z0, z1]` (octimeters) at the overlay y-lift, all corners
/// the same flat color.
fn push_overlay_quad(
    tris: &mut Vec<DrawTriangle>,
    x0: i32,
    z0: i32,
    x1: i32,
    z1: i32,
    color: [f32; 3],
) {
    let vert = |x: i32, z: i32| Vertex {
        x: x as f32 / OCTIMETERS_PER_METER,
        y: OVERLAY_Y_LIFT,
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
    use crate::world::{CELLS_PER_CHUNK_AREA, Chunk, Region};

    /// A world holding one chunk whose underlay is filled per a closure.
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

    /// Underlay triangles are the ones at `y == 0`; overlay ones sit at
    /// the y-lift.
    fn underlay_tris(tris: &[DrawTriangle]) -> usize {
        tris.iter()
            .filter(|t| t.verts.iter().all(|v| v.y == 0.0))
            .count()
    }

    fn overlay_tris(tris: &[DrawTriangle]) -> usize {
        tris.iter()
            .filter(|t| t.verts.iter().all(|v| v.y == OVERLAY_Y_LIFT))
            .count()
    }

    #[test]
    fn full_chunk_underlay_is_512_triangles() {
        // Budget tripwire: every cell emits its own quad (no greedy merge),
        // so a full chunk is a flat 512 underlay triangles regardless of
        // material uniformity.
        let world = world_with_underlay(ChunkPos { x: 0, z: 0 }, |_, _| Material::Grass);
        let tris = mesh_chunk(&world, ChunkPos { x: 0, z: 0 });
        assert_eq!(underlay_tris(&tris), 512, "16x16 cells * 2 = 512 tris");
    }

    #[test]
    fn void_chunk_meshes_to_nothing() {
        let mut world = World::new();
        world.insert_chunk(ChunkPos { x: 0, z: 0 }, Chunk::empty());
        let tris = mesh_chunk(&world, ChunkPos { x: 0, z: 0 });
        assert!(tris.is_empty(), "all-Void chunk emits no geometry");
    }

    #[test]
    fn corner_average_over_four_distinct_materials() {
        // A 2x2 block of distinct materials shares the corner at world
        // (1,1); it must be the exact integer average of the four cells'
        // jittered colors (catches wrong incidence set or corner
        // orientation).
        let world = world_with_underlay(ChunkPos { x: 0, z: 0 }, |x, z| match (x, z) {
            (0, 0) => Material::Grass,
            (1, 0) => Material::Dirt,
            (0, 1) => Material::Stone,
            (1, 1) => Material::Sand,
            _ => Material::Void,
        });
        let mut sum = [0u32; 3];
        for (m, cx, cz) in [
            (Material::Grass, 0, 0),
            (Material::Dirt, 1, 0),
            (Material::Stone, 0, 1),
            (Material::Sand, 1, 1),
        ] {
            let c = jittered_cell_color(m, cx, cz);
            sum[0] += c[0];
            sum[1] += c[1];
            sum[2] += c[2];
        }
        let expected = [
            (sum[0] / 4) as f32 / 65535.0,
            (sum[1] / 4) as f32 / 65535.0,
            (sum[2] / 4) as f32 / 65535.0,
        ];
        assert_eq!(corner_color(&world, 1, 1), expected);
    }

    #[test]
    fn jitter_is_pinned_and_varies_per_cell() {
        // Tripwire: the hash + channel scaling are pinned to exact output;
        // drift here re-mottles every world, so it must be a deliberate
        // change. Adjacent cells of the same material must differ, or the
        // jitter is not doing its job.
        assert_eq!(
            jittered_cell_color(Material::Grass, 0, 0),
            [4890, 16513, 3322],
        );
        assert_ne!(
            jittered_cell_color(Material::Grass, 0, 0),
            jittered_cell_color(Material::Grass, 1, 0),
            "adjacent same-material cells must mottle differently",
        );
    }

    #[test]
    fn corner_averages_over_non_void_incidents_only() {
        // Corner (1,1) has two non-Void incident cells (the diagonal);
        // its color must average over 2, not darken toward Void by
        // dividing by 4.
        let world = world_with_underlay(ChunkPos { x: 0, z: 0 }, |x, z| {
            if (x, z) == (0, 0) || (x, z) == (1, 1) {
                Material::Grass
            } else {
                Material::Void
            }
        });
        let a = jittered_cell_color(Material::Grass, 0, 0);
        let b = jittered_cell_color(Material::Grass, 1, 1);
        let expected = [
            u32::midpoint(a[0], b[0]) as f32 / 65535.0,
            u32::midpoint(a[1], b[1]) as f32 / 65535.0,
            u32::midpoint(a[2], b[2]) as f32 / 65535.0,
        ];
        assert_eq!(corner_color(&world, 1, 1), expected);
        // And it is not the four-way average (which would divide by 4).
        let four = [
            ((a[0] + b[0]) / 4) as f32 / 65535.0,
            ((a[1] + b[1]) / 4) as f32 / 65535.0,
            ((a[2] + b[2]) / 4) as f32 / 65535.0,
        ];
        assert_ne!(corner_color(&world, 1, 1), four);
    }

    #[test]
    fn border_corner_reads_the_neighbor_chunk_apron() {
        // Cell (15, z) in chunk (0,0) and cell (16, z) in chunk (1,0) meet
        // at the border corner world x = 16. With the neighbor present the
        // corner blends both materials; drop the neighbor and it must
        // change (proving the mesher read across the chunk boundary).
        let mut both = World::new();
        both.insert_chunk(ChunkPos { x: 0, z: 0 }, {
            let mut c = Chunk::empty();
            c.underlay = [Material::Grass; CELLS_PER_CHUNK_AREA];
            c
        });
        both.insert_chunk(ChunkPos { x: 1, z: 0 }, {
            let mut c = Chunk::empty();
            c.underlay = [Material::Dirt; CELLS_PER_CHUNK_AREA];
            c
        });
        let blended = corner_color(&both, 16, 8);

        // The same border corner, meshing chunk (0,0), must match the
        // direct read — the actor's cached mesh sees the apron too.
        let tris = mesh_chunk(&both, ChunkPos { x: 0, z: 0 });
        let border_vertex = tris
            .iter()
            .flat_map(|t| t.verts.iter())
            .find(|v| v.y == 0.0 && v.x == 16.0 && v.z == 8.0)
            .expect("chunk (0,0) emits a vertex at its border corner");
        assert_eq!([border_vertex.r, border_vertex.g, border_vertex.b], blended);

        let mut lone = World::new();
        lone.insert_chunk(ChunkPos { x: 0, z: 0 }, {
            let mut c = Chunk::empty();
            c.underlay = [Material::Grass; CELLS_PER_CHUNK_AREA];
            c
        });
        assert_ne!(
            corner_color(&lone, 16, 8),
            blended,
            "dropping the neighbor must change the border corner",
        );
    }

    /// A world whose one chunk carries a single overlay material at a
    /// caller-supplied mask per cell.
    fn world_with_overlay(
        pos: ChunkPos,
        material: Material,
        mask: impl Fn(i32, i32) -> u16,
    ) -> World {
        let mut chunk = Chunk::empty();
        for lz in 0..EDGE {
            for lx in 0..EDGE {
                let m = mask(lx, lz);
                if m != 0 {
                    chunk.overlay[(lz * EDGE + lx) as usize] = material;
                    chunk.overlay_mask[(lz * EDGE + lx) as usize] = m;
                }
            }
        }
        let mut world = World::new();
        world.insert_chunk(pos, chunk);
        world
    }

    #[test]
    fn full_overlay_everywhere_merges_to_one_quad() {
        // Chunk plus all eight neighbors fully covered → every window is
        // case 15 → the interior merges to a single rectangle = 2 tris,
        // with no border contour anywhere.
        let mut world = World::new();
        for dz in -1..=1 {
            for dx in -1..=1 {
                let mut c = Chunk::empty();
                c.overlay = [Material::Stone; CELLS_PER_CHUNK_AREA];
                c.overlay_mask = [0xFFFF; CELLS_PER_CHUNK_AREA];
                world.insert_chunk(ChunkPos { x: dx, z: dz }, c);
            }
        }
        let tris = mesh_chunk(&world, ChunkPos { x: 0, z: 0 });
        assert_eq!(
            overlay_tris(&tris),
            2,
            "fully covered neighborhood merges to one overlay quad",
        );
    }

    #[test]
    fn single_covered_subcell_emits_four_corner_triangles() {
        // One covered subcell is a corner sample of four windows, each
        // seeing exactly one inside corner → one triangle each → 4 tris.
        let world = world_with_overlay(ChunkPos { x: 0, z: 0 }, Material::Water, |x, z| {
            u16::from((x, z) == (5, 5))
        });
        let tris = mesh_chunk(&world, ChunkPos { x: 0, z: 0 });
        assert_eq!(overlay_tris(&tris), 4, "one subcell → four corner tris");
    }

    #[test]
    fn straight_edge_crossings_are_collinear_across_a_cell_boundary() {
        // Cover every subcell with global subcell-x < 6: cell (0,0) full,
        // cell (1,0)'s two left columns, then empty — a straight vertical
        // overlay edge that runs the full chunk height and whose covered
        // region continues across the cell (0,0)/(1,0) boundary. The
        // marching crossing on that edge must share one x (x = 1.5 m); a
        // misread apron / cell boundary would kink or shift it.
        let world = world_with_overlay(ChunkPos { x: 0, z: 0 }, Material::Stone, |cx, _| {
            let mut mask = 0u16;
            for sub_z in 0..SUB {
                for sub_x in 0..SUB {
                    if cx * SUB + sub_x < 6 {
                        mask |= 1 << (sub_z * SUB + sub_x);
                    }
                }
            }
            mask
        });
        let tris = mesh_chunk(&world, ChunkPos { x: 0, z: 0 });
        let edge: Vec<&Vertex> = tris
            .iter()
            .flat_map(|t| t.verts.iter())
            .filter(|v| v.y == OVERLAY_Y_LIFT)
            .collect();
        // No overlay geometry extends past the straight edge (interior
        // merge tops out at x = 1.375 m; the contour crossings sit at
        // 1.5 m), and every crossing on that far edge shares one x — a
        // kink from a misread apron / cell boundary would put a crossing
        // short of 1.5.
        let max_x = edge.iter().map(|v| v.x).fold(f32::MIN, f32::max);
        assert_eq!(
            max_x, 1.5,
            "the covered region's right edge is at x = 1.5 m"
        );
        let right_edge: Vec<f32> = edge.iter().map(|v| v.x).filter(|&x| x > 1.4).collect();
        assert!(!right_edge.is_empty());
        assert!(
            right_edge.iter().all(|&x| x == 1.5),
            "the right edge is one straight line, unbroken across the cell boundary",
        );
        // And it runs the chunk height, crossing every internal z cell
        // boundary rather than terminating at one.
        let edge_zs: Vec<f32> = edge.iter().filter(|v| v.x == 1.5).map(|v| v.z).collect();
        let min_z = edge_zs.iter().copied().fold(f32::MAX, f32::min);
        let max_z = edge_zs.iter().copied().fold(f32::MIN, f32::max);
        assert!(
            min_z < 1.0 && max_z > 15.0,
            "the straight edge spans the chunk height",
        );
    }

    #[test]
    fn saddle_resolves_to_the_connected_rule() {
        // Case 5 (BL + TR inside) must resolve by the fixed rule that
        // connects the inside diagonal through the center — a 6-vertex
        // hexagon fanned to 4 triangles, one of which joins the BL and TR
        // corners. A disconnected rule would emit two separate corner
        // triangles that never share the diagonal.
        let mut tris = Vec::new();
        emit_window_contour(2, 2, [0, 0], 5, [0.0; 3], &mut tris);
        assert_eq!(tris.len(), 4, "the connected hexagon fans to 4 triangles");
        // BL corner center = (160,160) oct = (0.625,0.625) m; TR = (224,224)
        // oct = (0.875,0.875) m.
        let joins_diagonal = tris.iter().any(|t| {
            let has = |x: f32, z: f32| {
                t.verts
                    .iter()
                    .any(|v| (v.x - x).abs() < 1e-6 && (v.z - z).abs() < 1e-6)
            };
            has(0.625, 0.625) && has(0.875, 0.875)
        });
        assert!(
            joins_diagonal,
            "the connected rule joins the inside diagonal"
        );
    }

    #[test]
    fn overlay_geometry_sits_at_the_chunks_world_position() {
        // The overlay pass emits in world space like the underlay: a chunk
        // away from the origin must place its overlay geometry over its own
        // world cells, not stacked back at the origin. A full-coverage
        // chunk at (1, 1) spans world cells x,z in [16, 32], so every
        // overlay vertex must land in that meter range — not [0, 16].
        let mut chunk = Chunk::empty();
        chunk.overlay = [Material::Water; CELLS_PER_CHUNK_AREA];
        chunk.overlay_mask = [0xFFFF; CELLS_PER_CHUNK_AREA];
        let mut world = World::new();
        world.insert_chunk(ChunkPos { x: 1, z: 1 }, chunk);
        let tris = mesh_chunk(&world, ChunkPos { x: 1, z: 1 });
        let overlay: Vec<&Vertex> = tris
            .iter()
            .flat_map(|t| t.verts.iter())
            .filter(|v| v.y == OVERLAY_Y_LIFT)
            .collect();
        assert!(
            !overlay.is_empty(),
            "a covered chunk emits overlay geometry"
        );
        for v in overlay {
            assert!(
                (16.0..=32.0).contains(&v.x) && (16.0..=32.0).contains(&v.z),
                "overlay vertex ({}, {}) escaped chunk (1,1)'s world extent",
                v.x,
                v.z,
            );
        }
    }

    #[test]
    fn every_vertex_is_on_the_octimeter_lattice() {
        // Both passes must land every vertex on a 1/256 m multiple —
        // integer meters for the underlay, 32-octimeter crossings plus the
        // one-octimeter lift for the overlay.
        let mut chunk = Chunk::empty();
        chunk.underlay = [Material::Grass; CELLS_PER_CHUNK_AREA];
        for (i, m) in chunk.overlay.iter_mut().enumerate() {
            if i % 3 == 0 {
                *m = Material::Stone;
            }
        }
        for (i, mask) in chunk.overlay_mask.iter_mut().enumerate() {
            if i % 3 == 0 {
                *mask = 0x0C3F; // an irregular partial-coverage mask
            }
        }
        let mut world = World::new();
        world.insert_chunk(ChunkPos { x: 0, z: 0 }, chunk);
        let tris = mesh_chunk(&world, ChunkPos { x: 0, z: 0 });
        assert!(!tris.is_empty());
        for t in &tris {
            for v in &t.verts {
                for coord in [v.x, v.y, v.z] {
                    assert_eq!(
                        (coord * 256.0).fract(),
                        0.0,
                        "vertex coord {coord} is not a 1/256 m multiple",
                    );
                }
            }
        }
    }

    #[test]
    fn mesher_reads_the_underlay_cascade_not_the_raw_plane() {
        // An all-Void underlay plane whose cells point at a region whose
        // default is Grass → the mesher sees Grass and emits geometry.
        let mut chunk = Chunk::empty();
        chunk.region = [1u16; CELLS_PER_CHUNK_AREA];
        let mut world = World::new();
        world.insert_chunk(ChunkPos { x: 0, z: 0 }, chunk);
        world.insert_region(
            1,
            Region {
                name: "meadow".into(),
                default_material: Material::Grass,
            },
        );
        let tris = mesh_chunk(&world, ChunkPos { x: 0, z: 0 });
        assert_eq!(
            underlay_tris(&tris),
            512,
            "cascade-resolved Grass fills the chunk"
        );
    }
}
