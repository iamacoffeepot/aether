// Stroke visibility as a field over each stroke's own parameterization
// (ADR-0172, iamacoffeepot/aether#4418): the GPU twin of
// `visibility::runs`.
//
// The CPU oracle asks, per point, whether a ray lifted off the surface
// reaches the eye without meeting the mesh. Here the mesh is rasterized
// once into a depth image and the same question becomes a comparison:
// the point is hidden when the nearest surface along its pixel is nearer
// to the eye than the point itself. Everything downstream — the taper
// that anchors at a run's end, the whole-or-nothing coverage rule an
// authored mark passes — is derived from that one plane by scans along
// each stroke's own arc, so the pen's semantics survive as textures
// rather than as control flow.
//
// # The field's addressing
//
// Curves are packed end to end into the flat texel index of a
// canvas-sized field, each preceded by one empty texel. A "row" is
// therefore a span rather than a texture row, and every tap below
// addresses the flat index — `texel_of` and `load_flat` are the only
// places that know the field is two-dimensional at all.
//
// The empty texel is load-bearing twice over. It reads as hidden, so a
// scan that walks off one curve's end meets a barrier instead of the
// next curve's first point; and its arc step is zero, so the barrier
// sits at zero arc from the last point — which is exactly what "arc
// distance to the nearest hidden point *or curve end*" means at an end.
//
// # Why the scans double
//
// Both derived fields are min-plus / plus-plus scans over a span, and
// both run in `log2(reach)` passes rather than one pass per point: pass
// `k` combines a texel with the texel `2^k` away, so after `K` passes
// each texel has seen everything within `2^K - 1` of it. The reach scan
// carries a companion chain of arc weights (`fs_arc_step`) because arc
// is not the index — a stroke's points are level-set crossings, so the
// arc between two of them turns with the camera even though their index
// distance does not.

// Everything every pass needs of the camera and the field, plus the one
// number that differs between the scan passes.
//
// `stride` is the scan's doubling step. It rides the uniform window
// rather than a specialization constant because a pass is a graph
// declaration and the graph is registered once: the dispatch lays down
// one copy of this block per step, and each scan pass windows the copy
// that carries its own.
struct SightParams {
    // The matrix the drawing was solved for, so a point projects into
    // the depth image the same way the subject rasterized into it.
    view_proj: mat4x4<f32>,
    // Where the viewer sits. The occlusion question is asked in
    // distance-to-eye rather than in device depth, because that is the
    // quantity the oracle's ray measures.
    eye: vec3<f32>,
    // Field size in texels, as floats — the flat index's radix.
    field: vec2<f32>,
    // How far a point is lifted off the surface before the question is
    // asked. `Mesh::surface_bias`, supplied rather than derived: it
    // belongs to the mesh the *point* came from (`visibility::hidden`).
    bias: f32,
    // The current scan's doubling step, in texels.
    stride: f32,
}
@group(0) @binding(0) var<uniform> params: SightParams;

// Pass inputs bind positionally as texture/sampler pairs — input `n` at
// `@binding(2 * n)`. Every read here is a `textureLoad` at an integer
// texel, so the samplers go undeclared; a pass that names fewer inputs
// simply leaves the later globals unreferenced.
@group(1) @binding(0) var source: texture_2d<f32>;
@group(1) @binding(2) var second: texture_2d<f32>;
@group(1) @binding(4) var third: texture_2d<f32>;

// The ray's near cut, matching `Mesh::occluded`'s own `t_min`. Without
// it a point is occluded by the surface it sits on.
const RAY_MIN: f32 = 1e-4;

// How far the surface must turn toward the eye before a point on it is
// drawn at all — `visibility::faces_eye`. The two must move together.
const FACING_FLOOR: f32 = 0.02;

// What the reach scan reports where no barrier is within its window,
// in radians of arc. Far past `style::pressure`'s `RAMP` of 0.0064, so
// a saturated reach and a genuinely distant one taper identically.
const REACH_FAR: f32 = 1.0;

