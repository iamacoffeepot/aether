use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::cmp::Ordering;

use aether_capabilities::render::DrawTriangle;

use super::atlas_support::{
    QuantizedPoint256, assert_height_break_walls_close_where, quantized_xyz, signed_xz_area_doubled, xz_area_doubled,
};
use super::mesh_chunk;
use super::style::StyleTable;
use crate::world::{
    CELLS_PER_CHUNK, CellPos, Chunk, Material, Region, SUBCELLS_PER_CELL, SUBCELLS_PER_CELL_EDGE, World,
};

const CLIFF_REGION_ID: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq)]
struct GroundCentroidMeters {
    x_meters: f32,
    z_meters: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
#[allow(clippy::struct_field_names)] // The scoped reduction vocabulary requires named axes with explicit meter units.
struct GroundBoundsMeters {
    min_x_meters: f32,
    min_z_meters: f32,
    max_x_meters: f32,
    max_z_meters: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct CaseReduction {
    triangle_count: usize,
    cap_area_square_meters: f32,
    area_weighted_ground_centroid: Option<GroundCentroidMeters>,
    projected_bounds: Option<GroundBoundsMeters>,
}

#[derive(Clone, Copy)]
struct ExteriorBoundaryMeters {
    ground_bounds: GroundBoundsMeters,
    boundary_band_meters: f32,
    max_edge_length_meters: f32,
}

#[derive(Clone, Copy)]
struct AtlasCaseSpec {
    kind: AtlasCaseKind,
    golden: CaseReduction,
    exterior_boundary: Option<ExteriorBoundaryMeters>,
    requires_height_break_closure: bool,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct QuantizedGroundPoint256 {
    x_256th_meter: i64,
    z_256th_meter: i64,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct GroundMeshEdge256 {
    start: QuantizedGroundPoint256,
    end: QuantizedGroundPoint256,
}

#[derive(Clone, Copy)]
struct GroundVectorMeters {
    x_meters: f32,
    z_meters: f32,
}

#[derive(Clone, Copy)]
struct WeightedGroundCentroidCubicMeters {
    x_cubic_meters: f32,
    z_cubic_meters: f32,
}

struct CapOrientation<'a> {
    triangle: &'a DrawTriangle,
    signed_area_doubled: f32,
}

#[derive(Clone, Copy)]
enum AtlasCaseKind {
    Empty,
    SingleLabelAllInside,
    TwoLabelWindow,
    CliffHeightBreak,
}

impl AtlasCaseKind {
    fn name(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::SingleLabelAllInside => "single_label_all_inside",
            Self::TwoLabelWindow => "two_label_window",
            Self::CliffHeightBreak => "cliff_height_break",
        }
    }

    fn anchor(self) -> CellPos {
        match self {
            Self::Empty => CellPos { x: 2, z: 2 },
            Self::SingleLabelAllInside => CellPos { x: 7, z: 2 },
            Self::TwoLabelWindow => CellPos { x: 2, z: 7 },
            Self::CliffHeightBreak => CellPos { x: 7, z: 7 },
        }
    }

