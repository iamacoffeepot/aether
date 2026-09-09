//! The mail shapes peers send the puppet.

use aether_math::Vec3;

use crate::extract::{LightFrame, Settings};

/// Padding the canonical material field was baked with, as a fraction of
/// the mesh's longest axis on each side.
pub const DEFAULT_MATERIAL_FIELD_PADDING: f32 = 0.12;

/// Point the puppet at a mesh in one of the substrate's I/O namespaces
/// (`save`, `assets`, `config`). The load is asynchronous; the cached
/// drawing is replaced atomically when the bytes arrive, so a failed load
/// leaves the previous subject on screen rather than blanking it.
#[aether_data::kind(name = "aether.puppet.load", partial_eq)]
pub struct Load {
    pub namespace: String,
    pub path: String,
    /// Optional path to the material field, a `NumPy` 1.0 `|u1`, C-order
    /// array shaped exactly `(n, n, n)` with `n >= 2`, baked over the same
    /// sculpt.
    ///
    /// Without it every crease is inked, and the sculpt carves a hair-strand
    /// seam exactly as deeply as it carves an eyelid — there are five times
    /// as many of them, so they bury the face. Depth cannot tell them apart;
    /// the field already knows which is which, so the drawing asks it rather
    /// than inventing a geometric proxy. Empty loads the mesh alone.
    pub labels: String,
    /// Padding the material field was baked with, as a fraction of the mesh's
    /// longest axis on each side. The default is the canonical asset's `0.12`;
    /// callers loading a differently baked field must declare its value here.
    pub material_field_padding: f32,
    /// Optional directory holding the rig that poses this subject:
    /// `weights.npy` (`NumPy` 1.0, `<f4`, C-order, shaped exactly
    /// `(mesh vertices, descriptor bones)`) and `rig.txt` (the bone order,
    /// pivots and long axes).
    ///
    /// Weights are per vertex of one sculpt and carry no identity of their
    /// own, so a rig whose vertex count disagrees with the mesh is refused
    /// rather than applied to whatever turned up. Empty leaves the subject
    /// unposable, which is what [`LoadResult::Ok`]'s `bones` then reports.
    pub rig: String,
    /// Optional path to the painter's box this subject is painted out of
    /// (`aether_puppet::easel::palette::Palette::decode_text`): its class
    /// vocabulary, one entry per painted class, the fall-throughs and the
    /// classes left as bare paper.
    ///
    /// The box is per subject because pigments are. A hillside's rock and
    /// timber are not her indigo and rose, and a field's cells name
    /// classes by position — so the same byte means one thing under her
    /// vocabulary and another under a scene's, and the field is validated
    /// against whichever box is going to paint it. Empty paints with the
    /// canonical box, which is what she was tuned on.
    pub palette: String,
}

impl Default for Load {
    fn default() -> Self {
        Self {
            namespace: String::new(),
            path: String::new(),
            labels: String::new(),
            material_field_padding: DEFAULT_MATERIAL_FIELD_PADDING,
            rig: String::new(),
            palette: String::new(),
        }
    }
}

/// What the subject turned out to be. `bones` is `0` when no rig was asked
/// for or none was accepted — the one place a refused rig is visible to the
/// caller rather than only in the log.
#[aether_data::kind(name = "aether.puppet.load_result", eq)]
pub enum LoadResult {
    Ok { vertices: u32, faces: u32, bones: u32 },
    Err { reason: String },
}

