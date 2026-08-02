//! Reading and writing the plane stack: the underlay cascade, the
//! per-subcell material and height points, the raw overlay, and the
//! per-cell table resolutions.

use super::coords::CellPos;
use super::layout::{SUBCELLS_PER_CELL, SUBCELLS_PER_CELL_EDGE, UNDERLAY_POINT_INHERIT};
use super::material::Material;
use super::surface::floor_to_i32;
use super::table::SmoothingProfile;
use super::world::World;

impl World {
    /// The cascade-resolved ground material at `cell`: the cell's own
    /// underlay if non-`Void`, else the cell's region's `default_material`,
    /// else `Void`.
    #[must_use]
    pub fn underlay(&self, cell: CellPos) -> Material {
        let Some(chunk) = self.chunks.get(&cell.chunk()) else {
            return Material::Void;
        };
        let idx = cell.chunk_index();
        let own = chunk.underlay[idx];
        if own != Material::Void {
            return own;
        }
        let region_id = chunk.region[idx];
        if region_id != 0
            && let Some(region) = self.regions.get(region_id as usize - 1)
        {
            return region.default_material;
        }
        Material::Void
    }

    /// The material at subcell point `(sub_x, sub_z)` of `cell` (each in
    /// `0..SUB`): the point's explicit [`Material`] if it pins one, else —
    /// the [`UNDERLAY_POINT_INHERIT`] sentinel, or a missing chunk — the
    /// cell's cascade-resolved [`World::underlay`]. This is the sample the
    /// mesher expands the ground from, so an all-inherit cell reads its
    /// single cascade material at every point (identical to a per-cell
    /// underlay), while an authored point shapes the fabric below cell
    /// scale. `sub_x` / `sub_z` fold into the cell's point block, so a
    /// caller passing a subcell that has walked into a neighbor still reads
    /// this cell's plane.
    #[must_use]
    pub fn underlay_point(&self, cell: CellPos, sub_x: i32, sub_z: i32) -> Material {
        let Some(chunk) = self.chunks.get(&cell.chunk()) else {
            return Material::Void;
        };
        let sub = SUBCELLS_PER_CELL_EDGE.cast_signed();
        let within = (sub_z.rem_euclid(sub) * sub + sub_x.rem_euclid(sub)) as usize;
        let byte = chunk.underlay_points[cell.chunk_index() * SUBCELLS_PER_CELL + within];
        if byte == UNDERLAY_POINT_INHERIT {
            return self.underlay(cell);
        }
        Material::from_u8_or_void(byte)
    }

    /// Write `cell`'s `SUB × SUB` underlay material points, creating the
    /// cell's chunk if absent. Each provided byte pins a point (a
    /// [`Material`] or the [`UNDERLAY_POINT_INHERIT`] sentinel); a short
    /// slice leaves the cell's remaining points inheriting, so an empty
    /// slice clears the cell back to all-inherit. Bytes past the cell's
    /// point count are ignored.
    pub fn set_cell_points(&mut self, cell: CellPos, points: &[u8]) {
        super::super::proposal::MutationTarget::set_cell_points(self, cell, points);
    }

    /// Write `cell`'s `SUB × SUB` height deltas (octimeters off the cell's
    /// [`Chunk::height`](crate::world::Chunk::height)), creating the cell's chunk if absent. Mirrors
    /// [`World::set_cell_points`]: a short slice leaves the cell's remaining
    /// points inheriting ([`HEIGHT_POINT_INHERIT`](crate::world::HEIGHT_POINT_INHERIT)), so an empty slice clears
    /// the cell back to no relief; deltas past the cell's point count are
    /// ignored.
    pub fn set_cell_heights(&mut self, cell: CellPos, deltas: &[i16]) {
        super::super::proposal::MutationTarget::set_cell_heights(self, cell, deltas);
    }

