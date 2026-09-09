//! Stage one: which lines would an illustrator draw on this surface?
//!
//! Two feature kinds, both of them level sets of a per-vertex scalar, so
//! both come out of the same machinery:
//!
//! - **Silhouette** — the zero set of `view . normal`. Where the surface
//!   turns away from the eye.
//! - **Hatch** — the level sets of `position . axis`, one family per
//!   resident [`hatch`] axis, switching on as the tone darkens. Which
//!   three of them a view draws is [`hatch::Choice`]'s answer, not this
//!   pass'.
//!
//! Nothing here consults visibility. Extraction says what exists; the
//! next pass says what survives.

use aether_math::Vec3;

use crate::anchor::{Anchor, Anchors};
use crate::chart;
use crate::easel::palette::{FACE_CLASSES, SKIN_CLASS};
use crate::feature::{Curve3, FeatureClass, Pen, SurfacePoint};
use crate::hatch;
use crate::labels::{self, Labels};
use crate::math3::{camera_frame, noise};
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

/// What [`Settings::light`] is a direction *in*.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LightFrame {
    /// A world direction. The right answer for a lit scene, where the
    /// light belongs to the room rather than to the viewer.
    ///
    /// Wrong for a subject on a turntable, and visibly so: the lit side
    /// stays put while the camera walks around it, so one azimuth shows
    /// the fully lit side and comes back a bare outline while the
    /// opposite one shows the fully shaded side and comes back a solid
    /// mesh. Nothing in between reads as a tone ramp because no view
    /// carries both ends of one.
    World,
    /// A direction in the camera's own frame: `x` to the viewer's right,
    /// `y` up, `z` toward the viewer.
    ///
    /// Where an illustrator's key light stands. It is placed relative to
    /// the drawing rather than to the subject, so it stays over the same
    /// shoulder while the subject turns and *every* view has a lit side
    /// and a shaded side to run a ramp between.
    Camera,
}

#[derive(Clone)]
pub struct Settings {
    /// Distance between hatch lines, as a fraction of the subject's
    /// longest bounding-box axis.
    ///
    /// A fraction rather than a world distance because a spacing tuned
    /// against one subject's scale means nothing on the next one: the
    /// same number that rules a readable hatch on a head-sized sculpt
    /// draws a wire mesh on a subject three times the size, and the
    /// drawing is supposed to be a property of the style, not of what
    /// units the modeller happened to work in.
    pub hatch_spacing: f32,
    /// Per-rank multiplier over [`Self::hatch_spacing`].
    ///
    /// Each successive rank is a little sparser than the last, so the
    /// step from one family to two adds tone rather than doubling it.
    /// Keyed by rank rather than by axis, so a view that swaps which
    /// axis it draws at a rank draws it at the same density
    /// ([`hatch::rank`]).
    pub hatch_family_spacing: [f32; hatch::RANKS],
    /// Tone below which each successive hatch rank switches on.
    ///
    /// The ramp the drawing shades with: above the first, bare paper;
    /// below the last, all three families crossing. They want to be
    /// spread across the range tone actually reaches — from
    /// [`Self::ambient`] at the terminator to `1` under the key light —
    /// because a threshold set below the ambient floor names a tone no
    /// point on the subject has and that family never draws.
    pub hatch_thresholds: [f32; hatch::RANKS],
    /// How far a hatch threshold is dithered, in tone.
    ///
    /// Comparing tone against a constant puts a family's edge exactly on
    /// a level curve of the lighting, which reads as a ruled line slicing
    /// across the figure. Perturbing the threshold by
    /// [`noise`] lets the family break into dashes
    /// as it fades, which is what a hand does. Zero rules the boundary.
    pub hatch_dither: f32,
    /// How far the resident axis set is turned about the model's `z`.
    ///
    /// The whole set rather than one family's angle: the axes are spread
    /// over the sphere and a view picks three of them, so there is no
    /// primary family left to be the angle of. The set is even enough
    /// that turning it does not change how well its best three cross —
    /// this moves where the strokes sit on the subject, not whether they
    /// cross.
    pub hatch_tilt: f32,
    /// Direction the key light arrives from, read in [`Self::light_frame`].
    pub light: Vec3,
    /// Whether [`Self::light`] is a world direction or a camera-frame one.
    pub light_frame: LightFrame,
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
            hatch_family_spacing: [1.0, 1.30, 1.62],
            // Spread across the range tone reaches. Under the old
            // numbers the last family switched on at 0.30 against an
            // ambient floor of 0.28, so two thirds of the ramp lived in
            // the last two percent of the tone range and the drawing had
            // one step in it rather than three.
            hatch_thresholds: [0.72, 0.58, 0.44],
            hatch_dither: 0.070,
            hatch_tilt: 0.62,
            // Over the viewer's left shoulder and a little above, which
            // is where an inker puts it. Read in the camera's frame, so
            // it stays there through a turn.
            light: Vec3::new(-0.52, 0.55, 0.66),
            light_frame: LightFrame::Camera,
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
    /// Lighting term at a point: `0` in shadow, `1` fully lit.
    ///
    /// Reads [`Self::light`] as a world direction, so ask
    /// [`Self::resolved`] for the settings a view shades with rather than
    /// sampling this against a camera-frame light.
    pub fn tone(&self, point: &SurfacePoint) -> f32 {
        let lambert = point.normal.dot(self.light.normalize()).max(0.0);
        self.ambient + (1.0 - self.ambient) * lambert + self.face_lift * face_weight(point.pos)
    }

