// Screen-space textured quad overlay (ADR-0105). Vertex positions are
// window pixels with a top-left origin; the viewport uniform maps them
// to clip space under a top-down ortho. The fragment stage samples the
// bound texture and multiplies by the per-vertex tint; the pipeline
// alpha-blends the result over the world pass.

struct Viewport {
    // x = width in pixels, y = height in pixels. zw pad to 16 bytes.
    size: vec2<f32>,
    pad: vec2<f32>,
}

@group(0) @binding(0)
var<uniform> viewport: Viewport;

@group(1) @binding(0)
var quad_texture: texture_2d<f32>;
@group(1) @binding(1)
var quad_sampler: sampler;

struct VertexInput {
    @location(0) position: vec2<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) tint: vec4<f32>,
}

struct VertexOutput {
    @builtin(position) clip_pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) tint: vec4<f32>,
}

@vertex
fn vs_main(in: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    // Pixel (0,0) top-left maps to clip (-1, 1); pixel (w,h) bottom-right
    // maps to clip (1, -1) — y flips because window pixels are top-down
    // while clip space is bottom-up.
    let ndc_x = in.position.x / viewport.size.x * 2.0 - 1.0;
    let ndc_y = 1.0 - in.position.y / viewport.size.y * 2.0;
    out.clip_pos = vec4<f32>(ndc_x, ndc_y, 0.0, 1.0);
    out.uv = in.uv;
    out.tint = in.tint;
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let texel = textureSample(quad_texture, quad_sampler, in.uv);
    return texel * in.tint;
}
