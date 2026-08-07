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
//!
//! The box is per subject. A landscape's rock, soil and timber are not her
//! pigments, and a box compiled into the binary can only ever paint the one
//! subject it was mixed for — so a [`Palette`] is authored data that rides
//! in beside the mesh and the field, in the line-oriented record format
//! [`Palette::decode_text`] reads. [`Palette::canonical`] is the box this
//! crate was tuned on, and what a subject that names no palette gets.

use aether_math::{Vec2, Vec3};

use super::image;
use crate::labels::{self, CLASSES};

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

/// Names a box may give a material that no vocabulary class carries, and
/// the meta class each takes.
///
/// A meta-material's coverage comes from the chart, so its name is not in
/// the field's vocabulary and cannot be resolved against it. The set is
/// closed: a material naming neither a vocabulary class nor one of these is
/// an authoring error rather than a new feature.
const META_MATERIALS: [(&str, u8); 1] = [("iris", IRIS)];

/// Classes the wash's own grammar treats specially, by name.
///
/// A few marks belong to a named material rather than to any material:
/// only hair is wet enough to throw drops, take a violet glaze and be
/// brushed down its own locks; only a garment gives up its far edge on the
/// shorter run and wears less water than a fall of hair; only skin
/// flushes. Asked by name because a class id is a position in whichever
/// vocabulary the subject authored — key these by number and a hillside's
/// third class takes the glaze meant for her hair.
pub const SKIN_CLASS: &str = "skin";
pub const HAIR_CLASS: &str = "hair";
pub const DRESS_CLASS: &str = "dress";

/// The classes the face machinery paints and charts, by name.
///
/// Whether a subject has a face is a question about its vocabulary: a box
/// mixed for a hillside carries no eye, so the chart plants nothing, the
/// iris finds no coverage and the care field falls through to its own
/// authored source. Named rather than numbered because a class id means
/// only what the active vocabulary says it means.
pub const LIPS_CLASS: &str = "lips";
pub const BROW_CLASS: &str = "brow";
pub const EYE_CLASS: &str = "eye";
pub const FACE_CLASSES: [&str; 3] = [LIPS_CLASS, BROW_CLASS, EYE_CLASS];

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
///
/// Public because it bounds a contract outside this crate's own code:
/// the packed bake plane carries tone in an 8-bit unorm channel that
/// clips at one, which is lossless only while every resolved `lit` stays
/// below one. `tests/program_bake_scenario.rs` holds that, and
/// [`Palette::decode_text`] refuses an authored `lit` that would break it.
pub const LIT: f32 = 0.92;

/// Tone at which a material is fully in shadow.
const SHADOWED: f32 = 0.3;

/// The stain a material throws past its own silhouette.
///
/// A region's gesture does not stop at the line the drawing set, and a
/// wash that does reads as filled in rather than painted. Only a material
/// carrying enough water to throw a stain names a policy; the rest paint
/// to their own edges and the air around them stays bare paper.
#[derive(Clone, Debug, PartialEq)]
pub struct Atmosphere {
    /// How far the region's own coverage is spread before it is carried
    /// off the figure, in the pixels of the reference sheet. Far wider
    /// than any wash's water, because what is carried is the region's
    /// presence rather than its shape.
    pub halo: f32,
    /// Where that presence is carried, across the sheet and down it.
    ///
    /// Read through [`Atmosphere::carried`], never taken raw: how far the
    /// stain may actually run is a question about the halo as much as
    /// about the drift.
    pub drift: (f32, f32),
    /// The pigment the stain is left in, and how far it may build.
    /// Greyer than the pigment it echoes: this is the region seen through
    /// air rather than the region.
    pub pigment: u32,
    pub cap: f32,
}

