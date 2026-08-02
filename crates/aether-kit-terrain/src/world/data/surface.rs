//! Resolving elevation into the drawn — and stood-on — surface: the water
//! plane pin, the corner-plate walk over the cell and point lattices, and
//! the bilinear patches the mesher emits and a mover stands on.

use super::super::kinds::{TerrainSurface, WorldPoint};
use super::coords::CellPos;
use super::layout::{HEIGHT_POINT_INHERIT, OCTIMETERS_PER_CELL, SUBCELLS_PER_CELL, SUBCELLS_PER_CELL_EDGE};
use super::material::Material;
use super::world::World;

/// The step ceiling in octimeters: two edge-adjacent cells whose heights
/// differ by strictly more than this meet at a cliff instead of a
/// continuous slope. The mesher derives cliff faces from it, and movement
/// will read the same constant as its traversability rule, so the drawn
/// break and the walkable break can never disagree.
pub const STEP_MAX_OCTIMETERS: i32 = 64;

impl World {
    /// The effective surface level in octimeters at subcell point
    /// `(sub_x, sub_z)` of `cell` — the point-lattice analogue of
    /// [`World::surface_level`]. A water cell's points resolve at the flat
    /// water level (a delta under water is lakebed relief the flat surface
    /// ignores); a land cell's points resolve at [`World::point_height`]. The
    /// corner-plate walk reads this so an authored break inside a cell splits
    /// its plates just as a cell-scale cliff splits the cell lattice.
    pub(crate) fn point_surface_level(&self, cell: CellPos, sub_x: i32, sub_z: i32) -> i32 {
        self.water_level(cell).unwrap_or_else(|| self.point_height(cell, sub_x, sub_z))
    }

    /// The effective surface level of the subcell whose global subcell-lattice
    /// base corner is `(sx, sz)` (`sx = cell.x * SUB + sub_x`). The point
    /// corner plate reads its four incident subcells through this.
    fn subcell_surface_level(&self, sx: i32, sz: i32) -> i32 {
        let sub = SUBCELLS_PER_CELL_EDGE.cast_signed();
        let cell = CellPos { x: sx.div_euclid(sub), z: sz.div_euclid(sub) };
        self.point_surface_level(cell, sx.rem_euclid(sub), sz.rem_euclid(sub))
    }

    /// Does `cell` or any of its eight neighbors carry an authored height
    /// delta? A `false` here is the shortcut that collapses a flat or legacy
    /// neighborhood to the cell-stride corner-plate math — the corner plate
    /// at any of `cell`'s corners reads at most this 3×3 cell window, so with
    /// no relief anywhere in it the point lattice would resolve identically
    /// and the finer walk is skipped. A `true` engages the per-point patches.
    pub(crate) fn cell_has_height_relief(&self, cell: CellPos) -> bool {
        for dz in -1..=1 {
            for dx in -1..=1 {
                let n = CellPos { x: cell.x + dx, z: cell.z + dz };
                let Some(chunk) = self.chunks.get(&n.chunk()) else {
                    continue;
                };
                let base = n.chunk_index() * SUBCELLS_PER_CELL;
                if chunk.height_points[base..base + SUBCELLS_PER_CELL].iter().any(|&d| d != HEIGHT_POINT_INHERIT) {
                    return true;
                }
            }
        }
        false
    }

    /// The authored water surface level in octimeters at `cell`, or `None`
    /// if the cell is not water. `Some` exactly when the cascade-resolved
    /// underlay is [`Material::Water`]: the level is the cell's water
    /// plane's [`WaterPlane::level_octimeters`](crate::world::WaterPlane::level_octimeters), with the datum `0` for
    /// plane id `0` or an unregistered id — the level is authored, never
    /// derived from the lakebed [`World::height`].
    #[must_use]
    pub fn water_level(&self, cell: CellPos) -> Option<i32> {
        if self.underlay(cell) != Material::Water {
            return None;
        }
        let Some(chunk) = self.chunks.get(&cell.chunk()) else {
            return Some(0);
        };
        let plane_id = chunk.water_plane[cell.chunk_index()];
        if plane_id == 0 {
            return Some(0);
        }
        Some(self.water_planes.get(plane_id as usize - 1).map_or(0, |plane| plane.level_octimeters))
    }

    /// The effective surface level in octimeters at `cell`: the water
    /// level for a water cell, else the lakebed [`World::height`]. The
    /// surface-resolution machinery — [`World::edge_is_cliff`], the corner
    /// plate walk, the mesher's lift and skirt passes — reads this so a
    /// water cell resolves at its flat authored level instead of the ground
    /// beneath it.
    pub(crate) fn surface_level(&self, cell: CellPos) -> i32 {
        self.water_level(cell).unwrap_or_else(|| self.height(cell))
    }

