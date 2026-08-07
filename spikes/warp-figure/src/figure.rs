//! The figure in material space: a voxel lattice humanoid and the one-shot
//! surface extraction that turns it into a shared-corner triangle list.
//!
//! Both halves run exactly once, at `init`. The lattice is the material — a
//! set of filled cells with a body-part color — and never changes afterwards.
//! What the actor animates is the *map* from material space to world space,
//! and the property that makes that map safe to animate is built here:
//! adjacent voxels reference the **same** corner index, so displacing a corner
//! moves every face that touches it. A watertight surface stays watertight
//! under any displacement, however violent, because there is no second copy of
//! a corner to drift away from the first.
//!
//! Extraction culls a face whose neighbor cell is filled, so the emitted
//! triangles are the boundary of the solid rather than six faces per cell.
//! Lambert shading is baked into the per-triangle color at extraction time
//! from the material-space face normal: the palette is flat, so without a
//! shading term the silhouette reads but the form does not.

use aether_math::{Rgb, Vec3};

/// Lattice extent along each axis, in cells. The figure occupies most of the
/// `x` and `y` range and a shallow slab in `z` — chunky and readable rather
/// than detailed.
pub const CELLS_X: usize = 24;
pub const CELLS_Y: usize = 24;
pub const CELLS_Z: usize = 8;

/// World-space edge length of one lattice cell.
pub const CELL_SIZE: f32 = 0.1;

/// Lattice coordinate that maps to the world origin. Chosen so the torso is
/// centered on `x = 0`, the feet rest on `y = 0`, and the body slab straddles
/// `z = 0` — a camera aimed near the origin frames the figure without an
/// offset baked into every caller.
const ORIGIN_CELL: Vec3 = Vec3::new(10.5, 0.0, 4.5);

/// Direction *towards* the key light, normalized at use. Chosen to graze the
/// front (`+Z`) and right (`+X`) faces so the extended arm keeps its lit side
/// facing a camera orbiting the front-right quadrant.
const LIGHT_DIRECTION: Vec3 = Vec3::new(0.45, 0.80, 0.55);

/// Fraction of a face's color that survives with the light fully behind it.
/// Without it a back-facing plane goes pure black and the silhouette swallows
/// the form.
const AMBIENT: f32 = 0.38;

const TROUSERS: Rgb = Rgb::new(0.28, 0.34, 0.52);
const TUNIC: Rgb = Rgb::new(0.78, 0.36, 0.30);
const SLEEVE: Rgb = Rgb::new(0.88, 0.48, 0.38);
const SKIN: Rgb = Rgb::new(0.93, 0.77, 0.63);

/// One axis-aligned box of filled cells, `min` inclusive and `max` exclusive.
struct Part {
    min: [usize; 3],
    max: [usize; 3],
    color: Rgb,
}

/// The humanoid, as boxes. Legs stand under a torso, a neck carries the head,
/// the left arm hangs at the side, and the right arm reaches along `+X` with
/// the hand at the far end — the arm the displacement field stretches.
const PARTS: &[Part] = &[
    Part { min: [7, 0, 3], max: [10, 9, 6], color: TROUSERS },
    Part { min: [11, 0, 3], max: [14, 9, 6], color: TROUSERS },
    Part { min: [6, 9, 2], max: [15, 17, 7], color: TUNIC },
    Part { min: [9, 17, 3], max: [12, 18, 6], color: SKIN },
    Part { min: [8, 18, 2], max: [13, 23, 7], color: SKIN },
    Part { min: [3, 10, 3], max: [6, 17, 6], color: SLEEVE },
    Part { min: [ARM_SHOULDER_CELL, ARM_MIN_Y, ARM_MIN_Z], max: [ARM_HAND_CELL, ARM_MAX_Y, ARM_MAX_Z], color: SLEEVE },
    Part { min: [ARM_HAND_CELL, ARM_MIN_Y, ARM_MIN_Z], max: [CELLS_X, ARM_MAX_Y, ARM_MAX_Z], color: SKIN },
];

/// Lattice plane where the right arm leaves the torso. Corners at or inboard
/// of this plane are pinned by the displacement field, so the arm stays welded
/// to the shoulder no matter how far the hand travels.
pub const ARM_SHOULDER_CELL: usize = 15;

