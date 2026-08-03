//! The mouth: the viseme table and the shape functions from the source
//! spike, unchanged.
//!
//! What differs is the output. The spike emits filled layers for a shaded
//! render — interior, teeth, tongue — and a pen has no fills, so tone
//! becomes hatch density and the light regions become bare paper.

use aether_math::Vec2;

use crate::anchor::Anchor;
use crate::feature::{FeatureClass, Pen};

use super::{Mark, STANDOFF_MOUTH};

/// Where the upper lip sits in the aperture's height, and the tongue's
/// reach and lift as fractions of the aperture.
const LIP: f32 = 0.44;
const TONGUE: (f32, f32) = (0.52, 0.30);

/// Samples across the aperture. The lip line is the longest smooth curve on
/// the face, so it is the one that shows faceting first.
const SAMPLES: usize = 80;

/// Spacing of the shadow hatch and how far across the aperture it runs,
/// both as fractions of the half-width — so the shade holds its density and
/// its inset whatever the mouth is doing.
const SHADE_SPACING: f32 = 0.085;
const SHADE_REACH: f32 = 0.92;

/// The rest mouth, closed and very slightly turned up. Compiled into
/// [`Face::REST`], so it is what every subject wears until a control
/// surface says otherwise.
///
/// [`Face::REST`]: super::Face::REST
pub const REST: [f32; 5] = [0.00, 0.74, 0.08, 0.00, 0.00];

/// `(openness, width, corner, teeth, skew)`.
///
/// The first four are the source spike's, unchanged — the five vowels plus
/// closed. `skew` is new and is the one thing the sculpt could never have
/// given us: the spike's corner term is even in `u`, so it lifts both
/// corners together and can only smile or frown. An odd term lifts one
/// corner and drops the other, which is a smirk.
pub const VISEMES: [(&str, [f32; 5]); 12] = [
    // Speech, skew zero.
    ("closed", [0.00, 0.74, 0.10, 0.00, 0.00]),
    ("A", [0.95, 0.88, 0.10, 0.30, 0.00]),
    ("I", [0.38, 0.98, 0.30, 0.75, 0.00]),
    ("U", [0.62, 0.52, -0.05, 0.00, 0.00]),
    ("E", [0.62, 0.92, 0.16, 0.55, 0.00]),
    ("O", [0.92, 0.58, 0.00, 0.10, 0.00]),
    // Expression.
    ("rest", REST),
    ("smile", [0.06, 0.88, 0.46, 0.00, 0.00]),
    ("grin", [0.34, 0.96, 0.52, 0.85, 0.00]),
    ("frown", [0.00, 0.70, -0.34, 0.00, 0.00]),
    ("smirk", [0.00, 0.72, 0.06, 0.00, 0.52]),
    ("pout", [0.10, 0.54, -0.18, 0.00, 0.00]),
];

pub fn shape(name: &str) -> Option<[f32; 5]> {
    VISEMES.iter().find(|(known, _)| *known == name).map(|&(_, shape)| shape)
}

/// Whether this shape opens an aperture, as opposed to being a lip line.
///
/// The table makes the handover clean: `closed` is 0.0 and every vowel is
/// at least 0.38, so nothing has to be blended across.
pub fn open([openness, ..]: [f32; 5]) -> bool {
    openness > 0.02
}

/// The mouth for one shape, as pen marks.
pub fn draw(anchor: &Anchor, shape: [f32; 5]) -> Vec<Mark> {
    let [_openness, _width, corner, teeth, skew] = shape;
    let aperture = Aperture::new(anchor, shape);

    let mut marks = if open(shape) {
        let mut open_marks = vec![aperture.rim()];
        open_marks.extend(aperture.shadow(corner, teeth, skew));
        open_marks.push(aperture.tongue());
        open_marks
    } else {
        aperture.closed_lips(corner, skew)
    };
    marks.push(aperture.upper_line());

    marks
}

struct Aperture {
    centre: Vec2,
    x: Vec<f32>,
    upper: Vec<f32>,
    lower: Vec<f32>,
    half_width: f32,
    height: f32,
}

