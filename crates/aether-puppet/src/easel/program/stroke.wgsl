// The ink pass: every curve rasterized unclipped, its widths decided by
// the stroke-parameter visibility field rather than by a CPU run split.
//
// The vertex stage is where the field is consumed. Each ribbon vertex
// carries the centre of its rail pair, the full-width offset that would
// place it at taper 1, and the flat field index of the point it belongs
// to — so the stage reads that point's verdict, reach, run arc and
// curve coverage, folds them into one width scale, and displaces the
// vertex by the scaled offset. A hidden point scales to zero, its two
// rails collapse onto the centre, and the segments either side of it
// rasterize no fragments at all. The split that used to produce runs on
// the CPU is therefore not performed anywhere: it falls out of the
// widths.

struct StrokeParams {
    view_proj: mat4x4<f32>,
    eye: vec3<f32>,
    // Distance the prepass pushes the subject away from the eye, so a
    // stroke lying on the surface it describes is not eaten by the
    // depth test. The mirror of the lift `visibility::hidden` gives the
    // probe before it casts.
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

// `style::pressure`'s floor and span: a stroke never thins past 0.42 of
// its weight while it is drawn at all, and reaches 1.0 a ramp in.
const PRESSURE_FLOOR: f32 = 0.42;
const PRESSURE_SPAN: f32 = 0.58;

// The fraction of a run's own arc the ramp is capped at, so a stroke
// shorter than two ramps still reaches full pressure at its middle
// instead of lensing.
const RAMP_OF_RUN: f32 = 0.45;

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

// The subject depth prepass. No colour is wanted of it — only the depth
// attachment the ink pass shares — so the fragment stage writes a
// constant and the whole pass exists for its `@builtin(position)` z.
//
// The push along the view ray is what keeps a stroke that lies exactly
// on the surface from z-fighting the surface: the drawing describes the
// mesh, so its points are coplanar with it by construction. Pushing the
// subject back by the same bias the occlusion probe is lifted by keeps
// one number answering for both.
@vertex
fn vs_prepass(@location(0) position: vec3<f32>) -> @builtin(position) vec4<f32> {
    let to_surface = position - params.eye;
    let distance = max(length(to_surface), 1e-4);
    let pushed = params.eye + to_surface * ((distance + params.bias) / distance);

    return params.view_proj * vec4<f32>(pushed, 1.0);
}

@fragment
fn fs_prepass() -> @location(0) vec4<f32> {
    return vec4<f32>(0.0, 0.0, 0.0, 1.0);
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

@vertex
fn vs_stroke(
    @location(0) centre: vec3<f32>,
    @location(1) offset: vec3<f32>,
    @location(2) slot: f32,
    @location(3) colour: vec4<f32>,
    @location(4) authored: f32,
) -> Ribbon {
    let scale = width_scale(i32(slot), authored);

    var out: Ribbon;
    out.clip = params.view_proj * vec4<f32>(centre + offset * scale, 1.0);
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
