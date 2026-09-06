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
    QUAD_VERTEX_STRIDE, QUAD_VERTICES_PER_QUAD, QUAD_VERTICES_PER_TRIANGLE, QuadPipeline, SHAPE_VERTEX_STRIDE,
    SHAPE_VERTICES_PER_SHAPE, ShapeParams, Targets, TextureBindings, build_main_pipeline, build_material_pipelines,
    build_quad_pipeline, build_texture_bindings, push_coverage_params, push_material_rect_vertices,
    push_screen_quad_vertices, push_screen_shape_vertices, push_screen_triangle_vertices, push_textured_params,
    push_world_quad_vertices, push_world_shape_vertices, record_material_pass, record_quad_overlay_pass,
};

use super::material::{MaterialBatch, accepts_coverage_texture};
use super::quad::{OverlayGeometry, QuadBatch};
use super::texture::TextureRegistry;
use crate::{DrawTexturedQuads, QuadBlend, Shape};

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
    }
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
/// texture registry; `observation` is the `SubstrateHarness`-only
/// committed-overlay sink (production passes `None`).
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
    observation: Option<&Mutex<Vec<DrawTexturedQuads>>>,
) {
    if batches.is_empty() {
        if let Some(observed) = observation {
            observed.lock().expect("mutex poisoned; fail-fast per ADR-0063").clear();
        }
        return;
    }

    #[allow(clippy::cast_precision_loss)]
    let viewport = [targets.width() as f32, targets.height() as f32];

    // First pass: realize / re-upload every texture the frame
    // references (Screen and World batches share the same atlas),
    // mutably borrowing the registry. A shape batch samples nothing.
    for batch in batches {
        if matches!(batch.geometry, OverlayGeometry::Shapes { .. }) {
            continue;
        }
        if let Some(entry) = registry.entries.get_mut(&batch.texture_id) {
            entry.ensure_realized(&gpu.device, &gpu.queue, &gpu.texture_bindings);
        } else {
            tracing::warn!(
                target: "aether_render",
                texture_id = batch.texture_id,
                "draw_textured_quads for unknown texture id; dropping the batch",
            );
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
            #[allow(clippy::cast_possible_truncation)]
            let first_vertex = (shape_vertex_bytes.len() / SHAPE_VERTEX_STRIDE as usize) as u32;
            match space {
                QuadSpace::Screen => {
                    for shape in shapes {
                        push_screen_shape_vertices(&mut shape_vertex_bytes, &shape_params(shape));
                    }
                }
                QuadSpace::World { anchor, scale } => {
                    let k = world_scale_factor(scale);
                    for shape in shapes {
                        push_world_shape_vertices(&mut shape_vertex_bytes, *anchor, &shape_params(shape), k);
                    }
                }
            }
            #[allow(clippy::cast_possible_truncation)]
            let vertex_count = (shapes.len() * SHAPE_VERTICES_PER_SHAPE) as u32;
            if vertex_count > 0 {
                draws.push(OverlayDraw { source: OverlaySource::Shapes, first_vertex, vertex_count, clip });
            }
            continue;
        }
        let Some(entry) = registry.entries.get(&batch.texture_id) else {
            continue;
        };
        if !entry.format.filterable() {
            // A non-filterable data plane binds through the non-filtering
            // layout, which the overlay pipeline was not built against
            // (ADR-0170) — drawing it would fail wgpu validation.
            tracing::warn!(
                target: "aether_render",
                texture_id = batch.texture_id,
                format = ?entry.format,
                "draw_textured_quads over a non-filterable data-plane texture; dropping the batch",
            );
            continue;
        }
        let Some(realized) = entry.realized.as_ref() else {
            continue;
        };
        #[allow(clippy::cast_possible_truncation)]
        let first_vertex = (vertex_bytes.len() / QUAD_VERTEX_STRIDE as usize) as u32;
        let vertices = match &batch.geometry {
            OverlayGeometry::Quads { space: QuadSpace::Screen, quads } => {
                for quad in quads {
                    push_screen_quad_vertices(
                        &mut vertex_bytes,
                        [quad.x, quad.y, quad.width, quad.height],
                        [quad.u0, quad.v0, quad.u1, quad.v1],
                        quad.tint.to_array(),
                    );
                }
                quads.len() * QUAD_VERTICES_PER_QUAD
            }
            OverlayGeometry::Quads { space: QuadSpace::World { anchor, scale }, quads } => {
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
                quads.len() * QUAD_VERTICES_PER_QUAD
            }
            OverlayGeometry::ScreenTriangles(triangles) => {
                for triangle in triangles {
                    let corners = [&triangle.a, &triangle.b, &triangle.c];
                    push_screen_triangle_vertices(
                        &mut vertex_bytes,
                        corners.map(|corner| [corner.x, corner.y]),
                        corners.map(|corner| corner.color.to_array()),
                    );
                }
                triangles.len() * QUAD_VERTICES_PER_TRIANGLE
            }
            OverlayGeometry::Shapes { .. } => unreachable!("shape batches are expanded above"),
        };
        #[allow(clippy::cast_possible_truncation)]
        let vertex_count = vertices as u32;
        if vertex_count == 0 {
            continue;
        }
        draws.push(OverlayDraw {
            source: OverlaySource::Textured { bind_group: realized.bind_group(), blend: composite_blend(batch.blend) },
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

    if let Some(observed) = observation {
        let mut recorded = Vec::new();
        if vertex_bytes.len() <= QUAD_VERTEX_BUFFER_BYTES {
            for batch in batches {
                // The sink's element is `DrawTexturedQuads`, so a
                // screen-triangle or shape batch has no quad list to
                // report here. The snapshot is a quad-batch view of the
                // committed overlay, and the pixels are what a triangle or
                // shape scenario asserts on.
                let OverlayGeometry::Quads { space, quads } = &batch.geometry else {
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
                        blend: batch.blend,
                        quads: quads.clone(),
                    });
                }
            }
        }
        *observed.lock().expect("mutex poisoned; fail-fast per ADR-0063") = recorded;
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
