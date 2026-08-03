//! The wash itself: how a region of the drawing becomes wet paint.
//!
//! A watercolour mark is the meeting of four instruments — brush, hand,
//! water and paper — and each contributes axes rather than looks. Those
//! axes are [`WashParams`], and a wash is what happens when they are
//! turned on together over one region:
//!
//! - **water** softens the region into a puddle, and the puddle is
//!   thresholded rather than drawn, so where the edge lands is the water's
//!   decision and not the mask's;
//! - **pours** are the successive touches of a loaded brush, each a
//!   slightly smaller sibling of the last, offset — never concentric,
//!   which is the one thing pooled water never is;
//! - the **rim** is the alpha minus its own blur, which is where pigment
//!   is carried as the puddle retreats, varying along the boundary so no
//!   two stretches of tide line read alike;
//! - **granulation** settles the pigment into the paper's tooth, and every
//!   material keys off the one shared tooth noise, so the whole picture
//!   agrees about which sheet it is painted on;
//! - **sag** walks the stain downhill, **spatter** throws drops off the
//!   brush, and a **lost edge** dissolves one side of the region into the
//!   paper entirely.
//!
//! How tightly those are held is [`Sheet::care`]: near the face the
//! painter cuts to the line, and past the fall of the hair the hand
//! relaxes.
//!
//! Past the drawing entirely is the atmosphere, where a material that
//! names the policy throws a thinned echo of itself into the air off the
//! figure — the one mark on the sheet with no line under it.

use core::f32::consts::TAU;

use aether_math::Vec2;

use super::accent::Accents;
use super::image::{self, Flow, Rng};
use super::palette::{self, Atmosphere, Coat, Material};
use crate::labels::{BROW, DRESS, EYE, HAIR, LIPS, SKIN};

/// The baked planes a sheet is painted from, all at `width * height` and
/// row major.
#[derive(Clone, Copy)]
pub struct Planes<'a> {
    /// Which material each pixel belongs to — the region class plane.
    pub classes: &'a [u8],
    /// How the key light falls, in `[0, 1]`. Value decides coverage.
    pub tone: &'a [f32],
    /// How square each pixel's surface sits to the viewer, in `[0, 1]`.
    ///
    /// Carried because it is part of the bake and the surface-anchored
    /// accent policies read it — a grazing cheek takes no blush a frontal
    /// one would. None of the washes ported here consult it.
    pub facing: &'a [f32],
    pub width: usize,
    pub height: usize,
}

/// One touch of a loaded brush.
pub struct Pour {
    /// Size relative to the region, about its own centroid. Below one the
    /// touch lands inside the last, which is how a pool gains a middle.
    pub scale: f32,
    /// How much pigment this touch releases.
    pub body: f32,
}

/// A single pour at full size, which is what a tight wash is.
const ONE_POUR: &[Pour] = &[Pour { scale: 1.0, body: 1.0 }];

/// Three touches of a drying brush, each smaller and heavier than the
/// last — a pool with tide rings in it.
const POOLED_POURS: &[Pour] =
    &[Pour { scale: 1.0, body: 0.16 }, Pour { scale: 0.86, body: 0.3 }, Pour { scale: 0.7, body: 0.5 }];

/// The second pigment dropped into a wash still wet.
const GLAZE_POURS: &[Pour] = &[Pour { scale: 0.88, body: 0.6 }];

/// The axes of one wash. Distances are in the pixels of the reference
/// sheet and are converted through [`image::tuned`] at the point of use.
pub struct WashParams {
    /// How wet the paper is — the radius the region is softened by before
    /// its edge is decided.
    pub water: f32,
    /// Where in the softened puddle the edge is taken. Lower spreads the
    /// wash past the line the drawing set.
    pub level: f32,
    /// How far the tide-line noise is allowed to move that edge.
    pub wobble: f32,
    /// Strength of the pigment carried to the retreating edge.
    pub rim: f32,
    /// How far a pour may wander from the region's centre.
    pub wander: f32,
    /// How strongly the pigment settles into the tooth.
    pub gran: f32,
    /// How much pigment the brush carries.
    pub load: f32,
    /// Whether the sheet is tilted and the stain walks downhill.
    pub sag: bool,
    /// Direction, in radians about the region's centroid, of the side that
    /// dissolves into the paper instead of holding an edge.
    pub lost: Option<f32>,
    /// Drops thrown off the brush.
    pub spatter: u32,
    pub pours: &'static [Pour],
}

impl WashParams {
    /// Wet on dry, cut to the line: a small puddle, a hard threshold, a
    /// thin rim, and nothing left to chance. Gongbi.
    pub const fn tight() -> Self {
        Self {
            water: 3.2,
            level: 0.5,
            wobble: 0.12,
            rim: 0.8,
            wander: 0.0,
            gran: 0.0,
            load: 1.0,
            sag: false,
            lost: None,
            spatter: 0,
            pours: ONE_POUR,
        }
    }

