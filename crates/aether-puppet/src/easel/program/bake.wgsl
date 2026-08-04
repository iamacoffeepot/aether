// The painter's input maps rasterized on the GPU (ADR-0171,
// iamacoffeepot/aether#4411, #4412): the draw-pass twin of
// `easel/regions.rs`'s `rasterize`. The CPU stays the oracle, and every
// choice below is made to keep the two comparable rather than merely
// plausible.
//
// One vertex stage feeds one fragment stage, which fills one `Rgba8`
// target's channels: R the class, G the tone, B the facing. ADR-0171
// declines multiple render targets precisely because a channel packing
// covers every consumer, and all three quantities are answered from one
// interpolated surface — so asking the hardware for the subject three
// times, once per plane, was three rasterizations to fill separately
// what one fills together.
//
// The channel contract, which the consumers and the parity scenario both
// read:
//
//   R  class, post-argmax, as `class / 255`. An 8-bit unorm store carries
//      a `k / 255` exactly and returns the same integer, so the class
//      survives as the integer it is — provided nothing linear-filters
//      it. The wash's consumers reach it through `textureLoad`, which
//      takes no sampler at all, and the texture is created nearest
//      besides.
//   G  tone, clamped into `[0, 1]` by the unorm store. `Settings::tone`
//      is unclamped by contract — the face lift carries it past one — but
//      every consumer runs it through `smoothstep(lit, SHADOWED, tone)`
//      and the largest `lit` in the palette is 0.92, so a tone at or
//      above one already saturates and the clipped range is unobservable.
//   B  facing, already in `[0, 1]`.
//
// Tone and facing therefore quantize to about one part in 255 — the same
// quantization the parity scenario's readback already carried when the
// planes were `R32Float`, so it costs nothing that was being measured
// before.
//
// `@interpolate(linear)` throughout, deliberately. The oracle blends its
// per-vertex quantities with barycentric weights taken from page
// coordinates alone — screen-space affine, not perspective-corrected —
// so the default perspective-correct interpolation would put a
// systematic geometric difference into every parity measurement and bury
// the quantities actually under test. Matching the oracle keeps the
// drift honest; the switch to perspective-correct is one attribute
// keyword the day the oracle retires.

// Everything the bake needs of the camera. `view_proj` is the matrix
// the ink was drawn from — column-major, as `Mat4::to_cols_array`
// writes it and WGSL reads it — and `eye` is where the viewer sits,
// carried apart because facing asks about the eye while projection
// asks about the matrix. The uniform window binds to the vertex stage
// as well as the fragment stage, which is what lets the camera reach
// the rasterizer at all.
struct BakeParams {
    view_proj: mat4x4<f32>,
    eye: vec3<f32>,
}
@group(0) @binding(0) var<uniform> bake_params: BakeParams;

// The interpolated surface, per pixel: the eight class indicators the
// argmax runs over, then the two scalars the other two channels carry
// out whole.
struct Baked {
    @builtin(position) clip: vec4<f32>,
    @location(0) @interpolate(linear) scores_low: vec3<f32>,
    @location(1) @interpolate(linear) scores_mid: vec3<f32>,
    @location(2) @interpolate(linear) scores_high: vec2<f32>,
    @location(3) @interpolate(linear) surface: vec2<f32>,
}

// Project, and answer facing where the eye is still known.
//
// `tone` rides in as a vertex attribute rather than being re-derived
// here: the oracle evaluates `Settings::tone` per vertex and blends the
// result, so blending the same per-vertex scalar is parity by
// construction instead of by a second transcription of `face_weight`.
// `facing` cannot ride in — it turns with the eye every frame — so it
// is computed here from the position and normal that are already
// present, restating `(eye - p).normalize_or(n) . n` floored at zero.
@vertex
fn vs_bake(
    @location(0) position: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) tone: f32,
    @location(3) scores_low: vec3<f32>,
    @location(4) scores_mid: vec3<f32>,
    @location(5) scores_high: vec2<f32>,
) -> Baked {
    var baked: Baked;
    baked.clip = bake_params.view_proj * vec4<f32>(position, 1.0);
    baked.scores_low = scores_low;
    baked.scores_mid = scores_mid;
    baked.scores_high = scores_high;

    // `normalize_or` keeps the normal when the eye has collapsed onto
    // the point and there is no direction to take.
    let toward = bake_params.eye - position;
    let reach = dot(toward, toward);
    var direction = normal;
    if reach > 0.0 {
        direction = toward * inverseSqrt(reach);
    }
    baked.surface = vec2<f32>(tone, max(dot(direction, normal), 0.0));

    return baked;
}

// The three planes as one texel.
//
// The class is the winner of the blended score vector — argmax *after*
// interpolation, which is the whole point of carrying indicators to the
// pixel instead of a label (spike 142, issue 4399). `regions.rs`'s
// `argmax_class` restated: a strict improvement over a running best that
// starts at zero, so nothing scoring at or below zero wins and a tie goes
// to the lower class.
@fragment
fn fs_packed(baked: Baked) -> @location(0) vec4<f32> {
    var scores = array<f32, 8>(
        baked.scores_low.x,
        baked.scores_low.y,
        baked.scores_low.z,
        baked.scores_mid.x,
        baked.scores_mid.y,
        baked.scores_mid.z,
        baked.scores_high.x,
        baked.scores_high.y,
    );

    var winner = 0.0;
    var best = 0.0;
    for (var index = 0; index < 8; index++) {
        if scores[index] > best {
            best = scores[index];
            winner = f32(index + 1);
        }
    }

    return vec4<f32>(winner / 255.0, baked.surface.x, baked.surface.y, 1.0);
}
