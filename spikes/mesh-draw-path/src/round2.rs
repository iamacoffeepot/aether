//! Round two: measure a prototype "render pass that draws a list of
//! instanced draws" (the `aether.render.spike.*` kinds, spike branch only).
//!
//! `mesh-draw-path round2 <suite> [--frames N] [--warmup N]`
//!
//! Suites: `q1` (list in the mail vs retained), `q1change` (editing a
//! retained list, instance updates), `q2` (texture binding), `q3` (target
//! and post-processing pipelines), `perdraw` (CPU per draw), `stress`
//! (scale limits), `indirect` (multi-draw-indirect), `cull`, `capture`,
//! `limits`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use aether_data::{Blob, Kind};
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_harness_substrate_capture::RenderHarnessBuilderExt;
use aether_harness_substrate_capture::test_helpers::envelope;
use aether_harness_substrate_capture::visual::{Image, decode_png};
use aether_kinds::QuadSpace;
use aether_math::Rgba;
use aether_render::{
    CreateGeometry, CreateGeometryResult, CreateTexture, CreateTextureResult, DrawTexturedQuads, InputSlot, OutputSlot,
    PassStage, ProgramDispatch, ProgramPass, ProgramRegister, ProgramRegisterResult, ProgramTimings,
    ProgramTimingsResult, QuadBlend, RenderCapability, SPIKE_FRAME_TARGET, SlotExtent, SlotSpec, SpikeCreateDrawList,
    SpikeCreateInstances, SpikeCreatePipeline, SpikeCreateTextureArray, SpikeCreated, SpikeCull, SpikeDestroyDrawList,
    SpikeDraw, SpikeDrawPass, SpikeLimits, SpikeLimitsResult, SpikePatchDrawList, SpikeTextures, SpikeUpdateInstances,
    TextureFormat, TextureSampling, TextureUsage, TexturedQuad, VertexAttribute, VertexFormat, spike_probe,
};

use crate::model::{self, Instance, PARTS};

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;
const TILE: usize = 128;
const UNIFORM_BYTES: u32 = 80;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tex {
    PerDraw,
    Array,
    Atlas,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum List {
    InlineTyped,
    InlineBytes,
    Retained,
    Indirect,
}

/// Where the scene pass draws and what follows it (Q3).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Pipe {
    /// Scene -> `Rgba8` texture -> `draw_textured_quads`.
    A,
    /// As `A`, the scene pass 4x multisampled and resolved.
    A4,
    /// Scene -> `Rgba16Float` -> one tonemap pass -> `Rgba8` -> quad.
    B0,
    /// Scene -> `Rgba16Float` -> bloom (8 fragment passes) -> `Rgba8` -> quad.
    B,
    /// As `B`, the scene pass 4x multisampled and resolved before bloom.
    C,
    /// Scene straight into the frame target; no quad.
    D1,
    /// Scene -> `Rgba16Float` -> bloom combine -> tonemap into the frame.
    D2,
    /// As `D2`, the scene pass 4x multisampled.
    D2C,
}

#[derive(Clone, Copy, Debug)]
struct Config {
    models: usize,
    instances: usize,
    tiles: usize,
    tex: Tex,
    list: List,
    /// Instance-range chunks per model: one draw per (model, chunk[, part]).
    splits: usize,
    per_model_geometry: bool,
    /// Order draws chunk-major so consecutive draws differ in model.
    round_robin: bool,
    cull: bool,
    pipe: Pipe,
    /// Draw only this many indices per draw (scale tests).
    stress_indices: Option<u32>,
    timings: bool,
}

impl Config {
    fn new(models: usize, instances: usize) -> Self {
        Self {
            models,
            instances,
            tiles: 64,
            tex: Tex::Array,
            list: List::Retained,
            splits: 1,
            per_model_geometry: false,
            round_robin: false,
            cull: false,
            pipe: Pipe::A,
            stress_indices: None,
            timings: false,
        }
    }
}

const SCENE_COMMON: &str = r"
struct U { view_proj: mat4x4<f32>, misc: vec4<f32> }
@group(0) @binding(0) var<uniform> u: U;

struct V {
    @builtin(position) clip: vec4<f32>,
    @location(0) color: vec4<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) light: f32,
    @location(3) @interpolate(flat) tile: vec4<u32>,
}

fn rot_y(v: vec3<f32>, yaw: f32) -> vec3<f32> {
    let c = cos(yaw);
    let s = sin(yaw);
    return vec3<f32>(c * v.x + s * v.z, v.y, -s * v.x + c * v.z);
}

@vertex
fn vs(
    @location(0) p: vec3<f32>,
    @location(1) n: vec3<f32>,
    @location(2) c: vec4<f32>,
    @location(3) uv: vec2<f32>,
    @location(4) tile: vec4<u32>,
    @location(5) origin: vec3<f32>,
    @location(6) yaw: f32,
) -> V {
    let inst = vec4<f32>(origin, yaw);
    var out: V;
    out.clip = u.view_proj * vec4<f32>(rot_y(p, inst.w) + inst.xyz, 1.0);
    out.color = c;
    out.uv = uv;
    let lit = max(dot(normalize(rot_y(n, inst.w)), normalize(vec3<f32>(0.4, 0.8, 0.45))), 0.0);
    out.light = 0.35 + 0.65 * lit;
    out.tile = tile;
    return out;
}

// The vertex-colour band glows by `u.misc.z` (1 on an 8-bit target).
fn shade(texel: vec3<f32>, v: V) -> vec4<f32> {
    let gain = mix(u.misc.z, 1.0, f32(v.tile.z));
    return vec4<f32>(texel * v.color.rgb * v.light * gain, 1.0);
}
";

const FS_PER_DRAW: &str = r"
@group(1) @binding(0) var tex: texture_2d<f32>;
@group(1) @binding(1) var smp: sampler;
@fragment
fn fs(v: V) -> @location(0) vec4<f32> {
    return shade(textureSample(tex, smp, v.uv).rgb, v);
}
";

const FS_ARRAY: &str = r"
@group(1) @binding(0) var tex: texture_2d_array<f32>;
@group(1) @binding(1) var smp: sampler;
@fragment
fn fs(v: V) -> @location(0) vec4<f32> {
    let layer = v.tile.x + v.tile.y * 256u;
    let t = textureSample(tex, smp, v.uv, layer).rgb;
    return shade(mix(vec3<f32>(1.0), t, f32(v.tile.z)), v);
}
";

const FS_ATLAS: &str = r"
@group(1) @binding(0) var tex: texture_2d<f32>;
@group(1) @binding(1) var smp: sampler;
@fragment
fn fs(v: V) -> @location(0) vec4<f32> {
    let index = v.tile.x + v.tile.y * 256u;
    let columns = u32(u.misc.x);
    let cell = vec2<f32>(f32(index % columns), f32(index / columns));
    let px = u.misc.w;
    let uv = (cell * px + fract(v.uv) * (px - 1.0) + 0.5) / (u.misc.xy * px);
    let t = textureSample(tex, smp, uv).rgb;
    return shade(mix(vec3<f32>(1.0), t, f32(v.tile.z)), v);
}
";

/// Bloom as ordinary fragment passes of today's program model.
const BLOOM: &str = r"
struct P { params: vec4<f32> }
@group(0) @binding(0) var<uniform> p: P;
@group(1) @binding(0) var a: texture_2d<f32>;
@group(1) @binding(1) var a_smp: sampler;
@group(1) @binding(2) var b: texture_2d<f32>;
@group(1) @binding(3) var b_smp: sampler;

fn box(uv: vec2<f32>) -> vec3<f32> {
    let px = 1.0 / vec2<f32>(textureDimensions(a));
    return 0.25 * (textureSample(a, a_smp, uv + px * vec2<f32>(-1.0, -1.0)).rgb
        + textureSample(a, a_smp, uv + px * vec2<f32>(1.0, -1.0)).rgb
        + textureSample(a, a_smp, uv + px * vec2<f32>(-1.0, 1.0)).rgb
        + textureSample(a, a_smp, uv + px * vec2<f32>(1.0, 1.0)).rgb);
}