impl Atmosphere {
    /// How far this stain is carried off the figure, in the texels of a
    /// plane `height` tall — the authored drift, held to the reach of the
    /// halo that softened it.
    ///
    /// The stain is a blur of the region's coverage read from one texel
    /// away and cut where the figure stands, so what the level cuts is a
    /// contour of that blur. While the run stays inside the halo's own
    /// reach, the contour it cuts still overlaps the region it came from:
    /// the mark hangs off her edge, thins as it goes, and reads as air.
    /// Carried further than the halo reaches, the same contour clears the
    /// region entirely and the cut lands on the region's own softened
    /// silhouette, whole and detached — a second figure standing beside
    /// the first rather than a stain (iamacoffeepot/aether#4468).
    ///
    /// So the run is held to the halo. The direction is the author's and
    /// is kept exactly; only its length is answerable to the softening,
    /// and it is held in the reference sheet's own pixels, before
    /// [`image::tuned`] takes it to this plane's — the relation between
    /// the two authored distances is a property of the palette rather
    /// than of whatever canvas is being painted.
    #[must_use]
    pub fn carried(&self, height: usize) -> Vec2 {
        let (across, down) = self.drift;
        let run = across.hypot(down);
        let held = if run > self.halo {
            self.halo / run
        } else {
            1.0
        };

        Vec2::new(image::tuned(across * held, height), image::tuned(down * held, height))
    }
}

/// One entry in the painter's box.
#[derive(Clone, Debug, PartialEq)]
pub struct Material {
    /// Which region class of the baked plane this material paints.
    pub class: u8,
    /// The class' own name, which is what the box calls this entry.
    pub name: String,
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

/// The painter's box, and the vocabulary it paints over.
///
/// The material order is the order the values were mixed in. Multiply
/// compositing commutes, so it carries no meaning for the colour — but the
/// accident stream is rolled material by material, so two boxes that name
/// the same entries in different orders are two different paintings, and
/// the order is authored rather than sorted.
#[derive(Clone, Debug, PartialEq)]
pub struct Palette {
    classes: Vec<String>,
    materials: Vec<Material>,
    /// Child to parent, in the order they were declared.
    remaps: Vec<(u8, u8)>,
    reserves: Vec<u8>,
    /// Where the hand's attention is anchored, in the subject's own space.
    ///
    /// Only a subject whose vocabulary carries no face needs one: R3 has
    /// the care field decay from the drawn features, and a face supplies
    /// those itself. A scene has no face to find, so it says where to
    /// look — the keep, the tower — and the care field radiates from
    /// there instead. Absent on a subject that says nothing, which is
    /// painted at one even looseness rather than refused.
    focus: Option<Vec3>,
}

/// The canonical box in the sidecar's own format.
///
/// The shipped default is built in Rust below, so a guest can never panic
/// resolving it — this is the same box written as a subject would author
/// it, and it is what documents the format. `tests` holds the two forms
/// against each other, which is what keeps the format able to express the
/// box this crate was tuned on.
pub const CANONICAL_TEXT: &str = "\
classes skin dress hair inner_ear tuft lips brow eye

material hair 0x2c3a5e 0.8 0.55 0.4
air hair 48 -68 92 0x4a5661 1
material dress 0x4a5661 0.5 0.5 0.3
material skin 0x96a0c8 0.32 0.15 0
lit skin 0.58
material inner_ear 0xd77fa1 0.5 0.2 0.45
small inner_ear
material tuft 0xd7a4b4 0.2 0.15 0.3
small tuft
material brow 0x6b5f56 0.22 0.1 0.6
small brow
material iris 0x3f7fd0 0.75 0.25 0.85
small iris

remap lips skin
reserve eye
";

impl Default for Palette {
    fn default() -> Self {
        Self::canonical()
    }
}

impl Palette {
    /// How many entries a box may hold.
    ///
    /// The accident stream is re-rolled whenever the visible set changes,
    /// and that set is carried as one bit per entry
    /// ([`Presence`](super::program::wash::Presence)). A vocabulary is
    /// already capped at [`CLASSES`] and each class takes at most one
    /// entry, so this binds only through the meta-materials — but it is
    /// the word that would silently wrap, so it is checked rather than
    /// reasoned about.
    pub const PRESENCE_LIMIT: usize = u16::BITS as usize;

