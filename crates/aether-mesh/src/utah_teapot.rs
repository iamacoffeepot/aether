//! The Utah teapot, tessellated from its Bézier patches.
//!
//! Martin Newell modelled the teapot in 1975 as 32 bicubic Bézier patches
//! and placed the control points in the public domain; they have been
//! circulated ever since as the `teapot` / `teapot.bpt` dataset, and
//! `CONTROL_POINTS` below is a transcription of one such copy
//! (<https://raw.githubusercontent.com/mkeeter/teapot-mrep/master/teapot.bpt>,
//! 32 patches of 16 control points each in Mackerras' `bpt` format). The
//! distribution repeats the points a patch shares with its neighbour, so
//! the table below stores each distinct position once and `PATCHES`
//! indexes it — which is also what makes the tessellated seams weld: two
//! patches that meet evaluate their shared boundary curve from the same
//! four table entries.
//!
//! The dataset's own frame is Z-up with the spout at +X. Aether's is Y-up,
//! and the `teapot.dsl` subject this one replaces points its spout at -X
//! and its handle at +X with the handle's plane at Z = 0. So the transform
//! is the up-axis convention change and a half turn about the vertical
//! together: `(x, y, z)` becomes `(-x, z, y)`, whose determinant is +1, so
//! the patches' winding survives it.
//!
//! Scale follows the same subject. A turntable sweeps its subject about the
//! vertical axis, so what has to stay inside `demo/turntable.json`'s framing
//! is the radius that sweep traces, and the scale here is the one that puts
//! this teapot's spout tip exactly where `teapot.dsl`'s handle reached. The
//! teapot then stands 1.02 units tall on the ground plane rather than 1.30,
//! which is a squatter and more faithful pot inside the same circle.

use aether_math::Vec3;

use crate::mesh::Triangle;

/// Patch-edge subdivisions the checked-in `examples/utah_teapot.obj` was
/// generated at. Ten is the resolution at which the exported file stays
/// small enough to read as text in the tree.
pub const DEFAULT_SEGMENTS: u16 = 10;

/// How many patches the dataset is.
pub const PATCH_COUNT: usize = PATCHES.len();

/// Where the spout tip lands in the dataset's own units — the furthest any
/// of the 32 patches reaches from the vertical axis, and so the radius of
/// the circle the teapot sweeps when a turntable turns it.
const SPOUT_REACH: f32 = 3.434_073;

/// The same radius for the `teapot.dsl` subject this one replaces, measured
/// off its meshed output: the outermost point of its handle.
const SWEPT_RADIUS: f32 = 1.116;

const SCALE: f32 = SWEPT_RADIUS / SPOUT_REACH;

/// Placeholder palette index, as `teapot.dsl` uses (ADR-0026 §palette).
const COLOR: u32 = 0;

/// How close two evaluated surface points have to be to count as one point.
/// Matches [`crate::obj::to_obj`]'s vertex weld, so a corner pair this
/// treats as collapsed is exactly a corner pair the exporter would have put
/// on one index. Distinct grid points on this dataset sit orders of
/// magnitude further apart than this, the knob crown included.
const COINCIDENT: f32 = 1e-6;

/// The teapot as a triangle list, `segments` subdivisions along each patch
/// edge, wound counter-clockwise from outside like every other surface this
/// crate produces.
///
/// Quads whose corners collapse onto the vertical axis — the first control
/// row of the knob crown's four patches and of the bottom's four is a single
/// repeated point — contribute one triangle rather than two.
#[must_use]
pub fn tessellate(segments: u16) -> Vec<Triangle> {
    let segments = segments.max(1);
    let span = usize::from(segments) + 1;
    let step = 1.0 / f32::from(segments);

    let mut grid = Vec::with_capacity(span * span);
    let mut triangles = Vec::with_capacity(PATCH_COUNT * usize::from(segments) * usize::from(segments) * 2);

    for patch in &PATCHES {
        grid.clear();
        for along in 0..=segments {
            for across in 0..=segments {
                grid.push(surface_point(patch, f32::from(along) * step, f32::from(across) * step));
            }
        }

        for along in 0..usize::from(segments) {
            for across in 0..usize::from(segments) {
                // Corners anticlockwise from the low one: stepping across
                // before along reverses the parameter cross product, which
                // on this dataset points into the solid.
                let low = grid[along * span + across];
                let across_step = grid[along * span + across + 1];
                let diagonal = grid[(along + 1) * span + across + 1];
                let along_step = grid[(along + 1) * span + across];

                push_unless_collapsed(&mut triangles, [low, across_step, diagonal]);
                push_unless_collapsed(&mut triangles, [low, diagonal, along_step]);
            }
        }
    }

    triangles
}

