// The coat sequencer's own glue entry points (iamacoffeepot/aether#4369,
// ADR-0170): the handful of pointwise steps the wash pipeline needs
// between the op modules — coverage from the class plane, the material's
// value from the tone, the pour's accumulation with its support carry,
// the iris lift, and the atmosphere's displaced spill cut. Every formula
// transcribes its CPU counterpart in easel/field.rs / easel/palette.rs;
// the whole-sheet parity scenario in tests/program_wash_scenario.rs holds
// the two develops together.
//
// This module is never registered alone: `wash::module()` concatenates
// puddle.wgsl, pigment.wgsl, sheet.wgsl and this file into one module,
// and the `hermite` ramp called below is puddle.wgsl's definition. Shared
// conventions as everywhere in the wash: uniform window at
// @group(0) @binding(0), input planes in declaration order at group 1
// (input n at @binding(2 * n)), samplers undeclared because every read is
// a textureLoad at the writing fragment's own texel.

// Class ids the coverage derives from, as the f32s the class plane
// carries (labels.rs SKIN / LIPS): the mouth is drawn rather than
// painted, so it falls through into the skin wash (palette.rs
// `remapped`).
const MASK_SKIN: f32 = 1.0;
const MASK_LIPS: f32 = 6.0;

struct MaskParams {
    // The material class this mask selects.
    material_class: f32,
    // 1: select every labelled texel instead — the figure mask the
    // atmosphere stain is cut back by.
    figure: u32,
}

@group(0) @binding(0) var<uniform> mask_params: MaskParams;
@group(1) @binding(0) var mask_packed: texture_2d<f32>;

// palette.rs `mask_of` (and the figure mask inside `atmosphere_spill`):
// one material's coverage over the bake's class channel.
//
// The class rides the packed plane's red channel as `class / 255`, which
// an 8-bit unorm carries exactly, so it comes back as the integer it is
// under one multiply and a round (bake.wgsl's header states the contract;
// care.wgsl decodes it the same way).
@fragment
fn fs_mask(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let labelled = round(textureLoad(mask_packed, vec2<i32>(position.xy), 0).r * 255.0);
    let remapped = select(labelled, MASK_SKIN, labelled == MASK_LIPS);
    let covered = select(f32(remapped == mask_params.material_class), f32(labelled != 0.0), mask_params.figure == 1u);
    return vec4<f32>(covered, 0.0, 0.0, 1.0);
}

// Tone at which a material is fully in shadow (palette.rs SHADOWED); the
// lit edge varies per material and rides the uniforms.
const SHADOWED: f32 = 0.3;

struct ShadeParams {
    // How much of the material survives full light
    // (`Material::shade_floor`).
    shade_floor: f32,
    // Tone at which this material counts as fully lit — the material's
    // own `shade_lit`, or palette.rs LIT.
    lit: f32,
    // Texels of the bound tone plane per texel of this pass's own output.
    // One for a material developed at the plane's own extent; the
    // reciprocal of the body divisor for a material developed finer than
    // the bake it reads (the iris, wash.rs `Grain`).
    source_scale: f32,
}

@group(0) @binding(0) var<uniform> shade_params: ShadeParams;
@group(1) @binding(0) var shade_packed: texture_2d<f32>;

// palette.rs `shade_of`: how much of a material's wash survives at each
// pixel, given the light.
//
// The scaled read is a point sample of the coarser plane rather than a
// filtered one, and deliberately: the tone plane is the key light, which
// varies over the whole figure rather than over a texel, and a nearest
// read of it at twice the rate costs the value plane nothing the wash's
// own support blur does not immediately soften away.
@fragment
fn fs_shade(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    // The key light is the packed plane's green channel (bake.wgsl).
    let source = vec2<i32>(floor(position.xy * shade_params.source_scale));
    let tone = textureLoad(shade_packed, source, 0).g;
    let value = shade_params.shade_floor
        + (1.0 - shade_params.shade_floor) * hermite(shade_params.lit, SHADOWED, tone);
    return vec4<f32>(value, 0.0, 0.0, 1.0);
}

// Floor under the region's own support (field.rs SUPPORT_FLOOR), so a
// thin sliver of coverage does not divide the value it carries up to
// something enormous.
const SUPPORT_FLOOR: f32 = 0.05;

struct AccumulateParams {
    // 1 to keep the density already poured, 0 on the chain's first pour
    // (there is no earlier density; the bound plane is a stand-in).
    keep: f32,
    // The pour's body times the wash's load — zeroed to neutralize an
    // absent material's pour (ADR-0170's zeroed-contribution convention).
    body_load: f32,
    // 1 when the wash carries a value plane; 0 pins the carry at one,
    // for a wash that carries pigment uniformly.
    has_value: f32,
}

@group(0) @binding(0) var<uniform> accumulate_params: AccumulateParams;
@group(1) @binding(0) var accumulate_prev: texture_2d<f32>;
@group(1) @binding(2) var accumulate_alpha: texture_2d<f32>;
@group(1) @binding(4) var accumulate_rim: texture_2d<f32>;
@group(1) @binding(6) var accumulate_value: texture_2d<f32>;
@group(1) @binding(8) var accumulate_reference: texture_2d<f32>;

