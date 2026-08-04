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
//
// The window is stated as a half-width in texels rather than a tap count,
// so it can fall between texels: a tap's weight is how much of its own
// texel the window covers, which is one for every tap inside and a
// fraction for the pair straddling each end. At the half-integer widths
// the CPU rounds to — radius plus a half — every weight is one or zero
// and the average is the CPU's exactly; a chain swept at a reduced extent
// (iamacoffeepot/aether#4437) needs the widths between, since the window
// it wants is the full-extent one divided by an extent divisor and lands
// wherever that division puts it.
struct BoxBlurParams {
    axis_x: i32,
    axis_y: i32,
    half_width_texels: f32,
}

@group(0) @binding(0) var<uniform> box_blur: BoxBlurParams;
@group(1) @binding(0) var box_blur_source: texture_2d<f32>;

@fragment
fn fs_box_blur(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let at = vec2<i32>(position.xy);
    let axis = vec2<i32>(box_blur.axis_x, box_blur.axis_y);
    // Only the outermost pair is ever partly covered, so the sweep stays
    // one load and one add per tap and pays the fraction once.
    let half_width = box_blur.half_width_texels;
    let reach = i32(ceil(half_width - 0.5));
    let straddle = half_width - f32(reach) + 0.5;

    var sum = 0.0;
    for (var tap = 1 - reach; tap < reach; tap++) {
        sum += load_clamped(box_blur_source, at + axis * tap);
    }
    if reach == 0 {
        sum = load_clamped(box_blur_source, at);
    } else {
        sum += straddle
            * (load_clamped(box_blur_source, at - axis * reach) + load_clamped(box_blur_source, at + axis * reach));
    }
    return plane_out(sum / (2.0 * half_width));
}

// The two ends of a reduced-extent blur chain (iamacoffeepot/aether#4437).
// Blur discards high frequencies by construction, so the sweeps between
// these two need no more texels than the softening leaves standing: the
// chain runs on a plane `divisor` times smaller on each axis, which is
// `divisor` cubed less work once the box window shrinks with it. The
// downsample averages each divisor-square block — the same box average
// the sweeps do, so the reduction is itself part of the softening rather
// than a resample laid on top — and the upsample carries the result back
// bilinearly, edges clamped exactly as the sweeps clamp theirs.
struct ScaleParams {
    divisor: i32,
}

@group(0) @binding(0) var<uniform> box_scale: ScaleParams;
@group(1) @binding(0) var box_scale_source: texture_2d<f32>;

@fragment
fn fs_box_downsample(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let base = vec2<i32>(position.xy) * box_scale.divisor;

    var sum = 0.0;
    for (var down = 0; down < box_scale.divisor; down++) {
        for (var across = 0; across < box_scale.divisor; across++) {
            sum += load_clamped(box_scale_source, base + vec2<i32>(across, down));
        }
    }
    return plane_out(sum / f32(box_scale.divisor * box_scale.divisor));
}

@fragment
fn fs_box_upsample(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    // Texel centres, not corners: the reduced texel covering full-extent
    // texel p is centred at (p + 0.5) / divisor, so the bilinear weights
    // come off that coordinate less the half-texel the corner sits at.
    let at = vec2<f32>(vec2<i32>(position.xy));
    let source = (at + vec2<f32>(0.5, 0.5)) / f32(box_scale.divisor) - vec2<f32>(0.5, 0.5);

    let corner = floor(source);
    let fraction = source - corner;
    let x0 = i32(corner.x);
    let y0 = i32(corner.y);
    let upper = mix(
        load_clamped(box_scale_source, vec2<i32>(x0, y0)),
        load_clamped(box_scale_source, vec2<i32>(x0 + 1, y0)),
        fraction.x,
    );
    let lower = mix(
        load_clamped(box_scale_source, vec2<i32>(x0, y0 + 1)),
        load_clamped(box_scale_source, vec2<i32>(x0 + 1, y0 + 1)),
        fraction.x,
    );
    return plane_out(mix(upper, lower, fraction.y));
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
