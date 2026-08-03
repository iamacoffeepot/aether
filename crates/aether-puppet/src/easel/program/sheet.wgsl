// The sheet ops as authored passes (ADR-0170, iamacoffeepot/aether#4368):
// the care-ramp mix of the tight and loose develops, the lost edge's
// directional giveback, and the palette composite against paper white.
// The CPU develop stays the oracle — `easel/field.rs` (`material_wash`,
// `threshold`'s lost branch) and `easel/palette.rs` (`composite`) — and
// the constants below are that code's, restated. Every op is pointwise,
// so planes are read with textureLoad at the writing fragment's own
// texel: exact fetches keep the oracle comparable within quantization
// rather than within filtering.

// Half-angle edges of the arc a lost edge gives way over, in radians
// (field.rs LOST_ARC): the giveback ramps in from FAR to NEAR about the
// lost direction.
const LOST_ARC_FAR: f32 = 1.3;
const LOST_ARC_NEAR: f32 = 0.55;

// How hard the paper takes the wash back where the edge is lost
// (field.rs LOST_FALLOFF), and how much of it survives as a stain with
// no edge at all (field.rs LOST_STAIN).
const LOST_FALLOFF: f32 = 1.8;
const LOST_STAIN: f32 = 0.85;

// Below this a coat is doing nothing the eye can find, so it is skipped
// (palette.rs MINIMUM_DEPOSIT).
const MINIMUM_DEPOSIT: f32 = 0.002;

// The paper's own colour (palette.rs PAPER), as unit transmissions.
const PAPER: vec3<f32> = vec3<f32>(246.0 / 255.0, 242.0 / 255.0, 233.0 / 255.0);

const TAU: f32 = 6.2831855;

// The one uniform block every windowed sheet pass binds; each entry
// point reads only its own fields, and `sheet::SHEET_PARAMS_BYTES` is
// this struct's size. The Rust encoders mirror this layout byte for
// byte: `LostEdgeParams` fills centre and lost_angle, `CoatParams`
// fills cap and transmission.
struct SheetParams {
    centre: vec2<f32>,
    lost_angle: f32,
    cap: f32,
    transmission: vec4<f32>,
}

@group(0) @binding(0) var<uniform> sheet_params: SheetParams;

// Input planes bind positionally — input n at @binding(2 * n) — so one
// set of declarations serves every entry point; each entry's comment
// names its roles. The paired samplers (odd bindings) go undeclared:
// every read is a textureLoad.
@group(1) @binding(0) var plane_a: texture_2d<f32>;
@group(1) @binding(2) var plane_b: texture_2d<f32>;
@group(1) @binding(4) var plane_c: texture_2d<f32>;

// image::smoothstep restated: a Hermite ramp whose edges may run either
// way (a > b descends). WGSL's own smoothstep leaves a descending edge
// pair undefined, and the lost arc ramps downward.
fn hermite(a: f32, b: f32, x: f32) -> f32 {
    let t = clamp((x - a) / (b - a), 0.0, 1.0);
    return t * t * (3.0 - 2.0 * t);
}

// The care ramp applied (field.rs material_wash): a = the tight
// develop, b = the loose develop, c = the care plane. Held where the
// hand is close, freed where it relaxes. The care field itself stays
// CPU-computed on the class plane and uploads as an ordinary plane.
@fragment
fn fs_care_mix(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let at = vec2<i32>(position.xy);
    let held = textureLoad(plane_a, at, 0).r;
    let freed = textureLoad(plane_b, at, 0).r;
    let care = textureLoad(plane_c, at, 0).r;
    return vec4<f32>(held * care + freed * (1.0 - care), 0.0, 0.0, 1.0);
}

// The lost edge (field.rs threshold's lost branch): a = the hard
// thresholded alpha, b = the softened puddle. Past the lost arc about
// the region's centroid the hard edge fades out, and what is left is a
// stain with no boundary at all. The soft plane is clamped before the
// fractional power exactly as the CPU clamps it: a blur residue a
// rounding error below zero would otherwise turn NaN and paint a
// region-shaped hole of bare paper.
@fragment
fn fs_lost_edge(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let at = vec2<i32>(position.xy);
    let hard = textureLoad(plane_a, at, 0).r;
    let soft = textureLoad(plane_b, at, 0).r;

    let offset = position.xy - vec2<f32>(0.5, 0.5) - sheet_params.centre;
    let bearing = atan2(offset.y, offset.x);
    let away = abs(bearing - sheet_params.lost_angle);
    let lostness = hermite(LOST_ARC_FAR, LOST_ARC_NEAR, min(away, TAU - away));
    let stain = pow(max(soft, 0.0), LOST_FALLOFF);
    return vec4<f32>(hard * (1.0 - lostness) + stain * LOST_STAIN * lostness, 0.0, 0.0, 1.0);
}

// The unpainted sheet (palette.rs composite's starting light): full
// transmission into the light accumulator — the empty product the
// absorption passes multiply down, coat by coat.
@fragment
fn fs_light_prime() -> @location(0) vec4<f32> {
    return vec4<f32>(1.0, 1.0, 1.0, 1.0);
}

// One coat's absorption (palette.rs composite's per-coat step): a = the
// light accumulated so far, b = the coat's density plane. The deposit
// is capped before the pigment power — the sheet holds only so much —
// and a deposit under the minimum leaves the light untouched, exactly
// as the CPU skips it. Alpha 1 so the blendable target replaces rather
// than mixes with the previous ping-pong content.
@fragment
fn fs_coat_absorb(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let at = vec2<i32>(position.xy);
    let light = textureLoad(plane_a, at, 0).rgb;
    let deposit = min(textureLoad(plane_b, at, 0).r, sheet_params.cap);
    let absorbed = select(
        vec3<f32>(1.0, 1.0, 1.0),
        pow(sheet_params.transmission.rgb, vec3<f32>(deposit, deposit, deposit)),
        deposit > MINIMUM_DEPOSIT,
    );
    return vec4<f32>(light * absorbed, 1.0);
}

// The resolve against paper white (palette.rs composite's final loop):
// a = the accumulated light, b = the paper-shade plane. Paper and shade
// multiply exactly once, here — never per coat — and the alpha is 1
// everywhere: the sheet is opaque paper, the convention the easel
// billboard depends on.
@fragment
fn fs_paper_composite(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let at = vec2<i32>(position.xy);
    let light = textureLoad(plane_a, at, 0).rgb;
    let shade = textureLoad(plane_b, at, 0).r;
    let sheet = clamp(PAPER * light * shade, vec3<f32>(0.0, 0.0, 0.0), vec3<f32>(1.0, 1.0, 1.0));
    return vec4<f32>(sheet, 1.0);
}
