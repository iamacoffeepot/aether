//! Resident silhouette inputs and the CPU reference for their eventual GPU
//! derivation.
//!
//! Nothing in this module is on the live render path. It fixes the data and
//! topology contracts the compute path will consume once the authored
//! compute/indirect surface exists:
//!
//! - one canonical record per undirected mesh edge, with incident faces in
//!   stable face/local-edge order;
//! - dense, bone-order pose weights rather than the quantised draw binding;
//! - checked worst-case buffer and field capacity arithmetic;
//! - a face-indexed transcription of [`crate::mesh::Mesh::level_set`];
//! - a topology compactor that exactly preserves the current weld direction
//!   wherever every joining edge has degree at most two, and declares one
//!   deterministic local rule for exceptional junctions.
//!
//! Keeping the reference here, rather than growing a second implementation
//! inside a shader test, gives the GPU path a byte-and-order oracle without
//! changing one puppet pixel before its visual evidence is approved.

use core::mem::size_of;

use aether_math::Vec3;

use crate::deform::{BONE_LIMIT, Skin};
use crate::mesh::Mesh;

/// Local triangle edges in face-corner order.
///
/// The index into this table is the `local_edge` carried by an
/// [`EdgeIncident`] and the corresponding lane in
/// [`CanonicalTopology::face_edges`].
const LOCAL_EDGES: [(usize, usize); 3] = [(0, 1), (1, 2), (2, 0)];

/// A storage-buffer vertex, aligned exactly as two WGSL `vec3<f32>` values
/// followed by eight scalar weights.
///
/// The explicit pads make the Rust record agree with storage-address-space
/// `vec3` alignment. Weights remain in bone-table order and at authored
/// `f32` precision; the compute shader will apply the same threshold and
/// accumulation order as `Skin::pose_surface`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResidentVertex {
    pub position: [f32; 3],
    position_pad: f32,
    pub normal: [f32; 3],
    normal_pad: f32,
    pub weights: [f32; BONE_LIMIT],
}

impl ResidentVertex {
    /// Pack the rest mesh and its optional rig without changing either.
    pub fn pack(mesh: &Mesh, skin: Option<&Skin>) -> Vec<Self> {
        mesh.positions
            .iter()
            .zip(&mesh.normals)
            .enumerate()
            .map(|(vertex, (&position, &normal))| Self {
                position: position.to_array(),
                position_pad: 0.0,
                normal: normal.to_array(),
                normal_pad: 0.0,
                weights: skin.map_or([0.0; BONE_LIMIT], |skin| skin.dense_weights(vertex)),
            })
            .collect()
    }
}

/// One canonical undirected mesh edge.
///
/// `vertices` is ascending. Its incident run is
/// `incidents[first_incident..first_incident + incident_count]`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CanonicalEdge {
    pub vertices: [u32; 2],
    pub first_incident: u32,
    pub incident_count: u32,
}

/// One face/local-edge use of a canonical edge.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct EdgeIncident {
    pub face: u32,
    pub local_edge: u32,
}

