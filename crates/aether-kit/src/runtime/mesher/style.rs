// The style layer works in continuous f32 color space; the coordinate and
// index casts (world cells to f32, noise bytes to array indices) are
// small integers whose precision / sign / truncation the pedantic set
// flags as non-issues in this bounded domain.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap
)]
// Color and value-noise math reads clearest with the conventional
// single-letter channel and lattice-corner names (h/s/l, a/b/c/d).
#![allow(clippy::many_single_char_names)]
// The noise and color arithmetic is written as explicit multiply-add
// chains for readability; a fused mul_add would need a libm symbol on
// the wasm target and does not change the result meaningfully here.
#![allow(clippy::suboptimal_flops)]
// The water lightness branches on presence of a depth value; a `match`
// reads clearer than the map_or_else the nursery lint prefers.
#![allow(clippy::option_if_let_else)]

//! The material style table and the deterministic color field that paints
//! each cell.
//!
//! A [`MaterialStyle`] row carries everything a material needs to render:
//! its base color in HSL, per-channel noise amplitudes, the value-noise
//! field shape (wavelength / octaves / persistence), a seed offset, the
//! stroke flow-field wavelength, the corner-smoothing setting its overlay
//! contours use, and the rim / wash / water tunables. [`StyleTable`] holds
//! one row per [`Material`] as runtime state — [`StyleTable::get`] reads a
//! row, [`StyleTable::apply`] overwrites one live from a
//! `aether.kit.world.set_material_style` mail — so a tuning pass needs no
//! rebuild.
//!
//! A cell's color resolves from its world coordinates alone: base HSL plus
//! three world-anchored fractal value-noise samples (one per channel,
//! independently seeded), converted to linear RGB. Because the sample is a
//! pure function of the integer world position and the material seed, two
//! chunks that share a border cell resolve it to the same color with no
//! shared state.

use core::f32::consts::FRAC_1_SQRT_2;

use crate::world::{MAX_SMOOTHING_ITERATIONS, Material, SetMaterialStyle};

/// One material's complete render style. Indexed by [`Material`] through
/// [`StyleTable::get`]; the [`Material::Void`] row is a placeholder that
/// is never painted.
pub struct MaterialStyle {
    /// Base hue in degrees `[0, 360)`.
    pub base_hue: f32,
    /// Base saturation in percent `[0, 100]`.
    pub base_sat: f32,
    /// Base lightness in percent `[0, 100]`.
    pub base_light: f32,
    /// Peak hue deviation in degrees the noise field adds.
    pub amp_hue: f32,
    /// Peak saturation deviation in percent.
    pub amp_sat: f32,
    /// Peak lightness deviation in percent.
    pub amp_light: f32,
    /// Noise wavelength in cells — the world distance over one lattice
    /// period at the base octave.
    pub wavelength: f32,
    /// Fractal octave count.
    pub octaves: u32,
    /// Per-octave amplitude falloff (lacunarity is fixed at 2).
    pub persistence: f32,
    /// Seed offset folded into every channel so each material keys its own
    /// decorrelated field.
    pub seed_offset: u32,
    /// Wavelength in cells of the stroke flow field the wash grades along.
    pub flow_wavelength: f32,
    /// Corner-smoothing angle in degrees for this material's overlay
    /// contours (`45` chamfers hardest, `90` only true right-angle
    /// corners).
    pub smoothing_degrees: u32,
    /// Corner-smoothing iteration count (`0` = raw blocky contours).
    pub smoothing_iterations: u32,
    /// Rim inset in octimeters — the width of a pooled edge strip.
    pub rim_inset_octimeters: i32,
    /// Rim lightness darkening `[0, 1]` where the paint pools.
    pub rim_darken: f32,
    /// Wash lightness gradient depth `[0, 1]` along the stroke direction.
    pub wash_grade: f32,
    /// Water lightness reduction in percent at full depth.
    pub water_depth_darken: f32,
    /// Blob-merge hue-step threshold in degrees: same-material cells whose
    /// resolved hue differs by more than this pool a rim between them.
    pub blob_merge_degrees: f32,
}

