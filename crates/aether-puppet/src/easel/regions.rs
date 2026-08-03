//! The painter's two input maps, baked through the drawing's own camera.
//!
//! Before a brush touches paper a painter has answered two questions per
//! patch of the subject: what is this, and how lit is it. This pass bakes
//! both answers per pixel — `class` carries the material at the nearest
//! surface (`0` background, then the [`labels`](crate::labels) ids), `tone`
//! carries the key light there — so a wash engine downstream places pigment
//! by material and reserves paper by light instead of guessing from the
//! strokes.
//!
//! `facing` carries the normal turned toward the eye, which is how much the
//! surface confronts the viewer, so surface-anchored paint policy — a blush
//! — can fade as a cheek turns away instead of stamping a sliver at full
//! frontal strength.
//!
//! One z-buffered barycentric pass over the triangles. The camera, mesh and
//! light are the ones the ink drawing used, which is the point: the maps
//! register with the drawing to the pixel.

use aether_math::{Mat4, Vec3};

use crate::extract::Settings;
use crate::feature::SurfacePoint;
use crate::labels::Labels;
use crate::mesh::Mesh;

/// The three planes, each `width * height` long in row-major order.
pub struct RegionPlanes {
    pub width: usize,
    pub height: usize,
    /// Material class at the nearest surface; `0` where nothing was drawn.
    pub class: Vec<u8>,
    /// Key-light term at the nearest surface, as [`Settings::tone`] gives
    /// it — unclamped, because the face lift can carry it past one and
    /// where the range is cut belongs to whoever mixes the pigment.
    pub tone: Vec<f32>,
    /// `normalize(eye - p) . n` at the nearest surface, zero where the
    /// surface has turned away.
    pub facing: Vec<f32>,
}

/// A vertex after the camera: where it lands on the page, and the depth the
/// buffer compares.
#[derive(Clone, Copy)]
struct Projected {
    page_x: f32,
    page_y: f32,
    depth: f32,
}

/// A world point through the camera, or `None` for anything at or behind
/// the near plane, whose homogeneous divide would fold it back onto the
/// page mirrored.
///
/// The pixel mapping is the one the substrate's own pipeline applies to the
/// triangles this drawing emits: x runs right across `[-1, 1]`, y runs up
/// across the same range against a framebuffer whose rows run downward, so
/// the vertical axis flips here.
fn project(view_proj: &Mat4, p: Vec3, half_width: f32, half_height: f32) -> Option<Projected> {
    let clip = *view_proj * p.extend(1.0);
    if clip.w <= 0.0 {
        return None;
    }

    // Depth is NDC z rather than the view distance sitting in `clip.w`.
    // `z/w` is the one depth quantity affine in screen space, so the
    // barycentric blend below is exact under perspective where a blend of
    // view distance is not, and `perspective_rh` is wgpu-style — 0 at the
    // near plane, 1 at the far one, nearer always smaller.
    let ndc = Vec3::new(clip.x, clip.y, clip.z) / clip.w;

    Some(Projected { page_x: (ndc.x + 1.0) * half_width, page_y: (1.0 - ndc.y) * half_height, depth: ndc.z })
}

