// The puddle ops as authored fragment passes (iamacoffeepot/aether#4366,
// ADR-0170): the wash's water vocabulary from easel/field.rs and
// easel/image.rs re-spoken as WGSL, one entry point per op. The CPU
// implementations stay the oracle, so every formula here is a transcription
// rather than an approximation: the box average reads the same mirrored
// window the running sum averages, the shrink resample is the same bilinear
// with everything off the plane reading as zero, and the threshold band is
// the same hermite ramp over the same displaced noise window.
//
// Every op but one reads its planes through textureLoad at integer texel
// coordinates, exactly as the CPU indexes them, and leaves the pass input
// contract's samplers undeclared. The exception is the fused sweep, which
// declares its sampler and reads between texels on purpose: a symmetric
// kernel's taps pair into one filtered read apiece, which is the same
// kernel out of half the fetches (iamacoffeepot/aether#4387). Entry points
// take the fragment's
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

// The CPU planes' mirror-at-edge read: image.rs `reflected` applied per
// axis — half-sample symmetric, so index -1 reads texel 0. Replicating
// the edge instead would pull a border average toward the edge texel's
// own value, and would not survive being filtered: iterating a sweep
// would stop being the same operator as one sweep of those iterations
// convolved (iamacoffeepot/aether#4444).
fn load_reflected(plane: texture_2d<f32>, at: vec2<i32>) -> f32 {
    let extent = vec2<i32>(textureDimensions(plane));
    let zero = vec2<i32>(0, 0);
    // One fold at each edge, which is every fold a window narrower than
    // the plane can want; the clamp answers the degenerate wider one, and
    // answers it the same way image.rs does. Folding by selects rather
    // than by a periodic remainder is deliberate — an integer division
    // per axis per tap costs more here than every other instruction in
    // the sweep put together.
    let under = select(at, -vec2<i32>(1, 1) - at, at < zero);
    let over = select(under, 2 * extent - vec2<i32>(1, 1) - under, under >= extent);
    return textureLoad(plane, clamp(over, zero, extent - vec2<i32>(1, 1)), 0).r;
}

// The same mirrored read over the first two channels of a paired soft
// plane. Each lane is one scalar plane carried through identical blur
// arithmetic; the other two channels are padding only.
fn load_reflected_pair(plane: texture_2d<f32>, at: vec2<i32>) -> vec2<f32> {
    let extent = vec2<i32>(textureDimensions(plane));
    let zero = vec2<i32>(0, 0);
    let under = select(at, -vec2<i32>(1, 1) - at, at < zero);
    let over = select(under, 2 * extent - vec2<i32>(1, 1) - under, under >= extent);
    return textureLoad(plane, clamp(over, zero, extent - vec2<i32>(1, 1)), 0).rg;
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
        sum += load_reflected(box_blur_source, at + axis * tap);
    }
    if reach == 0 {
        sum = load_reflected(box_blur_source, at);
    } else {
        sum += straddle
            * (load_reflected(box_blur_source, at - axis * reach) + load_reflected(box_blur_source, at + axis * reach));
    }
    return plane_out(sum / (2.0 * half_width));
}

@group(1) @binding(2) var box_blur_source_b: texture_2d<f32>;

// The first sweep of a paired chain. The two scalar sources have the
// same extent and kernel, so one fragment carries their independent
// answers in R and G without changing either lane's arithmetic.
@fragment
fn fs_box_blur_pair(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let at = vec2<i32>(position.xy);
    let axis = vec2<i32>(box_blur.axis_x, box_blur.axis_y);
    let half_width = box_blur.half_width_texels;
    let reach = i32(ceil(half_width - 0.5));
    let straddle = half_width - f32(reach) + 0.5;

    var sum = vec2<f32>(0.0, 0.0);
    for (var tap = 1 - reach; tap < reach; tap++) {
        let sample_at = at + axis * tap;
        sum += vec2<f32>(load_reflected(box_blur_source, sample_at), load_reflected(box_blur_source_b, sample_at));
    }
    if reach == 0 {
        sum = vec2<f32>(load_reflected(box_blur_source, at), load_reflected(box_blur_source_b, at));
    } else {
        let near = at - axis * reach;
        let far = at + axis * reach;
        sum += straddle
            * vec2<f32>(
                load_reflected(box_blur_source, near) + load_reflected(box_blur_source, far),
                load_reflected(box_blur_source_b, near) + load_reflected(box_blur_source_b, far),
            );
    }
    return vec4<f32>(sum / (2.0 * half_width), 0.0, 1.0);
}