    /// The cell's cascade default alone — its region's `default_material`
    /// (`Void` for no region, an unregistered region, or a missing chunk),
    /// ignoring any explicit underlay. The mesher's base/patch split reads
    /// this: a cell whose resolved underlay differs from its own default is
    /// a contoured patch over the default ground.
    #[must_use]
    pub fn cell_default(&self, cell: CellPos) -> Material {
        let Some(chunk) = self.chunks.get(&cell.chunk()) else {
            return Material::Void;
        };
        let region_id = chunk.region[cell.chunk_index()];
        if region_id == 0 {
            return Material::Void;
        }
        self.regions.get(region_id as usize - 1).map_or(Material::Void, |region| region.default_material)
    }

    /// The raw overlay material at `cell` — never cascade-resolved.
    #[must_use]
    pub fn overlay(&self, cell: CellPos) -> Material {
        self.chunks.get(&cell.chunk()).map_or(Material::Void, |chunk| chunk.overlay[cell.chunk_index()])
    }

    /// The raw overlay coverage byte at subcell point `(sub_x, sub_z)` of
    /// `cell` — never cascade-resolved. A missing chunk reads `0` (no
    /// coverage), which is the apron read the mesher relies on: a
    /// chunk-border window can sample one subcell into an absent neighbor
    /// and see empty space rather than panicking. The value is meaningless
    /// where [`World::overlay`] is `Void`.
    #[must_use]
    pub fn overlay_coverage(&self, cell: CellPos, sub_x: i32, sub_z: i32) -> u8 {
        let Some(chunk) = self.chunks.get(&cell.chunk()) else {
            return 0;
        };
        let sub = SUBCELLS_PER_CELL_EDGE.cast_signed();
        let within = (sub_z.rem_euclid(sub) * sub + sub_x.rem_euclid(sub)) as usize;
        chunk.overlay_mask[cell.chunk_index() * SUBCELLS_PER_CELL + within]
    }

    fn overlay_material_coverage(&self, global_subcell_x: i32, global_subcell_z: i32, material: Material) -> u8 {
        let subcells = SUBCELLS_PER_CELL_EDGE.cast_signed();
        let cell = CellPos { x: global_subcell_x.div_euclid(subcells), z: global_subcell_z.div_euclid(subcells) };
        if self.overlay(cell) != material {
            return 0;
        }
        self.overlay_coverage(cell, global_subcell_x.rem_euclid(subcells), global_subcell_z.rem_euclid(subcells))
    }

    fn reconstructed_overlay_coverage(&self, global_subcell_x: i32, global_subcell_z: i32, material: Material) -> u8 {
        super::super::mesher::contour::reconstructed_coverage(
            self.overlay_material_coverage(global_subcell_x, global_subcell_z, material),
            self.overlay_material_coverage(global_subcell_x.saturating_add(1), global_subcell_z, material),
            self.overlay_material_coverage(global_subcell_x, global_subcell_z.saturating_add(1), material),
            self.overlay_material_coverage(
                global_subcell_x.saturating_add(1),
                global_subcell_z.saturating_add(1),
                material,
            ),
            0.5,
            0.5,
        )
    }

    pub(super) fn continuous_overlay_coverage_at(&self, x_meters: f32, z_meters: f32, material: Material) -> f32 {
        let subcells = SUBCELLS_PER_CELL_EDGE as f32;
        let sample_x = x_meters.mul_add(subcells, -0.5);
        let sample_z = z_meters.mul_add(subcells, -0.5);
        let base_x = floor_to_i32(sample_x);
        let base_z = floor_to_i32(sample_z);
        let fraction_x = sample_x - base_x as f32;
        let fraction_z = sample_z - base_z as f32;
        super::super::mesher::contour::interpolated_coverage(
            self.reconstructed_overlay_coverage(base_x, base_z, material),
            self.reconstructed_overlay_coverage(base_x.saturating_add(1), base_z, material),
            self.reconstructed_overlay_coverage(base_x, base_z.saturating_add(1), material),
            self.reconstructed_overlay_coverage(base_x.saturating_add(1), base_z.saturating_add(1), material),
            fraction_x,
            fraction_z,
        )
    }

