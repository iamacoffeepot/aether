//! GPU bundle ([`RenderGpu`]) plus the shared record helpers for the
//! `aether.render` pumped runtime. `RenderGpu` holds the wgpu device,
//! queue, pipelines, and offscreen targets; the `record_*_batches` free
//! functions carry the ADR-0105 / ADR-0140 realize-then-expand logic the
//! pumped runtime records its owned-field accumulators through.

use std::sync::{Arc, Mutex};

use aether_kinds::{QuadScale, QuadSpace};
use aether_substrate::render::{
    CompositeBlend, MATERIAL_VERTEX_STRIDE, MATERIAL_VERTICES_PER_RECT, MaterialDraw, MaterialPassDraw,
    MaterialPassRecord, MaterialPipelines, OverlayDraw, OverlaySource, Pipeline, QUAD_VERTEX_BUFFER_BYTES,
    QUAD_VERTEX_STRIDE, QUAD_VERTICES_PER_QUAD, QUAD_VERTICES_PER_TRIANGLE, QuadPipeline, SHAPE_VERTEX_BUFFER_BYTES,
    SHAPE_VERTEX_STRIDE, ShapeParams, Targets, TextureBindings, build_main_pipeline, build_material_pipelines,
    build_quad_pipeline, build_texture_bindings, push_coverage_params, push_material_rect_vertices,
    push_screen_quad_vertices, push_screen_shape_vertices, push_screen_triangle_vertices, push_textured_params,
    push_world_quad_vertices, push_world_shape_vertices, push_world_triangle_vertices, record_material_pass,
    record_quad_overlay_pass,
};

use super::material::{MaterialBatch, accepts_coverage_texture};
use super::quad::{OverlayGeometry, QuadBatch};
use super::texture::TextureRegistry;
use crate::{DrawShapes, DrawTexturedQuads, QuadBlend, Shape};

/// The mail vocabulary's blend, as the record layer's selector. Two
/// enums rather than one because the substrate's render module owns no
/// kind types — the mapping is this one function.
fn composite_blend(blend: QuadBlend) -> CompositeBlend {
    match blend {
        QuadBlend::Straight => CompositeBlend::Straight,
        QuadBlend::Premultiplied => CompositeBlend::Premultiplied,
    }
}

/// The world quad path's scale factor for a `QuadSpace::World` batch:
/// `k < 0` selects Pixels mode (the shader uses `clip.w` for constant
/// on-screen size); `k > 0` is the Distance-mode reference distance
/// (the label shrinks with depth, holding its size at that distance).
fn world_scale_factor(scale: &QuadScale) -> f32 {
    match scale {
        QuadScale::Pixels => -1.0_f32,
        QuadScale::Distance { reference_distance } => *reference_distance,
    }
}

/// A mail-vocabulary [`Shape`] as the vertex writer's parameters: each
/// absent part becomes a zero-alpha colour with a zero width, which the
/// fragment stage composes nothing for.
fn shape_params(shape: &Shape) -> ShapeParams {
    ShapeParams {
        rect: [shape.x, shape.y, shape.width, shape.height],
        corner_radius: shape.corner_radius,
        fill: shape.fill.map_or([0.0; 4], aether_math::Rgba::to_array),
        stroke_width: shape.stroke.as_ref().map_or(0.0, |stroke| stroke.width_pixels),
        stroke: shape.stroke.as_ref().map_or([0.0; 4], |stroke| stroke.color.to_array()),
        shadow_blur: shape.shadow.as_ref().map_or(0.0, |shadow| shadow.blur_pixels),
        shadow_offset: shape.shadow.as_ref().map_or([0.0; 2], |shadow| shadow.offset),
        shadow: shape.shadow.as_ref().map_or([0.0; 4], |shadow| shadow.color.to_array()),
        uv_rect: shape
            .texture
            .as_ref()
            .map_or([0.0, 0.0, 1.0, 1.0], |texture| [texture.u0, texture.v0, texture.u1, texture.v1]),
        texture_premultiplied: shape
            .texture
            .as_ref()
            .is_some_and(|texture| matches!(texture.blend, QuadBlend::Premultiplied)),
    }
}