    /// The key light as a world direction, given where the eye stands.
    #[must_use]
    pub fn key_light(&self, eye: Vec3, target: Vec3) -> Vec3 {
        match self.light_frame {
            LightFrame::World => self.light,
            LightFrame::Camera => {
                let (right, up, back) = camera_frame(eye, target);

                (right * self.light.x + up * self.light.y + back * self.light.z).normalize_or(back)
            }
        }
    }

    /// This view's shading: the authored style with everything the eye
    /// decides already decided, stated in the world frame.
    ///
    /// Tone is sampled once per point of every hatch curve and again per
    /// vertex by the wash, so the view enters here, once, and everything
    /// downstream reads a plain [`LightFrame::World`] `Settings` that
    /// knows nothing about a camera.
    ///
    /// `charted` says whether a face is actually being drawn on this
    /// subject. [`Self::face_lift`] exists to keep hatch off an authored
    /// face; with no face charted there is no face to protect, and the
    /// lift's window is then just a bright box somewhere in the subject's
    /// own bounds — a hole in the shading of a subject that never had a
    /// face to begin with.
    #[must_use]
    pub fn resolved(&self, eye: Vec3, target: Vec3, charted: bool) -> Self {
        Self {
            light: self.key_light(eye, target),
            light_frame: LightFrame::World,
            face_lift: if charted {
                self.face_lift
            } else {
                0.0
            },
            ..self.clone()
        }
    }