    /// The box the crate was mixed for, in the order it was mixed.
    ///
    /// Built literally rather than decoded from [`CANONICAL_TEXT`] so
    /// resolving the default has no failure path at all: a subject that
    /// names no palette is the common case, and it cannot be the one that
    /// panics a guest.
    #[must_use]
    pub fn canonical() -> Self {
        use labels::{BROW, DRESS, HAIR, INNER_EAR, LIPS, SKIN, TUFT};

        let entry = |class: u8, name: &str, pigment: u32, load: f32, gran: f32, shade_floor: f32| Material {
            class,
            name: name.to_owned(),
            pigment,
            load,
            gran,
            shade_floor,
            shade_lit: None,
            small: false,
            atmosphere: None,
        };

        let mut hair = entry(HAIR, "hair", 0x2c_3a_5e, 0.8, 0.55, 0.4);
        // The one region wet enough to throw a stain, and it throws it
        // down and to the left, the way the hair itself falls. The grey
        // is the dress's own pigment rather than the hair's indigo: this
        // far off the figure the colour has gone out of it.
        hair.atmosphere = Some(Atmosphere { halo: 48.0, drift: (-68.0, 92.0), pigment: 0x4a_56_61, cap: 1.0 });

        let mut skin = entry(SKIN, "skin", 0x96_a0_c8, 0.32, 0.15, 0.0);
        skin.shade_lit = Some(0.58);

        // Both ear floors are the reference board's own values (issue
        // 4396). The parity drive had raised them (0.9 / 0.82, #4354) to
        // fill a white hole the value-carve left in the bowl — but the
        // raised floors painted the lit rim and tips at double the
        // approved strength, and the hole belongs to the tone plane at the
        // bowl, not to the floor.
        let mut inner_ear = entry(INNER_EAR, "inner_ear", 0xd7_7f_a1, 0.5, 0.2, 0.45);
        inner_ear.small = true;
        let mut tuft = entry(TUFT, "tuft", 0xd7_a4_b4, 0.2, 0.15, 0.3);
        tuft.small = true;
        let mut brow = entry(BROW, "brow", 0x6b_5f_56, 0.22, 0.1, 0.6);
        brow.small = true;

        // Last, and a meta-material: its coverage comes from the chart
        // rather than from the field, and everything else about it is an
        // entry in this box like any other. A high floor because an iris is
        // coloured wherever the light finds it — the reserve inside it is
        // the pupil, and that is cut out of the mask rather than lit out
        // of it.
        let mut iris = entry(IRIS, "iris", 0x3f_7f_d0, 0.75, 0.25, 0.85);
        iris.small = true;

        Self {
            classes: labels::CLASS_VOCABULARY.into_iter().map(str::to_owned).collect(),
            materials: vec![hair, entry(DRESS, "dress", 0x4a_56_61, 0.5, 0.5, 0.3), skin, inner_ear, tuft, brow, iris],
            remaps: vec![(LIPS, SKIN)],
            reserves: vec![labels::EYE],
            // She has a face, and a face anchors its own attention.
            focus: None,
        }
    }

    /// The class vocabulary this box paints over. Class id is the index
    /// plus one; cell `0` is unlabelled.
    #[must_use]
    pub fn classes(&self) -> &[String] {
        &self.classes
    }

    /// The box itself, in the order it was mixed.
    #[must_use]
    pub fn materials(&self) -> &[Material] {
        &self.materials
    }

    /// The class this vocabulary calls `name`, if it carries it at all.
    #[must_use]
    pub fn class_named(&self, name: &str) -> Option<u8> {
        labels::class_in(&self.classes, name)
    }

    /// Whether this box paints a face — whether the chart, the iris and
    /// the blush have any classes to work over.
    #[must_use]
    pub fn charts_a_face(&self) -> bool {
        FACE_CLASSES.iter().all(|&name| self.class_named(name).is_some())
    }

    /// The drawn features the hand is held tightest over: whichever of
    /// [`FACE_CLASSES`] this vocabulary carries.
    #[must_use]
    pub fn face_classes(&self) -> Vec<u8> {
        FACE_CLASSES.iter().filter_map(|&name| self.class_named(name)).collect()
    }