/// The registered texture a shape samples inside its fill, or `None` when
/// it samples nothing. The run key the shape expansion splits a batch on,
/// so consecutive shapes over one texture cost one draw.
fn shape_texture_id(shape: &Shape) -> Option<u32> {
    shape.texture.as_ref().map(|texture| texture.texture_id)
}

/// The group-1 bind group an overlay draw samples `texture_id` through,
/// or `None` when the draw has to be dropped: the id is unregistered, the
/// texture has not been realized on the GPU yet, or its format is a
/// non-filterable data plane — which binds through the non-filtering
/// layout no overlay pipeline was built against (ADR-0170), so drawing it
/// would fail wgpu validation rather than look wrong. `verb` names the
/// mail kind in the warn so the log says which sender to fix.
fn sampled_bind_group<'a>(
    registry: &'a TextureRegistry,
    texture_id: u32,
    verb: &'static str,
) -> Option<&'a wgpu::BindGroup> {
    let entry = registry.entries.get(&texture_id)?;
    if !entry.format.filterable() {
        tracing::warn!(
            target: "aether_render",
            texture_id,
            verb,
            format = ?entry.format,
            "overlay draw over a non-filterable data-plane texture; dropping it",
        );
        return None;
    }
    Some(entry.realized.as_ref()?.bind_group())
}

/// The `SubstrateHarness`-only committed-overlay sinks
/// [`record_overlay_batches`] fills: the quad batches and the shape batches
/// that survived one record, each as its public mail shape.
pub(super) struct OverlayObservation<'a> {
    pub quads: &'a Mutex<Vec<DrawTexturedQuads>>,
    pub shapes: &'a Mutex<Vec<DrawShapes>>,
}

/// The vertex index `bytes` into the shape vertex buffer names. The buffer
/// is capped at [`SHAPE_VERTEX_BUFFER_BYTES`], so the count fits a draw
/// range's `u32` with room.
fn shape_vertex_index(bytes: usize) -> u32 {
    let stride = usize::try_from(SHAPE_VERTEX_STRIDE).expect("the shape vertex stride is a small constant");
    u32::try_from(bytes / stride).expect("shape vertex bytes are capped well under u32::MAX vertices")
}

/// Mirror the low-level overlay pass's scissor rejection without moving that
/// validation earlier in the production render path. This runs only when
/// `SubstrateHarness` has installed an observation sink; keep its arithmetic aligned
/// with `aether_substrate::render::quad::clamped_scissor`.
#[allow(clippy::cast_precision_loss)]
fn overlay_clip_is_visible(clip: Option<[f32; 4]>, target_width: u32, target_height: u32) -> bool {
    let Some([x, y, width, height]) = clip else {
        return true;
    };
    if !x.is_finite() || !y.is_finite() || !width.is_finite() || !height.is_finite() {
        return false;
    }
    let min_x = x.max(0.0).min(target_width as f32).floor();
    let min_y = y.max(0.0).min(target_height as f32).floor();
    let max_x = (x + width).max(0.0).min(target_width as f32).ceil();
    let max_y = (y + height).max(0.0).min(target_height as f32).ceil();
    max_x > min_x && max_y > min_y
}