/// Load-time topology shared by every eye and pose of one subject.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CanonicalTopology {
    pub edges: Vec<CanonicalEdge>,
    pub incidents: Vec<EdgeIncident>,
    /// Canonical edge ids for each face's three [`LOCAL_EDGES`].
    pub face_edges: Vec<[u32; 3]>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TopologyError {
    CountExceedsU32 { what: &'static str, count: usize },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct UnsortedIncident {
    vertices: [u32; 2],
    incident: EdgeIncident,
}

impl CanonicalTopology {
    pub fn of(mesh: &Mesh) -> Result<Self, TopologyError> {
        Self::from_faces(&mesh.faces)
    }

    fn from_faces(faces: &[[u32; 3]]) -> Result<Self, TopologyError> {
        if faces.len() > u32::MAX as usize {
            return Err(TopologyError::CountExceedsU32 { what: "faces", count: faces.len() });
        }

        let incident_count = faces
            .len()
            .checked_mul(3)
            .ok_or(TopologyError::CountExceedsU32 { what: "edge incidents", count: usize::MAX })?;
        if incident_count > u32::MAX as usize {
            return Err(TopologyError::CountExceedsU32 { what: "edge incidents", count: incident_count });
        }

        let mut unsorted = Vec::with_capacity(incident_count);
        for (face, corners) in faces.iter().enumerate() {
            for (local_edge, &(a, b)) in LOCAL_EDGES.iter().enumerate() {
                let (a, b) = (corners[a], corners[b]);
                unsorted.push(UnsortedIncident {
                    vertices: [a.min(b), a.max(b)],
                    incident: EdgeIncident { face: face as u32, local_edge: local_edge as u32 },
                });
            }
        }
        unsorted.sort_unstable();

        let mut edges = Vec::new();
        let mut incidents = Vec::with_capacity(unsorted.len());
        let mut face_edges = vec![[0; 3]; faces.len()];
        let mut cursor = 0usize;
        while cursor < unsorted.len() {
            let first = cursor;
            let vertices = unsorted[first].vertices;
            while cursor < unsorted.len() && unsorted[cursor].vertices == vertices {
                cursor += 1;
            }

            let edge = u32::try_from(edges.len())
                .map_err(|_| TopologyError::CountExceedsU32 { what: "canonical edges", count: edges.len() })?;
            let first_incident = incidents.len() as u32;
            for held in &unsorted[first..cursor] {
                incidents.push(held.incident);
                face_edges[held.incident.face as usize][held.incident.local_edge as usize] = edge;
            }
            edges.push(CanonicalEdge { vertices, first_incident, incident_count: (cursor - first) as u32 });
        }

        Ok(Self { edges, incidents, face_edges })
    }

    pub fn incidents(&self, edge: u32) -> &[EdgeIncident] {
        let edge = self.edges[edge as usize];
        let first = edge.first_incident as usize;

        &self.incidents[first..first + edge.incident_count as usize]
    }
}

/// Checked worst-case counts for one subject's silhouette derivation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DerivationCapacity {
    pub vertices: u32,
    pub faces: u32,
    pub edges: u32,
    pub edge_incidents: u32,
    pub max_segments: u32,
    pub max_curves: u32,
    pub max_points: u32,
    pub max_ribbon_vertices: u32,
    pub max_field_texels: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapacityError {
    CountExceedsU32 { what: &'static str, count: usize },
    CountOverflow { what: &'static str },
    FieldOverflow { needed: u64, capacity: u64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FieldUse {
    pub needed: u64,
    pub capacity: u64,
}

impl DerivationCapacity {
    pub fn for_counts(vertices: usize, faces: usize, edges: usize) -> Result<Self, CapacityError> {
        let vertices = count("vertices", vertices)?;
        let faces = count("faces", faces)?;
        let edges = count("edges", edges)?;
        let edge_incidents = multiply("edge incidents", faces, 3)?;
        let max_points = multiply("points", faces, 2)?;
        let max_ribbon_vertices = multiply("ribbon vertices", faces, 6)?;
        let max_field_texels = max_points
            .checked_add(faces)
            .and_then(|texels| texels.checked_add(1))
            .ok_or(CapacityError::CountOverflow { what: "field texels" })?;

        Ok(Self {
            vertices,
            faces,
            edges,
            edge_incidents,
            max_segments: faces,
            max_curves: faces,
            max_points,
            max_ribbon_vertices,
            max_field_texels,
        })
    }

    pub fn of(mesh: &Mesh, topology: &CanonicalTopology) -> Result<Self, CapacityError> {
        Self::for_counts(mesh.positions.len(), mesh.faces.len(), topology.edges.len())
    }

    /// Worst-case bytes of the load-time records fixed in this module.
    pub fn resident_bytes(self) -> u64 {
        u64::from(self.vertices) * size_of::<ResidentVertex>() as u64
            + u64::from(self.edges) * size_of::<CanonicalEdge>() as u64
            + u64::from(self.edge_incidents) * size_of::<EdgeIncident>() as u64
            + u64::from(self.faces) * size_of::<[u32; 3]>() as u64
    }

    pub fn field_use(self, width: u32, height: u32) -> Result<FieldUse, CapacityError> {
        field_use(self.max_points, self.max_curves, width, height)
    }
}

pub fn field_use(points: u32, curves: u32, width: u32, height: u32) -> Result<FieldUse, CapacityError> {
    let needed = u64::from(points) + u64::from(curves) + 1;
    let capacity = u64::from(width) * u64::from(height);
    if needed > capacity {
        return Err(CapacityError::FieldOverflow { needed, capacity });
    }

    Ok(FieldUse { needed, capacity })
}

fn count(what: &'static str, value: usize) -> Result<u32, CapacityError> {
    u32::try_from(value).map_err(|_| CapacityError::CountExceedsU32 { what, count: value })
}

fn multiply(what: &'static str, value: u32, factor: u32) -> Result<u32, CapacityError> {
    value.checked_mul(factor).ok_or(CapacityError::CountOverflow { what })
}

/// One face-local level-set crossing, in the storage-neutral shape both a
/// CPU oracle and a future compute output can compare exactly.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MarchPoint {
    pub position: Vec3,
    pub normal: Vec3,
    pub face: u32,
    pub u: f32,
    pub v: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MarchEndpoint {
    pub edge: u32,
    pub point: MarchPoint,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FaceSegment {
    pub endpoints: [MarchEndpoint; 2],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarchError {
    ValueCount { values: usize, vertices: usize },
    TopologyFaceCount { topology: usize, mesh: usize },
}

/// March one level set without compacting away inactive faces.
pub fn march_faces(
    mesh: &Mesh,
    topology: &CanonicalTopology,
    values: &[f32],
    iso: f32,
) -> Result<Vec<Option<FaceSegment>>, MarchError> {
    if values.len() != mesh.positions.len() {
        return Err(MarchError::ValueCount { values: values.len(), vertices: mesh.positions.len() });
    }
    if topology.face_edges.len() != mesh.faces.len() {
        return Err(MarchError::TopologyFaceCount { topology: topology.face_edges.len(), mesh: mesh.faces.len() });
    }

    Ok(mesh
        .faces
        .iter()
        .enumerate()
        .map(|(face, &corners)| {
            let above = corners.map(|corner| values[corner as usize] >= iso);
            if above[0] == above[1] && above[1] == above[2] {
                return None;
            }

            let odd = match above {
                [x, y, _] if x == y => 2,
                [x, _, z] if x == z => 1,
                _ => 0,
            };
            let (a, b) = ((odd + 1) % 3, (odd + 2) % 3);
            let endpoint = |other| MarchEndpoint {
                edge: topology.face_edges[face][local_edge(odd, other)],
                point: crossing(mesh, face, odd, other, values, iso),
            };

            Some(FaceSegment { endpoints: [endpoint(a), endpoint(b)] })
        })
        .collect())
}

fn local_edge(a: usize, b: usize) -> usize {
    match (a.min(b), a.max(b)) {
        (0, 1) => 0,
        (1, 2) => 1,
        (0, 2) => 2,
        _ => unreachable!("triangle corners are in 0..3"),
    }
}

fn crossing(mesh: &Mesh, face: usize, i: usize, j: usize, values: &[f32], iso: f32) -> MarchPoint {
    let corners = mesh.faces[face];
    let (lo_corner, hi_corner) = if corners[i] < corners[j] {
        (i, j)
    } else {
        (j, i)
    };
    let (lo, hi) = (corners[lo_corner] as usize, corners[hi_corner] as usize);
    let span = values[hi] - values[lo];
    let t = if span.abs() < 1e-20 {
        0.5
    } else {
        (iso - values[lo]) / span
    };
    let mut shares = [0.0; 3];
    shares[lo_corner] = 1.0 - t;
    shares[hi_corner] = t;

    MarchPoint {
        position: mesh.positions[lo].lerp(mesh.positions[hi], t),
        normal: mesh.normals[lo].lerp(mesh.normals[hi], t).normalize_or(mesh.normals[lo]),
        face: face as u32,
        u: shares[1],
        v: shares[2],
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct EndpointRef {
    face: u32,
    end: u32,
}

/// One compacted curve, retaining the face trail for deterministic identity
/// and whether the declared exceptional-junction rule touched it.
#[derive(Clone, Debug, PartialEq)]
pub struct CompactedCurve {
    pub points: Vec<MarchPoint>,
    pub faces: Vec<u32>,
    pub exceptional: bool,
}

/// Compact face-indexed segments into stable curves.
///
/// Degree-one and degree-two edges are exactly the current weld's graph.
/// An edge with more than two active endpoints is exceptional: endpoints
/// are considered in `(face, end)` order and each takes the first later,
/// still-unpaired endpoint belonging to a different segment. This is a
/// local, deterministic pen-lift rule; callers must preserve the returned
/// `exceptional` bit for the visual evidence gate rather than treating the
/// changed pairing as ordinary parity.
pub fn compact(segments: &[Option<FaceSegment>], edge_count: usize) -> Vec<CompactedCurve> {
    let mut at_edge = vec![Vec::<EndpointRef>::new(); edge_count];
    for (face, segment) in segments.iter().enumerate() {
        let Some(segment) = segment else {
            continue;
        };
        for end in 0..2 {
            at_edge[segment.endpoints[end].edge as usize].push(EndpointRef { face: face as u32, end: end as u32 });
        }
    }

    let mut joined = vec![[None; 2]; segments.len()];
    let mut exceptional = vec![false; segments.len()];
    for endpoints in &mut at_edge {
        endpoints.sort_unstable();
        if endpoints.len() > 2 {
            for endpoint in endpoints.iter().copied() {
                exceptional[endpoint.face as usize] = true;
            }
        }

        let mut paired = vec![false; endpoints.len()];
        for at in 0..endpoints.len() {
            if paired[at] {
                continue;
            }
            let Some(next) =
                (at + 1..endpoints.len()).find(|&next| !paired[next] && endpoints[next].face != endpoints[at].face)
            else {
                continue;
            };
            paired[at] = true;
            paired[next] = true;
            let (a, b) = (endpoints[at], endpoints[next]);
            joined[a.face as usize][a.end as usize] = Some(b);
            joined[b.face as usize][b.end as usize] = Some(a);
        }
    }

    let mut used = vec![false; segments.len()];
    let mut curves = Vec::new();
    for start in 0..segments.len() {
        let Some(segment) = segments[start] else {
            continue;
        };
        if used[start] {
            continue;
        }
        used[start] = true;

        let mut points = vec![segment.endpoints[0].point, segment.endpoints[1].point];
        let mut faces = vec![start as u32];
        let mut curve_exceptional = exceptional[start];
        grow(
            segments,
            &joined,
            &exceptional,
            &mut used,
            &mut points,
            &mut faces,
            &mut curve_exceptional,
            EndpointRef { face: start as u32, end: 1 },
        );
        points.reverse();
        faces.reverse();
        grow(
            segments,
            &joined,
            &exceptional,
            &mut used,
            &mut points,
            &mut faces,
            &mut curve_exceptional,
            EndpointRef { face: start as u32, end: 0 },
        );

        curves.push(CompactedCurve { points, faces, exceptional: curve_exceptional });
    }

    curves
}

#[allow(clippy::too_many_arguments, reason = "the mutable curve assembly is one indivisible traversal state")]
fn grow(
    segments: &[Option<FaceSegment>],
    joined: &[[Option<EndpointRef>; 2]],
    exceptional: &[bool],
    used: &mut [bool],
    points: &mut Vec<MarchPoint>,
    faces: &mut Vec<u32>,
    curve_exceptional: &mut bool,
    mut tail: EndpointRef,
) {
    loop {
        let Some(next) = joined[tail.face as usize][tail.end as usize] else {
            return;
        };
        if used[next.face as usize] {
            return;
        }
        used[next.face as usize] = true;
        *curve_exceptional |= exceptional[next.face as usize];
        faces.push(next.face);

        let far = (next.end ^ 1) as usize;
        points.push(
            segments[next.face as usize].expect("a joined endpoint belongs to an active segment").endpoints[far].point,
        );
        tail = EndpointRef { face: next.face, end: far as u32 };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::deform;
    use crate::feature::SurfacePoint;
    use crate::weld;

    fn mesh(obj: &[u8]) -> Mesh {
        Mesh::from_obj_bytes(obj, 0).expect("fixture is a triangle mesh")
    }

    fn point(x: f32, y: f32) -> MarchPoint {
        MarchPoint { position: Vec3::new(x, y, 0.0), normal: Vec3::Z, face: 0, u: 0.0, v: 0.0 }
    }

    fn segment(a: MarchPoint, a_edge: u32, b: MarchPoint, b_edge: u32) -> FaceSegment {
        FaceSegment { endpoints: [MarchEndpoint { edge: a_edge, point: a }, MarchEndpoint { edge: b_edge, point: b }] }
    }

    fn positions(curves: &[CompactedCurve]) -> Vec<Vec<Vec3>> {
        curves.iter().map(|curve| curve.points.iter().map(|point| point.position).collect()).collect()
    }

    #[test]
    fn storage_records_have_the_wgsl_layout_their_contract_declares() {
        assert_eq!(size_of::<ResidentVertex>(), 64);
        assert_eq!(size_of::<CanonicalEdge>(), 16);
        assert_eq!(size_of::<EdgeIncident>(), 8);
    }

    #[test]
    fn canonical_edges_are_unique_sorted_and_carry_ordered_incidents() {
        let faces = [[2, 0, 1], [2, 3, 0], [2, 0, 4]];
        let topology = CanonicalTopology::from_faces(&faces).expect("small topology fits");

        assert_eq!(
            topology.edges.iter().map(|edge| edge.vertices).collect::<Vec<_>>(),
            [[0, 1], [0, 2], [0, 3], [0, 4], [1, 2], [2, 3], [2, 4],]
        );
        let shared = topology.edges.iter().position(|edge| edge.vertices == [0, 2]).expect("shared edge") as u32;
        assert_eq!(
            topology.incidents(shared),
            [
                EdgeIncident { face: 0, local_edge: 0 },
                EdgeIncident { face: 1, local_edge: 2 },
                EdgeIncident { face: 2, local_edge: 0 },
            ]
        );
        for (face, edges) in topology.face_edges.iter().enumerate() {
            for (local, &edge) in edges.iter().enumerate() {
                assert!(
                    topology.incidents(edge).contains(&EdgeIncident { face: face as u32, local_edge: local as u32 }),
                    "face {face} edge {local} must point back to its incident run"
                );
            }
        }
    }

    #[test]
    fn resident_vertices_keep_dense_bone_order_and_zero_fill_the_table() {
        let mesh = mesh(b"v 0 0 0\nv 1 0 0\nv 0 1 0\nf 1 2 3\n");
        let rows = [0.2, 0.7, 0.1, 0.6, 0.1, 0.3, 0.05, 0.15, 0.8];
        let weights = deform::npy(&rows, (3, 3));
        let skin = Skin::parse(&weights, "bones chest head jaw\n", 3).expect("three-bone fixture");

        let packed = ResidentVertex::pack(&mesh, Some(&skin));
        assert_eq!(packed[0].weights, [0.2, 0.7, 0.1, 0.0, 0.0, 0.0, 0.0, 0.0]);
        assert_eq!(packed[1].weights, [0.6, 0.1, 0.3, 0.0, 0.0, 0.0, 0.0, 0.0]);
        assert_eq!(packed[2].weights, [0.05, 0.15, 0.8, 0.0, 0.0, 0.0, 0.0, 0.0]);
        assert_eq!(
            ResidentVertex::pack(&mesh, None).iter().map(|vertex| vertex.weights).collect::<Vec<_>>(),
            [[0.0; BONE_LIMIT], [0.0; BONE_LIMIT], [0.0; BONE_LIMIT],]
        );
    }

    #[test]
    fn capacity_arithmetic_names_every_overflow_and_field_shortfall() {
        let capacity = DerivationCapacity::for_counts(214_532, 434_341, 651_512).expect("shipped scale fits");
        assert_eq!(capacity.max_segments, 434_341);
        assert_eq!(capacity.max_points, 868_682);
        assert_eq!(capacity.max_curves, 434_341);
        assert_eq!(capacity.max_ribbon_vertices, 2_606_046);
        assert_eq!(capacity.max_field_texels, 1_303_024);
        assert_eq!(capacity.resident_bytes(), 13_730_048 + 10_424_192 + 10_424_184 + 5_212_092);

        assert_eq!(
            DerivationCapacity::for_counts(0, u32::MAX as usize / 6 + 1, 0),
            Err(CapacityError::CountOverflow { what: "ribbon vertices" })
        );
        if usize::BITS > 32 {
            assert_eq!(
                DerivationCapacity::for_counts(u32::MAX as usize + 1, 0, 0),
                Err(CapacityError::CountExceedsU32 { what: "vertices", count: u32::MAX as usize + 1 })
            );
        }
        assert_eq!(field_use(200, 20, 10, 20), Err(CapacityError::FieldOverflow { needed: 221, capacity: 200 }));
        assert_eq!(field_use(179, 20, 10, 20), Ok(FieldUse { needed: 200, capacity: 200 }));
    }

    #[test]
    fn face_indexed_march_is_the_level_set_oracle_without_losing_holes() {
        let mesh = mesh(b"v -1 0 0\nv 0 1 0\nv 0 -1 0\nv 1 1 0\nv 1 -1 0\nf 1 2 3\nf 2 4 5\nf 2 5 3\n");
        let topology = CanonicalTopology::of(&mesh).expect("small topology");
        let values = [-1.0, -0.5, -0.25, 1.0, 0.75];
        let indexed = march_faces(&mesh, &topology, &values, 0.0).expect("values match the mesh");
        let dense = mesh.level_set(&values, &[], 0.0);

        assert_eq!(indexed.len(), mesh.faces.len());
        assert!(indexed[0].is_none(), "the all-negative face remains a hole");
        let indexed_points = indexed
            .iter()
            .flatten()
            .flat_map(|segment| segment.endpoints.map(|endpoint| endpoint.point))
            .collect::<Vec<_>>();
        let dense_points = dense.into_iter().flatten().collect::<Vec<_>>();
        assert_eq!(indexed_points.len(), dense_points.len());
        for (indexed, dense) in indexed_points.iter().zip(dense_points) {
            assert_eq!(indexed.position.to_array().map(f32::to_bits), dense.pos.to_array().map(f32::to_bits));
            assert_eq!(indexed.normal.to_array().map(f32::to_bits), dense.normal.to_array().map(f32::to_bits));
            assert_eq!(indexed.face, dense.at.face);
            assert_eq!(indexed.u.to_bits(), dense.at.u.to_bits());
            assert_eq!(indexed.v.to_bits(), dense.at.v.to_bits());
        }

        let exact_iso = [-1.0, 0.0, -0.25, 1.0, 0.75];
        let indexed = march_faces(&mesh, &topology, &exact_iso, 0.0).expect("exact-iso values match the mesh");
        let dense = mesh.level_set(&exact_iso, &[], 0.0);
        let indexed_points = indexed.iter().flatten().flat_map(|segment| segment.endpoints).collect::<Vec<_>>();
        let dense_points = dense.into_iter().flatten().collect::<Vec<_>>();
        assert_eq!(indexed_points.len(), dense_points.len());
        for (indexed, dense) in indexed_points.iter().zip(dense_points) {
            assert_eq!(indexed.point.position.to_array().map(f32::to_bits), dense.pos.to_array().map(f32::to_bits));
            assert_eq!(indexed.point.normal.to_array().map(f32::to_bits), dense.normal.to_array().map(f32::to_bits));
            assert_eq!(indexed.point.face, dense.at.face);
            assert_eq!(indexed.point.u.to_bits(), dense.at.u.to_bits());
            assert_eq!(indexed.point.v.to_bits(), dense.at.v.to_bits());
        }

        for edge in 0..topology.edges.len() as u32 {
            let crossings = indexed_points.iter().filter(|endpoint| endpoint.edge == edge).collect::<Vec<_>>();
            if let [a, b] = crossings.as_slice() {
                assert_eq!(
                    a.point.position.to_array().map(f32::to_bits),
                    b.point.position.to_array().map(f32::to_bits),
                    "the two face windings must spell a shared-edge crossing identically"
                );
                assert_eq!(a.point.normal.to_array().map(f32::to_bits), b.point.normal.to_array().map(f32::to_bits));
            }
        }

        assert_eq!(
            march_faces(&mesh, &topology, &values[..4], 0.0),
            Err(MarchError::ValueCount { values: 4, vertices: 5 })
        );
    }

    #[test]
    fn degree_two_open_components_match_weld_order_exactly() {
        let (a, b, c, d, x, y) =
            (point(0.0, 0.0), point(1.0, 0.0), point(2.0, 0.0), point(3.0, 0.0), point(0.0, 2.0), point(1.0, 2.0));
        let segments = [
            Some(segment(b, 1, c, 2)),
            None,
            Some(segment(a, 0, b, 1)),
            Some(segment(c, 2, d, 3)),
            Some(segment(x, 4, y, 5)),
        ];
        let compacted = compact(&segments, 6);

        assert_eq!(
            positions(&compacted),
            [vec![d.position, c.position, b.position, a.position], vec![y.position, x.position],]
        );
        assert_eq!(compacted.iter().map(|curve| curve.faces.clone()).collect::<Vec<_>>(), [vec![3, 0, 2], vec![4],]);
        assert!(compacted.iter().all(|curve| !curve.exceptional));

        let welded = weld::weld(
            segments
                .iter()
                .flatten()
                .map(|segment| {
                    segment
                        .endpoints
                        .map(|endpoint| SurfacePoint::on_surface(endpoint.point.position, endpoint.point.normal))
                })
                .collect(),
        );
        assert_eq!(
            positions(&compacted),
            welded.iter().map(|line| line.iter().map(|point| point.pos).collect()).collect::<Vec<Vec<Vec3>>>()
        );
    }

    #[test]
    fn degree_two_cycles_close_once_and_match_weld_direction() {
        let (a, b, c) = (point(0.0, 0.0), point(1.0, 0.0), point(0.5, 1.0));
        let segments = [Some(segment(a, 0, b, 1)), Some(segment(b, 1, c, 2)), Some(segment(c, 2, a, 0))];
        let compacted = compact(&segments, 3);

        assert_eq!(positions(&compacted), [vec![a.position, c.position, b.position, a.position]]);
        assert_eq!(compacted[0].faces, [2, 1, 0]);
        assert!(!compacted[0].exceptional);

        let welded = weld::weld(
            segments
                .iter()
                .flatten()
                .map(|segment| {
                    segment
                        .endpoints
                        .map(|endpoint| SurfacePoint::on_surface(endpoint.point.position, endpoint.point.normal))
                })
                .collect(),
        );
        assert_eq!(
            positions(&compacted),
            welded.iter().map(|line| line.iter().map(|point| point.pos).collect()).collect::<Vec<Vec<Vec3>>>()
        );
    }

    #[test]
    fn exceptional_junctions_pair_by_face_then_endpoint_and_announce_every_curve() {
        let joint = point(0.0, 0.0);
        let (left, right, down, up) = (point(-1.0, 0.0), point(1.0, 0.0), point(0.0, -1.0), point(0.0, 1.0));
        let segments = [
            Some(segment(left, 0, joint, 9)),
            Some(segment(joint, 9, right, 1)),
            Some(segment(down, 2, joint, 9)),
            Some(segment(joint, 9, up, 3)),
        ];

        let compacted = compact(&segments, 10);

        assert_eq!(
            positions(&compacted),
            [vec![right.position, joint.position, left.position], vec![up.position, joint.position, down.position],]
        );
        assert_eq!(compacted.iter().map(|curve| curve.faces.clone()).collect::<Vec<_>>(), [vec![1, 0], vec![3, 2],]);
        assert!(compacted.iter().all(|curve| curve.exceptional));
    }
}
