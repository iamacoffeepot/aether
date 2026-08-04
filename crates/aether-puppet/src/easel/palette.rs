//! What each material is made of, and how coats of it darken the paper.
//!
//! The table is the painter's box: a pigment, how much of it the brush
//! carries, whether it granulates, and how far into the light it is still
//! present. Compositing is subtractive throughout — every coat multiplies
//! the light coming back off the sheet, and the sheet's own white is the
//! only white there is, because watercolour has no white paint.
//!
//! Which is why two of the face's regions have no entry here at all. The
//! sclera and the pupil are **reserves**: paper, left bare, because the
//! only way to paint them would be to paint white. The sclera reserves
//! itself by carrying no material of its own; the pupil is cut out of the
//! iris' mask, so the wash stops at the slit the ink drew instead of
//! filling it.

use crate::labels::{BROW, DRESS, HAIR, INNER_EAR, LIPS, SKIN, TUFT};

/// The paper's own colour, in 8-bit channels. Everything painted is this
/// multiplied down.
pub const PAPER: [f32; 3] = [246.0, 242.0, 233.0];

/// First class id belonging to a **meta-material** — a painted feature the
/// [`chart`](crate::chart) owns rather than the label field.
///
/// Past the field's own range on purpose, so a meta-material and a labelled
/// one can never collide in the class plane and a coat's class still names
/// exactly one thing.
pub const META: u8 = 100;

/// The iris. Painted through the same pours, rim and granulation as any
/// other material, over a mask the chart's own eye frame supplies.
pub const IRIS: u8 = META;

/// The blush, and how far it may build.
///
/// Skin's own flush rather than a pigment of its own — the same rose the
/// inner ear takes, which is the other place blood sits near the surface.
pub const BLUSH_PIGMENT: u32 = 0xd7_7f_a1;
pub const BLUSH_CAP: f32 = 0.7;

/// Ceiling on how much pigment one coat may deposit at a pixel.
///
/// The drawing is composited over the finished wash, so a wash allowed to
/// reach full opacity would swallow its own line work. Capping short of
/// that keeps the ink reading through the darkest pour.
pub const DENSITY_CAP: f32 = 1.4;

/// Darkest a pigment channel may be taken.
///
/// A channel at zero is an absorber no number of thin glazes can lift, and
/// raising it to a real pigment's floor is what keeps deep shadow coloured
/// rather than black.
const PIGMENT_FLOOR: f32 = 0.02;

/// Below this a coat is doing nothing the eye can find, so it is skipped
/// rather than run through a per-channel power.
const MINIMUM_DEPOSIT: f32 = 0.002;

/// Tone at which a material is fully lit, unless it names its own. The
/// GPU shade pass takes the resolved value through its uniforms, so the
/// dispatch encoder reads this too.
pub(crate) const LIT: f32 = 0.92;

/// Tone at which a material is fully in shadow.
const SHADOWED: f32 = 0.3;

/// The stain a material throws past its own silhouette.
///
/// A region's gesture does not stop at the line the drawing set, and a
/// wash that does reads as filled in rather than painted. Only a material
/// carrying enough water to throw a stain names a policy; the rest paint
/// to their own edges and the air around them stays bare paper.
pub struct Atmosphere {
    /// How far the region's own coverage is spread before it is carried
    /// off the figure, in the pixels of the reference sheet. Far wider
    /// than any wash's water, because what is carried is the region's
    /// presence rather than its shape.
    pub halo: f32,
    /// Where that presence is carried, across the sheet and down it.
    pub drift: (f32, f32),
    /// The pigment the stain is left in, and how far it may build.
    /// Greyer than the pigment it echoes: this is the region seen through
    /// air rather than the region.
    pub pigment: u32,
    pub cap: f32,
}