@fragment
fn fs_threshold(@location(0) uv: vec2<f32>) -> @location(0) vec4<f32> {
    return vec4<f32>(max(box(uv) - vec3<f32>(p.params.x), vec3<f32>(0.0)), 1.0);
}

@fragment
fn fs_down(@location(0) uv: vec2<f32>) -> @location(0) vec4<f32> {
    return vec4<f32>(box(uv), 1.0);
}

@fragment
fn fs_up(@location(0) uv: vec2<f32>) -> @location(0) vec4<f32> {
    return vec4<f32>(box(uv) + textureSample(b, b_smp, uv).rgb, 1.0);
}

fn tonemap(c: vec3<f32>) -> vec3<f32> {
    let exposed = c * p.params.z;
    return exposed / (vec3<f32>(1.0) + exposed);
}

@fragment
fn fs_tonemap_only(@location(0) uv: vec2<f32>) -> @location(0) vec4<f32> {
    return vec4<f32>(tonemap(textureSample(a, a_smp, uv).rgb), 1.0);
}

@fragment
fn fs_tonemap(@location(0) uv: vec2<f32>) -> @location(0) vec4<f32> {
    let c = textureSample(a, a_smp, uv).rgb + p.params.y * textureSample(b, b_smp, uv).rgb;
    return vec4<f32>(tonemap(c), 1.0);
}

@fragment
fn fs_combine(@location(0) uv: vec2<f32>) -> @location(0) vec4<f32> {
    let c = textureSample(a, a_smp, uv).rgb + p.params.y * textureSample(b, b_smp, uv).rgb;
    return vec4<f32>(c * p.params.z, 1.0);
}
";

/// The final tonemap as a one-triangle draw list into the frame target.
const TONEMAP_TO_FRAME: &str = r"
struct U { params: vec4<f32> }
@group(0) @binding(0) var<uniform> u: U;
@group(1) @binding(0) var tex: texture_2d<f32>;
@group(1) @binding(1) var smp: sampler;
struct V { @builtin(position) clip: vec4<f32>, @location(0) uv: vec2<f32> }
@vertex
fn vs(@location(0) p: vec3<f32>, @location(5) origin: vec3<f32>, @location(6) yaw: f32) -> V {
    var out: V;
    out.clip = vec4<f32>(p.xy + origin.xy * yaw, 0.0, 1.0);
    out.uv = vec2<f32>(p.x * 0.5 + 0.5, 0.5 - p.y * 0.5);
    return out;
}
@fragment
fn fs(v: V) -> @location(0) vec4<f32> {
    let c = textureSampleLevel(tex, smp, clamp(v.uv, vec2<f32>(0.001), vec2<f32>(0.999)), 0.0).rgb * u.params.x;
    return vec4<f32>(c / (vec3<f32>(1.0) + c), 1.0);
}
";

fn vertex_layout() -> Vec<VertexAttribute> {
    vec![
        VertexAttribute { location: 0, format: VertexFormat::Float32x3 },
        VertexAttribute { location: 1, format: VertexFormat::Float32x3 },
        VertexAttribute { location: 2, format: VertexFormat::Unorm8x4 },
        VertexAttribute { location: 3, format: VertexFormat::Float32x2 },
        VertexAttribute { location: 4, format: VertexFormat::Uint8x4 },
    ]
}

/// Per instance: `(x, y, z, yaw)`, 16 bytes. `Float32x4` is not in the
/// engine's `VertexFormat` vocabulary, so it is a `Float32x3` at location
/// 5 plus a `Float32` at location 6.
fn instance_layout() -> Vec<VertexAttribute> {
    vec![VertexAttribute { location: 5, format: VertexFormat::Float32x3 }, VertexAttribute {
        location: 6,
        format: VertexFormat::Float32,
    }]
}

fn tile_pixels(texture: usize) -> Vec<u8> {
    let seed = texture as u32 * 2654 + 91;
    let base = [
        100.0 + 155.0 * model::unit(seed ^ 1),
        100.0 + 155.0 * model::unit(seed ^ 2),
        100.0 + 155.0 * model::unit(seed ^ 3),
    ];
    let mut pixels = Vec::with_capacity(TILE * TILE * 4);
    for y in 0..TILE {
        for x in 0..TILE {
            // A left-to-right and top-to-bottom ramp under the pattern, so
            // a wrap that bleeds or clamps shows as a hard step at the
            // tile edge instead of hiding in a periodic pattern.
            let ramp = 0.55 + 0.45 * (x as f32 / TILE as f32) * (0.5 + 0.5 * y as f32 / TILE as f32);
            let pattern = match texture % 4 {
                0 => ((x / 16 + y / 16) % 2) as f32,
                1 => ((x / 8) % 2) as f32,
                2 => ((x + y) / 12 % 2) as f32,
                _ => model::unit((x / 8 * 131 + y / 8 * 977 + texture) as u32),
            };
            let k = ramp * (0.6 + 0.4 * pattern);
            for channel in base {
                pixels.push((channel * k) as u8);
            }
            pixels.push(255);
        }
    }
    pixels
}

fn part_texture(model: usize, part: usize, tiles: usize) -> Option<usize> {
    match part {
        0 => Some((model * 7) % tiles),
        1 => Some((model * 13 + 5) % tiles),
        _ => None,
    }
}

fn atlas_columns(tiles: usize) -> usize {
    if tiles <= 64 { 8 } else { 32 }
}

macro_rules! request {
    ($harness:expr, $mail:expr, $reply:ty $(,)?) => {{
        let render = $harness.actor_ref::<RenderCapability>();
        $harness
            .execute(vec![("request", HarnessOp::send_and_await_reply(&render, $mail))])
            .expect("request")
            .reply::<$reply>("request")
            .expect("decode reply")
    }};
}

macro_rules! created {
    ($harness:expr, $mail:expr, $what:expr $(,)?) => {
        match request!($harness, $mail, SpikeCreated) {
            SpikeCreated::Ok { id } => Ok(id),
            SpikeCreated::Err { error } => Err(format!("{}: {error}", $what)),
        }
    };
}

fn texture(harness: &mut SubstrateHarness, mail: &CreateTexture) -> Result<u32, String> {
    match request!(harness, mail, CreateTextureResult) {
        CreateTextureResult::Ok { texture_id } => Ok(texture_id),
        CreateTextureResult::Err { error } => Err(format!("create_texture: {error}")),
    }
}

fn geometry(harness: &mut SubstrateHarness, layout: Vec<VertexAttribute>, vertices: Vec<u8>, indices: Vec<u32>) -> u32 {
    let mail = CreateGeometry {
        layout,
        vertices: Blob::from(vertices),
        indices: Blob::from(indices.into_iter().flat_map(u32::to_le_bytes).collect::<Vec<u8>>()),
    };
    match request!(harness, &mail, CreateGeometryResult) {
        CreateGeometryResult::Ok { geometry_id } => geometry_id,
        CreateGeometryResult::Err { error } => panic!("create_geometry refused: {error}"),
    }
}

fn target_texture(harness: &mut SubstrateHarness, format: TextureFormat) -> Result<u32, String> {
    texture(harness, &CreateTexture {
        width: WIDTH,
        height: HEIGHT,
        format,
        sampling: TextureSampling::Linear,
        usage: TextureUsage::Writable,
        pixels: Blob::from(Vec::new()),
    })
}

/// Where one model's triangles live.
#[derive(Clone, Copy)]
struct ModelRange {
    geometry: u32,
    base_vertex: u32,
    first_index: u32,
    part_indices: [u32; PARTS],
}