    /// The authored point the hand's attention is anchored at, in the
    /// subject's own space, for a subject with no face to find.
    ///
    /// Read through [`CareSource::resolve`](super::field::CareSource::resolve)
    /// rather than directly: a box that carries face classes anchors its
    /// attention on those, and this is the fall-back behind them.
    #[must_use]
    pub fn focus(&self) -> Option<Vec3> {
        self.focus
    }

    /// Where a region falls through when the paint policy names no
    /// material for it.
    ///
    /// No region ever paints nothing by accident: a class the box has no
    /// entry for takes its parent's wash instead of leaving a hole. The
    /// canonical box remaps the mouth to skin — it is drawn rather than
    /// painted, because in this grammar a mid tone is the paper's job — so
    /// the skin wash runs unbroken under the drawn line.
    #[must_use]
    pub fn remapped(&self, class: u8) -> u8 {
        self.remaps.iter().find(|&&(child, _)| child == class).map_or(class, |&(_, parent)| parent)
    }

    /// Coverage of one material over the region plane, counting the
    /// classes that fall through to it.
    #[must_use]
    pub fn mask_of(&self, classes: &[u8], class: u8) -> Vec<f32> {
        classes.iter().map(|&at| f32::from(self.remapped(at) == class)).collect()
    }

    /// The same coverage rule as a class bit set: `class` itself plus
    /// every class the box remaps onto it.
    ///
    /// What the GPU mask pass takes instead of an id. The fall-through
    /// table is the subject's, so a shader comparing one id could only
    /// ever apply the canonical remap — handing it the resolved set keeps
    /// the table on this side of the boundary, where it is authored.
    #[must_use]
    pub fn covered_by(&self, class: u8) -> u32 {
        let vocabulary = (1..=self.classes.len()).filter_map(|at| u8::try_from(at).ok());

        class_set(&vocabulary.filter(|&at| self.remapped(at) == class).collect::<Vec<_>>())
    }

    /// Decode and validate the authored palette format.
    ///
    /// Empty lines and `#` comments are harmless. Every other line is one
    /// of the format's records:
    ///
    /// ```text
    /// classes  <name>...                                  the field's vocabulary, class id = index + 1
    /// material <class> <pigment> <load> <gran> <floor>    one entry in the box, in mixing order
    /// lit      <class> <tone>                             the entry names its own fully-lit tone
    /// small    <class>                                    a region too small to loosen
    /// air      <class> <halo> <across> <down> <pigment> <cap>   what it leaves past its own edge
    /// remap    <child> <parent>                           the child takes the parent's wash
    /// reserve  <class>                                    paper, left bare
    /// focus    <x> <y> <z>                                where the hand's attention is anchored
    /// ```
    ///
    /// Every class the vocabulary declares must be painted, remapped or
    /// reserved exactly once: a class an author simply forgot would
    /// otherwise develop as a hole in the sheet with nothing raised
    /// anywhere.
    pub fn decode_text(text: &str) -> Result<Self, String> {
        let mut draft = Draft::default();

        for (line_index, line) in text.lines().enumerate() {
            let line_number = line_index + 1;
            let fields = line.split('#').next().unwrap_or(line).split_whitespace().collect::<Vec<_>>();
            if !fields.is_empty() {
                draft.record(&fields, line_number)?;
            }
        }

        let palette = Self {
            classes: draft.classes.ok_or_else(|| "palette names no classes".to_owned())?,
            materials: draft.materials,
            remaps: draft.remaps,
            reserves: draft.reserves,
            focus: draft.focus,
        };
        palette.validate()?;

        Ok(palette)
    }

