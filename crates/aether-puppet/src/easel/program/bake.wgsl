// The painter's input maps rasterized on the GPU (ADR-0171,
// iamacoffeepot/aether#4411): the draw-pass twin of
// `easel/regions.rs`'s `rasterize`. The CPU stays the oracle, and every
// choice below is made to keep the two comparable rather than merely
// plausible.
//
// One vertex stage feeds three fragment stages, one per plane, because
// a pass writes exactly one colour attachment (ADR-0171 declines
// multiple render targets). The three passes name one shared depth
// slot, so all three resolve the same surface: the first clears it and
// the later two load and re-test it, and `LessEqual` lets the identical
// geometry through at the identical depth.
//
// `@interpolate(linear)` throughout, deliberately. The oracle blends
// its per-vertex quantities with barycentric weights taken from page
// coordinates alone — screen-space affine, not perspective-corrected —
// so the default perspective-correct interpolation would put a
// systematic geometric difference into every parity measurement and
// bury the quantities actually under test. Matching the oracle keeps
// the drift honest; the switch to perspective-correct is one attribute
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
// argmax runs over, then the two scalars each of the other planes
// carries out whole.
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

// The winning class of the blended score vector — argmax *after*
// interpolation, which is the whole point of carrying indicators to
// the pixel instead of a label (spike 142, issue 4399). `regions.rs`'s
// `argmax_class` restated: a strict improvement over a running best
// that starts at zero, so nothing scoring at or below zero wins and a
// tie goes to the lower class.
@fragment
fn fs_class(baked: Baked) -> @location(0) vec4<f32> {
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

    return vec4<f32>(winner, 0.0, 0.0, 1.0);
}

// The key light at the nearest surface, carried out unclamped: the
// face lift can push it past one, and where the range is cut belongs to
// whoever mixes the pigment (`RegionPlanes::tone`).
@fragment
fn fs_tone(baked: Baked) -> @location(0) vec4<f32> {
    return vec4<f32>(baked.surface.x, 0.0, 0.0, 1.0);
}

// How much the surface confronts the viewer, zero where it has turned
// away (`RegionPlanes::facing`).
@fragment
fn fs_facing(baked: Baked) -> @location(0) vec4<f32> {
    return vec4<f32>(baked.surface.y, 0.0, 0.0, 1.0);
}