    /// Do two edge-adjacent cells meet at a cliff — an effective-level step
    /// strictly past [`STEP_MAX_OCTIMETERS`]? The rule is pairwise over the
    /// two cells' effective surface levels (`surface_level`), so any
    /// caller holding two adjacent
    /// cells derives the same answer, and a bank standing past the step
    /// ceiling above a water surface cliffs against it.
    #[must_use]
    pub fn edge_is_cliff(&self, a: CellPos, b: CellPos) -> bool {
        (self.surface_level(a) - self.surface_level(b)).abs() > STEP_MAX_OCTIMETERS
    }

    /// The plate-resolved elevation of lattice corner `(kx, kz)` as seen
    /// from `cell` (one of the corner's four incident cells), in meters.
    /// The four incident cells partition into groups connected by
    /// non-cliff shared edges — a walk around the corner — and the plate
    /// containing `cell` averages its members' effective surface levels
    /// ([`World::surface_level`]). Connected cells share a plate (the
    /// surface blends); a cliff splits the plates (the surface breaks, and
    /// the gap is the skirt's job).
    ///
    /// A plate with any water member pins to the mean of its **water**
    /// members' levels alone, not the mixed mean — so an interior water
    /// corner is exactly flat at the authored level, and a connected shore
    /// corner (land within the step ceiling of the level) meets the water
    /// plane exactly, blending the land down to the waterline like a beach
    /// with no slit and no extra geometry. Past the step ceiling the plates
    /// split as usual and the skirt closes the face.
    fn corner_plate(&self, kx: i32, kz: i32, cell: CellPos) -> f32 {
        // Incident cells in cyclic order, so consecutive entries (mod 4)
        // share an edge and the diagonal pairs do not.
        let cells = [
            CellPos { x: kx - 1, z: kz - 1 },
            CellPos { x: kx, z: kz - 1 },
            CellPos { x: kx, z: kz },
            CellPos { x: kx - 1, z: kz },
        ];
        let levels = cells.map(|c| self.surface_level(c));
        let is_water = cells.map(|c| self.water_level(c).is_some());
        let start = cells.iter().position(|&c| c == cell);
        debug_assert!(start.is_some(), "cell must be incident to the corner");
        let start = start.unwrap_or(2);
        plate_mean_octimeters(levels, is_water, start) / OCTIMETERS_PER_CELL as f32
    }

    /// The plate-resolved elevation in meters of the point-lattice corner at
    /// global subcell-lattice coordinate `(px, pz)` (world meters
    /// `(px / SUB, pz / SUB)`), seen from the incident subcell whose position
    /// in the corner's cyclic incidence is `anchor`. The subcell analogue of
    /// [`World::corner_plate`]: the four incident subcells partition into
    /// plates by the same non-cliff walk over their [`point_surface_level`]s
    /// (with `STEP_MAX_OCTIMETERS` tested between adjacent points), and the
    /// plate containing `anchor` averages its members. Water members pin the
    /// plate to the water level exactly as at cell scale.
    fn point_corner_plate(&self, px: i32, pz: i32, anchor: usize) -> f32 {
        // Incident subcells in the same cyclic order as `corner_plate`, so
        // consecutive entries (mod 4) share a subcell edge.
        let subs = [(px - 1, pz - 1), (px, pz - 1), (px, pz), (px - 1, pz)];
        let levels = subs.map(|(sx, sz)| self.subcell_surface_level(sx, sz));
        let is_water = subs.map(|(sx, sz)| {
            let sub = SUBCELLS_PER_CELL_EDGE.cast_signed();
            self.water_level(CellPos { x: sx.div_euclid(sub), z: sz.div_euclid(sub) }).is_some()
        });
        plate_mean_octimeters(levels, is_water, anchor) / OCTIMETERS_PER_CELL as f32
    }

    /// The four point-plate corner heights (meters) of the subcell
    /// `(sub_x, sub_z)` of `cell`, ordered like [`World::cell_corner_heights`]
    /// — `[(low), (x+), (z+), (x+ z+)]`. The subcell spans `1 / SUB` m; the
    /// mesher's per-point cap patches and [`World::surface_height_in`]'s relief
    /// branch bilerp these. Each corner's anchor index selects the plate this
    /// subcell belongs to, so an authored break between adjacent points reads
    /// on the higher-coordinate side exactly as a cell cliff does.
    pub(crate) fn subcell_corner_heights(&self, cell: CellPos, sub_x: i32, sub_z: i32) -> [f32; 4] {
        let sub = SUBCELLS_PER_CELL_EDGE.cast_signed();
        let sx = cell.x * sub + sub_x;
        let sz = cell.z * sub + sub_z;
        [
            self.point_corner_plate(sx, sz, 2),
            self.point_corner_plate(sx + 1, sz, 3),
            self.point_corner_plate(sx, sz + 1, 1),
            self.point_corner_plate(sx + 1, sz + 1, 0),
        ]
    }

