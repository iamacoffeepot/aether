//! The ear in material space: the baked voxel grid and the one-shot surface
//! extraction that turns it into a shared-corner triangle list.
//!
//! Both halves run exactly once, at `init`, and neither depends on a pose. What
//! the actor animates is the *map* from material space to world space, and the
//! property that makes that map safe to animate is built here: the corner
//! lattice is dense and indexed by position, so adjacent voxels reference the
//! **same** corner by construction. Displacing a corner moves every face that
//! meets there, and a watertight surface stays watertight under any
//! displacement, however violent, because there is no second copy of a corner
//! to drift away from the first.
//!
//! The dense corner grid earns its keep twice over. The surface needs only the
//! corners on exposed faces, but the fold guard needs the eight corners of every
//! **occupied** cell — interior cells included — to finite-difference a
//! Jacobian. Indexing corners by lattice position rather than by allocation
//! order gives both consumers the same array with no second mapping to keep in
//! sync.
//!
//! Unlike a small-rotation demo, this one cannot bake Lambert shading at
//! extraction time: the twist and fold rotate faces through most of a
//! hemisphere, so a rest-pose normal would be wrong by the time it mattered.
//! Only the flat class color is stored here; shading is recomputed per frame
//! from the posed triangle in [`crate::shade`].

use aether_math::{Rgb, Vec3};

use crate::data::{BOX_DIMS, RIG_BASE, VOXELS};

/// Lattice extent along each axis, in cells, straight from the dataset.
pub const CELLS_X: usize = BOX_DIMS[0];
pub const CELLS_Y: usize = BOX_DIMS[1];
pub const CELLS_Z: usize = BOX_DIMS[2];

/// Corner lattice extent — one more than the cell extent along each axis.
pub const CORNERS_X: usize = CELLS_X + 1;
pub const CORNERS_Y: usize = CELLS_Y + 1;
pub const CORNERS_Z: usize = CELLS_Z + 1;

/// Every corner of the lattice gets a slot, occupied neighbour or not.
pub const CORNER_COUNT: usize = CORNERS_X * CORNERS_Y * CORNERS_Z;

/// World height of one instance at rest, chosen so an ear reads at a
/// conversational camera distance rather than as a speck or a wall.
pub const EAR_HEIGHT: f32 = 1.5;

/// World-space edge length of one lattice cell. Derived from [`EAR_HEIGHT`] and
/// the dataset's own vertical extent so the two cannot drift apart.
#[allow(clippy::cast_precision_loss)] // Lattice extents are small integers.
pub const CELL_SIZE: f32 = EAR_HEIGHT / CELLS_Y as f32;

/// Lattice coordinate that maps to an instance's local origin. The rig base is
/// put on the local vertical axis and the box floor on `y = 0`, so an instance
/// stands on its own origin and a camera aimed between the two instances needs
/// no per-instance offset baked into it.
const ORIGIN_CELL: Vec3 = Vec3::new(RIG_BASE[0], 0.0, RIG_BASE[2]);

/// Class labels present in this ear. The vocabulary is shared across subjects,
/// so most of it never appears here — a fox ear is fur and cavity, no skin.
const LABEL_HAIR: u8 = 3;
const LABEL_INNER_EAR: u8 = 4;
const LABEL_TUFT: u8 = 5;

/// Russet-amber fur, the ear's outer mass.
const HAIR: Rgb = Rgb::new(0.66, 0.34, 0.15);
/// Cream, the tuft of fur standing inside the cavity.
const TUFT: Rgb = Rgb::new(0.93, 0.88, 0.75);
/// Dusky pink, the cavity itself.
const INNER_EAR: Rgb = Rgb::new(0.78, 0.46, 0.49);
/// Anything the dataset labels outside the three classes above. Present so an
/// unexpected label renders as an obvious grey rather than vanishing.
const UNCLASSED: Rgb = Rgb::new(0.55, 0.55, 0.55);

/// The six cell faces, each as an outward normal plus its four corner offsets
/// wound counter-clockwise seen from outside. Shared with [`crate::slab`],
/// which builds its plate as a box in its own right-handed frame and so needs
/// exactly this winding.
pub const FACES: [(Vec3, [[usize; 3]; 4]); 6] = [
    (Vec3::X, [[1, 0, 0], [1, 1, 0], [1, 1, 1], [1, 0, 1]]),
    (Vec3::new(-1.0, 0.0, 0.0), [[0, 0, 0], [0, 0, 1], [0, 1, 1], [0, 1, 0]]),
    (Vec3::Y, [[0, 1, 0], [0, 1, 1], [1, 1, 1], [1, 1, 0]]),
    (Vec3::new(0.0, -1.0, 0.0), [[0, 0, 0], [1, 0, 0], [1, 0, 1], [0, 0, 1]]),
    (Vec3::Z, [[0, 0, 1], [1, 0, 1], [1, 1, 1], [0, 1, 1]]),
    (Vec3::new(0.0, 0.0, -1.0), [[0, 0, 0], [0, 1, 0], [1, 1, 0], [1, 0, 0]]),
];

/// One occupied cell, as the eight shared corner indices the Jacobian
/// difference reads. Ordered so index `dx + 2·dy + 4·dz` is the corner at
/// offset `(dx, dy, dz)`, which is what makes the three difference columns a
/// fixed index pattern rather than a lookup.
pub struct Cell {
    pub corners: [u32; 8],
}

