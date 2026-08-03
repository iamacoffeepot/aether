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
    /// The mouth shape to draw, as spike 145's `(openness, width, corner,
    /// teeth)`. `None` leaves her mouth to the geometry alone.
    /// Which mark, if any, stands in for the nose. `None` leaves the nose
    /// to the sculpt, which can draw its own given a threshold it can
    /// actually reach.
    /// Relief threshold for `skin`, applied only inside the nose window.
    ///
    /// One global threshold cannot serve both an eyelid and a nose: a lid
    /// is carved three times as deep, so a number set for lids buries the
    /// nose, and a number set for the nose brings every swell on her cheeks
    /// with it. Zero switches the pass off.
    /// Curvature a suggestive contour must reach to be drawn. Zero is off.
    pub suggestive: f32,
    /// How near the turn a suggestive contour has to be, as `n . v`.
    pub suggestive_gate: f32,
    /// How far the tick's lower end kicks away from the midline.
    pub nose_bend: f32,
    /// Lattice resolution the silhouette mesh is clustered to, along the
    /// subject's longest axis. Zero solves the silhouette on the fine mesh.
    ///
    /// The silhouette is the only per-frame geometry pass, so this is the
    /// number that decides whether the drawing holds a frame rate — and it
    /// is a large-scale feature by definition, being where the surface
    /// turns away from the eye, so it does not need the faces the carving
    /// does. Creases and occlusion stay fine.
    pub silhouette_cells: u32,
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
            // Both off. Either can find her nose, and both draw it as a
            // contour — which is the right answer to a different question.
            // A single bar says nose without claiming to describe one, and
            // it gets out of the way as soon as the profile can.
            suggestive: 0.0,
            suggestive_gate: 0.60,
            // Straight. The curve made it a nose and then kept going —
            // past about 0.4 it reads as a contour and starts competing
            // with the profile for the same job.
            nose_bend: 0.0,
            // Measured on the sculpt rather than picked. At 128 the
            // drawing is indistinguishable from the fine solve at normal
            // framing, on an eighth of the faces, and the whole per-frame
            // path drops from 23.8ms to 3.7ms. 256 is the next rung and
            // costs 8.1ms for no visible gain.
            silhouette_cells: 128,
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
    // Every class the drawing wants inked. When the charted face lands
    // it will take the eye, brow and lip classes off this list — a charted
    // lid over a sculpted one is two lids a few pixels apart — but until
    // then the sculpt draws all of them.
    let classes = settings.crease_classes.clone();

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

/// Where the mouth is, measured from the `lips` class the same way spike
/// 145's `mouth_anchor` does — from the field, saying nothing about how it
/// looks.
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

/// Whether the profile already runs over the nose.
///
/// A box around the measured tip, not a screen-space test: the silhouette
/// only passes through here once the head has turned far enough for the
/// bridge to break the outline, which is exactly when the stand-in stops
/// being wanted. The box stays tight to the front and to the midline or a
/// cheek edge trips it while she is still face-on.
///
/// Skin only, and that is not a detail. Her fringe hangs down the midline
/// and throws a silhouette straight through the box at every angle
/// including dead ahead, so an unmasked test suppresses the tick always
/// and the nose simply never appears. The question is whether her *face*
/// The view-independent drawing: everything that describes the surface
/// rather than the viewer.
///
/// Hatch planes are world-space and creases are a property of the carving,
/// so neither moves when the eye does — which means both are solved once at
/// load and kept. Only the silhouette and the visibility split are per
/// frame. The offline renderer recomputes all of it every frame because it
/// has no reason not to; here that difference is most of the budget.
pub fn surface(mesh: &Mesh, labels: Option<&Labels>, settings: &Settings) -> Vec<Curve3> {
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