/// Rasterize the three planes for one view of `mesh`.
///
/// `eye` and `view_proj` are the camera the drawing was made from, passed
/// apart because the eye is what `facing` asks about and the matrix is what
/// projection asks about — deriving one from the other would let the maps
/// drift from the drawing.
pub fn rasterize(
    mesh: &Mesh,
    labels: &Labels,
    settings: &Settings,
    eye: Vec3,
    view_proj: &Mat4,
    width: usize,
    height: usize,
) -> RegionPlanes {
    let count = width * height;
    let mut planes =
        RegionPlanes { width, height, class: vec![0; count], tone: vec![0.0; count], facing: vec![0.0; count] };
    if count == 0 {
        return planes;
    }

    let (half_width, half_height) = (width as f32 * 0.5, height as f32 * 0.5);
    let mut depth = vec![f32::INFINITY; count];

    for face in &mesh.faces {
        let corners = face.map(|i| i as usize);
        let world = corners.map(|i| mesh.positions[i]);
        let [Some(a), Some(b), Some(c)] = world.map(|p| project(view_proj, p, half_width, half_height)) else {
            continue;
        };

        // Signed, so both windings rasterize: a back-facing triangle
        // negates the area and every weight with it, and the interior test
        // below reads the ratios.
        let area = (b.page_x - a.page_x) * (c.page_y - a.page_y) - (b.page_y - a.page_y) * (c.page_x - a.page_x);
        if area.abs() < 1e-6 {
            continue;
        }

        // One sample per face, at the centroid. The field is a coarse
        // lattice around the surface and a triangle is far smaller than a
        // cell, so per-pixel sampling would cost three interpolations to
        // arrive at the same answer.
        let class = labels.sample((world[0] + world[1] + world[2]) / 3.0);
        let lit = corners.map(|i| settings.tone(&SurfacePoint::on_surface(mesh.positions[i], mesh.normals[i])));
        let confront =
            corners.map(|i| (eye - mesh.positions[i]).normalize_or(mesh.normals[i]).dot(mesh.normals[i]).max(0.0));

        let min_x = a.page_x.min(b.page_x).min(c.page_x).floor().max(0.0) as usize;
        let max_x = (a.page_x.max(b.page_x).max(c.page_x).ceil() as usize).min(width - 1);
        let min_y = a.page_y.min(b.page_y).min(c.page_y).floor().max(0.0) as usize;
        let max_y = (a.page_y.max(b.page_y).max(c.page_y).ceil() as usize).min(height - 1);

        for y in min_y..=max_y {
            for x in min_x..=max_x {
                let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
                let wa = ((b.page_x - px) * (c.page_y - py) - (b.page_y - py) * (c.page_x - px)) / area;
                let wb = ((c.page_x - px) * (a.page_y - py) - (c.page_y - py) * (a.page_x - px)) / area;
                let wc = 1.0 - wa - wb;
                if wa < 0.0 || wb < 0.0 || wc < 0.0 {
                    continue;
                }

                let z = wa * a.depth + wb * b.depth + wc * c.depth;
                let at = y * width + x;
                if z < depth[at] {
                    depth[at] = z;
                    planes.class[at] = class;
                    planes.tone[at] = wa * lit[0] + wb * lit[1] + wc * lit[2];
                    planes.facing[at] = wa * confront[0] + wb * confront[1] + wc * confront[2];
                }
            }
        }
    }

    planes
}

#[cfg(test)]
mod tests {
    use core::fmt::Write as _;

    use super::{RegionPlanes, rasterize};
    use crate::extract::Settings;
    use crate::labels::{self, Labels};
    use crate::mesh::Mesh;
    use aether_math::{Mat4, Vec3};

    /// Page size every fixture rasterizes at. Even, so no pixel centre
    /// lands on the world origin and a test can name the half a pixel
    /// belongs to without a tie.
    const SIDE: usize = 16;

    const EYE: Vec3 = Vec3::new(0.0, 0.0, 5.0);

    /// A mesh from bare vertices and triangles. `Mesh` derives its normals,
    /// bounds and bvh at construction and OBJ bytes are the only door in,
    /// so a fixture goes through the text.
    fn mesh(vertices: &[Vec3], faces: &[[u32; 3]]) -> Mesh {
        let mut text = String::new();
        for v in vertices {
            writeln!(text, "v {} {} {}", v.x, v.y, v.z).expect("format vertex");
        }
        for f in faces {
            writeln!(text, "f {} {} {}", f[0] + 1, f[1] + 1, f[2] + 1).expect("format face");
        }

        Mesh::from_obj_bytes(text.as_bytes(), 0).expect("fixture mesh")
    }