    /// A flooded sheet: the wash overshoots the line, pools into tide
    /// rings, and gravity drags it down. Xieyi.
    pub const fn loose() -> Self {
        Self {
            water: 12.0,
            level: 0.38,
            wobble: 0.55,
            rim: 1.3,
            wander: 52.0,
            gran: 0.0,
            load: 1.0,
            sag: true,
            lost: None,
            spatter: 0,
            pours: POOLED_POURS,
        }
    }

    /// A violet bleeding through the wet indigo of the hair.
    pub const fn glaze() -> Self {
        Self {
            water: 22.0,
            level: 0.42,
            wobble: 0.35,
            rim: 0.25,
            wander: 60.0,
            gran: 0.3,
            load: 0.3,
            sag: true,
            lost: None,
            spatter: 0,
            pours: GLAZE_POURS,
        }
    }

    /// What the brush is charged with.
    pub const fn charged(mut self, gran: f32, load: f32) -> Self {
        self.gran = gran;
        self.load = load;
        self
    }

    /// Which way the region gives up its edge.
    pub const fn losing(mut self, angle: f32) -> Self {
        self.lost = Some(angle);
        self
    }

    /// How many drops come off the brush.
    pub const fn spattering(mut self, drops: u32) -> Self {
        self.spatter = drops;
        self
    }

    /// How wet the paper is under this wash.
    pub const fn wetted(mut self, water: f32) -> Self {
        self.water = water;
        self
    }
}

/// Half-width of the band the puddle is thresholded across.
const EDGE_BAND: f32 = 0.08;

/// How far past the wash's own water the rim's reference blur reaches.
const RIM_SPREAD: f32 = 1.6;

/// Radius added to a wash's water before the value and support are
/// softened, so neither reads as sharper than the wash carrying it.
const SUPPORT_MARGIN: f32 = 2.0;

/// How much stronger the rim reads than the body it edges.
const RIM_GAIN: f32 = 2.2;

/// Middle strength of the tide line and how far the paper's own noise
/// swings it either side, with a ceiling on the darkest it may go.
const RIM_VARY: (f32, f32) = (0.55, 1.5);
const RIM_VARY_CEILING: f32 = 1.3;

/// How far the rim's window into the noise is displaced past the one that
/// placed the edge, in multiples of the pour's own offset.
const RIM_RESTRIDE: (usize, usize) = (3, 7);

/// Floor under the region's own support, so a thin sliver of coverage does
/// not divide the value it carries up to something enormous.
const SUPPORT_FLOOR: f32 = 0.05;

/// Spacing of the two downhill samples a sagging wash drags behind it.
const SAG_STEP: f32 = 12.0;

/// How much of the wash each downhill sample carries.
const SAG_FALLOFF: [f32; 2] = [0.8, 0.55];

/// Half-angle over which a lost edge gives way, in radians.
const LOST_ARC: (f32, f32) = (1.3, 0.55);

/// How hard the paper takes the wash back where the edge is lost.
const LOST_FALLOFF: f32 = 1.8;

/// How much of a lost edge survives as a stain with no edge at all.
const LOST_STAIN: f32 = 0.85;

/// Density under which granulation is not worth applying.
const GRANULATION_FLOOR: f32 = 0.003;

/// How much of the tooth granulation is allowed to express, and the point
/// in the tooth it pivots about — below it the pigment is lifted off the
/// peaks, above it settled into the pits.
const GRANULATION_AUTHORITY: f32 = 0.85;
const GRANULATION_PIVOT: f32 = 0.18;

/// How far off the region a thrown drop lands, and how much further it
/// travels down the sheet than across it.
const SPATTER_THROW: (f32, f32) = (80.0, 240.0);
const SPATTER_DROOP: f32 = 1.25;

/// Radius and strength of one thrown drop.
const SPATTER_RADIUS: (f32, f32) = (1.6, 5.6);
const SPATTER_STRENGTH: (f32, f32) = (0.4, 0.7);

/// Distances from the face over which the hand relaxes: cut to the line
/// this close, wholly free past this far.
const CARE_NEAR: f32 = 160.0;
const CARE_FAR: f32 = 600.0;

/// Fraction of its granulation a material keeps in its tight coat.
///
/// The tight wash sits under the loose one across the whole care ramp, so
/// letting it granulate at full strength would double the tooth wherever
/// the two overlap.
const TIGHT_GRANULATION: f32 = 0.4;

/// Which way each material gives up its far edge, as a run over rise about
/// the region's centroid. The dress turns away sooner than the hair does.
const LOST_RISE: f32 = 0.6;
const LOST_RUN: (f32, f32) = (-0.4, -0.8);

/// Drops thrown off the brush loading the hair, the one region painted
/// with enough water to throw any.
const HAIR_SPATTER: u32 = 20;

