//! Opt-in resident GPU silhouette derivation.
//!
//! The CPU silhouette remains the default and the parity oracle. This layer
//! is mounted only by the diagnostic selector in [`crate::GpuSilhouetteMode`]:
//! it poses and classifies resident vertices, marches faces, pairs canonical
//! edge endpoints, compacts the result in the oracle's order, and consumes
//! the resulting vertex/index/control buffers through indexed-indirect draw.
//! A posed-subject depth prepass feeds centreline visibility classification
//! during compaction, so hidden segments leave before the depth-free ribbon
//! draw and visible segments keep both rails of the established pen.

use std::mem::{self, size_of};

use aether_math::{Mat4, Rgba, Vec3};
use aether_render::{
    ComputeBufferBinding, ComputePass, CreateGeometry, CreateTexture, DestroyTexture, DrawMaterialTextured, DrawPass,
    GeometryBuffer, GeometrySlotSpec, InputSlot, MaterialRect, MaterialTexturedRect, OutputSlot, PassLoad, PassStage,
    ProgramDestroy, ProgramDispatch, ProgramPass, ProgramRegister, QuadBlend, SlotExtent, SlotSpec, StorageAccess,
    TextureFormat, TextureSampling, TextureUsage, UpdateGeometry, VertexAttribute, VertexFormat,
};

use crate::deform::{BONE_LIMIT, Skin};
use crate::easel::{View, wash_canvas};
use crate::mesh::Mesh;
use crate::silhouette::{CanonicalTopology, DerivationCapacity, ResidentVertex};

const MODULE: &str = include_str!("gpu_silhouette.wgsl");
const PROGRAM_GEOMETRIES: usize = 4;
const SUPERSAMPLE: u32 = 2;
const STANDOFF: f32 = 0.045;
const DEPTH_FLOOR: f32 = 0.25;

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum Resident {
    #[default]
    Absent,
    Creating,
    Live(u32),
}

impl Resident {
    fn id(self) -> Option<u32> {
        match self {
            Self::Live(id) => Some(id),
            Self::Absent | Self::Creating => None,
        }
    }
}

#[derive(Default)]
struct Ordered {
    ids: Vec<u32>,
    asking: bool,
}

impl Ordered {
    fn ready(&self) -> bool {
        self.ids.len() == PROGRAM_GEOMETRIES
    }

    fn answered(&mut self, id: u32) {
        self.ids.push(id);
        if self.ready() {
            self.asking = false;
        }
    }

    fn clear(&mut self) {
        self.ids.clear();
        self.asking = false;
    }
}

#[derive(Clone, Copy)]
struct Standing {
    view_proj: Mat4,
    eye: Vec3,
    bones: [f32; BONE_LIMIT * 12],
}

/// State for the diagnostic candidate layer.
#[derive(Default)]
#[allow(clippy::struct_excessive_bools, reason = "orthogonal selection, evidence, cadence, and refusal state")]
pub struct GpuSilhouette {
    selected: bool,
    exceptional_overlay: bool,
    window: Option<(u32, u32)>,
    canvas: Option<(u32, u32)>,
    sized: Option<(u32, u32)>,
    counts: Option<[u32; 3]>,
    surface_bias: f32,
    registering_counts: Option<[u32; 3]>,
    registered_counts: Option<[u32; 3]>,
    program: Resident,
    stale_programs: Vec<u32>,
    texture: Resident,
    geometries: Ordered,
    staged: Option<Vec<CreateGeometry>>,
    standing: Option<Standing>,
    revision: u64,
    dispatched: Option<u64>,
    drawn: bool,
    disabled: bool,
}

impl GpuSilhouette {
    /// Select the diagnostic candidate and, independently, its exceptional-
    /// junction-only overlay. Returns whether the visible selection changed.
    pub fn select(&mut self, enabled: bool, exceptional_overlay: bool) -> bool {
        let changed = self.selected != enabled || self.exceptional_overlay != exceptional_overlay;
        self.selected = enabled;
        self.exceptional_overlay = exceptional_overlay;
        if changed {
            self.revision = self.revision.wrapping_add(1);
            self.drawn = false;
        }

        changed
    }

    pub fn selected(&self) -> bool {
        self.selected
    }

    pub fn resized(&mut self, width: u32, height: u32) {
        self.window = Some((width, height));
        let canvas = wash_canvas(width, height);
        let canvas = (canvas.width as u32, canvas.height as u32);
        if self.canvas != Some(canvas) {
            self.canvas = Some(canvas);
            self.revision = self.revision.wrapping_add(1);
        }
    }

    /// Stage all load-time resident resources. The allocations are the
    /// checked worst-case capacities declared by the CPU oracle.
    pub fn subject_changed(&mut self, mesh: &Mesh, skin: Option<&Skin>) -> Result<(), String> {
        let topology =
            CanonicalTopology::of(mesh).map_err(|error| format!("silhouette topology refused: {error:?}"))?;
        let capacity = DerivationCapacity::of(mesh, &topology)
            .map_err(|error| format!("silhouette capacity refused: {error:?}"))?;
        let counts = [capacity.vertices, capacity.faces, capacity.edges];
        if self.registered_counts.is_some_and(|registered| registered != counts) {
            if let Resident::Live(program_id) = self.program {
                self.stale_programs.push(program_id);
            }
            self.program = Resident::Absent;
            self.registered_counts = None;
        }

        self.counts = Some(counts);
        self.surface_bias = mesh.surface_bias();
        self.staged = Some(geometries(mesh, skin, &topology, capacity)?);
        self.revision = self.revision.wrapping_add(1);
        self.dispatched = None;
        self.drawn = false;

        Ok(())
    }