    /// A 2x2x2 field over the unit cube, `HAIR` where `x < 0` and `SKIN`
    /// where `x > 0`, so a sample's x alone decides its class.
    fn split_field() -> Labels {
        // Ten bytes of `.npy` preamble, of which only the little-endian
        // header length at [8..10] is read; zero puts the cells first.
        let mut bytes = vec![0u8; 10];
        bytes.extend([labels::HAIR; 4]);
        bytes.extend([labels::SKIN; 4]);

        Labels::parse(&bytes, Vec3::splat(-1.0), Vec3::splat(1.0), 0.0).expect("fixture field")
    }

    /// Key light straight down the view axis, so a facet square to the eye
    /// tones to exactly one and a facet turned away tones to the ambient
    /// floor.
    fn settings() -> Settings {
        Settings { light: Vec3::new(0.0, 0.0, 1.0), ambient: 0.25, face_lift: 0.0, ..Settings::default() }
    }

    /// Orthographic down -z from `EYE`, so world x and y reach the page by
    /// a rule a test can invert and name the pixel it expects.
    fn orthographic() -> Mat4 {
        Mat4::orthographic_rh(-1.0, 1.0, -1.0, 1.0, 1.0, 10.0)
            * Mat4::look_at_rh(EYE, Vec3::splat(0.0), Vec3::new(0.0, 1.0, 0.0))
    }

    fn perspective() -> Mat4 {
        Mat4::perspective_rh(1.2, 1.0, 0.1, 20.0) * Mat4::look_at_rh(EYE, Vec3::splat(0.0), Vec3::new(0.0, 1.0, 0.0))
    }

    /// The world point a pixel centre sits over under `orthographic`.
    fn world_at(x: usize, y: usize) -> (f32, f32) {
        ((x as f32 + 0.5) / SIDE as f32 * 2.0 - 1.0, 1.0 - (y as f32 + 0.5) / SIDE as f32 * 2.0)
    }

    /// Which pixels the pass drew on, read off `tone` rather than `class`:
    /// the field answers `0` for anything outside its lattice, so a drawn
    /// pixel can honestly carry no class, while `settings`'s ambient floor
    /// puts every drawn pixel's tone above zero.
    fn covered(planes: &RegionPlanes) -> Vec<(usize, usize)> {
        (0..planes.tone.len())
            .filter(|&i| planes.tone[i] != 0.0)
            .map(|i| (i % planes.width, i / planes.width))
            .collect()
    }

    fn rasterize_fixture(mesh: &Mesh, view_proj: &Mat4) -> RegionPlanes {
        rasterize(mesh, &split_field(), &settings(), EYE, view_proj, SIDE, SIDE)
    }

    /// Tripwire: the page's vertical axis runs opposite the world's. A
    /// triangle in the world's upper-right quadrant must land in the page's
    /// upper-right block, and a dropped flip puts it in the lower one while
    /// every other assertion in this file still passes.
    #[test]
    fn world_up_lands_on_the_page_top() {
        let subject =
            mesh(&[Vec3::new(0.1, 0.1, 0.0), Vec3::new(0.9, 0.1, 0.0), Vec3::new(0.9, 0.9, 0.0)], &[[0, 1, 2]]);
        let planes = rasterize_fixture(&subject, &orthographic());

        let drawn = covered(&planes);
        assert!(!drawn.is_empty(), "the triangle sits well inside the frame");
        assert!(
            drawn.iter().all(|&(x, y)| x >= SIDE / 2 && y < SIDE / 2),
            "world +x +y is the page's right half and top half, got {drawn:?}",
        );
    }

