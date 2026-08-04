//! Stage one: which lines would an illustrator draw on this surface?
//!
//! Two feature kinds, both of them level sets of a per-vertex scalar, so
//! both come out of the same machinery:
//!
//! - **Silhouette** — the zero set of `view . normal`. Where the surface
//!   turns away from the eye.
//! - **Hatch** — the level sets of `position . axis`, three families at
//!   different angles, switching on as the tone darkens.
//!
//! Nothing here consults visibility. Extraction says what exists; the
//! next pass says what survives.

use aether_math::Vec3;

use crate::anchor::{Anchor, Anchors};
use crate::chart;
use crate::feature::{Curve3, FeatureClass, Pen, SurfacePoint};
use crate::labels::{self, Labels};
use crate::math3::noise;
use crate::mesh::{Crossing, Mesh};
use crate::weld;

use core::mem;

/// Which side of a carved feature gets a line.
///
/// Relief is positive in a valley and negative on the ridge either side,
/// so drawing both level sets outlines every feature twice. On a large
/// feature that reads as a lip line plus a lip edge; on a small one the
/// two contours collapse into a blob a few pixels wide. Valley alone is
/// one line per crease, which is what an inker draws.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CreaseSides {
    Valley,
    Ridge,
    Both,
}

impl CreaseSides {
    fn isos(self, threshold: f32) -> Vec<(f32, u64)> {
        match self {
            Self::Valley => vec![(threshold, 0)],
            Self::Ridge => vec![(-threshold, 1)],
            Self::Both => vec![(threshold, 0), (-threshold, 1)],
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "valley" => Some(Self::Valley),
            "ridge" => Some(Self::Ridge),
            "both" => Some(Self::Both),
            _ => None,
        }
    }
}

#[derive(Clone)]
pub struct Settings {
    /// World distance between hatch lines, in model units.
    pub hatch_spacing: f32,
    /// Tone below which each successive hatch family switches on.
    pub hatch_thresholds: [f32; 3],
    /// Angle of the primary hatch family, in the model's XY plane.
    pub hatch_tilt: f32,
    /// Direction the key light arrives from.
    pub light: Vec3,
    /// Floor of the shading term, so nothing reads as pure black.
    pub ambient: f32,
    /// How much the face is lifted out of the hatching. The drawn face is
    /// authored, and hatch crossing it competes with the marks rather than
    /// describing anything — the same call the source pipeline makes by
    /// giving each material its own light band.
    pub face_lift: f32,
    /// Umbrella passes over the vertex normals before extraction. A
    /// reconstruction's normals are noisy at the triangle scale, and the
    /// silhouette is their zero set, so a couple of passes is the
    /// difference between one line and a frayed one.
    pub relaxation: usize,
    /// Smoothing scales for the relief band-pass, in umbrella passes.
    /// The gap between them selects the size of detail that gets a line.
    pub relief_fine: usize,
    pub relief_coarse: usize,
    /// How deep a crease has to be to earn a line, in mean edge lengths.
    pub relief_threshold: f32,
    /// How steeply the relief must change for a crossing to count as a
    /// carved line. With a class mask in play this only has to reject
    /// outright noise — the mask is the real discriminator, and leaving
    /// this high cuts the lid and lip lines, which are shallow features on
    /// a small part of the surface.
    pub crease_steepness: f32,
    /// Which side of each carved feature gets a line.
    pub crease_sides: CreaseSides,
    /// What her face is doing. `None` hands the eyes, brows and lips back
    /// to the sculpt, which is honest but nearly blank — the eye carries no
    /// relief at all and the lips clear the crease threshold 12% of the
    /// time.
    ///
    /// The state a control surface addresses (iamacoffeepot/aether#4338);
    /// until then every subject wears the rest pose.
    pub face: Option<chart::Face>,
    /// Which eye archetype the chart draws. Shape only — expression and
    /// gaze ride on top of whichever one is chosen.
    pub eye_style: chart::Style,
    /// Which mark, if any, stands in for the nose. `None` leaves the nose
    /// to the sculpt, which can draw its own given a threshold it can
    /// actually reach.
    pub nose: chart::Nose,
    /// Relief threshold for `skin`, applied only inside the nose window.
    ///
    /// One global threshold cannot serve both an eyelid and a nose: a lid
    /// is carved three times as deep, so a number set for lids buries the
    /// nose, and a number set for the nose brings every swell on her cheeks
    /// with it. Zero switches the pass off.
    pub nose_relief: f32,
    /// Curvature a suggestive contour must reach to be drawn. Zero is off.
    pub suggestive: f32,
    /// How near the turn a suggestive contour has to be, as `n . v`.
    pub suggestive_gate: f32,
    /// How far the tick's lower end kicks away from the midline.
    pub nose_bend: f32,
    /// Which material classes get their creases inked.
    ///
    /// Everything except `hair`, `skin` and `dress`. Hair is the class that
    /// forced the mask in the first place — its strand seams are carved as
    /// deeply as an eyelid and there are five times as many of them, so
    /// inking them buries the face. Skin is excluded because its creases
    /// are broad shallow swells that close into blotches, and because the
    /// one feature worth having there is the nose, which spike 145 already
    /// concluded belongs to the light rather than to a line.
    pub crease_classes: Vec<u8>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            hatch_spacing: 0.020,
            hatch_thresholds: [0.62, 0.44, 0.30],
            hatch_tilt: 0.62,
            light: Vec3::new(-0.46, 0.52, 0.78),
            ambient: 0.28,
            face_lift: 0.60,
            relaxation: 2,
            relief_fine: 2,
            relief_coarse: 16,
            relief_threshold: 0.36,
            crease_steepness: 0.10,
            crease_sides: CreaseSides::Valley,
            face: Some(chart::Face::REST),
            eye_style: chart::Style::default(),
            nose: chart::Nose::Tick,
            // Both off. Either can find her nose, and both draw it as a
            // contour — which is the right answer to a different question.
            // A single bar says nose without claiming to describe one, and
            // it gets out of the way as soon as the profile can.
            nose_relief: 0.0,
            suggestive: 0.0,
            suggestive_gate: 0.60,
            // Straight. The curve made it a nose and then kept going —
            // past about 0.4 it reads as a contour and starts competing
            // with the profile for the same job.
            nose_bend: 0.0,
            crease_classes: vec![labels::EYE, labels::BROW, labels::LIPS, labels::INNER_EAR, labels::TUFT],
        }
    }
}

