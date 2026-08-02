use crate::world::{CellPos, World};

use super::constants::{OCTIMETERS_PER_METER, OCTIMETERS_PER_SUBCELL, SUB};

fn cell_at(wx: f32, wz: f32) -> CellPos {
    CellPos { x: floor_to_i32(wx), z: floor_to_i32(wz) }
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
        return SubPatch::of(world, side.cell, side.sub_x.rem_euclid(SUB), side.sub_z.rem_euclid(SUB)).y(wx, wz);
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
        Self { x0: cell.x as f32, z0: cell.z as f32, corners: world.cell_corner_heights(cell) }
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
/// of stretching down the face (the wall draws the face). On continuous
/// ground the two forms agree and the rule is invisible. A material-break
/// crossing does not come here — it carries its side as data and lifts
/// through [`anchored_lift`].
pub(super) fn label_lift(world: &World, owner: CellPos, wx: f32, wz: f32) -> f32 {
    let cell = cell_at(wx, wz);
    if cell != owner && world.edge_is_cliff(cell, owner) {
        return cell_or_relief_lift(world, owner, wx, wz);
    }
    cell_or_relief_lift(world, cell, wx, wz)
}

/// Evaluate the committed surface through one explicit sample-side anchor.
/// The bounded cliff planner carries this anchor as data for both cap and
/// wall vertices, so neither path re-infers a side from a contour position.
/// The position stays within one half-sample of the anchor in every local
/// case; the selected cell/subcell patch therefore clamps only at its own
/// authored boundary.
#[derive(Clone, Copy)]
pub(super) struct SurfaceAnchor {
    pub(super) x_octimeters: i32,
    pub(super) z_octimeters: i32,
}

pub(super) fn side_anchor_lift(world: &World, anchor: SurfaceAnchor, wx: f32, wz: f32) -> f32 {
    let anchor_cell = CellPos { x: anchor.x_octimeters.div_euclid(256), z: anchor.z_octimeters.div_euclid(256) };
    if world.cell_has_height_relief(anchor_cell) {
        let sub_x = anchor.x_octimeters.rem_euclid(256) / OCTIMETERS_PER_SUBCELL;
        let sub_z = anchor.z_octimeters.rem_euclid(256) / OCTIMETERS_PER_SUBCELL;
        SubPatch::of(world, anchor_cell, sub_x, sub_z).y(wx, wz)
    } else {
        CellLift::of(world, anchor_cell).y(wx, wz)
    }
}

/// The lift for a vertex of a clipped flap fragment: position-pure through
/// the vertex's own (floor) subcell, except that a vertex lying exactly on
/// a clipped break line reads the subcell on the **fragment's** side of it
/// (`sides` per axis, from [`emit_clipped_flap`]) — the high fragment holds
/// its plate, the low fragment holds the ground, and the wall classes close
/// the vertical gap on the same subcells. Off the break lines the two forms
/// agree wherever the plates connect, so continuous relief stays seamless.
/// The whole-cell (relief-free) lift resolves through the fragment's own
/// side cell as well, so a clipped relief-free material-boundary fragment
/// reads its own plate; off any break line the side cell is the vertex's own
/// floor cell, leaving an unclipped relief-free vertex unchanged.
pub(super) fn fragment_lift(world: &World, owner: CellPos, sides: [Option<(i32, bool)>; 2], wx: f32, wz: f32) -> f32 {
    let cell = CellPos { x: floor_to_i32(wx), z: floor_to_i32(wz) };
    // The subcell on the fragment's own side of any clipped break line (per
    // axis): on the line, `sides` says which side; off it, the floor subcell.
    let sub_of = |w: f32, side: Option<(i32, bool)>| -> i32 {
        if let Some((line, above)) = side {
            let oct = w * OCTIMETERS_PER_METER;
            if (oct - line as f32).abs() < 0.5 {
                let lattice = line / OCTIMETERS_PER_SUBCELL;
                return if above {
                    lattice
                } else {
                    lattice - 1
                };
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
    // height (the twin of the `anchored_lift` bug, which left chamfered
    // relief-free corners with unclosed overhang slivers).
    let side_cell = CellPos { x: sx.div_euclid(SUB), z: sz.div_euclid(SUB) };
    side_resolved_lift(world, cell, owner, SideLiftSample { cell: side_cell, sub_x: sx, sub_z: sz }, wx, wz)
}

/// Floor to `i32` — `as i32` truncates toward zero, which is wrong for
/// negative world coordinates, so step down when it rounded up.
pub(super) fn floor_to_i32(v: f32) -> i32 {
    let t = v as i32;
    if (t as f32) > v {
        t - 1
    } else {
        t
    }
}

/// The effective point surface level in octimeters at octimeter position
/// `(px, pz)` — the cell and subcell it floors into, resolved through
/// [`World::point_surface_level`]. The marched wall gate samples this so its
/// break reads the same authored relief the cap drew.
pub(super) fn point_surface_level_at(world: &World, px: i32, pz: i32) -> i32 {
    let cell = CellPos { x: px.div_euclid(256), z: pz.div_euclid(256) };
    let sub_x = px.rem_euclid(256) / OCTIMETERS_PER_SUBCELL;
    let sub_z = pz.rem_euclid(256) / OCTIMETERS_PER_SUBCELL;
    world.point_surface_level(cell, sub_x, sub_z)
}