/// Per-material style rows. Base colors are the HSL of the ground palette's
/// sRGB design values (Grass `(0.30, 0.55, 0.25)`, Dirt
/// `(0.45, 0.32, 0.18)`, Stone `(0.55, 0.55, 0.58)`, Sand
/// `(0.85, 0.78, 0.55)`, Water `(0.20, 0.40, 0.70)`). Saturation
/// amplitude is zero on every row: a desaturated cell reads as a foreign
/// gray patch rather than a variant of the material, so the field varies
/// hue and lightness only.
const STYLES: [MaterialStyle; 6] = [
    // Void — never painted.
    MaterialStyle {
        base_hue: 0.0,
        base_sat: 0.0,
        base_light: 0.0,
        amp_hue: 0.0,
        amp_sat: 0.0,
        amp_light: 0.0,
        wavelength: 6.0,
        octaves: 2,
        persistence: 0.45,
        seed_offset: 0,
        flow_wavelength: 4.0,
        smoothing_degrees: 90,
        smoothing_iterations: 0,
        rim_inset_octimeters: 32,
        rim_darken: 0.15,
        wash_grade: 0.10,
        water_depth_darken: 0.0,
        blob_merge_degrees: 6.0,
    },
    // Grass — hsl(110, 37.5, 40).
    MaterialStyle {
        base_hue: 110.0,
        base_sat: 37.5,
        base_light: 40.0,
        amp_hue: 8.0,
        amp_sat: 0.0,
        amp_light: 6.0,
        wavelength: 6.0,
        octaves: 2,
        persistence: 0.45,
        seed_offset: 20011,
        flow_wavelength: 4.0,
        smoothing_degrees: 90,
        smoothing_iterations: 1,
        rim_inset_octimeters: 32,
        rim_darken: 0.15,
        wash_grade: 0.10,
        water_depth_darken: 0.0,
        blob_merge_degrees: 6.0,
    },
    // Dirt — hsl(31, 42.9, 31.5).
    MaterialStyle {
        base_hue: 31.0,
        base_sat: 42.9,
        base_light: 31.5,
        amp_hue: 6.0,
        amp_sat: 0.0,
        amp_light: 5.0,
        wavelength: 6.0,
        octaves: 2,
        persistence: 0.45,
        seed_offset: 40022,
        flow_wavelength: 4.0,
        smoothing_degrees: 60,
        smoothing_iterations: 3,
        rim_inset_octimeters: 32,
        rim_darken: 0.18,
        wash_grade: 0.10,
        water_depth_darken: 0.0,
        blob_merge_degrees: 6.0,
    },
    // Stone — hsl(240, 3.45, 56.5).
    MaterialStyle {
        base_hue: 240.0,
        base_sat: 3.45,
        base_light: 56.5,
        amp_hue: 4.0,
        amp_sat: 0.0,
        amp_light: 4.0,
        wavelength: 7.0,
        octaves: 2,
        persistence: 0.45,
        seed_offset: 60033,
        flow_wavelength: 4.0,
        smoothing_degrees: 75,
        smoothing_iterations: 2,
        rim_inset_octimeters: 32,
        rim_darken: 0.16,
        wash_grade: 0.08,
        water_depth_darken: 0.0,
        blob_merge_degrees: 6.0,
    },
    // Sand — hsl(46, 50, 70).
    MaterialStyle {
        base_hue: 46.0,
        base_sat: 50.0,
        base_light: 70.0,
        amp_hue: 6.0,
        amp_sat: 0.0,
        amp_light: 6.0,
        wavelength: 6.0,
        octaves: 2,
        persistence: 0.45,
        seed_offset: 80044,
        flow_wavelength: 4.0,
        smoothing_degrees: 90,
        smoothing_iterations: 2,
        rim_inset_octimeters: 32,
        rim_darken: 0.14,
        wash_grade: 0.10,
        water_depth_darken: 0.0,
        blob_merge_degrees: 6.0,
    },
    // Water — hsl(216, 55.6, 45).
    MaterialStyle {
        base_hue: 216.0,
        base_sat: 55.6,
        base_light: 45.0,
        amp_hue: 4.0,
        amp_sat: 0.0,
        amp_light: 8.0,
        wavelength: 12.0,
        octaves: 2,
        persistence: 0.45,
        seed_offset: 100_055,
        flow_wavelength: 4.0,
        smoothing_degrees: 90,
        smoothing_iterations: 3,
        rim_inset_octimeters: 32,
        rim_darken: 0.20,
        wash_grade: 0.06,
        water_depth_darken: 9.0,
        blob_merge_degrees: 6.0,
    },
];

/// Runtime-authored material style rows. `Default` seeds every row from
/// the built-in defaults; a `WorldView` actor holds one instance as
/// session-scoped
/// state (never serialized into the world format — the tuning loop's
/// output is new source defaults, not world data) and applies live writes
/// through [`StyleTable::apply`].
pub struct StyleTable([MaterialStyle; 6]);

impl Default for StyleTable {
    fn default() -> Self {
        Self(STYLES)
    }
}

impl StyleTable {
    /// The style row for `material`.
    #[must_use]
    pub fn get(&self, material: Material) -> &MaterialStyle {
        &self.0[material as usize]
    }