impl Settings {
    /// Three plane normals: a primary diagonal, its perpendicular, and one
    /// splitting the pair. Hatch lines run across these.
    fn hatch_axes(&self) -> [Vec3; 3] {
        let (sin, cos) = self.hatch_tilt.sin_cos();
        let (primary, cross) = (Vec3::new(cos, sin, 0.0), Vec3::new(-sin, cos, 0.0));

        [primary, cross, (primary + cross).normalize()]
    }

    /// Lighting term at a point: `0` in shadow, `1` fully lit.
    pub fn tone(&self, point: &SurfacePoint) -> f32 {
        let lambert = point.normal.dot(self.light.normalize()).max(0.0);
        self.ambient + (1.0 - self.ambient) * lambert + self.face_lift * face_weight(point.pos)
    }
}

/// How much of the face-lift applies at a point: full across the front of
/// the face, falling off before it reaches the jaw or the hair.
fn face_weight(p: Vec3) -> f32 {
    // Centred on her face, which the label classes put at eyes y 0.37 and
    // lips y 0.20 — not at y 0.50, where the midline depth profile's
    // brow-ridge maximum misled me into centring it, so the lift was
    // protecting her forehead while hatch ran across her cheeks.
    let horizontal = 1.0 - (p.x.abs() / 0.26).min(1.0);
    let vertical = 1.0 - ((p.y - 0.30).abs() / 0.24).min(1.0);
    let frontal = ((p.z - 0.16) / 0.12).clamp(0.0, 1.0);

    (horizontal * vertical * frontal).powf(0.40)
}

fn to_points(segments: Vec<[Crossing; 2]>) -> Vec<[SurfacePoint; 2]> {
    segments
        .into_iter()
        .map(|[a, b]| [SurfacePoint::on_surface(a.pos, a.normal), SurfacePoint::on_surface(b.pos, b.normal)])
        .collect()
}

pub fn silhouettes(mesh: &Mesh, eye: Vec3) -> Vec<Curve3> {
    let template =
        Curve3 { points: Vec::new(), class: FeatureClass::Silhouette, pen: Pen::Ink, seed: 0, authored: false };

    weld::curves(to_points(mesh.level_set(&mesh.facing(eye), &[], 0.0)), &template)
}

/// Hatching as three crossing families of world-space plane cuts.
///
/// Holding the planes in *world* space rather than deriving them from the
/// camera is what keeps the hatch welded to the figure: orbit the camera
/// and the strokes stay on the surface they belong to instead of sliding
/// across it. Each family shares one seed per plane, so a line that
/// crosses the whole figure wobbles as one line.
pub fn hatching(mesh: &Mesh, settings: &Settings) -> Vec<Curve3> {
    let mut out = Vec::new();

    for (family, axis) in settings.hatch_axes().into_iter().enumerate() {
        // Each successive family is sparser than the last. Equal spacing
        // would make the cross-hatched regions four times the density of
        // the single-hatched ones and the tone ramp would break at the
        // first threshold instead of climbing through it.
        let spacing = settings.hatch_spacing * [1.0, 1.45, 1.95][family];

        for (plane, segments) in mesh.level_sets(&mesh.projected(axis), spacing) {
            let template = Curve3 {
                points: Vec::new(),
                class: FeatureClass::Hatch { level: family as u8 },
                pen: Pen::Pale,
                seed: u64::from(family as u32) << 32 | u64::from(plane.unsigned_abs()),
                authored: false,
            };

            out.extend(weld::curves(to_points(segments), &template));
        }
    }

    out
}

