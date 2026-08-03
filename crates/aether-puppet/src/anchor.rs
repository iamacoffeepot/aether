//! Where each feature sits, measured off the material field.
//!
//! The charted face rests on one split: **placement derived, shape
//! authored**. This module is the derived half, and it is deliberately mute
//! about appearance — it reports where her eyes are and how big, and the
//! chart decides what an eye looks like.
//!
//! The split is forced rather than chosen. Her lips clear the crease
//! threshold only 12% of the time, and her eyes carry no relief at all,
//! because a pupil is painted texture over a smooth ball — the crease pass
//! finds a lid rim and nothing else. So extraction cannot supply the shape.
//! It can supply the coordinates, and those are the half that must not be
//! guessed: a drawn eye in the wrong place is worse than no eye.

use aether_math::{Vec2, Vec3};

use crate::labels::{self, Labels};
use crate::mesh::Mesh;

/// A feature's measured extent in the frontal plane, and how far forward
/// its own surface reaches.
#[derive(Clone, Copy, Debug)]
pub struct Anchor {
    pub centre: Vec2,
    pub half: Vec2,
    /// Frontmost `z` of the cells the anchor was measured from. Planting
    /// rays start in front of this, so they enter the head from outside it.
    pub front: f32,
}

/// Everything the chart needs to place a face on this subject.
///
/// Measured once when the subject changes rather than per frame: the
/// material field and the mesh both stand still, so the answer does too,
/// and the eye scan walks every vertex.
pub struct Anchors {
    pub mouth: Anchor,
    /// Her right at `-1`, her left at `+1`. One entry per side the field
    /// actually carries an eye class for, so a subject labelled on one side
    /// draws one eye rather than two wrong ones.
    pub eyes: Vec<(f32, Anchor)>,
    /// `None` when the eye and lip bands leave no window between them to
    /// search, which is what a field labelled for something other than a
    /// face looks like.
    pub nose: Option<Anchor>,
}

impl Anchors {
    pub fn measure(mesh: &Mesh, labels: &Labels) -> Option<Self> {
        let mouth = mouth(mesh, labels)?;
        let eyes = eyes(mesh, labels);
        let nose = nose(mesh, &eyes, &mouth);

        Some(Self { mouth, eyes, nose })
    }

    /// Where a planting ray starts, clear of the frontmost feature. Every
    /// mark shares it, so marks on one face are cast from one plane and
    /// cannot disagree about which surface is the front of her head.
    pub fn front(&self) -> f32 {
        self.mouth.front + 0.5
    }
}

/// Where the mouth is, measured from the `lips` class — from the field,
/// saying nothing about how it looks.
fn mouth(mesh: &Mesh, labels: &Labels) -> Option<Anchor> {
    let cells: Vec<Vec3> = mesh.positions.iter().copied().filter(|&p| labels.sample(p) == labels::LIPS).collect();

    (!cells.is_empty()).then(|| bounds(&cells))
}

/// Where each eye sits, from the `eye` class, split at the midline. The
/// brow above it is measured from the same anchor — a brow's whole job is
/// to be a certain distance above a certain eye.
fn eyes(mesh: &Mesh, labels: &Labels) -> Vec<(f32, Anchor)> {
    let mut anchors: Vec<(f32, Anchor)> = [-1.0f32, 1.0]
        .into_iter()
        .filter_map(|side| {
            let cells: Vec<Vec3> = mesh
                .positions
                .iter()
                .copied()
                .filter(|&p| p.x * side > 0.0 && labels.sample(p) == labels::EYE)
                .collect();

            (cells.len() >= 16).then(|| (side, bounds(&cells)))
        })
        .collect();

    // Share one size between the two, keeping each centre where it was
    // measured. The class is a few hundred cells over both eyes, and the
    // 10% the two sides disagree by is as easily the label boundary
    // wandering as the sculptor's intent — but drawn at full contrast an
    // eye 10% larger than its partner reads as a mistake rather than as a
    // face.
    if let [(_, left), (_, right)] = anchors.as_mut_slice() {
        let mean = (left.half + right.half) * 0.5;
        left.half = mean;
        right.half = mean;
    }

    anchors
}

/// How many rungs the midline profile is sampled at between the bands.
const NOSE_SAMPLES: usize = 48;

/// Where the nose is, found the way the depth profile finds it: the frontal
/// maximum along the midline, inside the window the eye and lip bands leave
/// between them.
///
/// There is no `nose` material class and there does not need to be. The two
/// classes either side of it bound the search, and within that window the
/// midline profile has exactly one maximum — it dips at the eye line,
/// bulges a little proud at the tip, and falls away again to the lip.
fn nose(mesh: &Mesh, eyes: &[(f32, Anchor)], lips: &Anchor) -> Option<Anchor> {
    let eye = &eyes.first()?.1;
    let (floor, ceiling) = (lips.centre.y + lips.half.y, eye.centre.y - eye.half.y);
    if ceiling <= floor {
        return None;
    }

    let into = Vec3::new(0.0, 0.0, -1.0);
    let front = eye.front + 0.5;
    let tip = (0..NOSE_SAMPLES)
        .map(|i| floor + (ceiling - floor) * i as f32 / (NOSE_SAMPLES - 1) as f32)
        .filter_map(|y| mesh.hit(Vec3::new(0.0, y, front), into).map(|c| (y, c.pos.z)))
        .max_by(|a, b| a.1.total_cmp(&b.1))?;

    // Width comes off the gap between the eyes rather than from the nose
    // itself, which has no measurable edge to read. The inner corner is
    // where a drawn nose is understood to sit under.
    let gap = eyes.iter().map(|(_, at)| (at.centre.x.abs() - at.half.x).abs()).fold(f32::MAX, f32::min);

    Some(Anchor { centre: Vec2::new(0.0, tip.0), half: Vec2::new(gap * 0.72, (ceiling - floor) * 0.34), front: tip.1 })
}

/// Centre at the mean and half-extents from the bounding box — the mean
/// rather than the box centre because a label boundary that wanders on one
/// side should move the mark by a fraction of that, not by half of it.
fn bounds(cells: &[Vec3]) -> Anchor {
    let (lo, hi) = cells.iter().fold((Vec3::splat(f32::MAX), Vec3::splat(f32::MIN)), |(l, h), &p| {
        (Vec3::new(l.x.min(p.x), l.y.min(p.y), l.z.min(p.z)), Vec3::new(h.x.max(p.x), h.y.max(p.y), h.z.max(p.z)))
    });
    let mean = cells.iter().fold(Vec3::splat(0.0), |a, &p| a + p) / cells.len() as f32;

    Anchor { centre: Vec2::new(mean.x, mean.y), half: Vec2::new((hi.x - lo.x) * 0.5, (hi.y - lo.y) * 0.5), front: hi.z }
}
