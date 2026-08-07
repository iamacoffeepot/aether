//! Face paint: the accents that ride on the chart's own geometry.
//!
//! One law separates these from the material washes. A wash asks the baked
//! label plane what a pixel is; an accent asks the [`chart`](crate::chart)
//! where a feature *is* — the iris centre the ink drew its ring on, the lid
//! curves it drew its lashes along — and never re-derives either from the
//! segmentation. A sticker iris centred on the eye label's blob is the
//! failure that rule exists for: the blob is the sculpt's painted eye, so
//! the paint sits where the texture is and she looks past the viewer.
//!
//! Two accents land here. The **iris** is a meta-material — it goes through
//! the same pours, rim and granulation as hair or dress, from a mask the
//! chart supplies rather than the field — clipped to the aperture and cut
//! around the slit, because the pupil is a reserve: bare paper inside the
//! ink's own outline, since painted black reads wrong under a pen drawing.
//! The **blush** is a flush of skin colour on the cheek apple, hung off the
//! same frame and gated twice over — by how much of its eye is actually in
//! view, and by how squarely its cheek confronts the viewer.

use core::f32::consts::PI;
use core::ops::RangeInclusive;

use aether_math::{Mat4, Vec2};

use super::field::Planes;
use super::image;
use super::palette::{self, EYE_CLASS, Palette, SKIN_CLASS};
use super::regions;
use crate::chart::EyeFrame;

/// How far the aperture clip is softened, in reference-sheet pixels, and
/// where its edge is taken afterwards.
///
/// Soft rather than hard because the polygon is a straight-edged sample of
/// a curve: thresholding a blurred fill puts the boundary back on the curve
/// the chart meant instead of on the chords it handed over.
pub const CLIP_BLUR: f32 = 1.6;
const CLIP_EDGE: (f32, f32) = (0.3, 0.6);

/// Clip strength under which a pixel is outside the eye and not worth
/// measuring against the iris at all.
const CLIP_FLOOR: f32 = 0.02;

/// Where the iris rim falls, in iris radii, and where the slit's does.
///
/// Both are ramps rather than edges, and the slit's runs the other way: the
/// wash builds toward the rim and cuts *out* around the pupil, so what is
/// left inside the slit is paper.
const IRIS_RIM: (f32, f32) = (1.1, 0.95);
const SLIT_RIM: (f32, f32) = (0.85, 1.05);

/// Floor under a pupil half-axis. A style with a zero axis has no slit to
/// reserve, and a zero divisor at the iris centre is the one input that
/// would put a NaN through the whole wash.
const PUPIL_FLOOR: f32 = 1e-3;

/// How far past its own axes an eye is measured, as a multiple of the
/// larger radius plus a margin in reference-sheet pixels.
const IRIS_REACH: f32 = 1.4;
const IRIS_MARGIN: f32 = 4.0;

/// How much heavier the iris runs where the lid crosses it, and the band
/// in iris radii over which that weight arrives.
///
/// An iris under a lid is not an even disc — the lash line shadows its top,
/// and a flat one reads as a printed dot.
const LIFT: f32 = 0.6;
const LIFT_BAND: (f32, f32) = (0.1, 0.6);

/// Determinant under which a projected frame has collapsed onto a line and
/// there is no basis left to invert — an eye seen exactly edge-on.
const DEGENERATE: f32 = 1e-6;

/// Visible fraction of an eye's own aperture area over which its cheek
/// earns a blush.
///
/// Measured on the subject: a fully visible eye reads 1.4 to 1.6 of that
/// area, and the fringe-hidden far eye reads 0.53. The window sits between
/// them, so an eye the viewer cannot see gives its cheek no blush at all
/// and the blush vanishes with the ink it was hung off.
const PRESENCE: (f32, f32) = (0.7, 1.1);

/// How far out the presence count reaches around an eye centre, in eye
/// sizes, across then down.
const PRESENCE_SPAN: (f32, f32) = (2.52, 1.8);

/// Where the cheek apple sits relative to its eye, in eye sizes: outward
/// and down.
///
/// Outward matters more than it looks. Hung straight below the eye the
/// patch crosses the nose the moment she turns three-quarters on, and a
/// blush over the bridge reads as a bruise.
const APPLE: (f32, f32) = (1.8, 4.4);