/// One point of a patch's surface, already in Aether's frame and scale.
fn surface_point(patch: &[u16; 16], along: f32, across: f32) -> Vec3 {
    let along_weights = bernstein(along);
    let across_weights = bernstein(across);

    let mut point = Vec3::ZERO;
    for (row, along_weight) in along_weights.into_iter().enumerate() {
        for (column, across_weight) in across_weights.into_iter().enumerate() {
            let control = CONTROL_POINTS[usize::from(patch[row * 4 + column])];
            point += Vec3::from_array(control) * (along_weight * across_weight);
        }
    }

    Vec3::new(-point.x * SCALE, point.z * SCALE, point.y * SCALE)
}

/// The four cubic Bernstein polynomials at `t`.
fn bernstein(t: f32) -> [f32; 4] {
    let s = 1.0 - t;
    [s * s * s, 3.0 * s * s * t, 3.0 * s * t * t, t * t * t]
}

fn push_unless_collapsed(triangles: &mut Vec<Triangle>, vertices: [Vec3; 3]) {
    let [first, second, third] = vertices;
    if coincident(first, second) || coincident(second, third) || coincident(third, first) {
        return;
    }

    triangles.push(Triangle { vertices, color: COLOR });
}

fn coincident(point: Vec3, other: Vec3) -> bool {
    (point.x - other.x).abs() < COINCIDENT
        && (point.y - other.y).abs() < COINCIDENT
        && (point.z - other.z).abs() < COINCIDENT
}

