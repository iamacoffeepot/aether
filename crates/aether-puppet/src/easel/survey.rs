//! What the subject looks like from here, measured off its own geometry.
//!
//! Two quantities the develop needs as *numbers* rather than as planes,
//! and the class plane they used to be read off now lives on the GPU
//! where nothing can read it back (ADR-0170 declines a readback path by
//! design). Both are answered here instead, from the surface itself:
//!
//! - **Where each material sits.** Every wash places its pours, its lost
//!   edge and its thrown drops about its region's centroid, and a
//!   centroid is a mean — so it can be taken over the surface that
//!   projects into the region instead of over the region's pixels.
//!   Weighting each vertex by the area it speaks for and by how squarely
//!   it turns toward the eye makes that mean the projected-area mean the
//!   pixels would have given, up to what occlusion hides: a front-facing
//!   patch behind the nose still counts here and does not there.
//! - **How much of an eye the viewer can see.** The gate that keeps a
//!   cheek from blushing off the back of her head when the fringe has
//!   eaten its eye. Read off the label plane this was a count of eye
//!   pixels near the eye; asked of the geometry it is the question
//!   itself — cast the lid loop against the subject and see how much of
//!   it comes back.
//!
//! Everything view-independent is measured once per subject in
//! [`Survey::measure`]; [`Survey::centroids`] is the per-frame half and
//! is a single pass over the vertices.

use core::array;

use aether_math::{Mat4, Vec2, Vec3};

use super::regions;
use crate::feature::SurfacePoint;
use crate::labels::CLASSES;
use crate::mesh::Mesh;
use crate::visibility;

use super::palette::Palette;

/// Centroid slots, indexed by class id: `0` is the background, which
/// never carries one, and the labelled classes run up to
/// [`CLASSES`].
pub const SLOTS: usize = CLASSES + 1;

/// Visible fraction of its own lid loop over which an eye earns its
/// cheek a blush.
///
/// The window sits well inside both ends. An eye in plain view returns
/// its whole loop and saturates; an eye the fringe has taken returns
/// almost none of it and gates to nothing. What the window has to avoid
/// is the middle reading as either extreme — a lid loop half behind a
/// lock of hair belongs to a cheek that is half turned away, and the ramp
/// is what makes it fade rather than switch.
const SEEN: (f32, f32) = (0.35, 0.75);

/// The subject's view-independent measurements, taken once when it loads.
pub struct Survey {
    /// Each vertex's material class after the palette's remap, or `0`
    /// where the field labelled nothing. Argmax over the blurred
    /// indicators — the same rule the bake's fragment stage applies to
    /// the interpolated ones, taken here at the vertices themselves.
    class: Vec<u8>,
    /// How much surface each vertex speaks for: a third of every face it
    /// belongs to, which is the weight that makes a mean over vertices a
    /// mean over the surface rather than over the tessellation.
    area: Vec<f32>,
}

impl Survey {
    /// Measure `mesh` under `scores`
    /// ([`Labels::vertex_scores`](crate::labels::Labels::vertex_scores)),
    /// with the fall-through of the box that will paint it.
    pub fn measure(mesh: &Mesh, scores: &[[f32; CLASSES]], palette: &Palette) -> Self {
        let class = (0..mesh.positions.len())
            .map(|index| {
                let indicators = scores.get(index).copied().unwrap_or([0.0; CLASSES]);
                palette.remapped(argmax_class(&indicators))
            })
            .collect();

        let mut area = vec![0.0; mesh.positions.len()];
        for face in &mesh.faces {
            let [a, b, c] = face.map(|corner| mesh.positions[corner as usize]);
            let third = (b - a).cross(c - a).length() / 6.0;
            for &corner in face {
                area[corner as usize] += third;
            }
        }

        Self { class, area }
    }

    /// Where each material sits on the canvas from this eye, or `None`
    /// for a class with no surface in view.
    ///
    /// One pass over the vertices: project, weight by area and facing,
    /// accumulate into the class' slot. A vertex the near plane has eaten
    /// or the frame has cropped is dropped, which is what the pixels the
    /// oracle counts would have done with it.
    pub fn centroids(
        &self,
        mesh: &Mesh,
        eye: Vec3,
        view_proj: &Mat4,
        width: usize,
        height: usize,
    ) -> [Option<Vec2>; SLOTS] {
        let mut sum = [Vec2::new(0.0, 0.0); SLOTS];
        let mut weight = [0.0f32; SLOTS];

        for (index, (&position, &normal)) in mesh.positions.iter().zip(&mesh.normals).enumerate() {
            let class = usize::from(self.class[index]);
            if class == 0 || class >= SLOTS {
                continue;
            }

            let facing = (eye - position).normalize_or(normal).dot(normal);
            if facing <= 0.0 {
                continue;
            }
            let Some(at) = regions::on_canvas(view_proj, position, width, height) else {
                continue;
            };
            if at.x < 0.0 || at.y < 0.0 || at.x >= width as f32 || at.y >= height as f32 {
                continue;
            }

            let speaks_for = self.area[index] * facing;
            sum[class] += at * speaks_for;
            weight[class] += speaks_for;
        }

        array::from_fn(|class| (weight[class] > 0.0).then(|| sum[class] / weight[class]))
    }
}