    /// Write a full style row from a `aether.kit.world.set_material_style`
    /// mail, clamping `smoothing_iterations` to [`MAX_SMOOTHING_ITERATIONS`]
    /// and `smoothing_degrees` to `[45, 90]` — the same rule
    /// [`crate::world::World::insert_smoothing_profile`] applies to the
    /// per-cell smoothing table. An undecodable or `Void` material byte is
    /// a no-op; the caller (the `WorldView` handler) is expected to have
    /// already rejected it with a warn log.
    pub fn apply(&mut self, msg: &SetMaterialStyle) {
        let Ok(material) = Material::try_from(msg.material) else {
            return;
        };
        if material == Material::Void {
            return;
        }
        self.0[material as usize] = MaterialStyle {
            base_hue: msg.base_hue,
            base_sat: msg.base_sat,
            base_light: msg.base_light,
            amp_hue: msg.amp_hue,
            amp_sat: msg.amp_sat,
            amp_light: msg.amp_light,
            wavelength: msg.wavelength,
            octaves: msg.octaves,
            persistence: msg.persistence,
            seed_offset: msg.seed_offset,
            flow_wavelength: msg.flow_wavelength,
            smoothing_degrees: msg.smoothing_degrees.clamp(45, 90),
            smoothing_iterations: msg.smoothing_iterations.min(MAX_SMOOTHING_ITERATIONS),
            rim_inset_octimeters: msg.rim_inset_octimeters,
            rim_darken: msg.rim_darken,
            wash_grade: msg.wash_grade,
            water_depth_darken: msg.water_depth_darken,
            blob_merge_degrees: msg.blob_merge_degrees,
        };
    }
}

/// Base seed for the hue channel. Each channel keys a distinct seed so the
/// three axes vary independently rather than in lockstep.
const SEED_HUE: u32 = 77;
/// Base seed for the saturation channel.
const SEED_SAT: u32 = 577;
/// Base seed for the lightness channel.
const SEED_LIGHT: u32 = 1077;
/// Seed for the stroke flow-direction field.
const SEED_DIRECTION: u32 = 6077;
/// Seed for the wash density field.
const SEED_WASH: u32 = 9077;

/// A world-anchored integer hash returning a value in `[0, 1)`.
fn hash(ix: i32, iz: i32, seed: u32) -> f32 {
    let mut h = (ix as u32).wrapping_mul(374_761_393)
        ^ (iz as u32).wrapping_mul(668_265_263)
        ^ seed.wrapping_mul(144_665);
    h = (h ^ (h >> 13)).wrapping_mul(1_274_126_177);
    h ^= h >> 16;
    (h as f32) / 4_294_967_296.0
}

/// Floor to `i32` — `as i32` truncates toward zero, which is wrong for
/// negative world coordinates, so step down when it rounded up.
fn floor_to_i32(v: f32) -> i32 {
    let t = v as i32;
    if (t as f32) > v { t - 1 } else { t }
}

/// Lattice value noise with smoothstep interpolation, in `[0, 1)`.
fn value_noise(x: f32, z: f32, seed: u32) -> f32 {
    let ix = floor_to_i32(x);
    let iz = floor_to_i32(z);
    let fx = x - ix as f32;
    let fz = z - iz as f32;
    let ux = fx * fx * (3.0 - 2.0 * fx);
    let uz = fz * fz * (3.0 - 2.0 * fz);
    let a = hash(ix, iz, seed);
    let b = hash(ix + 1, iz, seed);
    let c = hash(ix, iz + 1, seed);
    let d = hash(ix + 1, iz + 1, seed);
    let top = a + (b - a) * ux;
    let bot = c + (d - c) * ux;
    top + (bot - top) * uz
}

/// Fractal value noise (lacunarity 2) in `[-1, 1]`.
fn fbm(x: f32, z: f32, seed: u32, octaves: u32, wavelength: f32, persistence: f32) -> f32 {
    let mut amp = 1.0;
    let mut freq = 1.0 / wavelength;
    let mut sum = 0.0;
    let mut norm = 0.0;
    for o in 0..octaves {
        let n = value_noise(x * freq, z * freq, seed.wrapping_add(o.wrapping_mul(97))) * 2.0 - 1.0;
        sum += amp * n;
        norm += amp;
        amp *= persistence;
        freq *= 2.0;
    }
    if norm == 0.0 { 0.0 } else { sum / norm }
}

