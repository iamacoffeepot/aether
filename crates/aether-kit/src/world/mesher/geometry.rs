use aether_math::Rgb;
use aether_render::{DrawTriangle, Vertex};

use super::constants::OCTIMETERS_PER_METER;

/// Push the two triangles of one flat-colored underlay quad spanning `rect`
/// (`[x0, z0, x1, z1]` octimeters) on the given surface evaluator
/// (`(wx, wz)` meters to `y`). Every corner takes the same flat `color`.
// The quad's four corners (`a`..`d`) read clearest under these conventional
// short names.
#[allow(clippy::many_single_char_names)]
pub(super) fn emit_flat_quad(
    rect: [i32; 4],
    color: Rgb,
    surface: &impl Fn(f32, f32) -> f32,
    tris: &mut Vec<DrawTriangle>,
) {
    let corner = |xo: i32, zo: i32| {
        let wx = xo as f32 / OCTIMETERS_PER_METER;
        let wz = zo as f32 / OCTIMETERS_PER_METER;
        Vertex { x: wx, y: surface(wx, wz), z: wz, color }
    };
    let a = corner(rect[0], rect[1]);
    let b = corner(rect[2], rect[1]);
    let c = corner(rect[2], rect[3]);
    let d = corner(rect[0], rect[3]);
    tris.push(DrawTriangle { verts: [a, b, c] });
    tris.push(DrawTriangle { verts: [a, c, d] });
}

/// Push the two triangles of one vertical wall quad: a top edge from
/// `top_a` to `top_b` (`[wx, wz, y]` meters) dropped to `y_bottom_a` /
/// `y_bottom_b` at the same footprint, every vertex in the flat `color`.
pub(super) fn push_wall_quad(
    tris: &mut Vec<DrawTriangle>,
    top_a: [f32; 3],
    top_b: [f32; 3],
    y_bottom_a: f32,
    y_bottom_b: f32,
    color: Rgb,
) {
    let vert = |x: f32, z: f32, y: f32| Vertex { x, y, z, color };
    let a = vert(top_a[0], top_a[1], top_a[2]);
    let b = vert(top_b[0], top_b[1], top_b[2]);
    let c = vert(top_b[0], top_b[1], y_bottom_b);
    let d = vert(top_a[0], top_a[1], y_bottom_a);
    tris.push(DrawTriangle { verts: [a, b, c] });
    tris.push(DrawTriangle { verts: [a, c, d] });
}
