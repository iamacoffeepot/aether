//! Shape overlay pipeline (ADR-0213). A third pipeline in
//! the overlay pass beside the textured and premultiplied quad
//! pipelines: each shape is one axis-aligned box — with a corner radius,
//! an optional fill, an optional inside stroke, and an optional shadow —
//! expanded to one quad grown by its shadow extent, and `shape.wgsl`
//! evaluates a rounded-box signed distance per pixel. The batch takes the
//! same painter position and the same scissor as any other overlay draw:
//! it is one more [`super::quad::OverlayDraw`] source, not a pass and not
//! a layer.
//!
//! A batch draws in either of the quad overlay's two projections: `Screen`
//! reads the box's coordinates as absolute window pixels, `World` reads
//! them as pixel offsets from an anchor projected through `view_proj`. The
//! two share one vertex layout and one shader, differing only in the
//! `is_screen` flag each vertex carries.
//!
//! The vocabulary is fixed and substrate-owned: callers supply six
//! numbers and three colours, never WGSL, so the overlay lane stays a
//! closed contract the widget kit's hole cutting can reason about.

/// Bytes per expanded shape vertex: `anchor vec3<f32>` (12) + `offset_px
/// vec2<f32>` (8) + `local vec2<f32>` (8) + `half_size vec2<f32>` (8) +
/// `params vec4<f32>` (16) + `fill vec4<f32>` (16) + `stroke vec4<f32>`
/// (16) + `shadow vec4<f32>` (16) + `shadow_offset vec2<f32>` (8) +
/// `uv_rect vec4<f32>` (16) + `is_screen u32` (4) +
/// `texture_premultiplied u32` (4) = 132. [`push_screen_shape_vertices`]
/// and [`push_world_shape_vertices`] write exactly this stride per vertex,
/// textured or not: one layout serves both shape pipelines, so painter
/// order can interleave them inside one batch's vertex buffer.
pub const SHAPE_VERTEX_STRIDE: u64 = 132;

/// Vertices one shape expands to: two triangles, six vertices — the same
/// cornering as a quad, over the box grown by its shadow.
pub const SHAPE_VERTICES_PER_SHAPE: usize = 6;

/// Maximum size of the per-frame shape vertex buffer. The same 4 MiB cap
/// the quad overlay buffer carries; a frame whose expanded shape bytes
/// exceed it drops the overlay pass with a warn rather than overflow the
/// GPU buffer.
pub const SHAPE_VERTEX_BUFFER_BYTES: usize = 4 * 1024 * 1024;

/// Source for the shape overlay shader.
pub const SHAPE_SHADER_WGSL: &str = include_str!("shape.wgsl");

/// The margin, in pixels, a shape's expanded quad grows past the box and
/// its shadow on every side, so the outermost anti-aliased edge has
/// pixels to land on.
const SHAPE_EDGE_MARGIN: f32 = 1.0;

/// One shape's parameters, as the vertex writer takes them: the box, its
/// corner radius, and the three parts in premultiplication-ready linear
/// RGBA. An absent part is a colour with zero alpha and a zero width, so
/// the fragment stage composes nothing for it — the mail vocabulary's
/// `Option`s resolve to this before expansion.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShapeParams {
    /// `[x, y, width, height]` — top-left corner + size, in window pixels
    /// (Screen) or pixel offsets from the anchor (World).
    pub rect: [f32; 4],
    pub corner_radius: f32,
    pub fill: [f32; 4],
    pub stroke_width: f32,
    pub stroke: [f32; 4],
    pub shadow_blur: f32,
    pub shadow_offset: [f32; 2],
    pub shadow: [f32; 4],
    /// `[u0, v0, u1, v1]` — the texture sub-rect stretched across the box,
    /// read only by the textured pipeline. `[0, 0, 1, 1]` for an untextured
    /// shape, which never samples it.
    pub uv_rect: [f32; 4],
    /// Whether the sampled texel's colour was already scaled by its own
    /// coverage. Rides the vertex rather than a third pipeline, because the
    /// shader composes a premultiplied result either way and the two differ
    /// by one multiply.
    pub texture_premultiplied: bool,
}