/// A resolved cell color plus the fields the rim and wash passes read.
pub struct ResolvedCell {
    /// Hue in degrees.
    pub hue: f32,
    /// Saturation in percent.
    pub sat: f32,
    /// Lightness in percent.
    pub light: f32,
    /// Unit stroke direction `(cos, sin)` the wash grades along.
    pub stroke: (f32, f32),
}

/// Resolve a cell's color from its world center `(wx, wz)` in cells, using
/// the resolved style row `s`. When `depth` is `Some`, the material is
/// water and the lightness grades down toward the given shore depth
/// `[0, 1]` with its noise amplitude enveloped by depth.
#[must_use]
pub fn resolve_cell(s: &MaterialStyle, wx: f32, wz: f32, depth: Option<f32>) -> ResolvedCell {
    let seed = s.seed_offset;
    let n_hue = fbm(
        wx,
        wz,
        SEED_HUE.wrapping_add(seed),
        s.octaves,
        s.wavelength,
        s.persistence,
    );
    let n_sat = fbm(
        wx,
        wz,
        SEED_SAT.wrapping_add(seed),
        s.octaves,
        s.wavelength,
        s.persistence,
    );
    let n_light = fbm(
        wx,
        wz,
        SEED_LIGHT.wrapping_add(seed),
        s.octaves,
        s.wavelength,
        s.persistence,
    );

    let hue = s.base_hue + s.amp_hue * n_hue;
    let sat = (s.base_sat + s.amp_sat * n_sat).clamp(0.0, 100.0);
    let light = match depth {
        Some(d) => (s.base_light - s.water_depth_darken * d
            + s.amp_light * n_light * (0.25 + 0.75 * d))
            .clamp(0.0, 100.0),
        None => (s.base_light + s.amp_light * n_light).clamp(0.0, 100.0),
    };

    let byte = stroke_byte(s, wx, wz);
    let stroke = STROKE_LUT[byte as usize];
    ResolvedCell {
        hue,
        sat,
        light,
        stroke,
    }
}

/// The stroke-direction byte for a world point: a slow flow field sampled
/// at the material's flow wavelength, quantized to `[0, 255]`.
fn stroke_byte(s: &MaterialStyle, wx: f32, wz: f32) -> u8 {
    let n = value_noise(
        wx / s.flow_wavelength,
        wz / s.flow_wavelength,
        SEED_DIRECTION,
    );
    floor_to_i32(n * 256.0).clamp(0, 255) as u8
}

/// The wash-adjusted lightness at a world vertex `(wx, wz)`, using the
/// resolved style row `s`: project the world position onto the cell's
/// stroke direction and grade lightness by a low-frequency density field so
/// the wash runs continuously along the stroke rather than restarting per
/// cell.
#[must_use]
pub fn wash_lightness(s: &MaterialStyle, light: f32, wx: f32, wz: f32, stroke: (f32, f32)) -> f32 {
    let grade = s.wash_grade;
    if grade == 0.0 {
        return light;
    }
    let along = wx * stroke.0 + wz * stroke.1;
    let perp = -wx * stroke.1 + wz * stroke.0;
    let n = value_noise(along * 0.9, perp * 2.2, SEED_WASH) * 2.0 - 1.0;
    (light * (1.0 - grade * n)).clamp(0.0, 100.0)
}

/// The raw hue-field sample for a cell, mapped to `[0, 1]` — the grayscale
/// value the raw view mode paints for calibrating the table by eye. Uses
/// the resolved style row `s`.
#[must_use]
pub fn raw_field(s: &MaterialStyle, wx: f32, wz: f32) -> f32 {
    let n = fbm(
        wx,
        wz,
        SEED_HUE.wrapping_add(s.seed_offset),
        s.octaves,
        s.wavelength,
        s.persistence,
    );
    (n * 0.5 + 0.5).clamp(0.0, 1.0)
}

/// Convert HSL (hue degrees, saturation / lightness percent) to linear RGB
/// in `[0, 1]`. The HSL-to-sRGB step is the standard piecewise chroma
/// construction; the sRGB channels are then squared as an approximate
/// transfer into the linear space the render pipeline multiplies by
/// `view_proj`.
#[must_use]
pub fn hsl_to_linear_rgb(hue: f32, sat: f32, light: f32) -> [f32; 3] {
    let s = (sat / 100.0).clamp(0.0, 1.0);
    let l = (light / 100.0).clamp(0.0, 1.0);
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = (((hue % 360.0) + 360.0) % 360.0) / 60.0;
    let x = c * (1.0 - ((hp % 2.0) - 1.0).abs());
    let (r, g, b) = if hp < 1.0 {
        (c, x, 0.0)
    } else if hp < 2.0 {
        (x, c, 0.0)
    } else if hp < 3.0 {
        (0.0, c, x)
    } else if hp < 4.0 {
        (0.0, x, c)
    } else if hp < 5.0 {
        (x, 0.0, c)
    } else {
        (c, 0.0, x)
    };
    let m = l - c / 2.0;
    let srgb = [r + m, g + m, b + m];
    [srgb[0] * srgb[0], srgb[1] * srgb[1], srgb[2] * srgb[2]]
}

