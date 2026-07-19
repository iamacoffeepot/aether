use alloc::vec;
use alloc::vec::Vec;

use aether_render::{DrawTriangle, Vertex};

use crate::world::{CellPos, ChunkPos, Material, STEP_MAX_OCTIMETERS, World};

use super::constants::{
    EDGE, MAX_APRON_SUBCELLS, OCTIMETERS_PER_METER, OCTIMETERS_PER_SUBCELL, SUBCELLS_PER_CHUNK_EDGE,
};
use super::geometry::push_wall_quad;
use super::style::{StyleTable, flat_color};
use super::surface::{SurfaceAnchor, side_anchor_lift};
use super::voids::{VoidAnchor, enclosed_void_floor, void_low_base};

/// Fixed topology budget for the material x height arrangement in one
/// contour window. Four material labels can each intersect four pinned
/// height sectors, so the finite local case table cannot exceed sixteen.
pub(super) const MAX_CAP_FRAGMENTS_PER_WINDOW: usize = 16;

/// Fixed vertex budget for one convex material x height fragment. The
/// intersection of the local case polygons has a much smaller practical
/// maximum; sixteen leaves explicit headroom while keeping an accidental
/// topology expansion an internal error rather than an input-driven path.
pub(super) const MAX_CAP_FRAGMENT_VERTICES: usize = 16;

const OWNED_WINDOWS: usize = (SUBCELLS_PER_CHUNK_EDGE * SUBCELLS_PER_CHUNK_EDGE) as usize;
const SAMPLE_GRID_EDGE: usize = (SUBCELLS_PER_CHUNK_EDGE + 2 * MAX_APRON_SUBCELLS) as usize;

/// The exact number of canonical east/north adjacencies in the fixed
/// `320 x 320` sample apron.
pub(super) const CLIFF_ADJACENCY_BUDGET: usize = 2 * SAMPLE_GRID_EDGE * (SAMPLE_GRID_EDGE - 1);

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct PlanarPoint {
    pub(super) x_oct: f32,
    pub(super) z_oct: f32,
}

#[derive(Clone, Copy)]
pub(super) struct WindowCenter {
    pub(super) x_octimeters: i32,
    pub(super) z_octimeters: i32,
}

pub(super) struct MaterialCap<'a> {
    pub(super) polygon: &'a [PlanarPoint],
    pub(super) material: Material,
}

impl PlanarPoint {
    fn midpoint(self, other: Self) -> Self {
        Self { x_oct: (self.x_oct + other.x_oct) * 0.5, z_oct: (self.z_oct + other.z_oct) * 0.5 }
    }