fn build_geometry(harness: &mut SubstrateHarness, cfg: &Config) -> Vec<ModelRange> {
    let mut ranges = Vec::with_capacity(cfg.models);
    let mut vertices = Vec::new();
    let mut indices: Vec<u32> = Vec::new();
    let mut base_vertex = 0u32;
    for index in 0..cfg.models {
        let model = model::model(index);
        let first_index = indices.len() as u32;
        let mut part_indices = [0u32; PARTS];
        let mut part_base = 0u32;
        for (part, mesh) in model.parts.iter().enumerate() {
            let tile = part_texture(index, part, cfg.tiles)
                .map_or([0u8; 4], |texture| [(texture & 255) as u8, (texture >> 8) as u8, 1, 0]);
            for vertex in &mesh.vertices {
                for value in vertex.position.iter().chain(&vertex.normal) {
                    vertices.extend_from_slice(&value.to_le_bytes());
                }
                vertices.extend_from_slice(&vertex.color);
                for value in vertex.uv {
                    vertices.extend_from_slice(&value.to_le_bytes());
                }
                vertices.extend_from_slice(&tile);
            }
            indices.extend(mesh.indices.iter().map(|index| index + part_base));
            part_indices[part] = mesh.indices.len() as u32;
            part_base += mesh.vertices.len() as u32;
        }
        if cfg.per_model_geometry {
            let geometry =
                geometry(harness, vertex_layout(), std::mem::take(&mut vertices), std::mem::take(&mut indices));
            ranges.push(ModelRange { geometry, base_vertex: 0, first_index: 0, part_indices });
        } else {
            ranges.push(ModelRange { geometry: 0, base_vertex, first_index, part_indices });
            base_vertex += part_base;
        }
    }
    if !cfg.per_model_geometry {
        let geometry = geometry(harness, vertex_layout(), vertices, indices);
        for range in &mut ranges {
            range.geometry = geometry;
        }
    }
    ranges
}

fn instance_bytes(ordered: &[Instance]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(ordered.len() * 16);
    for instance in ordered {
        for value in [instance.position[0], instance.position[1], instance.position[2], instance.yaw] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
    }
    bytes
}

/// What the textures of one scene are, and what building them cost.
struct Textures {
    per_part: Vec<u32>,
    white: u32,
    array: u32,
    atlas: u32,
    build_millis: f64,
    bytes: usize,
}

fn build_textures(harness: &mut SubstrateHarness, cfg: &Config) -> Result<Textures, String> {
    let tiles: Vec<Vec<u8>> = (0..cfg.tiles).map(tile_pixels).collect();
    let mut built = Textures { per_part: Vec::new(), white: 0, array: 0, atlas: 0, build_millis: 0.0, bytes: 0 };
    let started = Instant::now();
    match cfg.tex {
        Tex::PerDraw => {
            for pixels in &tiles {
                built.bytes += pixels.len();
                built.per_part.push(texture(harness, &CreateTexture {
                    width: TILE as u32,
                    height: TILE as u32,
                    format: TextureFormat::Rgba8,
                    sampling: TextureSampling::Linear,
                    usage: TextureUsage::Sampled,
                    pixels: Blob::from(pixels.clone()),
                })?);
            }
            built.white = texture(harness, &CreateTexture {
                width: 1,
                height: 1,
                format: TextureFormat::Rgba8,
                sampling: TextureSampling::Linear,
                usage: TextureUsage::Sampled,
                pixels: Blob::from(vec![255u8; 4]),
            })?;
        }
        Tex::Array => {
            let pixels: Vec<u8> = tiles.concat();
            built.bytes = pixels.len();
            built.array = created!(harness,
                &SpikeCreateTextureArray {
                    width: TILE as u32,
                    height: TILE as u32,
                    layers: cfg.tiles as u32,
                    pixels: Blob::from(pixels),
                },
                "texture array",
            )?;
        }
        Tex::Atlas => {
            let columns = atlas_columns(cfg.tiles);
            let rows = cfg.tiles.div_ceil(columns);
            let (width, height) = (columns * TILE, rows * TILE);
            let mut pixels = vec![0u8; width * height * 4];
            for (index, tile) in tiles.iter().enumerate() {
                let (x0, y0) = ((index % columns) * TILE, (index / columns) * TILE);
                for y in 0..TILE {
                    let at = ((y0 + y) * width + x0) * 4;
                    pixels[at..at + TILE * 4].copy_from_slice(&tile[y * TILE * 4..(y + 1) * TILE * 4]);
                }
            }
            built.bytes = pixels.len();
            built.atlas = texture(harness, &CreateTexture {
                width: width as u32,
                height: height as u32,
                format: TextureFormat::Rgba8,
                sampling: TextureSampling::Linear,
                usage: TextureUsage::Sampled,
                pixels: Blob::from(pixels),
            })?;
        }
    }
    built.build_millis = started.elapsed().as_secs_f64() * 1e3;
    Ok(built)
}

fn build_draws(cfg: &Config, ranges: &[ModelRange], spans: &[(usize, usize)], instances_id: u32, t: &Textures) -> Vec<SpikeDraw> {
    let mut keyed: Vec<(usize, usize, SpikeDraw)> = Vec::new();
    for (index, (range, &(start, end))) in ranges.iter().zip(spans).enumerate() {
        let per_chunk = (end - start).div_ceil(cfg.splits).max(1);
        let mut chunk_start = start;
        let mut chunk = 0;
        while chunk_start < end {
            let count = per_chunk.min(end - chunk_start);
            let base = SpikeDraw {
                geometry_id: range.geometry,
                first_index: range.first_index,
                index_count: range.part_indices.iter().sum(),
                base_vertex: range.base_vertex,
                instances_id,
                first_instance: chunk_start as u32,
                instance_count: count as u32,
                texture_id: t.atlas,
            };
            if cfg.tex == Tex::PerDraw {
                let mut first_index = range.first_index;
                for part in 0..PARTS {
                    let texture_id = part_texture(index, part, cfg.tiles).map_or(t.white, |texture| t.per_part[texture]);
                    keyed.push((chunk, index, SpikeDraw {
                        first_index,
                        index_count: range.part_indices[part],
                        texture_id,
                        ..base
                    }));
                    first_index += range.part_indices[part];
                }
            } else {
                keyed.push((chunk, index, base));
            }
            chunk_start += count;
            chunk += 1;
        }
    }
    if cfg.round_robin {
        // Chunk-major: neighbours differ in model (and so in texture).
        keyed.sort_by_key(|(chunk, model, _)| (*chunk, *model));
    } else if cfg.tex == Tex::PerDraw {
        keyed.sort_by_key(|(_, _, draw)| (draw.texture_id, draw.geometry_id));
    }
    let mut draws: Vec<SpikeDraw> = keyed.into_iter().map(|(_, _, draw)| draw).collect();
    if let Some(indices) = cfg.stress_indices {
        for draw in &mut draws {
            draw.index_count = draw.index_count.min(indices);
        }
    }
    draws
}

struct Bloom {
    program_id: u32,
    bindings: Vec<u32>,
    uniforms: Vec<u8>,
    register_millis: f64,
}

fn fragment(entry: &str, inputs: Vec<InputSlot>, output: OutputSlot, uniform: bool) -> ProgramPass {
    ProgramPass {
        stage: PassStage::Fragment,
        entry_point: entry.to_owned(),
        inputs,
        output,
        uniform_offset: 0,
        uniform_length: if uniform { 16 } else { 0 },
        repeat: None,
    }
}