/// The violet dropped into the wet hair, and how far it may build.
const GLAZE_PIGMENT: u32 = 0x8d_84_b8;
const GLAZE_CAP: f32 = 0.8;

/// Passes and reach of the smear that rides the hair along its own locks.
const SMEAR_PASSES: u32 = 2;
const SMEAR_REACH: f32 = 12.0;

/// How far the figure's own coverage is softened before it pushes the
/// stain off itself, and how much of the stain it takes back where it
/// stands. Short of all of it: the air in front of a shoulder is still air.
const ATMOSPHERE_FIGURE: f32 = 6.0;
const ATMOSPHERE_RESIST: f32 = 0.85;

/// Where in the displaced halo the stain reaches full strength, and the
/// level its own mask is cut at. The ramp fades the far tail out; the cut
/// is also what keeps the stain off the figure, whose surviving fifteen
/// hundredths of spill falls under it.
const ATMOSPHERE_REACH: (f32, f32) = (0.1, 0.4);
const ATMOSPHERE_LEVEL: f32 = 0.45;

/// The wash the stain is painted as, against the loose one it echoes: a
/// smaller puddle, a thinner load, granulating far harder, and a dozen
/// drops thrown after it.
const ATMOSPHERE_WATER: f32 = 7.0;
const ATMOSPHERE_LOAD: f32 = 0.42;
const ATMOSPHERE_GRAN: f32 = 0.6;
const ATMOSPHERE_SPATTER: u32 = 12;

/// Which way the stain gives up its edge, as a rise over a run — steeper
/// and squarer than the figure's own, there being no form under it to turn
/// away.
const ATMOSPHERE_LOST: (f32, f32) = (1.0, -0.5);

/// Domain constant for the stain's own stream of chance.
const ATMOSPHERE_SEED: u64 = 0xa7_a7;

/// One sheet of paper with the drawing's planes registered on it.
///
/// The paper's grain, its mottle and its tide-line noise are sampled once
/// here and shared by every material, which is what makes the picture read
/// as painted on a single sheet rather than as regions composited
/// together. The care field is derived once for the same reason.
pub struct Sheet<'a> {
    planes: Planes<'a>,
    seed: u64,
    /// The paper's tooth: fine, high frequency, and the thing granulating
    /// pigment settles into.
    tooth: Vec<f32>,
    /// Coarse noise the wash's edges are dithered along, so no stretch of
    /// tide line reads like its neighbour.
    edge_noise: Vec<f32>,
    /// The sheet's own colour variation, as a multiplier about one.
    shade: Vec<f32>,
    /// How closely the hand is held, in `[0, 1]`.
    care: Vec<f32>,
}

/// Domain constants for the three noise fields, so the same sheet seed
/// gives each its own grain.
const TOOTH_SEED: u64 = 0xa1_f0;
const MOTTLE_SEED: u64 = 0x51_7e;
const EDGE_SEED: u64 = 0xe4_7b;

/// Octaves and base cell counts of the three fields. Cells span the sheet
/// in texture space, so a grain keeps its size relative to the picture at
/// any resolution.
const TOOTH_NOISE: (u32, f32) = (4, 96.0);
const MOTTLE_NOISE: (u32, f32) = (3, 6.0);
const EDGE_NOISE: (u32, f32) = (3, 24.0);

/// How far the tooth and the mottle move the paper's own colour, in 8-bit
/// channels either side of white.
const SHADE_SWING: (f32, f32) = (7.0, 4.0);

/// How much of the coarse noise and of the mottle the tide-line noise is.
const EDGE_MIX: (f32, f32) = (0.9, 0.5);

impl<'a> Sheet<'a> {
    pub fn new(planes: Planes<'a>, seed: u64) -> Self {
        let (width, height) = (planes.width, planes.height);
        let tooth = image::Noise::new(seed ^ TOOTH_SEED, TOOTH_NOISE.0, TOOTH_NOISE.1).plane(width, height);
        let mottle = image::Noise::new(seed ^ MOTTLE_SEED, MOTTLE_NOISE.0, MOTTLE_NOISE.1).plane(width, height);
        let coarse = image::Noise::new(seed ^ EDGE_SEED, EDGE_NOISE.0, EDGE_NOISE.1).plane(width, height);

        let edge_noise = coarse
            .iter()
            .zip(&mottle)
            .map(|(&at, &blotch)| (at - 0.5) * EDGE_MIX.0 + (blotch - 0.5) * EDGE_MIX.1)
            .collect();
        let shade = tooth
            .iter()
            .zip(&mottle)
            .map(|(&grain, &blotch)| 1.0 + ((grain - 0.5) * SHADE_SWING.0 + (blotch - 0.5) * SHADE_SWING.1) / 255.0)
            .collect();
        let care = care_field(planes.classes, width, height);

        Self { planes, seed, tooth, edge_noise, shade, care }
    }