    fn distance_squared(self, other: Self) -> f32 {
        let dx = self.x_oct - other.x_oct;
        let dz = self.z_oct - other.z_oct;
        dx * dx + dz * dz
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SampleAnchor {
    x_oct: i32,
    z_oct: i32,
}

impl SampleAnchor {
    fn point(self) -> PlanarPoint {
        PlanarPoint { x_oct: self.x_oct as f32, z_oct: self.z_oct as f32 }
    }

    fn cell(self) -> CellPos {
        CellPos { x: self.x_oct.div_euclid(256), z: self.z_oct.div_euclid(256) }
    }

    fn surface(self) -> SurfaceAnchor {
        SurfaceAnchor { x_octimeters: self.x_oct, z_octimeters: self.z_oct }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum EdgeAxis {
    East,
    North,
}

/// Stable identity for one physical-cliff adjacency. Coordinates name the
/// lower-coordinate sample in the fixed world subcell lattice.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct CanonicalEdgeKey {
    x_subcell: i32,
    z_subcell: i32,
    axis: EdgeAxis,
}

impl CanonicalEdgeKey {
    fn crossing_point(self) -> PlanarPoint {
        match self.axis {
            EdgeAxis::East => PlanarPoint {
                x_oct: ((self.x_subcell + 1) * OCTIMETERS_PER_SUBCELL) as f32,
                z_oct: (self.z_subcell * OCTIMETERS_PER_SUBCELL + OCTIMETERS_PER_SUBCELL / 2) as f32,
            },
            EdgeAxis::North => PlanarPoint {
                x_oct: (self.x_subcell * OCTIMETERS_PER_SUBCELL + OCTIMETERS_PER_SUBCELL / 2) as f32,
                z_oct: ((self.z_subcell + 1) * OCTIMETERS_PER_SUBCELL) as f32,
            },
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct CliffCrossing {
    key: CanonicalEdgeKey,
    point: PlanarPoint,
    high_anchor: SampleAnchor,
    low_anchor: SampleAnchor,
}

#[derive(Clone, Copy, Debug)]
enum LocalCase {
    /// A two-crossing case whose same high plate touches both crossings.
    /// The chord smooths the corner without inventing an iso-level.
    Chord { high_corners: u8 },
    /// A one/three/four-way junction. Each physical adjacency terminates at
    /// the stable window pin instead of extending an invented iso-contour.
    Pinned { rotation: u8 },
}

#[derive(Clone, Debug)]
struct WindowPlan {
    center: PlanarPoint,
    corners: [SampleAnchor; 4],
    levels: [i32; 4],
    crossings: [Option<CliffCrossing>; 4],
    case: LocalCase,
}

impl WindowPlan {
    fn crossing_count(&self) -> usize {
        self.crossings.iter().flatten().count()
    }

    fn corner_points(&self) -> [PlanarPoint; 4] {
        self.corners.map(SampleAnchor::point)
    }

    fn crossing_edges(&self) -> Vec<usize> {
        self.crossings.iter().enumerate().filter_map(|(edge, crossing)| crossing.map(|_| edge)).collect()
    }

    fn faces(&self) -> Vec<HeightFace> {
        match self.case {
            LocalCase::Chord { high_corners } => self.chord_faces(high_corners),
            LocalCase::Pinned { .. } => self.pinned_faces(),
        }
    }

    fn chord_faces(&self, high_corners: u8) -> Vec<HeightFace> {
        let edges = self.crossing_edges();
        debug_assert_eq!(edges.len(), 2);
        let a = self.crossings[edges[0]].expect("chord crossing").point;
        let b = self.crossings[edges[1]].expect("chord crossing").point;
        let junction = a.midpoint(b);
        let square = self.corner_points().to_vec();
        let mut high = clip_to_line_side(&square, a, b, high_corners, &self.corners);
        let low_corners = 0b1111 ^ high_corners;
        let mut low = clip_to_line_side(&square, a, b, low_corners, &self.corners);
        insert_on_edge(&mut high, a, b, junction);
        insert_on_edge(&mut low, a, b, junction);
        vec![
            HeightFace { polygon: high, anchor_corners: high_corners },
            HeightFace { polygon: low, anchor_corners: low_corners },
        ]
    }

    fn pinned_faces(&self) -> Vec<HeightFace> {
        let [bl, br, tr, tl] = self.corner_points();
        let bottom = bl.midpoint(br);
        let right = br.midpoint(tr);
        let top = tr.midpoint(tl);
        let left = tl.midpoint(bl);
        vec![
            HeightFace { polygon: vec![bl, bottom, self.center, left], anchor_corners: 1 },
            HeightFace { polygon: vec![br, right, self.center, bottom], anchor_corners: 2 },
            HeightFace { polygon: vec![tr, top, self.center, right], anchor_corners: 4 },
            HeightFace { polygon: vec![tl, left, self.center, top], anchor_corners: 8 },
        ]
    }

    fn wall_segments(&self) -> Vec<WallSegment> {
        match self.case {
            LocalCase::Chord { high_corners } => {
                let edges = self.crossing_edges();
                let a = self.crossings[edges[0]].expect("chord crossing");
                let b = self.crossings[edges[1]].expect("chord crossing");
                let junction = a.point.midpoint(b.point);
                let low_corners = 0b1111 ^ high_corners;
                debug_assert_eq!(self.anchor_for(high_corners, a.point), a.high_anchor);
                debug_assert_eq!(self.anchor_for(low_corners, a.point), a.low_anchor);
                debug_assert_eq!(self.anchor_for(high_corners, b.point), b.high_anchor);
                debug_assert_eq!(self.anchor_for(low_corners, b.point), b.low_anchor);
                vec![
                    WallSegment {
                        key: a.key,
                        start: a.point,
                        end: junction,
                        high_corners,
                        low_corners,
                        material_anchor: a.high_anchor,
                    },
                    WallSegment {
                        key: b.key,
                        start: junction,
                        end: b.point,
                        high_corners,
                        low_corners,
                        material_anchor: b.high_anchor,
                    },
                ]
            }
            LocalCase::Pinned { rotation } => {
                let mut segments = Vec::with_capacity(4);
                for offset in 0..4 {
                    let edge = (usize::from(rotation) + offset) % 4;
                    let Some(crossing) = self.crossings[edge] else {
                        continue;
                    };
                    let (a, b) = edge_corners(edge);
                    let high = if self.levels[a] > self.levels[b] {
                        a
                    } else {
                        b
                    };
                    let low = if high == a {
                        b
                    } else {
                        a
                    };
                    debug_assert_eq!(self.corners[high], crossing.high_anchor);
                    debug_assert_eq!(self.corners[low], crossing.low_anchor);
                    segments.push(WallSegment {
                        key: crossing.key,
                        start: crossing.point,
                        end: self.center,
                        high_corners: 1 << high,
                        low_corners: 1 << low,
                        material_anchor: crossing.high_anchor,
                    });
                }
                segments
            }
        }
    }

    fn anchor_for(&self, mask: u8, point: PlanarPoint) -> SampleAnchor {
        (0..4)
            .filter(|corner| mask & (1 << corner) != 0)
            .min_by(|&a, &b| {
                let da = self.corners[a].point().distance_squared(point);
                let db = self.corners[b].point().distance_squared(point);
                da.total_cmp(&db).then_with(|| {
                    let aa = self.corners[a];
                    let bb = self.corners[b];
                    (aa.x_oct, aa.z_oct).cmp(&(bb.x_oct, bb.z_oct))
                })
            })
            .map_or(self.corners[0], |corner| self.corners[corner])
    }

    fn cap_vertex(&self, world: &World, mask: u8, point: PlanarPoint, color: aether_math::Rgb) -> Vertex {
        let anchor = self.anchor_for(mask, point);
        let wx = point.x_oct / OCTIMETERS_PER_METER;
        let wz = point.z_oct / OCTIMETERS_PER_METER;
        Vertex { x: wx, y: side_anchor_lift(world, anchor.surface(), wx, wz), z: wz, color }
    }

    fn anchor_material(world: &World, anchor: SampleAnchor) -> Material {
        world.underlay_point(
            anchor.cell(),
            anchor.x_oct.rem_euclid(256) / OCTIMETERS_PER_SUBCELL,
            anchor.z_oct.rem_euclid(256) / OCTIMETERS_PER_SUBCELL,
        )
    }

    fn material_corner_mask(&self, world: &World, material: Material) -> u8 {
        self.corners.iter().enumerate().fold(0, |mask, (corner, &anchor)| {
            mask | (u8::from(Self::anchor_material(world, anchor) == material) << corner)
        })
    }

    fn enclosed_void_mask(&self, world: &World, mask: u8) -> u8 {
        (0..4).fold(0, |enclosed, corner| {
            let anchor = self.corners[corner];
            let floor =
                enclosed_void_floor(world, VoidAnchor { x_octimeters: anchor.x_oct, z_octimeters: anchor.z_oct });
            enclosed | (u8::from(mask & (1 << corner) != 0 && floor.is_some()) << corner)
        })
    }

    fn void_floor_vertex(&self, world: &World, mask: u8, point: PlanarPoint, color: aether_math::Rgb) -> Vertex {
        let anchor = self.anchor_for(mask, point);
        let floor = enclosed_void_floor(world, VoidAnchor { x_octimeters: anchor.x_oct, z_octimeters: anchor.z_oct })
            .expect("enclosed mask contains only floor anchors");
        Vertex { x: point.x_oct / OCTIMETERS_PER_METER, y: floor.y, z: point.z_oct / OCTIMETERS_PER_METER, color }
    }

    fn wall_height(&self, world: &World, mask: u8, point: PlanarPoint, top: f32) -> f32 {
        let anchor = self.anchor_for(mask, point);
        if world.underlay_point(
            anchor.cell(),
            anchor.x_oct.rem_euclid(256) / OCTIMETERS_PER_SUBCELL,
            anchor.z_oct.rem_euclid(256) / OCTIMETERS_PER_SUBCELL,
        ) == Material::Void
        {
            return void_low_base(world, VoidAnchor { x_octimeters: anchor.x_oct, z_octimeters: anchor.z_oct }, top);
        }
        let wx = point.x_oct / OCTIMETERS_PER_METER;
        let wz = point.z_oct / OCTIMETERS_PER_METER;
        side_anchor_lift(world, anchor.surface(), wx, wz)
    }
}

#[derive(Clone, Debug)]
struct HeightFace {
    polygon: Vec<PlanarPoint>,
    anchor_corners: u8,
}

#[derive(Clone, Copy, Debug)]
struct WallSegment {
    key: CanonicalEdgeKey,
    start: PlanarPoint,
    end: PlanarPoint,
    high_corners: u8,
    low_corners: u8,
    material_anchor: SampleAnchor,
}

/// One bounded per-chunk physical-cliff inventory. The constructor scans
/// the fixed sample apron exactly once in each canonical axis, then retains
/// only the 256 x 256 windows owned by this chunk.
pub(super) struct CliffPlan {
    at: ChunkPos,
    windows: Vec<WindowPlan>,
    window_lookup: Vec<Option<u32>>,
    cells_with_cliffs: Vec<bool>,
    #[cfg(test)]
    adjacency_visits: usize,
}

impl CliffPlan {
    #[allow(clippy::too_many_lines)] // one bounded pass: sample, scan both axes, build owned cases
    pub(super) fn build(world: &World, at: ChunkPos) -> Self {
        let n = SAMPLE_GRID_EDGE;
        let apron = MAX_APRON_SUBCELLS;
        let base_sub_x = at.x * SUBCELLS_PER_CHUNK_EDGE;
        let base_sub_z = at.z * SUBCELLS_PER_CHUNK_EDGE;
        let mut levels = vec![0i32; n * n];
        for row in 0..n {
            for column in 0..n {
                let gx = base_sub_x + column as i32 - apron;
                let gz = base_sub_z + row as i32 - apron;
                levels[row * n + column] = super::surface::point_surface_level_at(
                    world,
                    gx * OCTIMETERS_PER_SUBCELL + OCTIMETERS_PER_SUBCELL / 2,
                    gz * OCTIMETERS_PER_SUBCELL + OCTIMETERS_PER_SUBCELL / 2,
                );
            }
        }

        let mut east = vec![false; n * (n - 1)];
        let mut north = vec![false; (n - 1) * n];
        let mut adjacency_visits = 0;
        for row in 0..n {
            for column in 0..n - 1 {
                east[row * (n - 1) + column] =
                    (levels[row * n + column] - levels[row * n + column + 1]).abs() > STEP_MAX_OCTIMETERS;
                adjacency_visits += 1;
            }
        }
        for row in 0..n - 1 {
            for column in 0..n {
                north[row * n + column] =
                    (levels[row * n + column] - levels[(row + 1) * n + column]).abs() > STEP_MAX_OCTIMETERS;
                adjacency_visits += 1;
            }
        }
        debug_assert_eq!(adjacency_visits, CLIFF_ADJACENCY_BUDGET);

        let mut windows = Vec::new();
        let mut window_lookup = vec![None; OWNED_WINDOWS];
        let mut cells_with_cliffs = vec![false; (EDGE * EDGE) as usize];
        for local_z in 0..=SUBCELLS_PER_CHUNK_EDGE {
            for local_x in 0..=SUBCELLS_PER_CHUNK_EDGE {
                let wi = (local_x + apron - 1) as usize;
                let wj = (local_z + apron - 1) as usize;
                let flags = [
                    east[wj * (n - 1) + wi],
                    north[wj * n + wi + 1],
                    east[(wj + 1) * (n - 1) + wi],
                    north[wj * n + wi],
                ];
                if flags.iter().all(|flag| !flag) {
                    continue;
                }
                let gx = base_sub_x + local_x;
                let gz = base_sub_z + local_z;
                let center = PlanarPoint {
                    x_oct: (gx * OCTIMETERS_PER_SUBCELL) as f32,
                    z_oct: (gz * OCTIMETERS_PER_SUBCELL) as f32,
                };
                let half = OCTIMETERS_PER_SUBCELL / 2;
                let corners = [
                    SampleAnchor {
                        x_oct: gx * OCTIMETERS_PER_SUBCELL - half,
                        z_oct: gz * OCTIMETERS_PER_SUBCELL - half,
                    },
                    SampleAnchor {
                        x_oct: gx * OCTIMETERS_PER_SUBCELL + half,
                        z_oct: gz * OCTIMETERS_PER_SUBCELL - half,
                    },
                    SampleAnchor {
                        x_oct: gx * OCTIMETERS_PER_SUBCELL + half,
                        z_oct: gz * OCTIMETERS_PER_SUBCELL + half,
                    },
                    SampleAnchor {
                        x_oct: gx * OCTIMETERS_PER_SUBCELL - half,
                        z_oct: gz * OCTIMETERS_PER_SUBCELL + half,
                    },
                ];
                for (index, anchor) in corners.iter().enumerate() {
                    let cell = anchor.cell();
                    if corners[..index].iter().any(|other| other.cell() == cell) {
                        continue;
                    }
                    let cell_x = cell.x - at.x * EDGE;
                    let cell_z = cell.z - at.z * EDGE;
                    if (0..EDGE).contains(&cell_x) && (0..EDGE).contains(&cell_z) {
                        cells_with_cliffs[(cell_z * EDGE + cell_x) as usize] = true;
                    }
                }
                // The positive-boundary centers are owned and emitted by the
                // neighboring chunk. They still overlap this chunk's final
                // cell, which must leave its cap to the shared window.
                if local_x == SUBCELLS_PER_CHUNK_EDGE || local_z == SUBCELLS_PER_CHUNK_EDGE {
                    continue;
                }
                let window_levels = [
                    levels[wj * n + wi],
                    levels[wj * n + wi + 1],
                    levels[(wj + 1) * n + wi + 1],
                    levels[(wj + 1) * n + wi],
                ];
                let mut crossings = [None; 4];
                for edge in 0..4 {
                    if flags[edge] {
                        crossings[edge] = Some(make_crossing(gx, gz, edge, corners, window_levels));
                    }
                }
                let case = classify_case(gx, gz, crossings, window_levels);
                let plan = WindowPlan { center, corners, levels: window_levels, crossings, case };
                debug_assert!(plan.crossing_count() <= 4);
                let lookup_index = (local_z * SUBCELLS_PER_CHUNK_EDGE + local_x) as usize;
                window_lookup[lookup_index] = Some(windows.len() as u32);
                windows.push(plan);
            }
        }

        Self {
            at,
            windows,
            window_lookup,
            cells_with_cliffs,
            #[cfg(test)]
            adjacency_visits,
        }
    }

    #[cfg(test)]
    pub(super) fn adjacency_visits(&self) -> usize {
        self.adjacency_visits
    }

    #[cfg(test)]
    pub(super) fn window_count(&self) -> usize {
        self.windows.len()
    }

    #[cfg(test)]
    fn canonical_edges(&self) -> Vec<CanonicalEdgeKey> {
        let mut edges: Vec<_> = self
            .windows
            .iter()
            .flat_map(|window| window.crossings.iter().flatten().map(|crossing| crossing.key))
            .collect();
        edges.sort_unstable();
        edges.dedup();
        edges
    }

    pub(super) fn cell_has_cliff(&self, cell: CellPos) -> bool {
        let local_x = cell.x - self.at.x * EDGE;
        let local_z = cell.z - self.at.z * EDGE;
        (0..EDGE).contains(&local_x)
            && (0..EDGE).contains(&local_z)
            && self.cells_with_cliffs[(local_z * EDGE + local_x) as usize]
    }

    pub(super) fn has_window_at(&self, center: WindowCenter) -> bool {
        self.window_at(center).is_some()
    }

    fn window_at(&self, center: WindowCenter) -> Option<&WindowPlan> {
        let gx = center.x_octimeters.div_euclid(OCTIMETERS_PER_SUBCELL) - self.at.x * SUBCELLS_PER_CHUNK_EDGE;
        let gz = center.z_octimeters.div_euclid(OCTIMETERS_PER_SUBCELL) - self.at.z * SUBCELLS_PER_CHUNK_EDGE;
        if !(0..SUBCELLS_PER_CHUNK_EDGE).contains(&gx) || !(0..SUBCELLS_PER_CHUNK_EDGE).contains(&gz) {
            return None;
        }
        let plan_index = self.window_lookup[(gz * SUBCELLS_PER_CHUNK_EDGE + gx) as usize]?;
        self.windows.get(plan_index as usize)
    }

    pub(super) fn emit_cap_polygon(
        &self,
        world: &World,
        center: WindowCenter,
        cap: MaterialCap<'_>,
        cap_fragment_count: &mut usize,
        styles: &StyleTable,
        tris: &mut Vec<DrawTriangle>,
    ) {
        let Some(window) = self.window_at(center) else {
            return;
        };
        let faces = window.faces();
        debug_assert!(faces.len() <= 4);
        for face in faces {
            let mut fragment = intersect_convex(cap.polygon, &face.polygon);
            insert_boundary_vertices(&mut fragment, &face.polygon);
            if fragment.len() < 3 || polygon_area_doubled(&fragment) < 0.5 {
                continue;
            }
            assert!(
                fragment.len() <= MAX_CAP_FRAGMENT_VERTICES,
                "cliff cap fragment exceeded the fixed topology budget"
            );
            let void_mask = face.anchor_corners & window.material_corner_mask(world, Material::Void);
            let enclosed_mask = window.enclosed_void_mask(world, void_mask);
            if cap.material == Material::Void && enclosed_mask == 0 {
                continue; // open Void has a skirt but deliberately no low cap
            }
            *cap_fragment_count += 1;
            assert!(
                *cap_fragment_count <= MAX_CAP_FRAGMENTS_PER_WINDOW,
                "cliff cap fragments exceeded the fixed per-window topology budget"
            );
            let color = if cap.material == Material::Void {
                let anchor = window.anchor_for(enclosed_mask, fragment[0]);
                let floor =
                    enclosed_void_floor(world, VoidAnchor { x_octimeters: anchor.x_oct, z_octimeters: anchor.z_oct })
                        .expect("enclosed mask contains a floor");
                flat_color(styles.get(world.cliff_material(floor.border)))
            } else {
                flat_color(styles.get(cap.material))
            };
            for index in 1..fragment.len() - 1 {
                let vertex = |point| {
                    if cap.material == Material::Void {
                        window.void_floor_vertex(world, enclosed_mask, point, color)
                    } else {
                        window.cap_vertex(world, face.anchor_corners, point, color)
                    }
                };
                tris.push(DrawTriangle {
                    verts: [vertex(fragment[0]), vertex(fragment[index]), vertex(fragment[index + 1])],
                });
            }
        }
    }

    pub(super) fn emit_walls(&self, world: &World, styles: &StyleTable, tris: &mut Vec<DrawTriangle>) {
        for window in &self.windows {
            let segments = window.wall_segments();
            debug_assert!(segments.len() <= 4);
            for segment in segments {
                let crossing = segment.key.crossing_point();
                debug_assert!(
                    same_point(crossing, segment.start) || same_point(crossing, segment.end),
                    "every wall segment references its physical-cliff adjacency"
                );
                if WindowPlan::anchor_material(world, segment.material_anchor) == Material::Void {
                    continue;
                }
                let top_start =
                    window.cap_vertex(world, segment.high_corners, segment.start, aether_math::Rgb::default());
                let top_end = window.cap_vertex(world, segment.high_corners, segment.end, aether_math::Rgb::default());
                let bottom_start = window.wall_height(world, segment.low_corners, segment.start, top_start.y);
                let bottom_end = window.wall_height(world, segment.low_corners, segment.end, top_end.y);
                if (top_start.y - bottom_start).abs() < f32::EPSILON && (top_end.y - bottom_end).abs() < f32::EPSILON {
                    continue;
                }
                let face = flat_color(styles.get(world.cliff_material(segment.material_anchor.cell())));
                push_wall_quad(
                    tris,
                    [top_start.x, top_start.z, top_start.y],
                    [top_end.x, top_end.z, top_end.y],
                    bottom_start,
                    bottom_end,
                    face,
                );
            }
        }
    }
}

fn edge_corners(edge: usize) -> (usize, usize) {
    match edge {
        0 => (0, 1),
        1 => (1, 2),
        2 => (3, 2),
        _ => (0, 3),
    }
}

fn make_crossing(gx: i32, gz: i32, edge: usize, corners: [SampleAnchor; 4], levels: [i32; 4]) -> CliffCrossing {
    let (a, b) = edge_corners(edge);
    let (high, low) = if levels[a] > levels[b] {
        (a, b)
    } else {
        (b, a)
    };
    let point = corners[a].point().midpoint(corners[b].point());
    let key = match edge {
        0 => CanonicalEdgeKey { x_subcell: gx - 1, z_subcell: gz - 1, axis: EdgeAxis::East },
        1 => CanonicalEdgeKey { x_subcell: gx, z_subcell: gz - 1, axis: EdgeAxis::North },
        2 => CanonicalEdgeKey { x_subcell: gx - 1, z_subcell: gz, axis: EdgeAxis::East },
        _ => CanonicalEdgeKey { x_subcell: gx - 1, z_subcell: gz - 1, axis: EdgeAxis::North },
    };
    CliffCrossing { key, point, high_anchor: corners[high], low_anchor: corners[low] }
}

fn classify_case(gx: i32, gz: i32, crossings: [Option<CliffCrossing>; 4], levels: [i32; 4]) -> LocalCase {
    let edges: Vec<usize> =
        crossings.iter().enumerate().filter_map(|(edge, crossing)| crossing.map(|_| edge)).collect();
    if edges.len() == 2 {
        let mut component = [0usize, 1, 2, 3];
        for (edge, crossing) in crossings.iter().enumerate() {
            if crossing.is_some() {
                continue;
            }
            let (a, b) = edge_corners(edge);
            let from = component[b];
            let to = component[a];
            for item in &mut component {
                if *item == from {
                    *item = to;
                }
            }
        }
        let high_of = |edge: usize| {
            let (a, b) = edge_corners(edge);
            if levels[a] > levels[b] {
                a
            } else {
                b
            }
        };
        let first_high = high_of(edges[0]);
        let second_high = high_of(edges[1]);
        if component[first_high] == component[second_high] {
            let high_component = component[first_high];
            let high_corners =
                (0..4).fold(0u8, |mask, corner| mask | (u8::from(component[corner] == high_component) << corner));
            return LocalCase::Chord { high_corners };
        }
    }
    LocalCase::Pinned { rotation: (gx ^ gz).rem_euclid(4) as u8 }
}

fn cross(a: PlanarPoint, b: PlanarPoint, p: PlanarPoint) -> f32 {
    (b.x_oct - a.x_oct) * (p.z_oct - a.z_oct) - (b.z_oct - a.z_oct) * (p.x_oct - a.x_oct)
}

fn clip_to_line_side(
    polygon: &[PlanarPoint],
    a: PlanarPoint,
    b: PlanarPoint,
    corner_mask: u8,
    corners: &[SampleAnchor; 4],
) -> Vec<PlanarPoint> {
    let representative = (0..4).find(|corner| corner_mask & (1 << corner) != 0).unwrap_or(0);
    let keep_positive = cross(a, b, corners[representative].point()) >= 0.0;
    clip_half_plane(polygon, a, b, keep_positive)
}

fn clip_half_plane(polygon: &[PlanarPoint], a: PlanarPoint, b: PlanarPoint, keep_positive: bool) -> Vec<PlanarPoint> {
    let mut out = Vec::with_capacity(polygon.len() + 2);
    for index in 0..polygon.len() {
        let current = polygon[index];
        let next = polygon[(index + 1) % polygon.len()];
        let current_side = cross(a, b, current);
        let next_side = cross(a, b, next);
        let current_in = if keep_positive {
            current_side >= -0.01
        } else {
            current_side <= 0.01
        };
        let next_in = if keep_positive {
            next_side >= -0.01
        } else {
            next_side <= 0.01
        };
        if current_in {
            out.push(current);
        }
        if current_in != next_in {
            let denominator = current_side - next_side;
            if denominator.abs() > f32::EPSILON {
                let t = current_side / denominator;
                out.push(PlanarPoint {
                    x_oct: current.x_oct + (next.x_oct - current.x_oct) * t,
                    z_oct: current.z_oct + (next.z_oct - current.z_oct) * t,
                });
            }
        }
    }
    dedup_polygon(out)
}

fn insert_on_edge(polygon: &mut Vec<PlanarPoint>, edge_a: PlanarPoint, edge_b: PlanarPoint, point: PlanarPoint) {
    for index in 0..polygon.len() {
        let a = polygon[index];
        let b = polygon[(index + 1) % polygon.len()];
        if (same_point(a, edge_a) && same_point(b, edge_b)) || (same_point(a, edge_b) && same_point(b, edge_a)) {
            polygon.insert(index + 1, point);
            return;
        }
    }
}

/// Preserve the local case's named contour vertices after convex clipping.
/// Sutherland-Hodgman keeps subject vertices and intersections, but a clip
/// vertex lying on a coincident subject edge is otherwise omitted. Wall
/// half-ribbons end at those chord pins, so caps must retain the same point
/// explicitly for byte-identical shared-vertex closure.
fn insert_boundary_vertices(polygon: &mut Vec<PlanarPoint>, vertices: &[PlanarPoint]) {
    for &point in vertices {
        if polygon.iter().any(|&existing| same_point(existing, point)) {
            continue;
        }
        for index in 0..polygon.len() {
            let a = polygon[index];
            let b = polygon[(index + 1) % polygon.len()];
            if point_on_segment(point, a, b) {
                polygon.insert(index + 1, point);
                break;
            }
        }
    }
}

fn point_on_segment(point: PlanarPoint, a: PlanarPoint, b: PlanarPoint) -> bool {
    if cross(a, b, point).abs() > 0.01 {
        return false;
    }
    let dot = (point.x_oct - a.x_oct) * (point.x_oct - b.x_oct) + (point.z_oct - a.z_oct) * (point.z_oct - b.z_oct);
    dot <= 0.01
}

fn intersect_convex(subject: &[PlanarPoint], clip: &[PlanarPoint]) -> Vec<PlanarPoint> {
    let mut out = subject.to_vec();
    let clip_positive = signed_area(clip) >= 0.0;
    for index in 0..clip.len() {
        if out.is_empty() {
            break;
        }
        out = clip_half_plane(&out, clip[index], clip[(index + 1) % clip.len()], clip_positive);
    }
    dedup_polygon(out)
}

fn signed_area(polygon: &[PlanarPoint]) -> f32 {
    polygon
        .iter()
        .enumerate()
        .map(|(index, point)| {
            let next = polygon[(index + 1) % polygon.len()];
            point.x_oct * next.z_oct - next.x_oct * point.z_oct
        })
        .sum::<f32>()
        * 0.5
}

fn polygon_area_doubled(polygon: &[PlanarPoint]) -> f32 {
    signed_area(polygon).abs() * 2.0
}

fn same_point(a: PlanarPoint, b: PlanarPoint) -> bool {
    (a.x_oct - b.x_oct).abs() < 0.01 && (a.z_oct - b.z_oct).abs() < 0.01
}

fn dedup_polygon(points: Vec<PlanarPoint>) -> Vec<PlanarPoint> {
    let mut out = Vec::with_capacity(points.len());
    for point in points {
        if out.last().is_none_or(|last| !same_point(*last, point)) {
            out.push(point);
        }
    }
    if out.len() > 1 && same_point(out[0], *out.last().expect("nonempty")) {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::contour::{label_case, label_window_polys};
    use super::super::windows::label_case_is_connected;
    use super::*;
    use crate::world::{CELLS_PER_CHUNK_AREA, Chunk};
    use core::array::from_fn;

    fn fixture_anchors() -> [SampleAnchor; 4] {
        [
            SampleAnchor { x_oct: -8, z_oct: -8 },
            SampleAnchor { x_oct: 8, z_oct: -8 },
            SampleAnchor { x_oct: 8, z_oct: 8 },
            SampleAnchor { x_oct: -8, z_oct: 8 },
        ]
    }

    fn window_for_levels(levels: [i32; 4]) -> WindowPlan {
        let corners = fixture_anchors();
        let mut crossings = [None; 4];
        for (edge, crossing) in crossings.iter_mut().enumerate() {
            let (a, b) = edge_corners(edge);
            if (levels[a] - levels[b]).abs() > STEP_MAX_OCTIMETERS {
                *crossing = Some(make_crossing(0, 0, edge, corners, levels));
            }
        }
        WindowPlan {
            center: PlanarPoint { x_oct: 0.0, z_oct: 0.0 },
            corners,
            levels,
            crossings,
            case: classify_case(0, 0, crossings, levels),
        }
    }

    fn binary_height_window(high_corners: u8) -> WindowPlan {
        window_for_levels(from_fn(|corner| {
            if high_corners & (1 << corner) != 0 {
                200
            } else {
                0
            }
        }))
    }

    fn material_polygons(corners: [u8; 4]) -> Vec<Vec<PlanarPoint>> {
        let [bottom_left, bottom_right, top_right, top_left] = fixture_anchors().map(SampleAnchor::point);
        let points = [
            bottom_left,
            bottom_right,
            top_right,
            top_left,
            bottom_left.midpoint(bottom_right),
            bottom_right.midpoint(top_right),
            top_right.midpoint(top_left),
            top_left.midpoint(bottom_left),
        ];
        let mut polygons = Vec::new();
        for index in 0..4 {
            let label = corners[index];
            if corners[..index].contains(&label) {
                continue;
            }
            let case = label_case(&corners, 2, 0, 0, label);
            let connected = label_case_is_connected(corners, label, case);
            for polygon in label_window_polys(case, connected) {
                if !polygon.is_empty() {
                    polygons.push(polygon.iter().map(|&point| points[point as usize]).collect());
                }
            }
        }
        polygons
    }

    fn world_with_levels(level: impl Fn(i32, i32) -> i32) -> World {
        let mut world = World::new();
        for chunk_z in -1..=1 {
            for chunk_x in -1..=1 {
                let mut chunk = Chunk::empty();
                chunk.underlay = [Material::Grass; CELLS_PER_CHUNK_AREA];
                for local_z in 0..EDGE {
                    for local_x in 0..EDGE {
                        let x = chunk_x * EDGE + local_x;
                        let z = chunk_z * EDGE + local_z;
                        chunk.height[(local_z * EDGE + local_x) as usize] = level(x, z);
                    }
                }
                world.insert_chunk(ChunkPos { x: chunk_x, z: chunk_z }, chunk);
            }
        }
        world
    }

    #[test]
    fn fixed_apron_visits_every_canonical_adjacency_once() {
        let world = world_with_levels(|_, _| 0);
        let plan = CliffPlan::build(&world, ChunkPos { x: 0, z: 0 });
        assert_eq!(plan.adjacency_visits(), 204_160);
        assert_eq!(plan.adjacency_visits(), CLIFF_ADJACENCY_BUDGET);
        assert_eq!(plan.window_count(), 0);
    }

    #[test]
    fn legal_ramps_do_not_acquire_an_unrelated_cliff_contour() {
        let world = world_with_levels(|x, z| {
            let ramp = (x * 8).clamp(-64, 64);
            if z >= 4 {
                ramp + 256
            } else {
                ramp
            }
        });
        let shifted = world_with_levels(|x, z| {
            let ramp = (x * 8).clamp(-64, 64) + 10_000;
            if z >= 4 {
                ramp + 256
            } else {
                ramp
            }
        });
        let a = CliffPlan::build(&world, ChunkPos { x: 0, z: 0 });
        let b = CliffPlan::build(&shifted, ChunkPos { x: 0, z: 0 });
        assert_eq!(a.window_count(), b.window_count());
        assert!(a.window_count() <= OWNED_WINDOWS);
        for local_z in 0..SUBCELLS_PER_CHUNK_EDGE {
            for local_x in 0..SUBCELLS_PER_CHUNK_EDGE {
                let center_x = local_x * OCTIMETERS_PER_SUBCELL;
                let center_z = local_z * OCTIMETERS_PER_SUBCELL;
                let center = WindowCenter { x_octimeters: center_x, z_octimeters: center_z };
                assert_eq!(a.has_window_at(center), b.has_window_at(center),);
            }
        }
    }

    #[test]
    fn every_abstract_local_case_stays_within_the_four_segment_bound() {
        let corners = fixture_anchors();
        let levels = [0, 100, 200, 300];
        for crossing_mask in 1u8..16 {
            for rotation in 0..4 {
                let mut crossings = [None; 4];
                for (edge, crossing) in crossings.iter_mut().enumerate() {
                    if crossing_mask & (1 << edge) != 0 {
                        *crossing = Some(make_crossing(0, 0, edge, corners, levels));
                    }
                }
                let plan = WindowPlan {
                    center: PlanarPoint { x_oct: 0.0, z_oct: 0.0 },
                    corners,
                    levels,
                    crossings,
                    case: LocalCase::Pinned { rotation },
                };
                assert_eq!(plan.wall_segments().len(), crossing_mask.count_ones() as usize);
                assert!(plan.wall_segments().len() <= 4);
                assert_eq!(plan.faces().len(), 4);
            }
        }

        for high_corners in 1u8..15 {
            let low_corners = 0b1111 ^ high_corners;
            if high_corners > low_corners {
                continue;
            }
            let plan = binary_height_window(high_corners);
            assert!(plan.wall_segments().len() <= 4);
            assert!(plan.faces().len() <= 4);
        }
    }

    #[test]
    fn exhaustive_material_and_height_arrangements_fit_the_named_cap_bounds() {
        let mut height_arrangements = Vec::new();
        for high_corners in 1u8..15 {
            let low_corners = 0b1111 ^ high_corners;
            if high_corners > low_corners {
                continue;
            }
            height_arrangements.push(binary_height_window(high_corners).faces());
        }
        assert_eq!(height_arrangements.len(), 7);

        let mut maximum_fragments = 0;
        let mut maximum_vertices = 0;
        for encoded in 0..256usize {
            let mut remaining = encoded;
            let corners = from_fn(|_| {
                let label = (remaining % 4) as u8;
                remaining /= 4;
                label
            });
            let material_polygons = material_polygons(corners);
            for faces in &height_arrangements {
                let mut fragment_count = 0;
                for polygon in &material_polygons {
                    for face in faces {
                        let mut fragment = intersect_convex(polygon, &face.polygon);
                        insert_boundary_vertices(&mut fragment, &face.polygon);
                        if fragment.len() < 3 || polygon_area_doubled(&fragment) < 0.5 {
                            continue;
                        }
                        fragment_count += 1;
                        maximum_vertices = maximum_vertices.max(fragment.len());
                    }
                }
                maximum_fragments = maximum_fragments.max(fragment_count);
                assert!(fragment_count <= MAX_CAP_FRAGMENTS_PER_WINDOW);
                assert!(maximum_vertices <= MAX_CAP_FRAGMENT_VERTICES);
            }
        }
        assert_eq!(maximum_fragments, 6);
        assert_eq!(maximum_vertices, 7);
    }

    #[test]
    fn local_cases_chord_consistent_plates_and_pin_junctions() {
        let chord = window_for_levels([200, 0, 0, 0]);
        assert!(matches!(chord.case, LocalCase::Chord { .. }));
        assert_eq!(chord.wall_segments().len(), 2);
        assert_eq!(chord.faces().len(), 2);

        // One large break can return around the other three sides through
        // legal steps. It is a branch cut, not an iso-contour to extend
        // across the window, so the local case pins its sole physical edge.
        let branch = window_for_levels([0, 100, 70, 40]);
        assert!(matches!(branch.case, LocalCase::Pinned { .. }));
        assert_eq!(branch.wall_segments().len(), 1);

        let saddle = window_for_levels([0, 200, 0, 200]);
        assert!(matches!(saddle.case, LocalCase::Pinned { .. }));
        assert_eq!(saddle.wall_segments().len(), 4);
    }

    #[test]
    fn seam_ramp_keeps_only_the_real_cliff_and_shares_canonical_keys() {
        // Every x cell has a distinct legal ramp level. The only physical
        // cliff is the north/south 256-octimeter offset at z=8, crossing the
        // x=16 chunk seam. A global numeric-threshold march would contour
        // through the ramp; canonical adjacency classification cannot.
        let world = world_with_levels(|x, z| {
            x * 4
                + if z >= 8 {
                    256
                } else {
                    0
                }
        });
        let west = CliffPlan::build(&world, ChunkPos { x: 0, z: 0 });
        let east = CliffPlan::build(&world, ChunkPos { x: 1, z: 0 });
        let west_edges = west.canonical_edges();
        let east_edges = east.canonical_edges();
        assert!(!west_edges.is_empty() && !east_edges.is_empty());
        assert!(
            west_edges.iter().chain(&east_edges).all(|edge| edge.axis == EdgeAxis::North),
            "the unrelated legal x ramp must not acquire an east-facing contour",
        );
        assert!(
            west_edges.iter().any(|edge| east_edges.contains(edge)),
            "neighbor plans derive an identical canonical key at the chunk seam",
        );
        assert_eq!(west.adjacency_visits(), CLIFF_ADJACENCY_BUDGET);
        assert_eq!(east.adjacency_visits(), CLIFF_ADJACENCY_BUDGET);
    }
}