/// Crease lines: the level sets of the surface relief.
///
/// Everything the sculptor carved that is not big enough to change the
/// silhouette lives here — the eyelid fold, the lash shelf, the lip line,
/// the seam between hair strands. Drawing both a positive and a negative
/// level set gives valleys and ridges their own lines, which is what an
/// inker does: a fold reads as one line where it turns over, not as a
/// filled band.
pub fn creases(mesh: &Mesh, labels: Option<&Labels>, settings: &Settings) -> Vec<Curve3> {
    // Whichever of the two is drawing a feature has to draw all of it: a
    // charted lid over a sculpted one is two lids a few pixels apart, which
    // is the blob the crease-sides setting was added to stop. So the
    // classes the chart takes over swap out rather than stack — at rest as
    // much as in speech, because her lips are the shallowest feature on her
    // face and the sculpt cannot supply a mouth worth drawing at any shape.
    let charted = settings.face.is_some();
    let classes: Vec<u8> = settings
        .crease_classes
        .iter()
        .copied()
        .filter(|&class| !(charted && matches!(class, labels::LIPS | labels::BROW | labels::EYE)))
        .collect();

    let relief = mesh.relief(settings.relief_fine, settings.relief_coarse);
    let steepness = mesh.gradient(&relief);

    settings
        .crease_sides
        .isos(settings.relief_threshold)
        .into_iter()
        .flat_map(|(iso, side)| {
            let template = Curve3 {
                points: Vec::new(),
                class: FeatureClass::Decal,
                pen: if side == 0 {
                    Pen::Ink
                } else {
                    Pen::Pale
                },
                seed: 0xc4ea_0000 | side,
                authored: false,
            };

            // Discard the shallow crossings before welding, so a blotch is
            // never assembled in the first place rather than assembled and
            // then thrown away.
            let segments: Vec<_> = mesh
                .level_set(&relief, &steepness, iso)
                .into_iter()
                .filter(|[a, b]| a.strength.max(b.strength) >= settings.crease_steepness)
                // Keep only the classes the drawing wants inked. The sculpt
                // carves a hair seam as deeply as it carves a lid, so depth
                // cannot tell them apart — but the material field already
                // knows which is which, so ask it rather than inventing a
                // geometric proxy for the same question.
                .filter(|[a, b]| labels.is_none_or(|field| field.is(a.pos, &classes) || field.is(b.pos, &classes)))
                .collect();

            weld::curves(to_points(segments), &template)
        })
        .collect()
}

/// Suggestive contours, masked to the classes that have a form worth
/// implying.
///
/// The silhouette extractor one derivative further out: the set where
/// `n . v` bottoms out along the view direction without reaching zero. That
/// is what makes it worth having rather than a placed mark — it joins the
/// profile exactly, so run the eye around and a suggestive contour walks to
/// the profile edge and becomes it.
///
/// View-dependent for the same reason, which is why it belongs on the
/// per-eye path rather than with the cached surface.
///
/// Unmasked this fires on every strand seam in her hair, which is honest
/// and unreadable — a hair seam really is about to turn away, there are
/// just hundreds of them. Skin is where the criterion earns its place: the
/// nose, the brow ridge, the turn of a cheek.
pub fn suggestive(mesh: &Mesh, labels: Option<&Labels>, eye: Vec3, settings: &Settings) -> Vec<Curve3> {
    if settings.suggestive <= 0.0 {
        return Vec::new();
    }

    let template = Curve3 {
        points: Vec::new(),
        class: FeatureClass::Silhouette,
        pen: Pen::Ink,
        seed: 0x5966_0000,
        authored: false,
    };
    let segments: Vec<_> = mesh
        .suggestive(eye, settings.suggestive, settings.suggestive_gate)
        .into_iter()
        .filter(|[a, b]| {
            labels.is_none_or(|field| field.is(a.pos, &[labels::SKIN]) && field.is(b.pos, &[labels::SKIN]))
        })
        .collect();

    weld::curves(to_points(segments), &template)
}

/// How far past the measured window the relief nose pass reaches.
///
/// The window is bounded by the eye and lip *bands*, and a nose runs past
/// both of them — the bridge climbs between the eyes and the nostrils sit
/// below the top of the lip band. The tip is found inside the window; the
/// drawing is allowed out of it.
const NOSE_REACH: (f32, f32) = (1.45, 1.85);

