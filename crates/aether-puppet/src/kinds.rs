//! The mail shapes peers send the puppet.

use serde::{Deserialize, Serialize};

/// Padding the canonical material field was baked with, as a fraction of
/// the mesh's longest axis on each side.
pub const DEFAULT_MATERIAL_FIELD_PADDING: f32 = 0.12;

/// Point the puppet at a mesh in one of the substrate's I/O namespaces
/// (`save`, `assets`, `config`). The load is asynchronous; the cached
/// drawing is replaced atomically when the bytes arrive, so a failed load
/// leaves the previous subject on screen rather than blanking it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, aether_data::Kind, aether_data::Schema)]
#[kind(name = "aether.puppet.load")]
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, aether_data::Kind, aether_data::Schema)]
#[kind(name = "aether.puppet.load_result")]
pub enum LoadResult {
    Ok { vertices: u32, faces: u32, bones: u32 },
    Err { reason: String },
}

/// Select one of the chart's named faces.
///
/// The expression supplies the mouth, brows, and eye aperture. It leaves
/// [`Gaze`] alone, so looking somewhere and feeling something compose.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, aether_data::Kind, aether_data::Schema)]
#[kind(name = "aether.puppet.expression")]
pub struct Expression {
    pub name: String,
}

/// Move both irises together in the puppet's own frame.
///
/// Both axes are normalized and clamped to `[-1, 1]`. Positive `x` is
/// toward her left and positive `y` is up. The lids follow the vertical
/// axis with the chart's authored upper/lower weights.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize, aether_data::Kind, aether_data::Schema)]
#[kind(name = "aether.puppet.gaze")]
pub struct Gaze {
    pub x: f32,
    pub y: f32,
}

/// Select one of the chart's named mouth shapes without changing the
/// expression's brows or eyes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, aether_data::Kind, aether_data::Schema)]
#[kind(name = "aether.puppet.viseme")]
pub struct Viseme {
    pub name: String,
}

/// Select the eye design the chart draws without changing expression or gaze.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, aether_data::Kind, aether_data::Schema)]
#[kind(name = "aether.puppet.eye_archetype")]
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
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize, aether_data::Kind, aether_data::Schema)]
#[kind(name = "aether.puppet.pose")]
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
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize, aether_data::Kind, aether_data::Schema)]
#[kind(name = "aether.puppet.look")]
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
