use alloc::vec::Vec;

use aether_render::DrawTriangle;

use crate::world::World;

use super::cliffs::CliffPlan;
use super::style::StyleTable;

/// Emit every physical-cliff ribbon from the same bounded [`CliffPlan`]
/// that partitioned the caps. The plan's named side anchors are the only
/// height source: wall rings clone the cap positions instead of predicting
/// closure from a separate lattice or material-contour walk.
pub(super) fn emit_walls(world: &World, styles: &StyleTable, cliffs: &CliffPlan, tris: &mut Vec<DrawTriangle>) {
    cliffs.emit_walls(world, styles, tris);
}