impl Aperture {
    fn new(anchor: &Anchor, [openness, width, corner, _teeth, skew]: [f32; 5]) -> Self {
        let half_width = width * anchor.half.x;
        let height = openness * half_width * 0.62;

        let (mut x, mut upper, mut lower) = (Vec::new(), Vec::new(), Vec::new());
        for i in 0..SAMPLES {
            let u = -1.0 + 2.0 * i as f32 / (SAMPLES - 1) as f32;
            let (top, bottom) = edges(anchor.centre, half_width, height, corner, skew, u);

            x.push(anchor.centre.x + u * half_width);
            upper.push(top);
            lower.push(bottom);
        }

        Self { centre: anchor.centre, x, upper, lower, half_width, height }
    }

    /// The aperture edge. The source spike fills this; a pen outlines it,
    /// and leaves the teeth as bare paper — which is what they are.
    fn rim(&self) -> Mark {
        let mut points: Vec<Vec2> = (0..self.x.len()).map(|i| Vec2::new(self.x[i], self.upper[i])).collect();
        points.extend((0..self.x.len()).rev().map(|i| Vec2::new(self.x[i], self.lower[i])));
        points.push(points[0]);

        Mark {
            weights: vec![0.7; points.len()],
            points,
            pen: Pen::Ink,
            class: FeatureClass::Decal,
            standoff: STANDOFF_MOUTH,
        }
    }

    /// The dark gap under the teeth is the one place the drawing wants
    /// solid tone, so it gets hatched. Vertical, because that is how the
    /// shadow under an upper lip actually reads.
    fn shadow(&self, corner: f32, teeth: f32, skew: f32) -> Vec<Mark> {
        let spacing = (self.half_width * SHADE_SPACING).max(1e-4);
        let reach = self.half_width * SHADE_REACH;
        let steps = (2.0 * reach / spacing).ceil().max(0.0) as usize;

        (0..steps)
            .filter_map(|step| {
                let at = -reach + spacing * step as f32;
                let (top, bottom) =
                    edges(self.centre, self.half_width, self.height, corner, skew, at / self.half_width);
                let shade_top = top - teeth * (top - bottom);

                (shade_top - bottom > self.height * 0.06).then(|| {
                    let x = self.centre.x + at;

                    Mark {
                        points: vec![Vec2::new(x, bottom + self.height * 0.04), Vec2::new(x, shade_top)],
                        weights: vec![0.85; 2],
                        pen: Pen::Ink,
                        // Not `Hatch`: hatch density is derived from
                        // lighting and gets tone-gated, but the dark under a
                        // lip is authored by the chart and must not be
                        // second-guessed by a lamp.
                        class: FeatureClass::Decal,
                        standoff: STANDOFF_MOUTH,
                    }
                })
            })
            .collect()
    }

    /// An outline only. The tongue is a mid tone, and mid tones are the
    /// paper's job.
    fn tongue(&self) -> Mark {
        let (reach, lift) = TONGUE;
        let points: Vec<Vec2> = (0..48)
            .map(|i| {
                let u = -1.0 + 2.0 * i as f32 / 47.0;
                let x = self.centre.x + u * self.half_width * reach;
                let base = interpolate(&self.x, &self.lower, x);

                Vec2::new(x, base + self.height * lift * 2.0 * (1.0 - u.abs().powf(1.7)))
            })
            .collect();

        Mark {
            weights: vec![0.55; points.len()],
            points,
            pen: Pen::Pale,
            class: FeatureClass::Decal,
            standoff: STANDOFF_MOUTH,
        }
    }