/// One entry in the painter's box.
pub struct Material {
    /// Which region class of the baked plane this material paints.
    pub class: u8,
    pub name: &'static str,
    /// Packed `0xRRGGBB`, the form these were authored and tuned in.
    pub pigment: u32,
    /// How much pigment the brush carries — the wash's overall strength.
    pub load: f32,
    /// How strongly the pigment settles into the paper's tooth. Staining
    /// dyes barely do; an earth or a mineral does a great deal.
    pub gran: f32,
    /// How much of the material survives full light. Zero means the
    /// highlight is bare paper, which is what skin wants.
    pub shade_floor: f32,
    /// Tone at which this material counts as fully lit. Skin reserves far
    /// earlier than everything else, so it names its own.
    pub shade_lit: Option<f32>,
    /// A region too small to loosen. The hand stays tight over a feature
    /// however far it sits from the face.
    pub small: bool,
    /// What this material leaves in the air past its own edge, if
    /// anything.
    pub atmosphere: Option<Atmosphere>,
}

/// The painter's box, in the order it was mixed.
///
/// Multiply compositing commutes, so the order carries no meaning for the
/// result — it is the order the values were tuned in, kept so the table
/// still reads as the palette it was authored as.
pub const MATERIALS: &[Material] = &[
    Material {
        class: HAIR,
        name: "hair",
        pigment: 0x2c_3a_5e,
        load: 0.8,
        gran: 0.55,
        shade_floor: 0.4,
        shade_lit: None,
        small: false,
        // The one region wet enough to throw a stain, and it throws it
        // down and to the left, the way the hair itself falls. The grey
        // is the dress's own pigment rather than the hair's indigo: this
        // far off the figure the colour has gone out of it.
        atmosphere: Some(Atmosphere { halo: 48.0, drift: (-68.0, 92.0), pigment: 0x4a_56_61, cap: 1.0 }),
    },
    Material {
        class: DRESS,
        name: "dress",
        pigment: 0x4a_56_61,
        load: 0.5,
        gran: 0.5,
        shade_floor: 0.3,
        shade_lit: None,
        small: false,
        atmosphere: None,
    },
    Material {
        class: SKIN,
        name: "skin",
        pigment: 0x96_a0_c8,
        load: 0.32,
        gran: 0.15,
        shade_floor: 0.0,
        shade_lit: Some(0.58),
        small: false,
        atmosphere: None,
    },
    // Both ear floors are the reference board's own values (issue 4396).
    // The parity drive had raised them (0.9 / 0.82, #4354) to fill a
    // white hole the value-carve left in the bowl — but the raised floors
    // painted the lit rim and tips at double the approved strength, and
    // the hole belongs to the tone plane at the bowl, not to the floor.
    Material {
        class: INNER_EAR,
        name: "inner ear",
        pigment: 0xd7_7f_a1,
        load: 0.5,
        gran: 0.2,
        shade_floor: 0.45,
        shade_lit: None,
        small: true,
        atmosphere: None,
    },
    Material {
        class: TUFT,
        name: "ear tuft",
        pigment: 0xd7_a4_b4,
        load: 0.2,
        gran: 0.15,
        shade_floor: 0.3,
        shade_lit: None,
        small: true,
        atmosphere: None,
    },
    Material {
        class: BROW,
        name: "brow ridge (under)",
        pigment: 0x6b_5f_56,
        load: 0.22,
        gran: 0.1,
        shade_floor: 0.6,
        shade_lit: None,
        small: true,
        atmosphere: None,
    },
    // Last, and a meta-material: its coverage comes from the chart rather
    // than from the field, and everything else about it is an entry in
    // this box like any other. A high floor because an iris is coloured
    // wherever the light finds it — the reserve inside it is the pupil,
    // and that is cut out of the mask rather than lit out of it.
    Material {
        class: IRIS,
        name: "iris (meta)",
        pigment: 0x3f_7f_d0,
        load: 0.75,
        gran: 0.25,
        shade_floor: 0.85,
        shade_lit: None,
        small: true,
        atmosphere: None,
    },
];