/// The apple's radii, in eye sizes.
const APPLE_RADII: (f32, f32) = (4.2, 3.3);

/// How squarely a cheek must confront the viewer to hold its blush.
///
/// Surface-anchored policy fades as the surface turns away: without this
/// the far cheek's grazing sliver takes the same flush a frontal cheek
/// does, at a tenth of the width, and reads as a stripe down her jaw.
const FACING: (f32, f32) = (0.42, 0.62);

/// How far the skin mask and the flush itself are softened, in
/// reference-sheet pixels, and how much of the flush survives.
pub const SKIN_BLUR: f32 = 4.0;
pub const FLUSH_BLUR: f32 = 8.0;
const FLUSH_STRENGTH: f32 = 0.55;

/// One eye's frame on the canvas: the chart's planted frame after the
/// develop's own camera has had it.
pub struct Eye {
    /// Which of the chart's planted frames this eye came from.
    ///
    /// [`project`] drops a frame the near plane has eaten, so the
    /// projected list does not index the planted one. Everything asked of
    /// the *world* frame afterwards — how much of the lid loop the viewer
    /// can see ([`survey::presence`](crate::easel::survey::presence))
    /// above all — is asked through this.
    frame: usize,
    centre: Vec2,
    /// The iris' two radii as canvas offsets. Projected rather than
    /// scaled, so a turned eye's ellipse is foreshortened by the same
    /// camera that foreshortened the ink around it.
    width: Vec2,
    height: Vec2,
    /// Pupil half-axes as fractions of the iris radius.
    pupil: Vec2,
    aperture: Vec<Vec2>,
}

impl Eye {
    /// Which planted frame this eye was projected from.
    pub fn frame(&self) -> usize {
        self.frame
    }

    /// Where the iris centre landed on the canvas.
    pub fn centre(&self) -> Vec2 {
        self.centre
    }

    /// Pupil half-axes as fractions of the iris radius.
    pub fn pupil(&self) -> Vec2 {
        self.pupil
    }

    /// The lid loop, projected onto the canvas.
    pub fn aperture(&self) -> &[Vec2] {
        &self.aperture
    }

    /// How far out from the centre this eye is measured, in canvas
    /// pixels — the box `irises` walks and the GPU iris pass tests
    /// against.
    pub fn reach(&self, height: usize) -> f32 {
        self.size() * IRIS_REACH + image::tuned(IRIS_MARGIN, height)
    }

    /// How big this eye reads on the canvas — the longer of its two
    /// projected radii, which is the unit every accent hung off it is
    /// measured in.
    pub fn size(&self) -> f32 {
        self.width.length().max(self.height.length())
    }

    /// The projected frame inverted, as the two rows that take a canvas
    /// offset to iris coordinates — where the unit circle is the iris' own
    /// rim. `None` once the two axes have collapsed onto one line.
    pub fn inverse(&self) -> Option<(Vec2, Vec2)> {
        let determinant = self.width.x * self.height.y - self.width.y * self.height.x;

        (determinant.abs() > DEGENERATE).then(|| {
            (
                Vec2::new(self.height.y, -self.height.x) / determinant,
                Vec2::new(-self.width.y, self.width.x) / determinant,
            )
        })
    }
}

/// The face paint, as planes over the whole canvas.
pub struct Accents {
    /// The iris meta-material's coverage: clipped to the aperture, cut
    /// around the slit.
    iris: Vec<f32>,
    /// A multiplier over the iris' finished density, heavier where the lid
    /// crosses it. One everywhere else.
    pub lift: Vec<f32>,
    /// The cheek flush, already gated and softened — a density, not a
    /// mask, since nothing downstream washes it further.
    pub blush: Vec<f32>,
}

impl Accents {
    /// A meta-material's coverage, or `None` for a class the chart paints
    /// nothing for.
    pub fn mask(&self, class: u8) -> Option<&[f32]> {
        (class == palette::IRIS).then_some(self.iris.as_slice())
    }
}