/// Expand and record the textured-quad overlay batches (ADR-0105) into
/// `encoder`. The pumped render runtime records its owned-field quad
/// accumulator through here, so the realize-then-expand logic lives once.
/// `targets` and `registry` are the already-borrowed offscreen targets and
/// texture registry; `observation` is the `SubstrateHarness`-only pair of
/// committed-overlay sinks — quad batches and shape batches — (production
/// passes `None`).
///
/// Two-pass texture realization + quad expansion in a single function
/// avoids threading split borrows through multiple helpers; the line
/// count reflects the World/Screen branching added in #1699.
#[allow(clippy::too_many_lines)]
pub(super) fn record_overlay_batches(
    gpu: &RenderGpu,
    encoder: &mut wgpu::CommandEncoder,
    targets: &Targets,
    registry: &mut TextureRegistry,
    batches: &[QuadBatch],
    view_proj: [f32; 16],
    observation: Option<OverlayObservation<'_>>,
) {
    if batches.is_empty() {
        if let Some(observation) = observation {
            observation.quads.lock().expect("mutex poisoned; fail-fast per ADR-0063").clear();
            observation.shapes.lock().expect("mutex poisoned; fail-fast per ADR-0063").clear();
        }
        return;
    }

    #[allow(clippy::cast_precision_loss)]
    let viewport = [targets.width() as f32, targets.height() as f32];

    // First pass: realize / re-upload every texture the frame
    // references (Screen and World batches share the same atlas),
    // mutably borrowing the registry. A quad or triangle batch names one
    // texture; a shape batch names one per shape that carries an image,
    // and none for the shapes that do not.
    let mut realize = |texture_id: u32, verb: &'static str| {
        if let Some(entry) = registry.entries.get_mut(&texture_id) {
            entry.ensure_realized(&gpu.device, &gpu.queue, &gpu.texture_bindings);
        } else {
            tracing::warn!(
                target: "aether_render",
                texture_id,
                verb,
                "overlay draw for unknown texture id; dropping it",
            );
        }
    };
    for batch in batches {
        if let OverlayGeometry::Shapes { shapes, .. } = &batch.geometry {
            for texture_id in shapes.iter().filter_map(shape_texture_id) {
                realize(texture_id, "draw_shapes");
            }
        } else {
            realize(batch.texture_id, "draw_textured_quads");
        }
    }

    // Second pass: expand quads into vertices and build the draw
    // list, immutably borrowing each realized texture's bind group.
    // Shapes expand into their own buffer (ADR-0213) but take their draw
    // position in the one list, so painter order interleaves the two.
    let mut vertex_bytes = Vec::new();
    let mut shape_vertex_bytes = Vec::new();
    let mut draws: Vec<OverlayDraw<'_>> = Vec::new();
    for batch in batches {
        let clip = batch.clip.as_ref().map(|clip| [clip.x, clip.y, clip.width, clip.height]);
        if let OverlayGeometry::Shapes { space, shapes } = &batch.geometry {
            // One draw per run of consecutive shapes over the same texture
            // (or over none), so a batch mixing plates and images keeps its
            // authored painter order at the cost of one draw per change.
            for run in shapes.chunk_by(|a, b| shape_texture_id(a) == shape_texture_id(b)) {
                let texture = match run.first().and_then(shape_texture_id) {
                    None => None,
                    Some(texture_id) => match sampled_bind_group(registry, texture_id, "draw_shapes") {
                        Some(bind_group) => Some(bind_group),
                        None => continue,
                    },
                };
                let first_vertex = shape_vertex_index(shape_vertex_bytes.len());
                match space {
                    QuadSpace::Screen => {
                        for shape in run {
                            push_screen_shape_vertices(&mut shape_vertex_bytes, &shape_params(shape));
                        }
                    }
                    QuadSpace::World { anchor, scale } => {
                        let k = world_scale_factor(scale);
                        for shape in run {
                            push_world_shape_vertices(&mut shape_vertex_bytes, *anchor, &shape_params(shape), k);
                        }
                    }
                }
                let vertex_count = shape_vertex_index(shape_vertex_bytes.len()) - first_vertex;
                if vertex_count > 0 {
                    draws.push(OverlayDraw {
                        source: OverlaySource::Shapes { texture },
                        first_vertex,
                        vertex_count,
                        clip,
                    });
                }
            }
            continue;
        }
        let Some(bind_group) = sampled_bind_group(registry, batch.texture_id, "draw_textured_quads") else {
            continue;
        };
        #[allow(clippy::cast_possible_truncation)]
        let first_vertex = (vertex_bytes.len() / QUAD_VERTEX_STRIDE as usize) as u32;
        let (vertices, blend) = match &batch.geometry {
            OverlayGeometry::Quads { space: QuadSpace::Screen, blend, quads } => {
                for quad in quads {
                    push_screen_quad_vertices(
                        &mut vertex_bytes,
                        [quad.x, quad.y, quad.width, quad.height],
                        [quad.u0, quad.v0, quad.u1, quad.v1],
                        quad.tint.to_array(),
                    );
                }
                (quads.len() * QUAD_VERTICES_PER_QUAD, composite_blend(*blend))
            }
            OverlayGeometry::Quads { space: QuadSpace::World { anchor, scale }, blend, quads } => {
                let k = world_scale_factor(scale);
                for quad in quads {
                    push_world_quad_vertices(
                        &mut vertex_bytes,
                        *anchor,
                        [quad.x, quad.y, quad.width, quad.height],
                        [quad.u0, quad.v0, quad.u1, quad.v1],
                        quad.tint.to_array(),
                        k,
                    );
                }
                (quads.len() * QUAD_VERTICES_PER_QUAD, composite_blend(*blend))
            }
            OverlayGeometry::ScreenTriangles { space, triangles } => {
                for triangle in triangles {
                    let corners = [&triangle.a, &triangle.b, &triangle.c];
                    let positions = corners.map(|corner| [corner.x, corner.y]);
                    let tints = corners.map(|corner| corner.color.to_array());
                    match space {
                        QuadSpace::Screen => push_screen_triangle_vertices(&mut vertex_bytes, positions, tints),
                        QuadSpace::World { anchor, scale } => push_world_triangle_vertices(
                            &mut vertex_bytes,
                            *anchor,
                            positions,
                            tints,
                            world_scale_factor(scale),
                        ),
                    }
                }
                // Flat per-vertex colours over the reserved white texture:
                // the caller composited no image, so there is nothing that
                // could already be premultiplied.
                (triangles.len() * QUAD_VERTICES_PER_TRIANGLE, CompositeBlend::Straight)
            }
            OverlayGeometry::Shapes { .. } => unreachable!("shape batches are expanded above"),
        };
        #[allow(clippy::cast_possible_truncation)]
        let vertex_count = vertices as u32;
        if vertex_count == 0 {
            continue;
        }
        draws.push(OverlayDraw {
            source: OverlaySource::Textured { bind_group, blend },
            first_vertex,
            vertex_count,
            clip,
        });
    }

    record_quad_overlay_pass(
        &gpu.queue,
        encoder,
        &gpu.quad_pipeline,
        targets,
        &vertex_bytes,
        &shape_vertex_bytes,
        &draws,
        viewport,
        view_proj,
    );

    if let Some(observation) = observation {
        let mut recorded = Vec::new();
        let mut recorded_shapes = Vec::new();
        let pass_recorded =
            vertex_bytes.len() <= QUAD_VERTEX_BUFFER_BYTES && shape_vertex_bytes.len() <= SHAPE_VERTEX_BUFFER_BYTES;
        if pass_recorded {
            for batch in batches {
                if let OverlayGeometry::Shapes { space, shapes } = &batch.geometry {
                    let clip = batch.clip.as_ref().map(|clip| [clip.x, clip.y, clip.width, clip.height]);
                    if !shapes.is_empty() && overlay_clip_is_visible(clip, targets.width(), targets.height()) {
                        recorded_shapes.push(DrawShapes {
                            space: space.clone(),
                            clip: batch.clip.clone(),
                            shapes: shapes.clone(),
                        });
                    }
                    continue;
                }
                // The sink's element is `DrawTexturedQuads`, so a
                // screen-triangle or shape batch has no quad list to
                // report here. The snapshot is a quad-batch view of the
                // committed overlay, and the pixels are what a triangle or
                // shape scenario asserts on.
                let OverlayGeometry::Quads { space, blend, quads } = &batch.geometry else {
                    continue;
                };
                let clip = batch.clip.as_ref().map(|clip| [clip.x, clip.y, clip.width, clip.height]);
                let is_recorded = registry
                    .entries
                    .get(&batch.texture_id)
                    .is_some_and(|entry| entry.realized.is_some() && entry.format.filterable())
                    && !quads.is_empty()
                    && overlay_clip_is_visible(clip, targets.width(), targets.height());
                if is_recorded {
                    recorded.push(DrawTexturedQuads {
                        texture_id: batch.texture_id,
                        space: space.clone(),
                        clip: batch.clip.clone(),
                        blend: *blend,
                        quads: quads.clone(),
                    });
                }
            }
        }
        *observation.quads.lock().expect("mutex poisoned; fail-fast per ADR-0063") = recorded;
        *observation.shapes.lock().expect("mutex poisoned; fail-fast per ADR-0063") = recorded_shapes;
    }
}

