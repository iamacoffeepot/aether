//! WGSL for the drawing approaches. Texture coordinates are remapped in the
//! fragment stage because a program input must be the output extent divided
//! by an integer: a 64x64 tile lives in the corner of a 160x90 texture
//! (output / 8), and the atlas is a full-extent 1280x720 texture.

/// Uniform block of the per-instance (`repeat`), static and
/// dispatch-per-instance approaches: one model-view-projection per window.
pub const WINDOW_BYTES: u32 = 80;

pub const DIRECT: &str = r"
struct U { mvp: mat4x4<f32>, misc: vec4<f32> }
@group(0) @binding(0) var<uniform> u: U;
@group(1) @binding(0) var tex: texture_2d<f32>;
@group(1) @binding(1) var smp: sampler;

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

fn shade(n: vec3<f32>) -> f32 {
    return 0.35 + 0.65 * max(dot(normalize(n), normalize(vec3<f32>(0.4, 0.8, 0.45))), 0.0);
}

@vertex
fn vs36(
    @location(0) p: vec3<f32>,
    @location(1) n: vec3<f32>,
    @location(2) c: vec4<f32>,
    @location(3) uv: vec2<f32>,
) -> V {
    var out: V;
    out.clip = u.mvp * vec4<f32>(p, 1.0);
    out.color = c;
    out.uv = uv;
    out.light = shade(rot_y(n, u.misc.x));
    out.tile = vec4<u32>(0u);
    return out;
}

@vertex
fn vs40(
    @location(0) p: vec3<f32>,
    @location(1) n: vec3<f32>,
    @location(2) c: vec4<f32>,
    @location(3) uv: vec2<f32>,
    @location(4) tile: vec4<u32>,
) -> V {
    var out: V;
    out.clip = u.mvp * vec4<f32>(p, 1.0);
    out.color = c;
    out.uv = uv;
    out.light = shade(rot_y(n, u.misc.x));
    out.tile = tile;
    return out;
}

@fragment
fn fs_tex(v: V) -> @location(0) vec4<f32> {
    let uv = (fract(v.uv) * 63.0 + 0.5) / vec2<f32>(160.0, 90.0);
    let t = textureSample(tex, smp, uv);
    return vec4<f32>(t.rgb * v.color.rgb * v.light, 1.0);
}

@fragment
fn fs_col(v: V) -> @location(0) vec4<f32> {
    return vec4<f32>(v.color.rgb * v.light, 1.0);
}

@fragment
fn fs_atlas(v: V) -> @location(0) vec4<f32> {
    let uv = (vec2<f32>(v.tile.xy) * 64.0 + fract(v.uv) * 63.0 + 0.5) / vec2<f32>(1280.0, 720.0);
    let t = textureSample(tex, smp, uv);
    let texel = mix(vec3<f32>(1.0), t.rgb, f32(v.tile.z));
    return vec4<f32>(texel * v.color.rgb * v.light, 1.0);
}
";

/// The instanced approaches: a compute pass writes the geometry's indirect
/// control block (`index_count` from its capacity word, `instance_count`
/// from the uniform), and the vertex stage reads `(x, y, z, yaw)` per
/// instance out of an `R32Float` data texture (640x360 = output / 2, four
/// texels per instance, 160 instances per row).
pub const INSTANCED: &str = r"
struct U { view_proj: mat4x4<f32>, params: vec4<u32> }
@group(0) @binding(0) var<uniform> u: U;
@group(1) @binding(0) var inst: texture_2d<f32>;
@group(1) @binding(2) var tex: texture_2d<f32>;
@group(1) @binding(3) var smp: sampler;
@group(2) @binding(0) var<storage, read_write> control: array<u32>;

struct V {
    @builtin(position) clip: vec4<f32>,
    @location(0) color: vec4<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) light: f32,
    @location(3) @interpolate(flat) tile: vec4<u32>,
}

@compute @workgroup_size(1)
fn cs_count() {
    control[0] = control[6];
    control[1] = u.params.y;
    control[2] = 0u;
    control[3] = 0u;
    control[4] = 0u;
}

fn rot_y(v: vec3<f32>, yaw: f32) -> vec3<f32> {
    let c = cos(yaw);
    let s = sin(yaw);
    return vec3<f32>(c * v.x + s * v.z, v.y, -s * v.x + c * v.z);
}

fn shade(n: vec3<f32>) -> f32 {
    return 0.35 + 0.65 * max(dot(normalize(n), normalize(vec3<f32>(0.4, 0.8, 0.45))), 0.0);
}

fn instance(index: u32) -> vec4<f32> {
    let k = u.params.x + index;
    let x = i32((k % 160u) * 4u);
    let y = i32(k / 160u);
    return vec4<f32>(
        textureLoad(inst, vec2<i32>(x, y), 0).r,
        textureLoad(inst, vec2<i32>(x + 1, y), 0).r,
        textureLoad(inst, vec2<i32>(x + 2, y), 0).r,
        textureLoad(inst, vec2<i32>(x + 3, y), 0).r,
    );
}

@vertex
fn vs36(
    @builtin(instance_index) index: u32,
    @location(0) p: vec3<f32>,
    @location(1) n: vec3<f32>,
    @location(2) c: vec4<f32>,
    @location(3) uv: vec2<f32>,
) -> V {
    let i = instance(index);
    var out: V;
    out.clip = u.view_proj * vec4<f32>(rot_y(p, i.w) + i.xyz, 1.0);
    out.color = c;
    out.uv = uv;
    out.light = shade(rot_y(n, i.w));
    out.tile = vec4<u32>(0u);
    return out;
}

@vertex
fn vs40(
    @builtin(instance_index) index: u32,
    @location(0) p: vec3<f32>,
    @location(1) n: vec3<f32>,
    @location(2) c: vec4<f32>,
    @location(3) uv: vec2<f32>,
    @location(4) tile: vec4<u32>,
) -> V {
    let i = instance(index);
    var out: V;
    out.clip = u.view_proj * vec4<f32>(rot_y(p, i.w) + i.xyz, 1.0);
    out.color = c;
    out.uv = uv;
    out.light = shade(rot_y(n, i.w));
    out.tile = tile;
    return out;
}

@fragment
fn fs_tex(v: V) -> @location(0) vec4<f32> {
    let uv = (fract(v.uv) * 63.0 + 0.5) / vec2<f32>(160.0, 90.0);
    let t = textureSample(tex, smp, uv);
    return vec4<f32>(t.rgb * v.color.rgb * v.light, 1.0);
}

@fragment
fn fs_col(v: V) -> @location(0) vec4<f32> {
    return vec4<f32>(v.color.rgb * v.light, 1.0);
}

@fragment
fn fs_atlas(v: V) -> @location(0) vec4<f32> {
    let uv = (vec2<f32>(v.tile.xy) * 64.0 + fract(v.uv) * 63.0 + 0.5) / vec2<f32>(1280.0, 720.0);
    let t = textureSample(tex, smp, uv);
    let texel = mix(vec3<f32>(1.0), t.rgb, f32(v.tile.z));
    return vec4<f32>(texel * v.color.rgb * v.light, 1.0);
}
";