// Flat index of a texel, and the texel a flat index names. `u32`
// throughout: the field runs past a million texels, and an `f32`
// division at that magnitude is a rounding argument nobody should have
// to make.
fn flat_of(at: vec2<u32>) -> u32 {
    return at.y * u32(params.field.x) + at.x;
}

fn texel_of(flat: u32) -> vec2<u32> {
    let width = u32(params.field.x);
    return vec2<u32>(flat % width, flat / width);
}

// One texel of a field by flat index, with `missing` past either end.
// The scans step blindly by `2^k` and rely on this: off the field is
// not a neighbour, and what it should read as differs by chain (a far
// distance never wins a min; a zero arc never inflates one).
fn load_flat(tex: texture_2d<f32>, flat: i32, missing: f32) -> f32 {
    let count = i32(params.field.x * params.field.y);
    if flat < 0 || flat >= count {
        return missing;
    }

    return textureLoad(tex, vec2<i32>(texel_of(u32(flat))), 0).r;
}

// The subject, rasterized into the prepass. Position only: the pass
// writes distance to the eye and tests depth, and nothing here shades.
struct Surface {
    @builtin(position) clip: vec4<f32>,
    @location(0) world: vec3<f32>,
}

@vertex
fn vs_subject(@location(0) position: vec3<f32>) -> Surface {
    var out: Surface;
    out.clip = params.view_proj * vec4<f32>(position, 1.0);
    out.world = position;

    return out;
}

// Distance from the eye to the nearest surface at this pixel.
//
// Distance rather than device depth, so the comparison in `fs_seen` is
// the oracle's own ray length and not a reprojection of it. The pass
// clears to zero and only covered pixels are written, so zero reads as
// "no surface here" — which is what the bare page is.
@fragment
fn fs_depth(surface: Surface) -> @location(0) vec4<f32> {
    return vec4<f32>(length(surface.world - params.eye), 0.0, 0.0, 1.0);
}

// One stroke point, placed at its own texel of the field.
//
// Everything but the corner is per point, so it interpolates flat: the
// three vertices of a point's triangle carry identical attributes and
// exist only because a rasterizer needs three of them.
struct Point {
    @builtin(position) clip: vec4<f32>,
    @location(0) @interpolate(flat) probe: vec3<f32>,
    @location(1) @interpolate(flat) normal: vec3<f32>,
    // The point's own parameterization: world span to the next point,
    // points back to the curve's start, points on to its end, and
    // whether the curve grazes (a silhouette or a decal, which the
    // facing test must not reach).
    @location(2) @interpolate(flat) stroke: vec4<f32>,
}

// Place a point's triangle over exactly its own texel.
//
// The three corners sit nine tenths of a texel out from the centre, so
// the texel's own centre is covered and no neighbour's is — a point
// writes one texel and never its neighbour's, whatever the field size.
// The corner index rides the low two bits of `slot` because a vertex
// stage cannot ask which of a triangle's corners it is: `@builtin
// (vertex_index)` under an indexed draw is the *index value*, which
// three corners of one point would share.
@vertex
fn vs_point(
    @location(0) probe: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) slot: f32,
    @location(3) ends: vec2<f32>,
    @location(4) along: vec2<f32>,
) -> Point {
    let packed = u32(slot);
    let at = vec2<f32>(texel_of(packed >> 2u));
    var corners = array<vec2<f32>, 3>(vec2<f32>(0.0, -0.9), vec2<f32>(-0.9, 0.9), vec2<f32>(0.9, 0.9));
    let corner = corners[packed & 3u];

    let centre = (at + vec2<f32>(0.5, 0.5) + corner) / params.field;

    var out: Point;
    out.clip = vec4<f32>(centre.x * 2.0 - 1.0, 1.0 - centre.y * 2.0, 0.0, 1.0);
    out.probe = probe;
    out.normal = normal;
    // `stroke` is read as (span, head, tail, grazes) by every fragment
    // stage below, and the two incoming pairs are grouped by meaning
    // rather than by that order — so this is the one place the two
    // spellings meet, and the one place a lane can slide.
    out.stroke = vec4<f32>(along.x, ends.x, ends.y, along.y);

    return out;
}

