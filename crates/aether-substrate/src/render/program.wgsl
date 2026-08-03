// Substrate-owned vertex stage for authored render programs (ADR-0170).
// One triangle covering the whole target; authored modules declare
// fragment entry points only. The interpolated uv follows the texture
// convention — (0, 0) at the target's top-left, (1, 1) at its
// bottom-right — so a fragment sampling a Full-extent input at uv reads
// the texel under the fragment it writes.

struct FullscreenOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

@vertex
fn vs_fullscreen(@builtin(vertex_index) index: u32) -> FullscreenOut {
    // Indices 0, 1, 2 expand to clip corners (-1, -1), (3, -1), (-1, 3):
    // one triangle whose interior covers the whole [-1, 1] square.
    let corner = vec2<f32>(f32((index << 1u) & 2u), f32(index & 2u));
    let position = corner * 2.0 - 1.0;
    var out: FullscreenOut;
    out.position = vec4<f32>(position, 0.0, 1.0);
    // Clip y points up; texture v points down. Flip so the top row of
    // the target reads v = 0.
    out.uv = vec2<f32>(position.x, -position.y) * 0.5 + 0.5;
    return out;
}