/// The post chain over `hdr`, written into `out` (`Rgba8` tonemapped, or
/// `Rgba16Float` combined when `combine`). `bloom` false is one tonemap
/// pass.
fn build_bloom(harness: &mut SubstrateHarness, hdr: u32, out: u32, bloom: bool, combine: bool) -> Result<Bloom, String> {
    let hdr_slot = SlotSpec { format: TextureFormat::Rgba16Float, extent: SlotExtent::Full };
    let out_format = if combine { TextureFormat::Rgba16Float } else { TextureFormat::Rgba8 };
    let transient = |divisor| SlotSpec { format: TextureFormat::Rgba16Float, extent: SlotExtent::Divided { divisor } };
    let t = |index| InputSlot::Transient { index };
    let to = |index| OutputSlot::Transient { index };
    let hdr_in = InputSlot::Binding { index: 1 };
    let final_entry = if combine { "fs_combine" } else { "fs_tonemap" };
    // Transients: 0..=3 the down chain (/2 /4 /8 /16), 4..=6 the up chain
    // (/8 /4 /2). An up pass cannot add onto its own level (no blend on a
    // float target, and no pass may read its output), so it writes a
    // second transient per level.
    let passes = if bloom {
        vec![
            fragment("fs_threshold", vec![hdr_in], to(0), true),
            fragment("fs_down", vec![t(0)], to(1), false),
            fragment("fs_down", vec![t(1)], to(2), false),
            fragment("fs_down", vec![t(2)], to(3), false),
            fragment("fs_up", vec![t(3), t(2)], to(4), false),
            fragment("fs_up", vec![t(4), t(1)], to(5), false),
            fragment("fs_up", vec![t(5), t(0)], to(6), false),
            fragment(final_entry, vec![hdr_in, t(6)], OutputSlot::Binding { index: 0 }, true),
        ]
    } else {
        vec![fragment("fs_tonemap_only", vec![hdr_in], OutputSlot::Binding { index: 0 }, true)]
    };
    let register = ProgramRegister {
        wgsl: BLOOM.to_owned(),
        bindings: vec![SlotSpec { format: out_format, extent: SlotExtent::Full }, hdr_slot],
        transients: if bloom {
            vec![transient(2), transient(4), transient(8), transient(16), transient(8), transient(4), transient(2)]
        } else {
            Vec::new()
        },
        geometries: Vec::new(),
        depth_transients: Vec::new(),
        passes,
    };
    let started = Instant::now();
    let program_id = match request!(harness, &register, ProgramRegisterResult) {
        ProgramRegisterResult::Ok { program_id } => program_id,
        ProgramRegisterResult::Err { error } => return Err(format!("bloom program register: {error}")),
    };
    let mut uniforms = Vec::new();
    for value in [1.0f32, 0.8, 1.0, 0.0] {
        uniforms.extend_from_slice(&value.to_le_bytes());
    }
    Ok(Bloom { program_id, bindings: vec![out, hdr], uniforms, register_millis: started.elapsed().as_secs_f64() * 1e3 })
}

/// The one-triangle tonemap pass into the frame target.
struct FrameTonemap {
    pipeline: u32,
    list: u32,
}

fn build_frame_tonemap(harness: &mut SubstrateHarness, source: u32) -> Result<FrameTonemap, String> {
    let layout = vec![VertexAttribute { location: 0, format: VertexFormat::Float32x3 }];
    let mut vertices = Vec::new();
    for value in [-1.0f32, -1.0, 0.0, 3.0, -1.0, 0.0, -1.0, 3.0, 0.0] {
        vertices.extend_from_slice(&value.to_le_bytes());
    }
    let triangle = geometry(harness, layout.clone(), vertices, vec![0, 1, 2]);
    let instance_layout = vec![VertexAttribute { location: 5, format: VertexFormat::Float32x3 }, VertexAttribute {
        location: 6,
        format: VertexFormat::Float32,
    }];
    let instances_id = created!(harness,
        &SpikeCreateInstances { layout: instance_layout.clone(), data: Blob::from(vec![0u8; 16]) },
        "tonemap instances",
    )?;
    let pipeline = created!(harness,
        &SpikeCreatePipeline {
            wgsl: TONEMAP_TO_FRAME.to_owned(),
            vertex_entry: "vs".to_owned(),
            fragment_entry: "fs".to_owned(),
            vertex_layout: layout,
            instance_layout,
            textures: SpikeTextures::PerDraw,
            target_format: TextureFormat::Rgba8,
            to_frame: true,
            samples: 1,
            cull: SpikeCull::None,
            depth: false,
            uniform_bytes: 16,
        },
        "tonemap pipeline",
    )?;
    let list = created!(harness,
        &SpikeCreateDrawList {
            pipeline_id: pipeline,
            indirect: false,
            draws: vec![SpikeDraw {
                geometry_id: triangle,
                first_index: 0,
                index_count: 3,
                base_vertex: 0,
                instances_id,
                first_instance: 0,
                instance_count: 1,
                texture_id: source,
            }],
        },
        "tonemap list",
    )?;
    Ok(FrameTonemap { pipeline, list })
}

struct Stage {
    cfg: Config,
    harness: SubstrateHarness,
    draws: Vec<SpikeDraw>,
    instances_id: u32,
    pipeline: u32,
    target: u32,
    array: u32,
    list_id: u32,
    composite: Option<u32>,
    bloom: Option<Bloom>,
    frame_tonemap: Option<FrameTonemap>,
    texture_millis: f64,
    texture_bytes: usize,
    list_create_millis: f64,
    first_frame_millis: f64,
}

fn build(cfg: Config) -> Result<Stage, String> {
    let builder = SubstrateHarness::builder().size(WIDTH, HEIGHT);
    let builder = if cfg.timings { builder.with_render_pass_timings() } else { builder.with_render() };
    let mut harness = builder.build().expect("boot render harness");

    let ranges = build_geometry(&mut harness, &cfg);
    let placed = model::instances(cfg.models, cfg.instances);
    let mut ordered = Vec::with_capacity(placed.len());
    let mut spans = Vec::with_capacity(cfg.models);
    for index in 0..cfg.models {
        let start = ordered.len();
        ordered.extend(placed.iter().filter(|instance| instance.model == index));
        spans.push((start, ordered.len()));
    }
    let instances_id = created!(harness,
        &SpikeCreateInstances { layout: instance_layout(), data: Blob::from(instance_bytes(&ordered)) },
        "instances",
    )?;
    let textures = build_textures(&mut harness, &cfg)?;
    let draws = build_draws(&cfg, &ranges, &spans, instances_id, &textures);

    let hdr = matches!(cfg.pipe, Pipe::B0 | Pipe::B | Pipe::C | Pipe::D2 | Pipe::D2C);
    let msaa = matches!(cfg.pipe, Pipe::A4 | Pipe::C | Pipe::D2C);
    let to_frame = cfg.pipe == Pipe::D1;
    let scene_format = if hdr { TextureFormat::Rgba16Float } else { TextureFormat::Rgba8 };
    let fragment = match cfg.tex {
        Tex::PerDraw => FS_PER_DRAW,
        Tex::Array => FS_ARRAY,
        Tex::Atlas => FS_ATLAS,
    };
    let pipeline = created!(harness,
        &SpikeCreatePipeline {
            wgsl: format!("{SCENE_COMMON}{fragment}"),
            vertex_entry: "vs".to_owned(),
            fragment_entry: "fs".to_owned(),
            vertex_layout: vertex_layout(),
            instance_layout: instance_layout(),
            textures: if cfg.tex == Tex::Array { SpikeTextures::Array } else { SpikeTextures::PerDraw },
            target_format: scene_format,
            to_frame,
            samples: if msaa { 4 } else { 1 },
            cull: if cfg.cull { SpikeCull::Back } else { SpikeCull::None },
            depth: true,
            uniform_bytes: UNIFORM_BYTES,
        },
        "scene pipeline",
    )?;

    let target = if to_frame { SPIKE_FRAME_TARGET } else { target_texture(&mut harness, scene_format)? };
    let (mut composite, mut bloom, mut frame_tonemap) = (None, None, None);
    match cfg.pipe {
        Pipe::A | Pipe::A4 => composite = Some(target),
        Pipe::D1 => {}
        Pipe::B0 | Pipe::B | Pipe::C => {
            let out = target_texture(&mut harness, TextureFormat::Rgba8)?;
            bloom = Some(build_bloom(&mut harness, target, out, cfg.pipe != Pipe::B0, false)?);
            composite = Some(out);
        }
        Pipe::D2 | Pipe::D2C => {
            let combined = target_texture(&mut harness, TextureFormat::Rgba16Float)?;
            bloom = Some(build_bloom(&mut harness, target, combined, true, true)?);
            frame_tonemap = Some(build_frame_tonemap(&mut harness, combined)?);
        }
    }

    let mut list_id = 0;
    let mut list_create_millis = 0.0;
    if matches!(cfg.list, List::Retained | List::Indirect) {
        let started = Instant::now();
        list_id = created!(harness,
            &SpikeCreateDrawList { pipeline_id: pipeline, indirect: cfg.list == List::Indirect, draws: draws.clone() },
            "draw list",
        )?;
        list_create_millis = started.elapsed().as_secs_f64() * 1e3;
    }
    let mut stage = Stage {
        cfg,
        harness,
        draws,
        instances_id,
        pipeline,
        target,
        array: textures.array,
        list_id,
        composite,
        bloom,
        frame_tonemap,
        texture_millis: textures.build_millis,
        texture_bytes: textures.bytes,
        list_create_millis,
        first_frame_millis: 0.0,
    };
    stage.first_frame_millis = stage.frame(0).millis;
    Ok(stage)
}