    /// The paper's own colour variation, for [`palette::composite`].
    pub fn paper_shade(&self) -> &[f32] {
        &self.shade
    }

    /// How closely the hand is held at each pixel: one at the face, zero
    /// past the fall of the hair.
    pub fn care(&self) -> &[f32] {
        &self.care
    }

    /// Every material in the box, painted over the planes.
    ///
    /// Pass a `flow` solved from the drawing's own coverage — see
    /// [`image::structure_tensor_flow`] — to have the hair brushed down
    /// its locks rather than poured over them; the wash is complete
    /// without it. Pass the [`Accents`] a face was charted for and the
    /// meta-materials paint too: a subject with no face simply has no
    /// coverage for them, and every labelled wash develops unchanged.
    pub fn coats(&self, flow: Option<&Flow>, accents: Option<&Accents>) -> Vec<Coat> {
        let mut rng = Rng::new(self.seed);
        let mut coats = Vec::new();

        for material in palette::MATERIALS {
            let Some(mask) = self.coverage(material, accents) else {
                continue;
            };
            let value = palette::shade_of(material, self.planes.tone);
            let mut density = self.material_wash(material, &mask, &value, &mut rng);

            if material.class == HAIR
                && let Some(flow) = flow
            {
                let reach = image::tuned(SMEAR_REACH, self.planes.height).round() as i32;
                let (width, height) = (self.planes.width, self.planes.height);
                density = image::smear_along_flow(&density, flow, width, height, SMEAR_PASSES, reach);
            }

            // The lid crossing the iris is weight rather than coverage, so
            // it lands on the finished wash: folded into the mask instead
            // it would move the puddle's own edge and take the rim with it.
            if material.class == palette::IRIS
                && let Some(accents) = accents
            {
                for (at, &lift) in density.iter_mut().zip(&accents.lift) {
                    *at *= lift;
                }
            }

            coats.push(Coat { class: material.class, pigment: material.pigment, cap: palette::DENSITY_CAP, density });

            if material.class == HAIR {
                let glaze = self.wash(&mask, None, &WashParams::glaze().losing(lost_angle(HAIR)), &mut rng);
                coats.push(Coat { class: HAIR, pigment: GLAZE_PIGMENT, cap: GLAZE_CAP, density: glaze });
            }

            if let Some(policy) = material.atmosphere.as_ref() {
                // A stream of its own, keyed by the material. The stain's
                // accidents belong to the stain: hanging one on a material
                // then moves nothing already on the sheet, and two
                // materials that both throw one do not throw it alike.
                let mut air = Rng::new(self.seed ^ ATMOSPHERE_SEED ^ u64::from(material.class));
                let density = self.atmosphere(&mask, policy, &mut air);
                coats.push(Coat { class: material.class, pigment: policy.pigment, cap: policy.cap, density });
            }
        }

        // The flush belongs to the skin the way the glaze belongs to the
        // hair: a second pigment over a region already washed, and one
        // that arrives soft and edgeless rather than as a wash of its own.
        if let Some(accents) = accents {
            coats.push(Coat {
                class: SKIN,
                pigment: palette::BLUSH_PIGMENT,
                cap: palette::BLUSH_CAP,
                density: accents.blush.clone(),
            });
        }

        coats
    }

    /// The stain one material leaves in the air past its own silhouette.
    ///
    /// The region's presence — its coverage spread far wider than any wash
    /// reaches — is carried off the figure along the material's own drift
    /// and cut back wherever the figure stands in the way. What that
    /// leaves is then painted as a wash in its own right rather than
    /// composited as the blur it started as, so the stain arrives with
    /// pours, a tide line, a lost edge and a few thrown drops like
    /// everything else on the sheet.
    ///
    /// The reference board records its own weak spot here: displacing a
    /// blur is not the same as pouring deliberately, and a painter would
    /// place this mark rather than derive it. The parameters are the
    /// board's so its look ports intact, and deciding where the air ought
    /// to go is a taste pass rather than a port.
    fn atmosphere(&self, mask: &[f32], policy: &Atmosphere, rng: &mut Rng) -> Vec<f32> {
        let (width, height) = (self.planes.width, self.planes.height);
        let figure: Vec<f32> = self.planes.classes.iter().map(|&at| f32::from(at != 0)).collect();

        let halo = image::blur(mask, width, height, image::tuned(policy.halo, height));
        let standing = image::blur(&figure, width, height, image::tuned(ATMOSPHERE_FIGURE, height));
        let drift = (image::tuned(policy.drift.0, height), image::tuned(policy.drift.1, height));

        let mut spill = vec![0.0; mask.len()];
        for y in 0..height {
            let from_y = (y as f32 - drift.1).clamp(0.0, (height - 1) as f32) as usize * width;
            for x in 0..width {
                let from_x = (x as f32 - drift.0).clamp(0.0, (width - 1) as f32) as usize;
                let i = y * width + x;

                spill[i] = image::smoothstep(ATMOSPHERE_REACH.0, ATMOSPHERE_REACH.1, halo[from_y + from_x])
                    * (1.0 - standing[i] * ATMOSPHERE_RESIST);
            }
        }

        let params = WashParams::loose()
            .wetted(ATMOSPHERE_WATER)
            .charged(ATMOSPHERE_GRAN, ATMOSPHERE_LOAD)
            .losing(ATMOSPHERE_LOST.0.atan2(ATMOSPHERE_LOST.1))
            .spattering(ATMOSPHERE_SPATTER);

        self.wash(&spill.iter().map(|&at| f32::from(at > ATMOSPHERE_LEVEL)).collect::<Vec<f32>>(), None, &params, rng)
    }