    fn validate(&self) -> Result<(), String> {
        if self.materials.len() > Self::PRESENCE_LIMIT {
            return Err(format!(
                "palette mixes {} materials, but the box carries at most {}",
                self.materials.len(),
                Self::PRESENCE_LIMIT,
            ));
        }

        for material in &self.materials {
            if material.class >= META && material.atmosphere.is_some() {
                return Err(format!(
                    "palette gives '{}' a stain, but a meta-material's coverage comes from the chart and never \
                     stands in the class plane the stain is cut back against",
                    material.name,
                ));
            }
        }

        for (child, parent) in self.remaps.iter().copied() {
            if !self.materials.iter().any(|material| material.class == parent) {
                return Err(format!(
                    "palette remaps '{}' onto '{}', which carries no material of its own",
                    self.name_of(child),
                    self.name_of(parent),
                ));
            }
        }

        for (index, name) in self.classes.iter().enumerate() {
            let class = index as u8 + 1;
            let painted = self.materials.iter().any(|material| material.class == class);
            let remapped = self.remaps.iter().any(|&(child, _)| child == class);
            let reserved = self.reserves.contains(&class);
            match usize::from(painted) + usize::from(remapped) + usize::from(reserved) {
                1 => {}
                0 => {
                    return Err(format!(
                        "palette class '{name}' is neither painted, remapped nor reserved, so it would develop as a \
                         hole in the sheet",
                    ));
                }
                _ => {
                    return Err(format!(
                        "palette class '{name}' is painted, remapped or reserved more than once; a class takes exactly \
                         one of the three",
                    ));
                }
            }
        }

        Ok(())
    }

    /// What the box calls a class, for a diagnostic. Meta-materials are
    /// outside the vocabulary, so they answer with their own entry's name.
    fn name_of(&self, class: u8) -> String {
        self.classes
            .get(usize::from(class).wrapping_sub(1))
            .cloned()
            .or_else(|| self.materials.iter().find(|material| material.class == class).map(|at| at.name.clone()))
            .unwrap_or_else(|| class.to_string())
    }
}

/// One palette under construction, record by record.
///
/// Split from [`Palette`] because the two are different things: this one is
/// half-built by definition — the vocabulary may not have arrived yet, and a
/// class may still be waiting for the policy that will name it — while a
/// `Palette` is the validated whole. Keeping the partial state out of the
/// finished type is what lets every field of the finished one be trusted.
#[derive(Default)]
struct Draft {
    classes: Option<Vec<String>>,
    materials: Vec<Material>,
    remaps: Vec<(u8, u8)>,
    reserves: Vec<u8>,
    focus: Option<Vec3>,
}

impl Draft {
    /// Take one non-empty line of the format.
    fn record(&mut self, fields: &[&str], line: usize) -> Result<(), String> {
        match fields {
            ["classes", names @ ..] if !names.is_empty() => self.classes(names, line),
            ["material", name, pigment, load, gran, floor] => self.material(name, [pigment, load, gran, floor], line),
            ["lit", name, tone] => self.lit(name, tone, line),
            ["small", name] => {
                self.decorated(name, line, "small")?.small = true;
                Ok(())
            }
            ["air", name, halo, across, down, pigment, cap] => self.air(name, [halo, across, down, pigment, cap], line),
            ["remap", child, parent] => self.remap(child, parent, line),
            ["reserve", name] => self.reserve(name, line),
            ["focus", x, y, z] => self.focus(x, y, z, line),
            [record, ..] if RECORDS.contains(record) => Err(format!("palette line {line} has malformed {record} data")),
            [record, ..] => Err(format!("palette line {line} has unknown record '{record}'")),
            [] => unreachable!("empty palette lines are skipped"),
        }
    }

    fn classes(&mut self, names: &[&str], line: usize) -> Result<(), String> {
        if self.classes.is_some() {
            return Err(format!("palette line {line} repeats the classes table"));
        }
        if names.len() > CLASSES {
            return Err(format!(
                "palette line {line} names {} classes, but a field cell carries at most {CLASSES}",
                names.len(),
            ));
        }

        let mut declared: Vec<String> = Vec::with_capacity(names.len());
        for &name in names {
            if declared.iter().any(|earlier| earlier == name) {
                return Err(format!("palette line {line} repeats class '{name}'"));
            }
            if META_MATERIALS.iter().any(|&(meta, _)| meta == name) {
                return Err(format!(
                    "palette line {line} names class '{name}', which the box reserves for a meta-material the chart \
                     supplies",
                ));
            }
            declared.push(name.to_owned());
        }
        self.classes = Some(declared);

        Ok(())
    }