/// The blob-merge rim strength between a cell and one neighbor side.
/// A different material or the world edge always pools a full rim; two
/// cells of the same material pool proportionally to how far their hue
/// steps past the merge threshold.
#[must_use]
pub fn rim_strength(
    in_world: bool,
    same_material: bool,
    hue_a: f32,
    hue_b: f32,
    threshold_degrees: f32,
) -> f32 {
    if !in_world || !same_material {
        return 1.0;
    }
    let step = (hue_a - hue_b).abs();
    (step / threshold_degrees.max(0.5)).clamp(0.0, 1.0)
}

/// Unit stroke vectors `(cos, sin)` for each direction byte: entry `i`
/// is the angle `i / 256 * 2π`. A const table keeps the wasm build free of
/// a trig dependency and the direction exact for every byte.
#[allow(
    clippy::unreadable_literal,
    clippy::excessive_precision,
    clippy::approx_constant
)]
const STROKE_LUT: [(f32, f32); 256] = [
    (1.0, 0.0),
    (0.999698819, 0.0245412285),
    (0.998795456, 0.0490676743),
    (0.997290457, 0.0735645636),
    (0.995184727, 0.0980171403),
    (0.992479535, 0.122410675),
    (0.98917651, 0.146730474),
    (0.985277642, 0.170961889),
    (0.98078528, 0.195090322),
    (0.97570213, 0.21910124),
    (0.970031253, 0.24298018),
    (0.963776066, 0.266712757),
    (0.956940336, 0.290284677),
    (0.949528181, 0.31368174),
    (0.941544065, 0.336889853),
    (0.932992799, 0.359895037),
    (0.923879533, 0.382683432),
    (0.914209756, 0.405241314),
    (0.903989293, 0.427555093),
    (0.893224301, 0.44961133),
    (0.881921264, 0.471396737),
    (0.870086991, 0.492898192),
    (0.85772861, 0.514102744),
    (0.844853565, 0.53499762),
    (0.831469612, 0.555570233),
    (0.817584813, 0.575808191),
    (0.803207531, 0.595699304),
    (0.788346428, 0.615231591),
    (0.773010453, 0.634393284),
    (0.757208847, 0.653172843),
    (0.740951125, 0.671558955),
    (0.724247083, 0.689540545),
    (FRAC_1_SQRT_2, FRAC_1_SQRT_2),
    (0.689540545, 0.724247083),
    (0.671558955, 0.740951125),
    (0.653172843, 0.757208847),
    (0.634393284, 0.773010453),
    (0.615231591, 0.788346428),
    (0.595699304, 0.803207531),
    (0.575808191, 0.817584813),
    (0.555570233, 0.831469612),
    (0.53499762, 0.844853565),
    (0.514102744, 0.85772861),
    (0.492898192, 0.870086991),
    (0.471396737, 0.881921264),
    (0.44961133, 0.893224301),
    (0.427555093, 0.903989293),
    (0.405241314, 0.914209756),
    (0.382683432, 0.923879533),
    (0.359895037, 0.932992799),
    (0.336889853, 0.941544065),
    (0.31368174, 0.949528181),
    (0.290284677, 0.956940336),
    (0.266712757, 0.963776066),
    (0.24298018, 0.970031253),
    (0.21910124, 0.97570213),
    (0.195090322, 0.98078528),
    (0.170961889, 0.985277642),
    (0.146730474, 0.98917651),
    (0.122410675, 0.992479535),
    (0.0980171403, 0.995184727),
    (0.0735645636, 0.997290457),
    (0.0490676743, 0.998795456),
    (0.0245412285, 0.999698819),
    (6.123234e-17, 1.0),
    (-0.0245412285, 0.999698819),
    (-0.0490676743, 0.998795456),
    (-0.0735645636, 0.997290457),
    (-0.0980171403, 0.995184727),
    (-0.122410675, 0.992479535),
    (-0.146730474, 0.98917651),
    (-0.170961889, 0.985277642),
    (-0.195090322, 0.98078528),
    (-0.21910124, 0.97570213),
    (-0.24298018, 0.970031253),
    (-0.266712757, 0.963776066),
    (-0.290284677, 0.956940336),
    (-0.31368174, 0.949528181),
    (-0.336889853, 0.941544065),
    (-0.359895037, 0.932992799),
    (-0.382683432, 0.923879533),
    (-0.405241314, 0.914209756),
    (-0.427555093, 0.903989293),
    (-0.44961133, 0.893224301),
    (-0.471396737, 0.881921264),
    (-0.492898192, 0.870086991),
    (-0.514102744, 0.85772861),
    (-0.53499762, 0.844853565),
    (-0.555570233, 0.831469612),
    (-0.575808191, 0.817584813),
    (-0.595699304, 0.803207531),
    (-0.615231591, 0.788346428),
    (-0.634393284, 0.773010453),
    (-0.653172843, 0.757208847),
    (-0.671558955, 0.740951125),
    (-0.689540545, 0.724247083),
    (-FRAC_1_SQRT_2, FRAC_1_SQRT_2),
    (-0.724247083, 0.689540545),
    (-0.740951125, 0.671558955),
    (-0.757208847, 0.653172843),
    (-0.773010453, 0.634393284),
    (-0.788346428, 0.615231591),
    (-0.803207531, 0.595699304),
    (-0.817584813, 0.575808191),
    (-0.831469612, 0.555570233),
    (-0.844853565, 0.53499762),
    (-0.85772861, 0.514102744),
    (-0.870086991, 0.492898192),
    (-0.881921264, 0.471396737),
    (-0.893224301, 0.44961133),
    (-0.903989293, 0.427555093),
    (-0.914209756, 0.405241314),
    (-0.923879533, 0.382683432),
    (-0.932992799, 0.359895037),
    (-0.941544065, 0.336889853),
    (-0.949528181, 0.31368174),
    (-0.956940336, 0.290284677),
    (-0.963776066, 0.266712757),
    (-0.970031253, 0.24298018),
    (-0.97570213, 0.21910124),
    (-0.98078528, 0.195090322),
    (-0.985277642, 0.170961889),
    (-0.98917651, 0.146730474),
    (-0.992479535, 0.122410675),
    (-0.995184727, 0.0980171403),
    (-0.997290457, 0.0735645636),
    (-0.998795456, 0.0490676743),
    (-0.999698819, 0.0245412285),
    (-1.0, 1.2246468e-16),
    (-0.999698819, -0.0245412285),
    (-0.998795456, -0.0490676743),
    (-0.997290457, -0.0735645636),
    (-0.995184727, -0.0980171403),
    (-0.992479535, -0.122410675),
    (-0.98917651, -0.146730474),
    (-0.985277642, -0.170961889),
    (-0.98078528, -0.195090322),
    (-0.97570213, -0.21910124),
    (-0.970031253, -0.24298018),
    (-0.963776066, -0.266712757),
    (-0.956940336, -0.290284677),
    (-0.949528181, -0.31368174),
    (-0.941544065, -0.336889853),
    (-0.932992799, -0.359895037),
    (-0.923879533, -0.382683432),
    (-0.914209756, -0.405241314),
    (-0.903989293, -0.427555093),
    (-0.893224301, -0.44961133),
    (-0.881921264, -0.471396737),
    (-0.870086991, -0.492898192),
    (-0.85772861, -0.514102744),
    (-0.844853565, -0.53499762),
    (-0.831469612, -0.555570233),
    (-0.817584813, -0.575808191),
    (-0.803207531, -0.595699304),
    (-0.788346428, -0.615231591),
    (-0.773010453, -0.634393284),
    (-0.757208847, -0.653172843),
    (-0.740951125, -0.671558955),
    (-0.724247083, -0.689540545),
    (-FRAC_1_SQRT_2, -FRAC_1_SQRT_2),
    (-0.689540545, -0.724247083),
    (-0.671558955, -0.740951125),
    (-0.653172843, -0.757208847),
    (-0.634393284, -0.773010453),
    (-0.615231591, -0.788346428),
    (-0.595699304, -0.803207531),
    (-0.575808191, -0.817584813),
    (-0.555570233, -0.831469612),
    (-0.53499762, -0.844853565),
    (-0.514102744, -0.85772861),
    (-0.492898192, -0.870086991),
    (-0.471396737, -0.881921264),
    (-0.44961133, -0.893224301),
    (-0.427555093, -0.903989293),
    (-0.405241314, -0.914209756),
    (-0.382683432, -0.923879533),
    (-0.359895037, -0.932992799),
    (-0.336889853, -0.941544065),
    (-0.31368174, -0.949528181),
    (-0.290284677, -0.956940336),
    (-0.266712757, -0.963776066),
    (-0.24298018, -0.970031253),
    (-0.21910124, -0.97570213),
    (-0.195090322, -0.98078528),
    (-0.170961889, -0.985277642),
    (-0.146730474, -0.98917651),
    (-0.122410675, -0.992479535),
    (-0.0980171403, -0.995184727),
    (-0.0735645636, -0.997290457),
    (-0.0490676743, -0.998795456),
    (-0.0245412285, -0.999698819),
    (-1.8369702e-16, -1.0),
    (0.0245412285, -0.999698819),
    (0.0490676743, -0.998795456),
    (0.0735645636, -0.997290457),
    (0.0980171403, -0.995184727),
    (0.122410675, -0.992479535),
    (0.146730474, -0.98917651),
    (0.170961889, -0.985277642),
    (0.195090322, -0.98078528),
    (0.21910124, -0.97570213),
    (0.24298018, -0.970031253),
    (0.266712757, -0.963776066),
    (0.290284677, -0.956940336),
    (0.31368174, -0.949528181),
    (0.336889853, -0.941544065),
    (0.359895037, -0.932992799),
    (0.382683432, -0.923879533),
    (0.405241314, -0.914209756),
    (0.427555093, -0.903989293),
    (0.44961133, -0.893224301),
    (0.471396737, -0.881921264),
    (0.492898192, -0.870086991),
    (0.514102744, -0.85772861),
    (0.53499762, -0.844853565),
    (0.555570233, -0.831469612),
    (0.575808191, -0.817584813),
    (0.595699304, -0.803207531),
    (0.615231591, -0.788346428),
    (0.634393284, -0.773010453),
    (0.653172843, -0.757208847),
    (0.671558955, -0.740951125),
    (0.689540545, -0.724247083),
    (FRAC_1_SQRT_2, -FRAC_1_SQRT_2),
    (0.724247083, -0.689540545),
    (0.740951125, -0.671558955),
    (0.757208847, -0.653172843),
    (0.773010453, -0.634393284),
    (0.788346428, -0.615231591),
    (0.803207531, -0.595699304),
    (0.817584813, -0.575808191),
    (0.831469612, -0.555570233),
    (0.844853565, -0.53499762),
    (0.85772861, -0.514102744),
    (0.870086991, -0.492898192),
    (0.881921264, -0.471396737),
    (0.893224301, -0.44961133),
    (0.903989293, -0.427555093),
    (0.914209756, -0.405241314),
    (0.923879533, -0.382683432),
    (0.932992799, -0.359895037),
    (0.941544065, -0.336889853),
    (0.949528181, -0.31368174),
    (0.956940336, -0.290284677),
    (0.963776066, -0.266712757),
    (0.970031253, -0.24298018),
    (0.97570213, -0.21910124),
    (0.98078528, -0.195090322),
    (0.985277642, -0.170961889),
    (0.98917651, -0.146730474),
    (0.992479535, -0.122410675),
    (0.995184727, -0.0980171403),
    (0.997290457, -0.0735645636),
    (0.998795456, -0.0490676743),
    (0.999698819, -0.0245412285),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolved_grass_color_is_pinned() {
        // Tripwire: the whole color path (hash, fbm, HSL resolve, HSL to
        // linear RGB) folds into one pinned value. Any drift in the noise
        // or conversion re-keys every cell in every world, so a change here
        // must be deliberate.
        let styles = StyleTable::default();
        let r = resolve_cell(styles.get(Material::Grass), 8.5, 8.5, None);
        let rgb = hsl_to_linear_rgb(r.hue, r.sat, r.light);
        assert_eq!(rgb, [0.086_620_346, 0.271_466, 0.056_088_015]);
    }

    #[test]
    fn adjacent_same_material_cells_differ() {
        // The keyed quilt exists to make neighbors visibly distinct; if the
        // field collapsed to a constant, every grass cell would render the
        // same flat color.
        let styles = StyleTable::default();
        let grass = styles.get(Material::Grass);
        let a = resolve_cell(grass, 8.5, 8.5, None);
        let b = resolve_cell(grass, 9.5, 8.5, None);
        assert_ne!((a.hue, a.light), (b.hue, b.light));
    }

    #[test]
    fn hue_offset_stays_within_amplitude() {
        // The field must not push hue past its declared amplitude, or a
        // cell reads as a foreign material rather than a variant.
        let styles = StyleTable::default();
        let s = styles.get(Material::Grass);
        for i in 0..64 {
            let wx = i as f32 * 0.37 - 5.0;
            let wz = i as f32 * -0.21 + 3.0;
            let r = resolve_cell(s, wx, wz, None);
            assert!(
                (r.hue - s.base_hue).abs() <= s.amp_hue + 1e-4,
                "hue {} escaped base {} +/- {}",
                r.hue,
                s.base_hue,
                s.amp_hue,
            );
        }
    }

    #[test]
    fn stroke_lut_wraps_the_full_circle() {
        // Tripwire: the direction byte maps byte/256 * 2pi onto a full unit
        // circle. A regeneration error (wrong period, half circle, non-unit
        // entries) would break the wash flow field.
        assert!((STROKE_LUT[0].0 - 1.0).abs() < 1e-4 && STROKE_LUT[0].1.abs() < 1e-4);
        assert!(STROKE_LUT[64].0.abs() < 1e-4 && (STROKE_LUT[64].1 - 1.0).abs() < 1e-4);
        assert!((STROKE_LUT[128].0 + 1.0).abs() < 1e-4 && STROKE_LUT[128].1.abs() < 1e-4);
        assert!(STROKE_LUT[192].0.abs() < 1e-4 && (STROKE_LUT[192].1 + 1.0).abs() < 1e-4);
        for (cos, sin) in STROKE_LUT {
            assert!(
                (cos * cos + sin * sin - 1.0).abs() < 1e-4,
                "non-unit stroke vector"
            );
        }
    }

    #[test]
    fn material_boundary_and_world_edge_always_rim() {
        // A different material or the world edge (a Void neighbor) is always
        // a paint change, so its rim is full regardless of hue.
        assert_eq!(rim_strength(false, false, 110.0, 110.0, 6.0), 1.0);
        assert_eq!(rim_strength(true, false, 110.0, 31.0, 6.0), 1.0);
    }

    #[test]
    fn same_material_rims_only_past_the_threshold() {
        // Within a material a small hue step merges (no rim); a step past
        // the blob-merge threshold pools a full rim. This is the whole
        // blob-merge rule.
        assert_eq!(rim_strength(true, true, 110.0, 112.0, 6.0), 2.0 / 6.0);
        assert_eq!(rim_strength(true, true, 110.0, 130.0, 6.0), 1.0);
        assert_eq!(rim_strength(true, true, 110.0, 110.0, 6.0), 0.0);
    }

    #[test]
    fn apply_clamps_the_smoothing_pair() {
        // Catches a dropped clamp: the mesher's fixed two-cell smoothing
        // apron assumes `smoothing_iterations` never exceeds
        // MAX_SMOOTHING_ITERATIONS, and the windowed smoothing rule assumes
        // `smoothing_degrees` never drops below 45.
        let mut styles = StyleTable::default();
        styles.apply(&SetMaterialStyle {
            material: Material::Grass.to_u8(),
            base_hue: 110.0,
            base_sat: 37.5,
            base_light: 40.0,
            amp_hue: 8.0,
            amp_sat: 0.0,
            amp_light: 6.0,
            wavelength: 6.0,
            octaves: 2,
            persistence: 0.45,
            seed_offset: 20011,
            flow_wavelength: 4.0,
            smoothing_degrees: 10,
            smoothing_iterations: 99,
            rim_inset_octimeters: 32,
            rim_darken: 0.15,
            wash_grade: 0.10,
            water_depth_darken: 0.0,
            blob_merge_degrees: 6.0,
        });
        let row = styles.get(Material::Grass);
        assert_eq!(row.smoothing_iterations, MAX_SMOOTHING_ITERATIONS);
        assert_eq!(row.smoothing_degrees, 45);
    }

    #[test]
    fn apply_rejects_void_and_out_of_range_bytes() {
        // Void (0) is never painted and a byte past Water (5) doesn't decode
        // to a Material at all; both must leave every row untouched rather
        // than panic on an out-of-bounds index or silently write a row no
        // material reads.
        let mut styles = StyleTable::default();
        for byte in [0u8, 6, 255] {
            styles.apply(&SetMaterialStyle {
                material: byte,
                base_hue: 1.0,
                base_sat: 1.0,
                base_light: 1.0,
                amp_hue: 1.0,
                amp_sat: 1.0,
                amp_light: 1.0,
                wavelength: 1.0,
                octaves: 1,
                persistence: 1.0,
                seed_offset: 1,
                flow_wavelength: 1.0,
                smoothing_degrees: 45,
                smoothing_iterations: 0,
                rim_inset_octimeters: 1,
                rim_darken: 1.0,
                wash_grade: 1.0,
                water_depth_darken: 1.0,
                blob_merge_degrees: 1.0,
            });
        }
        let grass = styles.get(Material::Grass);
        assert_eq!(
            (grass.base_hue, grass.base_sat, grass.base_light),
            (110.0, 37.5, 40.0),
            "an undecodable or Void material byte must not mutate the table",
        );
    }
}