    /// Where a material's coverage comes from: the baked class plane for
    /// one the field labels, the chart's own frame for a meta-material.
    ///
    /// `None` is a meta-material on a subject that charted no face — it
    /// paints nothing rather than falling back to a class plane that has
    /// never carried its id.
    fn coverage(&self, material: &Material, accents: Option<&Accents>) -> Option<Vec<f32>> {
        if material.class < palette::META {
            return Some(palette::mask_of(self.planes.classes, material.class));
        }

        accents.and_then(|accents| accents.mask(material.class)).map(<[f32]>::to_vec)
    }

    /// One material's density, held as tightly as the care field says.
    ///
    /// A small region is cut tight wherever it sits: an ear or a brow is a
    /// feature, and no hand loosens over a feature. Everything else is
    /// painted twice — once gongbi, once xieyi — and the care ramp decides
    /// which of the two the eye is looking at.
    fn material_wash(&self, material: &Material, mask: &[f32], value: &[f32], rng: &mut Rng) -> Vec<f32> {
        if material.small {
            let held = WashParams::tight().charged(material.gran, material.load);
            return self.wash(mask, Some(value), &held, rng);
        }

        let drops = if material.class == HAIR {
            HAIR_SPATTER
        } else {
            0
        };
        let held = WashParams::tight().charged(material.gran * TIGHT_GRANULATION, material.load);
        let freed = WashParams::loose()
            .charged(material.gran, material.load)
            .losing(lost_angle(material.class))
            .spattering(drops);

        let tight = self.wash(mask, Some(value), &held, rng);
        let loose = self.wash(mask, Some(value), &freed, rng);

        tight
            .iter()
            .zip(&loose)
            .zip(&self.care)
            .map(|((&held, &free), &care)| held * care + free * (1.0 - care))
            .collect()
    }

    /// The wash: a region, the light on it, and the four instruments.
    ///
    /// `value` is the material's own coverage from [`palette::shade_of`],
    /// or `None` for a wash that carries pigment uniformly. The result is
    /// pigment density, uncapped — [`palette::composite`] decides how much
    /// of it the sheet can hold.
    pub fn wash(&self, mask: &[f32], value: Option<&[f32]>, params: &WashParams, rng: &mut Rng) -> Vec<f32> {
        let (width, height) = (self.planes.width, self.planes.height);
        let mut density = vec![0.0; mask.len()];
        let Some(centre) = centroid(mask, width) else {
            return density;
        };

        let margin = image::tuned(params.water + SUPPORT_MARGIN, height);
        let support = Support {
            centre,
            value: value.map(|plane| image::blur(plane, width, height, margin)),
            reference: image::blur(mask, width, height, margin),
        };

        for pour in params.pours {
            self.pour(&mut density, mask, params, pour, &support, rng);
        }

        if params.gran > 0.0 {
            self.granulate(&mut density, params.gran);
        }
        if params.spatter > 0 {
            self.spatter(&mut density, centre, params.spatter, rng);
        }

        density
    }