    fn material(&mut self, name: &str, values: [&str; 4], line: usize) -> Result<(), String> {
        let [pigment, load, gran, floor] = values;
        let class = self.class(name, line)?;
        if self.materials.iter().any(|material| material.class == class) {
            return Err(format!("palette line {line} repeats a material for class '{name}'"));
        }

        self.materials.push(Material {
            class,
            name: name.to_owned(),
            pigment: palette_pigment(pigment, line, "material")?,
            load: palette_share(load, line, "load")?,
            gran: palette_share(gran, line, "gran")?,
            shade_floor: palette_share(floor, line, "shade_floor")?,
            shade_lit: None,
            small: false,
            atmosphere: None,
        });

        Ok(())
    }

    /// The tone at which one entry counts as fully lit.
    ///
    /// Held below one because the packed bake plane carries tone in an
    /// 8-bit unorm channel that clips there (see [`LIT`]): a material fully
    /// lit at one could never be told from one lit past the plane's own
    /// ceiling.
    fn lit(&mut self, name: &str, tone: &str, line: usize) -> Result<(), String> {
        let tone = palette_float(tone, line, "lit")?;
        if !(0.0..1.0).contains(&tone) {
            return Err(format!(
                "palette line {line} lit is {tone}, expected a tone from 0 up to but not including 1, which is where \
                 the packed tone plane clips",
            ));
        }
        self.decorated(name, line, "lit")?.shade_lit = Some(tone);

        Ok(())
    }

    fn air(&mut self, name: &str, values: [&str; 5], line: usize) -> Result<(), String> {
        let [halo, across, down, pigment, cap] = values;
        let halo = palette_float(halo, line, "air")?;
        if halo <= 0.0 {
            return Err(format!(
                "palette line {line} gives '{name}' a halo of {halo}, expected one wider than nothing"
            ));
        }
        let policy = Atmosphere {
            halo,
            drift: (palette_float(across, line, "air")?, palette_float(down, line, "air")?),
            pigment: palette_pigment(pigment, line, "air")?,
            cap: palette_share(cap, line, "cap")?,
        };

        if self.decorated(name, line, "air")?.atmosphere.replace(policy).is_some() {
            return Err(format!("palette line {line} repeats air for '{name}'"));
        }

        Ok(())
    }

    fn remap(&mut self, child: &str, parent: &str, line: usize) -> Result<(), String> {
        let (child_class, parent_class) = (self.class(child, line)?, self.class(parent, line)?);
        if child_class == parent_class {
            return Err(format!("palette line {line} remaps '{child}' onto itself"));
        }
        if self.remaps.iter().any(|&(at, _)| at == child_class) {
            return Err(format!("palette line {line} repeats a remap for '{child}'"));
        }
        self.remaps.push((child_class, parent_class));

        Ok(())
    }

    fn reserve(&mut self, name: &str, line: usize) -> Result<(), String> {
        let class = self.class(name, line)?;
        if self.reserves.contains(&class) {
            return Err(format!("palette line {line} repeats reserve '{name}'"));
        }
        self.reserves.push(class);

        Ok(())
    }

    /// Where the hand's attention is anchored, in the subject's own space.
    fn focus(&mut self, x: &str, y: &str, z: &str, line: usize) -> Result<(), String> {
        let at = Vec3::new(
            palette_float(x, line, "focus")?,
            palette_float(y, line, "focus")?,
            palette_float(z, line, "focus")?,
        );
        if self.focus.replace(at).is_some() {
            return Err(format!("palette line {line} repeats the focus"));
        }

        Ok(())
    }

    /// The class a record names: a vocabulary entry, or one of the closed
    /// set of meta-materials the chart supplies coverage for.
    fn class(&self, name: &str, line: usize) -> Result<u8, String> {
        if let Some(&(_, class)) = META_MATERIALS.iter().find(|&&(meta, _)| meta == name) {
            return Ok(class);
        }
        let classes = self
            .classes
            .as_deref()
            .ok_or_else(|| format!("palette line {line} names class '{name}' before the classes table"))?;

        labels::class_in(classes, name)
            .ok_or_else(|| format!("palette line {line} names class '{name}', which the vocabulary does not carry"))
    }

