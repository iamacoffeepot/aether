use alloc::collections::BTreeSet;
use alloc::vec::Vec;

use crate::world::{CellPos, ChunkPos, Material, STEP_MAX_OCTIMETERS, World};

use super::constants::{
    OCTIMETERS_PER_METER, OCTIMETERS_PER_SUBCELL, SUB, SUBCELLS_PER_CHUNK_EDGE,
};

/// One discrete cliff transition found between adjacent surface-level
/// samples. The scalar coverage projection maps `low` to zero and `high`
/// to full coverage; any authored levels between them retain their fraction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct CliffStep {
    pub(super) low: i32,
    pub(super) high: i32,
}

/// The point-surface samples for one chunk plus its contour apron. `levels`
/// and `solid` are parallel `width × width` planes at subcell stride.
pub(super) struct LevelPlane {
    pub(super) levels: Vec<i32>,
    pub(super) solid: Vec<bool>,
    pub(super) width: usize,
}

fn cell_at(wx: f32, wz: f32) -> CellPos {
    CellPos {
        x: floor_to_i32(wx),
        z: floor_to_i32(wz),
    }
}

fn cell_or_relief_lift(world: &World, cell: CellPos, wx: f32, wz: f32) -> f32 {
    if world.cell_has_height_relief(cell) {
        SubPatch::containing(world, cell, wx, wz).y(wx, wz)
    } else {
        CellLift::of(world, cell).y(wx, wz)
    }
}

struct SideLiftSample {
    cell: CellPos,
    sub_x: i32,
    sub_z: i32,
}

fn side_resolved_lift(
    world: &World,
    sample_cell: CellPos,
    owner: CellPos,
    side: SideLiftSample,
    wx: f32,
    wz: f32,
) -> f32 {
    if sample_cell != owner && world.edge_is_cliff(sample_cell, owner) {
        if world.cell_has_height_relief(owner) {
            return SubPatch::containing(world, owner, wx, wz).y(wx, wz);
        }
        return CellLift::of(world, side.cell).y(wx, wz);
    }
    if world.cell_has_height_relief(sample_cell) {
        return SubPatch::of(
            world,
            side.cell,
            side.sub_x.rem_euclid(SUB),
            side.sub_z.rem_euclid(SUB),
        )
        .y(wx, wz);
    }
    CellLift::of(world, side.cell).y(wx, wz)
}

/// One cell's bilinear surface patch: the four plate-resolved corner
/// heights ([`World::cell_corner_heights`]) plus the height and shade
/// evaluations the mesher builds vertices from. Owner-pinned — a vertex
/// on a cliff edge evaluated through the high cell's patch reads the
/// high plate, which is exactly how the drawn break lands on the cliff
/// line.
pub(super) struct CellLift {
    x0: f32,
    z0: f32,
    corners: [f32; 4],
}

impl CellLift {
    pub(super) fn of(world: &World, cell: CellPos) -> Self {
        Self {
            x0: cell.x as f32,
            z0: cell.z as f32,
            corners: world.cell_corner_heights(cell),
        }
    }

    /// The patch height at `(wx, wz)` meters, coordinates clamped to the
    /// cell span — [`World::surface_height_in`] over the cached corners.
    pub(super) fn y(&self, wx: f32, wz: f32) -> f32 {
        let fx = (wx - self.x0).clamp(0.0, 1.0);
        let fz = (wz - self.z0).clamp(0.0, 1.0);
        let bottom = self.corners[0] + (self.corners[1] - self.corners[0]) * fx;
        let top = self.corners[2] + (self.corners[3] - self.corners[2]) * fx;
        bottom + (top - bottom) * fz
    }
}

/// One subcell's bilinear surface patch — the point-lattice analogue of
/// [`CellLift`], spanning `1 / SUB` m. Built only where a cell carries
/// authored height relief; its four corners are the point-plate heights
/// ([`World::subcell_corner_heights`]) the mesher lifts per-point caps and
/// wall tops through. [`World::surface_height_in`]'s relief branch resolves
/// the identical patch, so drawn and stood-on agree over authored relief.
pub(super) struct SubPatch {
    x0: f32,
    z0: f32,
    corners: [f32; 4],
}

impl SubPatch {
    pub(super) fn of(world: &World, cell: CellPos, sub_x: i32, sub_z: i32) -> Self {
        Self {
            x0: cell.x as f32 + sub_x as f32 / SUB as f32,
            z0: cell.z as f32 + sub_z as f32 / SUB as f32,
            corners: world.subcell_corner_heights(cell, sub_x, sub_z),
        }
    }