    /// The plate-resolved elevations of `cell`'s four corners, in meters,
    /// ordered `[(x, z), (x+1, z), (x, z+1), (x+1, z+1)]` — the bilinear
    /// patch [`World::surface_height_in`] interpolates and the mesher
    /// emits.
    #[must_use]
    pub fn cell_corner_heights(&self, cell: CellPos) -> [f32; 4] {
        if !self.cell_has_height_relief(cell) {
            return [
                self.corner_plate(cell.x, cell.z, cell),
                self.corner_plate(cell.x + 1, cell.z, cell),
                self.corner_plate(cell.x, cell.z + 1, cell),
                self.corner_plate(cell.x + 1, cell.z + 1, cell),
            ];
        }
        // Relief nearby: the cell's four outer corners resolve through the
        // point lattice, each anchored to the cell's own corner subcell so
        // the corner reads this cell's plate.
        let sub = SUBCELLS_PER_CELL_EDGE.cast_signed();
        [
            self.subcell_corner_heights(cell, 0, 0)[0],
            self.subcell_corner_heights(cell, sub - 1, 0)[1],
            self.subcell_corner_heights(cell, 0, sub - 1)[2],
            self.subcell_corner_heights(cell, sub - 1, sub - 1)[3],
        ]
    }

    /// The ground elevation in meters at `(wx, wz)` (meters, `1 cell =
    /// 1 m`) as `cell`'s bilinear surface patch reads it — coordinates
    /// clamp to the cell's span. This cell-pinned form is what the mesher
    /// emits vertices from: on a cliff edge the two sides read their own
    /// plates, so the drawn break is exactly the plate break. Two cells
    /// meeting without a cliff share their edge plates and therefore agree
    /// along the whole shared edge.
    #[must_use]
    pub fn surface_height_in(&self, cell: CellPos, wx: f32, wz: f32) -> f32 {
        if !self.cell_has_height_relief(cell) {
            let corners = self.cell_corner_heights(cell);
            let fx = (wx - cell.x as f32).clamp(0.0, 1.0);
            let fz = (wz - cell.z as f32).clamp(0.0, 1.0);
            let bottom = corners[0] + (corners[1] - corners[0]) * fx;
            let top = corners[2] + (corners[3] - corners[2]) * fx;
            return bottom + (top - bottom) * fz;
        }
        // Relief nearby: resolve through the subcell patch containing the
        // point. The coordinates clamp into the cell, then into its subcell,
        // so a caller off the cell span reads the nearest edge subcell.
        let sub = SUBCELLS_PER_CELL_EDGE.cast_signed();
        let sub_f = sub as f32;
        let local_x = ((wx - cell.x as f32) * sub_f).clamp(0.0, sub_f);
        let local_z = ((wz - cell.z as f32) * sub_f).clamp(0.0, sub_f);
        let sub_x = floor_to_i32(local_x).clamp(0, sub - 1);
        let sub_z = floor_to_i32(local_z).clamp(0, sub - 1);
        let corners = self.subcell_corner_heights(cell, sub_x, sub_z);
        let x0 = cell.x as f32 + sub_x as f32 / sub_f;
        let z0 = cell.z as f32 + sub_z as f32 / sub_f;
        let fx = ((wx - x0) * sub_f).clamp(0.0, 1.0);
        let fz = ((wz - z0) * sub_f).clamp(0.0, 1.0);
        let bottom = corners[0] + (corners[1] - corners[0]) * fx;
        let top = corners[2] + (corners[3] - corners[2]) * fx;
        bottom + (top - bottom) * fz
    }

    /// The surface elevation in meters at `(wx, wz)`, resolved through the
    /// owning cell (floor). This is the stood-on height for movers,
    /// ray-picks, and the camera — the same bilinear patch the mesher
    /// draws, so what is drawn is what is stood on. Over a water cell the
    /// patch reads the water surface (the corner plates pin to the water
    /// level), so a mover on water stands at the surface — the swimming
    /// datum, ahead of any blocking rules. A point exactly on a cliff edge
    /// reads the higher-coordinate side (the floor convention).
    #[must_use]
    pub fn surface_height(&self, wx: f32, wz: f32) -> f32 {
        let cell = CellPos { x: floor_to_i32(wx), z: floor_to_i32(wz) };
        self.surface_height_in(cell, wx, wz)
    }