    /// One touch of the brush, accumulated into `density`.
    fn pour(
        &self,
        density: &mut [f32],
        mask: &[f32],
        params: &WashParams,
        pour: &Pour,
        support: &Support,
        rng: &mut Rng,
    ) {
        let (width, height) = (self.planes.width, self.planes.height);
        let wander = image::tuned(params.wander, height);
        let jitter = Vec2::new((rng.next_unit() - 0.5) * wander, (rng.next_unit() - 0.5) * wander);

        // A pour that is neither shrunk nor displaced would resample the
        // mask onto itself, so the whole step is skipped rather than
        // paying for an identity transform.
        let placed = if (pour.scale - 1.0).abs() < f32::EPSILON && jitter.x == 0.0 && jitter.y == 0.0 {
            None
        } else {
            Some(shrink(mask, width, height, support.centre, jitter, pour.scale))
        };
        let mut soft =
            image::blur(placed.as_deref().unwrap_or(mask), width, height, image::tuned(params.water, height));
        if params.sag {
            soft = sagged(&soft, width, height);
        }

        // The tide-line noise is one field for the whole sheet, so each
        // pour reads a different window of it rather than repeating the
        // last pour's edge.
        let window = ((rng.next_unit() * width as f32) as usize, (rng.next_unit() * height as f32) as usize);
        let alpha = self.threshold(&soft, params, support.centre, window);
        let interior =
            image::blur(&alpha, width, height, image::tuned(params.water * RIM_SPREAD + SUPPORT_MARGIN, height));

        // The rim reads a further-displaced window again, so the strength
        // varying along the tide line is not the same signal that decided
        // where the tide line went.
        for y in 0..height {
            let noise_row = ((y + window.1 * RIM_RESTRIDE.1) % height) * width;
            for x in 0..width {
                let i = y * width + x;
                let rim = (alpha[i] - interior[i]).max(0.0);
                let noise = self.edge_noise[noise_row + (x + window.0 * RIM_RESTRIDE.0) % width];
                let vary = (RIM_VARY.0 + noise * RIM_VARY.1).clamp(0.0, RIM_VARY_CEILING);
                let carried = support
                    .value
                    .as_ref()
                    .map_or(1.0, |value| (value[i] / support.reference[i].max(SUPPORT_FLOOR)).min(1.0));

                density[i] +=
                    alpha[i] * pour.body * params.load * carried + rim * vary * params.rim * params.load * RIM_GAIN;
            }
        }
    }

    /// Decide where the puddle's edge falls.
    ///
    /// The threshold is dithered by the sheet's own coarse noise, and one
    /// side of the region may be given up entirely: past the lost arc the
    /// hard edge fades out and what is left is a stain with no boundary at
    /// all, which is how a wash meets the paper when the painter stops
    /// asking it to stop.
    fn threshold(&self, soft: &[f32], params: &WashParams, centre: Vec2, offset: (usize, usize)) -> Vec<f32> {
        let (width, height) = (self.planes.width, self.planes.height);
        let mut alpha = vec![0.0; soft.len()];

        for y in 0..height {
            let noise_row = ((y + offset.1) % height) * width;
            for x in 0..width {
                let i = y * width + x;
                let shift = self.edge_noise[noise_row + (x + offset.0) % width] * params.wobble;
                let hard =
                    image::smoothstep(params.level - EDGE_BAND + shift, params.level + EDGE_BAND + shift, soft[i]);

                alpha[i] = params.lost.map_or(hard, |lost| {
                    let bearing = (y as f32 - centre.y).atan2(x as f32 - centre.x);
                    let away = (bearing - lost).abs();
                    let lostness = image::smoothstep(LOST_ARC.0, LOST_ARC.1, away.min(TAU - away));

                    // Clamped because the running-sum blur can leave a
                    // pixel a rounding error below zero, and a negative
                    // base under a fractional power is NaN — which then
                    // fails every later comparison silently and leaves a
                    // region-shaped hole of bare paper.
                    let stain = soft[i].max(0.0).powf(LOST_FALLOFF);

                    hard * (1.0 - lostness) + stain * LOST_STAIN * lostness
                });
            }
        }

        alpha
    }

    /// Settle the pigment into the paper's tooth.
    fn granulate(&self, density: &mut [f32], gran: f32) {
        for (at, &grain) in density.iter_mut().zip(&self.tooth) {
            if *at > GRANULATION_FLOOR {
                *at *= 1.0 - gran * GRANULATION_AUTHORITY * (grain - GRANULATION_PIVOT);
            }
        }
    }

    /// Throw drops off the brush, around and mostly below the region.
    fn spatter(&self, density: &mut [f32], centre: Vec2, drops: u32, rng: &mut Rng) {
        let (width, height) = (self.planes.width, self.planes.height);

        for _ in 0..drops {
            let bearing = rng.next_unit() * TAU;
            let throw = image::tuned(SPATTER_THROW.0 + rng.next_unit() * SPATTER_THROW.1, height);
            let at = Vec2::new(centre.x + bearing.cos() * throw, centre.y + bearing.sin() * throw * SPATTER_DROOP);

            // Squared so most drops are fine and the occasional one is a
            // blot, which is what comes off a loaded brush.
            let radius = image::tuned(SPATTER_RADIUS.0 + rng.next_unit() * rng.next_unit() * SPATTER_RADIUS.1, height);
            let strength = SPATTER_STRENGTH.0 + rng.next_unit() * SPATTER_STRENGTH.1;

            let x0 = (at.x - radius - 1.0).max(0.0) as usize;
            let x1 = ((at.x + radius + 1.0) as usize).min(width.saturating_sub(1));
            let y0 = (at.y - radius - 1.0).max(0.0) as usize;
            let y1 = ((at.y + radius + 1.0) as usize).min(height.saturating_sub(1));

            for y in y0..=y1 {
                for x in x0..=x1 {
                    let reach = Vec2::new(x as f32 - at.x, y as f32 - at.y).length();
                    if reach < radius {
                        density[y * width + x] += strength * (1.0 - reach / radius);
                    }
                }
            }
        }
    }
}