/// Retune the hatching — the whole of the shading style, in one mail.
///
/// Absolute, like [`Pose`]: every field replaces its counterpart, and a
/// field left at its default is that default rather than "leave what was
/// there". Send [`Hatch::default()`] to put the authored style back.
///
/// The hatch planes are world-space level sets solved off the subject, so
/// a mail that changes [`Hatch::spacing`], [`Hatch::family_spacing`] or
/// [`Hatch::tilt`] re-extracts them; the rest are read per frame and cost
/// nothing but the next redraw. A field that is not finite, or a spacing
/// that is not positive, leaves the whole style alone and logs — a style
/// half-applied is harder to reason about than one refused.
///
/// # Agent
/// This is the knob to turn when the drawing reads too sparse or too
/// dense. `spacing` is a fraction of the subject's longest axis, so it
/// means the same thing on any subject; `thresholds` is the tone ramp,
/// spread between `ambient` and `1`.
#[aether_data::kind(name = "aether.puppet.hatch", copy, partial_eq)]
pub struct Hatch {
    /// Distance between hatch lines, as a fraction of the subject's
    /// longest bounding-box axis. Must be positive.
    pub spacing: f32,
    /// Per-family multiplier over `spacing`. Each successive family is a
    /// little sparser, so the step from one family to two adds tone
    /// rather than doubling it. Every entry must be positive.
    pub family_spacing: [f32; 3],
    /// Tone below which each successive family switches on: bare paper
    /// above the first, all three crossing below the last. Spread them
    /// across the range tone reaches — `ambient` at the terminator to `1`
    /// under the key light — or a family names a tone nothing has and
    /// never draws.
    pub thresholds: [f32; 3],
    /// How far a threshold is dithered, in tone, so a family breaks into
    /// dashes as it fades instead of ruling its boundary across the
    /// figure. Zero rules it.
    pub dither: f32,
    /// Angle of the primary family, in radians.
    pub tilt: f32,
    /// Where the key light stands, as a direction.
    ///
    /// Read in the camera's frame unless `world_light` — `x` to the
    /// viewer's right, `y` up, `z` toward the viewer — which is where an
    /// illustrator's key light stands and what gives a turning subject a
    /// lit side and a shaded side in every view.
    pub light: [f32; 3],
    /// Read `light` as a world direction instead. Right for a lit scene,
    /// wrong for a subject on a turntable: the lit side stays put while
    /// the camera walks around it, so one azimuth comes back a bare
    /// outline and the opposite one a solid mesh.
    pub world_light: bool,
    /// Floor of the shading term, so nothing reads as pure black.
    pub ambient: f32,
}

impl Default for Hatch {
    /// The authored style, read off [`Settings`] rather than restated
    /// here — two spellings of one default drift, and this one has to be
    /// the style the subject already carries or a round trip through the
    /// mail changes the drawing.
    fn default() -> Self {
        Self::of(&Settings::default())
    }
}

impl Hatch {
    /// The style a settings block is currently carrying.
    #[must_use]
    pub fn of(settings: &Settings) -> Self {
        Self {
            spacing: settings.hatch_spacing,
            family_spacing: settings.hatch_family_spacing,
            thresholds: settings.hatch_thresholds,
            dither: settings.hatch_dither,
            tilt: settings.hatch_tilt,
            light: settings.light.to_array(),
            world_light: settings.light_frame == LightFrame::World,
            ambient: settings.ambient,
        }
    }

    /// Whether every number here is one a drawing can be solved from.
    ///
    /// A spacing at or below zero divides the subject into infinitely
    /// many planes, and a light of zero length has no direction to shade
    /// from; neither is a style, so a mail carrying one is refused whole.
    #[must_use]
    pub fn is_solvable(&self) -> bool {
        let scalars = [self.dither, self.tilt, self.ambient];

        scalars.iter().chain(&self.thresholds).chain(&self.light).all(|value| value.is_finite())
            && self.spacing > 0.0
            && self.family_spacing.iter().all(|spacing| *spacing > 0.0 && spacing.is_finite())
            && Vec3::from_array(self.light).length() > 0.0
    }

    /// Write this style onto the settings that carry it.
    pub fn apply(&self, settings: &mut Settings) {
        settings.hatch_spacing = self.spacing;
        settings.hatch_family_spacing = self.family_spacing;
        settings.hatch_thresholds = self.thresholds;
        settings.hatch_dither = self.dither;
        settings.hatch_tilt = self.tilt;
        settings.light = Vec3::from_array(self.light);
        settings.light_frame = if self.world_light {
            LightFrame::World
        } else {
            LightFrame::Camera
        };
        settings.ambient = self.ambient;
    }
}

/// Select one of the chart's named faces.
///
/// The expression supplies the mouth, brows, and eye aperture. It leaves
/// [`Gaze`] alone, so looking somewhere and feeling something compose.
#[aether_data::kind(name = "aether.puppet.expression", eq)]
pub struct Expression {
    pub name: String,
}