impl ShapeParams {
    /// The quad this shape expands to: its box, unioned with its shadow's
    /// box (the box moved by the shadow offset and grown by the blur),
    /// then grown by one pixel of anti-aliasing margin. `[x0, y0, x1, y1]`.
    fn expanded_bounds(&self) -> [f32; 4] {
        let [x, y, width, height] = self.rect;
        let [offset_x, offset_y] = self.shadow_offset;
        let blur = self.shadow_blur.max(0.0);
        let casts = self.shadow[3] > 0.0;
        let (left, right, top, bottom) = if casts {
            (
                (blur - offset_x).max(0.0),
                (blur + offset_x).max(0.0),
                (blur - offset_y).max(0.0),
                (blur + offset_y).max(0.0),
            )
        } else {
            (0.0, 0.0, 0.0, 0.0)
        };
        [
            x - left - SHAPE_EDGE_MARGIN,
            y - top - SHAPE_EDGE_MARGIN,
            x + width + right + SHAPE_EDGE_MARGIN,
            y + height + bottom + SHAPE_EDGE_MARGIN,
        ]
    }
}

/// Owned GPU state for the shape overlay pipelines: the two render
/// pipelines and their shared per-frame vertex buffer. The viewport
/// uniform (group 0) is the quad overlay's — every overlay pipeline is
/// built against the one layout, so the pass binds it once and switches
/// pipelines freely.
///
/// Two variants because a shape either samples a texture inside its fill
/// or does not, and a pipeline layout that declares the group-1 texture
/// bind cannot draw without one bound. They share `vs_main` and the one
/// vertex layout; only the fragment entry point differs.
pub struct ShapePipeline {
    pub(super) plain: wgpu::RenderPipeline,
    pub(super) textured: wgpu::RenderPipeline,
    pub(super) vertex_buffer: wgpu::Buffer,
}