/// Paint every accent for one develop.
///
/// `frames` are the chart's planted eye frames and `view_proj` the camera
/// the maps under them were baked through — the same one the ink was drawn
/// from, which is what puts the paint in the drawing's own eyes.
pub fn paint(frames: &[EyeFrame], view_proj: &Mat4, planes: &Planes<'_>, palette: &Palette) -> Accents {
    let eyes = project(frames, view_proj, planes.width, planes.height);
    let (iris, lift) = irises(&eyes, planes);
    let presence = presences(&eyes, planes, palette);

    Accents { iris, lift, blush: blush(&eyes, planes, palette, &presence) }
}

/// The planted frames through the develop's camera. An eye the near plane
/// has eaten drops out entirely, which is the same thing that happens to
/// its ink.
pub fn project(frames: &[EyeFrame], view_proj: &Mat4, width: usize, height: usize) -> Vec<Eye> {
    let on_canvas = |p| regions::on_canvas(view_proj, p, width, height);

    frames
        .iter()
        .enumerate()
        .filter_map(|(index, frame)| {
            let centre = on_canvas(frame.centre)?;

            Some(Eye {
                frame: index,
                centre,
                width: on_canvas(frame.width_tip)? - centre,
                height: on_canvas(frame.height_tip)? - centre,
                pupil: Vec2::new(frame.pupil.x.max(PUPIL_FLOOR), frame.pupil.y.max(PUPIL_FLOOR)),
                aperture: frame.aperture.iter().filter_map(|&p| on_canvas(p)).collect(),
            })
        })
        .collect()
}

/// The iris coverage and the lid weight over it.
fn irises(eyes: &[Eye], planes: &Planes<'_>) -> (Vec<f32>, Vec<f32>) {
    let (width, height) = (planes.width, planes.height);
    let clip = clip_mask(eyes, planes);
    let (mut iris, mut lift) = (vec![0.0f32; width * height], vec![1.0f32; width * height]);

    for eye in eyes {
        let Some((across, down)) = eye.inverse() else {
            continue;
        };
        let reach = eye.reach(height);

        for y in span(eye.centre.y, reach, height) {
            for x in span(eye.centre.x, reach, width) {
                let i = y * width + x;
                let held = image::smoothstep(CLIP_EDGE.0, CLIP_EDGE.1, clip[i]);
                if held < CLIP_FLOOR {
                    continue;
                }

                let offset = Vec2::new(x as f32 + 0.5, y as f32 + 0.5) - eye.centre;
                let at = Vec2::new(across.dot(offset), down.dot(offset));
                let within = at.length();
                if within >= IRIS_RIM.0 {
                    continue;
                }

                let slit = Vec2::new(at.x / eye.pupil.x, at.y / eye.pupil.y).length();
                let coverage = held
                    * image::smoothstep(IRIS_RIM.0, IRIS_RIM.1, within)
                    * image::smoothstep(SLIT_RIM.0, SLIT_RIM.1, slit);

                iris[i] = iris[i].max(coverage);
                lift[i] = 1.0 + LIFT * image::smoothstep(LIFT_BAND.0, LIFT_BAND.1, at.y);
            }
        }
    }

    (iris, lift)
}

/// The aperture loops filled and softened — the only clip a painted eye
/// accent may use.
///
/// The chart's own lid curves, never the label volume the anchor was
/// measured from: the kitsune archetype opens a fifth wider than that
/// class, so paint clipped to the label lands visibly inside the ink that
/// drew the same eye.
fn clip_mask(eyes: &[Eye], planes: &Planes<'_>) -> Vec<f32> {
    let (width, height) = (planes.width, planes.height);
    let mut mask = vec![0.0; width * height];

    for eye in eyes.iter().filter(|eye| eye.aperture.len() >= 3) {
        fill(&mut mask, &eye.aperture, width, height);
    }

    image::blur(&mask, width, height, image::tuned(CLIP_BLUR, height))
}