const CONTROL_POINTS: [[f32; 3]; 290] = [
    [1.4, 0.0, 2.4],
    [1.4, -0.784, 2.4],
    [0.784, -1.4, 2.4],
    [0.0, -1.4, 2.4],
    [1.3375, 0.0, 2.53125],
    [1.3375, -0.749, 2.53125],
    [0.749, -1.3375, 2.53125],
    [0.0, -1.3375, 2.53125],
    [1.4375, 0.0, 2.53125],
    [1.4375, -0.805, 2.53125],
    [0.805, -1.4375, 2.53125],
    [0.0, -1.4375, 2.53125],
    [1.5, 0.0, 2.4],
    [1.5, -0.84, 2.4],
    [0.84, -1.5, 2.4],
    [0.0, -1.5, 2.4],
    [-0.784, -1.4, 2.4],
    [-1.4, -0.784, 2.4],
    [-1.4, 0.0, 2.4],
    [-0.749, -1.3375, 2.53125],
    [-1.3375, -0.749, 2.53125],
    [-1.3375, 0.0, 2.53125],
    [-0.805, -1.4375, 2.53125],
    [-1.4375, -0.805, 2.53125],
    [-1.4375, 0.0, 2.53125],
    [-0.84, -1.5, 2.4],
    [-1.5, -0.84, 2.4],
    [-1.5, 0.0, 2.4],
    [-1.4, 0.784, 2.4],
    [-0.784, 1.4, 2.4],
    [0.0, 1.4, 2.4],
    [-1.3375, 0.749, 2.53125],
    [-0.749, 1.3375, 2.53125],
    [0.0, 1.3375, 2.53125],
    [-1.4375, 0.805, 2.53125],
    [-0.805, 1.4375, 2.53125],
    [0.0, 1.4375, 2.53125],
    [-1.5, 0.84, 2.4],
    [-0.84, 1.5, 2.4],
    [0.0, 1.5, 2.4],
    [0.784, 1.4, 2.4],
    [1.4, 0.784, 2.4],
    [0.749, 1.3375, 2.53125],
    [1.3375, 0.749, 2.53125],
    [0.805, 1.4375, 2.53125],
    [1.4375, 0.805, 2.53125],
    [0.84, 1.5, 2.4],
    [1.5, 0.84, 2.4],
    [1.75, 0.0, 1.875],
    [1.75, -0.98, 1.875],
    [0.98, -1.75, 1.875],
    [0.0, -1.75, 1.875],
    [2.0, 0.0, 1.35],
    [2.0, -1.12, 1.35],
    [1.12, -2.0, 1.35],
    [0.0, -2.0, 1.35],
    [2.0, 0.0, 0.9],
    [2.0, -1.12, 0.9],
    [1.12, -2.0, 0.9],
    [0.0, -2.0, 0.9],
    [-0.98, -1.75, 1.875],
    [-1.75, -0.98, 1.875],
    [-1.75, 0.0, 1.875],
    [-1.12, -2.0, 1.35],
    [-2.0, -1.12, 1.35],
    [-2.0, 0.0, 1.35],
    [-1.12, -2.0, 0.9],
    [-2.0, -1.12, 0.9],
    [-2.0, 0.0, 0.9],
    [-1.75, 0.98, 1.875],
    [-0.98, 1.75, 1.875],
    [0.0, 1.75, 1.875],
    [-2.0, 1.12, 1.35],
    [-1.12, 2.0, 1.35],
    [0.0, 2.0, 1.35],
    [-2.0, 1.12, 0.9],
    [-1.12, 2.0, 0.9],
    [0.0, 2.0, 0.9],
    [0.98, 1.75, 1.875],
    [1.75, 0.98, 1.875],
    [1.12, 2.0, 1.35],
    [2.0, 1.12, 1.35],
    [1.12, 2.0, 0.9],
    [2.0, 1.12, 0.9],
    [2.0, 0.0, 0.45],
    [2.0, -1.12, 0.45],
    [1.12, -2.0, 0.45],
    [0.0, -2.0, 0.45],
    [1.5, 0.0, 0.225],
    [1.5, -0.84, 0.225],
    [0.84, -1.5, 0.225],
    [0.0, -1.5, 0.225],
    [1.5, 0.0, 0.15],
    [1.5, -0.84, 0.15],
    [0.84, -1.5, 0.15],
    [0.0, -1.5, 0.15],
    [-1.12, -2.0, 0.45],
    [-2.0, -1.12, 0.45],
    [-2.0, 0.0, 0.45],
    [-0.84, -1.5, 0.225],
    [-1.5, -0.84, 0.225],
    [-1.5, 0.0, 0.225],
    [-0.84, -1.5, 0.15],
    [-1.5, -0.84, 0.15],
    [-1.5, 0.0, 0.15],
    [-2.0, 1.12, 0.45],
    [-1.12, 2.0, 0.45],
    [0.0, 2.0, 0.45],
    [-1.5, 0.84, 0.225],
    [-0.84, 1.5, 0.225],
    [0.0, 1.5, 0.225],
    [-1.5, 0.84, 0.15],
    [-0.84, 1.5, 0.15],
    [0.0, 1.5, 0.15],
    [1.12, 2.0, 0.45],
    [2.0, 1.12, 0.45],
    [0.84, 1.5, 0.225],
    [1.5, 0.84, 0.225],
    [0.84, 1.5, 0.15],
    [1.5, 0.84, 0.15],
    [-1.6, 0.0, 2.025],
    [-1.6, -0.3, 2.025],
    [-1.5, -0.3, 2.25],
    [-1.5, 0.0, 2.25],
    [-2.3, 0.0, 2.025],
    [-2.3, -0.3, 2.025],
    [-2.5, -0.3, 2.25],
    [-2.5, 0.0, 2.25],
    [-2.7, 0.0, 2.025],
    [-2.7, -0.3, 2.025],
    [-3.0, -0.3, 2.25],
    [-3.0, 0.0, 2.25],
    [-2.7, 0.0, 1.8],
    [-2.7, -0.3, 1.8],
    [-3.0, -0.3, 1.8],
    [-3.0, 0.0, 1.8],
    [-1.5, 0.3, 2.25],
    [-1.6, 0.3, 2.025],
    [-2.5, 0.3, 2.25],
    [-2.3, 0.3, 2.025],
    [-3.0, 0.3, 2.25],
    [-2.7, 0.3, 2.025],
    [-3.0, 0.3, 1.8],
    [-2.7, 0.3, 1.8],
    [-2.7, 0.0, 1.575],
    [-2.7, -0.3, 1.575],
    [-3.0, -0.3, 1.35],
    [-3.0, 0.0, 1.35],
    [-2.5, 0.0, 1.125],
    [-2.5, -0.3, 1.125],
    [-2.65, -0.3, 0.9375],
    [-2.65, 0.0, 0.9375],
    [-2.0, -0.3, 0.9],
    [-1.9, -0.3, 0.6],
    [-1.9, 0.0, 0.6],
    [-3.0, 0.3, 1.35],
    [-2.7, 0.3, 1.575],
    [-2.65, 0.3, 0.9375],
    [-2.5, 0.3, 1.125],
    [-1.9, 0.3, 0.6],
    [-2.0, 0.3, 0.9],
    [1.7, 0.0, 1.425],
    [1.7, -0.66, 1.425],
    [1.7, -0.66, 0.6],
    [1.7, 0.0, 0.6],
    [2.6, 0.0, 1.425],
    [2.6, -0.66, 1.425],
    [3.1, -0.66, 0.825],
    [3.1, 0.0, 0.825],
    [2.3, 0.0, 2.1],
    [2.3, -0.25, 2.1],
    [2.4, -0.25, 2.025],
    [2.4, 0.0, 2.025],
    [2.7, 0.0, 2.4],
    [2.7, -0.25, 2.4],
    [3.3, -0.25, 2.4],
    [3.3, 0.0, 2.4],
    [1.7, 0.66, 0.6],
    [1.7, 0.66, 1.425],
    [3.1, 0.66, 0.825],
    [2.6, 0.66, 1.425],
    [2.4, 0.25, 2.025],
    [2.3, 0.25, 2.1],
    [3.3, 0.25, 2.4],
    [2.7, 0.25, 2.4],
    [2.8, 0.0, 2.475],
    [2.8, -0.25, 2.475],
    [3.525, -0.25, 2.49375],
    [3.525, 0.0, 2.49375],
    [2.9, 0.0, 2.475],
    [2.9, -0.15, 2.475],
    [3.45, -0.15, 2.5125],
    [3.45, 0.0, 2.5125],
    [2.8, 0.0, 2.4],
    [2.8, -0.15, 2.4],
    [3.2, -0.15, 2.4],
    [3.2, 0.0, 2.4],
    [3.525, 0.25, 2.49375],
    [2.8, 0.25, 2.475],
    [3.45, 0.15, 2.5125],
    [2.9, 0.15, 2.475],
    [3.2, 0.15, 2.4],
    [2.8, 0.15, 2.4],
    [0.0, 0.0, 3.15],
    [0.8, 0.0, 3.15],
    [0.8, -0.45, 3.15],
    [0.45, -0.8, 3.15],
    [0.0, -0.8, 3.15],
    [0.0, 0.0, 2.85],
    [0.2, 0.0, 2.7],
    [0.2, -0.112, 2.7],
    [0.112, -0.2, 2.7],
    [0.0, -0.2, 2.7],
    [-0.45, -0.8, 3.15],
    [-0.8, -0.45, 3.15],
    [-0.8, 0.0, 3.15],
    [-0.112, -0.2, 2.7],
    [-0.2, -0.112, 2.7],
    [-0.2, 0.0, 2.7],
    [-0.8, 0.45, 3.15],
    [-0.45, 0.8, 3.15],
    [0.0, 0.8, 3.15],
    [-0.2, 0.112, 2.7],
    [-0.112, 0.2, 2.7],
    [0.0, 0.2, 2.7],
    [0.45, 0.8, 3.15],
    [0.8, 0.45, 3.15],
    [0.112, 0.2, 2.7],
    [0.2, 0.112, 2.7],
    [0.4, 0.0, 2.55],
    [0.4, -0.224, 2.55],
    [0.224, -0.4, 2.55],
    [0.0, -0.4, 2.55],
    [1.3, 0.0, 2.55],
    [1.3, -0.728, 2.55],
    [0.728, -1.3, 2.55],
    [0.0, -1.3, 2.55],
    [1.3, 0.0, 2.4],
    [1.3, -0.728, 2.4],
    [0.728, -1.3, 2.4],
    [0.0, -1.3, 2.4],
    [-0.224, -0.4, 2.55],
    [-0.4, -0.224, 2.55],
    [-0.4, 0.0, 2.55],
    [-0.728, -1.3, 2.55],
    [-1.3, -0.728, 2.55],
    [-1.3, 0.0, 2.55],
    [-0.728, -1.3, 2.4],
    [-1.3, -0.728, 2.4],
    [-1.3, 0.0, 2.4],
    [-0.4, 0.224, 2.55],
    [-0.224, 0.4, 2.55],
    [0.0, 0.4, 2.55],
    [-1.3, 0.728, 2.55],
    [-0.728, 1.3, 2.55],
    [0.0, 1.3, 2.55],
    [-1.3, 0.728, 2.4],
    [-0.728, 1.3, 2.4],
    [0.0, 1.3, 2.4],
    [0.224, 0.4, 2.55],
    [0.4, 0.224, 2.55],
    [0.728, 1.3, 2.55],
    [1.3, 0.728, 2.55],
    [0.728, 1.3, 2.4],
    [1.3, 0.728, 2.4],
    [0.0, 0.0, 0.0],
    [1.425, 0.0, 0.0],
    [1.425, 0.798, 0.0],
    [0.798, 1.425, 0.0],
    [0.0, 1.425, 0.0],
    [1.5, 0.0, 0.075],
    [1.5, 0.84, 0.075],
    [0.84, 1.5, 0.075],
    [0.0, 1.5, 0.075],
    [-0.798, 1.425, 0.0],
    [-1.425, 0.798, 0.0],
    [-1.425, 0.0, 0.0],
    [-0.84, 1.5, 0.075],
    [-1.5, 0.84, 0.075],
    [-1.5, 0.0, 0.075],
    [-1.425, -0.798, 0.0],
    [-0.798, -1.425, 0.0],
    [0.0, -1.425, 0.0],
    [-1.5, -0.84, 0.075],
    [-0.84, -1.5, 0.075],
    [0.0, -1.5, 0.075],
    [0.798, -1.425, 0.0],
    [1.425, -0.798, 0.0],
    [0.84, -1.5, 0.075],
    [1.5, -0.84, 0.075],
];