fn quad(texture_id: u32) -> DrawTexturedQuads {
    DrawTexturedQuads {
        texture_id,
        space: QuadSpace::Screen,
        clip: None,
        blend: QuadBlend::Premultiplied,
        quads: vec![TexturedQuad {
            x: 0.0,
            y: 0.0,
            width: WIDTH as f32,
            height: HEIGHT as f32,
            u0: 0.0,
            v0: 0.0,
            u1: 1.0,
            v1: 1.0,
            tint: Rgba::new(1.0, 1.0, 1.0, 1.0),
        }],
    }
}

#[derive(Default, Clone, Copy)]
struct FrameCost {
    millis: f64,
    mails: usize,
    bytes: usize,
}

impl Stage {
    fn scene_pass(&self, frame: usize) -> SpikeDrawPass {
        let view_proj = model::view_projection(self.cfg.instances, frame, WIDTH as f32 / HEIGHT as f32);
        let mut uniforms = Vec::with_capacity(UNIFORM_BYTES as usize);
        for value in view_proj {
            uniforms.extend_from_slice(&value.to_le_bytes());
        }
        let columns = atlas_columns(self.cfg.tiles);
        let hdr = self.bloom.is_some();
        let misc = [
            columns as f32,
            self.cfg.tiles.div_ceil(columns) as f32,
            if hdr { 4.0 } else { 1.0 },
            TILE as f32,
        ];
        for value in misc {
            uniforms.extend_from_slice(&value.to_le_bytes());
        }
        let (draws, draw_bytes) = match self.cfg.list {
            List::InlineTyped => (self.draws.clone(), Vec::new()),
            List::InlineBytes => (Vec::new(), pack(&self.draws)),
            List::Retained | List::Indirect => (Vec::new(), Vec::new()),
        };
        SpikeDrawPass {
            pipeline_id: self.pipeline,
            target: self.target,
            clear: true,
            array_texture: self.array,
            uniforms,
            list_id: self.list_id,
            draws,
            draw_bytes,
        }
    }

    fn bloom_dispatch(&self) -> Option<ProgramDispatch> {
        self.bloom.as_ref().map(|bloom| ProgramDispatch {
            program_id: bloom.program_id,
            bindings: bloom.bindings.clone(),
            geometries: Vec::new(),
            uniforms: bloom.uniforms.clone(),
        })
    }

    fn tonemap_pass(&self) -> Option<SpikeDrawPass> {
        self.frame_tonemap.as_ref().map(|tonemap| SpikeDrawPass {
            pipeline_id: tonemap.pipeline,
            target: SPIKE_FRAME_TARGET,
            clear: false,
            array_texture: 0,
            uniforms: 1.0f32.to_le_bytes().iter().copied().chain([0u8; 12]).collect(),
            list_id: tonemap.list,
            draws: Vec::new(),
            draw_bytes: Vec::new(),
        })
    }

    /// Send one frame's mail and run one frame.
    fn frame(&mut self, frame: usize) -> FrameCost {
        let render = self.harness.actor_ref::<RenderCapability>();
        let scene = self.scene_pass(frame);
        let dispatch = self.bloom_dispatch();
        let tonemap = self.tonemap_pass();
        let composite = self.composite.map(quad);
        let mut cost = FrameCost::default();

        let started = Instant::now();
        let mut steps: Vec<(&str, HarnessOp)> = Vec::with_capacity(5);
        steps.push(("scene", HarnessOp::send_and_settle(&render, &scene)));
        if let Some(dispatch) = &dispatch {
            steps.push(("post", HarnessOp::send_and_settle(&render, dispatch)));
        }
        if let Some(tonemap) = &tonemap {
            steps.push(("tonemap", HarnessOp::send_and_settle(&render, tonemap)));
        }
        if let Some(composite) = &composite {
            steps.push(("composite", HarnessOp::send_and_settle(&render, composite)));
        }
        cost.mails = steps.len();
        steps.push(("frame", HarnessOp::advance(1)));
        self.harness.execute(steps).expect("frame");
        cost.millis = started.elapsed().as_secs_f64() * 1e3;

        cost.bytes = scene.encode_into_bytes().len()
            + dispatch.map_or(0, |mail| mail.encode_into_bytes().len())
            + tonemap.map_or(0, |mail| mail.encode_into_bytes().len())
            + composite.map_or(0, |mail| mail.encode_into_bytes().len());
        cost
    }

    fn capture(&mut self, name: &str) -> Image {
        let mut mails = vec![envelope("aether.render", &self.scene_pass(0))];
        mails.extend(self.bloom_dispatch().map(|mail| envelope("aether.render", &mail)));
        mails.extend(self.tonemap_pass().map(|mail| envelope("aether.render", &mail)));
        mails.extend(self.composite.map(|id| envelope("aether.render", &quad(id))));
        let result =
            self.harness.execute(vec![("capture", HarnessOp::capture_with_mails(mails, Vec::new()))]).expect("capture");
        let png = result.captured("capture").expect("capture step ran");
        std::fs::create_dir_all("captures").expect("create captures dir");
        std::fs::write(format!("captures/{name}.png"), png).expect("write capture");
        decode_png(png).expect("decode capture")
    }
}