/// The nose, drawn from the sculpt's own relief rather than charted.
///
/// It was never missing. At the global threshold it contributes two specks;
/// drop to a threshold a nose can actually reach and the bridge, both wings
/// and the nostril curls all come out — the model's own, no authoring
/// involved. What it needs is its own number and its own window, because
/// the same threshold cannot serve a feature carved this shallow and a lid
/// carved three times deeper, and because `skin` is kept out of the crease
/// classes for the good reason that its swells blotch everywhere else on
/// her face.
///
/// View-independent, so it rides with the rest of the cached surface rather
/// than being re-solved per frame the way the charted bar is — it is the
/// carving, and the carving does not move when the eye does.
fn nose_creases(mesh: &Mesh, labels: &Labels, window: &Anchor, settings: &Settings) -> Vec<Curve3> {
    let (reach_x, reach_y) = (window.half.x * NOSE_REACH.0, window.half.y * NOSE_REACH.1);
    let inside = |p: Vec3| (p.x - window.centre.x).abs() <= reach_x && (p.y - window.centre.y).abs() <= reach_y;

    let relief = mesh.relief(settings.relief_fine, settings.relief_coarse);
    let steepness = mesh.gradient(&relief);
    let template =
        Curve3 { points: Vec::new(), class: FeatureClass::Decal, pen: Pen::Ink, seed: 0x0503_0000, authored: false };

    let segments: Vec<_> = mesh
        .level_set(&relief, &steepness, settings.nose_relief)
        .into_iter()
        .filter(|[a, b]| inside(a.pos) && inside(b.pos))
        .filter(|[a, b]| labels.is(a.pos, &[labels::SKIN]) || labels.is(b.pos, &[labels::SKIN]))
        .collect();

    weld::curves(to_points(segments), &template)
}

/// The view-independent drawing: everything that describes the surface
/// rather than the viewer.
///
/// Hatch planes are world-space and creases are a property of the carving,
/// so neither moves when the eye does — which means both are solved once at
/// load and kept. What stays per frame is what depends on the eye: the
/// silhouette, the charted face, the suggestive contours and the visibility
/// split. The offline renderer recomputes all of it every frame because it
/// has no reason not to; here that difference is most of the budget.
pub fn surface(mesh: &Mesh, labels: Option<&Labels>, anchors: Option<&Anchors>, settings: &Settings) -> Vec<Curve3> {
    // Tone-gate the hatching here, once, rather than every frame.
    //
    // View-independent, not pose-independent: it reads each point's normal,
    // and skinning changes normals. Once she can be posed this has to come
    // back onto a per-frame path over the curve points —
    // iamacoffeepot/aether#4336.
    //
    // Which hatch families survive at a point is decided by the light and
    // the point's own normal and position — not by where the eye is. So the
    // gate belongs with the other view-independent work, and leaving it on
    // the per-frame path made every redraw recompute a lambert term, a
    // face-lift falloff and a noise sample for every point of every hatch
    // curve, to reach the same verdict as the frame before.
    let mut out: Vec<Curve3> = hatching(mesh, settings)
        .into_iter()
        .flat_map(|curve| {
            let limit = match curve.class {
                FeatureClass::Hatch { level } => settings.hatch_thresholds[usize::from(level)],
                FeatureClass::Silhouette | FeatureClass::Decal => return vec![curve],
            };

            lit_runs(&curve, |point| settings.tone(point) < limit + noise(point.pos) * DITHER)
        })
        .collect();

    out.extend(creases(mesh, labels, settings));

    // The relief nose only exists when it has been given a threshold it can
    // reach, a field to be masked against, and a window to be found in.
    if settings.nose_relief > 0.0
        && let (Some(labels), Some(window)) = (labels, anchors.and_then(|at| at.nose.as_ref()))
    {
        out.extend(nose_creases(mesh, labels, window, settings));
    }

    out
}

/// Threshold dither, so a family's boundary breaks up instead of slabbing
/// into a hard edge across a flat region.
const DITHER: f32 = 0.055;

/// Split a curve into the runs whose points pass `keep`, preserving order.
fn lit_runs(curve: &Curve3, keep: impl Fn(&SurfacePoint) -> bool) -> Vec<Curve3> {
    let mut runs = Vec::new();
    let mut current: Vec<SurfacePoint> = Vec::new();

    for point in &curve.points {
        if keep(point) {
            current.push(*point);
        } else if current.len() >= 2 {
            runs.push(Curve3 { points: mem::take(&mut current), ..curve.clone() });
        } else {
            current.clear();
        }
    }
    if current.len() >= 2 {
        runs.push(Curve3 { points: current, ..curve.clone() });
    }

    runs
}
