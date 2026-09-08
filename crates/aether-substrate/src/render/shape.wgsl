// Screen-space shape overlay (ADR-0213). Each shape is an axis-aligned
// box expanded to one quad grown by its shadow extent; the fragment
// stage evaluates a rounded-box signed distance per pixel and composes
// shadow under fill under stroke, each edge anti-aliased over one
// `fwidth`. The vertex stage shares the quad overlay's two paths: Screen
// vertices supply an absolute pixel position in `offset_px`; World
// vertices transform `anchor` through `view_proj` and apply `offset_px`
// as a clip-space pixel offset. The output is premultiplied, and the
// pipeline composites it with premultiplied blending.

struct Viewport {
    // Column-major view-projection matrix used by the World path.
    view_proj: mat4x4<f32>,
    // Width and height of the render target in pixels.
    size: vec2<f32>,
    _pad: vec2<f32>,
}

@group(0) @binding(0)
var<uniform> viewport: Viewport;

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
    // Non-zero => Screen path; zero => World path.
    @location(9) is_screen: u32,
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
}

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

    if in.is_screen != 0u {
        // Pixel (0,0) top-left => clip (-1, 1); pixel (w,h) bottom-right
        // => clip (1, -1).
        let ndc_x = in.offset_px.x / viewport.size.x * 2.0 - 1.0;
        let ndc_y = 1.0 - in.offset_px.y / viewport.size.y * 2.0;
        out.clip_pos = vec4<f32>(ndc_x, ndc_y, 0.0, 1.0);
    } else {
        var clip = viewport.view_proj * vec4<f32>(in.anchor, 1.0);
        if clip.w <= 0.0 {
            out.clip_pos = vec4<f32>(2.0, 2.0, 2.0, 1.0);
            return out;
        }
        // Negative k => Pixels mode (constant on-screen size); positive
        // k => Distance mode (shrinks with depth). Same rule as the quad
        // overlay.
        var k = in.params.w;
        if k < 0.0 {
            k = clip.w;
        }
        clip.x += in.offset_px.x / viewport.size.x * 2.0 * k;
        clip.y -= in.offset_px.y / viewport.size.y * 2.0 * k;
        out.clip_pos = clip;
    }
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

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
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

    var color = premultiplied(in.shadow, shadow_coverage);
    color = over(color, premultiplied(in.fill, fill_coverage));
    color = over(color, premultiplied(in.stroke, stroke_coverage));
    return color;
}
