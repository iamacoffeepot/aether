use aether_capabilities::render::{DrawTriangle, Vertex};
use aether_math::Rgb;

use crate::world::{CellPos, ChunkPos, Material, World};

use super::constants::{
    EDGE, MAX_APRON_SUBCELLS, OCTIMETERS_PER_METER, OCTIMETERS_PER_SUBCELL, SUB,
};
use super::contour::push_quad;
use super::style::{StyleTable, flat_color};

/// The floor a Void point grooves to — the bordering solid cell whose
/// `cliff_material` colors it — or `None` when the point is an open
/// silhouette edge. A groove is an **authored
/// depression**: the point's [`World::point_height`] stands below its cell's
/// base ground ([`World::height`], so a negative authored delta), *and* the
/// joint is enclosed by solid within the fill-over reach
/// ([`void_fill_border`]). An inherited Void point — a raised plateau's
/// cut-away silhouette rather than a dug well — carries the cell's base
/// height, not a floor, so it does not emit a groove cap. The floor level
/// itself is the void point's `point_height`.
fn void_groove_floor(world: &World, cell: CellPos, sub_x: i32, sub_z: i32) -> Option<CellPos> {
    if world.underlay_point(cell, sub_x, sub_z) != Material::Void {
        return None;
    }
    if world.point_height(cell, sub_x, sub_z) >= world.height(cell) {
        return None; // no authored depression — an open edge, not a groove
    }
    let gx = cell.x * SUB + sub_x;
    let gz = cell.z * SUB + sub_z;
    void_fill_border(world, gx, gz)
}

/// The color source for a Void point's fill-over floor, or `None` when the
/// point is not enclosed. The void point at global subcell `(gx0, gz0)` is
/// enclosed when every one of the four axis directions reaches a solid point
/// within [`MAX_APRON_SUBCELLS`] subcells — a bounded march past void; the
/// returned cell is the closest such solid point's, whose `cliff_material`
/// colors the groove. A direction that runs to the bound still in void means
/// the void reaches the world border within reach: an open edge, no floor.
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
            return None; // open toward this side within reach — no groove floor
        }
    }
    nearest.map(|(_, cell)| cell)
}

/// Emit the fill-over floor caps: every chunk-local Void point that
/// [`void_groove_floor`] proves an enclosed authored depression floors over at
/// its stored point height in the bordering cell's cliff material as a flat
/// color. The level-contour loft drops the groove's rim to this same stored
/// height, so the floor and its walls meet. A cell with no floored Void point
/// emits nothing, so a solid world — and a plateau's cut-away silhouette,
/// whose Void points carry no authored depression — stays byte-identical.
pub(super) fn emit_void_floors(
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
                    let Some(border) = void_groove_floor(world, cell, si, sj) else {
                        continue; // open edge or no authored depression — no floor
                    };
                    let y = world.point_height(cell, si, sj) as f32 / OCTIMETERS_PER_METER;
                    let rgb = flat_color(styles.get(world.cliff_material(border)));
                    let x0 = cell.x * 256 + si * OCTIMETERS_PER_SUBCELL;
                    let z0 = cell.z * 256 + sj * OCTIMETERS_PER_SUBCELL;
                    let vertex = |wx: f32, wz: f32| Vertex {
                        x: wx,
                        y,
                        z: wz,
                        color: Rgb::new(rgb[0], rgb[1], rgb[2]),
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