/// Fill a closed loop, one scanline at a time.
///
/// Even-odd by crossing count, which is all an aperture needs: it is a
/// simple loop — up one lid and back along the other — so every scanline
/// enters it once and leaves it once.
fn fill(mask: &mut [f32], outline: &[Vec2], width: usize, height: usize) {
    let (top, bottom) = outline.iter().fold((f32::MAX, f32::MIN), |(t, b), p| (t.min(p.y), b.max(p.y)));
    let (middle, half) = ((top + bottom) * 0.5, (bottom - top) * 0.5);
    let mut crossings: Vec<f32> = Vec::new();

    for y in span(middle, half, height) {
        let scan = y as f32 + 0.5;
        crossings.clear();
        for (from, to) in outline.iter().zip(outline.iter().cycle().skip(1)).take(outline.len()) {
            if (from.y > scan) != (to.y > scan) {
                crossings.push(from.x + (scan - from.y) / (to.y - from.y) * (to.x - from.x));
            }
        }
        crossings.sort_unstable_by(f32::total_cmp);

        for pair in crossings.chunks_exact(2) {
            let (first, last) = (pair[0].ceil().max(0.0) as usize, pair[1].floor().max(0.0) as usize);
            for x in first..=last.min(width.saturating_sub(1)) {
                mask[y * width + x] = 1.0;
            }
        }
    }
}

/// The rows or columns within `reach` of `centre`, clamped to the canvas.
///
/// Inclusive at both ends: a pixel whose centre lies inside the reach has
/// to be visited, and rounding the first bound inward drops the row a
/// polygon's own top edge falls in.
fn span(centre: f32, reach: f32, extent: usize) -> RangeInclusive<usize> {
    let first = (centre - reach).floor().max(0.0) as usize;
    let last = ((centre + reach).ceil().max(0.0) as usize).min(extent.saturating_sub(1));

    first..=last
}

/// Where the eyes agree the face's middle is.
///
/// Outward is decided against this rather than against her own left and
/// right, so it stays outward under a camera that has swapped which of
/// the two is nearer.
#[must_use]
pub fn midline(eyes: &[Eye]) -> f32 {
    if eyes.is_empty() {
        return 0.0;
    }

    eyes.iter().map(|eye| eye.centre.x).sum::<f32>() / eyes.len() as f32
}

/// Where one eye's cheek apple sits and how far it reaches — the single
/// statement of the placement, read by the CPU flush and by the GPU one.
#[must_use]
pub fn apple_of(eye: &Eye, midline: f32) -> (Vec2, Vec2) {
    let size = eye.size();
    let outward = if eye.centre.x < midline {
        -1.0
    } else {
        1.0
    };

    (
        eye.centre + Vec2::new(outward * size * APPLE.0, size * APPLE.1),
        Vec2::new(size * APPLE_RADII.0, size * APPLE_RADII.1),
    )
}

/// How much blush each eye has earned, counted off the label plane: the
/// visible fraction of its own aperture area, through the presence ramp.
///
/// Split out so both develops read one set of numbers. The GPU develop
/// has no label plane to count — the class plane lives on the GPU now —
/// so it measures the same question by casting the aperture against the
/// subject instead (see [`Easel`](crate::easel::Easel)); the parity
/// scenarios feed it these, so the two are held together on the accent
/// placement rather than on the presence policy.
#[must_use]
pub fn presences(eyes: &[Eye], planes: &Planes<'_>, palette: &Palette) -> Vec<f32> {
    let Some(eye_class) = palette.class_named(EYE_CLASS) else {
        return vec![0.0; eyes.len()];
    };
    let seen = palette.mask_of(planes.classes, eye_class);

    eyes.iter()
        .map(|eye| {
            let size = eye.size();

            image::smoothstep(PRESENCE.0, PRESENCE.1, visible(eye, &seen, planes) / (PI * size * size))
        })
        .collect()
}

/// The cheek flush, hung off both eye frames.
fn blush(eyes: &[Eye], planes: &Planes<'_>, palette: &Palette, presence: &[f32]) -> Vec<f32> {
    let (width, height) = (planes.width, planes.height);
    let mut flush = vec![0.0; width * height];
    let Some(skin_class) = palette.class_named(SKIN_CLASS) else {
        return flush;
    };
    if eyes.is_empty() {
        return flush;
    }

    let midline = midline(eyes);

    for (eye, &presence) in eyes.iter().zip(presence) {
        if presence <= 0.0 {
            continue;
        }
        let (apple, radii) = apple_of(eye, midline);

        for y in span(apple.y, radii.y, height) {
            for x in span(apple.x, radii.x, width) {
                let offset = Vec2::new((x as f32 - apple.x) / radii.x, (y as f32 - apple.y) / radii.y);
                let fall = (1.0 - offset.length_squared()).max(0.0);

                flush[y * width + x] += fall * fall * presence;
            }
        }
    }

    let skin =
        image::blur(&palette.mask_of(planes.classes, skin_class), width, height, image::tuned(SKIN_BLUR, height));
    for ((at, &under), &facing) in flush.iter_mut().zip(&skin).zip(planes.facing) {
        *at *= under * image::smoothstep(FACING.0, FACING.1, facing);
    }

    image::blur(&flush, width, height, image::tuned(FLUSH_BLUR, height)).iter().map(|&at| at * FLUSH_STRENGTH).collect()
}

