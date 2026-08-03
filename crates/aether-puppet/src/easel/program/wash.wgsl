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
@group(1) @binding(0) var mask_classes: texture_2d<f32>;

// palette.rs `mask_of` (and the figure mask inside `atmosphere_spill`):
// one material's coverage over the region plane.
@fragment
fn fs_mask(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let labelled = textureLoad(mask_classes, vec2<i32>(position.xy), 0).r;
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
}

@group(0) @binding(0) var<uniform> shade_params: ShadeParams;
@group(1) @binding(0) var shade_tone: texture_2d<f32>;

// palette.rs `shade_of`: how much of a material's wash survives at each
// pixel, given the light.
@fragment
fn fs_shade(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let tone = textureLoad(shade_tone, vec2<i32>(position.xy), 0).r;
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
    let from = vec2<i32>(clamp(vec2<f32>(at) - drift, vec2<f32>(0.0, 0.0), vec2<f32>(extent - vec2<i32>(1, 1))));

    let spill = hermite(ATMOSPHERE_REACH_NEAR, ATMOSPHERE_REACH_FULL, textureLoad(spill_halo, from, 0).r)
        * (1.0 - textureLoad(spill_standing, at, 0).r * ATMOSPHERE_RESIST);
    return vec4<f32>(f32(spill > ATMOSPHERE_LEVEL), 0.0, 0.0, 1.0);
}