// Every later sweep reads the paired plane written by the first.
@fragment
fn fs_box_blur_paired(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let at = vec2<i32>(position.xy);
    let axis = vec2<i32>(box_blur.axis_x, box_blur.axis_y);
    let half_width = box_blur.half_width_texels;
    let reach = i32(ceil(half_width - 0.5));
    let straddle = half_width - f32(reach) + 0.5;

    var sum = vec2<f32>(0.0, 0.0);
    for (var tap = 1 - reach; tap < reach; tap++) {
        sum += load_reflected_pair(box_blur_source, at + axis * tap);
    }
    if reach == 0 {
        sum = load_reflected_pair(box_blur_source, at);
    } else {
        sum += straddle
            * (load_reflected_pair(box_blur_source, at - axis * reach)
                + load_reflected_pair(box_blur_source, at + axis * reach));
    }
    return vec4<f32>(sum / (2.0 * half_width), 0.0, 1.0);
}

// The whole chain's softening as one kernel (iamacoffeepot/aether#4441).
// Convolution is associative, so the three box sweeps one axis carries are
// a single piecewise-quadratic kernel, and the axes commute — six sweeps
// collapse to two. The weights are the CPU's own: the three tap arrays
// above convolved exactly and normalized (see `composite_taps`), which is
// why one sweep here lands what three of `fs_box_blur` landed rather than
// approximating them.
//
// The kernel is symmetric, so the uniform carries it from the centre out —
// `weights[0].x` is the centre tap and the rest run outwards, four to a
// vector because the uniform address space strides arrays by sixteen
// bytes. The loop pays one load per side and one add, as the box sweep
// does, over a window three times its reach.
//
// Exact at the border too, which is what the plane's mirrored edge buys
// (iamacoffeepot/aether#4444): a symmetric extension survives a symmetric
// kernel, so re-extending between three sweeps lands where extending once
// does. Under a replicated edge the two would part within three reaches
// of every edge.
const FUSED_BLUR_VECTORS: u32 = 12u;

struct FusedBlurParams {
    // Filtered reads past the centre, each standing for a pair of the
    // composite kernel's taps — so the kernel spans up to `4 * reads + 1`
    // taps and costs `2 * reads + 1` fetches.
    reads: i32,
    // The kernel's own centre tap, which lands on a texel centre and so
    // is a point read whatever the sampler does.
    centre: f32,
    // `(offset, weight)` per read, two to a vector: the offset in texels
    // from the fragment's own centre, and the summed weight of the pair
    // the read stands for (`puddle::fused_taps`).
    reads_at: array<vec4<f32>, FUSED_BLUR_VECTORS>,
}

@group(0) @binding(0) var<uniform> fused_blur: FusedBlurParams;
@group(1) @binding(0) var fused_blur_source: texture_2d<f32>;
@group(1) @binding(1) var fused_blur_sampler: sampler;

// The mirrored read of `load_reflected`, taken at a fractional position
// rather than a texel index, so a filtering sampler can answer it.
//
// `at` is in texel-centre coordinates: texel `i` sits at `f32(i)`. The
// fold is the continuous form of the integer one above — reflection about
// `-0.5` and about `extent - 0.5`, the two half-sample lines the CPU
// mirrors across — and a reflection commutes with a linear interpolation,
// so folding the coordinate lands where folding each of the pair's two
// texels would. Past the fold the sampler's clamp answers the residual
// exactly as the CPU's does, because the texel a clamp reaches for and
// the texel a fold reaches for are the same one at the edge.
fn sample_reflected(at: vec2<f32>, extent: vec2<f32>) -> f32 {
    let under = select(at, -1.0 - at, at < vec2<f32>(-0.5, -0.5));
    let folded = select(under, 2.0 * extent - 1.0 - under, under > extent - vec2<f32>(0.5, 0.5));

    return textureSampleLevel(fused_blur_source, fused_blur_sampler, (folded + 0.5) / extent, 0.0).r;
}