    /// The declared material a decorating record applies to.
    fn decorated(&mut self, name: &str, line: usize, record: &str) -> Result<&mut Material, String> {
        let class = self.class(name, line)?;

        self.materials.iter_mut().find(|material| material.class == class).ok_or_else(|| {
            format!("palette line {line} gives {record} to '{name}', which the box carries no material for")
        })
    }
}

/// Every record the format declares, for the malformed-versus-unknown
/// split: a line whose first field is one of these got its own record
/// wrong, and anything else is not a record at all.
const RECORDS: [&str; 8] = ["classes", "material", "lit", "small", "air", "remap", "reserve", "focus"];

/// A set of class ids as one bit per id — how a class predicate crosses
/// into a shader, where the vocabulary itself cannot go.
///
/// Ids past the word carry no bit. A meta-material's id is the only one
/// that reaches there, and it never appears in a class plane, so the set
/// it would join is one nothing tests it against.
#[must_use]
pub fn class_set(classes: &[u8]) -> u32 {
    classes.iter().filter(|&&class| class < u32::BITS as u8).fold(0, |set, &class| set | 1 << class)
}

fn palette_float(value: &str, line: usize, record: &str) -> Result<f32, String> {
    value
        .parse::<f32>()
        .ok()
        .filter(|value| value.is_finite())
        .ok_or_else(|| format!("palette line {line} has non-finite or malformed {record} number '{value}'"))
}

/// A quantity the wash reads as a fraction of its whole, which is every
/// authored strength in the box.
fn palette_share(value: &str, line: usize, record: &str) -> Result<f32, String> {
    let value = palette_float(value, line, record)?;
    if !(0.0..=1.0).contains(&value) {
        return Err(format!("palette line {line} has {record} {value}, expected a share from 0 to 1"));
    }

    Ok(value)
}

/// A packed `0xRRGGBB` pigment, in the form the box is authored in.
fn palette_pigment(value: &str, line: usize, record: &str) -> Result<u32, String> {
    value
        .strip_prefix("0x")
        .filter(|digits| digits.len() == 6)
        .and_then(|digits| u32::from_str_radix(digits, 16).ok())
        .ok_or_else(|| format!("palette line {line} has {record} pigment '{value}', expected a packed 0xRRGGBB"))
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
        .map(|&at| material.shade_floor + (1.0 - material.shade_floor) * image::smoothstep(lit, SHADOWED, at))
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
    use crate::labels::{HAIR, LIPS, SKIN};

    /// A two-class box for a subject that is not her: no face, no
    /// meta-material, its own pigments.
    fn hillside() -> Palette {
        Palette::decode_text(
            "classes rock grass\n\
             material rock 0x7a6f63 0.4 0.6 0.35\n\
             material grass 0x5c7a4a 0.3 0.4 0.2\n",
        )
        .expect("the hillside box is well formed")
    }

    /// Tripwire: the authored format can express the box this crate was
    /// tuned on, exactly.
    ///
    /// The built-in default is Rust rather than text so resolving it
    /// cannot fail, which leaves the two forms free to drift — a record
    /// the decoder mis-parses, a field the format cannot carry, a mixing
    /// order the text reverses. Any of those makes a sidecar unable to
    /// reproduce the canonical subject while every other test still
    /// passes, because every other test uses the Rust form.
    #[test]
    fn the_authored_format_expresses_the_canonical_box() {
        assert_eq!(Palette::decode_text(CANONICAL_TEXT), Ok(Palette::canonical()));
    }

    /// Tripwire: a region with no material of its own is painted by its
    /// parent rather than left bare.
    ///
    /// The failure this guards is silent and specific — the mouth region
    /// stops matching any entry in the box, nothing errors, and the skin
    /// wash simply develops with a mouth-shaped hole in it.
    #[test]
    fn a_remapped_region_paints_with_its_parent() {
        let palette = Palette::canonical();
        let classes = [SKIN, LIPS, HAIR, 0];
        let skin = palette.mask_of(&classes, SKIN);

        assert_eq!(skin, [1.0, 1.0, 0.0, 0.0], "lips must fall through into the skin wash");
        assert_eq!(palette.mask_of(&classes, LIPS), [0.0; 4], "and must not also paint themselves");
    }

    /// Tripwire: no stain in the box outruns the halo that softened it.
    ///
    /// The failure this guards raises nothing and looks like a rendering
    /// bug in some other layer: a stain carried past its own halo cuts a
    /// contour that has cleared the region entirely, so the sheet
    /// develops a detached copy of the figure's silhouette standing
    /// beside her (iamacoffeepot/aether#4468). The reach is checked at
    /// the reference sheet's own height, which is where both distances
    /// were authored.
    #[test]
    fn a_stain_never_outruns_its_own_halo() {
        let height = image::TUNED_HEIGHT as usize;
        for material in Palette::canonical().materials() {
            let Some(policy) = material.atmosphere.as_ref() else {
                continue;
            };
            let carried = policy.carried(height);
            let reach = image::tuned(policy.halo, height);

            assert!(
                carried.x.hypot(carried.y) <= reach + 1e-3,
                "{}: carried {carried:?} past a halo reaching {reach}",
                material.name,
            );
        }
    }

    /// Tripwire: the hold above is doing work, and the direction it holds
    /// in is the author's.
    ///
    /// A clamp that never binds is indistinguishable from no clamp, so
    /// this pins that the box as authored does ask to be held — the
    /// hair's drift runs more than twice its halo — and that holding it
    /// shortens the run without turning it.
    #[test]
    fn the_hold_binds_on_the_box_as_authored_and_keeps_its_bearing() {
        let palette = Palette::canonical();
        let hair = palette.materials().iter().find(|material| material.class == HAIR).expect("the hair is in the box");
        let policy = hair.atmosphere.as_ref().expect("the hair throws a stain");
        let (across, down) = policy.drift;

        assert!(across.hypot(down) > policy.halo, "the authored drift must outrun the halo for the hold to bind");

        let carried = policy.carried(image::TUNED_HEIGHT as usize);
        assert!(
            (carried.x * down - carried.y * across).abs() < 1e-3,
            "the hold shortens the run, it does not turn it: {carried:?} left the authored bearing",
        );
    }

    /// Tripwire: a class an author left out of every policy is refused
    /// rather than developed as a hole.
    ///
    /// This is the one authoring mistake the sheet cannot report itself:
    /// an unlisted class simply matches no material's mask, and the wash
    /// develops around it in silence.
    #[test]
    fn a_class_no_policy_names_is_refused() {
        let refused = Palette::decode_text("classes rock grass\nmaterial rock 0x7a6f63 0.4 0.6 0.35\n")
            .expect_err("grass carries no policy at all");

        assert!(refused.contains("grass"), "the diagnostic must name the class that was left out: {refused}");
    }

    /// Tripwire: a box mixed for something that is not her charts no
    /// face, whatever ids its own classes happen to take.
    ///
    /// Class ids are positions in a vocabulary, so a two-class hillside
    /// carries classes 1 and 2 exactly as she carries skin and dress. A
    /// face gate that compared ids rather than names would plant her eyes
    /// on the rock.
    #[test]
    fn a_box_without_face_classes_charts_no_face() {
        let hillside = hillside();

        assert!(!hillside.charts_a_face(), "a hillside has no eye to chart");
        assert_eq!(hillside.face_classes(), Vec::<u8>::new());
        assert!(Palette::canonical().charts_a_face(), "she does");
    }

    /// Tripwire: the meta-material's id stays clear of every vocabulary
    /// class, so a coat's class names exactly one thing.
    #[test]
    fn a_meta_material_never_collides_with_a_field_class() {
        let hillside = hillside();
        let painted: Vec<u8> = hillside.materials().iter().map(|material| material.class).collect();

        assert_eq!(painted, [1, 2], "a two-class box paints classes 1 and 2");
        assert!(META > CLASSES as u8, "no vocabulary class can reach the meta range");
    }
}