/// Where a region falls through when the paint policy names no material
/// for it.
///
/// No region ever paints nothing by accident: a class the box has no entry
/// for takes its parent's wash instead of leaving a hole. The mouth is
/// drawn rather than painted — in this grammar a mid tone is the paper's
/// job — so it remaps to skin and the skin wash runs unbroken under the
/// drawn line.
pub fn remapped(class: u8) -> u8 {
    if class == LIPS {
        SKIN
    } else {
        class
    }
}

/// Coverage of one material over the region plane, counting the classes
/// that fall through to it.
pub fn mask_of(classes: &[u8], class: u8) -> Vec<f32> {
    classes.iter().map(|&at| f32::from(remapped(at) == class)).collect()
}

/// How much of a material's wash survives at each pixel, given the light.
///
/// Value decides coverage: pigment pools where the key light does not
/// reach and paper is reserved where it lands. The floor keeps a
/// material's identity present even in full light, and drops to zero for
/// skin, which is almost entirely reserve.
pub fn shade_of(material: &Material, tone: &[f32]) -> Vec<f32> {
    let lit = material.shade_lit.unwrap_or(LIT);

    tone.iter()
        .map(|&at| material.shade_floor + (1.0 - material.shade_floor) * super::image::smoothstep(lit, SHADOWED, at))
        .collect()
}

/// One pass of one pigment over the whole sheet.
///
/// A material lays down one of these; a glaze dropped into a wash still
/// wet lays down another over the same region, which is why a coat carries
/// its own pigment and cap rather than deferring to the table.
pub struct Coat {
    /// Region class this coat belongs to, so a caller can find the coat it
    /// wants to treat further.
    pub class: u8,
    /// Packed `0xRRGGBB`.
    pub pigment: u32,
    /// Ceiling on this coat's deposit — see [`DENSITY_CAP`].
    pub cap: f32,
    pub density: Vec<f32>,
}

/// Composite every coat against paper white.
///
/// `paper_shade` is the sheet's own tooth and mottle as a multiplier
/// around 1 (see [`super::field::Sheet`]), so the paper keeps its grain
/// through the bare passages where nothing was painted at all. The result
/// is straight RGBA8; the drawing multiplies over it afterwards.
pub fn composite(coats: &[Coat], paper_shade: &[f32]) -> Vec<u8> {
    let mut through = vec![[1.0f32; 3]; paper_shade.len()];

    for coat in coats {
        let pigment = channels(coat.pigment);
        for (light, &density) in through.iter_mut().zip(coat.density.iter()) {
            let deposit = density.min(coat.cap);
            if deposit > MINIMUM_DEPOSIT {
                for (channel, absorbed) in light.iter_mut().zip(pigment) {
                    *channel *= absorbed.powf(deposit);
                }
            }
        }
    }

    let mut rgba = Vec::with_capacity(paper_shade.len() * 4);
    for (light, &shade) in through.iter().zip(paper_shade) {
        for (channel, paper) in light.iter().zip(PAPER) {
            rgba.push((paper * channel * shade).round().clamp(0.0, 255.0) as u8);
        }
        rgba.push(u8::MAX);
    }

    rgba
}

/// A packed pigment as three transmission factors.
pub fn channels(pigment: u32) -> [f32; 3] {
    [16, 8, 0].map(|shift| (((pigment >> shift) & 0xff) as f32 / 255.0).max(PIGMENT_FLOOR))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tripwire: a region with no material of its own is painted by its
    /// parent rather than left bare.
    ///
    /// The failure this guards is silent and specific — the mouth region
    /// stops matching any entry in the box, nothing errors, and the skin
    /// wash simply develops with a mouth-shaped hole in it.
    #[test]
    fn a_remapped_region_paints_with_its_parent() {
        let classes = [SKIN, LIPS, HAIR, 0];
        let skin = mask_of(&classes, SKIN);

        assert_eq!(skin, [1.0, 1.0, 0.0, 0.0], "lips must fall through into the skin wash");
        assert_eq!(mask_of(&classes, LIPS), [0.0; 4], "and must not also paint themselves");
    }
}