fn sample_reflected_pair(at: vec2<f32>, extent: vec2<f32>) -> vec2<f32> {
    let under = select(at, -1.0 - at, at < vec2<f32>(-0.5, -0.5));
    let folded = select(under, 2.0 * extent - 1.0 - under, under > extent - vec2<f32>(0.5, 0.5));

    return textureSampleLevel(fused_blur_source, fused_blur_sampler, (folded + 0.5) / extent, 0.0).rg;
}

fn fused_sweep(at: vec2<f32>, axis: vec2<f32>) -> vec4<f32> {
    let extent = vec2<f32>(textureDimensions(fused_blur_source));

    var sum = fused_blur.centre * sample_reflected(at, extent);
    for (var read = 0; read < fused_blur.reads; read++) {
        let packed = fused_blur.reads_at[read / 2];
        let tap = select(packed.xy, packed.zw, (read & 1) == 1);
        let reach = axis * tap.x;
        sum += tap.y * (sample_reflected(at - reach, extent) + sample_reflected(at + reach, extent));
    }
    return plane_out(sum);
}

@fragment
fn fs_fused_blur_x(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    return fused_sweep(position.xy - 0.5, vec2<f32>(1.0, 0.0));
}

@fragment
fn fs_fused_blur_y(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    return fused_sweep(position.xy - 0.5, vec2<f32>(0.0, 1.0));
}

@group(1) @binding(2) var fused_blur_source_b: texture_2d<f32>;
@group(1) @binding(3) var fused_blur_sampler_b: sampler;

fn sample_reflected_split(at: vec2<f32>, extent: vec2<f32>) -> vec2<f32> {
    let under = select(at, -1.0 - at, at < vec2<f32>(-0.5, -0.5));
    let folded = select(under, 2.0 * extent - 1.0 - under, under > extent - vec2<f32>(0.5, 0.5));
    let uv = (folded + 0.5) / extent;

    return vec2<f32>(
        textureSampleLevel(fused_blur_source, fused_blur_sampler, uv, 0.0).r,
        textureSampleLevel(fused_blur_source_b, fused_blur_sampler_b, uv, 0.0).r,
    );
}

fn fused_split_sweep(at: vec2<f32>, axis: vec2<f32>) -> vec4<f32> {
    let extent = vec2<f32>(textureDimensions(fused_blur_source));
    var sum = fused_blur.centre * sample_reflected_split(at, extent);
    for (var read = 0; read < fused_blur.reads; read++) {
        let packed = fused_blur.reads_at[read / 2];
        let tap = select(packed.xy, packed.zw, (read & 1) == 1);
        let reach = axis * tap.x;
        sum += tap.y
            * (sample_reflected_split(at - reach, extent) + sample_reflected_split(at + reach, extent));
    }
    return vec4<f32>(sum, 0.0, 1.0);
}

fn fused_paired_sweep(at: vec2<f32>, axis: vec2<f32>) -> vec4<f32> {
    let extent = vec2<f32>(textureDimensions(fused_blur_source));
    var sum = fused_blur.centre * sample_reflected_pair(at, extent);
    for (var read = 0; read < fused_blur.reads; read++) {
        let packed = fused_blur.reads_at[read / 2];
        let tap = select(packed.xy, packed.zw, (read & 1) == 1);
        let reach = axis * tap.x;
        sum += tap.y * (sample_reflected_pair(at - reach, extent) + sample_reflected_pair(at + reach, extent));
    }
    return vec4<f32>(sum, 0.0, 1.0);
}

