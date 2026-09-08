// Shape overlay (ADR-0213). Each shape is an axis-aligned
// box expanded to one quad grown by its shadow extent; the fragment
// stage evaluates a rounded-box signed distance per pixel and composes
// shadow under fill under stroke, each edge anti-aliased over one
// `fwidth`. The vertex stage runs the quad overlay's own projection —
// `overlay_clip_position`, prepended from `overlay_projection.wgsl` at
// pipeline build — so Screen and World shapes land exactly where Screen and
// World quads do. The output is premultiplied, and the pipeline composites it
// with premultiplied blending.

struct VertexInput {
    // World-space anchor (World path) or (0,0,0) unused (Screen path).
    @location(0) anchor: vec3<f32>,
    // Screen: absolute pixel position. World: pixel offset from the
    // projected anchor in screen y-down convention.
    @location(1) offset_px: vec2<f32>,
    // This corner's position relative to the box centre, in the box's
    // own pixel units — the space the distance field is evaluated in.
    @location(2) local: vec2<f32>,
    // Half the box's width and height.
    @location(3) half_size: vec2<f32>,
    // corner radius, stroke width, shadow blur, world scale factor k.
    @location(4) params: vec4<f32>,
    @location(5) fill: vec4<f32>,
    @location(6) stroke: vec4<f32>,
    @location(7) shadow: vec4<f32>,
    // The shadow's offset from the box, in the same pixel units.
    @location(8) shadow_offset: vec2<f32>,
    // (u0, v0, u1, v1) stretched across the box; read only by the
    // textured fragment stage.
    @location(9) uv_rect: vec4<f32>,
    // Non-zero => Screen path; zero => World path.
    @location(10) is_screen: u32,
    // Non-zero => the sampled texel's colour is already scaled by its own
    // coverage; zero => straight alpha.
    @location(11) texture_premultiplied: u32,
}

struct VertexOutput {
    @builtin(position) clip_pos: vec4<f32>,
    @location(0) local: vec2<f32>,
    @location(1) @interpolate(flat) half_size: vec2<f32>,
    @location(2) @interpolate(flat) params: vec4<f32>,
    @location(3) @interpolate(flat) fill: vec4<f32>,
    @location(4) @interpolate(flat) stroke: vec4<f32>,
    @location(5) @interpolate(flat) shadow: vec4<f32>,
    @location(6) @interpolate(flat) shadow_offset: vec2<f32>,
    @location(7) @interpolate(flat) uv_rect: vec4<f32>,
    @location(8) @interpolate(flat) texture_premultiplied: u32,
}

// The batch's texture, bound by the textured pipeline only.
@group(1) @binding(0)
var overlay_texture: texture_2d<f32>;
@group(1) @binding(1)
var overlay_sampler: sampler;

@vertex
fn vs_main(in: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    out.local = in.local;
    out.half_size = in.half_size;
    out.params = in.params;
    out.fill = in.fill;
    out.stroke = in.stroke;
    out.shadow = in.shadow;
    out.shadow_offset = in.shadow_offset;
    out.uv_rect = in.uv_rect;
    out.texture_premultiplied = in.texture_premultiplied;
    // The world scale factor rides `params.w` here rather than its own
    // attribute, so it is unpacked at the call instead of in the block.
    out.clip_pos = overlay_clip_position(in.anchor, in.offset_px, in.params.w, in.is_screen);
    return out;
}

// Signed distance from `p` to a box of `half` half-extents with corners
// rounded by `radius`: negative inside, zero on the edge.
fn rounded_box_distance(p: vec2<f32>, half: vec2<f32>, radius: f32) -> f32 {
    let q = abs(p) - (half - vec2<f32>(radius, radius));
    return length(max(q, vec2<f32>(0.0, 0.0))) + min(max(q.x, q.y), 0.0) - radius;
}

// Coverage of the region `distance <= 0`, anti-aliased over one `width`.
fn inside(distance: f32, width: f32) -> f32 {
    return 1.0 - smoothstep(-0.5 * width, 0.5 * width, distance);
}

