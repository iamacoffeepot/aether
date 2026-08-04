// The ink coverage plane, baked on the GPU from resident ribbon geometry.
//
// The CPU oracle this reproduces is `easel::regions::ink`. It is not an
// antialiased rasterize: a pixel is either claimed or it is not, and a
// pixel is claimed when its centre lies inside the triangle widened by
// half a pixel along every edge. That slack is the whole point (#4356) —
// a ribbon is about two window pixels wide and the canvas is half of
// that or less, so under a bare pixel-centre test whole strokes fall
// between the samples and the flow field reads a dashed drawing.
//
// Hardware rasterization tests pixel centres against the bare triangle,
// so the slack has to be added back. The vertex stage widens each
// triangle enough that the hardware covers a superset of the claimed
// pixels, and the fragment stage runs the oracle's own edge test and
// discards the rest. What survives is the oracle's answer, not an
// approximation of it: same sample points, same edge functions, same
// slack, same cull rules.

struct InkUniforms {
    view_proj: mat4x4<f32>,
    // Half the canvas size in pixels — the projection's page mapping.
    half_size: vec2<f32>,
    padding: vec2<f32>,
}

@group(0) @binding(0) var<uniform> ink: InkUniforms;

// How far past its own edges a triangle still claims a pixel, in pixels.
// `regions::COVERAGE_SLACK`; the two must move together.
const COVERAGE_SLACK: f32 = 0.5;

// Below this the oracle calls a triangle degenerate and skips it.
// `regions::AREA_FLOOR`.
const AREA_FLOOR: f32 = 1e-6;

// How far past its bounding box the oracle walks, in pixels — its
// `reach`, `COVERAGE_SLACK + 1.0`.
//
// This bound is part of the rule, not an optimization of it. Where a
// sliver's two long edges converge, the offset edges meet far past the
// tip, so the slack test alone claims a long spike out there; the oracle
// never asks, because the spike is outside the box it walks. Reproducing
// the coverage means reproducing the box.
const REACH_PIXELS: f32 = COVERAGE_SLACK + 1.0;

// How far the vertex stage widens a triangle, in pixels.
//
// The claimed set is the intersection of three half-planes, each the
// triangle's own edge pushed out by the slack — so the shape the
// hardware must offer is exactly the triangle those three offset lines
// cut, and the widening is their miter. Only a lower bound matters: the
// fragment test decides the real boundary, so widening past it costs
// discarded fragments and nothing else.
//
// The miter runs away as a triangle thins, and so does the claimed set —
// a sliver's offset edges meet far past its own tip, and the oracle
// really does claim that spike. The cap is a numerical guard for the
// near-degenerate case the area floor lets through, set far enough out
// that no ribbon at canvas resolution reaches it.
const WIDEN_PIXELS: f32 = 1.0;
const MITER_CAP: f32 = 64.0;

struct InkVertex {
    @builtin(position) clip: vec4<f32>,
    // The triangle's three page-space corners, flat so every fragment
    // reads the provoking vertex's copy and the edge test is evaluated
    // from one consistent winding.
    @location(0) @interpolate(flat) a: vec2<f32>,
    @location(1) @interpolate(flat) b: vec2<f32>,
    @location(2) @interpolate(flat) c: vec2<f32>,
    // The triangle's page-space bounding box — `(min, max)` — which the
    // fragment stage clips against exactly as the oracle's walk does.
    @location(3) @interpolate(flat) bounds: vec4<f32>,
}

// Where a world point lands on the page, matching `regions::project`:
// x runs right across [-1, 1], y runs up across the same range against a
// canvas whose rows run downward, so the vertical axis flips.
fn page(clip: vec4<f32>, half_size: vec2<f32>) -> vec2<f32> {
    let ndc = clip.xy / clip.w;
    return vec2<f32>((ndc.x + 1.0) * half_size.x, (1.0 - ndc.y) * half_size.y);
}

// A page-space offset carried back into clip space at this vertex's own
// depth, so widening survives the perspective divide.
fn unpage(offset: vec2<f32>, clip: vec4<f32>, half_size: vec2<f32>) -> vec2<f32> {
    return vec2<f32>(offset.x / half_size.x, -offset.y / half_size.y) * clip.w;
}

// Outward unit normal of the edge running `p` to `q`, given the
// triangle's winding.
fn outward(p: vec2<f32>, q: vec2<f32>, winding: f32) -> vec2<f32> {
    let along = q - p;
    let span = max(length(along), 1e-12);
    return vec2<f32>(along.y, -along.x) / span * winding;
}