/// Lattice plane where the sleeve ends and the hand begins. The hand runs from
/// here to the lattice edge, which is what makes [`CELLS_X`] the arm's tip.
pub const ARM_HAND_CELL: usize = 22;

const ARM_MIN_Y: usize = 13;
const ARM_MAX_Y: usize = 16;
const ARM_MIN_Z: usize = 3;
const ARM_MAX_Z: usize = 6;

/// Lattice coordinate of the arm's centerline — the axis its cross-section
/// thins toward under stretch. Derived from the arm's own extents so the two
/// cannot drift apart.
#[allow(clippy::cast_precision_loss)] // Lattice extents are small integers.
pub const ARM_AXIS_CELL: [f32; 3] = [0.0, (ARM_MIN_Y + ARM_MAX_Y) as f32 * 0.5, (ARM_MIN_Z + ARM_MAX_Z) as f32 * 0.5];

/// The six cell faces, each as an outward normal plus its four corner offsets
/// wound counter-clockwise seen from outside.
const FACES: [(Vec3, [[usize; 3]; 4]); 6] = [
    (Vec3::X, [[1, 0, 0], [1, 1, 0], [1, 1, 1], [1, 0, 1]]),
    (Vec3::new(-1.0, 0.0, 0.0), [[0, 0, 0], [0, 0, 1], [0, 1, 1], [0, 1, 0]]),
    (Vec3::Y, [[0, 1, 0], [0, 1, 1], [1, 1, 1], [1, 1, 0]]),
    (Vec3::new(0.0, -1.0, 0.0), [[0, 0, 0], [1, 0, 0], [1, 0, 1], [0, 0, 1]]),
    (Vec3::Z, [[0, 0, 1], [1, 0, 1], [1, 1, 1], [0, 1, 1]]),
    (Vec3::new(0.0, 0.0, -1.0), [[0, 0, 0], [0, 1, 0], [1, 1, 0], [1, 0, 0]]),
];

/// The extracted surface. `rest` holds each shared corner's material-space
/// world position; `triangles` indexes into it; `colors` carries one baked
/// shaded color per triangle. Animation rewrites positions derived from
/// `rest` and never touches `triangles` or `colors`.
pub struct Surface {
    pub rest: Vec<Vec3>,
    pub triangles: Vec<[u32; 3]>,
    pub colors: Vec<Rgb>,
}

/// Fill the lattice from [`PARTS`] and extract its boundary onto a shared
/// corner set. Runs once.
#[must_use]
pub fn build() -> Surface {
    let solid = fill_lattice();

    let mut corner_slots = vec![u32::MAX; (CELLS_X + 1) * (CELLS_Y + 1) * (CELLS_Z + 1)];
    let mut surface = Surface { rest: Vec::new(), triangles: Vec::new(), colors: Vec::new() };
    let light = LIGHT_DIRECTION.normalize();

    for z in 0..CELLS_Z {
        for y in 0..CELLS_Y {
            for x in 0..CELLS_X {
                let Some(color) = solid[cell_index(x, y, z)] else {
                    continue;
                };
                for (normal, offsets) in FACES {
                    if is_solid(&solid, [x, y, z], normal) {
                        continue;
                    }
                    let quad = offsets.map(|offset| {
                        share_corner(
                            &mut corner_slots,
                            &mut surface.rest,
                            [x + offset[0], y + offset[1], z + offset[2]],
                        )
                    });
                    let shaded = shade(color, normal, light);
                    surface.triangles.push([quad[0], quad[1], quad[2]]);
                    surface.triangles.push([quad[0], quad[2], quad[3]]);
                    surface.colors.push(shaded);
                    surface.colors.push(shaded);
                }
            }
        }
    }

    surface
}

/// World position of a lattice coordinate, fractional coordinates included.
/// The single definition of the material-space → world map; the displacement
/// field anchors itself through this rather than restating the offset.
#[must_use]
pub const fn world_from_cell(cell: [f32; 3]) -> Vec3 {
    Vec3::new(
        (cell[0] - ORIGIN_CELL.x) * CELL_SIZE,
        (cell[1] - ORIGIN_CELL.y) * CELL_SIZE,
        (cell[2] - ORIGIN_CELL.z) * CELL_SIZE,
    )
}

