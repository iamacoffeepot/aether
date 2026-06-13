// World-aware textured quad overlay (ADR-0105). Two paths share one
// pipeline: Screen quads supply an absolute pixel position in
// `offset_px` and set `is_screen != 0`; World quads set `is_screen ==
// 0`, transform `anchor` through `view_proj`, and apply `offset_px`
// as a clip-space pixel offset so labels stay camera-facing and never
// skew. The fragment stage samples the bound texture and multiplies by
// the per-vertex tint; the pipeline alpha-blends the result over the
// world pass.

struct Viewport {
    // Column-major view-projection matrix used by the World path.
    view_proj: mat4x4<f32>,
    // Width and height of the render target in pixels.
    size: vec2<f32>,
    _pad: vec2<f32>,
}

@group(0) @binding(0)
var<uniform> viewport: Viewport;

@group(1) @binding(0)
var quad_texture: texture_2d<f32>;
@group(1) @binding(1)
var quad_sampler: sampler;

struct VertexInput {
    // World-space anchor (World path) or (0,0,0) unused (Screen path).
    @location(0) anchor: vec3<f32>,
    // Screen: absolute pixel position. World: pixel offset from the
    // projected anchor in screen y-down convention.
    @location(1) offset_px: vec2<f32>,
    @location(2) uv: vec2<f32>,
    @location(3) tint: vec4<f32>,
    // World scale factor. Negative => Pixels mode (k = clip.w,
    // constant screen size). Positive => Distance mode (constant k,
    // shrinks with depth). Unused on Screen quads.
    @location(4) k: f32,
    // Non-zero => Screen path; zero => World path.
    @location(5) is_screen: u32,
}

struct VertexOutput {
    @builtin(position) clip_pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) tint: vec4<f32>,
}

@vertex
fn vs_main(in: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    out.uv = in.uv;
    out.tint = in.tint;

    if in.is_screen != 0u {
        // Screen path: offset_px holds the absolute pixel position.
        // Pixel (0,0) top-left => clip (-1, 1); pixel (w,h)
        // bottom-right => clip (1, -1). y flips because pixels are
        // top-down while clip space is bottom-up.
        let ndc_x = in.offset_px.x / viewport.size.x * 2.0 - 1.0;
        let ndc_y = 1.0 - in.offset_px.y / viewport.size.y * 2.0;
        out.clip_pos = vec4<f32>(ndc_x, ndc_y, 0.0, 1.0);
    } else {
        // World path (ADR-0105): transform the anchor through
        // view_proj, then apply the per-vertex pixel offset in clip
        // space so labels face the camera and never skew.
        var clip = viewport.view_proj * vec4<f32>(in.anchor, 1.0);
        // Anchors behind the camera (clip.w <= 0) are silently
        // discarded by pushing the vertex outside the clip cube.
        if clip.w <= 0.0 {
            out.clip_pos = vec4<f32>(2.0, 2.0, 2.0, 1.0);
            return out;
        }
        // Resolve the scale factor: negative k means Pixels mode
        // (use clip.w, cancelling the perspective divide for constant
        // on-screen size); positive k means Distance mode (constant
        // k so the label shrinks as the anchor recedes).
        var k = in.k;
        if k < 0.0 {
            k = clip.w;
        }
        // offset_px uses screen y-down convention; negate y so a
        // positive offset_px.y moves downward on screen (i.e. a
        // negative offset_px.y, as produced for above-anchor glyphs,
        // increases clip.y and moves the label upward).
        clip.x += in.offset_px.x / viewport.size.x * 2.0 * k;
        clip.y -= in.offset_px.y / viewport.size.y * 2.0 * k;
        out.clip_pos = clip;
    }
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let texel = textureSample(quad_texture, quad_sampler, in.uv);
    return texel * in.tint;
}
