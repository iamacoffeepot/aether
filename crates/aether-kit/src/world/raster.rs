// Raster bounds and vertex-ring trigonometry cross deliberately between the
// integer octimeter lattice and floating-point area math. Coordinates remain
// i32 and each output index is bounded by the polygon's i32 bounding box.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]

//! CPU shape rasterization for world overlay stamps.
//!
//! Every public stamp shape becomes a polygon vertex ring in world
//! octimeters, then passes through [`rasterize_polygon`]. The rasterizer walks
//! eight horizontal sub-scanlines per semantic subcell, accumulating the
//! covered horizontal interval length into a scalar `0..=255` area estimate.
//! That is the sole coverage-producing path for polygons, discs, and regular
//! hexagons.

use alloc::collections::BTreeSet;
use alloc::vec;
use alloc::vec::Vec;
use core::{f64::consts::TAU, mem};

use super::{
    CellPos, ChunkPos, MAX_STAMP_EDGE_SUBCELLS, MAX_STAMP_RASTER_WORK, MAX_STAMP_SUBCELLS, MAX_STAMP_VERTICES,
    Material, SUBCELLS_PER_CELL, SUBCELLS_PER_CELL_EDGE, WorldPoint,
};

/// One cell is 256 octimeters and contains 16 subcells along each edge.
const OCTIMETERS_PER_SUBCELL: i32 = 256 / SUBCELLS_PER_CELL_EDGE.cast_signed();
/// Vertical area samples per semantic subcell. Horizontal interval overlap is
/// integrated exactly at each scanline, so this is the only sampling axis.
const AREA_SCANLINES_PER_SUBCELL: usize = 8;
/// A disc is a fixed regular polygon on the wire-independent raster side.
const DISC_VERTEX_COUNT: usize = 32;

#[derive(Debug)]
struct RasterizedCoverage {
    min_subcell_x: i32,
    min_subcell_z: i32,
    width: usize,
    coverage: Vec<u8>,
}

impl RasterizedCoverage {
    fn empty() -> Self {
        Self { min_subcell_x: 0, min_subcell_z: 0, width: 0, coverage: Vec::new() }
    }

    #[cfg(test)]
    fn coverage_at(&self, subcell_x: i32, subcell_z: i32) -> u8 {
        let local_x = subcell_x - self.min_subcell_x;
        let local_z = subcell_z - self.min_subcell_z;
        if local_x < 0 || local_z < 0 {
            return 0;
        }
        let (x, z) = (local_x as usize, local_z as usize);
        if x >= self.width || z >= self.coverage.len() / self.width {
            return 0;
        }
        self.coverage[z * self.width + x]
    }