// Whether the mesh stands between this point and the eye —
// `visibility::hidden`, asked of a depth image instead of a ray.
//
// A point that projects outside the image is reported clear. The oracle
// would still cast its ray there and could still find an occluder, so
// this is the one place the two mechanisms differ by construction
// rather than by a texel; at any framing that has the subject on the
// page it costs nothing, and the parity scenario counts it rather than
// hiding it.
fn occluded(probe: vec3<f32>, normal: vec3<f32>) -> bool {
    let lifted = probe + normal * params.bias;
    let clip = params.view_proj * vec4<f32>(lifted, 1.0);
    if clip.w <= 0.0 {
        return false;
    }

    let ndc = clip.xyz / clip.w;
    if abs(ndc.x) > 1.0 || abs(ndc.y) > 1.0 {
        return false;
    }

    let size = vec2<f32>(textureDimensions(source));
    let page = (vec2<f32>(ndc.x, -ndc.y) * 0.5 + vec2<f32>(0.5, 0.5)) * size;
    let at = vec2<i32>(clamp(page, vec2<f32>(0.0, 0.0), size - vec2<f32>(1.0, 1.0)));
    let front = textureLoad(source, at, 0).r;

    return front > 0.0 && front <= length(lifted - params.eye) - RAY_MIN;
}

// The verdict, one texel per point: drawn or not.
//
// The same conjunction `visibility::drawn` reaches, in the same order.
// The tone gate is absent because it is not a per-frame question — the
// shipped path gates hatching once at load and hands `runs` a predicate
// that keeps every point.
@fragment
fn fs_seen(point: Point) -> @location(0) vec4<f32> {
    let grazes = point.stroke.w > 0.5;
    let faces = dot(point.normal, normalize(params.eye - point.probe)) >= FACING_FLOOR;
    if !grazes && !faces {
        return vec4<f32>(0.0, 0.0, 0.0, 1.0);
    }
    if occluded(point.probe, point.normal) {
        return vec4<f32>(0.0, 0.0, 0.0, 1.0);
    }

    return vec4<f32>(1.0, 0.0, 0.0, 1.0);
}

// Arc from this point to the next, in radians — `ribbon`'s own measure,
// a world span over the distance to the eye. Angular rather than world
// so a taper means the same thing wherever the subject sits, and
// per-frame rather than baked because the divisor is the camera.
@fragment
fn fs_step(point: Point) -> @location(0) vec4<f32> {
    let depth = max(length(point.probe - params.eye), 1e-4);

    return vec4<f32>(point.stroke.x / depth, 0.0, 0.0, 1.0);
}

// Points back to the curve's first, and on to its last. Static across a
// frame — they change only when the drawing is re-extracted — but
// written per dispatch because a transient holds nothing between them.
@fragment
fn fs_head(point: Point) -> @location(0) vec4<f32> {
    return vec4<f32>(point.stroke.y, 0.0, 0.0, 1.0);
}

@fragment
fn fs_tail(point: Point) -> @location(0) vec4<f32> {
    return vec4<f32>(point.stroke.z, 0.0, 0.0, 1.0);
}

// Seed of the reach scan: zero arc at every barrier, far everywhere
// else. A barrier is a hidden point — and, through the empty texel
// between curves, a curve's own end.
@fragment
fn fs_reach_seed(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let seen = textureLoad(source, vec2<i32>(position.xy), 0).r;

    return vec4<f32>(select(0.0, REACH_FAR, seen > 0.5), 0.0, 0.0, 1.0);
}

// One doubling of the arc-weight chain: `w[k+1](i)` spans `2^(k+1)`
// points because `w[k](i)` and `w[k](i + 2^k)` span `2^k` each and meet.
// Off the field reads as zero arc, which cannot inflate a sum.
@fragment
fn fs_arc_step(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let flat = i32(flat_of(vec2<u32>(position.xy)));
    let stride = i32(params.stride);
    let here = load_flat(source, flat, 0.0);
    let ahead = load_flat(source, flat + stride, 0.0);

    return vec4<f32>(here + ahead, 0.0, 0.0, 1.0);
}