// `color` at `coverage`, premultiplied.
fn premultiplied(color: vec4<f32>, coverage: f32) -> vec4<f32> {
    let alpha = color.a * coverage;
    return vec4<f32>(color.rgb * alpha, alpha);
}

// `source` composited over `destination`, both premultiplied.
fn over(destination: vec4<f32>, source: vec4<f32>) -> vec4<f32> {
    return source + destination * (1.0 - source.a);
}

// The shape's three coverages. Both fragment entry points read the same
// distance field; only what they lay inside the fill's coverage differs.
struct Coverages {
    fill: f32,
    stroke: f32,
    shadow: f32,
}

fn coverages(in: VertexOutput) -> Coverages {
    // A radius at or above half the shorter side is a circle (or a
    // stadium); clamping keeps the field well-formed past that.
    let radius = min(in.params.x, min(in.half_size.x, in.half_size.y));
    let stroke_width = in.params.y;
    let blur = in.params.z;

    let distance = rounded_box_distance(in.local, in.half_size, radius);
    let width = max(fwidth(distance), 0.001);
    let fill_coverage = inside(distance, width);

    // The stroke is the band inside the outer edge and outside the edge
    // `stroke_width` further in; a zero-width stroke is no band at all.
    var stroke_coverage = 0.0;
    if stroke_width > 0.0 {
        stroke_coverage = fill_coverage * (1.0 - inside(distance + stroke_width, width));
    }

    // The shadow is the same box moved by its offset, its edge feathered
    // over `blur` pixels each side (never sharper than the anti-aliasing
    // width, so a zero-blur shadow is a hard, anti-aliased drop).
    let shadow_distance = rounded_box_distance(in.local - in.shadow_offset, in.half_size, radius);
    let feather = max(blur, width);
    let shadow_coverage = 1.0 - smoothstep(-feather, feather, shadow_distance);

    return Coverages(fill_coverage, stroke_coverage, shadow_coverage);
}

// Shadow under `body` under stroke, each premultiplied. `body` arrives
// premultiplied and already weighted by the fill's coverage.
fn compose(in: VertexOutput, parts: Coverages, body: vec4<f32>) -> vec4<f32> {
    var color = premultiplied(in.shadow, parts.shadow);
    color = over(color, body);
    color = over(color, premultiplied(in.stroke, parts.stroke));
    return color;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let parts = coverages(in);
    return compose(in, parts, premultiplied(in.fill, parts.fill));
}

// The textured variant: the same field, with the batch's texture sampled
// across the box and multiplied by `fill` where the fill covers. The
// corner radius, the circle, and the anti-aliased edge therefore apply to
// the image exactly as they apply to a flat colour.
@fragment
fn fs_textured(in: VertexOutput) -> @location(0) vec4<f32> {
    let parts = coverages(in);

    // `local` runs -half_size..half_size across the box, so this is the
    // box's own 0..1 coordinate mapped onto the caller's uv sub-rect.
    let half = max(in.half_size, vec2<f32>(0.001, 0.001));
    let unit = clamp(in.local / half * 0.5 + 0.5, vec2<f32>(0.0), vec2<f32>(1.0));
    let texel = textureSample(overlay_texture, overlay_sampler, mix(in.uv_rect.xy, in.uv_rect.zw, unit));

    // Both paths end premultiplied and weighted by the fill's coverage. A
    // straight texel's colour is scaled by the alpha it is handed; a
    // premultiplied one already carries its own, so only the tint's alpha
    // and the coverage remain to apply.
    let alpha = texel.a * in.fill.a * parts.fill;
    var body: vec4<f32>;
    if in.texture_premultiplied != 0u {
        body = vec4<f32>(texel.rgb * in.fill.rgb * in.fill.a * parts.fill, alpha);
    } else {
        body = vec4<f32>(texel.rgb * in.fill.rgb * alpha, alpha);
    }
    return compose(in, parts, body);
}
