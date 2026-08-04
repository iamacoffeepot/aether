// The care field as an authored jump-flood distance transform
// (iamacoffeepot/aether#4387, ADR-0171).
//
// `field::care_field` is a smoothstep over the distance from every pixel
// to the nearest drawn feature — lips, brow, eye — and the CPU answers
// that distance with two sequential chamfer sweeps. A sweep is the one
// shape a fragment pass cannot take: every pixel reads the answer its
// neighbour has already written. So the GPU answers the same question a
// different way, by jump flooding, which is parallel by construction and
// converges on the true Euclidean distance rather than on the chamfer's
// 3x3 approximation of it.
//
// The two therefore differ, and the difference is stated rather than
// hidden: `image::DIAGONAL_STEP` is 1.4 against a true 1.41421, so the
// chamfer runs up to about one percent long on a diagonal run and the
// jump flood does not. The ramp those distances feed spans four hundred
// and sixty reference pixels (`field::CARE_NEAR` to `CARE_FAR`), so a
// one-percent distance difference moves the care value by well under a
// hundredth — far below the threshold at which the tight and loose
// washes it mixes read differently.
//
// # Carrying a seed in one channel
//
// A flood carries the *position* of the nearest seed, which is two
// numbers, and the plane vocabulary here is single-channel `R32Float`.
// The two pack into one exactly: a seed is stored as its own linear
// texel index `y * width + x`, which is an integer below 2^24 for any
// canvas this engine paints at and so survives an f32 round trip
// unrounded. `CARE_UNSEEDED` is negative, which no index is.

// Class ids the hand is held tight around (labels.rs LIPS / BROW / EYE),
// as the integers the packed plane's class channel carries.
const CARE_LIPS: f32 = 6.0;
const CARE_BROW: f32 = 7.0;
const CARE_EYE: f32 = 8.0;

// No seed here. Negative so it can never be mistaken for a texel index,
// and tested for rather than arithmetic'd around.
const CARE_UNSEEDED: f32 = -1.0;

// Squared distance standing in for "no seed anywhere". Larger than any
// real squared distance on a canvas this engine paints at.
const CARE_UNREACHED_SQUARED: f32 = 1e18;

// Distance a pixel reached by nothing reports, matching the sentinel the
// CPU sweeps leave standing (`image::UNREACHED`). It has to survive the
// ramp as "infinitely far", which it does: the ramp is descending, so a
// huge distance gives zero care — a wholly free hand, which is what a
// figure with no face in frame should be painted with.
const CARE_UNREACHED: f32 = 1e9;

@group(1) @binding(0) var care_source: texture_2d<f32>;

// Where a seed index sits on the canvas.
fn care_seed_at(seed: f32, width: f32) -> vec2<f32> {
    let row = floor(seed / width);
    return vec2<f32>(seed - row * width, row);
}

// Squared distance from `here` to the seed `seed` names, or the
// unreached sentinel when it names none.
fn care_seed_distance(seed: f32, here: vec2<f32>, width: f32) -> f32 {
    if seed < 0.0 {
        return CARE_UNREACHED_SQUARED;
    }
    let offset = care_seed_at(seed, width) - here;
    return dot(offset, offset);
}

// The flood's first plane: every feature texel seeds itself, everything
// else is unseeded. `field::care_field`'s `features` predicate, verbatim.
@fragment
fn fs_care_seed(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let at = vec2<i32>(position.xy);
    let size = textureDimensions(care_source);
    let class = round(textureLoad(care_source, at, 0).r * 255.0);
    let feature = class == CARE_LIPS || class == CARE_BROW || class == CARE_EYE;

    let index = f32(at.y) * f32(size.x) + f32(at.x);
    return vec4<f32>(select(CARE_UNSEEDED, index, feature), 0.0, 0.0, 1.0);
}

struct CareJumpParams {
    // How far this hop reaches, in texels. The chain halves it from a
    // power of two past any canvas edge down to one.
    step: i32,
}

@group(0) @binding(0) var<uniform> care_jump: CareJumpParams;

// One flood hop: take the nearest seed among this texel's own and the
// eight it can see at the hop's reach. Probes are clamped to the canvas
// rather than dropped, which costs a re-read of a border texel and
// spares the branch.
@fragment
fn fs_care_jump(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let size = vec2<i32>(textureDimensions(care_source));
    let at = vec2<i32>(position.xy);
    let here = vec2<f32>(at);
    let width = f32(size.x);

    var best = textureLoad(care_source, at, 0).r;
    var nearest = care_seed_distance(best, here, width);

    for (var down = -1; down <= 1; down++) {
        for (var across = -1; across <= 1; across++) {
            let probe = clamp(at + vec2<i32>(across, down) * care_jump.step, vec2<i32>(0), size - 1);
            let candidate = textureLoad(care_source, probe, 0).r;
            let reach = care_seed_distance(candidate, here, width);
            if reach < nearest {
                nearest = reach;
                best = candidate;
            }
        }
    }

    return vec4<f32>(best, 0.0, 0.0, 1.0);
}

struct CareRampParams {
    // `field::CARE_FAR` and `CARE_NEAR`, already through `image::tuned`
    // at this canvas' own height. The ramp descends: wholly free at
    // `far`, cut to the line at `near`.
    far: f32,
    near: f32,
}

@group(0) @binding(0) var<uniform> care_ramp: CareRampParams;

// The flooded seeds resolved into how closely the hand is held —
// `field::care_field`'s own final map, over the distance the flood
// found.
@fragment
fn fs_care_ramp(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let size = textureDimensions(care_source);
    let at = vec2<i32>(position.xy);
    let seed = textureLoad(care_source, at, 0).r;

    var reach = CARE_UNREACHED;
    if seed >= 0.0 {
        reach = length(care_seed_at(seed, f32(size.x)) - vec2<f32>(at));
    }

    return vec4<f32>(hermite(care_ramp.far, care_ramp.near, reach), 0.0, 0.0, 1.0);
}