    /// Sample the markable top surface at meter-space XZ coordinates.
    ///
    /// Presence follows the same authored fields the mesher consumes: a
    /// non-Void resolved underlay point or a non-Void overlay sample at the
    /// shared half-coverage threshold. Missing terrain and explicit holes
    /// return None. Height resolves through the existing stood-on surface,
    /// including relief, plate breaks, and water levels.
    #[must_use]
    pub fn terrain_surface_at(&self, x_meters: f32, z_meters: f32) -> Option<TerrainSurface> {
        if !x_meters.is_finite() || !z_meters.is_finite() {
            return None;
        }
        let cell = CellPos { x: floor_to_i32(x_meters), z: floor_to_i32(z_meters) };
        let subcells = SUBCELLS_PER_CELL_EDGE.cast_signed();
        let subcells_f32 = subcells as f32;
        let sub_x = floor_to_i32((x_meters - cell.x as f32) * subcells_f32).clamp(0, subcells - 1);
        let sub_z = floor_to_i32((z_meters - cell.z as f32) * subcells_f32).clamp(0, subcells - 1);
        let underlay_present = self.underlay_point(cell, sub_x, sub_z) != Material::Void;
        let overlay_present = [Material::Grass, Material::Dirt, Material::Stone, Material::Sand, Material::Water]
            .into_iter()
            .any(|material| {
                super::super::mesher::contour::scalar_coverage_is_inside(
                    self.continuous_overlay_coverage_at(x_meters, z_meters, material),
                )
            });
        if !underlay_present && !overlay_present {
            return None;
        }
        let x_octimeters = i32::try_from((x_meters * OCTIMETERS_PER_CELL as f32).round() as i64).ok()?;
        let z_octimeters = i32::try_from((z_meters * OCTIMETERS_PER_CELL as f32).round() as i64).ok()?;
        Some(TerrainSurface {
            cell,
            mark_point: WorldPoint::new(x_octimeters, z_octimeters),
            height_meters: self.surface_height_in(cell, x_meters, z_meters),
        })
    }
}

/// Floor to `i32` — `as i32` truncates toward zero, which is wrong for
/// negative world coordinates, so step down when it rounded up.
pub(super) fn floor_to_i32(v: f32) -> i32 {
    #[allow(clippy::cast_possible_truncation)] // world coordinates are far inside i32
    let t = v as i32;
    if (t as f32) > v {
        t - 1
    } else {
        t
    }
}

/// Partition four cyclically-ordered incident members into non-cliff plates
/// and return the mean effective level (octimeters) of the plate containing
/// `start`. Consecutive entries (mod 4) share an edge; a pair within
/// [`STEP_MAX_OCTIMETERS`] joins the same plate, one past it splits. A plate
/// with any water member averages only its water members (the flat-plane
/// pin), else the whole connected plate. Shared by the cell corner plate
/// ([`World::corner_plate`]) and the subcell point corner plate
/// ([`World::point_corner_plate`]) so the two lattices resolve identically.
fn plate_mean_octimeters(levels: [i32; 4], is_water: [bool; 4], start: usize) -> f32 {
    let mut member = [false; 4];
    member[start] = true;
    // Closure over the four cyclic edges to a fixpoint (at most three rounds
    // absorb everything reachable).
    let mut changed = true;
    while changed {
        changed = false;
        for k in 0..4 {
            let a = k;
            let b = (k + 1) % 4;
            let connected = (levels[a] - levels[b]).abs() <= STEP_MAX_OCTIMETERS;
            if connected && member[a] != member[b] {
                member[a] = true;
                member[b] = true;
                changed = true;
            }
        }
    }
    let any_water = (0..4).any(|k| member[k] && is_water[k]);
    let mut sum = 0i32;
    let mut count = 0i32;
    for k in 0..4 {
        if member[k] && (!any_water || is_water[k]) {
            sum += levels[k];
            count += 1;
        }
    }
    sum as f32 / count as f32
}

#[cfg(test)]
mod tests {
    use super::super::chunk::Chunk;
    use super::super::coords::ChunkPos;
    use super::super::fixture::cell;
    use super::super::layout::{CELLS_PER_CHUNK, SCALAR_COVERAGE_THRESHOLD};
    use super::super::table::WaterPlane;
    use super::*;
    use crate::world::mesher::contour::COVERAGE_CROSSING;

