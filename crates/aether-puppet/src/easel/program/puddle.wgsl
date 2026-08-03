// The puddle ops as authored fragment passes (iamacoffeepot/aether#4366,
// ADR-0170): the wash's water vocabulary from easel/field.rs and
// easel/image.rs re-spoken as WGSL, one entry point per op. The CPU
// implementations stay the oracle, so every formula here is a transcription
// rather than an approximation: the box average reads the same clamped
// window the running sum averages, the shrink resample is the same bilinear
// with everything off the plane reading as zero, and the threshold band is
// the same hermite ramp over the same displaced noise window.
//
// Every op reads its planes through textureLoad at integer texel
// coordinates, exactly as the CPU indexes them; the pass input contract's
// samplers are never declared. Entry points take the fragment's
// @builtin(position), whose xy at a fragment center is (x + 0.5, y + 0.5),
// so the vec2<i32> truncation below recovers the exact texel index. Each
// writes its scalar result across rgb with alpha one: an R32Float target
// keeps the r channel at full precision, and an Rgba8 target reads as
// grayscale through the overlay path the parity scenarios observe.
//
// Entry points sharing @group(0) @binding(0) for their own uniform structs
// is deliberate: bindings collide per entry point, not per module, and each
// pass pipeline binds only its own window (aether-render builds one
// pipeline per pass over this shared module).

// The CPU planes' clamp-at-edge read: image.rs `clamped` applied per axis.
fn load_clamped(plane: texture_2d<f32>, at: vec2<i32>) -> f32 {
    let extent = vec2<i32>(textureDimensions(plane));
    return textureLoad(plane, clamp(at, vec2<i32>(0, 0), extent - vec2<i32>(1, 1)), 0).r;
}

// The CPU `sample_bilinear` corner read: everything off the plane is zero.
fn load_or_zero(plane: texture_2d<f32>, at: vec2<i32>) -> f32 {
    let extent = vec2<i32>(textureDimensions(plane));
    if any(at < vec2<i32>(0, 0)) || any(at >= extent) {
        return 0.0;
    }
    return textureLoad(plane, at, 0).r;
}

// The displaced wrapped read the noise windows use: component-wise
// (at + offset) % extent, the same integer wrap as the CPU's
// `[((y + offset.1) % height) * width + (x + offset.0) % width]`.
fn load_wrapped(plane: texture_2d<f32>, at: vec2<i32>, offset: vec2<i32>) -> f32 {
    let extent = vec2<i32>(textureDimensions(plane));
    return textureLoad(plane, (at + offset) % extent, 0).r;
}

// image::smoothstep verbatim: clamp first, then the hermite polynomial.
fn hermite(edge_low: f32, edge_high: f32, x: f32) -> f32 {
    let t = clamp((x - edge_low) / (edge_high - edge_low), 0.0, 1.0);
    return t * t * (3.0 - 2.0 * t);
}

fn plane_out(value: f32) -> vec4<f32> {
    return vec4<f32>(value, value, value, 1.0);
}

// One axis of the separable box average: the naive form of one
// image.rs `box_blur_pass` sweep, whose running sum computes the identical
// window average (its own tripwire test pins that equivalence). The axis
// rides the uniform window so one entry point serves both sweeps; three
// horizontal-then-vertical iterations reproduce `image::blur`.
struct BoxBlurParams {
    axis_x: i32,
    axis_y: i32,
    radius_texels: i32,
}

@group(0) @binding(0) var<uniform> box_blur: BoxBlurParams;
@group(1) @binding(0) var box_blur_source: texture_2d<f32>;

@fragment
fn fs_box_blur(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let at = vec2<i32>(position.xy);
    let axis = vec2<i32>(box_blur.axis_x, box_blur.axis_y);

    var sum = 0.0;
    for (var tap = -box_blur.radius_texels; tap <= box_blur.radius_texels; tap++) {
        sum += load_clamped(box_blur_source, at + axis * tap);
    }
    return plane_out(sum / f32(2 * box_blur.radius_texels + 1));
}