fn pack(draws: &[SpikeDraw]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(draws.len() * 32);
    for draw in draws {
        for word in [
            draw.geometry_id,
            draw.first_index,
            draw.index_count,
            draw.base_vertex,
            draw.instances_id,
            draw.first_instance,
            draw.instance_count,
            draw.texture_id,
        ] {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
    }
    bytes
}

struct Probe([u64; 14]);

fn probe() -> Probe {
    let read = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
    Probe([
        read(&spike_probe::FRAME_NANOS),
        read(&spike_probe::DRAW_PASS_NANOS),
        read(&spike_probe::RESOLVE_NANOS),
        read(&spike_probe::ENCODE_NANOS),
        read(&spike_probe::PROGRAM_RECORD_NANOS),
        read(&spike_probe::SUBMIT_NANOS),
        read(&spike_probe::GPU_WAIT_NANOS),
        read(&spike_probe::DRAWS),
        read(&spike_probe::STATE_SETS),
        read(&spike_probe::PASSES),
        read(&spike_probe::DROPPED_PASSES),
        read(&spike_probe::LIST_CREATE_NANOS),
        read(&spike_probe::LIST_PATCH_NANOS),
        read(&spike_probe::UPDATE_NANOS),
    ])
}

struct Row {
    frames: usize,
    median: f64,
    worst: f64,
    on_frame: f64,
    draw_pass: f64,
    resolve: f64,
    encode: f64,
    program: f64,
    submit: f64,
    wait: f64,
    mails: usize,
    bytes: usize,
    draws: f64,
    sets: f64,
    passes: f64,
    dropped: u64,
}

struct Run {
    frames: usize,
    warmup: usize,
}

fn measure(stage: &mut Stage, run: &Run) -> Row {
    for frame in 1..=run.warmup {
        stage.frame(frame);
    }
    let before = probe();
    let mut times = Vec::with_capacity(run.frames);
    let mut last = FrameCost::default();
    let mut frames = 0;
    let budget = Instant::now();
    while frames < run.frames {
        last = stage.frame(run.warmup + 1 + frames);
        times.push(last.millis);
        frames += 1;
        let slow = budget.elapsed().as_secs_f64() > 45.0;
        let enough = frames >= 20;
        if slow && enough {
            break;
        }
    }
    let after = probe();
    times.sort_by(f64::total_cmp);
    let per = |index: usize| (after.0[index] - before.0[index]) as f64 / frames as f64;
    Row {
        frames,
        median: times[times.len() / 2],
        worst: times[times.len() - 1],
        on_frame: per(0) / 1e6,
        draw_pass: per(1) / 1e6,
        resolve: per(2) / 1e6,
        encode: per(3) / 1e6,
        program: per(4) / 1e6,
        submit: per(5) / 1e6,
        wait: per(6) / 1e6,
        mails: last.mails,
        bytes: last.bytes,
        draws: per(7),
        sets: per(8),
        passes: per(9),
        dropped: after.0[10] - before.0[10],
    }
}

const HEADER: &str = "| variant | models | instances | draws/frame | frames | frame ms median | frame ms worst | on_frame ms | draw-pass record ms | of which validate ms | of which pass encode ms | program record ms | finish+submit ms | wait prev GPU ms | mails | bytes mailed | state sets/frame | spike passes | dropped |";

fn print_header() {
    println!("{HEADER}");
    println!("|{}", "---|".repeat(HEADER.matches('|').count() - 1));
}

fn print_row(label: &str, cfg: &Config, row: &Row) {
    println!(
        "| {label} | {} | {} | {:.0} | {} | {:.2} | {:.2} | {:.2} | {:.3} | {:.3} | {:.3} | {:.3} | {:.3} | {:.2} | {} | {} | {:.0} | {:.0} | {} |",
        cfg.models,
        cfg.instances,
        row.draws,
        row.frames,
        row.median,
        row.worst,
        row.on_frame,
        row.draw_pass,
        row.resolve,
        row.encode,
        row.program,
        row.submit,
        row.wait,
        row.mails,
        row.bytes,
        row.sets,
        row.passes,
        row.dropped,
    );
}

/// `--repeat R`: build and measure each configuration R times and keep
/// the run with the lowest median frame time (the GPU's power state makes
/// light loads bimodal on this machine).
static REPEAT: AtomicU64 = AtomicU64::new(1);

fn run_row(label: &str, cfg: Config, run: &Run) -> Option<(Stage, Row)> {
    let mut best: Option<(Stage, Row)> = None;
    for _ in 0..REPEAT.load(Ordering::Relaxed) {
        match build(cfg) {
            Ok(mut stage) => {
                let row = measure(&mut stage, run);
                let better = best.as_ref().is_none_or(|(_, kept)| row.median < kept.median);
                if better {
                    best = Some((stage, row));
                }
            }
            Err(error) => {
                println!("| {label} | {} | {} | refused: {error} |", cfg.models, cfg.instances);
                return None;
            }
        }
    }
    let (stage, row) = best?;
    print_row(label, &cfg, &row);
    Some((stage, row))
}

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

/// A config with `draws` draws over 30,000 instances (one texture array,
/// one shared geometry): up to 1,000 models, the rest by splitting each
/// model's instance range.
fn draws_config(draws: usize) -> Config {
    let models = draws.min(1000);
    let light = LIGHT.load(Ordering::Relaxed);
    Config { splits: draws / models, stress_indices: light.then_some(3), ..Config::new(models, draws.max(30_000)) }
}

/// `--light`: 3 indices per draw, so the GPU is idle and frame time is CPU.
static LIGHT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn suite_q1(run: &Run) {
    println!("\n## Q1: the draw list in the dispatch mail vs a retained list\n");
    print_header();
    let mut decode = Vec::new();
    for draws in [300, 900, 3000, 10_000, 30_000] {
        for (label, list) in [
            ("inline-typed", List::InlineTyped),
            ("inline-bytes", List::InlineBytes),
            ("retained", List::Retained),
        ] {
            let cfg = Config { list, ..draws_config(draws) };
            let Some((stage, _)) = run_row(&format!("{label} {draws}"), cfg, run) else { continue };
            if list != List::Retained {
                let bytes = stage.scene_pass(0).encode_into_bytes();
                let samples: Vec<f64> = (0..50)
                    .map(|_| {
                        let started = Instant::now();
                        let decoded = SpikeDrawPass::decode_from_bytes(&bytes).expect("decodes");
                        let micros = started.elapsed().as_secs_f64() * 1e6;
                        std::hint::black_box(decoded);
                        micros
                    })
                    .collect();
                let encode: Vec<f64> = (0..50)
                    .map(|_| {
                        let pass = stage.scene_pass(0);
                        let started = Instant::now();
                        std::hint::black_box(pass.encode_into_bytes());
                        started.elapsed().as_secs_f64() * 1e6
                    })
                    .collect();
                decode.push((label, draws, bytes.len(), median(encode), median(samples)));
            }
        }
    }
    println!("\n| list | draws | mail bytes | encode_into_bytes us | decode_from_bytes us | encode ns/draw | decode ns/draw |");
    println!("|---|---|---|---|---|---|---|");
    for (label, draws, bytes, encode, samples) in decode {
        println!(
            "| {label} | {draws} | {bytes} | {encode:.1} | {samples:.1} | {:.1} | {:.1} |",
            encode * 1e3 / draws as f64,
            samples * 1e3 / draws as f64
        );
    }
}

fn suite_q1change() {
    println!("\n## Q1: changing a retained list (20 repetitions each, median wall-clock of the send; engine = time inside the registry)\n");
    println!("| list draws | edit | entries | wall us | engine us | engine ns/entry |");
    println!("|---|---|---|---|---|---|");
    for draws in [3000, 30_000] {
        let cfg = draws_config(draws);
        let mut stage = build(cfg).expect("stage");
        let render = stage.harness.actor_ref::<RenderCapability>();
        let (mut walls, before) = (Vec::new(), probe());
        for _ in 0..20 {
            let mail =
                SpikeCreateDrawList { pipeline_id: stage.pipeline, indirect: false, draws: stage.draws.clone() };
            let started = Instant::now();
            let new_id = created!(stage.harness, &mail, "rebuild").expect("rebuild");
            let old = std::mem::replace(&mut stage.list_id, new_id);
            stage
                .harness
                .execute(vec![("destroy", HarnessOp::send_and_settle(&render, &SpikeDestroyDrawList { list_id: old }))])
                .expect("destroy");
            walls.push(started.elapsed().as_secs_f64() * 1e6);
            stage.frame(1);
        }
        let engine = (probe().0[11] - before.0[11]) as f64 / 20.0 / 1e3;
        println!("| {draws} | rebuild (create new + destroy old) | {draws} | {:.0} | {engine:.0} | {:.1} |", median(walls), engine * 1e3 / draws as f64);
        for entries in [1usize, 100, 1000, 10_000] {
            if entries > draws {
                continue;
            }
            let (mut walls, before) = (Vec::new(), probe());
            for repetition in 0..20 {
                let first = (repetition * 37) % (draws - entries + 1);
                let mail = SpikePatchDrawList {
                    list_id: stage.list_id,
                    first: first as u32,
                    draws: stage.draws[first..first + entries].to_vec(),
                };
                let started = Instant::now();
                stage.harness.execute(vec![("patch", HarnessOp::send_and_settle(&render, &mail))]).expect("patch");
                walls.push(started.elapsed().as_secs_f64() * 1e6);
                stage.frame(1);
            }
            let engine = (probe().0[12] - before.0[12]) as f64 / 20.0 / 1e3;
            println!("| {draws} | patch | {entries} | {:.0} | {engine:.1} | {:.1} |", median(walls), engine * 1e3 / entries as f64);
        }
    }

    println!("\n## Instance buffer sub-range update (20 repetitions, median wall-clock of the send)\n");
    println!("| buffer instances | instances written | bytes | wall us | engine us |");
    println!("|---|---|---|---|---|");
    let mut stage = build(Config::new(300, 100_000)).expect("stage");
    let render = stage.harness.actor_ref::<RenderCapability>();
    for count in [1usize, 100, 10_000, 100_000] {
        let (mut walls, before) = (Vec::new(), probe());
        let data = instance_bytes(&model::instances(300, count));
        for _ in 0..20 {
            let mail = SpikeUpdateInstances { instances_id: stage.instances_id, first_instance: 0, data: data.clone() };
            let started = Instant::now();
            stage.harness.execute(vec![("update", HarnessOp::send_and_settle(&render, &mail))]).expect("update");
            walls.push(started.elapsed().as_secs_f64() * 1e6);
            stage.frame(1);
        }
        let engine = (probe().0[13] - before.0[13]) as f64 / 20.0 / 1e3;
        println!("| 100000 | {count} | {} | {:.0} | {engine:.1} |", data.len(), median(walls));
    }
}

fn suite_q2(run: &Run, models: &[usize], instances: &[usize]) {
    println!("\n## Q2: how textures bind (scene -> Rgba8 texture -> quad; retained list)\n");
    let mut setups = Vec::new();
    print_header();
    for &tiles in &[64usize, 512] {
        for &model_count in models {
            for &instance_count in instances {
                for (label, tex) in [("a per-draw", Tex::PerDraw), ("b array", Tex::Array), ("c atlas", Tex::Atlas)] {
                    let cfg = Config { tiles, tex, ..Config::new(model_count, instance_count) };
                    let label = format!("{label} N={tiles}");
                    let before = spike_probe::BIND_GROUPS_CREATED.load(Ordering::Relaxed);
                    if let Some((stage, _)) = run_row(&label, cfg, run) {
                        let groups = spike_probe::BIND_GROUPS_CREATED.load(Ordering::Relaxed) - before;
                        setups.push((label, model_count, instance_count, stage, groups));
                    }
                }
            }
        }
    }
    println!("\n| variant | models | instances | texture MB | create textures ms | create list ms | first frame ms | texture bind groups |");
    println!("|---|---|---|---|---|---|---|---|");
    for (label, model_count, instance_count, stage, groups) in setups {
        println!(
            "| {label} | {model_count} | {instance_count} | {:.1} | {:.1} | {:.2} | {:.1} | {groups} |",
            stage.texture_bytes as f64 / 1e6,
            stage.texture_millis,
            stage.list_create_millis,
            stage.first_frame_millis,
        );
    }
}

fn suite_q3(run: &Run) {
    println!("\n## Q3: where the pass draws (300 models x 20,000 instances, texture array N=64, retained list, 300 draws)\n");
    print_header();
    let mut registers = Vec::new();
    for (label, pipe) in [
        ("a scene->Rgba8 tex->quad", Pipe::A),
        ("a4 as a, scene 4xMSAA resolved", Pipe::A4),
        ("b0 scene->HDR->tonemap pass->Rgba8->quad", Pipe::B0),
        ("b scene->HDR->bloom 8 passes->Rgba8->quad", Pipe::B),
        ("c as b, scene 4xMSAA resolved", Pipe::C),
        ("d1 scene->frame target (frame is 4xMSAA)", Pipe::D1),
        ("d2 scene->HDR->bloom->tonemap into frame", Pipe::D2),
        ("d2c as d2, scene 4xMSAA resolved", Pipe::D2C),
    ] {
        let cfg = Config { pipe, ..Config::new(300, 20_000) };
        if let Some((stage, _)) = run_row(label, cfg, run)
            && let Some(bloom) = &stage.bloom
        {
            registers.push((label, bloom.register_millis));
        }
    }
    println!();
    for (label, millis) in registers {
        println!("post program register for `{label}`: {millis:.1} ms");
    }

    println!("\n### GPU time per bloom pass (timestamp queries, pipeline b, separate run with the instrument on)\n");
    let cfg = Config { pipe: Pipe::B, timings: true, ..Config::new(300, 20_000) };
    match build(cfg) {
        Ok(mut stage) => {
            for frame in 1..120 {
                stage.frame(frame);
            }
            let program_id = stage.bloom.as_ref().expect("bloom").program_id;
            match request!(stage.harness, &ProgramTimings { program_id }, ProgramTimingsResult) {
                ProgramTimingsResult::Ok { rows, .. } => {
                    println!("| pass | entry | extent | mean us | mad us | samples |");
                    println!("|---|---|---|---|---|---|");
                    let mut total = 0.0;
                    for row in rows {
                        total += row.mean_nanos as f64 / 1e3;
                        println!(
                            "| {} | {} | {}x{} | {:.1} | {:.1} | {} |",
                            row.pass,
                            row.label,
                            row.width,
                            row.height,
                            row.mean_nanos as f64 / 1e3,
                            row.mad_nanos as f64 / 1e3,
                            row.samples
                        );
                    }
                    println!("| all | | | {total:.1} | | |");
                }
                ProgramTimingsResult::Absent { reason } => println!("timings absent: {reason}"),
                ProgramTimingsResult::Err { error } => println!("timings error: {error}"),
            }
        }
        Err(error) => println!("refused: {error}"),
    }
}

fn suite_perdraw(run: &Run) {
    println!("\n## CPU cost per draw inside one pass (retained list, so no validation; 30,000 instances)\n");
    print_header();
    let mut rows = Vec::new();
    for draws in [300usize, 3000, 30_000] {
        let base = draws_config(draws);
        let third = Config { splits: (draws / 3 / base.models).max(1), ..base };
        for (label, cfg) in [
            ("no state change (shared geometry, array)", base),
            ("bind group per draw (per-draw textures, round robin)", Config {
                tex: Tex::PerDraw,
                tiles: 512,
                round_robin: true,
                ..third
            }),
            ("vertex+index buffer per draw (geometry per model, round robin)", Config {
                per_model_geometry: true,
                round_robin: true,
                ..base
            }),
            ("both per draw", Config {
                tex: Tex::PerDraw,
                tiles: 512,
                per_model_geometry: true,
                round_robin: true,
                ..third
            }),
        ] {
            if let Some((_, row)) = run_row(&format!("{label} {draws}"), cfg, run) {
                rows.push((label, row));
            }
        }
    }
    let baseline = run_row("empty list (pass with zero draws)", Config { stress_indices: Some(0), ..Config::new(1, 1) }, run);
    let (base_encode, base_submit) = baseline.map_or((0.0, 0.0), |(_, row)| (row.encode, row.submit));
    println!("\n| variant | draws | state sets/draw | pass encode us/draw | finish+submit us/draw | CPU us/draw (both) |");
    println!("|---|---|---|---|---|---|");
    for (label, row) in rows {
        let encode = (row.encode - base_encode) * 1e3 / row.draws;
        let submit = (row.submit - base_submit) * 1e3 / row.draws;
        println!("| {label} | {:.0} | {:.2} | {encode:.3} | {submit:.3} | {:.3} |", row.draws, row.sets / row.draws, encode + submit);
    }
}

fn suite_validate(run: &Run) {
    println!("\n## Per-frame validation of an inline list (inline-bytes, so no codec): what the checks cost by how ids repeat\n");
    print_header();
    let mut rows = Vec::new();
    for draws in [3000usize, 30_000] {
        let base = Config { list: List::InlineBytes, ..draws_config(draws) };
        let third = Config { splits: (draws / 3 / base.models).max(1), ..base };
        for (label, cfg) in [
            ("every draw names the same geometry/instances (run-cached)", base),
            ("geometry id changes every draw (1,000 geometries)", Config {
                per_model_geometry: true,
                round_robin: true,
                ..base
            }),
            ("texture id changes every draw (512 textures)", Config {
                tex: Tex::PerDraw,
                tiles: 512,
                round_robin: true,
                ..third
            }),
            ("geometry and texture ids change every draw", Config {
                tex: Tex::PerDraw,
                tiles: 512,
                per_model_geometry: true,
                round_robin: true,
                ..third
            }),
        ] {
            if let Some((_, row)) = run_row(&format!("{label} {draws}"), cfg, run) {
                rows.push((label, row));
            }
        }
    }
    println!("\n| id pattern | draws | validate us/frame | validate ns/draw |");
    println!("|---|---|---|---|");
    for (label, row) in rows {
        println!("| {label} | {:.0} | {:.1} | {:.1} |", row.draws, row.resolve * 1e3, row.resolve * 1e6 / row.draws);
    }
}

fn suite_stress(run: &Run) {
    println!("\n## Scale: draws and instances until something fails (3 indices per draw, so the GPU is not the limit)\n");
    print_header();
    let run = Run { frames: run.frames.min(30), warmup: 5 };
    for draws in [100_000usize, 300_000, 1_000_000, 3_000_000] {
        for (label, list) in [("retained", List::Retained), ("inline-bytes", List::InlineBytes), ("inline-typed", List::InlineTyped)] {
            let cfg = Config {
                list,
                splits: draws / 1000,
                stress_indices: Some(3),
                ..Config::new(1000, draws)
            };
            run_row(&format!("{label} draws {draws}"), cfg, &run);
        }
    }
    for instances in [1_000_000usize, 4_000_000, 16_000_000, 16_777_216, 17_000_000] {
        let cfg = Config { stress_indices: Some(3), ..Config::new(300, instances) };
        run_row(&format!("instances {instances}"), cfg, &run);
    }
    println!("\n### Real triangles: 300-triangle models, texture array, retained list\n");
    print_header();
    for (models, instances) in [(1000, 300_000), (1000, 1_000_000)] {
        run_row("300-triangle instances", Config::new(models, instances), &run);
    }
}

fn suite_indirect(run: &Run) {
    println!("\n## multi_draw_indexed_indirect (one call for the list) vs draw_indexed per entry; retained, array, shared geometry\n");
    print_header();
    for draws in [3000usize, 30_000] {
        run_row(&format!("draw_indexed x{draws}"), draws_config(draws), run);
        run_row(&format!("multi_draw_indexed_indirect {draws}"), Config { list: List::Indirect, ..draws_config(draws) }, run);
    }
}

fn suite_cull(run: &Run) {
    println!("\n## Culling declared on the pass (texture array, retained)\n");
    print_header();
    for (models, instances) in [(300, 20_000), (300, 100_000)] {
        run_row("cull none", Config::new(models, instances), run);
        run_row("cull back", Config { cull: true, ..Config::new(models, instances) }, run);
    }
}

fn difference(image: &Image, reference: &Image) -> (f64, f64, u64, f64) {
    let background = &image.rgba[..4];
    let (mut total, mut covered, mut worst, mut off) = (0u64, 0u64, 0u64, 0u64);
    for (pixel, other) in image.rgba.chunks_exact(4).zip(reference.rgba.chunks_exact(4)) {
        let delta: u64 = pixel.iter().zip(other).map(|(a, b)| u64::from(a.abs_diff(*b))).sum();
        total += delta;
        let peak = pixel.iter().zip(other).map(|(a, b)| u64::from(a.abs_diff(*b))).max().unwrap_or(0);
        worst = worst.max(peak);
        off += u64::from(peak > 8);
        covered += u64::from(pixel != background);
    }
    let pixels = (image.rgba.len() / 4) as f64;
    (total as f64 / (pixels * 4.0), covered as f64 / pixels * 100.0, worst, off as f64 / covered.max(1) as f64 * 100.0)
}

fn suite_capture() {
    println!("\n## Captures (captures/r2-*.png); difference is mean absolute channel value (0-255) against the first row of each group\n");
    println!("| capture | % pixels drawn | mean abs diff vs reference | worst channel diff | % of drawn pixels off by more than 8 |");
    println!("|---|---|---|---|---|");
    let groups: Vec<Vec<(&str, Config)>> = vec![
        vec![
            ("r2-q2-a-perdraw-64", Config { tex: Tex::PerDraw, ..Config::new(12, 12) }),
            ("r2-q2-b-array-64", Config { tex: Tex::Array, ..Config::new(12, 12) }),
            ("r2-q2-c-atlas-64", Config { tex: Tex::Atlas, ..Config::new(12, 12) }),
            ("r2-q2-b-array-inline", Config { list: List::InlineTyped, ..Config::new(12, 12) }),
            ("r2-q2-b-array-bytes", Config { list: List::InlineBytes, ..Config::new(12, 12) }),
            ("r2-q2-b-array-indirect", Config { list: List::Indirect, ..Config::new(12, 12) }),
            ("r2-q2-b-array-cullback", Config { cull: true, ..Config::new(12, 12) }),
            ("r2-q3-a4-msaa", Config { pipe: Pipe::A4, ..Config::new(12, 12) }),
            ("r2-q3-d1-frame", Config { pipe: Pipe::D1, ..Config::new(12, 12) }),
        ],
        vec![
            ("r2-q2-a-perdraw-512", Config { tex: Tex::PerDraw, tiles: 512, ..Config::new(300, 5000) }),
            ("r2-q2-b-array-512", Config { tex: Tex::Array, tiles: 512, ..Config::new(300, 5000) }),
            ("r2-q2-c-atlas-512", Config { tex: Tex::Atlas, tiles: 512, ..Config::new(300, 5000) }),
        ],
        vec![
            ("r2-q3-b0-hdr-tonemap", Config { pipe: Pipe::B0, ..Config::new(12, 12) }),
            ("r2-q3-b-bloom", Config { pipe: Pipe::B, ..Config::new(12, 12) }),
            ("r2-q3-c-bloom-msaa", Config { pipe: Pipe::C, ..Config::new(12, 12) }),
            ("r2-q3-d2-bloom-frame", Config { pipe: Pipe::D2, ..Config::new(12, 12) }),
            ("r2-q3-d2c-bloom-frame-msaa", Config { pipe: Pipe::D2C, ..Config::new(12, 12) }),
        ],
    ];
    for group in groups {
        let mut reference: Option<Image> = None;
        for (name, cfg) in group {
            match build(cfg) {
                Ok(mut stage) => {
                    let image = stage.capture(name);
                    let (mean, covered, worst, off) = difference(&image, reference.as_ref().unwrap_or(&image));
                    println!("| {name} | {covered:.1} | {mean:.3} | {worst} | {off:.2} |");
                    reference.get_or_insert(image);
                }
                Err(error) => println!("| {name} | refused: {error} |"),
            }
        }
    }
}

fn suite_baseline(run: &Run) {
    println!("\n## The frame's fixed cost: one draw-list pass with no draws, then one 300-triangle draw\n");
    print_header();
    run_row("empty pass -> Rgba8 tex -> quad", Config { stress_indices: Some(0), ..Config::new(1, 1) }, run);
    run_row("empty pass -> frame target", Config { stress_indices: Some(0), pipe: Pipe::D1, ..Config::new(1, 1) }, run);
    run_row("one draw -> Rgba8 tex -> quad", Config::new(1, 1), run);
}

fn suite_limits() {
    let mut harness = SubstrateHarness::builder().size(WIDTH, HEIGHT).with_render().build().expect("boot");
    let reply = request!(harness, &SpikeLimits {}, SpikeLimitsResult);
    println!("\n## Device limits and features as the render device was created\n\n{}", reply.text);
}

pub fn main(args: &[String]) {
    let mut run = Run { frames: 200, warmup: 30 };
    let mut models = vec![300usize, 1000];
    let mut instances = vec![5000usize, 20_000, 100_000];
    let mut layers = 2048u32;
    let suite = args.first().map_or("all", String::as_str);
    let mut rest = args.iter().skip(1);
    while let Some(flag) = rest.next() {
        let mut value = || rest.next().expect("flag takes a value");
        match flag.as_str() {
            "--frames" => run.frames = value().parse().expect("frames"),
            "--warmup" => run.warmup = value().parse().expect("warmup"),
            "--models" => models = value().split(',').map(|item| item.parse().expect("count")).collect(),
            "--instances" => instances = value().split(',').map(|item| item.parse().expect("count")).collect(),
            "--array-layers" => layers = value().parse().expect("layers"),
            "--light" => LIGHT.store(true, Ordering::Relaxed),
            "--spin" => spike_probe::SPIN_WAIT.store(true, Ordering::Relaxed),
            "--repeat" => REPEAT.store(value().parse().expect("repeat"), Ordering::Relaxed),
            other => panic!("unknown flag {other}"),
        }
    }
    spike_probe::ARRAY_LAYER_LIMIT.store(layers, Ordering::Relaxed);
    match suite {
        "q1" => suite_q1(&run),
        "q1change" => suite_q1change(),
        "q2" => suite_q2(&run, &models, &instances),
        "q3" => suite_q3(&run),
        "perdraw" => suite_perdraw(&run),
        "validate" => suite_validate(&run),
        "stress" => suite_stress(&run),
        "indirect" => suite_indirect(&run),
        "cull" => suite_cull(&run),
        "capture" => suite_capture(),
        "limits" => suite_limits(),
        "baseline" => suite_baseline(&run),
        other => panic!("unknown suite {other}"),
    }
}
