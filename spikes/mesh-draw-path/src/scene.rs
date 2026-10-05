//! One scene per drawing approach: the registered program, the resources it
//! binds, and the dispatches a frame sends.

use aether_data::Blob;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_render::{
    ComputeBufferBinding, ComputePass, CreateGeometry, CreateGeometryResult, CreateTexture, CreateTextureResult,
    DrawPass, GeometryBuffer, GeometrySlotSpec, InputSlot, OutputSlot, PassLoad, PassRepeat, PassStage,
    ProgramDispatch, ProgramPass, ProgramRegister, ProgramRegisterResult, RenderCapability, SlotExtent, SlotSpec,
    StorageAccess, TextureFormat, TextureSampling, TextureUsage, VertexAttribute, VertexFormat,
};

use crate::model::{self, Instance, Mat4, Model, TEXTURE_COUNT, TILE};
use crate::shaders::{DIRECT, INSTANCED, WINDOW_BYTES};

pub const WIDTH: u32 = 1280;
pub const HEIGHT: u32 = 720;
/// A pass entry's repeat ceiling (`MAX_REPEAT_COUNT` in `validate.rs`).
const MAX_REPEAT: usize = 4096;
/// Instances baked into one static-batch geometry.
const STATIC_CHUNK: usize = 1000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Approach {
    /// One pass entry per (model, texture part), `repeat` once per instance.
    RepeatPerTexture,
    /// One pass entry per model over an atlas, `repeat` once per instance.
    RepeatAtlas,
    /// Compute writes `instance_count`; one indirect draw per (model, part).
    InstancedPerTexture,
    /// Compute writes `instance_count`; one indirect draw per model, atlas.
    InstancedAtlas,
    /// Every instance pre-transformed on the CPU into chunked geometries.
    StaticBatch,
    /// One dispatch per instance (expected to be wrong, measured anyway).
    DispatchPerInstance,
    /// No program at all: the frame's fixed cost.
    Baseline,
}

impl Approach {
    pub const ALL: [Self; 7] = [
        Self::Baseline,
        Self::RepeatPerTexture,
        Self::RepeatAtlas,
        Self::InstancedPerTexture,
        Self::InstancedAtlas,
        Self::StaticBatch,
        Self::DispatchPerInstance,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::RepeatPerTexture => "repeat-per-texture",
            Self::RepeatAtlas => "repeat-atlas",
            Self::InstancedPerTexture => "instanced-per-texture",
            Self::InstancedAtlas => "instanced-atlas",
            Self::StaticBatch => "static-batch",
            Self::DispatchPerInstance => "dispatch-per-instance",
            Self::Baseline => "baseline",
        }
    }
}

/// What one frame costs the executor, derived from the registered graph.
#[derive(Default, Clone, Copy)]
pub struct Counts {
    pub pass_entries: usize,
    pub render_passes: usize,
    pub compute_passes: usize,
    pub draw_calls: usize,
    pub geometries: usize,
    pub geometry_bytes: usize,
}

type Frame = Box<dyn Fn(&Mat4) -> Vec<ProgramDispatch>>;

pub struct Scene {
    pub output: u32,
    pub counts: Counts,
    pub register_millis: f64,
    pub frame: Frame,
}

fn layout(with_tile: bool) -> Vec<VertexAttribute> {
    let mut layout = vec![
        VertexAttribute { location: 0, format: VertexFormat::Float32x3 },
        VertexAttribute { location: 1, format: VertexFormat::Float32x3 },
        VertexAttribute { location: 2, format: VertexFormat::Unorm8x4 },
        VertexAttribute { location: 3, format: VertexFormat::Float32x2 },
    ];
    if with_tile {
        layout.push(VertexAttribute { location: 4, format: VertexFormat::Uint8x4 });
    }
    layout
}

fn tile_of(texture: Option<usize>) -> [u8; 4] {
    texture.map_or([0; 4], |texture| [(texture % 4) as u8, (texture / 4) as u8, 1, 0])
}