/// Expand and record the depth-tested material batches (ADR-0140) into
/// `encoder`. Records the pumped runtime's owned-field material accumulator,
/// so the realize-then-expand logic lives once. `targets` and `registry` are
/// the already-borrowed offscreen targets and texture registry; the camera
/// uniform is expected to have been written by an earlier world pass this
/// frame (the material pipeline shares the main pipeline's camera bind
/// group).
#[allow(clippy::too_many_lines)]
pub(super) fn record_material_batches(
    gpu: &RenderGpu,
    encoder: &mut wgpu::CommandEncoder,
    targets: &Targets,
    registry: &mut TextureRegistry,
    batches: &[MaterialBatch],
) {
    if batches.is_empty() {
        return;
    }

    for batch in batches {
        let texture_id = match batch {
            MaterialBatch::Textured { texture_id, .. } | MaterialBatch::Coverage { texture_id, .. } => *texture_id,
        };
        if let Some(entry) = registry.entries.get_mut(&texture_id) {
            entry.ensure_realized(&gpu.device, &gpu.queue, &gpu.texture_bindings);
        } else {
            tracing::warn!(
                target: "aether_render",
                texture_id,
                "material draw for unknown texture id; dropping the batch",
            );
        }
    }

    let mut vertex_bytes = Vec::new();
    let mut textured_params = Vec::new();
    let mut coverage_params = Vec::new();
    let mut draws = Vec::new();
    let vertex_count = u32::try_from(MATERIAL_VERTICES_PER_RECT).expect("material rect vertex count fits u32");
    for batch in batches {
        match batch {
            MaterialBatch::Textured { texture_id, blend, rects } => {
                let Some(entry) = registry.entries.get(texture_id) else {
                    continue;
                };
                if !entry.format.filterable() {
                    // Same layout incompatibility as the overlay pass: the
                    // textured material pipeline is built against the
                    // filtering layout (ADR-0170).
                    tracing::warn!(
                        target: "aether_render",
                        texture_id,
                        format = ?entry.format,
                        "textured material over a non-filterable data-plane texture; dropping the batch",
                    );
                    continue;
                }
                let Some(realized) = entry.realized.as_ref() else {
                    continue;
                };
                for rect in rects {
                    let Some(params_offset) = push_textured_params(&mut textured_params, rect.tint.to_array()) else {
                        tracing::warn!(
                            target: "aether_render",
                            texture_id,
                            "textured material params overflow; dropping rect",
                        );
                        continue;
                    };
                    #[allow(clippy::cast_possible_truncation)]
                    let first_vertex = (vertex_bytes.len() / MATERIAL_VERTEX_STRIDE as usize) as u32;
                    push_material_rect_vertices(
                        &mut vertex_bytes,
                        [rect.rect.x, rect.rect.y, rect.rect.z],
                        rect.rect.right,
                        rect.rect.up,
                        [rect.rect.width, rect.rect.height],
                        [rect.u0, rect.v0, rect.u1, rect.v1],
                    );
                    draws.push(MaterialPassDraw::Textured(MaterialDraw {
                        blend: composite_blend(*blend),
                        bind_group: realized.bind_group(),
                        first_vertex,
                        vertex_count,
                        params_offset,
                    }));
                }
            }
            MaterialBatch::Coverage { texture_id, rects } => {
                let Some(entry) = registry.entries.get(texture_id) else {
                    continue;
                };
                if !accepts_coverage_texture(entry.format) {
                    tracing::warn!(
                        target: "aether_render",
                        texture_id,
                        ?entry.format,
                        "coverage material requires an R8 texture; dropping the batch",
                    );
                    continue;
                }
                let Some(realized) = entry.realized.as_ref() else {
                    continue;
                };
                for rect in rects {
                    let Some(params_offset) = push_coverage_params(
                        &mut coverage_params,
                        rect.body_color.to_array(),
                        rect.rim_color.to_array(),
                        rect.rim_width,
                    ) else {
                        tracing::warn!(
                            target: "aether_render",
                            texture_id,
                            "coverage material params overflow; dropping rect",
                        );
                        continue;
                    };
                    #[allow(clippy::cast_possible_truncation)]
                    let first_vertex = (vertex_bytes.len() / MATERIAL_VERTEX_STRIDE as usize) as u32;
                    push_material_rect_vertices(
                        &mut vertex_bytes,
                        [rect.rect.x, rect.rect.y, rect.rect.z],
                        rect.rect.right,
                        rect.rect.up,
                        [rect.rect.width, rect.rect.height],
                        [0.0, 0.0, 1.0, 1.0],
                    );
                    draws.push(MaterialPassDraw::Coverage(MaterialDraw {
                        // A coverage material builds its colour from
                        // bands rather than compositing a source image.
                        blend: CompositeBlend::Straight,
                        bind_group: realized.bind_group(),
                        first_vertex,
                        vertex_count,
                        params_offset,
                    }));
                }
            }
        }
    }

    record_material_pass(
        encoder,
        MaterialPassRecord {
            queue: &gpu.queue,
            pipeline: &gpu.material_pipelines,
            main_pipeline: &gpu.pipeline,
            targets,
            vertex_bytes: &vertex_bytes,
            draws: &draws,
            textured_params: &textured_params,
            coverage_params: &coverage_params,
        },
    );
}