const PATCHES: [[u16; 16]; 32] = [
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
    [3, 16, 17, 18, 7, 19, 20, 21, 11, 22, 23, 24, 15, 25, 26, 27],
    [18, 28, 29, 30, 21, 31, 32, 33, 24, 34, 35, 36, 27, 37, 38, 39],
    [30, 40, 41, 0, 33, 42, 43, 4, 36, 44, 45, 8, 39, 46, 47, 12],
    [12, 13, 14, 15, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59],
    [15, 25, 26, 27, 51, 60, 61, 62, 55, 63, 64, 65, 59, 66, 67, 68],
    [27, 37, 38, 39, 62, 69, 70, 71, 65, 72, 73, 74, 68, 75, 76, 77],
    [39, 46, 47, 12, 71, 78, 79, 48, 74, 80, 81, 52, 77, 82, 83, 56],
    [56, 57, 58, 59, 84, 85, 86, 87, 88, 89, 90, 91, 92, 93, 94, 95],
    [59, 66, 67, 68, 87, 96, 97, 98, 91, 99, 100, 101, 95, 102, 103, 104],
    [68, 75, 76, 77, 98, 105, 106, 107, 101, 108, 109, 110, 104, 111, 112, 113],
    [77, 82, 83, 56, 107, 114, 115, 84, 110, 116, 117, 88, 113, 118, 119, 92],
    [120, 121, 122, 123, 124, 125, 126, 127, 128, 129, 130, 131, 132, 133, 134, 135],
    [123, 136, 137, 120, 127, 138, 139, 124, 131, 140, 141, 128, 135, 142, 143, 132],
    [132, 133, 134, 135, 144, 145, 146, 147, 148, 149, 150, 151, 68, 152, 153, 154],
    [135, 142, 143, 132, 147, 155, 156, 144, 151, 157, 158, 148, 154, 159, 160, 68],
    [161, 162, 163, 164, 165, 166, 167, 168, 169, 170, 171, 172, 173, 174, 175, 176],
    [164, 177, 178, 161, 168, 179, 180, 165, 172, 181, 182, 169, 176, 183, 184, 173],
    [173, 174, 175, 176, 185, 186, 187, 188, 189, 190, 191, 192, 193, 194, 195, 196],
    [176, 183, 184, 173, 188, 197, 198, 185, 192, 199, 200, 189, 196, 201, 202, 193],
    [203, 203, 203, 203, 204, 205, 206, 207, 208, 208, 208, 208, 209, 210, 211, 212],
    [203, 203, 203, 203, 207, 213, 214, 215, 208, 208, 208, 208, 212, 216, 217, 218],
    [203, 203, 203, 203, 215, 219, 220, 221, 208, 208, 208, 208, 218, 222, 223, 224],
    [203, 203, 203, 203, 221, 225, 226, 204, 208, 208, 208, 208, 224, 227, 228, 209],
    [209, 210, 211, 212, 229, 230, 231, 232, 233, 234, 235, 236, 237, 238, 239, 240],
    [212, 216, 217, 218, 232, 241, 242, 243, 236, 244, 245, 246, 240, 247, 248, 249],
    [218, 222, 223, 224, 243, 250, 251, 252, 246, 253, 254, 255, 249, 256, 257, 258],
    [224, 227, 228, 209, 252, 259, 260, 229, 255, 261, 262, 233, 258, 263, 264, 237],
    [265, 265, 265, 265, 266, 267, 268, 269, 270, 271, 272, 273, 92, 119, 118, 113],
    [265, 265, 265, 265, 269, 274, 275, 276, 273, 277, 278, 279, 113, 112, 111, 104],
    [265, 265, 265, 265, 276, 280, 281, 282, 279, 283, 284, 285, 104, 103, 102, 95],
    [265, 265, 265, 265, 282, 286, 287, 266, 285, 288, 289, 270, 95, 94, 93, 92],
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::obj::{parse_obj, to_obj};

    /// Patches whose first control row is one point repeated four times —
    /// the knob crown's four and the bottom's four — so their first row of
    /// quads collapses to one triangle each.
    const POLE_PATCHES: usize = 8;

    #[test]
    fn the_patch_seams_weld_into_one_surface() {
        let exported = to_obj(&tessellate(DEFAULT_SEGMENTS));
        let welded = parse_obj(exported.as_bytes()).expect("the exporter writes the subset the importer reads");

        let segments = usize::from(DEFAULT_SEGMENTS);
        assert_eq!(welded.faces.len(), PATCH_COUNT * segments * segments * 2 - POLE_PATCHES * segments);

        // Tripwire: the patches evaluate to 32 × 11 × 11 = 3872 grid points
        // and weld to 3241 distinct positions. The 631 that disappear are
        // the shared patch boundaries closing on each other, so a control
        // point mistyped off a seam — or an evaluator that walks a boundary
        // curve differently from the patch beside it — leaves those points
        // a hair apart and the count rises.
        assert_eq!(welded.positions.len(), 3241);
    }
}