fn push_vertex(bytes: &mut Vec<u8>, vertex: &model::Vertex, tile: Option<[u8; 4]>) {
    for value in vertex.position.iter().chain(&vertex.normal) {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes.extend_from_slice(&vertex.color);
    for value in vertex.uv {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    if let Some(tile) = tile {
        bytes.extend_from_slice(&tile);
    }
}

fn index_bytes(indices: impl Iterator<Item = u32>) -> Vec<u8> {
    indices.flat_map(u32::to_le_bytes).collect()
}

struct Builder<'a> {
    harness: &'a mut SubstrateHarness,
    counts: Counts,
}

impl Builder<'_> {
    fn geometry(&mut self, layout: Vec<VertexAttribute>, vertices: Vec<u8>, indices: Vec<u8>) -> u32 {
        self.counts.geometries += 1;
        self.counts.geometry_bytes += vertices.len() + indices.len();
        let mail = CreateGeometry { layout, vertices: Blob::from(vertices), indices: Blob::from(indices) };
        let render = self.harness.actor_ref::<RenderCapability>();
        let reply = self
            .harness
            .execute(vec![("geometry", HarnessOp::send_and_await_reply(&render, &mail))])
            .expect("create geometry")
            .reply::<CreateGeometryResult>("geometry")
            .expect("decode create geometry reply");
        match reply {
            CreateGeometryResult::Ok { geometry_id } => geometry_id,
            CreateGeometryResult::Err { error } => panic!("create_geometry refused: {error}"),
        }
    }

    fn texture(&mut self, mail: &CreateTexture) -> u32 {
        let render = self.harness.actor_ref::<RenderCapability>();
        let reply = self
            .harness
            .execute(vec![("texture", HarnessOp::send_and_await_reply(&render, mail))])
            .expect("create texture")
            .reply::<CreateTextureResult>("texture")
            .expect("decode create texture reply");
        match reply {
            CreateTextureResult::Ok { texture_id } => texture_id,
            CreateTextureResult::Err { error } => panic!("create_texture refused: {error}"),
        }
    }

    fn output(&mut self) -> u32 {
        self.texture(&CreateTexture {
            width: WIDTH,
            height: HEIGHT,
            format: TextureFormat::Rgba8,
            sampling: TextureSampling::Linear,
            usage: TextureUsage::Writable,
            pixels: Blob::from(Vec::new()),
        })
    }

    /// The whole tile pool in one full-extent texture: tile `t` at column
    /// `t % 4`, row `t / 4`, on a 64-texel pitch.
    fn atlas(&mut self) -> u32 {
        let (width, height) = (WIDTH as usize, HEIGHT as usize);
        let mut pixels = vec![0u8; width * height * 4];
        for texture in 0..TEXTURE_COUNT {
            model::blit(&mut pixels, width, (texture % 4) * TILE, (texture / 4) * TILE, &model::tile_pixels(texture));
        }
        self.texture(&CreateTexture {
            width: WIDTH,
            height: HEIGHT,
            format: TextureFormat::Rgba8,
            sampling: TextureSampling::Linear,
            usage: TextureUsage::Sampled,
            pixels: Blob::from(pixels),
        })
    }

    /// One texture per tile, each padded to the output extent divided by
    /// eight (160x90) because a program input must resolve to an integer
    /// division of the output.
    fn tiles(&mut self) -> Vec<u32> {
        let (width, height) = (WIDTH as usize / 8, HEIGHT as usize / 8);
        (0..TEXTURE_COUNT)
            .map(|texture| {
                let mut pixels = vec![0u8; width * height * 4];
                model::blit(&mut pixels, width, 0, 0, &model::tile_pixels(texture));
                self.texture(&CreateTexture {
                    width: width as u32,
                    height: height as u32,
                    format: TextureFormat::Rgba8,
                    sampling: TextureSampling::Linear,
                    usage: TextureUsage::Sampled,
                    pixels: Blob::from(pixels),
                })
            })
            .collect()
    }

    /// `(x, y, z, yaw)` per instance in an `R32Float` texture of the output
    /// extent divided by two, in the order `ordered` lists them.
    fn instance_texture(&mut self, ordered: &[Instance]) -> u32 {
        let (width, height) = (WIDTH as usize / 2, HEIGHT as usize / 2);
        assert!(ordered.len() * 4 <= width * height, "instance table exceeds the data texture");
        let mut pixels = vec![0u8; width * height * 4];
        for (slot, instance) in ordered.iter().enumerate() {
            let values = [instance.position[0], instance.position[1], instance.position[2], instance.yaw];
            for (k, value) in values.iter().enumerate() {
                let at = (slot * 4 + k) * 4;
                pixels[at..at + 4].copy_from_slice(&value.to_le_bytes());
            }
        }
        self.texture(&CreateTexture {
            width: width as u32,
            height: height as u32,
            format: TextureFormat::R32Float,
            sampling: TextureSampling::Nearest,
            usage: TextureUsage::Sampled,
            pixels: Blob::from(pixels),
        })
    }

    /// One geometry per (model, part), 36 bytes per vertex.
    fn part_geometries(&mut self, models: &[Model]) -> Vec<u32> {
        let mut ids = Vec::new();
        for model in models {
            for part in &model.parts {
                let mut vertices = Vec::new();
                for vertex in &part.vertices {
                    push_vertex(&mut vertices, vertex, None);
                }
                ids.push(self.geometry(layout(false), vertices, index_bytes(part.indices.iter().copied())));
            }
        }
        ids
    }

    /// One geometry per model, 40 bytes per vertex (the atlas tile rides a
    /// fifth attribute).
    fn atlas_geometries(&mut self, models: &[Model]) -> Vec<u32> {
        models
            .iter()
            .map(|model| {
                let mut vertices = Vec::new();
                let mut indices = Vec::new();
                let mut base = 0u32;
                for part in &model.parts {
                    for vertex in &part.vertices {
                        push_vertex(&mut vertices, vertex, Some(tile_of(part.texture)));
                    }
                    indices.extend(part.indices.iter().map(|index| index + base));
                    base += part.vertices.len() as u32;
                }
                self.geometry(layout(true), vertices, index_bytes(indices.into_iter()))
            })
            .collect()
    }

    fn register(&mut self, program: &ProgramRegister) -> (u32, f64) {
        let render = self.harness.actor_ref::<RenderCapability>();
        let started = std::time::Instant::now();
        let reply = self
            .harness
            .execute(vec![("register", HarnessOp::send_and_await_reply(&render, program))])
            .expect("register program")
            .reply::<ProgramRegisterResult>("register")
            .expect("decode register reply");
        let millis = started.elapsed().as_secs_f64() * 1e3;
        match reply {
            ProgramRegisterResult::Ok { program_id } => (program_id, millis),
            ProgramRegisterResult::Err { error } => panic!("program register refused: {error}"),
        }
    }
}

