// The face paint as authored passes (iamacoffeepot/aether#4387,
// ADR-0171): `easel/accent.rs`, re-spoken as one draw pass over the
// chart's own aperture loops and five pointwise passes over what it
// leaves.
//
// The law the CPU module opens with survives the move intact: a wash
// asks the label plane what a pixel is, an accent asks the chart where a
// feature *is*. So everything the chart owns — where each eye sits, how
// its iris projects, how much of it the viewer can see — is solved on
// the CPU off the chart and arrives here as uniforms and as the aperture
// triangles themselves. What runs on the GPU is only the per-pixel half:
// the clip fill, the two iris ramps, and the flush.
//
// The aperture arrives already projected, as clip-space triangles. The
// CPU projects two dozen loop points through `regions::on_canvas` — the
// one mapping the maps and the paint both register through — and fans
// them about the iris centre, which is inside every lid loop by
// construction. Fanning on the CPU rather than in a vertex stage keeps
// the projection single-sourced: the alternative is a second
// transcription of the page mapping that has to be kept in step with the
// first.

// How far the aperture clip is softened is a blur radius the chain
// carries; where its edge is taken afterwards is here
// (`accent::CLIP_EDGE`), with the strength under which a pixel is
// outside the eye entirely (`accent::CLIP_FLOOR`).
const CLIP_EDGE_LOW: f32 = 0.3;
const CLIP_EDGE_HIGH: f32 = 0.6;
const CLIP_FLOOR: f32 = 0.02;

// Where the iris rim falls, in iris radii, and where the slit's does
// (`accent::IRIS_RIM` / `SLIT_RIM`). The slit's runs the other way: the
// wash builds toward the rim and cuts out around the pupil, so what is
// left inside the slit is paper.
const IRIS_RIM_OUT: f32 = 1.1;
const IRIS_RIM_IN: f32 = 0.95;
const SLIT_RIM_IN: f32 = 0.85;
const SLIT_RIM_OUT: f32 = 1.05;

// How much heavier the iris runs where the lid crosses it, and the band
// in iris radii over which that weight arrives (`accent::LIFT` /
// `LIFT_BAND`).
const LIFT_WEIGHT: f32 = 0.6;
const LIFT_BAND_LOW: f32 = 0.1;
const LIFT_BAND_HIGH: f32 = 0.6;

// How squarely a cheek must confront the viewer to hold its blush
// (`accent::FACING`).
const FACING_LOW: f32 = 0.42;
const FACING_HIGH: f32 = 0.62;

// How much of the flush survives (`accent::FLUSH_STRENGTH`). Folded in
// before the softening blur rather than after it: the blur is linear, so
// the two orders are the same field, and folding spares a pass whose
// only work was one multiply.
const FLUSH_STRENGTH: f32 = 0.55;

// The most eyes one face is charted with. Two, with room for a subject
// that carries more; a count past this simply stops being painted rather
// than reading past the array.
const MAX_EYES: u32 = 4u;

// One eye's frame, packed four-wide so the block's layout in the uniform
// address space is unambiguous. The named accessors below are what the
// passes read; nothing indexes the lanes directly.
struct Eye {
    // centre.xy, then the first row of the inverted projected frame.
    centre_across: vec4<f32>,
    // the second row, then the pupil half-axes in iris radii.
    down_pupil: vec4<f32>,
    // how far out the iris is measured, whether the frame inverted at
    // all, then where the cheek apple sits.
    reach_valid_apple: vec4<f32>,
    // the apple's radii, then how much blush this eye has earned.
    radii_presence: vec4<f32>,
}

fn eye_centre(eye: Eye) -> vec2<f32> { return eye.centre_across.xy; }
fn eye_across(eye: Eye) -> vec2<f32> { return eye.centre_across.zw; }
fn eye_down(eye: Eye) -> vec2<f32> { return eye.down_pupil.xy; }
fn eye_pupil(eye: Eye) -> vec2<f32> { return eye.down_pupil.zw; }
fn eye_reach(eye: Eye) -> f32 { return eye.reach_valid_apple.x; }
fn eye_valid(eye: Eye) -> f32 { return eye.reach_valid_apple.y; }
fn eye_apple(eye: Eye) -> vec2<f32> { return eye.reach_valid_apple.zw; }
fn eye_radii(eye: Eye) -> vec2<f32> { return eye.radii_presence.xy; }
fn eye_presence(eye: Eye) -> f32 { return eye.radii_presence.z; }

struct FaceParams {
    count: u32,
    unused: vec3<u32>,
    eyes: array<Eye, 4>,
}

@group(0) @binding(0) var<uniform> face: FaceParams;

// Whether this texel is one the CPU's own `accent::span` walk would have
// visited around `centre`, reach by reach. The walk floors the near
// bound and ceils the far one, both clamped up to the canvas, and both
// ends are inclusive — a pixel whose centre lies inside the reach has to
// be visited, and rounding the near bound inward drops the row a
// polygon's own top edge falls in.
fn within_span(at: vec2<f32>, centre: vec2<f32>, reach: vec2<f32>) -> bool {
    let first = max(floor(centre - reach), vec2<f32>(0.0));
    let last = max(ceil(centre + reach), vec2<f32>(0.0));
    return all(at >= first) && all(at <= last);
}

