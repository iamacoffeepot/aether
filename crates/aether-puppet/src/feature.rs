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
#[derive(Clone, Copy, Debug, PartialEq)]
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