const fn rgba8(extent: SlotExtent) -> SlotSpec {
    SlotSpec { format: TextureFormat::Rgba8, extent }
}

fn draw_pass(
    indirect: bool,
    vertex: &str,
    fragment: &str,
    geometry: usize,
    inputs: Vec<InputSlot>,
    clear: bool,
    window: (usize, Option<PassRepeat>),
) -> ProgramPass {
    let draw = DrawPass {
        vertex_entry_point: vertex.to_owned(),
        geometry: geometry as u32,
        depth: Some(0),
        load: if clear {
            PassLoad::Clear
        } else {
            PassLoad::Load
        },
    };
    ProgramPass {
        stage: if indirect {
            PassStage::DrawIndexedIndirect(draw)
        } else {
            PassStage::Draw(draw)
        },
        entry_point: fragment.to_owned(),
        inputs,
        output: OutputSlot::Binding { index: 0 },
        uniform_offset: window.0 as u32,
        uniform_length: WINDOW_BYTES,
        repeat: window.1,
    }
}

fn count_pass(geometry: usize, uniform_offset: usize) -> ProgramPass {
    ProgramPass {
        stage: PassStage::Compute(ComputePass {
            buffers: vec![ComputeBufferBinding {
                geometry: geometry as u32,
                buffer: GeometryBuffer::DrawIndexedIndirect,
                access: StorageAccess::ReadWrite,
            }],
            workgroups: [1, 1, 1],
        }),
        entry_point: "cs_count".to_owned(),
        inputs: Vec::new(),
        output: OutputSlot::None,
        uniform_offset: uniform_offset as u32,
        uniform_length: WINDOW_BYTES,
        repeat: None,
    }
}