// A vertex pushed out along the bisector of its two edges far enough
// that both are cleared by `WIDEN_PIXELS`.
fn widen(previous: vec2<f32>, at: vec2<f32>, next: vec2<f32>, winding: f32) -> vec2<f32> {
    let incoming = outward(previous, at, winding);
    let outgoing = outward(at, next, winding);
    let bisector = incoming + outgoing;
    let span = length(bisector);
    if span < 1e-6 {
        // The two edges double back on each other; stepping along either
        // normal clears both.
        return at + incoming * WIDEN_PIXELS;
    }
    let direction = bisector / span;
    let reach = min(WIDEN_PIXELS / max(dot(direction, incoming), 1e-3), MITER_CAP);
    return at + direction * reach;
}

// A triangle the oracle skips, moved outside the clip volume so the
// rasterizer drops it. Every one of its vertices reaches the same
// verdict from the same three corners, so the whole triangle goes.
fn culled() -> InkVertex {
    var out: InkVertex;
    out.clip = vec4<f32>(0.0, 0.0, -1.0, 1.0);
    out.a = vec2<f32>(0.0);
    out.b = vec2<f32>(0.0);
    out.c = vec2<f32>(0.0);
    out.bounds = vec4<f32>(0.0);
    return out;
}

// The three corners arrive on every vertex in cyclic order starting at
// the vertex's own, so the stage can project the whole triangle, decide
// the shared cull, and widen this corner against its two neighbours.
@vertex
fn vs_ink(
    @location(0) own: vec3<f32>,
    @location(1) next: vec3<f32>,
    @location(2) last: vec3<f32>,
) -> InkVertex {
    let clip_own = ink.view_proj * vec4<f32>(own, 1.0);
    let clip_next = ink.view_proj * vec4<f32>(next, 1.0);
    let clip_last = ink.view_proj * vec4<f32>(last, 1.0);

    // The oracle drops a triangle with any corner at or behind the near
    // plane rather than letting the homogeneous divide fold it back onto
    // the page mirrored.
    if clip_own.w <= 0.0 || clip_next.w <= 0.0 || clip_last.w <= 0.0 {
        return culled();
    }

    let page_own = page(clip_own, ink.half_size);
    let page_next = page(clip_next, ink.half_size);
    let page_last = page(clip_last, ink.half_size);

    let area = (page_next.x - page_own.x) * (page_last.y - page_own.y)
        - (page_next.y - page_own.y) * (page_last.x - page_own.x);
    if abs(area) < AREA_FLOOR {
        return culled();
    }

    let widened = widen(page_last, page_own, page_next, sign(area));

    var out: InkVertex;
    out.clip = vec4<f32>(
        clip_own.xy + unpage(widened - page_own, clip_own, ink.half_size),
        clip_own.zw,
    );
    out.a = page_own;
    out.b = page_next;
    out.c = page_last;
    out.bounds = vec4<f32>(
        min(page_own, min(page_next, page_last)),
        max(page_own, max(page_next, page_last)),
    );
    return out;
}

// The oracle's test, verbatim: an edge function divided by its own
// edge's length is the signed perpendicular distance from the pixel to
// that edge, so the slack is compared in the function's units by
// multiplying it back through. Winding carries the sign — a back-facing
// ribbon negates every edge function at once.
@fragment
fn fs_ink(vertex: InkVertex) -> @location(0) vec4<f32> {
    // `@builtin(position)` is the framebuffer coordinate at the pixel
    // centre, which is the oracle's own `(x + 0.5, y + 0.5)` sample.
    let at = vertex.clip.xy;

    // The oracle's walk, transcribed: its bounds truncate to the pixel
    // below, and it never leaves the page, which a fragment cannot do
    // anyway.
    let pixel = floor(at);
    let low = floor(max(vertex.bounds.xy - REACH_PIXELS, vec2<f32>(0.0)));
    let high = floor(vertex.bounds.zw + REACH_PIXELS);
    if pixel.x < low.x || pixel.x > high.x || pixel.y < low.y || pixel.y > high.y {
        discard;
    }

    let area = (vertex.b.x - vertex.a.x) * (vertex.c.y - vertex.a.y)
        - (vertex.b.y - vertex.a.y) * (vertex.c.x - vertex.a.x);
    let winding = sign(area);

    // The edge opposite each vertex, the oracle's own `[(b, c), (c, a),
    // (a, b)]`.
    var heads = array<vec2<f32>, 3>(vertex.b, vertex.c, vertex.a);
    var tails = array<vec2<f32>, 3>(vertex.c, vertex.a, vertex.b);
    for (var edge = 0; edge < 3; edge++) {
        let p = heads[edge];
        let q = tails[edge];
        let value = (p.x - at.x) * (q.y - at.y) - (p.y - at.y) * (q.x - at.x);
        let slack = length(q - p) * COVERAGE_SLACK;
        if value * winding < -slack {
            discard;
        }
    }

    return vec4<f32>(1.0, 0.0, 0.0, 1.0);
}
