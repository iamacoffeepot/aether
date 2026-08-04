//! The stroke intermediate representation.
//!
//! Everything the renderer extracts is a `Curve3`: an ordered run of
//! surface points carrying the normal that shading needs. Nothing
//! downstream knows which feature produced a curve, which is what lets
//! one visibility pass and one styling pass serve every kind.

use aether_math::Vec3;

/// Which physical pen draws a stroke. A plotter changes pens between
/// passes, so the emitter groups by this.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Pen {
    /// Graphite — outlines and the darkest hatching.
    Ink,
    /// The character's blue, for accents.
    Accent,
    /// A lighter grey for tone that should recede behind the outline.
    Pale,
    /// Diagnostic only: one pen per bone, for the weight-paint view.
    Bone(u8),
}

impl Pen {
    /// Draw order, low first. Tone sits under the outline; the skeleton
    /// sits over everything, being a diagnostic rather than part of the
    /// drawing.
    pub fn layer(self) -> u8 {
        match self {
            Self::Pale => 0,
            Self::Accent => 1,
            Self::Ink => 2,
            Self::Bone(_) => 3,
        }
    }

    pub fn hex(self) -> &'static str {
        match self {
            Self::Ink => "#1b1b1f",
            Self::Accent => "#3f7fd0",
            Self::Pale => "#7d7d86",
            // chest, neck, head, jaw, ear_left, ear_right — in rig order.
            Self::Bone(index) => {
                ["#b9b4a8", "#0f9b8e", "#2f6fd0", "#d6402f", "#4aa832", "#e08a1e"][usize::from(index) % 6]
            }
        }
    }
}

/// What a curve is, which sets its weight and how heavily it wanders.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeatureClass {
    /// The outline of the form against what lies behind it.
    Silhouette,
    /// An authored surface marking — eye, brow, mouth.
    Decal,
    /// Tone. `level` 0 is the first pass, 1 the cross, 2 the third.
    Hatch { level: u8 },
}

impl FeatureClass {
    /// Base stroke weight in page pixels, before pressure and depth.
    pub fn base_width(self) -> f32 {
        match self {
            Self::Silhouette => 2.0,
            Self::Decal => 1.3,
            Self::Hatch { level } => 0.9 - 0.14 * f32::from(level),
        }
    }

    /// How far the hand wanders, in page pixels. An outline is drawn
    /// deliberately; hatching is faster and looser.
    pub fn wobble_amplitude(self) -> f32 {
        match self {
            Self::Silhouette => 0.8,
            Self::Decal => 0.45,
            Self::Hatch { .. } => 1.15,
        }
    }