fn push_window(blob: &mut Vec<u8>, matrix: &Mat4, tail: [u8; 16]) {
    for value in matrix {
        blob.extend_from_slice(&value.to_le_bytes());
    }
    blob.extend_from_slice(&tail);
}

fn float_tail(x: f32) -> [u8; 16] {
    let mut tail = [0u8; 16];
    tail[..4].copy_from_slice(&x.to_le_bytes());
    tail
}

fn count_tail(base: usize, count: usize) -> [u8; 16] {
    let mut tail = [0u8; 16];
    tail[..4].copy_from_slice(&(base as u32).to_le_bytes());
    tail[4..8].copy_from_slice(&(count as u32).to_le_bytes());
    tail
}

/// The instances grouped by model, model-contiguous: `(ordered, ranges)`
/// where `ranges[m]` is model `m`'s `start..end` in `ordered`.
fn by_model(models: usize, instances: &[Instance]) -> (Vec<Instance>, Vec<(usize, usize)>) {
    let mut ordered = Vec::with_capacity(instances.len());
    let mut ranges = Vec::with_capacity(models);
    for model in 0..models {
        let start = ordered.len();
        ordered.extend(instances.iter().filter(|instance| instance.model == model));
        ranges.push((start, ordered.len()));
    }
    (ordered, ranges)
}

/// One model-view-projection window per instance, in `ordered` order.
fn instance_windows(ordered: &[Instance], view_proj: &Mat4) -> Vec<u8> {
    let mut blob = Vec::with_capacity(ordered.len() * WINDOW_BYTES as usize);
    for instance in ordered {
        push_window(&mut blob, &model::mul(view_proj, &model::model_matrix(instance)), float_tail(instance.yaw));
    }
    blob
}

