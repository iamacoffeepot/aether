struct Camera {
    view_proj: mat4x4<f32>,
}

@group(0) @binding(0)
var<uniform> camera: Camera;

@group(1) @binding(0)
var material_texture: texture_2d<f32>;
@group(1) @binding(1)
var material_sampler: sampler;

struct MaterialParams {
    color0: vec4<f32>,
    rim_color: vec4<f32>,
    rim_width: f32,
    _pad0: vec3<f32>,
}

@group(2) @binding(0)
var<uniform> params: MaterialParams;

struct VertexInput {
    @location(0) position: vec3<f32>,
    @location(1) uv: vec2<f32>,
}

struct VertexOutput {
    @builtin(position) clip_pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

@vertex
fn vs_main(in: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    out.clip_pos = camera.view_proj * vec4<f32>(in.position, 1.0);
    out.uv = in.uv;
    return out;
}

@fragment
fn fs_textured(in: VertexOutput) -> @location(0) vec4<f32> {
    let texel = textureSample(material_texture, material_sampler, in.uv);
    return texel * params.color0;
}

@fragment
fn fs_coverage(in: VertexOutput) -> @location(0) vec4<f32> {
    let coverage = textureSample(material_texture, material_sampler, in.uv).r;
    let iso = 127.5 / 255.0;
    let width = max(fwidth(coverage), 0.001);
    let inside = smoothstep(iso - width, iso + width, coverage);
    let distance_inside = max(coverage - iso, 0.0);
    let rim_t = 1.0 - smoothstep(0.0, max(params.rim_width, width), distance_inside);
    let color = mix(params.color0, params.rim_color, rim_t);
    return vec4<f32>(color.rgb, color.a * inside);
}