    /// The subcell of `cell` containing `(wx, wz)`, coordinates clamped to
    /// the cell span so an off-cell caller reads the nearest edge subcell —
    /// the same selection [`World::surface_height_in`] makes.
    pub(super) fn containing(world: &World, cell: CellPos, wx: f32, wz: f32) -> Self {
        let sub = SUB as f32;
        let local_x = ((wx - cell.x as f32) * sub).clamp(0.0, sub);
        let local_z = ((wz - cell.z as f32) * sub).clamp(0.0, sub);
        let sub_x = floor_to_i32(local_x).clamp(0, SUB - 1);
        let sub_z = floor_to_i32(local_z).clamp(0, SUB - 1);
        Self::of(world, cell, sub_x, sub_z)
    }

    pub(super) fn y(&self, wx: f32, wz: f32) -> f32 {
        let sub = SUB as f32;
        let fx = ((wx - self.x0) * sub).clamp(0.0, 1.0);
        let fz = ((wz - self.z0) * sub).clamp(0.0, 1.0);
        let bottom = self.corners[0] + (self.corners[1] - self.corners[0]) * fx;
        let top = self.corners[2] + (self.corners[3] - self.corners[2]) * fx;
        bottom + (top - bottom) * fz
    }
}

/// The lift for a vertex of label geometry owned by `owner`: position-pure
/// through the vertex's own (floor) cell, unless that cell stands a cliff
/// apart from the owner — then the owner's clamped patch wins, so a
/// boundary polygon at a cliff stays on its own side of the break instead
/// of stretching down the face (the level loft draws the face). On
/// continuous ground the two forms agree and the rule is invisible.
pub(super) fn label_lift(world: &World, owner: CellPos, wx: f32, wz: f32) -> f32 {
    let cell = cell_at(wx, wz);
    if cell != owner && world.edge_is_cliff(cell, owner) {
        return cell_or_relief_lift(world, owner, wx, wz);
    }
    cell_or_relief_lift(world, cell, wx, wz)
}

/// The lift for a vertex of a clipped flap fragment: position-pure through
/// the vertex's own (floor) subcell, except that a vertex lying exactly on
/// a clipped break line reads the subcell on the **fragment's** side of it
/// (`sides` per axis, from the partition-window clip) — the high fragment holds
/// its plate and the low fragment holds the ground while the level loft owns
/// the vertical gap. Off the break lines the two forms
/// agree wherever the plates connect, so continuous relief stays seamless.
/// The whole-cell (relief-free) lift resolves through the fragment's own
/// side cell as well, so a clipped relief-free material-boundary fragment
/// reads its own plate; off any break line the side cell is the vertex's own
/// floor cell, leaving an unclipped relief-free vertex unchanged.
pub(super) fn fragment_lift(
    world: &World,
    owner: CellPos,
    sides: [Option<(i32, bool)>; 2],
    wx: f32,
    wz: f32,
) -> f32 {
    let cell = CellPos {
        x: floor_to_i32(wx),
        z: floor_to_i32(wz),
    };
    // The subcell on the fragment's own side of any clipped break line (per
    // axis): on the line, `sides` says which side; off it, the floor subcell.
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
    // The plate the fragment sits on — its own side cell resolved from the
    // clipped break sides, not the window-center owner. This is what a
    // relief-free clipped material-boundary fragment must read so its lift
    // lands on the correct plate rather than collapsing both sides to one
    // height.
    let side_cell = CellPos {
        x: sx.div_euclid(SUB),
        z: sz.div_euclid(SUB),
    };
    side_resolved_lift(
        world,
        cell,
        owner,
        SideLiftSample {
            cell: side_cell,
            sub_x: sx,
            sub_z: sz,
        },
        wx,
        wz,
    )
}

/// Floor to `i32` — `as i32` truncates toward zero, which is wrong for
/// negative world coordinates, so step down when it rounded up.
pub(super) fn floor_to_i32(v: f32) -> i32 {
    let t = v as i32;
    if (t as f32) > v { t - 1 } else { t }
}

/// The effective point surface level in octimeters at octimeter position
/// `(px, pz)` — the cell and subcell it floors into, resolved through
/// [`World::point_surface_level`]. The scalar level plane samples this so its
/// contour reads the same authored relief the cap drew.
pub(super) fn point_surface_level_at(world: &World, px: i32, pz: i32) -> i32 {
    let cell = CellPos {
        x: px.div_euclid(256),
        z: pz.div_euclid(256),
    };
    let sub_x = px.rem_euclid(256) / OCTIMETERS_PER_SUBCELL;
    let sub_z = pz.rem_euclid(256) / OCTIMETERS_PER_SUBCELL;
    world.point_surface_level(cell, sub_x, sub_z)
}