    pub fn solve(&mut self, view_proj: Mat4, eye: Vec3, bones: &[f32; BONE_LIMIT * 12]) {
        self.standing = Some(Standing { view_proj, eye, bones: *bones });
        self.revision = self.revision.wrapping_add(1);
    }

    pub fn take_program_destroys(&mut self) -> Vec<ProgramDestroy> {
        self.stale_programs.drain(..).map(|program_id| ProgramDestroy { program_id }).collect()
    }

    pub fn take_registers(&mut self) -> Vec<ProgramRegister> {
        if !self.selected || self.disabled || self.program != Resident::Absent || self.standing.is_none() {
            return Vec::new();
        }
        let Some(counts) = self.counts else {
            return Vec::new();
        };
        self.program = Resident::Creating;
        self.registering_counts = Some(counts);

        vec![program(counts)]
    }

    pub fn registered(&mut self, result: Result<u32, ()>) {
        let registering = self.registering_counts.take();
        match result {
            Ok(program_id) if registering == self.counts => {
                self.program = Resident::Live(program_id);
                self.registered_counts = registering;
            }
            Ok(program_id) => {
                self.stale_programs.push(program_id);
                self.program = Resident::Absent;
            }
            Err(()) => self.disabled = true,
        }
    }

    pub fn take_destroys(&mut self) -> Vec<DestroyTexture> {
        if self.texture == Resident::Creating || self.sized == self.canvas || self.sized.is_none() {
            return Vec::new();
        }
        let Resident::Live(texture_id) = mem::take(&mut self.texture) else {
            return Vec::new();
        };
        self.sized = None;
        self.dispatched = None;
        self.drawn = false;

        vec![DestroyTexture { texture_id }]
    }

    pub fn take_creates(&mut self) -> Vec<CreateTexture> {
        if !self.selected || self.disabled || self.program.id().is_none() || self.texture != Resident::Absent {
            return Vec::new();
        }
        let Some((width, height)) = self.canvas else {
            return Vec::new();
        };
        self.texture = Resident::Creating;
        self.sized = Some((width, height));

        vec![CreateTexture {
            width: width.saturating_mul(SUPERSAMPLE),
            height: height.saturating_mul(SUPERSAMPLE),
            format: TextureFormat::Rgba8,
            sampling: TextureSampling::Linear,
            usage: TextureUsage::Writable,
            pixels: Vec::new(),
        }]
    }

    pub fn created(&mut self, result: Result<u32, ()>) {
        if let Ok(texture_id) = result {
            self.texture = Resident::Live(texture_id);
        } else {
            self.texture = Resident::Absent;
            self.disabled = true;
        }
    }

    pub fn take_geometry_creates(&mut self) -> Vec<CreateGeometry> {
        if !self.selected || self.disabled || self.geometries.asking || self.geometries.ready() {
            return Vec::new();
        }
        let Some(staged) = self.staged.take() else {
            return Vec::new();
        };
        self.geometries.asking = true;

        staged
    }

    pub fn geometry_created(&mut self, result: Result<u32, ()>) {
        if let Ok(geometry_id) = result {
            self.geometries.answered(geometry_id);
        } else {
            self.geometries.clear();
            self.disabled = true;
        }
    }

    pub fn take_geometry_updates(&mut self) -> Vec<UpdateGeometry> {
        if !self.geometries.ready() {
            return Vec::new();
        }
        let Some(staged) = self.staged.take() else {
            return Vec::new();
        };

        self.geometries
            .ids
            .iter()
            .copied()
            .zip(staged)
            .map(|(geometry_id, geometry)| UpdateGeometry {
                geometry_id,
                vertices: geometry.vertices,
                indices: geometry.indices,
            })
            .collect()
    }

    pub fn take_dispatches(&mut self) -> Vec<ProgramDispatch> {
        if !self.selected || self.disabled || self.dispatched == Some(self.revision) || !self.geometries.ready() {
            return Vec::new();
        }
        let (Some(program_id), Some(texture_id), Some(counts), Some(standing)) =
            (self.program.id(), self.texture.id(), self.counts, self.standing.as_ref())
        else {
            return Vec::new();
        };

        self.dispatched = Some(self.revision);
        self.drawn = true;
        vec![ProgramDispatch {
            program_id,
            bindings: vec![texture_id],
            geometries: self.geometries.ids.clone(),
            uniforms: uniforms(standing, counts, self.surface_bias, self.exceptional_overlay),
        }]
    }