/// How much of one eye's lid loop the viewer can actually see, through
/// the presence ramp.
///
/// `aperture` is the chart's own planted loop in world space. Each point
/// is cast at the eye through the subject's own occlusion index, lifted
/// off the surface by the mesh's bias exactly as every other occlusion
/// question in the drawing is — the fringe that hides an eye is real
/// geometry standing in front of it, and this is the machinery that
/// already knows.
pub fn presence(mesh: &Mesh, eye: Vec3, aperture: &[Vec3]) -> f32 {
    if aperture.is_empty() {
        return 0.0;
    }

    let bias = mesh.surface_bias();
    let seen = aperture
        .iter()
        .filter(|&&point| {
            let toward = (eye - point).normalize_or(Vec3::new(0.0, 0.0, 1.0));

            !visibility::hidden(mesh, eye, &SurfacePoint::on_surface(point, toward), bias)
        })
        .count();

    super::image::smoothstep(SEEN.0, SEEN.1, seen as f32 / aperture.len() as f32)
}

/// The winning class of a score vector, `0` when nothing scored — the
/// same strict-improvement argmax [`regions`] applies to a blended one.
fn argmax_class(scores: &[f32; CLASSES]) -> u8 {
    let (mut class, mut best) = (0, 0.0);
    for (index, &score) in scores.iter().enumerate() {
        if score > best {
            (class, best) = (index as u8 + 1, score);
        }
    }

    class
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::labels::{HAIR, SKIN};

    /// Every fixture here is her own classes, so the box is hers.
    fn palette() -> Palette {
        Palette::canonical()
    }

    /// A quad split into two triangles at `z = 0`, facing `+z`, and
    /// standing well inside the frame — a corner exactly on the frame's
    /// edge projects onto a column the canvas does not have, and is
    /// dropped exactly as the pixels it would have covered are.
    fn quad() -> Mesh {
        Mesh::from_obj_bytes(b"v -0.5 -0.5 0\nv 0.5 -0.5 0\nv 0.5 0.5 0\nv -0.5 0.5 0\nf 1 2 3\nf 1 3 4\n", 0)
            .expect("fixture mesh")
    }

    fn orthographic() -> Mat4 {
        Mat4::orthographic_rh(-1.0, 1.0, -1.0, 1.0, 1.0, 10.0)
            * Mat4::look_at_rh(Vec3::new(0.0, 0.0, 5.0), Vec3::ZERO, Vec3::new(0.0, 1.0, 0.0))
    }

    /// Tripwire: each material's centroid lands on its own surface, and
    /// the canvas' vertical axis is the page's rather than the world's.
    ///
    /// Every wash places its pours, its lost edge and its thrown drops
    /// about this point, so a centroid taken through a dropped axis flip
    /// or a mixed-up class slot still produces a complete, plausible
    /// painting — with the hair's tide line hanging off the skin.
    #[test]
    fn each_material_centroid_lands_on_its_own_side_of_the_canvas() {
        let mesh = quad();
        let mut scores = vec![[0.0; CLASSES]; mesh.positions.len()];
        for (index, score) in scores.iter_mut().enumerate() {
            let class = if mesh.positions[index].x < 0.0 {
                HAIR
            } else {
                SKIN
            };
            score[usize::from(class) - 1] = 1.0;
        }

        let survey = Survey::measure(&mesh, &scores, &palette());
        let centroids = survey.centroids(&mesh, Vec3::new(0.0, 0.0, 5.0), &orthographic(), 100, 100);

        let hair = centroids[usize::from(HAIR)].expect("the hair side is in view");
        let skin = centroids[usize::from(SKIN)].expect("the skin side is in view");
        assert!(hair.x < 50.0, "the hair side sits left of the canvas centre, got {hair:?}");
        assert!(skin.x > 50.0, "the skin side sits right of it, got {skin:?}");
    }

    /// Tripwire: a surface turned away from the eye carries no centroid
    /// at all.
    ///
    /// The wash reads `None` as "this material has no coverage" and lays
    /// nothing down. A centroid that survived the turn would place a full
    /// wash for a region the viewer cannot see.
    #[test]
    fn a_surface_turned_away_carries_no_centroid() {
        let mesh = quad();
        let mut scores = vec![[0.0; CLASSES]; mesh.positions.len()];
        for score in &mut scores {
            score[usize::from(HAIR) - 1] = 1.0;
        }

        let survey = Survey::measure(&mesh, &scores, &palette());
        let behind = Mat4::orthographic_rh(-1.0, 1.0, -1.0, 1.0, 1.0, 10.0)
            * Mat4::look_at_rh(Vec3::new(0.0, 0.0, -5.0), Vec3::ZERO, Vec3::new(0.0, 1.0, 0.0));

        assert!(
            survey.centroids(&mesh, Vec3::new(0.0, 0.0, -5.0), &behind, 100, 100)[usize::from(HAIR)].is_none(),
            "a quad seen from behind confronts the viewer nowhere",
        );
    }
}