    /// A world with one chunk whose heights come from `f(x, z)` over the
    /// chunk-local cells.
    fn height_world(f: impl Fn(i32, i32) -> i32) -> World {
        let mut world = World::new();
        let mut chunk = Chunk::empty();
        for z in 0..CELLS_PER_CHUNK {
            for x in 0..CELLS_PER_CHUNK {
                chunk.height[(z * CELLS_PER_CHUNK + x) as usize] = f(x, z);
            }
        }
        world.insert_chunk(ChunkPos { x: 0, z: 0 }, chunk);
        world
    }

    #[test]
    fn step_ceiling_is_strictly_greater() {
        // Δh == STEP_MAX_OCTIMETERS is a legal step, one past it is a
        // cliff — the strictly-greater semantic movement will share.
        let world = height_world(|x, _| match x {
            0 => 0,
            1 => STEP_MAX_OCTIMETERS,
            _ => 2 * STEP_MAX_OCTIMETERS + 1,
        });
        assert!(!world.edge_is_cliff(cell(0, 0), cell(1, 0)));
        assert!(world.edge_is_cliff(cell(1, 0), cell(2, 0)));
    }

    #[test]
    fn corner_plates_split_by_the_cliff_walk() {
        // Rows z=0 at 0 and z=1 at 200 octimeters: a cliff runs along the
        // whole shared edge, so at corner (1,1) the four incident cells
        // split 2/2 and each side reads its own plate mean.
        let world = height_world(|_, z| {
            if z == 0 {
                0
            } else {
                200
            }
        });
        let low = world.cell_corner_heights(cell(0, 0));
        let high = world.cell_corner_heights(cell(0, 1));
        // Cell (0,0)'s far corners (indices 2, 3 — the z+1 pair) sit on the
        // cliff line and read the low plate; cell (0,1)'s near corners
        // (indices 0, 1) read the high plate at the same lattice points.
        assert_eq!(low[2], 0.0);
        assert_eq!(low[3], 0.0);
        assert_eq!(high[0], 200.0 / 256.0);
        assert_eq!(high[1], 200.0 / 256.0);
    }

    #[test]
    fn connected_corner_is_one_plate() {
        // Heights within the step ceiling all around a corner: every
        // incident cell reads the same mean — the blended-slope case.
        let world = height_world(|x, z| match (x, z) {
            (0, 0) => 0,
            (0, 1) => 64,
            _ => 32,
        });
        let mean = (0.0 + 32.0 + 64.0 + 32.0) / 4.0 / 256.0;
        assert_eq!(world.cell_corner_heights(cell(0, 0))[3], mean);
        assert_eq!(world.cell_corner_heights(cell(1, 0))[2], mean);
        assert_eq!(world.cell_corner_heights(cell(0, 1))[1], mean);
        assert_eq!(world.cell_corner_heights(cell(1, 1))[0], mean);
    }

    #[test]
    fn non_cliff_neighbors_agree_along_the_shared_edge() {
        // The drawn-equals-stood-on contract's continuity half: without a
        // cliff, the two cells' patches read the same height anywhere on
        // the shared edge (shared plates), so the meshes cannot crack.
        let world = height_world(|x, z| 8 * x + 4 * z);
        for step in 0..=4 {
            let wz = 3.0 + step as f32 / 4.0;
            let a = world.surface_height_in(cell(4, 3), 5.0, wz);
            let b = world.surface_height_in(cell(5, 3), 5.0, wz);
            assert!((a - b).abs() < 1e-6, "edge disagreement at wz {wz}");
        }
    }

    #[test]
    fn uniform_height_reads_everywhere() {
        let world = height_world(|_, _| 128);
        assert_eq!(world.surface_height(4.25, 7.75), 0.5);
        assert_eq!(world.surface_height_in(cell(3, 3), 3.5, 3.5), 0.5);
    }

    /// A one-chunk world whose underlay / water-plane / height planes come
    /// from `fill(lx, lz) -> (material, plane id, lakebed height)`, with the
    /// given `(id, level)` water planes registered.
    fn plane_world(planes: &[(u32, i32)], fill: impl Fn(i32, i32) -> (Material, u16, i32)) -> World {
        let mut chunk = Chunk::empty();
        for lz in 0..CELLS_PER_CHUNK {
            for lx in 0..CELLS_PER_CHUNK {
                let (material, plane, height) = fill(lx, lz);
                let i = (lz * CELLS_PER_CHUNK + lx) as usize;
                chunk.underlay[i] = material;
                chunk.water_plane[i] = plane;
                chunk.height[i] = height;
            }
        }
        let mut world = World::new();
        for &(id, level) in planes {
            world.insert_water_plane(id, WaterPlane { level_octimeters: level });
        }
        world.insert_chunk(ChunkPos { x: 0, z: 0 }, chunk);
        world
    }