    /// Composite the candidate after the ordinary ink. The transparent
    /// candidate sheet contains only the GPU-derived silhouette.
    pub fn draw(&self, view: &View, subject_radius: f32) -> Option<DrawMaterialTextured> {
        if !self.selected || !self.drawn {
            return None;
        }
        let texture_id = self.texture.id()?;
        let forward = (view.target - view.eye).normalize_or(Vec3::new(0.0, 0.0, -1.0));
        let depth = ((view.target - view.eye).length() - subject_radius - STANDOFF).max(DEPTH_FLOOR);
        let centre = view.eye + forward * depth;
        let right = forward.cross(Vec3::new(0.0, 1.0, 0.0)).normalize_or(Vec3::new(1.0, 0.0, 0.0));
        let up = right.cross(forward);
        let half_height = depth * (view.field_of_view * 0.5).tan();
        let half_width = half_height * view.aspect;
        let origin = centre - right * half_width - up * half_height;

        Some(DrawMaterialTextured {
            texture_id,
            blend: QuadBlend::Premultiplied,
            rects: vec![MaterialTexturedRect {
                rect: MaterialRect {
                    x: origin.x,
                    y: origin.y,
                    z: origin.z,
                    width: half_width * 2.0,
                    height: half_height * 2.0,
                    right: right.to_array(),
                    up: up.to_array(),
                },
                u0: 0.0,
                v0: 1.0,
                u1: 1.0,
                v1: 0.0,
                tint: Rgba::new(1.0, 1.0, 1.0, 1.0),
            }],
        })
    }
}

fn raw_slot() -> GeometrySlotSpec {
    GeometrySlotSpec { layout: vec![VertexAttribute { location: 0, format: VertexFormat::Float32 }] }
}

fn source_slot() -> GeometrySlotSpec {
    let mut layout = vec![
        VertexAttribute { location: 0, format: VertexFormat::Float32x3 },
        VertexAttribute { location: 1, format: VertexFormat::Float32 },
        VertexAttribute { location: 2, format: VertexFormat::Float32x3 },
        VertexAttribute { location: 3, format: VertexFormat::Float32 },
    ];
    layout.extend((4..12).map(|location| VertexAttribute { location, format: VertexFormat::Float32 }));

    GeometrySlotSpec { layout }
}

fn output_slot() -> GeometrySlotSpec {
    GeometrySlotSpec {
        layout: vec![
            VertexAttribute { location: 0, format: VertexFormat::Float32x3 },
            VertexAttribute { location: 1, format: VertexFormat::Float32 },
        ],
    }
}

fn compute(entry_point: &str, buffers: Vec<ComputeBufferBinding>, workgroups: u32) -> ProgramPass {
    ProgramPass {
        stage: PassStage::Compute(ComputePass { buffers, workgroups: [workgroups.max(1), 1, 1] }),
        entry_point: entry_point.to_owned(),
        inputs: Vec::new(),
        output: OutputSlot::None,
        uniform_offset: 0,
        uniform_length: 480,
        repeat: None,
    }
}

fn binding(geometry: u32, buffer: GeometryBuffer) -> ComputeBufferBinding {
    ComputeBufferBinding { geometry, buffer, access: StorageAccess::ReadWrite }
}

fn program(counts: [u32; 3]) -> ProgramRegister {
    let groups = |count: u32| count.saturating_add(63) / 64;
    let mut compact = compute(
        "cs_compact",
        vec![
            binding(2, GeometryBuffer::Vertices),
            binding(3, GeometryBuffer::Vertices),
            binding(3, GeometryBuffer::Indices),
            binding(3, GeometryBuffer::DrawIndexedIndirect),
        ],
        1,
    );
    compact.inputs = vec![InputSlot::Transient { index: 0 }];

    ProgramRegister {
        wgsl: MODULE.to_owned(),
        bindings: vec![SlotSpec { format: TextureFormat::Rgba8, extent: SlotExtent::Full }],
        transients: vec![SlotSpec { format: TextureFormat::R32Float, extent: SlotExtent::Full }],
        geometries: vec![source_slot(), raw_slot(), raw_slot(), output_slot()],
        depth_transients: vec![SlotExtent::Full],
        passes: vec![
            ProgramPass {
                stage: PassStage::Draw(DrawPass {
                    vertex_entry_point: "vs_subject".to_owned(),
                    geometry: 0,
                    depth: Some(0),
                    load: PassLoad::Clear,
                }),
                entry_point: "fs_subject_depth".to_owned(),
                inputs: Vec::new(),
                output: OutputSlot::Transient { index: 0 },
                uniform_offset: 0,
                uniform_length: 480,
                repeat: None,
            },
            compute(
                "cs_pose_classify",
                vec![binding(0, GeometryBuffer::Vertices), binding(2, GeometryBuffer::Vertices)],
                groups(counts[0]),
            ),
            compute(
                "cs_march_faces",
                vec![
                    binding(0, GeometryBuffer::Indices),
                    binding(1, GeometryBuffer::Vertices),
                    binding(2, GeometryBuffer::Vertices),
                ],
                groups(counts[1]),
            ),
            compute(
                "cs_link_edges",
                vec![binding(1, GeometryBuffer::Vertices), binding(2, GeometryBuffer::Vertices)],
                groups(counts[2]),
            ),
            compact,
            ProgramPass {
                stage: PassStage::DrawIndexedIndirect(DrawPass {
                    vertex_entry_point: "vs_silhouette".to_owned(),
                    geometry: 3,
                    depth: None,
                    load: PassLoad::Clear,
                }),
                entry_point: "fs_silhouette".to_owned(),
                inputs: Vec::new(),
                output: OutputSlot::Binding { index: 0 },
                uniform_offset: 0,
                uniform_length: 480,
                repeat: None,
            },
        ],
    }
}

