// The ink pass: every curve rasterized unclipped, its widths decided by
// the stroke-parameter visibility field rather than by a CPU run split,
// and its rails solved here rather than shipped.
//
// The vertex stage does two things. It **solves the rail pair** against
// the live eye — the perpendicular that keeps a line from vanishing
// edge-on, the depth cue that makes a nearer point bolder, the hand's
// wobble — from an anchor that is a function of the curve alone, so the
// buffer it reads never has to travel when the camera turns. Then it
// **consumes the field**: the point's verdict, reach, run arc and curve
// coverage fold into one width scale, and the vertex is displaced by
// the scaled offset. A hidden point scales to zero, its two rails
// collapse onto the centre, and the segments either side of it
// rasterize no fragments at all. The split that used to produce runs on
// the CPU is therefore not performed anywhere: it falls out of the
// widths.
//
// The one thing not derived here is `ribbon::reference_depth` — the
// stroke's own average distance to the eye, which is a reduction over
// a whole curve rather than a function of one point. It arrives
// already solved, one texel a curve, in the field's reference plane.

struct StrokeParams {
    view_proj: mat4x4<f32>,
    eye: vec3<f32>,
    // Unused by the stages here; the block's shape is shared with the
    // field's own so one packing serves both.
    bias: f32,
    // The field's texel dimensions — the canvas, half this pass' own
    // supersampled extent.
    field: vec2<f32>,
    // `style::pressure`'s ramp, in radians of arc.
    ramp: f32,
    // `visibility::whole_or_nothing`'s floor: an authored mark below
    // this surviving fraction leaves rather than crumbles.
    coverage_floor: f32,
}

@group(0) @binding(0) var<uniform> params: StrokeParams;

// The field planes, in the order the pass declares its inputs. Bound
// visible to both stages (ADR-0172), and read by `textureLoad` alone —
// a vertex stage has no derivatives to sample with.
// Named by slot rather than by meaning, because the two passes bind
// different things here: the ribbon stage reads the four field planes,
// and the resolve reads the raster it just laid down.
@group(1) @binding(0) var source: texture_2d<f32>;
@group(1) @binding(2) var second: texture_2d<f32>;
@group(1) @binding(4) var third: texture_2d<f32>;
@group(1) @binding(6) var fourth: texture_2d<f32>;
@group(1) @binding(8) var fifth: texture_2d<f32>;

// `style::pressure`'s floor and span: a stroke never thins past 0.42 of
// its weight while it is drawn at all, and reaches 1.0 a ramp in.
const PRESSURE_FLOOR: f32 = 0.42;
const PRESSURE_SPAN: f32 = 0.58;

// The fraction of a run's own arc the ramp is capped at, so a stroke
// shorter than two ramps still reaches full pressure at its middle
// instead of lensing.
const RAMP_OF_RUN: f32 = 0.45;

// `ribbon`'s own clamp on the depth cue: nearest and furthest a point
// is allowed to be read as, relative to its stroke's average. Restated
// here because a Rust constant cannot be imported into WGSL; the two
// must move together.
const DEPTH_WEIGHT_FLOOR: f32 = 0.82;
const DEPTH_WEIGHT_CEILING: f32 = 1.22;

fn texel_of(flat: i32) -> vec2<i32> {
    let width = i32(params.field.x);

    return vec2<i32>(flat % width, flat / width);
}

// A plane read at a flat index, with everything off the field reading
// as `missing`. The barrier texel between curves is on the field and
// reads as itself — hidden, zero arc — which is what ends a curve.
fn load_flat(plane: texture_2d<f32>, flat: i32, missing: f32) -> f32 {
    let extent = i32(params.field.x) * i32(params.field.y);
    if flat < 0 || flat >= extent {
        return missing;
    }

    return textureLoad(plane, texel_of(flat), 0).r;
}

struct Ribbon {
    @builtin(position) clip: vec4<f32>,
    @location(0) colour: vec4<f32>,
}

// The width scale one point contributes, in [0, 1]: zero where the
// point is not drawn at all, `style::pressure`'s taper where it is.
//
// Three verdicts are read rather than one. A point survives only in a
// run of at least two — the rule `visibility::runs` applies when it
// drops a lone survivor, and the same neighbourhood `fs_cover_seed`
// counts by — so an isolated visible point between two hidden ones
// scales to zero here rather than drawing the crumb the split exists to
// reject.
fn width_scale(flat: i32, authored: f32) -> f32 {
    let here = load_flat(source, flat, 0.0);
    let before = load_flat(source, flat - 1, 0.0);
    let after = load_flat(source, flat + 1, 0.0);
    if here < 0.5 || max(before, after) < 0.5 {
        return 0.0;
    }

    // Whole or nothing: an authored mark mostly hidden leaves entirely,
    // so a half-occluded eye vanishes instead of shattering.
    let coverage = load_flat(third, flat, 0.0);
    if authored > 0.5 && coverage < params.coverage_floor {
        return 0.0;
    }

    // The taper, anchored on the field's own reach rather than on a run
    // the CPU cut: arc to the nearest hidden point or curve end, ramped
    // over the shorter of `RAMP` and 45% of the run's arc.
    let total = load_flat(fourth, flat, 0.0);
    let ramp = min(params.ramp, total * RAMP_OF_RUN);
    if ramp <= 1e-6 {
        return 1.0;
    }
    let reach = load_flat(second, flat, 0.0);
    let ends = clamp(reach / ramp, 0.0, 1.0);

    return PRESSURE_FLOOR + PRESSURE_SPAN * sqrt(ends);
}

