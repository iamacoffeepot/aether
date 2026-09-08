// The overlay pass's shared vertex-stage projection (ADR-0105 / ADR-0213).
// Prepended to every overlay shader at pipeline build, so the `Viewport`
// uniform and the Screen/World projection are written once rather than once
// per overlay stage.
//
// Screen vertices supply an absolute pixel position in `offset_px`; World
// vertices transform `anchor` through `view_proj` and apply `offset_px` as a
// clip-space pixel offset, so an overlay batch stays camera-facing and never
// skews.

struct Viewport {
    // Column-major view-projection matrix used by the World path.
    view_proj: mat4x4<f32>,
    // Width and height of the render target in pixels.
    size: vec2<f32>,
    _pad: vec2<f32>,
}

@group(0) @binding(0)
var<uniform> viewport: Viewport;

// Project one overlay vertex into clip space.
//
// A non-zero `is_screen` takes the Screen path: pixel (0,0) top-left => clip
// (-1, 1), pixel (w,h) bottom-right => clip (1, -1) — y flips because pixels
// are top-down while clip space is bottom-up.
//
// Zero takes the World path: `anchor` through `view_proj`, then `offset_px`
// applied in clip space. `k` is the world scale factor — negative selects
// Pixels mode (k = clip.w, cancelling the perspective divide for constant
// on-screen size), positive is the Distance-mode reference distance (the
// batch shrinks as the anchor recedes). An anchor behind the camera
// (clip.w <= 0) is silently discarded by returning a position outside the
// clip cube.
fn overlay_clip_position(anchor: vec3<f32>, offset_px: vec2<f32>, k: f32, is_screen: u32) -> vec4<f32> {
    if is_screen != 0u {
        let ndc_x = offset_px.x / viewport.size.x * 2.0 - 1.0;
        let ndc_y = 1.0 - offset_px.y / viewport.size.y * 2.0;
        return vec4<f32>(ndc_x, ndc_y, 0.0, 1.0);
    }
    var clip = viewport.view_proj * vec4<f32>(anchor, 1.0);
    if clip.w <= 0.0 {
        return vec4<f32>(2.0, 2.0, 2.0, 1.0);
    }
    var scale = k;
    if scale < 0.0 {
        scale = clip.w;
    }
    // offset_px uses the screen y-down convention; negate y so a positive
    // offset_px.y moves downward on screen (i.e. a negative offset_px.y, as
    // produced for above-anchor glyphs, increases clip.y and moves the label
    // upward).
    clip.x += offset_px.x / viewport.size.x * 2.0 * scale;
    clip.y -= offset_px.y / viewport.size.y * 2.0 * scale;
    return clip;
}