    #[test]
    fn water_level_resolves_plane_then_datum_and_none_off_water() {
        // A water cell reads its plane's level; plane 0 and an unregistered
        // id both read the datum 0; a non-water cell reads None.
        let world = plane_world(&[(1, 100)], |lx, _| match lx {
            0 => (Material::Water, 1, 0),  // registered plane 1
            1 => (Material::Water, 0, 0),  // plane 0 → datum
            2 => (Material::Water, 9, 0),  // unregistered id → datum
            3 => (Material::Grass, 0, 50), // not water
            _ => (Material::Void, 0, 0),
        });
        assert_eq!(world.water_level(cell(0, 0)), Some(100));
        assert_eq!(world.water_level(cell(1, 0)), Some(0));
        assert_eq!(world.water_level(cell(2, 0)), Some(0));
        assert_eq!(world.water_level(cell(3, 0)), None);
        // The lakebed read is unchanged — height stays the raw ground.
        assert_eq!(world.height(cell(3, 0)), 50);
    }

    #[test]
    fn interior_water_surface_is_flat_at_the_plane_level() {
        // A block of water on one plane renders exactly flat at the authored
        // level regardless of the lakebed heights beneath — every corner of
        // an interior water cell pins to the level.
        let world = plane_world(&[(1, 128)], |lx, lz| {
            // A bumpy lakebed under the water so a non-pinned plate would tilt.
            (Material::Water, 1, 11 * lx - 7 * lz)
        });
        let level_m = 128.0 / 256.0;
        for corner in world.cell_corner_heights(cell(5, 5)) {
            assert!((corner - level_m).abs() < 1e-6, "water corner {corner} not flat at {level_m}");
        }
        // And it is the stood-on surface, while height stays the raw lakebed.
        assert!((world.surface_height(5.5, 5.5) - level_m).abs() < 1e-6);
        assert_eq!(world.height(cell(5, 5)), 11 * 5 - 7 * 5);
    }

    #[test]
    fn connected_shore_land_blends_to_the_water_level() {
        // Land within the step ceiling of the water level shares the corner
        // plate, and the plate pins to the water members — so the land's
        // shared corner meets the water plane exactly (the beach blend), not
        // the mixed mean.
        let world = plane_world(&[(1, 100)], |_, lz| {
            if lz == 0 {
                (Material::Water, 1, 0) // level 100, lakebed 0
            } else {
                (Material::Grass, 0, 140) // within 64 of 100 → connected
            }
        });
        let level_m = 100.0 / 256.0;
        // Corner (1, 1) is shared by the two water cells (0,0),(1,0) and the
        // two land cells (0,1),(1,1); the plate has water members, so every
        // incident cell reads the water level there.
        assert!((world.cell_corner_heights(cell(0, 0))[3] - level_m).abs() < 1e-6);
        assert!(
            (world.cell_corner_heights(cell(1, 1))[0] - level_m).abs() < 1e-6,
            "connected shore land does not blend to the waterline",
        );
    }

    #[test]
    fn past_step_bank_splits_from_the_water_plane() {
        // Land standing past the step ceiling above the water cliffs against
        // it: the corner plates split, the water side stays at its level and
        // the bank reads its own height — the gap the skirt closes.
        let world = plane_world(&[(1, 100)], |_, lz| {
            if lz == 0 {
                (Material::Water, 1, 0) // level 100
            } else {
                (Material::Grass, 0, 200) // |200 - 100| = 100 > 64 → cliff
            }
        });
        assert!(world.edge_is_cliff(cell(0, 0), cell(0, 1)));
        // At corner (1, 1) the water side reads 100 and the bank reads 200.
        assert!((world.cell_corner_heights(cell(0, 0))[3] - 100.0 / 256.0).abs() < 1e-6);
        assert!((world.cell_corner_heights(cell(1, 1))[0] - 200.0 / 256.0).abs() < 1e-6);
    }

    #[test]
    fn a_water_plane_row_rewrite_retunes_the_level() {
        // Retuning a lake is one table write: the same water cell resolves at
        // the new level after re-registering its plane id.
        let mut world = plane_world(&[(1, 100)], |_, _| (Material::Water, 1, 0));
        assert_eq!(world.water_level(cell(3, 3)), Some(100));
        world.insert_water_plane(1, WaterPlane { level_octimeters: 240 });
        assert_eq!(world.water_level(cell(3, 3)), Some(240));
    }