/// The extracted surface. `rest` is the dense corner lattice in an instance's
/// local world space; `triangles` indexes into it; `colors` carries one flat
/// class color per triangle; `tri_cell` records which entry of `cells` a
/// triangle came from, so a per-cell quantity (the det-J tint) reaches its
/// triangles without a search. All four are written once and read-only after.
pub struct Surface {
    pub rest: Vec<Vec3>,
    pub triangles: Vec<[u32; 3]>,
    pub colors: Vec<Rgb>,
    pub tri_cell: Vec<u32>,
    pub cells: Vec<Cell>,
}

/// Fill the lattice from the baked dataset and extract its boundary onto the
/// dense corner lattice. Runs once.
#[must_use]
pub fn build() -> Surface {
    let solid = fill_lattice();

    let mut surface = Surface {
        rest: (0..CORNER_COUNT).map(corner_position).collect(),
        triangles: Vec::new(),
        colors: Vec::new(),
        tri_cell: Vec::new(),
        cells: Vec::new(),
    };

    for z in 0..CELLS_Z {
        for y in 0..CELLS_Y {
            for x in 0..CELLS_X {
                let Some(label) = solid[cell_index(x, y, z)] else {
                    continue;
                };

                #[allow(clippy::cast_possible_truncation)] // At most 1292 occupied cells.
                let cell = surface.cells.len() as u32;
                surface.cells.push(Cell { corners: cell_corners([x, y, z]) });

                let color = color_for(label);
                for (normal, offsets) in FACES {
                    if is_solid(&solid, [x, y, z], normal) {
                        continue;
                    }
                    let quad = offsets.map(|offset| {
                        #[allow(clippy::cast_possible_truncation)] // The corner lattice is 6800 entries.
                        {
                            corner_index([x + offset[0], y + offset[1], z + offset[2]]) as u32
                        }
                    });
                    surface.triangles.push([quad[0], quad[1], quad[2]]);
                    surface.triangles.push([quad[0], quad[2], quad[3]]);
                    surface.colors.push(color);
                    surface.colors.push(color);
                    surface.tri_cell.push(cell);
                    surface.tri_cell.push(cell);
                }
            }
        }
    }

    surface
}

/// World position of a lattice coordinate, fractional coordinates included.
/// The single definition of the material-space → local-world map; the rig and
/// the contact plane anchor themselves through this rather than restating the
/// offset and the scale.
#[must_use]
pub const fn world_from_cell(cell: [f32; 3]) -> Vec3 {
    Vec3::new(
        (cell[0] - ORIGIN_CELL.x) * CELL_SIZE,
        (cell[1] - ORIGIN_CELL.y) * CELL_SIZE,
        (cell[2] - ORIGIN_CELL.z) * CELL_SIZE,
    )
}

/// Flat index of a lattice corner. Position *is* identity here — this is the
/// whole of the corner-sharing mechanism.
#[must_use]
pub const fn corner_index(corner: [usize; 3]) -> usize {
    (corner[2] * CORNERS_Y + corner[1]) * CORNERS_X + corner[0]
}

const fn cell_index(x: usize, y: usize, z: usize) -> usize {
    (z * CELLS_Y + y) * CELLS_X + x
}

fn fill_lattice() -> Vec<Option<u8>> {
    let mut solid = vec![None; CELLS_X * CELLS_Y * CELLS_Z];
    for &[i, j, k, label] in VOXELS {
        solid[cell_index(usize::from(i), usize::from(j), usize::from(k))] = Some(label);
    }
    solid
}

/// The eight corner indices of one cell, in `dx + 2·dy + 4·dz` order.
fn cell_corners(cell: [usize; 3]) -> [u32; 8] {
    let mut corners = [0u32; 8];
    for (slot, corner) in corners.iter_mut().enumerate() {
        let offset = [slot & 1, (slot >> 1) & 1, (slot >> 2) & 1];
        #[allow(clippy::cast_possible_truncation)] // The corner lattice is 6800 entries.
        {
            *corner = corner_index([cell[0] + offset[0], cell[1] + offset[1], cell[2] + offset[2]]) as u32;
        }
    }
    corners
}

/// Local-world position of the corner at a flat index.
fn corner_position(index: usize) -> Vec3 {
    let x = index % CORNERS_X;
    let y = (index / CORNERS_X) % CORNERS_Y;
    let z = index / (CORNERS_X * CORNERS_Y);
    #[allow(clippy::cast_precision_loss)] // Lattice coordinates are far below f32's exact-integer range.
    world_from_cell([x as f32, y as f32, z as f32])
}

/// Whether the neighbor of `cell` across the face with outward `normal` is
/// filled. Out of bounds counts as empty, so the lattice boundary is surface.
// Face normals are exactly ±1 or 0, and the bounds check rules out a negative
// before the index cast.
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap, clippy::cast_sign_loss)]
fn is_solid(solid: &[Option<u8>], cell: [usize; 3], normal: Vec3) -> bool {
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

const fn color_for(label: u8) -> Rgb {
    match label {
        LABEL_HAIR => HAIR,
        LABEL_INNER_EAR => INNER_EAR,
        LABEL_TUFT => TUFT,
        _ => UNCLASSED,
    }
}