    /// Shortest run worth drawing, in page pixels. Detail too small to read
    /// is dropped rather than inked as noise — a relief field on a
    /// reconstruction throws off specks, and a speck at full weight reads as
    /// dirt on the paper.
    ///
    /// The floor is per class because a speck means something different in
    /// each. Tone and surface marking are where the specks land, so they
    /// hold the high floors; a silhouette run that short is still the edge
    /// of the form, and dropping it opens a gap in an outline.
    pub fn min_length(self) -> f32 {
        match self {
            Self::Silhouette => 1.5,
            Self::Decal => 4.0,
            Self::Hatch { .. } => 3.5,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SurfacePoint {
    /// Where the pen goes.
    pub pos: Vec3,
    pub normal: Vec3,
    /// Where on the model this point is, for asking whether it can be seen.
    ///
    /// The same as `pos` for anything extracted from the surface. A chart
    /// mark is different: it is drawn on a plane fitted *through* the
    /// surface, so parts of it sit inside the head — and asking whether a
    /// point inside a head is occluded gets back yes, in patches, which is
    /// how a lid line becomes shards as she turns. A decal is visible
    /// exactly when the skin under it is, so that is the point to ask
    /// about.
    pub probe: Vec3,
    /// Width multiplier over the class's base weight.
    ///
    /// The chart has always authored a taper per point — 145's lip profile,
    /// the brow thinning outward — and it was being computed and dropped,
    /// so every authored mark came out at one flat width. Carrying it here
    /// means it survives welding and the visibility split, both of which
    /// cut curves up without knowing what they are.
    pub weight: f32,
}

impl SurfacePoint {
    /// A point that is its own probe — everything read off the surface.
    pub fn on_surface(pos: Vec3, normal: Vec3) -> Self {
        Self { pos, normal, probe: pos, weight: 1.0 }
    }
}

#[derive(Clone, Debug)]
pub struct Curve3 {
    pub points: Vec<SurfacePoint>,
    pub class: FeatureClass,
    pub pen: Pen,
    /// Stable stroke identity, seeding the wobble. Derived from the
    /// feature and plane index — never from traversal order or a clock.
    pub seed: u64,
    /// Whether the chart drew this rather than the surface.
    ///
    /// An extracted curve is happy to be cut into pieces — a hatch line
    /// crossing behind an ear *should* arrive as two runs. An authored mark
    /// is a single gesture and survives being shortened but not being
    /// shattered: a lash line reduced to four fragments between hair
    /// strands reads as dirt on the paper, not as an eye.
    pub authored: bool,
}

/// One frame's drawing, divided by whether a curve depends on the eye.
///
/// The division is the drawing's own: hatch and crease describe the
/// surface and are extracted once at load, while the silhouette, the
/// charted face and the suggestive contours are re-solved every time
/// the eye moves. Carrying it as a type rather than as an index into a
/// concatenated list is what lets the layout give the resident half
/// the same texels every frame, and the packer send it to the GPU once
/// (iamacoffeepot/aether#4435).
///
/// The two halves concatenate — resident first — wherever the whole
/// drawing is wanted, and that order is what every curve index in a
/// [`Layout`](crate::easel::program::sight::Layout) means.
///
/// The split is by volatility rather than by class, which is what
/// carries it into animation: a deforming pose moves curves from the
/// resident half to the volatile one and nothing else has to change.
#[derive(Clone, Copy)]
pub struct Drawing<'a> {
    /// View-independent, and so uploaded once per subject.
    pub resident: &'a [Curve3],
    /// Re-solved per eye, and so uploaded per re-split.
    pub volatile: &'a [Curve3],
}

/// One of a [`Drawing`]'s two halves, for an operation that runs over
/// the regions separately — the field's point buffers, the ink's ribbon
/// buffers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Half {
    Resident,
    Volatile,
}

impl<'a> Drawing<'a> {
    /// The whole drawing in curve-index order.
    pub fn curves(&self) -> impl Iterator<Item = &'a Curve3> {
        self.resident.iter().chain(self.volatile)
    }

    /// One half's curves in the order they arrived, each paired with
    /// the curve index it holds in the concatenation — which is what a
    /// [`Layout`](crate::easel::program::sight::Layout) indexes, and so
    /// how a curve here finds the span it was placed at.
    ///
    /// Arrival order rather than the field's own, because ribbons
    /// composite in the order they are drawn and that order is the
    /// drawing's.
    pub fn half(&self, half: Half) -> impl Iterator<Item = (usize, &'a Curve3)> {
        let (curves, base) = match half {
            Half::Resident => (self.resident, 0),
            Half::Volatile => (self.volatile, self.resident.len()),
        };

        curves.iter().enumerate().map(move |(at, curve)| (base + at, curve))
    }

    /// The `index`th curve of the concatenation — what a
    /// [`Span`](crate::easel::program::sight::Span) names.
    pub fn curve(&self, index: usize) -> Option<&'a Curve3> {
        self.resident.get(index).or_else(|| self.volatile.get(index.checked_sub(self.resident.len())?))
    }

    /// How many curves the drawing carries.
    pub fn len(&self) -> usize {
        self.resident.len() + self.volatile.len()
    }

    /// Whether the drawing has no curves at all.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