/// Move both irises together in the puppet's own frame.
///
/// Both axes are normalized and clamped to `[-1, 1]`. Positive `x` is
/// toward her left and positive `y` is up. The lids follow the vertical
/// axis with the chart's authored upper/lower weights.
#[aether_data::kind(name = "aether.puppet.gaze", copy, default, partial_eq)]
pub struct Gaze {
    pub x: f32,
    pub y: f32,
}

/// Select one of the chart's named mouth shapes without changing the
/// expression's brows or eyes.
#[aether_data::kind(name = "aether.puppet.viseme", eq)]
pub struct Viseme {
    pub name: String,
}

/// Select the eye design the chart draws without changing expression or gaze.
#[aether_data::kind(name = "aether.puppet.eye_archetype", eq)]
pub struct EyeArchetype {
    pub name: String,
}

/// What the rig is doing, in degrees per channel.
///
/// Channel names follow the rigging reference: `ARKit` where `ARKit` has
/// an opinion, `ARKit`-style where it does not — ears being the obvious
/// gap. This kind is absolute: every mail replaces the complete pose,
/// and an omitted or zero-valued channel is at rest rather than left at
/// its previous value. Everything at zero is the rest pose, and a subject
/// at rest skins nothing: the drawing extracted at load is the drawing
/// that ships.
///
/// Deformation clamps each value to the subject's authored arc before any
/// CPU or GPU bone map is derived: yaw `[-28, 28]`, pitch `[-12, 12]`,
/// roll `[-12, 12]`, jaw `[-12, 12]`, each ear flick `[-22, 22]`, and
/// each ear twist `[-22.5, 22.5]` degrees. Values outside an arc alias its
/// nearest endpoint without changing the wire shape.
#[aether_data::kind(name = "aether.puppet.pose", copy, default, partial_eq)]
pub struct Pose {
    /// Turn, about the head pivot, clamped to `[-28, 28]`. Shared with the
    /// neck, so it reads as a neck carrying a head rather than a head
    /// swivelling on a post.
    pub yaw: f32,
    /// Nod about the head pivot, clamped to `[-12, 12]`.
    pub pitch: f32,
    /// Tilt about the head pivot, clamped to `[-12, 12]`.
    pub roll: f32,
    /// The mandible, hinged below and in front of the ear canal, clamped
    /// to `[-12, 12]`.
    pub jaw: f32,
    /// The blade swinging out of the midline plane, clamped to
    /// `[-22, 22]` on each side.
    pub ear_flick_left: f32,
    pub ear_flick_right: f32,
    /// Aim, not flap: the cup sweeping toward a sound about the blade's
    /// own long axis. Clamped to `[-22.5, 22.5]`, the arc an ear actually
    /// has.
    pub ear_twist_left: f32,
    pub ear_twist_right: f32,
}

impl Pose {
    /// Whether every channel is at rest, in which case the posed subject
    /// is the rest subject and nothing has to be skinned at all.
    pub fn is_rest(&self) -> bool {
        *self == Self::default()
    }
}

/// Where the eye sits. Silhouettes are the zero set of `view . normal`, so
/// the renderer cannot draw a frame without knowing this — which is why the
/// puppet owns its camera rather than reading one.
///
/// `aether.view_projection` travels to `aether.render` as a directed send
/// rather than a subscribable stream, so a second actor's camera is not
/// something this one can overhear. Owning it also means she is one
/// component to boot: load her and she is on screen, framed.
#[aether_data::kind(name = "aether.puppet.look", copy, default, partial_eq)]
pub struct Look {
    /// Degrees around her, counterclockwise from facing the camera.
    pub azimuth: f32,
    /// Degrees above the horizon.
    pub elevation: f32,
    /// Distance from the framing target, in model units.
    pub distance: f32,
    /// Height of the point the camera aims at.
    pub height: f32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_defaults_to_the_canonical_material_field_padding() {
        assert_eq!(Load::default().material_field_padding, DEFAULT_MATERIAL_FIELD_PADDING);
    }
}