struct Rail {
    // Where the centre lands once the wobble displaces it.
    centre: vec3<f32>,
    // The offset reaching the right rail at full pressure.
    offset: vec3<f32>,
}

// `ribbon::rail`, line for line: one point's rail pair against the eye.
//
// Both halves scale by the distance to the eye, which is what holds a
// stroke's width constant on screen wherever the subject sits — the
// anchor carries each quantity per unit of depth, and this is where
// the depth arrives.
fn rail(origin: vec3<f32>, along: vec3<f32>, shape: vec2<f32>, reference: f32) -> Rail {
    let to_eye = params.eye - origin;
    let depth = max(length(to_eye), 1e-4);
    // Perpendicular to the stroke and to the view at once, which is
    // what keeps a line from vanishing when it turns edge-on.
    var across = cross(along, to_eye);
    if length(across) < 1e-9 {
        across = vec3<f32>(0.0, 0.0, 0.0);
    } else {
        across = normalize(across);
    }

    // Nearer stroke points are bolder — the one cue that keeps a flat
    // line drawing from reading as a decal on glass.
    let depth_weight = clamp(reference / depth, DEPTH_WEIGHT_FLOOR, DEPTH_WEIGHT_CEILING);

    var out: Rail;
    out.centre = origin + across * (shape.y * depth);
    out.offset = across * (shape.x * depth * depth_weight);

    return out;
}

@vertex
fn vs_stroke(
    @location(0) origin: vec3<f32>,
    @location(1) along: vec3<f32>,
    @location(2) address: vec2<f32>,
    @location(3) shape: vec2<f32>,
    @location(4) colour: vec4<f32>,
) -> Ribbon {
    // Two bits stolen from the point's texel index: which of the pair's
    // rails this vertex is, and whether the mark was authored. Neither
    // is askable of a vertex stage otherwise — `@builtin(vertex_index)`
    // under an indexed draw is the index value, and the two rails of a
    // point would share it.
    let packed = u32(address.x);
    let side = select(-1.0, 1.0, (packed & 1u) == 1u);
    let authored = f32((packed >> 1u) & 1u);
    let flat = i32(packed >> 2u);

    // The curve's reference depth, and its sign the verdict: a curve
    // that does not read at this eye at all arrives negative, and its
    // rails collapse onto the centre rather than drawing a speck.
    let reference = load_flat(fifth, i32(address.y), 0.0);
    var scale = 0.0;
    if reference > 0.0 {
        scale = width_scale(flat, authored);
    }

    let solved = rail(origin, along, shape, reference);

    var out: Ribbon;
    out.clip = params.view_proj * vec4<f32>(solved.centre + solved.offset * (side * scale), 1.0);
    out.colour = colour;

    return out;
}

@fragment
fn fs_stroke(ribbon: Ribbon) -> @location(0) vec4<f32> {
    return ribbon.colour;
}

// Straight alpha, with the colour dilated one texel into what the
// ribbons did not cover.
//
// The raster above is a *premultiplied* coverage buffer: a covered
// texel holds the ink at alpha one, an uncovered one holds transparent
// black. The overlay pass that composites this sheet blends straight
// alpha, though — `src.rgb * src.a + dst * (1 - src.a)` — so handing it
// the premultiplied buffer multiplies coverage in twice, and a half
// covered pixel lays down a quarter of the ink. Strokes here are one to
// two pixels wide, so very nearly every ink pixel is a partial one and
// the whole drawing reads pale.
//
// Dividing the colour back out fixes the covered texels, and the
// dilation fixes the rest: a bilinear tap straddling a stroke's edge
// averages an uncovered texel in, and an uncovered texel holding black
// would drag the stroke toward black exactly where it is faintest. Ink
// borrowed from the strongest neighbour leaves the tap averaging
// coverage alone, which is what the composite wants. One texel is
// enough — halving the edge means a tap reads a two-by-two block, so it
// never reaches past an immediate neighbour.
@fragment
fn fs_resolve(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let texel = vec2<i32>(position.xy);
    let here = textureLoad(source, texel, 0);
    if here.a > 1e-4 {
        return vec4<f32>(here.rgb / here.a, here.a);
    }

    var best = vec4<f32>(0.0, 0.0, 0.0, 0.0);
    for (var dy = -1; dy <= 1; dy++) {
        for (var dx = -1; dx <= 1; dx++) {
            let near = textureLoad(source, texel + vec2<i32>(dx, dy), 0);
            if near.a > best.a {
                best = near;
            }
        }
    }
    if best.a <= 1e-4 {
        return vec4<f32>(0.0, 0.0, 0.0, 0.0);
    }

    return vec4<f32>(best.rgb / best.a, 0.0);
}