// The accumulation inside field.rs `Sheet::pour`: the pour's body carried
// in proportion to the softened value over the softened support, plus the
// rim plane (already varied and strength-folded by fs_rim), added onto
// the density poured so far.
@fragment
fn fs_pour_accumulate(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let at = vec2<i32>(position.xy);
    let value = textureLoad(accumulate_value, at, 0).r;
    let reference = textureLoad(accumulate_reference, at, 0).r;
    let carried = mix(1.0, min(value / max(reference, SUPPORT_FLOOR), 1.0), accumulate_params.has_value);

    let poured = textureLoad(accumulate_alpha, at, 0).r * accumulate_params.body_load * carried
        + textureLoad(accumulate_rim, at, 0).r;
    let kept = textureLoad(accumulate_prev, at, 0).r * accumulate_params.keep;
    return vec4<f32>(kept + poured, 0.0, 0.0, 1.0);
}

// The same accumulation when the value and reference support blurs rode
// one paired Rgba16Float chain. R is the softened value and G the
// softened mask; each channel was filtered and quantized exactly as its
// former scalar R16Float plane was.
@fragment
fn fs_pour_accumulate_paired(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let at = vec2<i32>(position.xy);
    let support = textureLoad(accumulate_value, at, 0).rg;
    let carried = min(support.r / max(support.g, SUPPORT_FLOOR), 1.0);

    let poured = textureLoad(accumulate_alpha, at, 0).r * accumulate_params.body_load * carried
        + textureLoad(accumulate_rim, at, 0).r;
    let kept = textureLoad(accumulate_prev, at, 0).r * accumulate_params.keep;
    return vec4<f32>(kept + poured, 0.0, 0.0, 1.0);
}

struct LiftParams {
    // 1 applies the lift plane, 0 leaves the density untouched (a subject
    // that charted no face uploads no lift worth reading).
    gate: f32,
}

@group(0) @binding(0) var<uniform> lift_params: LiftParams;
@group(1) @binding(0) var lift_density: texture_2d<f32>;
@group(1) @binding(2) var lift_weight: texture_2d<f32>;

// The lid crossing the iris (the lift block inside field.rs
// `Sheet::coats`): weight over the finished wash rather than coverage.
@fragment
fn fs_lift(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let at = vec2<i32>(position.xy);
    let weight = mix(1.0, textureLoad(lift_weight, at, 0).r, lift_params.gate);
    return vec4<f32>(textureLoad(lift_density, at, 0).r * weight, 0.0, 0.0, 1.0);
}

struct LiftExtentParams {
    // Texels of the bound accumulator per texel of this pass's output —
    // the reciprocal of the body divisor (wash.rs `BODY_DIVISOR`).
    source_scale: f32,
}

@group(0) @binding(0) var<uniform> lift_extent: LiftExtentParams;
@group(1) @binding(0) var lift_extent_light: texture_2d<f32>;
@group(1) @binding(1) var lift_extent_sampler: sampler;

// The notch's one seam: the light accumulated over the notched body,
// carried up to the sheet's own pixels so the accents can be absorbed
// into it at full resolution.
//
// The read is filtered rather than pointwise, and this is the one place
// in the wash where that is right. Everything absorbed so far is the low
// frequencies the notch exists to develop coarsely (ADR-0170's
// frequency-split argument) — a wash body, its tide lines already
// softened, its granulation already settled — so a bilinear lift is the
// reconstruction the sampling assumed, where a nearest one would put a
// staircase along every tide line the body drew on the diagonal. The
// accumulator is `Rgba8`, which is filterable, so the executor hands this
// pass the linear sampler and the four taps cost one instruction.
@fragment
fn fs_light_lift(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let source = vec2<f32>(textureDimensions(lift_extent_light));
    let uv = position.xy * lift_extent.source_scale / source;

    return vec4<f32>(textureSampleLevel(lift_extent_light, lift_extent_sampler, uv, 0.0).rgb, 1.0);
}

// Where in the displaced halo the stain reaches full strength, how much
// of it the standing figure takes back, and the level the mask is cut at
// (field.rs ATMOSPHERE_REACH / ATMOSPHERE_RESIST / ATMOSPHERE_LEVEL).
const ATMOSPHERE_REACH_NEAR: f32 = 0.1;
const ATMOSPHERE_REACH_FULL: f32 = 0.4;
const ATMOSPHERE_RESIST: f32 = 0.85;
const ATMOSPHERE_LEVEL: f32 = 0.45;

struct SpillParams {
    // Where the region's presence is carried, across the sheet and down
    // it — the material's `Atmosphere::drift`, already tuned to this
    // sheet's pixels.
    drift_x: f32,
    drift_y: f32,
}

@group(0) @binding(0) var<uniform> spill_params: SpillParams;
@group(1) @binding(0) var spill_halo: texture_2d<f32>;
@group(1) @binding(2) var spill_standing: texture_2d<f32>;

// field.rs `Sheet::atmosphere_spill`: the halo fetched from where the
// stain came (the read is displaced backwards, clamped to the sheet),
// cut back where the figure stands, and hardened at the atmosphere level
// into the mask the stain's wash develops from. The float displacement
// clamps before it truncates, exactly as the CPU's
// `(y as f32 - drift).clamp(...) as usize` does.
@fragment
fn fs_atmosphere_spill(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let at = vec2<i32>(position.xy);
    let extent = vec2<i32>(textureDimensions(spill_halo));
    let drift = vec2<f32>(spill_params.drift_x, spill_params.drift_y);
    let came = vec2<i32>(clamp(vec2<f32>(at) - drift, vec2<f32>(0.0, 0.0), vec2<f32>(extent - vec2<i32>(1, 1))));

    let spill = hermite(ATMOSPHERE_REACH_NEAR, ATMOSPHERE_REACH_FULL, textureLoad(spill_halo, came, 0).r)
        * (1.0 - textureLoad(spill_standing, at, 0).r * ATMOSPHERE_RESIST);
    return vec4<f32>(f32(spill > ATMOSPHERE_LEVEL), 0.0, 0.0, 1.0);
}