/// Build the shape overlay pipeline against the overlay pass's viewport
/// bind group layout. `color_format` matches the overlay pass's color
/// target.
#[must_use]
pub(super) fn build_shape_pipeline(
    device: &wgpu::Device,
    color_format: wgpu::TextureFormat,
    viewport_bind_group_layout: &wgpu::BindGroupLayout,
    texture_bind_group_layout: &wgpu::BindGroupLayout,
) -> ShapePipeline {
    let shader = super::overlay_shader_module(device, "aether shape shader", SHAPE_SHADER_WGSL);

    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("aether shape pipeline layout"),
        bind_group_layouts: &[Some(viewport_bind_group_layout)],
        immediate_size: 0,
    });
    let textured_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("aether textured shape pipeline layout"),
        bind_group_layouts: &[Some(viewport_bind_group_layout), Some(texture_bind_group_layout)],
        immediate_size: 0,
    });

    let vertex_layout = wgpu::VertexBufferLayout {
        array_stride: SHAPE_VERTEX_STRIDE,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: &[
            // anchor: vec3<f32> at offset 0
            wgpu::VertexAttribute { offset: 0, shader_location: 0, format: wgpu::VertexFormat::Float32x3 },
            // offset_px: vec2<f32> at offset 12
            wgpu::VertexAttribute { offset: 12, shader_location: 1, format: wgpu::VertexFormat::Float32x2 },
            // local: vec2<f32> at offset 20
            wgpu::VertexAttribute { offset: 20, shader_location: 2, format: wgpu::VertexFormat::Float32x2 },
            // half_size: vec2<f32> at offset 28
            wgpu::VertexAttribute { offset: 28, shader_location: 3, format: wgpu::VertexFormat::Float32x2 },
            // params (radius, stroke width, shadow blur, k): vec4<f32> at offset 36
            wgpu::VertexAttribute { offset: 36, shader_location: 4, format: wgpu::VertexFormat::Float32x4 },
            // fill: vec4<f32> at offset 52
            wgpu::VertexAttribute { offset: 52, shader_location: 5, format: wgpu::VertexFormat::Float32x4 },
            // stroke: vec4<f32> at offset 68
            wgpu::VertexAttribute { offset: 68, shader_location: 6, format: wgpu::VertexFormat::Float32x4 },
            // shadow: vec4<f32> at offset 84
            wgpu::VertexAttribute { offset: 84, shader_location: 7, format: wgpu::VertexFormat::Float32x4 },
            // shadow_offset: vec2<f32> at offset 100
            wgpu::VertexAttribute { offset: 100, shader_location: 8, format: wgpu::VertexFormat::Float32x2 },
            // uv_rect (u0, v0, u1, v1): vec4<f32> at offset 108
            wgpu::VertexAttribute { offset: 108, shader_location: 9, format: wgpu::VertexFormat::Float32x4 },
            // is_screen: u32 at offset 124
            wgpu::VertexAttribute { offset: 124, shader_location: 10, format: wgpu::VertexFormat::Uint32 },
            // texture_premultiplied: u32 at offset 128
            wgpu::VertexAttribute { offset: 128, shader_location: 11, format: wgpu::VertexFormat::Uint32 },
        ],
    };

    // The fragment stage composes shadow, fill, and stroke into one
    // premultiplied colour, so the target blends it as such — the textured
    // variant included, which premultiplies its sampled texel itself. Overlay
    // content draws over the resolved world pass with no depth interaction,
    // like the quad pipelines.
    let build = |label, layout, fragment_entry| {
        super::render_pipeline(
            device,
            super::RenderPipelineSpec {
                label,
                layout,
                shader: &shader,
                fragment_entry,
                vertex_layout: &vertex_layout,
                color_format,
                blend: wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING,
                depth: None,
            },
        )
    };
    let plain = build("aether shape pipeline", &pipeline_layout, "fs_main");
    let textured = build("aether textured shape pipeline", &textured_pipeline_layout, "fs_textured");

    let vertex_buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("aether shape vertex buffer"),
        size: SHAPE_VERTEX_BUFFER_BYTES as u64,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });

    ShapePipeline { plain, textured, vertex_buffer }
}

/// Push the six vertices for one screen-space shape into `out` as raw
/// bytes — each [`SHAPE_VERTEX_STRIDE`] bytes, `is_screen` set, the
/// anchor zeroed. The quad is the shape's box grown by its shadow extent
/// and the anti-aliasing margin; every vertex carries the shape's
/// parameters and its own position relative to the box centre, which the
/// fragment stage evaluates the distance field in.
pub fn push_screen_shape_vertices(out: &mut Vec<u8>, shape: &ShapeParams) {
    push_shape_vertices(out, [0.0; 3], shape, -1.0, true);
}

/// Push the six vertices for one world-anchored shape into `out` — the
/// same expansion as [`push_screen_shape_vertices`] with the box's pixel
/// coordinates read as offsets from `anchor`, and `k` the world scale
/// factor the quad overlay uses (`k < 0` Pixels mode, `k > 0` the
/// Distance-mode reference distance).
pub fn push_world_shape_vertices(out: &mut Vec<u8>, anchor: [f32; 3], shape: &ShapeParams, k: f32) {
    push_shape_vertices(out, anchor, shape, k, false);
}