fn geometries(
    mesh: &Mesh,
    skin: Option<&Skin>,
    topology: &CanonicalTopology,
    capacity: DerivationCapacity,
) -> Result<Vec<CreateGeometry>, String> {
    let mut resident = Vec::with_capacity(capacity.vertices as usize * size_of::<ResidentVertex>());
    for vertex in ResidentVertex::pack(mesh, skin) {
        for value in vertex.position {
            resident.extend_from_slice(&value.to_le_bytes());
        }
        resident.extend_from_slice(&0.0f32.to_le_bytes());
        for value in vertex.normal {
            resident.extend_from_slice(&value.to_le_bytes());
        }
        resident.extend_from_slice(&0.0f32.to_le_bytes());
        for value in vertex.weights {
            resident.extend_from_slice(&value.to_le_bytes());
        }
    }
    let faces = mesh.faces.iter().flatten().flat_map(|index| index.to_le_bytes()).collect();

    let edge_offset = 8u32;
    let incident_offset = edge_offset
        .checked_add(capacity.edges.checked_mul(4).ok_or("topology edge word count overflow")?)
        .ok_or("topology incident offset overflow")?;
    let face_edge_offset = incident_offset
        .checked_add(capacity.edge_incidents.checked_mul(2).ok_or("topology incident word count overflow")?)
        .ok_or("topology face-edge offset overflow")?;
    let mut topology_words = vec![
        capacity.vertices,
        capacity.faces,
        capacity.edges,
        capacity.edge_incidents,
        edge_offset,
        incident_offset,
        face_edge_offset,
        0,
    ];
    for edge in &topology.edges {
        topology_words.extend([edge.vertices[0], edge.vertices[1], edge.first_incident, edge.incident_count]);
    }
    for incident in &topology.incidents {
        topology_words.extend([incident.face, incident.local_edge]);
    }
    topology_words.extend(topology.face_edges.iter().flatten().copied());
    let topology_bytes = topology_words.into_iter().flat_map(u32::to_le_bytes).collect();

    let scratch_words = u64::from(8u32)
        + u64::from(capacity.vertices) * 8
        + u64::from(capacity.faces) * 24
        + u64::from(capacity.max_points) * 8;
    let scratch_bytes = usize::try_from(scratch_words.checked_mul(4).ok_or("scratch byte count overflow")?)
        .map_err(|_| "scratch byte count exceeds usize")?;
    let output_vertices = usize::try_from(u64::from(capacity.max_points.max(1)) * 2 * 16)
        .map_err(|_| "output vertex byte count exceeds usize")?;
    let output_indices = usize::try_from(u64::from(capacity.faces.max(1)) * 6 * 4)
        .map_err(|_| "output index byte count exceeds usize")?;
    let stand_in = [0u32, 0, 0].into_iter().flat_map(u32::to_le_bytes).collect::<Vec<_>>();

    Ok(vec![
        CreateGeometry { layout: source_slot().layout, vertices: resident, indices: faces },
        CreateGeometry { layout: raw_slot().layout, vertices: topology_bytes, indices: stand_in.clone() },
        CreateGeometry { layout: raw_slot().layout, vertices: vec![0; scratch_bytes.max(4)], indices: stand_in },
        CreateGeometry {
            layout: output_slot().layout,
            vertices: vec![0; output_vertices.max(16)],
            indices: vec![0; output_indices.max(12)],
        },
    ])
}