// The aperture loops, arriving already projected and fanned. Nothing to
// do but hand the corners to the rasterizer.
@vertex
fn vs_aperture(@location(0) clip: vec2<f32>) -> @builtin(position) vec4<f32> {
    return vec4<f32>(clip, 0.0, 1.0);
}

// The fill itself. `accent::fill` writes one wherever a scanline is
// inside the loop; a rasterized fan of the same loop claims the same
// interior, and the soft edge the chain puts on it afterwards is what
// puts the boundary back on the curve the chart meant rather than on
// either rule's treatment of the chords.
@fragment
fn fs_aperture() -> @location(0) vec4<f32> {
    return vec4<f32>(1.0, 0.0, 0.0, 1.0);
}

@group(1) @binding(0) var face_clip: texture_2d<f32>;

// The iris meta-material's coverage: clipped to the aperture, ramped to
// its own rim, and cut around the slit. `accent::irises`, verbatim, with
// the loop over eyes kept because two irises may overlap on the canvas
// and the CPU takes the stronger.
@fragment
fn fs_iris(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let texel = floor(position.xy);
    let held = hermite(CLIP_EDGE_LOW, CLIP_EDGE_HIGH, textureLoad(face_clip, vec2<i32>(position.xy), 0).r);
    if held < CLIP_FLOOR {
        return plane_out(0.0);
    }

    var coverage = 0.0;
    for (var index = 0u; index < min(face.count, MAX_EYES); index++) {
        let eye = face.eyes[index];
        if eye_valid(eye) == 0.0 || !within_span(texel, eye_centre(eye), vec2<f32>(eye_reach(eye))) {
            continue;
        }

        let offset = texel + 0.5 - eye_centre(eye);
        let at = vec2<f32>(dot(eye_across(eye), offset), dot(eye_down(eye), offset));
        let within = length(at);
        if within >= IRIS_RIM_OUT {
            continue;
        }

        let slit = length(at / eye_pupil(eye));
        coverage = max(
            coverage,
            held * hermite(IRIS_RIM_OUT, IRIS_RIM_IN, within) * hermite(SLIT_RIM_IN, SLIT_RIM_OUT, slit),
        );
    }

    return plane_out(coverage);
}

// The lid's weight over the iris — one everywhere it does not cross.
//
// The same walk as the coverage, under the same guards, because the CPU
// writes both inside one loop body: a texel the iris ramp never reaches
// is a texel the lift never touches either. Where two eyes overlap the
// later one wins, as it does there.
@fragment
fn fs_lift(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let texel = floor(position.xy);
    let held = hermite(CLIP_EDGE_LOW, CLIP_EDGE_HIGH, textureLoad(face_clip, vec2<i32>(position.xy), 0).r);
    if held < CLIP_FLOOR {
        return plane_out(1.0);
    }

    var lift = 1.0;
    for (var index = 0u; index < min(face.count, MAX_EYES); index++) {
        let eye = face.eyes[index];
        if eye_valid(eye) == 0.0 || !within_span(texel, eye_centre(eye), vec2<f32>(eye_reach(eye))) {
            continue;
        }

        let offset = texel + 0.5 - eye_centre(eye);
        let at = vec2<f32>(dot(eye_across(eye), offset), dot(eye_down(eye), offset));
        if length(at) >= IRIS_RIM_OUT {
            continue;
        }

        lift = 1.0 + LIFT_WEIGHT * hermite(LIFT_BAND_LOW, LIFT_BAND_HIGH, at.y);
    }

    return plane_out(lift);
}

// The cheek flush before the skin under it and the facing of it have had
// their say: a quadratic fall over each apple, weighted by how much of
// its own eye the viewer can see. `accent::blush`'s accumulation loop —
// the apple's placement and the presence gate are the chart's business
// and arrive solved.
@fragment
fn fs_blush_flush(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let texel = floor(position.xy);
    var flush = 0.0;

    for (var index = 0u; index < min(face.count, MAX_EYES); index++) {
        let eye = face.eyes[index];
        let presence = eye_presence(eye);
        if presence <= 0.0 || !within_span(texel, eye_apple(eye), eye_radii(eye)) {
            continue;
        }

        let offset = (texel - eye_apple(eye)) / eye_radii(eye);
        let fall = max(1.0 - dot(offset, offset), 0.0);
        flush += fall * fall * presence;
    }

    return plane_out(flush);
}

@group(1) @binding(0) var blush_flush: texture_2d<f32>;
@group(1) @binding(2) var blush_skin: texture_2d<f32>;
@group(1) @binding(4) var blush_packed: texture_2d<f32>;

// The flush gated twice over — by the softened skin beneath it, so it
// never lands off her face, and by how squarely that surface confronts
// the viewer, so a grazing cheek's sliver does not take the frontal
// one's flush and read as a stripe down her jaw.
@fragment
fn fs_blush_gate(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let at = vec2<i32>(position.xy);
    let flush = textureLoad(blush_flush, at, 0).r;
    let under = textureLoad(blush_skin, at, 0).r;
    let facing = textureLoad(blush_packed, at, 0).b;

    return plane_out(flush * under * hermite(FACING_LOW, FACING_HIGH, facing) * FLUSH_STRENGTH);
}