/// Material-space world position of a lattice corner.
fn corner_position(corner: [usize; 3]) -> Vec3 {
    #[allow(clippy::cast_precision_loss)] // Lattice coordinates are far below f32's exact-integer range.
    world_from_cell([corner[0] as f32, corner[1] as f32, corner[2] as f32])
}

fn fill_lattice() -> Vec<Option<Rgb>> {
    let mut solid = vec![None; CELLS_X * CELLS_Y * CELLS_Z];
    for part in PARTS {
        for z in part.min[2]..part.max[2] {
            for y in part.min[1]..part.max[1] {
                for x in part.min[0]..part.max[0] {
                    solid[cell_index(x, y, z)] = Some(part.color);
                }
            }
        }
    }
    solid
}

const fn cell_index(x: usize, y: usize, z: usize) -> usize {
    (z * CELLS_Y + y) * CELLS_X + x
}

const fn corner_index(corner: [usize; 3]) -> usize {
    (corner[2] * (CELLS_Y + 1) + corner[1]) * (CELLS_X + 1) + corner[0]
}

/// Whether the neighbor of `cell` across the face with outward `normal` is
/// filled. Out of bounds counts as empty, so the lattice boundary is surface.
// Face normals are exactly ±1 or 0, and the bounds check above rules out a
// negative before the index cast.
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap, clippy::cast_sign_loss)]
fn is_solid(solid: &[Option<Rgb>], cell: [usize; 3], normal: Vec3) -> bool {
    let step = [normal.x as isize, normal.y as isize, normal.z as isize];
    let limit = [CELLS_X, CELLS_Y, CELLS_Z];
    let mut neighbor = [0usize; 3];
    for axis in 0..3 {
        let moved = cell[axis] as isize + step[axis];
        if moved < 0 || moved >= limit[axis] as isize {
            return false;
        }
        neighbor[axis] = moved as usize;
    }
    solid[cell_index(neighbor[0], neighbor[1], neighbor[2])].is_some()
}

/// Return the index of `corner`, allocating its rest position the first time
/// any face asks for it. This is the load-bearing step: every face that meets
/// at a corner gets the same index back, so a displacement applied per index
/// moves them together and opens no seam.
fn share_corner(slots: &mut [u32], rest: &mut Vec<Vec3>, corner: [usize; 3]) -> u32 {
    let slot = &mut slots[corner_index(corner)];
    if *slot == u32::MAX {
        #[allow(clippy::cast_possible_truncation)] // The corner lattice is 25 * 25 * 9 entries.
        let next = rest.len() as u32;
        rest.push(corner_position(corner));
        *slot = next;
    }
    *slot
}

fn shade(color: Rgb, normal: Vec3, light: Vec3) -> Rgb {
    let lambert = (1.0 - AMBIENT).mul_add(normal.dot(light).max(0.0), AMBIENT);
    Rgb::new(color.r * lambert, color.g * lambert, color.b * lambert)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tripwire: two cells sharing a face must emit ten quads, not twelve —
    /// the interior pair is culled — and the boundary must land on twelve
    /// distinct corners, not the twenty-four an unshared extraction produces.
    /// The corner count is what the displacement field's watertightness rests
    /// on, so it is asserted directly rather than inferred from the triangles.
    #[test]
    fn adjacent_cells_cull_their_shared_face_and_share_corners() {
        let mut slots = vec![u32::MAX; (CELLS_X + 1) * (CELLS_Y + 1) * (CELLS_Z + 1)];
        let mut rest = Vec::new();
        let mut solid = vec![None; CELLS_X * CELLS_Y * CELLS_Z];
        solid[cell_index(4, 4, 4)] = Some(SKIN);
        solid[cell_index(5, 4, 4)] = Some(SKIN);

        let mut quads = 0;
        for cell in [[4, 4, 4], [5, 4, 4]] {
            for (normal, offsets) in FACES {
                if is_solid(&solid, cell, normal) {
                    continue;
                }
                quads += 1;
                for offset in offsets {
                    share_corner(
                        &mut slots,
                        &mut rest,
                        [cell[0] + offset[0], cell[1] + offset[1], cell[2] + offset[2]],
                    );
                }
            }
        }

        assert_eq!(quads, 10, "the shared interior face pair should be culled");
        assert_eq!(rest.len(), 12, "the two cells' corners should be shared, not duplicated");
    }
}
