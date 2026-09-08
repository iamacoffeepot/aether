// World-aware textured quad overlay (ADR-0105). Two paths share one
// pipeline: Screen quads supply an absolute pixel position in
// `offset_px` and set `is_screen != 0`; World quads set `is_screen ==
// 0`, transform `anchor` through `view_proj`, and apply `offset_px`
// as a clip-space pixel offset so labels stay camera-facing and never
// skew — both through the shared `overlay_clip_position` prepended from
// `overlay_projection.wgsl` at pipeline build. The fragment stage samples
// the bound texture and multiplies by the per-vertex tint; the pipeline
// alpha-blends the result over the world pass.

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
    out.clip_pos = overlay_clip_position(in.anchor, in.offset_px, in.k, in.is_screen);
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let texel = textureSample(quad_texture, quad_sampler, in.uv);
    return texel * in.tint;
}