    /// Elevation at `cell` in octimeters — the raw lakebed read. Unset
    /// cells read `0`. Under a water cell this is the ground beneath the
    /// surface, not the water level ([`World::water_level`] resolves that).
    #[must_use]
    pub fn height(&self, cell: CellPos) -> i32 {
        self.chunks.get(&cell.chunk()).map_or(0, |chunk| chunk.height[cell.chunk_index()])
    }

    /// The lakebed elevation in octimeters at subcell point `(sub_x, sub_z)`
    /// of `cell` (each folded into `0..SUB`): the cell's [`World::height`]
    /// plus the point's authored delta, saturating rather than wrapping at
    /// the `i32` extremes. An inherit ([`HEIGHT_POINT_INHERIT`](crate::world::HEIGHT_POINT_INHERIT)) point — or a
    /// missing chunk — reads the cell height unchanged, so an all-zero plane
    /// resolves at cell stride. Like [`World::height`] this is the raw ground
    /// read: under a water cell it is the lakebed beneath the surface, not the
    /// water level (`World::point_surface_level` resolves the effective
    /// surface).
    #[must_use]
    pub fn point_height(&self, cell: CellPos, sub_x: i32, sub_z: i32) -> i32 {
        let base = self.height(cell);
        let Some(chunk) = self.chunks.get(&cell.chunk()) else {
            return base;
        };
        let sub = SUBCELLS_PER_CELL_EDGE.cast_signed();
        let within = (sub_z.rem_euclid(sub) * sub + sub_x.rem_euclid(sub)) as usize;
        let delta = chunk.height_points[cell.chunk_index() * SUBCELLS_PER_CELL + within];
        base.saturating_add(i32::from(delta))
    }

    /// The material `cell`'s cliff faces wear — its region's
    /// `cliff_material`, or [`Material::Stone`] for no region, an
    /// unregistered region, or a missing chunk.
    #[must_use]
    pub fn cliff_material(&self, cell: CellPos) -> Material {
        let Some(chunk) = self.chunks.get(&cell.chunk()) else {
            return Material::Stone;
        };
        let region_id = chunk.region[cell.chunk_index()];
        if region_id != 0
            && let Some(region) = self.regions.get(region_id as usize - 1)
        {
            return region.cliff_material;
        }
        Material::Stone
    }

