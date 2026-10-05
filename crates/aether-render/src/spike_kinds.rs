//! SPIKE-ONLY kinds (branch `spike/mesh-draw-path`, never on `main`): a
//! prototype "render pass that draws a list of instanced draws", used to
//! measure three open design questions. Nothing here is a proposed API.

use aether_data::Blob;
use serde::{Deserialize, Serialize};

use crate::kinds::{TextureFormat, VertexAttribute};

/// `SpikeDrawPass.target` value naming the frame's own colour target.
pub const SPIKE_FRAME_TARGET: u32 = u32::MAX;

/// Face culling a draw pipeline declares.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq)]
pub enum SpikeCull {
    None,
    Back,
}

/// How a draw pipeline's group 1 binds textures.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq)]
pub enum SpikeTextures {
    /// No group 1 at all.
    None,
    /// `texture_2d` + sampler, one bind group per distinct
    /// `SpikeDraw.texture_id`, cached.
    PerDraw,
    /// `texture_2d_array` + sampler, bound once per pass from
    /// `SpikeDrawPass.array_texture`.
    Array,
}

/// One entry of a draw list: 8 little-endian `u32`s, 32 bytes, which is
/// also its layout inside `SpikeDrawPass.draw_bytes`.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq, Default)]
pub struct SpikeDraw {
    pub geometry_id: u32,
    pub first_index: u32,
    pub index_count: u32,
    pub base_vertex: u32,
    pub instances_id: u32,
    pub first_instance: u32,
    pub instance_count: u32,
    /// Registry texture id under `SpikeTextures::PerDraw`; ignored
    /// otherwise.
    pub texture_id: u32,
}

/// Reply of every spike create.
#[aether_data::kind(name = "aether.render.spike.created")]
pub enum SpikeCreated {
    Ok { id: u32 },
    Err { error: String },
}

/// An instance buffer: `data` is `count * stride(layout)` bytes, bound at
/// vertex slot 1 with `VertexStepMode::Instance`, persistent.
#[aether_data::kind(name = "aether.render.spike.create_instances")]
pub struct SpikeCreateInstances {
    pub layout: Vec<VertexAttribute>,
    pub data: Blob,
}

/// Overwrite instances `first_instance..` of an instance buffer in place.
#[aether_data::kind(name = "aether.render.spike.update_instances")]
pub struct SpikeUpdateInstances {
    pub instances_id: u32,
    pub first_instance: u32,
    #[serde(with = "aether_data::bytes")]
    pub data: Vec<u8>,
}

/// An `Rgba8` `texture_2d_array` of `layers` tiles, `width * height * 4`
/// bytes each, layer after layer.
#[aether_data::kind(name = "aether.render.spike.create_texture_array")]
pub struct SpikeCreateTextureArray {
    pub width: u32,
    pub height: u32,
    pub layers: u32,
    pub pixels: Blob,
}

/// A draw pipeline: authored vertex + fragment entry points over a
/// per-vertex layout (slot 0) and a per-instance layout (slot 1).
#[aether_data::kind(name = "aether.render.spike.create_pipeline")]
pub struct SpikeCreatePipeline {
    pub wgsl: String,
    pub vertex_entry: String,
    pub fragment_entry: String,
    pub vertex_layout: Vec<VertexAttribute>,
    pub instance_layout: Vec<VertexAttribute>,
    pub textures: SpikeTextures,
    /// Colour format of a texture target; ignored when `to_frame`.
    pub target_format: TextureFormat,
    /// Built against the frame's colour format, sample count and depth.
    pub to_frame: bool,
    /// 1 or 4; a texture target with 4 renders into an executor-owned
    /// multisampled attachment resolved into the target.
    pub samples: u32,
    pub cull: SpikeCull,
    pub depth: bool,
    pub uniform_bytes: u32,
}

/// A retained draw list, resolved and validated once against
/// `pipeline_id`; it keeps every buffer and bind group it names alive.
/// `indirect` additionally writes one indexed-indirect args buffer for
/// the whole list (every draw must then share one geometry, one instance
/// buffer and one texture), drawn with `multi_draw_indexed_indirect`.
#[aether_data::kind(name = "aether.render.spike.create_draw_list")]
pub struct SpikeCreateDrawList {
    pub pipeline_id: u32,
    pub indirect: bool,
    pub draws: Vec<SpikeDraw>,
}

/// Replace entries `first..first + draws.len()` of a retained list.
#[aether_data::kind(name = "aether.render.spike.patch_draw_list")]
pub struct SpikePatchDrawList {
    pub list_id: u32,
    pub first: u32,
    pub draws: Vec<SpikeDraw>,
}

#[aether_data::kind(name = "aether.render.spike.destroy_draw_list")]
pub struct SpikeDestroyDrawList {
    pub list_id: u32,
}

/// One draw-list pass for the next frame: one `begin_render_pass`, then
/// every draw. The list is `list_id` (retained) when non-zero, else
/// `draw_bytes` (packed `SpikeDraw`s) when non-empty, else `draws`.
#[aether_data::kind(name = "aether.render.spike.draw_pass")]
pub struct SpikeDrawPass {
    pub pipeline_id: u32,
    /// A writable registry texture id, or `SPIKE_FRAME_TARGET`.
    pub target: u32,
    pub clear: bool,
    pub array_texture: u32,
    #[serde(with = "aether_data::bytes")]
    pub uniforms: Vec<u8>,
    pub list_id: u32,
    pub draws: Vec<SpikeDraw>,
    #[serde(with = "aether_data::bytes")]
    pub draw_bytes: Vec<u8>,
}

/// Device limits the spike reports.
#[aether_data::kind(name = "aether.render.spike.limits")]
pub struct SpikeLimits {}

#[aether_data::kind(name = "aether.render.spike.limits_result")]
pub struct SpikeLimitsResult {
    pub text: String,
}