/// Bundle of wgpu resources the pumped render runtime owns after its lazy
/// boot. Constructed from a wgpu device + queue obtained via
/// `Adapter::request_device` (desktop: with surface compatibility; harness:
/// offscreen-only). Holds the pipeline + offscreen targets so the runtime
/// can record draws and capture copies from its owned accumulators.
pub struct RenderGpu {
    pub device: Arc<wgpu::Device>,
    pub queue: Arc<wgpu::Queue>,
    pub pipeline: Pipeline,
    /// Shared texture+sampler bindings used by every texture-sampling
    /// pipeline. The quad overlay owns the first consumer; material
    /// pipelines added by ADR-0140 use the same layout object.
    pub texture_bindings: TextureBindings,
    /// Textured-quad overlay pipeline (ADR-0105). Built alongside the
    /// main pipeline so the overlay pass can draw the accumulated quads
    /// into the same offscreen target after the world pass.
    pub quad_pipeline: QuadPipeline,
    /// Depth-tested material pipelines (ADR-0140), recorded after the
    /// main pass and before the quad overlay.
    pub material_pipelines: MaterialPipelines,
    pub targets: Mutex<Targets>,
    pub color_format: wgpu::TextureFormat,
}

impl RenderGpu {
    /// Build the standard render pipeline + offscreen targets at the
    /// given size. `polygon_mode` is `Fill` for the normal case; a
    /// `AETHER_WIREFRAME=line` boot passes `Line` so the main pipeline
    /// draws as wireframe instead of building a separate overlay pipeline.
    /// `vertex_buffer_bytes` sizes the per-frame GPU vertex buffer — the
    /// runtime passes its resolved vertex-buffer cap so the buffer matches
    /// the accumulator's truncation cap.
    #[must_use]
    pub fn new(
        device: Arc<wgpu::Device>,
        queue: Arc<wgpu::Queue>,
        color_format: wgpu::TextureFormat,
        width: u32,
        height: u32,
        polygon_mode: wgpu::PolygonMode,
        vertex_buffer_bytes: usize,
    ) -> Self {
        let pipeline = build_main_pipeline(&device, &queue, color_format, polygon_mode, vertex_buffer_bytes);
        let texture_bindings = build_texture_bindings(&device);
        let quad_pipeline = build_quad_pipeline(&device, color_format, &texture_bindings);
        let material_pipelines =
            build_material_pipelines(&device, color_format, &pipeline.camera_bind_group_layout, &texture_bindings);
        let targets = Targets::new(&device, color_format, width, height);
        Self {
            device,
            queue,
            pipeline,
            texture_bindings,
            quad_pipeline,
            material_pipelines,
            targets: Mutex::new(targets),
            color_format,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Observation applies the same finite, clamped, non-empty scissor
    /// contract as the low-level overlay pass.
    #[test]
    fn overlay_observation_rejects_non_drawing_clips() {
        assert!(overlay_clip_is_visible(None, 64, 48));
        assert!(overlay_clip_is_visible(Some([-1.0, -1.0, 2.0, 2.0]), 64, 48));
        assert!(overlay_clip_is_visible(Some([63.5, 47.5, 1.0, 1.0]), 64, 48));
        assert!(!overlay_clip_is_visible(Some([64.0, 0.0, 1.0, 1.0]), 64, 48));
        assert!(!overlay_clip_is_visible(Some([0.0, 0.0, 0.0, 1.0]), 64, 48));
        assert!(!overlay_clip_is_visible(Some([f32::NAN, 0.0, 1.0, 1.0]), 64, 48));
    }
}