pub fn build(harness: &mut SubstrateHarness, approach: Approach, model_count: usize, instances: &[Instance]) -> Scene {
    let models: Vec<Model> = (0..model_count).map(model::model).collect();
    let mut builder = Builder { harness, counts: Counts::default() };
    let output = builder.output();
    let (ordered, ranges) = by_model(model_count, instances);
    match approach {
        Approach::Baseline => {
            Scene { output, counts: builder.counts, register_millis: 0.0, frame: Box::new(|_| Vec::new()) }
        }
        Approach::RepeatPerTexture | Approach::RepeatAtlas => {
            let atlas = approach == Approach::RepeatAtlas;
            let (geometries, textures, parts) = if atlas {
                (builder.atlas_geometries(&models), vec![builder.atlas()], 1)
            } else {
                (builder.part_geometries(&models), builder.tiles(), model::PARTS)
            };
            let mut passes = Vec::new();
            for (index, model) in models.iter().enumerate() {
                let (start, end) = ranges[index];
                for part in 0..parts {
                    let (fragment, inputs) = match (atlas, model.parts[part].texture) {
                        (true, _) => ("fs_atlas", vec![InputSlot::Binding { index: 1 }]),
                        (false, Some(texture)) => ("fs_tex", vec![InputSlot::Binding { index: 1 + texture as u32 }]),
                        (false, None) => ("fs_col", Vec::new()),
                    };
                    let mut chunk = start;
                    while chunk < end {
                        let count = (end - chunk).min(MAX_REPEAT);
                        let repeat = PassRepeat { count: count as u32, uniform_stride: WINDOW_BYTES };
                        builder.counts.render_passes += count;
                        builder.counts.draw_calls += count;
                        passes.push(draw_pass(
                            false,
                            if atlas {
                                "vs40"
                            } else {
                                "vs36"
                            },
                            fragment,
                            index * parts + part,
                            inputs.clone(),
                            passes.is_empty(),
                            (chunk * WINDOW_BYTES as usize, Some(repeat)),
                        ));
                        chunk += count;
                    }
                }
            }
            let slot = GeometrySlotSpec { layout: layout(atlas) };
            let texture_extent = if atlas {
                SlotExtent::Full
            } else {
                SlotExtent::Divided { divisor: 8 }
            };
            let mut bindings = vec![rgba8(SlotExtent::Full)];
            bindings.extend(textures.iter().map(|_| rgba8(texture_extent)));
            builder.counts.pass_entries = passes.len();
            let (program_id, register_millis) = builder.register(&ProgramRegister {
                wgsl: DIRECT.to_owned(),
                bindings,
                transients: Vec::new(),
                geometries: vec![slot; geometries.len()],
                depth_transients: vec![SlotExtent::Full],
                passes,
            });
            let mut bound = vec![output];
            bound.extend(&textures);
            Scene {
                output,
                counts: builder.counts,
                register_millis,
                frame: Box::new(move |view_proj| {
                    vec![ProgramDispatch {
                        program_id,
                        bindings: bound.clone(),
                        geometries: geometries.clone(),
                        uniforms: instance_windows(&ordered, view_proj),
                    }]
                }),
            }
        }
        Approach::InstancedPerTexture | Approach::InstancedAtlas => {
            let atlas = approach == Approach::InstancedAtlas;
            let (geometries, textures, parts) = if atlas {
                (builder.atlas_geometries(&models), vec![builder.atlas()], 1)
            } else {
                (builder.part_geometries(&models), builder.tiles(), model::PARTS)
            };
            let instance_texture = builder.instance_texture(&ordered);
            let mut passes = Vec::new();
            for (index, model) in models.iter().enumerate() {
                for part in 0..parts {
                    let instances_input = InputSlot::Binding { index: 1 };
                    let (fragment, inputs) = match (atlas, model.parts[part].texture) {
                        (true, _) => ("fs_atlas", vec![instances_input, InputSlot::Binding { index: 2 }]),
                        (false, Some(texture)) => {
                            ("fs_tex", vec![instances_input, InputSlot::Binding { index: 2 + texture as u32 }])
                        }
                        (false, None) => ("fs_col", vec![instances_input]),
                    };
                    let geometry = index * parts + part;
                    let window = index * WINDOW_BYTES as usize;
                    let clear = passes.is_empty();
                    passes.push(count_pass(geometry, window));
                    passes.push(draw_pass(
                        true,
                        if atlas {
                            "vs40"
                        } else {
                            "vs36"
                        },
                        fragment,
                        geometry,
                        inputs,
                        clear,
                        (window, None),
                    ));
                    builder.counts.compute_passes += 1;
                    builder.counts.render_passes += 1;
                    builder.counts.draw_calls += 1;
                }
            }
            let slot = GeometrySlotSpec { layout: layout(atlas) };
            let texture_extent = if atlas {
                SlotExtent::Full
            } else {
                SlotExtent::Divided { divisor: 8 }
            };
            let mut bindings = vec![
                rgba8(SlotExtent::Full),
                SlotSpec { format: TextureFormat::R32Float, extent: SlotExtent::Divided { divisor: 2 } },
            ];
            bindings.extend(textures.iter().map(|_| rgba8(texture_extent)));
            builder.counts.pass_entries = passes.len();
            let (program_id, register_millis) = builder.register(&ProgramRegister {
                wgsl: INSTANCED.to_owned(),
                bindings,
                transients: Vec::new(),
                geometries: vec![slot; geometries.len()],
                depth_transients: vec![SlotExtent::Full],
                passes,
            });
            let mut bound = vec![output, instance_texture];
            bound.extend(&textures);
            Scene {
                output,
                counts: builder.counts,
                register_millis,
                frame: Box::new(move |view_proj| {
                    let mut uniforms = Vec::with_capacity(ranges.len() * WINDOW_BYTES as usize);
                    for &(start, end) in &ranges {
                        push_window(&mut uniforms, view_proj, count_tail(start, end - start));
                    }
                    vec![ProgramDispatch {
                        program_id,
                        bindings: bound.clone(),
                        geometries: geometries.clone(),
                        uniforms,
                    }]
                }),
            }
        }
        Approach::StaticBatch => {
            let atlas = builder.atlas();
            let mut geometries = Vec::new();
            for chunk in instances.chunks(STATIC_CHUNK) {
                let mut vertices = Vec::new();
                let mut indices = Vec::new();
                let mut base = 0u32;
                for instance in chunk {
                    for part in &models[instance.model].parts {
                        for vertex in &part.vertices {
                            let rotated = model::rotate_y(vertex.position, instance.yaw);
                            let position = [
                                rotated[0] + instance.position[0],
                                rotated[1] + instance.position[1],
                                rotated[2] + instance.position[2],
                            ];
                            let moved = model::Vertex {
                                position,
                                normal: model::rotate_y(vertex.normal, instance.yaw),
                                ..*vertex
                            };
                            push_vertex(&mut vertices, &moved, Some(tile_of(part.texture)));
                        }
                        indices.extend(part.indices.iter().map(|index| index + base));
                        base += part.vertices.len() as u32;
                    }
                }
                geometries.push(builder.geometry(layout(true), vertices, index_bytes(indices.into_iter())));
            }
            let passes: Vec<ProgramPass> = (0..geometries.len())
                .map(|geometry| {
                    draw_pass(
                        false,
                        "vs40",
                        "fs_atlas",
                        geometry,
                        vec![InputSlot::Binding { index: 1 }],
                        geometry == 0,
                        (0, None),
                    )
                })
                .collect();
            builder.counts.pass_entries = passes.len();
            builder.counts.render_passes = passes.len();
            builder.counts.draw_calls = passes.len();
            let (program_id, register_millis) = builder.register(&ProgramRegister {
                wgsl: DIRECT.to_owned(),
                bindings: vec![rgba8(SlotExtent::Full), rgba8(SlotExtent::Full)],
                transients: Vec::new(),
                geometries: vec![GeometrySlotSpec { layout: layout(true) }; geometries.len()],
                depth_transients: vec![SlotExtent::Full],
                passes,
            });
            Scene {
                output,
                counts: builder.counts,
                register_millis,
                frame: Box::new(move |view_proj| {
                    let mut uniforms = Vec::new();
                    push_window(&mut uniforms, view_proj, float_tail(0.0));
                    vec![ProgramDispatch {
                        program_id,
                        bindings: vec![output, atlas],
                        geometries: geometries.clone(),
                        uniforms,
                    }]
                }),
            }
        }
        Approach::DispatchPerInstance => {
            let geometries = builder.atlas_geometries(&models);
            let atlas = builder.atlas();
            let mut register_millis = 0.0;
            let programs: Vec<u32> = [true, false]
                .into_iter()
                .map(|clear| {
                    let (program_id, millis) = builder.register(&ProgramRegister {
                        wgsl: DIRECT.to_owned(),
                        bindings: vec![rgba8(SlotExtent::Full), rgba8(SlotExtent::Full)],
                        transients: Vec::new(),
                        geometries: vec![GeometrySlotSpec { layout: layout(true) }],
                        depth_transients: vec![SlotExtent::Full],
                        passes: vec![draw_pass(
                            false,
                            "vs40",
                            "fs_atlas",
                            0,
                            vec![InputSlot::Binding { index: 1 }],
                            clear,
                            (0, None),
                        )],
                    });
                    register_millis += millis;
                    program_id
                })
                .collect();
            builder.counts.pass_entries = 2;
            builder.counts.render_passes = instances.len();
            builder.counts.draw_calls = instances.len();
            Scene {
                output,
                counts: builder.counts,
                register_millis,
                frame: Box::new(move |view_proj| {
                    ordered
                        .iter()
                        .enumerate()
                        .map(|(slot, instance)| {
                            let mut uniforms = Vec::with_capacity(WINDOW_BYTES as usize);
                            let mvp = model::mul(view_proj, &model::model_matrix(instance));
                            push_window(&mut uniforms, &mvp, float_tail(instance.yaw));
                            ProgramDispatch {
                                program_id: programs[usize::from(slot > 0)],
                                bindings: vec![output, atlas],
                                geometries: vec![geometries[instance.model]],
                                uniforms,
                            }
                        })
                        .collect()
                }),
            }
        }
    }
}