    /// Tripwire: the class plane is a per-face answer sampled at the
    /// centroid, not a per-pixel one. The face below straddles the field's
    /// split, so a pixel on its far side carries the centroid's class and
    /// not its own.
    #[test]
    fn class_is_the_centroid_sample_across_the_whole_face() {
        let subject =
            mesh(&[Vec3::new(-0.9, -0.5, 0.0), Vec3::new(0.5, -0.5, 0.0), Vec3::new(-0.9, 0.5, 0.0)], &[[0, 1, 2]]);
        let planes = rasterize_fixture(&subject, &orthographic());

        let (x, y) = (9, 11);
        let (world_x, world_y) = world_at(x, y);
        assert!(world_x > 0.0, "the probe pixel is on the field's SKIN side");
        assert_eq!(
            split_field().sample(Vec3::new(world_x, world_y, 0.0)),
            labels::SKIN,
            "the probe pixel samples SKIN on its own",
        );

        assert_eq!(planes.class[y * SIDE + x], labels::HAIR, "the face's centroid is on the HAIR side");
    }

    /// Tripwire: the depth test, and its independence from submission
    /// order. Both faces cover the probe pixel; the nearer one owns it
    /// whichever is rasterized second.
    #[test]
    fn the_nearer_face_owns_the_pixel_in_either_order() {
        let near = [Vec3::new(-0.9, -0.9, 0.5), Vec3::new(0.3, -0.9, 0.5), Vec3::new(-0.9, 0.9, 0.5)];
        let far = [Vec3::new(0.9, -0.9, -0.5), Vec3::new(0.9, 0.9, -0.5), Vec3::new(-0.3, -0.9, -0.5)];
        let vertices: Vec<Vec3> = near.into_iter().chain(far).collect();

        let (x, y) = (8, 13);
        for faces in [[[0, 1, 2], [3, 4, 5]], [[3, 4, 5], [0, 1, 2]]] {
            let planes = rasterize_fixture(&mesh(&vertices, &faces), &orthographic());

            assert_eq!(planes.class[y * SIDE + x], labels::HAIR, "the near face is the HAIR one, order {faces:?}");
        }
    }

    /// Tripwire: tone reads the light and facing reads the eye, separately.
    /// The same triangle wound both ways is square to the eye in one and
    /// turned away in the other — lit to one against the ambient floor, and
    /// confronting the viewer against nothing at all.
    #[test]
    fn tone_reads_the_light_and_facing_reads_the_eye() {
        let corners = [Vec3::new(-0.8, -0.8, 0.0), Vec3::new(0.8, -0.8, 0.0), Vec3::new(0.0, 0.8, 0.0)];
        let (x, y) = (8, 8);

        let toward = rasterize_fixture(&mesh(&corners, &[[0, 1, 2]]), &orthographic());
        assert!((toward.tone[y * SIDE + x] - 1.0).abs() < 1e-5, "a facet square to the light is fully lit");
        assert!(toward.facing[y * SIDE + x] > 0.95, "a facet square to the eye confronts it");

        let away = rasterize_fixture(&mesh(&corners, &[[2, 1, 0]]), &orthographic());
        assert!((away.tone[y * SIDE + x] - 0.25).abs() < 1e-5, "a facet turned from the light keeps the ambient floor");
        assert_eq!(away.facing[y * SIDE + x], 0.0, "a facet turned from the eye confronts it not at all");
    }

    /// Tripwire: geometry behind the near plane is dropped rather than
    /// divided through a negative w, which would mirror it back onto the
    /// page as a ghost of itself.
    #[test]
    fn geometry_behind_the_eye_never_reaches_the_page() {
        let corners = [Vec3::new(-0.8, -0.8, 0.0), Vec3::new(0.8, -0.8, 0.0), Vec3::new(0.0, 0.8, 0.0)];
        let front = mesh(&corners, &[[0, 1, 2]]);
        assert!(!covered(&rasterize_fixture(&front, &perspective())).is_empty(), "the control triangle is in frame");

        let behind = mesh(&corners.map(|p| p + Vec3::new(0.0, 0.0, 10.0)), &[[0, 1, 2]]);
        assert!(covered(&rasterize_fixture(&behind, &perspective())).is_empty(), "nothing behind the eye is drawn");
    }
}