// One doubling of the reach scan: relax each texel against the two
// `2^k` away, paying the arc between them.
//
// Exact, not approximate. After the pass at stride `2^k` every texel
// holds the least arc to any barrier within `2^(k+1) - 1` points of it:
// a barrier further out than `2^k` is reached through the neighbour at
// `2^k` and the arc telescopes, and one nearer than that was already
// found by the previous pass and survives the `min`.
@fragment
fn fs_reach_step(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let flat = i32(flat_of(vec2<u32>(position.xy)));
    let stride = i32(params.stride);

    let here = load_flat(source, flat, REACH_FAR);
    let back = load_flat(source, flat - stride, REACH_FAR) + load_flat(second, flat - stride, 0.0);
    let ahead = load_flat(source, flat + stride, REACH_FAR) + load_flat(second, flat, 0.0);

    return vec4<f32>(min(here, min(back, ahead)), 0.0, 0.0, 1.0);
}

// The reach as the field a consumer binds: arc to the nearest hidden
// point or curve end, saturating rather than running away where the
// scan's window found nothing.
@fragment
fn fs_reach_out(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let reach = textureLoad(source, vec2<i32>(position.xy), 0).r;

    return vec4<f32>(min(reach, REACH_FAR), 0.0, 0.0, 1.0);
}

// The coverage scan's seed: a point counts toward its curve's coverage
// only where it survives in a run of at least two.
//
// That is the quantity `visibility::whole_or_nothing` divides. The rule
// sums the *runs* a split produced, and the split drops a run of one
// point — so a raw count of visible points would differ from it by
// exactly the isolated survivors, which are the crumbs the rule exists
// to reject. Counting them would blunt the rule it feeds.
//
// The empty texel between curves reads as hidden, so it can never join
// one curve's last point to the next curve's first.
@fragment
fn fs_cover_seed(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let flat = i32(flat_of(vec2<u32>(position.xy)));
    let here = load_flat(source, flat, 0.0);
    let joined = max(load_flat(source, flat - 1, 0.0), load_flat(source, flat + 1, 0.0));

    return vec4<f32>(select(0.0, 1.0, here > 0.5 && joined > 0.5), 0.0, 0.0, 1.0);
}

// One doubling of the coverage scan: a prefix sum of the verdict along
// each curve, segmented so it never reaches past the curve's first
// point.
//
// The gate is the texel's own distance back to that first point. Where
// `head >= 2^k` the window `2^k` back is still inside the curve and its
// partial sum joins this one; where it is not, this texel's sum already
// covers the whole curve so far and there is nothing left to add.
@fragment
fn fs_cover_step(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let flat = i32(flat_of(vec2<u32>(position.xy)));
    let stride = i32(params.stride);
    let head = textureLoad(second, vec2<i32>(position.xy), 0).r;
    let here = load_flat(source, flat, 0.0);
    let behind = select(0.0, load_flat(source, flat - stride, 0.0), head >= params.stride);

    return vec4<f32>(here + behind, 0.0, 0.0, 1.0);
}

// The whole-or-nothing input: what fraction of each curve survived,
// carried to every one of its texels.
//
// The scan leaves each curve's total at its last texel, which every
// texel of the curve can name — it is exactly `tail` texels ahead. An
// empty texel between curves has no curve, no length worth dividing by
// and no verdict, so it falls out as zero.
@fragment
fn fs_cover_gather(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let at = vec2<i32>(position.xy);
    let flat = i32(flat_of(vec2<u32>(position.xy)));
    let head = textureLoad(second, at, 0).r;
    let tail = textureLoad(third, at, 0).r;
    let total = load_flat(source, flat + i32(tail), 0.0);

    return vec4<f32>(total / (head + tail + 1.0), 0.0, 0.0, 1.0);
}