    /// A closed mouth, in the anime grammar rather than a rendered one.
    ///
    /// Three marks, all lines: a cupid's bow dipped into the upper edge, a
    /// short detached shadow under the lower lip, and a tick at each
    /// corner. The convention weights the top and leaves the bottom
    /// implied — draw the lower lip as a full outline and she reads as
    /// wearing lipstick.
    fn closed_lips(&self, corner: f32, skew: f32) -> Vec<Mark> {
        let bow = 0.16 * self.half_width;
        let dip: Vec<Vec2> = (0..24)
            .map(|i| {
                let u = -1.0 + 2.0 * i as f32 / 23.0;
                let x = self.centre.x + u * bow;
                // Two peaks either side of a central notch.
                let y = self.centre.y + bow * 0.30 * (1.0 - 2.2 * u * u) * (1.0 - u * u);

                Vec2::new(x, y + skew * 0.30 * u * bow)
            })
            .collect();

        let shade: Vec<Vec2> = (0..16)
            .map(|i| {
                let u = -1.0 + 2.0 * i as f32 / 15.0;
                let x = self.centre.x + u * self.half_width * 0.44;

                Vec2::new(x, self.centre.y - self.half_width * (0.26 - 0.06 * u * u))
            })
            .collect();

        let mut marks = vec![
            Mark {
                weights: vec![0.62; dip.len()],
                points: dip,
                pen: Pen::Ink,
                class: FeatureClass::Decal,
                standoff: STANDOFF_MOUTH,
            },
            Mark {
                weights: vec![0.45; shade.len()],
                points: shade,
                pen: Pen::Pale,
                class: FeatureClass::Decal,
                standoff: STANDOFF_MOUTH,
            },
        ];

        marks.extend([-1.0f32, 1.0].map(|side| {
            let x = self.centre.x + side * self.half_width * 0.94;
            let lift = (corner + skew * side) * 0.30 * self.half_width;

            Mark {
                points: vec![
                    Vec2::new(x, self.centre.y + lift),
                    Vec2::new(x + side * self.half_width * 0.10, self.centre.y + lift + bow * 0.22),
                ],
                weights: vec![0.9; 2],
                pen: Pen::Ink,
                class: FeatureClass::Decal,
                standoff: STANDOFF_MOUTH,
            }
        }));

        marks
    }

    /// The upper line, and the source spike's note is the important part:
    /// anime weights the top edge and leaves the bottom to a thin shadow,
    /// because outlining the aperture evenly makes a mouth read as a hole
    /// cut in a mask. Its variable weight is the chart's own band profile,
    /// which maps onto a stroke's per-point width with nothing lost.
    fn upper_line(&self) -> Mark {
        const KNOTS: [(f32, f32); 3] = [(0.0, 0.16), (0.7, 0.13), (1.0, 0.04)];

        let (points, weights) = (0..self.x.len())
            .map(|i| {
                let u = -1.0 + 2.0 * i as f32 / (self.x.len() - 1) as f32;
                let hook = 0.26 * self.half_width * ((u.abs() - 0.72) / 0.28).clamp(0.0, 1.0).powi(2);

                (Vec2::new(self.x[i], self.upper[i] - hook), band(&KNOTS, u.abs()) / KNOTS[0].1)
            })
            .unzip();

        Mark { points, weights, pen: Pen::Ink, class: FeatureClass::Silhouette, standoff: STANDOFF_MOUTH }
    }
}

/// The two lip edges at `u`, which runs `-1` at one corner to `+1` at the
/// other. One function because the rim and the shadow hatch have to agree
/// exactly on where the aperture is; two copies of it disagreed by a
/// rounding error and the hatch poked through the outline.
fn edges(centre: Vec2, half_width: f32, height: f32, corner: f32, skew: f32, u: f32) -> (f32, f32) {
    let lip = 1.0 - u.abs().powf(1.7);
    // Even in `u` lifts both corners; odd lifts one and drops the other.
    let rise = (corner * u * u + skew * u) * 0.30 * half_width;

    (centre.y + height * lip * LIP + rise, centre.y - height * lip * (1.0 - LIP) * 1.9 + rise)
}

/// Piecewise-linear profile through `(position, value)` knots — the source
/// spike's band profile, which is what gives the lip line its weight taper.
fn band(knots: &[(f32, f32)], at: f32) -> f32 {
    for pair in knots.windows(2) {
        let ((x0, y0), (x1, y1)) = (pair[0], pair[1]);
        if at <= x1 {
            return y0 + (y1 - y0) * ((at - x0) / (x1 - x0)).clamp(0.0, 1.0);
        }
    }

    knots.last().map_or(0.0, |&(_, y)| y)
}

fn interpolate(xs: &[f32], ys: &[f32], at: f32) -> f32 {
    for i in 1..xs.len() {
        if at <= xs[i] {
            return ys[i - 1] + (ys[i] - ys[i - 1]) * ((at - xs[i - 1]) / (xs[i] - xs[i - 1])).clamp(0.0, 1.0);
        }
    }

    *ys.last().unwrap_or(&0.0)
}