    #[test]
    fn zeroed_deltas_restore_the_cell_stride_surface() {
        // Tripwire: with every height delta zero the surface resolves at cell
        // stride, byte-identical to a world that never carried the plane — the
        // per-cell shortcut must collapse an all-zero neighborhood to the cell
        // math, so authoring then clearing a cell's deltas moves nothing.
        let base = height_world(|x, z| 8 * x - 5 * z); // gentle, no cliffs
        let mut authored = base.clone();
        authored.set_cell_heights(cell(3, 3), &[40; SUBCELLS_PER_CELL]);
        authored.set_cell_heights(cell(3, 3), &[]); // clears back to zero relief
        assert!(
            !authored.cell_has_height_relief(cell(3, 3)),
            "a cleared cell reports no relief, so the shortcut engages",
        );
        for &(wx, wz) in &[(3.5, 3.5), (3.1, 3.9), (4.0, 3.0), (2.5, 4.5), (3.75, 3.25)] {
            assert_eq!(
                base.surface_height(wx, wz),
                authored.surface_height(wx, wz),
                "a net-zero delta plane must not move the surface at ({wx}, {wz})",
            );
        }
    }

    #[test]
    fn a_delta_ramp_reads_continuously() {
        // A per-point height ramp within the step ceiling stays continuous:
        // adjacent points share their plate, so surface_height varies smoothly
        // across subcell boundaries with no break.
        let sub = SUBCELLS_PER_CELL_EDGE.cast_signed();
        let mut world = height_world(|_, _| 0);
        // Cell (5,5) ramps 16 octimeters per subcell in x (0,16,32,48 ≤ step).
        let mut deltas = [0i16; SUBCELLS_PER_CELL];
        for sz in 0..sub {
            for sx in 0..sub {
                deltas[(sz * sub + sx) as usize] = (16 * sx) as i16;
            }
        }
        world.set_cell_heights(cell(5, 5), &deltas);
        // March densely across the ramp; no successive step exceeds the
        // per-subcell slope (a break would show a jump far past it).
        let mut prev = world.surface_height(5.02, 5.5);
        for i in 1..48 {
            let wx = 5.02 + i as f32 * 0.02;
            let h = world.surface_height(wx, 5.5);
            assert!((h - prev).abs() < 0.05, "a continuous ramp jumped {} at x {wx}", (h - prev).abs());
            prev = h;
        }
    }

    #[test]
    fn a_delta_plateau_splits_plates_on_its_perimeter() {
        // A 2×2 block of points raised past the step ceiling is a plateau: its
        // interior points stand at the raised level, a point just outside the
        // block stays at the base, and the surface between them breaks (the
        // plate splits on the block's perimeter) rather than blending.
        let sub = SUBCELLS_PER_CELL_EDGE.cast_signed();
        let mut world = height_world(|_, _| 0);
        let mut deltas = [0i16; SUBCELLS_PER_CELL];
        for sz in 1..3 {
            for sx in 1..3 {
                deltas[(sz * sub + sx) as usize] = 200; // > STEP_MAX_OCTIMETERS
            }
        }
        world.set_cell_heights(cell(5, 5), &deltas);
        // Center of the raised block (subcell (1,1)..(2,2)) stands at 200/256.
        let sub_f = sub as f32;
        let inside = world.surface_height(5.0 + 1.5 / sub_f, 5.0 + 1.5 / sub_f);
        assert!((inside - 200.0 / 256.0).abs() < 1e-4, "the plateau interior stands at the raised level, got {inside}");
        // A flat corner subcell stays at the base — the plate did not blend
        // the raise outward across the break.
        let outside = world.surface_height(5.0 + 0.5 / sub_f, 5.0 + 0.5 / sub_f);
        assert!(outside.abs() < 1e-4, "a flat subcell outside the plateau stays at the base, got {outside}");
    }

    #[test]
    fn terrain_surface_sampler_shares_presence_and_height_truth() {
        let mut world = World::new();
        let mut chunk = Chunk::empty();
        chunk.underlay[0] = Material::Stone;
        chunk.height[0] = 256;
        world.insert_chunk(ChunkPos { x: 0, z: 0 }, chunk);

        let surface = world.terrain_surface_at(0.5, 0.5).expect("resolved underlay is markable");
        assert_eq!(surface.cell, cell(0, 0));
        assert_eq!(surface.mark_point, WorldPoint::new(128, 128));
        assert!((surface.height_meters - 1.0).abs() < 1e-4);

        let subcell = (8 * SUBCELLS_PER_CELL_EDGE + 8) as usize;
        world.chunk_mut_or_insert(ChunkPos { x: 0, z: 0 }).underlay_points[subcell] = Material::Void.to_u8();
        assert!(world.terrain_surface_at(0.5, 0.5).is_none(), "an explicit underlay-point hole is not markable");

        let chunk = world.chunk_mut_or_insert(ChunkPos { x: 0, z: 0 });
        chunk.overlay[0] = Material::Sand;
        chunk.overlay_mask[..SUBCELLS_PER_CELL].fill(SCALAR_COVERAGE_THRESHOLD - 1);
        assert!(world.terrain_surface_at(0.5, 0.5).is_none(), "coverage below the contour threshold stays absent");
        world.chunk_mut_or_insert(ChunkPos { x: 0, z: 0 }).overlay_mask[..SUBCELLS_PER_CELL]
            .fill(SCALAR_COVERAGE_THRESHOLD);
        assert!(
            world.terrain_surface_at(0.5, 0.5).is_some(),
            "the exact contour threshold makes an overlay-only sample markable"
        );
    }