/// What every pour of one wash shares.
struct Support {
    centre: Vec2,
    /// The material's own value, softened to the wash's own water so it
    /// never reads as sharper than the wash carrying it.
    value: Option<Vec<f32>>,
    /// The region's coverage at the same radius, so the value can be read
    /// as a fraction of the support beneath it rather than as an absolute.
    reference: Vec<f32>,
}

/// Where the region sits, or `None` when nothing is covered at all.
fn centroid(mask: &[f32], width: usize) -> Option<Vec2> {
    let mut sum = Vec2::new(0.0, 0.0);
    let mut count = 0.0;

    for (i, &at) in mask.iter().enumerate() {
        if at > 0.5 {
            sum += Vec2::new((i % width) as f32, (i / width) as f32);
            count += 1.0;
        }
    }

    (count > 0.0).then(|| Vec2::new(sum.x / count, sum.y / count))
}

/// Resample the region smaller and off centre.
///
/// Shrinking about the centroid and then displacing is what makes the
/// pours siblings rather than rings: a concentric set of pours reads as a
/// target, and pooled water never does.
fn shrink(mask: &[f32], width: usize, height: usize, centre: Vec2, jitter: Vec2, scale: f32) -> Vec<f32> {
    let mut out = vec![0.0; mask.len()];

    for y in 0..height {
        let source_y = centre.y + (y as f32 - centre.y - jitter.y) / scale;
        for x in 0..width {
            let source_x = centre.x + (x as f32 - centre.x - jitter.x) / scale;
            out[y * width + x] = image::sample_bilinear(mask, width, height, source_x, source_y);
        }
    }

    out
}

/// Walk the stain downhill: two samples from above, each weaker than the
/// last, taken at their strongest so the wash grows downward and never
/// erases what it passes.
fn sagged(soft: &[f32], width: usize, height: usize) -> Vec<f32> {
    let step = image::tuned(SAG_STEP, height).round().max(1.0) as usize;
    let mut out = soft.to_vec();

    for y in 0..height {
        for x in 0..width {
            let i = y * width + x;
            for (drop, carried) in SAG_FALLOFF.iter().enumerate() {
                let above = (drop + 1) * step;
                if y >= above {
                    out[i] = out[i].max(soft[i - above * width] * carried);
                }
            }
        }
    }

    out
}

/// Which way a material gives up its far edge, in radians.
fn lost_angle(class: u8) -> f32 {
    LOST_RISE.atan2(if class == DRESS {
        LOST_RUN.0
    } else {
        LOST_RUN.1
    })
}