    fn world(self) -> World {
        let anchor = self.anchor();
        match self {
            Self::Empty => World::new(),
            Self::SingleLabelAllInside => {
                let mut world = World::new();
                world.set_cell_points(anchor, &[Material::Grass.to_u8(); SUBCELLS_PER_CELL]);
                world
            }
            Self::TwoLabelWindow => {
                let edge = usize::try_from(SUBCELLS_PER_CELL_EDGE).expect("subcell edge fits usize");
                let mut points = Vec::with_capacity(SUBCELLS_PER_CELL);
                for z in 0..edge {
                    for x in 0..edge {
                        points.push(if x + z < edge {
                            Material::Grass.to_u8()
                        } else {
                            Material::Stone.to_u8()
                        });
                    }
                }
                let mut world = World::new();
                world.set_cell_points(anchor, &points);
                world
            }
            Self::CliffHeightBreak => {
                let edge = usize::try_from(SUBCELLS_PER_CELL_EDGE).expect("subcell edge fits usize");
                let mut deltas = Vec::with_capacity(SUBCELLS_PER_CELL);
                for z in 0..edge {
                    for _ in 0..edge {
                        deltas.push(if z < edge / 2 {
                            256
                        } else {
                            0
                        });
                    }
                }
                let mut world = World::new();
                world.insert_region(
                    CLIFF_REGION_ID,
                    Region {
                        name: "atlas-cliff".into(),
                        default_material: Material::Grass,
                        cliff_material: Material::Sand,
                    },
                );
                let mut chunk = Chunk::empty();
                let local_x = usize::try_from(anchor.x.rem_euclid(CELLS_PER_CHUNK)).expect("local x is non-negative");
                let local_z = usize::try_from(anchor.z.rem_euclid(CELLS_PER_CHUNK)).expect("local z is non-negative");
                let chunk_edge = usize::try_from(CELLS_PER_CHUNK).expect("chunk edge is positive");
                chunk.region[local_z * chunk_edge + local_x] =
                    u16::try_from(CLIFF_REGION_ID).expect("atlas region id fits u16");
                world.insert_chunk(anchor.chunk(), chunk);
                world.set_cell_heights(anchor, &deltas);
                world
            }
        }
    }
}

fn reduce(mesh: &[DrawTriangle]) -> CaseReduction {
    let mut cap_area_square_meters = 0.0;
    let mut weighted_centroid = WeightedGroundCentroidCubicMeters { x_cubic_meters: 0.0, z_cubic_meters: 0.0 };
    let mut projected_bounds = None::<GroundBoundsMeters>;

    for triangle in mesh {
        for vertex in &triangle.verts {
            let bounds = projected_bounds.get_or_insert(GroundBoundsMeters {
                min_x_meters: vertex.x,
                min_z_meters: vertex.z,
                max_x_meters: vertex.x,
                max_z_meters: vertex.z,
            });
            bounds.min_x_meters = bounds.min_x_meters.min(vertex.x);
            bounds.min_z_meters = bounds.min_z_meters.min(vertex.z);
            bounds.max_x_meters = bounds.max_x_meters.max(vertex.x);
            bounds.max_z_meters = bounds.max_z_meters.max(vertex.z);
        }

        let area_square_meters = xz_area_doubled(triangle) * 0.5;
        if area_square_meters <= 1e-6 {
            continue;
        }
        let centroid = GroundCentroidMeters {
            x_meters: triangle.verts.iter().map(|vertex| vertex.x).sum::<f32>() / 3.0,
            z_meters: triangle.verts.iter().map(|vertex| vertex.z).sum::<f32>() / 3.0,
        };
        cap_area_square_meters += area_square_meters;
        weighted_centroid.x_cubic_meters += centroid.x_meters * area_square_meters;
        weighted_centroid.z_cubic_meters += centroid.z_meters * area_square_meters;
    }

    CaseReduction {
        triangle_count: mesh.len(),
        cap_area_square_meters,
        area_weighted_ground_centroid: (cap_area_square_meters > 1e-6).then(|| GroundCentroidMeters {
            x_meters: weighted_centroid.x_cubic_meters / cap_area_square_meters,
            z_meters: weighted_centroid.z_cubic_meters / cap_area_square_meters,
        }),
        projected_bounds,
    }
}

fn assert_close(case_name: &str, field_name: &str, actual: f32, expected: f32, tolerance: f32) {
    assert!(
        (actual - expected).abs() <= tolerance,
        "{case_name}: {field_name} expected {expected} ± {tolerance}, got {actual}",
    );
}

fn assert_reduction(case_name: &str, actual: CaseReduction, expected: CaseReduction) {
    assert_eq!(
        actual.triangle_count, expected.triangle_count,
        "{case_name}: triangle count drifted from the demo-seeded golden",
    );
    assert_close(
        case_name,
        "cap area (square meters)",
        actual.cap_area_square_meters,
        expected.cap_area_square_meters,
        1e-6,
    );
    if let Some(expected_centroid) = expected.area_weighted_ground_centroid {
        let actual_centroid = actual
            .area_weighted_ground_centroid
            .unwrap_or_else(|| panic!("{case_name}: expected an area-weighted ground centroid"));
        assert_close(case_name, "centroid x (meters)", actual_centroid.x_meters, expected_centroid.x_meters, 1e-5);
        assert_close(case_name, "centroid z (meters)", actual_centroid.z_meters, expected_centroid.z_meters, 1e-5);
    } else {
        assert!(actual.area_weighted_ground_centroid.is_none(), "{case_name}: expected no centroid");
    }
    if let Some(expected_bounds) = expected.projected_bounds {
        let actual_bounds =
            actual.projected_bounds.unwrap_or_else(|| panic!("{case_name}: expected projected ground bounds"));
        assert_close(
            case_name,
            "bounds min x (meters)",
            actual_bounds.min_x_meters,
            expected_bounds.min_x_meters,
            1e-6,
        );
        assert_close(
            case_name,
            "bounds min z (meters)",
            actual_bounds.min_z_meters,
            expected_bounds.min_z_meters,
            1e-6,
        );
        assert_close(
            case_name,
            "bounds max x (meters)",
            actual_bounds.max_x_meters,
            expected_bounds.max_x_meters,
            1e-6,
        );
        assert_close(
            case_name,
            "bounds max z (meters)",
            actual_bounds.max_z_meters,
            expected_bounds.max_z_meters,
            1e-6,
        );
    } else {
        assert!(actual.projected_bounds.is_none(), "{case_name}: expected no projected bounds");
    }
}

fn assert_consistent_cap_winding(case_name: &str, mesh: &[DrawTriangle]) {
    // Tripwire: a reversed contour fan or swapped clip edge flips one cap
    // triangle and makes back-face culling punch a named hole in the case.
    let mut caps = mesh
        .iter()
        .map(|triangle| CapOrientation { triangle, signed_area_doubled: signed_xz_area_doubled(triangle) })
        .filter(|cap| cap.signed_area_doubled.abs() > 1e-6);
    let Some(first_cap) = caps.next() else {
        assert!(mesh.is_empty(), "{case_name}: a non-empty mesh must contain cap triangles");
        return;
    };
    for cap in caps {
        assert!(
            cap.signed_area_doubled.signum() == first_cap.signed_area_doubled.signum(),
            "{case_name}: cap winding changed sign ({} then {}) at {:?}",
            first_cap.signed_area_doubled,
            cap.signed_area_doubled,
            cap.triangle.verts,
        );
    }
}

fn ground_point(point: QuantizedPoint256) -> QuantizedGroundPoint256 {
    QuantizedGroundPoint256 { x_256th_meter: point.x_256th_meter, z_256th_meter: point.z_256th_meter }
}

fn normalized_edge(start: QuantizedGroundPoint256, end: QuantizedGroundPoint256) -> Option<GroundMeshEdge256> {
    match start.cmp(&end) {
        Ordering::Less => Some(GroundMeshEdge256 { start, end }),
        Ordering::Equal => None,
        Ordering::Greater => Some(GroundMeshEdge256 { start: end, end: start }),
    }
}

fn squared_distance_256(start: QuantizedGroundPoint256, point: QuantizedGroundPoint256) -> i128 {
    let x = i128::from(point.x_256th_meter - start.x_256th_meter);
    let z = i128::from(point.z_256th_meter - start.z_256th_meter);
    x * x + z * z
}

fn point_lies_on_edge_256(point: QuantizedGroundPoint256, edge: GroundMeshEdge256) -> bool {
    let edge_x = i128::from(edge.end.x_256th_meter - edge.start.x_256th_meter);
    let edge_z = i128::from(edge.end.z_256th_meter - edge.start.z_256th_meter);
    let point_x = i128::from(point.x_256th_meter - edge.start.x_256th_meter);
    let point_z = i128::from(point.z_256th_meter - edge.start.z_256th_meter);
    if edge_x * point_z != edge_z * point_x {
        return false;
    }
    let projection = edge_x * point_x + edge_z * point_z;
    let length_squared = edge_x * edge_x + edge_z * edge_z;
    (0..=length_squared).contains(&projection)
}

fn quantize_meters(meters: f32) -> i64 {
    (meters * 256.0).round() as i64
}

fn edge_is_declared_exterior(edge: GroundMeshEdge256, boundary: ExteriorBoundaryMeters) -> bool {
    let min_x = quantize_meters(boundary.ground_bounds.min_x_meters);
    let min_z = quantize_meters(boundary.ground_bounds.min_z_meters);
    let max_x = quantize_meters(boundary.ground_bounds.max_x_meters);
    let max_z = quantize_meters(boundary.ground_bounds.max_z_meters);
    let within_x = |point: QuantizedGroundPoint256| (min_x..=max_x).contains(&point.x_256th_meter);
    let within_z = |point: QuantizedGroundPoint256| (min_z..=max_z).contains(&point.z_256th_meter);
    let boundary_band = quantize_meters(boundary.boundary_band_meters);
    let on_boundary = |point: QuantizedGroundPoint256| {
        within_x(point)
            && within_z(point)
            && ((point.x_256th_meter - min_x).abs() <= boundary_band
                || (point.x_256th_meter - max_x).abs() <= boundary_band
                || (point.z_256th_meter - min_z).abs() <= boundary_band
                || (point.z_256th_meter - max_z).abs() <= boundary_band)
    };
    let edge_vector = GroundVectorMeters {
        x_meters: (edge.end.x_256th_meter - edge.start.x_256th_meter) as f32 / 256.0,
        z_meters: (edge.end.z_256th_meter - edge.start.z_256th_meter) as f32 / 256.0,
    };
    on_boundary(edge.start)
        && on_boundary(edge.end)
        && edge_vector.x_meters.hypot(edge_vector.z_meters) <= boundary.max_edge_length_meters
}

fn assert_watertight_contour(
    case_name: &str,
    mesh: &[DrawTriangle],
    exterior_boundary: Option<ExteriorBoundaryMeters>,
) {
    // Tripwire: a missing or duplicate marched segment leaves a singleton or
    // over-shared interior edge instead of a paired contour seam.
    let cap_triangles: Vec<&DrawTriangle> = mesh.iter().filter(|triangle| xz_area_doubled(triangle) > 1e-6).collect();
    let mut mesh_points: Vec<QuantizedGroundPoint256> = cap_triangles
        .iter()
        .flat_map(|triangle| &triangle.verts)
        .map(|vertex| ground_point(quantized_xyz(vertex)))
        .collect();
    mesh_points.sort_unstable();
    mesh_points.dedup();
    let mut incidence = BTreeMap::<GroundMeshEdge256, usize>::new();
    for triangle in cap_triangles {
        for edge_index in 0..3 {
            let Some(edge) = normalized_edge(
                ground_point(quantized_xyz(&triangle.verts[edge_index])),
                ground_point(quantized_xyz(&triangle.verts[(edge_index + 1) % 3])),
            ) else {
                continue;
            };
            let mut edge_points: Vec<QuantizedGroundPoint256> =
                mesh_points.iter().copied().filter(|point| point_lies_on_edge_256(*point, edge)).collect();
            edge_points.sort_unstable_by_key(|point| squared_distance_256(edge.start, *point));
            for point_index in 0..edge_points.len() - 1 {
                let sub_edge = normalized_edge(edge_points[point_index], edge_points[point_index + 1])
                    .expect("deduplicated edge points make a non-degenerate segment");
                *incidence.entry(sub_edge).or_default() += 1;
            }
        }
    }
    let mut exterior_edge_count = 0;
    for (edge, count) in incidence {
        if exterior_boundary.is_some_and(|boundary| edge_is_declared_exterior(edge, boundary)) {
            exterior_edge_count += 1;
            continue;
        }
        if count == 2 {
            continue;
        }
        panic!("{case_name}: contour edge {edge:?} has incidence {count}, expected an interior pair");
    }
    if exterior_boundary.is_some() {
        assert!(exterior_edge_count > 0, "{case_name}: the declared exterior boundary classified no edges");
    }
}

fn demo_seeded_cases() -> [AtlasCaseSpec; 4] {
    [
        AtlasCaseSpec {
            kind: AtlasCaseKind::Empty,
            golden: CaseReduction {
                triangle_count: 0,
                cap_area_square_meters: 0.0,
                area_weighted_ground_centroid: None,
                projected_bounds: None,
            },
            exterior_boundary: None,
            requires_height_break_closure: false,
        },
        AtlasCaseSpec {
            kind: AtlasCaseKind::SingleLabelAllInside,
            golden: CaseReduction {
                triangle_count: 154,
                cap_area_square_meters: 0.998_046_9,
                area_weighted_ground_centroid: Some(GroundCentroidMeters { x_meters: 7.5, z_meters: 2.499_999_8 }),
                projected_bounds: Some(GroundBoundsMeters {
                    min_x_meters: 7.0,
                    min_z_meters: 2.0,
                    max_x_meters: 8.0,
                    max_z_meters: 3.0,
                }),
            },
            exterior_boundary: Some(ExteriorBoundaryMeters {
                ground_bounds: GroundBoundsMeters {
                    min_x_meters: 7.0,
                    min_z_meters: 2.0,
                    max_x_meters: 8.0,
                    max_z_meters: 3.0,
                },
                boundary_band_meters: 0.5 / SUBCELLS_PER_CELL_EDGE as f32 + 1e-4,
                max_edge_length_meters: 1.0 + 1e-4,
            }),
            requires_height_break_closure: false,
        },
        AtlasCaseSpec {
            kind: AtlasCaseKind::TwoLabelWindow,
            golden: CaseReduction {
                triangle_count: 294,
                cap_area_square_meters: 0.996_093_75,
                area_weighted_ground_centroid: Some(GroundCentroidMeters { x_meters: 2.499_949, z_meters: 7.499_95 }),
                projected_bounds: Some(GroundBoundsMeters {
                    min_x_meters: 2.0,
                    min_z_meters: 7.0,
                    max_x_meters: 3.0,
                    max_z_meters: 8.0,
                }),
            },
            exterior_boundary: Some(ExteriorBoundaryMeters {
                ground_bounds: GroundBoundsMeters {
                    min_x_meters: 2.0,
                    min_z_meters: 7.0,
                    max_x_meters: 3.0,
                    max_z_meters: 8.0,
                },
                boundary_band_meters: 0.5 / SUBCELLS_PER_CELL_EDGE as f32 + 1e-4,
                max_edge_length_meters: 1.0 + 1e-4,
            }),
            requires_height_break_closure: false,
        },
        AtlasCaseSpec {
            kind: AtlasCaseKind::CliffHeightBreak,
            golden: CaseReduction {
                triangle_count: 471,
                cap_area_square_meters: 0.998_046_9,
                area_weighted_ground_centroid: Some(GroundCentroidMeters { x_meters: 7.5, z_meters: 7.500_004 }),
                projected_bounds: Some(GroundBoundsMeters {
                    min_x_meters: 7.0,
                    min_z_meters: 7.0,
                    max_x_meters: 8.0,
                    max_z_meters: 8.0,
                }),
            },
            exterior_boundary: Some(ExteriorBoundaryMeters {
                ground_bounds: GroundBoundsMeters {
                    min_x_meters: 7.0,
                    min_z_meters: 7.0,
                    max_x_meters: 8.0,
                    max_z_meters: 8.0,
                },
                boundary_band_meters: 0.5 / SUBCELLS_PER_CELL_EDGE as f32 + 1e-4,
                max_edge_length_meters: 1.0 + 1e-4,
            }),
            requires_height_break_closure: true,
        },
    ]
}

#[test]
fn demo_seeded_case_atlas_golden_reductions() {
    for case in demo_seeded_cases() {
        let case_name = case.kind.name();
        let world = case.kind.world();
        let mesh = mesh_chunk(&world, case.kind.anchor().chunk(), &StyleTable::default());

        // Tripwire: these demo-seeded computed reductions drift whenever the
        // named case's producing geometry changes.
        assert_reduction(case_name, reduce(&mesh), case.golden);
    }
}

#[test]
fn demo_seeded_case_atlas_contours_are_watertight() {
    for case in demo_seeded_cases() {
        let case_name = case.kind.name();
        let world = case.kind.world();
        let mesh = mesh_chunk(&world, case.kind.anchor().chunk(), &StyleTable::default());
        assert_watertight_contour(case_name, &mesh, case.exterior_boundary);
    }
}

#[test]
fn demo_seeded_case_atlas_cap_winding_is_consistent() {
    for case in demo_seeded_cases() {
        let case_name = case.kind.name();
        let world = case.kind.world();
        let mesh = mesh_chunk(&world, case.kind.anchor().chunk(), &StyleTable::default());
        assert_consistent_cap_winding(case_name, &mesh);
    }
}

#[test]
fn cliff_height_break_walls_close_both_plates() {
    let case = demo_seeded_cases()
        .into_iter()
        .find(|case| case.requires_height_break_closure)
        .expect("the demo atlas declares a height-break case");
    let case_name = case.kind.name();
    let world = case.kind.world();
    let mesh = mesh_chunk(&world, case.kind.anchor().chunk(), &StyleTable::default());
    let exterior = case.exterior_boundary.expect("the height-break fixture declares its exterior boundary");
    let interior_bounds = GroundBoundsMeters {
        min_x_meters: exterior.ground_bounds.min_x_meters + exterior.boundary_band_meters,
        min_z_meters: exterior.ground_bounds.min_z_meters + exterior.boundary_band_meters,
        max_x_meters: exterior.ground_bounds.max_x_meters - exterior.boundary_band_meters,
        max_z_meters: exterior.ground_bounds.max_z_meters - exterior.boundary_band_meters,
    };
    // Tripwire: every split cap must meet a wall at both plates; this guards
    // the fixed #2856 low-corner wall-base regression.
    assert_height_break_walls_close_where(&mesh, case_name, |split| {
        (interior_bounds.min_x_meters..interior_bounds.max_x_meters).contains(&split.x_meters)
            && (interior_bounds.min_z_meters..interior_bounds.max_z_meters).contains(&split.z_meters)
    });
}