/// How much of one eye's aperture the bake actually drew, in pixels.
///
/// Counted off the label plane rather than off the clip: what is being
/// asked is whether the *surface* is in view — the fringe hanging over the
/// far eye is the case this exists for — and the clip is a chart polygon
/// that knows nothing about what stands in front of it.
fn visible(eye: &Eye, seen: &[f32], planes: &Planes<'_>) -> f32 {
    let size = eye.size();
    let mut count = 0.0;

    for y in span(eye.centre.y, size * PRESENCE_SPAN.1, planes.height) {
        for x in span(eye.centre.x, size * PRESENCE_SPAN.0, planes.width) {
            if seen[y * planes.width + x] > 0.5 {
                count += 1.0;
            }
        }
    }

    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_math::Vec3;

    use crate::labels::{EYE, SKIN};

    /// A canvas big enough to hold an eye at the scale the accents were
    /// tuned for, and small enough to paint in a test.
    const WIDE: usize = 120;
    const TALL: usize = 160;

    /// The iris radius every fixture eye is drawn at, and the aperture
    /// around it: wide, domed above, and shallower below.
    ///
    /// The proportions are the archetype's own, and the shallow floor is
    /// the load-bearing one — the iris runs past the lower lid and stops
    /// short of the upper, so a clip that has stopped working shows on one
    /// side of the same ellipse and not the other.
    const RADIUS: f32 = 0.1;
    const APERTURE: Vec2 = Vec2::new(0.16, 0.13);
    const FLOOR: f32 = 0.45;

    /// A frame straight in front of an orthographic camera, so the canvas
    /// coordinates a test names are the world ones scaled.
    fn frame(x: f32) -> EyeFrame {
        let centre = Vec3::new(x, 0.0, 0.0);
        let lid = |i: usize, rise: f32| {
            let across = -1.0 + 2.0 * i as f32 / 23.0;

            Vec3::new(x + APERTURE.x * across, (1.0 - across * across) * APERTURE.y * rise, 0.0)
        };

        EyeFrame {
            centre,
            width_tip: centre + Vec3::new(RADIUS, 0.0, 0.0),
            height_tip: centre + Vec3::new(0.0, RADIUS, 0.0),
            pupil: Vec2::new(0.26, 0.90),
            aperture: (0..24).map(|i| lid(i, 1.0)).chain((0..24).rev().map(|i| lid(i, -FLOOR))).collect(),
        }
    }

    /// Orthographic down -z over a unit square, so a world point maps to
    /// the canvas by a rule a test can invert.
    fn camera() -> Mat4 {
        Mat4::orthographic_rh(-0.5, 0.5, -0.5, 0.5, 1.0, 10.0)
            * Mat4::look_at_rh(Vec3::new(0.0, 0.0, 5.0), Vec3::ZERO, Vec3::Y)
    }

    /// Where the fixture's two eyes sit, in world x.
    const EYES: [f32; 2] = [-0.2, 0.2];

    /// Skin everywhere, confronting the viewer, with an eye class over
    /// each aperture — a face as far as the accents can tell.
    ///
    /// The patch is sized so each eye reads about one and a half times its
    /// own iris area, which is what the subject measures when the eye is
    /// fully in view.
    fn planes() -> (Vec<u8>, Vec<f32>, Vec<f32>) {
        let mut classes = vec![SKIN; WIDE * TALL];
        for eye in EYES {
            let centre = ((eye + 0.5) * WIDE as f32) as usize;
            for y in (TALL / 2 - 15)..(TALL / 2 + 15) {
                for x in (centre - 20)..(centre + 20) {
                    classes[y * WIDE + x] = EYE;
                }
            }
        }

        (classes, vec![0.5; WIDE * TALL], vec![1.0; WIDE * TALL])
    }

    fn painted(frames: &[EyeFrame]) -> Accents {
        let (classes, tone, facing) = planes();
        let planes = Planes { classes: &classes, tone: &tone, facing: &facing, width: WIDE, height: TALL };

        paint(frames, &camera(), &planes, &Palette::canonical())
    }

    /// The canvas pixel a world point lands on under `camera`.
    fn at(x: f32, y: f32) -> usize {
        let (px, py) = ((x + 0.5) * WIDE as f32, (0.5 - y) * TALL as f32);

        py as usize * WIDE + px as usize
    }

    /// Tripwire: the pupil is a reserve cut out of the iris, not a shape
    /// painted over it.
    ///
    /// The slit ramp runs the opposite way to the rim's, and a sign slip
    /// there produces a picture that still looks like a painted eye — an
    /// even disc of blue with no slit in it, or a slit and nothing around
    /// it. Both read as a decal rather than as an iris under a lid, and
    /// neither errors.
    #[test]
    fn the_slit_is_bare_paper_inside_a_painted_iris() {
        let accents = painted(&[frame(0.0)]);

        assert!(accents.iris[at(0.0, 0.0)] < 0.05, "the pupil takes no pigment");
        assert!(accents.iris[at(0.06, 0.0)] > 0.5, "the iris beside it does");
        assert!(accents.iris[at(0.135, 0.0)] < 0.05, "and the sclera past the rim does not");
    }

    /// Tripwire: paint stops where the ink's own lids do, not where its own
    /// ellipse ends.
    ///
    /// The pair is the whole test: two points at the same distance from the
    /// iris centre, one under the domed upper lid and one past the shallow
    /// lower one. A clip taken from the label volume — or dropped — paints
    /// both, and the wash spills onto her cheek as the sticker eye this
    /// exists to prevent.
    #[test]
    fn the_iris_is_held_inside_the_ink_s_own_aperture() {
        let accents = painted(&[frame(0.0)]);
        let (across, along) = (0.05, 0.085);

        assert!(accents.iris[at(across, along)] > 0.5, "under the upper lid the iris paints");
        assert!(accents.iris[at(across, -along)] < 0.05, "past the lower lid the same ellipse does not");
    }

    /// Tripwire: the blush hangs outward of its own eye, and never across
    /// the midline.
    ///
    /// Hung straight below the eye it crosses the nose at three-quarters,
    /// which was the user-caught bug this offset exists for. Measured on
    /// the canvas rather than on her own left and right, so it survives a
    /// camera that has swapped which eye is nearer.
    #[test]
    fn each_blush_sits_outward_of_its_eye_and_off_the_midline() {
        let accents = painted(&EYES.map(frame));
        let (apple, cheek) = (0.35, -0.2);
        let (right, left) = (accents.blush[at(-apple, cheek)], accents.blush[at(apple, cheek)]);
        let nose = accents.blush[at(0.0, cheek)];

        assert!(right > 0.0, "her right cheek carries a blush");
        assert!(left > 0.0, "so does her left");
        // Not zero: the softening blur leaves a rounding tail everywhere.
        // What the midline must not carry is any of the flush the eye can
        // find, and the two are orders of magnitude apart.
        assert!(nose < right * 0.01, "the nose between them carries none, got {nose} against {right}");
    }

    /// Tripwire: an eye the bake never drew gives its cheek no blush.
    ///
    /// The far eye at three-quarters is hidden by the fringe while its
    /// chart frame still plants and still projects — nothing upstream
    /// knows it is gone. Without the presence gate its cheek blushes at
    /// full strength off the back of her head.
    #[test]
    fn a_hidden_eye_gives_its_cheek_no_blush() {
        let (classes, tone, facing) = (vec![SKIN; WIDE * TALL], vec![0.5; WIDE * TALL], vec![1.0; WIDE * TALL]);
        let planes = Planes { classes: &classes, tone: &tone, facing: &facing, width: WIDE, height: TALL };

        let accents = paint(&EYES.map(frame), &camera(), &planes, &Palette::canonical());
        assert!(accents.blush.iter().all(|&at| at == 0.0), "an eye absent from the bake blushes nowhere");
    }
}