/// How closely the hand is held, everywhere on the sheet.
///
/// Distance from the drawn features, by chamfer transform: near the face
/// the painter cuts like gongbi, and past the fall of the hair the hand
/// relaxes into xieyi. The transition is the painting.
pub fn care_field(classes: &[u8], width: usize, height: usize) -> Vec<f32> {
    let features: Vec<bool> = classes.iter().map(|&at| matches!(at, LIPS | BROW | EYE)).collect();
    let distance = image::chamfer_distance(&features, width, height);
    let (far, near) = (image::tuned(CARE_FAR, height), image::tuned(CARE_NEAR, height));

    distance.iter().map(|&at| image::smoothstep(far, near, at)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::labels::SKIN;

    /// A small figure with a face, a fall of hair and a dress, which is
    /// enough of a subject for every branch of the wash to run.
    fn subject(width: usize, height: usize) -> (Vec<u8>, Vec<f32>, Vec<f32>) {
        let mut classes = vec![0u8; width * height];
        let mut tone = vec![0.0; width * height];

        for y in 0..height {
            for x in 0..width {
                let i = y * width + x;
                let across = x as f32 / width as f32;
                let down = y as f32 / height as f32;
                let over = |left: f32, right: f32, top: f32, bottom: f32| {
                    (left..right).contains(&across) && (top..bottom).contains(&down)
                };

                classes[i] = if over(0.35, 0.5, 0.15, 0.2) {
                    EYE
                } else if over(0.4, 0.6, 0.3, 0.35) {
                    LIPS
                } else if over(0.2, 0.8, 0.1, 0.4) {
                    SKIN
                } else if over(0.1, 0.9, 0.4, 0.75) {
                    HAIR
                } else if over(0.15, 0.85, 0.75, 1.0) {
                    DRESS
                } else {
                    0
                };
                tone[i] = (across + down) * 0.5;
            }
        }

        let facing = tone.clone();
        (classes, tone, facing)
    }

    fn painted(width: usize, height: usize, seed: u64) -> Vec<u8> {
        let (classes, tone, facing) = subject(width, height);
        let planes = Planes { classes: &classes, tone: &tone, facing: &facing, width, height };
        let sheet = Sheet::new(planes, seed);

        palette::composite(&sheet.coats(None, None), sheet.paper_shade())
    }

    /// Tripwire: the sheet is a pure function of its seed.
    ///
    /// Every accident in the engine — where a pour wanders, which stretch
    /// of tide line hardens, where a drop lands — is drawn from one
    /// counter. A single unseeded draw anywhere in that chain leaves the
    /// picture unreproducible while still looking entirely correct, so the
    /// same seed must give the same bytes and a different one must not.
    #[test]
    fn the_same_seed_paints_the_same_sheet() {
        let (width, height) = (90, 115);

        let once = painted(width, height, 0x5e_ed);
        assert_eq!(once, painted(width, height, 0x5e_ed), "a repainted sheet must be identical");
        assert_ne!(once, painted(width, height, 0x5e_ee), "and the seed must actually be reaching the accidents");
    }

    /// Tripwire: every coat is a number.
    ///
    /// A NaN density is the one failure this engine cannot show you: every
    /// comparison against it is false, so the pixel is quietly left
    /// unpainted and the result is a region-shaped hole of bare paper that
    /// reads as a reserve someone meant. One escaped through the lost
    /// edge's fractional power, over a blur residue a rounding error below
    /// zero, and painted a whole dress white.
    #[test]
    fn no_coat_carries_a_value_that_is_not_a_number() {
        let (width, height) = (90, 115);
        let (classes, tone, facing) = subject(width, height);
        let planes = Planes { classes: &classes, tone: &tone, facing: &facing, width, height };

        for coat in Sheet::new(planes, 0x5e_ed).coats(None, None) {
            assert!(coat.density.iter().all(|at| at.is_finite()), "class {} laid down a NaN", coat.class);
        }
    }

    /// Tripwire: an empty region paints nothing rather than dividing by a
    /// centroid it does not have.
    ///
    /// A class absent from the bake is normal — a figure can be framed
    /// without ears — and the centroid of an empty mask is the one input
    /// the wash cannot form.
    #[test]
    fn a_region_with_no_coverage_paints_nothing() {
        let (width, height) = (16, 16);
        let planes = Planes {
            classes: &vec![0u8; width * height],
            tone: &vec![0.5; width * height],
            facing: &vec![1.0; width * height],
            width,
            height,
        };
        let sheet = Sheet::new(planes, 1);

        let density = sheet.wash(&vec![0.0; width * height], None, &WashParams::loose(), &mut Rng::new(1));
        assert!(density.iter().all(|&at| at == 0.0), "an uncovered region must lay down no pigment");
    }

    /// Where a density field's pigment sits, weighted by how much of it is
    /// there — the region's own [`centroid`] counts pixels instead, and a
    /// stain has no threshold at which it starts counting.
    fn mass_centre(density: &[f32], width: usize) -> Vec2 {
        let mut sum = Vec2::new(0.0, 0.0);
        let mut total = 0.0;

        for (i, &at) in density.iter().enumerate() {
            sum += Vec2::new((i % width) as f32, (i / width) as f32) * at;
            total += at;
        }

        Vec2::new(sum.x / total, sum.y / total)
    }

    /// Tripwire: the stain drifts off the region it echoes, the way the
    /// region falls.
    ///
    /// Everything else in the atmosphere is a composition of primitives
    /// held elsewhere — the blur, the ramp, the wash. The displacement is
    /// the step this file owns, and it is read backwards from the plane it
    /// samples: the halo is fetched from where the stain came, not where
    /// it is going. Taken the wrong way round the stain is still soft,
    /// still spattered, still entirely plausible, and hanging off the
    /// opposite side of the head.
    #[test]
    fn the_atmosphere_stain_drifts_off_the_region_it_echoes() {
        let (width, height) = (240, 300);
        let mut classes = vec![0u8; width * height];
        for y in 60..140 {
            classes[y * width + 80..y * width + 160].fill(HAIR);
        }

        let tone = vec![0.5; width * height];
        let sheet = Sheet::new(Planes { classes: &classes, tone: &tone, facing: &tone, width, height }, 0x5e_ed);
        let mask = palette::mask_of(&classes, HAIR);
        let policy = palette::MATERIALS
            .iter()
            .find(|material| material.class == HAIR)
            .and_then(|material| material.atmosphere.as_ref())
            .expect("the hair carries an atmosphere policy");

        let stain = sheet.atmosphere(&mask, policy, &mut Rng::new(1));
        assert!(stain.iter().any(|&at| at > 0.0), "a region present on the sheet must leave something in the air");

        let (from, to) = (centroid(&mask, width).expect("the fixture region"), mass_centre(&stain, width));
        assert!(to.x < from.x, "the hair's stain drifts left across the sheet: {from:?} to {to:?}");
        assert!(to.y > from.y, "and down it: {from:?} to {to:?}");
    }
}