fn push_shape_vertices(out: &mut Vec<u8>, anchor: [f32; 3], shape: &ShapeParams, k: f32, is_screen: bool) {
    let [x, y, width, height] = shape.rect;
    let centre = [width.mul_add(0.5, x), height.mul_add(0.5, y)];
    let half_size = [width * 0.5, height * 0.5];
    let [x0, y0, x1, y1] = shape.expanded_bounds();
    // Two triangles over the expanded quad, in the quad overlay's
    // cornering; cull mode is off so winding doesn't gate visibility.
    let corners = [(x0, y0), (x0, y1), (x1, y1), (x0, y0), (x1, y1), (x1, y0)];
    for (px, py) in corners {
        let floats: [f32; 31] = [
            anchor[0],
            anchor[1],
            anchor[2],
            px,
            py,
            px - centre[0],
            py - centre[1],
            half_size[0],
            half_size[1],
            shape.corner_radius.max(0.0),
            shape.stroke_width.max(0.0),
            shape.shadow_blur.max(0.0),
            k,
            shape.fill[0],
            shape.fill[1],
            shape.fill[2],
            shape.fill[3],
            shape.stroke[0],
            shape.stroke[1],
            shape.stroke[2],
            shape.stroke[3],
            shape.shadow[0],
            shape.shadow[1],
            shape.shadow[2],
            shape.shadow[3],
            shape.shadow_offset[0],
            shape.shadow_offset[1],
            shape.uv_rect[0],
            shape.uv_rect[1],
            shape.uv_rect[2],
            shape.uv_rect[3],
        ];
        out.extend_from_slice(bytemuck::cast_slice(&floats));
        out.extend_from_slice(bytemuck::cast_slice(&[u32::from(is_screen), u32::from(shape.texture_premultiplied)]));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(rect: [f32; 4]) -> ShapeParams {
        ShapeParams {
            rect,
            corner_radius: 0.0,
            fill: [1.0; 4],
            stroke_width: 0.0,
            stroke: [0.0; 4],
            shadow_blur: 0.0,
            shadow_offset: [0.0; 2],
            shadow: [0.0; 4],
            uv_rect: [0.0, 0.0, 1.0, 1.0],
            texture_premultiplied: false,
        }
    }

    /// Tripwire: the byte layout the pipeline's `VertexBufferLayout`
    /// describes is what the writer emits — six vertices of exactly the
    /// stride, so a field added to one side and not the other misaligns
    /// every attribute after it.
    #[test]
    fn a_shape_expands_to_six_vertices_of_the_declared_stride() {
        let mut out = Vec::new();
        push_screen_shape_vertices(&mut out, &plain([4.0, 8.0, 16.0, 12.0]));
        assert_eq!(u64::try_from(out.len()).expect("fits"), SHAPE_VERTICES_PER_SHAPE as u64 * SHAPE_VERTEX_STRIDE);
    }

    /// The expanded quad covers the shadow on the side the offset pushes it
    /// to and only the anti-aliasing margin on the side it pulls it from —
    /// a quad that stopped at the box would clip the shadow off, and one
    /// grown by blur plus offset on every side would rasterize empty pixels.
    #[test]
    fn the_expanded_quad_grows_by_the_shadow_where_the_shadow_falls() {
        let mut shape = plain([10.0, 10.0, 20.0, 20.0]);
        shape.shadow = [0.0, 0.0, 0.0, 0.5];
        shape.shadow_blur = 4.0;
        shape.shadow_offset = [2.0, 6.0];
        let [x0, y0, x1, y1] = shape.expanded_bounds();
        assert_eq!([x0, y0, x1, y1], [10.0 - 2.0 - 1.0, 10.0 - 1.0, 30.0 + 6.0 + 1.0, 30.0 + 10.0 + 1.0]);
    }

    /// A shape with no shadow to draw grows by the margin alone, whatever
    /// blur or offset its unused shadow fields carry.
    #[test]
    fn an_invisible_shadow_grows_nothing() {
        let mut shape = plain([10.0, 10.0, 20.0, 20.0]);
        shape.shadow_blur = 12.0;
        shape.shadow_offset = [5.0, 5.0];
        assert_eq!(shape.expanded_bounds(), [9.0, 9.0, 31.0, 31.0]);
    }
}
