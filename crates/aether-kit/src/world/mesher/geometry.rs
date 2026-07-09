use aether_capabilities::render::{DrawTriangle, Vertex};
use aether_math::Rgb;

use super::constants::OCTIMETERS_PER_METER;

/// Push the two triangles of one flat-colored underlay quad spanning `rect`
/// (`[x0, z0, x1, z1]` octimeters) on the given surface evaluator
/// (`(wx, wz)` meters to `y`). Every corner takes the same flat `color`.
// The quad's four corners (`a`..`d`) read clearest under these conventional
// short names.
#[allow(clippy::many_single_char_names)]
pub(super) fn emit_flat_quad(
    rect: [i32; 4],
    color: [f32; 3],
    surface: &impl Fn(f32, f32) -> f32,
    tris: &mut Vec<DrawTriangle>,
) {
    let corner = |xo: i32, zo: i32| {
        let wx = xo as f32 / OCTIMETERS_PER_METER;
        let wz = zo as f32 / OCTIMETERS_PER_METER;
        Vertex {
            x: wx,
            y: surface(wx, wz),
            z: wz,
            color: Rgb::new(color[0], color[1], color[2]),
        }
    };
    let a = corner(rect[0], rect[1]);
    let b = corner(rect[2], rect[1]);
    let c = corner(rect[2], rect[3]);
    let d = corner(rect[0], rect[3]);
    tris.push(DrawTriangle { verts: [a, b, c] });
    tris.push(DrawTriangle { verts: [a, c, d] });
}