@fragment
fn fs_fused_blur_pair_x(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    return fused_split_sweep(position.xy - 0.5, vec2<f32>(1.0, 0.0));
}

@fragment
fn fs_fused_blur_paired_x(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    return fused_paired_sweep(position.xy - 0.5, vec2<f32>(1.0, 0.0));
}

@fragment
fn fs_fused_blur_paired_y(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    return fused_paired_sweep(position.xy - 0.5, vec2<f32>(0.0, 1.0));
}

@group(1) @binding(0) var soft_carry_source: texture_2d<f32>;

// A plane carried across, texel for texel, onto the soft plane the sweeps
// read (iamacoffeepot/aether#4387).
//
// A chain pairs its taps through a filtering sampler; filtering is a
// property of the format; and a plane a chain is handed from outside the
// graph — a binding another program wrote, a texture staged from the CPU
// — stands at whatever format its owner declared. So it is carried onto
// the format the sweeps need first. Pointwise and one fetch, against the
// several hundred a sweep of the same plane makes.
@fragment
fn fs_soft_carry(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    return plane_out(textureLoad(soft_carry_source, vec2<i32>(position.xy), 0).r);
}

// The two ends of a reduced-extent blur chain (iamacoffeepot/aether#4437).
// Blur discards high frequencies by construction, so the sweeps between
// these two need no more texels than the softening leaves standing: the
// chain runs on a plane `divisor` times smaller on each axis, which is
// `divisor` cubed less work once the box window shrinks with it. The
// downsample averages each divisor-square block — the same box average
// the sweeps do, so the reduction is itself part of the softening rather
// than a resample laid on top — and the upsample carries the result back
// bilinearly, edges mirrored exactly as the sweeps mirror theirs.
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
            sum += load_reflected(box_scale_source, base + vec2<i32>(across, down));
        }
    }
    return plane_out(sum / f32(box_scale.divisor * box_scale.divisor));
}

@group(1) @binding(2) var box_scale_source_b: texture_2d<f32>;

@fragment
fn fs_box_downsample_pair(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let base = vec2<i32>(position.xy) * box_scale.divisor;

    var sum = vec2<f32>(0.0, 0.0);
    for (var down = 0; down < box_scale.divisor; down++) {
        for (var across = 0; across < box_scale.divisor; across++) {
            let at = base + vec2<i32>(across, down);
            sum += vec2<f32>(load_reflected(box_scale_source, at), load_reflected(box_scale_source_b, at));
        }
    }
    return vec4<f32>(sum / f32(box_scale.divisor * box_scale.divisor), 0.0, 1.0);
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
        load_reflected(box_scale_source, vec2<i32>(x0, y0)),
        load_reflected(box_scale_source, vec2<i32>(x0 + 1, y0)),
        fraction.x,
    );
    let lower = mix(
        load_reflected(box_scale_source, vec2<i32>(x0, y0 + 1)),
        load_reflected(box_scale_source, vec2<i32>(x0 + 1, y0 + 1)),
        fraction.x,
    );
    return plane_out(mix(upper, lower, fraction.y));
}

@fragment
fn fs_box_upsample_paired(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let at = vec2<f32>(vec2<i32>(position.xy));
    let source = (at + vec2<f32>(0.5, 0.5)) / f32(box_scale.divisor) - vec2<f32>(0.5, 0.5);

    let corner = floor(source);
    let fraction = source - corner;
    let x0 = i32(corner.x);
    let y0 = i32(corner.y);
    let upper = mix(
        load_reflected_pair(box_scale_source, vec2<i32>(x0, y0)),
        load_reflected_pair(box_scale_source, vec2<i32>(x0 + 1, y0)),
        fraction.x,
    );
    let lower = mix(
        load_reflected_pair(box_scale_source, vec2<i32>(x0, y0 + 1)),
        load_reflected_pair(box_scale_source, vec2<i32>(x0 + 1, y0 + 1)),
        fraction.x,
    );
    return vec4<f32>(mix(upper, lower, fraction.y), 0.0, 1.0);
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