fn uniforms(standing: &Standing, counts: [u32; 3], surface_bias: f32, exceptional_overlay: bool) -> Vec<u8> {
    let mut bytes = vec![0u8; 480];
    for (lane, value) in bytes[..64].chunks_exact_mut(4).zip(standing.view_proj.to_cols_array()) {
        lane.copy_from_slice(&value.to_le_bytes());
    }
    for (lane, value) in bytes[64..76].chunks_exact_mut(4).zip(standing.eye.to_array()) {
        lane.copy_from_slice(&value.to_le_bytes());
    }
    bytes[76..80].copy_from_slice(&u32::from(exceptional_overlay).to_le_bytes());
    for (lane, value) in bytes[80..92].chunks_exact_mut(4).zip(counts) {
        lane.copy_from_slice(&value.to_le_bytes());
    }
    bytes[92..96].copy_from_slice(&surface_bias.to_le_bytes());
    for (lane, value) in bytes[96..].chunks_exact_mut(4).zip(standing.bones) {
        lane.copy_from_slice(&value.to_le_bytes());
    }

    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::{PI, TAU};
    use std::fmt::Write as _;
    use std::time::{Duration, Instant};

    use aether_harness_substrate::{HarnessOp, SubstrateHarness};
    use aether_harness_substrate_capture::test_helpers::{envelope, has_wgpu_adapter};
    use aether_harness_substrate_capture::visual::{Image, background_top_left, coverage, decode_png};
    use aether_harness_substrate_capture::{RenderHarnessBuilderExt, RenderHarnessExt};
    use aether_kinds::QuadSpace;
    use aether_render::{
        CreateGeometryResult, CreateTextureResult, DrawTexturedQuads, PassStageKind, ProgramRegisterResult,
        ProgramTimings, ProgramTimingsResult, TexturedQuad,
    };

    use crate::deform::bone_uniform;

    const TRIANGLE: &[u8] = b"v -1 -1 0\nv 1 -1 0\nv 0 1 0\nf 1 2 3\n";
    const REAR_CUBE: &str = r"
v -1 -1 -1
v 1 -1 -1
v 1 1 -1
v -1 1 -1
v -1 -1 1
v 1 -1 1
v 1 1 1
v -1 1 1
f 1 3 2
f 1 4 3
f 5 6 7
f 5 7 8
f 1 2 6
f 1 6 5
f 2 3 7
f 2 7 6
f 3 4 8
f 3 8 7
f 4 1 5
f 4 5 8
";

    fn occluded_cube() -> Vec<u8> {
        const GRID: u32 = 16;
        const HALF_EXTENT: f32 = 2.0;

        let mut obj = REAR_CUBE.to_owned();
        for y in 0..=GRID {
            for x in 0..=GRID {
                let px = -HALF_EXTENT + 2.0 * HALF_EXTENT * x as f32 / GRID as f32;
                let py = -HALF_EXTENT + 2.0 * HALF_EXTENT * y as f32 / GRID as f32;
                writeln!(obj, "v {px} {py} 2").expect("write front-occluder vertex");
            }
        }
        for y in 0..GRID {
            for x in 0..GRID {
                let a = 9 + y * (GRID + 1) + x;
                let b = a + 1;
                let d = a + GRID + 1;
                let c = d + 1;
                writeln!(obj, "f {a} {b} {c}\nf {a} {c} {d}").expect("write front-occluder faces");
            }
        }

        obj.into_bytes()
    }

    fn visible_sphere() -> Vec<u8> {
        const RINGS: u32 = 24;
        const SEGMENTS: u32 = 64;

        let mut obj = String::from("v 0 0 1\n");
        for ring in 1..RINGS {
            let latitude = PI * ring as f32 / RINGS as f32;
            let radius = latitude.sin();
            let z = latitude.cos();
            for segment in 0..SEGMENTS {
                let longitude = TAU * segment as f32 / SEGMENTS as f32;
                writeln!(obj, "v {} {} {z}", radius * longitude.cos(), radius * longitude.sin())
                    .expect("write sphere ring vertex");
            }
        }
        writeln!(obj, "v 0 0 -1").expect("write sphere bottom vertex");

        for segment in 0..SEGMENTS {
            let next = (segment + 1) % SEGMENTS;
            writeln!(obj, "f 1 {} {}", 2 + segment, 2 + next).expect("write sphere top faces");
        }
        for ring in 0..RINGS - 2 {
            let upper = 2 + ring * SEGMENTS;
            let lower = upper + SEGMENTS;
            for segment in 0..SEGMENTS {
                let next = (segment + 1) % SEGMENTS;
                let a = upper + segment;
                let b = upper + next;
                let c = lower + segment;
                let d = lower + next;
                writeln!(obj, "f {a} {c} {d}\nf {a} {d} {b}").expect("write sphere band faces");
            }
        }
        let bottom = 2 + (RINGS - 1) * SEGMENTS;
        let last = bottom - SEGMENTS;
        for segment in 0..SEGMENTS {
            let next = (segment + 1) % SEGMENTS;
            writeln!(obj, "f {bottom} {} {}", last + next, last + segment).expect("write sphere bottom faces");
        }

        obj.into_bytes()
    }

    fn mesh() -> Mesh {
        Mesh::from_obj_bytes(TRIANGLE, 0).expect("triangle parses")
    }

    #[test]
    fn graph_is_posed_occlusion_four_bounded_derivations_and_resident_indirect_draw() {
        let register = program([65, 129, 257]);
        let groups = register
            .passes
            .iter()
            .skip(1)
            .take(4)
            .map(|pass| match &pass.stage {
                PassStage::Compute(pass) => pass.workgroups,
                _ => panic!("derivation pass is compute"),
            })
            .collect::<Vec<_>>();
        assert_eq!(groups, [[2, 1, 1], [3, 1, 1], [5, 1, 1], [1, 1, 1]]);
        assert!(matches!(register.passes[0].stage, PassStage::Draw(DrawPass { geometry: 0, depth: Some(0), .. })));
        assert!(matches!(
            register.passes[5].stage,
            PassStage::DrawIndexedIndirect(DrawPass { geometry: 3, depth: None, .. })
        ));
        assert_eq!(register.passes[4].inputs, vec![InputSlot::Transient { index: 0 }]);
        assert_eq!(register.depth_transients, vec![SlotExtent::Full]);
    }

    #[test]
    fn staging_fixes_worst_case_capacities_and_uniform_offsets() {
        let mesh = mesh();
        let topology = CanonicalTopology::of(&mesh).expect("triangle topology");
        let capacity = DerivationCapacity::of(&mesh, &topology).expect("triangle capacity");
        let staged = geometries(&mesh, None, &topology, capacity).expect("stage fixture");
        assert_eq!(staged[0].vertices.len(), mesh.positions.len() * 64);
        assert_eq!(
            staged[2].vertices.len(),
            (8 + capacity.vertices * 8 + capacity.faces * 24 + capacity.max_points * 8) as usize * 4
        );
        assert_eq!(staged[3].vertices.len(), capacity.max_points as usize * 2 * 16);
        assert_eq!(staged[3].indices.len(), capacity.faces as usize * 6 * 4);

        let standing = Standing { view_proj: Mat4::IDENTITY, eye: Vec3::new(1.0, 2.0, 3.0), bones: bone_uniform(&[]) };
        let encoded = uniforms(&standing, [3, 1, 3], 0.125, true);
        assert_eq!(encoded.len(), 480);
        assert_eq!(u32::from_le_bytes(encoded[76..80].try_into().expect("mode word")), 1);
        assert_eq!(u32::from_le_bytes(encoded[80..84].try_into().expect("vertex count")), 3);
        assert_eq!(f32::from_le_bytes(encoded[92..96].try_into().expect("surface bias")), 0.125);
    }

    #[test]
    fn selector_keeps_the_candidate_inert_until_explicitly_enabled() {
        let mut layer = GpuSilhouette::default();
        layer.subject_changed(&mesh(), None).expect("stage fixture");
        layer.solve(Mat4::IDENTITY, Vec3::new(0.0, 0.0, 3.0), &bone_uniform(&[]));
        assert!(layer.take_registers().is_empty());
        assert!(layer.select(true, false));
        assert_eq!(layer.take_registers().len(), 1);
    }

    fn create_geometry(harness: &mut SubstrateHarness, label: &'static str, geometry: &CreateGeometry) -> u32 {
        let result = harness
            .execute(vec![(label, HarnessOp::send_and_await_reply("aether.render", geometry))])
            .expect("create geometry sequence")
            .reply::<CreateGeometryResult>(label)
            .expect("decode geometry reply");
        match result {
            CreateGeometryResult::Ok { geometry_id } => geometry_id,
            CreateGeometryResult::Err { reason } => panic!("create geometry failed: {reason}"),
        }
    }

    fn create_texture(harness: &mut SubstrateHarness) -> u32 {
        let texture = CreateTexture {
            width: 64,
            height: 64,
            format: TextureFormat::Rgba8,
            sampling: TextureSampling::Linear,
            usage: TextureUsage::Writable,
            pixels: Vec::new(),
        };
        let result = harness
            .execute(vec![("texture", HarnessOp::send_and_await_reply("aether.render", &texture))])
            .expect("create texture sequence")
            .reply::<CreateTextureResult>("texture")
            .expect("decode texture reply");
        match result {
            CreateTextureResult::Ok { texture_id } => texture_id,
            CreateTextureResult::Err { error } => panic!("create texture failed: {error}"),
        }
    }

    fn register(harness: &mut SubstrateHarness, counts: [u32; 3]) -> u32 {
        let result = harness
            .execute(vec![("register", HarnessOp::send_and_await_reply("aether.render", &program(counts)))])
            .expect("register sequence")
            .reply::<ProgramRegisterResult>("register")
            .expect("decode register reply");
        match result {
            ProgramRegisterResult::Ok { program_id } => program_id,
            ProgramRegisterResult::Err { reason } => panic!("register candidate failed: {reason}"),
        }
    }

    fn overlay(texture_id: u32) -> DrawTexturedQuads {
        DrawTexturedQuads {
            texture_id,
            space: QuadSpace::Screen,
            clip: None,
            blend: QuadBlend::Premultiplied,
            quads: vec![TexturedQuad {
                x: 0.0,
                y: 0.0,
                width: 64.0,
                height: 64.0,
                u0: 0.0,
                v0: 0.0,
                u1: 1.0,
                v1: 1.0,
                tint: Rgba::new(1.0, 1.0, 1.0, 1.0),
            }],
        }
    }

    fn pixel_differs(image: &Image, background: [u8; 3], x: u32, y: u32, tolerance: u8) -> bool {
        let at = ((y * image.width + x) * 4) as usize;
        image.rgba[at..at + 3].iter().zip(background).any(|(&value, background)| value.abs_diff(background) > tolerance)
    }

    #[test]
    fn resident_candidate_is_deterministic_bounded_occluded_timed_and_recovers_with_the_device() {
        if !has_wgpu_adapter() {
            return;
        }
        let mesh = Mesh::from_obj_bytes(REAR_CUBE.as_bytes(), 0).expect("cube parses");
        let topology = CanonicalTopology::of(&mesh).expect("cube topology");
        let capacity = DerivationCapacity::of(&mesh, &topology).expect("cube capacity");
        let staged = geometries(&mesh, None, &topology, capacity).expect("stage cube");
        let counts = [capacity.vertices, capacity.faces, capacity.edges];
        let standing = Standing {
            view_proj: Mat4::from_scale(Vec3::splat(0.55)),
            // Identity-like clip projection does not supply perspective's
            // angular magnification. A distant eye makes the production
            // angular width resolvable in this deliberately tiny target.
            eye: Vec3::new(0.0, 0.0, 100.0),
            bones: bone_uniform(&[]),
        };

        let mut harness =
            SubstrateHarness::builder().size(64, 64).with_render_pass_timings().build().expect("boot render harness");
        let geometry_ids = staged
            .iter()
            .enumerate()
            .map(|(at, geometry)| {
                create_geometry(&mut harness, ["source", "topology", "scratch", "output"][at], geometry)
            })
            .collect::<Vec<_>>();
        let output = create_texture(&mut harness);
        let program_id = register(&mut harness, counts);
        let dispatch = |geometries: Vec<u32>| ProgramDispatch {
            program_id,
            bindings: vec![output],
            geometries,
            uniforms: uniforms(&standing, counts, mesh.surface_bias(), false),
        };
        let capture = |harness: &mut SubstrateHarness, label, mail: ProgramDispatch| {
            decode_png(
                harness
                    .execute(vec![(
                        label,
                        HarnessOp::capture_with_mails(
                            vec![envelope("aether.render", &mail), envelope("aether.render", &overlay(output))],
                            Vec::new(),
                        ),
                    )])
                    .expect("capture candidate")
                    .captured(label)
                    .expect("capture ran"),
            )
            .expect("decode candidate")
        };

        let first = capture(&mut harness, "first", dispatch(geometry_ids.clone()));
        let second = capture(&mut harness, "second", dispatch(geometry_ids.clone()));
        assert_eq!(first.rgba, second.rgba, "the same resident inputs must produce the same silhouette bytes");
        let background = background_top_left(&first);
        assert!(coverage(&first, background, 5) > 0.001, "the derived cube silhouette must render");

        harness.force_render_device_loss().expect("force render device loss");
        let recovered = capture(&mut harness, "recovered", dispatch(geometry_ids.clone()));
        assert_eq!(first.rgba, recovered.rgba, "reconstruction must preserve the resident derivation");

        let overflow_geometry =
            CreateGeometry { layout: output_slot().layout, vertices: vec![0; 16], indices: vec![0; 12] };
        let overflow_id = create_geometry(&mut harness, "overflow", &overflow_geometry);
        let mut overflow_ids = geometry_ids.clone();
        overflow_ids[3] = overflow_id;
        let overflow = capture(&mut harness, "overflow_capture", dispatch(overflow_ids));
        assert!(
            coverage(&overflow, background_top_left(&overflow), 5) < 0.0001,
            "capacity overflow must zero the indirect index count",
        );

        for _ in 0..6 {
            harness
                .execute(vec![
                    ("timed", HarnessOp::send_and_settle("aether.render", &dispatch(geometry_ids.clone()))),
                    ("timed_frame", HarnessOp::advance(1)),
                ])
                .expect("timed candidate dispatch");
        }
        let timing = measured_timings(&mut harness, program_id);
        match timing {
            ProgramTimingsResult::Absent { reason } => {
                assert!(!reason.trim().is_empty(), "an unavailable timing instrument must state why");
            }
            ProgramTimingsResult::Err { reason } => panic!("candidate timings failed: {reason}"),
            ProgramTimingsResult::Ok { rows, .. } => {
                assert_eq!(rows.len(), 6, "one depth prepass, four derivations, and one resident draw");
                assert_eq!(rows[0].stage, PassStageKind::Draw);
                assert!(rows[0].samples > 0);
                assert!(rows[1..5].iter().all(|row| row.stage == PassStageKind::Compute && row.samples > 0));
                assert_eq!(rows[5].stage, PassStageKind::Draw);
                assert!(rows[5].samples > 0);
            }
        }

        assert_visible_silhouette_full_width(&mut harness);
        assert_rear_silhouette_occluded(&mut harness);
    }

    /// Read the candidate's pass timings once the instrument has actually
    /// measured them.
    ///
    /// The timing instrument resolves a frame's timestamp queries a frame
    /// or more later, off that frame's critical path, and drops samples
    /// rather than stalling whenever a readback slot is still in flight
    /// (`aether_render`'s program timing module states both properties).
    /// The measurement therefore sits on no mail chain this test can
    /// settle: reading straight after the dispatches races the readback,
    /// and under the GPU contention of a full-suite run the last pass's
    /// row still read `samples: 0` (issue 5021). The harvest that folds a
    /// finished readback runs at the top of each frame record, so advance
    /// frames and re-read until every declared pass carries a sample --
    /// bounded, so a pass that is genuinely never measured still reaches
    /// the assertions below and fails there rather than hanging here.
    fn measured_timings(harness: &mut SubstrateHarness, program_id: u32) -> ProgramTimingsResult {
        let deadline = Instant::now() + Duration::from_secs(10);

        loop {
            let timing = harness
                .execute(vec![(
                    "timings",
                    HarnessOp::send_and_await_reply("aether.render", &ProgramTimings { program_id }),
                )])
                .expect("timing query")
                .reply::<ProgramTimingsResult>("timings")
                .expect("decode timing reply");
            let measured = match &timing {
                ProgramTimingsResult::Ok { rows, .. } => rows.len() == 6 && rows.iter().all(|row| row.samples > 0),
                ProgramTimingsResult::Absent { .. } | ProgramTimingsResult::Err { .. } => true,
            };

            if measured || Instant::now() >= deadline {
                return timing;
            }
            harness.execute(vec![("harvest", HarnessOp::advance(1))]).expect("advance for timing harvest");
        }
    }

    fn assert_visible_silhouette_full_width(harness: &mut SubstrateHarness) {
        let mesh = Mesh::from_obj_bytes(&visible_sphere(), 0).expect("visible sphere parses");
        let topology = CanonicalTopology::of(&mesh).expect("visible sphere topology");
        let capacity = DerivationCapacity::of(&mesh, &topology).expect("visible sphere capacity");
        let staged = geometries(&mesh, None, &topology, capacity).expect("stage visible sphere");
        let counts = [capacity.vertices, capacity.faces, capacity.edges];
        let eye = Vec3::new(0.0, 0.0, 100.0);
        let standing = Standing {
            view_proj: Mat4::orthographic_rh(-1.25, 1.25, -1.25, 1.25, 1.0, 200.0)
                * Mat4::look_at_rh(eye, Vec3::ZERO, Vec3::Y),
            eye,
            bones: bone_uniform(&[]),
        };
        let geometry_ids = staged
            .iter()
            .enumerate()
            .map(|(at, geometry)| {
                create_geometry(
                    harness,
                    ["sphere_source", "sphere_topology", "sphere_scratch", "sphere_output"][at],
                    geometry,
                )
            })
            .collect::<Vec<_>>();
        let output = create_texture(harness);
        let program_id = register(harness, counts);
        let dispatch = ProgramDispatch {
            program_id,
            bindings: vec![output],
            geometries: geometry_ids,
            uniforms: uniforms(&standing, counts, mesh.surface_bias(), false),
        };
        let capture = decode_png(
            harness
                .execute(vec![(
                    "full_width",
                    HarnessOp::capture_with_mails(
                        vec![envelope("aether.render", &dispatch), envelope("aether.render", &overlay(output))],
                        Vec::new(),
                    ),
                )])
                .expect("capture full-width silhouette")
                .captured("full_width")
                .expect("full-width capture ran"),
        )
        .expect("decode full-width silhouette");
        let background = background_top_left(&capture);
        let centreline_radius = capture.width as f32 * 0.4;
        let mut inner_rail_pixels = 0;
        let mut outer_rail_pixels = 0;
        for y in 0..capture.height {
            for x in 0..capture.width {
                if pixel_differs(&capture, background, x, y, 5) {
                    let dx = x as f32 + 0.5 - capture.width as f32 * 0.5;
                    let dy = y as f32 + 0.5 - capture.height as f32 * 0.5;
                    let radius = dx.hypot(dy);
                    inner_rail_pixels += usize::from(radius < centreline_radius - 0.6);
                    outer_rail_pixels += usize::from(radius > centreline_radius + 0.6);
                }
            }
        }

        // The unit sphere's centreline projects to radius 25.6. Shared
        // fragment depth erases every pixel more than 0.6 px inward while
        // retaining the outward half. Requiring substantial coverage on
        // both sides therefore proves the final depth-free draw retains the
        // full symmetric ribbon rather than a clipped single rail.
        assert!(
            inner_rail_pixels >= 100 && outer_rail_pixels >= 100,
            "visible silhouette must cover both rails, got {inner_rail_pixels} inner and {outer_rail_pixels} outer pixels",
        );
    }

    fn assert_rear_silhouette_occluded(harness: &mut SubstrateHarness) {
        let mesh = Mesh::from_obj_bytes(&occluded_cube(), 0).expect("occluded cube parses");
        let topology = CanonicalTopology::of(&mesh).expect("occluded cube topology");
        let capacity = DerivationCapacity::of(&mesh, &topology).expect("occluded cube capacity");
        let staged = geometries(&mesh, None, &topology, capacity).expect("stage occluded cube");
        let counts = [capacity.vertices, capacity.faces, capacity.edges];
        let eye = Vec3::new(0.0, 0.0, 100.0);
        let standing = Standing {
            view_proj: Mat4::orthographic_rh(-1.5, 1.5, -1.5, 1.5, 1.0, 200.0)
                * Mat4::look_at_rh(eye, Vec3::ZERO, Vec3::Y),
            eye,
            bones: bone_uniform(&[]),
        };

        let geometry_ids = staged
            .iter()
            .enumerate()
            .map(|(at, geometry)| create_geometry(harness, ["source", "topology", "scratch", "output"][at], geometry))
            .collect::<Vec<_>>();
        let output = create_texture(harness);
        let program_id = register(harness, counts);
        let dispatch = ProgramDispatch {
            program_id,
            bindings: vec![output],
            geometries: geometry_ids,
            uniforms: uniforms(&standing, counts, mesh.surface_bias(), false),
        };
        let capture = decode_png(
            harness
                .execute(vec![(
                    "occluded",
                    HarnessOp::capture_with_mails(
                        vec![envelope("aether.render", &dispatch), envelope("aether.render", &overlay(output))],
                        Vec::new(),
                    ),
                )])
                .expect("capture occluded candidate")
                .captured("occluded")
                .expect("occluded capture ran"),
        )
        .expect("decode occluded candidate");

        let rear_coverage = coverage(&capture, background_top_left(&capture), 5);
        assert!(
            rear_coverage < 0.0001,
            "the front surface must hide the disconnected rear silhouette, got {rear_coverage}",
        );
    }
}