    /// Whether the tone gate can be settled once, at load.
    ///
    /// The *tone* half of it. Which three axes a view hatches with is
    /// never settled at load — the eye decides that, and the eye moves —
    /// so the shader refuses an unselected axis whatever this answers
    /// (`sight.wgsl`'s `hatched`).
    ///
    /// Only when nothing it reads can move under it afterwards. A key
    /// light on the camera rig turns with every orbit, so the verdict it
    /// reaches at load is the verdict for one azimuth and wrong for the
    /// rest — that subject's hatching goes to the GPU ungated and is
    /// gated in the vertex stage instead, the same place a rig's does.
    #[must_use]
    pub fn gate_settles_at_load(&self) -> bool {
        self.light_frame == LightFrame::World
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
    segments.into_iter().map(|[a, b]| [SurfacePoint::anchored(&a), SurfacePoint::anchored(&b)]).collect()
}

pub fn silhouettes(mesh: &Mesh, eye: Vec3) -> Vec<Curve3> {
    let template =
        Curve3 { points: Vec::new(), class: FeatureClass::Silhouette, pen: Pen::Ink, seed: 0, authored: false };

    weld::curves(to_points(mesh.silhouette_level_set(eye)), &template)
}

/// Hatching as one family of world-space plane cuts per resident axis.
///
/// Holding the planes in *world* space rather than deriving them from the
/// camera is what keeps the hatch welded to the figure: orbit the camera
/// and the strokes stay on the surface they belong to instead of sliding
/// across it. Each family shares one seed per plane, so a line that
/// crosses the whole figure wobbles as one line.
///
/// Every [`hatch::AXES`] family is solved here and kept, and a view draws
/// the three of them that cross best from where it stands. The extra
/// families are the price of that choice: solving them per view would
/// mean re-cutting the subject on every orbit step, which is the work
/// residency exists to avoid.
pub fn hatching(mesh: &Mesh, settings: &Settings) -> Vec<Curve3> {
    let mut out = Vec::new();

    // The spacing is authored as a fraction of the subject, so it is the
    // subject that says what it means in world units. A degenerate mesh
    // has no scale to measure against and gets no hatching rather than a
    // division by zero.
    let extent = mesh.max - mesh.min;
    let reach = extent.x.max(extent.y).max(extent.z);
    if !reach.is_finite() || reach <= 0.0 {
        return out;
    }

    for (axis, normal) in hatch::axes(settings.hatch_tilt).into_iter().enumerate() {
        let axis = axis as u8;
        let spacing = settings.hatch_spacing * reach * settings.hatch_family_spacing[hatch::rank(axis)];
        if !spacing.is_finite() || spacing <= 0.0 {
            continue;
        }

        for (plane, segments) in mesh.level_sets(&mesh.projected(normal), spacing) {
            let template = Curve3 {
                points: Vec::new(),
                class: FeatureClass::Hatch { axis },
                pen: Pen::Pale,
                seed: u64::from(u32::from(axis)) << 32 | u64::from(plane.unsigned_abs()),
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
    // Which classes the chart takes over is a question about the
    // subject's own vocabulary, asked of the field it stamped: a class id
    // is a position in that vocabulary and means nothing on its own.
    let charted = settings.face.is_some();
    let face = labels.map(|field| field.classes_named(&FACE_CLASSES)).unwrap_or_default();
    let classes: Vec<u8> =
        settings.crease_classes.iter().copied().filter(|class| !(charted && face.contains(class))).collect();

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
/// `rest` is the sculpt the material field was placed against. The
/// contours are solved on `mesh`, which is the posed surface when a rig is
/// driving one — but the field's lattice sits on the rest sculpt, so the
/// mask is asked where each crossing sits *there*, through the anchorage
/// the crossing carries. Sampling a posed position against a rest lattice
/// reads whichever material has drifted under it, which on an ear that
/// swung is a different one entirely.
pub fn suggestive(mesh: &Mesh, rest: &Mesh, labels: Option<&Labels>, eye: Vec3, settings: &Settings) -> Vec<Curve3> {
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
    let skin = labels.map(|field| field.classes_named(&[SKIN_CLASS])).unwrap_or_default();
    let segments: Vec<_> = mesh
        .suggestive(eye, settings.suggestive, settings.suggestive_gate)
        .into_iter()
        .filter(|[a, b]| labels.is_none_or(|field| field.is(rest.at(a.at), &skin) && field.is(rest.at(b.at), &skin)))
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

    let skin = labels.classes_named(&[SKIN_CLASS]);
    let segments: Vec<_> = mesh
        .level_set(&relief, &steepness, settings.nose_relief)
        .into_iter()
        .filter(|[a, b]| inside(a.pos) && inside(b.pos))
        .filter(|[a, b]| labels.is(a.pos, &skin) || labels.is(b.pos, &skin))
        .collect();

    weld::curves(to_points(segments), &template)
}

/// Which hatch families survive at each point, given the light and the
/// point's own normal and position.
///
/// A function of the pose and of the light, and of nothing else. It reads
/// each point's normal, and skinning turns normals; it reads the key
/// light, and a light stated in the camera's frame turns with every
/// orbit. Whichever of the two can still move afterwards, the gate has to
/// stand downstream of it — which is why the only subject gated here, at
/// load, is the one [`Settings::gate_settles_at_load`] describes and the
/// rest are gated in the vertex stage (`sight.wgsl`'s `hatched`). Left at
/// load against either, the shading freezes to one pose or one azimuth
/// and slides under the drawing (iamacoffeepot/aether#4336).
pub fn tone_gate(curves: Vec<Curve3>, settings: &Settings) -> Vec<Curve3> {
    curves
        .into_iter()
        .flat_map(|curve| {
            let limit = match curve.class {
                FeatureClass::Hatch { axis } => settings.hatch_thresholds[hatch::rank(axis)],
                FeatureClass::Silhouette | FeatureClass::Decal => return vec![curve],
            };

            lit_runs(&curve, |point| settings.tone(point) < limit + noise(point.pos) * settings.hatch_dither)
        })
        .collect()
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
///
/// Ungated, and every point carries the
/// [`Anchorage`](crate::mesh::Anchorage) it was found at — so a pose skins
/// these curves rather than re-solving them, and [`tone_gate`] then runs
/// against the normals the pose gave them.
pub fn surface(mesh: &Mesh, labels: Option<&Labels>, anchors: Option<&Anchors>, settings: &Settings) -> Vec<Curve3> {
    let mut out = hatching(mesh, settings);

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