// field.rs `shrink`: resample the region smaller about the wash's centroid
// and off centre by the pour's pre-rolled jitter. The source coordinate is
// centre + (p - centre - jitter) / scale over the integer pixel index p,
// bilinear with off-plane reading zero, exactly the CPU loop.
struct ShrinkParams {
    centre_x: f32,
    centre_y: f32,
    jitter_x: f32,
    jitter_y: f32,
    scale: f32,
}

@group(0) @binding(0) var<uniform> shrink: ShrinkParams;
@group(1) @binding(0) var shrink_source: texture_2d<f32>;

@fragment
fn fs_shrink(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let at = vec2<f32>(vec2<i32>(position.xy));
    let centre = vec2<f32>(shrink.centre_x, shrink.centre_y);
    let jitter = vec2<f32>(shrink.jitter_x, shrink.jitter_y);
    let source = centre + (at - centre - jitter) / shrink.scale;

    let corner = floor(source);
    let fraction = source - corner;
    let x0 = i32(corner.x);
    let y0 = i32(corner.y);
    let upper = load_or_zero(shrink_source, vec2<i32>(x0, y0)) * (1.0 - fraction.x)
        + load_or_zero(shrink_source, vec2<i32>(x0 + 1, y0)) * fraction.x;
    let lower = load_or_zero(shrink_source, vec2<i32>(x0, y0 + 1)) * (1.0 - fraction.x)
        + load_or_zero(shrink_source, vec2<i32>(x0 + 1, y0 + 1)) * fraction.x;
    return plane_out(upper * (1.0 - fraction.y) + lower * fraction.y);
}

// Sheet::threshold's hard band: the hermite ramp across the softened
// puddle, its edges shifted by the tide-line noise read at the pour's
// displaced window. The lost-edge giveback stays with the composite slice;
// this pass is the band that decides where the puddle's edge falls.
struct ThresholdParams {
    offset_x: i32,
    offset_y: i32,
    level: f32,
    band: f32,
    wobble: f32,
}

@group(0) @binding(0) var<uniform> threshold: ThresholdParams;
@group(1) @binding(0) var threshold_soft: texture_2d<f32>;
@group(1) @binding(2) var threshold_edge_noise: texture_2d<f32>;

@fragment
fn fs_threshold(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let at = vec2<i32>(position.xy);
    let shift = load_wrapped(threshold_edge_noise, at, vec2<i32>(threshold.offset_x, threshold.offset_y))
        * threshold.wobble;
    let soft = textureLoad(threshold_soft, at, 0).r;
    return plane_out(hermite(threshold.level - threshold.band + shift, threshold.level + threshold.band + shift, soft));
}

// The rim block inside Sheet::pour: pigment carried to the retreating
// edge as alpha minus its own blur (the interior plane arrives from a
// box-blur chain), varied along the tide line by the edge noise read at a
// further-displaced window and clamped at the vary ceiling. `strength`
// folds the pour's rim, load, and gain factors into one multiplier.
struct RimParams {
    offset_x: i32,
    offset_y: i32,
    vary_base: f32,
    vary_swing: f32,
    vary_ceiling: f32,
    strength: f32,
}

@group(0) @binding(0) var<uniform> rim: RimParams;
@group(1) @binding(0) var rim_alpha: texture_2d<f32>;
@group(1) @binding(2) var rim_interior: texture_2d<f32>;
@group(1) @binding(4) var rim_edge_noise: texture_2d<f32>;

@fragment
fn fs_rim(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let at = vec2<i32>(position.xy);
    let receded = max(textureLoad(rim_alpha, at, 0).r - textureLoad(rim_interior, at, 0).r, 0.0);
    let noise = load_wrapped(rim_edge_noise, at, vec2<i32>(rim.offset_x, rim.offset_y));
    let vary = clamp(rim.vary_base + noise * rim.vary_swing, 0.0, rim.vary_ceiling);
    return plane_out(receded * vary * rim.strength);
}