    fn covered_samples(&self) -> impl Iterator<Item = (i32, i32, u8)> + '_ {
        self.coverage.iter().copied().enumerate().filter(|(_, coverage)| *coverage > 0).map(|(index, coverage)| {
            let x = index % self.width;
            let z = index / self.width;
            (self.min_subcell_x + x as i32, self.min_subcell_z + z as i32, coverage)
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SampleAddress {
    cell: CellPos,
    subcell_x: i32,
    subcell_z: i32,
}

/// Map a global subcell-lattice address into the world's cell-local plane
/// layout. Euclidean division keeps the mapping correct left/below origin.
fn sample_address(global_subcell_x: i32, global_subcell_z: i32) -> SampleAddress {
    let subcells = SUBCELLS_PER_CELL_EDGE.cast_signed();
    SampleAddress {
        cell: CellPos { x: global_subcell_x.div_euclid(subcells), z: global_subcell_z.div_euclid(subcells) },
        subcell_x: global_subcell_x.rem_euclid(subcells),
        subcell_z: global_subcell_z.rem_euclid(subcells),
    }
}

/// Rasterize a polygon vertex ring in world octimeters to scalar coverage.
/// The even-odd fill rule handles concave rings; fewer than three vertices or
/// a zero-width/height bounding box produces no samples.
fn rasterize_polygon(points: &[WorldPoint]) -> RasterizedCoverage {
    let Some(first) = points.first().copied() else {
        return RasterizedCoverage::empty();
    };
    if points.len() < 3 || points.len() > MAX_STAMP_VERTICES {
        return RasterizedCoverage::empty();
    }

    let mut min_x = first.x_octimeters;
    let mut min_z = first.z_octimeters;
    let mut max_x = first.x_octimeters;
    let mut max_z = first.z_octimeters;
    for point in &points[1..] {
        min_x = min_x.min(point.x_octimeters);
        min_z = min_z.min(point.z_octimeters);
        max_x = max_x.max(point.x_octimeters);
        max_z = max_z.max(point.z_octimeters);
    }
    if min_x == max_x || min_z == max_z {
        return RasterizedCoverage::empty();
    }

    let min_subcell_x = min_x.div_euclid(OCTIMETERS_PER_SUBCELL);
    let min_subcell_z = min_z.div_euclid(OCTIMETERS_PER_SUBCELL);
    let max_subcell_x = div_ceil(max_x, OCTIMETERS_PER_SUBCELL);
    let max_subcell_z = div_ceil(max_z, OCTIMETERS_PER_SUBCELL);
    let Ok(width) = usize::try_from(i64::from(max_subcell_x) - i64::from(min_subcell_x)) else {
        return RasterizedCoverage::empty();
    };
    let Ok(height) = usize::try_from(i64::from(max_subcell_z) - i64::from(min_subcell_z)) else {
        return RasterizedCoverage::empty();
    };
    let Some(len) = width.checked_mul(height) else {
        return RasterizedCoverage::empty();
    };
    if width > MAX_STAMP_EDGE_SUBCELLS || height > MAX_STAMP_EDGE_SUBCELLS || len > MAX_STAMP_SUBCELLS {
        return RasterizedCoverage::empty();
    }
    if estimated_raster_work(width, height, points.len()).is_none_or(|work| work > MAX_STAMP_RASTER_WORK) {
        return RasterizedCoverage::empty();
    }

    let mut coverage = vec![0; len];
    let mut intersections = Vec::with_capacity(points.len());
    for local_z in 0..height {
        let global_subcell_z = min_subcell_z + local_z as i32;
        rasterize_row(
            points,
            global_subcell_z,
            min_subcell_x,
            width,
            &mut intersections,
            &mut coverage[local_z * width..(local_z + 1) * width],
        );
    }

    RasterizedCoverage { min_subcell_x, min_subcell_z, width, coverage }
}

/// Conservative work estimate for the current scanline implementation.
/// Every sub-scanline tests each polygon edge and sorts its intersections;
/// every possible intersection pair may then walk the full raster row.
fn estimated_raster_work(width: usize, height: usize, vertex_count: usize) -> Option<usize> {
    let scanlines = height.checked_mul(AREA_SCANLINES_PER_SUBCELL)?;
    let edge_tests = scanlines.checked_mul(vertex_count)?;
    let sort_levels = usize::try_from(vertex_count.next_power_of_two().ilog2()).ok()?;
    let sort_work = edge_tests.checked_mul(sort_levels.max(1))?;
    let interval_visits = width.checked_mul(scanlines)?.checked_mul(vertex_count / 2)?;
    edge_tests.checked_add(sort_work)?.checked_add(interval_visits)
}

fn div_ceil(value: i32, divisor: i32) -> i32 {
    value.div_euclid(divisor) + i32::from(value.rem_euclid(divisor) != 0)
}

fn rasterize_row(
    points: &[WorldPoint],
    global_subcell_z: i32,
    min_subcell_x: i32,
    width: usize,
    intersections: &mut Vec<f64>,
    output: &mut [u8],
) {
    let mut covered_scanlines = vec![0.0; width];
    let row_start = f64::from(global_subcell_z) * f64::from(OCTIMETERS_PER_SUBCELL);
    for sample in 0..AREA_SCANLINES_PER_SUBCELL {
        let fraction = (sample as f64 + 0.5) / AREA_SCANLINES_PER_SUBCELL as f64;
        let z = fraction.mul_add(f64::from(OCTIMETERS_PER_SUBCELL), row_start);
        polygon_scanline_intersections(points, z, intersections);
        for pair in intersections.chunks_exact(2) {
            add_interval_coverage(&mut covered_scanlines, min_subcell_x, pair[0], pair[1]);
        }
    }

    for (slot, covered) in output.iter_mut().zip(covered_scanlines) {
        let fraction = covered / AREA_SCANLINES_PER_SUBCELL as f64;
        *slot = (fraction * 255.0).round().clamp(0.0, 255.0) as u8;
    }
}

fn polygon_scanline_intersections(points: &[WorldPoint], z: f64, intersections: &mut Vec<f64>) {
    intersections.clear();
    let mut previous = points[points.len() - 1];
    for &current in points {
        let (az, bz) = (f64::from(previous.z_octimeters), f64::from(current.z_octimeters));
        if (az <= z && z < bz) || (bz <= z && z < az) {
            let t = (z - az) / (bz - az);
            intersections.push(
                (f64::from(current.x_octimeters) - f64::from(previous.x_octimeters))
                    .mul_add(t, f64::from(previous.x_octimeters)),
            );
        }
        previous = current;
    }
    intersections.sort_by(f64::total_cmp);
}

fn add_interval_coverage(
    covered_scanlines: &mut [f64],
    min_subcell_x: i32,
    mut interval_start: f64,
    mut interval_end: f64,
) {
    if interval_end < interval_start {
        mem::swap(&mut interval_start, &mut interval_end);
    }
    let raster_start = f64::from(min_subcell_x) * f64::from(OCTIMETERS_PER_SUBCELL);
    for (local_x, covered) in covered_scanlines.iter_mut().enumerate() {
        let subcell_start = (local_x as f64).mul_add(f64::from(OCTIMETERS_PER_SUBCELL), raster_start);
        let subcell_end = subcell_start + f64::from(OCTIMETERS_PER_SUBCELL);
        let overlap = interval_end.min(subcell_end) - interval_start.max(subcell_start);
        if overlap > 0.0 {
            *covered += overlap / f64::from(OCTIMETERS_PER_SUBCELL);
        }
    }
}

/// Generate the fixed-resolution polygon ring used by a disc stamp.
pub(super) fn disc_vertices(center: WorldPoint, radius_octimeters: u32) -> Vec<WorldPoint> {
    regular_polygon_vertices(center, radius_octimeters, DISC_VERTEX_COUNT)
}

/// Generate a flat-top regular hexagon vertex ring. The radius is measured
/// from the center to each vertex.
pub(super) fn regular_hexagon_vertices(center: WorldPoint, radius_octimeters: u32) -> Vec<WorldPoint> {
    regular_polygon_vertices(center, radius_octimeters, 6)
}

fn regular_polygon_vertices(center: WorldPoint, radius_octimeters: u32, vertex_count: usize) -> Vec<WorldPoint> {
    if radius_octimeters == 0 || vertex_count < 3 {
        return Vec::new();
    }
    let radius = f64::from(radius_octimeters);
    (0..vertex_count)
        .map(|index| {
            let angle = index as f64 * TAU / vertex_count as f64;
            WorldPoint::new(
                angle.cos().mul_add(radius, f64::from(center.x_octimeters)).round() as i32,
                angle.sin().mul_add(radius, f64::from(center.z_octimeters)).round() as i32,
            )
        })
        .collect()
}

/// Rasterize and compose a polygon stamp into the scalar overlay coverage
/// plane, returning every chunk whose stored planes changed. Same-material
/// stamps union by maximum coverage. A different material takes painter's
/// order at the cell ownership boundary and clears that cell's prior mask
/// before receiving the new shape. `Void` (including an unknown material byte
/// decoded to `Void`) paints nothing.
pub(super) fn stamp_polygon<T: super::proposal::MutationTarget + ?Sized>(
    world: &mut T,
    points: &[WorldPoint],
    material: Material,
) -> BTreeSet<ChunkPos> {
    stamp_polygon_bounded(world, points, material, u32::MAX).touched
}

/// Accounting returned by [`stamp_polygon_bounded`].
#[derive(Debug, PartialEq, Eq)]
pub(super) struct BoundedStamp {
    pub(super) touched: BTreeSet<ChunkPos>,
    pub(super) subcells_written: u32,
    pub(super) exhausted: bool,
}

/// Rasterize and compose at most `max_subcells` covered samples.
///
/// Each storage-changing sample is charged before its painter-order write;
/// unchanged max-composition costs zero. Taking a cell from another material
/// atomically charges every nonzero sample cleared plus the new sample. When
/// the cap is reached, the returned world contains exactly the accepted
/// prefix, `exhausted` is true, and no later sample has been touched. The
/// ordinary stamp path is the unbounded wrapper above.
pub(super) fn stamp_polygon_bounded<T: super::proposal::MutationTarget + ?Sized>(
    world: &mut T,
    points: &[WorldPoint],
    material: Material,
    max_subcells: u32,
) -> BoundedStamp {
    let mut result = BoundedStamp { touched: BTreeSet::new(), subcells_written: 0, exhausted: false };
    if material == Material::Void {
        return result;
    }
    let raster = rasterize_polygon(points);
    for (global_subcell_x, global_subcell_z, coverage) in raster.covered_samples() {
        let address = sample_address(global_subcell_x, global_subcell_z);
        let chunk_pos = address.cell.chunk();
        let cell_index = address.cell.chunk_index();
        let base = cell_index * SUBCELLS_PER_CELL;
        let within = (address.subcell_z * SUBCELLS_PER_CELL_EDGE.cast_signed() + address.subcell_x) as usize;

        let (material_changes, prior_coverage, cleared_subcells) =
            world.chunk(chunk_pos).map_or((true, 0, 0), |chunk| {
                let material_changes = chunk.overlay[cell_index] != material;
                let cleared_subcells = if material_changes {
                    chunk.overlay_mask[base..base + SUBCELLS_PER_CELL].iter().filter(|&&sample| sample != 0).count()
                        as u32
                } else {
                    0
                };
                (material_changes, chunk.overlay_mask[base + within], cleared_subcells)
            });
        let composed = if material_changes {
            coverage
        } else {
            prior_coverage.max(coverage)
        };
        let coverage_writes = u32::from(material_changes || prior_coverage != composed);
        let write_cost = cleared_subcells + coverage_writes;
        if write_cost > max_subcells - result.subcells_written {
            result.exhausted = true;
            return result;
        }
        result.subcells_written += write_cost;
        if write_cost == 0 {
            continue;
        }

        let chunk = world.chunk_mut_or_insert(chunk_pos);
        if material_changes {
            chunk.overlay[cell_index] = material;
            chunk.overlay_mask[base..base + SUBCELLS_PER_CELL].fill(0);
        }
        let slot = &mut chunk.overlay_mask[base + within];
        *slot = composed;
        result.touched.insert(chunk_pos);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::World;

    fn point(x_octimeters: i32, z_octimeters: i32) -> WorldPoint {
        WorldPoint::new(x_octimeters, z_octimeters)
    }

    #[test]
    fn polygon_area_coverage_pins_partial_interior_and_exterior_samples() {
        // Right triangle x + z <= 64. Subcell (0,0) is fully inside,
        // (2,1) is bisected by the diagonal, and (3,3) is outside while
        // still lying in the polygon's bounding raster.
        let raster = rasterize_polygon(&[point(0, 0), point(64, 0), point(0, 64)]);
        assert_eq!(raster.coverage_at(0, 0), 255, "full interior");
        assert_eq!(raster.coverage_at(2, 1), 128, "half-covered edge");
        assert_eq!(raster.coverage_at(3, 3), 0, "exterior");
    }

    #[test]
    fn global_subcell_mapping_handles_cells_negatives_and_chunk_borders() {
        assert_eq!(
            sample_address(15, -1),
            SampleAddress { cell: CellPos { x: 0, z: -1 }, subcell_x: 15, subcell_z: 15 },
        );
        assert_eq!(sample_address(255, 0).cell.chunk(), ChunkPos { x: 0, z: 0 });
        let across = sample_address(256, 0);
        assert_eq!(across.cell, CellPos { x: 16, z: 0 });
        assert_eq!(across.cell.chunk(), ChunkPos { x: 1, z: 0 });
        assert_eq!((across.subcell_x, across.subcell_z), (0, 0));
    }

    #[test]
    fn world_octimeters_map_to_the_expected_cell_local_coverage_slot() {
        let mut world = World::new();
        let touched = stamp_polygon(
            &mut world,
            &[point(272, -16), point(288, -16), point(288, 0), point(272, 0)],
            Material::Sand,
        );
        let cell = CellPos { x: 1, z: -1 };
        assert_eq!(touched, BTreeSet::from([ChunkPos { x: 0, z: -1 }]));
        assert_eq!(world.overlay(cell), Material::Sand);
        assert_eq!(
            world.overlay_coverage(cell, 1, 15),
            255,
            "[272,288) × [-16,0) octimeters is cell (1,-1), subcell (1,15)",
        );
        assert_eq!(world.overlay_coverage(cell, 0, 15), 0, "adjacent slot");
    }

    #[test]
    fn polygon_stamp_writes_both_sides_of_a_chunk_border() {
        let mut world = World::new();
        let touched = stamp_polygon(
            &mut world,
            &[point(4080, 256), point(4112, 256), point(4112, 512), point(4080, 512)],
            Material::Stone,
        );
        assert_eq!(touched, BTreeSet::from([ChunkPos { x: 0, z: 0 }, ChunkPos { x: 1, z: 0 }]),);
        assert_eq!(world.overlay(CellPos { x: 15, z: 1 }), Material::Stone);
        assert_eq!(world.overlay_coverage(CellPos { x: 15, z: 1 }, 15, 0), 255);
        assert_eq!(world.overlay(CellPos { x: 16, z: 1 }), Material::Stone);
        assert_eq!(world.overlay_coverage(CellPos { x: 16, z: 1 }, 0, 0), 255);
    }

    #[test]
    fn bounded_stamp_reports_exact_cross_chunk_writes() {
        let mut world = World::new();
        let result = stamp_polygon_bounded(
            &mut world,
            &[point(4080, 256), point(4112, 256), point(4112, 272), point(4080, 272)],
            Material::Stone,
            2,
        );

        assert_eq!(result.subcells_written, 2);
        assert!(!result.exhausted);
        assert_eq!(result.touched, BTreeSet::from([ChunkPos { x: 0, z: 0 }, ChunkPos { x: 1, z: 0 }]),);
    }

    #[test]
    fn bounded_stamp_stops_before_the_over_cap_write() {
        let mut world = World::new();
        let result = stamp_polygon_bounded(
            &mut world,
            &[point(0, 0), point(32, 0), point(32, 16), point(0, 16)],
            Material::Sand,
            1,
        );

        assert_eq!(result.subcells_written, 1);
        assert!(result.exhausted);
        let cell = CellPos { x: 0, z: 0 };
        assert_eq!(world.overlay_coverage(cell, 0, 0), 255);
        assert_eq!(world.overlay_coverage(cell, 1, 0), 0, "the second covered sample is the rejected over-cap write");
    }

    #[test]
    fn bounded_material_takeover_is_atomic_and_charges_exact_changes() {
        let mut world = World::new();
        let two_samples = [point(0, 0), point(32, 0), point(32, 16), point(0, 16)];
        stamp_polygon(&mut world, &two_samples, Material::Stone);
        let first_sample = [point(0, 0), point(16, 0), point(16, 16), point(0, 16)];

        let rejected = stamp_polygon_bounded(&mut world, &first_sample, Material::Sand, 2);
        assert!(rejected.exhausted);
        assert_eq!(rejected.subcells_written, 0);
        assert!(rejected.touched.is_empty());
        let cell = CellPos { x: 0, z: 0 };
        assert_eq!(world.overlay(cell), Material::Stone);
        assert_eq!(world.overlay_coverage(cell, 0, 0), 255);
        assert_eq!(world.overlay_coverage(cell, 1, 0), 255);

        let applied = stamp_polygon_bounded(&mut world, &first_sample, Material::Sand, 3);
        assert!(!applied.exhausted);
        assert_eq!(applied.subcells_written, 3);
        assert_eq!(applied.touched, BTreeSet::from([ChunkPos { x: 0, z: 0 }]));
        assert_eq!(world.overlay(cell), Material::Sand);
        assert_eq!(world.overlay_coverage(cell, 0, 0), 255);
        assert_eq!(world.overlay_coverage(cell, 1, 0), 0);
    }

    #[test]
    fn repeated_stamp_composition_is_max_for_same_material_and_cell_replace_for_different() {
        let mut world = World::new();
        let full_first_subcell = [point(0, 0), point(16, 0), point(16, 16), point(0, 16)];
        stamp_polygon(&mut world, &full_first_subcell, Material::Stone);
        let half_first_subcell = [point(0, 0), point(16, 0), point(0, 16)];
        let unchanged = stamp_polygon(&mut world, &half_first_subcell, Material::Stone);
        assert!(unchanged.is_empty(), "lower same-material coverage cannot reduce the max");
        assert_eq!(world.overlay_coverage(CellPos { x: 0, z: 0 }, 0, 0), 255);

        let third_subcell = [point(32, 0), point(48, 0), point(48, 16), point(32, 16)];
        stamp_polygon(&mut world, &third_subcell, Material::Sand);
        assert_eq!(world.overlay(CellPos { x: 0, z: 0 }), Material::Sand);
        assert_eq!(
            world.overlay_coverage(CellPos { x: 0, z: 0 }, 0, 0),
            0,
            "a different material takes cell ownership and clears the old mask",
        );
        assert_eq!(world.overlay_coverage(CellPos { x: 0, z: 0 }, 2, 0), 255);
    }

    #[test]
    fn regular_hexagon_has_scalar_edge_coverage() {
        let vertices = regular_hexagon_vertices(point(512, 512), 300);
        let raster = rasterize_polygon(&vertices);
        assert!(
            raster.coverage.iter().any(|coverage| (1..=254).contains(coverage)),
            "a non-lattice hexagon should author partial edge coverage",
        );
        assert_eq!(vertices.len(), 6);
    }

    #[test]
    fn disc_generator_builds_its_fixed_vertex_ring() {
        let vertices = disc_vertices(point(10, -20), 64);
        assert_eq!(vertices.len(), DISC_VERTEX_COUNT);
        assert_eq!(vertices[0], point(74, -20), "first vertex is radius-east");
        assert!(disc_vertices(point(0, 0), 0).is_empty(), "zero-radius disc");
    }

    #[test]
    fn oversized_or_overcomplex_polygon_is_rejected_before_allocation() {
        let extreme = [
            point(i32::MIN, i32::MIN),
            point(i32::MAX, i32::MIN),
            point(i32::MAX, i32::MAX),
            point(i32::MIN, i32::MAX),
        ];
        assert!(rasterize_polygon(&extreme).coverage.is_empty());

        let too_many = vec![point(0, 0); MAX_STAMP_VERTICES + 1];
        assert!(rasterize_polygon(&too_many).coverage.is_empty());

        let edge_too_long = [
            point(0, 0),
            point((MAX_STAMP_EDGE_SUBCELLS as i32 + 1) * OCTIMETERS_PER_SUBCELL, 0),
            point((MAX_STAMP_EDGE_SUBCELLS as i32 + 1) * OCTIMETERS_PER_SUBCELL, OCTIMETERS_PER_SUBCELL),
            point(0, OCTIMETERS_PER_SUBCELL),
        ];
        assert!(rasterize_polygon(&edge_too_long).coverage.is_empty());

        let area_too_large = [
            point(0, 0),
            point(2048 * OCTIMETERS_PER_SUBCELL, 0),
            point(2048 * OCTIMETERS_PER_SUBCELL, 1024 * OCTIMETERS_PER_SUBCELL),
            point(0, 1024 * OCTIMETERS_PER_SUBCELL),
        ];
        assert!(rasterize_polygon(&area_too_large).coverage.is_empty());

        let mut sorting_too_expensive = Vec::with_capacity(MAX_STAMP_VERTICES);
        for index in 0..MAX_STAMP_VERTICES {
            sorting_too_expensive.push(point(
                if index % 2 == 0 {
                    0
                } else {
                    OCTIMETERS_PER_SUBCELL
                },
                if index % 4 < 2 {
                    0
                } else {
                    MAX_STAMP_EDGE_SUBCELLS as i32 * OCTIMETERS_PER_SUBCELL
                },
            ));
        }
        assert!(rasterize_polygon(&sorting_too_expensive).coverage.is_empty());
    }
}