    /// The smoothing override at `cell`, if the cell's smoothing plane
    /// points at a registered profile. `None` — plane `0`, missing chunk,
    /// or an unregistered id — means the material default applies.
    #[must_use]
    pub fn smoothing_override(&self, cell: CellPos) -> Option<SmoothingProfile> {
        let chunk = self.chunks.get(&cell.chunk())?;
        let id = chunk.smoothing[cell.chunk_index()];
        if id == 0 {
            return None;
        }
        self.smoothing_profiles.get(id as usize - 1).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::super::chunk::Chunk;
    use super::super::coords::ChunkPos;
    use super::super::fixture::cell;
    use super::super::table::Region;
    use super::*;

    #[test]
    fn underlay_cascade_resolves_cell_then_region_then_void() {
        let mut world = World::new();
        let mut chunk = Chunk::empty();
        // Cell (2,3): explicit Stone underlay — cell override wins.
        chunk.underlay[3 * 16 + 2] = Material::Stone;
        // Cell (4,5): Void underlay but in region 1 → region default.
        chunk.region[5 * 16 + 4] = 1;
        // Cell (6,7): Void underlay, no region → Void.
        world.insert_chunk(ChunkPos { x: 0, z: 0 }, chunk);
        world.insert_region(
            1,
            Region { name: "meadow".into(), default_material: Material::Grass, cliff_material: Material::Stone },
        );

        assert_eq!(world.underlay(cell(2, 3)), Material::Stone, "cell override");
        assert_eq!(world.underlay(cell(4, 5)), Material::Grass, "region default");
        assert_eq!(world.underlay(cell(6, 7)), Material::Void, "no cascade source");
    }

    #[test]
    fn underlay_point_inherits_cascade_or_pins_explicit() {
        let mut world = World::new();
        let mut chunk = Chunk::empty();
        // Cell (2,3): explicit Stone underlay; every point inherits it.
        chunk.underlay[3 * 16 + 2] = Material::Stone;
        // Cell (4,5): Void underlay in region 1 (Grass default); point (0,0)
        // pinned Sand, point (1,0) pinned explicit Void, the rest inherit.
        chunk.region[5 * 16 + 4] = 1;
        let base = (5 * 16 + 4) * SUBCELLS_PER_CELL;
        chunk.underlay_points[base] = Material::Sand.to_u8();
        chunk.underlay_points[base + 1] = Material::Void.to_u8();
        world.insert_chunk(ChunkPos { x: 0, z: 0 }, chunk);
        world.insert_region(
            1,
            Region { name: "meadow".into(), default_material: Material::Grass, cliff_material: Material::Stone },
        );

        // Cell (2,3): inherit points resolve the cell's own paint.
        assert_eq!(world.underlay_point(cell(2, 3), 0, 0), Material::Stone, "inherit resolves the cell paint");
        assert_eq!(world.underlay_point(cell(2, 3), 3, 3), Material::Stone);
        // Cell (4,5): inherit points resolve the region default; explicit
        // points pin, and an explicit Void reads Void even in a painted cell.
        assert_eq!(world.underlay_point(cell(4, 5), 2, 2), Material::Grass, "inherit resolves the region default");
        assert_eq!(world.underlay_point(cell(4, 5), 0, 0), Material::Sand, "an explicit point overrides the cascade");
        assert_eq!(
            world.underlay_point(cell(4, 5), 1, 0),
            Material::Void,
            "an explicit Void point reads Void in a painted cell",
        );
        // Cell (6,7): no cascade source, so an inherit point reads Void.
        assert_eq!(world.underlay_point(cell(6, 7), 0, 0), Material::Void, "no cascade source");
    }

    #[test]
    fn set_cell_points_writes_a_cell_and_a_short_slice_inherits_the_tail() {
        let mut world = World::new();
        let mut chunk = Chunk::empty();
        chunk.underlay[3 * 16 + 3] = Material::Grass; // cell (3,3)
        world.insert_chunk(ChunkPos { x: 0, z: 0 }, chunk);

        // Stamp two Stone points; the short slice leaves the tail inheriting.
        world.set_cell_points(cell(3, 3), &[Material::Stone.to_u8(), Material::Stone.to_u8()]);
        assert_eq!(world.underlay_point(cell(3, 3), 0, 0), Material::Stone);
        assert_eq!(world.underlay_point(cell(3, 3), 1, 0), Material::Stone);
        assert_eq!(
            world.underlay_point(cell(3, 3), 2, 0),
            Material::Grass,
            "the unwritten tail inherits the cell's Grass",
        );
        // An empty slice clears the cell back to all-inherit.
        world.set_cell_points(cell(3, 3), &[]);
        assert_eq!(
            world.underlay_point(cell(3, 3), 0, 0),
            Material::Grass,
            "an empty stamp clears the cell to inherit",
        );
        // A stamp on an absent chunk creates it and pins the point.
        world.set_cell_points(cell(100, 100), &[Material::Sand.to_u8()]);
        assert_eq!(world.underlay_point(cell(100, 100), 0, 0), Material::Sand);
    }

    #[test]
    fn overlay_never_cascades() {
        let mut world = World::new();
        let mut chunk = Chunk::empty();
        // Cell in region 1 with a region default, but Void overlay.
        chunk.region[0] = 1;
        world.insert_chunk(ChunkPos { x: 0, z: 0 }, chunk);
        world.insert_region(
            1,
            Region { name: "r".into(), default_material: Material::Grass, cliff_material: Material::Stone },
        );
        // Underlay cascades to Grass; overlay stays raw Void.
        assert_eq!(world.underlay(cell(0, 0)), Material::Grass);
        assert_eq!(world.overlay(cell(0, 0)), Material::Void);
    }

    #[test]
    fn sparse_world_reads_void_and_zero() {
        let world = World::new();
        assert_eq!(world.underlay(cell(100, -50)), Material::Void);
        assert_eq!(world.overlay(cell(100, -50)), Material::Void);
        assert_eq!(world.overlay_coverage(cell(100, -50), 0, 0), 0);
        assert_eq!(world.height(cell(100, -50)), 0);
        assert!(world.chunk(ChunkPos { x: 3, z: 3 }).is_none());
    }

    #[test]
    fn smoothing_override_resolves_plane_then_table_then_none() {
        let mut world = World::new();
        let mut chunk = Chunk::empty();
        chunk.smoothing[0] = 1; // registered below
        chunk.smoothing[1] = 9; // never registered
        world.insert_chunk(ChunkPos { x: 0, z: 0 }, chunk);
        world.insert_smoothing_profile(
            1,
            SmoothingProfile {
                iterations: 7, // past the cap — clamps to 4
                degrees: 30,   // under the floor — clamps to 45
            },
        );

        assert_eq!(
            world.smoothing_override(cell(0, 0)),
            Some(SmoothingProfile { iterations: 4, degrees: 45 }),
            "registration clamps to the apron-safe range",
        );
        assert_eq!(world.smoothing_override(cell(1, 0)), None, "an unregistered id is no override");
        assert_eq!(world.smoothing_override(cell(2, 0)), None, "plane 0 is no override");
        assert_eq!(world.smoothing_override(cell(100, 100)), None, "a missing chunk is no override");
    }

    #[test]
    fn point_height_inherits_applies_and_saturates() {
        let mut world = World::new();
        let mut chunk = Chunk::empty();
        chunk.height[3 * 16 + 3] = 100; // cell (3,3)
        world.insert_chunk(ChunkPos { x: 0, z: 0 }, chunk);

        // An inherit (zero) point reads the cell height unchanged.
        assert_eq!(world.point_height(cell(3, 3), 0, 0), 100, "inherit reads cell");
        // A stamped delta offsets the point off the cell height.
        world.set_cell_heights(cell(3, 3), &[40, -25]);
        assert_eq!(world.point_height(cell(3, 3), 0, 0), 140, "+delta lifts");
        assert_eq!(world.point_height(cell(3, 3), 1, 0), 75, "-delta drops");
        assert_eq!(world.point_height(cell(3, 3), 2, 0), 100, "the untouched tail inherits the cell height");
        // A short stamp leaves the tail inheriting; an empty stamp clears all.
        world.set_cell_heights(cell(3, 3), &[]);
        assert_eq!(world.point_height(cell(3, 3), 0, 0), 100, "an empty stamp clears the cell to inherit");

        // Extremes saturate rather than wrap: a max-magnitude delta on a
        // near-i32-max cell height clamps at the bound, not overflow-wraps.
        let mut extreme = Chunk::empty();
        extreme.height[0] = i32::MAX - 10;
        let mut ex_world = World::new();
        ex_world.insert_chunk(ChunkPos { x: 0, z: 0 }, extreme);
        ex_world.set_cell_heights(cell(0, 0), &[i16::MAX]);
        assert_eq!(ex_world.point_height(cell(0, 0), 0, 0), i32::MAX, "a lift past the range saturates at i32::MAX");
    }

    #[test]
    fn a_missing_chunk_point_height_reads_zero() {
        let world = World::new();
        assert_eq!(world.point_height(cell(50, -20), 2, 1), 0);
    }
}