    #[test]
    fn terrain_surface_sampler_matches_a_continuous_scalar_overlay_crossing() {
        let mut world = World::new();
        let mut chunk = Chunk::empty();
        chunk.overlay[0] = Material::Stone;
        let subcells = SUBCELLS_PER_CELL_EDGE as usize;
        for subcell_z in 0..subcells {
            for subcell_x in 0..subcells {
                chunk.overlay_mask[subcell_z * subcells + subcell_x] = if subcell_x < subcells / 2 {
                    100
                } else {
                    200
                };
            }
        }
        world.insert_chunk(ChunkPos { x: 0, z: 0 }, chunk);

        let low_sample_x = (subcells / 2 - 2) as f32 / subcells as f32 + 0.5 / subcells as f32;
        let high_sample_x = (subcells / 2 - 1) as f32 / subcells as f32 + 0.5 / subcells as f32;
        let high_reconstructed = 150.0;
        let crossing_fraction = (COVERAGE_CROSSING - 100.0) / (high_reconstructed - 100.0);
        let crossing_x = low_sample_x + (high_sample_x - low_sample_x) * crossing_fraction;

        assert!(
            world.terrain_surface_at(crossing_x - 0.001, 0.5).is_none(),
            "the point immediately before the rendered 100→200 crossing stays absent"
        );
        assert!(
            world.terrain_surface_at(crossing_x + 0.001, 0.5).is_some(),
            "the point immediately after the rendered 100→200 crossing is markable"
        );
    }

    #[test]
    fn terrain_surface_sampler_follows_relief_and_rejects_nonfinite_coordinates() {
        let mut world = World::new();
        let mut chunk = Chunk::empty();
        chunk.underlay.fill(Material::Grass);
        world.insert_chunk(ChunkPos { x: 0, z: 0 }, chunk);
        let mut deltas = [0; SUBCELLS_PER_CELL];
        deltas[8 * SUBCELLS_PER_CELL_EDGE as usize + 8] = 128;
        world.set_cell_heights(cell(0, 0), &deltas);

        let surface = world.terrain_surface_at(0.53125, 0.53125).expect("relief remains markable");
        assert!(surface.height_meters > 0.0, "the sampler uses the same relief-aware surface-height path");
        assert!(world.terrain_surface_at(f32::NAN, 0.5).is_none());
        assert!(world.terrain_surface_at(0.5, f32::INFINITY).is_none());
    }

    #[test]
    fn terrain_surface_sampler_resolves_water_and_negative_coordinates_directly() {
        let mut water = World::new();
        water.insert_water_plane(1, WaterPlane { level_octimeters: 512 });
        let mut water_chunk = Chunk::empty();
        water_chunk.underlay[0] = Material::Water;
        water_chunk.height[0] = -256;
        water_chunk.water_plane[0] = 1;
        water.insert_chunk(ChunkPos { x: 0, z: 0 }, water_chunk);
        let water_surface = water.terrain_surface_at(0.5, 0.5).expect("water is a markable top surface");
        assert_eq!(water_surface.cell, cell(0, 0));
        assert_eq!(water_surface.mark_point, WorldPoint::new(128, 128));
        assert!((water_surface.height_meters - 2.0).abs() < 1e-4);

        let negative_cell = cell(-1, -1);
        let mut negative_chunk = Chunk::empty();
        negative_chunk.underlay[negative_cell.chunk_index()] = Material::Grass;
        negative_chunk.height[negative_cell.chunk_index()] = 128;
        let mut negative = World::new();
        negative.insert_chunk(negative_cell.chunk(), negative_chunk);
        let negative_surface =
            negative.terrain_surface_at(-0.5, -0.5).expect("negative lattice coordinates remain markable");
        assert_eq!(negative_surface.cell, negative_cell);
        assert_eq!(negative_surface.mark_point, WorldPoint::new(-128, -128));
        assert!((negative_surface.height_meters - 0.5).abs() < 1e-4);
    }
}