/// Sample the scalar surface-level field over one chunk plus `apron`
/// subcells on every side. Samples sit at the same subcell centers as the
/// material partition's source plane; the placement code adds the half-step
/// offset when the plane is marched.
pub(super) fn sample_level_plane(world: &World, at: ChunkPos, apron: i32) -> LevelPlane {
    let width = (SUBCELLS_PER_CHUNK_EDGE + 2 * apron) as usize;
    let mut levels = Vec::with_capacity(width * width);
    let mut solid = Vec::with_capacity(width * width);
    for sj in -apron..SUBCELLS_PER_CHUNK_EDGE + apron {
        for si in -apron..SUBCELLS_PER_CHUNK_EDGE + apron {
            let gx = at.x * SUBCELLS_PER_CHUNK_EDGE + si;
            let gz = at.z * SUBCELLS_PER_CHUNK_EDGE + sj;
            let cell = CellPos {
                x: gx.div_euclid(SUB),
                z: gz.div_euclid(SUB),
            };
            let sub_x = gx.rem_euclid(SUB);
            let sub_z = gz.rem_euclid(SUB);
            levels.push(point_surface_level_at(
                world,
                gx * OCTIMETERS_PER_SUBCELL,
                gz * OCTIMETERS_PER_SUBCELL,
            ));
            solid.push(world.underlay_point(cell, sub_x, sub_z) != Material::Void);
        }
    }
    LevelPlane {
        levels,
        solid,
        width,
    }
}

/// Distinct high/low cliff transitions in `plane`, in stable level order.
/// Step discovery reads the level field independently of material solidity:
/// a high Void sample can carry the cell level that reveals the threshold,
/// while [`level_coverage_plane`] still excludes that sample from the cap.
/// This lets a solid island inside a cut-away high cell loft its own outline.
/// Legal slopes at or below the step ceiling remain one plate and do not
/// produce a contour.
pub(super) fn cliff_steps(plane: &LevelPlane) -> Vec<CliffStep> {
    let mut steps = BTreeSet::new();
    for z in 0..plane.width {
        for x in 0..plane.width {
            let i = z * plane.width + x;
            for (nx, nz) in [(x + 1, z), (x, z + 1)] {
                if nx >= plane.width || nz >= plane.width {
                    continue;
                }
                let j = nz * plane.width + nx;
                let (low_i, high_i) = if plane.levels[i] <= plane.levels[j] {
                    (i, j)
                } else {
                    (j, i)
                };
                let low = plane.levels[low_i];
                let high = plane.levels[high_i];
                if i64::from(high) - i64::from(low) > i64::from(STEP_MAX_OCTIMETERS) {
                    steps.insert(CliffStep { low, high });
                }
            }
        }
    }
    steps.into_iter().collect()
}

/// Project one scalar surface level into a cliff step's `0..=255` coverage
/// interval. The low side floors, the high side saturates, and intermediate
/// authored levels preserve their linear fraction so the `127.5` march
/// crosses the actual level isoline instead of a binary midpoint.
pub(super) fn project_level_coverage(level: i32, step: CliffStep) -> u8 {
    if level <= step.low {
        return 0;
    }
    if level >= step.high {
        return u8::MAX;
    }
    let span = i64::from(step.high) - i64::from(step.low);
    let above_low = i64::from(level) - i64::from(step.low);
    ((above_low * i64::from(u8::MAX) + span / 2) / span) as u8
}

/// Project a sampled level plane for one cliff step. Void samples stay
/// uncovered even if their stored lakebed height is numerically high: there
/// is no solid top surface to cap on that side.
pub(super) fn level_coverage_plane(plane: &LevelPlane, step: CliffStep) -> Vec<u8> {
    plane
        .levels
        .iter()
        .zip(&plane.solid)
        .map(|(&level, &solid)| {
            if solid {
                project_level_coverage(level, step)
            } else {
                0
            }
        })
        .collect()
}

/// Whether this subcell is the high side of any physical cliff. The underlay
/// omits these boundary patches so the level-contour cap owns the whole top
/// ribbon at a convex corner instead of leaving a square lattice sliver over
/// a rounded contour.
pub(super) fn subcell_is_high_cliff(world: &World, cell: CellPos, sub_x: i32, sub_z: i32) -> bool {
    let gx = cell.x * SUB + sub_x;
    let gz = cell.z * SUB + sub_z;
    let own = world.point_surface_level(cell, sub_x, sub_z);
    [(1, 0), (-1, 0), (0, 1), (0, -1)]
        .into_iter()
        .any(|(dx, dz)| {
            let nx = gx + dx;
            let nz = gz + dz;
            let neighbor = CellPos {
                x: nx.div_euclid(SUB),
                z: nz.div_euclid(SUB),
            };
            i64::from(own)
                - i64::from(world.point_surface_level(
                    neighbor,
                    nx.rem_euclid(SUB),
                    nz.rem_euclid(SUB),
                ))
                > i64::from(STEP_MAX_OCTIMETERS)
        })
}

/// Whether any point of `cell` contributes a high cliff boundary.
pub(super) fn cell_has_high_cliff(world: &World, cell: CellPos) -> bool {
    (0..SUB).any(|sub_z| (0..SUB).any(|sub_x| subcell_is_high_cliff(world, cell, sub_x, sub_z)))
}
